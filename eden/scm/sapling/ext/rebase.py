# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# rebase.py - rebasing feature for mercurial
#
# Copyright 2008 Stefano Tortarolo <stefano.tortarolo at gmail dot com>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.

"""command to move sets of revisions to a different ancestor

This extension lets you rebase changesets in an existing @Product@
repository.

For more information:
https://mercurial-scm.org/wiki/RebaseExtension
"""

import errno
from collections.abc import MutableMapping, MutableSet

import bindings
from bindings import checkout as nativecheckout
from sapling import (
    bookmarks,
    cmdutil,
    context,
    copies,
    dirstateguard,
    error,
    extensions,
    hg,
    i18n,
    identity,
    lock,
    match as matchmod,
    merge as mergemod,
    mergeutil,
    mutation,
    perftrace,
    phases,
    progress,
    registrar,
    revsetlang,
    rewriteutil,
    scmutil,
    simplemerge,
    smartset,
    templatefilters,
    util,
    visibility,
)
from sapling.i18n import _
from sapling.node import hex, nullid, nullrev, short
from sapling.utils import subtreeutil

release = lock.release

# The following constants are used throughout the rebase module. The ordering of
# their values must be maintained.

# Indicates that a revision needs to be rebased
revtodo = -1
revtodostr = "-1"

# legacy revstates no longer needed in current code
# -2: nullmerge, -3: revignored, -4: revprecursor, -5: revpruned
legacystates = {"-2", "-3", "-4", "-5"}

cmdtable = {}
command = registrar.command(cmdtable)
# Note for extension authors: ONLY specify testedwith = 'ships-with-hg-core' for
# extensions which SHIP WITH MERCURIAL. Non-mainline extensions should
# be specifying the version(s) of Mercurial they are tested with, or
# leave the attribute unspecified.
testedwith = "ships-with-hg-core"

colortable = {"rebase.manual.update": "yellow"}


def _savegraft(ctx, extra) -> None:
    s = ctx.extra().get("source", None)
    if s is not None:
        extra["source"] = s
    s = ctx.extra().get("intermediate-source", None)
    if s is not None:
        extra["intermediate-source"] = s


def _reproduciblecommits(ctx, extra) -> None:
    # If we want reproducible commits, we need stable dates. The mutation date in
    # extras is always "now", which affects the commit hash. Nothing uses this
    # extra, so should be okay to skip.
    extra.pop("mutdate", None)


def _makeextrafn(copiers):
    """make an extrafn out of the given copy-functions.

    A copy function takes a context and an extra dict, and mutates the
    extra dict as needed based on the given context.
    """

    def extrafn(ctx, extra):
        for c in copiers:
            c(ctx, extra)

    return extrafn


def _ctxdesc(ctx) -> str:
    """short description for a context"""
    desc = '%s "%s"' % (ctx, ctx.description().split("\n", 1)[0])
    repo = ctx.repo()
    names = []
    for nsname, ns in repo.names.items():
        names.extend(ns.names(repo, ctx.node()))
    if names:
        desc += " (%s)" % " ".join(names)
    return desc


class RevToRevCompatMap(MutableMapping):
    """{rev: rev} mapping backed by {node: node}

    This allows changelog to reassign node to different revs, which can happen
    with segmented changelog backend, for example:

        with repo.transaction(...):
            node = repo.commitctx(...)      # dag change is buffered in memory
            rev1 = repo.changelog.rev(node) # could be 1<<56.
            # transaction close - write to disk and might reassign revs,
            # especially when devel.segmented-changelog-rev-compat=true
        rev2 = repo.changelog.rev(node)  # could be 0, different from rev1

    Ideally all callsites migrate to use nodes directly. However that involves
    too many places to migrate confidently without a strict type checker.
    """

    def __init__(self, repo, state=None):
        self.repo = repo
        self.node2node = {}
        if state:
            for k, v in state.items():
                self[k] = v

    def __setitem__(self, rev_key, rev_value):
        node_key = self.repo.changelog.node(rev_key)
        node_value = self.repo.changelog.node(rev_value)
        self.node2node[node_key] = node_value
        self._invalidate()

    def __getitem__(self, rev):
        return self._rev2rev.__getitem__(rev)

    def __delitem__(self, rev):
        node = self.repo.changelog.node(rev)
        del self.node2node[node]
        self._invalidate()

    def __iter__(self):
        return iter(self._rev2rev)

    def __len__(self):
        return len(self.node2node)

    @util.propertycache
    def _rev2rev(self):
        rev = self.repo.changelog.rev
        return {rev(k): rev(v) for k, v in self.node2node.items()}

    def _invalidate(self):
        self.__dict__.pop("_rev2rev", None)


class RevCompatSet(MutableSet):
    """see also RevToRevCompatMap. {rev} backed by {node}"""

    def __init__(self, repo, revs=None):
        self.repo = repo
        self.nodes = set()
        if revs:
            for r in revs:
                self.add(r)

    def __contains__(self, rev):
        return rev in self._revs

    def __iter__(self):
        return iter(self._revs)

    def __len__(self):
        return len(self.nodes)

    def add(self, rev):
        node = self.repo.changelog.node(rev)
        self.nodes.add(node)
        self._invalidate()

    def discard(self, rev):
        node = self.repo.changelog.node(rev)
        self.nodes.discard(node)
        self._invalidate()

    @util.propertycache
    def _revs(self):
        rev = self.repo.changelog.rev
        return {rev(n) for n in self.nodes}

    def _invalidate(self):
        self.__dict__.pop("_revs", None)


