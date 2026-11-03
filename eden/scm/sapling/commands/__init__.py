# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# commands.py - command processing for mercurial
#
# Copyright 2005-2007 Olivia Mackall <olivia@selenic.com>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.

# @lint-ignore-every SPELL


import difflib
import errno
import os
import pathlib
import re
import shlex
import sys
import time

import bindings

from .. import (
    agent,
    annotate as annotatemod,
    archival,
    autopull,
    bookmarks,
    bundle2,
    changegroup,
    cloneuri,
    cmdutil,
    color,
    context,
    copies,
    destutil,
    dirstateguard,
    discovery,
    encoding,
    error,
    exchange,
    extensions,
    formatter,
    git,
    hbisect,
    help,
    hg,
    hgdemandimport,
    hintutil,
    identity,
    lock as lockmod,
    match as matchmod,
    merge as mergemod,
    mutation,
    patch,
    phases,
    rcutil,
    registrar,
    revsetlang,
    rewriteutil,
    scmutil,
    server,
    sshserver,
    templatekw,
    templater,
    ui as uimod,
    util,
)
from ..i18n import _
from ..node import hex, nullid, short
from ..utils import pathaclutil, sparseutil, subtreeutil
from . import cmdtable

with hgdemandimport.deactivated():
    # Importing these modules have side effect on the command table.
    from . import (  # noqa: F401
        blackbox,
        clean as cleancmd,
        debug,
        debugbenchmark,
        debugcheckoutidentifier,
        debugconflictcontext,
        debugdirs,
        debugdryup,
        debugmergetext,
        debugmetalog,
        debugmutation,
        debugrebuildchangelog,
        debugrunshell,
        debugsendunbundle,
        debugstack,
        debugstatus,
        debugstrip,
        doctor,
        eden,
        fs,
        isl,
        lint,
        subtree,
        uncommit,
    )

release = lockmod.release
table = cmdtable.table
command = cmdtable.command


readonly = registrar.command.readonly

# common command options

globalopts = cmdutil._typedflags(
    [
        (
            "R",
            "repository",
            "",
            _("repository root directory or name of overlay bundle file"),
            _("REPO"),
        ),
        ("", "cwd", "", _("change working directory"), _("DIR")),
        (
            "y",
            "noninteractive",
            False,
            _("do not prompt, use the default answer for all prompts"),
        ),
        ("q", "quiet", False, _("suppress output")),
        ("v", "verbose", False, _("enable additional output")),
        (
            "",
            "color",
            "",
            # i18n: 'always', 'auto', 'never', and 'debug' are keywords
            # and should not be translated
            _("when to colorize (boolean, always, auto, never, or debug)"),
            _("TYPE"),
        ),
        (
            "",
            "config",
            [],
            _("set/override config option (use 'section.name=value')"),
            _("CONFIG"),
        ),
        ("", "configfile", [], _("enables the given config file"), _("FILE")),
        ("", "debug", False, _("enable debugging output")),
        ("", "debugger", False, _("start debugger")),
        ("", "encoding", encoding.encoding, _("set the charset encoding"), _("ENCODE")),
        (
            "",
            "encodingmode",
            encoding.encodingmode,
            _("set the charset encoding mode"),
            _("MODE"),
        ),
        ("", "insecure", False, _("do not verify server certificate")),
        (
            "",
            "outputencoding",
            encoding.outputencoding or "same as encoding",
            _("set the output encoding"),
            _("ENCODE"),
        ),
        ("", "traceback", False, _("always print a traceback on exception")),
        ("", "trace", False, _("enable more detailed tracing")),
        ("", "time", False, _("time how long the command takes")),
        ("", "profile", False, _("print command execution profile")),
        ("", "version", False, _("output version information and exit")),
        ("h", "help", False, _("display help and exit")),
        ("", "hidden", False, _("consider hidden changesets")),
        (
            "",
            "pager",
            "auto",
            _("when to paginate (boolean, always, auto, or never)"),
            _("TYPE"),
        ),
        ("", "reason", [], _("why this runs, usually set by automation (ADVANCED)")),
    ]
)

dryrunopts = cmdutil.dryrunopts
walkopts = cmdutil.walkopts
untrackedfileopts = cmdutil.untrackedfileopts
commitopts = cmdutil.commitopts
commitopts2 = cmdutil.commitopts2
formatteropts = cmdutil.formatteropts
templateopts = cmdutil.templateopts
logopts = cmdutil.logopts
diffopts = cmdutil.diffopts
diffwsopts = cmdutil.diffwsopts
diffopts2 = cmdutil.diffopts2
mergetoolopts = cmdutil.mergetoolopts
similarityopts = cmdutil.similarityopts
debugrevlogopts = cmdutil.debugrevlogopts
diffgraftopts = cmdutil.diffgraftopts

# Commands start here, listed alphabetically


@command("add", walkopts + dryrunopts, _("[OPTION]... [FILE]..."), inferrepo=True)
def add(ui, repo, *pats, **opts):
    """start tracking the specified files

    Specify files to be tracked by @Product@. The files will be added to
    the repository at the next commit.

    To undo an add before files have been committed, use :prog:`forget`.
    To undo an add after files have been committed, use :prog:`rm`.

    If no names are given, add all files to the repository (except
    files matching ``.gitignore``).

    .. container:: verbose

       Examples:

         - New (unknown) files are added
           automatically by :prog:`add`::

             $ ls
             foo.c
             $ @prog@ status
             ? foo.c
             $ @prog@ add
             adding foo.c
             $ @prog@ status
             A foo.c

         - Add specific files::

             $ ls
             bar.c  foo.c
             $ @prog@ status
             ? bar.c
             ? foo.c
             $ @prog@ add bar.c
             $ @prog@ status
             A bar.c
             ? foo.c

    Returns 0 if all files are successfully added.
    """

    m = scmutil.match(repo[None], pats, opts)
    rejected = cmdutil.add(ui, repo, m, "", False, **opts)
    return rejected and 1 or 0


@command(
    "addremove|addrm",
    similarityopts + walkopts + dryrunopts,
    _("[OPTION]... [FILE]..."),
    inferrepo=True,
    legacyaliases=["addr", "addre", "addrem", "addremo", "addremov"],
)
def addremove(ui, repo, *pats, **opts):
    """add all new files, delete all missing files

    Start tracking all new files and stop tracking all missing files
    in the working copy. As with :prog:`add`, these changes take
    effect at the next commit.

    Unless file names are given, new files are ignored if they match any of
    the patterns in ``.gitignore``.

    Use the ``-s/--similarity`` option to detect renamed files. This
    option takes a percentage between 0 (disabled) and 100 (files must
    be identical) as its parameter. With a parameter greater than 0,
    this compares every removed file with every added file and records
    those similar enough as renames. Detecting renamed files this way
    can be expensive. After using this option, :prog:`status -C` can be
    used to check which files were identified as moved or renamed. If
    not specified, ``-s/--similarity`` defaults to 100, only marking
    identical files renamed.

    .. container:: verbose

       Examples:

         - Files bar.c and foo.c are new,
           while foobar.c has been removed (without using :prog:`remove`)
           from the repository::

             $ ls
             bar.c foo.c
             $ @prog@ status
             ! foobar.c
             ? bar.c
             ? foo.c
             $ @prog@ addremove
             adding bar.c
             adding foo.c
             removing foobar.c
             $ @prog@ status
             A bar.c
             A foo.c
             R foobar.c

         - A file foobar.c was moved to foo.c without using :prog:`rename`.
           Afterwards, it was edited slightly::

             $ ls
             foo.c
             $ @prog@ status
             ! foobar.c
             ? foo.c
             $ @prog@ addremove --similarity 90
             removing foobar.c
             adding foo.c
             recording removal of foobar.c as rename to foo.c (94% similar)
             $ @prog@ status -C
             A foo.c
               foobar.c
             R foobar.c

    Returns 0 if all files are successfully added/removed.
    """
    matcher = scmutil.match(repo[None], pats, opts)
    return scmutil.addremove(
        repo,
        matcher,
        addremove=True,
        automv=False,
        similarity=opts.get("similarity") or 100,
        dry_run=opts.get("dry_run"),
    )


@command(
    "annotate|blame|an",
    [
        ("r", "rev", "", _("annotate the specified revision"), _("REV")),
        ("", "no-follow", False, _("don't follow copies and renames")),
        ("a", "text", None, _("treat all files as text")),
        ("u", "user", None, _("list the author (long with -v)")),
        ("f", "file", None, _("list the filename")),
        ("d", "date", None, _("list the date (short with -q)")),
        ("n", "number", None, _("list the revision number (default)")),
        ("c", "changeset", None, _("list the changeset")),
        ("l", "line-number", None, _("show line number at the first appearance")),
        ("", "short-date", None, _("list the brief date (EXPERIMENTAL)")),
    ]
    + diffwsopts
    + walkopts
    + formatteropts,
    _("[OPTION] [-r REV] FILE..."),
    inferrepo=True,
    legacyaliases=["blam", "blam", "ann", "anno", "annot", "annota", "annotat"],
)
def annotate(ui, repo, *pats, **opts):
    """show per-line commit information for given files

    Show file contents where each line is annotated with information
    about the commit that last changed that line.

    This command is useful for discovering when a change was made and
    by whom.

    If you include ``--file``, ``--user``, or ``--date``, the revision number is
    suppressed unless you also include ``--number``.

    Without the ``-a/--text`` option, annotate will skip binary files.
    With ``-a``, binary files will be annotated anyway.

    Cross‑repo commits introduced through subtree import appear with a “~” suffix
    in the output.

    Returns 0 on success.
    """
    if not pats:
        raise error.Abort(_("at least one filename or pattern is required"))

    for f in ui.configlist("annotate", "default-flags"):
        if opts.get(f) is None:
            opts[f] = True

    if opts.get("date") is None:
        # If --date wasn't specified, allow short-date to flow from config.
        opts["date"] = opts.get("short-date")
    else:
        # User specified a date flag - that overrides short-date.
        opts.pop("short-date", None)

    ctx = scmutil.revsingle(repo, opts.get("rev"))
    curr_origin_url = repo.origin_url()

    rootfm = ui.formatter("annotate", opts)
    # short-date exists to allow us to enable brief date display via
    # config without needing to enable --quiet.
    if ui.quiet or opts.get("short-date"):
        datefunc = util.shortdate
    else:
        datefunc = util.datestr

    def format_changeset_helper(
        hex: str, annotated_line: annotatemod.annotateline
    ) -> str:
        if annotated_line.origin_url() != curr_origin_url:
            title = "%s~" % hex
        else:
            title = "%s" % hex

        if ui.formatted and ui.configbool("blame", "hyperlink"):
            link = subtreeutil.xrepo_link(
                repo,
                annotated_line.origin_url(),
                hex,
                annotated_line.path(),
                annotated_line.lineno,
            )
            if link:
                return color.hyperlink(link, title)
        return title

    if ctx.rev() is None:

        def hexfn(node):
            if node is None:
                return None
            else:
                return rootfm.hexfunc(node)

        if opts.get("changeset"):
            # omit "+" suffix which is appended to node hex
            def formatrev(rev):
                if rev is None:
                    return "%d" % ctx.p1().rev()
                else:
                    return "%d" % rev

        else:

            def formatrev(rev):
                if rev is None:
                    return "%d+" % ctx.p1().rev()
                else:
                    return "%d " % rev

        def formatchangeset(args):
            hex, annotated_line = args
            if hex is None:
                return "%s+" % rootfm.hexfunc(ctx.p1().node())
            else:
                return format_changeset_helper(hex, annotated_line)

    else:
        hexfn = rootfm.hexfunc
        formatrev = str

        def formatchangeset(args):
            hex, annotated_line = args
            return format_changeset_helper(hex, annotated_line)

    def getphabdiff(annotated_line):
        d = annotated_line.ctx().description()
        pat = r"https://.*/(D\d+)"
        m = re.search(pat, d)
        return m.group(1) if m else ""

    now = time.time()

    def agebucket(
        annotated_line: annotatemod.annotateline, add_xrepo_suffix: bool = False
    ) -> str:
        if add_xrepo_suffix and annotated_line.origin_url() != curr_origin_url:
            suffix = ".xrepo"
        else:
            suffix = ""

        d = annotated_line.date()
        t = d[0]
        day = 86400

        if t > now - 3600:
            bucket = "1hour"
        elif t > now - day:
            bucket = "1day"
        elif t > now - 7 * day:
            bucket = "7day"
        elif t > now - 30 * day:
            bucket = "30day"
        elif t > now - 60 * day:
            bucket = "60day"
        elif t > now - 180 * day:
            bucket = "180day"
        elif t > now - 360 * day:
            bucket = "360day"
        else:
            bucket = "old"

        return bucket + suffix

    opmap = [
        # op, sep, get, fmt
        ("user", " ", lambda x: x.user(), ui.shortuser),
        ("number", " ", lambda x: x.rev(), formatrev),
        (
            "changeset",
            " ",
            lambda x: (hexfn(x.node()), x),
            formatchangeset,
        ),
        ("date", " ", lambda x: x.date(), util.cachefunc(datefunc)),
        ("file", " ", lambda x: x.path(), str),
        ("line_number", ":", lambda x: x.lineno, str),
        ("age_bucket", "", lambda x: agebucket(x), lambda x: ""),
        ("phabdiff", " ", getphabdiff, str),
    ]
    fieldnamemap = {"number": "rev", "changeset": "node", "phabdiff": "blame_phabdiff"}

    if (
        not opts.get("user")
        and not opts.get("changeset")
        and not opts.get("date")
        and not opts.get("file")
    ):
        opts["number"] = True
    opts["age_bucket"] = True

    linenumber = opts.get("line_number") is not None
    if linenumber and (not opts.get("changeset")) and (not opts.get("number")):
        raise error.Abort(_("at least one of -n/-c is required for -l"))

    ui.pager("annotate")

    if rootfm.isplain():

        def makefunc(op, get, fmt):
            return lambda x: fmt(get(x))

    else:

        def makefunc(op, get, fmt):
            if op == "changeset":
                # maintain backward compatibility for json output
                return lambda x: get(x)[0]
            else:
                return get

    funcmap = [
        [op, makefunc(op, get, fmt), sep] for op, sep, get, fmt in opmap if opts.get(op)
    ]
    # no separator in front of first column
    funcmap[0][-1] = ""
    fields = " ".join(
        fieldnamemap.get(op, op) for op, sep, get, fmt in opmap if opts.get(op)
    )

    def bad(x, y):
        raise error.Abort("%s: %s" % (x, y))

    m = scmutil.match(ctx, pats, opts, badfn=bad)

    follow = not opts.get("no_follow")
    diffopts = patch.difffeatureopts(ui, opts, section="annotate", whitespace=True)

    for abs in ctx.walk(m):
        fctx = ctx[abs]
        rootfm.startitem()
        rootfm.context(ctx=ctx)
        rootfm.data(abspath=abs, path=m.rel(abs))
        if not opts.get("text") and fctx.isbinary():
            rootfm.plain(_("%s: binary file\n") % ((pats and m.rel(abs)) or abs))
            continue

        fm = rootfm.nested("lines")
        lines = list(
            fctx.annotate(
                follow=follow,
                linenumber=True,
                diffopts=diffopts,
            )
        )
        if not lines:
            fm.end()
            continue
        formats = []
        pieces = []

        for op, f, sep in funcmap:
            l = [f(n) for n, dummy in lines]
            if fm.isplain():
                sizes = [encoding.colwidth(color.OSC8_RE.sub("", x)) for x in l]
                ml = max(sizes)
                if op == "changeset":
                    # Left-align the changeset to allow suffixes:
                    # b0466945ccdf~: line1
                    # 46f2e833b4fb+: line2
                    # 46f2e833b4fb : line3
                    formats.append([sep + "%s" + " " * (ml - w) for w in sizes])
                else:
                    formats.append([sep + " " * (ml - w) + "%s" for w in sizes])
            else:
                formats.append(["%s" for x in l])
            pieces.append(l)

        agebuckets = [agebucket(x, add_xrepo_suffix=True) for x, dummy in lines]

        for f, p, l, a in zip(zip(*formats), zip(*pieces), lines, agebuckets):
            fm.startitem()
            fm.write(fields, "".join(f) + ": ", *p, label="blame.age." + a)
            fm.write("line", "%s", l[1].decode(errors="replace"))

        if not lines[-1][1].endswith(b"\n"):
            fm.plain("\n")
        fm.end()

    rootfm.end()


@command(
    "archive|ar|arc|arch|archi|archiv",
    [
        ("", "no-decode", None, _("do not pass files through decoders (DEPRECATED)")),
        ("p", "prefix", "", _("directory prefix for files in archive"), _("PREFIX")),
        ("r", "rev", "", _("revision to distribute"), _("REV")),
        ("t", "type", "", _("type of distribution to create"), _("TYPE")),
    ]
    + walkopts,
    _("[OPTION]... DEST"),
)
def archive(ui, repo, dest, **opts):
    """create an unversioned archive of a repository revision

    By default, the revision used is the parent of the working
    directory; use -r/--rev to specify a different revision.

    The archive type is automatically detected based on file
    extension (to override, use -t/--type).

    .. container:: verbose

      Examples:

      - create a zip file containing the 1.0 release::

          @prog@ archive -r 1.0 project-1.0.zip

      - create a tarball excluding .hg files::

          @prog@ archive project.tar.gz -X ".hg*"

    Valid types are:

    :``files``: a directory full of files (default)
    :``tar``:   tar archive, uncompressed
    :``tbz2``:  tar archive, compressed using bzip2
    :``tgz``:   tar archive, compressed using gzip
    :``uzip``:  zip archive, uncompressed
    :``zip``:   zip archive, compressed using deflate

    The exact name of the destination archive or directory is given
    using a format string; see :prog:`help export` for details.

    Each member added to an archive file has a directory prefix
    prepended. Use -p/--prefix to specify a format string for the
    prefix. The default is the basename of the archive, with suffixes
    removed.

    Returns 0 on success.
    """

    ctx = scmutil.revsingle(repo, opts.get("rev"))
    if not ctx:
        raise error.Abort(_("no working directory: please specify a revision"))
    node = ctx.node()
    dest = cmdutil.makefilename(repo, dest, node)
    if os.path.realpath(dest) == repo.root:
        raise error.Abort(_("repository root cannot be destination"))

    kind = opts.get("type") or archival.guesskind(dest) or "files"
    prefix = opts.get("prefix")

    if dest == "-":
        if kind == "files":
            raise error.Abort(_("cannot archive plain files to stdout"))
        dest = cmdutil.makefileobj(repo, dest)
        if not prefix:
            prefix = os.path.basename(repo.root) + "-%h"

    prefix = cmdutil.makefilename(repo, prefix, node)
    match = scmutil.match(ctx, [], opts)
    if repo.ui.configbool("scale", "largeworkingcopy") and match.always():
        raise error.Abort(
            _(
                "this repository has a very large working copy and "
                "requires an explicit set of files to be archived"
            )
        )
    archival.archive(repo, dest, ctx, kind, match, prefix)


@command(
    "backout",
    [
        ("", "merge", None, _("combine existing pending changes with backout changes")),
        ("", "no-commit", False, _("do not commit")),
        (
            "",
            "parent",
            "",
            _("parent to choose when backing out merge (DEPRECATED)"),
            _("REV"),
        ),
        ("r", "rev", "", _("revision to back out"), _("REV")),
        ("e", "edit", False, _("open editor to specify custom commit message")),
    ]
    + mergetoolopts
    + walkopts
    + commitopts
    + commitopts2,
    _("[OPTION]... [-r] REV"),
    legacyaliases=["backo", "backou"],
)
def backout(ui, repo, node=None, rev=None, **opts):
    """reverse the effects of an earlier commit

    Create an inverse commit of the specified commit. Backout is commonly
    used to undo the effects of a public commit.

    By default, :prog:`backout` creates a new commit on top of the
    current commit. Specify ``--no-commit`` to skip making a new
    commit, leaving the changes outstanding in your working copy.

    If merge conflicts are encountered during the backout, changes will be
    left in the working copy with conflict markers inserted. When this occurs,
    resolve the conflicts and then run :prog:`commit`.

    By default, :prog:`backout` will abort if pending changes are present in the
    working copy. Specify ``--merge`` to combine changes from the backout with
    your pending changes.

    .. container:: verbose

      Examples:

      - Reverse the effect of the parent of the working copy.
        This backout will be committed immediately::

          @prog@ backout -r .

      - Reverse the effect of previous bad commit 42e8ddebe::

          @prog@ backout -r 42e8ddebe

      - Reverse the effect of previous bad revision 42e8ddebe and
        leave changes uncommitted::

          @prog@ backout -r 42e8ddebe --no-commit
          @prog@ commit -m "Backout 42e8ddebe"

      By default, the new commit will have one parent,
      maintaining a linear history. With ``--merge``, the commit
      will instead have two parents: the old parent of the
      working copy and a new child of REV that simply undoes REV.

    See :prog:`help dates` for a list of formats valid for ``-d/--date``.

    See :prog:`help revert` for a way to restore files to the state
    of another revision.

    Returns 0 on success, 1 if nothing to backout or there are unresolved
    files.
    """
    wlock = lock = None
    try:
        wlock = repo.wlock()
        lock = repo.lock()
        return _dobackout(ui, repo, node, rev, **opts)
    finally:
        release(lock, wlock)


def _makebackoutmessage(repo, message, node):
    addmessage = "\n\nOriginal commit changeset: %s" % short(node)
    if not message:
        title = repo.changelog.changelogrevision(node).description.splitlines()[0]
        message = 'Back out "%s"' % title
    return message + addmessage


def _replayrenames(repo, node):
    """Ensure that any renames from node are replayed in reverse on the current
    working copy.
    """
    origctx = repo[node]
    wctx = repo[None]
    status = wctx.p1().status()

    # For each file in the commit that's being backed out..
    for origfile in origctx.visiblefiles():
        # ...if it wasn't a deletion...
        if origfile in origctx:
            origfilectx = origctx[origfile]
            origrenamed = origfilectx.renamed()
            # ...and it was a rename...
            if origrenamed:  # In the negative case this can be None or False
                precopypath = origrenamed[0]
                # ...then if the working copy has the pre-rename file...
                if precopypath in wctx:
                    newfilectx = wctx[precopypath]
                    newrenamed = newfilectx.renamed()
                    # ...and it's not already marked renamed...
                    # ...and the post-rename file is marked delete...
                    if newrenamed is None and origfile in status.removed:
                        # ...then reverse the rename.
                        repo.dirstate.copy(origfile, precopypath)


def _dobackout(ui, repo, node=None, rev=None, **opts):
    if opts.get("commit") and opts.get("no_commit"):
        raise error.Abort(_("cannot use --commit with --no-commit"))
    if opts.get("merge") and opts.get("no_commit"):
        raise error.Abort(_("cannot use --merge with --no-commit"))

    if rev and node:
        raise error.Abort(_("please specify just one revision"))

    if not rev:
        rev = node

    if not rev:
        raise error.Abort(_("please specify a revision to backout"))

    date = opts.get("date")
    if date:
        opts["date"] = util.parsedate(date)

    cmdutil.checkunfinished(repo)
    cmdutil.bailifchanged(repo)
    node = scmutil.revsingle(repo, rev).node()

    op1, op2 = repo.dirstate.parents()
    if not repo.changelog.isancestor(node, op1):
        raise error.Abort(_("cannot backout change that is not an ancestor"))

    p1, p2 = repo.changelog.parents(node)
    if p1 == nullid:
        raise error.Abort(_("cannot backout a change with no parents"))
    if p2 != nullid:
        if not opts.get("parent"):
            raise error.Abort(_("cannot backout a merge changeset"))
        p = repo.lookup(opts["parent"])
        if p not in (p1, p2):
            raise error.Abort(_("%s is not a parent of %s") % (short(p), short(node)))
        parent = p
    else:
        if opts.get("parent"):
            raise error.Abort(_("cannot use --parent on non-merge changeset"))
        parent = p1

    subtreeutil.check_commit_backoutable(repo, node)

    rctx = scmutil.revsingle(repo, hex(parent))
    if not opts.get("merge") and op1 != node:
        dsguard = dirstateguard.dirstateguard(repo, "backout")
        try:
            ui.setconfig("ui", "forcemerge", opts.get("tool", ""), "backout")
            stats = mergemod.merge(repo, parent, force=True, ancestor=node)
            repo.setparents(op1, op2)

            # Ensure reverse-renames are preserved during the backout. In theory
            # merge.merge() should handle this, but it's extremely complex, so
            # let's just double check it here.
            _replayrenames(repo, node)

            dsguard.close()
            hg.showstats(repo, stats)
            if stats[3]:
                repo.ui.status(
                    _("use '@prog@ resolve' to retry unresolved file merges\n")
                )
                return 1
        finally:
            ui.setconfig("ui", "forcemerge", "", "")
            lockmod.release(dsguard)
    else:
        hg.clean(repo, node, show_stats=False)
        cmdutil.revert(ui, repo, rctx, repo.dirstate.parents())
        # Ensure reverse-renames are preserved during the backout. In theory
        # cmdutil.revert() should handle this, but it's extremely complex, so
        # let's just double check it here.
        _replayrenames(repo, node)

    if opts.get("no_commit"):
        msg = _("changeset %s backed out, don't forget to commit.\n")
        ui.status(msg % short(node))
        return 0

    def commitfunc(ui, repo, message, match, opts):
        return repo.commit(
            _makebackoutmessage(repo, message, node),
            opts.get("user"),
            opts.get("date"),
            match,
            editor=cmdutil.getcommiteditor(editform="backout", **opts),
        )

    newnode = cmdutil.commit(ui, repo, commitfunc, [], opts)
    if not newnode:
        ui.status(_("nothing changed\n"))
        return 1
    cmdutil.commitstatus(repo, newnode)

    nice = short
    ui.status(
        _("changeset %s backs out changeset %s\n")
        % (nice(repo.changelog.tip()), nice(node))
    )
    if opts.get("merge") and op1 != node:
        hg.clean(repo, op1, show_stats=False)
        ui.status(_("merging with changeset %s\n") % nice(repo.changelog.tip()))
        with ui.configoverride({("ui", "forcemerge"): opts.get("tool", "")}, "backout"):
            return hg.merge(repo, hex(repo.changelog.tip()))
    return 0


