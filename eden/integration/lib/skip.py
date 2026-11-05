#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import os
import sys
from typing import Dict, List, Union


#
# Disabled tests definitions.
# This is a dictionary of class names. For each class the value can be set to True to
# skip all tests in this class, or a list of specific test functions to skip.
#
# We are currently skipping most existing test cases on Windows, but over time we
# should gradually remove tests from this list as we get them passing on Windows.
#
TEST_DISABLED: Dict[str, Union[List[str], bool]] = {}
if sys.platform == "linux":
    # These fsck cases never mount a checkout, so transport variants add no coverage.
    TEST_DISABLED["fsck.basic_snapshot_tests.Basic20251104TestIoUring"] = [
        "test_incorrect_next_inode_number",
        "test_corrupt_next_inode_number",
    ]
if sys.platform == "win32":
    # Note that on Windows we also exclude some test source files entirely
    # in CMakeLists.txt, for tests that never make sense to run on Windows.
    TEST_DISABLED: Dict[str, Union[List[str], None]] = {
        #
        # Test classes from the main integration test binary
        #
        "basic_test.PosixTest": True,
        "chown_test.ChownTest": True,
        "clone_test.CloneFakeEdenFSTestAdHoc": True,
        "clone_test.CloneFakeEdenFSTestManaged": True,
        "clone_test.CloneTestHg": True,
        "config_test.ConfigTest": True,
        "changes_test.ChangesTestNix": True,
        "corrupt_overlay_test.CorruptOverlayTest": True,  # Corrupts the file overlay that isn't available on Windows
        "debug_getpath_test.DebugGetPathTestHg": True,
        "doteden_test.DotEdenTestHg": [
            "test_mkdir_fails",  # ProjectedFS doesn't allow refusing directory creation
            "test_create_file_fails",  # ProjectedFS doesn't allow refusing file creation
            "test_mknod_fails",  # mknod doesn't exist on Windows
            "test_symlink_fails",  # ProjectedFS doesn't allow refusing symlink creation
            "test_chown_fails",  # chown doesn't exist on Windows
        ],
        "edenclient_test.EdenClientTestHg": True,
        "facebook.buck.buck_test.BuckTestHg": True,
        "fsck_test.FsckTestDefault": True,
        "fsck_test.FsckTestNoEdenfs": True,
        "fsck.basic_snapshot_tests.Basic20251104Test": True,
        "health_test.HealthOfFakeEdenFSTestAdHoc": True,
        "health_test.HealthOfFakeEdenFSTestManaged": True,
        "info_test.InfoTestHg": True,
        "materialized_query_test.MaterializedQueryTestHg": True,
        "mmap_test.MmapTestHg": True,
        "mount_test.MountTestHg": True,
        "oexcl_test.OpenExclusiveTestHg": True,
        "patch_test.PatchTestHg": True,
        "persistence_test.PersistenceTestHg": [
            "test_does_not_reuse_inode_numbers_after_cold_restart"
        ],
        # These tests need FUSE or NFS. Windows mounts use ProjectedFS.
        "pressure_gc_test.ActiveFuseInvalidationTest": [
            "test_active_invalidation_forgets_file_inode",
            "test_active_invalidation_forgets_file_inode_after_takeover",
            "test_active_invalidation_preserves_bind_redirection_0_without_takeover",
            "test_active_invalidation_preserves_bind_redirection_1_after_takeover",
            "test_active_invalidation_preserves_getcwd_of_running_process",
            "test_active_invalidation_reclaims_siblings_of_open_file",
            "test_active_invalidation_reclaims_unpinned_directories",
        ],
        "pressure_gc_test.PressureGcWithoutPinScanTest": True,
        # Windows has no privhelper process to kill.
        "privhelper_test.PrivHelperDeathTest": True,
        "rage_test.RageTestDefault": True,
        "rc_test.RCTestHg": True,
        "readdir_test.ReaddirTestHg": [
            # creating sockets is not supported in python on Windows until 3.9
            "test_get_attributes_socket",
        ],
        "redirect_test.RedirectTestHg": ["test_disallow_bind_mount_outside_repo"],
        "remount_test.RemountTestHg": True,
        "rename_test.RenameTestHg": True,
        "restart_test.RestartTestAdHoc": True,
        "restart_test.RestartTestManaged": True,
        "sed_test.SedTestHg": True,
        "service_log_test.ServiceLogFakeEdenFSTestAdHoc": True,
        "service_log_test.ServiceLogFakeEdenFSTestManaged": True,
        "service_log_test.ServiceLogRealEdenFSTest": True,
        "setattr_test.SetAttrTestHg": True,
        "snapshot.test_snapshots.InfraTests": True,
        "snapshot.test_snapshots.Test": True,
        "stale_test.StaleTestDefault": True,
        "start_test.DirectInvokeTest": True,
        "start_test.StartFakeEdenFSTestAdHoc": True,
        "start_test.StartFakeEdenFSTestManaged": True,
        "start_test.StartTest": True,
        "start_test.StartWithRepoTestHg": True,
        "stats_test.GenericStatsTest": [
            "test_summary_counters_available",  # counter not implemented for PrjFS (T147669123)
        ],
        "stats_test.ObjectCacheStatsTest": [
            "test_get_tree_memory",  # unloadInodeForPath not implemented
        ],
        "stop_test.AutoStopTest": True,
        "stop_test.StopTestAdHoc": True,
        "stop_test.StopTestManaged": True,
        "takeover_test.TakeoverRocksDBStressTestHg": True,
        "takeover_test.TakeoverTestHg": True,
        "takeover_test.TakeoverTestNoNFSServerHg": True,
        "thrift_test.ThriftTestHg": [
            "test_get_blake3_throws_for_symlink",
            "test_pid_fetch_counts",
            "test_unload_free_inodes",
            "test_unload_thrift_api_accepts_single_dot_as_root",
        ],
        "unixsocket_test.UnixSocketTestHg": True,
        "userinfo_test.UserInfoTest": True,
        #
        # Test classes from the hg integration test binary
        #
        "hg.debug_get_parents.DebugGetParentsTest": True,
        "hg.debug_hg_dirstate_test.DebugHgDirstateTest": True,
        "hg.diff_test.DiffTest": True,
        # The fake watchman is a shell script and `eden doctor` stops at the
        # elevated-process check on the Windows runners.
        "hg.doctor_test.DoctorTest": ["test_doctor_watchman_error_reply"],
        "hg.grep_test.GrepTest": [
            "test_grep_directory_from_root",
            "test_grep_directory_from_subdirectory",
        ],
        "hg.rebase_test.RebaseTest": ["test_rebase_commit_with_independent_folder"],
        "hg.rm_test.RmTest": [
            "test_rm_directory_with_modification",
            "test_rm_modified_file_permissions",
        ],
        "hg.split_test.SplitTest": ["test_split_one_commit_into_two"],
        "hg.status_deadlock_test.StatusDeadlockTest": True,
        "hg.symlink_test.SymlinkTest": [
            # ProjectedFS rejects creating a file under a directory that a
            # blocked checkout is holding, so the race cannot be staged.
            "test_update_symlink_with_file_created_during_checkout",
        ],
        "hg.status_test.StatusTest": [
            # TODO: Opening a file with O_TRUNC inside an EdenFS mount fails on Windows
            "test_partial_truncation_after_open_modifies_file",
            # TODO: These tests do not report the file as modified after truncation
            "test_truncation_after_open_modifies_file",
            "test_truncation_upon_open_modifies_file",
        ],
        "hg.update_test.UpdateCacheInvalidationTest": [
            "test_changing_file_contents_creates_new_inode_and_flushes_dcache",
            # Similar to above, this test is marked flaky on the TestX UI. To avoid
            # the FilteredFS mixin showing up as broken, we skip it altogether.
            "test_file_locked_removal",
        ],
        "hg.update_test.UpdateTest": [
            # Windows keeps abort-on-eden-conflict-error off, so the abort
            # this test expects never happens.
            "test_update_reports_checkout_error",
            # TODO: A \r\n is used
            "test_mount_state_during_unmount_with_in_progress_checkout",
            # Windows doesn't support executable files; mode changes are no-op
            "test_mode_change_with_no_content_change",
        ],
        "stale_inode_test.StaleInodeTestHgNFS": True,
        "windows_fsck_test.WindowsFsckTestHg": [
            # T146967686
            "test_detect_removed_file_from_dirty_placeholder_directory",
            "test_detect_removed_file_from_placeholder_directory",
        ],
        # This is broken on Vanilla EdenFS but only disabled through the TestX
        # UI. To avoid the FilteredFS mixin showing up as broken, we skip it.
        "hg.eden_journal_test.EdenJournalTest": [
            "test_journal_position_write",
            "test_journal_stream_changes_since",
            "test_journal_stream_selected_changes_since",
            "test_journal_stream_selected_changes_since_empty_glob",
        ],
    }
