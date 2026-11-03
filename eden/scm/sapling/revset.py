# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# revset.py - revision set queries for mercurial
#
# Copyright 2010 Olivia Mackall <olivia@selenic.com>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.


import re
import sys

from . import (
    autopull,
    bookmarks,
    dagop,
    error,
    git,
    hbisect,
    hintutil,
    match as matchmod,
    mutation,
    node,
    pathutil,
    phases,
    registrar,
    revsetlang,
    scmutil,
    smartset,
    util,
    visibility,
)
from .i18n import _

# helpers for processing parsed tree
getsymbol = revsetlang.getsymbol
getstring = revsetlang.getstring
getinteger = revsetlang.getinteger
getboolean = revsetlang.getboolean
getlist = revsetlang.getlist
getrange = revsetlang.getrange
getargs = revsetlang.getargs
getargsdict = revsetlang.getargsdict

baseset = smartset.baseset
generatorset = smartset.generatorset
spanset = smartset.spanset
fullreposet = smartset.fullreposet

# Constants for ordering requirement, used in getset():
#
# If 'define', any nested functions and operations MAY change the ordering of
# the entries in the set (but if changes the ordering, it MUST ALWAYS change
# it). If 'follow', any nested functions and operations MUST take the ordering
# specified by the first operand to the '&' operator.
#
# For instance,
#
#   X & (Y | Z)
#   ^   ^^^^^^^
#   |   follow
#   define
#
# will be evaluated as 'or(y(x()), z(x()))', where 'x()' can change the order
# of the entries in the set, but 'y()', 'z()' and 'or()' shouldn't.
#
# 'any' means the order doesn't matter. For instance,
#
#   (X & !Y) | ancestors(Z)
#         ^              ^
#         any            any
#
# For 'X & !Y', 'X' decides the order and 'Y' is subtracted from 'X', so the
# order of 'Y' does not matter. For 'ancestors(Z)', Z's order does not matter
# since 'ancestors' does not care about the order of its argument.
#
# Currently, most revsets do not care about the order, so 'define' is
# equivalent to 'follow' for them, and the resulting order is based on the
# 'subset' parameter passed down to them:
#
#   m = revset.match(...)
#   m(repo, subset, order=defineorder)
#           ^^^^^^
#      For most revsets, 'define' means using the order this subset provides
#
# There are a few revsets that always redefine the order if 'define' is
# specified: 'sort(X)', 'reverse(X)', 'x:y'.
anyorder = "any"  # don't care the order, could be even random-shuffled
defineorder = "define"  # ALWAYS redefine, or ALWAYS follow the current order
followorder = "follow"  # MUST follow the current order

# helpers


def getset(repo, subset, x, order=defineorder):
    if not x:
        raise error.ParseError(_("missing argument"))
    return methods[x[0]](repo, subset, *x[1:], order=order)


def _getrevsource(repo, r):
    extra = repo[r].extra()
    for label in ("source", "transplant_source", "rebase_source"):
        if label in extra:
            try:
                return repo[extra[label]].rev()
            except error.RepoLookupError:
                pass
    return None


def _getdepthargs(name, args):
    startdepth = stopdepth = None
    if "startdepth" in args:
        n = getinteger(args["startdepth"], "%s expects an integer startdepth" % name)
        if n < 0:
            raise error.ParseError("negative startdepth")
        startdepth = n
    if "depth" in args:
        n = getinteger(args["depth"], _("%s expects an integer depth") % name)
        if n < 0:
            raise error.ParseError(_("negative depth"))
        stopdepth = n + 1
    return startdepth, stopdepth


_ageparser = re.compile(r"^(?:(\d+)d)?(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s?)?$")


def parseage(age):
    m = _ageparser.match(age)
    if not m:
        raise error.ParseError("invalid age in age range: %s" % age)
    days, hours, minutes, seconds = m.groups()
    agedelta = 0
    agedelta += int(days or 0) * 60 * 60 * 24
    agedelta += int(hours or 0) * 60 * 60
    agedelta += int(minutes or 0) * 60
    agedelta += int(seconds or 0)
    return agedelta


def parseagerange(agerange):
    now = util.parsedate("now")[0]
    if agerange.startswith("<"):
        start = now - parseage(agerange[1:])
        end = None
    elif agerange.startswith(">"):
        start = None
        end = now - parseage(agerange[1:])
    elif "-" in agerange:
        a1, a2 = agerange.split("-", 1)
        start = now - parseage(a2)
        end = now - parseage(a1)
    else:
        raise error.ParseError("invalid age range")
    return start, end


# operator methods


def _warnrevnum(ui, x):
    # 'devel.legacy.revnum:real' is set by scmutil.revrange to scope the
    # check to user-provided inputs (i.e. excluding internal APIs like
    # repo.revs(...)).
    config = ui.config("devel", "legacy.revnum:real")
    if config:
        # Log the usage.
        # This now uses 2 because 1 has too many false positives.
        ui.log("revnum_used", revnum_used=2)
        # Also log the detailed use (traceback) of this (deprecated) feature to
        # another table.
        ui.log(
            "features",
            fullargs=repr(sys.argv),
            feature="revnum",
            traceback=util.smarttraceback(),
        )
    if config == "warn":
        hintutil.trigger("revnum-deprecate", x)
    elif config == "abort":
        raise error.Abort(_("local revision number is disabled in this repo"))


def stringset(repo, subset, x, order):
    i = scmutil.intrev(repo[x])

    if x.startswith("-") or x == str(i):
        # 'x' was used as a revision number. Maybe warn and log it.
        _warnrevnum(repo.ui, x)
        scmutil.trackrevnumfortests(repo, [x])

    x = i
    if x in subset or x == node.nullrev and isinstance(subset, fullreposet):
        return baseset([x], repo=repo)
    return baseset(repo=repo)


def rangeset(repo, subset, x, y, order):
    m = getset(repo, fullreposet(repo), x)
    n = getset(repo, fullreposet(repo), y)

    if not m or not n:
        return baseset(repo=repo)
    return _makerangeset(repo, subset, m.first(), n.last(), order)


def rangeall(repo, subset, x, order):
    assert x is None
    return _makerangeset(repo, subset, 0, smartset.maxrev, order)


def rangepre(repo, subset, y, order):
    # ':y' can't be rewritten to '0:y' since '0' may be hidden
    n = getset(repo, fullreposet(repo), y)
    if not n:
        return baseset(repo=repo)
    return _makerangeset(repo, subset, 0, n.last(), order)


def rangepost(repo, subset, x, order):
    m = getset(repo, fullreposet(repo), x)
    if not m:
        return baseset(repo=repo)
    return _makerangeset(repo, subset, m.first(), smartset.maxrev, order)


def _makerangeset(repo, subset, m, n, order):
    if m == n:
        r = baseset([m], repo=repo)
    elif n == node.wdirrev:
        r = spanset(repo, m, smartset.maxrev) + baseset([n], repo=repo)
    elif m == node.wdirrev:
        r = baseset([m], repo=repo) + spanset(repo, smartset.maxrev, n - 1)
    elif m < n:
        r = spanset(repo, m, n + 1)
    else:
        r = spanset(repo, m, n - 1)

    if order == defineorder:
        return r & subset
    else:
        # carrying the sorting over when possible would be more efficient
        return subset & r


def dagrange(repo, subset, x, y, order):
    r = fullreposet(repo)
    xs = getset(repo, r, x)
    ys = getset(repo, r, y)

    # rust changelog alternative path
    cl = repo.changelog
    t = cl.tonodes
    # special case: null::ys == ::ys (rust dag does not have null)
    if node.nullrev in xs:
        nodes = cl.dag.ancestors(t(ys))
    else:
        nodes = cl.dag.range(t(xs), t(ys))
    return subset & cl.torevset(nodes)


def andset(repo, subset, x, y, order):
    if order == anyorder:
        yorder = anyorder
    else:
        yorder = followorder
    return getset(repo, getset(repo, subset, x, order), y, yorder)


def andsmallyset(repo, subset, x, y, order):
    # 'andsmally(x, y)' is equivalent to 'and(x, y)', but faster when y is small
    if order == anyorder:
        yorder = anyorder
    else:
        yorder = followorder
    return getset(repo, getset(repo, subset, y, yorder), x, order)


def differenceset(repo, subset, x, y, order):
    return getset(repo, subset, x, order) - getset(repo, subset, y, anyorder)


def _orsetlist(repo, subset, xs, order):
    assert xs
    if len(xs) == 1:
        return getset(repo, subset, xs[0], order)
    p = len(xs) // 2
    a = _orsetlist(repo, subset, xs[:p], order)
    b = _orsetlist(repo, subset, xs[p:], order)
    return a + b


def orset(repo, subset, x, order):
    xs = getlist(x)
    if order == followorder:
        # slow path to take the subset order
        return subset & _orsetlist(repo, fullreposet(repo), xs, anyorder)
    else:
        return _orsetlist(repo, subset, xs, order)


def notset(repo, subset, x, order):
    return subset - getset(repo, subset, x, anyorder)


def relationset(repo, subset, x, y, order):
    raise error.ParseError(_("can't use a relation in this context"))


def relsubscriptset(repo, subset, x, y, z, order):
    # this is pretty basic implementation of 'x#y[z]' operator, still
    # experimental so undocumented. see the wiki for further ideas.
    # https://www.mercurial-scm.org/wiki/RevsetOperatorPlan
    rel = getsymbol(y)
    n = getinteger(z, _("relation subscript must be an integer"))

    # TODO: perhaps this should be a table of relation functions
    if rel in ("g", "generations"):
        # TODO: support range, rewrite tests, and drop startdepth argument
        # from ancestors() and descendants() predicates
        if n <= 0:
            n = -n
            return _ancestors(repo, subset, x, startdepth=n, stopdepth=n + 1)
        else:
            return _descendants(repo, subset, x, startdepth=n, stopdepth=n + 1)

    raise error.UnknownIdentifier(rel, ["generations"])


def subscriptset(repo, subset, x, y, order):
    raise error.ParseError(_("can't use a subscript in this context"))


def listset(repo, subset, *xs, **opts):
    raise error.ParseError(
        _("can't use a list in this context"),
        hint=_('see @prog@ help "revsets.x or y"'),
    )


def keyvaluepair(repo, subset, k, v, order):
    raise error.ParseError(_("can't use a key-value pair in this context"))


def oldnonobsworkingcopyparentset(repo, subset, x, order):
    func = symbols.get("oldnonobsworkingcopyparent")
    if func is None:
        raise error.ParseError(
            _(
                "please enable 'journal' extension to use '-' as old non-obsolete "
                "working copy parent"
            )
        )
    else:
        return func(repo, subset, x)


def func(repo, subset, a, b, order):
    f = getsymbol(a)
    if f in symbols:
        func = symbols[f]
        if getattr(func, "_takeorder", False):
            return func(repo, subset, b, order)
        return func(repo, subset, b)

    keep = lambda fn: getattr(fn, "__doc__", None) is not None

    syms = [s for (s, fn) in symbols.items() if keep(fn)]
    raise error.UnknownIdentifier(f, syms)


# functions

# symbols are callables like:
#   fn(repo, subset, x)
# with:
#   repo - current repository instance
#   subset - of revisions to be examined
#   x - argument in tree form
symbols = revsetlang.symbols

