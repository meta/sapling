#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import os
import re
import sys
import unittest
from typing import Dict, List, Optional

from eden.fs.cli.util import poll_until
from eden.fs.service.eden.thrift_types import FaultDefinition

from .lib import testcase


class MountHealthCheckTestBase(testcase.EdenRepoTest):
    git_test_supported = False
    enable_fault_injection = True

    def populate_repo(self) -> None:
        self.repo.write_file("hello", "hola\n")
        self.repo.commit("Initial commit.")

    def mount_config(self) -> List[str]:
        return ['mount-health-check-interval = "1s"']

    def edenfs_extra_config(self) -> Optional[Dict[str, List[str]]]:
        configs = dict(super().edenfs_extra_config() or {})
        configs["mount"] = list(configs.get("mount", [])) + self.mount_config()
        return configs

    def counter(self, name: str) -> float:
        return self.get_counters().get(f"mount_health_check.{name}.sum", 0)

    def wait_for_counter(self, name: str) -> None:
        poll_until(lambda: True if self.counter(name) > 0 else None, timeout=30)

    def delay_next_probe(self, seconds: float, mount: Optional[str] = None) -> None:
        with self.eden.get_thrift_client() as client:
            client.injectFault(
                FaultDefinition(
                    keyClass="mountHealthCheck",
                    keyValueRegex=re.escape(mount) if mount else ".*",
                    delayMilliseconds=int(seconds * 1000),
                    count=1,
                )
            )


@testcase.eden_repo_test(run_on_nfs=False, run_coroutines=False, run_io_uring=False)
@unittest.skipIf(sys.platform != "linux", "exercised on Linux FUSE mounts")
class MountHealthCheckTest(MountHealthCheckTestBase):
    def test_healthy_mount_is_counted(self) -> None:
        self.wait_for_counter("healthy")
        self.assertGreater(self.counter("probes"), 0)
        self.assertEqual(0, self.counter("issue"))
        self.assertEqual(0, self.counter("timed_out"))

    def test_slow_probe_is_counted_as_timed_out(self) -> None:
        self.wait_for_counter("healthy")
        self.delay_next_probe(7)
        self.wait_for_counter("timed_out")
        self.assertEqual(0, self.counter("not_started"))

    def test_probe_waiting_for_a_thread_is_counted_as_not_started(self) -> None:
        stuck = [self.mount, os.path.join(self.mounts_dir, "second")]
        self.eden.clone(self.repo.path, stuck[1])
        self.eden.clone(self.repo.path, os.path.join(self.mounts_dir, "third"))
        self.wait_for_counter("healthy")
        # Two stuck probes hold both health check threads, so the third
        # checkout's probe cannot start.
        for mount in stuck:
            self.delay_next_probe(8, mount)
        self.wait_for_counter("not_started")


@testcase.eden_repo_test(run_on_nfs=False, run_coroutines=False, run_io_uring=False)
@unittest.skipIf(sys.platform != "linux", "exercised on Linux FUSE mounts")
class StatxMountHealthCheckTest(MountHealthCheckTestBase):
    def mount_config(self) -> List[str]:
        return super().mount_config() + ["mount-health-check-statx = true"]

    def test_mount_is_confirmed_with_statx(self) -> None:
        # The device is recorded at mount time, so even the first probe skips
        # the mount table.
        self.wait_for_counter("healthy")
        healthy = self.counter("healthy")
        self.assertGreaterEqual(self.counter("statx_mount_check"), healthy)
        self.assertEqual(0, self.counter("issue"))


@testcase.eden_repo_test(run_on_nfs=False, run_coroutines=False, run_io_uring=False)
@unittest.skipIf(sys.platform != "linux", "exercised on Linux FUSE mounts")
class HangThresholdMountHealthCheckTest(MountHealthCheckTestBase):
    def mount_config(self) -> List[str]:
        return super().mount_config() + ['mount-health-check-hang-threshold = "6s"']

    def test_probe_past_the_threshold_is_counted_as_hung(self) -> None:
        self.wait_for_counter("healthy")
        self.delay_next_probe(9)
        self.wait_for_counter("hung")
        self.assertEqual(0, self.counter("timed_out"))