@command(
    "bisect|bi",
    [
        ("r", "reset", False, _("reset bisect state")),
        ("g", "good", False, _("mark changeset good")),
        ("b", "bad", False, _("mark changeset bad")),
        ("s", "skip", False, _("skip testing changeset")),
        ("e", "extend", False, _("extend the bisect range")),
        ("c", "command", "", _("use command to check changeset state"), _("CMD")),
        ("U", "noupdate", False, _("do not update to target")),
        (
            "S",
            "nosparseskip",
            False,
            _("do not skip changesets with no changes in sparse profile"),
        ),
    ],
    _("[OPTION]... [-c CMD] [REV]"),
    legacyaliases=["bis", "bise", "bisec"],
)
def bisect(
    ui,
    repo,
    rev=None,
    extra=None,
    command=None,
    reset=None,
    good=None,
    bad=None,
    skip=None,
    extend=None,
    noupdate=None,
    nosparseskip=None,
):
    """binary search of commits

    Find the commit that introduced a problem. To use, mark the
    earliest commit you know exhibits the problem as bad, then mark
    the latest commit which is free from the problem as good. Bisect
    will update your working copy to a commit for testing (unless the
    ``-U/--noupdate`` option is specified). Once you have tested the
    commit, mark the working copy as good or bad, and bisect will
    either update to another candidate commit or announce that it has
    found the bad commit.

    When using a sparse profile, bisect skips commits that don't
    overlap with the sparse config unless the ``-S/--nosparseskip``
    is specified.

    As a shortcut, you can use the REV argument to mark a
    commit as good or bad without checking it out first.

    If you supply a command with ``-c/--command``, it will be used for
    automatic bisection. The environment variable @PROG@_NODE will
    contain the ID of the commit being tested. The exit status of the
    command will be used to mark commits as good or bad: status 0
    means good, 125 means to skip the commit, 127 (command not
    found) will abort the bisection, and any other non-zero exit
    status means the commit is bad.

    .. container:: verbose

      Some examples:

      - start a bisection with known bad commit 2589fca98, and good commit 3fc9965cd::

          @prog@ bisect --bad 2589fca98
          @prog@ bisect --good 3fc9965cd

      - advance the current bisection by marking current commit as good or
        bad::

          @prog@ bisect --good
          @prog@ bisect --bad

      - mark the current commit, or a known commit, to be skipped (e.g. if
        that commit is not usable because of another issue)::

          @prog@ bisect --skip
          @prog@ bisect --skip 530553bab

      - skip all commits that do not touch directories ``foo`` or ``bar``::

          @prog@ bisect --skip "!( file('path:foo') & file('path:bar') )"

      - forget the current bisection::

          @prog@ bisect --reset

      - use ``make && make tests`` to automatically find the first broken
        commit::

          @prog@ bisect --reset
          @prog@ bisect --bad 2589fca98
          @prog@ bisect --good 3fc9965cd
          @prog@ bisect --command "make && make tests"

      - see all commits whose states are already known in the current
        bisection::

          @prog@ log -r "bisect(pruned)"

      - see the commit currently being bisected (especially useful
        if running with ``-U/--noupdate``)::

          @prog@ log -r "bisect(current)"

      - see all commits that took part in the current bisection::

          @prog@ log -r "bisect(range)"

      - you can even get a nice graph::

          @prog@ log --graph -r "bisect(range)"

      See :prog:`help revisions.bisect` for more about the `bisect()` predicate.

    Returns 0 on success.
    """
    # backward compatibility
    if rev in "good bad reset init".split():
        ui.warn(
            _(
                "(use of '@prog@ bisect <cmd>' is deprecated.\n"
                "Use '@prog@ bisect --good ARGS', '@prog@ bisect --bad ARGS' etc. instead.)\n"
            )
        )
        cmd, rev, extra = rev, extra, None
        if cmd == "good":
            good = True
        elif cmd == "bad":
            bad = True
        else:
            reset = True
    elif extra:
        raise error.Abort(_("incompatible arguments"))

    incompatibles = {
        "--bad": bad,
        "--command": bool(command),
        "--extend": extend,
        "--good": good,
        "--reset": reset,
        "--skip": skip,
    }

    enabled = [x for x in incompatibles if incompatibles[x]]

    if len(enabled) > 1:
        raise error.Abort(_("%s and %s are incompatible") % tuple(sorted(enabled)[0:2]))

    if reset:
        hbisect.resetstate(repo)
        return

    state = hbisect.load_state(repo)

    # update state
    if good or bad or skip:
        _update_state(repo, state, rev, good, bad, skip)
        hbisect.save_state(repo, state)
        if not (state["good"] and state["bad"]):
            return

    def mayupdate(repo, node, show_stats=True):
        """common used update sequence"""
        if noupdate:
            return
        cmdutil.checkunfinished(repo)
        cmdutil.bailifchanged(repo)
        return hg.clean(repo, node, show_stats=show_stats)

    def showtestingnext(rev, node, changesets):
        # compute the approximate number of remaining tests
        tests, size = 0, 2
        while size <= changesets:
            tests, size = tests + 1, size * 2
        ui.write(
            _("Testing changeset %s (%d changesets remaining, ~%d tests)\n")
            % (short(node), changesets, tests)
        )

    def sparseskip(node, changesets, bgood, badnode, goodnode):
        """
        Skip testing next nodes if nothing was changed since last good or bad node
        in sparse profile.
        """

        nodestate = hbisect.checksparsebisectskip(repo, node, badnode, goodnode)
        while nodestate != "check":
            basenode = goodnode if nodestate == "good" else badnode
            skipsparsed = "Skipping changeset %s as there are no changes inside\n\
the sparse profile from the known %s changeset %s\n"
            ui.write(_(skipsparsed) % (short(node), nodestate, short(basenode)))

            state[nodestate].append(node)

            nodes, _untested, changesets, bgood, badnode, goodnode = hbisect.bisect(
                repo, state
            )
            node = nodes[0]

            # update state
            state["current"] = [node]
            hbisect.save_state(repo, state)

            if changesets == 0:
                return (node, changesets, bgood)
            nodestate = hbisect.checksparsebisectskip(repo, node, badnode, goodnode)

        return (node, changesets, bgood)

    displayer = cmdutil.show_changeset(ui, repo, {})

    if command:
        changesets = 1
        if noupdate:
            try:
                node = state["current"][0]
            except LookupError:
                raise error.Abort(
                    _("current bisect revision is unknown - start a new bisect to fix")
                )
        else:
            node, p2 = repo.dirstate.parents()
            if p2 != nullid:
                raise error.Abort(_("current bisect revision is a merge"))
        if rev:
            node = repo[scmutil.revsingle(repo, rev, node)].node()
        try:
            while changesets:
                # update state
                state["current"] = [node]
                hbisect.save_state(repo, state)
                varname = identity.default().cliname().upper()
                status = ui.system(
                    command,
                    environ={f"{varname}_NODE": hex(node)},
                    blockedtag="bisect_check",
                )
                if status == 125:
                    transition = "skip"
                elif status == 0:
                    transition = "good"
                # status < 0 means process was killed
                elif status == 127:
                    raise error.Abort(_("failed to execute %s") % command)
                elif status < 0:
                    raise error.Abort(_("%s killed") % command)
                else:
                    transition = "bad"
                state[transition].append(node)
                ctx = repo[node]
                ui.status(_("changeset %s: %s\n") % (ctx, transition))
                hbisect.checkstate(state)
                # bisect
                nodes, untested, changesets, bgood, badnode, goodnode = hbisect.bisect(
                    repo, state
                )
                node = nodes[0]

                if changesets != 0 and not nosparseskip:
                    node, changesets, bgood = sparseskip(
                        node, changesets, bgood, badnode, goodnode
                    )
                    if changesets == 0:
                        nodes = [node]
                        break

                showtestingnext(repo.changelog.rev(node), node, changesets)
                mayupdate(repo, node, show_stats=False)
        finally:
            state["current"] = [node]
            hbisect.save_state(repo, state)
        hbisect.printresult(ui, repo, state, displayer, nodes, untested, bgood)
        return

    hbisect.checkstate(state)

    # actually bisect
    nodes, untested, changesets, good, badnode, goodnode = hbisect.bisect(repo, state)

    # update to next check
    if extend:
        if not changesets:
            extendnode = hbisect.extendrange(repo, state, nodes, good)
            if extendnode is not None:
                extnode = extendnode.node()
                state["current"] = [extnode]
                hbisect.save_state(repo, state)
                ui.write(_("Extending search to changeset %s\n") % extendnode)

                if not nosparseskip:
                    node, changesets, good = sparseskip(
                        extnode, changesets, good, badnode, goodnode
                    )
                    if node != extnode:
                        if changesets == 0:
                            hbisect.printresult(
                                ui, repo, state, displayer, [node], [], good
                            )
                            return
                        showtestingnext(repo.changelog.rev(node), node, changesets)
                        extnode = node

                return mayupdate(repo, extnode)
        raise error.Abort(_("nothing to extend"))

    if changesets == 0:
        hbisect.printresult(ui, repo, state, displayer, nodes, untested, good)
    else:
        assert len(nodes) == 1  # only a single node can be tested next
        node = nodes[0]

        state["current"] = [node]
        hbisect.save_state(repo, state)

        if not nosparseskip:
            node, changesets, good = sparseskip(
                node, changesets, good, badnode, goodnode
            )
            if changesets == 0:
                hbisect.printresult(ui, repo, state, displayer, [node], [], good)
                return

        rev = repo.changelog.rev(node)
        showtestingnext(rev, node, changesets)

        return mayupdate(repo, node)


def _update_state(repo, state, rev, good, bad, skip):
    def get_nodes(rev):
        nodes = (
            [repo.lookup(i) for i in scmutil.revrange(repo, [rev])]
            if rev
            else [repo.lookup(".")]
        )
        return nodes

    def get_revs(rev):
        """lazy evaluate `rev` revset expression if it is large.

        This is only used for 'skip' status now.
        """
        revs = scmutil.revrange(repo, [rev or "."])
        fastlen = revs.fastlen()
        # revset check is slower than nodes check, so avoid revset expr for small revsets.
        # for example, user might just want to skip a single commit.
        if fastlen is not None and fastlen < 10:
            return list(repo.changelog.tonodes(revs))
        else:
            return [f"revset:{rev}"]

    if good:
        state["good"] += get_nodes(rev)
    elif bad:
        state["bad"] += get_nodes(rev)
    elif skip:
        state["skip"] += get_revs(rev)


@command(
    "bookmark|bo|book|bookmarks",
    [
        ("f", "force", False, _("force")),
        ("r", "rev", "", _("revision for bookmark action"), _("REV")),
        ("d", "delete", False, _("delete a given bookmark")),
        ("D", "strip", None, _("like --delete, but also strip changesets")),
        ("m", "rename", "", _("rename a given bookmark"), _("OLD")),
        ("i", "inactive", False, _("mark a bookmark inactive")),
    ]
    + formatteropts,
    _("[OPTION]... [NAME]..."),
    legacyaliases=["boo", "bookm", "bookma", "bookmar"],
)
def bookmark(ui, repo, *names, **opts):
    """create a new bookmark or list existing bookmarks

    Bookmarks are labels on changesets to help track lines of development.
    Bookmarks are unversioned and can be moved, renamed and deleted.
    Deleting or moving a bookmark has no effect on the associated changesets.

    Creating or updating to a bookmark causes it to be marked as 'active'.
    The active bookmark is indicated with a '*'.
    When a commit is made, the active bookmark will advance to the new commit.
    A plain :prog:`goto` will also advance an active bookmark, if possible.
    Updating away from a bookmark will cause it to be deactivated.

    Bookmarks can be pushed and pulled between repositories (see
    :prog:`help push` and :prog:`help pull`). If a shared bookmark has
    diverged, a new 'divergent bookmark' of the form 'name@path' will
    be created. Using :prog:`merge` will resolve the divergence.

    Specifying bookmark as '.' to -m or -d options is equivalent to specifying
    the active bookmark's name.

    .. container:: verbose

      Examples:

      - create an active bookmark for a new line of development::

          @prog@ book new-feature

      - create an inactive bookmark as a place marker::

          @prog@ book -i reviewed

      - create an inactive bookmark on another changeset::

          @prog@ book -r .^ tested

      - rename bookmark turkey to dinner::

          @prog@ book -m turkey dinner

      - move the '@' bookmark from another branch::

          @prog@ book -f @

    In Git repos, bookmarks correspond to branches. Remote Git branches can be listed using the ``--remote`` flag.

    .. container:: verbose

      Examples:

      - list remote branches::

          @prog@ bookmark --remote

      - list remote tags::

          @prog@ bookmark --remote tags

      - list all refs::

          @prog@ bookmark --remote 'refs/*'

      - list branches from specified path::

          @prog@ bookmark --remote --remote-path my-fork

    """
    force = opts.get(r"force")
    rev = opts.get(r"rev")
    delete = opts.get(r"delete")
    rename = opts.get(r"rename")
    inactive = opts.get(r"inactive")
    strip = opts.get("strip")

    if delete and rename:
        raise error.Abort(_("--delete and --rename are incompatible"))
    if delete and rev:
        raise error.Abort(_("--rev is incompatible with --delete"))
    if rename and rev:
        raise error.Abort(_("--rev is incompatible with --rename"))
    if not names and (delete or rev):
        raise error.Abort(_("bookmark name required"))
    if strip:
        # Check for incompatible options.
        for name in [
            "force",
            "rev",
            "rename",
            "inactive",
            "track",
            "untrack",
            "all",
            "remote",
        ]:
            if opts.get(name):
                raise error.Abort(
                    _("--strip cannot be used together with %s") % ("--%s" % name)
                )
            # book --strip is just an alias for hide -B.
            # (it may raise UnknownCommand)
            stripfunc = cmdutil.findcmd("hide", table)[1][0]
            return stripfunc(ui, repo, bookmark=names)

    if delete or rename or names or inactive:
        with repo.wlock(), repo.lock(), repo.transaction("bookmark") as tr:
            if delete:
                names = list(map(repo._bookmarks.expandname, set(names)))
                bookmarks.delete(repo, tr, names)
            elif rename:
                if not names:
                    raise error.Abort(_("new bookmark name required"))
                elif len(names) > 1:
                    raise error.Abort(_("only one new bookmark name allowed"))
                rename = repo._bookmarks.expandname(rename)
                bookmarks.rename(repo, tr, rename, names[0], force, inactive)
            elif names:
                bookmarks.addbookmarks(repo, tr, names, rev, force, inactive)
            elif inactive:
                if len(repo._bookmarks) == 0:
                    ui.status(_("no bookmarks set\n"))
                elif not repo._activebookmark:
                    ui.status(_("no active bookmark\n"))
                else:
                    bookmarks.deactivate(repo)
    else:  # show bookmarks
        bookmarks.printbookmarks(ui, repo, **opts)


@command(
    "branch|br|bra|bran|branc",
    [
        ("f", "force", None, _("do nothing (DEPRECATED)")),
        ("C", "clean", None, _("raise an exception (DEPRECATED)")),
        # Note: for compatibility for tweakdefaults users
        ("", "new", None, _("do nothing (DEPRECATED)")),
    ],
    _("[NAME]"),
)
def branch(ui, repo, label=None, **opts):
    """(deprecated. use '@prog@ bookmark' instead)

    This command does nothing meaningful and will be removed in the future.

    For now, it always prints "default" or raise an exception if NAME or -C is
    provided.
    """
    if ui.identity.cliname() != "hg":
        # we don't need compatibility layer and deprecation messages for Sapling identity
        ui.write(("unknown command 'branch'\n"))
        ui.write(
            _(
                'hint: perhaps you\'d like to use "@prog@ bookmark".\nMore info: '
                "https://sapling-scm.com/docs/overview/bookmarks\n"
            )
        )
        return 255
    if not util.istest():
        ui.deprecate("hg-branch", "branches are deprecated at Meta")
    hintutil.trigger("branch-command-deprecate")
    if not opts.get("clean") and not label:
        ui.write("default\n")
        return

    raise error.Abort(
        _("named branches are disabled in this repository"),
        hint=ui.config("ui", "disallowedbrancheshint", _("use bookmarks instead")),
    )


@command(
    "bundle|bu|bun|bund|bundl",
    [
        ("f", "force", None, _("run even when the destination is unrelated")),
        (
            "r",
            "rev",
            [],
            _("a changeset intended to be added to the destination"),
            _("REV"),
        ),
        (
            "",
            "base",
            [],
            _("a base changeset assumed to be available at the destination"),
            _("REV"),
        ),
        ("a", "all", None, _("bundle all changesets in the repository")),
        ("t", "type", "bzip2", _("bundle compression type to use"), _("TYPE")),
    ],
    _("[-f] [-t BUNDLESPEC] [-a] [-r REV]... [--base REV]... FILE [DEST]"),
)
def bundle(ui, repo, fname, dest=None, **opts):
    """create a bundle file

    Generate a bundle file containing data to be added to a repository.

    To create a bundle containing all changesets, use -a/--all
    (or --base null). Otherwise, @prog@ assumes the destination will have
    all the nodes you specify with --base parameters. Otherwise, hg
    will assume the repository has all the nodes in destination, or
    default-push/default if no destination is specified.

    You can change bundle format with the -t/--type option. See
    :prog:`help bundlespec` for documentation on this format. By default,
    the most appropriate format is used and compression defaults to
    bzip2.

    The bundle file can then be transferred using conventional means
    and applied to another repository with the unbundle or pull
    command. This is useful when direct push and pull are not
    available or when exporting an entire repository is undesirable.

    Applying bundles preserves all changeset contents including
    permissions, copy/rename information, and revision history.

    Returns 0 on success, 1 if no changes found.
    """
    revs = None
    if "rev" in opts:
        revstrings = opts["rev"]
        revs = scmutil.revrange(repo, revstrings)
        if revstrings and not revs:
            raise error.Abort(_("no commits to bundle"))

    bundletype = opts.get("type", "bzip2").lower()
    try:
        bcompression, cgversion, params = exchange.parsebundlespec(
            repo, bundletype, strict=False
        )
    except error.UnsupportedBundleSpecification as e:
        raise error.Abort(
            str(e),
            hint=_("see '@prog@ help bundlespec' for supported values for --type"),
        )

    if opts.get("all"):
        if dest:
            raise error.Abort(_("--all is incompatible with specifying a destination"))
        if opts.get("base"):
            ui.warn(_("ignoring --base because --all was specified\n"))
        base = ["null"]
    else:
        base = scmutil.revrange(repo, opts.get("base"))
    if cgversion not in changegroup.supportedoutgoingversions(repo):
        raise error.Abort(
            _("repository does not support bundle version %s") % cgversion
        )

    if base:
        if dest:
            raise error.Abort(_("--base is incompatible with specifying a destination"))
        common = [repo.lookup(rev) for rev in base]
        heads = revs and list(map(repo.lookup, revs)) or None
        outgoing = discovery.outgoing(repo, common, heads)
    else:
        dest = ui.expandpath(dest or "default-push", dest or "default")
        dest = hg.parseurl(dest)
        other = hg.peer(repo, opts, dest)
        heads = revs and list(map(repo.lookup, revs)) or revs
        outgoing = discovery.findcommonoutgoing(
            repo, other, onlyheads=heads, force=opts.get("force"), portable=True
        )

    if not outgoing.missing:
        scmutil.nochangesfound(ui, repo, not base and outgoing.excluded)
        return 1

    if cgversion == "01":  # bundle1
        if bcompression is None:
            bcompression = "UN"
        bversion = "HG10" + bcompression
        bcompression = None
    elif cgversion in ("02", "03"):
        bversion = "HG20"
    else:
        raise error.ProgrammingError(
            "bundle: unexpected changegroup version %s" % cgversion
        )

    # TODO compression options should be derived from bundlespec parsing.
    # This is a temporary hack to allow adjusting bundle compression
    # level without a) formalizing the bundlespec changes to declare it
    # b) introducing a command flag.
    compopts = {}
    complevel = ui.configint("experimental", "bundlecomplevel")
    if complevel is not None:
        compopts["level"] = complevel

    contentopts = {"cg.version": cgversion}
    if repo.ui.configbool("experimental", "bundle-phases"):
        contentopts["phases"] = True
    if git.isgitstore(repo):
        return git.bundle(repo, fname, outgoing.missing)
    bundle2.writenewbundle(
        ui,
        repo,
        "bundle",
        fname,
        bversion,
        outgoing,
        contentopts,
        compression=bcompression,
        compopts=compopts,
    )


@command(
    "cat",
    [
        ("o", "output", "", _("print output to file with formatted name"), _("FORMAT")),
        ("r", "rev", "", _("print the given revision"), _("REV")),
    ]
    + walkopts
    + formatteropts,
    inferrepo=True,
    cmdtype=readonly,
)
def cat(ui, repo, file1, *pats, **opts):
    ctx = scmutil.revsingle(repo, opts.get("rev"))
    m = scmutil.match(ctx, (file1,) + pats, opts)
    fntemplate = opts.pop("output", "")
    if cmdutil.isstdiofilename(fntemplate):
        fntemplate = ""

    if fntemplate:
        fm = formatter.nullformatter(ui, "cat")
    else:
        ui.pager("cat")
        fm = ui.formatter("cat", opts)
    with fm:
        return cmdutil.cat(ui, repo, ctx, m, fm, fntemplate, "", **opts)


@command(
    "clone",
    [
        (
            "U",
            "noupdate",
            None,
            _("the clone will include an empty working directory (only a repository)"),
        ),
        ("u", "updaterev", "", _("revision or branch to check out"), _("REV")),
        (
            "",
            "shallow",
            True,
            _("use remotefilelog (has no effect) (DEPRECATED)"),
        ),
        ("", "git", None, _("use git protocol (EXPERIMENTAL)")),
    ],
    norepo=True,
)
def clone(ui, source, dest=None, **opts):
    if opts.get("noupdate") and opts.get("updaterev"):
        raise error.Abort(_("cannot specify both --noupdate and --updaterev"))
    giturl = cloneuri.determine_git_uri(opts.get("git"), source)
    if giturl is not None:
        if opts.get("noupdate"):
            update = False
        else:
            update = opts.get("updaterev") or True
        r = git.clone(ui, giturl, dest, update)
    else:
        r = hg.clone(
            ui,
            opts,
            source,
            dest,
            update=opts.get("updaterev") or not opts.get("noupdate"),
        )

    return r


