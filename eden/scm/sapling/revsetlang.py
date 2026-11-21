# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# revsetlang.py - parser, tokenizer and utility for revision set language
#
# Copyright 2010 Olivia Mackall <olivia@selenic.com>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.


import string

from . import error, node, parser, util
from .i18n import _

elements = {
    # token-type: binding-strength, primary, prefix, infix, suffix
    "(": (21, None, ("group", 1, ")"), ("func", 1, ")"), None),
    "[": (21, None, None, ("subscript", 1, "]"), None),
    "#": (21, None, None, ("relation", 21), None),
    "##": (20, None, None, ("_concat", 20), None),
    "~": (18, None, None, ("ancestor", 18), None),
    "^": (18, None, None, ("parent", 18), "parentpost"),
    "-": (5, "oldnonobsworkingcopyparent", ("negate", 19), ("minus", 5), None),
    "::": (17, "dagrangeall", ("dagrangepre", 17), ("dagrange", 17), "dagrangepost"),
    "..": (17, "dagrangeall", ("dagrangepre", 17), ("dagrange", 17), "dagrangepost"),
    ":": (15, "rangeall", ("rangepre", 15), ("range", 15), "rangepost"),
    "not": (10, None, ("not", 10), None, None),
    "!": (10, None, ("not", 10), None, None),
    "and": (5, None, None, ("and", 5), None),
    "&": (5, None, None, ("and", 5), None),
    "%": (5, None, None, ("only", 5), "onlypost"),
    "or": (4, None, None, ("or", 4), None),
    "|": (4, None, None, ("or", 4), None),
    "+": (4, None, None, ("or", 4), None),
    "=": (3, None, None, ("keyvalue", 3), None),
    ",": (2, None, None, ("list", 2), None),
    ")": (0, None, None, None, None),
    "]": (0, None, None, None, None),
    "symbol": (0, "symbol", None, None, None),
    "string": (0, "string", None, None, None),
    "end": (0, None, None, None, None),
}

keywords = {"and", "or", "not"}

symbols = {}


_simpleopletters = set(iter("()[]#:=,-|&+!~^%"))

# default set of valid characters for the initial letter of symbols
_syminitletters = set(iter(string.ascii_letters + string.digits + "._@")) | set(
    map(chr, range(128, 256))
)

# default set of valid characters for non-initial letters of symbols
_symletters = _syminitletters | set(iter("-/"))


def tokenize(program, lookup=None, syminitletters=None, symletters=None):
    """
    Parse a revset statement into a stream of tokens

    ``syminitletters`` is the set of valid characters for the initial
    letter of symbols.

    By default, character ``c`` is recognized as valid for initial
    letter of symbols, if ``c.isalnum() or c in '._@' or ord(c) > 127``.

    ``symletters`` is the set of valid characters for non-initial
    letters of symbols.

    By default, character ``c`` is recognized as valid for non-initial
    letters of symbols, if ``c.isalnum() or c in '-._/@' or ord(c) > 127``.

    Check that @ is a valid unquoted token character (issue3686):
    >>> list(tokenize("@::"))
    [('symbol', '@', 0), ('::', None, 1), ('end', None, 3)]

    """
    program = str(program)
    if syminitletters is None:
        syminitletters = _syminitletters
    if symletters is None:
        symletters = _symletters

    if program and lookup:
        # attempt to parse old-style ranges first to deal with
        # things like old-tag which contain query metacharacters
        parts = program.split(":", 1)
        if all(lookup(sym) for sym in parts if sym):
            if parts[0]:
                yield ("symbol", parts[0], 0)
            if len(parts) > 1:
                s = len(parts[0])
                yield (":", None, s)
                if parts[1]:
                    yield ("symbol", parts[1], s + 1)
            yield ("end", None, len(program))
            return

    pos, l = 0, len(program)
    while pos < l:
        c = program[pos]
        if c.isspace():  # skip inter-token whitespace
            pass
        elif c == ":" and program[pos : pos + 2] == "::":  # look ahead carefully
            yield ("::", None, pos)
            pos += 1  # skip ahead
        elif c == "." and program[pos : pos + 2] == "..":  # look ahead carefully
            yield ("..", None, pos)
            pos += 1  # skip ahead
        elif c == "#" and program[pos : pos + 2] == "##":  # look ahead carefully
            yield ("##", None, pos)
            pos += 1  # skip ahead
        elif c in _simpleopletters:  # handle simple operators
            yield (c, None, pos)
        elif s := parser.consumestring(program, pos):
            pos = s[0]
            yield s[1]
        # gather up a symbol/keyword
        elif c in syminitletters:
            s = pos
            pos += 1
            while pos < l:  # find end of symbol
                d = program[pos]
                if d not in symletters:
                    break
                if d == "." and program[pos - 1] == ".":  # special case for ..
                    pos -= 1
                    break
                pos += 1
            sym = program[s:pos]
            if sym in keywords:  # operator keywords
                yield (sym, None, s)
            elif "-" in sym:
                # some jerk gave us foo-bar-baz, try to check if it's a symbol
                if lookup and lookup(sym):
                    # looks like a real symbol
                    yield ("symbol", sym, s)
                else:
                    # looks like an expression
                    parts = sym.split("-")
                    for p in parts[:-1]:
                        if p:  # possible consecutive -
                            yield ("symbol", p, s)
                        s += len(p)
                        yield ("-", None, pos)
                        s += 1
                    if parts[-1]:  # possible trailing -
                        yield ("symbol", parts[-1], s)
            else:
                yield ("symbol", sym, s)
            pos -= 1
        else:
            raise error.ParseError(_("syntax error in revset '%s'") % program, pos)
        pos += 1
    yield ("end", None, pos)


