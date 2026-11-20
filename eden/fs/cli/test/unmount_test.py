#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import contextlib
import io
import os
from unittest.mock import MagicMock, patch

from eden.fs.cli import main as main_mod
from eden.fs.cli.config import EdenInstance
from eden.fs.cli.subcmd import CmdError
from eden.fs.service.eden.thrift_types import MountInfo, MountState
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
        # FIXME: Allow unmount of a daemon-known checkout without checkout metadata.
        with self.assertRaisesRegex(CmdError, "no EdenFS checkout found"):
            self.run_unmount(str(self.mount))
        self.thrift.unmountV2.assert_not_called()
        self.assert_state_preserved()