# symbols which can't be used for a DoS attack for any given input
# (e.g. those which accept regexes as plain strings shouldn't be included)
# functions that just return a lot of changesets (like all) don't count here
safesymbols = set()

predicate = registrar.revsetpredicate()


@predicate("adds(pattern)", safe=True, weight=30)
def adds(repo, subset, x):
    """Changesets that add a file matching pattern.

    The pattern without explicit kind like ``glob:`` is expected to be
    relative to the current directory and match against a file or a
    directory.
    """
    # i18n: "adds" is a keyword
    pat = getstring(x, _("adds requires a pattern"))
    return checkstatus(repo, subset, pat, 1)


@predicate("age(agerange)", weight=10)
def age(repo, subset, x):
    """Changesets that are in a specific age range.

    The age range can be specified in days, hours, minutes or seconds:

    - ``<30d``  : Newer than 30 days old
    - ``>4h30m``: Older than 4 hours 30 minutes old
    - ``<15s``  : Newer than 15 seconds old
    - ``1h-5h`` : Between 1 and 5 hours old

    If no unit is specified, seconds are assumed.
    """
    agerange = getstring(x, "age requires an age range")
    start, end = parseagerange(agerange)

    def agefunc(x):
        xdate = repo[x].date()[0]
        return (start is None or start < xdate) and (end is None or xdate < end)

    return subset.prefetch("text").filter(agefunc, condrepr=("<age %r>", agerange))


@predicate("ancestor(*changeset)", safe=True, weight=0.5)
def ancestor(repo, subset, x):
    """A greatest common ancestor of the changesets.

    Accepts 0 or more changesets.
    Will return empty list when passed no args.
    Greatest common ancestor of a single changeset is that changeset.
    """
    # i18n: "ancestor" is a keyword
    l = getlist(x)
    rl = fullreposet(repo)

    # (getset(repo, rl, i) for i in l) generates a list of lists
    def yieldrevs(repo, rl, l):
        for revs in (getset(repo, rl, i) for i in l):
            for r in revs:
                yield r

    cl = repo.changelog
    nodes = cl.tonodes(yieldrevs(repo, rl, l))
    anc = cl.dag.gcaone(nodes)
    if anc is None:
        return baseset(repo=repo)
    else:
        ancrev = cl.rev(anc)
        if ancrev in subset:
            return baseset([ancrev], repo=repo)
        else:
            return baseset(repo=repo)


def _depth(init, func, startdepth, stopdepth):
    """General purposed (slow) depth handling.

    func could be parents, children, etc.
    (func(x) - result) being an empty set is considered as a stop condition.

    startdepth starts with 0, is inclusive.
    stopdepth is exclusive.

    The return value None should be treated as an empty set.
    """
    result = None
    current = init

    i = 0
    while stopdepth is None or i < stopdepth:
        if startdepth is None or i >= startdepth:
            # include 'current' in result
            if result is None:
                result = current
            else:
                result |= current
        current = func(current)
        if result is not None:
            current -= result
        # empty set?
        if current.first() is None:
            break
        i += 1

    return result


def _ancestors(repo, subset, x, followfirst=False, startdepth=None, stopdepth=None):
    heads = getset(repo, fullreposet(repo), x)

    # special case: ancestors(null) == null (used by log -fr null)
    # Ideally `null` can be handled by the Rust graph in a different
    # way.
    if node.nullrev in heads and len(heads) == 1:
        return subset & heads

    if not heads:
        return baseset(repo=repo)

    # rust changelog alternative path
    cl = repo.changelog
    headnodes = cl.tonodes(heads)
    if stopdepth is None and startdepth is None:
        return subset & cl.torevset(cl.dag.ancestors(headnodes))
    else:
        # slow path calling parents n times
        nodes = _depth(headnodes, cl.dag.parents, startdepth, stopdepth) or cl.dag.sort(
            []
        )
        return subset & cl.torevset(nodes)


@predicate("ancestors(set[, depth])", safe=True)
def ancestors(repo, subset, x):
    """Changesets that are ancestors of changesets in set, including the
    given changesets themselves.

    If depth is specified, the result only includes changesets up to
    the specified generation.
    """
    # startdepth is for internal use only until we can decide the UI
    args = getargsdict(x, "ancestors", "set depth startdepth")
    if "set" not in args:
        # i18n: "ancestors" is a keyword
        raise error.ParseError(_("ancestors takes at least 1 argument"))

    startdepth, stopdepth = _getdepthargs("ancestors", args)
    return _ancestors(
        repo, subset, args["set"], startdepth=startdepth, stopdepth=stopdepth
    )


@predicate("_firstancestors", safe=True)
def _firstancestors(repo, subset, x):
    # ``_firstancestors(set)``
    # Like ``ancestors(set)`` but follows only the first parents.
    cl = repo.changelog
    s = getset(repo, fullreposet(repo), x)
    result = cl.dag.firstancestors(cl.tonodes(s))
    return subset & cl.torevset(result)


def _childrenspec(repo, subset, x, n, order):
    """Changesets that are the Nth child of a changeset
    in set.
    """
    cs = set()
    for r in getset(repo, fullreposet(repo), x):
        for i in range(n):
            c = repo[r].children()
            if len(c) == 0:
                break
            if len(c) > 1:
                raise error.RepoLookupError(
                    _("revision in set has more than one child")
                )
            r = c[0].rev()
        else:
            cs.add(r)
    return subset & cs


def ancestorspec(repo, subset, x, n, order):
    """``set~n``
    Changesets that are the Nth ancestor (first parents only) of a changeset
    in set.
    """
    n = getinteger(n, _("~ expects a number"))
    if n < 0:
        # children lookup
        return _childrenspec(repo, subset, x, -n, order)
    n = min(n, smartset.maxrev + 1)
    ps = set()
    cl = repo.changelog
    s = getset(repo, fullreposet(repo), x)
    firstancestornth = cl.dag.firstancestornth
    tonode = cl.node
    torev = cl.rev
    psadd = ps.add
    nullrev = node.nullrev
    for r in s:
        if r != nullrev:
            ancestor = firstancestornth(tonode(r), n)
            if ancestor is not None:
                psadd(torev(ancestor))
    return subset & baseset(ps, repo=repo)


@predicate("ancestorsaged(set, agerange)")
def ancestorsaged(repo, subset, x):
    """Ancestors in an age range, stopping at the first that is too old.

    Similar to ``ancestors(set) & age(agerange)`` except that all ancestors that
    are ancestors of any commit that is older than the age range are also
    excluded. This only matters if there are changesets that have ancestors that
    are newer than them.

    For example, given the changesets:

    - ``aaa``: (1 hour ago)
    - ``bbb``: (2 hours ago)
    - ``ccc``: (3 hours ago)
    - ``ddd``: (4 hours ago)
    - ``eee``: (2 hours ago)
    - ``fff``: (5 hours ago)

    The expression ``ancestorsaged(aaa, "30m-3h30m")`` would match changesets
    ``bbb`` and ``ccc`` only.  The changeset ``eee`` is excluded by virtue of
    being an ancestor of ``ddd``, which is outside the age range.

    The age range can be specified in days, hours, minutes or seconds:

    - ``<30d``  : Newer than 30 days old
    - ``>4h30m``: Older than 4 hours 30 minutes old
    - ``<15s``  : Newer than 15 seconds old
    - ``1h-5h`` : Between 1 and 5 hours old

    If no unit is specified, seconds are assumed.
    """
    args = getargsdict(x, "ancestorsaged", "set agerange")
    if "set" not in args or "agerange" not in args:
        # i18n: "ancestorsaged" is a keyword
        raise error.ParseError(_("ancestorsaged takes at least 2 arguments"))
    heads = getset(repo, fullreposet(repo), args["set"])
    if not heads:
        return baseset(repo=repo)
    agerange = getstring(args["agerange"], _("ancestorsaged requires an age range"))
    start, end = parseagerange(agerange)

    def older(x):
        return repo[x].date()[0] < start

    def notyounger(x):
        return repo[x].date()[0] < end

    s = dagop.revancestors(
        repo, heads, cutfunc=older if start is not None else None, prefetchtext=True
    )
    if end is not None:
        s = s.filter(notyounger)

    return subset & s


@predicate("author(string)", safe=True, weight=10)
def author(repo, subset, x):
    """Alias for ``user(string)``."""
    # i18n: "author" is a keyword
    n = getstring(x, _("author requires a string"))
    kind, pattern, matcher = _substringmatcher(n, casesensitive=False)
    return subset.prefetch("text").filter(
        lambda x: matcher(repo[x].user()), condrepr=("<user %r>", n)
    )


@predicate("bisect(string)", safe=True)
def bisect(repo, subset, x):
    """Changesets marked in the specified bisect status:

    - ``good``, ``bad``, ``skip``: csets explicitly marked as good/bad/skip
    - ``goods``, ``bads``      : csets topologically good/bad
    - ``range``              : csets taking part in the bisection
    - ``pruned``             : csets that are goods, bads or skipped
    - ``untested``           : csets whose fate is yet unknown
    - ``ignored``            : csets ignored due to DAG topology
    - ``current``            : the cset currently being bisected
    """
    # i18n: "bisect" is a keyword
    status = getstring(x, _("bisect requires a string")).lower()
    state = set(hbisect.get(repo, status))
    return subset & state


@predicate("bsearch(condition, heads[, roots])", safe=True)
def bsearch(repo, subset, x):
    """Use binary search to find the first changeset in ``condition``.

    This can be useful to speed up "date" search. For example::

        bsearch(date(">2010-1-1"),.)

    The search starts from ``heads`` and follows their ancestors. ``roots``
    can be provided to limit how far back the search goes.

    Binary search requires the search range to be sorted by ``condition``. If
    a changeset is in ``condition``, then all of its descendants in the range
    should also be part of ``condition``. Otherwise this function could return
    wrong results.
    """
    args = getargsdict(x, "bsearch", "condition heads roots")
    if "condition" not in args or "heads" not in args:
        raise error.ParseError(_("bsearch takes at least two arguments"))
    cond = getset(repo, fullreposet(repo), args["condition"])
    headrevs = getset(repo, fullreposet(repo), args["heads"])

    cl = repo.changelog
    dag = cl.dag
    rev = cl.rev
    heads = dag.sort(
        node for node in dag.headsancestors(cl.tonodes(headrevs)) if rev(node) in cond
    )
    if not heads:
        return baseset(repo=repo)

    if "roots" in args:
        rootrevs = getset(repo, fullreposet(repo), args["roots"])
        roots = dag.sort(cl.tonodes(rootrevs))
    else:
        roots = dag.roots(dag.ancestors(heads))
    matchingroots = dag.sort(node for node in roots if rev(node) in cond)
    if matchingroots:
        return subset & cl.torevset(matchingroots.take(1))

    skip = dag.sort([])
    while True:
        node, _untested, firstheads = dag.suggest_bisect(roots, heads, skip)
        if node is None:
            return subset & cl.torevset(firstheads)
        if rev(node) in cond:
            heads += dag.sort([node])
        else:
            roots += dag.sort([node])