# helpers

_notset = object()


def getsymbol(x):
    if x and x[0] == "symbol":
        return x[1]
    raise error.ParseError(_("not a symbol"))


def getstring(x, err):
    if x and (x[0] == "string" or x[0] == "symbol"):
        return x[1]
    raise error.ParseError(err)


def getinteger(x, err, default=_notset):
    if not x and default is not _notset:
        return default
    try:
        return int(getstring(x, err))
    except ValueError:
        raise error.ParseError(err)


def getboolean(x, err):
    value = util.parsebool(getsymbol(x))
    if value is not None:
        return value
    raise error.ParseError(err)


def getlist(x):
    if not x:
        return []
    if x[0] == "list":
        return list(x[1:])
    return [x]


def getrange(x, err):
    if not x:
        raise error.ParseError(err)
    op = x[0]
    if op == "range":
        return x[1], x[2]
    elif op == "rangepre":
        return None, x[1]
    elif op == "rangepost":
        return x[1], None
    elif op == "rangeall":
        return None, None
    raise error.ParseError(err)


def getargs(x, min, max, err):
    l = getlist(x)
    if len(l) < min or (max >= 0 and len(l) > max):
        raise error.ParseError(err)
    return l


def getargsdict(x, funcname, keys):
    return parser.buildargsdict(
        getlist(x),
        funcname,
        parser.splitargspec(keys),
        keyvaluenode="keyvalue",
        keynode="symbol",
    )


# cache of {spec: raw parsed tree} built internally
_treecache = {}


def _cachedtree(spec):
    # thread safe because parse() is reentrant and dict.__setitem__() is atomic
    tree = _treecache.get(spec)
    if tree is None:
        _treecache[spec] = tree = parse(spec)
    return tree


def _build(tmplspec, *repls):
    """Create raw parsed tree from a template revset statement

    >>> _build('f(_) and _', ('string', '1'), ('symbol', '2'))
    ('and', ('func', ('symbol', 'f'), ('string', '1')), ('symbol', '2'))
    """
    template = _cachedtree(tmplspec)
    return parser.buildtree(template, ("symbol", "_"), *repls)


def _match(patspec, tree):
    """Test if a tree matches the given pattern statement; return the matches

    >>> _match('f(_)', parse('f()'))
    >>> _match('f(_)', parse('f(1)'))
    [('func', ('symbol', 'f'), ('symbol', '1')), ('symbol', '1')]
    >>> _match('f(_)', parse('f(1, 2)'))
    """
    pattern = _cachedtree(patspec)
    return parser.matchtree(pattern, tree, ("symbol", "_"), {"keyvalue", "list"})


