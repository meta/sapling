# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# fbhistedit.py - improved amend functionality
"""extends the existing histedit functionality

Adds a s/stop verb to histedit to stop after a changeset was picked.
"""

import os

from sapling import (
    cmdutil,
    error,
    extensions,
    hg,
    json,
    lock,
    merge as mergemod,
    mergeutil,
    node,
    registrar,
    scmutil,
)
from sapling.i18n import _

cmdtable = {}
command = registrar.command(cmdtable)

testedwith = "ships-with-fb-ext"


def defineactions():
    histedit = extensions.find("histedit")

    @histedit.action(
        ["stop", "s"], _("pick changeset, and stop after committing changes")
    )
    class stop(histedit.histeditaction):
        def run(self):
            parentctx, replacements = super(stop, self).run()
            self.state.read()
            self.state.replacements.extend(replacements)
            self.state.write()
            raise error.InterventionRequired(
                _(
                    "Changes committed as %s. You may amend the changeset now.\n"
                    "When you are done, run @prog@ histedit --continue to resume"
                )
                % parentctx
            )

        def continueclean(self):
            self.state.replacements = [
                (n, r) for (n, r) in self.state.replacements if n != self.node
            ]
            return super(stop, self).continueclean()

    @histedit.action(["exec", "x"], _("execute given command"))
    class execute(histedit.histeditaction):
        def __init__(self, state, command):
            self.state = state
            self.repo = state.repo
            self.command = command
            self.cwd = state.repo.root
            self.node = None

        @classmethod
        def fromrule(cls, state, rule):
            """Parses the given rule, returns an instance of the histeditaction."""
            command = rule
            return cls(state, command)

        def torule(self, *args, **kwargs):
            return "%s %s" % (self.verb, self.command)

        def tostate(self):
            """Print an action in format used by histedit state files
            (the first line is a verb, the remainder is the second)
            """
            return "%s\n%s" % (self.verb, self.command)

        def verify(self, *args, **kwds):
            pass

        def constraints(self):
            return set()

        def nodetoverify(self):
            return None

        def run(self):
            state = self.state
            repo, ctxnode = state.repo, state.parentctxnode
            with repo.wlock(), repo.lock(), repo.transaction("histedit"):
                hg.update(repo, ctxnode)

            # release locks so the program can call hg and then relock.
            lock.release(state.lock, state.wlock)

            try:
                ctx = repo[ctxnode]
                cmd = self.command
                rc = repo.ui.system(
                    cmd,
                    environ={"HGNODE": ctx.hex()},
                    cwd=self.cwd,
                    blockedtag="histedit_exec",
                )
            except OSError as ose:
                raise error.InterventionRequired(
                    _("Cannot execute command '%s': %s") % (self.command, ose)
                )
            finally:
                # relock the repository
                state.wlock = repo.wlock()
                state.lock = repo.lock()
                repo.invalidateall()

            if rc != 0:
                raise error.InterventionRequired(
                    _("Command '%s' failed with exit status %d") % (self.command, rc)
                )

            m, a, r, d = self.repo.status()[:4]
            if m or a or r or d:
                self.continuedirty()
            return self.continueclean()

        def continuedirty(self):
            raise error.Abort(
                _("working copy has pending changes"),
                hint=_(
                    "amend, commit, or revert them and run histedit "
                    "--continue/--retry, or abort with histedit --abort"
                ),
            )

        def continueclean(self):
            newctx = self.repo["."]
            return newctx, []

    @histedit.action(
        ["execr", "xr"], _("execute given command relative to current directory")
    )
    class executerelative(execute):
        def __init__(self, state, command):
            super(executerelative, self).__init__(state, command)
            self.cwd = os.getcwd()

    @histedit.action(["graft", "g"], _("graft a commit from elsewhere"))
    class graft(histedit.histeditaction):
        def _verifynodeconstraints(self, prev, expected, seen):
            if self.node in expected:
                msg = _('%s "%s" changeset was an edited list candidate')
                raise error.ParseError(
                    msg % (self.verb, node.short(self.node)),
                    hint=_("graft must only use unlisted changesets"),
                )

        def continueclean(self):
            ctx, replacement = super(graft, self).continueclean()
            return ctx, []

    return stop, execute, executerelative


