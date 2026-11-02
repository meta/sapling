# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# merge.py - directory-level update/merge handling for Mercurial
#
# Copyright 2006, 2007 Olivia Mackall <olivia@selenic.com>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.


import hashlib
import os
import posixpath
import shutil
import sys
from collections import defaultdict

from bindings import (
    checkout as nativecheckout,
    error as rusterror,
    manifest as rustmanifest,
    status as nativestatus,
    worker as rustworker,
    workingcopy as rustworkingcopy,
)
from sapling import tracing

from . import (
    copies,
    edenfs,
    error,
    extensions,
    filemerge,
    git,
    grepo,
    i18n,
    match as matchmod,
    pathutil,
    perftrace,
    progress,
    scmutil,
    util,
    worker,
)
from .i18n import _
from .node import addednodeid, bin, hex, nullhex, nullid, wdirhex
from .utils import sparseutil, subtreeutil

# merge action types
ACTION_MERGE = "m"
ACTION_KEEP = "k"
ACTION_GET = "g"
ACTION_REMOVE_GET = "rg"  # symlink->file change
ACTION_EXEC = "e"
ACTION_CHANGED_DELETED = "cd"
ACTION_REMOVE = "r"
ACTION_FORGET = "f"
ACTION_CREATED = "c"
ACTION_CREATED_MERGE = "cm"
ACTION_DELETED_CHANGED = "dc"
ACTION_VISIBLE_RESTRICTED = "vr"
ACTION_PATH_CONFLICT_RESOLVE = "pr"
ACTION_PATH_CONFLICT = "p"
ACTION_LOCAL_DIR_RENAME_GET = "dg"
ACTION_DIR_RENAME_MOVE_LOCAL = "dm"
ACTION_ADD = "a"
ACTION_ADD_MODIFIED = "am"


def _restricted_sparse_matcher(*ctxs):
    if not ctxs or not ctxs[0].repo().ui.configbool(
        "subtree", "filter-restricted-paths", True
    ):
        return None

    if not any(ctx.manifest().hasgrafts() for ctx in ctxs):
        return None

    matcher = None
    for ctx in ctxs:
        repo = ctx.repo()
        if not sparseutil.shouldsparsematch(repo):
            continue
        ctxmatcher = repo.sparsematch(ctx.rev())
        grafts = ctx.manifest().diffgrafts()
        if grafts:
            ctxmatcher = matchmod.graftmatcher(ctxmatcher, grafts)
        matcher = matchmod.intersectmatchers(matcher, ctxmatcher)
    return matcher


class mergestate:
    """track 3-way merge state of individual files

    The merge state is stored on disk when needed. See the
    `repostate::MergeState` Rust type for details on the format.

    Merge driver run states (experimental):
    u: driver-resolved files unmarked -- needs to be run next time we're about
       to resolve or commit
    m: driver-resolved files marked -- only needs to be run before commit
    s: success/skipped -- does not need to be run any more

    Merge record states (stored in self._state, indexed by filename):
    u: unresolved conflict
    r: resolved conflict
    pu: unresolved path conflict (file conflicts with directory)
    pr: resolved path conflict
    d: driver-resolved conflict

    The resolve command transitions between 'u' and 'r' for conflicts and
    'pu' and 'pr' for path conflicts.

    """

    statepath = "merge/state2"

    @classmethod
    def clean(
        cls,
        repo,
        node=None,
        other=None,
        labels=None,
        ancestors=None,
        inmemory=False,
        from_repo=None,
    ) -> "mergestate":
        """Initialize a brand new merge state, removing any existing state on disk."""
        shutil.rmtree(repo.localvfs.join("merge"), True)
        rust_ms = rustworkingcopy.mergestate(node, other, labels)
        obj = cls.__new__(cls)
        obj._init(
            repo=repo,
            rust_ms=rust_ms,
            ancestors=ancestors,
            inmemory=inmemory,
            from_repo=from_repo,
        )
        return obj

    @classmethod
    def read(cls, repo) -> "mergestate":
        """Initialize the merge state, reading it from disk."""
        rust_ms = repo._rsrepo.workingcopy().mergestate()

        # Note: ancestors isn't written into the state file since the current
        # state file predates it.
        #
        # It's only needed during `applyupdates` in the initial call to merge,
        # so it's set transiently there. It isn't read during `hg resolve`.
        ancestors = None

        from_repo = None
        if subtree_merges := rust_ms.subtree_merges():
            from_repo_url = subtree_merges[0]["from_url"]
            if from_repo_url:
                with repo.ui.configoverride({("ui", "quiet"): True}):
                    from_repo = subtreeutil.get_or_clone_git_repo(
                        repo.ui, from_repo_url
                    )

        obj = cls.__new__(cls)
        obj._init(
            repo=repo,
            rust_ms=rust_ms,
            ancestors=ancestors,
            inmemory=False,
            from_repo=from_repo,
        )
        return obj

    def __init__(self, repo, rust_ms, ancestors, inmemory, from_repo=None):
        raise RuntimeError("Use mergestate.read() or mergestate.clean()")

    def _init(self, repo, rust_ms, ancestors, inmemory, from_repo=None):
        self._repo = repo
        self._from_repo = from_repo or repo
        self._ancestors = ancestors
        self._inmemory = inmemory
        self._rust_ms = rust_ms
        if md := rust_ms.mergedriver():
            self._readmergedriver = md[0]
            self._mdstate = md[1]
        else:
            self._readmergedriver = None
            self._mdstate = "s"

        self._dirty = False
        self._results = {}
        self._inmemory_to_be_merged = {}

        # Optimize various aspects during "in-memory" merges.
        self._optimize_inmemory = repo.ui.configbool(
            "experimental", "optimize-in-memory-merge-state", True
        )

    def reset(
        self,
        node=None,
        other=None,
        labels=None,
        ancestors=None,
        inmemory=False,
        from_repo=None,
    ):
        shutil.rmtree(self._repo.localvfs.join("merge"), True)
        rust_ms = rustworkingcopy.mergestate(node, other, labels)
        self._init(
            repo=self._repo,
            rust_ms=rust_ms,
            ancestors=ancestors,
            inmemory=inmemory,
            from_repo=from_repo,
        )
        for var in ("localctx", "otherctx", "ancestorctxs"):
            if var in vars(self):
                delattr(self, var)

    @util.propertycache
    def mergedriver(self):
        # protect against the following:
        # - A configures a malicious merge driver in their hgrc, then
        #   pauses the merge
        # - A edits their hgrc to remove references to the merge driver
        # - A gives a copy of their entire repo, including .hg, to B
        # - B inspects .hgrc and finds it to be clean
        # - B then continues the merge and the malicious merge driver
        #  gets invoked
        configmergedriver = self._repo.ui.config("experimental", "mergedriver")
        if (
            self._readmergedriver is not None
            and self._readmergedriver != configmergedriver
        ):
            raise error.ConfigError(
                _("merge driver changed since merge started")
                + "\n"
                + _("revert merge driver change or abort merge")
            )

        return configmergedriver

    @util.propertycache
    def localctx(self):
        if self._local is None:
            msg = "localctx accessed but self._local isn't set"
            raise error.ProgrammingError(msg)
        return self._repo[self._local]

    @util.propertycache
    def otherctx(self):
        if self._other is None:
            msg = "otherctx accessed but self._other isn't set"
            raise error.ProgrammingError(msg)
        return self._repo[self._other]

    @util.propertycache
    def ancestorctxs(self):
        if self._ancestors is None:
            raise error.ProgrammingError(
                "ancestorctxs accessed but self._ancestors aren't set"
            )
        return [self._repo[node] for node in self._ancestors]

    @util.propertycache
    def _local(self):
        return self._rust_ms.local()

    @util.propertycache
    def _other(self):
        return self._rust_ms.other()

    @util.propertycache
    def _labels(self):
        # Maintain historical behavior of no labels being `None`, not `[]`.
        return self._rust_ms.labels() or None

    @util.propertycache
    def subtree_merges(self):
        """Subtree merge info.

        Return a list of (from_node, from_path, to_path).
        """
        return self._rust_ms.subtree_merges()

    def from_repo(self):
        return self._from_repo

    def add_subtree_merge(self, from_node, from_path, to_path, from_repo_url=None):
        """Add a subtree merge record"""
        self._rust_ms.add_subtree_merge(from_node, from_path, to_path, from_repo_url)
        self._dirty = True

    def active(self):
        """Whether mergestate is active.

        Returns True if there appears to be mergestate. This is a rough proxy
        for "is a merge in progress."
        """
        # Check local variables before looking at filesystem for performance
        # reasons.
        return (
            bool(self._local)
            or not self._rust_ms.isempty()
            or self._repo.localvfs.exists(self.statepath)
        )

    def commit(self):
        """Write current state on disk (if necessary)"""

        if self._inmemory and self._optimize_inmemory:
            # Don't bother writing out to disk if we are doing an "in memory" merge.
            # There should be no need for cross-process merge state persistence.
            return

        if self._dirty:
            if md := self.mergedriver:
                self._rust_ms.setmergedriver((md, self._mdstate))
            else:
                self._rust_ms.setmergedriver(None)

            self._repo._rsrepo.workingcopy().writemergestate(self._rust_ms)

    def add(self, fcl, fco, fca, fd):
        """add a new (potentially?) conflicting file to the merge state
        fcl: file context for local,
        fco: file context for remote,
        fca: file context for ancestors,
        fd:  file path of the resulting merge.

        note: also write the local version to the `.hg/merge` directory.
        """
        if fcl.isabsent():
            hash = nullhex
        else:
            hash = hex(hashlib.sha1(fcl.path().encode()).digest())
            wctx = fcl.changectx()
            if wctx.isinmemory() and self._optimize_inmemory:
                # Detach data to maintain laziness, but disassociate the data from wctx
                # so it isn't influenced by wctx modifications (such as
                # `wctx.remove(path)`).
                self._inmemory_to_be_merged[hash] = fcl.detacheddata()
            else:
                self._repo.localvfs.write("merge/" + hash, fcl.data())
        self._rust_ms.insert(
            fd,
            [
                "u",
                hash,
                fcl.path(),
                fca.path(),
                hex(fca.filenode()),
                fco.path(),
                hex(fco.filenode()),
                fcl.flags(),
            ],
        )
        self._rust_ms.setextra(fd, "ancestorlinknode", hex(fca.node()))
        self._dirty = True

    def addpath(self, path, frename, forigin):
        """add a new conflicting path to the merge state
        path:    the path that conflicts
        frename: the filename the conflicting file was renamed to
        forigin: origin of the file ('l' or 'r' for local/remote)
        """
        self._rust_ms.insert(path, ["pu", frename, forigin])
        self._dirty = True

    def __contains__(self, dfile):
        return self._rust_ms.contains(dfile)

    def __getitem__(self, dfile):
        return self._rust_ms.get(dfile)[0]

    def __iter__(self):
        return iter(sorted(self._rust_ms.files()))

    def files(self):
        return self._rust_ms.files()

    def mark(self, dfile, state):
        self._rust_ms.setstate(dfile, state)
        self._dirty = True

    def mdstate(self):
        return self._mdstate

    def unresolved(self):
        """Obtain the paths of unresolved files."""
        return self._rust_ms.files(("u", "pu"))

    def driverresolved(self):
        """Obtain the paths of driver-resolved files."""
        return self._rust_ms.files(("d",))

    def extras(self, filename):
        return self._rust_ms.extras(filename)

    def _resolve(self, preresolve, dfile, wctx):
        """rerun merge process for file path `dfile`"""
        if self[dfile] in "rd":
            return True, 0
        stateentry = self._rust_ms.get(dfile)
        _state, hexdnode, lfile, afile, hexanode, ofile, hexonode, flags = stateentry
        dnode = bin(hexdnode)
        onode = bin(hexonode)
        anode = bin(hexanode)
        octx = self._from_repo[self._other]
        extras = self.extras(dfile)
        anccommitnode = extras.get("ancestorlinknode")
        if anccommitnode:
            actx = self._from_repo[anccommitnode]
        else:
            actx = None

        self._repo.ui.log(
            "merge_resolve", "resolving %s, preresolve = %s", dfile, preresolve
        )
        fcd = self._filectxorabsent(dnode, wctx, dfile)
        fco = self._filectxorabsent(onode, octx, ofile)
        fca = self._from_repo.filectx(afile, fileid=anode, changeid=actx)
        # "premerge" x flags
        flo = fco.flags()
        fla = fca.flags()
        if "x" in flags + flo + fla and "l" not in flags + flo + fla:
            if fca.node() == nullid and flags != flo:
                if preresolve:
                    self._repo.ui.warn(
                        _(
                            "warning: cannot merge flags for %s "
                            "without common ancestor - keeping local flags\n"
                        )
                        % afile
                    )
            elif flags == fla:
                flags = flo
        if preresolve:
            # restore local
            if dnode != nullid:
                if wctx.isinmemory() and self._optimize_inmemory:
                    wctx.write(dfile, self._inmemory_to_be_merged[hexdnode], flags)
                else:
                    f = self._repo.localvfs("merge/" + hexdnode)
                    wctx[dfile].write(f.read(), flags)
                    f.close()
            else:
                if not fcd.isrestricted():
                    wctx[dfile].remove(ignoremissing=True)
            while True:
                try:
                    complete, r, deleted = filemerge.premerge(
                        self._repo,
                        wctx,
                        self._local,
                        lfile,
                        fcd,
                        fco,
                        fca,
                        labels=self._labels,
                    )
                    break
                except error.RetryFileMerge as ex:
                    fcd = ex.fcd
        else:
            while True:
                try:
                    complete, r, deleted = filemerge.filemerge(
                        self._repo,
                        wctx,
                        self._local,
                        lfile,
                        fcd,
                        fco,
                        fca,
                        labels=self._labels,
                    )
                    break
                except error.RetryFileMerge as ex:
                    fcd = ex.fcd
        if r is None:
            # no real conflict
            self._rust_ms.remove(dfile)
            self._dirty = True
        elif not r:
            self.mark(dfile, "r")

        if complete:
            action = None
            if deleted:
                if fcd.isabsent():
                    # dc: local picked. Need to drop if present, which may
                    # happen on re-resolves.
                    action = ACTION_FORGET
                else:
                    # cd: remote picked (or otherwise deleted)
                    action = ACTION_REMOVE
            else:
                if fcd.isabsent():  # dc: remote picked
                    action = ACTION_GET
                elif fco.isabsent():  # cd: local picked
                    if dfile in self.localctx:
                        action = ACTION_ADD_MODIFIED
                    else:
                        action = ACTION_ADD
                # else: regular merges (no action necessary)
            self._results[dfile] = r, action

        return complete, r

    def _filectxorabsent(self, node, ctx, f):
        assert len(node) == len(nullid)
        if node != nullid:
            return ctx[f]

        if ctx.manifest().lookupfile(f) is None:
            return filemerge.absentfilectx(ctx, f, restricted=True)

        return filemerge.absentfilectx(ctx, f)

    def preresolve(self, dfile, wctx):
        """run premerge process for dfile

        Returns whether the merge is complete, and the exit code."""
        return self._resolve(True, dfile, wctx)

    def resolve(self, dfile, wctx):
        """run merge process (assuming premerge was run) for dfile

        Returns the exit code of the merge."""
        return self._resolve(False, dfile, wctx)[1]

    def counts(self):
        """return counts for updated, merged and removed files in this
        session"""
        updated, merged, removed = 0, 0, 0
        for r, action in self._results.values():
            if r is None:
                updated += 1
            elif r == 0:
                if action == ACTION_REMOVE:
                    removed += 1
                else:
                    merged += 1
        return updated, merged, removed

    def unresolvedcount(self):
        """get unresolved count for this merge (persistent)"""
        return len(list(self.unresolved()))

    def actions(self):
        """return lists of actions to perform on the dirstate"""
        actions = {
            ACTION_REMOVE: [],
            ACTION_FORGET: [],
            ACTION_ADD: [],
            ACTION_ADD_MODIFIED: [],
            ACTION_GET: [],
        }
        for f, (r, action) in self._results.items():
            if action is not None:
                actions[action].append((f, None, "merge result"))
        return actions

    def recordactions(self):
        """record remove/add/get actions in the dirstate"""
        ds = self._repo.dirstate
        branchmerge = ds.is_merge()
        recordupdates(
            self._repo, self.actions(), branchmerge, from_repo=self._from_repo
        )

    def queueremove(self, f):
        """queues a file to be removed from the dirstate

        Meant for use by custom merge drivers."""
        self._results[f] = 0, ACTION_REMOVE

    def queueadd(self, f):
        """queues a file to be added to the dirstate

        Meant for use by custom merge drivers."""
        self._results[f] = 0, ACTION_ADD

    def queueget(self, f):
        """queues a file to be marked modified in the dirstate

        Meant for use by custom merge drivers."""
        self._results[f] = 0, ACTION_GET


