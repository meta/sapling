/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <memory>

#include <folly/ThreadLocal.h>

#include "eden/common/telemetry/DurationScope.h"
#include "eden/common/telemetry/Stats.h"
#include "eden/common/telemetry/StatsGroup.h"
#include "eden/common/utils/RefPtr.h"
#include "eden/fs/eden-config.h"

/**
 * All the EdenFS stats are documented in the EdenFS wiki
 * https://www.internalfb.com/intern/wiki/EdenFS/Development_Tips/EdenFS_ODS_Counters_and_Duration/
 * as well as the .md files in eden/fs/docs/stats/EdenStats.md
 * if you are adding or editing stats, please consider updating
 * the wiki and the .md files.
 */

namespace facebook::eden {

struct FuseStats;
struct NfsStats;
struct PrjfsStats;
struct ObjectStoreStats;
struct SaplingBackingStoreStats;
struct JournalStats;
struct GlobStats;
struct ThriftStats;
struct OverlayStats;
struct InodeMapStats;
struct InodeMetadataTableStats;
struct BlobCacheStats;
struct TreeCacheStats;
struct ScmStatusCacheStats;
struct TakeoverStats;
struct CheckoutStats;
struct TreeInodeStats;
struct CgroupFileCacheStats;
struct CheckoutRecoveryStats;
struct MountHealthCheckStats;
struct FakeStats;

class EdenStats : public RefCounted {
 public:
  /**
   * Records a specified elapsed duration. Updates thread-local storage, and
   * aggregates into the fb303 ServiceData in the background and on reads.
   */
  template <typename T, typename Rep, typename Period>
  void addDuration(
      StatsGroupBase::Duration T::* duration,
      std::chrono::duration<Rep, Period> elapsed) {
    (getStatsForCurrentThread<T>().*duration).addDuration(elapsed);
  }

  template <typename T>
  void increment(StatsGroupBase::Counter T::* counter, int64_t value = 1) {
    (getStatsForCurrentThread<T>().*counter).addValue(value);
  }

  template <typename T>
  std::string_view getName(StatsGroupBase::Counter T::* counter) {
    return (getStatsForCurrentThread<T>().*counter).getName();
  }

  /**
   * Aggregates thread-locals into fb303's ServiceData.
   *
   * This function can be called on any thread.
   */
  void flush();

  template <typename T>
  T& getStatsForCurrentThread() = delete;

 private:
  class ThreadLocalTag {};

  template <typename T>
  using ThreadLocal = folly::ThreadLocal<T, ThreadLocalTag, void>;

