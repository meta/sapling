# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# undo.py: records data in revlog for future undo functionality


import bindings
from bindings import cliparser
from sapling import (
    cmdutil,
    commands,
    dispatch,
    encoding,
    error,
    extensions,
    hg,
    hintutil,
    identity,
    localrepo,
    lock as lockmod,
    merge,
    mutation,
    registrar,
    revlog,
    revset,
    revsetlang,
    smartset,
    templatekw,
    templater,
    transaction,
    util,
    visibility,
)
from sapling.i18n import _
from sapling.node import bin, hex, nullid, short

mpatch = bindings.cext.mpatch


if not util.iswindows:
    from . import interactiveui
else:
    interactiveui = None


cmdtable = {}
command = registrar.command(cmdtable)

hint = registrar.hint()


@hint("undo")
def hintundo():
    return _("you can undo this using the `@prog@ undo` command")


@hint("undo-uncommit-unamend")
def hintuncommit(command, oldhash):
    return _(
        "undoing %ss discards their changes.\n"
        "to restore the changes to the working copy, run '@prog@ revert -r %s --all'\n"
        "in the future, you can use '@prog@ un%s' instead of '@prog@ undo' to keep changes"
    ) % (command, oldhash, command)


@hint("undo-corrupt")
def hintundocorrupt(path=None):
    msg = _("undo history is corrupted\n")
    msg += _("(try deleting %s to recover)\n") % (
        path or "%s/undolog" % identity.default().dotdir()
    )
    return msg


# Setup


def extsetup(ui):
    extensions.wrapfunction(dispatch, "runcommand", _runcommandwrapper)

    # undo has its own locking, add itself to bypass repo lock audit
    localrepo.localrepository._wlockfreeprefix.add("undolog/")


# Wrappers

# Track pid of _runcommandwrapper caller. This is so we can avoid nested undo
# activity without unwrapping "runcommand". Unwrapping breaks compatibility with
# "debugruntest".
_pid = None


def _runcommandwrapper(orig, lui, repo, cmd, fullargs, *args):
    # For debugrebuildchangelog, it is incompatible with undo. Running undo
    # logic might break it. So bypass undo in that case.
    if cmd == "debugrebuildchangelog":
        return orig(lui, repo, cmd, fullargs, *args)

    global _pid
    pid = util.getpid()
    if _pid == pid:
        # Avoid nested undo activity from in-process "runcommand" invocations (e.g. "hg continue").
        return orig(lui, repo, cmd, fullargs, *args)

    _pid = pid

    _orig = orig

    def origunsetpid(*args, **opts):
        try:
            return _orig(*args, **opts)
        finally:
            # Unset _pid after command finishes. Further "runcommand"
            # invocations in this process will use "undo" normally.
            global _pid
            _pid = None

    orig = origunsetpid

    # For non-repo command, it's unnecessary to go through the undo logic
    if repo is None:
        return orig(lui, repo, cmd, fullargs, *args)

    command = [cmd] + fullargs

    # Whether something (transaction, or update) has triggered the writing of
    # the *before* state to undolog or not. Possible values:
    #  - []: not triggered, should trigger if write operation happens
    #  - [True]: already triggered by this process, should also log end state
    #  - [False]: already triggered by a parent process, should skip logging
    triggered = []

    # '_undologactive' is set by a parent hg process with before state written
    # to undolog. In this case, the current process should not write undolog.
    if "_undologactive" in encoding.environ:
        triggered.append(False)

    def log(orig, *args, **kwargs):
        # trigger a log of the initial state of a repo before a command tries
        # to modify that state.
        if not triggered:
            triggered.append(True)
            encoding.environ["_undologactive"] = "active"

            # Check whether undolog is consistent
            # ie check whether the undo ext was
            # off before this command
            changes = safelog(repo, [""])
            if changes:
                _recordnewgap(repo)

        return orig(*args, **kwargs)

    # Only write undo log if we know a command is going to do some writes. This
    # saves time calculating visible heads if the command is read-only (ex.
    # status).
    #
    # To detect a write command, wrap all possible entries:
    #  - transaction.__init__
    #  - merge.goto
    #  - merge.merge
    w = extensions.wrappedfunction
    with (
        w(merge, "goto", log),
        w(merge, "merge", log),
        w(transaction.transaction, "__init__", log),
    ):
        try:
            result = orig(lui, repo, cmd, fullargs, *args)
        finally:
            # record changes to repo
            if triggered and triggered[0]:
                # invalidatevolatilesets should really be done in Mercurial's
                # transaction handling code. We workaround it here before that
                # upstream change.
                repo.invalidatevolatilesets()
                safelog(repo, command)
                del encoding.environ["_undologactive"]

    return result


# Write: Log control


def safelog(repo, command):
    """boilerplate for log command

    input:
        repo: sapling.localrepo
        command: list of strings, first is string of command run
    output: bool
        True if changes have been recorded, False otherwise
    """
    changes = False
    if repo is not None:  # some hg commands don't require repo
        # undolog specific lock
        # allows running command during other commands when
        # otherwise legal.  Could cause weird undolog states,
        # which gap handling generally covers.
        try:
            try:
                repo.localvfs.makedirs("undolog")
            except OSError:
                repo.ui.debug("can't make undolog folder in .hg\n")
                return changes
            with lockmod.lock(
                repo.localvfs, "undolog/lock", desc="undolog", timeout=2, ui=repo.ui
            ):
                repo.ui.log("undologlock", "lock acquired\n")
                tr = lighttransaction(repo)
                with tr:
                    changes = trylog(repo, command, tr)
                    if changes and not ("undo" == command[0] or "redo" == command[0]):
                        _delundoredo(repo)
        except error.LockUnavailable:  # no write permissions
            repo.ui.debug("undolog lacks write permission\n")
        except error.LockHeld:  # timeout, not fatal: don't abort actual command
            # This shouldn't happen too often as it would
            # create gaps in the undo log
            repo.ui.debug("undolog lock timeout\n")
            _logtoscuba(repo.ui, "undolog lock timeout")
    return changes