def _getcheckunknownconfig(repo, section, name):
    config = repo.ui.config(section, name)
    valid = ["abort", "ignore", "warn"]
    if config not in valid:
        validstr = ", ".join(["'" + v + "'" for v in valid])
        raise error.ConfigError(
            _("%s.%s not valid ('%s' is none of %s)")
            % (section, name, config, validstr)
        )
    return config


def _checkunknownfile(repo, wctx, mctx, f, f2=None):
    if wctx.isinmemory():
        # Nothing to do in IMM because nothing in the "working copy" can be an
        # unknown file.
        #
        # Note that we should bail out here, not in ``_checkunknownfiles()``,
        # because that function does other useful work.
        return False

    if f2 is None:
        f2 = f
    mfctx = mctx[f2]
    return (
        repo.wvfs.audit.check(f)
        and repo.wvfs.isfileorlink(f)
        and repo.dirstate.normalize(f) not in repo.dirstate
        and mfctx.filelog().cmp(mfctx.filenode(), wctx[f].data())
    )


class _unknowndirschecker:
    """
    Look for any unknown files or directories that may have a path conflict
    with a file.  If any path prefix of the file exists as a file or link,
    then it conflicts.  If the file itself is a directory that contains any
    file that is not tracked, then it conflicts.

    Returns the shortest path at which a conflict occurs, or None if there is
    no conflict.
    """

    def __init__(self):
        # A set of paths known to be good.  This prevents repeated checking of
        # dirs.  It will be updated with any new dirs that are checked and found
        # to be safe.
        self._unknowndircache = set()

        # A set of paths that are known to be absent.  This prevents repeated
        # checking of subdirectories that are known not to exist. It will be
        # updated with any new dirs that are checked and found to be absent.
        self._missingdircache = set()

    def __call__(self, repo, wctx, f):
        if wctx.isinmemory():
            # Nothing to do in IMM for the same reason as ``_checkunknownfile``.
            return False

        # Check for path prefixes that exist as unknown files.
        for p in reversed(list(util.finddirs(f))):
            if p in self._missingdircache:
                return
            if p in self._unknowndircache:
                continue
            if repo.wvfs.audit.check(p):
                if (
                    repo.wvfs.isfileorlink(p)
                    and repo.dirstate.normalize(p) not in repo.dirstate
                ):
                    return p
                if not repo.wvfs.lexists(p):
                    self._missingdircache.add(p)
                    return
                self._unknowndircache.add(p)

        # Check if the file conflicts with a directory containing unknown files.
        if repo.wvfs.audit.check(f) and repo.wvfs.isdir(f):
            # Does the directory contain any files that are not in the dirstate?
            root = os.path.normpath(repo.wvfs.join(None))
            # This conflict check does not read file contents, write files, or
            # expose the listing. The actual merge writes go through protected
            # paths, so a plain path-based walk is acceptable here even though
            # it does not use VFS path audit or no-follow traversal.
            prefixlen = len(pathutil.normasprefix(root))
            for p, _dirs, files in os.walk(repo.wvfs.join(f)):
                p = util.pconvert(p[prefixlen:])
                for fn in files:
                    relf = repo.dirstate.normalize(posixpath.join(p, fn))
                    if relf not in repo.dirstate:
                        return f
        return None


@perftrace.tracefunc("Check Unknown Files")
def _checkunknownfiles(repo, wctx, mctx, force, actions):
    """
    Considers any actions that care about the presence of conflicting unknown
    files. For some actions, the result is to abort; for others, it is to
    choose a different action.
    """
    fileconflicts = set()
    pathconflicts = set()
    warnconflicts = set()
    abortconflicts = set()
    unknownconfig = _getcheckunknownconfig(repo, "merge", "checkunknown")
    ignoredconfig = _getcheckunknownconfig(repo, "merge", "checkignored")
    pathconfig = repo.ui.configbool("experimental", "merge.checkpathconflicts")

    def progiter(itr):
        return progress.each(repo.ui, itr, "check untracked")

    if not force:

        def collectconflicts(conflicts, config):
            if config == "abort":
                abortconflicts.update(conflicts)
            elif config == "warn":
                warnconflicts.update(conflicts)

        checkunknowndirs = _unknowndirschecker()
        count = 0
        for f, (m, args, msg) in progiter(actions.items()):
            if m in (ACTION_CREATED, ACTION_DELETED_CHANGED):
                count += 1
                f2 = args[0] if m == ACTION_CREATED else args[1]
                if _checkunknownfile(repo, wctx, mctx, f, f2):
                    fileconflicts.add(f)
                elif pathconfig and f not in wctx:
                    path = checkunknowndirs(repo, wctx, f)
                    if path is not None:
                        pathconflicts.add(path)
            elif m == ACTION_LOCAL_DIR_RENAME_GET:
                count += 1
                if _checkunknownfile(repo, wctx, mctx, f, args[0]):
                    fileconflicts.add(f)

        allconflicts = fileconflicts | pathconflicts
        ignoredconflicts = set([c for c in allconflicts if repo.dirstate._ignore(c)])
        unknownconflicts = allconflicts - ignoredconflicts
        collectconflicts(ignoredconflicts, ignoredconfig)
        collectconflicts(unknownconflicts, unknownconfig)
    else:
        for f, (m, args, msg) in progiter(actions.items()):
            if m == ACTION_CREATED_MERGE:
                f2, fl2, anc = args
                different = _checkunknownfile(repo, wctx, mctx, f, f2)
                if repo.dirstate._ignore(f):
                    config = ignoredconfig
                else:
                    config = unknownconfig

                # The behavior when force is True is described by this table:
                #  config  different    |    action    backup
                #    *         n        |      get        n
                #    *         y        |     merge       -
                #   abort      y        |     merge       -   (1)
                #   warn       y        |  warn + get     y
                #  ignore      y        |      get        y
                #
                # (1) this is probably the wrong behavior here -- we should
                #     probably abort, but some actions like rebases currently
                #     don't like an abort happening in the middle of
                #     merge.update/goto.
                if not different:
                    actions[f] = (ACTION_GET, (f2, fl2, False), "remote created")
                elif config == "abort":
                    actions[f] = (
                        ACTION_MERGE,
                        (f, f, None, False, anc),
                        "remote differs from untracked local",
                    )
                else:
                    if config == "warn":
                        warnconflicts.add(f)
                    actions[f] = (ACTION_GET, (f2, fl2, True), "remote created")

    for f in sorted(abortconflicts):
        warn = repo.ui.warn
        if f in pathconflicts:
            if repo.wvfs.isfileorlink(f):
                warn(_("%s: untracked file conflicts with directory\n") % f)
            else:
                warn(_("%s: untracked directory conflicts with file\n") % f)
        else:
            warn(_("%s: untracked file differs\n") % f)
    if abortconflicts:
        raise error.Abort(
            _(
                "untracked files in working directory "
                "differ from files in requested revision"
            )
        )

    for f in sorted(warnconflicts):
        if repo.wvfs.isfileorlink(f):
            repo.ui.warn(_("%s: replacing untracked file\n") % f)
        else:
            repo.ui.warn(_("%s: replacing untracked files in directory\n") % f)

    for f, (m, args, msg) in actions.items():
        if m == ACTION_CREATED:
            backup = (
                f in fileconflicts
                or f in pathconflicts
                or any(p in pathconflicts for p in util.finddirs(f))
            )
            (f2, flags) = args
            actions[f] = (ACTION_GET, (f2, flags, backup), msg)


def _forgetremoved(wctx, mctx, branchmerge):
    """
    Forget removed files

    If we're jumping between revisions (as opposed to merging), and if
    neither the working directory nor the target rev has the file,
    then we need to remove it from the dirstate, to prevent the
    dirstate from listing the file when it is no longer in the
    manifest.

    If we're merging, and the other revision has removed a file
    that is not present in the working directory, we need to mark it
    as removed.
    """

    actions = {}
    m = ACTION_FORGET
    if branchmerge:
        m = ACTION_REMOVE
    for f in wctx.deleted():
        if f not in mctx:
            actions[f] = m, None, "forget deleted"

    if not branchmerge:
        for f in wctx.removed():
            if f not in mctx:
                actions[f] = ACTION_FORGET, None, "forget removed"

    return actions


def driverpreprocess(repo, ms, wctx, labels=None):
    """run the preprocess step of the merge driver, if any

    This is currently not implemented -- it's an extension point."""
    return True


