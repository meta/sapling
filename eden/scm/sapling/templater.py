# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# templater.py - template expansion for output
#
# Copyright 2005, 2006 Olivia Mackall <olivia@selenic.com>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.


import os
import re
import types
from dataclasses import dataclass, field
from typing import Dict, List, Optional

from . import (
    color,
    config,
    encoding,
    error,
    extensions,
    hintutil,
    minirst,
    node,
    parser,
    patch,
    registrar,
    revset as revsetmod,
    revsetlang,
    scmutil,
    templatefilters,
    templatekw,
    templatenew,
    util,
)
from .i18n import _

# template parsing

elements = {
    # token-type: binding-strength, primary, prefix, infix, suffix
    "(": (20, None, ("group", 1, ")"), ("func", 1, ")"), None),
    ".": (18, None, None, (".", 18), None),
    "%": (15, None, None, ("%", 15), None),
    "|": (15, None, None, ("|", 15), None),
    "*": (5, None, None, ("*", 5), None),
    "/": (5, None, None, ("/", 5), None),
    "+": (4, None, None, ("+", 4), None),
    "-": (4, None, ("negate", 19), ("-", 4), None),
    "=": (3, None, None, ("keyvalue", 3), None),
    ",": (2, None, None, ("list", 2), None),
    ")": (0, None, None, None, None),
    "integer": (0, "integer", None, None, None),
    "symbol": (0, "symbol", None, None, None),
    "string": (0, "string", None, None, None),
    "template": (0, "template", None, None, None),
    "end": (0, None, None, None, None),
}


def tokenize(program, start, end, term=None):
    """Parse a template expression into a stream of tokens, which must end
    with term if specified"""
    pos = start
    program = str(program)
    while pos < end:
        c = program[pos]
        if c.isspace():  # skip inter-token whitespace
            pass
        elif c in "(=,).%|+-*/":  # handle simple operators
            yield (c, None, pos)
        elif c in "\"'":  # handle quoted templates
            s = pos + 1
            data, pos = _parsetemplate(program, s, end, c)
            yield ("template", data, s)
            pos -= 1
        elif c == "r" and program[pos : pos + 2] in ("r'", 'r"'):
            # handle quoted strings
            c = program[pos + 1]
            s = pos = pos + 2
            while pos < end:  # find closing quote
                d = program[pos]
                if d == "\\":  # skip over escaped characters
                    pos += 2
                    continue
                if d == c:
                    yield ("string", program[s:pos], s)
                    break
                pos += 1
            else:
                raise error.ParseError(_("unterminated string"), s)
        elif c.isdigit():
            s = pos
            while pos < end:
                d = program[pos]
                if not d.isdigit():
                    break
                pos += 1
            yield ("integer", program[s:pos], s)
            pos -= 1
        elif (
            c == "\\"
            and program[pos : pos + 2] in (r"\'", r"\"")
            or c == "r"
            and program[pos : pos + 3] in (r"r\'", r"r\"")
        ):
            # handle escaped quoted strings for compatibility with 2.9.2-3.4,
            # where some of nested templates were preprocessed as strings and
            # then compiled. therefore, \"...\" was allowed. (issue4733)
            #
            # processing flow of _evalifliteral() at 5ab28a2e9962:
            # outer template string    -> stringify()  -> compiletemplate()
            # ------------------------    ------------    ------------------
            # {f("\\\\ {g(\"\\\"\")}"}    \\ {g("\"")}    [r'\\', {g("\"")}]
            #             ~~~~~~~~
            #             escaped quoted string
            if c == "r":
                pos += 1
                token = "string"
            else:
                token = "template"
            quote = program[pos : pos + 2]
            s = pos = pos + 2
            while pos < end:  # find closing escaped quote
                if program.startswith("\\\\\\", pos, end):
                    pos += 4  # skip over double escaped characters
                    continue
                if program.startswith(quote, pos, end):
                    # interpret as if it were a part of an outer string
                    data = parser.unescapestr(program[s:pos])
                    if token == "template":
                        data = _parsetemplate(data, 0, len(data))[0]
                    yield (token, data, s)
                    pos += 1
                    break
                pos += 1
            else:
                raise error.ParseError(_("unterminated string"), s)
        elif c.isalnum() or c in "_":
            s = pos
            pos += 1
            while pos < end:  # find end of symbol
                d = program[pos]
                if not (d.isalnum() or d == "_"):
                    break
                pos += 1
            sym = program[s:pos]
            yield ("symbol", sym, s)
            pos -= 1
        elif c == term:
            yield ("end", None, pos + 1)
            return
        else:
            raise error.ParseError(_("syntax error"), pos)
        pos += 1
    if term:
        raise error.ParseError(_("unterminated template expansion"), start)
    yield ("end", None, pos)


def _parsetemplate(tmpl, start, stop, quote=""):
    r"""
    >>> _parsetemplate('foo{bar}"baz', 0, 12)
    ([('string', 'foo'), ('symbol', 'bar'), ('string', '"baz')], 12)
    >>> _parsetemplate('foo{bar}"baz', 0, 12, quote='"')
    ([('string', 'foo'), ('symbol', 'bar')], 9)
    >>> _parsetemplate('foo"{bar}', 0, 9, quote='"')
    ([('string', 'foo')], 4)
    >>> _parsetemplate(r'foo\"bar"baz', 0, 12, quote='"')
    ([('string', 'foo"'), ('string', 'bar')], 9)
    >>> _parsetemplate(r'foo\\"bar', 0, 10, quote='"')
    ([('string', 'foo\\')], 6)
    """
    parsed = []
    sepchars = "{" + quote
    pos = start
    p = parser.parser(elements)
    try:
        while pos < stop:
            n = min(
                (tmpl.find(c, pos, stop) for c in sepchars), key=lambda n: (n < 0, n)
            )
            if n < 0:
                parsed.append(("string", parser.unescapestr(tmpl[pos:stop])))
                pos = stop
                break
            c = tmpl[n : n + 1]
            bs = (n - pos) - len(tmpl[pos:n].rstrip("\\"))
            if bs % 2 == 1:
                # escaped (e.g. '\{', '\\\{', but not '\\{')
                parsed.append(("string", parser.unescapestr(tmpl[pos : n - 1]) + c))
                pos = n + 1
                continue
            if n > pos:
                parsed.append(("string", parser.unescapestr(tmpl[pos:n])))
            if c == quote:
                return parsed, n + 1

            parseres, pos = p.parse(tokenize(tmpl, n + 1, stop, "}"))
            parsed.append(parseres)

    except error.ParseError as inst:
        if len(inst.args) > 1:  # has location
            loc = inst.args[1]
            # Offset the caret location by the number of newlines before the
            # location of the error, since we will replace one-char newlines
            # with the two-char literal r'\n'.
            offset = tmpl[:loc].count("\n")
            tmpl = tmpl.replace("\n", r"\n")
            # We want the caret to point to the place in the template that
            # failed to parse, but in a hint we get a open paren at the
            # start. Therefore, we print "loc" spaces (instead of "loc - 1")
            # to line up the caret with the location of the error.
            inst.hint = tmpl + "\n" + " " * (loc + offset) + "^ " + _("here")
        raise

    if quote:
        raise error.ParseError(_("unterminated string"), start)
    return parsed, pos


