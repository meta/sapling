# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# cmdutil.py - help for command processing in mercurial
#
# Copyright 2005-2007 Olivia Mackall <olivia@selenic.com>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.


import errno
import io
import itertools
import os
import re
import stat
import tempfile
import typing
from enum import Enum
from typing import Any, Dict, List, Optional, Set, Tuple

import bindings
from bindings import renderdag
from sapling import tracing

from . import (
    bookmarks,
    changelog,
    context,
    copies,
    crecord as crecordmod,
    dagop,
    dirstateguard,
    edenfs,
    encoding,
    error,
    formatter,
    git,
    graphmod,
    hintutil,
    identity,
    json,
    match as matchmod,
    mdiff,
    merge as mergemod,
    mergeutil,
    mutation,
    patch,
    pathutil,
    perftrace,
    progress,
    registrar,
    revlog,
    revsetlang,
    scmutil,
    smartset,
    templatekw,
    templater,
    util,
    vfs as vfsmod,
    wcoverlay,
)
from .i18n import _, _x
from .node import bin, hex, nullid, nullrev, short
from .utils import iterutil, pathaclutil, subtreeutil

if typing.TYPE_CHECKING:
    from .ui import ui
    from .uiconfig import uiconfig


# templates of common command options


def _typedflags(flags):
    return flags


dryrunopts = [("n", "dry-run", None, _("do not perform actions, just print output"))]

walkopts = _typedflags(
    [
        (
            "I",
            "include",
            [],
            _("include files matching the given patterns"),
            _("PATTERN"),
        ),
        (
            "X",
            "exclude",
            [],
            _("exclude files matching the given patterns"),
            _("PATTERN"),
        ),
    ]
)

commitopts = [
    ("m", "message", [], _("use text as commit message"), _("TEXT")),
    ("l", "logfile", "", _("read commit message from file"), _("FILE")),
]

commitopts2 = [
    ("d", "date", "", _("record the specified date as commit date"), _("DATE")),
    ("u", "user", "", _("record the specified user as committer"), _("USER")),
]

messagefieldopts = [
    (
        "",
        "message-field",
        [],
        _(
            "rewrite fields of commit message (e.g. --message-field=Summary='New Summary' to update or --message-field=-Summary to remove) (EXPERIMENTAL)"
        ),
        _("fieldname=fieldvalue"),
    ),
]

# hidden for now
formatteropts = _typedflags(
    [("T", "template", "", _("display with template (EXPERIMENTAL)"), _("TEMPLATE"))]
)

templateopts = _typedflags(
    [
        (
            "",
            "style",
            "",
            _("display using template map file (DEPRECATED)"),
            _("STYLE"),
        ),
        ("T", "template", "", _("display with template"), _("TEMPLATE")),
    ]
)

logopts = (
    _typedflags(
        [
            ("p", "patch", None, _("show patch")),
            ("g", "git", None, _("use git extended diff format")),
            ("l", "limit", "", _("limit number of changes displayed"), _("NUM")),
            ("M", "no-merges", None, _("do not show merges")),
            ("", "stat", None, _("output diffstat-style summary of changes")),
            ("G", "graph", None, _("show the revision DAG")),
        ]
    )
    + templateopts
)

diffopts = [
    ("a", "text", None, _("treat all files as text")),
    ("g", "git", None, _("use git extended diff format")),
    ("", "binary", None, _("generate binary diffs in git mode (default)")),
    ("", "nodates", None, _("omit dates from diff headers")),
]

untrackedfileopts = _typedflags(
    [
        (
            "",
            "max-untracked-size",
            "",
            _(
                "filter out any untracked files larger than this size, in megabytes (DEPRECATED)"
            ),
            _("MAX_SIZE"),
        ),
        (
            "",
            "max-untracked-size-bytes",
            "",
            _("filter out any untracked files larger than this size, in bytes"),
            _("MAX_SIZE_BYTES"),
        ),
    ]
)

diffwsopts = _typedflags(
    [
        ("w", "ignore-all-space", None, _("ignore white space when comparing lines")),
        (
            "b",
            "ignore-space-change",
            None,
            _("ignore changes in the amount of white space"),
        ),
        (
            "B",
            "ignore-blank-lines",
            None,
            _("ignore changes whose lines are all blank"),
        ),
        ("Z", "ignore-space-at-eol", None, _("ignore changes in whitespace at EOL")),
    ]
)

diffopts2 = (
    _typedflags(
        [
            ("", "noprefix", None, _("omit a/ and b/ prefixes from filenames")),
            ("p", "show-function", None, _("show which function each change is in")),
            ("", "reverse", None, _("produce a diff that undoes the changes")),
        ]
    )
    + diffwsopts
    + _typedflags(
        [
            ("U", "unified", "", _("number of lines of context to show"), _("NUM")),
            ("", "stat", None, _("output diffstat-style summary of changes")),
            ("", "root", "", _("produce diffs relative to subdirectory"), _("DIR")),
            (
                "",
                "only-files-in-revs",
                None,
                _("only show changes for files modified in the requested revisions"),
            ),
        ]
    )
)

mergetoolopts = [("t", "tool", "", _("specify merge tool"))]

similarityopts = [
    (
        "s",
        "similarity",
        "",
        _("guess renamed files by similarity (0<=s<=100)"),
        _("SIMILARITY"),
    )
]

debugrevlogopts = [
    ("c", "changelog", False, _("open changelog")),
    ("m", "manifest", False, _("open manifest")),
    ("", "dir", "", _("open directory manifest")),
]

subtree_path_opts = [
    (
        "",
        "from-path",
        [],
        _("the path of source directory or file"),
        _("PATH"),
    ),
    (
        "",
        "to-path",
        [],
        _("the path of dest directory or file"),
        _("PATH"),
    ),
]


def ishunk(x):
    hunkclasses = (crecordmod.uihunk, patch.recordhunk)
    return isinstance(x, hunkclasses)


def newandmodified(chunks, originalchunks):
    newlyaddedandmodifiedfiles = set()
    for chunk in chunks:
        if ishunk(chunk) and chunk.header.isnewfile() and chunk not in originalchunks:
            newlyaddedandmodifiedfiles.add(chunk.header.filename())
    return newlyaddedandmodifiedfiles


def extractcopies(chunks) -> "Dict[str, str]":
    result = {}
    for chunk in chunks:
        if ishunk(chunk):
            copyfrom = chunk.header.copyfrom()
            if copyfrom:
                copyto = chunk.header.filename()
                result[copyto] = copyfrom
    return result


def comparechunks(chunks, headers):
    """
    Determine whether the sets of chunks is the same as the original set of
    headers, after they had been filtered.

    Generate patches for both sets of data and then compare the patches.
    """

    originalpatch = io.BytesIO()
    for header in headers:
        header.write(originalpatch)
        for hunk in header.hunks:
            hunk.write(originalpatch)

    newpatch = io.BytesIO()
    for chunk in chunks:
        chunk.write(newpatch)

    return newpatch.getvalue() == originalpatch.getvalue()


class AliasList(list):
    """Like "list" but provides a "documented" API to hide "undocumented" aliases"""

    _raw = ""

    def documented(self):
        """documented aliases"""
        return self._raw.lstrip("^").split("||", 1)[0].split("|")


def parsealiases(cmd):
    aliases = AliasList(s for s in cmd.lstrip("^").split("|") if s)
    aliases._raw = cmd
    return aliases


def setupwrapcolorwrite(ui):
    # wrap ui.write so diff output can be labeled/colorized
    def wrapwritebytes(orig, *args, **kw):
        label = kw.pop(r"label", "")
        for chunk, l in patch.difflabel(lambda: args):
            orig(chunk, label=label + l)

    oldwrite = ui.writebytes

    def wrap(*args, **kwargs):
        return wrapwritebytes(oldwrite, *args, **kwargs)

    setattr(ui, "writebytes", wrap)
    return oldwrite


def filterchunks(ui, originalhunks, interface, testfile, operation=None):
    if interface == "curses":
        if testfile:
            recordfn = crecordmod.testdecorator(testfile, crecordmod.testchunkselector)
        else:
            recordfn = crecordmod.chunkselector

        return crecordmod.filterpatch(ui, originalhunks, recordfn, operation)
    if interface == "repl":
        from . import repl_record

        return crecordmod.filterpatch(
            ui, originalhunks, repl_record.chunkselector, operation
        )
    return patch.filterpatch(ui, originalhunks, operation)


def recordfilter(ui, originalhunks, operation=None):
    """Prompts the user to filter the originalhunks and return a list of
    selected hunks.
    *operation* is used for to build ui messages to indicate the user what
    kind of filtering they are doing: reverting, committing, shelving, etc.
    (see patch.filterpatch).
    """
    interface = ui.interface("chunkselector")
    if interface == "curses" and not crecordmod.checkcurses(ui):
        interface = "text"
    testfile = ui.config("experimental", "crecordtest")
    oldwrite = None
    if interface != "repl":
        oldwrite = setupwrapcolorwrite(ui)
    try:
        newchunks, newopts = filterchunks(
            ui, originalhunks, interface, testfile, operation
        )
    finally:
        if oldwrite is not None:
            ui.writebytes = oldwrite
    return newchunks, newopts


def dorecord(
    ui,
    repo,
    commitfunc,
    cmdsuggest,
    backupall,
    filterfn,
    *pats,
    inmemory=False,
    **opts,
):
    """Select changes with ``filterfn`` and pass them to ``commitfunc``.

    With ``inmemory``, ``commitfunc`` sees the selected content through a
    working copy overlay and the files on disk are left untouched. Otherwise
    the working copy is temporarily rewritten to the selected content.
    """
    if not ui.interactive():
        if cmdsuggest:
            msg = _("running non-interactively, use %s instead") % cmdsuggest
        else:
            msg = _("running non-interactively")
        raise error.Abort(msg)

    # make sure username is set before going interactive
    if not opts.get("user"):
        ui.username()  # raise exception, username not provided

    def recordfunc(ui, repo, message, match, opts):
        """This is generic record driver.

        Its job is to interactively filter local changes, and
        accordingly prepare working directory into a state in which the
        job can be delegated to a non-interactive commit command such as
        'commit' or 'qrefresh'.

        After the actual job is done by non-interactive command, the
        working directory is restored to its original state.

        In the end we'll record interesting changes, and everything else
        will be left in place, so the user can continue working.
        """

        checkunfinished(repo, op="commit")
        wctx = repo[None]
        merge = len(wctx.parents()) > 1
        if merge:
            raise error.Abort(
                _('cannot partially commit a merge (use "@prog@ commit" instead)')
            )

        def fail(f, msg):
            raise error.Abort("%s: %s" % (f, msg))

        force = opts.get("force")
        if not force:
            match.bad = fail

        status = repo.status(match=match)
        if not force:
            repo.checkcommitpatterns(wctx, match, status, fail)
        diffopts = patch.difffeatureopts(ui, opts=opts, whitespace=True)
        diffopts.nodates = True
        diffopts.git = True
        diffopts.showfunc = True
        originaldiff = patch.diff(
            repo, repo[repo.dirstate.p1()], repo[None], changes=status, opts=diffopts
        )
        originalchunks = patch.parsepatch(originaldiff)

        # 1. filter patch, since we are intending to apply subset of it
        try:
            chunks, newopts = filterfn(ui, originalchunks)
        except error.PatchError as err:
            raise error.Abort(_("error parsing patch: %s") % err)
        opts.update(newopts)

        # We need to keep a backup of files that have been newly added and
        # modified during the recording process because there is a previous
        # version without the edit in the workdir
        newlyaddedandmodifiedfiles = newandmodified(chunks, originalchunks)
        contenders = set()
        for h in chunks:
            try:
                contenders.update(set(h.files()))
            except AttributeError:
                pass
        changed = status.modified + status.added + status.removed
        newfiles = [f for f in changed if f in contenders]
        if not newfiles:
            ui.status(_("no changes to record\n"))
            return 0

        if inmemory and ui.configbool("record", "inmemory", True):
            ui.metrics.inc("record.inmemory", 1)
            return _commitselectedinmemory(ui, repo, commitfunc, chunks, newfiles, opts)
        ui.metrics.inc("record.workingcopy", 1)

        modified = set(status.modified)

        # 2. backup changed files, so we can restore them in the end

        if backupall:
            tobackup = changed
        else:
            tobackup = [
                f for f in newfiles if f in modified or f in newlyaddedandmodifiedfiles
            ]
        copied = extractcopies(chunks)
        tobackup += sorted(copied.keys())  # backup "copyto" - delete by step 3a
        tobackup += sorted(copied.values())  # backup "copyfrom" - rewrite by step 3a
        backups = {}
        if tobackup:
            backupdir = repo.localvfs.join("record-backups")
            try:
                os.mkdir(backupdir)
            except OSError as err:
                if err.errno != errno.EEXIST:
                    raise
        try:
            # backup continues
            for f in tobackup:
                if not repo.wvfs.exists(f):
                    continue
                fd, tmpname = tempfile.mkstemp(dir=backupdir)
                os.close(fd)
                ui.debug("backup %r as %r\n" % (f, tmpname))
                util.copyfile(repo.wjoin(f), tmpname, copystat=True)
                backups[f] = tmpname

            fp = io.BytesIO()
            for c in chunks:
                if c.filename() in backups:
                    c.write(fp)
            dopatch = fp.tell()
            fp.seek(0)

            # 2.5 optionally review / modify patch in text editor
            if opts.get("review", False):
                patchtext = (
                    crecordmod.diffhelptext
                    + crecordmod.patchhelptext
                    + fp.read().decode()
                )
                reviewedpatch = ui.edit(
                    patchtext, "", action="diff", repopath=repo.path
                )
                fp.truncate(0)
                fp.write(reviewedpatch.encode())
                fp.seek(0)

            [os.unlink(repo.wjoin(c)) for c in newlyaddedandmodifiedfiles]

            # 3a. Prepare "copyfrom" -> "copyto" files. Write "copyfrom"
            # and remove "copyto".
            # This is used by patch._applydiff. If _applydiff reads directly
            # from repo["."], not repo.wvfs, then this could be unnecessary.
            for copyto, copyfrom in copied.items():
                content = repo["."][copyfrom].data()
                repo.wvfs.write(copyfrom, content)
                repo.wvfs.tryunlink(copyto)

            # 3b. apply filtered patch to clean repo  (clean)
            if backups:
                m = scmutil.matchfiles(repo, backups.keys())
                revert(
                    ui,
                    repo,
                    repo["."],
                    repo.dirstate.parents(),
                    match=m,
                    no_backup=True,
                )

            # 3c. (apply)
            if dopatch:
                try:
                    ui.debug("applying patch\n")
                    patch.internalpatch(ui, repo, fp, 1, eolmode=None)
                except error.PatchError as err:
                    raise error.Abort(str(err))
            del fp

            # 4. We prepared working directory according to filtered
            #    patch. Now is the time to delegate the job to
            #    commit/qrefresh or the like!

            # Make all of the pathnames absolute.
            newfiles = [repo.wjoin(nf) for nf in newfiles]
            return commitfunc(ui, repo, *newfiles, **opts)
        finally:
            # 5. finally restore backed-up files
            try:
                dirstate = repo.dirstate
                for realname, tmpname in backups.items():
                    ui.debug("restoring %r to %r\n" % (tmpname, realname))

                    if dirstate[realname] == "n":
                        # without normallookup, restoring timestamp
                        # may cause partially committed files
                        # to be treated as unmodified
                        dirstate.normallookup(realname)

                    # copystat=True here and above are a hack to trick any
                    # editors that have f open that we haven't modified them.
                    #
                    # Also note that this racy as an editor could notice the
                    # file's mtime before we've finished writing it.
                    try:
                        util.copyfile(tmpname, repo.wjoin(realname), copystat=True)
                    except Exception as ex:
                        ui.warn(
                            _("error restoring %s to %s: %s\n")
                            % (tmpname, realname, ex)
                        )
                        continue

                    os.unlink(tmpname)

                # Don't clean up the backup dir if we failed to restore files.
                if tobackup and not os.listdir(backupdir):
                    os.rmdir(backupdir)
            except OSError:
                pass

    def recordinwlock(ui, repo, message, match, opts):
        with repo.wlock():
            return recordfunc(ui, repo, message, match, opts)

    return commit(ui, repo, recordinwlock, pats, opts)


def _commitselectedinmemory(ui, repo, commitfunc, chunks, newfiles, opts):
    """Run ``commitfunc`` with the selected chunks overlaid on the working copy"""
    fp = io.BytesIO()
    for c in chunks:
        c.write(fp)
    patchdata = fp.getvalue()

    if opts.get("review", False):
        patchtext = (
            crecordmod.diffhelptext + crecordmod.patchhelptext + patchdata.decode()
        )
        patchdata = ui.edit(patchtext, "", action="diff", repopath=repo.path).encode()

    overlay = selectedcontents(ui, repo, patchdata, newfiles)
    with wcoverlay.applied(repo, overlay):
        return commitfunc(ui, repo, *[repo.wjoin(f) for f in newfiles], **opts)


def selectedcontents(ui, repo, patchdata, paths):
    """Apply a patch to the working copy parent in memory.

    Return {path: (data, flags)} for each of ``paths`` that exists after the
    patch. Paths removed by the patch are omitted.
    """
    p1 = repo["."]

    def p1filectx(_repo, _memctx, path):
        return p1[path] if path in p1 else None

    mctx = context.memctx(repo, [p1], "", [], p1filectx)
    store = patch.mempatchstore(mctx)
    backend = patch.mempatchbackend(ui, mctx, store)
    try:
        ret = patch.applydiff(ui, io.BytesIO(patchdata), backend, store)
    except error.PatchError as err:
        raise error.Abort(str(err)) from err
    if ret < 0:
        # Only a hunk edited during selection can fail to apply.
        raise error.Abort(
            _("selected changes do not apply to %s") % ", ".join(backend.rejs),
            hint=_(
                "an edited hunk no longer matches the file; select it again "
                "without editing, or keep its context lines unchanged"
            ),
        )

    contents = {}
    for path in paths:
        fctx = mctx[path]
        if fctx is not None:
            contents[path] = (fctx.data(), fctx.flags())
    return contents


class dirnode:
    """
    Represent a directory in user working copy with information required for
    the purpose of tersing its status.

    path is the path to the directory

    statuses is a set of statuses of all files in this directory (this includes
    all the files in all the subdirectories too)

    files is a list of files which are direct child of this directory

    subdirs is a dictionary of sub-directory name as the key and it's own
    dirnode object as the value
    """

    def __init__(self, dirpath):
        self.path = dirpath
        self.statuses = set([])
        self.files = []
        self.subdirs = {}

    def _addfileindir(self, filename, status):
        """Add a file in this directory as a direct child."""
        self.files.append((filename, status))

    def addfile(self, filename, status):
        """
        Add a file to this directory or to its direct parent directory.

        If the file is not direct child of this directory, we traverse to the
        directory of which this file is a direct child of and add the file
        there.
        """

        # the filename contains a path separator, it means it's not the direct
        # child of this directory
        if "/" in filename:
            subdir, filep = filename.split("/", 1)

            # does the dirnode object for subdir exists
            if subdir not in self.subdirs:
                subdirpath = os.path.join(self.path, subdir)
                self.subdirs[subdir] = dirnode(subdirpath)

            # try adding the file in subdir
            self.subdirs[subdir].addfile(filep, status)

        else:
            self._addfileindir(filename, status)

        if status not in self.statuses:
            self.statuses.add(status)

    def iterfilepaths(self):
        """Yield (status, path) for files directly under this directory."""
        for f, st in self.files:
            yield st, os.path.join(self.path, f)

    def tersewalk(self, terseargs):
        """
        Yield (status, path) obtained by processing the status of this
        dirnode.

        terseargs is the string of arguments passed by the user with `--terse`
        flag.

        Following are the cases which can happen:

        1) All the files in the directory (including all the files in its
        subdirectories) share the same status and the user has asked us to terse
        that status. -> yield (status, dirpath)

        2) Otherwise, we do following:

                a) Yield (status, filepath)  for all the files which are in this
                    directory (only the ones in this directory, not the subdirs)

                b) Recurse the function on all the subdirectories of this
                   directory
        """

        if len(self.statuses) == 1:
            onlyst = self.statuses.pop()

            # Making sure we terse only when the status abbreviation is
            # passed as terse argument
            if onlyst in terseargs:
                yield onlyst, self.path + os.sep
                return

        # add the files to status list
        for st, fpath in self.iterfilepaths():
            yield st, fpath

        # recurse on the subdirs
        for dirobj in self.subdirs.values():
            for st, fpath in dirobj.tersewalk(terseargs):
                yield st, fpath


