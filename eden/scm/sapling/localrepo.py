# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# localrepo.py - read/write repository class for mercurial
#
# Copyright 2005-2007 Olivia Mackall <olivia@selenic.com>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.


import errno
import hashlib
import os
import random
import time
import weakref
from contextlib import contextmanager
from functools import partial
from typing import List, Optional, Set

import bindings
from sapling import tracing
from sapling.ext.extlib.phabricator import diffprops

from . import (
    bookmarks,
    bundle2,
    changegroup,
    changelog2,
    color,
    commitscheme,
    connectionpool,
    context,
    dirstate as dirstatemod,
    dirstateguard,
    discovery,
    eagerepo,
    edenfs,
    encoding,
    error as errormod,
    exchange,
    extensions,
    filelog,
    git,
    grepo,
    hook,
    identity,
    lock as lockmod,
    manifest,
    match as matchmod,
    merge as mergemod,
    mergeutil,
    mutation,
    namespaces,
    peer,
    perftrace,
    phases,
    pushkey,
    repository,
    revset,
    revsetlang,
    rewriteutil,
    scmutil,
    signing,
    smallcommitmetadata,
    store,
    transaction,
    util,
    vfs as vfsmod,
    visibility,
)
from .i18n import _, _n
from .node import bin, hex, nullhex, nullid

release = lockmod.release
urlerr = util.urlerr
urlreq = util.urlreq

# set of (path, vfs-location) tuples. vfs-location is:
# - 'plain for vfs relative paths
# - '' for svfs relative paths
_cachedfiles = set()

SECONDS_IN_A_DAY = 60 * 60 * 24


class _basefilecache(scmutil.filecache):
    """filecache usage on repo"""

    def __get__(self, repo, type=None):
        if repo is None:
            return self
        return super(_basefilecache, self).__get__(repo, type)

    def __set__(self, repo, value):
        return super(_basefilecache, self).__set__(repo, value)

    def __delete__(self, repo):
        return super(_basefilecache, self).__delete__(repo)


class repofilecache(_basefilecache):
    """filecache for files in .hg but outside of .hg/store"""

    def __init__(self, localpaths=(), sharedpaths=()):
        paths = [(path, self.localjoin) for path in localpaths] + [
            (path, self.sharedjoin) for path in sharedpaths
        ]
        super(repofilecache, self).__init__(*paths)
        for path in localpaths:
            _cachedfiles.add((path, "local"))
        for path in sharedpaths:
            _cachedfiles.add((path, "shared"))

    def localjoin(self, obj, fname):
        return obj.localvfs.join(fname)

    def sharedjoin(self, obj, fname):
        return obj.sharedvfs.join(fname)


class storecache(_basefilecache):
    """filecache for files in the store"""

    def __init__(self, *paths):
        super(storecache, self).__init__(*paths)
        for path in paths:
            _cachedfiles.add((path, ""))

    def join(self, obj, fname):
        return obj.sjoin(fname)


class metalogcache(scmutil.keyedcache):
    """property cache based on given metalog keys"""

    def __init__(self, *metalog_keys):
        if metalog_keys:
            key = lambda repo: tuple(repo.metalog().get_hash(k) for k in metalog_keys)
        else:
            key = lambda repo: repo.metalog().root()
        super().__init__(key)


class dagcache(scmutil.keyedcache):
    """property cache based on dag version

    Side effect: creates changelog.

    Note: dag only tracks commits, not bookmarks, or remote names, or
    visibility.
    """

    def __init__(self):
        super().__init__(lambda repo: repo.changelog.dag.version())


def isfilecached(repo, name):
    """check if a repo has already cached "name" filecache-ed property

    This returns (cachedobj-or-None, iscached) tuple.
    """
    cacheentry = repo._filecache.get(name, None)
    if not cacheentry:
        return None, False
    return cacheentry.obj, True


def hascache(repo, name) -> bool:
    """check if a repo has an value for <name>"""
    return name in vars(repo)


moderncaps = {"lookup", "branchmap", "pushkey", "known", "getbundle", "unbundle"}
legacycaps: Set[str] = moderncaps.union({"changegroupsubset"})


class localpeer(repository.peer):
    """peer for a local repo; reflects only the most recent API"""

    def __init__(self, repo, caps=None):
        super(localpeer, self).__init__()

        if caps is None:
            caps = moderncaps.copy()
        self._repo = repo
        self._ui = repo.ui
        self._caps = repo._restrictcapabilities(caps)

    # Begin of _basepeer interface.

    @util.propertycache
    def ui(self):
        return self._ui

    def url(self):
        return self._repo.url()

    def local(self):
        return self._repo

    def peer(self):
        return self

    def canpush(self):
        return True

    def close(self):
        self._repo.close()

    # End of _basepeer interface.

    # Begin of _basewirecommands interface.

    def branchmap(self):
        return self._repo.branchmap()

    def capabilities(self):
        return self._caps

    def debugwireargs(self, one, two, three=None, four=None, five=None):
        """Used to test argument passing over the wire"""
        return "%s %s %s %s %s" % (one, two, three, four, five)

    def getbundle(self, source, heads=None, common=None, bundlecaps=None, **kwargs):
        chunks = exchange.getbundlechunks(
            self._repo,
            source,
            heads=heads,
            common=common,
            bundlecaps=bundlecaps,
            **kwargs,
        )
        cb = util.chunkbuffer(chunks)

        if exchange.bundle2requested(bundlecaps):
            # When requesting a bundle2, getbundle returns a stream to make the
            # wire level function happier. We need to build a proper object
            # from it in local peer.
            return bundle2.getunbundler(self.ui, cb)
        else:
            return changegroup.getunbundler("01", cb, None)

    def known(self, nodes):
        return self._repo.known(nodes)

    def listkeys(self, namespace, patterns=None):
        return self._repo.listkeys(namespace, patterns)

    def listkeyspatterns(self, namespace, patterns=None):
        return self._repo.listkeys(namespace, patterns)

    def lookup(self, key):
        return self._repo.lookup(key)

    def pushkey(self, namespace, key, old, new):
        return self._repo.pushkey(namespace, key, old, new)

    def unbundle(self, cg, heads, url):
        """apply a bundle on a repo

        This function handles the repo locking itself."""
        try:
            try:
                cg = exchange.readbundle(self.ui, cg, None)
                ret = exchange.unbundle(self._repo, cg, heads, "push", url)
                if hasattr(ret, "getchunks"):
                    # This is a bundle20 object, turn it into an unbundler.
                    # This little dance should be dropped eventually when the
                    # API is finally improved.
                    stream = util.chunkbuffer(ret.getchunks())
                    ret = bundle2.getunbundler(self.ui, stream)
                return ret
            except Exception as exc:
                # If the exception contains output salvaged from a bundle2
                # reply, we need to make sure it is printed before continuing
                # to fail. So we build a bundle2 with such output and consume
                # it directly.
                #
                # This is not very elegant but allows a "simple" solution for
                # issue4594
                output = getattr(exc, "_bundle2salvagedoutput", ())
                if output:
                    bundler = bundle2.bundle20(self._repo.ui)
                    for out in output:
                        bundler.addpart(out)
                    stream = util.chunkbuffer(bundler.getchunks())
                    b = bundle2.getunbundler(self.ui, stream)
                    bundle2.processbundle(self._repo, b)
                raise
        except errormod.PushRaced as exc:
            raise errormod.ResponseError(_("push failed:"), str(exc))

    # End of _basewirecommands interface.

    # Begin of peer interface.

    def iterbatch(self):
        return peer.localiterbatcher(self)

    # End of peer interface.


class locallegacypeer(repository.legacypeer, localpeer):
    """peer extension which implements legacy methods too; used for tests with
    restricted capabilities"""

    def __init__(self, repo):
        super(locallegacypeer, self).__init__(repo, caps=legacycaps)

    # Begin of baselegacywirecommands interface.

    def between(self, pairs):
        return self._repo.between(pairs)

    def branches(self, nodes):
        return self._repo.branches(nodes)

    def changegroup(self, basenodes, source):
        outgoing = discovery.outgoing(
            self._repo, missingroots=basenodes, missingheads=self._repo.heads()
        )
        return changegroup.makechangegroup(self._repo, outgoing, "01", source)

    def changegroupsubset(self, bases, heads, source):
        outgoing = discovery.outgoing(
            self._repo, missingroots=bases, missingheads=heads
        )
        return changegroup.makechangegroup(self._repo, outgoing, "01", source)

    # End of baselegacywirecommands interface.


