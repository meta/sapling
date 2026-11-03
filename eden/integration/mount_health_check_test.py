#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import sys
import unittest
from typing import Dict, List, Optional

from eden.fs.cli.util import poll_until

from .lib import testcase


@testcase.eden_repo_test(run_on_nfs=False, run_coroutines=False, run_io_uring=False)
@unittest.skipIf(sys.platform != "linux", "exercised on Linux FUSE mounts")
class MountHealthCheckTest(testcase.EdenRepoTest):
    git_test_supported = False

    def populate_repo(self) -> None:
        self.repo.write_file("hello", "hola\n")
        self.repo.commit("Initial commit.")

    def edenfs_extra_config(self) -> Optional[Dict[str, List[str]]]:
        configs = dict(super().edenfs_extra_config() or {})
        configs["mount"] = list(configs.get("mount", [])) + [
            'mount-health-check-interval = "1s"'
        ]
        return configs

    def counter(self, name: str) -> float:
        return self.get_counters().get(f"mount_health_check.{name}.sum", 0)

    def wait_for_counter(self, name: str) -> None:
        poll_until(lambda: True if self.counter(name) > 0 else None, timeout=30)

    def test_healthy_mount_is_counted(self) -> None:
        self.wait_for_counter("healthy")
        self.assertGreater(self.counter("probes"), 0)
        self.assertEqual(0, self.counter("issue"))
        self.assertEqual(0, self.counter("timed_out"))