def tersedir(statuslist, terseargs):
    """
    Terse the status if all the files in a directory shares the same status.

    statuslist is scmutil.status() object which contains a list of files for
    each status.
    terseargs is string which is passed by the user as the argument to `--terse`
    flag.

    The function makes a tree of objects of dirnode class, and at each node it
    stores the information required to know whether we can terse a certain
    directory or not.
    """
    # the order matters here as that is used to produce final list
    allst = ("m", "a", "r", "d", "u", "i", "c")

    # checking the argument validity
    for s in str(terseargs):
        if s not in allst:
            raise error.Abort(_("'%s' not recognized") % s)

    # creating a dirnode object for the root of the repo
    rootobj = dirnode("")
    pstatus = ("modified", "added", "deleted", "clean", "unknown", "ignored", "removed")

    tersedict = {}
    for attrname in pstatus:
        statuschar = attrname[0:1]
        for f in getattr(statuslist, attrname):
            rootobj.addfile(f, statuschar)
        tersedict[statuschar] = []

    # we won't be tersing the root dir, so add files in it
    for st, fpath in rootobj.iterfilepaths():
        tersedict[st].append(fpath)

    # process each sub-directory and build tersedict
    for subdir in rootobj.subdirs.values():
        for st, f in subdir.tersewalk(terseargs):
            tersedict[st].append(f)

    tersedlist = []
    for st in allst:
        tersedict[st].sort()
        tersedlist.append(tersedict[st])

    return tersedlist


def findpossible(cmd, table):
    """
    Return cmd -> (aliases, command table entry)
    for each matching command.
    Return debug commands (or their aliases) only if no normal command matches.
    """
    choice = {}
    debugchoice = {}

    if cmd in table:
        # short-circuit exact matches, "log" alias beats "^log|history"
        keys = [cmd]
    else:
        keys = table.keys()

    allcmds = []
    for e in keys:
        aliases = parsealiases(e)
        allcmds.extend(aliases)
        found = None
        if cmd in aliases:
            found = cmd
        if found is not None:
            if aliases[0].startswith("debug") or found.startswith("debug"):
                debugchoice[found] = (aliases, table[e])
            else:
                choice[found] = (aliases, table[e])

    if not choice and debugchoice:
        choice = debugchoice

    return choice, allcmds


def getcmdanddefaultopts(cmdname, table):
    """Returns (command, defaultopts) for cmd string
    This function returns command and all the default options already
    initialized. It should be used by commands that call other commands. For
    example, calling pull inside of update, calling log inside of show etc.
    getcmdanddefaultopts has important benefits:
    1) It returns "wrapped" command i.e. command with all the overrides applied.
    This is better than calling commands.pull() directly.
    2) getcmdanddefaultopts correctly initializes options to their default value
    and correctly changes their name - replace '-'  with '_'.
    """

    cmdname = cmdname.split()
    for subcmd in cmdname:
        _aliases, cmdwithopts = findcmd(subcmd, table)
        table = cmdwithopts[0].subcommands

    cmd, optsdescription = cmdwithopts[:2]

    opts = {}
    for opt in optsdescription:
        name = opt[1].replace("-", "_")
        value = opt[2]
        opts[name] = value

    return (cmd, opts)


def findcmd(cmd, table):
    """Return (aliases, command table entry) for command string."""
    choice, allcmds = findpossible(cmd, table)

    if cmd in choice:
        return choice[cmd]

    if len(choice) > 1:
        clist = sorted(choice)
        raise error.AmbiguousCommand(cmd, clist)

    if choice:
        return list(choice.values())[0]

    raise error.UnknownCommand(cmd)


def findsubcmd(args, table, partial=False):
    cmd, args, level = args[0], args[1:], 1
    aliases, entry = findcmd(cmd, table)
    cmd = aliases[0]
    while args and entry[0] and hasattr(entry[0], "subcommands"):
        try:
            subaliases, subentry = findcmd(args[0], entry[0].subcommands)
        except error.UnknownCommand as e:
            if entry[0].subonly:
                raise error.UnknownSubcommand(cmd, *e.args)
            else:
                break
        else:
            aliases, entry = subaliases, subentry
            cmd, args, level = "%s %s" % (cmd, aliases[0]), args[1:], level + 1
    if not partial and hasattr(entry[0], "subonly") and entry[0].subonly:
        raise error.UnknownSubcommand(cmd, None, None)
    return cmd, args, aliases, entry, level


def findrepo(p):
    root = identity.sniffroot(p)
    if root:
        return root[0]

    return None


def uncommittedchanges(repo):
    """Returns if there are uncommitted changes"""
    modified, added, removed, deleted = repo.status()[:4]
    return modified or added or removed or deleted


def bailifchanged(repo, merge=True, hint=None):
    """enforce the precondition that working directory must be clean.

    'merge' can be set to false if a pending uncommitted merge should be
    ignored (such as when 'update --check' runs).

    'hint' is the usual hint given to Abort exception.
    """
    if merge and repo.dirstate.p2() != nullid:
        raise error.Abort(_("outstanding uncommitted merge"), hint=hint)

    if uncommittedchanges(repo):
        raise error.UncommittedChangesAbort(_("uncommitted changes"), hint=hint)


def logmessage(repo, opts):
    """get the log message according to -m and -l option"""
    ui = repo.ui

    # Allow the commit message from another commit to be reused.
    reuserev = opts.get("reuse_message")
    if reuserev:
        incompatibleopts = ["message", "logfile"]
        currentinvaliopts = [opt for opt in incompatibleopts if opts.get(opt)]
        if currentinvaliopts:
            raise error.Abort(
                _("--reuse-message and --%s are mutually exclusive")
                % (currentinvaliopts[0])
            )
        opts["message"] = scmutil.revsingle(repo, reuserev).description()
        opts["reuse_message"] = False

    message = opts.get("message")
    logfile = opts.get("logfile")
    # Typed -m values are lists. Commands that fill in an existing message
    # (amend, graft, shelve, --reuse-message) pass a str, which is not checked.
    typed = isinstance(message, list) or bool(logfile)

    if message and logfile:
        raise error.Abort(_("options --message and --logfile are mutually exclusive"))
    if isinstance(message, list):
        message = "\n\n".join(message)
    if not message and logfile:
        try:
            if isstdiofilename(logfile):
                message = ui.fin.read().decode()
            else:
                message = b"\n".join(util.readfile(logfile).splitlines()).decode()
        except IOError as inst:
            raise error.Abort(
                _("can't read commit message '%s': %s") % (logfile, inst.strerror)
            )
    if typed and message:
        _checkescapednewlines(ui, message)

    message = _update_commit_message_fields(
        message,
        ui.configlist("committemplate", "commit-message-fields"),
        opts,
    )

    return message


def _checkescapednewlines(ui, message):
    r"""Abort agents and warn humans if the title has a typed "\n" before a field.

    The "\n" is stored as two characters, so the whole message becomes the
    title. Only the title is checked: a body may quote "\n" on purpose.
    """
    title = message.lstrip().split("\n", 1)[0]
    fields = ui.configlist("committemplate", "commit-message-fields")
    if "\\n" not in title or not fields:
        return
    if not ui.configbool("commit", "check-escaped-newlines", True):
        return
    match = re.search(
        r"\\n\s*(%s):" % "|".join(re.escape(field) for field in fields),
        title,
        re.IGNORECASE,
    )
    if not match:
        return
    if ui.plain():
        ui.metrics.inc("commit.escapednewline.automation_allowed", 1)
        return

    msg = _("commit message title contains a '\\n' escape sequence before '%s:'") % (
        match.group(1)
    )
    hint = _(
        "use literal newlines instead of '\\n' escape sequences, "
        "which @Product@ does not interpret"
    )
    if bindings.agentdetect.is_agent():
        ui.metrics.inc("commit.escapednewline.agent_rejected", 1)
        raise error.Abort(msg, hint=hint)
    ui.metrics.inc("commit.escapednewline.human_warned", 1)
    ui.warn(_("warning: %s\n(%s)\n") % (msg, hint))


def _update_commit_message_fields(
    message: str, configured_fields: List[str], opts
) -> str:
    """Update fields in message based on opts["message_fields"].

    >>> _update_commit_message_fields("title\\nSummary: summary.", ["Summary"], {})
    'title\\nSummary: summary.'

    >>> _update_commit_message_fields("title\\nSummary: summary.", ["Summary"], {"message_field": ["Summary=\\nnew summary."]})
    'title\\nSummary:\\nnew summary.'

    >>> _update_commit_message_fields("old title\\nSummary:\\nOld summary.\\nReviewer: foo", ["Summary", "Test Plan", "Reviewer"], {"message_field": ["Title=new title", "Test Plan=new test plan"]})
    'new title\\nSummary:\\nOld summary.\\nTest Plan: new test plan\\nReviewer: foo'

    >>> _update_commit_message_fields("old title\\nSummary:\\nOld summary.\\nReviewer: foo", ["Summary", "Test Plan", "Reviewer"], {"message_field": ["-Summary"]})
    'old title\\nReviewer: foo'
    """
    fields = opts.get("message_field") or []
    if not fields:
        return message

    fields_to_update = {}
    for field in fields:
        remove = False
        if field and field[0] == "-":
            remove = True
            field = field[1:]

        parts = field.split("=", 1)
        if len(parts) == 1 != remove:
            raise error.Abort(
                _("--message-field format is name=value or -name to remove")
            )

        if remove:
            fields_to_update[field] = None
            continue

        name, value = parts
        if name != "Title":
            if value and value[0] != "\n":
                value = " " + value
            # Format value to include the section name. This is consistent with what
            # _parse_commit_message() returns.
            value = f"{name}:{value}"
        fields_to_update[name] = value

    parsed_message = _parse_commit_message(message.split("\n"), set(configured_fields))

    # Pretend like title has a field name of "Title" so it is addressable by user.
    if parsed_message and parsed_message[0][0] is None:
        parsed_message[0] = ("Title", parsed_message[0][1])

    updated_lines = []
    for field in ["Title"] + configured_fields:
        if parsed_message and parsed_message[0][0] == field:
            parsed_value = parsed_message.pop(0)[1]
            # If user is not updating this field, carry over old value.
            if field not in fields_to_update:
                updated_lines.extend(parsed_value)

        # User is updating (or inserting) this value - stick in the new value.
        if field in fields_to_update:
            value = fields_to_update.pop(field)
            if value is not None:
                updated_lines.extend(value.split("\n"))

    for unused in fields_to_update:
        raise error.Abort(
            _("field name %r not configured in committemplate.commit-message-fields")
            % unused
        )

    return "\n".join(updated_lines)


def mergeeditform(ctxorbool, baseformname):
    """return appropriate editform name (referencing a committemplate)

    'ctxorbool' is either a ctx to be committed, or a bool indicating whether
    merging is committed.

    This returns baseformname with '.merge' appended if it is a merge,
    otherwise '.normal' is appended.
    """
    if isinstance(ctxorbool, bool):
        if ctxorbool:
            return baseformname + ".merge"
    elif 1 < len(ctxorbool.parents()):
        return baseformname + ".merge"

    return baseformname + ".normal"


def getcommiteditor(edit=False, editform="", summaryfooter="", **opts):
    """get appropriate commit message editor according to '--edit' option

    'editform' is a dot-separated list of names, to distinguish
    the purpose of commit text editing.

    'summaryfooter' is a prepopulated message to add extra info to the commit
    summary.
    """
    if edit:
        return lambda r, c: commitforceeditor(
            r,
            c,
            editform=editform,
            summaryfooter=summaryfooter,
        )
    else:
        return lambda r, c: commiteditor(
            r,
            c,
            editform=editform,
            summaryfooter=summaryfooter,
        )


def loglimit(opts):
    """get the log limit according to option -l/--limit"""
    limit = opts.get("limit")
    if limit:
        try:
            limit = int(limit)
        except ValueError:
            raise error.Abort(_("limit must be a positive integer"))
        if limit <= 0:
            raise error.Abort(_("limit must be positive"))
    else:
        limit = None
    return limit


def makefilename(
    repo, pat, node, desc=None, total=None, seqno=None, revwidth=None, pathname=None
):
    node_expander = {
        "H": lambda: hex(node),
        "R": lambda: "%d" % repo.changelog.rev(node),
        "h": lambda: short(node),
        "m": lambda: re.sub(r"[^\w]", "_", desc or ""),
    }
    expander = {"%": lambda: "%", "b": lambda: os.path.basename(repo.root)}

    try:
        if node:
            expander.update(node_expander)
        if node:
            expander["r"] = lambda: ("%d" % repo.changelog.rev(node)).zfill(
                revwidth or 0
            )
        if total is not None:
            expander["N"] = lambda: "%d" % total
        if seqno is not None:
            expander["n"] = lambda: "%d" % seqno
        if total is not None and seqno is not None:
            expander["n"] = lambda: ("%d" % seqno).zfill(len("%d" % total))
        if pathname is not None:
            expander["s"] = lambda: os.path.basename(pathname)
            expander["d"] = lambda: os.path.dirname(pathname) or "."
            expander["p"] = lambda: pathname

        newname = []
        patlen = len(pat)
        i = 0
        while i < patlen:
            c = pat[i : i + 1]
            if c == "%":
                i += 1
                c = pat[i : i + 1]
                c = expander[c]()
            newname.append(c)
            i += 1
        return "".join(newname)
    except KeyError as inst:
        raise error.Abort(
            _("invalid format spec '%%%s' in output filename") % inst.args[0]
        )


def isstdiofilename(pat):
    """True if the given pat looks like a filename denoting stdin/stdout"""
    return not pat or pat == "-"


def rendertemplate(ui, tmpl, props=None):
    """Render tmpl written in the template language. props provides the
    "environment" which the template program runs in.

    Return the rendered string.

    >>> import sapling.ui as uimod
    >>> rendertemplate(uimod.ui(), '{a} {b|json}', {'a': 'x', 'b': [3, None]})
    'x [3, null]'
    """
    t = formatter.maketemplater(ui, tmpl, cache=templatekw.defaulttempl)
    mapping = {"ui": ui, "templ": t}
    if props:
        if "ctx" in props:
            mapping["revcache"] = {}
        mapping.update(props)
    mapping.update(templatekw.keywords)
    return t.render(mapping)


class _unclosablefile:
    def __init__(self, fp):
        self._fp = fp

    def close(self):
        pass

    def __iter__(self):
        return iter(self._fp)

    def __getattr__(self, attr):
        return getattr(self._fp, attr)

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc_value, exc_tb):
        pass


def makefileobj(
    repo,
    pat,
    node=None,
    desc=None,
    total=None,
    seqno=None,
    revwidth=None,
    mode="wb",
    modemap=None,
    pathname=None,
):
    writable = mode not in ("r", "rb")

    if isstdiofilename(pat):
        if writable:
            fp = repo.ui.fout
        else:
            fp = repo.ui.fin
        return _unclosablefile(fp)
    fn = makefilename(repo, pat, node, desc, total, seqno, revwidth, pathname)
    if modemap is not None:
        mode = modemap.get(fn, mode)
        if mode == "wb":
            modemap[fn] = "ab"
    return open(fn, mode)


def openrevlog(repo, cmd, file_, opts):
    """opens the changelog, manifest, a filelog or a given revlog"""
    cl = opts["changelog"]
    mf = opts["manifest"]
    dir = opts["dir"]
    msg = None
    if cl and mf:
        msg = _("cannot specify --changelog and --manifest at the same time")
    elif cl and dir:
        msg = _("cannot specify --changelog and --dir at the same time")
    elif cl or mf or dir:
        if file_:
            msg = _("cannot specify filename with --changelog or --manifest")
        elif not repo:
            msg = _(
                "cannot specify --changelog or --manifest or --dir without a repository"
            )
    if msg:
        raise error.Abort(msg)

    r = None
    if repo:
        if cl:
            cl = repo.changelog
            r = revlog.revlog(cl.opener, cl.indexfile)
        elif dir:
            if "treemanifest" not in repo.requirements:
                raise error.Abort(
                    _("--dir can only be used on repos with treemanifest enabled")
                )
            dirlog = repo.manifestlog._revlog.dirlog(dir)
            if len(dirlog):
                r = dirlog
        elif mf:
            r = repo.manifestlog._revlog
        elif file_:
            filelog = repo.file(file_)
            if len(filelog):
                r = filelog
    if not r:
        if not file_:
            raise error.CommandError(cmd, _("invalid arguments"))
        if not os.path.isfile(file_):
            raise error.Abort(_("revlog '%s' not found") % file_)
        r = revlog.revlog(vfsmod.vfs(os.getcwd(), audit=False), file_[:-2] + ".i")
    return r


