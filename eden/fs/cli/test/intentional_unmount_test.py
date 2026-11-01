#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import contextlib
import json
from typing import Iterator, Optional
from unittest.mock import MagicMock, patch

from eden.fs.cli import daemon
from eden.fs.cli.config import EdenInstance
from eden.fs.cli.util import INTENTIONALLY_UNMOUNTED
from eden.test_support.testcase import EdenTestCaseBase


@contextlib.contextmanager
def _fake_thrift_client(
    _instance: EdenInstance, timeout: Optional[float] = None
) -> Iterator[MagicMock]:
    yield MagicMock()


@contextlib.contextmanager
def _failing_thrift_client(
    _instance: EdenInstance, timeout: Optional[float] = None
) -> Iterator[MagicMock]:
    client = MagicMock()
    client.unmountV2.side_effect = RuntimeError("unmountV2 timed out")
    yield client


class IntentionalUnmountMarkerTest(EdenTestCaseBase):
    def setUp(self) -> None:
        super().setUp()
        state_dir = self.tmp_dir / ".eden"
        self.mount_path = (self.tmp_dir / "checkout").resolve()
        self.marker = state_dir / "clients" / "checkout" / INTENTIONALLY_UNMOUNTED
        self.marker.parent.mkdir(parents=True)
        (state_dir / "config.json").write_text(
            json.dumps({str(self.mount_path): "checkout"})
        )
        self.instance = EdenInstance(
            state_dir, self.tmp_dir / "etc", self.tmp_dir / "home"
        )
        self.exit_stack.enter_context(
            patch.object(EdenInstance, "get_thrift_client", _fake_thrift_client)
        )

    def test_unmount_marks_the_checkout_intentionally_unmounted(self) -> None:
        self.instance.unmount(str(self.mount_path))
        self.assertTrue(self.marker.exists())

    def test_unintentional_unmount_removes_the_marker(self) -> None:
        self.marker.touch()
        self.instance.unmount(str(self.mount_path), unintentional_unmount=True)
        self.assertFalse(self.marker.exists())

    def test_failed_unintentional_unmount_removes_the_marker(self) -> None:
        self.marker.touch()
        with patch.object(EdenInstance, "get_thrift_client", _failing_thrift_client):
            with self.assertRaises(RuntimeError):
                self.instance.unmount(str(self.mount_path), unintentional_unmount=True)
        self.assertFalse(self.marker.exists())


class GracefulRestartMarkerTest(EdenTestCaseBase):
    def setUp(self) -> None:
        super().setUp()
        state_dir = self.tmp_dir / ".eden"
        clients_dir = state_dir / "clients"
        self.unmounted_marker = clients_dir / "unmounted" / INTENTIONALLY_UNMOUNTED
        self.mounted_marker = clients_dir / "mounted" / INTENTIONALLY_UNMOUNTED
        self.unmounted_marker.parent.mkdir(parents=True)
        self.mounted_marker.parent.mkdir(parents=True)
        self.unmounted_marker.touch()
        self.instance = EdenInstance(
            state_dir, self.tmp_dir / "etc", self.tmp_dir / "home"
        )

    def test_restart_marks_every_checkout_while_it_runs(self) -> None:
        def start(**kwargs: object) -> int:
            self.assertTrue(self.mounted_marker.exists())
            self.assertTrue(self.unmounted_marker.exists())
            return 0

        with patch.object(daemon, "_start_edenfs_service", side_effect=start):
            daemon.gracefully_restart_edenfs_service(self.instance)

    def test_restart_keeps_markers_it_did_not_create(self) -> None:
        with patch.object(daemon, "_start_edenfs_service", return_value=0):
            daemon.gracefully_restart_edenfs_service(self.instance)
        self.assertFalse(self.mounted_marker.exists())
        self.assertTrue(self.unmounted_marker.exists())

    def test_failed_restart_still_clears_its_markers(self) -> None:
        with patch.object(
            daemon, "_start_edenfs_service", side_effect=RuntimeError("failed")
        ):
            with self.assertRaises(RuntimeError):
                daemon.gracefully_restart_edenfs_service(self.instance)
        self.assertFalse(self.mounted_marker.exists())
        self.assertTrue(self.unmounted_marker.exists())