class rebaseruntime:
    """This class is a container for rebase runtime state"""

    def __init__(self, repo, ui, templ, inmemory=False, opts=None):
        if opts is None:
            opts = {}

        # prepared: whether we have rebasestate prepared or not. Currently it
        # decides whether "self.repo" is unfiltered or not.
        # The rebasestate has explicit hash to hash instructions not depending
        # on visibility. If rebasestate exists (in-memory or on-disk), use
        # unfiltered repo to avoid visibility issues.
        # Before knowing rebasestate (i.e. when starting a new rebase (not
        # --continue or --abort)), the original repo should be used so
        # visibility-dependent revsets are correct.
        self.prepared = False
        self._repo = repo

        self.ui = ui
        self.templ = templ
        self.opts = opts
        self.originalwd = None
        self.external = nullrev
        # Mapping between the old revision id and either what is the new rebased
        # revision or what needs to be done with the old revision. The state
        # dict will be what contains most of the rebase progress state.
        self.state = RevToRevCompatMap(repo)
        self.activebookmark = None
        self.destmap = RevToRevCompatMap(repo)
        self.skipped = RevCompatSet(repo)

        self.collapsef = opts.get("collapse", False)
        self.collapsemsg = cmdutil.logmessage(repo, opts)
        self.contf = opts.get("continue", False)
        self.date = opts.get("date", None)

        e = opts.get("extrafn")  # internal, used by e.g. hgsubversion
        self.extrafns = [_savegraft]

        if repo.ui.configbool("rebase", "reproducible-commits"):
            self.extrafns.append(_reproduciblecommits)

        if e:
            self.extrafns = [e]

        self.keepf = opts.get("keep", False)
        self.obsoletenotrebased = RevToRevCompatMap(repo)
        self.obsoletewithoutsuccessorindestination = RevCompatSet(repo)
        self.inmemory = inmemory

    @property
    def repo(self):
        if self.prepared:
            return self._repo
        else:
            return self._repo

    def storestatus(self, tr=None):
        """Store the current status to allow recovery"""
        # An in-memory rebase runs in one transaction that is rolled back
        # when it fails, so there is never anything to continue or abort.
        # A state file would only make later commands refuse with "rebase
        # in progress".
        if self.inmemory:
            return
        if tr:
            tr.addfilegenerator(
                "rebasestate", ("rebasestate",), self._writestatus, location="local"
            )
        else:
            with self.repo.localvfs("rebasestate", "w") as f:
                self._writestatus(f)

    def _writestatus(self, f):
        repo = self.repo
        f.write((repo[self.originalwd].hex() + "\n").encode())
        # was "dest". we now write dest per src root below.
        f.write(b"\n")
        f.write((repo[self.external].hex() + "\n").encode())
        f.write(b"%d\n" % int(self.collapsef))
        f.write(b"%d\n" % int(self.keepf))
        f.write(b"0\n")  # used to be the "keepbranches" flag.
        activebookmark = b""
        if self.activebookmark:
            activebookmark = self.activebookmark.encode()
        f.write(b"%s\n" % activebookmark)
        destmap = self.destmap.node2node
        for d, v in self.state.node2node.items():
            destnode = hex(destmap[d])
            f.write(("%s:%s:%s\n" % (hex(d), hex(v), destnode)).encode())
        repo.ui.debug("rebase status stored\n")

    def restorestatus(self):
        """Restore a previously stored status"""
        self.prepared = True
        repo = self.repo
        legacydest = None
        collapse = False
        external = nullrev
        activebookmark = None
        state = {}
        destmap = {}
        originalwd = None

        try:
            f = repo.localvfs("rebasestate")
            for i, l in enumerate(f.read().decode().splitlines()):
                if i == 0:
                    originalwd = repo[l].rev()
                elif i == 1:
                    # this line should be empty in newer version. but legacy
                    # clients may still use it
                    if l:
                        legacydest = repo[l].rev()
                elif i == 2:
                    external = repo[l].rev()
                elif i == 3:
                    collapse = bool(int(l))
                elif i == 4:
                    keep = bool(int(l))
                elif i == 5:
                    # used to be the "keepbranches" flag
                    pass
                elif i == 6 and not (len(l) == 81 and ":" in l):
                    # line 6 is a recent addition, so for backwards
                    # compatibility check that the line doesn't look like the
                    # oldrev:newrev lines
                    activebookmark = l
                else:
                    args = l.split(":")
                    oldrev = args[0]
                    newrev = args[1]
                    if newrev in legacystates:
                        continue
                    if len(args) > 2:
                        destnode = args[2]
                    else:
                        destnode = legacydest
                    destmap[repo[oldrev].rev()] = repo[destnode].rev()
                    if newrev in (nullid, revtodostr):
                        state[repo[oldrev].rev()] = revtodo
                        # Legacy compat special case
                    else:
                        state[repo[oldrev].rev()] = repo[newrev].rev()

        except IOError as err:
            if err.errno != errno.ENOENT:
                raise
            cmdutil.wrongtooltocontinue(repo, _("rebase"))

        if originalwd is None:
            dotdir = identity.default().dotdir()
            raise error.Abort(_("%s/rebasestate is incomplete") % dotdir)

        # recompute the predecessor map
        skipped = set()
        # recompute the set of skipped revs
        if not collapse:
            seen = set(destmap.values())
            for old, new in sorted(state.items()):
                if new != revtodo and new in seen:
                    skipped.add(old)
                seen.add(new)
        repo.ui.debug(
            "computed skipped revs: %s\n"
            % (" ".join(str(r) for r in sorted(skipped)) or None)
        )
        repo.ui.debug("rebase status resumed\n")

        self.originalwd = originalwd
        self.destmap = RevToRevCompatMap(repo, destmap)
        self.state = RevToRevCompatMap(repo, state)
        self.skipped = RevCompatSet(repo, skipped)
        self.collapsef = collapse
        self.keepf = keep
        self.external = external
        self.activebookmark = activebookmark

    def _handleskippingobsolete(self, obsoleterevs, destmap):
        """Compute structures necessary for skipping obsolete revisions

        obsoleterevs:   iterable of all obsolete revisions in rebaseset
        destmap:        {srcrev: destrev} destination revisions
        """
        self.obsoletenotrebased = RevToRevCompatMap(self.repo)
        if not self.ui.configbool("experimental", "rebaseskipobsolete"):
            return
        obsoleteset = set(obsoleterevs)
        (
            obsoletenotrebased,
            obsoletewithoutsuccessorindestination,
        ) = _computeobsoletenotrebased(self.repo, obsoleteset, destmap)
        self.obsoletenotrebased = RevToRevCompatMap(self.repo, obsoletenotrebased)
        self.obsoletewithoutsuccessorindestination = RevCompatSet(
            self.repo, obsoletewithoutsuccessorindestination
        )
        skippedset = set(self.obsoletenotrebased)
        skippedset.update(self.obsoletewithoutsuccessorindestination)
        if not mutation.enabled(self.repo):
            _checkobsrebase(self.repo, self.ui, obsoleteset, skippedset)

    def _prepareabortcontinueorquit(self, isabort, isquit):
        try:
            self.restorestatus()
            if self.collapsef:
                self.collapsemsg = restorecollapsemsg(self.repo, isabort)
        except error.RepoLookupError:
            if isabort:
                clearstatus(self.repo)
                clearcollapsemsg(self.repo)
                self.repo.ui.warn(
                    _(
                        "rebase aborted (no revision is removed,"
                        " only broken state is cleared)\n"
                    )
                )
                return 0
            else:
                msg = _("cannot continue inconsistent rebase")
                hint = _('use "@prog@ rebase --abort" to clear broken state')
                raise error.Abort(msg, hint=hint)
        if isabort:
            return abort(
                self.repo,
                self.originalwd,
                self.destmap,
                self.state,
                activebookmark=self.activebookmark,
            )
        if isquit:
            return quit_rebase(self.repo, activebookmark=self.activebookmark)

    def _preparenewrebase(self, destmap):
        if not destmap:
            return 0

        rebaseset = destmap.keys()
        allowunstable = visibility.tracking(self.repo)
        if not (self.keepf or allowunstable) and self.repo.revs(
            "first(children(%ld) - %ld)", rebaseset, rebaseset
        ):
            raise error.Abort(
                _("can't remove original changesets with unrebased descendants"),
                hint=_("use --keep to keep original changesets"),
            )

        result = buildstate(self.repo, destmap, self.collapsef)

        if not result:
            # Empty state built, nothing to rebase
            self.ui.status(_("nothing to rebase\n"))
            return 0

        for root in self.repo.set("roots(%ld)", rebaseset):
            if not self.keepf and not root.mutable():
                raise error.Abort(
                    _("can't rebase public changeset %s") % root,
                    hint=_("see '@prog@ help phases' for details"),
                )

        (self.originalwd, destmap, state) = result
        self.destmap = RevToRevCompatMap(self.repo, destmap)
        self.state = RevToRevCompatMap(self.repo, state)
        if self.collapsef:
            dests = set(self.destmap.values())
            if len(dests) != 1:
                raise error.Abort(
                    _("--collapse does not work with multiple destinations")
                )
            destrev = next(iter(dests))
            destancestors = self.repo.changelog.ancestors([destrev], inclusive=True)
            self.external = externalparent(self.repo, self.state, destancestors)

        self.prepared = True
        self._logrebasesize(destmap)

    def _logrebasesize(self, destmap):
        """Log metrics about the rebase size and distance"""
        repo = self.repo

        # internal config: rebase.logsizemetrics
        if not repo.ui.configbool("rebase", "logsizemetrics", default=True):
            return

        # The code assumes the rebase source is roughly a linear stack within a
        # single feature branch, and there is only one destination. If that is not
        # the case, the distance might be not accurate.
        destrev = max(destmap.values())
        rebaseset = destmap.keys()
        commitcount = len(rebaseset)
        distance = len(
            repo.revs(
                "(%ld %% %d) + (%d %% %ld)", rebaseset, destrev, destrev, rebaseset
            )
        )
        # 'distance' includes the commits being rebased, so subtract them to get the
        # actual distance being traveled. Even though we log update_distance above,
        # a rebase may run multiple updates, so that value might be not be accurate.
        repo.ui.log(
            "rebase_size",
            rebase_commitcount=commitcount,
            rebase_distance=distance - commitcount,
        )

    def _assignworkingcopy(self):
        if self.inmemory:
            from sapling.context import overlayworkingctx

            self.wctx = overlayworkingctx(self.repo)
            self.repo.ui.debug("rebasing in-memory\n")
            msg = self.repo.ui.config("rebase", "experimental.inmemorywarning")
            if msg:
                self.repo.ui.warn(msg + "\n")
        else:
            self.wctx = self.repo[None]
            self.repo.ui.debug("rebasing on disk\n")
        self.repo.ui.log("rebase", rebase_imm_used=str(self.wctx.isinmemory()).lower())

    def _performrebase(self, tr):
        self._assignworkingcopy()
        repo, ui = self.repo, self.ui

        # Calculate self.obsoletenotrebased
        obsrevs = _filterobsoleterevs(self.repo, self.state)
        self._handleskippingobsolete(obsrevs, self.destmap)
        # A landed side of a conflict-free merge is not rewritten: the merge
        # keeps combining it until the stack is rebased past the landed
        # commit, which drops it. Rewriting it here would only make a
        # divergent copy of a public commit.
        for rev in self._noconflictlandedsides(obsrevs):
            del self.state[rev]
            del self.destmap[rev]
        if not self.keepf and not self.contf:
            # Rebase has its own obsolete skipping and divergence handling; only
            # run the shared precheck on revisions this operation will rewrite.
            precheckrevs = {
                rev for rev, state in self.state.items() if state == revtodo
            }
            precheckrevs.difference_update(self.obsoletenotrebased)
            if not mutation.allow_divergence(ui):
                precheckrevs.difference_update(
                    repo.revs(
                        "descendants(%ld)", self.obsoletewithoutsuccessorindestination
                    )
                )
            rewriteutil.precheck(
                repo,
                precheckrevs,
                "rebase",
                checkmerge=False,
            )
            if self.collapsef and self.collapsemsg:
                # Validate the collapse message before any commit is rewritten
                # so a rejection cannot leave an interrupted rebase behind.
                rewriteutil.precheckmessage(
                    repo,
                    [repo[rev] for rev in sorted(precheckrevs)],
                    self.collapsemsg,
                )
            rewriteutil.rebasedestcheck(repo, self.destmap)

        # Keep track of the active bookmarks in order to reset them later
        self.activebookmark = self.activebookmark or repo._activebookmark
        if self.activebookmark:
            bookmarks.deactivate(repo)

        # Store the state before we begin so users can run 'hg rebase --abort'
        # if we fail before the transaction closes.
        self.storestatus()

        if ui.configbool("rebase", "prefetch-trees", True):
            self._prefetch()

        cands = [k for k, v in self.state.items() if v == revtodo]
        total = len(cands)
        pos = 0
        with progress.bar(ui, _("rebasing"), _("changesets"), total) as prog:
            for subset in sortsource(self.destmap):
                pos = self._performrebasesubset(tr, subset, pos, prog)
        ui.note(_("rebase merging completed\n"))

    def _performrebasesubset(self, tr, subset, pos, prog):
        repo, ui = self.repo, self.ui
        sortedrevs = repo.revs("sort(%ld, -topo)", subset)
        allowdivergence = mutation.allow_divergence(self.ui)
        if not allowdivergence:
            sortedrevs -= repo.revs(
                "descendants(%ld) and not %ld",
                self.obsoletewithoutsuccessorindestination,
                self.obsoletewithoutsuccessorindestination,
            )
        for rev in sortedrevs:
            dest = self.destmap[rev]
            ctx = repo[rev]
            desc = _ctxdesc(ctx)
            if self.state[rev] == rev:
                ui.status(_("already rebased %s\n") % desc)
            elif (
                not allowdivergence
                and rev in self.obsoletewithoutsuccessorindestination
            ):
                msg = (
                    _(
                        "note: not rebasing %s and its descendants as "
                        "this would cause divergence\n"
                    )
                    % desc
                )
                repo.ui.status(msg)
                self.skipped.add(rev)
            elif rev in self.obsoletenotrebased:
                succ = self.obsoletenotrebased[rev]
                if succ is None:
                    msg = _("note: not rebasing %s, it has no successor\n") % desc
                else:
                    succdesc = _ctxdesc(repo[succ])
                    msg = _("note: not rebasing %s, already in destination as %s\n") % (
                        desc,
                        succdesc,
                    )
                repo.ui.status(msg)
                # Make clearrebased aware state[rev] is not a true successor
                self.skipped.add(rev)
                # Record rev as moved to its desired destination in self.state.
                # This helps bookmark and working parent movement.
                dest = max(
                    adjustdest(repo, rev, self.destmap, self.state, self.skipped)
                )
                self.state[rev] = dest
            elif self.state[rev] == revtodo:
                pos += 1
                prog.value = (pos, "%s" % (ctx))
                try:
                    if (
                        repo.ui.configbool("nativecheckout", "rebaseonenative")
                        and not self.collapsef
                        and not mergemod.is_noconflict_merge(ctx)
                    ):
                        self._performrebaseonenative(rev, ctx, desc, dest)
                    else:
                        self._performrebaseone(rev, ctx, desc, tr, dest)
                    inmemoryerror = None
                except error.InMemoryMergeConflictsError as e:
                    inmemoryerror = e

                # Do the fallback outside the except clause since Python 3 hides
                # any stack trace from errors inside except clauses, and instead
                # shows the original exception.
                if inmemoryerror is not None:
                    perftrace.traceflag("disk-fallback")
                    # in-memory merge doesn't support conflicts, so if we hit any, abort
                    # and re-run as an on-disk merge.
                    clearstatus(repo)
                    mergemod.mergestate.clean(repo)

                    pathstr = ", ".join(
                        i18n.limititems(inmemoryerror.paths, maxitems=3)
                    )
                    if (
                        inmemoryerror.type
                        == error.InMemoryMergeConflictsError.TYPE_FILE_CONFLICTS
                    ):
                        kindstr = _("hit merge conflicts")
                    else:
                        kindstr = _("artifact rebuild required")

                    if self.opts.get("noconflict"):
                        # internal config: rebase.noconflictmsg
                        msg = ui.config(
                            "rebase",
                            "noconflictmsg",
                            _("%s (in %s) and --noconflict passed; exiting"),
                        )
                        # Some commits might have been rebased. Still move
                        # their bookmarks.
                        clearrebased(
                            ui,
                            repo,
                            self.templ,
                            self.destmap,
                            self.state,
                            self.skipped,
                            None,
                            self.keepf,
                        )
                        raise error.AbortMergeToolError(msg % (kindstr, pathstr))
                    elif cmdutil.uncommittedchanges(repo):
                        raise error.UncommittedChangesAbort(
                            _(
                                "must use on-disk merge for this rebase (%s in %s), but you have working copy changes"
                            )
                            % (kindstr, pathstr),
                            hint=_("commit, revert, or shelve them"),
                        )
                    else:
                        ui.warn(
                            _("%s (in %s); switching to on-disk merge\n")
                            % (kindstr, pathstr)
                        )
                        cmdutil.bailifchanged(repo)
                        if (
                            ui.configbool("rebase", "close-tx-on-disk", True)
                            and tr
                            and tr.running()
                        ):
                            tr.close()
                            tr = None
                    ui.log(
                        "rebase",
                        rebase_imm_new_restart=str(True).lower(),
                        rebase_imm_restart=str(True).lower(),
                    )
                    self.inmemory = False
                    self._assignworkingcopy()
                    self._performrebaseone(rev, ctx, desc, tr, dest)
            else:
                ui.status(
                    _("already rebased %s as %s\n") % (desc, repo[self.state[rev]])
                )
        return pos

    def _performrebaseonenative(self, rev, ctx, desc, dest):
        repo, ui, opts = self.repo, self.ui, self.opts
        ui.status(_("rebasing %s\n") % desc)
        p1, p2, base = defineparents(
            repo, rev, self.destmap, self.state, self.skipped, self.obsoletenotrebased
        )
        p1ctx = repo[p1]
        basectx = repo[base]
        mergeresult = nativecheckout.mergeresult(
            ctx.manifest(), p1ctx.manifest(), basectx.manifest()
        )
        manifestbuilder = mergeresult.manifestbuilder()
        if manifestbuilder is None:
            raise error.InMemoryMergeConflictsError(
                "Native merge returned conflicts",
                error.InMemoryMergeConflictsError.TYPE_FILE_CONFLICTS,
                mergeresult.conflict_paths(),
            )

        resolved = _simplemerge(ui, basectx, ctx, p1ctx, manifestbuilder)

        commitmsg = ctx.description()
        extra = {"rebase_source": ctx.hex()}
        extra.update(subtreeutil.get_subtree_metadata(ctx.extra()))
        mutinfo = None
        if not self.keepf:
            mutop = "rebase"
            preds = [ctx.node()]
            mutinfo = mutation.record(repo, extra, preds, mutop)

        _makeextrafn(self.extrafns)(ctx, extra)

        loginfo = {"predecessors": ctx.hex(), "mutation": "rebase"}

        if "narrowheads" in repo.storerequirements:
            # with narrow-heads, phases.new-commit is meaningless
            overrides = {}
        else:
            destphase = max(ctx.phase(), phases.draft)
            overrides = {("phases", "new-commit"): destphase}
        with repo.ui.configoverride(overrides, "rebase"):
            commitmsg = _adjustrebasecopymessage(repo, commitmsg, self.keepf, ctx)
            # # Replicates the empty check in ``repo.commit``.
            # if wctx.isempty() and not repo.ui.configbool("ui", "allowemptycommit"):
            #     return None
            if self.date is None:
                date = ctx.date()
            else:
                date = self.date

            actions = manifestbuilder.pymerge_actions()

            def getfilectx(repo, memctx, path):
                data = resolved.get(path, None)
                if data is not None:
                    return context.overlayfilectx(
                        ctx[path],
                        datafunc=lambda: data,
                        copied=False,
                        ctx=memctx,
                    )

                action, (flags, _backup), _message = actions[path]
                if action == "r":
                    return None
                # Permission-only actions retain the destination's content.
                fctx = p1ctx[path] if action == "e" else ctx[path]
                return context.overlayfilectx(fctx, flags=flags, ctx=memctx)

            merging = p2 != nullrev
            editform = cmdutil.mergeeditform(merging, "rebase")
            editor = cmdutil.getcommiteditor(editform=editform, **opts)

            memctx = context.memctx(
                repo,
                parents=(repo[p1], repo[p2]),
                text=commitmsg,
                files=sorted(actions.keys() | resolved.keys()),
                filectxfn=getfilectx,
                date=date,
                extra=extra,
                user=ctx.user(),
                editor=editor,
                loginfo=loginfo,
                mutinfo=mutinfo,
            )
            newnode = repo.commitctx(memctx)

        self.state[rev] = repo[newnode].rev()
        ui.debug("rebased as %s\n" % short(newnode))

    def _performrebaseone(self, rev, ctx, desc, tr, dest):
        repo, ui, opts = self.repo, self.ui, self.opts
        ui.status(_("rebasing %s\n") % desc)
        p1, p2, base = defineparents(
            repo, rev, self.destmap, self.state, self.skipped, self.obsoletenotrebased
        )
        self.storestatus(tr=tr)

        if self.collapsef:
            storecollapsemsg(repo, self.collapsemsg)

        noconflictmerge = p2 != nullrev and mergemod.is_noconflict_merge(ctx)
        if noconflictmerge:
            # A merge of stale parents means nothing, so also follow parents
            # that were rewritten outside of this rebase (e.g. both parents
            # amended before restacking).
            p1, p2 = (self._noconflictparent(p) for p in (p1, p2))
            p1node, p2node = repo[p1].node(), repo[p2].node()
            if p1 == p2 or repo.changelog.isancestor(p1node, p2node):
                p1, p2 = p2, nullrev
            elif repo.changelog.isancestor(p2node, p1node):
                p2 = nullrev
        if p2 == nullrev and mergemod.is_noconflict_merge(ctx):
            # One parent is now an ancestor of the other, typically because
            # one of them landed and the stack moved past it. There is nothing
            # left to merge, so the merge is dropped instead of replayed.
            if not self.collapsef:
                ui.status(
                    _(
                        "note: dropping conflict-free merge %s: one parent is now an ancestor of the other\n"
                    )
                    % desc
                )
                self.skipped.add(rev)
            self.state[rev] = p1
            return
        if self.contf and len(repo.working_parent_nodes()) == 2:
            repo.ui.debug("resuming interrupted rebase\n")
        elif noconflictmerge:
            self._noconflictremerge(ctx, p1, p2)
        else:
            with ui.configoverride(
                {("ui", "forcemerge"): opts.get("tool", "")}, "rebase"
            ):
                stats = rebasenode(
                    repo,
                    rev,
                    p1,
                    base,
                    self.state,
                    self.collapsef,
                    dest,
                    wctx=self.wctx,
                )
                if stats and stats[3] > 0:
                    if self.wctx.isinmemory():
                        # This is a fallback in case the merge itself did not raise
                        # this exception (which, in general, it should).
                        raise error.InMemoryMergeConflictsError(
                            _("merge returned conflicts"),
                            type=error.InMemoryMergeConflictsError.TYPE_FILE_CONFLICTS,
                            # We don't have access to the paths here, so fake it:
                            paths=["%d files" % stats[3]],
                        )
                    else:
                        raise error.InterventionRequired(
                            _(
                                "unresolved conflicts (see @prog@ "
                                "resolve, then @prog@ rebase --continue)"
                            )
                        )
        if not self.collapsef:
            merging = p2 != nullrev
            editform = cmdutil.mergeeditform(merging, "rebase")
            if opts.get("edit"):
                editor = cmdutil.getcommiteditor(editform=editform, **opts)
            else:
                editor = None
            if self.wctx.isinmemory():
                newnode = concludememorynode(
                    repo,
                    rev,
                    p1,
                    p2,
                    self.keepf,
                    wctx=self.wctx,
                    extrafn=_makeextrafn(self.extrafns),
                    editor=editor,
                    date=self.date,
                    noconflictmerge=noconflictmerge,
                )
                mergemod.mergestate.clean(repo)
            else:
                newnode = concludenode(
                    repo,
                    rev,
                    p1,
                    p2,
                    self.keepf,
                    extrafn=_makeextrafn(self.extrafns),
                    editor=editor,
                    date=self.date,
                    noconflictmerge=noconflictmerge,
                )

            if newnode is None:
                # If it ended up being a no-op commit, then the normal
                # merge state clean-up path doesn't happen, so do it
                # here. Fix issue5494
                mergemod.mergestate.clean(repo)
        else:
            # Skip commit if we are collapsing
            if self.wctx.isinmemory():
                self.wctx.setbase(repo[p1])
            else:
                repo.setparents(repo[p1].node())
            newnode = None
        # Update the state
        if newnode is not None:
            self.state[rev] = repo[newnode].rev()
            ui.debug("rebased as %s\n" % short(newnode))
        else:
            if not self.collapsef:
                ui.warn(
                    _(
                        "note: not rebasing %s, its destination (rebasing onto) commit "
                        "already has all its changes\n"
                    )
                    % (ctx)
                )
                self.skipped.add(rev)
            self.state[rev] = p1
            ui.debug("next revision set to %s\n" % p1)

    def _noconflictparent(self, rev):
        """The unique visible draft successor of an obsolete rev, if there is
        one. A landed rev stands in for itself: merging its public successor
        in would pull that commit and everything below it into the stack."""
        if rev in self.state or not self.repo[rev].obsolete():
            return rev
        succs = list(
            self.repo.revs("heads(successors(%d) - obsolete() - public())", rev)
        )
        if len(succs) > 1:
            self._noconflictabort(
                _(
                    "cannot rebase conflict-free merge: parent %s has multiple "
                    "draft successors: %s"
                )
                % (self.repo[rev], ", ".join(str(self.repo[s]) for s in succs)),
                _("resolve the divergent versions of that parent before restacking"),
            )
        return succs[0] if succs else rev

    def _noconflictlandedsides(self, obsrevs):
        """The revs in `obsrevs` that landed and whose only children in the
        rebase are conflict-free merges."""
        repo = self.repo
        result = []
        for rev in obsrevs:
            if rev in self.obsoletenotrebased or (
                rev in self.obsoletewithoutsuccessorindestination
            ):
                continue
            if mergemod.landed_successor(repo, repo[rev]) is None:
                continue
            children = list(repo.revs("children(%d) & %ld", rev, list(self.state)))
            if children and all(
                mergemod.is_noconflict_merge(repo[c]) for c in children
            ):
                result.append(rev)
        return result

    def _noconflictremerge(self, ctx, p1, p2):
        """Rebase a conflict-free merge commit by merging its new parents
        again. The commit is the automatic merge of its parents, so replaying
        its diff against one parent (what rebasenode does) is not what it
        means; and if the parents no longer merge cleanly the rebase must
        stop rather than leave a conflicted merge behind.
        """
        repo, ui = self.repo, self.ui
        # A parent only becomes public here when one side was moved onto a
        # public commit while the other stayed behind. Merging the public
        # commit in would pull it and everything below it into the stack.
        for parent, other in ((p1, p2), (p2, p1)):
            if repo[parent].ispublic():
                self._noconflictabort(
                    _("cannot rebase conflict-free merge %s: %s is public")
                    % (ctx, repo[parent]),
                    _(
                        "rebase the whole stack instead, e.g. '@prog@ rebase -s %s -d %s'"
                    )
                    % (repo[other], repo[parent]),
                )
        labels = ["dest", "source"]
        try:
            # In memory this is the whole rebase of the commit; on disk it is
            # a check that runs before the working copy is touched, so a
            # conflict never leaves a half-done merge behind.
            mergemod.merge_in_memory(
                repo,
                repo[p2].node(),
                force=True,
                labels=labels,
                basectx=repo[p1],
                wctx=self.wctx if self.wctx.isinmemory() else None,
            )
        except error.InMemoryMergeConflictsError as e:
            paths = sorted(set(e.paths))
            pathstr = ", ".join(i18n.limititems(paths, maxitems=3)) or str(e)
            if self.opts.get("noconflict"):
                # Reported like any other conflict under --noconflict (amend's
                # automatic restack relies on the message config), and the
                # rebase is rolled back.
                msg = ui.config(
                    "rebase",
                    "noconflictmsg",
                    _("%s (in %s) and --noconflict passed; exiting"),
                )
                kindstr = _("merging the parents of %s again would conflict") % ctx
                raise error.AbortMergeToolError(msg % (kindstr, pathstr))
            self._noconflictabort(
                _(
                    "cannot rebase conflict-free merge %s: merging %s and %s "
                    "would have conflicts in %d file(s):\n %s"
                )
                % (ctx, repo[p1], repo[p2], len(paths), "\n ".join(paths)),
                _(
                    "amend one of the parents so they merge cleanly, then run "
                    "'@prog@ rebase --restack'"
                ),
            )
        if not self.wctx.isinmemory():
            if repo["."].rev() != p1:
                mergemod.goto(repo, p1, force=True)
                repo.dirstate.write(repo.currenttransaction())
            stats = mergemod.merge(repo, p2, force=True, labels=labels)
            if stats[3] > 0:
                # The in-memory merge above succeeded, so this is not expected.
                self._noconflictabort(
                    _("cannot rebase conflict-free merge %s: %d unresolved files")
                    % (ctx, stats[3]),
                    _("run '@prog@ goto --clean .' to discard the partial merge"),
                )

    def _noconflictabort(self, msg, hint):
        """Abort rebasing a conflict-free merge. An in-memory rebase is
        rolled back with its transaction. On disk the commits rebased so far
        are kept, as after 'rebase --quit', so that a restack picks up from
        them once a parent is fixed."""
        if not self.wctx.isinmemory():
            repo = self.repo
            if repo.currenttransaction() is None and any(
                v > 0 for v in self.state.values()
            ):
                # Finish the run for the commits already rewritten: move
                # their bookmarks and hide their predecessors. The rest of
                # the stack then sits on obsolete parents, which is exactly
                # what a restack picks up.
                clearrebased(
                    self.ui,
                    repo,
                    None,
                    self.destmap,
                    self.state,
                    self.skipped,
                    keepf=self.keepf,
                )
                hint = _("the commits rebased so far are kept; %s") % hint
            clearstatus(repo)
            clearcollapsemsg(repo)
        raise error.Abort(msg, hint=hint)

    def _finishrebase(self):
        repo, ui, opts = self.repo, self.ui, self.opts
        if self.collapsef:
            p1, p2, _base = defineparents(
                repo,
                min(self.state),
                self.destmap,
                self.state,
                self.skipped,
                self.obsoletenotrebased,
            )
            editopt = opts.get("edit")
            editform = "rebase.collapse"
            if self.collapsemsg:
                commitmsg = self.collapsemsg
            else:
                commitmsg = "Collapsed revision"
                for rebased in sorted(self.state):
                    if rebased not in self.skipped:
                        commitmsg += "\n* %s" % repo[rebased].description()
                editopt = True
            preds = []
            for rebased in sorted(self.state):
                if rebased not in self.skipped:
                    preds.append(repo[rebased])
            editor = cmdutil.getcommiteditor(edit=editopt, editform=editform)
            revtoreuse = max(self.state)

            dsguard = None
            if self.inmemory:
                newnode = concludememorynode(
                    repo,
                    revtoreuse,
                    p1,
                    self.external,
                    self.keepf,
                    commitmsg=commitmsg,
                    extrafn=_makeextrafn(self.extrafns),
                    editor=editor,
                    date=self.date,
                    wctx=self.wctx,
                    preds=preds,
                )
            else:
                if ui.configbool("rebase", "singletransaction"):
                    dsguard = dirstateguard.dirstateguard(repo, "rebase")
                with util.acceptintervention(dsguard):
                    newnode = concludenode(
                        repo,
                        revtoreuse,
                        p1,
                        self.external,
                        self.keepf,
                        commitmsg=commitmsg,
                        extrafn=_makeextrafn(self.extrafns),
                        editor=editor,
                        date=self.date,
                        preds=preds,
                    )
            if newnode is not None:
                newrev = repo[newnode].rev()
                for oldrev in self.state.keys():
                    self.state[oldrev] = newrev

        # restore original working directory
        # (we do this before stripping)
        newwd = self.state.get(self.originalwd, self.originalwd)
        if newwd < 0:
            # original directory is a parent of rebase set root or ignored
            newwd = self.originalwd

        if newwd not in [c.rev() for c in repo[None].parents()]:
            ui.note(_("update back to initial working directory parent\n"))
            try:
                hg.updaterepo(repo, newwd, False)
            except error.UpdateAbort:
                if self.inmemory:
                    # Print a nice message rather than aborting/restarting.
                    newctx = repo[newwd]
                    firstline = templatefilters.firstline(newctx.description())
                    ui.warn(
                        _(
                            "important: run `@prog@ up %s` to get the new "
                            'version of your current commit ("%s")\n'
                            "(this was not done automatically because you "
                            "made working copy changes during the "
                            "rebase)\n"
                        )
                        % (short(newctx.node()), firstline),
                        label="rebase.manual.update",
                    )
                else:
                    raise  # Keep old behavior

        # Ensure p2 and merge state are cleared. Normally concludenode handles
        # this per-commit, but if all revisions were skipped (e.g. detected as
        # "already in destination" during --continue), concludenode is never
        # called and p2 from the interrupted merge would leak out.
        # Only do this for on-disk rebases — in-memory rebases never touch
        # the dirstate, so we shouldn't clear a pre-existing p2.
        if not self.inmemory:
            repo.setparents(repo[None].p1().node())
            mergemod.mergestate.clean(repo)

        collapsedas = None
        if not self.keepf:
            if self.collapsef:
                collapsedas = newnode
        clearrebased(
            ui,
            repo,
            self.templ,
            self.destmap,
            self.state,
            self.skipped,
            collapsedas,
            self.keepf,
        )

        clearstatus(repo)
        clearcollapsemsg(repo)

        ui.note(_("rebase completed\n"))
        util.unlinkpath(repo.sjoin("undo"), ignoremissing=True)
        if self.skipped:
            skippedlen = len(self.skipped)
            ui.note(_("%d revisions have been skipped\n") % skippedlen)

        if (
            self.activebookmark
            and self.activebookmark in repo._bookmarks
            and repo["."].node() == repo._bookmarks[self.activebookmark]
        ):
            bookmarks.activate(repo, self.activebookmark)

    @perftrace.tracefunc("Rebase Prefetch")
    def _prefetch(self):
        repo = self.repo

        # Only remotefilelog repos fetch file content lazily; other formats already
        # have every file locally.
        if "remotefilelog" not in repo.requirements:
            return

        # Collect list of relevant commits: source commits, their parents, and dest commits.
        source_nodes = bindings.dag.nameset(self.destmap.node2node.keys())
        tofetch = (
            source_nodes
            + repo.changelog.dag.parents(source_nodes)
            + bindings.dag.nameset(self.destmap.node2node.values())
        )

        # Batch lazy DAG resolutions.
        repo.changelog.filternodes(tofetch)

        # Batch fetch commit text (contains the root tree node and file list).
        repo.changelog.inner.getcommitrawtextlist(tofetch)

        if hasattr(repo, "sparsematch"):
            matcher = repo.sparsematch()
        else:
            matcher = matchmod.always(repo.root, "")

        manifest_pairs = self._mergeinputpairs(
            self._sourcestacks(source_nodes), matcher
        )
        if not manifest_pairs:
            return

        stats = bindings.filewalk.prefetchdiffs(
            repo._rsrepo, manifest_pairs, matcher, self.ui._rcfg
        )
        perftrace.tracevalue("Prefetched merge inputs (local)", stats["local"])
        perftrace.tracevalue("Prefetched merge inputs (remote)", stats["remote"])

    def _sourcestacks(self, source_nodes):
        """Group source commits by the root reached through first parents that
        are themselves being rebased.

        Replay puts every non-root commit on its rebased parent (see
        `adjustdest`), so only a root's destination decides what its stack merges
        against. Returns a list of (root context, stack contexts) pairs.
        """
        repo = self.repo
        root_of = {}
        stacks = {}
        # dag.sort is topologically descending; reversed, parents come first.
        for node in reversed(list(repo.changelog.dag.sort(source_nodes))):
            ctx = repo[node]
            root = root_of.get(ctx.p1().node(), node)
            root_of[node] = root
            stacks.setdefault(root, []).append(ctx)
        return [(repo[root], contexts) for root, contexts in stacks.items()]

    def _mergeinputpairs(self, stacks, matcher):
        """Return (manifest, base manifest, matcher) triples whose modified entries
        are the three-way merge inputs replay needs.

        Each root's destination is compared with the root's parent over the paths
        its stack changed. Only paths modified on both sides are merged during
        replay; every other path takes one side's entry without reading content.
        Paths added on both sides have no base, so the native merge hands them to
        the on-disk fallback and nothing is prefetched for them either.
        """
        repo = self.repo
        groups = []
        diff_pairs = []
        group_of_pair = {}
        for root, stack_contexts in stacks:
            candidate_files = {
                path
                for ctx in stack_contexts
                for path in (ctx.files() or [])
                if matcher(path)
            }
            if not candidate_files:
                continue
            candidate_matcher = scmutil.matchfiles(repo, candidate_files)
            destination_manifest = repo[self.destmap.node2node[root.node()]].manifest()
            ancestor_manifest = root.p1().manifest()
            # The source pairs contribute no merge paths, but diffing them here
            # prefetches the trees replay walks even when nothing needs merging.
            diff_pairs.extend(
                (ctx.manifest(), ctx.p1().manifest(), candidate_matcher)
                for ctx in stack_contexts
            )
            group_of_pair[len(diff_pairs)] = len(groups)
            diff_pairs.append(
                (destination_manifest, ancestor_manifest, candidate_matcher)
            )
            groups.append((stack_contexts, destination_manifest, ancestor_manifest))
        if not diff_pairs:
            return []

        merge_files = [set() for _ in groups]
        for entry in bindings.manifest.diff_manifests(diff_pairs):
            index, path, (destination_filenode, _), (ancestor_filenode, _) = entry
            group = group_of_pair.get(index)
            if group is not None and destination_filenode and ancestor_filenode:
                merge_files[group].add(path)

        # Merge commits are paired with p1 only, so paths only reachable through
        # p2 are fetched lazily during replay.
        manifest_pairs = []
        for (stack_contexts, destination_manifest, ancestor_manifest), files in zip(
            groups, merge_files
        ):
            if not files:
                continue
            merge_matcher = scmutil.matchfiles(repo, files)
            manifest_pairs.extend(
                (ctx.manifest(), ctx.p1().manifest(), merge_matcher)
                for ctx in stack_contexts
            )
            manifest_pairs.append(
                (destination_manifest, ancestor_manifest, merge_matcher)
            )
        return manifest_pairs


