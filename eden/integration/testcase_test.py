#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

from __future__ import annotations

import unittest
import warnings
from typing import Any
from unittest import mock

import eden.config
from eden.fs.service.eden.thrift_types import MountInfo, MountState
from parameterized import parameterized

from .lib import edenclient, testcase


class FuseTransportPolicyTest(unittest.TestCase):
    @parameterized.expand(
        [
            ("fallback_then_missing", ("devfuse", None), AssertionError),
            ("missing_then_fallback", (None, "devfuse"), AssertionError),
            ("fallback_then_unknown", ("devfuse", "unknown"), AssertionError),
            ("fallback_only", ("devfuse", "io_uring"), unittest.SkipTest),
        ]
    )
    def test_checks_all_mounts_before_skipping(
        self, name: str, transports: tuple[str | None, ...], error: type[Exception]
    ) -> None:
        mounts = [
            MountInfo(
                mountPoint=f"/mount{index}".encode(),
                edenClientPath=f"/client{index}".encode(),
                state=MountState.RUNNING,
                fsChannelType="fuse",
                fuseTransport=transport,
            )
            for index, transport in enumerate(transports)
        ]
        with self.assertRaises(error):
            edenclient.assert_fuse_transports(mounts, "io_uring")

    def test_matching_transport_passes(self) -> None:
        for transport in ("io_uring", "devfuse"):
            with self.subTest(transport=transport):
                self.assertIsNone(
                    edenclient.assert_fuse_transport(b"/mount", transport, transport)
                )

    def test_devfuse_fallback_skips_io_uring(self) -> None:
        with self.assertRaisesRegex(unittest.SkipTest, "/mount.*devfuse fallback"):
            edenclient.assert_fuse_transport(b"/mount", "io_uring", "devfuse")

    def test_other_mismatches_fail(self) -> None:
        for expected, actual in (
            ("io_uring", None),
            ("io_uring", "unknown"),
            ("devfuse", "io_uring"),
            ("devfuse", None),
        ):
            with self.subTest(expected=expected, actual=actual):
                with self.assertRaisesRegex(AssertionError, "/mount: expected"):
                    edenclient.assert_fuse_transport(b"/mount", expected, actual)


