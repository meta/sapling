# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# dispatch.py - command dispatching for mercurial
#
# Copyright 2005-2007 Olivia Mackall <olivia@selenic.com>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.


import difflib
import errno
import getopt
import os
import pdb
import signal
import sys
import time

import bindings

from . import (
    alerts,
    blackbox,
    cmdutil,
    color,
    commands,
    encoding,
    error,
    extensions,
    hg,
    hgdemandimport as demandimport,
    hintutil,
    hook,
    i18n,
    perftrace,
    profiling,
    progress,
    registrar,
    scmutil,
    tracing,
    ui as uimod,
    util,
)
from .i18n import _, _x

cliparser = bindings.cliparser
rscontext = bindings.context.context


unrecoverablewrite = registrar.command.unrecoverablewrite


class request:
    def __init__(
        self,
        args,
        ui=None,
        repo=None,
        fin=None,
        fout=None,
        ferr=None,
        prereposetups=None,
        skipprehooks=False,
    ):
        self.args = args
        self.ui = ui
        self.repo = repo
        self.skipprehooks = skipprehooks
        self._permission_denied_handled = False

        # The repo, if any, that ends up being used for command execution.
        self.cmdrepo = None

        # input/output/error streams
        if fin and not isinstance(fin, util.refcell):
            fin = util.refcell(fin)
        if fout and not isinstance(fout, util.refcell):
            fout = util.refcell(fout)
        if ferr and not isinstance(ferr, util.refcell):
            ferr = util.refcell(ferr)

        self.fin = fin
        self.fout = fout
        self.ferr = ferr

        # remember options pre-parsed by _earlyparseopts()
        self.earlyoptions = {}

        # reposetups which run before extensions.
        self.prereposetups = prereposetups or []

    def _runexithandlers(self):
        # Silence potential EPIPE or SIGPIPE errors when writing to stdout or
        # stderr.
        if hasattr(signal, "SIGPIPE"):
            try:
                util.signal(signal.SIGPIPE, signal.SIG_IGN)
            except ValueError:
                # This can happen if the command runs as a library from a
                # non-main thread.
                if not util.istest():
                    raise

        class ignoreerrorui(self.ui.__class__):
            def _write(self, *args, **kwargs):
                try:
                    return super(ignoreerrorui, self)._write(*args, **kwargs)
                except (OSError, IOError):
                    pass

            def _write_err(self, *args, **kwargs):
                try:
                    return super(ignoreerrorui, self)._write_err(*args, **kwargs)
                except (OSError, IOError):
                    pass

        exc = None
        self.ui.__class__ = ignoreerrorui

        handlers = self.ui._exithandlers
        try:
            while handlers:
                func, args, kwargs = handlers.pop()
                try:
                    func(*args, **kwargs)
                except:  # re-raises below
                    if exc is None:
                        exc = sys.exc_info()[1]
                    self.ui.warn(_x("error in exit handlers:\n"))
                    self.ui.traceback(force=True)
        finally:
            if exc is not None:
                raise exc


def run(args, fin, fout, ferr, rctx: rscontext, skipprehooks: bool):
    "run the command in sys.argv"
    _initstdio()

    ui = uimod.ui(rctx=rctx)

    if not ui or ui.configbool("experimental", "mercurial-shim", True):
        from . import mercurialshim

        # Insert at the beginning so nested module imports are
        # redirected to the true module instead of the shim module
        # (even though the shim module appears in sys.modules).
        sys.meta_path.insert(0, mercurialshim.MercurialImporter())

    req = request(
        args[1:], fin=fin, fout=fout, ferr=ferr, ui=ui, skipprehooks=skipprehooks
    )
    err = None
    try:
        status = (dispatch(req) or 0) & 255
    except error.StdioError as e:
        err = e
        status = -1
    if hasattr(req.ui, "fout"):
        try:
            req.ui.fout.flush()
        except IOError as e:
            err = e
            status = -1
    if hasattr(req.ui, "ferr"):
        if err is not None and err.errno != errno.EPIPE:
            errormsg = err.strerror.encode()
            req.ui.ferr.write(b"abort: %s\n" % errormsg)
        req.ui.ferr.flush()
    sys.exit(status & 255)