  ThreadLocal<FuseStats> fuseStats_;
  ThreadLocal<NfsStats> nfsStats_;
  ThreadLocal<PrjfsStats> prjfsStats_;
  ThreadLocal<ObjectStoreStats> objectStoreStats_;
  ThreadLocal<SaplingBackingStoreStats> saplingBackingStoreStats_;
  ThreadLocal<JournalStats> journalStats_;
  ThreadLocal<GlobStats> globStats_;
  ThreadLocal<ThriftStats> thriftStats_;
  ThreadLocal<TelemetryStats> telemetryStats_;
  ThreadLocal<OverlayStats> overlayStats_;
  ThreadLocal<InodeMapStats> inodeMapStats_;
  ThreadLocal<InodeMetadataTableStats> inodeMetadataTableStats_;
  ThreadLocal<BlobCacheStats> blobCacheStats_;
  ThreadLocal<TreeCacheStats> treeCacheStats_;
  ThreadLocal<ScmStatusCacheStats> scmStatusCacheStats_;
  ThreadLocal<TakeoverStats> takeoverStats_;
  ThreadLocal<CheckoutStats> checkoutStats_;
  ThreadLocal<TreeInodeStats> treeInodeStats_;
  ThreadLocal<CgroupFileCacheStats> cgroupFileCacheStats_;
  ThreadLocal<CheckoutRecoveryStats> checkoutRecoveryStats_;
  ThreadLocal<MountHealthCheckStats> mountHealthCheckStats_;
  ThreadLocal<FakeStats> fakeStats_;
};

using EdenStatsPtr = RefPtr<EdenStats>;

template <>
inline FuseStats& EdenStats::getStatsForCurrentThread<FuseStats>() {
  return *fuseStats_.get();
}

template <>
inline NfsStats& EdenStats::getStatsForCurrentThread<NfsStats>() {
  return *nfsStats_.get();
}

template <>
inline PrjfsStats& EdenStats::getStatsForCurrentThread<PrjfsStats>() {
  return *prjfsStats_.get();
}

template <>
inline ObjectStoreStats&
EdenStats::getStatsForCurrentThread<ObjectStoreStats>() {
  return *objectStoreStats_.get();
}

template <>
inline SaplingBackingStoreStats&
EdenStats::getStatsForCurrentThread<SaplingBackingStoreStats>() {
  return *saplingBackingStoreStats_.get();
}

template <>
inline JournalStats& EdenStats::getStatsForCurrentThread<JournalStats>() {
  return *journalStats_.get();
}

template <>
inline GlobStats& EdenStats::getStatsForCurrentThread<GlobStats>() {
  return *globStats_.get();
}

template <>
inline ThriftStats& EdenStats::getStatsForCurrentThread<ThriftStats>() {
  return *thriftStats_.get();
}

template <>
inline TelemetryStats& EdenStats::getStatsForCurrentThread<TelemetryStats>() {
  return *telemetryStats_.get();
}

template <>
inline OverlayStats& EdenStats::getStatsForCurrentThread<OverlayStats>() {
  return *overlayStats_.get();
}

template <>
inline InodeMapStats& EdenStats::getStatsForCurrentThread<InodeMapStats>() {
  return *inodeMapStats_.get();
}

template <>
inline InodeMetadataTableStats&
EdenStats::getStatsForCurrentThread<InodeMetadataTableStats>() {
  return *inodeMetadataTableStats_.get();
}

template <>
inline BlobCacheStats& EdenStats::getStatsForCurrentThread<BlobCacheStats>() {
  return *blobCacheStats_.get();
}

template <>
inline TreeCacheStats& EdenStats::getStatsForCurrentThread<TreeCacheStats>() {
  return *treeCacheStats_.get();
}

template <>
inline ScmStatusCacheStats&
EdenStats::getStatsForCurrentThread<ScmStatusCacheStats>() {
  return *scmStatusCacheStats_.get();
}

template <>
inline TakeoverStats& EdenStats::getStatsForCurrentThread<TakeoverStats>() {
  return *takeoverStats_.get();
}

template <>
inline CheckoutStats& EdenStats::getStatsForCurrentThread<CheckoutStats>() {
  return *checkoutStats_.get();
}

template <>
inline TreeInodeStats& EdenStats::getStatsForCurrentThread<TreeInodeStats>() {
  return *treeInodeStats_.get();
}

template <>
inline CgroupFileCacheStats&
EdenStats::getStatsForCurrentThread<CgroupFileCacheStats>() {
  return *cgroupFileCacheStats_.get();
}

template <>
inline CheckoutRecoveryStats&
EdenStats::getStatsForCurrentThread<CheckoutRecoveryStats>() {
  return *checkoutRecoveryStats_.get();
}

template <>
inline MountHealthCheckStats&
EdenStats::getStatsForCurrentThread<MountHealthCheckStats>() {
  return *mountHealthCheckStats_.get();
}

template <>
inline FakeStats& EdenStats::getStatsForCurrentThread<FakeStats>() {
  return *fakeStats_.get();
}

struct FuseStats : StatsGroup<FuseStats> {
  Duration lookup{"fuse.lookup_us"};
  Counter lookupSuccessful{"fuse.lookup_successful"};
  Counter lookupFailure{"fuse.lookup_failure"};
  Duration forget{"fuse.forget_us"};
  Counter forgetSuccessful{"fuse.forget_successful"};
  Counter forgetFailure{"fuse.forget_failure"};
  Duration getattr{"fuse.getattr_us"};
  Counter getattrSuccessful{"fuse.getattr_successful"};
  Counter getattrFailure{"fuse.getattr_failure"};
  Duration setattr{"fuse.setattr_us"};
  Counter setattrSuccessful{"fuse.setattr_successful"};
  Counter setattrFailure{"fuse.setattr_failure"};
  Duration readlink{"fuse.readlink_us"};
  Counter readlinkSuccessful{"fuse.readlink_successful"};
  Counter readlinkFailure{"fuse.readlink_failure"};
  Duration mknod{"fuse.mknod_us"};
  Counter mknodSuccessful{"fuse.mknod_successful"};
  Counter mknodFailure{"fuse.mknod_failure"};
  Duration mkdir{"fuse.mkdir_us"};
  Counter mkdirSuccessful{"fuse.mkdir_successful"};
  Counter mkdirFailure{"fuse.mkdir_failure"};
  Duration unlink{"fuse.unlink_us"};
  Counter unlinkSuccessful{"fuse.unlink_successful"};
  Counter unlinkFailure{"fuse.unlink_failure"};
  Duration rmdir{"fuse.rmdir_us"};
  Counter rmdirSuccessful{"fuse.rmdir_successful"};
  Counter rmdirFailure{"fuse.rmdir_failure"};
  Duration symlink{"fuse.symlink_us"};
  Counter symlinkSuccessful{"fuse.symlink_successful"};
  Counter symlinkFailure{"fuse.symlink_failure"};
  Duration rename{"fuse.rename_us"};
  Counter renameSuccessful{"fuse.rename_successful"};
  Counter renameFailure{"fuse.rename_failure"};
  Duration rename2{"fuse.rename2_us"};
  Counter rename2Successful{"fuse.rename2_successful"};
  Counter rename2Failure{"fuse.rename2_failure"};
  Duration link{"fuse.link_us"};
  Counter linkSuccessful{"fuse.link_successful"};
  Counter linkFailure{"fuse.link_failure"};
  Duration open{"fuse.open_us"};
  Counter openSuccessful{"fuse.open_successful"};
  Counter openFailure{"fuse.open_failure"};
  Duration read{"fuse.read_us"};
  Counter readSuccessful{"fuse.read_successful"};
  Counter readFailure{"fuse.read_failure"};
  Duration write{"fuse.write_us"};
  Counter writeSuccessful{"fuse.write_successful"};
  Counter writeFailure{"fuse.write_failure"};
  Duration flush{"fuse.flush_us"};
  Counter flushSuccessful{"fuse.flush_successful"};
  Counter flushFailure{"fuse.flush_failure"};
  Duration release{"fuse.release_us"};
  Counter releaseSuccessful{"fuse.release_successful"};
  Counter releaseFailure{"fuse.release_failure"};
  Duration fsync{"fuse.fsync_us"};
  Counter fsyncSuccessful{"fuse.fsync_successful"};
  Counter fsyncFailure{"fuse.fsync_failure"};
  Duration opendir{"fuse.opendir_us"};
  Counter opendirSuccessful{"fuse.opendir_successful"};
  Counter opendirFailure{"fuse.opendir_failure"};
  Duration readdir{"fuse.readdir_us"};
  Counter readdirSuccessful{"fuse.readdir_successful"};
  Counter readdirFailure{"fuse.readdir_failure"};
  Duration releasedir{"fuse.releasedir_us"};
  Counter releasedirSuccessful{"fuse.releasedir_successful"};
  Counter releasedirFailure{"fuse.releasedir_failure"};
  Duration fsyncdir{"fuse.fsyncdir_us"};
  Counter fsyncdirSuccessful{"fuse.fsyncdir_successful"};
  Counter fsyncdirFailure{"fuse.fsyncdir_failure"};
  Duration statfs{"fuse.statfs_us"};
  Counter statfsSuccessful{"fuse.statfs_successful"};
  Counter statfsFailure{"fuse.statfs_failure"};
  Duration setxattr{"fuse.setxattr_us"};
  Counter setxattrSuccessful{"fuse.setxattr_successful"};
  Counter setxattrFailure{"fuse.setxattr_failure"};
  Duration getxattr{"fuse.getxattr_us"};
  Counter getxattrSuccessful{"fuse.getxattr_successful"};
  Counter getxattrFailure{"fuse.getxattr_failure"};
  Duration listxattr{"fuse.listxattr_us"};
  Counter listxattrSuccessful{"fuse.listxattr_successful"};
  Counter listxattrFailure{"fuse.listxattr_failure"};
  Duration removexattr{"fuse.removexattr_us"};
  Counter removexattrSuccessful{"fuse.removexattr_successful"};
  Counter removexattrFailure{"fuse.removexattr_failure"};
  Duration access{"fuse.access_us"};
  Counter accessSuccessful{"fuse.access_successful"};
  Counter accessFailure{"fuse.access_failure"};
  Duration create{"fuse.create_us"};
  Counter createSuccessful{"fuse.create_successful"};
  Counter createFailure{"fuse.create_failure"};
  Duration bmap{"fuse.bmap_us"};
  Counter bmapSuccessful{"fuse.bmap_successful"};
  Counter bmapFailure{"fuse.bmap_failure"};
  Duration ioctl{"fuse.ioctl_us"};
  Duration poll{"fuse.poll_us"};
  Duration forgetmulti{"fuse.forgetmulti_us"};
  Counter forgetmultiSuccessful{"fuse.forgetmulti_successful"};
  Counter forgetmultiFailure{"fuse.forgetmulti_failure"};
  Duration fallocate{"fuse.fallocate_us"};
  Counter fallocateSuccessful{"fuse.fallocate_successful"};
  Counter fallocateFailure{"fuse.fallocate_failure"};

