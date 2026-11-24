#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import abc
import configparser
import errno
import os
import stat
from typing import TYPE_CHECKING

from eden.fs.service.eden.thrift_types import (
    AclEntry,
    AclInfo,
    AclInfoOrError,
    AttributesRequestScope,
    CheckoutMode,
    CheckOutRevisionParams,
    ConflictType,
    DirListAttributeDataOrError,
    FileAttributes,
    GetAttributesFromFilesParams,
    GetAttributesFromFilesResultV2,
    GetScmStatusParams,
    GlobParams,
    ReaddirParams,
    SyncBehavior,
    UnderAclOrError,
)
from eden.integration.hg.lib.hg_extension_test_base import EdenHgTestCase, hg_test
from eden.integration.lib import hgrepo
from eden.integration.lib.eagerrepo import EagerRepo


class _RestrictedTreeTestBase(EdenHgTestCase, metaclass=abc.ABCMeta):
    """Base class for restricted tree tests using standard eagerepo setup."""

    initial_commit: str = ""
    swapped_commit: str = ""
    added_restricted_commit: str = ""
    ignored_file_restricted_commit: str = ""
    # Subclasses flip this to False to have the server allow every tree.
    enable_server_acl_enforcement: bool = True
    # Subclasses flip this to True when they exercise backing-store path ACLs.
    enable_path_acl_lookup: bool = False
    restricted_content_mode: str = "restricted"

    def edenfs_extra_config(self) -> dict[str, list[str]] | None:
        configs = super().edenfs_extra_config() or {}
        # Snapshotted onto ServerState at daemon startup, which is when this
        # config file is read.
        configs.setdefault("acl", []).append(
            f'restricted-content-mode = "{self.restricted_content_mode}"'
        )
        return configs

    def apply_hg_config_variant(self, hgrc: configparser.ConfigParser) -> None:
        super().apply_hg_config_variant(hgrc)
        # scmstore reads these from the backing repo's .hg/hgrc. EdenHgTestCase
        # writes this hgrc before eden.clone(), so the config is in place by
        # the time the backing store reads it.
        # ACL data rides on tree child metadata.
        if not hgrc.has_section("scmstore"):
            hgrc.add_section("scmstore")
        hgrc["scmstore"]["tree-metadata-mode"] = "always"
        if self.enable_path_acl_lookup:
            if not hgrc.has_section("backingstore"):
                hgrc.add_section("backingstore")
            hgrc["backingstore"]["enable-path-acl-lookup"] = "true"
        if not hgrc.has_section("slacl"):
            hgrc.add_section("slacl")
        # These settings apply only to the backing repo hgrc used by Sapling
        # commands in test setup, not the checked-out Eden client.
        hgrc["slacl"]["on-permission-denied"] = "warn"
        hgrc["slacl"]["server-acl-enforcement"] = (
            "true" if self.enable_server_acl_enforcement else "false"
        )

    def populate_backing_repo(self, repo: hgrepo.HgRepository) -> None:
        # Populate the eager backing repo directly so pulled trees carry the
        # ACL child metadata these tests are exercising. Committing in the
        # backing repo would go through a local-only path that bypasses it.
        eagerepo_path = repo.eagerepo
        assert eagerepo_path is not None, (
            "backing HgRepository.init() must populate self.eagerepo before "
            "populate_backing_repo runs"
        )
        eager = EagerRepo(
            eagerepo_path,
            hg_environment=repo.hg_environment,
            system_hgrc=None,
        )
        eager.init()

        # Initial commit: restricted/ has .slacl, regular/ does not.
        eager.write_file("regular/file.txt", "regular content")
        eager.write_file("restricted/.slacl", "acl config")
        eager.write_file("restricted/secret.txt", "secret content")
        eager.write_file("parent/normal_file.txt", "normal")
        eager.write_file("parent/nested_restricted/.slacl", "acl config")
        eager.write_file("parent/nested_restricted/deep.txt", "deep secret")
        eager.write_file("hello.txt", "hello")
        eager.write_file("ignored_local_only_restricted/tracked.txt", "tracked")
        eager.write_file(".gitignore", "ignored_local_only_restricted/local.txt\n")
        self.initial_commit = eager.commit("Initial commit.")

        # Swapped commit: restricted/ loses .slacl, regular/ gains it.
        eager.remove_file("restricted/.slacl")
        eager.write_file("regular/.slacl", "regular acl config")
        self.swapped_commit = eager.commit("Swap ACL state.")

        # Added restricted commit: local_only_restricted/ does not exist in
        # initial_commit, and is created as a restricted tree here. Tests create
        # a local untracked directory at this path before checking this out.
        eager.write_file("local_only_restricted/.slacl", "acl config")
        eager.write_file("local_only_restricted/server.txt", "server content")
        self.added_restricted_commit = eager.commit("Add restricted tree.")

        eager.write_file("ignored_local_only_restricted/.slacl", "acl config")
        self.ignored_file_restricted_commit = eager.commit(
            "Restrict tree containing ignored file."
        )

        # Pull the commits into the backing repo via SLAPI. ``fetch_edenapi``
        # populates ``indexedlog_cache`` with entries that include
        # ``acl_children_indices`` derived from ``has_acl`` on children.
        # Pulling by hash avoids needing a server bookmark (``master`` is on
        # the disallowed list anyway).
        repo.hg("pull", "-r", self.initial_commit)
        repo.hg("pull", "-r", self.swapped_commit)
        repo.hg("pull", "-r", self.added_restricted_commit)
        repo.hg("pull", "-r", self.ignored_file_restricted_commit)
        repo.hg("update", self.initial_commit)


