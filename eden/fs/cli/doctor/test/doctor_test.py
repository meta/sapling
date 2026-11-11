#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import binascii
import errno
import os
import stat
import struct
import subprocess
import sys
import typing
import unittest
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional, Tuple
from unittest.mock import call, MagicMock, patch

import eden.fs.cli.doctor as doctor
from eden.fs.cli.config import EdenCheckout, EdenInstance, SnapshotState
from eden.fs.cli.doctor import (
    check_hg,
    check_mount,
    check_network,
    check_running_mount,
    check_watchman,
    get_doctor_link,
    get_local_commit_recovery_link,
)
from eden.fs.cli.doctor.check_filesystems import (
    check_hg_status_match_hg_diff,
    check_loaded_content,
    check_materialized_are_accessible,
    DEFAULT_MAX_MODIFIED_FILES_FOR_HG_DIFF,
)
from eden.fs.cli.doctor.check_redirections import check_redirections
from eden.fs.cli.doctor.facebook.internal_consts import get_netinfo_link
from eden.fs.cli.doctor.problem import ProblemSeverity
from eden.fs.cli.doctor.test.lib.fake_client import ResetParentsCommitsArgs
from eden.fs.cli.doctor.test.lib.fake_eden_instance import FakeEdenInstance
from eden.fs.cli.doctor.test.lib.fake_fs_util import FakeFsUtil
from eden.fs.cli.doctor.test.lib.fake_hg_repo import FakeHgRepo
from eden.fs.cli.doctor.test.lib.fake_mount_table import FakeMountTable
from eden.fs.cli.doctor.test.lib.fake_network_checker import FakeNetworkChecker
from eden.fs.cli.doctor.test.lib.fake_vscode_extensions_checker import (
    getFakeVSCodeExtensionsChecker,
    getFakeVSCodeExtensionsCheckerWithExtensions,
)
from eden.fs.cli.doctor.test.lib.problem_collector import ProblemCollector
from eden.fs.cli.doctor.test.lib.testcase import DoctorTestBase
from eden.fs.cli.doctor.util import (
    CheckoutInfo,
    get_checkouts_info,
    get_dependent_repos,
)
from eden.fs.cli.prjfs import PRJ_FILE_STATE
from eden.fs.cli.redirect import Redirection, RedirectionState, RedirectionType
from eden.fs.cli.test.lib.output import TestOutput
from eden.fs.service.eden.thrift_enums import MountState
from eden.fs.service.eden.thrift_types import (
    GetScmStatusResult,
    InternalStats,
    MountInodeInfo,
    MountInodeInfo as LegacyMountInodeInfo,
    ScmFileStatus,
    ScmStatus,
    SHA1Result,
    TreeInodeDebugInfo,
    TreeInodeEntryDebugInfo,
)
from fb303_core.thrift_types import fb303_status


# Invalid decoration [56]: Pyre was not able to infer the type of argument `b"�eC!".__mul__(5)` to decorator factory `unittest.mock.patch`.
# eden/fs/cli/doctor/test/doctor_test.py:728:14 Missing parameter annotation [2]: Parameter `mock_get_tip_commit_hash` has no type specified.
# eden/fs/cli/doctor/test/doctor_test.py:770:5 Invalid decoration [56]: Pyre was not able to infer the type of argument `b"�eC!".__mul__(5)` to decorator factory `unittest.mock.patch`.


class SnapshotFormatTest(DoctorTestBase):
    """
    EdenFS doctor can parse the SNAPSHOT file directly. Validate its parse
    against different formats.
    """

    def setUp(self) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        self.checkout = instance.create_test_mount(
            "path",
        )

    def test_format1_one_parent(self) -> None:
        (self.checkout.state_dir / "SNAPSHOT").write_bytes(
            b"eden\x00\x00\x00\x01" + binascii.unhexlify("11223344556677889900" * 2)
        )
        self.assertEqual("11223344556677889900" * 2, self.checkout.get_snapshot()[0])

    def test_format1_two_parents(self) -> None:
        (self.checkout.state_dir / "SNAPSHOT").write_bytes(
            b"eden\x00\x00\x00\x01"
            + binascii.unhexlify("11223344556677889900" * 2)
            + binascii.unhexlify("00998877665544332211" * 2)
        )
        self.assertEqual("11223344556677889900" * 2, self.checkout.get_snapshot()[0])

    def test_format2_ascii(self) -> None:
        (self.checkout.state_dir / "SNAPSHOT").write_bytes(
            b"eden\x00\x00\x00\x02"
            + struct.pack(">L", 40)
            + b"11223344556677889900" * 2
        )
        self.assertEqual("11223344556677889900" * 2, self.checkout.get_snapshot()[0])


class CheckoutConfigCorruptionRepoTypeTest(DoctorTestBase):
    """
    EdenCheckoutConfigCorruption rebuilds config.toml, so it cannot read the
    scm_type or the repo dir layout from it. Validate that it recovers both
    from the checkout's own repo dir.
    """

    def _problem(self, tmp_dir: str) -> doctor.EdenCheckoutConfigCorruption:
        instance = FakeEdenInstance(tmp_dir)
        checkout_info = CheckoutInfo(
            typing.cast(EdenInstance, instance),
            Path(tmp_dir) / "mount",
        )
        return doctor.EdenCheckoutConfigCorruption(checkout_info, Exception("corrupt"))

    def test_in_mount_layout(self) -> None:
        tmp_dir = self.make_temporary_directory()
        state_dir = Path(tmp_dir) / "state"
        state_dir.mkdir()
        hg_dir = Path(tmp_dir) / "mount" / ".hg"
        hg_dir.mkdir(parents=True)
        (hg_dir / "requires").write_text("eden\nstore\n")

        self.assertEqual(("hg", False), self._problem(tmp_dir).get_repo_type(state_dir))

    def test_sl_dot_dir(self) -> None:
        tmp_dir = self.make_temporary_directory()
        state_dir = Path(tmp_dir) / "state"
        state_dir.mkdir()
        sl_dir = Path(tmp_dir) / "mount" / ".sl"
        sl_dir.mkdir(parents=True)
        (sl_dir / "requires").write_text("eden\nedensparse\nstore\n")

        self.assertEqual(
            ("filteredhg", False), self._problem(tmp_dir).get_repo_type(state_dir)
        )

    @unittest.skipIf(sys.platform == "win32", "requires symlink creation")
    def test_off_mount_layout(self) -> None:
        # The repo dir lives in the state dir, and the mount only holds a
        # symlink to it. The regenerated config has to say so.
        tmp_dir = self.make_temporary_directory()
        state_dir = Path(tmp_dir) / "state"
        repo_dir = state_dir / "sl-repo-dir"
        repo_dir.mkdir(parents=True)
        (repo_dir / "requires").write_text("eden\nedensparse\nstore\n")
        mount_path = Path(tmp_dir) / "mount"
        mount_path.mkdir()
        os.symlink(repo_dir, mount_path / ".hg", target_is_directory=True)

        self.assertEqual(
            ("filteredhg", True), self._problem(tmp_dir).get_repo_type(state_dir)
        )

    def test_no_requires_found(self) -> None:
        tmp_dir = self.make_temporary_directory()
        state_dir = Path(tmp_dir) / "state"
        state_dir.mkdir()

        self.assertEqual(
            ("other", False), self._problem(tmp_dir).get_repo_type(state_dir)
        )