  Counter dispatchImmediate{"fuse.dispatch_immediate"};
  Counter dispatchDeferred{"fuse.dispatch_deferred"};
  Counter dispatchLookupImmediate{"fuse.dispatch_lookup_immediate"};
  Counter dispatchLookupDeferred{"fuse.dispatch_lookup_deferred"};
  Counter dispatchGetattrImmediate{"fuse.dispatch_getattr_immediate"};
  Counter dispatchGetattrDeferred{"fuse.dispatch_getattr_deferred"};
  Counter dispatchOpenImmediate{"fuse.dispatch_open_immediate"};
  Counter dispatchOpenDeferred{"fuse.dispatch_open_deferred"};
  Counter dispatchReadImmediate{"fuse.dispatch_read_immediate"};
  Counter dispatchReadDeferred{"fuse.dispatch_read_deferred"};
  Counter dispatchReaddirImmediate{"fuse.dispatch_readdir_immediate"};
  Counter dispatchReaddirDeferred{"fuse.dispatch_readdir_deferred"};
  Counter dispatchGetxattrImmediate{"fuse.dispatch_getxattr_immediate"};
  Counter dispatchGetxattrDeferred{"fuse.dispatch_getxattr_deferred"};
  Counter dispatchReadlinkImmediate{"fuse.dispatch_readlink_immediate"};
  Counter dispatchReadlinkDeferred{"fuse.dispatch_readlink_deferred"};

  Counter ioUringReplySameThread{"fuse.io_uring_reply_same_thread"};
  Counter ioUringReplyCrossThread{"fuse.io_uring_reply_cross_thread"};
  Counter ioUringPreCreateQueuesSuccess{
      "fuse.io_uring_pre_create_queues_success"};
  Counter ioUringPreCreateQueuesFailure{
      "fuse.io_uring_pre_create_queues_failure"};
  Counter invalidationQueueThrottleWait{
      "fuse.invalidation.queue_throttle_wait"};
};

struct NfsStats : StatsGroup<NfsStats> {
  Duration nfsNull{"nfs.null_us"};
  Counter nfsNullSuccessful{"nfs.null_successful"};
  Counter nfsNullFailure{"nfs.null_failure"};
  Duration nfsGetattr{"nfs.getattr_us"};
  Counter nfsGetattrSuccessful{"nfs.getattr_successful"};
  Counter nfsGetattrFailure{"nfs.getattr_failure"};
  Duration nfsSetattr{"nfs.setattr_us"};
  Counter nfsSetattrSuccessful{"nfs.setattr_successful"};
  Counter nfsSetattrFailure{"nfs.setattr_failure"};
  Duration nfsLookup{"nfs.lookup_us"};
  Counter nfsLookupSuccessful{"nfs.lookup_successful"};
  Counter nfsLookupFailure{"nfs.lookup_failure"};
  Duration nfsAccess{"nfs.access_us"};
  Counter nfsAccessSuccessful{"nfs.access_successful"};
  Counter nfsAccessFailure{"nfs.access_failure"};
  Duration nfsReadlink{"nfs.readlink_us"};
  Counter nfsReadlinkSuccessful{"nfs.readlink_successful"};
  Counter nfsReadlinkFailure{"nfs.readlink_failure"};
  Duration nfsRead{"nfs.read_us"};
  Counter nfsReadSuccessful{"nfs.read_successful"};
  Counter nfsReadFailure{"nfs.read_failure"};
  Duration nfsWrite{"nfs.write_us"};
  Counter nfsWriteSuccessful{"nfs.write_successful"};
  Counter nfsWriteFailure{"nfs.write_failure"};
  Duration nfsCreate{"nfs.create_us"};
  Counter nfsCreateSuccessful{"nfs.create_successful"};
  Counter nfsCreateFailure{"nfs.create_failure"};
  Duration nfsMkdir{"nfs.mkdir_us"};
  Counter nfsMkdirSuccessful{"nfs.mkdir_successful"};
  Counter nfsMkdirFailure{"nfs.mkdir_failure"};
  Duration nfsSymlink{"nfs.symlink_us"};
  Counter nfsSymlinkSuccessful{"nfs.symlink_successful"};
  Counter nfsSymlinkFailure{"nfs.symlink_failure"};
  Duration nfsMknod{"nfs.mknod_us"};
  Counter nfsMknodSuccessful{"nfs.mknod_successful"};
  Counter nfsMknodFailure{"nfs.mknod_failure"};
  Duration nfsRemove{"nfs.remove_us"};
  Counter nfsRemoveSuccessful{"nfs.remove_successful"};
  Counter nfsRemoveFailure{"nfs.remove_failure"};
  Duration nfsRmdir{"nfs.rmdir_us"};
  Counter nfsRmdirSuccessful{"nfs.rmdir_successful"};
  Counter nfsRmdirFailure{"nfs.rmdir_failure"};
  Duration nfsRename{"nfs.rename_us"};
  Counter nfsRenameSuccessful{"nfs.rename_successful"};
  Counter nfsRenameFailure{"nfs.rename_failure"};
  Duration nfsLink{"nfs.link_us"};
  Counter nfsLinkSuccessful{"nfs.link_successful"};
  Counter nfsLinkFailure{"nfs.link_failure"};
  Duration nfsReaddir{"nfs.readdir_us"};
  Counter nfsReaddirSuccessful{"nfs.readdir_successful"};
  Counter nfsReaddirFailure{"nfs.readdir_failure"};
  Duration nfsReaddirplus{"nfs.readdirplus_us"};
  Counter nfsReaddirplusSuccessful{"nfs.readdirplus_successful"};
  Counter nfsReaddirplusFailure{"nfs.readdirplus_failure"};
  Duration nfsFsstat{"nfs.fsstat_us"};
  Counter nfsFsstatSuccessful{"nfs.fsstat_successful"};
  Counter nfsFsstatFailure{"nfs.fsstat_failure"};
  Duration nfsFsinfo{"nfs.fsinfo_us"};
  Counter nfsFsinfoSuccessful{"nfs.fsinfo_successful"};
  Counter nfsFsinfoFailure{"nfs.fsinfo_failure"};
  Duration nfsPathconf{"nfs.pathconf_us"};
  Counter nfsPathconfSuccessful{"nfs.pathconf_successful"};
  Counter nfsPathconfFailure{"nfs.pathconf_failure"};
  Duration nfsCommit{"nfs.commit_us"};
  Counter nfsCommitSuccessful{"nfs.commit_successful"};
  Counter nfsCommitFailure{"nfs.commit_failure"};

