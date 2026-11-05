#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import binascii
import hashlib
import os
import re
import subprocess
import sys
from errno import EINVAL, EISDIR, ENOENT
from pathlib import Path
from typing import Dict, List, Optional, Pattern, TypeVar, Union

from eden.fs.config.eden_config.thrift_types import ConfigReloadBehavior
from eden.fs.service.eden.thrift_types import (
    Blake3Result,
    DigestHashResult,
    EdenErrorType,
    FileAttributeDataOrErrorV2,
    GetConfigParams,
    GetFileContentRequest,
    MountId,
    ScmBlobOrError,
    ScmFileStatus,
    SHA1Result,
    SyncBehavior,
    TimeSpec,
)

from .lib import testcase
from .lib.find_executables import FindExe

EdenThriftResult = TypeVar(
    "EdenThriftResult",
    FileAttributeDataOrErrorV2,
    SHA1Result,
    Blake3Result,
    DigestHashResult,
    ScmBlobOrError,
)


@testcase.eden_repo_test
class ThriftTest(testcase.EdenRepoTest):
    # The following members are initialized in populate_repo()
    commit1: str = ""
    commit2: str = ""
    commit3: str = ""
    local_commit: str = ""
    expected_adir_digest_hash: bytes = b""
    expected_hello_blake3: bytes = b""
    expected_adir_file_blake3: bytes = b""
    _blake3_computed: bool = False

    def edenfs_extra_config(self) -> Optional[Dict[str, List[str]]]:
        result = super().edenfs_extra_config() or {}
        result.setdefault("hash", []).append(
            'blake3-key = "20220728-2357111317192329313741#"'
        )
        return result

    async def blake3_hash(self, blob: bytes) -> bytes:
        key: Optional[str] = None
        async with self.get_async_thrift_client() as client:
            config = await client.getConfig(
                GetConfigParams(reload=ConfigReloadBehavior.ForceReload)
            )
            maybe_key = config.values.get("hash:blake3-key")
            key = (
                maybe_key.parsedValue
                if maybe_key is not None and maybe_key.parsedValue != ""
                else None
            )
            print(f"Resolved key: {maybe_key}, actual key: {key}")

        cmd = [FindExe.BLAKE3_SUM]
        if key is not None:
            cmd.extend(["--key", key])

        p = subprocess.run(cmd, stdout=subprocess.PIPE, input=blob)
        assert p.returncode == 0, "0 exit code is expected for blake3_sum"
        return bytes.fromhex(p.stdout.decode("ascii"))

    async def _ensure_blake3_computed(self) -> None:
        """Compute blake3 hashes if not already computed."""
        if not self._blake3_computed:
            self.expected_hello_blake3 = await self.blake3_hash(b"hola\n")
            self.expected_adir_file_blake3 = await self.blake3_hash(b"foo!\n")
            self._blake3_computed = True

    def populate_repo(self) -> None:
        self.repo.write_file("hello", "hola\n")
        self.repo.write_file("test_fetch1", "testing fetch\n")
        self.repo.write_file("test_fetch2", "testing fetch\n")
        self.repo.write_file("README", "docs\n")
        self.repo.write_file("adir/file", "foo!\n")
        self.repo.write_file("bdir/file", "bar!\n")
        self.repo.symlink("slink", "hello")
        self.commit1 = self.repo.commit("Initial commit.")

        self.repo.write_file("bdir/file", "bar?\n")
        self.repo.write_file("cdir/subdir/new.txt", "and improved")
        self.repo.remove_file("README")
        self.commit2 = self.repo.commit("Commit 2.")

        # revert the change made to bdir/file
        self.repo.write_file("bdir/file", "bar!\n")
        self.commit3 = self.repo.commit("Commit 3.")

        # Eagerepo requires commits to be pushed to the server so that
        # aux data can be derived for trees
        if self.repo_type in ["hg", "filteredhg"]:
            self.repo.push(rev=".", target="master", create=True)

        # Commit changes, but don't push them to the server (simulates local-only trees)
        self.repo.write_file("local_dir/file", "hola\n")
        self.local_commit = self.repo.commit("Local Commit")

        # There is no easy way to compute the dighest hash for a directory on the fly (in Python)
        # Since these hashes/sizes should stay constant, we can just hardcode the expected result
        #
        # We define digest hashes as bytes since defining them as hex and then converting to bytes
        # could lead to an integer overflow.
        #
        # Future computation of results:
        #   digest_hash_res = results[0].get_digestHash()
        #   digest_hash_bytes = "\\".join(list(map(hex, digest_hash_res)))
        #   expected_digest_hash = "\\" + digest_hash_bytes.replace("0x", "x")
        self.expected_adir_digest_hash = b"\x73\xf0\xc6\xe3\x6b\x3c\xb9\xfc\x64\xa8\xa3\x39\x24\x57\xd3\xc9\xd0\x2d\x11\xfd\x22\xe5\x36\x71\x94\x5d\x95\x3f\xfa\xc3\x8c\x92"

        # For files, digest hashes are just the blake3 hash of the file contents
        # We'll compute these as needed within tests, since populate_repo can't be async
        self.expected_hello_blake3 = b""
        self.expected_adir_file_blake3 = b""
        self._blake3_computed = False

    async def get_loaded_inodes_count(self, path: str) -> int:
        async with self.get_async_thrift_client() as client:
            result = await client.debugInodeStatus(
                self.mount_path_bytes, os.fsencode(path), flags=0, sync=SyncBehavior()
            )
        inode_count = 0
        for item in result:
            assert item.entries is not None
            for inode in item.entries:
                if inode.loaded:
                    inode_count += 1
        return inode_count

    async def test_get_file_content(self) -> None:
        thrift_mount = MountId(mountPoint=self.mount.encode("utf-8"))

        # Unmaterialized file
        async with self.eden.get_async_thrift_client() as client:
            thrift_req = GetFileContentRequest(
                mount=thrift_mount, filePath=b"hello", sync=SyncBehavior()
            )
            response = await client.getFileContent(thrift_req)
            self.assertEqual(
                ScmBlobOrError.Type.blob,
                response.blob.type,
                "expect success",
            )
            self.assertEqual(b"hola\n", response.blob.blob)

        # Materialized file
        self.write_file("tmpdir/file", "hola\n")
        async with self.eden.get_async_thrift_client() as client:
            thrift_req = GetFileContentRequest(
                mount=thrift_mount, filePath=b"tmpdir/file", sync=SyncBehavior()
            )
            response = await client.getFileContent(thrift_req)
            self.assertEqual(
                ScmBlobOrError.Type.blob,
                response.blob.type,
                "expect success",
            )
            self.assertEqual(b"hola\n", response.blob.blob)

        # Invalid paths
        self.rm("tmpdir/file")
        async with self.eden.get_async_thrift_client() as client:
            thrift_req = GetFileContentRequest(
                mount=thrift_mount, filePath=b"tmpdir/file", sync=SyncBehavior()
            )
            response = await client.getFileContent(thrift_req)
            self.assertEqual(
                ScmBlobOrError.Type.error,
                response.blob.type,
                "expect error",
            )
            self.assert_eden_error(
                response.blob,
                "tmpdir/file: No such file or directory",
                EdenErrorType.POSIX_ERROR,
                ENOENT,
            )
            thrift_req = GetFileContentRequest(
                mount=thrift_mount, filePath=b"tmpdir", sync=SyncBehavior()
            )
            response = await client.getFileContent(thrift_req)
            self.assertEqual(
                ScmBlobOrError.Type.error,
                response.blob.type,
                "expect error",
            )
            self.assert_eden_error(
                response.blob,
                "tmpdir: Is a directory",
                EdenErrorType.POSIX_ERROR,
                EISDIR,
            )
        self.rmdir("tmpdir")

    async def test_pid_fetch_counts(self) -> None:
        # We already test that our fetch counts get incremented correctly in
        # unit tests. This is an end to end test to make sure that our fetch
        # counts are reasonable values. We know touching a file means we must
        # read it at least once. So we expect at least 2 fetches here. In
        # reality there may be more than that because touch will cause multiple
        # requests into fuse for each file.
        touch_p = subprocess.Popen(
            "touch test_fetch1 test_fetch2".split(), cwd=self.mount_path
        )
        touch_p.communicate()

        async with self.get_async_thrift_client() as client:
            counts = await client.getAccessCounts(1)
            accesses = counts.accessesByMount[self.mount_path_bytes]
            self.assertLessEqual(2, accesses.fetchCountsByPid[touch_p.pid])

    async def test_list_mounts(self) -> None:
        async with self.get_async_thrift_client() as client:
            mounts = await client.listMounts()
        self.assertEqual(1, len(mounts))

        mount = mounts[0]
        self.assertEqual(self.mount_path_bytes, mount.mountPoint)
        assert mount.edenClientPath is not None
        # The client path should always be inside the main eden directory
        # Path.relative_to() will throw a ValueError if self.eden.eden_dir is not a
        # directory prefix of mount.edenClientPath
        Path(os.fsdecode(mount.edenClientPath)).relative_to(self.eden.eden_dir)
        if sys.platform.startswith("linux"):
            self.assertTrue(mount.visibleInDaemonNamespace)

    async def test_get_sha1(self) -> None:
        expected_sha1_for_hello = hashlib.sha1(b"hola\n").digest()
        result_for_hello = SHA1Result(sha1=expected_sha1_for_hello)

        expected_sha1_for_adir_file = hashlib.sha1(b"foo!\n").digest()
        result_for_adir_file = SHA1Result(sha1=expected_sha1_for_adir_file)

        async with self.get_async_thrift_client() as client:
            self.assertEqual(
                [result_for_hello, result_for_adir_file],
                await client.getSHA1(
                    self.mount_path_bytes,
                    [b"hello", b"adir/file"],
                    sync=SyncBehavior(),
                ),
            )

    async def test_get_blake3(self) -> None:
        await self._ensure_blake3_computed()
        result_for_hello = Blake3Result(blake3=self.expected_hello_blake3)
        result_for_adir_file = Blake3Result(blake3=self.expected_adir_file_blake3)

        async with self.get_async_thrift_client() as client:
            self.assertEqual(
                [result_for_hello, result_for_adir_file],
                await client.getBlake3(
                    self.mount_path_bytes,
                    [b"hello", b"adir/file"],
                    sync=SyncBehavior(),
                ),
            )

    async def test_get_digest_hash_for_file(self) -> None:
        await self._ensure_blake3_computed()
        async with self.get_async_thrift_client() as client:
            results = await client.getDigestHash(
                self.mount_path_bytes,
                [b"hello", b"adir/file"],
                sync=SyncBehavior(syncTimeoutSeconds=60),
            )

        self.assertEqual(2, len(results))
        self.assertEqual(
            results,
            [
                DigestHashResult(digestHash=self.expected_hello_blake3),
                DigestHashResult(digestHash=self.expected_adir_file_blake3),
            ],
        )

    async def test_get_digest_hash_for_directory(self) -> None:
        async with self.get_async_thrift_client() as client:
            results = await client.getDigestHash(
                self.mount_path_bytes,
                [b"adir"],
                sync=SyncBehavior(syncTimeoutSeconds=60),
            )

        self.assertEqual(
            results, [DigestHashResult(digestHash=self.expected_adir_digest_hash)]
        )

    async def test_get_sha1_throws_for_path_with_dot_components(self) -> None:
        async with self.get_async_thrift_client() as client:
            results = await client.getSHA1(
                self.mount_path_bytes, [b"./hello"], sync=SyncBehavior()
            )
        self.assertEqual(1, len(results))
        self.assert_sha1_error(
            results[0],
            re.compile(
                r".*PathComponentValidationError.*: PathComponent must not be \."
            ),
            EdenErrorType.GENERIC_ERROR,
        )

    async def test_get_blake3_throws_for_path_with_dot_components(self) -> None:
        async with self.get_async_thrift_client() as client:
            results = await client.getBlake3(
                self.mount_path_bytes, [b"./hello"], sync=SyncBehavior()
            )
        self.assertEqual(1, len(results))
        self.assert_blake3_error(
            results[0],
            re.compile(
                r".*PathComponentValidationError.*: PathComponent must not be \."
            ),
            EdenErrorType.GENERIC_ERROR,
        )

    async def test_get_digest_hash_throws_for_path_with_dot_components(self) -> None:
        async with self.get_async_thrift_client() as client:
            results = await client.getDigestHash(
                self.mount_path_bytes, [b"./hello"], sync=SyncBehavior()
            )
        self.assertEqual(1, len(results))
        self.assert_digest_hash_error(
            results[0],
            re.compile(
                r".*PathComponentValidationError.*: PathComponent must not be \."
            ),
            EdenErrorType.GENERIC_ERROR,
        )

    async def test_get_sha1_throws_for_empty_string(self) -> None:
        async with self.get_async_thrift_client() as client:
            results = await client.getSHA1(
                self.mount_path_bytes, [b""], sync=SyncBehavior()
            )
        self.assertEqual(1, len(results))
        self.assert_sha1_error(
            results[0], ": Is a directory", EdenErrorType.POSIX_ERROR, EISDIR
        )

    async def test_get_blake3_throws_for_empty_string(self) -> None:
        async with self.get_async_thrift_client() as client:
            results = await client.getBlake3(
                self.mount_path_bytes, [b""], sync=SyncBehavior()
            )
        self.assertEqual(1, len(results))
        self.assert_blake3_error(
            results[0], ": Is a directory", EdenErrorType.POSIX_ERROR, EISDIR
        )

    async def test_get_digest_hash_throws_for_empty_string(self) -> None:
        async with self.get_async_thrift_client() as client:
            results = await client.getDigestHash(
                self.mount_path_bytes, [b""], sync=SyncBehavior()
            )
        self.assertEqual(1, len(results))
        self.assert_digest_hash_error(
            results[0],
            "tree aux data missing for tree",
            EdenErrorType.ATTRIBUTE_UNAVAILABLE,
            ENOENT,
        )

    async def test_get_sha1_throws_for_directory(self) -> None:
        async with self.get_async_thrift_client() as client:
            results = await client.getSHA1(
                self.mount_path_bytes,
                [b"adir"],
                sync=SyncBehavior(syncTimeoutSeconds=60),
            )
        self.assertEqual(1, len(results))
        self.assert_sha1_error(
            results[0], "adir: Is a directory", EdenErrorType.POSIX_ERROR, EISDIR
        )

    async def test_get_blake3_throws_for_directory(self) -> None:
        async with self.get_async_thrift_client() as client:
            results = await client.getBlake3(
                self.mount_path_bytes,
                [b"adir"],
                sync=SyncBehavior(syncTimeoutSeconds=60),
            )
        self.assertEqual(1, len(results))
        self.assert_blake3_error(
            results[0], "adir: Is a directory", EdenErrorType.POSIX_ERROR, EISDIR
        )

    async def test_get_sha1_throws_for_non_existent_file(self) -> None:
        async with self.get_async_thrift_client() as client:
            results = await client.getSHA1(
                self.mount_path_bytes, [b"i_do_not_exist"], sync=SyncBehavior()
            )
        self.assertEqual(1, len(results))
        self.assert_sha1_error(
            results[0],
            "i_do_not_exist: No such file or directory",
            EdenErrorType.POSIX_ERROR,
            ENOENT,
        )

    async def test_get_blake3_throws_for_non_existent_file(self) -> None:
        async with self.get_async_thrift_client() as client:
            results = await client.getBlake3(
                self.mount_path_bytes, [b"i_do_not_exist"], sync=SyncBehavior()
            )
        self.assertEqual(1, len(results))
        self.assert_blake3_error(
            results[0],
            "i_do_not_exist: No such file or directory",
            EdenErrorType.POSIX_ERROR,
            ENOENT,
        )

    async def test_get_digest_hash_throws_for_non_existent_file(self) -> None:
        async with self.get_async_thrift_client() as client:
            results = await client.getDigestHash(
                self.mount_path_bytes, [b"i_do_not_exist"], sync=SyncBehavior()
            )
        self.assertEqual(1, len(results))
        self.assert_digest_hash_error(
            results[0],
            "i_do_not_exist: No such file or directory",
            EdenErrorType.POSIX_ERROR,
            ENOENT,
        )

    async def test_get_sha1_throws_for_symlink(self) -> None:
        """Fails because caller should resolve the symlink themselves."""
        async with self.get_async_thrift_client() as client:
            results = await client.getSHA1(
                self.mount_path_bytes, [b"slink"], sync=SyncBehavior()
            )
        self.assertEqual(1, len(results))
        self.assert_sha1_error(
            results[0],
            "slink: file is a symlink: Invalid argument",
            EdenErrorType.POSIX_ERROR,
            EINVAL,
        )

    async def test_get_blake3_throws_for_symlink(self) -> None:
        """Fails because caller should resolve the symlink themselves."""
        async with self.get_async_thrift_client() as client:
            results = await client.getBlake3(
                self.mount_path_bytes, [b"slink"], sync=SyncBehavior()
            )
        self.assertEqual(1, len(results))
        self.assert_blake3_error(
            results[0],
            "slink: file is a symlink: Invalid argument",
            EdenErrorType.POSIX_ERROR,
            EINVAL,
        )

    async def test_get_digest_hash_throws_for_materialized_directory(self) -> None:
        await self._ensure_blake3_computed()
        # Materialize a file in a nested directory
        self.mkdir("adir2")
        # Reuse contents of "hello" file so we don't need to calculate another blake3
        self.write_file("adir2/file", "hola\n")

        async with self.get_async_thrift_client() as client:
            results = await client.getDigestHash(
                self.mount_path_bytes, [b"adir2", b"adir2/file"], sync=SyncBehavior()
            )
        self.assertEqual(2, len(results))
        self.assert_digest_hash_error(
            results[0],
            "tree aux data missing for tree",
            EdenErrorType.ATTRIBUTE_UNAVAILABLE,
            ENOENT,
        )
        self.assertEqual(
            results[1],
            DigestHashResult(digestHash=self.expected_hello_blake3),
        )

    async def test_get_digest_hash_throws_for_local_directory(self) -> None:
        await self._ensure_blake3_computed()
        # By default, the checked out revision for eagerepos is the latest commit pushed to the
        # server. There's no way to do source control operations in generic Eden tests, so we
        # will clone a new repo w/ the specified local commit instead
        new_clone = Path(self.make_temporary_directory())
        self.eden.run_cmd(
            "clone", "--rev", self.local_commit, self.repo.path, str(new_clone)
        )

        async with self.get_async_thrift_client() as client:
            results = await client.getDigestHash(
                bytes(new_clone),
                [b"local_dir", b"local_dir/file"],
                sync=SyncBehavior(),
            )
        print(results)
        self.assertEqual(2, len(results))
        self.assert_digest_hash_error(
            results[0],
            "tree aux data missing for tree",
            EdenErrorType.ATTRIBUTE_UNAVAILABLE,
            ENOENT,
        )
        self.assertEqual(
            results[1],
            DigestHashResult(digestHash=self.expected_hello_blake3),
        )

    def assert_eden_error(
        self,
        result: EdenThriftResult,
        error_message: Union[str, Pattern],
        error_type: Optional[EdenErrorType] = None,
        error_code: Optional[int] = None,
    ) -> None:
        error = result.error
        self.assertIsNotNone(error)
        if isinstance(error_message, str):
            self.assertEqual(error_message, error.message)
        else:
            self.assertRegex(error.message, error_message)

        if error_type is not None:
            self.assertEqual(error.errorType, error_type)

        if error_code is not None:
            self.assertEqual(error.errorCode, error_code)

    def assert_sha1_error(
        self,
        sha1result: SHA1Result,
        error_message: Union[str, Pattern],
        error_type: Optional[EdenErrorType] = None,
        error_code: Optional[int] = None,
    ) -> None:
        self.assertIsNotNone(sha1result, msg="Must pass a SHA1Result")
        self.assertEqual(
            SHA1Result.Type.error, sha1result.type, msg="SHA1Result must be an error"
        )
        self.assert_eden_error(sha1result, error_message, error_type, error_code)

    def assert_digest_hash_error(
        self,
        digest_hash_result: DigestHashResult,
        error_message: Union[str, Pattern],
        error_type: Optional[EdenErrorType] = None,
        error_code: Optional[int] = None,
    ) -> None:
        self.assertIsNotNone(digest_hash_result, msg="Must pass a DigestHashResult")
        self.assertEqual(
            DigestHashResult.Type.error,
            digest_hash_result.type,
            msg="DigestHashResult must be an error",
        )
        self.assert_eden_error(
            digest_hash_result, error_message, error_type, error_code
        )

    def assert_blake3_error(
        self,
        blake3_result: Blake3Result,
        error_message: Union[str, Pattern],
        error_type: Optional[EdenErrorType] = None,
        error_code: Optional[int] = None,
    ) -> None:
        self.assertIsNotNone(blake3_result, msg="Must pass a Blake3Result")
        self.assertEqual(
            Blake3Result.Type.error,
            blake3_result.type,
            msg="Blake3Result must be an error",
        )
        self.assert_eden_error(blake3_result, error_message, error_type, error_code)

    async def test_unload_free_inodes(self) -> None:
        for i in range(100):
            self.write_file("testfile%d.txt" % i, "unload test case")

        inode_count_before_unload = await self.get_loaded_inodes_count("")
        self.assertGreater(
            inode_count_before_unload, 100, "Number of loaded inodes should increase"
        )

        age = TimeSpec(seconds=0, nanoSeconds=0)
        async with self.get_async_thrift_client() as client:
            unload_count = await client.unloadInodeForPath(
                self.mount_path_bytes, b"", age
            )

        self.assertGreaterEqual(
            unload_count, 100, "Number of loaded inodes should reduce after unload"
        )

    async def test_unload_thrift_api_accepts_single_dot_as_root(self) -> None:
        self.write_file("testfile.txt", "unload test case")

        age = TimeSpec(seconds=0, nanoSeconds=0)
        async with self.get_async_thrift_client() as client:
            unload_count = await client.unloadInodeForPath(
                self.mount_path_bytes, b".", age
            )

        self.assertGreater(
            unload_count, 0, "Number of loaded inodes should reduce after unload"
        )

    def get_counter(self, name: str) -> float:
        return self.get_counters()[name]

    async def test_diff_revisions(self) -> None:
        # Convert the commit hashes to binary for the thrift call
        async with self.get_async_thrift_client() as client:
            diff = await client.getScmStatusBetweenRevisions(
                os.fsencode(self.mount),
                binascii.unhexlify(self.commit1),
                binascii.unhexlify(self.commit2),
            )

        self.assertDictEqual(dict(diff.errors), {})
        self.assertDictEqual(
            dict(diff.entries),
            {
                b"cdir/subdir/new.txt": ScmFileStatus.ADDED,
                b"bdir/file": ScmFileStatus.MODIFIED,
                b"README": ScmFileStatus.REMOVED,
            },
        )

    async def test_diff_revisions_hex(self) -> None:
        # Watchman currently calls getScmStatusBetweenRevisions()
        # with 40-byte hexadecimal commit IDs, so make sure that works.
        async with self.get_async_thrift_client() as client:
            diff = await client.getScmStatusBetweenRevisions(
                os.fsencode(self.mount),
                self.commit1.encode("utf-8"),
                self.commit2.encode("utf-8"),
            )

        self.assertDictEqual(dict(diff.errors), {})
        self.assertDictEqual(
            dict(diff.entries),
            {
                b"cdir/subdir/new.txt": ScmFileStatus.ADDED,
                b"bdir/file": ScmFileStatus.MODIFIED,
                b"README": ScmFileStatus.REMOVED,
            },
        )

    async def test_diff_revisions_with_reverted_file(self) -> None:
        # Convert the commit hashes to binary for the thrift call
        async with self.get_async_thrift_client() as client:
            diff = await client.getScmStatusBetweenRevisions(
                os.fsencode(self.mount),
                binascii.unhexlify(self.commit1),
                binascii.unhexlify(self.commit3),
            )

        self.assertDictEqual(dict(diff.errors), {})
        # bdir/file was modified twice between commit1 and commit3 but had a
        # net change of 0 so it should not be reported in the diff results
        self.assertDictEqual(
            dict(diff.entries),
            {
                b"cdir/subdir/new.txt": ScmFileStatus.ADDED,
                b"README": ScmFileStatus.REMOVED,
            },
        )
