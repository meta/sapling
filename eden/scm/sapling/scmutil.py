# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# scmutil.py - Mercurial core utility functions
#
#  Copyright Olivia Mackall <olivia@selenic.com>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.


import errno
import glob
import os
import re
import socket
import sys
import tempfile
import time

import bindings

from . import (
    copies,
    encoding,
    error,
    hintutil,
    match as matchmod,
    pathutil,
    phases,
    revsetlang,
    similar,
    smartset,
    util,
    vfs,
    visibility,
    winutil,
)
from .i18n import _
from .node import hex, nullid, short, wdirid, wdirrev

if util.iswindows:
    from . import scmwindows as scmplatform
else:
    from . import scmposix as scmplatform

termsize = scmplatform.termsize


class status(tuple):
    """Named tuple with a list of files per status. The 'deleted', 'unknown'
    and 'ignored' properties are only relevant to the working copy.
    """

    def __new__(
        cls,
        modified,
        added,
        removed,
        deleted,
        unknown,
        ignored,
        clean,
        invalid_path=None,
    ):
        for files in (modified, added, removed, deleted, unknown, ignored, clean):
            assert all(isinstance(f, str) for f in files)

        self = super().__new__(
            cls, (modified, added, removed, deleted, unknown, ignored, clean)
        )
        self.invalid_path = invalid_path or []
        return self

    @property
    def modified(self):
        """files that have been modified"""
        return self[0]

    @property
    def added(self):
        """files that have been added"""
        return self[1]

    @property
    def removed(self):
        """files that have been removed"""
        return self[2]

    @property
    def deleted(self):
        """files that are in the dirstate, but have been deleted from the
        working copy (aka "missing")
        """
        return self[3]

    @property
    def unknown(self):
        """files not in the dirstate that are not ignored"""
        return self[4]

    @property
    def ignored(self):
        """files not in the dirstate that are ignored (by _dirignore())"""
        return self[5]

    @property
    def clean(self):
        """files that have not been modified"""
        return self[6]

    def __repr__(self, *args, **kwargs):
        return (
            "<status modified=%r, added=%r, removed=%r, deleted=%r, "
            "unknown=%r, ignored=%r, clean=%r>"
        ) % self


def nochangesfound(ui, repo, excluded=None):
    """Report no changes for push/pull, excluded is None or a list of
    nodes excluded from the push/pull.
    """
    secretlist = []
    if excluded:
        for n in excluded:
            ctx = repo[n]
            if ctx.phase() >= phases.secret:
                secretlist.append(n)

    if secretlist:
        ui.status(
            _("no changes found (ignored %d secret changesets)\n") % len(secretlist)
        )
    else:
        ui.status(_("no changes found\n"))


def callcatch(ui, req, func):
    """call func() with global exception handling

    return func() if no exception happens. otherwise do some error handling
    and return an exit code accordingly. does not handle all exceptions.
    """
    trace = []
    category = None
    try:
        ret = _callcatch(ui, req, func, trace)
        if trace:
            category = _errorcategory(trace[0][1])
        return ret
    except BaseException as ex:
        category = _errorcategory(ex)
        raise
    finally:
        if trace:
            traceui, ex, formatted = trace[0]
            try:
                if category:
                    traceui.log("command_info", error_category=category)
                if category not in (error.USAGE_ERROR, error.CERTIFICATE_ERROR):
                    _uploadtraceback(traceui, str(ex), formatted)
            except Exception as ex:
                ui.warn(_("warning: could not log command traceback: %s\n") % ex)


def _errorcategory(ex):
    if isinstance(ex, error.CertificateError):
        return error.CERTIFICATE_ERROR
    return getattr(ex, "error_category", None)


