#!/usr/bin/env python3
#
# run-tests.py - Run a set of tests on Mercurial
#
# Copyright 2006 Olivia Mackall <olivia@selenic.com>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.

# Modifying this script is tricky because it has many modes:
#   - serial vs parallel (default) (-jN, N > 1)
#   - no coverage (default) vs coverage (-c, -C, -s)
#   - temp install vs specific hg script (--with-hg, --local (default))
#   - tests are a mix of shell scripts and Python scripts
#
# If you change this script, it is recommended that you ensure you
# haven't broken it by running it in various modes with a representative
# sample of test scripts.  For example:
#
#  1) serial, no coverage, temp install:
#      ./run-tests.py -j1 --build test-s*
#  2) serial, no coverage, local hg:
#      ./run-tests.py -j1 --local test-s*
#  3) serial, coverage, temp install:
#      ./run-tests.py -j1 -b -c test-s*
#  4) serial, coverage, local hg:
#      ./run-tests.py -j1 -c --local test-s*  # unsupported
#  5) parallel, no coverage, temp install:
#      ./run-tests.py -j2 -b test-s*
#  6) parallel, no coverage, local hg:
#      ./run-tests.py -j2 --local test-s*
#  7) parallel, coverage, temp install:
#      ./run-tests.py -j2 -c -b test-s*       # currently broken
#  8) parallel, coverage, local install:
#      ./run-tests.py -j2 -c --local test-s*  # unsupported (and broken)
#  9) parallel, custom tmp dir:
#      ./run-tests.py -j2 --tmpdir /tmp/myhgtests
#
# (You could use any subset of the tests: test-s* happens to match
# enough that it's worth doing parallel runs, few enough that it
# completes fairly quickly, includes both shell and Python scripts, and
# includes some scripts that run daemon processes.)

import argparse
import collections
import difflib
import errno
import hashlib
import json
import multiprocessing
import os
import random
import re
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import sysconfig
import tempfile
import threading
import time
import unittest
import xml.dom.minidom as minidom
from pathlib import Path

# If we're running in an embedded Python build, it won't add the test directory
# to the path automatically, so let's add it manually.
sys.path.insert(0, os.path.dirname(os.path.realpath(__file__)))
try:
    # pyre-fixme[21]: Could not find module `features`.
    import features
except Exception:
    features = None

import queue

shellquote = shlex.quote

RLock = threading.RLock

try:
    # pyre-fixme[21]: Could not find module `libfb.py.pathutils`.
    import libfb.py.pathutils as pathutils

    def buckpath(rulename, ruletype):
        path = pathutils.get_build_rule_output_path(rulename, ruletype)
        if not os.path.exists(path):
            return None
        return path

    # pyre-fixme[16]: Module `py` has no attribute `pathutils`.
    buckruletype = pathutils.BuildRuleTypes
except ImportError:
    # pyre-fixme[9]: buckpath has type `(rulename: unknown, ruletype: unknown) ->
    #  Any`; used as `None`.
    buckpath = buckruletype = None

# pyre-fixme[21]: Could not find module `watchman`.
from watchman import Watchman, WatchmanTimeout

if os.environ.get("HGTEST_USE_EDEN", "0") == "1":
    # pyre-fixme[21]: Could not find module `edenfs`.
    from edenfs import EdenFsManager

    use_edenfs = True
else:
    use_edenfs = False

if os.environ.get("RTUNICODEPEDANTRY", False):
    try:
        # pyre-fixme[10]: Name `reload` is used but not defined.
        reload(sys)
        # pyre-fixme[16]: Module `sys` has no attribute `setdefaultencoding`.
        sys.setdefaultencoding("undefined")
    except NameError:
        pass

origenviron = os.environ.copy()

pygmentspresent = False
# ANSI color is unsupported prior to Windows 10
if os.name != "nt":
    try:  # is pygments installed
        import pygments
        import pygments.formatters as formatters

        # pyre-fixme[21]: Could not find module `pygments.lexer`.
        import pygments.lexer as lexer
        import pygments.lexers as lexers

        # pyre-fixme[21]: Could not find module `pygments.style`.
        import pygments.style as style

        # pyre-fixme[21]: Could not find module `pygments.token`.
        import pygments.token as token

        pygmentspresent = True
        difflexer = lexers.DiffLexer()
        terminal256formatter = formatters.Terminal256Formatter()
    except ImportError:
        pass

if pygmentspresent:
    # pyre-fixme[11]: Annotation `Style` is not defined as a type.
    class TestRunnerStyle(style.Style):
        default_style = ""
        # pyre-fixme[16]: Module `pygments` has no attribute `token`.
        skipped = token.string_to_tokentype("Token.Generic.Skipped")
        # pyre-fixme[16]: Module `pygments` has no attribute `token`.
        failed = token.string_to_tokentype("Token.Generic.Failed")
        # pyre-fixme[16]: Module `pygments` has no attribute `token`.
        skippedname = token.string_to_tokentype("Token.Generic.SName")
        # pyre-fixme[16]: Module `pygments` has no attribute `token`.
        failedname = token.string_to_tokentype("Token.Generic.FName")
        styles = {
            skipped: "#e5e5e5",
            skippedname: "#00ffff",
            failed: "#7f0000",
            failedname: "#ff0000",
        }

    # pyre-fixme[11]: Annotation `RegexLexer` is not defined as a type.
    class TestRunnerLexer(lexer.RegexLexer):
        tokens = {
            "root": [
                # pyre-fixme[16]: Module `pygments` has no attribute `token`.
                (r"^Skipped", token.Generic.Skipped, "skipped"),
                # pyre-fixme[16]: Module `pygments` has no attribute `token`.
                (r"^Failed ", token.Generic.Failed, "failed"),
                # pyre-fixme[16]: Module `pygments` has no attribute `token`.
                (r"^ERROR: ", token.Generic.Failed, "failed"),
            ],
            "skipped": [
                # pyre-fixme[16]: Module `pygments` has no attribute `token`.
                (r"[\w-]+\.(t|py)", token.Generic.SName),
                # pyre-fixme[16]: Module `pygments` has no attribute `token`.
                (r":.*", token.Generic.Skipped),
            ],
            "failed": [
                # pyre-fixme[16]: Module `pygments` has no attribute `token`.
                (r"[\w-]+\.(t|py)", token.Generic.FName),
                # pyre-fixme[16]: Module `pygments` has no attribute `token`.
                (r"(:| ).*", token.Generic.Failed),
            ],
        }

    # pyrefly: ignore [unbound-name]
    runnerformatter = formatters.Terminal256Formatter(style=TestRunnerStyle)
    runnerlexer = TestRunnerLexer()


def _bytespath(p):
    if p is None:
        return p
    return p.encode("utf-8")


def _strpath(p):
    if p is None:
        return p
    return p.decode("utf-8")


# For Windows support
wifexited = getattr(os, "WIFEXITED", lambda x: False)


# Whether to use IPv6
def checksocketfamily(name, port=20058):
    """return true if we can listen on localhost using family=name

    name should be either 'AF_INET', or 'AF_INET6'.
    port being used is okay - EADDRINUSE is considered as successful.
    """
    family = getattr(socket, name, None)
    if family is None:
        return False
    try:
        with socket.socket(family, socket.SOCK_STREAM) as s:
            s.bind(("localhost", port))
        return True
    except socket.error as exc:
        if exc.errno == errno.EADDRINUSE:
            return True
        elif exc.errno in (errno.EADDRNOTAVAIL, errno.EPROTONOSUPPORT):
            return False
        else:
            raise
    else:
        return False


# useipv6 will be set by parseargs
useipv6 = None


def checkportisavailable(port):
    """return true if a port seems free to bind on localhost"""
    if useipv6:
        family = socket.AF_INET6
    else:
        family = socket.AF_INET
    try:
        with socket.socket(family, socket.SOCK_STREAM) as s:
            s.bind(("localhost", port))
        return True
    except socket.error as exc:
        # pyrefly: ignore [missing-attribute]
        if os.name == "nt" and exc.winerror == errno.WSAEACCES:
            return False
        elif exc.errno in (
            errno.EADDRINUSE,
            errno.EADDRNOTAVAIL,
            errno.EPROTONOSUPPORT,
        ):
            return False
        else:
            raise
    else:
        return False


closefds = os.name == "posix"

if os.name == "nt":
    preexec = None
else:
    preexec = lambda: os.setpgid(0, 0)


def Popen4(cmd, wd, timeout, env=None):
    shell = not isinstance(cmd, list)
    p = subprocess.Popen(
        cmd,
        shell=shell,
        bufsize=-1,
        cwd=wd,
        env=env,
        close_fds=closefds,
        preexec_fn=preexec,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )

    # pyrefly: ignore [missing-attribute]
    p.fromchild = p.stdout
    # pyrefly: ignore [missing-attribute]
    p.tochild = p.stdin
    # pyrefly: ignore [missing-attribute]
    p.childerr = p.stderr

    # pyrefly: ignore [missing-attribute]
    p.timeout = False
    if timeout:
        track(p)

        def t():
            start = time.time()
            while time.time() - start < timeout and p.returncode is None:
                time.sleep(5)
            # pyrefly: ignore [missing-attribute]
            p.timeout = True
            if p.returncode is None:
                terminate(p)

        threading.Thread(target=t, name=f"Timeout tracker {cmd=}", daemon=True).start()

    return p


if buckpath:
    # pyrefly: ignore [missing-attribute]
    PYTHON = buckpath("//eden/scm:hgpython", buckruletype.SH_BINARY)
else:
    PYTHON = os.environ.get("PYTHON_SYS_EXECUTABLE", sys.executable)
IMPL_PATH = "PYTHONPATH"
if "java" in sys.platform:
    IMPL_PATH = "JYTHONPATH"

defaults = {
    "jobs": ("HGTEST_JOBS", multiprocessing.cpu_count()),
    "timeout": ("HGTEST_TIMEOUT", 450),
    "slowtimeout": ("HGTEST_SLOWTIMEOUT", 1000),
    "port": ("HGTEST_PORT", 20059),
    "shell": ("HGTEST_SHELL", "bash"),
    "locale": ("HGTEST_LOCALE", "en_US.UTF-8"),
    "maxdifflines": ("HGTEST_MAXDIFFLINES", 200),
}


def s(strorbytes):
    """Normalize to a str"""
    if isinstance(strorbytes, str):
        return strorbytes
    elif isinstance(strorbytes, bytes):
        return strorbytes.decode("utf-8")
    else:
        raise TypeError("%r is not str or bytes" % (strorbytes,))


def canonpath(path):
    return os.path.realpath(os.path.expanduser(path))


def parselistfiles(files, listtype, warn=True):
    entries = dict()
    for filename in files:
        try:
            path = os.path.expanduser(os.path.expandvars(filename))
            f = open(path, "r", encoding="utf8")
        except IOError as err:
            if err.errno != errno.ENOENT:
                raise
            if warn:
                print("warning: no such %s file: %s" % (listtype, filename))
            continue

        for line in f.readlines():
            line = line.split("#", 1)[0].strip()
            if line:
                entries[line] = filename

        f.close()
    return entries


def parsettestcases(path):
    """read a .t test file, return a set of test case names

    If path does not exist, return an empty set.
    """
    cases = set()
    try:
        with open(path, "r", encoding="utf8") as f:
            for l in f:
                if l.startswith("#testcases "):
                    cases.update(l[11:].split())
    except IOError as ex:
        if ex.errno != errno.ENOENT:
            raise
    return cases


def compatiblewithdebugruntest(path):
    """check whether a .t test is compatible with debugruntest"""
    try:
        with open(path, "r", encoding="utf8") as f:
            contents = f.read(1024)
            return "#debugruntest-compatible" in contents or (
                os.environ.get("DEBUGRUNTEST_DEFAULT_DISABLED") != "1"
                and "#debugruntest-incompatible" not in contents
            )
    except IOError as ex:
        if ex.errno != errno.ENOENT:
            raise
    return False


def getparser():
    """Obtain the OptionParser used by the CLI."""
    parser = argparse.ArgumentParser(usage="%(prog)s [options] [tests]")

    selection = parser.add_argument_group("Test Selection")
    selection.add_argument(
        "--allow-slow-tests", action="store_true", help="allow extremely slow tests"
    )
    selection.add_argument(
        "--blacklist",
        action="append",
        help="skip tests listed in the specified blacklist file",
    )
    selection.add_argument(
        "--changed",
        help="run tests that are changed in parent rev or working directory",
    )
    selection.add_argument("-k", "--keywords", help="run tests matching keywords")
    selection.add_argument(
        "-r", "--retest", action="store_true", help="retest failed tests"
    )
    selection.add_argument(
        "--retry",
        type=int,
        help="number of attempts to retry failed tests",
    )
    selection.add_argument(
        "--test-list", action="append", help="read tests to run from the specified file"
    )
    selection.add_argument(
        "--whitelist",
        action="append",
        help="always run tests listed in the specified whitelist file",
    )
    selection.add_argument("tests", metavar="TESTS", nargs="*", help="Tests to run")

    harness = parser.add_argument_group("Test Harness Behavior")
    harness.add_argument(
        "--getdeps-build", action="store_true", help="let us know build is from getdeps"
    )
    harness.add_argument(
        "--bisect-repo",
        metavar="bisect_repo",
        help="Path of a repo to bisect. Use together with --known-good-rev",
    )
    harness.add_argument(
        "-d",
        "--debug",
        action="store_true",
        help="debug mode: write output of test scripts to console"
        " rather than capturing and diffing it (disables timeout)",
    )
    harness.add_argument(
        "-f", "--first", action="store_true", help="exit on the first test failure"
    )
    harness.add_argument(
        "-i",
        "--interactive",
        action="store_true",
        help="prompt to accept changed output",
    )
    harness.add_argument(
        "-u",
        "--update-output",
        "--fix",
        action="store_true",
        help="update test outputs",
    )
    harness.add_argument(
        "-j",
        "--jobs",
        type=int,
        help="number of jobs to run in parallel"
        " (default: $%s or %d)" % defaults["jobs"],
    )
    harness.add_argument(
        "--keep-tmpdir",
        action="store_true",
        help="keep temporary directory after running tests",
    )
    harness.add_argument(
        "--known-good-rev",
        metavar="known_good_rev",
        help=(
            "Automatically bisect any failures using this "
            "revision as a known-good revision."
        ),
    )
    harness.add_argument(
        "--list-tests", action="store_true", help="list tests instead of running them"
    )
    harness.add_argument("--loop", action="store_true", help="loop tests repeatedly")
    harness.add_argument(
        "--random", action="store_true", help="run tests in random order"
    )
    harness.add_argument(
        "-p",
        "--port",
        type=int,
        help="port on which servers should listen"
        " (default: $%s or %d)" % defaults["port"],
    )
    harness.add_argument(
        "--profile-runner", action="store_true", help="run statprof on run-tests"
    )
    harness.add_argument(
        "-R", "--restart", action="store_true", help="restart at last error"
    )
    harness.add_argument(
        "--runs-per-test",
        type=int,
        dest="runs_per_test",
        help="run each test N times (default=1)",
        default=1,
    )
    harness.add_argument(
        "--shell", help="shell to use (default: $%s or %s)" % defaults["shell"]
    )
    harness.add_argument(
        "--locale", help="locale to use (default: $%s or %s)" % defaults["locale"]
    )
    harness.add_argument(
        "--showchannels", action="store_true", help="show scheduling channels"
    )
    harness.add_argument(
        "--nofeatures",
        action="store_true",
        help="do not enable extra features from features.py",
    )
    harness.add_argument(
        "--noprogress", action="store_true", help="do not show progress"
    )
    harness.add_argument(
        "--slowtimeout",
        type=int,
        help="kill errant slow tests after SLOWTIMEOUT seconds"
        " (default: $%s or %d)" % defaults["slowtimeout"],
    )
    harness.add_argument(
        "-t",
        "--timeout",
        type=int,
        help="kill errant tests after TIMEOUT seconds"
        " (default: $%s or %d)" % defaults["timeout"],
    )
    harness.add_argument(
        "--tmpdir",
        help="run tests in the given temporary directory (implies --keep-tmpdir)",
    )
    harness.add_argument(
        "-v", "--verbose", action="store_true", help="output verbose messages"
    )
    harness.add_argument(
        "--testpilot", action="store_true", help="run tests with testpilot"
    )

    hgconf = parser.add_argument_group("Mercurial Configuration")
    hgconf.add_argument(
        "--watchman", action="store_true", help="shortcut for --with-watchman=watchman"
    )
    hgconf.add_argument("--compiler", help="compiler to build with")
    hgconf.add_argument(
        "--extra-config-opt",
        action="append",
        default=[],
        help="set the given config opt in the test hgrc",
    )
    hgconf.add_argument(
        "--extra-rcpath",
        action="append",
        default=[],
        help="load the given config file or directory in the test hgrc",
    )
    hgconf.add_argument(
        "-l",
        "--local",
        action="store_true",
        help="shortcut for --with-hg=<testdir>/../hg",
    )
    hgconf.add_argument(
        "-b",
        "--rebuild",
        dest="local",
        action="store_false",
        help="build and install to a temporary location before running tests, "
        "the reverse of --local",
    )
    hgconf.set_defaults(local=True)
    hgconf.add_argument(
        "--ipv4",
        action="store_true",
        help="use IPv4 for network related tests",
    )
    hgconf.add_argument(
        "--record",
        action="store_true",
        help="track $TESTTMP changes in git (implies --keep-tmpdir)",
    )
    hgconf.add_argument(
        "--with-hg",
        metavar="HG",
        help="test using specified hg script rather than a temporary installation",
    )
    hgconf.add_argument(
        "--with-watchman", metavar="WATCHMAN", help="test using specified watchman"
    )

    reporting = parser.add_argument_group("Results Reporting")
    reporting.add_argument(
        "-C",
        "--annotate",
        action="store_true",
        help="output files annotated with coverage",
    )
    reporting.add_argument(
        "--color",
        choices=["always", "auto", "never"],
        default=os.environ.get("HGRUNTESTSCOLOR", "auto"),
        help="colorisation: always|auto|never (default: auto)",
    )
    reporting.add_argument(
        "-c", "--cover", action="store_true", help="print a test coverage report"
    )
    reporting.add_argument(
        "--exceptions",
        action="store_true",
        help="log all exceptions and generate an exception report",
    )
    reporting.add_argument(
        "-H",
        "--htmlcov",
        action="store_true",
        help="create an HTML report of the coverage of the files",
    )
    reporting.add_argument(
        "--json",
        action="store_true",
        help="store test result data in 'report.json' file",
    )
    reporting.add_argument(
        "--outputdir", help="directory to write error logs to (default=test directory)"
    )
    reporting.add_argument(
        "--temporary-error-output",
        action="store_true",
        help="use per-test scratch space for error output instead of persistent .err files",
    )
    reporting.add_argument(
        "-n", "--nodiff", action="store_true", help="skip showing test changes"
    )
    reporting.add_argument(
        "--maxdifflines",
        type=int,
        help="maximum lines of diff output"
        " (default: $%s or %d)" % defaults["maxdifflines"],
    )
    reporting.add_argument(
        "-S", "--noskips", action="store_true", help="don't report skip tests verbosely"
    )

    reporting.add_argument(
        "--time", action="store_true", help="time how long each test takes"
    )
    reporting.add_argument("--view", help="external diff viewer")
    reporting.add_argument("--xunit", help="record xunit results at specified path")

    for option, (envvar, default) in defaults.items():
        # pyrefly: ignore [unsupported-operation]
        defaults[option] = type(default)(os.environ.get(envvar, default))
    parser.set_defaults(**defaults)

    return parser


