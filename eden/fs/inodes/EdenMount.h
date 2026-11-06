/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/CancellationToken.h>
#include <folly/Portability.h>
#include <folly/SharedMutex.h>
#include <folly/Synchronized.h>
#include <folly/ThreadLocal.h>
#include <folly/concurrency/memory/AtomicReadMostlyMainPtr.h>
#include <folly/concurrency/memory/ReadMostlySharedPtr.h>
#include <folly/coro/safe/NowTask.h>
#include <folly/futures/Future.h>
#include <folly/futures/Promise.h>
#include <folly/futures/SharedPromise.h>
#include <folly/logging/Logger.h>
#include <folly/stop_watch.h>
#include <folly/synchronization/CallOnce.h>
#include <gflags/gflags.h>
#include <chrono>
#include <cstdint>
#include <memory>
#include <mutex>
#include <optional>
#include <shared_mutex>
#include <stdexcept>

#include "eden/common/telemetry/TraceBus.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/fs/config/CheckoutConfig.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/config/InodeCatalogType.h"
#include "eden/fs/inodes/CacheHint.h"
#include "eden/fs/inodes/FsChannel.h"
#include "eden/fs/inodes/InodeNumber.h"
#include "eden/fs/inodes/InodePathCache.h"
#include "eden/fs/inodes/InodePressurePolicy.h"
#include "eden/fs/inodes/InodePtrFwd.h"
#include "eden/fs/inodes/InodeTimestamps.h"
#include "eden/fs/inodes/Overlay.h"
#include "eden/fs/inodes/VirtualInode.h"
#include "eden/fs/journal/JournalDelta.h"
#include "eden/fs/model/RootId.h"
#include "eden/fs/privhelper/PrivHelper.h"
#include "eden/fs/service/gen-cpp2/eden_types.h"
#include "eden/fs/store/BlobAccess.h"
#include "eden/fs/store/ScmStatusCache.h"
#include "eden/fs/store/ScmStatusDiffCallback.h"
#include "eden/fs/store/WalkDetector.h"
#include "eden/fs/takeover/TakeoverData.h"
#include "eden/fs/telemetry/ActivityBuffer.h"
#include "eden/fs/telemetry/IActivityRecorder.h"

#ifndef _WIN32
#include "eden/fs/inodes/OverlayFileAccess.h"
#endif

DECLARE_string(edenfsctlPath);

namespace folly {
class EventBase;
class File;

template <typename T>
class Future;
} // namespace folly

namespace facebook::eden {

class BindMount;
class BlobCache;
class CheckoutConfig;
class CheckoutConflict;
class CheckoutContext;
class Clock;
class DiffContext;
class EdenConfig;
class FuseChannel;
class FuseDeviceUnmountedDuringInitialization;
class DiffCallback;
class InodeMap;
struct InodeMetadata;
template <typename T>
class InodeTable;
using InodeMetadataTable = InodeTable<InodeMetadata>;
class Journal;
class MountPoint;
class Nfsd3;
class ObjectStore;
class Overlay;
class OverlayFileAccess;
class PrjfsChannel;
class ServerState;
class Tree;
class UnboundedQueueExecutor;
template <typename T>
class ImmediateFuture;
class TreeEntry;

class RenameLock;
class SharedRenameLock;

constexpr int kMaxSymlinkChainDepth = 40; // max depth of symlink chain

// Checkout carries RenameLock across async continuations, so release may run
// on a different worker thread from acquire. Use the same SharedMutex
// implementation without TSan's thread-affine rwlock annotations.
using RenameMutex = folly::SharedMutexSuppressTSAN;

/**
 * Represents an inode state transition and the duration it took for the event
 * to occur. Currently this tracks inode loads and inode materializations. This
 * type extends the TraceEventBase class so that events can be added to a
 * tracebus for which an ActivityBuffer subscribes to and stores events from.
 *
 * An inode materialization specifically refers to when a new version of
 * an inode's contents are saved in the overlay while before they referred
 * directly to a source control object. The duration we count for an inode
 * materialization consists of any time spent preparing/collecting file data,
 * writing the data to EdenFS's overlay, and materializing any parent inodes.
 *
 * An inode load refers to fetching state for the inode to store into memory.
 * Note: this is not the same as fetching data content for the inode. Fetching
 * data content from an hg BackingStore in particular is an HgImportTraceEvent.
 *
 * Note, path could be the full path (in the case of inode creations), or,
 * more commonly, just base filenames depending on how much is easily
 * available during the inode event.
 */
struct InodeTraceEvent : TraceEventBase {
  template <typename Path>
  InodeTraceEvent(
      std::chrono::system_clock::time_point startTime,
      InodeNumber ino,
      InodeType inodeType,
      InodeEventType eventType,
      InodeEventProgress progress,
      const Path& path)
      : InodeTraceEvent{startTime, ino, inodeType, eventType, progress} {
    setPath(path.view());
  }

  // Simple accessor that hides the internal memory representation of the trace
  // event's path. Note this could be just the base filename or it could be the
  // full path depending on how much was available and if the event has already
  // been added into the ActivityBuffer.
  std::string getPath() const {
    return path.get();
  }

  // Setter that allocates new memory on the heap and memcpy's a StringPiece's
  // data into the InodeTraceEvent's path attribute
  void setPath(std::string_view stringPath);

  InodeNumber ino;
  InodeType inodeType;
  InodeEventType eventType;
  InodeEventProgress progress;
  std::chrono::microseconds duration{};
  // Always null-terminated, and saves space in the trace event structure.
  std::shared_ptr<char[]> path;

 private:
  InodeTraceEvent(
      std::chrono::system_clock::time_point startTime,
      InodeNumber ino,
      InodeType inodeType,
      InodeEventType eventType,
      InodeEventProgress progress);
};

/**
 * Represents types of keys for some fb303 counters.
 */
enum class CounterName {
  /**
   * Represents count of loaded inodes in the current mount.
   */
  INODEMAP_LOADED,
  /**
   * Represents count of unloaded inodes in the current mount.
   */
  INODEMAP_UNLOADED,
  /**
   * Represents the amount of memory used by deltas in the change log
   */
  JOURNAL_MEMORY,
  /**
   * Represents the number of entries in the change log
   */
  JOURNAL_ENTRIES,
  /**
   * Represents the duration of the journal in seconds end to end
   */
  JOURNAL_DURATION,
  /**
   * Represents the maximum deltas iterated over in the Journal's forEachDelta
   */
  JOURNAL_MAX_FILES_ACCUMULATED,

  /**
   * Represents the number of inodes unloaded for this mount by periodic
   * linked inode unloading. This is used as an optimization to prevent inode
   * build up.
   */
  PERIODIC_INODE_UNLOAD,