@command(
    "commit|ci",
    [
        (
            "A",
            "addremove",
            None,
            _("mark new/missing files as added/removed before committing"),
        ),
        ("", "amend", None, _("amend the parent of the working copy (DEPRECATED)")),
        ("e", "edit", None, _("invoke editor on commit messages")),
        ("i", "interactive", None, _("use interactive mode")),
        ("M", "reuse-message", "", _("reuse commit message from REV"), _("REV")),
        (
            "",
            "no-automv",
            None,
            _("disable automatic file move detection (DEPRECATED)"),
        ),
        ("", "no-move-detection", None, _("disable automatic file move detection")),
    ]
    + walkopts
    + commitopts
    + commitopts2
    + cmdutil.messagefieldopts,
    _("[OPTION]... [FILE]..."),
    inferrepo=True,
    legacyaliases=["com", "comm", "commi"],
)
def commit(ui, repo, *pats, **opts):
    """save all pending changes or specified files in a new commit

    Commit changes to the given files to your local repository.

    By default, all pending changes (in other words, those reported by
    :prog:`status`) are committed. If you want to commit only some of your
    changes, choose one of the following options:

    - Specify an exact list of files for which you want changes committed.

    - Use the ``-I`` or ``-X`` flags to match or exclude file names
      using a pattern or fileset. See :prog:`help patterns` and
      :prog:`help filesets` for details.

    - Specify the ``--interactive`` flag to open a UI to select
      individual files, hunks, or lines.

    To meld pending changes into the current commit instead of creating
    a new commit, see :prog:`amend`.

    If you are committing the result of a merge, such as when merge
    conflicts occur during :prog:`goto`, commit all pending changes.
    Do not specify files or use ``-I``, ``-X``, or ``-i``.

    Specify the ``-m`` flag to include a free-form commit message. Repeat
    ``-m`` to concatenate multiple messages separated by blank lines. If you
    do not specify ``-m``, @Product@ opens your configured editor where you
    can enter a message based on a pre-loaded commit template.

    Returns 0 on success, 1 if nothing changed.

    .. container:: verbose

      If your commit fails, you can find a backup of your commit message in
      ``.@prog@/last-message.txt``.

      Examples:

      - commit all files ending in .py::

          @prog@ commit --include "glob:**.py"

      - commit all non-binary files::

          @prog@ commit --exclude "set:binary()"
    """

    wlock = lock = None
    try:
        wlock = repo.wlock()
        lock = repo.lock()

        opts["automv"] = not (opts.get("no_automv") or opts.get("no_move_detection"))

        return _docommit(ui, repo, *pats, **opts)
    finally:
        release(lock, wlock)


def _docommit(ui, repo, *pats, **opts):
    subtree_copy_state = subtreeutil.read_subtree_copy_state(repo)
    if subtree_copy_state and (
        pats or opts.get("include") or opts.get("exclude") or opts.get("interactive")
    ):
        raise error.Abort(
            _("cannot partially commit pending subtree copy changes"),
            hint=_("run '@prog@ commit' to commit the subtree copy changes"),
        )

    if opts.get(r"interactive"):
        opts.pop(r"interactive")
        ret = cmdutil.dorecord(
            ui,
            repo,
            commit,
            None,
            False,
            cmdutil.recordfilter,
            *pats,
            inmemory=True,
            **opts,
        )
        # ret can be 0 (no changes to record) or the value returned by
        # commit(), 1 if nothing changed or None on success.
        return 1 if ret == 0 else ret

    cmdutil.checkunfinished(repo, op="amend" if opts.get("amend") else "commit")

    extra = {}
    predecessor = None
    if opts.get("amend"):
        old = repo["."]
        predecessor = old
        rewriteutil.precheck(repo, [old.rev()], "amend")

        # Currently histedit gets confused if an amend happens while histedit
        # is in progress. Since we have a checkunfinished command, we are
        # temporarily honoring it.
        #
        # Note: eventually this guard will be removed. Please do not expect
        # this behavior to remain.
        if not mutation.enabled(repo):
            cmdutil.checkunfinished(repo)

        node = cmdutil.amend(ui, repo, old, extra, pats, opts)
        if node == old.node():
            ui.status(_("nothing changed\n"))
            return 1
    else:
        mutinfo = None
        commitmutinfofunc = opts.get("_commitmutinfofunc")
        if commitmutinfofunc is not None:
            mutinfo = commitmutinfofunc(extra)

        def commitfunc(ui, repo, message, match, opts):
            ms = mergemod.mergestate.read(repo)
            subtree_merges = ms.subtree_merges
            if subtree_copy_state and subtree_merges:
                raise error.Abort(
                    _("unexpected simultaneous subtree copy and merge state"),
                    hint=_(
                        "use '@prog@ goto . --clean' to recover; "
                        "this will destroy uncommitted changes"
                    ),
                )
            summaryfooter = ""
            if subtree_merges:
                extra.update(subtreeutil.gen_merge_info(repo, subtree_merges))
                summaryfooter = subtreeutil.gen_merge_commit_msg(subtree_merges)
                parents = repo.working_parent_nodes()
                repo.setparents(parents[0])
            elif subtree_copy_state:
                extra.update(
                    subtreeutil.subtree_copy_state_to_extra(repo, subtree_copy_state)
                )
                summaryfooter = subtreeutil.gen_copy_commit_msg_from_subtree_copy_state(
                    repo, subtree_copy_state
                )
            # Block merge commits unless explicitly allowed. If repo state is not
            # maintained for commands like rebase, commit can end up creating a
            # merge commit, which is incorrect and has performance implications.
            if not ui.configbool("ui", "allowmerge", default=True):
                _p1, p2 = repo.dirstate.parents()
                if p2 != nullid:
                    raise error.Abort(
                        _(
                            "working copy has two parents (merge state) - "
                            "refusing to create a merge commit"
                        ),
                    )
            editform = cmdutil.mergeeditform(repo[None], "commit.normal")
            editor = cmdutil.getcommiteditor(
                editform=editform, summaryfooter=summaryfooter, **opts
            )
            return repo.commit(
                message,
                opts.get("user"),
                opts.get("date"),
                match,
                editor=editor,
                extra=extra,
                mutinfo=mutinfo,
            )

        node = cmdutil.commit(ui, repo, commitfunc, pats, opts)
        if subtree_copy_state:
            subtreeutil.clear_subtree_copy_state(repo)

        if not node:
            stat = cmdutil.postcommitstatus(repo, pats, opts)
            if stat[3]:
                ui.status(
                    _("nothing changed (%d missing files, see '@prog@ status')\n")
                    % len(stat[3])
                )
            else:
                ui.status(_("nothing changed\n"))
            return 1

    cmdutil.commitstatus(repo, node, opts=opts, predecessor=predecessor)


@command(
    "config|conf",
    [
        (
            "e",
            "edit",
            False,
            _("edit config, implying --user if no other flags set (DEPRECATED)"),
        ),
        (
            "u",
            "user",
            False,
            _("edit user config, opening in editor if no args given"),
        ),
        (
            "l",
            "local",
            False,
            _("edit repository config, opening in editor if no args given"),
        ),
        (
            "s",
            "system",
            False,
            _("edit system config, opening in editor if no args given"),
        ),
        (
            "g",
            "global",
            False,
            _("edit system config, opening in editor if no args given (DEPRECATED)"),
        ),
        (
            "d",
            "delete",
            False,
            _("delete specified config items"),
        ),
    ]
    + formatteropts,
    optionalrepo=True,
    cmdtype=readonly,
    legacyaliases=["showconfig", "debugconfig", "confi"],
)
def config(ui, repo, *values, **opts):
    if any(opts.get(flag) for flag in {"edit", "user", "local", "system", "global"}):
        editconfig(ui, repo, *values, **opts)
        return

    if opts.get("delete"):
        raise error.Abort(_("--delete requires one of --user, --local or --system"))

    ui.pager("config")
    fm = ui.formatter("config", opts)
    if values:
        sections = [v for v in values if "." not in v]
        items = [v for v in values if "." in v]
        if len(items) > 1 or items and sections:
            raise error.Abort(_("only one config item permitted"))
    matched = False
    for section, name, value in ui.walkconfig():
        source = ui.configsource(section, name)
        value = str(value)
        if fm.isplain():
            source = source or "none"
            value = value.replace("\n", "\\n")
        entryname = section + "." + name
        if values:
            for v in values:
                if v == section:
                    fm.startitem()
                    fm.condwrite(ui.debugflag, "source", "%s: ", source)
                    fm.write("name value", "%s=%s\n", entryname, value)
                    matched = True
                elif v == entryname:
                    fm.startitem()
                    fm.condwrite(ui.debugflag, "source", "%s: ", source)
                    fm.write("value", "%s\n", value)
                    fm.data(name=entryname)
                    matched = True
        elif not source.startswith("<builtin>") and not source.startswith("builtin:"):
            # Maintain backwards compatibility - don't write
            # builtin.rc values (formerly configitems.py) by default.
            fm.startitem()
            fm.condwrite(ui.debugflag, "source", "%s: ", source)
            fm.write("name value", "%s=%s\n", entryname, value)
            matched = True
    fm.end()
    if matched:
        return 0
    return 1


def editconfig(ui, repo, *values, **opts):
    target = [flag for flag in {"user", "local", "system", "global"} if opts.get(flag)]

    if not target and opts.get("edit"):
        target.append("user")

    if len(target) != 1:
        raise error.Abort(_("please specify exactly one config location"))

    target = target[0]
    if target == "global":
        target = "system"

    ident = ui.identity

    if target == "local":
        if not repo:
            raise error.Abort(_("can't use --local outside a repository"))
        paths = [repo.localvfs.join(ident.configrepofile())]
    elif target == "system":
        paths = ident.systemconfigpaths()
        if not paths:
            raise error.Abort(_("can't determine system config path"))
    elif target == "user":
        paths = ident.userconfigpaths()
    else:
        raise error.ProgrammingError("unexpected config target %r" % target)

    for targetpath in paths:
        if os.path.exists(targetpath):
            break
    else:
        targetpath = paths[0]
        os.makedirs(pathlib.Path(targetpath).parent.absolute(), exist_ok=True)
        fp = open(targetpath, "wb")
        fp.write(util.tonativeeol(_(uimod.samplehgrcs[target])).encode())
        fp.close()

    if not values:
        ui.status(_("opening %s for editing...\n") % targetpath)
        editor = ui.geteditor()
        ui.system(
            '%s "%s"' % (editor, targetpath),
            onerr=error.Abort,
            errprefix=_("edit failed"),
            blockedtag="config_edit",
        )
        return

    section_name = value = None
    invalid_arg = None
    to_edit = []
    is_delete = opts.get("delete")

    for arg in values:
        if section_name is None:
            if "=" in arg:
                if is_delete:
                    invalid_arg = arg
                    break

                section_name, value = arg.split("=", 1)
            else:
                section_name = arg
        else:
            value = arg

        # For whitespace separated pairs like "sl config --local foo.bar baz",
        # we skip to the next iteration to get config value "baz".
        if value is None and not is_delete:
            continue

        try:
            section, name = section_name.split(".", 1)
        except ValueError:
            # ex. not enough values to unpack
            break

        to_edit.append((section, name, value))
        section_name = value = None

    if invalid_arg is None:
        # If we are left with a live section_name, user failed to specify final
        # (whitespace separated) value.
        invalid_arg = section_name

    if invalid_arg is not None:
        if is_delete:
            raise error.Abort(
                _("invalid config deletion: %r") % invalid_arg,
                hint=("try section.name"),
            )
        else:
            raise error.Abort(
                _("invalid config edit: %r") % invalid_arg,
                hint=("try section.name=value"),
            )

    for section, name, value in to_edit:
        if value is None:
            ui.note(_("deleting %s.%s from %s\n") % (section, name, targetpath))
        else:
            ui.note(_("setting %s.%s=%s in %s\n") % (section, name, value, targetpath))
        rcutil.editconfig(ui, targetpath, section, name, value)

    ui.status(_("updated config in %s\n") % targetpath)


@command("continue|cont")
def continuecmd(ui, repo):
    """resume operation after resolving conflicts"""
    for name, cmd in cmdutil.afterresolvedstates:
        if repo.localvfs.exists(name):
            args = shlex.split(_(cmd))
            if not ui.interactive():
                args.append("--noninteractive")
            return bindings.commands.run(args)
    else:
        ms = mergemod.mergestate.read(repo)
        cmdutil.abort_on_unresolved_conflicts(ms)

        if len(repo.working_parent_nodes()) > 1:
            # 'merge' or 'subtree merge'
            if ui.interactive():
                cliname = ui.identity.cliname()
                args = [cliname, "commit"]
                return bindings.commands.run(args)
            else:
                raise error.Abort(
                    _("cannot continue with '@prog@ commit' in non-interactive mode"),
                    hint=_(
                        "use '@prog@ commit' to commit or '@prog@ status' for more info"
                    ),
                )

        if ms.files():
            # no command support --continue, just delete the merge state.
            ui.status(_("(exiting merge state)\n"))
            ms.reset()
        else:
            raise error.Abort(_("nothing to continue"))


@command(
    "copy|cp",
    [
        ("", "mark", None, _("mark as a copy without actual copying")),
        ("", "amend", None, _("amend the current commit to mark a copy")),
        ("A", "after", None, _("alias to --mark (DEPRECATED)")),
        ("f", "force", None, _("forcibly copy over an existing managed file")),
    ]
    + walkopts
    + dryrunopts,
    _("[OPTION]... [SOURCE]... DEST"),
    legacyaliases=["cop"],
)
def copy(ui, repo, *pats, **opts):
    """mark files as copied for the next commit

    Mark dest as having copies of source files. If dest is a
    directory, copies are put in that directory. If dest is a file,
    the source must be a single file.

    By default, this command copies the contents of files as they
    exist in the working directory. If invoked with ``--mark``, the
    operation is recorded, but no copying is performed.

    This command takes effect with the next commit. To undo a copy
    before that, see :prog:`revert`.

    Returns 0 on success, 1 if errors are encountered.
    """
    with repo.wlock():
        return cmdutil.copy(ui, repo, pats, opts)


@command(
    "uncopy",
    walkopts + dryrunopts,
    _("[OPTION]... [SOURCES]..."),
)
def uncopy(ui, repo, *pats, **opts):
    """mark files as not copied for the next commit

    Unmark sources previously marked by :prog:`copy` so they are no longer copies.

    See :prog:`help patterns` and :prog:`help filesets` for more information
    on specifying file patterns.

    This command takes effect with the next commit.

    Returns 0 on success, 1 if nothing changed.
    """
    with repo.wlock():
        matcher = scmutil.match(repo[None], pats, opts)
        return cmdutil.uncopy(ui, repo, matcher, opts)


@command("debugcommands", [], _("[COMMAND]"), norepo=True)
def debugcommands(ui, cmd="", *args):
    """list all available commands and options"""
    for cmd, vals in sorted(table.items()):
        cmd = cmd.split("|")[0].strip("^")
        opts = ", ".join([i[1] for i in vals[1]])
        ui.write("%s: %s\n" % (cmd, opts))


@command(
    "debugcomplete",
    [("o", "options", None, _("show the command options"))],
    _("[-o] CMD"),
    norepo=True,
)
def debugcomplete(ui, cmd="", **opts):
    """returns the completion list associated with the given command"""

    if opts.get(r"options"):
        options = []
        otables = [globalopts]
        if cmd:
            aliases, entry = cmdutil.findcmd(cmd, table)
            otables.append(entry[1])
        for t in otables:
            for o in t:
                if "(DEPRECATED)" in o[3]:
                    continue
                if o[0]:
                    options.append("-%s" % o[0])
                options.append("--%s" % o[1])
        ui.write("%s\n" % "\n".join(options))
        return

    cmdlist = []
    includedebug = "debug" in cmd
    for name, entry in table.items():
        if not includedebug and "debug" in name:
            continue
        aliases = name.split("|")
        cmddesc = None  # ex. "update" or "update checkout" with -v
        for alias in aliases:
            if alias.startswith(cmd):
                if cmddesc is None:
                    cmddesc = alias
                    break
        if cmddesc is not None:
            if ui.verbose:
                # Also show aliases, but not prefixes.
                aliases = sorted(aliases)
                for i, alias in enumerate(aliases):
                    if alias in cmddesc or any(
                        a.startswith(alias) for a in aliases[i + 1 : i + 2]
                    ):
                        # alias is a prefix, skip.
                        continue
                    cmddesc += " %s" % alias
            cmdlist.append(cmddesc)
    ui.write("%s\n" % "\n".join(sorted(cmdlist)))


@command(
    "diff|d",
    [
        ("r", "rev", [], _("revision"), _("REV")),
        ("c", "change", "", _("change made by revision"), _("REV")),
    ]
    + diffopts
    + diffopts2
    + walkopts
    + diffgraftopts,
    _("[OPTION]... ([-c REV] | [-r REV1 [-r REV2]]) [FILE]..."),
    inferrepo=True,
    cmdtype=readonly,
    legacyaliases=["di", "dif"],
)
def diff(ui, repo, *pats, **opts):
    """show differences between commits

    Show the differences between two commits. If only one commit is specified,
    show the differences between the specified commit and your working copy.
    If no commits are specified, show your pending changes.

    Specify ``-c`` to see the changes in the specified commit relative to its
    parent.

    By default, this command skips binary files. To override this behavior,
    specify ``-a`` to include binary files in the diff.

    By default, diffs are shown using the unified diff format. Specify ``-g``
    to generate diffs in the git extended diff format. For more information,
    see :prog:`help diffs`.

    To compare directories, use :prog:`subtree diff`.

    .. note::

       :prog:`diff` might generate unexpected results during merges because it
       defaults to comparing against your working copy's first parent commit
       if no commits are specified.

    .. container:: verbose

      Examples:

      - compare a file in the current working directory to its parent::

          @prog@ diff foo.c

      - compare two historical versions of a directory, with rename info::

          @prog@ diff --git -r 5be761874:431ec8e07 lib/

      - get change stats relative to the last change on some date::

          @prog@ diff --stat -r "date('may 2')"

      - diff all newly-added files that contain a keyword::

          @prog@ diff "set:added() and grep(GNU)"

      - compare a revision and its parents::

          @prog@ diff -c 340f3fef5              # compare against first parent
          @prog@ diff -r 340f3fef5^:340f3fef5   # same using revset syntax
          @prog@ diff -r 340f3fef5^2:340f3fef5  # compare against the second parent

    Returns 0 on success.
    """
    if opts.get("from_path") or opts.get("to_path"):
        raise error.Abort(
            _("'--from-path' and '--to-path' are deprecated for '@prog@ diff' command"),
            hint=_("use '@prog@ subtree diff' to diff between directories"),
        )

    do_diff(ui, repo, *pats, **opts)


def do_diff(ui, repo, *pats, **opts):
    revs = opts.get("rev")
    change = opts.get("change")
    stat = opts.get("stat")
    reverse = opts.get("reverse")
    onlyfilesinrevs = opts.get("only_files_in_revs")

    if revs and change:
        msg = _("cannot specify --rev and --change at the same time")
        raise error.Abort(msg)
    elif change:
        ctx2 = scmutil.revsingle(repo, change, None)
        ctx1 = ctx2.p1()
    else:
        ctx1, ctx2 = (repo[node] for node in scmutil.revpair(repo, revs))

    if reverse:
        ctx1, ctx2 = ctx2, ctx1
        opts["from_path"], opts["to_path"] = opts.get("to_path"), opts.get("from_path")

    from_paths = scmutil.rootrelpaths(ctx1, opts.get("from_path"))
    to_paths = scmutil.rootrelpaths(ctx1, opts.get("to_path"))
    subtreeutil.validate_path_exist(ui, ctx1, from_paths, abort_on_missing=True)
    subtreeutil.validate_path_exist(ui, ctx2, to_paths, abort_on_missing=True)
    cmdutil.registerdiffgrafts(from_paths, to_paths, ctx1)

    if onlyfilesinrevs:
        files1 = set(ctx1.files())
        m1 = ctx1.manifest()
        if m1.hasgrafts():
            files1 = set(f for f in files1 for f in m1.graftedpaths(f))
        files2 = set(ctx2.files())
        pats = pats + tuple(repo.wvfs.join(f) for f in files1 | files2)

    diffopts = patch.diffallopts(ui, opts)
    m = scmutil.match(ctx2, pats, opts)
    ui.pager("diff")
    cmdutil.diffordiffstat(
        ui, repo, diffopts, ctx1, ctx2, m, stat=stat, root=opts.get("root")
    )


@command(
    "export|e|ex|exp|expo|expor",
    [
        ("o", "output", "", _("print output to file with formatted name"), _("FORMAT")),
        ("", "switch-parent", None, _("diff against the second parent")),
        ("r", "rev", [], _("revisions to export"), _("REV")),
        ("", "pattern", [], _("file patterns"), _("PATTERN")),
    ]
    + diffopts
    + walkopts,
    _("[OPTION]... [-o OUTFILESPEC] [-r] [REV]..."),
    cmdtype=readonly,
)
def export(ui, repo, *changesets, **opts):
    """dump the header and diffs for one or more changesets

    Print the changeset header and diffs for one or more revisions.
    If no revision is given, the parent of the working directory is used.

    The information shown in the changeset header is: author, date,
    branch name (if non-default), changeset hash, parent(s) and commit
    comment.

    .. note::

       :prog:`export` may generate unexpected diff output for merge
       changesets, as it will compare the merge changeset against its
       first parent only.

    Output may be to a file, in which case the name of the file is
    given using a format string. The formatting rules are as follows:

    :``%%``: literal "%" character
    :``%H``: changeset hash (40 hexadecimal digits)
    :``%N``: number of patches being generated
    :``%R``: changeset revision number
    :``%b``: basename of the exporting repository
    :``%h``: short-form changeset hash (12 hexadecimal digits)
    :``%m``: first line of the commit message (only alphanumeric characters)
    :``%n``: zero-padded sequence number, starting at 1
    :``%r``: zero-padded changeset revision number

    Without the -a/--text option, export will avoid generating diffs
    of files it detects as binary. With -a, export will generate a
    diff anyway, probably with undesirable results.

    Use the -g/--git option to generate diffs in the git extended diff
    format. See :prog:`help diffs` for more information.

    With the --switch-parent option, the diff will be against the
    second parent. It can be useful to review a merge.

    .. container:: verbose

      Examples:

      - use export and import to transplant a bugfix to the current
        branch::

          @prog@ export -r 9353 | @prog@ import -

      - export all the changesets between two revisions to a file with
        rename information::

          @prog@ export --git -r 123:150 > changes.txt

      - split outgoing changes into a series of patches with
        descriptive names::

          @prog@ export -r "outgoing()" -o "%n-%m.patch"

    Returns 0 on success.
    """
    changesets += tuple(opts.get("rev", []))
    m = scmutil.match(repo[None], opts.get("pattern", []), opts)
    if not changesets:
        changesets = ["."]
    revs = scmutil.revrange(repo, changesets)
    if not revs:
        raise error.Abort(_("export requires at least one changeset"))
    if len(revs) > 1:
        ui.note(_("exporting patches:\n"))
    else:
        ui.note(_("exporting patch:\n"))
    ui.pager("export")
    cmdutil.export(
        repo,
        revs,
        fntemplate=opts.get("output"),
        switch_parent=opts.get("switch_parent"),
        opts=patch.diffallopts(ui, opts),
        match=m,
    )


@command(
    "files|fi|fil|file",
    [
        ("r", "rev", "", _("search the repository as it is in REV"), _("REV")),
        ("0", "print0", None, _("end filenames with NUL, for use with xargs")),
    ]
    + walkopts
    + formatteropts,
    _("[OPTION]... [FILE]..."),
    cmdtype=readonly,
)
def files(ui, repo, *pats, **opts):
    """list tracked files

    Print files under @Product@ control in the working directory or
    specified revision for given files (excluding removed files).
    Files can be specified as filenames or filesets.

    If no files are given to match, this command prints the names
    of all files under @Product@ control.

    .. note::

       Running without file arguments can be slow in large repositories.
       Always provide a path or pattern to limit the output.

    .. container:: verbose

      Examples:

      - list all files under the current directory::

          @prog@ files .

      - shows sizes and flags for current revision::

          @prog@ files -vr .

      - list all files named README::

          @prog@ files -I "**/README"

      - list all binary files::

          @prog@ files "set:binary()"

      - find files containing a regular expression::

          @prog@ files "set:grep('bob')"

      - search tracked file contents with xargs and grep::

          @prog@ files -0 | xargs -0 grep foo

    See :prog:`help patterns` and :prog:`help filesets` for more information
    on specifying file patterns.

    Returns 0 if a match is found, 1 otherwise.

    """

    ctx = scmutil.revsingle(repo, opts.get("rev"), None)

    end = "\n"
    if opts.get("print0"):
        end = "\0"
    fmt = "%s" + end

    m = scmutil.match(ctx, pats, opts)
    shouldsparsematch = sparseutil.shouldsparsematch(repo)
    if isinstance(ctx, context.workingctx) and shouldsparsematch:
        m = matchmod.intersectmatchers(m, repo.sparsematch())

    ui.pager("files")
    with ui.formatter("files", opts) as fm:
        return cmdutil.files(ui, ctx, m, fm, fmt)


