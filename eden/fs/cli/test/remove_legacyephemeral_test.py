#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import tempfile
import unittest
from contextlib import contextmanager
from pathlib import Path
from typing import cast, Generator, List
from unittest.mock import patch

from eden.fs.cli import main as main_mod
from eden.fs.cli.config import CheckoutConfig, EdenCheckout, EdenInstance
from eden.fs.cli.doctor.test.lib.fake_eden_instance import FakeEdenInstance

from .lib.output import TestOutput


class RemoveLegacyEphemeralCheckoutsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp_dir = tempfile.mkdtemp(prefix="eden_test.")

    def _add_mock_methods(
        self, instance: FakeEdenInstance
    ) -> tuple[List[str], List[str]]:
        unmount_calls: List[str] = []
        destroy_mount_calls: List[str] = []

        def mock_unmount(path: str, **kwargs: bool) -> None:
            unmount_calls.append(path)

        def mock_destroy_mount(path: str, preserve_mount_point: bool) -> None:
            destroy_mount_calls.append(path)

        def mock_cleanup_mount(
            path: Path, preserve_mount_point: bool, debug: bool
        ) -> None:
            pass

        # pyre-ignore[16]: FakeEdenInstance has no attribute unmount
        instance.unmount = mock_unmount
        # pyre-ignore[16]: FakeEdenInstance has no attribute destroy_mount
        instance.destroy_mount = mock_destroy_mount
        # pyrefly: ignore[missing-attribute]
        instance.cleanup_mount = mock_cleanup_mount

        return unmount_calls, destroy_mount_calls

    def test_no_checkouts(self) -> None:
        """Test that no checkouts returns 0."""
        instance = FakeEdenInstance(self.tmp_dir)
        out = TestOutput()

        result = main_mod.remove_legacyephemeral_checkouts(
            cast(EdenInstance, instance), out
        )

        self.assertEqual(result, 0)
        self.assertEqual(out.getvalue(), "")

    def test_no_legacyephemeral_checkouts(self) -> None:
        """Test that checkouts with other catalog types are not removed."""
        instance = FakeEdenInstance(self.tmp_dir)
        out = TestOutput()

        checkout1 = instance.create_test_mount("mount1")
        checkout2 = instance.create_test_mount("mount2")
        checkout3 = instance.create_test_mount("mount3")

        config1 = checkout1.get_config()
        checkout1.save_config(config1._replace(inode_catalog_type="legacy"))

        config2 = checkout2.get_config()
        checkout2.save_config(config2._replace(inode_catalog_type="legacy"))

        config3 = checkout3.get_config()
        checkout3.save_config(config3._replace(inode_catalog_type=None))

        result = main_mod.remove_legacyephemeral_checkouts(
            cast(EdenInstance, instance), out
        )

        self.assertEqual(result, 0)
        self.assertEqual(out.getvalue(), "")

    def test_single_legacyephemeral_checkout_daemon_not_running(self) -> None:
        """Test removing a single legacyephemeral checkout when daemon is not running."""
        instance = FakeEdenInstance(self.tmp_dir)
        out = TestOutput()

        checkout = instance.create_test_mount("ephemeral_mount", active=False)
        config = checkout.get_config()
        checkout.save_config(config._replace(inode_catalog_type="legacyephemeral"))

        unmount_calls, destroy_mount_calls = self._add_mock_methods(instance)

        result = main_mod.remove_legacyephemeral_checkouts(
            cast(EdenInstance, instance), out
        )

        self.assertEqual(result, 1)
        output = out.getvalue()
        self.assertIn("Found 1 checkout(s) with legacyephemeral", output)
        self.assertIn("ephemeral_mount", output)
        self.assertIn("Removing legacyephemeral checkout", output)
        self.assertIn("Deleting mount", output)
        self.assertIn("  Removed", output)
        self.assertIn("Removed 1 legacyephemeral checkout(s)", output)

        # Verify destroy_mount was called and unmount was not (daemon not running)
        self.assertEqual(len(unmount_calls), 0)
        self.assertEqual(len(destroy_mount_calls), 1)
        self.assertIn("ephemeral_mount", destroy_mount_calls[0])

    def test_single_legacyephemeral_checkout_daemon_running(self) -> None:
        """Test removing a legacyephemeral checkout when daemon is running."""
        instance = FakeEdenInstance(self.tmp_dir)
        out = TestOutput()

        checkout = instance.create_test_mount("ephemeral_mount", active=True)
        config = checkout.get_config()
        checkout.save_config(config._replace(inode_catalog_type="legacyephemeral"))

        unmount_calls, destroy_mount_calls = self._add_mock_methods(instance)

        # Need to mock stop_aux_processes_for_path as well
        with patch.object(main_mod, "stop_aux_processes_for_path"):
            result = main_mod.remove_legacyephemeral_checkouts(
                cast(EdenInstance, instance), out
            )

        self.assertEqual(result, 1)
        output = out.getvalue()
        self.assertIn("Stopping aux processes", output)
        self.assertIn("Unmounting", output)
        self.assertIn("Deleting mount", output)
        self.assertIn("  Removed", output)

        self.assertEqual(len(unmount_calls), 1)
        self.assertEqual(len(destroy_mount_calls), 1)

    def test_multiple_checkouts_mixed_types(self) -> None:
        """Test removing only legacyephemeral checkouts when mixed with others."""
        instance = FakeEdenInstance(self.tmp_dir)
        out = TestOutput()

        checkout1 = instance.create_test_mount("mount1", active=False)
        checkout2 = instance.create_test_mount("mount2", active=False)
        checkout3 = instance.create_test_mount("mount3", active=False)
        checkout4 = instance.create_test_mount("mount4", active=False)

        config1 = checkout1.get_config()
        checkout1.save_config(config1._replace(inode_catalog_type="legacyephemeral"))

        config2 = checkout2.get_config()
        checkout2.save_config(config2._replace(inode_catalog_type="legacy"))

        config3 = checkout3.get_config()
        checkout3.save_config(config3._replace(inode_catalog_type="legacyephemeral"))

        config4 = checkout4.get_config()
        checkout4.save_config(config4._replace(inode_catalog_type="legacy"))

        unmount_calls, destroy_mount_calls = self._add_mock_methods(instance)

        result = main_mod.remove_legacyephemeral_checkouts(
            cast(EdenInstance, instance), out
        )

        self.assertEqual(result, 2)
        output = out.getvalue()
        self.assertIn("Found 2 checkout(s) with legacyephemeral", output)
        self.assertIn("Removed 2 legacyephemeral checkout(s)", output)

        self.assertEqual(len(destroy_mount_calls), 2)

    def test_unmount_failure_continues_with_cleanup(self) -> None:
        """Test that unmount failure doesn't block config cleanup."""
        instance = FakeEdenInstance(self.tmp_dir)
        out = TestOutput()

        checkout = instance.create_test_mount("ephemeral_mount", active=True)
        config = checkout.get_config()
        checkout.save_config(config._replace(inode_catalog_type="legacyephemeral"))

        unmount_calls, destroy_mount_calls = self._add_mock_methods(instance)

        unmount_calls_typed: List[str] = unmount_calls

        def failing_unmount(path: str, **kwargs: bool) -> None:
            unmount_calls_typed.append(path)
            raise Exception("Unmount failed")

        # pyre-ignore[16]
        instance.unmount = failing_unmount

        with patch.object(main_mod, "stop_aux_processes_for_path"):
            result = main_mod.remove_legacyephemeral_checkouts(
                cast(EdenInstance, instance), out
            )

        self.assertEqual(result, 1)
        output = out.getvalue()
        self.assertIn("Warning: Unmount failed", output)
        self.assertIn("Continuing with config cleanup", output)
        self.assertIn("  Removed", output)

        # Verify destroy_mount was still called despite unmount failure
        self.assertEqual(len(unmount_calls), 1)
        self.assertEqual(len(destroy_mount_calls), 1)

    def test_destroy_mount_failure_logs_error(self) -> None:
        """Test that destroy_mount failure is logged but doesn't throw."""
        instance = FakeEdenInstance(self.tmp_dir)
        out = TestOutput()

        checkout = instance.create_test_mount("ephemeral_mount", active=False)
        config = checkout.get_config()
        checkout.save_config(config._replace(inode_catalog_type="legacyephemeral"))

        self._add_mock_methods(instance)

        def failing_destroy_mount(path: str, preserve_mount_point: bool) -> None:
            raise Exception("Cleanup failed")

        # pyre-ignore[16]
        instance.destroy_mount = failing_destroy_mount

        result = main_mod.remove_legacyephemeral_checkouts(
            cast(EdenInstance, instance), out
        )

        self.assertEqual(result, 0)  # Failure doesn't count as removed
        output = out.getvalue()
        self.assertIn("Error deleting configuration", output)
        self.assertIn("Cleanup failed", output)
        self.assertIn("Manual cleanup recommended", output)
        self.assertIn("eden rm", output)

    def test_cleanup_and_destroy_failures_log_both_errors(self) -> None:
        instance = FakeEdenInstance(self.tmp_dir)
        out = TestOutput()

        checkout = instance.create_test_mount("ephemeral_mount", active=False)
        config = checkout.get_config()
        checkout.save_config(config._replace(inode_catalog_type="legacyephemeral"))
        self._add_mock_methods(instance)

        with (
            patch.object(
                instance, "cleanup_mount", side_effect=OSError("Folder cleanup failed")
            ),
            patch.object(
                instance,
                "destroy_mount",
                side_effect=OSError("Configuration deletion failed"),
            ),
        ):
            result = main_mod.remove_legacyephemeral_checkouts(
                cast(EdenInstance, instance), out
            )

        self.assertEqual(result, 0)
        output = out.getvalue()
        self.assertIn("Warning: Failed to clean up mount point", output)
        self.assertIn("Folder cleanup failed", output)
        self.assertIn("Error deleting configuration", output)
        self.assertIn("Configuration deletion failed", output)
        self.assertIn(f"Run: rm -rf {checkout.path}", output)
        self.assertIn(f"Run: eden rm {checkout.path}", output)

    def test_cleanup_mount_failure_still_removes_checkout(self) -> None:
        instance = FakeEdenInstance(self.tmp_dir)
        out = TestOutput()

        checkout = instance.create_test_mount("ephemeral_mount", active=False)
        config = checkout.get_config()
        checkout.save_config(config._replace(inode_catalog_type="legacyephemeral"))

        _unmount_calls, destroy_mount_calls = self._add_mock_methods(instance)

        def destroy_mount(path: str, preserve_mount_point: bool) -> None:
            destroy_mount_calls.append(path)
            instance.remove_checkout_configuration(path)

        def failing_cleanup_mount(
            path: Path, preserve_mount_point: bool, debug: bool
        ) -> None:
            raise Exception("Cleanup failed")

        # pyrefly: ignore[missing-attribute]
        instance.cleanup_mount = failing_cleanup_mount
        # pyrefly: ignore[missing-attribute]
        instance.destroy_mount = destroy_mount

        result = main_mod.remove_legacyephemeral_checkouts(
            cast(EdenInstance, instance), out
        )

        self.assertEqual(result, 1)
        output = out.getvalue()
        self.assertIn("Warning: Failed to clean up mount point", output)
        self.assertIn("Cleanup failed", output)
        self.assertIn("Manual cleanup recommended", output)
        self.assertIn("rm -rf", output)
        self.assertEqual(destroy_mount_calls, [str(checkout.path)])
        self.assertEqual(instance.get_checkouts(), [])
        self.assertFalse((instance.clients_path / "ephemeral_mount").exists())

    def test_get_checkouts_failure_continues(self) -> None:
        """Test that failure to enumerate checkouts doesn't crash."""
        instance = FakeEdenInstance(self.tmp_dir)
        out = TestOutput()

        def failing_get_checkouts() -> List[EdenCheckout]:
            raise Exception("Cannot read checkouts")

        # pyre-ignore[8,16]
        instance.get_checkouts = failing_get_checkouts

        result = main_mod.remove_legacyephemeral_checkouts(
            cast(EdenInstance, instance), out
        )

        self.assertEqual(result, 0)
        output = out.getvalue()
        self.assertIn("Warning: Could not enumerate checkouts", output)
        self.assertIn("Continuing with daemon start", output)

    def test_get_config_failure_skips_checkout(self) -> None:
        """Test that failure to read checkout config skips that checkout."""
        instance = FakeEdenInstance(self.tmp_dir)
        out = TestOutput()

        instance.create_test_mount("mount1", active=False)
        checkout2 = instance.create_test_mount("mount2", active=False)

        config2 = checkout2.get_config()
        checkout2.save_config(config2._replace(inode_catalog_type="legacyephemeral"))

        unmount_calls, destroy_mount_calls = self._add_mock_methods(instance)

        from typing import Callable

        original_get_config: Callable[[EdenCheckout], CheckoutConfig] = (
            EdenCheckout.get_config
        )

        def mock_get_config(self: EdenCheckout) -> CheckoutConfig:
            if "mount1" in str(self.path):
                raise Exception("Cannot read config")
            return original_get_config(self)

        with patch.object(EdenCheckout, "get_config", mock_get_config):
            result = main_mod.remove_legacyephemeral_checkouts(
                cast(EdenInstance, instance), out
            )

        self.assertEqual(result, 1)  # Only mount2 should be removed
        output = out.getvalue()
        self.assertIn("Warning: Could not read config for", output)
        self.assertIn("mount1", output)
        self.assertIn("Skipping this checkout", output)
        self.assertIn("Removed 1 legacyephemeral checkout(s)", output)

    def test_daemon_not_running_exception_handled(self) -> None:
        """Test that EdenNotRunningError is handled gracefully."""
        instance = FakeEdenInstance(self.tmp_dir)
        out = TestOutput()

        checkout = instance.create_test_mount("ephemeral_mount", active=False)
        config = checkout.get_config()
        checkout.save_config(config._replace(inode_catalog_type="legacyephemeral"))

        unmount_calls, destroy_mount_calls = self._add_mock_methods(instance)

        @contextmanager
        def raise_not_running(
            timeout: float | None = None,
        ) -> Generator[None, None, None]:
            raise main_mod.EdenNotRunningError("Daemon not running")
            yield  # Never reached, but needed for generator syntax

        # pyre-ignore[8, 16]
        instance.get_thrift_client = raise_not_running

        result = main_mod.remove_legacyephemeral_checkouts(
            cast(EdenInstance, instance), out
        )

        self.assertEqual(result, 1)
        output = out.getvalue()
        # Should not try to unmount, just cleanup
        self.assertNotIn("Unmounting", output)
        self.assertIn("Deleting mount", output)
        self.assertIn("  Removed", output)
        self.assertEqual(len(unmount_calls), 0)
        self.assertEqual(len(destroy_mount_calls), 1)

    def test_windows_platform_skips_check(self) -> None:
        """Test that the function returns early on Windows platform."""
        instance = FakeEdenInstance(self.tmp_dir)
        out = TestOutput()

        checkout = instance.create_test_mount("ephemeral_mount", active=False)
        config = checkout.get_config()
        checkout.save_config(config._replace(inode_catalog_type="legacyephemeral"))

        with patch("eden.fs.cli.main.sys.platform", "win32"):
            result = main_mod.remove_legacyephemeral_checkouts(
                cast(EdenInstance, instance), out
            )

        # Should return 0 immediately without any processing
        self.assertEqual(result, 0)
        self.assertEqual(out.getvalue(), "")
