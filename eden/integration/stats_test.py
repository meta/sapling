#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-unsafe

import logging
import os
import sys
import time
import typing
from pathlib import Path, PurePath

from eden.fs.service.eden.thrift_types import (
    GetStatInfoParams,
    JournalInfo,
    STATS_MOUNTS_STATS,
    SynchronizeWorkingCopyParams,
    TimeSpec,
)

from .lib import testcase
from .lib.hgrepo import HgRepository

Counters = typing.Mapping[str, float]

logger = logging.getLogger(__name__)


@testcase.eden_test(run_io_uring=True)
class GenericStatsTest(testcase.EdenRepoTest):
    def protocol_type(self) -> str:
        if sys.platform == "linux" or sys.platform == "darwin":
            return "nfs" if self.use_nfs() else "fuse"
        else:
            return "prjfs"

    def test_reading_committed_file_bumps_read_counter(self) -> None:
        counters_before = self.get_counters()
        path = Path(self.mount) / "file"
        path.read_bytes()

        counter_names = [
            self.protocol_type() + ".read_us.count",
            self.protocol_type() + ".lookup_us.count",
            self.protocol_type() + ".read_successful.count",
            self.protocol_type() + ".lookup_successful.count",
        ]
        for counter_name in counter_names:
            self.poll_until_counter_condition(
                lambda counters_after, counter_name=counter_name: self.assertGreater(
                    counters_after[counter_name],
                    counters_before.get(counter_name, 0),
                    f"Reading {path} should increment {counter_name}",
                )
            )

    def test_debug_counters_delta_across_a_restart(self) -> None:
        # Make the cumulative counters non-zero, then snapshot them.
        path = Path(self.mount) / "file"
        for _ in range(5):
            path.read_bytes()
        self.poll_until_counter_condition(
            lambda counters: self.assertGreater(
                counters[self.protocol_type() + ".lookup_us.count"], 0
            )
        )
        snapshot = os.path.join(self.tmp_dir, "counters_snapshot.json")
        # `eden debug counters` is a Rust command; force the Rust dispatch so
        # the test does not depend on the rollout state of this host.
        rust_env = {"EDENFSCTL_ONLY_RUST": "1"}
        self.eden.run_cmd(
            "debug",
            "counters",
            "start-recording",
            "--snapshot-path",
            snapshot,
            env=rust_env,
        )

        # A full restart starts the daemon's counters again from zero.
        self.eden.restart()

        # The later snapshot is smaller than the earlier one; the delta
        # saturates at zero instead of wrapping around or panicking.
        output = self.eden.run_cmd(
            "debug",
            "counters",
            "finalize",
            "--snapshot-path",
            snapshot,
            env=rust_env,
        )
        self.assertNotIn("1844674407", output, msg=f"output: {output}")
        self.assertNotIn("lookup", output, msg=f"output: {output}")

    def test_debug_counters_report_statfs(self) -> None:
        if self.protocol_type() != "fuse":
            self.skipTest("only FUSE exports a statfs counter")
        import json

        # Issue statfs calls on the mount and wait for the daemon to count them.
        for _ in range(5):
            os.statvfs(self.mount)
        self.poll_until_counter_condition(
            lambda counters: self.assertGreater(
                counters["fuse.statfs_successful.sum"], 0
            )
        )

        # `eden debug counters` is a Rust command; force the Rust dispatch so
        # the test does not depend on the rollout state of this host.
        output = self.eden.run_cmd(
            "debug",
            "counters",
            "from-start",
            "--full",
            env={"EDENFSCTL_ONLY_RUST": "1"},
        )
        fs_stats = json.loads(output)["fs_stats"]
        # The statfs counter is never requested from the daemon, so it is
        # reported as 0 although the daemon counted the calls above.
        self.assertEqual(fs_stats["syscall_statfs"], 0, msg=f"output: {output}")
        self.assertGreater(fs_stats["syscall_lookup"], 0, msg=f"output: {output}")

    def test_writing_untracked_file_bumps_write_counter(self) -> None:
        counters_before = self.get_counters()
        path = Path(self.mount) / "new_file"
        path.write_bytes(b"hello")

        if self.protocol_type() == "prjfs":
            counter_names = [
                self.protocol_type() + ".newFileCreated_us.count",
                self.protocol_type() + ".fileHandleClosedFileModified_us.count",
                self.protocol_type() + ".lookup_us.count",
                self.protocol_type() + ".newFileCreated_successful.count",
                self.protocol_type() + ".fileHandleClosedFileModified_successful.count",
                self.protocol_type() + ".lookup_successful.count",
            ]
        else:
            counter_names = [
                self.protocol_type() + ".write_us.count",
                self.protocol_type() + ".lookup_us.count",
                self.protocol_type() + ".getattr_us.count",
                self.protocol_type() + ".create_us.count",
                self.protocol_type() + ".write_successful.count",
                self.protocol_type() + ".lookup_successful.count",
                self.protocol_type() + ".getattr_successful.count",
                self.protocol_type() + ".create_successful.count",
            ]
        for counter_name in counter_names:
            self.poll_until_counter_condition(
                lambda counters_after, counter_name=counter_name: self.assertGreater(
                    counters_after[counter_name],
                    counters_before.get(counter_name, 0),
                    f"Writing to {path} should increment {counter_name}",
                )
            )

    def test_summary_counters_available(self) -> None:
        mountName = PurePath(self.mount).name
        protocol_name = self.protocol_type()
        counter_names_to_check = [
            f"{protocol_name}.{mountName}.live_requests.count",
            f"{protocol_name}.{mountName}.live_requests.max_duration_us",
            f"{protocol_name}.{mountName}.pending_requests.count",
        ]

        counters = self.get_counters()

        for counter_name in counter_names_to_check:
            self.assertIn(counter_name, counters, f"{counter_name} should be available")

    def create_repo(self, name: str) -> HgRepository:
        return self.create_hg_repo(name)

    def create_eden_repo(self) -> HgRepository:
        return HgRepository(
            self.mount,
            system_hgrc=self.system_hgrc,
            filtered=self.backing_store_type == "filteredhg",
        )

    def populate_repo(self) -> None:
        self.repo.write_file("file", "hello world!\n")
        self.repo.commit("Initial commit.")

    def poll_until_counter_condition(
        self, assertion_condition: typing.Callable[[Counters], None]
    ) -> None:
        timeout_seconds = 2.0
        poll_interval_seconds = 0.1
        deadline = time.monotonic() + timeout_seconds
        while True:
            counters = self.get_counters()
            try:
                assertion_condition(counters)
                break
            except AssertionError as e:
                if time.monotonic() >= deadline:
                    raise
                logger.info(
                    f"Assertion failed. Waiting {poll_interval_seconds} "
                    f"seconds before trying again. {e}"
                )
                time.sleep(poll_interval_seconds)
                continue