  Counter nfsRpcExtraConnection{"nfs.rpc.extra_connection"};
  Counter nfsRpcExtraConnectionRefused{"nfs.rpc.extra_connection_refused"};

  // Backpressure
  Counter nfsBackpressureJukebox{"nfs.backpressure_jukebox"};
  Counter nfsInflightAtRequest{"nfs.inflight_at_request"};

  // Requests rejected by a "block" or over-budget "rate_limit" entry in
  // nfs:uid-access-policy / nfs:gid-access-policy.
  // nfs.{access,policed,blocked}.{uid,gid}.<id> take their id from config, so
  // they are fb303 dynamic timeseries declared in Nfsd3.cpp, not members here.
  Counter nfsBlockedAccess{"nfs.blocked_access"};

  // NFS GC invalidation counters
  Counter nfsInvalidationGcAttempt{"nfs.invalidation.gc.attempt"};
  Counter nfsInvalidationGcSuccess{"nfs.invalidation.gc.success"};
  Counter nfsInvalidationGcFailure{"nfs.invalidation.gc.failure"};
  Counter nfsInvalidationGcEnoent{"nfs.invalidation.gc.enoent"};
  Counter nfsInvalidationGcStaleReply{"nfs.invalidation.gc.stale_reply"};

  Counter nfsInvalidationGcClearFsRefcountAttempt{
      "nfs.invalidation.gc.clear_fs_refcount.attempt"};
  Counter nfsInvalidationGcClearFsRefcountCleared{
      "nfs.invalidation.gc.clear_fs_refcount.cleared"};
  Counter nfsInvalidationGcClearFsRefcountSkipped{
      "nfs.invalidation.gc.clear_fs_refcount.skipped_not_loaded_or_remembered"};

  // Phase timing (aggregate across all NFS procedures)
  Duration nfsPhaseAccept{"nfs.phase_accept_us"};
  Duration nfsPhaseQueueWait{"nfs.phase_queue_wait_us"};
  Duration nfsPhaseProcessing{"nfs.phase_processing_us"};
  Duration nfsPhaseWriteWait{"nfs.phase_write_wait_us"};
  Duration nfsPhaseTotal{"nfs.phase_total_us"};

  // NFS error counters
  Counter nfsErrorPerm{"nfs.error.perm"};
  Counter nfsErrorNoEnt{"nfs.error.noent"};
  Counter nfsErrorIO{"nfs.error.io"};
  Counter nfsErrorNXIO{"nfs.error.nxio"};
  Counter nfsErrorAcces{"nfs.error.access"};
  Counter nfsErrorExist{"nfs.error.exist"};
  Counter nfsErrorXDev{"nfs.error.xdev"};
  Counter nfsErrorNoDev{"nfs.error.nodev"};
  Counter nfsErrorNotDir{"nfs.error.notdir"};
  Counter nfsErrorIsDir{"nfs.error.isdir"};
  Counter nfsErrorInval{"nfs.error.inval"};
  Counter nfsErrorFBig{"nfs.error.fbig"};
  Counter nfsErrorROFS{"nfs.error.rofs"};
  Counter nfsErrorMLink{"nfs.error.mlink"};
  Counter nfsErrorNameTooLong{"nfs.error.nametoolong"};
  Counter nfsErrorNotEmpty{"nfs.error.notempty"};
  Counter nfsErrorDQuot{"nfs.error.dquot"};
  Counter nfsErrorStale{"nfs.error.stale"};
  Counter nfsErrorNotSupp{"nfs.error.notsupp"};

  // Jukebox and ServerFault errors can stem from multiple sources, so log
  // subtypes of generic top level errors.
  Counter nfsErrorJukeboxTimedOut{"nfs.error.jukebox.timedout"};
  Counter nfsErrorJukeboxAgain{"nfs.error.jukebox.again"};
  Counter nfsErrorJukeboxNoMem{"nfs.error.jukebox.nomem"};
  Counter nfsErrorJukeboxFollyFut{"nfs.error.jukebox.follyfut"};

