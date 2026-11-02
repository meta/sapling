#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import binascii
import os
import sys
from pathlib import Path

from eden.fs.service.eden.thrift_clients import EdenService
from eden.fs.service.eden.thrift_types import (
    BlobMetadataOrError,
    BlobMetadataWithOrigin,
    DataFetchOrigin,
    DebugGetBlobMetadataRequest,
    DebugGetBlobMetadataResponse,
    DebugGetScmBlobRequest,
    DebugGetScmBlobResponse,
    DebugGetScmTreeRequest,
    DebugGetScmTreeResponse,
    MountId,
    ScmBlobMetadata,
    ScmBlobOrError,
    ScmBlobWithOrigin,
    ScmTreeEntry,
    ScmTreeOrError,
    ScmTreeWithOrigin,
    SyncBehavior,
)

from .lib import hgrepo, testcase


@testcase.eden_repo_test
class DebugBlobTest(testcase.EdenRepoTest):
    def populate_repo(self) -> None:
        self.repo.write_file("binary", b"\xff\xfe\xfd\xfc")
        self.repo.commit("Initial commit.")

    # TODO: enable when using the modern Python 3 Thrift API
    async def xtest_debug_blob_prints_binary_data(self) -> None:
        async with self.eden.get_async_thrift_client() as client:
            debugInfo = await client.debugInodeStatus(
                os.fsencode(self.mount), b".", flags=0, sync=SyncBehavior()
            )

        [root] = [entry for entry in debugInfo if entry.path == b""]
        self.assertEqual(1, root.inodeNumber)

        [file] = [entry for entry in root.entries if entry.name == b"binary"]
        self.assertEqual(False, file.materialized)
        blob_id = binascii.hexlify(file.hash).decode()
        print(blob_id)

        output = self.eden.run_cmd("debug", "blob", ".", blob_id, cwd=self.mount)
        self.assertEqual(b"\xff\xfe\xfd\xfc", output)


@testcase.eden_nfs_repo_test(run_coroutines=True)
class DebugBlobHgTest(testcase.HgRepoTestMixin, testcase.EdenRepoTest):
    def populate_repo(self) -> None:
        self.repo.write_file("binary", b"\xff\xfe\xfd\xfc")
        self.repo.commit("Initial commit.")

    async def assert_blob_not_available(
        self, client: EdenService.Async, blob_id: bytes, origin: int
    ) -> None:
        response = await client.debugGetBlob(
            DebugGetScmBlobRequest(
                mountId=MountId(mountPoint=self.mount.encode()),
                id=blob_id,
                origins=origin,
            )
        )
        print(response)
        # this should not error
        (response.blobs[0].blob.error)

    async def assert_blob_available(
        self,
        client: EdenService.Async,
        blob_id: bytes,
        origin: DataFetchOrigin,
        data: bytes,
    ) -> None:
        response = await client.debugGetBlob(
            DebugGetScmBlobRequest(
                mountId=MountId(mountPoint=self.mount.encode()),
                id=blob_id,
                origins=origin,
            )
        )
        print(response)
        self.assertEqual(
            DebugGetScmBlobResponse(
                blobs=[ScmBlobWithOrigin(blob=ScmBlobOrError(blob=data), origin=origin)]
            ),
            response,
        )

    async def test_debug_blob_locations(self) -> None:
        async with self.eden.get_async_thrift_client() as client:
            debugInfo = await client.debugInodeStatus(
                os.fsencode(self.mount), b".", flags=0, sync=SyncBehavior()
            )
        [root] = [entry for entry in debugInfo if entry.path == b""]
        self.assertEqual(1, root.inodeNumber)

        [file] = [entry for entry in root.entries if entry.name == b"binary"]
        self.assertEqual(False, file.materialized)
        blob_id = binascii.hexlify(file.hash).decode()
        print(file.hash)
        print(blob_id)

        self.eden.run_cmd("gc", cwd=self.mount)

        async with self.eden.get_async_thrift_client() as client:
            # not present in the local storage yet
            for origin in [
                DataFetchOrigin.MEMORY_CACHE,
            ]:
                print(origin)
                print(file.hash)
                await self.assert_blob_not_available(
                    client,
                    file.hash,
                    origin,
                )

            # "fetch from network"
            await self.assert_blob_available(
                client,
                file.hash,
                DataFetchOrigin.ANYWHERE,
                b"\xff\xfe\xfd\xfc",
            )

            # now its available locally.
            for fromWhere in [
                DataFetchOrigin.LOCAL_BACKING_STORE,
                # reading a blob is actually insufficient to put it in
                # DataFetchFromWhere.MEMORY_CACHE,
            ]:
                print(fromWhere)
                await self.assert_blob_available(
                    client, file.hash, fromWhere, b"\xff\xfe\xfd\xfc"
                )

            # check a request from multiple places:
            response = await client.debugGetBlob(
                DebugGetScmBlobRequest(
                    mountId=MountId(mountPoint=self.mount.encode()),
                    id=file.hash,
                    origins=DataFetchOrigin.MEMORY_CACHE
                    | DataFetchOrigin.LOCAL_BACKING_STORE
                    | DataFetchOrigin.REMOTE_BACKING_STORE
                    | DataFetchOrigin.ANYWHERE,
                )
            )
            print(response)

            self.assertEqual(4, len(response.blobs))
            for blob in response.blobs:
                if blob.origin == DataFetchOrigin.MEMORY_CACHE:
                    blob.blob.error
                elif blob.origin == DataFetchOrigin.LOCAL_BACKING_STORE:
                    self.assertEqual(b"\xff\xfe\xfd\xfc", blob.blob.blob)
                elif blob.origin == DataFetchOrigin.REMOTE_BACKING_STORE:
                    blob.blob.error
                elif blob.origin == DataFetchOrigin.ANYWHERE:
                    self.assertEqual(b"\xff\xfe\xfd\xfc", blob.blob.blob)

            if sys.platform != "win32":
                # on non windows platforms materializing an inode does cache it's
                # original blob contents, so now it should be in the local store.
                with open(Path(self.mount) / "binary", "a") as binary_file:
                    binary_file.buffer.write(b"\xfc")

                await self.assert_blob_available(
                    client,
                    file.hash,
                    DataFetchOrigin.MEMORY_CACHE,
                    b"\xff\xfe\xfd\xfc",
                )


