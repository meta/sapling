#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import json
import os
import sys
import threading
import time
import unittest
from pathlib import Path
from typing import Dict, List, Optional

from eden.fs.cli.util import INTENTIONALLY_UNMOUNTED, poll_until
from eden.fs.service.eden.thrift_types import GetBlockedFaultsRequest, UnblockFaultArg
from parameterized import parameterized

from .lib import testcase


# Recovery runs often enough that a few seconds cover several attempts.
RECOVERY_INTERVAL = "1s"
SETTLE_SECONDS = 3


@testcase.eden_repo_test(run_on_nfs=False, run_coroutines=False, run_io_uring=False)
@unittest.skipIf(sys.platform != "linux", "exercised on Linux FUSE mounts")
class UnmountRecoveryTest(testcase.EdenRepoTest):
    git_test_supported = False
    enable_fault_injection = True

    def populate_repo(self) -> None:
        self.repo.write_file("hello", "hola\n")
        self.repo.commit("Initial commit.")

    def edenfs_extra_config(self) -> Optional[Dict[str, List[str]]]:
        configs = dict(super().edenfs_extra_config() or {})
        configs["mount"] = list(configs.get("mount", [])) + [
            f'accidental-unmount-recovery-interval = "{RECOVERY_INTERVAL}"'
        ]
        return configs

    def mount_state(self, mount: Optional[str] = None) -> Optional[str]:
        return self.eden.list_cmd_simple().get(mount or self.mount)

    def wait_until_running(self, mount: Optional[str] = None) -> None:
        poll_until(
            lambda: True if self.mount_state(mount) == "RUNNING" else None,
            timeout=30,
        )

    def intentional_unmount_marker(self) -> Path:
        client_dir = self.eden.client_dir_for_mount(Path(self.mount))
        return client_dir / INTENTIONALLY_UNMOUNTED

    def assert_stays_unmounted(self, mount: Optional[str] = None) -> None:
        time.sleep(SETTLE_SECONDS)
        self.assertEqual("NOT_RUNNING", self.mount_state(mount))

    def recovery_counter(self, name: str) -> float:
        return self.get_counters().get(f"checkout_recovery.{name}.sum", 0)

    def test_unintentional_unmount_is_remounted(self) -> None:
        self.eden.run_cmd("unmount", "--debug-mark-unintentional", self.mount)
        self.wait_until_running()
        # The counters are bumped once mount() completes, which can be just
        # after the mount is listed as RUNNING.
        poll_until(
            lambda: True if self.recovery_counter("success") == 1 else None,
            timeout=30,
        )
        self.assertEqual(1, self.recovery_counter("attempt"))

    def test_eden_unmount_is_not_remounted(self) -> None:
        self.eden.unmount(Path(self.mount))
        self.assert_stays_unmounted()

    def test_marker_is_written_before_the_unmount_finishes(self) -> None:
        marker = self.intentional_unmount_marker()
        with self.run_with_blocking_fault(keyClass="unmount"):
            unmount = threading.Thread(
                target=self.eden.unmount, args=(Path(self.mount),)
            )
            unmount.start()
            self.wait_on_fault_hit("unmount")
            self.assertTrue(marker.exists())
            self.assertEqual("RUNNING", self.mount_state())
        unmount.join(timeout=60)
        self.assert_stays_unmounted()

    @parameterized.expand(
        [
            ("rust", {"EDENFSCTL_ONLY_RUST": "1"}),
            ("python", {"EDENFSCTL_SKIP_RUST": "1"}),
        ]
    )
    def test_eden_rm_leaves_no_mount(self, impl: str, env: Dict[str, str]) -> None:
        self.eden.run_cmd("remove", "--yes", self.mount, env=env)
        time.sleep(SETTLE_SECONDS)
        self.assertNotIn(self.mount, self.eden.list_cmd_simple())
        self.assertFalse(os.path.exists(self.mount))

    def test_broken_checkout_does_not_block_recovery_of_others(self) -> None:
        config_json = self.eden.eden_dir / "config.json"
        original = config_json.read_text()
        self.addCleanup(config_json.write_text, original)
        directory_map = json.loads(original)
        for i in range(3):
            directory_map[f"/nonexistent/checkout-{i}"] = f"missing-client-{i}"
        config_json.write_text(json.dumps(directory_map))

        self.eden.run_cmd("unmount", "--debug-mark-unintentional", self.mount)
        self.wait_until_running()

    def test_recovery_gives_up_after_three_failed_attempts(self) -> None:
        with self.run_with_blocking_fault(keyClass="mount"):
            self.eden.run_cmd("unmount", "--debug-mark-unintentional", self.mount)
            for _ in range(3):
                self.wait_on_fault_hit("mount")
                with self.eden.get_thrift_client() as client:
                    client.unblockFault(
                        UnblockFaultArg(
                            keyClass="mount",
                            errorType="runtime_error",
                            errorMessage="injected mount failure",
                        )
                    )
            # Later recovery runs make no fourth attempt.
            time.sleep(SETTLE_SECONDS)
            with self.eden.get_thrift_client() as client:
                blocked = client.getBlockedFaults(
                    GetBlockedFaultsRequest(keyclass="mount")
                ).keyValues
            self.assertEqual([], list(blocked))
        self.assertEqual("NOT_RUNNING", self.mount_state())
        self.assertEqual(3, self.recovery_counter("failure"))
        self.assertEqual(1, self.recovery_counter("gave_up"))
