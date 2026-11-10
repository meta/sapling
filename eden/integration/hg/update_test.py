#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-unsafe

import asyncio
import logging
import os
import pathlib
import re
import signal
import sys
import threading
from contextlib import asynccontextmanager
from enum import Enum
from textwrap import dedent
from threading import Thread
from typing import AsyncGenerator, Awaitable, Dict, List, Optional, Set

from eden.fs.cli import util
from eden.fs.service.eden.thrift_types import (
    CheckoutMode,
    CheckOutRevisionParams,
    DIS_ENABLE_FLAGS,
    EdenError,
    EdenErrorType,
    FaultDefinition,
    GetScmStatusParams,
    MountState,
    SyncBehavior,
    UnblockFaultArg,
)
from eden.integration.hg.lib.hg_extension_test_base import EdenHgTestCase, hg_test
from eden.integration.lib import hgrepo

if sys.platform == "win32":
    from eden.fs.cli import prjfs
    from eden.integration.lib.util import open_locked


@hg_test
# pyre-ignore[13]: T62487924
class UpdateTest(EdenHgTestCase):
    # pyre-fixme[13]: Attribute `commit1` is never initialized.
    commit1: str
    # pyre-fixme[13]: Attribute `commit2` is never initialized.
    commit2: str
    # pyre-fixme[13]: Attribute `commit3` is never initialized.
    commit3: str
    enable_fault_injection: bool = True

    def edenfs_logging_settings(self) -> Dict[str, str]:
        return {
            "eden.fs.inodes.TreeInode": "DBG5",
            "eden.fs.inodes.CheckoutAction": "DBG5",
            "eden.fs.inodes.CheckoutContext": "DBG5",
        }

    def populate_backing_repo(self, repo: hgrepo.HgRepository) -> None:
        repo.write_file("hello.txt", "hola")
        repo.write_file(".gitignore", "ignoreme\n")
        repo.write_file("foo/.gitignore", "*.log\n")
        repo.write_file("foo/bar.txt", "test\n")
        repo.write_file("foo/subdir/test.txt", "test\n")
        self.commit1 = repo.commit("Initial commit.")

        repo.write_file("foo/.gitignore", "*.log\n/_*\n")
        self.commit2 = repo.commit("Update foo/.gitignore")

        repo.write_file("foo/bar.txt", "updated in commit 3\n")
        self.commit3 = repo.commit("Update foo/.gitignore")

    def test_mode_change_with_no_content_change(self) -> None:
        """Test changing the mode of a file but NOT the contents."""
        self.assert_status_empty()

        self.chmod("hello.txt", 0o755)
        self.assert_status({"hello.txt": "M"})
        commit4 = self.repo.commit("Update hello.txt mode")

        self.repo.update(self.commit1)
        self.repo.update(commit4)
        self.touch("hello.txt")
        self.repo.update(self.commit1)
        self.repo.update(commit4)
        self.assert_status_empty()

    def test_update_clean_reverts_modified_files(self) -> None:
        """Test using `hg update --clean .` to revert file modifications."""
        self.assert_status_empty()

        self.write_file("hello.txt", "saluton")
        self.assert_status({"hello.txt": "M"})

        self.repo.update(".", clean=True)
        self.assertEqual("hola", self.read_file("hello.txt"))
        self.assert_status_empty()

    def test_update_clean_removes_added_and_removed_statuses(self) -> None:
        """Test using `hg update --clean .` in the presence of added and removed
        files."""
        self.write_file("bar/some_new_file.txt", "new file\n")
        self.hg("add", "bar/some_new_file.txt")
        self.hg("remove", "foo/bar.txt")
        self.assertFalse(os.path.isfile(self.get_path("foo/bar.txt")))
        self.assert_status({"foo/bar.txt": "R", "bar/some_new_file.txt": "A"})

        self.repo.update(".", clean=True)
        self.assert_status({"bar/some_new_file.txt": "?"})
        self.assertTrue(os.path.isfile(self.get_path("foo/bar.txt")))
        self.assert_dirstate_empty()

    def test_update_with_gitignores(self) -> None:
        """
        Test `hg update` with gitignore files.

        This exercises the normal checkout and ignore logic, but also exercises
        some additional interesting cases:  The `hg status` calls cause eden to
        create FileInode objects for the .gitignore files, even though they
        have never been requested via FUSE APIs.  When we update them via
        checkout, this triggers FUSE inode invalidation events.  We want to
        make sure the invalidation doesn't cause any errors even though the
        kernel didn't previously know that these inode objects existed.
        """
        # Call `hg status`, which causes eden to internally create FileInode
        # objects for the .gitignore files.
        self.assert_status_empty()

        self.write_file("foo/subdir/test.log", "log data")
        self.write_file("foo/_data", "data file")
        self.assert_status_empty(
            check_ignored=False, msg="test.log and _data should be ignored"
        )
        self.assert_status({"foo/subdir/test.log": "I", "foo/_data": "I"})

        # Call `hg update` to move from commit2 to commit1, which will
        # change the contents of foo/.gitignore.  This will cause edenfs
        # to send an inode invalidation event to FUSE, but FUSE never knew
        # about this inode in the first place.  edenfs should ignore the
        # resulting ENOENT error in response to the invalidation request.
        self.repo.update(self.commit1)
        self.assert_status({"foo/_data": "?"}, check_ignored=False)
        self.assert_status({"foo/subdir/test.log": "I", "foo/_data": "?"})
        self.assertEqual("*.log\n", self.read_file("foo/.gitignore"))
        self.assertEqual("test\n", self.read_file("foo/bar.txt"))

    def test_update_with_new_commits(self) -> None:
        """
        Test running `hg update` to check out commits that were created after
        the edenfs daemon originally started.

        This makes sure edenfs can correctly import new commits that appear in
        the backing store repository.
        """
        new_contents = "New contents for bar.txt\n"
        self.backing_repo.write_file("foo/bar.txt", new_contents)
        new_commit = self.backing_repo.commit("Update foo/bar.txt")

        self.assert_status_empty()
        self.assertNotEqual(new_contents, self.read_file("foo/bar.txt"))

        self.repo.update(new_commit)
        self.assertEqual(new_contents, self.read_file("foo/bar.txt"))
        self.assert_status_empty()

    def test_reset(self) -> None:
        """
        Test `hg reset`
        """
        self.assert_status_empty()
        self.assertEqual("updated in commit 3\n", self.read_file("foo/bar.txt"))

        self.repo.reset(self.commit2, keep=True)
        self.assert_status({"foo/bar.txt": "M"})
        self.assertEqual("updated in commit 3\n", self.read_file("foo/bar.txt"))

        self.repo.update(self.commit2, clean=True)
        self.assert_status_empty()
        self.assertEqual("test\n", self.read_file("foo/bar.txt"))

    def test_update_replace_untracked_dir(self) -> None:
        """
        Create a local untracked directory, then run "hg update -C" to
        checkout a commit where this directory exists in source control.
        """
        self.assert_status_empty()
        # Write some new files in the eden working directory
        self.mkdir("new_project")
        self.write_file("new_project/newcode.c", "test\n")
        self.write_file("new_project/Makefile", "all:\n\techo done!\n")
        self.write_file("new_project/.gitignore", "*.o\n")
        self.write_file("new_project/newcode.o", "\x00\x01\x02\x03\x04")

        # Add the same files to a commit in the backing repository
        self.backing_repo.write_file("new_project/newcode.c", "test\n")
        self.backing_repo.write_file("new_project/Makefile", "all:\n\techo done!\n")
        self.backing_repo.write_file("new_project/.gitignore", "*.o\n")
        new_commit = self.backing_repo.commit("Add new_project")

        # Check the status before we update
        self.assert_status(
            {
                "new_project/newcode.o": "I",
                "new_project/newcode.c": "?",
                "new_project/Makefile": "?",
                "new_project/.gitignore": "?",
            }
        )

        # Now run "hg update -C new_commit"
        self.repo.update(new_commit, clean=True)
        self.assert_status({"new_project/newcode.o": "I"})

    def test_update_with_merge_flag_and_conflict(self) -> None:
        self.write_file("foo/bar.txt", "changing yet again\n")
        with self.assertRaises(hgrepo.HgError) as context:
            self.hg("update", ".^", "--merge")
        self.assertIn(
            b"1 conflicts while merging foo/bar.txt!",
            context.exception.stderr,
        )
        self.assert_status({"foo/bar.txt": "M"}, op="updatemerge")
        self.assert_file_regex(
            "foo/bar.txt",
            """\
            <<<<<<< .*
            changing yet again
            =======
            test
            >>>>>>> .*
            """,
        )

    def test_merge_update_added_file_with_same_contents_in_destination(self) -> None:
        base_commit = self.repo.get_head_hash()

        file_contents = "new file\n"
        self.write_file("bar/some_new_file.txt", file_contents)
        self.hg("add", "bar/some_new_file.txt")
        self.write_file("foo/bar.txt", "Modify existing file.\n")
        new_commit = self.repo.commit("add some_new_file.txt")
        self.assert_status_empty()

        self.repo.update(base_commit)
        self.assert_status_empty()
        self.wait_for_path_absent("bar")
        self.write_file("bar/some_new_file.txt", file_contents)
        self.hg("add", "bar/some_new_file.txt")
        self.assert_status({"bar/some_new_file.txt": "A"})

        # Note the update fails even though some_new_file.txt is the same in
        # both the working copy and the destination.
        with self.assertRaises(hgrepo.HgError) as context:
            self.repo.update(new_commit)
        self.assertIn(
            b"abort: 1 conflicting file changes:\n bar/some_new_file.txt\n",
            context.exception.stderr,
        )
        self.assertEqual(
            base_commit,
            self.repo.get_head_hash(),
            msg="We should still be on the base commit because the merge was aborted.",
        )
        self.assert_dirstate({"bar/some_new_file.txt": ("a", 0, "MERGE_BOTH")})
        self.assert_status({"bar/some_new_file.txt": "A"})
        self.assertEqual(file_contents, self.read_file("bar/some_new_file.txt"))

        # Now do the update with --merge specified.
        self.repo.update(new_commit, merge=True)
        self.assert_status_empty()
        self.assertEqual(
            new_commit,
            self.repo.get_head_hash(),
            msg="Should be expected commit hash because nothing has changed.",
        )

    def test_merge_update_untracked_file_with_same_contents_in_destination(
        self,
    ) -> None:
        base_commit = self.repo.get_head_hash()

        file_contents = "new file\n"
        self.write_file("bar/some_new_file.txt", file_contents)
        self.hg("add", "bar/some_new_file.txt")
        new_commit = self.repo.commit("add some_new_file.txt")
        self.assert_status_empty()

        self.repo.update(base_commit)
        self.assert_status_empty()
        self.wait_for_path_absent("bar")
        self.write_file("bar/some_new_file.txt", file_contents)

        # the update succeeds because some_new_file has the same contents
        self.repo.update(new_commit)
        self.assert_status_empty()
        self.assertEqual(
            new_commit,
            self.repo.get_head_hash(),
            msg="Should be expected commit hash because nothing has changed.",
        )

        self.repo.update(base_commit)
        self.wait_for_path_absent("bar")
        new_file_contents = "some OTHER contents\n"
        self.write_file("bar/some_new_file.txt", new_file_contents)
        self.assert_status({"bar/some_new_file.txt": "?"})

        # now the update aborts because some_new_file has the different contents
        with self.assertRaises(hgrepo.HgError) as context:
            self.repo.update(new_commit)
        self.assertIn(
            b"1 conflicting file changes:\n bar/some_new_file.txt",
            context.exception.stderr,
        )
        self.assertEqual(
            base_commit,
            self.repo.get_head_hash(),
            msg="We should still be on the base commit because the merge was aborted.",
        )
        self.assert_dirstate({})
        self.assert_status({"bar/some_new_file.txt": "?"})
        self.assertEqual(new_file_contents, self.read_file("bar/some_new_file.txt"))

    def test_merge_update_ignored_file_tracked_in_destination(
        self,
    ) -> None:
        self.write_file(".gitignore", "ignoredfiles/\n")

        file_contents = "hello\n"
        self.write_file("ignoredfiles/bad.txt", file_contents)
        self.hg("add", "ignoredfiles/bad.txt")
        added_commit = self.repo.commit("add ignored bad file")
        self.assert_status_empty()

        self.rm("ignoredfiles/bad.txt")
        self.hg("forget", "ignoredfiles/bad.txt")
        self.repo.commit("remove ignored bad file")
        self.assert_status_empty()

        self.write_file("ignoredfiles/bad.txt", "something else\n")
        self.assert_status({"ignoredfiles/bad.txt": "I"})

        # Go back before the file was removed, it should succeed
        self.repo.update(added_commit)
        # The file is tracked, and its content was replaced with the version in this commit.
        self.assert_status_empty()
        self.assertEqual(file_contents, self.read_file("ignoredfiles/bad.txt"))

    def test_merge_update_added_file_with_conflict_in_destination(self) -> None:
        self._test_merge_update_file_with_conflict_in_destination(True)

    def test_merge_update_untracked_file_with_conflict_in_destination(self) -> None:
        self._test_merge_update_file_with_conflict_in_destination(False)

    def _test_merge_update_file_with_conflict_in_destination(
        self, add_before_updating: bool
    ) -> None:
        base_commit = self.repo.get_head_hash()
        original_contents = "Original contents.\n"
        self.write_file("some_new_file.txt", original_contents)
        self.hg("add", "some_new_file.txt")
        self.write_file("foo/bar.txt", "Modify existing file.\n")
        commit = self.repo.commit("Commit a new file.")
        self.assert_status_empty()

        # Do an `hg prev` and re-create the new file with different contents.
        self.repo.update(base_commit)
        self.assert_status_empty()
        self.assertFalse(os.path.exists(self.get_path("some_new_file.txt")))
        modified_contents = "Re-create the file with different contents.\n"
        self.write_file("some_new_file.txt", modified_contents)

        if add_before_updating:
            self.hg("add", "some_new_file.txt")
            self.assert_status({"some_new_file.txt": "A"})
        else:
            self.assert_status({"some_new_file.txt": "?"})

        # Verify `hg next` updates such that the original contents and commit
        # hash are restored. No conflicts should be reported.
        path_to_backup = ".hg/origbackups/some_new_file.txt"
        expected_backup_file = os.path.join(self.mount, path_to_backup)
        self.assertFalse(os.path.isfile(expected_backup_file))
        with self.assertRaises(hgrepo.HgError) as context:
            self.repo.update(commit, merge=True)
        self.assertIn(
            b"warning: 1 conflicts while merging some_new_file.txt!",
            context.exception.stderr,
        )
        self.assertEqual(
            commit,
            self.repo.get_head_hash(),
            msg="Even though we have a merge conflict, "
            "we should still be at the new commit.",
        )
        self.assert_dirstate_empty()
        self.assert_status({"some_new_file.txt": "M"}, op="updatemerge")
        merge_contents = dedent(
            """\
        <<<<<<< .*
        Re-create the file with different contents.
        =======
        Original contents.
        >>>>>>> .*
        """
        )
        self.assertRegex(self.read_file("some_new_file.txt"), merge_contents)
        self.assert_unresolved(["some_new_file.txt"])

        # Verify the previous version of the file was backed up as expected.
        self.assertTrue(os.path.isfile(expected_backup_file))
        self.assertEqual(modified_contents, self.read_file(path_to_backup))

        # Resolve the merge conflict and complete the update
        resolved_contents = "Merged contents.\n"
        self.write_file("some_new_file.txt", resolved_contents)
        self.hg("resolve", "--mark", "some_new_file.txt")
        self.assert_dirstate_empty()
        self.assert_status({"some_new_file.txt": "M"}, op="updatemerge")
        self.repo.commit("Resolved file changes.")
        self.assert_dirstate_empty()
        self.hg("update", "--continue")
        self.assert_status_empty()
        self.assertEqual(resolved_contents, self.read_file("some_new_file.txt"))

    def test_update_modified_file_to_removed_file_taking_other(self) -> None:
        self.write_file("some_new_file.txt", "I am new!\n")
        self.hg("add", "some_new_file.txt")
        self.repo.commit("Commit a new file.")
        self.write_file("some_new_file.txt", "Make some changes to that new file.\n")

        self.hg("update", ".^", "--merge", "--tool", ":other")
        self.assertFalse(os.path.exists(self.get_path("some_new_file.txt")))
        self.assertFalse(
            os.path.isfile(
                os.path.join(self.mount, ".hg/origbackups/some_new_file.txt")
            ),
            msg="There should not be a backup file because "
            ":other was specified explicitly.",
        )

    def test_update_modified_file_to_removed_file_taking_local(self) -> None:
        self.write_file("some_new_file.txt", "I am new!\n")
        self.hg("add", "some_new_file.txt")
        self.repo.commit("Commit a new file.")
        new_contents = "Make some changes to that new file.\n"
        self.write_file("some_new_file.txt", new_contents)

        self.hg("update", ".^", "--merge", "--tool", ":local")
        self.assertEqual(new_contents, self.read_file("some_new_file.txt"))
        self.assert_status({"some_new_file.txt": "A"})

    def test_update_untracked_added_conflict(self) -> None:
        # Create a commit with a newly-created file foo/new_file.txt
        self.write_file("foo/new_file.txt", "new file\n")
        self.hg("add", "foo/new_file.txt")
        new_commit = self.repo.commit("Add foo/new_file.txt")

        # Switch back to commit 3
        self.hg("update", self.commit3)

        # Write foo/new_file.txt as an untracked file
        self.write_file("foo/new_file.txt", "different contents\n")

        # Try to switch back to the new commit
        result = self.repo.run_hg(
            "update",
            new_commit,
            check=False,
            traceback=False,
        )
        self.maxDiff = None
        # TODO: Make this an assertEquals() once "goto" renaming in docs is
        # rolled out everywhere.
        self.assertRegex(
            result.stderr.decode("utf-8"),
            re.compile(
                "abort: 1 conflicting file changes:\n foo/new_file.txt",
                re.MULTILINE,
            ),
        )
        self.assertNotEqual(0, result.returncode)

        self.assert_status({"foo/new_file.txt": "?"})

    def test_update_ignores_untracked_directory(self) -> None:
        base_commit = self.repo.get_head_hash()
        self.mkdir("foo/bar")
        self.write_file("foo/bar/a.txt", "File in directory two levels deep.\n")
        self.write_file("foo/bar/b.txt", "Another file.\n")
        self.hg("add", "foo/bar/a.txt")
        self.assert_status({"foo/bar/a.txt": "A", "foo/bar/b.txt": "?"})
        self.repo.commit("Commit only a.txt.")
        self.assert_status({"foo/bar/b.txt": "?"})
        self.repo.update(base_commit)
        self.assert_status({"foo/bar/b.txt": "?"})
        self.assertFalse(os.path.exists(self.get_path("foo/bar/a.txt")))
        self.assertTrue(os.path.exists(self.get_path("foo/bar/b.txt")))

    async def wait_for_checkout_in_progress(self) -> None:
        hg_parent = self.hg("log", "-r.", "-T{node}")

        async def checkout_in_progress() -> Optional[bool]:
            try:
                async with self.eden.get_async_thrift_client() as client:
                    await client.getScmStatusV2(
                        GetScmStatusParams(
                            mountPoint=bytes(self.mount, encoding="utf-8"),
                            commit=bytes(hg_parent, encoding="utf-8"),
                            listIgnored=False,
                            rootIdOptions=None,
                        )
                    )
            except EdenError as ex:
                if ex.errorType == EdenErrorType.CHECKOUT_IN_PROGRESS:
                    if "checkout is currently in progress" in ex.message:
                        return True
                    else:
                        return None
                else:
                    raise ex
            return None

        await util.poll_until_async(checkout_in_progress, timeout=30)

    @asynccontextmanager
    async def block_checkout(self) -> AsyncGenerator[None, None]:
        async with self.eden.get_async_thrift_client() as client:
            await client.injectFault(
                FaultDefinition(
                    keyClass="inodeCheckout", keyValueRegex=".*", block=True
                )
            )

        try:
            yield
        finally:
            async with self.eden.get_async_thrift_client() as client:
                await client.unblockFault(
                    UnblockFaultArg(keyClass="inodeCheckout", keyValueRegex=".*")
                )

    async def get_async_error(
        self, awaitable: Optional[Awaitable[object]]
    ) -> Optional[Exception]:
        if awaitable is None:
            return None
        try:
            await awaitable
        except Exception as ex:
            return ex
        return None

    async def wait_for_mount_shutting_down(self) -> None:
        async def state_shutting_down() -> Optional[bool]:
            state = await self.eden.get_mount_state_async(pathlib.Path(self.mount))
            if state == MountState.SHUTTING_DOWN:
                return True
            if state in (None, MountState.SHUT_DOWN, MountState.DESTROYING):
                self.fail(
                    "mount should not list status as not mounted while "
                    "checkout is in progress"
                )
            return None

        await util.poll_until_async(state_shutting_down, timeout=30)

    def wait_for_path_absent(self, path: str) -> None:
        def path_absent() -> Optional[bool]:
            if not os.path.exists(self.get_path(path)):
                return True
            return None

        util.poll_until(path_absent, timeout=30)

    def check_unmount_checkout_errors(
        self,
        update_error: Optional[Exception],
        unmount_error: Optional[Exception],
    ) -> None:
        if unmount_error is not None:
            raise unmount_error

        if update_error is None:
            return

        if not isinstance(update_error, hgrepo.HgError):
            raise update_error

        stderr = update_error.stderr or b""
        if isinstance(stderr, str):
            stderr = stderr.encode()
        if b"No such file or directory" not in stderr or b"/.hg/" not in stderr:
            raise update_error

    async def test_mount_state_during_unmount_with_in_progress_checkout(self) -> None:
        mounts = self.eden.run_cmd("list")
        self.assertEqual(f"{self.mount}\n", mounts)

        self.backing_repo.write_file("foo/bar.txt", "new contents")
        new_commit = self.backing_repo.commit("Update foo/bar.txt")

        update_task: Optional[asyncio.Task[str]] = None
        unmount_task: Optional[asyncio.Task[None]] = None
        update_error: Optional[Exception] = None
        unmount_error: Optional[Exception] = None
        try:
            async with self.block_checkout():
                update_task = asyncio.create_task(
                    self.repo.update_async(new_commit), name="hg-update"
                )

                await self.wait_for_checkout_in_progress()

                unmount_task = asyncio.create_task(
                    self.eden.unmount_async(pathlib.Path(self.mount)),
                    name="eden-unmount",
                )

                await self.wait_for_mount_shutting_down()
        finally:
            update_error = await self.get_async_error(update_task)
            unmount_error = await self.get_async_error(unmount_task)

        self.check_unmount_checkout_errors(update_error, unmount_error)

    def test_dir_locking(self) -> None:
        """
        Test performing checkouts that modify the directory foo/ while other
        clients are simultaneously renaming untracked files under foo/

        This exercises the interaction of the kernel's inode locks and Eden's
        own user-space locking.  We previously had some situations where
        deadlock could occur because FUSE requests holding kernel inode lock
        were blocked on userspace locks that were held by other threads blocked
        on the kernel inode lock.
        """
        num_checkout_changed_files = 500
        num_rename_threads = 4
        num_checkouts = 4

        # Create a new commit in the backing repository with many new files in
        # the foo/ directory
        for n in range(num_checkout_changed_files):
            path = os.path.join(self.backing_repo.path, "foo", "tracked.%d" % n)
            with open(path, "w") as f:
                f.write("file %d\n" % n)
        self.backing_repo.add_files(["foo"])
        new_commit = self.backing_repo.commit("Add many files under foo/")

        # Spawn several threads that repeatedly rename ignored files under foo/
        stop = threading.Event()

        def rename_worker(thread_id):
            logging.info("rename thread %d starting", thread_id)
            path1 = os.path.join(self.repo.path, "foo", "_%d.log" % thread_id)
            path2 = os.path.join(self.repo.path, "foo", "_%d.log2" % thread_id)

            with open(path1, "w") as f:
                f.write("ignored %d\n" % thread_id)

            count = 0
            while not stop.is_set():
                os.rename(path1, path2)
                os.rename(path2, path1)
                count += 1

            logging.info("rename thread %d performed %d renames", thread_id, count)

        threads = []
        for n in range(num_rename_threads):
            thread = threading.Thread(target=rename_worker, args=(n,))
            threads.append(thread)
            thread.start()

        logging.info("===== starting checkouts")

        commits = [new_commit, self.commit3]
        for n in range(num_checkouts):
            self.repo.update(commits[n % len(commits)])

        logging.info("===== checkouts complete")

        stop.set()
        for thread in threads:
            thread.join()

        logging.info("===== threads stopped")

        # For the most part this test is mainly checking to ensure that
        # we reach this point without causing a deadlock.
        # However go ahead and check that the repository is left in an expected
        # state too.
        if num_checkouts % 2 == 0:
            self.assertEqual(self.commit3, self.repo.get_head_hash())
        else:
            self.assertEqual(new_commit, self.repo.get_head_hash())

        # Assert that the status is empty other than the ignored files
        # created by the rename threads
        self.assert_status(
            {f"foo/_{thread_id}.log": "I" for thread_id in range(num_rename_threads)}
        )

    def test_change_casing_of_populated(self) -> None:
        self.repo.update(self.commit1)

        self.repo.write_file("DIR2/FILE1", "one upper")
        self.repo.write_file("DIR2/FILE2", "two upper")
        upper = self.repo.commit("Upper")

        self.repo.update(self.commit1)

        self.repo.write_file("dir2/file1", "one lower")
        self.repo.write_file("dir2/file2", "two lower")
        self.repo.commit("Lower")

        # Make sure that everything was committed
        self.assert_status_empty()

        # Now update to the first commit...
        self.repo.update(upper)

        # And verify that status is clean
        self.assert_status_empty()

        self.assertEqual(self.read_file("DIR2/FILE1"), "one upper")
        self.assertEqual(self.read_file("DIR2/FILE2"), "two upper")

        if sys.platform == "win32":
            # Double check that the the old names could still be read thanks to
            # the insensitivity of the FS
            self.assertEqual(self.read_file("dir2/file1"), "one upper")
            self.assertEqual(self.read_file("dir2/file2"), "two upper")

        # Finally, make sure that the on-disk casing is the expected one.
        rootlisting = os.listdir(self.repo.path)
        self.assertIn("DIR2", rootlisting)
        self.assertNotIn("dir2", rootlisting)
        self.assertEqual(
            set(os.listdir(os.path.join(self.repo.path, "DIR2"))),
            {"FILE1", "FILE2"},
        )

    def test_change_casing_of_materialized_file(self) -> None:
        self.repo.update(self.commit1)
        self.repo.write_file("dir2/FILE1", "one upper")
        upper = self.repo.commit("Upper")

        self.repo.remove_file("dir2/FILE1")
        self.repo.commit("Removed")

        self.repo.write_file("dir2/file1", "one lower")
        self.repo.commit("Lower")

        self.repo.update(upper)

        # And verify that status is clean
        self.assert_status_empty()

        self.assertEqual(self.read_file("dir2/FILE1"), "one upper")

        dir2listing = os.listdir(os.path.join(self.repo.path, "dir2"))
        self.assertEqual({"FILE1"}, set(dir2listing))

    def test_change_casing_non_populated(self) -> None:
        self.repo.update(self.commit1)
        self.repo.write_file("dir2/FILE1", "one upper")
        upper = self.repo.commit("Upper")

        self.repo.remove_file("dir2/FILE1")
        self.repo.commit("Removed")

        self.repo.write_file("dir2/file1", "one lower")
        lower = self.repo.commit("Lower")

        # First, let's dematerialize everything
        self.repo.update(self.commit1)

        # Go to the lower cased commit
        self.repo.update(lower)

        # Then simply list all the entries
        os.listdir(os.path.join(self.repo.path, "dir2"))

        # Finally, update to the upper cased commit
        self.repo.update(upper)

        # And verify that status is clean
        self.assert_status_empty()

        self.assertEqual(self.read_file("dir2/FILE1"), "one upper")

        dir2listing = os.listdir(os.path.join(self.repo.path, "dir2"))
        self.assertEqual({"FILE1"}, set(dir2listing))

    def test_change_casing_with_untracked(self) -> None:
        self.repo.update(self.commit1)
        self.repo.write_file("DIR2/FILE1", "one upper")
        upper = self.repo.commit("Upper")

        self.repo.remove_file("DIR2/FILE1")
        self.repo.commit("Removed")

        self.repo.write_file("dir2/file1", "one lower")
        self.repo.commit("Lower")

        self.repo.write_file("dir2/untracked", "untracked")

        self.repo.update(upper)

        # On Windows, due to the untracked file, the casing of the directory
        # stays lower case, hence we do not expect "DIR2" to be present in the
        # working copy.
        dirname = "dir2" if sys.platform == "win32" else "DIR2"
        self.assertIn(dirname, set(os.listdir(self.repo.path)))

        if sys.platform == "win32":
            self.assertEqual(
                {"untracked", "FILE1"},
                set(os.listdir(os.path.join(self.repo.path, "DIR2"))),
            )
            self.assertEqual(self.read_file("DIR2/untracked"), "untracked")

        untrackedPath = "dir2/untracked"
        self.assert_status({untrackedPath: "?"})

    def test_update_to_null_with_untracked_directory(self) -> None:
        self.mkdir("foo/subdir/bar")
        self.repo.update("null")
        self.assertEqual(os.listdir(self.get_path("foo")), ["subdir"])

    if sys.platform == "win32":

        def test_remove_materialized_while_stopped(self) -> None:
            # Materialize the file so it's present on disk and will stay there when EdenFS is stopped
            self.repo.write_file("foo/bar.txt", "Materialized\n")

            # When EdenFS is stopped, materialized files can be modified,
            # causing the working copy to differ from EdenFS view of what it
            # should be.
            self.eden.shutdown()
            os.unlink(self.get_path("foo/bar.txt"))
            self.eden.start()

            self.assert_status_empty()
            self.assertEqual(self.read_file("foo/bar.txt"), "updated in commit 3\n")

    def test_update_dir_to_file(self) -> None:
        self.repo.remove_file("foo/subdir")
        self.repo.write_file("foo/subdir", "I am not a directory any more\n")
        commit4 = self.repo.commit("directory to file")

        # test without the directory materialized
        self.repo.update(self.commit3)
        self.repo.update(commit4)

        # and with the directory materialized
        self.repo.update(self.commit3)
        self.read_dir("foo/subdir")
        self.repo.update(commit4)

    async def kill_eden_during_checkout_and_restart(
        self, commit: str, keyValue: str
    ) -> None:
        async with self.eden.get_async_thrift_client() as client:
            await client.injectFault(
                FaultDefinition(
                    keyClass="TreeInode::checkout",
                    keyValueRegex=keyValue,
                    kill=True,
                )
            )

            try:
                self.repo.update(commit)
            except Exception:
                pass
            else:
                self.fail("'hg update' should've failed if eden crashes")

        # Restart eden
        if self.eden._process is not None:
            util.poll_until(self.eden._process.poll, timeout=30)
        self.eden = self.init_eden_client()
        self.eden.start()

    async def test_checkout_failure_before_start_preserves_status(self) -> None:
        self.repo.update(self.commit3)
        snapshot_path = (
            self.eden.client_dir_for_mount(pathlib.Path(self.mount)) / "SNAPSHOT"
        )
        snapshot_before = snapshot_path.read_bytes()

        async with self.eden.get_async_thrift_client() as client:
            await client.injectFault(
                FaultDefinition(
                    keyClass="waitForPendingWrites",
                    keyValueRegex=".*",
                    errorType="runtime_error",
                    errorMessage="checkout not started",
                    count=1,
                )
            )
            with self.assertRaisesRegex(EdenError, "checkout not started"):
                await client.checkOutRevision(
                    mountPoint=self.mount_path_bytes,
                    snapshotHash=self.commit1.encode(),
                    checkoutMode=CheckoutMode.NORMAL,
                    params=CheckOutRevisionParams(),
                )

        self.assertEqual(snapshot_before, snapshot_path.read_bytes())
        self.assertEqual("updated in commit 3\n", self.read_file("foo/bar.txt"))
        self.assert_status_empty()

        self.repo.update(self.commit2)
        self.assert_status_empty()

    async def test_resume_interrupted_update(self) -> None:
        """
        Test resuming a hg checkout after Eden was killed mid-checkout
        previously.
        """
        self.backing_repo.write_file("dir1/foo.txt", "Content 1")
        self.backing_repo.write_file("dir2/bar.txt", "Content 1")
        self.backing_repo.write_file("dir3/dog.txt", "Content 1")
        bottom = self.backing_repo.commit("Add")
        self.backing_repo.write_file("dir1/foo.txt", "Content 2")
        self.backing_repo.write_file("dir2/bar.txt", "Content 2")
        self.backing_repo.write_file("dir3/dog.txt", "Content 2")
        middle = self.backing_repo.commit("Edit")
        self.backing_repo.write_file("dir1/foo.txt", "Content 3")
        self.backing_repo.write_file("dir2/bar.txt", "Content 3")
        self.backing_repo.write_file("dir3/dog.txt", "Content 3")
        top = self.backing_repo.commit("Edit again")
        self.repo.update(top)

        # Do no-op write to trigger materialization of dir2. This will force the
        # checkout to process that inode, allowing us to hit the kill fault.
        # At that point, dir1 will have been processed and be pointing to
        # `bottom` while dir2 and dir3 won't have been processed, and be
        # pointing at top still. After we recover, we'll verify that dir2 and
        # dir3 were not materialized during the resumed checkout.
        self.repo.write_file("dir2/bar.txt", "Content 3")

        await self.kill_eden_during_checkout_and_restart(bottom, "dir2, false")

        with self.assertRaisesRegex(
            hgrepo.HgError, f"a previous checkout was interrupted.*{bottom}"
        ):
            self.assert_status_empty()

        with self.assertRaisesRegex(
            hgrepo.HgError, f"a previous checkout was interrupted.*{bottom}"
        ):
            self.repo.update(middle)

        output = self.repo.update(bottom, clean=True)
        self.assertEqual("update complete\n", output)

        async with self.eden.get_async_thrift_client() as client:
            inode_status = await client.debugInodeStatus(
                self.repo.path.encode("utf8"),
                b"",
                flags=DIS_ENABLE_FLAGS,
                sync=SyncBehavior(),
            )
            inodes = {i.path.decode("utf8"): i for i in inode_status}
            self.assertNotIn("dir1", inodes)
            # dir2 will either be not loaded or not materialized.
            dir2 = next(inode for inode in inodes[""].entries if inode.name == b"dir2")
            self.assertFalse(dir2.loaded and inodes["dir2"].materialized)
            self.assertNotIn("dir3", inodes)

    async def test_resume_interrupted_update_with_applied_file_loaded(self) -> None:
        self.backing_repo.write_file("dir1/foo.txt", "Content 1")
        self.backing_repo.write_file("dir2/bar.txt", "Content 1")
        self.backing_repo.write_file("dir3/dog.txt", "Content 1")
        bottom = self.backing_repo.commit("Add files")
        self.backing_repo.write_file("dir1/foo.txt", "Content 2")
        self.backing_repo.write_file("dir2/bar.txt", "Content 2")
        self.backing_repo.write_file("dir3/dog.txt", "Content 2")
        top = self.backing_repo.commit("Edit files")
        self.repo.update(top)

        self.repo.write_file("dir1/foo.txt", "Content 2")
        self.assertTrue(os.path.isdir(self.get_path("dir2")))

        with self.eden.get_thrift_client() as client:
            client.injectFault(
                FaultDefinition(
                    keyClass="TreeInode::checkout",
                    keyValueRegex="dir2, false",
                    block=True,
                )
            )

        checkout_error = []

        def update_to_bottom() -> None:
            try:
                self.repo.update(bottom)
            except hgrepo.HgError as ex:
                checkout_error.append(ex)

        checkout_thread = Thread(target=update_to_bottom)
        checkout_thread.start()
        try:
            self.wait_on_fault_hit(key_class="TreeInode::checkout")
            util.poll_until(
                lambda: self.read_file("dir1/foo.txt") == "Content 1", timeout=30
            )
        finally:
            with self.eden.get_thrift_client() as client:
                daemon_pid = client.getPid()
            # On Windows, os.kill(SIGTERM) calls TerminateProcess.
            kill_signal = signal.SIGTERM if sys.platform == "win32" else signal.SIGKILL
            os.kill(daemon_pid, kill_signal)
            checkout_thread.join(timeout=30)
        self.assertFalse(checkout_thread.is_alive())
        self.assertEqual(1, len(checkout_error))

        if self.eden._process is not None:
            util.poll_until(self.eden._process.poll, timeout=30)
        self.eden = self.init_eden_client()
        self.eden.start()

        self.assertEqual("Content 1", self.read_file("dir1/foo.txt"))

        async with self.eden.get_async_thrift_client() as client:
            conflicts = await client.checkOutRevision(
                mountPoint=self.mount_path_bytes,
                snapshotHash=bottom.encode(),
                checkoutMode=CheckoutMode.DRY_RUN,
                params=CheckOutRevisionParams(),
            )
        self.assertEqual([], conflicts)

        output = self.repo.update(bottom)
        self.assertEqual("update complete\n", output)
        self.assertEqual("Content 1", self.read_file("dir1/foo.txt"))
        self.assertEqual("Content 1", self.read_file("dir2/bar.txt"))
        self.assertEqual("Content 1", self.read_file("dir3/dog.txt"))
        self.assert_status_empty()

    async def test_restart_after_failed_interrupted_update_resume(self) -> None:
        if sys.platform == "win32":
            self.skipTest("PrjFS projects destination content after EdenFS restarts")

        self.backing_repo.write_file("dir1/foo.txt", "Content 1")
        self.backing_repo.write_file("dir2/bar.txt", "Content 1")
        bottom = self.backing_repo.commit("Add files")
        self.backing_repo.write_file("dir1/foo.txt", "Content 2")
        self.backing_repo.write_file("dir2/bar.txt", "Content 2")
        top = self.backing_repo.commit("Edit files")
        self.repo.update(top)

        self.assertEqual("Content 2", self.read_file("dir1/foo.txt"))
        self.assertTrue(os.path.isdir(self.get_path("dir2")))

        await self.kill_eden_during_checkout_and_restart(bottom, "<root>, false")

        self.assertEqual("Content 2", self.read_file("dir1/foo.txt"))
        self.assertEqual("Content 2", self.read_file("dir2/bar.txt"))

        async with self.eden.get_async_thrift_client() as client:
            await client.injectFault(
                FaultDefinition(
                    keyClass="TreeInode::checkout",
                    keyValueRegex="<root>, false",
                    errorType="runtime_error",
                    errorMessage="intentional checkout error",
                    count=1,
                )
            )

        with self.assertRaisesRegex(hgrepo.HgError, "intentional checkout error"):
            self.repo.update(bottom)
        self.assertEqual("Content 2", self.read_file("dir1/foo.txt"))
        self.assertEqual("Content 2", self.read_file("dir2/bar.txt"))

        self.eden.restart()

        output = self.repo.update(bottom)
        self.assertEqual("update complete\n", output)
        self.assertEqual("Content 1", self.read_file("dir1/foo.txt"))
        self.assertEqual("Content 1", self.read_file("dir2/bar.txt"))
        self.assert_status_empty()

    async def test_resume_interrupted_with_concurrent_update(self) -> None:
        self.repo.write_file("foo/baz.txt", "Content 3")
        await self.kill_eden_during_checkout_and_restart(self.commit1, "foo, false")

        async def start_force_checkout(commit: str) -> None:
            async with self.eden.get_async_thrift_client() as client:
                await client.checkOutRevision(
                    mountPoint=self.mount_path_bytes,
                    snapshotHash=commit.encode(),
                    checkoutMode=CheckoutMode.FORCE,
                    params=CheckOutRevisionParams(),
                )

        async with self.block_checkout():
            first_update = threading.Thread(
                target=util.run_async_func_in_thread,
                args=(start_force_checkout, self.commit1),
            )
            first_update.start()

            await self.wait_for_checkout_in_progress()

            # Now let's run update a second time.
            with self.assertRaisesRegex(
                EdenError, "another checkout operation is still in progress"
            ):
                await start_force_checkout(self.commit1)

        first_update.join()

    def test_update_with_hg_failure(self) -> None:
        """
        Test running `hg update` to check that a failure that leads to hg and
        edenfs states diverging is detected and fixed correctly.
        """
        new_contents = "New contents for bar.txt\n"
        self.backing_repo.write_file("foo/bar.txt", new_contents)
        self.backing_repo.commit("Update foo/bar.txt")

        self.assert_status_empty()
        self.assertNotEqual(new_contents, self.read_file("foo/bar.txt"))

        # We expect an exception, and expect it to leave the repo in a bad state
        with self.assertRaisesRegex(
            hgrepo.HgError, r"Error set by checkout-pre-set-parents FAILPOINTS"
        ):
            self.repo.update(
                self.commit2,
                env={
                    "FAILPOINTS": "checkout-pre-set-parents=return",
                },
            )

        # Confirm that we'll get an error message about the divergent state
        self.hg("config", "--local", "experimental.repair-eden-dirstate", "False")
        with self.assertRaisesRegex(BaseException, r"error computing status: .*"):
            self.repo.status()

        # Setting the experimental.repair-eden-dirstate config option to true (the default) will fix the issue
        self.hg("config", "--local", "experimental.repair-eden-dirstate", "True")
        self.repo.status()