def copy(ui, repo, pats, opts, rename=False):
    # called with the repo lock held
    #
    # hgsep => pathname that uses "/" to separate directories
    # ossep => pathname that uses os.sep to separate directories
    cwd = repo.getcwd()
    targets = {}
    amend = opts.get("amend")
    mark = opts.get("mark") or opts.get("after")
    dryrun = opts.get("dry_run")
    force = opts.get("force")
    wctx = repo[None]
    auditor = pathutil.pathauditor(repo.root)

    if amend and not mark:
        raise error.Abort(_("--amend without --mark is not supported"))

    if amend:
        walkctx = repo["."]
        if rename:
            walkctx = walkctx.p1()
    else:
        walkctx = wctx

    def walkpat(pat):
        srcs = []
        if mark:
            badstates = "?"
        else:
            badstates = "?r"
        m = scmutil.match(walkctx, [pat], opts, globbed=True)
        for abs in walkctx.walk(m):
            rel = m.rel(abs)
            exact = m.exact(abs)
            if amend:
                srcs.append((abs, rel, exact))
                continue
            state = repo.dirstate[abs]
            if state in badstates:
                if exact and state == "?":
                    ui.warn(_("%s: not copying - file is not managed\n") % rel)
                if exact and state == "r":
                    ui.warn(
                        _("%s: not copying - file has been marked for remove\n") % rel
                    )
                continue
            # abs: hgsep
            # rel: ossep
            srcs.append((abs, rel, exact))
        return srcs

    # (without --amend)
    # target exists  --mark   --force|  action
    #       n            n      *    |  copy
    #       n            y      *    |  (1)
    #   untracked        n      n    |  (4) (a)
    #   untracked        n      y    |  (3)
    #   untracked        y      *    |  (2)
    #       y            n      n    |  (4) (b)
    #       y            n      y    |  (3)
    #       y            y      n    |  (2)
    #       y            y      y    |  (3)
    #    deleted         n      n    |  copy
    #    deleted         n      y    |  (3)
    #    deleted         y      n    |  (1)
    #    deleted         y      y    |  (1)
    #
    # * = don't care
    # (1) <src>: not recording move - <target> does not exist
    # (2) preserve target contents
    # (3) replace target contents
    # (4) <target>: not overwriting - file {exists,already committed};
    # (a) with '--mark' hint
    # (b) with '--amend --mark' hint

    # [(abssrc, abstarget)]
    to_amend = []

    # abssrc: hgsep
    # relsrc: ossep
    # otarget: ossep
    def copyfile(abssrc, relsrc, otarget, exact):
        abstarget = pathutil.canonpath(repo.root, cwd, otarget)
        auditor(abstarget)
        if "/" in abstarget:
            # We cannot normalize abstarget itself, this would prevent
            # case only renames, like a => A.
            abspath, absname = abstarget.rsplit("/", 1)
            abstarget = repo.dirstate.normalize(abspath) + "/" + absname
        reltarget = repo.pathto(abstarget, cwd)
        target = repo.wjoin(abstarget)
        src = repo.wjoin(abssrc)
        state = repo.dirstate[abstarget]

        scmutil.checkportable(ui, abstarget)

        # check for collisions
        prevsrc = targets.get(abstarget)
        if prevsrc is not None:
            ui.warn(
                _("%s: not overwriting - %s collides with %s\n")
                % (reltarget, repo.pathto(abssrc, cwd), repo.pathto(prevsrc, cwd))
            )
            return

        # check for overwrites
        exists = os.path.lexists(target)
        samefile = False
        if exists and abssrc != abstarget:
            if repo.dirstate.normalize(abssrc) == repo.dirstate.normalize(abstarget):
                if not rename:
                    ui.warn(_("%s: can't copy - same file\n") % reltarget)
                    return
                exists = False
                samefile = True

        if not mark and exists or (mark and state in "mn" and not amend):
            if not force:
                if state in "mn":
                    msg = _("%s: not overwriting - file already committed\n")
                    if rename:
                        hint = _(
                            "(use '@prog@ rename --amend --mark' to amend the current commit)\n"
                        )
                    else:
                        hint = _(
                            "(use '@prog@ copy --amend --mark' to amend the current commit)\n"
                        )
                else:
                    msg = _("%s: not overwriting - file exists\n")
                    if rename:
                        hint = _("(@prog@ rename --mark to record the rename)\n")
                    else:
                        hint = _("(@prog@ copy --mark to record the copy)\n")
                ui.warn(msg % reltarget)
                ui.warn(hint)
                return

        if mark:
            if not exists:
                if rename:
                    ui.warn(
                        _("%s: not recording move - %s does not exist\n")
                        % (relsrc, reltarget)
                    )
                else:
                    ui.warn(
                        _("%s: not recording copy - %s does not exist\n")
                        % (relsrc, reltarget)
                    )
                return
        elif not dryrun and not amend:
            try:
                if exists:
                    os.unlink(target)
                targetdir = os.path.dirname(target) or "."
                if not os.path.isdir(targetdir):
                    os.makedirs(targetdir)
                if samefile:
                    tmp = target + "~hgrename"
                    os.rename(src, tmp)
                    os.rename(tmp, target)
                else:
                    util.copyfile(src, target)
                srcexists = True
            except IOError as inst:
                if inst.errno == errno.ENOENT:
                    ui.warn(_("%s: deleted in working directory\n") % relsrc)
                    srcexists = False
                else:
                    ui.warn(_("%s: cannot copy - %s\n") % (relsrc, inst.strerror))
                    return True  # report a failure

        if ui.verbose or not exact:
            if rename:
                ui.status(_("moving %s to %s\n") % (relsrc, reltarget))
            else:
                ui.status(_("copying %s to %s\n") % (relsrc, reltarget))

        targets[abstarget] = abssrc

        if amend:
            to_amend.append((abssrc, abstarget))
        else:
            # fix up dirstate
            scmutil.dirstatecopy(
                ui, repo, wctx, abssrc, abstarget, dryrun=dryrun, cwd=cwd
            )
            if rename and not dryrun:
                if not mark and srcexists and not samefile:
                    repo.wvfs.unlinkpath(abssrc)
                wctx.forget([abssrc])

    # pat: ossep
    # dest ossep
    # srcs: list of (hgsep, hgsep, ossep, bool)
    # return: function that takes hgsep and returns ossep
    def targetpathfn(pat, dest, srcs):
        if os.path.isdir(pat):
            abspfx = pathutil.canonpath(repo.root, cwd, pat)
            abspfx = util.localpath(abspfx)
            if destdirexists:
                striplen = len(os.path.split(abspfx)[0])
            else:
                striplen = len(abspfx)
            if striplen:
                striplen += len(os.sep)
            res = lambda p: os.path.join(dest, util.localpath(p)[striplen:])
        elif destdirexists:
            res = lambda p: os.path.join(dest, os.path.basename(util.localpath(p)))
        else:
            res = lambda p: dest
        return res

    # pat: ossep
    # dest ossep
    # srcs: list of (hgsep, hgsep, ossep, bool)
    # return: function that takes hgsep and returns ossep
    def targetpathafterfn(pat, dest, srcs):
        if matchmod.patkind(pat):
            # a mercurial pattern
            res = lambda p: os.path.join(dest, os.path.basename(util.localpath(p)))
        else:
            abspfx = pathutil.canonpath(repo.root, cwd, pat)
            if len(abspfx) < len(srcs[0][0]):
                # A directory. Either the target path contains the last
                # component of the source path or it does not.
                def evalpath(striplen):
                    score = 0
                    for s in srcs:
                        t = os.path.join(dest, util.localpath(s[0])[striplen:])
                        if os.path.lexists(t):
                            score += 1
                    return score

                abspfx = util.localpath(abspfx)
                striplen = len(abspfx)
                if striplen:
                    striplen += len(os.sep)
                if os.path.isdir(os.path.join(dest, os.path.split(abspfx)[1])):
                    score = evalpath(striplen)
                    striplen1 = len(os.path.split(abspfx)[0])
                    if striplen1:
                        striplen1 += len(os.sep)
                    if evalpath(striplen1) > score:
                        striplen = striplen1
                res = lambda p: os.path.join(dest, util.localpath(p)[striplen:])
            else:
                # a file
                if destdirexists:
                    res = lambda p: os.path.join(
                        dest, os.path.basename(util.localpath(p))
                    )
                else:
                    res = lambda p: dest
        return res

    pats = scmutil.expandpats(pats)
    if not pats:
        raise error.Abort(_("no source or destination specified"))
    if len(pats) == 1:
        raise error.Abort(_("no destination specified"))
    dest = pats.pop()
    destdirexists = os.path.isdir(dest) and not os.path.islink(dest)
    if not destdirexists:
        if len(pats) > 1 or matchmod.patkind(pats[0]):
            raise error.Abort(
                _("with multiple sources, destination must be an existing directory")
            )
        if util.endswithsep(dest):
            raise error.Abort(_("destination %s is not a directory") % dest)

    tfn = targetpathfn
    if mark:
        tfn = targetpathafterfn

    currctx = repo["."]
    rootreldest = scmutil.rootrelpath(currctx, dest)
    copylist = []
    opname = "move" if rename else "copy"
    for pat in pats:
        srcs = walkpat(pat)
        if not srcs:
            continue
        rootrelsrcs = [s[0] for s in srcs]
        pathaclutil.validate_files_acl(
            repo, rootrelsrcs, rootreldest, currctx, op_name=opname
        )
        copylist.append((tfn(pat, dest, srcs), srcs))
    if not copylist and not to_amend:
        hint = _("use '--amend --mark' if you want to amend the current commit")
        raise error.Abort(
            _("no files to copy"),
            hint=hint,
        )

    errors = 0
    for targetpath, srcs in copylist:
        for abssrc, relsrc, exact in srcs:
            if copyfile(abssrc, relsrc, targetpath(abssrc), exact):
                errors += 1

    if to_amend:
        amend_copy(repo, to_amend, rename, force)

    if errors and not mark:
        ui.warn(_("(consider using --mark)\n"))

    return errors != 0


def amend_copy(repo, to_amend, rename, force):
    with repo.lock():
        ctx = repo["."]
        pctx = ctx.p1()

        # Checks
        for src_path, dst_path in to_amend:
            if rename:
                if src_path in ctx or src_path not in pctx:
                    raise error.Abort(
                        _("source path '%s' is not deleted by the current commit")
                        % src_path
                    )
            else:
                if src_path not in ctx:
                    raise error.Abort(
                        _("source path '%s' is not present the current commit")
                        % src_path
                    )
            if dst_path not in ctx or dst_path in pctx:
                raise error.Abort(
                    _("target path '%s' is not added by the current commit") % dst_path
                )
            if not force:
                # Check already renamed.
                renamed = ctx[dst_path].renamed()
                if renamed:
                    renamed_path = renamed[0]
                    if renamed_path == src_path:
                        continue
                    raise error.Abort(
                        _("target path '%s' is already marked as copied from '%s'")
                        % (dst_path, renamed_path),
                        hint=_("use --force to skip this check"),
                    )
                # Check similarity. Can be bypassed by --force.
                if rename:
                    src_data = pctx[src_path].data()
                else:
                    src_data = ctx[src_path].data()
                dst_data = ctx[dst_path].data()
                if not bindings.copytrace.is_content_similar(
                    src_data, dst_data, repo.ui._rcfg
                ):
                    raise error.Abort(
                        _("'%s' and '%s' does not look similar") % (src_path, dst_path),
                        hint=_("use --force to skip similarity check"),
                    )

        mctx = context.memctx.mirrorformutation(ctx, "amend")
        for src_path, dst_path in to_amend:
            mctx[dst_path] = context.overlayfilectx(
                mctx[dst_path], copied=(src_path, pctx.node())
            )
        new_node = mctx.commit()
        mapping = {ctx.node(): (new_node,)}
        scmutil.cleanupnodes(repo, mapping, rename and "rename" or "copy")
        repo.dirstate.rebuild(new_node, allfiles=[], changedfiles=[], exact=True)


def uncopy(ui, repo, matcher, opts):
    # called with the repo lock held
    ret = 1  # return 1 if nothing changed
    dryrun = opts.get("dry_run")
    status = repo.status(match=matcher)
    matches = sorted(status.modified + status.added)
    for fname in matches:
        if ui.verbose:
            ui.status(_("uncopying %s") % (fname,))
        if not dryrun:
            ret = 0
            repo.dirstate.copy(None, fname)
    return ret


## facility to let extension process additional data into an import patch
# list of identifier to be executed in order
extrapreimport = []  # run before commit
extrapostimport = []  # run after commit
# mapping from identifier to actual import function
#
# 'preimport' are run before the commit is made and are provided the following
# arguments:
# - repo: the localrepository instance,
# - patchdata: data extracted from patch header (cf m.patch.patchheadermap),
# - extra: the future extra dictionary of the changeset, please mutate it,
# - opts: the import options.
# XXX ideally, we would just pass an ctx ready to be computed, that would allow
# mutation of in memory commit and more. Feel free to rework the code to get
# there.
extrapreimportmap = {}
# 'postimport' are run after the commit is made and are provided the following
# argument:
# - ctx: the changectx created by import.
extrapostimportmap = {}


def tryimportone(ui, repo, hunk, parents, opts, msgs, updatefunc):
    """Utility function used by commands.import to import a single patch

    This function is explicitly defined here to help the evolve extension to
    wrap this part of the import logic.

    The API is currently a bit ugly because it a simple code translation from
    the import command. Feel free to make it better.

    :hunk: a patch (as a binary string)
    :parents: nodes that will be parent of the created commit
    :opts: the full dict of option passed to the import command
    :msgs: list to save commit message to.
           (used in case we need to save it when failing)
    :updatefunc: a function that update a repo to a given node
                 updatefunc(<repo>, <node>)
    """

    extractdata = patch.extract(ui, hunk)
    tmpname = extractdata.get("filename")
    message = extractdata.get("message")
    user = opts.get("user") or extractdata.get("user")
    date = opts.get("date") or extractdata.get("date")
    nodeid = extractdata.get("nodeid")
    p1 = extractdata.get("p1")
    p2 = extractdata.get("p2")

    nocommit = opts.get("no_commit")
    update = not opts.get("bypass")
    strip = opts["strip"]
    prefix = opts["prefix"]
    sim = float(opts.get("similarity") or 0)
    if not tmpname:
        return (None, None, False)

    rejects = False

    try:
        cmdline_message = logmessage(repo, opts)
        if cmdline_message:
            # pickup the cmdline msg
            message = cmdline_message
        elif message:
            # pickup the patch msg
            message = message.strip()
        else:
            # launch the editor
            message = None
        ui.debug("message:\n%s\n" % message)

        if len(parents) == 1:
            parents.append(repo[nullid])
        if opts.get("exact"):
            if not nodeid or not p1:
                raise error.Abort(_("not a @Product@ patch"))
            p1 = repo[p1]
            p2 = repo[p2 or nullid]
        elif p2:
            try:
                p1 = repo[p1]
                p2 = repo[p2]
                # Without any options, consider p2 only if the
                # patch is being applied on top of the recorded
                # first parent.
                if p1 != parents[0]:
                    p1 = parents[0]
                    p2 = repo[nullid]
            except error.RepoError:
                p1, p2 = parents
            if p2.node() == nullid:
                ui.warn(
                    _(
                        "warning: import the patch as a normal revision\n"
                        "(use --exact to import the patch as a merge)\n"
                    )
                )
        else:
            p1, p2 = parents

        n = None
        if update:
            if p1 != parents[0]:
                updatefunc(repo, p1.node())
            if p2 != parents[1]:
                repo.setparents(p1.node(), p2.node())

            partial = opts.get("partial", False)
            files = set()
            try:
                patch.patch(
                    ui,
                    repo,
                    tmpname,
                    strip=strip,
                    prefix=prefix,
                    files=files,
                    eolmode=None,
                    similarity=sim / 100.0,
                )
            except error.PatchError as e:
                if not partial:
                    raise error.Abort(str(e))
                if partial:
                    rejects = True

            files = list(files)
            if nocommit:
                if message:
                    msgs.append(message)
            else:
                if opts.get("exact") or p2:
                    # If you got here, you either use --force and know what
                    # you are doing or used --exact or a merge patch while
                    # being updated to its first parent.
                    m = None
                else:
                    m = scmutil.matchfiles(repo, files or [])
                editform = mergeeditform(repo[None], "import.normal")
                if opts.get("exact"):
                    editor = None
                else:
                    editor = getcommiteditor(editform=editform, **opts)
                extra = {}
                for idfunc in extrapreimport:
                    extrapreimportmap[idfunc](repo, extractdata, extra, opts)
                overrides = {}
                if partial:
                    overrides[("ui", "allowemptycommit")] = True
                with repo.ui.configoverride(overrides, "import"):
                    n = repo.commit(
                        message, user, date, match=m, editor=editor, extra=extra
                    )
                    for idfunc in extrapostimport:
                        extrapostimportmap[idfunc](repo[n])
        else:
            store = patch.filestore()
            try:
                files = set()
                try:
                    patch.patchrepo(
                        ui, repo, p1, store, tmpname, strip, prefix, files, eolmode=None
                    )
                except error.PatchError as e:
                    raise error.Abort(str(e))
                if opts.get("exact"):
                    editor = None
                else:
                    editor = getcommiteditor(editform="import.bypass")
                memctx = context.memctx(
                    repo,
                    (p1, p2),
                    message,
                    files=files,
                    filectxfn=store,
                    user=user,
                    date=date,
                    editor=editor,
                )
                n = memctx.commit()
            finally:
                store.close()
        if opts.get("exact") and nocommit:
            # --exact with --no-commit is still useful in that it does merge
            # and branch bits
            ui.warn(_("warning: can't check exact import with --no-commit\n"))
        elif opts.get("exact") and hex(n) != nodeid:
            # Write the commit out. The "Abort" should not cancel the transaction.
            # This is the behavior tested by test-import-merge.t (issue3616).
            # It's a questionable behavior, though.
            tr = repo.currenttransaction()
            tr.close()
            raise error.Abort(_("patch is damaged or loses information"))
        msg = _("applied to working directory")
        if n:
            # i18n: refers to a short changeset id
            msg = _("created %s") % short(n)
        return (msg, n, rejects)
    finally:
        os.unlink(tmpname)


# facility to let extensions include additional data in an exported patch
# list of identifiers to be executed in order
extraexport = []
# mapping from identifier to actual export function
# function as to return a string to be added to the header or None
# it is given two arguments (sequencenumber, changectx)
extraexportmap = {}


def _exportsingle(
    repo, ctx, match, switch_parent, rev, seqno, write, diffopts, writestr=None
):
    if writestr is None:

        def writestr(s):
            write(s.encode())

    node = scmutil.binnode(ctx)
    parents = [p.node() for p in ctx.parents() if p]
    if switch_parent:
        parents.reverse()

    if parents:
        prev = parents[0]
    else:
        prev = nullid

    writestr(f"# {identity.tmplprefix()} changeset patch\n")
    writestr("# User %s\n" % ctx.user())
    writestr("# Date %d %d\n" % ctx.date())
    writestr("#      %s\n" % util.datestr(ctx.date()))
    writestr("# Node ID %s\n" % hex(node))
    writestr("# Parent  %s\n" % hex(prev))
    if len(parents) > 1:
        writestr("# Parent  %s\n" % hex(parents[1]))

    for headerid in extraexport:
        header = extraexportmap[headerid](seqno, ctx)
        if header is not None:
            writestr("# %s\n" % header)
    writestr(ctx.description().rstrip())
    writestr("\n\n")

    for chunk, label in patch.diffui(
        repo, repo[prev], repo[node], match, opts=diffopts
    ):
        write(chunk, label=label)


def export(
    repo,
    revs,
    fntemplate="hg-%h.patch",
    fp=None,
    switch_parent=False,
    opts=None,
    match=None,
):
    """export changesets as hg patches

    Args:
      repo: The repository from which we're exporting revisions.
      revs: A list of revisions to export as revision numbers.
      fntemplate: An optional string to use for generating patch file names.
      fp: An optional file-like object to which patches should be written.
      switch_parent: If True, show diffs against second parent when not nullid.
                     Default is false, which always shows diff against p1.
      opts: diff options to use for generating the patch.
      match: If specified, only export changes to files matching this matcher.

    Returns:
      Nothing.

    Side Effect:
      "HG Changeset Patch" data is emitted to one of the following
      destinations:
        fp is specified: All revs are written to the specified
                         file-like object.
        fntemplate specified: Each rev is written to a unique file named using
                            the given template.
        Neither fp nor template specified: All revs written to repo.ui.write()
    """

    total = len(revs)
    revwidth = max(len(str(rev)) for rev in revs)
    filemode = {}

    write = None
    writestr = None
    dest = "<unnamed>"
    if fp:
        dest = getattr(fp, "name", dest)
        write = lambda s, **kw: fp.write(s)
    elif not fntemplate:
        write = repo.ui.writebytes
        writestr = repo.ui.write

    for seqno, rev in enumerate(revs, 1):
        ctx = repo[rev]
        fo = None
        if not fp and fntemplate:
            desc_lines = ctx.description().rstrip().split("\n")
            desc = desc_lines[0]  # Commit always has a first line.
            fo = makefileobj(
                repo,
                fntemplate,
                ctx.node(),
                desc=desc,
                total=total,
                seqno=seqno,
                revwidth=revwidth,
                mode="wb",
                modemap=filemode,
            )
            dest = getattr(fo, "name", "<unnamed>")
            write = lambda s, **kw: fo.write(s)

        if not dest.startswith("<"):
            repo.ui.note("%s\n" % dest)
        _exportsingle(
            repo, ctx, match, switch_parent, rev, seqno, write, opts, writestr
        )
        if fo is not None:
            fo.close()


def diffordiffstat(
    ui,
    repo,
    diffopts,
    ctx1,
    ctx2,
    match,
    changes=None,
    stat=False,
    fp=None,
    prefix="",
    root="",
    hunksfilterfn=None,
):
    """show diff or diffstat."""
    if fp is None:
        if stat:
            write = ui.write
        else:
            write = ui.writebytes
    else:

        def write(s, **kw):
            fp.write(s)

    if root:
        relroot = pathutil.canonpath(repo.root, repo.getcwd(), root)
    else:
        relroot = ""
    if relroot != "":
        # XXX relative roots currently don't work if the root is within a
        # subrepo
        uirelroot = match.uipath(relroot)
        relroot += "/"
        for matchroot in match.files():
            if not matchroot.startswith(relroot):
                ui.warn(
                    _("warning: %s not inside relative root %s\n")
                    % (match.uipath(matchroot), uirelroot)
                )

    if stat:
        diffopts = diffopts.copy(context=0, noprefix=False)
        width = 80
        if not ui.plain():
            width = ui.termwidth()
        chunks = patch.diff(
            repo,
            ctx1,
            ctx2,
            match,
            changes,
            opts=diffopts,
            prefix=prefix,
            relroot=relroot,
            hunksfilterfn=hunksfilterfn,
        )
        for chunk, label in patch.diffstatui(util.iterlines(chunks), width=width):
            write(chunk, label=label)
    else:
        for chunk, label in patch.diffui(
            repo,
            ctx1,
            ctx2,
            match,
            changes,
            opts=diffopts,
            prefix=prefix,
            relroot=relroot,
            hunksfilterfn=hunksfilterfn,
        ):
            write(chunk, label=label)


def _changesetlabels(ctx, isxrepo=False):
    labels = ["log.changeset", "changeset.%s" % ctx.phasestr()]
    if ctx.obsolete():
        labels.append("changeset.obsolete")
    if isxrepo:
        labels.append("changeset.xrepo")
    return " ".join(labels)