class DoctorTest(DoctorTestBase):
    # The diffs for what is written to stdout can be large.
    # pyre-fixme[4]: Attribute must be annotated.
    maxDiff = None

    def format_win_path_for_regex(self, path: str) -> str:
        # Formats the path to be compatible with regex matching on windows
        if sys.platform == "win32":
            return path.replace("\\", "\\\\")
        return path

    def setUpEdenMountTest(
        self,
        state: Optional[MountState] = None,
    ) -> Tuple[doctor.ProblemFixer, TestOutput, EdenCheckout]:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")

        path = checkout.path
        checkout_info = CheckoutInfo(
            # pyre-fixme[6]: For 3rd param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            instance,
            path,
            state=state,
            backing_repo=checkout.get_backing_repo_path(),
        )

        fixer, out = self.create_fixer(dry_run=False)
        mount_table = instance.mount_table

        edenfs_path = "/path/to/eden-mount"
        watchman_roots = {edenfs_path}
        watchman_info = check_watchman.WatchmanCheckInfo(watchman_roots)

        check_mount(
            out,
            fixer,
            # pyre-fixme[6]: For 3rd param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            instance,
            checkout_info,
            mount_table,
            watchman_info,
            [checkout_info],
            set(),
            set(),
            FakeNetworkChecker(),
            True,
            True,
        )
        return fixer, out, checkout

    def test_running_mount_not_visible_is_unmounted_before_remount(self) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")
        path = checkout.path
        checkout_info = CheckoutInfo(
            # pyre-fixme[6]: For 3rd param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            instance,
            path,
            state=MountState.RUNNING,
            backing_repo=checkout.get_backing_repo_path(),
            running_state_dir=checkout.state_dir,
            configured_state_dir=checkout.state_dir,
            visible_in_daemon_namespace=False,
        )

        instance._fake_client.change_visible_in_daemon_namespace(path, None)
        fixer, out = self.create_fixer(dry_run=False)
        check_mount(
            out,
            fixer,
            # pyre-fixme[6]: For 3rd param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            instance,
            checkout_info,
            instance.mount_table,
            check_watchman.WatchmanCheckInfo(set()),
            [checkout_info],
            set(),
            set(),
            FakeNetworkChecker(),
            True,
            True,
        )

        self.assertEqual(
            f"""\
<yellow>- Found problem:<reset>
{path} is RUNNING in EdenFS state, but is not visible in EdenFS's mount namespace
Unmounting and remounting {path}...<green>fixed<reset>

""",
            out.getvalue(),
        )
        self.assertEqual([(str(path), True, True)], instance.unmount_calls)
        self.assertEqual([(str(path), False)], instance.mount_calls)

    @patch("eden.fs.cli.doctor.check_filesystems.check_inode_counts")
    @patch("eden.fs.cli.doctor.check_filesystems.check_using_nfs_path")
    @patch("eden.fs.cli.doctor.check_hg.check_hg")
    @patch("eden.fs.cli.doctor.check_filesystems.check_hg_status_match_hg_diff")
    @patch("eden.fs.cli.config.EdenCheckout.get_config")
    def setUpEdenNetworkTest(
        self,
        mock_get_config: MagicMock,
        mock_check_hg_status: MagicMock,
        mock_check_hg: MagicMock,
        mock_check_nfs: MagicMock,
        mock_check_inode: MagicMock,
        *,
        fast: bool = False,
    ) -> Tuple[doctor.ProblemFixer, TestOutput, EdenCheckout]:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")
        checkout_config = instance._checkouts_by_path[str(checkout.path)].config

        mock_get_config.return_value = checkout_config
        path = checkout.path
        checkout_info = CheckoutInfo(
            # pyre-fixme[6]: For 3rd param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            instance,
            path,
            state=MountState.RUNNING,
            backing_repo=checkout.get_backing_repo_path(),
            running_state_dir=path,
            configured_state_dir=path,
        )

        fixer, out = self.create_fixer(dry_run=False)
        mount_table = instance.mount_table

        edenfs_path = "/path/to/eden-mount"
        watchman_roots = {edenfs_path}
        watchman_info = check_watchman.WatchmanCheckInfo(watchman_roots)

        check_mount(
            out,
            fixer,
            # pyre-fixme[6]: For 3rd param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            instance,
            checkout_info,
            mount_table,
            watchman_info,
            [],
            set(),
            set(),
            check_network.NetworkChecker(),
            False,
            fast,
        )
        if fast:
            mock_check_hg_status.assert_not_called()
        else:
            mock_check_hg_status.assert_called_once()
        return fixer, out, checkout

    def test_fast_skips_hg_status_and_diff_check(self) -> None:
        _, out, _ = self.setUpEdenNetworkTest(fast=True)
        self.assertEqual(out.getvalue(), "")

    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    def test_end_to_end_test_with_various_scenarios(
        self, mock_watchman: MagicMock
    ) -> None:
        side_effects: List[Dict[str, Any]] = []
        calls = []
        instance = FakeEdenInstance(self.make_temporary_directory())

        # In edenfs_path1, we will break the snapshot check.
        edenfs_path1_snapshot = "abcd" * 10
        edenfs_path1_dirstate_parent = "12345678" * 5
        checkout = instance.create_test_mount(
            "path1",
            snapshot=edenfs_path1_snapshot,
            dirstate_parent=edenfs_path1_dirstate_parent,
        )
        edenfs_path1 = str(checkout.path)
        edenfs_dot_hg_path1 = str(checkout.hg_dot_path)

        # In edenfs_path2, we will break the inotify check and the Nuclide
        # subscriptions check.
        edenfs_path2 = str(
            instance.create_test_mount("path2", scm_type="git", setup_path=False).path
        )

        # In edenfs_path3, we do not create the .hg directory
        edenfs_path3 = instance.create_test_mount("path3", setup_path=False).path
        edenfs_dot_hg_path3 = edenfs_path3 / ".hg"
        edenfs_path3 = str(edenfs_path3)
        os.makedirs(edenfs_path3)

        calls.append(call(["watch-list"]))
        side_effects.append({"roots": [edenfs_path1, edenfs_path2, edenfs_path3]})

        calls.append(call(["watch-project", edenfs_path1]))
        side_effects.append({"watcher": "eden"})

        calls.append(call(["watch-project", edenfs_path2]))
        side_effects.append({"watcher": "inotify"})
        calls.append(call(["watch-del", edenfs_path2]))
        side_effects.append({"watch-del": True, "root": edenfs_path2})
        calls.append(call(["watch-project", edenfs_path2]))
        side_effects.append({"watcher": "eden"})
        calls.append(call(["watch-project", edenfs_path3]))
        side_effects.append({"watcher": "eden"})

        mock_watchman.side_effect = side_effects

        out = TestOutput()
        dry_run = False

        exit_code = doctor.cure_what_ails_you(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            #  `FakeEdenInstance`.
            instance,
            dry_run,
            min_severity_to_report=ProblemSeverity.ALL,
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
<yellow>- Found problem:<reset>
Found inconsistent/missing data in {edenfs_dot_hg_path1}:
  mercurial's parent commit is {edenfs_path1_dirstate_parent}, \
but Eden's internal parent commit is {edenfs_path1_snapshot}
Repairing hg directory contents for {edenfs_path1}...<green>fixed<reset>

Checking {edenfs_path2}
<yellow>- Found problem:<reset>
Watchman is watching {edenfs_path2} with the wrong watcher type: \
"inotify" instead of "eden"
Fixing watchman watch for {edenfs_path2}...<green>fixed<reset>

Checking {edenfs_path3}
<yellow>- Found problem:<reset>
Missing hg directory: {edenfs_dot_hg_path3}
Repairing hg directory contents for {edenfs_path3}...<green>fixed<reset>

<yellow>Successfully fixed 3 problems.<reset>
""",
            out.getvalue(),
        )
        mock_watchman.assert_has_calls(calls)
        self.assertEqual(0, exit_code)

    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    # pyre-fixme[2]: Parameter must be annotated.
    def test_not_all_mounts_have_watchman_watcher(self, mock_watchman) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        edenfs_path = str(instance.create_test_mount("eden-mount", scm_type="git").path)
        edenfs_path_not_watched = str(
            instance.create_test_mount("eden-mount-not-watched", scm_type="git").path
        )
        side_effects: List[Dict[str, Any]] = []
        calls = []

        calls.append(call(["watch-list"]))
        side_effects.append({"roots": [edenfs_path]})
        calls.append(call(["watch-project", edenfs_path]))
        side_effects.append({"watcher": "eden"})
        mock_watchman.side_effect = side_effects

        out = TestOutput()
        dry_run = False
        exit_code = doctor.cure_what_ails_you(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            #  `FakeEdenInstance`.
            instance,
            dry_run,
            min_severity_to_report=ProblemSeverity.ALL,
            mount_table=instance.mount_table,
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            network_checker=FakeNetworkChecker(),
            out=out,
        )

        self.assertEqual(
            f"Checking {edenfs_path}\n"
            f"Checking {edenfs_path_not_watched}\n"
            "<green>No issues detected.<reset>\n",
            out.getvalue(),
        )
        mock_watchman.assert_has_calls(calls)
        self.assertEqual(0, exit_code)

    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    # pyre-fixme[2]: Parameter must be annotated.
    def test_eden_not_in_use(self, mock_watchman) -> None:
        instance = FakeEdenInstance(
            self.make_temporary_directory(), status=fb303_status.DEAD
        )

        out = TestOutput()
        dry_run = False
        exit_code = doctor.cure_what_ails_you(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            #  `FakeEdenInstance`.
            instance,
            dry_run,
            min_severity_to_report=ProblemSeverity.ALL,
            mount_table=FakeMountTable(),
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            network_checker=FakeNetworkChecker(),
            out=out,
        )

        self.assertEqual(
            "EdenFS daemon is not running. No Eden checkouts are configured, "
            "so this is not considered an issue. If you need Eden checkouts, "
            "create one with 'fbclone <repo> --eden'.\n",
            out.getvalue(),
        )
        self.assertEqual(0, exit_code)

    @patch("eden.fs.cli.util.HealthStatus.is_healthy")
    @patch("eden.fs.cli.daemon.start_edenfs_service")
    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    def test_edenfs_not_running_then_started(
        self,
        mock_watchman: MagicMock,
        mock_start: MagicMock,
        mock_is_healthy: MagicMock,
    ) -> None:
        instance = FakeEdenInstance(
            self.make_temporary_directory(), status=fb303_status.DEAD
        )
        instance.create_test_mount("eden-mount")

        out = TestOutput()
        dry_run = False
        # We can't actually start the Eden daemon in a unit test, so we mock
        # it instead.
        mock_start.return_value = None
        mock_is_healthy.return_value = True
        exit_code = doctor.cure_what_ails_you(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            #  `FakeEdenInstance`.
            instance,
            dry_run,
            min_severity_to_report=ProblemSeverity.ALL,
            mount_table=FakeMountTable(),
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            network_checker=FakeNetworkChecker(),
            out=out,
        )

        self.assertRegex(
            out.getvalue(),
            r"""<yellow>- Found problem:<reset>
EdenFS is not running
Running `eden start` to start EdenFS......<green>fixed<reset>

<yellow>Successfully fixed 1 problem.<reset>
""",
        )
        self.assertEqual(0, exit_code)

    @patch("eden.fs.cli.util.HealthStatus.is_starting")
    @patch("eden.fs.cli.daemon.start_edenfs_service")
    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    def test_edenfs_not_running_then_still_starting(
        self,
        mock_watchman: MagicMock,
        mock_start: MagicMock,
        mock_is_starting: MagicMock,
    ) -> None:
        instance = FakeEdenInstance(
            self.make_temporary_directory(), status=fb303_status.DEAD
        )
        instance.create_test_mount("eden-mount")

        out = TestOutput()
        dry_run = False
        # We can't actually start the Eden daemon in a unit test, so we mock
        # it instead.
        mock_start.return_value = None
        mock_is_starting.return_value = True
        exit_code = doctor.cure_what_ails_you(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            #  `FakeEdenInstance`.
            instance,
            dry_run,
            min_severity_to_report=ProblemSeverity.ALL,
            mount_table=FakeMountTable(),
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            out=out,
        )

        self.assertRegex(
            out.getvalue(),
            r"""<yellow>- Found problem:<reset>
EdenFS is not running
Running `eden start` to start EdenFS......<yellow>EdenFS still starting, use `eden status --wait` to watch progress and ensure it starts<reset>
<red>error<reset>.*""",
        )
        self.assertEqual(1, exit_code)

    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    # pyre-fixme[2]: Parameter must be annotated.
    def test_edenfs_starting(self, mock_watchman) -> None:
        instance = FakeEdenInstance(
            self.make_temporary_directory(), status=fb303_status.STARTING
        )
        instance.create_test_mount("eden-mount")

        out = TestOutput()
        dry_run = False
        exit_code = doctor.cure_what_ails_you(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            #  `FakeEdenInstance`.
            instance,
            dry_run,
            min_severity_to_report=ProblemSeverity.ALL,
            mount_table=FakeMountTable(),
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            network_checker=FakeNetworkChecker(),
            out=out,
        )

        self.assertRegex(
            out.getvalue(),
            r"""<yellow>- Found problem:<reset>
EdenFS is currently still starting\.
Please wait for edenfs to finish starting\. You can watch its progress with
`eden status --wait`\.

If EdenFS seems to be taking too long to start you can try restarting it
with "eden restart --force"

<yellow>1 issue with recommended fixes\.<reset>
Collect an 'eden rage' and ask in the EdenFS (Windows |macOS )?Users group if you need help fixing issues with EdenFS:
(https://fb\.workplace\.com/groups/eden\.users|https://fb\.workplace\.com/groups/edenfswindows|https://fb\.workplace\.com/groups/edenfsmacos)
""",
        )
        self.assertEqual(1, exit_code)

    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    @patch("eden.fs.cli.config.EdenCheckout.get_config")
    def test_edenfs_starting_mount_error(
        self, mock_get_config: MagicMock, mock_watchman: MagicMock
    ) -> None:
        # Not strictly the correct file to but easier to mock the first call
        mock_get_config.side_effect = FileNotFoundError(
            errno.ENOENT, os.strerror(errno.ENOENT), "SNAPSHOT"
        )
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")

        path = checkout.path
        checkout_info = CheckoutInfo(
            # pyre-fixme[6]: For 3rd param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            instance,
            path,
            state=MountState.STARTING,
            backing_repo=path,
            configured_state_dir=path,
        )

        fixer, out = self.create_fixer(dry_run=False)
        mount_table = instance.mount_table

        edenfs_path = "/path/to/eden-mount"
        watchman_roots = {edenfs_path}
        watchman_info = check_watchman.WatchmanCheckInfo(watchman_roots)

        check_mount(
            out,
            fixer,
            # pyre-fixme[6]: For 3rd param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            instance,
            checkout_info,
            mount_table,
            watchman_info,
            [checkout_info],
            set(),
            set(),
            FakeNetworkChecker(),
            True,
            True,
        )

        self.assertEqual(
            out.getvalue(),
            f"""<yellow>- Found problem:<reset>
Eden's checkout state for {path} has been corrupted: [Errno 2] No such file or directory: 'SNAPSHOT'
To recover, you will need to remove and reclone the repo.
Your local commits will be unaffected, but reclones will lose uncommitted work or shelves.
However, the local changes are manually recoverable before the reclone.
If you have local changes you would like to save before reclone, see {get_local_commit_recovery_link()}, or reachout to the EdenFS team.
To reclone the corrupted repo, run: `fbclone $REPO --reclone --eden`
For additional info see the wiki at {get_doctor_link()}

""",
        )
        self.assertEqual(len(fixer.problem_types), 1)
        self.assertEqual(fixer.num_fixed_problems, 0)
        self.assertEqual(fixer.num_manual_fixes, 1)

    @unittest.skipIf(sys.platform == "win32", ".eden/root is Unix-only metadata")
    def test_logs_missing_eden_root_for_running_configured_checkout(self) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("eden-mount")
        checkout_info = CheckoutInfo(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            instance,
            checkout.path,
            running_state_dir=checkout.state_dir,
            configured_state_dir=checkout.state_dir,
            state=MountState.RUNNING,
        )

        doctor.log_missing_eden_root_mount_health_issues(
            typing.cast(EdenInstance, instance), [checkout_info]
        )

        checkout_path = str(checkout.path)
        self.assertEqual(1, len(instance.logged_samples))
        log_type, sample = instance.logged_samples[0]
        self.assertEqual("eden_mount_health_issue", log_type)
        self.assertEqual(
            {
                "source": "doctor_startup",
                "reason": "missing_eden_root",
                "mount_path": checkout_path,
                "checkout_path": checkout_path,
                "path_type": "configured_checkout",
                "attempted_repair": False,
                "success": False,
            },
            {key: sample[key] for key in sample if key != "error"},
        )
        self.assertIn(
            str(checkout.path / ".eden" / "root"), typing.cast(str, sample["error"])
        )

    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    # pyre-fixme[2]: Parameter must be annotated.
    def test_edenfs_no_warnings(self, mock_watchman) -> None:
        # Test that doctor will hide warnings this time (we know that this setup writes a warning from the previous test,
        # but this time we expect an empty output since we raised the minimum level to get anything)
        instance = FakeEdenInstance(
            self.make_temporary_directory(), status=fb303_status.STARTING
        )
        instance.create_test_mount("eden-mount")

        out = TestOutput()
        dry_run = False
        doctor.cure_what_ails_you(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            #  `FakeEdenInstance`.
            instance,
            dry_run,
            min_severity_to_report=ProblemSeverity.MELTDOWN,
            mount_table=FakeMountTable(),
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            network_checker=FakeNetworkChecker(),
            out=out,
        )

        self.assertRegex(
            out.getvalue(),
            r"""
""",
        )

    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    # pyre-fixme[2]: Parameter must be annotated.
    def test_edenfs_stopping(self, mock_watchman) -> None:
        instance = FakeEdenInstance(
            self.make_temporary_directory(), status=fb303_status.STOPPING
        )
        instance.create_test_mount("eden-mount")

        out = TestOutput()
        dry_run = False
        exit_code = doctor.cure_what_ails_you(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            #  `FakeEdenInstance`.
            instance,
            dry_run,
            min_severity_to_report=ProblemSeverity.ALL,
            mount_table=FakeMountTable(),
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            network_checker=FakeNetworkChecker(),
            out=out,
        )

        self.assertRegex(
            out.getvalue(),
            r"""<yellow>- Found problem:<reset>
EdenFS is currently shutting down\.
Either wait for edenfs to exit, or to forcibly kill EdenFS, run:

    eden stop --kill

<yellow>1 issue requires manual attention\.<reset>
Collect an 'eden rage' and ask in the EdenFS (Windows |macOS )?Users group if you need help fixing issues with EdenFS:
(https://fb\.workplace\.com/groups/eden\.users|https://fb\.workplace\.com/groups/edenfswindows|https://fb\.workplace\.com/groups/edenfsmacos)
""",
        )
        self.assertEqual(1, exit_code)

    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    # pyre-fixme[2]: Parameter must be annotated.
    def test_no_issue_when_watchman_using_eden_watcher(self, mock_watchman) -> None:
        fixer, out = self._test_watchman_watcher_check(
            mock_watchman, initial_watcher="eden"
        )
        self.assertEqual("", out)
        self.assert_results(fixer, num_problems=0)

    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    # pyre-fixme[2]: Parameter must be annotated.
    def test_fix_when_watchman_using_inotify_watcher(self, mock_watchman) -> None:
        fixer, out = self._test_watchman_watcher_check(
            mock_watchman, initial_watcher="inotify", new_watcher="eden", dry_run=False
        )
        self.assertEqual(
            (
                "<yellow>- Found problem:<reset>\n"
                "Watchman is watching /path/to/eden-mount with the wrong watcher type: "
                '"inotify" instead of "eden"\n'
                "Fixing watchman watch for /path/to/eden-mount...<green>fixed<reset>\n"
                "\n"
            ),
            out,
        )
        self.assert_results(fixer, num_problems=1, num_fixed_problems=1)

    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    def test_watchman_error_response(self, mock_watchman: MagicMock) -> None:
        edenfs_path = "/path/to/eden-mount"
        mock_watchman.side_effect = [{"error": "watch-project timed out"}]
        fixer, out = self.create_fixer(dry_run=False)
        watchman_info = check_watchman.WatchmanCheckInfo({edenfs_path})
        check_watchman.check_active_mount(fixer, edenfs_path, watchman_info)
        mock_watchman.assert_called_once_with(["watch-project", edenfs_path])
        self.assertEqual("", out.getvalue())
        self.assert_results(fixer, num_problems=0)

    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    def test_watchman_response_without_watcher(self, mock_watchman: MagicMock) -> None:
        edenfs_path = "/path/to/eden-mount"
        mock_watchman.side_effect = [{"version": "2026.10.20.00", "watch": edenfs_path}]
        fixer, out = self.create_fixer(dry_run=False)
        watchman_info = check_watchman.WatchmanCheckInfo({edenfs_path})
        check_watchman.check_active_mount(fixer, edenfs_path, watchman_info)
        mock_watchman.assert_called_once_with(["watch-project", edenfs_path])
        self.assertEqual(
            (
                "<yellow>- Found problem:<reset>\n"
                "Watchman returned an unexpected reply for "
                "`watch-project /path/to/eden-mount`, with no watcher: "
                '{"version": "2026.10.20.00", "watch": "/path/to/eden-mount"}\n'
                'Try running "watchman watch-del /path/to/eden-mount" and then '
                '"watchman watch-project /path/to/eden-mount"\n'
                "\n"
            ),
            out.getvalue(),
        )
        self.assert_results(fixer, num_problems=1, num_manual_fixes=1)

    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    # pyre-fixme[2]: Parameter must be annotated.
    def test_dry_run_identifies_inotify_watcher_issue(self, mock_watchman) -> None:
        fixer, out = self._test_watchman_watcher_check(
            mock_watchman, initial_watcher="inotify", dry_run=True
        )
        self.assertEqual(
            (
                "<yellow>- Found problem:<reset>\n"
                "Watchman is watching /path/to/eden-mount with the wrong watcher type: "
                '"inotify" instead of "eden"\n'
                "Would fix watchman watch for /path/to/eden-mount\n"
                "\n"
            ),
            out,
        )
        self.assert_results(fixer, num_problems=1)

    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    def test_doctor_reports_failure_if_cannot_replace_inotify_watcher(
        self,
        # pyre-fixme[2]: Parameter must be annotated.
        mock_watchman,
    ) -> None:
        fixer, out = self._test_watchman_watcher_check(
            mock_watchman,
            initial_watcher="inotify",
            new_watcher="inotify",
            dry_run=False,
        )
        self.assertIn(
            (
                "<yellow>- Found problem:<reset>\n"
                "Watchman is watching /path/to/eden-mount with the wrong watcher type: "
                '"inotify" instead of "eden"\n'
                "Fixing watchman watch for /path/to/eden-mount...<red>error<reset>\n"
                "Failed to fix or verify fix for problem IncorrectWatchmanWatch: RemediationError: Failed to replace "
                'watchman watch for /path/to/eden-mount with an "eden" watcher'
            ),
            "\n".join(out.split("\n")[:5]),
        )
        self.assert_results(fixer, num_problems=1, num_failed_fixes=1)

    def _test_watchman_watcher_check(
        self,
        # pyre-fixme[2]: Parameter must be annotated.
        mock_watchman,
        initial_watcher: str,
        new_watcher: Optional[str] = None,
        dry_run: bool = True,
    ) -> Tuple[doctor.ProblemFixer, str]:
        edenfs_path = "/path/to/eden-mount"
        side_effects: List[Dict[str, Any]] = []
        calls = []

        calls.append(call(["watch-project", edenfs_path]))
        side_effects.append({"watch": edenfs_path, "watcher": initial_watcher})

        if initial_watcher != "eden" and not dry_run:
            calls.append(call(["watch-del", edenfs_path]))
            side_effects.append({"watch-del": True, "root": edenfs_path})

            self.assertIsNotNone(
                new_watcher,
                msg='Must specify new_watcher when initial_watcher is "eden".',
            )
            calls.append(call(["watch-project", edenfs_path]))
            side_effects.append({"watch": edenfs_path, "watcher": new_watcher})
        mock_watchman.side_effect = side_effects

        fixer, out = self.create_fixer(dry_run)

        watchman_roots = {edenfs_path}
        watchman_info = check_watchman.WatchmanCheckInfo(watchman_roots)
        check_watchman.check_active_mount(fixer, edenfs_path, watchman_info)

        mock_watchman.assert_has_calls(calls)
        return fixer, out.getvalue()

    def test_snapshot_and_dirstate_file_match(self) -> None:
        dirstate_hash_hex = "12345678" * 5
        snapshot_hex = "12345678" * 5
        _checkout, fixer, out = self._test_hash_check(dirstate_hash_hex, snapshot_hex)
        self.assertEqual("", out)
        self.assert_results(fixer, num_problems=0)

    def test_snapshot_and_dirstate_file_differ(self) -> None:
        dirstate_hash_hex = "12000000" * 5
        snapshot_hex = "12345678" * 5
        checkout, fixer, out = self._test_hash_check(dirstate_hash_hex, snapshot_hex)
        self.assertEqual(
            f"""\
<yellow>- Found problem:<reset>
Found inconsistent/missing data in {checkout.hg_dot_path}:
  mercurial's parent commit is 1200000012000000120000001200000012000000, \
but Eden's internal parent commit is \
1234567812345678123456781234567812345678
Repairing hg directory contents for {checkout.path}...<green>fixed<reset>

""",
            out,
        )
        self.assert_results(fixer, num_problems=1, num_fixed_problems=1)
        # The dirstate file should have been updated to use the snapshot hash
        self.assertEqual(
            # pyre-fixme[16]: `EdenClient` has no attribute `set_parents_calls`.
            checkout.instance._fake_client.set_parents_calls,
            [],
        )
        self.assert_dirstate_p0(checkout, snapshot_hex)

    @patch("eden.fs.cli.config.EdenCheckout.get_snapshot")
    def test_snapshot_and_dirstate_file_differ_and_snapshot_invalid(
        self, mock_get_snapshot: MagicMock
    ) -> None:
        def check_commit_validity(commit: str) -> bool:
            if commit == "12345678" * 5:
                return False
            return True

        dirstate_hash_hex = "12000000" * 5
        snapshot_hex = "12345678" * 5

        def snapshot_state_factory(hash_hex: str) -> SnapshotState:
            return SnapshotState(hash_hex, hash_hex)

        mock_get_snapshot.side_effect = [
            snapshot_state_factory(snapshot_hex),
            snapshot_state_factory(snapshot_hex),
            snapshot_state_factory(dirstate_hash_hex),
        ]
        checkout, fixer, out = self._test_hash_check(
            dirstate_hash_hex, snapshot_hex, commit_checker=check_commit_validity
        )
        self.assertEqual(
            f"""\
<yellow>- Found problem:<reset>
Found inconsistent/missing data in {checkout.hg_dot_path}:
  Eden's snapshot file points to a bad commit: {snapshot_hex}
Repairing hg directory contents for {checkout.path}...<green>fixed<reset>

""",
            out,
        )
        self.assert_results(fixer, num_problems=1, num_fixed_problems=1)
        # Make sure resetParentCommits() was called once with the expected arguments
        self.assertEqual(
            # pyre-fixme[16]: `EdenClient` has no attribute `set_parents_calls`.
            checkout.instance._fake_client.set_parents_calls,
            [
                ResetParentsCommitsArgs(
                    mount=bytes(checkout.path),
                    parent1=b"\x12\x00\x00\x00" * 5,
                    parent2=None,
                    hg_root_manifest=None,
                    rootIdOptions=None,
                )
            ],
        )

    # pyre-fixme[56]: Pyre was not able to infer the type of argument
    #  `b"�eC!".__mul__(5)` to decorator factory `unittest.mock.patch`.
    @patch(
        "eden.fs.cli.doctor.check_hg.get_tip_commit_hash",
        return_value=b"\x87\x65\x43\x21" * 5,
    )
    @patch("eden.fs.cli.config.EdenCheckout.get_snapshot")
    @patch("eden.fs.cli.doctor.check_hg.DirstateChecker._is_commit_hash_valid")
    def test_snapshot_and_dirstate_file_differ_and_all_commit_hash_invalid(
        self,
        mock_is_commit_hash_valid: MagicMock,
        mock_get_snapshot: MagicMock,
        mock_get_tip_commit_hash: MagicMock,
    ) -> None:
        dirstate_hash_hex = "12000000" * 5
        snapshot_hex = "12345678" * 5
        valid_commit_hash = "87654321" * 5
        mock_is_commit_hash_valid.side_effect = [
            False,
            True,
            False,
            True,
            True,
            True,
        ]

        def snapshot_state_factory(hash_hex: str) -> SnapshotState:
            return SnapshotState(hash_hex, hash_hex)

        mock_get_snapshot.side_effect = [
            snapshot_state_factory(snapshot_hex),
            snapshot_state_factory(snapshot_hex),
            snapshot_state_factory(valid_commit_hash),
        ]
        checkout, fixer, out = self._test_hash_check(dirstate_hash_hex, snapshot_hex)

        self.assertEqual(
            f"""\
<yellow>- Found problem:<reset>
Found inconsistent/missing data in {checkout.hg_dot_path}:
  mercurial's p0 commit points to a bad commit: {dirstate_hash_hex}
  Eden's snapshot file points to a bad commit: {snapshot_hex}
Repairing hg directory contents for {checkout.path}...<green>fixed<reset>

""",
            out,
        )
        self.assert_results(fixer, num_problems=1, num_fixed_problems=1)
        # Make sure resetParentCommits() was called once with the expected arguments
        self.assertEqual(
            # pyre-fixme[16]: `EdenClient` has no attribute `set_parents_calls`.
            checkout.instance._fake_client.set_parents_calls,
            [
                ResetParentsCommitsArgs(
                    mount=bytes(checkout.path),
                    parent1=b"\x87\x65\x43\x21" * 5,
                    parent2=None,
                    hg_root_manifest=None,
                    rootIdOptions=None,
                )
            ],
        )
        self.assert_dirstate_p0(checkout, valid_commit_hash)

    # pyre-fixme[56]: Pyre was not able to infer the type of argument
    #  `b"�eC!".__mul__(5)` to decorator factory `unittest.mock.patch`.
    @patch(
        "eden.fs.cli.doctor.check_hg.get_tip_commit_hash",
        return_value=b"\x87\x65\x43\x21" * 5,
    )
    @patch("eden.fs.cli.config.EdenCheckout.get_snapshot")
    @patch("eden.fs.cli.doctor.check_hg.DirstateChecker._is_commit_hash_valid")
    def test_snapshot_and_dirstate_file_differ_and_all_parents_invalid(
        self,
        mock_is_commit_hash_valid: MagicMock,
        mock_get_snapshot: MagicMock,
        mock_get_tip_commit_hash: MagicMock,
    ) -> None:
        dirstate_hash_hex = "12000000" * 5
        dirstate_parent2_hash_hex = "12340000" * 5
        snapshot_hex = "12345678" * 5
        valid_commit_hash = "87654321" * 5

        mock_is_commit_hash_valid.side_effect = [
            False,
            False,
            False,
            True,
            True,
            True,
        ]

        def snapshot_state_factory(hash_hex: str) -> SnapshotState:
            return SnapshotState(hash_hex, hash_hex)

        mock_get_snapshot.side_effect = [
            snapshot_state_factory(snapshot_hex),
            snapshot_state_factory(snapshot_hex),
            snapshot_state_factory(valid_commit_hash),
        ]

        checkout, fixer, out = self._test_hash_check(
            dirstate_hash_hex,
            snapshot_hex,
            dirstate_parent2_hash_hex,
        )

        self.assertEqual(
            f"""\
<yellow>- Found problem:<reset>
Found inconsistent/missing data in {checkout.hg_dot_path}:
  mercurial's p0 commit points to a bad commit: {dirstate_hash_hex}
  mercurial's p1 commit points to a bad commit: {dirstate_parent2_hash_hex}
  Eden's snapshot file points to a bad commit: {snapshot_hex}
Repairing hg directory contents for {checkout.path}...<green>fixed<reset>

""",
            out,
        )
        self.assert_results(fixer, num_problems=1, num_fixed_problems=1)
        # Make sure resetParentCommits() was called once with the expected arguments
        self.assertEqual(
            # pyre-fixme[16]: `EdenClient` has no attribute `set_parents_calls`.
            checkout.instance._fake_client.set_parents_calls,
            [
                ResetParentsCommitsArgs(
                    mount=bytes(checkout.path),
                    parent1=b"\x87\x65\x43\x21" * 5,
                    parent2=None,
                    hg_root_manifest=None,
                    rootIdOptions=None,
                )
            ],
        )
        self.assert_dirstate_p0(checkout, valid_commit_hash)

    def test_snapshot_and_dirstate_file_differ_and_dirstate_commit_hash_invalid(
        self,
    ) -> None:
        def check_commit_validity(commit: str) -> bool:
            if commit == "12000000" * 5:
                return False
            return True

        dirstate_hash_hex = "12000000" * 5
        snapshot_hex = "12345678" * 5
        checkout, fixer, out = self._test_hash_check(
            dirstate_hash_hex, snapshot_hex, commit_checker=check_commit_validity
        )

        self.assertEqual(
            f"""\
<yellow>- Found problem:<reset>
Found inconsistent/missing data in {checkout.hg_dot_path}:
  mercurial's p0 commit points to a bad commit: {dirstate_hash_hex}
Repairing hg directory contents for {checkout.path}...<green>fixed<reset>

""",
            out,
        )
        self.assert_results(fixer, num_problems=1, num_fixed_problems=1)
        # The dirstate file should have been updated to use the snapshot hash
        self.assertEqual(
            # pyre-fixme[16]: `EdenClient` has no attribute `set_parents_calls`.
            checkout.instance._fake_client.set_parents_calls,
            [],
        )
        self.assert_dirstate_p0(checkout, snapshot_hex)

    def _test_hash_check(
        self,
        dirstate_hash_hex: str,
        snapshot_hex: str,
        # pyre-fixme[2]: Parameter must be annotated.
        dirstate_parent2_hash_hex=None,
        commit_checker: Optional[Callable[[str], bool]] = None,
    ) -> Tuple[EdenCheckout, doctor.ProblemFixer, str]:
        instance = FakeEdenInstance(self.make_temporary_directory())
        if dirstate_parent2_hash_hex is None:
            checkout = instance.create_test_mount(
                "path1", snapshot=snapshot_hex, dirstate_parent=dirstate_hash_hex
            )
        else:
            checkout = instance.create_test_mount(
                "path1",
                snapshot=snapshot_hex,
                dirstate_parent=(dirstate_hash_hex, dirstate_parent2_hash_hex),
            )

        hg_repo = checkout.instance.get_hg_repo(checkout.path)
        if commit_checker and hg_repo is not None:
            fake_hg_repo = typing.cast(FakeHgRepo, hg_repo)
            fake_hg_repo.commit_checker = commit_checker

        fixer, out = self.create_fixer(dry_run=False)
        check_hg.check_hg(fixer, checkout)
        return checkout, fixer, out.getvalue()

    @patch("eden.fs.cli.version.get_current_version_parts")
    def test_edenfs_when_installed_and_running_match(
        self, mock_getver: MagicMock
    ) -> None:
        fixer, out = self._test_edenfs_version(mock_getver, ("20171213", "165642"))
        self.assertEqual("", out)
        self.assert_results(fixer, num_problems=0)

    @patch("eden.fs.cli.version.get_current_version_parts")
    def test_edenfs_when_installed_and_running_recent(
        self, mock_getver: MagicMock
    ) -> None:
        fixer, out = self._test_edenfs_version(mock_getver, ("20171220", "165643"))
        self.assertEqual("", out)
        self.assert_results(fixer, num_problems=0)

    @patch("eden.fs.cli.version.get_current_version_parts")
    def test_edenfs_when_installed_and_running_old(
        self, mock_getver: MagicMock
    ) -> None:
        fixer, out = self._test_edenfs_version(mock_getver, ("20171227", "246561"))
        self.assertRegex(
            out,
            r"""<yellow>- Found problem:<reset>
The version of EdenFS that is installed on your machine is:
    fb.eden.20171227-246561(\.x86_64)?
but the version of EdenFS that is currently running is:
    fb.eden.20171213-165642(\.x86_64)?

Consider running `edenfsctl restart( --graceful)?` to migrate to the newer version,
which may have important bug fixes or performance improvements\.

""",
        )
        self.assert_results(fixer, num_problems=1, num_advisory_fixes=1)

    @patch("eden.fs.cli.version.get_current_version_parts")
    def test_edenfs_when_installed_and_running_old_likely_automation(
        self, mock_getver: MagicMock
    ) -> None:
        fixer, out = self._test_edenfs_version(
            mock_getver, ("20171227", "246561"), likely_automation=True
        )
        self.assertEqual("", out)
        self.assert_results(fixer, num_problems=0)

    def _test_edenfs_version(
        self,
        mock_rpm_q: MagicMock,
        rpm_value: Tuple[str, str],
        likely_automation: bool = False,
    ) -> Tuple[doctor.ProblemFixer, str]:
        side_effects: List[Tuple[str, str]] = []
        calls = []
        calls.append(call())
        side_effects.append(rpm_value)
        mock_rpm_q.side_effect = side_effects

        instance = FakeEdenInstance(
            self.make_temporary_directory(),
            build_info={
                "build_package_version": "20171213",
                "build_package_release": "165642",
            },
        )
        fixer, out = self.create_fixer(dry_run=False)
        doctor.check_edenfs_version(
            fixer, typing.cast(EdenInstance, instance), likely_automation
        )
        if likely_automation:
            mock_rpm_q.assert_not_called()
        else:
            mock_rpm_q.assert_has_calls(calls)
        return fixer, out.getvalue()

    def test_remount_checkouts(self) -> None:
        exit_code, out, mounts = self._test_remount_checkouts(dry_run=False)
        self.assertEqual(
            f"""\
Checking {mounts[0]}
Checking {mounts[1]}
<yellow>- Found problem:<reset>
{mounts[1]} is not currently mounted
Remounting {mounts[1]}...<green>fixed<reset>

<yellow>Successfully fixed 1 problem.<reset>
""",
            out,
        )
        self.assertEqual(exit_code, 0)

    def test_remount_checkouts_dry_run(self) -> None:
        exit_code, out, mounts = self._test_remount_checkouts(
            dry_run=True,
        )
        self.assertEqual(
            f"""\
Checking {mounts[0]}
Checking {mounts[1]}
<yellow>- Found problem:<reset>
{mounts[1]} is not currently mounted
Would remount {mounts[1]}

<yellow>Discovered 1 problem during --dry-run<reset>
""",
            out,
        )
        self.assertEqual(exit_code, 1)

    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    def _test_remount_checkouts(
        self,
        # pyre-fixme[2]: Parameter must be annotated.
        mock_watchman,
        dry_run: bool,
    ) -> Tuple[int, str, List[Path]]:
        """Test that `eden doctor` remounts configured mount points that are not
        currently mounted.
        """
        tmp_dir = self.make_temporary_directory()
        instance = FakeEdenInstance(tmp_dir)

        mounts = []
        mount1 = instance.create_test_mount("path1")
        mounts.append(mount1.path)
        mounts.append(instance.create_test_mount("path2", active=False).path)

        out = TestOutput()
        exit_code = doctor.cure_what_ails_you(
            typing.cast(EdenInstance, instance),
            dry_run,
            min_severity_to_report=ProblemSeverity.ALL,
            mount_table=instance.mount_table,
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            network_checker=FakeNetworkChecker(),
            out=out,
        )
        return exit_code, out.getvalue(), mounts

    def _run_doctor_with_unreadable_config_json(
        self, instance: FakeEdenInstance
    ) -> Tuple[int, str]:
        # The error EdenInstance._get_directory_map() raises for a truncated
        # config.json.
        error = Exception(
            f"invalid JSON data found in {instance.state_dir}/config.json"
        )
        out = TestOutput()
        with (
            patch.object(instance, "get_mount_paths", side_effect=error),
            patch.object(instance, "get_checkouts", side_effect=error),
        ):
            exit_code = doctor.cure_what_ails_you(
                typing.cast(EdenInstance, instance),
                dry_run=True,
                min_severity_to_report=ProblemSeverity.ALL,
                mount_table=instance.mount_table,
                fs_util=FakeFsUtil(),
                proc_utils=self.make_proc_utils(),
                vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
                network_checker=FakeNetworkChecker(),
                out=out,
            )
        return exit_code, out.getvalue()

    @patch("eden.fs.cli.doctor.check_filesystems.check_disk_usage")
    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    def test_unreadable_config_json_with_edenfs_running(
        self, mock_watchman: MagicMock, mock_check_disk_usage: MagicMock
    ) -> None:
        mock_watchman.return_value = {"roots": []}
        instance = FakeEdenInstance(self.make_temporary_directory())
        instance.create_test_mount("path1")

        exit_code, out = self._run_doctor_with_unreadable_config_json(instance)

        self.assertIn("Could not read the list of configured EdenFS checkouts", out)
        self.assertIn(f"invalid JSON data found in {instance.state_dir}", out)
        self.assertIn(
            f"Make sure {instance.state_dir / 'config.json'} can be read and holds",
            out,
        )
        # The disk usage check still covers the state and temp directories.
        mock_check_disk_usage.assert_called_once()
        self.assertEqual(mock_check_disk_usage.call_args.args[1], [])
        self.assertEqual(exit_code, 1)

    @patch("eden.fs.cli.doctor.check_filesystems.check_disk_usage")
    @patch("eden.fs.cli.doctor.check_stale_mounts.check_for_stale_mounts")
    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    def test_unreadable_config_json_with_edenfs_not_running(
        self,
        mock_watchman: MagicMock,
        mock_check_for_stale_mounts: MagicMock,
        mock_check_disk_usage: MagicMock,
    ) -> None:
        mock_watchman.return_value = {"roots": []}
        instance = FakeEdenInstance(
            self.make_temporary_directory(), status=fb303_status.DEAD
        )
        instance.create_test_mount("path1", active=False)

        exit_code, out = self._run_doctor_with_unreadable_config_json(instance)

        self.assertIn("Could not read the list of configured EdenFS checkouts", out)
        self.assertIn(f"invalid JSON data found in {instance.state_dir}", out)
        # Without the list, doctor cannot tell whether EdenFS is in use, so it
        # does not ask for EdenFS to be started; checks that do not need the
        # list still run.
        self.assertNotIn("EdenFS is not running", out)
        mock_check_for_stale_mounts.assert_called_once()
        mock_check_disk_usage.assert_called_once()
        self.assertEqual(mock_check_disk_usage.call_args.args[1], [])
        self.assertEqual(exit_code, 1)

    def test_unmounted_checkout_missing_config_dry_run(self) -> None:
        exit_code, out, instance, checkout = (
            self._test_unmounted_checkout_missing_config(dry_run=True)
        )
        self.assertIn(
            f"{checkout.path} is configured, but EdenFS cannot mount it because "
            "its checkout config could not be read:\n",
            out,
        )
        self.assertIn(f"{checkout.state_dir / 'config.toml'} does not exist", out)
        self.assertIn(f"To remove it, run: `eden rm {checkout.path}`", out)
        self.assertNotIn("unexpected error", out)
        self.assertNotIn("AssertionError", out)
        self.assertNotIn("Would remount", out)
        self.assertEqual(exit_code, 1)

    def test_unmounted_checkout_missing_config_is_not_remounted(self) -> None:
        exit_code, out, instance, checkout = (
            self._test_unmounted_checkout_missing_config(dry_run=False)
        )
        self.assertIn(f"{checkout.path} is configured, but EdenFS cannot mount it", out)
        self.assertIn("1 issue requires manual attention.", out)
        self.assertNotIn("unexpected error", out)
        self.assertEqual(instance.mount_calls, [])
        self.assertEqual(exit_code, 1)

    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    def _test_unmounted_checkout_missing_config(
        self,
        mock_watchman: MagicMock,
        dry_run: bool,
    ) -> Tuple[int, str, FakeEdenInstance, EdenCheckout]:
        """A checkout listed in config.json whose client directory has lost its
        config.toml (e.g. after an interrupted `eden rm`). EdenFS skips it on
        startup, so it is configured but not mounted, and its backing repo is
        unknown.
        """
        tmp_dir = self.make_temporary_directory()
        instance = FakeEdenInstance(tmp_dir)
        instance.create_test_mount("path1")
        checkout = instance.create_test_mount("path2", active=False)
        os.unlink(checkout.state_dir / "config.toml")

        out = TestOutput()
        exit_code = doctor.cure_what_ails_you(
            typing.cast(EdenInstance, instance),
            dry_run,
            min_severity_to_report=ProblemSeverity.ALL,
            mount_table=instance.mount_table,
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            network_checker=FakeNetworkChecker(),
            out=out,
        )
        return exit_code, out.getvalue(), instance, checkout

    def test_get_dependent_repos_skips_checkout_with_unreadable_config(self) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        healthy = instance.create_test_mount("path1", active=False)
        broken = instance.create_test_mount("path2", active=False)
        os.unlink(broken.state_dir / "config.toml")
        checkouts = get_checkouts_info(typing.cast(EdenInstance, instance))

        self.assertEqual(
            get_dependent_repos(
                instance.default_backing_repo, list(checkouts.values())
            ),
            [healthy.path],
        )

    def test_get_checkouts_info_keeps_checkout_with_unreadable_config(self) -> None:
        # `eden health-report` relies on such checkouts still being listed so
        # it can report them as not mounted.
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1", active=False)
        os.unlink(checkout.state_dir / "config.toml")

        checkouts = get_checkouts_info(typing.cast(EdenInstance, instance))

        info = checkouts[checkout.path]
        self.assertIsNone(info.state)
        self.assertIsNone(info.backing_repo)
        self.assertIn("config.toml does not exist", str(info.config_error))

    @patch("eden.fs.cli.doctor.check_watchman._call_watchman")
    # pyre-fixme[2]: Parameter must be annotated.
    def test_watchman_fails(self, mock_watchman) -> None:
        tmp_dir = self.make_temporary_directory()
        instance = FakeEdenInstance(tmp_dir)

        mount = instance.create_test_mount("path1", active=False).path

        # Make calls to watchman fail rather than returning expected output
        side_effects = [{"error": "watchman failed"}]
        mock_watchman.side_effect = side_effects

        out = TestOutput()
        exit_code = doctor.cure_what_ails_you(
            typing.cast(EdenInstance, instance),
            dry_run=False,
            min_severity_to_report=ProblemSeverity.ALL,
            mount_table=instance.mount_table,
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            network_checker=FakeNetworkChecker(),
            out=out,
        )

        # "watchman watch-list" should have been called by the doctor code
        calls = [call(["watch-list"])]
        mock_watchman.assert_has_calls(calls)

        self.assertEqual(
            out.getvalue(),
            f"""\
Checking {mount}
<yellow>- Found problem:<reset>
{mount} is not currently mounted
Remounting {mount}...<green>fixed<reset>

<yellow>Successfully fixed 1 problem.<reset>
""",
        )
        self.assertEqual(exit_code, 0)

    def test_pwd_out_of_date(self) -> None:
        tmp_dir = self.make_temporary_directory()
        instance = FakeEdenInstance(tmp_dir)
        mount = instance.create_test_mount("path1").path

        exit_code, out = self._test_with_pwd(instance, pwd=tmp_dir)
        self.assertRegex(
            out,
            r"""<yellow>- Found problem:<reset>
Your current working directory is out-of-date\.
This can happen if you have \(re\)started EdenFS but your shell is still pointing to
the old directory from before the EdenFS checkouts were mounted\.

Run "cd / && cd -" to update your shell's working directory\.

Checking .*
<yellow>1 issue requires manual attention\.<reset>
Collect an 'eden rage' and ask in the EdenFS (Windows |macOS )?Users group if you need help fixing issues with EdenFS:
(https://fb\.workplace\.com/groups/eden\.users|https://fb\.workplace\.com/groups/edenfswindows|https://fb\.workplace\.com/groups/edenfsmacos)
""",
        )
        self.assertEqual(1, exit_code)

    def test_pwd_not_set(self) -> None:
        tmp_dir = self.make_temporary_directory()
        instance = FakeEdenInstance(tmp_dir)
        mount = instance.create_test_mount("path1").path

        exit_code, out = self._test_with_pwd(instance, pwd=None)
        self.assertEqual(
            out,
            f"""\
Checking {mount}
<green>No issues detected.<reset>
""",
        )
        self.assertEqual(0, exit_code)

    def _test_with_pwd(
        self, instance: "FakeEdenInstance", pwd: Optional[str]
    ) -> Tuple[int, str]:
        if pwd is None:
            old_pwd = os.environ.pop("PWD", None)
        else:
            old_pwd = os.environ.get("PWD")
            os.environ["PWD"] = pwd
        try:
            out = TestOutput()
            exit_code = doctor.cure_what_ails_you(
                typing.cast(EdenInstance, instance),
                dry_run=False,
                min_severity_to_report=ProblemSeverity.ALL,
                mount_table=instance.mount_table,
                fs_util=FakeFsUtil(),
                proc_utils=self.make_proc_utils(),
                vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
                network_checker=FakeNetworkChecker(),
                out=out,
            )
            return exit_code, out.getvalue()
        finally:
            if old_pwd is not None:
                os.environ["PWD"] = old_pwd

    @patch(
        "eden.fs.cli.doctor.test.lib.fake_eden_instance.FakeEdenInstance.check_privhelper_connection",
        return_value=True,
    )
    def test_privhelper_check_accessible(
        self,
        # pyre-fixme[2]: Parameter must be annotated.
        mock_check_privhelper_connection,
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
            min_severity_to_report=ProblemSeverity.ALL,
            mount_table=instance.mount_table,
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            network_checker=FakeNetworkChecker(),
            out=out,
        )

        self.assertEqual(
            f"""\
Checking {mount}
<green>No issues detected.<reset>
""",
            out.getvalue(),
        )
        self.assertEqual(0, exit_code)

    @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.debugInodeStatus")
    # pyre-fixme[2]: Parameter must be annotated.
    def test_accessible_are_inodes(self, mock_debugInodeStatus) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")
        mount = checkout.path

        os.makedirs(mount / "a" / "b")

        mock_debugInodeStatus.return_value = [
            # Pretend that a/b is a file (it's a directory)
            TreeInodeDebugInfo(
                inodeNumber=1,
                path=b"a",
                materialized=True,
                treeHash=b"abcd",
                entries=[],
                refcount=1,
            ),
            # a/b is now missing from inodes
        ]

        tracker = ProblemCollector(instance)
        check_materialized_are_accessible(
            tracker,
            typing.cast(EdenInstance, instance),
            checkout,
            lambda p: os.lstat(p).st_mode,
        )

        self.assertEqual(
            tracker.problems[0].description(),
            f"{Path('a/b')} is not known to EdenFS but is accessible on disk",
        )

    @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.debugInodeStatus")
    # pyre-fixme[2]: Parameter must be annotated.
    def test_inaccessible_materialized(self, mock_debugInodeStatus) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")
        mount = checkout.path

        os.makedirs(mount / "a")
        b = mount / "a" / "b"
        b.touch()

        mock_debugInodeStatus.return_value = [
            TreeInodeDebugInfo(
                inodeNumber=1,
                path=b"a",
                materialized=True,
                treeHash=b"abcd",
                entries=[
                    TreeInodeEntryDebugInfo(
                        name=b"b",
                        inodeNumber=2,
                        mode=stat.S_IFREG,
                        loaded=True,
                        materialized=True,
                        hash=b"dcba",
                    )
                ],
                refcount=1,
            ),
        ]

        def get_mode(path: Path) -> int:
            if path.name == "b":
                raise PermissionError("Permission denied")
            else:
                return os.lstat(path).st_mode

        tracker = ProblemCollector(instance)
        check_materialized_are_accessible(
            tracker, typing.cast(EdenInstance, instance), checkout, get_mode
        )

        self.assertEqual(
            tracker.problems[0].description(),
            f"{Path('a/b')} is inaccessible despite EdenFS believing it should be: Permission denied",
        )

    @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.debugInodeStatus")
    # pyre-fixme[2]: Parameter must be annotated.
    def test_materialized_are_accessible(self, mock_debugInodeStatus) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")
        mount = checkout.path

        # Just create a/b folders
        os.makedirs(mount / "a" / "b")

        mock_debugInodeStatus.return_value = [
            # Pretend that a/b is a file (it's a directory)
            TreeInodeDebugInfo(
                inodeNumber=1,
                path=b"a",
                materialized=True,
                treeHash=b"abcd",
                entries=[
                    TreeInodeEntryDebugInfo(
                        name=b"b",
                        inodeNumber=2,
                        mode=stat.S_IFREG,
                        loaded=False,
                        materialized=True,
                        hash=b"dcba",
                    ),
                    TreeInodeEntryDebugInfo(
                        name=b"d",
                        inodeNumber=4,
                        mode=stat.S_IFREG,
                        loaded=False,
                        materialized=False,
                        hash=b"efgh",
                    ),
                    TreeInodeEntryDebugInfo(
                        name=b"d",
                        inodeNumber=5,
                        mode=stat.S_IFREG,
                        loaded=False,
                        materialized=False,
                        hash=b"efgh",
                    ),
                ],
                refcount=1,
            ),
            # Pretend that a/b/c is a directory (it doesn't exist)
            TreeInodeDebugInfo(
                inodeNumber=2,
                path=b"a/b",
                materialized=True,
                treeHash=b"dcba",
                entries=[
                    TreeInodeEntryDebugInfo(
                        name=b"c",
                        inodeNumber=3,
                        mode=stat.S_IFREG,
                        loaded=False,
                        materialized=True,
                        hash=b"1234",
                    )
                ],
                refcount=1,
            ),
        ]

        tracker = ProblemCollector(instance)
        check_materialized_are_accessible(
            tracker,
            typing.cast(EdenInstance, instance),
            checkout,
            lambda p: os.lstat(p).st_mode,
        )

        problemDescriptions = {problem.description() for problem in tracker.problems}
        self.assertEqual(
            problemDescriptions,
            {
                f"""\
{Path("a/d")} is not present on disk despite EdenFS believing it should be
{Path("a/b/c")} is not present on disk despite EdenFS believing it should be""",
                f"{Path('a/d')} is duplicated in EdenFS",
                f"{Path('a/b')} has an unexpected file type: known to EdenFS as a file, but is a directory on disk",
            },
        )

    @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.debugInodeStatus")
    def test_materialized_different_mode_fixer(
        self, mock_debugInodeStatus: MagicMock
    ) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")
        mount: Path = checkout.path

        # Just create a/b folders
        os.makedirs(mount / "a" / "b")

        mock_debugInodeStatus.side_effect = [
            # Pretend that a/b is a file (it's a directory)
            [
                TreeInodeDebugInfo(
                    inodeNumber=1,
                    path=b"a",
                    materialized=True,
                    treeHash=b"abcd",
                    entries=[
                        TreeInodeEntryDebugInfo(
                            name=b"b",
                            inodeNumber=2,
                            mode=stat.S_IFREG,
                            loaded=False,
                            materialized=True,
                            hash=b"dcba",
                        ),
                    ],
                    refcount=1,
                )
            ],
            # now report it as a directory
            [
                TreeInodeDebugInfo(
                    inodeNumber=1,
                    path=b"a",
                    materialized=True,
                    treeHash=b"abcd",
                    entries=[
                        TreeInodeEntryDebugInfo(
                            name=b"b",
                            inodeNumber=2,
                            mode=stat.S_IFDIR,
                            loaded=False,
                            materialized=True,
                            hash=b"dcba",
                        ),
                    ],
                    refcount=1,
                )
            ],
        ]

        fixer, output = self.create_fixer(dry_run=False)
        check_materialized_are_accessible(
            fixer,
            typing.cast(EdenInstance, instance),
            checkout,
            lambda p: os.lstat(p).st_mode,
        )

        self.assertEqual(
            f"""<yellow>- Found problem:<reset>
{Path("a/b")} has an unexpected file type: known to EdenFS as a file, but is a directory on disk
Fixing mismatched files/directories in {Path(mount)}...<green>fixed<reset>

""",
            output.getvalue(),
        )
        self.assert_results(fixer, num_problems=1, num_fixed_problems=1)

    @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.debugInodeStatus")
    def test_materialized_directory_that_is_a_symlink_on_disk(
        self, mock_debugInodeStatus: MagicMock
    ) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")
        mount: Path = checkout.path

        os.makedirs(mount / "p")
        os.makedirs(mount / "real")
        os.symlink(mount / "real", mount / "p" / "a")

        mock_debugInodeStatus.return_value = [
            TreeInodeDebugInfo(
                inodeNumber=1,
                path=b"p",
                materialized=True,
                treeHash=b"abcd",
                entries=[
                    TreeInodeEntryDebugInfo(
                        name=b"a",
                        inodeNumber=2,
                        mode=stat.S_IFDIR,
                        loaded=True,
                        materialized=True,
                        hash=b"dcba",
                    ),
                ],
                refcount=1,
            ),
            TreeInodeDebugInfo(
                inodeNumber=2,
                path=b"p/a",
                materialized=True,
                treeHash=b"abcd",
                entries=[],
                refcount=1,
            ),
        ]

        fixer, output = self.create_fixer(dry_run=False)
        check_materialized_are_accessible(
            fixer,
            typing.cast(EdenInstance, instance),
            checkout,
            lambda p: os.lstat(p).st_mode,
        )

        self.assertRegex(
            output.getvalue(),
            r"""<yellow>- Found problem:<reset>
.* has an unexpected file type: known to EdenFS as a directory, but is a symlink on disk
Fixing mismatched files/directories in .*...<red>error<reset>
Failed to fix or verify fix for problem MaterializedInodesHaveDifferentModeOnDisk: RemediationError: Failed check for MaterializedInodesHaveDifferentModeOnDisk failed:
Path .* is a symlink on disk but directory in eden
(.|\n)*""",
        )
        self.assert_results(fixer, num_problems=1, num_failed_fixes=1)

    @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.debugInodeStatus")
    def test_materialized_directory_that_is_a_file_on_disk(
        self, mock_debugInodeStatus: MagicMock
    ) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")
        mount: Path = checkout.path

        (mount / "f").touch()

        mock_debugInodeStatus.return_value = [
            TreeInodeDebugInfo(
                inodeNumber=1,
                path=b"f",
                materialized=True,
                treeHash=b"abcd",
                entries=[],
                refcount=1,
            ),
        ]

        tracker = ProblemCollector(instance)
        check_materialized_are_accessible(
            tracker,
            typing.cast(EdenInstance, instance),
            checkout,
            lambda p: os.lstat(p).st_mode,
        )
        self.assertEqual(
            [problem.description() for problem in tracker.problems],
            [
                f"{Path('f')} has an unexpected file type: known to EdenFS as a directory, but is a file on disk"
            ],
        )

    @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.debugInodeStatus")
    def test_materialized_different_mode_fixer_fail(
        self, mock_debugInodeStatus: MagicMock
    ) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")
        mount: Path = checkout.path

        # Just create a/b folders
        os.makedirs(mount / "a" / "b")

        # Pretend that a/b is a file (it's a directory)
        mock_debugInodeStatus.return_value = [
            TreeInodeDebugInfo(
                inodeNumber=1,
                path=b"a",
                materialized=True,
                treeHash=b"abcd",
                entries=[
                    TreeInodeEntryDebugInfo(
                        name=b"b",
                        inodeNumber=2,
                        mode=stat.S_IFREG,
                        loaded=False,
                        materialized=True,
                        hash=b"dcba",
                    ),
                ],
                refcount=1,
            )
        ]

        fixer, output = self.create_fixer(dry_run=False)
        check_materialized_are_accessible(
            fixer,
            typing.cast(EdenInstance, instance),
            checkout,
            lambda p: os.lstat(p).st_mode,
        )

        self.assertRegex(
            output.getvalue(),
            r"""<yellow>- Found problem:<reset>
.* has an unexpected file type: known to EdenFS as a file, but is a directory on disk
Fixing mismatched files/directories in .*...<red>error<reset>
Failed to fix or verify fix for problem MaterializedInodesHaveDifferentModeOnDisk: RemediationError: Failed check for MaterializedInodesHaveDifferentModeOnDisk failed:
Path .* is a directory on disk but file in eden
(.|\n)*""",
        )
        self.assert_results(fixer, num_problems=1, num_failed_fixes=1)

    @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.debugInodeStatus")
    @patch("eden.fs.cli.doctor.check_filesystems.MissingFilesForInodes.perform_fix")
    def test_materialized_missing_file_fixer(
        self, mock_perform_fix: MagicMock, mock_debugInodeStatus: MagicMock
    ) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")
        mount: Path = checkout.path

        # Just create a folders
        os.makedirs(mount / "a")

        def side_effect() -> None:
            (mount / "a" / "d").touch()

        mock_perform_fix.side_effect = side_effect

        mock_debugInodeStatus.return_value = [
            # Pretend that a/d is a file (it doesn't exist)
            TreeInodeDebugInfo(
                inodeNumber=1,
                path=b"a",
                materialized=True,
                treeHash=b"abcd",
                entries=[
                    TreeInodeEntryDebugInfo(
                        name=b"d",
                        inodeNumber=4,
                        mode=stat.S_IFREG,
                        loaded=False,
                        materialized=False,
                        hash=b"efgh",
                    ),
                ],
                refcount=1,
            ),
        ]

        fixer, output = self.create_fixer(dry_run=False)
        check_materialized_are_accessible(
            fixer,
            typing.cast(EdenInstance, instance),
            checkout,
            lambda p: os.lstat(p).st_mode,
        )
        mock_perform_fix.assert_called_once()

        self.assertEqual(
            f"""<yellow>- Found problem:<reset>
{Path("a/d")} is not present on disk despite EdenFS believing it should be
Fixing files known to EdenFS but not present on disk in {Path(mount)}...<green>fixed<reset>

""",
            output.getvalue(),
        )
        self.assert_results(fixer, num_problems=1, num_fixed_problems=1)

    @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.debugInodeStatus")
    def test_materialized_missing_inode_fixer(
        self, mock_debugInodeStatus: MagicMock
    ) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")
        mount: Path = checkout.path

        os.makedirs(mount / "a" / "b")

        mock_debugInodeStatus.return_value = [
            # Pretend that a/b is a file (it's a directory)
            TreeInodeDebugInfo(
                inodeNumber=1,
                path=b"a",
                materialized=True,
                treeHash=b"abcd",
                entries=[],
                refcount=1,
            ),
            # a/b is now missing from inodes
        ]

        fixer, output = self.create_fixer(dry_run=False)
        check_materialized_are_accessible(
            fixer,
            typing.cast(EdenInstance, instance),
            checkout,
            lambda p: os.lstat(p).st_mode,
        )

        self.assertEqual(
            f"""<yellow>- Found problem:<reset>
{Path("a/b")} is not known to EdenFS but is accessible on disk
Fixing files present on disk but not known to EdenFS in {Path(mount)}...<green>fixed<reset>

""",
            output.getvalue(),
        )
        self.assert_results(fixer, num_problems=1, num_fixed_problems=1)

    if sys.platform == "win32":

        @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.debugInodeStatus")
        @patch("eden.fs.cli.doctor.check_filesystems.MissingFilesForInodes.perform_fix")
        def test_loaded_missing_file_fixer(
            self, mock_perform_fix, mock_debugInodeStatus
        ) -> None:
            instance = FakeEdenInstance(self.make_temporary_directory())
            checkout = instance.create_test_mount("path1")
            mount = checkout.path

            # Just create a folders
            os.makedirs(mount / "a")

            def side_effect():
                (mount / "a" / "d").touch()

            mock_perform_fix.side_effect = side_effect

            mock_debugInodeStatus.return_value = [
                # Pretend that a/d is a file (it doesn't exist)
                TreeInodeDebugInfo(
                    inodeNumber=1,
                    path=b"a",
                    materialized=True,
                    treeHash=b"abcd",
                    entries=[
                        TreeInodeEntryDebugInfo(
                            name=b"d",
                            inodeNumber=4,
                            mode=stat.S_IFREG,
                            loaded=False,
                            materialized=False,
                            hash=b"efgh",
                        ),
                    ],
                    refcount=1,
                ),
            ]

            fake_PrjGetOnDiskFileState = MagicMock()
            fake_PrjGetOnDiskFileState.side_effect = [
                FileNotFoundError,
                PRJ_FILE_STATE.HydratedPlaceholder,
            ]

            fixer, output = self.create_fixer(dry_run=False)
            check_loaded_content(
                fixer,
                typing.cast(EdenInstance, instance),
                checkout,
                fake_PrjGetOnDiskFileState,
            )
            mock_perform_fix.assert_called_once()

            self.assertEqual(
                f"""<yellow>- Found problem:<reset>
{Path("a/d")} is not present on disk despite EdenFS believing it should be
Fixing files known to EdenFS but not present on disk in {Path(mount)}...<green>fixed<reset>

""",
                output.getvalue(),
            )
            self.assert_results(fixer, num_problems=1, num_fixed_problems=1)

        @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.debugInodeStatus")
        def test_loaded_missing_inode_fixer(
            self, mock_debugInodeStatus: MagicMock
        ) -> None:
            instance = FakeEdenInstance(self.make_temporary_directory())
            checkout = instance.create_test_mount("path1")
            mount = checkout.path

            unmaterialized = checkout.path / "unmaterialized"
            os.makedirs(unmaterialized)
            with open(unmaterialized / "extra", "wb") as f:
                f.write(b"read all about it")

            mock_debugInodeStatus.return_value = [
                TreeInodeDebugInfo(
                    inodeNumber=3,
                    path=b"unmaterialized",
                    materialized=False,
                    treeHash=b"bcde",
                    entries=[],
                    refcount=1,
                ),
            ]

            fake_PrjGetOnDiskFileState = MagicMock()
            fake_PrjGetOnDiskFileState.side_effect = [
                FileNotFoundError,
                PRJ_FILE_STATE.HydratedPlaceholder,
            ]

            fixer, output = self.create_fixer(dry_run=False)
            check_loaded_content(
                fixer,
                typing.cast(EdenInstance, instance),
                checkout,
                fake_PrjGetOnDiskFileState,
            )

            self.assertEqual(
                f"""<yellow>- Found problem:<reset>
{Path("unmaterialized/extra")} is not known to EdenFS but is accessible on disk
Fixing files present on disk but not known to EdenFS in {Path(mount)}...<green>fixed<reset>

""",
                output.getvalue(),
            )
            self.assert_results(fixer, num_problems=1, num_fixed_problems=1)

        @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.debugInodeStatus")
        def test_materialized_different_case(self, mock_debugInodeStatus) -> None:
            instance = FakeEdenInstance(self.make_temporary_directory())
            checkout = instance.create_test_mount("path1")
            mount = checkout.path

            os.makedirs(mount / "a")
            with open(mount / "a" / "B", "wb") as f:
                f.write(b"foobar")

            mock_debugInodeStatus.return_value = [
                TreeInodeDebugInfo(
                    inodeNumber=1,
                    path=b"a",
                    materialized=True,
                    treeHash=b"abcd",
                    entries=[
                        TreeInodeEntryDebugInfo(
                            name=b"b",
                            inodeNumber=2,
                            mode=stat.S_IFREG,
                            loaded=False,
                            materialized=True,
                            hash=b"dcba",
                        )
                    ],
                    refcount=1,
                ),
            ]

            tracker = ProblemCollector(instance)
            check_materialized_are_accessible(
                tracker,
                typing.cast(EdenInstance, instance),
                checkout,
                lambda p: os.lstat(p).st_mode,
            )

            problemDescriptions = {
                problem.description() for problem in tracker.problems
            }
            self.assertEqual(problemDescriptions, set())

        @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.debugInodeStatus")
        def test_materialized_file_as_symlink(self, mock_debugInodeStatus) -> None:
            instance = FakeEdenInstance(self.make_temporary_directory())
            checkout = instance.create_test_mount("path1")
            mount = checkout.path
            os.makedirs(mount / "a")
            with open(mount / "a" / "b", "wb") as f:
                f.write(b"foobar")
            os.symlink("b", mount / "a" / "c")
            mock_debugInodeStatus.return_value = [
                TreeInodeDebugInfo(
                    inodeNumber=1,
                    path=b"a",
                    materialized=True,
                    treeHash=b"abcd",
                    entries=[
                        TreeInodeEntryDebugInfo(
                            name=b"b",
                            inodeNumber=2,
                            mode=stat.S_IFREG,
                            loaded=False,
                            materialized=True,
                            hash=b"dcba",
                        ),
                        TreeInodeEntryDebugInfo(
                            name=b"c",
                            inodeNumber=3,
                            mode=stat.S_IFREG,
                            loaded=False,
                            materialized=True,
                            hash=b"dcba",
                        ),
                    ],
                    refcount=1,
                ),
            ]
            tracker = ProblemCollector(instance)
            check_materialized_are_accessible(
                tracker,
                typing.cast(EdenInstance, instance),
                checkout,
                lambda p: os.lstat(p).st_mode,
            )
            problemDescriptions = {
                problem.description() for problem in tracker.problems
            }
            self.assertEqual(
                problemDescriptions,
                {
                    f"{Path('a/c')} has an unexpected file type: known to EdenFS as a file, but is a symlink on disk",
                },
            )

        @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.debugInodeStatus")
        def test_materialized_symlink_as_file(self, mock_debugInodeStatus) -> None:
            instance = FakeEdenInstance(self.make_temporary_directory())
            checkout = instance.create_test_mount("path1")
            mount = checkout.path
            os.makedirs(mount / "a")
            with open(mount / "a" / "b", "wb") as f:
                f.write(b"foobar")
            with open(mount / "a" / "c", "wb") as f:
                f.write(b"b")
            mock_debugInodeStatus.return_value = [
                TreeInodeDebugInfo(
                    inodeNumber=1,
                    path=b"a",
                    materialized=True,
                    treeHash=b"abcd",
                    entries=[
                        TreeInodeEntryDebugInfo(
                            name=b"b",
                            inodeNumber=2,
                            mode=stat.S_IFREG,
                            loaded=False,
                            materialized=True,
                            hash=b"dcba",
                        ),
                        TreeInodeEntryDebugInfo(
                            name=b"c",
                            inodeNumber=3,
                            mode=stat.S_IFLNK,
                            loaded=False,
                            materialized=True,
                            hash=b"dcba",
                        ),
                    ],
                    refcount=1,
                ),
            ]
            tracker = ProblemCollector(instance)
            check_materialized_are_accessible(
                tracker,
                typing.cast(EdenInstance, instance),
                checkout,
                lambda p: os.lstat(p).st_mode,
            )
            self.assertEqual(len(tracker.problems), 1)
            self.assertEqual(
                tracker.problems[0].description(),
                f"{Path('a/c')} has an unexpected file type: known to EdenFS as a symlink, but is a file on disk",
            )

        @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.debugInodeStatus")
        def test_materialized_junction(self, mock_debugInodeStatus) -> None:
            instance = FakeEdenInstance(self.make_temporary_directory())
            checkout = instance.create_test_mount("path1")
            mount = checkout.path

            # Just create a folders
            os.makedirs(mount / "a" / "b")
            subprocess.run(
                f"cmd.exe /c mklink /J {mount}\\a\\c {mount}\\a\\b", check=True
            )
            subprocess.run(
                f"cmd.exe /c mklink /J {mount}\\a\\d {mount}\\a\\b", check=True
            )

            mock_debugInodeStatus.return_value = [
                TreeInodeDebugInfo(
                    inodeNumber=1,
                    path=b"a",
                    materialized=True,
                    treeHash=b"abcd",
                    entries=[
                        TreeInodeEntryDebugInfo(
                            name=b"c",
                            inodeNumber=4,
                            mode=stat.S_IFREG,
                            loaded=False,
                            materialized=True,
                            hash=b"12ef",
                        ),
                        TreeInodeEntryDebugInfo(
                            name=b"b",
                            inodeNumber=2,
                            mode=stat.S_IFDIR,
                            loaded=False,
                            materialized=False,
                            hash=b"12ef",
                        ),
                        TreeInodeEntryDebugInfo(
                            name=b"d",
                            inodeNumber=3,
                            mode=stat.S_IFDIR,
                            loaded=False,
                            materialized=True,
                            hash=b"12ef",
                        ),
                    ],
                    refcount=1,
                ),
                TreeInodeDebugInfo(
                    inodeNumber=2,
                    path=b"a/b",
                    materialized=True,
                    treeHash=b"dcba",
                    entries=[],
                    refcount=1,
                ),
                TreeInodeDebugInfo(
                    inodeNumber=2,
                    path=b"a/d",
                    materialized=True,
                    treeHash=b"dcba",
                    entries=[],
                    refcount=1,
                ),
            ]

            tracker = ProblemCollector(instance)
            check_materialized_are_accessible(
                tracker,
                typing.cast(EdenInstance, instance),
                checkout,
                lambda p: os.lstat(p).st_mode,
            )

            problemDescriptions = {
                problem.description() for problem in tracker.problems
            }
            self.assertEqual(
                problemDescriptions,
                {
                    f"""\
{Path("a/c")} has an unexpected file type: known to EdenFS as a file, but is a symlink on disk
{Path("a/d")} has an unexpected file type: known to EdenFS as a directory, but is a symlink on disk"""
                },
            )

        @patch("eden.fs.cli.redirect.Redirection.apply")
        @patch("eden.fs.cli.doctor.check_redirections.get_effective_redirections")
        def test_redirection_failed_symlink(
            self, mock_get_effective_redirections, mock_apply
        ) -> None:
            instance = FakeEdenInstance(self.make_temporary_directory())
            checkout = instance.create_test_mount("path1")

            mock_get_effective_redirections.return_value = {
                "A": Redirection(
                    checkout.path,
                    RedirectionType.BIND,
                    None,
                    "",
                    RedirectionState.SYMLINK_MISSING,
                )
            }
            mock_apply.side_effect = OSError(0, "Test error", "a", 1314, "b")

            fixer, out = self.create_fixer(dry_run=False)
            mount_table = instance.mount_table

            check_redirections(
                fixer,
                instance,
                checkout,
                mount_table,
            )
            mock_apply.assert_called_once()
            self.assertRegex(
                "\n".join(out.getvalue().splitlines()[:7]),
                r"""<yellow>- Found problem:<reset>
Misconfigured redirection at .*
Fixing redirection at .*...<red>error<reset>
Failed to fix or verify fix for problem MisconfiguredRedirection: RemediationError: Error occurred when trying to create symlink: \[WinError 1314\] Test error: 'a' -> 'b'.
User is missing permissions to create symlinks.
Check that the Developer Mode has been enabled in Windows, or that the user is allowed to create symlinks in the Local Security Policy.
Running chef may fix this.*""",
            )

    @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.getSHA1")
    @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.debugInodeStatus")
    # pyre-fixme[2]: Parameter must be annotated.
    def test_loaded_content(self, mock_debugInodeStatus, mock_getSHA1) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")

        with open(checkout.path / "a", "wb") as f:
            f.write(b"foobar")

        unmaterialized = checkout.path / "unmaterialized"
        os.makedirs(unmaterialized)
        with open(unmaterialized / "extra", "wb") as f:
            f.write(b"read all about it")

        mock_getSHA1.return_value = [SHA1Result(sha1=b"\x01\x02\x03\x04")]

        mock_debugInodeStatus.return_value = [
            TreeInodeDebugInfo(
                inodeNumber=1,
                path=b"",
                materialized=True,
                treeHash=b"abcd",
                entries=[
                    TreeInodeEntryDebugInfo(
                        name=b"a",
                        inodeNumber=2,
                        mode=stat.S_IFREG,
                        loaded=True,
                        materialized=False,
                        hash=b"1234",
                    )
                ],
                refcount=1,
            ),
            TreeInodeDebugInfo(
                inodeNumber=3,
                path=b"unmaterialized",
                materialized=False,
                treeHash=b"bcde",
                entries=[],
                refcount=1,
            ),
        ]

        # pyre-fixme[53]: Captured variable `checkout` is not annotated.
        def fake_PrjGetOnDiskFileState(path: Path) -> PRJ_FILE_STATE:
            if path == checkout.path / "a":
                return PRJ_FILE_STATE.HydratedPlaceholder
            else:
                return PRJ_FILE_STATE.Placeholder

        tracker = ProblemCollector(instance)
        check_loaded_content(
            tracker,
            typing.cast(EdenInstance, instance),
            checkout,
            fake_PrjGetOnDiskFileState,
        )

        self.assertTrue(len(tracker.problems) == 2)
        self.assertEqual(
            tracker.problems[0].description(),
            "The on-disk file at a is out of sync from EdenFS. Expected SHA1: 01020304, on-disk SHA1: 8843d7f92416211de9ebb963ff4ce28125932878",
        )
        # .hg is a materialized directory and will not present for check_loaded_content alone
        self.assertEqual(
            tracker.problems[1].description(),
            f"{Path('unmaterialized/extra')} is not known to EdenFS but is accessible on disk",
        )

    @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.getStatInfo")
    def test_inode_counts(self, mock_get_stat_info: MagicMock) -> None:
        tmp_dir = self.make_temporary_directory()
        instance = FakeEdenInstance(tmp_dir)
        checkout = instance.create_test_mount("path")

        before_mount_point_info = {
            os.fsencode(checkout.path): LegacyMountInodeInfo(
                unloadedInodeCount=2_000_000,
                loadedFileCount=3_000_000,
                loadedTreeCount=4_000_000,
            )
        }

        after_mount_point_info = {
            os.fsencode(checkout.path): LegacyMountInodeInfo(
                unloadedInodeCount=0,
                loadedFileCount=0,
                loadedTreeCount=0,
            )
        }

        out = TestOutput()
        dry_run = False
        mock_get_stat_info.side_effect = [
            InternalStats(mountPointInfo=before_mount_point_info),
            InternalStats(mountPointInfo=after_mount_point_info),
        ]

        exit_code = doctor.cure_what_ails_you(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            #  `FakeEdenInstance`.
            instance,
            dry_run,
            min_severity_to_report=ProblemSeverity.ALL,
            mount_table=instance.mount_table,
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            network_checker=FakeNetworkChecker(),
            out=out,
        )

        # Making platform-specific assertions dynamically because pyre checks
        # fail for Windows-only targets.
        if sys.platform != "linux":
            self.assertEqual(
                f"""\
Checking {checkout.path}
<yellow>- Found problem:<reset>
Mount point {checkout.path} has 9000000 files on disk, which may impact EdenFS performance
Starting background invalidation of not recently used files and directories in {checkout.path}...<green>fixed<reset>

<yellow>Successfully fixed 1 problem.<reset>
""",
                out.getvalue(),
            )
        self.assertEqual(exit_code, 0)

    def test_slow_hg_import(self) -> None:
        tmp_dir = self.make_temporary_directory()
        instance = FakeEdenInstance(tmp_dir)

        instance._fake_client.set_counter_value(
            "store.sapling.live_import.max_duration_us", 15 * 60 * 1_000_000
        )

        out = TestOutput()
        dry_run = False

        exit_code = doctor.cure_what_ails_you(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            #  `FakeEdenInstance`.
            instance,
            dry_run,
            min_severity_to_report=ProblemSeverity.ALL,
            mount_table=instance.mount_table,
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            network_checker=FakeNetworkChecker(),
            out=out,
        )

        self.assertRegex(
            out.getvalue(),
            r"""<yellow>- Found problem:<reset>
Slow file download taking up to 15 minutes observed
Try:
- Running `sl debugnetwork`\.
- Checking your network connection's performance\.
- Running `eden top` to check whether downloads are making progress\.

<yellow>1 issue with recommended fixes\.<reset>
Collect an 'eden rage' and ask in the EdenFS (Windows |macOS )?Users group if you need help fixing issues with EdenFS:
(https://fb\.workplace\.com/groups/eden\.users|https://fb\.workplace\.com/groups/edenfswindows|https://fb\.workplace\.com/groups/edenfsmacos)
""",
        )
        self.assertEqual(exit_code, 1)

    @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.getScmStatusV2")
    @patch("subprocess.run")
    def test_hg_status_and_diff_agree(
        self,
        # pyre-fixme[2]: Parameter must be annotated.
        mock_subprocess_run,
        # pyre-fixme[2]: Parameter must be annotated.
        mock_getScmStatusV2,
    ) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")

        mock_getScmStatusV2.return_value = GetScmStatusResult(
            status=ScmStatus(entries={b"foo/bar": ScmFileStatus.MODIFIED})
        )
        mock_subprocess_run.return_value = subprocess.CompletedProcess(
            stdout=b'{"foo/bar": {"adds": 2, "isbinary": false, "removes": 0}}',
            args=["hg", "diff", "--per-file-stat-json"],
            returncode=0,
        )

        tracker = ProblemCollector(instance)
        # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
        # `FakeEdenInstance`.
        check_hg_status_match_hg_diff(tracker, instance, checkout)
        self.assertEqual(tracker.problems, [])

    @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.getScmStatusV2")
    @patch("subprocess.run")
    def test_hg_status_and_diff_mismatch(
        self,
        # pyre-fixme[2]: Parameter must be annotated.
        mock_subprocess_run,
        # pyre-fixme[2]: Parameter must be annotated.
        mock_getScmStatusV2,
    ) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")

        mock_getScmStatusV2.return_value = GetScmStatusResult(
            status=ScmStatus(entries={b"foo/bar": ScmFileStatus.MODIFIED})
        )
        mock_subprocess_run.return_value = subprocess.CompletedProcess(
            stdout=b"{}", args=["hg", "diff", "--stat"], returncode=0
        )

        tracker = ProblemCollector(instance)
        # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
        # `FakeEdenInstance`.
        check_hg_status_match_hg_diff(tracker, instance, checkout)
        self.assertEqual(len(tracker.problems), 1)
        self.assertEqual(
            tracker.problems[0].description(),
            f"{Path('foo/bar')} is present as modified in `sl status` but not in `sl diff`",
        )

    @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.getScmStatusV2")
    @patch("subprocess.run")
    def test_hg_diff_is_scoped_to_modified_files(
        self,
        # pyre-fixme[2]: Parameter must be annotated.
        mock_subprocess_run,
        # pyre-fixme[2]: Parameter must be annotated.
        mock_getScmStatusV2,
    ) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")

        mock_getScmStatusV2.return_value = GetScmStatusResult(
            status=ScmStatus(
                entries={
                    b"foo/bar": ScmFileStatus.MODIFIED,
                    b"untracked": ScmFileStatus.ADDED,
                }
            )
        )

        patterns: List[str] = []

        def fake_run(
            args: List[str], **kwargs: Any
        ) -> "subprocess.CompletedProcess[bytes]":
            self.assertEqual(args[-1], "listfile0:-")
            patterns.extend(os.fsdecode(p) for p in kwargs["input"].split(b"\0"))
            return subprocess.CompletedProcess(
                stdout=b'{"foo/bar": {"adds": 2, "isbinary": false, "removes": 0}}',
                args=args,
                returncode=0,
            )

        mock_subprocess_run.side_effect = fake_run

        tracker = ProblemCollector(instance)
        # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
        # `FakeEdenInstance`.
        check_hg_status_match_hg_diff(tracker, instance, checkout)

        self.assertEqual(tracker.problems, [])
        self.assertEqual(patterns, ["path:foo/bar"])

    @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.getScmStatusV2")
    @patch("eden.fs.cli.doctor.check_filesystems.get_hg_diff")
    def test_hg_diff_runs_at_modified_file_limit(
        self,
        # pyre-fixme[2]: Parameter must be annotated.
        mock_get_hg_diff,
        # pyre-fixme[2]: Parameter must be annotated.
        mock_getScmStatusV2,
    ) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")
        modified_paths = {
            Path(f"path{index}")
            for index in range(DEFAULT_MAX_MODIFIED_FILES_FOR_HG_DIFF)
        }
        mock_getScmStatusV2.return_value = GetScmStatusResult(
            status=ScmStatus(
                entries={bytes(path): ScmFileStatus.MODIFIED for path in modified_paths}
            )
        )
        mock_get_hg_diff.return_value = modified_paths

        tracker = ProblemCollector(instance)
        # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
        # `FakeEdenInstance`.
        check_hg_status_match_hg_diff(tracker, instance, checkout)

        self.assertEqual(tracker.problems, [])
        mock_get_hg_diff.assert_called_once()
        self.assertEqual(mock_get_hg_diff.call_args.args[0], checkout)
        self.assertEqual(mock_getScmStatusV2.call_count, 2)

    @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.getScmStatusV2")
    @patch("eden.fs.cli.doctor.check_filesystems.get_hg_diff")
    def test_hg_diff_skipped_above_modified_file_limit(
        self,
        # pyre-fixme[2]: Parameter must be annotated.
        mock_get_hg_diff,
        # pyre-fixme[2]: Parameter must be annotated.
        mock_getScmStatusV2,
    ) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")
        mock_getScmStatusV2.return_value = GetScmStatusResult(
            status=ScmStatus(
                entries={
                    f"path{index}".encode(): ScmFileStatus.MODIFIED
                    for index in range(DEFAULT_MAX_MODIFIED_FILES_FOR_HG_DIFF + 1)
                }
            )
        )

        tracker = ProblemCollector(instance)
        with self.assertLogs(
            "eden.fs.cli.doctor.check_filesystems", level="INFO"
        ) as logs:
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            check_hg_status_match_hg_diff(tracker, instance, checkout)

        self.assertEqual(tracker.problems, [])
        self.assertIn(
            "1001 modified files exceed the configured limit of 1000",
            "\n".join(logs.output),
        )
        mock_get_hg_diff.assert_not_called()
        mock_getScmStatusV2.assert_called_once()

    @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.getScmStatusV2")
    @patch("eden.fs.cli.doctor.check_filesystems.get_hg_diff")
    def test_hg_diff_limit_ignores_non_modified_entries(
        self,
        # pyre-fixme[2]: Parameter must be annotated.
        mock_get_hg_diff,
        # pyre-fixme[2]: Parameter must be annotated.
        mock_getScmStatusV2,
    ) -> None:
        # EdenFS reports untracked files as ADDED and missing files as REMOVED.
        # Neither shows up in `sl diff`, so neither may count against the limit.
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")
        entries = {
            f"added{index}".encode(): ScmFileStatus.ADDED
            for index in range(DEFAULT_MAX_MODIFIED_FILES_FOR_HG_DIFF)
        }
        entries[b"removed"] = ScmFileStatus.REMOVED
        entries[b"modified"] = ScmFileStatus.MODIFIED
        mock_getScmStatusV2.return_value = GetScmStatusResult(
            status=ScmStatus(entries=entries)
        )
        mock_get_hg_diff.return_value = {Path("modified")}

        tracker = ProblemCollector(instance)
        # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
        # `FakeEdenInstance`.
        check_hg_status_match_hg_diff(tracker, instance, checkout)

        self.assertEqual(tracker.problems, [])
        mock_get_hg_diff.assert_called_once_with(checkout, ["modified"])
        self.assertEqual(mock_getScmStatusV2.call_count, 2)

    @patch("eden.fs.cli.doctor.test.lib.fake_client.FakeClient.getScmStatusV2")
    @patch("eden.fs.cli.doctor.check_filesystems.get_hg_diff")
    def test_hg_diff_modified_file_limit_can_be_disabled(
        self,
        # pyre-fixme[2]: Parameter must be annotated.
        mock_get_hg_diff,
        # pyre-fixme[2]: Parameter must be annotated.
        mock_getScmStatusV2,
    ) -> None:
        instance = FakeEdenInstance(
            self.make_temporary_directory(),
            config={"doctor.max-modified-files-for-hg-diff": "0"},
        )
        checkout = instance.create_test_mount("path1")
        modified_paths = {
            Path(f"path{index}")
            for index in range(DEFAULT_MAX_MODIFIED_FILES_FOR_HG_DIFF + 1)
        }
        mock_getScmStatusV2.return_value = GetScmStatusResult(
            status=ScmStatus(
                entries={bytes(path): ScmFileStatus.MODIFIED for path in modified_paths}
            )
        )
        mock_get_hg_diff.return_value = modified_paths

        tracker = ProblemCollector(instance)
        # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
        # `FakeEdenInstance`.
        check_hg_status_match_hg_diff(tracker, instance, checkout)

        self.assertEqual(tracker.problems, [])
        mock_get_hg_diff.assert_called_once()
        self.assertEqual(mock_get_hg_diff.call_args.args[0], checkout)
        self.assertEqual(mock_getScmStatusV2.call_count, 2)

    def test_ignored_problems_config(self) -> None:
        tmp_dir = self.make_temporary_directory()
        instance = FakeEdenInstance(
            tmp_dir,
            config={
                "doctor.ignored-problem-class-names": '["FooProblem", "SlowHgImportProblem", "BarProblem"]'
            },
        )

        instance._fake_client.set_counter_value(
            "store.sapling.live_import.max_duration_us", 15 * 60 * 1_000_000
        )

        out = TestOutput()
        dry_run = False

        exit_code = doctor.cure_what_ails_you(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            #  `FakeEdenInstance`.
            instance,
            dry_run,
            min_severity_to_report=ProblemSeverity.ALL,
            mount_table=instance.mount_table,
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            network_checker=FakeNetworkChecker(),
            out=out,
        )

        # SlowHgImportProblem should not be reported because we've ignored it in
        # the config.
        self.assertEqual(exit_code, 0)

    def test_vscode_extension_warn_list_config(self) -> None:
        tmp_dir = self.make_temporary_directory()
        instance = FakeEdenInstance(
            tmp_dir,
            config={
                "doctor.vscode-extensions-warn-list": '["nuclide.arclint-1.0.618"]'
            },
        )

        out = TestOutput()
        dry_run = True

        exit_code = doctor.cure_what_ails_you(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            #  `FakeEdenInstance`.
            instance,
            dry_run,
            min_severity_to_report=ProblemSeverity.ALL,
            mount_table=instance.mount_table,
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            network_checker=FakeNetworkChecker(),
            out=out,
        )

        self.assertEqual(
            out.getvalue(),
            r"""<yellow>- Found problem:<reset>
Unsupported Visual Studio Code extension detected, this extension may interact poorly with EdenFS:
nuclide.arclint
Please consider the effects of this extension.

<yellow>Discovered 1 problem during --dry-run<reset>
""",
        )

        self.assertEqual(exit_code, 1)

    def test_vscode_extension_block_list_config(self) -> None:
        tmp_dir = self.make_temporary_directory()
        instance = FakeEdenInstance(
            tmp_dir,
            config={
                "doctor.vscode-extensions-block-list": '["nuclide.arclint-1.0.618"]'
            },
        )

        out = TestOutput()
        dry_run = True

        exit_code = doctor.cure_what_ails_you(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            #  `FakeEdenInstance`.
            instance,
            dry_run,
            min_severity_to_report=ProblemSeverity.ALL,
            mount_table=instance.mount_table,
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsChecker(),
            network_checker=FakeNetworkChecker(),
            out=out,
        )

        self.assertEqual(
            out.getvalue(),
            r"""<yellow>- Found problem:<reset>
Harmful Visual Studio Code extension detected, this extension is known to interact poorly with EdenFS:
nuclide.arclint
Please uninstall this extension.

<yellow>Discovered 1 problem during --dry-run<reset>
""",
        )

        self.assertEqual(exit_code, 1)

    def test_vscode_extension_allow_list_config(self) -> None:
        tmp_dir = self.make_temporary_directory()
        instance = FakeEdenInstance(
            tmp_dir,
            config={
                "doctor.vscode-extensions-allow-list": '["randomdev.unknownextension"]'
            },
        )

        out = TestOutput()
        dry_run = True

        exit_code = doctor.cure_what_ails_you(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            #  `FakeEdenInstance`.
            instance,
            dry_run,
            min_severity_to_report=ProblemSeverity.ALL,
            mount_table=instance.mount_table,
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsCheckerWithExtensions(
                ["randomdev.unknownextension"]
            ),
            network_checker=FakeNetworkChecker(),
            out=out,
        )

        self.assertEqual(exit_code, 0)

    def test_vscode_extension_author_allow_list_config(self) -> None:
        tmp_dir = self.make_temporary_directory()
        instance = FakeEdenInstance(
            tmp_dir,
            config={"doctor.vscode-extensions-author-allow-list": '["randomdev"]'},
        )

        out = TestOutput()
        dry_run = True

        exit_code = doctor.cure_what_ails_you(
            # pyre-fixme[6]: For 1st param expected `EdenInstance` but got
            #  `FakeEdenInstance`.
            instance,
            dry_run,
            min_severity_to_report=ProblemSeverity.ALL,
            mount_table=instance.mount_table,
            fs_util=FakeFsUtil(),
            proc_utils=self.make_proc_utils(),
            vscode_extensions_checker=getFakeVSCodeExtensionsCheckerWithExtensions(
                ["randomdev.unknownextension"]
            ),
            network_checker=FakeNetworkChecker(),
            out=out,
        )

        self.assertEqual(exit_code, 0)

    @patch("eden.fs.cli.doctor.test.lib.fake_eden_instance.FakeEdenInstance.mount")
    def test_missing_mount_fixed(
        self,
        mock_mount: MagicMock,
    ) -> None:
        mock_mount.side_effect = [0, 1]
        fixer, out, checkout = self.setUpEdenMountTest()

        self.assertEqual(mock_mount.call_count, 2)
        self.assertEqual(mock_mount.mock_calls, [call(str(checkout.path), False)] * 2)
        self.assertEqual(len(fixer.problem_types), 1)
        self.assertEqual(fixer.num_fixed_problems, 1)
        self.assertEqual(
            f"""\
<yellow>- Found problem:<reset>
{checkout.path} is not currently mounted
Remounting {checkout.path}...<green>fixed<reset>

""",
            out.getvalue(),
        )

    @patch("eden.fs.cli.doctor.test.lib.fake_eden_instance.FakeEdenInstance.mount")
    def test_missing_mount_hg_fixed(
        self,
        mock_mount: MagicMock,
    ) -> None:
        mock_mount.side_effect = [Exception(), 0, 1]
        fixer, out, checkout = self.setUpEdenMountTest()

        self.assertEqual(mock_mount.call_count, 3)
        self.assertEqual(mock_mount.mock_calls, [call(str(checkout.path), False)] * 3)
        self.assertEqual(len(fixer.problem_types), 1)
        self.assertEqual(fixer.num_fixed_problems, 1)
        self.assertEqual(
            f"""\
<yellow>- Found problem:<reset>
{checkout.path} is not currently mounted
Remounting {checkout.path}...
Mount failed. Running `sl doctor` in the backing repo and then will retry the mount.
<green>fixed<reset>

""",
            out.getvalue(),
        )

    @patch("eden.fs.cli.doctor.test.lib.fake_eden_instance.FakeEdenInstance.mount")
    def test_missing_mount_too_short(
        self,
        mock_mount: MagicMock,
    ) -> None:
        mock_mount.side_effect = [Exception("is too short for header"), 0, 1]
        fixer, out, checkout = self.setUpEdenMountTest()

        self.assertEqual(mock_mount.call_count, 1)
        self.assertEqual(mock_mount.mock_calls, [call(str(checkout.path), False)] * 1)
        self.assertEqual(len(fixer.problem_types), 1)
        self.assertEqual(fixer.num_fixed_problems, 0)
        self.assertEqual(fixer.num_failed_fixes, 1)
        clean_path = self.format_win_path_for_regex(str(checkout.path))
        self.assertRegex(
            out.getvalue(),
            r"""<yellow>- Found problem:<reset>
{path} is not currently mounted
Remounting {path}...<red>error<reset>
Failed to fix or verify fix for problem CheckoutNotMounted: Exception: is too short for header

{path} appears to have been corrupted.
This can happen if your machine was hard-rebooted.
To recover, you will need to remove and reclone the repo.
Your local commits will be unaffected, but reclones will lose uncommitted work or shelves.
However, the local changes are manually recoverable before the reclone.
If you have local changes you would like to save before reclone, see {recovery_link}, or reachout to the EdenFS team.
To reclone the corrupted repo, run: `fbclone \$REPO --reclone --eden`
((.|\n)*)""".format(path=clean_path, recovery_link=get_local_commit_recovery_link()),
        )

    @patch("eden.fs.cli.doctor.test.lib.fake_eden_instance.FakeEdenInstance.mount")
    @patch("eden.fs.cli.doctor.get_doctor_link")
    @patch("eden.fs.cli.doctor.get_local_commit_recovery_link")
    def test_missing_mount_too_short_oss(
        self,
        mock_get_recovery_link: MagicMock,
        mock_get_doctor_link: MagicMock,
        mock_mount: MagicMock,
    ) -> None:
        mock_mount.side_effect = [Exception("is too short for header"), 0, 1]
        mock_get_recovery_link.return_value = ""
        mock_get_doctor_link.return_value = ""

        fixer, out, checkout = self.setUpEdenMountTest()

        self.assertEqual(mock_mount.call_count, 1)
        self.assertEqual(mock_mount.mock_calls, [call(str(checkout.path), False)] * 1)
        self.assertEqual(len(fixer.problem_types), 1)
        self.assertEqual(fixer.num_fixed_problems, 0)
        self.assertEqual(fixer.num_failed_fixes, 1)
        clean_path = self.format_win_path_for_regex(str(checkout.path))
        self.assertRegex(
            out.getvalue(),
            r"""<yellow>- Found problem:<reset>
{path} is not currently mounted
Remounting {path}...<red>error<reset>
Failed to fix or verify fix for problem CheckoutNotMounted: Exception: is too short for header

{path} appears to have been corrupted.
This can happen if your machine was hard-rebooted.
To recover, you will need to remove and reclone the repo.
Your local commits will be unaffected, but reclones will lose uncommitted work or shelves.
However, the local changes are manually recoverable before the reclone.
To remove the corrupted repo, run: `eden rm {path}`
((.|\n)*)""".format(path=clean_path),
        )

    @patch("eden.fs.cli.doctor.test.lib.fake_eden_instance.FakeEdenInstance.mount")
    def test_missing_mount_fail_recheck(
        self,
        mock_mount: MagicMock,
    ) -> None:
        mock_mount.side_effect = [0, Exception("error text"), 0, 1]
        fixer, out, checkout = self.setUpEdenMountTest()

        self.assertEqual(mock_mount.call_count, 2)
        self.assertEqual(mock_mount.mock_calls, [call(str(checkout.path), False)] * 2)
        self.assertEqual(len(fixer.problem_types), 1)
        self.assertEqual(fixer.num_fixed_problems, 0)
        self.assertEqual(fixer.num_failed_fixes, 1)
        self.assertEqual(
            f"""\
<yellow>- Found problem:<reset>
{checkout.path} is not currently mounted
Remounting {checkout.path}...
Attempt to fix missing mount failed: error text.
<red>error<reset>
Attempted and failed to fix problem CheckoutNotMounted

""",
            out.getvalue(),
        )

    @unittest.skipUnless(sys.platform == "darwin", "macOS only")
    @patch("eden.fs.cli.util.get_repo")
    def test_missing_config_fix_uses_nfs_on_macos(
        self,
        mock_get_repo: MagicMock,
    ) -> None:
        instance = FakeEdenInstance(
            self.make_temporary_directory(),
            config={"clone.default-mount-protocol": "FUSE"},
        )
        checkout = instance.create_test_mount("path1")
        checkout_config = instance._checkouts_by_path[str(checkout.path)].config

        path = checkout.path

        hg_repo = checkout.instance.get_hg_repo(path)
        mock_get_repo.return_value = hg_repo

        checkout_info = CheckoutInfo(
            # pyre-fixme[6]: For 3rd param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            instance,
            path,
            state=None,
            backing_repo=checkout_config.backing_repo,
            running_state_dir=checkout.state_dir,
            configured_state_dir=checkout.state_dir,
        )

        fixer, out = self.create_fixer(dry_run=False)
        mount_table = instance.mount_table

        edenfs_path = "/path/to/eden-mount"
        watchman_roots = {edenfs_path}
        watchman_info = check_watchman.WatchmanCheckInfo(watchman_roots)

        os.unlink(checkout.state_dir / "config.toml")

        check_running_mount(
            out,
            fixer,
            # pyre-fixme[6]: For 2nd param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            instance,
            checkout_info,
            mount_table,
            watchman_info,
            False,
            False,
        )

        self.assertTrue(os.path.exists(checkout.state_dir / "config.toml"))
        self.assertEqual("nfs", checkout.get_config().mount_protocol)
        self.assertEqual(
            out.getvalue(),
            f"""\
<yellow>- Found problem:<reset>
Eden's checkout state for {checkout.path} has been corrupted: {checkout.state_dir / "config.toml"} does not exist. [Errno 2] No such file or directory: '{self.format_win_path_for_regex(str(checkout.state_dir / "config.toml"))}'
Reinitialize checkout config.......<green>fixed<reset>

""",
        )
        self.assertEqual(len(fixer.problem_types), 1)
        self.assertEqual(fixer.num_fixed_problems, 1)

    @patch("eden.fs.cli.util.get_repo")
    def test_corrupted_config_fix(
        self,
        mock_get_repo: MagicMock,
    ) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")
        checkout_config = instance._checkouts_by_path[str(checkout.path)].config

        path = checkout.path

        hg_repo = checkout.instance.get_hg_repo(path)
        mock_get_repo.return_value = hg_repo

        checkout_info = CheckoutInfo(
            # pyre-fixme[6]: For 3rd param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            instance,
            path,
            state=None,
            backing_repo=checkout_config.backing_repo,
            running_state_dir=checkout.state_dir,
            configured_state_dir=checkout.state_dir,
        )

        fixer, out = self.create_fixer(dry_run=False)
        mount_table = instance.mount_table

        edenfs_path = "/path/to/eden-mount"
        watchman_roots = {edenfs_path}
        watchman_info = check_watchman.WatchmanCheckInfo(watchman_roots)

        with open(checkout.state_dir / "config.toml", "w") as f:
            f.write("corrupted config")
        with open(checkout.state_dir / "config.toml", "r") as f:
            print(f.read())

        check_running_mount(
            out,
            fixer,
            # pyre-fixme[6]: For 2nd param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            instance,
            checkout_info,
            mount_table,
            watchman_info,
            False,
            False,
        )

        self.assertTrue(os.path.exists(checkout.state_dir / "config.toml"))
        self.assertEqual(
            out.getvalue(),
            f"""\
<yellow>- Found problem:<reset>
Eden's checkout state for {checkout.path} has been corrupted: toml config file {checkout.state_dir / "config.toml"} not valid: Found invalid character in key name: 'c'. Try quoting the key name. (line 1 column 11 char 10)Detected here (line 1):

corrupted config

Reinitialize checkout config.......<green>fixed<reset>

""",
        )
        self.assertEqual(len(fixer.problem_types), 1)
        self.assertEqual(fixer.num_fixed_problems, 1)

    @patch("eden.fs.cli.config.EdenCheckout.get_config")
    def test_corrupted_config_fail(
        self,
        mock_get_config: MagicMock,
    ) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")
        checkout_config = instance._checkouts_by_path[str(checkout.path)].config

        mock_get_config.side_effect = [
            checkout_config,
            Exception("GenericOtherException"),
        ]
        path = checkout.path
        checkout_info = CheckoutInfo(
            # pyre-fixme[6]: For 3rd param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            instance,
            path,
            state=None,
            backing_repo=checkout.get_backing_repo_path(),
            running_state_dir=path,
            configured_state_dir=path,
        )

        fixer, out = self.create_fixer(dry_run=False)
        mount_table = instance.mount_table

        edenfs_path = "/path/to/eden-mount"
        watchman_roots = {edenfs_path}
        watchman_info = check_watchman.WatchmanCheckInfo(watchman_roots)

        check_running_mount(
            out,
            fixer,
            # pyre-fixme[6]: For 2nd param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            instance,
            checkout_info,
            mount_table,
            watchman_info,
            False,
            False,
        )

        self.assertEqual(mock_get_config.call_count, 2)
        self.assertEqual(len(fixer.problem_types), 1)
        self.assertEqual(fixer.num_fixed_problems, 0)
        self.assertEqual(fixer.num_manual_fixes, 1)
        self.assertEqual(
            f"""\
<yellow>- Found problem:<reset>
Eden's checkout state for {checkout.path} has been corrupted: GenericOtherException
To recover, you will need to remove and reclone the repo.
Your local commits will be unaffected, but reclones will lose uncommitted work or shelves.
However, the local changes are manually recoverable before the reclone.
If you have local changes you would like to save before reclone, see {get_local_commit_recovery_link()}, or reachout to the EdenFS team.
To reclone the corrupted repo, run: `fbclone $REPO --reclone --eden`"""
            + (
                f"\nFor additional info see the wiki at {get_doctor_link()}\n\n"
                if get_doctor_link()
                else "\n\n"
            ),
            out.getvalue(),
        )

    @patch("eden.fs.cli.config.EdenCheckout.get_config")
    @patch("eden.fs.cli.doctor.get_doctor_link")
    @patch("eden.fs.cli.doctor.get_local_commit_recovery_link")
    def test_corrupted_config_fail_oss(
        self,
        mock_get_recovery_link: MagicMock,
        mock_get_doctor_link: MagicMock,
        mock_get_config: MagicMock,
    ) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")
        checkout_config = instance._checkouts_by_path[str(checkout.path)].config

        mock_get_recovery_link.return_value = ""
        mock_get_doctor_link.return_value = ""

        mock_get_config.side_effect = [
            checkout_config,
            Exception("GenericOtherException"),
        ]
        path = checkout.path
        checkout_info = CheckoutInfo(
            # pyre-fixme[6]: For 3rd param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            instance,
            path,
            state=None,
            backing_repo=checkout.get_backing_repo_path(),
            running_state_dir=path,
            configured_state_dir=path,
        )

        fixer, out = self.create_fixer(dry_run=False)
        mount_table = instance.mount_table

        edenfs_path = "/path/to/eden-mount"
        watchman_roots = {edenfs_path}
        watchman_info = check_watchman.WatchmanCheckInfo(watchman_roots)

        check_running_mount(
            out,
            fixer,
            # pyre-fixme[6]: For 2nd param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            instance,
            checkout_info,
            mount_table,
            watchman_info,
            False,
            False,
        )

        self.assertEqual(mock_get_config.call_count, 2)
        self.assertEqual(len(fixer.problem_types), 1)
        self.assertEqual(fixer.num_fixed_problems, 0)
        self.assertEqual(fixer.num_manual_fixes, 1)
        self.assertEqual(
            f"""\
<yellow>- Found problem:<reset>
Eden's checkout state for {checkout.path} has been corrupted: GenericOtherException
To recover, you will need to remove and reclone the repo.
Your local commits will be unaffected, but reclones will lose uncommitted work or shelves.
However, the local changes are manually recoverable before the reclone.
To remove the corrupted repo, run: `eden rm {checkout.path}`

""",
            out.getvalue(),
        )

    @patch("eden.fs.cli.config.EdenCheckout.get_snapshot")
    @patch("eden.fs.cli.config.EdenCheckout.get_config")
    def test_corrupted_snapshot(
        self,
        mock_get_config: MagicMock,
        mock_get_snapshot: MagicMock,
    ) -> None:
        instance = FakeEdenInstance(self.make_temporary_directory())
        checkout = instance.create_test_mount("path1")
        checkout_config = instance._checkouts_by_path[str(checkout.path)].config

        mock_get_config.return_value = checkout_config
        mock_get_snapshot.side_effect = RuntimeError("Missing SNAPSHOT file")
        path = checkout.path
        checkout_info = CheckoutInfo(
            # pyre-fixme[6]: For 3rd param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            instance,
            path,
            state=None,
            backing_repo=checkout.get_backing_repo_path(),
            running_state_dir=path,
            configured_state_dir=path,
            mount_inode_info=MountInodeInfo(
                unloadedInodeCount=1, loadedFileCount=1, loadedTreeCount=1
            ),
        )

        fixer, out = self.create_fixer(dry_run=False)
        mount_table = instance.mount_table

        edenfs_path = "/path/to/eden-mount"
        watchman_roots = {edenfs_path}
        watchman_info = check_watchman.WatchmanCheckInfo(watchman_roots)

        check_running_mount(
            out,
            fixer,
            # pyre-fixme[6]: For 2nd param expected `EdenInstance` but got
            # `FakeEdenInstance`.
            instance,
            checkout_info,
            mount_table,
            watchman_info,
            False,
            False,
        )

        self.assertEqual(
            f"""\
<yellow>- Found problem:<reset>
Eden's checkout state for {checkout.path} has been corrupted: Missing SNAPSHOT file
To recover, you will need to remove and reclone the repo.
Your local commits will be unaffected, but reclones will lose uncommitted work or shelves.
However, the local changes are manually recoverable before the reclone.
If you have local changes you would like to save before reclone, see {get_local_commit_recovery_link()}, or reachout to the EdenFS team.
To reclone the corrupted repo, run: `fbclone $REPO --reclone --eden`"""
            + (
                f"\nFor additional info see the wiki at {get_doctor_link()}\n\n"
                if get_doctor_link()
                else "\n\n"
            ),
            out.getvalue(),
        )
        self.assertEqual(mock_get_snapshot.call_count, 1)
        self.assertEqual(len(fixer.problem_types), 1)
        self.assertEqual(fixer.num_fixed_problems, 0)
        self.assertEqual(fixer.num_manual_fixes, 1)

    @patch("subprocess.run")
    def test_network_fail_command_doctor(
        self,
        mock_subprocess_run: MagicMock,
    ) -> None:
        mock_subprocess_run.side_effect = subprocess.CalledProcessError(
            1, "test", output="stdout", stderr="stderror"
        )

        fixer, out, checkout = self.setUpEdenNetworkTest()
        self.assertEqual(
            out.getvalue(),
            """\
<yellow>- Found problem:<reset>
Encountered an error checking connection to Source Control Servers: command 'sl debugnetworkdoctor' reported an error:
Stdout:
stdout
Stderr:
stderror

Please check your network connection. If you are connected to the VPN, please try reconnecting.

""",
        )
        self.assertEqual(mock_subprocess_run.call_count, 1)
        self.assertEqual(len(fixer.problem_types), 1)
        self.assertEqual(fixer.num_fixed_problems, 0)
        self.assertEqual(fixer.num_manual_fixes, 1)
        problems = sorted(fixer.problem_manual_fixes)
        self.assertEqual(problems[0], "ConnectivityProblem")

    @patch("subprocess.run")
    def test_network_fail_command_connection(
        self,
        mock_subprocess_run: MagicMock,
    ) -> None:
        mock_subprocess_run.side_effect = [
            None,
            subprocess.CalledProcessError(
                1, "test", output="stdout", stderr="stderror"
            ),
        ]

        fixer, out, checkout = self.setUpEdenNetworkTest()
        self.assertEqual(
            out.getvalue(),
            """\
<yellow>- Found problem:<reset>
Encountered an error checking connection to Source Control Servers: command 'sl debugnetwork --connection' reported an error:
Stdout:
stdout
Stderr:
stderror

Please check your network connection. If you are connected to the VPN, please try reconnecting.

""",
        )
        self.assertEqual(mock_subprocess_run.call_count, 2)
        self.assertEqual(len(fixer.problem_types), 1)
        self.assertEqual(fixer.num_fixed_problems, 0)
        self.assertEqual(fixer.num_manual_fixes, 1)
        problems = sorted(fixer.problem_manual_fixes)
        self.assertEqual(problems[0], "ConnectivityProblem")

    @patch("subprocess.run")
    def test_network_fail_command_speed(
        self,
        mock_subprocess_run: MagicMock,
    ) -> None:
        mock_subprocess_run.side_effect = [
            None,
            None,
            subprocess.CalledProcessError(
                1, "test", output="stdout", stderr="stderror"
            ),
        ]

        fixer, out, checkout = self.setUpEdenNetworkTest()
        self.assertEqual(
            out.getvalue(),
            """\
<yellow>- Found problem:<reset>
Failed to verify speed of connection to eden services:
Stdout:
stdout
Stderr:
stderror

Check the speed report in sl debugnetwork --speed

""",
        )
        self.assertEqual(mock_subprocess_run.call_count, 3)
        self.assertEqual(len(fixer.problem_types), 1)
        self.assertEqual(fixer.num_fixed_problems, 0)
        self.assertEqual(fixer.num_manual_fixes, 1)
        problems = sorted(fixer.problem_manual_fixes)
        self.assertEqual(problems[0], "NetworkSpeedProblem")

    @patch("subprocess.run")
    def test_network_timeout(
        self,
        mock_subprocess_run: MagicMock,
    ) -> None:
        timeout = subprocess.TimeoutExpired(
            "test", timeout=1.0, output=b"stdout", stderr=b"stderror"
        )
        mock_subprocess_run.side_effect = [timeout, None, timeout, None, None, timeout]
        for method in [
            "debugnetworkdoctor",
            "debugnetwork --connection",
            "debugnetwork --speed",
        ]:
            fixer, out, checkout = self.setUpEdenNetworkTest()

            self.assertEqual(len(fixer.problem_types), 1)
            self.assertEqual(fixer.num_fixed_problems, 0)
            self.assertEqual(fixer.num_manual_fixes, 1)
            self.assertEqual(list(fixer.problem_manual_fixes)[0], "ConnectivityProblem")
            suffix = (
                "timed out:"
                if "speed" not in method
                else (
                    "exceeded timeout of 15.0s.\n"
                    "Your network might be too slow, please check the stdout for more details.\n"
                    "There should be 2 rounds of download and upload speed tests."
                )
            )
            self.assertEqual(
                out.getvalue(),
                f"""\
<yellow>- Found problem:<reset>
Encountered an error checking connection to Source Control Servers: command 'sl {method}' {suffix}
Stdout:
stdout
Stderr:
stderror

Please check your network connection. If you are connected to the VPN, please try reconnecting.

""",
            )

    @patch("subprocess.run")
    def test_network_speed_fail_check(
        self,
        mock_subprocess_run: MagicMock,
    ) -> None:
        mock_subprocess_run.side_effect = [
            None,
            None,
            subprocess.CalledProcessError(
                1, "test", output="stdout", stderr="stderror"
            ),
        ]
        fixer, out, checkout = self.setUpEdenNetworkTest()

        self.assertEqual(mock_subprocess_run.call_count, 3)
        self.assertEqual(len(fixer.problem_types), 1)
        self.assertEqual(fixer.num_fixed_problems, 0)
        self.assertEqual(fixer.num_manual_fixes, 1)
        problems = sorted(fixer.problem_manual_fixes)
        self.assertEqual(problems[0], "NetworkSpeedProblem")
        self.assertEqual(
            out.getvalue(),
            """\
<yellow>- Found problem:<reset>
Failed to verify speed of connection to eden services:
Stdout:
stdout
Stderr:
stderror

Check the speed report in sl debugnetwork --speed

""",
        )

    @patch("subprocess.run")
    def test_network_speed_pass(
        self,
        mock_subprocess_run: MagicMock,
    ) -> None:
        mock_subprocess_run.side_effect = [
            None,
            None,
            subprocess.CompletedProcess(
                "",
                0,
                stdout="""
debugnetwork: Latency: 646.2 us (average of 5 round-trips)
debugnetwork: Speed: (round 1) uploaded 50.0 MB in 134.6 ms (3116.99 Mbit/s, 371.57 MiB/s)
debugnetwork: Speed: (round 2) uploaded 50.0 MB in 132.3 ms (3170.47 Mbit/s, 377.95 MiB/s)
debugnetwork: Speed: (round 1) downloaded 250 MB in 437.5 ms (4793.85 Mbit/s, 571.47 MiB/s)
debugnetwork: Speed: (round 2) downloaded 250 MB in 446.1 ms (4700.91 Mbit/s, 560.39 MiB/s)
""",
            ),
        ]
        fixer, out, checkout = self.setUpEdenNetworkTest()

        self.assertEqual(out.getvalue(), "")
        self.assertEqual(len(fixer.problem_types), 0)

    @patch("subprocess.run")
    def test_network_latency_fail(
        self,
        mock_subprocess_run: MagicMock,
    ) -> None:
        mock_subprocess_run.side_effect = [
            None,
            None,
            subprocess.CompletedProcess(
                "",
                0,
                stdout="""
debugnetwork: Latency: 646.2 ms (average of 5 round-trips)
debugnetwork: Speed: (round 1) uploaded 50.0 MB in 134.6 ms (3116.99 Mbit/s, 371.57 MiB/s)
debugnetwork: Speed: (round 2) uploaded 50.0 MB in 132.3 ms (3170.47 Mbit/s, 377.95 MiB/s)
debugnetwork: Speed: (round 1) downloaded 250 MB in 437.5 ms (4793.85 Mbit/s, 571.47 MiB/s)
debugnetwork: Speed: (round 2) downloaded 250 MB in 446.1 ms (4700.91 Mbit/s, 560.39 MiB/s)
""",
            ),
        ]
        fixer, out, checkout = self.setUpEdenNetworkTest()

        self.assertEqual(
            out.getvalue(),
            f"""\
<yellow>- Found problem:<reset>
High network latency detected: Latency 646.2 ms higher than 250ms
Please check if anything is causing high ping on your network.{get_netinfo_link()}

""",
        )
        self.assertEqual(mock_subprocess_run.call_count, 3)
        self.assertEqual(len(fixer.problem_types), 1)
        self.assertEqual(fixer.num_fixed_problems, 0)
        self.assertEqual(fixer.num_manual_fixes, 1)
        problems = sorted(fixer.problem_manual_fixes)
        self.assertEqual(problems[0], "NetworkLatencyProblem")

    @patch("subprocess.run")
    def test_network_speed_fail_download(
        self,
        mock_subprocess_run: MagicMock,
    ) -> None:
        mock_subprocess_run.side_effect = [
            None,
            None,
            subprocess.CompletedProcess(
                "",
                0,
                stdout="""
debugnetwork: Latency: 646.2 us (average of 5 round-trips)
debugnetwork: Speed: (round 1) uploaded 50.0 MB in 134.6 ms (31.1699 Mbit/s, 37.157 MiB/s)
debugnetwork: Speed: (round 2) uploaded 50.0 MB in 132.3 ms (31.7047 Mbit/s, 37.795 MiB/s)
debugnetwork: Speed: (round 1) downloaded 250 MB in 437.5 ms (47.9385 Mbit/s, 57.147 MiB/s)
debugnetwork: Speed: (round 2) downloaded 250 MB in 446.1 ms (47.0091 Mbit/s, 56.039 MiB/s)
""",
            ),
        ]
        fixer, out, checkout = self.setUpEdenNetworkTest()

        self.assertEqual(
            out.getvalue(),
            f"""\
<yellow>- Found problem:<reset>
Slow network speed detected: Average download speed 47.47 Mbit/s slower than 50 Mbit/s
Please check if anything is consuming an excess amount of bandwidth on your network.{get_netinfo_link()}

""",
        )
        self.assertEqual(mock_subprocess_run.call_count, 3)
        self.assertEqual(len(fixer.problem_types), 1)
        self.assertEqual(fixer.num_fixed_problems, 0)
        self.assertEqual(fixer.num_manual_fixes, 1)
        problems = sorted(fixer.problem_manual_fixes)
        self.assertEqual(problems[0], "NetworkSlowSpeedProblem")

    @patch("subprocess.run")
    def test_network_speed_fail_upload(
        self,
        mock_subprocess_run: MagicMock,
    ) -> None:
        mock_subprocess_run.side_effect = [
            None,
            None,
            subprocess.CompletedProcess(
                "",
                0,
                stdout="""
debugnetwork: Latency: 646.2 us (average of 5 round-trips)
debugnetwork: Speed: (round 1) uploaded 15.0 MB in 134.6 ms (9.1699 Mbit/s, 37.157 MiB/s)
debugnetwork: Speed: (round 2) uploaded 15.0 MB in 132.3 ms (9.7047 Mbit/s, 37.795 MiB/s)
debugnetwork: Speed: (round 1) downloaded 250 MB in 437.5 ms (52.9385 Mbit/s, 57.147 MiB/s)
debugnetwork: Speed: (round 2) downloaded 250 MB in 446.1 ms (52.0091 Mbit/s, 56.039 MiB/s)
""",
            ),
        ]
        fixer, out, checkout = self.setUpEdenNetworkTest()

        self.assertEqual(
            out.getvalue(),
            f"""\
<yellow>- Found problem:<reset>
Slow network speed detected: Average upload speed 9.44 Mbit/s slower than 10 Mbit/s
Please check if anything is consuming an excess amount of bandwidth on your network.{get_netinfo_link()}

""",
        )
        self.assertEqual(mock_subprocess_run.call_count, 3)
        self.assertEqual(len(fixer.problem_types), 1)
        self.assertEqual(fixer.num_fixed_problems, 0)
        self.assertEqual(fixer.num_manual_fixes, 1)
        problems = sorted(fixer.problem_manual_fixes)
        self.assertEqual(problems[0], "NetworkSlowSpeedProblem")

    @patch("subprocess.run")
    def test_network_speed_fail_both(
        self,
        mock_subprocess_run: MagicMock,
    ) -> None:
        mock_subprocess_run.side_effect = [
            None,
            None,
            subprocess.CompletedProcess(
                "",
                0,
                stdout="""
debugnetwork: Latency: 17.14 ms (average of 5 round-trips)
debugnetwork: Speed: (round 1) uploaded 2.00 MB in 1.143 s (11.68 Mbit/s, 1.75 MiB/s)
debugnetwork: Speed: (round 2) uploaded 2.00 MB in 609.6 ms (7.52 Mbit/s, 3.28 MiB/s)
debugnetwork: Speed: (round 1) downloaded 10.0 MB in 1.635 s (40.31 Mbit/s, 6.12 MiB/s)
debugnetwork: Speed: (round 2) downloaded 10.0 MB in 1.439 s (42.31 Mbit/s, 6.95 MiB/s)
""",
            ),
        ]
        fixer, out, checkout = self.setUpEdenNetworkTest()

        self.assertEqual(
            out.getvalue(),
            f"""\
<yellow>- Found problem:<reset>
Slow network speed detected: Average download speed 41.31 Mbit/s slower than 50 Mbit/s, and average upload speed 9.60Mbit/s slower than 10 Mbit/s
Please check if anything is consuming an excess amount of bandwidth on your network.{get_netinfo_link()}

""",
        )
        self.assertEqual(mock_subprocess_run.call_count, 3)
        self.assertEqual(len(fixer.problem_types), 1)
        self.assertEqual(fixer.num_fixed_problems, 0)
        self.assertEqual(fixer.num_manual_fixes, 1)
        problems = sorted(fixer.problem_manual_fixes)
        self.assertEqual(problems[0], "NetworkSlowSpeedProblem")


def _create_watchman_subscription(
    filewatcher_subscriptions: Optional[List[str]] = None,
    # pyre-fixme[24]: Generic type `dict` expects 2 type parameters, use
    #  `typing.Dict[<key type>, <value type>]` to avoid runtime subscripting errors.
) -> Dict:
    if filewatcher_subscriptions is None:
        filewatcher_subscriptions = []
    subscribers = []
    for filewatcher_subscription in filewatcher_subscriptions:
        subscribers.append(
            {
                "info": {
                    "name": filewatcher_subscription,
                    "query": {
                        "empty_on_fresh_instance": True,
                        "defer_vcs": False,
                        "fields": ["name", "new", "exists", "mode"],
                        "relative_root": "fbcode",
                        "since": "c:1511985586:2749065:2774073346:354",
                    },
                }
            }
        )
    return {"subscribers": subscribers}