def _matchonly(revs, bases):
    return _match("ancestors(_) and not ancestors(_)", ("and", revs, bases))


def _fixops(x):
    """Rewrite raw parsed tree to resolve ambiguous syntax which cannot be
    handled well by our simple top-down parser"""
    if not isinstance(x, tuple):
        return x

    op = x[0]
    if op == "parent":
        # x^:y means (x^) : y, not x ^ (:y)
        # x^:  means (x^) :,   not x ^ (:)
        # x^::  means (x^) ::,   not x ^ (::)
        post = ("parentpost", x[1])
        if x[2][0] == "dagrangepre":
            return _fixops(("dagrange", post, x[2][1]))
        elif x[2][0] == "dagrangeall":
            return _fixops(("dagrangepost", post))
        elif x[2][0] == "rangepre":
            return _fixops(("range", post, x[2][1]))
        elif x[2][0] == "rangeall":
            return _fixops(("rangepost", post))
    elif op == "or":
        # make number of arguments deterministic:
        # x + y + z -> (or x y z) -> (or (list x y z))
        return (op, _fixops(("list",) + x[1:]))
    elif op == "subscript" and x[1][0] == "relation":
        # x#y[z] ternary
        return _fixops(("relsubscript", x[1][1], x[1][2], x[2]))

    return (op,) + tuple(_fixops(y) for y in x[1:])


def _analyze(x):
    if x is None:
        return x

    op = x[0]
    if op == "minus":
        return _analyze(_build("_ and not _", *x[1:]))
    elif op == "only":
        return _analyze(_build("only(_, _)", *x[1:]))
    elif op == "onlypost":
        return _analyze(_build("only(_)", x[1]))
    elif op == "dagrangeall":
        raise error.ParseError(_("can't use '::' in this context"))
    elif op == "dagrangepre":
        return _analyze(_build("ancestors(_)", x[1]))
    elif op == "dagrangepost":
        return _analyze(_build("descendants(_)", x[1]))
    elif op == "negate":
        s = getstring(x[1], _("can't negate that"))
        return _analyze(("string", "-" + s))
    elif op in ("string", "symbol", "oldnonobsworkingcopyparent"):
        return x
    elif op == "rangeall":
        return (op, None)
    elif op in {"or", "not", "rangepre", "rangepost", "parentpost"}:
        return (op, _analyze(x[1]))
    elif op == "group":
        return _analyze(x[1])
    elif op in {
        "and",
        "dagrange",
        "range",
        "parent",
        "ancestor",
        "relation",
        "subscript",
    }:
        ta = _analyze(x[1])
        tb = _analyze(x[2])
        return (op, ta, tb)
    elif op == "relsubscript":
        ta = _analyze(x[1])
        tb = _analyze(x[2])
        tc = _analyze(x[3])
        return (op, ta, tb, tc)
    elif op == "list":
        return (op,) + tuple(_analyze(y) for y in x[1:])
    elif op == "keyvalue":
        return (op, x[1], _analyze(x[2]))
    elif op == "func":
        return (op, x[1], _analyze(x[2]))
    raise ValueError("invalid operator %r" % op)


def analyze(x):
    """Transform raw parsed tree to evaluatable tree which can be fed to
    optimize() or getset()

    All pseudo operations should be mapped to real operations or functions
    defined in methods or symbols table respectively.
    """
    return _analyze(x)