def driverconclude(repo, ms, wctx, labels=None):
    """run the conclude step of the merge driver, if any

    This is currently not implemented -- it's an extension point."""
    return True


def recountunresolvedafterconclude(ui):
    return ui.configbool(
        "experimental",
        "mergedriver.recount-unresolved-after-conclude",
        default=True,
    )


def _filesindirs(repo, manifest, dirs):
    """
    Generator that yields pairs of all the files in the manifest that are found
    inside the directories listed in dirs, and which directory they are found
    in.
    """
    for dir in dirs:
        dirmatch = matchmod.match(repo.root, "", include=[dir + "/**"])
        for f in manifest.matches(dirmatch):
            yield f, dir


@perftrace.tracefunc("Check Path Conflicts")
def checkpathconflicts(repo, wctx, mctx, actions):
    """
    Check if any actions introduce path conflicts in the repository, updating
    actions to record or handle the path conflict accordingly.
    """
    mf = wctx.manifest()
    mctxmf = mctx.manifest()

    # The set of local files that conflict with a remote directory.
    localconflicts = set()

    # The set of directories that conflict with a remote file, and so may cause
    # conflicts if they still contain any files after the merge.
    remoteconflicts = set()

    # The set of directories that appear as both a file and a directory in the
    # remote manifest.  These indicate an invalid remote manifest, which
    # can't be updated to cleanly.
    invalidconflicts = set()

    # The set of directories that contain files that are being created.
    createdfiledirs = set()

    # The set of files deleted by all the actions.
    deletedfiles = set()

    for f, (m, args, msg) in actions.items():
        if m in (
            ACTION_CREATED,
            ACTION_DELETED_CHANGED,
            ACTION_MERGE,
            ACTION_CREATED_MERGE,
            ACTION_VISIBLE_RESTRICTED,
        ):
            # This action may create a new local file.
            createdfiledirs.update(util.finddirs(f))
            if mf.lookupdir(f):
                # The file aliases a local directory.  This might be ok if all
                # the files in the local directory are being deleted.  This
                # will be checked once we know what all the deleted files are.
                remoteconflicts.add(f)
        # Track the names of all deleted files.
        if m == ACTION_REMOVE:
            deletedfiles.add(f)
        elif m == ACTION_MERGE:
            f1, f2, fa, move, anc = args
            if move:
                deletedfiles.add(f1)
        elif m == ACTION_DIR_RENAME_MOVE_LOCAL:
            f2, flags = args
            deletedfiles.add(f2)

    # Check all directories that contain created files for path conflicts.
    for p in createdfiledirs:
        if mf.lookupfile(p):
            if mctxmf.lookupfile(p):
                # A file is in a directory which aliases both a local
                # and a remote file.  This is an internal inconsistency
                # within the remote manifest.
                invalidconflicts.add(p)
            else:
                # A file is in a directory which aliases a local file.
                # We will need to rename the local file.
                localconflicts.add(p)
        if p in actions and actions[p][0] in (
            ACTION_CREATED,
            ACTION_DELETED_CHANGED,
            ACTION_MERGE,
            ACTION_CREATED_MERGE,
            ACTION_VISIBLE_RESTRICTED,
        ):
            # The file is in a directory which aliases a remote file.
            # This is an internal inconsistency within the remote
            # manifest.
            invalidconflicts.add(p)

    # Rename all local conflicting files that have not been deleted.
    for p in localconflicts:
        if p not in deletedfiles:
            ctxname = str(wctx).rstrip("+")
            pnew = util.safename(p, ctxname, wctx, set(actions.keys()))
            actions[pnew] = (ACTION_PATH_CONFLICT_RESOLVE, (p,), "local path conflict")
            actions[p] = (ACTION_PATH_CONFLICT, (pnew, "l"), "path conflict")

    if remoteconflicts:
        # Check if all files in the conflicting directories have been removed.
        ctxname = str(mctx).rstrip("+")
        for f, p in _filesindirs(repo, mf, remoteconflicts):
            if f not in deletedfiles:
                m, args, msg = actions[p]
                pnew = util.safename(p, ctxname, wctx, set(actions.keys()))
                if m in (
                    ACTION_DELETED_CHANGED,
                    ACTION_MERGE,
                    ACTION_VISIBLE_RESTRICTED,
                ):
                    # Action was merge, just update target.
                    actions[pnew] = (m, args, msg)
                elif m in (ACTION_CREATED, ACTION_CREATED_MERGE):
                    # Action was create, change to renamed get action.
                    fl = args[1]
                    actions[pnew] = (
                        ACTION_LOCAL_DIR_RENAME_GET,
                        (p, fl),
                        "remote path conflict",
                    )
                else:
                    raise error.ProgrammingError(f"unexpected action type '{m}'")
                actions[p] = (ACTION_PATH_CONFLICT, (pnew, "r"), "path conflict")
                remoteconflicts.remove(p)
                break

    if invalidconflicts:
        for p in invalidconflicts:
            repo.ui.warn(_("%s: is both a file and a directory\n") % p)
        raise error.Abort(_("destination manifest contains path conflicts"))


def manifestmerge(
    to_repo,
    wctx,
    p2,
    pa,
    branchmerge,
    force,
    acceptremote,
    followcopies,
    forcefulldiff=False,
    from_repo=None,
):
    """
    Merge wctx and p2 with ancestor pa and generate merge action list

    branchmerge and force are as passed in to update
    acceptremote = accept the incoming changes without prompting
    """

    def handle_file_on_other_side(f, diff, reverse_copies):
        """check if file `f` should be handled on other side.

        For example, if file `f` is moved to `f1`, then there will be
        two entries the in the manifest diff:
            - (f, ((n, ""), (None, "")))
            - (f1, ((None, ""), (n1, "")))
        For this case, we only need to generate one action for `f1`.
        """
        if f not in reverse_copies:
            return False
        for f1 in reverse_copies[f]:
            try:
                ((n1, _fl1), (n2, _fl2)) = diff[f1]
                # Ensures that `f1` is not processed by the 'if n1 and n2:' branch
                # in the main `for` loop over `diff.items()` below. Otherwise,
                # the conflict for file `f` would be overlooked.
                if not (n1 and n2):
                    return True
            except KeyError:
                continue
        return False

    def files_equal(node1, node2, ctx1, ctx2, f1, f2) -> bool:
        if ctx1.repo() == ctx2.repo():
            return node1 == node2
        elif node1 and node2:
            # PERF: compare file content hash instead of file content and
            # consider moving this to fctx.cmp()
            return ctx1[f1].data() == ctx2[f2].data()
        elif node1 is None and node2 is None:
            return True
        else:
            return False

    from_repo = from_repo or to_repo
    is_crossrepo = not to_repo.is_same_repo(from_repo)

    ui = to_repo.ui
    copy = {}

    # manifests fetched in order are going to be faster, so prime the caches
    [x.manifest() for x in sorted(wctx.parents() + [p2, pa], key=scmutil.intrev)]

    # XXX: handle copy tracing for crossrepo merges
    if followcopies and not is_crossrepo:
        copy = copies.mergecopies(to_repo, wctx, p2, pa)

    boolbm = str(bool(branchmerge))
    boolf = str(bool(force))
    # XXX: handle sparsematch for cross-repo merges
    shouldsparsematch = sparseutil.shouldsparsematch(to_repo) and not is_crossrepo
    sparsematch = to_repo.sparsematch if shouldsparsematch else None
    ui.note(_("resolving manifests\n"))
    ui.debug(" branchmerge: %s, force: %s\n" % (boolbm, boolf))
    ui.debug(" ancestor: %s, local: %s, remote: %s\n" % (pa, wctx, p2))

    m1, m2, ma = wctx.manifest(), p2.manifest(), pa.manifest()

    matcher = None
    sparsematcher = None
    restrictedmatcher = (
        _restricted_sparse_matcher(wctx.p1(), p2, pa)
        if sparsematch is not None
        else None
    )

    # Don't use m2-vs-ma optimization if:
    # - ma is the same as m1 or m2, which we're just going to diff again later
    # - The caller specifically asks for a full diff, which is useful during bid
    #   merge.
    if pa not in ([wctx, p2] + wctx.parents()) and not forcefulldiff:
        # Identify which files are relevant to the merge, so we can limit the
        # total m1-vs-m2 diff to just those files. This has significant
        # performance benefits in large repositories.
        relevantfiles = set(_diff_manifests(ma, m2))

        # For copied and moved files, we need to add the source file too.
        for copykey, copyvalue in copy.items():
            if copyvalue in relevantfiles:
                relevantfiles.add(copykey)
        matcher = scmutil.matchfiles(to_repo, relevantfiles)

    # For sparse repos, attempt to use the sparsematcher to narrow down
    # calculation.  Consider a typical rebase:
    #
    #     o new master (m1, wctx, rebase destination)
    #     .
    #     . o (m2, rebase source)
    #     |/
    #     o (ma)
    #
    # Split diff(m1, m2) into 2 parts:
    # - diff(m2, ma) is small, and cannot use sparseamtcher for correctness.
    #   (see test-sparse-rebase.t)
    # - diff(m1, ma) is potentially huge, and can use sparsematcher.
    elif sparsematch is not None and not forcefulldiff:
        if branchmerge:
            relevantfiles = set(_diff_manifests(ma, m2))
            for copykey, copyvalue in copy.items():
                if copyvalue in relevantfiles:
                    relevantfiles.add(copykey)
            filesmatcher = scmutil.matchfiles(to_repo, relevantfiles)
        else:
            filesmatcher = None

        revs = {to_repo.dirstate.p1(), to_repo.dirstate.p2(), pa.node(), p2.node()}
        revs -= {nullid, None}
        sparsematcher = sparsematch(*list(revs))

        # use sparsematcher to make diff(m1, ma) less expensive.
        if filesmatcher is not None:
            sparsematcher = matchmod.unionmatcher([sparsematcher, filesmatcher])
        matcher = matchmod.intersectmatchers(matcher, sparsematcher)

    matcher = matchmod.intersectmatchers(matcher, restrictedmatcher)

    with perftrace.trace("Manifest Diff"):
        if hasattr(to_repo, "resettreefetches"):
            to_repo.resettreefetches()
        diff = _diff_manifests(m1, m2, matcher=matcher)
        perftrace.tracevalue("Differences", len(diff))
        if hasattr(to_repo, "resettreefetches"):
            perftrace.tracevalue("Tree Fetches", to_repo.resettreefetches())

    if matcher is None:
        matcher = matchmod.always("", "")

    reverse_copies = defaultdict(list)
    for k, v in copy.items():
        reverse_copies[v].append(k)

    # skip changed/subtree-copied conflict check for cross repo cases
    if is_crossrepo:
        subtree_branches = []
    else:
        subtree_branches = subtreeutil.get_subtree_branches(to_repo, p2)
    subtree_branch_dests = [b.to_path for b in subtree_branches]

    actions = {}
    # (n1, fl1) = "local" (m1)
    # (n2, fl2) = "remote" (m2)
    # `n` means node, `fl` means flags (also called file type, see `types::tree::FileType` Rust type)
    for f1, ((n1, fl1), (n2, fl2)) in diff.items():
        # If the diff operation had re-mapped directories for one side, "m.ungraftedpath()"
        # will recover the original path for m.
        f2 = m2.ungraftedpath(f1) or f1
        fa = ma.ungraftedpath(f1) or f1

        ancestor_file = ma.lookupfile(fa)
        if ancestor_file:
            na, fla = ancestor_file
        else:
            na = None
            fla = ""

        subtree_copy_dest = subtreeutil.find_enclosing_dest(f1, subtree_branch_dests)
        allow_merge_subtree_copy = ui.configbool(
            "subtree", "allow-merge-subtree-copy-commit"
        )
        if (
            not allow_merge_subtree_copy
            and subtree_copy_dest
            and (n1 != na or fl1 != fla)
        ):
            hint = _("use '@prog@ subtree copy' to re-create the directory branch")
            if extra_hint := ui.config("subtree", "copy-conflict-hint"):
                hint = f"{hint}. {extra_hint}"
            raise error.Abort(
                _(
                    "subtree copy dest path '%s' of '%s' has been updated on the other side"
                )
                % (subtree_copy_dest, p2),
                hint=hint,
            )
        elif n1 and n2:  # file exists on both local and remote side
            if not ancestor_file:
                fa = copy.get(f1, None)
                if fa is not None:
                    actions[f1] = (
                        ACTION_MERGE,
                        (f1, f2, fa, False, pa.node()),
                        "both renamed from " + fa,
                    )
                else:
                    actions[f1] = (
                        ACTION_MERGE,
                        (f1, f2, None, False, pa.node()),
                        "both created",
                    )
            else:
                nol = "l" not in fl1 + fl2 + fla
                if (
                    files_equal(n2, na, p2, pa, f2, fa) and fl2 == fla
                ):  # remote unchanged
                    actions[f1] = (ACTION_KEEP, (), "remote unchanged")
                elif (
                    files_equal(n1, na, wctx, pa, f1, fa) and fl1 == fla
                ):  # local unchanged - use remote
                    if fl1 == fl2:
                        actions[f1] = (ACTION_GET, (f2, fl2, False), "remote is newer")
                    else:
                        actions[f1] = (
                            ACTION_REMOVE_GET,
                            (f2, fl2, False),
                            "flag differ",
                        )
                elif nol and files_equal(
                    n2, na, p2, pa, f2, fa
                ):  # remote only changed 'x' (file executable)
                    actions[f1] = (ACTION_EXEC, (fl2,), "update permissions")
                elif nol and files_equal(
                    n1, na, wctx, pa, f1, fa
                ):  # local only changed 'x' (file executable)
                    actions[f1] = (ACTION_GET, (f2, fl1, False), "remote is newer")
                else:  # both changed something
                    actions[f1] = (
                        ACTION_MERGE,
                        (f1, f2, fa, False, pa.node()),
                        "versions differ",
                    )
        elif n1:  # file exists only on local side
            if handle_file_on_other_side(f1, diff, reverse_copies):
                pass  # we'll deal with it on `elif n2` side
            elif f1 in copy:
                f1prev = copy[f1]
                f2 = m2.ungraftedpath(f1prev) or f1prev
                remote_copy_source = m2.lookupfile(f2)
                if remote_copy_source is None:
                    actions[f1] = (
                        ACTION_VISIBLE_RESTRICTED,
                        (f1, None, f2, False, pa.node()),
                        "prompt restricted",
                    )
                elif remote_copy_source:
                    actions[f1] = (
                        ACTION_MERGE,
                        (f1, f2, f2, False, pa.node()),
                        "local copied/moved from " + f1prev,
                    )
                else:
                    # copy source doesn't exist - treat this as
                    # a change/delete conflict.
                    actions[f1] = (
                        ACTION_CHANGED_DELETED,
                        (f1, None, f2, False, pa.node()),
                        "prompt changed/deleted copy source",
                    )
            elif ancestor_file:  # clean, a different, no remote
                if not files_equal(n1, na, wctx, pa, f1, fa):
                    if m2.lookupfile(f2) is None:
                        actions[f1] = (
                            ACTION_VISIBLE_RESTRICTED,
                            (f1, None, fa, False, pa.node()),
                            "prompt restricted",
                        )
                    elif acceptremote:
                        actions[f1] = (ACTION_REMOVE, None, "remote delete")
                    else:
                        actions[f1] = (
                            ACTION_CHANGED_DELETED,
                            (f1, None, fa, False, pa.node()),
                            "prompt changed/deleted",
                        )
                elif n1 == addednodeid:
                    # addednodeid is added by working copy manifest to mark
                    # the file as locally added. We should forget it instead of
                    # deleting it.
                    actions[f1] = (ACTION_FORGET, None, "remote deleted")
                else:
                    actions[f1] = (ACTION_REMOVE, None, "other deleted")
            elif m2.lookupfile(f2) is None:
                actions[f1] = (
                    ACTION_VISIBLE_RESTRICTED,
                    (f1, None, f2, False, pa.node()),
                    "prompt restricted",
                )
        elif n2:  # file exists only on remote side
            if handle_file_on_other_side(f1, diff, reverse_copies):
                pass  # we'll deal with it on `elif n1` side
            elif f1 in copy:
                f1prev = copy[f1]
                f2prev = m2.ungraftedpath(f1prev) or f1prev
                remote_copy_source = m2.lookupfile(f2prev)
                if m1.lookupfile(f1) is None:
                    actions[f1] = (
                        ACTION_VISIBLE_RESTRICTED,
                        (None, f2, f2prev, False, pa.node()),
                        "prompt restricted",
                    )
                elif remote_copy_source:
                    actions[f1] = (
                        ACTION_MERGE,
                        (f1prev, f2, f2prev, False, pa.node()),
                        "remote copied from " + f2prev,
                    )
                else:
                    actions[f1] = (
                        ACTION_MERGE,
                        (f1prev, f2, f2prev, True, pa.node()),
                        "remote moved from " + f2prev,
                    )
            elif not ancestor_file:
                if m1.lookupfile(f1) is None:
                    actions[f1] = (
                        ACTION_VISIBLE_RESTRICTED,
                        (None, f2, fa, False, pa.node()),
                        "prompt restricted",
                    )
                else:
                    # local unknown, remote created: the logic is described by the
                    # following table:
                    #
                    # force  branchmerge  different  |  action
                    #   n         *           *      |   create
                    #   y         n           *      |   create
                    #   y         y           n      |   create
                    #   y         y           y      |   merge
                    #
                    # Checking whether the files are different is expensive, so we
                    # don't do that when we can avoid it.
                    if not force:
                        actions[f1] = (ACTION_CREATED, (f2, fl2), "remote created")
                    elif not branchmerge:
                        actions[f1] = (ACTION_CREATED, (f2, fl2), "remote created")
                    else:
                        actions[f1] = (
                            ACTION_CREATED_MERGE,
                            (f2, fl2, pa.node()),
                            "remote created, get or merge",
                        )
            elif not files_equal(n2, na, p2, pa, f2, fa):
                if m1.lookupfile(f1) is None:
                    actions[f1] = (
                        ACTION_VISIBLE_RESTRICTED,
                        (None, f2, fa, False, pa.node()),
                        "prompt restricted",
                    )
                elif acceptremote:
                    actions[f1] = (
                        ACTION_CREATED,
                        (f2, fl2),
                        "remote recreating",
                    )
                else:
                    actions[f1] = (
                        ACTION_DELETED_CHANGED,
                        (None, f2, fa, False, pa.node()),
                        "prompt deleted/changed",
                    )

    if ui.configbool("experimental", "merge.checkpathconflicts"):
        # If we are merging, look for path conflicts.
        checkpathconflicts(to_repo, wctx, p2, actions)

    return actions