  /**
   * Represents the number of inodes unloaded for this mount by periodic
   * unlinked inode unloading. This is used on NFS mounts to clean up old
   * inodes.
   */
  PERIODIC_UNLINKED_INODE_UNLOAD,
};

/**
 * Contains the uid and gid of the owner of the files in the mount
 */
struct Owner {
  uid_t uid;
  gid_t gid;
};

/**
 * Durations of the various stages of checkout.
 */
struct CheckoutTimes {
  using duration = std::chrono::steady_clock::duration;
  duration didLookupTrees{};
  duration didDiff{};
  duration didAcquireRenameLock{};
  duration didCheckout{};
  duration didFinish{};
};

/**
 * Durations of the various stages of setPathObjectId.
 */
struct SetPathObjectIdTimes {
  using duration = std::chrono::steady_clock::duration;
  duration didLookupTreesOrGetInodeByPath{};
  duration didCheckout{};
  duration didFinish{};
};

struct CheckoutResult {
  std::vector<CheckoutConflict> conflicts;
  CheckoutTimes times;
};

struct SetPathObjectIdResultAndTimes {
  SetPathObjectIdResult result;
  SetPathObjectIdTimes times;
};

struct SetPathObjectIdObjectAndPath {
  RelativePath path;
  ObjectId id;
  ObjectType type;

  std::string toString() const {
    return fmt::format(
        "path={}, objectId={}, type={}",
        path.value(),
        id.asString(),
        convertTypeToString());
  }

 private:
  std::string_view convertTypeToString() const {
    switch (type) {
      case ObjectType::TREE:
        return "tree";
      case ObjectType::REGULAR_FILE:
        return "regular_file";
      case ObjectType::EXECUTABLE_FILE:
        return "executable_file";
      case ObjectType::SYMLINK:
        return "symlink";
    }
    return "<unknown>";
  }
};

struct ParentCommitState {
  /** RootId that the working copy is checked out to */
  RootId checkedOutRootId;
  std::shared_ptr<const Tree> checkedOutRootTree;
  /**
   * RootId that the working copy is reset to. This is usually the same as
   * checkedOutRootId, except when a reset is done, in which case it will
   * differ.
   */
  RootId workingCopyParentRootId;

  /**
   * No NORMAL or FORCE checkout are currently ongoing. A DRY_RUN might be
   * started.
   */
  struct NoOngoingCheckout {};

  /* Any kind of checkout (DRY_RUN, NORMAL or FORCE) is ongoing. */
  struct CheckoutInProgress {
    // Checkout progress info should exist iff a checkout is ongoing, and
    // its existence should change in the same way a ParentCommitState does.
    // We also want this progress info to be accessible in other places, which
    // is why this is a shared pointer to the (current) checkout info struct.
    // For now this is only an uint64_t, but soon this will change to hold
    // a different struct.
    // TODO(sggutier): remove the line above once the progress info becomes
    // more complex.
    std::shared_ptr<std::atomic<uint64_t>> checkoutProgress;
  };

  /** A checkout was previously interrupted */
  struct InterruptedCheckout {
    RootId fromCommit;
    RootId toCommit;
  };

  /**
   * At mount time, the checkoutState can either be in the NoOngoingCheckout,
   * or in the InterruptedCheckout state in the case where EdenFS got killed or
   * crashed during a checkout operation.
   *
   * At the beginning of (non DRY_RUN) checkout, the checkoutState will be set
   * to CheckoutInProgress, and reset to NoOngoingCheckout at the end of it.
   *
   * The other allowed transition is from InterruptedCheckout to
   * CheckoutInProgress when resuming an interrupted checkout operation.
   */
  using CheckoutState =
      std::variant<NoOngoingCheckout, CheckoutInProgress, InterruptedCheckout>;
  CheckoutState checkoutState;

  bool isCheckoutInProgressOrInterrupted() const {
    return !std::holds_alternative<NoOngoingCheckout>(checkoutState);
  }
};

/**
 * EdenMount contains all of the data about a specific eden mount point.
 *
 * This contains:
 * - The MountPoint object which manages our FUSE interactions with the kernel.
 * - The ObjectStore object used for retrieving/storing object data.
 * - The Overlay object used for storing local changes (that have not been
 *   committed/snapshotted yet).
 */
class EdenMount : public std::enable_shared_from_this<EdenMount> {
 public:
  using State = MountState;

  /**
   * Create a shared_ptr to an EdenMount.
   *
   * The caller must call initialize() after creating the EdenMount to load data
   * required to access the mount's inodes.  No inode-related methods may be
   * called on the EdenMount until initialize() has successfully completed.
   */
  static std::shared_ptr<EdenMount> create(
      std::unique_ptr<CheckoutConfig> config,
      std::shared_ptr<ObjectStore> objectStore,
      std::shared_ptr<BlobCache> blobCache,
      std::shared_ptr<ServerState> serverState,
      std::unique_ptr<Journal> journal,
      EdenStatsPtr stats,
      std::optional<InodeCatalogType> inodeCatalogType = std::nullopt,
      std::shared_ptr<WalkDetector> walkDetector = nullptr);

  /**
   * Asynchronous EdenMount initialization - post instantiation.
   *
   * If takeover data is specified, it is used to initialize the inode map.
   *
   * On any error, the mount moves to INIT_ERROR and the returned future fails.
   */
  [[nodiscard]] ImmediateFuture<folly::Unit> initialize(
      OverlayChecker::ProgressCallback&& progressCallback = [](auto) {},
      const std::optional<SerializedInodeMap>& takeover = std::nullopt,
      const std::optional<MountProtocol>& takeoverMountProtocol = std::nullopt);

  /**
   * Destroy the EdenMount.
   *
   * This method generally does not need to be invoked directly, and will
   * instead be invoked automatically by the shared_ptr<EdenMount> returned by
   * create(), once it becomes unreferenced.
   *
   * If the EdenMount has not already been explicitly shutdown(), destroy()
   * will trigger the shutdown().  destroy() blocks until the shutdown is
   * complete, so it is advisable for callers to callers to explicitly trigger
   * shutdown() themselves if they want to ensure that the shared_ptr
   * destruction will not block on this operation.
   */
  void destroy();

  /**
   * Shutdown the EdenMount.
   *
   * This should be called *after* calling unmount() (i.e. after the FUSE mount
   * point has been unmounted from the kernel).
   *
   * This cleans up the in-memory data associated with the EdenMount, and waits
   * for all outstanding InodeBase objects to become unreferenced and be
   * destroyed.
   *
   * If doTakeover is true, this function will return populated
   * SerializedFileHandleMap and SerializedInodeMap instances generated by
   * calling FileHandleMap::serializeMap() and InodeMap::shutdown.
   *
   * If doTakeover is false, this function will return default-constructed
   * SerializedFileHandleMap and SerializedInodeMap instances.
   */
  folly::SemiFuture<SerializedInodeMap> shutdown(
      bool doTakeover,
      bool allowFuseNotStarted = false);