if TYPE_CHECKING:
    # At type-check time, pretend the mixin inherits from the test base so
    # Pyre can resolve self.assertRaises, self.mount, etc. without a flood
    # of type-ignore annotations. At runtime, the mixin stays a plain object
    # so the concrete subclasses' MRO (mixin + real base) is unchanged.
    _MethodsBase = _RestrictedTreeTestBase
else:
    _MethodsBase = object


class _RestrictedTreeAclAttributesTestMethods(_MethodsBase, metaclass=abc.ABCMeta):
    async def _get_attributes_from_files(
        self,
        paths: list[bytes],
        requested: int,
        scope: AttributesRequestScope | None = None,
    ) -> GetAttributesFromFilesResultV2:
        async with self.get_async_thrift_client() as client:
            return await client.getAttributesFromFilesV2(
                GetAttributesFromFilesParams(
                    mountPoint=self.mount_path_bytes,
                    paths=paths,
                    requestedAttributes=requested,
                    sync=SyncBehavior(),
                    scope=scope,
                )
            )

    def _expected_under_acl(self, under_acl: bool) -> UnderAclOrError:
        return UnderAclOrError(underAcl=under_acl)

    def _expected_acl_info(
        self, under_acl: bool, restriction_roots: list[str]
    ) -> AclInfoOrError:
        return AclInfoOrError(
            aclInfo=AclInfo(
                underAcl=under_acl,
                acls=[
                    AclEntry(
                        restrictionRoot=root,
                        repoRegionAcl="some-acl",
                        requestAcl="some-acl",
                    )
                    for root in restriction_roots
                ],
            )
        )

    async def test_get_attributes_under_acl_only(self) -> None:
        paths = [
            b"restricted",
            b"restricted/secret.txt",
            b"parent/nested_restricted/deep.txt",
            b"hello.txt",
        ]
        result = await self._get_attributes_from_files(paths, FileAttributes.UNDER_ACL)

        self.assertEqual(4, len(result.res))
        self.assertEqual(
            self._expected_under_acl(True),
            result.res[0].fileAttributeData.underAcl,
        )
        self.assertEqual(
            self._expected_under_acl(True),
            result.res[1].fileAttributeData.underAcl,
        )
        self.assertEqual(
            self._expected_under_acl(True),
            result.res[2].fileAttributeData.underAcl,
        )
        self.assertEqual(
            self._expected_under_acl(False),
            result.res[3].fileAttributeData.underAcl,
        )

    async def test_get_attributes_acls_only(self) -> None:
        paths = [
            b"restricted",
            b"restricted/secret.txt",
            b"parent/nested_restricted/deep.txt",
            b"hello.txt",
        ]
        result = await self._get_attributes_from_files(paths, FileAttributes.ACLs)

        self.assertEqual(4, len(result.res))
        for entry in result.res:
            self.assertIsNone(entry.fileAttributeData.underAcl)
        self.assertEqual(
            self._expected_acl_info(True, ["restricted"]),
            result.res[0].fileAttributeData.aclInfo,
        )
        self.assertEqual(
            self._expected_acl_info(True, ["restricted"]),
            result.res[1].fileAttributeData.aclInfo,
        )
        self.assertEqual(
            self._expected_acl_info(True, ["parent/nested_restricted"]),
            result.res[2].fileAttributeData.aclInfo,
        )
        self.assertEqual(
            self._expected_acl_info(False, []),
            result.res[3].fileAttributeData.aclInfo,
        )

    async def test_get_attributes_acls_only_files_scope(self) -> None:
        result = await self._get_attributes_from_files(
            [b"restricted/secret.txt"],
            FileAttributes.ACLs,
            scope=AttributesRequestScope.FILES,
        )

        self.assertEqual(1, len(result.res))
        self.assertEqual(
            self._expected_acl_info(True, ["restricted"]),
            result.res[0].fileAttributeData.aclInfo,
        )

    async def test_get_attributes_under_acl_and_acls(self) -> None:
        requested = FileAttributes.UNDER_ACL | FileAttributes.ACLs

        result = await self._get_attributes_from_files(
            [b"restricted/secret.txt", b"hello.txt"], requested
        )

        self.assertEqual(
            self._expected_under_acl(True),
            result.res[0].fileAttributeData.underAcl,
        )
        self.assertEqual(
            self._expected_acl_info(True, ["restricted"]),
            result.res[0].fileAttributeData.aclInfo,
        )
        self.assertEqual(
            self._expected_under_acl(False),
            result.res[1].fileAttributeData.underAcl,
        )
        self.assertEqual(
            self._expected_acl_info(False, []),
            result.res[1].fileAttributeData.aclInfo,
        )


