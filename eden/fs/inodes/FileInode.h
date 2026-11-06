/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/ExceptionWrapper.h>
#include <folly/Synchronized.h>
#include <folly/coro/safe/NowTask.h>
#include <folly/futures/SharedPromise.h>
#include <chrono>
#include <optional>
#include "eden/common/utils/CoverageSet.h"
#include "eden/common/utils/FileOffset.h"
#include "eden/fs/inodes/CacheHint.h"
#include "eden/fs/inodes/InodeBase.h"
#include "eden/fs/model/BlobAuxData.h"
#include "eden/fs/model/Tree.h"
#include "eden/fs/store/BlobCache.h"
#include "eden/fs/store/IObjectStore.h"
#include "eden/fs/store/ImportPriority.h"
#include "eden/fs/utils/BufVec.h"

namespace folly {
class File;
}

namespace facebook::eden {

class Blob;
class ObjectFetchContext;
class ObjectStore;
class OverlayFileAccess;

/**
 * The contents of a FileInode.
 *
 * This structure exists to allow the entire contents to be protected inside
 * folly::Synchronized.  This ensures proper synchronization when accessing
 * any member variables of FileInode.
 *
 * A FileInode can be in one of three states:
 *   - not loading: the blob may be in cache, but it is not currently being
 *                  loaded
 *   - loading: fetching data from backing store, but it's not available yet
 *   - materialized: contents are written into overlay
 *
 * Valid state transitions:
 *   - not loading -> loading
 *   - not loading -> materialized (O_TRUNC)
 *   - loading -> not loaded (blob available during transition)
 *   - loading -> materialized (O_TRUNC or not)
 *   - loading -> not loading -> materialized
 */
struct FileInodeState {
  enum Tag : uint8_t {
    BLOB_NOT_LOADING,
    BLOB_LOADING,
    MATERIALIZED_IN_OVERLAY,
  };

  friend inline auto format_as(Tag t) {
    switch (t) {
      case BLOB_NOT_LOADING:
        return "BLOB_NOT_LOADING";
      case BLOB_LOADING:
        return "BLOB_LOADING";
      case MATERIALIZED_IN_OVERLAY:
        return "MATERIALIZED_IN_OVERLAY";
      default:
        return "UNKNOWN";
    }
  }

  explicit FileInodeState(const ObjectId* id);
  explicit FileInodeState();
  ~FileInodeState();

  /**
   * In lieu of std::variant, enforce the state machine invariants.
   * Called after construction and each time we unlock the state.
   */
  void checkInvariants();

  /**
   * Returns true if the file is materialized in the overlay.
   */
  bool isMaterialized() const {
    return tag == MATERIALIZED_IN_OVERLAY;
  }

  Tag tag;

  static constexpr uint64_t kUnknownSize = std::numeric_limits<uint64_t>::max();

  struct NonMaterializedState {
    ObjectId id;

    /**
     * Cached size to speedup FileInode::stat calls. The max uint64_t value is
     * used to represent a non-cached size, this is used instead of a
     * std::optional to save 8 bytes.
     */
    uint64_t size{kUnknownSize};

    explicit NonMaterializedState(const ObjectId& id) : id(id) {}
  };

  struct MaterializedState {
    /**
     * Get the sha1 for this inode.
     *
     * In the case where a sha1 is not yet cached, it will be computed and
     * stored so future calls will be served from the cache.
     */
    Hash20 getSha1(FileInode& inode);

    /**
     * Get the blake3 for this inode.
     *
     * In the case where a blake3 is not yet cached, it will be computed and
     * stored so future calls will be served from the cache.
     */
    Hash32 getBlake3(
        FileInode& inode,
        const std::optional<std::string>& maybeBlake3Key);

    /**
     * Get the file size for this inode.
     *
     * In the case where a file size is not yet cached, it will be computed and
     * stored so future calls will be served from the cache.
     */
    uint64_t getSize(FileInode& inode);

    std::optional<uint64_t> getCachedSize() const;

    /**
     * Reset the cached sha1 and size.
     *
     * This must be used for every write operation to this inode to ensure that
     * the cache is not out of sync with the Overlay/on-disk state.
     */
    void invalidate();

   private:
    std::optional<Hash20> sha1_;
    /**
     * See NonMaterializedState::size.
     * TODO: We probably want to merge NonMaterializedState::size and this one.
     */
    uint64_t size_{kUnknownSize};
  };

