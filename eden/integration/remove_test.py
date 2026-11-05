#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-unsafe

import json
import os
import pathlib
import shutil
import subprocess
import sys
import time
from typing import Set

from parameterized import parameterized

from .lib import testcase


class RemoveTestBase(testcase.EdenRepoTest):
    """Base class for Eden remove command tests."""

    # pyre-fixme[13]: Attribute `expected_mount_entries` is never initialized.
    expected_mount_entries: Set[str]
    rust_env = {"EDENFSCTL_ONLY_RUST": "1"}

    def populate_repo(self) -> None:
        self.repo.write_file("hello", "hola\n")
        self.repo.write_file("adir/file", "foo!\n")
        self.repo.write_file("bdir/test.sh", "#!/bin/bash\necho test\n", mode=0o755)
        self.repo.write_file("bdir/noexec.sh", "#!/bin/bash\necho test\n")
        self.repo.symlink("slink", os.path.join("adir", "file"))

        self.repo.commit("Initial commit.")

        self.expected_mount_entries = {".eden", "adir", "bdir", "hello", "slink"}
        if self.repo.get_type() in ["hg", "filteredhg"]:
            self.expected_mount_entries.add(".hg")

    def load_directory_map(self) -> dict[str, str]:
        config_path = self.eden.eden_dir / "config.json"
        with open(config_path, "r") as opened_config:
            return json.load(opened_config)

    def write_directory_map(self, directory_map: dict[str, str]) -> None:
        config_path = self.eden.eden_dir / "config.json"
        config_path.write_text(json.dumps(directory_map) + "\n")