def _optimize(x):
    if x is None:
        return 0, x

    op = x[0]
    if op in ("string", "symbol", "oldnonobsworkingcopyparent"):
        return 0.5, x  # single revisions are small
    elif op == "and":
        wa, ta = _optimize(x[1])
        wb, tb = _optimize(x[2])
        w = min(wa, wb)

        # (::x and not ::y)/(not ::y and ::x) have a fast path
        m = _matchonly(ta, tb) or _matchonly(tb, ta)
        if m:
            return w, _build("only(_, _)", *m[1:])

        m = _match("not _", tb)
        if m:
            return wa, ("difference", ta, m[1])
        if wa > wb:
            op = "andsmally"
        return w, (op, ta, tb)
    elif op == "or":
        # fast path for machine-generated expression, that is likely to have
        # lots of trivial revisions: 'a + b + c()' to '_list(a b) + c()'
        ws, ts, ss = [], [], []

        def flushss():
            if not ss:
                return
            if len(ss) == 1:
                w, t = ss[0]
            else:
                s = "\0".join(t[1] for w, t in ss)
                y = _build("_list(_)", ("string", s))
                w, t = _optimize(y)
            ws.append(w)
            ts.append(t)
            del ss[:]

        for y in getlist(x[1]):
            w, t = _optimize(y)
            if t is not None and (t[0] == "string" or t[0] == "symbol"):
                ss.append((w, t))
                continue
            flushss()
            ws.append(w)
            ts.append(t)
        flushss()
        if len(ts) == 1:
            return ws[0], ts[0]  # 'or' operation is fully optimized out
        return max(ws), (op, ("list",) + tuple(ts))
    elif op == "not":
        # Optimize not public() to _notpublic() because we have a fast version
        if _match("public()", x[1]):
            o = _optimize(_build("_notpublic()"))
            return o[0], o[1]
        else:
            o = _optimize(x[1])
            return o[0], (op, o[1])
    elif op == "rangeall":
        return 1, x
    elif op in ("rangepre", "rangepost", "parentpost"):
        o = _optimize(x[1])
        return o[0], (op, o[1])
    elif op in ("dagrange", "range"):
        wa, ta = _optimize(x[1])
        wb, tb = _optimize(x[2])
        return wa + wb, (op, ta, tb)
    elif op in ("parent", "ancestor", "relation", "subscript"):
        w, t = _optimize(x[1])
        return w, (op, t, x[2])
    elif op == "relsubscript":
        w, t = _optimize(x[1])
        return w, (op, t, x[2], x[3])
    elif op == "list":
        ws, ts = zip(*(_optimize(y) for y in x[1:]))
        return sum(ws), (op,) + ts
    elif op == "keyvalue":
        w, t = _optimize(x[2])
        return w, (op, x[1], t)
    elif op == "func":
        f = getsymbol(x[1])
        wa, ta = _optimize(x[2])
        w = getattr(symbols.get(f), "_weight", 1)
        return w + wa, (op, x[1], ta)
    raise ValueError("invalid operator %r" % op)


def optimize(tree):
    """Optimize evaluatable tree

    All pseudo operations should be transformed beforehand.
    """
    _weight, newtree = _optimize(tree)
    return newtree


# the set of valid characters for the initial letter of symbols in
# alias declarations and definitions
_aliassyminitletters = _syminitletters | {"$"}


def _parsewith(spec, lookup=None, syminitletters=None):
    """Generate a parse tree of given spec with given tokenizing options

    >>> _parsewith('foo($1)', syminitletters=_aliassyminitletters)
    ('func', ('symbol', 'foo'), ('symbol', '$1'))
    >>> _parsewith('$1')
    Traceback (most recent call last):
      ...
    sapling.error.ParseError: ("syntax error in revset '$1'", 0)
    >>> _parsewith('foo bar')
    Traceback (most recent call last):
      ...
    sapling.error.ParseError: ('invalid token', 4)
    """
    p = parser.parser(elements)
    tree, pos = p.parse(tokenize(spec, lookup=lookup, syminitletters=syminitletters))
    if pos != len(spec):
        raise error.ParseError(_("invalid token"), pos)
    return _fixops(parser.simplifyinfixops(tree, ("list", "or")))


class _aliasrules(parser.basealiasrules):
    """Parsing and expansion rule set of revset aliases"""

    _section = _("revset alias")

    @staticmethod
    def _parse(spec):
        """Parse alias declaration/definition ``spec``

        This allows symbol names to use also ``$`` as an initial letter
        (for backward compatibility), and callers of this function should
        examine whether ``$`` is used also for unexpected symbols or not.
        """
        return _parsewith(spec, syminitletters=_aliassyminitletters)

    @staticmethod
    def _trygetfunc(tree):
        if tree[0] == "func" and tree[1][0] == "symbol":
            return tree[1][1], getlist(tree[2])