def _callcatch(ui, req, func, trace):
    try:
        try:
            try:
                return func()
            except Exception as ex:  # re-raises
                # Swap in the repo's ui if available since this includes the repo's config.
                if req.cmdrepo:
                    ui = req.cmdrepo.ui

                ui.traceback()

                trace.append((ui, ex, util.smartformatexc()))

                raise
            finally:
                # Print 'remote:' messages before 'abort:' messages.
                # This also avoids sshpeer.__del__ during Py_Finalize -> GC
                # on Python 3, which can cause deadlocks waiting for the
                # stderr reading thread.
                from . import sshpeer

                sshpeer.cleanupall()

        except (
            error.HttpError,
            error.FetchError,
            error.NetworkError,
            error.TlsError,
        ) as inst:
            if ui.configbool("experimental", "network-doctor"):
                problem = bindings.doctor.diagnose_network(ui._rcfg)
                if problem:
                    fd, path = tempfile.mkstemp(prefix="hg-error-details-")
                    with util.fdopen(fd, "wb") as tmp:
                        tmp.write(
                            "Doctor output:\n{}\n\n{}\n\nOriginal Error:\n{}\n\n{}".format(
                                problem[0], problem[1], inst, util.smartformatexc()
                            ).encode()
                        )

                    ui.warn(
                        _(
                            "command failed due to network error (see {} for details)\n".format(
                                path
                            )
                        ),
                        error=_("abort"),
                    )
                    ui.warn("\n{}\n".format(problem[0]), label="doctor.treatment")
                    ui.note("  {}\n".format(problem[1]))
                    ui.debug("\nOriginal error:\n{}\n".format(inst))
                    if problem[2]:
                        inst.error_category = error.CERTIFICATE_ERROR
                    return 1

            raise

    # Global exception handling, alphabetically
    # Mercurial-specific first, followed by built-in and library exceptions
    except error.LockHeld as inst:
        if inst.errno == errno.ETIMEDOUT:
            reason = _("timed out waiting for lock held by %s") % inst.lockinfo
        else:
            reason = _("lock held by %r") % inst.lockinfo
        ui.warn(_("%s: %s\n") % (inst.desc or inst.filename, reason), error=_("abort"))
        if not inst.lockinfo:
            ui.warn(_("(lock might be very busy)\n"))
    except error.LockUnavailable as inst:
        ui.warn(
            _("could not lock %s: %s\n") % (inst.desc or inst.filename, inst.strerror),
            error=_("abort"),
        )
    except error.OutOfBandError as inst:
        if inst.args:
            msg = _("remote error:\n")
        else:
            msg = _("remote error\n")
        ui.warn(msg, error=_("abort"))
        if inst.args:
            ui.warn("".join(inst.args))
        if inst.hint:
            ui.warn("(%s)\n" % inst.hint, label="ui.hint")
    except error.RepoError as inst:
        ui.warn(_("%s!\n") % inst, error=_("abort"))
        inst.printcontext(ui)
        if inst.hint:
            ui.warn(_("(%s)\n") % inst.hint, label="ui.hint")
    except error.ResponseError as inst:
        ui.warn(inst.args[0], error=_("abort"))
        if not isinstance(inst.args[1], str):
            ui.warn(" %r\n" % (inst.args[1],))
        elif not inst.args[1]:
            ui.warn(_(" empty string\n"))
        else:
            ui.warn("\n%r\n" % util.ellipsis(inst.args[1]))
    except error.PermissionDeniedError as inst:
        path, hgid, request_acl, denial_message = inst.args
        rctx = getattr(getattr(ui, "_uiconfig", None), "_rctx", None)
        if rctx:
            msg = bindings.context.format_permission_denied(
                rctx, path, hgid, request_acl, denial_message
            )
        else:
            msg = "path '%s' is restricted by ACL '%s'" % (path, request_acl)
            if denial_message:
                msg += "\n%s" % denial_message
        ui.warn("%s\n" % msg, error=_("abort"))
        if req:
            req._permission_denied_handled = True
    except error.RepoAccessDeniedError as inst:
        ui.warn(_("permission denied: %s\n") % inst, error=_("abort"))
        ui.warn(_("(%s)\n") % repoaccesshint(ui), label="ui.hint")
    except error.CensoredNodeError as inst:
        ui.warn(_("file censored %s!\n") % inst, error=_("abort"))
    except error.CommitLookupError as inst:
        ui.warn(_("%s!\n") % inst.args[0], error=_("abort"))
    except error.CertificateError as inst:
        # This error is definitively due to a problem with the user's client
        # certificate, so print the configured remediation message.
        helptext = ui.config("help", "tlsauthhelp")
        if helptext is None:
            helptext = _("(run '@prog@ config auth' to see configured certificates)")
        ui.warn(
            _("%s!\n\n%s\n") % (inst.args[0], helptext),
            error=_("certificate error"),
        )
    except error.TlsError as inst:
        # This is a generic TLS error that may or may not be due to the user's
        # client certificate, so print a more generic message about TLS errors.
        helptext = ui.config("help", "tlshelp")
        if helptext is None:
            helptext = _("(is your client certificate valid?)")
        ui.warn(
            _("%s!\n\n%s\n") % (inst.args[0], helptext),
            error=_("tls error"),
        )
    except error.RevlogError as inst:
        ui.warn(_("%s!\n") % inst, error=_("abort"))
        inst.printcontext(ui)
    except error.InterventionRequired as inst:
        ui.warn("%s\n" % inst)
        if inst.hint:
            ui.warn(_("(%s)\n") % inst.hint, label="ui.hint")
        return 1
    except error.WdirUnsupported:
        ui.warn(_("working directory revision cannot be specified\n"), error=_("abort"))
    except error.Abort as inst:
        ui.warn(_("%s\n") % inst, error=_("abort"), component=inst.component)
        inst.printcontext(ui)
        if inst.hint:
            ui.warn(_("(%s)\n") % inst.hint, label="ui.hint")
        if req and getattr(inst, "permission_denied_handled", False):
            req._permission_denied_handled = True
        return inst.exitcode
    except (error.IndexedLogError, error.MetaLogError) as inst:
        ui.warn(_("internal storage is corrupted\n"), error=_("abort"))
        ui.warn(_("  %s\n\n") % str(inst).replace("\n", "\n  "))
        ui.warn(_("(this usually happens after hard reboot or system crash)\n"))
        ui.warn(_("(try '@prog@ doctor' to attempt to fix it)\n"))
    except (
        error.ConfigError,
        error.InvalidRepoPath,
        error.MaxFetchCountError,
        error.NonUTF8PathError,
        error.PathMatcherError,
        error.RepoInitError,
        error.WorkingCopyError,
        error.UncategorizedNativeError,
    ) as inst:
        ui.warn(_("%s\n") % inst, error=_("abort"))
    except ImportError as inst:
        ui.warn(_("%s!\n") % inst, error=_("abort"))
        m = str(inst).split()[-1]
        if m in "mpatch bdiff".split():
            ui.warn(_("(did you forget to compile extensions?)\n"))
        elif m in "zlib".split():
            ui.warn(_("(is your Python install correct?)\n"))
    except IOError as inst:
        if hasattr(inst, "code"):
            ui.warn(_("%s\n") % inst, error=_("abort"))
        elif hasattr(inst, "reason"):
            try:  # usually it is in the form (errno, strerror)
                reason = inst.reason.args[1]
            except (AttributeError, IndexError):
                # it might be anything, for example a string
                reason = inst.reason
            ui.warn(_("error: %s\n") % reason, error=_("abort"))
        elif (
            not ui.debugflag
            and hasattr(inst, "args")
            and inst.args
            and inst.args[0] == errno.EPIPE
            # "sl files . | head" yields "Broken pipe"
            # "sl files ." then "q" in streampager yields "pipe reader has been dropped"
            # But, Windows gives you a BrokenPipe error in different cases such as unlinking a file that is in use.
            # Let's only eat the error if it explicitly mentions "pipe".
            and "pipe" in inst.args[1]
        ):
            # Exit like a process killed by SIGPIPE, so callers can tell a
            # closed pipe from a real error (255).
            return 128 + 13
        elif getattr(inst, "strerror", None):
            filename = getattr(inst, "filename", None)
            if filename:
                ui.warn(
                    _("%s: %s\n") % (inst.strerror, inst.filename),
                    error=_("abort"),
                )
            else:
                ui.warn(_("%s\n") % inst.strerror, error=_("abort"))
            if not util.iswindows:
                # For permission errors on POSIX. Show more information about the
                # current user, group, and stat results.
                num = getattr(inst, "errno", None)
                if filename is not None and num in {errno.EACCES, errno.EPERM}:
                    if util.istest():
                        uid = 42
                    else:
                        uid = os.getuid()
                    ui.warn(_("(current process runs with uid %s)\n") % uid)
                    _printstat(ui, filename)
                    _printstat(ui, os.path.dirname(filename))
        else:
            ui.warn(_("%s\n") % inst, error=_("abort"))
    except OSError as inst:
        if getattr(inst, "filename", None) is not None:
            ui.warn(
                _("%s: %s\n") % (inst.strerror, inst.filename),
                error=_("abort"),
            )
        else:
            ui.warn(_("%s\n") % inst.strerror, error=_("abort"))
    except MemoryError:
        ui.warn(_("out of memory\n"), error=_("abort"))
    except SystemExit as inst:
        # Commands shouldn't sys.exit directly, but give a return code.
        # Just in case catch this and pass exit code to caller.
        return inst.code
    except socket.error as inst:
        ui.warn(_("%s\n") % inst.args[-1], error=_("abort"))
    except Exception as e:
        error_name = type(e).__name__
        if error_name == "EdenError":
            ui.warn(_("EdenError: %s\n") % e, error=_("abort"))
            if edenf_faq := ui.config("edenfs", "faq-page"):
                ui.warn(
                    _("(see %s for more troubleshooting information)\n") % edenf_faq
                )
        elif error_name == "TApplicationException":
            ui.warn(_("ThriftError: %s\n") % e, error=_("abort"))
            ui.warn(_("(try 'eden doctor' to diagnose this issue)\n"))
        else:
            raise

    return -1