def _unnesttemplatelist(tree):
    """Expand list of templates to node tuple

    >>> def f(tree):
    ...     print(prettyformat(_unnesttemplatelist(tree)))
    >>> f(('template', []))
    (string '')
    >>> f(('template', [('string', 'foo')]))
    (string 'foo')
    >>> f(('template', [('string', 'foo'), ('symbol', 'rev')]))
    (template
      (string 'foo')
      (symbol 'rev'))
    >>> f(('template', [('symbol', 'rev')]))  # template(rev) -> str
    (template
      (symbol 'rev'))
    >>> f(('template', [('template', [('string', 'foo')])]))
    (string 'foo')
    """
    if not isinstance(tree, tuple):
        return tree
    op = tree[0]
    if op != "template":
        return (op,) + tuple(_unnesttemplatelist(x) for x in tree[1:])

    assert len(tree) == 2
    xs = tuple(_unnesttemplatelist(x) for x in tree[1])
    if not xs:
        return ("string", "")  # empty template ""
    elif len(xs) == 1 and xs[0][0] == "string":
        return xs[0]  # fast path for string with no template fragment "x"
    else:
        return (op,) + xs


def parse(tmpl):
    """Parse template string into tree"""
    parsed, pos = _parsetemplate(tmpl, 0, len(tmpl))
    assert pos == len(tmpl), "unquoted template should be consumed"
    return _unnesttemplatelist(("template", parsed))


def parseexpandaliases(repo, tmpl):
    """Parse template string into AST with aliases expanded

    Return () if the template is empty.
    """
    if not tmpl:
        return ()
    aliases = repo.ui.configitems("templatealias")
    tree = parse(tmpl)
    return expandaliases(tree, aliases)


def _parseexpr(expr):
    """Parse a template expression into tree

    >>> _parseexpr('"foo"')
    ('string', 'foo')
    >>> _parseexpr('foo(bar)')
    ('func', ('symbol', 'foo'), ('symbol', 'bar'))
    >>> _parseexpr('foo(')
    Traceback (most recent call last):
      ...
    sapling.error.ParseError: ('not a prefix: end', 4)
    >>> _parseexpr('"foo" "bar"')
    Traceback (most recent call last):
      ...
    sapling.error.ParseError: ('invalid token', 7)
    """
    p = parser.parser(elements)
    tree, pos = p.parse(tokenize(expr, 0, len(expr)))
    if pos != len(expr):
        raise error.ParseError(_("invalid token"), pos)
    return _unnesttemplatelist(tree)


def prettyformat(tree):
    return parser.prettyformat(tree, ("integer", "string", "symbol"))


def compileexp(exp, context, curmethods):
    """Compile parsed template tree to (func, data) pair"""
    t = exp[0]
    if t in curmethods:
        return curmethods[t](exp, context)
    raise error.ParseError(_("unknown method '%s'") % t)


# template evaluation


def getsymbol(exp):
    if exp[0] == "symbol":
        return exp[1]
    raise error.ParseError(_("expected a symbol, got '%s'") % exp[0])


def getlist(x):
    if not x:
        return []
    if x[0] == "list":
        return getlist(x[1]) + [x[2]]
    return [x]


def gettemplate(exp, context):
    """Compile given template tree or load named template from map file;
    returns (func, data) pair"""
    if exp[0] in ("template", "string"):
        return compileexp(exp, context, methods)
    if exp[0] == "symbol":
        # unlike runsymbol(), here 'symbol' is always taken as template name
        # even if it exists in mapping. this allows us to override mapping
        # by web templates, e.g. 'changelogtag' is redefined in map file.
        return context._load(exp[1])
    raise error.ParseError(_("expected template specifier"))


def extractsymbols(repo, templ):
    """Parse template and extract symbols for smartset to prefetch.

    Returns None for empty templates,
    so that callers can fallback to the default template;

    Returns a set of symbols otherwise -
    Note that the set can be empty iff no symbols are found.
    """
    ast = parseexpandaliases(repo, templ)
    if not ast:
        # empty template
        return None
    symbols = set()
    for t in parser.walktree(ast):
        if len(t) < 2 or t[0] != "symbol":
            continue
        symbols.add(t[1])
    return symbols


def findsymbolicname(arg):
    """Find symbolic name for the given compiled expression; returns None
    if nothing found reliably"""
    while True:
        func, data = arg
        if func is runsymbol:
            return data
        elif func is runfilter:
            arg = data[0]
        else:
            return None


def evalrawexp(context, mapping, arg):
    """Evaluate given argument as a bare template object which may require
    further processing (such as folding generator of strings)"""
    func, data = arg
    return func(context, mapping, data)


def evalfuncarg(context, mapping, arg):
    """Evaluate given argument as value type"""
    thing = evalrawexp(context, mapping, arg)
    thing = templatekw.unwrapvalue(thing)
    # evalrawexp() may return string, generator of strings or arbitrary object
    # such as date tuple, but filter does not want generator.
    if isinstance(thing, types.GeneratorType):
        thing = stringify(thing)
    return thing


def evalboolean(context, mapping, arg):
    """Evaluate given argument as boolean, but also takes boolean literals"""
    func, data = arg
    if func is runsymbol:
        thing = func(context, mapping, data, default=None)
        if thing is None:
            # not a template keyword, takes as a boolean literal
            thing = util.parsebool(data)
    else:
        thing = func(context, mapping, data)
    thing = templatekw.unwrapvalue(thing)
    if isinstance(thing, bool):
        return thing
    # other objects are evaluated as strings, which means 0 is True, but
    # empty dict/list should be False as they are expected to be ''
    return bool(stringify(thing))


def evalinteger(context, mapping, arg, err=None):
    v = evalfuncarg(context, mapping, arg)
    try:
        return int(v)
    except (TypeError, ValueError):
        raise error.ParseError(err or _("not an integer"))


def evalstring(context, mapping, arg):
    return stringify(evalrawexp(context, mapping, arg))


def evalstringliteral(context, mapping, arg):
    """Evaluate given argument as string template, but returns symbol name
    if it is unknown"""
    func, data = arg
    if func is runsymbol:
        thing = func(context, mapping, data, default=data)
    else:
        thing = func(context, mapping, data)
    return stringify(thing)


_evalfuncbytype = {bool: evalboolean, bytes: evalstring, int: evalinteger}


def evalastype(context, mapping, arg, typ):
    """Evaluate given argument and coerce its type"""
    try:
        f = _evalfuncbytype[typ]
    except KeyError:
        raise error.ProgrammingError("invalid type specified: %r" % typ)
    return f(context, mapping, arg)


def runinteger(context, mapping, data):
    return int(data)


def runstring(context, mapping, data):
    return data


def _recursivesymbolblocker(key):
    def showrecursion(**args):
        raise error.Abort(_("recursive reference '%s' in template") % key)

    return showrecursion


def _runrecursivesymbol(context, mapping, key):
    raise error.Abort(_("recursive reference '%s' in template") % key)


def runsymbol(context, mapping, key, default=""):
    v = mapping.get(key)
    if v is None:
        v = context._defaults.get(key)
    if v is None:
        # put poison to cut recursion. we can't move this to parsing phase
        # because "x = {x}" is allowed if "x" is a keyword. (issue4758)
        safemapping = mapping.copy()
        safemapping[key] = _recursivesymbolblocker(key)
        try:
            v = context.process(key, safemapping)
        except TemplateNotFound:
            v = default
    if callable(v):
        try:
            v = util.checksignature(v)(**mapping)
        except error.SignatureError:
            # If certain data requested is missing. Pretend the template
            # function does not exist.
            v = default

    return v