def lighttransaction(repo):
    # full fledged transactions have two serious issues:
    # 1. they may cause infinite loops through hooks
    #    that run commands
    # 2. they are really expensive performance wise
    #
    # lighttransaction avoids certain hooks from being
    # executed, doesn't check repo locks, doesn't check
    # abandoned tr's (since we only record info) and doesn't
    # do any tag handling
    vfsmap = {"shared": repo.sharedvfs, "local": repo.localvfs}
    tr = transaction.transaction(
        repo.ui.warn, repo.localvfs, vfsmap, "undolog/tr.journal", "undolog/tr.undo"
    )
    return tr


def trylog(repo, command, tr):
    try:
        return log(repo, command, tr)
    except error.RevlogError:
        hintutil.trigger("undo-corrupt", repo.localvfs.join("undolog"))
        return False


def log(repo, command, tr):
    """logs data necessary for undo if repo state has changed

    input:
        repo: sapling.localrepo
        command: los, first is command to be recorded as run
        tr: transaction
    output: bool
        True if changes recorded
        False if no changes to record
    """
    newnodes = {
        "bookmarks": _logbookmarks(repo, tr),
        "workingparent": _logworkingparent(repo, tr),
    }
    if repo.ui.configbool("experimental", "narrow-heads"):
        # Assuming mutation and visibility are used. only log visibility heads.
        newnodes.update({"visibleheads": _logvisibleheads(repo, tr)})
    else:
        # Legacy mode: log draftheads and draftobsolete.
        newnodes.update(
            {
                "draftheads": _logdraftheads(repo, tr),
                "draftobsolete": _logdraftobsolete(repo, tr),
            }
        )

    try:
        existingnodes = _readindex(repo, 0)
    except IndexError:
        existingnodes = {}

    if all(newnodes.get(x) == existingnodes.get(x) for x in newnodes.keys()):
        # no changes to record
        return False
    else:
        newnodes.update(
            {
                "date": _logdate(repo, tr),
                "command": _logcommand(repo, tr, command),
                "unfinished": unfinished(repo),
            }
        )
        _logindex(repo, tr, newnodes)
        # changes have been recorded
        return True


def unfinished(repo):
    """like cmdutil.checkunfinished without raising an Abort"""
    return repo._rsrepo.workingcopy().commandstate() is not None


# Write: Logs


def writelog(repo, tr, name, revstring, retry=False):
    if tr is None:
        raise error.ProgrammingError
    rlog = _getrevlog(repo, name)
    try:
        node = rlog.addrevision(revstring.encode(), tr, 1, nullid, nullid)
    except mpatch.mpatchError:
        if retry:
            # Only allow a single retry to avoid infinite recursion.
            raise

        _nukerevlogs(repo, rlog.indexfile)
        return writelog(repo, tr, name, revstring, retry=True)
    return hex(node)


def _logdate(repo, tr):
    revstring = " ".join(str(x) for x in util.makedate())
    return writelog(repo, tr, "date.i", revstring)


def _logvisibleheads(repo, tr):
    hexnodes = sorted(hex(node) for node in repo.changelog._visibleheads.heads)
    revstring = "\n".join(sorted(hexnodes))
    return writelog(repo, tr, "visibleheads.i", revstring)


def _logdraftheads(repo, tr):
    spec = revsetlang.formatspec("heads(draft())")
    hexnodes = tohexnode(repo, spec)
    revstring = "\n".join(sorted(hexnodes))
    return writelog(repo, tr, "draftheads.i", revstring)


def _logdraftobsolete(repo, tr):
    spec = revsetlang.formatspec("draft() & obsolete()")
    hexnodes = tohexnode(repo, spec)
    revstring = "\n".join(sorted(hexnodes))
    return writelog(repo, tr, "draftobsolete.i", revstring)


def _logcommand(repo, tr, command):
    revstring = "\0".join(command)
    return writelog(repo, tr, "command.i", revstring)


def _logbookmarks(repo, tr):
    revstring = "\n".join(
        sorted("%s %s" % (name, hex(node)) for name, node in repo._bookmarks.items())
    )
    return writelog(repo, tr, "bookmarks.i", revstring)


def _logworkingparent(repo, tr):
    revstring = repo["."].hex()
    return writelog(repo, tr, "workingparent.i", revstring)


def _logindex(repo, tr, nodes):
    revstring = "\n".join(sorted("%s %s" % (k, v) for k, v in nodes.items()))
    return writelog(repo, tr, "index.i", revstring)


def _logundoredoindex(repo, reverseindex, branch=""):
    rlog = _getrevlog(repo, "index.i")
    hexnode = hex(rlog.node(_invertindex(rlog, reverseindex)))
    body = str(hexnode) + "\0" + branch
    return repo.localvfs.writeutf8("undolog/redonode", body)


def _delundoredo(repo):
    path = "undolog" + "/" + "redonode"
    repo.localvfs.tryunlink(path)


def _recordnewgap(repo, absoluteindex=None):
    path = "undolog" + "/" + "gap"
    if absoluteindex is None:
        rlog = _getrevlog(repo, "index.i")
        repo.localvfs.writeutf8(path, str(len(rlog) - 1))
    else:
        repo.localvfs.writeutf8(path, str(absoluteindex))


# Read


def _readindex(repo, reverseindex, prefetchedrevlog=None):
    if prefetchedrevlog is None:
        rlog = _getrevlog(repo, "index.i")
    else:
        rlog = prefetchedrevlog
    index = _invertindex(rlog, reverseindex)
    if index < 0 or index > len(rlog) - 1:
        raise IndexError
    chunk = rlog.revision(index).decode()
    indexdict = {}
    for row in chunk.split("\n"):
        kvpair = row.split(" ", 1)
        if kvpair[0]:
            indexdict[kvpair[0]] = kvpair[1]
    return indexdict


def _readindexorabort(repo, reverseindex):
    try:
        return _readindex(repo, reverseindex)
    except IndexError:
        raise error.Abort(_("index out of bounds"))


def _readnode(repo, filename, hexnode):
    rlog = _getrevlog(repo, filename)
    return rlog.revision(bin(hexnode)).decode()


def _logtoscuba(ui, message):
    ui.log("undo", message, undo=message)