def repoaccesshint(ui) -> str:
    """Hint for a user whose access to the repo was denied by the server."""
    return ui.config("help", "repo-access-help") or _(
        "check that you have access to this repository"
    )


def _uploadtraceback(ui, message, trace):
    key = "flat/errortrace-%(host)s-%(pid)s-%(time)s" % {
        "host": socket.gethostname(),
        "pid": os.getpid(),
        "time": time.time(),
    }

    payload = message + "\n\n" + trace
    # TODO: Move this into a background task that renders from
    # blackbox instead.
    ui.log("errortrace", "Trace:\n%s\n", trace, key=key, payload=payload)
    ui.log("errortracekey", "Trace key:%s\n", key, errortracekey=key)
    error_prefix = payload[:500]
    ui.log("error_prefix", "%s", error_prefix, error_prefix=error_prefix)


def _printstat(ui, path):
    """Attempt to print filesystem stat information on path"""
    if util.istest():
        mode = uid = gid = 42
    else:
        try:
            st = os.stat(path)
            mode = st.st_mode
            uid = st.st_uid
            gid = st.st_gid
        except Exception:
            return
    ui.warn(_("(%s: mode 0o%o, uid %s, gid %s)\n") % (path, mode, uid, gid))


def checknewlabel(repo, lbl, kind):
    # Do not use the "kind" parameter in ui output.
    # It makes strings difficult to translate.
    if lbl in ["tip", ".", "null"]:
        raise error.Abort(_("the name '%s' is reserved") % lbl)
    for c in (":", "\0", "\n", "\r"):
        if c in lbl:
            raise error.Abort(_("%r cannot be used in a name") % c)
    try:
        int(lbl)
        raise error.Abort(_("cannot use an integer as a name"))
    except ValueError:
        pass


def checkfilename(f):
    """Check that the filename f is an acceptable filename for a tracked file"""
    if "\r" in f or "\n" in f:
        raise error.Abort(_("'\\n' and '\\r' disallowed in filenames: %r") % f)


def checkportable(ui, f):
    """Check if filename f is portable and warn or abort depending on config"""
    checkfilename(f)
    abort, warn = checkportabilityalert(ui)
    if abort or warn:
        msg = winutil.checkwinfilename(f)
        if msg:
            msg = "%s: %s" % (msg, util.shellquote(f))
            if abort:
                raise error.Abort(msg)
            ui.warn(_("%s\n") % msg, notice=_("warning"))


def checkportabilityalert(ui):
    """check if the user's config requests nothing, a warning, or abort for
    non-portable filenames"""
    val = ui.config("ui", "portablefilenames")
    lval = val.lower()
    bval = util.parsebool(val)
    abort = lval == "abort"
    warn = bval or lval == "warn"
    if bval is None and not (warn or abort or lval == "ignore"):
        raise error.ConfigError(_("ui.portablefilenames value is invalid ('%s')") % val)
    return abort, warn


class casecollisionauditor:
    def __init__(self, ui, abort, dirstate):
        self._ui = ui
        self._abort = abort
        # Still need an in-memory set to collect files being tested, but
        # haven't been added to treestate yet.
        self._loweredfiles = set()
        self._dirstate = dirstate
        # The purpose of _newfiles is so that we don't complain about
        # case collisions if someone were to call this object with the
        # same filename twice.
        self._newfiles = set()

    def __call__(self, f):
        if f in self._newfiles:
            return
        fl = f.lower()
        ds = self._dirstate
        shouldwarn = False
        if f not in ds:
            dmap = ds._map

            for candidate in dmap.getfiltered(fl, str.lower):
                if candidate == f:
                    continue
                node = dmap.get(candidate)

                # Don't warn regarding untracked files.
                if not node or node[0] == "?":
                    continue

                shouldwarn = True
                break

        if not shouldwarn:
            shouldwarn = fl in self._loweredfiles and f not in ds
            self._loweredfiles.add(fl)
        if shouldwarn:
            msg = _("possible case-folding collision for %s") % f
            if self._abort:
                raise error.Abort(msg)
            self._ui.warn(_("%s\n") % msg, notice=_("warning"))
        self._newfiles.add(f)


def binnode(ctx):
    """Return binary node id for a given basectx"""
    node = ctx.node()
    if node is None:
        return wdirid
    return node


def intrev(ctx):
    """Return integer for a given basectx that can be used in comparison or
    arithmetic operation"""
    rev = ctx.rev()
    if rev is None:
        return wdirrev
    return rev


def formatchangeid(ctx, isxrepo=False):
    """Format changectx as '{node|formatnode}', which is the default
    template provided by cmdutil.changeset_templater
    """
    repo = ctx.repo()
    ui = repo.ui
    if ui.debugflag:
        hexfunc = hex
    else:
        hexfunc = short
    suffix = "~" if isxrepo else ""
    return hexfunc(binnode(ctx)) + suffix


def revsingle(repo, revspec, default=".", localalias=None):
    """Resolve a single revset with user-defined revset aliases.

    This should only be used for resolving user-provided command-line flags or
    arguments.

    For internal code paths not interacting with user-provided arguments,
    use repo.revs (ignores user-defined revset aliases) or repo.anyrevs
    (respects user-defined revset aliases) instead.
    """
    if not revspec and revspec != 0:
        return repo[default]

    # Used by amend/common calling rebase.rebase with non-string opts.
    if isinstance(revspec, int):
        return repo[revspec]

    l = revrange(repo, [revspec], localalias=localalias)
    if not l:
        raise error.Abort(_("empty revision set"))
    return repo[l.last()]


def _pairspec(revspec):
    tree = revsetlang.parse(revspec)
    return tree and tree[0] in ("range", "rangepre", "rangepost", "rangeall")