def _simplemerge(ui, basectx, ctx, p1ctx, manifestbuilder):
    from ..simplemerge import Merge3Text, render_minimized

    conflicts = []
    resolved = {}
    for file in manifestbuilder.modifiedconflicts():
        basetext = basectx[file].data()
        localtext = ctx[file].data()
        othertext = p1ctx[file].data()

        m3 = Merge3Text(basetext, localtext, othertext, ui=ui)

        merged_lines, conflictscount = render_minimized(m3)
        merged = b"".join(merged_lines)

        # Suppress message if merged result is the same as local contents.
        if merged != localtext:
            ui.status(_("merging %s\n") % file)

        if conflictscount:
            conflicts.append(file)
        else:
            resolved[file] = merged

    if conflicts:
        raise error.InMemoryMergeConflictsError(
            _("textural merge returned conflicts"),
            error.InMemoryMergeConflictsError.TYPE_FILE_CONFLICTS,
            conflicts,
        )

    return resolved


@command(
    "rebase",
    [
        (
            "s",
            "source",
            "",
            _("rebase the specified commit and descendants"),
            _("REV"),
        ),
        (
            "b",
            "base",
            "",
            _("rebase everything from branching point of specified commit"),
            _("REV"),
        ),
        ("r", "rev", [], _("rebase these revisions"), _("REV")),
        ("d", "dest", [], _("rebase onto the specified revision"), _("REV")),
        ("", "collapse", False, _("collapse the rebased commits")),
        ("m", "message", "", _("use text as collapse commit message"), _("TEXT")),
        ("e", "edit", False, _("invoke editor on commit messages")),
        ("l", "logfile", "", _("read collapse commit message from file"), _("FILE")),
        ("k", "keep", False, _("keep original commits")),
        ("D", "detach", False, _("(DEPRECATED)")),
        ("i", "interactive", False, _("(DEPRECATED)")),
        ("t", "tool", "", _("specify merge tool")),
        ("c", "continue", False, _("continue an interrupted rebase")),
        ("a", "abort", False, _("abort an interrupted rebase")),
        (
            "",
            "quit",
            False,
            _("quit an interrupted rebase and keep the already rebased commits"),
        ),
        (
            "",
            "noconflict",
            False,
            _("cancel the rebase if there are conflicts (EXPERIMENTAL)"),
        ),
    ]
    + cmdutil.formatteropts,
    _("[-s REV | -b REV] [-d REV] [OPTION]"),
    cmdtemplate=True,
    legacyaliases=["reb", "reba", "rebas"],
)
def rebase(ui, repo, templ=None, **opts):
    """move commits from one location to another

    Move commits from one part of the commit graph to another. This
    behavior is achieved by creating a copy of the commit at the
    destination and hiding the original commit.

    Use ``-k/--keep`` to skip the hiding and keep the original commits visible.

    If the commits being rebased have bookmarks, rebase moves the bookmarks
    onto the new versions of the commits. Bookmarks are moved even if ``--keep``
    is specified.

    Public commits cannot be rebased unless you use the ``--keep`` option
    to copy them.

    Use the following options to select the commits you want to rebase:

      1. ``-r/--rev`` to explicitly select commits

      2. ``-s/--source`` to select a root commit and include all of its
         descendants

      3. ``-b/--base`` to select a commit and its ancestors and descendants

    If no option is specified to select commits, ``-b .`` is used by default.

      .. container:: verbose

        If ``--source`` or ``--rev`` is used, special names ``SRC`` and ``ALLSRC``
        can be used in ``--dest``. Destination would be calculated per source
        revision with ``SRC`` substituted by that single source revision and
        ``ALLSRC`` substituted by all source revisions.

    If multiple ``--rev``s are specified, they can be paired with multiple
    ``--dest``s. For example::

        rebase -r A+B -d X -r C::E -d Y

    will rebase ``A+B`` to ``X``, and rebase ``C::E`` to ``Y``.

    If commits that you are rebasing consist entirely of changes that are
    already present in the destination, those commits are not moved (in
    other words, they are rebased out).

    Sometimes conflicts can occur when you rebase. When this happens, by
    default, @Product@ launches an editor for every conflict. Conflict markers
    are inserted into affected files, like::

        <<<<
        dest
        ====
        source
        >>>>

    To fix the conflicts, for each file, remove the markers and replace the
    whole block of code with the correctly merged code.

    If you close the editor without resolving the conflict, the rebase is
    interrupted and you are returned to the command line. At this point, you
    can resolve conflicts in manual resolution mode. See :prog:`help resolve` for
    details.

    After manually resolving conflicts, resume the rebase with
    :prog:`rebase --continue`. If you are not able to successfully
    resolve all conflicts, run :prog:`rebase --abort` to abort the
    entire rebase; or run :prog:`rebase --quit` to quit the interrupted
    rebase state and keep the already rebased commits.

    Alternatively, you can use a custom merge tool to automate conflict
    resolution. To specify a custom merge tool, use the ``--tool`` flag. See
    :prog:`help merge-tools` for a list of available tools and for information
    about configuring the default merge behavior.

    .. container:: verbose

      Examples:

      - Move a single commit to master::

          @prog@ rebase -r 5f493448 -d master

      - Move a commit and all its descendants to another part of the commit graph::

          @prog@ rebase --source c0c3 --dest 4cf9

      - Rebase everything on a local branch marked by a bookmark to master::

          @prog@ rebase --base myfeature --dest master

      - Rebase orphaned commits onto the latest version of their parents::

          @prog@ rebase --restack

      Configuration Options:

      By default, rebase will close the transaction after each commit. For
      performance purposes, you can configure rebase to use a single transaction
      across the entire rebase. WARNING: This setting introduces a significant
      risk of losing the work you've done in a rebase if the rebase aborts
      unexpectedly::

        [rebase]
        singletransaction = True

      By default, rebase writes to the working copy, but you can configure it
      to run in-memory for better performance, and to allow it to run if the
      current checkout is dirty::

        [rebase]
        experimental.inmemory = True

      It will also print a configurable warning::

        [rebase]
        experimental.inmemorywarning = Using experimental in-memory rebase

      If the destination is based on older versions of commits that the
      rebased commits are based on, rebase warns that their newer changes will
      be lost (agents abort instead). Set ``warn``, ``prompt`` or ``abort`` to
      choose the behavior; any other value disables the check::

        [rebase]
        obsolete-dest-mode = warn

    Returns 0 on success (also when nothing to rebase), 1 if there are
    unresolved conflicts.

    """

    if not opts.get("date"):
        opts["date"] = _defaultdate(ui)

    if not (
        opts.get("continue")
        or opts.get("abort")
        or opts.get("restack")
        or opts.get("quit")
    ):
        # 'hg rebase' w/o args should do nothing
        dests = opts.get("dest")
        if not dests:
            raise error.Abort("you must specify a destination (-d) for the rebase")

        if not isinstance(dests, list):
            dests = [dests]
        for dest in dests:
            bookmarks.checkagentpreferredtarget(repo, dest, "rebase -d")

        # 'hg rebase' can fast-forward bookmark
        prev = repo["."]
        dests = opts.get("dest")

        # Only fast-forward the bookmark if no source nodes were explicitly
        # specified.
        if not (opts.get("base") or opts.get("source") or opts.get("rev")):
            if len(dests) == 1 and dests[0] != prev:
                dest = scmutil.revsingle(repo, dests[0])
                common = dest.ancestor(prev)
                if prev == common and dest != prev:
                    activebookmark = repo._activebookmark
                    result = hg.updatetotally(ui, repo, dest.node(), activebookmark)
                    if activebookmark:
                        with repo.wlock():
                            bookmarks.update(repo, [prev.node()], dest.node())
                    ui.status(_("nothing to rebase - fast-forwarded to %s\n") % dest)
                    return result

    inmemory = ui.configbool("rebase", "experimental.inmemory")

    # Check for conditions that disable in-memory merge if it was requested.
    if inmemory:
        whynotimm = None

        # in-memory rebase is not compatible with resuming rebases.
        if opts.get("continue") or opts.get("abort") or opts.get("quit"):
            whynotimm = "--continue, --abort or --quit passed"

        # in-memory rebase cannot currently run within a parent transaction,
        # since the restarting logic will fail the entire transaction.
        elif repo.currenttransaction() is not None:
            whynotimm = "rebase run inside a transaction"
        elif opts.get("collapse", False):
            whynotimm = "--collapse"

        if whynotimm:
            ui.log(
                "rebase", "disabling IMM because: %s" % whynotimm, why_not_imm=whynotimm
            )
            inmemory = False

    if opts.get("noconflict") and not inmemory:
        raise error.Abort("--noconflict requires in-memory merge")

    rbsrt = rebaseruntime(repo, ui, templ, inmemory, opts)

    if rbsrt.inmemory:
        try:
            overrides = {
                # It's important to check for path conflicts with IMM to
                # prevent commits with path<>file conflicts from being created
                # (if you rebase on disk the filesystem prevents you from doing
                # this).
                ("experimental", "merge.checkpathconflicts"): True
            }
            with ui.configoverride(overrides):
                return _origrebase(ui, repo, rbsrt, **opts)
        except error.AbortMergeToolError as e:
            ui.status(_("%s\n") % e)
            clearstatus(repo)
            mergemod.mergestate.clean(repo)
            if repo.currenttransaction():
                repo.currenttransaction().abort()
    else:
        return _origrebase(ui, repo, rbsrt, **opts)


