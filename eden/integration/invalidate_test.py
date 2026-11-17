#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import contextlib
import os
import sys
import time
from typing import Dict, Iterator, List

from eden.fs.service.eden.thrift_types import (
    DebugInvalidateRequest,
    GetStatInfoParams,
    MountId,
    STATS_MOUNTS_STATS,
    TimeSpec,
)

from .lib import testcase


@testcase.eden_repo_test
class InvalidateTest(testcase.EdenRepoTest):
    directories: List[str] = ["a", "b", "c"]
    num_files: int = 10

    def edenfs_logging_settings(self) -> Dict[str, str]:
        return {
            "eden.fs.inodes.TreeInode": "DBG5",
        }

    def populate_repo(self) -> None:
        for directory in self.directories:
            for i in range(self.num_files):
                self.repo.write_file(f"{directory}/{i}", f"{i}\n")
        self.repo.commit("Initial commit.")

    async def get_loaded_count(self) -> int:
        async with self.get_async_thrift_client() as client:
            stats = await client.getStatInfo(
                GetStatInfoParams(statsMask=STATS_MOUNTS_STATS)
            )
        mountPointInfo = stats.mountPointInfo
        if mountPointInfo is None:
            raise Exception("stats.mountPointInfo is not set")
        self.assertEqual(len(mountPointInfo), 1)
        for mountPath in mountPointInfo:
            info = mountPointInfo[mountPath]
            return info.loadedFileCount + info.loadedTreeCount
        return 0  # Apppease pyre

    async def assert_invalidation(
        self,
        invalidated: int,
        expected_invalidated_darwin: int,
        expected_invalidated_windows: int,
        expected_invalidated_linux: int,
        expected_loaded_darwin: int,
        expected_loaded_windows: int,
        expected_loaded_linux: int,
        delta: int = 0,
        darwin_bounds: bool = False,
    ) -> None:
        """
        On macOS and Windows, both trees and files are invalidated.
        On macOS, the count is the number of FS references the
        invalidation cleared: one per file under an invalidated
        directory. Directories stay referenced: without pressure GC there is
        no pin set, and the NFS pass only clears directories it knows are
        not in use. When the whole checkout is invalidated, whatever files
        under .hg the tools have read during the test are reclaimed too, and
        how many were loaded varies, so with darwin_bounds the count is a
        lower bound and the loaded count an upper bound.
        On Linux, we don't invalidate any inode as the first step of GC.
        Because FUSE decrease the inode FS refcounts when needed

        Additionally, when all inodes are invalidated, GC
        unload behavior for top-level directories differs by
        platform, so the expected loaded count after GC can vary.
        """
        if sys.platform == "darwin":
            expected_invalidated = expected_invalidated_darwin
            expected_loaded = expected_loaded_darwin
        elif sys.platform == "win32":
            expected_invalidated = expected_invalidated_windows
            expected_loaded = expected_loaded_windows
        else:
            expected_invalidated = expected_invalidated_linux
            expected_loaded = expected_loaded_linux
        if sys.platform == "darwin" and darwin_bounds:
            self.assertGreaterEqual(invalidated, expected_invalidated)
            self.assertLessEqual(await self.get_loaded_count(), expected_loaded)
            return
        self.assertEqual(invalidated, expected_invalidated)
        self.assertAlmostEqual(
            await self.get_loaded_count(), expected_loaded, delta=delta
        )

    async def invalidate(
        self, path: str, seconds: int = 0, background: bool = False
    ) -> int:
        async with self.get_async_thrift_client() as client:
            result = await client.debugInvalidateNonMaterialized(
                DebugInvalidateRequest(
                    mount=MountId(mountPoint=self.mount_path_bytes),
                    path=os.fsencode(path),
                    age=TimeSpec(seconds=seconds, nanoSeconds=0),
                    background=background,
                )
            )
            return result.numInvalidated

    def read_directory(
        self, directory: str, start: int = 0, stop: int = num_files
    ) -> None:
        for i in range(start, stop):
            content = self.read_file(f"{directory}/{i}")
            self.assertEqual(content, f"{i}\n")

    def read_all(self) -> None:
        for directory in self.directories:
            self.read_directory(directory)

    @contextlib.contextmanager
    def keep_open_on_linux(
        self, directory: str, start: int = 0, stop: int = num_files
    ) -> Iterator[None]:
        """Keep files `start` to `stop - 1` of `directory` open on Linux."""
        with contextlib.ExitStack() as stack:
            if sys.platform == "linux":
                # The kernel can forget an inode that has no open file.
                # Then EdenFS GC can unload the inode.
                # A recent read of the file does not prevent the unload.
                for i in range(start, stop):
                    stack.enter_context(open(self.get_path(f"{directory}/{i}")))
            yield

    async def test_invalidate_all(self) -> None:
        initial_loaded = await self.get_loaded_count()
        self.read_all()
        self.assertEqual(await self.get_loaded_count(), initial_loaded + 33)
        invalidated = await self.invalidate("")
        await self.assert_invalidation(
            invalidated,
            expected_invalidated_darwin=30,
            expected_invalidated_windows=33,
            expected_invalidated_linux=0,
            expected_loaded_darwin=initial_loaded + 2,
            expected_loaded_windows=initial_loaded - 1,
            expected_loaded_linux=1,
            darwin_bounds=True,
        )
        self.read_all()

    async def test_invalidate_subdir(self) -> None:
        initial_loaded = await self.get_loaded_count()
        self.read_all()
        self.assertEqual(await self.get_loaded_count(), initial_loaded + 33)
        invalidated = await self.invalidate("a")
        await self.assert_invalidation(
            invalidated,
            expected_invalidated_darwin=10,
            expected_invalidated_windows=10,
            expected_invalidated_linux=0,
            expected_loaded_darwin=initial_loaded + 23,
            expected_loaded_windows=initial_loaded + 23,
            expected_loaded_linux=initial_loaded + 23,
        )
        self.read_all()

    async def test_no_invalidation_with_age(self) -> None:
        initial_loaded = await self.get_loaded_count()
        self.read_all()
        with self.keep_open_on_linux("a"):
            self.assertEqual(await self.get_loaded_count(), initial_loaded + 33)
            invalidated = await self.invalidate("a", seconds=3600)
            self.assertEqual(invalidated, 0)
            self.assertEqual(await self.get_loaded_count(), initial_loaded + 33)

    async def test_invalidate_with_age(self) -> None:
        initial_loaded = await self.get_loaded_count()
        self.read_all()
        self.assertEqual(await self.get_loaded_count(), initial_loaded + 33)
        time.sleep(10)
        invalidated = await self.invalidate("a", seconds=5)
        await self.assert_invalidation(
            invalidated,
            expected_invalidated_darwin=10,
            expected_invalidated_windows=10,
            expected_invalidated_linux=0,
            expected_loaded_darwin=initial_loaded + 23,
            expected_loaded_windows=initial_loaded + 23,
            expected_loaded_linux=initial_loaded + 23,
        )
        self.read_all()

    async def test_partial_invalidate(self) -> None:
        initial_loaded = await self.get_loaded_count()
        self.read_directory("a")
        self.assertEqual(await self.get_loaded_count(), initial_loaded + 11)
        time.sleep(10)
        self.read_directory("b")
        with self.keep_open_on_linux("b"):
            self.assertEqual(await self.get_loaded_count(), initial_loaded + 22)
            invalidated = await self.invalidate("", seconds=5)
            await self.assert_invalidation(
                invalidated,
                expected_invalidated_darwin=10,
                expected_invalidated_windows=11,
                expected_invalidated_linux=0,
                expected_loaded_darwin=initial_loaded + 11,
                expected_loaded_windows=initial_loaded + 10,
                expected_loaded_linux=initial_loaded + 5,
                # different test flavors can have different initial_loaded.
                # We use delta=1 to cover all the test flavors.
                delta=1,
            )
        self.read_all()

    async def test_partial_directory_invalidate(self) -> None:
        initial_loaded = await self.get_loaded_count()
        self.read_directory("a", 0, 6)
        self.assertEqual(await self.get_loaded_count(), initial_loaded + 7)
        time.sleep(10)
        self.read_directory("a", 6)
        with self.keep_open_on_linux("a", 6):
            self.assertEqual(await self.get_loaded_count(), initial_loaded + 11)
            invalidated = await self.invalidate("a", seconds=5)
            # On macOS, the lastAccessedTime of the directory is the latest lastAccessedTime
            # of its children. Therefore, as one of the children is accessed after the cutoff,
            # the whole directory is not invalidated.
            await self.assert_invalidation(
                invalidated,
                expected_invalidated_darwin=0,
                expected_invalidated_windows=6,
                expected_invalidated_linux=0,
                expected_loaded_darwin=initial_loaded + 11,
                expected_loaded_windows=initial_loaded + 5,
                expected_loaded_linux=initial_loaded + 5,
            )
        self.read_all()

    async def test_invalidate_background(self) -> None:
        """Verify that starting an invalidation in the background doesn't crash EdenFS."""
        self.read_all()
        await self.invalidate("", seconds=10, background=True)
        time.sleep(2)

    async def test_invalidate_keep_timestamp(self) -> None:
        self.read_all()
        st_before = os.stat(self.get_path("a/1"))
        time.sleep(5)
        await self.invalidate("", seconds=0)
        st_after = os.stat(self.get_path("a/1"))

        self.assertEqual(st_before.st_mtime, st_after.st_mtime)
        # pyre-fixme[16]: `stat_result` has no attribute `st_ctime`.
        self.assertEqual(st_before.st_ctime, st_after.st_ctime)
