#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import unittest

from ..top import format_duration, Process
from ..util import format_cmd, format_mount


class TopTest(unittest.TestCase):
    def setUp(self) -> None:
        self.process = Process(42, b"ls", b"fbsource")

    def test_format_cmd(self) -> None:
        self.assertEqual("ls", format_cmd(b"/bin/ls"))
        self.assertEqual("'chg[worker/0]'", format_cmd(b"chg[worker/0]"))

    def test_format_cmd_with_arg(self) -> None:
        self.assertEqual("ls -l", format_cmd(b"/bin/ls\x00-l"), "ls -l")
        self.assertEqual("ls -l 'one two'", format_cmd(b"ls\0-l\0one two"))

    def test_format_cmd_trailing_null(self) -> None:
        self.assertEqual("ls -l", format_cmd(b"ls\x00-l\x00"), "ls -l")
        self.assertEqual("ls -l ''", format_cmd(b"ls\x00-l\x00\x00"), "ls -l ''")

    def test_format_mount(self) -> None:
        self.assertEqual(format_mount(b"/data/users/zuck/fbsource"), "fbsource")

    def test_format_duration(self) -> None:
        self.assertEqual(format_duration(1), "1ns")
        self.assertEqual(format_duration(1 * 1000), "1us")
        self.assertEqual(format_duration(1 * 1000 * 1000), "1ms")
        self.assertEqual(format_duration(1 * 1000 * 1000 * 1000), "1s")
        self.assertEqual(format_duration(1 * 1000 * 1000 * 1000 * 60), "1m")
        self.assertEqual(format_duration(1 * 1000 * 1000 * 1000 * 60 * 60), "1h")
        self.assertEqual(format_duration(1 * 1000 * 1000 * 1000 * 60 * 60 * 24), "1d")


class ProcessAggregationTest(unittest.TestCase):
    def test_aggregate_running_process_with_older_access(self) -> None:
        exited = Process(100, b"hg status", b"/data/users/alice/fbsource")
        exited.last_access = 1000.0
        exited.is_running = False
        exited.set_fetches(3)
        running = Process(200, b"hg status", b"/data/users/alice/fbsource")
        running.last_access = 900.0
        running.is_running = True
        running.set_fetches(5)

        exited.aggregate(running)

        self.assertEqual(exited.pid, 200)
        self.assertTrue(exited.is_running)
        self.assertEqual(exited.last_access, 1000.0)
        self.assertEqual(exited.fuseFetch, 8)
