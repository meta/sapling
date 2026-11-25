# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""test line matching with some failing examples and some which warn

run-test.t only checks positive matches and can not see warnings
(both by design)
"""

from __future__ import absolute_import, print_function

import doctest
import os
import re
import shutil
import subprocess
import tempfile
import unittest

import silenttestrunner


# this is hack to make sure no escape characters are inserted into the output
if "TERM" in os.environ:
    del os.environ["TERM"]

os.environ.pop("HGTEST_USE_EDEN", None)

run_tests = __import__("run-tests")


def prn(ex):
    m = ex.args[0]
    if isinstance(m, str):
        print(m)
    else:
        print(m.decode("utf-8"))


def lm(expected, output):
    r"""check if output matches expected

    does it generally work?
        >>> lm(b'H*e (glob)\n', b'Here\n')
        True

    fail on bad test data
        >>> try: lm(b'a\n',b'a')
        ... except AssertionError as ex: print(ex)
        missing newline
        >>> try: lm(b'single backslash\n', b'single \backslash\n')
        ... except AssertionError as ex: prn(ex)
        single backslash or unknown char
    """
    assert expected.endswith(b"\n") and output.endswith(b"\n"), "missing newline"
    assert not re.search(rb"[^ \w\\/\r\n()*?]", expected + output), (
        b"single backslash or unknown char"
    )
    test = run_tests.TTest("test-run-test.t", ".", ".")
    match = test.linematch(expected, output)
    if isinstance(match, str):
        return "special: " + match
    elif isinstance(match, bytes):
        return "special: " + match.decode("utf-8")
    else:
        return bool(match)  # do not return match object


def wintests():
    r"""test matching like running on windows

    enable windows matching on any os
        >>> _osaltsep = os.altsep
        >>> os.altsep = True
        >>> _osname = os.name
        >>> os.name = 'nt'

    valid match on windows
        >>> lm(b'g/a*/d (glob)\n', b'g\\abc/d\n')
        True

    direct matching, glob unnecessary
        >>> lm(b'g/b (glob)\n', b'g/b\n')
        True

    missing glob
        >>> lm(b'/g/c/d/fg\n', b'\\g\\c\\d/fg\n')
        True
        >>> lm(b'/g/c/d/fg\n', b'\\g\\c\\d\\fg\r\n')
        True

    restore os.altsep
        >>> os.altsep = _osaltsep
        >>> os.name = _osname
    """
    pass


def otherostests():
    r"""test matching like running on non-windows os

    disable windows matching on any os
        >>> _osaltsep = os.altsep
        >>> os.altsep = False
        >>> _osname = os.name
        >>> os.name = 'nt'

    backslash does not match slash
        >>> lm(b'h/a* (glob)\n', b'h\\ab\n')
        False

    direct matching glob can not be recognized
        >>> lm(b'h/b (glob)\n', b'h/b\n')
        True

    missing glob can not not be recognized
        >>> lm(b'/h/c/df/g/\n', b'\\h/c\\df/g\\\n')
        False

    restore os.altsep
        >>> os.altsep = _osaltsep
        >>> os.name = _osname
    """
    pass


class ErrorOutputTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = directory.name
        self.path = os.path.join(self.directory, "test-fail.t")
        with open(self.path, "wb") as f:
            f.write(b"  $ echo actual\n  expected\n")
        self.hg = (
            os.environ.get("HGEXECUTABLEPATH")
            or os.environ.get("HGTEST_HG")
            or shutil.which("sl")
        )
        if not self.hg:
            self.skipTest("requires a Sapling executable")
        self.env = os.environ.copy()
        for name in ("HGTEST_USE_EDEN", "USE_MONONOKE", "HGTEST_REPORT_SKIPS"):
            self.env.pop(name, None)

    def command(self, *args):
        return [
            self.hg,
            "debugpython",
            "--",
            os.path.abspath(run_tests.__file__),
            "--with-hg",
            self.hg,
            "--nofeatures",
            "--color=never",
            "-j1",
            *args,
            self.path,
        ]

    def assertfailure(self, returncode, output):
        self.assertEqual(returncode, 1, output)
        self.assertIn(b"-  expected", output)
        self.assertIn(b"+  actual", output)
        self.assertNotIn(b"Traceback", output)

    def runrunner(self, *args):
        result = subprocess.run(
            self.command(*args),
            cwd=self.directory,
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=90,
        )
        self.assertfailure(result.returncode, result.stdout)
        return result.stdout

    def test_temporary_error_output_preserves_failure_diffs(self):
        errpath = self.path + ".err"
        with open(errpath, "wb") as f:
            f.write(b"previous failure")
        processes = [
            subprocess.Popen(
                self.command(
                    "--temporary-error-output",
                    "--verbose",
                    "--tmpdir",
                    os.path.join(self.directory, name),
                ),
                cwd=self.directory,
                env=self.env,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
            )
            for name in ("one", "two")
        ]
        try:
            outputs = [p.communicate(timeout=90)[0] for p in processes]
        finally:
            for p in processes:
                if p.poll() is None:
                    p.kill()
                p.wait()
        paths = []
        for process, output in zip(processes, outputs):
            self.assertfailure(process.returncode, output)
            match = re.search(rb"Keeping threadtmp dir: ([^\r\n]+)", output)
            self.assertIsNotNone(match, output)
            private = os.path.join(
                os.fsdecode(match.group(1).strip()), "test-fail.t.err"
            )
            paths.append(private)
            with open(private, "rb") as f:
                self.assertEqual(f.read(), b"  $ echo actual\n  actual\n")
        self.assertNotEqual(paths[0], paths[1])
        self.runrunner("--temporary-error-output")
        with open(errpath, "rb") as f:
            self.assertEqual(f.read(), b"previous failure")

    def test_local_error_output_supports_retest_and_restart(self):
        self.assertFalse(os.path.exists(self.path + ".err"))
        self.runrunner("--temporary-error-output", "--retry=1")
        self.runrunner()
        for option in ("--retest", "--restart"):
            self.runrunner("--temporary-error-output", option)
            with open(self.path + ".err", "rb") as f:
                self.assertEqual(f.read(), b"  $ echo actual\n  actual\n")
        outputdir = os.path.join(self.directory, "output")
        os.mkdir(outputdir)
        self.runrunner("--temporary-error-output", "--outputdir", outputdir)
        with open(os.path.join(outputdir, "test-fail.t.err"), "rb") as f:
            self.assertEqual(f.read(), b"  $ echo actual\n  actual\n")


if __name__ == "__main__":
    doctest.testmod()
    silenttestrunner.main(__name__)