# Backward-compatibility
# - no help entry so that we do not advertise it any more
@predicate("bisected", safe=True)
def bisected(repo, subset, x):
    "(HIDDEN)"
    return bisect(repo, subset, x)


@predicate("bookmark([name])", safe=True)
def bookmark(repo, subset, x):
    """The named bookmark or all bookmarks.

    Pattern matching is supported for `name`. See :prog:`help revisions.patterns`.
    """
    # i18n: "bookmark" is a keyword
    args = getargs(x, 0, 1, _("bookmark takes one or no arguments"))
    if args:
        bm = getstring(
            args[0],
            # i18n: "bookmark" is a keyword
            _("the argument to bookmark must be a string"),
        )
        kind, pattern, matcher = util.stringmatcher(bm)
        bms = set()
        if kind == "literal":
            bmrev = repo._bookmarks.get(pattern, None)
            if not bmrev:
                raise error.RepoLookupError(_("bookmark '%s' does not exist") % pattern)
            bms.add(repo[bmrev].rev())
        else:
            matchrevs = set()
            for name, bmrev in repo._bookmarks.items():
                if matcher(name):
                    matchrevs.add(bmrev)
            if not matchrevs:
                raise error.RepoLookupError(
                    _("no bookmarks exist that match '%s'") % pattern
                )
            for bmrev in matchrevs:
                bms.add(repo[bmrev].rev())
    else:
        bms = {repo[r].rev() for r in repo._bookmarks.values()}
    bms -= {node.nullrev}
    return subset & bms


@predicate("branch(string or set)", safe=True, weight=10)
def branch(repo, subset, x):
    """
    All changesets belonging to the given branch or the branches of the given
    changesets.

    Pattern matching is supported for `string`. See
    :prog:`help revisions.patterns`.
    """
    # There is only the "default" branch. Every commit belongs to it.
    # So just return directly.
    return subset


@predicate("bumped()", safe=True)
def bumped(repo, subset, x):
    "(HIDDEN)"
    msg = "'bumped()' is deprecated, use 'phasedivergent()'"
    repo.ui.deprecwarn(msg, "4.4")
    repo.ui.deprecate(
        "bumped-revset", "bumped() has been replaced with phasedivergent()"
    )

    return phasedivergent(repo, subset, x)


@predicate("phasedivergent()", safe=True)
def phasedivergent(repo, subset, x):
    """Mutable changesets marked as successors of public changesets.

    Only non-public and non-obsolete changesets can be `phasedivergent`.
    (EXPERIMENTAL)
    """
    if mutation.enabled(repo):
        raise error.Abort(_("'phasedivergent' is not supported with mutation"))
    # i18n: "phasedivergent" is a keyword
    getargs(x, 0, 0, _("phasedivergent takes no arguments"))
    if mutation.enabled(repo):
        clrev = repo.changelog.rev
        phasedivergent = baseset(
            [clrev(n) for n in mutation.phasedivergentnodes(repo) if n in repo],
            repo=repo,
        )
    else:
        phasedivergent = baseset(repo=repo)
    return subset & phasedivergent


@predicate("bundle()", safe=True)
def bundle(repo, subset, x):
    """Changesets in the bundle.

    Bundle must be specified by the -R option."""

    try:
        bundlerevs = repo.changelog.bundlerevs
    except AttributeError:
        raise error.Abort(_("no bundle provided - specify with -R"))
    return subset & bundlerevs


def checkstatus(repo, subset, pat, field):
    hasset = matchmod.patkind(pat) == "set"

    mcache = [None]

    def matches(x):
        c = repo[x]
        if not mcache[0] or hasset:
            mcache[0] = matchmod.match(repo.root, repo.getcwd(), [pat], ctx=c)
        m = mcache[0]
        fname = None
        if not m.anypats() and len(m.files()) == 1:
            fname = m.files()[0]
        if fname is not None:
            if fname not in c.files():
                return False
        else:
            for f in c.files():
                if m(f):
                    break
            else:
                return False
        files = repo.status(c.p1().node(), c.node())[field]
        if fname is not None:
            if fname in files:
                return True
        else:
            for f in files:
                if m(f):
                    return True

    return subset.prefetch("text").filter(
        matches, condrepr=("<status[%r] %r>", field, pat)
    )


def _children(repo, subset, parentset):
    if not parentset:
        return baseset(repo=repo)
    cs = set()
    pr = repo.changelog.parentrevs
    minrev = parentset.min()
    nullrev = node.nullrev
    if repo.ui.configbool("experimental", "narrow-heads"):

        def isvisible(rev, getphase=repo._phasecache.phase):
            return getphase(repo, rev) != phases.secret

    else:

        def isvisible(rev):
            return True

    if minrev is not None:
        for r in _makerangeset(repo, subset, minrev, smartset.maxrev, anyorder):
            if r == nullrev:
                continue

            p1, p2 = pr(r)
            if p1 in parentset and isvisible(r):
                cs.add(r)
            if p2 != nullrev and p2 in parentset and isvisible(r):
                cs.add(r)
    return baseset(cs, repo=repo)


@predicate("children(set)", safe=True)
def children(repo, subset, x):
    """Child changesets of changesets in set."""
    s = getset(repo, fullreposet(repo), x)

    # rust changelog alternative path
    cl = repo.changelog
    nodes = repo.dageval(lambda dag: dag.children(cl.tonodes(s)) & (draft() | public()))
    return subset & cl.torevset(nodes)


@predicate("closed()", safe=True, weight=10)
def closed(repo, subset, x):
    """Deprecated. Changeset is closed."""
    # i18n: "closed" is a keyword
    getargs(x, 0, 0, _("closed takes no arguments"))
    return subset.filter(lambda r: False, condrepr="<branch closed>")


@predicate("contains(pattern)", weight=100)
def contains(repo, subset, x):
    """The revision's manifest contains a file matching pattern (but might not
    modify it). See :prog:`help patterns` for information about file patterns.

    The pattern without explicit kind like ``glob:`` is expected to be
    relative to the current directory and match against a file exactly
    for efficiency.
    """
    # i18n: "contains" is a keyword
    pat = getstring(x, _("contains requires a pattern"))

    def matches(x):
        if not matchmod.patkind(pat):
            pats = pathutil.canonpath(repo.root, repo.getcwd(), pat)
            if pats in repo[x]:
                return True
        else:
            c = repo[x]
            m = matchmod.match(repo.root, repo.getcwd(), [pat], ctx=c)
            for f in c.manifest():
                if m(f):
                    return True
        return False

    return subset.prefetch("text").filter(matches, condrepr=("<contains %r>", pat))


@predicate("converted([id])", safe=True, weight=10)
def converted(repo, subset, x):
    """Changesets converted from the given identifier in the old repository if
    present, or all converted changesets if no identifier is specified.
    """

    # There is exactly no chance of resolving the revision, so do a simple
    # string compare and hope for the best

    rev = None
    # i18n: "converted" is a keyword
    l = getargs(x, 0, 1, _("converted takes one or no arguments"))
    if l:
        # i18n: "converted" is a keyword
        rev = getstring(l[0], _("converted requires a revision"))

    def _matchvalue(r):
        source = repo[r].extra().get("convert_revision", None)
        return source is not None and (rev is None or source.startswith(rev))

    return subset.prefetch("text").filter(
        lambda r: _matchvalue(r), condrepr=("<converted %r>", rev)
    )


@predicate("date(interval)", safe=True, weight=10)
def date(repo, subset, x):
    """Changesets within the interval, see :prog:`help dates`."""
    # i18n: "date" is a keyword
    ds = getstring(x, _("date requires a string"))
    dm = util.matchdate(ds)
    # suggest bsearch if the scan is probably going to be slow
    # 86400: within a day, like date("2020-2-2")
    if dm.end[0] - dm.start[0] <= 86400 and (subset.fastlen() or 0) > 10000:
        hintutil.triggershow(repo.ui, "date-revset", ds, bookmarks.mainbookmark(repo))
    return subset.prefetch("text").filter(
        lambda x: dm(repo[x].date()[0]), condrepr=("<date %r>", ds)
    )


@predicate("desc(string)", safe=True, weight=10)
def desc(repo, subset, x):
    """Search commit message for string. The match is case-insensitive.

    Pattern matching is supported for `string`. See
    :prog:`help revisions.patterns`.
    """
    # i18n: "desc" is a keyword
    ds = getstring(x, _("desc requires a string"))

    kind, pattern, matcher = _substringmatcher(ds, casesensitive=False)

    return subset.prefetch("text").filter(
        lambda r: matcher(repo[r].description()), condrepr=("<desc %r>", ds)
    )


def _descendants(repo, subset, x, startdepth=None, stopdepth=None):
    roots = getset(repo, fullreposet(repo), x)
    if not roots:
        return baseset(repo=repo)

    # rust changelog alternative path
    cl = repo.changelog
    heads = repo.heads()
    rootnodes = cl.tonodes(roots)
    if startdepth is None and stopdepth is None:
        # special case: null::heads == ::heads (rust dag does not have null)
        if node.nullrev in roots:
            return subset & cl.torevset(cl.dag.ancestors(heads))
        return subset & cl.torevset(cl.dag.range(rootnodes, heads))
    else:
        # slow path: call children n times.
        # NOTE: This does not handle 'null' special case.
        nodes = _depth(
            rootnodes, cl.dag.children, startdepth, stopdepth
        ) or cl.dag.sort([])
        # filter by visible heads
        nodes = cl.dag.ancestors(heads) & nodes
        return subset & cl.torevset(nodes)


@predicate("descendants(set[, depth])", safe=True)
def descendants(repo, subset, x):
    """Changesets which are descendants of changesets in set, including the
    given changesets themselves.

    If depth is specified, the result only includes changesets up to
    the specified generation.
    """
    # startdepth is for internal use only until we can decide the UI
    args = getargsdict(x, "descendants", "set depth startdepth")
    if "set" not in args:
        # i18n: "descendants" is a keyword
        raise error.ParseError(_("descendants takes at least 1 argument"))

    startdepth, stopdepth = _getdepthargs("descendants", args)
    return _descendants(
        repo, subset, args["set"], startdepth=startdepth, stopdepth=stopdepth
    )


@predicate("destination([set])", safe=True, weight=10)
def destination(repo, subset, x):
    """Deprecated. Use successors instead."""
    raise error.Abort(
        _("destination() revset is being removed. Use successors() instead.")
    )


@predicate("divergent()", safe=True)
def divergent(repo, subset, x):
    "(HIDDEN)"
    msg = "'divergent()' is deprecated, use 'contentdivergent()'"
    repo.ui.deprecwarn(msg, "4.4")
    repo.ui.deprecate(
        "divergent-revset", "divergent() has been replaced with contentdivergent()"
    )

    return contentdivergent(repo, subset, x)


@predicate("contentdivergent()", safe=True)
def contentdivergent(repo, subset, x):
    """
    Final successors of changesets with an alternative set of final
    successors. (EXPERIMENTAL)
    """
    if mutation.enabled(repo):
        raise error.Abort(_("'contentdivergent' is not supported with mutation"))
    # i18n: "contentdivergent" is a keyword
    getargs(x, 0, 0, _("contentdivergent takes no arguments"))
    if mutation.enabled(repo):
        clrev = repo.changelog.rev
        contentdivergent = baseset(
            [clrev(n) for n in mutation.contentdivergentnodes(repo) if n in repo],
            repo=repo,
        )
    else:
        contentdivergent = baseset(repo=repo)
    return subset & contentdivergent


