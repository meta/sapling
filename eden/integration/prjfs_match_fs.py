#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

from typing import List

from eden.fs.service.eden.thrift_types import (
    EdenError,
    EdenErrorType,
    FaultDefinition,
    MatchFileSystemRequest,
    MountId,
    ScmFileStatus,
)

from .lib import prjfs_test, testcase


@testcase.eden_repo_test
class PrjfsMatchFsTest(prjfs_test.PrjFSTestBase):
    """Windows fsck integration tests"""

    initial_commit: str = ""
    enable_fault_injection: bool = True

    def populate_repo(self) -> None:
        self.repo.write_file("hello", "hola\n")
        self.repo.write_file("adir/file", "foo!\n")
        self.repo.write_file("subdir/bdir/file", "foo!\n")
        self.repo.write_file("subdir/cdir/file", "foo!\n")
        self.repo.write_file(".gitignore", "ignored/\n")
        self.initial_commit = self.repo.commit("Initial commit.")

    def get_initial_commit(self) -> str:
        return self.initial_commit

    async def match_fs(self, files: List[bytes]) -> None:
        async with self.eden.get_async_thrift_client() as client:
            errors = await client.matchFilesystem(
                MatchFileSystemRequest(
                    mountPoint=MountId(mountPoint=self.mount.encode()), paths=files
                )
            )
            for error in errors.results:
                self.assertEqual(error.error, None)

    async def test_fix_no_problems(self) -> None:
        await self.assertNotInStatus(b"adir/file")

        await self.match_fs([b"adir/file"])

        await self.assertNotInStatus(b"adir/file")

    async def test_fix_missed_removal(self) -> None:
        await self.assertNotInStatus(b"adir/file")

        async with self.run_with_notifications_dropped_fault():
            afile = self.mount_path / "adir" / "file"
            afile.unlink()

        await self.assertNotInStatus(b"adir/file")
        await self.match_fs([b"adir/file"])

        self.assertEqual(
            await self.eden_status(),
            {b"adir/file": ScmFileStatus.REMOVED},
        )

    async def test_fix_missed_addition(self) -> None:
        await self.assertNotInStatus(b"adir/anewfile")

        async with self.run_with_notifications_dropped_fault():
            afile = self.mount_path / "adir" / "anewfile"
            afile.touch()

        await self.assertNotInStatus(b"adir/anewfile")
        await self.match_fs([b"adir/anewfile"])

        self.assertEqual(
            await self.eden_status(),
            {b"adir/anewfile": ScmFileStatus.ADDED},
        )

    async def test_fix_missed_directory_delete(self) -> None:
        await self.assertNotInStatus(b"adir/file")

        async with self.run_with_notifications_dropped_fault():
            adir = self.mount_path / "adir"
            afile = adir / "file"
            afile.unlink()
            adir.rmdir()

        await self.assertNotInStatus(b"adir/file")
        await self.match_fs([b"adir"])

        self.assertEqual(
            await self.eden_status(),
            {b"adir/file": ScmFileStatus.REMOVED},
        )

    async def test_fix_missed_directory_addition(self) -> None:
        await self.assertNotInStatus(b"adir/asubdir/anewfile")

        async with self.run_with_notifications_dropped_fault():
            asubdir = self.mount_path / "adir" / "asubdir"
            afile = asubdir / "anewfile"
            asubdir.mkdir()
            afile.touch()

        await self.assertNotInStatus(b"adir/asubdir/anewfile")
        await self.match_fs([b"adir/asubdir"])

        self.assertEqual(
            await self.eden_status(),
            {b"adir/asubdir/anewfile": ScmFileStatus.ADDED},
        )

    async def test_fix_failed(self) -> None:
        await self.assertNotInStatus(b"adir/file")

        async with self.run_with_notifications_dropped_fault():
            afile = self.mount_path / "adir" / "file"
            afile.unlink()

        await self.assertNotInStatus(b"adir/file")

        async with self.eden.get_async_thrift_client() as client:
            await client.injectFault(
                FaultDefinition(
                    keyClass="PrjfsDispatcherImpl::fileNotification",
                    keyValueRegex=".*",
                    errorMessage="Blocked",
                    errorType="runtime_error",
                )
            )
            errors = await client.matchFilesystem(
                MatchFileSystemRequest(
                    mountPoint=MountId(mountPoint=self.mount.encode()),
                    paths=[b"adir/file"],
                )
            )
            print(errors)
            for error in errors.results:
                self.assertEqual(
                    error.error,
                    EdenError(
                        message="class std::runtime_error: Blocked",
                        errorType=EdenErrorType.GENERIC_ERROR,
                    ),
                )