@testcase.eden_nfs_repo_test
class ObjectStoreStatsTest(testcase.EdenRepoTest):
    def create_repo(self, name: str) -> HgRepository:
        return self.create_hg_repo(name)

    def create_eden_repo(self) -> HgRepository:
        return HgRepository(
            self.mount,
            system_hgrc=self.system_hgrc,
            filtered=self.backing_store_type == "filteredhg",
        )

    def populate_repo(self) -> None:
        self.repo.write_file("foo.txt", "foo\n")

        self.repo.commit("Initial commit.")

    def test_get_blob(self) -> None:
        TEMPLATE = "object_store.get_blob.{}_store.count"
        LOCAL = TEMPLATE.format("local")
        BACKING = TEMPLATE.format("backing")

        counters = self.get_counters()
        self.assertEqual(counters.get(LOCAL, 0) + counters.get(BACKING, 0), 0)

        foo = Path(self.mount) / "foo.txt"
        foo.read_bytes()

        counters = self.get_counters()
        self.assertEqual(counters.get(LOCAL, 0) + counters.get(BACKING, 0), 1)


@testcase.eden_test(run_io_uring=True)
class ObjectCacheStatsTest(testcase.EdenRepoTest):
    def create_repo(self, name: str) -> HgRepository:
        return self.create_hg_repo(name)

    def create_eden_repo(self) -> HgRepository:
        return HgRepository(
            self.mount,
            system_hgrc=self.system_hgrc,
            filtered=self.backing_store_type == "filteredhg",
        )

    def populate_repo(self) -> None:
        self.repo.mkdir("dir")
        self.repo.write_file("dir/one.txt", "1\n")
        self.repo.write_file("dir/two.txt", "2\n")

        self.repo.commit("Initial commit.")

    async def test_get_tree_memory(self) -> None:
        MEMORY_COUNTER = "object_store.get_tree.memory.count"

        list(os.scandir(Path(self.mount) / "dir"))

        initial_count = self.get_counters().get(MEMORY_COUNTER, 0)

        # To exercise the in-memory tree cache we have to first unload the
        # corresponding inodes (which contains its own cache of directory
        # entries) and OS kernel caches.
        async with self.get_async_thrift_client() as thrift_client:
            await thrift_client.unloadInodeForPath(
                self.mount.encode("utf-8"), b"", TimeSpec(seconds=0, nanoSeconds=0)
            )

            await thrift_client.invalidateKernelInodeCache(
                self.mount.encode("utf-8"), b"dir"
            )

        # List the directory again, which should result in a TreeCache hit this
        # time around.  We use os.scandir because Path.glob seems to do some
        # caching of its own?
        list(os.scandir(Path(self.mount) / "dir"))

        final_count = self.get_counters().get(MEMORY_COUNTER, 0)
        self.assertTrue(final_count > initial_count)