def _defaultdate(ui):
    if ui.configbool("tweakdefaults", "rebasekeepdate"):
        # We want to "keep" the source commit's date. We don't actually enable this
        # anywhere since things like ISL use commit date to infer "liveness", so we want
        # to bump it on rebase.
        return None

    if ui.configbool("rebase", "reproducible-commits"):
        # We want rebase to create reproducible commits, which in practice means dates
        # must be stable (so we want to maintain the commit's date).
        return None

    if stub := ui.config("devel", "default-date"):
        return stub

    return "%d %d" % util.makedate()


@perftrace.tracefunc("Rebase")
def _origrebase(ui, repo, rbsrt, **opts):
    with repo.wlock(), repo.lock(), simplemerge.managed_merge_resource(ui, repo.name):
        # Validate input and define rebasing points
        dests = opts.get("dest", [])
        # Compatible with old code that expects --dest as a single flag
        if dests is None:
            dests = []
        elif not isinstance(dests, list):
            dests = [dests]
        srcf = opts.get("source", None)
        basef = opts.get("base", None)
        revf = opts.get("rev", [])
        keepf = opts.get("keep", False)
        # search default destination in this space
        # used in the 'hg pull --rebase' case, see issue 5214.
        destspace = opts.get("_destspace")
        contf = opts.get("continue", False)
        abortf = opts.get("abort", False)
        quitf = opts.get("quit", False)
        if opts.get("interactive"):
            try:
                if extensions.find("histedit"):
                    enablehistedit = ""
            except KeyError:
                enablehistedit = " --config extensions.histedit="
            help = "hg%s help -e histedit" % enablehistedit
            msg = (
                _(
                    "interactive history editing is supported by the "
                    "'histedit' extension (see \"%s\")"
                )
                % help
            )
            raise error.Abort(msg)

        if rbsrt.collapsemsg and not rbsrt.collapsef:
            raise error.Abort(_("message can only be specified with collapse"))

        if contf or abortf or quitf:
            if (contf + abortf + quitf) > 1:
                raise error.Abort(
                    _("can only use one of the following: abort, continue or quit")
                )
            if rbsrt.collapsef:
                raise error.Abort(_("cannot use collapse with continue, abort or quit"))
            if srcf or basef or dests:
                raise error.Abort(
                    _("abort, continue and quit do not allow specifying revisions")
                )
            if (abortf or quitf) and opts.get("tool", False):
                ui.warn(_("tool option will be ignored\n"))
            if contf:
                ms = mergemod.mergestate.read(repo)
                mergeutil.checkunresolved(ms)

            retcode = rbsrt._prepareabortcontinueorquit(abortf, quitf)
            if retcode is not None:
                return retcode
        else:
            destmap = _definedestmap(
                ui,
                repo,
                rbsrt,
                dests,
                srcf,
                basef,
                revf,
                keepf=keepf,
                destspace=destspace,
            )
            retcode = rbsrt._preparenewrebase(destmap)
            if retcode is not None:
                return retcode

        tr = None
        dsguard = None

        singletr = ui.configbool("rebase", "singletransaction")
        # Always use single transaction mode during in-memory rebase. Single transaction
        # mode is a lot faster, and in-memory rebase doesn't really benefit from
        # per-commit transaction because it is all-or-nothing in nature up until the first
        # commit with conflicts (at which time we will switch out of single transaction
        # mode).
        if singletr or rbsrt.inmemory:
            tr = repo.transaction("rebase")

        # If `rebase.singletransaction` is enabled, wrap the entire operation in
        # one transaction here. Otherwise, transactions are obtained when
        # committing each node, which is slower but allows partial success.
        try:
            with util.acceptintervention(tr):
                # Same logic for the dirstate guard, except we don't create one when
                # rebasing in-memory (it's not needed).
                if singletr and not rbsrt.inmemory:
                    dsguard = dirstateguard.dirstateguard(repo, "rebase")
                with util.acceptintervention(dsguard):
                    try:
                        rbsrt._performrebase(tr)
                    except error.AbortMergeToolError:
                        # Above we run all in-memory rebases in single transaction mode.
                        # Emulate multi-transaction mode by committing transaction on
                        # --noconflict error (this saves commits that were rebased before we
                        # hit a conflict).
                        if not singletr:
                            tr.close()
                        raise
        except BaseException:
            # bookmarks.current is written outside the rebase transaction, so
            # the rollback does not restore the active bookmark.
            if rbsrt.inmemory and rbsrt.activebookmark in repo._bookmarks:
                bookmarks.activate(repo, rbsrt.activebookmark)
            raise

        rbsrt._finishrebase()