def revpair(repo, revs):
    if not revs:
        return repo.dirstate.p1(), None

    l = revrange(repo, revs)

    if not l:
        first = second = None
    elif l.isascending():
        first = l.min()
        second = l.max()
    elif l.isdescending():
        first = l.max()
        second = l.min()
    else:
        first = l.first()
        second = l.last()

    if first is None:
        raise error.Abort(_("empty revision range"))
    if (
        first == second
        and len(revs) >= 2
        and not all(revrange(repo, [r]) for r in revs)
    ):
        raise error.Abort(_("empty revision on one side of range"))

    # if top-level is range expression, the result must always be a pair
    if first == second and len(revs) == 1 and not _pairspec(revs[0]):
        return repo.lookup(first), None

    return repo.lookup(first), repo.lookup(second)


def revrange(repo, specs, localalias=None):
    """Execute 1 to many revsets and return the union.

    This is the preferred mechanism for executing revsets using user-specified
    config options, such as revset aliases.

    The revsets specified by ``specs`` will be executed via a chained ``OR``
    expression. If ``specs`` is empty, an empty result is returned.

    ``specs`` can contain integers, in which case they are assumed to be
    revision numbers.

    It is assumed the revsets are already formatted. If you have arguments
    that need to be expanded in the revset, call ``revsetlang.formatspec()``
    and pass the result as an element of ``specs``.

    Specifying a single revset is allowed.

    Returns a ``revset.abstractsmartset`` which is a list-like interface over
    integer revisions.

    This should only be used for resolving user-provided command-line flags or
    arguments.

    For internal code paths not interacting with user-provided arguments,
    use repo.revs (ignores user-defined revset aliases) or repo.anyrevs
    (respects user-defined revset aliases) instead.
    """
    # Used by amend/common calling rebase.rebase with non-string opts.
    if isinstance(specs, smartset.abstractsmartset):
        return specs
    allspecs = []
    for spec in specs:
        if isinstance(spec, int):
            # specs are usually strings. int means legacy code using rev
            # numbers. revsetlang no longer accepts int revs. Wrap it before
            # passing to revsetlang.
            spec = revsetlang.formatspec("%d", spec)
        allspecs.append(spec)
    legacyrevnum = repo.ui.config("devel", "legacy.revnum")
    with (
        repo.ui.configoverride({("devel", "legacy.revnum:real"): legacyrevnum}),
        repo.names.included_user(),
    ):
        return repo.anyrevs(allspecs, user=True, localalias=localalias)


def expandpats(pats):
    """Expand bare globs when running on windows.
    On posix we assume it already has already been done by sh."""
    if not util.expandglobs:
        return list(pats)
    ret = []
    for kindpat in pats:
        kind, pat = matchmod._patsplit(kindpat, None)
        if kind is None:
            try:
                globbed = glob.glob(pat)
            except re.error:
                globbed = [pat]
            if globbed:
                ret.extend(globbed)
                continue
        ret.append(kindpat)
    return ret


def matchandpats(
    ctx,
    pats=(),
    opts=None,
    globbed=False,
    default="relpath",
    badfn=None,
):
    """Return a matcher and the patterns that were used.
    The matcher will warn about bad matches, unless an alternate badfn callback
    is provided."""
    if pats == ("",):
        pats = []
    if opts is None:
        opts = {}
    if not globbed and default == "relpath":
        pats = expandpats(pats or [])

    seen_bad = set()

    def bad(f, msg):
        if f not in seen_bad:
            ctx.repo().ui.warn("%s: %s\n" % (m.rel(f), msg))
        seen_bad.add(f)

    if badfn is None:
        badfn = bad

    m = ctx.match(
        pats,
        opts.get("include"),
        opts.get("exclude"),
        default,
        badfn=badfn,
        warn=ctx.repo().ui.warn,
    )

    if m.always():
        pats = []
    return m, pats


def match(
    ctx,
    pats=(),
    opts=None,
    globbed: bool = False,
    default="relpath",
    badfn=None,
):
    """Return a matcher that will warn about bad matches."""
    m = matchandpats(ctx, pats, opts, globbed, default, badfn=badfn)[0]

    # Test some rare dirs that probably wouldn't match unless the
    # matcher matches everything. Test for "visitdir is True" which
    # indicates the lack of a traversal fast path.
    ui = ctx.repo().ui
    if all(
        m.visitdir(d) is True for d in (f"{ui.identity.dotdir()}/foo", "a/a/a", "z/z/z")
    ):
        hintutil.triggershow(
            ui,
            "match-full-traversal",
            ", ".join([*pats, *opts.get("include", ())]),
        )

    return m


def maybe_show_path_typo_hint(ui, repo, pats, *, rev=None, limit=5) -> None:
    hintname = "rel-path-typo"
    cwd = repo.getcwd()
    if (
        not pats
        or not cwd
        or ui.quiet
        or ui.plain("hint")
        or hintutil.isacked(ui, hintname)
    ):
        return

    checked = 0
    if not rev:  # None or []
        exists = repo.wvfs.exists
    else:
        startrev = revrange(repo, rev).fastmax()
        if startrev is None:
            # Avoid slow revset iteration.
            return
        exists = repo[startrev].__contains__

    for pat in pats:
        if matchmod.patkind(pat):
            continue
        if checked >= limit:
            return
        checked += 1

        try:
            cwdpath = pathutil.canonpath(repo.root, cwd, pat)
            rootpath = pathutil.canonpath(repo.root, "", pat)
            if cwdpath == rootpath:
                continue
            cwdpath_exists = exists(cwdpath)
            rootpath_exists = exists(rootpath)
        except (OSError, error.Abort):
            continue

        if not cwdpath_exists and rootpath_exists:
            hintutil.triggershow(ui, hintname, rootpath)
            return


def matchall(repo):
    """Return a matcher that will efficiently match everything."""
    return matchmod.always(repo.root, repo.getcwd())


def matchfiles(repo, files, badfn=None):
    """Return a matcher that will efficiently match exactly these files."""
    return matchmod.exact(repo.root, repo.getcwd(), files, badfn=badfn)


def parsefollowlinespattern(repo, rev, pat, msg):
    """Return a file name from `pat` pattern suitable for usage in followlines
    logic.
    """
    if not matchmod.patkind(pat):
        return pathutil.canonpath(repo.root, repo.getcwd(), pat)
    else:
        ctx = repo[rev]
        m = matchmod.match(repo.root, repo.getcwd(), [pat], ctx=ctx)
        files = [f for f in ctx if m(f)]
        if len(files) != 1:
            raise error.ParseError(msg)
        return files[0]