class changeset_printer:
    """show changeset information when templating not requested."""

    def __init__(self, ui, repo, matchfn, diffopts, buffered):
        self.ui = ui
        self.repo = repo
        self.buffered = buffered
        self.matchfn = matchfn
        self.diffopts = diffopts
        self.header = {}
        self.hunk = {}
        self.lastheader = None
        self.footer = None
        self._columns = templatekw.getlogcolumns()
        self.use_committer_date = ui.configbool("log", "use-committer-date")

    def flush(self, ctx):
        rev = ctx.rev()
        if rev in self.header:
            h = self.header[rev]
            if h != self.lastheader:
                self.lastheader = h
                self.ui.write(h)
            del self.header[rev]
        if rev in self.hunk:
            for elem in self.hunk[rev]:
                if isinstance(elem, str):
                    self.ui.write(elem)
                else:
                    self.ui.writebytes(elem)
            del self.hunk[rev]
            return 1
        return 0

    def close(self):
        if self.footer:
            self.ui.write(self.footer)

    def show(
        self, ctx, revcache=None, matchfn=None, hunksfilterfn=None, prevfn=None, **props
    ):
        props = props
        if revcache is None:
            revcache = {}
        if self.buffered:
            self.ui.pushbuffer(labeled=True)
            self._show(ctx, revcache, matchfn, hunksfilterfn, prevfn, props)
            self.hunk[ctx.rev()] = self.ui.popbufferlist()
        else:
            self._show(ctx, revcache, matchfn, hunksfilterfn, prevfn, props)

    def _show(self, ctx, revcache, matchfn, hunksfilterfn, prevfn, props):
        """show a single changeset or file revision"""
        changenode = ctx.node()
        rev = ctx.rev()

        isxrepo = bool(revcache.get("xreponame"))
        if self.ui.quiet:
            self.ui.write(
                "%s\n" % scmutil.formatchangeid(ctx, isxrepo), label="log.node"
            )
            return

        columns = self._columns
        if changenode:
            self.ui.write(
                columns["changeset"] % scmutil.formatchangeid(ctx, isxrepo),
                label=_changesetlabels(ctx, isxrepo),
            )

        for nsname, ns in self.repo.names.items():
            # we will use the templatename as the color name since those two
            # should be the same
            for name in ns.names(self.repo, changenode):
                self.ui.write(ns.logfmt % name, label="log.%s" % ns.colorname)
        if self.ui.debugflag:
            self.ui.write(columns["phase"] % ctx.phasestr(), label="log.phase")

        if self.ui.debugflag and rev is not None:
            mnode = ctx.manifestnode()
            self.ui.write(
                columns["manifest"] % hex(mnode), label="ui.debug log.manifest"
            )
        self.ui.write(columns["user"] % ctx.user(), label="log.user")
        date = ctx.date()
        if self.use_committer_date:
            if committer_info := git.committer_and_date_from_extras(ctx.extra()):
                date = committer_info[1:]
        self.ui.write(columns["date"] % util.datestr(date), label="log.date")

        if self.ui.debugflag:
            files = ctx.p1().status(ctx)[:3]
            for key, value in zip(["files", "files+", "files-"], files):
                if value:
                    self.ui.write(
                        columns[key] % " ".join(value), label="ui.debug log.files"
                    )
        elif self.ui.verbose and ctx.files():
            self.ui.write(
                columns["files"] % " ".join(ctx.files()), label="ui.note log.files"
            )
        copies = revcache.get("copies")
        if copies and self.ui.verbose:
            copies = ["%s (%s)" % c for c in copies]
            self.ui.write(
                columns["copies"] % " ".join(copies), label="ui.note log.copies"
            )

        extra = ctx.extra()
        if extra and self.ui.debugflag:
            for key, value in sorted(extra.items()):
                self.ui.write(
                    columns["extra"] % (key, util.escapestr(value)),
                    label="ui.debug log.extra",
                )

        description = ctx.description().strip()
        if description:
            if self.ui.verbose:
                self.ui.write(_("description:\n"), label="ui.note log.description")
                self.ui.write(description, label="ui.note log.description")
                self.ui.write("\n\n")
            else:
                self.ui.write(
                    columns["summary"] % description.splitlines()[0],
                    label="log.summary",
                )
        self.ui.write("\n")

        self.showpatch(ctx, matchfn, hunksfilterfn=hunksfilterfn, prevfn=prevfn)

    def showpatch(self, ctx, matchfn, hunksfilterfn=None, prevfn=None):
        if not matchfn:
            matchfn = self.matchfn
        if matchfn:
            stat = self.diffopts.get("stat")
            diff = self.diffopts.get("patch")
            diffopts = patch.diffallopts(self.ui, self.diffopts)
            if prevfn is not None:
                prevctx = prevfn(ctx)
            else:
                prevctx = ctx.p1()
            if stat:
                diffordiffstat(
                    self.ui,
                    self.repo,
                    diffopts,
                    prevctx,
                    ctx,
                    match=matchfn,
                    stat=True,
                    hunksfilterfn=hunksfilterfn,
                )
            if diff:
                if stat:
                    self.ui.write("\n")
                diffordiffstat(
                    self.ui,
                    self.repo,
                    diffopts,
                    prevctx,
                    ctx,
                    match=matchfn,
                    stat=False,
                    hunksfilterfn=hunksfilterfn,
                )
            self.ui.write("\n")


class jsonchangeset(changeset_printer):
    """format changeset information."""

    def __init__(self, ui, repo, matchfn, diffopts, buffered):
        changeset_printer.__init__(self, ui, repo, matchfn, diffopts, buffered)
        self.cache = {}
        self._first = True

    def close(self):
        if not self._first:
            self.ui.write("\n]\n")
        else:
            self.ui.write("[]\n")

    def _show(self, ctx, revcache, matchfn, hunksfilterfn, prevfn, props):
        """show a single changeset or file revision"""
        rev = ctx.rev()
        if rev is None:
            jrev = jnode = "null"
        else:
            jrev = "%d" % scmutil.revf64encode(rev)
            jnode = '"%s"' % hex(ctx.node())
        j = lambda v: json.dumps(v, paranoid=False)

        if self._first:
            self.ui.write("[\n {")
            self._first = False
        else:
            self.ui.write(",\n {")

        if self.ui.quiet:
            self.ui.write(_x('\n  "rev": %s') % jrev)
            self.ui.write(_x(',\n  "node": %s') % jnode)
            self.ui.write("\n }")
            return

        self.ui.write(_x('\n  "rev": %s') % jrev)
        self.ui.write(_x(',\n  "node": %s') % jnode)
        self.ui.write(_x(',\n  "branch": %s') % j("default"))
        self.ui.write(_x(',\n  "phase": "%s"') % ctx.phasestr())
        self.ui.write(_x(',\n  "user": %s') % j(ctx.user()))
        date = ctx.date()
        if author_date := git.author_date_from_extras(ctx.extra()):
            self.ui.write(_x(',\n  "author_date": [%d, %d]') % author_date)
        if committer_info := git.committer_and_date_from_extras(ctx.extra()):
            self.ui.write(_x(',\n  "committer": %s') % j(committer_info[0]))
            self.ui.write(_x(',\n  "committer_date": [%d, %d]') % committer_info[1:])
            if self.use_committer_date:
                date = committer_info[1:]
        self.ui.write(_x(',\n  "date": [%d, %d]') % date)
        self.ui.write(_x(',\n  "desc": %s') % j(ctx.description()))

        self.ui.write(
            _x(',\n  "bookmarks": [%s]')
            % ", ".join("%s" % j(b) for b in ctx.bookmarks())
        )
        self.ui.write(
            _x(',\n  "parents": [%s]')
            % ", ".join('"%s"' % c.hex() for c in ctx.parents())
        )

        if self.ui.debugflag:
            if rev is None:
                jmanifestnode = "null"
            else:
                jmanifestnode = '"%s"' % hex(ctx.manifestnode())
            self.ui.write(_x(',\n  "manifest": %s') % jmanifestnode)

            self.ui.write(
                _x(',\n  "extra": {%s}')
                % ", ".join("%s: %s" % (j(k), j(v)) for k, v in ctx.extra().items())
            )

            files = ctx.p1().status(ctx)
            self.ui.write(
                _x(',\n  "modified": [%s]') % ", ".join("%s" % j(f) for f in files[0])
            )
            self.ui.write(
                _x(',\n  "added": [%s]') % ", ".join("%s" % j(f) for f in files[1])
            )
            self.ui.write(
                _x(',\n  "removed": [%s]') % ", ".join("%s" % j(f) for f in files[2])
            )

        elif self.ui.verbose:
            self.ui.write(
                _x(',\n  "files": [%s]') % ", ".join("%s" % j(f) for f in ctx.files())
            )

            copies = revcache.get("copies")
            if copies:
                self.ui.write(
                    _x(',\n  "copies": {%s}')
                    % ", ".join("%s: %s" % (j(k), j(v)) for k, v in copies)
                )

        matchfn = self.matchfn
        if matchfn:
            stat = self.diffopts.get("stat")
            diff = self.diffopts.get("patch")
            diffopts = patch.difffeatureopts(self.ui, self.diffopts, git=True)
            if prevfn is None:
                prevctx = ctx.p1()
            else:
                prevctx = prevfn(ctx)
            if stat:
                self.ui.pushbuffer()
                diffordiffstat(
                    self.ui, self.repo, diffopts, prevctx, ctx, match=matchfn, stat=True
                )
                self.ui.write(
                    _x(',\n  "diffstat": %s') % json.dumps(self.ui.popbuffer())
                )
            if diff:
                self.ui.pushbuffer()
                diffordiffstat(
                    self.ui,
                    self.repo,
                    diffopts,
                    prevctx,
                    ctx,
                    match=matchfn,
                    stat=False,
                )
                # Don't use the j() helper because it expects utf8 strings.
                diff = encoding.jsonescape(self.ui.popbufferbytes())
                self.ui.writebytes(b',\n  "diff": "%s"' % diff)

        self.ui.write("\n }")


class changeset_templater(changeset_printer):
    """format changeset information.

    Note: there are a variety of convenience functions to build a
    changeset_templater for common cases. See functions such as:
    makelogtemplater, show_changeset, buildcommittemplate, or other
    functions that use changesest_templater.
    """

    # Arguments before "buffered" used to be positional. Consider not
    # adding/removing arguments before "buffered" to not break callers.
    def __init__(self, ui, repo, tmplspec, matchfn=None, diffopts=None, buffered=False):
        diffopts = diffopts or {}

        changeset_printer.__init__(self, ui, repo, matchfn, diffopts, buffered)
        self.t = formatter.loadtemplater(ui, tmplspec, cache=templatekw.defaulttempl)
        self._counter = itertools.count()
        self.cache = {}

        self._tref = tmplspec.ref
        self._parts = {
            "header": "",
            "footer": "",
            tmplspec.ref: tmplspec.ref,
            "docheader": "",
            "docfooter": "",
            "separator": "",
        }
        if tmplspec.mapfile:
            # find correct templates for current mode, for backward
            # compatibility with 'log -v/-q/--debug' using a mapfile
            tmplmodes = [
                (True, ""),
                (self.ui.verbose, "_verbose"),
                (self.ui.quiet, "_quiet"),
                (self.ui.debugflag, "_debug"),
            ]
            for mode, postfix in tmplmodes:
                for t in self._parts:
                    cur = t + postfix
                    if mode and cur in self.t:
                        self._parts[t] = cur
        else:
            partnames = [p for p in self._parts.keys() if p != tmplspec.ref]
            m = formatter.templatepartsmap(tmplspec, self.t, partnames)
            self._parts.update(m)

        if self._parts["docheader"]:
            self.ui.write(templater.stringify(self.t(self._parts["docheader"])))

    def close(self):
        if self._parts["docfooter"]:
            if not self.footer:
                self.footer = ""
            self.footer += templater.stringify(self.t(self._parts["docfooter"]))
        return super(changeset_templater, self).close()

    def _show(self, ctx, revcache, matchfn, hunksfilterfn, prevfn, props):
        """show a single changeset or file revision"""
        props = props.copy()
        props.update(templatekw.keywords)
        props["templ"] = self.t
        props["ctx"] = ctx
        props["repo"] = self.repo
        props["ui"] = self.repo.ui
        props["index"] = index = next(self._counter)
        props["revcache"] = revcache
        props["cache"] = self.cache

        # write separator, which wouldn't work well with the header part below
        # since there's inherently a conflict between header (across items) and
        # separator (per item)
        if self._parts["separator"] and index > 0:
            self.ui.write(templater.stringify(self.t(self._parts["separator"])))

        # write header
        if self._parts["header"]:
            h = templater.stringify(self.t(self._parts["header"], **props))
            if self.buffered:
                self.header[ctx.rev()] = h
            else:
                if self.lastheader != h:
                    self.lastheader = h
                    self.ui.write(h)

        # write changeset metadata, then patch if requested
        key = self._parts[self._tref]
        self.ui.writebytes(templater.byteify(self.t(key, **props)))
        self.showpatch(ctx, matchfn, hunksfilterfn=hunksfilterfn, prevfn=prevfn)

        if self._parts["footer"]:
            if not self.footer:
                self.footer = templater.stringify(
                    self.t(self._parts["footer"], **props)
                )


def logtemplatespec(tmpl, mapfile):
    if mapfile:
        return formatter.templatespec("changeset", tmpl, mapfile)
    else:
        return formatter.templatespec("", tmpl, None)


def _lookuplogtemplate(ui, tmpl, style):
    """Find the template matching the given template spec or style

    See formatter.lookuptemplate() for details.
    """

    # ui settings
    if not tmpl and not style:  # template are stronger than style
        tmpl = ui.config("ui", "logtemplate")
        if tmpl:
            return logtemplatespec(templater.unquotestring(tmpl), None)
        else:
            style = util.expandpath(ui.config("ui", "style", ""))

    if not tmpl and style:
        mapfile = style
        if not os.path.split(mapfile)[0]:
            mapname = templater.templatepath(
                "map-cmdline." + mapfile
            ) or templater.templatepath(mapfile)
            if mapname:
                mapfile = mapname
        return logtemplatespec(None, mapfile)

    if not tmpl:
        return logtemplatespec(None, None)

    return formatter.lookuptemplate(ui, "changeset", tmpl)


def makelogtemplater(ui, repo, tmpl, buffered=False):
    """Create a changeset_templater from a literal template 'tmpl'
    byte-string."""
    spec = logtemplatespec(tmpl, None)
    return changeset_templater(ui, repo, spec, buffered=buffered)


def show_changeset(ui, repo, opts, buffered=False):
    """show one changeset using template or regular display.

    Display format will be the first non-empty hit of:
    1. option 'template'
    2. option 'style'
    3. [ui] setting 'logtemplate'
    4. [ui] setting 'style'
    If all of these values are either the unset or the empty string,
    regular display via changeset_printer() is done.
    """
    # options
    match = None
    if opts.get("patch") or opts.get("stat"):
        match = scmutil.matchall(repo)

    if opts.get("template") == "json":
        return jsonchangeset(ui, repo, match, opts, buffered)

    spec = _lookuplogtemplate(ui, opts.get("template"), opts.get("style"))

    if not spec.ref and not spec.tmpl and not spec.mapfile:
        return changeset_printer(ui, repo, match, opts, buffered)

    return changeset_templater(ui, repo, spec, match, opts, buffered)


def showmarker(fm, marker, index=None):
    """utility function to display obsolescence marker in a readable way

    To be used by debug function."""
    if index is not None:
        fm.write("index", "%i ", index)
    fm.write("prednode", "%s ", hex(marker.prednode()))
    succs = marker.succnodes()
    fm.condwrite(
        succs, "succnodes", "%s ", fm.formatlist(list(map(hex, succs)), name="node")
    )
    fm.write("flag", "%X ", marker.flags())
    parents = marker.parentnodes()
    if parents is not None:
        fm.write(
            "parentnodes",
            "{%s} ",
            fm.formatlist(list(map(hex, parents)), name="node", sep=", "),
        )
    fm.write("date", "(%s) ", fm.formatdate(marker.date()))
    meta = marker.metadata().copy()
    meta.pop("date", None)
    fm.write("metadata", "{%s}", fm.formatdict(meta, fmt="%r: %r", sep=", "))
    fm.plain("\n")


def finddate(ui, repo, date):
    """Find the tipmost changeset that matches the given date spec"""

    if not ui.quiet:
        hintutil.triggershow(ui, "date-option", date, bookmarks.mainbookmark(repo))
    df = util.matchdate(date)
    m = scmutil.matchall(repo)
    results = {}

    def prep(ctx, fns):
        d = ctx.date()
        if df(d[0]):
            results[ctx.node()] = d

    for ctx in walkchangerevs(repo, m, {"rev": None}, prep):
        node = ctx.node()
        if node in results:
            ui.status(
                _("found revision %s from %s\n")
                % (hex(node), util.datestr(results[node]))
            )
            return node

    raise error.Abort(_("revision matching date not found"))


def increasingwindows(windowsize=8, sizelimit=512):
    while True:
        yield windowsize
        if windowsize < sizelimit:
            windowsize *= 2


class FileWalkError(Exception):
    pass


def walkfilerevs(repo, match, follow, revs, fncache):
    """Walks the file history for the matched files.

    Returns the changeset revs that are involved in the file history.

    Throws FileWalkError if the file history can't be walked using
    filelogs alone.
    """
    wanted = set()
    copies = []
    minrev, maxrev = min(revs), max(revs)

    def filerevgen(filelog, last):
        """
        Only files, no patterns.  Check the history of each file.

        Examines filelog entries within minrev, maxrev linkrev range
        Returns an iterator yielding (linkrev, parentlinkrevs, copied)
        tuples in backwards order
        """
        cl_count = len(repo)
        revs = []
        for j in range(0, last + 1):
            linkrev = filelog.linkrev(j)
            if linkrev < minrev:
                continue
            # only yield rev for which we have the changelog, it can
            # happen while doing "hg log" during a pull or commit
            if linkrev >= cl_count:
                break

            parentlinkrevs = []
            for p in filelog.parentrevs(j):
                if p != nullrev:
                    parentlinkrevs.append(filelog.linkrev(p))
            n = filelog.node(j)
            revs.append((linkrev, parentlinkrevs, follow and filelog.renamed(n)))

        return reversed(revs)

    def iterfiles():
        pctx = repo["."]
        for filename in match.files():
            if follow:
                if filename not in pctx:
                    raise error.Abort(
                        _('cannot follow file not in parent revision: "%s"') % filename
                    )
                yield filename, pctx[filename].filenode()
            else:
                yield filename, None
        for filename_node in copies:
            yield filename_node

    for file_, node in iterfiles():
        filelog = repo.file(file_)
        if not len(filelog):
            if node is None:
                # A zero count may be a directory or deleted file, so
                # try to find matching entries on the slow path.
                if follow:
                    raise error.Abort(_('cannot follow nonexistent file: "%s"') % file_)
                raise FileWalkError("Cannot walk via filelog")
            else:
                continue

        if node is None:
            last = len(filelog) - 1
        else:
            last = filelog.rev(node)

        # keep track of all ancestors of the file
        ancestors = {filelog.linkrev(last)}

        # iterate from latest to oldest revision
        for rev, flparentlinkrevs, copied in filerevgen(filelog, last):
            if not follow:
                if rev > maxrev:
                    continue
            else:
                # Note that last might not be the first interesting
                # rev to us:
                # if the file has been changed after maxrev, we'll
                # have linkrev(last) > maxrev, and we still need
                # to explore the file graph
                if rev not in ancestors:
                    continue
                # XXX insert 1327 fix here
                if flparentlinkrevs:
                    ancestors.update(flparentlinkrevs)

            fncache.setdefault(rev, []).append(file_)
            wanted.add(rev)
            if copied:
                copies.append(copied)

    return wanted


