#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

from __future__ import annotations

import os
import stat
import sys
import tomllib
import unittest
from pathlib import Path
from typing import Any, Iterable, Tuple, Type
from unittest import mock

from eden.integration.lib import edenclient, skip, testcase
from parameterized import parameterized

from . import snapshot as snapshot_mod, verify as verify_mod
from .types.basic import BasicSnapshot


def _replicate_snapshot_test(
    test_class: Type[unittest.TestCase],
) -> Iterable[Tuple[str, Type[unittest.TestCase]]]:
    variants = []

    snapshot_dir = snapshot_mod.get_snapshots_root()
    for snapshot_path in snapshot_dir.iterdir():

        class EdenSnapshot(test_class):
            _snapshot_path = snapshot_path

            def _getSnapshotPath(self) -> Path:
                return self._snapshot_path

        # We don't use Path.stem here since it only strips off the very last suffix,
        # so foo.tar.bz2 becomes foo.tar rather than foo.
        stem = snapshot_path.name.split(".", 1)[0]
        for suffix, variant in testcase._replicate_eden_test(
            EdenSnapshot, run_io_uring=True
        ):
            variants.append((stem if suffix == "Default" else stem + suffix, variant))

    return variants


snapshot_test = testcase.test_replicator(_replicate_snapshot_test)


@snapshot_test
@unittest.skipIf(not edenclient.can_run_eden(), "unable to run edenfs")
class Test(unittest.TestCase):
    """Tests to verify the contents of various saved snapshots.

    All of the test functions in this class are dynamically added by register_tests()
    """

    def _getSnapshotPath(self) -> Path:
        # This is usually implemented by the @snapshot_tests decorator
        raise NotImplementedError("Subclass must implement getSnapshotPath()")

    def use_io_uring(self) -> bool:
        return False

    def test_snapshot(self) -> None:
        if self.use_io_uring():
            edenclient.require_io_uring_kernel()
        with snapshot_mod.create_tmp_dir() as tmp_dir:
            snapshot = snapshot_mod.unpack_into(self._getSnapshotPath(), tmp_dir)
            self._run_test(snapshot)

    def _run_test(self, snapshot: snapshot_mod.BaseSnapshot) -> None:
        verifier = verify_mod.SnapshotVerifier()
        snapshot.verify(verifier, use_io_uring=self.use_io_uring())

        # Fail the test if any errors were found.
        # The individual errors will have been printed out previously
        # as they were found.
        if verifier.errors:
            self.fail(f"found {len(verifier.errors)} errors")