def _definedestmap(
    ui,
    repo,
    rbsrt,
    dests,
    srcf,
    basef,
    revf,
    keepf,
    destspace,
):
    """use revisions argument to define destmap {srcrev: destrev}"""
    if revf is None:
        revf = []

    destlen = len(dests)
    if destlen == 1:
        destf = dests[0]
    elif destlen == 0:
        raise error.Abort(_("rebase dest is required"))
    else:
        destf = None

        if srcf or basef:
            raise error.Abort(
                _("multiple --dest requires --rev, not --base or --source")
            )
        elif len(revf) != destlen:
            raise error.Abort(_("number of --dest does not match number of --rev"))

    # destspace is here to work around issues with `hg pull --rebase` see
    # issue5214 for details
    if srcf and basef:
        raise error.Abort(_("cannot specify both a source and a base"))
    if revf and basef:
        raise error.Abort(_("cannot specify both a revision and a base"))
    if revf and srcf:
        raise error.Abort(_("cannot specify both a revision and a source"))

    cmdutil.checkunfinished(repo)
    if not rbsrt.inmemory:
        cmdutil.bailifchanged(repo)

    dest = None
    destmap = None

    if revf:
        if destlen > 1:
            # Handles multiple --dest.
            assert destlen == len(revf)
            # Each rev resolves to a set. Their order should match dests.
            revs_list = [scmutil.revrange(repo, [r]) for r in revf]
            dest_list = [scmutil.revsingle(repo, r).rev() for r in dests]
            destmap = {}
            for rev_revs, dest_rev in zip(revs_list, dest_list):
                for rev in rev_revs:
                    destmap[rev] = dest_rev
            rebaseset = smartset.baseset(destmap, repo=repo)
        else:
            rebaseset = scmutil.revrange(repo, revf)
        if not rebaseset:
            ui.status(_('empty "rev" revision set - nothing to rebase\n'))
            return None
    elif srcf:
        src = scmutil.revrange(repo, [srcf])
        if not src:
            ui.status(_('empty "source" revision set - nothing to rebase\n'))
            return None
        rebaseset = repo.revs("(%ld)::", src)
        if not rebaseset:
            ui.status(_('"source" revision set is invisible - nothing to rebase\n'))
            ui.status(_("(hint: use '@prog@ unhide' to make commits visible first)\n"))
            return None
    else:
        base = scmutil.revrange(repo, [basef or "."])
        if not base:
            ui.status(_('empty "base" revision set - can\'t compute rebase set\n'))
            return None

        # --base does not support multiple destinations
        dest = scmutil.revsingle(repo, destf)

        rootnodes = []  # selected children of branching points
        bpbase = {}  # {branchingpoint: [origbase]}
        for b in base:  # group bases by branching points
            bp = repo.revs("ancestor(%d, %d)", b, dest).first()
            bpbase[bp] = bpbase.get(bp, []) + [b]
        if None in bpbase:
            # emulate the old behavior, showing "nothing to rebase" (a better
            # behavior may be abort with "cannot find branching point" error)
            bpbase.clear()
        tonodes = repo.changelog.tonodes
        for bp, bs in bpbase.items():  # calculate roots
            rootnodes += list(
                repo.dageval(lambda: children(tonodes([bp])) & ancestors(tonodes(bs)))
            )

        rebasenodes = repo.dageval(
            lambda: descendants(rootnodes) & (draft() + public())
        )
        if not keepf:
            rebasenodes -= repo.dageval(lambda: public())
        rebaseset = repo.changelog.torevset(rebasenodes)

        if not rebaseset:
            # transform to list because smartsets are not comparable to
            # lists. This should be improved to honor laziness of
            # smartset.
            if list(base) == [dest.rev()]:
                if basef:
                    ui.status(
                        _('nothing to rebase - %s is both "base" and destination\n')
                        % dest
                    )
                else:
                    ui.status(
                        _(
                            "nothing to rebase - working directory "
                            "parent is also destination\n"
                        )
                    )
            elif not repo.revs("%ld - ::%d", base, dest):
                if basef:
                    ui.status(
                        _(
                            'nothing to rebase - "base" %s is '
                            "already an ancestor of destination "
                            "%s\n"
                        )
                        % ("+".join(str(repo[r]) for r in base), dest)
                    )
                else:
                    ui.status(
                        _(
                            "nothing to rebase - working "
                            "directory parent is already an "
                            "ancestor of destination %s\n"
                        )
                        % dest
                    )
            else:  # can it happen?
                ui.status(
                    _("nothing to rebase from %s to %s\n")
                    % ("+".join(str(repo[r]) for r in base), dest)
                )
            return None

    allsrc = revsetlang.formatspec("%ld", rebaseset)
    alias = {"ALLSRC": allsrc}

    if dest is None and not "SRC" in str(destf) and destmap is None:
        try:
            # fast path: try to resolve dest without SRC alias
            dest = scmutil.revsingle(repo, destf, localalias=alias)
        except error.RepoLookupError:
            pass

    if dest is None and destmap is None:
        # multi-dest path: resolve dest for each SRC separately
        destmap = {}
        for r in rebaseset:
            alias["SRC"] = revsetlang.formatspec("%d", r)
            # use repo.anyrevs instead of scmutil.revsingle because we
            # don't want to abort if destset is empty.
            destset = repo.anyrevs([destf], user=True, localalias=alias)
            size = len(destset)
            if size == 1:
                destmap[r] = destset.first()
            elif size == 0:
                ui.note(_("skipping %s - empty destination\n") % repo[r])
            else:
                raise error.Abort(
                    _("rebase destination for %s is not unique") % repo[r]
                )

    if dest is not None:
        # single-dest case: assign dest to each rev in rebaseset
        destrev = dest.rev()
        destmap = {r: destrev for r in rebaseset}  # {srcrev: destrev}

    rbsrt.rebasingwcp = destmap is not None and repo["."].rev() in destmap
    ui.log("rebase", rebase_rebasing_wcp=rbsrt.rebasingwcp)
    if rbsrt.inmemory and rbsrt.rebasingwcp:
        # Require a clean working copy if rebasing the current commit, as the
        # last step of the rebase is an update.
        #
        # Technically this could be refined to hg update's checker, which can
        # be more permissive (e.g., allow if only non-conflicting paths are
        # changed).
        cmdutil.bailifchanged(repo)

    if not destmap:
        ui.status(_("nothing to rebase - empty destination\n"))
        return None

    return destmap