@command(
    "forget",
    walkopts,
    _("[OPTION]... FILE..."),
    inferrepo=True,
    legacyaliases=["for", "forg", "forge"],
)
def forget(ui, repo, *pats, **opts):
    """stop tracking the specified files

    Mark the specified files so they will no longer be tracked
    after the next commit.

    Forget does not delete the files from the working copy. To delete
    the file from the working copy, see :prog:`remove`.

    Forget does not remove files from the repository history. The files
    will only be removed in the next commit and its descendants.

    To undo a forget before the next commit, see :prog:`add`.

    .. container:: verbose

      Examples:

      - forget newly-added binary files::

          @prog@ forget "set:added() and binary()"

      - forget files that would be excluded by .gitignore::

          @prog@ forget "set:gitignore()"

    Returns 0 on success.
    """

    if not pats:
        raise error.Abort(_("no files specified"))

    m = scmutil.match(repo[None], pats, opts)
    rejected = cmdutil.forget(ui, repo, m, prefix="", explicitonly=False)[0]
    return rejected and 1 or 0


@command(
    "graft",
    [
        ("r", "rev", [], _("revisions to graft"), _("REV")),
        ("c", "continue", False, _("resume interrupted graft")),
        ("", "abort", False, _("abort an interrupted graft")),
        ("e", "edit", False, _("invoke editor on commit messages")),
        ("", "log", None, _("append graft info to log message")),
        ("f", "force", False, _("force graft")),
        ("D", "currentdate", False, _("record the current date as commit date")),
        (
            "U",
            "currentuser",
            False,
            _("record the current user as committer"),
        ),
    ]
    + commitopts
    + commitopts2
    + cmdutil.messagefieldopts
    + mergetoolopts
    + dryrunopts,
    _("[OPTION]... REV..."),
    legacyaliases=["gra", "graf"],
)
def graft(ui, repo, *revs, **opts):
    """copy commits from a different location

    Use @Product@'s merge logic to copy individual commits from other
    locations without making merge commits. This is sometimes known as
    'backporting' or 'cherry-picking'. By default, graft will also
    copy user and description from the source commits.

    .. container:: verbose

      If you want to keep the date of the source commits, you can add below config to your
      configuration file::

        [tweakdefaults]
        graftkeepdate = True

      Source commits will be skipped if they are ancestors of the
      current commit, have already been grafted, or are merges.

      If ``--log`` is specified, commit messages will have a comment appended
      of the form::

        (grafted from COMMITHASH)

      If ``--force`` is specified, commits will be grafted even if they
      are already ancestors of, or have been grafted to, the destination.
      This is useful when the commits have since been backed out.

      If a graft results in conflicts, the graft process is interrupted
      so that the current merge can be manually resolved. Once all
      conflicts are resolved, the graft process can be continued with
      the ``-c/--continue`` option.

      .. note::

         The ``-c/--continue`` operation does not remember options from
         the original invocation, except for ``--force``.

      Examples:

      - copy a single change to the stable branch and edit its description::

          @prog@ goto stable
          @prog@ graft --edit ba7e89595

      - graft a range of changesets with one exception, updating dates::

          @prog@ graft -D "0e13e529c::224010e02 and not 85c0535a4"

      - continue a graft after resolving conflicts::

          @prog@ graft -c

      - abort an interrupted graft::

          @prog@ graft --abort

      - show the source of a grafted changeset::

          @prog@ log --debug -r .

      See :prog:`help revisions` for more about specifying revisions.

    Returns 0 on success.
    """
    with repo.wlock():
        return _dograft(ui, repo, *revs, **opts)


def _dograft(ui, to_repo, *revs, from_repo=None, **opts):
    """copy commits from a different location

    * from_repo: The source repo. This may be an external Git repo (e.g., in subtree graft).
    * to_repo: The destination repo (typically the current working repo).
    """
    from_repo = from_repo or to_repo
    if revs and opts.get("rev"):
        ui.warn(
            _(
                "warning: inconsistent use of --rev might give unexpected "
                "revision ordering!\n"
            )
        )

    revs = list(revs)
    revs.extend(opts.get("rev"))

    if not opts.get("user") and opts.get("currentuser"):
        opts["user"] = ui.username()
    if not opts.get("date") and opts.get("currentdate"):
        opts["date"] = "%d %d" % util.makedate()

    cont = False
    if opts.get("continue") or opts.get("abort"):
        if revs and opts.get("continue"):
            raise error.Abort(_("can't specify --continue and revisions"))
        if revs and opts.get("abort"):
            raise error.Abort(_("can't specify --abort and revisions"))
        if opts.get("from_path") or opts.get("to_path"):
            raise error.Abort(
                _("--from-path/--to-path cannot be used with --continue or --abort")
            )

        if not to_repo.localvfs.exists("graftstate"):
            cmdutil.wrongtooltocontinue(to_repo, _("graft"))

        if opts.get("abort"):
            to_repo.localvfs.tryunlink("graftstate")
            return update(ui, to_repo, node=".", clean=True)

        cont = True

        ms = mergemod.mergestate.read(to_repo)
        from_repo = ms.from_repo()
        # read in unfinished revisions
        nodes = to_repo.localvfs.readutf8("graftstate").splitlines()
        revs = [from_repo[node].rev() for node in nodes]
        from_paths = [m["from_path"] for m in ms.subtree_merges]
        to_paths = [m["to_path"] for m in ms.subtree_merges]
        is_crossrepo = not to_repo.is_same_repo(from_repo)
    else:
        cmdutil.checkunfinished(to_repo)
        cmdutil.bailifchanged(to_repo)
        if not revs:
            raise error.Abort(_("no revisions specified"))
        revs = scmutil.revrange(from_repo, revs)

        is_crossrepo = not to_repo.is_same_repo(from_repo)
        if is_crossrepo:
            # In cross-repo grafts, from_paths are expected to be root-relative already
            from_paths = opts.get("from_path", [])
        else:
            from_paths = scmutil.rootrelpaths(from_repo["."], opts.get("from_path", []))
        to_paths = scmutil.rootrelpaths(to_repo["."], opts.get("to_path", []))

    skipped = set()
    # check for merges
    for rev in from_repo.revs("%ld and merge()", revs):
        ui.warn(_("skipping ungraftable merge revision %d\n") % rev)
        skipped.add(rev)
    # check subtree copy, import and merge commit
    for rev in revs:
        if rev in skipped:
            continue
        if not subtreeutil.is_commit_graftable(from_repo, rev):
            skipped.add(rev)
    revs = [r for r in revs if r not in skipped]
    if not revs:
        raise error.Abort(_("empty revision set was specified"))

    # When continuing an in-progress graft (--continue), or running in forced mode (--force),
    # or using --from-path (e.g., for subtree grafts), skip the ancestor check.
    #
    # Normally, we skip grafting any revisions that are already ancestors of the destination
    # commit. But in --continue or --force mode, the user has explicitly asked to apply
    # everything, including commits that may have already been in history.
    if not (cont or opts.get("force") or opts.get("from_path")):
        # check for ancestors of dest branch
        crev = to_repo["."].rev()
        ancestors = to_repo.changelog.ancestors([crev], inclusive=True)
        # XXX make this lazy in the future
        # don't mutate while iterating, create a copy
        for rev in list(revs):
            if rev in ancestors:
                ui.warn(_("skipping ancestor revision %s\n") % (to_repo[rev]))
                # XXX remove on list is slow
                revs.remove(rev)

        if not revs:
            return -1
    to_ctx = to_repo["."]
    for pos, ctx in enumerate(from_repo.set("%ld", revs)):
        # XXX: only check acl for non-crossrepo case
        if not is_crossrepo:
            files = ctx.files()
            for from_path, to_path in zip(from_paths, to_paths):
                from_files = [
                    f for f in files if f.startswith(from_path + "/") or f == from_path
                ]
                pathaclutil.validate_files_acl(
                    to_repo, from_files, to_path, to_ctx, op_name="graft"
                )

        desc = '%s "%s"' % (ctx, ctx.description().split("\n", 1)[0])
        names = from_repo.nodebookmarks(ctx.node())
        if names:
            desc += " (%s)" % " ".join(names)
        ui.status(_("grafting %s\n") % desc)
        if opts.get("dry_run"):
            continue

        source = ctx.extra().get("source")
        extra = {}
        # TODO: add cross-repo graft source to subtree metadata
        if not is_crossrepo:
            if source:
                extra["source"] = source
                extra["intermediate-source"] = ctx.hex()
            else:
                extra["source"] = ctx.hex()
        user = ctx.user()
        if opts.get("user"):
            user = opts["user"]
        date = ctx.date()
        if opts.get("date"):
            date = opts["date"]

        # Apply --from-path/--to-path mappings to manifest being grafted, and its
        # parent manifest.
        cmdutil.registerdiffgrafts(
            from_paths, to_paths, ctx, ctx.p1(), is_crossrepo=is_crossrepo
        )

        # we don't merge the first commit when continuing
        if not cont:
            # perform the graft merge with p1(rev) as 'ancestor'
            with to_repo.ui.configoverride(
                {("ui", "forcemerge"): opts.get("tool", "")}, "graft"
            ):
                stats = mergemod.graft(
                    to_repo,
                    ctx,
                    ctx.p1(),
                    ["local", "graft"],
                    from_repo=from_repo,
                )
            # report any conflicts
            if stats and stats[3] > 0:
                # write out state for --continue
                nodelines = [from_repo[rev].hex() + "\n" for rev in revs[pos:]]
                to_repo.localvfs.writeutf8("graftstate", "".join(nodelines))
                extra = ""
                if opts.get("user"):
                    extra += " --user %s" % util.shellquote(opts["user"])
                if opts.get("date"):
                    extra += " --date %s" % util.shellquote(opts["date"])
                if opts.get("log"):
                    extra += " --log"
                hint = _("use '@prog@ resolve' and '@prog@ graft --continue%s'") % extra
                raise error.Abort(_("unresolved conflicts, can't continue"), hint=hint)
        else:
            cont = False

        # commit
        editor = cmdutil.getcommiteditor(editform="graft", **opts)
        message, is_from_user = _makegraftmessage(
            to_repo, ctx, opts, from_paths, to_paths, from_repo
        )
        if not is_from_user and to_repo[None].dirty():
            message = rewriteutil.copycommitmessage(to_repo, message, "graft", ctx)
        node = to_repo.commit(
            text=message, user=user, date=date, extra=extra, editor=editor
        )
        if node is None:
            # users might provide wrong from paths, this validate function will
            # provide more info for users to understand why no changes to commit
            subtreeutil.validate_path_exist(ui, ctx, from_paths)
            ui.warn(_("note: graft of %s created no changes to commit\n") % (ctx))

    # remove state when we complete successfully
    if not opts.get("dry_run"):
        to_repo.localvfs.unlinkpath("graftstate", ignoremissing=True)
        mergemod.mergestate.clean(to_repo)

    return 0


def _makegraftmessage(to_repo, ctx, opts, from_paths, to_paths, from_repo):
    opts = dict(opts)

    if not opts.get("logfile") and not opts.get("message"):
        opts["message"] = ctx.description()

    description = cmdutil.logmessage(to_repo, opts)
    is_from_user = description != ctx.description()

    is_crossrepo = not to_repo.is_same_repo(from_repo)
    message = []
    if from_paths:
        # For xdir grafts, include "grafted from" breadcrumb by default.
        if opts.get("log") is not False:
            if is_crossrepo:
                from_repo_url = from_repo.ui.config("paths", "default")
                message.append("Grafted %s from %s" % (ctx.hex(), from_repo_url))
            else:
                message.append("Grafted %s" % ctx.hex())
            for f, t in zip(from_paths, to_paths):
                f = subtreeutil.normalize_cross_repo_path(f)
                message.append("- Grafted %s to %s" % (f, t))

            # don't update the user provided title
            if not is_from_user:
                try:
                    title, rest = description.split("\n", 1)
                    description = f'Graft "{title}"\n{rest}'
                except ValueError:
                    description = f'Graft "{description}"'
    else:
        if opts.get("log"):
            message.append("(grafted from %s)" % ctx.hex())
    message = "\n".join(message)
    return cmdutil.add_summary_footer(ctx.repo().ui, description, message), is_from_user


@command(
    "heads|hea|head",
    [
        (
            "r",
            "rev",
            "",
            _("show only heads which are descendants of STARTREV"),
            _("STARTREV"),
        ),
        ("t", "topo", False, _("show topological heads only (DEPRECATED)")),
        ("a", "active", False, _("show active branchheads only (DEPRECATED)")),
        ("c", "closed", False, _("show normal and closed branch heads (DEPRECATED)")),
    ]
    + templateopts,
    _("[OPTION]... [REV]..."),
    cmdtype=readonly,
)
def heads(ui, repo, *branchrevs, **opts):
    """show heads

    With no arguments, show all heads in the repository. Heads are
    changesets that have no descendants. They are where development
    generally takes place and are the usual targets for update and
    merge operations.

    If STARTREV is specified, only those heads that are descendants of
    STARTREV will be displayed.

    Returns 0 if matching heads are found, 1 if not.
    """
    if not util.istest():
        ui.deprecate(
            _("@prog@-heads"),
            _("heads is deprecated - use `@prog@ log -r 'head()'` instead"),
        )

    start = None
    if "rev" in opts:
        start = scmutil.revsingle(repo, opts["rev"], None).node()

    heads = [repo[h] for h in repo.heads(start)]

    if not heads:
        return 1

    ui.pager("heads")
    heads = sorted(heads, key=lambda x: -(x.rev()))
    displayer = cmdutil.show_changeset(ui, repo, opts)
    for ctx in heads:
        displayer.show(ctx)
    displayer.close()


@command(
    "help",
    [
        ("e", "extension", None, _("show help for extensions")),
        ("c", "command", None, _("show help for commands")),
        ("k", "keyword", None, _("show topics matching keyword")),
        ("s", "system", [], _("show help for specific platform(s)")),
    ],
    _("[-ecks] [TOPIC]"),
    norepo=True,
    cmdtype=readonly,
    legacyaliases=["hel"],
)
def help_(ui, *names, **opts):
    """show help for a given topic or a help overview

    With no arguments, print a list of commands with short help messages.

    Given a topic, extension, or command name, print help for that
    topic.

    Returns 0 if successful.
    """
    # The "help" command bypasses the usual "dispatch" logic including the hint
    # handling. To show hints (ex. ".. hint:: hint_name" in docstring),
    # explicitly call related functions.
    hintutil.loadhintconfig(ui)

    if names and names[0] == "agent":
        agent_instructions = agent.get_agent_instructions(ui, names)
        ui.write(agent_instructions)
        # Static config values have trailing whitespace trimmed, so instructions
        # coming from builtin_static lose their final newline.
        if not agent_instructions.endswith("\n"):
            ui.write("\n")
        return 0

    name = " ".join(names) if names and names != (None,) else None
    keep = opts.get(r"system") or []
    if len(keep) == 0:
        if sys.platform.startswith("win"):
            keep.append("windows")
        elif sys.platform == "OpenVMS":
            keep.append("vms")
        elif sys.platform == "plan9":
            keep.append("plan9")
        else:
            keep.append("unix")
            keep.append(sys.platform.lower())
    if ui.verbose:
        keep.append("verbose")
    if ui.agent():
        keep.append("agent")

    commands = sys.modules[__name__]
    formatted = help.formattedhelp(ui, commands, name, keep=keep, **opts)
    ui.pager("help")
    ui.write(formatted)
    hintutil.show(ui)


@command(
    "hint|hin",
    [("", "ack", False, _("acknowledge and silence hints"))],
    _("[--ack] NAME ..."),
    norepo=True,
)
def hint(ui, *names, **opts):
    """acknowledge hints

    ``@prog@ hint --ack NAME`` modifies hgrc to silence hints starting with
    ``hint[NAME]``.
    """
    if opts.get("ack") and names:
        hintutil.silence(ui, names)
        if not ui.quiet:
            ui.write(_("hints about %s are silenced\n") % _(", ").join(names))


@command(
    "histgrep",
    [
        ("0", "print0", None, _("end fields with NUL")),
        ("", "all", None, _("print all revisions that match")),
        ("a", "text", None, _("treat all files as text")),
        (
            "f",
            "follow",
            None,
            _("follow changeset history, or file history across copies and renames"),
        ),
        ("i", "ignore-case", None, _("ignore case when matching")),
        (
            "l",
            "files-with-matches",
            None,
            _("print only filenames and revisions that match"),
        ),
        ("n", "line-number", None, _("print matching line numbers")),
        (
            "r",
            "rev",
            [],
            _("only search files changed within revision range"),
            _("REV"),
        ),
        ("u", "user", None, _("list the author (long with -v)")),
        ("d", "date", None, _("list the date (short with -q)")),
    ]
    + formatteropts
    + walkopts,
    _("[OPTION]... PATTERN [FILE]..."),
    inferrepo=True,
    cmdtype=readonly,
)
def histgrep(ui, repo, pattern, *pats, **opts):
    """search backwards through history for a pattern in the specified files

    Search revision history for a regular expression in the specified
    files or the entire project.

    By default, grep prints the most recent revision number for each
    file in which it finds a match. To get it to print every revision
    that contains a change in match status ("-" for a match that becomes
    a non-match, or "+" for a non-match that becomes a match), use the
    --all flag.

    PATTERN can be any Python (roughly Perl-compatible) regular
    expression.

    If no FILEs are specified (and -f/--follow isn't set), all files in
    the repository are searched, including those that don't exist in the
    current branch or have been deleted in a prior changeset.

    .. container:: verbose

      ``histgrep.allowfullrepogrep`` controls whether the entire repo can be
      queried without any patterns, which can be expensive in big repositories.

    Returns 0 if a match is found, 1 otherwise.
    """
    if not util.istest():
        ui.deprecate(
            _("@prog@-histgrep"),
            "histgrep is deprecated because it does not scale - use diffgrep instead",
        )
    if not pats and not ui.configbool("histgrep", "allowfullrepogrep"):
        m = _("can't run histgrep on the whole repo, please provide filenames")
        h = _("this is disabled to avoid very slow greps over the whole repo")
        raise error.Abort(m, hint=h)

    reflags = re.M
    if opts.get("ignore_case"):
        reflags |= re.I
    try:
        regexp = re.compile(pattern, reflags)
    except re.error as inst:
        ui.warn(_("grep: invalid match pattern: %s\n") % inst)
        return 1
    sep, eol = ":", "\n"
    if opts.get("print0"):
        sep = eol = "\0"

    getfile = util.lrucachefunc(repo.file)

    def matchlines(body):
        body = body.decode(errors="replace")
        begin = 0
        linenum = 0
        while begin < len(body):
            match = regexp.search(body, begin)
            if not match:
                break
            mstart, mend = match.span()
            linenum += body.count("\n", begin, mstart) + 1
            lstart = body.rfind("\n", begin, mstart) + 1 or begin
            begin = body.find("\n", mend) + 1 or len(body) + 1
            lend = begin - 1
            yield linenum, mstart - lstart, mend - lstart, body[lstart:lend]

    class linestate:
        def __init__(self, line, linenum, colstart, colend):
            self.line = line
            self.linenum = linenum
            self.colstart = colstart
            self.colend = colend

        def __hash__(self):
            return hash((self.linenum, self.line))

        def __eq__(self, other):
            return self.line == other.line

        def findpos(self):
            """Iterate all (start, end) indices of matches"""
            yield self.colstart, self.colend
            p = self.colend
            while p < len(self.line):
                m = regexp.search(self.line, p)
                if not m:
                    break
                yield m.span()
                p = m.end()

    matches = {}
    copies = {}

    def grepbody(fn, rev, body):
        matches[rev].setdefault(fn, [])
        m = matches[rev][fn]
        for lnum, cstart, cend, line in matchlines(body):
            s = linestate(line, lnum, cstart, cend)
            m.append(s)

    def difflinestates(a, b):
        sm = difflib.SequenceMatcher(None, a, b)
        for tag, alo, ahi, blo, bhi in sm.get_opcodes():
            if tag == "insert":
                for i in range(blo, bhi):
                    yield ("+", b[i])
            elif tag == "delete":
                for i in range(alo, ahi):
                    yield ("-", a[i])
            elif tag == "replace":
                for i in range(alo, ahi):
                    yield ("-", a[i])
                for i in range(blo, bhi):
                    yield ("+", b[i])

    def display(fm, fn, ctx, pstates, states):
        rev = ctx.rev()
        node = ctx.node()
        if fm.isplain():
            formatuser = ui.shortuser
        else:
            formatuser = str
        if ui.quiet:
            datefmt = "%Y-%m-%d"
        else:
            datefmt = "%a %b %d %H:%M:%S %Y %1%2"
        found = False

        @util.cachefunc
        def binary():
            flog = getfile(fn)
            return util.binary(flog.read(ctx.filenode(fn)))

        fieldnamemap = {"filename": "file", "linenumber": "line_number"}
        if opts.get("all"):
            iter = difflinestates(pstates, states)
        else:
            iter = [("", l) for l in states]
        for change, l in iter:
            fm.startitem()
            fm.data(node=fm.hexfunc(ctx.node()))
            cols = [
                ("filename", fn, True),
                ("rev", rev, False),
                ("node", fm.hexfunc(node), True),
                ("linenumber", l.linenum, opts.get("line_number")),
            ]
            if opts.get("all"):
                cols.append(("change", change, True))
            cols.extend(
                [
                    ("user", formatuser(ctx.user()), opts.get("user")),
                    ("date", fm.formatdate(ctx.date(), datefmt), opts.get("date")),
                ]
            )
            lastcol = next(name for name, data, cond in reversed(cols) if cond)
            for name, data, cond in cols:
                field = fieldnamemap.get(name, name)
                fm.condwrite(cond, field, "%s", data, label="grep.%s" % name)
                if cond and name != lastcol:
                    fm.plain(sep, label="grep.sep")
            if not opts.get("files_with_matches"):
                fm.plain(sep, label="grep.sep")
                if not opts.get("text") and binary():
                    fm.plain(_(" Binary file matches"))
                else:
                    displaymatches(fm.nested("texts"), l)
            fm.plain(eol)
            found = True
            if opts.get("files_with_matches"):
                break
        return found

    def displaymatches(fm, l):
        p = 0
        for s, e in l.findpos():
            if p < s:
                fm.startitem()
                fm.write("text", "%s", l.line[p:s])
                fm.data(matched=False)
            fm.startitem()
            fm.write("text", "%s", l.line[s:e], label="grep.match")
            fm.data(matched=True)
            p = e
        if p < len(l.line):
            fm.startitem()
            fm.write("text", "%s", l.line[p:])
            fm.data(matched=False)
        fm.end()

    skip = {}
    revfiles = {}
    match = scmutil.match(repo[None], pats, opts)
    found = False
    follow = opts.get("follow")

    def prep(ctx, fns):
        rev = ctx.rev()
        pctx = ctx.p1()
        parent = pctx.rev()
        matches.setdefault(rev, {})
        matches.setdefault(parent, {})
        files = revfiles.setdefault(rev, [])
        for fn in fns:
            flog = getfile(fn)
            try:
                fnode = ctx.filenode(fn)
            except error.LookupError:
                continue

            copied = flog.renamed(fnode)
            copy = follow and copied and copied[0]
            if copy:
                copies.setdefault(rev, {})[fn] = copy
            if fn in skip:
                if copy:
                    skip[copy] = True
                continue
            files.append(fn)

            if fn not in matches[rev]:
                grepbody(fn, rev, flog.read(fnode))

            pfn = copy or fn
            if pfn not in matches[parent]:
                try:
                    fnode = pctx.filenode(pfn)
                    grepbody(pfn, parent, flog.read(fnode))
                except error.LookupError:
                    pass

    ui.pager("grep")
    fm = ui.formatter("grep", opts)
    for ctx in cmdutil.walkchangerevs(repo, match, opts, prep):
        rev = ctx.rev()
        parent = ctx.p1().rev()
        for fn in sorted(revfiles.get(rev, [])):
            states = matches[rev][fn]
            copy = copies.get(rev, {}).get(fn)
            if fn in skip:
                if copy:
                    skip[copy] = True
                continue
            pstates = matches.get(parent, {}).get(copy or fn, [])
            if pstates or states:
                r = display(fm, fn, ctx, pstates, states)
                found = found or r
                if r and not opts.get("all"):
                    skip[fn] = True
                    if copy:
                        skip[copy] = True
        del matches[rev]
        del revfiles[rev]
    fm.end()

    return not found