def _preimportmodules():
    """pre-import modules that are side-effect free"""
    extmods = []
    extprefix = "sapling.ext."
    modnames = sorted(bindings.modules.list())

    is_win = util.iswindows
    win_modnames = {
        "sapling.scmwindows",
        "sapling.win32",
        "sapling.windows_socket",
        "sapling.windows",
    }

    for name in modnames:
        # Skip other modules.
        if not any(name.startswith(p) for p in ("ghstack", "sapling")):
            continue
        # Skip windows modules on non-windows platforms
        if not is_win and name in win_modnames:
            continue
        # Extensions are handled below.
        if name.startswith(extprefix):
            parts = name.split(".")
            if len(parts) == 3:
                extmods.append(parts[-1])
            continue
        # Skip side-effect main modules.
        if name.endswith("__main__"):
            continue
        try:
            __import__(name)
        except (ImportError, AttributeError):
            # some modules might fail to import due to incompatible OS.
            pass
    for extname in extmods:
        try:
            extensions.preimport(extname)
        except (ImportError, AttributeError):
            # some extensions might fail to import due to incompatible OS.
            pass


def _initstdio():
    for fp in (sys.stdin, sys.stdout, sys.stderr):
        util.setbinary(fp)


def _getsimilar(symbols, value):
    sim = lambda x: difflib.SequenceMatcher(None, value, x).ratio()
    # The cutoff for similarity here is pretty arbitrary. It should
    # probably be investigated and tweaked.
    return [s for s in symbols if sim(s) > 0.6]


def _reportsimilar(write, similar):
    if len(similar) == 1:
        write(_("(did you mean %s?)\n") % similar[0])
    elif similar:
        ss = ", ".join(sorted(similar))
        write(_("(did you mean one of %s?)\n") % ss)


def _formatparse(write, inst):
    similar = []
    if isinstance(inst, error.UnknownIdentifier):
        # make sure to check fileset first, as revset can invoke fileset
        similar = _getsimilar(inst.symbols, inst.function)
    if len(inst.args) > 1:
        write(_("@prog@: parse error at %s: %s\n") % (inst.args[1], inst.args[0]))
        if inst.args[0][0] == " ":
            write(_("unexpected leading whitespace\n"))
    else:
        write(_("@prog@: parse error: %s\n") % inst.args[0])
        _reportsimilar(write, similar)
    if inst.hint:
        write(_("(%s)\n") % inst.hint)


def _formatargs(args):
    return " ".join(util.shellquote(a) for a in args)


def _check_permission_denied_paths(req, ret):
    """Warn about permission-denied paths and optionally change exit code."""
    if req._permission_denied_handled:
        return ret

    rctx = getattr(getattr(req.ui, "_uiconfig", None), "_rctx", None)
    if rctx is None:
        return ret

    warning, acl_details, exit_nonzero = bindings.context.check_permission_denied(rctx)
    if warning is not None:
        req.ui.warn(warning)
        for detail in acl_details:
            req.ui.warn(detail)

    if exit_nonzero:
        return ret if ret else 1

    return ret


