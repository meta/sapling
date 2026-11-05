#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-unsafe

import os
import signal
import subprocess
import sys
import threading
import warnings
from pathlib import Path
from typing import Dict, List, Optional

import pexpect
from eden.fs.cli.util import EdenStartError, get_pid_using_lockfile, poll_until_async
from eden.fs.service.eden.thrift_clients import EdenService
from eden.fs.service.eden.thrift_types import (
    FaultDefinition,
    MountState,
    UnblockFaultArg,
)
from fb303_core.thrift_types import fb303_status

from .lib import testcase
from .lib.find_executables import FindExe

# Suppress Python 3.12 DeprecationWarning about forkpty() in multi-threaded processes.
# This is a Python stdlib internal warning that we cannot fix. It affects the takeover
# tests which use subprocess calls during graceful restarts.
warnings.filterwarnings(
    "ignore",
    category=DeprecationWarning,
    message=".*forkpty.*",
    module="pty",
)


class TakeoverTestCase(testcase.EdenRepoTest):
    def setUp(self) -> None:
        # TODO: Remove once in-flight request handoff is safe and the io_uring
        # takeover suites pass on both fbk 6.13 and 6.16.
        if self.use_io_uring():
            self.skipTest("io_uring takeover can strand in-flight requests")
        super().setUp()


class TakeoverTestBase(TakeoverTestCase):
    # pyre-fixme[13]: Attribute `pagesize` is never initialized.
    pagesize: int
    # pyre-fixme[13]: Attribute `page1` is never initialized.
    page1: str
    # pyre-fixme[13]: Attribute `page2` is never initialized.
    page2: str
    # pyre-fixme[13]: Attribute `commit1` is never initialized.
    commit1: str
    # pyre-fixme[13]: Attribute `commit2` is never initialized.
    commit2: str
    enable_fault_injection: bool = True

    def populate_repo(self) -> None:
        import resource

        self.pagesize = resource.getpagesize()
        self.page1 = "1" * self.pagesize
        self.page2 = "2" * self.pagesize
        self.repo.write_file("tree/hello", self.page1 + self.page2)
        self.repo.write_file("tree/deleted", self.page1 + self.page2)
        self.repo.write_file("src/main.c", "hello world")
        self.commit1 = self.repo.commit("Initial commit.")

        self.repo.write_file("src/main.c", "hello world v2")
        self.repo.write_file("src/test/test1.py", "test1")
        self.repo.write_file("src/test/test2.py", "test2")
        self.commit2 = self.repo.commit("Initial commit.")

    def edenfs_logging_settings(self) -> Dict[str, str]:
        if self._testMethodName == "test_takeover_with_io":
            # test_takeover_with_io causes lots of I/O, so do not enable
            # verbose logging of I/O operations in this test.
            return {}
        return {
            "eden.strace": "DBG7",
            "eden.fs.fuse": "DBG7",
            "eden.fs.inodes.InodeMap": "DBG6",
            "eden.fs.takeover": "DBG7",
            "eden.fs.service": "DBG4",
        }

    def do_takeover_test(self) -> None:
        hello = os.path.join(self.mount, "tree/hello")
        deleted = os.path.join(self.mount, "tree/deleted")
        deleted_local = os.path.join(self.mount, "deleted-local")

        # To test our handling of unlinked inodes, in addition
        # to unlinking something that is in the manifest we
        # need to check that we handle the case of a local
        # file being deleted to make sure that we cover both
        # code paths for FileInode.
        with open(deleted_local, "w") as dl:
            dl.write(self.page1)
            dl.write(self.page2)

        # We'd like to make sure that we do something reasonable
        # for directories that have been unlinked and that are
        # still referenced via a file descriptor.  Ideally we'd call
        # opendir() here and then readdir() it after we've performed
        # the graceful restart, but we can't directly call those
        # functions from python.  The approach used here is to
        # open a file descriptor to the directory and then try
        # to stat() it after the restart.  Since the directory
        # has to be empty in order to be unlinked, a readdir
        # from it wouldn't return any interesting results anyway.
        deleted_dir = os.path.join(self.mount, "deleted-dir")
        os.mkdir(deleted_dir)
        deleted_dir_fd = os.open(deleted_dir, 0)

        # The NFS client will cache the removal in the process it's run in.
        # So we need to delete from another process, so we can access the
        # deleted file descriptor later.
        subprocess.run(["rm", str(deleted_dir)])

        with (
            open(hello, "r") as f,
            open(deleted, "r") as d,
            open(deleted_local, "r") as dl,
        ):
            # Read the first page only (rather than the whole file)
            # before we restart the process.
            # This is so that we can check that the kernel really
            # does call in to us for the second page and that we're
            # really servicing the read for the second page and that
            # it isn't just getting served from the kernel buffer cache
            self.assertEqual(self.page1, f.read(self.pagesize))

            # Let's make sure that unlinked inodes continue to
            # work appropriately too.  We've opened the file
            # handles and are holding them alive in `d` and `dl`,
            # so now let's unlink it from the filesystem
            os.unlink(deleted)
            os.unlink(deleted_local)

            print("=== beginning restart ===", file=sys.stderr)
            self.eden.graceful_restart()
            print("=== restart complete ===", file=sys.stderr)

            # Ensure that our file handle is still live across
            # the restart boundary
            f.seek(0)
            self.assertEqual(self.page1, f.read(self.pagesize))
            self.assertEqual(self.page2, f.read(self.pagesize))

            # We should be able to read from the `d` file handle
            # even though we deleted the file from the tree
            self.assertEqual(self.page1, d.read(self.pagesize))
            self.assertEqual(self.page2, d.read(self.pagesize))
            # Likewise for the `dl` file handle
            self.assertEqual(self.page1, dl.read(self.pagesize))
            self.assertEqual(self.page2, dl.read(self.pagesize))

        # Now check that the unlinked directory handle still seems
        # connected.  This is difficult to do directly in python;
        # the directory had to be empty in order to be removed
        # so even if we could read its directory entries there
        # wouldn't be anything to read.
        # Note that os.stat() will throw if the fd is deemed
        # bad either by the kernel or the eden instance,
        # so we're just calling it and discarding the return
        # value.
        os.stat(deleted_dir_fd)
        os.close(deleted_dir_fd)

        # Let's also test opening the same file up again,
        # just to make sure that that is still working after
        # the graceful restart.
        with open(hello, "r") as f:
            self.assertEqual(self.page1, f.read(self.pagesize))
            self.assertEqual(self.page2, f.read(self.pagesize))