@command(
    "identify|id|ide|iden|ident|identi|identif",
    [
        ("r", "rev", "", _("identify the specified revision"), _("REV")),
        ("n", "num", None, _("show local revision number")),
        ("i", "id", None, _("show global revision id")),
        ("b", "branch", None, _("print 'default' (DEPRECATED)")),
        ("t", "tags", None, _("show tags (DEPRECATED)")),
        ("B", "bookmarks", None, _("show bookmarks")),
    ]
    + formatteropts,
    _("[-nibtB] [-r REV] [SOURCE]"),
    optionalrepo=True,
    cmdtype=readonly,
)
def identify(
    ui,
    repo,
    source=None,
    rev=None,
    num=None,
    id=None,
    branch=None,
    tags=None,
    bookmarks=None,
    **opts,
):
    """identify the working directory or specified revision

    Print a summary identifying the repository state at REV using one or
    two parent hash identifiers, followed by a "+" if the working
    directory has uncommitted changes and a list of bookmarks.

    When REV is not given, print a summary of the current state of the
    repository.

    Specifying a path to a repository root or @Product@ bundle will
    cause lookup to operate on that repository/bundle.

    .. container:: verbose

      Examples:

      - generate a build identifier for the working directory::

          @prog@ id --id > build-id.dat

      - check the most recent revision of a remote repository::

          @prog@ id -r tip https://www.mercurial-scm.org/repo/hg/

    See :prog:`log` for generating more information about specific revisions,
    including full hash identifiers.

    Returns 0 if successful.
    """
    if not util.istest():
        ui.deprecate(
            _("@prog@-identify"),
            _("identify is deprecated - use `@prog@ whereami` instead"),
        )
    if not repo and not source:
        raise error.Abort(_("there is no @Product@ repository here (.hg not found)"))

    if ui.debugflag:
        hexfunc = hex
    else:
        hexfunc = short
    default = not (num or id or branch or bookmarks)
    output = []

    if source:
        source = hg.parseurl(ui.expandpath(source))
        peer = hg.peer(repo or ui, opts, source)  # only pass ui when no repo
        repo = peer.local()

    fm = ui.formatter("identify", opts)
    fm.startitem()

    if not repo:
        if num or branch:
            raise error.Abort(_("can't query remote revision number or branch"))
        if not rev:
            rev = "tip"

        remoterev = peer.lookup(rev)
        hexrev = hexfunc(remoterev)
        if default or id:
            output = [hexrev]
        fm.data(id=hexrev)

        def getbms():
            bms = []

            if "bookmarks" in peer.listkeys("namespaces"):
                hexremoterev = hex(remoterev)
                bms = [
                    bm
                    for bm, bmr in peer.listkeys("bookmarks").items()
                    if bmr == hexremoterev
                ]

            return sorted(bms)

        bms = getbms()
        if bookmarks:
            output.extend(bms)
        elif default and not ui.quiet:
            # multiple bookmarks for a single parent separated by '/'
            bm = "/".join(bms)
            if bm:
                output.append(bm)

        fm.data(node=hex(remoterev))
        fm.data(bookmarks=fm.formatlist(bms, name="bookmark"))
    else:
        ctx = scmutil.revsingle(repo, rev, None)

        if ctx.rev() is None:
            ctx = repo[None]
            parents = ctx.parents()

            dirty = ""
            if ctx.dirty(missing=True, merge=False):
                dirty = "+"
            fm.data(dirty=dirty)

            hexoutput = [hexfunc(p.node()) for p in parents]
            if default or id:
                output = ["%s%s" % ("+".join(hexoutput), dirty)]
            fm.data(id="%s%s" % ("+".join(hexoutput), dirty))

            if num:
                numoutput = ["%d" % p.rev() for p in parents]
                output.append("%s%s" % ("+".join(numoutput), dirty))

            fn = fm.nested("parents")
            for p in parents:
                fn.startitem()
                fn.data(rev=p.rev())
                fn.data(node=p.hex())
                fn.context(ctx=p)
            fn.end()
        else:
            hexoutput = hexfunc(ctx.node())
            if default or id:
                output = [hexoutput]
            fm.data(id=hexoutput)

            if num:
                output.append(str(ctx.rev()))

        if default and not ui.quiet:
            # multiple bookmarks for a single parent separated by '/'
            bm = "/".join(ctx.bookmarks())
            if bm:
                output.append(bm)
        else:
            if branch:
                output.append("default")

            if bookmarks:
                output.extend(ctx.bookmarks())

        fm.data(node=ctx.hex())
        fm.data(bookmarks=fm.formatlist(ctx.bookmarks(), name="bookmark"))
        fm.context(ctx=ctx)

    fm.plain("%s\n" % " ".join(output))
    fm.end()


@command(
    "import|patch|im|imp|impo|impor|patc",
    [
        (
            "p",
            "strip",
            1,
            _(
                "directory strip option for patch. This has the same "
                "meaning as the corresponding patch option"
            ),
            _("NUM"),
        ),
        ("b", "base", "", _("base path (DEPRECATED)"), _("PATH")),
        ("e", "edit", False, _("invoke editor on commit messages")),
        (
            "f",
            "force",
            None,
            _("skip check for outstanding uncommitted changes (DEPRECATED)"),
        ),
        ("", "no-commit", None, _("don't commit, just update the working directory")),
        ("", "bypass", None, _("apply patch without touching the working directory")),
        ("", "partial", None, _("commit even if some hunks fail")),
        ("", "exact", None, _("abort if patch would apply lossily")),
        ("", "prefix", "", _("apply patch to subdirectory"), _("DIR")),
    ]
    + commitopts
    + commitopts2
    + similarityopts,
    _("[OPTION]... PATCH..."),
)
def import_(ui, repo, patch1=None, *patches, **opts):
    """import an ordered set of patches

    Import a list of patches and commit them individually (unless
    --no-commit is specified).

    To read a patch from standard input (stdin), use "-" as the patch
    name. If a URL is specified, the patch will be downloaded from
    there.

    Import first applies changes to the working directory (unless
    --bypass is specified), import will abort if there are outstanding
    changes.

    Use --bypass to apply and commit patches directly to the
    repository, without affecting the working directory. Without
    --exact, patches will be applied on top of the working directory
    parent revision.

    You can import a patch straight from a mail message. Even patches
    as attachments work (to use the body part, it must have type
    text/plain or text/x-patch). From and Subject headers of email
    message are used as default committer and commit message. All
    text/plain body parts before first diff are added to the commit
    message.

    If the imported patch was generated by :prog:`export`, user and
    description from patch override values from message headers and
    body. Values given on command line with -m/--message and -u/--user
    override these.

    If --exact is specified, import will set the working directory to
    the parent of each patch before applying it, and will abort if the
    resulting changeset has a different ID than the one recorded in
    the patch. This will guard against various ways that portable
    patch formats and mail systems might fail to transfer @Product@
    data or metadata. See :prog:`bundle` for lossless transmission.

    Use --partial to ensure a changeset will be created from the patch
    even if some hunks fail to apply. Hunks that fail to apply will be
    written to a <target-file>.rej file. Conflicts can then be resolved
    by hand before :prog:`commit --amend` is run to update the created
    changeset. This flag exists to let people import patches that
    partially apply without losing the associated metadata (author,
    date, description, ...).

    .. note::

       When no hunks apply cleanly, :prog:`import --partial` will create
       an empty changeset, importing only the patch metadata.

    With -s/--similarity, @prog@ will attempt to discover renames and
    copies in the patch in the same way as :prog:`addremove`.

    It is possible to use external patch programs to perform the patch
    by setting the ``ui.patch`` configuration option. For the default
    internal tool, the fuzz can also be configured via ``patch.fuzz``.
    See :prog:`help config` for more information about configuration
    files and how to use these options.

    See :prog:`help dates` for a list of formats valid for -d/--date.

    .. container:: verbose

      Examples:

      - import a traditional patch from a website and detect renames::

          @prog@ import -s 80 http://example.com/bugfix.patch

      - import a changeset from an hgweb server::

          @prog@ import https://www.mercurial-scm.org/repo/hg/rev/5ca8c111e9aa

      - import all the patches in an Unix-style mbox::

          @prog@ import incoming-patches.mbox

      - import patches from stdin::

          @prog@ import -

      - attempt to exactly restore an exported changeset (not always
        possible)::

          @prog@ import --exact proposed-fix.patch

      - use an external tool to apply a patch which is too fuzzy for
        the default internal tool.

          @prog@ import --config ui.patch="patch --merge" fuzzy.patch

      - change the default fuzzing from 2 to a less strict 7

          @prog@ import --config ui.fuzz=7 fuzz.patch

    Returns 0 on success, 1 on partial success (see --partial).
    """

    if not patch1:
        raise error.Abort(_("need at least one patch to import"))

    patches = (patch1,) + patches

    date = opts.get("date")
    if date:
        opts["date"] = util.parsedate(date)

    exact = opts.get("exact")
    update = not opts.get("bypass")
    if not update and opts.get("no_commit"):
        raise error.Abort(_("cannot use --no-commit with --bypass"))
    try:
        sim = float(opts.get("similarity") or 0)
    except ValueError:
        raise error.Abort(_("similarity must be a number"))
    if sim < 0 or sim > 100:
        raise error.Abort(_("similarity must be between 0 and 100"))
    if sim and not update:
        raise error.Abort(_("cannot use --similarity with --bypass"))
    if exact:
        if opts.get("edit"):
            raise error.Abort(_("cannot use --exact with --edit"))
        if opts.get("prefix"):
            raise error.Abort(_("cannot use --exact with --prefix"))

    base = opts["base"]
    wlock = dsguard = lock = tr = None
    msgs = []
    ret = 0

    try:
        wlock = repo.wlock()

        if update:
            cmdutil.checkunfinished(repo)
            if exact or not opts.get("force"):
                cmdutil.bailifchanged(repo)

        if not opts.get("no_commit"):
            lock = repo.lock()
            tr = repo.transaction("import")
        else:
            dsguard = dirstateguard.dirstateguard(repo, "import")
        parents = repo[None].parents()
        for patchurl in patches:
            if patchurl == "-":
                ui.status(_("applying patch from stdin\n"))
                patchfile = ui.fin
                patchurl = "stdin"  # for error message
            else:
                patchurl = os.path.join(base, patchurl)
                ui.status(_("applying %s\n") % patchurl)
                patchfile = hg.openpath(ui, patchurl)

            haspatch = False
            for hunk in patch.split(patchfile):
                (msg, node, rej) = cmdutil.tryimportone(
                    ui, repo, hunk, parents, opts, msgs, hg.clean
                )
                if msg:
                    haspatch = True
                    ui.note(msg + "\n")
                if update or exact:
                    parents = repo[None].parents()
                else:
                    parents = [repo[node]]
                if rej:
                    ui.write_err(_("patch applied partially\n"))
                    ui.write_err(
                        _("(fix the .rej files and run `@prog@ commit --amend`)\n")
                    )
                    ret = 1
                    break

            if not haspatch:
                raise error.Abort(_("%s: no diffs found") % patchurl)

        if tr:
            tr.close()
        if msgs:
            repo.savecommitmessage("\n* * *\n".join(msgs))
        if dsguard:
            dsguard.close()
        return ret
    finally:
        if tr:
            tr.release()
        release(lock, dsguard, wlock)


@command(
    "init",
    [
        ("", "git", None, _("use git as the backend (EXPERIMENTAL)")),
    ],
    _("[DEST]"),
    norepo=True,
    legacyaliases=["ini"],
)
def init(ui, dest=".", **opts):
    """create a new repository in the given directory

    Initialize a new repository in the given directory. If the given
    directory does not exist, it will be created. If no directory is
    given, the current directory is used.

    Returns 0 on success.
    """
    destpath = ui.expandpath(dest)
    usegit = opts.get("git")
    if usegit is None and ui.configbool("init", "prefer-git"):
        # In the OSS build, non-git mode doesn't give you a usable repo.
        ui.status_err(
            _(
                """Creating a ".sl" repo with Git compatible storage. For full "git" compatibility, create repo using "git init". See https://sapling-scm.com/docs/git/git_support_modes for more information."""
            )
        )
        usegit = True

    virtual_repo_size_factor = ui.configint(
        "format", "use-virtual-repo-with-size-factor"
    )
    if usegit:
        git.clone(ui, "", destpath)
    elif (
        ui.configbool("format", "use-eager-repo")
        or virtual_repo_size_factor is not None
    ):
        MAX_FACTOR_BITS = bindings.eagerepo.MAX_VIRTUAL_REPO_SIZE_FACTOR_BITS
        if (
            virtual_repo_size_factor is not None
            and not 0 <= virtual_repo_size_factor <= MAX_FACTOR_BITS
        ):
            raise error.Abort(
                _("format.use-virtual-repo-with-size-factor must be between 0 and %d")
                % MAX_FACTOR_BITS
            )
        eager_repo = bindings.eagerepo.EagerRepo.open(destpath)
        if virtual_repo_size_factor is not None:
            eager_repo.populate_virtual_commits(virtual_repo_size_factor)
            eager_repo.flush()
            # Virtual repos could include too many commits that conflict with
            # the revf64compat feature. Disable revf64compat.
            ident = bindings.identity.sniffdir(destpath)
            config_path = os.path.join(destpath, ident.dotdir(), ident.configrepofile())
            rcutil.editconfig(ui, config_path, "experimental", "revf64compat", "false")
            with open(
                os.path.join(destpath, ident.dotdir(), "store", "requires"),
                "a",
                newline="\n",
            ) as f:
                f.write("invalid-hash\n")
    else:
        if util.url(destpath).scheme == "bundle":
            hg.repository(ui, destpath, create=True)
        else:
            initial_config = None
            bindings.repo.repo.initialize(destpath, ui._rcfg, initial_config)

            if util.istest():
                # Mark legacy repos in tests with "eagercompat" requirement. This
                # indicates they use an eager store under the hood and implement
                # SaplingRemoteAPI.
                repo = hg.repository(ui, destpath)
                if repo.storage_format() == "revlog":
                    with repo.lock():
                        repo.storerequirements.add("eagercompat")
                        repo._writestorerequirements()


@command(
    "locate|loc|loca|locat",
    [
        ("r", "rev", "", _("search the repository as it is in REV"), _("REV")),
        ("0", "print0", None, _("end filenames with NUL, for use with xargs")),
        ("f", "fullpath", None, _("print complete paths from the filesystem root")),
    ]
    + walkopts,
    _("[OPTION]... [PATTERN]..."),
)
def locate(ui, repo, *pats, **opts):
    """locate files matching specific patterns (DEPRECATED)

    Print files under @Product@ control in the working directory whose
    names match the given patterns.

    By default, this command searches all directories in the working
    directory. To search just the current directory and its
    subdirectories, use "--include .".

    .. note::

       `locate` can be slow in large repositories. Consider using :prog:`files`
       with a path filter instead.

    If no patterns are given to match, this command prints the names
    of all files under @Product@ control in the working directory.

    If you want to feed the output of this command into the "xargs"
    command, use the -0 option to both this command and "xargs". This
    will avoid the problem of "xargs" treating single filenames that
    contain whitespace as multiple filenames.

    See :prog:`help files` for a more versatile command.

    Returns 0 if a match is found, 1 otherwise.
    """
    if opts.get("print0"):
        end = "\0"
    else:
        end = "\n"
    rev = scmutil.revsingle(repo, opts.get("rev"), None).node()

    ret = 1
    ctx = repo[rev]
    m = scmutil.match(ctx, pats, opts, default="relglob", badfn=lambda x, y: False)

    ui.pager("locate")
    for abs in ctx.matches(m):
        if opts.get("fullpath"):
            ui.write(repo.wjoin(abs), end)
        else:
            ui.write(((pats and m.rel(abs)) or abs), end)
        ret = 0

    return ret


@command(
    "log",
    [
        (
            "f",
            "follow",
            None,
            _("follow changeset history, or file history across copies and renames"),
        ),
        (
            "",
            "follow-first",
            None,
            _("only follow the first parent of merge changesets (DEPRECATED)"),
        ),
        ("d", "date", "", _("show revisions matching date spec"), _("DATE")),
        ("C", "copies", None, _("show copied files")),
        (
            "k",
            "keyword",
            [],
            _("do case-insensitive search for a given text"),
            _("TEXT"),
        ),
        ("r", "rev", [], _("show the specified revision or revset"), _("REV")),
        (
            "L",
            "line-range",
            [],
            _("follow line range of specified file (EXPERIMENTAL)"),
            _("FILE,RANGE"),
        ),
        ("", "removed", None, _("include revisions where files were removed")),
        ("m", "only-merges", None, _("show only merges (DEPRECATED)")),
        ("u", "user", [], _("revisions committed by user"), _("USER")),
        (
            "b",
            "branch",
            [],
            _("show changesets within the given named branch (DEPRECATED)"),
            _("BRANCH"),
        ),
        (
            "P",
            "prune",
            [],
            _("do not display revision or any of its ancestors"),
            _("REV"),
        ),
    ]
    + logopts
    + [
        (
            "t",
            "mutation",
            False,
            _("use mutation history for DAG and diffs (EXPERIMENTAL)"),
        ),
        (
            "",
            "show-missing",
            False,
            _("with --mutation, show commits not available locally"),
        ),
    ]
    + walkopts,
    _("[OPTION]... [FILE]"),
    inferrepo=True,
    cmdtype=readonly,
    legacyaliases=["history"],
)
def log(ui, repo, *pats, **opts):
    """show commit history

    Print the revision history of the specified files or the entire
    project.

    If no revision range is specified, the default revision range is the
    current commit and all of its ancestors (``::.``).

    The experimental ``--mutation`` option links the revisions by mutation
    (amend, rebase, fold, split and other rewrites) rather than by parent,
    and shows patches and ``--stat`` against the commit each one was created
    from. Its default revision range is the mutation history of the current
    commit (``mutationhistory(.)``); use ``-r 'mutationhistory(REV)'`` for
    another commit's. Commits that are not shown are skipped, with a dashed
    line over them; ``--show-missing`` also shows the commits not available
    locally.

    File history is shown without following the rename or copy
    history of files. To follow file history across renames and
    copies, use the ``-f/-- follow`` option. If the ``--follow``
    option is used without a filename, only the ancestors or
    descendants of the starting revision are shown.

    By default, :prog:`log` prints the commit's hash, non-trivial
    parents, user, date, time, and the single-line summary. When the
    ``-v/--verbose`` option is used, the list of changed files and
    full commit message are shown.

    With the ``--graph`` option, revisions are shown as an ASCII art
    graph with the most recent commit at the top. The graph nodes
    are depicted as follows: **o** is a commit, **@** is a working
    directory parent, **x** is obsolete, and **+** represents a fork
    where the commit from the lines below is a parent of the **o**
    merge on the same line. Paths in the graph are represented with
    **|**, **/** and so forth. **:** in place of a **|** indicates
    one or more revisions in a path are omitted.


    .. container:: verbose

      Use the ``-L/--line-range FILE,M:N`` option to follow the
      history of lines from **M** to **N** in FILE. With the ``-p/--
      patch`` option, only diff hunks affecting specified line range
      will be shown. The ``-L`` option can be specified multiple
      times and requires the ``--follow`` option. Currently, the line
      range option is not compatible with ``--graph`` and is an
      experimental feature.

    .. note::

      :prog:`log --patch` may generate unexpected diff output for merge
      commits, as it will only compare the merge commit against
      its first parent. Also, only files different from BOTH parents
      will appear in the **files:** section.

    .. note::

      For performance reasons, :prog:`log FILE` may omit duplicate changes
      made on branches and will not show removals or mode changes. To
      see all such changes, use the ``--removed`` switch.

    .. container:: verbose

       .. note::

          The history resulting from ``-L/--line-range`` options depends on
          diff options: for instance, if white-spaces are ignored,
          respective changes with only white-spaces in specified line range
          will not be listed.

    .. container:: verbose

      Some examples:

      - commits with full descriptions and file lists::

          @prog@ log -v

      - commits ancestral to the working directory::

          @prog@ log -f

      - last 10 commits on the current branch::

          @prog@ log -l 10 -b .

      - commits showing all modifications of a file, including removals::

          @prog@ log --removed file.c

      - all commits that touch a directory, with diffs, excluding merges::

          @prog@ log -Mp lib/

      - all revision numbers that match a keyword::

          @prog@ log -k bug --template "{rev}\\n"

      - the full hash identifier of the working directory parent::

          @prog@ log -r . --template "{node}\\n"

      - list available log templates::

          @prog@ log -T list

      - check if a given commit is included in a bookmarked release::

          @prog@ log -r "a21ccf and ancestor(release_1.9)"

      - find all commits by some user in a date range::

          @prog@ log -k alice -d "may 2008 to jul 2008"

      - commits touching lines 13 to 23 for file.c::

          @prog@ log -L file.c,13:23

      - commits touching lines 13 to 23 for file.c and lines 2 to 6 of
        main.c with patch::

          @prog@ log -L file.c,13:23 -L main.c,2:6 -p

    See :prog:`help dates` for a list of formats valid for ``-d/--date``.

    See :prog:`help revisions` for more about specifying and ordering
    revisions.

    See :prog:`help templates` for more about pre-packaged styles and
    specifying custom templates. The default template used by the log
    command can be customized via the ``ui.logtemplate`` configuration
    setting.

    Returns 0 on success.

    """
    linerange = opts.get("line_range")

    if linerange and not opts.get("follow"):
        raise error.Abort(_("--line-range requires --follow"))

    if linerange and pats:
        raise error.Abort(
            _("FILE arguments are not compatible with --line-range option")
        )

    if opts.get("follow") and opts.get("rev"):
        opts["rev"] = [revsetlang.formatspec("reverse(::%lr)", opts.get("rev"))]
        del opts["follow"]

    if opts.get("show_missing") and not (opts.get("mutation") and opts.get("graph")):
        raise error.Abort(_("--show-missing requires --mutation and --graph"))

    scmutil.maybe_show_path_typo_hint(ui, repo, pats, rev=opts.get("rev"))

    if opts.get("graph"):
        if linerange:
            raise error.Abort(_("graph not supported with line range patterns"))
        return cmdutil.graphlog(ui, repo, pats, opts)

    count = 0
    xreponame = None
    curr_repo, curr_pats, curr_opts = repo, pats, opts
    while True:
        lastctx, count = _dolog(ui, curr_repo, curr_pats, curr_opts, count, xreponame)
        if not lastctx:
            break

        xrepoinfo = cmdutil.xrepologinfo(curr_repo, curr_pats, curr_opts, lastctx)
        if not xrepoinfo:
            break

        from_repo, from_commit, from_path = xrepoinfo
        xreponame = from_repo.ui.config("remotefilelog", "reponame", "unknown")

        curr_repo = from_repo
        curr_pats = [os.path.join(from_repo.root, from_path)]
        curr_opts = curr_opts.copy()

        curr_opts["rev"] = [f"reverse(::{from_commit})"]