def dispatch(req):
    "run the command specified in req.args"
    hintutil.clear()
    if req.ferr:
        ferr = req.ferr
    elif req.ui:
        ferr = req.ui.ferr
    else:
        ferr = util.stderr

    try:
        if not req.ui:
            req.ui = uimod.ui.load()
        req.earlyoptions.update(_earlyparseopts(req.args))
        if req.earlyoptions["traceback"] or req.earlyoptions["trace"]:
            req.ui.setconfig("ui", "traceback", "on", "--traceback")

        # set ui streams from the request
        if req.fin:
            req.ui.fin = req.fin
        if req.fout:
            req.ui.fout = req.fout
        if req.ferr:
            req.ui.ferr = req.ferr
    except error.Abort as inst:
        ferr.write(_("abort: %s\n") % inst)
        if inst.hint:
            ferr.write(_("(%s)\n") % inst.hint)
        return -1
    except error.ParseError as inst:
        _formatparse(ferr.write, inst)
        return -1

    cmdmsg = _formatargs(req.args)
    starttime = util.timer()
    ret = None
    retmask = 255

    def logatexit():
        ui = req.ui
        if ui.logmeasuredtimes:
            ui.log("measuredtimes", **(ui._measuredtimes))
        blackbox.sync()

    versionthresholddays = req.ui.configint("ui", "version-age-threshold-days")
    versionagedays = util.versionagedays()
    if versionthresholddays and versionagedays > versionthresholddays:
        hintutil.trigger("old-version", versionagedays)

    # by registering this exit handler here, we guarantee that it runs
    # after other exithandlers, like the killpager one
    req.ui.atexit(logatexit)

    try:
        ret = _runcatch(req)
        ret = _check_permission_denied_paths(req, ret)
    except error.ProgrammingError as inst:
        req.ui.warn(_("** ProgrammingError: %s\n") % inst)
        if inst.hint:
            req.ui.warn(_("** (%s)\n") % inst.hint, label="ui.hint")
        raise
    except error.SignalInterrupt:
        # maybe pager would quit without consuming all the output, and
        # SIGPIPE was raised. we cannot print anything in this case.
        pass
        ret = -1
    except IOError as inst:
        # Windows does not have SIGPIPE, so pager exit does not
        # get raised as a SignalInterrupt. Let's handle the error
        # explicitly here
        if not util.iswindows or inst.errno != errno.EINVAL:
            raise
        ret = -1
    finally:
        duration = util.timer() - starttime
        req.ui.flush()
        req.ui.log(
            "command_finish",
            "%s exited %d after %0.2f seconds\n",
            cmdmsg,
            ret or 0,
            duration,
        )

        retmask = req.ui.configint("ui", "exitcodemask")

        try:
            req._runexithandlers()
        except:  # exiting, so no re-raises
            ret = ret or -1
    if ret is None:
        ret = 0
    if retmask is not None:
        ret = ret & retmask
    return ret


def _runcatch(req):
    def catchterm(*args):
        raise error.SignalInterrupt

    ui = req.ui
    try:
        # SIGTERM and SIGHUP are handled by Rust (ctrlc crate).
        for name in ["SIGBREAK"]:
            num = getattr(signal, name, None)
            if num:
                util.signal(num, catchterm)
    except ValueError:
        pass  # happens if called in a thread
    # In some cases, SIGINT handler is set to SIG_IGN on OSX.
    # Reset it to raise KeyboardInterrupt.
    sigint = getattr(signal, "SIGINT", None)
    if sigint is not None and util.getsignal(sigint) == signal.SIG_IGN:
        util.signal(sigint, signal.default_int_handler)

    realcmd = None
    try:
        cmdargs = cliparser.parseargs(req.args[:])
        cmd = cmdargs[0]
        aliases, entry = cmdutil.findcmd(cmd, commands.table)
        realcmd = aliases[0]
    except (
        error.UnknownCommand,
        error.AmbiguousCommand,
        IndexError,
        getopt.GetoptError,
        UnicodeDecodeError,
    ):
        # Don't handle this here. We know the command is
        # invalid, but all we're worried about for now is that
        # it's not a command that server operators expect to
        # be safe to offer to users in a sandbox.
        pass

    def _runcatchfunc():
        if realcmd == "serve" and "--stdio" in cmdargs:
            # Uncontionally turn off narrow-heads. The hg servers use
            # full, revlog-based repos. Keep them using old revlog-based
            # algorithms and do not risk switching to new algorithms.
            if "SSH_CLIENT" in encoding.environ and req.ui.configbool(
                "experimental", "disable-narrow-heads-ssh-server"
            ):
                req.ui.setconfig("experimental", "narrow-heads", "false", "serve")

            # We want to constrain 'hg serve --stdio' instances pretty
            # closely, as many shared-ssh access tools want to grant
            # access to run *only* 'hg -R $repo serve --stdio'. We
            # restrict to exactly that set of arguments, and prohibit
            # any repo name that starts with '--' to prevent
            # shenanigans wherein a user does something like pass
            # --debugger or --config=ui.debugger=1 as a repo
            # name. This used to actually run the debugger.
            if (
                len(req.args) != 4
                or req.args[0] != "-R"
                or req.args[1].startswith("--")
                or req.args[2] != "serve"
                or req.args[3] != "--stdio"
            ):
                raise error.Abort(
                    _("potentially unsafe serve --stdio invocation: %r") % (req.args,)
                )

        try:
            debugger = "pdb"
            debugtrace = {"pdb": pdb.set_trace}
            debugmortem = {"pdb": pdb.post_mortem}

            # read --config before doing anything else
            # (e.g. to change trust settings for reading .hg/hgrc)
            req.ui.setclioverrides(
                req.earlyoptions["config"], req.earlyoptions["configfile"]
            )
            if req.repo:
                req.repo.ui.setclioverrides(
                    req.earlyoptions["config"], req.earlyoptions["configfile"]
                )

            # developer config: ui.debugger
            debugger = ui.config("ui", "debugger")
            debugmod = pdb
            if not debugger or ui.plain():
                # if we are in HGPLAIN mode, then disable custom debugging
                debugger = "pdb"
            elif req.earlyoptions["debugger"]:
                # This import can be slow for fancy debuggers, so only
                # do it when absolutely necessary, i.e. when actual
                # debugging has been requested
                with demandimport.deactivated():
                    try:
                        debugmod = __import__(debugger)
                    except ImportError:
                        pass  # Leave debugmod = pdb

            debugtrace[debugger] = debugmod.set_trace
            debugmortem[debugger] = debugmod.post_mortem

            # enter the debugger before command execution
            if req.earlyoptions["debugger"]:
                ui.disablepager()
                ui.warn(
                    _(
                        "entering debugger - "
                        "type c to continue starting hg or h for help\n"
                    )
                )

                if debugger != "pdb" and debugtrace[debugger] == debugtrace["pdb"]:
                    ui.warn(
                        _("%s debugger specified but its module was not found\n")
                        % debugger
                    )
                with demandimport.deactivated():
                    debugtrace[debugger]()
            try:
                return _dispatch(req)
            finally:
                ui.flush()
        except:  # re-raises
            # Potentially enter the debugger when we hit an exception
            startdebugger = req.earlyoptions["debugger"]
            if startdebugger:
                debugmod = getdebugmod()
                ui.write_err(util.smartformatexc())
                debugmod.post_mortem(sys.exc_info()[2])
                os._exit(255)
            raise

    return _callcatch(ui, req, _runcatchfunc)


