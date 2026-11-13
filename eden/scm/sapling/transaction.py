# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# transaction.py - simple journaling scheme for mercurial
#
# This transaction scheme is intended to gracefully handle program
# errors and interruptions. More serious failures like system crashes
# can be recovered with an fsck-like tool. As the whole repository is
# effectively log-structured, this should amount to simply truncating
# anything that isn't referenced in the changelog.
#
# Copyright 2005, 2006 Olivia Mackall <olivia@selenic.com>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.


import errno
import sys

import bindings

from . import error, json, util
from .i18n import _
from .node import bin, hex

version = 2

# These are the file generators that should only be executed after the
# finalizers are done, since they rely on the output of the finalizers (like
# the changelog having been written).
postfinalizegenerators = {"bookmarks", "dirstate"}

gengroupall = "all"
gengroupprefinalize = "prefinalize"
gengrouppostfinalize = "postfinalize"

# Environment variable name to store metalog pending.
# Format: JSON-serialized {path: hex(root)}
ENV_PENDING_METALOG = "HG_PENDING_METALOG"

METALOG_TRACKED = set(bindings.metalog.tracked())


def decodependingmetalog(content):
    result = {}
    try:
        if content:
            parsed = json.loads(content)
            if isinstance(parsed, dict):
                result = {path: bin(hexroot) for path, hexroot in parsed.items()}
    except Exception:
        pass
    return result


def encodependingmetalog(pathroots):
    return json.dumps(
        {path: hex(binroot) for path, binroot in (pathroots or {}).items()}
    )


def active(func):
    def _active(self, *args, **kwds):
        if self.count == 0:
            raise error.Abort(
                _("cannot use transaction when it is already committed/aborted")
            )
        return func(self, *args, **kwds)

    return _active


def lockfree_incompatible(func):
    def _check_lockfree(self, *args, **kwargs):
        if self.lockfree:
            raise error.ProgrammingError("unsupported in lockfree transaction")
        return func(self, *args, **kwargs)

    return _check_lockfree


def _playback(
    journal,
    report,
    opener,
    vfsmap,
    entries,
    backupentries,
    unlink: bool = True,
    checkambigfiles=None,
) -> None:
    for f, o, _ignore in entries:
        if o or not unlink:
            checkambig = checkambigfiles and (f, "") in checkambigfiles
            try:
                util.truncatefile(f, opener, o, checkambig=checkambig)
            except IOError:
                report(_("failed to truncate %s\n") % f)
                raise
        else:
            try:
                opener.unlink(f)
            except (IOError, OSError) as inst:
                if inst.errno != errno.ENOENT:
                    raise

    backupfiles = []
    for l, f, b, c in backupentries:
        if l not in vfsmap and c:
            report("couldn't handle %s: unknown cache location %s\n" % (b, l))
        vfs = vfsmap[l]
        try:
            if f and b:
                filepath = vfs.join(f)
                backuppath = vfs.join(b)
                checkambig = checkambigfiles and (f, l) in checkambigfiles
                try:
                    util.copyfile(backuppath, filepath, checkambig=checkambig)
                    backupfiles.append(b)
                except IOError:
                    report(_("failed to recover %s\n") % f)
            else:
                target = f or b
                try:
                    vfs.unlink(target)
                except (IOError, OSError) as inst:
                    if inst.errno != errno.ENOENT:
                        raise
        except (IOError, OSError, error.Abort):
            if not c:
                raise

    backuppath = "%s.backupfiles" % journal
    if opener.exists(backuppath):
        opener.unlink(backuppath)
    opener.unlink(journal)
    try:
        for f in backupfiles:
            if opener.exists(f):
                opener.unlink(f)
    except (IOError, OSError, error.Abort):
        # only pure backup file remains, it is sage to ignore any error
        pass