def parseargs(args, parser):
    """Parse arguments with our OptionParser and validate results."""
    options = parser.parse_args(args)

    # Populate default paths inside buck build or test.
    if buckpath is not None:
        # pyrefly: ignore [missing-attribute]
        options.with_hg = buckpath("//eden/scm:hg", buckruletype.SH_BINARY)
        # pyrefly: ignore [missing-attribute]
        options.with_watchman = buckpath("//watchman:watchman", buckruletype.CXX_BINARY)

    if options.with_hg:
        options.with_hg = canonpath(options.with_hg)
        # HGEXECUTABLEPATH is used by util.hgcmd()
        os.environ["HGEXECUTABLEPATH"] = options.with_hg
        if not (
            os.path.isfile(options.with_hg) and os.access(options.with_hg, os.X_OK)
        ):
            parser.error("--with-hg must specify an executable hg script")
    if options.local and not options.with_hg:
        testdir = os.path.dirname(canonpath(sys.argv[0]))
        reporootdir = os.path.dirname(testdir)
        exe_names = ("sl", "hg")
        if os.name == "nt":
            exe_names = ("sl", "sl.exe", "hg", "hg.exe")
        for exe_name in exe_names:
            binpath = os.path.join(reporootdir, exe_name)
            if os.access(binpath, os.X_OK):
                options.with_hg = binpath
                break
        if not options.with_hg:
            parser.error(
                "--local specified, but %s not found in %r or not executable"
                % (" or ".join(exe_names), reporootdir)
            )

    if options.watchman and options.with_watchman:
        parser.error(
            "--watchman does not work when --with-watchman is specified "
            "(use --with-watchman instead)"
        )

    if options.color == "always" and not pygmentspresent:
        sys.stderr.write(
            "warning: --color=always ignored because pygments is not installed\n"
        )

    if options.bisect_repo and not options.known_good_rev:
        parser.error("--bisect-repo cannot be used without --known-good-rev")

    global useipv6
    if options.ipv4:
        useipv6 = False
    else:
        # only use IPv4 if IPv6 is unavailable
        useipv6 = checksocketfamily("AF_INET6")

    options.anycoverage = options.cover or options.annotate or options.htmlcov
    if options.anycoverage:
        try:
            # pyre-fixme[21]: Could not find module `coverage`.
            import coverage

            coverage.__version__
        except ImportError:
            parser.error("coverage options now require the coverage package")

    if options.anycoverage and options.local:
        # this needs some path mangling somewhere, I guess
        parser.error("sorry, coverage options do not work when --local is specified")

    if options.anycoverage and options.with_hg:
        parser.error("sorry, coverage options do not work when --with-hg is specified")

    global verbose
    if options.verbose:
        verbose = ""

    try_increase_open_file_limit()
    setup_sigtrace()

    if options.tmpdir:
        options.tmpdir = canonpath(options.tmpdir)

    if options.jobs < 1:
        parser.error("--jobs must be positive")
    if options.update_output:
        options.interactive = True
    if options.interactive and options.debug:
        parser.error("-i/--interactive and -d/--debug are incompatible")
    if options.debug:
        options.noprogress = True
        if options.timeout != defaults["timeout"]:
            sys.stderr.write("warning: --timeout option ignored with --debug\n")
        if options.slowtimeout != defaults["slowtimeout"]:
            sys.stderr.write("warning: --slowtimeout option ignored with --debug\n")
        options.timeout = 0
        options.slowtimeout = 0
        # Enter ipdb shell on error.
        options.extra_config_opt += [
            "devel.debugger=1",
            "ui.interactive=1",
            "ui.paginate=0",
        ]

    if options.blacklist:
        options.blacklist = parselistfiles(options.blacklist, "blacklist")
    if options.whitelist:
        options.whitelisted = parselistfiles(options.whitelist, "whitelist")
    else:
        options.whitelisted = {}

    if options.showchannels:
        options.nodiff = True
        options.noprogress = True
    if options.noprogress:
        global showprogress
        showprogress = False

    if (
        options.retest
        or options.restart
        or options.retry
        or options.interactive
        or options.view
        or options.outputdir
    ):
        options.temporary_error_output = False

    return options


def try_increase_open_file_limit():
    try:
        import resource

        old_soft, old_hard = resource.getrlimit(resource.RLIMIT_NOFILE)
        if old_soft < 1_048_576:
            resource.setrlimit(resource.RLIMIT_NOFILE, (old_hard, old_hard))
        new_soft, new_hard = resource.getrlimit(resource.RLIMIT_NOFILE)
        vlog(
            "Maximum number of open file descriptors:"
            f" old(soft_limit={old_soft}, hard_limit={old_hard}),"
            f" new(soft_limit={new_soft}, hard_limit={new_hard}),"
        )
    except Exception:
        # `resource` module only available on unix-like platforms (Linux, Mac)
        # Windows does not have the open file descriptors limit issue.
        pass


def rename(src, dst):
    """Like os.rename(), trade atomicity and opened files friendliness
    for existing destination support.
    """
    shutil.copy(src, dst)
    os.remove(src)


import functools

# pyre-fixme[9]: _unified_diff has type `(a: Sequence[str], b: Sequence[str],
#  fromfile: str = ..., tofile: str = ..., fromfiledate: str = ..., tofiledate: str
#  = ..., n: int = ..., lineterm: str = ...) -> Iterator[str]`; used as
#  `partial[Iterator[bytes]]`.
_unified_diff = functools.partial(difflib.diff_bytes, difflib.unified_diff)


def getdiff(expected, output, ref, err):
    servefail = False
    lines = []
    if not expected:
        # No expected output. Do not run diff but just return the (error)
        # output directly.
        return servefail, output
    for line in _unified_diff(
        expected,
        output,
        _bytespath(os.path.basename(ref)),
        _bytespath(os.path.basename(err)),
    ):
        if line.startswith(b"+++") or line.startswith(b"---"):
            line = line.replace(b"\\", b"/")
            if line.endswith(b" \n"):
                line = line[:-2] + b"\n"
        lines.append(line)
        if not servefail and line.startswith(
            b"+  abort: child process failed to start"
        ):
            servefail = True

        # mactest may have too many open files issue due to system settings,
        # let's skip them. It should be okay since we have tests on Linux.
        if (
            line.startswith(b"+")
            and b"Too many open files" in line
            and sys.platform == "darwin"
        ):
            raise unittest.SkipTest("Too many open files")

    return servefail, lines


verbose = False


def vlog(*msg):
    """Log only when in verbose mode."""
    if verbose is False:
        return

    return log(*msg)


def setup_sigtrace():
    if os.name == "nt":
        return

    import traceback

    def printstacks(sig, currentframe) -> None:
        path = os.path.join(
            tempfile.gettempdir(), f"trace-{os.getpid()}-{int(time.time())}.log"
        )
        writesigtrace(path)

    def writesigtrace(path) -> None:
        content = ""
        tid_name = {t.ident: t.name for t in threading.enumerate()}
        for tid, frame in sys._current_frames().items():
            tb = "".join(traceback.format_stack(frame))
            content += f"Thread {tid_name.get(tid) or 'unnamed'} {tid}:\n{tb}\n"

        with open(path, "w") as f:
            f.write(content)

        # Also print to stderr
        sys.stderr.write(content)
        sys.stderr.write("\nStacktrace written to %s\n" % path)
        sys.stderr.flush()

    sig = getattr(signal, "SIGUSR1")
    if sig is not None:
        signal.signal(sig, printstacks)
        vlog("sigtrace: use 'kill -USR1 %d' to dump stacktrace\n" % os.getpid())


# Bytes that break XML even in a CDATA block: control characters 0-31
# sans \t, \n and \r
CDATA_EVIL = re.compile(rb"[\000-\010\013\014\016-\037]")

# Match feature conditionalized output lines in the form, capturing the feature
# list in group 2, and the preceding line output in group 1:
#
#   output..output (feature !)\n
optline = re.compile(b"(.*) \\((.+?) !\\)\n$")


def cdatasafe(data):
    """Make a string safe to include in a CDATA block.

    Certain control characters are illegal in a CDATA block, and
    there's no way to include a ]]> in a CDATA either. This function
    replaces illegal bytes with ? and adds a space between the ]] so
    that it won't break the CDATA block.
    """
    return CDATA_EVIL.sub(b"?", data).replace(b"]]>", b"] ]>")


def log(*msg):
    """Log something to stdout.

    Arguments are strings to print.
    """
    with iolock:
        if verbose:
            print(verbose, end=" ")
        for m in msg:
            print(m, end=" ")
        print()
        sys.stdout.flush()


def highlightdiff(line, color):
    if not color:
        return line
    assert pygmentspresent
    # pyrefly: ignore [missing-attribute]
    return pygments.highlight(line, difflexer, terminal256formatter).encode("utf-8")


def highlightmsg(msg, color):
    if not color:
        return msg
    assert pygmentspresent
    # pyrefly: ignore [missing-attribute]
    return pygments.highlight(msg, runnerlexer, runnerformatter)


_pgroups = {}


def track(proc):
    """Register a process to a process group. So it can be killed later."""
    pgroup = ProcessGroup()
    pid = proc.pid
    pgroup.add(pid)
    _pgroups[pid] = pgroup


def terminate(proc):
    """Terminate subprocess"""
    try:
        pgroup = _pgroups.pop(proc.pid)
        vlog("# Terminating process %d recursively" % proc.pid)
        pgroup.terminate()
    except KeyError:
        vlog("# Terminating process %d" % proc.pid)
        try:
            proc.terminate()
        except OSError:
            pass


def killdaemons(pidfile):
    # pyre-fixme[21]: Could not find module `killdaemons`.
    import killdaemons as killmod

    return killmod.killdaemons(pidfile, tryhard=False, remove=True, logfn=vlog)


if os.name == "nt":

    class ProcessGroup:
        """Process group backed by Windows JobObject.

        It provides a clean way to kill processes recursively.
        """

        def __init__(self):
            self._hjob = _kernel32.CreateJobObjectA(None, None)

        def add(self, pid):
            hprocess = _kernel32.OpenProcess(
                PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid
            )
            if not hprocess or hprocess == _INVALID_HANDLE_VALUE:
                raise ctypes.WinError(_kernel32.GetLastError())
            try:
                _kernel32.AssignProcessToJobObject(self._hjob, hprocess)
            finally:
                _kernel32.CloseHandle(hprocess)

        def terminate(self):
            if self._hjob:
                _kernel32.TerminateJobObject(self._hjob, 0)
                _kernel32.CloseHandle(self._hjob)
                self._hjob = 0

else:

    class ProcessGroup:
        """Fallback implementation on *nix. Kill process groups.

        This is less reliable than Windows' JobObject, because child processes
        can change their process groups. But it's better than nothing.

        On Linux, the "most correct" solution would be cgroup. But that
        requires root permission.
        """

        def __init__(self):
            self._pids = []

        def add(self, pid):
            self._pids.append(pid)

        def terminate(self):
            for pid in self._pids:
                try:
                    os.killpg(pid, signal.SIGKILL)
                except OSError:
                    try:
                        os.kill(pid, signal.SIGKILL)
                    except OSError:
                        pass
            self._pids = []


