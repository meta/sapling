# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""command to display a relevant subgraph

With this extension installed, Mercurial gains one new command: smartlog.
It displays a subgraph of changesets containing only the changesets relevant
to the user.

::

    [smartlog]
    # (remote) names to show
    repos = , remote/, default/
    names = @, master, stable
    # move the top non-public stack to the second column
    indentnonpublic = True

    # Default parameter for master
    master = remote/master

    # Collapse obsoleted commits
    collapse-obsolete = True
"""

import datetime
import re
import sys
import time
from dataclasses import dataclass, field, replace

import bindings
from sapling import (
    bookmarks,
    cmdutil,
    commands,
    dagop,
    error,
    extensions,
    graphmod,
    hintutil,
    node as nodemod,
    phases,
    registrar,
    revset,
    revsetlang,
    scmutil,
    smartset,
    templater,
    util,
)
from sapling.i18n import _

if not util.iswindows:
    from . import interactiveui
else:
    interactiveui = None
from . import rebase

cmdtable = {}
command = registrar.command(cmdtable)
revsetpredicate = registrar.revsetpredicate()

testedwith = "ships-with-fb-ext"

# Remove unsupported --limit option.
logopts = [opt for opt in commands.logopts if opt[1] != "limit"]


templatekeyword = registrar.templatekeyword()
templatefunc = registrar.templatefunc()
hint = registrar.hint()


@templatekeyword("shelveenabled")
def shelveenabled(repo, ctx, **args):
    """Bool. Return true if shelve extension is enabled"""
    return "shelve" in extensions.enabled().keys()


def getdag(ui, repo, revs, masterstring, template):
    knownrevs = set(revs)
    gpcache = {}
    results = []
    reserved = []

    # we store parents together with the parent type information
    # but sometimes we need just a list of parents
    # [(a,b), (c,d), (e,f)] => [b, d, f]
    def unzip(parents):
        if parents:
            return list(list(zip(*parents))[1])
        else:
            return list()

    simplifygrandparents = ui.configbool("log", "simplify-grandparents")
    cl = repo.changelog
    if cl.algorithmbackend != "segments":
        simplifygrandparents = False
    if simplifygrandparents:
        rootnodes = cl.tonodes(revs)

    masterrev = repo.anyrevs([masterstring], user=True).first()
    firstbranch = []
    if masterrev is not None:
        firstbranch.append(masterrev)
    revs = repo.revs("sort(%ld,topo,topo.firstbranch=%ld)", revs, firstbranch)
    ctxstream = revs.prefetchbytemplate(repo, template).iterctx()

    # For each rev we need to show, compute it's parents in the dag.
    # If we have to reach for a grandparent, insert a fake node so we
    # can show '...' in the graph.
    # Use 'reversed' to start at the lowest commit so fake nodes are
    # placed at their lowest possible positions.
    for ctx in ctxstream:
        rev = ctx.rev()
        # Parents in the dag
        parents = sorted(
            set(
                [
                    (graphmod.PARENT, p.rev())
                    for p in ctx.parents()
                    if p.rev() in knownrevs
                ]
            )
        )
        # Parents not in the dag
        mpars = [
            p.rev()
            for p in ctx.parents()
            if p.rev() != nodemod.nullrev and p.rev() not in unzip(parents)
        ]

        for mpar in mpars:
            gp = gpcache.get(mpar)
            if gp is None:
                if simplifygrandparents:
                    gp = gpcache[mpar] = cl.torevs(
                        cl.dageval(
                            lambda: headsancestors(
                                ancestors(cl.tonodes([mpar])) & rootnodes
                            )
                        )
                    )
                else:
                    gp = gpcache[mpar] = dagop.reachableroots(repo, revs, [mpar])
            if not gp:
                parents.append((graphmod.MISSINGPARENT, mpar))
            else:
                gp = [g for g in gp if g not in unzip(parents)]
                for g in gp:
                    parents.append((graphmod.GRANDPARENT, g))

        results.append((ctx.rev(), "C", ctx, parents))

    # indent the top non-public stack
    if ui.configbool("smartlog", "indentnonpublic", False):
        rev, ch, ctx, parents = results[0]
        if ctx.phase() != phases.public:
            # find a public parent and add a fake node, so the non-public nodes
            # will be shown in the non-first column
            prev = None
            for i in range(1, len(results)):
                pctx = results[i][2]
                if pctx.ispublic():
                    prev = results[i][0]
                    break
            if prev is not None:
                reserved.append(prev)

    return results, reserved


def _reposnames(repo):
    ui = repo.ui

    # '' is local repo. This also defines an order precedence for master.
    repos = ui.configlist("smartlog", "repos", ["", "remote/", "default/"])

    if ui.configbool("experimental", "smartlog-use-sp-bm", True):
        main = bookmarks.mainbookmark(repo)
    else:
        main = "master"

    names = ui.configlist("smartlog", "names", ["@", main, "stable"])

    for repo in repos:
        for name in names:
            yield repo + name


@revsetpredicate("smartlog([heads], [master])")
def smartlogrevset(repo, subset, x):
    """``smartlog([heads], [master])``
    Changesets relevant to you.

    'heads' overrides what feature branches to include.
    (default: 'interestingbookmarks() + heads(draft()) + .')

    'master' is the head of the public branch.
    (default: 'interestingmaster()')
    """
    args = revset.getargsdict(x, "smartlogrevset", "heads master")
    if "master" in args:
        masterset = revset.getset(repo, subset, args["master"])
    else:
        masterset = repo.revs("interestingmaster()")

    if "heads" in args:
        heads = set(revset.getset(repo, subset, args["heads"]))
    else:
        heads = set(repo.revs("interestingbookmarks() + heads(draft()) + ."))

    # Remove "null" commit. "::x" does not support it.
    masterset -= smartset.baseset([nodemod.nullrev], repo=repo)
    if nodemod.nullrev in heads:
        heads.remove(nodemod.nullrev)

    cl = repo.changelog
    if cl.algorithmbackend == "segments":
        heads = cl.tonodes(heads)
        master = cl.tonodes(masterset)
        nodes = smartlognodes(repo, heads, master)
        return subset & cl.torevset(nodes)

    # Explicitly disable revnum deprecation warnings.
    with repo.ui.configoverride({("devel", "legacy.revnum:real"): ""}):
        # Select ancestors that are draft.
        drafts = repo.revs("draft() & ::%ld", heads)
        # Include parents of drafts, and public heads.
        revs = repo.revs(
            "parents(%ld) + %ld + %ld + %ld", drafts, drafts, heads, masterset
        )

    # Include the ancestor of above commits to make the graph connected.
    #
    # When calculating ancestors, filter commits using 'public()' to reduce the
    # number of commits to calculate. This is sound because the above logic
    # includes p1 of draft commits, and assume master is public. Practically,
    # this optimization can make a 3x difference.
    revs = smartset.baseset(
        repo.revs("ancestor(%ld & public()) + %ld", revs, revs), repo=repo
    )

    # Collapse long obsoleted stack - only keep their heads and roots.
    # This is incompatible with automation (namely, nuclide-core) yet.
    if repo.ui.configbool("smartlog", "collapse-obsolete") and not repo.ui.plain():
        obsrevs = smartset.baseset(repo.revs("%ld & obsolete()", revs), repo=repo)
        hiderevs = smartset.baseset(
            repo.revs("%ld - (heads(%ld) + roots(%ld))", obsrevs, obsrevs, obsrevs),
            repo=repo,
        )
        revs = repo.revs("(%ld - %ld) + %ld", revs, hiderevs, heads)

    return subset & revs


@revsetpredicate("draftbranch([set])")
def draftbranchrevset(repo, subset, x):
    """``draftbranch(set)``
    The draft branches containing the given changesets.
    """
    args = revset.getargs(x, 1, 1, _("draftbranch expects one argument"))
    revs = revset.getset(repo, subset, args[0])
    return subset & repo.revs("(draft() & ::%ld)::", revs)


@revsetpredicate("mutrelated([set])")
def mutrelatedrevset(repo, subset, x):
    """``mutrelated([set])``
    Draft changesets that are related via mutations.
    """
    args = revset.getargs(x, 1, 1, _("mutrelated expects one argument"))
    revs = revset.getset(repo, subset, args[0])
    return subset & repo.revs(
        "descendants((predecessors(%ld) + successors(%ld)) & not public())", revs, revs
    )


@revsetpredicate("focusedbranch([set])")
def focusedbranchrevset(repo, subset, x):
    """``focusedbranch([set])``
    The focused branches of the given changesets, being the draft
    stack and any draft changesets that are related via mutations.
    """
    args = revset.getargs(x, 1, 1, _("focusedbranch expects one argument"))
    revs = revset.getset(repo, subset, args[0])
    return subset & repo.revs(
        "draft() & mutrelated(draftbranch(%ld)) + %ld", revs, revs
    )


def smartlognodes(repo, headnodes, masternodes):
    """Calculate nodes based on new DAG abstraction.
    This function does not use revs or revsets.
    """
    # Use "- public()" so if the user specifies secret commits in headnodes
    # it will work as expected.
    draftnodes = repo.dageval(lambda: ancestors(headnodes) - public())
    nodes = repo.dageval(
        lambda: parents(draftnodes) | draftnodes | headnodes | masternodes
    )

    # Protection to avoid running into "very slow" cases. This does not
    # usually happen. But wrong visibleheads might trigger it (ex. large
    # draft() size). Note this is tested before "collapse-obsolete", because
    # "collapse-obsolete" can be very slow if there are too many nodes.
    limit = repo.ui.configint("smartlog", "max-commit-threshold")
    if len(nodes) > limit:
        repo.ui.warn(
            _("smartlog: too many (%d) commits, not rendering all of them\n")
            % (len(nodes),)
        )
        repo.ui.warn(
            _("(consider running '@prog@ doctor' to hide unrelated commits)\n")
        )
        nodes = nodes.take(limit) + headnodes

    # Include the ancestor of above commits to make the graph connected.
    nodes = repo.dageval(lambda: nodes | filter(None, [gcaone(nodes)]))

    # Collapse long obsoleted stack - only keep their heads and roots.
    # This is incompatible with automation (namely, nuclide-core) yet.
    if repo.ui.configbool("smartlog", "collapse-obsolete") and not repo.ui.plain():
        obsnodes = repo.dageval(lambda: nodes & obsolete())
        hidenodes = repo.dageval(lambda: obsnodes - heads(obsnodes) - roots(obsnodes))
        nodes = nodes - hidenodes + headnodes

    return nodes


def _ignorebookmarks(repo):
    return re.compile(repo.ui.config("smartlog", "ignorebookmarks", "!"))


@revsetpredicate("interestingbookmarks()")
def interestingheads(repo, subset, x):
    """Set of interesting bookmarks (local and remote)"""
    rev = repo.changelog.rev
    heads = set()
    books = bookmarks.bmstore(repo)
    ignore = _ignorebookmarks(repo)
    for b in books:
        if not ignore.match(b):
            heads.add(rev(books[b]))

    # add 'interesting' remote bookmarks as well
    if hasattr(repo, "names") and "remotebookmarks" in repo.names:
        ns = repo.names["remotebookmarks"]
        for name in _reposnames(repo):
            if ignore.match(name):
                continue
            nodes = ns.namemap(repo, name)
            if nodes:
                heads.add(rev(nodes[0]))

    return subset & smartset.baseset(heads, repo=repo)


@revsetpredicate("interestingmaster()")
def interestingmaster(repo, subset, x):
    """Interesting 'master' commit"""

    ignore = _ignorebookmarks(repo)
    names = set(bookmarks.bmstore(repo).keys())
    if hasattr(repo, "names") and "remotebookmarks" in repo.names:
        names.update(set(repo.names["remotebookmarks"].listnames(repo)))

    for name in _reposnames(repo):
        if ignore.match(name):
            continue
        if name in names:
            revs = repo.revs("%s", name)
            break
    else:
        revs = repo.revs("last(public())")

    return subset & revs


@templatefunc("simpledate(date[, tz])")
def simpledate(context, mapping, args):
    """Date.  Returns a human-readable date/time that is simplified for
    dates and times in the recent past.
    """
    ctx = mapping["ctx"]
    repo = ctx.repo()
    date_now = util.parsedate("now")
    date = templater.evalfuncarg(context, mapping, args[0])
    tz = None
    if len(args) == 2:
        tzname = templater.evalstring(context, mapping, args[1])
        if tzname:
            try:
                import zoneinfo

                tz = zoneinfo.ZoneInfo(tzname)
            except ImportError:
                msg = "Couldn't import ZoneInfo, using default time zone\n"
                repo.ui.warn(msg)
            except zoneinfo._common.ZoneInfoNotFoundError:
                msg = "Unknown time zone: %s\n" % tzname
                repo.ui.warn(msg)
    then = datetime.datetime.fromtimestamp(date[0], tz)
    now = datetime.datetime.fromtimestamp(date_now[0], tz)
    td = now.date() - then.date()
    if then > now:
        # Time is in the future, render it in full
        return then.strftime("%Y-%m-%d %H:%M")
    elif td.days == 0:
        # Today ("Today at HH:MM")
        return then.strftime("Today at %H:%M")
    elif td.days == 1:
        # Yesterday ("Yesterday at HH:MM")
        return then.strftime("Yesterday at %H:%M")
    elif td.days <= 6:
        # In the last week (e.g. "Monday at HH:MM")
        return then.strftime("%A at %H:%M")
    elif now.year == then.year or td.days <= 90:
        # This year or in the last 3 months (e.g. "Jan 05 at HH:MM")
        return then.strftime("%b %d at %H:%M")
    else:
        # Before, render it in full
        return then.strftime("%Y-%m-%d %H:%M")


@templatefunc("smartdate(date, threshold, recent, other)")
def smartdate(context, mapping, args):
    """Date.  Returns one of two values depending on whether the date provided
    is in the past and recent or not."""
    date = templater.evalfuncarg(context, mapping, args[0])
    threshold = templater.evalinteger(context, mapping, args[1])
    now = time.time()
    then = date[0]
    if now - threshold <= then <= now:
        return templater.evalstring(context, mapping, args[2])
    else:
        return templater.evalstring(context, mapping, args[3])


@hint("smartlog-default-command")
def hintsmartlogdefaultcommand():
    return _("you can run smartlog with simply `@prog@`")


@command(
    "smartlog|sl|slog|sm|sma|smar|smart|smartl|smartlo",
    [
        ("", "master", "", _("master bookmark"), _("BOOKMARK")),
        ("r", "rev", [], _("show the specified revisions or range"), _("REV")),
        ("", "all", False, _("don't hide old local changesets")),
        ("", "commit-info", False, _("show changes in current changeset")),
        (
            "i",
            "interactive",
            False,
            _("enter command line interactive smartlog (EXPERIMENTAL)"),
        ),
        (
            "t",
            "mutation",
            False,
            _("show the mutation history of the revisions instead"),
        ),
        (
            "",
            "show-missing",
            False,
            _("with --mutation, show commits not available locally"),
        ),
    ]
    + logopts,
    _("[OPTION]... [[-r] REV]"),
    cmdtype=registrar.command.readonly,
)
def smartlog(ui, repo, *pats, **opts):
    """show a graph of the commits that are relevant to you

    Includes:

    - Your local commits
    - The master bookmark for your repository
    - Any commits with local bookmarks

    Excludes:

    - All commits under master that aren't related to your commits
    - Your local commits that are older than a specified date

    ``--commit-info`` shows status-like file changes for the current commit, or
    all drafts when using ``--verbose``. Note this is not the working copy
    ``status`` and only works with default templates.

    ``--stat` shows ``diff --stat``-like file changes for all drafts.

    ``--mutation`` (``-t``) shows the mutation history of REV (by default, of
    the current commit) instead, as in :prog:`sl -t`: :prog:`log --graph
    --mutation` of ``mutationhistory(REV)``, with a line before each commit
    saying how it was created. Hidden commits that were created from them
    are shown with ``--hidden``, and commits that are not available locally
    with ``--show-missing``.
    """

    maybe_hint_about_default_command(ui)

    if opts.get("mutation"):
        if opts.get("interactive"):
            raise error.Abort(_("--mutation cannot be used with --interactive"))
    elif opts.get("show_missing"):
        raise error.Abort(_("--show-missing requires --mutation"))

    overrides = {}
    if opts.get("mutation"):
        overrides[("templatealias", "sl_show_mutatedfrom")] = "'true'"
    if opts.get("commit_info"):
        if ui.verbose:
            config = "'true'"
        else:
            config = "\"{ifeq(graphnode, '@', 'true')}\""
        overrides[("templatealias", "sl_show_file_change_summary")] = config
    with ui.configoverride(overrides):
        return _smartlog(ui, repo, *pats, **opts)


def maybe_hint_about_default_command(ui):
    # Make sure the default command is configured to run smartlog.
    if ui.config("commands", "naked-default.in-repo") != "sl":
        return

    # Make sure the user typed "... sl" or "... smartlog".
    if len(sys.argv) < 2 or sys.argv[-1] not in {"sl", "smartlog"}:
        return

    hintutil.trigger("smartlog-default-command")


def getrevs(ui, repo, masterstring, headrevs):
    if headrevs:
        headspec = revsetlang.formatspec("%lr", headrevs)
    else:
        headspec = "interestingbookmarks() + heads(draft()) + ."

    revstring = revsetlang.formatspec(
        "smartlog(heads=%r, master=%r)", headspec, masterstring
    )

    revs = set(repo.anyrevs([revstring], user=True))

    if -1 in revs:
        revs.remove(-1)

    return revs


if interactiveui is not None:

    @dataclass
    class _visiblecommitrender:
        ctx: object
        current_lines: list
        width: int
        revcache: dict
        matchfn: object
        full_span: tuple
        message_column: int
        normal_lines: list
        current_message_lines: list
        highlighted_lines: list = field(default=None)

    class interactivesmartlog(interactiveui.viewframe):
        def __init__(self, ui, repo, masterstring, headrevs, template, opts):
            super().__init__(ui, repo)
            self.masterstring = masterstring
            self.template = template
            self.headrevs = headrevs
            self.opts = opts
            self.dag_index = 0
            self._compute_graph()
            self.status = ""
            self.rebase_source = None

        # Compute new index of ctx in revdag
        # TODO: Handle case where ctx is no longer in revdag. Currently should be impossible but could happen if we change what the selected_node is when rebasing.
        def _new_index(self, ctx):
            return [node[2] for node in self.revdag].index(ctx)

        def _compute_graph(self):
            revs = getrevs(self.ui, self.repo, self.masterstring, self.headrevs)
            if len(revs) == 0:
                self.finish()
            self.revdag, self.reserved = getdag(
                self.ui, self.repo, sorted(revs), self.masterstring, self.template
            )
            self._render_cache = None

        def render(self):
            ui = self.ui
            ui.pushbuffer()
            # Print it!
            displayer = cmdutil.show_changeset(
                self.ui, self.repo, self.opts, buffered=True
            )

            current_line = 0
            selected_rows = None
            visible_commits = {}
            visible_indices = {}

            def on_output(ctx, output, metadata=None):
                nonlocal current_line
                nonlocal selected_rows
                metadata = metadata or {}
                lines = output.splitlines()
                height = len(lines)
                start_line = current_line
                selected_ctx = self.revdag[self.dag_index][2]
                if ctx == selected_ctx:
                    # start and end indices (inclusive)
                    selected_rows = (start_line, start_line + height - 1)
                message_column = 0
                current_message_lines = metadata.get("message", "").splitlines()
                for row_index, message_line in enumerate(current_message_lines):
                    if not message_line or row_index >= len(lines):
                        continue
                    prefix_pos = lines[row_index].find(message_line)
                    if prefix_pos != -1:
                        message_column = prefix_pos
                        break
                visible_commits[ctx] = _visiblecommitrender(
                    ctx=ctx,
                    current_lines=lines,
                    width=metadata.get("width", 0),
                    revcache=metadata.get("revcache", {}),
                    matchfn=metadata.get("matchfn"),
                    full_span=(start_line, start_line + height),
                    message_column=message_column,
                    normal_lines=None if ctx == selected_ctx else lines,
                    current_message_lines=current_message_lines,
                    highlighted_lines=lines if ctx == selected_ctx else None,
                )
                current_line += height

            cmdutil.displaygraph(
                self.ui,
                self.repo,
                self.revdag,
                displayer,
                reserved=self.reserved,
                props={"highlighted_node": self.revdag[self.dag_index][2].hex()},
                on_output=on_output,
            )
            output = ui.popbuffer().splitlines()
            alignment = None
            if selected_rows is not None:
                alignment = (selected_rows[1], interactiveui.Alignment.bottom)
            renderstate = interactiveui.getrenderstate(self, output, alignment)
            for index, node in enumerate(self.revdag):
                ctx = node[2]
                commitrender = visible_commits.get(ctx)
                if commitrender is None:
                    continue
                span_start, span_end = commitrender.full_span
                if (
                    span_start < renderstate.visible_start
                    or span_end > renderstate.visible_end
                ):
                    continue
                visible_indices[index] = ctx
                visible_commits[ctx] = replace(
                    commitrender,
                    full_span=(
                        span_start - renderstate.visible_start,
                        span_end - renderstate.visible_start,
                    ),
                )
            self._render_cache = {
                "renderstate": renderstate,
                "visible_commits": visible_commits,
                "visible_indices": visible_indices,
                "status": self.status,
            }
            return output, alignment

        def _render_message_variant(self, commitrender, highlighted):
            displayer = cmdutil.show_changeset(
                self.ui, self.repo, self.opts, buffered=True
            )
            props = {}
            if highlighted:
                props["highlighted_node"] = commitrender.ctx.hex()
            displayer.show(
                commitrender.ctx,
                revcache=commitrender.revcache,
                matchfn=commitrender.matchfn,
                _graphwidth=commitrender.width,
                **props,
            )
            msg_lines = [
                s if isinstance(s, str) else s.decode(errors="replace")
                for s in displayer.hunk.pop(commitrender.ctx.rev())
            ]
            displayer.flush(commitrender.ctx)
            displayer.close()
            return "".join(msg_lines).splitlines()

        def _build_variant(self, commitrender, message_lines):
            built_lines = []
            for index, current_line in enumerate(commitrender.current_lines):
                if index >= len(commitrender.current_message_lines):
                    built_lines.append(current_line)
                    continue
                source_message = commitrender.current_message_lines[index]
                if source_message:
                    prefix_pos = current_line.find(source_message)
                    if prefix_pos == -1:
                        built_lines.append(current_line)
                        continue
                    prefix = current_line[:prefix_pos]
                    suffix = current_line[prefix_pos + len(source_message) :]
                else:
                    prefix = current_line.rstrip()
                    if len(prefix) < commitrender.message_column:
                        prefix = prefix.ljust(commitrender.message_column)
                    suffix = ""
                new_message = message_lines[index] if index < len(message_lines) else ""
                if new_message:
                    if source_message:
                        built_lines.append(prefix + new_message + suffix)
                    else:
                        built_lines.append(prefix + new_message + suffix)
                else:
                    built_lines.append((prefix + suffix).rstrip())
            return built_lines

        def _ensure_variant(self, commitrender, highlighted):
            if highlighted:
                if commitrender.highlighted_lines is None:
                    message_lines = self._render_message_variant(commitrender, True)
                    commitrender.highlighted_lines = self._build_variant(
                        commitrender, message_lines
                    )
                return commitrender.highlighted_lines
            if commitrender.normal_lines is None:
                message_lines = self._render_message_variant(commitrender, False)
                commitrender.normal_lines = self._build_variant(
                    commitrender, message_lines
                )
            return commitrender.normal_lines

        def _maybe_partial_move(self, old_index, new_index):
            render_cache = self._render_cache
            if render_cache is None:
                return False
            if self.status != render_cache["status"]:
                return False
            renderstate = render_cache["renderstate"]
            width, height = scmutil.termsize(self.ui)
            if width != renderstate.width or height != renderstate.height:
                return False
            if old_index == new_index:
                return False
            old_ctx = render_cache["visible_indices"].get(old_index)
            new_ctx = render_cache["visible_indices"].get(new_index)
            if old_ctx is None or new_ctx is None:
                return False
            old_render = render_cache["visible_commits"].get(old_ctx)
            new_render = render_cache["visible_commits"].get(new_ctx)
            if old_render is None or new_render is None:
                return False
            updates = []
            for commitrender, highlighted in (
                (old_render, False),
                (new_render, True),
            ):
                lines = self._ensure_variant(commitrender, highlighted)
                start, _end = commitrender.full_span
                for row_offset, line in enumerate(lines):
                    screen_row = start + row_offset
                    render_cache["renderstate"].visible_lines[screen_row] = line
                    updates.append((screen_row, line))
            interactiveui.rewrite_rows(self, updates)
            return True

        def _flushmovement(self, delta):
            if delta == 0:
                return False
            old_index = self.dag_index
            self.dag_index = min(max(self.dag_index + delta, 0), len(self.revdag) - 1)
            return self.dag_index != old_index

        def _handlenonmovementkeypress(self, key):
            if key == self.KEY_Q:
                self.finish()
                return False
            if key == self.KEY_RETURN:
                self.ui.pushbuffer(error=True)
                selected_ctx = self.revdag[self.dag_index][2]
                try:
                    if self.rebase_source is None:
                        # Equivalent to `hg update selected_ctx`
                        commands.update(self.ui, self.repo, selected_ctx.hex())
                    else:
                        # Equivalent to `hg rebase -s self.rebase_source -d selected_ctx`
                        rebase.rebase(
                            self.ui,
                            self.repo,
                            source=self.rebase_source[2].hex(),
                            dest=selected_ctx.hex(),
                        )
                        self.rebase_source = None
                        self._compute_graph()
                        self.dag_index = self._new_index(selected_ctx)
                except Exception as ex:
                    self.ui.write_err("operation failed: %s\n" % ex)
                self.status = self.ui.popbuffer()
                return self._active
            if key == self.KEY_R:
                self.rebase_source = self.revdag[self.dag_index]
                self.status = _("rebasing from %s") % (self.rebase_source[2])
                return True
            if key == self.KEY_S:
                bindings.commands.run(
                    util.hgcmd()
                    + [
                        "show",
                        self.revdag[self.dag_index][2].hex(),
                        "--config",
                        "pager.interface=fullscreen",
                    ]
                )
                return True
            if key == self.KEY_SHIFT_H:
                bindings.commands.run(
                    util.hgcmd()
                    + [
                        "hide",
                        self.revdag[self.dag_index][2].hex(),
                    ]
                )
                # I couldn't figure out how to make the graph refresh so will just end
                self.finish()
                return False
            return False

        def handlekeypress(self, key):
            if key == self.KEY_J or key == self.KEY_DOWN:
                self._flushmovement(1)
                return
            if key == self.KEY_K or key == self.KEY_UP:
                self._flushmovement(-1)
                return
            self._handlenonmovementkeypress(key)

        def handlekeypresses(self, keys):
            redraw = False
            movement = 0
            for key in keys:
                if key == self.KEY_J or key == self.KEY_DOWN:
                    movement += 1
                    continue
                if key == self.KEY_K or key == self.KEY_UP:
                    movement -= 1
                    continue
                redraw = self._flushmovement(movement) or redraw
                movement = 0
                redraw = self._handlenonmovementkeypress(key) or redraw
                if not self._active:
                    return False
            if movement != 0:
                old_index = self.dag_index
                moved = self._flushmovement(movement)
                if (
                    moved
                    and not redraw
                    and self._maybe_partial_move(old_index, self.dag_index)
                ):
                    moved = False
                redraw = moved or redraw
            return redraw


def _smartlog(ui, repo, *pats, **opts):
    masterfallback = "interestingmaster()"

    masterstring = (
        opts.get("master") or ui.config("smartlog", "master") or masterfallback
    )

    template = opts.get("template") or ""
    headrevs = opts.get("rev")

    if opts.get("mutation"):
        revs = list(headrevs or []) + list(pats)
        if revs:
            revs = [revsetlang.formatspec("mutationhistory(%lr)", revs)]
        logopts = dict(opts, rev=revs, graph=True)
        cmdutil.graphlog(ui, repo, (), logopts)
        return

    if opts.get("interactive"):
        if interactiveui is None:
            raise error.Abort(_("interactive ui is not supported on Windows"))

        viewobj = interactivesmartlog(ui, repo, masterstring, headrevs, template, opts)
        if util.istest():
            input_str = ui.fin.read()
            if isinstance(input_str, str):
                input_str = input_str.encode()
            input_str = input_str.replace(b"\n", b"")
            index = 0
            special_keys = (
                interactiveui.viewframe.KEY_UP,
                interactiveui.viewframe.KEY_DOWN,
                interactiveui.viewframe.KEY_RIGHT,
                interactiveui.viewframe.KEY_LEFT,
            )

            def getchar():
                nonlocal input_str
                nonlocal index
                if index >= len(input_str):
                    return None
                for key in special_keys:
                    if input_str.startswith(key, index):
                        index += len(key)
                        return key
                ch = input_str[index : index + 1]
                index += 1
                return ch

            interactiveui.view(viewobj, getchar)
        else:
            interactiveui.view(viewobj)
        return

    revs = getrevs(ui, repo, masterstring, headrevs)

    if len(revs) == 0:
        return
    # Print it!
    revdag, reserved = getdag(ui, repo, sorted(revs), masterstring, template)
    displayer = cmdutil.show_changeset(ui, repo, opts, buffered=True)
    ui.pager("smartlog")
    cmdutil.displaygraph(ui, repo, revdag, displayer, reserved=reserved)

    try:
        with open(repo.localvfs.join("completionhints"), "w") as f:
            for rev in revdag:
                commit_hash = rev[2].node()
                f.write(nodemod.short(commit_hash) + "\n")
    except IOError:
        # No write access. No big deal.
        pass
