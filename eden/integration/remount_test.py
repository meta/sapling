#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-unsafe

import json
import os
import pathlib
import sys

from eden.fs.service.eden.thrift_types import EdenError, MountArgument, MountState

from .lib import edenclient, testcase


NOT_MOUNTED_DIR_LIST = ["README_EDEN.txt"]


@testcase.eden_repo_test
class RemountTest(testcase.EdenRepoTest):
    def populate_repo(self) -> None:
        self.repo.write_file("hello", "hola\n")
        self.repo.write_file("adir/file", "foo!\n")
        self.repo.symlink("slink", "hello")
        self.repo.commit("Initial commit.")

    def _assert_mounted(self, mount_path: str) -> None:
        expected_entries = {".eden", "adir", "hello", "slink"}
        self.assert_checkout_root_entries(expected_entries, path=mount_path)

    def test_remount_basic(self) -> None:
        # Mount multiple clients
        self._clone_checkouts(5)

        self.eden.shutdown()
        self.eden.start()

        # Verify that clients are remounted on startup
        for i in range(5):
            self._assert_mounted(f"{self.mount}-{i}")

        # Verify that default repo created by EdenRepoTestBase is remounted
        self._assert_mounted(self.mount)

    def test_git_and_hg(self) -> None:
        # Create git and hg repositories for mounting
        repo_names = {"git": "git_repo", "hg": "hg_repo"}
        git_repo = self.create_git_repo(repo_names["git"])
        hg_repo = self.create_hg_repo(repo_names["hg"])
        repos = {"git_repo": git_repo, "hg_repo": hg_repo}

        git_repo.write_file("hello", "hola\n")
        git_repo.commit("Initial commit.")

        hg_repo.write_file("hello", "hola\n")
        hg_repo.commit("Initial commit.")

        # Mount git and hg clients
        for name, repo in repos.items():
            for i in range(3):
                self.eden.clone(repo.path, os.path.join(self.mounts_dir, f"{name}-{i}"))

        self.eden.shutdown()
        self.eden.start()

        # Verify that clients are remounted on startup
        for name, repo in repos.items():
            scm_type = repo.get_type()
            for i in range(3):
                mount_path = os.path.join(self.mounts_dir, f"{name}-{i}")
                self.assert_checkout_root_entries(
                    {".eden", "hello"}, mount_path, scm_type=scm_type
                )

                hello = os.path.join(mount_path, "hello")
                with open(hello, "r") as f:
                    self.assertEqual("hola\n", f.read())

    def test_partial_unmount(self) -> None:
        self._clone_checkouts(5)

        # Remove the main checkout
        self.eden.remove(self.mount)
        self.assertFalse(os.path.exists(self.mount))

        self.eden.shutdown()
        self.eden.start()

        # Verify that clients that were still mounted at shutdown are remounted
        for i in range(5):
            self._assert_mounted(f"{self.mount}-{i}")

        # Verify that unmounted client is not remounted
        self.assertFalse(os.path.exists(self.mount))

    def test_restart_twice(self) -> None:
        self.maxDiff = None
        self._clone_checkouts(5)

        self.eden.shutdown()
        self.eden.start()

        # Remove some checkouts
        self.eden.remove(self.mount)
        self.eden.remove(self.mount + "-3")

        self.eden.shutdown()
        self.eden.start()

        # Verify that clients that were still mounted at shutdown are remounted
        checkouts = self.eden.list_cmd_simple()
        expected_checkouts = {
            f"{self.mount}-{i}": "RUNNING" for i in range(5) if i != 3
        }
        self.assertEqual(expected_checkouts, checkouts)

        for i in range(5):
            if i == 3:
                continue
            self._assert_mounted(f"{self.mount}-{i}")

        # Verify that unmounted clients are not remounted
        self.assertFalse(os.path.exists(self.mount))
        self.assertFalse(os.path.exists(self.mount + "3"))

    def test_try_remount_existing_mount(self) -> None:
        """Verify trying to mount an existing mount prints a sensible error."""
        mount_destination = self.mount + "-0"
        self.eden.clone(self.repo.path, mount_destination)
        with self.assertRaises(edenclient.EdenCommandError) as context:
            self.eden.run_cmd("mount", mount_destination)
        self.assertIn(
            (
                "ERROR: Mount point in use! %s is already mounted by EdenFS.\n"
                % mount_destination
            ),
            context.exception.stderr,
        )
        self.assertEqual(1, context.exception.returncode)

    def test_empty_config_json(self) -> None:
        self._clone_checkouts(5)

        # Clear the contents of config.json file
        open(os.path.join(self.eden_dir, "config.json"), "w").close()

        self.eden.shutdown()
        self.eden.start()

        self._verify_not_mounted(5)

    def test_deleted_config_json(self) -> None:
        self._clone_checkouts(5)

        # Delete the config.json file
        os.remove(os.path.join(self.eden_dir, "config.json"))

        self.eden.shutdown()
        self.eden.start()

        self._verify_not_mounted(5)

    def test_incorrect_config_json(self) -> None:
        self._clone_checkouts(5)

        # Reload config.json file with incorrect data
        config_data = {
            self.mounts_dir + "/incorrectdir1": "jskdnbailreylbflhv",
            self.mounts_dir + "/incorrectdir2": "ndjnhaleruybfialus",
        }

        with open(os.path.join(self.eden_dir, "config.json"), "w") as f:
            json.dump(config_data, f, indent=2, sort_keys=True)
            f.write("\n")

        self.eden.shutdown()
        self.eden.start()

        self._verify_not_mounted(5)

        # Incorrect mount paths from config.json should not have been created
        for incorrect_mount in config_data:
            self.assertFalse(os.path.isdir(incorrect_mount))

    def test_bad_config_json(self) -> None:
        self._clone_checkouts(5)

        # Reload config.json file with random data
        with open(os.path.join(self.eden_dir, "config.json"), "w") as f:
            f.write("njfaeriurbvailuwrawikc\n")

        self.eden.shutdown()
        self.eden.start()
        self._verify_not_mounted(5)

    def _clone_checkouts(self, num_mounts) -> None:
        for i in range(num_mounts):
            self.eden.clone(self.repo.path, self.mount + "-" + str(i))

    def _verify_not_mounted(self, num_mounts, main_mounted: bool = False) -> None:
        # Verify that no clients are remounted. No errors should be thrown here
        for i in range(num_mounts):
            entries = sorted(os.listdir(self.mount + "-" + str(i)))
            self.assertEqual(NOT_MOUNTED_DIR_LIST, entries)

        if not main_mounted:
            entries = sorted(os.listdir(self.mount))
            self.assertEqual(NOT_MOUNTED_DIR_LIST, entries)