  // ServerFault errors can also stem from multiple sources
  Counter nfsErrorServerFaultNoErrno{"nfs.error.serverfault.no_errno"};
  Counter nfsErrorServerFaultEnfile{"nfs.error.serverfault.enfile"};
  Counter nfsErrorServerFaultDefaultErrno{
      "nfs.error.serverfault.default_errno"};
  Counter nfsErrorServerFaultDefaultEx{"nfs.error.serverfault.default_ex"};
};

struct PrjfsStats : StatsGroup<PrjfsStats> {
  Counter outOfOrderCreate{"prjfs.out_of_order_create"};
  Duration queuedFileNotification{"prjfs.queued_file_notification_us"};
  Duration filesystemSync{"prjfs.filesystem_sync_us"};
  Counter filesystemSyncSuccessful{"prjfs.filesystem_sync_successful"};
  Counter filesystemSyncFailure{"prjfs.filesystem_sync_failure"};

  Duration newFileCreated{"prjfs.newFileCreated_us"};
  Counter newFileCreatedSuccessful{"prjfs.newFileCreated_successful"};
  Counter newFileCreatedFailure{"prjfs.newFileCreated_failure"};
  Duration fileOverwritten{"prjfs.fileOverwritten_us"};
  Counter fileOverwrittenSuccessful{"prjfs.fileOverwritten_successful"};
  Counter fileOverwrittenFailure{"prjfs.fileOverwritten_failure"};

  Duration fileHandleClosedFileModified{
      "prjfs.fileHandleClosedFileModified_us"};
  Counter fileHandleClosedFileModifiedSuccessful{
      "prjfs.fileHandleClosedFileModified_successful"};
  Counter fileHandleClosedFileModifiedFailure{
      "prjfs.fileHandleClosedFileModified_failure"};
  Duration fileRenamed{"prjfs.fileRenamed_us"};
  Counter fileRenamedSuccessful{"prjfs.fileRenamed_successful"};
  Counter fileRenamedFailure{"prjfs.fileRenamed_failure"};
  Duration preDelete{"prjfs.preDelete_us"};
  Counter preDeleteSuccessful{"prjfs.preDelete_successful"};
  Counter preDeleteFailure{"prjfs.preDelete_failure"};
  Duration preRenamed{"prjfs.preRenamed_us"};
  Counter preRenamedSuccessful{"prjfs.preRenamed_successful"};
  Counter preRenamedFailure{"prjfs.preRenamed_failure"};
  Duration fileHandleClosedFileDeleted{"prjfs.fileHandleClosedFileDeleted_us"};
  Counter fileHandleClosedFileDeletedSuccessful{
      "prjfs.fileHandleClosedFileDeleted_successful"};
  Counter fileHandleClosedFileDeletedFailure{
      "prjfs.fileHandleClosedFileDeleted_failure"};
  Duration preSetHardlink{"prjfs.preSetHardlink_us"};
  Counter preSetHardlinkSuccessful{"prjfs.preSetHardlink_successful"};
  Counter preSetHardlinkFailure{"prjfs.preSetHardlink_failure"};
  Duration preConvertToFull{"prjfs.preConvertToFull_us"};
  Counter preConvertToFullSuccessful{"prjfs.preConvertToFull_successful"};
  Counter preConvertToFullFailure{"prjfs.preConvertToFull_failure"};

  Duration openDir{"prjfs.opendir_us"};
  Counter openDirSuccessful{"prjfs.opendir_successful"};
  Counter openDirFailure{"prjfs.opendir_failure"};
  Duration readDir{"prjfs.readdir_us"};
  Counter readDirSuccessful{"prjfs.readdir_successful"};
  Counter readDirFailure{"prjfs.readdir_failure"};
  Duration lookup{"prjfs.lookup_us"};
  Counter lookupSuccessful{"prjfs.lookup_successful"};
  Counter lookupFailure{"prjfs.lookup_failure"};
  Duration access{"prjfs.access_us"};
  Counter accessSuccessful{"prjfs.access_successful"};
  Counter accessFailure{"prjfs.access_failure"};
  Duration read{"prjfs.read_us"};
  Counter readSuccessful{"prjfs.read_successful"};
  Counter readFailure{"prjfs.read_failure"};

  Duration removeCachedFile{"prjfs.remove_cached_file_us"};
  Counter removeCachedFileSuccessful{"prjfs.remove_cached_file_successful"};
  Counter removeCachedFileFailure{"prjfs.remove_cached_file_failure"};
  Duration addDirectoryPlaceholder{"prjfs.add_directory_placeholder_us"};
  Counter addDirectoryPlaceholderSuccessful{
      "prjfs.add_directory_placeholder_successful"};
  Counter addDirectoryPlaceholderFailure{
      "prjfs.add_directory_placeholder_failure"};
};

/**
 * @see ObjectStore
 */
struct ObjectStoreStats : StatsGroup<ObjectStoreStats> {
  Duration getTree{"store.get_tree_us"};
  Duration getTreeMemoryDuration{"store.get_tree.memory_us"};
  Duration getTreeBackingstoreDuration{"store.get_tree.backingstore_us"};
  Duration getTreeAuxData{"store.get_tree_metadata_us"};
  Duration getTreeAuxDataMemoryDuration{"store.get_tree_metadata.memory_us"};
  Duration getTreeAuxDataBackingstoreDuration{
      "store.get_tree_metadata.backingstore_us"};
  Duration getBlob{"store.get_blob_us"};
  Duration getBlobAuxData{"store.get_blob_metadata_us"};
  Duration getBlobAuxDataMemoryDuration{"store.get_blob_metadata.memory_us"};
  Duration getBlobAuxDataBackingstoreDuration{
      "store.get_blob_metadata.backingstore_us"};
  Duration getBlobAuxDataFromBlobDuration{
      "store.get_blob_metadata.from_blob_us"};
  Duration getRootTree{"store.get_root_tree_us"};

  Counter getBlobFromMemory{"object_store.get_blob.memory"};
  Counter getBlobFromBackingStore{"object_store.get_blob.backing_store"};
  Counter getBlobFailed{"object_store.get_blob_failed"};

