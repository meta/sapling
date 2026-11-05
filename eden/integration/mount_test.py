#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

from __future__ import annotations

import errno
import os
import re
import shutil
import subprocess
import sys
import unittest
from pathlib import Path
from typing import Any, Dict, List, Optional, Set
from unittest import mock

from eden.fs.cli import mtab
from eden.fs.cli.config import EdenCheckout, EdenInstance
from eden.fs.cli.doctor import check_stale_mounts
from eden.fs.cli.util import poll_until_async
from eden.fs.service.eden.thrift_clients import EdenService
from eden.fs.service.eden.thrift_types import (
    EdenError,
    EdenErrorType,
    FaultDefinition,
    GetScmStatusParams,
    MountState,
    RemoveFaultArg,
    ResetParentCommitsParams,
    SyncBehavior,
    UnblockFaultArg,
    WorkingDirectoryParents,
)
from fb303_core.thrift_types import fb303_status
from thrift.python.exceptions import TransportError

from .lib import testcase
from .lib.edenclient import EdenCommandError


@testcase.eden_repo_test
class MountTest(testcase.EdenRepoTest):
    # pyre-fixme[13]: Attribute `expected_mount_entries` is never initialized.
    expected_mount_entries: Set[str]
    enable_fault_injection: bool = True

    def edenfs_extra_config(self) -> Optional[Dict[str, List[str]]]:
        configs = super().edenfs_extra_config() or {}
        configs.setdefault("fuse", []).append('vfs-type = "fuse.edenfs"')
        return configs

    def populate_repo(self) -> None:
        self.maxDiff = None
        self.repo.write_file("hello", "hola\n")
        self.repo.write_file("adir/file", "foo!\n")
        self.repo.write_file("bdir/test.sh", "#!/bin/bash\necho test\n", mode=0o755)
        self.repo.write_file("bdir/noexec.sh", "#!/bin/bash\necho test\n")
        self.repo.symlink("slink", "hello")
        self.repo.commit("Initial commit.")

        self.expected_mount_entries = {".eden", "adir", "bdir", "hello", "slink"}

    def test_remove_unmounted_checkout(self) -> None:
        # Clone a second checkout mount point
        mount2 = os.path.join(self.mounts_dir, "mount2")
        self.eden.clone(self.repo.path, mount2)
        self.assertEqual(
            {self.mount: "RUNNING", mount2: "RUNNING"}, self.eden.list_cmd_simple()
        )

        # Now unmount it
        self.eden.run_cmd("unmount", mount2)
        self.assertEqual(
            {self.mount: "RUNNING", mount2: "NOT_RUNNING"}, self.eden.list_cmd_simple()
        )
        # The Eden README telling users what to do if their mount point is not mounted
        # should be present in the original mount point directory.
        self.assertTrue(os.path.exists(os.path.join(mount2, "README_EDEN.txt")))

        # Now use "eden remove" to destroy mount2
        self.eden.remove(mount2)
        self.assertEqual({self.mount: "RUNNING"}, self.eden.list_cmd_simple())
        self.assertFalse(os.path.exists(mount2))

    def test_unmount_remount(self) -> None:
        # write a file into the overlay to test that it is still visible
        # when we remount.
        filename = os.path.join(self.mount, "overlayonly")
        with open(filename, "w") as f:
            f.write("foo!\n")

        self.assert_checkout_root_entries(self.expected_mount_entries | {"overlayonly"})
        self.assertTrue(self.eden.in_proc_mounts(self.mount))

        # do a normal user-facing unmount, preserving state
        self.eden.run_cmd("unmount", self.mount)

        self.assertFalse(self.eden.in_proc_mounts(self.mount))
        entries = set(os.listdir(self.mount))
        self.assertEqual({"README_EDEN.txt"}, entries)

        # Now remount it with the mount command
        self.eden.run_cmd("mount", self.mount)

        self.assertTrue(self.eden.in_proc_mounts(self.mount))
        self.assert_checkout_root_entries(self.expected_mount_entries | {"overlayonly"})

        with open(filename, "r") as f:
            self.assertEqual("foo!\n", f.read(), msg="overlay file is correct")

    def test_double_unmount(self) -> None:
        # Test calling "unmount" twice.  The second should fail, but edenfs
        # should still work normally afterwards
        self.eden.run_cmd("unmount", self.mount)
        self.eden.run_unchecked("unmount", self.mount)

        # Now remount it with the mount command
        self.eden.run_cmd("mount", self.mount)

        self.assertTrue(self.eden.in_proc_mounts(self.mount))
        self.assert_checkout_root_entries({".eden", "adir", "bdir", "hello", "slink"})

    def test_unmount_succeeds_while_file_handle_is_open(self) -> None:
        fd = os.open(os.path.join(self.mount, "hello"), os.O_RDWR)
        # This test will fail or time out if unmounting times out.
        self.eden.run_cmd("unmount", self.mount)
        # Surprisingly, os.close does not return an error when the mount has
        # gone away.
        os.close(fd)

    def test_unmount_succeeds_while_dir_handle_is_open(self) -> None:
        fd = os.open(self.mount, 0)
        # This test will fail or time out if unmounting times out.
        self.eden.run_cmd("unmount", self.mount)
        # Surprisingly, os.close does not return an error when the mount has
        # gone away.
        os.close(fd)

    async def test_mount_init_state(self) -> None:
        await self._exercise_mount_init_state()

    async def test_mount_init_fallback_reaps_cli(self) -> None:
        processes = []
        popen = subprocess.Popen

        def record_process(*args: Any, **kwargs: Any) -> subprocess.Popen:
            process = popen(*args, **kwargs)
            processes.append(process)
            self.addCleanup(process.wait, timeout=30)
            return process

        with (
            mock.patch.object(subprocess, "Popen", side_effect=record_process),
            mock.patch.object(
                self.eden,
                "assert_running_fuse_transports",
                side_effect=unittest.SkipTest("simulated devfuse fallback"),
            ) as verify_transport,
        ):
            with self.assertRaisesRegex(
                unittest.SkipTest, "simulated devfuse fallback"
            ):
                await self._exercise_mount_init_state()
        verify_transport.assert_called_once()
        self.assertTrue(processes)
        for process in processes:
            self.assertIsNotNone(process.returncode, f"unreaped CLI: {process.args}")

    async def _exercise_mount_init_state(self) -> None:
        self.eden.run_cmd("unmount", self.mount)
        self.assertEqual({self.mount: "NOT_RUNNING"}, self.eden.list_cmd_simple())

        async with self.eden.get_async_thrift_client() as client:
            fault = FaultDefinition(keyClass="mount", keyValueRegex=".*", block=True)
            await client.injectFault(fault)

            # Run the "eden mount" CLI command.
            # This won't succeed until we unblock the mount.
            mount_cmd, edenfsctl_env = self.eden.get_edenfsctl_cmd_env(
                "mount", self.mount
            )
            mount_proc = subprocess.Popen(mount_cmd, env=edenfsctl_env)

            # Wait for the new mount to be reported by edenfs
            async def mount_started() -> Optional[bool]:
                if (
                    await self.eden.get_mount_state_async(Path(self.mount), client)
                    is not None
                ):
                    return True
                if mount_proc.poll() is not None:
                    raise Exception(
                        f"eden mount command finished (with status "
                        f"{mount_proc.returncode}) while mounting was "
                        f"still blocked"
                    )
                return None

            await poll_until_async(mount_started, timeout=30)
            self.wait_on_fault_hit(key_class="mount")
            self.assertEqual({self.mount: "INITIALIZING"}, self.eden.list_cmd_simple())

            # Most thrift calls to access the mount should be disallowed while it is
            # still initializing.
            await self._assert_thrift_calls_fail_during_mount_init(client)

            await client.unblockFault(
                UnblockFaultArg(keyClass="mount", keyValueRegex=".*")
            )
            mount_proc.wait(timeout=60)
            await self._wait_for_mount_running(client)

            self.assertEqual({self.mount: "RUNNING"}, self.eden.list_cmd_simple())

    async def _assert_thrift_calls_fail_during_mount_init(self, client) -> None:
        error_regex = "mount point .* is still initializing"
        mount_path = Path(self.mount)
        null_commit = b"\00" * 20

        with self.assertRaisesRegex(EdenError, error_regex) as ctx:
            await client.getFileInformation(
                mountPoint=bytes(mount_path), paths=[b""], sync=SyncBehavior()
            )
        self.assertEqual(EdenErrorType.POSIX_ERROR, ctx.exception.errorType)

        with self.assertRaisesRegex(EdenError, error_regex) as ctx:
            await client.getScmStatusV2(
                GetScmStatusParams(
                    mountPoint=bytes(self.mount, encoding="utf-8"),
                    commit=null_commit,
                    listIgnored=False,
                    rootIdOptions=None,
                )
            )
        self.assertEqual(EdenErrorType.POSIX_ERROR, ctx.exception.errorType)

        parents = WorkingDirectoryParents(parent1=null_commit)
        params = ResetParentCommitsParams()
        with self.assertRaisesRegex(EdenError, error_regex) as ctx:
            await client.resetParentCommits(
                mountPoint=bytes(mount_path), parents=parents, params=params
            )
        self.assertEqual(EdenErrorType.POSIX_ERROR, ctx.exception.errorType)

    async def _wait_until_initializing(self, num_mounts: int = 1) -> None:
        """Wait until EdenFS is initializing mount points.
        This is primarily intended to be used to wait until the mount points are
        initializing when starting EdenFS with --fault_injection_block_mounts.
        """

        async def is_initializing() -> Optional[bool]:
            try:
                async with self.eden.get_async_thrift_client() as client:
                    # Return successfully when listMounts() reports the number of
                    # mounts that we expect.
                    mounts = await client.listMounts()
                    if len(mounts) == num_mounts:
                        return True
                edenfs_process = self.eden._process
                assert edenfs_process is not None
                if edenfs_process.poll():
                    self.fail("eden exited before becoming healthy")
                return None
            except TransportError:
                return None

        await poll_until_async(is_initializing, 60)

    async def test_start_blocked_mount_init(self) -> None:
        self.eden.shutdown()
        self.eden.spawn_nowait(
            extra_args=["--enable_fault_injection", "--fault_injection_block_mounts"]
        )

        # Wait for eden to report the mount point in the listMounts() output
        await self._wait_until_initializing()

        async with self.eden.get_async_thrift_client() as client:
            # Since we blocked mount initialization the mount should still
            # report as INITIALIZING, and edenfs should report itself STARTING
            self.assertEqual(
                {self.mount: "INITIALIZING"},
                self.eden.list_cmd_simple({"EDENFS_SKIP_DAEMON_READY_CHECK": "1"}),
            )
            daemon_info = await client.getDaemonInfo()
            self.assertEqual(fb303_status.STARTING, daemon_info.status)

            # Unblock mounting and wait for the mount to transition to running
            await client.unblockFault(
                UnblockFaultArg(keyClass="mount", keyValueRegex=".*")
            )
            await self._wait_for_mount_running(client)
            await self._wait_until_alive(client)
            daemon_info = await client.getDaemonInfo()
            self.assertEqual(fb303_status.ALIVE, daemon_info.status)

        self.assertEqual({self.mount: "RUNNING"}, self.eden.list_cmd_simple())

    async def test_remount_after_initialization_failure(self) -> None:
        # Unmount and inject a fault that blocks subsequent mount attempts
        self.eden.run_cmd("unmount", self.mount)
        async with self.eden.get_async_thrift_client() as client:
            fault = FaultDefinition(
                keyClass="failMountInitialization",
                keyValueRegex=".*",
                errorType="runtime_error",
                errorMessage="PC LOAD LETTER",
            )
            await client.injectFault(fault)

        # Run the "eden mount" CLI command.
        # This will fail because we injected an exception
        with self.assertRaises(EdenCommandError):
            self.eden.run_cmd("mount", self.mount)

        # Remove the previously added fault
        async with self.eden.get_async_thrift_client() as client:
            await client.removeFault(
                RemoveFaultArg(keyClass="failMountInitialization", keyValueRegex=".*")
            )

        # A subsequent attempt to remount should not crash the Eden daemon
        self.eden.wait_for_checkout_removed(self.mount)
        self.eden.run_cmd("mount", self.mount)

        # Eden should be running and the checkout should be mounted
        self.assertEqual(True, self.eden.is_healthy())
        self.assertEqual(
            {self.mount: "RUNNING"},
            self.eden.list_cmd_simple(),
        )

    async def _wait_for_mount_running(
        self, client, path: Optional[Path] = None
    ) -> None:
        mount_path = path if path is not None else Path(self.mount)

        async def mount_running() -> Optional[bool]:
            if (
                await self.eden.get_mount_state_async(mount_path, client)
                == MountState.RUNNING
            ):
                return True
            return None

        await poll_until_async(mount_running, timeout=60)
        self.eden.assert_running_fuse_transports()

    async def _wait_until_alive(self, client: EdenService.Async) -> None:
        async def is_alive() -> Optional[bool]:
            info = await client.getDaemonInfo()
            if info.status == fb303_status.ALIVE:
                return True
            return None

        await poll_until_async(is_alive, timeout=60)
        self.eden.assert_running_fuse_transports()

    def test_remount_creates_mount_point_dir(self) -> None:
        """Test that eden will automatically create the mount point directory if
        needed when it is setting up its mount points.
        """
        # Create a second checkout in a directory a couple levels deep so that
        # we can remove some of the parent directories of the checkout.
        checkout_path = Path(self.tmp_dir) / "checkouts" / "stuff" / "myproject"
        self.eden.clone(self.repo.path, str(checkout_path))
        self.assert_checkout_root_entries(self.expected_mount_entries, checkout_path)

        self.eden.run_cmd("unmount", str(checkout_path))
        self.assertEqual(
            {self.mount: "RUNNING", str(checkout_path): "NOT_RUNNING"},
            self.eden.list_cmd_simple(),
        )

        # Confirm that "eden mount" recreates the mount point directory
        shutil.rmtree(Path(self.tmp_dir) / "checkouts")
        self.eden.run_cmd("mount", str(checkout_path))
        self.assert_checkout_root_entries(self.expected_mount_entries, checkout_path)
        self.assertTrue(self.eden.in_proc_mounts(str(checkout_path)))
        self.assertEqual(
            {self.mount: "RUNNING", str(checkout_path): "RUNNING"},
            self.eden.list_cmd_simple(),
        )

        # Also confirm that Eden recreates the mount points on startup as well
        self.eden.shutdown()
        shutil.rmtree(Path(self.tmp_dir) / "checkouts")
        shutil.rmtree(self.mount_path)
        self.eden.start()
        self.assertEqual(
            {self.mount: "RUNNING", str(checkout_path): "RUNNING"},
            self.eden.list_cmd_simple(),
        )
        self.assert_checkout_root_entries(self.expected_mount_entries, checkout_path)

        # Check the behavior if Eden fails to create one of the mount point directories.
        # We just confirm that Eden still starts and mounts the other checkout normally
        # in this case.
        self.eden.shutdown()
        checkouts_dir = Path(self.tmp_dir) / "checkouts"
        shutil.rmtree(checkouts_dir)
        checkouts_dir.write_text("now a file\n")
        self.eden.start()
        self.assertEqual(
            {self.mount: "RUNNING", str(checkout_path): "NOT_RUNNING"},
            self.eden.list_cmd_simple(),
        )

    def _assert_start_reports_mount_failure(
        self, failed_mount: Path, expected_error: str
    ) -> None:
        startup_log = Path(self.tmp_dir) / "startup.log"
        self.eden.start(extra_args=["--startupLogPath", str(startup_log)])
        output = startup_log.read_text()
        print(output)

        self.assertTrue(self.eden.is_healthy(), output)
        self.assertEqual(
            MountState.RUNNING, self.eden.get_mount_state(self.mount_path), output
        )
        self.assert_checkout_root_entries(self.expected_mount_entries)
        self.assertEqual("hola\n", (self.mount_path / "hello").read_text())
        self.assertIsNone(self.eden.get_mount_state(failed_mount), output)
        with self.eden.get_thrift_client() as client:
            self.assertEqual(1, client.getCounter("startup_mount_failures"), output)

        self.assertEqual(1, output.count(f"Failed to remount {failed_mount}: "), output)
        self.assertIn(expected_error, output)
        self.assertIn(f"Successfully remounted {self.mount}", output)

    def test_start_reports_backing_store_construction_failure(self) -> None:
        failed_mount = Path(self.mounts_dir) / "missing_repo_checkout"
        self.eden.clone(self.repo.path, str(failed_mount))
        self.eden.shutdown()

        instance = EdenInstance(
            self.eden_dir, etc_eden_dir=self.etc_eden_dir, home_dir=self.home_dir
        )
        checkout = EdenCheckout(
            instance, failed_mount, self.eden.client_dir_for_mount(failed_mount)
        )
        missing_repo = Path(self.repos_dir) / "missing_repo"
        checkout.save_config(checkout.get_config()._replace(backing_repo=missing_repo))

        self._assert_start_reports_mount_failure(
            failed_mount, f"realpath({missing_repo.as_posix()}) failed"
        )

    def test_start_reports_checkout_config_failure(self) -> None:
        failed_mount = Path(self.mounts_dir) / "missing_config_checkout"
        self.eden.clone(self.repo.path, str(failed_mount))
        self.eden.shutdown()
        config_path = self.eden.client_dir_for_mount(failed_mount) / "config.toml"
        config_path.unlink()

        self._assert_start_reports_mount_failure(
            failed_mount, f"{config_path} could not be opened for parsing"
        )

    async def test_start_with_mount_failures(self) -> None:
        # Clone a few other checkouts
        mount2 = os.path.join(self.mounts_dir, "extra_mount_1")
        self.eden.clone(self.repo.path, mount2)
        mount3 = os.path.join(self.mounts_dir, "extra_mount_2")
        self.eden.clone(self.repo.path, mount3)
        self.assertEqual(
            {self.mount: "RUNNING", mount2: "RUNNING", mount3: "RUNNING"},
            self.eden.list_cmd_simple(),
        )

        # Now restart EdenFS with mounting blocked
        self.eden.shutdown()
        self.eden.spawn_nowait(
            extra_args=["--enable_fault_injection", "--fault_injection_block_mounts"]
        )

        # Wait for eden to have started mount point initialization
        await self._wait_until_initializing(num_mounts=3)

        async with self.eden.get_async_thrift_client() as client:
            # Since we blocked mount initialization the mount should still
            # report as INITIALIZING, and edenfs should report itself STARTING
            self.assertEqual(
                {
                    self.mount: "INITIALIZING",
                    mount2: "INITIALIZING",
                    mount3: "INITIALIZING",
                },
                self.eden.list_cmd_simple({"EDENFS_SKIP_DAEMON_READY_CHECK": "1"}),
            )
            daemon_info = await client.getDaemonInfo()
            self.assertEqual(fb303_status.STARTING, daemon_info.status)

            # Fail mounting of the additional 2 mounts we created
            await client.unblockFault(
                UnblockFaultArg(
                    keyClass="mount",
                    keyValueRegex=".*/extra_mount.*",
                    errorType="runtime_error",
                    errorMessage="PC LOAD LETTER",
                )
            )
            # Unblock mounting of the first mount
            await client.unblockFault(
                UnblockFaultArg(keyClass="mount", keyValueRegex=re.escape(self.mount))
            )
            # Wait until EdenFS reports itself as alive
            await self._wait_until_alive(client)

        self.assertEqual(
            {self.mount: "RUNNING", mount2: "NOT_RUNNING", mount3: "NOT_RUNNING"},
            self.eden.list_cmd_simple(),
        )
        # The startup_mount_failures counter should indicate that 2 mounts failed to
        # remount.
        async with self.eden.get_async_thrift_client() as client:
            mount_failures = await client.getCounter("startup_mount_failures")
            self.assertEqual(2, mount_failures)

    def test_start_with_hanging_mounts(self) -> None:
        """This test checks that hanging mounts are remounted upon restart.
        It does this by checking the location of a nonexistent file in the mount.
        If the mount is valid, a FileNotFoundError is raised. If the mount is
        hanging, an OSError is raised.

        Only for Linux since on MacOS a hanging mount will cause a system popup
        and only FUSE since NFS fails to remount before the hanging mount is detected.
        """
        if sys.platform != "linux":
            return

        if self.use_nfs():
            return

        mount_table = mtab.new()
        test_mounts = set()
        for mount_point, mount_type in check_stale_mounts.get_all_eden_mount_points(
            mount_table
        ):
            if mount_point.decode() == self.mount:
                test_mounts.add((mount_point, mount_type))

        # Check that mounts are valid
        for errored_mount_list in check_stale_mounts.get_stale_eden_mount_points(
            mount_table, test_mounts
        ):
            self.assertTrue(
                len(errored_mount_list) == 0, f"errored mounts: {errored_mount_list}"
            )

        # Now restart EdenFS with mounting blocked
        self.eden.kill_dirty()

        # Check that mounts are hanging
        stale_mounts = check_stale_mounts.get_stale_eden_mount_points(
            mount_table, test_mounts
        )
        self.assertEqual(len(stale_mounts[0]), 1)
        self.assertEqual(stale_mounts[0][0].decode(), self.mount)

        # Check that mounts are valid upon restart
        self.eden.start()
        for errored_mount_list in check_stale_mounts.get_stale_eden_mount_points(
            mount_table, test_mounts
        ):
            self.assertTrue(
                len(errored_mount_list) == 0, f"errored mounts: {errored_mount_list}"
            )

    @unittest.skipIf(sys.platform != "linux", "FUSE subtypes are Linux-only")
    @unittest.skipUnless(
        os.environ.get("EDENFS_PRIVHELPER_PATH"),
        "mounts are made by the installed privhelper unless "
        "EDENFS_PRIVHELPER_PATH points at the privhelper under test",
    )
    def test_fuse_mount_advertises_edenfs_subtype(self) -> None:
        if self.use_nfs():
            self.skipTest("only FUSE mounts carry a subtype")

        mount_entries = [
            mount
            for mount in mtab.new().read()
            if mount.mount_point.decode() == self.mount
        ]
        self.assertEqual(len(mount_entries), 1, f"mount entries: {mount_entries}")
        self.assertEqual(mount_entries[0].vfstype, b"fuse.edenfs")


