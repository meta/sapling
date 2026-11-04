# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# movement.py - commands to move working parent like previous, next, etc.


from itertools import count

from sapling import (
    bookmarks,
    cmdutil,
    commands,
    error,
    hintutil,
    registrar,
    revsetlang,
    scmutil,
    smartset,
)
from sapling.i18n import _
from sapling.node import hex, nullrev, short

from . import common, revsets

_MULTI_CHILDREN_HINT = _(
    "use the --newest flag to always pick the newest child at each step"
)

cmdtable = {}
command = registrar.command(cmdtable)

revsetpredicate = revsets.revsetpredicate

moveopts = [
    ("C", "clean", False, _("discard uncommitted changes (no backup)")),
    ("B", "move-bookmark", False, _("move active bookmark")),
    ("m", "merge", False, _("merge uncommitted changes")),
    ("c", "check", False, _("require clean working directory")),
]


@command(
    "previous|prev",
    [
        (
            "",
            "newest",
            False,
            _("always pick the newest parent when a commit has multiple parents"),
        ),
        (
            "",
            "bottom",
            False,
            _("update to the lowest non-public ancestor of the current commit"),
        ),
        ("", "bookmark", False, _("update to the first ancestor with a bookmark")),
        (
            "",
            "no-activate-bookmark",
            False,
            _("do not activate the bookmark on the destination commit"),
        ),
    ]
    + moveopts,
    _("[OPTIONS]... [STEPS]"),
    # @lint-ignore SPELL
    legacyaliases=["previ", "previo", "previou"],
)
def previous(ui, repo, *args, **opts):
    """check out an ancestor commit

    Update to an ancestor commit of the current commit. When working with a stack
    of commits, you can use :prog:`previous` to move down your stack with ease.

    - Use the ``--newest`` flag to always pick the newest of multiple parents commits.
      You can set ``amend.alwaysnewest`` to true in your global @Product@ config file to make
      this the default.

    - Use the ``--merge`` flag to bring along uncommitted changes to the destination
      commit.

    - Use the ``--bookmark`` flag to move to the first ancestor commit with a bookmark.

    Examples:

    - Move 1 level down the stack::

        @prog@ prev

    - Move 2 levels down the stack::

        @prog@ prev 2

    - Move to the bottom of the stack::

        @prog@ prev --bottom
    """
    _moverelative(ui, repo, args, opts, reverse=True)


@command(
    "next",
    [
        (
            "",
            "newest",
            False,
            _("always pick the newest child when a commit has multiple children"),
        ),
        ("", "rebase", False, _("rebase each commit if necessary")),
        ("", "top", False, _("update to the head of the current stack")),
        ("", "bookmark", False, _("update to the first commit with a bookmark")),
        (
            "",
            "no-activate-bookmark",
            False,
            _("do not activate the bookmark on the destination commit"),
        ),
        ("", "towards", "", _("move linearly towards the specified head")),
    ]
    + moveopts,
    _("[OPTIONS]... [STEPS]"),
    legacyaliases=["n", "ne", "nex"],
)
def next_(ui, repo, *args, **opts):
    """check out a descendant commit

    Update to a descendant commit of the current commit. When working with a stack
    of commits, you can use :prog:`next` to move up your stack with ease.

    - Use the ``--newest`` flag to always pick the newest of multiple child commits.
      You can set ``amend.alwaysnewest`` to true in your global @Product@ config file
      to make this the default.

    - Use the ``--merge`` flag to bring along uncommitted changes to the destination
      commit.

    - Use the ``--bookmark`` flag to move to the next commit with a bookmark.

    - Use the ``--rebase`` flag to rebase any child commits that were left behind
      after ``amend``, ``split``, ``fold``, or ``histedit``.

    Examples:

    - Move 1 level up the stack::

        @prog@ next

    - Move 2 levels up the stack::

        @prog@ next 2

    - Move to the top of the stack::

        @prog@ next --top
    """
    _moverelative(ui, repo, args, opts, reverse=False)