def _gapcheck(ui, repo, reverseindex):
    rlog = _getrevlog(repo, "index.i")
    absoluteindex = _invertindex(rlog, reverseindex)
    path = "undolog" + "/" + "gap"
    # Callers treat False as "history may be missing" and refuse to undo, so it
    # is the safe answer whenever the check cannot be completed.
    result = False
    try:
        try:
            result = absoluteindex >= int(repo.localvfs.read(path))
        except IOError:
            # recreate file
            repo.ui.debug(
                "failed to read gap file in %s, attempting recreation\n" % path
            )
            _logtoscuba(ui, "gap file corruption")
            rlog = _getrevlog(repo, "index.i")
            i = 0
            while i < (len(rlog)):
                indexdict = _readindex(repo, i, rlog)
                if "" == _readnode(repo, "command.i", indexdict["command"]):
                    break
                i += 1
            # defaults to before oldest command
            _recordnewgap(repo, _invertindex(rlog, i))
            result = absoluteindex >= _invertindex(rlog, i)
    except Exception as e:
        repo.ui.debug(
            "gap check in %s failed (%s), assuming missing history\n" % (path, e)
        )
        result = False
    return result


# Visualize

"""debug commands and instrumentation for the undo extension

Adds the `debugundohistory` and `debugundosmartlog` commands to visualize
operational history and to give a preview of how undo will behave.
"""


@command(
    "debugundohistory",
    [
        ("n", "index", 0, _("details about specific operation")),
        ("l", "list", False, _("list recent undo-able operation")),
    ],
)
def debugundohistory(ui, repo, *args, **opts):
    """Print operational history
    0 is the most recent operation
    """
    if repo is not None:
        if opts.get("list"):
            if args and args[0].isdigit():
                offset = int(args[0])
            else:
                offset = 0
            _debugundolist(ui, repo, offset)
        else:
            reverseindex = opts.get("index")
            if 0 == reverseindex and args and args[0].isdigit():
                reverseindex = int(args[0])
            _debugundoindex(ui, repo, reverseindex)


def _debugundolist(ui, repo, offset):
    offset = abs(offset)

    template = "{sub('\0', ' ', undo)}\n"
    fm = ui.formatter("debugundohistory", {"template": template})
    prefetchedrevlog = _getrevlog(repo, "index.i")
    recentrange = min(5, len(prefetchedrevlog) - offset)
    if 0 == recentrange:
        fm.startitem()
        fm.write("undo", "%s", "None")
    for i in range(recentrange):
        nodedict = _readindex(repo, i + offset, prefetchedrevlog)
        commandstr = _readnode(repo, "command.i", nodedict["command"])
        if "" == commandstr:
            commandstr = " -- gap in log -- "
        else:
            commandstr = commandstr.split("\0", 1)[1]
        fm.startitem()
        fm.write("undo", "%s", str(i + offset) + ": " + commandstr)
    fm.end()


def _debugundoindex(ui, repo, reverseindex):
    try:
        nodedict = _readindex(repo, reverseindex)
    except IndexError:
        raise error.Abort(_("index out of bounds"))
        return
    template = "{tabindent(sub('\0', ' ', content))}\n"
    fm = ui.formatter("debugundohistory", {"template": template})
    cabinet = (
        "command.i",
        "bookmarks.i",
        "date.i",
        "draftheads.i",
        "draftobsolete.i",
        "visibleheads.i",
        "workingparent.i",
    )
    for filename in cabinet:
        name = filename[:-2]
        header = name + ":\n"
        if name not in nodedict:
            continue
        rawcontent = _readnode(repo, filename, nodedict[name])
        if "date.i" == filename:
            splitdate = rawcontent.split(" ")
            datetuple = (float(splitdate[0]), int(splitdate[1]))
            content = util.datestr(datetuple)
        elif filename in {"draftheads.i", "visibleheads.i"}:
            try:
                oldnodes = _readindex(repo, reverseindex + 1)
                oldheads = _readnode(repo, filename, oldnodes[filename[:-2]])
            except IndexError:  # index is oldest log
                content = rawcontent
            else:
                content = "ADDED:\n\t" + "\n\t".join(
                    sorted(set(rawcontent.split("\n")) - set(oldheads.split("\n")))
                )
                content += "\nREMOVED:\n\t" + "\n\t".join(
                    sorted(set(oldheads.split("\n")) - set(rawcontent.split("\n")))
                )
        elif "command.i" == filename:
            if "" == rawcontent:
                content = "unknown command(s) run, gap in log"
            else:
                content = rawcontent.split("\0", 1)[1]
        else:
            content = rawcontent
        fm.startitem()
        fm.write("content", "%s", header + content)
    fm.write("content", "%s", "unfinished:\t" + nodedict["unfinished"])
    fm.end()


# Revset logic


def _getolddrafts(repo, reverseindex):
    # convert reverseindex to node
    # this makes caching guaranteed correct
    # bc immutable history
    nodedict = _readindexorabort(repo, reverseindex)
    return _cachedgetolddrafts(repo, nodedict)


def _cachedgetolddrafts(repo, nodedict):
    if not hasattr(repo, "_undoolddraftcache"):
        repo._undoolddraftcache = {}
    cache = repo._undoolddraftcache
    if repo.ui.configbool("experimental", "narrow-heads"):
        headnode = key = nodedict["visibleheads"]
        if key not in cache:
            oldheads = _readnode(repo, "visibleheads.i", headnode).split("\n")
            cache[key] = repo.revs("(not public()) & ::%ls", oldheads)
    else:
        draftnode = nodedict["draftheads"]
        obsnode = nodedict["draftobsolete"]
        key = draftnode + obsnode
        if key not in cache:
            olddraftheads = _readnode(repo, "draftheads.i", draftnode)
            oldheadslist = olddraftheads.split("\n")
            oldobs = _readnode(repo, "draftobsolete.i", obsnode)
            oldobslist = filter(None, oldobs.split("\n"))
            oldlogrevstring = revsetlang.formatspec(
                "(draft() & ancestors(%ls)) - %ls", oldheadslist, oldobslist
            )
            urepo = repo
            cache[key] = smartset.baseset(urepo.revs(oldlogrevstring), repo=repo)
    return cache[key]


revsetpredicate = registrar.revsetpredicate()