class PrjFsState(Enum):
    UNKNOWN = 0
    VIRTUAL = 1
    PLACEHOLDER = 2
    HYDRATED_PLACEHOLDER = 3
    DIRTY_PLACEHOLDER = 4
    FULL = 5
    TOMBSTONE = 6


@hg_test
# pyre-ignore[13]: T62487924
class UpdateCacheInvalidationTest(EdenHgTestCase):
    # pyre-fixme[13]: Attribute `commit1` is never initialized.
    commit1: str
    # pyre-fixme[13]: Attribute `commit2` is never initialized.
    commit2: str
    # pyre-fixme[13]: Attribute `commit3` is never initialized.
    commit3: str
    # pyre-fixme[13]: Attribute `commit4` is never initialized.
    commit4: str
    # pyre-fixme[13]: Attribute `file2Stat` is never initialized.
    file2Stat: os.stat_result
    enable_fault_injection: bool = True

    def edenfs_logging_settings(self) -> Dict[str, str]:
        return {
            "eden.fs.inodes.TreeInode": "DBG5",
            "eden.fs.inodes.CheckoutAction": "DBG5",
            "eden.fs.inodes.CheckoutContext": "DBG5",
            "eden.fs.fuse.FuseChannel": "DBG3",
        }

    def populate_backing_repo(self, repo: hgrepo.HgRepository) -> None:
        repo.write_file("dir/file1", "one")
        repo.write_file("dir/file2", "two")
        self.commit1 = repo.commit("Initial commit.")

        repo.remove_file("dir/file1")
        self.commit2 = repo.commit("Remove file1")

        repo.write_file("dir/file3", "three")
        self.commit3 = repo.commit("Add file3")

        repo.update(self.commit1)
        repo.write_file("dir/file2", "new two")
        self.file2Stat = os.stat(repo.get_path("dir/file2"))
        self.commit4 = repo.commit("Change file2")

    def _populate_kernel_caches(self) -> None:
        # Populate the kernel's readdir caches.
        for _dirpath, _dirnames, _filenames in os.walk(self.repo.path):
            pass

    def _list_contents(self, path) -> Set[str]:
        return set(os.listdir(os.path.join(self.repo.path, path)))

    def _scan_contents(self, path) -> List[os.DirEntry]:
        entries = list(os.scandir(os.path.join(self.repo.path, path)))
        entries.sort(key=lambda entry: entry.name)
        return entries

    def test_update_adding_file_invalidates_tree_inode_caches(self) -> None:
        self.repo.update(self.commit2)
        self._populate_kernel_caches()
        self.assertEqual({"file2"}, self._list_contents("dir"))

        # The checkout operation should invalidate the kernel's caches.
        self.repo.update(self.commit3)
        self.assertEqual({"file2", "file3"}, self._list_contents("dir"))

    def test_update_removing_file_invalidates_tree_inode_caches(self) -> None:
        self.repo.update(self.commit1)
        self._populate_kernel_caches()

        self.assertEqual({"file1", "file2"}, self._list_contents("dir"))

        # The checkout operation should invalidate the kernel's caches.
        self.repo.update(self.commit2)
        self.assertEqual({"file2"}, self._list_contents("dir"))

    def test_changing_file_contents_creates_new_inode_and_flushes_dcache(self) -> None:
        self.repo.update(self.commit1)
        self._populate_kernel_caches()

        before = self._scan_contents("dir")

        self.repo.update(self.commit4)

        after = self._scan_contents("dir")

        self.assertEqual(["file1", "file2"], [x.name for x in before])
        self.assertEqual(["file1", "file2"], [x.name for x in after])

        self.assertEqual(before[0].inode(), after[0].inode())
        self.assertNotEqual(before[1].inode(), after[1].inode())

    def test_clean_update_removes_added_file(self) -> None:
        self.repo.update(self.commit1)

        self.write_file("dir/new_file.txt", "new file")
        self.hg("add", "dir/new_file.txt")
        self.assertTrue(os.path.isfile(self.get_path("dir/new_file.txt")))
        self.assert_status({"dir/new_file.txt": "A"})

        self._populate_kernel_caches()
        self.repo.update(".", clean=True)
        self.assert_status({"dir/new_file.txt": "?"})
        self.assertTrue(os.path.isfile(self.get_path("dir/new_file.txt")))
        self.assert_dirstate_empty()

        self.assertEqual({"file1", "file2", "new_file.txt"}, self._list_contents("dir"))

    def test_clean_update_adds_removed_file(self) -> None:
        self.hg("remove", "dir/file1")
        self.assertFalse(os.path.isfile(self.get_path("dir/file1")))
        self.assert_status({"dir/file1": "R"})

        self._populate_kernel_caches()
        self.repo.update(".", clean=True)
        self.assert_status({})
        self.assertTrue(os.path.isfile(self.get_path("dir/file1")))
        self.assert_dirstate_empty()

        self.assertEqual({"file1", "file2"}, self._list_contents("dir"))

    def test_update_change_stat(self) -> None:
        self.repo.write_file("dir/file2", "foobar")
        self.repo.commit("Change file2")

        filepath = self.get_path("dir/file2")
        prestats = os.stat(filepath)
        self.assertEqual(prestats.st_size, 6)

        self.repo.update(self.commit4)

        poststats = os.stat(filepath)
        self.assertEqual(poststats.st_size, 7)

    def test_open_unmaterialized_file(self) -> None:
        filepath = self.get_path("dir/file2")
        # Open the file for writing, which should truncate the file
        with open(filepath, "w") as f:  # noqa
            pass

        self.assertEqual(self.read_file("dir/file2"), "")

    def test_open_materialized_file(self) -> None:
        # This test is to demonstrate that materialized files behave as expected
        filepath = self.get_path("dir/file2")
        # Open the file for reading, which should materialize the file
        with open(filepath, "r") as f:  # noqa
            pass
        # Open the file for writing, which should truncate the file
        with open(filepath, "w") as f:  # noqa
            pass

        # The file should be truncated
        self.assertEqual(self.read_file("dir/file2"), "")

    def test_open_unmaterialized_folder(self) -> None:
        filepath = self.get_path("dir")
        # Expect an error when trying to open a folder
        if sys.platform == "win32":
            with self.assertRaises(PermissionError):
                with open(filepath, "w") as f:  # noqa
                    pass
        else:
            with self.assertRaises(IsADirectoryError):
                with open(filepath, "w") as f:  # noqa
                    pass

    def test_open_unmaterialized_touch(self) -> None:
        # Test that running non-write operations does not truncate the file
        filepath = self.get_path("dir/file2")
        self.touch("dir/file2")
        # Verify the file now exists and has the same contents
        self.assertTrue(os.path.isfile(filepath))
        self.assertEqual(self.read_file("dir/file2"), "new two")
        postStat = os.stat(filepath)
        self.assertEqual(postStat.st_size, self.file2Stat.st_size)
        self.assertNotEqual(postStat.st_mtime, self.file2Stat.st_mtime)

    if sys.platform == "win32":  # noqa: C901

        async def _retry_update_after_failed_entry_cache_invalidation(
            self,
            initial_state: PrjFsState,
        ) -> None:
            self.hg(
                "config", "--local", "experimental.abort-on-eden-conflict-error", "True"
            )

            if initial_state == PrjFsState.PLACEHOLDER:
                # Stat file2 to populate a placeholder, making the file non-virtual.
                os.stat(self.get_path("dir/file2"))
            elif initial_state == PrjFsState.HYDRATED_PLACEHOLDER:
                # Read file2 to hydrate its placeholder.
                self.read_file("dir/file2")
            elif initial_state == PrjFsState.FULL:
                original_file2 = self.read_file("dir/file2")
                self.write_file("dir/file2", "modified two")
                self.write_file("dir/file2", original_file2)
                self.assert_status({})
            else:
                raise ValueError("Unsupported initial state: {}".format(initial_state))

            # Simulate failed invalidation of file2.
            async with self.eden.get_async_thrift_client() as client:
                await client.injectFault(
                    FaultDefinition(
                        keyClass="invalidateChannelEntryCache",
                        keyValueRegex="file2",
                        errorType="runtime_error",
                    )
                )
            with self.assertRaises(hgrepo.HgError):
                self.repo.update(self.commit1)

            self.assertEqual(self.repo.get_head_hash(), self.commit4)
            self.assert_unfinished_operation("update")

            # Try to update again, this time without failure.
            async with self.eden.get_async_thrift_client() as client:
                await client.unblockFault(
                    UnblockFaultArg(
                        keyClass="invalidateChannelEntryCache", keyValueRegex="file2"
                    )
                )
            self.repo.update(self.commit1)

            self.assertEqual(self.repo.get_head_hash(), self.commit1)

            # TODO(mshroyer): These two assertions should succeed for a
            # successfully retried invalidation, but at the moment they fail.
            # self.assert_status({}, op=None)
            # self.assertEqual(self.read_file("dir/file2"), "two")

        async def test_retry_update_after_failed_entry_cache_invalidation_placeholder(
            self,
        ) -> None:
            await self._retry_update_after_failed_entry_cache_invalidation(
                initial_state=PrjFsState.PLACEHOLDER,
            )

        async def test_retry_update_after_failed_entry_cache_invalidation_hydrated_placeholder(
            self,
        ) -> None:
            await self._retry_update_after_failed_entry_cache_invalidation(
                initial_state=PrjFsState.HYDRATED_PLACEHOLDER,
            )

        async def test_retry_update_after_failed_entry_cache_invalidation_full(
            self,
        ) -> None:
            await self._retry_update_after_failed_entry_cache_invalidation(
                initial_state=PrjFsState.FULL,
            )

        def test_update_clean_lay_placeholder_on_full(self) -> None:
            self.repo.write_file("dir2/dir3/file1", "foobar")
            commit5 = self.repo.commit("dir2")

            # Make the directory virtual by update back and forth
            self.repo.update(self.commit4)
            self.repo.update(commit5)

            # Now, remove the whole hierarchy
            self.rm("dir2/dir3/file1")
            self.rmdir("dir2/dir3")
            self.rmdir("dir2")

            # And re-create the directory
            self.mkdir("dir2/dir3")

            self.repo.update(commit5, clean=True)

            state = prjfs.PrjGetOnDiskFileState(self.mount_path / "dir2")
            self.assertEqual(state, prjfs.PRJ_FILE_STATE.Placeholder)

        def test_update_clean_keep_not_in_commit_full(self) -> None:
            self.write_file("dir2/dir3/file1", "foobar")
            self.repo.update(self.commit3)

            state = prjfs.PrjGetOnDiskFileState(self.mount_path / "dir2")
            self.assertEqual(state, prjfs.PRJ_FILE_STATE.Full)

        def test_update_clean_lay_placeholder_on_existing(self) -> None:
            self.repo.write_file("dir2/dir3/file1", "foobar")
            commit5 = self.repo.commit("dir2")

            # Make sure the directory is removed
            self.repo.update(self.commit4)

            self.assertFalse(os.path.exists(self.get_path("dir2")))

            # And re-create the directory
            self.mkdir("dir2/dir3")

            self.repo.update(commit5, clean=True)

            state = prjfs.PrjGetOnDiskFileState(self.mount_path / "dir2")
            self.assertEqual(state, prjfs.PRJ_FILE_STATE.Placeholder)

            state = prjfs.PrjGetOnDiskFileState(self.mount_path / "dir2" / "dir3")
            self.assertEqual(state, prjfs.PRJ_FILE_STATE.Placeholder)

        def test_update_remove_on_full(self) -> None:
            self.repo.write_file("dir2/dir3/file1", "foobar")
            commit5 = self.repo.commit("dir2")

            # Make the directory virtual by update back and forth
            self.repo.update(self.commit4)
            self.repo.update(commit5)

            # Now, remove the whole hierarchy
            self.rm("dir2/dir3/file1")
            self.rmdir("dir2/dir3")
            self.rmdir("dir2")

            # And re-create the directory
            self.mkdir("dir2/dir3")

            self.repo.update(self.commit4, clean=True)

            self.assertFalse(os.path.exists(self.get_path("dir2")))

        def test_file_locked_change_content(self) -> None:
            # TODO(zhaolong): remove this once this option is enabled everywhere.
            self.hg(
                "config", "--local", "experimental.abort-on-eden-conflict-error", "True"
            )
            self.repo.update(self.commit1)

            with open_locked(self.get_path("dir/file2")):
                with self.assertRaises(hgrepo.HgError):
                    self.repo.update(self.commit4)

            self.assertEqual(self.read_file("dir/file2"), "new")

        def test_file_locked_removal(self) -> None:
            # TODO(zhaolong): remove this once this option is enabled everywhere.
            self.hg(
                "config", "--local", "experimental.abort-on-eden-conflict-error", "True"
            )
            self.repo.update(self.commit3)
            self.assertEqual(self.read_file("dir/file3"), "three")
            with open_locked(self.get_path("dir/file3")):
                with self.assertRaises(hgrepo.HgError):
                    self.repo.update(self.commit4)

            self.assertEqual(self.read_file("dir/file3"), "three")
            with self.assertRaises(hgrepo.HgError):
                self.repo.status()