@testcase.eden_test(run_io_uring=True)
class FSChannelStatsTest(testcase.EdenRepoTest):
    def protocol_type(self) -> str:
        if sys.platform == "linux" or sys.platform == "darwin":
            return "nfs" if self.use_nfs() else "fuse"
        else:
            return "prjfs"

    def getFuseCounters(self) -> typing.List[str]:
        return [
            "lookup_",
            "forget_",
            "getattr_",
            "setattr_",
            "readlink_",
            "mknod_",
            "mkdir_",
            "unlink_",
            "rmdir_",
            "symlink_",
            "rename_",
            "link_",
            "open_",
            "read_",
            "write_",
            "flush_",
            "release_",
            "fsync_",
            "opendir_",
            "readdir_",
            "releasedir_",
            "fsyncdir_",
            "statfs_",
            "setxattr_",
            "getxattr_",
            "listxattr_",
            "removexattr_",
            "access_",
            "create_",
            "bmap_",
            "forgetmulti_",
            "fallocate_",
        ]

    def getNfsCounters(self) -> typing.List[str]:
        return [
            "null_",
            "getattr_",
            "setattr_",
            "lookup_",
            "access_",
            "readlink_",
            "read_",
            "write_",
            "create_",
            "mkdir_",
            "symlink_",
            "mknod_",
            "remove_",
            "rmdir_",
            "rename_",
            "link_",
            "readdir_",
            "readdirplus_",
            "fsstat_",
            "fsinfo_",
            "pathconf_",
            "commit_",
        ]

    def getPrjfsCounters(self) -> typing.List[str]:
        return [
            "filesystem_sync_",
            "newFileCreated_",
            "fileOverwritten_",
            "fileHandleClosedFileModified_",
            "fileRenamed_",
            "preDelete_",
            "preRenamed_",
            "fileHandleClosedFileDeleted_",
            "preSetHardlink_",
            "preConvertToFull_",
            "opendir_",
            "readdir_",
            "lookup_",
            "access_",
            "read_",
            "remove_cached_file_",
            "add_directory_placeholder_",
        ]

    def test_fschannel_counters_available(self) -> None:
        counters = self.get_counters()
        template_duration = self.protocol_type() + ".{}us.count"
        template_successful = self.protocol_type() + ".{}successful.count"
        template_failure = self.protocol_type() + ".{}failure.count"
        if sys.platform == "linux":
            platform_counters = self.getFuseCounters()
        if sys.platform == "win32":
            platform_counters = self.getPrjfsCounters()
        if sys.platform == "darwin":
            platform_counters = self.getNfsCounters()
        for counter in platform_counters:
            duration_counter_name = template_duration.format(counter)
            successful_counter_name = template_successful.format(counter)
            failure_counter_name = template_failure.format(counter)
            self.assertIn(
                duration_counter_name,
                counters,
                f"{duration_counter_name} should be available",
            )
            self.assertIn(
                successful_counter_name,
                counters,
                f"{successful_counter_name} should be available",
            )
            self.assertIn(
                failure_counter_name,
                counters,
                f"{failure_counter_name} should be available",
            )

    def create_repo(self, name: str) -> HgRepository:
        return self.create_hg_repo(name)

    def create_eden_repo(self) -> HgRepository:
        return HgRepository(
            self.mount,
            system_hgrc=self.system_hgrc,
            filtered=self.backing_store_type == "filteredhg",
        )

    def populate_repo(self) -> None:
        # This file evades EdenFS' automatic prefetching by being two levels
        # inside the root.
        self.repo.write_file("dir/subdir/file", "hello world!\n")

        self.repo.commit("Initial commit.")