  union {
    /**
     * Set only in 'not loading' and 'loading' states. std::nullopt otherwise.
     */
    NonMaterializedState nonMaterializedState;
    /** Set only when materialized */
    MaterializedState materializedState;
  };

  class BlobLoadingPromise;

  /**
   * Set if 'loading'. Unset when load completes.
   *
   * It's possible for this future to complete with a null blob - that happens
   * if a truncate operation occurs during load. In that case, the future is
   * completed and the inode transitions to the materialized state without
   * a blob. Callbacks on this future must handle that case.
   */
  std::unique_ptr<BlobLoadingPromise> blobLoadingPromise;

  /**
   * If the blob has ever been loaded from cache, this handle represents this
   * inode's interest in it. By explicitly resetting the interest handle, the
   * inode indicates to the cache that the blob can be released.
   *
   * This also indicates to the cache that the blob is no longer needed in
   * memory when the FileInode is deallocated.
   *
   * Before attempting to reload the blob, check if the interestHandle has it
   * first.
   */
  BlobInterestHandle interestHandle;

#ifndef _WIN32
  /**
   * Records the ranges that have been read() when not materialized.
   */
  CoverageSet readByteRanges;
#endif
};

class FileInode final : public InodeBaseMetadata<FileInodeState> {
 public:
  using Base = InodeBaseMetadata<FileInodeState>;

  enum : int { WRONG_TYPE_ERRNO = EISDIR };

  /**
   * If id is nullptr, this opens the file in the overlay and leaves the inode
   * in MATERIALIZED_IN_OVERLAY state.  If id is set, the inode is in
   * NOT_LOADED state.
   */
  FileInode(
      InodeNumber ino,
      TreeInodePtr parentInode,
      PathComponentPiece name,
      mode_t initialMode,
      const std::optional<InodeTimestamps>& initialTimestamps,
      const ObjectId* id);

  /**
   * Construct an inode using a freshly created overlay file.
   */
  FileInode(
      InodeNumber ino,
      TreeInodePtr parentInode,
      PathComponentPiece name,
      mode_t initialMode,
      const InodeTimestamps& initialTimestamps);

  ImmediateFuture<struct stat> setattr(
      const DesiredMetadata& desired,
      const ObjectFetchContextPtr& fetchContext) override;

#ifndef _WIN32
  /// Throws InodeError EINVAL if inode is not a symbolic node.
  ImmediateFuture<std::string> readlink(
      const ObjectFetchContextPtr& fetchContext,
      CacheHint cacheHint = CacheHint::LikelyNeededAgain);

  ImmediateFuture<std::string> getxattr(
      folly::StringPiece name,
      const ObjectFetchContextPtr& context) override;
  ImmediateFuture<std::vector<std::string>> listxattr() override;
#endif

  ImmediateFuture<Hash20> getSha1(const ObjectFetchContextPtr& fetchContext);

  folly::coro::now_task<Hash20> co_getSha1(
      const ObjectFetchContextPtr& fetchContext);

  folly::coro::now_task<Hash32> co_getBlake3(
      const ObjectFetchContextPtr& fetchContext);

  ImmediateFuture<BlobAuxData> getBlobAuxData(
      const ObjectFetchContextPtr& fetchContext,
      bool blake3Required = false);

  /// Sync peek: returns blob aux only if the in-memory cache has a hit
  /// satisfying blake3Required. Returns nullopt for: cache miss,
  /// materialized inodes (defer to async to avoid overlay I/O), or
  /// blake3-missing hits when blake3Required is true.
  std::optional<BlobAuxData> tryGetCachedBlobAuxData(
      const ObjectFetchContextPtr& context,
      bool blake3Required);

  /**
   * Check to see if the file has the same contents as the specified blob
   * and the same tree entry type.
   *
   * This is more efficient than manually comparing the contents, as it may be
   * able to perform a simple id check if the file is not materialized.
   */
  ImmediateFuture<bool> isSameAs(
      const ObjectId& blobID,
      TreeEntryType entryType,
      const ObjectFetchContextPtr& fetchContext);