class Test(unittest.TestCase):
    """Encapsulates a single, runnable test.

    While this class conforms to the unittest.TestCase API, it differs in that
    instances need to be instantiated manually. (Typically, unittest.TestCase
    classes are instantiated automatically by scanning modules.)
    """

    # Status code reserved for skipped tests (used by hghave).
    SKIPPED_STATUS = 80

    # Exception happened during the test (used by DebugRunTestTest).
    EXCEPTION_STATUS = 81

    def __init__(
        self,
        path,
        outputdir,
        tmpdir,
        keeptmpdir=False,
        debug=False,
        first=False,
        timeout=None,
        startport=None,
        extraconfigopts=None,
        extrarcpaths=None,
        shell=None,
        hgcommand=None,
        slowtimeout=None,
        useipv6=False,
        watchman=None,
        options=None,
    ):
        """Create a test from parameters.

        path is the full path to the file defining the test.

        tmpdir is the main temporary directory to use for this test.

        keeptmpdir determines whether to keep the test's temporary directory
        after execution. It defaults to removal (False).

        debug mode will make the test execute verbosely, with unfiltered
        output.

        timeout controls the maximum run time of the test. It is ignored when
        debug is True. See slowtimeout for tests with #require slow.

        slowtimeout overrides timeout if the test has #require slow.

        startport controls the starting port number to use for this test. Each
        test will reserve 3 port numbers for execution. It is the caller's
        responsibility to allocate a non-overlapping port range to Test
        instances.

        extraconfigopts is an iterable of extra hgrc config options. Values
        must have the form "key=value" (something understood by hgrc). Values
        of the form "foo.key=value" will result in "[foo] key=value".

        extrarcpaths is an iterable for extra hgrc paths (files or
        directories).

        shell is the shell to execute tests in.
        """
        if timeout is None:
            timeout = defaults["timeout"]
        if startport is None:
            startport = defaults["port"]
        if slowtimeout is None:
            slowtimeout = defaults["slowtimeout"]
        self.path = path
        self.name = os.path.basename(path)
        self.basename = self.name
        self._testdir = os.path.dirname(path)
        self._outputdir = outputdir
        self._tmpname = os.path.basename(path)
        self.errpath = os.path.join(self._outputdir, "%s.err" % self.name)

        self._threadtmp = tmpdir
        self._keeptmpdir = keeptmpdir
        self._debug = debug
        self._first = first
        self._timeout = timeout
        self._slowtimeout = slowtimeout
        self._startport = startport
        self._extraconfigopts = extraconfigopts or []
        self._extrarcpaths = extrarcpaths or []
        self._shell = shell
        self._hgcommand = hgcommand or "hg"
        self._useipv6 = useipv6
        self._watchman = watchman
        self._options = options
        self._temporary_error_output = getattr(options, "temporary_error_output", False)

        self._aborted = False
        self._daemonpids = []
        self._finished = None
        self._ret = None
        self._out = None
        self._skipped = None
        self._testtmp = None
        self._edenfsmanager = None

        self._refout = self.readrefout()

    def readrefout(self):
        """read reference output"""
        # If we're not in --debug mode and reference output file exists,
        # check test output against it.
        if self._debug:
            return None  # to match "out is None"
        # pyrefly: ignore [missing-attribute]
        elif os.path.exists(self.refpath):
            # pyrefly: ignore [missing-attribute]
            with open(self.refpath, "rb") as f:
                return f.read().splitlines(True)
        else:
            return []

    # needed to get base class __repr__ running
    @property
    # pyrefly: ignore [bad-override]
    def _testMethodName(self):
        return self.name

    def __str__(self):
        return self.name

    def shortDescription(self):
        return self.name

    def setUp(self):
        """Tasks to perform before run()."""
        self._finished = False
        self._ret = None
        self._out = None
        self._skipped = None

        try:
            os.mkdir(self._threadtmp)
        except OSError as e:
            if e.errno != errno.EEXIST:
                raise

        name = self._tmpname
        self._testtmp = os.path.join(self._threadtmp, name)
        if self._keeptmpdir:
            log("testtmp dir: %s" % self._testtmp)
        os.mkdir(self._testtmp)
        if self._temporary_error_output:
            self.errpath = os.path.join(self._threadtmp, os.path.basename(self.errpath))

        # Remove any previous output files.
        if os.path.exists(self.errpath):
            try:
                os.remove(self.errpath)
            except OSError as e:
                # We might have raced another test to clean up a .err
                # file, so ignore ENOENT when removing a previous .err
                # file.
                if e.errno != errno.ENOENT:
                    raise

        if self._watchman:
            shortname = hashlib.sha1(_bytespath("%s" % name)).hexdigest()[:6]
            self._watchmandir = os.path.join(self._threadtmp, "%s.watchman" % shortname)
            os.mkdir(self._watchmandir)
            self._watchmanproc = Watchman(self._watchman, Path(self._watchmandir))
            try:
                self._watchmanproc.start()
            except WatchmanTimeout:
                self.tearDown()
                raise RuntimeError("timed out waiting for watchman")

        if use_edenfs:
            shortname = hashlib.sha1(_bytespath("%s" % name)).hexdigest()[:6]
            self._edenfsdir = Path(self._threadtmp) / f"{shortname}.edenfs"
            self._edenfsmanager = EdenFsManager(self._edenfsdir)

    # pyrefly: ignore [bad-override]
    def run(self, result):
        """Run this test and report results against a TestResult instance."""
        # This function is extremely similar to unittest.TestCase.run(). Once
        # we require Python 2.7 (or at least its version of unittest), this
        # function can largely go away.
        self._result = result
        result.startTest(self)
        try:
            try:
                self.setUp()
            except (KeyboardInterrupt, SystemExit):
                self._aborted = True
                raise
            except Exception:
                result.addError(self, sys.exc_info())
                return

            success = False
            try:
                self.runTest()
            except KeyboardInterrupt:
                self._aborted = True
                raise
            except unittest.SkipTest as e:
                result.addSkip(self, str(e))
                # The base class will have already counted this as a
                # test we "ran", but we want to exclude skipped tests
                # from those we count towards those run.
                result.testsRun -= 1
            except self.failureException as e:
                # This differs from unittest in that we don't capture
                # the stack trace. This is for historical reasons and
                # this decision could be revisited in the future,
                # especially for PythonTest instances.
                if result.addFailure(self, str(e)):
                    success = True
            except Exception:
                result.addError(self, sys.exc_info())
            else:
                success = True

            try:
                self.tearDown()
            except (KeyboardInterrupt, SystemExit):
                self._aborted = True
                raise
            except Exception:
                result.addError(self, sys.exc_info())
                success = False

            if success:
                result.addSuccess(self)
        finally:
            result.stopTest(self, interrupted=self._aborted)

    def runTest(self):
        """Run this test instance.

        This will return a tuple describing the result of the test.
        """
        env = self._getenv()
        self._genrestoreenv(env)
        self._daemonpids.append(env["DAEMON_PIDS"])
        hgrcpath = env["HGRCPATH"].rsplit(os.pathsep, 1)[-1]
        self._createhgrc(hgrcpath)
        # pyrefly: ignore [missing-attribute]
        if features and not self._options.nofeatures:
            features.setup(self.name.split()[0], hgrcpath)

        vlog("# Test", self.name)
        ret, out = self._run(env)
        self._finished = True
        self._ret = ret
        self._out = out

        def describe(ret):
            if ret < 0:
                return "killed by signal: %d" % -ret
            return "returned error code %d" % ret

        self._skipped = False

        if ret == self.SKIPPED_STATUS:
            if out is None:  # Debug mode, nothing to parse.
                missing = ["unknown"]
                failed = None
            else:
                missing, failed = TTest.parsehghaveoutput(out)

            if not missing:
                missing = ["skipped"]

            if failed:
                self.fail("hg have failed checking for %s" % failed[-1])
            else:
                self._skipped = True
                raise unittest.SkipTest(missing[-1])
        elif ret == self.EXCEPTION_STATUS:
            # Print exception (with traceback) as output mismatch.
            self._result.addOutputMismatch(self, ret, out, "")
            self.fail("exception")
        elif ret == "timeout":
            self.fail("timed out")
        elif ret is False:
            self.fail("no result code from test")
        elif out != self._refout:
            # Diff generation may rely on written .err file.
            if (
                (ret != 0 or out != self._refout)
                and not self._temporary_error_output
                and not self._skipped
                and not self._debug
            ):
                with open(self.errpath, "wb") as f:
                    for line in out:
                        f.write(line)

            # The result object handles diff calculation for us.
            if self._result.addOutputMismatch(self, ret, out, self._refout):
                # change was accepted, skip failing
                return
            if self._first:
                global firsterror
                firsterror = True

            if ret:
                msg = "output changed and " + describe(ret)
            else:
                msg = "output changed"

            self.fail(msg)
        elif ret:
            self.fail(describe(ret))

    def tearDown(self):
        """Tasks to perform after run()."""
        for entry in self._daemonpids:
            killdaemons(entry)
        self._daemonpids = []

        # EdenFS must stop before the tmp dirs are removed: on Linux its control
        # socket lives under threadtmp, and without it the stop falls back to
        # SIGKILL, leaving the test's FUSE mounts behind.
        edenfsmanager = self._edenfsmanager
        eden = edenfsmanager.eden if edenfsmanager is not None else None
        if eden is not None:
            try:
                eden.kill()
                if self._keeptmpdir:
                    log(f"Keeping edenfs dir: {edenfsmanager.test_dir}\n")
                else:
                    eden.cleanup()
                    shutil.rmtree(edenfsmanager.test_dir, ignore_errors=True)
            except Exception as e:
                log(f"\nFailed to stop edenfs: {e}\n")

        if self._keeptmpdir:
            log(
                "\nKeeping testtmp dir: %s\nKeeping threadtmp dir: %s"
                % (self._testtmp, self._threadtmp)
            )
            log(
                "\nSet up config environment by:\n"
                "  export HGRCPATH=%s\n  export SL_CONFIG_PATH=%s"
                % (s(self._gethgrcpath()), s(self._gethgrcpath()))
            )
        else:
            # pyrefly: ignore [no-matching-overload]
            shutil.rmtree(self._testtmp, True)
            shutil.rmtree(self._threadtmp, True)

        if self._watchman:
            try:
                self._watchmanproc.stop()
                if self._keeptmpdir:
                    log(
                        # pyrefly: ignore [missing-attribute]
                        "Keeping watchman dir: %s\n" % self._watchmandir.decode("utf-8")
                    )
                else:
                    shutil.rmtree(self._watchmandir, ignore_errors=True)
            except Exception:
                pass

        if (
            (self._ret != 0 or self._out != self._refout)
            and not self._temporary_error_output
            and not self._skipped
            and not self._debug
            and self._out
        ):
            with open(self.errpath, "wb") as f:
                for line in self._out:
                    f.write(line)

        vlog("# Ret was:", self._ret, "(%s)" % self.name)

    def _run(self, env):
        # This should be implemented in child classes to run tests.
        raise unittest.SkipTest("unknown test type")

    def abort(self):
        """Terminate execution of this test."""
        self._aborted = True

    def _portmap(self, i):
        offset = b"" if i == 0 else b"%d" % i
        return (rb":%d\b" % (self._startport + i), b":$HGPORT%s" % offset)

    def _getreplacements(self):
        """Obtain a mapping of text replacements to apply to test output.

        Test output needs to be normalized so it can be compared to expected
        output. This function defines how some of that normalization will
        occur.
        """
        r = [
            # This list should be parallel to defineport in _getenv
            self._portmap(0),
            self._portmap(1),
            self._portmap(2),
            # This hack allows us to have same outputs for ipv4 and v6 urls:
            # [ipv6]:port
            (
                rb"([^0-9:])\[%s\]:[0-9]+" % re.escape(_bytespath(self._localip())),
                rb"\1$LOCALIP:$LOCAL_PORT",
            ),
            # [ipv6]
            (
                rb"([^0-9:])\[%s\]" % re.escape(_bytespath(self._localip())),
                rb"\1$LOCALIP",
            ),
            # ipv4:port
            (
                rb"([^0-9])%s:[0-9]+" % re.escape(_bytespath(self._localip())),
                rb"\1$LOCALIP:$LOCAL_PORT",
            ),
            # [ipv4]
            (rb"([^0-9])%s" % re.escape(_bytespath(self._localip())), rb"\1$LOCALIP"),
            # localhost:port
            (
                rb"([^0-9])localhost:[0-9]+",
                rb"\1localhost:$LOCAL_PORT",
            ),
            (rb"\bHG_TXNID=TXN:[a-f0-9]{40}\b", rb"HG_TXNID=TXN:$ID$"),
        ]
        r.append((_bytespath(self._escapepath(self._testtmp)), b"$TESTTMP"))
        r.append((rb"eager:///", rb"eager://"))

        replacementfile = os.path.join(self._testdir, "common-pattern.py")

        if os.path.exists(replacementfile):
            data = {}
            with open(replacementfile, mode="rb") as source:
                # the intermediate 'compile' step help with debugging
                code = compile(source.read(), replacementfile, "exec")
                exec(code, data)
                r.extend(data.get("substitutions", ()))
        return r

    def _escapepath(self, p):
        if os.name == "nt":
            return r"(?:[/\\]{2,4}\?[/\\]{1,2})?" + "".join(
                c.isalpha()
                and "[%s%s]" % (c.lower(), c.upper())
                or c in "/\\"
                and r"[/\\]{1,2}"
                or c.isdigit()
                and c
                or "\\" + c
                for c in p
            )
        else:
            return re.escape(p)

    def _localip(self):
        if self._useipv6:
            return "::1"
        else:
            return "127.0.0.1"

    def _genrestoreenv(self, testenv):
        """Generate a script that can be used by tests to restore the original
        environment."""
        # Put the restoreenv script inside self._threadtmp
        scriptpath = os.path.join(self._threadtmp, "restoreenv.sh")
        testenv["HGTEST_RESTOREENV"] = scriptpath

        # Only restore environment variable names that the shell allows
        # us to export.
        name_regex = re.compile("^[a-zA-Z][a-zA-Z0-9_]*$")

        # Do not restore these variables; otherwise tests would fail.
        reqnames = {}

        with open(scriptpath, "w") as envf:
            for name, value in origenviron.items():
                if not name_regex.match(name):
                    # Skip environment variables with unusual names not
                    # allowed by most shells.
                    continue
                if name in reqnames:
                    continue
                envf.write("%s=%s\n" % (name, shellquote(value)))

            for name in testenv:
                if name in origenviron or name in reqnames:
                    continue
                envf.write("unset %s\n" % (name,))

    def _gethgrcpath(self):
        rcpath = os.path.join(self._threadtmp, ".hgrc")
        rcpaths = [p for p in self._extrarcpaths] + [rcpath]
        return os.pathsep.join(rcpaths)

    def _getenv(self):
        """Obtain environment variables to use during test execution."""

        def defineport(i):
            offset = "" if i == 0 else "%s" % i
            env["HGPORT%s" % offset] = "%s" % (self._startport + i)

        env = os.environ.copy()
        if os.name != "nt":
            # Now that we *only* load stdlib from python.zip on Windows,
            # there's no userbase
            userbase = sysconfig.get_config_var("userbase")
            if userbase:
                env["PYTHONUSERBASE"] = sysconfig.get_config_var("userbase")
        env["HGEMITWARNINGS"] = "1"
        # pyrefly: ignore [unsupported-operation]
        env["TESTTMP"] = self._testtmp
        env["TESTFILE"] = self.path
        # pyrefly: ignore [unsupported-operation]
        env["HOME"] = self._testtmp  # Unix
        # pyrefly: ignore [unsupported-operation]
        env["USERPROFILE"] = self._testtmp  # Windows
        # pyrefly: ignore [unsupported-operation]
        env["APPDATA"] = self._testtmp  # Windows
        # This number should match portneeded in _getport
        for port in range(3):
            # This list should be parallel to _portmap in _getreplacements
            defineport(port)
        env["HGRCPATH"] = self._gethgrcpath()
        env["SL_CONFIG_PATH"] = self._gethgrcpath()
        env["DAEMON_PIDS"] = os.path.join(self._threadtmp, "daemon.pids")
        env["HGEDITOR"] = "internal:none"
        env["HGMERGE"] = "internal:merge"
        env["HGUSER"] = "test"
        env["HGENCODING"] = "ascii"
        env["HGENCODINGMODE"] = "strict"
        env["HGOUTPUTENCODING"] = "ascii"
        env["HGIPV6"] = str(int(self._useipv6))

        # LOCALIP could be ::1 or 127.0.0.1. Useful for tests that require raw
        # IP addresses.
        env["LOCALIP"] = self._localip()

        # Reset some environment variables to well-known values so that
        # the tests produce repeatable output.
        # pyrefly: ignore [missing-attribute]
        env["LANG"] = env["LC_ALL"] = env["LANGUAGE"] = self._options.locale
        env["TZ"] = "GMT"
        env["COLUMNS"] = "80"

        # Claim that 256 colors is not supported.
        env["HGCOLORS"] = "16"
        # Python 3.14 colorizes tracebacks and unittest output by default.
        env["PYTHON_COLORS"] = "0"
        # Normalize TERM to avoid control sequence variations.
        # We use a non-existent terminal to avoid any terminfo dependency.
        env["TERM"] = "fake-term"

        # Do not log to scuba (fb).
        env["FB_SCM_DIAGS_NO_SCUBA"] = "1"

        keys_to_del = (
            "HG HGPROF CDPATH GREP_OPTIONS http_proxy https_proxy no_proxy "
            + "HGPLAIN HGPLAINEXCEPT SL_AUTOMATION SL_AUTOMATION_EXCEPT "
            + "EDITOR VISUAL PAGER "
            + "HTTP_PROXY HTTPS_PROXY NO_PROXY RUST_BACKTRACE RUST_LIB_BACKTRACE "
            + " EDENSCM_TRACE_LEVEL EDENSCM_TRACE_OUTPUT"
            + " EDENSCM_TRACE_PY TRACING_DATA_FAKE_CLOCK"
            + " EDENSCM_LOG LOG FAILPOINTS"
            # Used by dummyssh
            + " DUMMYSSH_STABLE_ORDER"
        ).split()

        # pyrefly: ignore [missing-attribute]
        if not self._options.getdeps_build:
            # LD_LIBRARY_PATH is usually set by buck sh_binary wrapper to import
            # Python extensions depending on buck runtime shared objects.
            # However, that breaks system executables like "curl" depending on
            # system libraries. Just unset it here. For code requiring importing
            # Python extensions, expect them to use "hg debugpython" or "python"
            # in the test, which will go through the wrapper and get a correct
            # environment again.
            keys_to_del.append("LD_LIBRARY_PATH")

        for k in keys_to_del:
            if k in env:
                del env[k]

        # unset env related to hooks
        for k in list(env.keys()):
            if k.startswith("HG_"):
                del env[k]

        if self._watchman:
            env["WATCHMAN_SOCK"] = str(self._watchmanproc.socket)
            env["HGFSMONITOR_TESTS"] = "1"

        return env

    def _createhgrc(self, path):
        """Create an hgrc file for this test."""
        # If you want to update the default hgrc, update `default_hgrc.py`
        # so it applies to all test runners ideally.
        # pyre-fixme[21]: Could not find module `default_hgrc`.
        import default_hgrc

        # Note: This code path is not run for 'debugruntest' tests.
        content = default_hgrc.get_content(
            use_watchman=self._watchman,
            use_ipv6=self._useipv6,
        )
        with open(path, "w") as hgrc:
            hgrc.write(content)

            for opt in self._extraconfigopts:
                section, key = opt.split(".", 1)
                assert "=" in key, (
                    "extra config opt %s must have an = for assignment" % opt
                )
                hgrc.write("[%s]\n%s\n" % (section, key))

    # pyrefly: ignore [bad-override]
    def fail(self, msg):
        # unittest differentiates between errored and failed.
        # Failed is denoted by AssertionError (by default at least).
        raise AssertionError(msg)

    def _runcommand(self, cmd, env, normalizenewlines=False, linecallback=None):
        """Run command in a sub-process, capturing the output (stdout and
        stderr).

        Return a tuple (exitcode, output). output is None in debug mode.
        """
        if self._debug:
            proc = subprocess.Popen(cmd, shell=True, cwd=self._testtmp, env=env)
            ret = proc.wait()
            return (ret, [])

        proc = Popen4(cmd, self._testtmp, self._timeout, env)
        track(proc)

        def cleanup():
            terminate(proc)
            ret = proc.wait()
            if ret == 0:
                ret = signal.SIGTERM << 8
            killdaemons(env["DAEMON_PIDS"])
            return ret

        proc.tochild.close()
        lines = []

        try:
            f = proc.fromchild
            while True:
                # defend against very long line outputs
                line = f.readline(5000)
                # Make the test abort faster if other tests are Ctrl+C-ed.
                # Code path: for test in runtests: test.abort()
                if self._aborted:
                    raise KeyboardInterrupt()
                if not line:
                    break
                if linecallback:
                    linecallback(line)
                lines.append(line)
                if len(lines) > 50000:
                    log(f"Test command '{cmd}' outputs too many lines")
                    cleanup()
                    break

                # defend against very large outputs
                # 10_000_000 = 50_000 * 200 (assuming each line has 200 bytes)
                if sum(len(s) for s in lines) > 10_000_000:
                    log(f"Test command '{cmd}' outputs too large")
                    cleanup()
                    break

        except KeyboardInterrupt:
            vlog("# Handling keyboard interrupt")
            cleanup()
            raise

        finally:
            proc.fromchild.close()

        output = b"".join(lines)

        ret = proc.wait()
        if wifexited(ret):
            ret = os.WEXITSTATUS(ret)

        if proc.timeout:
            ret = "timeout"

        if ret:
            killdaemons(env["DAEMON_PIDS"])

        for s, r in self._getreplacements():
            output = re.sub(s, r, output)

        if normalizenewlines:
            output = output.replace(b"\r\n", b"\n")

        # Convert clear-to-end-of-screen sequence into a separate line
        # to give some division of output.
        output = output.split(b"\x1b[J")
        for i in range(len(output) - 1):
            output[i] += b" (clear)"

        return ret, [l for part in output for l in part.splitlines(True)]