elif sys.platform.startswith("linux") and not os.path.exists("/etc/redhat-release"):
    # The ChownTest.setUp() code tries to look up the "nobody" group, which doesn't
    # exist on Ubuntu.
    TEST_DISABLED["chown_test.ChownTest"] = True
    TEST_DISABLED["changes_test.ChangesTestNix"] = [
        "test_modify_folder_chown",
    ]

    # These tests try to run "hg whereami", which isn't available on Ubuntu.
    # This command is provided by the scm telemetry wrapper rather than by hg
    # itself, and we currently don't install the telemetry wrapper on Ubuntu.
    TEST_DISABLED["hg.doctor_test.DoctorTestTreeOnly"] = [
        "test_eden_doctor_fixes_valid_mismatched_parents",
    ]

    TEST_DISABLED["hg.post_clone_test.SymlinkTestTreeOnly"] = [
        # This test fails with mismatched permissions (0775 vs 0755).
        # I haven't investigated too closely but it could be a umask configuration
        # issue.
        "test_post_clone_permissions"
    ]

elif sys.platform.startswith("darwin"):
    # OSERROR AF_UNIX path too long
    TEST_DISABLED["hg.status_test.StatusTestTreeOnly"] = [
        "test_status",
        "test_status_thrift_apis",
    ]

    # Requires Password on macOS
    TEST_DISABLED["changes_test.ChangesTestNix"] = [
        "test_modify_folder_chown",
    ]

    # Times out on macOS
    TEST_DISABLED["stats_test.ObjectCacheStatsTest"] = [
        "test_get_tree_memory",
    ]

    TEST_DISABLED["hg.update_test.UpdateTest"] = [
        # update fails because new file created while checkout operation in progress
        "test_change_casing_with_untracked",
        # TODO(T163970408)
        "test_mount_state_during_unmount_with_in_progress_checkout",
    ]

    # Assertion error and invalid argument
    TEST_DISABLED["snapshot.test_snapshots.InfraTestsDefault"] = [
        "test_snapshot",
        "test_verify_directory",
    ]
    TEST_DISABLED["snapshot.test_snapshots.Testbasic-20251104"] = True

    TEST_DISABLED["basic_test.PosixTest"] = [
        "test_create_using_mknod",  # PermissionDenied
        "test_statvfs",  # NFS block size appears to be too small.
    ]

    # `eden chown` requires the use of `sudo` for chowning redirections. We
    # don't have access to passwordless `sudo` on macOS Sandcastle hosts, so
    # we should disable these test.
    TEST_DISABLED["chown_test.ChownTest"] = True

    # T89441739
    TEST_DISABLED["corrupt_overlay_test.CorruptOverlayTest"] = [
        "test_unmount_succeeds",
        "test_unlink_deletes_corrupted_files",
    ]

    # CalledProcessError
    TEST_DISABLED["health_test.HealthOfFakeEdenFSTest"] = True

    # Broken on NFS since NFS will just hang with the message "nfs server
    # edenfs:: not responding". There was an attempt to fix this with commit
    # 1f5512cf74ca, but it seems like something in test teardown is causing
    # hangs still.
    TEST_DISABLED["stale_test.StaleTest"] = True

    # CalledProcessError (same as health_test above?). Seems like
    # FakeEdenFS is broken on MacOS
    TEST_DISABLED["service_log_test.ServiceLogFakeEdenFSTest"] = True
    TEST_DISABLED["service_log_test.ServiceLogRealEdenFSTest"] = True

    # Expect OSError but does not happen (T89439721)
    TEST_DISABLED["setattr_test.SetAttrTest"] = [
        "test_chown_gid_as_nonroot_fails_if_not_member",
        "test_chown_uid_as_nonroot_fails",
        "test_setuid_setgid_and_sticky_bits_fail_with_eperm",
    ]

    # On NFS, we don't have the per-mount live_request counters implemented.
    # We can enable this test once those are added (T147665665)
    TEST_DISABLED["stats_test.GenericStatsTest"] = [
        "test_summary_counters_available",
    ]

    TEST_DISABLED["takeover_test.TakeoverTestHg"] = [
        # The non pinging version of takeover does not support NFS. Non pinging
        # was version 3 and NFS support was version 5.
        "test_takeover_doesnt_send_ping",
        # I think this one should be fixable. It seems like the nfsd3 is
        # triggering channel shutdown when it's restarted. I think this is a
        # legit bug, though it seems specific to macOS.
        "test_takeover_failure",
        # Using a fd before and after takeover seems to hang sometimes. I can
        # not manually repro this, but we should probably investigate this bcz
        # hangs like this are a really painful experience.
        # investigation hint: I don't think I see IO stuck in eden when this
        # happens, it might be in the kernel. My M1 seems to crash a lot when
        # I run these tests (though the crashes don't seem to mention NFS), so
        # something is weird here.
        "test_takeover_with_io",
        "test_takeover",
        "test_readdir_after_graceful_restart",
        "test_contents_are_the_same_if_handle_is_held_open",
    ]
    # same fd issue as above.
    TEST_DISABLED["takeover_test.TakeoverTestNoNFSServer"] = [
        "test_takeover",
    ]

    # T89440575: We aren't able to get fetch counts by PID on NFS, which doesn't
    # provide any information about client processes.
    TEST_DISABLED["thrift_test.ThriftTest"] = [
        "test_pid_fetch_counts",
    ]

    # OSError: AF_UNIX path too long
    TEST_DISABLED["unixsocket_test.UnixSocketTest"] = True

    # fsck doesn't work on macOS?
    TEST_DISABLED["fsck.basic_snapshot_tests.Basic20251104TestDefault"] = True

    # flakey (actual timing doesn't match expected timing)
    TEST_DISABLED["config_test.ConfigTest"] = True

    if "SANDCASTLE" in os.environ:
        # S362020: Redirect tests leave behind garbage on macOS Sandcastle hosts
        TEST_DISABLED["redirect_test.RedirectTest"] = True