class localrepository:
    """local repository object

    ``unsafe.wvfsauditorcache`` config option allows the user to enable
    the auditor caching for the repo's working copy. This significantly
    reduces the amount of stat-like system calls and thus saves time.
    This option should be safe if symlinks are not used in the repo"""

    supportedformats = {
        "revlogv1",
        "generaldelta",
        "treemanifest",
        # no longer used, but needed to preserve compatibility
        "lz4revlog",
    }
    _basesupported = supportedformats | set(bindings.repo.SUPPORTED_REQUIREMENTS)
    _basestoresupported = set(bindings.repo.SUPPORTED_STORE_REQUIREMENTS)
    openerreqs = {"revlogv1", "generaldelta", "treemanifest"}

    # sets of (ui, featureset) functions for repo and store features.
    # only functions defined in module of enabled extensions are invoked
    # pyre-fixme[20]: Argument `expr` expected.
    featuresetupfuncs = set()
    # pyre-fixme[20]: Argument `expr` expected.
    storefeaturesetupfuncs = set()

    # list of prefix for file which can be written without 'wlock'
    # Extensions should extend this list when needed
    _wlockfreeprefix = {
        # We might consider requiring 'wlock' for the next
        # two, but pretty much all the existing code assume
        # wlock is not needed so we keep them excluded for
        # now.
        "config",
        "hgrc",
        "requires",
        # XXX cache is a complicated business someone
        # should investigate this in depth at some point
        "cache/",
        # XXX shouldn't be dirstate covered by the wlock?
        "dirstate",
        # XXX checkoutidentifier has same rules as dirstate.
        "checkoutidentifier",
        # XXX bisect was still a bit too messy at the time
        # this changeset was introduced. Someone should fix
        # the remaining bit and drop this line
        "bisect.state",
        # Race condition to this file is okay.
        "lastsqlsync",
    }

    # Set of prefixes of store files which can be written without 'lock'.
    # Extensions should extend this set when necessary.
    # pyre-fixme[20]: Argument `expr` expected.
    _lockfreeprefix = set()

    @perftrace.tracefunc("localrepo.__init__")
    def __init__(
        self,
        baseui,
        path,
        create=False,
        initial_config: Optional[str] = None,
    ):
        """Instantiate local repo object, optionally creating a new repo on disk if `create` is True.
        If specified, `initial_config` is added to the created repo's config."""

        # Simplify things by keeping identity cache scoped at max to
        # a single repo's lifetime. In particular this is necessary
        # with respect to git submodules.
        identity.sniffdir.cache_clear()

        if create:
            bindings.repo.repo.initialize(path, baseui._rcfg, initial_config)

        # Make sure repo dot dir exists.
        if not identity.sniffdir(path):
            raise errormod.RepoError(_("repository %s not found") % path)

        self._containscount = 0
        self.requirements = set()
        self.storerequirements = set()

        self.root = os.path.realpath(util.expandpath(path))

        self.baseui = baseui
        self.ui = baseui.copy()
        self.ui.loadrepoconfig(self.root)

        self._rsrepo = bindings.repo.repo(self.root, self.ui.rustcontext())

        # sharedvfs: the local vfs of the primary shared repo for shared repos.
        # for non-shared repos this is the same as localvfs.
        self.sharedvfs = None
        # svfs: store vfs - usually rooted at .hg/store, used to access repository history
        # If this is a shared repository, this vfs may point to another
        # repository's .hg/store directory.
        self.svfs = None
        self.path = self._rsrepo.dot_path
        self.origroot = path
        # localvfs: rooted at .hg, used to access repo files outside of
        # the store that are local to this working copy.
        self.localvfs = vfsmod.vfs(self.path, cacheaudited=True)
        if self.ui.configbool("devel", "all-warnings") or self.ui.configbool(
            "devel", "check-locks"
        ):
            self.localvfs.audit = self._getvfsward(self.localvfs.audit)

        # A list of callback to shape the phase if no data were found.
        # Callback are in the form: func(repo, roots) --> processed root.
        # This list it to be filled by extension during repo setup
        self._phasedefaults = []

        self._loadextensions()

        self.supported = self._featuresetup(self.featuresetupfuncs, self._basesupported)
        self.storesupported = self._featuresetup(
            self.storefeaturesetupfuncs, self._basestoresupported
        )
        color.setup(self.ui)

        # Add compression engines.
        for name in util.compengines:
            engine = util.compengines[name]
            if engine.revlogheader():
                self.supported.add("exp-compression-%s" % name)

        try:
            self.requirements = scmutil.readrequires(self.localvfs, self.supported)
        except IOError as inst:
            if inst.errno != errno.ENOENT:
                raise

        # wvfs: rooted at the repository root, used to access the working copy
        disablesymlinks = util.iswindows and "windowssymlinks" not in self.requirements
        self.wvfs = vfsmod.vfs(
            path,
            expandpath=True,
            realpath=True,
            cacheaudited=False,
            disablesymlinks=disablesymlinks,
        )

        cacheaudited = self.ui.configbool("unsafe", "wvfsauditorcache")
        self.wvfs.audit._cached = cacheaudited

        cachepath = self.localvfs.join("cache")
        self.sharedpath = self.path
        self.sharedroot = os.path.realpath(self._rsrepo.shared_root)
        self.sharedvfs = self.localvfs
        sharedpath = self._rsrepo.shared_dot_path
        if sharedpath != self.path:
            sharedvfs = vfsmod.vfs(sharedpath, realpath=True)
            cachepath = sharedvfs.join("cache")
            s = sharedvfs.base
            if not sharedvfs.exists():
                raise errormod.RepoError(
                    _("%s/sharedpath points to nonexistent directory %s")
                    % (identity.default().dotdir(), s)
                )
            self.sharedpath = s
            self.sharedvfs = sharedvfs
            # Read the requirements of the shared repo to make sure we
            # support them.
            try:
                scmutil.readrequires(self.sharedvfs, self.supported)
            except IOError as inst:
                if inst.errno != errno.ENOENT:
                    raise
            self.sharedvfs.audit = self._getvfsward(self.sharedvfs.audit)

        self.store = store.store(
            self.requirements,
            self.sharedpath,
            lambda base: vfsmod.vfs(base, cacheaudited=True),
            self.ui.uiconfig(),
        )
        self.spath = self.store_path = self.store.path
        self.svfs = self.store.vfs
        self.svfs._rsrepo = self._rsrepo
        self.sjoin = self.store.join
        store.setvfsmode(self.localvfs)
        store.setvfsmode(self.sharedvfs)
        self.cachevfs = vfsmod.vfs(cachepath, cacheaudited=True)
        store.setvfsmode(self.cachevfs)
        if self.ui.configbool("devel", "all-warnings") or self.ui.configbool(
            "devel", "check-locks"
        ):
            self.svfs.audit = self._getsvfsward(self.svfs.audit)
        if "store" in self.requirements:
            try:
                self.storerequirements = scmutil.readrequires(
                    self.svfs, self.storesupported
                )
            except IOError as inst:
                if inst.errno != errno.ENOENT:
                    raise

        if "hgsql" in self.requirements:
            # hgsql wants raw access to revlog. Disable modern features
            # unconditionally for hgsql.
            self.ui.setconfig("experimental", "narrow-heads", "false", "hgsql")
            self.ui.setconfig("visibility", "enabled", "false", "hgsql")

        self._dirstatevalidatewarned = False

        self._branchcaches = {}
        self._transref = self._lockref = self._wlockref = None

        # headcache might belong to the changelog object for easier
        # invalidation. However, that requires moving the dependencies
        # involved in `head` calculation to changelog too, including
        # remotenames.
        self._headcache = {}

        self.connectionpool = connectionpool.connectionpool(self)

        self._smallcommitmetadata = None

        # A cache for various files under .hg/ that tracks file changes,
        # (used by the filecache decorator)
        #
        # Maps a property name to its util.filecacheentry
        self._filecache = {}

        # post-dirstate-status hooks
        self._postdsstatus = []

        # generic mapping between names and nodes
        self.names = namespaces.namespaces(self)

        # associated submodule
        self.submodule = None

        # whether the repo is changed (by transaction).
        # currently used to decide whether to run fsync.
        self._txnreleased = False

        self._applyopenerreqs()

        self._eventreporting = True

        self.svfs._reporef = weakref.ref(self)

        # needed by revlog2
        sfmt = self.storage_format()
        if not create and sfmt == "revlog":
            from . import revlog2

            revlog2.patch_types()

            if "eagercompat" not in self.storerequirements:
                with self.lock(wait=False):
                    self.storerequirements.add("eagercompat")
                    self._writestorerequirements()

        try:
            self._visibilitymigration()
            self._svfsmigration()
            self._narrowheadsmigration()
        except errormod.LockHeld:
            self.ui.debug("skipping automigrate because lock is held\n")
        except errormod.AbandonedTransactionFoundError:
            self.ui.debug("skipping automigrate due to an abandoned transaction\n")

        if not create:
            self._report_conflicting_requirements()
            if util.istest() and self.ui.configbool(
                "devel", "track-legacy-repo-format"
            ):
                self._track_legacy_repo_format()

    def _track_legacy_repo_format(self):
        if self.storage_format() != "revlog":
            return
        testdir = encoding.environ.get("RUNTESTDIR")
        path = f"{testdir}/.test-legacy-repo"
        testname = os.path.basename(encoding.environ.get("TESTFILE"))
        try:
            with open(path, "r") as f:
                if testname in f.read():
                    return
        except FileNotFoundError:
            pass
        with open(path, "a") as f:
            f.write(f"{testname}\n")

    def _report_conflicting_requirements(self):
        """Find conflicting requirements"""
        repo_types = []
        if "remotefilelog" in self.requirements:
            repo_types.append("shallow")
        if eagerepo.iseagerepo(self):
            repo_types.append("eager")
        if git.isgitstore(self):
            repo_types.append("git")
        if len(repo_types) > 1:
            hint = "check problematic logic writing to requires directly"
            raise errormod.ProgrammingError(
                f"conflicting repo types: {repo_types}\n{hint}"
            )

    def _visibilitymigration(self):
        if (
            visibility.tracking(self) != self.ui.configbool("visibility", "enabled")
        ) and not "hgsql" in self.requirements:
            with self.lock(wait=False):
                visibility.automigrate(self)

    def _svfsmigration(self):
        # Migrate 'remotenames' and 'bookmarks' state from sharedvfs to
        # storevfs.
        # This cannot be safely done in the remotenames extension because
        # changelog might access 'remotenames' and other extensions might
        # use changelog before 'remotenames.reposetup'.
        for name in ["remotenames", "bookmarks"]:
            if self.sharedvfs.exists(name) and not os.path.exists(self.svfs.join(name)):
                with self.wlock(wait=False), self.lock(wait=False):
                    data = self.sharedvfs.read(name)
                    # avoid svfs.write so it does not write into metalog.
                    util.writefile(self.svfs.join(name), data)

    def _narrowheadsmigration(self):
        """Migrate if 'narrow-heads' config has changed."""
        narrowheadsdesired = self.ui.configbool("experimental", "narrow-heads")
        # 'narrow-heads' must work with visibility and remotenames
        if (
            not self.ui.configbool("visibility", "enabled")
            or self.ui.config("extensions", "remotenames") == "!"
            or "visibleheads" not in self.storerequirements
        ):
            # Set narrow-heads to False so other code paths wouldn't try
            # to use it.
            if narrowheadsdesired:
                self.ui.setconfig("experimental", "narrow-heads", False)
            narrowheadsdesired = False
        narrowheadscurrent = "narrowheads" in self.storerequirements
        if narrowheadsdesired != narrowheadscurrent:
            if narrowheadsdesired:
                # Migrating up is easy: Just add the requirement.
                with self.lock(wait=False):
                    self.storerequirements.add("narrowheads")
                    self._writestorerequirements()
            else:
                # Migrating down to non-narrow heads requires restoring phases.
                # For this invocation, still pretend that we use narrow-heads.
                # But the next invocation will use non-narrow-heads.
                self.ui.setconfig("experimental", "narrow-heads", True)
                with self.lock(wait=False):
                    # Writing to <shared repo path>/.hg/phaseroots
                    # Accessing the raw file directly without going through
                    # complicated phasescache APIs.
                    draftroots = self.nodes("roots(draft())")
                    lines = set(self.svfs.tryread("phaseroots").splitlines(True))
                    toadd = ""
                    for root in draftroots:
                        # 1: phases.draft
                        line = "1 %s\n" % hex(root)
                        if line not in lines:
                            toadd += line
                    with self.svfs.open("phaseroots", "ab") as f:
                        f.write(toadd.encode())
                    self.storerequirements.remove("narrowheads")
                    self._writestorerequirements()

    @contextmanager
    def disableeventreporting(self):
        self._eventreporting = False
        yield
        self._eventreporting = True

    @property
    def vfs(self):
        self.ui.develwarn(
            "use of bare vfs instead of localvfs or sharedvfs", stacklevel=1
        )
        return self.localvfs

    def _getvfsward(self, origfunc):
        """build a ward for self.localvfs and self.sharedvfs"""
        rref = weakref.ref(self)

        def checkvfs(path, mode=None):
            ret = origfunc(path, mode=mode)
            repo = rref()
            if (
                repo is None
                or not hasattr(repo, "_wlockref")
                or not hasattr(repo, "_lockref")
            ):
                return
            if mode in (None, "r", "rb"):
                return
            if path.startswith(repo.path):
                # truncate name relative to the repository (.hg)
                path = path[len(repo.path) + 1 :]
            if path.startswith("cache/"):
                msg = 'accessing cache with vfs instead of cachevfs: "%s"'
                repo.ui.develwarn(msg % path, stacklevel=2, config="cache-vfs")
            if path.startswith("journal."):
                # journal is covered by 'lock'
                if repo._currentlock(repo._lockref) is None:
                    repo.ui.develwarn(
                        'write with no lock: "%s"' % path,
                        stacklevel=2,
                        config="check-locks",
                    )
            elif repo._currentlock(repo._wlockref) is None:
                # rest of vfs files are covered by 'wlock'
                #
                # exclude special files
                for prefix in self._wlockfreeprefix:
                    if path.startswith(prefix):
                        return
                repo.ui.develwarn(
                    'write with no wlock: "%s"' % path,
                    stacklevel=2,
                    config="check-locks",
                )
            return ret

        return checkvfs

    def _getsvfsward(self, origfunc):
        """build a ward for self.svfs"""
        rref = weakref.ref(self)

        def checksvfs(path, mode=None):
            ret = origfunc(path, mode=mode)
            repo = rref()
            if repo is None or not hasattr(repo, "_lockref"):
                return
            if mode in (None, "r", "rb"):
                return
            if path.startswith(repo.sharedpath):
                # truncate name relative to the repository (.hg)
                path = path[len(repo.sharedpath) + 1 :]
            if repo._currentlock(repo._lockref) is None:
                for prefix in self._lockfreeprefix:
                    if path.startswith(prefix):
                        return
                repo.ui.develwarn('write with no lock: "%s"' % path, stacklevel=3)
            return ret

        return checksvfs

    def _featuresetup(self, setupfuncs, basesupported):
        if setupfuncs:
            supported = set(basesupported)  # use private copy
            extmods = set(m.__name__ for n, m in extensions.extensions(self.ui))
            for setupfunc in setupfuncs:
                if setupfunc.__module__ in extmods:
                    setupfunc(self.ui, supported)
        else:
            supported = basesupported
        return supported

    def close(self):
        if hasattr(self, "connectionpool"):
            self.connectionpool.close()

        self.commitpending()

    def commitpending(self):
        # If we have any pending manifests, commit them to disk.
        flush_rust = False
        if "manifestlog" in self.__dict__:
            self.manifestlog.commitpending()
        else:
            flush_rust = True

        if "fileslog" in self.__dict__:
            self.fileslog.commitpending()
        else:
            flush_rust = True

        if flush_rust:
            # We have have done a pure-Rust operation that wrote to caches.
            # Flush via the Rust repo.
            self._rsrepo.flushstores()

        if "changelog" in self.__dict__ and self.changelog.isvertexlazy():
            # Errors are not fatal. We lost some caches downloaded from the
            # server triggered by `hg log` or something, but that's okay - we
            # just re-download next time.
            #
            # This might raise "bug: cannot persist with re-assigned ids
            # unresolved" in rare cases where a commit hash has two ids
            # (incorrectly): one non-master id and one master id (lazy, not
            # existed locally). Then iterating a revset like `::master` (via
            # `hg log`) triggers resolving the lazy master id. Ideally we
            # figure out why that happened (crash + doctor? missing checks
            # in lazy pull paths? race? server reports commits that should
            # exist as missing?). But for now let's just silent the error
            # as a stopgap solution for commands like `hg log`.
            #
            # Note: For write commands like `commit`, or `pull`, they go
            # through transaction, which does not silent errors.
            try:
                self.changelog.inner.flushcommitdata()
            except Exception as e:
                self.ui.log(
                    "features", feature="cannot-flush-commitdata", message=str(e)
                )

    @property
    def config(self):
        return self.ui._rcfg

    def workingcopy(self):
        return self._rsrepo.workingcopy()

    def storage_format(self):
        """return the main storage format: remotefilelog, git, eager, revlog

        In the future we might want to add lazy git storage and others.
        """
        if "remotefilelog" in self.requirements:
            return "remotefilelog"
        if git.isgitformat(self):
            return "git"
        if eagerepo.iseagerepo(self):
            return "eager"
        return "revlog"

    def _loadextensions(self):
        extensions.loadall(self.ui)

    def _restrictcapabilities(self, caps):
        if self.ui.configbool("experimental", "bundle2-advertise"):
            caps = set(caps)
            capsblob = bundle2.encodecaps(bundle2.getrepocaps(self))
            caps.add("bundle2=" + urlreq.quote(capsblob))
        return caps

    def _applyopenerreqs(self):
        self.svfs.options = dict(
            (r, 1) for r in self.requirements if r in self.openerreqs
        )
        # experimental config: format.chunkcachesize
        chunkcachesize = self.ui.configint("format", "chunkcachesize")
        if chunkcachesize is not None:
            self.svfs.options["chunkcachesize"] = chunkcachesize
        # experimental config: format.maxchainlen
        maxchainlen = self.ui.configint("format", "maxchainlen")
        if maxchainlen is not None:
            self.svfs.options["maxchainlen"] = maxchainlen
        # experimental config: format.manifestcachesize
        manifestcachesize = self.ui.configint("format", "manifestcachesize")
        if manifestcachesize is not None:
            self.svfs.options["manifestcachesize"] = manifestcachesize
        # experimental config: format.aggressivemergedeltas
        aggressivemergedeltas = self.ui.configbool("format", "aggressivemergedeltas")
        self.svfs.options["aggressivemergedeltas"] = aggressivemergedeltas
        self.svfs.options["lazydeltabase"] = not scmutil.gddeltaconfig(self.ui)
        mmapindexthreshold = self.ui.configbytes("experimental", "mmapindexthreshold")
        if mmapindexthreshold is not None:
            self.svfs.options["mmapindexthreshold"] = mmapindexthreshold

        for r in self.requirements:
            if r.startswith("exp-compression-"):
                self.svfs.options["compengine"] = r[len("exp-compression-") :]

        bypassrevlogtransaction = self.ui.configbool("experimental", "narrow-heads")
        self.svfs.options["bypass-revlog-transaction"] = bypassrevlogtransaction

    def _writerequirements(self):
        scmutil.writerequires(self.localvfs, self.requirements)
        self._rsrepo.invalidaterequires()

    def _writestorerequirements(self):
        if "store" in self.requirements:
            util.info(
                "writestorerequirements",
                requirements=" ".join(sorted(self.storerequirements)),
            )
            scmutil.writerequires(self.svfs, self.storerequirements)
            self._rsrepo.invalidaterequires()

    def peer(self):
        return localpeer(self)  # not cached to avoid reference cycle

    def _get_common_prefix(self, low, high):
        for i, c in enumerate(low):
            if high[i] != c:
                return low[:i]
        return low

    def _http_prefix_lookup(self, prefixes, reponame=None):
        if reponame:
            client = bindings.edenapi.client(self.ui._rcfg, reponame=reponame)
        else:
            client = self.edenapi

        responses = client.hashlookup(prefixes)
        for resp in responses:
            hgids = resp["hgids"]
            hashrange = resp["request"]["InclusiveRange"]
            (low, high) = hex(hashrange[0]), hex(hashrange[1])
            prefix = self._get_common_prefix(low, high)
            hgids = [hex(hgid) for hgid in hgids]
            tracing.debug(
                "edenapi hash lookups: %s" % str(hgids),
                target="pull::httphashlookup",
            )
            if len(hgids) == 0:
                raise errormod.RepoLookupError(_("%s not found!") % prefix)
            elif len(hgids) > 1:
                raise errormod.Abort(
                    _("ambiguous identifier: %s") % prefix,
                    hint=_("suggestions are:\n%s") % "\n".join(hgids),
                )
            yield bin(hgids[0])

    @util.timefunction("pull", 0, "ui")
    def pull(
        self,
        source="default",
        bookmarknames=(),
        headnodes=(),
        headnames=(),
        quiet=True,
        visible=True,
        remotebookmarks=None,
        force=False,
    ):
        """Pull specified revisions and remote bookmarks.

        - headnodes is a list of binary nodes to pull.
        - headnames is a list of text names (ex. hex prefix of a commit hash).
        - bookmarknames is a list of bookmark names to pull.
        - remotebookmarks is a map of {bookmark: node}. Instead of fetching the
          new nodes of those remote bookmarks from server, we just use the value
          from this map.

        - visible=False can disable updating visible heads. This means the pulled
        commit hashes will not be visible, although bookmarks are still updated.

        - force=true allows pulling unrelated heads (i.e. empty common)

        If a remote bookmark no longer exists on the server-side, it will be
        removed.

        This differs from the traditional pull command in a few ways:
        - Pull nothing if both bookmarknames and headnodes are empty.
        - Do not write local bookmarks.
        - Only update remote bookmarks that are explicitly specified. Do not
          save every name the server has.

        This is also done in proper ways so the remote bookmarks updated will
        match the commits pulled. The remotenames extension might fail to do so
        as it issues a second wireproto command to fetch remote bookmarks,
        which can be racy.
        """
        # All nodes are known locally. No need to pull.
        if (
            not bookmarknames
            and all(n in self for n in headnodes)
            and all(n in self for n in headnames)
        ):
            if visible:
                # Ensure unnamed commits are visible (i.e. draft, not secret).
                nodes = []
                for n in sorted(set(headnodes) | set(headnames)):
                    ctx = self[n]
                    if ctx.phase() == phases.secret:
                        nodes.append(ctx.node())
                if nodes:
                    visibility.add(self, nodes)
            return

        configoverride = {}
        if quiet:
            configoverride[("ui", "quiet")] = True

        if git.isgitpeer(self):
            # git does not support "lookup", aka. prefix match
            if headnames:
                # some headnames are 40-byte hex that are just nodes.
                nodes = []
                unknownnames = []
                for headname in headnames:
                    if len(headname) == len(nullhex):
                        try:
                            nodes.append(bin(headname))
                            continue
                        except TypeError:
                            pass
                    unknownnames.append(headname)
                if unknownnames:
                    raise errormod.Abort(
                        _("pulling %s in git repo is not supported")
                        % _(", ").join(unknownnames)
                    )
                headnodes = list(headnodes) + nodes
            with self.ui.configoverride(configoverride):
                git.pull(self, source, names=bookmarknames, nodes=headnodes)
                # recompute phases taking new pulled heads into account
                self.invalidatevolatilesets()
            return

        lockfree = self.config.get.as_bool("experimental", "lock-free-pull")

        with (
            self.conn(source) as conn,
            self.wlock(lockfree=lockfree),
            self.lock(lockfree=lockfree),
            self.transaction("pull", lockfree=lockfree),
            self.ui.configoverride(configoverride),
        ):
            remote = conn.peer
            remotenamechanges = {}  # changes to remotenames, {name: hexnode}
            heads = set()
            fastpath = []

            # Resolve the bookmark names to heads.
            if bookmarknames:
                # Convert nodes to hexnodes, so it matches the return type of bookmarks
                # api calls below
                remotebookmarks = {
                    b: hex(n) for b, n in (remotebookmarks or {}).items()
                }
                missing_bookmarknames = [
                    b for b in bookmarknames if b not in remotebookmarks
                ]
                if missing_bookmarknames:
                    if self.nullableedenapi is not None:
                        fetchedbookmarks = self.edenapi.bookmarks(missing_bookmarknames)
                        tracing.debug(
                            "edenapi fetched bookmarks: %s" % str(fetchedbookmarks),
                            target="pull::httpbookmarks",
                        )
                        remotebookmarks.update(
                            {
                                bm: n
                                for (bm, n) in fetchedbookmarks.items()
                                if n is not None
                            }
                        )
                    else:
                        remotebookmarks.update(
                            remote.listkeyspatterns(
                                "bookmarks", patterns=missing_bookmarknames
                            )
                        )

                for name in bookmarknames:
                    if name in remotebookmarks:
                        hexnode = remotebookmarks[name]
                        newnode = bin(hexnode)
                        if (
                            name == bookmarks.mainbookmark(self)
                            and self.ui.configbool("pull", "master-fastpath")
                            and "lazychangelog" in self.storerequirements
                        ):
                            # The remotenames might be stale. Try to get the
                            # head from the master group.
                            publicheads = self.dageval(lambda dag: dag.heads(public()))
                            masterheads = self.dageval(
                                lambda dag: dag.heads(dag.mastergroup())
                            )

                            # The "masterheads" might contain some
                            # "uninteresting" heads that are no longer referred
                            # by main public remote bookmarks. Try to ignore
                            # them by selecting heads that are actually
                            # referred by public remote bookmarks.
                            oldnodes = masterheads & publicheads

                            oldnode = oldnodes.last() or masterheads.last()
                            if oldnode == newnode:
                                tracing.debug(
                                    "%s: %s (unchanged)"
                                    % (
                                        name,
                                        hexnode,
                                    ),
                                    target="pull::fastpath",
                                )
                            elif oldnode is not None:
                                tracing.debug(
                                    "%s: %s => %s" % (name, hex(oldnode), hexnode),
                                    target="pull::fastpath",
                                )
                                fastpath.append(([oldnode], [newnode]))
                            else:
                                tracing.debug(
                                    "%s: new %s" % (name, hexnode),
                                    target="pull::fastpath",
                                )
                                fastpath.append(([], [newnode]))
                        heads.add(newnode)
                        remotenamechanges[name] = hexnode  # update it
                    else:
                        remotenamechanges[name] = nullhex  # delete it
            # Resolve headnames to heads.
            if headnames:
                if (
                    eagerepo.iseagerepo(self)
                    or self.ui.configbool("pull", "httphashprefix")
                ) and self.nullableedenapi is not None:
                    for name in headnames:
                        # check if headname can be a hex hash prefix
                        if any(n not in "abcdefg1234567890" for n in name.lower()):
                            raise errormod.Abort(_("%s not found!") % name)
                        if len(name) > len(nullhex):
                            raise errormod.Abort(_("%s not found!") % name)
                    heads.update(self._http_prefix_lookup(headnames))
                else:
                    batch = remote.iterbatch()
                    for name in headnames:
                        batch.lookup(name)
                    batch.submit()
                    heads.update(batch.results())

            # Merge headnodes into heads.
            for node in headnodes:
                heads.add(node)

            fastpathheads = set()
            fastpathcommits, fastpathsegments, fastpathfallbacks = 0, 0, 0
            for old, new in fastpath:
                try:
                    commits, segments = bindings.exchange.fastpull(
                        self.edenapi,
                        self.changelog.inner,
                        old,
                        new,
                    )

                    self.ui.status(
                        _("imported commit graph for %s (%s)\n")
                        % (
                            _n(f"{commits:,} commit", f"{commits:,} commits", commits),
                            _n(
                                f"{segments:,} segment",
                                f"{segments:,} segments",
                                segments,
                            ),
                        )
                    )
                    fastpathheads.update(new)
                    fastpathcommits += commits
                    fastpathsegments += segments
                except errormod.NeedSlowPathError as e:
                    fastpathfallbacks += 1
                    tracing.warn(
                        "cannot use pull fast path: %s\n" % e, target="pull::fastpath"
                    )
                except Exception as e:
                    self.ui.status_err(
                        _("failed to get fast pull data (%s), using fallback path\n")
                        % (e,)
                    )
                    fastpathfallbacks += 1

            pullheads = heads - fastpathheads

            # Filter out heads that exist in the repo.
            if pullheads:
                pullheads -= set(self.changelog.filternodes(list(pullheads)))

            self.ui.log(
                "pull",
                fastpathheads=len(fastpathheads),
                slowpathheads=len(pullheads),
                fastpathcommits=fastpathcommits,
                fastpathsegments=fastpathsegments,
                fastpathfallbacks=fastpathfallbacks,
            )

            # Only perform a pull if heads are not empty.
            if pullheads:
                # Bypass the bookmarks logic as remotenames are updated here.
                # Note: remotenamechanges contains bookmark deletion
                # information while the exchange.pull does not know about what
                # to delete.  Consider also bypass phases if narrow-heads is
                # enabled everywhere.
                # Bypass inefficient visibility updating as this function will
                # take care of them.
                extras = {
                    "bookmarks": False,
                    "obsolete": False,
                    "updatevisibility": False,
                }
                opargs = {"extras": extras}
                pullheads = sorted(pullheads)
                exchange.pull(
                    self,
                    remote,
                    pullheads,
                    force=force,
                    opargs=opargs,
                )

            # Update remotenames.
            if remotenamechanges:
                remotename = bookmarks.remotenameforurl(
                    self.ui, remote.url()
                )  # ex. 'default' or 'remote'
                # `remotename` can be None when source is a filesystem path
                if remotename is not None:
                    # saveremotenames will invalidate self.heads by bumping
                    # _remotenames.changecount, and invalidate phase sets
                    # like `public()` by calling invalidatevolatilesets.
                    bookmarks.saveremotenames(
                        self, {remotename: remotenamechanges}, override=False
                    )

            # Update visibleheads:
            if visible and heads:
                # Exclude obvious public heads (not all public heads for
                # performance). Note: legacy non-narrow-heads won't be
                # able to provide only public heads and cannot use this
                # optimization.
                if self.ui.configbool("experimental", "narrow-heads"):
                    nondraftheads = self.heads(includepublic=True, includedraft=False)
                    heads = sorted(set(heads) - set(nondraftheads))
                if heads:
                    visibility.add(self, heads)

    def conn(self, source="default", **opts):
        """Create a connection from the connection pool"""
        from . import hg  # avoid cycle

        source = hg.parseurl(self.ui.expandpath(source))
        return self.connectionpool.get(source, opts=opts)

    @repofilecache(localpaths=["shared"])
    def sharedfeatures(self):
        """Returns the set of enabled 'shared' features for this repo"""
        try:
            return set(self.localvfs.read("shared").splitlines())
        except IOError as inst:
            if inst.errno != errno.ENOENT:
                raise
            return set()

    # not checking bookmarks.current - is it necessary?
    @metalogcache("bookmarks")
    def _bookmarks(self):
        return bookmarks.bmstore(self)

    @metalogcache("remotenames")
    def _remotenames(self):
        return bookmarks.remotenames(self)

    @property
    def _activebookmark(self):
        return self._bookmarks.active

    # _phasesets depend on changelog. what we need is to call
    # _phasecache.invalidate() if '00changelog.i' was changed, but it
    # can't be easily expressed in filecache mechanism.
    @storecache()
    def _phasecache(self):
        return phases.phasecache(self, self._phasedefaults)

    @storecache()
    def changelog(self):
        # Trigger loading of the metalog, before loading changelog.
        # This avoids potential races such as metalog refers to
        # unknown commits.
        self.metalog()

        cl = _openchangelog(self)

        return cl

    def _getedenapi(self, nullable=True):
        if nullable:
            return self._rsrepo.nullableedenapi()
        else:
            return self._rsrepo.edenapi()

    @util.propertycache
    def edenapi(self):
        return self._getedenapi(nullable=False)

    @util.propertycache
    def nullableedenapi(self):
        return self._getedenapi(nullable=True)

    def edenapiwithcapabilities(self, capabilities):
        return self._rsrepo.edenapiwithcapabilities(capabilities)

    @dagcache()
    def _dagcopytrace(self):
        return bindings.copytrace.dagcopytrace(
            self.changelog.inner,
            self.manifestlog.datastore,
            self._rsrepo,
            self.fileslog.filestore,
            self.changelog.dag,
            self.ui._rcfg,
        )

    def _constructmanifest(self):
        # This is a temporary function while we migrate from manifest to
        # manifestlog. It allows bundlerepo to intercept the manifest creation.
        return manifest.manifestrevlog(self.svfs)

    @storecache()
    def manifestlog(self):
        return manifest.manifestlog(self.svfs, self)

    @util.propertycache
    def fileslog(self):
        return filelog.fileslog(self)

    @repofilecache(localpaths=["dirstate"])
    def dirstate(self) -> "dirstatemod.dirstate":
        if (
            edenfs.requirement in self.requirements
            or git.DOTGIT_REQUIREMENT in self.requirements
            or grepo.GREPO_REQUIREMENT in self.requirements
        ):
            return self._eden_dirstate

        if (
            not "treestate" in self.requirements
            and git.DOTGIT_REQUIREMENT not in self.requirements
            and grepo.GREPO_REQUIREMENT not in self.requirements
        ):
            raise errormod.RequirementError(
                f"legacy dirstate implementations are no longer supported (path={self.path}, requirements={self.requirements})"
            )

        ds = dirstatemod.dirstate(
            self.localvfs,
            self.ui,
            self.root,
            self._dirstatevalidate,
            self,
        )

        try:
            # If the dirstate was successfully loaded, let's check if it's pointed at
            # the nullid to warn the user that a clone may not have succeeded.
            if (
                self.localvfs.exists("updatestate")
                and (
                    self.ui.configbool("experimental", "nativecheckout")
                    or self.ui.configbool("clone", "nativecheckout")
                )
                and ds.p1() == nullid
            ):
                self.ui.warn(
                    _(
                        "warning: this repository appears to have not "
                        "finished cloning - run '@prog@ checkout --continue' to resume the "
                        "clone\n"
                    )
                )
        except Exception:
            pass

        return ds

    def working_parent_nodes(self) -> List[bytes]:
        """similar to workingctx._parents, but returns nodes instead of ctxs"""
        p = self.dirstate.parents()
        if p[1] == nullid:
            return p[:-1]
        else:
            return p

    @util.propertycache
    def _eden_dirstate(self) -> "dirstatemod.dirstate":
        # Disable demand import when pulling in the thrift runtime,
        # as it attempts to import missing modules and changes behavior
        # based on what it finds.  Demand import masks those and causes
        # obscure and false import errors at runtime.
        from sapling import hgdemandimport

        with hgdemandimport.deactivated():
            try:
                from . import eden_dirstate as dirstate_reimplementation
            except ImportError:
                raise errormod.Abort(
                    _("edenfs support was not available in this build")
                )

        return dirstate_reimplementation.eden_dirstate(self, self.ui, self.root)

    @util.propertycache
    def commitscheme(self):
        return commitscheme.schemes(self)

    def _dirstatevalidate(self, node: bytes) -> bytes:
        self.changelog.rev(node)
        return node

    def __getitem__(self, changeid):
        if changeid is None:
            return context.workingctx(self)
        if isinstance(changeid, slice):
            # wdirrev isn't contiguous so the slice shouldn't include it
            return [
                context.changectx(self, i) for i in range(*changeid.indices(len(self)))
            ]
        try:
            return context.changectx(self, changeid)
        except errormod.WdirUnsupported:
            return context.workingctx(self)

    def __contains__(self, changeid):
        """True if the given changeid exists

        error.LookupError is raised if an ambiguous node specified.
        """
        if self._containscount == 500:
            self.ui.develwarn("excess usage of repo.__contains__")
        self._containscount += 1

        try:
            self[changeid]
            return True
        except errormod.RepoLookupError:
            return False

    def __nonzero__(self):
        return True

    __bool__ = __nonzero__

    def __len__(self):
        return len(self.changelog)

    def __iter__(self):
        return iter(self.changelog)

    def dageval(self, func):
        """Evaluate func with the current DAG context.

        Unlike changelog.dageval, this function provides extra utilities:
        - draft(): draft commits
        - public(): public commits
        - obsolete(): obsoleted commits
        """

        def getphaseset(repo, phase):
            return repo.changelog.tonodes(repo._phasecache.getrevset(repo, [phase]))

        def removenull(nodes):
            return [n for n in nodes if n != nullid]

        return self.changelog.dageval(
            func,
            extraenv={
                "dot": lambda: removenull(self.dirstate.parents()[:1]),
                "draft": lambda: getphaseset(self, phases.draft),
                "lookup": lambda *names: removenull([self[n].node() for n in names]),
                "obsolete": lambda: mutation.obsoletenodes(self),
                "public": lambda: getphaseset(self, phases.public),
                "tonodes": self.changelog.tonodes,
            },
        )

    def revs(self, expr, *args, **kwargs):
        """Find revisions matching a revset.

        The revset is specified as a string ``expr`` that may contain
        %-formatting to escape certain types. See ``revsetlang.formatspec``.

        Note that `repo.revs(expr)` is equivalent to `repo.revs("%r", expr).

        Revset aliases from the configuration are not expanded. To expand
        user aliases, consider calling ``scmutil.revrange()`` or
        ``repo.anyrevs([expr], user=True)``.

        Returns a revset.abstractsmartset, which is a list-like interface
        that contains integer revisions.
        """
        if args:
            expr = revsetlang.formatspec(expr, *args)
        m = revset.match(None, expr)
        subset = kwargs.get("subset", None)
        return m(self, subset=subset)

    def set(self, expr, *args):
        """Find revisions matching a revset and emit changectx instances.

        This is a convenience wrapper around ``revs()`` that iterates the
        result and is a generator of changectx instances.

        Revset aliases from the configuration are not expanded. To expand
        user aliases, consider calling ``scmutil.revrange()``.
        """
        return self.revs(expr, *args).iterctx()

    @property
    def volatile_state(self):
        """Random temporary states attached to the repo object."""
        return self._rsrepo.volatile_state

    def nodes(self, expr, *args):
        """Find revisions matching a revset and emit their nodes.

        This is a convenience wrapper around ``revs()`` that iterates the
        result and is a generator of nodes.

        Revset aliases from the configuration are not expanded. To expand
        user aliases, consider calling ``scmutil.revrange()``.
        """
        clnode = self.changelog.node
        for r in self.revs(expr, *args):
            yield clnode(r)

    def anyrevs(self, specs, user=False, localalias=None):
        """Find revisions matching one of the given revsets.

        Revset aliases from the configuration are not expanded by default. To
        expand user aliases, specify ``user=True``. To provide some local
        definitions overriding user aliases, set ``localalias`` to
        ``{name: definitionstring}``.
        """
        if user:
            m = revset.matchany(self.ui, specs, repo=self, localalias=localalias)
        else:
            m = revset.matchany(None, specs, localalias=localalias)
        return m(self)

    def url(self):
        return "file:" + self.root

    def origin_url(self):
        return self.ui.config("paths", "default")

    def hook(self, name, throw=False, **args):
        """Call a hook, passing this repo instance.

        This a convenience method to aid invoking hooks. Extensions likely
        won't call this unless they have registered a custom hook or are
        replacing code that is expected to call a hook.
        """
        return hook.hook(self.ui, self, name, throw, **args)

    @util.propertycache
    def _mutationstore(self):
        return mutation.makemutationstore(self)

    def nodebookmarks(self, node):
        """return the list of bookmarks pointing to the specified node"""
        marks = []
        for bookmark, n in self._bookmarks.items():
            if n == node:
                marks.append(bookmark)
        return sorted(marks)

    def branchmap(self):
        """returns a dictionary {branch: [branchheads]} with branchheads
        ordered by increasing revision number"""
        branches = {}
        if heads := list(self.changelog.dag.sort(self.heads()).reverse()):
            branches["default"] = heads
        return branches

    def lookup(self, key):
        return self[key].node()

    def known(self, nodes):
        cl = self.changelog
        nm = cl.nodemap
        result = []
        for n in nodes:
            r = nm.get(n)
            resp = not (r is None)
            result.append(resp)
        return result

    def local(self):
        return self

    def pathhistory(self, paths, nodes):
        """Return an iterator of commit nodes that change paths.

        path in paths can be either a file or a directory.
        nodes decides the search range (ex. "::." or "_firstancestors(.)")

        This can be used for `log` operations.
        """
        use_tree_manifest = self.ui.configbool(
            "experimental", "pathhistory-treemanifest"
        )
        hist = bindings.pathhistory.pathhistory(
            nodes,
            paths,
            self.changelog.inner,
            self.manifestlog.datastore,
            self._rsrepo,
            use_tree_manifest,
        )
        return hist

    def pathcreation(self, path, nodes):
        """Return the most recent commit where path was added.

        path can be either a file or a directory.
        nodes decides the search range (ex. "::." or "_firstancestors(.)")
        """
        use_tree_manifest = self.ui.configbool(
            "experimental", "pathhistory-treemanifest"
        )
        return bindings.pathhistory.lastcreation(
            nodes,
            path,
            self.changelog.inner,
            self.manifestlog.datastore,
            self._rsrepo,
            use_tree_manifest,
        )

    def publishing(self):
        # narrow-heads repos are NOT publishing. This ensures pushing to a
        # narrow-heads repo would cause visible heads changes to make the
        # pushed commits visible. Otherwise the pushed commits are invisible
        # during discovery if the push does not update bookmarks on the commit.
        if "narrowheads" in self.storerequirements:
            return False
        # it's safe (and desirable) to trust the publish flag unconditionally
        # so that we don't finalize changes shared between users via ssh or nfs
        return self.ui.configbool("phases", "publish")

    def cancopy(self):
        return self.local()

    def draft_titles(self):
        """return a stream of (draft_node, title), used by namespace"""
        rgen = self._draft_titles_gen
        return rgen.iter()

    @metalogcache("visibleheads", "remotenames")
    def _draft_titles_gen(self):
        """cached generator that yields (draft_node, title).

        Lazily load and cached content:
        - Only titles of the iterated commits are read.
        - Already read commit titles are cached (by metalogcache and
          RGenerator) so they won't be read again by the next iteration
          via `draft_titles`.
        """
        # allow user override via revsetalias._interestingdraft config.
        draftrevs = self.anyrevs(["_interestingdraft()"], user=True).prefetch("text")

        def gen():
            for c in draftrevs.iterctx():
                yield c.node(), c.description().split("\n", 1)[0].lower()

        # Wrap in the RGenerator so it can be iterated through multiple times
        # and be cached.
        rgen = bindings.threading.RGenerator(gen())
        return rgen

    def shared(self):
        """the type of shared repository (None if not shared)"""
        if self.sharedpath != self.path:
            return "store"
        return None

    def wjoin(self, f, *insidef):
        return self.localvfs.reljoin(self.root, f, *insidef)

    def file(self, f):
        if f[0] == "/":
            f = f[1:]
        if git.isgitstore(self):
            return git.gitfilelog(self)
        elif eagerepo.iseagerepo(self):
            return eagerepo.eagerfilelog(self, f)
        return filelog.filelog(self.svfs, f)

    def changectx(self, changeid):
        return self[changeid]

    def setparents(self, p1, p2=nullid):
        with self.dirstate.parentchange():
            copies = self.dirstate.setparents(p1, p2)
            pctx = self[p1]
            if copies:
                # Adjust copy records, the dirstate cannot do it, it
                # requires access to parents manifests. Preserve them
                # only for entries added to first parent.
                for f in copies:
                    if f not in pctx and copies[f] in pctx:
                        self.dirstate.copy(copies[f], f)
            if p2 == nullid:
                for f, s in sorted(self.dirstate.copies().items()):
                    if f not in pctx and s not in pctx:
                        self.dirstate.copy(None, f)

    def filectx(self, path, changeid=None, fileid=None):
        """changeid can be a changeset revision or node.
        fileid can be a file revision or node."""
        return context.filectx(self, path, changeid, fileid)

    def getcwd(self):
        return self.dirstate.getcwd()

    def pathto(self, f, cwd=None):
        return self.dirstate.pathto(f, cwd)

    def wread(self, filename):
        if self.wvfs.islink(filename):
            return self.wvfs.readlink(filename).encode()
        else:
            return self.wvfs.read(filename)

    def wwrite(
        self, filename: str, data: bytes, flags: str, backgroundclose: bool = False
    ) -> int:
        """write ``data`` into ``filename`` in the working directory

        This returns length of written (maybe decoded) data.
        """
        if "l" in flags:
            self.wvfs.symlink(data, filename)
        else:
            self.wvfs.write(filename, data, backgroundclose=backgroundclose)
            if "x" in flags:
                self.wvfs.setflags(filename, False, True)
        return len(data)

    def currenttransaction(self):
        """return the current transaction or None if non exists"""
        if self._transref:
            tr = self._transref()
        else:
            tr = None

        if tr and tr.running():
            return tr
        return None

    def _is_within_lockfree_transaction(self):
        tr = self.currenttransaction()
        return tr is not None and tr.lockfree

    def transaction(self, desc, report=None, lockfree=False):
        tr = self.currenttransaction()
        if tr is not None and tr.lockfree:
            lockfree = True
        if not lockfree and (
            self.ui.configbool("devel", "all-warnings")
            or self.ui.configbool("devel", "check-locks")
        ):
            if self._currentlock(self._lockref) is None:
                raise errormod.ProgrammingError("transaction requires locking")
        if tr is not None:
            return tr.nest(desc)

        # An existing journal means a previous transaction was interrupted.
        # Lock-free transactions do not own the journal file.
        if not lockfree and self.svfs.exists("journal"):
            self.recover()

        idbase = b"%.40f#%f" % (random.random(), time.time())
        ha = hex(hashlib.sha1(idbase).digest())
        txnid = "TXN:" + ha
        self.hook("pretxnopen", throw=True, txnname=desc, txnid=txnid)

        # Lock-free transactions do not create fixed journal files.
        if not lockfree:
            self._writejournal(desc)
            renames = [(vfs, x, undoname(x)) for vfs, x in self._journalfiles()]
        else:
            renames = []
        if report:
            rp = report
        else:
            rp = self.ui.warn
        vfsmap = {"shared": self.sharedvfs, "local": self.localvfs}
        # we must avoid cyclic reference between repo and transaction.
        reporef = weakref.ref(self)

        def validate(tr2):
            """will run pre-closing hooks"""
            # XXX the transaction API is a bit lacking here so we take a hacky
            # path for now
            #
            # We cannot add this as a "pending" hooks since the 'tr.hookargs'
            # dict is copied before these run. In addition we needs the data
            # available to in memory hooks too.
            #
            # Moreover, we also need to make sure this runs before txnclose
            # hooks and there is no "pending" mechanism that would execute
            # logic only if hooks are about to run.
            #
            # Fixing this limitation of the transaction is also needed to track
            # other families of changes (bookmarks, phases, obsolescence).
            #
            # This will have to be fixed before we remove the experimental
            # gating.
            repo = reporef()
            if hook.hashook(repo.ui, "pretxnclose-bookmark"):
                for name, (old, new) in sorted(tr.changes["bookmarks"].items()):
                    args = tr.hookargs.copy()
                    args.update(bookmarks.preparehookargs(name, old, new))
                    repo.hook("pretxnclose-bookmark", throw=True, txnname=desc, **args)
            if hook.hashook(repo.ui, "pretxnclose-phase"):
                cl = repo.changelog
                for rev, (old, new) in tr.changes["phases"].items():
                    args = tr.hookargs.copy()
                    node = hex(cl.node(rev))
                    args.update(phases.preparehookargs(node, old, new))
                    repo.hook("pretxnclose-phase", throw=True, txnname=desc, **args)

            repo.hook("pretxnclose", throw=True, txnname=desc, **(tr.hookargs))

            # Remove visible heads that become public, since we now have
            # up-to-date remotenames.
            cl = repo.changelog
            heads = cl._visibleheads.heads
            draftheads = list(repo.dageval(lambda: heads - public()))
            cl._visibleheads.heads = draftheads

            # Write a dirty dirstate as part of the transaction, before the
            # metalog is written and exported (ex. to git refs), so working
            # copy clients such as EdenFS or the git index see the parent
            # change first.
            if not tr.lockfree and not repo.dirstate.pendingparentchange():
                repo.dirstate.write(tr)

            # Flush changelog before flushing metalog.
            _flushchangelog(repo)

        def _flushchangelog(repo):
            cl = repo.changelog

            if (
                repo.ui.configbool("devel", "segmented-changelog-rev-compat")
                and "lazychangelog" not in repo.storerequirements
            ):
                # Preserve the revlog revision number compatibility.  This can
                # cause fragmentation in the master group, hurt performance.
                # However, it produces the same revision numbers as if the
                # backend is revlog. This helps with test compatibility for
                # older tests using revision numbers directly. Due to the
                # performance penalty, this should only be used in tests.
                #
                # Implemented by flushing dirty nodes in insertion order to the
                # master group. It is incompatible with "lazychangelog", since
                # "lazychangelog" assumes commits in the master group are lazy
                # and resolvable by the server, which is no longer true if we
                # force local "hg commit" commits in the master group.
                assert util.istest(), (
                    "devel.segmented-changelog-rev-compat should not be used outside tests"
                )
                cl.inner.flush(list(cl.dag.dirty().iterrev()))
                return

            # Flush changelog. At this time remotenames should be up-to-date.
            # We need to write out changelog before remotenames so remotenames
            # do not have dangling pointers.
            mainnodes = []
            main = bookmarks.mainbookmark(repo)
            if main:
                # Explicitly resolve 'main' in the 'hoistednames' and
                # 'bookmarks' namespace only. The "bookmarks" namespace is
                # needed by some tests and paths, unfortunately.
                try:
                    main_node = repo.names.singlenode(
                        repo, main, {"hoistednames", "bookmarks"}
                    )
                    if main_node != nullid:
                        mainnodes.append(main_node)
                except KeyError:
                    # no such namespace, or not resolved in namespace
                    pass
            cl.inner.flush(mainnodes)

            # flush(mainnodes) might reassign ids that makes the cached `public()`
            # incompatible (force slow paths). Invalidate cached `public()` to
            # avoid slow paths.
            # Note: invalidation is not done in the phase cache. See D47478975
            # and S353203.
            new_version = cl.dag.version()
            old_version = repo.dageval(lambda: public()).hints().get("dag_version")
            if old_version is None or old_version.cmp(new_version) is None:
                repo.invalidatevolatilesets()

        def writependingchangelog(tr):
            repo = reporef()
            _flushchangelog(repo)
            return False

        def releasefn(tr, success):
            repo = reporef()
            if repo is None:
                # In __del__, the repo is no longer valid.
                return
            if success:
                # Lock-free transactions do not modify dirstate and might not
                # hold the working copy lock required to write it.
                if not tr.lockfree:
                    # this should be explicitly invoked here, because
                    # in-memory changes aren't written out at closing
                    # transaction, if tr.addfilegenerator (via
                    # dirstate.write or so) isn't invoked while
                    # transaction running
                    repo.dirstate.write(None)
                repo._txnreleased = True
            else:
                # discard all changes (including ones already written
                # out) in this transaction
                if tr is None or not tr.lockfree:
                    repo.dirstate.restorebackup(None, "journal.dirstate")

                repo.invalidate(clearfilecache=True)
                repo.invalidatemetalog()

        tr = transaction.transaction(
            rp,
            self.svfs,
            vfsmap,
            "journal",
            "undo",
            aftertrans(reporef, renames),
            self.store.createmode,
            validator=validate,
            releasefn=releasefn,
            checkambigfiles=_cachedfiles,
            uiconfig=self.ui.uiconfig(),
            desc=desc,
            lockfree=lockfree,
            pendingroot=self.root,
            sharedpendingroot=self.sharedroot,
        )
        tr.changes["nodes"] = []
        tr.changes["obsmarkers"] = set()
        tr.changes["phases"] = {}
        tr.changes["bookmarks"] = {}

        tr.hookargs["txnid"] = txnid

        # TODO: Consider changing 'self' to 'reporef()'.

        def commitnotransaction(tr):
            self.commitpending()

        def abortnotransaction(tr):
            if "manifestlog" in self.__dict__:
                self.manifestlog.abortpending()

            if "fileslog" in self.__dict__:
                self.fileslog.abortpending()

        def writependingnotransaction(tr):
            commitnotransaction(tr)
            tr.addpending("notransaction", writependingnotransaction)
            return False

        tr.addfinalize("notransaction", commitnotransaction)
        tr.addabort("notransaction", abortnotransaction)
        tr.addpending("notransaction", writependingnotransaction)
        tr.addpending("writependingchangelog", writependingchangelog)

        # If any writes happened outside the transaction, go ahead and flush
        # them before opening the new transaction.
        commitnotransaction(None)

        def txnclosehook(tr2):
            """To be run if transaction is successful, will schedule a hook run"""
            # Don't reference tr2 in hook() so we don't hold a reference.
            # This reduces memory consumption when there are multiple
            # transactions per lock. This can likely go away if issue5045
            # fixes the function accumulation.
            hookargs = tr2.hookargs

            def hookfunc():
                repo = reporef()
                if hook.hashook(repo.ui, "txnclose-bookmark"):
                    bmchanges = sorted(tr.changes["bookmarks"].items())
                    for name, (old, new) in bmchanges:
                        args = tr.hookargs.copy()
                        args.update(bookmarks.preparehookargs(name, old, new))
                        repo.hook(
                            "txnclose-bookmark", throw=False, txnname=desc, **args
                        )

                if hook.hashook(repo.ui, "txnclose-phase"):
                    cl = repo.changelog
                    phasemv = sorted(tr.changes["phases"].items())
                    for rev, (old, new) in phasemv:
                        args = tr.hookargs.copy()
                        node = hex(cl.node(rev))
                        args.update(phases.preparehookargs(node, old, new))
                        repo.hook("txnclose-phase", throw=False, txnname=desc, **args)

                repo.hook("txnclose", throw=False, txnname=desc, **hookargs)

            if tr2.lockfree:
                # There is no lock to defer the hook through. Run it after the
                # transaction has committed its final metalog root instead.
                tr2.addpostclose("txnclose-hook", lambda _tr: hookfunc())
            else:
                reporef()._afterlock(hookfunc)

        tr.addfinalize("txnclose-hook", txnclosehook)
        tr.addpostclose("warms-cache", self._buildcacheupdater(tr))

        def txnaborthook(tr2):
            """To be run if transaction is aborted"""
            repo = reporef()
            if repo is None:
                # repo was released (by __del__)
                return
            repo.hook("txnabort", throw=False, txnname=desc, **tr2.hookargs)

        tr.addabort("txnabort-hook", txnaborthook)
        # avoid eager cache invalidation. in-memory data should be identical
        # to stored data if transaction has no error.
        tr.addpostclose("refresh-filecachestats", self._refreshfilecachestats)
        self._transref = weakref.ref(tr)
        return tr

    # Flush pending changelog/store writes. Used to make unflushed
    # changes visible to EdenFS.
    def flushpendingtransaction(self):
        if tx := self.currenttransaction():
            tx.writepending()

    def _journalfiles(self):
        return (
            (self.svfs, "journal"),
            (self.localvfs, "journal.dirstate"),
            (self.localvfs, "journal.branch"),
            (self.localvfs, "journal.desc"),
            (self.svfs, "journal.bookmarks"),
            (self.svfs, "journal.phaseroots"),
            (self.svfs, "journal.visibleheads"),
        )

    def undofiles(self):
        return [(vfs, undoname(x)) for vfs, x in self._journalfiles()]

    def _writejournal(self, desc):
        self.dirstate.savebackup(None, "journal.dirstate")
        self.localvfs.writeutf8("journal.branch", "default")
        self.localvfs.writeutf8("journal.desc", "%d\n%s\n" % (len(self), desc))
        self.svfs.write("journal.bookmarks", self.svfs.tryread("bookmarks"))
        self.svfs.write("journal.phaseroots", self.svfs.tryread("phaseroots"))

    def recover(self):
        with self.lock():
            if self.svfs.exists("journal"):
                self.ui.debug("rolling back interrupted transaction\n")
                vfsmap = {
                    "": self.svfs,
                    "shared": self.sharedvfs,
                    "local": self.localvfs,
                }
                transaction.rollback(
                    self.svfs,
                    vfsmap,
                    "journal",
                    self.ui.warn,
                    checkambigfiles=_cachedfiles,
                )
                self.invalidate()
                return True
            else:
                self.ui.warn(_("no interrupted transaction available\n"))
                return False

    def rollback(self, dryrun=False, force=False):
        wlock = lock = dsguard = None
        try:
            wlock = self.wlock()
            lock = self.lock()
            if self.svfs.exists("undo"):
                dsguard = dirstateguard.dirstateguard(self, "rollback")

                return self._rollback(dryrun, force, dsguard)
            else:
                self.ui.warn(_("no rollback information available\n"))
                return 1
        finally:
            release(dsguard, lock, wlock)

    def _rollback(self, dryrun, force, dsguard):
        ui = self.ui
        try:
            args = self.localvfs.read("undo.desc").decode().splitlines()
            (oldlen, desc, detail) = (int(args[0]), args[1], None)
            if len(args) >= 3:
                detail = args[2]
            oldtip = oldlen - 1

            if detail and ui.verbose:
                msg = _("repository tip rolled back to revision %d (undo %s: %s)\n") % (
                    oldtip,
                    desc,
                    detail,
                )
            else:
                msg = _("repository tip rolled back to revision %d (undo %s)\n") % (
                    oldtip,
                    desc,
                )
        except IOError:
            msg = _("rolling back unknown transaction\n")
            desc = None

        if not force and self["."] != self["tip"] and desc == "commit":
            raise errormod.Abort(
                _("rollback of last commit while not checked out may lose data"),
                hint=_("use -f to force"),
            )

        ui.status(msg)
        if dryrun:
            return 0

        parents = self.dirstate.parents()
        self.destroying()
        vfsmap = {"": self.svfs, "shared": self.sharedvfs, "local": self.localvfs}
        transaction.rollback(
            self.svfs, vfsmap, "undo", ui.warn, checkambigfiles=_cachedfiles
        )
        if self.svfs.exists("undo.bookmarks"):
            self.svfs.rename("undo.bookmarks", "bookmarks", checkambig=True)
        if self.svfs.exists("undo.phaseroots"):
            self.svfs.rename("undo.phaseroots", "phaseroots", checkambig=True)
        self.invalidate()

        parentgone = (
            parents[0] not in self.changelog.nodemap
            or parents[1] not in self.changelog.nodemap
        )
        if parentgone:
            # prevent dirstateguard from overwriting already restored one
            dsguard.close()

            self.dirstate.restorebackup(None, "undo.dirstate")

            parents = tuple([p.rev() for p in self[None].parents()])
            if len(parents) > 1:
                ui.status(
                    _("working directory now based on revisions %d and %d\n") % parents
                )
            else:
                ui.status(_("working directory now based on revision %d\n") % parents)
            mergemod.mergestate.clean(self, self["."].node())

        # TODO: if we know which new heads may result from this rollback, pass
        # them to destroy(), which will prevent the branchhead cache from being
        # invalidated.
        self.destroyed()
        return 0

    def _buildcacheupdater(self, newtransaction):
        """called during transaction to build the callback updating cache

        Lives on the repository to help extension who might want to augment
        this logic. For this purpose, the created transaction is passed to the
        method.
        """
        # we must avoid cyclic reference between repo and transaction.
        reporef = weakref.ref(self)

        def updater(tr):
            repo = reporef()
            repo.updatecaches(tr)

        return updater

    def updatecaches(self, tr=None):
        """warm appropriate caches

        If this function is called after a transaction closed. The transaction
        will be available in the 'tr' argument. This can be used to selectively
        update caches relevant to the changes in that transaction.
        """
        if tr is not None and tr.hookargs.get("source") == "strip":
            # During strip, many caches are invalid but
            # later call to `destroyed` will refresh them.
            return

    def invalidatecaches(self):
        self._branchcaches.clear()
        self.invalidatevolatilesets()

    def invalidatevolatilesets(self):
        mutation.clearobsoletecache(self)
        if "_phasecache" in self._filecache and "_phasecache" in self.__dict__:
            self._phasecache.invalidate()

    def invalidatedirstate(self):
        """Invalidates the dirstate, causing the next call to dirstate
        to check if it was modified since the last time it was read,
        rereading it if it has.

        This is different to dirstate.invalidate() that it doesn't always
        rereads the dirstate. Use dirstate.invalidate() if you want to
        explicitly read the dirstate again (i.e. restoring it to a previous
        known good state)."""
        # eden_dirstate has its own invalidation logic.
        if (
            edenfs.requirement in self.requirements
            or git.DOTGIT_REQUIREMENT in self.requirements
            or grepo.GREPO_REQUIREMENT in self.requirements
        ):
            self.dirstate.invalidate()
            return

        if hascache(self, "dirstate"):
            for k in self.dirstate._filecache:
                try:
                    delattr(self.dirstate, k)
                except AttributeError:
                    pass
            delattr(self, "dirstate")

    def flushandinvalidate(self, clearfilecache=False):
        self.commitpending()
        self.invalidate(clearfilecache=clearfilecache)

    def invalidate(self, clearfilecache=False):
        """Invalidates both store and non-store parts other than dirstate

        If a transaction is running, invalidation of store is omitted,
        because discarding in-memory changes might cause inconsistency
        (e.g. incomplete fncache causes unintentional failure, but
        redundant one doesn't).
        """
        self._headcache.clear()
        for k in list(self._filecache.keys()):
            # dirstate is invalidated separately in invalidatedirstate()
            if k == "dirstate":
                continue
            if k == "changelog":
                if self.currenttransaction():
                    # The changelog object may store unwritten revisions. We don't
                    # want to lose them.
                    # TODO: Solve the problem instead of working around it.
                    continue
                else:
                    # Use dedicated function to properly invalidate changelog.
                    self.invalidatechangelog()
                    continue

            if k == "manifestlog" and "manifestlog" in self.__dict__:
                # The manifestlog may have uncommitted additions, let's just
                # flush them to disk so we don't lose them.
                self.manifestlog.commitpending()

            if clearfilecache:
                del self._filecache[k]
            try:
                delattr(self, k)
            except AttributeError:
                pass

        if "fileslog" in self.__dict__:
            # The fileslog may have uncommitted additions, let's just
            # flush them to disk so we don't lose them.
            self.fileslog.commitpending()
            del self.__dict__["fileslog"]

        self.invalidatecaches()
        if not self.currenttransaction():
            # TODO: Changing contents of store outside transaction
            # causes inconsistency. We should make in-memory store
            # changes detectable, and abort if changed.
            self.store.invalidatecaches()

    def invalidatechangelog(self):
        """Invalidates the changelog. Discard pending changes."""

        if "changelog" in self._filecache:
            del self._filecache["changelog"]
        if "changelog" in self.__dict__:
            del self.__dict__["changelog"]
        # The 'revs' might have been changed (ex. changelog migrated to a
        # different form).
        self._headcache.clear()
        # Do not construct the phasecache here: its construction reads heads,
        # hence bookmarks, and bmstore.__init__ is a caller of this method.
        phasecache, cached = isfilecached(self, "_phasecache")
        if cached:
            phasecache.invalidate()
        self.invalidatedagcopytrace()

    def invalidatedagcopytrace(self):
        self.__dict__.pop("_dagcopytrace", None)

    def invalidatemetalog(self):
        """Invalidates the metalog. Discard pending changes."""
        self.svfs.invalidatemetalog()
        # The bookmark store is changed in place while a transaction runs and
        # only written to the metalog when it closes, so after a rollback its
        # metalog cache key still matches and it has to be dropped explicitly.
        self.__dict__.pop("_bookmarks", None)

    def invalidateall(self):
        """Fully invalidates both store and non-store parts, causing the
        subsequent operation to reread any outside changes."""
        # extension should hook this to invalidate its caches
        # Order matters. Invalidate changelog first so loading dirstate will
        # pick up new commits in the changelog.
        self.invalidatechangelog()
        # Trigger _rsrepo.invalidatechangelog()
        self.changelog
        self.invalidate()
        self.invalidatedirstate()
        self.invalidatemetalog()

    def _refreshfilecachestats(self, tr):
        """Reload stats of cached files so that they are flagged as valid"""
        for k, ce in self._filecache.items():
            if k == "dirstate" or k not in self.__dict__:
                continue
            ce.refresh()

    def _lock(
        self,
        vfs,
        lockname,
        wait,
        releasefn,
        acquirefn,
        desc,
    ):
        timeout = 0
        warntimeout = None
        if wait:
            timeout = self.ui.configint("ui", "timeout")
            warntimeout = self.ui.configint("ui", "timeout.warn")

        # Defer to Rust to acquire repo and wc locks. This allows lock sharing
        # between Python and Rust.
        trylockfn = None
        if lockname == "lock":
            trylockfn = self._rsrepo.trylock
        elif lockname == "wlock":
            trylockfn = partial(self._rsrepo.trywlock, vfs.base)

        return lockmod.trylock(
            self.ui,
            vfs,
            lockname,
            timeout,
            warntimeout,
            releasefn=releasefn,
            acquirefn=acquirefn,
            desc=desc,
            trylockfn=trylockfn,
        )

    def _afterlock(self, callback):
        """add a callback to be run when the repository is fully unlocked

        The callback will be executed when the outermost lock is released
        (with wlock being higher level than 'lock')."""
        for ref in (self._wlockref, self._lockref):
            l = ref and ref()
            if l and l.held:
                l.postrelease.append(callback)
                break
        else:  # no lock have been found.
            callback()

    def lock(self, wait=True, lockfree=False):
        """Lock the repository store (.hg/store) and return a weak reference
        to the lock. Use this before modifying the store (e.g. committing or
        stripping). If you are opening a transaction, get a lock as well.)

        If both 'lock' and 'wlock' must be acquired, ensure you always acquires
        'wlock' first to avoid a dead-lock hazard.

        Returns 'nullcontextmanager' without reading or writing on-disk locks,
        if the current active transaction is marked as lockfree.

        If lockfree is True, returns util.nullcontextmanager() without waiting.
        """
        if lockfree:
            return util.nullcontextmanager()

        if self._is_within_lockfree_transaction():
            return util.nullcontextmanager()

        l = self._currentlock(self._lockref)
        if l is not None:
            l.lock()
            return l

        if self.ui.configbool("devel", "debug-lockers"):
            util.debugstacktrace(
                "acquiring store lock for %s" % self.root, skip=1, depth=5
            )

        # Invalidate metalog state.
        metalog = self.metalog()
        if metalog.isdirty():
            # |<- A ->|<----------- repo lock --------->|
            #         |<- B ->|<- transaction ->|<- C ->|
            #  ^^^^^^^
            raise errormod.ProgrammingError(
                "metalog should not be changed before repo.lock"
            )

        def metalogdirtycheck(metalog=metalog):
            # |<- A ->|<----------- repo lock --------->|
            #         |<- B ->|<- transaction ->|<- C ->|
            #                                    ^^^^^^^
            if metalog.isdirty():
                raise errormod.ProgrammingError(
                    "metalog change outside a transaction is unsupported"
                )

        releasefn = metalogdirtycheck

        l = self._lock(
            self.svfs,
            "lock",
            wait,
            releasefn,
            self.flushandinvalidate,
            _("repository %s") % self.origroot,
        )
        self._lockref = weakref.ref(l)
        return l

    def wlock(self, wait=True, lockfree=False):
        """Lock the non-store parts of the repository (everything under
        .hg except .hg/store) and return a weak reference to the lock.

        Use this before modifying files in .hg.

        If both 'lock' and 'wlock' must be acquired, ensure you always acquires
        'wlock' first to avoid a dead-lock hazard.

        If lockfree is True, returns util.nullcontextmanager() without waiting.
        """
        if lockfree:
            return util.nullcontextmanager()

        if self._is_within_lockfree_transaction():
            raise errormod.ProgrammingError(
                "wlock inside lockfree transaction is not currently allowed",
            )
        l = self._wlockref and self._wlockref()
        if l is not None and l.held:
            l.lock()
            return l

        if self.ui.configbool("devel", "debug-lockers"):
            util.debugstacktrace("acquiring wlock for %s" % self.root, skip=1, depth=5)

        # We do not need to check for non-waiting lock acquisition.  Such
        # acquisition would not cause dead-lock as they would just fail.
        if wait and (
            self.ui.configbool("devel", "all-warnings")
            or self.ui.configbool("devel", "check-locks")
        ):
            if self._currentlock(self._lockref) is not None:
                self.ui.develwarn('"wlock" acquired after "lock"')

        # if this is a shared repo and we must also lock the shared wlock
        # or else we can deadlock due to lock ordering issues
        #
        # If this is Git submodule repo, then the backing repo is not intended
        # to be used as a working copy. Let's skip the sharedwlock to avoid
        # deadlock. See test-git-submodule-loop.t.
        sharedwlock = None
        if (
            self.shared()
            and self.submodule is None
            # Escape hatch in case we need to bring below locking back.
            and self.ui.configbool("experimental", "allow-shared-wlock")
        ):
            sharedwlock = self._lock(
                self.sharedvfs,
                "wlock",
                wait,
                None,
                None,
                _("shared repository %s") % self.sharedroot,
            )

        def unlock():
            if sharedwlock:
                sharedwlock.release()
            if self.dirstate.pendingparentchange():
                self.dirstate.invalidate()
            else:
                self.dirstate.write(None)

            self._filecache["dirstate"].refresh()

        l = self._lock(
            self.localvfs,
            "wlock",
            wait,
            unlock,
            self.invalidatedirstate,
            _("working directory of %s") % self.origroot,
        )
        self._wlockref = weakref.ref(l)
        return l

    def _currentlock(self, lockref):
        """Returns the lock if it's held, or None if it's not."""
        if lockref is None:
            return None
        l = lockref()
        if l is None or not l.held:
            return None
        return l

    def currentwlock(self):
        """Returns the wlock if it's held, or None if it's not."""
        return self._currentlock(self._wlockref)

    def _filecommit(self, fctx, manifest1, manifest2, linkrev, tr, changelist):
        """
        commit an individual file as part of a larger transaction
        """

        fname = fctx.path()
        fparent1 = manifest1.get(fname, nullid)
        fparent2 = manifest2.get(fname, nullid)
        node = fctx.filenode()
        if node in [fparent1, fparent2]:
            self.ui.debug("reusing %s filelog entry (parent match)\n" % fname)
            if node == fparent1:
                if manifest1.flags(fname) != fctx.flags():
                    changelist.append(fname)
            else:
                if manifest2.flags(fname) != fctx.flags():
                    changelist.append(fname)
            return node

        flog = self.file(fname)
        meta = {}
        copy = fctx.renamed()

        # Is filelog metadata (currently only the copy information) unchanged?
        # If it is, then filenode hash could be unchanged if data is also known
        # unchanged. This allows fast path of adding new file nodes without
        # calculating .data() or new hash.
        #
        # Metadata is unchanged if there is no copy information.
        # Otherwise, the "copy from" revision needs to be checked.
        metamatched = not bool(copy)

        if copy and copy[0] != fname:
            # Mark the new revision of this file as a copy of another
            # file.  This copy data will effectively act as a parent
            # of this new revision.  If this is a merge, the first
            # parent will be the nullid (meaning "look up the copy data")
            # and the second one will be the other parent.  For example:
            #
            # 0 --- 1 --- 3   rev1 changes file foo
            #   \       /     rev2 renames foo to bar and changes it
            #    \- 2 -/      rev3 should have bar with all changes and
            #                      should record that bar descends from
            #                      bar in rev2 and foo in rev1
            #
            # this allows this merge to succeed:
            #
            # 0 --- 1 --- 3   rev4 reverts the content change from rev2
            #   \       /     merging rev3 and rev4 should use bar@rev2
            #    \- 2 --- 4        as the merge base
            #

            cfname, oldcrev = copy
            crev = manifest1.get(cfname)
            newfparent = fparent2

            if manifest2:  # branch merge
                if fparent2 == nullid or crev is None:  # copied on remote side
                    if cfname in manifest2:
                        crev = manifest2[cfname]
                        newfparent = fparent1

            # Here, we used to search backwards through history to try to find
            # where the file copy came from if the source of a copy was not in
            # the parent directory. However, this doesn't actually make sense to
            # do (what does a copy from something not in your working copy even
            # mean?) and it causes bugs (eg, issue4476). Instead, we will warn
            # the user that copy information was dropped, so if they didn't
            # expect this outcome it can be fixed, but this is the correct
            # behavior in this circumstance.

            if crev:
                self.ui.debug(" %s: copy %s:%s\n" % (fname, cfname, hex(crev)))
                meta["copy"] = cfname
                meta["copyrev"] = hex(crev)
                metamatched = crev == oldcrev
                fparent1, fparent2 = nullid, newfparent
            else:
                self.ui.warn(
                    _("warning: can't find ancestor for '%s' copied from '%s'!\n")
                    % (fname, cfname)
                )

        elif fparent1 == nullid:
            fparent1, fparent2 = fparent2, nullid
        elif fparent2 != nullid:
            # is one parent an ancestor of the other?
            fparentancestors = flog.commonancestorsheads(fparent1, fparent2)
            if fparent1 in fparentancestors:
                fparent1, fparent2 = fparent2, nullid
            elif fparent2 in fparentancestors:
                fparent2 = nullid

        # Fast path: reuse rawdata? (skip .data() (ex. flagprocessors) and hash
        # calculation)
        if (
            metamatched
            and node is not None
            and hasattr(flog, "nodemap")
            and fctx.filelog().parents(node) == (fparent1, fparent2)
        ):
            if (
                # Assume that if the above `parents(node)` call finds data for node, then
                # node must already exist in the filelog. For remote entries, parents()
                # comes from the server, so it should hold. For local entries, we insert
                # into the "dpack" before "hpack", so I think it also holds.
                (
                    self.ui.configbool("experimental", "infer-filenode-available", True)
                    and "remotefilelog" in self.requirements
                )
                or node in flog.nodemap
            ):
                changelist.append(fname)
                self.ui.debug("reusing %s filelog node (exact match)\n" % fname)
                return node

            if (
                # some filectxs do not support rawdata or flags
                hasattr(fctx, "rawdata")
                and hasattr(fctx, "rawflags")
                # some (external) filelogs do not have addrawrevision
                and hasattr(flog, "addrawrevision")
                # parents must match to be able to reuse rawdata
            ):
                # node is different from fparents, no need to check manifest flag
                changelist.append(fname)
                self.ui.debug("reusing %s filelog rawdata\n" % fname)
                return flog.addrawrevision(
                    fctx.rawdata(),
                    tr,
                    linkrev,
                    fparent1,
                    fparent2,
                    node,
                    fctx.rawflags(),
                )

        # is the file changed?
        text = fctx.data()
        if (
            util.iswindows
            and "windowssymlinks" in self.requirements
            and "l" in fctx.flags()
        ):
            text = text.replace(b"\\", b"/")
        if fparent2 != nullid or flog.cmp(fparent1, text) or meta:
            changelist.append(fname)
            return flog.add(text, meta, tr, linkrev, fparent1, fparent2)
        # are just the flags changed during merge?
        elif fname in manifest1 and manifest1.flags(fname) != fctx.flags():
            changelist.append(fname)

        return fparent1

    def _filecommitgit(self, fctx) -> bytes:
        fnode = git.submodule_node_from_fctx(fctx)
        if fnode is not None:
            return fnode
        store = self.fileslog.abstracted_file_store()
        opts = {}
        data = fctx.data()
        path = ""  # git does not need a path
        fnode = store.insert_file(opts, path, data)
        return fnode

    def checkcommitpatterns(self, wctx, match, status, fail):
        """check for commit arguments that aren't committable"""
        if match.isexact() or match.prefix():
            matched = set(status.modified + status.added + status.removed)

            for f in match.files():
                f = self.dirstate.normalize(f)
                if f == "." or f in matched:
                    continue
                if f in status.deleted:
                    fail(f, _("file not found!"))

                d = f + "/"
                for mf in matched:
                    if mf.startswith(d):
                        break
                else:
                    if self.wvfs.isdir(f):
                        fail(f, _("no match under directory!"))
                    elif f not in self.dirstate:
                        fail(f, _("file not tracked!"))

    def commit(
        self,
        text="",
        user=None,
        date=None,
        match=None,
        force=False,
        editor=False,
        extra=None,
        loginfo=None,
        mutinfo=None,
    ):
        """Add a new revision to current repository.

        Revision information is gathered from the working directory,
        match can be used to filter the committed files. If editor is
        supplied, it is called to get a commit message.
        """
        if extra is None:
            extra = {}

        if loginfo is None:
            loginfo = {}

        def fail(f, msg):
            raise errormod.Abort("%s: %s" % (f, msg))

        if not match:
            match = matchmod.always(self.root, "")

        if not force:
            match.bad = fail

        wlock = lock = tr = None
        try:
            wlock = self.wlock()
            lock = self.lock()  # for recent changelog (see issue4368)

            wctx = self[None]
            merge = len(wctx.parents()) > 1

            if not force and merge and not match.always():
                raise errormod.Abort(
                    _(
                        "cannot partially commit a merge "
                        "(do not specify files or patterns)"
                    )
                )

            status = self.status(match=match, clean=force)

            # make sure all explicit patterns are matched
            if not force:
                self.checkcommitpatterns(wctx, match, status, fail)

            loginfo.update({"checkoutidentifier": self.dirstate.checkoutidentifier})

            cctx = context.workingcommitctx(
                self, status, text, user, date, extra, loginfo, mutinfo
            )

            # internal config: ui.allowemptycommit
            allowemptycommit = (
                merge or cctx.files() or self.ui.configbool("ui", "allowemptycommit")
            )
            if not allowemptycommit:
                return None

            if merge and cctx.deleted():
                raise errormod.Abort(_("cannot commit merge with missing files"))

            ms = mergemod.mergestate.read(self)
            mergeutil.checkunresolved(ms)

            if editor:
                cctx._text = editor(self, cctx)
            edited = text != cctx._text

            # Save commit message in case this transaction gets rolled back
            # (e.g. by a pretxncommit hook).  Leave the content alone on
            # the assumption that the user will use the same editor again.
            msgfn = self.savecommitmessage(cctx._text)

            p1, p2 = self.dirstate.parents()
            hookp1, hookp2 = hex(p1), (p2 != nullid and hex(p2) or "")
            try:
                self.hook("precommit", throw=True, parent1=hookp1, parent2=hookp2)
                tr = self.transaction("commit")
                ret = self.commitctx(cctx, True)
            except:  # re-raises
                if edited:
                    self.ui.write(_("note: commit message saved in %s\n") % msgfn)
                raise
            # update bookmarks, dirstate and mergestate
            bookmarks.update(self, [p1, p2], ret)
            cctx.markcommitted(ret)
            ms.reset()
            tr.close()

        finally:
            lockmod.release(tr, lock, wlock)

        self.hook("commit", node=hex(ret), parent1=hookp1, parent2=hookp2)

        return ret

    def commitctx(self, ctx, error=False):
        """Add a new revision to current repository.
        Revision information is passed via the context argument.
        """
        _validate_committable_ctx(self.ui, ctx)

        tr = None
        p1, p2 = ctx.p1(), ctx.p2()
        user = ctx.user()

        isgit = git.isgitformat(self)
        rewriteutil.commitcheck(self, ctx)
        lock = self.lock()
        try:
            tr = self.transaction("commit")
            trp = weakref.proxy(tr)

            mn, files = ctx.write_manifest_and_compute_files(trp)

            # update changelog
            self.ui.note(_("committing changelog\n"))
            self.changelog.delayupdate(tr)
            extra = ctx.extra().copy()
            if isgit:
                git.update_extra_with_git_committer(self.ui, ctx, extra)

            n = self.changelog.add(
                mn,
                files,
                ctx.description(),
                trp,
                p1.node(),
                p2.node(),
                user,
                ctx.date(),
                extra,
                signing.get_signing_backend(self.ui),
            )
            xp1, xp2 = p1.hex(), p2 and p2.hex() or ""
            self.hook("pretxncommit", throw=True, node=hex(n), parent1=xp1, parent2=xp2)
            # set the new commit is proper phase
            targetphase = phases.newcommitphase(self.ui)
            if targetphase:
                # retract boundary do not alter parent changeset.
                # if a parent have higher the resulting phase will
                # be compliant anyway
                #
                # if minimal phase was 0 we don't need to retract anything
                phases.registernew(self, tr, targetphase, [n])
            # Newly committed commits should be visible.
            if targetphase > phases.public:
                visibility.add(self, [n])
            mutinfo = ctx.mutinfo()
            if mutinfo is not None:
                entry = mutation.createentry(n, mutinfo)
                mutation.recordentries(self, [entry], skipexisting=False)
            tr.close()

            # Generate and log interesting data.
            loginfo = ctx.loginfo()
            path = os.path.commonprefix([os.path.dirname(p) for p in files])
            if len(path) > 0:
                loginfo.update({"common_directory_path": path})

            diffnumber = diffprops.parserevfromcommitmsg(ctx.description())
            if diffnumber is not None:
                loginfo.update({"phabricator_diff_number": diffnumber})

            _set_public_base_age(self, p1, loginfo)

            self.ui.log(
                "commit_info",
                node=hex(n),
                author=user,
                repo=self.ui.config("remotefilelog", "reponame"),
                **loginfo,
            )

            return n
        finally:
            if tr:
                tr.release()
            lock.release()

    def destroying(self):
        """Inform the repository that nodes are about to be destroyed.
        Intended for use by strip and rollback, so there's a common
        place for anything that has to be done before destroying history.

        This is mostly useful for saving state that is in memory and waiting
        to be flushed when the current lock is released. Because a call to
        destroyed is imminent, the repo will be invalidated causing those
        changes to stay in memory (waiting for the next unlock), or vanish
        completely.
        """
        # When using the same lock to commit and strip, the phasecache is left
        # dirty after committing. Then when we strip, the repo is invalidated,
        # causing those changes to disappear.
        if "_phasecache" in vars(self):
            self._phasecache.write()

    def destroyed(self):
        """Inform the repository that nodes have been destroyed.
        Intended for use by strip and rollback, so there's a common
        place for anything that has to be done after destroying history.
        """
        # When one tries to:
        # 1) destroy nodes thus calling this method (e.g. strip)
        # 2) use phasecache somewhere (e.g. commit)
        #
        # then 2) will fail because the phasecache contains nodes that were
        # removed. We can either remove phasecache from the filecache,
        # causing it to reload next time it is accessed, or simply filter
        # the removed nodes now and write the updated cache.
        self._phasecache.filterunknown(self)
        self._phasecache.write()

        # refresh all repository caches
        self.updatecaches()
        self.invalidate()

    def walk(self, match, node=None):
        """
        walk recursively through the directory tree or a given
        changeset, finding all files matched by the match
        function
        """
        self.ui.deprecwarn("use repo[node].walk instead of repo.walk", "4.3")
        return self[node].walk(match)

    def status(
        self,
        node1=".",
        node2=None,
        match=None,
        ignored=False,
        clean=False,
        unknown=False,
    ):
        """a convenience method that calls node1.status(node2)"""
        return self[node1].status(node2, match, ignored, clean, unknown)

    def addpostdsstatus(self, ps, afterdirstatewrite=True):
        """Add a callback to run within the wlock, at the point at which status
        fixups happen.

        On status completion, callback(wctx, status) will be called with the
        wlock held, unless the dirstate has changed from underneath or the wlock
        couldn't be grabbed.

        If afterdirstatewrite is True, it runs after writing dirstate,
        otherwise it runs before writing dirstate.

        Callbacks should not capture and use a cached copy of the dirstate --
        it might change in the meanwhile. Instead, they should access the
        dirstate via wctx.repo().dirstate.

        This list is emptied out after each status run -- extensions should
        make sure it adds to this list each time dirstate.status is called.
        Extensions should also make sure they don't call this for statuses
        that don't involve the dirstate.
        """

        # The list is located here for uniqueness reasons -- it is actually
        # managed by the workingctx, but that isn't unique per-repo.
        self._postdsstatus.append((ps, afterdirstatewrite))

    def postdsstatus(self, afterdirstatewrite=True):
        """Used by workingctx to get the list of post-dirstate-status hooks."""
        return [ps for (ps, after) in self._postdsstatus if after == afterdirstatewrite]

    def clearpostdsstatus(self):
        """Used by workingctx to clear post-dirstate-status hooks."""
        del self._postdsstatus[:]

    def _cachedheadnodes(self, includepublic=True, includedraft=True):
        """Get nodes of both public and draft heads.

        Cached. Invalidate on transaction commit.

        Respect visibility.all-heads (aka. --hidden).
        """
        cl = self.changelog
        if includedraft:
            draftkey = cl._visibleheads.changecount
        else:
            draftkey = None
        if includepublic:
            publickey = self._remotenames.changecount
        else:
            publickey = None

        # Invalidate heads if metalog root has changed.
        metalog = self.metalog()
        mlroot = metalog.root()

        key = (tuple(self.dirstate.parents()), mlroot, len(cl), draftkey, publickey)
        result = self._headcache.get(key)
        if result is not None:
            return result

        if includedraft:
            nodes = [
                n
                for n in self.dirstate.parents() + list(self._bookmarks.values())
                if n != nullid
            ]
        else:
            nodes = []
        # Do not treat the draft heads returned by remotenames as
        # unconditionally visible. This makes it possible to hide
        # them by "hg hide".
        publicnodes, draftnodes = _remotenodes(self)
        cl = self.changelog
        if includepublic:
            nodes += publicnodes
        if includedraft:
            nodes += draftnodes

            if cl._uiconfig.configbool("visibility", "all-heads"):  # aka. --hidden
                visibleheads = cl._visibleheads.allheads()
            else:
                visibleheads = cl._visibleheads.heads
            nodes += visibleheads
        # Do not report nullid. index2.headsancestors does not support it.
        hasnode = cl.hasnode
        nodes = [n for n in set(nodes) if n != nullid and hasnode(n)]
        headnodes = cl.dag.headsancestors(nodes)
        self._headcache[key] = headnodes
        return headnodes

    def _cachedheadrevs(self, includepublic=True, includedraft=True):
        nodes = self._cachedheadnodes(includepublic, includedraft)
        cl = self.changelog
        return list(cl.torevs(nodes))

    def headrevs(self, start=None, includepublic=True, includedraft=True, reverse=True):
        cl = self.changelog
        if self.ui.configbool("experimental", "narrow-heads"):
            headrevs = self._cachedheadrevs(
                includedraft=includedraft, includepublic=includepublic
            )
            # headrevs is already in DESC.
            reverse = not reverse
        else:
            headrevs = cl.rawheadrevs()
        if start is not None:
            startrev = cl.rev(start)
            headrevs = [r for r in headrevs if r > startrev]
        if reverse:
            return list(reversed(headrevs))
        else:
            return headrevs

    def heads(self, start=None, includepublic=True, includedraft=True):
        headrevs = self.headrevs(start, includepublic, includedraft)
        return list(map(self.changelog.node, headrevs))

    def branches(self, nodes):
        if not nodes:
            nodes = [self.changelog.tip()]
        b = []
        for n in nodes:
            t = n
            while True:
                p = self.changelog.parents(n)
                if p[1] != nullid or p[0] == nullid:
                    b.append((t, n, p[0], p[1]))
                    break
                n = p[0]
        return b

    def between(self, pairs):
        r = []

        for top, bottom in pairs:
            n, l, i = top, [], 0
            f = 1

            while n != bottom and n != nullid:
                p = self.changelog.parents(n)[0]
                if i == f:
                    l.append(n)
                    f = f * 2
                n = p
                i += 1

            r.append(l)

        return r

    def checkpush(self, pushop):
        """Extensions can override this function if additional checks have
        to be performed before pushing, or call it if they override push
        command.
        """

    @util.propertycache
    def prepushoutgoinghooks(self):
        """Return util.hooks consists of a pushop with repo, remote, outgoing
        methods, which are called before pushing changesets.
        """
        return util.hooks()

    def pushkey(self, namespace, key, old, new):
        try:
            tr = self.currenttransaction()
            hookargs = {}
            if tr is not None:
                hookargs.update(tr.hookargs)
            hookargs["namespace"] = namespace
            hookargs["key"] = key
            hookargs["old"] = old
            hookargs["new"] = new
            self.hook("prepushkey", throw=True, **hookargs)
        except errormod.HookAbort as exc:
            self.ui.write_err(_("pushkey-abort: %s\n") % exc)
            if exc.hint:
                self.ui.write_err(_("(%s)\n") % exc.hint, label="ui.hint")
            return False
        self.ui.debug('pushing key for "%s:%s"\n' % (namespace, key))
        ret = pushkey.push(self, namespace, key, old, new)

        def runhook():
            self.hook(
                "pushkey", namespace=namespace, key=key, old=old, new=new, ret=ret
            )

        self._afterlock(runhook)
        return ret

    def listkeys(self, namespace, patterns=None):
        self.hook("prelistkeys", throw=True, namespace=namespace)
        self.ui.debug('listing keys for "%s"\n' % namespace)
        values = pushkey.list(self, namespace)
        self.hook("listkeys", namespace=namespace, values=values)
        if patterns is not None and namespace == "bookmarks":
            bmarks = values
            values = util.sortdict()
            for pattern in patterns:
                if pattern.endswith("*"):
                    pattern = "re:^" + pattern[:-1] + ".*"
                kind, pat, matcher = util.stringmatcher(pattern)
                for bookmark, node in bmarks.items():
                    if matcher(bookmark):
                        values[bookmark] = node
        return values

    def debugwireargs(self, one, two, three=None, four=None, five=None):
        """used to test argument passing over the wire"""
        return "%s %s %s %s %s" % (one, two, three, four, five)

    def savecommitmessage(self, text):
        self.localvfs.writeutf8("last-message.txt", text)
        path = self.localvfs.join("last-message.txt")
        return self.pathto(path[len(self.root) + 1 :])

    def automigratestart(self):
        """perform potentially expensive in-place migrations

        Called at the start of pull if pull.automigrate is true
        """
        # Migration of visibility, and treestate were moved to __init__ so they
        # run for users not running 'pull'.

    def automigratefinish(self):
        """perform potentially expensive in-place migrations

        Called at the end of pull if pull.automigrate is true
        """

    @property
    def smallcommitmetadata(self):
        # Load smallcommitmetdata lazily because it is rarely used.
        if self._smallcommitmetadata is None:
            self._smallcommitmetadata = smallcommitmetadata.smallcommitmetadata(
                self.cachevfs, self.ui.configint("smallcommitmetadata", "entrylimit")
            )
        return self._smallcommitmetadata

    def metalog(self):
        return self.svfs.metalog

    def is_same_repo(self, other):
        """returns true if both repos have the same root directory"""
        if self is other:
            return True
        else:
            return other and self.root == other.root