def _moverelative(ui, repo, args, opts, reverse=False):
    """Update to a commit relative to the current commit.
    Implements both `@prog@ previous` and `@prog@ next`.

    Takes in a list of positional arguments and a dict of command line
    options. (See help for `@prog@ previous` and `@prog@ next` to see which
    arguments and flags are supported.)

    Moves forward through history by default -- the behavior of `@prog@ next`.
    Setting reverse=True will change the behavior to that of `@prog@ previous`.
    """
    # Parse positional argument.
    try:
        n = int(args[0]) if args else 1
    except ValueError:
        raise error.Abort(_("argument must be an integer"))
    if n <= 0:
        return

    prev_steps_threshold = ui.configint("experimental", "prev.steps-threshold", 1000)
    if reverse and n >= prev_steps_threshold:
        hintutil.triggershow(ui, "prev-steps-threshold", n)

    if ui.configbool("amend", "alwaysnewest") and not ui.interactive():
        opts["newest"] = True

    # Check that the given combination of arguments is valid.
    if args:
        if opts.get("bookmark", False):
            raise error.Abort(_("cannot use both number and --bookmark"))
        if opts.get("top", False):
            raise error.Abort(_("cannot use both number and --top"))
        if opts.get("bottom", False):
            raise error.Abort(_("cannot use both number and --bottom"))
    if opts.get("bookmark", False):
        if opts.get("top", False):
            raise error.Abort(_("cannot use both --top and --bookmark"))
        if opts.get("bottom", False):
            raise error.Abort(_("cannot use both --bottom and --bookmark"))
    if opts.get("towards", False) and opts.get("top", False):
        raise error.Abort(_("cannot use both --top and --towards"))
    if opts.get("merge", False) and opts.get("rebase", False):
        raise error.Abort(_("cannot use both --merge and --rebase"))

    # Check if there is an outstanding operation.
    cmdutil.checkunfinished(repo)

    # If we have both --clean and --rebase, we need to discard any outstanding
    # changes now before we attempt to perform any rebases.
    if opts.get("clean") and opts.get("rebase"):
        # prev/next print their own destination line via _showchangesets.
        with ui.configoverride({("checkout", "show-destination"): False}, "movement"):
            commands.update(ui, repo, rev=repo["."].hex(), clean=True)

    with repo.wlock(), repo.lock():
        # Record the active bookmark, if any.
        bookmark = repo._activebookmark
        noactivate = opts.get("no_activate_bookmark", False)
        movebookmark = opts.get("move_bookmark", False)

        with repo.transaction("moverelative") as tr:
            # Find the desired commit. May potentially perform rebase.
            try:
                target = _findtarget(ui, repo, n, opts, reverse)
            except error.InterventionRequired:
                # Rebase failed. Need to manually close transaction to allow
                # `hg rebase --continue` to work correctly.
                tr.close()
                raise

            # Move the active bookmark if necessary. Needs to happen before
            # we update to avoid getting a 'leaving bookmark X' message.
            if movebookmark and bookmark is not None:
                _setbookmark(repo, tr, bookmark, target)

            # Update to the target commit. _showchangesets below prints the
            # destination, so suppress goto's own destination line.
            with ui.configoverride(
                {("checkout", "show-destination"): False}, "movement"
            ):
                commands.update(
                    ui,
                    repo,
                    rev=hex(target),
                    clean=opts.get("clean", False),
                    merge=opts.get("merge", False),
                    check=opts.get("check", False),
                )

            # Print out the commit we landed on.
            _showchangesets(ui, repo, nodes=[target])

            # Activate the bookmark on the new commit.
            if not noactivate and not movebookmark:
                _activate(ui, repo, target)


def _findtarget(ui, repo, n, opts, reverse):
    """Find the appropriate target commit for `@prog@ previous` and
    `@prog@ next` based on the provided options. May rebase the traversed
    commits if the rebase option is given in the opts dict.
    """
    towards = opts.get("towards")
    newest = opts.get("newest", False)
    bookmark = opts.get("bookmark", False)
    rebase = opts.get("rebase", False)
    top = opts.get("top", False)
    bottom = opts.get("bottom", False)
    nextpreferdraft = ui.configbool("update", "nextpreferdraft")

    if top and not rebase:
        # If we're not rebasing, jump directly to the top instead of
        # walking up the stack.
        return _findstacktop(ui, repo, newest)
    elif bottom:
        return _findstackbottom(
            ui,
            repo,
        )
    elif reverse:
        return _findprevtarget(ui, repo, n, bookmark, newest)
    else:
        return _findnexttarget(
            ui,
            repo,
            n,
            bookmark,
            newest,
            rebase,
            top,
            towards,
            nextpreferdraft,
        )


def _findprevtarget(ui, repo, n=None, bookmark=False, newest=False):
    """Get the revision n levels down the stack from the current revision.
    If newest is True, if a commit has multiple parents the newest
    will always be chosen. Otherwise, throws an exception.
    """
    ctx = repo["."]

    # The caller must specify a stopping condition -- either a number
    # of steps to walk or a bookmark to search for.
    if not n and not bookmark:
        raise error.Abort(_("no stop condition specified"))

    for i in count(0):
        # Loop until we're gone the desired number of steps, or we reach a
        # node with a bookmark if the bookmark option was specified.
        if bookmark:
            if i > 0 and ctx.bookmarks():
                break
        elif i >= n:
            break

        parents = ctx.parents()

        # Is this the root of the current branch?
        if not parents or parents[0].rev() == nullrev:
            if ctx.rev() == repo["."].rev():
                raise error.Abort(_("current commit has no parents"))
            ui.status(_("reached root commit\n"))
            break

        # Are there multiple parents?
        if len(parents) > 1 and not newest:
            ui.status(
                _("commit %s has multiple parents, namely:\n") % short(ctx.node())
            )
            parents = _showchangesets(
                ui, repo, contexts=parents, indices=ui.interactive()
            )
            if ui.interactive():
                ctx = _choosenode(ui, parents)
            else:
                raise error.Abort(
                    _("ambiguous previous commit"),
                    hint=_(
                        "use the --newest flag to always "
                        "pick the newest parent at each step"
                    ),
                )
        else:
            # Get the parent with the highest revision number.
            ctx = max(parents, key=lambda x: x.rev())

    return ctx.node()


