# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# rewriteutil.py - utility functions for rewriting changesets
#
# Copyright 2017 Octobus <contact@octobus.net>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.

from contextlib import contextmanager

from bindings import agentdetect

from . import error, extensions, hintutil, mutation, node, slacl
from .i18n import _
from .node import short

_SPLIT_IN_PROGRESS = "rewriteutil.split-in-progress"
_OBSOLETE_REWRITE_APPROVALS = "rewriteutil.obsolete-rewrite-approvals"


def copycommitmessage(repo, message, operation, source):
    """Allow extensions to adjust a copied commit's message.

    ``source`` is the commit being copied.
    """
    return message


def precheckmessage(repo, predecessors, message):
    """Allow extensions to validate a rewrite's final message upfront."""


def precheck(repo, revs, action="rewrite", checkmerge=True):
    """check if revs can be rewritten
    action is used to control the error message.

    Make sure this function is called after taking the lock.
    """
    if node.nullrev in revs:
        msg = _("cannot %s null changeset") % action
        hint = _("no changeset checked out")
        raise error.Abort(msg, hint=hint)

    publicrevs = repo.revs("%ld and public()", revs)
    if checkmerge and len(repo.working_parent_nodes()) > 1:
        raise error.Abort(_("cannot %s while merging") % action)

    if publicrevs:
        msg = _("cannot %s public changesets") % action
        hint = _("see '@prog@ help phases' for details")
        raise error.Abort(msg, hint=hint)

    slacl.abort_if_restricted(repo, (repo[rev] for rev in revs))

    _record_obsolete_approvals(repo, _checkobsolete(repo, [repo[rev] for rev in revs]))


def _record_obsolete_approvals(repo, approved):
    if not approved:
        return
    if _OBSOLETE_REWRITE_APPROVALS not in repo.volatile_state:
        repo.volatile_state[_OBSOLETE_REWRITE_APPROVALS] = set()
        repo.ui.atexit(
            lambda: repo.volatile_state.pop(_OBSOLETE_REWRITE_APPROVALS, None)
        )
    approvals = repo.volatile_state[_OBSOLETE_REWRITE_APPROVALS]
    approvals.update(ctx.node() for ctx in approved)


def actormetric(prefix, suffix):
    """Metric name split by whether an agent or a human drove the command."""
    actor = "agent_" if agentdetect.is_agent() else "human_"
    return prefix + actor + suffix


def _hasvisibledescendant(repo, ctx):
    # wdir() is a virtual descendant of the checked-out commit, not an
    # existing committed stack.
    descendants = repo.nodes(
        "limit((((%n::) - %n) - hidden()) - wdir(), 1)",
        ctx.node(),
        ctx.node(),
    )
    return next(iter(descendants), None) is not None


# Boolean configs the mode configs replaced, still honored for rollback.
_LEGACY_BOOLEAN_CONFIGS = {
    ("commit", "modify-obsolete-mode"): ("commit", "reject-modifying-obsolete"),
}


def _obsolete_mode(repo, config):
    default = "abort" if agentdetect.is_agent() else "warn"
    mode = repo.ui.config(config[0], config[1])
    if mode is None:
        legacy = _LEGACY_BOOLEAN_CONFIGS.get(config)
        if legacy is not None and repo.ui.config(legacy[0], legacy[1]) is not None:
            return default if repo.ui.configbool(legacy[0], legacy[1]) else "ignore"
        return default
    # Unrecognized values (e.g. "off") disable the guard so emergency
    # rollback configs fail open.
    return mode if mode in ("warn", "prompt", "abort") else "ignore"