class PythonTest(Test):
    """A Python-based test."""

    @property
    def refpath(self):
        return os.path.join(self._testdir, "%s.out" % self.basename)

    def _processoutput(self, output):
        if os.path.exists(self.refpath):
            with open(self.refpath, "rb") as f:
                expected = f.readlines()
        else:
            return output

        processed = ["" for i in output]
        i = 0
        while i < len(expected) and i < len(output):
            line = expected[i].strip()

            # by default, processed output is the same as received output
            processed[i] = output[i]
            if line.endswith(b" (re)"):
                # pattern, should try to match
                pattern = line[:-5]
                if not pattern.endswith(b"$"):
                    pattern += b"$"
                if re.match(pattern, output[i].strip()):
                    processed[i] = expected[i]
            i = i + 1

        # output is longer than expected, we don't need to process
        # the tail
        while i < len(output):
            processed[i] = output[i]
            i = i + 1

        return processed

    def _run(self, env):
        debugargs = ""
        # pyrefly: ignore [missing-attribute]
        if self._options.debug:
            try:
                pass
            except ImportError:
                print(
                    "WARNING: ipdb is not available, not running %s under debug mode"
                    % (self.path,)
                )
                pass
            else:
                debugargs = " -m ipdb "
        cmd = '%s debugpython -- %s "%s"' % (self._hgcommand, debugargs, self.path)
        vlog("# Running", cmd)
        normalizenewlines = os.name == "nt"
        with open(self.path, "r", encoding="utf8") as f:
            code = f.read()
        for level in ("trace", "debug", "info", "warn", "error"):
            comment = "# tracing-level: %s" % level
            if comment in code:
                env = env.copy()
                env["EDENSCM_TRACE_LEVEL"] = level
                vlog("# EDENSCM_TRACE_LEVEL=%s" % level)
                break
        env["HG"] = self._hgcommand
        result = self._runcommand(cmd, env, normalizenewlines=normalizenewlines)
        if self._aborted:
            raise KeyboardInterrupt()

        return result[0], self._processoutput(result[1])


# pyre-fixme[9]: bchr has type `(int) -> str`; used as `(x: Any) -> bytes`.
bchr = lambda x: bytes([x])


class TTest(Test):
    """A "t test" is a test backed by a .t file."""

    SKIPPED_PREFIX = b"skipped: "
    FAILED_PREFIX = b"hghave check failed: "

    ESCAPESUB = re.compile(rb"[\x00-\x08\x0b-\x1f\\\x7f-\xff]").sub
    ESCAPEMAP = dict((bchr(i), rb"\x%02x" % i) for i in range(256))
    # pyre-fixme[6]: For 1st argument expected `SupportsKeysAndGetItem[str, bytes]`
    #  but got `Dict[bytes, bytes]`.
    ESCAPEMAP.update({b"\\": b"\\\\", b"\r": rb"\r"})

    def __init__(self, path, *args, **kwds):
        # accept an extra "case" parameter
        case = kwds.pop("case", None)
        self._case = case
        self._allcases = parsettestcases(path)
        super(TTest, self).__init__(path, *args, **kwds)
        if case:
            self.name = "%s (case %s)" % (self.name, case)
            self.errpath = "%s.%s.err" % (self.errpath[:-4], case)
            self._tmpname += "-%s" % case
        self._hghavecache = {}

    @property
    def refpath(self):
        return os.path.join(self._testdir, self.basename)

    def _run(self, env):
        with open(self.path, "rb") as f:
            lines = f.readlines()

        # .t file is both reference output and the test input, keep reference
        # output updated with the the test input. This avoids some race
        # conditions where the reference output does not match the actual test.
        if self._refout is not None:
            self._refout = lines

        salt, saltcount, script, after, expected = self._parsetest(lines)
        self.progress = (0, saltcount, 0)

        # Write out the generated script.
        fname = "%s.sh" % self._testtmp
        with open(fname, "wb") as f:
            for l in script:
                f.write(l)

        cmd = '%s "%s"' % (self._shell, fname)
        vlog("# Running", cmd)

        saltseen = [0]

        def linecallback(line):
            if salt in line:
                saltseen[0] += 1
                try:
                    linenum = int(line.split()[1].decode("utf-8")) + 1
                except Exception:
                    linenum = "?"
                # pyrefly: ignore [bad-assignment]
                self.progress = (saltseen[0], saltcount, linenum)

        exitcode, output = self._runcommand(cmd, env, linecallback=linecallback)

        if self._aborted:
            raise KeyboardInterrupt()

        # Do not merge output if skipped. Return hghave message instead.
        # Similarly, with --debug, output is None.
        if exitcode == self.SKIPPED_STATUS or output is None:
            return exitcode, output

        return self._processoutput(exitcode, output, salt, after, expected)

    def _hghave(self, reqs):
        # Cache the results of _hghave() checks.
        # In some cases the same _hghave() call can be repeated hundreds of
        # times in a row.  (For instance, if a linematch check with a hghave
        # requirement does not match, the _hghave() call will be repeated for
        # each remaining line in the test output.)
        key = tuple(reqs)
        result = self._hghavecache.get(key)
        if result is None:
            result = self._computehghave(reqs)
            self._hghavecache[key] = result
        return result

    def _computehghave(self, reqs):
        # TODO do something smarter when all other uses of hghave are gone.
        if (runtestdir := os.environ.get("RUNTESTDIR", None)) is None:
            runtestdir = os.path.abspath(os.path.dirname(__file__))
        tdir = runtestdir.replace("\\", "/")
        proc = Popen4(
            '%s debugpython -- "%s/hghave" %s'
            % (self._hgcommand, tdir, " ".join(reqs)),
            self._testtmp,
            0,
            self._getenv(),
        )
        stdout, stderr = proc.communicate()
        ret = proc.wait()
        if wifexited(ret):
            ret = os.WEXITSTATUS(ret)
        if ret == 2:
            # The feature is "missing" - hghave does not know how to check it.
            # This is most likely a mis-spelled feature name, or some
            # codemod that removes features from hghave without cleaning
            # up related test code. Treat it as a test failure.
            raise AssertionError("feature unknown to hghave: %r" % [s(r) for r in reqs])

        if ret != 0:
            return False, _strpath(stdout)

        if "slow" in reqs:
            self._timeout = self._slowtimeout
        return True, None

    def _iftest(self, args):
        # implements "#if"
        reqs = []
        for arg in args:
            if arg.startswith("no-") and arg[3:] in self._allcases:
                if arg[3:] == self._case:
                    return False
            elif arg in self._allcases:
                if arg != self._case:
                    return False
            else:
                reqs.append(arg)
        return self._hghave(reqs)[0]

    def _parsetest(self, lines):
        # We generate a shell script which outputs unique markers to line
        # up script results with our source. These markers include input
        # line number and the last return code.
        salt = b"SALT%d" % time.time()
        saltcount = [0]

        # pyrefly: ignore [missing-attribute]
        record = self._options.record

        def addsalt(line, inpython, linecontent):
            saltcount[0] += 1
            if inpython:
                script.append(b"%s %d 0\n" % (salt, line))
            else:
                script.append(b"echo %s %d $?\n" % (salt, line))
                if record:
                    # Commit to git.
                    script.append(
                        b"testrecord %r\n"
                        % ("%s (line %s)" % (s(linecontent.strip()), line),)
                    )

        script = []

        # After we run the shell script, we re-unify the script output
        # with non-active parts of the source, with synchronization by our
        # SALT line number markers. The after table contains the non-active
        # components, ordered by line number.
        after = {}

        # Expected shell script output.
        expected = {}

        pos = prepos = -1

        # True or False when in a true or false conditional section
        skipping = None

        # We keep track of whether or not we're in a Python block so we
        # can generate the surrounding doctest magic.
        inpython = False

        if self._debug:
            script.append(b"set -x\n")
        if os.getenv("MSYSTEM"):
            script.append(b'pwd() { builtin pwd -W "$@"; }\n')

        # Source $RUNTESTDIR/tinit.sh for utility functions
        script.append(b'source "$RUNTESTDIR/tinit.sh"\n')
        if record:
            script.append(b'git init -q "$TESTTMP"\n')
            script.append(
                b"""testrecord() {
                (
                cd "$TESTTMP"
                git add -A &>/dev/null
                GIT_AUTHOR_NAME=test \
                GIT_AUTHOR_EMAIL=test@localhost \
                GIT_AUTHOR_DATE=2000-01-01T00:00:00 \
                GIT_COMMITTER_NAME=test \
                GIT_COMMITTER_EMAIL=test@localhost \
                GIT_COMMITTER_DATE=2000-01-01T00:00:00 \
                git commit -a -m "$1" --allow-empty -q
                )
            }\n"""
            )

        n = 0
        for n, l in enumerate(lines):
            if not l.endswith(b"\n"):
                l += b"\n"
            if l.startswith(b"#require"):
                lsplit = _strpath(l).split()
                if len(lsplit) < 2 or lsplit[0] != "#require":
                    # pyrefly: ignore [missing-attribute]
                    after.setdefault(pos, []).append("  !!! invalid #require\n")
                if not skipping:
                    haveresult, message = self._hghave(lsplit[1:])
                    if not haveresult:
                        script = [b'echo "%s"\nexit 80\n' % _bytespath(message)]
                        break
                # pyrefly: ignore [missing-attribute]
                after.setdefault(pos, []).append(l)
            elif l.startswith(b"#if"):
                lsplit = _strpath(l).split()
                if len(lsplit) < 2 or lsplit[0] != "#if":
                    # pyrefly: ignore [missing-attribute]
                    after.setdefault(pos, []).append(b"  !!! invalid #if\n")
                if skipping is not None:
                    # pyrefly: ignore [missing-attribute]
                    after.setdefault(pos, []).append(b"  !!! nested #if\n")
                skipping = not self._iftest(lsplit[1:])
                # pyrefly: ignore [missing-attribute]
                after.setdefault(pos, []).append(l)
            elif l.startswith(b"#else"):
                if skipping is None:
                    # pyrefly: ignore [missing-attribute]
                    after.setdefault(pos, []).append(b"  !!! missing #if\n")
                skipping = not skipping
                # pyrefly: ignore [missing-attribute]
                after.setdefault(pos, []).append(l)
            elif l.startswith(b"#endif"):
                if skipping is None:
                    # pyrefly: ignore [missing-attribute]
                    after.setdefault(pos, []).append(b"  !!! missing #if\n")
                skipping = None
                # pyrefly: ignore [missing-attribute]
                after.setdefault(pos, []).append(l)
            elif skipping:
                # pyrefly: ignore [missing-attribute]
                after.setdefault(pos, []).append(l)
            elif l.startswith(b"  >>> "):  # python inlines
                # pyrefly: ignore [missing-attribute]
                after.setdefault(pos, []).append(l)
                prepos = pos
                pos = n
                if not inpython:
                    # We've just entered a Python block. Add the header.
                    inpython = True
                    addsalt(prepos, False, l)  # Make sure we report the exit code.
                    if os.name == "nt":
                        script.append(
                            b"%s %s <<EOF\n"
                            % (
                                b"hg debugpython --",
                                self._stringescape(
                                    _bytespath(
                                        os.path.join(self._testdir, "heredoctest.py")
                                    )
                                ),
                            )
                        )
                    else:
                        script.append(
                            b"%s -m heredoctest <<EOF\n" % b"hg debugpython --"
                        )
                addsalt(n, True, l)
                script.append(l[2:])
            elif l.startswith(b"  ... "):  # python inlines
                # pyrefly: ignore [missing-attribute]
                after.setdefault(prepos, []).append(l)
                script.append(l[2:])
            elif l.startswith(b"  $ "):  # commands
                if inpython:
                    script.append(b"EOF\n")
                    inpython = False
                # pyrefly: ignore [missing-attribute]
                after.setdefault(pos, []).append(l)
                prepos = pos
                pos = n
                addsalt(n, False, l)
                cmd = l[4:].split()
                if len(cmd) == 2 and cmd[0] == b"cd":
                    l = b"  $ cd %s || exit 1\n" % cmd[1]
                script.append(b"export TESTLINE=%d\n" % n)
                script.append(l[4:])
            elif l.startswith(b"  > "):  # continuations
                # pyrefly: ignore [missing-attribute]
                after.setdefault(prepos, []).append(l)
                script.append(l[4:])
            elif l.startswith(b"  "):  # results
                # Queue up a list of expected results.
                # pyrefly: ignore [missing-attribute]
                expected.setdefault(pos, []).append(l[2:])
            else:
                if inpython:
                    script.append(b"EOF\n")
                    inpython = False
                # Non-command/result. Queue up for merged output.
                # pyrefly: ignore [missing-attribute]
                after.setdefault(pos, []).append(l)

        if inpython:
            script.append(b"EOF\n")
        if skipping is not None:
            # pyrefly: ignore [missing-attribute]
            after.setdefault(pos, []).append("  !!! missing #endif\n")
        addsalt(n + 1, False, "EOF")

        return salt, saltcount[0], script, after, expected

    def _processoutput(self, exitcode, output, salt, after, expected):
        # Merge the script output back into a unified test.
        warnonly = 1  # 1: not yet; 2: yes; 3: for sure not
        if exitcode != 0:
            warnonly = 3

        pos = -1
        postout = []
        for l in output:
            lout, lcmd = l, None
            if salt in l:
                lout, lcmd = l.split(salt, 1)

            while lout:
                if not lout.endswith(b"\n"):
                    if b"\x1b" in lout or b"\r" in lout:
                        lout = (
                            lout.replace(b"\x1b", rb"\x1b").replace(b"\r", rb"\r")
                            + b" (no-eol) (esc)\n"
                        )
                    else:
                        lout += b" (no-eol)\n"

                # Find the expected output at the current position.
                els = [None]
                if expected.get(pos, None):
                    els = expected[pos]

                i = 0
                while i < len(els):
                    el = els[i]
                    trimmed_el, is_optional = self.parse_optional_directive(el)
                    success = lout == el or self.linematch(trimmed_el, lout)

                    if success:
                        els.pop(i)
                        break
                    if is_optional:
                        # pyrefly: ignore [unsupported-operation]
                        postout.append(b"  " + el)
                        els.pop(i)
                        break
                    i += 1

                if success:
                    # pyrefly: ignore [unsupported-operation]
                    postout.append(b"  " + el)
                elif is_optional:
                    continue
                else:
                    postout.append(b"  " + lout)  # Let diff deal with it.
                    warnonly = 3  # for sure not
                break
            else:
                # clean up any optional leftovers
                while expected.get(pos, None):
                    el = expected[pos].pop(0)
                    if el:
                        if not el.endswith(b" (?)\n"):
                            m = optline.match(el)
                            if m:
                                conditions = [
                                    _strpath(c) for c in m.group(2).split(b" ")
                                ]

                                if self._iftest(conditions):
                                    # Don't append as optional line
                                    continue
                            else:
                                continue
                    postout.append(b"  " + el)

            if lcmd:
                # Add on last return code.
                try:
                    ret = int(lcmd.split()[1])
                except ValueError:
                    ret = 1
                if ret != 0:
                    postout.append(b"  [%d]\n" % ret)
                if pos in after:
                    # Merge in non-active test bits.
                    postout += after.pop(pos)
                pos = int(lcmd.split()[0])

        if pos in after:
            postout += after.pop(pos)

        if warnonly == 2:
            exitcode = False  # Set exitcode to warned.

        return exitcode, postout

    @staticmethod
    def rematch(el, l):
        try:
            el = b"(?:" + el + b")"
            # use \Z to ensure that the regex matches to the end of the string
            if os.name == "nt":
                return re.match(el + rb"\r?\n\Z", l)
            return re.match(el + rb"\n\Z", l)
        except re.error:
            # el is an invalid regex
            return False

    @staticmethod
    def globmatch(el, l):
        # The only supported special characters are * and ? plus / which also
        # matches \ on windows. Escaping of these characters is supported.
        if el + b"\n" == l:
            return True
        el = el.replace(b"$LOCALIP", b"*")
        # $HGPORT might be changed in test. Do a fuzzy match.
        el = el.replace(b"$HGPORT1", b"*")
        el = el.replace(b"$HGPORT2", b"*")
        el = el.replace(b"$HGPORT", b"*")
        i, n = 0, len(el)
        res = b""
        while i < n:
            c = el[i : i + 1]
            i += 1
            if c == b"\\" and i < n and el[i : i + 1] in b"*?\\/":
                res += el[i - 1 : i + 1]
                i += 1
            elif c == b"*":
                res += b".*"
            elif c == b"?":
                res += b"."
            elif c == b"/" and os.altsep:
                res += b"[/\\\\]"
            else:
                # pyrefly: ignore [unsupported-operation]
                res += re.escape(c)
        return TTest.rematch(res, l)

    def parse_optional_directive(self, el):
        """Determine whether el is optional, and strip the optional marker if so."""
        if not el:
            # The function is sometimes passed None for some reason, so we have to early
            return el, False
        if el.endswith(b" (?)\n"):
            return el[:-5] + b"\n", True

        m = optline.match(el)
        if m:
            conditions = [_strpath(c) for c in m.group(2).split(b" ")]

            el = m.group(1) + b"\n"
            optional = not self._iftest(conditions)  # Not required by listed features
            return el, optional
        return el, False

    def linematch(self, el, l):
        if not el:
            return False
        if el.endswith(b" (esc)\n"):
            if repr(el[:-7]) == repr(l[:-1]).replace("\\", "\\\\"):
                return True
            el = el[:-7].decode("unicode_escape") + "\n"
            el = el.encode("utf-8")
        if el == l or os.name == "nt" and el[:-1] + b"\r\n" == l:
            return True
        if el.endswith(b" (re)\n"):
            return TTest.rematch(el[:-6], l) or False
        if el.endswith(b" (glob)\n"):
            # ignore '(glob)' added to l by 'replacements'
            if l.endswith(b" (glob)\n"):
                l = l[:-8] + b"\n"
            return TTest.globmatch(el[:-8], l) or False
        if os.altsep:
            _l = l.replace(b"\\", b"/")
            if el == _l or os.name == "nt" and el[:-1] + b"\r\n" == _l:
                return True
        return False

    @staticmethod
    def parsehghaveoutput(lines):
        """Parse hghave log lines.

        Return tuple of lists (missing, failed):
          * the missing/unknown features
          * the features for which existence check failed"""
        missing = []
        failed = []
        for line in lines:
            if line.startswith(TTest.SKIPPED_PREFIX):
                line = line.splitlines()[0]
                missing.append(line[len(TTest.SKIPPED_PREFIX) :].decode("utf-8"))
            elif line.startswith(TTest.FAILED_PREFIX):
                line = line.splitlines()[0]
                failed.append(line[len(TTest.FAILED_PREFIX) :].decode("utf-8"))

        return missing, failed

    @staticmethod
    def _escapef(m):
        return TTest.ESCAPEMAP[m.group(0)]

    @staticmethod
    def _stringescape(s):
        return TTest.ESCAPESUB(TTest._escapef, s)


