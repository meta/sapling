#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import errno
import os
import stat
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock

from eden.fs.service.eden.thrift_types import (
    DaemonInfo,
    TreeInodeDebugInfo,
    TreeInodeEntryDebugInfo,
)
from eden.thrift.client import EdenNotRunningError
from fb303_core.thrift_types import fb303_status
from thrift.python.exceptions import (
    ApplicationError,
    ApplicationErrorType,
    TransportError,
    TransportErrorType,
)

from .. import util


class UtilTest(unittest.TestCase):
    def test_is_edenfs_mount(self) -> None:
        self.assertTrue(util.is_edenfs_mount(b"edenfs:", b"nfs"))
        self.assertTrue(util.is_edenfs_mount(b"edenfs:", b"edenfs:"))
        # An NFS checkout served over a Unix domain socket on macOS, with and
        # without the filesystem type override.
        self.assertTrue(
            util.is_edenfs_mount(
                b"</Users/me/.eden/clients/repo/nfsd.socket>:/Users/me/repo",
                b"edenfs:",
            )
        )
        self.assertTrue(
            util.is_edenfs_mount(
                b"</Users/me/.eden/clients/repo/edenfs-nfsd.socket>:/Users/me/repo",
                b"nfs",
            )
        )
        self.assertFalse(util.is_edenfs_mount(b"/dev/disk3s5", b"apfs"))
        self.assertFalse(util.is_edenfs_mount(b"server:/export", b"nfs"))
        self.assertFalse(
            util.is_edenfs_mount(b"</var/run/other/nfsd.socket>:/mnt", b"nfs")
        )

    def test_missing_backing_repo_does_not_block_edensparse_migration(self) -> None:
        backing_repo = MagicMock()
        backing_repo._run_hg.side_effect = FileNotFoundError("backing repo deleted")

        checkout = MagicMock()
        checkout.path = Path("/deleted/backing/repo")
        checkout.get_backing_repo.return_value = backing_repo

        instance = MagicMock()
        instance.get_checkouts.return_value = [checkout]

        util.maybe_edensparse_migration(
            instance, util.EdensparseMigrationStep.PRE_EDEN_START
        )

        instance.log_sample.assert_not_called()

    def test_is_valid_sha1(self) -> None:
        def is_valid(sha1: str) -> bool:
            return util.is_valid_sha1(sha1)

        self.assertTrue(is_valid("0123456789abcabcabcd0123456789abcabcabcd"))
        self.assertTrue(is_valid("0" * 40))

        self.assertFalse(is_valid("0123456789abcabcabcd0123456789abcabcabc"))
        self.assertFalse(is_valid("z123456789abcabcabcd0123456789abcabcabcd"))
        self.assertFalse(is_valid(""))
        self.assertFalse(is_valid("abc"))
        self.assertFalse(is_valid("z" * 40))

    INODE_RESULTS_0 = [
        TreeInodeDebugInfo(
            inodeNumber=1,
            path=os.path.join("some_path", "d1").encode(),
            materialized=True,
            treeHash=b"abc",
            entries=[
                TreeInodeEntryDebugInfo(
                    name=b"read_file",
                    inodeNumber=2,
                    mode=stat.S_IFREG,
                    loaded=True,
                    materialized=False,
                    hash=b"1abc",
                    fileSize=300,
                ),
                TreeInodeEntryDebugInfo(
                    name=b"written_file",
                    inodeNumber=3,
                    mode=stat.S_IFREG,
                    loaded=True,
                    materialized=True,
                    fileSize=400,
                ),
            ],
            refcount=0,
        )
    ]

    def test_read_write_separation(self) -> None:
        read_files, written_files = util.split_inodes_by_operation_type(
            self.INODE_RESULTS_0
        )
        self.assertListEqual(
            read_files, [(os.path.join("some_path", "d1", "read_file"), 300)]
        )
        self.assertListEqual(
            written_files, [(os.path.join("some_path", "d1", "written_file"), 400)]
        )

    INODE_RESULTS_1 = [
        TreeInodeDebugInfo(
            inodeNumber=1,
            path=b"some_path/d1",
            materialized=True,
            treeHash=b"abc",
            entries=[
                TreeInodeEntryDebugInfo(
                    name=b"read_file",
                    inodeNumber=2,
                    mode=stat.S_IFLNK,
                    loaded=True,
                    materialized=False,
                    hash=b"1abc",
                    fileSize=300,
                ),
                TreeInodeEntryDebugInfo(
                    name=b"written_file",
                    inodeNumber=3,
                    mode=stat.S_IFDIR,
                    loaded=True,
                    materialized=True,
                    fileSize=400,
                ),
            ],
            refcount=0,
        )
    ]

    def test_ignore_symlinks_and_directories(self) -> None:
        read_files, written_files = util.split_inodes_by_operation_type(
            self.INODE_RESULTS_1
        )
        self.assertListEqual(read_files, [])
        self.assertListEqual(written_files, [])

    INODE_RESULTS_2 = [
        TreeInodeDebugInfo(
            inodeNumber=1,
            path=os.path.join("some_path", "d1").encode(),
            materialized=True,
            treeHash=b"abc",
            entries=[
                TreeInodeEntryDebugInfo(
                    name=b"read_file",
                    inodeNumber=2,
                    mode=stat.S_IFREG,
                    loaded=True,
                    materialized=False,
                    hash=b"1abc",
                    fileSize=300,
                )
            ],
            refcount=0,
        ),
        TreeInodeDebugInfo(
            inodeNumber=3,
            path=os.path.join("some_path", "d1").encode(),
            materialized=True,
            treeHash=b"abc",
            entries=[
                TreeInodeEntryDebugInfo(
                    name=b"written_file",
                    inodeNumber=4,
                    mode=stat.S_IFREG,
                    loaded=True,
                    materialized=True,
                    fileSize=400,
                )
            ],
            refcount=0,
        ),
    ]

    def test_multiple_trees(self) -> None:
        read_files, written_files = util.split_inodes_by_operation_type(
            self.INODE_RESULTS_2
        )
        self.assertListEqual(
            read_files, [(os.path.join("some_path", "d1", "read_file"), 300)]
        )
        self.assertListEqual(
            written_files, [(os.path.join("some_path", "d1", "written_file"), 400)]
        )

    INODE_RESULTS_3 = [
        TreeInodeDebugInfo(
            inodeNumber=1,
            path=b"some_path/d1",
            materialized=True,
            treeHash=b"abc",
            entries=[
                TreeInodeEntryDebugInfo(
                    name=b"read_file",
                    inodeNumber=2,
                    mode=stat.S_IFREG,
                    loaded=False,
                    materialized=False,
                    hash=b"1abc",
                    fileSize=300,
                ),
                TreeInodeEntryDebugInfo(
                    name=b"written_file",
                    inodeNumber=3,
                    mode=stat.S_IFREG,
                    loaded=False,
                    materialized=True,
                    fileSize=400,
                ),
            ],
            refcount=0,
        )
    ]

    def test_ignore_unloaded(self) -> None:
        read_files, written_files = util.split_inodes_by_operation_type(
            self.INODE_RESULTS_3
        )
        self.assertListEqual(read_files, [])
        self.assertListEqual(written_files, [])