def externalparent(repo, state, destancestors):
    """Return the revision that should be used as the second parent
    when the revisions in state is collapsed on top of destancestors.
    Abort if there is more than one parent.
    """
    parents = set()
    source = min(state)
    for rev in state:
        if rev == source:
            continue
        for p in repo[rev].parents():
            if p.rev() not in state and p.rev() not in destancestors:
                parents.add(p.rev())
    if not parents:
        return nullrev
    if len(parents) == 1:
        return parents.pop()
    raise error.Abort(
        _("unable to collapse on top of %s, there is more than one external parent: %s")
        % (repo[max(destancestors)], ", ".join(str(repo[p]) for p in sorted(parents)))
    )


def _adjustrebasecopymessage(repo, message, keepf, source):
    # Ordinary rebase rewrites its source; only --keep creates a second live copy.
    if keepf:
        return rewriteutil.copycommitmessage(repo, message, "rebase --keep", source)
    return message


def concludememorynode(
    repo,
    rev,
    p1,
    p2,
    keepf,
    wctx=None,
    commitmsg=None,
    editor=None,
    extrafn=None,
    date=None,
    preds=None,
    noconflictmerge=False,
):
    """Commit the memory changes with parents p1 and p2. Reuse commit info from
    rev but also store useful information in extra.
    Return node of committed revision."""
    ctx = repo[rev]
    if commitmsg is None:
        commitmsg = ctx.description()
    extra = {"rebase_source": ctx.hex()}
    extra.update(subtreeutil.get_subtree_metadata(ctx.extra()))
    if noconflictmerge:
        extra[mergemod.NOCONFLICT_MERGE_EXTRA] = "1"
    mutinfo = None
    if not keepf:
        mutop = "rebase"
        if preds is None:
            preds = [ctx]
        preds = [p.node() for p in preds]
        mutinfo = mutation.record(repo, extra, preds, mutop)
    if extrafn:
        extrafn(ctx, extra)
    loginfo = {"predecessors": ctx.hex(), "mutation": "rebase"}

    if "narrowheads" in repo.storerequirements:
        # with narrow-heads, phases.new-commit is meaningless
        overrides = {}
    else:
        destphase = max(ctx.phase(), phases.draft)
        overrides = {("phases", "new-commit"): destphase}
    with repo.ui.configoverride(overrides, "rebase"):
        # Replicates the empty check in ``repo.commit``.
        if wctx.isempty() and not repo.ui.configbool("ui", "allowemptycommit"):
            return None

        if date is None:
            date = ctx.date()

        commitmsg = _adjustrebasecopymessage(repo, commitmsg, keepf, ctx)
        memctx = wctx.tomemctx(
            commitmsg,
            parents=(repo[p1], repo[p2]),
            date=date,
            extra=extra,
            user=ctx.user(),
            editor=editor,
            loginfo=loginfo,
            mutinfo=mutinfo,
        )
        commitres = repo.commitctx(memctx)
        wctx.clean()  # Might be reused
        return commitres


def concludenode(
    repo,
    rev,
    p1,
    p2,
    keepf,
    commitmsg=None,
    editor=None,
    extrafn=None,
    date=None,
    preds=None,
    noconflictmerge=False,
):
    """Commit the wd changes with parents p1 and p2. Reuse commit info from rev
    but also store useful information in extra.
    Return node of committed revision."""
    dsguard = util.nullcontextmanager()
    if not repo.ui.configbool("rebase", "singletransaction"):
        dsguard = dirstateguard.dirstateguard(repo, "rebase")
    with dsguard:
        repo.setparents(repo[p1].node(), repo[p2].node())
        ctx = repo[rev]
        if commitmsg is None:
            commitmsg = ctx.description()
        extra = {"rebase_source": ctx.hex()}
        extra.update(subtreeutil.get_subtree_metadata(ctx.extra()))
        if noconflictmerge:
            extra[mergemod.NOCONFLICT_MERGE_EXTRA] = "1"
        mutinfo = None
        if not keepf:
            mutop = "rebase"
            if preds is None:
                preds = [ctx]
            preds = [p.node() for p in preds]
            mutinfo = mutation.record(repo, extra, preds, mutop)
        if extrafn:
            extrafn(ctx, extra)

        loginfo = {"predecessors": ctx.hex(), "mutation": "rebase"}

        destphase = max(ctx.phase(), phases.draft)
        overrides = {("phases", "new-commit"): destphase}
        with repo.ui.configoverride(overrides, "rebase"):
            commitmsg = _adjustrebasecopymessage(repo, commitmsg, keepf, ctx)
            # Commit might fail if unresolved files exist
            if date is None:
                date = ctx.date()
            newnode = repo.commit(
                text=commitmsg,
                user=ctx.user(),
                date=date,
                extra=extra,
                editor=editor,
                loginfo=loginfo,
                mutinfo=mutinfo,
            )

        if newnode and p2 == nullrev:
            # log the newnode of a rebase operation, this data is used for
            # merge conflicts analysis.
            reponame = repo.ui.config("remotefilelog", "reponame", "unknown")
            repo.ui.log(
                "merge_conflicts",
                dest_hex=repo[p1].hex(),
                src_hex=ctx.hex(),
                newnode_hex=hex(newnode),
                repo=reponame,
            )

        return newnode