@revsetpredicate("olddraft")
def _olddraft(repo, subset, x):
    """``olddraft([index])``
    previous draft commits

    'index' is how many undoable commands you want to look back
    an undoable command is one that changed draft heads, bookmarks
    and or working copy parent.  Note that olddraft uses an absolute index and
    so olddraft(1) represents the state after an hg undo -a and not an hg undo.
    Note: this revset may include hidden commits
    """
    args = revset.getargsdict(x, "olddraftrevset", "reverseindex")
    reverseindex = revsetlang.getinteger(
        args.get("reverseindex"), _("index must be a positive integer"), 1
    )
    revs = _getolddrafts(repo, reverseindex)
    return subset & smartset.baseset(revs, repo=repo)


@revsetpredicate("_localbranch")
def _localbranch(repo, subset, x):
    """``_localbranch(changectx)``
    localbranch changesets

    Returns all commits within the same localbranch as the changeset(s). A local
    branch is all draft changesets that are connected, uninterrupted by public
    changesets.  Any draft commit within a branch, or a public commit at the
    base of the branch, can be used to identify localbranches.
    """
    # executed on an filtered repo
    args = revset.getargsdict(x, "branchrevset", "changectx")
    revstring = revsetlang.getstring(
        args.get("changectx"), _("localbranch argument must be a changectx")
    )
    revs = repo.revs(revstring)
    # we assume that there is only a single rev
    if repo[revs.first()].ispublic():
        querystring = revsetlang.formatspec("(children(%d) & draft())::", revs.first())
    else:
        querystring = revsetlang.formatspec("((::%ld) & draft())::", revs)
    return subset & smartset.baseset(repo.revs(querystring), repo=repo)


def _getoldworkingcopyparent(repo, reverseindex):
    # convert reverseindex to node
    # this makes caching guaranteed correct
    # bc immutable history
    nodedict = _readindexorabort(repo, reverseindex)
    return _cachedgetoldworkingcopyparent(repo, nodedict["workingparent"])


def _cachedgetoldworkingcopyparent(repo, wkpnode):
    if not hasattr(repo, "_undooldworkingparentcache"):
        repo._undooldworkingparentcache = {}
    cache = repo._undooldworkingparentcache
    key = wkpnode
    if key not in cache:
        oldworkingparent = _readnode(repo, "workingparent.i", wkpnode)
        oldworkingparent = filter(None, oldworkingparent.split("\n"))
        oldwkprevstring = revsetlang.formatspec("%ls", oldworkingparent)
        urepo = repo
        cache[key] = smartset.baseset(urepo.revs(oldwkprevstring), repo=repo)
    return cache[key]


@revsetpredicate("oldworkingcopyparent")
def _oldworkingcopyparent(repo, subset, x):
    """``oldworkingcopyparent([index])``
    previous working copy parent

    'index' is how many undoable commands you want to look back.  See 'hg undo'.
    """
    args = revset.getargsdict(x, "oldoworkingcopyrevset", "reverseindex")
    reverseindex = revsetlang.getinteger(
        args.get("reverseindex"), _("index must be a positive integer"), 1
    )
    revs = _getoldworkingcopyparent(repo, reverseindex)
    return subset & smartset.baseset(revs, repo=repo)


# Templates
templatefunc = registrar.templatefunc()


def _undonehexnodes(repo, reverseindex):
    revstring = "olddraft(0) - olddraft(%d)" % reverseindex
    return list(repo.nodes(revstring))


@templatefunc("undonecommits(reverseindex)")
def showundonecommits(context, mapping, args):
    """String.  Changectxs added since reverseindex command."""
    reverseindex = templater.evalinteger(
        context, mapping, args[0], _("undonecommits needs an integer argument")
    )
    repo = mapping["ctx"]._repo
    ctx = mapping["ctx"]
    hexnodes = _undonehexnodes(repo, reverseindex)
    if ctx.node() in hexnodes:
        result = ctx.hex()
    else:
        result = None
    return result


def _donehexnodes(repo, reverseindex):
    revstring = "olddraft(%d)" % reverseindex
    return list(repo.nodes(revstring))


@templatefunc("donecommits(reverseindex)")
def showdonecommits(context, mapping, args):
    """String.  Changectxs reverseindex repo states ago."""
    reverseindex = templater.evalinteger(
        context, mapping, args[0], _("donecommits needs an integer argument")
    )
    repo = mapping["ctx"]._repo
    ctx = mapping["ctx"]
    hexnodes = _donehexnodes(repo, reverseindex)
    if ctx.node() in hexnodes:
        result = ctx.hex()
    else:
        result = None
    return result


def _oldmarks(repo, reverseindex):
    nodedict = _readindexorabort(repo, reverseindex)
    bookstring = _readnode(repo, "bookmarks.i", nodedict["bookmarks"])
    oldmarks = bookstring.split("\n")
    result = []
    for mark in oldmarks:
        kv = mark.rsplit(" ", 1)
        if len(kv) == 2:
            result.append(kv)
    return result


@templatefunc("oldbookmarks(reverseindex)")
def showoldbookmarks(context, mapping, args):
    """List of Strings. Bookmarks that used to be at the changectx reverseindex
    repo states ago."""
    reverseindex = templater.evalinteger(
        context, mapping, args[0], _("oldbookmarks needs an integer argument")
    )
    repo = mapping["ctx"]._repo
    ctx = mapping["ctx"]
    oldmarks = _oldmarks(repo, reverseindex)
    bookmarks = []
    ctxhex = ctx.hex()
    for kv in oldmarks:
        if kv[1] == ctxhex:
            bookmarks.append(kv[0])
    active = repo._activebookmark
    makemap = lambda v: {"bookmark": v, "active": active, "current": active}
    f = templatekw._showlist("bookmark", bookmarks, mapping)
    return templatekw._hybrid(f, bookmarks, makemap, lambda x: x["bookmark"])