# Windows specific tests
# we keep them in the build graph on linux so pyre will run and disable them
# here
if sys.platform != "win32":
    TEST_DISABLED.update(
        {
            "changes_test.ChangesTestWin": True,
            "windows_fsck_test.WindowsFsckTest": True,
            "prjfs_stress.PrjFSStress": True,
            "prjfs_stress.PrjfsStressNoListenToFull": True,
            "projfs_buffer.PrjFSBuffer": True,
            "projfs_enumeration.ProjFSEnumeration": True,
            "projfs_enumeration.ProjFSEnumerationInsufficientBuffer": True,
            "prjfs_match_fs.PrjfsMatchFsTest": True,
            "hg.symlink_test.SymlinkWindowsDisabledTest": True,
            "hg.update_test.PrjFSStressTornReads": True,
        }
    )


def _have_ntapi_extension_module() -> bool:
    if sys.platform != "win32":
        return False

    try:
        # pyrefly: ignore [missing-import]
        from eden.integration.lib.ntapi import (  # @manual  # noqa: F401
            get_directory_entry_size,
        )

        return True
    except ImportError:
        return False


# ProjFS enumeration tests depend on a Python extension module, which may not be
# available with all build systems.
if not _have_ntapi_extension_module():
    TEST_DISABLED.update(
        {
            "projfs_enumeration.ProjFSEnumeration": True,
            "projfs_enumeration.ProjFSEnumerationInsufficientBuffer": True,
        }
    )

