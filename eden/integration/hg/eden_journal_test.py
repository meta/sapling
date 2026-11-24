#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import os
from typing import Optional

from eden.fs.service.eden.thrift_types import EdenError, EdenErrorType, ScmFileStatus
from eden.fs.service.streamingeden.thrift_clients import StreamingEdenService
from eden.fs.service.streamingeden.thrift_types import StreamChangesSinceParams
from eden.integration.lib import hgrepo
from thrift.python.client import ClientType, get_client

from .lib.hg_extension_test_base import EdenHgTestCase, hg_test


@hg_test
# pyre-ignore[13]: T62487924
class EdenJournalTest(EdenHgTestCase):
    # pyre-fixme[13]: Attribute `commit1` is never initialized.
    commit1: str
    # pyre-fixme[13]: Attribute `commit2` is never initialized.
    commit2: str

    def populate_backing_repo(self, repo: hgrepo.HgRepository) -> None:
        repo.write_file("hello.txt", "hello\n")
        self.commit1 = repo.commit("Initial commit")
        repo.write_file("foo/bar.txt", "bar\n")
        self.commit2 = repo.commit("Commit 2")

    async def test_journal_position_write(self) -> None:
        """
        Verify that the journal is updated when writing to the working copy.
        """
        async with self.get_async_thrift_client() as client:
            before = await client.getCurrentJournalPosition(self.mount_path_bytes)

        self.repo.write_file("hello.txt", "hola\n")

        async with self.get_async_thrift_client() as client:
            after = await client.getCurrentJournalPosition(self.mount_path_bytes)

        self.assertNotEqual(before, after)

    def get_streaming_client(
        self, timeout: Optional[float] = None
    ) -> StreamingEdenService.Async:
        eden_dir = self.eden._eden_dir
        socket_path = os.path.join(eden_dir, "socket")
        if timeout is None:
            timeout = 0
        return get_client(
            StreamingEdenService,
            path=socket_path,
            timeout=timeout,
            client_type=ClientType.THRIFT_ROCKET_CLIENT_TYPE,
        )

    async def test_journal_change_stream_reports_shutdown(self) -> None:
        async with self.get_streaming_client() as client:
            stream = await client.streamJournalChanged(self.mount_path_bytes)
            self.eden.shutdown()

            with self.assertRaises(EdenError) as context:
                async for _ in stream:
                    pass

            self.assertEqual(
                context.exception.errorType,
                EdenErrorType.SHUTTING_DOWN,
            )

    async def test_journal_stream_changes_since(self) -> None:
        """
        Verify that the streamChangesSince API reports all the changed
        files/directories across update.
        """

        # This is the only integration test that needs Thrift streaming
        # support, which currently is not supported in the open source build.

        async with self.get_streaming_client() as client:
            before = await client.getCurrentJournalPosition(self.mount_path_bytes)

        self.repo.update(self.commit1)

        self.repo.write_file("hello.txt", "hola\n")
        self.repo.write_file("bar.txt", "bar\n")

        added = set()
        removed = set()
        modified = set()

        async with self.get_streaming_client() as client:
            params = StreamChangesSinceParams(
                mountPoint=self.mount_path_bytes,
                fromPosition=before,
            )
            result, changes = await client.streamChangesSince(params)
            async for change in changes:
                path = change.name.decode()
                if path.startswith(".hg"):
                    continue

                status = change.status
                if status == ScmFileStatus.ADDED:
                    added.add(path)
                elif status == ScmFileStatus.MODIFIED:
                    modified.add(path)
                else:
                    self.assertEqual(status, ScmFileStatus.REMOVED)
                    removed.add(path)

        # Files not in commits:
        self.assertIn("hello.txt", modified)
        self.assertIn("bar.txt", added)

        # Files in commits:
        self.assertIn("foo/bar.txt", removed)

        # The directory is also removed.
        self.assertIn("foo", removed)

        self.assertNotEqual(before, result.toPosition)

        counter_name = (
            "thrift.StreamingEdenService.streamChangesSince.streaming_time_us.avg.60"
        )
        counters = self.get_counters()
        self.assertIn(counter_name, counters)
        self.assertGreater(counters[counter_name], 0)
