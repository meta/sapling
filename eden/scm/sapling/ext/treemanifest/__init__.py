# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""
treemanifest extension is to aid in the transition from flat manifests to
treemanifests. It has a client portion that's used to construct trees during
client pulls and commits, and a server portion which is used to generate
tree manifests side-by-side normal flat manifests.

Configs:

Setting `treemanifest.pullprefetchcount` to an integer N will cause the latest N
commits' manifests to be downloaded (if they aren't already).

::

    [treemanifest]
    pullprefetchcount = 0

`treemanifest.pullprefetchrevs` specifies a revset of commits who's trees should
be prefetched after a pull. Defaults to None.

::

    [treemanifest]
    pullprefetchrevs = master + stable

`treemanifest.fetchdepth` sets the default depth to fetch trees when fetching
trees from the server.

::

    [treemanifest]
    fetchdepth = 65536

`treemanifest.bfsprefetch` causes the client to perform a BFS over the
tree to be prefetched and manually request all missing nodes from the
server, rather than relying on the server to perform this computation.

::

    [treemanifest]
    bfsprefetch = True

`experimental.verify-manifest-root` looks up a manifest's root tree in the store
as soon as the manifest is requested, so a missing root fails fast instead of
when the tree is first read. Each lookup is a separate store request, so this is
off by default and manifests are resolved lazily.

::

    [experimental]
    verify-manifest-root = True

`treemanifest.http` causes treemanifest to fetch tress over HTTP using EdenAPI.

::

    [treemanifest]
    http = True
