#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

from __future__ import annotations

import errno
import os
import select
import stat
import sys
import unittest

from .lib import hgrepo, testcase


@testcase.eden_repo_test
@unittest.skipUnless(sys.platform == "linux", "FIFOs are supported on Linux")
class FifoTest(testcase.EdenRepoTest):
    def populate_repo(self) -> None:
        self.repo.write_file("hello", "hola\n")
        self.repo.commit("Initial commit.")

    def assert_fifo_io(self, path: str) -> None:
        self.assertTrue(stat.S_ISFIFO(os.lstat(path).st_mode))
        with self.assertRaises(OSError) as error:
            os.open(path, os.O_WRONLY | os.O_NONBLOCK)
        self.assertEqual(errno.ENXIO, error.exception.errno)

        with os.fdopen(os.open(path, os.O_RDONLY | os.O_NONBLOCK), "rb") as reader:
            with os.fdopen(os.open(path, os.O_WRONLY | os.O_NONBLOCK), "wb") as writer:
                with self.assertRaises(BlockingIOError) as error:
                    os.read(reader.fileno(), 1)
                self.assertEqual(errno.EAGAIN, error.exception.errno)

                payload = b"fifo payload\x00\n"
                self.assertEqual(len(payload), os.write(writer.fileno(), payload))
                readable, _, _ = select.select([reader], [], [], 2)
                self.assertEqual([reader], readable)
                self.assertEqual(payload, os.read(reader.fileno(), len(payload)))

            self.assertEqual(b"", os.read(reader.fileno(), 1))

    def test_fifo_io(self) -> None:
        path = os.path.join(self.mount, "pipe")
        os.mkfifo(path, 0o600)
        self.assertEqual(stat.S_IFIFO | 0o600, os.lstat(path).st_mode)
        self.assert_fifo_io(path)

    def test_fifo_persists_across_restart(self) -> None:
        path = os.path.join(self.mount, "pipe")
        renamed = os.path.join(self.mount, "renamed")
        os.mkfifo(path, 0o600)
        os.chmod(path, 0o640)
        os.rename(path, renamed)

        self.eden.restart()

        self.assertIn("renamed", os.listdir(self.mount))
        self.assertFalse(os.path.lexists(path))
        self.assertEqual(stat.S_IFIFO | 0o640, os.lstat(renamed).st_mode)
        self.assert_fifo_io(renamed)
        os.unlink(renamed)
        self.assertFalse(os.path.lexists(renamed))

    def test_fifo_status(self) -> None:
        os.mkfifo(os.path.join(self.mount, "pipe"), 0o600)
        tracked_path = os.path.join(self.mount, "hello")
        os.unlink(tracked_path)
        os.mkfifo(tracked_path, 0o600)

        assert isinstance(self.eden_repo, hgrepo.HgRepository)
        self.assertEqual({"hello": "M", "pipe": "?"}, self.eden_repo.status())

    def test_disable_fifo_creation(self) -> None:
        path = os.path.join(self.mount, "existing")
        os.mkfifo(path, 0o600)
        self.eden.user_rc_path.write_text("[core]\nenable-fifo = false\n")
        self.eden.restart()

        with self.assertRaises(OSError) as error:
            os.mkfifo(os.path.join(self.mount, "disabled"), 0o600)
        self.assertEqual(errno.EPERM, error.exception.errno)
        self.assert_fifo_io(path)