def origpath(ui, repo, filepath):
    """customize where .orig files are created

    Fetch user defined path from config file: [ui] origbackuppath = <path>
    Fall back to default (filepath with .orig suffix) if not specified
    """
    origbackuppath = ui.config("ui", "origbackuppath")
    if not origbackuppath:
        return filepath + ".orig"

    origbackuppath = origbackuppath.replace("@DOTDIR@", ui.identity.dotdir())

    # Convert filepath from an absolute path into a path inside the repo.
    filepathfromroot = util.normpath(os.path.relpath(filepath, start=repo.root))

    # Auto-correct identity path
    if origbackuppath.startswith("."):
        for ident in bindings.identity.all():
            dotdir = ident.dotdir()
            if origbackuppath.startswith(dotdir):
                origbackuppath = (
                    repo.ui.identity.dotdir() + origbackuppath[len(dotdir) :]
                )
                break

    origvfs = vfs.vfs(repo.wjoin(origbackuppath))
    origbackupdir = origvfs.dirname(filepathfromroot)
    if not origvfs.isdir(origbackupdir) or origvfs.islink(origbackupdir):
        ui.note(_("creating directory: %s\n") % origvfs.join(origbackupdir))

        # The configured backup root itself may be a file or symlink. Handle
        # that here instead of requiring the VFS to support a non-directory root.
        origbackuproot = origvfs.join("")
        if os.path.isfile(origbackuproot) or os.path.islink(origbackuproot):
            ui.note(_("removing conflicting file: %s\n") % origbackuproot)
            util.unlink(origbackuproot)

        # Remove any files that conflict with the backup file's path
        for f in reversed(list(util.finddirs(filepathfromroot))):
            if origvfs.isfileorlink(f):
                ui.note(_("removing conflicting file: %s\n") % origvfs.join(f))
                origvfs.unlink(f)
                break

        origvfs.makedirs(origbackupdir)

    if origvfs.isdir(filepathfromroot) and not origvfs.islink(filepathfromroot):
        ui.note(
            _("removing conflicting directory: %s\n") % origvfs.join(filepathfromroot)
        )
        origvfs.rmtree(filepathfromroot, forcibly=True)

    return origvfs.join(filepathfromroot)


class _containsnode:
    """proxy __contains__(node) to container.__contains__ which accepts revs"""

    def __init__(self, repo, revcontainer):
        self._torev = repo.changelog.rev
        self._revcontains = revcontainer.__contains__

    def __contains__(self, node):
        return self._revcontains(self._torev(node))


def cleanupnodes(repo, replacements, operation, moves=None, metadata=None):
    """do common cleanups when old nodes are replaced by new nodes

    That includes writing obsmarkers or stripping nodes, and moving bookmarks.
    (we might also want to move working directory parent in the future)

    By default, bookmark moves are calculated automatically from 'replacements',
    but 'moves' can be used to override that. Also, 'moves' may include
    additional bookmark moves that should not have associated obsmarkers.

    replacements is {oldnode: [newnode]} or a iterable of nodes if they do not
    have replacements. operation is a string, like "rebase".

    metadata is dictionary containing metadata to be stored in obsmarker if
    obsolescence is enabled.

    Return the calculated 'moves' mapping that is from a single old node to a
    single new node.
    """
    if not replacements and not moves:
        return {}

    # translate mapping's other forms
    if not hasattr(replacements, "items"):
        replacements = {n: () for n in replacements}

    # Calculate bookmark movements
    if moves is None:
        moves = {}
    # Unfiltered repo is needed since nodes in replacements might be hidden.
    unfi = repo
    for oldnode, newnodes in replacements.items():
        if oldnode in moves:
            continue
        if len(newnodes) > 1:
            # usually a split, take the one with biggest rev number
            newnode = next(unfi.set("max(%ln)", newnodes)).node()
        elif len(newnodes) == 0:
            # Handle them in a second loop
            continue
        else:
            newnode = newnodes[0]
        moves[oldnode] = newnode

    # Move bookmarks pointing to stripped commits backwards.
    # If hit a replaced node, use the replacement.
    def movebackwards(node):
        p1 = unfi.changelog.parents(node)[0]
        if p1 == nullid:
            return p1
        elif p1 in moves:
            return moves[p1]
        elif p1 in replacements:
            return movebackwards(p1)
        else:
            return p1

    for oldnode, newnodes in replacements.items():
        if oldnode in moves:
            continue
        assert len(newnodes) == 0
        moves[oldnode] = movebackwards(oldnode)

    with repo.transaction("cleanup") as tr:
        # Move bookmarks
        bmarks = repo._bookmarks
        bmarkchanges = []
        allnewnodes = [n for ns in replacements.values() for n in ns]

        # Move extra Git refs (only used for dotgit mode, git_refs is empty otherwise)
        metalog = repo.metalog()
        git_refs = metalog.get_git_refs()  # {name: oid}
        git_ref_by_oid = {}  # {oid: [name]}
        git_ref_changed = False
        for name, oid in git_refs.items():
            names = git_ref_by_oid.get(oid)
            if names is None:
                git_ref_by_oid[oid] = [name]
            else:
                names.append(name)

        for oldnode, newnode in moves.items():
            names = git_ref_by_oid.get(oldnode)
            if names:
                repo.ui.debug(
                    "moving git ref %r from %s to %s\n"
                    % (names, hex(oldnode), hex(newnode))
                )
                git_ref_changed = True
                for name in names:
                    git_refs[name] = newnode

            oldbmarks = repo.nodebookmarks(oldnode)
            if not oldbmarks:
                continue
            from . import bookmarks  # avoid import cycle

            repo.ui.debug(
                "moving bookmarks %r from %s to %s\n"
                % (oldbmarks, hex(oldnode), hex(newnode))
            )

            # Delete divergent bookmarks being parents of related newnodes
            deleterevs = repo.revs(
                "parents(roots(%ln & (::%n))) - parents(%n)",
                allnewnodes,
                newnode,
                oldnode,
            )
            deletenodes = _containsnode(repo, deleterevs)
            for name in oldbmarks:
                bmarkchanges.append((name, newnode))
                for b in bookmarks.divergent2delete(repo, deletenodes, name):
                    bmarkchanges.append((b, None))

        if bmarkchanges:
            bmarks.applychanges(repo, tr, bmarkchanges)
        if git_ref_changed:
            metalog.set_git_refs(git_refs)

        # adjust visibility, or strip nodes
        strip = True
        if visibility.tracking(repo):
            visibility.remove(repo, replacements.keys())
            strip = False

        if strip:
            from . import repair  # avoid import cycle

            tostrip = list(replacements)
            if tostrip:
                repair.delayedstrip(repo.ui, repo, tostrip, operation)

    # Notify ISL that commits are moved.
    ipc = bindings.nodeipc.IPC
    if ipc:
        ipc.send(
            {
                "type": "commitRewrite",
                "nodeMap": {hex(old): hex(new) for old, new in moves.items()},
            }
        )

    return moves