class IoUringTestMixinTest(unittest.TestCase):
    def test_start_skip_still_cleans_up(self) -> None:
        case = testcase.EdenTestCase()
        with (
            mock.patch.object(case, "__unittest_skip__", False, create=True),
            mock.patch.object(case, "init_eden_client") as init_client,
            mock.patch.object(case, "report_time"),
            mock.patch.object(case, "runTest", return_value=None, create=True) as body,
            mock.patch.object(case, "set_rust_rollout_config"),
            mock.patch.object(case, "write_configs"),
        ):
            init_client.return_value.start.side_effect = unittest.SkipTest("fallback")
            result = unittest.TestResult()
            case.run(result)
            self.assertTrue(result.wasSuccessful(), (result.failures, result.errors))
            self.assertEqual([(case, "fallback")], result.skipped)
            body.assert_not_called()
            init_client.return_value.cleanup.assert_called_once_with()

    @parameterized.expand(
        [("none", None), ("empty", {}), ("cached", {"fuse": ["existing = true"]})]
    )
    def test_transport_config(
        self, name: str, extra_config: dict[str, list[str]] | None
    ) -> None:
        original = (
            {section: list(lines) for section, lines in extra_config.items()}
            if extra_config is not None
            else None
        )
        for enabled in (False, True):
            with self.subTest(io_uring=enabled):
                case = testcase.EdenTestCase()
                with (
                    mock.patch.object(case, "__unittest_skip__", False, create=True),
                    mock.patch.object(case, "init_eden_client"),
                    mock.patch.object(case, "report_time"),
                    mock.patch.object(case, "runTest", return_value=None, create=True),
                    mock.patch.object(case, "use_io_uring", return_value=enabled),
                    mock.patch.object(case, "set_rust_rollout_config"),
                    mock.patch.object(
                        case, "edenfs_extra_config", return_value=extra_config
                    ),
                    mock.patch.object(case, "write_configs") as write_configs,
                    mock.patch.object(testcase.sys, "platform", "linux"),
                    mock.patch.object(
                        testcase.os,
                        "uname",
                        return_value=mock.Mock(release="6.13.2-0_fbk7"),
                        create=True,
                    ),
                    warnings.catch_warnings(),
                ):
                    warnings.simplefilter("always", DeprecationWarning)
                    result = unittest.TestResult()
                    case.run(result)
                self.assertTrue(
                    result.wasSuccessful(), (result.failures, result.errors)
                )
                self.assertEqual([], result.skipped)
                self.assertEqual(original, extra_config)
                fuse_config = write_configs.call_args.args[0]["fuse"]
                self.assertEqual(
                    1, fuse_config.count(f"use-io-uring = {str(enabled).lower()}")
                )
                self.assertNotIn(
                    f"use-io-uring = {str(not enabled).lower()}", fuse_config
                )
                if original:
                    self.assertIn("existing = true", fuse_config)
                self.assertEqual(
                    enabled, "io-uring-pre-create-queues = true" in fuse_config
                )

    def test_selects_io_uring(self) -> None:
        self.assertFalse(testcase.EdenTestCase().use_io_uring())
        self.assertTrue(testcase.IoUringTestMixin().use_io_uring())
        self.assertFalse(issubclass(testcase.IoUringTestMixin, unittest.TestCase))

    @parameterized.expand(
        [
            ("6.13.2-0_fbk7", True),
            ("6.16.4-0_fbk1", True),
            ("6.13.2-generic", False),
            ("6.12.0_fbk1", False),
            ("6.17.0_fbk1", False),
        ]
    )
    def test_kernel_gate(self, release: str, supported: bool) -> None:
        class IoUringCase(testcase.IoUringTestMixin, testcase.EdenTestCase):
            pass

        case = IoUringCase()
        original_env = testcase.os.environ.get("EDENFS_INTEGRATION_TEST")
        with (
            mock.patch.object(testcase.sys, "platform", "linux"),
            mock.patch.object(
                testcase.os,
                "uname",
                return_value=mock.Mock(release=release),
                create=True,
            ),
            mock.patch.object(case, "__unittest_skip__", False, create=True),
            mock.patch.object(case, "runTest", return_value=None, create=True),
            mock.patch.object(case, "setup_eden_test") as setup,
        ):
            result = unittest.TestResult()
            case.run(result)
        self.assertTrue(result.wasSuccessful(), (result.failures, result.errors))
        self.assertEqual(
            original_env, testcase.os.environ.get("EDENFS_INTEGRATION_TEST")
        )
        self.assertEqual(not supported, bool(result.skipped))
        if supported:
            setup.assert_called_once()
        else:
            setup.assert_not_called()
            self.assertIn("requires an fbk", result.skipped[0][1])

    @parameterized.expand([("darwin",), ("win32",)])
    def test_non_linux_is_skipped_before_starting_eden(self, platform: str) -> None:
        with (
            mock.patch.object(testcase.sys, "platform", platform),
            mock.patch.object(testcase.EdenTestCase, "setup_eden_test") as setup,
        ):

            class IoUringCase(testcase.IoUringTestMixin, testcase.EdenTestCase):
                pass

            with self.assertRaisesRegex(unittest.SkipTest, "requires an fbk"):
                IoUringCase().setUp()
            setup.assert_not_called()