@testcase.eden_nfs_repo_test
class SaplingBackingStoreStatsTest(testcase.EdenRepoTest):
    def test_reading_file_bump_sapling_stats(self) -> None:
        counters_before = self.get_counters()
        path = Path(self.mount) / "dir" / "subdir" / "file"
        path.read_bytes()
        counters_after = self.get_counters()

        TEMPLATE = "store.sapling.{}.count"
        for counter, init_val, incr_val in self.getSaplingCounters():
            counter_name = TEMPLATE.format(counter)
            self.assertEqual(
                counters_before[counter_name],
                init_val,
                f"Reading {path} should increment {counter_name}",
            )
            self.assertEqual(
                counters_after[counter_name],
                counters_before.get(counter_name, 0) + incr_val,
                f"Reading {path} should increment {counter_name}",
            )

    def getSaplingCounters(self) -> typing.List[typing.Tuple[str, int, int]]:
        # (counter, init_val, incr_val)
        return [
            # Two trees are fetched
            ("get_tree_us", 0, 2),
            ("fetch_tree_success", 0, 2),
            ("fetch_tree_local", 0, 2),
            # The root tree is only fetched once in the initial checkout
            ("get_root_tree_us", 1, 0),
            ("get_root_tree_success", 1, 0),
            ("get_root_tree_local", 1, 0),
            # One blob is fetched
            ("get_blob_us", 0, 1),
            ("fetch_blob_success", 0, 1),
            ("fetch_blob_local", 0, 1),
            # One blob metadata is fetched
            ("get_blob_metadata_us", 0, 1),
            ("fetch_blob_metadata_success", 0, 1),
            ("fetch_blob_metadata_local", 0, 1),
        ]

    def test_pending_import_counters_available(self) -> None:
        counters = self.get_counters()

        counter_names_to_check = [
            "store.sapling.pending_import.blob.count",
            "store.sapling.pending_import.blobmeta.count",
            "store.sapling.pending_import.tree.count",
            "store.sapling.pending_import.prefetch.count",
            "store.sapling.pending_import.count",
            "store.sapling.pending_import.blob.max_duration_us",
            "store.sapling.pending_import.blobmeta.max_duration_us",
            "store.sapling.pending_import.tree.max_duration_us",
            "store.sapling.pending_import.prefetch.max_duration_us",
            "store.sapling.pending_import.max_duration_us",
            "store.sapling.live_import.blob.count",
            "store.sapling.live_import.tree.count",
            "store.sapling.live_import.prefetch.count",
            "store.sapling.live_import.count",
            "store.sapling.live_import.blob.max_duration_us",
            "store.sapling.live_import.tree.max_duration_us",
            "store.sapling.live_import.prefetch.max_duration_us",
            "store.sapling.live_import.max_duration_us",
            "store.sapling.live_import.batched_blob.count",
            "store.sapling.live_import.batched_tree.count",
            "store.sapling.live_import.batched_blobmeta.count",
            "store.sapling.live_import.batched_blob.max_duration_us",
            "store.sapling.live_import.batched_tree.max_duration_us",
            "store.sapling.live_import.batched_blobmeta.max_duration_us",
        ]

        for counter_name in counter_names_to_check:
            self.assertIn(counter_name, counters, f"{counter_name} should be available")

    def create_repo(self, name: str) -> HgRepository:
        return self.create_hg_repo(name)

    def create_eden_repo(self) -> HgRepository:
        return HgRepository(
            self.mount,
            system_hgrc=self.system_hgrc,
            filtered=self.backing_store_type == "filteredhg",
        )

    def populate_repo(self) -> None:
        # This file evades EdenFS' automatic prefetching by being two levels
        # inside the root.
        self.repo.write_file("dir/subdir/file", "hello world!\n")

        self.repo.commit("Initial commit.")