def _resolvetrivial(wctx, mctx, ancestor, actions):
    """Resolves false conflicts where the nodeid changed but the content
    remained the same."""
    for f, (m, args, msg) in list(actions.items()):
        if m == ACTION_CHANGED_DELETED:
            fa = args[2]
            if msg == "prompt changed/deleted copy source":
                # TODO: handle copy case
                continue
            if fa in ancestor and not wctx[f].cmp(ancestor[fa]):
                # local did change but ended up with same content
                actions[f] = "r", None, "prompt same"
        elif m == ACTION_DELETED_CHANGED:
            f2, fa = args[1], args[2]
            if fa in ancestor and not mctx[f2].cmp(ancestor[fa]):
                # remote did change but ended up with same content
                del actions[f]  # don't get = keep local deleted


@perftrace.tracefunc("Calculate Updates")
@util.timefunction("calculateupdates", 0, "ui")
def calculateupdates(
    to_repo,
    wctx,
    mctx,
    ancestors,
    branchmerge,
    force,
    acceptremote,
    followcopies,
    from_repo=None,
):
    """Calculate the actions needed to merge mctx into wctx using ancestors"""
    from_repo = from_repo or to_repo
    ui = to_repo.ui

    if len(ancestors) == 1:  # default
        actions = manifestmerge(
            to_repo,
            wctx,
            mctx,
            ancestors[0],
            branchmerge,
            force,
            acceptremote,
            followcopies,
            from_repo=from_repo,
        )
        _checkunknownfiles(to_repo, wctx, mctx, force, actions)

    else:  # only when merge.preferancestor=* - the default
        ui.note(
            _("note: merging %s and %s using bids from ancestors %s\n")
            % (wctx, mctx, _(" and ").join(str(anc) for anc in ancestors))
        )

        # Call for bids
        fbids = {}  # mapping filename to bids (action method to list af actions)
        for ancestor in ancestors:
            ui.note(_("\ncalculating bids for ancestor %s\n") % ancestor)
            actions = manifestmerge(
                to_repo,
                wctx,
                mctx,
                ancestor,
                branchmerge,
                force,
                acceptremote,
                followcopies,
                forcefulldiff=True,
                from_repo=from_repo,
            )
            _checkunknownfiles(to_repo, wctx, mctx, force, actions)

            for f, a in sorted(actions.items()):
                m, args, msg = a
                ui.debug(" %s: %s -> %s\n" % (f, msg, m))
                if f in fbids:
                    d = fbids[f]
                    if m in d:
                        d[m].append(a)
                    else:
                        d[m] = [a]
                else:
                    fbids[f] = {m: [a]}

        # Pick the best bid for each file
        ui.note(_("\nauction for merging merge bids\n"))
        actions = {}
        dms = []  # filenames that have dm actions
        for f, bids in sorted(fbids.items()):
            # bids is a mapping from action method to list af actions
            # Consensus?
            if len(bids) == 1:  # all bids are the same kind of method
                m, l = list(bids.items())[0]
                if all(a == l[0] for a in l[1:]):  # len(bids) is > 1
                    ui.note(_(" %s: consensus for %s\n") % (f, m))
                    actions[f] = l[0]
                    if m == ACTION_DIR_RENAME_MOVE_LOCAL:
                        dms.append(f)
                    continue
            # If keep is an option, just do it.
            if ACTION_KEEP in bids:
                ui.note(_(" %s: picking 'keep' action\n") % f)
                actions[f] = bids[ACTION_KEEP][0]
                continue
            # If there are gets and they all agree [how could they not?], do it.
            if ACTION_GET in bids:
                ga0 = bids[ACTION_GET][0]
                if all(a == ga0 for a in bids[ACTION_GET][1:]):
                    ui.note(_(" %s: picking 'get' action\n") % f)
                    actions[f] = ga0
                    continue
            # Same for symlink->file change
            if ACTION_REMOVE_GET in bids:
                ga0 = bids[ACTION_REMOVE_GET][0]
                if all(a == ga0 for a in bids[ACTION_REMOVE_GET][1:]):
                    ui.note(_(" %s: picking 'remove-then-get' action\n") % f)
                    actions[f] = ga0
                    continue
            # TODO: Consider other simple actions such as mode changes
            # Handle inefficient democrazy.
            ui.note(_(" %s: multiple bids for merge action:\n") % f)
            for m, l in sorted(bids.items()):
                for _f, args, msg in l:
                    ui.note("  %s -> %s\n" % (msg, m))
            # Pick random action. TODO: Instead, prompt user when resolving
            m, l = list(bids.items())[0]
            ui.warn(_(" %s: ambiguous merge - picked %s action\n") % (f, m))
            actions[f] = l[0]
            if m == ACTION_DIR_RENAME_MOVE_LOCAL:
                dms.append(f)
            continue
        # Work around 'dm' that can cause multiple actions for the same file
        for f in dms:
            dm, (f0, flags), msg = actions[f]
            assert dm == ACTION_DIR_RENAME_MOVE_LOCAL, dm
            if f0 in actions and actions[f0][0] == ACTION_REMOVE:
                # We have one bid for removing a file and another for moving it.
                # These two could be merged as first move and then delete ...
                # but instead drop moving and just delete.
                del actions[f]
        ui.note(_("end of auction\n\n"))

    _resolvetrivial(wctx, mctx, ancestors[0], actions)

    if wctx.rev() is None and not wctx.isinmemory():
        fractions = _forgetremoved(wctx, mctx, branchmerge)
        actions.update(fractions)

    return actions


def _diff_manifests(m1, m2, matcher=None):
    if m1.hasgrafts() or m2.hasgrafts():
        m1, m2 = rustmanifest.treemanifest.applydiffgrafts(m1, m2)
    return m1.diff(m2, matcher)


def removeone(repo, wctx, f):
    wctx[f].audit()
    try:
        wctx[f].remove(ignoremissing=True)
    except OSError as inst:
        repo.ui.warn(_("update failed to remove %s: %s!\n") % (f, inst.strerror))


def batchremove(repo, wctx, actions):
    """apply removes to the working directory

    yields tuples for progress updates
    """
    verbose = repo.ui.verbose
    cwd = util.getcwdsafe()
    i = 0
    for f, args, msg in actions:
        repo.ui.debug(" %s: %s -> r\n" % (f, msg))
        if verbose:
            repo.ui.note(_("removing %s\n") % f)
        removeone(repo, wctx, f)
        if i == 100:
            yield i, 0, f
            i = 0
        i += 1
    if i > 0:
        yield i, 0, f

    if cwd and not util.getcwdsafe():
        # cwd was removed in the course of removing files; print a helpful
        # warning.
        repo.ui.warn(
            _("current directory was removed\n(consider changing to repo root: %s)\n")
            % repo.root
        )