@mock.patch.object(eden.config, "HAVE_NFS", True)
@mock.patch.object(eden.config, "HAVE_FILTEREDHG", True)
@mock.patch.object(testcase.sys, "platform", "linux")
class RepoTestVariantsTest(unittest.TestCase):
    def variants(self, **kwargs: Any) -> dict[str, type[testcase.EdenRepoTest]]:
        class Scope:
            @testcase.eden_repo_test(**kwargs)
            class Example(testcase.EdenRepoTest):
                def test_example(self) -> None:
                    self.assertIsInstance(self, testcase.EdenRepoTest)

        return {
            name.removeprefix("Example"): cls
            for name, cls in vars(Scope).items()
            if isinstance(cls, type) and issubclass(cls, testcase.EdenRepoTest)
        }

    def test_repository_and_coroutine_variants(self) -> None:
        variants = self.variants()
        self.assertEqual(
            {
                "Hg",
                "FilteredHg",
                "NFSHg",
                "NFSFilteredHg",
                "Coroutines",
                "IoUring",
                "FilteredHgIoUring",
                "CoroutinesIoUring",
            },
            set(variants),
        )
        for label, cls in variants.items():
            with self.subTest(variant=label):
                case = cls()
                self.assertEqual(label.endswith("IoUring"), case.use_io_uring())
                self.assertEqual(label.startswith("NFS"), case.use_nfs())
                self.assertEqual("hg", case.repo_type)
                self.assertEqual(
                    "FilteredHg" in label, case.backing_store_type == "filteredhg"
                )
                self.assertEqual(
                    label.startswith("Coroutines"), bool(case.get_coroutines_configs())
                )

    def test_case_sensitivity_composes_with_io_uring(self) -> None:
        variants = self.variants(case_sensitivity_dependent=True, run_coroutines=False)
        self.assertEqual(18, len(variants))
        for scm in ("Hg", "FilteredHg"):
            for label, sensitive in (
                ("SystemCaseSensitivity", None),
                ("CaseSensitive", True),
                ("CaseInsensitive", False),
            ):
                with self.subTest(scm=scm, sensitivity=label):
                    case = variants[f"{scm}{label}IoUring"]()
                    self.assertIs(sensitive, case.is_case_sensitive)
                    self.assertTrue(case.use_io_uring())
                    self.assertFalse(case.use_nfs())

    def test_opt_out_preserves_baseline(self) -> None:
        self.assertEqual(
            {label for label in self.variants() if not label.endswith("IoUring")},
            set(self.variants(run_io_uring=False)),
        )

    @parameterized.expand(
        [
            ("Hg",),
            ("IoUring",),
            ("FilteredHg",),
            ("FilteredHgIoUring",),
            ("Coroutines",),
            ("CoroutinesIoUring",),
            ("",),
        ]
    )
    def test_method_skips_are_independent(self, suffix: str) -> None:
        with mock.patch.dict(
            testcase.skip.TEST_DISABLED,
            {f"testcase_test.Example{suffix}": ["test_example"]},
        ):
            variants = self.variants()
        for label, cls in variants.items():
            with self.subTest(variant=label):
                self.assertEqual(
                    bool(suffix) and label != suffix,
                    callable(getattr(cls, "test_example", None)),
                )

    @parameterized.expand([("Hg",), ("IoUring",), ("",)])
    def test_class_skips_are_independent(self, suffix: str) -> None:
        expected = set(self.variants()) - {suffix} if suffix else set()
        with mock.patch.dict(
            testcase.skip.TEST_DISABLED, {f"testcase_test.Example{suffix}": True}
        ):
            self.assertEqual(expected, set(self.variants()))

    def test_platform_and_build_gates(self) -> None:
        for platform in ("darwin", "win32"):
            with (
                self.subTest(platform=platform),
                mock.patch.object(testcase.sys, "platform", platform),
            ):
                self.assertEqual(
                    set(self.variants(run_io_uring=False)), set(self.variants())
                )
        with (
            mock.patch.object(eden.config, "HAVE_FILTEREDHG", False),
            mock.patch.object(eden.config, "HAVE_NFS", False),
        ):
            self.assertEqual(
                {"Hg", "IoUring", "Coroutines", "CoroutinesIoUring"},
                set(self.variants()),
            )