  /**
   * Call the umount(2) syscall to tell the kernel to remove this filesystem.
   *
   * After umount(2) succeeds, the following operations happen independently and
   * concurrently:
   *
   * * The future returned by unmount() is fulfilled successfully.
   * * The future returned by getFsChannelCompletionFuture() is fulfilled.
   *
   * If startChannel() is in progress, unmount() can cancel startChannel().
   *
   * If startChannel() is in progress, unmount() might wait for startChannel()
   * to finish before calling umount(2).
   *
   * If neither startChannel() nor takeoverFuse() has been called, unmount()
   * finishes successfully without calling umount(2). Thereafter, startChannel()
   * and takeoverFuse() will both fail with an EdenMountCancelled exception.
   *
   * unmount() is idempotent: If unmount() has already been called, this
   * function immediately returns a Future which will complete at the same time
   * the original call to unmount() completes.
   */
  [[nodiscard]] folly::SemiFuture<folly::Unit> unmount(UnmountOptions options);

  /**
   * Get the current state of this mount.
   *
   * Note that the state may be changed by another thread immediately after this
   * method is called, so this method should primarily only be used for
   * debugging & diagnostics.
   */
  State getState() const {
    return state_.load(std::memory_order_acquire);
  }

  /**
   * Check if inode operations can be performed on this EdenMount.
   *
   * This returns false for mounts that are still initializing and do not have
   * their root inode loaded yet. This also returns false for mounts that are
   * shutting down.
   */
  bool isSafeForInodeAccess() const {
    auto state = getState();
    return (
        state == State::INITIALIZED || state == State::STARTING ||
        state == State::RUNNING);
  }

  /**
   * Returns the FsChannel for this mount point. Returns nullptr if it's not
   * initialized yet.
   */
  FsChannel* FOLLY_NULLABLE getFsChannel() const;

  /**
   * Get the FUSE/NFS/Prjfs channel for this mount point.
   *
   * This should only be called after the mount point has been successfully
   * started.  (It is the caller's responsibility to perform proper
   * synchronization here with the mount start operation.  This method provides
   * no internal synchronization of its own.)
   */
  PrjfsChannel* FOLLY_NULLABLE getPrjfsChannel() const;

  /**
   * Set a test channel for this mount point.
   *
   * This should only be used in test to set a fake channel.
   */
  void setTestFsChannel(FsChannelPtr channel);

  FuseChannel* FOLLY_NULLABLE getFuseChannel() const;
#ifndef _WIN32
  std::shared_ptr<FuseChannel> getFuseChannelShared() const;
  std::shared_ptr<UnboundedQueueExecutor> getInodeGCInvalidationExecutor()
      const;
#endif
  Nfsd3* FOLLY_NULLABLE getNfsdChannel() const;

  /**
   * Return true if the mount is initialized and it's NFS. A few parts of EdenFS
   * special-case their behavior if NFS is in use.
   *
   * TODO: Ideally this function would not exist and any differences would be
   * exposed via the FsChannel interface.
   */
  bool isNfsdChannel() const;

  /**
   * This allows earlier detection if this is an NFS mount than isNfsdChannel.
   *
   * This should only be called after the mount is "Initializing". (It is the
   * caller's responsibility to perform proper synchronization here with the
   * mount start operation.  This method provides no internal synchronization of
   * its own.)
   */
  bool shouldBeOrIsNfsChannel() const;

  bool throwEstaleIfInodeIsMissing() const;

  enum class ReadLocation { InRepo, Overlay };

  /**
   * Depending on the channel type materialized files (and directories in some
   * cases) should be read directly out of the repo (ProjFS).
   */
  ReadLocation getReadLocationForMaterializedFiles() const;

  /**
   * Wait for all inflight notifications to complete.
   *
   * On Windows, inflight notifications are processed asynchronously and thus
   * the on-disk state of the repository may differ from the inode state.
   * This ensures that all pending notifications have completed.
   *
   * On macOS and Linux, this immediately return.
   *
   * This can be called from any thread/executor.
   */
  folly::coro::now_task<folly::Unit> co_waitForPendingWrites() const;

  /**
   * Test if the working copy persist on disk after this mount will be
   * destroyed.
   *
   * This is only true on Windows when using ProjectedFS as files are left
   * around in the working copy after the mount is unmounted.
   */
  constexpr bool isWorkingCopyPersistent() const {
    return folly::kIsWindows;
  }

  ProcessAccessLog& getProcessAccessLog() const;

  /**
   * Return the path to the mount point.
   */
  const AbsolutePath& getPath() const;

  /**
   * Get the RootId of the working directory's parent commit.
   *
   * EdenFS will populate the working copy from this RootId. This is set to the
   * last RootId passed to checkout.
   */
  RootId getCheckedOutRootId() const {
    return parentState_.rlock()->checkedOutRootId;
  }

  /**
   * Get the RootId of the working copy parent commit.
   *
   * This RootId is set when calling resetParent, and should be used when
   * performing diff operation. A checkout operation will also set this.
   */
  RootId getWorkingCopyParent() const {
    return parentState_.rlock()->workingCopyParentRootId;
  }

  std::optional<int64_t> getCheckoutProgress() const;

  /**
   * Return the ObjectStore used by this mount point.
   */
  const std::shared_ptr<ObjectStore>& getObjectStore() const {
    return objectStore_;
  }

  /**
   * The walk detector that watches this mount's reads, or nullptr when the
   * backing store does not detect walks.
   */
  WalkDetector* getWalkDetector() const {
    return walkDetector_.get();
  }

  /**
   * Return Eden's blob cache.
   *
   * It is guaranteed to be valid for the lifetime of the EdenMount.
   */
  BlobCache* getBlobCache() const {
    return blobCache_.get();
  }

  /**
   * Return the BlobAccess used by this mount point.
   *
   * The BlobAccess is guaranteed to be valid for the lifetime of the EdenMount.
   */
  BlobAccess* getBlobAccess() {
    return &blobAccess_;
  }

  /**
   * Return the InodeMap for this mount.
   */
  InodeMap* getInodeMap() const {
    return inodeMap_.get();
  }

  /**
   * Return a waek pointer InodeMap for this mount.
   */
  std::weak_ptr<InodeMap> getInodeMapWeak() const {
    return inodeMap_;
  }

  InodePathCache& getInodePathCache() {
    return inodePathCache_;
  }

  /**
   * Return the Overlay for this mount.
   */
  Overlay* getOverlay() const {
    return overlay_.get();
  }

#ifndef _WIN32
  OverlayFileAccess* getOverlayFileAccess() {
    return &overlayFileAccess_;
  }

#endif // !_WIN32

  InodeMetadataTable* getInodeMetadataTable() const;

  /**
   * Return the Journal used by this mount point.
   *
   * The Journal is guaranteed to be valid for the lifetime of the EdenMount.
   */
  Journal& getJournal() const {
    return *journal_;
  }

  folly::Synchronized<std::unique_ptr<IActivityRecorder>>&
  getActivityRecorder() {
    return activityRecorder_;
  }

  uint64_t getMountGeneration() const {
    return mountGeneration_;
  }

  folly::ReadMostlySharedPtr<const EdenConfig> getEdenConfig() const;

  /**
   * Get the current InodePressurePolicy. Lock-free read.
   * The policy is rebuilt by updateInodePressurePolicy(), called
   * from the periodic config reload task.
   */
  folly::ReadMostlySharedPtr<const InodePressurePolicy> getInodePressurePolicy()
      const {
    return cachedPressurePolicy_.load();
  }