@templatefunc("removedbookmarks(reverseindex)")
def removedbookmarks(context, mapping, args):
    """List of Strings.  Bookmarks that have been moved or removed from a given
    changectx by reverseindex repo state."""
    reverseindex = templater.evalinteger(
        context, mapping, args[0], _("removedbookmarks needs an integer argument")
    )
    repo = mapping["ctx"]._repo
    ctx = mapping["ctx"]
    currentbookmarks = mapping["ctx"].bookmarks()
    oldmarks = _oldmarks(repo, reverseindex)
    oldbookmarks = []
    ctxhex = ctx.hex()
    for kv in oldmarks:
        if kv[1] == ctxhex:
            oldbookmarks.append(kv[0])
    bookmarks = list(set(currentbookmarks) - set(oldbookmarks))
    active = repo._activebookmark
    makemap = lambda v: {"bookmark": v, "active": active, "current": active}
    f = templatekw._showlist("bookmark", bookmarks, mapping)
    return templatekw._hybrid(f, bookmarks, makemap, lambda x: x["bookmark"])


@templatefunc("oldworkingcopyparent(reverseindex)")
def oldworkingparenttemplate(context, mapping, args):
    """String. Workingcopyparent reverseindex repo states ago."""
    reverseindex = templater.evalinteger(
        context, mapping, args[0], _("undonecommits needs an integer argument")
    )
    repo = mapping["ctx"]._repo
    ctx = mapping["ctx"]
    revstring = "oldworkingcopyparent(%d)" % reverseindex
    nodes = list(repo.nodes(revstring))
    if ctx.node() in nodes:
        result = ctx.hex()
    else:
        result = None
    return result


# Undo:


@command(
    "undo",
    [
        (
            "a",
            "absolute",
            False,
            _("absolute based on command index instead of relative undo"),
        ),
        ("b", "branch", "", _("local branch undo, accepts commit hash (ADVANCED)")),
        ("f", "force", False, _("undo across missing undo history (ADVANCED)")),
        ("i", "interactive", False, _("use interactive ui for undo")),
        ("k", "keep", False, _("keep working copy changes")),
        ("n", "step", 1, _("how many steps to undo back")),
        ("p", "preview", False, _("see smartlog-like preview of future undo state")),
    ],
)
def undo(ui, repo, **opts):
    """undo the last local command

    Reverse the effects of the last local command. A local command is one that
    changed the currently checked out commit, that modified the contents of
    local commits, or that changed local bookmarks. Examples of local commands
    include :prog:`goto`, :prog:`commit`, :prog:`amend`, and :prog:`rebase`.

    You cannot use :prog:`undo` to undo uncommitted changes in the working copy,
    or changes to remote bookmarks.

    You can run :prog:`undo` multiple times to undo a series of local commands.
    Alternatively, you can explicitly specify the number of local commands to
    undo using ``--step``. This number can also be specified as a positional
    argument.

    To undo the effects of :prog:`undo`, run :prog:`redo`. Run
    :prog:`help redo` for more information.


    Include ``--keep`` to preserve the state of the working copy. For example,
    specify ``--keep`` when running :prog:`undo` to reverse the effects of an
    :prog:`commit` or :prog:`amend` operation while still preserving changes
    in the working copy. These changes will appear as pending changes.

    Specify ``--preview`` to see a graphical display that shows what
    your smartlog will look like after you run the command. Specify
    ``--interactive`` for an interactive version of this preview in which
    you can step backwards and forwards in the undo history.


    .. note::

       :prog:`undo` cannot be used with non-local commands, or with commands
       that are read-only. :prog:`undo` will skip over these commands in the
       undo history.

       For hybrid commands that result in both local and remote changes,
       :prog:`undo` will undo the local changes, but not the remote changes.
       For example, `@prog@ pull --rebase` might move remote/master and also
       rebase local commits. In this situation, :prog:`undo` will revert the
       rebase, but not the change to remote/master.

    .. container:: verbose

        Branch limits the scope of an undo to a group of local (draft)
        changectxs, identified by any one member of this group.
    """
    reverseindex = opts.get("step")
    relativeundo = not opts.get("absolute")
    keep = opts.get("keep")
    branch = opts.get("branch")
    preview = opts.get("preview")
    interactive = opts.get("interactive")
    if interactive and interactiveui is None:
        raise error.Abort(_("interactive ui is not supported on Windows"))
    if interactive:
        preview = True

    if branch and reverseindex != 1 and reverseindex != -1:
        raise error.Abort(_("--branch with --index not supported"))
    if relativeundo:
        try:
            reverseindex = _computerelative(
                repo, reverseindex, absolute=not relativeundo, branch=branch
            )
        except IndexError:
            raise error.Abort(
                _("cannot undo this far - undo extension was not enabled")
            )

    if branch and preview:
        raise error.Abort(_("--branch with --preview not supported"))

    if interactive:
        cmdutil.checkunfinished(repo)
        cmdutil.bailifchanged(repo)

        class undopreview(interactiveui.viewframe):
            def __init__(self, ui, repo, index):
                super().__init__(ui, repo)
                self.index = index

            def render(self):
                ui = self.ui
                ui.pushbuffer()
                return_code = _preview(ui, self.repo, self.index)
                if return_code == 1:
                    if self.index < 0:
                        self.index += 1
                        repo.ui.status(_("Already at newest repo state\a\n"))
                    elif self.index > 0:
                        self.index -= 1
                        repo.ui.status(_("Already at oldest repo state\a\n"))
                    _preview(ui, self.repo, self.index)
                text = ui.config(
                    "undo",
                    "interactivehelptext",
                    "legend: red - to hide; green - to revive\n",
                )
                repo.ui.status(text)
                repo.ui.status(_("<-: newer  ->: older  q: abort  enter: confirm\n"))
                return ui.popbuffer().splitlines(), None

            def handlekeypress(self, key):
                if key == self.KEY_Q:
                    self.finish()
                if key == self.KEY_RETURN:
                    del opts["preview"]
                    del opts["interactive"]
                    opts["absolute"] = "absolute"
                    opts["step"] = self.index
                    undo(ui, repo, **opts)
                    self.finish()
                if key == self.KEY_RIGHT:
                    self.index += 1
                if key == self.KEY_LEFT:
                    self.index -= 1

        viewobj = undopreview(ui, repo, reverseindex)
        interactiveui.view(viewobj)
        return
    elif preview:
        _preview(ui, repo, reverseindex)
        return

    with repo.wlock(), repo.lock(), repo.transaction("undo"):
        cmdutil.checkunfinished(repo)
        cmdutil.bailifchanged(repo)
        if not (opts.get("force") or _gapcheck(ui, repo, reverseindex)):
            raise error.Abort(_("attempted risky undo across missing history"))
        _tryundoto(ui, repo, reverseindex, keep=keep, branch=branch)

        # store undo data
        # for absolute undos, think of this as a reset
        # for relative undos, think of this as an update
        _logundoredoindex(repo, reverseindex, branch)