  Counter prewarmTreeAuxMemCacheForTreeFromBackingStore{
      "object_store.prewarm_tree_aux.mem.backing_store_tree"};
  Counter prewarmTreeAuxLocalCacheForTreeFromBackingStore{
      "object_store.prewarm_tree_aux.local.backing_store_tree"};

  Counter getTreeFromMemory{"object_store.get_tree.memory"};
  Counter getTreeFromBackingStore{"object_store.get_tree.backing_store"};
  Counter getTreeFailed{"object_store.get_tree_failed"};

  Counter getTreeAuxDataFromMemory{"object_store.get_tree_metadata.memory"};
  Counter getTreeAuxDataFromBackingStore{
      "object_store.get_tree_metadata.backing_store"};
  Counter getTreeAuxDataFailed{"object_store.get_tree_metadata_failed"};

  Counter getRootTreeFromBackingStore{
      "object_store.get_root_tree.backing_store"};
  Counter getRootTreeFailed{"object_store.get_root_tree_failed"};

  Counter getBlobAuxDataFromMemory{"object_store.get_blob_metadata.memory"};
  Counter getBlobAuxDataFromBackingStore{
      "object_store.get_blob_metadata.backing_store"};
  Counter getBlobAuxDataFromBlob{"object_store.get_blob_metadata.blob"};
  Counter getBlobAuxDataFailed{"object_store.get_blob_metadata_failed"};

  Counter checkPermission{"object_store.check_permission"};
  Counter checkPermissionFromBackingStore{
      "object_store.check_permission.backing_store"};
};

/**
 * @see SaplingBackingStore
 *
 * Terminology:
 *   get = entire lookup process, including both Sapling disk hits and fetches
 *   fetch = includes asynchronous retrieval from Mononoke
 */
struct SaplingBackingStoreStats : StatsGroup<SaplingBackingStoreStats> {
  Duration getTree{"store.sapling.get_tree_us"};
  Duration fetchTree{"store.sapling.fetch_tree_us"};
  Duration getRootTree{"store.sapling.get_root_tree_us"};
  Duration importManifestForRoot{"store.sapling.import_manifest_for_root_us"};
  Counter fetchTreeLocal{"store.sapling.fetch_tree_local"};
  Counter fetchTreeSuccess{"store.sapling.fetch_tree_success"};
  Counter fetchTreeFailure{"store.sapling.fetch_tree_failure"};
  Counter fetchTreeSuccessDogfooding{
      "store.sapling.fetch_tree_success_dogfooding"};
  Counter fetchTreeFailureDogfooding{
      "store.sapling.fetch_tree_failure_dogfooding"};
  Duration getTreeAuxData{"store.sapling.get_tree_metadata_us"};
  Duration fetchTreeAuxData{"store.sapling.fetch_tree_metadata_us"};
  Counter fetchTreeAuxDataLocal{"store.sapling.fetch_tree_metadata_local"};
  Counter fetchTreeAuxDataSuccess{"store.sapling.fetch_tree_metadata_success"};
  Counter fetchTreeAuxDataFailure{"store.sapling.fetch_tree_metadata_failure"};
  Counter getRootTreeLocal{"store.sapling.get_root_tree_local"};
  Counter getRootTreeSuccess{"store.sapling.get_root_tree_success"};
  Counter getRootTreeFailure{"store.sapling.get_root_tree_failure"};
  Counter importManifestForRootLocal{
      "store.sapling.import_manifest_for_root_local"};
  Counter importManifestForRootSuccess{
      "store.sapling.import_manifest_for_root_success"};
  Counter importManifestForRootFailure{
      "store.sapling.import_manifest_for_root_failure"};
  Duration getBlob{"store.sapling.get_blob_us"};
  Duration fetchBlob{"store.sapling.fetch_blob_us"};
  Counter fetchBlobLocal{"store.sapling.fetch_blob_local"};
  Counter fetchBlobSuccess{"store.sapling.fetch_blob_success"};
  Counter fetchBlobFailure{"store.sapling.fetch_blob_failure"};
  Counter fetchBlobSuccessDogfooding{
      "store.sapling.fetch_blob_success_dogfooding"};
  Counter fetchBlobFailureDogfooding{
      "store.sapling.fetch_blob_failure_dogfooding"};
  Duration prefetchBlob{"store.sapling.prefetch_blob_us"};
  Counter prefetchBlobLocal{"store.sapling.prefetch_blob_local"};
  Counter prefetchBlobSuccess{"store.sapling.prefetch_blob_success"};
  Counter prefetchBlobFailure{"store.sapling.prefetch_blob_failure"};
  Duration getBlobAuxData{"store.sapling.get_blob_metadata_us"};
  Duration fetchBlobAuxData{"store.sapling.fetch_blob_metadata_us"};
  Counter fetchBlobAuxDataLocal{"store.sapling.fetch_blob_metadata_local"};
  Counter fetchBlobAuxDataSuccess{"store.sapling.fetch_blob_metadata_success"};
  Counter fetchBlobAuxDataFailure{"store.sapling.fetch_blob_metadata_failure"};
  Counter loadProxyHash{"store.sapling.load_proxy_hash"};
};

struct JournalStats : StatsGroup<JournalStats> {
  Counter truncatedReads{"journal.truncated_reads"};
  Counter filesAccumulated{"journal.files_accumulated"};
  Counter journalStatusCacheHit{"journal.status_cache_hit"};
  Counter journalStatusCachePend{"journal.status_cache_pend"};
  Counter journalStatusCacheMiss{"journal.status_cache_miss"};
  Counter journalStatusCacheSkip{"journal.status_cache_skip"};
  Duration accumulateRange{"journal.accumulate_range_us"};
};

struct GlobStats : StatsGroup<GlobStats> {
  Counter memoizedFailureStateLimitExceeded{
      "glob_match.memoized_failure_state_limit_exceeded"};
  Counter backtrackingStepLimitExceeded{
      "glob_match.backtracking_step_limit_exceeded"};
};

struct ThriftStats : StatsGroup<ThriftStats> {
  Duration streamChangesSince{
      "thrift.StreamingEdenService.streamChangesSince.streaming_time_us"};
  Duration streamSelectedChangesSince{
      "thrift.StreamingEdenService.streamSelectedChangesSince.streaming_time_us"};

