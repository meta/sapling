# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

# Translate run-tests.py tests to Python standard unittests

import contextlib
import glob
import os
import random
import re
import shlex
import shutil
import subprocess
import sys
import unittest
from typing import Callable, Dict, Generator, List, Optional, Tuple


hgpath: Optional[str] = os.environ.get("HGTEST_HG")
pythonbinpath: str = os.environ.get("HGTEST_PYTHON", "python3")
watchman: Optional[str] = os.environ.get("HGTEST_WATCHMAN")
mononoke_server: Optional[str] = os.environ.get("HGTEST_MONONOKE_SERVER")
dummyssh: Optional[str] = os.environ.get("HGTEST_DUMMYSSH")
get_free_socket: Optional[str] = os.environ.get("HGTEST_GET_FREE_SOCKET")
# We want to make the one below a hard requirement for running tests
run_tests_py: str = os.environ["HGTEST_RUN_TESTS_PY"]


if watchman is not None and not os.path.exists(str(watchman)):
    watchman = None

try:
    shlex_quote: Callable[[str], str] = shlex.quote  # Python 3.3 and up
except AttributeError:
    # pyre-fixme[9]: shlex_quote has type `(s: str) -> str`; used as `(seq:
    #  Sequence[str]) -> str`.
    # pyre-fixme[9]: shlex_quote has type `(s: str) -> str`; used as `(seq:
    #  Sequence[str]) -> str`.
    shlex_quote = subprocess.list2cmdline


@contextlib.contextmanager
def chdir(path: str) -> Generator[None, None, None]:
    oldpwd = os.getcwd()
    try:
        os.chdir(path)
        yield
    finally:
        os.chdir(oldpwd)


def prepareargsenv(
    runtestsdir: str, port: Optional[int] = None
) -> Tuple[List[str], Dict[str, str]]:
    """return (args, env) for running run-tests.py"""
    env = os.environ.copy()
    # We need Python to run .par files on Windows
    args = ([pythonbinpath] if os.name == "nt" else []) + [
        run_tests_py,
        "--maxdifflines=1000",
    ]
    if port:
        args += ["--port", "%s" % port]

    global hgpath

    if hgpath is None:
        hgpath = shutil.which("hg.real")
        # Make sure to keep these in sync with targets.bzl
        # pyrefly: ignore [unsupported-operation]
        env["HGEXECUTABLEPATH"] = hgpath
        # pyrefly: ignore [unsupported-operation]
        env["HGTEST_HG"] = hgpath
        # pyrefly: ignore [unsupported-operation]
        env["HG_REAL_BIN"] = hgpath
    args.append("--with-hg=%s" % hgpath)
    if watchman:
        args += ["--with-watchman", watchman]
    # set HGDATAPATH
    datapath = os.path.join(runtestsdir, "../sapling")
    env["HGDATAPATH"] = datapath
    env["PYTHONPATH"] = os.path.join(runtestsdir, "../")
    # set HGPYTHONPATH since PYTHONPATH might be discarded
    pythonpath = os.pathsep.join([runtestsdir])
    env["HGPYTHONPATH"] = pythonpath
    # set other environments useful for buck testing
    env["HGTEST_NORMAL_LAYOUT"] = "0"
    if dummyssh is not None:
        env["DUMMYSSH"] = dummyssh

    # Variables needed for mononoke integration
    if os.environ.get("USE_MONONOKE"):
        # pyrefly: ignore [unsupported-operation]
        env["MONONOKE_SERVER"] = mononoke_server
        # pyrefly: ignore [unsupported-operation]
        env["GET_FREE_SOCKET"] = get_free_socket

    return args, env


def gettestmethod(name: str, port: int) -> Callable[["hgtests"], None]:
    def runsingletest(self: "hgtests") -> None:
        sys.tracebacklimit = 1000  # Unhide stacktraces.
        reportskips = os.getenv("HGTEST_REPORT_SKIPS")
        with chdir(self._runtests_dir):
            args, env = prepareargsenv(self._runtests_dir, port)
            args.append("--temporary-error-output")
            args += os.getenv("HGTEST_RUNTESTS_ARGS", "").split()
            # run run-tests.py for a single test
            p = subprocess.Popen(
                args + [name], env=env, stderr=subprocess.PIPE, stdout=subprocess.PIPE
            )
            # pyrefly: ignore [bad-argument-type]
            out, err = p.communicate("")
            message = err + out
            returncode = p.returncode
            if b"Lost connection to MySQL server" in message:
                raise unittest.SkipTest("MySQL is unavailable")
            if returncode == 80:
                if not reportskips:
                    return
                # Extract skipped reason from output
                match = re.search(b"Skipped [^:]*: (.*)", message)
                if match:
                    reason = match.group(1)
                else:
                    reason = b"skipped by run-tests.py"
                # pyrefly: ignore [bad-argument-type]
                raise unittest.SkipTest(reason)
            elif returncode != 0:
                decoded_message = message.decode("utf-8", errors="surrogateescape")
                sys.tracebacklimit = 0  # Hide stacktraces.
                raise self.failureException(decoded_message)

    return runsingletest


class hgtests(unittest.TestCase):
    _runtests_dir: str = ""

    @classmethod
    def collecttests(cls, path: str) -> None:
        """scan tests in path and add them as test methods"""
        if os.environ.get("HGTEST_IGNORE_INCLUDED") == "1":
            included = None
        else:
            included = re.compile(r"\A%s\Z" % os.environ.get("HGTEST_INCLUDED", ".*"))

        if os.environ.get("HGTEST_IGNORE_EXCLUDED") == "1":
            excluded = None
        else:
            excluded = re.compile(r"\A%s\Z" % os.environ.get("HGTEST_EXCLUDED", ""))

        blocklist_env = os.environ.get("HGTEST_BLOCKLIST", None)
        testdir_env = os.environ.get("HGTEST_DIR", None)
        blocklist = set()
        if blocklist_env and testdir_env:
            blocklist_env = os.path.join(testdir_env, blocklist_env)
            with open(blocklist_env, "r") as f:
                for line in f:
                    line = line.strip()
                    if len(line) > 0 and line[0] != "#":
                        blocklist.add(line)

        # Randomize the port so a stress run of a single test would be fine
        port = random.randint(10000, 60000)
        with chdir(path):
            cls._runtests_dir = os.getcwd()
            for name in glob.glob("test-*.t") + glob.glob("test-*.py"):
                method_name = name.replace(".", "_").replace("-", "_")
                if included and not included.match(method_name):
                    continue
                if excluded and excluded.match(method_name):
                    continue
                if name in blocklist:
                    continue
                # Running a lot run-tests.py in parallel will trigger race
                # condition of the original port detection logic. So allocate
                # ports here. run-tests.py could do adjustments.
                # A test needs 3 ports at most. See portneeded in run-tests.py
                port += 3
                setattr(cls, method_name, gettestmethod(name, port))


def main() -> None:
    args, env = prepareargsenv(os.getcwd())
    os.execvpe(args[0], args + sys.argv[1:], env)


if __name__ == "__main__":
    main()
else:
    hgtests.collecttests(os.environ.get("HGTEST_DIR", "."))