def buildtemplate(exp, context):
    ctmpl = [compileexp(e, context, methods) for e in exp[1:]]
    return (runtemplate, ctmpl)


def runtemplate(context, mapping, template):
    for arg in template:
        yield evalrawexp(context, mapping, arg)


def buildfilter(exp, context):
    n = getsymbol(exp[2])
    if n in context._filters:
        filt = context._filters[n]
        arg = compileexp(exp[1], context, methods)
        return (runfilter, (arg, filt))
    if n in funcs:
        f = funcs[n]
        args = _buildfuncargs(exp[1], context, methods, n, f._argspec)
        return (f, args)
    raise error.ParseError(_("unknown function '%s'") % n)


def runfilter(context, mapping, data):
    arg, filt = data
    thing = evalfuncarg(context, mapping, arg)
    try:
        return filt(thing)
    except (ValueError, AttributeError, TypeError):
        sym = findsymbolicname(arg)
        if sym:
            msg = _("template filter '%s' is not compatible with keyword '%s'") % (
                filt.__name__,
                sym,
            )
        else:
            msg = _("incompatible use of template filter '%s'") % filt.__name__
        raise error.Abort(msg)


def buildmap(exp, context):
    darg = compileexp(exp[1], context, methods)
    targ = gettemplate(exp[2], context)
    return (runmap, (darg, targ))


def runmap(context, mapping, data):
    darg, targ = data
    d = evalrawexp(context, mapping, darg)
    if hasattr(d, "itermaps"):
        diter = d.itermaps()
    else:
        try:
            diter = iter(d)
        except TypeError:
            sym = findsymbolicname(darg)
            if sym:
                raise error.ParseError(_("keyword '%s' is not iterable") % sym)
            else:
                raise error.ParseError(_("%r is not iterable") % d)

    for i, v in enumerate(diter):
        lm = mapping.copy()
        lm["index"] = i
        if isinstance(v, dict):
            lm.update(v)
            lm["originalnode"] = mapping.get("node")
            yield evalrawexp(context, lm, targ)
        else:
            # v is not an iterable of dicts, this happen when 'key'
            # has been fully expanded already and format is useless.
            # If so, return the expanded value.
            yield v


def buildmember(exp, context):
    darg = compileexp(exp[1], context, methods)
    memb = getsymbol(exp[2])
    return (runmember, (darg, memb))


def runmember(context, mapping, data):
    darg, memb = data
    d = evalrawexp(context, mapping, darg)
    if hasattr(d, "tomap"):
        lm = mapping.copy()
        lm.update(d.tomap())
        return runsymbol(context, lm, memb)
    if hasattr(d, "get"):
        return _getdictitem(d, memb)

    sym = findsymbolicname(darg)
    if sym:
        raise error.ParseError(_("keyword '%s' has no member") % sym)
    else:
        raise error.ParseError(_("%r has no member") % d)


def buildnegate(exp, context):
    arg = compileexp(exp[1], context, exprmethods)
    return (runnegate, arg)


def runnegate(context, mapping, data):
    data = evalinteger(context, mapping, data, _("negation needs an integer argument"))
    return -data


def buildarithmetic(exp, context, func):
    left = compileexp(exp[1], context, exprmethods)
    right = compileexp(exp[2], context, exprmethods)
    return (runarithmetic, (func, left, right))


def runarithmetic(context, mapping, data):
    func, left, right = data
    left = evalinteger(context, mapping, left, _("arithmetic only defined on integers"))
    right = evalinteger(
        context, mapping, right, _("arithmetic only defined on integers")
    )
    try:
        return func(left, right)
    except ZeroDivisionError:
        raise error.Abort(_("division by zero is not defined"))


def buildfunc(exp, context):
    n = getsymbol(exp[1])
    if n in funcs:
        f = funcs[n]
        args = _buildfuncargs(exp[2], context, exprmethods, n, f._argspec)
        return (f, args)
    if n in context._filters:
        args = _buildfuncargs(exp[2], context, exprmethods, n, argspec=None)
        if len(args) != 1:
            raise error.ParseError(_("filter %s expects one argument") % n)
        f = context._filters[n]
        return (runfilter, (args[0], f))
    raise error.ParseError(_("unknown function '%s'") % n)


def _buildfuncargs(exp, context, curmethods, funcname, argspec):
    """Compile parsed tree of function arguments into list or dict of
    (func, data) pairs

    >>> context = engine(lambda t: (runsymbol, t))
    >>> def fargs(expr, argspec):
    ...     x = _parseexpr(expr)
    ...     n = getsymbol(x[1])
    ...     return _buildfuncargs(x[2], context, exprmethods, n, argspec)
    >>> list(fargs('a(l=1, k=2)', 'k l m').keys())
    ['l', 'k']
    >>> args = fargs('a(opts=1, k=2)', '**opts')
    >>> list(args.keys()), list(args['opts'].keys())
    (['opts'], ['opts', 'k'])
    """

    def compiledict(xs):
        return util.sortdict(
            (k, compileexp(x, context, curmethods)) for k, x in xs.items()
        )

    def compilelist(xs):
        return [compileexp(x, context, curmethods) for x in xs]

    if not argspec:
        # filter or function with no argspec: return list of positional args
        return compilelist(getlist(exp))

    # function with argspec: return dict of named args
    _poskeys, varkey, _keys, optkey = argspec = parser.splitargspec(argspec)
    treeargs = parser.buildargsdict(
        getlist(exp), funcname, argspec, keyvaluenode="keyvalue", keynode="symbol"
    )
    compargs = util.sortdict()
    if varkey:
        compargs[varkey] = compilelist(treeargs.pop(varkey))
    if optkey:
        compargs[optkey] = compiledict(treeargs.pop(optkey))
    compargs.update(compiledict(treeargs))
    return compargs


def buildkeyvaluepair(exp, content):
    raise error.ParseError(_("can't use a key-value pair in this context"))


# dict of template built-in functions
funcs = {}

templatefunc = registrar.templatefunc(funcs)


@templatefunc("date(date[, fmt])")
def date(context, mapping, args):
    """Format a date. See :prog:`help dates` for formatting
    strings. The default is a Unix date format, including the timezone:
    "Mon Sep 04 15:13:13 2006 0700"."""
    if not (1 <= len(args) <= 2):
        # i18n: "date" is a keyword
        raise error.ParseError(_("date expects one or two arguments"))

    date = evalfuncarg(context, mapping, args[0])
    fmt = None
    if len(args) == 2:
        fmt = evalstring(context, mapping, args[1])
    try:
        if fmt is None:
            return util.datestr(date)
        else:
            return util.datestr(date, fmt)
    except (TypeError, ValueError):
        # i18n: "date" is a keyword
        raise error.ParseError(_("date expects a date information"))


@templatefunc("dict([[key=]value...])", argspec="*args **kwargs")
def dict_(context, mapping, args):
    """Construct a dict from key-value pairs. A key may be omitted if
    a value expression can provide an unambiguous name."""
    data = util.sortdict()

    for v in args["args"]:
        k = findsymbolicname(v)
        if not k:
            raise error.ParseError(_("dict key cannot be inferred"))
        if k in data or k in args["kwargs"]:
            raise error.ParseError(_("duplicated dict key '%s' inferred") % k)
        data[k] = evalfuncarg(context, mapping, v)

    data.update(
        (k, evalfuncarg(context, mapping, v)) for k, v in args["kwargs"].items()
    )
    return templatekw.hybriddict(data)