  Counter cancelRequestSuccess{
      "thrift.EdenServiceHandler.cancel_request.success"};
  Counter cancelRequestLongRunning{
      "thrift.EdenServiceHandler.cancel_request.long_running"};
};

struct OverlayStats : StatsGroup<OverlayStats> {
  Duration saveOverlayDir{"overlay.save_overlay_dir_us"};
  Duration loadOverlayDir{"overlay.load_overlay_dir_us"};
  Duration openOverlayFile{"overlay.open_overlay_file_us"};
  Duration createOverlayFile{"overlay.create_overlay_file_us"};
  Duration removeOverlayFile{"overlay.remove_overlay_file_us"};
  Duration removeOverlayDir{"overlay.remove_overlay_dir_us"};
  Duration recursivelyRemoveOverlayDir{
      "overlay.recursively_remove_overlay_dir_us"};
  Duration hasOverlayDir{"overlay.has_overlay_dir_us"};
  Duration hasOverlayFile{"overlay.has_overlay_file_us"};
  Duration addChild{"overlay.add_child_us"};
  Duration removeChild{"overlay.remove_child_us"};
  Duration removeChildren{"overlay.remove_children_us"};
  Duration renameChild{"overlay.rename_child_us"};
  Duration materializeChild{"overlay.materialize_child_us"};
  // Whether the fd from creating a new overlay file was handed to the
  // open-file cache, or dropped because a concurrent by-number request
  // already opened the file.
  Counter createdFdCached{"overlay.created_fd_cached"};
  Counter createdFdAlreadyOpen{"overlay.created_fd_already_open"};
  // SHA-1 requests for materialized files that had to read and hash the
  // file, rather than being served from the inode's cache or settled by a
  // size mismatch.
  Counter fileHashComputed{"overlay.file_hash_computed"};

  Counter loadOverlayDirSuccessful{"overlay.load_overlay_dir_successful"};
  Counter loadOverlayDirFailure{"overlay.load_overlay_dir_failure"};
  Counter saveOverlayDirSuccessful{"overlay.save_overlay_dir_successful"};
  Counter saveOverlayDirFailure{"overlay.save_overlay_dir_failure"};
  Counter openOverlayFileSuccessful{"overlay.open_overlay_file_successful"};
  Counter openOverlayFileFailure{"overlay.open_overlay_file_failure"};
  Counter createOverlayFileSuccessful{"overlay.create_overlay_file_successful"};
  Counter createOverlayFileFailure{"overlay.create_overlay_file_failure"};
  Counter removeOverlayFileSuccessful{"overlay.remove_overlay_file_successful"};
  Counter removeOverlayFileFailure{"overlay.remove_overlay_file_failure"};
  Counter removeOverlayDirSuccessful{"overlay.remove_overlay_dir_successful"};
  Counter removeOverlayDirFailure{"overlay.remove_overlay_dir_failure"};
  Counter recursivelyRemoveOverlayDirSuccessful{
      "overlay.recursively_remove_overlay_dir_successful"};
  Counter recursivelyRemoveOverlayDirFailure{
      "overlay.recursively_remove_overlay_dir_failure"};
  Counter hasOverlayDirSuccessful{"overlay.has_overlay_dir_successful"};
  Counter hasOverlayDirFailure{"overlay.has_overlay_dir_failure"};
  Counter hasOverlayFileSuccessful{"overlay.has_overlay_file_successful"};
  Counter hasOverlayFileFailure{"overlay.has_overlay_file_failure"};
  Counter addChildSuccessful{"overlay.add_child_successful"};
  Counter addChildFailure{"overlay.add_child_failure"};
  Counter removeChildSuccessful{"overlay.remove_child_successful"};
  Counter removeChildFailure{"overlay.remove_child_failure"};
  Counter removeChildrenSuccessful{"overlay.remove_children_successful"};
  Counter removeChildrenFailure{"overlay.remove_children_failure"};
  Counter renameChildSuccessful{"overlay.rename_child_successful"};
  Counter renameChildFailure{"overlay.rename_child_failure"};
  Counter materializeChildSuccessful{"overlay.materialize_child_successful"};
  Counter materializeChildFailure{"overlay.materialize_child_failure"};

  // WAL for deferred overlay directory writes (overlay:use-wal config gate).
  Counter walAppend{"overlay.wal_append"};
  Counter walReplay{"overlay.wal_replay"};
  Counter walEntriesReplayed{"overlay.wal_entries_replayed"};
  Counter walParseFailure{"overlay.wal_parse_failure"};
  Counter walCompaction{"overlay.wal_compaction"};
  Counter onlineFsckDirectoryRecovery{"overlay.online_fsck_directory_recovery"};
  Counter onlineFsckInodeRecovery{"overlay.online_fsck_inode_recovery"};
  Counter onlineFsckStartupSkip{"overlay.online_fsck_startup_skip"};
  // Wall time spent inside the synchronous saveOverlayDir triggered by
  // maybeCompactWal. Bounded by kCompactionCap, but worth a histogram
  // because compaction runs on the FUSE/NFS dispatch thread under the
  // parent contents lock.
  Duration walCompactionInline{"overlay.wal_compaction_inline_us"};

  // Preallocated overlay file pool (overlay:file-prealloc-pool-size gate).
  Counter preallocFileClaimed{"overlay.prealloc_file_claimed"};
  Counter preallocFileMissed{"overlay.prealloc_file_missed"};
  // A pool entry was popped but writing the initial contents failed.
  Counter preallocFileClaimFailed{"overlay.prealloc_file_claim_failed"};