def rebasenode(repo, rev, p1, base, state, collapse, dest, wctx):
    """Rebase a single revision rev on top of p1 using base as merge ancestor

    Return a tuple of counts (updated, merged, removed, unresolved).
    """
    # Merge phase
    # Update to destination and merge it with local
    if wctx.isinmemory():
        wctx.setbase(repo[p1])
    else:
        if repo["."].rev() != p1:
            repo.ui.debug(" update to %s\n" % (repo[p1]))
            mergemod.goto(repo, p1, force=True)
        else:
            repo.ui.debug(" already in destination\n")
        # This is, alas, necessary to invalidate workingctx's manifest cache,
        # as well as other data we litter on it in other places.
        wctx = repo[None]
        repo.dirstate.write(repo.currenttransaction())
    repo.ui.debug(" merge against %s\n" % (repo[rev]))
    if base is not None:
        repo.ui.debug("   detach base %s\n" % (repo[base]))
    # Prepare the labels for rebase. This is currently a config option since
    # not all users are ready for the new label.
    if repo.ui.configbool("experimental", "rebase-long-labels"):
        labels = [
            "dest (rebasing onto)",
            "source (being rebased)",
            "base (parent of source)",
        ]
    else:
        labels = ["dest", "source", "base"]
    # When collapsing in-place, the parent is the common ancestor, we
    # have to allow merging with it.
    stats = mergemod.merge(
        repo,
        rev,
        force=True,
        ancestor=base,
        mergeancestor=collapse,
        labels=labels,
        wc=wctx,
    )
    if collapse:
        copies.duplicatecopies(repo, wctx, rev, dest)
    else:
        # If we're not using --collapse, we need to
        # duplicate copies between the revision we're
        # rebasing and its first parent, but *not*
        # duplicate any copies that have already been
        # performed in the destination.
        p1rev = repo[rev].p1().rev()
        copies.duplicatecopies(repo, wctx, rev, p1rev, skiprev=dest)
    return stats


def adjustdest(repo, rev, destmap, state, skipped):
    r"""adjust rebase destination given the current rebase state

    rev is what is being rebased. Return a list of two revs, which are the
    adjusted destinations for rev's p1 and p2, respectively. If a parent is
    nullrev, return dest without adjustment for it.

    For example, when doing rebasing B+E to F, C to G, rebase will first move B
    to B1, and E's destination will be adjusted from F to B1.

        B1 <- written during rebasing B
        |
        F <- original destination of B, E
        |
        | E <- rev, which is being rebased
        | |
        | D <- prev, one parent of rev being checked
        | |
        | x <- skipped, ex. no successor or successor in (::dest)
        | |
        | C <- rebased as C', different destination
        | |
        | B <- rebased as B1     C'
        |/                       |
        A                        G <- destination of C, different

    Another example about merge changeset, rebase -r C+G+H -d K, rebase will
    first move C to C1, G to G1, and when it's checking H, the adjusted
    destinations will be [C1, G1].

            H       C1 G1
           /|       | /
          F G       |/
        K | |  ->   K
        | C D       |
        | |/        |
        | B         | ...
        |/          |/
        A           A

    Besides, adjust dest according to existing rebase information. For example,

      B C D    B needs to be rebased on top of C, C needs to be rebased on top
       \|/     of D. We will rebase C first.
        A

          C'   After rebasing C, when considering B's destination, use C'
          |    instead of the original C.
      B   D
       \ /
        A
    """
    # pick already rebased revs with same dest from state as interesting source
    dest = destmap[rev]
    source = [
        s for s, d in state.items() if d > 0 and destmap[s] == dest and s not in skipped
    ]

    result = []
    for prev in repo.changelog.parentrevs(rev):
        adjusted = dest
        if prev != nullrev:
            candidate = repo.revs("max(%ld and (::%d))", source, prev).first()
            if candidate is not None:
                adjusted = state[candidate]
        if adjusted == dest and dest in state:
            adjusted = state[dest]
            if adjusted == revtodo:
                # sortsource should produce an order that makes this impossible
                raise error.ProgrammingError(
                    "rev %d should be rebased already at this time" % dest
                )
        result.append(adjusted)
    return result


def _checkobsrebase(repo, ui, rebaseobsrevs, rebaseobsskipped) -> None:
    """
    Abort if rebase will create divergence or rebase is noop because of markers

    `rebaseobsrevs`: set of obsolete revision in source
    `rebaseobsskipped`: set of revisions from source skipped because they have
    successors in destination
    """
    # Obsolete node with successors not in dest leads to divergence
    divergenceok = mutation.allow_divergence(ui)
    divergencebasecandidates = rebaseobsrevs - rebaseobsskipped

    if divergencebasecandidates and not divergenceok:
        divhashes = (str(repo[r]) for r in divergencebasecandidates)
        msg = _("this rebase will cause divergences from: %s")
        h = _("to force the rebase please set mutation.allow-divergence=True")
        raise error.Abort(msg % (",".join(divhashes),), hint=h)


def successorrevs(unfi, rev):
    """yield revision numbers for successors of rev"""
    nodemap = unfi.changelog.nodemap
    node = unfi[rev].node()
    if mutation.enabled(unfi):
        successors = mutation.allsuccessors(unfi, [node])
    else:
        successors = []
    for s in successors:
        if s in nodemap:
            yield nodemap[s]


def defineparents(repo, rev, destmap, state, skipped, obsskipped):
    """Return new parents and optionally a merge base for rev being rebased

    The destination specified by "dest" cannot always be used directly because
    previously rebase result could affect destination. For example,

          D E    rebase -r C+D+E -d B
          |/     C will be rebased to C'
        B C      D's new destination will be C' instead of B
        |/       E's new destination will be C' instead of B
        A

    The new parents of a merge is slightly more complicated. See the comment
    block below.
    """
    # use unfiltered changelog since successorrevs may return filtered nodes
    cl = repo.changelog

    def isancestor(a, b):
        # take revision numbers instead of nodes
        if a == b:
            return True
        elif a > b:
            return False
        return cl.isancestor(cl.node(a), cl.node(b))

    dest = destmap[rev]
    oldps = repo.changelog.parentrevs(rev)  # old parents
    newps = [nullrev, nullrev]  # new parents
    dests = adjustdest(repo, rev, destmap, state, skipped)
    bases = list(oldps)  # merge base candidates, initially just old parents

    if all(r == nullrev for r in oldps[1:]):
        # For non-merge changeset, just move p to adjusted dest as requested.
        newps[0] = dests[0]
    else:
        # For merge changeset, if we move p to dests[i] unconditionally, both
        # parents may change and the end result looks like "the merge loses a
        # parent", which is a surprise. This is a limit because "--dest" only
        # accepts one dest per src.
        #
        # Therefore, only move p with reasonable conditions (in this order):
        #   1. use dest, if dest is a descendent of (p or one of p's successors)
        #   2. use p's rebased result, if p is rebased (state[p] > 0)
        #
        # Comparing with adjustdest, the logic here does some additional work:
        #   1. decide which parents will not be moved towards dest
        #   2. if the above decision is "no", should a parent still be moved
        #      because it was rebased?
        #
        # For example:
        #
        #     C    # "rebase -r C -d D" is an error since none of the parents
        #    /|    # can be moved. "rebase -r B+C -d D" will move C's parent
        #   A B D  # B (using rule "2."), since B will be rebased.
        #
        # The loop tries to be not rely on the fact that a Mercurial node has
        # at most 2 parents.
        noconflictmerge = mergemod.is_noconflict_merge(repo[rev])
        for i, p in enumerate(oldps):
            np = p  # new parent
            if any(isancestor(x, dests[i]) for x in successorrevs(repo, p)):
                np = dests[i]
            elif p in state and state[p] > 0:
                np = state[p]

            # "bases" only record "special" merge bases that cannot be
            # calculated from changelog DAG (i.e. isancestor(p, np) is False).
            # For example:
            #
            #   B'   # rebase -s B -d D, when B was rebased to B'. dest for C
            #   | C  # is B', but merge base for C is B, instead of
            #   D |  # changelog.ancestor(C, B') == A. If changelog DAG and
            #   | B  # "state" edges are merged (so there will be an edge from
            #   |/   # B to B'), the merge base is still ancestor(C, B') in
            #   A    # the merged graph.
            #
            # Also see https://bz.mercurial-scm.org/show_bug.cgi?id=1950#c8
            # which uses "virtual null merge" to explain this situation.
            if isancestor(p, np):
                bases[i] = nullrev

            # If one parent becomes an ancestor of the other, drop the ancestor
            for j, x in enumerate(newps[:i]):
                if x == nullrev:
                    continue
                if isancestor(np, x):  # CASE-1
                    np = nullrev
                elif isancestor(x, np):  # CASE-2
                    newps[j] = np
                    np = nullrev
                    # New parents forming an ancestor relationship does not
                    # mean the old parents have a similar relationship. Do not
                    # set bases[x] to nullrev.
                    bases[j], bases[i] = bases[i], bases[j]

            newps[i] = np

        # "rebasenode" updates to new p1, and the old p1 will be used as merge
        # base. If only p2 changes, merging using unchanged p1 as merge base is
        # suboptimal. Therefore swap parents to make the merge sane. A
        # conflict-free merge is merged again from scratch instead and keeps
        # its parent order.
        if newps[1] != nullrev and oldps[0] == newps[0] and not noconflictmerge:
            assert len(newps) == 2 and len(oldps) == 2
            newps.reverse()
            bases.reverse()

        # No parent change might be an error because we fail to make rev a
        # descendent of requested dest. This can happen, for example:
        #
        #     C    # rebase -r C -d D
        #    /|    # None of A and B will be changed to D and rebase fails.
        #   A B D
        if set(newps) == set(oldps) and dest not in newps:
            if noconflictmerge:
                # A conflict-free merge has no content of its own, so it can
                # also move backwards: a destination that is an ancestor of
                # exactly one parent replaces that parent. An ancestor of
                # both could mean either and stays an error.
                below = [i for i, p in enumerate(oldps) if isancestor(dest, p)]
                if len(below) == 1:
                    newps[below[0]] = dest
            if set(newps) == set(oldps) and dest not in newps:
                raise error.Abort(
                    _("cannot rebase %s without moving at least one of its parents")
                    % (repo[rev])
                )

    # Source should not be ancestor of dest. The check here guarantees it's
    # impossible. With multi-dest, the initial check does not cover complex
    # cases since we don't have abstractions to dry-run rebase cheaply.
    if any(p != nullrev and isancestor(rev, p) for p in newps):
        raise error.Abort(_("source is ancestor of destination"))

    # "rebasenode" updates to new p1, use the corresponding merge base.
    if bases[0] != nullrev:
        base = bases[0]
    else:
        base = None

    # Check if the merge will contain unwanted changes. That may happen if
    # there are multiple special (non-changelog ancestor) merge bases, which
    # cannot be handled well by the 3-way merge algorithm. For example:
    #
    #     F
    #    /|
    #   D E  # "rebase -r D+E+F -d Z", when rebasing F, if "D" was chosen
    #   | |  # as merge base, the difference between D and F will include
    #   B C  # C, so the rebased F will contain C surprisingly. If "E" was
    #   |/   #  chosen, the rebased F will contain B.
    #   A Z
    #
    # But our merge base candidates (D and E in above case) could still be
    # better than the default (ancestor(F, Z) == null). Therefore still
    # pick one (so choose p1 above).
    # A conflict-free merge is merged again from its parents or dropped when
    # they end up on one line; its diff is never replayed, so the merge base
    # does not matter.
    if not mergemod.is_noconflict_merge(repo[rev]) and (
        sum(1 for b in bases if b != nullrev) > 1
    ):
        unwanted = [None, None]  # unwanted[i]: unwanted revs if choose bases[i]
        for i, base in enumerate(bases):
            if base == nullrev:
                continue
            # Revisions in the side (not chosen as merge base) branch that
            # might contain "surprising" contents
            siderevs = list(repo.revs("((%ld-%d) %% (%d+%d))", bases, base, base, dest))

            # If those revisions are covered by rebaseset, the result is good.
            # A merge in rebaseset would be considered to cover its ancestors.
            if siderevs:
                rebaseset = [
                    r for r, d in state.items() if d > 0 and r not in obsskipped
                ]
                merges = [r for r in rebaseset if cl.parentrevs(r)[1] != nullrev]
                unwanted[i] = list(
                    repo.revs("%ld - (::%ld) - %ld", siderevs, merges, rebaseset)
                )

        # Choose a merge base that has a minimal number of unwanted revs.
        l, i = min(
            (len(revs), i) for i, revs in enumerate(unwanted) if revs is not None
        )
        base = bases[i]

        # newps[0] should match merge base if possible. Currently, if newps[i]
        # is nullrev, the only case is newps[i] and newps[j] (j < i), one is
        # the other's ancestor. In that case, it's fine to not swap newps here.
        # (see CASE-1 and CASE-2 above)
        if i != 0 and newps[i] != nullrev:
            newps[0], newps[i] = newps[i], newps[0]

        # The merge will include unwanted revisions. Abort now. Revisit this if
        # we have a more advanced merge algorithm that handles multiple bases.
        if l > 0:
            unwanteddesc = _(" or ").join(
                (
                    ", ".join("%s" % (repo[r]) for r in revs)
                    for revs in unwanted
                    if revs is not None
                )
            )
            raise error.Abort(
                _("rebasing %s will include unwanted changes from %s")
                % (repo[rev], unwanteddesc)
            )

    repo.ui.debug(" future parents are %s and %s\n" % tuple(repo[p] for p in newps))

    return newps[0], newps[1], base


