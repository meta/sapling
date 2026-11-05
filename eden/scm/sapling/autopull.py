# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# autopull.py - utilities to pull commits automatically.


import re
from typing import Callable, Optional

from . import bookmarks, error, registrar, util
from .i18n import _
from .node import hex

_commithashre = re.compile(r"\A[0-9a-f]{6,40}\Z")
_table = {}  # {name: (repo, name) -> Optional[pullattempt]}

builtinautopullpredicate = registrar.autopullpredicate(_table)


class pullattempt:
    """Describe an auto-pull attempt"""

    def __init__(
        self,
        friendlyname=None,
        headnodes=None,
        headnames=None,
        bookmarknames=None,
        source=None,
    ):
        # Name to display during pull. ex. "commit abcdef", "bookmark foo".
        self.friendlyname = friendlyname
        # Parameters passed to repo.pull.
        self.headnodes = headnodes or []
        self.headnames = headnames or []
        self.bookmarknames = bookmarknames or []
        self.source = source

    def execute(self, repo):
        """Execute the pull on a repo."""
        if self.source:
            source = self.source
        elif "default" in repo.ui.paths:
            source = "default"
        else:
            return

        # Print the "pulling ..." message.
        if self.friendlyname is None:
            name = ", ".join(
                repr(s)
                for s in sorted(
                    set(
                        self.bookmarknames
                        + self.headnames
                        + [hex(n) for n in self.headnodes]
                    )
                )
            )
        else:
            name = self.friendlyname
        path = repo.ui.paths.get(source)
        if path is None:
            return
        url = str(path.url)
        repo.ui.status_err(_("pulling %s from %r\n") % (name, url))

        # Pull.
        #
        # When pulling a single commit. Also pull selectivepull bookmarks so
        # it does not end up with lagged master issues.
        bookmarknames = list(self.bookmarknames)
        if self.headnodes or self.headnames:
            bookmarknames += bookmarks.selectivepullbookmarknames(repo, source)

        # (best-effort) avoid changing visible heads for readonly command.
        visible = repo.ui.cmdtype != registrar.command.readonly

        # Set by commands that resolve a commit with a readonly command and then
        # check it out, like `worktree add` running `log`.
        if repo.ui.configbool("experimental", "autopull-always-visible"):
            visible = True

        try:
            repo.pull(
                source,
                bookmarknames=bookmarknames,
                headnodes=self.headnodes,
                headnames=self.headnames,
                visible=visible,
            )
        except Exception as ex:
            repo.ui.status_err(_("pull failed: %s\n") % ex)

    def trymerge(self, other):
        """Merge this pullattempt with another pullattempt.

        If pullattempt cannot be merged, return None.
        """
        if type(self) != type(other) or self.source != other.source:
            return None
        return pullattempt(
            bookmarknames=self.bookmarknames + other.bookmarknames,
            headnodes=self.headnodes + other.headnodes,
            headnames=self.headnames + other.headnames,
            source=self.source,
        )


class deferredpullattempt(pullattempt):
    """Defer pullattempt generation until execute() time.

    deferredpullattempts are not mergeable with any other pullattempts.
    """

    def __init__(self, generate: Callable[[], Optional[pullattempt]]):
        self.generate = generate

    def execute(self, repo):
        if attempt := self.generate():
            attempt.execute(repo)

    def trymerge(self, _other):
        return None


@util.no_recursion
def calculate_attempts(repo, xs):
    """Calculate the "pullattempt"s for the given names.
    This function might return None or a list.
    """
    # If paths.default is not set. Do not attempt to pull.
    if repo.ui.paths.get("default") is None:
        return

    def sortkey(tup):
        name, func = tup
        return (func._priority, name)

    # Try autopull functions.
    funcs = [
        func
        for _name, func in sorted(_table.items(), key=lambda t: (t[1]._priority, t[0]))
    ]
    if not funcs:
        return

    # Collect all attempts.
    attempts = []
    for x in xs:
        for func in funcs:
            attempt = func(repo, x)
            if attempt:
                assert isinstance(attempt, pullattempt)
                attempts.append(attempt)

    return attempts


