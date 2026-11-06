#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

from __future__ import annotations

import os
import sys
import unittest
from typing import Any, Callable, cast, Dict, Type
from unittest import mock

import eden.config
from eden.integration.lib import hgrepo, skip, testcase
from parameterized import parameterized

from .hg_extension_test_base import (
    EdenHgTestCase,
    filteredhg_test,
    FilteredHgTestCase,
    hg_cached_status_test,
    hg_test,
)


@hg_test
# pyre-ignore[13]: T62487924
class HgExtensionTestBaseTest(EdenHgTestCase):
    """Test to make sure that HgExtensionTestBase creates Eden mounts that are
    properly configured with the Hg extension.
    """

    def populate_backing_repo(self, repo: hgrepo.HgRepository) -> None:
        repo.write_file("hello.txt", "hola")
        repo.commit("Initial commit.")

    def test_setup(self) -> None:
        hg_dir = os.path.join(self.mount, ".hg")
        self.assertTrue(os.path.isdir(hg_dir))

        eden_extension = self.hg("config", "extensions.eden", check=False).rstrip()
        self.assertEqual("", eden_extension)

        self.assertTrue(os.path.isfile(self.get_path("hello.txt")))


@mock.patch.object(eden.config, "HAVE_NFS", True)
@mock.patch.object(eden.config, "HAVE_FILTEREDHG", True)
@mock.patch.object(sys, "platform", "linux")
class HgTestVariantsTest(unittest.TestCase):
    def _variants(
        self,
        decorator: Callable[..., Any],
        base: Type[EdenHgTestCase] = EdenHgTestCase,
        **kwargs: Any,
    ) -> Dict[str, Type[EdenHgTestCase]]:
        class Scope:
            @decorator(**kwargs)
            class Example(base):
                def populate_backing_repo(self, repo: hgrepo.HgRepository) -> None:
                    pass

                def test_example(self) -> None:
                    self.assertIsInstance(self, EdenHgTestCase)

        return {
            name.removeprefix("Example"): cast(Type[EdenHgTestCase], cls)
            for name, cls in vars(Scope).items()
            if isinstance(cls, type) and issubclass(cls, EdenHgTestCase)
        }

    def test_hg_variants_preserve_existing_combinations(self) -> None:
        variants = self._variants(hg_test)
        self.assertEqual(
            {
                "TreeOnly",
                "TreeOnlyIoUring",
                "TreeOnlyFilteredHg",
                "TreeOnlyFilteredHgIoUring",
                "TreeOnlyNFS",
                "TreeOnlyNFSFilteredHg",
                "Coroutines",
                "CoroutinesIoUring",
            },
            set(variants),
        )
        for label, cls in variants.items():
            with self.subTest(variant=label):
                case = cls()
                self.assertEqual(label.endswith("IoUring"), case.use_io_uring())
                self.assertEqual("NFS" in label, case.use_nfs())
                self.assertEqual(
                    "filteredhg" if "FilteredHg" in label else None,
                    case.backing_store_type,
                )
                self.assertEqual(
                    label.startswith("Coroutines"),
                    isinstance(case, testcase.CoroutinesTestMixin),
                )

    def test_filteredhg_io_uring_variant_excludes_nfs(self) -> None:
        variants = self._variants(filteredhg_test, FilteredHgTestCase)
        self.assertEqual({"TreeOnly", "TreeOnlyIoUring", "TreeOnlyNFS"}, set(variants))
        for label, cls in variants.items():
            with self.subTest(variant=label):
                self.assertTrue(issubclass(cls, FilteredHgTestCase))
                self.assertEqual(label.endswith("IoUring"), cls().use_io_uring())
                self.assertEqual(label == "TreeOnlyNFS", cls().use_nfs())

    @parameterized.expand([("hg", EdenHgTestCase), ("filtered", FilteredHgTestCase)])
    def test_transport_only_variants_preserve_migration_setup(
        self, name: str, base: type[EdenHgTestCase]
    ) -> None:
        variants = self._variants(testcase.eden_test, base, run_io_uring=True)
        self.assertEqual({"Default", "IoUring"}, set(variants))
        for label, cls in variants.items():
            with self.subTest(variant=label):
                case = cls()
                self.assertTrue(issubclass(cls, base))
                self.assertEqual(label == "IoUring", case.use_io_uring())
                self.assertFalse(case.use_nfs())
                self.assertEqual(base.backing_store_type, case.backing_store_type)
                self.assertNotIsInstance(case, testcase.CoroutinesTestMixin)

    def test_cached_status_composes_with_each_hg_variant(self) -> None:
        variants = self._variants(hg_cached_status_test)
        self.assertEqual(
            {
                f"{label}WithStatusCache{state}"
                for label in self._variants(hg_test)
                for state in ("Disabled", "Enabled")
            },
            set(variants),
        )
        for label, cls in variants.items():
            with self.subTest(variant=label):
                case = cls()
                self.assertEqual(label.endswith("Enabled"), case.enable_status_cache)
                self.assertEqual("IoUring" in label, case.use_io_uring())
                self.assertEqual("NFS" in label, case.use_nfs())

    def test_io_uring_opt_out_preserves_baseline_variants(self) -> None:
        for decorator, base in (
            (hg_test, EdenHgTestCase),
            (filteredhg_test, FilteredHgTestCase),
            (hg_cached_status_test, EdenHgTestCase),
        ):
            with self.subTest(decorator=decorator):
                expected = {
                    label
                    for label in self._variants(decorator, base)
                    if "IoUring" not in label
                }
                variants = self._variants(decorator, base, run_io_uring=False)
                self.assertEqual(expected, set(variants))
                self.assertTrue(
                    all(not cls().use_io_uring() for cls in variants.values())
                )

    def test_io_uring_variants_are_linux_only(self) -> None:
        for platform in ("darwin", "win32"):
            for decorator, base in (
                (hg_test, EdenHgTestCase),
                (filteredhg_test, FilteredHgTestCase),
                (hg_cached_status_test, EdenHgTestCase),
            ):
                with self.subTest(platform=platform, decorator=decorator):
                    with mock.patch.object(sys, "platform", platform):
                        variants = self._variants(decorator, base)
                        baseline = self._variants(decorator, base, run_io_uring=False)
                    self.assertEqual(set(baseline), set(variants))
                    self.assertTrue(
                        all(not cls().use_io_uring() for cls in variants.values())
                    )

    @parameterized.expand(
        [
            ("hg_baseline", hg_test, EdenHgTestCase, "TreeOnly"),
            ("hg_io_uring", hg_test, EdenHgTestCase, "TreeOnlyIoUring"),
            ("filtered_hg", hg_test, EdenHgTestCase, "TreeOnlyFilteredHg"),
            ("coroutines", hg_test, EdenHgTestCase, "Coroutines"),
            ("filtered_baseline", filteredhg_test, FilteredHgTestCase, "TreeOnly"),
            (
                "filtered_io_uring",
                filteredhg_test,
                FilteredHgTestCase,
                "TreeOnlyIoUring",
            ),
            (
                "cached_baseline",
                hg_cached_status_test,
                EdenHgTestCase,
                "TreeOnlyWithStatusCacheEnabled",
            ),
            (
                "cached_io_uring",
                hg_cached_status_test,
                EdenHgTestCase,
                "TreeOnlyIoUringWithStatusCacheEnabled",
            ),
            ("whole_family", hg_test, EdenHgTestCase, ""),
        ]
    )
    def test_method_skips_are_independent(
        self,
        name: str,
        decorator: Callable[..., Any],
        base: type[EdenHgTestCase],
        suffix: str,
    ) -> None:
        module = __name__.removeprefix("eden.integration.")
        with mock.patch.dict(
            skip.TEST_DISABLED, {f"{module}.Example{suffix}": ["test_example"]}
        ):
            variants = self._variants(decorator, base)
        for label, cls in variants.items():
            with self.subTest(variant=label):
                self.assertEqual(
                    bool(suffix) and label != suffix,
                    callable(getattr(cls, "test_example", None)),
                )