def extsetup(ui):
    try:
        extensions.find("histedit")
    except KeyError:
        raise error.Abort(_("fbhistedit: please enable histedit extension as well"))

    defineactions()
    _extend_histedit(ui)

    rebase = extensions.find("rebase")
    extensions.wrapcommand(rebase.cmdtable, "rebase", _rebase, synopsis="[-i]")
    aliases, entry = cmdutil.findcmd("rebase", rebase.cmdtable)
    newentry = list(entry)
    options = newentry[1]
    # dirty hack because we need to change an existing switch
    for idx, opt in enumerate(options):
        if opt[0] == "i":
            del options[idx]
    options.append(("i", "interactive", False, "interactive rebase"))
    rebase.cmdtable["rebase"] = tuple(newentry)


def _extend_histedit(ui):
    histedit = extensions.find("histedit")

    _aliases, entry = cmdutil.findcmd("histedit", histedit.cmdtable)
    options = entry[1]
    options.append(
        ("x", "retry", False, _("retry exec command that failed and try to continue"))
    )
    options.append(
        (
            "",
            "show-plan",
            False,
            _("show remaining actions, or the starting plan if no histedit is running"),
        )
    )

    extensions.wrapfunction(histedit, "_histedit", _histedit)
    extensions.wrapfunction(histedit, "parserules", parserules)


def parserules(orig, rules, state):
    try:
        rules = _parsejsonrules(rules, state)
    except ValueError:
        pass
    return orig(rules, state)


def _parsejsonrules(rules, state):
    jsondata = json.loads(rules)

    parsedrules = ""
    try:
        for entry in jsondata["histedit"]:
            if entry["action"] in set(["exec", "execr"]):
                rest = entry["command"]
            else:
                rest = entry["node"]
            parsedrules += entry["action"] + " " + rest + "\n"
    except KeyError:
        state.repo.ui.status(_("invalid JSON format, falling back to normal parsing\n"))
        return rules

    return parsedrules


goalretry = "retry"
goalshowplan = "show-plan"
goalorig = "orig"


def _getgoal(opts):
    if opts.get("retry"):
        return goalretry
    if opts.get("show_plan"):
        return goalshowplan
    return goalorig


def _validateargs(ui, repo, state, freeargs, opts, goal, rules, revs):
    # basic argument incompatibility processing
    outg = opts.get("outgoing")
    editplan = opts.get("edit_plan")
    abort = opts.get("abort")

    plan = opts.get("plan")

    if goal == goalretry:
        if any((outg, abort, revs, freeargs, rules, editplan, plan)):
            raise error.Abort(_("no arguments allowed with --retry"))
    elif goal == goalshowplan:
        if any((outg, abort, rules, editplan, plan)) or (
            (revs or freeargs) and state.inprogress()
        ):
            raise error.Abort(_("no arguments allowed with --show-plan"))
    elif goal == goalorig:
        # We explicitly left the validation of arguments to orig
        pass


def _histedit(orig, ui, repo, state, *freeargs, **opts):
    histedit = extensions.find("histedit")

    goal = _getgoal(opts)
    revs = opts.get("rev", [])
    rules = opts.get("commands", "")
    state.keep = opts.get("keep", False)

    _validateargs(ui, repo, state, freeargs, opts, goal, rules, revs)

    if goal == goalretry:
        state.read()
        state = bootstrapretry(ui, state, opts)

        histedit._continuehistedit(ui, repo, state)
        histedit._finishhistedit(ui, repo, state)
    elif goal == goalshowplan:
        if state.inprogress():
            state.read()
            showplan(ui, state)
        else:
            showstartplan(ui, repo, state, freeargs, opts)
    else:
        return orig(ui, repo, state, *freeargs, **opts)


def bootstrapretry(ui, state, opts):
    repo = state.repo

    ms = mergemod.mergestate.read(repo)
    mergeutil.checkunresolved(ms)

    if not state.actions or state.actions[0].verb != "exec":
        msg = _("no exec in progress")
        hint = _(
            "if you want to continue a non-exec histedit command"
            ' use "histedit --continue" instead.'
        )
        raise error.Abort(msg, hint=hint)

    if repo[None].dirty(missing=True):
        raise error.Abort(
            _("working copy has pending changes"),
            hint=_(
                "amend, commit, or revert them and run histedit "
                "--retry, or abort with histedit --abort"
            ),
        )

    return state