@mock.patch.object(eden.config, "HAVE_NFS", True)
@mock.patch.object(testcase.sys, "platform", "linux")
class CustomTestVariantsTest(unittest.TestCase):
    def variants(
        self, decorator: Any, **kwargs: Any
    ) -> dict[str, type[testcase.EdenRepoTest]]:
        class Scope:
            @decorator(**kwargs)
            class Example(testcase.EdenRepoTest):
                repo_type = "custom"

                def test_example(self) -> None:
                    self.assertIsInstance(self, testcase.EdenRepoTest)

        return {
            name.removeprefix("Example"): cls
            for name, cls in vars(Scope).items()
            if isinstance(cls, type) and issubclass(cls, testcase.EdenRepoTest)
        }

    def test_custom_repository_setup_is_preserved(self) -> None:
        variants = self.variants(testcase.eden_nfs_repo_test, run_coroutines=True)
        self.assertEqual(
            {"Default", "NFS", "Coroutines", "DefaultIoUring", "CoroutinesIoUring"},
            set(variants),
        )
        for label, cls in variants.items():
            with self.subTest(variant=label):
                self.assertEqual("custom", cls().repo_type)
                self.assertEqual(label.endswith("IoUring"), cls().use_io_uring())
                self.assertEqual(label == "NFS", cls().use_nfs())

    def test_wal_composes_with_io_uring(self) -> None:
        variants = self.variants(testcase.eden_nfs_repo_test_with_wal_variant)
        self.assertEqual(
            {
                "Default",
                "NFS",
                "DefaultIoUring",
                "DefaultWal",
                "NFSWal",
                "DefaultIoUringWal",
            },
            set(variants),
        )
        case = variants["DefaultIoUringWal"]()
        self.assertTrue(case.use_io_uring())
        self.assertFalse(case.use_nfs())
        self.assertIn("use-wal = true", (case.edenfs_extra_config() or {})["overlay"])

    def test_opt_out_and_platform_gates(self) -> None:
        for decorator in (
            testcase.eden_nfs_repo_test,
            testcase.eden_nfs_repo_test_with_wal_variant,
        ):
            with self.subTest(decorator=decorator):
                baseline = self.variants(decorator, run_io_uring=False)
                self.assertFalse(any("IoUring" in label for label in baseline))
                for platform in ("darwin", "win32"):
                    with mock.patch.object(testcase.sys, "platform", platform):
                        baseline = self.variants(decorator, run_io_uring=False)
                        self.assertEqual(set(baseline), set(self.variants(decorator)))

    def test_plain_tests_require_explicit_opt_in(self) -> None:
        self.assertEqual({"Default"}, set(self.variants(testcase.eden_test)))
        self.assertEqual(
            {"Default", "IoUring"},
            set(self.variants(testcase.eden_test, run_io_uring=True)),
        )

    @parameterized.expand(
        [
            ("Default",),
            ("DefaultIoUring",),
            ("Coroutines",),
            ("CoroutinesIoUring",),
            ("",),
        ]
    )
    def test_method_skips_are_independent(self, suffix: str) -> None:
        with mock.patch.dict(
            testcase.skip.TEST_DISABLED,
            {f"testcase_test.Example{suffix}": ["test_example"]},
        ):
            variants = self.variants(testcase.eden_nfs_repo_test, run_coroutines=True)
        for label, cls in variants.items():
            with self.subTest(variant=label):
                self.assertEqual(
                    bool(suffix) and label != suffix,
                    callable(getattr(cls, "test_example", None)),
                )

    def test_wal_preserves_transport_skip_isolation(self) -> None:
        with mock.patch.dict(
            testcase.skip.TEST_DISABLED,
            {"testcase_test.ExampleDefault": ["test_example"]},
        ):
            variants = self.variants(testcase.eden_nfs_repo_test_with_wal_variant)
        self.assertFalse(callable(getattr(variants["Default"], "test_example", None)))
        for label in ("DefaultIoUring", "DefaultIoUringWal"):
            self.assertTrue(callable(getattr(variants[label], "test_example", None)))
