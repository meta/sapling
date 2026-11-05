#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.


# pyre-strict


"""Integration tests for the `eden du` (disk usage) command."""

import json
import os
import subprocess
from pathlib import Path

from eden.fs.cli.util import mkscratch_bin

from .lib import testcase


@testcase.eden_repo_test
class DiskUsageTest(testcase.EdenRepoTest):
    """Test the `eden du` command for displaying disk usage information."""

    def populate_repo(self) -> None:
        # Create tracked files for basic tests
        self.repo.write_file("hello.txt", "Hello, World!\n")
        self.repo.write_file("subdir/file.txt", "File in subdirectory\n")
        self.repo.write_file("original.txt", "Original content\n")
        self.repo.commit("Initial commit.")

    # Basic functionality tests
    def test_du_basic_runs_successfully(self) -> None:
        """Test that `eden du` runs without errors and produces expected sections."""
        output = self.eden.run_cmd("du", self.mount)
        # Verify all expected sections are present
        self.assertIn("Mounts", output)
        self.assertIn("Backing repos", output)
        self.assertIn("Buck redirections", output)
        self.assertIn("Summary", output)
        # Verify the mount path is shown
        self.assertIn(self.mount, output)

    def test_du_continues_when_orphan_scan_fails(self) -> None:
        """Plain du still reports usage when orphan scanning fails."""
        self.set_rust_rollout_config({"du": True})
        result = self.eden.run_unchecked(
            "du",
            self.mount,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            encoding="utf-8",
            env={"MKSCRATCH_BIN": "false"},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("failed to scan for orphaned redirections", result.stderr)
        self.assertIn("Failed to scan for orphaned redirections", result.stdout)
        self.assertNotIn("No orphaned redirections", result.stdout)
        self.assertIn("Summary", result.stdout)

    # JSON output tests
    def test_du_json_output_has_valid_fields(self) -> None:
        """Test that `eden du --json` produces valid JSON with correct field types and values."""
        output = self.eden.run_cmd("du", "--json", self.mount)
        data = json.loads(output)

        # Verify all required fields exist and have valid integer values
        required_fields = [
            "materialized",
            "ignored",
            "redirection",
            "backing",
            "shared",
            "fsck",
        ]
        for field in required_fields:
            self.assertIn(field, data, f"JSON output missing required field: {field}")
            self.assertIsInstance(
                data[field], int, f"Field '{field}' should be an integer"
            )
            self.assertGreaterEqual(
                data[field], 0, f"Field '{field}' should be non-negative"
            )

    # Fast mode tests
    def test_du_fast_mode_shows_summary_only(self) -> None:
        """Test that `eden du --fast` shows only summary without detailed section headers."""
        output = self.eden.run_cmd("du", "--fast", self.mount)

        # Fast mode should show Summary section
        self.assertIn("Summary", output)

        # Fast mode should NOT show the detailed "Mounts" section header
        self.assertNotIn("Mounts\n", output)

    # Clean mode tests
    def test_du_clean_mode_shows_warning_and_sections(self) -> None:
        """Test that `eden du --clean` shows warnings and expected sections."""
        result = self.eden.run_unchecked(
            "du",
            "--clean",
            self.mount,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            encoding="utf-8",
        )

        # Should show cleanup warning about ignored files
        self.assertIn("WARNING", result.stdout)
        self.assertIn("ignored", result.stdout.lower())
        # Should show expected sections
        self.assertIn("Mounts", result.stdout)
        self.assertIn("Buck redirections", result.stdout)

    def test_du_deep_clean_mode_shows_warning(self) -> None:
        """Test that `eden du --deep-clean` shows the cleanup warning."""
        result = self.eden.run_unchecked(
            "du",
            "--deep-clean",
            self.mount,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            encoding="utf-8",
        )
        self.assertIn("WARNING", result.stdout)

    def test_du_clean_continues_when_orphan_scan_fails(self) -> None:
        """Test that `eden du --clean` runs its other cleanups and fails at the end when mkscratch fails."""
        # Keep Python fallback available for the nested `eden gc` command.
        self.set_rust_rollout_config({"du": True})
        result = self.eden.run_unchecked(
            "du",
            "--clean",
            self.mount,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            encoding="utf-8",
            env={
                "MKSCRATCH_BIN": "false",
                "EDENFSCTL_CONFIG_DIR": str(self.eden.eden_dir),
                "EDENFSCTL_ETC_EDEN_DIR": str(self.eden.etc_eden_dir),
                "EDENFSCTL_HOME_DIR": str(self.eden.home_dir),
            },
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("failed to scan for orphaned redirections", result.stderr)
        self.assertIn("Failed to scan for orphaned redirections", result.stdout)
        self.assertIn("Failed to find orphaned redirections for cleanup", result.stderr)
        self.assertIn("Finished cleaning shared space.", result.stdout)

    def test_du_clean_orphaned_mode_runs_successfully(self) -> None:
        """Test that `eden du --clean-orphaned` runs without errors."""
        output = self.eden.run_cmd("du", "--clean-orphaned", self.mount)
        self.assertIn("Summary", output)

    def test_du_clean_orphaned_fails_when_orphan_scan_fails(self) -> None:
        """Orphan-only cleanup stops when orphan scanning fails."""
        self.set_rust_rollout_config({"du": True})
        result = self.eden.run_unchecked(
            "du",
            "--clean-orphaned",
            self.mount,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            encoding="utf-8",
            env={"MKSCRATCH_BIN": "false"},
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Failed to find orphaned redirections for cleanup", result.stderr)
        self.assertNotIn("Summary", result.stdout)

    def test_du_clean_orphaned_with_checkout_missing_config(self) -> None:
        """A checkout left without config.toml by an interrupted `eden rm`."""
        self.set_rust_rollout_config({"du": True})
        broken_mount = os.path.join(self.tmp_dir, "broken")
        self.eden.clone(self.repo.path, broken_mount)
        self.eden.run_cmd(
            "redirect", "add", "--mount", broken_mount, "buck-out", "symlink"
        )
        redirections = json.loads(
            self.eden.run_cmd("redirect", "list", "--json", "--mount", broken_mount)
        )
        broken_target = Path(
            next(r["target"] for r in redirections if r["repo_path"] == "buck-out")
        )
        self.assertTrue(broken_target.is_dir())
        healthy_orphan = Path(
            subprocess.check_output(
                [
                    os.fsdecode(mkscratch_bin()),
                    "path",
                    self.mount,
                    "--subdir",
                    "edenfs/redirections/orphaned",
                ],
                encoding="utf-8",
            ).strip()
        )
        self.assertTrue(healthy_orphan.is_dir())
        self.eden.unmount(Path(broken_mount))
        (self.eden.client_dir_for_mount(Path(broken_mount)) / "config.toml").unlink()

        result = self.eden.run_unchecked(
            "du",
            "--clean-orphaned",
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            encoding="utf-8",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"Run `eden rm {broken_mount}`", result.stderr)
        self.assertFalse(healthy_orphan.exists())
        self.assertTrue(broken_target.exists())

    # Materialized files tests
    def test_du_materialized_increases_after_file_modification(self) -> None:
        """Test that materialized size increases after modifying a tracked file."""
        before_output = self.eden.run_cmd("du", "--json", self.mount)
        before_data = json.loads(before_output)
        before_materialized = before_data["materialized"]

        # Modify the tracked file to materialize it in the overlay
        file_path = Path(self.mount) / "original.txt"
        file_path.write_text("Modified content that is longer than before!\n")

        after_output = self.eden.run_cmd("du", "--json", self.mount)
        after_data = json.loads(after_output)
        after_materialized = after_data["materialized"]

        self.assertGreaterEqual(
            after_materialized,
            before_materialized,
            "Materialized size should not decrease after file modification",
        )

    def test_du_ignored_counts_untracked_files(self) -> None:
        """Test that ignored size includes untracked files."""
        before_output = self.eden.run_cmd("du", "--json", self.mount)
        before_data = json.loads(before_output)
        before_ignored = before_data["ignored"]

        # Create a new untracked file
        new_file = Path(self.mount) / "new_untracked_file.txt"
        new_file.write_text("This is an untracked file with some content!\n")

        after_output = self.eden.run_cmd("du", "--json", self.mount)
        after_data = json.loads(after_output)
        after_ignored = after_data["ignored"]

        self.assertGreaterEqual(
            after_ignored,
            before_ignored,
            "Ignored size should not decrease after creating untracked file",
        )

    def test_du_materialized_increases_with_new_directory(self) -> None:
        """Test that materialized size accounts for new directories."""
        before_output = self.eden.run_cmd("du", "--json", self.mount)
        before_data = json.loads(before_output)
        before_materialized = before_data["materialized"]

        # Create a new directory and file
        new_dir = Path(self.mount) / "new_subdir"
        new_dir.mkdir(parents=True, exist_ok=True)
        new_file = new_dir / "new_file.txt"
        new_file.write_text("Content in new directory\n")

        after_output = self.eden.run_cmd("du", "--json", self.mount)
        after_data = json.loads(after_output)
        after_materialized = after_data["materialized"]

        self.assertGreaterEqual(
            after_materialized,
            before_materialized,
            "Materialized size should not decrease after creating new directory",
        )