@templatefunc("diff([includepattern [, excludepattern]])")
def diff(context, mapping, args):
    """Show a diff, optionally
    specifying files to include or exclude."""
    if len(args) > 2:
        # i18n: "diff" is a keyword
        raise error.ParseError(_("diff expects zero, one, or two arguments"))

    def getpatterns(i):
        if i < len(args):
            s = evalstring(context, mapping, args[i]).strip()
            if s:
                return [s]
        return []

    ctx = mapping["ctx"]
    chunks = ctx.diff(match=ctx.match([], getpatterns(0), getpatterns(1)))

    return b"".join(chunks).decode(errors="surrogateescape")


@templatefunc("enabled(extname)", argspec="extname")
def enabled(context, mapping, args):
    """Test if an extension is enabled (ADVANCED)"""
    if "extname" not in args:
        raise error.ParseError(_("enabled expects one argument"))
    extname = evalstring(context, mapping, args["extname"])
    value = extensions.isloaded(extname)
    return value


@templatefunc("files(pattern)")
def files(context, mapping, args):
    """All files of the current changeset matching the pattern. See
    :prog:`help patterns`."""
    if not len(args) == 1:
        # i18n: "files" is a keyword
        raise error.ParseError(_("files expects one argument"))

    raw = evalstring(context, mapping, args[0])
    ctx = mapping["ctx"]
    m = ctx.match([raw])
    files = list(ctx.matches(m))
    return templatekw.showlist("file", files, mapping)


@templatefunc("fill(text[, width[, initialident[, hangindent]]])")
def fill(context, mapping, args):
    """Fill many
    paragraphs with optional indentation. See the "fill" filter."""
    if not (1 <= len(args) <= 4):
        # i18n: "fill" is a keyword
        raise error.ParseError(_("fill expects one to four arguments"))

    text = evalstring(context, mapping, args[0])
    width = 76
    initindent = ""
    hangindent = ""
    if 2 <= len(args) <= 4:
        width = evalinteger(
            context,
            mapping,
            args[1],
            # i18n: "fill" is a keyword
            _("fill expects an integer width"),
        )
        try:
            initindent = evalstring(context, mapping, args[2])
            hangindent = evalstring(context, mapping, args[3])
        except IndexError:
            pass

    return templatefilters.fill(text, width, initindent, hangindent)


@templatefunc("formatnode(node)")
def formatnode(context, mapping, args):
    """Obtain the preferred form of a changeset hash. (DEPRECATED)"""
    if len(args) != 1:
        # i18n: "formatnode" is a keyword
        raise error.ParseError(_("formatnode expects one argument"))

    ui = mapping["ui"]
    node = evalstring(context, mapping, args[0])
    if ui.debugflag:
        return node
    return templatefilters.short(node)


@templatefunc(
    "pad(text, width[, fillchar=' '[, left=False]])", argspec="text width fillchar left"
)
def pad(context, mapping, args):
    """Pad text with a
    fill character."""
    if "text" not in args or "width" not in args:
        # i18n: "pad" is a keyword
        raise error.ParseError(_("pad() expects two to four arguments"))

    width = evalinteger(
        context,
        mapping,
        args["width"],
        # i18n: "pad" is a keyword
        _("pad() expects an integer width"),
    )

    text = evalstring(context, mapping, args["text"])

    left = False
    fillchar = " "
    if "fillchar" in args:
        fillchar = evalstring(context, mapping, args["fillchar"])
        fillchar_width = encoding.colwidth(color.stripeffects(fillchar))
        if fillchar_width != 1:
            # i18n: "pad" is a keyword
            raise error.ParseError(_("pad() expects a single fill character"))
    if "left" in args:
        left = evalboolean(context, mapping, args["left"])

    fillwidth = width - encoding.colwidth(color.stripeffects(text))
    if fillwidth <= 0:
        return text
    if left:
        return fillchar * fillwidth + text
    else:
        return text + fillchar * fillwidth


@templatefunc("indent(text, indentchars[, firstline])")
def indent(context, mapping, args):
    """Indents all non-empty lines
    with the characters given in the indentchars string. An optional
    third parameter will override the indent for the first line only
    if present."""
    if not (2 <= len(args) <= 3):
        # i18n: "indent" is a keyword
        raise error.ParseError(_("indent() expects two or three arguments"))

    text = evalstring(context, mapping, args[0])
    indent = evalstring(context, mapping, args[1])

    if len(args) == 3:
        firstline = evalstring(context, mapping, args[2])
    else:
        firstline = indent

    # the indent function doesn't indent the first line, so we do it here
    return templatefilters.indent(firstline + text, indent)


@templatefunc("get(dict, key[, default])")
def get(context, mapping, args):
    """Get an attribute/key from an object. Some keywords
    are complex types. This function allows you to obtain the value of an
    attribute on these types."""
    if not (2 <= len(args) <= 3):
        # i18n: "get" is a keyword
        raise error.ParseError(_("get() expects two or three arguments"))

    dictarg = evalfuncarg(context, mapping, args[0])
    if not hasattr(dictarg, "get"):
        # i18n: "get" is a keyword
        raise error.ParseError(_("get() expects a dict as first argument"))

    key = evalfuncarg(context, mapping, args[1])
    value = _getdictitem(dictarg, key)
    if value is None and len(args) == 3:
        value = evalrawexp(context, mapping, args[2])
    return value


def _getdictitem(dictarg, key):
    val = dictarg.get(key)
    if val is None:
        return None
    return templatekw.wraphybridvalue(dictarg, key, val)


@templatefunc("if(expr, then[, else])")
def if_(context, mapping, args):
    """Conditionally execute based on the result of
    an expression."""
    if not (2 <= len(args) <= 3):
        # i18n: "if" is a keyword
        raise error.ParseError(_("if expects two or three arguments"))

    test = evalboolean(context, mapping, args[0])
    if test:
        yield evalrawexp(context, mapping, args[1])
    elif len(args) == 3:
        yield evalrawexp(context, mapping, args[2])


@templatefunc("ifcontains(needle, haystack, then[, else])")
def ifcontains(context, mapping, args):
    """Conditionally execute based
    on whether the item "needle" is in "haystack"."""
    if not (3 <= len(args) <= 4):
        # i18n: "ifcontains" is a keyword
        raise error.ParseError(_("ifcontains expects three or four arguments"))

    haystack = evalfuncarg(context, mapping, args[1])
    try:
        # Disable revf64 compatibility when calculating needle.
        # This makes templates like {ifcontains(rev, revset('.'), '@', 'o')}
        # work with revf64 compatibility.
        origf64encode = scmutil.revf64encode
        try:
            scmutil.revf64encode = lambda rev: rev
            needle = evalastype(
                context, mapping, args[0], getattr(haystack, "keytype", None) or bytes
            )
        finally:
            scmutil.revf64encode = origf64encode
    except error.ParseError:
        found = False
    else:
        try:
            found = needle in haystack
        except TypeError:
            sym = findsymbolicname(args[1])
            if sym:
                raise error.ParseError(_("keyword '%s' is not iterable") % sym)
            raise error.ParseError(_("%r is not iterable") % (haystack,))

    if found:
        yield evalrawexp(context, mapping, args[2])
    elif len(args) == 4:
        yield evalrawexp(context, mapping, args[3])