# used to avoid circular references so destructors work
# This function is passed as the `after` function to `transaction`, and is
# called after writing metalog. See `transaction.py` and `def transaction`
# above.
def aftertrans(reporef, files):
    renamefiles = [tuple(t) for t in files]

    def a():
        for vfs, src, dest in renamefiles:
            # if src and dest refer to a same file, vfs.rename is a no-op,
            # leaving both src and dest on disk. delete dest to make sure
            # the rename couldn't be such a no-op.
            vfs.tryunlink(dest)
            try:
                vfs.rename(src, dest)
            except OSError:  # journal file does not yet exist
                pass

        # Sync metalog references back to git references.
        # This should happen after writing metalog.
        repo = reporef()
        if repo is not None and git.isgitstore(repo):
            repo.changelog.inner.updatereferences(repo.metalog())

    return a


def undoname(fn) -> str:
    base, name = os.path.split(fn)
    assert name.startswith("journal")
    return os.path.join(base, name.replace("journal", "undo", 1))


def instance(ui, path, create, initial_config) -> localrepository:
    return localrepository(ui, util.urllocalpath(path), create, initial_config)


def islocal(path) -> bool:
    return True


def newreporequirements(repo) -> Set[str]:
    """Determine the set of requirements for a new local repository.

    Extensions can wrap this function to specify custom requirements for
    new repositories.
    """
    ui = repo.ui
    requirements = {"revlogv1"}
    requirements.add("store")

    compengine = ui.config("experimental", "format.compression")
    if compengine not in util.compengines:
        raise errormod.Abort(
            _(
                "compression engine %s defined by "
                "experimental.format.compression not available"
            )
            % compengine,
            hint=_('run "hg debuginstall" to list available compression engines'),
        )

    requirements.add("treestate")

    # zlib is the historical default and doesn't need an explicit requirement.
    if compengine != "zlib":
        requirements.add("exp-compression-%s" % compengine)

    if scmutil.gdinitconfig(ui):
        requirements.add("generaldelta")
    if ui.configbool("experimental", "treemanifest"):
        requirements.add("treemanifest")

    return requirements