def _callcatch(ui, req, func):
    """like scmutil.callcatch but handles more high-level exceptions about
    config parsing and commands. besides, use handlecommandexception to handle
    uncaught exceptions.
    """
    try:
        return scmutil.callcatch(ui, req, func)
    except error.AmbiguousCommand as inst:
        ui.warn(_("@prog@: command '%s' is ambiguous:\n") % inst.args[0])

        for match in inst.args[1]:
            cmds = match.split(" or ")
            parts = [cmd.partition(inst.args[0]) for cmd in cmds]
            msg = " or ".join(
                ui.label(part[1], "ui.prefix.component") + part[2] for part in parts
            )

            ui.write("\t%s\n" % msg)

    except error.CommandError as inst:
        if inst.args[0]:
            msgbytes = str(inst.args[1])
            ui.warn(_("@prog@ %s: %s\n") % (inst.args[0], msgbytes))
            ui.warn(_("(use '@prog@ %s -h' to get help)\n") % (inst.args[0],))
        else:
            ui.warn(_("@prog@: %s\n") % inst.args[1])
            ui.warn(_("(use '@prog@ -h' to get help)\n"))
    except error.ParseError as inst:
        _formatparse(ui.warn, inst)
        return -1
    except error.UnknownCommand as inst:
        ui.warn(
            _("unknown command %r\n(use '@prog@ help' to get help)\n") % inst.args[0]
        )
    except error.UnknownSubcommand as inst:
        cmd, subcmd = inst.args[:2]
        if subcmd is not None:
            nosubcmdmsg = _("@prog@ %s: unknown subcommand '%s'\n") % (cmd, subcmd)
        else:
            nosubcmdmsg = _("@prog@ %s: subcommand required\n") % cmd
        ui.warn(nosubcmdmsg)
    except IOError:
        raise
    except:  # probably re-raises
        # Potentially enter ipdb debugger when we hit an uncaught exception
        ex = sys.exc_info()[1]
        isipdb = isinstance(ex, NameError) and "'ipdb'" in str(ex)
        if (
            (ui.configbool("devel", "debugger") or isipdb)
            and ui.interactive()
            and not ui.pageractive
            and not ui.plain()
            and ui.formatted
        ):
            if isipdb:
                ui.write_err(_("Starting ipdb for 'ipdb'\n"))
            else:
                ui.write_err(
                    _(
                        "Starting ipdb for this exception\nIf you don't want the behavior, set devel.debugger to False\n"
                    )
                )

            debugmod = getdebugmod()
            if not ui.tracebackflag:
                ui.write_err(util.smartformatexc())

            # disable progress bar to avoid polluting the pdb debugger when typing
            if ui.configbool("devel", "debugger.noprogress", True):
                util.get_main_io().disable_progress()

            debugmod.post_mortem(sys.exc_info()[2])
            os._exit(255)
        if not handlecommandexception(ui):
            raise

    return -1