  folly::coro::now_task<bool> co_isSameAs(
      const ObjectId& blobID,
      const Blob& blob,
      TreeEntryType entryType,
      const ObjectFetchContextPtr& fetchContext);
  folly::coro::now_task<bool> co_isSameAs(
      const ObjectId& blobID,
      const Hash20& blobSha1,
      TreeEntryType entryType,
      const ObjectFetchContextPtr& fetchContext);
  folly::coro::now_task<bool> co_isSameAs(
      const ObjectId& blobID,
      TreeEntryType entryType,
      const ObjectFetchContextPtr& fetchContext);

  /**
   * Get the file mode_t value.
   */
  mode_t getMode() const;

#ifndef _WIN32
  /**
   * Get the file dev_t value.
   */
  dev_t getRdev() const;

  /**
   * Get the permissions bits from the file mode.
   *
   * This returns the mode with the file type bits masked out.
   */
  mode_t getPermissions() const;

  /**
   * Returns a copy of this inode's metadata.
   */
  InodeMetadata getMetadata() const override;
#endif // !_WIN32

  void forceMetadataUpdate() override;

  /**
   * If this file is backed by a source control Blob, return the ID of the
   * Blob, or return std::nullopt if this file is materialized in the overlay.
   *
   * Beware that the file's materialization state may have changed by the time
   * you use the return value of this method.  This method is primarily
   * intended for use in tests and debugging functions.  Its return value
   * generally cannot be trusted in situations where there may be concurrent
   * modifications by other threads.
   *
   * Do not call this function while the inode's lock is held.
   */
  std::optional<ObjectId> getObjectId() const override;

  /**
   * Returns whether the file is materialized in the overlay.
   *
   * Beware that the file's materialization state may have changed by the time
   * you use the return value of this method.  This method is primarily
   * intended for use in tests and debugging functions.  Its return value
   * generally cannot be trusted in situations where there may be concurrent
   * modifications by other threads.
   *
   * Do not call this function while the inode's lock is held.
   */
  bool isMaterialized() const override;

  /**
   * Read the entire file contents, and return them as a string.
   *
   * Note that this API generally should only be used for fairly small files.
   */
  [[nodiscard]] ImmediateFuture<std::string> readAll(
      const ObjectFetchContextPtr& fetchContext,
      CacheHint cacheHint = CacheHint::LikelyNeededAgain);

  /**
   * Read the entire file contents, and return them as a string.
   *
   * Note that this API generally should only be used for fairly small files.
   */
  [[nodiscard]] folly::coro::now_task<std::string> co_readAll(
      const ObjectFetchContextPtr& fetchContext,
      CacheHint cacheHint = CacheHint::LikelyNeededAgain);

  /**
   * Read up to size bytes from the file at the specified offset.
   *
   * Returns a tuple of a BufVec containing the data and a boolean
   * indicating if the end-of-file was reached.  This may return fewer bytes
   * than requested.  If the specified offset is at or past the end of the
   * buffer an empty IOBuf will be returned.  Otherwise between 1 and size
   * bytes will be returned.  If fewer than size bytes are returned this
   * does *not* guarantee that the end of the file was reached, the boolean
   * should be checked for this.
   *
   * May throw exceptions on error.
   */
  ImmediateFuture<std::tuple<BufVec, bool>>
  read(size_t size, FileOffset off, const ObjectFetchContextPtr& context);

  ImmediateFuture<size_t> write(
      BufVec&& buf,
      FileOffset off,
      const ObjectFetchContextPtr& fetchContext);

#ifdef _WIN32
  // This function will update the FileInode's state as materialized. This is a
  // Windows only function. On POSIX systems the write() functions mark a file
  // as Materialized.
  void materialize();
#else

  ImmediateFuture<size_t> write(
      folly::StringPiece data,
      FileOffset off,
      const ObjectFetchContextPtr& fetchContext);

  void fsync(bool datasync);

  [[nodiscard]] ImmediateFuture<folly::Unit> fallocate(
      uint64_t offset,
      uint64_t length,
      const ObjectFetchContextPtr& fetchContext);

  ImmediateFuture<folly::Unit> ensureMaterialized(
      const ObjectFetchContextPtr& fetchContext,
      bool followSymlink) override;

#endif // !_WIN32

  ImmediateFuture<struct stat> stat(
      const ObjectFetchContextPtr& context) override;

  folly::coro::now_task<struct stat> co_stat(
      const ObjectFetchContextPtr& context) override;