def _dolog(ui, repo, pats, opts, count, xreponame):
    revs, expr, filematcher = cmdutil.getlogrevs(repo, pats, opts)
    hunksfilter = None
    prevfn = None
    if opts.get("mutation"):
        prevfn = cmdutil.mutation_prevfn

    linerange = opts.get("line_range")
    if linerange:
        revs, lrfilematcher, hunksfilter = cmdutil.getloglinerangerevs(repo, revs, opts)

        if filematcher is not None and lrfilematcher is not None:
            basefilematcher = filematcher

            def filematcher(rev):
                files = basefilematcher(rev).files() + lrfilematcher(rev).files()
                return scmutil.matchfiles(repo, files)

        elif filematcher is None:
            filematcher = lrfilematcher

    limit = cmdutil.loglimit(opts)

    getrenamed = None
    if opts.get("copies"):
        endrev = None
        if opts.get("rev"):
            endrev = scmutil.revrange(repo, opts.get("rev")).max() + 1
        getrenamed = templatekw.getrenamedfn(repo, endrev=endrev)

    ui.pager("log")
    displayer = cmdutil.show_changeset(ui, repo, opts, buffered=True)

    template = opts.get("template") or ""
    symbols = templater.extractsymbols(repo, template)
    subdag = None
    if symbols is not None and "grandparents" in symbols:
        cl = repo.changelog
        subdag = cl.dag.subdag(cl.tonodes(revs))
        ui.debug("commands.log(): finished computing subdag\n")
    ctxstream = revs.prefetchbysymbols(symbols).iterctx()

    lastctx = None
    for ctx in ctxstream:
        lastctx = ctx
        rev = ctx.rev()
        copies = None
        if getrenamed is not None:
            copies = []
            for fn in ctx.files():
                rename = getrenamed(fn, rev)
                if rename:
                    copies.append((fn, rename[0]))
        if filematcher:
            revmatchfn = filematcher(ctx.rev())
        else:
            revmatchfn = None
        if hunksfilter:
            revhunksfilter = hunksfilter(rev)
        else:
            revhunksfilter = None
        revcache = {"copies": copies, "subdag": subdag, "xreponame": xreponame}
        displayer.show(
            ctx,
            revcache=revcache,
            matchfn=revmatchfn,
            hunksfilterfn=revhunksfilter,
            prevfn=prevfn,
        )
        if displayer.flush(ctx):
            count += 1
            if count == limit:
                lastctx = None
                break

    displayer.close()
    return lastctx, count


@command(
    "manifest|mani",
    [
        ("r", "rev", "", _("revision to display"), _("REV")),
        ("", "all", False, _("list files from all revisions (DEPRECATED)")),
    ]
    + formatteropts,
    _("[-r REV]"),
    cmdtype=readonly,
)
def manifest(ui, repo, node=None, rev=None, **opts):
    """output the current or given revision of the project manifest (DEPRECATED)

    See :prog:`help files` for a more versatile command.

    Print a list of version controlled files for the given revision.
    If no revision is given, the first parent of the working directory
    is used, or the null revision if no revision is checked out.

    .. note::

       This command lists all files and can be slow in large repositories.
       Consider using :prog:`files` with a path filter instead.

    With -v, print file permissions, symlink and executable bits.
    With --debug, print file revision hashes.

    If option --all is specified, the list of all files from all revisions
    is printed. This includes deleted and renamed files.

    Returns 0 on success.
    """
    fm = ui.formatter("manifest", opts)

    if opts.get("all"):
        raise error.Abort(_("--all not supported"))

    if rev and node:
        raise error.Abort(_("please specify just one revision"))

    if not node:
        node = rev

    char = {"l": "@", "x": "*", "m": "submod"}
    mode = {"x": "755", "m": ""}
    ctx = scmutil.revsingle(repo, node)
    mf = ctx.manifest()
    ui.pager("manifest")
    for f in ctx:
        fm.startitem()
        fl = ctx[f].flags()
        fm.condwrite(ui.debugflag, "hash", "%s ", hex(mf[f]))
        fm.condwrite(
            ui.verbose, "mode type", "%s %1s ", mode.get(fl, "644"), char.get(fl, "")
        )
        fm.write("path", "%s\n", f)
    fm.end()


@command(
    "merge",
    [
        (
            "f",
            "force",
            None,
            _("force a merge including outstanding changes (DEPRECATED)"),
        ),
        ("r", "rev", "", _("revision to merge"), _("REV")),
        ("P", "preview", None, _("review revisions to merge (no merge is performed)")),
        (
            "",
            "noconflict",
            None,
            _(
                "commit a conflict-free merge directly, or abort without "
                "touching the working copy if the merge would have conflicts "
                "(EXPERIMENTAL)"
            ),
        ),
        (
            "",
            "parent",
            [],
            _(
                "parent of the conflict-free merge, given twice; the working "
                "copy is not involved (with --noconflict, EXPERIMENTAL)"
            ),
            _("REV"),
        ),
    ]
    + mergetoolopts
    + commitopts
    + commitopts2,
    _("[OPTION].. [REV]"),
    legacyaliases=["mer", "merg"],
)
def merge(ui, repo, node=None, **opts):
    """merge another revision into working directory

    The current working directory is updated with all changes made in
    the requested revision since the last common predecessor revision.

    Files that changed between either parent are marked as changed for
    the next commit and a commit must be performed before any further
    updates to the repository are allowed. The next commit will have
    two parents.

    ``--tool`` can be used to specify the merge tool used for file
    merges. It overrides the HGMERGE environment variable and your
    configuration files. See :prog:`help merge-tools` for options.

    If no revision is specified, the working directory's parent is a
    head revision, and the current branch contains exactly one other
    head, the other head is merged with by default. Otherwise, an
    explicit revision with which to merge with must be provided.

    See :prog:`help resolve` for information on handling file conflicts.

    To undo an uncommitted merge, use :prog:`goto --clean .` which
    will check out a clean copy of the original merge parent, losing
    all changes.

    With ``--noconflict`` the merge is only performed if no file needs manual
    resolution. It is then committed right away (use ``-m`` for the message)
    and the working copy is moved to the new commit; the commit is marked as
    conflict-free and cannot be amended. Otherwise the command aborts, listing
    the conflicting files, and leaves the working copy untouched. Both commits
    must be draft.

    ``--parent REV --parent REV`` names both parents of a conflict-free merge
    in order, instead of merging into the working copy parent. The working
    copy is not involved: it may be dirty and it stays where it is. The new
    commit is only printed.

    .. container:: verbose

      Regular merges can be disabled by setting ``ui.allowmerge`` to false.
      Conflict-free merges are controlled by ``experimental.noconflict-merge``,
      which is enabled by default.

    Returns 0 on success, 1 if there are unresolved files.
    """
    parents = opts.get("parent")
    if parents:
        if not opts.get("noconflict"):
            raise error.Abort(_("--parent requires --noconflict"))
        if node or opts.get("rev"):
            raise error.Abort(_("--parent cannot be combined with REV or --rev"))
        if len(parents) != 2:
            raise error.Abort(_("--parent must be given exactly twice"))
    if not opts.get("noconflict"):
        for opt in ("message", "logfile", "date", "user"):
            if opts.get(opt):
                raise error.Abort(_("--%s requires --noconflict") % opt)
    # Conflict-free merges only record that two draft commits can be merged
    # automatically and are meant for stacks, so they are allowed where merges
    # otherwise are not.
    if not ui.configbool("ui", "allowmerge", default=True) and not opts.get(
        "noconflict"
    ):
        raise error.Abort(
            _("merging is not supported for this repository"),
            hint=_(
                "use rebase, or '@prog@ merge --noconflict' for a conflict-free merge"
            ),
        )
    if opts.get("rev") and node:
        raise error.Abort(_("please specify just one revision"))
    if not node:
        node = opts.get("rev")

    if parents:
        p1node, node = (scmutil.revsingle(repo, p).node() for p in parents)
    else:
        p1node = repo["."].node()
        if node:
            node = scmutil.revsingle(repo, node).node()
        if not node:
            node = repo[destutil.destmerge(repo)].node()

    max_distance = ui.configint("merge", "max-distance")
    if max_distance:
        # merge distance is computed as the number of commit between the common ancestors and the merge
        distance = repo.dageval(
            lambda: len(
                range(
                    children(gcaall(lookup(p1node) + lookup(node))),
                    lookup(p1node) + lookup(node),
                )
            )
        )
        if distance > max_distance:
            raise error.Abort(
                _("merging distant ancestors is not supported for this repository"),
                hint=_("use rebase instead"),
            )

    if opts.get("preview"):
        # find nodes that are ancestors of p2 but not of p1
        nodes = repo.changelog.findmissing(common=[p1node], heads=[node])

        displayer = cmdutil.show_changeset(ui, repo, opts)
        for node in nodes:
            displayer.show(repo[node])
        displayer.close()
        return 0

    # ui.forcemerge is an internal variable, do not document
    with ui.configoverride({("ui", "forcemerge"): opts.get("tool", "")}, "merge"):
        force = opts.get("force")
        labels = ["working copy", "merge rev"]
        if not opts.get("noconflict"):
            return hg.merge(repo, node, force=force, labels=labels)
        return _noconflictmerge(
            ui, repo, repo[p1node], repo[node], labels, opts, update=not parents
        )


def _noconflictmerge(ui, repo, p1, p2, labels, opts, update):
    """Merge `p2` into `p1` in memory and commit the result as a conflict-free
    merge. With `update` the working copy, which must be clean, is moved to
    the new commit; otherwise it is left alone."""
    if not ui.configbool("experimental", "noconflict-merge", default=True):
        raise error.Abort(
            _("conflict-free merges are disabled by experimental.noconflict-merge"),
            hint=_("set experimental.noconflict-merge=true to enable them"),
        )
    with repo.wlock(), repo.lock():
        if update:
            cmdutil.bailifchanged(repo)
        if p1 == p2:
            raise error.Abort(
                _("cannot create a conflict-free merge of %s with itself") % p1
            )
        for ctx in (p1, p2):
            if ctx.ispublic():
                raise error.Abort(
                    _("cannot create a conflict-free merge with public commit %s")
                    % ctx,
                    hint=_(
                        "both parents must be draft; rebase onto the public "
                        "commit instead"
                    ),
                )
        wctx = mergemod.check_noconflict_merge(
            repo, p2.node(), force=opts.get("force"), labels=labels, basectx=p1
        )
        message = cmdutil.logmessage(repo, opts) or _("Merge %s") % (
            p2.description().split("\n", 1)[0] or p2
        )
        with repo.transaction("merge"):
            memctx = wctx.tomemctx(
                message,
                parents=(p1, p2),
                date=opts.get("date") or None,
                user=opts.get("user") or None,
                extra={mergemod.NOCONFLICT_MERGE_EXTRA: "1"},
            )
            newnode = repo.commitctx(memctx)
        if update:
            hg.updaterepo(repo, newnode, False)
    ui.status(_("created conflict-free merge %s\n") % repo[newnode])
    return 0


@command(
    "parents|parent",
    [("r", "rev", "", _("show parents of the specified revision"), _("REV"))]
    + templateopts,
    _("[-r REV] [FILE]"),
    inferrepo=True,
    legacyaliases=["par", "pare", "paren"],
)
def parents(ui, repo, file_=None, **opts):
    """show the parents of the working directory or revision (DEPRECATED)

    Print the working directory's parent revisions. If a revision is
    given via -r/--rev, the parent of that revision will be printed.
    If a file argument is given, the revision in which the file was
    last changed (before the working directory revision or the
    argument to --rev if given) is printed.

    This command is equivalent to::

        @prog@ log -r "p1()+p2()" or
        @prog@ log -r "p1(REV)+p2(REV)" or
        @prog@ log -r "max(::p1() and file(FILE))+max(::p2() and file(FILE))" or
        @prog@ log -r "max(::p1(REV) and file(FILE))+max(::p2(REV) and file(FILE))"

    See :prog:`summary` and :prog:`help revsets` for related information.

    Returns 0 on success.
    """
    if not util.istest():
        ui.deprecate(
            _("@prog@-parents"),
            _("parents is deprecated - use `@prog@ log -r 'parents(.)'` instead"),
        )

    ctx = scmutil.revsingle(repo, opts.get("rev"), None)

    if file_:
        m = scmutil.match(ctx, (file_,), opts)
        if len(m.files()) != 1:
            raise error.Abort(_("can only specify an explicit filename"))
        file_ = m.files()[0]
        parent_nodes = [pctx.node() for pctx in ctx.parents() if file_ in pctx]
        if not parent_nodes:
            raise error.Abort(_("'%s' not found in manifest!") % file_)
        dag = repo.changelog.dag
        p = []
        covered = dag.sort([])
        # To preserve order, we don't pass ancestors(parent_nodes) to
        # pathhistory but iterate parents one by one.
        for parent_node in parent_nodes:
            nodes = dag.ancestors([parent_node])
            history = repo.pathhistory(m.files(), nodes)
            for node in history:
                if node not in covered:
                    p.append(node)
                    covered |= dag.ancestors([node]) | dag.range([node], parent_nodes)
                if not (nodes - covered):
                    # No more interesting history.
                    break
    else:
        p = [cp.node() for cp in ctx.parents()]

    displayer = cmdutil.show_changeset(ui, repo, opts)
    for n in p:
        if n != nullid:
            displayer.show(repo[n])
    displayer.close()


@command("paths|path", formatteropts, _("[NAME]"), optionalrepo=True, cmdtype=readonly)
def paths(ui, repo, search=None, **opts):
    """show aliases for remote repositories

    Show definition of symbolic path name NAME. If no name is given,
    show definition of all available names.

    Option -q/--quiet suppresses all output when searching for NAME
    and shows only the path names when listing all definitions.

    Path names are defined in the [paths] section of your
    configuration file and in ``/etc/mercurial/hgrc``. If run inside a
    repository, ``.hg/hgrc`` is used, too.

    The path names ``default`` and ``default-push`` have a special
    meaning.  When performing a push or pull operation, they are used
    as fallbacks if no location is specified on the command-line.
    When ``default-push`` is set, it will be used for push and
    ``default`` will be used for pull; otherwise ``default`` is used
    as the fallback for both.  When cloning a repository, the clone
    source is written as ``default`` in ``.hg/hgrc``.

    .. note::

       ``default`` and ``default-push`` apply to all inbound (e.g.
       :prog:`incoming`) and outbound (e.g. :prog:`outgoing`, :prog:`email`
       and :prog:`bundle`) operations.

    See :prog:`help urls` for more information.

    Returns 0 on success.
    """

    ui.pager("paths")
    if search:
        pathitems = [(name, path) for name, path in ui.paths.items() if name == search]
    else:
        pathitems = sorted(ui.paths.items())

    fm = ui.formatter("paths", opts)
    if fm.isplain():
        hidepassword = util.hidepassword
    else:
        hidepassword = str
    if ui.quiet:
        namefmt = "%s\n"
    else:
        namefmt = "%s = "
    showsubopts = not search and not ui.quiet

    for name, path in pathitems:
        fm.startitem()
        fm.condwrite(not search, "name", namefmt, name)
        fm.condwrite(not ui.quiet, "url", "%s\n", hidepassword(path.rawloc))
        for subopt, value in sorted(path.suboptions.items()):
            assert subopt not in ("name", "url")
            if showsubopts:
                fm.plain("%s:%s = " % (name, subopt))
            fm.condwrite(showsubopts, subopt, "%s\n", value)

    fm.end()

    if search and not pathitems:
        if not ui.quiet:
            ui.warn(_("not found!\n"))
        return 1
    else:
        return 0


@command(
    "phase",
    [
        ("p", "public", False, _("set changeset phase to public")),
        ("d", "draft", False, _("set changeset phase to draft")),
        ("s", "secret", False, _("set changeset phase to secret")),
        ("f", "force", False, _("allow to move boundary backward")),
        ("r", "rev", [], _("target revision"), _("REV")),
    ],
    _("[OPTION]... [REV...]"),
    legacyaliases=["ph", "pha", "phas"],
)
def phase(ui, repo, *revs, **opts):
    """set or show the current phase name

    With no argument, show the phase name of the current revision(s).

    With one of -p/--public, -d/--draft or -s/--secret, change the
    phase value of the specified revisions.

    Unless -f/--force is specified, :prog:`phase` won't move changesets from a
    lower phase to a higher phase. Phases are ordered as follows::

        public < draft < secret

    Returns 0 on success, 1 if some phases could not be changed.

    (For more information about the phases concept, see :prog:`help phases`.)
    """
    # search for a unique phase argument
    targetphase = None
    for idx, name in enumerate(phases.phasenames):
        if opts[name]:
            if targetphase is not None:
                raise error.Abort(_("only one phase can be specified"))
            targetphase = idx

    # look for specified revision
    revs = list(revs)
    revs.extend(opts["rev"])
    if not revs:
        # display both parents as the second parent phase can influence
        # the phase of a merge commit
        revs = [hex(c) for c in repo.working_parent_nodes()]

    revs = scmutil.revrange(repo, revs)

    lock = None
    ret = 0
    if targetphase is None:
        # display
        for r in revs:
            ctx = repo[r]
            ui.write("%s: %s\n" % (ctx.hex(), ctx.phasestr()))
    else:
        if repo.ui.configbool("experimental", "narrow-heads"):
            ui.warn(
                _(
                    "(phases are now managed by remotenames and heads; manually editing phases is a no-op)\n"
                )
            )
            return 0
        tr = None
        lock = repo.lock()
        try:
            tr = repo.transaction("phase")
            # set phase
            if not revs:
                raise error.Abort(_("empty revision set"))
            nodes = [repo[r].node() for r in revs]

            phases.advanceboundary(repo, tr, targetphase, nodes)
            if opts["force"]:
                phases.retractboundary(repo, tr, targetphase, nodes)
            tr.close()
        finally:
            if tr is not None:
                tr.release()
            lock.release()

        # moving revision from public to draft may hide them
        # We have to check result on an unfiltered repository
        unfi = repo
        cl = unfi.changelog
        getphase = unfi._phasecache.phase
        rejected = [n for n in nodes if getphase(unfi, cl.rev(n)) < targetphase]
        if rejected:
            ui.warn(
                _("cannot move %i changesets to a higher phase, use --force\n")
                % len(rejected)
            )
            ret = 1
    return ret


def postincoming(ui, repo, modheads, optupdate, checkout, brev):
    """Run after a changegroup has been added via pull/unbundle

    This takes arguments below:

    :modheads: change of heads by pull/unbundle
    :optupdate: updating working directory is needed or not
    :checkout: update destination revision (or None to default destination)
    :brev: a name, which might be a bookmark to be activated after updating
    """
    if optupdate:
        try:
            return hg.updatetotally(ui, repo, checkout or repo["tip"].node(), brev)
        except error.UpdateAbort as inst:
            msg = _("not updating: %s") % str(inst)
            hint = inst.hint
            raise error.UpdateAbort(msg, hint=hint)


@command(
    "pull",
    [
        (
            "u",
            "update",
            None,
            _("update to new branch head if new descendants were pulled"),
        ),
        ("f", "force", None, _("run even when remote repository is unrelated")),
        ("r", "rev", [], _("a remote commit to pull"), _("REV")),
        ("B", "bookmark", [], _("a bookmark to pull"), _("BOOKMARK")),
    ],
    _("[OPTION]... [-r REV]... [SOURCE]"),
    legacyaliases=["pul"],
)
def pull(ui, repo, source="default", **opts):
    """pull commits from the specified source

    Pull commits from a remote repository to a local one. This command modifies
    the commit graph, but doesn't mutate local commits or the working copy.

    Use ``-B/--bookmark`` to specify a remote bookmark to pull. For Git
    repos, remote bookmarks correspond to branches. If no bookmark is
    specified, a default set of relevant remote names are pulled.

    If SOURCE is omitted, the default path is used. Use :prog:`path
    --add` to add a named source.

    See :prog:`help urls` and :prog:`help path` for more information.

    .. container:: verbose

      Examples:

      - pull relevant remote bookmarks from default source::

          @prog@ pull

      - pull a bookmark named my-branch from source my-fork::

          @prog@ pull my-fork --bookmark my-branch

    .. container:: verbose

        You can use ``.`` for BOOKMARK to specify the active bookmark.

    Returns 0 on success, 1 on failure, including if ``--update`` was
    specified but the update had unresolved conflicts.
    """
    if ui.configbool("pull", "automigrate"):
        repo.automigratestart()

    # Allows us to announce larger changes affecting all the users by displaying
    # config-driven hint on pull.
    for name, _message in ui.configitems("hint-definitions"):
        if name.startswith("pull:"):
            hintutil.trigger(name)

    source = hg.parseurl(ui.expandpath(source))
    ui.status_err(_("pulling from %s\n") % util.hidepassword(source))

    modheads, checkout = _newpull(ui, repo, source, **opts)

    # brev is a name, which might be a bookmark to be activated at
    # the end of the update. In other words, it is an explicit
    # destination of the update
    brev = None

    # Run 'update' in another transaction.
    if checkout and checkout in repo:
        checkout = repo[checkout].node()

        # opts['bookmark'] is ignored, because 'checkout' is determined without it.
        if opts.get("rev"):
            brev = opts["rev"][0]
        else:
            brev = None
    repo._subtoppath = source
    try:
        ret = postincoming(ui, repo, modheads, opts.get("update"), checkout, brev)

    finally:
        del repo._subtoppath

    if ui.configbool("pull", "automigrate"):
        repo.automigratefinish()

    return ret


def _newpull(ui, repo, source, **opts):
    """Main logic of a modern pull command.

    Do not use named branches.
    Do not issue duplicated listkey commands.
    No remotenames race conditions.
    """
    revs = opts.get("rev") or []
    bmarks = opts.get("bookmark") or []
    checkout = None
    if revs:
        revs = autopull.rewritepullrevs(repo, revs)
        checkout = revs[0]

    url = ui.paths.getpath(source)
    remotename = bookmarks.remotenameforurl(ui, url.rawloc)
    selected = bookmarks.selectivepullbookmarknames(repo, remotename)

    if not bmarks:
        bmarks += selected
    elif ui.configbool("pull", "include-default-bookmarks"):
        bmarks = list({*bmarks, *selected[:1]})

    # De-duplicate.
    bmarks = sorted(set(bmarks))

    # Pull commits and detect changes.
    oldlen = len(repo)
    repo.pull(
        source,
        bookmarknames=bmarks,
        headnames=revs,
        quiet=False,
        force=opts.get("force"),
    )
    newlen = len(repo)

    # Check that required bookmarks are pulled (repo.pull does not raise on
    # missing bookmarks).
    for name in opts.get("bookmark") or []:
        fullname = "%s/%s" % (remotename, name)
        if fullname not in repo:
            raise error.Abort(_("remote bookmark %s not found") % name)
        if checkout is None:
            checkout = fullname
    if checkout is None:
        checkout = "tip"

    # Convert remote bookmark names to {name: hexnode} dict.
    def namestonamehex(names, repo=repo, remotename=remotename):
        result = {}
        for name in names:
            fullname = "%s/%s" % (remotename, name)
            if fullname in repo:
                result[name] = repo[fullname].hex()
        return result

    # Decide return value.
    if oldlen == newlen:
        # Not changed.
        modheads = 0
    else:
        # Changed.
        modheads = 1

    return modheads, checkout