class _RestrictedTreeTestMethods(_MethodsBase, metaclass=abc.ABCMeta):
    """Mixin with test methods parameterized by expect_restricted."""

    # Subclasses set this to False for config-off variants
    expect_restricted: bool = True

    def _assert_dir_blocked(self, path: str) -> None:
        """Assert directory is blocked (EACCES) or accessible, based on expect_restricted."""
        if self.expect_restricted:
            with self.assertRaises(OSError) as ctx:
                os.listdir(path)
            self.assertEqual(ctx.exception.errno, errno.EACCES)
        else:
            os.listdir(path)

    def _assert_file_blocked(self, path: str) -> None:
        """Assert file access is blocked or accessible."""
        if self.expect_restricted:
            with self.assertRaises(OSError) as ctx:
                with open(path, "r") as f:
                    f.read()
            self.assertEqual(ctx.exception.errno, errno.EACCES)
        else:
            with open(path, "r") as f:
                f.read()  # should not raise

    def _create_local_only_restricted_file(self) -> tuple[str, str]:
        local_dir = os.path.join(self.mount, "local_only_restricted")
        local_file = os.path.join(local_dir, "local.txt")
        os.mkdir(local_dir)
        with open(local_file, "w") as f:
            f.write("local content")
        return local_dir, local_file

    def test_regular_dir_is_accessible(self) -> None:
        """Regular directories should always be fully accessible."""
        entries = sorted(os.listdir(os.path.join(self.mount, "regular")))
        self.assertEqual(["file.txt"], entries)

        with open(os.path.join(self.mount, "regular", "file.txt"), "r") as f:
            self.assertEqual("regular content", f.read())

    def test_root_listing_includes_restricted_dir(self) -> None:
        """The root listing should include restricted directories."""
        entries = os.listdir(self.mount)
        self.assertIn("regular", entries)
        self.assertIn("restricted", entries)

    def test_regular_dir_stat_has_normal_permissions(self) -> None:
        st = os.lstat(os.path.join(self.mount, "regular"))
        self.assertTrue(stat.S_ISDIR(st.st_mode))
        self.assertNotEqual(st.st_mode & 0o7777, 0)

    def test_restricted_dir_stat_permissions(self) -> None:
        restricted_path = os.path.join(self.mount, "restricted")
        st = os.lstat(restricted_path)
        self.assertTrue(stat.S_ISDIR(st.st_mode))
        if self.expect_restricted:
            self.assertEqual(st.st_mode & 0o7777, 0)
        else:
            self.assertNotEqual(st.st_mode & 0o7777, 0)

    def test_restricted_dir_access(self) -> None:
        restricted_path = os.path.join(self.mount, "restricted")
        if self.expect_restricted:
            self.assertFalse(os.access(restricted_path, os.R_OK))
            self.assertFalse(os.access(restricted_path, os.W_OK))
            self.assertFalse(os.access(restricted_path, os.X_OK))
        else:
            self.assertTrue(os.access(restricted_path, os.R_OK))

    def test_restricted_dir_listdir(self) -> None:
        restricted_path = os.path.join(self.mount, "restricted")
        if self.expect_restricted:
            with self.assertRaises(OSError) as ctx:
                os.listdir(restricted_path)
            self.assertEqual(ctx.exception.errno, errno.EACCES)
        else:
            entries = os.listdir(restricted_path)
            self.assertIn("secret.txt", entries)

    def test_restricted_dir_file_access(self) -> None:
        secret_path = os.path.join(self.mount, "restricted", "secret.txt")
        if self.expect_restricted:
            with self.assertRaises(OSError) as ctx:
                with open(secret_path, "r") as f:
                    f.read()
            self.assertEqual(ctx.exception.errno, errno.EACCES)
        else:
            with open(secret_path, "r") as f:
                self.assertEqual("secret content", f.read())

    def test_nested_restricted_parent_is_accessible(self) -> None:
        parent_entries = sorted(os.listdir(os.path.join(self.mount, "parent")))
        self.assertIn("normal_file.txt", parent_entries)
        self.assertIn("nested_restricted", parent_entries)

    def test_nested_restricted_dir(self) -> None:
        nested_path = os.path.join(self.mount, "parent", "nested_restricted")
        if self.expect_restricted:
            st = os.lstat(nested_path)
            self.assertEqual(st.st_mode & 0o7777, 0)
            with self.assertRaises(OSError) as ctx:
                os.listdir(nested_path)
            self.assertEqual(ctx.exception.errno, errno.EACCES)
        else:
            entries = os.listdir(nested_path)
            self.assertIn("deep.txt", entries)

    def test_create_file_in_restricted_dir(self) -> None:
        new_file_path = os.path.join(self.mount, "restricted", "new_file.txt")
        if self.expect_restricted:
            with self.assertRaises(OSError) as ctx:
                with open(new_file_path, "w") as f:
                    f.write("should not be allowed")
            self.assertEqual(ctx.exception.errno, errno.EACCES)
        else:
            with open(new_file_path, "w") as f:
                f.write("should be allowed")

    async def test_glob_restricted_dirs(self) -> None:
        params = GlobParams(
            mountPoint=self.mount_path_bytes,
            globs=["**/*.txt"],
            includeDotfiles=False,
        )
        async with self.get_async_thrift_client() as client:
            result = await client.globFiles(params)

        matching = sorted(result.matchingFiles)

        self.assertIn(b"hello.txt", matching)
        self.assertIn(b"regular/file.txt", matching)
        self.assertIn(b"parent/normal_file.txt", matching)

        restricted_files = [f for f in matching if f.startswith(b"restricted/")]
        nested_restricted_files = [f for f in matching if b"nested_restricted/" in f]
        if self.expect_restricted:
            self.assertEqual(restricted_files, [])
            self.assertEqual(nested_restricted_files, [])
        else:
            self.assertGreater(len(restricted_files), 0)

    async def test_status_restricted_dirs(self) -> None:
        async with self.get_async_thrift_client() as client:
            status = await client.getScmStatusV2(
                GetScmStatusParams(
                    mountPoint=self.mount_path_bytes,
                    commit=self.initial_commit.encode(),
                    listIgnored=False,
                    rootIdOptions=None,
                )
            )

        if self.expect_restricted:
            restricted_entries = [
                path
                for path in status.status.entries
                if path.startswith(b"restricted/") or b"nested_restricted/" in path
            ]
            self.assertEqual(restricted_entries, [])

    async def test_readdir_thrift_on_restricted_dir(self) -> None:
        async with self.get_async_thrift_client() as client:
            result = await client.readdir(
                ReaddirParams(
                    mountPoint=self.mount_path_bytes,
                    directoryPaths=[b"restricted"],
                    sync=SyncBehavior(),
                )
            )
            dir_data = result.dirLists[0]
            if self.expect_restricted:
                # Restricted directories should return the union's error arm.
                self.assertEqual(DirListAttributeDataOrError.Type.error, dir_data.type)
                err = dir_data.error
                self.assertIsNotNone(err)
                if err.errorCode is not None:
                    self.assertIn(err.errorCode, [errno.EACCES, errno.EPERM])
            else:
                self.assertEqual(
                    DirListAttributeDataOrError.Type.dirListAttributeData,
                    dir_data.type,
                )
                self.assertIsNotNone(dir_data.dirListAttributeData)

    def test_checkout_restricted_to_unrestricted(self) -> None:
        self.repo.hg("update", self.initial_commit)

        if self.expect_restricted:
            with self.assertRaises(OSError) as ctx:
                os.listdir(os.path.join(self.mount, "restricted"))
            self.assertEqual(ctx.exception.errno, errno.EACCES)
        else:
            entries = os.listdir(os.path.join(self.mount, "restricted"))
            self.assertIn("secret.txt", entries)

        # After checkout to swapped, restricted/ loses .slacl -- always accessible
        self.repo.hg("update", self.swapped_commit)
        entries = sorted(os.listdir(os.path.join(self.mount, "restricted")))
        self.assertEqual(["secret.txt"], entries)

    def test_checkout_unrestricted_to_restricted(self) -> None:
        self.repo.hg("update", self.initial_commit)
        entries = sorted(os.listdir(os.path.join(self.mount, "regular")))
        self.assertEqual(["file.txt"], entries)

        # After checkout to swapped, regular/ gains .slacl
        self.repo.hg("update", self.swapped_commit)
        if self.expect_restricted:
            with self.assertRaises(OSError) as ctx:
                os.listdir(os.path.join(self.mount, "regular"))
            self.assertEqual(ctx.exception.errno, errno.EACCES)
        else:
            entries = os.listdir(os.path.join(self.mount, "regular"))
            self.assertIn("file.txt", entries)

    def test_checkout_unrestricted_to_restricted_with_local_changes(
        self,
    ) -> None:
        """Non-force checkout of unrestricted -> restricted with dirty subtree
        should surface a conflict and preserve the unrestricted directory."""
        # Start at initial_commit where regular/ is unrestricted.
        self.repo.hg("update", self.initial_commit)

        # Materialize regular/ by writing to a file inside it via the mount.
        regular_file = os.path.join(self.mount, "regular", "file.txt")
        with open(regular_file, "w") as f:
            f.write("local modification")

        if self.expect_restricted:
            # Non-force update into a restriction transition over a dirty
            # subtree should fail rather than burying the local modification
            # behind an EACCES wall. The checkout pre-check in
            # TreeInode::checkoutUpdateEntry surfaces a MODIFIED_MODIFIED
            # conflict; hg's checkout code reports this as an abort.
            with self.assertRaises(hgrepo.HgError):
                self.repo.hg("update", self.swapped_commit)

            # Local modification must still be accessible and intact.
            entries = os.listdir(os.path.join(self.mount, "regular"))
            self.assertIn("file.txt", entries)
            with open(regular_file, "r") as f:
                self.assertEqual("local modification", f.read())
        else:
            self.repo.hg("update", self.swapped_commit)
            entries = os.listdir(os.path.join(self.mount, "regular"))
            self.assertIn("file.txt", entries)

    async def test_checkout_unrestricted_to_restricted_with_deleted_file(
        self,
    ) -> None:
        """A locally deleted tracked file does not block the transition."""
        self.repo.hg("update", self.initial_commit)
        os.unlink(os.path.join(self.mount, "regular", "file.txt"))

        async with self.eden.get_async_thrift_client() as client:
            conflicts = await client.checkOutRevision(
                mountPoint=self.mount_path_bytes,
                snapshotHash=self.swapped_commit.encode(),
                checkoutMode=CheckoutMode.NORMAL,
                params=CheckOutRevisionParams(),
            )
        # An unrestricted destination leaves the unchanged entry alone and
        # reports nothing for it.
        expected = (
            [(b"regular/file.txt", ConflictType.MISSING_REMOVED)]
            if self.expect_restricted
            else []
        )
        self.assertEqual(
            expected, [(conflict.path, conflict.type) for conflict in conflicts]
        )
        self._assert_dir_blocked(os.path.join(self.mount, "regular"))

    def test_checkout_unrestricted_to_restricted_force(self) -> None:
        """Force checkout (-C) of unrestricted -> restricted with dirty
        subtree should drive the transition through and restrict the dir."""
        self.repo.hg("update", self.initial_commit)

        regular_file = os.path.join(self.mount, "regular", "file.txt")
        with open(regular_file, "w") as f:
            f.write("local modification")

        # Force update: discards local state and applies the transition.
        self.repo.hg("update", "-C", self.swapped_commit)

        # Verify the subtree is now restricted (under expect_restricted) or
        # still accessible (under config-off).
        self._assert_dir_blocked(os.path.join(self.mount, "regular"))

    def test_checkout_restricted_tree_addition_over_untracked_file(
        self,
    ) -> None:
        """Non-force checkout should preserve local-only files when their
        parent becomes restricted."""
        self.repo.hg("update", self.initial_commit)

        local_dir, local_file = self._create_local_only_restricted_file()

        if self.expect_restricted:
            with self.assertRaises(hgrepo.HgError):
                self.repo.hg("update", self.added_restricted_commit)

            self.assertEqual(["local.txt"], sorted(os.listdir(local_dir)))
            with open(local_file, "r") as f:
                self.assertEqual("local content", f.read())
        else:
            self.repo.hg("update", self.added_restricted_commit)
            with open(local_file, "r") as f:
                self.assertEqual("local content", f.read())

    def test_force_checkout_restricted_tree_addition_over_untracked_file(
        self,
    ) -> None:
        """Force checkout applies the restriction over a
        local-only file when the restricted directory is new in the target."""
        self.repo.hg("update", self.initial_commit)

        local_dir, local_file = self._create_local_only_restricted_file()

        self.repo.hg("update", "-C", self.added_restricted_commit)

        if self.expect_restricted:
            with self.assertRaises(OSError) as ctx:
                os.listdir(local_dir)
            self.assertEqual(ctx.exception.errno, errno.EACCES)
        else:
            self.repo.hg("update", self.added_restricted_commit)
            with open(local_file, "r") as f:
                self.assertEqual("local content", f.read())

    def test_rebase_across_restricted_tree_addition_with_untracked_file(
        self,
    ) -> None:
        """Rebase should avoid prefetching restricted trees and then apply the
        checkout behavior for a local-only file under the restricted path."""
        self.repo.hg("update", self.initial_commit)

        local_dir, local_file = self._create_local_only_restricted_file()

        self.write_file("hello.txt", "local hello")
        local_commit = self.repo.commit("Modify unrelated file.")

        if self.expect_restricted:
            self.hg(
                "rebase",
                "--config",
                "rebase.experimental.inmemory=False",
                "-r",
                local_commit,
                "-d",
                self.added_restricted_commit,
            )
            self._assert_dir_blocked(local_dir)
        else:
            self.hg(
                "rebase",
                "--config",
                "rebase.experimental.inmemory=False",
                "-r",
                local_commit,
                "-d",
                self.added_restricted_commit,
            )
            with open(local_file, "r") as f:
                self.assertEqual("local content", f.read())

    def test_checkout_roundtrip_restricted(self) -> None:
        self.repo.hg("update", self.initial_commit)

        self._assert_dir_blocked(os.path.join(self.mount, "restricted"))

        self.repo.hg("update", self.swapped_commit)
        entries = sorted(os.listdir(os.path.join(self.mount, "restricted")))
        self.assertEqual(["secret.txt"], entries)

        self.repo.hg("update", self.initial_commit)
        self._assert_dir_blocked(os.path.join(self.mount, "restricted"))

    def test_checkout_unloaded_inode_becomes_restricted(self) -> None:
        self.repo.hg("update", self.initial_commit)

        # Do NOT access regular/ before checkout -- leave the inode unloaded.
        self.repo.hg("update", self.swapped_commit)

        # regular/ gains .slacl after checkout
        if self.expect_restricted:
            with self.assertRaises(OSError) as ctx:
                os.listdir(os.path.join(self.mount, "regular"))
            self.assertEqual(ctx.exception.errno, errno.EACCES)
        else:
            entries = os.listdir(os.path.join(self.mount, "regular"))
            self.assertIn("file.txt", entries)

    def test_checkout_nested_restricted_transition(self) -> None:
        self.repo.hg("update", self.initial_commit)

        # parent/nested_restricted/ has .slacl in initial commit
        if self.expect_restricted:
            with self.assertRaises(OSError) as ctx:
                os.listdir(os.path.join(self.mount, "parent", "nested_restricted"))
            self.assertEqual(ctx.exception.errno, errno.EACCES)
        else:
            entries = os.listdir(
                os.path.join(self.mount, "parent", "nested_restricted")
            )
            self.assertIn("deep.txt", entries)

        # parent/normal_file.txt is always accessible
        with open(os.path.join(self.mount, "parent", "normal_file.txt"), "r") as f:
            self.assertEqual("normal", f.read())

    def test_checkout_read_file_after_unrestrict(self) -> None:
        self.repo.hg("update", self.initial_commit)

        # restricted/secret.txt blocked or readable depending on config
        self._assert_file_blocked(os.path.join(self.mount, "restricted", "secret.txt"))

        # After checkout to swapped, restricted/ becomes accessible
        self.repo.hg("update", self.swapped_commit)
        with open(os.path.join(self.mount, "restricted", "secret.txt"), "r") as f:
            self.assertEqual("secret content", f.read())

    def test_checkout_stat_permissions_change(self) -> None:
        self.repo.hg("update", self.initial_commit)
        st = os.lstat(os.path.join(self.mount, "regular"))
        self.assertNotEqual(st.st_mode & 0o777, 0)

        # After checkout to swapped, regular/ gains .slacl
        self.repo.hg("update", self.swapped_commit)
        st = os.lstat(os.path.join(self.mount, "regular"))
        if self.expect_restricted:
            self.assertEqual(st.st_mode & 0o777, 0)
        else:
            self.assertNotEqual(st.st_mode & 0o777, 0)