def expandaliases(tree, aliases, warn=None):
    """Expand aliases in a tree, aliases is a list of (name, value) tuples

    Simple expansion:

        >>> expandaliases(('symbol', 'foo'), [('foo', 'bar')])
        ('symbol', 'bar')

    Last definition wins:

        >>> expandaliases(('symbol', 'foo'), [('foo', 'bar1'), ('foo', 'bar2')])
        ('symbol', 'bar2')

    Function name replacement:

        >>> expandaliases(parse('foo()'), [('foo', 'bar')])
        ('func', ('symbol', 'bar'), None)

    Overloading using the same name:

        >>> expandaliases(parse('foo'), [('foo', 'foo(1)'), ('foo(x)', 'foo(x,2)')])
        ('func', ('symbol', 'foo'), ('list', ('symbol', '1'), ('symbol', '2')))

        >>> expandaliases(parse('foo'), [('foo', 'foo()')])
        ('func', ('symbol', 'foo'), None)

        >>> expandaliases(parse('foo(2)'), [('foo', 'bar'), ('bar', 'bar()')])
        ('func', ('symbol', 'bar'), ('symbol', '2'))

        >>> expandaliases(parse('foo'), [('foo(x)', 'foo'), ('foo', 'foo(1,2)')])
        ('func', ('symbol', 'foo'), ('list', ('symbol', '1'), ('symbol', '2')))

        >>> expandaliases(parse('foo(3)'), [('foo(x)', 'foo'), ('foo', 'foo(1,2)')])
        ('func', ('symbol', 'foo'), ('list', ('symbol', '1'), ('symbol', '2')))

    Alias loop:

        >>> expandaliases(parse('foo(3)'), [('foo(x)', 'foo'), ('foo', 'foo(1)')])
        Traceback (most recent call last):
          ...
        sapling.error.ParseError: infinite expansion of revset alias "foo" detected

        >>> expandaliases(parse('foo'), [('foo', 'foo()'), ('foo()', 'foo(1)'), ('foo(x)', 'foo')])
        Traceback (most recent call last):
          ...
        sapling.error.ParseError: infinite expansion of revset alias "foo" detected
    """
    aliases = _aliasrules.buildmap(aliases)
    tree = _aliasrules.expand(aliases, tree)
    # warn about problematic (but not referred) aliases
    if warn is not None:
        for (name, _args), alias in sorted(aliases.items()):
            if alias.error and not alias.warned:
                warn(_("warning: %s\n") % (alias.error))
                alias.warned = True
    return tree


def foldconcat(tree):
    """Fold elements to be concatenated by `##`"""
    if not isinstance(tree, tuple) or tree[0] in ("string", "symbol"):
        return tree
    if tree[0] == "_concat":
        pending = [tree]
        l = []
        while pending:
            e = pending.pop()
            if e[0] == "_concat":
                pending.extend(reversed(e[1:]))
            elif e[0] in ("string", "symbol"):
                l.append(e[1])
            else:
                msg = _('"##" can\'t concatenate "%s" element') % (e[0])
                raise error.ParseError(msg)
        return ("string", "".join(l))
    else:
        return tuple(foldconcat(t) for t in tree)


def parse(spec, lookup=None):
    try:
        return _parsewith(spec, lookup=lookup)
    except error.ParseError as inst:
        if len(inst.args) > 1:  # has location
            # Add 1 to location because unlike templates, revset parse errors
            # point to the char where the error happened, not the char after.
            loc = inst.args[1] + 1
            # Remove newlines -- spaces are equivalent whitespace.
            spec = spec.replace("\n", " ")
            # We want the caret to point to the place in the template that
            # failed to parse, but in a hint we get a open paren at the
            # start. Therefore, we print "loc + 1" spaces (instead of "loc")
            # to line up the caret with the location of the error.
            inst.hint = spec + "\n" + " " * loc + "^ " + _("here")
        raise