@testcase.eden_repo_test
class JournalInfoTest(testcase.EdenRepoTest):
    async def test_journal_info(self) -> None:
        journal = await self.journal_stats()
        old_mem = journal.memoryUsage
        old_data_counts = journal.entryCount
        path = Path(self.mount) / "new_file"
        path.write_bytes(b"hello")
        journal = await self.journal_stats()
        new_mem = journal.memoryUsage
        new_data_counts = journal.entryCount
        self.assertLess(
            old_data_counts,
            new_data_counts,
            "Changing the repo should cause entry count to increase",
        )
        self.assertLess(
            old_mem, new_mem, "Changing the repo should cause memory usage to increase"
        )

    async def journal_stats(self) -> JournalInfo:
        async with self.get_async_thrift_client() as thrift_client:
            await thrift_client.synchronizeWorkingCopy(
                self.mount.encode("utf-8"), SynchronizeWorkingCopyParams()
            )

            stats = await thrift_client.getStatInfo(
                GetStatInfoParams(statsMask=STATS_MOUNTS_STATS)
            )
            journal_key = self.mount.encode()
            mountPointJournalInfo = stats.mountPointJournalInfo
            journal = (
                None
                if mountPointJournalInfo is None
                else mountPointJournalInfo[journal_key]
            )
            self.assertIsNotNone(journal, "Journal does not exist")
            return journal

    def populate_repo(self) -> None:
        self.repo.write_file("file", "hello world!\n")
        self.repo.commit("Initial commit.")


@testcase.eden_repo_test
class CountersTest(testcase.EdenRepoTest):
    """Test counters are registered/unregistered correctly."""

    def populate_repo(self) -> None:
        self.repo.write_file("hello", "hola\n")
        self.repo.commit("Initial commit.")

    # We filter out counters that sporadically appear due to background
    # infrastructure activity, since they can appear between counters and
    # counters2 and cause test flakiness:
    # - scribe.*: Scribe logging counters
    # - thrift.*: Thrift RPC counters
    # - keystore.*: from Sapling keystore operations
    # - memory_vm_rss_bytes: reported sporadically in background
    # - obc.*: OBC monitoring system counters, added in D84351520
    #          to track counter growth across services
    @staticmethod
    def get_nonthrift_set(s):
        return {
            item
            for item in s
            if not item.startswith("scribe.")
            and not item.startswith("thrift.")
            and not item.startswith("keystore.")
            and not item.startswith("memory_vm_rss_bytes")
            and not item.startswith("obc.")
        }

    def test_mount_unmount_counters(self) -> None:
        self.eden.unmount(self.mount_path)
        counters = self.get_nonthrift_set(self.get_counters().keys())
        mount2 = os.path.join(self.mounts_dir, "mount2")
        self.eden.clone(self.repo.path, mount2)
        self.eden.unmount(Path(mount2))
        counters2 = self.get_nonthrift_set(self.get_counters().keys())
        self.assertEqual(counters, counters2)
