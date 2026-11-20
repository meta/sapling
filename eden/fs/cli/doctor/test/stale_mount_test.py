#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict


import errno
from typing import List, Tuple
from unittest.mock import MagicMock, patch

import eden.fs.cli.doctor as doctor
from eden.fs.cli.doctor import check_stale_mounts
from eden.fs.cli.doctor.test.lib.fake_constants import FAKE_UID
from eden.fs.cli.doctor.test.lib.fake_eden_instance import FakeEdenInstance
from eden.fs.cli.doctor.test.lib.fake_mount_table import FakeMountTable
from eden.fs.cli.doctor.test.lib.testcase import DoctorTestBase
from eden.fs.cli.test.lib.output import TestOutput


class StaleMountsCheckTest(DoctorTestBase):
    # pyre-fixme[4]: Attribute must be annotated.
    maxDiff = None

    def setUp(self) -> None:
        self.active_mounts: List[bytes] = [b"/mnt/active1", b"/mnt/active2"]
        self.mount_table = FakeMountTable()
        self.mount_table.add_mount("/mnt/active1")
        self.mount_table.add_mount("/mnt/active2")

    def run_check(self, dry_run: bool) -> Tuple[doctor.ProblemFixer, str]:
        fixer, out = self.create_fixer(dry_run)
        check_stale_mounts.check_for_stale_mounts(fixer, mount_table=self.mount_table)
        return fixer, out.getvalue()

    def test_does_not_unmount_active_mounts(self) -> None:
        fixer, out = self.run_check(dry_run=False)
        self.assertEqual("", out)
        self.assert_results(fixer, num_problems=0)
        self.assertEqual([], self.mount_table.unmount_lazy_calls)
        self.assertEqual([], self.mount_table.unmount_force_calls)

    def test_does_not_unmount_active_mounts_fuseedenfs(self) -> None:
        self.active_mounts.append(b"/mnt/fuseedenfs")
        self.mount_table.add_mount("/mnt/fuseedenfs", vfstype="fuse.edenfs")
        fixer, out = self.run_check(dry_run=False)
        self.assertEqual("", out)
        self.assert_results(fixer, num_problems=0)
        self.assertEqual([], self.mount_table.unmount_lazy_calls)
        self.assertEqual([], self.mount_table.unmount_force_calls)

    def test_working_nonactive_mount_is_not_unmounted(self) -> None:
        # Add a working edenfs mount that is not part of our active list
        self.mount_table.add_mount("/mnt/other1")

        fixer, out = self.run_check(dry_run=False)
        self.assertEqual("", out)
        self.assert_results(fixer, num_problems=0)
        self.assertEqual([], self.mount_table.unmount_lazy_calls)
        self.assertEqual([], self.mount_table.unmount_force_calls)

    def test_working_nonactive_mount_is_not_unmounted_fuseedenfs(self) -> None:
        # Add a working edenfs mount that is not part of our active list
        self.mount_table.add_mount("/mnt/fuseedenfs", vfstype="fuse.edenfs")

        fixer, out = self.run_check(dry_run=False)
        self.assertEqual("", out)
        self.assert_results(fixer, num_problems=0)
        self.assertEqual([], self.mount_table.unmount_lazy_calls)
        self.assertEqual([], self.mount_table.unmount_force_calls)

    @patch("eden.fs.cli.doctor.check_stale_mounts.get_sudo_perms")
    def test_force_unmounts_if_lazy_fails(self, mock_get_sudo_perms: MagicMock) -> None:
        mock_get_sudo_perms.return_value = True
        self.mount_table.add_stale_mount("/mnt/stale1")
        self.mount_table.add_stale_mount("/mnt/stale2")
        self.mount_table.fail_unmount_lazy(b"/mnt/stale1")

        fixer, out = self.run_check(dry_run=False)
        self.assertEqual(
            f"""\
<yellow>- Found problem:<reset>
Found 2 stale edenfs mounts:
  /mnt/stale1
  /mnt/stale2
Unmounting 2 stale edenfs mounts...<green>fixed<reset>

""",
            out,
        )
        self.assert_results(fixer, num_problems=1, num_fixed_problems=1)
        self.assertEqual(
            [b"/mnt/stale1", b"/mnt/stale2"], self.mount_table.unmount_lazy_calls
        )
        self.assertEqual([b"/mnt/stale1"], self.mount_table.unmount_force_calls)

    @patch("eden.fs.cli.doctor.check_stale_mounts.get_sudo_perms")
    def test_unmount_for_fuseedenfs_mount(self, mock_get_sudo_perms: MagicMock) -> None:
        mock_get_sudo_perms.return_value = True
        self.mount_table.add_stale_mount("/mnt/fuseedenfs", vfstype="fuse.edenfs")

        fixer, out = self.run_check(dry_run=False)
        self.assertEqual(
            """\
<yellow>- Found problem:<reset>
Found 1 stale edenfs mount:
  /mnt/fuseedenfs
Unmounting 1 stale edenfs mount...<green>fixed<reset>

""",
            out,
        )
        self.assert_results(fixer, num_problems=1, num_fixed_problems=1)
        self.assertEqual([b"/mnt/fuseedenfs"], self.mount_table.unmount_lazy_calls)

    @patch("eden.fs.cli.doctor.check_stale_mounts.StaleMountsFound.perform_fix")
    def test_check_fix(self, mock_perform_fix: MagicMock) -> None:
        self.mount_table.add_stale_mount("/mnt/stale1")
        self.mount_table.add_stale_mount("/mnt/stale2")
        self.mount_table.fail_unmount_lazy(b"/mnt/stale1")

        mock_perform_fix.return_value = None
        fixer, out = self.create_fixer(False)
        check_stale_mounts.check_for_stale_mounts(fixer, mount_table=self.mount_table)
        self.assertEqual(
            """\
<yellow>- Found problem:<reset>
Found 2 stale edenfs mounts:
  /mnt/stale1
  /mnt/stale2
Unmounting 2 stale edenfs mounts...<red>error<reset>
Attempted and failed to fix problem StaleMountsFound

""",
            out.getvalue(),
        )
        self.assert_results(
            fixer, num_problems=1, num_fixed_problems=0, num_failed_fixes=1
        )

    @patch("eden.fs.cli.doctor.check_stale_mounts.get_sudo_perms")
    def test_fix_with_an_unknown_status_mount(
        self, mock_get_sudo_perms: MagicMock
    ) -> None:
        mock_get_sudo_perms.return_value = True
        self.mount_table.add_stale_mount("/mnt/stale1")
        self.mount_table.add_mount("/mnt/eio")
        self.mount_table.fail_access("/mnt/eio/.eden", errno.EIO)

        fixer, out = self.run_check(dry_run=False)
        self.assertEqual(
            """\
<yellow>- Found problem:<reset>
Found 1 stale edenfs mount:
  /mnt/stale1
Unmounting 1 stale edenfs mount...<green>fixed<reset>

""",
            out,
        )
        self.assert_results(fixer, num_problems=1, num_fixed_problems=1)
        self.assertEqual([b"/mnt/stale1"], self.mount_table.unmount_lazy_calls)
        self.assertEqual([], self.mount_table.unmount_force_calls)

    def test_dry_run_prints_stale_mounts_and_does_not_unmount(self) -> None:
        self.mount_table.add_stale_mount("/mnt/stale1")
        self.mount_table.add_stale_mount("/mnt/stale2")

        fixer, out = self.run_check(dry_run=True)
        self.assertEqual(
            f"""\
<yellow>- Found problem:<reset>
Found 2 stale edenfs mounts:
  /mnt/stale1
  /mnt/stale2
Would unmount 2 stale edenfs mounts

""",
            out,
        )
        self.assert_results(fixer, num_problems=1)
        self.assertEqual([], self.mount_table.unmount_lazy_calls)
        self.assertEqual([], self.mount_table.unmount_force_calls)

    @patch("eden.fs.cli.doctor.check_stale_mounts.get_sudo_perms")
    def test_fails_if_unmount_fails(self, mock_get_sudo_perms: MagicMock) -> None:
        mock_get_sudo_perms.return_value = True
        self.mount_table.add_stale_mount("/mnt/stale1")
        self.mount_table.add_stale_mount("/mnt/stale2")
        self.mount_table.fail_unmount_lazy(b"/mnt/stale1", b"/mnt/stale2")
        self.mount_table.fail_unmount_force(b"/mnt/stale1")

        fixer, out = self.run_check(dry_run=False)
        self.assertIn(
            """\
<yellow>- Found problem:<reset>
Found 2 stale edenfs mounts:
  /mnt/stale1
  /mnt/stale2
Unmounting 2 stale edenfs mounts...<red>error<reset>
Failed to fix or verify fix for problem StaleMountsFound: RemediationError: Failed to unmount 1 mount point:
  /mnt/stale1
""",
            out,
        )
        self.assert_results(fixer, num_problems=1, num_failed_fixes=1)
        self.assertEqual(
            [b"/mnt/stale1", b"/mnt/stale2"], self.mount_table.unmount_lazy_calls
        )
        self.assertEqual(
            [b"/mnt/stale1", b"/mnt/stale2"], self.mount_table.unmount_force_calls
        )

    def test_ignores_noneden_mounts(self) -> None:
        self.mount_table.add_mount("/", device="/dev/sda1", vfstype="ext4")
        fixer, out = self.run_check(dry_run=False)
        self.assertEqual("", out)
        self.assert_results(fixer, num_problems=0)
        self.assertEqual([], self.mount_table.unmount_lazy_calls)
        self.assertEqual([], self.mount_table.unmount_force_calls)

    def test_ignores_errors_other_than_enotconn(self) -> None:
        self.mount_table.fail_access("/mnt/active1", errno.EPERM)
        self.mount_table.fail_access("/mnt/active1/.eden", errno.EPERM)

        fixer, out = self.run_check(dry_run=False)
        self.assertEqual("", out)
        self.assert_results(fixer, num_problems=0)

    def test_does_not_unmount_if_cannot_stat_stale_mount(self) -> None:
        self.mount_table.add_mount("/mnt/stale1")
        self.mount_table.fail_access("/mnt/stale1", errno.EACCES)
        self.mount_table.fail_access("/mnt/stale1/.eden", errno.EACCES)

        with self.assertLogs() as logs_assertion:
            fixer, out = self.run_check(dry_run=False)
            self.assertEqual("", out)
            self.assertEqual(0, fixer.num_problems)
            self.assertEqual([], self.mount_table.unmount_lazy_calls)
            self.assertEqual([], self.mount_table.unmount_force_calls)
        # Verify that the reason for skipping this mount is logged.
        self.assertIn(
            "Unclear whether /mnt/stale1 is stale or not. "
            "lstat() failed: [Errno 13] Permission denied",
            "\n".join(logs_assertion.output),
        )

    @patch("eden.fs.cli.doctor.check_stale_mounts.get_sudo_perms")
    def test_does_unmount_if_stale_mount_is_unconnected(
        self, mock_get_sudo_perms: MagicMock
    ) -> None:
        mock_get_sudo_perms.return_value = True
        self.mount_table.add_stale_mount("/mnt/stale1")

        fixer, out = self.run_check(dry_run=False)
        self.assertEqual(
            f"""\
<yellow>- Found problem:<reset>
Found 1 stale edenfs mount:
  /mnt/stale1
Unmounting 1 stale edenfs mount...<green>fixed<reset>

""",
            out,
        )
        self.assert_results(fixer, num_problems=1, num_fixed_problems=1)
        self.assertEqual([b"/mnt/stale1"], self.mount_table.unmount_lazy_calls)
        self.assertEqual([], self.mount_table.unmount_force_calls)

    @patch("eden.fs.cli.doctor.check_stale_mounts.get_sudo_perms")
    def test_logs_configured_stale_mount(self, mock_get_sudo_perms: MagicMock) -> None:
        mock_get_sudo_perms.return_value = True
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("configured")
        mount_table = instance.mount_table
        mount_table.fail_access(str(checkout.path / ".eden"), errno.ENOTCONN)
        fixer = doctor.ProblemFixer(instance, TestOutput())

        check_stale_mounts.check_for_stale_mounts(fixer, mount_table, instance)

        checkout_path = str(checkout.path)
        self.assertEqual(
            [
                (
                    "eden_mount_health_issue",
                    {
                        "source": "doctor",
                        "reason": "stale_mount",
                        "mount_path": checkout_path,
                        "path_type": "configured_checkout",
                        "attempted_repair": False,
                        "success": False,
                        "checkout_path": checkout_path,
                    },
                )
            ],
            instance.logged_samples,
        )

    @patch("eden.fs.cli.doctor.check_stale_mounts.get_sudo_perms")
    def test_hanging_mount_is_shown_but_not_logged(
        self, mock_get_sudo_perms: MagicMock
    ) -> None:
        mock_get_sudo_perms.return_value = True
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("hanging")
        mount_table = instance.mount_table
        mount_table.fail_access(str(checkout.path / ".eden"), errno.ETIMEDOUT)
        fixer = doctor.ProblemFixer(instance, TestOutput())

        check_stale_mounts.check_for_stale_mounts(fixer, mount_table, instance)

        self.assert_results(fixer, num_problems=1, num_manual_fixes=1)
        self.assertEqual({"HangingMountFound"}, fixer.problem_types)
        self.assertEqual([], instance.logged_samples)

    def test_does_not_unmount_other_users_mounts(self) -> None:
        self.mount_table.add_mount("/mnt/stale1", uid=FAKE_UID + 1)

        fixer, out = self.run_check(dry_run=False)
        self.assertEqual("", out)
        self.assert_results(fixer, num_problems=0)
        self.assertEqual([], self.mount_table.unmount_lazy_calls)
        self.assertEqual([], self.mount_table.unmount_force_calls)

    def test_does_not_unmount_mounts_with_same_device_as_active_mount(self) -> None:
        active1_dev = self.mount_table.lstat("/mnt/active1").st_dev
        self.mount_table.add_mount("/mnt/stale1", dev=active1_dev)

        fixer, out = self.run_check(dry_run=False)
        self.assertEqual("", out)
        self.assert_results(fixer, num_problems=0)
        self.assertEqual([], self.mount_table.unmount_lazy_calls)
        self.assertEqual([], self.mount_table.unmount_force_calls)

    @patch("eden.fs.cli.doctor.check_stale_mounts.get_sudo_perms")
    def test_missing_sudo_perms(self, mock_get_sudo_perms: MagicMock) -> None:
        mock_get_sudo_perms.return_value = False
        self.mount_table.add_stale_mount("/mnt/stale1")

        fixer, out = self.run_check(dry_run=False)
        self.assertRegex(
            out,
            r"""<yellow>- Found problem:<reset>
Found 1 stale edenfs mount:
  /mnt/stale1
Unmounting 1 stale edenfs mount...<red>error<reset>
Failed to fix or verify fix for problem StaleMountsFound: RemediationError: Unable to unmount stale mounts due to missing sudo permissions.
(.*|\n)+
""",
        )
