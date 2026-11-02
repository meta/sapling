#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import unittest
from datetime import datetime, timedelta, tzinfo
from unittest.mock import patch

from eden.fs.cli import version


class FixedClock(datetime):
    """`datetime` with a fixed `now()`: `get_version_info` reads the clock once
    per version, so a real clock crossing midnight would move the ages."""

    @classmethod
    def now(cls, tz: tzinfo | None = None) -> "FixedClock":
        return FIXED_NOW


FIXED_NOW = FixedClock(2026, 3, 15, 12, 0, 0)


class VersionAgesTest(unittest.TestCase):
    def setUp(self) -> None:
        patcher = patch.object(version, "datetime", FixedClock)
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_ages_delta_when_the_installed_build_is_from_today(self) -> None:
        installed_version = FIXED_NOW.strftime("%Y%m%d-%H%M%S")
        running_version = (FIXED_NOW - timedelta(days=60)).strftime("%Y%m%d-%H%M%S")

        with patch.object(
            version, "get_current_version", return_value=installed_version
        ):
            info = version.get_version_info(running_version)

        self.assertEqual(info.installed_version_age, 0)
        self.assertEqual(info.running_version_age, 60)
        self.assertEqual(info.ages_deltas, 60)

    def test_ages_delta_when_both_builds_are_older_than_today(self) -> None:
        installed_version = (FIXED_NOW - timedelta(days=1)).strftime("%Y%m%d-%H%M%S")
        running_version = (FIXED_NOW - timedelta(days=61)).strftime("%Y%m%d-%H%M%S")

        with patch.object(
            version, "get_current_version", return_value=installed_version
        ):
            info = version.get_version_info(running_version)

        self.assertEqual(info.ages_deltas, 60)