@predicate("extinct()", safe=True)
def extinct(repo, subset, x):
    """Obsolete changesets with obsolete descendants only."""
    if mutation.enabled(repo):
        raise error.Abort(_("'extinct' is not supported with mutation"))
    # i18n: "extinct" is a keyword
    getargs(x, 0, 0, _("extinct takes no arguments"))
    extinct = baseset(repo=repo)
    return subset & extinct


@predicate("extra(label, [value])", safe=True, weight=10)
def extra(repo, subset, x):
    """Changesets with the given label in the extra metadata, with the given
    optional value.

    Pattern matching is supported for `value`. See
    :prog:`help revisions.patterns`.
    """
    args = getargsdict(x, "extra", "label value")
    if "label" not in args:
        # i18n: "extra" is a keyword
        raise error.ParseError(_("extra takes at least 1 argument"))
    # i18n: "extra" is a keyword
    label = getstring(args["label"], _("first argument to extra must be a string"))
    value = None

    if "value" in args:
        # i18n: "extra" is a keyword
        value = getstring(args["value"], _("second argument to extra must be a string"))
        kind, value, matcher = util.stringmatcher(value)

    def _matchvalue(r):
        extra = repo[r].extra()
        return label in extra and (value is None or matcher(extra[label]))

    return subset.prefetch("text").filter(
        lambda r: _matchvalue(r), condrepr=("<extra[%r] %r>", label, value)
    )


@predicate("filelog(pattern)", safe=True)
def filelog(repo, subset, x):
    """Changesets connected to the specified filelog.

    For performance reasons, visits only revisions mentioned in the file-level
    filelog, rather than filtering through all changesets (much faster, but
    doesn't include deletes or duplicate changes). For a slower, more accurate
    result, use ``file()``.

    The pattern without explicit kind like ``glob:`` is expected to be
    relative to the current directory and match against a file exactly
    for efficiency.
    """

    # i18n: "filelog" is a keyword
    pat = getstring(x, _("filelog requires a pattern"))
    s = set()
    cl = repo.changelog

    if not matchmod.patkind(pat):
        f = pathutil.canonpath(repo.root, repo.getcwd(), pat)
        files = [f]
    else:
        m = matchmod.match(repo.root, repo.getcwd(), [pat], ctx=repo[None])
        files = (f for f in repo[None] if m(f))

    for f in files:
        fl = repo.file(f)
        known = {}
        scanpos = 0
        for fr in list(fl):
            fn = fl.node(fr)
            if fn in known:
                s.add(known[fn])
                continue

            lr = fl.linkrev(fr)
            if lr in cl:
                s.add(lr)
            elif scanpos is not None:
                # lowest matching changeset is filtered, scan further
                # ahead in changelog
                start = max(lr, scanpos) + 1
                scanpos = None
                for r in cl.revs(start):
                    # minimize parsing of non-matching entries
                    if f.encode() in cl.revision(r) and f in cl.readfiles(r):
                        try:
                            # try to use manifest delta fastpath
                            n = repo[r].filenode(f)
                            if n not in known:
                                if n == fn:
                                    s.add(r)
                                    scanpos = r
                                    break
                                else:
                                    known[n] = r
                        except error.ManifestLookupError:
                            # deletion in changelog
                            continue

    return subset & s


@predicate("first(set, [n])", safe=True, takeorder=True, weight=0)
def first(repo, subset, x, order):
    """An alias for limit()."""
    return limit(repo, subset, x, order)


def _follow(repo, subset, x, name, followfirst=False):
    args = getargsdict(x, name, "file startrev")
    revs = None
    if "startrev" in args:
        revs = getset(repo, fullreposet(repo), args["startrev"])
    if "file" in args:
        x = getstring(args["file"], _("%s expected a pattern") % name)
        if revs is None:
            revs = [None]
        fctxs = []
        for r in revs:
            ctx = mctx = repo[r]
            if r is None:
                ctx = repo["."]
            m = matchmod.match(repo.root, repo.getcwd(), [x], ctx=mctx, default="path")
            files = []
            for f in ctx.manifest().walk(m):
                files.append(f)
                # 3 is not chosen scientifically. But it looks sane.
                if len(files) >= 3:
                    # Too many files. The "file" might be a prefix pattern
                    # matching every file in a directory.  Use a different code
                    # path that might be cheaper.
                    #
                    # See cmdutil._makelogrevset for the example use of
                    # _matchfiles.
                    pat = "p:%s" % x
                    matchfiles = revsetlang.formatspec(
                        '_matchfiles("r:", "d:path", %s)', pat
                    )
                    if revs == [None]:
                        s = repo.revs("reverse(ancestors(.)) & %r", matchfiles)
                    else:
                        s = repo.revs("reverse(ancestors(%ld)) & %r", revs, matchfiles)
                    return subset & s
            fctxs.extend(ctx[f].introfilectx() for f in files)
        s = dagop.filerevancestors(repo, fctxs, followfirst)
    else:
        if revs is None:
            revs = baseset([repo["."].rev()], repo=repo)
        s = dagop.revancestors(repo, revs, followfirst)

    return subset & s


@predicate("follow([file[, startrev]])", safe=True)
def follow(repo, subset, x):
    """
    An alias for ``::.`` (ancestors of the working directory's first parent).
    If file pattern is specified, the histories of files matching given
    pattern in the revision given by startrev are followed, including copies.
    """
    return _follow(repo, subset, x, "follow")


@predicate("_followfirst", safe=True)
def _followfirst(repo, subset, x):
    # ``followfirst([file[, startrev]])``
    # Like ``follow([file[, startrev]])`` but follows only the first parent
    # of every revisions or files revisions.
    return _follow(repo, subset, x, "_followfirst", followfirst=True)


@predicate("_pathhistory(set, paths...)", safe=True, weight=20)
def pathhistory(repo, subset, x):
    """Changesets changing specified paths relative to repo root"""
    # To achieve "follow" effect, pass `ancestors(.)` as `set`.
    # To achieve "_followfirst" effect, pass `_firstancestors(.)` as `set`.
    args = getargs(x, 2, (1 << 30), _("_pathhistory takes at least a set and a path"))
    revs = getset(repo, subset, args[0], _("_pathhistory requires a set"))
    paths = [getstring(x, _("argument should be paths")) for x in args[1:]]
    nodes = repo.changelog.tonodes(revs)
    hist = repo.pathhistory(paths, nodes)
    torev = repo.changelog.rev
    s = generatorset((torev(n) for n in hist), repo=repo, iterasc=False)
    s.reverse()  # use DESC order to maintain laziness
    s = subset & s
    if isinstance(s, generatorset) and s.isascending() is True:
        s.reverse()  # "subset &" might lose DESC order and laziness
    return s


@predicate("all()", safe=True)
def getall(repo, subset, x):
    """All changesets, the same as ``0:tip``."""

    # rust changelog alternative path
    cl = repo.changelog
    return subset & cl.torevset(repo.dageval(lambda: public() | draft()), reverse=True)


@predicate("_all()", safe=True)
def _all(repo, subset, x):
    """All changesets regardless of visibility"""
    return subset


@predicate("grep(regex)", weight=10)
def grep(repo, subset, x):
    """Like ``keyword(string)`` but accepts a regex. Use ``grep(r'...')``
    to ensure special escape characters are handled correctly. Unlike
    ``keyword(string)``, the match is case-sensitive.
    """
    try:
        # i18n: "grep" is a keyword
        gr = re.compile(getstring(x, _("grep requires a string")))
    except re.error as e:
        raise error.ParseError(_("invalid match pattern: %s") % e)

    def matches(x):
        c = repo[x]
        for e in list(c.files()) + [c.user(), c.description()]:
            if gr.search(e):
                return True
        return False

    return subset.prefetch("text").filter(matches, condrepr=("<grep %r>", gr.pattern))


@predicate("_matchfiles", safe=True, weight=10)
def _matchfiles(repo, subset, x):
    # _matchfiles takes a revset list of prefixed arguments:
    #
    #   [p:foo, i:bar, x:baz]
    #
    # builds a match object from them and filters subset. Allowed
    # prefixes are 'p:' for regular patterns, 'i:' for include
    # patterns and 'x:' for exclude patterns. Use 'r:' prefix to pass
    # a revision identifier, or the empty string to reference the
    # working directory, from which the match object is
    # initialized. Use 'd:' to set the default matching mode, default
    # to 'glob'. At most one 'r:' and 'd:' argument can be passed.

    l = getargs(x, 1, -1, "_matchfiles requires at least one argument")
    pats, inc, exc = [], [], []
    rev, default = None, None
    for arg in l:
        s = getstring(arg, "_matchfiles requires string arguments")
        prefix, value = s[:2], s[2:]
        if prefix == "p:":
            pats.append(value)
        elif prefix == "i:":
            inc.append(value)
        elif prefix == "x:":
            exc.append(value)
        elif prefix == "r:":
            if rev is not None:
                raise error.ParseError("_matchfiles expected at most one revision")
            if value != "":  # empty means working directory; leave rev as None
                rev = value
        elif prefix == "d:":
            if default is not None:
                raise error.ParseError("_matchfiles expected at most one default mode")
            default = value
        else:
            raise error.ParseError("invalid _matchfiles prefix: %s" % prefix)
    if not default:
        default = "glob"

    m = matchmod.match(
        repo.root,
        repo.getcwd(),
        pats,
        include=inc,
        exclude=exc,
        ctx=repo[rev],
        default=default,
    )

    # This directly read the changelog data as creating changectx for all
    # revisions is quite expensive.
    # Note: it does not support git format repo, because CommitFields::files()
    # returns None for git format repos (see commit_fields.rs).
    getfiles = repo.changelog.readfiles
    wdirrev = node.wdirrev

    def matches(x):
        if git.isgitformat(repo) or x == wdirrev or x is None:
            files = repo[x].files()
        else:
            files = getfiles(x)
        for f in files:
            if m(f):
                return True
        return False

    return subset.prefetch("text").filter(
        matches,
        condrepr=(
            "<matchfiles patterns=%r, include=%r exclude=%r, default=%r, rev=%r>",
            pats,
            inc,
            exc,
            default,
            rev,
        ),
    )


@predicate("file(pattern)", safe=True, weight=10)
def hasfile(repo, subset, x):
    """Changesets affecting files matched by pattern.

    For a faster but less accurate result, consider using ``filelog()``
    instead.

    This predicate uses ``glob:`` as the default kind of pattern.
    """
    # i18n: "file" is a keyword
    pat = getstring(x, _("file requires a pattern"))
    return _matchfiles(repo, subset, ("string", "p:" + pat))


@predicate("head()", safe=True)
def head(repo, subset, x):
    """Changeset is a head."""
    # i18n: "head" is a keyword
    getargs(x, 0, 0, _("head takes no arguments"))
    hs = set(repo.headrevs())
    return subset & baseset(hs, repo=repo)