@testcase.eden_nfs_repo_test
class DebugBlobMetadataHgTest(testcase.HgRepoTestMixin, testcase.EdenRepoTest):
    def populate_repo(self) -> None:
        self.repo.write_file("binary", b"\xff\xfe\xfd\xfc")
        self.repo.commit("Initial commit.")

    async def assert_metadata_not_available(
        self, client: EdenService.Async, blob_id: bytes, origin: int
    ) -> None:
        response = await client.debugGetBlobMetadata(
            DebugGetBlobMetadataRequest(
                mountId=MountId(mountPoint=self.mount.encode()),
                id=blob_id,
                origins=origin,
            )
        )
        print(response)
        # this should not error
        (response.metadatas[0].metadata.error)

    async def assert_metadata_available(
        self,
        client: EdenService.Async,
        blob_id: bytes,
        origin: DataFetchOrigin,
        sha1: bytes,
        size: int,
    ) -> None:
        response = await client.debugGetBlobMetadata(
            DebugGetBlobMetadataRequest(
                mountId=MountId(mountPoint=self.mount.encode()),
                id=blob_id,
                origins=origin,
            )
        )
        print(response)
        self.assertEqual(
            DebugGetBlobMetadataResponse(
                metadatas=[
                    BlobMetadataWithOrigin(
                        metadata=BlobMetadataOrError(
                            metadata=ScmBlobMetadata(size=size, contentsSha1=sha1)
                        ),
                        origin=origin,
                    )
                ]
            ),
            response,
        )

    async def test_debug_blob_metadata_locations(self) -> None:
        async with self.eden.get_async_thrift_client() as client:
            debugInfo = await client.debugInodeStatus(
                os.fsencode(self.mount), b".", flags=0, sync=SyncBehavior()
            )
        [root] = [entry for entry in debugInfo if entry.path == b""]
        self.assertEqual(1, root.inodeNumber)

        [file] = [entry for entry in root.entries if entry.name == b"binary"]
        self.assertEqual(False, file.materialized)
        blob_id = binascii.hexlify(file.hash).decode()
        print(file.hash)
        print(blob_id)

        self.eden.run_cmd("gc", cwd=self.mount)
        self.eden.restart()

        async with self.eden.get_async_thrift_client() as client:
            # not present in the local storage yet
            for origin in [
                DataFetchOrigin.MEMORY_CACHE,
            ]:
                print(origin)
                print(file.hash)
                await self.assert_metadata_not_available(
                    client,
                    file.hash,
                    origin,
                )

            # "fetch from network"
            await self.assert_metadata_available(
                client,
                file.hash,
                DataFetchOrigin.ANYWHERE,
                b"\x007\xc9\xb5h<\x0e\x8d\x8c\xa6qM\xb2\xf1Q\x9b#.\x10\xe2",
                4,
            )

            # now its available locally.
            for fromWhere in [
                DataFetchOrigin.MEMORY_CACHE,
                DataFetchOrigin.LOCAL_BACKING_STORE,
            ]:
                print(fromWhere)
                await self.assert_metadata_available(
                    client,
                    file.hash,
                    fromWhere,
                    b"\x007\xc9\xb5h<\x0e\x8d\x8c\xa6qM\xb2\xf1Q\x9b#.\x10\xe2",
                    4,
                )

            # check a request from multiple places:
            response = await client.debugGetBlobMetadata(
                DebugGetBlobMetadataRequest(
                    mountId=MountId(mountPoint=self.mount.encode()),
                    id=file.hash,
                    origins=DataFetchOrigin.MEMORY_CACHE
                    | DataFetchOrigin.LOCAL_BACKING_STORE
                    | DataFetchOrigin.REMOTE_BACKING_STORE
                    | DataFetchOrigin.ANYWHERE,
                )
            )
            print(response)

            self.assertEqual(4, len(response.metadatas))
            for metadata in response.metadatas:
                self.assertEqual(
                    4,
                    metadata.metadata.metadata.size,
                    f"wrong size for origin={metadata.origin.name}",
                )
                self.assertEqual(
                    b"\x007\xc9\xb5h<\x0e\x8d\x8c\xa6qM\xb2\xf1Q\x9b#.\x10\xe2",
                    metadata.metadata.metadata.contentsSha1,
                    f"wrong contentsSha1 for origin={metadata.origin.name}",
                )