def updateone(repo, fctxfunc, wctx, f, f2, flags, backup=False, backgroundclose=False):
    if backup:
        # If a file or directory exists with the same name, back that
        # up.  Otherwise, look to see if there is a file that conflicts
        # with a directory this file is in, and if so, back that up.
        absf = repo.wjoin(f)
        if not repo.wvfs.lexists(f):
            for p in util.finddirs(f):
                if repo.wvfs.isfileorlink(p):
                    absf = repo.wjoin(p)
                    break
        orig = scmutil.origpath(repo.ui, repo, absf)
        if repo.wvfs.lexists(absf):
            util.rename(absf, orig)
    fctx = fctxfunc(f2)
    if fctx.flags() == "m" and not wctx.isinmemory():
        # Do not handle submodules for on-disk checkout here.
        # They are handled separately.
        return 0
    wctx[f].clearunknown()
    wctx[f].write(fctx, flags, backgroundclose=backgroundclose)

    if wctx.isinmemory():
        # The "size" return value is only used for logging "Disk Writes" - not important
        # for in-memory work.
        return 0

    if fctx.flags() == "m":
        # size() doesn't seem to work for submodules
        return len(fctx.data())
    else:
        return fctx.size()


def batchget(repo, mctx, wctx, actions):
    """apply gets to the working directory

    mctx is the context to get from

    yields tuples for progress updates
    """
    verbose = repo.ui.verbose
    fctx = mctx.filectx
    ui = repo.ui
    i = 0
    size = 0
    with repo.wvfs.backgroundclosing(ui, expectedcount=len(actions)):
        for f, (f2, flags, backup), msg in actions:
            repo.ui.debug(" %s: %s -> g\n" % (f, msg))
            if verbose:
                repo.ui.note(_("getting %s\n") % f)

            size += updateone(
                repo, fctx, wctx, f, f2, flags, backup, backgroundclose=True
            )
            if i == 100:
                yield i, size, f
                i = 0
                size = 0
            i += 1
    if i > 0:
        yield i, size, f


@perftrace.tracefunc("Apply Updates")
@util.timefunction("applyupdates", 0, "ui")
def applyupdates(
    to_repo, actions, wctx, mctx, overwrite, labels=None, ancestors=None, from_repo=None
):
    """apply the merge action list to the working directory

    wctx is the working copy context
    mctx is the context to be merged into the working copy

    Return a tuple of counts (updated, merged, removed, unresolved) that
    describes how many files were affected by the update.
    """
    perftrace.tracevalue("Actions", sum(len(v) for k, v in actions.items()))

    from_repo = from_repo or to_repo
    is_crossrepo = not to_repo.is_same_repo(from_repo)
    ui = to_repo.ui

    updated, merged, removed = 0, 0, 0
    other_node = mctx.node()

    ms = mergestate.clean(
        to_repo,
        node=wctx.p1().node(),
        other=other_node,
        # Ancestor can include the working copy, so we use this helper:
        ancestors=(
            [scmutil.contextnodesupportingwdir(c) for c in ancestors]
            if ancestors
            else None
        ),
        labels=labels,
        inmemory=wctx.isinmemory(),
        from_repo=from_repo,
    )

    from_repo_url = None
    if is_crossrepo:
        from_repo_url = from_repo.ui.config("paths", "default")
    for from_path, to_path in mctx.manifest().diffgrafts():
        ms.add_subtree_merge(other_node, from_path, to_path, from_repo_url)

    moves = []
    for m, l in actions.items():
        l.sort()

    def iter_merge_conflict_actions():
        for action_type in (
            ACTION_CHANGED_DELETED,
            ACTION_DELETED_CHANGED,
            ACTION_VISIBLE_RESTRICTED,
            ACTION_MERGE,
        ):
            for f, args, msg in actions.get(action_type, ()):
                yield action_type, f, args, msg

    # Prefetch content for files to be merged to avoid serial lookups.
    merge_prefetch = []
    for _action_type, f, args, msg in iter_merge_conflict_actions():
        f1, f2, fa, move, anc = args
        if f1 is not None:
            merge_prefetch.append(wctx[f1])
        if is_crossrepo:
            # For cross-repo merges the external git repo is not a lazy repo,
            # so there’s no need to prefetch the files.
            continue
        if f2 is not None:
            merge_prefetch.append(mctx[f2])
        actx = from_repo[anc]
        if fa in actx:
            merge_prefetch.append(actx[fa])
    if merge_prefetch and hasattr(to_repo, "fileservice"):
        to_repo.fileservice.prefetch(
            [
                (fc.path(), fc.filenode())
                for fc in merge_prefetch
                if fc.filenode() not in (None, nullid)
            ],
            fetchhistory=False,
        )

    # These are m(erge) actions that aren't actually conflicts, such as remote copying a
    # file. We don't want to expose them to merge drivers since merge drivers might get
    # confused.
    extra_gets = []

    # These actions are treated like other merge conflicts.
    mergeactions = []
    for action_type, f, args, msg in iter_merge_conflict_actions():
        f1, f2, fa, move, anc = args
        restricted_side = None
        if action_type == ACTION_VISIBLE_RESTRICTED:
            restricted_side = "local" if f1 is None else "other"

        if restricted_side == "local":
            fcl = filemerge.absentfilectx(wctx, f, restricted=True)
        elif f1 is None:
            fcl = filemerge.absentfilectx(wctx, fa)
        else:
            ui.debug(" preserving %s for resolve of %s\n" % (f1, f))
            fcl = wctx[f1]
        if restricted_side == "other":
            fco = filemerge.absentfilectx(mctx, f, restricted=True)
        elif f2 is None:
            fco = filemerge.absentfilectx(mctx, fa)
        else:
            fco = mctx[f2]
        actx = from_repo[anc]
        if fa in actx:
            fca = actx[fa]
        else:
            fca = to_repo.filectx(fa or f1 or f2 or f, changeid=nullid, fileid=nullid)
        # Skip submodules for now
        if fcl.flags() == "m" or fco.flags() == "m":
            continue

        # Whether local file and ancestor file differ in any way. Restricted
        # sides still need to be surfaced to the user even when represented as
        # absent, because we cannot safely apply the changed side there.
        conflicting = action_type == ACTION_VISIBLE_RESTRICTED or (
            fca.cmp(fcl) or fca.flags() != fcl.flags() or fca.path() != fcl.path()
        )

        if conflicting:
            ms.add(fcl, fco, fca, f)
            mergeactions.append((f, args, msg))
        else:
            # Ancestor file and local file are identical - no real conflict. Turn this
            # action into a "g", and don't record in mergestate. We keep it an "m" in
            # actions so that recordupdates() has all the "m" info to record copy
            # information.
            extra_gets.append((f, (f2, fco.flags(), False), msg))

        if f1 != f and move:
            moves.append(f1)

    # remove renamed files after safely stored
    for f in moves:
        if wctx[f].lexists():
            ui.debug("removing %s\n" % f)
            wctx[f].audit()
            wctx[f].remove()

    numupdates = sum(len(l) for m, l in actions.items() if m != ACTION_KEEP)
    z = 0

    def userustworker():
        return "remotefilelog" in to_repo.requirements and not wctx.isinmemory()

    rustworkers = userustworker()

    # record path conflicts
    with (
        progress.bar(ui, _("updating"), _("files"), numupdates) as prog,
        ui.timesection("updateworker"),
    ):
        for f, args, msg in actions[ACTION_PATH_CONFLICT]:
            f1, fo = args
            s = ui.status
            s(
                _(
                    "%s: path conflict - a file or link has the same name as a "
                    "directory\n"
                )
                % f
            )
            if fo == "l":
                s(_("the local file has been renamed to %s\n") % f1)
            else:
                s(_("the remote file has been renamed to %s\n") % f1)
            s(_("resolve manually then use '@prog@ resolve --mark %s'\n") % f)
            ms.addpath(f, f1, fo)
            z += 1
            prog.value = (z, f)

        # remove in parallel (must come before resolving path conflicts and
        # getting)
        if rustworkers:
            # Flush any pending data to disk before forking workers, so the workers
            # don't all flush duplicate data.
            to_repo.commitpending()

            # Removing lots of files very quickly is known to cause FSEvents to
            # lose events which forces watchman to recrwawl the entire
            # repository. For very large repository, this can take many
            # minutes, slowing down all the other tools that rely on it. Thus
            # add a config that can be tweaked to specifically reduce the
            # amount of concurrency.
            numworkers = ui.configint(
                "experimental", "numworkersremover", worker._numworkers(ui)
            )
            remover = rustworker.removerworker(to_repo.wvfs.base, numworkers)
            for f, args, msg in actions[ACTION_REMOVE] + actions[ACTION_REMOVE_GET]:
                # The remove method will either return immediately or block if
                # the internal worker queue is full.
                remover.remove(f)
                z += 1
                prog.value = (z, f)
            retry = remover.wait()
            for f in retry:
                ui.debug("retrying %s\n" % f)
                removeone(to_repo, wctx, f)
        else:
            for i, size, item in batchremove(
                to_repo, wctx, actions[ACTION_REMOVE] + actions[ACTION_REMOVE_GET]
            ):
                z += i
                prog.value = (z, item)
        # "rg" actions are counted in updated below
        removed = len(actions[ACTION_REMOVE])

        # resolve path conflicts (must come before getting)
        for f, args, msg in actions[ACTION_PATH_CONFLICT_RESOLVE]:
            ui.debug(" %s: %s -> pr\n" % (f, msg))
            (f0,) = args
            if wctx[f0].lexists():
                ui.note(_("moving %s to %s\n") % (f0, f))
                wctx[f].audit()
                wctx[f].write(wctx.filectx(f0), wctx.filectx(f0).flags())
                wctx[f0].remove()
            z += 1
            prog.value = (z, f)

        # get in parallel
        writesize = 0

        get_actions = actions[ACTION_GET] + actions[ACTION_REMOVE_GET] + extra_gets

        if rustworkers:
            numworkers = ui.configint(
                "experimental", "numworkerswriter", worker._numworkers(ui)
            )

            writer = rustworker.writerworker(
                to_repo.fileslog.filestore, to_repo.wvfs.base, numworkers
            )
            fctx = mctx.filectx
            slinkfix = util.iswindows and to_repo.wvfs._cansymlink
            slinks = []
            ftof2 = {}
            for f, (f2, flags, backup), msg in get_actions:
                if f != f2:
                    ftof2[f] = f2
                if slinkfix and "l" in flags:
                    slinks.append(f)
                fnode = fctx(f2).filenode()
                # The write method will either return immediately or block if
                # the internal worker queue is full.
                writer.write(f, fnode, flags)

                z += 1
                prog.value = (z, f)

            writesize, retry = writer.wait()
            for f, flag in retry:
                ui.debug("retrying %s\n" % f)
                writesize += updateone(to_repo, fctx, wctx, f, ftof2.get(f, f), flag)
            if slinkfix:
                nativecheckout.fixsymlinks(slinks, to_repo.wvfs.base)
        else:
            for i, size, item in batchget(to_repo, mctx, wctx, get_actions):
                z += i
                writesize += size
                prog.value = (z, item)
        updated = len(get_actions)
        perftrace.tracebytes("Disk Writes", writesize)

        # forget (manifest only, just log it) (must come first)
        for f, args, msg in actions[ACTION_FORGET]:
            ui.debug(" %s: %s -> f\n" % (f, msg))
            z += 1
            prog.value = (z, f)

        # re-add (manifest only, just log it)
        for f, args, msg in actions[ACTION_ADD]:
            ui.debug(" %s: %s -> a\n" % (f, msg))
            z += 1
            prog.value = (z, f)

        # re-add/mark as modified (manifest only, just log it)
        for f, args, msg in actions[ACTION_ADD_MODIFIED]:
            ui.debug(" %s: %s -> am\n" % (f, msg))
            z += 1
            prog.value = (z, f)

        # keep (noop, just log it)
        for f, args, msg in actions[ACTION_KEEP]:
            ui.debug(" %s: %s -> k\n" % (f, msg))
            # no progress

        # directory rename, move local
        for f, args, msg in actions[ACTION_DIR_RENAME_MOVE_LOCAL]:
            ui.debug(" %s: %s -> dm\n" % (f, msg))
            z += 1
            prog.value = (z, f)
            f0, flags = args
            ui.note(_("moving %s to %s\n") % (f0, f))
            wctx[f].audit()
            wctx[f].write(wctx.filectx(f0), flags)
            wctx[f0].remove()
            updated += 1

        # local directory rename, get
        for f, args, msg in actions[ACTION_LOCAL_DIR_RENAME_GET]:
            ui.debug(" %s: %s -> dg\n" % (f, msg))
            z += 1
            prog.value = (z, f)
            f0, flags = args
            ui.note(_("getting %s to %s\n") % (f0, f))
            wctx[f].write(mctx.filectx(f0), flags)
            updated += 1

        # exec
        for f, args, msg in actions[ACTION_EXEC]:
            ui.debug(" %s: %s -> e\n" % (f, msg))
            z += 1
            prog.value = (z, f)
            (flags,) = args
            wctx[f].audit()
            wctx[f].setflags("l" in flags, "x" in flags)
            updated += 1

        perftrace.tracevalue("Deleted Files", removed)
        perftrace.tracevalue("Written Files", updated)

        # the ordering is important here -- ms.mergedriver will raise if the
        # merge driver has changed, and we want to be able to bypass it when
        # overwrite is True
        usemergedriver = not overwrite and mergeactions and ms.mergedriver

        if usemergedriver:
            ms.commit()
            with ui.timesection("mergedriver"):
                # This will return False if the function raises an exception.
                failed = not driverpreprocess(to_repo, ms, wctx, labels=labels)
            driverresolved = [f for f in ms.driverresolved()]

            ui.log("command_metrics", mergedriver_num_files=len(driverresolved))

            # If preprocess() marked any files as driver-resolved and we're
            # merging in-memory, abort on the assumption that driver scripts
            # require the working directory.
            if driverresolved and wctx.isinmemory():
                errorstr = (
                    "some of your files require mergedriver to run, "
                    "which in-memory merge does not support"
                )
                raise error.InMemoryMergeConflictsError(
                    errorstr,
                    type=error.InMemoryMergeConflictsError.TYPE_MERGEDRIVER,
                    paths=driverresolved,
                )

            # NOTE(phillco): This used to say "the driver might leave some files unresolved",
            # but this actually only handles the case where preprocess() fails. A preprocess()
            # script can also leave files unmarked without failing.
            unresolvedf = set(ms.unresolved())
            if failed:
                # Preprocess failed, so don't proceed in either case.
                if wctx.isinmemory():
                    raise error.InMemoryMergeConflictsError(
                        "preprocess() raised an exception",
                        type=error.InMemoryMergeConflictsError.TYPE_FILE_CONFLICTS,
                        paths=list(unresolvedf),
                    )
                else:
                    # XXX setting unresolved to at least 1 is a hack to make sure we
                    # error out
                    return updated, merged, removed, max(len(unresolvedf), 1)
            newactions = []
            for f, args, msg in mergeactions:
                if f in unresolvedf:
                    newactions.append((f, args, msg))
            mergeactions = newactions

        try:
            # premerge
            tocomplete = []
            completed = []
            for f, args, msg in mergeactions:
                ui.debug(" %s: %s -> m (premerge)\n" % (f, msg))
                z += 1
                prog.value = (z, f)
                wfctx = wctx[f]
                wfctx.audit()
                # Skip submodules for now
                try:
                    if wfctx.flags() == "m":
                        continue
                except (error.ManifestLookupError, error.PermissionDeniedError):
                    # Cannot check the flags, so treat this as a regular file.
                    # This code path is hit by test-rebase-inmemory-conflicts.t.
                    pass
                complete, r = ms.preresolve(f, wctx)
                if not complete:
                    numupdates += 1
                    tocomplete.append((f, args, msg))
                else:
                    completed.append(f)

            # merge
            files = []
            for f, args, msg in tocomplete:
                ui.debug(" %s: %s -> m (merge)\n" % (f, msg))
                z += 1
                prog.value = (z, f)
                ms.resolve(f, wctx)
                files.append(f)
            reponame = ui.config("fbscmquery", "reponame")
            command = " ".join(util.shellquote(a) for a in sys.argv)
            ui.log(
                "manualmergefiles",
                manual_merge_files=",".join(files),
                auto_merge_files=",".join(completed),
                command=command,
                repo=reponame,
            )
            if files:
                ui.log(
                    "merge_conflicts",
                    command=ui.cmdname,
                    full_command=command,
                    dest_hex=_gethex(wctx),
                    src_hex=_gethex(mctx),
                    repo=reponame,
                    manual_merge_files_count=len(files),
                    manual_merge_files=",".join(files),
                )
        finally:
            ms.commit()

        unresolved = ms.unresolvedcount()

        if usemergedriver and not unresolved and ms.mdstate() != "s":
            with ui.timesection("mergedriver"):
                if not driverconclude(to_repo, ms, wctx, labels=labels):
                    # XXX setting unresolved to at least 1 is a hack to make
                    # sure we error out
                    unresolved = max(unresolved, 1)
                elif recountunresolvedafterconclude(ui):
                    unresolved = ms.unresolvedcount()

            ms.commit()

        msupdated, msmerged, msremoved = ms.counts()
        updated += msupdated
        merged += msmerged
        removed += msremoved

        extraactions = ms.actions()
        if extraactions:
            # A same file might exist both in extraactions[ACTION_REMOVE] (to remove)
            # list, and actions[ACTION_GET] (to create) list. Remove them from
            # actions[ACTION_GET] to avoid conflicts.
            extraremoved = {item[0] for item in extraactions[ACTION_REMOVE]}
            if extraremoved:
                actions[ACTION_GET] = [
                    item for item in actions[ACTION_GET] if item[0] not in extraremoved
                ]

            mfiles = set(a[0] for a in actions[ACTION_MERGE])
            for k, acts in extraactions.items():
                actions[k].extend(acts)
                # Remove these files from actions[ACTION_MERGE] as well. This is
                # important because in recordupdates, files in actions[ACTION_MERGE] are
                # processed after files in other actions, and the merge driver
                # might add files to those actions via extraactions above. This
                # can lead to a file being recorded twice, with poor results.
                # This is especially problematic for actions[ACTION_REMOVE] (currently
                # only possible with the merge driver in the initial merge
                # process; interrupted merges don't go through this flow).
                #
                # The real fix here is to have indexes by both file and action
                # so that when the action for a file is changed it is
                # automatically reflected in the other action lists. But that
                # involves a more complex data structure, so this will do for
                # now.
                #
                # We don't need to do the same operation for ACTION_DELETED_CHANGED and
                # ACTION_CHANGED_DELETED, because those lists aren't consulted again.
                mfiles.difference_update(a[0] for a in acts)

            actions[ACTION_MERGE] = [a for a in actions[ACTION_MERGE] if a[0] in mfiles]

    return updated, merged, removed, unresolved