@predicate("heads(set)", safe=True)
def heads(repo, subset, x):
    """Members of set with no children in set."""
    s = getset(repo, subset, x)

    # rust changelog alternative path
    cl = repo.changelog
    return subset & cl.torevset(cl.dag.heads(cl.tonodes(s)))


@predicate("hidden()", safe=True)
def hidden(repo, subset, x):
    """Hidden changesets."""
    getargs(x, 0, 0, _("hidden takes no arguments"))
    # _all() gives all commits, getall() gives visible commits.
    return _all(repo, subset, x) - getall(repo, subset, x)


@predicate("keyword(string)", safe=True, weight=10)
def keyword(repo, subset, x):
    """Search commit message, user name, and names of changed files for
    string. The match is case-insensitive.

    For a regular expression or case sensitive search of these fields, use
    ``grep(regex)``.
    """
    # i18n: "keyword" is a keyword
    kw = getstring(x, _("keyword requires a string")).lower()

    def matches(r):
        c = repo[r]
        return any(
            kw in t.lower() for t in list(c.files()) + [c.user(), c.description()]
        )

    return subset.prefetch("text").filter(matches, condrepr=("<keyword %r>", kw))


@predicate("limit(set[, n[, offset]])", safe=True, takeorder=True, weight=0)
def limit(repo, subset, x, order):
    """First n members of set, defaulting to 1, starting from offset."""
    args = getargsdict(x, "limit", "set n offset")
    if "set" not in args:
        # i18n: "limit" is a keyword
        raise error.ParseError(_("limit requires one to three arguments"))
    # i18n: "limit" is a keyword
    lim = getinteger(args.get("n"), _("limit expects a number"), default=1)
    if lim < 0:
        raise error.ParseError(_("negative number to select"))
    # i18n: "limit" is a keyword
    ofs = getinteger(args.get("offset"), _("limit expects a number"), default=0)
    if ofs < 0:
        raise error.ParseError(_("negative offset"))
    os = getset(repo, fullreposet(repo), args["set"])
    ls = os.slice(ofs, ofs + lim)
    if order == followorder and lim > 1:
        return subset & ls
    return ls & subset


@predicate("last(set, [n])", safe=True, takeorder=True)
def last(repo, subset, x, order):
    """Last n members of set, defaulting to 1."""
    # i18n: "last" is a keyword
    l = getargs(x, 1, 2, _("last requires one or two arguments"))
    lim = 1
    if len(l) == 2:
        # i18n: "last" is a keyword
        lim = getinteger(l[1], _("last expects a number"))
    if lim < 0:
        raise error.ParseError(_("negative number to select"))
    os = getset(repo, fullreposet(repo), l[0])
    os.reverse()
    ls = os.slice(0, lim)
    if order == followorder and lim > 1:
        return subset & ls
    ls.reverse()
    return ls & subset


@predicate("max(set)", safe=True)
def maxrev(repo, subset, x):
    """Changeset with highest revision number in set."""
    os = getset(repo, fullreposet(repo), x)
    try:
        m = os.max()
        if m in subset:
            return baseset([m], datarepr=("<max %r, %r>", subset, os), repo=repo)
    except ValueError:
        # os.max() throws a ValueError when the collection is empty.
        # Same as python's max().
        pass
    return baseset(datarepr=("<max %r, %r>", subset, os), repo=repo)


@predicate("merge()", safe=True, weight=10)
def merge(repo, subset, x):
    """Changeset is a merge changeset."""
    # i18n: "merge" is a keyword
    getargs(x, 0, 0, _("merge takes no arguments"))
    cl = repo.changelog
    return cl.torevset(cl.dag.merges(cl.tonodes(subset)))


@predicate("min(set)", safe=True)
def minrev(repo, subset, x):
    """Changeset with lowest revision number in set."""
    os = getset(repo, fullreposet(repo), x)
    try:
        m = os.min()
        if m in subset:
            return baseset([m], datarepr=("<min %r, %r>", subset, os), repo=repo)
    except ValueError:
        # os.min() throws a ValueError when the collection is empty.
        # Same as python's min().
        pass
    return baseset(datarepr=("<min %r, %r>", subset, os), repo=repo)


@predicate("modifies(pattern)", safe=True, weight=30)
def modifies(repo, subset, x):
    """Changesets modifying files matched by pattern.

    The pattern without explicit kind like ``glob:`` is expected to be
    relative to the current directory and match against a file or a
    directory.
    """
    # i18n: "modifies" is a keyword
    pat = getstring(x, _("modifies requires a pattern"))
    return checkstatus(repo, subset, pat, 0)


@predicate("named(namespace)")
def named(repo, subset, x):
    """The changesets in a given namespace.

    Pattern matching is supported for `namespace`. See
    :prog:`help revisions.patterns`.
    """
    # i18n: "named" is a keyword
    args = getargs(x, 1, 1, _("named requires a namespace argument"))

    ns = getstring(
        args[0],
        # i18n: "named" is a keyword
        _("the argument to named must be a string"),
    )
    kind, pattern, matcher = util.stringmatcher(ns)
    namespaces = set()
    if kind == "literal":
        if pattern not in repo.names:
            raise error.RepoLookupError(_("namespace '%s' does not exist") % ns)
        namespaces.add(repo.names[pattern])
    else:
        for name, ns in repo.names.items():
            if matcher(name):
                namespaces.add(ns)
        if not namespaces:
            raise error.RepoLookupError(
                _("no namespace exists that match '%s'") % pattern
            )

    names = set()
    for ns in namespaces:
        for name in ns.listnames(repo):
            if name not in ns.deprecated:
                names.update(repo[n].rev() for n in ns.nodes(repo, name))

    names -= {node.nullrev}
    return subset & names


@predicate("id(string)", safe=True)
def node_(repo, subset, x):
    """Revision non-ambiguously specified by the given hex string prefix."""
    # i18n: "id" is a keyword
    l = getargs(x, 1, 1, _("id requires one argument"))
    # i18n: "id" is a keyword
    n = getstring(l[0], _("id requires a string"))
    if len(n) == 40:
        try:
            rn = repo.changelog.rev(node.bin(n))
        except error.WdirUnsupported:
            rn = node.wdirrev
        except (LookupError, TypeError):
            rn = None
    else:
        rn = None
        try:
            pm = repo.changelog._partialmatch(n)
            if pm is not None:
                rn = repo.changelog.rev(pm)
        except error.WdirUnsupported:
            rn = node.wdirrev

    if rn is None:
        return baseset(repo=repo)
    result = baseset([rn], repo=repo)
    return result & subset


@predicate("obsolete()", safe=True)
def obsolete(repo, subset, x):
    """Mutable changeset with a newer version."""
    # i18n: "obsolete" is a keyword
    getargs(x, 0, 0, _("obsolete takes no arguments"))
    if mutation.enabled(repo):
        nodes = mutation.obsoletenodes(repo)

        # rust changelog alternative path
        cl = repo.changelog
        return subset & cl.torevset(nodes)
    else:
        obsoletes = baseset(repo=repo)
    return subset & obsoletes


@predicate("only(set, [set])", safe=True)
def only(repo, subset, x):
    """Changesets that are ancestors of the first set that are not ancestors
    of any other head in the repo. If a second set is specified, the result
    is ancestors of the first set that are not ancestors of the second set
    (i.e. ::<set1> - ::<set2>).
    """
    cl = repo.changelog
    # i18n: "only" is a keyword
    args = getargs(x, 1, 2, _("only takes one or two arguments"))
    include = getset(repo, fullreposet(repo), args[0])

    if len(args) == 1:
        if not include:
            return baseset(repo=repo)

        descendants = set(dagop.revdescendants(repo, include, False))
        exclude = [
            rev
            for rev in repo.headrevs(reverse=False)
            if not rev in descendants and not rev in include
        ]
    else:
        exclude = getset(repo, fullreposet(repo), args[1])

    # rust changelog alternative path
    cl = repo.changelog
    t = cl.tonodes
    return subset & cl.torevset(cl.dag.only(t(include), t(exclude)))


@predicate("origin([set])", safe=True)
def origin(repo, subset, x):
    """Deprecated. Use predecessors instead."""
    raise error.Abort(
        _("origin() revset is being removed. Use predecessors() instead.")
    )


@predicate("outgoing([path])", safe=False, weight=10)
def outgoing(repo, subset, x):
    """Changesets not found in the specified destination repository, or the
    default push location.
    """
    # Avoid cycles.
    from . import discovery, hg

    # i18n: "outgoing" is a keyword
    l = getargs(x, 0, 1, _("outgoing takes one or no arguments"))
    # i18n: "outgoing" is a keyword
    dest = l and getstring(l[0], _("outgoing requires a repository path")) or ""
    if not dest:
        # ui.paths.getpath() explicitly tests for None, not just a boolean
        dest = None
    path = repo.ui.paths.getpath(dest, default=("default-push", "default"))
    if not path:
        raise error.Abort(
            _("default repository not configured!"),
            hint=_("see '@prog@ help config.paths'"),
        )
    dest = path.pushloc or path.loc
    other = hg.peer(repo, {}, dest)
    with repo.ui.configoverride({("ui", "quiet"): True}):
        outgoing = discovery.findcommonoutgoing(repo, other)
    cl = repo.changelog
    o = {cl.rev(r) for r in outgoing.missing}
    return subset & o


@predicate("p1([set])", safe=True)
def p1(repo, subset, x):
    """First parent of changesets in set, or the working directory."""
    if x is None:
        p = repo[x].p1().rev()
        if p >= 0:
            return subset & baseset([p], repo=repo)
        return baseset(repo=repo)

    ps = set()
    cl = repo.changelog
    for r in getset(repo, fullreposet(repo), x):
        try:
            ps.add(cl.parentrevs(r)[0])
        except error.WdirUnsupported:
            ps.add(repo[r].p1().rev())
    ps -= {node.nullrev}
    # XXX we should turn this into a baseset instead of a set, smartset may do
    # some optimizations from the fact this is a baseset.
    return subset & ps


@predicate("p2([set])", safe=True)
def p2(repo, subset, x):
    """Second parent of changesets in set, or the working directory."""
    if x is None:
        ps = repo[x].parents()
        try:
            p = ps[1].rev()
            if p >= 0:
                return subset & baseset([p], repo=repo)
            return baseset(repo=repo)
        except IndexError:
            return baseset(repo=repo)

    ps = set()
    cl = repo.changelog
    for r in getset(repo, fullreposet(repo), x):
        try:
            ps.add(cl.parentrevs(r)[1])
        except error.WdirUnsupported:
            parents = repo[r].parents()
            if len(parents) == 2:
                ps.add(parents[1])
    ps -= {node.nullrev}
    # XXX we should turn this into a baseset instead of a set, smartset may do
    # some optimizations from the fact this is a baseset.
    return subset & ps


def parentpost(repo, subset, x, order):
    return p1(repo, subset, x)


@predicate("parents([set])", safe=True)
def parents(repo, subset, x):
    """
    The set of all parents for all changesets in set, or the working directory.
    """
    if x is None:
        ps = set(p.rev() for p in repo[x].parents())
    else:
        s = getset(repo, fullreposet(repo), x)

        # rust changelog alternative path
        cl = repo.changelog
        return subset & cl.torevset(cl.dag.parents(cl.tonodes(s)))
    ps -= {node.nullrev}
    return subset & ps