def trypull(repo, xs):
    # Do not attempt to pull the same name twice, or names in the repo.
    autopulled = repo.volatile_state.get("autopulled")
    if autopulled is None:
        autopulled = repo.volatile_state["autopulled"] = set()
    xs = [x for x in xs if x not in autopulled and x not in repo]
    if not xs:
        return False
    autopulled.update(xs)

    attempts = calculate_attempts(repo, xs)

    # Merge all pullattempts and execute it.
    if attempts:
        attempt = attempts[0]
        for other in attempts[1:]:
            merged = attempt.trymerge(other)
            if merged:
                attempt = merged
            else:
                attempt.execute(repo)
                attempt = other
        attempt.execute(repo)
        unfi = repo
        return all(x in unfi for x in xs)

    return False


def rewritepullrevs(repo, revs):
    """Rewrite names used by 'pull -r REVS' by applying autopull functions that
    have rewritepullrev set. For example, this can be used to rewrite ["D123"] to
    ["COMMIT_HASH"].
    """
    funcs = [
        func
        for _name, func in sorted(_table.items(), key=lambda t: (t[1]._priority, t[0]))
        if func._rewritepullrev
    ]
    newrevs = []
    for rev in revs:
        rewritten = None
        for func in funcs:
            if not rewritten:
                attempt = func(repo, rev, rewritepullrev=True)
                if attempt:
                    if attempt.bookmarknames:
                        raise error.ProgrammingError(
                            "rewriting 'pull -r REV' into bookmark pulls is unsupported"
                        )
                    rewritten = [hex(n) for n in attempt.headnodes] + attempt.headnames
        if rewritten:
            repo.ui.status_err(
                _("rewriting pull rev %r into %s\n")
                % (rev, ", ".join(repr(r) for r in rewritten))
            )
            newrevs += rewritten
        else:
            newrevs.append(rev)
    return newrevs


@builtinautopullpredicate("remotenames", priority=10)
def _pullremotebookmarks(repo, x):
    # Pull remote names like "remote/foo" automatically.
    pattern = repo.ui.config("remotenames", "autopullpattern")
    hoist = repo.ui.config("remotenames", "hoist")
    if pattern and "/" in x:
        matchfn = util.cachedstringmatcher(pattern)
        if matchfn(x):
            remotename, name = bookmarks.splitremotename(x)
            if remotename == hoist:
                source = None
            else:
                source = remotename
            return pullattempt(bookmarknames=[name], headnames=[], source=source)


@builtinautopullpredicate("commits", priority=20)
def _pullcommits(repo, x):
    # Pull commit hashes automatically.
    if repo.ui.configbool("ui", "autopullcommits") and _commithashre.match(x):
        return pullattempt(headnames=[x])


@builtinautopullpredicate("hoistednames", priority=30)
def _pullhoistnames(repo, x):
    # Pull hoist remote names automatically. For example, "foo" -> "remote/foo".
    hoistpattern = repo.ui.config("remotenames", "autopullhoistpattern")
    if hoistpattern:
        matchfn = util.cachedstringmatcher(hoistpattern)
        if matchfn(x):
            # XXX: remotenames.hoist config should be the "source" but is
            # ignored here. See "_pullremotebookmarks" for reasons.
            return pullattempt(bookmarknames=[x])


@builtinautopullpredicate("commitschemes", priority=100, rewritepullrev=True)
def _commitscheme(repo, x, rewritepullrev=False):
    # Translate commit of a foreign scheme to local repo's scheme.
    if not repo.commitscheme.possible_schemes(x):
        return None

    def generateattempt() -> Optional[pullattempt]:
        localnode = repo.commitscheme.translate(x, "local")
        if not localnode:
            return None
        return pullattempt(headnodes=[localnode])

    if rewritepullrev:
        return generateattempt()
    else:
        return deferredpullattempt(generate=generateattempt)


def loadpredicate(ui, extname, registrarobj):
    for name, func in registrarobj._table.items():
        if name in _table:
            raise error.ProgrammingError("namespace '%s' is already registered", name)
        _table[name] = func
