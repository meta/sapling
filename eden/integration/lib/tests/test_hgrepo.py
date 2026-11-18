# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

from __future__ import annotations

import os
import subprocess
import unittest
from unittest.mock import AsyncMock, MagicMock, patch

from eden.integration.lib import hgrepo
from later.unittest import TestCase as AsyncTestCase


def make_repository(environment: dict[str, str]) -> hgrepo.HgRepository:
    repo = object.__new__(hgrepo.HgRepository)
    repo.path = "/temporary/repository"
    repo.hg_bin = "/unchanged/hg"
    repo.hg_environment = environment
    return repo


class HgSanitizerEnvironmentTest(unittest.TestCase):
    def test_only_child_disables_inherited_leak_detection(self) -> None:
        environment = {
            "ASAN_OPTIONS": "halt_on_error=1:detect_leaks=1:malloc_context_size=40",
            "LSAN_OPTIONS": "report_objects=1",
            "OTHER": "untouched",
        }
        original = dict(environment)
        child = hgrepo._hg_sanitizer_environment(environment)
        self.assertEqual(environment, original)
        self.assertEqual(
            child["ASAN_OPTIONS"], original["ASAN_OPTIONS"] + ":detect_leaks=0"
        )
        self.assertEqual(child["LSAN_OPTIONS"], "report_objects=1")
        self.assertEqual(child["OTHER"], "untouched")

    def test_absent_asan_options_keep_default_policy(self) -> None:
        self.assertEqual(
            hgrepo._hg_sanitizer_environment({"OTHER": "value"}), {"OTHER": "value"}
        )

    def test_empty_options_and_trailing_separator(self) -> None:
        for value, expected in [
            ("", "detect_leaks=0"),
            ("detect_leaks=1:", "detect_leaks=1:detect_leaks=0"),
        ]:
            with self.subTest(value=value):
                self.assertEqual(
                    hgrepo._hg_sanitizer_environment({"ASAN_OPTIONS": value})[
                        "ASAN_OPTIONS"
                    ],
                    expected,
                )

    def test_existing_tsan_policy_is_preserved(self) -> None:
        environment = {
            "ASAN_OPTIONS": "detect_leaks=1",
            "TSAN_OPTIONS": "halt_on_error=1:second_deadlock_stack=1",
            "EDEN_TSAN_SUPPRESSIONS": "relative-suppressions.txt",
        }
        child = hgrepo._hg_sanitizer_environment(environment)
        self.assertEqual(
            child["TSAN_OPTIONS"],
            environment["TSAN_OPTIONS"]
            + ":suppressions="
            + os.path.abspath("relative-suppressions.txt"),
        )
        self.assertNotIn("suppressions=", environment["TSAN_OPTIONS"])
        self.assertEqual(child["ASAN_OPTIONS"], "detect_leaks=1:detect_leaks=0")

    def test_custom_tsan_suppressions_are_not_replaced(self) -> None:
        environment = {
            "TSAN_OPTIONS": "halt_on_error=1:suppressions=/custom/file",
            "EDEN_TSAN_SUPPRESSIONS": "/other/file",
        }
        self.assertEqual(hgrepo._hg_sanitizer_environment(environment), environment)

    def test_sync_launch_keeps_binary_and_parent_environment(self) -> None:
        parent = {"ASAN_OPTIONS": "detect_leaks=1", "OTHER": "original"}
        override = {"ASAN_OPTIONS": "halt_on_error=1:detect_leaks=1"}
        repo = make_repository(parent)
        with (
            patch.dict(os.environ, {"ASAN_OPTIONS": "detect_leaks=1"}),
            patch.object(
                hgrepo.subprocess,
                "run",
                return_value=subprocess.CompletedProcess([], 0),
            ) as run,
        ):
            repo.run_hg("init", stdout=None, stderr=None, env=override)
            self.assertEqual(os.environ["ASAN_OPTIONS"], "detect_leaks=1")
        self.assertEqual(
            run.call_args.args[0], ["/unchanged/hg", "--traceback", "init"]
        )
        self.assertEqual(
            run.call_args.kwargs["env"]["ASAN_OPTIONS"],
            "halt_on_error=1:detect_leaks=1:detect_leaks=0",
        )
        self.assertEqual(parent["ASAN_OPTIONS"], "detect_leaks=1")
        self.assertEqual(override["ASAN_OPTIONS"], "halt_on_error=1:detect_leaks=1")


class AsyncHgSanitizerEnvironmentTest(AsyncTestCase):
    async def test_async_launch_uses_the_same_child_policy(self) -> None:
        parent = {
            "ASAN_OPTIONS": "detect_leaks=1",
            "TSAN_OPTIONS": "halt_on_error=1",
            "EDEN_TSAN_SUPPRESSIONS": "/test/suppressions",
        }
        original = dict(parent)
        repo = make_repository(parent)
        process = MagicMock()
        process.communicate = AsyncMock(return_value=(b"updated", b""))
        process.returncode = 0
        with patch.object(
            hgrepo.asyncio,
            "create_subprocess_exec",
            new_callable=AsyncMock,
            return_value=process,
        ) as launch:
            result = await repo.update_async("revision", clean=True)
        self.assertEqual(result, "updated")
        self.assertEqual(
            launch.call_args.args,
            ("/unchanged/hg", "--traceback", "update", "--clean", "revision"),
        )
        self.assertEqual(
            launch.call_args.kwargs["env"]["ASAN_OPTIONS"],
            "detect_leaks=1:detect_leaks=0",
        )
        self.assertEqual(
            launch.call_args.kwargs["env"]["TSAN_OPTIONS"],
            "halt_on_error=1:suppressions="
            + os.path.abspath(parent["EDEN_TSAN_SUPPRESSIONS"]),
        )
        self.assertEqual(parent, original)