  /// Sync peek: returns the stat if fully resolvable from in-memory
  /// state, else nullopt. No logging side effects.
  ///
  /// Not const: takes the inode state lock via LockedState, which
  /// mutates the Synchronized<> wrapper.
  std::optional<struct stat> tryGetCachedStat();

  /**
   * Attributes of a file this process just created, for the entry reply.
   * Logs the access like stat() does, but does not feed the parent's readdir
   * prefetch heuristic: the client asked to create the file, not to stat it.
   */
  struct stat statNewlyCreated(const ObjectFetchContext& context);

 private:
  using State = FileInodeState;
  class LockedState;

  struct BuiltStat {
    struct stat partialStat{};
    /// Set when non-materialized cache miss — caller dispatches
    /// getBlobSize(*needsBlobSize) to fill in st_size.
    std::optional<ObjectId> needsBlobSize;
    /// Set when materialized but uncached AND peekOnly was true.
    bool materializedSizeMissing = false;
  };

  /// Build a stat from in-memory state under a single lock acquisition.
  /// peekOnly=true: never blocks on overlay I/O; sets
  ///   materializedSizeMissing instead of syscalling.
  /// peekOnly=false: may syscall to compute a missing materialized size.
  BuiltStat buildStatUnderLock(bool peekOnly);

  /**
   * Run a function with the FileInode data loaded.
   *
   * fn(state, blob) will be invoked when state->tag is either NOT_LOADING or
   * MATERIALIZED_IN_OVERLAY. If state->tag is MATERIALIZED_IN_OVERLAY,
   * state->file will be available. If state->tag is NOT_LOADING, then the
   * second argument will be a non-null std::shared_ptr<const Blob>.
   *
   * The blob parameter is used when recursing.
   *
   * Returns an ImmediateFuture with the result of fn(state_.wlock(), blob)
   */
  template <typename Fn>
  ImmediateFuture<
      std::invoke_result_t<Fn, LockedState&&, std::shared_ptr<const Blob>>>
  runWhileDataLoaded(
      LockedState state,
      BlobCache::Interest interest,
      const ObjectFetchContextPtr& fetchContext,
      std::shared_ptr<const Blob> blob,
      Fn&& fn);

#ifndef _WIN32
  /**
   * Run a function with the FileInode materialized.
   *
   * fn(state) will be invoked when state->tag is MATERIALIZED_IN_OVERLAY.
   *
   * Returns an ImmediateFuture with the result of fn(state_.wlock())
   */
  template <typename Fn>
  ImmediateFuture<std::invoke_result_t<Fn, LockedState&&>> runWhileMaterialized(
      LockedState state,
      std::shared_ptr<const Blob> blob,
      Fn&& fn,
      const ObjectFetchContextPtr& fetchContext,
      std::optional<std::chrono::system_clock::time_point> startTime =
          std::nullopt);

  /**
   * Truncate the file and then call a function.
   *
   * This immediately truncates the file, and never has to wait for data to
   * load from the ObjectStore.
   *
   * fn(state) will be invoked with state->tag set to MATERIALIZED_IN_OVERLAY.
   *
   * Returns the result of fn(state_.wlock())
   */
  template <typename Fn>
  typename std::invoke_result_t<Fn, LockedState&&> truncateAndRun(
      LockedState state,
      Fn&& fn);

#endif // !_WIN32

  class LoadingOngoing;

  /**
   * Start loading the file data.
   *
   * state->tag must be NOT_LOADED when this is called.
   *
   * This should normally only be invoked by runWhileDataLoaded() or
   * runWhileMaterialized().  Most other callers should use
   * runWhileDataLoaded() or runWhileMaterialized() instead.
   */
  [[nodiscard]] ImmediateFuture<BlobPtr> startLoadingData(
      LockedState state,
      BlobCache::Interest interest,
      const ObjectFetchContextPtr& fetchContext);

  /**
   * Start loading the file data.
   *
   * state->tag must be NOT_LOADED when this is called.
   *
   * This should normally only be invoked by runWhileDataLoaded() or
   * runWhileMaterialized().  Most other callers should use
   * runWhileDataLoaded() or runWhileMaterialized() instead.
   */
  [[nodiscard]] folly::coro::now_task<BlobPtr> co_startLoadingData(
      LockedState state,
      BlobCache::Interest interest,
      const ObjectFetchContextPtr& fetchContext);