# We only run tests on linux currently, so we only need to disable them there.
if sys.platform.startswith("linux"):
    # tests to skip on nfs, this list allows us to avoid writing the nfs postfix
    # on the test.
    NFS_TEST_DISABLED = {
        "takeover_test.TakeoverTest": [
            "test_takeover_doesnt_send_ping"  # this test uses protocol 3 of
            # graceful restart. Version 3 does not support restarting
            # NFS mounts. So we inherently expect this test to fail on
            # NFS.
        ],
        "invalidate_test.InvalidateTest": True,
        # EdenFS's NFS implementation is NFSv3, which doesn't support extended
        # attributes.
        "xattr_test.XattrTest": [  # T89439481
            "test_get_sha1_xattr",
            "test_get_sha1_xattr_succeeds_after_querying_xattr_on_dir",
            "test_get_digest_hash_xattr",
        ],
        "setattr_test.SetAttrTest": [  # T89439721
            "test_chown_gid_as_nonroot_fails_if_not_member",
            "test_chown_uid_as_nonroot_fails",
            "test_setuid_setgid_and_sticky_bits_fail_with_eperm",
        ],
        "stats_test.GenericStatsTest": [
            # On NFS, we don't have the per-mount live_request counters.
            # We can enable this test once those are added (T147665665).
            "test_summary_counters_available",
        ],
        "stats_test.CountersTest": [
            # Same as above test: (T147665665).
            "test_mount_unmount_counters"
        ],
        # T89440575: We aren't able to get fetch counts by PID on NFS, which
        # doesn't provide any information about client processes.
        "thrift_test.ThriftTest": ["test_pid_fetch_counts"],
        "mount_test.MountTest": [  # T91790656
            "test_unmount_succeeds_while_file_handle_is_open",
            "test_unmount_succeeds_while_dir_handle_is_open",
        ],
    }

    for testModule, disabled in NFS_TEST_DISABLED.items():
        TEST_DISABLED[testModule + "NFSHg"] = disabled

    # custom nfs tests that we also need to disable
    TEST_DISABLED.update(
        {
            "corrupt_overlay_test.CorruptOverlayTestNFS": [  # T89441739
                "test_unlink_deletes_corrupted_files",
                "test_unmount_succeeds",
            ],
            "fsck_test.FsckTestNFS": [  # T89442010
                "test_fsck_force_and_check_only",
                "test_fsck_multiple_mounts",
            ],
            "stale_test.StaleTestNFS": True,  # T89442539
            "hg.update_test.UpdateTestTreeOnlyNFS": [
                "test_mount_state_during_unmount_with_in_progress_checkout"  # T90881795
            ],
        }
    )