def _findnexttarget(
    ui,
    repo,
    n=None,
    bookmark=False,
    newest=False,
    rebase=False,
    top=False,
    towards=None,
    preferdraft=False,
):
    """Get the revision n levels up the stack from the current revision.
    If newest is True, if a commit has multiple children the newest
    will always be chosen. Otherwise, throws an exception. If the rebase
    option is specified, potentially rebase unstable children as we
    walk up the stack.
    """
    node = repo["."].node()

    # The caller must specify a stopping condition -- either a number
    # of steps to walk, a bookmark to search for, or --top.
    if not n and not bookmark and not top:
        raise error.Abort(_("no stop condition specified"))

    # If we're moving towards a rev, get the chain of revs up to that rev.
    line = set()
    if towards:
        towardsrevs = scmutil.revrange(repo, [towards])
        if len(towardsrevs) > 1:
            raise error.Abort(_("'%s' refers to multiple commits") % towards)
        towardsrev = towardsrevs.first()
        line = set(repo.nodes(".::%d", towardsrev))
        if not line:
            raise error.Abort(
                _("the current commit is not an ancestor of '%s'") % towards
            )

    for i in count(0):
        # Loop until we're gone the desired number of steps, or we reach a
        # node with a bookmark if the bookmark option was specified.
        # If top is specified, loop until we reach a head.
        if bookmark:
            if i > 0 and repo[node].bookmarks():
                break
        elif (n is None or i >= n) and not top:
            break

        # If the rebase flag is present, rebase any unstable children.
        # This means we can't rely on precomputed child relationships.
        if rebase:
            common.restackonce(ui, repo, repo[node].rev(), childrenonly=True)
        children = set(c.node() for c in repo[node].children())

        # Remove children not along the specified line.
        children = (children & line) or children

        # Have we reached a head?
        if not children:
            if node == repo["."].node():
                raise error.Abort(_("current commit has no children"))
            if not top:
                ui.status(_("reached head commit\n"))
            break

        # Are there multiple children?
        if len(children) > 1 and not newest:
            ui.status(_("commit %s has multiple children, namely:\n") % short(node))
            children = [
                c.node()
                for c in _showchangesets(
                    ui, repo, nodes=children, indices=ui.interactive()
                )
            ]
            # if there's only one nonobsolete we're guessing it's the one
            nonobschildren = list(filter(lambda c: not repo[c].obsolete(), children))
            draftchildren = list(filter(lambda c: repo[c].mutable(), children))
            if len(nonobschildren) == 1:
                node = nonobschildren[0]
                ui.status(_("choosing the only non-obsolete child: %s\n") % short(node))
            elif preferdraft and len(draftchildren) == 1:
                node = draftchildren[0]
                ui.status(_("choosing the only draft child: %s\n") % short(node))
            elif ui.interactive():
                node = _choosenode(ui, children)
            else:
                raise error.Abort(
                    _("ambiguous next commit"),
                    error_category=error.USAGE_ERROR,
                    hint=_(
                        "use the --newest or --towards flags "
                        "to specify which child to pick"
                    ),
                )
        else:
            # Get the child with the highest revision number.
            node = max(children, key=lambda childnode: repo[childnode].rev())

    return node


def _choosenode(ui, nodes):
    pref = " $$ &"
    n = len(nodes)
    options = pref + pref.join(map(str, range(1, n + 1)))
    cancel = _("(c)ancel")
    cancelclean = _("cancel")
    options = f" [1-{n}/{cancel}]? $$ &{cancelclean}{options}"
    choice = ui.promptchoice(_("which commit to select{}").format(options))
    if choice == 0:
        raise error.Abort(
            _("cancelling as requested"),
        )
    else:
        return nodes[choice - 1]