class transaction(util.transactional):
    def __init__(
        self,
        report,
        opener,
        vfsmap,
        journalname,
        undoname=None,
        after=None,
        createmode=None,
        validator=None,
        releasefn=None,
        checkambigfiles=None,
        uiconfig=None,
        desc=None,
        lockfree=False,
        pendingroot=None,
        sharedpendingroot=None,
    ):
        """Begin a new transaction

        Begins a new transaction that allows rolling back writes in the event of
        an exception.

        * `after`: called after the transaction has been committed
        * `createmode`: the mode of the journal file that will be created
        * `releasefn`: called after releasing (with transaction and result)

        `checkambigfiles` is a set of (path, vfs-location) tuples,
        which determine whether file stat ambiguity should be avoided
        for corresponded files.

        If `lockfree` is True, the transaction does not require a repo lock,
        and during the lifetime of this transaction, subsequent repo locks
        (but not wlocks) become no-ops, and nested transactions are lockfree too.
        Certain traditional features like addfilegenerator, addbackup, etc. are
        unsupported in a lockfree transaction.
        """
        self.count = 1
        self.usages = 1
        self.report = report
        self.desc = desc
        self.lockfree = lockfree
        # Repo roots used to scope legacy pending files for child processes.
        self._pendingroot = pendingroot
        self._sharedpendingroot = sharedpendingroot
        # a vfs to the store content
        self.opener = opener
        # a map to access file in various {location -> vfs}
        vfsmap = vfsmap.copy()
        vfsmap[""] = opener  # set default value
        self._vfsmap = vfsmap
        self.after = after
        self.entries = []
        self.map = {}
        self.journal = journalname
        self.undoname = undoname
        self._queue = []
        # A callback to validate transaction content before closing it.
        # should raise exception is anything is wrong.
        # target user is repository hooks.
        if validator is None:
            validator = lambda tr: None
        self.validator = validator
        # A callback to do something just after releasing transaction.
        if releasefn is None:
            releasefn = lambda tr, success: None
        self.releasefn = releasefn

        self.checkambigfiles = set()
        if checkambigfiles:
            self.checkambigfiles.update(checkambigfiles)

        self.uiconfig = uiconfig

        # A dict dedicated to precisely tracking the changes introduced in the
        # transaction.
        self.changes = {}

        # a dict of arguments to be passed to hooks
        self.hookargs = {}
        if lockfree:
            self.file = None
        else:
            self.file = opener.open(self.journal, "wb")

        # a list of ('location', 'path', 'backuppath', cache) entries.
        # - if 'backuppath' is empty, no file existed at backup time
        # - if 'path' is empty, this is a temporary transaction file
        # - if 'location' is not empty, the path is outside main opener reach.
        #   use 'location' value as a key in a vfsmap to find the right 'vfs'
        # (cache is currently unused)
        self._backupentries = []
        self._backupmap = {}
        self._backupjournal = "%s.backupfiles" % self.journal
        if lockfree:
            self._backupsfile = None
        else:
            self._backupsfile = opener.open(self._backupjournal, "wb")
            self._backupsfile.write(b"%d\n" % version)

        if createmode is not None and not lockfree:
            opener.chmod(self.journal, createmode & 0o666)
            opener.chmod(self._backupjournal, createmode & 0o666)

        # hold file generations to be performed on commit
        self._filegenerators = {}
        # hold callback to write pending data for hooks
        self._pendingcallback = {}
        # holds callback to call when writing the transaction
        self._finalizecallback = {}
        # hold callback for post transaction close
        self._postclosecallback = {}
        # holds callbacks to call during abort
        self._abortcallback = {}
        # Reload metalog state when entering transaction.
        metalog = (
            opener.invalidatemetalog() if hasattr(opener, "invalidatemetalog") else None
        )
        if metalog and metalog.isdirty():
            # |<- A ->|<----------- repo lock --------->|
            #         |<- B ->|<- transaction ->|<- C ->|
            #          ^^^^^^^
            raise error.ProgrammingError(
                "metalog should not be changed before transaction"
            )

        self._spans = []
        self._pushspan(desc)

    def __del__(self):
        try:
            if self.journal:
                self._abort()
        except AttributeError:
            # AttributeError: 'transaction' object has noattribute 'journal'
            pass

    def _pushspan(self, desc):
        self._spans.append(
            util.tracer.span([("name", f"Transaction: {desc}"), ("cat", "transaction")])
        )
        util.tracer.enter(self._spans[-1])

    def _popspan(self):
        if self._spans:
            util.tracer.exit(self._spans.pop())

    @active
    def startgroup(self):
        """delay registration of file entry

        This is used by strip to delay vision of strip offset. The transaction
        sees either none or all of the strip actions to be done."""
        self._queue.append([])

    @active
    def endgroup(self):
        """apply delayed registration of file entry.

        This is used by strip to delay vision of strip offset. The transaction
        sees either none or all of the strip actions to be done."""
        q = self._queue.pop()
        for f, o, data in q:
            self._addentry(f, o, data)

    @active
    def add(self, file, offset, data=None):
        """record the state of an append-only file before update"""
        if file in self.map or file in self._backupmap:
            return
        if self._queue:
            self._queue[-1].append((file, offset, data))
            return

        self._addentry(file, offset, data)

    @lockfree_incompatible
    def _addentry(self, file, offset, data):
        """add a append-only entry to memory and on-disk state
        Raises in a lockfree transaction.
        """
        if file in self.map or file in self._backupmap:
            return
        self.entries.append((file, offset, data))
        self.map[file] = len(self.entries) - 1
        # add enough data to the journal to do the truncate
        self.file.write(b"%s\0%d\n" % (file.encode(), offset))
        self.file.flush()

    @active
    @lockfree_incompatible
    def addbackup(self, file, hardlink=True, location=""):
        """Adds a backup of the file to the transaction

        Calling addbackup() creates a hardlink backup of the specified file
        that is used to recover the file in the event of the transaction
        aborting.

        * `file`: the file path, relative to .hg/store
        * `hardlink`: use a hardlink to quickly create the backup

        Raises in a lockfree transaction.
        """
        if self._queue:
            msg = 'cannot use transaction.addbackup inside "group"'
            raise error.ProgrammingError(msg)

        if (
            file in self.map
            or file in self._backupmap
            or file in bindings.metalog.tracked()
        ):
            return
        vfs = self._vfsmap[location]
        dirname, filename = vfs.split(file)
        backupfilename = "%s.backup.%s" % (self.journal, filename)
        backupfile = vfs.reljoin(dirname, backupfilename)
        if vfs.exists(file):
            filepath = vfs.join(file)
            backuppath = vfs.join(backupfile)
            util.copyfile(filepath, backuppath, hardlink=hardlink)
        else:
            backupfile = ""

        self._addbackupentry((location, file, backupfile, False))

    @lockfree_incompatible
    def _addbackupentry(self, entry):
        """register a new backup entry and write it to disk
        Raises in a lockfree transaction.
        """
        self._backupentries.append(entry)
        self._backupmap[entry[1]] = len(self._backupentries) - 1
        self._backupsfile.write(("%s\0%s\0%s\0%d\n" % entry).encode())
        self._backupsfile.flush()

    @active
    def registertmp(self, tmpfile, location=""):
        """register a temporary transaction file

        Such files will be deleted when the transaction exits (on both
        failure and success).
        """
        self._addbackupentry((location, "", tmpfile, False))

    @active
    def addfilegenerator(self, genid, filenames, genfunc, order=0, location=""):
        """add a function to generate some files at transaction commit

        The `genfunc` argument is a function capable of generating proper
        content of each entry in the `filename` tuple.

        At transaction close time, `genfunc` will be called with one file
        object argument per entries in `filenames`.

        The transaction itself is responsible for the backup, creation and
        final write of such file.

        The `genid` argument is used to ensure the same set of file is only
        generated once. Call to `addfilegenerator` for a `genid` already
        present will overwrite the old entry.

        The `order` argument may be used to control the order in which multiple
        generator will be executed.

        The `location` arguments may be used to indicate the files are located
        outside of the standard directory for transaction. It should match
        one of the key of the `transaction.vfsmap` dictionary.

        In a lockfree transaction, filenames must be tracked by metalog.
        """
        if self.lockfree:
            incompatible_filenames = [f for f in filenames if f not in METALOG_TRACKED]
            if incompatible_filenames:
                raise error.ProgrammingError(
                    f"addfilegenerator requires metalog tracked filenames {METALOG_TRACKED} in a lockfree transaction, incompatible filenames: {incompatible_filenames}"
                )
        # For now, we are unable to do proper backup and restore of custom vfs
        # but for bookmarks that are handled outside this mechanism.
        self._filegenerators[genid] = (order, filenames, genfunc, location)

    @active
    def removefilegenerator(self, genid):
        """reverse of addfilegenerator, remove a file generator function"""
        if genid in self._filegenerators:
            del self._filegenerators[genid]

    def _generatefiles(self, suffix="", group=gengroupall):
        # write files registered for generation
        pending = bool(suffix)
        wrotephysical = False
        for id, entry in sorted(self._filegenerators.items()):
            order, filenames, genfunc, location = entry

            # for generation at closing, check if it's before or after finalize
            postfinalize = group == gengrouppostfinalize
            if group != gengroupall and (id in postfinalizegenerators) != postfinalize:
                continue

            vfs = self._vfsmap[location]
            files = []
            try:
                for name in filenames:
                    if pending and not location and name in METALOG_TRACKED:
                        files.append(vfs.metaopen(name, "w", mirror=False))
                        continue
                    wrotephysical = True
                    name += suffix
                    if suffix:
                        self.registertmp(name, location=location)
                        checkambig = False
                    else:
                        if not self.lockfree:
                            self.addbackup(name, location=location)
                        checkambig = (name, location) in self.checkambigfiles
                    files.append(vfs(name, "w", atomictemp=True, checkambig=checkambig))
                genfunc(*files)
            finally:
                for f in files:
                    f.close()
        return wrotephysical

    @active
    def find(self, file):
        if file in self.map:
            return self.entries[self.map[file]]
        if file in self._backupmap:
            return self._backupentries[self._backupmap[file]]
        return None

    @active
    @lockfree_incompatible
    def replace(self, file, offset, data=None):
        """
        replace can only replace already committed entries
        that are not pending in the queue
        Raises in a lockfree transaction.
        """
        if file not in self.map:
            raise KeyError(file)
        index = self.map[file]
        self.entries[index] = (file, offset, data)
        self.file.write(b"%s\0%d\n" % (file.encode(), offset))
        self.file.flush()

    @active
    def nest(self, desc):
        self._pushspan(desc)
        self.count += 1
        self.usages += 1
        return self

    def release(self):
        self._popspan()
        if self.count > 0:
            self.usages -= 1
        # if the transaction scopes are left without being closed, fail
        if self.count > 0 and self.usages == 0:
            self._abort()

    def running(self):
        return self.count > 0

    def addpending(self, category, callback):
        """add a callback to be called when the transaction is pending

        The transaction will be given as callback's first argument.

        Category is a unique identifier to allow overwriting an old callback
        with a newer callback.
        """
        self._pendingcallback[category] = callback

    @active
    def writepending(self, env=None):
        """write pending files

        This is used to allow hooks to view a transaction before commit

        If `env` is not None, it is a dictionary that will be mutated to
        include information to pick up pending changes.
        """
        for cat, callback in sorted(self._pendingcallback.items()):
            callback(self)
        islegacypending = self._generatefiles(suffix=".pending")

        if env is not None and islegacypending:
            pendingenv = (
                ("HG_PENDING", self._pendingroot),
                ("HG_SHAREDPENDING", self._sharedpendingroot),
            )
            for name, root in pendingenv:
                if root is not None:
                    env[name] = root

        # Write pending metalog changes. Other processes can load the
        # metalog with rootid set to `mlrootid` explicitly to see the
        # changes. But the changes won't be visible if the rootid is
        # not explicitly set.
        ml = self._vfsmap[""].metalog
        if ml.isdirty():
            rootid = ml.commit("(transaction pending)", pending=True)
            if env is not None:
                # Set the environment variable to specify metalog root for the
                # current metalog.
                pathroots = decodependingmetalog(env.get(ENV_PENDING_METALOG))
                pathroots[ml.path()] = rootid
                env[ENV_PENDING_METALOG] = encodependingmetalog(pathroots)
        else:
            if env is not None:
                # Clear the environment variable for metalog path.
                pathroots = decodependingmetalog(env.get(ENV_PENDING_METALOG))
                pathroots.pop(ml.path(), None)
                if pathroots:
                    env[ENV_PENDING_METALOG] = encodependingmetalog(pathroots)
                else:
                    env.pop(ENV_PENDING_METALOG, None)

    @active
    def addfinalize(self, category, callback):
        """add a callback to be called when the transaction is closed

        The transaction will be given as callback's first argument.

        Category is a unique identifier to allow overwriting old callbacks with
        newer callbacks.
        """
        self._finalizecallback[category] = callback

    @active
    def addpostclose(self, category, callback):
        """add or replace a callback to be called after the transaction closed

        The transaction will be given as callback's first argument.

        Category is a unique identifier to allow overwriting an old callback
        with a newer callback.
        """
        self._postclosecallback[category] = callback

    @active
    def getpostclose(self, category):
        """return a postclose callback added before, or None"""
        return self._postclosecallback.get(category, None)

    @active
    def addabort(self, category, callback):
        """add a callback to be called when the transaction is aborted.

        The transaction will be given as the first argument to the callback.

        Category is a unique identifier to allow overwriting an old callback
        with a newer callback.
        """
        self._abortcallback[category] = callback

    @active
    def close(self):
        """commit the transaction"""
        if self.count == 1:
            self.validator(self)  # will raise exception if needed
            self.validator = None  # Help prevent cycles.
            self._generatefiles(group=gengroupprefinalize)
            categories = sorted(self._finalizecallback)
            for cat in categories:
                self._finalizecallback[cat](self)
            # Prevent double usage and help clear cycles.
            self._finalizecallback = None
            self._generatefiles(group=gengrouppostfinalize)

        self.count -= 1
        if self.count != 0:
            return
        if not self.lockfree:
            self.file.close()
            self._backupsfile.close()
        # cleanup temporary files
        for l, f, b, c in self._backupentries:
            if l not in self._vfsmap and c:
                self.report("couldn't remove %s: unknown cache location %s\n" % (b, l))
                continue
            vfs = self._vfsmap[l]
            if not f and b and vfs.exists(b):
                try:
                    vfs.unlink(b)
                except (IOError, OSError, error.Abort) as inst:
                    if not c:
                        raise
                    # Abort may be raise by read only opener
                    self.report("couldn't remove %s: %s\n" % (vfs.join(b), inst))
        self.entries = []
        self._writeundo()
        self._writemetalog()

        if self.after:
            self.after()
            self.after = None  # Help prevent cycles.
        if not self.lockfree:
            if self.opener.isfile(self._backupjournal):
                self.opener.unlink(self._backupjournal)
            if self.opener.isfile(self.journal):
                self.opener.unlink(self.journal)
        for l, _f, b, c in self._backupentries:
            if l not in self._vfsmap and c:
                self.report("couldn't remove %s: unknown cache location%s\n" % (b, l))
                continue
            vfs = self._vfsmap[l]
            if b and vfs.exists(b):
                try:
                    vfs.unlink(b)
                except (IOError, OSError, error.Abort) as inst:
                    if not c:
                        raise
                    # Abort may be raise by read only opener
                    self.report("couldn't remove %s: %s\n" % (vfs.join(b), inst))

        self._backupentries = []
        self.journal = None

        self.releasefn(self, True)  # notify success of closing transaction
        self.releasefn = None  # Help prevent cycles.

        # run post close action
        categories = sorted(self._postclosecallback)
        for cat in categories:
            self._postclosecallback[cat](self)
        # Prevent double usage and help clear cycles.
        self._postclosecallback = None

    @active
    def abort(self):
        """abort the transaction (generally called on error, or when the
        transaction is not explicitly committed before going out of
        scope)"""
        self._abort()

    def _writeundo(self):
        """write transaction data for possible future undo call
        Does nothing in a lockfree transaction.
        """
        if self.lockfree:
            return
        if self.undoname is None:
            return
        undobackupfile = self.opener.open("%s.backupfiles" % self.undoname, "wb")
        undobackupfile.write(("%d\n" % version).encode())
        for l, f, b, c in self._backupentries:
            if not f:  # temporary file
                continue
            if not b:
                u = ""
            else:
                if l not in self._vfsmap and c:
                    self.report(
                        "couldn't remove %s: unknown cache location%s\n" % (b, l)
                    )
                    continue
                vfs = self._vfsmap[l]
                base, name = vfs.split(b)
                assert name.startswith(self.journal), name
                uname = name.replace(self.journal, self.undoname, 1)
                u = vfs.reljoin(base, uname)
                util.copyfile(vfs.join(b), vfs.join(u), hardlink=True)
            undobackupfile.write(("%s\0%s\0%s\0%d\n" % (l, f, u, c)).encode())
        undobackupfile.close()

    def _writemetalog(self):
        """write data managed by svfs.metalog"""
        # Write metalog.
        svfs = self._vfsmap[""]
        metalog = getattr(svfs, "metalog", None)
        if metalog:
            # write down configs used by the repo for debugging purpose
            if self.uiconfig and self.uiconfig.configbool("metalog", "track-config"):
                metalog.set("config", self.uiconfig.configtostring().encode())

            command = " ".join(map(util.shellquote, sys.argv[1:]))
            parent = "Parent: %s" % hex(metalog.root())
            trdesc = "Transaction: %s" % self.desc
            message = "\n".join([command, parent, trdesc])

            try:
                util.failpoint("transaction-metalog-commit")
            except Exception:
                # Explicit clean up.
                # Otherwise cleanup might rely on __del__ and run at a wrong time.
                self._abort()
                raise
            metalog.commit(
                message,
                int(util.timer()),
            )
            # Discard metalog state when exiting transaction.
            del svfs.__dict__["metalog"]

    def _abort(self):
        self.count = 0
        self.usages = 0
        if not self.lockfree:
            self.file.close()
            self._backupsfile.close()

        # Discard metalog state when exiting transaction.
        svfs = self._vfsmap[""]
        if hasattr(svfs, "invalidatemetalog"):
            svfs.invalidatemetalog()

        try:
            if not self.entries and not self._backupentries:
                if not self.lockfree:
                    if self._backupjournal:
                        self.opener.unlink(self._backupjournal)
                    if self.journal:
                        self.opener.unlink(self.journal)
                return

            self.report(_("transaction abort!\n"))

            try:
                for cat in sorted(self._abortcallback):
                    self._abortcallback[cat](self)
                # Prevent double usage and help clear cycles.
                self._abortcallback = None
                if not self.lockfree:
                    _playback(
                        self.journal,
                        self.report,
                        self.opener,
                        self._vfsmap,
                        self.entries,
                        self._backupentries,
                        False,
                        checkambigfiles=self.checkambigfiles,
                    )
                    self.report(_("rollback completed\n"))
            except BaseException:
                self.report(_("rollback failed - please run @prog@ recover\n"))
        finally:
            self.journal = None
            self.releasefn(self, False)  # notify failure of transaction
            self.releasefn = None  # Help prevent cycles.