def _checkobsolete(
    repo,
    contexts,
    message=None,
    hint=None,
    allow_public_successors=False,
    allow_visible_descendants=False,
    mode_config=("commit", "modify-obsolete-mode"),
):
    if not mutation.enabled(repo):
        return []

    obsolete_contexts = [ctx for ctx in contexts if ctx.obsolete()]
    if not obsolete_contexts:
        return []

    # Counters are namespaced by the guard: commit.obsolete.* for rewrite and
    # commit guards, checkout.obsolete.* for the goto guard.
    prefix = mode_config[0] + ".obsolete."

    # All bypasses below come before the per-context fate and visibility
    # queries, so automation, explicit opt-ins, and disabled modes only pay
    # for the obsolete() checks. Their counters may therefore include
    # contexts the allowances would have exempted anyway.
    if repo.ui.plain():
        repo.ui.metrics.inc(prefix + "automation_allowed", 1)
        return obsolete_contexts

    # allow-divergence is the established opt-in for operations that
    # deliberately rewrite obsolete commits.
    if mutation.allow_divergence(repo.ui):
        repo.ui.metrics.inc(prefix + "config_allowed", 1)
        return obsolete_contexts

    mode = _obsolete_mode(repo, mode_config)
    if mode == "ignore":
        repo.ui.metrics.inc(prefix + "mode_ignored", 1)
        return obsolete_contexts

    ispublic = mutation.getispublicfunc(repo) if allow_public_successors else None
    obsolete = []
    for ctx in obsolete_contexts:
        if allow_visible_descendants and _hasvisibledescendant(repo, ctx):
            continue
        fates = mutation.fate(repo, ctx.node())
        # After every successor lands, working on top of the old draft is a
        # routine post-land state rather than new divergence. A fate with no
        # successors (e.g. a prune) is not "landed", so it does not qualify.
        if (
            allow_public_successors
            and fates
            and all(
                successors and all(ispublic(successor) for successor in successors)
                for successors, _operation in fates
            )
        ):
            continue
        obsolete.append((ctx, fates))
    if not obsolete:
        return []

    if message is None:
        message = _("changing an old version of a commit will diverge your stack")
    details = []
    for ctx, fates in obsolete:
        for successors, operation in fates:
            successor_ids = ", ".join(short(successor) for successor in successors)
            details.append(
                "- %s -> %s (%s)" % (short(ctx.node()), successor_ids, operation)
            )
        if not fates:
            details.append("- %s is obsolete" % short(ctx.node()))
    if details:
        message += ":\n" + "\n".join(details)

    if hint is None:
        hint = _(
            "switch to the newer version listed above, or run '@prog@ graft' "
            "with the old commit hash to deliberately fork it; '@prog@ sl' "
            "shows the latest graph"
        )
    if mode == "abort":
        repo.ui.metrics.inc(actormetric(prefix, "rejected"), 1)
        raise error.Abort(message, hint=hint)

    repo.ui.warn(_("warning: %s\n") % message)
    if mode == "prompt":
        choice = repo.ui.promptchoice(_("proceed (Yn)? $$ &Yes $$ &No"), default=0)
        if choice != 0:
            repo.ui.metrics.inc(actormetric(prefix, "prompt_no"), 1)
            raise error.Abort(_("aborted by user"))
        repo.ui.metrics.inc(actormetric(prefix, "prompt_yes"), 1)
        return [ctx for ctx, _fates in obsolete]

    repo.ui.metrics.inc(actormetric(prefix, "warned"), 1)
    return [ctx for ctx, _fates in obsolete]


def restackdescendants(ui, repo, oldnode, descendants, source) -> None:
    """Rebase ``descendants`` of rewritten commits onto the new versions if
    ``amend.autorestack`` restacks changed commits, or hint that they were
    left behind on ``oldnode``.

    The rebase is done in memory and never stops for a conflict, since the
    working copy may hold changes: descendants that would conflict stay
    where they are. ``source`` names the command for config overrides.
    """
    try:
        amend = extensions.find("amend")
    except KeyError:
        return
    if amend.autorestackconf(ui) not in (
        amend.RESTACK_NO_CONFLICT,
        amend.RESTACK_ALWAYS,
    ) or not ui.configbool("rebase", "experimental.inmemory"):
        hintutil.trigger("amend-restack", oldnode)
        return
    # internal config: amend.autorestackmsg
    msg = ui.config(
        "amend",
        "autorestackmsg",
        _("restacking children automatically (unless they conflict)"),
    )
    if msg:
        ui.status("%s\n" % msg)
    noconflictmsg = _(
        "restacking would create conflicts (%s in %s), so you must run it manually\n"
        "(run `@prog@ restack` manually to restack this commit's children)"
    )
    with ui.configoverride({("rebase", "noconflictmsg"): noconflictmsg}, source):
        amend.restack.restack(
            ui, repo, rev=[node.hex(n) for n in descendants], noconflict=True
        )


@contextmanager
def splitting(repo):
    # Intermediate split commits lack mutation metadata. Mark the split in
    # progress so hooks can defer validation until the terminal split commit.
    if _SPLIT_IN_PROGRESS in repo.volatile_state:
        raise error.ProgrammingError("split is already in progress")
    repo.volatile_state[_SPLIT_IN_PROGRESS] = True
    try:
        yield
    finally:
        repo.volatile_state.pop(_SPLIT_IN_PROGRESS)


def issplitting(repo):
    return _SPLIT_IN_PROGRESS in repo.volatile_state


def _localcontexts(repo, nodes):
    return [
        repo[commit_node]
        for commit_node in repo.changelog.filternodes(nodes, local=True)
    ]