class _followfilter:
    def __init__(self, repo, onlyfirst=False):
        self.repo = repo
        self.startrev = nullrev
        self.roots = set()
        self.onlyfirst = onlyfirst

    def match(self, rev):
        def realparents(rev):
            if self.onlyfirst:
                return self.repo.changelog.parentrevs(rev)[0:1]
            else:
                return filter(
                    lambda x: x != nullrev, self.repo.changelog.parentrevs(rev)
                )

        if self.startrev == nullrev:
            self.startrev = rev
            return True

        if rev > self.startrev:
            # forward: all descendants
            if not self.roots:
                self.roots.add(self.startrev)
            for parent in realparents(rev):
                if parent in self.roots:
                    self.roots.add(rev)
                    return True
        else:
            # backwards: all parents
            if not self.roots:
                self.roots.update(realparents(self.startrev))
            if rev in self.roots:
                self.roots.remove(rev)
                self.roots.update(realparents(rev))
                return True

        return False


def walkchangerevs(repo, match, opts, prepare):
    """Iterate over files and the revs in which they changed.

    Callers most commonly need to iterate backwards over the history
    in which they are interested. Doing so has awful (quadratic-looking)
    performance, so we use iterators in a "windowed" way.

    We walk a window of revisions in the desired order.  Within the
    window, we first walk forwards to gather data, then in the desired
    order (usually backwards) to display it.

    This function returns an iterator yielding contexts. Before
    yielding each context, the iterator will first call the prepare
    function on each context in the window in forward order."""

    follow = opts.get("follow") or opts.get("follow_first")
    revs = _logrevs(repo, opts)
    if not revs:
        return []
    wanted = set()
    slowpath = match.anypats() or (
        (match.isexact() or match.prefix()) and opts.get("removed")
    )
    fncache = {}
    change = repo.changectx

    # First step is to fill wanted, the set of revisions that we want to yield.
    # When it does not induce extra cost, we also fill fncache for revisions in
    # wanted: a cache of filenames that were changed (ctx.files()) and that
    # match the file filtering conditions.

    if match.always():
        # No files, no patterns.  Display all revs.
        wanted = revs
    elif not slowpath:
        # We only have to read through the filelog to find wanted revisions

        try:
            wanted = walkfilerevs(repo, match, follow, revs, fncache)
        except FileWalkError:
            slowpath = True

            # We decided to fall back to the slowpath because at least one
            # of the paths was not a file. Check to see if at least one of them
            # existed in history, otherwise simply return
            for path in match.files():
                if path == "." or path in repo.store:
                    break
            else:
                return []

    if slowpath:
        # We have to read the changelog to match filenames against
        # changed files

        if follow:
            raise error.Abort(
                _("can only follow copies/renames for explicit filenames")
            )

        # The slow path checks files modified in every changeset.
        # This is really slow on large repos, so compute the set lazily.
        class lazywantedset:
            def __init__(self):
                self.set = set()
                self.revs = set(revs)

            # No need to worry about locality here because it will be accessed
            # in the same order as the increasing window below.
            def __contains__(self, value):
                if value in self.set:
                    return True
                elif not value in self.revs:
                    return False
                else:
                    self.revs.discard(value)
                    ctx = change(value)
                    matches = filter(match, ctx.files())
                    if matches:
                        fncache[value] = matches
                        self.set.add(value)
                        return True
                    return False

            def discard(self, value):
                self.revs.discard(value)
                self.set.discard(value)

        wanted = lazywantedset()

    # it might be worthwhile to do this in the iterator if the rev range
    # is descending and the prune args are all within that range
    for rev in opts.get("prune", ()):
        rev = repo[rev].rev()
        ff = _followfilter(repo)
        stop = min(revs[0], revs[-1])
        for x in range(rev, stop - 1, -1):
            if ff.match(x):
                wanted = wanted - [x]

    # Now that wanted is correctly initialized, we can iterate over the
    # revision range, yielding only revisions in wanted.
    def iterate():
        if follow and match.always():
            ff = _followfilter(repo, onlyfirst=opts.get("follow_first"))

            def want(rev):
                return ff.match(rev) and rev in wanted

        else:

            def want(rev):
                return rev in wanted

        it = iter(revs)
        stopiteration = False
        for windowsize in increasingwindows():
            nrevs = []
            for i in range(windowsize):
                rev = next(it, None)
                if rev is None:
                    stopiteration = True
                    break
                elif want(rev):
                    nrevs.append(rev)
            for rev in sorted(nrevs):
                fns = fncache.get(rev)
                ctx = change(rev)
                if not fns:

                    def fns_generator():
                        for f in ctx.files():
                            if match(f):
                                yield f

                    fns = fns_generator()
                prepare(ctx, fns)
            for rev in nrevs:
                yield change(rev)

            if stopiteration:
                break

    return iterate()


def _makefollowlogfilematcher(repo, paths, followfirst, pctx):
    # When displaying a revision with --patch --follow FILE, we have
    # to know which file of the revision must be diffed. With
    # --follow, we want the names of the ancestors of FILE in the
    # revision, stored in "fcache". "fcache" is populated by
    # reproducing the graph traversal already done by --follow revset
    # and relating revs to file names (which is not "correct" but
    # good enough).
    fcache = {}
    fcacheready = [False]

    dirs = []
    files = []

    for path in paths:
        if path not in pctx:
            dirs.append(path)
        else:
            files.append(path)

    # When directories are passed in, walking the ancestors graph can be
    # extremely expensive, let's not attempt to do it and instead just match
    # all the files under the given directories.
    if not dirs:
        dirmatcher = None
    else:
        dirmatcher = matchmod.match(
            repo.root, repo.getcwd(), patterns=["path:%s" % path for path in dirs]
        )

    def populatefiles():
        # Walk the ancestors graph for all the files passed in.
        for fn in files:
            fctx = pctx[fn]
            fcache.setdefault(fctx.introrev(), set()).add(fctx.path())
            for c in fctx.ancestors(followfirst=followfirst):
                fcache.setdefault(c.rev(), set()).add(c.path())

    def filematcher(rev):
        if not fcacheready[0]:
            populatefiles()
            fcacheready[0] = True

        fileset = fcache.get(rev, [])
        if not fileset:
            filematcher = None
        else:
            filematcher = scmutil.matchfiles(repo, fcache.get(rev, []))

        return matchmod.union([dirmatcher, filematcher], repo.root, repo.getcwd())

    return filematcher


def _makenofollowlogfilematcher(repo, pats, opts):
    """hook for extensions to override the filematcher for non-follow cases"""
    return None


def _usepathhistory(repo):
    """whether to use the PathHistory API for log history"""
    # Git repo does not have filelog, linkrev. Must use PathHistory.
    if git.isgitformat(repo):
        return True
    return repo.ui.configbool("experimental", "pathhistory")


def _makelogrevset(repo, pats, opts, revs):
    """Return (expr, filematcher) where expr is a revset string built
    from log options and file patterns or None. If --stat or --patch
    are not passed filematcher is None. Otherwise it is a callable
    taking a revision number and returning a match objects filtering
    the files to be detailed when displaying the revision.
    """
    # Revset expressions for the log command. They will be merged using "and"
    # later.
    revset_filters = []

    if opts.get("no_merges"):
        revset_filters.append("not merge()")
    if opts.get("only_merges"):
        revset_filters.append("merge()")
    if opts.get("date"):
        revset_filters.append(revsetlang.formatspec("date(%s)", opts.get("date")))

    def append_revset_list(
        format_spec, values, join_op, extra_format_args=(), out=revset_filters
    ):
        if values:
            spec = revsetlang.formatlist(
                [
                    revsetlang.formatspec(format_spec, v, *extra_format_args)
                    for v in values
                ],
                join_op,
            )
            out.append(spec)

    append_revset_list("keyword(%s)", opts.get("keyword"), "or")
    append_revset_list("user(%s)", opts.get("user"), "or")
    append_revset_list("not ancestors(%s)", opts.get("prune"), "and")

    # follow or not follow?
    follow = opts.get("follow") or opts.get("follow_first")
    if opts.get("follow_first"):
        followfirst = 1
    else:
        followfirst = 0
    # --follow with FILE behavior depends on revs...
    it = iter(revs)
    startrev = next(it)
    followdescendants = startrev < next(it, startrev)

    # pats/include/exclude are passed to match.match() directly in
    # _matchfiles() revset but walkchangerevs() builds its matcher with
    # scmutil.match(). The difference is input pats are globbed on
    # platforms without shell expansion (windows).
    wctx = repo[None]
    match, pats = scmutil.matchandpats(wctx, pats, opts)

    # Different strategies to calculate file history:
    # - pathhistory: use "_pathhistory()" revset.
    # - follow: use "follow()" revset.
    # - filelog: use "filelog() revset (legacy).
    # - matchfiles: general slow path that scans commits one by one.
    # These functions return `None` if the strategy is unavailable, or a list
    # of revset expressions to be appended to `revset_filters`.

    def strategy_no_patterns():
        if pats or opts.get("include") or opts.get("exclude"):
            # Cannot be used when there are patterns.
            return

        if not follow:
            return []
        if followdescendants:
            spec = revsetlang.formatspec("descendants(%d)", startrev)
        else:
            if followfirst:
                spec = revsetlang.formatspec("_firstancestors(%d)", startrev)
            else:
                spec = revsetlang.formatspec("ancestors(%d)", startrev)
        return [spec]

    def strategy_pathhistory():
        if not _usepathhistory(repo):
            # pathhistory is currently opt-in only.
            # we might want to make it the default later.
            # or maybe move its implementation directly to the follow() and
            # _followfirst() revsets (but they probably want to take multiple
            # paths somehow).
            return
        if match.anypats():
            # pathhistory does not yet support glob patterns.
            # Maybe we can support it by extracting "prefix" and run
            # pathhistory on the prefix, then run extra filtering later.
            return

        if pats:
            paths = list(match.files())
            # pathhistory: force "follow" if "pats" is given.
            if followfirst:
                phrevs = revsetlang.formatspec("_firstancestors(%d)", startrev)
            else:
                phrevs = revsetlang.formatspec("ancestors(%d)", startrev)
            return [revsetlang.formatspec("_pathhistory(%r,%vs)", phrevs, paths)]

    def strategy_follow():
        if match.anypats() or (
            (match.isexact() or match.prefix()) and opts.get("removed")
        ):
            # follow does not support glob patterns, or --removed
            return

        maybe_startrev = []
        if not follow:
            # Usually, the follow strategy requires --follow or --follow-first.
            # However, if the user passes a --rev that is known large, and has
            # a single head, we might still want to use `follow()` to speed it
            # up even without --follow.
            if not opts.get("rev"):
                # Only interested in user-provided --rev.
                # This avoids potentially slow calculation like `heads(_all())`.
                return None
            threshold = repo.ui.configint(
                "experimental", "log-implicit-follow-threshold"
            )
            if threshold is None or threshold < 0:
                # Implicit follow can be disabled using a negative config.
                return
            revs_len = revs.fastlen()
            if revs_len is None:
                # Cannot figure out the size of `revs` quickly.
                return
            if pats:
                # If there are many files, follow() might be not worth it.
                files_len = len(match.files())
                if files_len > 0:
                    revs_len //= files_len
            if revs_len < threshold:
                # strategy_matchfiles is not too slow for a small `revs` set.
                return
            cl = repo.changelog
            heads = cl.dag.heads(cl.tonodes(revs))
            if len(heads) > 1:
                # follow() requires a single head.
                return
            maybe_startrev = [hex(heads.first())]

        if pats:
            # Compatible with the old behavior. If we don't raise we might
            # still use other strategy for the request.
            for f in match.files():
                if f not in wctx:
                    # If the file exists, it may be a directory. The "follow"
                    # revset can handle directories fine. So no need to use
                    # the slow path.
                    if os.path.exists(repo.wjoin(f)):
                        continue
                    else:
                        hint = None
                        if len(pats) == 1 and pats[0] in repo:
                            hint = _(
                                """did you mean "@prog@ log -r '%s'", or "@prog@ log -r '%s' -f" to follow history?"""
                            ) % (pats[0], pats[0])
                        raise error.Abort(
                            _('cannot follow file not in parent revision: "%s"') % f,
                            hint=hint,
                        )

            # follow() revset interprets its file argument as a
            # manifest entry, so use match.files(), not pats.
            result = []
            if followfirst:
                append_revset_list(
                    f"_followfirst(%s,%vs)",
                    match.files(),
                    "or",
                    extra_format_args=(maybe_startrev,),
                    out=result,
                )
            else:
                append_revset_list(
                    f"follow(%s,%vs)",
                    match.files(),
                    "or",
                    extra_format_args=(maybe_startrev,),
                    out=result,
                )
            return result

    def strategy_filelog():
        if follow:
            # filelog() is incompatible with "follow"
            return None
        if repo.storage_format() != "revlog":
            # Only the legacy revlog format supports filelog() logging.
            return None
        if match.anypats() or (
            (match.isexact() or match.prefix()) and opts.get("removed")
        ):
            # filelog does not support glob patterns, or --removed
            return None
        if any(not repo.file(f) for f in match.files()):
            # repo.file(f) is empty
            return None

        result = []
        append_revset_list("filelog(%s)", pats, "or", out=result)
        return result

    def strategy_matchfiles():
        matchargs = ["r:", "d:relpath"]
        for p in pats:
            matchargs.append("p:" + p)
        for p in opts.get("include", []):
            matchargs.append("i:" + p)
        for p in opts.get("exclude", []):
            matchargs.append("x:" + p)
        result = [revsetlang.formatspec("_matchfiles(%vs)", matchargs)]
        if followfirst:
            result.append("_firstancestors(.)")
        elif follow:
            result.append("ancestors(.)")
        return result

    strategy_candidates = [
        strategy_no_patterns,
        strategy_pathhistory,
        strategy_follow,
        strategy_filelog,
        strategy_matchfiles,
    ]
    chosen_strategy = None
    for strategy in strategy_candidates:
        new_filters = strategy()
        if new_filters is None:
            continue
        revset_filters += new_filters
        chosen_strategy = strategy
        break
    if not chosen_strategy:
        raise error.ProgrammingError("makelogrevset: no strategy chosen")

    filematcher = None
    if opts.get("patch") or opts.get("stat"):
        # When following files, track renames via a special matcher.
        # If we're forced to take the slowpath it means we're following
        # at least one pattern/directory, so don't bother with rename tracking.
        #
        # If path history is used, avoid using filelog and linkrev.
        if (
            follow
            and not match.always()
            and chosen_strategy not in {strategy_pathhistory, strategy_matchfiles}
        ):
            # _makefollowlogfilematcher expects its files argument to be
            # relative to the repo root, so use match.files(), not pats.
            filematcher = _makefollowlogfilematcher(
                repo, match.files(), followfirst, repo[startrev]
            )
        else:
            filematcher = _makenofollowlogfilematcher(repo, pats, opts)
            if filematcher is None:
                filematcher = lambda rev: match

    if revset_filters:
        expr = revsetlang.formatlist(revset_filters, "and")
        tracing.debug("log revset: %s\n" % expr, target="log::makelogrevset")
    else:
        expr = None
    return expr, filematcher


def _logrevs(repo, opts):
    # Default --rev value depends on --follow but --follow behavior
    # depends on revisions resolved from --rev...
    follow = opts.get("follow") or opts.get("follow_first")
    if opts.get("rev"):
        revs = scmutil.revrange(repo, opts["rev"])
    elif follow and repo.dirstate.p1() == nullid:
        revs = smartset.baseset(repo=repo)
    elif follow:
        revs = repo.revs("reverse(:.)")
    elif opts.get("mutation"):
        if repo.dirstate.p1() == nullid:
            revs = smartset.baseset(repo=repo)
        else:
            revs = repo.revs("mutationhistory(.)")
    else:
        revs = repo.revs("all()")
        revs.reverse()
    return revs


def getgraphlogrevs(repo, pats, opts):
    """Return (revs, expr, filematcher) where revs is an iterable of
    revision numbers, expr is a revset string built from log options
    and file patterns or None, and used to filter 'revs'. If --stat or
    --patch are not passed filematcher is None. Otherwise it is a
    callable taking a revision number and returning a match objects
    filtering the files to be detailed when displaying the revision.
    """
    limit = loglimit(opts)
    revs = _logrevs(repo, opts)
    if not revs:
        return smartset.baseset(repo=repo), None, None
    expr, filematcher = _makelogrevset(repo, pats, opts, revs)
    if expr:
        if opts.get("rev"):
            revs = repo.revs(expr, subset=revs)
        else:
            # revs is likely huge. "x & y" is more efficient if "x" is small.
            # "x & y" respects "x"'s order. Once rewritten to "y & x", the
            # order is decided by "y". Fortunately we know the order of "x" is
            # always "reverse" in this case. So just do a reverse sort.
            revs = repo.revs(expr) & revs
            revs.sort(reverse=True)
    if opts.get("mutation"):
        revs = smartset.baseset(graphmod.mutationsort(repo, revs), repo=repo)
    elif opts.get("rev"):
        # User-specified revs might be unsorted, but don't sort before
        # _makelogrevset because it might depend on the order of revs
        if not (revs.isdescending() or revs.istopo()):
            revs.sort(reverse=True)
    if limit is not None:
        revs = revs.slice(0, limit)

    return revs, expr, filematcher


def getlogrevs(repo, pats, opts):
    """Return (revs, expr, filematcher) where revs is an iterable of
    revision numbers, expr is a revset string built from log options
    and file patterns or None, and used to filter 'revs'. If --stat or
    --patch are not passed filematcher is None. Otherwise it is a
    callable taking a revision number and returning a match objects
    filtering the files to be detailed when displaying the revision.
    """
    limit = loglimit(opts)
    revs = _logrevs(repo, opts)
    if not revs:
        return smartset.baseset([], repo=repo), None, None
    expr, filematcher = _makelogrevset(repo, pats, opts, revs)
    if expr:
        if opts.get("rev"):
            revs = repo.revs(expr, subset=revs)
        else:
            # revs is likely huge. "x & y" is more efficient if "x" is small.
            # "x & y" respects "x"'s order. Once rewritten to "y & x", the
            # order is decided by "y". Fortunately we know the order of "x" is
            # always "reverse" in this case. So just do a reverse sort.
            revs = repo.revs(expr) & revs
            revs.sort(reverse=True)
    if opts.get("mutation"):
        revs = smartset.baseset(graphmod.mutationsort(repo, revs), repo=repo)
    if limit is not None:
        revs = revs.slice(0, limit)
    return revs, expr, filematcher


def _parselinerangelogopt(repo, opts):
    """Parse --line-range log option and return a list of tuples (filename,
    (fromline, toline)).
    """
    linerangebyfname = []
    for pat in opts.get("line_range", []):
        try:
            pat, linerange = pat.rsplit(",", 1)
        except ValueError:
            raise error.Abort(_("malformatted line-range pattern %s") % pat)
        try:
            fromline, toline = list(map(int, linerange.split(":")))
        except ValueError:
            raise error.Abort(_("invalid line range for %s") % pat)
        msg = _("line range pattern '%s' must match exactly one file") % pat
        fname = scmutil.parsefollowlinespattern(repo, None, pat, msg)
        linerangebyfname.append((fname, util.processlinerange(fromline, toline)))
    return linerangebyfname


def getloglinerangerevs(repo, userrevs, opts):
    """Return (revs, filematcher, hunksfilter).

    "revs" are revisions obtained by processing "line-range" log options and
    walking block ancestors of each specified file/line-range.

    "filematcher(rev) -> match" is a factory function returning a match object
    for a given revision for file patterns specified in --line-range option.
    If neither --stat nor --patch options are passed, "filematcher" is None.

    "hunksfilter(rev) -> filterfn(fctx, hunks)" is a factory function
    returning a hunks filtering function.
    If neither --stat nor --patch options are passed, "filterhunks" is None.
    """
    wctx = repo[None]

    # Two-levels map of "rev -> file ctx -> [line range]".
    linerangesbyrev = {}
    for fname, (fromline, toline) in _parselinerangelogopt(repo, opts):
        if fname not in wctx:
            raise error.Abort(
                _('cannot follow file not in parent revision: "%s"') % fname
            )
        fctx = wctx.filectx(fname)
        for fctx, linerange in dagop.blockancestors(fctx, fromline, toline):
            rev = fctx.introrev()
            if rev not in userrevs:
                continue
            linerangesbyrev.setdefault(rev, {}).setdefault(fctx.path(), []).append(
                linerange
            )

    if opts.get("patch") or opts.get("stat"):

        def nofilterhunksfn(fctx, hunks):
            return hunks

        def hunksfilter(rev):
            fctxlineranges = linerangesbyrev.get(rev)
            if fctxlineranges is None:
                return nofilterhunksfn

            def filterfn(fctx, hunks):
                lineranges = fctxlineranges.get(fctx.path())
                if lineranges is not None:
                    for hr, lines in hunks:
                        if hr is None:  # binary
                            yield hr, lines
                            continue
                        if any(mdiff.hunkinrange(hr[2:], lr) for lr in lineranges):
                            yield hr, lines
                else:
                    for hunk in hunks:
                        yield hunk

            return filterfn

        def filematcher(rev):
            files = list(linerangesbyrev.get(rev, []))
            return scmutil.matchfiles(repo, files)

    else:
        filematcher = None
        hunksfilter = None

    revs = sorted(linerangesbyrev, reverse=True)

    return smartset.baseset(revs, repo=repo), filematcher, hunksfilter


