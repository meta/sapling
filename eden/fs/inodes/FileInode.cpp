/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/inodes/FileInode.h"

#include <fmt/format.h>
#include <optional>

#include <folly/ExceptionWrapper.h>
#include <folly/Try.h>
#include <folly/coro/Collect.h>
#include <folly/coro/Invoke.h>
#include <folly/coro/Task.h>
#include <folly/coro/safe/NowTask.h>
#include <folly/io/Cursor.h>
#include <folly/io/IOBuf.h>
#include <folly/io/async/EventBase.h>
#include <folly/logging/xlog.h>

#include "eden/common/utils/Bug.h"
#include "eden/common/utils/DirType.h"
#include "eden/common/utils/EnumValue.h"
#include "eden/common/utils/FileUtils.h"
#include "eden/common/utils/ImmediateFuture.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/UnboundedQueueExecutor.h"
#include "eden/common/utils/XAttr.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/InodeError.h"
#include "eden/fs/inodes/InodeTable.h"
#include "eden/fs/inodes/Overlay.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/model/Blob.h"
#include "eden/fs/model/Hash.h"
#include "eden/fs/store/BackingStore.h"
#include "eden/fs/store/BlobAccess.h"
#include "eden/fs/store/ObjectStore.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/utils/Clock.h"
#include "eden/fs/utils/FileHash.h"
#include "eden/fs/utils/NotImplemented.h"

using folly::StringPiece;
using std::string;
using std::vector;

namespace facebook::eden {

/*********************************************************************
 * FileInode::LockedState
 ********************************************************************/

/**
 * LockedState is a helper class that wraps
 * folly::Synchronized<State>::LockedPtr
 *
 * It implements operator->() and operator*() so it can be used just like
 * LockedPtr.
 */
class FileInode::LockedState {
 public:
  explicit LockedState(FileInode* inode) : ptr_{inode->state_.wlock()} {}
  explicit LockedState(const FileInodePtr& inode)
      : ptr_{inode->state_.wlock()} {}

  LockedState(LockedState&&) = default;
  LockedState& operator=(LockedState&&) = default;

  ~LockedState();

  State* operator->() const {
    return ptr_.operator->();
  }
  State& operator*() const {
    return ptr_.operator*();
  }

  bool isNull() const {
    return ptr_.isNull();
  }
  explicit operator bool() const {
    return !ptr_.isNull();
  }

  /**
   * Explicitly unlock the LockedState object before it is destroyed.
   */
  void unlock();

  /**
   * Move the file into the MATERIALIZED_IN_OVERLAY state.
   *
   * This updates state->tag, state->nonMaterializedState and
   * state->materializedState.
   */
  void setMaterialized();

  /**
   * If this inode still has access to a cached blob, return it.
   *
   * Can only be called when not materialized.
   */
  std::shared_ptr<const Blob> getCachedBlob(
      EdenMount* mount,
      BlobCache::Interest interest);

 private:
  folly::Synchronized<State>::LockedPtr ptr_;
};

FileInode::LockedState::~LockedState() {
  if (!ptr_) {
    return;
  }
  // Check the state invariants every time we release the lock
  ptr_->checkInvariants();
}

void FileInode::LockedState::unlock() {
  ptr_->checkInvariants();
  ptr_.unlock();
}

std::shared_ptr<const Blob> FileInode::LockedState::getCachedBlob(
    EdenMount* mount,
    BlobCache::Interest interest) {
  XCHECK(!ptr_->isMaterialized())
      << "getCachedBlob can only be called when not materialized";

  // Is the previous handle still valid? If so, return it.
  if (auto blob = ptr_->interestHandle.getObject()) {
    return blob;
  }
  // Otherwise, does the cache have one?
  //
  // The BlobAccess::getBlob call in startLoadingData on a cache miss will also
  // check the BlobCache, but by checking it here, we can avoid a transition to
  // BLOB_LOADING and back, and also avoid allocating some futures and closures.
  auto result =
      mount->getBlobCache()->get(ptr_->nonMaterializedState.id, interest);
  if (result.object) {
    ptr_->interestHandle = std::move(result.interestHandle);
    return std::move(result.object);
  }

  // If we received a read and missed cache because the blob was
  // already evicted, assume the existing readByteRanges CoverageSet
  // doesn't accurately reflect how much data is in the kernel's
  // caches.
  ptr_->interestHandle.reset();
#ifndef _WIN32
  ptr_->readByteRanges.clear();
#endif // !_WIN32

  return nullptr;
}

void FileInode::LockedState::setMaterialized() {
  if (ptr_->tag != State::MATERIALIZED_IN_OVERLAY) {
    ptr_->nonMaterializedState.~NonMaterializedState();
    new (&ptr_->materializedState) FileInodeState::MaterializedState{};
    ptr_->tag = State::MATERIALIZED_IN_OVERLAY;
  }

  ptr_->materializedState.invalidate();
  ptr_->interestHandle.reset();

#ifndef _WIN32
  ptr_->readByteRanges.clear();
#endif
}

class FileInodeState::BlobLoadingPromise {
 public:
  /**
   * Return the ImmediateFuture associated with this promise.
   *
   * This will complete either when the blob is loaded, or when the inode is
   * truncated. In the second case, the future will return with a nullptr
   */
  ImmediateFuture<BlobPtr> getImmediateFuture() {
    return ImmediateFuture<BlobPtr>{promise.getSemiFuture()}.thenError(
        [](folly::exception_wrapper&& ew) -> ImmediateFuture<BlobPtr> {
          if (ew.is_compatible_with<folly::BrokenPromise>()) {
            return BlobPtr{nullptr};
          }
          return makeImmediateFuture<BlobPtr>(std::move(ew));
        });
  }

  /**
   * Obtain the raw underlying SemiFuture.
   *
   * The main difference with the above is how this method will return an
   * error when the inode is truncated. This should only be used by
   * startLoadingData as that code needs to interrupt the loading.
   */
  folly::SemiFuture<BlobPtr> getRawSemiFuture() {
    return promise.getSemiFuture();
  }

  void setValue(BlobPtr&& blob) {
    promise.setValue(std::move(blob));
  }

  void setException(folly::exception_wrapper&& ex) {
    promise.setException(std::move(ex));
  }