@testcase.eden_repo_test
class TakeoverTest(TakeoverTestBase):
    def test_takeover(self) -> None:
        return self.do_takeover_test()

    async def test_takeover_after_diff_revisions(self) -> None:
        # Make a getScmStatusBetweenRevisions() call to Eden.
        # Previously this thrift call caused Eden to create temporary inode
        # objects outside of the normal root inode tree, and this would cause
        # Eden to crash when shutting down afterwards.
        async with self.get_async_thrift_client() as client:
            await client.getScmStatusBetweenRevisions(
                os.fsencode(self.mount),
                self.commit1.encode("utf-8"),
                self.commit2.encode("utf-8"),
            )

        return self.do_takeover_test()

    def test_takeover_with_io(self) -> None:
        num_threads = 4
        write_chunk_size = 1024 * 1024
        max_file_length = write_chunk_size * 100

        # TODO: Setting this higher than 1 currently makes it likely that
        # edenfs will crash during restart.
        # There are still some other bugs we need to track down in the restart
        # ordering.
        num_restarts = 1

        stop = threading.Event()
        bufs = [b"x" * write_chunk_size, b"y" * write_chunk_size]

        def do_io(thread_id: int, running_event: threading.Event) -> None:
            path = os.path.join(self.mount, "src", "test", "data%d.log" % thread_id)
            with open(path, "wb") as f:
                # Use raw file descriptors to avoid going through python's I/O
                # buffering code.
                fd = f.fileno()

                buf_idx = 0
                buf = bufs[buf_idx]
                offset = 0

                # Repeatedly write and rewrite the same file,
                # alternating between two different data buffers.
                running_event.set()
                while True:
                    os.pwrite(fd, buf, offset)
                    if stop.is_set():
                        return
                    offset += len(buf)
                    if offset >= max_file_length:
                        buf_idx += 1
                        buf = bufs[buf_idx % len(bufs)]
                        offset = 0

        # Log the mount points device ID at the start of the test
        # (Just in case anything hangs and we need to abort the mount
        # using /sys/fs/fuse/connections/<dev>/)
        st = os.lstat(self.mount)
        print("=== eden mount device=%d ===" % st.st_dev, file=sys.stderr)

        # Start several threads doing I/O while we we perform a takeover
        threads = []
        try:
            running_events = []
            for n in range(num_threads):
                running = threading.Event()
                thread = threading.Thread(target=do_io, args=(n, running))
                thread.start()
                threads.append(thread)
                running_events.append(running)

            # Wait until all threads have started and are doing I/O
            for event in running_events:
                event.wait()

            # Restart edenfs
            for n in range(num_restarts):
                print("=== beginning restart %d ===" % n, file=sys.stderr)
                self.eden.graceful_restart()
                print("=== restart %d complete ===" % n, file=sys.stderr)
        finally:
            stop.set()
            for thread in threads:
                thread.join()

    def test_takeover_updates_process_id_in_lock_file(self) -> None:
        self.assertEqual(
            self.eden.get_pid_via_thrift(),
            get_pid_using_lockfile(Path(self.eden.eden_dir)),
        )
        self.eden.graceful_restart()
        self.assertEqual(
            self.eden.get_pid_via_thrift(),
            get_pid_using_lockfile(Path(self.eden.eden_dir)),
        )

    def test_takeover_preserves_inode_numbers_for_open_nonmaterialized_files(
        self,
    ) -> None:
        hello = os.path.join(self.mount, "tree/hello")

        fd = os.open(hello, os.O_RDONLY)
        try:
            inode_number = os.fstat(fd).st_ino

            self.eden.graceful_restart()

            self.assertEqual(inode_number, os.fstat(fd).st_ino)
        finally:
            os.close(fd)

        fd = os.open(hello, os.O_RDONLY)
        try:
            self.assertEqual(inode_number, os.fstat(fd).st_ino)
        finally:
            os.close(fd)

    def test_contents_are_the_same_if_handle_is_held_open(self) -> None:
        with (
            open(os.path.join(self.mount, "tree", "hello")) as c2_hello_file,
            open(os.path.join(self.mount, "src", "main.c")) as c2_mainc_file,
        ):
            self.eden.graceful_restart()
            self.eden.run_cmd(
                "debug", "flush_cache", os.path.join("tree", "hello"), cwd=self.mount
            )
            self.eden.run_cmd(
                "debug", "flush_cache", os.path.join("src", "main.c"), cwd=self.mount
            )

            self.assertEqual(self.page1 + self.page2, c2_hello_file.read())
            self.assertEqual("hello world v2", c2_mainc_file.read())

    def test_readdir_after_graceful_restart(self) -> None:
        # Ensure capability flags (e.g. FUSE_NO_OPENDIR_SUPPORT) survive
        # graceful restart
        self.eden.graceful_restart()
        self.assertEqual(
            ["test1.py", "test2.py"],
            sorted(os.listdir(os.path.join(self.mount, "src", "test"))),
        )

    def test_readdir_before_and_after_graceful_restart(self) -> None:
        self.assertEqual(
            ["test1.py", "test2.py"],
            sorted(os.listdir(os.path.join(self.mount, "src", "test"))),
        )
        self.eden.graceful_restart()
        self.assertEqual(
            ["test1.py", "test2.py"],
            sorted(os.listdir(os.path.join(self.mount, "src", "test"))),
        )

    def test_takeover_failure(self) -> None:
        print("=== beginning restart ===", file=sys.stderr)
        self.eden.takeover_without_ping_response()
        print("=== restart complete ===", file=sys.stderr)
        self.assertTrue(self.eden.wait_for_is_healthy())

    def run_restart(self) -> "pexpect.spawn[bytes]":
        edenfsctl, env = FindExe.get_edenfsctl_env()
        restart_cmd = [
            edenfsctl,
            "--config-dir",
            str(self.eden_dir),
            "--etc-eden-dir",
            str(self.etc_eden_dir),
            "--home-dir",
            str(self.home_dir),
            "restart",
            "--daemon-binary",
            FindExe.FAKE_EDENFS,
        ]

        print("Restarting eden: %r" % (restart_cmd,))
        return pexpect.spawn(
            restart_cmd[0],
            restart_cmd[1:],
            logfile=sys.stdout.buffer,
            timeout=120,
            env=env,
        )

    async def assert_restart_fails_with_in_progress_graceful_restart(
        self, client: EdenService.Async
    ) -> None:
        pid = self.eden.get_pid_via_thrift()
        p = self.run_restart()
        p.expect_exact(
            f"The current edenfs daemon (pid {pid}) is in the middle of stopping."
            f"\r\nUse `eden restart --force` if you want to forcibly restart the current daemon\r\n"
        )
        p.wait()
        self.assertEqual(p.exitstatus, 4)

        daemon_info = await client.getDaemonInfo()
        self.assertEqual(daemon_info.status, fb303_status.STOPPING)

    async def assert_shutdown_fails_with_in_progress_graceful_restart(
        self, client: EdenService.Async
    ) -> None:
        # call initiateShutdown. This should not throw.
        try:
            await client.initiateShutdown("shutdown requested during graceful restart")
        except Exception:
            self.fail(
                "initiateShutdown should not throw when graceful restart is in progress"
            )

        daemon_info = await client.getDaemonInfo()
        self.assertEqual(daemon_info.status, fb303_status.STOPPING)

    async def assert_sigkill_fails_with_in_progress_graceful_restart(
        self, client: EdenService.Async
    ) -> None:
        # send SIGTERM to process. This should not throw.
        pid = self.eden.get_pid_via_thrift()
        try:
            os.kill(pid, signal.SIGTERM)
        except Exception:
            self.fail(
                "sending SIGTERM should not throw when graceful restart is in progress"
            )

        daemon_info = await client.getDaemonInfo()
        self.assertEqual(daemon_info.status, fb303_status.STOPPING)

    async def test_stop_during_takeover(self) -> None:
        # block graceful restart
        async with self.get_async_thrift_client() as client:
            await client.injectFault(
                FaultDefinition(
                    keyClass="takeover", keyValueRegex="server_shutdown", block=True
                )
            )

            self.eden.wait_for_is_healthy()

            # Run a graceful restart
            # This won't succeed until we unblock the shutdown.
            old_process = self.eden.graceful_restart(
                should_wait_for_old=False, should_wait_for_new=False
            )

            # Wait for the state to be shutting down
            async def state_shutting_down() -> Optional[bool]:
                daemon_info = await client.getDaemonInfo()
                if daemon_info.status is fb303_status.STOPPING:
                    return True
                return None

            await poll_until_async(state_shutting_down, timeout=60)

            # Normal restart should be rejected while a graceful restart
            # is in progress
            await self.assert_restart_fails_with_in_progress_graceful_restart(client)

            # Normal shutdown should be rejected while a graceful restart
            # is in progress
            await self.assert_shutdown_fails_with_in_progress_graceful_restart(client)

            # Getting SIGTERM should not kill process while a graceful restart is in
            # progress
            await self.assert_sigkill_fails_with_in_progress_graceful_restart(client)

            # Unblock the server shutdown and wait for the graceful restart to complete.
            await client.unblockFault(
                UnblockFaultArg(keyClass="takeover", keyValueRegex="server_shutdown")
            )

            self.eden.wait_for_is_healthy()

            return_code = old_process.wait()
            if return_code != 0:
                raise Exception(
                    "eden exited unsuccessfully with status {}".format(return_code)
                )

    async def test_takeover_during_mount(self) -> None:
        self.eden.unmount(self.mount_path)

        async with self.get_async_thrift_client() as client:
            await client.injectFault(
                FaultDefinition(keyClass="mount", keyValueRegex=".*", block=True)
            )

        try:
            mount_command, env = self.eden.get_edenfsctl_cmd_env(
                "mount", "--", str(self.mount_path)
            )
            mountProcess = subprocess.Popen(
                mount_command,
                env=env,
            )

            async def mount_initializing() -> Optional[bool]:
                async with self.get_async_thrift_client() as client:
                    mount_list = await client.listMounts()
                    for mount_info in mount_list:
                        if mount_info.mountPoint == self.mount_path_bytes:
                            if mount_info.state == MountState.INITIALIZING:
                                return True
                            return False
                return None

            await poll_until_async(mount_initializing, timeout=60)

            with self.assertRaisesRegex(
                EdenStartError, "edenfs exited before becoming healthy"
            ):
                self.eden.graceful_restart()
        finally:
            async with self.get_async_thrift_client() as client:
                await client.unblockFault(
                    UnblockFaultArg(keyClass="mount", keyValueRegex=".*")
                )
            mountProcess.wait()

        self.eden.assert_running_fuse_transports()

    def test_takeover_with_checkout_missing_snapshot(self) -> None:
        broken_mount = Path(self.mounts_dir) / "missing-snapshot"
        self.eden.clone(self.repo.path, broken_mount)
        client_dir = self.eden.client_dir_for_mount(broken_mount)
        self.eden.shutdown()
        (client_dir / "SNAPSHOT").unlink()
        self.eden.start()

        self.assertEqual(
            {self.mount: "RUNNING", str(broken_mount): "NOT_RUNNING"},
            self.eden.list_cmd_simple(),
        )
        self.eden.graceful_restart()