@hg_test
# pyre-ignore[13]: T62487924
class RestrictedTreeTest(_RestrictedTreeTestMethods, _RestrictedTreeTestBase):
    """The server denies restricted trees."""

    def test_checkout_restricted_tree_over_ignored_file(self) -> None:
        """Ignored files are discarded when their parent becomes restricted."""
        self.repo.hg("update", self.initial_commit)

        local_dir = os.path.join(self.mount, "ignored_local_only_restricted")
        local_file = os.path.join(local_dir, "local.txt")
        with open(local_file, "w") as f:
            f.write("local content")
        self.assert_status({"ignored_local_only_restricted/local.txt": "I"})

        self.repo.hg("update", self.ignored_file_restricted_commit)

        with self.assertRaises(OSError) as ctx:
            os.listdir(local_dir)
        self.assertEqual(errno.EACCES, ctx.exception.errno)
        self.assert_status_empty()


@hg_test
# pyre-ignore[13]: T62487924
class RestrictedTreeAclAttributesTest(
    _RestrictedTreeAclAttributesTestMethods, _RestrictedTreeTestBase
):
    """getAttributesFromFilesV2 ACL attributes."""

    enable_path_acl_lookup: bool = True


@hg_test
# pyre-ignore[13]: T62487924
class RestrictedTreeConfigOffTest(_RestrictedTreeTestMethods, _RestrictedTreeTestBase):
    """The server enforces nothing — all directories accessible."""

    enable_server_acl_enforcement: bool = False
    expect_restricted: bool = False