def showstartplan(ui, repo, state, freeargs, opts) -> None:
    """Print the plan a new histedit would start with, without running it."""
    histedit = extensions.find("histedit")
    revs = list(opts.get("rev", []))
    histedit._validateargs(ui, repo, state, freeargs, opts, "new", "", revs)
    root, topmost, nodes = histedit.histeditrevs(repo, state, revs)
    ui.write(
        _(
            "histedit plan for %s to %s (to run it, pass each line to"
            ' "histedit --plan", or the whole plan file to "histedit --commands"):\n'
        )
        % (node.short(root), node.short(topmost))
    )
    for n in nodes:
        ui.write("    %s\n" % histedit.pick(state, n).torule())


def showplan(ui, state):
    if not state.actions:
        msg = _("no histedit actions in progress")
        hint = _('did you meant to run histedit without "--show-plan"?')
        raise error.Abort(msg, hint=hint)

    ui.write(
        _(
            'histedit plan (call "histedit --continue/--retry" to resume it'
            ' or "histedit --abort" to abort it):\n'
        )
    )
    for action in state.actions:
        ui.write("    %s\n" % action.torule())


def _rebase(orig, ui, repo, *pats, **opts):
    histedit = extensions.find("histedit")

    contf = opts.get("continue")
    abortf = opts.get("abort")

    if (
        (contf or abortf)
        and not repo.localvfs.exists("rebasestate")
        and repo.localvfs.exists("histedit.state")
    ):
        msg = _("no rebase in progress")
        hint = _(
            "If you want to continue or abort an interactive rebase please"
            ' use "histedit --continue/--abort" instead.'
        )
        raise error.Abort(msg, error_category=error.USAGE_ERROR, hint=hint)

    if not opts.get("interactive"):
        return orig(ui, repo, *pats, **opts)

    # the argument parsing has as lot of copy-paste from rebase.py
    # Validate input and define rebasing points
    destf = opts.get("dest", None)
    srcf = opts.get("source", None)
    basef = opts.get("base", None)
    revf = opts.get("rev", [])
    keepf = opts.get("keep", False)

    src = None

    if contf or abortf:
        raise error.Abort("no interactive rebase in progress")
    if len(destf) == 1:
        dest = scmutil.revsingle(repo, destf[0])
    elif len(destf) > 1:
        raise error.Abort(
            "multiple --dest is incompatible with intereactive rebase (histedit)"
        )
    else:
        raise error.Abort("you must specify a destination (-d) for the rebase")

    if srcf and basef:
        raise error.Abort(_("cannot specify both a source and a base"))
    if revf:
        raise error.Abort("--rev not supported with interactive rebase")
    elif srcf:
        src = scmutil.revsingle(repo, srcf)
    else:
        base = scmutil.revrange(repo, [basef or "."])
        if not base:
            ui.status(_('empty "base" revision set - can\'t compute rebase set\n'))
            return 1
        commonanc = repo.revs("ancestor(%ld, %d)", base, dest).first()
        if commonanc is not None:
            src = repo.revs(
                "min((%d::(%ld) - %d)::)", commonanc, base, commonanc
            ).first()
        else:
            src = None

    if src is None:
        raise error.Abort("no revisions to rebase")
    src = repo[src].node()

    topmost, empty = repo.dirstate.parents()
    revs = histedit.between(repo, src, topmost, keepf)

    if srcf and not revs:
        raise error.Abort(
            _(
                "source revision (-s) must be an ancestor of the "
                "working directory for interactive rebase"
            )
        )

    ctxs = [repo[r] for r in revs]
    state = histedit.histeditstate(repo)
    rules = [histedit.base(state, repo[dest])] + [
        histedit.pick(state, ctx) for ctx in ctxs
    ]
    editcomment = """#
# Interactive rebase is just a wrapper over histedit (adding the 'base' line as
# the first rule). To continue or abort it you should use:
# "hg histedit --continue" and "--abort"
#
"""
    editcomment += histedit.geteditcomment(ui, node.short(src), node.short(topmost))
    histedit.ruleeditor(repo, ui, rules, editcomment=editcomment)

    return histedit.histedit(
        ui,
        repo,
        node.hex(src),
        keep=keepf,
        commands=repo.localvfs.join("histedit-last-edit.txt"),
    )