def _findstacktop(ui, repo, newest=False, hint=_MULTI_CHILDREN_HINT):
    """Find the head of the current stack."""
    heads = list(repo.nodes("heads(.::)"))
    if len(heads) > 1:
        if newest:
            # We can't simply return heads.max() since this might give
            # a different answer from walking up the stack as in
            # _findnexttarget(), which picks the child with the greatest
            # revision number at each step. This would be confusing, since
            # it would mean that `hg next --top` and `hg next --top --rebase`
            # would result in different destination commits.
            return _findnexttarget(ui, repo, newest=True, top=True)
        ui.warn(_("current stack has multiple heads, namely:\n"))
        _showchangesets(ui, repo, nodes=heads, indices=ui.interactive())
        if ui.interactive():
            return _choosenode(ui, heads)
        raise error.Abort(
            _("ambiguous next commit"),
            error_category=error.USAGE_ERROR,
            hint=hint,
        )
    else:
        return next(iter(heads), None)


def _findstackbottom(ui, repo):
    """Find the lowest non-public ancestor of the current commit."""
    if repo["."].ispublic():
        raise error.Abort(_("current commit is public"))
    bottoms = list(repo.nodes("roots(draft() & ::.)"))
    if len(bottoms) > 1:
        ui.warn(_("current stack has multiple bottom commits, namely:\n"))
        _showchangesets(ui, repo, nodes=bottoms, indices=ui.interactive())
        if ui.interactive():
            return _choosenode(ui, bottoms)
        raise error.Abort(
            _("ambiguous bottom commit"),
        )
    else:
        return next(iter(bottoms), None)


def _showchangesets(ui, repo, contexts=None, revs=None, nodes=None, indices=False):
    """Pretty print a list of commits. Can take a list of
    change contexts, a list of revision numbers, or a list of
    commit hashes.
    """
    if contexts is None:
        contexts = []
    if revs is not None:
        contexts.extend(repo[r] for r in revs)
    if nodes is not None:
        contexts.extend(repo[n] for n in nodes)
    showopts = {
        "template": '[{shortest(node, 6)}] {if(bookmarks, "({bookmarks}) ")}'
        "{desc|firstline}\n"
    }
    displayer = cmdutil.show_changeset(ui, repo, showopts)
    contexts = sorted(contexts, key=lambda c: c.rev())
    for idx, ctx in enumerate(contexts, start=1):
        if indices:
            ui.status(_("({}) ").format(idx))
        displayer.show(ctx)
    return contexts


def _setbookmark(repo, tr, bookmark, node):
    """Make the given bookmark point to the given revision."""
    repo._bookmarks[bookmark] = node
    repo._bookmarks.recordchange(tr)


def _activate(ui, repo, node):
    """Activate the bookmark on the given revision if it only has one bookmark."""
    marks = repo.nodebookmarks(node)
    if len(marks) == 1:
        b = ui.label(marks[0], "bookmarks.active")
        ui.status(_("(activating bookmark %s)\n") % b)
        bookmarks.activate(repo, marks[0])


@revsetpredicate("next([n])")
def _next(repo, subset, x):
    """``next([n=1])``
    The next commit in the stack.
    Similar to ``.~-n``, except prompting for ambiguous choices.
    """
    args = revsetlang.getargs(x, 0, 1, _("next takes at most one argument"))
    n = args and revsetlang.getinteger(args[0], _("next requires an integer")) or 1
    node = _findnexttarget(repo.ui, repo, n=n)
    return _torevset(repo, subset, node)


@revsetpredicate("previous([n])")
def _previous(repo, subset, x):
    """``previous([n=1])``
    The previous commit.
    Similar to ``.~n``, except prompting for ambiguous choices.
    """
    args = revsetlang.getargs(x, 0, 1, _("previous takes at most one argument"))
    n = args and revsetlang.getinteger(args[0], _("previous requires an integer")) or 1
    node = _findprevtarget(repo.ui, repo, n=n)
    return _torevset(repo, subset, node)


@revsetpredicate("top()")
def _top(repo, subset, x):
    """``top()``
    Top of the draft stack.
    """
    revsetlang.getargs(x, 0, 0, _("top takes no arguments"))
    node = _findstacktop(repo.ui, repo, hint=None)
    return _torevset(repo, subset, node)


@revsetpredicate("bottom()")
def _bottom(repo, subset, x):
    """``bottom()``
    Bottom of the draft stack.
    """
    revsetlang.getargs(x, 0, 0, _("bottom takes no arguments"))
    node = _findstackbottom(repo.ui, repo)
    return _torevset(repo, subset, node)


@revsetpredicate("stack()")
def _stack(repo, subset, x):
    """``stack()``
    Linear draft stack containing the working directory parent.
    """
    revsetlang.getargs(x, 0, 0, _("stack takes no arguments"))

    if repo["."].ispublic():
        return smartset.baseset(repo=repo)

    top = _findstacktop(repo.ui, repo, hint=None)
    bottom = _findstackbottom(repo.ui, repo)

    cl = repo.changelog
    nodes = cl.dag.range([bottom], [top])
    return subset & cl.torevset(nodes)


def _torevset(repo, subset, node):
    cl = repo.changelog
    return subset & cl.torevset(cl.dag.sort([node]))
