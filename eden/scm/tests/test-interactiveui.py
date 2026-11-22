# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import os
import unittest

import silenttestrunner
from sapling import util
from sapling.ext import interactiveui


class fakestdin:
    """stands in for sys.stdin, backed by a real fd"""

    def __init__(self, fd):
        self._fd = fd

    def fileno(self):
        return self._fd


@unittest.skipIf(util.iswindows, "interactiveui does not support Windows")
class testgetchar(unittest.TestCase):
    def setUp(self):
        # a pipe is a real non-tty fd; a pty secondary is a real tty fd
        self.pipefd, writefd = os.pipe()
        self.addCleanup(os.close, self.pipefd)
        self.addCleanup(os.close, writefd)
        primary, self.ttyfd = os.openpty()
        self.addCleanup(os.close, self.ttyfd)
        self.addCleanup(os.close, primary)

    def _getchar(self, keys, fd=None):
        """run getchar() against `fd`, with the raw read faked to yield `keys`

        Returns the getchar() result and the fds the read was attempted on, so
        tests can assert the terminal is left alone on the non-tty path.
        """
        reads = []

        def readraw(fd):
            reads.append(fd)
            return keys

        result = interactiveui.getchar(
            stdin=fakestdin(self.ttyfd if fd is None else fd), readraw=readraw
        )
        return result, reads

    def testnottty(self):
        result, reads = self._getchar(b"j", fd=self.pipefd)
        self.assertIsNone(result)
        # the terminal must not be touched at all when stdin is not a tty
        self.assertEqual(reads, [])

    def testkeypress(self):
        result, reads = self._getchar(b"j")
        self.assertEqual(result, b"j")
        self.assertEqual(reads, [self.ttyfd])

    def testinterrupt(self):
        # ctrl-c and ctrl-d end the session rather than returning a keypress
        self.assertIsNone(self._getchar(b"\x03")[0])
        self.assertIsNone(self._getchar(b"\x04")[0])

    def testescapesequence(self):
        result, _reads = self._getchar(b"\x1b[A")
        self.assertEqual(result, interactiveui.viewframe.KEY_UP)
        self.assertEqual(
            interactiveui._splitkeypresses(result),
            [interactiveui.viewframe.KEY_UP],
        )

    def testescapesequencerun(self):
        # a single read can deliver several arrow keys plus a normal key
        result, _reads = self._getchar(b"\x1b[A\x1b[Dj")
        self.assertEqual(
            interactiveui._splitkeypresses(result),
            [
                interactiveui.viewframe.KEY_UP,
                interactiveui.viewframe.KEY_LEFT,
                b"j",
            ],
        )


class testrealread(unittest.TestCase):
    def testnottty(self):
        # the default readraw is never reached for a non-tty, so this exercises
        # the real production code path end to end
        readfd, writefd = os.pipe()
        try:
            self.assertIsNone(interactiveui.getchar(stdin=fakestdin(readfd)))
        finally:
            os.close(readfd)
            os.close(writefd)


if __name__ == "__main__":
    silenttestrunner.main(__name__)