if "SANDCASTLE" in os.environ:
    # This test seems to leave behind unkillable processes on sandcastle.
    # Disable it for now. Append to TEST_DISABLED below to preserve existing disabled tests.
    class_name = "hg.update_test.UpdateTest"
    method_name = "test_dir_locking"
    class_skipped = TEST_DISABLED.get(class_name)
    if class_skipped is None:
        TEST_DISABLED[class_name] = [method_name]
    elif isinstance(class_skipped, list):
        if method_name not in class_skipped:
            class_skipped.append(method_name)

    # We don't have passwordless sudo on sandcastle
    # Disable if it's not already disabled
    if TEST_DISABLED.get("changes_test.ChangesTestNix") is None:
        TEST_DISABLED["changes_test.ChangesTestNix"] = [
            "test_modify_folder_chown",
        ]
    class_name = "chown_test.ChownTest"
    method_name = "test_chown_with_bindmount"
    class_skipped = TEST_DISABLED.get(class_name)
    if class_skipped is None:
        TEST_DISABLED[class_name] = [method_name]
    elif isinstance(class_skipped, list):
        if method_name not in class_skipped:
            class_skipped.append(method_name)


try:
    from eden.integration.facebook.lib.skip import add_fb_specific_skips

    add_fb_specific_skips(TEST_DISABLED)
except ImportError:
    pass

# Propagate skips to FilteredFS and Coroutines test variants so that
# platform-specific skips apply to those mixins as well. Do not add
# FilteredFS-specific skips here; add them below to FILTERED_TEST_DISABLED.
VARIANT_PARITY = {}
for class_name, value_name in TEST_DISABLED.items():
    for suffix in ("FilteredHg", "Coroutines"):
        if class_name.endswith("Hg"):
            VARIANT_PARITY[class_name.replace("Hg", suffix)] = value_name
        else:
            VARIANT_PARITY[class_name + suffix] = value_name
TEST_DISABLED.update(VARIANT_PARITY)