@command(
    "push",
    [
        ("f", "force", None, _("force push")),
        (
            "r",
            "rev",
            [],
            _("a commit to push"),
            _("REV"),
        ),
        ("B", "bookmark", [], _("bookmark to push (ADVANCED)"), _("BOOKMARK")),
        ("", "new-branch", False, _("allow pushing a new branch (DEPRECATED)")),
        ("", "pushvars", [], _("variables that can be sent to server (ADVANCED)")),
    ],
    _("[OPTION]... [--to BOOKMARK] [-r REV]... [DEST]"),
)
def push(ui, repo, dest=None, **opts):
    """push commits to the specified destination

    Push commits from the local repository to the specified
    destination.

    Use ``-t/--to`` to specify the remote bookmark. For Git repos,
    remote bookmarks correspond to Git branches.

    To add a named remote destination, see :prog:`path --add`.

    ``-r/--rev`` specifies the commit(s) (including ancestors) to push to
    the remote repository. Defaults to the current commit.

    Add ``--create`` to create the remote bookmark if it doesn't already exist.

    Add ``--no-rebase`` to move the remote bookmark to exactly the pushed
    commit instead of rebasing the commit onto the remote bookmark, which
    ``--rebase`` (the default) does. The move must be a fast-forward unless
    ``-f/--force`` is given or the ``NON_FAST_FORWARD=true`` push var is set.
    ``--rebase`` and ``--no-rebase`` require ``--to``. Git pushes never
    rebase, so ``--no-rebase`` changes nothing for Git repos and ``--rebase``
    is an error.

    The ``-f/--force`` flag allows non-fast-forward pushes.

    If DESTINATION is omitted, the default path will be used. See
    :prog:`help urls` and :prog:`help path` for more information.

    .. container:: verbose

      Examples:

      - push your current commit to "main" on the default destination::

          @prog@ push --to main

      - force push commit 05a82320d to "my-branch" on the "my-fork" destination::

          @prog@ push --rev 05a82320d my-fork --to my-branch --force

    .. container:: verbose

        The ``--pushvars`` flag sends key-value metadata to the server.
        For example, ``--pushvars ENABLE_SOMETHING=true``. Push vars are
        typically used to override commit hook behavior, or enable extra
        debugging. Push vars are not supported for Git repos.

    Returns 0 on success.
    """

    if opts.get("bookmark"):
        ui.setconfig("bookmarks", "pushing", opts["bookmark"], "push")
        for b in opts["bookmark"]:
            # translate -B options to -r so changesets get pushed
            b = repo._bookmarks.expandname(b)
            if b in repo._bookmarks:
                opts.setdefault("rev", []).append(b)
            else:
                # if we try to push a deleted bookmark, translate it to null
                # this lets simultaneous -r, -b options continue working
                opts.setdefault("rev", []).append("null")

    path = ui.paths.getpath(dest, default=("default-push", "default"))
    if not path:
        raise error.Abort(
            _("default repository not configured!"),
            hint=_("see '@prog@ help config.paths'"),
        )
    dest = path.pushloc or path.loc
    ui.status_err(_("pushing to %s\n") % util.hidepassword(dest))
    revs = opts.get("rev") or None
    other = hg.peer(repo, opts, dest)

    if revs:
        clnode = repo.changelog.node
        if all(isinstance(r, int) for r in revs):
            revs = [clnode(r) for r in revs]
        else:
            revs = [clnode(r) for r in scmutil.revrange(repo, revs)]
        if not revs:
            raise error.Abort(
                _("specified revisions evaluate to an empty set"),
                hint=_("use different revision arguments"),
            )
    elif path.pushrev:
        # It doesn't make any sense to specify ancestor revisions. So limit
        # to DAG heads to make discovery simpler.
        expr = revsetlang.formatspec("heads(%r)", path.pushrev)
        revs = scmutil.revrange(repo, [expr])
        revs = [repo[rev].node() for rev in revs]
        if not revs:
            raise error.Abort(
                _("default push revset for path evaluates to an empty set")
            )

    if ui.configbool("push", "requirereason"):
        pushvar = "PUSH_REASON="
        reasons = list(v for v in opts.get("pushvars", []) if v.startswith(pushvar))
        if reasons:
            reason = reasons[-1][len(pushvar) :]
            ui.log(
                "pushreason",
                "bypassing push block with reason: %s",
                reason,
                pushreason=reason,
            )
        else:
            msg = ui.config("push", "requirereasonmsg")
            raise error.Abort(msg, hint="use `--pushvars PUSH_REASON='because ...'`")

    opargs = dict(opts.get("opargs", {}))  # copy opargs since we may mutate it
    opargs.setdefault("pushvars", []).extend(opts.get("pushvars", []))

    pushop = exchange.push(
        repo,
        other,
        opts.get("force"),
        revs=revs,
        bookmarks=opts.get("bookmark", ()),
        opargs=opargs,
    )

    result = not pushop.cgresult

    if pushop.bkresult is not None:
        if pushop.bkresult == 2:
            result = 2
        elif not result and pushop.bkresult:
            result = 2

    return result


@command(
    "record",
    [
        (
            "A",
            "addremove",
            None,
            _("mark new/missing files as added/removed before committing"),
        ),
        ("", "amend", None, _("amend the parent of the working directory")),
        ("s", "secret", None, _("use the secret phase for committing")),
        ("e", "edit", None, _("invoke editor on commit messages")),
    ]
    + commitopts
    + commitopts2
    + diffwsopts
    + walkopts,
    _("[OPTION]... [FILE]..."),
)
def record(ui, repo, *pats, **opts):
    """interactively select changes to commit

    If a list of files is omitted, all changes reported by :prog:`status`
    will be candidates for recording.

    See :prog:`help dates` for a list of formats valid for -d/--date.

    If using the text interface (see :prog:`help config`),
    you will be prompted for whether to record changes to each
    modified file, and for files with multiple changes, for each
    change to use. For each query, the following responses are
    possible::

      y - record this change
      n - skip this change
      e - edit this change manually

      s - skip remaining changes to this file
      f - record remaining changes to this file

      d - done, skip remaining changes and files
      a - record all changes to all remaining files
      q - quit, recording no changes

      ? - display help

    This command is not available when committing a merge."""
    if not util.istest():
        ui.deprecate(
            _("@prog@-record"),
            _("record is deprecated - use `@prog@ commit -i` instead"),
        )

    if not ui.interactive():
        raise error.Abort(_("running non-interactively, use %s instead") % "commit")

    opts[r"interactive"] = True
    overrides = {("experimental", "crecord"): False}
    with ui.configoverride(overrides, "record"):
        return commit(ui, repo, *pats, **opts)


@command("recover", [])
def recover(ui, repo):
    """roll back an interrupted transaction

    Recover from an interrupted commit or pull.

    This command tries to fix the repository status after an
    interrupted operation. It should only be necessary when @Product@
    suggests it.

    Returns 0 if successful, 1 if nothing to recover.
    """
    if repo.recover():
        return 0
    return 1


@command(
    "remove|rm",
    [
        ("", "mark", None, _("mark as a deletion for already missing files")),
        ("A", "after", None, _("alias to --mark (DEPRECATED)")),
        ("f", "force", None, _("forget added files, delete modified files")),
    ]
    + walkopts,
    _("[OPTION]... FILE..."),
    inferrepo=True,
    legacyaliases=["rem", "remo", "remove"],
)
def remove(ui, repo, *pats, **opts):
    """delete the specified tracked files

    Remove the specified tracked files from the repository and delete
    them. The files will be deleted from the repository at the next
    commit.

    To undo a remove before files have been committed, use :prog:`revert`.
    To stop tracking files without deleting them, use :prog:`forget`.

    .. container:: verbose

      ``--mark`` can be used to remove only files that have already
      been deleted, ``-f/--force`` can be used to force deletion, and ``-Af``
      can be used to remove files from the next revision without
      deleting them from the working directory.

      The following table details the behavior of remove for different
      file states (columns) and option combinations (rows). The file
      states are Added (**A**), Clean (**C**), Modified (**M**) and
      Missing (**!**) (as reported by :prog:`status`). The actions are
      Warn (**W**), Remove (**R**) (from branch) and Delete (**D**)
      (from disk):

      =========    =====  ====== ====== =====
      opt/state    **A**  **C**  **M**  **!**
      =========    =====  ====== ====== =====
      none         **W**  **RD** **W**  **R**
      ``-f``       **R**  **RD** **RD** **R**
      ``-A``       **W**  **W**  **W**  **R**
      ``-Af``      **R**  **R**  **R**  **R**
      =========    =====  ====== ====== =====

      .. note::

         :prog:`remove` never deletes files in **Added** state from the
         working directory, not even if ``--force`` is specified.

    Returns 0 on success, 1 if any warnings encountered.
    """

    mark = opts.get("mark") or opts.get("after")
    force = opts.get("force")
    if not pats and not mark:
        raise error.Abort(_("no files specified"))

    m = scmutil.match(repo[None], pats, opts)
    return cmdutil.remove(ui, repo, m, mark, force)


@command(
    "rename|move|mv",
    [
        ("", "mark", None, _("mark a rename that has already occurred")),
        ("", "amend", None, _("amend the current commit to mark a rename")),
        ("A", "after", None, _("alias to --mark (DEPRECATED)")),
        ("f", "force", None, _("forcibly copy over an existing managed file")),
    ]
    + walkopts
    + dryrunopts,
    _("[OPTION]... SOURCE... DEST"),
    legacyaliases=["ren", "rena", "renam", "mo", "mov"],
)
def rename(ui, repo, *pats, **opts):
    """rename files; equivalent of copy + remove

    Mark dest as copies of sources; mark sources for deletion. If dest
    is a directory, copies are put in that directory. If dest is a
    file, there can only be one source.

    By default, this command copies the contents of files as they
    exist in the working directory. If invoked with --mark, the
    operation is recorded, but no copying is performed.

    This command takes effect at the next commit. To undo a rename
    before that, see :prog:`revert`.

    Returns 0 on success, 1 if errors are encountered.
    """
    with repo.wlock():
        return cmdutil.copy(ui, repo, pats, opts, rename=True)


@command(
    "resolve",
    [
        ("a", "all", None, _("select all unresolved files")),
        ("l", "list", None, _("list state of files needing merge")),
        ("m", "mark", None, _("mark files as resolved")),
        ("u", "unmark", None, _("mark files as unresolved")),
        ("n", "no-status", None, _("hide status prefix")),
        ("", "root-relative", None, _("show paths relative to repo root")),
    ]
    + mergetoolopts
    + walkopts
    + formatteropts,
    _("[OPTION]... [FILE]..."),
    inferrepo=True,
    legacyaliases=["reso", "resol", "resolv"],
)
def resolve(ui, repo, *pats, **opts):
    """redo merges or set/view the merge status of files

    Merges with unresolved conflicts are often the result of
    non-interactive merging using the ``internal:merge`` configuration
    setting, or a command-line merge tool like ``diff3``. The resolve
    command is used to manage the files involved in a merge, after
    :prog:`merge` has been run, and before :prog:`commit` is run (i.e. the
    working directory must have two parents). See :prog:`help
    merge-tools` for information on configuring merge tools.

    The resolve command can be used in the following ways:

    - :prog:`resolve [--tool TOOL] FILE...`: attempt to re-merge the specified
      files, discarding any previous merge attempts. Re-merging is not
      performed for files already marked as resolved. Use ``--all/-a``
      to select all unresolved files. ``--tool`` can be used to specify
      the merge tool used for the given files. It overrides the HGMERGE
      environment variable and your configuration files.  Previous file
      contents are saved with a ``.orig`` suffix.

    - :prog:`resolve -m [FILE]`: mark a file as having been resolved
      (e.g. after having manually fixed-up the files). The default is
      to mark all unresolved files.

    - :prog:`resolve -u [FILE]...`: mark a file as unresolved. The
      default is to mark all resolved files.

    - :prog:`resolve -l`: list files which had or still have conflicts.
      In the printed list, ``U`` = unresolved and ``R`` = resolved.
      You can use ``set:unresolved()`` or ``set:resolved()`` to filter
      the list. See :prog:`help filesets` for details.

    .. note::

       @Product@ will not let you commit files with unresolved merge
       conflicts. You must use :prog:`resolve -m ...` before you can
       commit after a conflicting merge.

    Returns 0 on success, 1 if any files fail a resolve attempt.
    """

    flaglist = "all mark unmark list no_status root_relative".split()
    all, mark, unmark, show, nostatus, rootrel = [opts.get(o) for o in flaglist]

    # Enable --root-relative by default if HGPLAIN is set, for compatibility.
    if rootrel is None and ui.plain():
        rootrel = True

    if (show and (mark or unmark)) or (mark and unmark):
        raise error.Abort(_("too many options specified"))
    if pats and all:
        raise error.Abort(_("can't specify --all and patterns"))
    if not (all or pats or show or mark or unmark):
        raise error.Abort(
            _("no files or directories specified"),
            hint=_("use --all to re-merge all unresolved files"),
        )

    if show:
        ui.pager("resolve")
        fm = ui.formatter("resolve", opts)
        ms = mergemod.mergestate.read(repo)
        m = scmutil.match(repo[None], pats, opts)

        # Labels and keys based on merge state.  Unresolved path conflicts show
        # as 'P'.  Resolved path conflicts show as 'R', the same as normal
        # resolved conflicts.
        mergestateinfo = {
            "u": ("resolve.unresolved", "U"),
            "r": ("resolve.resolved", "R"),
            "pu": ("resolve.unresolved", "P"),
            "pr": ("resolve.resolved", "R"),
            "d": ("resolve.driverresolved", "D"),
        }

        cwd = "" if rootrel else repo.getcwd()

        for f in ms:
            if not m(f):
                continue

            label, key = mergestateinfo[ms[f]]
            fm.startitem()
            fm.condwrite(not nostatus, "status", "%s ", key, label=label)
            # User-friendly paths
            f = repo.pathto(f, cwd)
            fm.write("path", "%s\n", f, label=label)
        fm.end()
        return 0

    with repo.wlock():
        ms = mergemod.mergestate.read(repo)

        if not (ms.active() or repo.dirstate.p2() != nullid):
            raise error.Abort(_("resolve command not applicable when not merging"))

        wctx = repo[None]

        if ms.mergedriver and ms.mdstate() == "u":
            proceed = mergemod.driverpreprocess(repo, ms, wctx)
            ms.commit()
            # allow mark and unmark to go through
            if not mark and not unmark and not proceed:
                return 1

        m = scmutil.match(wctx, pats, opts)
        ret = 0
        didwork = False
        runconclude = False

        tocomplete = []
        for f in ms:
            if not m(f):
                continue

            didwork = True

            # don't let driver-resolved files be marked, and run the conclude
            # step if asked to resolve
            if ms[f] == "d":
                exact = m.exact(f)
                if mark:
                    if exact:
                        ui.warn(_("not marking %s as it is driver-resolved\n") % f)
                elif unmark:
                    if exact:
                        ui.warn(_("not unmarking %s as it is driver-resolved\n") % f)
                else:
                    runconclude = True
                continue

            # path conflicts must be resolved manually
            if ms[f] in ("pu", "pr"):
                if mark:
                    ms.mark(f, "pr")
                elif unmark:
                    ms.mark(f, "pu")
                elif ms[f] == "pu":
                    ui.warn(_("%s: path conflict must be resolved manually\n") % f)
                continue

            if mark:
                ms.mark(f, "r")
            elif unmark:
                ms.mark(f, "u")
            else:
                # backup pre-resolve (merge uses .orig for its own purposes)
                a = repo.wjoin(f)
                try:
                    util.copyfile(a, a + ".resolve")
                except (IOError, OSError) as inst:
                    if inst.errno != errno.ENOENT:
                        raise

                try:
                    # preresolve file
                    ui.setconfig("ui", "forcemerge", opts.get("tool", ""), "resolve")
                    complete, r = ms.preresolve(f, wctx)
                    if not complete:
                        tocomplete.append(f)
                    elif r:
                        ret = 1
                finally:
                    ui.setconfig("ui", "forcemerge", "", "resolve")
                    ms.commit()

                # replace filemerge's .orig file with our resolve file, but only
                # for merges that are complete
                if complete:
                    try:
                        util.rename(a + ".resolve", scmutil.origpath(ui, repo, a))
                    except OSError as inst:
                        if inst.errno != errno.ENOENT:
                            raise

        for f in tocomplete:
            try:
                # resolve file
                ui.setconfig("ui", "forcemerge", opts.get("tool", ""), "resolve")
                r = ms.resolve(f, wctx)
                if r:
                    ret = 1
            finally:
                ui.setconfig("ui", "forcemerge", "", "resolve")
                ms.commit()

            # replace filemerge's .orig file with our resolve file
            a = repo.wjoin(f)
            try:
                util.rename(a + ".resolve", scmutil.origpath(ui, repo, a))
            except OSError as inst:
                if inst.errno != errno.ENOENT:
                    raise

        ms.commit()
        ms.recordactions()

        if not didwork and pats:
            hint = None
            if not any([p for p in pats if p.find(":") >= 0]):
                pats = ["path:%s" % p for p in pats]
                m = scmutil.match(wctx, pats, opts)
                for f in ms:
                    if not m(f):
                        continue
                    flags = "".join(["-%s " % o[0] for o in flaglist if opts.get(o)])
                    hint = _("(try: @prog@ resolve %s%s)\n") % (flags, " ".join(pats))
                    break
            ui.warn(_("arguments do not match paths that need resolving\n"))
            if hint:
                ui.warn(hint)
        elif ms.mergedriver and ms.mdstate() != "s":
            # run conclude step when either a driver-resolved file is requested
            # or there are no driver-resolved files
            # we can't use 'ret' to determine whether any files are unresolved
            # because we might not have tried to resolve some
            if (runconclude or not list(ms.driverresolved())) and not list(
                ms.unresolved()
            ):
                proceed = mergemod.driverconclude(repo, ms, wctx)
                ms.commit()
                ms.recordactions()
                if not proceed:
                    return 1
                # conclude may re-mark files unresolved, so report them
                if mergemod.recountunresolvedafterconclude(ui) and list(
                    ms.unresolved()
                ):
                    ret = 1

    # Nudge users into finishing an unfinished operation
    unresolvedf = list(ms.unresolved())
    driverresolvedf = list(ms.driverresolved())
    if not unresolvedf and not driverresolvedf:
        ui.status(_("(no more unresolved files)\n"))
        cmdutil.checkafterresolved(repo)
    elif not unresolvedf:
        ui.status(
            _('(no more unresolved files -- run "@prog@ resolve --all" to conclude)\n')
        )

    return ret


@command(
    "revert",
    [
        ("a", "all", None, _("revert all changes when no arguments given")),
        ("d", "date", "", _("tipmost revision matching date"), _("DATE")),
        ("r", "rev", "", _("revert to the specified revision"), _("REV")),
        ("C", "no-backup", None, _("do not save backup copies of files")),
        ("i", "interactive", None, _("interactively select the changes")),
    ]
    + walkopts
    + dryrunopts,
    _("[OPTION]... [-r REV] [NAME]..."),
    legacyaliases=["reve", "rever", "rev"],
)
def revert(ui, repo, *pats, **opts):
    """change the specified files to match a commit

    With no revision specified, restore the contents of files to an
    unmodified state and unschedule adds, removes, copies, and renames.
    In other words, revert the specified files or directories to the
    contents they had in the current commit. If you are in the middle of
    an unfinished merge state, you must explicitly specify a revision.

    Use the ``-r/--rev`` option to revert the given files or directories to
    their states as of a specific commit. Because revert does not actually
    check out the specified commit, the files appear as modified and show
    up as pending changes in :prog:`status`.

    Revert causes files to match their contents in another commit. If
    instead you want to undo a specific landed commit, use :prog:`backout`
    instead. Run :prog:`help backout` for more information.

    Modified files are saved with an .orig suffix before reverting.
    To disable these backups, use ``--no-backup``. You can configure @Product@
    to store these backup files in a custom directory relative to the root
    of the repository by setting the ``ui.origbackuppath`` configuration
    option.

    Returns 0 on success.
    """

    if opts.get("date"):
        if opts.get("rev"):
            raise error.Abort(_("you can't specify a revision and a date"))
        opts["rev"] = hex(cmdutil.finddate(ui, repo, opts["date"]))

    parent, p2 = repo.dirstate.parents()
    if not opts.get("rev") and p2 != nullid:
        # revert after merge is a trap for new users (issue2915)
        raise error.Abort(
            _("uncommitted merge with no revision specified"),
            hint=_("use '@prog@ goto' or see '@prog@ help revert'"),
        )

    ctx = scmutil.revsingle(repo, opts.get("rev"))

    if not (
        pats
        or opts.get("include")
        or opts.get("exclude")
        or opts.get("all")
        or opts.get("interactive")
    ):
        msg = _("no files or directories specified")
        if p2 != nullid:
            hint = _(
                "uncommitted merge, use --all to discard all changes,"
                " or '@prog@ goto -C .' to abort the merge"
            )
            raise error.Abort(msg, hint=hint)
        dirty = any(repo.status())
        node = ctx.node()
        if node != parent:
            if dirty:
                hint = (
                    _(
                        "uncommitted changes, use --all to discard all"
                        " changes, or '@prog@ goto %s' to update"
                    )
                    % ctx
                )
            else:
                hint = (
                    _("use --all to revert all files, or '@prog@ goto %s' to update")
                    % ctx
                )
        elif dirty:
            hint = _("uncommitted changes, use --all to discard all changes")
        else:
            hint = _("use --all to revert all files")
        raise error.Abort(msg, hint=hint)

    return cmdutil.revert(ui, repo, ctx, (parent, p2), *pats, **opts)


@command("rollback", dryrunopts + [("f", "force", False, _("ignore safety measures"))])
def rollback(ui, repo, **opts):
    """roll back the last transaction (DANGEROUS) (DEPRECATED)

    Please use :prog:`commit --amend` instead of rollback to correct
    mistakes in the last commit.

    This command should be used with care. There is only one level of
    rollback, and there is no way to undo a rollback. It will also
    restore the dirstate at the time of the last transaction, losing
    any dirstate changes since that time. This command does not alter
    the working directory.

    Transactions are used to encapsulate the effects of all commands
    that create new commits or propagate existing commits into a
    repository.

    .. container:: verbose

      For example, the following commands are transactional, and their
      effects can be rolled back:

      - commit
      - import
      - pull
      - push (with this repository as the destination)
      - unbundle

      To avoid permanent data loss, rollback will refuse to rollback a
      commit transaction if it isn't checked out. Use --force to
      override this protection.

      The rollback command can be entirely disabled by setting the
      ``ui.rollback`` configuration setting to false. If you're here
      because you want to use rollback and it's disabled, you can
      re-enable the command by setting ``ui.rollback`` to true.

    This command is not intended for use on public repositories. Once
    changes are visible for pull by other users, rolling a transaction
    back locally is ineffective (someone else may already have pulled
    the changes). Furthermore, a race is possible with readers of the
    repository; for example an in-progress pull from the repository
    may fail if a rollback is performed.

    Returns 0 on success, 1 if no rollback data is available.
    """
    raise error.Abort(_("rollback is dangerous and should not be used"))


@command(
    "serve",
    [
        ("A", "accesslog", "", _("name of access log file to write to"), _("FILE")),
        ("d", "daemon", None, _("run server in background")),
        ("", "daemon-postexec", [], _("used internally by daemon mode")),
        ("E", "errorlog", "", _("name of error log file to write to"), _("FILE")),
        # use string type, then we can check if something was passed
        ("p", "port", "", _("port to listen on (default: 8000)"), _("PORT")),
        (
            "a",
            "address",
            "",
            _("address to listen on (default: all interfaces)"),
            _("ADDR"),
        ),
        (
            "",
            "prefix",
            "",
            _("prefix path to serve from (default: server root)"),
            _("PREFIX"),
        ),
        (
            "n",
            "name",
            "",
            _("name to show in web pages (default: working directory)"),
            _("NAME"),
        ),
        ("", "pid-file", "", _("name of file to write process ID to"), _("FILE")),
        (
            "",
            "port-file",
            "",
            _("name of file to write port to (useful with '-p 0')"),
            _("FILE"),
        ),
        ("", "stdio", None, _("for remote clients (ADVANCED)")),
        ("", "cmdserver", "", _("for remote clients (ADVANCED)"), _("MODE")),
        ("t", "templates", "", _("web templates to use"), _("TEMPLATE")),
        ("", "style", "", _("template style to use"), _("STYLE")),
        ("6", "ipv6", None, _("use IPv6 in addition to IPv4")),
        ("", "certificate", "", _("SSL certificate file"), _("FILE")),
    ],
    _("[OPTION]..."),
    optionalrepo=True,
)
def serve(ui, repo, **opts):
    """start stand-alone webserver

    Start a local HTTP repository browser and pull server. You can use
    this for ad-hoc sharing and browsing of repositories. It is
    recommended to use a real web server to serve a repository for
    longer periods of time.

    Please note that the server does not implement access control.
    This means that, by default, anybody can read from the server and
    nobody can write to it by default. Set the ``web.allow-push``
    option to ``*`` to allow everybody to push to the server. You
    should use a real web server if you need to authenticate users.

    By default, the server logs accesses to stdout and errors to
    stderr. Use the -A/--accesslog and -E/--errorlog options to log to
    files.

    To have the server choose a free port number to listen on, specify
    a port number of 0; in this case, the server will print the port
    number it uses.

    Returns 0 on success.
    """

    if opts["stdio"] and opts["cmdserver"]:
        raise error.Abort(_("cannot use --stdio with --cmdserver"))

    if opts["stdio"]:
        if repo is None:
            raise error.RepoError(
                _("there is no @Product@ repository here (.hg not found)")
            )
        s = sshserver.sshserver(ui, repo)
        s.serve_forever()

    service = server.createservice(ui, repo, opts)
    return server.runservice(opts, initfn=service.init, runfn=service.run)


