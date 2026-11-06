#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict


import os
from pathlib import Path

from eden.fs.cli.config import CheckoutConfig, EdenCheckout, EdenInstance

from .lib import testcase


@testcase.eden_repo_test
class LegacyEphemeralCleanupTest(testcase.EdenRepoTest):
    def populate_repo(self) -> None:
        self.repo.write_file("hello.txt", "hello world\n")
        self.repo.commit("Initial commit.")

    def _create_legacyephemeral_checkout(self, name: str) -> Path:
        """Create a fake checkout with legacyephemeral catalog type.

        This simulates a checkout that was created with legacyephemeral
        catalog type and then the daemon crashed/was killed.
        """
        instance = EdenInstance(
            str(self.eden.eden_dir), etc_eden_dir=None, home_dir=None
        )

        mount_path = self.tmp_dir / name
        mount_path.mkdir()

        client_name = name.replace("/", "_")
        client_dir = instance._get_clients_dir() / client_name
        client_dir.mkdir(parents=True)

        config = CheckoutConfig(
            backing_repo=Path(self.repo.path),
            scm_type="hg",
            guid=f"test-{name}",
            mount_protocol="fuse",
            case_sensitive=False,
            require_utf8_path=True,
            default_revision=self.repo.get_head_hash(),
            redirections={},
            redirection_targets={},
            active_prefetch_profiles=[],
            use_write_back_cache=False,
            inode_catalog_type="legacyephemeral",
            off_mount_repo_dir=False,
        )

        checkout = EdenCheckout(instance, mount_path, client_dir)
        checkout.save_config(config)
        checkout.save_snapshot(self.repo.get_head_hash().encode())

        instance._add_path_to_directory_map(mount_path, client_name)

        return mount_path

    def test_start_removes_legacyephemeral_but_preserves_normal_checkouts(self) -> None:
        """Test that 'eden start' removes legacyephemeral checkouts while preserving normal ones."""
        self.eden.shutdown()

        ephemeral_mount = self._create_legacyephemeral_checkout("ephemeral_test")

        instance = EdenInstance(
            str(self.eden.eden_dir), etc_eden_dir=None, home_dir=None
        )
        checkouts_before = instance.get_checkouts()
        checkout_paths_before = {str(c.path) for c in checkouts_before}
        self.assertIn(str(ephemeral_mount), checkout_paths_before)

        normal_checkouts_before = checkout_paths_before - {str(ephemeral_mount)}
        self.assertGreater(
            len(normal_checkouts_before), 0, "Should have at least one normal checkout"
        )

        self.eden.start()

        checkouts_after = instance.get_checkouts()
        checkout_paths_after = {str(c.path) for c in checkouts_after}
        self.assertNotIn(
            str(ephemeral_mount),
            checkout_paths_after,
            "legacyephemeral checkout should have been removed",
        )

        self.assertEqual(
            checkout_paths_after,
            normal_checkouts_before,
            "Normal checkouts should be preserved",
        )

        instance_refreshed = EdenInstance(
            str(self.eden.eden_dir), etc_eden_dir=None, home_dir=None
        )
        client_dirs = os.listdir(instance_refreshed._get_clients_dir())
        self.assertNotIn(
            "ephemeral_test",
            client_dirs,
            "legacyephemeral client directory should have been removed",
        )

    def test_restart_removes_legacyephemeral_checkout(self) -> None:
        """Test that 'eden restart' removes legacyephemeral checkouts."""
        if not self.eden.is_healthy():
            self.eden.start()

        ephemeral_mount = self._create_legacyephemeral_checkout("ephemeral_restart")

        instance = EdenInstance(
            str(self.eden.eden_dir), etc_eden_dir=None, home_dir=None
        )
        checkouts_before = instance.get_checkouts()
        checkout_paths_before = {str(c.path) for c in checkouts_before}
        self.assertIn(str(ephemeral_mount), checkout_paths_before)

        self.eden.restart()

        checkouts_after = instance.get_checkouts()
        checkout_paths_after = {str(c.path) for c in checkouts_after}
        self.assertNotIn(
            str(ephemeral_mount),
            checkout_paths_after,
            "legacyephemeral checkout should have been removed",
        )

    def test_multiple_legacyephemeral_checkouts_removed(self) -> None:
        """Test that multiple legacyephemeral checkouts are all removed."""
        self.eden.shutdown()

        ephemeral1 = self._create_legacyephemeral_checkout("ephemeral1")
        ephemeral2 = self._create_legacyephemeral_checkout("ephemeral2")
        ephemeral3 = self._create_legacyephemeral_checkout("ephemeral3")

        instance = EdenInstance(
            str(self.eden.eden_dir), etc_eden_dir=None, home_dir=None
        )
        checkouts_before = instance.get_checkouts()
        checkout_paths_before = {str(c.path) for c in checkouts_before}
        self.assertIn(str(ephemeral1), checkout_paths_before)
        self.assertIn(str(ephemeral2), checkout_paths_before)
        self.assertIn(str(ephemeral3), checkout_paths_before)

        self.eden.start()

        checkouts_after = instance.get_checkouts()
        checkout_paths_after = {str(c.path) for c in checkouts_after}
        self.assertNotIn(str(ephemeral1), checkout_paths_after)
        self.assertNotIn(str(ephemeral2), checkout_paths_after)
        self.assertNotIn(str(ephemeral3), checkout_paths_after)