@hg_test
# pyre-ignore[13]: T62487924
class PrjFSStressTornReads(EdenHgTestCase):
    long_file_commit: str = ""
    short_file_commit: str = ""

    enable_fault_injection: bool = True

    def populate_backing_repo(self, repo: hgrepo.HgRepository) -> None:
        repo.write_file("file", "1234567890\n")
        self.long_file_commit = repo.commit("Initial commit.")
        repo.write_file("file", "54321\n")
        self.short_file_commit = repo.commit("Shorter file commit.")

    def edenfs_logging_settings(self) -> Dict[str, str]:
        return {"eden.strace": "DBG7"}

    def test_torn_read_long_to_short(self) -> None:
        self.repo.update(self.long_file_commit)
        rel_path = "file"
        path = self.mount_path / rel_path

        read_exception: Optional[OSError] = None

        def read_file() -> None:
            nonlocal read_exception
            with self.run_with_blocking_fault(
                keyClass="PrjfsDispatcherImpl::read",
                keyValueRegex="file",
            ):
                try:
                    with path.open("rb") as f:
                        f.read()
                except Exception as err:
                    # pyrefly: ignore [bad-assignment]
                    read_exception = err

        read_thread = Thread(target=read_file)
        read_thread.start()

        try:
            self.repo.update(self.short_file_commit)
        except Exception:
            pass

        self.remove_fault(keyClass="PrjfsDispatcherImpl::read", keyValueRegex="file")
        self.wait_on_fault_unblock(
            keyClass="PrjfsDispatcherImpl::read", keyValueRegex="file"
        )
        read_thread.join()
        self.assertIsNotNone(read_exception)
        if read_exception is not None:  # pyre :(
            self.assertEqual(read_exception.errno, 22)  # invalid argument error

    def test_torn_read_short_to_long(self) -> None:
        self.repo.update(self.short_file_commit)

        rel_path = "file"
        path = self.mount_path / rel_path

        read_contents = None

        def read_file() -> None:
            nonlocal read_contents
            with self.run_with_blocking_fault(
                keyClass="PrjfsDispatcherImpl::read",
                keyValueRegex="file",
            ):
                with path.open("rb") as f:
                    read_contents = f.read()

        read_thread = Thread(target=read_file)
        read_thread.start()

        try:
            self.repo.update(self.long_file_commit)
        except Exception:
            pass

        self.remove_fault(keyClass="PrjfsDispatcherImpl::read", keyValueRegex="file")
        self.wait_on_fault_unblock(
            keyClass="PrjfsDispatcherImpl::read", keyValueRegex="file"
        )
        read_thread.join()
        self.assertIsNotNone(read_contents)
        # This is not correct behavior, we want the contents to be either
        # the contents from the first or second commit, not this inconsistent
        # mashup. This test is for not documenting the behavior of torn reads.
        # This case requires a larger fix.
        # TODO(kmancini): fix torn reads.
        self.assertEqual(read_contents, b"123456")

    def test_torn_read_invalidation(self) -> None:
        self.repo.update(self.long_file_commit)
        rel_path = "file"
        path = self.mount_path / rel_path

        read_exception: Optional[OSError] = None

        def read_file() -> None:
            nonlocal read_exception
            with self.run_with_blocking_fault(
                keyClass="PrjfsDispatcherImpl::read",
                keyValueRegex="file",
            ):
                try:
                    with path.open("rb") as f:
                        f.read()
                except Exception as err:
                    # pyrefly: ignore [bad-assignment]
                    read_exception = err

        read_thread = Thread(target=read_file)
        read_thread.start()

        try:
            self.repo.update(self.short_file_commit)
        except Exception:
            pass

        self.remove_fault(keyClass="PrjfsDispatcherImpl::read", keyValueRegex="file")
        self.wait_on_fault_unblock(
            keyClass="PrjfsDispatcherImpl::read", keyValueRegex="file"
        )
        read_thread.join()
        self.assertIsNotNone(read_exception)
        if read_exception is not None:  # pyre :(
            self.assertEqual(read_exception.errno, 22)  # invalid argument error

        def read_file_without_error() -> Optional[str]:
            try:
                with path.open("rb") as f:
                    # pyrefly: ignore [bad-return]
                    return f.read()
            except Exception:
                return None

        contents = util.poll_until(
            read_file_without_error,
            timeout=30,
            interval=2,
            timeout_ex=Exception(
                f"path: {path} did not become readable. Invalidation didn't happen?"
            ),
        )

        self.assertEqual(contents, b"54321\n")

    async def test_torn_read_invalidation_shutdown(self) -> None:
        self.repo.update(self.long_file_commit)
        rel_path = "file"
        path = self.mount_path / rel_path

        read_exception: Optional[OSError] = None

        def read_file() -> None:
            nonlocal read_exception
            with self.run_with_blocking_fault(
                keyClass="PrjfsDispatcherImpl::read",
                keyValueRegex="file",
            ):
                try:
                    with path.open("rb") as f:
                        f.read()
                except Exception as err:
                    # pyrefly: ignore [bad-assignment]
                    read_exception = err

        read_thread = Thread(target=read_file)
        read_thread.start()

        self.wait_on_fault_hit(key_class="PrjfsDispatcherImpl::read")

        try:
            self.repo.update(self.short_file_commit)
        except Exception:
            pass

        async with self.eden.get_async_thrift_client() as client:
            await client.injectFault(
                FaultDefinition(
                    keyClass="PrjFSChannelInner::getFileData-invalidation",
                    keyValueRegex="file",
                    block=True,
                )
            )

        self.remove_fault(keyClass="PrjfsDispatcherImpl::read", keyValueRegex="file")
        self.wait_on_fault_unblock(
            keyClass="PrjfsDispatcherImpl::read", keyValueRegex="file"
        )
        read_thread.join()

        def stop_eden() -> None:
            self.eden.shutdown()

        shutdown_thread = Thread(target=stop_eden)
        shutdown_thread.start()

        try:
            self.remove_fault(
                keyClass="PrjFSChannelInner::getFileData-invalidation",
                keyValueRegex="file",
            )
            self.unblock_fault(
                keyClass="PrjFSChannelInner::getFileData-invalidation",
                keyValueRegex="file",
            )
        finally:
            # we can't let shutdown be on going when the test tries to
            # clean up or we might hide the actual error.
            # throws if eden exits uncleanly - which it does if there is some
            # sort of crash.
            shutdown_thread.join()


