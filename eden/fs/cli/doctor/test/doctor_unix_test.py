#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import shutil
import sys
from typing import Optional
from unittest.mock import MagicMock, patch

import eden.fs.cli.doctor as doctor
from eden.fs.cli.doctor.test.lib.fake_eden_instance import FakeEdenInstance
from eden.fs.cli.doctor.test.lib.fake_fs_util import FakeFsUtil
from eden.fs.cli.doctor.test.lib.fake_network_checker import FakeNetworkChecker
from eden.fs.cli.doctor.test.lib.fake_vscode_extensions_checker import (
    getFakeVSCodeExtensionsChecker,
)
from eden.fs.cli.doctor.test.lib.testcase import DoctorTestBase
from eden.fs.cli.test.lib.output import TestOutput


def get_support_suggestion() -> str:
    if sys.platform == "darwin":
        return """<yellow>1 issue requires manual attention.<reset>
Collect an 'eden rage' and ask in the EdenFS macOS Users group if you need help fixing issues with EdenFS:
https://fb.workplace.com/groups/edenfsmacos
"""
    else:
        return """<yellow>1 issue requires manual attention.<reset>
Collect an 'eden rage' and ask in the EdenFS Users group if you need help fixing issues with EdenFS:
https://fb.workplace.com/groups/eden.users
"""


class DoctorUnixTest(DoctorTestBase):
    """Doctor tests relevant to only Unix-like platforms"""

    # The diffs for what is written to stdout can be large.
    maxDiff: Optional[int] = None

    @patch(
        "eden.fs.cli.doctor.test.lib.fake_eden_instance.FakeEdenInstance.check_privhelper_connection",
        return_value=False,
    )
    def test_privhelper_check_not_accessible(
        self, mock_check_privhelper_connection: MagicMock
    ) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        mount = instance.create_test_mount("path1").path
        dry_run = False
        out = TestOutput()
        exit_code = doctor.cure_what_ails_you(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            #  `FakeEdenInstance`.
            instance,
            dry_run,
            mount_table=instance.mount_table,
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            network_checker=FakeNetworkChecker(),
            out=out,
        )

        self.assertEqual(
            f"""\
<yellow>- Found problem:<reset>
The PrivHelper process is not accessible.
To restore the connection to the PrivHelper, run `eden restart`

Checking {mount}
{get_support_suggestion()}""",
            out.getvalue(),
        )
        self.assertEqual(1, exit_code)

    # remove_checkout_configuration silently fails on Windows, leading to a test
    # failure.
    def test_unconfigured_mount_without_state_dir_is_reported(self) -> None:
        # If EdenFS advertises that a mount is active, but it is not in the
        # configuration and its state directory is gone, report it with the
        # removal remediation and don't throw an exception.
        instance = FakeEdenInstance(self.make_temporary_directory())
        edenfs_path1 = instance.create_test_mount("path1").path
        edenfs_path2 = instance.create_test_mount("path2").path
        # Remove path2 from the list of mounts in the instance
        instance.remove_checkout_configuration(str(edenfs_path2))
        state_dir = instance.clients_path / "path2"

        dry_run = False
        out = TestOutput()
        exit_code = doctor.cure_what_ails_you(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            #  `FakeEdenInstance`.
            instance,
            dry_run,
            mount_table=instance.mount_table,
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            network_checker=FakeNetworkChecker(),
            out=out,
        )

        self.assertEqual(
            f"""\
Checking {edenfs_path1}
Checking {edenfs_path2}
<yellow>- Found problem:<reset>
Checkout {edenfs_path2} is running but not listed in Eden's configuration file, and its state directory {state_dir} cannot be registered.
Running "eden rm {edenfs_path2}" will remove this checkout.

{get_support_suggestion()}""",
            out.getvalue(),
        )
        self.assertEqual(1, exit_code)

    def test_configured_checkout_without_state_dir_is_unregistered(self) -> None:
        # A removal interrupted after the client directory was deleted leaves
        # a config.json entry EdenFS cannot mount; doctor removes the entry.
        instance = FakeEdenInstance(self.make_temporary_directory())
        edenfs_path1 = instance.create_test_mount("path1").path
        checkout2 = instance.create_test_mount("path2", active=False)
        shutil.rmtree(checkout2.state_dir)

        out = TestOutput()
        exit_code = doctor.cure_what_ails_you(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            #  `FakeEdenInstance`.
            instance,
            dry_run=False,
            mount_table=instance.mount_table,
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            network_checker=FakeNetworkChecker(),
            out=out,
        )

        self.assertEqual(
            f"""\
Checking {edenfs_path1}
Checking {checkout2.path}
<yellow>- Found problem:<reset>
{checkout2.path} is listed in Eden's configuration file, but its state directory {checkout2.state_dir} does not exist, so EdenFS cannot mount it.
Removing {checkout2.path} from Eden's configuration file...<green>fixed<reset>

<yellow>Successfully fixed 1 problem.<reset>
""",
            out.getvalue(),
        )
        self.assertEqual(0, exit_code)
        self.assertEqual([str(checkout2.path)], instance.unregister_calls)
        self.assertNotIn(str(checkout2.path), list(instance.get_mount_paths()))

    def test_unconfigured_mount_is_registered(self) -> None:
        # An `eden clone` that stopped between the mount and the directory-map
        # write leaves a running mount with its state directory but no
        # config.json entry; doctor registers it.
        instance = FakeEdenInstance(self.make_temporary_directory())
        edenfs_path1 = instance.create_test_mount("path1").path
        edenfs_path2 = instance.create_test_mount("path2").path
        instance.remove_checkout_configuration(str(edenfs_path2), keep_state_dir=True)
        self.assertNotIn(str(edenfs_path2), list(instance.get_mount_paths()))

        out = TestOutput()
        exit_code = doctor.cure_what_ails_you(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            #  `FakeEdenInstance`.
            instance,
            dry_run=False,
            mount_table=instance.mount_table,
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            network_checker=FakeNetworkChecker(),
            out=out,
        )

        self.assertEqual(
            f"""\
Checking {edenfs_path1}
Checking {edenfs_path2}
<yellow>- Found problem:<reset>
Checkout {edenfs_path2} is running but not listed in Eden's configuration file, so EdenFS would not remount it after a restart.
Registering {edenfs_path2} in Eden's configuration file...<green>fixed<reset>

<yellow>Successfully fixed 1 problem.<reset>
""",
            out.getvalue(),
        )
        self.assertEqual(0, exit_code)
        self.assertIn(str(edenfs_path2), list(instance.get_mount_paths()))