def _phase(repo, subset, *targets):
    """helper to select all rev in <targets> phases"""
    return repo._phasecache.getrevset(repo, targets, subset)


@predicate("draft()", safe=True)
def getdraft(repo, subset, x):
    """Changeset in draft phase.
    This means non landed commits. Not to be confused with draft diffs.
    All non landed diffs are associated with a draft commit.
    """
    # i18n: "draft" is a keyword
    getargs(x, 0, 0, _("draft takes no arguments"))
    target = phases.draft
    return _phase(repo, subset, target)


@predicate("secret()", safe=True)
def secret(repo, subset, x):
    """Changeset in secret phase."""
    # i18n: "secret" is a keyword
    getargs(x, 0, 0, _("secret takes no arguments"))
    target = phases.secret
    return _phase(repo, subset, target)


def parentspec(repo, subset, x, n, order):
    """``set^0``
    The set.
    ``set^1`` (or ``set^``), ``set^2``
    First or second parent, respectively, of all changesets in set.
    """
    try:
        n = int(n[1])
        if n not in (0, 1, 2):
            raise ValueError
    except (TypeError, ValueError):
        raise error.ParseError(_("^ expects a number 0, 1, or 2"))
    ps = set()
    cl = repo.changelog
    for r in getset(repo, fullreposet(repo), x):
        if n == 0:
            ps.add(r)
        elif n == 1:
            try:
                ps.add(cl.parentrevs(r)[0])
            except error.WdirUnsupported:
                ps.add(repo[r].p1().rev())
        else:
            try:
                parents = cl.parentrevs(r)
                if parents[1] != node.nullrev:
                    ps.add(parents[1])
            except error.WdirUnsupported:
                parents = repo[r].parents()
                if len(parents) == 2:
                    ps.add(parents[1].rev())
    return subset & ps


@predicate("present(set)", safe=True, takeorder=True)
def present(repo, subset, x, order):
    """An empty set, if any revision in set isn't found; otherwise,
    all revisions in set.

    If any of specified revisions is not present in the local repository,
    the query is normally aborted. But this predicate allows the query
    to continue even in such cases.
    """
    try:
        return getset(repo, subset, x, order)
    except error.RepoLookupError:
        return baseset(repo=repo)


# for internal use
@predicate("_notpublic", safe=True)
def _notpublic(repo, subset, x):
    getargs(x, 0, 0, "_notpublic takes no arguments")
    return _phase(repo, subset, phases.draft, phases.secret)


# for internal use
@predicate("_phaseandancestors(phasename, set)", safe=True)
def _phaseandancestors(repo, subset, x):
    # equivalent to (phasename() & ancestors(set)) but more efficient
    # phasename could be one of 'draft', 'secret', or '_notpublic'
    args = getargs(x, 2, 2, "_phaseandancestors requires two arguments")
    phasename = getsymbol(args[0])
    s = getset(repo, fullreposet(repo), args[1])

    draft = phases.draft
    secret = phases.secret
    phasenamemap = {
        "_notpublic": draft,
        "draft": draft,  # follow secret's ancestors
        "secret": secret,
    }
    if phasename not in phasenamemap:
        raise error.ParseError("%r is not a valid phasename" % phasename)

    minimalphase = phasenamemap[phasename]
    getphase = repo._phasecache.phase

    def cutfunc(rev):
        return getphase(repo, rev) < minimalphase

    revs = dagop.revancestors(repo, s, cutfunc=cutfunc)

    if phasename == "draft":  # need to remove secret changesets
        revs = revs.filter(lambda r: getphase(repo, r) == draft)
    return subset & revs


@predicate("public()", safe=True, weight=3)
def getpublic(repo, subset, x):
    """Changeset in public phase."""
    # i18n: "public" is a keyword
    getargs(x, 0, 0, _("public takes no arguments"))
    return _phase(repo, subset, phases.public)


@predicate("remote([id [,path]])", safe=False)
def remote(repo, subset, x):
    """Local revision that corresponds to the given identifier in a
    remote repository, if present. Here, the '.' identifier is a
    synonym for the current local branch.
    """

    from . import hg  # avoid start-up nasties

    # i18n: "remote" is a keyword
    l = getargs(x, 0, 2, _("remote takes zero, one, or two arguments"))

    # Legacy default of using the current branch (which is now only "default").
    # Maybe we should use the current remote bookmark (if one is checked out)?
    q = "default"
    if len(l) > 0:
        # i18n: "remote" is a keyword
        q = getstring(l[0], _("remote requires a string id"))

    dest = ""
    if len(l) > 1:
        # i18n: "remote" is a keyword
        dest = getstring(l[1], _("remote requires a repository path"))
    dest = repo.ui.expandpath(dest or "default")
    dest = hg.parseurl(dest)
    other = hg.peer(repo, {}, dest)
    n = other.lookup(q)
    if n in repo:
        r = repo[n].rev()
        if r in subset:
            return baseset([r], repo=repo)
    return baseset(repo=repo)


@predicate("removes(pattern)", safe=True, weight=30)
def removes(repo, subset, x):
    """Changesets which remove files matching pattern.

    The pattern without explicit kind like ``glob:`` is expected to be
    relative to the current directory and match against a file or a
    directory.
    """
    # i18n: "removes" is a keyword
    pat = getstring(x, _("removes requires a pattern"))
    return checkstatus(repo, subset, pat, 2)


@predicate("rev(number)", safe=True)
def rev(repo, subset, x):
    """Revision with the given numeric identifier."""
    # i18n: "rev" is a keyword
    l = getargs(x, 1, 1, _("rev requires one argument"))
    try:
        # i18n: "rev" is a keyword
        l = int(getstring(l[0], _("rev requires a number")))
        _warnrevnum(repo.ui, l)
    except (TypeError, ValueError):
        # i18n: "rev" is a keyword
        raise error.ParseError(_("rev expects a number"))
    if l not in repo.changelog and l not in (node.nullrev, node.wdirrev):
        return baseset(repo=repo)
    return subset & baseset([l], repo=repo)


@predicate("matching(revision [, field])", safe=True, weight=10)
def matching(repo, subset, x):
    """Changesets in which a given set of fields match the set of fields in the
    selected revision or set.

    To match more than one field pass the list of fields to match separated
    by spaces (e.g. ``author description``).

    Valid fields are most regular revision fields and some special fields.

    Regular revision fields are ``description``, ``author``, ``branch``,
    ``date``, ``files``, ``phase``, ``parents``, ``user`` and ``diff``.
    Note that ``author`` and ``user`` are synonyms. ``diff`` refers to the
    contents of the revision. Two revisions matching their ``diff`` will
    also match their ``files``.

    Special fields are ``summary`` and ``metadata``:
    ``summary`` matches the first line of the description.
    ``metadata`` is equivalent to matching ``description user date``
    (i.e. it matches the main metadata fields).

    ``metadata`` is the default field which is used when no fields are
    specified. You can match more than one field at a time.
    """
    # i18n: "matching" is a keyword
    l = getargs(x, 1, 2, _("matching takes 1 or 2 arguments"))

    revs = getset(repo, fullreposet(repo), l[0])

    fieldlist = ["metadata"]
    if len(l) > 1:
        fieldlist = getstring(
            l[1],
            # i18n: "matching" is a keyword
            _("matching requires a string as its second argument"),
        ).split()

    # Make sure that there are no repeated fields,
    # expand the 'special' 'metadata' field type
    # and check the 'files' whenever we check the 'diff'
    fields = []
    for field in fieldlist:
        if field == "metadata":
            fields += ["user", "description", "date"]
        elif field == "diff":
            # a revision matching the diff must also match the files
            # since matching the diff is very costly, make sure to
            # also match the files first
            fields += ["files", "diff"]
        else:
            if field == "author":
                field = "user"
            fields.append(field)
    fields = set(fields)
    if "summary" in fields and "description" in fields:
        # If a revision matches its description it also matches its summary
        fields.discard("summary")

    # We may want to match more than one field
    # Not all fields take the same amount of time to be matched
    # Sort the selected fields in order of increasing matching cost
    fieldorder = [
        "phase",
        "parents",
        "user",
        "date",
        "branch",
        "summary",
        "files",
        "description",
        "diff",
    ]

    def fieldkeyfunc(f):
        try:
            return fieldorder.index(f)
        except ValueError:
            # assume an unknown field is very costly
            return len(fieldorder)

    fields = list(fields)
    fields.sort(key=fieldkeyfunc)

    # Each field will be matched with its own "getfield" function
    # which will be added to the getfieldfuncs array of functions
    getfieldfuncs = []
    _funcs = {
        "user": lambda r: repo[r].user(),
        "branch": lambda r: "default",
        "date": lambda r: repo[r].date(),
        "description": lambda r: repo[r].description(),
        "files": lambda r: repo[r].files(),
        "parents": lambda r: repo[r].parents(),
        "phase": lambda r: repo[r].phase(),
        "summary": lambda r: repo[r].description().splitlines()[0],
        "diff": lambda r: list(repo[r].diff(git=True)),
    }
    for info in fields:
        getfield = _funcs.get(info, None)
        if getfield is None:
            raise error.ParseError(
                # i18n: "matching" is a keyword
                _("unexpected field name passed to matching: %s") % info
            )
        getfieldfuncs.append(getfield)
    # convert the getfield array of functions into a "getinfo" function
    # which returns an array of field values (or a single value if there
    # is only one field to match)
    getinfo = lambda r: [f(r) for f in getfieldfuncs]

    def matches(x):
        for rev in revs:
            target = getinfo(rev)
            match = True
            for n, f in enumerate(getfieldfuncs):
                if target[n] != f(x):
                    match = False
            if match:
                return True
        return False

    return subset.prefetch("text").filter(
        matches, condrepr=("<matching%r %r>", fields, revs)
    )


@predicate("reverse(set)", safe=True, takeorder=True, weight=0)
def reverse(repo, subset, x, order):
    """Reverse order of set."""
    l = getset(repo, subset, x, order)
    if order == defineorder:
        l.reverse()
    return l


@predicate("roots(set)", safe=True)
def roots(repo, subset, x):
    """Changesets in set with no parent changeset in set."""
    s = getset(repo, fullreposet(repo), x)

    # rust changelog alternative path
    cl = repo.changelog
    return subset & cl.torevset(cl.dag.roots(cl.tonodes(s)))


_sortkeyfuncs = {
    "rev": lambda c: c.rev(),
    "branch": lambda c: "default",
    "desc": lambda c: c.description(),
    "user": lambda c: c.user(),
    "author": lambda c: c.user(),
    "date": lambda c: c.date()[0],
}