def recordupdates(to_repo, actions, branchmerge, from_repo=None):
    "record merge actions to the dirstate"

    ui = to_repo.ui
    from_repo = from_repo or to_repo
    is_crossrepo = not from_repo.is_same_repo(to_repo)

    total = sum(map(len, actions.values()))

    with progress.bar(ui, _("recording"), _("files"), total) as prog:
        # remove (must come first)
        for f, args, msg in actions.get(ACTION_REMOVE, []):
            if branchmerge:
                to_repo.dirstate.remove(f)
            else:
                to_repo.dirstate.delete(f)
            prog.value += 1

        # forget (must come first)
        for f, args, msg in actions.get(ACTION_FORGET, []):
            to_repo.dirstate.untrack(f)
            prog.value += 1

        # resolve path conflicts
        copied = to_repo.dirstate.copies()
        for f, args, msg in actions.get(ACTION_PATH_CONFLICT_RESOLVE, []):
            (f0,) = args
            origf0 = copied.get(f0, f0)
            to_repo.dirstate.add(f)
            to_repo.dirstate.copy(origf0, f)
            if f0 == origf0:
                to_repo.dirstate.remove(f0)
            else:
                to_repo.dirstate.delete(f0)
            prog.value += 1

        # re-add
        for f, args, msg in actions.get(ACTION_ADD, []):
            to_repo.dirstate.add(f)
            prog.value += 1

        # re-add/mark as modified
        for f, args, msg in actions.get(ACTION_ADD_MODIFIED, []):
            if branchmerge:
                to_repo.dirstate.normallookup(f)
            else:
                to_repo.dirstate.add(f)
            prog.value += 1

        # exec change
        for f, args, msg in actions.get(ACTION_EXEC, []):
            to_repo.dirstate.normallookup(f)
            prog.value += 1

        # keep
        for f, args, msg in actions.get(ACTION_KEEP, []):
            prog.value += 1

        # get
        for f, args, msg in actions.get(ACTION_GET, []) + actions.get(
            ACTION_REMOVE_GET, []
        ):
            if branchmerge:
                to_repo.dirstate.otherparent(f)
            else:
                to_repo.dirstate.normal(f)
            prog.value += 1

        # merge
        for f, args, msg in actions.get(ACTION_MERGE, []):
            f1, f2, fa, move, anc = args
            if branchmerge:
                # We've done a branch merge, mark this file as merged
                # so that we properly record the merger later
                to_repo.dirstate.merge(f)
                # XXX: handle cross-repo copy
                if not is_crossrepo and f1 != f2:  # copy/rename
                    if move:
                        to_repo.dirstate.remove(f1)
                    if f1 != f:
                        to_repo.dirstate.copy(f1, f)
                    else:
                        to_repo.dirstate.copy(f2, f)
            else:
                # We've update-merged a locally modified file, so
                # we set the dirstate to emulate a normal checkout
                # of that file some time in the past. Thus our
                # merge will appear as a normal local file
                # modification.
                if f2 == f:  # file not locally copied/moved
                    to_repo.dirstate.normallookup(f)
                if move:
                    to_repo.dirstate.delete(f1)
            prog.value += 1

        # directory rename, move local
        for f, args, msg in actions.get(ACTION_DIR_RENAME_MOVE_LOCAL, []):
            f0, flag = args
            if branchmerge:
                to_repo.dirstate.add(f)
                to_repo.dirstate.remove(f0)
                to_repo.dirstate.copy(f0, f)
            else:
                to_repo.dirstate.normal(f)
                to_repo.dirstate.delete(f0)
            prog.value += 1

        # directory rename, get
        for f, args, msg in actions.get(ACTION_LOCAL_DIR_RENAME_GET, []):
            f0, flag = args
            if branchmerge:
                to_repo.dirstate.add(f)
                to_repo.dirstate.copy(f0, f)
            else:
                to_repo.dirstate.normal(f)
            prog.value += 1


def _logupdatedistance(ui, repo, node):
    """Logs the update distance, if configured"""
    # internal config: merge.recordupdatedistance
    if not ui.configbool("merge", "recordupdatedistance", default=True):
        return

    try:
        # The passed in node might actually be a rev, and if it's -1, that
        # doesn't play nicely with revsets later because it resolve to the tip
        # commit.
        node = repo[node].node()
        distance = len(repo.revs("(%n %% .) + (. %% %n)", node, node))
        repo.ui.log("update_size", update_distance=distance)
    except Exception:
        # error may happen like: RepoLookupError: unknown revision '-1'
        pass


def _prefetchlazychildren(repo, node):
    """Prefetch children for ``node`` for lazy changelog.

    This helps making committing on ``node`` offline-friendly.
    """
    # Prefetch lazy node to make offline commit possible.
    if "lazychangelog" in repo.storerequirements:
        # node might be a revision number.
        if not isinstance(node, bytes):
            node = repo[node].node()
        dag = repo.changelog.dag
        if node in dag.mastergroup():
            # See D30004908. Pre-calculate children(node) so
            # commit on node is more offline friendly.
            try:
                childrennodes = list(dag.children([node]))
            except Exception as e:
                tracing.debug(
                    "cannot resolve children of %s: %r" % (hex(node), e),
                    target="checkout::prefetch",
                )
            else:
                tracing.debug(
                    "children of %s: [%s]"
                    % (hex(node), ", ".join(map(hex, childrennodes))),
                    target="checkout::prefetch",
                )
        else:
            tracing.debug(
                "skip prefetch because %s is not in master (lazy) group" % hex(node),
                target="checkout::prefetch",
            )

    else:
        tracing.debug(
            "skip prefetch for non-lazychangelog",
            target="checkout::prefetch",
        )