  // Preallocated empty overlay dir records
  // (overlay:dir-prealloc-pool-size gate).
  Counter preallocDirClaimed{"overlay.prealloc_dir_claimed"};
  Counter preallocDirMissed{"overlay.prealloc_dir_missed"};
};

struct InodeMapStats : StatsGroup<InodeMapStats> {
  Counter lookupTreeInodeHit{"inode_map.lookup_tree_inode_hit"};
  Counter lookupBlobInodeHit{"inode_map.lookup_blob_inode_hit"};
  Counter lookupTreeInodeMiss{"inode_map.lookup_tree_inode_miss"};
  Counter lookupBlobInodeMiss{"inode_map.lookup_blob_inode_miss"};
  Counter lookupInodeError{"inode_map.lookup_inode_error"};
  Counter forgetUnknownInode{"inode_map.forget_unknown_inode"};
};

struct InodeMetadataTableStats : StatsGroup<InodeMetadataTableStats> {
  Counter getHit{"inode_metadata_table.get_hit"};
  Counter getMiss{"inode_metadata_table.get_miss"};
};

struct BlobCacheStats : StatsGroup<BlobCacheStats> {
  Counter getHit{"blob_cache.get_hit"};
  Counter getMiss{"blob_cache.get_miss"};
  Counter insertEviction{"blob_cache.insert_eviction"};
  Counter objectDrop{"blob_cache.object_drop"};
};

struct TreeCacheStats : StatsGroup<TreeCacheStats> {
  Counter getHit{"tree_cache.get_hit"};
  Counter getMiss{"tree_cache.get_miss"};
  Counter insertEviction{"tree_cache.insert_eviction"};
  Counter objectDrop{"tree_cache.object_drop"};
};

struct ScmStatusCacheStats : StatsGroup<TreeCacheStats> {
  Counter getHit{"scm_status_cache.get_hit"};
  Counter getMiss{"scm_status_cache.get_miss"};
  Counter insertEviction{"scm_status_cache.insert_eviction"};
  Counter objectDrop{"scm_status_cache.object_drop"};
};

/**
 * Tracks duration and outcome of the graceful restart (takeover) protocol.
 * sendFailure is measured on the old daemon (which stays alive on failure).
 * receiveSuccess is measured on the new daemon (which stays alive on success).
 */
struct TakeoverStats : StatsGroup<TakeoverStats> {
  Duration send{"takeover.send_us"};
  Counter sendFailure{"takeover.send_failure"};
  Duration receive{"takeover.receive_us"};
  Counter receiveSuccess{"takeover.receive_success"};
};

struct CheckoutStats : StatsGroup<CheckoutStats> {
  Counter avoidedDestinationConflicts{"checkout.avoided_destination_conflicts"};
};

struct TreeInodeStats : StatsGroup<TreeInodeStats> {
  Counter readdirIndexHit{"inodes.readdir_index_hit"};
  Counter readdirIndexCached{"inodes.readdir_index_cached"};
  Counter readdirIndexDroppedByGc{"inodes.readdir_index_dropped_by_gc"};
};

/**
 * The `local.` prefix keeps these out of the ODS export regexes; they are for
 * inspection on the host. Fleet-wide efficacy shows up as the cgroup's memory
 * usage itself.
 */
struct CgroupFileCacheStats : StatsGroup<CgroupFileCacheStats> {
  Counter reclaimedBytes{"local.memory.cgroup_file_cache.reclaimed_bytes"};
  Counter reclaimFailures{"local.memory.cgroup_file_cache.reclaim_failures"};
};

/**
 * Remounts of checkouts the daemon stopped serving without being asked to.
 */
struct CheckoutRecoveryStats : StatsGroup<CheckoutRecoveryStats> {
  Counter attempt{"checkout_recovery.attempt"};
  Counter success{"checkout_recovery.success"};
  Counter failure{"checkout_recovery.failure"};
  Counter gaveUp{"checkout_recovery.gave_up"};
  Counter notServed{"checkout_recovery.not_served"};
  Counter flapLimited{"checkout_recovery.flap_limited"};
  // Queued recoveries dropped because EdenFS left RUNNING before they mounted.
  Counter cancelled{"checkout_recovery.cancelled"};
  // Channel stops that did not trigger a recovery, by reason.
  Counter channelStopSkippedTakeover{
      "checkout_recovery.channel_stop_skipped.takeover"};
  Counter channelStopSkippedUnmountRequested{
      "checkout_recovery.channel_stop_skipped.unmount_requested"};
  Counter channelStopSkippedDaemonNotRunning{
      "checkout_recovery.channel_stop_skipped.daemon_not_running"};
  Counter channelStopSkippedExpectedStop{
      "checkout_recovery.channel_stop_skipped.expected_stop"};
  // Not served any more, intentionally unmounted, or not recoverable.
  Counter channelStopSkippedNotNeeded{
      "checkout_recovery.channel_stop_skipped.not_needed"};
  // In flight, gave up, or flapping.
  Counter channelStopSkippedThrottled{
      "checkout_recovery.channel_stop_skipped.throttled"};
};

/**
 * Probes of the checkouts the daemon is serving
 * (`mount:mount-health-check-interval`).
 */
struct MountHealthCheckStats : StatsGroup<MountHealthCheckStats> {
  Counter probes{"mount_health_check.probes"};
  Counter healthy{"mount_health_check.healthy"};
  Counter issue{"mount_health_check.issue"};
  Counter timedOut{"mount_health_check.timed_out"};
  Counter notStarted{"mount_health_check.not_started"};
  Counter hung{"mount_health_check.hung"};
  Counter slow{"mount_health_check.slow"};
  Counter hangEnded{"mount_health_check.hang_ended"};
  // Probes that confirmed the mount with statx instead of the mount table.
  Counter statxMountCheck{"mount_health_check.statx_mount_check"};
};

/*
 * This is a fake stats object that is used for testing. Counter/Duration
 * objects can be added here to mirror variables used in real stats object as
 * needed
 */
struct FakeStats : StatsGroup<FakeStats> {
  Counter getHit{"do_not_export_0"};
  Counter getMiss{"do_not_export_1"};
  Counter insertEviction{"do_not_export_2"};
  Counter objectDrop{"do_not_export_3"};
};

} // namespace facebook::eden
