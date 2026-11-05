#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import os
from collections import Counter

from .lib import testcase


@testcase.eden_repo_test
class ReaddirCookiesBreakAcrossCheckoutTest(testcase.EdenRepoTest):
    # Readdir resume cookies are inode numbers, checkout reassigns them via
    # erase-and-emplace with fresh monotonically increasing numbers, and the
    # NFS cookie verifier is hard-coded to 0, so a directory stream spanning
    # a checkout can silently duplicate or drop entries.
    #
    # Scale requirement, learned the hard way: the straddle is only real if
    # the directory does NOT fit in a single getdirentries response. With a
    # small directory, os.scandir prefetches every entry into its userspace
    # buffer before the checkout runs, the mid-stream checkout is invisible,
    # and no duplicates appear. Thousands of long-named entries force
    # several RPCs per half-stream, so the checkout genuinely lands
    # mid-stream. The exact-count assertions below guard against silent
    # underpopulation of the fixture.
    num_files: int = 2000
    commit0: str = ""
    commit1: str = ""

    def file_name(self, i: int) -> str:
        return f"churn/f{i:04d}_padding_to_force_multiple_readdir_rpcs.txt"

    def populate_repo(self) -> None:
        for i in range(self.num_files):
            self.repo.write_file(self.file_name(i), f"v0-{i}\n")
        self.commit0 = self.repo.commit("Commit 0.")
        for i in range(self.num_files):
            self.repo.write_file(self.file_name(i), f"v1-{i}\n")
        self.commit1 = self.repo.commit("Commit 1.")

    def list_names(self) -> set[str]:
        return set(os.listdir(self.get_path("churn")))

    def straddle_checkout(self, target: str) -> tuple[set[str], set[str], list[str]]:
        """Half-read a directory stream, check out target mid-stream, drain.

        Returns (before, after, stream): the full listings bracketing the
        checkout and the concatenated names observed through one open DIR*.
        """
        before = self.list_names()
        self.assertEqual(len(before), self.num_files)

        stream: list[str] = []
        it = os.scandir(self.get_path("churn"))
        try:
            half = max(len(before) // 2, 1)
            for _ in range(half):
                try:
                    stream.append(next(it).name)
                except StopIteration:
                    break

            read_before_checkout = len(stream)
            self.eden_repo.update(target)

            for entry in it:
                stream.append(entry.name)
            read_after_checkout = len(stream) - read_before_checkout
        finally:
            it.close()

        # Assert the checkout genuinely landed mid-stream: entries were read
        # both before the update and from the drain afterwards. The duplicates
        # assertion in the test itself is the stronger guard: a stream that
        # os.scandir had fully prefetched before the checkout could not show
        # duplicates, so observing them proves the checkout landed mid-stream.
        self.assertGreater(read_before_checkout, 0)
        self.assertGreater(read_after_checkout, 0)

        after = self.list_names()
        self.assertEqual(len(after), self.num_files)
        return before, after, stream

    def stream_inconsistencies(
        self, before: set[str], after: set[str], stream: list[str]
    ) -> tuple[list[str], list[str]]:
        stable = before & after
        seen = set(stream)
        missing = sorted(stable - seen)
        duplicates = sorted(n for n, c in Counter(stream).items() if c > 1)
        return duplicates, missing

    # TODO(T287885560): the straddling stream currently shows the cookie bug.
    # This test asserts the buggy behavior so it passes today and fails once
    # the bug is fixed; then flip it to assert no duplicates or gaps.
    def test_straddling_stream_shows_cookie_bug(self) -> None:
        # Straddle in both directions: the cookie collision does not depend
        # on which commit is newer, and asserting on the union halves the
        # chance that a lucky layout masks it in any single attempt.
        self.eden_repo.update(self.commit1)
        before0, after0, stream0 = self.straddle_checkout(self.commit0)
        before1, after1, stream1 = self.straddle_checkout(self.commit1)

        duplicates0, missing0 = self.stream_inconsistencies(before0, after0, stream0)
        duplicates1, missing1 = self.stream_inconsistencies(before1, after1, stream1)
        msg = (
            f"direction0: stream={len(stream0)} "
            f"duplicates={duplicates0[:5]} missing={missing0[:5]} "
            f"direction1: stream={len(stream1)} "
            f"duplicates={duplicates1[:5]} missing={missing1[:5]}"
        )
        self.assertTrue(
            duplicates0 or missing0 or duplicates1 or missing1,
            msg=f"T287885560: expected the cookie bug, got a clean stream. {msg}",
        )