def addremove(
    repo,
    matcher,
    addremove: bool = True,
    automv: bool = True,
    similarity=None,
    dry_run: bool = False,
) -> int:
    m = matcher

    rename_detection_file_limit = None
    if automv:
        if similarity is None:
            similarity = repo.ui.configint("automv", "similarity")
        rename_detection_file_limit = repo.ui.configint("automv", "max-files")

    try:
        similarity = float(similarity or 0)
    except ValueError:
        raise error.Abort(_("similarity must be a number"))
    if similarity < 0 or similarity > 100:
        raise error.Abort(_("similarity must be between 0 and 100"))

    similarity = similarity / 100.0

    # Is there a better place for this?
    from . import git

    git.maybe_cleanup_submodule_in_treestate(repo)

    rejected = []

    def badfn(f, msg):
        if f in m.files():
            m.bad(f, msg)
        rejected.append(f)

    badmatch = matchmod.badmatch(m, badfn)
    added, unknown, deleted, removed, forgotten = _interestingfiles(repo, badmatch)

    if addremove:
        unknownset = set(unknown + forgotten)
        toprint = unknownset.copy()
        toprint.update(deleted)
        for abs in sorted(toprint):
            if repo.ui.verbose or not m.exact(abs):
                if abs in unknownset:
                    status = _("adding %s\n") % m.uipath(abs)
                else:
                    status = _("removing %s\n") % m.uipath(abs)
                repo.ui.status(status)

        # Tentatively include unknown and delete in added and removed for _findrenames() call.
        added += unknown
        removed += deleted

    renames = {}
    if added:
        if (
            rename_detection_file_limit
            and len(added) + len(removed) > rename_detection_file_limit
        ):
            repo.ui.status_err(_("too many files - skipping rename detection\n"))
        else:
            renames = _findrenames(repo, m, added, removed, similarity)

    if not dry_run:
        if addremove:
            _markchanges(repo, unknown + forgotten, deleted, renames)
        else:
            _markchanges(repo, [], [], renames)

    if addremove:
        for f in rejected:
            if f in m.files():
                return 1
    return 0


def marktouched(repo, files, similarity=0.0):
    """Assert that files have somehow been operated upon. files are relative to
    the repo root."""
    m = matchfiles(repo, files, badfn=lambda x, y: rejected.append(x))
    rejected = []

    added, unknown, deleted, removed, forgotten = _interestingfiles(repo, m)

    if repo.ui.verbose:
        unknownset = set(unknown + forgotten)
        toprint = unknownset.copy()
        toprint.update(deleted)
        for abs in sorted(toprint):
            if abs in unknownset:
                status = _("adding %s\n") % abs
            else:
                status = _("removing %s\n") % abs
            repo.ui.status(status)

    renames = _findrenames(repo, m, added + unknown, removed + deleted, similarity)

    _markchanges(repo, unknown + forgotten, deleted, renames)

    for f in rejected:
        if f in m.files():
            return 1
    return 0


def _interestingfiles(repo, matcher):
    """Walk dirstate with matcher, looking for files that addremove would care
    about.

    This is different from dirstate.status because it doesn't care about
    whether files are modified or clean."""
    removed, forgotten = [], []
    audit_path = pathutil.pathauditor(repo.root, cached=True)

    dirstate = repo.dirstate
    exists = repo.wvfs.isfileorlink
    status = dirstate.status(matcher, False, False, True)

    unknown = [file for file in status.unknown if audit_path.check(file)]

    for file in status.removed:
        # audit here to make sure "file" hasn't reappeared behind a symlink
        if exists(file) and audit_path.check(file):
            if dirstate.normalize(file) == file:
                forgotten.append(file)
            else:
                removed.append(file)
        else:
            removed.append(file)

    # The user may have specified ignored files. It's expensive to compute them
    # via status, so let's manually add them here.
    ignored = repo.dirstate._ignore
    unknown.extend(
        file
        for file in matcher.files()
        if ignored(file) and repo.wvfs.isfileorlink(file) and audit_path.check(file)
    )

    # remove the added files for which we already have copy info
    added = status.added
    copy = copies.pathcopies(repo["."], repo[None], matcher)
    added = [f for f in added if f not in copy]

    return added, unknown, status.deleted, removed, forgotten


def _findrenames(repo, matcher, added, removed, similarity):
    """Find renames from removed files to added ones."""
    renames = {}
    if similarity > 0:
        for old, new, score in similar.findrenames(repo, added, removed, similarity):
            if repo.ui.verbose or not matcher.exact(old) or not matcher.exact(new):
                repo.ui.status(
                    _("recording removal of %s as rename to %s (%d%% similar)\n")
                    % (matcher.rel(old), matcher.rel(new), score * 100)
                )
            renames[new] = old
    return renames


def _markchanges(repo, unknown, deleted, renames):
    """Marks the files in unknown as added, the files in deleted as removed,
    and the files in renames as copied."""
    wctx = repo[None]
    with repo.wlock():
        wctx.forget(deleted)
        wctx.add(unknown)
        for new, old in renames.items():
            wctx.copy(old, new)


def dirstatecopy(ui, repo, wctx, src, dst, dryrun: bool = False, cwd=None):
    """Update the dirstate to reflect the intent of copying src to dst. For
    different reasons it might not end with dst being marked as copied from src.
    """
    origsrc = repo.dirstate.copied(src) or src
    if dst == origsrc:  # copying back a copy?
        if repo.dirstate[dst] not in "mn" and not dryrun:
            repo.dirstate.normallookup(dst)
    else:
        if repo.dirstate[origsrc] == "a" and origsrc == src:
            if not ui.quiet:
                ui.warn(
                    _(
                        "%s has not been committed yet, so no copy "
                        "data will be stored for %s.\n"
                    )
                    % (repo.pathto(origsrc, cwd), repo.pathto(dst, cwd))
                )
            if repo.dirstate[dst] in "?r" and not dryrun:
                wctx.add([dst])
        elif not dryrun:
            wctx.copy(origsrc, dst)


def readrequires(opener, supported=None):
    """Reads and parses .hg/requires or .hg/store/requires and checks if all
    entries found are in the list of supported features.

    If supported is None, read all features without checking.
    """
    requirements = set(opener.readutf8("requires").splitlines())
    missing = []
    if supported:
        for r in requirements:
            if r not in supported:
                if not r or not r[0].isalnum():
                    raise error.RequirementError(
                        _("%s file is corrupt") % opener.join("requires")
                    )
                missing.append(r)
    missing.sort()
    if missing:
        raise error.RequirementError(
            _("repository requires features unknown to this @Product@: %s")
            % " ".join(missing),
            hint=_(
                "see https://mercurial-scm.org/wiki/MissingRequirement"
                " for more information"
            ),
        )
    return requirements


