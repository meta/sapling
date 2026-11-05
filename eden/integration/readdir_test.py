#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-unsafe

import hashlib
import os
import re
import socket
import stat
import subprocess
import sys
from errno import EINVAL, EISDIR, ENOENT
from pathlib import Path
from typing import Dict, List, Optional, Pattern, Tuple, Union

from eden.fs.service.eden.thrift_types import (
    AclInfo,
    AclInfoOrError,
    Blake3OrError,
    DigestHashOrError,
    DigestSizeOrError,
    DirListAttributeDataOrError,
    EdenError,
    EdenErrorType,
    FileAttributeDataOrErrorV2,
    FileAttributeDataV2,
    FileAttributes,
    GetAttributesFromFilesParams,
    GetAttributesFromFilesResultV2,
    GetConfigParams,
    ModeOrError,
    MtimeOrError,
    ObjectIdOrError,
    ReaddirParams,
    ReaddirResult,
    Sha1OrError,
    SizeOrError,
    SourceControlType,
    SourceControlTypeOrError,
    SyncBehavior,
    TimeSpec,
    UnderAclOrError,
)

from .lib import testcase
from .lib.find_executables import FindExe
from .lib.util import stat_mtime_to_timespec

# Change this if more attributes are added
ALL_ATTRIBUTES = (
    FileAttributes.FILE_SIZE
    | FileAttributes.SHA1_HASH
    | FileAttributes.SOURCE_CONTROL_TYPE
    | FileAttributes.OBJECT_ID
    | FileAttributes.BLAKE3_HASH
    | FileAttributes.DIGEST_SIZE
    | FileAttributes.DIGEST_HASH
    | FileAttributes.MTIME
    | FileAttributes.MODE
    | FileAttributes.UNDER_ACL
    | FileAttributes.ACLs
)


class RawObjectId:
    raw_oid: bytes
    raw_scm_type: SourceControlType

    def __init__(self, raw_oid: bytes, raw_scm_type: SourceControlType) -> None:
        self.raw_oid = raw_oid
        self.raw_scm_type = raw_scm_type