  /**
   * Rebuild the InodePressurePolicy from the current EdenConfig.
   */
  void updateInodePressurePolicy();

  /**
   * Record the outcome of a completed pressure-based GC run, updating
   * isPressureGcBackedOff(). numUnloaded is what the run reclaimed: what its
   * own sweep unloaded plus the remembered inodes forgotten while it ran.
   * pinScanFailed says the run wanted a pin set and got none, so it left
   * every directory alone.
   */
  void recordPressureGcOutcome(
      uint64_t numInvalidated,
      uint64_t numUnloaded,
      bool pinScanFailed);

  /**
   * Whether pressure-based GC should wait the regular GC period before
   * running again, because the most recent pressure-based run reclaimed too
   * few of the inodes it invalidated, its pin scan failed so it could not
   * reclaim directories, or a run was cancelled for repeated tree-load
   * failures. When EdenFS tracks FS refcounts the kernel no longer
   * holds, GC invalidations fail (silently) with ENOENT and produce no
   * FORGETs, so rerunning pressure GC just re-invalidates the same inodes to
   * no effect. Cleared by the next pressure-based run that reclaims enough.
   */
  bool isPressureGcBackedOff() const {
    return pressureGcBackoff_.load(std::memory_order_relaxed);
  }

  const CheckoutConfig* getCheckoutConfig() const {
    return checkoutConfig_.get();
  }

  /**
   * Returns the server's thread pool.
   */
  const std::shared_ptr<UnboundedQueueExecutor>& getServerThreadPool() const;

#ifdef _WIN32
  /**
   * Returns the thread pool where directory invalidation need to be performed.
   */
  const std::shared_ptr<UnboundedQueueExecutor>& getInvalidationThreadPool()
      const;
#endif

  /**
   * Returns the Clock with which this mount was configured.
   */
  const Clock& getClock() const {
    return *clock_;
  }

  /**
   * Used for getting the repo name for logging purposes. This is the repo name
   * as specified by the checkout config
   */
  folly::StringPiece getRepoSourceName() const {
    return basename(checkoutConfig_->getRepoSource());
  }

  /**
   * Get the TreeInode for the root of the mount.
   *
   * This may race with the mount being torn down and the InodeMap being
   * destroyed, it is therefore unsafe to call this unless guaranteed that
   * another reference was acquired under the EdenServer::mountPoints_ lock.
   *
   * In debug builds, this invariant is checked.
   */
  TreeInodePtr getRootInode() const;

  /**
   * Same as above, but without checking the invariant.
   *
   * THIS SHOULD ONLY BE CALLED IN EdenServer::getMountAndRootInode.
   */
  TreeInodePtr getRootInodeUnchecked() const;

  /**
   * Get the inode number for the .eden dir.  Returns an empty InodeNumber
   * prior to the .eden directory being set up.
   */
  InodeNumber getDotEdenInodeNumber() const;

  /**
   * Loads and returns the Tree corresponding to the root of the mount's working
   * copy parent (commit id or root ID). Note that the returned Tree may not
   * corresponding to the mount's current inode structure.
   */
  std::shared_ptr<const Tree> getCheckedOutRootTree() const;

  /**
   * Look up the Tree or TreeEntry for the specified path.
   *
   * When the source control object referenced by the path is a file, a
   * TreeEntry will be returned, a Tree otherwise.
   *
   * This may fail with an InodeError containing ENOENT if the path does not
   * exist, or ENOTDIR if one of the intermediate components along the path is
   * not a directory.
   *
   * This may also fail with other exceptions if something else goes wrong
   * besides the path being invalid (for instance, an error loading data from
   * the ObjectStore).
   */
  ImmediateFuture<std::variant<std::shared_ptr<const Tree>, TreeEntry>>
  getTreeOrTreeEntry(
      RelativePathPiece path,
      const ObjectFetchContextPtr& context) const;

  /**
   * Walk the Tree hierarchy and return a path whose case matches the Tree path
   * components.
   *
   * This is only really useful on Windows where the casing of paths from
   * ProjectedFS is arbitrary but EdenFS must return the proper casing for
   * them. On other platform, this will simply returns the passed in path.
   *
   * This may fail with the same errors as the getTreeOrTreeEntry method.
   */
  ImmediateFuture<RelativePath> canonicalizePathFromTree(
      RelativePathPiece path,
      const ObjectFetchContextPtr& context) const;

  /**
   * Look up the Inode object for the specified path.
   *
   * This may fail with an InodeError containing ENOENT if the path does not
   * exist, or ENOTDIR if one of the intermediate components along the path is
   * not a directory.
   *
   * This may also fail with other exceptions if something else goes wrong
   * besides the path being invalid (for instance, an error loading data from
   * the ObjectStore).
   *
   * This function is marked slow due to forcing to load inodes that may not
   * have been loaded previously. Loading an Inode is unfortunately both
   * expensive to load (due to writing to the overlay), and may slow down
   * future checkout operations. The method getVirtualInode below should
   * instead be preferred as it doesn't suffer from these pathological cases.
   */
  ImmediateFuture<InodePtr> getInodeSlow(
      RelativePathPiece path,
      const ObjectFetchContextPtr& context) const;

  folly::coro::now_task<InodePtr> co_getInodeSlow(
      RelativePathPiece path,
      const ObjectFetchContextPtr& context) const;

  /**
   * Look up the Inode, Tree, or TreeEntry for the specified path.
   *
   * When the source control object referenced by the path is
   * materialized, an Inode will be returned. If the object is a file,
   * a TreeEntry will be returned, a Tree otherwise.
   *
   * This may fail with a PathBaseError containing ENOENT if the path does not
   * exist, or ENOTDIR if one of the intermediate components along the path is
   * not a directory.
   *
   * This may also fail with other exceptions if something else goes wrong
   * besides the path being invalid (for instance, an error loading data from
   * the ObjectStore).
   */
  ImmediateFuture<VirtualInode> getVirtualInode(
      RelativePathPiece path,
      const ObjectFetchContextPtr& context) const;

  folly::coro::now_task<VirtualInode> co_getVirtualInode(
      RelativePathPiece path,
      const ObjectFetchContextPtr& context) const;

  /**
   * Check out the specified commit.
   *
   * This updates the checkedOutRootId as well as the workingCopyParentRootId to
   * the passed in snapshotId.
   */
  folly::coro::now_task<CheckoutResult> co_checkout(
      TreeInodePtr rootInode,
      const RootId& snapshotId,
      const ObjectFetchContextPtr& fetchContext,
      folly::StringPiece thriftMethodCaller,
      CheckoutMode checkoutMode = CheckoutMode::NORMAL);

 private:
  class JournalDiffCallback;
  struct CheckoutInProgressGuard;
  struct CheckoutSetup;