@templatefunc("ifeq(expr1, expr2, then[, else])")
def ifeq(context, mapping, args):
    """Conditionally execute based on
    whether 2 items are equivalent."""
    if not (3 <= len(args) <= 4):
        # i18n: "ifeq" is a keyword
        raise error.ParseError(_("ifeq expects three or four arguments"))

    test = evalstring(context, mapping, args[0])
    match = evalstring(context, mapping, args[1])
    if test == match:
        yield evalrawexp(context, mapping, args[2])
    elif len(args) == 4:
        yield evalrawexp(context, mapping, args[3])


@templatefunc("ifgt(expr1, expr2, then[, else])")
def ifgt(context, mapping, args):
    """Conditionally execute based on whether expr1 is greater than expr2.
    Both expr1 and expr2 need to be integers."""
    if not (3 <= len(args) <= 4):
        # i18n: "ifgt" is a keyword
        raise error.ParseError(_("ifgt expects three or four arguments"))

    expr1 = evalstring(context, mapping, args[0])
    expr2 = evalstring(context, mapping, args[1])
    try:
        gt = int(expr1) > int(expr2)
    except ValueError:
        raise error.ParseError(_("ifgt can only compare integers"))

    if gt:
        yield evalrawexp(context, mapping, args[2])
    elif len(args) == 4:
        yield evalrawexp(context, mapping, args[3])


@templatefunc("case(expr, case1, then1[, case2, then2 ...][, else])")
def case(context, mapping, args):
    """Conditionally execute based on whether the expression matches
    any of the cases."""
    if len(args) < 2:
        # i18n: "case" is a keyword
        raise error.ParseError(_("case expects at least two arguments"))

    test = evalstring(context, mapping, args[0])
    for index in range(1, len(args) - 1, 2):
        match = evalstring(context, mapping, args[index])
        if test == match:
            yield evalrawexp(context, mapping, args[index + 1])
            return
    if len(args) % 2 == 0:
        yield evalrawexp(context, mapping, args[-1])


@templatefunc("join(list, sep)")
def join(context, mapping, args):
    """Join items in a list with a delimiter."""
    if not (1 <= len(args) <= 2):
        # i18n: "join" is a keyword
        raise error.ParseError(_("join expects one or two arguments"))

    # TODO: perhaps this should be evalfuncarg(), but it can't because hgweb
    # abuses generator as a keyword that returns a list of dicts.
    joinset = evalrawexp(context, mapping, args[0])
    joinset = templatekw.unwrapvalue(joinset)
    joinfmt = getattr(joinset, "joinfmt", util.identity)
    joiner = " "
    if len(args) > 1:
        joiner = evalstring(context, mapping, args[1])

    first = True
    for x in joinset:
        if first:
            first = False
        else:
            yield joiner
        yield joinfmt(x)


@templatefunc("label(label, expr)")
def label(context, mapping, args):
    """Apply a label to generated content. Content with
    a label applied can result in additional post-processing, such as
    automatic colorization."""
    if len(args) != 2:
        # i18n: "label" is a keyword
        raise error.ParseError(_("label expects two arguments"))

    ui = mapping["ui"]
    thing = evalstring(context, mapping, args[1])
    # preserve unknown symbol as literal so effects like 'red', 'bold',
    # etc. don't need to be quoted
    label = evalstringliteral(context, mapping, args[0])

    return ui.label(thing, label)


@templatefunc("localdate(date[, tz])")
def localdate(context, mapping, args):
    """Converts a date to the specified timezone.
    The default is local date."""
    if not (1 <= len(args) <= 2):
        # i18n: "localdate" is a keyword
        raise error.ParseError(_("localdate expects one or two arguments"))

    date = evalfuncarg(context, mapping, args[0])
    try:
        date = util.parsedate(date)
    except AttributeError:  # not str nor date tuple
        # i18n: "localdate" is a keyword
        raise error.ParseError(_("localdate expects a date information"))
    if len(args) >= 2:
        tzoffset = None
        tz = evalfuncarg(context, mapping, args[1])
        if isinstance(tz, str):
            tzoffset, remainder = util.parsetimezone(tz)
            if remainder:
                tzoffset = None
        if tzoffset is None:
            try:
                tzoffset = int(tz)
            except (TypeError, ValueError):
                # i18n: "localdate" is a keyword
                raise error.ParseError(_("localdate expects a timezone"))
    else:
        tzoffset = util.makedate()[1]
    return (date[0], tzoffset)


@templatefunc("max(iterable)")
def max_(context, mapping, args, **kwargs):
    """Return the max of an iterable"""
    if len(args) != 1:
        # i18n: "max" is a keyword
        raise error.ParseError(_("max expects one argument"))

    iterable = evalfuncarg(context, mapping, args[0])
    try:
        x = max(iterable)
    except (TypeError, ValueError):
        # i18n: "max" is a keyword
        raise error.ParseError(_("max first argument should be an iterable"))
    return templatekw.wraphybridvalue(iterable, x, x)


@templatefunc("min(iterable)")
def min_(context, mapping, args, **kwargs):
    """Return the min of an iterable"""
    if len(args) != 1:
        # i18n: "min" is a keyword
        raise error.ParseError(_("min expects one argument"))

    iterable = evalfuncarg(context, mapping, args[0])
    try:
        x = min(iterable)
    except (TypeError, ValueError):
        # i18n: "min" is a keyword
        raise error.ParseError(_("min first argument should be an iterable"))
    return templatekw.wraphybridvalue(iterable, x, x)


@templatefunc("mod(a, b)")
def mod(context, mapping, args):
    """Calculate a mod b such that a / b + a mod b == a"""
    if not len(args) == 2:
        # i18n: "mod" is a keyword
        raise error.ParseError(_("mod expects two arguments"))

    func = lambda a, b: a % b
    return runarithmetic(context, mapping, (func, args[0], args[1]))


@templatefunc("smallcommitmeta(category)")
def showsmallcommitmeta(context, mapping, args):
    """String. The small commit metadata in the provided category associated with the changeset."""
    category = evalstring(context, mapping, args[0])
    repo = mapping["repo"]
    ctx = mapping["ctx"]
    commitmeta = repo.smallcommitmetadata
    try:
        return commitmeta.read(ctx.node(), category)
    except KeyError:
        return ""


@templatefunc("commitid(scheme)")
def translatecommitid(context, mapping, args):
    to_scheme = evalstring(context, mapping, args[0])
    repo = mapping["repo"]
    ctx = mapping["ctx"]
    translated = repo.commitscheme.translate(ctx.hex(), to_scheme, from_scheme="local")
    if translated:
        return node.hex(translated)
    else:
        return ""


@templatefunc("capitalize(word)")
def capitalize(context, mapping, args):
    """Capitalize the first letter"""
    if len(args) != 1:
        raise error.ParseError(_("capitalize expects one argument"))
    word = evalfuncarg(context, mapping, args[0])
    return word.capitalize()


@templatefunc("relpath(path)")
def relpath(context, mapping, args):
    """Convert a repository-absolute path into a filesystem path relative to
    the current working directory."""
    if len(args) != 1:
        # i18n: "relpath" is a keyword
        raise error.ParseError(_("relpath expects one argument"))

    repo = mapping["repo"]
    path = evalstring(context, mapping, args[0])
    return repo.pathto(path)