@testcase.eden_repo_test
class ReaddirTest(testcase.EdenRepoTest):
    commit1: str = ""
    commit2: str = ""
    commit3: str = ""

    adir_file_id: bytes = b""
    bdir_file_id: bytes = b""
    hello_id: bytes = b""
    slink_id: bytes = b""
    adir_id: bytes = b""
    adir_digest_size_result: DigestSizeOrError = DigestSizeOrError()
    adir_digest_hash_result: DigestHashOrError = DigestHashOrError()
    cdir_subdir_id: bytes = b""
    cdir_subdir_digest_size_result: DigestSizeOrError = DigestSizeOrError()
    cdir_subdir_digest_hash_result: DigestHashOrError = DigestHashOrError()

    def edenfs_extra_config(self) -> Optional[Dict[str, List[str]]]:
        result = super().edenfs_extra_config() or {}
        result.setdefault("hash", []).append(
            'blake3-key = "20220728-2357111317192329313741#"'
        )
        return result

    async def blake3_hash(self, file_path: str) -> bytes:
        key: Optional[str] = None
        async with self.get_async_thrift_client() as client:
            config = await client.getConfig(GetConfigParams())
            maybe_key = config.values.get("hash:blake3-key")
            key = (
                maybe_key.parsedValue
                if maybe_key is not None and maybe_key.parsedValue != ""
                else None
            )

            print(f"Resolved key: {maybe_key}, actual key: {key}")

        cmd = [FindExe.BLAKE3_SUM, "--file", file_path]
        if key is not None:
            cmd.extend(["--key", key])

        p = subprocess.run(
            cmd, stdout=subprocess.PIPE, stdin=subprocess.PIPE, encoding="ascii"
        )
        assert p.returncode == 0, "0 exit code is expected for blake3_sum"
        return bytes.fromhex(p.stdout)

    def convert_raw_oid_to_foid(self, path: Path, oid: RawObjectId) -> bytes:
        """Converts an expected ObjectID into a FilteredObjectID. For non-FFS
        repos, this is a no-op. For FFS repos, we attach the appropriate type
        and filter_id to the oid. We assume no filter_id is active in our
        integration tests.
        """

        def get_type_bytes(scm_type: SourceControlType) -> bytes:
            """Determines a FilteredObjectIdType based on a given SCM Type.
            The list of FilteredObjectIdTypes can be found here: fs/store/filter/FilteredObjectId.h
            """
            kRecursivelyUnfilteredTreeType = "18"
            kUnfilteredBlobType = "16"

            # No filters are activated for integration tests, so we can assume
            # every tree is unfiltered. If that changes, we would have to
            # evaluate whether the given path is included in the filter or not.
            if scm_type == SourceControlType.TREE:
                return kRecursivelyUnfilteredTreeType.encode("utf-8")
            # The FilteredBackingStore only supports tree and blob objects
            elif scm_type == SourceControlType.UNKNOWN:
                raise ValueError("cannot create a foid with an unknown scm type")
            # All other types evaluate to a blob
            else:
                return kUnfilteredBlobType.encode("utf-8")

        if self.backing_store_type == "filteredhg":
            if path.parent != Path("."):
                # With unfiltered fast path (see D82507321), filteredfs uses the underlying ObjectId for
                # unfiltered trees. But, the root tree is always wrapped, so return underlying id if we
                # aren't in the root tree.
                return oid.raw_oid
            type_bytes = get_type_bytes(oid.raw_scm_type)
            return type_bytes + ":".encode("utf-8") + oid.raw_oid
        else:
            return oid.raw_oid

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
        self.repo.push(rev=".", target="master", create=True)

        self.adir_file_id = self.convert_raw_oid_to_foid(
            Path("adir/file"),
            RawObjectId(
                b"41825fd37af5796284289a1e0770ccd3d27d4832:adir/file",
                SourceControlType.REGULAR_FILE,
            ),
        )

        self.bdir_file_id = self.convert_raw_oid_to_foid(
            Path("bdir/file"),
            RawObjectId(
                b"e5336dae10d1e7590fc25db9d417d089295875e0:bdir/file",
                SourceControlType.REGULAR_FILE,
            ),
        )

        self.hello_id = self.convert_raw_oid_to_foid(
            Path("hello"),
            RawObjectId(
                b"edd9bdab9ab7a84b21a7a19fffe7a29709ac3b47:hello",
                SourceControlType.REGULAR_FILE,
            ),
        )

        self.slink_id = self.convert_raw_oid_to_foid(
            Path("slink"),
            RawObjectId(
                b"7fac34a232926c628f2d890d3eed95be7ab57f34:slink",
                SourceControlType.SYMLINK,
            ),
        )

        self.adir_id = self.convert_raw_oid_to_foid(
            Path("adir"),
            RawObjectId(
                b"6ae9e90c9c90aa85adbdab16808203e64a5163cb:adir",
                SourceControlType.TREE,
            ),
        )

        # There is no easy way to compute the blake3/size of a directory on the fly (in Python)
        # Since these hashes/sizes should stay constant, we can just hardcode the expected result
        adir_digest_size = 207
        adir_blake3 = b"\x73\xf0\xc6\xe3\x6b\x3c\xb9\xfc\x64\xa8\xa3\x39\x24\x57\xd3\xc9\xd0\x2d\x11\xfd\x22\xe5\x36\x71\x94\x5d\x95\x3f\xfa\xc3\x8c\x92"
        self.adir_digest_size_result = DigestSizeOrError(digestSize=adir_digest_size)

        self.adir_digest_hash_result = DigestHashOrError(digestHash=adir_blake3)

        self.cdir_subdir_id = self.convert_raw_oid_to_foid(
            Path("cdir/subdir"),
            RawObjectId(
                b"cd765cb479197becdca10a4baa87e983244bf24f:cdir/subdir",
                SourceControlType.TREE,
            ),
        )

        # There is no easy way to compute the blake3/size of a directory on the fly (in Python)
        # Since these hashes/sizes should stay constant, we can just hardcode the expected result
        cdir_subdir_digest_size = 211
        cdir_subdir_blake3 = b"\x2f\xe0\x36\xd6\xf0\x6a\xf6\xb5\x60\xe7\xe1\xf4\x95\x1c\x9c\xd7\xf1\x62\x08\x32\xa2\xee\xb8\x42\x16\x9b\xd4\xe7\x7b\x83\x0a\x94"
        self.cdir_subdir_digest_size_result = DigestSizeOrError(
            digestSize=cdir_subdir_digest_size
        )

        self.cdir_subdir_digest_hash_result = DigestHashOrError(
            digestHash=cdir_subdir_blake3
        )

    def assert_eden_error(
        self,
        result: FileAttributeDataOrErrorV2,
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
            self.assertEqual(error_type, error.errorType)

        if error_code is not None:
            self.assertEqual(error_code, error.errorCode)

    async def get_attributes_v2(
        self, files: List[bytes], req_attr: int
    ) -> GetAttributesFromFilesResultV2:
        async with self.get_async_thrift_client() as client:
            thrift_params = GetAttributesFromFilesParams(
                mountPoint=self.mount_path_bytes,
                paths=files,
                requestedAttributes=req_attr,
            )
            return await client.getAttributesFromFilesV2(thrift_params)

    async def get_all_attributes(
        self, files: List[bytes]
    ) -> GetAttributesFromFilesResultV2:
        return await self.get_attributes_v2(files, ALL_ATTRIBUTES)

    def under_acl_false(self) -> UnderAclOrError:
        return UnderAclOrError(underAcl=False)

    def constructReaddirUnderAcl(
        self,
        req_attr: int,
        expected_under_acl: Optional[UnderAclOrError],
    ) -> Optional[UnderAclOrError]:
        if not req_attr & FileAttributes.UNDER_ACL:
            return None
        return expected_under_acl

    def constructReaddirAclInfo(
        self,
        req_attr: int,
        expected_acl_info: Optional[AclInfoOrError],
    ) -> Optional[AclInfoOrError]:
        if not req_attr & FileAttributes.ACLs:
            return None
        return expected_acl_info

    def expected_under_acl(self) -> UnderAclOrError:
        return self.under_acl_false()

    def expected_readdir_under_acl(self) -> UnderAclOrError:
        return self.under_acl_false()

    def acl_info_false(self) -> AclInfoOrError:
        return AclInfoOrError(aclInfo=AclInfo(underAcl=False, acls=[]))

    def expected_acl_info(self) -> AclInfoOrError:
        return self.acl_info_false()

    def expected_readdir_acl_info(self) -> AclInfoOrError:
        return self.acl_info_false()

    def wrap_expected_attributes(
        self,
        raw_attributes: Tuple[
            Optional[bytes],
            Optional[int],
            Optional[SourceControlType],
            Optional[bytes],
            Optional[bytes],
            Optional[int],
            Optional[bytes],
            Optional[TimeSpec],
            Optional[int],
            Optional[UnderAclOrError],
            Optional[AclInfoOrError],
        ],
    ) -> FileAttributeDataOrErrorV2:
        (
            raw_sha1,
            raw_size,
            raw_type,
            raw_object_id,
            raw_blake3,
            digest_size,
            digest_hash,
            raw_mtime,
            raw_mode,
            under_acl,
            acl_info,
        ) = raw_attributes
        data = FileAttributeDataV2(
            sha1=Sha1OrError(sha1=raw_sha1) if raw_sha1 is not None else None,
            blake3=Blake3OrError(blake3=raw_blake3) if raw_blake3 is not None else None,
            size=SizeOrError(size=raw_size) if raw_size is not None else None,
            sourceControlType=SourceControlTypeOrError(sourceControlType=raw_type)
            if raw_type is not None
            else None,
            objectId=ObjectIdOrError(objectId=raw_object_id)
            if raw_object_id is not None
            else None,
            digestSize=DigestSizeOrError(digestSize=digest_size)
            if digest_size is not None
            else None,
            digestHash=DigestHashOrError(digestHash=digest_hash)
            if digest_hash is not None
            else None,
            mtime=MtimeOrError(mtime=raw_mtime) if raw_mtime is not None else None,
            mode=ModeOrError(mode=raw_mode) if raw_mode is not None else None,
            underAcl=under_acl,
            aclInfo=acl_info,
        )

        return FileAttributeDataOrErrorV2(fileAttributeData=data)

    async def assert_attribute_result(
        self,
        fn,
        expected_result,
        paths,
        attributes: int = ALL_ATTRIBUTES,
    ) -> None:
        print("expected: \n{}", expected_result)
        actual_result = await fn(paths, attributes)
        print("actual: \n{}", actual_result)
        self.assertEqual(len(paths), len(actual_result.res))
        self.assertEqual(
            expected_result,
            actual_result,
        )

    async def assert_attributes_result(
        self,
        expected_result,
        paths,
        attributes: int = ALL_ATTRIBUTES,
    ) -> None:
        await self.assert_attribute_result(
            self.get_attributes_v2, expected_result, paths, attributes=attributes
        )

    async def test_get_attributes(self) -> None:
        # expected results for file named "hello"

        expected_hello_result = self.wrap_expected_attributes(
            await self.get_expected_file_attributes(
                "hello",
                self.hello_id,
            )
        )

        # expected results for file "adir/file"
        expected_adir_result = self.wrap_expected_attributes(
            await self.get_expected_file_attributes(
                "adir/file",
                self.adir_file_id,
            )
        )

        # list of expected_results
        expected_result = GetAttributesFromFilesResultV2(
            res=[
                expected_hello_result,
                expected_adir_result,
            ]
        )

        await self.assert_attributes_result(expected_result, [b"hello", b"adir/file"])

    async def test_get_size_only(self) -> None:
        # expected size result for file
        expected_hello_size = await self.get_expected_file_attributes("hello", None)

        expected_hello_result = self.wrap_expected_attributes(
            (
                None,
                expected_hello_size[1],
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
        )

        # create result object for "hello"
        expected_result = GetAttributesFromFilesResultV2(
            res=[
                expected_hello_result,
            ]
        )

        await self.assert_attributes_result(
            expected_result, [b"hello"], FileAttributes.FILE_SIZE
        )

    async def test_get_type_only(self) -> None:
        # expected size result for file
        expected_hello_type = await self.get_expected_file_attributes("hello", None)
        expected_hello_result = self.wrap_expected_attributes(
            (
                None,
                None,
                expected_hello_type[2],
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
        )

        # create result object for "hello"
        expected_result = GetAttributesFromFilesResultV2(
            res=[
                expected_hello_result,
            ]
        )

        await self.assert_attributes_result(
            expected_result,
            [b"hello"],
            FileAttributes.SOURCE_CONTROL_TYPE,
        )

    async def test_get_attributes_throws_for_non_existent_file(self) -> None:
        results = await self.get_all_attributes([b"i_do_not_exist"])
        self.assertEqual(1, len(results.res))
        self.assert_attribute_error(
            results,
            "i_do_not_exist: No such file or directory",
            0,
            error_type=EdenErrorType.POSIX_ERROR,
            error_code=ENOENT,
        )

    async def test_get_sha1_only(self) -> None:
        # expected sha1 result for file
        expected_hello_sha1 = await self.get_expected_file_attributes("hello", None)

        expected_hello_result = self.wrap_expected_attributes(
            (
                expected_hello_sha1[0],
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
        )

        # create result object for "hello"
        expected_result = GetAttributesFromFilesResultV2(
            res=[
                expected_hello_result,
            ]
        )

        await self.assert_attributes_result(
            expected_result,
            [b"hello"],
            FileAttributes.SHA1_HASH,
        )

    async def test_get_blake3_only(self) -> None:
        # expected blake3 result for file
        expected_hello_blake3 = await self.get_expected_file_attributes("hello", None)

        expected_hello_result = self.wrap_expected_attributes(
            (
                None,
                None,
                None,
                None,
                expected_hello_blake3[4],
                None,
                None,
                None,
                None,
                None,
                None,
            )
        )

        # create result object for "hello"
        expected_result = GetAttributesFromFilesResultV2(
            res=[
                expected_hello_result,
            ]
        )

        await self.assert_attributes_result(
            expected_result,
            [b"hello"],
            FileAttributes.BLAKE3_HASH,
        )

    async def test_get_mtime_only(self) -> None:
        # expected mtime result for file
        expected_hello_mtime = await self.get_expected_file_attributes("hello", None)
        expected_hello_result = self.wrap_expected_attributes(
            (
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                expected_hello_mtime[7],
                None,
                None,
                None,
            )
        )

        # create result object for "hello"
        expected_result = GetAttributesFromFilesResultV2(
            res=[
                expected_hello_result,
            ]
        )

        await self.assert_attributes_result(
            expected_result,
            [b"hello"],
            FileAttributes.MTIME,
        )

    async def test_get_mode_only(self) -> None:
        # expected mtime result for file
        expected_hello_mode = await self.get_expected_file_attributes("hello", None)

        expected_hello_result = self.wrap_expected_attributes(
            (
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                expected_hello_mode[8],
                None,
                None,
            )
        )

        # create result object for "hello"
        expected_result = GetAttributesFromFilesResultV2(
            res=[
                expected_hello_result,
            ]
        )

        await self.assert_attributes_result(
            expected_result,
            [b"hello"],
            FileAttributes.MODE,
        )

    async def test_get_attributes_throws_for_path_with_dot_components(self) -> None:
        results = await self.get_all_attributes([b"./hello"])
        self.assertEqual(1, len(results.res))
        self.assert_attribute_error(
            results,
            re.compile(r"PathComponent must not be \."),
            0,
            error_type=EdenErrorType.ARGUMENT_ERROR,
            error_code=None,
        )

    async def test_get_attributes_throws_for_empty_string(self) -> None:
        results = await self.get_all_attributes([b""])
        self.assertEqual(1, len(results.res))
        self.assert_attribute_error(
            results,
            "path cannot be the empty string",
            0,
            error_type=EdenErrorType.ARGUMENT_ERROR,
            error_code=None,
        )

    async def test_get_attributes_directory(self) -> None:
        stat = self.stat("adir")
        (mode, mtime) = self.get_expected_mode_and_mtime(stat)

        expected_adir_result = FileAttributeDataOrErrorV2(
            fileAttributeData=FileAttributeDataV2(
                sha1=Sha1OrError(
                    error=EdenError(
                        message="adir: Is a directory",
                        errorCode=EISDIR,
                        errorType=EdenErrorType.POSIX_ERROR,
                    )
                ),
                size=SizeOrError(
                    error=EdenError(
                        message="adir: Is a directory",
                        errorCode=EISDIR,
                        errorType=EdenErrorType.POSIX_ERROR,
                    )
                ),
                sourceControlType=SourceControlTypeOrError(
                    sourceControlType=SourceControlType.TREE
                ),
                objectId=ObjectIdOrError(objectId=self.adir_id),
                blake3=Blake3OrError(
                    error=EdenError(
                        message="adir: Is a directory",
                        errorCode=EISDIR,
                        errorType=EdenErrorType.POSIX_ERROR,
                    )
                ),
                digestSize=self.adir_digest_size_result,
                digestHash=self.adir_digest_hash_result,
                mtime=MtimeOrError(mtime=mtime),
                mode=ModeOrError(mode=mode),
                underAcl=self.expected_under_acl(),
                aclInfo=self.expected_acl_info(),
            )
        )

        expected_result = GetAttributesFromFilesResultV2(
            res=[
                expected_adir_result,
            ]
        )
        print(f"expected: \n{expected_result}")
        results = await self.get_all_attributes([b"adir"])
        print(f"actual: \n{results}")
        self.assertEqual(1, len(results.res))
        self.assertEqual(expected_result, results)

    async def test_get_attributes_socket(self) -> None:
        sockpath = self.get_path("adir/asock")
        # UDS are not supported in python on Win until 3.9:
        # https://bugs.python.org/issue33408
        with socket.socket(socket.AF_UNIX) as sock:
            sock.bind(sockpath)

            stat = self.stat("adir/asock")
            (mode, mtime) = self.get_expected_mode_and_mtime(stat)
            expected_adir_result = FileAttributeDataOrErrorV2(
                fileAttributeData=FileAttributeDataV2(
                    sha1=Sha1OrError(
                        error=EdenError(
                            message="adir/asock: file is a non-source-control type: 12: Invalid argument",
                            errorCode=EINVAL,
                            errorType=EdenErrorType.POSIX_ERROR,
                        )
                    ),
                    size=SizeOrError(
                        error=EdenError(
                            message="adir/asock: file is a non-source-control type: 12: Invalid argument",
                            errorCode=EINVAL,
                            errorType=EdenErrorType.POSIX_ERROR,
                        )
                    ),
                    sourceControlType=SourceControlTypeOrError(
                        sourceControlType=SourceControlType.UNKNOWN
                    ),
                    objectId=ObjectIdOrError(objectId=None),
                    blake3=Blake3OrError(
                        error=EdenError(
                            message="adir/asock: file is a non-source-control type: 12: Invalid argument",
                            errorCode=EINVAL,
                            errorType=EdenErrorType.POSIX_ERROR,
                        )
                    ),
                    digestSize=DigestSizeOrError(
                        error=EdenError(
                            message="adir/asock: file is a non-source-control type: 12: Invalid argument",
                            errorCode=EINVAL,
                            errorType=EdenErrorType.POSIX_ERROR,
                        )
                    ),
                    digestHash=DigestHashOrError(
                        error=EdenError(
                            message="adir/asock: file is a non-source-control type: 12: Invalid argument",
                            errorCode=EINVAL,
                            errorType=EdenErrorType.POSIX_ERROR,
                        )
                    ),
                    mtime=MtimeOrError(mtime=mtime),
                    mode=ModeOrError(mode=mode),
                    underAcl=self.expected_under_acl(),
                    aclInfo=self.expected_acl_info(),
                )
            )

            expected_result = GetAttributesFromFilesResultV2(
                res=[
                    expected_adir_result,
                ]
            )
            print(f"expected: \n{expected_result}")
            results = await self.get_all_attributes([b"adir/asock"])
            print(f"actual: \n{results}")
            self.assertEqual(1, len(results.res))
            self.assertEqual(expected_result, results)

    async def test_get_attributes_symlink(self) -> None:
        stat = self.stat("slink")
        (mode, mtime) = self.get_expected_mode_and_mtime(stat)
        expected_slink_result = FileAttributeDataOrErrorV2(
            fileAttributeData=FileAttributeDataV2(
                sha1=Sha1OrError(
                    error=EdenError(
                        message="slink: file is a symlink: Invalid argument",
                        errorCode=EINVAL,
                        errorType=EdenErrorType.POSIX_ERROR,
                    )
                ),
                size=SizeOrError(
                    error=EdenError(
                        message="slink: file is a symlink: Invalid argument",
                        errorCode=EINVAL,
                        errorType=EdenErrorType.POSIX_ERROR,
                    )
                ),
                sourceControlType=SourceControlTypeOrError(
                    sourceControlType=SourceControlType.SYMLINK
                ),
                objectId=ObjectIdOrError(objectId=self.slink_id),
                blake3=Blake3OrError(
                    error=EdenError(
                        message="slink: file is a symlink: Invalid argument",
                        errorCode=EINVAL,
                        errorType=EdenErrorType.POSIX_ERROR,
                    )
                ),
                digestSize=DigestSizeOrError(
                    error=EdenError(
                        message="slink: file is a symlink: Invalid argument",
                        errorCode=EINVAL,
                        errorType=EdenErrorType.POSIX_ERROR,
                    )
                ),
                digestHash=DigestHashOrError(
                    error=EdenError(
                        message="slink: file is a symlink: Invalid argument",
                        errorCode=EINVAL,
                        errorType=EdenErrorType.POSIX_ERROR,
                    )
                ),
                mtime=MtimeOrError(mtime=mtime),
                mode=ModeOrError(mode=mode),
                underAcl=self.expected_under_acl(),
                aclInfo=self.expected_acl_info(),
            )
        )

        expected_result = GetAttributesFromFilesResultV2(
            res=[
                expected_slink_result,
            ]
        )
        print(f"expected: \n{expected_result}")
        results = await self.get_all_attributes([b"slink"])
        print(f"actual: \n{results}")
        self.assertEqual(1, len(results.res))
        self.assertEqual(expected_result, results)

    async def test_get_attributes_no_files(self) -> None:
        results = await self.get_all_attributes([])
        self.assertEqual(0, len(results.res))

    async def test_get_no_attributes(self) -> None:
        expected_hello_result = FileAttributeDataOrErrorV2(
            fileAttributeData=FileAttributeDataV2()
        )

        # create result object for "hello"
        expected_result = GetAttributesFromFilesResultV2(
            res=[
                expected_hello_result,
            ]
        )

        await self.assert_attributes_result(
            expected_result,
            [b"hello"],
            0,
        )

    async def test_get_materialized_attributes(self) -> None:
        # Materialize some files/dirs
        self.write_file("hello_file", "something different")
        self.mkdir("hello_dir")

        # Materialized files should not have ObjectID values, but they should
        # have valid digest hash/size values
        blake3_hash_result = await self.blake3_hash(self.get_path("hello_file"))
        expected_hello_result_file = FileAttributeDataOrErrorV2(
            fileAttributeData=FileAttributeDataV2(
                digestHash=DigestHashOrError(digestHash=blake3_hash_result),
                digestSize=DigestSizeOrError(digestSize=19),
                objectId=ObjectIdOrError(),
            )
        )

        # Materialized directories will not have digest hash/size values and
        # return an error as a result. ObjectId is also not valid, but returns
        # empty ObjectID because this legacy behavior is expected by Meerkat
        expected_hello_result_dir = FileAttributeDataOrErrorV2(
            fileAttributeData=FileAttributeDataV2(
                digestHash=DigestHashOrError(
                    error=EdenError(
                        message="hello_dir: digesthash requested, but no digesthash available",
                        errorType=EdenErrorType.ATTRIBUTE_UNAVAILABLE,
                        errorCode=ENOENT,
                    )
                ),
                digestSize=DigestSizeOrError(
                    error=EdenError(
                        message="hello_dir: digestsize requested, but no digestsize available",
                        errorType=EdenErrorType.ATTRIBUTE_UNAVAILABLE,
                        errorCode=ENOENT,
                    )
                ),
                objectId=ObjectIdOrError(),
            )
        )

        expected_results = GetAttributesFromFilesResultV2(
            res=[expected_hello_result_file, expected_hello_result_dir]
        )

        await self.assert_attribute_result(
            self.get_attributes_v2,
            expected_results,
            [b"hello_file", b"hello_dir"],
            attributes=(
                FileAttributes.DIGEST_HASH
                | FileAttributes.DIGEST_SIZE
                | FileAttributes.OBJECT_ID
            ),
        )

    def assert_attribute_error(
        self,
        attribute_result: GetAttributesFromFilesResultV2,
        error_message: Union[str, Pattern],
        map_entry: int,
        error_type: Optional[EdenErrorType] = None,
        error_code: Optional[int] = None,
    ) -> None:
        self.assertIsNotNone(
            attribute_result, msg="Must pass a GetAttributesFromFilesResult"
        )
        attr_or_err = attribute_result.res[map_entry]
        self.assertIsNotNone(
            attr_or_err.error,
            msg="GetAttributesFromFilesResultV2 must be an error",
        )
        self.assert_eden_error(
            attr_or_err,
            error_message,
            error_type,
            error_code,
        )

    def get_expected_mode_and_mtime(
        self, file_stat: os.stat_result
    ) -> Tuple[int, TimeSpec]:
        """Get the expected mode and mtime given a stat result.
        On Windows, the mode and mtime are always 0.
        """
        if sys.platform == "win32":
            return (0, TimeSpec(seconds=0, nanoSeconds=0))
        else:
            mode = file_stat.st_mode
            mtime = stat_mtime_to_timespec(file_stat)
            return (mode, mtime)

    async def get_expected_file_attributes(
        self,
        path: str,
        object_id: Optional[bytes],
    ) -> Tuple[
        bytes,
        int,
        SourceControlType,
        Optional[bytes],
        bytes,
        int,
        bytes,
        TimeSpec,
        int,
        Optional[UnderAclOrError],
        Optional[AclInfoOrError],
    ]:
        """Get attributes for the file with the specified path inside
        the eden repository. For now, just sha1 and file size.
        """

        file_stat = self.stat(path)

        mode = file_stat.st_mode
        (expected_mode, expected_mtime) = self.get_expected_mode_and_mtime(file_stat)

        file_type = SourceControlType.REGULAR_FILE
        if stat.S_ISDIR(mode):
            return (
                (0).to_bytes(20, byteorder="big"),
                0,
                SourceControlType.TREE,
                object_id,
                (0).to_bytes(32, byteorder="big"),
                0,
                (0).to_bytes(32, byteorder="big"),
                expected_mtime,
                expected_mode,
                self.expected_under_acl(),
                self.expected_acl_info(),
            )
        if stat.S_ISLNK(mode):
            return (
                (0).to_bytes(20, byteorder="big"),
                0,
                SourceControlType.SYMLINK,
                object_id,
                (0).to_bytes(32, byteorder="big"),
                0,
                (0).to_bytes(32, byteorder="big"),
                expected_mtime,
                expected_mode,
                self.expected_under_acl(),
                self.expected_acl_info(),
            )
        if not stat.S_ISREG(mode):
            return (
                (0).to_bytes(20, byteorder="big"),
                0,
                SourceControlType.UNKNOWN,
                object_id,
                (0).to_bytes(32, byteorder="big"),
                0,
                (0).to_bytes(32, byteorder="big"),
                expected_mtime,
                expected_mode,
                self.expected_under_acl(),
                self.expected_acl_info(),
            )
        if stat.S_IXUSR & mode:
            file_type = SourceControlType.EXECUTABLE_FILE
        file_size = file_stat.st_size
        fullpath = self.get_path(path)
        ifile = open(fullpath, "rb")
        file_contents = ifile.read()
        sha1_hash = hashlib.sha1(file_contents).digest()
        ifile.close()
        blake3 = await self.blake3_hash(fullpath)

        return (
            sha1_hash,
            file_size,
            file_type,
            object_id,
            blake3,
            file_size,
            blake3,
            expected_mtime,
            expected_mode,
            self.expected_under_acl(),
            self.expected_acl_info(),
        )

    def get_counter(self, name: str) -> float:
        return self.get_counters()[name]

    def constructReaddirResult(
        self,
        expected_attributes: Tuple[
            bytes,
            int,
            SourceControlType,
            Optional[bytes],
            Optional[bytes],
            int,
            Optional[bytes],
            TimeSpec,
            int,
            Optional[UnderAclOrError],
            Optional[AclInfoOrError],
        ],
        req_attr: int = ALL_ATTRIBUTES,
        entry_path: str = "",
    ) -> FileAttributeDataOrErrorV2:
        sha1 = None
        if req_attr & FileAttributes.SHA1_HASH:
            sha1 = Sha1OrError(sha1=expected_attributes[0])

        size = None
        if req_attr & FileAttributes.FILE_SIZE:
            size = SizeOrError(size=expected_attributes[1])

        sourceControlType = None
        if req_attr & FileAttributes.SOURCE_CONTROL_TYPE:
            sourceControlType = SourceControlTypeOrError(
                sourceControlType=expected_attributes[2]
            )

        objectId = None
        if req_attr & FileAttributes.OBJECT_ID:
            objectId = ObjectIdOrError(objectId=expected_attributes[3])

        blake3 = None
        if (req_attr & FileAttributes.BLAKE3_HASH) and expected_attributes[
            4
        ] is not None:
            blake3 = Blake3OrError(blake3=expected_attributes[4])

        digestSize = None
        if (req_attr & FileAttributes.DIGEST_SIZE) and expected_attributes[5]:
            digestSize = DigestSizeOrError(digestSize=expected_attributes[5])

        digestHash = None
        if (req_attr & FileAttributes.DIGEST_HASH) and expected_attributes[
            6
        ] is not None:
            digestHash = DigestHashOrError(digestHash=expected_attributes[6])

        mtime = None
        if (req_attr & FileAttributes.MTIME) and expected_attributes[7] is not None:
            mtime = MtimeOrError(mtime=expected_attributes[7])

        mode = None
        if (req_attr & FileAttributes.MODE) and expected_attributes[8] is not None:
            mode = ModeOrError(mode=expected_attributes[8])

        under_acl = self.constructReaddirUnderAcl(req_attr, expected_attributes[9])
        acl_info = self.constructReaddirAclInfo(req_attr, expected_attributes[10])

        return FileAttributeDataOrErrorV2(
            fileAttributeData=FileAttributeDataV2(
                sha1=sha1,
                size=size,
                sourceControlType=sourceControlType,
                objectId=objectId,
                blake3=blake3,
                digestSize=digestSize,
                digestHash=digestHash,
                mtime=mtime,
                mode=mode,
                underAcl=under_acl,
                aclInfo=acl_info,
            )
        )

    async def test_readdir(self) -> None:
        # each of these tests should arguably be their own test case,
        # but integration tests are expensive, so we will do it all in one.

        # non empty directories
        async with self.get_async_thrift_client() as client:
            adir_result = DirListAttributeDataOrError(
                dirListAttributeData={
                    b"file": self.constructReaddirResult(
                        await self.get_expected_file_attributes(
                            "adir/file",
                            self.adir_file_id,
                        ),
                        entry_path="adir/file",
                    )
                }
            )
            bdir_result = DirListAttributeDataOrError(
                dirListAttributeData={
                    b"file": self.constructReaddirResult(
                        await self.get_expected_file_attributes(
                            "bdir/file",
                            self.bdir_file_id,
                        ),
                        entry_path="bdir/file",
                    ),
                }
            )

            expected = ReaddirResult(dirLists=[adir_result, bdir_result])
            actual_result = await client.readdir(
                ReaddirParams(
                    mountPoint=self.mount_path_bytes,
                    directoryPaths=[b"adir", b"bdir"],
                    requestedAttributes=ALL_ATTRIBUTES,
                    sync=SyncBehavior(),
                )
            )
            print(f"expected: \n{expected}")
            print(f"actual: \n{actual_result}")
            self.assertEqual(
                expected,
                actual_result,
            )

            # empty directory
            # can't prep this before hand, because the initial setup if for the
            # backing repo, and we can not commit an empty directory, so it be added
            # via the backing repo.
            path = Path(self.mount) / "emptydir"
            os.mkdir(path)

            expected = ReaddirResult(
                dirLists=[DirListAttributeDataOrError(dirListAttributeData={})]
            )
            actual = await client.readdir(
                ReaddirParams(
                    mountPoint=self.mount_path_bytes,
                    directoryPaths=[b"emptydir"],
                    sync=SyncBehavior(),
                )
            )
            print(f"expected: \n{expected}")
            print(f"actual: \n{actual}")
            self.assertEqual(expected, actual)

            # non existent directory
            expected = ReaddirResult(
                dirLists=[
                    DirListAttributeDataOrError(
                        error=EdenError(
                            message="ddir: No such file or directory",
                            errorCode=2,
                            errorType=EdenErrorType.POSIX_ERROR,
                        )
                    )
                ]
            )
            actual = await client.readdir(
                ReaddirParams(
                    mountPoint=self.mount_path_bytes,
                    directoryPaths=[b"ddir"],
                    sync=SyncBehavior(),
                )
            )
            print(f"expected: \n{expected}")
            print(f"actual: \n{actual}")
            self.assertEqual(expected, actual)

            # file
            expected = ReaddirResult(
                dirLists=[
                    DirListAttributeDataOrError(
                        error=EdenError(
                            message="hello: path must be a directory",
                            errorCode=EINVAL,
                            errorType=EdenErrorType.ARGUMENT_ERROR,
                        )
                    )
                ]
            )
            actual = await client.readdir(
                ReaddirParams(
                    mountPoint=self.mount_path_bytes,
                    directoryPaths=[b"hello"],
                    sync=SyncBehavior(),
                )
            )
            print(f"expected: \n{expected}")
            print(f"actual: \n{actual}")
            self.assertEqual(expected, actual)

            # empty string
            actual = await client.readdir(
                ReaddirParams(
                    mountPoint=self.mount_path_bytes,
                    directoryPaths=[b""],
                    sync=SyncBehavior(),
                )
            )
            # access the data to ensure this does not throw and we have legit
            # data in the response
            self.assertIn(b"test_fetch1", actual.dirLists[0].dirListAttributeData)
            self.assertIn(b"hello", actual.dirLists[0].dirListAttributeData)
            self.assertIn(b"cdir", actual.dirLists[0].dirListAttributeData)

    async def test_readdir_invalid_path_fails_only_that_path(self) -> None:
        # An invalid path (e.g. an absolute path) in a readdir batch produces
        # an error in that path's slot of the result and leaves the other
        # paths' listings intact.
        async with self.get_async_thrift_client() as client:
            actual = await client.readdir(
                ReaddirParams(
                    mountPoint=self.mount_path_bytes,
                    directoryPaths=[b"adir", b"/etc"],
                    sync=SyncBehavior(),
                )
            )
            self.assertEqual(2, len(actual.dirLists))
            self.assertIn(b"file", actual.dirLists[0].dirListAttributeData)
            error = actual.dirLists[1].error
            self.assertEqual(EINVAL, error.errorCode)
            self.assertEqual(EdenErrorType.ARGUMENT_ERROR, error.errorType)
            self.assertIn(
                "attempt to construct a RelativePath from an absolute path",
                error.message,
            )

    async def readdir_single_attr_only(self, req_attr: int) -> None:
        async with self.get_async_thrift_client() as client:
            adir_result = DirListAttributeDataOrError(
                dirListAttributeData={
                    b"file": self.constructReaddirResult(
                        await self.get_expected_file_attributes(
                            "adir/file", self.adir_file_id
                        ),
                        req_attr=req_attr,
                        entry_path="adir/file",
                    )
                }
            )
            bdir_result = DirListAttributeDataOrError(
                dirListAttributeData={
                    b"file": self.constructReaddirResult(
                        await self.get_expected_file_attributes(
                            "bdir/file", self.bdir_file_id
                        ),
                        req_attr=req_attr,
                        entry_path="bdir/file",
                    )
                }
            )

            expected = ReaddirResult(dirLists=[adir_result, bdir_result])
            actual_result = await client.readdir(
                ReaddirParams(
                    mountPoint=self.mount_path_bytes,
                    directoryPaths=[b"adir", b"bdir"],
                    requestedAttributes=req_attr,
                    sync=SyncBehavior(),
                )
            )
            print(f"expected: \n{expected}")
            print(f"actual: \n{actual_result}")

            self.assertEqual(
                expected,
                actual_result,
            )

    async def test_readdir_single_attr_only(self) -> None:
        await self.readdir_single_attr_only(FileAttributes.SHA1_HASH)

        await self.readdir_single_attr_only(FileAttributes.BLAKE3_HASH)

        await self.readdir_single_attr_only(FileAttributes.FILE_SIZE)

        await self.readdir_single_attr_only(FileAttributes.SOURCE_CONTROL_TYPE)

    async def readdir_no_size_or_sha1(
        self,
        parent_name: bytes,
        entry_name: bytes,
        source_control_type: SourceControlType,
        sha1_result: Sha1OrError,
        blake3_result: Blake3OrError,
        size_result: SizeOrError,
        object_id: Optional[bytes],
        digest_size_result: DigestSizeOrError,
        digest_hash_result: DigestHashOrError,
        mtime_result=MtimeOrError,
        mode_result=ModeOrError,
    ) -> None:
        async with self.get_async_thrift_client() as client:
            expected = FileAttributeDataOrErrorV2(
                fileAttributeData=FileAttributeDataV2(
                    sha1=sha1_result,
                    size=size_result,
                    sourceControlType=SourceControlTypeOrError(
                        sourceControlType=source_control_type
                    ),
                    objectId=ObjectIdOrError(
                        objectId=object_id,
                    ),
                    blake3=blake3_result,
                    digestSize=digest_size_result,
                    digestHash=digest_hash_result,
                    # pyrefly: ignore [bad-argument-type]
                    mtime=mtime_result,
                    # pyrefly: ignore [bad-argument-type]
                    mode=mode_result,
                    underAcl=self.expected_readdir_under_acl(),
                    aclInfo=self.expected_readdir_acl_info(),
                )
            )

            actual = await client.readdir(
                ReaddirParams(
                    mountPoint=self.mount_path_bytes,
                    directoryPaths=[parent_name],
                    requestedAttributes=ALL_ATTRIBUTES,
                    sync=SyncBehavior(),
                )
            )
            print(f"expected: \n{expected}")
            print(f"actual: \n{actual}")

            self.assertEqual(
                expected,
                actual.dirLists[0].dirListAttributeData[entry_name],
            )

            expected = FileAttributeDataOrErrorV2(
                fileAttributeData=FileAttributeDataV2(
                    sha1=None,
                    size=None,
                    sourceControlType=SourceControlTypeOrError(
                        sourceControlType=source_control_type,
                    ),
                    blake3=None,
                    digestSize=None,
                    digestHash=None,
                    mtime=None,
                    mode=None,
                )
            )

            actual = await client.readdir(
                ReaddirParams(
                    mountPoint=self.mount_path_bytes,
                    directoryPaths=[parent_name],
                    requestedAttributes=FileAttributes.SOURCE_CONTROL_TYPE,
                    sync=SyncBehavior(),
                )
            )
            print(f"expected: \n{expected}")
            print(f"actual: \n{actual}")
            self.assertEqual(
                expected,
                actual.dirLists[0].dirListAttributeData[entry_name],
            )

    async def test_readdir_directory_symlink_and_other(self) -> None:
        stat = self.stat("cdir")
        (mode, mtime) = self.get_expected_mode_and_mtime(stat)
        await self.readdir_no_size_or_sha1(
            parent_name=b"cdir",
            entry_name=b"subdir",
            source_control_type=SourceControlType.TREE,
            sha1_result=Sha1OrError(
                error=EdenError(
                    message="cdir/subdir: Is a directory",
                    errorCode=EISDIR,
                    errorType=EdenErrorType.POSIX_ERROR,
                )
            ),
            blake3_result=Blake3OrError(
                error=EdenError(
                    message="cdir/subdir: Is a directory",
                    errorCode=EISDIR,
                    errorType=EdenErrorType.POSIX_ERROR,
                )
            ),
            size_result=SizeOrError(
                error=EdenError(
                    message="cdir/subdir: Is a directory",
                    errorCode=EISDIR,
                    errorType=EdenErrorType.POSIX_ERROR,
                )
            ),
            object_id=self.cdir_subdir_id,
            digest_size_result=self.cdir_subdir_digest_size_result,
            digest_hash_result=self.cdir_subdir_digest_hash_result,
            mtime_result=MtimeOrError(mtime=mtime),
            mode_result=ModeOrError(mode=mode),
        )

        if sys.platform != "win32":
            sockpath = self.get_path("adir/asock")
            sock_error = EdenError(
                message="adir/asock: file is a non-source-control type: 12: Invalid argument",
                errorCode=EINVAL,
                errorType=EdenErrorType.POSIX_ERROR,
            )

            # UDS are not supported in python on Win until 3.9:
            # https://bugs.python.org/issue33408
            with socket.socket(socket.AF_UNIX) as sock:
                sock.bind(sockpath)
                stat = self.stat("adir/asock")
                (mode, mtime) = self.get_expected_mode_and_mtime(stat)
                await self.readdir_no_size_or_sha1(
                    parent_name=b"adir",
                    entry_name=b"asock",
                    source_control_type=SourceControlType.UNKNOWN,
                    sha1_result=Sha1OrError(error=sock_error),
                    blake3_result=Blake3OrError(
                        error=sock_error,
                    ),
                    size_result=SizeOrError(
                        error=sock_error,
                    ),
                    object_id=None,
                    digest_size_result=DigestSizeOrError(
                        error=sock_error,
                    ),
                    digest_hash_result=DigestHashOrError(
                        error=sock_error,
                    ),
                    mtime_result=MtimeOrError(mtime=mtime),
                    mode_result=ModeOrError(mode=mode),
                )

        slink_error = EdenError(
            message="slink: file is a symlink: Invalid argument",
            errorCode=EINVAL,
            errorType=EdenErrorType.POSIX_ERROR,
        )

        stat = self.stat("slink")
        (mode, mtime) = self.get_expected_mode_and_mtime(stat)
        await self.readdir_no_size_or_sha1(
            parent_name=b"",
            entry_name=b"slink",
            source_control_type=SourceControlType.SYMLINK,
            sha1_result=Sha1OrError(error=slink_error),
            blake3_result=Blake3OrError(
                error=slink_error,
            ),
            size_result=SizeOrError(
                error=slink_error,
            ),
            object_id=self.slink_id,
            digest_size_result=DigestSizeOrError(
                error=slink_error,
            ),
            digest_hash_result=DigestHashOrError(
                error=slink_error,
            ),
            mtime_result=MtimeOrError(mtime=mtime),
            mode_result=ModeOrError(mode=mode),
        )

    async def test_materialized_files_return_no_object_id(self) -> None:
        self.write_file("adir/file", "new contents\n")

        actual = await self.get_attributes_v2([b"adir/file"], FileAttributes.OBJECT_ID)
        self.assertEqual(1, len(actual.res))
        expected = FileAttributeDataOrErrorV2(
            fileAttributeData=FileAttributeDataV2(
                objectId=ObjectIdOrError(objectId=None)
            )
        )
        self.assertEqual(expected, actual.res[0])