  /**
   * Validate parent state, mark `CheckoutInProgress`, build the
   * `CheckoutContext`, and return the prepared state.
   * The parent-state lock is held only while validating and updating
   * `parentState_`.
   */
  folly::Try<CheckoutSetup> beginCheckout(
      const RootId& snapshotId,
      const ObjectFetchContextPtr& fetchContext,
      folly::StringPiece thriftMethodCaller,
      CheckoutMode checkoutMode);

  folly::Try<CheckoutResult> finalizeCheckout(
      const std::shared_ptr<CheckoutContext>& ctx,
      CheckoutMode checkoutMode,
      const CheckoutTimes& checkoutTimes,
      const folly::stop_watch<>& stopWatch,
      const RootId& oldParent,
      const RootId& snapshotId,
      JournalDiffCallback* journalDiffCallback,
      folly::Try<std::vector<CheckoutConflict>>&& conflictsResult);

 public:
  /**
   * Chown the repository to the given uid and gid
   */
  ImmediateFuture<folly::Unit>
  chown(uid_t uid, gid_t gid, const ObjectFetchContextPtr& context);

  [[nodiscard]] folly::coro::now_task<std::unique_ptr<ScmStatus>> co_diff(
      TreeInodePtr rootInode,
      const RootId& commitId,
      folly::CancellationToken cancellation,
      const ObjectFetchContextPtr& fetchContext,
      bool listIgnored = false,
      bool enforceCurrentParent = true);

  /**
   * Reset the state to point to the specified parent commit, without
   * modifying the working directory contents at all.
   *
   * This set the workingCopyParentRootId to the passed in RootId.
   */
  void resetParent(const RootId& parent);

  /**
   * Acquire the rename lock in exclusive mode.
   */
  RenameLock acquireRenameLock();

  /**
   * Acquire the rename lock in shared mode.
   */
  SharedRenameLock acquireSharedRenameLock();

  /**
   * Returns a pointer to a stats instance associated with this mountpoint.
   * Today this is the global stats instance, but in the future it will be
   * a mount point specific instance.
   */
  const EdenStatsPtr& getStats() const;

  const folly::Logger& getStraceLogger() const {
    return straceLogger_;
  }

  const std::shared_ptr<ServerState>& getServerState() const {
    return serverState_;
  }

  std::optional<ActivityBuffer<InodeTraceEvent>>& getActivityBuffer() {
    return inodeActivityBuffer_;
  }

  TraceBus<InodeTraceEvent>& getInodeTraceBus() const {
    return *inodeTraceBus_;
  }

  /**
   * Returns the last checkout time in the Eden mount.
   */
  EdenTimestamp getLastCheckoutTime() const;

  /**
   * Set the last checkout time.
   */
  void setLastCheckoutTime(EdenTimestamp time);

  /**
   * Returns true if a checkout is in progress, false otherwise.
   */
  bool isCheckoutInProgress();

  /**
   * Returns the key value to an fb303 counter.
   */
  std::string getCounterName(CounterName name);

  /**
   * Mounts the filesystem in the VFS and spawns worker threads to
   * dispatch the fuse session.
   *
   * Returns a Future that will complete as soon as the filesystem has been
   * successfully mounted, or as soon as the mount fails (state transitions
   * to RUNNING or FUSE_ERROR).
   *
   * If unmount() is called before startChannel() is called, then startChannel()
   * does the following:
   *
   * * startChannel() does not attempt to mount the filesystem
   * * The returned Future is fulfilled with an EdenMountCancelled exception
   *
   * If unmount() is called while startChannel() is in progress, then
   * startChannel() does the following:
   *
   * * The filesystem is unmounted (if it was mounted)
   * * The returned Future is fulfilled with an
   *   FuseDeviceUnmountedDuringInitialization exception
   */
  [[nodiscard]] folly::Future<folly::Unit> startFsChannel(bool readOnly);

  /**
   * Take over a FUSE channel for an existing mount point.
   *
   * This spins up worker threads to service the existing FUSE channel and
   * returns a Future that completes once the channel is ready to service
   * requests, or as soon as startup fails.
   *
   * If unmount() is called before takeoverFuse() is called, then takeoverFuse()
   * throws an EdenMountCancelled exception.
   *
   * throws a runtime_error if fuse is not supported on this platform.
   */
  folly::Future<folly::Unit> takeoverFuse(FuseChannelData takeoverData);

  /**
   * Takeover an NFSd3 channel for an existing mount point.
   *
   * This starts listening on the socket and the future completes
   * when the channel is completely setup.
   *
   * If unmount() is called before takeoverNfs() is called, then takeoverNfs()
   * throws an EdenMountCancelled exception.
   *
   * throws a runtime_error if NFS is not supported on this platform.
   */
  folly::Future<folly::Unit> takeoverNfs(NfsChannelData connectedSocket);

  /**
   * Obtains a future that will complete once the channel has wound down.
   *
   * This method may be called at any time, but the returned future will only be
   * fulfilled if startChannel() completes successfully.  If startChannel()
   * fails or is never called, the future returned by
   * getFsChannelCompletionFuture() will never complete.
   */
  [[nodiscard]] folly::Future<TakeoverData::MountInfo>
  getFsChannelCompletionFuture();

  struct FsChannelStop {
    FsStopReason reason;
    // When the channel was asked to stop, before its outstanding requests
    // finished.
    std::chrono::steady_clock::time_point time;
  };

  /**
   * Whether unmount() was called on this mount.
   */
  bool isUnmountStarted() const {
    return mountingUnmountingState_.rlock()->fsChannelUnmountStarted();
  }

  /**
   * Why and when the FsChannel stopped, or unset if it has not stopped.
   */
  std::optional<FsChannelStop> getFsChannelStop() const {
    return *fsChannelStop_.rlock();
  }

  Owner getOwner() const {
    return *owner_.rlock();
  }

  void setOwner(uid_t uid, gid_t gid) {
    auto owner = owner_.wlock();
    owner->uid = uid;
    owner->gid = gid;
  }

  /**
   * Return a new stat structure that has been minimally initialized with
   * data for this mount point.
   *
   * The caller must still initialize all file-specific data (inode number,
   * file mode, size, timestamps, link count, etc).
   */
  struct stat initStatData() const;

  /**
   * Given a mode_t, return an initial InodeMetadata.  All timestamps are set
   * to the last checkout time and uid and gid are set to the creator of the
   * mount.
   */
  struct InodeMetadata getInitialInodeMetadata(mode_t mode) const;

  /**
   * Return a newly initialized ActivityBuffer<InodeTraceEvent> for the mount if
   * using ActivityBuffers is enabled and return std::nullopt otherwise.
   */
  std::optional<ActivityBuffer<InodeTraceEvent>> initInodeActivityBuffer();

  /**
   * Subscribes inodeActivityBuffer_ to the inodeTraceBus_ in order to read and
   * store InodeTraceEvents into the ActivityBuffer as they occur. In addition,
   * path names for the inodes are calculated here outside of the critical path
   * of the inode event in order to be displayed in the eden inode tracing CLI.
   *
   * Note: subscribers will acquire the InodeMap's data_ and an InodeBase's
   * location_ lock to calculate paths for inodes. However, we must ensure
   * subscribers NEVER acquire EdenMount's Rename or a TreeInode's contents_
   * lock since inode events can still be published to the inode tracebus
   * holding those locks.
   */
  void subscribeInodeActivityBuffer();