@command(
    "redo",
    [("p", "preview", False, _("see smartlog-like preview of future redo state"))],
)
def redo(ui, repo, *args, **opts):
    """undo the last undo

    Reverse the effects of an :prog:`undo` operation.

    You can run :prog:`redo` multiple times to undo a series of :prog:`undo`
    commands. Alternatively, you can explicitly specify the number of
    :prog:`undo` commands to undo by providing a number as a positional argument.

    Specify ``--preview`` to see a graphical display that shows what
    your smartlog will look like after you run the command.

    For an interactive interface, run :prog:`undo --interactive`. This command
    enables you to visually step backwards and forwards in the undo history.
    Run :prog:`help undo` for more information.

    Returns 0 on success.
    """
    shiftedindex = _computerelative(repo, 0)
    preview = opts.get("preview")

    branch = ""
    reverseindex = 0
    redocount = 0
    done = False
    while not done:
        # we step back the linear undo log
        # redoes cancel out undoes, if we have one more undo, we should undo
        # there, otherwise we continue looking
        # we are careful to not redo past absolute undoes (bc we loose undoredo
        # log info)
        # if we run into something that isn't undo or redo, we Abort (including
        # gaps in the log)
        # we extract the --index arguments out of undoes to make sure we update
        # the undoredo index correctly
        nodedict = _readindex(repo, reverseindex)
        commandstr = _readnode(repo, "command.i", nodedict["command"])
        commandlist = commandstr.split("\0")

        if "True" == nodedict["unfinished"]:
            # don't want to redo to an interrupted state
            reverseindex += 1
        elif commandlist[0] == "undo":
            _args, undoopts, _specifiedopts = cliparser.parsecommand(
                commandlist,
                cmdtable["undo"][1] + commands.globalopts,
            )
            if redocount == 0:
                # want to go to state before the undo (not after)
                toshift = undoopts["step"]
                shiftedindex -= toshift
                reverseindex += 1
                branch = undoopts.get("branch")
                done = True
            else:
                if undoopts["absolute"]:
                    raise error.Abort(_("can't redo past absolute undo"))
                reverseindex += 1
                redocount -= 1
        elif commandlist[0] == "redo":
            redocount += 1
            reverseindex += 1
        else:
            raise error.Abort(_("nothing to redo"))

    if preview:
        _preview(ui, repo, reverseindex)
        return

    with repo.wlock(), repo.lock(), repo.transaction("redo"):
        cmdutil.checkunfinished(repo)
        cmdutil.bailifchanged(repo)
        _tryundoto(ui, repo, reverseindex)
        # update undredo by removing what the given undo added
        _logundoredoindex(repo, shiftedindex, branch)


def _tryundoto(ui, repo, reverseindex, keep=False, branch=None):
    try:
        _undoto(ui, repo, reverseindex, keep, branch)
    except error.RevlogError:
        ui.write_err(_("cannot undo: undo history is corrupted\n"))


def _undoto(ui, repo, reverseindex, keep=False, branch=None):
    # undo to specific reverseindex
    # branch is a changectx hash (potentially short form)
    # which identifies its branch via localbranch revset

    if branch and repo.ui.configbool("experimental", "narrow-heads"):
        raise error.Abort(
            _("'undo --branch' is no longer supported in the current setup")
        )

    try:
        nodedict = _readindex(repo, reverseindex)
    except IndexError:
        raise error.Abort(_("index out of bounds"))

    # bookmarks
    bookstring = _readnode(repo, "bookmarks.i", nodedict["bookmarks"])
    booklist = bookstring.split("\n")
    if branch:
        spec = revsetlang.formatspec("_localbranch(%s)", branch)
        branchcommits = tohexnode(repo, spec)
    else:
        branchcommits = False

    # copy implementation for bookmarks
    itercopy = []
    for mark in repo._bookmarks.items():
        itercopy.append(mark)
    bmremove = []
    for mark in itercopy:
        if not branchcommits or hex(mark[1]) in branchcommits:
            bmremove.append((mark[0], None))
    repo._bookmarks.applychanges(repo, repo.currenttransaction(), bmremove)
    bmchanges = []
    for mark in booklist:
        if mark:
            kv = mark.rsplit(" ", 1)
            if not branchcommits or kv[1] in branchcommits or (kv[0], None) in bmremove:
                bmchanges.append((kv[0], bin(kv[1])))
    repo._bookmarks.applychanges(repo, repo.currenttransaction(), bmchanges)

    # working copy parent
    workingcopyparent = _readnode(repo, "workingparent.i", nodedict["workingparent"])
    if not keep:
        if not branchcommits or workingcopyparent in branchcommits:
            # bailifchanged is run, so this should be safe
            hg.clean(repo, workingcopyparent, show_stats=False)
    elif not branchcommits or workingcopyparent in branchcommits:
        # keeps working copy files
        prednode = bin(workingcopyparent)
        predctx = repo[prednode]

        changedfiles = []
        wctx = repo[None]
        wctxmanifest = wctx.manifest()
        predctxmanifest = predctx.manifest()
        dirstate = repo.dirstate
        diff = predctxmanifest.diff(wctxmanifest)
        changedfiles.extend(diff.keys())

        with dirstate.parentchange():
            dirstate.rebuild(prednode, predctxmanifest, changedfiles)
            # we want added and removed files to be shown
            # properly, not with ? and ! prefixes
            for filename, data in diff.items():
                if data[0][0] is None:
                    dirstate.add(filename)
                if data[1][0] is None:
                    dirstate.remove(filename)

    # visible changesets
    addedrevs = "olddraft(0) - olddraft(%d)" % reverseindex
    removedrevs = "olddraft(%d) - olddraft(0)" % reverseindex
    if not branch:
        if repo.ui.configbool("experimental", "narrow-heads"):
            # Assuming mutation and visibility are used. Restore visibility heads
            # directly.
            _restoreheads(repo, reverseindex)
        else:
            # Legacy path.
            smarthide(repo, addedrevs, removedrevs)
            revealcommits(repo, removedrevs)
    else:
        localadds = revsetlang.formatspec(
            "(olddraft(0) - olddraft(%d)) and _localbranch(%%s)" % reverseindex, branch
        )
        localremoves = revsetlang.formatspec(
            "(olddraft(%d) - olddraft(0)) and _localbranch(%%s)" % reverseindex,
            branch,
        )
        smarthide(repo, localadds, removedrevs)
        smarthide(repo, addedrevs, localremoves, local=True)
        revealcommits(repo, localremoves)

    # informative output
    time = _readnode(repo, "date.i", nodedict["date"])
    time = util.datestr([float(x) for x in time.split(" ")])

    try:
        nodedict = _readindex(repo, reverseindex - 1)
    except IndexError:
        raise error.Abort(_("most recent state: undoing here won't change anything"))
    commandstr = _readnode(repo, "command.i", nodedict["command"])
    commandlist = commandstr.split("\0")[1:]
    commandstr = " ".join(commandlist)
    uimessage = _("undone to %s, before %s\n") % (time, commandstr)
    if reverseindex == 1 and any(cmd in ("commit", "amend") for cmd in commandlist):
        command = "commit"
        if any(cmd in ("amend", "--amend") for cmd in commandlist):
            command = "amend"
        oldcommithash = _readnode(repo, "workingparent.i", nodedict["workingparent"])
        shorthash = short(bin(oldcommithash))
        if not keep:
            hintutil.trigger("undo-uncommit-unamend", command, shorthash)
    repo.ui.status(uimessage)