def _quote(s):
    r"""Quote a value in order to make it safe for the revset engine.

    >>> _quote('asdf')
    "'asdf'"
    >>> _quote("asdf'\"")
    '\'asdf\\\'"\''
    >>> _quote('asdf\'')
    "'asdf\\''"
    >>> _quote(1)
    "'1'"
    """
    return "'%s'" % util.escapestr(str(s))


def _hexnode(arg):
    if not isinstance(arg, bytes):
        raise error.Abort(
            _("%%n expects a binary node, not %r") % (arg,),
            hint=_("use %s for a hex node"),
        )
    return node.hex(arg)


def _nodelist(arg):
    if isinstance(arg, (str, bytes)):
        # list() would give the characters of a str or the ints of a node.
        hint = (
            _("use %s for a hex node")
            if isinstance(arg, str)
            else _("use %n for a binary node")
        )
        raise error.Abort(
            _("%%ln expects a list of binary nodes, not %r") % (arg,), hint=hint
        )
    nodes = list(arg)
    for n in nodes:
        if not isinstance(n, bytes):
            raise error.Abort(
                _("%%ln expects a list of binary nodes, but it has %r") % (n,),
                hint=_("use %ls for a list of hex nodes"),
            )
    return nodes


def formatspec(expr, *args):
    """
    This is a convenience function for using revsets internally, and
    escapes arguments appropriately. Aliases are intentionally ignored
    so that intended expression behavior isn't accidentally subverted.

    Supported arguments:

    %r = revset expression, parenthesized
    %d = int(arg), no quoting
    %s = string(arg), escaped and single-quoted
    %b = 'default'
    %n = hex(arg), single-quoted
    %% = a literal '%'

    Prefixing the type with 'l' specifies a parenthesized list of that type.
    Prefixing with 'v' for repetitive arguments separated by ','.

    >>> formatspec('%r:: and %lr', '10 or 11', ("this()", "that()"))
    '(10 or 11):: and ((this()) or (that()))'
    >>> formatspec('%d:: and not %d::', 10, 20)
    "_intlist('10'):: and not _intlist('20')::"
    >>> formatspec('%ld or %ld', [], [1])
    "_list('') or _intlist('1')"
    >>> formatspec('keyword(%s)', 'foo\\\\xe9')
    "keyword('foo\\\\\\\\xe9')"
    >>> b = lambda: 'default'
    >>> b.branch = b
    >>> formatspec('branch(%b)', b)
    "branch('default')"
    >>> formatspec('root(%ls)', ['a', 'b', 'c', 'd'])
    "root(_list('a\\x00b\\x00c\\x00d'))"
    >>> formatspec('foo(%vs,%vd,%vz)', ['a', 'b'], [], [1, 2])
    "foo('a','b',1,2)"
    >>> formatspec('%n', b'\\x11' * 20)
    "'1111111111111111111111111111111111111111'"
    >>> formatspec('%ln', [b'\\x11' * 20, b'\\x22' * 20])
    "_hexlist('1111111111111111111111111111111111111111\\x002222222222222222222222222222222222222222')"
    >>> formatspec('%n', '1' * 40)
    Traceback (most recent call last):
      ...
    sapling.error.Abort: %n expects a binary node, not '1111111111111111111111111111111111111111'
    >>> formatspec('%ln', '1' * 40)
    Traceback (most recent call last):
      ...
    sapling.error.Abort: %ln expects a list of binary nodes, not '1111111111111111111111111111111111111111'
    >>> formatspec('%ln', b'ab')
    Traceback (most recent call last):
      ...
    sapling.error.Abort: %ln expects a list of binary nodes, not b'ab'
    >>> formatspec('%ln', ['1' * 40])
    Traceback (most recent call last):
      ...
    sapling.error.Abort: %ln expects a list of binary nodes, but it has '1111111111111111111111111111111111111111'
    """

    def argtype(c, arg):
        if c == "d":
            # Do not turn an int rev into a string that can confuse
            # the ui.ignorerevnum setting. Wrap with _intlist.
            return "_intlist('%d')" % int(arg)
        elif c == "z":
            # Used by things like "limit(..., %z)". Integer, but not
            # a revision number.
            return "%d" % int(arg)
        elif c == "s":
            return _quote(arg)
        elif c == "r":
            parse(arg)  # make sure syntax errors are confined
            return "(%s)" % arg
        elif c == "n":
            return _quote(_hexnode(arg))
        elif c == "b":
            return _quote("default")

    def listexp(s, t):
        l = len(s)
        if l == 0:
            return "_list('')"
        elif l == 1 and t != "d":
            # Do not turn an int rev into a string that can confuse
            # the ui.ignorerevnum setting.
            return argtype(t, s[0])
        elif t == "d":
            return "_intlist('%s')" % "\0".join("%d" % int(a) for a in s)
        elif t == "s":
            return "_list('%s')" % "\0".join(s)
        elif t == "n":
            return "_hexlist('%s')" % "\0".join(node.hex(a) for a in s)
        elif t == "b":
            return "_list('%s')" % "\0".join("default" for a in s)

        m = l // 2
        return "(%s or %s)" % (listexp(s[:m], t), listexp(s[m:], t))

    expr = str(expr)
    ret = ""
    pos = 0
    arg = 0
    while pos < len(expr):
        c = expr[pos]
        if c == "%":
            pos += 1
            d = expr[pos]
            if d == "%":
                ret += d
            elif d in "dzsnbr":
                ret += argtype(d, args[arg])
                arg += 1
            elif d == "l":
                # a list of some type
                pos += 1
                d = expr[pos]
                items = _nodelist(args[arg]) if d == "n" else list(args[arg])
                ret += listexp(items, d)
                arg += 1
            elif d == "v":
                pos += 1
                d = expr[pos]
                if args[arg]:
                    ret += ",".join(argtype(d, a) for a in args[arg])
                elif ret.endswith(","):
                    # Strip trailing comma.
                    ret = ret.rstrip(",")
                arg += 1
            else:
                raise error.Abort(_("unexpected revspec format character %s") % d)
        else:
            ret += c
        pos += 1

    return ret