@testcase.eden_repo_test
class RemoveTest(RemoveTestBase):
    """Tests for the eden remove command.

    These tests exercise various remove scenarios including redirections,
    timeouts, and handling of busy mounts.
    """

    @parameterized.expand(
        [
            ("rust", {"EDENFSCTL_ONLY_RUST": "1"}),
            ("python", {"EDENFSCTL_SKIP_RUST": "1"}),
        ]
    )
    def test_remove_checkout_with_redirections(self, impl: str, env: dict) -> None:
        """Test that eden rm properly unmounts redirections before removing checkout.


        Tests both Rust and Python implementations of eden rm.
        """
        # Setup: add a bind redirection
        repo_path = os.path.join(f"test-redirect-{impl}", "bind-mount")
        self.eden.run_cmd("redirect", "add", "--mount", self.mount, repo_path, "bind")

        # Verify redirection exists and is functional
        output = self.eden.run_cmd("redirect", "list", "--json", "--mount", self.mount)
        redirections = json.loads(output)
        our_redir = [r for r in redirections if r["repo_path"] == repo_path]
        self.assertEqual(len(our_redir), 1, msg="should have our redirection")

        self.assertIn(
            our_redir[0]["state"],
            ["ok", "not-mounted"],
            msg=f"redirection {repo_path} should be in a valid state",
        )

        # Verify the redirection path exists
        redirect_path = os.path.join(self.mount, repo_path)
        self.assertTrue(
            os.path.exists(redirect_path),
            msg=f"redirection path {redirect_path} should exist",
        )

        # Run eden rm with the appropriate env var to force the implementation
        self.eden.run_cmd("remove", "--yes", self.mount, env=env)

        # Verify the mount is removed
        self.assertFalse(
            os.path.exists(self.mount),
            msg=f"mount point should be removed after eden rm ({impl})",
        )

        # Re-clone to verify eden is still working
        self.eden.clone(self.repo.path, self.mount)
        self.assertTrue(
            os.path.isdir(self.mount),
            msg="should be able to re-clone after eden rm",
        )

    @parameterized.expand(
        [
            ("rust", {"EDENFSCTL_ONLY_RUST": "1"}),
            ("python", {"EDENFSCTL_SKIP_RUST": "1"}),
        ]
    )
    def test_remove_aux_process_timeout(self, impl: str, env: dict) -> None:
        """Test that eden rm correctly handles aux process timeout.

        Uses two mounts with an injected 2-second delay and a 1s timeout
        to verify that:
        1. Each mount gets its own full timeout
        2. The timeout message includes the step name to help identify what was stuck
        3. Both mounts are removed despite the timeouts
        """
        self.skipTest("flakiness")
        # Create a second mount
        mount2 = os.path.join(self.tmp_dir, "mount2")
        self.eden.clone(self.repo.path, mount2)

        full_env = {
            **env,
            # Inject a 2-second delay to simulate slow aux process stopping
            "TEST_ONLY_AUX_PROCESSES_STOP_DELAY_SECS": "2",
        }

        start_time = time.time()
        result = self.eden.run_unchecked(
            "remove",
            "--yes",
            "--timeout",
            "1",
            self.mount,
            mount2,
            env=full_env,
            capture_output=True,
            text=True,
        )
        elapsed_time = time.time() - start_time

        # With per-mount timeout, each mount gets 1s timeout independently.
        # Total time: 2 mounts × 1s timeout + overhead = ~2-3s
        self.assertGreater(
            elapsed_time,
            2.0,
            f"eden rm ({impl}) should use per-mount timeout (expected >2s, got {elapsed_time:.1f}s)",
        )
        self.assertLess(
            elapsed_time,
            3.0,
            f"eden rm ({impl}) should not wait for the full delay (expected <3s, got {elapsed_time:.1f}s)",
        )

        output = result.stderr or ""

        # Verify the timeout message includes step information
        self.assertIn(
            "timed out",
            output.lower(),
            f"eden rm ({impl}) should log timeout message. Output: {output}",
        )
        # Check that the output includes the step name.
        # Python uses a step tracker that starts at "initializing" (the delay is injected
        # before the step is updated), while Rust reports "unmounting redirections" directly.
        if impl == "python":
            expected_step = "initializing"
        else:
            expected_step = "unmounting redirections"
        self.assertIn(
            expected_step,
            output.lower(),
            f"eden rm ({impl}) should log the step name that timed out. Output: {output}",
        )

        # Verify both mounts were removed despite the timeouts
        self.assertFalse(
            os.path.exists(self.mount),
            "first mount point should be removed",
        )
        self.assertFalse(
            os.path.exists(mount2),
            "second mount point should be removed",
        )

    @parameterized.expand(
        [
            ("rust", {"EDENFSCTL_ONLY_RUST": "1"}),
            ("python", {"EDENFSCTL_SKIP_RUST": "1"}),
        ]
    )
    def test_remove_with_busy_bind_mount(self, impl: str, env: dict) -> None:
        """Test eden rm behavior when a bind mount is actively in use.

        Creates a bind redirection and holds an open file handle on a file in
        the bind mount. This simulates an aux process (like buck) actively
        using the redirection.

        This test only runs on macOS because unmount behavior differs by platform:
        - macOS: uses MNT_FORCE for unmount, which can hang if the mount is busy
        - Linux: uses MNT_DETACH (lazy unmount), always succeeds immediately
        - Windows: uses symlinks instead of bind mounts, unlink() is instant

        The test verifies that eden rm with --timeout completes without hanging
        indefinitely, even when the bind mount is in active use.
        """
        if sys.platform != "darwin":
            self.skipTest(
                "Busy bind mount test is macOS-only (Linux/Windows unmounts don't hang)"
            )

        # Setup: add a bind redirection
        repo_path = f"busy-{impl}"
        self.eden.run_cmd("redirect", "add", "--mount", self.mount, repo_path, "bind")

        # Get the mount point path (inside the checkout)
        mount_point = os.path.join(self.mount, repo_path)
        self.assertTrue(
            os.path.isdir(mount_point), f"Mount point should exist: {mount_point}"
        )

        # Keep the mount busy by holding an open file handle.
        test_file = os.path.join(mount_point, "busy_file")
        with open(test_file, "w") as f:
            f.write("x")
        busy_proc = subprocess.Popen(
            ["tail", "-f", test_file],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

        try:
            # Give the process a moment to start and open the file
            time.sleep(0.5)

            # Verify the process is running and has the file open
            self.assertIsNone(busy_proc.poll(), "busy process should still be running")

            # Run eden rm with a timeout
            start_time = time.time()
            result = self.eden.run_unchecked(
                "remove",
                "--yes",
                "--timeout",
                "1",
                self.mount,
                env=env,
                capture_output=True,
                text=True,
            )
            elapsed_time = time.time() - start_time

            # The operation should complete (not hang indefinitely)
            # On macOS, MNT_FORCE could hang on busy mounts, but --timeout should prevent that (timeout + overhead = ~2s)
            self.assertLess(
                elapsed_time,
                2.0,
                f"eden rm ({impl}) should not hang indefinitely with busy bind mount",
            )

            # Log the output for debugging
            output = result.stderr or ""

            # Verify the mount was removed despite the busy bind mount
            self.assertFalse(
                os.path.exists(self.mount),
                f"mount point should be removed after eden rm ({impl}) with busy bind mount. Output: {output}",
            )
        finally:
            # Clean up the busy process
            busy_proc.terminate()
            try:
                busy_proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                busy_proc.kill()
                busy_proc.wait()

    def test_rust_remove_succeeds_when_configured_client_dir_is_missing(self) -> None:
        mount_path = pathlib.Path(self.mount)
        client_dir = self.eden.client_dir_for_mount(mount_path)
        directory_map = self.load_directory_map()
        original_client_name = directory_map[self.mount]
        missing_client_name = f"{original_client_name}-missing"

        self.assertTrue(
            client_dir.is_dir(),
            f"client dir should exist before re-pointing config entry: {client_dir}",
        )

        directory_map[self.mount] = missing_client_name
        self.write_directory_map(directory_map)

        result = self.eden.run_unchecked(
            "remove",
            "--yes",
            self.mount,
            env=self.rust_env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            0,
            result.returncode,
            f"rust eden remove should succeed when the configured client dir is missing. stderr={result.stderr}",
        )
        self.assertNotIn(
            self.mount,
            self.load_directory_map(),
            "successful remove should unregister the broken config entry",
        )
        self.assertFalse(
            os.path.exists(self.mount),
            "successful remove should delete the checkout path",
        )
        self.assertIsNone(
            self.eden.get_mount_state(mount_path),
            "successful remove should leave the checkout unmounted",
        )

    @parameterized.expand(
        [
            ("python", {"EDENFSCTL_SKIP_RUST": "1"}),
        ]
    )
    def test_remove_succeeds_when_mount_is_not_registered(
        self, impl: str, env: dict
    ) -> None:
        """A clone interrupted between the mount and the directory-map write
        leaves a mount the daemon serves but config.json does not list."""
        mount_path = pathlib.Path(self.mount)
        client_dir = self.eden.client_dir_for_mount(mount_path)
        directory_map = self.load_directory_map()
        del directory_map[self.mount]
        self.write_directory_map(directory_map)
        self.assertIn("(unconfigured)", self.eden.run_cmd("list"))

        result = self.eden.run_unchecked(
            "remove",
            "--yes",
            self.mount,
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            0,
            result.returncode,
            f"eden remove ({impl}) should succeed on an unregistered mount. stderr={result.stderr}",
        )
        self.assertIsNone(
            self.eden.get_mount_state(mount_path),
            "successful remove should leave the checkout unmounted",
        )
        self.assertFalse(
            client_dir.exists(),
            "successful remove should delete the client directory the mount used",
        )
        self.assertFalse(
            os.path.exists(self.mount),
            "successful remove should delete the checkout path",
        )

    def test_remove_without_the_config_dir_of_the_serving_instance(self) -> None:
        """Run without `--config-dir`, `eden rm` classifies the checkout against
        the default instance, which does not list it, and then finds the
        instance that serves it through the checkout's `.eden` links. That
        instance lists the checkout, so it is removed from there whole."""
        mount_path = pathlib.Path(self.mount)
        client_dir = self.eden.client_dir_for_mount(mount_path)
        cmd, env = self.eden.get_edenfsctl_cmd_env(
            "remove", "--yes", self.mount, config_dir=False
        )

        result = subprocess.run(
            cmd,
            env=env | {"EDENFSCTL_SKIP_RUST": "1"},
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            0,
            result.returncode,
            f"eden remove without --config-dir should succeed. stderr={result.stderr}",
        )
        self.assertIsNone(
            self.eden.get_mount_state(mount_path),
            "successful remove should leave the checkout unmounted",
        )
        self.assertNotIn(
            self.mount,
            self.load_directory_map(),
            "successful remove should unregister the checkout from the instance that served it",
        )
        self.assertFalse(
            client_dir.exists(),
            "successful remove should delete the client directory",
        )
        self.assertFalse(
            os.path.exists(self.mount),
            "successful remove should delete the checkout path",
        )

    @parameterized.expand(
        [
            ("python", {"EDENFSCTL_SKIP_RUST": "1"}),
        ]
    )
    def test_remove_of_a_non_checkout_exits_ex_noinput(
        self, impl: str, env: dict
    ) -> None:
        inside = os.path.join(self.mount, "adir")
        for path in [os.path.join(self.tmp_dir, "missing"), inside, str(self.tmp_dir)]:
            result = self.eden.run_unchecked(
                "remove", "--yes", path, env=env, capture_output=True, text=True
            )
            self.assertEqual(
                66,
                result.returncode,
                f"eden remove ({impl}) {path} should exit EX_NOINPUT. stderr={result.stderr}",
            )
        self.assertTrue(os.path.isdir(inside), "the checkout must be untouched")

    @parameterized.expand(
        [
            ("rust", {"EDENFSCTL_ONLY_RUST": "1"}),
            ("python", {"EDENFSCTL_SKIP_RUST": "1"}),
        ]
    )
    def test_remove_succeeds_when_config_toml_is_missing(
        self, impl: str, env: dict
    ) -> None:
        if sys.platform == "win32":
            self.skipTest(
                "On Windows, .eden\\config stays after `eden unmount`, so `eden rm` "
                "takes the checkout for a mounted one and fails to unmount it"
            )
        mount_path = pathlib.Path(self.mount)
        self.eden.unmount(mount_path)
        (self.eden.client_dir_for_mount(mount_path) / "config.toml").unlink()

        result = self.eden.run_unchecked(
            "remove",
            "--yes",
            self.mount,
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            0,
            result.returncode,
            f"{impl} eden remove should succeed when config.toml is missing. stderr={result.stderr}",
        )
        self.assertNotIn(self.mount, self.load_directory_map())
        self.assertFalse(os.path.exists(self.mount))

    @parameterized.expand([("rust", {"EDENFSCTL_ONLY_RUST": "1"})])
    def test_remove_succeeds_when_folder_is_missing(self, impl: str, env: dict) -> None:
        mount_path = pathlib.Path(self.mount)
        client_dir = self.eden.client_dir_for_mount(mount_path)
        self.eden.unmount(mount_path)
        shutil.rmtree(mount_path)

        result = self.eden.run_unchecked(
            "remove",
            "--yes",
            self.mount,
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            0,
            result.returncode,
            f"{impl} eden remove should succeed when the folder is missing. stderr={result.stderr}",
        )
        self.assertNotIn(self.mount, self.load_directory_map())
        self.assertFalse(client_dir.exists())