  /**
   * Helper function to publish a new InodeTraceEvent to the mount's
   * inodeTraceBus for telemetry. Used in FileInode, TreeInode, and InodeMap.
   * This function is marked noexcept and is guaranteed to never throw an
   * exception. If tracebus fails (i.e. due to being out of memory), then this
   * exception is caught and telemetry is lost.
   *
   * Note: we must make sure to NEVER call this while holding the InodeMap's
   * data_ lock or an InodeBase's location_ lock since subscribers will also
   * attempt to acquire those locks, causing a deadlock if capacity is reached
   * and tracebus starts to block.
   */
  void publishInodeTraceEvent(InodeTraceEvent&& event) noexcept;

  /**
   * mount any configured bind mounts.
   * This requires that the filesystem already be mounted, and must not
   * be called in the context of a fuseWorkerThread().
   */
  [[nodiscard]] folly::SemiFuture<folly::Unit> performBindMounts();

  /**
   * Ensures the path `fromRoot` is a directory. If it is not, then it creates
   * subdirectories until it is. If creating a subdirectory fails, it throws an
   * exception. Returns the TreeInodePtr to the directory.
   */
  [[nodiscard]] ImmediateFuture<TreeInodePtr> ensureDirectoryExists(
      RelativePathPiece fromRoot,
      const ObjectFetchContextPtr& context);

  /** Holds GC admission for either a background GC or checkout. */
  class InodeGCLease {
   public:
    ~InodeGCLease();

    const folly::CancellationToken& getCancellationToken() const {
      return cancellationToken_;
    }

    InodeGCLease(const InodeGCLease&) = delete;
    InodeGCLease& operator=(const InodeGCLease&) = delete;
    InodeGCLease(InodeGCLease&& other) noexcept;
    InodeGCLease& operator=(InodeGCLease&&) = delete;

   private:
    friend class EdenMount;

    InodeGCLease(
        std::shared_ptr<EdenMount> mount,
        bool representsRunningGc,
        folly::CancellationToken cancellationToken)
        : mount_{std::move(mount)},
          representsRunningGc_{representsRunningGc},
          cancellationToken_{std::move(cancellationToken)} {}

    void release() noexcept;

    std::shared_ptr<EdenMount> mount_;
    bool representsRunningGc_;
    folly::CancellationToken cancellationToken_;
  };

  /**
   * Attempt to start a background inode GC.
   *
   * The returned lease must be held for the duration of the GC to ensure that
   * no other concurrent background GC can be started.
   *
   * This returns a std::nullopt if a background GC is still running or
   * checkout currently holds the GC lease.
   */
  std::optional<InodeGCLease> tryStartInodeGC(bool pressureBased = false);

  struct RunningInodeGC {
    std::chrono::steady_clock::duration age;
    bool pressureBased;
  };

  /** The running background inode GC, if any. */
  std::optional<RunningInodeGC> getRunningInodeGC() const;

  /**
   * The kernel device of this mount, as last seen by the mount health check,
   * or 0 if not seen yet.
   */
  uint64_t getHealthCheckDevice() const {
    return healthCheckDevice_.load(std::memory_order_relaxed);
  }
  void setHealthCheckDevice(uint64_t device) {
    healthCheckDevice_.store(device, std::memory_order_relaxed);
  }

  /**
   * Count a tree-load failure in the active GC run, cancelling it and backing
   * off pressure-based GC when the configured failure limit is reached.
   */
  void recordInodeGCTreeLoadFailure();

  /**
   * Cancel the active GC and supersede its lease. The returned lease prevents
   * new GCs from starting while the canceled GC finishes asynchronously.
   */
  InodeGCLease stealInodeGCLease();

  /**
   * Returns true if a GC is currently running. This is used to determine if
   * we should wait for a GC to complete before graceful restart.
   */
  bool isInodeGCRunning() const;

  /**
   * Get a weak_ptr to this EdenMount object. EdenMounts are stored as shared
   * pointers inside of EdenServer's MountList.
   */
  std::weak_ptr<EdenMount> getWeakMount() {
    return weak_from_this();
  }

  /**
   * Graft a tree or blob to a path. Returns a folly::Future that will be
   * fulfilled when the setPathObjectId operation is complete. The return result
   * include conflicts if any.
   *
   * CheckoutMode is similar to checkout:
   * 1. In NORMAL mode, new tree or blob will be incrementally added to an Eden
   * mount. The operation will not continue if any conflicts were found.
   * 2. In FORCE mode, only new tree will exist after the operation and any
   * other contents will disappear
   * 3. In DRYRUN mode, no action action will be executed.
   */
  [[nodiscard]] ImmediateFuture<SetPathObjectIdResultAndTimes>
  setPathsToObjectIds(
      std::vector<SetPathObjectIdObjectAndPath> objects,
      CheckoutMode checkoutMode,
      const ObjectFetchContextPtr& context);

  /**
   * Should only be called before the EdenMount transitions to the INITIALIZING
   * state.
   * We decide whether this mount should use nfs before initialization time and
   * do not change the decision. This is so that we can consistently determine
   * if we are determining if we are using an nfs mount without checking if the
   * channel is an NFS mount. Needed because we need to know the type of mount
   * earlier than channel object creation. This is for responding to stale
   * inodes specially for NFS and reading inodes specially off nfs on windows.
   */
  void calculateIsNfsMount(
      const std::optional<MountProtocol>& takeoverMountProtocol);
  /**
   * Clear the fs reference count for all stale inodes. Stale inodes are those
   * that have been unlinked and not recently referenced.
   *
   * "referenced" means atime changed on the inodes. This is not a perfect
   * measure as GETATTR calls do not update atime. We might want to use a
   * "referenced" time instead that we update on every inode access.
   *
   * "recently" means 10s by default and is controlled by
   * postCheckoutDelayToUnloadInodes.
   */
  void forgetStaleInodes();

  /**
   * If we have a FUSE or NFS channel, flush all invalidations we sent to the
   * kernel This will ensure that other processes will see up-to-date data once
   * we return.
   */
  [[nodiscard]] ImmediateFuture<folly::Unit> flushInvalidations();

  [[nodiscard]] folly::coro::now_task<folly::Unit> co_flushInvalidations();

 private:
  friend class RenameLock;
  friend class SharedRenameLock;

  /**
   * Convert a FsChannelPtr (unique_ptr with FsChannelDeleter) to a
   * shared_ptr and store it in channel_.
   */
  void setChannel(FsChannelPtr channel);

  /**
   * Attempt to transition from expected -> newState.
   * If the current state is expected then the state is set to newState
   * and returns boolean.
   * Otherwise the current state is left untouched and returns false.
   */
  [[nodiscard]] bool tryToTransitionState(State expected, State newState);