@hg_test
# pyre-ignore[13]: T62487924
class UpdateDedicatedExecutorTest(EdenHgTestCase):
    # pyre-fixme[13]: Attribute `commit1` is never initialized.
    commit1: str
    # pyre-fixme[13]: Attribute `commit2` is never initialized.
    commit2: str
    # pyre-fixme[13]: Attribute `commit3` is never initialized.
    commit3: str
    enable_fault_injection: bool = True

    def edenfs_logging_settings(self) -> Dict[str, str]:
        return {
            "eden.fs.inodes.TreeInode": "DBG5",
            "eden.fs.inodes.CheckoutAction": "DBG5",
            "eden.fs.inodes.CheckoutContext": "DBG5",
        }

    def populate_backing_repo(self, repo: hgrepo.HgRepository) -> None:
        repo.write_file("hello.txt", "hola")
        repo.write_file(".gitignore", "ignoreme\n")
        repo.write_file("foo/.gitignore", "*.log\n")
        repo.write_file("foo/bar.txt", "test\n")
        repo.write_file("foo/subdir/test.txt", "test\n")
        self.commit1 = repo.commit("Initial commit.")

        repo.write_file("foo/.gitignore", "*.log\n/_*\n")
        self.commit2 = repo.commit("Update foo/.gitignore")

        repo.write_file("foo/bar.txt", "updated in commit 3\n")
        self.commit3 = repo.commit("Update foo/.gitignore")

    def test_checkout_on_dedicated_executor(self) -> None:
        """Test that checkout can be completed on a dedicated executor."""
        self.assert_status_empty()

        self.write_file("hello.txt", "saluton")
        self.assert_status({"hello.txt": "M"})

        self.repo.update(".", clean=True)
        self.assertEqual("hola", self.read_file("hello.txt"))
        self.assert_status_empty()
        self.write_file("goodbye.txt", "cya")
        self.assert_status({"goodbye.txt": "?"})