def rollback(opener, vfsmap, file, report, checkambigfiles=None) -> None:
    """Rolls back the transaction contained in the given file

    Reads the entries in the specified file, and the corresponding
    '*.backupfiles' file, to recover from an incomplete transaction.

    * `file`: a file containing a list of entries, specifying where
    to truncate each file.  The file should contain a list of
    file\0offset pairs, delimited by newlines. The corresponding
    '*.backupfiles' file should contain a list of file\0backupfile
    pairs, delimited by \0.

    `checkambigfiles` is a set of (path, vfs-location) tuples,
    which determine whether file stat ambiguity should be avoided at
    restoring corresponded files.
    """
    entries = []
    backupentries = []

    fp = opener.open(file, "rb")
    lines = fp.readlines()
    fp.close()
    for l in lines:
        l = l.decode()
        try:
            f, o = l.split("\0")
            entries.append((f, int(o), None))
        except ValueError:
            report(_("couldn't read journal entry %r!\n") % l)

    backupjournal = "%s.backupfiles" % file
    if opener.exists(backupjournal):
        fp = opener.open(backupjournal, "rb")
        lines = fp.readlines()
        if lines:
            ver = lines[0][:-1].decode()
            if ver == str(version):
                for line in lines[1:]:
                    if line:
                        # Shave off the trailing newline
                        line = line[:-1]
                        line = line.decode()
                        try:
                            l, f, b, c = line.split("\0")
                        except ValueError:
                            raise AssertionError(
                                "Invalid line format in {}: {}".format(
                                    backupjournal, line
                                )
                            )
                        backupentries.append((l, f, b, bool(c)))
            else:
                report(_("journal was created by a different version of @Product@\n"))

    _playback(
        file,
        report,
        opener,
        vfsmap,
        entries,
        backupentries,
        checkambigfiles=checkambigfiles,
    )