@testcase.eden_repo_test
class CorruptCheckoutConfigTest(testcase.EdenRepoTest):
    """Mounting a checkout whose on-disk state is corrupt must fail with a
    clean EdenError and leave the daemon fully usable."""

    def populate_repo(self) -> None:
        self.repo.write_file("hello", "hola\n")
        self.repo.commit("Initial commit.")

    def _prepare_unmounted_checkout(self) -> tuple[str, pathlib.Path]:
        mount2 = os.path.join(self.mounts_dir, "mount2")
        self.eden.clone(self.repo.path, mount2)
        self.eden.unmount(pathlib.Path(mount2))
        return mount2, self.eden.client_dir_for_mount(pathlib.Path(mount2))

    async def _mount(self, client, mount2: str, client_dir: pathlib.Path) -> None:
        await client.mount(
            MountArgument(
                mountPoint=os.fsencode(mount2),
                edenClientPath=os.fsencode(client_dir),
                readOnly=False,
            )
        )

    async def _assert_daemon_healthy(self, client, pid: int) -> None:
        self.assertEqual(pid, (await client.getDaemonInfo()).pid)
        self.assertEqual("hola\n", self.read_file("hello"))

    async def test_mount_config_toml_without_repository_section(self) -> None:
        if sys.platform != "linux":
            self.skipTest("Linux daemon reliability regression")
        self.addCleanup(self.eden.shutdown, retry=True)
        mount2, client_dir = self._prepare_unmounted_checkout()
        config_toml = client_dir / "config.toml"
        config_toml.write_text('[redirection-targets]\nfoo = "bar"\n')

        async with self.eden.get_async_thrift_client(timeout=30) as client:
            pid = (await client.getDaemonInfo()).pid
            with self.assertRaises(EdenError) as ctx:
                await self._mount(client, mount2, client_dir)
            self.assertIn("missing [repository] table", str(ctx.exception))
            await self._assert_daemon_healthy(client, pid)

    async def test_mount_config_toml_without_repository_section_control(
        self,
    ) -> None:
        if sys.platform != "linux":
            self.skipTest("Linux daemon reliability regression")
        self.addCleanup(self.eden.shutdown, retry=True)
        mount2, client_dir = self._prepare_unmounted_checkout()
        config_toml = client_dir / "config.toml"
        config_toml.write_text(config_toml.read_text() + "\n[redirection-targets]\n")

        async with self.eden.get_async_thrift_client(timeout=30) as client:
            pid = (await client.getDaemonInfo()).pid
            await self._mount(client, mount2, client_dir)
            await self._assert_daemon_healthy(client, pid)

    def test_start_with_config_toml_without_repository_section(self) -> None:
        if sys.platform != "linux":
            self.skipTest("Linux daemon reliability regression")
        mount2 = os.path.join(self.mounts_dir, "mount2")
        self.eden.clone(self.repo.path, mount2)
        config_toml = (
            self.eden.client_dir_for_mount(pathlib.Path(mount2)) / "config.toml"
        )
        self.eden.shutdown()
        config_toml.write_text('[redirection-targets]\nfoo = "bar"\n')

        # The daemon must come up and serve the healthy checkout; only the
        # corrupt one may fail to remount.
        self.eden.start()
        self.assertEqual("hola\n", self.read_file("hello"))
        with self.eden.get_thrift_client() as client:
            self.assertEqual(
                MountState.RUNNING,
                self.eden.get_mount_state(pathlib.Path(self.mount), client),
            )
            self.assertIsNone(self.eden.get_mount_state(pathlib.Path(mount2), client))
            self.assertEqual(1, client.getCounter("startup_mount_failures"))