@templatefunc("revset(query[, formatargs...])")
def revset(context, mapping, args):
    """Execute a revision set query. See
    :prog:`help revset`."""
    if not len(args) > 0:
        # i18n: "revset" is a keyword
        raise error.ParseError(_("revset expects one or more arguments"))

    raw = evalstring(context, mapping, args[0])
    ctx = mapping["ctx"]
    repo = ctx.repo()

    def query(expr):
        m = revsetmod.match(repo.ui, expr, repo=repo)
        # support template like: ifcontains(rev, revset('{node} and age(\'<10m\')'), ...)
        with repo.ui.configoverride({("ui", "ignorerevnum"): False}):
            return m(repo)

    if len(args) > 1:
        formatargs = [evalfuncarg(context, mapping, a) for a in args[1:]]
        revs = query(revsetlang.formatspec(raw, *formatargs))
    else:
        revsetcache = mapping["cache"].setdefault("revsetcache", {})
        if raw in revsetcache:
            revs = revsetcache[raw]
        else:
            revs = query(raw)
            revsetcache[raw] = revs

    return templatekw.showrevslist("revision", revs, **mapping)


@templatefunc("rstdoc(text, style)")
def rstdoc(context, mapping, args):
    """Format reStructuredText."""
    if len(args) != 2:
        # i18n: "rstdoc" is a keyword
        raise error.ParseError(_("rstdoc expects two arguments"))

    text = evalstring(context, mapping, args[0])
    style = evalstring(context, mapping, args[1])

    return minirst.format(text, style=style, keep=["verbose"])


@templatefunc("separate(sep, args)", argspec="sep *args")
def separate(context, mapping, args):
    """Add a separator between non-empty arguments."""
    if "sep" not in args:
        # i18n: "separate" is a keyword
        raise error.ParseError(_("separate expects at least one argument"))

    sep = evalstring(context, mapping, args["sep"])
    first = True
    for arg in args["args"]:
        argstr = evalstring(context, mapping, arg)
        if not argstr:
            continue
        if first:
            first = False
        else:
            yield sep
        yield argstr


@templatefunc("shortest(node, minlength=4)")
def shortest(context, mapping, args):
    """Obtain the shortest representation of
    a node."""
    if not (1 <= len(args) <= 2):
        # i18n: "shortest" is a keyword
        raise error.ParseError(_("shortest() expects one or two arguments"))

    node = evalstring(context, mapping, args[0])

    minlength = 4
    if len(args) > 1:
        minlength = evalinteger(
            context,
            mapping,
            args[1],
            # i18n: "shortest" is a keyword
            _("shortest() expects an integer minlength"),
        )

    # _partialmatch() of filtered changelog could take O(len(repo)) time,
    # which would be unacceptably slow. so we look for hash collision in
    # unfiltered space, which means some hashes may be slightly longer.
    cl = mapping["ctx"]._repo.changelog
    return cl.shortest(node, minlength)


@templatefunc("strip(text[, chars])")
def strip(context, mapping, args):
    """Strip characters from a string. By default,
    strips all leading and trailing whitespace."""
    if not (1 <= len(args) <= 2):
        # i18n: "strip" is a keyword
        raise error.ParseError(_("strip expects one or two arguments"))

    text = evalstring(context, mapping, args[0])
    if len(args) == 2:
        chars = evalstring(context, mapping, args[1])
        return text.strip(chars)
    return text.strip()


@templatefunc("stat(style=none)", argspec="style")
def showdiffstat(context, mapping, args):
    """String. Return diffstat-style summary of changes.

    If 'style' is not 'none', it could be 'status', in which case "added",
    "changed", "removed" will be shown before file names.
    """
    if "style" in args:
        style = args["style"][1]
    else:
        style = "none"

    repo = mapping["repo"]
    ctx = mapping["ctx"]
    revcache = mapping["revcache"]
    width = repo.ui.termwidth()

    if style == "none":
        status = None
    elif style == "status":
        status = templatekw.getfiles(repo, ctx, revcache)
    else:
        raise error.ParseError(_("stat does not support style %r") % (style,))

    return patch.diffstat(
        util.iterlines(ctx.diff(noprefix=False)), width=width, status=status
    )


@templatefunc("sub(pattern, replacement, expression)")
def sub(context, mapping, args):
    """Perform text substitution
    using regular expressions."""
    if len(args) != 3:
        # i18n: "sub" is a keyword
        raise error.ParseError(_("sub expects three arguments"))

    pat = evalstring(context, mapping, args[0])
    rpl = evalstring(context, mapping, args[1])
    src = evalstring(context, mapping, args[2])
    try:
        patre = re.compile(pat)
    except re.error:
        # i18n: "sub" is a keyword
        raise error.ParseError(_("sub got an invalid pattern: %s") % pat)
    try:
        yield patre.sub(rpl, src)
    except re.error:
        # i18n: "sub" is a keyword
        raise error.ParseError(_("sub got an invalid replacement: %s") % rpl)


@templatefunc("startswith(pattern, text)")
def startswith(context, mapping, args):
    """Returns the value from the "text" argument
    if it begins with the content from the "pattern" argument."""
    if len(args) != 2:
        # i18n: "startswith" is a keyword
        raise error.ParseError(_("startswith expects two arguments"))

    patn = evalstring(context, mapping, args[0])
    text = evalstring(context, mapping, args[1])
    if text.startswith(patn):
        return text
    return ""


@templatefunc("filterby(pattern, files)")
def filterby(context, mapping, args):
    """Returns the files that match the given "pattern". See :prog:`help patterns`."""
    if len(args) != 2:
        raise error.ParseError(_("filterby expects two arguments"))

    patn = evalstring(context, mapping, args[0])
    texts = evalfuncarg(context, mapping, args[1])

    ctx = mapping["ctx"]
    m = ctx.match([patn])
    texts = [t for t in texts if m.matchfn(t)]
    return templatekw.showlist("text", texts, mapping)


@templatefunc("truncate(text, maxlines, [suffix])")
def truncate(context, mapping, args):
    """Truncate text to no more than "maxlines" in length. If "suffix" is
    supplied, then it replaces the last line if, and only if, the text was
    truncated."""
    if not (2 <= len(args) <= 3):
        # i18n: "truncate" is a keyword
        raise error.ParseError(
            _("truncate expects two or three arguments, got %d") % len(args)
        )
    text = evalstring(context, mapping, args[0])
    maxlines = evalinteger(
        context,
        mapping,
        args[1],
        # i18n: "truncate" is a keyword
        _("truncate expects an integer line count"),
    )
    if len(args) == 3:
        suffix = evalstring(context, mapping, args[2])
        truncatelines = maxlines - 1
    else:
        suffix = None
        truncatelines = maxlines

    lines = text.splitlines(True)
    if len(lines) <= maxlines:
        return text

    lines = lines[0:truncatelines]
    if suffix:
        lines.append(suffix)
    return "".join(lines)


@templatefunc("truncatelonglines(text, maxwidth, [suffix])")
def truncatelonglines(context, mapping, args):
    """Truncate lines in text to no more than "maxwidth" in width. If "suffix"
    is supplied, then it replaces the last characters on the line if, and only
    if, the lines was truncated."""
    if not (2 <= len(args) <= 3):
        # i18n: "truncatelonglines" is a keyword
        raise error.ParseError(
            _("truncatelonglines expects two or three arguments, got %d") % len(args)
        )
    text = evalstring(context, mapping, args[0])
    maxwidth = evalinteger(
        context,
        mapping,
        args[1],
        # i18n: "truncatelonglines" is a keyword
        _("truncatelonglines expects an integer line width"),
    )
    truncatedwidth = maxwidth
    if len(args) == 3:
        suffix = evalstring(context, mapping, args[2])
        if isinstance(suffix, bytes):
            # Python 2
            truncatedwidth -= len(suffix.decode("utf-8"))
        else:
            # Python 3
            truncatedwidth -= len(suffix)
    else:
        suffix = None

    if truncatedwidth < 0:
        truncatedwidth = 0

    lines = text.splitlines(True)
    output = []
    for line in lines:
        stripped = line.rstrip("\r\n")
        if len(stripped) > maxwidth:
            eol = line[len(stripped) :]
            line = stripped[:truncatedwidth]
            if suffix is not None:
                line += suffix
            line += eol

        output.append(line)

    return "".join(output)