@testcase.eden_repo_test(run_on_nfs=False)
class TakeoverTestNoNFSServer(TakeoverTestBase):
    def edenfs_extra_config(self) -> Optional[Dict[str, List[str]]]:
        return {}

    def test_takeover(self) -> None:
        return self.do_takeover_test()


@testcase.eden_repo_test(run_coroutines=False, run_io_uring=False)
class TakeoverNfsServerDisabledTest(TakeoverTestCase):
    """
    The old daemon hands an NFS-mounted checkout over regardless of the new
    daemon's `experimental:enable-nfs-server`, so a config change between the
    two daemons must not take the daemon down.
    """

    git_test_supported = False
    nfs_server_enabled: bool = True

    def setUp(self) -> None:
        if not self.use_nfs():
            self.skipTest("needs an NFS-mounted checkout")
        super().setUp()

    def populate_repo(self) -> None:
        self.repo.write_file("hello", "hola\n")
        self.repo.commit("Initial commit.")

    def edenfs_extra_config(self) -> Optional[Dict[str, List[str]]]:
        configs = super().edenfs_extra_config() or {}
        configs["experimental"] = [
            setting
            for setting in configs["experimental"]
            if not setting.startswith("enable-nfs-server")
        ] + [f"enable-nfs-server = {'true' if self.nfs_server_enabled else 'false'}"]
        return configs

    def _restart_with_nfs_server(self, enabled: bool) -> None:
        self.nfs_server_enabled = enabled
        self.write_edenfs_system_config()
        self.eden.graceful_restart()

    def _assert_nfs_mount_served(self) -> None:
        self.assertEqual("hola\n", self.read_file("hello"))
        with self.get_thrift_client() as client:
            mounts = [
                m for m in client.listMounts() if m.mountPoint == self.mount_path_bytes
            ]
        self.assertEqual(1, len(mounts))
        self.assertEqual(MountState.RUNNING, mounts[0].state)
        self.assertIsNotNone(mounts[0].nfsTransport)

    def test_takeover_keeps_nfs_mount_with_nfs_server_enabled(self) -> None:
        self._restart_with_nfs_server(enabled=True)
        self._assert_nfs_mount_served()

    def test_takeover_keeps_nfs_mount_with_nfs_server_disabled(self) -> None:
        self._restart_with_nfs_server(enabled=False)
        self._assert_nfs_mount_served()