class cmdtemplatestate:
    """Template-related state for a command.

    Used together with cmdtemplate=True.

    In MVC's sense, this is the "M". The template language takes the "M" and
    renders the "V".
    """

    def __init__(self, ui, opts):
        self._ui = ui
        self._templ = opts.get("template")
        if self._templ:
            # Suppress traditional outputs.
            ui.pushbuffer()
        self._props = {}

    def setprop(self, name, value):
        self._props[name] = value

    def end(self):
        if self._templ:
            ui = self._ui
            ui.popbuffer()
            text = cmdutil.rendertemplate(self._ui, self._templ, self._props)
            self._ui.write(text)


def _parse(ui, args):
    options = {}
    cmdoptions = {}

    # -cm foo: --config m foo
    fullargs = list(args)

    args, options, replace = cliparser.parse(args, True)
    if args:
        try:
            replacement, aliases = cliparser.expandargs(ui._rcfg, args)
        except cliparser.AmbiguousCommand as e:
            e.args[2].sort()
            possibilities = e.args[2]
            raise error.AmbiguousCommand(e.args[1], possibilities)
        except cliparser.CircularReference as e:
            alias = e.args[1]
            raise error.Abort(_("circular alias: %s") % alias)
        except cliparser.MalformedAlias as e:
            msg = e.args[0]
            raise error.Abort(msg)

    else:
        replacement = []

    if len(replacement) > 0:
        # FIXME: Find a way to avoid calling expandargs twice.
        fullargs = (
            fullargs[:replace] + cliparser.expandargs(ui._rcfg, fullargs[replace:])[0]
        )

        # Only need to figure out the command name. Parse result is dropped.
        cmd, _args, a, entry, level = cmdutil.findsubcmd(replacement, commands.table)
        c = list(entry[1])
    else:
        aliases = []
        cmd = None
        level = 0
        c = []

    # combine global options into local
    c += commands.globalopts

    try:
        args, cmdoptions, specifiedopts = cliparser.parsecommand(fullargs, c)

        ui.log(
            "command_info",
            positional_args=args,
            option_names=list(specifiedopts),
            option_values=[cmdoptions.get(o) for o in specifiedopts],
        )

        args = args[level:]
    except (
        cliparser.OptionNotRecognized,
        cliparser.OptionRequiresArgument,
        cliparser.OptionAmbiguous,
    ) as e:
        raise error.CommandError(cmd, e.args[0])
    except cliparser.OptionArgumentInvalid as e:
        raise error.Abort(e.args[0])

    # separate global options back out
    for o in commands.globalopts:
        n = o[1]
        options[n] = cmdoptions[n]
        del cmdoptions[n]

    if (
        options.get("quiet")
        and specifiedopts.get("quiet", 0) == 1
        and ui.agent()
        and not ui.plain()
        and ui.configbool("agent", "ignore-quiet", True)
    ):
        # A lone --quiet from an agent is ignored: output can contain
        # important details. --quiet --quiet forces quiet.
        options["quiet"] = False
        options["agent_quiet_ignored"] = True
        ui.write_err(
            _(
                "note: --quiet ignored for agents since output can contain "
                "important details (pass --quiet --quiet to force quiet)\n"
            )
        )

    return (cmd, cmd and entry[0] or None, args, options, cmdoptions, aliases)


def _earlyparseopts(args):
    try:
        return cliparser.earlyparse(args)
    except UnicodeDecodeError:
        raise error.Abort(_("cannot decode command line arguments"))


def _joinfullargs(fullargs):
    fullargs = [util.shellquote(arg) for arg in fullargs]
    return " ".join(fullargs)