def _graphnodeformatter(ui, displayer):
    spec = ui.config("ui", "graphnodetemplate")
    if not spec:
        return templatekw.showgraphnode  # fast path for "{graphnode}"

    spec = templater.unquotestring(spec)
    templ = formatter.maketemplater(ui, spec)
    cache = {}
    if isinstance(displayer, changeset_templater):
        cache = displayer.cache  # reuse cache of slow templates
    props = templatekw.keywords.copy()
    props["templ"] = templ
    props["cache"] = cache

    def formatnode(repo, ctx):
        props["ctx"] = ctx
        props["repo"] = repo
        props["ui"] = repo.ui
        props["revcache"] = {}
        return templ.render(props)

    return formatnode


def displaygraph(
    ui,
    repo,
    dag,
    displayer,
    repogetrenamed=None,
    repofilematcher=None,
    props=None,
    reserved=None,
    out=None,
    on_output=None,
    graphnodeid_to_rev=None,
    mutation=False,
):
    repogetrenamed = repogetrenamed or {}
    repofilematcher = repofilematcher or {}
    props = props or {}
    if out is None:
        out = ui.write
    formatnode = _graphnodeformatter(ui, displayer)
    prevfn = None
    if mutation:
        prevfn = mutation_prevfn
    if ui.plain("graph"):
        renderername = "ascii"
    else:
        renderername = ui.config("experimental", "graph.renderer")
    if renderername == "lines":
        # Find which renderer can render to the current output encoding.  If
        # none are supported we will fall back to the ASCII renderer.
        for chars, candidate in [
            (renderdag.linescurvedchars, "lines-curved"),
            (renderdag.linessquarechars, "lines-square"),
        ]:
            try:
                chars.encode(encoding.outputencoding or encoding.encoding, "strict")
                renderername = candidate
                break
            except Exception:
                continue
    renderers = {
        "ascii": renderdag.ascii,
        "ascii-large": renderdag.asciilarge,
        "lines-curved": renderdag.linescurved,
        "lines-square": renderdag.linessquare,
        "lines-dec": renderdag.linesdec,
    }
    renderer = renderers.get(renderername, renderdag.ascii)
    minheight = 1 if ui.configbool("experimental", "graphshorten") else 2
    minheight = ui.configint("experimental", "graph.min-row-height", minheight)
    renderer = renderer(minheight)

    if reserved:
        for graphnodeid in reserved:
            renderer.reserve(graphnodeid)

    getrenamed = repogetrenamed.get(repo.root)
    filematcher = repofilematcher.get(repo.root)
    prev_repo = repo
    xreponame = None

    show_abbreviated_ancestors = ShowAbbreviatedAncestorsWhen.load_from_config(repo.ui)
    for graphnodeid, _type, ctx, parents in dag:
        if _type == graphmod.MISSINGCHANGESET:
            # "ctx" is the node of a commit that is not available locally.
            msg = _("%s  (not available locally)\n") % hex(ctx)[:12]
            out(renderer.nextrow(graphnodeid, parents, "?", msg))
            continue
        curr_repo = ctx.repo()
        if curr_repo.root != prev_repo.root:
            getrenamed = repogetrenamed.get(curr_repo.root)
            filematcher = repofilematcher.get(curr_repo.root)
            xreponame = curr_repo.ui.config("remotefilelog", "reponame", "unknown")
            prev_repo = curr_repo

        char = formatnode(curr_repo, ctx)
        copies = None
        if getrenamed and ctx.rev():
            copies = []
            for fn in ctx.files():
                rename = getrenamed(fn, ctx.rev())
                if rename:
                    copies.append((fn, rename[0]))
        revmatchfn = None
        if filematcher is not None:
            revmatchfn = filematcher(ctx.rev())
        if show_abbreviated_ancestors is ShowAbbreviatedAncestorsWhen.ONLYMERGE:
            if len(parents) == 1 and parents[0][0] == graphmod.MISSINGPARENT:
                parents = []
        elif show_abbreviated_ancestors is ShowAbbreviatedAncestorsWhen.NEVER:
            parents = [p for p in parents if p[0] != graphmod.MISSINGPARENT]

        gprevs = [
            graphnodeid_to_rev(p[1]) if graphnodeid_to_rev else p[1]
            for p in parents
            if p[0] == graphmod.GRANDPARENT
        ]
        if all(isinstance(gp, bytes) for gp in gprevs):
            # parents are already nodes (when called from ext/commitcloud/commands.py)
            gpnodes = gprevs
        else:
            gpnodes = curr_repo.changelog.tonodes(gprevs)
        if mutation and gpnodes:
            # {grandparents} can only refer to commits available locally.
            gpnodes = curr_repo.changelog.filternodes(gpnodes, local=True)
        revcache = {"copies": copies, "gpnodes": gpnodes}
        if xreponame:
            revcache["xreponame"] = xreponame
        width = renderer.width(graphnodeid, parents)
        displayer.show(
            ctx,
            revcache=revcache,
            matchfn=revmatchfn,
            prevfn=prevfn,
            _graphwidth=width,
            **props,
        )
        # The Rust graph renderer works with unicode.
        msg = "".join(
            s if isinstance(s, str) else s.decode(errors="replace")
            for s in displayer.hunk.pop(ctx.rev())
        )
        nextrow = renderer.nextrow(graphnodeid, parents, char, msg)
        out(nextrow)
        if on_output is not None:
            on_output(
                ctx,
                nextrow,
                {
                    "width": width,
                    "revcache": revcache,
                    "matchfn": revmatchfn,
                    "message": msg,
                },
            )
        displayer.flush(ctx)

    displayer.close()


class ShowAbbreviatedAncestorsWhen(Enum):
    ALWAYS = "always"
    ONLYMERGE = "onlymerge"
    NEVER = "never"

    @staticmethod
    def load_from_config(
        config: "typing.Union[ui, uiconfig]",
    ) -> "ShowAbbreviatedAncestorsWhen":
        section = "experimental"
        setting_name = "graph.show-abbreviated-ancestors"
        value = config.config(section, setting_name)
        if value is None:
            return ShowAbbreviatedAncestorsWhen.ALWAYS
        try:
            return ShowAbbreviatedAncestorsWhen(value)
        except ValueError:
            pass
        b = util.parsebool(value)
        if b is not None:
            return (
                ShowAbbreviatedAncestorsWhen.ALWAYS
                if b
                else ShowAbbreviatedAncestorsWhen.NEVER
            )
        raise error.ConfigError(
            _(
                "%s.%s is invalid; expected 'always' or 'never' or 'onlymerge', but got '%s'"
            )
            % (section, setting_name, value)
        )


def graphlog(ui, repo, pats: Tuple[str, ...], opts: Dict[str, Any]):
    # Parameters are identical to log command ones
    repogetrenamed, repofilematcher, repoids = {}, {}, {}
    mutation = opts.get("mutation")
    revdag = _logdagwalker(
        repo, pats, opts, repogetrenamed, repofilematcher, repoids, mutation
    )

    ui.pager("log")
    displayer = show_changeset(ui, repo, opts, buffered=True)
    # The mutation graph's ids are already nodes.
    graphnodeid_to_rev = None if mutation else lambda graphnodeid: graphnodeid[1]
    displaygraph(
        ui,
        repo,
        revdag,
        displayer,
        repogetrenamed,
        repofilematcher,
        graphnodeid_to_rev=graphnodeid_to_rev,
        mutation=mutation,
    )


def mutation_prevfn(ctx):
    repo = ctx.repo()
    cl = repo.changelog
    node = ctx.node()
    entry = repo._mutationstore.get(node)
    if entry and len(entry.preds()) == 1 and not entry.split():
        pred = entry.preds()[0]
        # Skip, if the pred is not known locally.
        if cl.filternodes([pred], local=True):
            # Do not try to diff if the stack is rebased to another public commit.
            # The diff will include public changes, and could be a waste of time.
            # Ideally, we can show second-order diff (diff of two diffs),
            # and no need for this protection.
            rebased = repo.dageval(
                lambda: set(headsancestors(ancestors([node]) & public()))
                != set(headsancestors(ancestors([pred]) & public()))
            )
            if not rebased:
                return repo[pred]

    # fallback to regular p1 parent
    return ctx.p1()


def _logdagwalker(repo, pats, opts, repogetrenamed, repofilematcher, repoids, mutation):
    revs, expr, filematcher = getgraphlogrevs(repo, pats, opts)
    template = opts.get("template") or ""
    if mutation:
        revdag = graphmod.mutationdagwalker(
            repo, revs, template, showmissing=opts.get("show_missing")
        )
    else:
        if repo.root not in repoids:
            repoids[repo.root] = len(repoids)
        rid = repoids[repo.root]
        rev_to_graphnodeid = lambda rev: (rid, rev)
        revdag = graphmod.dagwalker(repo, revs, template, idfunc=rev_to_graphnodeid)

    getrenamed = None
    if opts.get("copies"):
        endrev = None
        if opts.get("rev"):
            endrev = scmutil.revrange(repo, opts.get("rev")).max() + 1
        getrenamed = templatekw.getrenamedfn(repo, endrev=endrev)

    repogetrenamed[repo.root] = getrenamed
    repofilematcher[repo.root] = filematcher
    if mutation:
        return revdag
    return _dagfollowxrepo(
        repo, pats, opts, revdag, repogetrenamed, repofilematcher, repoids, mutation
    )


def _dagfollowxrepo(
    repo, pats, opts, revdag, repogetrenamed, repofilematcher, repoids, mutation
):
    for item, is_last in iterutil.mark_last(revdag):
        # It currently doesn't handle multiple imports/merges. In those cases, the
        # xrepo operation can appear in the middle of the dag.
        if is_last:
            xrepoinfo = xrepologinfo(repo, pats, opts, item[2])
            if not xrepoinfo:
                yield item
                break

            from_repo, from_commit, from_path = xrepoinfo
            pats = [os.path.join(from_repo.root, from_path)]
            opts = opts.copy()
            opts["rev"] = [f"reverse(::{from_commit})"]
            for from_item, is_first in iterutil.mark_first(
                _logdagwalker(
                    from_repo,
                    pats,
                    opts,
                    repogetrenamed,
                    repofilematcher,
                    repoids,
                    mutation,
                )
            ):
                if is_first:
                    # clear the in-repo missing parents and add cross-repo parents
                    item[3].clear()
                    item[3].append((graphmod.XREPOPARENT, from_item[0]))
                    yield item
                yield from_item
        else:
            yield item


def xrepologinfo(curr_repo, curr_pats, curr_opts, lastctx):
    if not curr_repo.ui.configbool("log", "follow-xrepo", True):
        return None

    # XXX: currently supports only one path
    if lastctx is None or len(curr_pats) != 1:
        return None
    match, pats = scmutil.matchandpats(curr_repo[None], curr_pats, curr_opts)
    if len(match.files()) != 1:
        return None

    curr_path = match.files()[0]
    subtree_import = subtreeutil.find_subtree_import(
        curr_repo, lastctx.node(), curr_path
    )
    if not subtree_import:
        return None

    from_url, from_commit, from_path = subtree_import
    # XXX: currently only support import from git repo
    git_url = git.maybegiturl(from_url)
    if not git_url:
        return None

    with curr_repo.ui.configoverride({("ui", "quiet"): True}):
        from_repo = subtreeutil.get_or_clone_git_repo(
            curr_repo.ui, git_url, from_commit
        )
    return from_repo, from_commit, from_path


def checkunsupportedgraphflags(pats, opts):
    for op in ["newest_first"]:
        if op in opts and opts[op]:
            raise error.Abort(
                _("-G/--graph option is incompatible with --%s") % op.replace("_", "-")
            )


def add(ui, repo, match, prefix, explicitonly, **opts):
    bad = []

    badfn = lambda x, y: bad.append(x) or match.bad(x, y)
    names = []
    wctx = repo[None]
    pctx = wctx.p1()
    cca = None
    abort, warn = scmutil.checkportabilityalert(ui)
    if abort or warn:
        cca = scmutil.casecollisionauditor(ui, abort, repo.dirstate)

    badmatch = matchmod.badmatch(match, badfn)

    # While we technically can't add certain files, and therefore may not need
    # them returned from status, we still want them returned from status so we
    # can report errors if the user tries to add something that already exists.
    status = repo.dirstate.status(badmatch, False, False, True)
    files = set(file for files in status for file in files)

    # Status might not have returned clean or ignored files, so let's add them
    # so we can add ignored files and warn them if they try to add an existing
    # file.
    ignored = repo.dirstate._ignore
    files.update(
        file
        for file in match.files()
        if file in pctx or (ignored(file) and repo.wvfs.isfileorlink(file))
    )

    if util.fscasesensitive(repo.root):

        def normpath(f):
            return f

    else:

        def normpath(f):
            return f.lower()

    # Mapping of normalized exact path to actual exact path.
    # On case sensitive filesystems this serves no purpose.
    norm_to_exact = {normpath(f): f for f in files if match.exact(f)}

    ignored_added = []
    for f in sorted(files):
        fn = normpath(f)
        if fn in norm_to_exact and f != norm_to_exact[fn]:
            # Skip if we are a case collision with an exactly matched file. The
            # Rust matcher is case insensitive on case insensitive file
            # systems, but there is one situation where you can still have
            # files that differ only in case:
            #
            #   $ sl status
            #   ? FOO
            #   R foo
            #
            # If you run "sl add FOO" we only want to add FOO even though the
            # "FOO" pattern matches both foo and FOO (assuming you are on a
            # case insensitive filesystem).
            continue

        exact = fn in norm_to_exact
        if exact or not explicitonly and f not in wctx and repo.wvfs.lexists(f):
            if cca:
                cca(f)
            names.append(f)
            if ui.verbose or not exact:
                ui.status(_("adding %s\n") % match.rel(f))
            if ignored(f) and f not in pctx:
                ignored_added.append(match.rel(f))

    if ignored_added:
        ui.status(
            _(
                "the following files are ignored, but still added because they are explicitly specified:\n"
            )
        )
        for p in ignored_added:
            ui.status("  %s\n" % p)
        ui.status(
            _("(use '@prog@ debugignore <file>' to check why they are ignored)\n")
        )

    if not opts.get(r"dry_run"):
        rejected = wctx.add(names, prefix)
        bad.extend(f for f in rejected if f in match.files())
    return bad


def addwebdirpath(repo, serverpath, webconf):
    webconf[serverpath] = repo.root
    repo.ui.debug("adding %s = %s\n" % (serverpath, repo.root))


def forget(ui, repo, match, prefix, explicitonly):
    bad = []
    badfn = lambda x, y: bad.append(x) or match.bad(x, y)
    wctx = repo[None]
    forgot = []

    s = repo.status(match=matchmod.badmatch(match, badfn), clean=True)
    forget = sorted(s.modified + s.added + s.deleted + s.clean)
    if explicitonly:
        forget = [f for f in forget if match.exact(f)]

    if not explicitonly:
        for f in match.files():
            if f not in repo.dirstate and not repo.wvfs.isdir(f):
                if f not in forgot:
                    if repo.wvfs.exists(f):
                        # Don't complain if the exact case match wasn't given.
                        # But don't do this until after checking 'forgot', so
                        # that subrepo files aren't normalized, and this op is
                        # purely from data cached by the status walk above.
                        if repo.dirstate.normalize(f) in repo.dirstate:
                            continue
                        ui.warn(
                            _("not removing %s: file is already untracked\n")
                            % match.rel(f)
                        )
                    bad.append(f)

    for f in forget:
        if ui.verbose or not match.exact(f):
            ui.status(_("removing %s\n") % match.rel(f))

    rejected = wctx.forget(forget, prefix)
    bad.extend(f for f in rejected if f in match.files())
    forgot.extend(f for f in forget if f not in rejected)
    return bad, forgot


def files(ui, ctx, m, fm, fmt):
    if (ctx.rev() is None) and (edenfs.requirement in ctx.repo().requirements):
        return eden_files(ui, ctx, m, fm, fmt)

    rev = ctx.rev()
    ret = 1
    ds = ctx.repo().dirstate

    for f in ctx.matches(m):
        if rev is None and ds[f] == "r":
            continue
        fm.startitem()
        if ui.verbose:
            fc = ctx[f]
            fm.write("size flags", "% 10d % 1s ", fc.size(), fc.flags())
        fm.data(abspath=f)
        fm.write("path", fmt, m.rel(f))
        ret = 0

    return ret


def eden_files(ui, ctx, m, fm, fmt):
    # The default files() function code looks up the dirstate entry for ever
    # single matched file.  This is unnecessary in most cases, and will trigger
    # a lot of thrift calls to Eden.  We have augmented the Eden dirstate with
    # a function that can return only non-removed files without requiring
    # looking up every single match.
    ret = 1
    ds = ctx.repo().dirstate
    for f in sorted(ds.non_removed_matches(m)):
        fm.startitem()
        if ui.verbose:
            fc = ctx[f]
            fm.write("size flags", "% 10d % 1s ", fc.size(), fc.flags())
        fm.data(abspath=f)
        fm.write("path", fmt, m.rel(f))
        ret = 0

    return ret


def remove(ui, repo, m, mark, force, warnings=None):
    ret = 0
    clean = force or not mark
    s = repo.status(match=m, clean=clean)
    modified, added, deleted, clean = s[0], s[1], s[3], s[6]

    wctx = repo[None]

    if warnings is None:
        warnings = []
        warn = True
    else:
        warn = False

    # warn about failure to delete explicit files/dirs
    deleteddirs = util.dirs(deleted)
    files = m.files()
    with progress.bar(ui, _("deleting"), _("files"), len(files)) as prog:
        for f in files:
            prog.value += 1
            isdir = f in deleteddirs or wctx.hasdir(f)
            if f in repo.dirstate or isdir or f == "":
                continue

            if repo.wvfs.exists(f):
                if repo.wvfs.isdir(f):
                    warnings.append(_("not removing %s: no tracked files\n") % m.rel(f))
                else:
                    warnings.append(
                        _("not removing %s: file is untracked\n") % m.rel(f)
                    )
            # missing files will generate a warning elsewhere
            ret = 1

    if force:
        file_list = modified + deleted + clean + added
    elif mark:
        file_list = deleted
        # For performance, "remaining" only lists "exact" matches.
        # In theory it should also list "clean" files but that's too expensive
        # for a large repo.
        remaining = set(files) - set(deleted) - set(s.removed)
        for f in sorted(remaining):
            if repo.wvfs.exists(f) and not repo.wvfs.isdir(f):
                warnings.append(_("not removing %s: file still exists\n") % m.rel(f))
                ret = 1
    else:
        file_list = deleted + clean
        total = len(modified) + len(added)
        with progress.bar(ui, _("skipping"), _("files"), total) as prog:
            for f in modified:
                prog.value += 1
                warnings.append(
                    _("not removing %s: file is modified (use -f to force removal)\n")
                    % m.rel(f)
                )
                ret = 1
            for f in added:
                prog.value += 1
                warnings.append(
                    _(
                        "not removing %s: file has been marked for"
                        " add (use '@prog@ forget' to undo add)\n"
                    )
                    % m.rel(f)
                )
                ret = 1

    file_list = sorted(file_list)
    added = set(added)
    with repo.wlock():
        with progress.bar(ui, _("deleting"), _("files"), len(file_list)) as prog:
            for i, f in enumerate(file_list, 1):
                prog.value = i
                if ui.verbose or not m.exact(f):
                    ui.status(_("removing %s\n") % m.rel(f))
                if not mark:
                    if f in added:
                        continue  # we never unlink added files on remove
                    repo.wvfs.unlinkpath(f, ignoremissing=True)
        repo[None].forget(file_list)

    if warn:
        for warning in warnings:
            ui.warn(warning)

    return ret