@testcase.eden_test
class InfraTests(unittest.TestCase):
    """Tests for the snapshot generation/verification code itself."""

    def _snapshot_variants(self) -> dict[str, Any]:
        class Scope:
            @snapshot_test
            class Example(unittest.TestCase):
                def use_io_uring(self) -> bool:
                    return False

                def test_example(self) -> None:
                    pass

        return {name: cls for name, cls in vars(Scope).items() if isinstance(cls, type)}

    @parameterized.expand([("linux",), ("darwin",), ("win32",)])
    def test_snapshot_variants_preserve_archive_and_transport(
        self, platform: str
    ) -> None:
        paths = [Path("first.tar.bz2"), Path("second.tar.bz2")]
        with (
            mock.patch.object(sys, "platform", platform),
            mock.patch.object(Path, "iterdir", return_value=iter(paths)),
            mock.patch.dict(skip.TEST_DISABLED, {}, clear=True),
        ):
            variants = self._snapshot_variants()
        expected = {
            f"Example{path.name.split('.', 1)[0]}{suffix}": (path, suffix == "IoUring")
            for path in paths
            for suffix in (("", "IoUring") if platform == "linux" else ("",))
        }
        self.assertEqual(set(expected), set(variants))
        self.assertEqual(len(variants), len(set(variants.values())))
        for name, cls in variants.items():
            with self.subTest(variant=name):
                case = cls()
                self.assertEqual(
                    expected[name], (case._getSnapshotPath(), case.use_io_uring())
                )

    def test_snapshot_variant_skip_does_not_leak_to_io_uring(self) -> None:
        with (
            mock.patch.object(sys, "platform", "linux"),
            mock.patch.object(
                Path, "iterdir", return_value=iter([Path("first.tar.bz2")])
            ),
            mock.patch.dict(
                skip.TEST_DISABLED,
                {"snapshot.test_snapshots.Examplefirst": ["test_example"]},
                clear=True,
            ),
        ):
            variants = self._snapshot_variants()
        self.assertFalse(
            callable(getattr(variants["Examplefirst"], "test_example", None))
        )
        self.assertTrue(callable(variants["ExamplefirstIoUring"].test_example))

    @parameterized.expand([("tools", None), ("devfuse", False), ("io_uring", True)])
    def test_snapshot_transport_config(
        self, name: str, use_io_uring: bool | None
    ) -> None:
        original = """\
[other]
MixedCase = "100%=value"
values = [
    "one",
    "two",
]
[fuse]
max-background-requests = 17
"""
        with snapshot_mod.create_tmp_dir() as tmp_dir:
            snapshot = BasicSnapshot(tmp_dir)
            snapshot.create_transient_dir()
            config_path = snapshot.etc_eden_dir / "edenfs.rc"
            config_path.write_text(original)
            with mock.patch.object(edenclient, "require_io_uring_kernel"):
                eden = snapshot.edenfs(use_io_uring=use_io_uring)
                first_contents = config_path.read_text()
                snapshot.edenfs(use_io_uring=use_io_uring)
            contents = config_path.read_text()
        config = tomllib.loads(contents)
        self.assertEqual(first_contents, contents)
        self.assertEqual(tomllib.loads(original)["other"], config["other"])
        self.assertEqual(17, config["fuse"]["max-background-requests"])
        if use_io_uring is None:
            self.assertEqual(original, contents)
            self.assertIsNone(eden.expected_fuse_transport)
        else:
            self.assertEqual(name, eden.expected_fuse_transport)
            self.assertEqual(use_io_uring, config["fuse"]["use-io-uring"])
            if use_io_uring:
                self.assertTrue(config["fuse"]["io-uring-pre-create-queues"])
                self.assertEqual(
                    edenclient.FUSE_IO_URING_KERNEL_RELEASE_REGEX,
                    config["fuse"]["io-uring-kernel-release-regex"],
                )

    @parameterized.expand([(False,), (True,)])
    def test_snapshot_transport_config_without_existing_file(
        self, use_io_uring: bool
    ) -> None:
        with snapshot_mod.create_tmp_dir() as tmp_dir:
            snapshot = BasicSnapshot(tmp_dir)
            snapshot.create_transient_dir()
            with mock.patch.object(edenclient, "require_io_uring_kernel"):
                eden = snapshot.edenfs(use_io_uring=use_io_uring)
            config = tomllib.loads(eden.system_rc_path.read_text())
        self.assertEqual(use_io_uring, config["fuse"]["use-io-uring"])

    def test_snapshot_unsupported_kernel_skips_before_client_creation(self) -> None:
        with (
            mock.patch.object(
                edenclient, "require_io_uring_kernel", side_effect=unittest.SkipTest
            ),
            mock.patch.object(edenclient, "EdenFS") as client,
        ):
            with self.assertRaises(unittest.SkipTest):
                BasicSnapshot(Path("unused")).edenfs(use_io_uring=True)
        client.assert_not_called()

    def test_verify_directory(self) -> None:
        expected = verify_mod.ExpectedFileSet()
        expected.add_file("a/b/normal.txt", b"abc\n", 0o644)
        expected.add_file("a/b/normal_exe.exe", b"abc\n", 0o755)
        expected.add_file("a/b/missing.txt", b"abc\n", 0o644)
        expected.add_file("a/b/wrong_perms.txt", b"abc\n", 0o644)
        expected.add_file("a/b/wrong_file_type.txt", b"abc\n", 0o644)
        expected.add_socket("a/normal.sock", 0o644)
        expected.add_socket("a/exe.sock", 0o755)
        expected.add_symlink("a/normal.link", b"symlink contents", 0o777)
        expected.add_symlink("a/missing.link", b"missing symlink", 0o777)

        # Define a subclass of HgSnapshot.  We use define this solely so we can use its
        # helper write_file(), make_socket(), and mkdir() methods
        class MockSnapshot(snapshot_mod.HgSnapshot):
            def populate_backing_repo(self) -> None:
                pass

            def populate_checkout(self) -> None:
                pass

            def verify_snapshot_data(
                self, verifier: verify_mod.SnapshotVerifier, eden: edenclient.EdenFS
            ) -> None:
                pass

        with snapshot_mod.create_tmp_dir() as tmp_dir:
            snapshot = MockSnapshot(tmp_dir)
            snapshot.data_dir.mkdir()
            snapshot.checkout_path.mkdir()
            snapshot.write_file("a/b/normal.txt", b"abc\n", 0o644)
            snapshot.write_file("a/b/normal_exe.exe", b"abc\n", 0o755)
            snapshot.write_file("a/b/wrong_perms.txt", b"abc\n", 0o755)
            snapshot.make_socket("a/b/wrong_file_type.txt", 0o755)
            snapshot.make_socket("a/normal.sock", 0o644)
            snapshot.make_socket("a/exe.sock", 0o755)
            os.symlink(b"symlink contents", snapshot.checkout_path / "a/normal.link")
            # The verifier code only checks files, not directories, so it should not
            # complain about extra directories that may be present.
            snapshot.mkdir("a/b/c/extra_dir", 0o755)

            verifier = verify_mod.SnapshotVerifier()
            verifier.verify_directory(
                snapshot.checkout_path, expected, [snapshot.checkout_scm_dir]
            )

        expected_errors = [
            "a/b/missing.txt: file not present in snapshot",
            "a/missing.link: file not present in snapshot",
            f"a/b/wrong_file_type.txt: expected file type to be {stat.S_IFREG:#o}, "
            f"found {stat.S_IFSOCK:#o}",
            "a/b/wrong_file_type.txt: expected permissions to be 0o644, found 0o755",
            "a/b/wrong_perms.txt: expected permissions to be 0o644, found 0o755",
        ]
        self.assertEqual(sorted(verifier.errors), sorted(expected_errors))