 private:
  folly::SharedPromise<BlobPtr> promise;
};

/*********************************************************************
 * Implementations of FileInode private template methods
 * These definitions need to appear before any functions that use them.
 ********************************************************************/

template <typename Fn>
ImmediateFuture<std::invoke_result_t<
    Fn,
    FileInode::LockedState&&,
    std::shared_ptr<const Blob>>>
FileInode::runWhileDataLoaded(
    LockedState state,
    BlobCache::Interest interest,
    const ObjectFetchContextPtr& fetchContext,
    std::shared_ptr<const Blob> blob,
    Fn&& fn) {
  auto future = ImmediateFuture<std::shared_ptr<const Blob>>::makeEmpty();
  switch (state->tag) {
    case State::BLOB_NOT_LOADING:
      if (!blob) {
        // If no blob is given, check cache.
        blob = state.getCachedBlob(getMount(), interest);
      }
      if (blob) {
        logAccess(*fetchContext);
        // The blob was still in cache, so we can run the function immediately.
        return makeImmediateFutureWith([&] {
          return std::forward<Fn>(fn)(std::move(state), std::move(blob));
        });
      } else {
        future = startLoadingData(std::move(state), interest, fetchContext);
      }
      break;
    case State::BLOB_LOADING:
      // If we're already loading, latch on to the in-progress load
      future = state->blobLoadingPromise->getImmediateFuture();
      state.unlock();
      break;
    case State::MATERIALIZED_IN_OVERLAY:
      logAccess(*fetchContext);
      return makeImmediateFutureWith(
          [&] { return std::forward<Fn>(fn)(std::move(state), nullptr); });
  }

  return std::move(future).thenValue(
      [self = inodePtrFromThis(),
       fn = std::forward<Fn>(fn),
       interest,
       fetchContext =
           fetchContext.copy()](std::shared_ptr<const Blob> blob) mutable {
        // Simply call runWhileDataLoaded() again when we we finish loading the
        // blob data.  The state should be BLOB_NOT_LOADING or
        // MATERIALIZED_IN_OVERLAY this time around.
        auto stateLock = LockedState{self};
        XDCHECK(
            stateLock->tag == State::BLOB_NOT_LOADING ||
            stateLock->tag == State::MATERIALIZED_IN_OVERLAY)
            << "unexpected FileInode state after loading: " << stateLock->tag;
        return self->runWhileDataLoaded(
            std::move(stateLock),
            interest,
            fetchContext,
            std::move(blob),
            std::forward<Fn>(fn));
      });
}

#ifndef _WIN32
template <typename Fn>
ImmediateFuture<std::invoke_result_t<Fn, FileInode::LockedState&&>>
FileInode::runWhileMaterialized(
    LockedState state,
    std::shared_ptr<const Blob> blob,
    Fn&& fn,
    const ObjectFetchContextPtr& fetchContext,
    std::optional<std::chrono::system_clock::time_point> startTime) {
  // If we don't have a startTime and aren't materialized already, start timing
  // the upcoming materialization. If we have a startTime already, then we came
  // from a recursive call waiting for/timing how long it takes to load the blob
  if (!startTime.has_value() && !state->isMaterialized()) {
    startTime = std::chrono::system_clock::now();
    getMount()->publishInodeTraceEvent(InodeTraceEvent(
        startTime.value(),
        getNodeId(),
        InodeType::FILE,
        InodeEventType::MATERIALIZE,
        InodeEventProgress::START,
        getNameRacy()));
  }

  auto future = ImmediateFuture<BlobPtr>::makeEmpty();
  switch (state->tag) {
    case State::BLOB_NOT_LOADING:
      if (!blob) {
        // If no blob is given, check cache.
        blob = state.getCachedBlob(
            getMount(), BlobCache::Interest::UnlikelyNeededAgain);
      }
      if (blob) {
        logAccess(*fetchContext);
        // We have the blob data loaded.
        // Materialize the file now.
        materializeNow(state, blob, fetchContext);
        // Note that we explicitly create a temporary LockedState object
        // to pass to the caller to ensure that the state lock will be released
        // when they return, even if the caller's function accepts the state as
        // an rvalue-reference and does not release it themselves.
        auto result = makeImmediateFutureWith([&] {
          return std::forward<Fn>(fn)(LockedState{std::move(state)});
        });
        XCHECK(state.isNull());
        // Update our parent now that the state lock has been released.  This
        // does mean that our parent won't have updated our state until after
        // the caller's function runs, but this is okay.  There is always a
        // brief gap between when we materialize ourself and when our parent
        // gets updated to indicate this. If we do crash during this period it
        // is not too unreasonable that recent change right before the crash
        // might be reverted to their non-materialized state.
        //
        // The parent's overlay save can throw, so this must not run inside a
        // noexcept scope guard: the error is returned to the caller instead,
        // unless the caller's function already failed.
        if (auto parentError = completeMaterialization(startTime.value())) {
          return std::move(result).thenValue(
              [parentError = std::move(parentError)](
                  auto&&) -> std::invoke_result_t<Fn, LockedState&&> {
                parentError.throw_exception();
              });
        }
        return result;
      }

      // The blob must be loaded, so kick that off. There's no point in caching
      // it in memory - the blob will immediately be written into the overlay
      // and then dropped.
      future = startLoadingData(
          std::move(state),
          BlobCache::Interest::UnlikelyNeededAgain,
          fetchContext);
      break;
    case State::BLOB_LOADING:
      // If we're already loading, latch on to the in-progress load
      future = state->blobLoadingPromise->getImmediateFuture();
      state.unlock();
      break;
    case State::MATERIALIZED_IN_OVERLAY:
      logAccess(*fetchContext);
      state->materializedState.invalidate();
      return makeImmediateFutureWith(
          [&] { return std::forward<Fn>(fn)(LockedState{std::move(state)}); });
  }

  return std::move(future).thenValue(
      [self = inodePtrFromThis(),
       fn = std::forward<Fn>(fn),
       fetchContext = fetchContext.copy(),
       startTime](std::shared_ptr<const Blob> blob) mutable {
        // Simply call runWhileMaterialized() again when we we are finished
        // loading the blob data.
        auto stateLock = LockedState{self};
        XDCHECK(
            stateLock->tag == State::BLOB_NOT_LOADING ||
            stateLock->tag == State::MATERIALIZED_IN_OVERLAY)
            << "unexpected FileInode state after loading: " << stateLock->tag;
        return self->runWhileMaterialized(
            std::move(stateLock),
            std::move(blob),
            std::forward<Fn>(fn),
            fetchContext,
            startTime);
      });
}

template <typename Fn>
typename std::invoke_result_t<Fn, FileInode::LockedState&&>
FileInode::truncateAndRun(LockedState state, Fn&& fn) {
  switch (state->tag) {
    case State::BLOB_NOT_LOADING:
    case State::BLOB_LOADING: {
      // We are not materialized yet.  We need to materialize the file now.
      //
      // Note that we have to be pretty careful about ordering of operations
      // here and how we behave if an exception is thrown at any point.  We
      // want to:
      // - Truncate the file.
      // - Invoke the input function with the state lock still held.
      // - Release the state lock
      // - Assuming we successfully materialized the file, mark ourself
      //   materialized in our parent TreeInode.
      // - If we successfully materialized the file and were in the
      //   BLOB_LOADING state, fulfill the blobLoadingPromise.
      auto startTime = std::chrono::system_clock::now();
      getMount()->publishInodeTraceEvent(InodeTraceEvent(
          startTime,
          getNodeId(),
          InodeType::FILE,
          InodeEventType::MATERIALIZE,
          InodeEventProgress::START,
          getNameRacy()));

      // Call materializeAndTruncate()
      materializeAndTruncate(state);

      // Now that materializeAndTruncate() has succeeded, extract the
      // blobLoadingPromise, it'll be fulfilled with a BrokenPromise on scope
      // exit, after the parent has been updated below.
      auto loadingPromise = std::move(state->blobLoadingPromise);
      state->blobLoadingPromise.reset();

      // Now invoke the input function.
      // Note that we explicitly create a temporary LockedState object
      // to pass to the caller to ensure that the state lock will be released
      // when they return, even if the caller's function accepts the state as
      // an rvalue-reference and does not release it themselves.
      auto result = folly::makeTryWith(
          [&] { return std::forward<Fn>(fn)(LockedState{std::move(state)}); });
      XCHECK(state.isNull());

      // Update our parent now that the state lock has been released. The
      // parent's overlay save can throw, so this must not run inside a
      // noexcept scope guard: the error is returned to the caller instead,
      // unless the caller's function already failed.
      if (auto parentError = completeMaterialization(startTime)) {
        if (!result.hasException()) {
          result.emplaceException(std::move(parentError));
        }
      }
      return std::move(result).value();
    }
    case State::MATERIALIZED_IN_OVERLAY:
      // We are already materialized.
      // Truncate the file in the overlay, then call the function.
      truncateInOverlay(state);
      return std::forward<Fn>(fn)(std::move(state));
  }

  XLOGF(FATAL, "unexpected FileInode state {}", state->tag);
}
#endif // !_WIN32

/*********************************************************************
 * FileInode::State methods
 ********************************************************************/

FileInodeState::FileInodeState(const ObjectId* h) {
  if (h) {
    new (&nonMaterializedState) NonMaterializedState{*h};
    tag = BLOB_NOT_LOADING;
  } else {
    new (&materializedState) MaterializedState{};
    tag = MATERIALIZED_IN_OVERLAY;
  }
  checkInvariants();
}

FileInodeState::FileInodeState() : tag(MATERIALIZED_IN_OVERLAY) {
  new (&materializedState) MaterializedState{};
  checkInvariants();
}

/*
 * Define FileInodeState destructor explicitly to avoid including
 * some header files in FileInode.h
 */
FileInodeState::~FileInodeState() {
  switch (tag) {
    case BLOB_NOT_LOADING:
    case BLOB_LOADING:
      nonMaterializedState.~NonMaterializedState();
      break;
    case MATERIALIZED_IN_OVERLAY:
      materializedState.~MaterializedState();
      break;
  }
}

void FileInodeState::checkInvariants() {
  // FileInode is the most allocated structure in EdenFS, make sure that its
  // size is under control.
  static_assert(sizeof(NonMaterializedState) == 32);
  static_assert(
      sizeof(NonMaterializedState) + sizeof(Hash32) >=
      sizeof(MaterializedState));

  switch (tag) {
    case BLOB_NOT_LOADING:
      XCHECK(!blobLoadingPromise);
      return;
    case BLOB_LOADING:
      XCHECK(blobLoadingPromise);
#ifndef _WIN32
      XCHECK(readByteRanges.empty());
#endif
      return;
    case MATERIALIZED_IN_OVERLAY:
      // 'materialized'
      XCHECK(!blobLoadingPromise);
#ifndef _WIN32
      XCHECK(readByteRanges.empty());
#endif
      return;
  }

  XLOGF(FATAL, "Unexpected tag value: {}", tag);
}

Hash20 FileInodeState::MaterializedState::getSha1(FileInode& inode) {
  if (sha1_.has_value()) {
    return sha1_.value();
  }
  inode.getMount()->getStats()->increment(&OverlayStats::fileHashComputed);

#ifdef _WIN32
  auto sha1 = getFileSha1(inode.getMaterializedFilePath());
#else
  auto sha1 = inode.getMount()->getOverlayFileAccess()->getSha1(inode);
#endif // _WIN32

  sha1_ = sha1;
  return sha1;
}

Hash32 FileInodeState::MaterializedState::getBlake3(
    FileInode& inode,
    const std::optional<std::string>& maybeBlake3Key) {
  // always delegate to overlayFileAccess to save on the materialized state
  // memory footprint
#ifdef _WIN32
  const auto blake3 =
      getFileBlake3(inode.getMaterializedFilePath(), maybeBlake3Key);
#else
  const auto blake3 = inode.getMount()->getOverlayFileAccess()->getBlake3(
      inode, maybeBlake3Key);
#endif // _WIN32

  return blake3;
}

uint64_t FileInodeState::MaterializedState::getSize(FileInode& inode) {
  if (auto cached = getCachedSize()) {
    return *cached;
  }

#ifdef _WIN32
  auto size = getMaterializedFileSize(inode.getMaterializedFilePath()).value();
#else
  auto size = inode.getMount()->getOverlayFileAccess()->getFileSize(inode);
#endif

  size_ = size;
  return size;
}

std::optional<uint64_t> FileInodeState::MaterializedState::getCachedSize()
    const {
  if (size_ == FileInodeState::kUnknownSize) {
    return std::nullopt;
  }
  return size_;
}

void FileInodeState::MaterializedState::invalidate() {
  sha1_ = std::nullopt;
  size_ = FileInodeState::kUnknownSize;
}

/*********************************************************************
 * FileInode methods
 ********************************************************************/

// The FileInode is in NOT_LOADED or MATERIALIZED_IN_OVERLAY state.
FileInode::FileInode(
    InodeNumber ino,
    TreeInodePtr parentInode,
    PathComponentPiece name,
    mode_t initialMode,
    const std::optional<InodeTimestamps>& initialTimestamps,
    const ObjectId* id)
    : Base(ino, initialMode, initialTimestamps, std::move(parentInode), name),
      state_(std::in_place, id) {}

// The FileInode is in MATERIALIZED_IN_OVERLAY state.
FileInode::FileInode(
    InodeNumber ino,
    TreeInodePtr parentInode,
    PathComponentPiece name,
    mode_t initialMode,
    const InodeTimestamps& initialTimestamps)
    : Base(ino, initialMode, initialTimestamps, std::move(parentInode), name),
      state_(std::in_place) {}

ImmediateFuture<struct stat> FileInode::setattr(
    const DesiredMetadata& desired,
    const ObjectFetchContextPtr& fetchContext) {
#ifndef _WIN32
  if (desired.is_nop(false /* ignoreAtime */)) {
    // Short-circuit completely nop requests as early as possible, without doing
    // any additional work to fetch current metadata.
    //
    // On ARM64, macOS will send empty/nop `setattr` requests,
    // so we need to filter those out, otherwise we will cause
    // spurious notification changes.
    XLOG(DBG7, "Skipping nop setattr without ignoring `atime`");
    return this->stat(fetchContext);
  }

  // If this file is inside of .eden it cannot be reparented, so getParentRacy()
  // is okay.
  auto parent = getParentRacy();
  if (parent && parent->getNodeId() == getMount()->getDotEdenInodeNumber()) {
    return makeImmediateFuture<struct stat>(
        InodeError(EPERM, inodePtrFromThis()));
  }

  auto setAttrs = [self = inodePtrFromThis(), desired](LockedState&& state) {
    auto ino = self->getNodeId();
    auto result = self->getMount()->initStatData();

    XDCHECK_EQ(State::MATERIALIZED_IN_OVERLAY, state->tag)
        << "Must have a file in the overlay at this point";

    // Set the size of the file when desired.size is set
    if (desired.size.has_value()) {
      // Throws upon error.
      self->getOverlayFileAccess(state)->truncate(*self, desired.size.value());
      state->materializedState.invalidate();
    }

    auto metadata = self->getMount()->getInodeMetadataTable()->modifyOrThrow(
        ino, [&](auto& metadata) {
          metadata.updateFromDesired(self->getClock(), desired);
        });

    // We need to call fstat function here to get the size of the overlay
    // file. We might update size in the result while truncating the file
    // when desired.size flag is set but when the flag is not set we
    // have to return the correct size of the file even if some size is sent
    // in attr.st.st_size.
    auto size = state->materializedState.getSize(*self);
    result.st_ino = ino.get();
    result.st_size = size;
    metadata.applyToStat(result);
    result.st_nlink = 1;
    updateBlockCount(result);

    // Update the Journal
    self->updateJournal();
    return result;
  };

  // Minor optimization: if we know that the file is being completely truncated
  // as part of this operation, there's no need to fetch the underlying data,
  // so use truncateAndRun() rather than runWhileMaterialized()
  bool truncate = desired.size.has_value() && desired.size.value() == 0;
  auto state = LockedState{this};
  if (truncate) {
    return truncateAndRun(std::move(state), setAttrs);
  } else {
    return runWhileMaterialized(
        std::move(state), nullptr, setAttrs, fetchContext);
  }
#else
  (void)desired;
  (void)fetchContext;
  // neither overlay access nor Inode metadata table is supported on Windows
  return makeImmediateFutureWith([]() -> struct stat { NOT_IMPLEMENTED(); });
#endif
}

#ifndef _WIN32
ImmediateFuture<std::string> FileInode::readlink(
    const ObjectFetchContextPtr& fetchContext,
    CacheHint cacheHint) {
  if (dtype_t::Symlink != getType()) {
    // man 2 readlink says:  EINVAL The named file is not a symbolic link.
    throw InodeError(EINVAL, inodePtrFromThis(), "not a symlink");
  }

  // The symlink contents are simply the file contents!
  return readAll(fetchContext, cacheHint)
      .thenValue([self = inodePtrFromThis()](std::string target) {
        // Conflict recovery leaves an empty target so the link can be removed.
        // Linux can interpret an empty target as its parent during traversal.
        if (target.empty()) {
          throw InodeError(EIO, self, "empty symlink target");
        }
        return target;
      });
}
#endif // !_WIN32

std::optional<bool> FileInode::isSameAsFast(
    const ObjectId& blobID,
    TreeEntryType entryType) {
  auto state = state_.rlock();
#ifndef _WIN32
  if (entryType != treeEntryTypeFromMode(getMetadataLocked(*state).mode)) {
    return false;
  }
#else
  // On Windows, files can be symlinks or regular files.
  if (compareTreeEntryType(entryType, TreeEntryType::SYMLINK)) {
    if (!isSymlink()) {
      return false;
    }
  } else if (!compareTreeEntryType(entryType, TreeEntryType::REGULAR_FILE)) {
    return false;
  }
#endif // !_WIN32

  if (state->isMaterialized()) {
    // Materialized files must be manually compared with the blob contents.
    return std::nullopt;
  }

  switch (getObjectStore().compareObjectsById(
      state->nonMaterializedState.id, blobID)) {
    case ObjectComparison::Unknown:
      return std::nullopt;
    case ObjectComparison::Identical:
      return true;
    case ObjectComparison::Different:
      return false;
  }
  EDEN_BUG() << "unexpected ObjectComparison result";
}

ImmediateFuture<bool> FileInode::isSameAs(
    const ObjectId& blobID,
    TreeEntryType entryType,
    const ObjectFetchContextPtr& fetchContext) {
  auto result = isSameAsFast(blobID, entryType);
  if (result.has_value()) {
    return result.value();
  }

  auto f1 = getSha1(fetchContext);
  auto f2 = getObjectStore().getBlobSha1(blobID, fetchContext);
  return collectAllSafe(f1, f2).thenTry(
      [](folly::Try<std::tuple<Hash20, Hash20>>&& try_) {
        if (try_.hasException()) {
          XLOGF(DBG2, "Assuming changed: {}", try_.exception());
          return false;
        } else {
          auto ids = std::move(try_).value();
          return std::get<0>(ids) == std::get<1>(ids);
        }
      });
}

// Slow path: fetch SHA-1 (cached on the inode or computed from the backing
// blob) and compare against the expected value.
folly::coro::now_task<bool> co_isSameAsSlowImpl(
    FileInode* self,
    Hash20 expectedBlobSha1,
    ObjectFetchContextPtr fetchContext) {
  auto try_ = co_await folly::coro::co_awaitTry(self->co_getSha1(fetchContext));
  if (try_.hasException()) {
    XLOGF(DBG2, "Assuming changed: {}", try_.exception());
    co_return false;
  }
  co_return std::move(try_).value() == expectedBlobSha1;
}

folly::coro::now_task<bool> FileInode::co_isSameAs(
    const ObjectId& id,
    const Blob& blob,
    TreeEntryType entryType,
    const ObjectFetchContextPtr& fetchContext) {
  auto result = isSameAsFast(id, entryType);
  if (result.has_value()) {
    co_return result.value();
  }
  co_return co_await co_isSameAsSlowImpl(
      this, Hash20::sha1(blob.getContents()), fetchContext.copy());
}

folly::coro::now_task<bool> FileInode::co_isSameAs(
    const ObjectId& blobID,
    const Hash20& blobSha1,
    TreeEntryType entryType,
    const ObjectFetchContextPtr& fetchContext) {
  auto result = isSameAsFast(blobID, entryType);
  if (result.has_value()) {
    co_return result.value();
  }
  co_return co_await co_isSameAsSlowImpl(this, blobSha1, fetchContext.copy());
}

folly::coro::now_task<bool> FileInode::co_isSameAs(
    const ObjectId& blobID,
    TreeEntryType entryType,
    const ObjectFetchContextPtr& fetchContext) {
  auto result = isSameAsFast(blobID, entryType);
  if (result.has_value()) {
    co_return result.value();
  }

  // The blob's size arrives together with its SHA-1, and the size of a
  // materialized file is at most an fstat away, so a size mismatch settles
  // the comparison without reading and hashing the file.
  auto auxTry = co_await folly::coro::co_awaitTry(
      getObjectStore().co_getBlobAuxData(blobID, fetchContext));
  if (auxTry.hasException()) {
    XLOGF(DBG2, "Assuming changed: {}", auxTry.exception());
    co_return false;
  }
  auto sizeTry = folly::makeTryWith([&] { return getMaterializedSize(); });
  if (sizeTry.hasException()) {
    XLOGF(DBG2, "Assuming changed: {}", sizeTry.exception());
    co_return false;
  }
  if (sizeTry.value().has_value() && *sizeTry.value() != auxTry->size) {
    co_return false;
  }
  co_return co_await co_isSameAsSlowImpl(
      this, auxTry->sha1, fetchContext.copy());
}

std::optional<uint64_t> FileInode::getMaterializedSize() {
  auto state = LockedState{this};
  if (state->tag != State::MATERIALIZED_IN_OVERLAY) {
    return std::nullopt;
  }
  return state->materializedState.getSize(*this);
}

#ifndef _WIN32
mode_t FileInode::getMode() const {
  return getMetadata().mode;
}

mode_t FileInode::getPermissions() const {
  return (getMode() & 07777);
}

InodeMetadata FileInode::getMetadata() const {
  auto lock = state_.rlock();
  return getMetadataLocked(*lock);
}

#else
mode_t FileInode::getMode() const {
  // On Windows we only store the dir type info and no permissions bits here.
  // For file it will always be a regular file.
  return _S_IFREG;
}
#endif // !_WIN32

void FileInode::forceMetadataUpdate() {
  auto state = state_.wlock();
  InodeBaseMetadata::updateMtimeAndCtimeLocked(*state, getNow());
}

std::optional<ObjectId> FileInode::getObjectId() const {
  if (auto state = state_.rlock(); !state->isMaterialized()) {
    return state->nonMaterializedState.id;
  } else {
    return std::nullopt;
  }
}

bool FileInode::isMaterialized() const {
  return state_.rlock()->isMaterialized();
}

void FileInode::materializeInParent() {
  auto renameLock = getMount()->acquireRenameLock();
  auto loc = getLocationInfo(renameLock);
  if (loc.parent && !loc.unlinked) {
    loc.parent->childMaterialized(renameLock, loc.name);
  }
}

#ifndef _WIN32
folly::exception_wrapper FileInode::completeMaterialization(
    std::chrono::system_clock::time_point startTime) {
  auto parentError = folly::try_and_catch([this] { materializeInParent(); });
  if (parentError) {
    XLOGF(
        ERR,
        "failed to record the materialization of {} in its parent: {}",
        getLogPath(),
        folly::exceptionStr(parentError));
  }
  getMount()->publishInodeTraceEvent(InodeTraceEvent(
      startTime,
      getNodeId(),
      InodeType::FILE,
      InodeEventType::MATERIALIZE,
      InodeEventProgress::END,
      getNameRacy()));
  return parentError;
}
#endif // !_WIN32

#ifndef _WIN32
ImmediateFuture<vector<string>> FileInode::listxattr() {
  vector<string> attributes;
  // TODO: Re-evaluate if we should return a valid list of attributes now that
  // appledouble files can be turned off via an EdenFS config option.
  //
  // We used to return kXattrSha1 here for regular files, but
  // that caused some annoying behavior with appledouble
  // metadata files being created by various tools that wanted
  // to preserve all of these attributes across copy on macos.
  // So now we just return an empty set on all systems.
  return attributes;
}

ImmediateFuture<string> FileInode::getxattr(
    StringPiece name,
    const ObjectFetchContextPtr& context) {
  // Currently, we only support the xattr for the SHA-1 and BLAKE3 of a regular
  // file.
  if (name == kXattrSha1) {
    return getSha1(context).thenValue(
        [](Hash20 hash) { return hash.toString(); });
  }

  if (name == kXattrBlake3 || name == kXattrDigestHash) {
    return ImmediateFuture<string>{
        // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
        folly::coro::co_invoke(
            [](FileInodePtr self,
               ObjectFetchContextPtr context) -> folly::coro::Task<string> {
              co_return (co_await self->co_getBlake3(context)).toString();
            },
            inodePtrFromThis(),
            context.copy())
            .semi()};
  }

  return makeImmediateFuture<string>(InodeError(kENOATTR, inodePtrFromThis()));
}
#else

AbsolutePath FileInode::getMaterializedFilePath() {
  auto filePath = getPath();
  if (!filePath.has_value()) {
    throw InodeError(
        EINVAL,
        inodePtrFromThis(),
        fmt::format("{}: File is unlinked", getLogPath()));
  }
  return getMount()->getPath() + filePath.value();
}

#endif

// DEPRECATED: use co_getSha1 directly. Kept only because FileInode::getxattr
// and the future FileInode::isSameAs implementations still consume
// ImmediateFuture chains.
ImmediateFuture<Hash20> FileInode::getSha1(
    const ObjectFetchContextPtr& fetchContext) {
  auto state = LockedState{this};

  logAccess(*fetchContext);
  switch (state->tag) {
    case State::BLOB_NOT_LOADING:
    case State::BLOB_LOADING:
      // If a file is not materialized, it should have a id value.
      return getObjectStore().getBlobSha1(
          state->nonMaterializedState.id, fetchContext);
    case State::MATERIALIZED_IN_OVERLAY:
      return makeImmediateFutureWith(
          [&] { return state->materializedState.getSha1(*this); });
  }

  XLOGF(FATAL, "FileInode in illegal state: {}", state->tag);
}

folly::coro::now_task<Hash20> FileInode::co_getSha1(
    const ObjectFetchContextPtr& fetchContext) {
  auto state = LockedState{this};

  logAccess(*fetchContext);
  if (state->tag == State::BLOB_NOT_LOADING ||
      state->tag == State::BLOB_LOADING) {
    auto id = state->nonMaterializedState.id;
    state.unlock();
    co_return co_await getObjectStore().co_getBlobSha1(id, fetchContext);
  } else if (state->tag == State::MATERIALIZED_IN_OVERLAY) {
    co_return state->materializedState.getSha1(*this);
  }

  XLOGF(FATAL, "FileInode in illegal state: {}", state->tag);
}

folly::coro::now_task<Hash32> FileInode::co_getBlake3(
    const ObjectFetchContextPtr& fetchContext) {
  auto state = LockedState{this};

  logAccess(*fetchContext);
  if (state->tag == State::BLOB_NOT_LOADING ||
      state->tag == State::BLOB_LOADING) {
    auto id = state->nonMaterializedState.id;
    state.unlock();
    // If a file is not materialized, it should have a id value.
    co_return co_await getObjectStore().co_getBlobBlake3(id, fetchContext);
  } else if (state->tag == State::MATERIALIZED_IN_OVERLAY) {
    co_return state->materializedState.getBlake3(
        *this, getMount()->getEdenConfig()->blake3Key.getValue());
  }

  XLOGF(FATAL, "FileInode in illegal state: {}", state->tag);
}

ImmediateFuture<BlobAuxData> FileInode::getBlobAuxData(
    const ObjectFetchContextPtr& fetchContext,
    bool blake3Required) {
  auto state = LockedState{this};
  logAccess(*fetchContext);
  switch (state->tag) {
    case State::BLOB_NOT_LOADING:
    case State::BLOB_LOADING:
      // If a file is not materialized, it should have a id value.
      return getObjectStore().getBlobAuxData(
          state->nonMaterializedState.id, fetchContext, blake3Required);
    case State::MATERIALIZED_IN_OVERLAY:
      return makeImmediateFutureWith([&] {
        return BlobAuxData{
            state->materializedState.getSha1(*this),
            state->materializedState.getBlake3(
                *this, getMount()->getEdenConfig()->blake3Key.getValue()),
            state->materializedState.getSize(*this)};
      });
  }

  XLOGF(FATAL, "FileInode in illegal state: {}", state->tag);
}

std::optional<BlobAuxData> FileInode::tryGetCachedBlobAuxData(
    const ObjectFetchContextPtr& context,
    bool blake3Required) {
  // Pure peek: no logAccess. Callers log at entry.
  auto state = LockedState{this};
  switch (state->tag) {
    case State::BLOB_NOT_LOADING:
    case State::BLOB_LOADING: {
      return getObjectStore().getBlobAuxDataFromInMemoryCache(
          state->nonMaterializedState.id, context, blake3Required);
    }
    case State::MATERIALIZED_IN_OVERLAY:
      // Materialized blob aux may block on overlay I/O — defer to async.
      return std::nullopt;
  }
  XLOGF(FATAL, "FileInode in illegal state: {}", state->tag);
}

ImmediateFuture<struct stat> FileInode::stat(
    const ObjectFetchContextPtr& context) {
  logAccess(*context);

  auto built = buildStatUnderLock(false);
  if (!built.needsBlobSize.has_value()) {
    return built.partialStat;
  }

  // ObjectId snapshotted under the lock; dispatch without re-holding.
  auto sizeFut = getObjectStore().getBlobSize(*built.needsBlobSize, context);
  return std::move(sizeFut).thenValue([self = inodePtrFromThis(),
                                       st = built.partialStat](
                                          const uint64_t size) mutable {
    if (auto lockedState = LockedState{self}; !lockedState->isMaterialized()) {
      lockedState->nonMaterializedState.size = size;
    }
    st.st_size = size;
    self->updateBlockCount(st);
    return st;
  });
}

folly::coro::now_task<struct stat> FileInode::co_stat(
    const ObjectFetchContextPtr& context) {
  // Keep this FileInode alive across the co_await.
  [[maybe_unused]] auto self = inodePtrFromThis();
  logAccess(*context);

  auto built = buildStatUnderLock(false);
  if (!built.needsBlobSize.has_value()) {
    co_return built.partialStat;
  }

  auto size =
      co_await getObjectStore().co_getBlobSize(*built.needsBlobSize, context);

  if (auto lockedState = LockedState{this}; !lockedState->isMaterialized()) {
    lockedState->nonMaterializedState.size = size;
  }
  built.partialStat.st_size = size;
  updateBlockCount(built.partialStat);
  co_return built.partialStat;
}

struct stat FileInode::statNewlyCreated(const ObjectFetchContext& context) {
  logAccess(context);
  auto built = buildStatUnderLock(false);
  XDCHECK(!built.needsBlobSize.has_value());
  return built.partialStat;
}

std::optional<struct stat> FileInode::tryGetCachedStat() {
  auto built = buildStatUnderLock(true);
  if (built.needsBlobSize.has_value() || built.materializedSizeMissing) {
    return std::nullopt;
  }
  return built.partialStat;
}

FileInode::BuiltStat FileInode::buildStatUnderLock(bool peekOnly) {
  BuiltStat result;
  result.partialStat = getMount()->initStatData();
  result.partialStat.st_nlink = 1;
  result.partialStat.st_ino = getNodeId().get();
  auto state = LockedState{this};
#ifndef _WIN32
  getMetadataLocked(*state).applyToStat(result.partialStat);
#endif
  if (state->isMaterialized()) {
    auto size = peekOnly
        ? state->materializedState.getCachedSize()
        : std::optional{state->materializedState.getSize(*this)};
    if (!size.has_value()) {
      result.materializedSizeMissing = true;
      return result;
    }
    result.partialStat.st_size = *size;
    updateBlockCount(result.partialStat);
    return result;
  }
  if (state->nonMaterializedState.size != FileInodeState::kUnknownSize) {
    result.partialStat.st_size = state->nonMaterializedState.size;
    updateBlockCount(result.partialStat);
    return result;
  }
  // Snapshot under the lock so callers don't need to re-acquire.
  result.needsBlobSize = state->nonMaterializedState.id;
  return result;
}

void FileInode::updateBlockCount([[maybe_unused]] struct stat& st) {
  // win32 does not have stat::st_blocks
#ifndef _WIN32
  // Compute a value to store in st_blocks based on st_size.
  // Note that st_blocks always refers to 512 byte blocks, regardless of the
  // value we report in st.st_blksize.
  static constexpr FileOffset kBlockSize = 512;
  st.st_blocks = ((st.st_size + kBlockSize - 1) / kBlockSize);
#endif
}

#ifndef _WIN32
void FileInode::fsync(bool datasync) {
  auto state = LockedState{this};
  if (state->isMaterialized()) {
    getOverlayFileAccess(state)->fsync(*this, datasync);
  }
}

ImmediateFuture<folly::Unit> FileInode::fallocate(
    uint64_t offset,
    uint64_t length,
    const ObjectFetchContextPtr& fetchContext) {
  return runWhileMaterialized(
      LockedState{this},
      nullptr,
      [offset, length, self = inodePtrFromThis()](LockedState&& state) {
        self->getOverlayFileAccess(state)->fallocate(*self, offset, length);
        return folly::unit;
      },
      fetchContext);
}
#endif

ImmediateFuture<string> FileInode::readAll(
    const ObjectFetchContextPtr& fetchContext,
    CacheHint cacheHint) {
  // DEPRECATED: use co_readAll directly. Kept only because
  // TreeInode::loadGitIgnoreThenDiff (diff path) still calls it;
  // delete once diff is migrated to coroutines.
  return ImmediateFuture{
      // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
      folly::coro::co_invoke(
          [](auto&& self, auto&&... args) -> folly::coro::Task<std::string> {
            co_return co_await self->co_readAll(
                std::forward<decltype(args)>(args)...);
          },
          inodePtrFromThis(),
          fetchContext.copy(),
          cacheHint)
          .semi()};
}

folly::coro::now_task<std::string> FileInode::co_readAll(
    const ObjectFetchContextPtr& fetchContext,
    CacheHint cacheHint) {
  auto self = inodePtrFromThis();

  BlobCache::Interest interest;
  switch (cacheHint) {
    case CacheHint::NotNeededAgain:
      interest = BlobCache::Interest::UnlikelyNeededAgain;
      break;
    case CacheHint::LikelyNeededAgain:
      // readAll() with LikelyNeededAgain is primarily called for files read
      // by Eden itself, like .gitignore, and for symlinks on kernels that don't
      // cache readlink. At least keep the blob around while the inode is
      // loaded.
      interest = BlobCache::Interest::WantHandle;
      break;
    default:
      EDEN_BUG() << "unexpected CacheHint value: "
                 << static_cast<int>(cacheHint);
  }

  // Acquire lock, check state, load if needed.
  std::shared_ptr<const Blob> blob;
  {
    auto state = LockedState{self};
    switch (state->tag) {
      case State::BLOB_NOT_LOADING:
        blob = state.getCachedBlob(getMount(), interest);
        if (blob) {
          // Blob was in cache, read immediately.
          logAccess(*fetchContext);
          const auto& contentsBuf = blob->getContents();
          folly::io::Cursor cursor(&contentsBuf);
          auto result =
              cursor.readFixedString(contentsBuf.computeChainDataLength());
          updateAtimeLocked(*state);
          co_return result;
        }
        blob = co_await co_startLoadingData(
            std::move(state), interest, fetchContext);
        break;
      case State::BLOB_LOADING: {
        // Already loading, latch on to the in-progress load.
        // Unlock before co_await: completeDataLoad() needs this lock to
        // fulfill the promise.
        auto future = state->blobLoadingPromise->getImmediateFuture();
        state.unlock();
        blob = co_await std::move(future).semi();
        break;
      }
      case State::MATERIALIZED_IN_OVERLAY: {
        // File is materialized, read from overlay.
        logAccess(*fetchContext);
#ifdef _WIN32
        auto result = readFile(self->getMaterializedFilePath()).value();
#else
        auto result = self->getOverlayFileAccess(state)->readAllContents(*self);
#endif
        updateAtimeLocked(*state);
        co_return result;
      }
      default:
        EDEN_BUG() << "unexpected FileInode state in co_readAll(): "
                   << state->tag;
    }
  }

  // Re-acquire lock after loading completes.
  auto state = LockedState{self};
  XDCHECK(
      state->tag == State::BLOB_NOT_LOADING ||
      state->tag == State::MATERIALIZED_IN_OVERLAY)
      << "unexpected FileInode state after loading: " << state->tag;
  logAccess(*fetchContext);

  std::string result;
  switch (state->tag) {
    case State::MATERIALIZED_IN_OVERLAY: {
      // Concurrent materialization during load — read from overlay.
#ifdef _WIN32
      result = readFile(self->getMaterializedFilePath()).value();
#else
      result = self->getOverlayFileAccess(state)->readAllContents(*self);
#endif
      break;
    }
    case State::BLOB_NOT_LOADING: {
      // A BrokenPromise from truncation would have transitioned to
      // MATERIALIZED_IN_OVERLAY. If we have a null blob and are still in
      // BLOB_NOT_LOADING, it means the background task was dropped
      if (!blob) {
        throw InodeError(
            EIO,
            self,
            fmt::format(
                "Blob loading for {} was cancelled",
                state->nonMaterializedState.id));
      }
      const auto& contentsBuf = blob->getContents();
      folly::io::Cursor cursor(&contentsBuf);
      result = cursor.readFixedString(contentsBuf.computeChainDataLength());
      break;
    }
    case State::BLOB_LOADING:
      EDEN_BUG() << "unexpected BLOB_LOADING state after loading completed in "
                    "co_readAll() call";
    default:
      EDEN_BUG() << "unexpected FileInode state after loading in co_readAll(): "
                 << state->tag;
  }
  updateAtimeLocked(*state);
  co_return result;
}
ImmediateFuture<std::tuple<BufVec, bool>> FileInode::read(
    size_t size,
    FileOffset off,
    const ObjectFetchContextPtr& context) {
#ifndef _WIN32
  XDCHECK_GE(off, 0);
  return runWhileDataLoaded(
      LockedState{this},
      BlobCache::Interest::WantHandle,
      // This function is only called by FUSE.
      context,
      nullptr,
      [size, off, self = inodePtrFromThis()](
          LockedState&& state,
          std::shared_ptr<const Blob> blob) -> std::tuple<BufVec, bool> {
        SCOPE_SUCCESS {
          self->updateAtimeLocked(*state);
        };

        // Materialized either before or during blob load.
        if (state->isMaterialized()) {
          // TODO(xavierd): For materialized files, only return EOF when
          // read returned no bytes. This will force some FS Channel
          // (like NFS) to issue at least 2 read calls: one for reading
          // the entire file, and the second one to get the EOF bit.
          auto buf = self->getOverlayFileAccess(state)->read(*self, size, off);
          auto eof = size != 0 && buf->empty();
          return {std::move(buf), eof};
        }

        // runWhileDataLoaded() ensures that the state is either
        // MATERIALIZED_IN_OVERLAY or BLOB_NOT_LOADING
        XDCHECK_EQ(state->tag, State::BLOB_NOT_LOADING);
        XDCHECK(blob) << "blob missing after load completed";

        state->readByteRanges.add(off, off + size);
        if (state->readByteRanges.covers(0, blob->getSize())) {
          XLOGF(
              DBG4,
              "Inode {} dropping interest for blob because it's been fully read.",
              self->getNodeId());
          state->interestHandle.reset();
          state->readByteRanges.clear();
        }

        auto buf = blob->getContents();
        folly::io::Cursor cursor(&buf);

        if (!cursor.canAdvance(off)) {
          // Seek beyond EOF.  Return an empty result.
          return {BufVec{folly::IOBuf::wrapBuffer("", 0)}, true};
        }

        cursor.skip(off);

        std::unique_ptr<folly::IOBuf> result;
        cursor.cloneAtMost(result, size);

        return {BufVec{std::move(result)}, cursor.isAtEnd()};
      });
#else
  (void)size;
  (void)off;
  (void)context;
  // TODO: overlay access not available on Windows.
  return makeImmediateFutureWith(
      []() -> std::tuple<BufVec, bool> { NOT_IMPLEMENTED(); });
#endif
}

ImmediateFuture<size_t> FileInode::write(
    BufVec&& buf,
    FileOffset off,
    const ObjectFetchContextPtr& fetchContext) {
#ifndef _WIN32
  return runWhileMaterialized(
      LockedState{this},
      nullptr,
      [buf = std::move(buf), off, self = inodePtrFromThis()](
          LockedState&& state) {
        auto vec = buf->getIov();
        return self->writeImpl(state, vec.data(), vec.size(), off);
      },
      fetchContext);
#else
  (void)buf;
  (void)off;
  (void)fetchContext;
  // TODO: enable writing on Windows, overlay access is not available.
  return makeImmediateFutureWith([]() -> size_t { NOT_IMPLEMENTED(); });
#endif
}

#ifdef _WIN32
void FileInode::materialize() {
  {
    auto state = LockedState{this};
    state.setMaterialized();
  }

  materializeInParent();
  updateJournal();
}
#else

ImmediateFuture<folly::Unit> FileInode::ensureMaterialized(
    const ObjectFetchContextPtr& fetchContext,
    bool followSymlink) {
  if (dtype_t::Symlink == getType()) {
    if (!followSymlink) {
      return folly::unit;
    }

    return readlink(fetchContext, CacheHint::LikelyNeededAgain)
        .thenValue(
            [this, followSymlink, fetchContext = fetchContext.copy()](
                auto target) -> ImmediateFuture<folly::Unit> {
              auto filePath = getPath();
              if (!filePath) {
                XLOGF(
                    DBG4,
                    "Skip materialization of the symlink {}: file is unlinked",
                    getLogPath());
                return folly::unit;
              }

              // It is possible joinAndNormalize return expected errors, such as
              // symlinking to an absolute or a path out of current mount. Those
              // are fine.
              // joinAndNormalize may throw (i.e. non-UTF-8 path) which would
              // bubble the exception to the caller.
              auto targetPath =
                  joinAndNormalize(filePath.value().dirname(), target);
              if (targetPath.hasError()) {
                XLOGF(
                    DBG4,
                    "Skip materialization of the symlink {}: {}",
                    getLogPath(),
                    targetPath.error());
                return folly::unit;
              }

              XLOGF(
                  DBG4,
                  "Materialize symlink {}, whose target is {}",
                  getLogPath(),
                  targetPath.value());
              return getMount()
                  ->getInodeSlow(targetPath.value(), fetchContext)
                  .thenValue(
                      [followSymlink,
                       fetchContext = fetchContext.copy()](InodePtr inode) {
                        return inode->ensureMaterialized(
                            fetchContext, followSymlink);
                      });
            });
  }

  XLOGF(DBG4, "ensureMaterialize {}", getLogPath());
  return runWhileMaterialized(
      LockedState{this},
      nullptr,
      [](LockedState&&) { return folly::unit; },
      fetchContext);
}

size_t FileInode::writeImpl(
    LockedState& state,
    const struct iovec* iov,
    size_t numIovecs,
    FileOffset off) {
  XDCHECK_EQ(state->tag, State::MATERIALIZED_IN_OVERLAY);

  auto xfer = getOverlayFileAccess(state)->write(*this, iov, numIovecs, off);

  updateMtimeAndCtimeLocked(*state, getNow());

  state.unlock();

  updateJournal();

  return xfer;
}

ImmediateFuture<size_t> FileInode::write(
    folly::StringPiece data,
    FileOffset off,
    const ObjectFetchContextPtr& fetchContext) {
  auto state = LockedState{this};

  // If we are currently materialized we don't need to copy the input data.
  if (state->isMaterialized()) {
    state->materializedState.invalidate();
    struct iovec iov;
    iov.iov_base = const_cast<char*>(data.data());
    iov.iov_len = data.size();
    return writeImpl(state, &iov, 1, off);
  }

  return runWhileMaterialized(
      std::move(state),
      nullptr,
      [data = data.str(), off, self = inodePtrFromThis()](
          LockedState&& stateLock) {
        struct iovec iov;
        iov.iov_base = const_cast<char*>(data.data());
        iov.iov_len = data.size();
        return self->writeImpl(stateLock, &iov, 1, off);
      },
      fetchContext);
}
#endif

/**
 * Simple class to track when the loading future is alive.
 *
 * In the case where the future returned by startLoadingData is dropped, the
 * loading future would not have a chance to be added to an executor, thus
 * leaving the state to BLOB_LOADING. Subsequent loads of this blob would
 * thus never trigger a load and hang.
 *
 * In that case, the state needs to be reset to BLOB_NOT_LOADING to let
 * future loads restart the loading process.
 */
class FileInode::LoadingOngoing {
 public:
  explicit LoadingOngoing(FileInodePtr inode) : inode_{std::move(inode)} {}