def storecollapsemsg(repo, collapsemsg: str) -> None:
    "Store the collapse message to allow recovery"
    collapsemsg = collapsemsg or ""
    f = repo.localvfs("last-message.txt", "wb")
    f.write(b"%s\n" % collapsemsg.encode())
    f.close()


def clearcollapsemsg(repo) -> None:
    "Remove collapse message file"
    repo.localvfs.unlinkpath("last-message.txt", ignoremissing=True)


def restorecollapsemsg(repo, isabort) -> str:
    "Restore previously stored collapse message"
    try:
        f = repo.localvfs("last-message.txt", "rb")
        collapsemsg = f.readline().strip().decode()
        f.close()
    except IOError as err:
        if err.errno != errno.ENOENT:
            raise
        if isabort:
            # Oh well, just abort like normal
            collapsemsg = ""
        else:
            raise error.Abort(
                _("missing %s/last-message.txt for rebase")
                % identity.default().dotdir()
            )
    return collapsemsg


def clearstatus(repo) -> None:
    "Remove the status files"
    # Make sure the active transaction won't write the state file
    tr = repo.currenttransaction()
    if tr:
        tr.removefilegenerator("rebasestate")
    repo.localvfs.unlinkpath("rebasestate", ignoremissing=True)


def needupdate(repo, state) -> bool:
    """check whether we should `goto --clean` away from a merge, or if
    somehow the working dir got forcibly updated, e.g. by older hg"""
    parents = [p.rev() for p in repo[None].parents()]

    # Are we in a merge state at all?
    if len(parents) < 2:
        return False

    # We should be standing on the first as-of-yet unrebased commit.
    firstunrebased = min([old for old, new in state.items() if new == nullrev])
    if firstunrebased in parents:
        return True

    return False


def abort(repo, originalwd, destmap, state, activebookmark=None) -> int:
    """Restore the repository to its original state.  Additional args:

    activebookmark: the name of the bookmark that should be active after the
        restore"""

    try:
        # If the first commits in the rebased set get skipped during the rebase,
        # their values within the state mapping will be the dest rev id. The
        # rebased list must must not contain the dest rev (issue4896)
        rebased = [s for r, s in state.items() if s >= 0 and s != r and s != destmap[r]]
        immutable = [d for d in rebased if not repo[d].mutable()]
        cleanup = True
        if immutable:
            repo.ui.warn(
                _("warning: can't clean up public changesets %s\n")
                % ", ".join(str(repo[r]) for r in immutable),
                hint=_("see '@prog@ help phases' for details"),
            )
            cleanup = False

        descendants = set()
        if rebased:
            descendants = set(repo.changelog.descendants(rebased))
        if descendants - set(rebased):
            repo.ui.warn(
                _(
                    "warning: new changesets detected on destination "
                    "branch, can't strip\n"
                )
            )
            cleanup = False

        if cleanup:
            shouldupdate = False

            updateifonnodes = set(rebased)
            updateifonnodes.update(destmap.values())
            updateifonnodes.add(originalwd)
            shouldupdate = repo["."].rev() in updateifonnodes

            # Update away from the rebase if necessary
            if shouldupdate or needupdate(repo, state):
                mergemod.goto(repo, originalwd, force=True)

            # Strip from the first rebased revision
            if rebased:
                # no backup of rebased cset versions needed
                nodes = list(map(repo.changelog.node, rebased))
                scmutil.cleanupnodes(repo, nodes, "rebase")

        if activebookmark and activebookmark in repo._bookmarks:
            bookmarks.activate(repo, activebookmark)

    finally:
        clearstatus(repo)
        clearcollapsemsg(repo)
        repo.ui.warn(_("rebase aborted\n"))
    return 0


def quit_rebase(repo, activebookmark=None) -> int:
    """Quit an interrupted rebase and keep the already rebased commits."""
    try:
        mergemod.goto(repo, repo["."].rev(), force=True)
        if activebookmark and activebookmark in repo._bookmarks:
            bookmarks.activate(repo, activebookmark)
    finally:
        clearstatus(repo)
        clearcollapsemsg(repo)
        repo.ui.warn(_("rebase quited\n"))
    return 0


def sortsource(destmap):
    """yield source revisions in an order that we only rebase things once

    If source and destination overlaps, we should filter out revisions
    depending on other revisions which hasn't been rebased yet.

    Yield a sorted list of revisions each time.

    For example, when rebasing A to B, B to C. This function yields [B], then
    [A], indicating B needs to be rebased first.

    Raise if there is a cycle so the rebase is impossible.
    """
    srcset = set(destmap)
    while srcset:
        srclist = sorted(srcset)
        result = []
        for r in srclist:
            if destmap[r] not in srcset:
                result.append(r)
        if not result:
            raise error.Abort(_("source and destination form a cycle"))
        srcset -= set(result)
        yield result


def buildstate(repo, destmap, collapse):
    """Define which revisions are going to be rebased and where

    repo: repo
    destmap: {srcrev: destrev}
    """
    rebaseset = destmap.keys()
    originalwd = repo["."].rev()

    # Get "cycle" error early by exhausting the generator.
    sortedsrc = list(sortsource(destmap))  # a list of sorted revs
    if not sortedsrc:
        raise error.Abort(_("no matching revisions"))

    # Only check the first batch of revisions to rebase not depending on other
    # rebaseset. This means "source is ancestor of destination" for the second
    # (and following) batches of revisions are not checked here. We rely on
    # "defineparents" to do that check.
    roots = list(repo.set("sort(roots(%ld))", sortedsrc[0]))
    if not roots:
        raise error.Abort(_("no matching revisions"))
    state = dict.fromkeys(rebaseset, revtodo)
    emptyrebase = len(sortedsrc) == 1
    for root in roots:
        dest = repo[destmap[root.rev()]]
        commonbase = root.ancestor(dest)
        if commonbase == root:
            raise error.Abort(_("source is ancestor of destination"))
        if commonbase == dest:
            rootparents = root.parents()
            if not collapse and (
                dest in rootparents or (dest.node() == nullid and not rootparents)
            ):
                # mark the revision as done by setting its new revision
                # equal to its old (current) revisions
                state[root.rev()] = root.rev()
                repo.ui.debug("source is a child of destination\n")
                continue

        emptyrebase = False
        repo.ui.debug("rebase onto %s starting from %s\n" % (dest, root))
    if emptyrebase:
        return None
    for rev in sorted(state):
        parents = [p for p in repo.changelog.parentrevs(rev) if p != nullrev]
        # if all parents of this revision are done, then so is this revision
        if parents and all((state.get(p) == p for p in parents)):
            state[rev] = rev
    return originalwd, destmap, state


def clearrebased(
    ui, repo, templ, destmap, state, skipped, collapsedas=None, keepf: bool = False
) -> None:
    """dispose of rebased revision at the end of the rebase

    If `collapsedas` is not None, the rebase was a collapse whose result if the
    `collapsedas` node.

    If `keepf` is True, the rebase has --keep set and no nodes should be
    removed (but bookmarks still need to be moved).
    """
    tonode = repo.changelog.node
    replacements = util.sortdict()
    moves = {}
    for rev, newrev in sorted(state.items()):
        if newrev >= 0 and newrev != rev:
            oldnode = tonode(rev)
            newnode = collapsedas or tonode(newrev)
            moves[oldnode] = newnode
            if not keepf:
                if rev in skipped:
                    succs = ()
                else:
                    succs = (newnode,)
                replacements[oldnode] = succs
    scmutil.cleanupnodes(repo, replacements, "rebase", moves)
    if templ:
        templ.setprop("nodereplacements", replacements)


def _filterobsoleterevs(repo, revs):
    """returns a set of the obsolete revisions in revs"""
    return set(r for r in revs if repo[r].obsolete())


def _computeobsoletenotrebased(repo, rebaseobsrevs, destmap):
    """Return (obsoletenotrebased, obsoletewithoutsuccessorindestination).

    `obsoletenotrebased` is a mapping mapping obsolete => successor for all
    obsolete nodes to be rebased given in `rebaseobsrevs`.

    `obsoletewithoutsuccessorindestination` is a set with obsolete revisions
    without a successor in destination.
    """
    obsoletenotrebased = {}
    obsoletewithoutsuccessorindestination = set([])

    cl = repo.changelog
    nodemap = cl.nodemap
    for srcrev in rebaseobsrevs:
        srcnode = cl.node(srcrev)
        destnode = cl.node(destmap[srcrev])
        # XXX: more advanced APIs are required to handle split correctly
        # allsuccessors can include nodes that aren't present
        # in the repo and changelog nodemap. This is normal if CommitCloud
        # extension is enabled.
        if mutation.enabled(repo):
            successors = list(mutation.allsuccessors(repo, [srcnode]))
        else:
            successors = []
        if len(successors) == 1:
            # allsuccessors includes node itself. When the list only
            # contains one element, it means there are no successors.
            obsoletenotrebased[srcrev] = None
        else:
            for succnode in successors:
                if succnode == srcnode or succnode not in nodemap:
                    continue
                if cl.isancestor(succnode, destnode):
                    obsoletenotrebased[srcrev] = nodemap[succnode]
                    break
            else:
                # If 'srcrev' has a successor in rebase set but none in
                # destination (which would be caught above), we shall skip it
                # and its descendants to avoid divergence.
                # allsuccessors can include nodes that aren't present
                # in changelog nodemap.
                if any(
                    nodemap[s] in destmap
                    for s in successors
                    if s != srcnode and s in nodemap
                ):
                    obsoletewithoutsuccessorindestination.add(srcrev)

    return obsoletenotrebased, obsoletewithoutsuccessorindestination


def summaryhook(ui, repo) -> None:
    if not repo.localvfs.exists("rebasestate"):
        return
    try:
        rbsrt = rebaseruntime(repo, ui, None, {})
        rbsrt.restorestatus()
        state = rbsrt.state
    except error.RepoLookupError:
        # i18n: column positioning for "hg summary"
        msg = _('rebase: (use "@prog@ rebase --abort" to clear broken state)\n')
        ui.write(msg)
        return
    numrebased = len([i for i in state.values() if i >= 0])
    # i18n: column positioning for "hg summary"
    ui.write(
        _("rebase: %s, %s (rebase --continue)\n")
        % (
            ui.label(_("%d rebased"), "rebase.rebased") % numrebased,
            ui.label(_("%d remaining"), "rebase.remaining") % (len(state) - numrebased),
        )
    )


def uisetup(ui) -> None:
    cmdutil.summaryhooks.add("rebase", summaryhook)
    cmdutil.afterresolvedstates.append(("rebasestate", "@prog@ rebase --continue"))