@templatefunc("word(number, text[, separator])")
def word(context, mapping, args):
    """Return the nth word from a string."""
    if not (2 <= len(args) <= 3):
        # i18n: "word" is a keyword
        raise error.ParseError(
            _("word expects two or three arguments, got %d") % len(args)
        )

    num = evalinteger(
        context,
        mapping,
        args[0],
        # i18n: "word" is a keyword
        _("word expects an integer index"),
    )
    text = evalstring(context, mapping, args[1])
    if len(args) == 3:
        splitter = evalstring(context, mapping, args[2])
    else:
        splitter = None

    tokens = text.split(splitter)
    if num >= len(tokens) or num < -len(tokens):
        return ""
    else:
        return tokens[num]


@templatefunc("triggerhint(name)")
def triggerhint(context, mapping, args):
    """Trigger the hint named by the argument.  Expands to the empty string."""
    if len(args) != 1:
        raise error.ParseError(
            _("triggerhint expects one argument, got %s") % len(args)
        )
    name = evalstring(context, mapping, args[0])
    hintutil.trigger(name)
    return ""


@templatefunc("hyperlink(link, title)")
def hyperlink(context, mapping, args):
    """Render hyperlink in terminal"""
    if len(args) != 2:
        raise error.ParseError("hyperlink expects two arguments, got %s" % len(args))

    title = evalstring(context, mapping, args[1])
    if not title:
        return ""

    ui = mapping["ui"]
    hyperlink = ui.formatted and ui.configbool("ui", "hyperlink", False)

    if hyperlink:
        link = evalstring(context, mapping, args[0])
        return color.hyperlink(link, title)
    return title


@dataclass(slots=True)
class _SummaryDir:
    # None means this directory already reached the limit, so descent stopped.
    children: Optional[Dict[str, "_SummaryDir"]] = field(default_factory=dict)
    files: List[str] = field(default_factory=list)
    # Descendant files, excluding files directly in this directory.
    count: int = 0


class _PathSummary:
    def __init__(self, limit):
        self.root = _SummaryDir()
        self.limit = limit

    def add(self, path):
        target = self.root
        elems = path.split("/")
        for elem in elems[:-1]:
            target.count += 1
            if target.children is None or len(target.children) == self.limit:
                # Deeper names can no longer affect this directory's summary.
                target.children = None
                return
            if elem not in target.children:
                target.children[elem] = _SummaryDir()
            target = target.children[elem]
        target.files.append(elems[-1])

    def buildchildren(self, limit, directory, prefix):
        # Reserve one line per child and, if needed, one for direct files.
        havefiles = 1 if directory.files else 0
        if directory.children is None or len(directory.children) + havefiles > limit:
            total = directory.count + len(directory.files)
            return [f"{prefix}… ({total} files)"]

        # Each child already owns one line; distribute remaining lines recursively.
        items = []
        surplus = limit - len(directory.children) - havefiles
        for childelem, childdir in sorted(directory.children.items()):
            childlimit = surplus + 1
            childprefix = prefix + childelem + "/"
            childitems = self.buildchildren(childlimit, childdir, childprefix)
            surplus -= len(childitems) - 1
            items.extend(childitems)

        # Spend any remaining lines on files directly in this directory.
        filecount = len(directory.files)
        if surplus + 1 >= filecount:
            items.extend(f"{prefix}{f}" for f in directory.files)
        elif prefix:
            items.append(f"{prefix} ({filecount} files)")
        else:
            items.append(f"({filecount} files)")

        return items

    def build(self):
        return self.buildchildren(self.limit, self.root, "")


def summarizepaths(files, limit=5):
    summary = _PathSummary(limit)
    for path in files:
        summary.add(path)
    return summary.build()


@templatefunc("pathsummary(files, limit)")
def pathsummary(context, mapping, args):
    """List of files.  Returns a summary of those files' paths.

    The summary is limited to `limit` lines (default: 5).

    Where multiple files are changed in the same directory, it may be summarized as
    `path/to/dir (5 files)`.

    Where multiple files are changed in a directory tree, the whole tree may be
    summarized as `path/to/top/dir/… (70 files)`.
    """

    if len(args) not in (1, 2):
        raise error.ParseError(
            "pathsummary expects one or two arguments, got %s" % len(args)
        )
    files = evalfuncarg(context, mapping, args[0])
    limit = 5
    if len(args) == 2:
        limit = evalinteger(context, mapping, args[1])
    return templatekw.hybridlist(summarizepaths(files, limit), "path")