  /**
   * Transition from expected -> newState.
   *
   * Throws an error if the current state does not match the expected state.
   */
  void transitionState(State expected, State newState);

  /**
   * Transition from the STARTING state to the FUSE_ERROR state.
   *
   * Preconditions:
   * - `getState()` is STARTING or DESTROYING or SHUTTING_DOWN or SHUT_DOWN.
   *
   * Postconditions:
   * - If `getState()` was STARTING, `getState()` is now FUSE_ERROR.
   * - If `getState()` was not STARTING, `getState()` is unchanged.
   *
   * TODO: we should make this a bit more channel type agnostic.
   */
  void transitionToFsChannelInitializationErrorState();

  /**
   * Returns overlay type based on settings.
   */
  InodeCatalogType getInodeCatalogType(
      std::optional<InodeCatalogType> inodeCatalogType);

  EdenMount(
      std::unique_ptr<CheckoutConfig> checkoutConfig,
      std::shared_ptr<ObjectStore> objectStore,
      std::shared_ptr<BlobCache> blobCache,
      std::shared_ptr<ServerState> serverState,
      std::unique_ptr<Journal> journal,
      EdenStatsPtr stats,
      std::optional<InodeCatalogType> inodeCatalogType,
      std::shared_ptr<WalkDetector> walkDetector);

  /**
   * Tell the walk detector, if this mount has one, which commit the working
   * copy is at.
   */
  void setWalkDetectorParent(const RootId& parent);

  // Forbidden copy constructor and assignment operator
  EdenMount(EdenMount const&) = delete;
  EdenMount& operator=(EdenMount const&) = delete;

  ImmediateFuture<folly::Unit> initializeImpl(
      OverlayChecker::ProgressCallback&& progressCallback,
      const std::optional<SerializedInodeMap>& takeover);

  TreeInodePtr createRootInode(std::shared_ptr<const Tree> tree);

  [[nodiscard]] ImmediateFuture<folly::Unit> setupDotEden(TreeInodePtr root);

  folly::SemiFuture<SerializedInodeMap> shutdownImpl(bool doTakeover);

  /**
   * Create a DiffContext to be passed through the TreeInode diff codepath. This
   * will be used to record differences through the callback (in which
   * listIgnored determines if ignored files will be reported in the callback)
   * and houses the thrift request in order to check to see if the diff() should
   * be short circuited
   */
  std::unique_ptr<DiffContext> createDiffContext(
      DiffCallback* callback,
      const folly::CancellationToken& cancellation,
      const ObjectFetchContextPtr& fetchContext,
      bool listIgnored = false) const;

  /**
   * Signal to unmount() that fsChannelMount() or takeoverFuse() has started.
   *
   * beginMount() returns a reference to
   * *mountingUnmountingState_->channelMountPromise. To signal that the
   * fsChannelMount() has completed, set the promise's value (or exception)
   * without mountingUnmountingState_'s lock held.
   *
   * If unmount() was called in the past, beginMount() throws
   * EdenMountCancelled.
   *
   * Preconditions:
   * - `beginMount()` has not been called before.
   */
  [[nodiscard]] folly::Promise<folly::Unit>& beginMount();

  using StopFuture = folly::SemiFuture<FsStopDataPtr>;

  /**
   * Open the platform-specific device and mount it.
   */
  folly::Future<folly::Unit> fsChannelMount(bool readOnly);

  /**
   * Once the channel has been initialized, set up callbacks to clean up
   * correctly when it shuts down.
   */
  void fsChannelInitSuccessful(EdenMount::StopFuture fsChannelCompleteFuture);

  // TODO: This function only has one call site. Inline it.
  void preparePostFsChannelCompletion(
      EdenMount::StopFuture fsChannelCompleteFuture);

  /**
   * Private destructor.
   *
   * This should not be invoked by callers directly.  Use the destroy() method
   * above (or the EdenMountDeleter if you plan to store the EdenMount in a
   * std::unique_ptr or std::shared_ptr).
   */
  ~EdenMount();

  void releaseInodeGCLease(bool representsRunningGc) noexcept;

  const std::unique_ptr<const CheckoutConfig> checkoutConfig_;

  /**
   * A promise associated with the future returned from
   * EdenMount::getFsChannelCompletionFuture() that completes when the
   * FsChannel has no work remaining and can be torn down.
   * The future yields the underlying file descriptors (in the case of FUSE and
   * NFS); it can be passed on during graceful restart or simply closed if we're
   * unmounting and shutting down completely.  In the unmount scenario
   * the device should be closed prior to calling EdenMount::shutdown()
   * so that the subsequent privilegedFuseUnmount() call won't block
   * waiting on us for a response.
   */
  folly::Promise<TakeoverData::MountInfo> fsChannelCompletionPromise_;

  /**
   * Eden server state shared across multiple mount points.
   */
  std::shared_ptr<ServerState> serverState_;

#ifdef _WIN32
  /**
   * On Windows, directory invalidation will run on this executor.
   */
  std::shared_ptr<UnboundedQueueExecutor> invalidationExecutor_;
#else
  /** Runs bounded FUSE GC submissions away from shared executors. */
  mutable folly::once_flag inodeGCInvalidationExecutorOnce_;
  mutable std::shared_ptr<UnboundedQueueExecutor> inodeGCInvalidationExecutor_;
#endif

  /**
   * Should the created mount use NFS (only currently supported on Linux and
   * Windows). We calculate this when the mount is created based on the
   * underlying dynamic configuration.
   */
  std::optional<bool> shouldUseNFSMount_{std::nullopt};

  std::shared_ptr<InodeMap> inodeMap_;
  InodePathCache inodePathCache_;

  std::shared_ptr<ObjectStore> objectStore_;
  std::shared_ptr<WalkDetector> walkDetector_;
  std::shared_ptr<BlobCache> blobCache_;
  BlobAccess blobAccess_;
  std::shared_ptr<Overlay> overlay_;

#ifndef _WIN32
  OverlayFileAccess overlayFileAccess_;
#endif // !_WIN32
  InodeNumber dotEdenInodeNumber_{};

  /**
   * A mutex around all name-changing operations in this mount point.
   *
   * This includes rename() operations as well as unlink() and rmdir().
   * Any operation that modifies an existing InodeBase's location_ data must
   * hold the rename lock.
   */
  mutable RenameMutex renameMutex_;

  /**
   * The IDs of the parent commit of the working directory.
   */
 public:
  using ParentLock = folly::Synchronized<ParentCommitState>;

 private:
  ParentLock parentState_;

  std::shared_ptr<Journal>
      journal_; // since ScmStatusCache depends on Journal we would like Journal
                // to outlive the cache

  folly::Synchronized<std::unique_ptr<IActivityRecorder>> activityRecorder_;

  /**
   * A number to uniquely identify this particular incarnation of this mount.
   * We use bits from the process id and the time at which we were mounted.
   */
  const uint64_t mountGeneration_;

  /**
   * The path to the unix socket that can be used to address us via thrift
   */
  AbsolutePath socketPath_;