def runcommand(
    lui,
    repo,
    cmd,
    fullargs,
    ui,
    options,
    d,
    cmdpats,
    cmdoptions,
    namesforhooks,
    skipprehooks,
):
    fullargs = _joinfullargs(fullargs)

    for name in namesforhooks:
        # run pre-hook, and abort if it fails
        hook.hook(
            # Prefer using repo's ui, which may have config mutations from extensions.
            getattr(repo, "ui", lui),
            repo,
            "pre-%s" % name,
            True,
            skipshell=skipprehooks,
            args=fullargs,
            pats=cmdpats,
            opts=cmdoptions,
        )

    try:
        hintutil.loadhintconfig(lui)
        bindings.dag.configure(lui._rcfg)
        ui.log("jobid", jobid=encoding.environ.get("HG_JOB_ID", "unknown"))
        ret = _runcommand(ui, options, cmd, d)

        # Special case clone return value so we have access to the new repo.
        if cmd == "clone":
            repo = ret
            ret = 0 if repo else 1

        # run post-hook, passing command result
        for name in namesforhooks:
            hook.hook(
                # Prefer using repo's ui, which may have config mutations from extensions.
                getattr(repo, "ui", lui),
                repo,
                "post-%s" % name,
                False,
                args=fullargs,
                result=ret,
                pats=cmdpats,
                opts=cmdoptions,
            )
    except Exception as e:
        for name in namesforhooks:
            # run failure hook and re-raise
            hook.hook(
                lui,
                repo,
                "fail-%s" % name,
                False,
                args=fullargs,
                pats=cmdpats,
                opts=cmdoptions,
            )
        _log_exception(lui, e)
        raise
    if getattr(repo, "_txnreleased", False):
        hook.hook(lui, repo, "postwritecommand", False)
    util.printrecordedtracebacks()
    return ret


def _log_exception(lui, e):
    try:
        lui.log_exception(
            exception_type=type(e).__name__,
            exception_msg=str(e),
            source="log_exception",
        )
    except Exception as e:
        try:
            wrapped = error.ProgrammingError("failed to log exception: {!r}".format(e))
            lui.log_exception(
                exception_type=type(wrapped).__name__,
                exception_msg=str(wrapped),
                source="log_exception_wrapped",
            )
        except Exception:
            # If there's an unrecoverable error in the exception handling flow
            # we'd rather ignore it and not shadow the original exception
            # (which wouldn't be re-raised if we throw an exception).
            pass


def _getlocal(ui, rpath):
    """Return (path, local ui object) for the given target path.

    Takes paths in [cwd]/.hg/hgrc into account."
    """
    if rpath:
        path = ui.expandpath(rpath)
        lui = ui.copy()
    else:
        try:
            wd = os.getcwd()
        except OSError as e:
            if e.errno == errno.ENOTCONN:
                ui.warn(_("current working directory is not connected\n"))
                ui.warn(
                    _(
                        "(for virtual checkouts, run '@prog@ fs doctor' to diagnose issues with edenfs)\n"
                    )
                )
                return "", ui
            raise error.Abort(
                _("error getting current working directory: %s") % e.strerror
            )
        path = cmdutil.findrepo(wd) or ""

        if not path:
            lui = ui
        else:
            lui = ui.copy()

    # Don't load repo configs if the path is a bundle file.
    if path and os.path.isdir(path):
        lui.loadrepoconfig(path)

    return path, lui


