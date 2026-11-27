/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/inodes/InodeBase.h"

#include <folly/Likely.h>
#include <folly/logging/xlog.h>
#include <folly/small_vector.h>

#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/InodeAccessLogger.h"
#include "eden/fs/inodes/InodeMap.h"
#include "eden/fs/inodes/InodeTable.h"
#include "eden/fs/inodes/Overlay.h"
#include "eden/fs/inodes/OverlayFileAccess.h"
#include "eden/fs/inodes/ParentInodeInfo.h"
#include "eden/fs/inodes/ServerState.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/journal/Journal.h"
#include "eden/fs/telemetry/EdenErrorInfoBuilder.h"
#include "eden/fs/telemetry/ErrorLogger.h"
#include "eden/fs/utils/Clock.h"
#include "eden/fs/utils/NotImplemented.h"

namespace facebook::eden {

namespace {

/**
 * Brackets a directory's location change for the path cache. Files are not
 * cached, so their location changes need no bracket.
 */
class DirectoryLocationChange {
 public:
  DirectoryLocationChange(InodePathCache& cache, bool isDirectory)
      : cache_{isDirectory ? &cache : nullptr} {
    if (cache_) {
      cache_->beginChange();
    }
  }
  ~DirectoryLocationChange() {
    if (cache_) {
      cache_->endChange();
    }
  }
  DirectoryLocationChange(const DirectoryLocationChange&) = delete;
  DirectoryLocationChange(DirectoryLocationChange&&) = delete;
  DirectoryLocationChange& operator=(const DirectoryLocationChange&) = delete;
  DirectoryLocationChange& operator=(DirectoryLocationChange&&) = delete;