def _check_non_printable(ui, message: str) -> None:
    """Raise if non-printable ASCII characters are detected

    This should probably be updated to check special non-ASCII Unicode
    characters too. But for now it's easier to only check ASCII.
    """
    import string

    printable = set(string.printable)
    lines = []
    bad = False
    for line in message.splitlines(True):
        for i, c in enumerate(line):
            if ord(c) < 128 and c not in printable:
                # Problematic line
                lines.append("  > %s" % line)
                lines.append(" " * (i + 4) + "^\n")
                bad = True
                break
        else:
            # Regular line
            lines.append("    %s" % line)

    if bad:
        # Show hint about the problem.
        text = "".join(lines)
        raise errormod.Abort(
            _("non-printable characters in commit message:\n%s") % text,
            hint=_("edit commit message to fix this issue"),
        )


def _remotenodes(repo):
    """Return (remote public nodes, remote draft nodes)."""
    publicnodes = []
    draftnodes = []

    draftpattern = repo.ui.config("infinitepush", "branchpattern")
    if draftpattern:
        isdraft = util.stringmatcher(draftpattern)[-1]
    else:

        def isdraft(name):
            return False

    # publicheads is a direct way to define what remotenames are "public".
    # publicheads is a 'treematcher' constructed from a list of gitignore
    # rules, like `<remote>/*`, `!<remote>/<draft>`, etc.
    publicheads = repo.config.get.as_matcher("remotenames", "publicheads")

    remotebookmarks = repo._remotenames["bookmarks"]
    for fullname, nodes in remotebookmarks.items():
        # fullname: remote/foo/bar
        # remote: remote, name: foo/bar

        if publicheads:
            if publicheads.matches(fullname):
                publicnodes += nodes
            else:
                draftnodes += nodes
        else:
            _, name = bookmarks.splitremotename(fullname)
            if isdraft(name):
                draftnodes += nodes
            else:
                publicnodes += nodes

    return publicnodes, draftnodes