class CheckHealthTest(unittest.TestCase):
    def check_health(self, get_client: MagicMock) -> util.HealthStatus:
        with tempfile.TemporaryDirectory() as config_dir:
            return util.check_health(get_client, Path(config_dir))

    def test_alive_daemon_has_no_error_fields(self) -> None:
        get_client = MagicMock()
        get_client.return_value.__enter__.return_value.getDaemonInfo.return_value = (
            DaemonInfo(pid=1234, commandLine=[], status=fb303_status.ALIVE, uptime=5.0)
        )

        self.assertEqual(
            self.check_health(get_client).telemetry_fields(),
            {"reason": "ALIVE", "pid": 1234},
        )

    def test_rpc_timeout_records_the_transport_error(self) -> None:
        timeout = TransportError(TransportErrorType.TIMED_OUT, "Timed Out", 0, 0)
        get_client = MagicMock()
        get_client.return_value.__enter__.return_value.getDaemonInfo.side_effect = (
            timeout
        )

        health = self.check_health(get_client)

        self.assertIs(health.error, timeout)
        self.assertEqual(
            health.telemetry_fields(),
            {
                "reason": "DEAD",
                "error_reason": "TransportError:TIMED_OUT",
                "error": "Timed Out",
            },
        )

    def test_connect_failure_records_the_cause(self) -> None:
        refused = TransportError(
            TransportErrorType.NOT_OPEN,
            "connect failed",
            errno.ECONNREFUSED,
            0,
        )
        get_client = MagicMock(
            side_effect=EdenNotRunningError("/home/test/.eden/socket", refused)
        )

        fields = self.check_health(get_client).telemetry_fields()

        self.assertEqual(
            fields["error_reason"],
            f"TransportError:NOT_OPEN:errno={errno.ECONNREFUSED}",
        )
        self.assertEqual(fields["error"], "connect failed")

    def test_application_error_records_its_message(self) -> None:
        get_client = MagicMock()
        get_client.return_value.__enter__.return_value.getDaemonInfo.side_effect = (
            ApplicationError(ApplicationErrorType.UNKNOWN, "boom")
        )

        fields = self.check_health(get_client).telemetry_fields()

        self.assertEqual(fields["error_reason"], "ApplicationError")
        self.assertEqual(fields["error"], "boom")

    def test_not_running_error_without_cause_describes_itself(self) -> None:
        get_client = MagicMock(
            side_effect=EdenNotRunningError("/home/test/.eden/socket")
        )

        fields = self.check_health(get_client).telemetry_fields()

        self.assertEqual(fields["error_reason"], "EdenNotRunningError")