"""

import hashlib
import os
import struct

import bindings
from bindings import manifest as rustmanifest
from sapling import (
    bundle2,
    bundlerepo,
    changegroup,
    changelog2,
    commands,
    eagerepo,
    error,
    exchange,
    extensions,
    git,
    hg,
    localrepo,
    mdiff,
    perftrace,
    progress,
    registrar,
    repair,
    revlog,
    revlog2,
    revsetlang,
    scmutil,
    sshserver,
    templatekw,
    util,
    wireproto,
)
from sapling.commands import debug as debugcommands
from sapling.i18n import _
from sapling.node import bin, hex, nullid, short

from ..remotefilelog import (
    cmdtable as remotefilelogcmdtable,
    resolveprefetchopts,
    shallowbundle,
    shallowrepo,
    shallowutil,
    wirepack,
)
from ..remotefilelog.contentstore import unioncontentstore
from ..remotefilelog.historypack import memhistorypack
from ..remotefilelog.metadatastore import unionmetadatastore


cmdtable = {}
command = registrar.command(cmdtable)

# The default depth to fetch during tree fetches
TREE_DEPTH_MAX = 2**16


PACK_CATEGORY = "manifests"

TREEGROUP_PARTTYPE = "b2x:treegroup"
# Temporary part type while we migrate the arguments
TREEGROUP_PARTTYPE2 = "b2x:treegroup2"
RECEIVEDNODE_RECORD = "receivednodes"

# When looking for a recent manifest to consider our base during tree
# prefetches, this constant defines how far back we should search.
BASENODESEARCHMAX = 25000


def usehttpfetching(repo):
    """Returns True if HTTP (EdenApi) fetching should be used."""
    if repo.ui.config("ui", "ssh") == "false":
        # Cannot use SSH.
        return True
    return (
        repo.ui.configbool("treemanifest", "http") and repo.nullableedenapi is not None
    )


def hgupdate(orig, repo, node, quietempty=False, updatecheck=None):
    oldfallbackpath = getattr(repo, "fallbackpath", None)
    if hasattr(repo, "stickypushpath"):
        repo.fallbackpath = repo.stickypushpath

    try:
        return orig(repo, node, quietempty, updatecheck)
    finally:
        repo.fallbackpath = oldfallbackpath


def expush(orig, repo, remote, *args, **kwargs):
    if repo.ui.configbool(
        "treemanifest", "stickypushpath", True
    ) and "gettreepack" in shallowutil.peercapabilities(remote):
        # In case of pushrebase using paths.default-push, the pushback bundle
        # part does not contain trees. The client might prefetch trees from
        # paths.default, which could be lagging and cause tree prefetch to
        # fail. Set fallbackpath explicitly so the client pulls from the same
        # server as it pushes to.
        #
        # This assumes the server supporting pushrebase also supports
        # treemanifest, which is true for now.
        repo.stickypushpath = remote.url()

    return orig(repo, remote, *args, **kwargs)


def uisetup(ui):
    extensions.wrapfunction(exchange, "push", expush)
    extensions.wrapfunction(hg, "update", hgupdate)

    extensions.wrapfunction(
        changegroup.cg1unpacker, "_unpackmanifests", _unpackmanifestscg1
    )
    extensions.wrapfunction(
        changegroup.cg3unpacker, "_unpackmanifests", _unpackmanifestscg3
    )
    extensions.wrapfunction(
        exchange, "_pullbundle2extraprepare", pullbundle2extraprepare
    )
    extensions.wrapfunction(revlog.revlog, "checkhash", _checkhash)
    extensions.wrapfunction(revlog2.revlog2, "checkhash", _checkhash)

    extensions.wrapfilecache(localrepo.localrepository, "manifestlog", getmanifestlog)
    extensions.wrapfilecache(
        bundlerepo.bundlerepository, "manifestlog", getbundlemanifestlog
    )

    extensions.wrapcommand(commands.table, "pull", pull)

    wireproto.commands["gettreepack"] = (servergettreepack, "*")
    wireproto.wirepeer.gettreepack = clientgettreepack
    localrepo.localpeer.gettreepack = localgettreepack

    extensions.wrapfunction(
        debugcommands, "_findtreemanifest", _debugcmdfindtreemanifest
    )
    extensions.wrapfunction(debugcommands, "_debugbundle2part", _debugbundle2part)
    extensions.wrapfunction(repair, "_collectfiles", collectfiles)
    extensions.wrapfunction(repair, "_collectmanifest", _collectmanifest)
    extensions.wrapfunction(repair, "stripmanifest", stripmanifest)
    extensions.wrapfunction(bundle2, "_addpartsfromopts", _addpartsfromopts)
    extensions.wrapfunction(
        bundlerepo.bundlerepository, "_handlebundle2part", _handlebundle2part
    )
    extensions.wrapfunction(bundle2, "getrepocaps", getrepocaps)
    _registerbundle2parts()

    extensions.wrapfunction(templatekw, "showmanifest", showmanifest)
    templatekw.keywords["manifest"] = templatekw.showmanifest

    # Change manifest template output
    templatekw.defaulttempl["manifest"] = "{node}"

    def _wrapremotefilelog(loaded):
        if loaded:
            remotefilelogmod = extensions.find("remotefilelog")
            extensions.wrapcommand(
                remotefilelogmod.cmdtable, "prefetch", _prefetchwrapper
            )
        else:
            # There is no prefetch command to wrap around. In this case, we use
            # the command table entry for prefetch in the remotefilelog to
            # define the prefetch command, wrap it, and then override it
            # completely.  This ensures that the options to the prefetch command
            # are consistent.
            cmdtable["prefetch"] = remotefilelogcmdtable["prefetch"]
            extensions.wrapcommand(cmdtable, "prefetch", _overrideprefetch)

    extensions.afterloaded("remotefilelog", _wrapremotefilelog)

    # Work around the chicken-egg issue that a linkrev brings by delaying adding
    # data to the store until the changelog has been updated. This breaks the
    # assumption that manifests are written before the changelog, but unless
    # linknode are moved outside of the historypack entries, we have to solve
    # the dependency loop.
    # A similar hack is done in remotefilelog for the same reasons.
    pendingadd = []

    def addtreeentry(
        orig, self, dpack, hpack, nname, nnode, ntext, np1, np2, linknode, linkrev=None
    ):
        if linkrev is not None:
            pendingadd.append(
                (self, dpack, hpack, nname, nnode, ntext, np1, np2, linkrev)
            )
        else:
            orig(self, dpack, hpack, nname, nnode, ntext, np1, np2, linknode)

    extensions.wrapfunction(treemanifestlog, "_addtreeentry", addtreeentry)

    def changelogadd(orig, self, *args):
        oldlen = len(self)
        node = orig(self, *args)
        newlen = len(self)
        if oldlen != newlen:
            for oldargs in pendingadd:
                log, dpack, hpack, nname, nnode, ntext, np1, np2, linkrev = oldargs
                log._addtreeentry(dpack, hpack, nname, nnode, ntext, np1, np2, node)
        else:
            # Nothing was added to the changelog, let's make sure that we don't
            # have pending adds.
            if len(set(x[8] for x in pendingadd)) > 1:
                raise error.ProgrammingError(
                    "manifest entries were added, but no matching revisions were"
                )

        del pendingadd[:]
        return node

    extensions.wrapfunction(changelog2.changelog, "add", changelogadd)


def showmanifest(orig, **args):
    """Same implementation as the upstream showmanifest, but without the 'rev'
    field."""
    ctx, templ = args[r"ctx"], args[r"templ"]
    mnode = ctx.manifestnode()
    if mnode is None:
        # just avoid crash, we might want to use the 'ff...' hash in future
        return

    mhex = hex(mnode)
    args = args.copy()
    args.update({r"node": mhex})
    f = templ("manifest", **args)
    return templatekw._mappable(f, None, f, lambda x: {"node": mhex})


def getrepocaps(orig, repo, *args, **kwargs):
    caps = orig(repo, *args, **kwargs)
    caps["treemanifest"] = ("True",)
    caps["treeonly"] = ("True",)
    return caps


def _collectmanifest(orig, repo, striprev):
    return []


def stripmanifest(orig, repo, striprev, tr, files):
    repair.striptrees(repo, tr, striprev, files)


def _addtreecaps(caps):
    caps = set(caps)
    caps.add("gettreepack")
    caps.add("designatednodes")
    caps.add("treeonly")
    return list(caps)


def reposetup(ui, repo):
    # Update "{manifest}" again since it might be rewritten by templatekw.init.
    templatekw.defaulttempl["manifest"] = "{node}"

    if not isinstance(repo, localrepo.localrepository):
        return

    repo.name = repo.ui.config("remotefilelog", "reponame", "unknown")

    def _capabilities(orig, repo, proto):
        return _addtreecaps(orig(repo, proto))

    extensions.wrapfunction(wireproto, "_capabilities", _capabilities)

    wraprepo(repo)


def wraprepo(repo):
    class treerepository(repo.__class__):
        @perftrace.tracefunc("Prefetch Trees")
        def prefetchtrees(self, mfnodes, basemfnodes=None):
            if eagerepo.iseagerepo(self):
                return
            if self.storage_format() == "revlog":
                return

            mfnodes = list(mfnodes)
            perftrace.tracevalue("Keys", len(mfnodes))

            mfstore = self.manifestlog.datastore
            missingentries = mfstore.getmissing(("", n) for n in mfnodes)
            mfnodes = list(n for path, n in missingentries)
            perftrace.tracevalue("Missing", len(mfnodes))
            if not mfnodes:
                return

            self.manifestlog.datastore.prefetch(list(("", node) for node in mfnodes))

        def resettreefetches(self):
            fetches = self._treefetches
            self._treefetches = 0
            return fetches

        def _restrictcapabilities(self, caps):
            caps = super(treerepository, self)._restrictcapabilities(caps)
            return _addtreecaps(caps)

        def forcebfsprefetch(self, mfnodes):
            self._bfsprefetch(mfnodes)

        @perftrace.tracefunc("BFS Prefetch")
        def _bfsprefetch(self, mfnodes):
            with progress.spinner(self.ui, "prefetching trees using BFS"):
                store = self.manifestlog.datastore
                for node in mfnodes:
                    if node != nullid:
                        rustmanifest.prefetch(store, [node])

    repo.__class__ = treerepository
    repo._treefetches = 0


def setuptreestores(repo, mfl):
    if git.isgitstore(repo):
        mfl.datastore = git.openstore(repo)
    elif eagerepo.iseagerepo(repo) or repo.storage_format() == "revlog":
        store = repo.fileslog.filestore
        mfl._raw_store = store
        mfl.datastore = EagerDataStore(store)
        mfl.historystore = mfl.datastore.historystore
        if not isinstance(store, bindings.eagerepo.EagerRepoStore):
            raise error.ProgrammingError(
                "incompatible eagerrepo store: %r (expect EagerRepoStore)" % store
            )
    else:
        mask = os.umask(0o002)
        try:
            mfl.treescmstore = repo._rsrepo.treescmstore()
            mfl.datastore = mfl.treescmstore
            mfl.historystore = mfl.treescmstore.metadatastore()
        finally:
            os.umask(mask)


class treemanifestlog:
    def __init__(self, opener, repo):
        cachesize = 4
        self._treemanifestcache = util.lrucachedict(cachesize)
        # store object used to construct storemodel.TreeStore
        self._raw_store = None

        self._repo = repo
        self._opener = opener
        self.ui = repo.ui
        self._verifyroot = repo.ui.configbool("experimental", "verify-manifest-root")

        setuptreestores(repo, self)

    def clearcaches(self):
        pass

    def _maplinknode(self, linknode):
        """Turns a linknode into a linkrev. Only needed for revlog backed
        manifestlogs."""
        return self._repo.changelog.rev(linknode)

    def _maplinkrev(self, linkrev):
        """Turns a linkrev into a linknode. Only needed for revlog backed
        manifestlogs."""
        return self._repo.changelog.node(linkrev)

    def abstract_store(self):
        """returns storemodel.TreeStore backed by Rust trait object"""
        return bindings.storemodel.TreeStore.from_store(
            self._raw_store or self.datastore
        )

    def buildtree(self, node=None):
        """Create a tree manifest from a root tree node.

        Goes through the Rust tree resolver chain by default.
        Overridden by bundlemanifestlog to use the Python datastore.
        """
        return _apply_pymanifest_config(
            self._repo,
            self._repo._rsrepo.manifest_by_root_id(
                node if node is not None else nullid
            ),
        )

    @util.propertycache
    def _isgit(self):
        """Whether the Git serialization format is used.
        Note: This does not mean a libgit2 or ".git" store. Other stores like
        the EagerRepoStore, or the revisionstore can also speak the Git format.
        """
        return self.abstract_store().format() == "git"

    def add(
        self,
        ui,
        newtree,
        p1node,
        p2node,
        linknode,
        tr=None,
        linkrev=None,
    ):
        """Writes the given tree into the manifestlog."""
        return _finalize(self, newtree, p1node, p2node)

    def _getmutablelocalpacks(self):
        """Returns a tuple containing a data pack and a history pack."""
        return (self.datastore, self.historystore)

    def getmutablesharedpacks(self):
        return (
            self.datastore.getsharedmutable(),
            self.historystore.getsharedmutable(),
        )

    def _addtreeentry(
        self, dpack, hpack, nname, nnode, ntext, np1, np2, linknode, linkrev=None
    ):
        if linkrev is not None:
            raise error.ProgrammingError("linkrev cannot be added")
        # Not using deltas, since there aren't any other trees in
        # this pack it could delta against.
        dpack.add(nname, nnode, revlog.nullid, ntext)
        hpack.add(nname, nnode, np1, np2, linknode, "")

    def commitsharedpacks(self):
        """Persist the dirty trees written to the shared packs."""
        self.abstract_store().flush()

    def commitpending(self):
        self.commitsharedpacks()

    def abortpending(self):
        self.commitsharedpacks()

    def __nonzero__(self):
        return True

    __bool__ = __nonzero__

    def __getitem__(self, node):
        return self.get("", node)

    def get(self, dir, node, verify=True):
        if dir != "":
            raise RuntimeError(
                "native tree manifestlog doesn't support "
                "subdir reads: (%s, %s)" % (dir, hex(node))
            )
        # git store does not have the Python `.get(path, node)` method.
        # it can only be accessed via the Rust treemanifest.
        # eager store does not require remote lookup.
        if node == nullid or "remotefilelog" not in self._repo.requirements:
            return treemanifestctx(self, dir, node)

        if node in self._treemanifestcache:
            m = self._treemanifestcache[node]
            if m.dirty():
                # Manifest has been modified in memory - don't share it.
                del self._treemanifestcache[node]
            else:
                # Manifest is clean. Copy it so mutations aren't shared between
                # objects accidentally. Since the manifest is clean, this should
                # be a cheap, shallow copy.
                if m._tree:
                    m._tree = m._tree.copy()
                return m

        if self._verifyroot:
            try:
                self.datastore.get(dir, node)
            except KeyError:
                raise shallowutil.MissingNodesError([(dir, node)])
            except error.HttpError as ex:
                # Hack to handle eagerstore errors. This should be converted to a KeyError
                # somewhere in Rust.
                if "404" in str(ex):
                    raise shallowutil.MissingNodesError([(dir, node)])
                else:
                    raise ex

        m = treemanifestctx(self, dir, node)
        self._treemanifestcache[node] = m
        return m

    def edenapistore(self, repo):
        edenapi = repo.nullableedenapi
        if usehttpfetching(repo) and edenapi:
            return edenapi.treestore()
        return None


def _apply_pymanifest_config(repo, tree):
    regex = repo.ui.config("experimental", "slacl-ignore-permission-denied-regex")
    if regex:
        tree.set_ignore_permission_denied_regex(regex)
    return tree


def _buildtree_from_store(manifestlog, node=None):
    """Create a tree manifest from the Python datastore.

    Used by bundlemanifestlog where the datastore has bundle overlays
    that the Rust store doesn't see.
    """
    # this code seems to belong in manifestlog but I have no idea how
    # manifestlog objects work
    # XXX: This breaks abstraction. But we want the "native" store, instead of a
    # Python object (EagerDataStore) so store APIs like "format()" etc work
    # well. Alternatively, we need to define "def format()" to pass the
    # "format()" as-is across languages. Once we kill historystore then we might
    # remove the EagerDataStore Python wrapper.
    store = manifestlog.datastore
    if isinstance(store, EagerDataStore):
        store = store._store
    initfn = rustmanifest.treemanifest
    if node is not None and node != nullid:
        tree = initfn(store, node)
    else:
        tree = initfn(store)
    return _apply_pymanifest_config(manifestlog._repo, tree)


def _finalize(manifestlog, tree, p1node=None, p2node=None):
    parents = []
    p1tree = _getparenttree(manifestlog, p1node)
    if p1tree is not None:
        parents.append(p1tree)
    p2tree = _getparenttree(manifestlog, p2node)
    if p2tree is not None:
        parents.append(p2tree)
    return tree.finalize(*parents)


def _getparenttree(manifestlog, node=None):
    if node is None or node == nullid:
        return None
    tree = manifestlog[node].read()
    if hasattr(tree, "_treemanifest"):
        # Detect hybrid manifests and unwrap them
        tree = tree._treemanifest()
    return tree


class treemanifestctx:
    def __init__(self, manifestlog, dir, node):
        self._manifestlog = manifestlog
        self._dir = dir
        self._node = node
        self._tree = None

    def read(self):
        if self._tree is None:
            self._tree = self._manifestlog.buildtree(self._node)
        return self._tree

    def node(self):
        return self._node

    def new(self, dir=""):
        if dir != "":
            raise RuntimeError(
                "native tree manifestlog doesn't support subdir creation: '%s'" % dir
            )
        return self._manifestlog.buildtree()

    def copy(self):
        memmf = memtreemanifestctx(self._manifestlog, dir=self._dir)
        memmf._treemanifest = self.read().copy()
        return memmf

    @util.propertycache
    def parents(self):
        store = self._manifestlog.historystore
        p1, p2, linkrev, copyfrom = store.getnodeinfo(self._dir, self._node)
        if copyfrom:
            p1 = nullid
        return p1, p2

    def readnew(self, shallow=False):
        """Returns a manifest containing just the entries that are present
        in this manifest, but not in its p1 manifest. This is efficient to read
        if the revlog delta is already p1.

        If `shallow` is True, this will read the delta for this directory,
        without recursively reading subdirectory manifests. Instead, any
        subdirectory entry will be reported as it appears in the manifest, i.e.
        the subdirectory will be reported among files and distinguished only by
        its 't' flag.
        """
        p1, p2 = self.parents
        mf = self.read()
        parentmf = self._manifestlog.buildtree(p1)

        if shallow:
            # This appears to only be used for changegroup creation in
            # upstream changegroup.py. Since we use pack files for all native
            # tree exchanges, we shouldn't need to implement this.
            raise NotImplemented("native trees don't support shallow readdelta yet")
        else:
            md = self._manifestlog.buildtree()
            for f, ((n1, fl1), (n2, fl2)) in parentmf.diff(mf).items():
                if n2:
                    md[f] = n2
                    if fl2:
                        md.setflag(f, fl2)
            return md

    def find(self, key):
        return self.read().find(key)

    def dirty(self):
        return self._tree and self._tree.dirty()


class memtreemanifestctx:
    def __init__(self, manifestlog, dir=""):
        self._manifestlog = manifestlog
        self._dir = dir
        self._treemanifest = manifestlog.buildtree()

    def new(self, dir=""):
        return memtreemanifestctx(self._manifestlog, dir=dir)

    def copy(self):
        memmf = memtreemanifestctx(self._manifestlog, dir=self._dir)
        memmf._treemanifest = self._treemanifest.copy()
        return memmf

    def read(self):
        return self._treemanifest

    def write(self, tr, linkrev, p1, p2, added, removed):
        mfl = self._manifestlog
        newtree = self._treemanifest
        return _finalize(mfl, newtree, p1, p2)


def getmanifestlog(orig, repo):
    return treemanifestlog(repo.svfs, repo)


def getbundlemanifestlog(orig, self):
    mfl = orig(self)

    wrapmfl = mfl

    class bundlemanifestlog(wrapmfl.__class__):
        def buildtree(self, node=None):
            return _buildtree_from_store(self, node)

        def add(
            self,
            ui,
            newtree,
            p1node,
            p2node,
            linknode,
            tr=None,
            linkrev=None,
        ):
            return _finalize(self, newtree, p1node, p2node)

        def commitpending(self):
            pass

        def abortpending(self):
            pass

    wrapmfl.__class__ = bundlemanifestlog

    return mfl


@command("debuggetroottree", [], "NODE")
def debuggetroottree(ui, repo, rootnode):
    with ui.configoverride({("treemanifest", "fetchdepth"): "1"}, "forcesinglefetch"):
        repo.prefetchtrees([bin(rootnode)])


def _unpackmanifestscg3(orig, self, repo, *args, **kwargs):
    self.manifestheader()
    for chunk in self.deltaiter():
        raise error.ProgrammingError(
            "manifest deltas are not supported in a changegroup"
        )
    # Handle sub-tree manifests
    for chunkdata in iter(self.filelogheader, {}):
        raise error.ProgrammingError("sub-trees are not supported in a changegroup")


def _unpackmanifestscg1(orig, self, repo, revmap, trp, numchanges):
    self.manifestheader()
    for chunk in self.deltaiter():
        raise error.ProgrammingError(
            "manifest deltas are not supported in a changegroup"
        )


def _checkhash(orig, self, *args, **kwargs):
    # Don't validate root hashes during the transition to treemanifest
    if self.indexfile.endswith("00manifesttree.i"):
        return
    return orig(self, *args, **kwargs)


# Wrapper around the 'prefetch' command which also allows for prefetching the
# trees along with the files.
def _prefetchwrapper(orig, ui, repo, *pats, **opts):
    _prefetchonlytrees(repo, opts)
    _prefetchonlyfiles(orig, ui, repo, *pats, **opts)


# Wrapper around the 'prefetch' command which overrides the command completely
# and only allows for prefetching trees. This is only required when the
# 'prefetch' command is not available because the remotefilelog extension is not
# loaded and we want to be able to at least prefetch trees. The wrapping just
# ensures that we get a consistent interface to the 'prefetch' command.
def _overrideprefetch(orig, ui, repo, *pats, **opts):
    if opts.get("repack"):
        raise error.Abort(_("repack requires remotefilelog extension"))

    _prefetchonlytrees(repo, opts)


def _prefetchonlyfiles(orig, ui, repo, *pats, **opts):
    if shallowrepo.requirement in repo.requirements:
        orig(ui, repo, *pats, **opts)


def _prefetchonlytrees(repo, opts):
    opts = resolveprefetchopts(repo.ui, opts)
    revs = scmutil.revrange(repo, opts.get("rev"))

    # No trees need to be downloaded for the non-public commits.
    spec = revsetlang.formatspec("%ld & public()", revs)
    mfnodes = set(ctx.manifestnode() for ctx in repo.set(spec))

    basemfnode = set()
    base = opts.get("base")
    if base is not None:
        basemfnode.add(repo[base].manifestnode())

    repo.prefetchtrees(mfnodes, basemfnodes=basemfnode)


def _registerbundle2parts():
    @bundle2.parthandler(TREEGROUP_PARTTYPE2, ("version", "cache", "category"))
    def treeparthandler2(op, part):
        """Handles received tree packs. If `cache` is True, the received
        data goes in to the shared pack cache. Otherwise, the received data
        goes into the permanent repo local data.
        """
        repo = op.repo

        versionstr = part.params.get("version")
        try:
            version = int(versionstr)
        except ValueError:
            version = 0

        if version < 1 or version > 2:
            raise error.Abort(
                _("unknown treegroup bundle2 part version: %s") % versionstr
            )

        category = part.params.get("category", "")
        if category != PACK_CATEGORY:
            raise error.Abort(_("invalid treegroup pack category: %s") % category)

        mfl = repo.manifestlog

        if part.params.get("cache", "False") == "True":
            dpack, hpack = mfl.getmutablesharedpacks()
        else:
            dpack, hpack = mfl._getmutablelocalpacks()

        receivedhistory, receiveddata = wirepack.receivepack(
            repo.ui, part, dpack, hpack, version=version
        )

        op.records.add(RECEIVEDNODE_RECORD, receiveddata)

    @bundle2.parthandler(TREEGROUP_PARTTYPE, ("version", "treecache"))
    def treeparthandler(op, part):
        treecache = part.params.pop("treecache")
        part.params["cache"] = treecache
        part.params["category"] = PACK_CATEGORY
        return treeparthandler2(op, part)

    @exchange.b2partsgenerator(TREEGROUP_PARTTYPE)
    def gettreepackpart(pushop, bundler):
        # We no longer generate old tree groups
        pass

    @exchange.b2partsgenerator(TREEGROUP_PARTTYPE2)
    @perftrace.tracefunc("gettreepackpart2")
    def gettreepackpart2(pushop, bundler):
        """add parts containing trees being pushed"""
        if "treepack" in pushop.stepsdone:
            return
        pushop.stepsdone.add("treepack")

        # Only add trees if we have them
        sendtrees = shallowbundle.cansendtrees(
            pushop.repo, pushop.outgoing.missing, b2caps=bundler.capabilities
        )
        if sendtrees != shallowbundle.NoTrees and pushop.outgoing.missing:
            part = createtreepackpart(
                pushop.repo, pushop.outgoing, TREEGROUP_PARTTYPE2, sendtrees=sendtrees
            )
            bundler.addpart(part)

    @exchange.getbundle2partsgenerator(TREEGROUP_PARTTYPE2)
    def _getbundlechangegrouppart(
        bundler,
        repo,
        source,
        bundlecaps=None,
        b2caps=None,
        heads=None,
        common=None,
        **kwargs,
    ):
        """add parts containing trees being pulled"""
        if "True" not in b2caps.get("treemanifest", []) or not kwargs.get("cg", True):
            return

        outgoing = exchange._computeoutgoing(repo, heads, common)
        sendtrees = shallowbundle.cansendtrees(
            repo, outgoing.missing, bundlecaps=bundlecaps, b2caps=b2caps
        )
        if sendtrees != shallowbundle.NoTrees:
            try:
                part = createtreepackpart(
                    repo, outgoing, TREEGROUP_PARTTYPE2, sendtrees=sendtrees
                )
                bundler.addpart(part)
            except BaseException as ex:
                bundler.addpart(bundle2.createerrorpart(str(ex)))


def createtreepackpart(repo, outgoing, partname, sendtrees=shallowbundle.AllTrees):
    if sendtrees == shallowbundle.NoTrees:
        raise error.ProgrammingError("calling createtreepackpart with NoTrees")

    rootdir = ""
    mfnodes = []
    basemfnodes = []
    directories = []

    linknodemap = {}
    for node in outgoing.missing:
        ctx = repo[node]
        if sendtrees == shallowbundle.AllTrees or not ctx.ispublic():
            mfnode = ctx.manifestnode()
            if mfnode != nullid:
                mfnodes.append(mfnode)
                linknodemap.setdefault(mfnode, node)

    basectxs = repo.set("parents(%ln) - %ln", outgoing.missing, outgoing.missing)
    for basectx in basectxs:
        basemfnodes.append(basectx.manifestnode())
    linknodefixup = (set(outgoing.missing), linknodemap)

    # createtreepackpart is used to form bundles for normal pushes and pulls, so
    # we always pass depth=max here.
    packstream = generatepackstream(
        repo,
        rootdir,
        mfnodes,
        basemfnodes,
        directories,
        TREE_DEPTH_MAX,
        linknodefixup,
        version=1,
    )
    part = bundle2.bundlepart(partname, data=packstream)
    part.addparam("version", "1")
    part.addparam("cache", "False")
    part.addparam("category", PACK_CATEGORY)

    return part


def pull(orig, ui, repo, *pats, **opts):
    result = orig(ui, repo, *pats, **opts)

    try:
        _postpullprefetch(ui, repo)
    except Exception as ex:
        # Errors are not fatal.
        ui.warn(_("failed to prefetch trees after pull: %s\n") % ex)
        ui.log_exception(
            exception_type=type(ex).__name__,
            exception_msg=str(ex),
            fatal="false",
            source="post_pull_prefetch",
        )

    return result


def _postpullprefetch(ui, repo):
    if "default" not in repo.ui.paths:
        return
    if repo.storage_format() != "remotefilelog":
        return

    ctxs = []
    mfstore = repo.manifestlog.datastore

    # prefetch if it's configured
    prefetchcount = ui.configint("treemanifest", "pullprefetchcount", None)
    if prefetchcount:
        # Calculate what recent manifests are we missing
        ctxs.extend(repo.set("limit(sort(::tip,-rev),%s) & public()", prefetchcount))

    # Prefetch specific commits
    prefetchrevs = ui.config("treemanifest", "pullprefetchrevs", None)
    if prefetchrevs:
        ctxs.extend(repo.set(prefetchrevs))

    mfnodes = None
    if ctxs:
        mfnodes = list(c.manifestnode() for c in ctxs)

    if mfnodes:
        if not (
            ui.agent() and ui.configbool("agent", "hide-pull-prefetch-status", True)
        ):
            if len(mfnodes) == 1:
                ui.status(_("prefetching tree for %s\n") % short(ctxs[0].node()))
            else:
                ui.status(_("prefetching trees for %d commits\n") % len(mfnodes))
        # Calculate which parents we already have
        ctxnodes = list(ctx.node() for ctx in ctxs)
        parentctxs = repo.set("parents(%ln) - %ln", ctxnodes, ctxnodes)
        basemfnodes = set(ctx.manifestnode() for ctx in parentctxs)
        missingbases = list(mfstore.getmissing(("", n) for n in basemfnodes))
        basemfnodes.difference_update(n for k, n in missingbases)

        repo.prefetchtrees(mfnodes, basemfnodes=basemfnodes)


def clientgettreepack(remote, rootdir, mfnodes, basemfnodes, directories, depth):
    opts = {}
    opts["rootdir"] = rootdir
    opts["mfnodes"] = wireproto.encodelist(mfnodes)
    opts["basemfnodes"] = wireproto.encodelist(basemfnodes)
    # Serialize directories with a trailing , so we can differentiate the empty
    # directory from the end of the list!
    opts["directories"] = "".join(
        [wireproto.escapestringarg(d) + "," for d in directories]
    )
    opts["depth"] = str(depth)

    ui = remote.ui
    ui.metrics.inc("ssh_gettreepack_basemfnodes", len(basemfnodes))
    ui.metrics.inc("ssh_gettreepack_mfnodes", len(mfnodes))
    ui.metrics.inc("ssh_gettreepack_calls", 1)

    f = remote._callcompressable("gettreepack", **opts)
    return bundle2.getunbundler(remote.ui, f)


def localgettreepack(remote, rootdir, mfnodes, basemfnodes, directories, depth):
    bundler = _gettreepack(
        remote._repo, rootdir, mfnodes, basemfnodes, directories, depth
    )
    chunks = bundler.getchunks()
    cb = util.chunkbuffer(chunks)
    return bundle2.getunbundler(remote._repo.ui, cb)


def servergettreepack(repo, proto, args):
    """A server api for requesting a pack of tree information."""
    if shallowrepo.requirement in repo.requirements:
        raise error.Abort(_("cannot fetch remote files from shallow repo"))
    if not isinstance(proto, sshserver.sshserver):
        raise error.Abort(_("cannot fetch remote files over non-ssh protocol"))

    rootdir = args["rootdir"]
    depth = int(args.get("depth", str(2**16)))

    mfnodes = wireproto.decodelist(args["mfnodes"])
    basemfnodes = wireproto.decodelist(args["basemfnodes"])
    directories = list(
        wireproto.unescapearg(d) for d in args["directories"].split(",") if d != ""
    )

    bundler = _gettreepack(repo, rootdir, mfnodes, basemfnodes, directories, depth)
    return wireproto.streamres(gen=bundler.getchunks(), v1compressible=True)


def _gettreepack(repo, rootdir, mfnodes, basemfnodes, directories, depth):
    try:
        bundler = bundle2.bundle20(repo.ui)
        packstream = generatepackstream(
            repo, rootdir, mfnodes, basemfnodes, directories, depth, version=1
        )
        part = bundler.newpart(TREEGROUP_PARTTYPE2, data=packstream)
        part.addparam("version", "1")
        part.addparam("cache", "True")
        part.addparam("category", PACK_CATEGORY)

    except error.Abort as exc:
        # cleanly forward Abort error to the client
        bundler = bundle2.bundle20(repo.ui)
        bundler.addpart(bundle2.createerrorpart(str(exc), hint=exc.hint))
    except BaseException as exc:
        bundler = bundle2.bundle20(repo.ui)
        bundler.addpart(bundle2.createerrorpart(str(exc)))

    return bundler


def generatepackstream(
    repo,
    rootdir,
    mfnodes,
    basemfnodes,
    directories,
    depth,
    linknodefixup=None,
    version=1,
):
    """
    All size/len/counts are network order unsigned ints.

    Request args:

    `rootdir` - The directory of the tree to send (including its children)
    `mfnodes` - The manifest nodes of the specified root directory to send.
    `basemfnodes` - The manifest nodes of the specified root directory that are
    already on the client.
    `directories` - The fullpath (not relative path) of directories underneath
    the rootdir that should be sent.
    `depth` - The depth from the root that should be sent.
    `linknodefixup` - If not None, this is a pair of (validset, mapping) where
    validset is a set of nodes that are valid linknodes, and mapping is a map
    from root manifest node to the linknode that should be used for all new
    trees in that manifest which don't have a valid linknode.

    Response format:

    [<fileresponse>,...]<10 null bytes>
    fileresponse = <filename len: 2 byte><filename><history><deltas>
    history = <count: 4 byte>[<history entry>,...]
    historyentry = <node: 20 byte><p1: 20 byte><p2: 20 byte>
                   <linknode: 20 byte><copyfrom len: 2 byte><copyfrom>
    deltas = <count: 4 byte>[<delta entry>,...]
    deltaentry = <node: 20 byte><deltabase: 20 byte>
                 <delta len: 8 byte><delta>
    """
    datastore = repo.manifestlog.datastore

    # Throw an exception early if the requested nodes aren't present. It's
    # important that we throw it now and not later during the pack stream
    # generation because at that point we've already added the part to the
    # stream and it's difficult to switch to an error then.
    missing = []
    if len(directories) > 0:
        iterator = zip(mfnodes, directories)
        assert depth == 1
    else:
        iterator = list((n, rootdir) for n in mfnodes)

    # Throw an exception early if the requested nodes aren't present. It's
    # important that we throw it now and not later during the pack stream
    # generation because at that point we've already added the part to the
    # stream and it's difficult to switch to an error then.
    missing = []
    if len(directories) > 0:
        iterator = zip(mfnodes, directories)
        assert depth == 1
    else:
        iterator = list((n, rootdir) for n in mfnodes)
    for n, dir in iterator:
        try:
            datastore.get(dir, n)
        except shallowutil.MissingNodesError:
            missing.append((dir, n))
    if missing:
        raise shallowutil.MissingNodesError(missing, "tree nodes missing on server")

    return _generatepackstream(
        repo, rootdir, mfnodes, basemfnodes, directories, depth, linknodefixup, version
    )


def _existonserver(repo, mfnode):
    """Check if the root manifest exists on server-side.

    Return True if the server has the mfnode, False otherwise.
    """
    stream, _stats = repo.edenapi.trees(
        [("", mfnode)],
        {"parents": False, "manifest_blob": False, "child_metadata": False},
    )
    try:
        list(stream)
        return True
    except Exception:
        # The error type story isn't great for now.
        return False


class EagerHistoryStore:
    def __init__(self, store):
        self._store = store
        self._added = {}

    # This API is needed so the client can know the p1, p2 of mfnode.
    # Without p1, p2 the client won't be able to send those trees via
    # bundle2.
    def getnodeinfo(self, dir, mfnode):
        added = self._added.get((dir, mfnode))
        if added is not None:
            return added
        p1p2 = self._store.get_sha1_blob(mfnode)[:40]
        p1 = p1p2[:20]
        p2 = p1p2[20:]
        if p1 == nullid:
            p1, p2 = p2, p1
        # Fake linknode and copyfrom.
        return p1, p2, nullid, None

    # used by remotefilelog.wirepack.receivepack
    def add(self, filename, node, p1, p2, linknode, copyfrom):
        self._added[(filename, node)] = (p1, p2, linknode, copyfrom)

    def getsharedmutable(self):
        return self


class EagerDataStore:
    def __init__(self, store):
        self._store = store
        # need the historystore to provide p1, p2 information
        self.historystore = EagerHistoryStore(store)

    def format(self):
        format = bindings.storemodel.TreeStore.from_store(self._store).format()
        self.format = lambda: format
        return format

    def get(self, dir, node):
        rawtext = self._store.get_content(node)
        if rawtext is None:
            raise KeyError("EagerDataStore does not have %s:%s" % (dir, hex(node)))
        return rawtext

    def getmissing(self, lst):
        missing = []
        for item in lst:
            if self._store.get_sha1_blob(item[1]) is None:
                missing.append(item)
        return missing

    # used by unioncontentstore
    def getdeltachain(self, name, node):
        content = self.get(name, node)
        return [(name, node, "", nullid, content)]

    # used by remotefilelog.wirepack.receivepack
    def add(self, name, node, deltabase, delta, metadata):
        if deltabase == nullid:
            # unlike revlog2.addgroup, delta == nullid needs special
            # handling here.
            rawtext = delta
        else:
            # apply delta
            basetext = self._store.get_content(deltabase)
            rawtext = mdiff.patch(basetext, delta)
        # get p1, p2 from the history store
        p1, p2 = self.historystore.getnodeinfo(name, node)[:2]
        blob = revlog.textwithheader(rawtext, p1, p2)
        if hashlib.sha1(blob).digest() == node:
            bases = []
            if deltabase != nullid:
                bases.append(deltabase)
            new_node = self._store.add_sha1_blob(blob, bases)
            assert new_node == node
        else:
            # root manifest might have a faked hash for flat
            # manifest compatibility
            assert name == ""
            self._store.add_arbitrary_blob(node, blob)

    def prefetch(self, items):
        # EagerRepoStore is not lazy.
        pass

    def getsharedmutable(self):
        return self

    def __getattr__(self, name):
        return getattr(self._store, name)


def _generatepackstream(
    repo, rootdir, mfnodes, basemfnodes, directories, depth, linknodefixup, version
):
    """A simple helper function for generatepackstream. This helper is a
    generator, while the main function is not, so we can execute the
    validation logic in the main function immediately without waiting for the
    first iteration.
    """
    historystore = repo.manifestlog.historystore
    datastore = repo.manifestlog.datastore

    # getdesignatednodes
    if len(directories) > 0:
        for mfnode, dir in zip(mfnodes, directories):
            text = datastore.get(dir, mfnode)
            p1node, p2node = historystore.getnodeinfo(dir, mfnode)[:2]

            data = [(mfnode, nullid, text, 0)]

            histdata = historystore.getnodeinfo(dir, mfnode)
            p1node, p2node, linknode, copyfrom = histdata
            history = [(mfnode, p1node, p2node, linknode, copyfrom)]

            for chunk in wirepack.sendpackpart(dir, history, data, version=version):
                yield chunk
        yield wirepack.closepart()
        return

    mfnodeset = set(mfnodes)
    basemfnodeset = set(basemfnodes)

    # Helper function for filtering out non-existent base manifests that were
    # passed. This can happen if the remote client passes a base manifest that
    # the server doesn't know about yet.
    def treeexists(mfnode):
        return bool(not datastore.getmissing([(rootdir, mfnode)]))

    prevmfnode = None
    for node in mfnodes:
        try:
            p1node, p2node = historystore.getnodeinfo(rootdir, node)[:2]
        except KeyError:
            if _existonserver(repo, node):
                # No need to bundle the tree. It is already present on the
                # server-side. Similar to what we do for public commits
                # (createtreepackpart checking ctx.phase() != phases.public)
                continue
            else:
                raise
        # If p1 is being sent or is already on the client, chances are
        # that's the best thing for us to delta against.
        if p1node != nullid and (p1node in mfnodeset or p1node in basemfnodeset):
            basetrees = [(rootdir, p1node)]
        elif basemfnodes and any(treeexists(mfnode) for mfnode in basemfnodes):
            basetrees = [
                (rootdir, basenode) for basenode in basemfnodes if treeexists(basenode)
            ]
        elif prevmfnode:
            # If there are no base nodes and the parent isn't one of the
            # requested mfnodes, then pick another mfnode as a base.
            basetrees = [(rootdir, prevmfnode)]
        else:
            basetrees = []
        prevmfnode = node

        if p2node != nullid and (p2node in mfnodeset or p2node in basemfnodeset):
            basetrees.append((rootdir, p2node))

        # Only use the first two base trees, since the current tree
        # implementation cannot handle more yet.
        basenodes = [mybasenode for (_path, mybasenode) in basetrees]
        subtrees = rustmanifest.subdirdiff(datastore, rootdir, node, basenodes, depth)
        rootlinknode = None
        if linknodefixup is not None:
            validlinknodes, linknodemap = linknodefixup
            rootlinknode = linknodemap.get(node)
        for subname, subnode, subtext, _x, _x in subtrees:
            # Append data
            data = [(subnode, nullid, subtext, 0)]

            # Append history
            # Only append first history for now, since the entire manifest
            # history is very long.
            histdata = historystore.getnodeinfo(subname, subnode)
            p1node, p2node, linknode, copyfrom = histdata
            if rootlinknode is not None and linknode not in validlinknodes:
                linknode = rootlinknode
            history = [(subnode, p1node, p2node, linknode, copyfrom)]

            for chunk in wirepack.sendpackpart(subname, history, data, version=version):
                yield chunk

    yield wirepack.closepart()


def _debugcmdfindtreemanifest(orig, ctx):
    manifest = ctx.manifest()
    # Check if the manifest we have is a treemanifest.
    if isinstance(manifest, rustmanifest.treemanifest):
        return manifest
    try:
        # Look up the treemanifest in the treemanifestlog.  There might not be
        # one, so ignore any failures.
        return ctx.repo().manifestlog.treemanifestlog.get("", ctx.manifestnode()).read()
    except Exception:
        pass
    return orig(ctx)


def _debugbundle2part(orig, ui, part, all, **opts):
    if part.type == TREEGROUP_PARTTYPE2:
        indent_string = "    "
        tempstore = wirepack.wirepackstore(part.read())

        ui.write(indent_string)
        ui.write("%s\n" % tempstore.debugstats())
        for key in sorted(tempstore):
            ui.write(indent_string)
            ui.write("%s %s\n" % (hex(key[1]), key[0]))

    orig(ui, part, all, **opts)


def collectfiles(orig, repo, striprev):
    """find out the filelogs affected by the strip"""
    files = set()

    for x in range(striprev, len(repo)):
        ctx = repo[x]
        parents = ctx.parents()
        if len(parents) > 1:
            # Merge commits may not list all the files that are different
            # between the two sides. This is fine for stripping filelogs (since
            # any filelog that was changed will be listed), but is not fine for
            # directories, which may have changes despite filelogs not changing
            # (imagine a directory where two different files were added on
            # different sides of the merge. No filelogs change in the merge, but
            # the directory does).
            for parent in parents:
                diff = ctx.manifest().diff(parent.manifest())
                files.update(diff)
        else:
            files.update(repo[x].files())

    return sorted(files)


def _addpartsfromopts(orig, ui, repo, bundler, source, outgoing, *args, **kwargs):
    orig(ui, repo, bundler, source, outgoing, *args, **kwargs)

    # Only add trees to bundles for tree enabled clients. Servers use revlogs
    # and therefore will use changegroup tree storage.
    sendtrees = shallowbundle.cansendtrees(
        repo, outgoing.missing, b2caps=bundler.capabilities
    )
    if sendtrees != shallowbundle.NoTrees:
        part = createtreepackpart(
            repo, outgoing, TREEGROUP_PARTTYPE2, sendtrees=sendtrees
        )
        bundler.addpart(part)


def _handlebundle2part(orig, self, bundle, part):
    if part.type == TREEGROUP_PARTTYPE2:
        tempstore = wirepack.wirepackstore(part.read())

        # Point the bundle repo at the temp stores
        mfl = self.manifestlog
        mfl.datastore = unioncontentstore(tempstore, mfl.datastore)
        mfl.historystore = unionmetadatastore(tempstore, mfl.historystore)
    else:
        orig(self, bundle, part)


NODEINFOFORMAT = "!20s20s20sI"
NODEINFOLEN = struct.calcsize(NODEINFOFORMAT)


class nodeinfoserializer:
    """Serializer for node info"""

    @staticmethod
    def serialize(value):
        p1, p2, linknode, copyfrom = value
        copyfrom = (copyfrom if copyfrom else "").encode()
        return struct.pack(NODEINFOFORMAT, p1, p2, linknode, len(copyfrom)) + copyfrom

    @staticmethod
    def deserialize(raw):
        p1, p2, linknode, copyfromlen = struct.unpack_from(NODEINFOFORMAT, raw, 0)
        if len(raw) != NODEINFOLEN + copyfromlen:
            raise IOError(
                "invalid nodeinfo serialization: %s %s %s %s %s"
                % (hex(p1), hex(p2), hex(linknode), str(copyfromlen), raw[NODEINFOLEN:])
            )
        return (
            p1,
            p2,
            linknode,
            raw[NODEINFOLEN : NODEINFOLEN + copyfromlen].decode(),
        )


class cachestoreserializer:
    """Simple serializer that attaches key and sha1 to the content"""

    def __init__(self, key):
        self.key = key.encode()

    def serialize(self, value):
        sha = hashlib.sha1(value).digest()
        return sha + struct.pack("!I", len(self.key)) + self.key + value

    def deserialize(self, raw):
        key = self.key
        if not raw:
            raise IOError("missing content for the key in the cache: %s" % key)
        sha = raw[:20]
        keylen = struct.unpack_from("!I", raw, 20)[0]
        storedkey = raw[24 : 24 + keylen]
        if storedkey != key:
            raise IOError(
                "cache value has key '%s' but '%s' expected" % (storedkey, key)
            )
        value = raw[24 + keylen :]
        realsha = hashlib.sha1(value).digest()
        if sha != realsha:
            raise IOError("invalid content for the key in the cache: %s" % key)
        return value


def pullbundle2extraprepare(orig, pullop, kwargs):
    bundlecaps = kwargs.get("bundlecaps", set())
    bundlecaps.add("treeonly")