def _dispatch(req):
    args = req.args
    ui = req.ui

    # check for cwd
    cwd = req.earlyoptions["cwd"]
    if cwd:
        os.chdir(cwd)

    rpath = req.earlyoptions["repository"]
    path, lui = _getlocal(ui, rpath)

    uis = {ui, lui}

    if req.repo:
        uis.add(req.repo.ui)

    with profiling.profile(lui) as profiler:
        # progress behavior might be changed by extensions
        progress.init()
        # Configure extensions in phases: uisetup, extsetup, cmdtable, and
        # reposetup
        extensions.initialload(lui)
        # Propagate any changes to lui.__class__ by extensions
        ui.__class__ = lui.__class__

        # setup color handling before pager, because setting up pager
        # might cause incorrect console information
        coloropt = req.earlyoptions.get("color", False)
        for ui_ in uis:
            if coloropt:
                ui_.setconfig("ui", "color", coloropt, "--color")
            color.setup(ui_)

        # (uisetup and extsetup are handled in extensions.loadall)

        # (reposetup is handled in hg.repository)

        # check for fallback encoding
        fallback = lui.config("ui", "fallbackencoding")
        if fallback:
            encoding.fallbackencoding = fallback

        fullargs = args

        cmd, func, args, options, cmdoptions, foundaliases = _parse(lui, args)

        tracing.debug(
            target="command_info", command=getattr(func, "legacyname", None) or cmd
        )

        lui.cmdname = ui.cmdname = cmd
        lui.cmdtype = ui.cmdtype = getattr(func, "cmdtype", None)

        # Do not profile the 'debugshell' command.
        if cmd == "debugshell":
            profiler.stop()

        if cmd == "help" and len(foundaliases) > 0:
            cmd = foundaliases[0]
            options["help"] = True

        if options["encoding"]:
            encoding.encoding = options["encoding"]
        if options["encodingmode"]:
            encoding.encodingmode = options["encodingmode"]
        if (
            options["outputencoding"]
            and options["outputencoding"] != "same as encoding"
        ):
            encoding.outputencoding = options["outputencoding"]
        i18n.init()

        if options["config"] != req.earlyoptions["config"]:
            raise error.Abort(
                _(
                    "option --config may not be abbreviated, used in aliases, or used as a value for another option"
                )
            )
        if options["configfile"] != req.earlyoptions["configfile"]:
            raise error.Abort(
                _("option --configfile may not be abbreviated or used in aliases")
            )
        if options["cwd"] != req.earlyoptions["cwd"]:
            raise error.Abort(
                _("option --cwd may not be abbreviated or used in aliases")
            )
        if options["repository"] != req.earlyoptions["repository"]:
            raise error.Abort(
                _(
                    "option -R must appear alone, and --repository may not be "
                    "abbreviated or used in aliases"
                )
            )
        if options["debugger"] != req.earlyoptions["debugger"]:
            raise error.Abort(
                _("option --debugger may not be abbreviated or used in aliases")
            )
        # don't validate --profile/--traceback, which can be enabled from now

        if options["time"]:

            def get_times():
                t = os.times()
                if t[4] == 0.0:
                    x = time.perf_counter()
                    t = (t[0], t[1], t[2], t[3], x)
                return t

            s = get_times()

            def print_time():
                t = get_times()
                ui.warn(
                    _("time: real %.3f secs (user %.3f+%.3f sys %.3f+%.3f)\n")
                    % (t[4] - s[4], t[0] - s[0], t[2] - s[2], t[1] - s[1], t[3] - s[3])
                )

            ui.atexit(print_time)

        for ui_ in uis:
            ui_.deriveconfigfromclioptions(options)

        if util.parsebool(options["pager"]):
            # ui.pager() expects 'internal-always-' prefix in this case
            ui.pager("internal-always-" + cmd)
        elif options["pager"] != "auto":
            for ui_ in uis:
                ui_.disablepager()

        if options["version"]:
            return commands.version_(ui)

        if options["help"]:
            if len(foundaliases) > 0:
                aliascmd = foundaliases[0]
                return commands.help_(ui, aliascmd, command=cmd is not None)

            return commands.help_(ui, cmd, command=cmd is not None)
        elif not cmd:
            return commands.help_(ui)

        msg = _formatargs(fullargs)
        with perftrace.trace("Main Python Command"):
            repo = None
            # Right now Rust `commands` (undesirably) sets `func` to the
            # command description, not a callable function.
            if not callable(func):
                raise error.ProgrammingError(
                    f"'{cmd}' only has a Rust implementation but it wants to fallback to Python."
                )
            if func.cmdtemplate:
                templ = cmdtemplatestate(ui, cmdoptions)
                args.insert(0, templ)
                ui.atexit(templ.end)
            cmdpats = args[:]
            if not func.norepo:
                # use the repo from the request only if we don't have -R
                if not rpath and not cwd:
                    repo = req.repo

                if repo:
                    # set the descriptors of the repo ui to those of ui
                    repo.ui.fin = ui.fin
                    repo.ui.fout = ui.fout
                    repo.ui.ferr = ui.ferr
                else:
                    try:
                        repo = hg.repository(
                            ui, path=path, presetupfuncs=req.prereposetups
                        )
                        if not repo.local():
                            raise error.Abort(_("repository '%s' is not local") % path)
                        _initblackbox(req, repo, func.cmdtype)
                        scmutil.setup(repo.ui)
                        repo.ui.setconfig("bundle", "mainreporoot", repo.root, "repo")
                    except error.RequirementError:
                        raise
                    except error.RepoError:
                        if rpath:  # invalid -R path
                            raise
                        if not func.optionalrepo:
                            if func.inferrepo and args and not path:
                                # try to infer -R from command args
                                repos = list(map(cmdutil.findrepo, args))
                                guess = repos[0]
                                if guess and repos.count(guess) == len(repos):
                                    req.args = ["--repository", guess] + fullargs
                                    req.earlyoptions["repository"] = guess
                                    return _dispatch(req)
                            if not path:
                                raise error.RepoError(
                                    _(
                                        "'%s' is not inside a repository, but this command requires a repository"
                                    )
                                    % os.getcwd(),
                                    hint=_(
                                        "use 'cd' to go to a directory inside a repository and try again"
                                    ),
                                    error_category=error.USAGE_ERROR,
                                )
                            raise
                if repo:
                    ui = repo.ui
                    if options["hidden"]:
                        repo.ui.setconfig("visibility", "all-heads", "true", "--hidden")
                    if repo != req.repo:
                        ui.atexit(repo.close)

                    # Stuff this in to the request so exception handling code has access to repo/repo.ui.
                    req.cmdrepo = repo
                args.insert(0, repo)
            elif rpath:
                ui.warn(_("warning: --repository ignored\n"))

            ui.log("command", "%s\n", msg)
            if repo:
                repo.dirstate.loginfo(ui, "pre")
            strcmdopt = cmdoptions
            d = lambda: util.checksignature(func)(ui, *args, **strcmdopt)
            ret = runcommand(
                lui,
                repo,
                cmd,
                fullargs,
                ui,
                options,
                d,
                cmdpats,
                cmdoptions,
                func.namesforhooks,
                req.skipprehooks,
            )
            hintutil.show(lui)
            if repo:
                repo.dirstate.loginfo(ui, "post")
            return ret