@hg_test
# pyre-ignore[13]: T62487924
class RestrictedTreeOmittedModeTest(_RestrictedTreeTestBase):
    """acl:restricted-content-mode = omitted: restricted roots are hidden
    from enumeration while explicit descendant access still returns EACCES.

    Stat/lookup OF the restricted root itself intentionally still behaves
    like restricted mode (mode-000 placeholder); only enumeration changes.
    """

    restricted_content_mode: str = "omitted"

    def test_listdir_omits_restricted_roots(self) -> None:
        root_entries = os.listdir(self.mount)
        self.assertNotIn("restricted", root_entries)
        self.assertIn("regular", root_entries)
        self.assertIn("parent", root_entries)
        self.assertIn("hello.txt", root_entries)

        parent_entries = os.listdir(os.path.join(self.mount, "parent"))
        self.assertNotIn("nested_restricted", parent_entries)
        self.assertIn("normal_file.txt", parent_entries)

    def test_explicit_descendant_access_returns_eacces(self) -> None:
        secret_path = os.path.join(self.mount, "restricted", "secret.txt")
        with self.assertRaises(OSError) as ctx:
            with open(secret_path, "r") as f:
                f.read()
        self.assertEqual(errno.EACCES, ctx.exception.errno)

    async def test_thrift_readdir_omits_restricted_root(self) -> None:
        async with self.get_async_thrift_client() as client:
            result = await client.readdir(
                ReaddirParams(
                    mountPoint=self.mount_path_bytes,
                    directoryPaths=[b"", b"parent"],
                    sync=SyncBehavior(),
                )
            )

        root_data = result.dirLists[0]
        self.assertEqual(
            DirListAttributeDataOrError.Type.dirListAttributeData, root_data.type
        )
        root_entries = root_data.dirListAttributeData
        self.assertNotIn(b"restricted", root_entries)
        self.assertIn(b"regular", root_entries)
        self.assertIn(b"parent", root_entries)
        self.assertIn(b"hello.txt", root_entries)

        parent_data = result.dirLists[1]
        self.assertEqual(
            DirListAttributeDataOrError.Type.dirListAttributeData, parent_data.type
        )
        parent_entries = parent_data.dirListAttributeData
        self.assertNotIn(b"nested_restricted", parent_entries)
        self.assertIn(b"normal_file.txt", parent_entries)


