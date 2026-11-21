#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import contextlib
import io
import json
import os
from pathlib import Path
from unittest.mock import MagicMock, patch

import toml
from eden.fs.cli import config as config_mod, main as main_mod, util
from eden.fs.cli.config import EdenInstance
from eden.fs.service.eden.thrift_types import EdenError, MountInfo, MountState
from eden.test_support.testcase import EdenTestCaseBase


class UnconfiguredUnmountTest(EdenTestCaseBase):
    def setUp(self) -> None:
        super().setUp()
        self.root = self.tmp_dir.resolve()
        self.state = self.root / "state"
        self.client_dir = self.state / "clients" / "orphan"
        self.mount = self.root / "orphan"
        self.mount.mkdir()
        self.client_dir.mkdir(parents=True)
        self.mapping = self.state / "config.json"
        self.mapping.write_text("{}")
        self.overlay = self.client_dir / "overlay" / "uncommitted"
        self.overlay.parent.mkdir()
        self.overlay.write_bytes(b"keep local overlay data\n")
        self.backing = self.root / "backing" / "local-data"
        self.backing.parent.mkdir()
        self.backing.write_bytes(b"keep backing data\n")
        self.client_config = self.client_dir / "config.toml"
        self.client_config.write_text("# deliberately unreadable checkout config\n")
        self.thrift = MagicMock()
        self.thrift.listMounts.return_value = [
            MountInfo(
                mountPoint=os.fsencode(self.mount),
                edenClientPath=os.fsencode(self.client_dir),
                state=MountState.RUNNING,
                backingRepoPath=os.fsencode(self.backing.parent),
            )
        ]
        self.factory = self.exit_stack.enter_context(
            patch.object(EdenInstance, "get_thrift_client")
        )
        self.factory.return_value.__enter__.return_value = self.thrift
        self.stderr = io.StringIO()
        self.exit_stack.enter_context(contextlib.redirect_stderr(self.stderr))

    def run_unmount(self, *paths: str, flags: tuple[str, ...] = ()) -> int:
        parser = main_mod.create_parser()
        args = parser.parse_args(
            [
                "--config-dir",
                str(self.state),
                "--etc-eden-dir",
                str(self.root / "etc"),
                "--home-dir",
                str(self.root / "home"),
                "unmount",
                *flags,
                *paths,
            ]
        )
        return args.func(args)

    def assert_state_preserved(self) -> None:
        self.assertEqual("{}", self.mapping.read_text())
        self.assertEqual(b"keep local overlay data\n", self.overlay.read_bytes())
        self.assertEqual(b"keep backing data\n", self.backing.read_bytes())
        self.assertEqual(
            "# deliberately unreadable checkout config\n",
            self.client_config.read_text(),
        )
        self.assertTrue(self.mount.is_dir())

    def test_unmounts_exact_daemon_only_path_without_checkout_metadata(self) -> None:
        self.assertEqual(0, self.run_unmount(str(self.mount)))
        self.thrift.unmountV2.assert_called_once()
        request = self.thrift.unmountV2.call_args.args[0]
        self.assertEqual(os.fsencode(self.mount), request.mountId.mountPoint)
        self.assertTrue(request.useForce)
        self.assert_state_preserved()

    def test_initializing_registration_reaches_the_unmount_rpc(self) -> None:
        info = self.thrift.listMounts.return_value[0]
        self.thrift.listMounts.return_value = [
            MountInfo(
                mountPoint=info.mountPoint,
                edenClientPath=info.edenClientPath,
                state=MountState.INITIALIZING,
            )
        ]
        self.assertEqual(0, self.run_unmount(str(self.mount)))
        self.thrift.unmountV2.assert_called_once()
        self.assert_state_preserved()

    def test_intact_unconfigured_metadata_uses_the_existing_checkout_path(self) -> None:
        self.client_config.write_text(
            toml.dumps({"repository": {"path": str(self.backing.parent), "type": "hg"}})
        )
        metadata = self.mount / ".eden"
        metadata.mkdir()
        for name, target in (
            ("socket", self.state / "socket"),
            ("root", self.mount),
            ("client", self.client_dir),
        ):
            (metadata / name).symlink_to(target)
        self.assertEqual(0, self.run_unmount(str(self.mount)))
        self.thrift.listMounts.assert_not_called()
        self.thrift.unmountV2.assert_called_once()
        self.assertEqual("{}", self.mapping.read_text())
        self.assertEqual(b"keep local overlay data\n", self.overlay.read_bytes())

    def test_no_force_reaches_the_unmount_rpc(self) -> None:
        self.assertEqual(0, self.run_unmount(str(self.mount), flags=("--no-force",)))
        request = self.thrift.unmountV2.call_args.args[0]
        self.assertFalse(request.useForce)
        self.assert_state_preserved()

    def test_unknown_child_prefix_and_traversal_paths_are_rejected(self) -> None:
        paths = (
            self.root / "unknown",
            self.mount / "child",
            self.root / "orphan-suffix",
            self.mount / ".." / "unknown",
        )
        for path in paths:
            with self.subTest(path=path):
                self.assertEqual(1, self.run_unmount(str(path)))
                self.assertIn(
                    f"no EdenFS checkout found at {path}", self.stderr.getvalue()
                )
        self.thrift.unmountV2.assert_not_called()
        self.assert_state_preserved()

    def test_symlink_alias_unmounts_the_exact_daemon_mount(self) -> None:
        alias = self.root / "alias"
        alias.symlink_to(self.mount, target_is_directory=True)
        self.assertEqual(0, self.run_unmount(str(alias)))
        self.thrift.unmountV2.assert_called_once()
        request = self.thrift.unmountV2.call_args.args[0]
        self.assertEqual(os.fsencode(self.mount), request.mountId.mountPoint)
        self.assert_state_preserved()

    def test_symlink_parent_unmounts_the_exact_daemon_mount(self) -> None:
        alias = self.root / "alias"
        alias.symlink_to(self.root, target_is_directory=True)
        self.assertEqual(0, self.run_unmount(str(alias / self.mount.name)))
        self.thrift.unmountV2.assert_called_once()
        request = self.thrift.unmountV2.call_args.args[0]
        self.assertEqual(os.fsencode(self.mount), request.mountId.mountPoint)
        self.assert_state_preserved()

    def test_symlink_to_an_unknown_path_is_rejected(self) -> None:
        unknown = self.root / "unknown"
        unknown.mkdir()
        alias = self.root / "alias"
        alias.symlink_to(unknown, target_is_directory=True)
        self.assertEqual(1, self.run_unmount(str(alias)))
        self.assertIn("no EdenFS checkout found", self.stderr.getvalue())
        self.thrift.unmountV2.assert_not_called()
        self.assert_state_preserved()

    def test_daemon_absence_is_not_config_only_authorization(self) -> None:
        self.thrift.listMounts.return_value = []
        self.assertEqual(1, self.run_unmount(str(self.mount)))
        self.assertIn("no EdenFS checkout found", self.stderr.getvalue())
        self.thrift.unmountV2.assert_not_called()
        self.assert_state_preserved()

    def test_missing_client_state_of_a_configured_mount_is_not_bypassed(self) -> None:
        self.mapping.write_text(json.dumps({str(self.mount): "missing-client"}))
        with self.assertRaises(config_mod.CheckoutConfigCorruptedError):
            self.run_unmount(str(self.mount))
        self.thrift.listMounts.assert_not_called()
        self.thrift.unmountV2.assert_not_called()
        self.assertEqual(b"keep local overlay data\n", self.overlay.read_bytes())

    def test_configured_unmount_keeps_its_normal_intentional_marker(self) -> None:
        self.mapping.write_text(json.dumps({str(self.mount): "orphan"}))
        self.client_config.write_text(
            toml.dumps({"repository": {"path": str(self.backing.parent), "type": "hg"}})
        )
        self.assertEqual(0, self.run_unmount(str(self.mount)))
        self.thrift.listMounts.assert_not_called()
        self.thrift.unmountV2.assert_called_once()
        self.assertTrue((self.client_dir / util.INTENTIONALLY_UNMOUNTED).exists())
        self.assertEqual(b"keep local overlay data\n", self.overlay.read_bytes())
        self.assertEqual(b"keep backing data\n", self.backing.read_bytes())

    def test_debug_unintentional_skips_missing_metadata_before_rpc(self) -> None:
        self.assertEqual(
            1,
            self.run_unmount(str(self.mount), flags=("--debug-mark-unintentional",)),
        )
        self.assertIn("checkout metadata is required", self.stderr.getvalue())
        self.thrift.listMounts.assert_not_called()
        self.thrift.unmountV2.assert_not_called()
        self.assert_state_preserved()

    def test_debug_skip_still_unmounts_a_later_configured_path(self) -> None:
        configured = self.root / "configured"
        configured.mkdir()
        client_dir = self.state / "clients" / "configured"
        client_dir.mkdir()
        (client_dir / "config.toml").write_text(
            toml.dumps({"repository": {"path": str(self.backing.parent), "type": "hg"}})
        )
        self.mapping.write_text(json.dumps({str(configured): "configured"}))
        marker = client_dir / util.INTENTIONALLY_UNMOUNTED
        marker.touch()
        self.assertEqual(
            1,
            self.run_unmount(
                str(self.mount), str(configured), flags=("--debug-mark-unintentional",)
            ),
        )
        self.thrift.listMounts.assert_not_called()
        self.thrift.unmountV2.assert_called_once()
        request = self.thrift.unmountV2.call_args.args[0]
        self.assertEqual(os.fsencode(configured), request.mountId.mountPoint)
        self.assertFalse(marker.exists())
        self.assertEqual(
            {str(configured): "configured"}, json.loads(self.mapping.read_text())
        )
        self.assertEqual(b"keep local overlay data\n", self.overlay.read_bytes())

    def test_multiple_daemon_only_paths_share_one_lookup(self) -> None:
        second = self.root / "second"
        second.mkdir()
        self.thrift.listMounts.return_value.append(
            MountInfo(mountPoint=os.fsencode(second), state=MountState.RUNNING)
        )
        self.assertEqual(0, self.run_unmount(str(self.mount), str(second)))
        self.thrift.listMounts.assert_called_once()
        self.assertEqual(2, self.thrift.unmountV2.call_count)
        self.assert_state_preserved()

    def test_a_successfully_unmounted_path_is_removed_from_the_cache(self) -> None:
        self.assertEqual(1, self.run_unmount(str(self.mount), str(self.mount)))
        self.assertIn("no EdenFS checkout found", self.stderr.getvalue())
        self.thrift.listMounts.assert_called_once()
        self.thrift.unmountV2.assert_called_once()
        self.assert_state_preserved()

    def test_mount_cache_is_not_reused_between_commands(self) -> None:
        self.assertEqual(0, self.run_unmount(str(self.mount)))
        self.thrift.listMounts.return_value = []
        self.assertEqual(1, self.run_unmount(str(self.mount)))
        self.assertIn("no EdenFS checkout found", self.stderr.getvalue())
        self.assertEqual(2, self.thrift.listMounts.call_count)
        self.thrift.unmountV2.assert_called_once()

    def test_mount_cache_is_scoped_to_the_selected_daemon(self) -> None:
        first = EdenInstance(str(self.state), None, None)
        second = EdenInstance(str(self.root / "second-state"), None, None)
        cache: dict[Path, set[bytes]] = {}
        command = main_mod.UnmountCmd(main_mod.create_parser())
        self.assertEqual(
            str(self.mount),
            command._resolve_daemon_only_mount(first, str(self.mount), cache),
        )
        self.thrift.listMounts.return_value = []
        self.assertIsNone(
            command._resolve_daemon_only_mount(second, str(self.mount), cache)
        )
        self.assertIn("no EdenFS checkout found", self.stderr.getvalue())
        self.assertEqual(2, self.thrift.listMounts.call_count)
        self.thrift.unmountV2.assert_not_called()

    def test_list_rpc_failure_is_a_failure_not_authorization(self) -> None:
        self.thrift.listMounts.side_effect = EdenError(message="list rejected")
        self.assertEqual(1, self.run_unmount(str(self.mount)))
        self.assertIn("could not verify", self.stderr.getvalue())
        self.assertIn("list rejected", self.stderr.getvalue())
        self.thrift.unmountV2.assert_not_called()
        self.assert_state_preserved()

    def test_unknown_path_does_not_prevent_a_later_unmount(self) -> None:
        unknown = self.root / "unknown"
        self.assertEqual(1, self.run_unmount(str(unknown), str(self.mount)))
        self.assertIn(f"no EdenFS checkout found at {unknown}", self.stderr.getvalue())
        self.thrift.listMounts.assert_called_once()
        self.thrift.unmountV2.assert_called_once()
        request = self.thrift.unmountV2.call_args.args[0]
        self.assertEqual(os.fsencode(self.mount), request.mountId.mountPoint)
        self.assert_state_preserved()

    def test_failed_lookup_does_not_cache_an_empty_mount_list(self) -> None:
        mounts = self.thrift.listMounts.return_value
        self.thrift.listMounts.side_effect = [
            EdenError(message="list rejected"),
            mounts,
        ]
        self.assertEqual(1, self.run_unmount(str(self.mount), str(self.mount)))
        self.assertIn("could not verify", self.stderr.getvalue())
        self.assertEqual(2, self.thrift.listMounts.call_count)
        self.thrift.unmountV2.assert_called_once()
        self.assert_state_preserved()

    def test_failed_lookup_returns_none_without_populating_the_cache(self) -> None:
        instance = EdenInstance(str(self.state), None, None)
        command = main_mod.UnmountCmd(main_mod.create_parser())
        cache: dict[Path, set[bytes]] = {}
        self.thrift.listMounts.side_effect = EdenError(message="list rejected")
        self.assertIsNone(
            command._resolve_daemon_only_mount(instance, str(self.mount), cache)
        )
        self.assertEqual({}, cache)
        self.assertIn("list rejected", self.stderr.getvalue())
        self.thrift.unmountV2.assert_not_called()
        self.assert_state_preserved()

    def test_unmount_rpc_failure_is_returned_without_state_cleanup(self) -> None:
        self.thrift.unmountV2.side_effect = EdenError(message="unmount rejected")
        self.assertEqual(1, self.run_unmount(str(self.mount)))
        self.assertIn("unmount rejected", self.stderr.getvalue())
        self.assert_state_preserved()

    def test_timeout_is_not_mistaken_for_success(self) -> None:
        self.thrift.unmountV2.side_effect = TimeoutError("unmount timed out")
        with self.assertRaisesRegex(TimeoutError, "unmount timed out"):
            self.run_unmount(str(self.mount))
        self.assert_state_preserved()

    def test_lookup_and_unmount_use_bounded_rpc_timeouts(self) -> None:
        self.assertEqual(0, self.run_unmount(str(self.mount)))
        self.assertEqual(
            [3, config_mod.UNMOUNT_TIMEOUT_SECONDS],
            [call.kwargs["timeout"] for call in self.factory.call_args_list],
        )

    def test_multiple_paths_stop_at_the_first_unmount_failure(self) -> None:
        second = self.root / "second"
        third = self.root / "third"
        for path in (second, third):
            path.mkdir()
        self.thrift.listMounts.return_value = [
            MountInfo(
                mountPoint=os.fsencode(path),
                edenClientPath=os.fsencode(self.client_dir),
                state=MountState.RUNNING,
            )
            for path in (self.mount, second, third)
        ]
        self.thrift.unmountV2.side_effect = [None, EdenError(message="second failed")]
        self.assertEqual(1, self.run_unmount(str(self.mount), str(second), str(third)))
        self.assertEqual(
            [os.fsencode(self.mount), os.fsencode(second)],
            [
                call.args[0].mountId.mountPoint
                for call in self.thrift.unmountV2.call_args_list
            ],
        )
        self.assert_state_preserved()