# These platform limitations also apply to the independent io_uring siblings.
if sys.platform == "linux":
    for test_class in (
        "hg.doctor_test.DoctorTest",
        "hg.post_clone_test.SymlinkTest",
        "hg.update_test.UpdateTest",
    ):
        for suffix in ("TreeOnly", "TreeOnlyFilteredHg"):
            name = test_class + suffix
            if name in TEST_DISABLED:
                TEST_DISABLED[name + "IoUring"] = TEST_DISABLED[name]

# Any future FilteredHg skips should be added here
FILTEREDFS_TEST_DISABLED = {
    # These tests will behave the exact same on FilteredFS. Duplicating them can
    # cause issues on macOS (too many APFS subvolumes), so we'll disable the
    # FilteredHg variants for now.
    "redirect_test.RedirectTest": [
        "test_list",
        "test_fixup_mounts_things",
        "test_add_absolute_target",
        "test_redirect_no_config_dir",
        "test_unmount_unmounts_things",
        "test_list_no_legacy_bind_mounts",
        "test_disallow_bind_mount_outside_repo",
    ],
}
for testModule, disabled in FILTEREDFS_TEST_DISABLED.items():
    # We should add skips for all combinations of FilteredHg mixins.
    other_mixins = ["", "NFS"] if sys.platform != "win32" else ["", "InMemory"]
    filtered_variants = [f"{mixin}FilteredHg" for mixin in other_mixins]
    if sys.platform == "linux":
        filtered_variants.append("FilteredHgIoUring")
    for variant in filtered_variants:
        # We need to be careful that we don't overwrite any pre-existing lists
        # or bulk disables that were disabled by other criteria.
        new_class_name = testModule + variant
        prev_disabled = TEST_DISABLED.get(new_class_name)
        if prev_disabled is None:
            # There are no previously disabled tests, we're free to bulk add
            TEST_DISABLED[new_class_name] = disabled
        else:
            # If there's a list of previously disabled tests, we need to append
            # our list. Otherwise, we can no-op since all tests (including the
            # ones we specified) are already disabled.
            if isinstance(prev_disabled, list):
                TEST_DISABLED[new_class_name] = prev_disabled + disabled

# TODO(T233006794) The following tests have an issue on NFS runners where they
# will prevent the edenfs daemon from shutting down.
# We are disabling them for now to unblock the conveyors but should fix them
EDEN_CRASH_NFS_DISABLED = {
    "takeover_test.TakeoverTestNFS": [
        "test_takeover",
        "test_takeover_failure",
        "test_takeover_after_diff_revisions",
        "test_readdir_after_graceful_restart",
        "test_readdir_before_and_after_graceful_restart",
        "test_takeover_with_io",
        "test_takeover_updates_process_id_in_lock_file",
        "test_takeover_preserves_inode_numbers_for_open_nonmaterialized_files",
        "test_contents_are_the_same_if_handle_is_held_open",
    ],
    "takeover_test.TakeoverRocksDBStressTestNFS": [
        "test_takeover_with_tree_inode_loading_from_local_store",
    ],
}

for testModule, disabled in EDEN_CRASH_NFS_DISABLED.items():
    other_mixins = ["Hg", "FilteredHg"]
    for mixin in other_mixins:
        # We need to be careful that we don't overwrite any pre-existing lists
        # or bulk disables that were disabled by other criteria.
        new_class_name = testModule + mixin
        prev_disabled = TEST_DISABLED.get(new_class_name)
        if prev_disabled is None:
            # There are no previously disabled tests, we're free to bulk add
            TEST_DISABLED[new_class_name] = disabled
        else:
            # If there's a list of previously disabled tests, we need to append
            # our list. Otherwise, we can no-op since all tests (including the
            # ones we specified) are already disabled.
            if isinstance(prev_disabled, list):
                TEST_DISABLED[new_class_name] = prev_disabled + disabled


def is_class_disabled(class_name: str) -> bool:
    class_skipped = TEST_DISABLED.get(class_name)
    if class_skipped is None:
        return False
    if isinstance(class_skipped, bool):
        assert class_skipped is True
        # All classes in the test are skipped
        return True
    return False


def is_method_disabled(class_name: str, method_name: str) -> bool:
    method_skipped = TEST_DISABLED.get(class_name)
    if method_skipped is None:
        return False
    assert isinstance(method_skipped, list), "skipped methods must be a list"
    return method_name in method_skipped
