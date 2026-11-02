#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import os
import pathlib
import subprocess
from pathlib import Path
from typing import Tuple

from eden.test_support.temporary_directory import TemporaryDirectoryMixin

from .lib import edenclient, overlay as overlay_mod, repobase, testcase
from .lib.hgrepo import HgRepository


@testcase.eden_nfs_repo_test_with_wal_variant
class FsckTest(testcase.EdenRepoTest):
    # pyre-fixme[13]: Attribute `overlay` is never initialized.
    overlay: overlay_mod.OverlayStore

    def populate_repo(self) -> None:
        self.repo.write_file("README.md", "tbd\n")
        self.repo.write_file("proj/src/main.c", "int main() { return 0; }\n")
        self.repo.write_file("proj/src/lib.c", "void foo() {}\n")
        self.repo.write_file("proj/src/include/lib.h", "#pragma once\nvoid foo();\n")
        self.repo.write_file(
            "proj/test/test.sh", "#!/bin/bash\necho test\n", mode=0o755
        )
        self.repo.write_file("doc/foo.txt", "foo\n")
        self.repo.write_file("doc/bar.txt", "bar\n")
        self.repo.symlink("proj/doc", "../doc")
        self.repo.commit("Initial commit.")

    def create_repo(self, name: str) -> repobase.Repository:
        return self.create_hg_repo("main")

    def create_eden_repo(self) -> HgRepository:
        return HgRepository(
            self.mount,
            system_hgrc=self.system_hgrc,
            filtered=self.backing_store_type == "filteredhg",
        )

    def setup_eden_test(self) -> None:
        super().setup_eden_test()
        self.overlay = overlay_mod.OverlayStore(self.eden, self.mount_path)

    def run_fsck(self, *args: str) -> Tuple[int, str]:
        """Run `eden fsck [args]` and return a tuple of the return code and
        the combined stdout and stderr.

        The command output will be decoded as UTF-8 and returned as a string.
        """
        cmd_result = self.eden.run_unchecked(
            "fsck", *args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT
        )
        fsck_out = cmd_result.stdout.decode("utf-8", errors="replace")
        return (cmd_result.returncode, fsck_out)

    def test_fsck_force_and_check_only(self) -> None:
        """Test the behavior of the --force and --check-only fsck flags."""
        foo_overlay_path = self.overlay.materialize_file(pathlib.Path("doc/foo.txt"))

        # Running fsck with the mount still mounted should fail
        returncode, fsck_out = self.run_fsck(self.mount)
        self.assertIn(f"failed to acquire overlay lock on {self.eden_dir}", fsck_out)

        # Running fsck with --force should override that
        returncode, fsck_out = self.run_fsck(self.mount, "--force")
        self.assertIn("Overlay was shut down uncleanly", fsck_out)
        self.assertIn(f"Starting fsck scan on overlay {self.eden_dir}", fsck_out)
        self.assertIn("completed checking for errors", fsck_out)

        # fsck should perform the check normally without --force
        # if the mount is not mounted
        self.eden.run_cmd("unmount", self.mount)
        returncode, fsck_out = self.run_fsck(self.mount)
        self.assertIn(f"Checking {self.mount}", fsck_out)
        self.assertIn("no problems found", fsck_out)

        # Truncate the overlay file for doc/foo.txt to 0 length
        with foo_overlay_path.open("wb"):
            pass

        # Running fsck with --check-only should report the error but not try to fix it.
        returncode, fsck_out = self.run_fsck("--check-only")
        self.assertIn(f"Checking {self.mount}", fsck_out)
        self.assertIn(
            "file was too short to contain overlay header: read 0 bytes", fsck_out
        )
        self.assertRegex(fsck_out, r"found 1 problem")

        # Running fsck with --check-only a second time should still report the error
        returncode, fsck_out = self.run_fsck("--check-only")
        self.assertIn(f"Checking {self.mount}", fsck_out)
        self.assertIn(
            "file was too short to contain overlay header: read 0 bytes", fsck_out
        )
        self.assertRegex(fsck_out, r"found 1 problem")

        # Running fsck with no arguments should attempt to fix the errors
        returncode, fsck_out = self.run_fsck()

        self.assertIn(
            "file was too short to contain overlay header: read 0 bytes", fsck_out
        )
        self.assertIn("successfully repaired all 1 problems", fsck_out)

        # There should be no more errors if we run fsck again
        returncode, fsck_out = self.run_fsck()
        self.assertIn(f"Checking {self.mount}", fsck_out)
        self.assertIn("no problems found", fsck_out)

    def test_fsck_multiple_mounts(self) -> None:
        mount2 = Path(self.mounts_dir) / "second_mount"
        mount3 = Path(self.mounts_dir) / "third_mount"
        mount4 = Path(self.mounts_dir) / "fourth_mount"

        self.eden.clone(self.repo.path, mount2)
        self.eden.clone(self.repo.path, mount3)
        self.eden.clone(self.repo.path, mount4)

        # Unmount all but mount3
        self.eden.unmount(Path(self.mount))
        self.eden.unmount(mount2)
        self.eden.unmount(mount4)

        # Running fsck should check all but mount3
        returncode, fsck_out = self.run_fsck()
        self.assertIn(
            f"fsck:{self.eden_dir}/clients/main/local: completed checking for errors, no problems found",
            fsck_out,
        )
        self.assertIn(
            f"fsck:{self.eden_dir}/clients/second_mount/local: completed checking for errors, no problems found",
            fsck_out,
        )
        self.assertIn(
            f"failed to acquire overlay lock on {self.eden_dir}/clients/third_mount/local/info",
            fsck_out,
        )
        self.assertIn(
            f"fsck:{self.eden_dir}/clients/fourth_mount/local: completed checking for errors, no problems found",
            fsck_out,
        )

        # Running fsck with --force should check everything
        returncode, fsck_out = self.run_fsck("--force")
        self.assertIn(
            f"fsck:{self.eden_dir}/clients/main/local: completed checking for errors, no problems found",
            fsck_out,
        )
        self.assertIn(
            f"fsck:{self.eden_dir}/clients/second_mount/local: completed checking for errors, no problems found",
            fsck_out,
        )
        self.assertIn(
            f"fsck:{self.eden_dir}/clients/third_mount/local: completed checking for errors, no problems found",
            fsck_out,
        )
        self.assertIn(
            f"fsck:{self.eden_dir}/clients/fourth_mount/local: completed checking for errors, no problems found",
            fsck_out,
        )

    def test_fsck_ignores_configured_error_discovery_threads(self) -> None:
        """`eden fsck` reads fsck.num-error-discovery-threads from the config
        but spawns eden_fsck with the command-line default instead."""
        self.write_configs(
            {"fsck": ['num-error-discovery-threads = "16"']},
            os.path.join(self.home_dir, ".edenrc"),
        )
        fake_bin = Path(self.tmp_dir) / "fake_fsck_bin"
        fake_bin.mkdir()
        argv_file = fake_bin / "argv"
        fake_fsck = fake_bin / "eden_fsck"
        fake_fsck.write_text(f'#!/bin/sh\nprintf "%s\\n" "$@" > "{argv_file}"\n')
        fake_fsck.chmod(0o755)

        self.eden.run_cmd("unmount", self.mount)
        cmd_result = self.eden.run_unchecked(
            "fsck",
            self.mount,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            env={"EDENFS_FSCK": str(fake_fsck)},
        )
        self.assertEqual(0, cmd_result.returncode, cmd_result.stdout)
        argv = argv_file.read_text().splitlines()
        self.assertIn("--num_error_discovery_threads=4", argv)
        self.assertNotIn("--num_error_discovery_threads=16", argv)


@testcase.eden_test
class FsckTestNoEdenfs(testcase.IntegrationTestCase, TemporaryDirectoryMixin):
    def test_fsck_no_checkouts(self) -> None:
        tmp_dir = self.make_temporary_directory()
        eden = edenclient.EdenFS(Path(tmp_dir))
        cmd_result = eden.run_unchecked(
            "fsck",
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            encoding="utf-8",
            errors="replace",
        )
        self.assertIn(
            "No EdenFS checkouts are configured.  Nothing to check.", cmd_result.stderr
        )
        self.assertEqual("", cmd_result.stdout)
        self.assertEqual(0, cmd_result.returncode)