def formatlist(exprs, join_op="and"):
    """chain multiple formatted revset expressions together

    >>> formatlist(['date(1)', '2+3', 'range(4, 6)'])
    'date(1) and (2+3) and range(4, 6)'
    >>> formatlist(['user(1)', 'user(2)::', 'user(3)'], 'or')
    'user(1) or (user(2)::) or user(3)'
    """
    assert join_op in {"or", "and"}
    if len(exprs) == 1:
        return exprs[0]
    return f" {join_op} ".join(_maybe_group(e) for e in exprs)


def _maybe_group(spec: str) -> str:
    """optionally surround spec with () to remove ambiguity in expressions

    >>> _maybe_group('abc123')
    'abc123'
    >>> _maybe_group('a+b')
    '(a+b)'
    >>> _maybe_group('a::b')
    '(a::b)'
    >>> _maybe_group('f((a::b-c+d),k::m,10)')
    'f((a::b-c+d),k::m,10)'
    >>> _maybe_group('f()+g()')
    '(f()+g())'
    >>> _maybe_group('(f()+g())')
    '(f()+g())'
    >>> _maybe_group('(f()+g())+k()')
    '((f()+g())+k())'
    """
    need_group = False
    level = 0
    for c in spec:
        if level == 0 and (c.isalnum() or c == "_"):
            continue
        elif c == "(":
            level += 1
        elif c == ")":
            level -= 1
        elif level == 0:
            need_group = True
            break
    if need_group:
        return f"({spec})"
    else:
        return spec


def prettyformat(tree):
    return parser.prettyformat(tree, ("string", "symbol"))


def depth(tree):
    if isinstance(tree, tuple):
        return max(list(map(depth, tree))) + 1
    else:
        return 0


def funcsused(tree):
    if not isinstance(tree, tuple) or tree[0] in ("string", "symbol"):
        return set()
    else:
        funcs = set()
        for s in tree[1:]:
            funcs |= funcsused(s)
        if tree[0] == "func":
            funcs.add(tree[1][1])
        return funcs