@testcase.eden_nfs_repo_test
class DebugTreeHgTest(testcase.HgRepoTestMixin, testcase.EdenRepoTest):
    def populate_repo(self) -> None:
        self.repo.write_file("testDir/testTree/binary", b"\xff\xfe\xfd\xfc")
        self.repo.commit("Initial commit.")

    async def assert_tree_not_available(
        self, client: EdenService.Async, tree_id: bytes, origin: int
    ) -> None:
        response = await client.debugGetTree(
            DebugGetScmTreeRequest(
                mountId=MountId(mountPoint=self.mount.encode()),
                id=tree_id,
                origins=origin,
            )
        )
        print(response)
        # this should not error
        response.trees[0].scmTreeData.error

    async def assert_tree_available(
        self,
        client: EdenService.Async,
        tree_id: bytes,
        origin: DataFetchOrigin,
        name: bytes,
        mode: int,
        thrift_obj_id: bytes,
    ) -> None:
        response = await client.debugGetTree(
            DebugGetScmTreeRequest(
                mountId=MountId(mountPoint=self.mount.encode()),
                id=tree_id,
                origins=origin,
            )
        )
        print(f"response of debugGetTree: {response}")
        self.assertEqual(
            DebugGetScmTreeResponse(
                trees=[
                    ScmTreeWithOrigin(
                        scmTreeData=ScmTreeOrError(
                            treeEntries=[
                                ScmTreeEntry(name=name, mode=mode, id=thrift_obj_id)
                            ]
                        ),
                        origin=origin,
                    )
                ]
            ),
            response,
        )

    async def test_debug_tree_of_empty_tree(self) -> None:
        # A commit without files has an empty root tree. `debugGetTree` returns
        # an empty `treeEntries` list for it and `eden debug tree` prints an
        # empty tree.
        repo = self.repo
        assert isinstance(repo, hgrepo.HgRepository)
        repo.hg("rm", "testDir/testTree/binary")
        empty_commit = repo.commit("Remove every file.")
        manifest = repo.hg("log", "-r", empty_commit, "-T", "{manifest}").strip()

        async with self.eden.get_async_thrift_client() as client:
            response = await client.debugGetTree(
                DebugGetScmTreeRequest(
                    mountId=MountId(mountPoint=self.mount.encode()),
                    id=manifest.encode(),
                    origins=DataFetchOrigin.ANYWHERE,
                )
            )
        tree = response.trees[0].scmTreeData
        self.assertEqual(ScmTreeOrError.Type.treeEntries, tree.type)
        self.assertEqual([], list(tree.treeEntries))

        output = self.eden.run_cmd("debug", "tree", self.mount, manifest)
        self.assertEqual("", output)

    async def test_debug_tree_locations(self) -> None:
        async with self.eden.get_async_thrift_client() as client:
            debugInfo = await client.debugInodeStatus(
                os.fsencode(self.mount),
                b"testDir/testTree",
                flags=0,
                sync=SyncBehavior(),
            )

        print(f"debug info: {debugInfo}")
        """
        [TreeInodeDebugInfo(
            inodeNumber=24,
            path=b'testDir/testTree',
            materialized=False,
            treeHash=b'6e6f3da79253c3861bd140490d95cbe2c9323de8:testDir/testTree',
            entries=[TreeInodeEntryDebugInfo(
                name=b'binary',
                inodeNumber=25,
                mode=32768,
                loaded=False,
                materialized=False,
                hash=b'4bee6c9836b6c191a528063745ded4a1a17cdedd:testDir/testTree/binary',
                fileSize=4)],
            refcount=0)]
        """
        self.assertEqual(1, len(debugInfo))
        treeInfo = debugInfo[0]
        treeEntriesInfo = treeInfo.entries
        self.assertEqual(1, len(treeEntriesInfo))

        treeEntry = treeEntriesInfo[0]

        self.eden.run_cmd("gc", cwd=self.mount)  # this clears cache in disk
        self.eden.restart()  # this clears cache in memory

        async with self.eden.get_async_thrift_client() as client:
            # not present in the local storage yet
            for origin in [DataFetchOrigin.MEMORY_CACHE]:
                await self.assert_tree_not_available(client, treeInfo.treeHash, origin)

            debugInfo = await client.debugInodeStatus(
                os.fsencode(self.mount),
                b"testDir/testTree",
                flags=0,
                sync=SyncBehavior(),
            )

            # "fetch from network"
            await self.assert_tree_available(
                client,
                treeInfo.treeHash,
                DataFetchOrigin.ANYWHERE,
                treeEntry.name,
                treeEntry.mode | 0o644,
                treeEntry.hash,
            )

            # now its available locally.
            for fromWhere in [
                DataFetchOrigin.MEMORY_CACHE,
                DataFetchOrigin.LOCAL_BACKING_STORE,
            ]:
                await self.assert_tree_available(
                    client,
                    treeInfo.treeHash,
                    fromWhere,
                    treeEntry.name,
                    treeEntry.mode | 0o644,
                    treeEntry.hash,
                )

            # check a request from multiple places:
            response = await client.debugGetTree(
                DebugGetScmTreeRequest(
                    mountId=MountId(mountPoint=self.mount.encode()),
                    id=treeInfo.treeHash,
                    origins=DataFetchOrigin.MEMORY_CACHE
                    | DataFetchOrigin.LOCAL_BACKING_STORE
                    | DataFetchOrigin.REMOTE_BACKING_STORE
                    | DataFetchOrigin.ANYWHERE,
                )
            )
            self.assertEqual(4, len(response.trees))

            for tree in response.trees:
                # It seems from a unit test, we can't get the tree from remote backing store.
                # error example:
                """
                ScmTreeWithOrigin(
                        scmTreeData=ScmTreeOrError(
                    error=EdenError(
                          message=('rust::cxxbridge1::Error: Key fetch failed '
                           '6e6f3da79253c3861bd140490d95cbe2c9323de8 : [server did not provide content]'),
                          errorType=4)),
                origin=16)
                """
                if tree.origin == DataFetchOrigin.REMOTE_BACKING_STORE:
                    tree.scmTreeData.error
                else:
                    self.assertEqual(
                        ScmTreeOrError(
                            treeEntries=[
                                ScmTreeEntry(
                                    name=treeEntry.name,
                                    mode=treeEntry.mode | 0o644,
                                    id=treeEntry.hash,
                                )
                            ]
                        ),
                        tree.scmTreeData,
                        f"unexpected response {tree.scmTreeData} for origin {tree.origin.name}",
                    )