def _getsortargs(x):
    """Parse sort options into (set, [(key, reverse)], opts)"""
    args = getargsdict(x, "sort", "set keys topo.firstbranch")
    if "set" not in args:
        # i18n: "sort" is a keyword
        raise error.ParseError(_("sort requires one or two arguments"))
    keys = "rev"
    if "keys" in args:
        # i18n: "sort" is a keyword
        keys = getstring(args["keys"], _("sort spec must be a string"))

    keyflags = []
    for k in keys.split():
        fk = k
        reverse = k[0] == "-"
        if reverse:
            k = k[1:]
        if k not in _sortkeyfuncs and k != "topo":
            raise error.ParseError(_("unknown sort key %r") % fk)
        keyflags.append((k, reverse))

    if len(keyflags) > 1 and any(k == "topo" for k, reverse in keyflags):
        # i18n: "topo" is a keyword
        raise error.ParseError(
            _("topo sort order cannot be combined with other sort keys")
        )

    opts = {}
    if "topo.firstbranch" in args:
        if any(k == "topo" for k, reverse in keyflags):
            opts["topo.firstbranch"] = args["topo.firstbranch"]
        else:
            # i18n: "topo" and "topo.firstbranch" are keywords
            raise error.ParseError(
                _("topo.firstbranch can only be used when using the topo sort key")
            )

    return args["set"], keyflags, opts


@predicate("sort(set[, [-]key... [, ...]])", safe=True, takeorder=True, weight=10)
def sort(repo, subset, x, order):
    """Sort set by keys. The default sort order is ascending, specify a key
    as ``-key`` to sort in descending order.

    The keys can be:

    - ``rev`` for the revision number,
    - ``branch`` for the branch name,
    - ``desc`` for the commit message (description),
    - ``user`` for user name (``author`` can be used as an alias),
    - ``date`` for the commit date
    - ``topo`` for a reverse topographical sort

    The ``topo`` sort order cannot be combined with other sort keys. This sort
    takes one optional argument, ``topo.firstbranch``, which takes a revset that
    specifies what topographical branches to prioritize in the sort.

    """
    s, keyflags, opts = _getsortargs(x)
    revs = getset(repo, subset, s, order)

    if not keyflags or order != defineorder:
        return revs
    if len(keyflags) == 1 and keyflags[0][0] == "rev":
        revs.sort(reverse=keyflags[0][1])
        return revs
    elif keyflags[0][0] == "topo":
        firstbranch = ()
        if "topo.firstbranch" in opts:
            firstbranch = getset(repo, subset, opts["topo.firstbranch"])
        cl = repo.changelog
        # XXX: Some tests are not using segments backend
        if cl.algorithmbackend == "segments":
            dag = cl.dag
            subdag = dag.subdag(cl.tonodes(revs))
            subdag = subdag.beautify(dag.ancestors(cl.tonodes(firstbranch)))
            revlist = list(map(cl.rev, subdag.all().iter()))
        else:
            revlist = dagop.toposort(revs, cl.parentrevs, firstbranch)
        revs = baseset(
            revlist,
            istopo=True,
            repo=repo,
        )
        if keyflags[0][1]:
            revs.reverse()
        return revs

    # sort() is guaranteed to be stable
    ctxs = list(revs.prefetch("text").iterctx())
    for k, reverse in reversed(keyflags):
        ctxs.sort(key=_sortkeyfuncs[k], reverse=reverse)
    return baseset([c.rev() for c in ctxs], repo=repo)


def _mapbynodefunc(repo, s, f, visibleonly=False):
    """(repo, smartset, [node] -> [node]) -> smartset

    Helper method to map a smartset to another smartset given a function only
    talking about nodes. Handles converting between rev numbers and nodes, and
    filtering.

    If visibleonly is True, filter further by only returning visible nodes.
    """
    cl = repo.changelog
    torev = cl.rev
    tonode = cl.node
    if visibleonly:
        filter = mutation.getisvisiblefunc(repo)
    else:
        filter = mutation.getislocal(repo)
    result = set(torev(n) for n in f(tonode(r) for r in s) if filter(n))
    return smartset.baseset(result, repo=repo)


@predicate("allprecursors(set[, depth])")
@predicate("allpredecessors(set[, depth])")
def allpredecessors(repo, subset, x):
    """Changesets that are predecessors of changesets in set, excluding the
    given changesets themselves. (DEPRECATED)

    If depth is specified, the result only includes changesets up to
    the specified iteration.
    """
    # startdepth is for internal use only until we can decide the UI
    args = getargsdict(x, "allpredecessors", "set depth startdepth")
    if "set" not in args:
        # i18n: "allpredecessors" is a keyword
        raise error.ParseError(_("allpredecessors takes at least 1 argument"))
    startdepth, stopdepth = _getdepthargs("allpredecessors", args)
    if startdepth is None:
        startdepth = 1

    return _predecessors(repo, subset, args["set"], startdepth, stopdepth)


@predicate("precursors(set[, depth])", safe=True)
@predicate("predecessors(set[, depth])", safe=True)
def predecessors(repo, subset, x):
    """Changesets that are predecessors of changesets in set, including the
    given changesets themselves.

    If depth is specified, the result only includes changesets up to
    the specified iteration.
    """
    # startdepth is for internal use only until we can decide the UI
    args = getargsdict(x, "predecessors", "set depth startdepth")
    if "set" not in args:
        # i18n: "predecessors" is a keyword
        raise error.ParseError(_("predecessors takes at least 1 argument"))
    startdepth, stopdepth = _getdepthargs("predecessors", args)

    return _predecessors(repo, subset, args["set"], startdepth, stopdepth)


def _predecessors(repo, subset, targetset, startdepth, stopdepth):
    if mutation.enabled(repo):
        f = lambda nodes: mutation.allpredecessors(
            repo, nodes, startdepth=startdepth, stopdepth=stopdepth
        )
    else:
        return baseset(repo=repo)
    s = getset(repo, fullreposet(repo), targetset)
    d = _mapbynodefunc(repo, s, f)
    return subset & d


@predicate("mutationhistory(set)", safe=True)
def mutationhistory(repo, subset, x):
    """The mutation history of changesets in set: the changesets themselves,
    the visible changesets connected to them by mutations (amend, rebase,
    fold, split and other rewrites) in either direction, and every changeset
    those were created from. A hidden changeset that only leads to hidden
    changesets is included only with ``--hidden``.
    """
    s = getset(repo, fullreposet(repo), x)
    if not mutation.enabled(repo):
        return subset & s
    d = _mapbynodefunc(repo, s, lambda nodes: mutation.historynodes(repo, list(nodes)))
    return subset & d


@predicate("allsuccessors(set)")
def allsuccessors(repo, subset, x):
    """Changesets that are successors of changesets in set, excluding the
    given changesets themselves. (DEPRECATED)

    If depth is specified, the result only includes changesets up to
    the specified iteration.
    """
    # startdepth is for internal use only until we can decide the UI
    args = getargsdict(x, "allsuccessors", "set depth startdepth")
    if "set" not in args:
        # i18n: "allsuccessors" is a keyword
        raise error.ParseError(_("allsuccessors takes at least 1 argument"))
    startdepth, stopdepth = _getdepthargs("allsuccessors", args)
    if startdepth is None:
        startdepth = 1

    return _successors(repo, subset, args["set"], startdepth, stopdepth)


@predicate("successors(set[, depth])", safe=True)
def successors(repo, subset, x):
    """Visible changesets that are successors of changesets in set, including
    the given changesets themselves.

    If depth is specified, the result only includes changesets up to
    the specified iteration.
    """
    # startdepth is for internal use only until we can decide the UI
    args = getargsdict(x, "successors", "set depth startdepth")
    if "set" not in args:
        # i18n: "successors" is a keyword
        raise error.ParseError(_("successors takes at least 1 argument"))
    startdepth, stopdepth = _getdepthargs("successors", args)

    return _successors(repo, subset, args["set"], startdepth, stopdepth)


@predicate("lost()")
def lost(repo, subset, x):
    "Finds invisible local commits without successors."

    def yieldlost(repo):
        metalog = repo.metalog()
        roots = metalog.roots()
        lostrevs = set()
        tonode = repo.changelog.node
        for root in reversed(roots):
            meta = metalog.checkout(root)
            heads = visibility.decodeheads(meta["visibleheads"] or b"")
            # ignore heads no longer in the repo
            heads = repo.changelog.filternodes(heads, local=True)
            invisible = repo.revs("ancestors(%ln) - all()", heads)
            for rev in invisible:
                if (
                    rev not in lostrevs
                    and len(mutation.lookupsuccessors(repo, tonode(rev))) == 0
                ):
                    lostrevs.add(rev)
                    yield rev

    return subset & generatorset(yieldlost(repo), repo=repo, iterasc=True)


def _successors(repo, subset, targetset, startdepth, stopdepth):
    if mutation.enabled(repo):
        f = lambda nodes: mutation.allsuccessors(
            repo, nodes, startdepth=startdepth, stopdepth=stopdepth
        )
    else:
        return baseset(repo=repo)
    s = getset(repo, fullreposet(repo), targetset)
    d = _mapbynodefunc(repo, s, f, visibleonly=True)
    return subset & d


def _substringmatcher(pattern, casesensitive=True):
    kind, pattern, matcher = util.stringmatcher(pattern, casesensitive=casesensitive)
    if kind == "literal":
        if not casesensitive:
            pattern = pattern.lower()
            matcher = lambda s: pattern in s.lower()
        else:
            matcher = lambda s: pattern in s
    return kind, pattern, matcher


@predicate("unstable()", safe=True)
def unstable(repo, subset, x):
    "(HIDDEN)"
    msg = "'unstable()' is deprecated, use 'orphan()'"
    repo.ui.deprecwarn(msg, "4.4")
    repo.ui.deprecate("unstable-revset", "unstable() has been replaced with orphan()")

    return orphan(repo, subset, x)


@predicate("orphan()", safe=True)
def orphan(repo, subset, x):
    """Non-obsolete changesets with obsolete ancestors. (EXPERIMENTAL)"""
    if mutation.enabled(repo):
        raise error.Abort(_("'orphan' is not supported with mutation"))
    # i18n: "orphan" is a keyword
    getargs(x, 0, 0, _("orphan takes no arguments"))
    orphan = baseset(repo=repo)
    return subset & orphan


@predicate("user(string)", safe=True, weight=10)
def user(repo, subset, x):
    """User name contains string. The match is case-insensitive.

    Pattern matching is supported for `string`. See
    :prog:`help revisions.patterns`.
    """
    return author(repo, subset, x)


@predicate("wdir()", safe=True, weight=0)
def wdir(repo, subset, x):
    """Working directory. (EXPERIMENTAL)"""
    # i18n: "wdir" is a keyword
    getargs(x, 0, 0, _("wdir takes no arguments"))
    if node.wdirrev in subset or isinstance(subset, fullreposet):
        return baseset([node.wdirrev], repo=repo)
    return baseset(repo=repo)


@predicate("remotenames()")
def remotenamesrevset(repo, subset, x):
    """All remote bookmarks and branches."""
    getargs(x, 0, 0, "remotenames takes no arguments")
    remoterevs = set()
    for rname in repo._remotenames.keys():
        remoterevs.update(_getremoterevs(repo, "remote" + rname))
    return subset & smartset.baseset(sorted(remoterevs), repo=repo)


@predicate("remotebookmark([name])")
def remotebookmarkrevset(repo, subset, x):
    """The named remote bookmark, or all remote bookmarks.

    Pattern matching is supported for `name`. See :prog:`help revisions.patterns`.
    """
    args = getargs(x, 0, 1, _("remotebookmark takes one or no arguments"))
    if args:
        bookmarkname = getstring(
            args[0], _("the argument to remotebookmark must be a string")
        )
    else:
        bookmarkname = None
    remoterevs = _getremoterevs(repo, "remotebookmarks", bookmarkname)
    if not remoterevs and bookmarkname is not None:
        raise error.RepoLookupError(
            _("no remote bookmarks exist that match '%s'") % bookmarkname
        )
    return subset & smartset.baseset(sorted(remoterevs), repo=repo)