@hg_test
class RestrictedTreeOmittedModeRebaseTest(_RestrictedTreeTestBase):
    """Rebase rewrites a directory that has a restricted child, in omitted mode."""

    restricted_content_mode: str = "omitted"

    base_commit: str = ""
    dest_commit: str = ""

    def populate_backing_repo(self, repo: hgrepo.HgRepository) -> None:
        eagerepo_path = repo.eagerepo
        assert eagerepo_path is not None, (
            "backing HgRepository.init() must populate self.eagerepo before "
            "populate_backing_repo runs"
        )
        eager = EagerRepo(
            eagerepo_path,
            hg_environment=repo.hg_environment,
            system_hgrc=None,
        )
        eager.init()

        eager.write_file("dir/open.txt", "open\n")
        eager.write_file("dir/restricted/.slacl", "acl config\n")
        eager.write_file("dir/restricted/secret.txt", "secret\n")
        self.base_commit = eager.commit("Initial restricted tree.")

        # The destination changes "dir", so the rebase writes a new "dir" tree.
        eager.write_file("dir/other.txt", "destination\n")
        self.dest_commit = eager.commit("Change dir.")

        repo.hg("pull", "-r", self.base_commit)
        repo.hg("pull", "-r", self.dest_commit)
        repo.hg("update", self.base_commit)

    def test_listdir_after_rebase(self) -> None:
        self.assertNotIn("restricted", os.listdir(os.path.join(self.mount, "dir")))

        self.write_file("dir/new.txt", "new\n")
        self.repo.add_file("dir/new.txt")
        local_commit = self.repo.commit("Add a file under dir.")
        self.hg("rebase", "-r", local_commit, "-d", self.dest_commit)

        entries = os.listdir(os.path.join(self.mount, "dir"))
        self.assertIn("new.txt", entries)
        self.assertIn("other.txt", entries)
        # The rebased "dir" is stored without its ACL child, so "restricted" is listed.
        self.assertIn("restricted", entries)