def commitcheck(repo, ctx):
    """Run extension checks for one commit being written."""
    mutinfo = ctx.mutinfo() or {}
    predecessors = _localcontexts(
        repo, mutation.nodesfrominfo(mutinfo.get("mutpred")) or []
    )
    split_successors = _localcontexts(
        repo, mutation.nodesfrominfo(mutinfo.get("mutsplit")) or []
    )
    # An approval covers the whole command: one approved predecessor may be
    # rewritten into several successors (e.g. a split), each reaching here.
    approvals = repo.volatile_state.get(_OBSOLETE_REWRITE_APPROVALS, set())
    unapproved_predecessors = [
        predecessor
        for predecessor in predecessors
        if predecessor.node() not in approvals
    ]
    # Record approvals granted here too, so rewrite paths without a command
    # precheck also prompt at most once per predecessor.
    _record_obsolete_approvals(repo, _checkobsolete(repo, unapproved_predecessors))
    predecessor_parents = {
        parent.node()
        for predecessor in predecessors
        for parent in predecessor.parents()
    }
    _checkobsolete(
        repo,
        [
            parent
            for parent in ctx.parents()
            if parent.node() != node.nullid and parent.node() not in predecessor_parents
        ],
        _("creating a child of an old version of a commit will diverge your stack"),
        _(
            "switch to the newer version listed above first -- '@prog@ goto' "
            "carries uncommitted changes along, or use '@prog@ shelve' and "
            "'@prog@ unshelve' to move them"
        ),
        allow_public_successors=not predecessors,
        allow_visible_descendants=not predecessors,
    )
    return predecessors, split_successors


def rebasedestcheck(repo, destmap):
    """Guard rebases onto older versions of the moved commits' own ancestry.

    Depending on `rebase.obsolete-dest-mode` and whether the caller is an
    agent, this warns, prompts, or aborts when a destination's ancestors
    include predecessors of the draft commits the moved commits are based on.
    """
    if not mutation.enabled(repo):
        return
    # Skip the ancestry walk entirely when the guard cannot act on what it
    # finds (automation, the divergence opt-in, or a disabled mode).
    if (
        repo.ui.plain()
        or mutation.allow_divergence(repo.ui)
        or _obsolete_mode(repo, ("rebase", "obsolete-dest-mode")) == "ignore"
    ):
        return
    rebaseset = list(destmap)

    def finaldest(dest):
        # A destination that is itself being rebased ends up wherever its own
        # rebase sends it, so follow the chain to the commit that stays put.
        # That commit's ancestry is the ancestry the moved roots inherit.
        seen = set()
        while dest in destmap and dest not in seen:
            seen.add(dest)
            dest = destmap[dest]
        return dest

    offenders = set()
    for root in repo.revs("roots(%ld)", rebaseset):
        dest = finaldest(destmap[root])
        # A rebase that lands on an obsolete commit gives the moved roots an
        # old-version parent directly, which the commitcheck guard reports;
        # leave that warning to it so only one guard fires.
        if repo[dest].obsolete():
            continue
        base = repo.revs("draft() & ::parents(%d)", root)
        if not base:
            continue
        # Transitive on purpose: the stale version is often several rewrites
        # behind the one in the stack.
        old = repo.revs("allpredecessors(%ld) - %ld", base, base)
        offenders.update(repo.revs("(%ld & ::%d) - %ld", old, dest, rebaseset))
    if not offenders:
        return
    _checkobsolete(
        repo,
        [repo[rev] for rev in sorted(offenders)],
        _(
            "the destination is based on old versions of commits in your stack, "
            "so the rebased commits will lose their newer changes"
        ),
        _(
            "check out the destination and run '@prog@ restack' to move it onto "
            "the newer versions first, or use '--keep' to copy the commits onto "
            "the old versions without hiding them"
        ),
        mode_config=("rebase", "obsolete-dest-mode"),
    )


def gotocheck(repo, targets):
    """Guard checkouts of hidden obsolete commits.

    Depending on `checkout.obsolete-mode` and whether the caller is an agent,
    this warns, prompts, or aborts; visible obsolete commits are not guarded.
    """
    hidden_nodes = set(repo.nodes("%ln & hidden()", [ctx.node() for ctx in targets]))
    hidden_targets = [ctx for ctx in targets if ctx.node() in hidden_nodes]
    if not hidden_targets:
        return
    target_args = " ".join(short(ctx.node()) for ctx in hidden_targets)
    _checkobsolete(
        repo,
        hidden_targets,
        _("checking out an old version of a commit risks diverging your stack"),
        _(
            "check out the newer version listed above instead, or run "
            "'@prog@ unhide %s' to explicitly allow checking out this old commit"
        )
        % target_args,
        allow_public_successors=True,
        allow_visible_descendants=True,
        mode_config=("checkout", "obsolete-mode"),
    )