class DebugRunTestTest(Test):
    """test compatible with debugruntest runner"""

    @property
    def refpath(self):
        return os.path.join(self._testdir, self.basename)

    def _run(self, env):
        if use_edenfs:
            self._edenfsmanager.start(env)
            env["EDENFSTMP"] = str(self._edenfsmanager.edenfs_dir)

        cmd = env.get("HGTEST_DEBUGRUNTEST_HG", self._hgcommand)

        cmdargs = [
            cmd,
            "debugpython",
            "--",
            "-m",
            "sapling.testing.single",
            self.path,
            "-o",
            self.errpath,
        ]
        # Note: we need to make sure that the Mononoke extension starts after the EdenFS one
        if use_edenfs:
            cmdargs += ["--ext", "sapling.testing.ext.edenfs"]
        if env.get("USE_MONONOKE"):
            cmdargs += ["--ext", "sapling.testing.ext.mononoke"]
        vlog("# Running", shlex.join(cmdargs))
        exitcode, out = self._runcommand(cmdargs, env)

        # Make sure libc/rust output directly to stderr/stdout shows up, albeit
        # not interleaved properly with well behaved output.
        with iolock:
            for line in out:
                sys.stdout.buffer.write(line)
            sys.stdout.flush()

        if exitcode == 1:
            if os.path.exists(self.errpath):
                with open(self.errpath, "rb") as f:
                    out = f.readlines()
        # exitcode can also be EXCEPTION_STATUS
        if exitcode == 0:
            out = self._refout

        return exitcode, out


firsterror = False


class NoopLock:
    def __enter__(self):
        pass

    def __exit__(self, exc_type, exc_value, traceback):
        pass

    def acquire(self):
        pass

    def release(self):
        pass


showprogress = sys.stderr.isatty()
_iolock = showprogress and RLock() or NoopLock()


class Progress:
    def __init__(self):
        self.lines = []
        self.out = sys.stderr

    def clear(self):
        self.update([])

    def update(self, lines):
        content = ""
        toclear = len(self.lines) - len(lines)
        moveup = len(self.lines) - 1
        if toclear > 0:
            content += "\r\033[K\033[1A" * toclear
            moveup -= toclear
        if moveup > 0:
            content += "\033[%dA" % moveup
        if lines:
            # Disable line wrapping while outputting the progress entries
            content += "\x1b[?7l"
            content += "\n".join("\r\033[K%s" % line.rstrip() for line in lines)
            content += "\x1b[?7h"
        self._write(content)
        self.lines = lines

    def setup(self):
        pass

    def finalize(self):
        pass

    def _write(self, content):
        with _iolock:
            self.out.write(content)
            self.out.flush()


progress = Progress()

if os.name == "nt":
    import ctypes

    _HANDLE = ctypes.c_void_p
    _DWORD = ctypes.c_ulong
    _INVALID_HANDLE_VALUE = _HANDLE(-1).value
    _STD_ERROR_HANDLE = _DWORD(-12).value

    _LPVOID = ctypes.c_void_p
    _BOOL = ctypes.c_long
    _UINT = ctypes.c_uint
    _HANDLE = ctypes.c_void_p

    _INVALID_HANDLE_VALUE = _HANDLE(-1).value

    ENABLE_VIRTUAL_TERMINAL_PROCESSING = 0x4

    PROCESS_SET_QUOTA = 0x0100
    PROCESS_TERMINATE = 0x0001

    # pyre-fixme[16]: Module `ctypes` has no attribute `WinDLL`.
    _kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

    _kernel32.CreateJobObjectA.argtypes = [_LPVOID, _LPVOID]
    _kernel32.CreateJobObjectA.restype = _HANDLE

    _kernel32.OpenProcess.argtypes = [_DWORD, _BOOL, _DWORD]
    _kernel32.OpenProcess.restype = _HANDLE

    _kernel32.AssignProcessToJobObject.argtypes = [_HANDLE, _HANDLE]
    _kernel32.AssignProcessToJobObject.restype = _BOOL

    _kernel32.TerminateJobObject.argtypes = [_HANDLE, _UINT]
    _kernel32.TerminateJobObject.restype = _BOOL

    _kernel32.CloseHandle.argtypes = [_HANDLE]
    _kernel32.CloseHandle.restype = _BOOL