  /**
   * Give the walk detector the parent directory's entry counts along with
   * the blob load, unless `experimental:walk-dir-size-hints` is off.
   */
  void attachParentDirEntryCounts(const ObjectFetchContextPtr& fetchContext);

  /**
   * Complete the load of a blob.
   *
   * Called internally by startLoadingData when the load completes or the
   * loading future is dropped.
   */
  void completeDataLoad(folly::Try<BlobCache::GetResult> result);

#ifndef _WIN32
  /**
   * Materialize the file as an empty file in the overlay.
   *
   * state->tag must not already be MATERIALIZED_IN_OVERLAY when this is called.
   *
   * After this function returns the caller must call materializeInParent()
   * after releasing the state lock.  If the state was previously BLOB_LOADING
   * the caller must also fulfill the blobLoadingPromise.
   *
   * This should normally only be invoked by truncateAndRun().  Most callers
   * should use truncateAndRun() instead of calling this function directly.
   */
  void materializeAndTruncate(LockedState& state);

  /**
   * Replace this file's contents in the overlay with an empty file.
   *
   * state->tag must be MATERIALIZED_IN_OVERLAY when this is called.
   *
   * This should normally only be invoked by truncateAndRun().  Most callers
   * should use truncateAndRun() instead of calling this function directly.
   */
  void truncateInOverlay(LockedState& state);

#endif // !_WIN32

  /**
   * Transition from NOT_LOADING to MATERIALIZED_IN_OVERLAY by copying the
   * blob into the overlay.
   */
  void materializeNow(
      LockedState& state,
      std::shared_ptr<const Blob> blob,
      const ObjectFetchContextPtr& fetchContext);

  /**
   * Get a FileInodePtr to ourself.
   *
   * This uses FileInodePtr::newPtrFromExisting() internally.
   *
   * This should only be called in contexts where we know an external caller
   * already has an existing reference to us.  (Which is most places--a caller
   * has to have a reference to us in order to call any of our APIs.)
   */
  FileInodePtr inodePtrFromThis() {
    return FileInodePtr::newPtrFromExisting(this);
  }

  /**
   * Mark this FileInode materialized in its parent directory.
   *
   * The state_ lock must not be held when calling this method.
   */
  void materializeInParent();

#ifndef _WIN32
  /**
   * Finish a materialization started at startTime: mark this FileInode
   * materialized in its parent directory, then publish the MATERIALIZE END
   * trace event.
   *
   * Returns the exception thrown while updating the parent, or an empty
   * exception_wrapper on success. The END event is published either way.
   *
   * The state_ lock must not be held when calling this method.
   */
  folly::exception_wrapper completeMaterialization(
      std::chrono::system_clock::time_point startTime);
#endif // !_WIN32

  /**
   * Helper function for isSameAs().
   *
   * This does the initial portion of the check which never requires an
   * ImmediateFuture.  Returns optional<bool> if the check completes
   * immediately, or std::nullopt if the contents need to be checked against
   * sha1 of file contents.
   */
  std::optional<bool> isSameAsFast(
      const ObjectId& blobID,
      TreeEntryType entryType);

  /**
   * Returns the size of the file's contents if it is materialized, and
   * std::nullopt otherwise. Costs at most an fstat of the overlay file.
   */
  std::optional<uint64_t> getMaterializedSize();

#ifndef _WIN32
  /**
   * Returns the OverlayFileAccess used to mediate access to an overlay file.
   *
   * An unused LockedState& is passed in to help avoid unsynchronized access.
   * (Don't use the returned OverlayFileAccess outside of the lock).
   */
  OverlayFileAccess* getOverlayFileAccess(LockedState&) const;

  size_t writeImpl(
      LockedState& state,
      const struct iovec* iov,
      size_t numIovecs,
      FileOffset off);
#endif // !_WIN32

  /**
   * Update the st_blocks field in a stat structure based on the st_size value.
   */
  static void updateBlockCount(struct stat& st);

#ifdef _WIN32
  /**
   * The getMaterializedFilePath() will return the Absolute path to the file in
   * the ProjectedFS cache.
   */
  AbsolutePath getMaterializedFilePath();
#endif // _WIN32

  folly::Synchronized<State> state_;

  // So it can call inodePtrFromThis() for better error messages.
  friend class ::facebook::eden::OverlayFileAccess;
  friend struct FileInodeState::MaterializedState;
};
} // namespace facebook::eden