def _restoreheads(repo, reverseindex):
    """Revert visibility heads to a previous state"""
    nodedict = _readindex(repo, reverseindex)
    headnode = nodedict["visibleheads"]
    oldheads = list(
        filter(None, map(bin, _readnode(repo, "visibleheads.i", headnode).split("\n")))
    )
    tr = repo.currenttransaction()
    repo.changelog._visibleheads.setvisibleheads(repo, oldheads, tr)


def _computerelative(repo, reverseindex, absolute=False, branch=""):
    # allows for relative undos using
    # redonode storage
    # allows for branch undos using
    # findnextdelta logic
    if reverseindex != 0:
        sign = reverseindex // abs(reverseindex)
    else:
        sign = None
    if not absolute:
        try:  # attempt to get relative shift
            nodebranch = repo.localvfs.read("undolog/redonode").split(b"\0")
            hexnode = nodebranch[0].decode()
            try:
                oldbranch = nodebranch[1].decode()
            except IndexError:
                oldbranch = ""
            rlog = _getrevlog(repo, "index.i")
            rev = rlog.rev(bin(hexnode))
            shiftedindex = _invertindex(rlog, rev)
        except (IOError, error.RevlogError):
            # no shift
            shiftedindex = 0
            oldbranch = ""
    else:
        shiftedindex = 0
        oldbranch = ""

    if not branch:
        if not oldbranch:
            reverseindex = shiftedindex + reverseindex
        # else: previous command was branch undo
        # perform absolute undo (no shift)
    else:
        # check if relative branch
        if (branch != oldbranch) and (oldbranch != ""):
            rootdelta = revsetlang.formatspec(
                "roots(_localbranch(%s)) - roots(_localbranch(%s))", branch, oldbranch
            )
            if repo.revs(rootdelta):
                # different group of commits
                shiftedindex = 0

        # from shifted index, find reverse index # of states that change
        # branch
        # remember that reverseindex can be negative
        sign = reverseindex // abs(reverseindex)
        for count in range(abs(reverseindex)):
            shiftedindex = _findnextdelta(repo, shiftedindex, branch, direction=sign)
        reverseindex = shiftedindex
    # skip interrupted commands
    if sign:
        done = False
        rlog = _getrevlog(repo, "index.i")
        while not done:
            indexdict = _readindex(repo, reverseindex, rlog)
            if "True" == indexdict["unfinished"]:
                reverseindex += sign
            else:
                done = True
    return reverseindex


def _findnextdelta(repo, reverseindex, branch, direction):
    # finds closest repos state making changes to branch in direction
    # input:
    #   repo: sapling.localrepo
    #   reverseindex: positive int for index.i
    #   branch: string changectx (commit hash)
    #   direction: positive or negative int
    # output:
    #   int index with next branch delta
    #   this is the first repo state that makes a changectx, bookmark or working
    #   copy parent change that effects the given branch
    if 0 == direction:  # no infinite cycles guarantee
        raise error.ProgrammingError
    # current state
    try:
        nodedict = _readindex(repo, reverseindex)
    except IndexError:
        raise error.Abort(_("index out of bounds"))
    alphaworkingcopyparent = _readnode(
        repo, "workingparent.i", nodedict["workingparent"]
    )
    alphabookstring = _readnode(repo, "bookmarks.i", nodedict["bookmarks"])
    incrementalindex = reverseindex

    spec = revsetlang.formatspec("_localbranch(%s)", branch)
    hexnodes = tohexnode(repo, spec)

    done = False
    while not done:
        # move index
        incrementalindex += direction
        # check this index
        try:
            nodedict = _readindex(repo, incrementalindex)
        except IndexError:
            raise error.Abort(_("index out of bounds"))
        # skip interrupted commands
        if "True" == nodedict["unfinished"]:
            break
        # check wkp, commits, bookmarks
        workingcopyparent = _readnode(
            repo, "workingparent.i", nodedict["workingparent"]
        )
        bookstring = _readnode(repo, "bookmarks.i", nodedict["bookmarks"])
        # local changes in respect to visible changectxs
        # disjunctive union of present and old = changes
        # intersection of changes and local = localchanges
        localctxchanges = revsetlang.formatspec(
            ("((olddraft(%d) + olddraft(%d)) -" % (incrementalindex, reverseindex))
            + ("(olddraft(%d) and olddraft(%d)))" % (incrementalindex, reverseindex))
            + " and _localbranch(%s)",
            branch,
        )
        done = done or repo.revs(localctxchanges)
        if done:  # perf boost
            break
        # bookmark changes
        if alphabookstring != bookstring:
            diff = set(alphabookstring.split("\n")) ^ set(bookstring.split("\n"))
            for mark in diff:
                if mark:
                    kv = mark.rsplit(" ", 1)
                    # was or will the mark be in the localbranch
                    if kv[1] in hexnodes:
                        done = True
                        break

        # working copy parent changes
        # for workingcopyparent, only changes within the scope are interesting
        if alphaworkingcopyparent != workingcopyparent:
            done = done or (
                workingcopyparent in hexnodes and alphaworkingcopyparent in hexnodes
            )

    return incrementalindex