def cat(ui, repo, ctx, matcher, basefm, fntemplate, prefix, **opts):
    err = 1

    def write(path):
        filename = None
        if fntemplate:
            filename = makefilename(
                repo, fntemplate, ctx.node(), pathname=os.path.join(prefix, path)
            )
            # attempt to create the directory if it does not already exist
            try:
                os.makedirs(os.path.dirname(filename))
            except OSError:
                pass
        with formatter.maybereopen(basefm, filename, opts) as fm:
            data = ctx[path].data()
            fm.startitem()
            fm.writebytes("data", b"%s", data)
            fm.data(abspath=path, path=matcher.rel(path))

    for abs in ctx.walk(matcher):
        write(abs)
        err = 0

    return err


def commit(ui, repo, commitfunc, pats, opts):
    """commit the specified files or all outstanding changes"""
    date = opts.get("date")
    if date:
        opts["date"] = util.parsedate(date)
    message = logmessage(repo, opts)
    matcher = scmutil.match(repo[None], pats, opts)

    dsguard = None
    # extract addremove carefully -- this function can be called from a command
    # that doesn't support addremove
    addremove = opts.get("addremove")
    automv = opts.get("automv")
    if addremove or automv:
        dsguard = dirstateguard.dirstateguard(repo, "commit")
    with dsguard or util.nullcontextmanager():
        if dsguard:
            if (
                scmutil.addremove(repo, matcher, addremove=addremove, automv=automv)
                != 0
            ):
                raise error.Abort(
                    _("failed to mark all new/missing files as added/removed")
                )

        return commitfunc(ui, repo, message, matcher, opts)


def samefile(f, ctx1, ctx2, m1=None, m2=None):
    if m1 is None:
        m1 = ctx1.manifest()
    if m2 is None:
        m2 = ctx2.manifest()
    if f in m1:
        a = ctx1.filectx(f)
        if f in m2:
            b = ctx2.filectx(f)
            return not a.cmp(b) and a.flags() == b.flags()
        else:
            return False
    else:
        return f not in m2


def amend(ui, repo, old, extra, pats, opts):
    wctx = repo[None]
    matcher = scmutil.match(wctx, pats, opts)

    amend_copies = copies.collect_amend_copies(repo.ui, wctx, old, matcher)

    node = _amend(ui, repo, wctx, old, extra, opts, matcher)

    copies.record_amend_copies(repo, amend_copies, old, repo[node])

    return node


def _amend(ui, repo, wctx, old, extra, opts, matcher):
    ui.note(_("amending changeset %s\n") % old)
    base = old.p1()

    with repo.wlock(), repo.lock(), repo.transaction("amend"):
        # Participating changesets:
        #
        # wctx     o - workingctx that contains changes from working copy
        #          |   to go into amending commit
        #          |
        # old      o - changeset to amend
        #          |
        # base     o - first parent of the changeset to amend

        # Copy to avoid mutating input
        extra = extra.copy()
        # Update extra dict from amended commit (e.g. to preserve graft
        # source)
        extra.update(old.extra())

        # Also update it from the from the wctx
        extra.update(wctx.extra())

        user = opts.get("user") or old.user()
        date = opts.get("date") or old.date()

        # Parse the date to allow comparison between date and old.date()
        date = util.parsedate(date)

        if len(old.parents()) > 1:
            # ctx.files() isn't reliable for merges, so fall back to the
            # slower repo.status() method
            files = {fn for st in repo.status(base, old)[:3] for fn in st}
        else:
            files = set(old.visiblefiles())

        # add/remove the files to the working copy if the "addremove" option
        # was specified.
        addremove = opts.get("addremove")
        automv = opts.get("automv")
        if (addremove or automv) and scmutil.addremove(
            repo,
            matcher,
            addremove=addremove,
            automv=automv,
        ):
            raise error.Abort(
                _("failed to mark all new/missing files as added/removed")
            )

        ms = mergemod.mergestate.read(repo)
        mergeutil.checkunresolved(ms)

        status = repo.status(match=matcher)
        filestoamend = set(status.modified + status.added + status.removed)

        changes = len(filestoamend) > 0
        if changes:
            if mergemod.is_noconflict_merge(old):
                raise error.Abort(
                    _(
                        "cannot amend conflict-free merge %s: it must stay the "
                        "automatic merge of its parents"
                    )
                    % old,
                    hint=_("commit the change on top of it instead"),
                )
            # Recompute copies (avoid recording a -> b -> a)
            copied = copies.pathcopies(base, wctx, matcher)
            if old.p2:
                copied.update(copies.pathcopies(old.p2(), wctx, matcher))

            # Prune files which were reverted by the updates: if old
            # introduced file X and the file was renamed in the working
            # copy, then those two files are the same and
            # we can discard X from our list of files. Likewise if X
            # was removed, it's no longer relevant. If X is missing (aka
            # deleted), old X must be preserved.
            with perftrace.trace("Prune files reverted by amend"):
                statusmanifest = wctx.buildstatusmanifest(status)

                if "remotefilelog" in repo.requirements:
                    # Prefetch files in `base` to avoid serial lookups.
                    fileids = base.manifest().walkfiles(
                        matchmod.exact("", "", status.modified + status.added)
                    )
                    repo.fileslog.filestore.prefetch(fileids)
                    repo.fileslog.metadatastore.prefetch(fileids, length=1)

                for f in sorted(filestoamend):
                    if f in status.deleted or not samefile(
                        f, wctx, base, m1=statusmanifest
                    ):
                        files.add(f)
                    else:
                        files.discard(f)
                files = list(files)

            def filectxfn(repo, ctx_, path):
                try:
                    # If the file being considered is not amongst the files
                    # to be amended, we should return the file context from the
                    # old changeset. This avoids issues when only some files in
                    # the working copy are being amended but there are also
                    # changes to other files from the old changeset.
                    if path not in filestoamend:
                        return old.filectx(path)

                    # Return None for removed files.
                    if path in status.removed:
                        return None

                    fctx = wctx[path]
                except KeyError:
                    return None
                else:
                    c = copied.get(path, False)
                    return context.overlayfilectx(fctx, ctx=ctx_, copied=c)

        else:
            ui.note(_("copying changeset %s to %s\n") % (old, base))

            # Use version of files as in the old cset
            def filectxfn(repo, ctx_, path):
                try:
                    return old.filectx(path)
                except KeyError:
                    return None

        # See if we got a message from -m or -l, if not, open the editor with
        # the message of the changeset to amend.
        message = logmessage(repo, opts)

        editform = mergeeditform(old, "commit.amend")
        editor = getcommiteditor(editform=editform, **opts)

        if not message:
            editor = getcommiteditor(edit=True, editform=editform)
            message = old.description()

        pureextra = extra.copy()
        extra["amend_source"] = old.hex()
        mutinfo = mutation.record(repo, extra, [old.node()], "amend")

        loginfo = {
            "checkoutidentifier": repo.dirstate.checkoutidentifier,
            "predecessors": old.hex(),
            "mutation": "amend",
        }

        new = context.memctx(
            repo,
            parents=[base, old.p2()],
            text=message,
            files=files,
            filectxfn=filectxfn,
            user=user,
            date=date,
            extra=extra,
            editor=editor,
            loginfo=loginfo,
            mutinfo=mutinfo,
        )

        newdesc = changelog.stripdesc(new.description())
        if (
            (not changes)
            and newdesc == old.description()
            and user == old.user()
            and date == old.date()
            and pureextra == old.extra()
        ):
            # nothing changed. continuing here would create a new node
            # anyway because of the mutation or amend_source data.
            #
            # This not what we expect from amend.
            return old.node()

        if opts.get("secret"):
            commitphase = "secret"
        else:
            commitphase = old.phase()
        overrides = {("phases", "new-commit"): commitphase}
        with ui.configoverride(overrides, "amend"):
            newid = repo.commitctx(new)

        # Reroute the working copy parent to the new changeset
        repo.setparents(newid, nullid)
        mapping = {old.node(): (newid,)}
        obsmetadata = None
        if opts.get("note"):
            obsmetadata = {"note": opts["note"]}
        scmutil.cleanupnodes(repo, mapping, "amend", metadata=obsmetadata)

        # Fixing the dirstate because localrepo.commitctx does not update
        # it. This is rather convenient because we did not need to update
        # the dirstate for all the files in the new commit which commitctx
        # could have done if it updated the dirstate. Now, we can
        # selectively update the dirstate only for the amended files.
        dirstate = repo.dirstate

        # Update the state of the files which were added and
        # and modified in the amend to "normal" in the dirstate.
        normalfiles = set(status.modified + status.added) & filestoamend
        for f in normalfiles:
            wcoverlay.marknormal(repo, f)

        # Update the state of files which were removed in the amend
        # to "removed" in the dirstate.
        removedfiles = set(status.removed) & filestoamend
        for f in removedfiles:
            dirstate.untrack(f)

    return newid


def commiteditor(repo, ctx, editform="", summaryfooter=""):
    if description := ctx.description():
        return add_summary_footer(repo.ui, description, summaryfooter)

    return commitforceeditor(
        repo,
        ctx,
        editform=editform,
        unchangedmessagedetection=True,
        summaryfooter=summaryfooter,
    )


def add_summary_footer(
    ui,
    commit_msg: str,
    summary_footer: str,
) -> str:
    """
    >>> class FakeUI:
    ...   def configlist(self, section, key):
    ...     assert section == "committemplate" and key == "commit-message-fields"
    ...     return ["Summary", "Test Plan"]
    ...
    ...   def config(self, section, key):
    ...     assert section == "committemplate" and key == "summary-field"
    ...     return "Summary"
    ...

    >>> ui = FakeUI()
    >>> print(add_summary_footer(ui, "", "i am a summary footer"))
    <BLANKLINE>
    i am a summary footer

    >>> print(add_summary_footer(ui, "this is a title", ""))
    this is a title

    >>> print(add_summary_footer(ui, "this is a title", "i am a summary footer"))
    this is a title
    <BLANKLINE>
    i am a summary footer

    >>> print(add_summary_footer(
    ...   ui,
    ...   "this is a title\\n\\nSummary: I am a summary",
    ...   "i am a summary footer"
    ... ))
    this is a title
    <BLANKLINE>
    Summary: I am a summary
    <BLANKLINE>
    i am a summary footer

    >>> print(add_summary_footer(
    ...   ui,
    ...   "this is a title\\n\\nSummary: I am a summary\\n\\nTest Plan: I am a test plan",
    ...   "i am a summary footer"
    ... ))
    this is a title
    <BLANKLINE>
    Summary: I am a summary
    <BLANKLINE>
    i am a summary footer
    <BLANKLINE>
    Test Plan: I am a test plan

    >>> print(add_summary_footer(
    ...   ui,
    ...   "this is a title\\n\\nTest Plan: I am a test plan",
    ...   "i am a summary footer"
    ... ))
    this is a title
    <BLANKLINE>
    i am a summary footer
    <BLANKLINE>
    Test Plan: I am a test plan
    """

    def get_lines(field_content_list):
        return [line for (_field, content) in field_content_list for line in content]

    if not summary_footer:
        return commit_msg

    commit_fields = set(ui.configlist("committemplate", "commit-message-fields"))
    summary_field = ui.config("committemplate", "summary-field")

    lines = commit_msg.split("\n")
    field_content_list = _parse_commit_message(lines, commit_fields)

    # defaults to the last field
    summary_index = len(field_content_list) - 1
    summary_content = field_content_list[summary_index][1]
    for i, (field, content) in enumerate(field_content_list):
        if field == summary_field:
            summary_index, summary_content = i, content
            break
        elif field is not None:
            summary_index = i - 1
            if summary_index >= 0:
                summary_content = field_content_list[summary_index][1]
            else:
                summary_content = [""]

    if summary_content[-1]:
        summary_content.append("")
    summary_content.append(summary_footer)
    if summary_index < len(field_content_list) - 1:
        summary_content.append("")

    new_lines = (
        get_lines(field_content_list[:summary_index])
        + summary_content
        + get_lines(field_content_list[summary_index + 1 :])
    )
    return "\n".join(new_lines)


def extract_summary(ui, message: str) -> str:
    """Extract summary (including title) of a commit message.

    >>> class FakeUI:
    ...   def configlist(self, section, key):
    ...     assert section == "committemplate" and key == "commit-message-fields"
    ...     return ["Summary", "Test Plan"]
    ...
    ...   def config(self, section, key):
    ...     assert section == "committemplate" and key == "summary-field"
    ...     return "Summary"
    ...

    >>> ui = FakeUI()

    >>> print(extract_summary(ui, "this is a title"))
    this is a title

    >>> print(extract_summary(
    ...   ui,
    ...   "this is a title\\n\\nSummary: I am a summary"
    ... ))
    this is a title
    <BLANKLINE>
    Summary: I am a summary

    >>> print(extract_summary(
    ...   ui,
    ...   "this is a title\\n\\nSummary: I am a summary\\n\\nTest Plan: I am a test plan",
    ... ))
    this is a title
    <BLANKLINE>
    Summary: I am a summary

    >>> print(extract_summary(
    ...   ui,
    ...   "this is a title\\n\\nTest Plan: I am a test plan",
    ... ))
    this is a title
    """
    commit_fields = set(ui.configlist("committemplate", "commit-message-fields"))
    summary_field = ui.config("committemplate", "summary-field")
    lines = message.split("\n")
    field_content_list = _parse_commit_message(lines, commit_fields)

    new_lines = []
    for field, content in field_content_list:
        if field is None:
            new_lines.extend(content)
        else:
            if field == summary_field:
                new_lines.extend(content)
            break
    while new_lines and not new_lines[-1]:
        new_lines.pop()
    return "\n".join(new_lines)


def _parse_commit_message(
    lines: List[str], commit_fields: Set[str]
) -> List[Tuple[Optional[str], List[str]]]:
    """Parse commit message lines and return list of (field, content_lines) pairs.

    >>> _parse_commit_message(["this is a title"], {"Summary"})
    [(None, ['this is a title'])]
    >>> _parse_commit_message(
    ...   ['this is a title', '', 'Summary: I am a summary'],
    ...   {'Summary', 'Test Plan'},
    ... )
    [(None, ['this is a title', '']), ('Summary', ['Summary: I am a summary'])]
    >>> _parse_commit_message(
    ...   ["this is a title", "", "Summary: I am a summary", "", "Test Plan: I am a test plan"],
    ...   {"Summary", "Test Plan"},
    ... )
    [(None, ['this is a title', '']), ('Summary', ['Summary: I am a summary', '']), ('Test Plan', ['Test Plan: I am a test plan'])]
    """
    result = []
    curr_key, curr_content = None, []
    for line in lines:
        try:
            key = line[: line.index(":")]
        except ValueError:
            # not found ":"
            curr_content.append(line)
            continue

        if key in commit_fields:
            if curr_content:
                result.append((curr_key, curr_content))
            curr_key, curr_content = key, [line]
        else:
            curr_content.append(line)

    if curr_content:
        result.append((curr_key, curr_content))
    return result


def commitforceeditor(
    repo,
    ctx,
    editform="",
    unchangedmessagedetection=False,
    summaryfooter="",
):
    forms = [e for e in editform.split(".") if e]
    forms.insert(0, "changeset")
    templatetext = None
    while forms:
        ref = ".".join(forms)
        if repo.ui.config("committemplate", ref):
            templatetext = committext = buildcommittemplate(
                repo, ctx, ref, summaryfooter
            )
            break
        forms.pop()
    else:
        committext = buildcommittext(repo, ctx, summaryfooter)

    # run editor in the repository root
    olddir = os.getcwd()
    os.chdir(repo.root)

    # make in-memory changes visible to external process
    tr = repo.currenttransaction()
    repo.dirstate.write(tr)
    pendingenv = {}
    if tr:
        tr.writepending(env=pendingenv)

    editortext = repo.ui.edit(
        committext,
        ctx.user(),
        ctx.extra(),
        editform=editform,
        env=pendingenv,
        repopath=repo.path,
        action="commit",
    )
    text = editortext

    all_prefixes = "|".join(
        ident.cliname().upper() for ident in bindings.identity.all()
    )

    # strip away anything below this special string (used for editors that want
    # to display the diff)
    linebelow = (
        f"^(?:{all_prefixes}): ------------------------ >8 ------------------------$"
    )

    stripbelow = re.search(linebelow, text, flags=re.MULTILINE)
    if stripbelow:
        text = text[: stripbelow.start()]

    text = re.sub(f"(?m)^({all_prefixes}):.*(\n|$)", "", text)
    os.chdir(olddir)

    if not text.strip():
        raise error.Abort(_("empty commit message"))
    if unchangedmessagedetection and editortext == templatetext:
        raise error.Abort(_("commit message unchanged"))

    return text


def buildcommittemplate(repo, ctx, ref, summaryfooter=""):
    ui = repo.ui
    spec = formatter.templatespec(ref, None, None)
    t = changeset_templater(ui, repo, spec, None, {}, False)
    t.t.cache.update(
        (k, templater.unquotestring(v))
        for k, v in repo.ui.configitems("committemplate")
    )

    if summaryfooter:
        t.t.cache.update({"summaryfooter": summaryfooter})

    # load extra aliases based on changed files
    # Note: this feature allows executing arbitrary templates controlled by the
    # repo content. Do not enable for untrusted repos!
    if repo.ui.configbool("experimental", "local-committemplate"):
        localtemplate = localcommittemplate(repo, ctx)
        t.t.cache.update((k, templater.unquotestring(v)) for k, v in localtemplate)

    ui.pushbuffer()
    t.show(ctx)
    return ui.popbufferbytes().decode(errors="replace")


def localcommittemplate(repo, ctx):
    """return [(k, v)], local [committemplate] config based on changed files.

    The local commit template only works for working copy ctx.

    The local commit template is decided in these steps:
    - First, calculate the common prefix of the changed files.
      For example, the common prefix of a/x/1, a/x/2/3 is a/x.
    - Then, find the config file, starting with prefix/.committemplate,
      recursively look at parent directories until repo root.
      For the above example, check a/x/.committemplate, then a/.committemplate,
      then .committemplate from repo root.
    - Load the config file. The format of the config file is ``key = value``,
      similar to config files but without the [committemplate] header.
    """

    if ctx.node() is None:
        files = ctx.files()
        prefix = os.path.commonpath(files) if files else ""
        wvfs = repo.wvfs
        while True:
            config_path = prefix + (prefix and "/" or "") + ".committemplate"
            if wvfs.isdir(prefix) and wvfs.exists(config_path):
                cfg = bindings.configloader.config()
                content = wvfs.tryreadutf8(config_path)
                cfg.parse(content, source=config_path)
                section = ""
                names = cfg.names(section)
                return [(name, cfg.get(section, name)) for name in names]
            next_prefix = os.path.dirname(prefix)
            if next_prefix == prefix:
                break
            prefix = next_prefix

    return []


def hgprefix(msg):
    return "\n".join([f"{identity.tmplprefix()}: {a}" for a in msg.split("\n") if a])


def buildcommittext(repo, ctx, summaryfooter=""):
    edittext = []
    modified, added, removed = ctx.modified(), ctx.added(), ctx.removed()
    description = ctx.description()
    edittext.append(add_summary_footer(repo.ui, description, summaryfooter))
    if edittext[-1]:
        edittext.append("")
    edittext.append("")  # Empty line between message and comments.
    edittext.append(
        hgprefix(
            _(
                "Enter commit message."
                f"  Lines beginning with '{identity.tmplprefix()}:' are removed."
            )
        )
    )
    edittext.append(hgprefix(_("Leave message empty to abort commit.")))
    edittext.append(f"{identity.tmplprefix()}: --")
    edittext.append(hgprefix(_("user: %s") % ctx.user()))
    if ctx.p2():
        edittext.append(hgprefix(_("branch merge")))
    if bookmarks.isactivewdirparent(repo):
        edittext.append(hgprefix(_("bookmark '%s'") % repo._activebookmark))
    edittext.extend([hgprefix(_("added %s") % f) for f in added])
    edittext.extend([hgprefix(_("changed %s") % f) for f in modified])
    edittext.extend([hgprefix(_("removed %s") % f) for f in removed])
    if not added and not modified and not removed:
        edittext.append(hgprefix(_("no files changed")))
    edittext.append("")

    return "\n".join(edittext)