def _initblackbox(req, repo, cmdtype):
    """Initialize the native blackbox logging at the shared repo path.

    This might choose to disable logging if 'blackbox.enable' is false, or if
    the blackbox extension is disabled via '--config=extensions.blackbox=!' or
    '--config=blackbox.track=' and the command is read-only. (In other words,
    read-write commands will always be logged)
    """
    if not repo.ui.configbool("blackbox", "enable", True):
        return

    # See "class command" in registrar.py for valid command types.
    # Enforce blackbox logging for non-readonly commands, so if an automation
    # runs commands like `hg commit --config extensions.blackbox=!`, we still
    # log it.
    if cmdtype == "readonly":
        config = req.earlyoptions["config"]
        if "extensions.blackbox=!" in config or "blackbox.track=" in config:
            # Explicitly disabled via command line. Do not initialize blackbox.
            return

    # Create the log in sharedvfs.
    path = repo.sharedvfs.join("blackbox", "v1")
    size = repo.ui.configbytes("blackbox", "maxsize")
    count = repo.ui.configint("blackbox", "maxfiles")
    try:
        blackbox.init(path, count, size)
    except IOError:
        # Likely permission errors. Not fatal.
        pass


def _runcommand(ui, options, cmd, cmdfunc):
    """Run a command function, possibly with profiling enabled."""
    try:
        return cmdfunc()
    except error.SignatureError:
        raise error.CommandError(cmd, _("invalid arguments"))


def _exceptionwarning(ui):
    """Produce a warning message for the current active exception"""

    return _("** @LongProduct@ (version %s) has crashed:\n") % util.version()


# NB: handlecommandexception() is replaced by the errorredirect extension.
def handlecommandexception(ui):
    """Produce a warning message for broken commands

    Called when handling an exception; the exception is reraised if
    this function returns False, ignored otherwise.
    """
    if ui.configbool("devel", "silence-crash"):
        return True  # do not re-raise
    warning = _exceptionwarning(ui)
    crash = util.smartformatexc()
    ui.log("command_exception", "%s\n%s\n", warning, crash)
    ui.warn(warning)

    alerts.print_matching_alerts_for_exception(ui, crash)

    return False  # re-raise the exception


def getdebugmod(default=pdb):
    """Try to use ipdb, if it's not available, then fallback to the default."""
    try:
        with demandimport.deactivated():
            import ipdb

            return ipdb
    except ImportError:
        return default