# hide and reveal commits
def smarthide(repo, revhide, revshow, local=False):
    """hides changecontexts and reveals some commits

    tries to connect related hides and shows with obs marker
    when reasonable and correct

    use local to not hide revhides without corresponding revshows
    """
    hidectxs = repo.set(revhide)
    showctxs = repo.set(revshow)
    markers = []
    nodes = []
    for ctx in hidectxs:
        unfi = repo
        related = set()
        if mutation.enabled(unfi):
            related.update(mutation.allpredecessors(unfi, [ctx.node()]))
            related.update(mutation.allsuccessors(unfi, [ctx.node()]))
        related.intersection_update(x.node() for x in showctxs)
        destinations = [repo[x] for x in related]

        # two primary objectives:
        # 1. correct divergence/nondivergence
        # 2. correct visibility of changesets for the user
        # secondary objectives:
        # 3. useful ui message in hg sl: "Undone to"
        # Design choices:
        # 1-to-1 correspondence is easy
        # 1-to-many correspondence is hard:
        #   it's either divergent A to B, A to C
        #   or split A to B,C
        #   because of undo we don't know which
        #   without complex logic
        # Solution: provide helpful ui message for
        # common and easy case (1 to 1), use simplest
        # correct solution for complex edge case

        if len(destinations) == 1:
            markers.append((ctx, destinations))
            nodes.append(ctx.node())
        elif len(destinations) > 1:  # split
            markers.append((ctx, []))
            nodes.append(ctx.node())
        elif len(destinations) == 0:
            if not local:
                markers.append((ctx, []))
                nodes.append(ctx.node())

    visibility.remove(repo, nodes)


def revealcommits(repo, rev):
    ctxs = list(repo.set(rev))
    visibility.add(repo, [ctx.node() for ctx in ctxs])


def _preview(ui, repo, reverseindex):
    # Print smartlog like preview of undo
    # Input:
    #   ui:
    #   repo: sapling.localrepo
    # Output:
    #   returns 1 on index error, 0 otherwise

    # override "UNDOINDEX" as a variable usable in template
    if not _gapcheck(ui, repo, reverseindex):
        repo.ui.status(_("WARN: missing history between present and this state\n"))
    overrides = {("templates", "UNDOINDEX"): str(reverseindex)}

    opts = {}
    opts["template"] = "{undopreview}"

    try:
        nodedict = _readindex(repo, reverseindex)
        curdict = _readindex(repo, reverseindex)
    except IndexError:
        return 1

    bookstring = _readnode(repo, "bookmarks.i", nodedict["bookmarks"])
    oldmarks = bookstring.split("\n")
    oldpairs = set()
    for mark in oldmarks:
        kv = mark.rsplit(" ", 1)
        if len(kv) == 2:
            oldpairs.update(kv)
    bookstring = _readnode(repo, "bookmarks.i", curdict["bookmarks"])
    curmarks = bookstring.split("\n")
    curpairs = set()
    for mark in curmarks:
        kv = mark.rsplit(" ", 1)
        if len(kv) == 2:
            curpairs.update(kv)

    diffpairs = oldpairs.symmetric_difference(curpairs)
    # extract hashes from diffpairs

    bookdiffs = []
    for kv in diffpairs:
        bookdiffs += kv[0]

    revstring = revsetlang.formatspec(
        ("ancestor(olddraft(0), olddraft(%s)) +" % reverseindex)
        + ("(draft() & ::((olddraft(0) - olddraft(%s)) + " % reverseindex)
        + ("(olddraft(%s) - olddraft(0)) + %%ls + '.' + " % reverseindex)
        + ("oldworkingcopyparent(%s)))" % reverseindex),
        bookdiffs,
    )

    opts["rev"] = [revstring]
    try:
        with ui.configoverride(overrides):
            cmdutil.graphlog(ui, repo, (), opts)
        # informative output
        nodedict = _readindex(repo, reverseindex)
        time = _readnode(repo, "date.i", nodedict["date"])
        time = util.datestr([float(x) for x in time.split(" ")])
    except IndexError:
        # don't print anything
        return 1

    try:
        nodedict = _readindex(repo, reverseindex - 1)
        commandstr = _readnode(repo, "command.i", nodedict["command"])
        commandlist = commandstr.split("\0")[1:]
        commandstr = " ".join(commandlist)
        uimessage = _("undo to %s, before %s\n") % (time, commandstr)
        repo.ui.status(uimessage)
    except IndexError:
        repo.ui.status(_("most recent state: undoing here won't change anything\n"))
    return 0


# Tools


def _invertindex(rlog, indexorreverseindex):
    return len(rlog) - 1 - indexorreverseindex


def _getrevlog(repo, filename):
    path = "undolog/" + filename
    try:
        return revlog.revlog(repo.localvfs, path)
    except error.RevlogError:
        # corruption: for now, we can simply nuke all files
        repo.ui.debug("caught revlog error. %s was probably corrupted\n" % path)
        _nukerevlogs(repo, path)
        # if we get the error a second time
        # then someone is actively messing with these files
        return revlog.revlog(repo.localvfs, path)


def _nukerevlogs(repo, path):
    repo.ui.debug(f"caught revlog error. {path} was probably corrupted\n")
    _logtoscuba(repo.ui, "revlog error")
    repo.localvfs.rmtree("undolog")
    repo.localvfs.makedirs("undolog")


def tohexnode(repo, spec):
    nodes = repo.nodes(spec)
    hexnodes = [hex(x) for x in nodes]
    return hexnodes