def commitstatus(repo, node, opts=None, predecessor=None):
    ui = repo.ui
    if opts is None:
        opts = {}
    ctx = repo[node]
    show_status = ui.configbool("commit", "show-status", True)
    # ui.quiet is false when debug output is enabled, so inspect the raw option.
    quiet = ui.configbool("ui", "quiet")

    if ui.debugflag:
        # --debug always reports at least the legacy full-hash line, even in
        # otherwise suppressed modes.
        if quiet or ui.plain() or not show_status:
            ui.write(_("committed %s\n") % ctx.hex())
            return
    elif quiet or (ui.plain() and not ui.verbose):
        return
    elif not show_status:
        if ui.verbose:
            ui.write(_("committed %s\n") % ctx)
        return

    if predecessor is not None:
        repo.ui.write(_("amended %s -> %s\n") % (predecessor.hex(), ctx.hex()))
        status = repo.status(predecessor, ctx)
        files = status.modified + status.added + status.removed
    else:
        repo.ui.write(_("committed %s\n") % (ctx.hex()))
        files = ctx.files()

    if files:
        limit = max(1, repo.ui.configint("commit", "status-path-limit", 5))
        repo.ui.write(_("changed %d file(s):\n") % len(files))
        for path in templater.summarizepaths(files, limit):
            repo.ui.write("  %s\n" % path)


def postcommitstatus(repo, pats, opts):
    return repo.status(match=scmutil.match(repo[None], pats, opts))


def revert(ui, repo, ctx, parents, *pats, match=None, **opts):
    if match and (pats or opts.get("include") or opts.get("exclude")):
        raise error.ProgrammingError(
            "revert: match and patterns are mutually exclusive"
        )

    parent, p2 = parents
    node = ctx.node()

    mf = ctx.manifest()
    if node == p2:
        parent = p2

    # need all matching names in dirstate and manifest of target rev,
    # so have to walk both. do not print errors if files exist in one
    # but not other. in both cases, filesets should be evaluated against
    # workingctx to get consistent result (issue4497). this means 'set:**'
    # cannot be used to select missing files from target rev.

    # `names` is a mapping for all elements in working copy and target revision
    # The mapping is in the form:
    #   <asb path in repo> -> (<path from CWD>, <exactly specified by matcher?>)
    names = {}

    with repo.wlock():
        ## filling of the `names` mapping
        # walk dirstate to fill `names`

        interactive = opts.get("interactive", False)
        wctx = repo[None]
        m = match or scmutil.match(wctx, pats, opts)

        # we'll need this later
        badfiles = set()

        def badfn(path, msg):
            # We only report errors about paths that do not exist in the original
            # node.
            #
            # For other errors we normally will successfully revert the file anyway.
            # This includes situations where the file was replaced by an unsupported
            # file type (e.g., a FIFO, socket, or device node.
            if path not in ctx:
                badfiles.add(path)
                msg = _("no such file in rev %s") % short(node)
                ui.warn("%s: %s\n" % (m.rel(path), msg))

        changes = repo.status(node1=node, match=matchmod.badmatch(m, badfn))
        for kind in changes:
            for abs in kind:
                names[abs] = m.rel(abs), m.exact(abs)

        # Look for exact filename matches that were not returned in the results.
        # These will not be returned if they are clean, but we want to include them
        # to report them as not needing changes.
        for abs in m.files():
            if abs in names or abs in badfiles:
                continue
            # Check to see if this looks like a file or directory.
            # We don't need to report directories in the clean list.
            try:
                st = repo.wvfs.lstat(abs)
                if stat.S_ISDIR(st.st_mode):
                    continue
            except OSError:
                # This is can occur if the file was locally removed and is
                # untracked, and did not exist in the node we are reverting from,
                # but does exist in the current commit.
                # Continue on and report this file as clean.
                pass

            names[abs] = m.rel(abs), m.exact(abs)
            if abs in wctx:
                changes.clean.append(abs)
            else:
                # We don't really know if this file is unknown or ignored, but
                # fortunately this does not matter.  Revert treats unknown and
                # ignored exactly files the same.
                changes.unknown.append(abs)

        modified = set(changes.modified)
        added = set(changes.added)
        removed = set(changes.removed)
        _deleted = set(changes.deleted)
        unknown = set(changes.unknown)
        unknown.update(changes.ignored)
        clean = set(changes.clean)
        modadded = set()

        # We need to account for the state of the file in the dirstate,
        # even when we revert against something else than parent. This will
        # slightly alter the behavior of revert (doing back up or not, delete
        # or just forget etc).
        if parent == node:
            dsmodified = modified
            dsadded = added
            dsremoved = removed
            # store all local modifications, useful later for rename detection
            localchanges = dsmodified | dsadded
            modified, added, removed = set(), set(), set()
        else:
            exactfilesmatch = scmutil.matchfiles(repo, names)
            changes = repo.status(node1=parent, match=exactfilesmatch)
            dsmodified = set(changes.modified)
            dsadded = set(changes.added)
            dsremoved = set(changes.removed)
            # store all local modifications, useful later for rename detection
            localchanges = dsmodified | dsadded

            # only take into account for removes between wc and target
            clean |= dsremoved - removed
            dsremoved &= removed
            # distinct between dirstate remove and other
            removed -= dsremoved

            modadded = added & dsmodified
            added -= modadded

            # tell newly modified apart.
            dsmodified &= modified
            dsmodified |= modified & dsadded  # dirstate added may need backup
            modified -= dsmodified

            # We need to wait for some post-processing to update this set
            # before making the distinction. The dirstate will be used for
            # that purpose.
            dsadded = added

        # in case of merge, files that are actually added can be reported as
        # modified, we need to post process the result
        if p2 != nullid:
            mergeadd = set(dsmodified)
            for path in dsmodified:
                if path in mf:
                    mergeadd.remove(path)
            dsadded |= mergeadd
            dsmodified -= mergeadd

        # if f is a rename, update `names` to also revert the source
        cwd = repo.getcwd()
        for f in localchanges:
            src = repo.dirstate.copied(f)
            # XXX should we check for rename down to target node?
            if src and src not in names and repo.dirstate[src] == "r":
                dsremoved.add(src)
                names[src] = (repo.pathto(src, cwd), True)

        # determine the exact nature of the deleted changesets
        deladded = set(_deleted)
        for path in _deleted:
            if path in mf:
                deladded.remove(path)
        deleted = _deleted - deladded

        # distinguish between file to forget and the other
        added = set()
        for abs in dsadded:
            if repo.dirstate[abs] != "a":
                added.add(abs)
        dsadded -= added

        for abs in deladded:
            if repo.dirstate[abs] == "a":
                dsadded.add(abs)
        deladded -= dsadded

        # For files marked as removed, we check if an unknown file is present at
        # the same path. If a such file exists it may need to be backed up.
        # Making the distinction at this stage helps have simpler backup
        # logic.
        removunk = set()
        for abs in removed:
            target = repo.wjoin(abs)
            if os.path.lexists(target):
                removunk.add(abs)
        removed -= removunk

        dsremovunk = set()
        for abs in dsremoved:
            target = repo.wjoin(abs)
            if os.path.lexists(target):
                dsremovunk.add(abs)
        dsremoved -= dsremovunk

        # action to be actually performed by revert
        # (<list of file>, message>) tuple
        actions = {
            "revert": ([], _("reverting %s\n")),
            "add": ([], _("adding %s\n")),
            "remove": ([], _("removing %s\n")),
            "drop": ([], _("removing %s\n")),
            "forget": ([], _("forgetting %s\n")),
            "undelete": ([], _("undeleting %s\n")),
            "noop": (None, _("no changes needed to %s\n")),
            "unknown": (None, _("file not managed: %s\n")),
        }

        # "constant" that convey the backup strategy.
        # All set to `discard` if `no-backup` is set do avoid checking
        # no_backup lower in the code.
        # These values are ordered for comparison purposes
        backupinteractive = 3  # do backup if interactively modified
        backup = 2  # unconditionally do backup
        check = 1  # check if the existing file differs from target
        discard = 0  # never do backup
        if opts.get("no_backup"):
            backupinteractive = backup = check = discard
        if interactive:
            dsmodifiedbackup = backupinteractive
        else:
            dsmodifiedbackup = backup
        tobackup = set()

        backupanddel = actions["remove"]
        if not opts.get("no_backup"):
            backupanddel = actions["drop"]

        disptable = (
            # dispatch table:
            #   file state
            #   action
            #   make backup
            ## Sets that results that will change file on disk
            # Modified compared to target, no local change
            (modified, actions["revert"], discard),
            # Modified compared to target, but local file is deleted
            (deleted, actions["revert"], discard),
            # Modified compared to target, local change
            (dsmodified, actions["revert"], dsmodifiedbackup),
            # Added since target
            (added, actions["remove"], discard),
            # Added in working directory
            (dsadded, actions["forget"], discard),
            # Added since target, have local modification
            (modadded, backupanddel, backup),
            # Added since target but file is missing in working directory
            (deladded, actions["drop"], discard),
            # Removed since  target, before working copy parent
            (removed, actions["add"], discard),
            # Same as `removed` but an unknown file exists at the same path
            (removunk, actions["add"], check),
            # Removed since targe, marked as such in working copy parent
            (dsremoved, actions["undelete"], discard),
            # Same as `dsremoved` but an unknown file exists at the same path
            (dsremovunk, actions["undelete"], check),
            ## the following sets does not result in any file changes
            # File with no modification
            (clean, actions["noop"], discard),
            # Existing file, not tracked anywhere
            (unknown, actions["unknown"], discard),
        )

        quiet = ui.quiet
        for abs, (rel, exact) in sorted(names.items()):
            # target file to be touch on disk (relative to cwd)
            target = repo.wjoin(abs)
            # search the entry in the dispatch table.
            # if the file is in any of these sets, it was touched in the working
            # directory parent and we are sure it needs to be reverted.
            for table, (xlist, msg), dobackup in disptable:
                if abs not in table:
                    continue
                if xlist is not None:
                    xlist.append(abs)
                    if dobackup:
                        # If in interactive mode, don't automatically create
                        # .orig files (issue4793)
                        if dobackup == backupinteractive:
                            tobackup.add(abs)
                        elif (backup <= dobackup or wctx[abs].cmp(ctx[abs])) and wctx[
                            abs
                        ].flags() != "m":
                            bakname = scmutil.origpath(ui, repo, rel)
                            ui.note(
                                _("saving current version of %s as %s\n")
                                % (rel, bakname)
                            )
                            if not opts.get("dry_run"):
                                # Don't backup symlinks, since they can
                                # interfere with future backup paths that
                                # overlap with the symlink path (like
                                # accidentally trying to move something
                                # into the symlink).
                                if not os.path.islink(target):
                                    if interactive:
                                        util.copyfile(target, bakname)
                                    else:
                                        util.rename(target, bakname)
                    if ui.verbose or not exact:
                        if not isinstance(msg, str):
                            msg = msg(abs)
                        ui.status(msg % rel)
                elif exact and not quiet:
                    ui.warn(msg % rel)
                break

        if not opts.get("dry_run"):
            needdata = ("revert", "add", "undelete")
            _revertprefetch(repo, ctx, *[actions[name][0] for name in needdata])
            _performrevert(
                repo,
                parents,
                ctx,
                actions,
                interactive,
                tobackup,
            )


def _revertprefetch(repo, ctx, *files):
    """Let extension changing the storage layer prefetch content"""


def _performrevert(
    repo,
    parents,
    ctx,
    actions,
    interactive=False,
    tobackup=None,
):
    """function that actually perform all the actions computed for revert

    This is an independent function to let extension to plug in and react to
    the imminent revert.

    Make sure you have the working directory locked when calling this function.
    """
    parent, p2 = parents
    node = ctx.node()
    excluded_files = []
    matcher_opts = {"exclude": excluded_files}
    wctx = repo[None]

    def checkout(f):
        fc = ctx[f]
        if fc.flags() == "m":
            # f is a submodule, need special path to change
            git.submodulecheckout(ctx, match=lambda p: p == f, force=True)
            return
        wctx[f].clearunknown()
        repo.wwrite(f, fc.data(), fc.flags())

    def doremove(f):
        try:
            repo.wvfs.unlinkpath(f)
        except OSError:
            pass
        repo.dirstate.remove(f)

    audit_path = pathutil.pathauditor(repo.root, cached=True)
    for f in actions["forget"][0]:
        if interactive:
            choice = repo.ui.promptchoice(
                _("forget added file %s (Yn)?$$ &Yes $$ &No") % f
            )
            if choice == 0:
                repo.dirstate.untrack(f)
            else:
                excluded_files.append(repo.wjoin(f))
        else:
            repo.dirstate.untrack(f)
    for f in actions["remove"][0]:
        audit_path(f)
        if interactive:
            choice = repo.ui.promptchoice(
                _("remove added file %s (Yn)?$$ &Yes $$ &No") % f
            )
            if choice == 0:
                doremove(f)
            else:
                excluded_files.append(repo.wjoin(f))
        else:
            doremove(f)
    for f in actions["drop"][0]:
        audit_path(f)
        repo.dirstate.remove(f)

    # By default, use "normallookup" so the file is marked as
    # "need content check" for the next "status" run. The only fast path is
    # when reverting to the non-merge working parent, where the file can be
    # marked as clean.
    normal = repo.dirstate.normallookup
    if node == parent and p2 == nullid:
        # We're reverting to our parent. If possible, we'd like status
        # to report the file as clean. We have to use normallookup for
        # merges to avoid losing information about merged/dirty files.
        normal = repo.dirstate.normal

    newlyaddedandmodifiedfiles = set()
    if interactive:
        # Prompt the user for changes to revert
        torevert = [repo.wjoin(f) for f in actions["revert"][0]]
        m = scmutil.match(ctx, torevert, matcher_opts)
        diffopts = patch.difffeatureopts(repo.ui, whitespace=True)
        diffopts.nodates = True
        diffopts.git = True
        operation = "discard"
        reversehunks = True
        if node != parent:
            operation = "apply"
            reversehunks = False
        if reversehunks:
            diff = patch.diff(repo, ctx, repo[None], m, opts=diffopts)
        else:
            diff = patch.diff(repo, repo[None], ctx, m, opts=diffopts)
        originalchunks = patch.parsepatch(diff)

        try:
            chunks, opts = recordfilter(repo.ui, originalchunks, operation=operation)
            if reversehunks:
                chunks = patch.reversehunks(chunks)

        except error.PatchError as err:
            raise error.Abort(_("error parsing patch: %s") % err)

        newlyaddedandmodifiedfiles = newandmodified(chunks, originalchunks)
        if tobackup is None:
            tobackup = set()
        # Apply changes
        fp = io.BytesIO()
        for c in chunks:
            # Create a backup file only if this hunk should be backed up
            if ishunk(c) and c.header.filename() in tobackup:
                abs = c.header.filename()
                target = repo.wjoin(abs)
                bakname = scmutil.origpath(repo.ui, repo, m.rel(abs))
                util.copyfile(target, bakname)
                tobackup.remove(abs)
            c.write(fp)
        dopatch = fp.tell()
        fp.seek(0)
        if dopatch:
            try:
                patch.internalpatch(repo.ui, repo, fp, 1, eolmode=None)
            except error.PatchError as err:
                raise error.Abort(str(err))
        del fp
    else:
        for f in actions["revert"][0]:
            checkout(f)
            normal(f)

    for f in actions["add"][0]:
        # Don't checkout modified files, they are already created by the diff
        if f not in newlyaddedandmodifiedfiles:
            checkout(f)
            repo.dirstate.add(f)

    for f in actions["undelete"][0]:
        checkout(f)
        normal(f)

    copied = copies.pathcopies(repo[parent], ctx)
    for f in actions["add"][0] + actions["undelete"][0] + actions["revert"][0]:
        if f in copied:
            repo.dirstate.copy(copied[f], f)


class command(registrar.command):
    """deprecated: used registrar.command instead"""

    def _doregister(self, func, name, *args, **kwargs):
        func._deprecatedregistrar = True  # flag for deprecwarn in extensions.py
        return super(command, self)._doregister(func, name, *args, **kwargs)


# a list of (ui, repo, otherpeer, opts, missing) functions called by
# commands.outgoing.  "missing" is "missing" of the result of
# "findcommonoutgoing()"
outgoinghooks = util.hooks()

# a list of (ui, repo) functions called by commands.summary
summaryhooks = util.hooks()

# a list of (ui, repo, opts, changes) functions called by commands.summary.
#
# functions should return tuple of booleans below, if 'changes' is None:
#  (whether-incomings-are-needed, whether-outgoings-are-needed)
#
# otherwise, 'changes' is a tuple of tuples below:
#  - (sourceurl, sourcebranch, sourcepeer, incoming)
#  - (desturl,   destbranch,   destpeer,   outgoing)
summaryremotehooks = util.hooks()


def checkunfinished(repo, op=None):
    """Look for an unfinished multistep operation, like graft, and abort
    if found. It's probably good to check this right before
    bailifchanged().
    """
    state = repo._rsrepo.workingcopy().commandstate(op)
    if state:
        raise error.Abort(state[0], hint=state[1])


afterresolvedstates = [
    ("graftstate", "@prog@ graft --continue"),
    ("updatemergestate", "@prog@ goto --continue"),
]


def howtocontinue(repo):
    """Check for an unfinished operation and return the command to finish
    it.

    afterresolvedstates tuples define a .hg/{file} and the corresponding
    command needed to finish it.

    Returns a (msg, warning) tuple. 'msg' is a string and 'warning' is
    a boolean.
    """
    contmsg = _("continue: %s")
    for f, msg in afterresolvedstates:
        if repo.localvfs.exists(f):
            return contmsg % _(msg), True
    if repo[None].dirty(missing=True, merge=False):
        return contmsg % _("@prog@ commit"), False
    return None, None


def checkafterresolved(repo):
    """Inform the user about the next action after completing hg resolve

    If there's a matching afterresolvedstates, howtocontinue will yield
    repo.ui.warn as the reporter.

    Otherwise, it will yield repo.ui.note.
    """
    msg, warning = howtocontinue(repo)
    if msg is not None:
        if warning:
            repo.ui.warn("%s\n" % msg)
        else:
            repo.ui.note("%s\n" % msg)


def wrongtooltocontinue(repo, task):
    """Raise an abort suggesting how to properly continue if there is an
    active task.

    Uses howtocontinue() to find the active task.

    If there's no task (repo.ui.note for 'hg commit'), it does not offer
    a hint.
    """
    after = howtocontinue(repo)
    hint = None
    if after[1]:
        hint = after[0]
    raise error.Abort(_("no %s in progress") % task, hint=hint)


diffgraftopts = [
    (
        "",
        "from-path",
        [],
        _("re-map this path to corresponding --to-path (ADVANCED)"),
        _("PATH"),
    ),
    (
        "",
        "to-path",
        [],
        _("re-map corresponding --from-path to this path (ADVANCED)"),
        _("PATH"),
    ),
]


def registerdiffgrafts(from_paths, to_paths, *ctxs, is_crossrepo=False):
    """Register --from-path/--to-path manifest "grafts" in ctx's manifest.

    These grafts are applied temporarily before diff operations, allowing users
    to "remap" directories.
    """
    if not ctxs:
        return error.ProgrammingError("registerdiffgrafts() requires ctxs")

    if not from_paths and not to_paths:
        return None

    subtreeutil.validate_path_size(from_paths, to_paths)
    subtreeutil.validate_path_overlap(from_paths, to_paths, is_crossrepo=is_crossrepo)

    for ctx in ctxs:
        manifest = ctx.manifest()
        for f, t in zip(from_paths, to_paths):
            manifest.registerdiffgraft(f, t)


def abort_on_unresolved_conflicts(ms):
    """Abort if there are unresolved conflicts in the merge state."""
    if ms.unresolvedcount() > 0:
        raise error.Abort(
            _("outstanding merge conflicts"),
            hint=_(
                "use '@prog@ resolve --list' to list, '@prog@ resolve --mark FILE' to mark resolved"
            ),
        )