def writerequires(opener, requirements):
    content = "".join("%s\n" % r for r in sorted(requirements))
    opener.writeutf8("requires", content)


class filecachesubentry:
    def __init__(self, path, stat):
        self.path = path
        self.cachestat = None

        if stat:
            path = self.path
        else:
            path = None
        self.cachestat = filecachesubentry.stat(path)

    def refresh(self):
        self.cachestat = filecachesubentry.stat(self.path)

    def changed(self):
        newstat = filecachesubentry.stat(self.path)

        if self.cachestat != newstat:
            self.cachestat = newstat
            return True
        else:
            return False

    @staticmethod
    def stat(path):
        return util.cachestat(path)


class filecacheentry:
    def __init__(self, paths, stat: bool = True):
        self._entries = []
        for path in paths:
            self._entries.append(filecachesubentry(path, stat))

    def changed(self):
        """true if any entry has changed"""
        for entry in self._entries:
            if entry.changed():
                return True
        return False

    def refresh(self):
        for entry in self._entries:
            entry.refresh()


class filecache:
    """A property like decorator that tracks files under .hg/ for updates.

    Records stat info when called in _filecache.

    On subsequent calls, compares old stat info with new info, and recreates the
    object when any of the files changes, updating the new stat info in
    _filecache.

    Mercurial either atomic renames or appends for files under .hg,
    so to ensure the cache is reliable we need the filesystem to be able
    to tell us if a file has been replaced. If it can't, we fallback to
    recreating the object on every call (essentially the same behavior as
    propertycache).

    """

    def __init__(self, *paths):
        self.paths = [
            path if isinstance(path, tuple) else (path, self.join) for path in paths
        ]

    def join(self, obj, fname):
        """Used to compute the runtime path of a cached file.

        Users should subclass filecache and provide their own version of this
        function to call the appropriate join function on 'obj' (an instance
        of the class that its member function was decorated).
        """
        raise NotImplementedError

    def __call__(self, func):
        self.func = func
        self.name = func.__name__
        return self

    def __get__(self, obj, type=None):
        # if accessed on the class, return the descriptor itself.
        if obj is None:
            return self
        # do we need to check if the file changed?
        if self.name in obj.__dict__:
            assert self.name in obj._filecache, self.name
            return obj.__dict__[self.name]

        entry = obj._filecache.get(self.name)

        if entry:
            if entry.changed():
                entry.obj = self.func(obj)
        else:
            paths = [joiner(obj, path) for (path, joiner) in self.paths]

            # We stat -before- creating the object so our cache doesn't lie if
            # a writer modified between the time we read and stat
            entry = filecacheentry(paths, True)
            entry.obj = self.func(obj)

            obj._filecache[self.name] = entry

        obj.__dict__[self.name] = entry.obj
        return entry.obj

    def __set__(self, obj, value):
        if self.name not in obj._filecache:
            # we add an entry for the missing value because X in __dict__
            # implies X in _filecache
            paths = [joiner(obj, path) for (path, joiner) in self.paths]
            ce = filecacheentry(paths, False)
            obj._filecache[self.name] = ce
        else:
            ce = obj._filecache[self.name]

        ce.obj = value  # update cached copy
        obj.__dict__[self.name] = value  # update copy returned by obj.x

    def __delete__(self, obj):
        try:
            del obj.__dict__[self.name]
        except KeyError:
            raise AttributeError(self.name)


class keyedcache:
    """
    Property cache based on key. Changing the key invalidates the cache.

    Example:

        >>> class Object1:
        ...     count = 0
        ...     def __init__(self):
        ...         self.key = 1
        ...     @keyedcache(lambda o: o.key)
        ...     def next(self):
        ...         Object1.count += 1
        ...         return Object1.count
        >>> o = Object1()
        >>> o.next
        1
        >>> o.next # cached
        1
        >>> o.key = 2 # invalidate cache
        >>> o.next # new value
        2

    """

    NOT_SET = object()

    def __init__(self, key):
        """key: function to obtain 'key' from 'self'."""
        self.key_function = key
        self.current_key = self.NOT_SET

    def __call__(self, func):
        # apply the decorator
        assert callable(func)
        self.func = func
        self.name = func.__name__
        return self

    def __get__(self, obj, objtype=None):
        # class property access
        if obj is None:
            return self
        # property get
        new_key = self.key_function(obj)
        changed = self.current_key is self.NOT_SET or new_key != self.current_key
        if changed:
            # invalidate property cache on key change
            obj.__dict__.pop(self.name, None)
            self.current_key = new_key
        if self.name not in obj.__dict__:
            # populate cache
            obj.__dict__[self.name] = self.func(obj)
        return obj.__dict__[self.name]

    # does not support __set__

    def __delete__(self, obj):
        try:
            del obj.__dict__[self.name]
        except KeyError:
            raise AttributeError(self.name)


def gdinitconfig(ui):
    """helper function to know if a repo should be created as general delta"""
    # experimental config: format.generaldelta
    return ui.configbool("format", "generaldelta") or ui.configbool(
        "format", "usegeneraldelta"
    )


def gddeltaconfig(ui):
    """helper function to know if incoming delta should be optimised"""
    # experimental config: format.generaldelta
    return ui.configbool("format", "generaldelta")