if showprogress and os.name == "nt":
    # From mercurial/color.py.
    # Enable virtual terminal mode for the associated console.

    handle = _kernel32.GetStdHandle(_STD_ERROR_HANDLE)  # don't close the handle
    if handle == _INVALID_HANDLE_VALUE:
        showprogress = False
    else:
        mode = _DWORD(0)
        if _kernel32.GetConsoleMode(handle, ctypes.byref(mode)):
            if (mode.value & ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0:
                mode.value |= ENABLE_VIRTUAL_TERMINAL_PROCESSING
                if not _kernel32.SetConsoleMode(handle, mode):
                    showprogress = False


class IOLockWithProgress:
    def __enter__(self):
        _iolock.acquire()
        try:
            progress.clear()
        except:  # no re-raises
            _iolock.release()

    def __exit__(self, exc_type, exc_value, traceback):
        _iolock.release()


iolock = showprogress and IOLockWithProgress() or _iolock


class TestResult(unittest.TextTestResult):
    """Holds results when executing via unittest."""

    # Don't worry too much about accessing the non-public _TextTestResult.
    # It is relatively common in Python testing tools.
    def __init__(self, options, *args, **kwargs):
        super(TestResult, self).__init__(*args, **kwargs)

        self._options = options

        # unittest.TestResult didn't have skipped until 2.7. We need to
        # polyfill it.
        self.skipped = []

        # We have a custom "ignored" result that isn't present in any Python
        # unittest implementation. It is very similar to skipped. It may make
        # sense to map it into skip some day.
        self.ignored = []

        self.times = []
        self._firststarttime = None
        # Data stored for the benefit of generating xunit reports.
        self.successes = []
        self.faildata = {}

        if options.color == "auto":
            self.color = pygmentspresent and self.stream.isatty()
        elif options.color == "never":
            self.color = False
        else:  # 'always', for testing purposes
            self.color = pygmentspresent

    # pyrefly: ignore [bad-param-name-override]
    def addFailure(self, test, reason):
        self.failures.append((test, reason))

        if self._options.first:
            self.stop()
        else:
            if reason == "timed out":
                if not showprogress:
                    with iolock:
                        self.stream.write("t")
            else:
                if not self._options.nodiff:
                    with iolock:
                        self.stream.write("\n")
                        # Exclude the '\n' from highlighting to lex correctly
                        formatted = "ERROR: %s output changed\n" % test
                        self.stream.write(highlightmsg(formatted, self.color))
                if not showprogress:
                    with iolock:
                        self.stream.write("!")

            self.stream.flush()

    def addSuccess(self, test):
        if showprogress and not self.showAll:
            super(unittest.TextTestResult, self).addSuccess(test)
        else:
            with iolock:
                super(TestResult, self).addSuccess(test)
        self.successes.append(test)

    def addError(self, test, err):
        if showprogress and not self.showAll:
            super(unittest.TextTestResult, self).addError(test, err)
        else:
            with iolock:
                super(TestResult, self).addError(test, err)
        if self._options.first:
            self.stop()

    # Polyfill.
    def addSkip(self, test, reason):
        self.skipped.append((test, reason))
        if self.showAll:
            if reason != "not retesting":
                with iolock:
                    self.stream.writeln("skipped %s" % reason)
        else:
            if not showprogress:
                with iolock:
                    self.stream.write("s")
                    self.stream.flush()

    def addIgnore(self, test, reason):
        self.ignored.append((test, reason))
        if self.showAll:
            if reason != "not retesting":
                with iolock:
                    self.stream.writeln("ignored %s" % reason)
        else:
            if reason not in ("not retesting", "doesn't match keyword"):
                if not showprogress:
                    with iolock:
                        self.stream.write("i")
            else:
                self.testsRun += 1
            self.stream.flush()

    def addOutputMismatch(self, test, ret, got, expected):
        """Record a mismatch in test output for a particular test."""
        if self.shouldStop or firsterror:
            # don't print, some other test case already failed and
            # printed, we're just stale and probably failed due to our
            # temp dir getting cleaned up.
            return

        accepted = False
        lines = []

        with iolock:
            if self._options.nodiff:
                pass
            elif self._options.view:
                v = self._options.view
                os.system("%s %s %s" % (v, test.refpath, test.errpath))
            else:
                servefail, lines = getdiff(expected, got, test.refpath, test.errpath)
                if servefail:
                    raise test.failureException(
                        "server failed to start (HGPORT=%s)" % test._startport
                    )
                else:
                    self.stream.write("\n")
                    if len(lines) > self._options.maxdifflines:
                        omitted = len(lines) - self._options.maxdifflines
                        lines = lines[: self._options.maxdifflines] + [
                            b"... (%d lines omitted. set --maxdifflines to see more) ..."
                            % omitted
                        ]
                    for line in lines:
                        line = highlightdiff(line, self.color)
                        self.stream.flush()
                        self.stream.buffer.write(line)
                        self.stream.buffer.flush()

            # handle interactive prompt without releasing iolock
            if self._options.interactive:
                if test.readrefout() != expected:
                    self.stream.write(
                        "Reference output has changed (run again to prompt changes)"
                    )
                else:
                    self.stream.write("Accept this change? [n]o/yes/all ")
                    self.stream.flush()
                    if self._options.update_output:
                        isyes = True
                    else:
                        answer = sys.stdin.readline().strip()
                        if answer.lower() in ("a", "all"):
                            self._options.update_output = True
                            isyes = True
                        else:
                            isyes = answer.lower() in ("y", "yes")
                    if isyes:
                        if test.path.endswith(".t"):
                            rename(test.errpath, test.path)
                        else:
                            rename(test.errpath, "%s.out" % test.path)
                        accepted = True
            if not accepted:
                self.faildata[test.name] = b"".join(lines)

        return accepted

    def startTest(self, test):
        super(TestResult, self).startTest(test)

        # os.times module computes the user time and system time spent by
        # child's processes along with real elapsed time taken by a process.
        test.started = os_times()
        if self._firststarttime is None:  # thread racy but irrelevant
            self._firststarttime = test.started[4]

    def stopTest(self, test, interrupted=False):
        super(TestResult, self).stopTest(test)

        test.stopped = os_times()

        starttime = test.started
        endtime = test.stopped
        origin = self._firststarttime
        self.times.append(
            (
                test.name,
                endtime[2] - starttime[2],  # user space CPU time
                endtime[3] - starttime[3],  # sys  space CPU time
                endtime[4] - starttime[4],  # real time
                starttime[4] - origin,  # start date in run context
                endtime[4] - origin,  # end date in run context
            )
        )

        if interrupted:
            with iolock:
                self.stream.writeln(
                    "INTERRUPTED: %s (after %d seconds)"
                    % (test.name, self.times[-1][3])
                )


class TestSuite(unittest.TestSuite):
    """Custom unittest TestSuite that knows how to execute Mercurial tests."""

    def __init__(
        self,
        testdir,
        jobs=1,
        whitelist=None,
        blacklist=None,
        retest=False,
        keywords=None,
        loop=False,
        runs_per_test=1,
        loadtest=None,
        showchannels=False,
        *args,
        **kwargs,
    ):
        """Create a new instance that can run tests with a configuration.

        testdir specifies the directory where tests are executed from. This
        is typically the ``tests`` directory from Mercurial's source
        repository.

        jobs specifies the number of jobs to run concurrently. Each test
        executes on its own thread. Tests actually spawn new processes, so
        state mutation should not be an issue.

        If there is only one job, it will use the main thread.

        whitelist and blacklist denote tests that have been whitelisted and
        blacklisted, respectively. These arguments don't belong in TestSuite.
        Instead, whitelist and blacklist should be handled by the thing that
        populates the TestSuite with tests. They are present to preserve
        backwards compatible behavior which reports skipped tests as part
        of the results.

        retest denotes whether to retest failed tests. This arguably belongs
        outside of TestSuite.

        keywords denotes key words that will be used to filter which tests
        to execute. This arguably belongs outside of TestSuite.

        loop denotes whether to loop over tests forever.
        """
        super(TestSuite, self).__init__(*args, **kwargs)

        self._jobs = jobs
        self._whitelist = whitelist
        self._blacklist = blacklist
        self._retest = retest
        self._keywords = keywords
        self._loop = loop
        self._runs_per_test = runs_per_test
        self._loadtest = loadtest
        self._showchannels = showchannels

    # pyrefly: ignore [bad-override]
    def run(self, result):
        # We have a number of filters that need to be applied. We do this
        # here instead of inside Test because it makes the running logic for
        # Test simpler.
        tests = []
        num_tests = [0]
        for test in self._tests:

            def get():
                num_tests[0] += 1
                if getattr(test, "should_reload", False):
                    # pyrefly: ignore [not-callable]
                    return self._loadtest(test, num_tests[0])
                return test

            # pyrefly: ignore [missing-attribute]
            if not os.path.exists(test.path):
                result.addSkip(test, "Doesn't exist")
                continue

            # pyrefly: ignore [missing-attribute]
            if not (self._whitelist and test.basename in self._whitelist):
                # pyrefly: ignore [missing-attribute]
                if self._blacklist and test.basename in self._blacklist:
                    result.addSkip(test, "blacklisted")
                    continue

                # pyrefly: ignore [missing-attribute]
                if self._retest and not os.path.exists(test.errpath):
                    result.addIgnore(test, "not retesting")
                    continue

                if self._keywords:
                    # pyrefly: ignore [missing-attribute]
                    with open(test.path, "r") as f:
                        # pyrefly: ignore [missing-attribute]
                        t = f.read().lower() + test.basename.lower()
                    ignored = False
                    for k in self._keywords.lower().split():
                        if k not in t:
                            result.addIgnore(test, "doesn't match keyword")
                            ignored = True
                            break

                    if ignored:
                        continue
            for _ in range(self._runs_per_test):
                tests.append(get())

        runtests = list(tests)
        done = queue.Queue()
        running = 0

        channels = [""] * self._jobs
        runningtests = collections.OrderedDict()  # {test name: (test, start time)}

        def job(test, result):
            for n, v in enumerate(channels):
                if not v:
                    channel = n
                    break
            else:
                raise ValueError("Could not find output channel")
            runningtests[test.name] = (test, time.time())
            channels[channel] = "=" + test.name[5:].split(".")[0]
            try:
                test(result)
                done.put(None)
            except KeyboardInterrupt:
                pass
            except:  # re-raises
                done.put(("!", test, "run-test raised an error, see traceback"))
                raise
            finally:
                try:
                    del runningtests[test.name]
                except KeyError:
                    pass
                try:
                    channels[channel] = ""
                except IndexError:
                    pass

        def stat():
            count = 0
            while channels:
                d = "\n%03s  " % count
                for n, v in enumerate(channels):
                    if v:
                        d += v[0]
                        channels[n] = v[1:] or "."
                    else:
                        d += " "
                    d += " "
                with iolock:
                    sys.stdout.write(d + "  ")
                    sys.stdout.flush()
                for x in range(10):
                    if channels:
                        time.sleep(0.1)
                count += 1

        def singleprogressbar(value, total, width=14, char="="):
            barwidth = width - 2
            if total:
                if value > total:
                    value = total
                progresschars = char * int(value * barwidth / total)
                if progresschars and len(progresschars) < barwidth:
                    progresschars += ">"
                return "[%-*s]" % (barwidth, progresschars)
            else:
                return " " * width

        blacklisted = len(result.skipped)
        initialtestsrun = result.testsRun

        def progressrenderer():
            lines = []
            suitestart = time.time()
            total = len(runtests)
            while channels:
                failed = len(result.failures) + len(result.errors)
                skipped = len(result.skipped) - blacklisted
                testsrun = result.testsRun - initialtestsrun
                remaining = total - testsrun - skipped
                passed = testsrun - failed - len(runningtests)
                now = time.time()
                timepassed = now - suitestart
                lines = []
                runningfrac = 0.0
                for name, (test, teststart) in list(runningtests.items()):
                    try:
                        saltseen, saltcount, linenum = getattr(test, "progress")
                        runningfrac += saltseen * 1.0 / saltcount
                        testprogress = singleprogressbar(saltseen, saltcount, char="-")
                        linenum = "(%4s)" % linenum
                    except Exception:
                        testprogress = singleprogressbar(0, 0)
                        linenum = " " * 6
                    lines.append(
                        "%s %s %-52s %.1fs"
                        % (testprogress, linenum, name[:52], now - teststart)
                    )
                progfrac = runningfrac + failed + passed + skipped
                lines[0:0] = [
                    "%s (%3s%%) %-52s %.1fs"
                    % (
                        singleprogressbar(progfrac, total),
                        int(progfrac * 100 / total) if total else 0,
                        "%s Passed. %s Failed. %s Skipped. %s Remaining"
                        % (passed, failed, skipped, remaining),
                        timepassed,
                    )
                ]
                progress.update(lines)
                time.sleep(0.1)

        stoppedearly = False

        if self._showchannels:
            statthread = threading.Thread(target=stat, name="stat")
            statthread.start()
        elif showprogress:
            progressthread = threading.Thread(target=progressrenderer, name="progress")
            progressthread.daemon = True
            progressthread.start()

        try:
            while tests or running:
                if not done.empty() or running == self._jobs or not tests:
                    try:
                        done.get(True, 1)
                        running -= 1
                        if result and result.shouldStop:
                            stoppedearly = True
                            break
                    except queue.Empty:
                        continue
                if tests and not running == self._jobs:
                    test = tests.pop(0)
                    if self._loop:
                        if getattr(test, "should_reload", False):
                            num_tests[0] += 1
                            # pyrefly: ignore [not-callable]
                            tests.append(self._loadtest(test, num_tests[0]))
                        else:
                            tests.append(test)
                    if self._jobs == 1:
                        job(test, result)
                    else:
                        t = threading.Thread(
                            target=job, name=test.name, args=(test, result)
                        )
                        t.start()
                    running += 1

            # If we stop early we still need to wait on started tests to
            # finish. Otherwise, there is a race between the test completing
            # and the test's cleanup code running. This could result in the
            # test reporting incorrect.
            if stoppedearly:
                while running:
                    try:
                        done.get(True, 1)
                        running -= 1
                    except queue.Empty:
                        continue
        except KeyboardInterrupt:
            for test in runtests:
                test.abort()

        channels = []

        return result


# Save the most recent 5 wall-clock runtimes of each test to a
# human-readable text file named .testtimes. Tests are sorted
# alphabetically, while times for each test are listed from oldest to
# newest.


def loadtimes(outputdir):
    times = []
    try:
        with open(os.path.join(outputdir, ".testtimes-")) as fp:
            for line in fp:
                ts = line.split()
                times.append((ts[0], [float(t) for t in ts[1:]]))
    except IOError as err:
        if err.errno != errno.ENOENT:
            raise
    return times


def savetimes(outputdir, result):
    saved = dict(loadtimes(outputdir))
    maxruns = 5
    skipped = set([str(t[0]) for t in result.skipped])
    for tdata in result.times:
        test, real = tdata[0], tdata[3]
        if test not in skipped:
            ts = saved.setdefault(test, [])
            ts.append(real)
            ts[:] = ts[-maxruns:]

    fd, tmpname = tempfile.mkstemp(prefix=".testtimes", dir=outputdir, text=True)
    with os.fdopen(fd, "w") as fp:
        for name, ts in sorted(saved.items()):
            fp.write("%s %s\n" % (name, " ".join(["%.3f" % (t,) for t in ts])))
    timepath = os.path.join(outputdir, ".testtimes")
    try:
        os.unlink(timepath)
    except OSError:
        pass
    try:
        os.rename(tmpname, timepath)
    except OSError:
        pass


def xunit_time(t):
    # The TestPilot documentation says to follow the JUnit spec, which says time
    # should be in seconds, but that's not quite what TestPilot actually does
    # (see D2822375: it expects the time in milliseconds). There's other code at
    # Facebook that relies on the TPX to identify if we're running
    # tests, so it should be reasonably safe (albeit hacky) to rely on this.
    if os.environ.get("TESTPILOT_PROCESS") or os.environ.get("TPX"):
        t = t * 1000
    return "%.3f" % t


class TextTestRunner(unittest.TextTestRunner):
    """Custom unittest test runner that uses appropriate settings."""

    def __init__(self, runner, *args, **kwargs):
        super(TextTestRunner, self).__init__(*args, **kwargs)

        self._runner = runner

    def listtests(self, test):
        result = TestResult(self._runner.options, self.stream, self.descriptions, 0)
        test = sorted(test, key=lambda t: t.name)
        for t in test:
            print(t.name)
            result.addSuccess(t)

        if self._runner.options.xunit:
            with open(self._runner.options.xunit, "wb") as xuf:
                self._writexunit(result, xuf)

        if self._runner.options.json:
            jsonpath = os.path.join(self._runner._outputdir, "report.json")
            with open(jsonpath, "w") as fp:
                self._writejson(result, fp)

        return result

    def run(self, test):
        result = TestResult(
            self._runner.options, self.stream, self.descriptions, self.verbosity
        )

        test(result)

        failed = len(result.failures)
        skipped = len(result.skipped)
        ignored = len(result.ignored)

        with iolock:
            self.stream.write("\n%s\n" % ("-" * 70))

            for title, tests in [
                ("Skipped", result.skipped),
                ("Failed", result.failures),
                ("Errored", result.errors),
            ]:
                if not tests:
                    continue
                # Group by failure messages.
                messages = sorted({msg for _test, msg in tests})
                for message in messages:
                    # Normalize "test-foo.t (case bar)" to filename "test-foo.t".
                    names = sorted(
                        # pyrefly: ignore [missing-attribute]
                        {test.name.split()[0] for test, msg in tests if msg == message}
                    )
                    self.stream.write(
                        "%s %s tests (%s):\n" % (title, len(names), message)
                    )
                    # Print the file names without noises (ex. why test are failed).
                    # The file names can be copy-pasted to adhoc scripts like
                    # `hg revert $FILENAMES`.
                    for name in names:
                        self.stream.write("  %s\n" % (name,))
                    self.stream.write("\n")
                    # Also write the file names to temporary files.  So it can be
                    # used in adhoc scripts like `hg revert $(cat .testfailed)`.
                    testsdir = os.path.abspath(os.path.dirname(__file__))
                    if not os.path.exists(testsdir):
                        # It's possible for the current directory to not exist if tests are run using Buck
                        testsdir = ""
                    filepath = os.path.join(testsdir, f".test{title.lower()}")
                    with open(filepath, "a") as f:
                        for name in names:
                            f.write(name + "\n")
                        f.write("\n")

            if self._runner.options.xunit:
                with open(self._runner.options.xunit, "wb") as xuf:
                    self._writexunit(result, xuf)

            if self._runner.options.json:
                jsonpath = os.path.join(self._runner._outputdir, "report.json")
                with open(jsonpath, "w") as fp:
                    self._writejson(result, fp)

            self._runner._checkhglib("Tested")

            savetimes(self._runner._outputdir, result)

            if failed and self._runner.options.known_good_rev:
                self._bisecttests(t for t, m in result.failures)
            self.stream.writeln(
                "# Ran %d tests, %d skipped, %d failed."
                % (result.testsRun, skipped + ignored, failed)
            )
            # pyrefly: ignore [missing-attribute]
            result.testsSkipped = skipped + ignored
            result.testsPassed = len(result.successes)
            if failed:
                self.stream.writeln(
                    "python hash seed: %s" % os.environ["PYTHONHASHSEED"]
                )
            if self._runner.options.time:
                self.printtimes(result.times)

            if self._runner.options.exceptions:
                exceptions = aggregateexceptions(
                    os.path.join(self._runner._outputdir, "exceptions")
                )
                total = sum(exceptions.values())

                self.stream.writeln("Exceptions Report:")
                self.stream.writeln(
                    "%d total from %d frames" % (total, len(exceptions))
                )
                for (frame, line, exc), count in exceptions.most_common():
                    self.stream.writeln("%d\t%s: %s" % (count, frame, exc))

            self.stream.flush()

        return result

    def _bisecttests(self, tests):
        bisectcmd = ["hg", "bisect"]
        bisectrepo = self._runner.options.bisect_repo
        if bisectrepo:
            bisectcmd.extend(["-R", os.path.abspath(bisectrepo)])

        def pread(args):
            env = os.environ.copy()
            env["HGPLAIN"] = "1"
            p = subprocess.Popen(
                args, stderr=subprocess.STDOUT, stdout=subprocess.PIPE, env=env
            )
            # pyrefly: ignore [missing-attribute]
            data = p.stdout.read()
            p.wait()
            return data

        for test in tests:
            (pread(bisectcmd + ["--reset"]),)
            pread(bisectcmd + ["--bad", "."])
            pread(bisectcmd + ["--good", self._runner.options.known_good_rev])
            # TODO: we probably need to forward more options
            # that alter hg's behavior inside the tests.
            opts = ""
            withhg = self._runner.options.with_hg
            if withhg:
                opts += " --with-hg=%s " % shellquote(withhg)
            rtc = "%s %s %s %s" % (PYTHON, sys.argv[0], opts, test)
            data = pread(bisectcmd + ["--command", rtc])
            m = re.search(
                (
                    rb"\nThe first (?P<goodbad>bad|good) revision "
                    rb"is:\ncommit: +(?P<node>[a-f0-9]+)\n.*\n"
                    rb"summary: +(?P<summary>[^\n]+)\n"
                ),
                data,
                (re.MULTILINE | re.DOTALL),
            )
            if m is None:
                # self.stream.writeln("Failed to identify failure point for %s:\n%s" % (test, data.decode("utf-8")))
                self.stream.writeln("Failed to identify failure point for %s" % test)
                continue
            dat = m.groupdict()
            verb = "broken" if dat["goodbad"] == b"bad" else b"fixed"
            self.stream.writeln(
                "%s %s by %s (%s)"
                % (
                    test,
                    verb,
                    dat["node"].decode("utf-8"),
                    dat["summary"].decode("utf-8"),
                )
            )

    def printtimes(self, times):
        # iolock held by run
        self.stream.writeln("# Producing time report")
        times.sort(key=lambda t: (t[3]))
        cols = "%7.3f %7.3f %7.3f %7.3f %7.3f   %s"
        self.stream.writeln(
            "%-7s %-7s %-7s %-7s %-7s   %s"
            % ("start", "end", "cuser", "csys", "real", "Test")
        )
        for tdata in times:
            test = tdata[0]
            cuser, csys, real, start, end = tdata[1:6]
            self.stream.writeln(cols % (start, end, cuser, csys, real, test))

    @staticmethod
    def _writexunit(result, outf):
        # See http://llg.cubic.org/docs/junit/ for a reference.
        timesd = dict((t[0], t[3]) for t in result.times)
        doc = minidom.Document()
        s = doc.createElement("testsuite")
        s.setAttribute("name", "run-tests")
        s.setAttribute("tests", str(result.testsRun))
        s.setAttribute("errors", "0")  # TODO
        s.setAttribute("failures", str(len(result.failures)))
        s.setAttribute("skipped", str(len(result.skipped) + len(result.ignored)))
        doc.appendChild(s)
        for tc in result.successes:
            t = doc.createElement("testcase")
            t.setAttribute("name", tc.name)
            tctime = timesd.get(tc.name)
            if tctime is not None:
                t.setAttribute("time", xunit_time(tctime))
            s.appendChild(t)
        for tc, err in sorted(result.faildata.items()):
            t = doc.createElement("testcase")
            t.setAttribute("name", tc)
            tctime = timesd.get(tc)
            if tctime is not None:
                t.setAttribute("time", xunit_time(tctime))
            # createCDATASection expects a unicode or it will
            # convert using default conversion rules, which will
            # fail if string isn't ASCII.
            err = cdatasafe(err).decode("utf-8", "replace")
            cd = doc.createCDATASection(err)
            # Use 'failure' here instead of 'error' to match errors = 0,
            # failures = len(result.failures) in the testsuite element.
            failelem = doc.createElement("failure")
            failelem.setAttribute("message", "output changed")
            failelem.setAttribute("type", "output-mismatch")
            failelem.appendChild(cd)
            t.appendChild(failelem)
            s.appendChild(t)
        for tc, message in result.skipped:
            # According to the schema, 'skipped' has no attributes. So store
            # the skip message as a text node instead.
            t = doc.createElement("testcase")
            t.setAttribute("name", tc.name)
            binmessage = message.encode("utf-8")
            message = cdatasafe(binmessage).decode("utf-8", "replace")
            cd = doc.createCDATASection(message)
            skipelem = doc.createElement("skipped")
            skipelem.appendChild(cd)
            t.appendChild(skipelem)
            s.appendChild(t)
        outf.write(doc.toprettyxml(indent="  ", encoding="utf-8"))

    @staticmethod
    def _writejson(result, outf):
        timesd = {}
        for tdata in result.times:
            test = tdata[0]
            timesd[test] = tdata[1:]

        outcome = {}
        groups = [
            ("success", ((tc, None) for tc in result.successes)),
            ("failure", result.failures),
            ("skip", result.skipped),
        ]
        for res, testcases in groups:
            for tc, __ in testcases:
                if tc.name in timesd:
                    diff = result.faildata.get(tc.name, b"")
                    try:
                        diff = diff.decode("unicode_escape")
                    except UnicodeDecodeError as e:
                        diff = "%r decoding diff, sorry" % e
                    tres = {
                        "result": res,
                        "time": ("%0.3f" % timesd[tc.name][2]),
                        "cuser": ("%0.3f" % timesd[tc.name][0]),
                        "csys": ("%0.3f" % timesd[tc.name][1]),
                        "start": ("%0.3f" % timesd[tc.name][3]),
                        "end": ("%0.3f" % timesd[tc.name][4]),
                        "diff": diff,
                    }
                else:
                    # blacklisted test
                    tres = {"result": res}

                outcome[tc.name] = tres
        outf.write(
            json.dumps(outcome, sort_keys=True, indent=4, separators=(",", ": "))
        )


class TestpilotTestResult:
    def __init__(self, testpilotjson):
        self.testsSkipped = 0
        self.testsPassed = 0
        self.errors = 0
        self.failures = []

        with open(testpilotjson, "r") as fp:
            for line in fp:
                result = json.loads(line)
                summary = result["summary"]
                if summary == "passed":
                    self.testsPassed += 1
                else:
                    if summary == "OMITTED" or summary == "skipped":
                        self.testsSkipped += 1
                    else:
                        self.failures += result
                        self.errors += 1


class TestpilotTestRunner:
    def __init__(self, runner):
        self._runner = runner

    def run(self, tests):
        testlist = []

        for t in tests:
            testlist.append(
                {
                    "type": "custom",
                    "target": "hg-%s-%s" % (sys.platform, t),
                    "command": [os.path.abspath(__file__), "%s" % t.path],
                }
            )

        jsonpath = os.path.join(self._runner._outputdir, "buck-test-info.json")
        with open(jsonpath, "w") as fp:
            json.dump(testlist, fp)

        # Remove the proxy settings. If they are set, testpilot will hang
        # trying to get test information from the network
        env = os.environ
        env["http_proxy"] = ""
        env["https_proxy"] = ""

        testpilotjson = os.path.join(self._runner._outputdir, "testpilot-result.json")
        try:
            os.remove(testpilotjson)
        except OSError as e:
            if e.errno != errno.ENOENT:
                raise

        subprocess.call(
            [
                "testpilot",
                "--buck-test-info",
                jsonpath,
                "--print-json-to-file",
                testpilotjson,
            ],
            env=env,
        )

        return TestpilotTestResult(testpilotjson)


KICKSTART_FSEVENTSD = "/usr/local/bin/kickstart-fseventsd"


def startfseventsd():
    """Start fseventsd on macOS hosts that provide KICKSTART_FSEVENTSD.

    Watchman cannot watch anything on macOS without fseventsd. Sandcastle's
    Mac VM images keep it off and provide the script, which the job user may
    run with ``sudo -n``; other hosts do not have the script.
    """
    if sys.platform == "darwin" and os.access(KICKSTART_FSEVENTSD, os.X_OK):
        subprocess.call(["sudo", "-n", KICKSTART_FSEVENTSD])


class TestRunner:
    """Holds context for executing tests.

    Tests rely on a lot of state. This object holds it for them.
    """

    # Programs required to run tests.
    REQUIREDTOOLS = [
        "diff",
        "grep",
        "unzip",
        "gunzip",
        "bunzip2",
        "sed",
        "cmp",
        "dd",
    ]

    # Maps file extensions to test class.
    TESTTYPES = [(".py", PythonTest), (".t", TTest)]

    def __init__(self):
        self.options = None
        self._hgroot = None
        self._testdir = None
        self._outputdir = None
        self._hgtmp = None
        self._installdir = None
        self._bindir = None
        self._tmpbinddir = None
        self._pythondir = None
        self._coveragefile = None
        self._createdfiles = []
        self._hgcommand = None
        self._hgpath = None
        self._portoffset = 0
        self._ports = {}

    def run(self, args, parser=None):
        """Run the test suite."""
        oldmask = os.umask(0o22)
        try:
            if showprogress:
                progress.setup()
            parser = parser or getparser()
            options = parseargs(args, parser)
            tests = list(options.tests)
            if options.test_list is not None:
                for listfile in options.test_list:
                    with open(listfile, "r") as f:
                        tests.extend(t for t in f.read().splitlines() if t)
            self.options = options

            self._checktools()
            testdescs = self.findtests(tests)
            if options.profile_runner:
                # pyre-fixme[21]: Could not find module `statprof`.
                import statprof

                statprof.start()
            result = self._run(testdescs)
            if options.profile_runner:
                # pyrefly: ignore [unbound-name]
                statprof.stop()
                statprof.display()
            return result

        finally:
            if showprogress:
                progress.finalize()
            os.umask(oldmask)

    def _run(self, testdescs):
        # pyrefly: ignore [missing-attribute]
        if self.options.random:
            random.shuffle(testdescs)
        else:
            # keywords for slow tests
            slow = {
                "svn": 10,
                "cvs": 10,
                "hghave": 10,
                "run-tests": 10,
                "corruption": 10,
                "race": 10,
                "i18n": 10,
                "check": 100,
                "gendoc": 100,
                "contrib-perf": 200,
            }
            perf = {}

            def sortkey(f):
                # run largest tests first, as they tend to take the longest
                f = f["path"]
                try:
                    return perf[f]
                except KeyError:
                    try:
                        val = -os.stat(f).st_size
                    except OSError as e:
                        if e.errno != errno.ENOENT:
                            raise
                        perf[f] = -1e9  # file does not exist, tell early
                        return -1e9
                    for kw, mul in slow.items():
                        if kw in f:
                            val *= mul
                    if f.endswith(".py"):
                        val /= 10.0
                    perf[f] = val / 1000.0
                    return perf[f]

            testdescs.sort(key=sortkey)

        testdir = os.getcwd()
        os.environ["TESTDIR"] = testdir
        self._testdir = testdir

        # assume all tests in same folder for now
        if testdescs:
            pathname = os.path.dirname(testdescs[0]["path"])
            if pathname:
                os.environ["TESTDIR"] = os.path.join(os.environ["TESTDIR"], pathname)
        # pyrefly: ignore [missing-attribute]
        if self.options.outputdir:
            self._outputdir = canonpath(self.options.outputdir)
        else:
            self._outputdir = self._testdir
            # pyrefly: ignore [unbound-name]
            if testdescs and pathname:
                self._outputdir = os.path.join(self._outputdir, pathname)

        if "PYTHONHASHSEED" not in os.environ:
            # use a random python hash seed all the time
            # we do the randomness ourself to know what seed is used
            os.environ["PYTHONHASHSEED"] = str(random.getrandbits(32))

        # pyrefly: ignore [missing-attribute]
        if self.options.record:
            # pyrefly: ignore [missing-attribute]
            self.options.keep_tmpdir = True

        # pyrefly: ignore [missing-attribute]
        if self.options.tmpdir:
            # pyrefly: ignore [missing-attribute]
            self.options.keep_tmpdir = True
            tmpdir = self.options.tmpdir
            # pyrefly: ignore [bad-argument-type]
            if os.path.exists(tmpdir):
                # Meaning of tmpdir has changed since 1.3: we used to create
                # HGTMP inside tmpdir; now HGTMP is tmpdir.  So fail if
                # tmpdir already exists.
                print("error: temp dir %r already exists" % tmpdir)
                return 1

                # Automatically removing tmpdir sounds convenient, but could
                # really annoy anyone in the habit of using "--tmpdir=/tmp"
                # or "--tmpdir=$HOME".
                # vlog("# Removing temp dir", tmpdir)
                # shutil.rmtree(tmpdir)
            # pyrefly: ignore [bad-argument-type]
            os.makedirs(tmpdir)
        else:
            d = None
            if os.name == "nt":
                # without this, we get the default temp dir location, but
                # in all lowercase, which causes troubles with paths (issue3490)
                d = os.environ.get("TMP", None)
            tmpdir = tempfile.mkdtemp("", "hgtests.", d)

        # pyrefly: ignore [no-matching-overload]
        hgtmp = os.path.realpath(tmpdir)
        self._hgtmp = hgtmp
        os.environ["HGTMP"] = hgtmp

        # pyrefly: ignore [missing-attribute]
        if self.options.with_hg:
            self._installdir = None
            whg = self.options.with_hg
            # pyrefly: ignore [no-matching-overload]
            self._bindir = os.path.dirname(os.path.realpath(whg))
            # use full path, since _hgcommand will also be used as ui.remotecmd
            # pyrefly: ignore [no-matching-overload]
            self._hgcommand = os.path.realpath(whg)
            self._tmpbindir = os.path.join(self._hgtmp, "install", "bin")
            os.makedirs(self._tmpbindir)

            # This looks redundant with how Python initializes sys.path from
            # the location of the script being executed.  Needed because the
            # "hg" specified by --with-hg is not the only Python script
            # executed in the test suite that needs to import 'mercurial'
            # ... which means it's not really redundant at all.
            self._pythondir = self._bindir
        else:
            self._installdir = os.path.join(self._hgtmp, "install")
            self._bindir = os.path.join(self._installdir, "bin")
            self._hgcommand = os.path.join(self._bindir, "hg")
            self._tmpbindir = self._bindir
            self._pythondir = os.path.join(self._installdir, "lib", "python")

        # pyrefly: ignore [missing-attribute]
        if self.options.with_watchman or self.options.watchman:
            # pyrefly: ignore [missing-attribute]
            self._watchman = self.options.with_watchman or "watchman"
            os.environ["HGFSMONITOR_TESTS"] = "1"
            startfseventsd()
        else:
            os.environ["BINDIR"] = self._bindir
            self._watchman = None
            if "HGFSMONITOR_TESTS" in os.environ:
                del os.environ["HGFSMONITOR_TESTS"]

        os.environ["BINDIR"] = self._bindir
        os.environ["TMPBINDIR"] = self._tmpbindir
        os.environ["PYTHON"] = PYTHON

        # One of our Buck targets sets this env var pointing to all the misc.
        # files under tests/ including the .t tests themselves. run-tests.py
        # directly tries to launch files like tinit.sh, so we need to help
        # it to find these files.
        if (runtestdir := os.environ.get("RUNTESTDIR", None)) is None:
            runtestdir = os.path.abspath(os.path.dirname(__file__))
            os.environ["RUNTESTDIR"] = runtestdir
        path = [self._bindir, runtestdir] + os.environ["PATH"].split(os.pathsep)
        if os.path.islink(__file__):
            # test helper will likely be at the end of the symlink
            realfile = os.path.realpath(__file__)
            realdir = os.path.abspath(os.path.dirname(realfile))
            path.insert(2, realdir)
        if self._testdir != runtestdir:
            path = [self._testdir] + path
        if self._tmpbindir != self._bindir:
            path = [self._tmpbindir] + path
        os.environ["PATH"] = os.pathsep.join(path)

        # Include TESTDIR in PYTHONPATH so that out-of-tree extensions
        # can run .../tests/run-tests.py test-foo where test-foo
        # adds an extension to HGRC. Also include run-test.py directory to
        # import modules like heredoctest.
        # self._pythondir should make "import mercurial" do the right thing.
        pypath = [self._pythondir, self._testdir, runtestdir]
        # We have to augment PYTHONPATH, rather than simply replacing
        # it, in case external libraries are only available via current
        # PYTHONPATH.  (In particular, the Subversion bindings on OS X
        # are in /opt/subversion.)
        oldpypath = os.environ.get(IMPL_PATH)
        if oldpypath:
            pypath.append(oldpypath)
        os.environ[IMPL_PATH] = os.pathsep.join(pypath)

        # pyrefly: ignore [missing-attribute]
        if self.options.allow_slow_tests:
            os.environ["HGTEST_SLOW"] = "slow"
        elif "HGTEST_SLOW" in os.environ:
            del os.environ["HGTEST_SLOW"]

        self._coveragefile = os.path.join(self._testdir, ".coverage")

        # pyrefly: ignore [missing-attribute]
        if self.options.exceptions:
            exceptionsdir = os.path.join(self._outputdir, "exceptions")
            try:
                os.makedirs(exceptionsdir)
            except OSError as e:
                if e.errno != errno.EEXIST:
                    raise

            # Remove all existing exception reports.
            for f in os.listdir(exceptionsdir):
                os.unlink(os.path.join(exceptionsdir, f))

            os.environ["HGEXCEPTIONSDIR"] = exceptionsdir
            logexceptions = os.path.join(self._testdir, "logexceptions.py")
            # pyrefly: ignore [missing-attribute]
            self.options.extra_config_opt.append(
                # pyrefly: ignore [missing-attribute]
                "extensions.logexceptions=%s" % logexceptions.decode("utf-8")
            )

        vlog(f"# Show progress: {showprogress}")
        vlog(f"# IO lock: {type(_iolock).__name__}")
        vlog("# Using TESTDIR", self._testdir)
        vlog("# Using RUNTESTDIR", os.environ["RUNTESTDIR"])
        vlog("# Using HGTMP", self._hgtmp)
        vlog("# Using PATH", os.environ["PATH"])
        vlog("# Using", IMPL_PATH, os.environ[IMPL_PATH])
        if self._watchman:
            vlog("# Using watchman", self._watchman)
        vlog("# Writing to directory", self._outputdir)

        try:
            return self._runtests(testdescs) or 0
        finally:
            time.sleep(0.1)
            self._cleanup()

    def findtests(self, args):
        """Finds possible test files from arguments.

        If you wish to inject custom tests into the test harness, this would
        be a good function to monkeypatch or override in a derived class.
        """

        def transform_test_basename(path):
            """transform test_revert_t to test-revert.t"""
            dirname, basename = os.path.split(path)
            if basename.startswith("test_") and basename.endswith("_t"):
                basename = basename[:-2].replace("_", "-") + ".t"
                return os.path.join(dirname, basename)
            else:
                return path

        if not args:
            # pyrefly: ignore [missing-attribute]
            if self.options.changed:
                proc = Popen4(
                    'hg st --rev "%s" -man0 .' % self.options.changed, None, 0
                )
                stdout, stderr = proc.communicate()
                args = _strpath(stdout).strip("\0").split("\0")
            else:
                args = os.listdir(".")

        expanded_args = []
        for arg in args:
            if os.path.isdir(arg):
                if not arg.endswith("/"):
                    arg += "/"
                expanded_args.extend([arg + a for a in os.listdir(arg)])
            else:
                expanded_args.append(arg)
        args = expanded_args

        tests = []
        for t in args:
            t = transform_test_basename(t)
            if not (
                os.path.basename(t).startswith("test-")
                and (t.endswith(".py") or t.endswith(".t"))
            ):
                continue
            if t.endswith(".t"):
                if compatiblewithdebugruntest(t):
                    tests.append({"path": t, "runner": "debugruntest"})
                    continue
                # .t file may contain multiple test cases
                cases = sorted(parsettestcases(t))
                if cases:
                    tests += [{"path": t, "case": c} for c in sorted(cases)]
                else:
                    tests.append({"path": t})
            else:
                tests.append({"path": t})
        return tests

    def _runtests(self, testdescs):
        def _reloadtest(test, i):
            # convert a test back to its description dict
            desc = {"path": test.path}
            if isinstance(test, DebugRunTestTest):
                desc["runner"] = "debugruntest"
            else:
                case = getattr(test, "_case", None)
                if case:
                    desc["case"] = case
            return self._gettest(desc, i)

        failed = False
        allskipped = False
        errored = False
        try:
            # pyrefly: ignore [missing-attribute]
            if self.options.restart:
                orig = list(testdescs)
                while testdescs:
                    desc = testdescs[0]
                    # desc['path'] is a relative path
                    if "case" in desc:
                        errpath = "%s.%s.err" % (desc["path"], desc["case"])
                    else:
                        errpath = "%s.err" % desc["path"]
                    # pyrefly: ignore [no-matching-overload]
                    errpath = os.path.join(self._outputdir, errpath)
                    if os.path.exists(errpath):
                        break
                    testdescs.pop(0)
                if not testdescs:
                    print("running all tests")
                    testdescs = orig

            tests = [self._gettest(d, i) for i, d in enumerate(testdescs)]

            # pyrefly: ignore [missing-attribute]
            kws = self.options.keywords

            # pyrefly: ignore [missing-attribute]
            vlog("# Running TestSuite with %d jobs" % self.options.jobs)
            # pyrefly: ignore [missing-attribute]
            retry = self.options.retry or 0
            attempt = 0
            # pyrefly: ignore [missing-attribute]
            retest = self.options.retest
            while True:
                suite = TestSuite(
                    self._testdir,
                    # pyrefly: ignore [missing-attribute]
                    jobs=self.options.jobs,
                    # pyrefly: ignore [missing-attribute]
                    whitelist=self.options.whitelisted,
                    # pyrefly: ignore [missing-attribute]
                    blacklist=self.options.blacklist,
                    retest=retest,
                    keywords=kws,
                    # pyrefly: ignore [missing-attribute]
                    loop=self.options.loop,
                    # pyrefly: ignore [missing-attribute]
                    runs_per_test=self.options.runs_per_test,
                    # pyrefly: ignore [missing-attribute]
                    showchannels=self.options.showchannels,
                    tests=tests,
                    loadtest=_reloadtest,
                )
                verbosity = 1
                # pyrefly: ignore [missing-attribute]
                if self.options.verbose:
                    verbosity = 2

                # pyrefly: ignore [missing-attribute]
                if self.options.testpilot:
                    runner = TestpilotTestRunner(self)
                else:
                    runner = TextTestRunner(self, verbosity=verbosity)

                # pyrefly: ignore [missing-attribute]
                if self.options.list_tests:
                    # pyrefly: ignore [missing-attribute]
                    result = runner.listtests(suite)
                else:
                    if self._installdir:
                        self._installhg()
                        self._checkhglib("Testing")
                    else:
                        self._usecorrectpython()

                    self._usecorrecthg()

                    result = runner.run(suite)
                    allskipped = result.testsSkipped > 0 and not (
                        result.testsPassed or result.failures or result.errors
                    )
                    if tests and result.errors:
                        errored = True

                if result.failures:
                    if attempt < retry:
                        attempt += 1
                        vlog(
                            "# Retrying failed tests attempt %d of %d"
                            % (attempt, retry)
                        )
                        retest = True
                        continue
                    failed = True

                break

            # pyrefly: ignore [missing-attribute]
            if self.options.anycoverage:
                self._outputcoverage()
        except KeyboardInterrupt:
            failed = True
            print("\ninterrupted!")

        if failed:
            return 1
        elif allskipped:
            # Preserve skips for unittestify without changing the default CLI
            # exit status for an all-skipped run.
            return Test.SKIPPED_STATUS if os.environ.get("HGTEST_REPORT_SKIPS") else 0
        elif errored:
            return 2

    def _getport(self, count):
        port = self._ports.get(count)  # do we have a cached entry?
        if port is None:
            portneeded = 3
            # above 100 tries we just give up and let test reports failure
            for tries in range(100):
                allfree = True
                # pyrefly: ignore [missing-attribute]
                port = self.options.port + self._portoffset
                for idx in range(portneeded):
                    if not checkportisavailable(port + idx):
                        allfree = False
                        break
                self._portoffset += portneeded
                if allfree:
                    break
            self._ports[count] = port
        return port

    def _gettest(self, testdesc, count):
        """Obtain a Test by looking at its filename.

        Returns a Test instance. The Test may not be runnable if it doesn't
        map to a known type.
        """
        path = testdesc["path"]
        if testdesc.get("runner") == "debugruntest":
            testcls = DebugRunTestTest
        else:
            lctest = path.lower()
            testcls = Test

            for ext, cls in self.TESTTYPES:
                if lctest.endswith(ext):
                    testcls = cls
                    break

        # pyrefly: ignore [no-matching-overload]
        refpath = os.path.join(self._testdir, path)
        # pyrefly: ignore [no-matching-overload]
        tmpdir = os.path.join(self._hgtmp, "child%d" % count)

        # extra keyword parameters. 'case' is used by .t tests
        kwds = dict((k, testdesc[k]) for k in ["case"] if k in testdesc)

        t = testcls(
            refpath,
            self._outputdir,
            tmpdir,
            # pyrefly: ignore [missing-attribute]
            keeptmpdir=self.options.keep_tmpdir,
            # pyrefly: ignore [missing-attribute]
            debug=self.options.debug,
            # pyrefly: ignore [missing-attribute]
            first=self.options.first,
            # pyrefly: ignore [missing-attribute]
            timeout=self.options.timeout,
            startport=self._getport(count),
            # pyrefly: ignore [missing-attribute]
            extraconfigopts=self.options.extra_config_opt,
            # pyrefly: ignore [missing-attribute]
            extrarcpaths=self.options.extra_rcpath,
            # pyrefly: ignore [missing-attribute]
            shell=self.options.shell,
            hgcommand=self._hgcommand,
            useipv6=useipv6,
            watchman=self._watchman,
            options=self.options,
            **kwds,
        )
        # pyrefly: ignore [missing-attribute]
        t.should_reload = True
        return t

    def _cleanup(self):
        """Clean up state from this test invocation."""
        # pyrefly: ignore [missing-attribute]
        if self.options.keep_tmpdir:
            return

        vlog("# Cleaning up HGTMP", self._hgtmp)
        # pyrefly: ignore [no-matching-overload]
        shutil.rmtree(self._hgtmp, True)
        for f in self._createdfiles:
            try:
                os.remove(f)
            except OSError:
                pass

    def _usecorrectpython(self):
        """Configure the environment to use the appropriate Python in tests."""
        # Tests must use the same interpreter as us or bad things will happen.
        pyexename = sys.platform == "win32" and "python.exe" or "python"
        if getattr(os, "symlink", None):
            vlog("# Making python executable in test path a symlink to '%s'" % PYTHON)
            mypython = os.path.join(self._tmpbindir, pyexename)
            try:
                if os.readlink(mypython) == PYTHON:
                    return
                os.unlink(mypython)
            except OSError as err:
                if err.errno != errno.ENOENT:
                    raise
            if self._findprogram(pyexename) != PYTHON:
                try:
                    os.symlink(PYTHON, mypython)
                    self._createdfiles.append(mypython)
                except OSError as err:
                    # child processes may race, which is harmless
                    if err.errno != errno.EEXIST:
                        raise
        else:
            exedir, exename = os.path.split(PYTHON)
            vlog(
                "# Modifying search path to find %s as %s in '%s'"
                % (exename, pyexename, exedir)
            )
            path = os.environ["PATH"].split(os.pathsep)
            while exedir in path:
                path.remove(exedir)
            os.environ["PATH"] = os.pathsep.join([exedir] + path)
            if not self._findprogram(pyexename):
                print("WARNING: Cannot find %s in search path" % pyexename)

    def _usecorrecthg(self):
        """Configure the environment to use the appropriate hg in tests."""
        # pyrefly: ignore [no-matching-overload]
        if os.path.basename(self._hgcommand) in ("hg", "hg.exe"):
            # No correction is needed
            return
        if getattr(os, "symlink", None):
            tmphgpath = os.path.join(self._tmpbindir, "hg")
            vlog("# Symlink %s to %s" % (self._hgcommand, tmphgpath))

            try:
                # pyrefly: ignore [bad-argument-type]
                os.symlink(self._hgcommand, tmphgpath)
                self._createdfiles.append(tmphgpath)
            except OSError as err:
                # child processes may race, which is harmless
                if err.errno != errno.EEXIST:
                    raise
        else:
            raise SystemExit("%s could not be put in search path" % self._hgcommand)

    def _installhg(self):
        """Install hg into the test environment.

        This will also configure hg with the appropriate testing settings.
        """
        vlog("# Performing temporary installation of HG")
        # pyrefly: ignore [no-matching-overload]
        installerrs = os.path.join(self._hgtmp, "install.err")
        compiler = ""
        # pyrefly: ignore [missing-attribute]
        if self.options.compiler:
            # pyrefly: ignore [unsupported-operation]
            compiler = "--compiler " + self.options.compiler

        # Run installer in hg root
        script = os.path.realpath(sys.argv[0])
        exe = PYTHON
        hgroot = os.path.dirname(os.path.dirname(script))
        self._hgroot = hgroot
        os.chdir(hgroot)
        nohome = b'--home=""'
        if os.name == "nt":
            # The --home="" trick works only on OS where os.sep == '/'
            # because of a distutils convert_path() fast-path. Avoid it at
            # least on Windows for now, deal with .pydistutils.cfg bugs
            # when they happen.
            nohome = b""
        cmd = (
            b"%(exe)s setup.py clean --all"
            b' build %(compiler)s --build-base="%(base)s"'
            b' install --force --prefix="%(prefix)s"'
            b' --install-lib="%(libdir)s"'
            b' --install-scripts="%(bindir)s" %(nohome)s >%(logfile)s 2>&1'
            % {
                b"exe": exe,
                b"compiler": compiler,
                # pyrefly: ignore [no-matching-overload]
                b"base": os.path.join(self._hgtmp, b"build"),
                b"prefix": self._installdir,
                b"libdir": self._pythondir,
                b"bindir": self._bindir,
                b"nohome": nohome,
                b"logfile": installerrs,
            }
        )

        # setuptools requires install directories to exist.
        def makedirs(p):
            try:
                os.makedirs(p)
            except OSError as e:
                if e.errno != errno.EEXIST:
                    raise

        makedirs(self._pythondir)
        makedirs(self._bindir)

        vlog("# Running", cmd)
        if os.system(cmd) == 0:
            # pyrefly: ignore [missing-attribute]
            if not self.options.verbose:
                try:
                    os.remove(installerrs)
                except OSError as e:
                    if e.errno != errno.ENOENT:
                        raise
        else:
            with open(installerrs, "rb") as f:
                for line in f:
                    sys.stdout.buffer.write(line)
            sys.exit(1)
        # pyrefly: ignore [bad-argument-type]
        os.chdir(self._testdir)

        self._usecorrectpython()

        # pyrefly: ignore [no-matching-overload]
        hgbat = os.path.join(self._bindir, "hg.bat")
        if os.path.isfile(hgbat):
            # hg.bat expects to be put in bin/scripts while run-tests.py
            # installation layout put it in bin/ directly. Fix it
            with open(hgbat, "rb") as f:
                data = f.read()
            if b'"%~dp0..\\python" "%~dp0hg" %*' in data:
                data = data.replace(
                    b'"%~dp0..\\python" "%~dp0hg" %*', b'"%~dp0python" "%~dp0hg" %*'
                )
                with open(hgbat, "wb") as f:
                    f.write(data)
            else:
                print("WARNING: cannot fix hg.bat reference to python.exe")

        # pyrefly: ignore [missing-attribute]
        if self.options.anycoverage:
            # pyrefly: ignore [no-matching-overload]
            custom = os.path.join(self._testdir, "sitecustomize.py")
            # pyrefly: ignore [no-matching-overload]
            target = os.path.join(self._pythondir, "sitecustomize.py")
            vlog("# Installing coverage trigger to %s" % target)
            shutil.copyfile(custom, target)
            # pyrefly: ignore [no-matching-overload]
            rc = os.path.join(self._testdir, ".coveragerc")
            vlog("# Installing coverage rc to %s" % rc)
            os.environ["COVERAGE_PROCESS_START"] = rc
            # pyrefly: ignore [no-matching-overload]
            covdir = os.path.join(self._installdir, "..", "coverage")
            try:
                os.mkdir(covdir)
            except OSError as e:
                if e.errno != errno.EEXIST:
                    raise

            os.environ["COVERAGE_DIR"] = covdir

    def _checkhglib(self, verb):
        """Ensure that the 'mercurial' package imported by python is
        the one we expect it to be.  If not, print a warning to stderr."""
        if (self._bindir == self._pythondir) and (self._bindir != self._tmpbindir):
            # The pythondir has been inferred from --with-hg flag.
            # We cannot expect anything sensible here.
            return
        # pyrefly: ignore [no-matching-overload]
        expecthg = os.path.join(self._pythondir, "sapling")
        actualhg = self._gethgpath()
        if os.path.abspath(actualhg) != os.path.abspath(expecthg):
            sys.stderr.write(
                "warning: %s with unexpected mercurial lib: %s\n"
                "         (expected %s)\n" % (verb, actualhg, expecthg)
            )

    def _gethgpath(self):
        """Return the path to the mercurial package that is actually found by
        the current Python interpreter."""
        if self._hgpath is not None:
            return self._hgpath

        cmd = '%s -c "from sapling import mercurial; print (mercurial.__path__[0])"'
        cmd = cmd % PYTHON
        pipe = os.popen(cmd)
        try:
            self._hgpath = pipe.read().strip()
        finally:
            pipe.close()

        return self._hgpath

    def _outputcoverage(self):
        """Produce code coverage output."""
        # pyrefly: ignore [missing-import]
        import coverage

        coverage = coverage.coverage

        vlog("# Producing coverage report")
        # chdir is the easiest way to get short, relative paths in the
        # output.
        # pyrefly: ignore [bad-argument-type]
        os.chdir(self._hgroot)
        # pyrefly: ignore [no-matching-overload]
        covdir = os.path.join(self._installdir, "..", "coverage")
        cov = coverage(data_file=os.path.join(covdir, "cov"))

        # Map install directory paths back to source directory.
        cov.config.paths["srcdir"] = [".", self._pythondir]

        cov.combine()

        # pyrefly: ignore [no-matching-overload]
        omit = [os.path.join(x, "*") for x in [self._bindir, self._testdir]]
        cov.report(ignore_errors=True, omit=omit)

        # pyrefly: ignore [missing-attribute]
        if self.options.htmlcov:
            # pyrefly: ignore [no-matching-overload]
            htmldir = os.path.join(self._outputdir, "htmlcov")
            cov.html_report(directory=htmldir, omit=omit)
        # pyrefly: ignore [missing-attribute]
        if self.options.annotate:
            # pyrefly: ignore [no-matching-overload]
            adir = os.path.join(self._outputdir, "annotated")
            if not os.path.isdir(adir):
                os.mkdir(adir)
            cov.annotate(directory=adir, omit=omit)

    def _findprogram(self, program):
        """Search PATH for a executable program"""
        for p in os.environ.get("PATH", os.defpath).split(os.pathsep):
            name = os.path.join(p, program)
            if os.name == "nt" or os.access(name, os.X_OK):
                return name
        return None

    def _checktools(self):
        """Ensure tools required to run tests are present."""
        for p in self.REQUIREDTOOLS:
            if os.name == "nt" and not p.endswith(".exe"):
                p += ".exe"
            found = self._findprogram(p)
            if found:
                vlog("# Found prerequisite", p, "at", found)
            else:
                print("WARNING: Did not find prerequisite tool: %s " % p)


def aggregateexceptions(path):
    exceptions = collections.Counter()

    for f in os.listdir(path):
        with open(os.path.join(path, f), "rb") as fh:
            data = fh.read().split(b"\0")
            if len(data) != 4:
                continue

            exc, mainframe, hgframe, hgline = data
            exc = exc.decode("utf-8")
            mainframe = mainframe.decode("utf-8")
            hgframe = hgframe.decode("utf-8")
            hgline = hgline.decode("utf-8")
            exceptions[(hgframe, hgline, exc)] += 1

    return exceptions


def ensureenv():
    """Load build/env's environment variables.

    If build/env has specified a different set of environment variables,
    restart the current command. Otherwise do nothing.
    """
    hgdir = os.path.dirname(os.path.dirname(os.path.realpath(__file__)))
    envpath = os.path.join(hgdir, "build", "env")
    skipensureenv = "HGRUNTEST_SKIP_ENV" in os.environ
    iswindows = os.name == "nt"
    if (skipensureenv and not iswindows) or not os.path.exists(envpath):
        return
    with open(envpath, "r") as f:
        env = dict(l.split("=", 1) for l in f.read().splitlines() if "=" in l)

    # A minority of our old non-debugruntest tests still rely on being able to
    # run bash commands plus having commands like `less` available. On our CI
    # this is not an issue, since those are already part of our path there.
    # However, locally we usually don't have those on our path so let's try
    # to get them from the list of env bars in ../build/env and relaunch this
    # with that appended to our path. Note that in this case we cannot simply
    # keep loading all the environment variables from `build/env`, since running
    # run-tests.py as a Buck target generates a .par file. When this
    # run-tests.py is built and run as a par, the par logic will modify PATH and
    # other library and Python-related environment variables at startup.
    # Changing the Python-related and library ones can prevent the .par from
    # properly running in the first place, while the PATH one can make it loop.
    # The modified PATH will trick ensureenv to relaunch with a different PATH,
    # and enter an infinite loop of (par modifies PATH, ensureenv modifies PATH
    # and relaunch). To avoid the loop we use a separate env var to tell
    # ensureenv to skip.
    if skipensureenv and iswindows:
        esep = ";"
        ppath = os.path.join("fbcode", "eden", "scm", "build", "bin")
        k = "PATH"
        origpath = os.environ[k]
        newpath = esep.join([p for p in env[k].split(esep) if ppath in p])
        if newpath and newpath not in origpath:
            newpath += esep + origpath
        else:
            newpath = origpath
        env = {k: newpath}

    if all(os.environ.get(k) == v for k, v in env.items()):
        # No restart needed
        return
    # Restart with new environment
    newenv = os.environ.copy()
    newenv.update(env)
    # Pick the right Python interpreter
    python = env.get("PYTHON_SYS_EXECUTABLE", PYTHON)
    p = subprocess.Popen([python] + sys.argv, env=newenv)
    sys.exit(p.wait())


def os_times():
    times = os.times()
    if os.name == "nt":
        # patch times[4] (elapsed, 0 on Windows) to be the wall clock time
        times = list(times)
        times[4] = time.monotonic()
        times = tuple(times)
    return times


def main() -> None:
    ensureenv()
    runner = TestRunner()

    try:
        import msvcrt

        # pyre-fixme[16]: Module `msvcrt` has no attribute `setmode`.
        msvcrt.setmode(sys.stdin.fileno(), os.O_BINARY)
        # pyre-fixme[16]: Module `msvcrt` has no attribute `setmode`.
        msvcrt.setmode(sys.stdout.fileno(), os.O_BINARY)
        # pyre-fixme[16]: Module `msvcrt` has no attribute `setmode`.
        msvcrt.setmode(sys.stderr.fileno(), os.O_BINARY)
    except ImportError:
        pass

    sys.exit(runner.run(sys.argv[1:]))


if __name__ == "__main__":
    main()  # pragma: no cover