@command(
    "show",
    [
        (
            "",
            "nodates",
            None,
            _("omit dates from diff headers (but keeps it in commit header)"),
        ),
        ("", "noprefix", None, _("omit a/ and b/ prefixes from filenames")),
        ("", "stat", None, _("output diffstat-style summary of changes")),
        ("g", "git", None, _("use git extended diff format")),
        ("U", "unified", 3, _("number of lines of diff context to show")),
        ("r", "rev", [], _("show the specified revision")),
        (
            "t",
            "mutation",
            False,
            _("use mutation history for diffs (EXPERIMENTAL)"),
        ),
    ]
    + diffwsopts
    + templateopts
    + walkopts,
    _("[OPTION]... [-r REV | REV] [FILE]..."),
    inferrepo=True,
    cmdtype=readonly,
)
def show(ui, repo, *args, **opts):
    """show commit in detail

    Show the commit message and contents for the specified commit. If no commit
    is specified, shows the current commit.

    The revision can be given positionally or via ``-r/--rev``:

    - ``@prog@ show REV [FILE]...`` — first positional is the revision, the rest
      are files.
    - ``@prog@ show -r REV [FILE]...`` — all positionals are files.

    A bare ``@prog@ show FILE`` does not work, because ``FILE`` is parsed as a
    revision.

    :prog:`show` behaves similarly to :prog:`log -vp -r REV [OPTION]... [FILE]...`, or
    if called without a ``REV``, :prog:`log -vp -r . [OPTION]...` Use
    :prog:`log` for more powerful operations than supported by :prog:`show`.

    """
    ui.pager("show")
    if opts.get("rev"):
        pats = args
    elif len(args) == 0:
        opts["rev"] = ["."]
        pats = []
    else:
        opts["rev"] = [args[0]]
        pats = args[1:]
        if not scmutil.revrange(repo, opts["rev"]):
            h = _("if %s is a file, try `@prog@ show . %s`") % (args[0], args[0])
            raise error.Abort(_("unknown revision %s") % args[0], hint=h)

    opts["patch"] = not opts["stat"]
    opts["verbose"] = True

    # Show full commit message.
    overrides = {("ui", "verbose"): True}

    logcmd, defaultlogopts = cmdutil.getcmdanddefaultopts("log", table)
    defaultlogopts.update(opts)

    with ui.configoverride(overrides, "show"):
        logcmd(ui, repo, *pats, **defaultlogopts)


@command(
    "status|st",
    [
        ("A", "all", None, _("show status of all files")),
        ("m", "modified", None, _("show only modified files")),
        ("a", "added", None, _("show only added files")),
        ("r", "removed", None, _("show only removed files")),
        ("d", "deleted", None, _("show only deleted (but tracked) files")),
        ("c", "clean", None, _("show only files without changes")),
        ("u", "unknown", None, _("show only unknown (not tracked) files")),
        ("i", "ignored", None, _("show only ignored files")),
        ("n", "no-status", None, _("hide status prefix")),
        ("t", "terse", "", _("show the terse output (EXPERIMENTAL)")),
        ("C", "copies", None, _("show source of copied files")),
        ("0", "print0", None, _("end filenames with NUL, for use with xargs")),
        ("", "rev", [], _("show difference from revision"), _("REV")),
        ("", "change", "", _("list the changed files of a revision"), _("REV")),
    ]
    + walkopts
    + formatteropts,
    inferrepo=True,
    cmdtype=readonly,
    legacyaliases=["sta", "stat", "statu"],
)
def status(ui, repo, *pats, **opts):
    revs = opts.get("rev")
    change = opts.get("change")
    terse = opts.get("terse")

    ui.log(
        "status_info",
        status_mode="python",
    )

    if revs and change:
        msg = _("cannot specify --rev and --change at the same time")
        raise error.Abort(msg)
    elif revs and terse:
        msg = _("cannot use --terse with --rev")
        raise error.Abort(msg)
    elif change:
        node2 = scmutil.revsingle(repo, change, None).node()
        node1 = repo[node2].p1().node()
    else:
        node1, node2 = scmutil.revpair(repo, revs)

    if pats or ui.configbool("commands", "status.relative"):
        cwd = repo.getcwd()
    else:
        cwd = ""

    if opts.get("print0"):
        end = "\0"
    else:
        end = "\n"
    copy = {}
    states = "modified added removed deleted unknown ignored clean".split()
    show = [k for k in states if opts.get(k)]
    if opts.get("all"):
        show += ui.quiet and (states[:4] + ["clean"]) or states

    if not show:
        if ui.quiet:
            show = states[:4]
        else:
            show = states[:5]

    m = scmutil.match(repo[node2], pats, opts)
    if terse:
        # we need to compute clean and unknown to terse
        stat = repo.status(
            node1, node2, m, "ignored" in show or "i" in terse, True, True
        )

        stat = cmdutil.tersedir(stat, terse)
    else:
        stat = repo.status(
            node1, node2, m, "ignored" in show, "clean" in show, "unknown" in show
        )

    changestates = zip(states, iter("MAR!?IC"), stat)

    if (
        opts.get("all") or opts.get("copies") or ui.configbool("ui", "statuscopies")
    ) and not opts.get("no_status"):
        with repo.ui.configoverride(
            {("copytrace", "fallback-to-content-similarity"): False},
            "status",
        ):
            copy = copies.pathcopies(repo[node1], repo[node2], m)

    ui.pager("status")
    fm = ui.formatter("status", opts)
    fmt = "%s" + end
    showchar = not opts.get("no_status")

    for state, char, files in changestates:
        if state in show:
            label = "status." + state
            for f in files:
                fm.startitem()
                fm.templatedata(repo=repo)
                fm.condwrite(showchar, "status", "%s ", char, label=label)
                fm.write("path", fmt, repo.pathto(f, cwd), label=label)
                if f in copy:
                    fm.write(
                        "copy",
                        "  %s" + end,
                        repo.pathto(copy[f], cwd),
                        label="status.copied",
                    )

    fm.end()


@command(
    "summary|sum|su|summ|summa|summar",
    [("", "remote", None, _("check for push and pull"))],
    "[--remote]",
    cmdtype=readonly,
)
def summary(ui, repo, **opts):
    """summarize working directory state

    This generates a brief summary of the working directory state,
    including parents, branch, commit status, phase and available updates.

    With the --remote option, this will check the default paths for
    incoming and outgoing changes. This can be time-consuming.

    Returns 0 on success.
    """
    if not util.istest():
        ui.deprecate(
            _("@prog@-summary"),
            _("summary is deprecated - use `@prog@ sl` and `@prog@ status` instead"),
        )

    ui.pager("summary")
    ctx = repo[None]
    parents = ctx.parents()
    marks = []

    ms = None
    try:
        ms = mergemod.mergestate.read(repo)
    except error.UnsupportedMergeRecords as e:
        s = " ".join(e.recordtypes)
        ui.warn(_("warning: merge state has unsupported record types: %s\n") % s)
        unresolved = []
    else:
        unresolved = list(ms.unresolved())

    for p in parents:
        # label with log.changeset (instead of log.parent) since this
        # shows a working directory parent *changeset*:
        # i18n: column positioning for "hg summary"
        ui.write(_("parent: %s ") % (p), label=cmdutil._changesetlabels(p))
        if p.bookmarks():
            marks.extend(p.bookmarks())
        if p.rev() == -1:
            if not len(repo):
                ui.write(_(" (empty repository)"))
            else:
                ui.write(_(" (no revision checked out)"))
        if p.obsolete():
            ui.write(_(" (obsolete)"))
        ui.write("\n")
        if p.description():
            ui.status(
                " " + p.description().splitlines()[0].strip() + "\n",
                label="log.summary",
            )

    if marks:
        active = repo._activebookmark
        # i18n: column positioning for "hg summary"
        ui.write(_("bookmarks:"), label="log.bookmark")
        if active is not None:
            if active in marks:
                ui.write(" *" + active, label=bookmarks.activebookmarklabel)
                marks.remove(active)
            else:
                ui.write(" [%s]" % active, label=bookmarks.activebookmarklabel)
        for m in marks:
            ui.write(" " + m, label="log.bookmark")
        ui.write("\n", label="log.bookmark")

    status = repo.status(unknown=True)

    c = repo.dirstate.copies()
    copied, renamed = [], []
    for d, s in c.items():
        if s in status.removed:
            status.removed.remove(s)
            renamed.append(d)
        else:
            copied.append(d)
        if d in status.added:
            status.added.remove(d)

    labels = [
        (ui.label(_("%d modified"), "status.modified"), status.modified),
        (ui.label(_("%d added"), "status.added"), status.added),
        (ui.label(_("%d removed"), "status.removed"), status.removed),
        (ui.label(_("%d renamed"), "status.copied"), renamed),
        (ui.label(_("%d copied"), "status.copied"), copied),
        (ui.label(_("%d deleted"), "status.deleted"), status.deleted),
        (ui.label(_("%d unknown"), "status.unknown"), status.unknown),
        (ui.label(_("%d unresolved"), "resolve.unresolved"), unresolved),
    ]
    t = []
    for l, s in labels:
        if s:
            t.append(l % len(s))

    t = ", ".join(t)
    cleanworkdir = False

    if repo.localvfs.exists("graftstate"):
        t += _(" (graft in progress)")
    if repo.localvfs.exists("updatestate"):
        t += _(" (interrupted update)")
    elif len(parents) > 1:
        t += _(" (merge)")
    elif not (status.modified or status.added or status.removed or renamed or copied):
        t += _(" (clean)")
        cleanworkdir = True

    if parents:
        pendingphase = max(p.phase() for p in parents)
    else:
        pendingphase = phases.public

    if pendingphase > phases.newcommitphase(ui):
        t += " (%s)" % phases.phasenames[pendingphase]

    if cleanworkdir:
        # i18n: column positioning for "hg summary"
        ui.status(_("commit: %s\n") % t.strip())
    else:
        # i18n: column positioning for "hg summary"
        ui.write(_("commit: %s\n") % t.strip())

    t = []
    draft = len(repo.revs("draft()"))
    if draft:
        t.append(_("%d draft") % draft)
    secret = len(repo.revs("secret()"))
    if secret:
        t.append(_("%d secret") % secret)

    if draft or secret:
        ui.status(_("phases: %s\n") % ", ".join(t))

    cmdutil.summaryhooks(ui, repo)

    if opts.get("remote"):
        needsincoming, needsoutgoing = True, True
    else:
        needsincoming, needsoutgoing = False, False
        for i, o in cmdutil.summaryremotehooks(ui, repo, opts, None):
            if i:
                needsincoming = True
            if o:
                needsoutgoing = True
        if not needsincoming and not needsoutgoing:
            return

    def getincoming():
        source = hg.parseurl(ui.expandpath("default"))
        try:
            other = hg.peer(repo, {}, source)
        except error.RepoError:
            if opts.get("remote"):
                raise
            return source, None, None, None
        ui.debug("comparing with %s\n" % util.hidepassword(source))
        with repo.ui.configoverride({("ui", "quiet"): True}):
            commoninc = discovery.findcommonincoming(repo, other)
        return source, other, commoninc, commoninc[1]

    if needsincoming:
        source, sother, commoninc, incoming = getincoming()
    else:
        source = sother = commoninc = incoming = None

    def getoutgoing():
        dest = hg.parseurl(ui.expandpath("default-push", "default"))
        if source != dest:
            try:
                dother = hg.peer(repo, {}, dest)
            except error.RepoError:
                if opts.get("remote"):
                    raise
                return dest, None, None
            ui.debug("comparing with %s\n" % util.hidepassword(dest))
        elif sother is None:
            # there is no explicit destination peer, but source one is invalid
            return dest, None, None
        else:
            dother = sother
        if source != dest:
            common = None
        else:
            common = commoninc
        with repo.ui.configoverride({("ui", "quiet"): True}):
            outgoing = discovery.findcommonoutgoing(repo, dother, commoninc=common)
        return dest, dother, outgoing

    if needsoutgoing:
        dest, dother, outgoing = getoutgoing()
    else:
        dest = dother = outgoing = None

    if opts.get("remote"):
        t = []
        if incoming:
            t.append(_("1 or more incoming"))
        o = outgoing.missing
        if o:
            t.append(_("%d outgoing") % len(o))
        other = dother or sother
        if "bookmarks" in other.listkeys("namespaces"):
            counts = bookmarks.summary(repo, other)
            if counts[0] > 0:
                t.append(_("%d incoming bookmarks") % counts[0])
            if counts[1] > 0:
                t.append(_("%d outgoing bookmarks") % counts[1])

        if t:
            # i18n: column positioning for "hg summary"
            ui.write(_("remote: %s\n") % (", ".join(t)))
        else:
            # i18n: column positioning for "hg summary"
            ui.status(_("remote: (synced)\n"))

    cmdutil.summaryremotehooks(
        ui,
        repo,
        opts,
        ((source, None, sother, commoninc), (dest, None, dother, outgoing)),
    )


@command(
    "tag",
    [
        ("f", "force", None, _("force tag")),
        ("l", "local", None, _("make the tag local")),
        ("r", "rev", "", _("revision to tag"), _("REV")),
        ("", "remove", None, _("remove a tag")),
        # -l/--local is already there, commitopts cannot be used
        ("e", "edit", None, _("invoke editor on commit messages")),
        ("m", "message", "", _("use text as commit message"), _("TEXT")),
    ]
    + commitopts2,
    _("[OPTION]... [-r REV] NAME..."),
)
def tag(ui, repo, name1, *names, **opts):
    """add one or more tags for the current or given revision

    This command is deprecated.
    """
    if not util.istest():
        ui.deprecate("hg-tag", "tag is deprecated")
    ui.warn(_("error: the tag command has been deprecated - it is now a no-op\n"))
    return 1


@command("tags", formatteropts, "", cmdtype=readonly)
def tags(ui, repo, **opts):
    """list repository tags

    This command is deprecated.
    """
    if not util.istest():
        ui.deprecate("hg-tags", "tags is deprecated")
    ui.warn(_("error: the tags command has been deprecated - it is now a no-op\n"))
    return 1


@command(
    "tip",
    [
        ("p", "patch", None, _("show patch")),
        ("g", "git", None, _("use git extended diff format")),
    ]
    + templateopts,
    _("[OPTION]..."),
    legacyaliases=["ti"],
)
def tip(ui, repo, **opts):
    """show the tip revision (DEPRECATED)

    The tip revision (usually just called the tip) is the changeset
    most recently added to the repository (and therefore the most
    recently changed head).

    If you have just made a commit, that commit will be the tip. If
    you have just pulled changes from another repository, the tip of
    that repository becomes the current tip. The "tip" tag is special
    and cannot be renamed or assigned to a different changeset.

    This command is deprecated, please use :prog:`heads` instead.

    Returns 0 on success.
    """
    if not util.istest():
        ui.deprecate("hg-tip", "tip is deprecated")
    displayer = cmdutil.show_changeset(ui, repo, opts)
    displayer.show(repo["tip"])
    displayer.close()


@command(
    "unbundle",
    [
        (
            "u",
            "update",
            None,
            _("update to new branch head if changesets were unbundled"),
        )
    ],
    _("[-u] FILE..."),
    legacyaliases=["unb", "unbu", "unbun", "unbund", "unbundl"],
)
def unbundle(ui, repo, fname1, *fnames, **opts):
    """apply one or more bundle files

    Apply one or more bundle files generated by :prog:`bundle`.

    Returns 0 on success, 1 if an update has unresolved files.
    """
    fnames = (fname1,) + fnames

    if git.isgitstore(repo):
        newheads = []
        for fname in fnames:
            newheads += git.unbundle(repo, fname)
        if newheads and opts.get("update"):
            hg.update(repo, newheads[-1])
        return 0

    with repo.wlock(), repo.lock():
        for fname in fnames:
            try:
                _ensurebaserev(ui, repo, fname)

                f = hg.openpath(ui, fname)
                gen = exchange.readbundle(ui, f, fname)

                url = "bundle:" + fname
                txnname = "unbundle"
                if not isinstance(gen, bundle2.unbundle20):
                    txnname = "unbundle\n%s" % util.hidepassword(url)
                with repo.transaction(txnname) as tr:
                    op = bundle2.applybundle(repo, gen, tr, source="unbundle", url=url)
            except error.BundleUnknownFeatureError as exc:
                raise error.Abort(
                    _("%s: unknown bundle feature, %s") % (fname, exc),
                    hint=_(
                        "see https://mercurial-scm.org/"
                        "wiki/BundleFeature for more "
                        "information"
                    ),
                )
            modheads = bundle2.combinechangegroupresults(op)

    return postincoming(ui, repo, modheads, opts.get(r"update"), None, None)


def _ensurebaserev(ui, repo, fname):
    """ensure that the repo has the necessary base commits for the given bundle
    file"""
    f = hg.openpath(ui, fname)
    gen = exchange.readbundle(ui, f, fname)
    # Bundle 1 not supported
    if not isinstance(gen, bundle2.unbundle20):
        return

    nodes = set()
    contained = set()
    with bundle2.partiterator(repo, None, gen) as parts:
        for part in parts:
            if part.type == "changegroup":
                unpackerversion = part.params.get("version", "01")
                unpacker = changegroup.getunbundler(unpackerversion, part, None)
                for deltadata in unpacker.deltaiter():
                    node, p1, p2, cs, deltabase, delta, flags = deltadata
                    contained.add(node)
                    nodes.add(node)
                    nodes.add(p1)
                    nodes.add(p2)
    basenodes = nodes - contained
    basenodes.discard(nullid)
    missingnodes = list(repo.changelog.filternodes(list(basenodes), inverse=True))
    if missingnodes:
        ui.status(
            _("pulling missing base commits: %s\n")
            % (", ".join(hex(n) for n in missingnodes))
        )
        # First pull everything, so the subsequent pull doesn't accidentally
        # bring in public commits as draft.
        pull(ui, repo)
        repo.pull(headnodes=missingnodes)


@command(
    "goto|go||up|update|co|checkout",
    [
        ("C", "clean", None, _("discard uncommitted changes (no backup)")),
        ("c", "check", None, _("require clean working copy")),
        ("m", "merge", None, _("merge uncommitted changes")),
        ("d", "date", "", _("tipmost revision matching date (ADVANCED)"), _("DATE")),
        ("r", "rev", "", _("revision"), _("REV")),
        ("", "inactive", None, _("update without activating bookmarks")),
        ("", "continue", None, _("resume interrupted update --merge (ADVANCED)")),
    ]
    + mergetoolopts,
    legacyaliases=[
        "upd",
        "upda",
        "updat",  # typos:ignore-line
        "che",
        "chec",
        "check",
        "checko",
        "checkou",
    ],
    legacyname="update",
)
def update(
    ui,
    repo,
    node=None,
    rev=None,
    clean=False,
    date=None,
    check=False,
    merge=None,
    tool=None,
    inactive=None,
    **opts,
):
    ui.log("checkout_info", checkout_mode="python")

    def abort_or_reset_mergestate():
        if repo.localvfs.exists("updatemergestate"):
            ms = mergemod.mergestate.read(repo)
            cmdutil.abort_on_unresolved_conflicts(ms)
            repo.localvfs.tryunlink("updatemergestate")
            ms.reset()

    if opts.get("continue"):
        with repo.wlock():
            if repo.localvfs.exists("updatemergestate"):
                abort_or_reset_mergestate()
                return 0
            elif repo.localvfs.exists("updatestate") and (
                repo.ui.configbool("experimental", "nativecheckout")
                or repo.ui.configbool("clone", "nativecheckout")
            ):
                if rev or node:
                    raise error.Abort(
                        _("cannot specify --continue and a update revision")
                    )

                rev = repo.localvfs.readutf8("updatestate")
                repo.ui.warn(_("continuing checkout to '%s'\n") % rev)
            else:
                raise error.Abort(_("not in an interrupted update state"))
    else:
        # proactively clean this up if we aren't continuing
        repo.localvfs.tryunlink("updatestate")

    if rev is not None and rev != "" and node is not None:
        raise error.Abort(_("please specify just one revision"))

    if rev is None or rev == "":
        rev = node

    if date and rev is not None:
        raise error.Abort(_("you can't specify a revision and a date"))

    if len([x for x in (clean, check, merge) if x]) > 1:
        raise error.Abort(
            _("can only specify one of -C/--clean, -c/--check, or -m/--merge")
        )

    if node is None and rev is None and not date:
        raise error.Abort(
            _(
                "You must specify a destination to update to,"
                + ' for example "@prog@ goto main".'
            ),
            hint=_(
                "If you're trying to move a bookmark forward, try "
                + '"@prog@ rebase -d <destination>".'
            ),
        )

    if not opts.get("continue"):
        bookmarks.checkagentpreferredtarget(repo, rev, "goto")

    # Suggest `hg prev` as an alternative to 'hg update .^'.
    # internal config: ui.suggesthgprev
    if node == ".^" and ui.configbool("ui", "suggesthgprev", False):
        hintutil.trigger("update-prev")

    updatecheck = None
    if check:
        updatecheck = "abort"
    elif merge:
        updatecheck = "none"

    with repo.wlock():
        # Don't delete the "updatemergestate" marker if we have conflicts.
        if clean:
            repo.localvfs.tryunlink("updatemergestate")
        else:
            abort_or_reset_mergestate()

        # Either we consumed this with "--continue" or are replacing it with
        # a different destination.
        repo.localvfs.tryunlink("updatestate")

        cmdutil.checkunfinished(repo, op="goto_clean" if clean else None)

        if date:
            rev = hex(cmdutil.finddate(ui, repo, date))

        if inactive:
            brev = None
        else:
            brev = rev
        rev = scmutil.revsingle(repo, rev, rev).rev()
        if not opts.get("continue"):
            rewriteutil.gotocheck(repo, [repo[rev]])

        repo.ui.setconfig("ui", "forcemerge", tool, "update")

        if merge:
            # Write down a state so we know how to continue after resolving
            # conflicts.
            repo.localvfs.writeutf8("updatemergestate", "")

        result = hg.updatetotally(
            ui, repo, rev, brev, clean=clean, updatecheck=updatecheck
        )

        if (
            ui.configbool("checkout", "show-destination", True)
            and not ui.plain()
            and not ui.quiet
        ):
            destctx = repo[rev]
            title = next(iter(destctx.description().splitlines()), "")
            ui.write(_('checked out %s "%s"\n') % (short(destctx.node()), title))

        if merge:
            mergemod.try_conclude_merge_state(repo)

        return result


@command(
    "verify",
    [
        (
            "r",
            "rev",
            [],
            _("verify the specified revision or revset (DEPRECATED)"),
            _("REV"),
        ),
        ("", "dag", False, _("perform slower commit graph checks with server")),
    ],
)
def verify(ui, repo, **opts):
    """verify the integrity of the repository

    This command is a no-op.
    """
    from .. import verify

    ret = 0
    if "lazychangelog" in repo.storerequirements:
        ret |= verify.checklazychangelog(repo)
        if ret == 0 and opts.get("dag"):
            ret |= verify.checklazychangelogwithserver(repo)
        else:
            ui.write_err(_("(pass --dag to perform slow checks with server)\n"))
    else:
        ui.status(_("warning: verify does not actually check anything in this repo\n"))
    return ret


@command(
    "version|vers",
    [] + formatteropts,
    norepo=True,
    cmdtype=readonly,
    legacyaliases=["versi", "versio"],
)
def version_(ui, **opts):
    if ui.verbose:
        ui.pager("version")
    fm = ui.formatter("version", opts)
    fm.startitem()
    fm.write("ver", _("@LongProduct@ (version %s)\n"), util.version())
    license = _(
        "(see https://mercurial-scm.org for more information)\n"
        "\nCopyright (C) 2005-2017 Olivia Mackall and others\n"
        "This is free software; see the source for copying conditions. "
        "There is NO\nwarranty; "
        "not even for MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.\n"
    )
    if not ui.quiet:
        fm.plain(license)

    if ui.verbose:
        fm.plain(_("\nEnabled extensions:\n\n"))
    # format names and versions into columns
    names = []
    vers = []
    isinternals = []
    for name, module in extensions.extensions():
        names.append(name)
        vers.append(extensions.moduleversion(module) or None)
        isinternals.append(extensions.ismoduleinternal(module))
    fn = fm.nested("extensions")
    if names:
        namefmt = "  %%-%ds  " % max(len(n) for n in names)
        places = [_("external"), _("internal")]
        for n, v, p in zip(names, vers, isinternals):
            fn.startitem()
            fn.condwrite(ui.verbose, "name", namefmt, n)
            if ui.verbose:
                fn.plain("%s  " % places[p])
            fn.data(bundled=p)
            fn.condwrite(ui.verbose and v, "ver", "%s", v)
            if ui.verbose:
                fn.plain("\n")
    fn.end()
    fm.end()


def loadcmdtable(ui, name, cmdtable):
    """Load command functions from specified cmdtable"""
    overrides = [cmd for cmd in cmdtable if cmd in table]
    if overrides:
        ui.warn(
            _("extension '%s' overrides commands: %s\n") % (name, " ".join(overrides))
        )
    table.update(cmdtable)