 private:
  InodePathCache* cache_;
};

} // namespace

InodeBase::InodeBase(EdenMount* mount)
    : ino_{kRootNodeId},
      mount_{mount},
      initialMode_{S_IFDIR | 0755},
      // The root inode always starts with an implicit reference from FUSE.
      numFsReferences_{1},
      location_{LocationInfo{
          nullptr,
          PathComponentPiece{"", detail::SkipPathSanityCheck()}}} {
  XLOGF(
      DBG5,
      "root inode {} ({}) created for mount {}",
      fmt::ptr(this),
      ino_,
      mount_->getPath());

#ifndef _WIN32
  mount->getInodeMetadataTable()->populateIfNotSet(
      ino_, [&] { return mount->getInitialInodeMetadata(S_IFDIR | 0755); });
#endif
}

InodeBase::InodeBase(
    InodeNumber ino,
    mode_t initialMode,
    [[maybe_unused]] const std::optional<InodeTimestamps>& initialTimestamps,
    TreeInodePtr parent,
    PathComponentPiece name)
    : ino_{ino},
      mount_{parent->mount_},
      initialMode_{initialMode},
      location_{LocationInfo{std::move(parent), name}} {
  // Inode numbers generally shouldn't be 0.
  // Older versions of glibc have bugs handling files with an inode number of 0
  XDCHECK(ino_.hasValue());
  XLOGF(DBG5, "inode {} ({}) created: {}", fmt::ptr(this), ino_, getLogPath());

#ifndef _WIN32
  mount_->getInodeMetadataTable()->populateIfNotSet(ino_, [&] {
    auto metadata = mount_->getInitialInodeMetadata(initialMode);
    auto now = getNow();
    if (initialTimestamps) {
      metadata.timestamps = *initialTimestamps;
      XLOGF(
          DBG9,
          "Setting initial timestamps for inode {} atime: {} while now time: {}",
          ino_,
          metadata.timestamps.atime.toTimespec().tv_sec,
          now.toTimespec().tv_sec);
    } else {
      XLOGF(
          DBG9,
          "Initial timestamps was NULL for inode {} then the atime remains unchanged. atime: {} while now time: {}",
          ino_,
          metadata.timestamps.atime.toTimespec().tv_sec,
          now.toTimespec().tv_sec);
    }
    return metadata;
  });
#endif

  auto p = getParentRacy();
  while (p) {
    p->increaseInMemoryDescendants(1);
    p = p->getParentRacy();
  }
}

InodeBase::~InodeBase() {
  XLOGF(
      DBG5, "inode {} ({}) destroyed: {}", fmt::ptr(this), ino_, getLogPath());
  if (removeOverlayDataOnDestruction_) {
    removeOverlayData();
  }
  auto p = getParentRacy();
  while (p) {
    p->increaseInMemoryDescendants(-1);
    p = p->getParentRacy();
  }
}

bool InodeBase::isOnFSKitMount() const {
  return mount_->shouldBeOrIsFSKitChannel();
}

#ifndef _WIN32
struct stat InodeBase::getStatForCorruptOverlay() const noexcept {
  struct stat st = {};
  st.st_ino = getNodeId().get();
  st.st_mode = getInitialMode() & S_IFMT;
  st.st_nlink = 1;
  return st;
}
#endif

void InodeBase::removeOverlayData() noexcept {
  try {
    auto* overlay = mount_->getOverlay();
    const bool isDir = getType() == dtype_t::Dir;
#ifndef _WIN32
    // Drop the cached descriptor of a deleted file now, so its space is freed
    // when this scope or the GC thread closes it rather than whenever the
    // cache happens to evict it.
    std::shared_ptr<void> openFile;
    if (!isDir) {
      openFile = mount_->getOverlayFileAccess()->releaseEntry(ino_);
    }
    // Removing overlay data is a btrfs unlink and inode evict, several
    // metadata tree walks that serialize on the subvolume's root lock. Doing
    // them from many FsChannel threads at once contends with each other and
    // with the writes being served; one GC thread doing them in sequence
    // does not.
    if (overlay->removeOverlayDataInBackground(
            ino_, isDir, std::move(openFile))) {
      return;
    }
#endif
    if (isDir) {
      overlay->removeOverlayDir(ino_);
    } else {
      overlay->removeOverlayFile(ino_);
    }
  } catch (const std::exception& ex) {
    // Nothing more can be done: the inode is gone and fsck reclaims orphaned
    // overlay data. The usual cause is overlay data left corrupt by a crash
    // that did not sync the filesystem.
    auto outcome = mount_->getServerState()->getErrorLogger().log(
        EdenErrorInfo::overlay(ex, ino_.getRawValue())
            .withMountPoint(mount_->getPath().asString())
            .withErrorType("overlay_unload_failed"));
    if (outcome != ErrorLogOutcome::RateLimited) {
      XLOGF(
          ERR,
          "error removing overlay data of unlinked inode {} ({}): {}",
          ino_,
          getLogPath(),
          folly::exceptionStr(ex));
    }
  }
}

#ifndef _WIN32
folly::Future<folly::Unit> InodeBase::setxattr(
    folly::StringPiece /*name*/,
    folly::StringPiece /*value*/,
    int /*flags*/) {
  // setxattr is not supported for any type of inode. This instructs the kernel
  // to automatically fail all future setxattr() syscalls with EOPNOTSUPP.
  FUSELL_NOT_IMPL();
}

folly::Future<folly::Unit> InodeBase::removexattr(folly::StringPiece /*name*/) {
  // removexattr is not supported for any type of inode. This instructs the
  // kernel to automatically fail all future removexattr() syscalls with
  // EOPNOTSUPP.
  FUSELL_NOT_IMPL();
}

folly::Future<folly::Unit> InodeBase::access(int /*mask*/) {
  // Returning ENOSYS instructs FUSE that access() will always succeed, so does
  // not need to call back into the FUSE daemon.
  FUSELL_NOT_IMPL();
}
#endif

void InodeBase::markGcRef() {
  auto now = getNow();
  lastGcRefTime_.exchange(now, std::memory_order_relaxed);
}

bool InodeBase::isUnlinked() const {
  auto loc = location_.rlock();
  return loc->unlinked;
}

bool InodeBase::getPathHelper(
    std::vector<PathComponent>& names,
    bool stopOnUnlinked) const {
  TreeInodePtr parent;
  bool unlinked = false;
  {
    auto loc = location_.rlock();
    if (loc->unlinked) {
      if (stopOnUnlinked) {
        return false;
      }
      unlinked = true;
    }
    parent = loc->parent;
    // Our caller should ensure that we are not the root
    XDCHECK(parent);
    names.push_back(loc->name);
  }

  while (true) {
    // Stop at the root inode.
    // We check for this based on inode number so we can stop without having to
    // acquire the root inode's location lock.  (Otherwise all path lookups
    // would have to acquire the root's lock, making it more likely to be
    // contended.)
    if (parent->ino_ == kRootNodeId) {
      // Reverse the names vector, since we built it from bottom to top.
      std::reverse(names.begin(), names.end());
      return !unlinked;
    }

    auto loc = parent->location_.rlock();
    // In general our parent should not be unlinked if we are not unlinked,
    // which we checked above.  However, we have since released our location
    // lock, so it's possible (but unlikely) that someone unlinked us and our
    // parent directories since we checked above.
    if (UNLIKELY(loc->unlinked)) {
      if (stopOnUnlinked) {
        return false;
      }
      unlinked = true;
    }
    names.push_back(loc->name);
    parent = loc->parent;
    XDCHECK(parent);
  }
}

std::optional<RelativePath> InodeBase::getPath() const {
  if (ino_ == kRootNodeId) {
    return RelativePath();
  }

  auto& cache = mount_->getInodePathCache();
  // While a directory is moving, read the locations directly: see
  // InodePathCache::changing().
  if (!cache.enabled() || cache.changing()) {
    std::vector<PathComponent> names;
    if (!getPathHelper(names, true)) {
      return std::nullopt;
    }
    return RelativePath(names);
  }

  const auto generation = cache.generation();
  const bool isDirectory = getType() == dtype_t::Dir;
  if (isDirectory) {
    // A cached entry for this directory is current as long as no directory
    // has moved or been removed since it was recorded, which is exactly what
    // an unchanged generation says, so no lock is needed.
    if (auto path = cache.find(ino_, generation)) {
      if (cache.generation() == generation) {
        return path;
      }
    }
  }
  // Names from this inode up to, but not including, the nearest directory
  // whose path is cached, innermost first.
  folly::small_vector<PathComponent, 2> names;
  TreeInodePtr parent;
  {
    auto loc = location_.rlock();
    if (loc->unlinked) {
      return std::nullopt;
    }
    parent = loc->parent;
    names.push_back(loc->name);
  }
  if (!parent) {
    return std::nullopt;
  }
  const auto parentNumber = parent->ino_;

  std::optional<RelativePath> path;
  while (true) {
    if (parent->ino_ == kRootNodeId) {
      path = RelativePath();
      break;
    }
    path = cache.find(parent->ino_, generation);
    if (path) {
      break;
    }
    auto loc = parent->location_.rlock();
    if (loc->unlinked) {
      return std::nullopt;
    }
    names.push_back(loc->name);
    parent = loc->parent;
    if (!parent) {
      return std::nullopt;
    }
  }
  for (auto iter = names.rbegin(); iter != names.rend(); ++iter) {
    path = *path + *iter;
  }

  if (cache.generation() != generation) {
    // A directory moved or was removed while this path was being built, so
    // the cached prefix may not match the names read after it. Build the
    // path from the locations alone.
    std::vector<PathComponent> all;
    if (!getPathHelper(all, true)) {
      return std::nullopt;
    }
    return RelativePath(all);
  }
  if (names.size() > 1) {
    cache.insert(parentNumber, path->dirname(), generation);
  }
  if (isDirectory) {
    cache.insert(ino_, *path, generation);
  }
  return path;
}

RelativePath InodeBase::getUnsafePath() const {
  if (ino_ == kRootNodeId) {
    return RelativePath();
  }

  std::vector<PathComponent> names;
  getPathHelper(names, false);
  return RelativePath{names};
}

std::string InodeBase::getLogPath() const {
  if (ino_ == kRootNodeId) {
    // We use "<root>" here instead of the empty string to make log messages
    // more understandable.  The empty string would likely be confusing, as it
    // would appear if the file name were missing.
    return "<root>";
  }

  std::vector<PathComponent> names;
  bool unlinked = !getPathHelper(names, false);
  auto path = RelativePath(names);
  if (unlinked) {
    return fmt::format("<deleted:{}>", path);
  }
  return std::move(path).value();
}

void InodeBase::markUnlinkedAfterLoad() {
  DirectoryLocationChange change{
      mount_->getInodePathCache(), getType() == dtype_t::Dir};
  auto loc = location_.wlock();
  XDCHECK(!loc->unlinked);
  loc->unlinked = true;
}

std::unique_ptr<InodeBase> InodeBase::markUnlinked(
    TreeInode* parent,
    PathComponentPiece name,
    const RenameLock& renameLock) {
  XLOGF(DBG5, "inode {} unlinked: {}", fmt::ptr(this), getLogPath());
  XDCHECK(renameLock.isHeld(mount_));

  {
    DirectoryLocationChange change{
        mount_->getInodePathCache(), getType() == dtype_t::Dir};
    auto loc = location_.wlock();
    XDCHECK(!loc->unlinked);
    XDCHECK_EQ(loc->parent.get(), parent);
    loc->unlinked = true;
  }

  // Grab the inode map lock, and check if we should unload
  // ourself immediately.
  auto* inodeMap = getMount()->getInodeMap();
  auto inodeMapLock = inodeMap->lockForUnload();
  if (isPtrAcquireCountZero() && getFsRefcount() == 0) {
    inodeMap->unloadInode(this, parent, name, true, inodeMapLock);
    // We have to delete ourself now.
    // Do this by returning a unique_ptr to ourself, so that our caller will
    // destroy us.  This ensures we get destroyed after releasing the InodeMap
    // lock.  Our calling TreeInode should wait to destroy us until they
    // release their contents lock as well.
    //
    // (Technically it should probably be fine even if the caller deletes us
    // before releasing their contents lock, it just seems safer to wait.
    // The main area of concern is that deleting us will drop a reference count
    // on our parent, which could require the code to acquire locks to destroy
    // our parent.  However, we are only ever invoked from unlink(), rmdir(),
    // or rename() operations which must already be holding a reference on our
    // parent.  Therefore our parent should never be destroyed when our
    // destructor gets invoked here, so we won't need to acquire our parent's
    // contents lock in our destructor.)
    return std::unique_ptr<InodeBase>(this);
  }
  // We don't need our caller to delete us, so return null.
  return nullptr;
}

void InodeBase::updateLocation(
    TreeInodePtr newParent,
    PathComponentPiece newName,
    const RenameLock& renameLock) {
  XLOGF(
      DBG5,
      "inode {:p} renamed: {} --> {} / \"{}\"",
      fmt::ptr(this),
      getLogPath(),
      newParent->getLogPath(),
      newName);
  XDCHECK(renameLock.isHeld(mount_));
  XDCHECK_EQ(mount_, newParent->mount_);

  DirectoryLocationChange change{
      mount_->getInodePathCache(), getType() == dtype_t::Dir};
  auto loc = location_.wlock();
  XDCHECK(!loc->unlinked);
  loc->parent = newParent;
  loc->name = newName.copy();
}

void InodeBase::onPtrRefZero() const {
  // onPtrRefZero() is const since we treat incrementing and decrementing the
  // pointer refcount as a non-modifying operation.  (The refcount is updated
  // atomically so the operation is thread-safe.)
  //
  // However when the last reference goes to zero we destroy the inode object,
  // which is a modifying operation.  Cast ourself back to non-const in this
  // case.  We are guaranteed that no-one else has a reference to us anymore so
  // this is safe.
  //
  // We could perhaps just make incrementPtrRef() and decrementPtrRef()
  // non-const instead.  InodePtr objects always point to non-const InodeBase
  // objects; we do not currently ever use pointer-to-const InodePtrs.
  auto* inodeMap = getMount()->getInodeMap();
  auto* inode = const_cast<InodeBase*>(this);
  if (inodeMap->onLinkedInodeUnreferenced(inode)) {
    return;
  }
  inodeMap->onInodeUnreferenced(inode, getParentInfo());
}

ParentInodeInfo InodeBase::getParentInfo() const {
  using ParentContentsPtr = folly::Synchronized<TreeInodeState>::LockedPtr;

  // Grab our parent's contents_ lock.
  //
  // We need a retry loop here in case we get renamed or unlinked while trying
  // to acquire our parent's lock.
  //
  // (We could acquire the mount point rename lock first, to ensure that we
  // can't be renamed during this process.  However it seems unlikely that we
  // would get renamed or unlinked, so retrying seems probably better than
  // holding a mountpoint-wide lock.)
  size_t numTries = {0};
  while (true) {
    ++numTries;
    TreeInodePtr parent;
    // Get our current parent.
    {
      auto loc = location_.rlock();
      parent = loc->parent;
      if (loc->unlinked) {
        XLOGF(
            DBG6,
            "getParentInfo(): unlinked inode detected after {} tries",
            numTries);
        return ParentInodeInfo{
            loc->name, loc->parent, loc->unlinked, std::nullopt};
      }
    }

    if (!parent) {
      // We are the root inode.
      XDCHECK_EQ(numTries, 1u);
      return ParentInodeInfo{
          PathComponentPiece{"", detail::SkipPathSanityCheck()},
          nullptr,
          false,
          std::nullopt};
    }

    auto parentContents = parent->getContentsUnchecked().wlock();

    // After acquiring our parent's contents lock we have to make sure it is
    // actually still our parent.  If it is we are done and can break out of
    // this loop.
    {
      auto loc = location_.rlock();
      if (loc->unlinked) {
        // This file was unlinked since we checked earlier
        XLOGF(
            DBG6,
            "getParentInfo(): file is newly unlinked on try {}",
            numTries);
        return ParentInodeInfo{
            loc->name, loc->parent, loc->unlinked, std::nullopt};
      }
      if (loc->parent == parent) {
        if (parent->isRestricted()) {
          // Restricted parents cannot expose contents to callers, but this is
          // internal inode cleanup. If the existing in-memory contents still
          // contain a loaded entry for this inode, keep the lock so the caller
          // can clear that raw pointer before freeing the inode. If checkout
          // already removed the entry, treat it like an unavailable parent
          // entry.
          auto entry = parentContents->entries.find(loc->name);
          if (entry == parentContents->entries.end() ||
              entry->second.getInode() != this) {
            XLOGF(
                DBG9,
                "getParentInfo() found restricted parent with no loaded child entry after {} tries",
                numTries);
            parentContents.unlock();
            return ParentInodeInfo{
                loc->name, loc->parent, loc->unlinked, std::nullopt};
          }
        }

        // Our parent is still the same.  We're done.
        XLOGF(
            DBG9,
            "getParentInfo() acquired parent lock after {} tries",
            numTries);
        return ParentInodeInfo{
            loc->name,
            loc->parent,
            loc->unlinked,
            std::optional<ParentContentsPtr>{std::move(parentContents)}};
      }
    }
    // Otherwise our parent changed, and we have to retry.
    parent.reset();
    parentContents.unlock();
  }
}

#ifndef _WIN32
InodeMetadata InodeBase::getMetadataLocked() const {
  return getMount()->getInodeMetadataTable()->getOrThrow(getNodeId());
}
#endif

void InodeBase::updateAtime() {
#ifndef _WIN32
  // TODO: Is it worth implementing relatime-like logic?
  auto now = getNow();
  getMount()->getInodeMetadataTable()->modifyOrThrow(
      getNodeId(), [&](auto& metadata) { metadata.timestamps.atime = now; });
  XLOGF(
      DBG9,
      "Updated atime for inode {} to now: {}",
      ino_,
      now.toTimespec().tv_sec);
#endif
}

void InodeBase::updateMtimeAndCtime([[maybe_unused]] EdenTimestamp now) {
#ifndef _WIN32
  XLOGF(DBG9, "Updating timestamps for: {}", ino_);
  getMount()->getInodeMetadataTable()->modifyOrThrow(
      getNodeId(), [&](auto& record) {
        record.timestamps.ctime = now;
        record.timestamps.mtime = now;
      });
#endif
}

EdenTimestamp InodeBase::getNow() const {
  return EdenTimestamp{getClock().getRealtime()};
}

const Clock& InodeBase::getClock() const {
  return getMount()->getClock();
}

ObjectStore& InodeBase::getObjectStore() const {
  return *getMount()->getObjectStore();
}

void InodeBase::updateJournal() {
  auto path = getPath();
  if (path.has_value()) {
    getMount()->getJournal().recordChanged(std::move(path.value()), getType());
  }
}

void InodeBase::logAccess(const ObjectFetchContext& fetchContext) {
  auto ino = getNodeId();

  // Don't log root inode access
  if (ino == kRootNodeId) {
    return;
  }

  // Don't log file accesses that originate from a prefetch. In practice, this
  // case should not be hit, but since we have the data for free here let's
  // safeguard against it anyway
  if (fetchContext.getCause() == ObjectFetchContext::Cause::Prefetch) {
    return;
  }

  std::optional<std::string> fetchDetail;

  const auto detail = fetchContext.getCauseDetail();
  if (detail.has_value()) {
    fetchDetail.emplace(std::string{detail.value()});
  }

  getMount()->getServerState()->getInodeAccessLogger()->logInodeAccess(
      InodeAccess{
          ino,
          getType(),
          fetchContext.getCause(),
          std::move(fetchDetail),
          getMount()->getWeakMount()});
}

} // namespace facebook::eden