def _openchangelog(repo):
    if "emergencychangelog" in repo.storerequirements:
        repo.ui.warn(
            _(
                "warning: this repo was cloned for emergency commit+push use-case only! "
                "accessing older commits is broken!\n"
            )
        )

    # Refresh Rust changelog object every time we (re)load the
    # changelog. This is the simplest thing which matches all the
    # Python cache invalidation logic exactly.
    repo._rsrepo.invalidatechangelog()
    inner = repo._rsrepo.commits()
    return changelog2.changelog(repo, inner, repo.ui.uiconfig())


def _validate_committable_ctx(ui, ctx):
    if not ui.configbool("commit", "allow-non-printable") and not ui.plain():
        _check_non_printable(ui, ctx.description())

    descriptionlimit = ui.configbytes("commit", "description-size-limit")
    if descriptionlimit:
        descriptionlen = len(ctx.description())
        if descriptionlen > descriptionlimit:
            raise errormod.Abort(
                _("commit message length (%s) exceeds configured limit (%s)")
                % (descriptionlen, descriptionlimit)
            )

    extraslimit = ui.configbytes("commit", "extras-size-limit")
    if extraslimit:
        extraslen = sum(len(k) + len(v) for k, v in ctx.extra().items())
        if extraslen > extraslimit:
            raise errormod.Abort(
                _("commit extras total size (%s) exceeds configured limit (%s)")
                % (extraslen, extraslimit)
            )

    file_count_limit = ui.configint("commit", "file-count-limit")
    if file_count_limit and file_count_limit < len(ctx.files()):
        support = ui.config("ui", "supportcontact")
        if support:
            hint = (
                _(
                    "contact %s for help or use '--config commit.file-count-limit=N' cautiously to override"
                )
                % support
            )
        else:
            hint = _("use '--config commit.file-count-limit=N' cautiously to override")
        raise errormod.Abort(
            _("commit file count (%d) exceeds configured limit (%d)")
            % (len(ctx.files()), file_count_limit),
            hint=hint,
        )


def _set_public_base_age(repo, ctx, loginfo):
    if publicbase := scmutil.publicbase(repo, ctx):
        age = int(time.time() - publicbase.date()[0]) / SECONDS_IN_A_DAY
        loginfo.update(
            {
                "public_base": publicbase.hex(),
                "public_base_age": age,
            }
        )
