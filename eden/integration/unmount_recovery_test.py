#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import sys
import time
import unittest
from pathlib import Path
from typing import Dict, List, Optional

from eden.fs.cli.util import poll_until

from .lib import testcase


# Recovery runs often enough that a few seconds cover several attempts.
RECOVERY_INTERVAL = "1s"
SETTLE_SECONDS = 3


@testcase.eden_repo_test(run_on_nfs=False, run_coroutines=False, run_io_uring=False)
@unittest.skipIf(sys.platform != "linux", "exercised on Linux FUSE mounts")
class UnmountRecoveryTest(testcase.EdenRepoTest):
    git_test_supported = False

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

    def assert_stays_unmounted(self, mount: Optional[str] = None) -> None:
        time.sleep(SETTLE_SECONDS)
        self.assertEqual("NOT_RUNNING", self.mount_state(mount))

    def test_unintentional_unmount_is_remounted(self) -> None:
        self.eden.run_cmd("unmount", "--debug-mark-unintentional", self.mount)
        self.wait_until_running()

    def test_eden_unmount_is_not_remounted(self) -> None:
        self.eden.unmount(Path(self.mount))
        self.assert_stays_unmounted()