def goto(
    repo,
    node,
    force=False,
    labels=None,
    updatecheck=None,
):
    if not force:
        # TODO: remove the default once all callers that pass force=False pass
        # a value for updatecheck. We may want to allow updatecheck='abort' to
        # better support some of these callers.
        if updatecheck is None:
            updatecheck = "none"
        assert updatecheck in ("none", "noconflict")

    if grepo.GREPO_REQUIREMENT in repo.requirements and not force:
        grepo.checkout_manifest(repo, repo["."], repo[node], force, dry_run=True)

    if (
        repo.ui.configbool("workingcopy", "rust-checkout")
        and repo.ui.configbool("checkout", "use-rust")
        and (force or updatecheck != "none")
    ):
        repo.ui.log("checkout_info", python_checkout="rust")
        source = repo["."]
        target = repo[node]
        try:
            with repo.dirstate.parentchange():
                # Trigger lazy loading of Python's treestate. If the below repo.setparents
                # triggers loading, there will be an apparent mismatch between the dirstate
                # read from disk and the in-memory-modified treestate.
                repo.dirstate._map

                if (
                    edenfs.requirement in repo.requirements
                    or git.DOTGIT_REQUIREMENT in repo.requirements
                ):
                    # Flush pending commit data so eden has access to data that that
                    # hasn't been flushed yet.
                    repo.flushpendingtransaction()

                ret = repo._rsrepo.goto(
                    ctx=repo.ui.rustcontext(),
                    target=target.node(),
                    bookmark={"action": "none"},
                    mode="revert_conflicts" if force else "abort_if_conflicts",
                    report_mode="quiet",
                )
                if git.isgitformat(repo):
                    _checkout_projects(repo, source, target, force)
                repo.setparents(target.node())
        except rusterror.CheckoutConflictsError as ex:
            abort_on_conflicts(ex.args[0])
    else:
        repo.ui.log("checkout_info", python_checkout="python")

        _logupdatedistance(repo.ui, repo, node)
        _prefetchlazychildren(repo, node)

        if (
            edenfs.requirement in repo.requirements
            or git.DOTGIT_REQUIREMENT in repo.requirements
        ):
            from . import eden_update

            ret = eden_update.update(
                repo,
                node,
                force=force,
                labels=labels,
                updatecheck=updatecheck,
            )
        else:
            # If we're doing the initial checkout from null, let's use the new fancier
            # nativecheckout, since it has more efficient fetch mechanics.
            # git backend only supports nativecheckout at present.
            isclonecheckout = repo["."].node() == nullid
            did_native_checkout = False

            if (
                repo.ui.configbool("experimental", "nativecheckout")
                or (repo.ui.configbool("clone", "nativecheckout") and isclonecheckout)
                or git.isgitstore(repo)
            ):
                wc = repo[None]

                if (
                    not isclonecheckout
                    and (force or updatecheck != "noconflict")
                    and (wc.dirty(missing=True) or mergestate.read(repo).active())
                ):
                    fallbackcheckout = "Working copy is dirty and --clean specified - not supported yet"
                elif not hasattr(repo.fileslog, "filestore"):
                    fallbackcheckout = "Repo does not have remotefilelog"
                else:
                    fallbackcheckout = None

                if fallbackcheckout:
                    repo.ui.debug("Not using native checkout: %s\n" % fallbackcheckout)
                else:
                    # If the user is attempting to checkout for the first time, let's assume
                    # they don't have any pending changes and let's do a force checkout.
                    # This makes it much faster, by skipping the entire "check for unknown
                    # files" and "check for conflicts" code paths, and makes it so they
                    # aren't blocked by pending files and have to purge+clone over and over.
                    if isclonecheckout:
                        force = True

                    p1 = wc.parents()[0]
                    p2 = repo[node]

                    with repo.wlock():
                        ret = donativecheckout(repo, p1, p2, force, wc)
                        if git.isgitformat(repo):
                            _checkout_projects(repo, p1, p2, force)
                    did_native_checkout = True

            if not did_native_checkout:
                ret = _update(
                    repo,
                    node,
                    force=force,
                    labels=labels,
                    updatecheck=updatecheck,
                )

    return ret


def _checkout_projects(repo, source, target, force):
    """Check out the submodules of `target`.

    In a grepo, check out the manifests before any project. As a result, a
    failed project leaves the manifests at `target`. Call this only after the
    main checkout passed the `preupdate` hook and its conflict checks.
    """
    if grepo.GREPO_REQUIREMENT in repo.requirements:
        grepo.checkout_manifest(repo, source, target, force)
    git.submodulecheckout(target, force=force)


def merge(
    to_repo,
    node,
    force=False,
    ancestor=None,
    mergeancestor=False,
    labels=None,
    wc=None,
    from_repo=None,
):
    from_repo = from_repo or to_repo
    is_crossrepo = not to_repo.is_same_repo(from_repo)
    if not is_crossrepo:
        _prefetchlazychildren(to_repo, node)

    return _update(
        to_repo,
        node,
        branchmerge=True,
        ancestor=ancestor,
        mergeancestor=mergeancestor,
        force=force,
        labels=labels,
        wc=wc,
        from_repo=from_repo,
    )


# Commit extra set on merge commits created by 'merge --noconflict'. Such a
# commit has no content of its own: it only records that its parents combine
# without conflicts, so tools can treat it as bookkeeping.
NOCONFLICT_MERGE_EXTRA = "noconflict_merge"


def merge_in_memory(repo, node, force=False, labels=None, basectx=None, wctx=None):
    """Merge `node` into `basectx` (default: the working copy parent) in
    memory and return the resulting overlay context (`wctx` if given).
    Raises InMemoryMergeConflictsError, with the first affected paths, if
    the merge cannot be completed automatically. The working copy is not
    touched either way.
    """
    from . import context

    if wctx is None:
        wctx = context.overlayworkingctx(repo)
    wctx.setbase(basectx if basectx is not None else repo["."])
    # Callers report progress themselves; this merge is silent.
    repo.ui.pushbuffer(error=True)
    try:
        stats = merge(repo, node, force=force, labels=labels, wc=wctx)
    finally:
        repo.ui.popbuffer()
    if stats[3] > 0:
        # Conflicts normally surface from the file merge itself with their
        # paths; this only guards against a merge that reported unresolved
        # files without raising.
        raise error.InMemoryMergeConflictsError(
            _("%d unresolved files") % stats[3],
            type=error.InMemoryMergeConflictsError.TYPE_FILE_CONFLICTS,
            paths=[],
        )
    return wctx


def check_noconflict_merge(repo, node, force=False, labels=None, basectx=None):
    """Merge `node` into `basectx` (default: the working copy parent) in
    memory and return the result, or abort with the first affected paths."""
    try:
        return merge_in_memory(repo, node, force=force, labels=labels, basectx=basectx)
    except error.InMemoryMergeConflictsError as e:
        paths = sorted(set(e.paths))
        if paths:
            msg = _("merge of %s stopped at conflicts in:\n %s") % (
                repo[node],
                "\n ".join(paths),
            )
        else:
            msg = _("merge of %s would have conflicts: %s") % (repo[node], e)
        raise error.Abort(
            msg,
            hint=_("run '@prog@ merge' without --noconflict to resolve them by hand"),
        )


def is_noconflict_merge(ctx):
    return len(ctx.parents()) == 2 and ctx.extra().get(NOCONFLICT_MERGE_EXTRA) == "1"


def landed_successor(repo, ctx):
    """The public commit an obsolete `ctx` was rewritten into, or None.

    A commit counts as landed when all terminal successors are public and
    the mutation history is present locally.
    """
    from . import mutation

    if not ctx.obsolete() or not mutation.enabled(repo):
        return None
    nodemap = repo.changelog.nodemap
    succs = [n for n in mutation.allsuccessors(repo, [ctx.node()]) if n != ctx.node()]
    if any(n not in nodemap for n in succs):
        return None
    terminal = [
        n for n in succs if repo[n].ispublic() or not mutation.lookupsuccessors(repo, n)
    ]
    if not terminal or not all(repo[n].ispublic() for n in terminal):
        return None
    return terminal[0]


@perftrace.tracefunc("Update")
@util.timefunction("mergeupdate", 0, "ui")
def _update(
    to_repo,
    node,
    branchmerge=False,
    force=False,
    ancestor=None,
    mergeancestor=False,
    labels=None,
    updatecheck=None,
    wc=None,
    from_repo=None,
):
    """
    Perform a merge between the working directory and the given node

    node = the node to update to
    branchmerge = whether to merge between branches
    force = whether to force branch merging or file overwriting
    mergeancestor = whether it is merging with an ancestor. If true,
      we should accept the incoming changes for any prompts that occur.
      If false, merging with an ancestor (fast-forward) is only allowed
      between different named branches. This flag is used by rebase extension
      as a temporary fix and should be avoided in general.
    labels = labels to use for base, local and other

    The table below shows all the behaviors of the update command given the
    -c/--check and -C/--clean or no options, whether the working directory is
    dirty, whether a revision is specified, and the relationship of the parent
    rev to the target rev (linear or not). Match from top first. The -n
    option doesn't exist on the command line, but represents the
    commands.update.check=noconflict option.

    This logic is tested by test-update-branches.t.

    -c  -C  -n  -m  dirty  rev  linear  |  result
     y   y   *   *    *     *     *     |    (1)
     y   *   y   *    *     *     *     |    (1)
     y   *   *   y    *     *     *     |    (1)
     *   y   y   *    *     *     *     |    (1)
     *   y   *   y    *     *     *     |    (1)
     *   *   y   y    *     *     *     |    (1)
     *   *   *   *    *     n     n     |     x
     *   *   *   *    n     *     *     |    ok
     n   n   n   n    y     *     y     |   merge
     n   n   n   n    y     y     n     |    (2)
     n   n   n   y    y     *     *     |   merge
     n   n   y   n    y     *     *     |  merge if no conflict
     n   y   n   n    y     *     *     |  discard
     y   n   n   n    y     *     *     |    (3)

    x = can't happen
    * = don't-care
    1 = incompatible options (checked in commands.py)
    2 = abort: uncommitted changes (commit or goto --clean to discard changes)
    3 = abort: uncommitted changes (checked in commands.py)

    The merge is performed inside ``wc``, a workingctx-like objects. It defaults
    to repo[None] if None is passed.

    Return the same tuple as applyupdates().
    """

    assert node is not None

    ui = to_repo.ui
    from_repo = from_repo or to_repo
    is_crossrepo = not to_repo.is_same_repo(from_repo)

    # Positive indication we aren't using eden fastpath for eden integration tests.
    if edenfs.requirement in to_repo.requirements:
        ui.debug("falling back to non-eden update code path: merge\n")

    with to_repo.wlock():
        if wc is None:
            wc = to_repo[None]
        pl = wc.parents()
        p1 = pl[0]
        pas = [None]
        if ancestor is not None:
            # XXX: For cross-repo grafts, we only support the case where the merge ancestor
            # belongs to from_repo.
            pas = [from_repo[ancestor]]

        overwrite = force and not branchmerge

        p2 = from_repo[node]

        fp1, fp2, xp1, xp2 = p1.node(), p2.node(), str(p1), str(p2)

        if pas[0] is None:
            if is_crossrepo:
                pas = [from_repo[nullid]]
            elif ui.configlist("merge", "preferancestor") == ["*"]:
                cahs = to_repo.changelog.commonancestorsheads(p1.node(), p2.node())
                pas = [to_repo[anc] for anc in (sorted(cahs) or [nullid])]
            else:
                pas = [p1.ancestor(p2, warn=branchmerge)]

        ### check phase
        if not overwrite:
            if len(pl) > 1:
                raise error.Abort(_("outstanding uncommitted merge"))
            ms = mergestate.read(to_repo)
            if list(ms.unresolved()):
                raise error.Abort(_("outstanding merge conflicts"))
        if branchmerge:
            xdir = p2.manifest().hasgrafts()
            if pas == [p2] and not xdir:
                raise error.Abort(
                    _("merging with a working directory ancestor has no effect")
                )
            elif pas == [p1] and not xdir:
                if not mergeancestor:
                    raise error.Abort(
                        _("nothing to merge"),
                        hint=_("use '@prog@ goto' or check '@prog@ heads'"),
                    )
            if not force and (wc.files() or wc.deleted()):
                raise error.Abort(
                    _("uncommitted changes"),
                    hint=_("use '@prog@ status' to list changes"),
                )

        elif not overwrite:
            if p1 == p2:  # no-op update
                # call the hooks and exit early
                if not wc.isinmemory():
                    to_repo.hook("preupdate", throw=True, parent1=xp2, parent2="")
                    to_repo.hook("update", parent1=xp2, parent2="", error=0)
                return 0, 0, 0, 0

        if overwrite:
            pas = [wc]
        elif not branchmerge:
            pas = [p1]

        followcopies = ui.configbool("merge", "followcopies")
        if overwrite:
            followcopies = False
        elif not pas[0]:
            followcopies = False
        if not branchmerge and not wc.dirty(missing=True):
            followcopies = False

        ### calculate phase
        with progress.spinner(ui, "calculating merge actions"):
            actionbyfile = calculateupdates(
                to_repo,
                wc,
                p2,
                pas,
                branchmerge,
                force,
                mergeancestor,
                followcopies,
                from_repo=from_repo,
            )

        if updatecheck == "noconflict":
            paths = []
            cwd = to_repo.getcwd()
            for f, (m, args, msg) in actionbyfile.items():
                if m not in (
                    ACTION_GET,
                    ACTION_KEEP,
                    ACTION_EXEC,
                    ACTION_REMOVE,
                    ACTION_REMOVE_GET,
                    ACTION_PATH_CONFLICT_RESOLVE,
                ):
                    paths.append(to_repo.pathto(f, cwd))

            if paths:
                paths = sorted(paths)
                abort_on_conflicts(paths)

        # Convert to dictionary-of-lists format
        actions = {
            m: []
            for m in (
                ACTION_ADD,
                ACTION_ADD_MODIFIED,
                ACTION_FORGET,
                ACTION_GET,
                ACTION_CHANGED_DELETED,
                ACTION_DELETED_CHANGED,
                ACTION_VISIBLE_RESTRICTED,
                ACTION_REMOVE,
                ACTION_REMOVE_GET,
                ACTION_DIR_RENAME_MOVE_LOCAL,
                ACTION_LOCAL_DIR_RENAME_GET,
                ACTION_MERGE,
                ACTION_EXEC,
                ACTION_KEEP,
                ACTION_PATH_CONFLICT,
                ACTION_PATH_CONFLICT_RESOLVE,
            )
        }
        for f, (m, args, msg) in actionbyfile.items():
            if m not in actions:
                actions[m] = []
            actions[m].append((f, args, msg))

        ### apply phase
        if not branchmerge:  # just jump to the new rev
            fp1, fp2, xp1, xp2 = fp2, nullid, xp2, ""
        if not wc.isinmemory():
            # XXX: extend preupdate hook to support cross repo merge case
            to_repo.hook("preupdate", throw=True, parent1=xp1, parent2=xp2)
            # note that we're in the middle of an update
            to_repo.localvfs.writeutf8("updatestate", p2.hex())

        # Advertise fsmonitor when its presence could be useful.
        #
        # We only advertise when performing an update from an empty working
        # directory. This typically only occurs during initial clone.
        #
        # We give users a mechanism to disable the warning in case it is
        # annoying.
        #
        # We only allow on Linux and MacOS because that's where fsmonitor is
        # considered stable.
        fsmonitorwarning = ui.configbool("fsmonitor", "warn_when_unused")
        fsmonitorthreshold = ui.configint("fsmonitor", "warn_update_file_count")
        try:
            extensions.find("fsmonitor")
            fsmonitorenabled = ui.config("fsmonitor", "mode") != "off"
            # We intentionally don't look at whether fsmonitor has disabled
            # itself because a) fsmonitor may have already printed a warning
            # b) we only care about the config state here.
        except KeyError:
            fsmonitorenabled = False

        if (
            fsmonitorwarning
            and not fsmonitorenabled
            and p1.node() == nullid
            and len(actions[ACTION_GET]) >= fsmonitorthreshold
            and sys.platform.startswith(("linux", "darwin"))
        ):
            ui.warn(
                _(
                    "(warning: large working directory being used without "
                    "fsmonitor enabled; enable fsmonitor to improve performance; "
                    'see "@prog@ help -e fsmonitor")\n'
                )
            )

        stats = applyupdates(
            to_repo,
            actions,
            wc,
            p2,
            overwrite,
            labels=labels,
            ancestors=pas,
            from_repo=from_repo,
        )

        if not wc.isinmemory():
            with to_repo.dirstate.parentchange():
                if is_crossrepo:
                    to_repo.setparents(fp1)
                    to_repo.dirstate.set_xrepo_merge()
                else:
                    to_repo.setparents(fp1, fp2)
                recordupdates(to_repo, actions, branchmerge, from_repo=from_repo)
                # update completed, clear state
                util.unlink(to_repo.localvfs.join("updatestate"))

                # After recordupdates has finished, the checkout is considered
                # finished and we should persist the sparse profile config
                # changes.
                #
                # Ideally this would be part of some wider transaction framework
                # that ensures these things all happen atomically, but that
                # doesn't exist for the dirstate right now.
                if hasattr(to_repo, "_persistprofileconfigs"):
                    to_repo._persistprofileconfigs()

    if git.isgitformat(to_repo) and not wc.isinmemory() and not is_crossrepo:
        if branchmerge:
            git.submodulecheckout(p1, force=force, mctx=p2)
        else:
            _checkout_projects(to_repo, p1, p2, force)

    if not wc.isinmemory():
        # XXX: extend preupdate hook to support cross repo merge case
        to_repo.hook("update", parent1=xp1, parent2=xp2, error=stats[3])

    # Log the number of files updated.
    ui.log("update_size", update_filecount=sum(stats))

    return stats


