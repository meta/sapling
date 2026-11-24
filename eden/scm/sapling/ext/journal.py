# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""track previous positions of bookmarks (EXPERIMENTAL)

This extension adds a new command: `@prog@ journal`, which shows you where
bookmarks were previously located.

"""

import os
import weakref
from typing import Dict

from bindings import journal as rsjournal
from sapling import (
    bookmarks,
    cmdutil,
    dispatch,
    error,
    extensions,
    hg,
    localrepo,
    lock,
    node,
    registrar,
    smartset,
    util,
)
from sapling.i18n import _

cmdtable = {}
command = registrar.command(cmdtable)

# Note for extension authors: ONLY specify testedwith = 'ships-with-hg-core' for
# extensions which SHIP WITH MERCURIAL. Non-mainline extensions should
# be specifying the version(s) of Mercurial they are tested with, or
# leave the attribute unspecified.
testedwith = "ships-with-hg-core"

# storage format version; increment when the format changes
storageversion = 0

# namespaces
bookmarktype = "bookmark"
wdirparenttype = "wdirparent"
# In a shared repository, what shared feature name is used
# to indicate this namespace is shared with the source?
sharednamespaces: Dict[str, str] = {bookmarktype: hg.sharedbookmarks}


# Journal recording, register hooks and storage object
def extsetup(ui) -> None:
    extensions.wrapfunction(dispatch, "runcommand", runcommand)
    extensions.wrapfunction(bookmarks.bmstore, "_write", recordbookmarks)
    extensions.wrapfilecache(localrepo.localrepository, "dirstate", wrapdirstate)
    extensions.wrapfunction(hg, "postshare", wrappostshare)
    extensions.wrapfunction(hg, "copystore", unsharejournal)


def reposetup(ui, repo) -> None:
    repo.journal = journalstorage(repo)
    repo._wlockfreeprefix.add("namejournal")

    dirstate, cached = localrepo.isfilecached(repo, "dirstate")
    if cached:
        # already instantiated dirstate isn't yet marked as
        # "journal"-ing, even though repo.dirstate() was already
        # wrapped by own wrapdirstate()
        _setupdirstate(repo, dirstate)


def runcommand(orig, lui, repo, cmd, fullargs, *args):
    """Track the command line options for recording in the journal"""
    journalstorage.recordcommand(*fullargs)
    return orig(lui, repo, cmd, fullargs, *args)


def _setupdirstate(repo, dirstate) -> None:
    dirstate.journalstorage = repo.journal
    dirstate.addparentchangecallback("journal", recorddirstateparents)


# hooks to record dirstate changes
def wrapdirstate(orig, repo):
    """Make journal storage available to the dirstate object"""
    dirstate = orig(repo)
    if hasattr(repo, "journal"):
        _setupdirstate(repo, dirstate)
    return dirstate


def recorddirstateparents(dirstate, old, new) -> None:
    """Records all dirstate parent changes in the journal."""
    old = list(old)
    new = list(new)
    if hasattr(dirstate, "journalstorage"):
        # only record two hashes if there was a merge
        oldhashes = old[:1] if old[1] == node.nullid else old
        newhashes = new[:1] if new[1] == node.nullid else new
        dirstate.journalstorage.record(wdirparenttype, ".", oldhashes, newhashes)


# hooks to record bookmark changes (both local and remote)
def recordbookmarks(orig, store, fp):
    """Records all bookmark changes in the journal."""
    repo = store._repo
    if hasattr(repo, "journal"):
        oldmarks = bookmarks.bmstore(repo)
        for mark, value in store.items():
            oldvalue = oldmarks.get(mark, node.nullid)
            if value != oldvalue:
                repo.journal.record(bookmarktype, mark, oldvalue, value)
    return orig(store, fp)


def _mergeentriesiter(*iterables, **kwargs):
    """Given a set of sorted iterables, yield the next entry in merged order

    Note that by default entries go from most recent to oldest.
    """
    order = kwargs.pop(r"order", max)
    iterables = [iter(it) for it in iterables]
    # this tracks still active iterables; iterables are deleted as they are
    # exhausted, which is why this is a dictionary and why each entry also
    # stores the key. Entries are mutable so we can store the next value each
    # time.
    iterable_map = {}
    for key, it in enumerate(iterables):
        try:
            iterable_map[key] = [next(it), key, it]
        except StopIteration:
            # empty entry, can be ignored
            pass

    while iterable_map:
        value, key, it = order(iterable_map.values())
        yield value
        try:
            iterable_map[key][0] = next(it)
        except StopIteration:
            # this iterable is empty, remove it from consideration
            del iterable_map[key]


def wrappostshare(orig, sourcerepo, destrepo, **kwargs) -> None:
    """Mark this shared working copy as sharing journal information"""
    with destrepo.wlock():
        orig(sourcerepo, destrepo, **kwargs)
        with destrepo.localvfs("shared", "ab") as fp:
            fp.write(b"journal\n")


def unsharejournal(orig, ui, repo, repopath):
    """Copy shared journal entries into this repo when unsharing"""
    if repo.path == repopath and repo.shared() and hasattr(repo, "journal"):
        if repo.shared() and "journal" in repo.sharedfeatures:
            # there is a shared repository and there are shared journal entries
            # to copy. move shared data over from source to destination but
            # rename the local file first
            if repo.localvfs.exists("namejournal"):
                journalpath = repo.localvfs.join("namejournal")
                util.rename(journalpath, journalpath + ".bak")
            storage = repo.journal
            local = storage._open(
                repo.localvfs, filename="namejournal.bak", _newestfirst=False
            )
            shared = (
                e
                for e in storage._open(repo.sharedvfs, _newestfirst=False)
                if sharednamespaces.get(e.namespace) in repo.sharedfeatures
            )
            for entry in _mergeentriesiter(local, shared, order=min):
                storage._write(repo.localvfs, [entry])

    return orig(ui, repo, repopath)


revsetpredicate = registrar.revsetpredicate()


@revsetpredicate("oldnonobsworkingcopyparent")
def _oldnonobsworkingcopyparent(repo, subset, x):
    """``oldnonobsworkingcopyparent()``
    previous non-obsolete working copy parent
    """
    current_node = repo["."].node()
    for entry in repo.journal.filtered(namespace="wdirparent"):
        # Rebase can update wc to each commit as it goes. We don't want consider the last
        # rebased commit as our last parent.
        # TODO: this is fragile since "commands" records the actual CLI args, not the
        # canonical command name.
        if entry.command.startswith("rebase"):
            continue

        nodes = entry.oldhashes

        # Skip merge states.
        if len(nodes) != 1:
            continue

        if nodes[0] == current_node or repo[nodes[0]].obsolete():
            continue

        return subset & smartset.baseset(nodes, repo=repo)

    return set()


class journalstorage:
    """Storage for journal entries

    Entries are divided over two files; one with entries that pertain to the
    local working copy *only*, and one with entries that are shared across
    multiple working copies when shared using the share extension.

    Entries are stored with NUL bytes as separators. See the journalentry
    class for the per-entry structure.

    The file format starts with an integer version, delimited by a NUL.

    This storage uses a dedicated lock; this makes it easier to avoid issues
    with adding entries that added when the regular wlock is unlocked (e.g.
    the dirstate).

    """

    _currentcommand = ()
    _lockref = None

    def __init__(self, repo):
        self.user = util.getuser()
        self.ui = repo.ui
        self.localvfs = repo.localvfs
        self.sharedfeatures = repo.sharedfeatures
        if repo.shared() and "journal" in self.sharedfeatures:
            self.sharedvfs = repo.sharedvfs
        else:
            self.sharedvfs = None

    # track the current command for recording in journal entries
    @property
    def command(self):
        commandstr = " ".join(map(util.shellquote, journalstorage._currentcommand))
        if "\n" in commandstr:
            # truncate multi-line commands
            commandstr = commandstr.partition("\n")[0] + " ..."
        return commandstr

    @classmethod
    def recordcommand(cls, *fullargs):
        """Set the current @prog@ arguments, stored with recorded entries"""
        # Set the current command on the class because we may have started
        # with a non-local repo (cloning for example).
        cls._currentcommand = fullargs

    def _currentlock(self, lockref):
        """Returns the lock if it's held, or None if it's not.

        (This is copied from the localrepo class)
        """
        if lockref is None:
            return None
        l = lockref()
        if l is None or not l.held:
            return None
        return l

    def jlock(self, vfs):
        """Create a lock for the journal file"""
        if self._currentlock(self._lockref) is not None:
            raise error.Abort(_("journal lock does not support nesting"))
        desc = _("journal of %s") % vfs.base
        try:
            l = lock.lock(vfs, "namejournal.lock", 0, desc=desc, ui=self.ui)
        except error.LockHeld as inst:
            self.ui.warn(
                _("waiting for lock on %s held by %r\n") % (desc, inst.lockinfo)
            )
            # default to 600 seconds timeout
            l = lock.lock(
                vfs,
                "namejournal.lock",
                self.ui.configint("ui", "timeout"),
                desc=desc,
                ui=self.ui,
            )
            self.ui.warn(_("got lock after %s seconds\n") % l.delay)
        self._lockref = weakref.ref(l)
        return l

    def record(self, namespace, name, oldhashes, newhashes):
        """Record a new journal entry

        * namespace: an opaque string; this can be used to filter on the type
          of recorded entries.
        * name: the name defining this entry; for bookmarks, this is the
          bookmark name. Can be filtered on when retrieving entries.
        * oldhashes and newhashes: each a single binary hash, or a list of
          binary hashes. These represent the old and new position of the named
          item.

        """

        entry = self._buildjournalentry(namespace, name, oldhashes, newhashes)
        vfs = self._getvfs(namespace)

        self._write(vfs, [entry])

    def recordmany(self, namespace, entrydata):
        """Batch version of `record()`

        Records a lot of entries at once. namespace is the same as in `record()`
        `entrydata` is an iterable of `(name, oldhashes, newhashes)`
        """
        vfs = self._getvfs(namespace)

        entries = []
        for name, oldhashes, newhashes in entrydata:
            entry = self._buildjournalentry(namespace, name, oldhashes, newhashes)
            entries.append(entry)

        self._write(vfs, entries)

    def _getvfs(self, namespace):
        # write to the shared repository if this feature is being
        # shared between working copies.
        if (
            self.sharedvfs is not None
            and sharednamespaces.get(namespace) in self.sharedfeatures
        ):
            return self.sharedvfs
        else:
            return self.localvfs

    def _buildjournalentry(self, namespace, name, oldhashes, newhashes):
        if not isinstance(oldhashes, list):
            oldhashes = [oldhashes]
        if not isinstance(newhashes, list):
            newhashes = [newhashes]

        return rsjournal.journalentry(
            util.makedate(),
            self.user,
            self.command,
            namespace,
            name,
            oldhashes,
            newhashes,
        )

    def _write(self, vfs, entries):
        with self.jlock(vfs):
            version = None
            # open file in amend mode to ensure it is created if missing
            with vfs("namejournal", mode="a+b") as f:
                f.seek(0, os.SEEK_SET)
                # Read just enough bytes to get a version number (up to 2
                # digits plus separator)
                version = f.read(3).partition(b"\0")[0].decode()
                if version and version != str(storageversion):
                    # different version of the storage. Exit early (and not
                    # write anything) if this is not a version we can handle or
                    # the file is corrupt. In future, perhaps rotate the file
                    # instead?
                    self.ui.warn(_("unsupported journal file version '%s'\n") % version)
                    return
                if not version:
                    # empty file, write version first
                    f.write((str(storageversion) + "\0").encode())
                f.seek(0, os.SEEK_END)
                for entry in entries:
                    f.write(entry.serialize() + b"\0")

    def filtered(self, namespace=None, name=None):
        """Yield all journal entries with the given namespace or name

        Both the namespace and the name are optional; if neither is given all
        entries in the journal are produced.

        Matching supports regular expressions by using the `re:` prefix
        (use `literal:` to match names or namespaces that start with `re:`)

        """
        if namespace is not None:
            namespace = util.stringmatcher(namespace)[-1]
        if name is not None:
            name = util.stringmatcher(name)[-1]
        for entry in self:
            if namespace is not None and not namespace(entry.namespace):
                continue
            if name is not None and not name(entry.name):
                continue
            yield entry

    def __iter__(self):
        """Iterate over the storage

        Yields journalentry instances for each contained journal record.

        """
        local = self._open(self.localvfs)

        if self.sharedvfs is None:
            return local

        # iterate over both local and shared entries, but only those
        # shared entries that are among the currently shared features
        shared = (
            e
            for e in self._open(self.sharedvfs)
            if sharednamespaces.get(e.namespace) in self.sharedfeatures
        )
        return _mergeentriesiter(local, shared)

    def _open(self, vfs, filename="namejournal", _newestfirst=True):
        if not vfs.exists(filename):
            return

        with vfs(filename) as f:
            raw = f.read()

        lines = raw.split(b"\0")
        version = lines and lines[0].decode()
        if version != str(storageversion):
            version = version or _("not available")
            raise error.Abort(_("unknown journal file version '%s'") % version)

        # Skip the first line, it's a version number. Normally we iterate over
        # these in reverse order to list newest first; only when copying across
        # a shared storage do we forgo reversing.
        lines = lines[1:]
        if _newestfirst:
            lines = reversed(lines)
        for line in lines:
            if not line:
                continue
            try:
                yield rsjournal.journalentry.fromstorage(line)
            except ValueError as ex:
                self.ui.debug("skipping corrupt journalentry: %s" % ex)
                # If a journal entry is corrupt, just skip it.


# journal reading
# log options that don't make sense for journal
_ignoreopts = ("no-merges", "graph")


@command(
    "journal|jo",
    [
        ("", "all", None, "show history for all names"),
        ("c", "commits", None, "show commit metadata"),
    ]
    + [opt for opt in cmdutil.logopts if opt[1] not in _ignoreopts],
    "[OPTION]... [BOOKMARKNAME]",
    legacyaliases=["j", "jou", "jour", "journ", "journa"],
)
def journal(ui, repo, *args, **opts) -> None:
    """show the history of the checked out commit or a bookmark

    Show the history of all the commits that were once the current commit. In
    other words, shows a list of your previously checked out commits.
    :prog:`journal` can be used to find older versions of commits (for example,
    when you want to revert to a previous state). It can also be used to
    discover commits that were previously hidden.

    By default, :prog:`journal` displays the history of the current commit. To
    display a list of commits pointed to by a bookmark, specify a bookmark
    name.

    Specify ``--all`` to show the history of both the current commit and all
    bookmarks. In the output for ``--all``, bookmarks are listed by name, and
    ``.`` indicates the current commit.

    Specify ``-Tjson`` to produce machine-readable output.

    .. container:: verbose

      By default, :prog:`journal` only shows the commit hash and the
      corresponding command. Specify ``--verbose`` to also include the
      previous commit hash, user, and timestamp.

      Use ``-c/--commits`` to output log information about each commit
      hash. To customize the log output, you can also specify switches
      like ``--patch``, ``git``, ``--stat``, and ``--template``.

      If a bookmark name starts with ``re:``, the remainder of the name
      is treated as a regular expression. To match a name that actually
      starts with ``re:``, use the prefix ``literal:``.

    """
    name = "."
    if opts.get("all"):
        if args:
            raise error.Abort(_("You can't combine --all and filtering on a name"))
        name = None
    if args:
        name = args[0]

    fm = ui.formatter("journal", opts)
    ui.pager("journal")

    if not opts.get("template"):
        if name is None:
            displayname = _("the working copy and bookmarks")
        else:
            displayname = "'%s'" % name
        ui.status(_("previous locations of %s:\n") % displayname)

    limit = cmdutil.loglimit(opts)
    entry = None
    for count, entry in enumerate(repo.journal.filtered(name=name)):
        if count == limit:
            break
        newhashesstr = fm.formatlist(
            list(map(fm.hexfunc, entry.newhashes)), name="node", sep=","
        )
        oldhashesstr = fm.formatlist(
            list(map(fm.hexfunc, entry.oldhashes)), name="node", sep=","
        )

        fm.startitem()
        fm.templatedata(repo=repo)
        fm.condwrite(ui.verbose, "oldhashes", "%s -> ", oldhashesstr)
        fm.write("newhashes", "%s", newhashesstr)
        fm.condwrite(ui.verbose, "user", " %-8s", entry.user)
        fm.condwrite(
            opts.get("all") or name.startswith("re:"), "name", "  %-8s", entry.name
        )

        timestring = fm.formatdate(entry.timestamp, "%Y-%m-%d %H:%M %1%2")
        fm.condwrite(ui.verbose, "date", " %s", timestring)
        fm.write("command", "  %s\n", entry.command)

        if opts.get("commits"):
            displayer = cmdutil.show_changeset(ui, repo, opts, buffered=False)
            for hash in entry.newhashes:
                try:
                    ctx = repo[hash]
                    displayer.show(ctx)
                except error.RepoLookupError as e:
                    fm.write("repolookuperror", "%s\n\n", str(e))
            displayer.close()

    fm.end()

    if entry is None and not opts.get("template"):
        ui.status(_("no recorded locations\n"))