  LoadingOngoing(LoadingOngoing&&) = default;
  LoadingOngoing& operator=(LoadingOngoing&&) = default;
  LoadingOngoing(const LoadingOngoing&) = delete;
  LoadingOngoing& operator=(const LoadingOngoing&) = delete;

  ~LoadingOngoing() {
    if (!inode_) {
      // The load finished, nothing to do.
      return;
    }

    inode_->completeDataLoad(
        folly::Try<BlobCache::GetResult>(
            folly::BrokenPromise{folly::tag<BlobCache::GetResult>}));
  }

  /**
   * Get the stored FileInodePtr
   *
   * If this function isn't called, the LoadingOngoing class assumes blob
   * loading didn't complete and will reset the loading state for the stored
   * inode.
   */
  FileInodePtr extractInodePtr() && {
    FileInodePtr ret{std::move(inode_)};
    return ret;
  }

 private:
  FileInodePtr inode_;
};

void FileInode::attachParentDirEntryCounts(
    const ObjectFetchContextPtr& fetchContext) {
  if (!getMount()->getEdenConfig()->walkDirSizeHints.getValue()) {
    return;
  }
  if (auto parent = getParentRacy()) {
    fetchContext->setParentDirEntryCounts(parent->loadTimeEntryCounts());
  }
}

ImmediateFuture<BlobPtr> FileInode::startLoadingData(
    LockedState state,
    BlobCache::Interest interest,
    const ObjectFetchContextPtr& fetchContext) {
  XDCHECK_EQ(state->tag, State::BLOB_NOT_LOADING);

  attachParentDirEntryCounts(fetchContext);

  // Start the blob load first in case this throws an exception.
  // Ideally the state transition is no-except in tandem with the
  // Future's .then call.
  auto getBlobFuture = getMount()->getBlobAccess()->getBlob(
      state->nonMaterializedState.id, fetchContext, interest);
  auto blobLoadingPromise =
      std::make_unique<FileInodeState::BlobLoadingPromise>();

  // Everything from here through blobFuture.then should be noexcept.
  state->blobLoadingPromise = std::move(blobLoadingPromise);
  auto resultFuture = state->blobLoadingPromise->getRawSemiFuture();
  state->tag = State::BLOB_LOADING;

  // Unlock state_ while we wait on the blob data to load
  state.unlock();

  auto loadingFuture =
      std::move(getBlobFuture)
          .thenTry([load = LoadingOngoing{inodePtrFromThis()}](
                       folly::Try<BlobCache::GetResult> tryResult) mutable {
            auto self = std::move(load).extractInodePtr();
            self->completeDataLoad(std::move(tryResult));
          })
          .thenError([](folly::exception_wrapper&&) {
            // We get here if EDEN_BUG() didn't terminate the process, or if we
            // threw in the preceding block.  Both are bad because we won't
            // automatically propagate the exception to resultFuture and we
            // can't trust the state of anything if we get here.  Rather than
            // leaving something hanging, we suicide.  We could probably do a
            // bit better with the error handling here :-/
            //
            // TODO(xavierd): Calling FileInode::completeDataLoad with the
            // exception might be sufficient to propagate the error and reset
            // the loading state.
            XLOG(
                FATAL,
                "Failed to propagate failure in getBlob(), no choice but to die");
          });

  // This is using `collect` instead of `collectAll` to handle the case where
  // the blobLoadingPromise is being cancelled as a result of a truncation. In
  // the case of a truncation, the resultFuture will early return with a
  // `BrokenPromise` error and the loadingFuture will not complete due to
  // collect short-circuiting in that case. However, when loadingFuture
  // completes, it'll set the loadingPromise which will then complete the
  // collect below.
  //
  // The continuations are ImmediateFuture ones rather than deferValue and
  // deferError: when the blob came from a local cache, collect is already
  // complete here, and a deferred callback would hide that behind a future
  // that only completes once an executor drives it, pushing the read reply
  // onto the thread pool.
  return ImmediateFuture<std::tuple<BlobPtr, folly::Unit>>{
      folly::collect(std::move(resultFuture), std::move(loadingFuture).semi())}
      .thenValue([](std::tuple<BlobPtr, folly::Unit>&& res) {
        return std::get<BlobPtr>(std::move(res));
      })
      .thenError([](folly::exception_wrapper&& ew) -> ImmediateFuture<BlobPtr> {
        if (ew.is_compatible_with<folly::BrokenPromise>()) {
          return BlobPtr{nullptr};
        }
        return makeImmediateFuture<BlobPtr>(std::move(ew));
      });
}

folly::coro::now_task<BlobPtr> FileInode::co_startLoadingData(
    LockedState state,
    BlobCache::Interest interest,
    const ObjectFetchContextPtr& fetchContext) {
  XDCHECK_EQ(state->tag, State::BLOB_NOT_LOADING);

  auto blobLoadingPromise =
      std::make_unique<FileInodeState::BlobLoadingPromise>();
  // Everything from here through state.unlock() should be noexcept.
  // Once we transition to BLOB_LOADING, other callers will latch onto
  // blobLoadingPromise, so we must guarantee completeDataLoad is called.
  state->blobLoadingPromise = std::move(blobLoadingPromise);
  auto resultFuture = state->blobLoadingPromise->getRawSemiFuture();
  state->tag = State::BLOB_LOADING;
  auto objectId = state->nonMaterializedState.id;

  // Unlock state_ while we wait on the blob data to load
  state.unlock();

  attachParentDirEntryCounts(fetchContext);

  // Load the blob and fulfill the promise. LoadingOngoing RAII ensures that
  // if the coroutine is cancelled, completeDataLoad(BrokenPromise) is called
  // to reset the inode from BLOB_LOADING back to BLOB_NOT_LOADING, preventing
  // other callers latched onto blobLoadingPromise from hanging.
  {
    LoadingOngoing load{inodePtrFromThis()};
    try {
      auto tryResult = co_await folly::coro::co_awaitTry(
          getMount()->getBlobAccess()->co_getBlob(
              objectId, fetchContext, interest));
      auto self = std::move(load).extractInodePtr();
      self->completeDataLoad(std::move(tryResult));
    } catch (const std::exception&) {
      XLOG(
          FATAL,
          "Failed to propagate failure in getBlob(), no choice but to die");
      throw;
    }
  }

  // resultFuture is now fulfilled by completeDataLoad above.
  // On truncation, the promise is destroyed before completeDataLoad runs,
  // so resultFuture throws BrokenPromise — return nullptr so the caller
  // retries the load.
  try {
    co_return co_await std::move(resultFuture);
  } catch (const folly::BrokenPromise&) {
    co_return nullptr;
  }
}

void FileInode::completeDataLoad(folly::Try<BlobCache::GetResult> tryResult) {
  auto state = LockedState{this};

  switch (state->tag) {
    case State::BLOB_NOT_LOADING:
      EDEN_BUG()
          << "A blob load finished when the inode was in BLOB_NOT_LOADING state";

    // Since the load doesn't hold the state lock for its duration, sanity
    // check that the inode is still in loading state.
    //
    // Note that someone else may have grabbed the lock before us and
    // materialized the FileInode, so we may already be MATERIALIZED_IN_OVERLAY
    // at this point.
    case State::BLOB_LOADING: {
      auto promise = std::move(*state->blobLoadingPromise);
      state->blobLoadingPromise.reset();
      state->tag = State::BLOB_NOT_LOADING;

      // Call the Future's subscribers while the state_ lock is not held. Even
      // if the FileInode has transitioned to a materialized state, any pending
      // loads must be unblocked.
      if (tryResult.hasValue()) {
        auto& result = tryResult.value();
        state->interestHandle = std::move(result.interestHandle);
        state.unlock();
        promise.setValue(std::move(result.object));
      } else {
        state.unlock();
        promise.setException(std::move(tryResult).exception());
      }
      return;
    }

    case State::MATERIALIZED_IN_OVERLAY:
      // The load raced with a someone materializing the file to truncate it.
      // Nothing left to do here. The truncation completed the promise with a
      // null blob.
      XCHECK_EQ(state->blobLoadingPromise.get(), nullptr);
      return;
  }
}

#ifndef _WIN32
void FileInode::materializeNow(
    LockedState& state,
    std::shared_ptr<const Blob> blob,
    const ObjectFetchContextPtr& fetchContext) {
  // This function should only be called from the BLOB_NOT_LOADING state
  XDCHECK_EQ(state->tag, State::BLOB_NOT_LOADING);

  // If the blob aux data is immediately available, use it to populate the
  // cached hashes in the overlay for this file.
  // Since this uses state->nonMaterializedState.id we perform this before
  // calling state.setMaterialized().
  std::optional<Hash20> blobSha1;
  std::optional<Hash32> blobBlake3;
  // Preserve a cached SHA-1 even when the entry does not include BLAKE3.
  if (auto blobAuxData = getObjectStore().getBlobAuxDataFromInMemoryCache(
          state->nonMaterializedState.id,
          fetchContext,
          false /* blake3Required */)) {
    blobSha1 = blobAuxData->sha1;
    blobBlake3 = blobAuxData->blake3;
  }

  getOverlayFileAccess(state)->createFile(
      getNodeId(), *blob, blobSha1, blobBlake3);

  state.setMaterialized();
}

void FileInode::materializeAndTruncate(LockedState& state) {
  XCHECK_NE(state->tag, State::MATERIALIZED_IN_OVERLAY);
  getOverlayFileAccess(state)->createEmptyFile(
      getNodeId(), getMount()->getEdenConfig()->blake3Key.getValue());
  state.setMaterialized();
}

void FileInode::truncateInOverlay(LockedState& state) {
  XCHECK_EQ(state->tag, State::MATERIALIZED_IN_OVERLAY);

  getOverlayFileAccess(state)->truncate(*this);
  state->materializedState.invalidate();
}

OverlayFileAccess* FileInode::getOverlayFileAccess(LockedState&) const {
  return getMount()->getOverlayFileAccess();
}
#endif // !_WIN32

} // namespace facebook::eden