def abort_on_conflicts(paths):
    msg = _("%d conflicting file changes:\n") % len(paths)
    for path in i18n.limititems(paths):
        msg += " %s\n" % path

    hint = _(
        "commit, shelve, goto --clean to discard all your changes"
        ", or goto --merge to merge them"
    )

    raise error.Abort(msg.strip(), hint=hint)


def getsparsematchers(repo, fp1, fp2):
    shouldsparsematch = sparseutil.shouldsparsematch(repo)
    sparsematch = repo.sparsematch if shouldsparsematch else None
    if sparsematch is not None:
        from sapling.ext import sparse

        revs = {fp1, fp2}
        revs -= {nullid}

        repo._clearpendingprofileconfig(all=True)
        oldpatterns = sparse.getsparsepatterns(repo, fp1)
        oldmatcher = sparsematch(fp1)

        repo._creatependingprofileconfigs()

        newpatterns = sparse.getsparsepatterns(repo, fp2)
        newmatcher = sparsematch(fp2)

        # Ignore files that are not in either source or target sparse match
        # This is not enough if sparse profile changes, but works for checkout within same sparse profile
        matcher = sparsematch(*list(revs))

        # If sparse configs are identical, don't set old/new matchers.
        # This signals to nativecheckout that there isn't a sparse
        # profile transition.
        oldnewmatchers = None
        if not oldpatterns.equivalent(newpatterns):
            oldnewmatchers = (oldmatcher, newmatcher)

        # This can be optimized - if matchers are same, we can set sparsematchers = None
        # sparse.py does not do it, so we are not making things worse
        return matcher, oldnewmatchers
    else:
        return None, None


@util.timefunction("makenativecheckoutplan", 0, "ui")
def makenativecheckoutplan(repo, p1, p2, updateprogresspath=None):
    matcher, sparsematchers = getsparsematchers(repo, p1.node(), p2.node())

    return nativecheckout.checkoutplan(
        repo.ui._rcfg,
        repo.wvfs.base,
        p1.manifest(),
        p2.manifest(),
        matcher,
        sparsematchers,
        updateprogresspath,
    )


@util.timefunction("donativecheckout", 0, "ui")
def donativecheckout(repo, p1, p2, force, wc):
    repo.ui.debug("Using native checkout\n")
    repo.ui.log(
        "nativecheckout",
        using_nativecheckout=True,
    )

    xp1 = str(p1)
    xp2 = str(p2)

    updateprogresspath = None
    if repo.ui.configbool("checkout", "resumable"):
        updateprogresspath = repo.localvfs.join("updateprogress")

    plan = makenativecheckoutplan(repo, p1, p2, updateprogresspath)

    if repo.ui.debugflag:
        repo.ui.debug("Native checkout plan:\n%s\n" % plan)

    if not force:
        status = nativestatus.status(repo.status(unknown=True))
        unknown = plan.check_unknown_files(
            p2.manifest(),
            repo.fileslog.filestore,
            repo.dirstate._map._tree,
            status,
        )
        if unknown:
            for f in unknown:
                repo.ui.warn(_("%s: untracked file differs\n") % f)

            raise error.Abort(
                _(
                    "untracked files in working directory "
                    "differ from files in requested revision"
                )
            )

        conflicts = plan.check_conflicts(status)
        if conflicts:
            msg = _("%d conflicting file changes:\n") % len(conflicts)
            msg += " " + "\n ".join(i18n.limititems(conflicts)) + "\n"
            hint = _(
                "commit, shelve, goto --clean to discard all your changes"
                ", or update --merge to merge them"
            )
            raise error.Abort(msg.strip(), hint=hint)

    # preserving checks as is, even though wc.isinmemory always false here
    if not wc.isinmemory():
        repo.hook("preupdate", throw=True, parent1=xp1, parent2=xp2)
        # note that we're in the middle of an update
        repo.localvfs.writeutf8("updatestate", p2.hex())

    fp1, fp2, xp1, xp2 = p2.node(), nullid, xp2, ""
    cwd = util.getcwdsafe()

    repo.ui.debug("Applying to %s \n" % repo.wvfs.base)
    failed_removes = plan.apply(
        repo.fileslog.filestore,
    )
    for path, err in failed_removes:
        repo.ui.warn(_("update failed to remove %s: %s!\n") % (path, err))
    repo.ui.debug("Apply done\n")
    stats = plan.stats()

    if cwd and not util.getcwdsafe():
        # cwd was removed in the course of removing files; print a helpful
        # warning.
        repo.ui.warn(
            _("current directory was removed\n(consider changing to repo root: %s)\n")
            % repo.root
        )

    if not wc.isinmemory():
        with repo.dirstate.parentchange():
            repo.setparents(fp1, fp2)
            plan.record_updates(repo.dirstate._map._tree)
            # update completed, clear state
            repo.localvfs.unlink("updatestate")
            repo.localvfs.unlink("updateprogress")

            # After recordupdates has finished, the checkout is considered
            # finished and we should persist the sparse profile config
            # changes.
            #
            # Ideally this would be part of some wider transaction framework
            # that ensures these things all happen atomically, but that
            # doesn't exist for the dirstate right now.
            if hasattr(repo, "_persistprofileconfigs"):
                repo._persistprofileconfigs()

        repo.hook("update", parent1=xp1, parent2=xp2, error=stats[3])

    return stats


def graft(to_repo, ctx, pctx, labels, keepparent=False, from_repo=None):
    """Do a graft-like merge.

    This is a merge where the merge ancestor is chosen such that one
    or more changesets are grafted onto the current changeset. In
    addition to the merge, this fixes up the dirstate to include only
    a single parent (if keepparent is False) and tries to duplicate any
    renames/copies appropriately.

    from_repo - the source repo. This may be an external Git repo.
    to_repo - the destination repo (typically the current working repo).
    ctx - changeset to rebase
    pctx - merge base, usually ctx.p1()
    labels - merge labels eg ['local', 'graft']
    keepparent - keep second parent if any
    """
    from_repo = from_repo or to_repo
    is_crossrepo = not to_repo.is_same_repo(from_repo)
    # If we're grafting a descendant onto an ancestor, be sure to pass
    # mergeancestor=True to update. This does two things: 1) allows the merge if
    # the destination is the same as the parent of the ctx (so we can use graft
    # to copy commits), and 2) informs update that the incoming changes are
    # newer than the destination so it doesn't prompt about "remote changed foo
    # which local deleted".
    if is_crossrepo:
        mergeancestor = False
    else:
        mergeancestor = to_repo.changelog.isancestor(to_repo["."].node(), ctx.node())

    stats = merge(
        to_repo,
        ctx,
        force=True,
        ancestor=pctx,
        mergeancestor=mergeancestor and not ctx.manifest().hasgrafts(),
        labels=labels,
        from_repo=from_repo,
    )

    pother = nullid
    parents = ctx.parents()
    if keepparent and len(parents) == 2 and pctx in parents:
        parents.remove(pctx)
        pother = parents[0].node()

    with to_repo.dirstate.parentchange():
        to_repo.setparents(to_repo["."].node(), pother)
        to_repo.dirstate.write(to_repo.currenttransaction())
        if not is_crossrepo:
            # fix up dirstate for copies and renames
            copies.duplicatecopies(to_repo, to_repo[None], ctx.rev(), pctx.rev())
    return stats


def _gethex(ctx):
    # for workingctx return p1 hex
    return ctx.hex() if ctx.node() and ctx.hex() != wdirhex else ctx.p1().hex()


def try_conclude_merge_state(repo):
    ms = mergestate.read(repo)
    # Are conflicts resolved?
    # If so, exit the updatemergestate.
    if not ms.active() or ms.unresolvedcount() == 0:
        ms.reset()
        repo.localvfs.tryunlink("updatemergestate")