  /**
   * A log category for logging strace-events for this mount point.
   *
   * All FUSE operations to this mount point will get logged to this category.
   * The category name is of the following form: "eden.strace.<mount_path>"
   */
  folly::Logger straceLogger_;

  /**
   * The timestamp of the last time that a checkout operation was performed in
   * this mount.  This is used to initialize the timestamps of newly loaded
   * inodes.  (Since the file contents might have logically been update by the
   * checkout operation.)
   */
  std::atomic<EdenTimestamp> lastCheckoutTime_;

  struct MountingUnmountingState {
    bool fsChannelMountStarted() const noexcept;
    bool fsChannelUnmountStarted() const noexcept;

    /**
     * Whether or not the mount(2) syscall has been called (via fsChannelMount).
     *
     * Use this promise to wait for fsChannelMount to finish.
     *
     * * nullopt: fsChannelMount/mount(2) has not been called yet.
     *   (startChannel/fsChannelMount can be called.)
     * * Unfulfilled: fsChannelMount is in progress.
     * * Fulfilled with Unit: fsChannelMount completed successfully (via
     *   startChannel), or we took over the FUSE device from another process
     *   (via takeoverFuse). (startChannel or takeoverFuse can still be in
     *   progress.)
     * * Fulfilled with error: fsChannelMount failed, or fsChannelMount was
     * cancelled.
     *
     * The state of this variable might not reflect whether the file system is
     * mounted. For example, if this promise is fulfilled with Unit, then
     * umount(8) is called by another process, the file system will not be
     * mounted.
     */
    std::optional<folly::Promise<folly::Unit>> fsChannelMountPromise;

    /**
     * Whether or not unmount has been called.
     *
     * * nullopt: unmount has not been called yet. (unmount can be
     *   called.)
     * * Unfulfilled: unmount is in progress, either waiting for a concurrent
     *   fsChannelMount to complete or waiting for fuseUnmount to complete.
     * * Fulfilled with Unit: unmount was called. fuseUnmount completed
     *   successfully, or fsChannelMount was never called for this EdenMount.
     * * Fulfilled with error: unmount was called, but fuseUnmount failed.
     *
     * The state of this variable might not reflect whether the file system is
     * unmounted.
     */
    std::optional<folly::SharedPromise<folly::Unit>> fsChannelUnmountPromise;
  };

  folly::Synchronized<MountingUnmountingState> mountingUnmountingState_;

  folly::Synchronized<std::optional<FsChannelStop>> fsChannelStop_;

  /**
   * The current state of the mount point.
   */
  std::atomic<State> state_{State::UNINITIALIZED};

  /**
   * uid and gid that we'll set as the owners in the stat information
   * returned via initStatData().
   */
  folly::Synchronized<Owner> owner_;

  struct InodeGCState {
    bool gcRunning{false};
    bool pressureBased{false};
    std::chrono::steady_clock::time_point startTime;
    uint64_t inhibitorCount{0};
    uint64_t treeLoadFailureLimit{0};
    uint64_t remainingTreeLoadFailures{0};
    folly::CancellationSource cancellationSource;
  };
  folly::Synchronized<InodeGCState> inodeGCState_;

  /**
   * Fixed sized buffer containing recent inode events that have occurred within
   * EdenFS. Used in the retroactive version of the eden inode trace command.
   *
   * The initialization of this buffer depends on serverState_ being
   * initialized to get eden config information, so inodeActivityBuffer_ is
   * ordered after serverState_ in this header file. Also, inodeTraceBus_
   * subscriptions publish to inodeActivityBuffer_ so inodeActivityBuffer_ is
   * ordered before inodeTraceBus_.
   */
  std::optional<ActivityBuffer<InodeTraceEvent>> inodeActivityBuffer_;

  std::shared_ptr<TraceBus<InodeTraceEvent>> inodeTraceBus_;
  TraceSubscriptionHandle<InodeTraceEvent> inodeTraceHandle_;

  folly::AtomicReadMostlyMainPtr<FsChannel> channel_;

  /**
   * The clock.  This is also available as serverState_->getClock().
   * We still keep it as a separate member variable for now so that getClock()
   * can be inline without having to include ServerState.h in this file.
   */
  std::shared_ptr<Clock> clock_;

  mutable folly::Synchronized<std::shared_ptr<ScmStatusCache>> scmStatusCache_;

  /**
   * Cached InodePressurePolicy, rebuilt by updateInodePressurePolicy().
   */
  folly::AtomicReadMostlyMainPtr<const InodePressurePolicy>
      cachedPressurePolicy_;

  /**
   * Whether pressure-based GC should wait the regular GC period before
   * running again. See isPressureGcBackedOff().
   */
  std::atomic<bool> pressureGcBackoff_{false};

  std::atomic<uint64_t> healthCheckDevice_{0};
};

/**
 * RenameLock is a holder for an EdenMount's rename mutex.
 *
 * This is primarily useful so it can be forward declared easily,
 * but it also provides a helper method to ensure that it is currently holding
 * a lock on the desired mount.
 */
class RenameLock : public std::unique_lock<RenameMutex> {
 public:
  RenameLock() {}
  explicit RenameLock(EdenMount* mount)
      : std::unique_lock<RenameMutex>{mount->renameMutex_} {}

  bool isHeld(EdenMount* mount) const {
    return owns_lock() && (mutex() == &mount->renameMutex_);
  }
};

/**
 * SharedRenameLock is a holder for an EdenMount's rename mutex in shared mode.
 */
class SharedRenameLock : public std::shared_lock<RenameMutex> {
 public:
  explicit SharedRenameLock(EdenMount* mount)
      : std::shared_lock<RenameMutex>{mount->renameMutex_} {}

  bool isHeld(EdenMount* mount) const {
    return owns_lock() && (mutex() == &mount->renameMutex_);
  }
};

/**
 * EdenMountDeleter acts as a deleter argument for std::shared_ptr or
 * std::unique_ptr.
 */
class EdenMountDeleter {
 public:
  void operator()(EdenMount* mount) {
    mount->destroy();
  }
};

class EdenMountCancelled : public std::runtime_error {
 public:
  explicit EdenMountCancelled();
};

} // namespace facebook::eden

template <>
struct fmt::formatter<facebook::eden::InodeTraceEvent> {
  constexpr auto parse(format_parse_context& ctx) {
    return ctx.begin();
  }

  template <typename Context>
  auto format(const facebook::eden::InodeTraceEvent& event, Context& ctx)
      const {
    return fmt::format_to(
        ctx.out(),
        "Inode Trace Event: {} {} {} {} {} {} {}",
        event.systemTime.time_since_epoch().count(),
        event.eventType == facebook::eden::InodeEventType::MATERIALIZE ? "M"
                                                                       : "L",
        event.progress == facebook::eden::InodeEventProgress::START
            ? "Start"
            : (event.progress == facebook::eden::InodeEventProgress::END
                   ? "End"
                   : "Fail"),
        event.duration.count(),
        event.ino.getRawValue(),
        event.inodeType == facebook::eden::InodeType::TREE ? "Tree" : "File",
        event.getPath());
  }
};