@testcase.eden_repo_test
class TakeoverRocksDBStressTest(TakeoverTestCase):
    enable_fault_injection: bool = True

    def populate_repo(self) -> None:
        self.repo.write_file("test-directory/file", "")
        # pyre-fixme[16]: `TakeoverRocksDBStressTest` has no attribute `commit1`.
        self.commit1 = self.repo.commit("Initial commit.")

    async def test_takeover_with_tree_inode_loading_from_local_store(self) -> None:
        """
        Restart edenfs while a tree inode is being loaded asynchronously. Ensure
        restarting does not deadlock.
        """

        def load_test_directory_inode_from_local_store_asynchronously() -> None:
            """
            Make edenfs start loading "/test-directory" from the local store.

            To ensure that the local store is in use during takeover, load the tree
            inode using a prefetch.

            At the time of writing, os.listdir("foo") causes edenfs to prefetch
            the tree inodes of foo/*. Exploit this to load the tree inode for
            "/directory".

            Other options considered:

            * At the time of writing, if we load the tree inode using a FUSE
              request (e.g. os.stat), edenfs would wait for the FUSE request to
              finish before starting the inode shutdown procedure.

            * At the time of writing, 'edenfsctl prefetch' does not prefetch
              tree inodes asynchronously.
            """
            os.listdir(self.mount)

        graceful_restart_startup_time = 5.0

        async with self.get_async_thrift_client() as client:
            for key_class in ["local store get single", "local store get batch"]:
                await client.injectFault(
                    FaultDefinition(
                        keyClass=key_class,
                        keyValueRegex=".*",
                        delayMilliseconds=int(graceful_restart_startup_time * 1000),
                        count=100,
                    )
                )

        load_test_directory_inode_from_local_store_asynchronously()
        self.eden.graceful_restart()