# methods to interpret function arguments or inner expressions (e.g. {_(x)})
exprmethods = {
    "integer": lambda e, c: (runinteger, e[1]),
    "string": lambda e, c: (runstring, e[1]),
    "symbol": lambda e, c: (runsymbol, e[1]),
    "template": buildtemplate,
    "group": lambda e, c: compileexp(e[1], c, exprmethods),
    ".": buildmember,
    "|": buildfilter,
    "%": buildmap,
    "func": buildfunc,
    "keyvalue": buildkeyvaluepair,
    "+": lambda e, c: buildarithmetic(e, c, lambda a, b: a + b),
    "-": lambda e, c: buildarithmetic(e, c, lambda a, b: a - b),
    "negate": buildnegate,
    "*": lambda e, c: buildarithmetic(e, c, lambda a, b: a * b),
    "/": lambda e, c: buildarithmetic(e, c, lambda a, b: a // b),
}

# methods to interpret top-level template (e.g. {x}, {x|_}, {x % "y"})
methods = exprmethods.copy()
methods["integer"] = exprmethods["symbol"]  # '{1}' as variable


class _aliasrules(parser.basealiasrules):
    """Parsing and expansion rule set of template aliases"""

    _section = _("template alias")
    _parse = staticmethod(_parseexpr)

    @staticmethod
    def _trygetfunc(tree):
        """Return (name, args) if tree is func(...) or ...|filter; otherwise
        None"""
        if tree[0] == "func" and tree[1][0] == "symbol":
            return tree[1][1], getlist(tree[2])
        if tree[0] == "|" and tree[2][0] == "symbol":
            return tree[2][1], [tree[1]]


def expandaliases(tree, aliases):
    """Return new tree of aliases are expanded"""
    aliasmap = _aliasrules.buildmap(aliases)
    return _aliasrules.expand(aliasmap, tree)


# template engine

stringify = templatefilters.stringify
byteify = templatefilters.byteify


def _flatten(thing):
    """yield a single stream from a possibly nested set of iterators"""
    thing = templatekw.unwraphybrid(thing)
    if isinstance(thing, str):
        yield thing.encode(errors="surrogateescape")
    elif isinstance(thing, bytes):
        yield thing
    elif thing is None:
        pass
    elif not hasattr(thing, "__iter__"):
        yield str(thing).encode()
    else:
        for i in thing:
            for j in _flatten(i):
                yield j


def unquotestring(s):
    """unwrap quotes if any; otherwise returns unmodified string"""
    if len(s) < 2 or s[0] not in "'\"" or s[0] != s[-1]:
        return s
    return s[1:-1]


class engine:
    """template expansion engine.

    template expansion works like this. a map file contains key=value
    pairs. if value is quoted, it is treated as string. otherwise, it
    is treated as name of template file.

    templater is asked to expand a key in map. it looks up key, and
    looks for strings like this: {foo}. it expands {foo} by looking up
    foo in map, and substituting it. expansion is recursive: it stops
    when there is no more {foo} to replace.

    expansion also allows formatting and filtering.

    format uses key to expand each item in list. syntax is
    {key%format}.

    filter uses function to transform value. syntax is
    {key|filter1|filter2|...}."""

    def __init__(self, loader, filters=None, defaults=None, aliases=()):
        self._loader = loader
        if filters is None:
            filters = {}
        self._filters = filters
        if defaults is None:
            defaults = {}
        self._defaults = defaults
        self._aliasmap = _aliasrules.buildmap(aliases)
        self._cache = {}  # key: (func, data)

    def _load(self, t):
        """load, parse, and cache a template"""
        if t not in self._cache:
            # put poison to cut recursion while compiling 't'
            self._cache[t] = (_runrecursivesymbol, t)
            try:
                x = parse(self._loader(t))
                if self._aliasmap:
                    x = _aliasrules.expand(self._aliasmap, x)
                self._cache[t] = compileexp(x, self, methods)
            except:  # re-raises
                del self._cache[t]
                raise
        return self._cache[t]

    def process(self, t, mapping):
        """Perform expansion. t is name of map element to expand.
        mapping contains added elements for use during expansion. Is a
        generator."""
        func, data = self._load(t)
        return _flatten(func(self, mapping, data))


engines = {"default": engine}


_builtinmapfiles = templatenew.builtinmapfiles

_builtinprefix = "map-cmdline."


def stylelist():
    return ", ".join(sorted(_builtinmapfiles))


def _readbuiltinstyle(path):
    # Translate paths like "a/b/map-cmdline.default" to "default",
    # Then lookup builtin style
    basename = os.path.basename(path)
    if basename.startswith(_builtinprefix):
        basename = basename[len(_builtinprefix) :]
    return _builtinmapfiles.get(basename)


def _readmapfile(mapfile):
    """Load template elements from the given map file"""
    builtin = _readbuiltinstyle(mapfile)
    if builtin is not None:
        return builtin
    if not os.path.exists(mapfile):
        raise error.Abort(
            _("style '%s' not found") % mapfile,
            hint=_("available styles: %s") % stylelist(),
        )

    base = os.path.dirname(mapfile)
    conf = config.config(includepaths=templatepaths())
    conf.read(mapfile, remap={"": "templates"})

    cache = {}
    tmap = {}
    aliases = []

    val = conf.get("templates", "__base__")
    if val and val[0] not in "'\"":
        # treat as a pointer to a base class for this style
        path = util.normpath(os.path.join(base, val))

        # fallback check in template paths
        if not os.path.exists(path):
            for p in templatepaths():
                p2 = util.normpath(os.path.join(p, val))
                if os.path.isfile(p2):
                    path = p2
                    break
                p3 = util.normpath(os.path.join(p2, "map"))
                if os.path.isfile(p3):
                    path = p3
                    break

        cache, tmap, aliases = _readmapfile(path)

    for key, val in conf["templates"].items():
        if not val:
            raise error.ParseError(_("missing value"), conf.source("templates", key))
        if val[0] in "'\"":
            if val[0] != val[-1]:
                raise error.ParseError(
                    _("unmatched quotes"), conf.source("templates", key)
                )
            cache[key] = unquotestring(val)
        elif key != "__base__":
            val = "default", val
            if ":" in val[1]:
                val = val[1].split(":", 1)
            tmap[key] = val[0], os.path.join(base, val[1])
    aliases.extend(conf["templatealias"].items())
    return cache, tmap, aliases


class TemplateNotFound(error.Abort):
    pass


class templater:
    def __init__(
        self,
        filters=None,
        defaults=None,
        cache=None,
        aliases=(),
        minchunk=1024,
        maxchunk=65536,
    ):
        """set up template engine.
        filters is dict of functions. each transforms a value into another.
        defaults is dict of default map definitions.
        aliases is list of alias (name, replacement) pairs.
        """
        if filters is None:
            filters = {}
        if defaults is None:
            defaults = {}
        if cache is None:
            cache = {}
        self.cache = cache.copy()
        self.map = {}
        self.filters = templatefilters.filters.copy()
        self.filters.update(filters)
        self.defaults = defaults
        self._aliases = aliases
        self.minchunk, self.maxchunk = minchunk, maxchunk
        self.ecache = {}

    @classmethod
    def frommapfile(
        cls,
        mapfile,
        filters=None,
        defaults=None,
        cache=None,
        minchunk=1024,
        maxchunk=65536,
    ):
        """Create templater from the specified map file"""
        t = cls(filters, defaults, cache, [], minchunk, maxchunk)
        cache, tmap, aliases = _readmapfile(mapfile)
        t.cache.update(cache)
        t.map = tmap
        t._aliases = aliases
        return t

    def __contains__(self, key):
        return key in self.cache or key in self.map

    def load(self, t):
        """Get the template for the given template name. Use a local cache."""
        if t not in self.cache:
            try:
                self.cache[t] = util.readfile(self.map[t][1]).decode()
            except KeyError as inst:
                raise TemplateNotFound(_('"%s" not in template map') % inst.args[0])
            except IOError as inst:
                raise IOError(
                    inst.args[0],
                    _("template file %s: %s") % (self.map[t][1], inst.args[1]),
                )
        return self.cache[t]

    def render(self, mapping):
        """Render the default unnamed template and return result as string"""
        mapping = mapping
        return stringify(self("", **mapping))

    def __call__(self, t, **mapping):
        mapping = mapping
        ttype = t in self.map and self.map[t][0] or "default"
        if ttype not in self.ecache:
            try:
                ecls = engines[ttype]
            except KeyError:
                raise error.Abort(_("invalid template engine: %s") % ttype)
            self.ecache[ttype] = ecls(
                self.load, self.filters, self.defaults, self._aliases
            )
        proc = self.ecache[ttype]

        stream = proc.process(t, mapping)
        if self.minchunk:
            stream = util.increasingchunks(stream, min=self.minchunk, max=self.maxchunk)
        return stream


def templatepaths():
    """return locations used for template files."""
    pathsrel = ["templates"]
    paths = [os.path.normpath(os.path.join(util.datapath, f)) for f in pathsrel]
    return [p for p in paths if os.path.isdir(p)]


def isbuiltin(name):
    return name in _builtinmapfiles


def templatepath(name):
    """return location of template file. returns None if not found."""
    if name.startswith(_builtinprefix):
        # Strip the common prefix (compatibility tech debt)
        builtinname = name[len(_builtinprefix) :]
        if isbuiltin(builtinname):
            return builtinname
    if isbuiltin(name):
        return name
    for p in templatepaths():
        f = os.path.join(p, name)
        if os.path.exists(f):
            return f
    return None


def loadfunction(ui, extname, registrarobj):
    """Load template function from specified registrarobj"""
    for name, func in registrarobj._table.items():
        funcs[name] = func


# tell hggettext to extract docstrings from these functions:
i18nfunctions = funcs.values()