@testcase.eden_repo_test(run_on_nfs=False, run_io_uring=False)
@unittest.skipIf(
    os.environ.get("EDEN_TEST_TSAN") == "1",
    "FUSE io_uring uses kernel shared rings that TSan reports as liburing races",
)
@unittest.skipIf(sys.platform != "linux", "FUSE connection abort is Linux-only")
class FuseIoUringMountTest(testcase.IoUringTestMixin, testcase.EdenRepoTest):
    def populate_repo(self) -> None:
        self.repo.write_file("hello", "hola\n")
        self.repo.commit("Initial commit.")

    async def test_aborted_io_uring_connection_stops_mount(self) -> None:
        async with self.eden.get_async_thrift_client() as client:
            mounts = await client.listMounts()
            mount = self._find_mount(mounts)
            self.assertIsNotNone(mount)
            self.assertEqual("io_uring", mount.fuseTransport)

        connection_id = os.lstat(self.mount).st_dev
        abort_path = Path("/sys/fs/fuse/connections") / str(connection_id) / "abort"
        if not abort_path.exists():
            self.skipTest(f"FUSE abort file does not exist: {abort_path}")
        if not os.access(abort_path, os.W_OK):
            self.skipTest(f"FUSE abort file is not writable: {abort_path}")

        self.addCleanup(self._cleanup_aborted_mount)
        abort_path.write_text("1\n")
        self._assert_mount_io_fails_with_enotconn()

        async with self.eden.get_async_thrift_client() as client:

            async def mount_removed() -> Optional[bool]:
                if self._find_mount(await client.listMounts()) is None:
                    return True
                return None

            await poll_until_async(mount_removed, timeout=60)

    def _assert_mount_io_fails_with_enotconn(self) -> None:
        with self.assertRaises(OSError) as context:
            os.listdir(self.mount)
        self.assertEqual(errno.ENOTCONN, context.exception.errno)

    def _find_mount(self, mounts):
        for mount in mounts:
            if Path(os.fsdecode(mount.mountPoint)) == self.mount_path:
                return mount
        return None

    def _cleanup_aborted_mount(self) -> None:
        with open(os.devnull, "wb") as devnull:
            subprocess.call(
                ["/bin/umount", "-lf", self.mount],
                stdout=devnull,
                stderr=devnull,
            )
            if self.eden.in_proc_mounts(self.mount):
                subprocess.call(
                    ["sudo", "-n", "/bin/umount", "-lf", self.mount],
                    stdout=devnull,
                    stderr=devnull,
                )