@hg_test
# pyre-ignore[13]: T62487924
class RestrictedTreeRebaseCombinedEnforcementTest(_RestrictedTreeTestBase):
    """Rebase over a destination that removes a loaded restricted child."""

    enable_server_acl_enforcement: bool = True

    base_commit: str = ""
    dest_commit: str = ""
    conflict_path: str = "project/notes/conflict.txt"
    restricted_path: str = "project/notes/restricted_child"

    def populate_backing_repo(self, repo: hgrepo.HgRepository) -> None:
        eagerepo_path = repo.eagerepo
        assert eagerepo_path is not None, (
            "backing HgRepository.init() must populate self.eagerepo before "
            "populate_backing_repo runs"
        )
        eager = EagerRepo(
            eagerepo_path,
            hg_environment=repo.hg_environment,
            system_hgrc=None,
        )
        eager.init()

        eager.write_file("project/notes/README.md", "base\n")
        eager.write_file(f"{self.restricted_path}/.slacl", "acl config\n")
        eager.write_file(f"{self.restricted_path}/secret.txt", "secret\n")
        self.base_commit = eager.commit("Initial restricted tree.")

        eager.remove_file(f"{self.restricted_path}/.slacl")
        eager.remove_file(f"{self.restricted_path}/secret.txt")
        eager.write_file(self.conflict_path, "destination\n")
        self.dest_commit = eager.commit("Remove restricted tree and add conflict file.")

        repo.hg("pull", "-r", self.base_commit)
        repo.hg("pull", "-r", self.dest_commit)
        repo.hg("update", self.base_commit)

    def test_rebase_removes_loaded_restricted_tree(self) -> None:
        self.repo.hg("update", self.base_commit)

        restricted_abspath = os.path.join(self.mount, self.restricted_path)
        restricted_stat = os.lstat(restricted_abspath)
        self.assertTrue(stat.S_ISDIR(restricted_stat.st_mode))
        self.assertEqual(restricted_stat.st_mode & 0o7777, 0)
        with self.assertRaises(OSError) as ctx:
            os.listdir(restricted_abspath)
        self.assertEqual(ctx.exception.errno, errno.EACCES)

        self.write_file(self.conflict_path, "local\n")
        self.repo.add_file(self.conflict_path)
        local_commit = self.repo.commit("Add local conflict file.")

        try:
            with self.assertRaises(hgrepo.HgError) as context:
                self.hg(
                    "rebase",
                    "--config",
                    "rebase.experimental.inmemory=False",
                    "-r",
                    local_commit,
                    "-d",
                    self.dest_commit,
                )

            self.assertIn(
                f"conflicts while merging {self.conflict_path}!".encode(),
                context.exception.stderr,
            )
            self.assertNotIn(b"EdenError", context.exception.stderr)
            self.assertNotIn(b"path ACL restriction", context.exception.stderr)
            self.assert_unresolved(unresolved=[self.conflict_path])
            self.assert_status({self.conflict_path: "M"}, op="rebase")
            self.assertFalse(os.path.exists(restricted_abspath))
        finally:
            self.hg("rebase", "--abort", check=False)