class simplekeyvaluefile:
    """A simple file with key=value lines

    Keys must be alphanumerics and start with a letter, values must not
    contain '\n' characters"""

    firstlinekey = "__firstline"

    def __init__(self, vfs, path, keys=None):
        self.vfs = vfs
        self.path = path

    def read(self, firstlinenonkeyval: bool = False):
        """Read the contents of a simple key-value file

        'firstlinenonkeyval' indicates whether the first line of file should
        be treated as a key-value pair or reuturned fully under the
        __firstline key."""
        lines = self.vfs.readutf8(self.path).splitlines(True)
        d = {}
        if firstlinenonkeyval:
            if not lines:
                e = _("empty simplekeyvalue file")
                raise error.CorruptedState(e)
            # we don't want to include '\n' in the __firstline
            d[self.firstlinekey] = lines[0][:-1]
            del lines[0]

        try:
            # the 'if line.strip()' part prevents us from failing on empty
            # lines which only contain '\n' therefore are not skipped
            # by 'if line'
            updatedict = dict(line[:-1].split("=", 1) for line in lines if line.strip())
            if self.firstlinekey in updatedict:
                e = _("%r can't be used as a key")
                raise error.CorruptedState(e % self.firstlinekey)
            d.update(updatedict)
        except ValueError as e:
            raise error.CorruptedState(str(e))
        return d

    def write(self, data, firstline=None):
        """Write key=>value mapping to a file
        data is a dict. Keys must be alphanumerical and start with a letter.
        Values must not contain newline characters.

        If 'firstline' is not None, it is written to file before
        everything else, as it is, not in a key=value form"""
        lines = []
        if firstline is not None:
            lines.append("%s\n" % firstline)

        for k, v in data.items():
            if k == self.firstlinekey:
                e = "key name '%s' is reserved" % self.firstlinekey
                raise error.ProgrammingError(e)
            if not k[0].isalpha():
                e = "keys must start with a letter in a key-value file"
                raise error.ProgrammingError(e)
            if not k.isalnum():
                e = "invalid key name in a simple key-value file"
                raise error.ProgrammingError(e)
            if "\n" in v:
                e = "invalid value in a simple key-value file"
                raise error.ProgrammingError(e)
            lines.append("%s=%s\n" % (k, v))
        with self.vfs(self.path, mode="wb", atomictemp=True) as fp:
            fp.write("".join(lines).encode("utf-8"))


def nodesummaries(repo, nodes, maxnumnodes=4):
    if len(nodes) <= maxnumnodes or repo.ui.verbose:
        return " ".join(short(h) for h in nodes)
    first = " ".join(short(h) for h in nodes[:maxnumnodes])
    return _("%s and %d others") % (first, len(nodes) - maxnumnodes)


def wrapconvertsink(sink):
    """Allow extensions to wrap the sink returned by convcmd.convertsink()
    before it is used, whether or not the convert extension was formally loaded.
    """
    return sink


def contextnodesupportingwdir(ctx):
    """Returns `ctx`'s node, or `wdirid` if it is a `workingctx`.

    Alas, `workingxtx.node()` normally returns None, necessitating this
    convenience function for when you need to serialize the workingxctx.

    `repo[wdirid]` works fine so there's no need the reverse function.
    """
    from sapling import context

    if isinstance(ctx, context.workingctx):
        return wdirid

    # Neither `None` nor `wdirid` feels right here:
    if isinstance(ctx, context.overlayworkingctx):
        raise error.ProgrammingError(
            "contextnodesupportingwdir doesn't support overlayworkingctx"
        )

    return ctx.node()


def trackrevnumfortests(repo, specs):
    """Attempt to collect information to replace revision number with revset
    expressions in tests.

    This works with the TESTFILE and TESTLINE environment variable set by
    run-tests.py.

    Information will be written to $TESTDIR/.testrevnum.
    """
    if not util.istest():
        return

    trackrevnum = encoding.environ.get("TRACKREVNUM")
    testline = encoding.environ.get("TESTLINE")
    testfile = encoding.environ.get("TESTFILE")
    testdir = encoding.environ.get("TESTDIR")
    if not trackrevnum or not testline or not testfile or not testdir:
        return

    for spec in specs:
        # 'spec' should be in sys.argv
        if not any(spec in a for a in sys.argv):
            continue
        # Consider 'spec' as a revision number.
        rev = int(spec)
        if rev < -1:
            continue
        ctx = repo[rev]
        if not ctx:
            return

        # Check candidate revset expressions.
        candidates = []
        if rev == -1:
            candidates.append("null")
        desc = ctx.description()
        if desc:
            candidates.append("desc(%s)" % desc.split()[0])
            candidates.append("max(desc(%s))" % desc.split()[0])
        candidates.append("%s" % ctx.hex())

        for candidate in candidates:
            try:
                nodes = list(repo.nodes(candidate))
            except Exception:
                continue
            if nodes == [ctx.node()]:
                with open(testdir + "/.testrevnum", "ab") as f:
                    f.write(
                        "fix(%r, %s, %r, %r)\n" % (testfile, testline, spec, candidate)
                    )
                break


_use_f64_compat = True


def revf64encode(rev):
    """Convert rev to within f64 "safe" range.

    This avoids issues that JSON cannot represent the revs precisely.
    """
    if _use_f64_compat and rev is not None and rev >= 0x100000000000000:
        rev -= 0xFF000000000000
    return rev


def revf64decode(rev):
    """Convert rev encoded by revf64encode back to the original rev

    >>> revs = [i + j for i in [0, 1 << 56] for j in range(2)] + [None]
    >>> encoded = [revf64encode(i) for i in revs]
    >>> decoded = [revf64decode(i) for i in encoded]
    >>> revs == decoded
    True
    """
    if (
        _use_f64_compat
        and rev is not None
        and 0x1000000000000 <= rev < 0x100000000000000
    ):
        rev += 0xFF000000000000
    return rev


def setup(ui):
    # Disable f64 compatibility
    global _use_f64_compat
    _use_f64_compat = ui.configbool("experimental", "revf64compat")


def rootrelpath(ctx, path):
    """Convert a path or a relative path pattern to a root relative path."""
    SUPPORTED_PAT_KINDS = {"path", "relpath"}
    if kind := matchmod.patkind(path):
        if kind not in SUPPORTED_PAT_KINDS:
            raise error.Abort(_("unsupported pattern kind: '%s'"), kind)
    files = ctx.match(pats=[path], default="relpath").files()
    if not files:
        # path is repo root directory
        return ""
    # this should be true since we only pass "path" or "relpath" pattern kinds to match()
    assert len(files) == 1, f"path '{path}' should match exactly one file path"
    return files[0]


def rootrelpaths(ctx, paths):
    """Convert a list of path or relative path patterns to root relative paths."""
    return [rootrelpath(ctx, path) for path in paths]


def walkfiles(repo, walkctx, matcher, base=None, nodes_only: bool = False):
    """Return a list (path, filenode) pairs that match the matcher in the given context."""
    mf = walkctx.manifest()
    if base is None and hasattr(mf, "walkfiles"):
        # If there is no base, skip diff and use more efficient walk.
        return mf.walkfiles(matcher, nodes_only=nodes_only)
    else:
        basemf = repo[base or nullid].manifest()
        return [
            (p, n[0])
            for p, (n, _o) in mf.diff(basemf, matcher, nodes_only=nodes_only).items()
            if n[0]
        ]


def publicbase(repo, ctx):
    base = repo.revs("max(::%d & public())", ctx.rev())
    if len(base):
        return repo[base.first()]
    return None