@predicate("_interestingdraft()", takeorder=True)
def _interestingdraft(repo, subset, x, order):
    """Subset of draft() used by the `titles` namespace."""
    getargs(x, 0, 0, "_interestingdraft takes no arguments")
    revs = repo.revs("limit(reverse(draft()) & age('<30d'), 1000)")
    if order == followorder:
        return subset & revs
    else:
        return revs & subset


def _getremoterevs(repo, namespacename, matchpattern=None):
    try:
        ns = repo.names[namespacename]
    except KeyError:
        return set()

    if matchpattern is None:
        nodes = set()
        for name in ns.listnames(repo):
            nodes.update(ns.namemap(repo, name))
    else:
        kind, pattern, matcher = util.stringmatcher(matchpattern)
        if kind == "literal":
            nodes = ns.namemap(repo, pattern)
        else:
            nodes = set()
            for name in ns.listnames(repo):
                if matcher(name):
                    nodes.update(ns.namemap(repo, name))
    return {repo[node].rev() for node in nodes if node in repo}


def _orderedlist(repo, subset, x):
    s = getstring(x, "internal error")
    if not s:
        return baseset(repo=repo)
    # remove duplicates here. it's difficult for caller to deduplicate sets
    # because different symbols can point to the same rev.
    cl = repo.changelog
    ls = []
    seen = set()
    for t in s.split("\0"):
        try:
            # fast path for integer revision
            r = int(t)
            if str(r) != t or r not in cl:
                raise ValueError
            _warnrevnum(repo.ui, r)
            revs = [r]
        except ValueError:
            revs = stringset(repo, subset, t, defineorder)

        for r in revs:
            if r in seen:
                continue
            if r in subset or r == node.nullrev and isinstance(subset, fullreposet):
                ls.append(r)
            seen.add(r)
    return baseset(ls, repo=repo)


# for internal use
@predicate("_list", safe=True, takeorder=True)
def _list(repo, subset, x, order):
    if order == followorder:
        # slow path to take the subset order
        return subset & _orderedlist(repo, fullreposet(repo), x)
    else:
        return _orderedlist(repo, subset, x)


def _orderedintlist(repo, subset, x):
    s = getstring(x, "internal error")
    if not s:
        return baseset(repo=repo)
    ls = [int(r) for r in s.split("\0")]
    s = subset
    return baseset([r for r in ls if r in s], repo=repo)


# for internal use
@predicate("_intlist", safe=True, takeorder=True, weight=0)
def _intlist(repo, subset, x, order):
    if order == followorder:
        # slow path to take the subset order
        return subset & _orderedintlist(repo, fullreposet(repo), x)
    else:
        return _orderedintlist(repo, subset, x)


def _orderedhexlist(repo, subset, x):
    s = getstring(x, "internal error")
    if not s:
        return baseset(repo=repo)
    cl = repo.changelog
    ls = [cl.rev(node.bin(r)) for r in s.split("\0")]
    s = subset
    return baseset([r for r in ls if r in s], repo=repo)


# for internal use
@predicate("_hexlist", safe=True, takeorder=True)
def _hexlist(repo, subset, x, order):
    if order == followorder:
        # slow path to take the subset order
        return subset & _orderedhexlist(repo, fullreposet(repo), x)
    else:
        return _orderedhexlist(repo, subset, x)


@predicate("subtreemergebase(from_path, to_path[, from_rev])")
def subtreemergebase(repo, subset, x):
    """Subtree merge base of the specified from/to paths (EXPERIMENTAL)

    If no from_rev is specified, current working copy parent is assumed.
    """
    args = getargs(x, 2, 3, _("subtreemergebase expects two or three arguments"))
    from_path = getstring(args[0], _("from_path expected a path"))
    to_path = getstring(args[1], _("to_path expected a path"))
    if len(args) == 3:
        from_rev = getstring(args[2], _("from_rev expected a revision"))
    else:
        from_rev = "."

    ui = repo.ui
    ctx = repo["."]
    from_ctx = scmutil.revsingle(repo, from_rev)
    from_path = scmutil.rootrelpath(ctx, from_path)
    to_path = scmutil.rootrelpath(ctx, to_path)

    from sapling.commands import subtree
    from sapling.utils import subtreeutil

    subtreeutil.validate_path_overlap([from_path], [to_path])
    subtreeutil.validate_path_exist(ui, from_ctx, [from_path], abort_on_missing=True)
    subtreeutil.validate_path_exist(ui, ctx, [to_path], abort_on_missing=True)
    subtreeutil.validate_path_depth(ui, [from_path, to_path])

    with repo.ui.configoverride({("ui", "quiet"): True}):
        merge_base_ctx = subtree._subtree_merge_base(
            repo, ctx, [to_path], from_ctx, [from_path], opts={}
        )
    return subset & smartset.baseset({merge_base_ctx.rev()}, repo=repo)


methods = {
    "range": rangeset,
    "rangeall": rangeall,
    "rangepre": rangepre,
    "rangepost": rangepost,
    "dagrange": dagrange,
    "string": stringset,
    "symbol": stringset,
    "and": andset,
    "andsmally": andsmallyset,
    "or": orset,
    "not": notset,
    "difference": differenceset,
    "relation": relationset,
    "relsubscript": relsubscriptset,
    "subscript": subscriptset,
    "list": listset,
    "keyvalue": keyvaluepair,
    "func": func,
    "ancestor": ancestorspec,
    "parent": parentspec,
    "parentpost": parentpost,
    "oldnonobsworkingcopyparent": oldnonobsworkingcopyparentset,
}


def posttreebuilthook(tree, repo):
    # hook for extensions to execute code on the optimized tree
    pass


def scanunknowns(trees, lookup):
    """Scan the tree and yield unknown names.

    A name is unknown if lookup(name) returns False.
    Does not evaluate the revset.

    >>> list(scanunknowns([revsetlang.parse("limit(a+b,1,2)+c")], lambda x: False))
    ['a', 'b', 'c']
    """
    for x in trees:
        if isinstance(x, tuple):
            headname = x[0]
            args = []
            if headname in {"ancestor", "parent"}:
                # 1st argument is a set.
                args = x[1:2]
            elif headname in {
                "range",
                "rangepre",
                "rangepost",
                "dagrange",
                "and",
                "andsmally",
                "or",
                "not",
                "difference",
                "parentpost",
                "list",
            }:
                # all arguments are sets.
                args = x[1:]
            elif headname == "func":
                # ex. (func (symbol parents) (symbol master))
                #     (func (symbol limit)   (list (x) (symbol 1) (symbol 2)))
                funcname = x[1][1]
                if len(x) > 2 and x[2] and x[2][0] == "list":
                    funcargs = x[2][1:]
                else:
                    funcargs = x[2:]
                if funcname in {
                    "ancestors",
                    "ancestoraged",
                    "children",
                    "descendants",
                    "first",
                    "limit",
                    "heads",
                    "last",
                    "max",
                    "min",
                    "predecessors",
                    "reverse",
                    "successors",
                }:
                    # 1st argument is a set.
                    # the 2nd argument is not a revset, skip it.
                    args = funcargs[0:1]
                elif funcname in {"only", "ancestor", "roots", "p1", "p2", "parents"}:
                    # all arguments are sets.
                    args = funcargs
            elif headname in {"symbol", "string"}:
                # x[1] is the name to lookup.
                # SRC is a special name used by rebase.
                name = x[1]
                if name != "SRC" and not lookup(name):
                    yield name
            for name in scanunknowns(args, lookup):
                yield name


def match(ui, spec, repo=None):
    """Create a matcher for a single revision spec"""
    return matchany(ui, [spec], repo=repo)


def matchany(ui, specs, repo=None, localalias=None):
    """Create a matcher that will include any revisions matching one of the
    given specs

    If localalias is not None, it is a dict {name: definitionstring}. It takes
    precedence over [revsetalias] config section.
    """
    if not specs:

        def mfunc(repo, subset=None):
            return baseset(repo=repo)

        return mfunc
    if not all(specs):
        raise error.ParseError(_("empty query"))
    lookup = None
    if repo:
        lookup = repo.__contains__

    if len(specs) == 1:
        tree = revsetlang.parse(specs[0], lookup)
    else:
        tree = ("or", ("list",) + tuple(revsetlang.parse(s, lookup) for s in specs))

    aliases = []
    warn = None
    if ui:
        aliases.extend(ui.configitems("revsetalias"))
        warn = ui.warn
    if localalias:
        aliases.extend(localalias.items())
    if aliases:
        tree = revsetlang.expandaliases(tree, aliases, warn=warn)
    tree = revsetlang.foldconcat(tree)
    tree = revsetlang.analyze(tree)
    if repo is not None:
        lookup = repo.__contains__
        unknownnames = list(scanunknowns([tree], lookup))
    tree = revsetlang.optimize(tree)

    # Figure out the unknown symbol names statically and attempt to autopull them.
    if repo is not None:
        # If there appears to be unknown names, attempt to auto pull the full
        # revset name first. For example, for "releases/foo-bar-2010.1.1",
        # unknown names are "releases/foo", "bar", and "2010.1.1". Attempt to
        # resolve "releases/foo-bar-2010.1.1" as a whole first. This is an
        # optimization to reduce network lookups for the common case
        # (ex. hg up releases/foo-bar-2010.1.1). Other cases (ex.
        # hg log -r '"releases/foo-2010"::"releases/foo-2020"') will have more
        # network lookups if `"releases/foo-2010"::"releases/foo-2020"` matches
        # the configured autopull pattern.
        #
        # The order of auto-pull attempts is:
        # - Use parsed revset if symbols seem to be locally resolvable (no
        #   auto-pull).
        # - Auto-pull the whole name.
        # - Auto-pull individual names in the parsed revset.
        if unknownnames:
            # Auto-pull the whole name.
            resolved = autopull.trypull(
                repo, [spec for spec in specs if not lookup(spec)]
            )
            if resolved:
                # Re-parse the revset specs.
                return matchany(ui, specs, repo, localalias)
            # Auto-pull individual names in the parsed revset.
            autopull.trypull(repo, unknownnames)

    posttreebuilthook(tree, repo)
    return makematcher(tree)


def makematcher(tree):
    """Create a matcher from an evaluatable tree"""

    def mfunc(repo, subset=None, order=None):
        if order is None:
            if subset is None:
                order = defineorder  # 'x'
            else:
                order = followorder  # 'subset & x'
        if subset is None:
            subset = fullreposet(repo)
        return getset(repo, subset, tree, order)

    return mfunc


def loadpredicate(ui, extname, registrarobj):
    """Load revset predicates from specified registrarobj"""
    for name, func in registrarobj._table.items():
        symbols[name] = func
        if func._safe:
            safesymbols.add(name)


# load built-in predicates explicitly to setup safesymbols
loadpredicate(None, None, predicate)

# tell hggettext to extract docstrings from these functions:
i18nfunctions = symbols.values()
