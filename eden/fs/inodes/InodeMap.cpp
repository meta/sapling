/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/inodes/InodeMap.h"

#include <boost/polymorphic_cast.hpp>

#include <folly/Exception.h>
#include <folly/Likely.h>
#include <folly/chrono/Conv.h>
#include <folly/logging/xlog.h>

#include "eden/common/utils/Bug.h"
#include "eden/common/utils/FaultInjector.h"
#include "eden/common/utils/SystemError.h"
#include "eden/common/utils/TimeUtil.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/config/ReloadableConfig.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/FileInode.h"
#include "eden/fs/inodes/InodeError.h"
#include "eden/fs/inodes/Overlay.h"
#include "eden/fs/inodes/ParentInodeInfo.h"
#include "eden/fs/inodes/ServerState.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/telemetry/EdenErrorInfo.h"
#include "eden/fs/telemetry/EdenErrorInfoBuilder.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/telemetry/ErrorLogger.h"
#include "eden/fs/telemetry/LogEvent.h"
#include "eden/fs/utils/Clock.h"
#include "eden/fs/utils/NotImplemented.h"

using folly::Future;
using folly::Promise;
using folly::throwSystemErrorExplicit;
using folly::Unit;
using std::optional;

namespace facebook::eden {

InodeMap::UnloadedInode::UnloadedInode(
    InodeNumber parentNum,
    PathComponentPiece entryName,
    mode_t mode,
    EdenTimestamp lastGcRefTime)
    : parent(parentNum),
      name(entryName),
      mode{mode},
      lastGcRefTime{lastGcRefTime} {}

InodeMap::UnloadedInode::UnloadedInode(
    InodeNumber parentNum,
    PathComponentPiece entryName,
    bool isUnlinked,
    mode_t mode,
    std::optional<ObjectId> id,
    uint32_t fsRefcount,
    EdenTimestamp lastGcRefTime)
    : parent(parentNum),
      name(entryName),
      isUnlinked{isUnlinked},
      mode{mode},
      id{std::move(id)},
      lastGcRefTime{lastGcRefTime},
      numFsReferences{fsRefcount} {
  if (folly::kIsWindows) {
    XDCHECK_LE(numFsReferences, 1u);
  }
}

InodeMap::UnloadedInode::UnloadedInode(
    FileInode* inode,
    TreeInode* parent,
    PathComponentPiece entryName,
    bool isUnlinked,
    uint32_t fsRefcount)
    : parent{parent->getNodeId()},
      name{entryName},
      isUnlinked{isUnlinked},
      mode{inode->getMode()},
      id{inode->getObjectId()},
      lastGcRefTime{inode->getLastGcRefTime()},
      numFsReferences{fsRefcount} {
  if (folly::kIsWindows) {
    XDCHECK_LE(numFsReferences, 1u);
  }
}

InodeType InodeMap::UnloadedInode::getInodeType() const {
  return S_ISDIR(mode) ? InodeType::TREE : InodeType::FILE;
}

bool InodeMap::UnloadedInode::matchesLinkedChild(
    InodeNumber parentNumber,
    PathComponentPiece entryName,
    mode_t initialMode) const {
  return parent == parentNumber && name == entryName && !isUnlinked &&
      (mode & S_IFMT) == (initialMode & S_IFMT);
}

InodeMap::InodeMap(
    EdenMount* mount,
    std::shared_ptr<ReloadableConfig> config,
    EdenStatsPtr stats,
    std::shared_ptr<EdenFsEventsLogger> logger)
    : mount_{mount},
      config_{std::move(config)},
      stats_{std::move(stats)},
      edenFsEventsLogger_{std::move(logger)} {}

InodeMap::~InodeMap() {
  // TODO: We need to clean up the EdenMount / InodeMap destruction process a
  // bit.
  //
  // When an EdenMount is unmounted it should signal us that we are about to be
  // destroyed.  At that point we should:
  // - set a flag to immediately fail all future lookupInode() calls
  // - fail all pending lookup promises
  // - set a flag (maybe the same one as above) that causes us to immediately
  //   destroy inodes when their reference count drops to 0
  // - immediately destroy all loaded inodes whose reference count is already 0
  // - decrement our reference count on the root inode
  //
  // Only when the root inode count drops to 0 is it then safe to actually
  // destroy the EdenMount.
}

inline void InodeMap::insertLoadedInode(
    const folly::Synchronized<Members>::LockedPtr& data,
    InodeBase* inode) {
  auto ret = data->loadedInodes_.emplace(inode->getNodeId(), inode);
  XCHECK(ret.second) << fmt::format(
      "inode {} is already loaded as {} while inserting {}",
      inode->getNodeId(),
      ret.first->second->getLogPath(),
      inode->getLogPath());
  if (inode->getType() == dtype_t::Dir) {
    ++data->numTreeInodes_;
  } else {
    ++data->numFileInodes_;
  }
  totalInodeCount_.fetch_add(1, std::memory_order_relaxed);
}

inline void InodeMap::eraseLoadedInode(
    const folly::Synchronized<Members>::LockedPtr& data,
    InodeBase* inode) {
  auto numErased = data->loadedInodes_.erase(inode->getNodeId());
  XCHECK_EQ(numErased, 1u) << "inconsistent loaded inodes data: "
                           << inode->getLogPath();
  if (inode->getType() == dtype_t::Dir) {
    --data->numTreeInodes_;
  } else {
    --data->numFileInodes_;
  }
  totalInodeCount_.fetch_sub(1, std::memory_order_relaxed);
}

inline InodeMap::UnloadedInode& InodeMap::insertUnloadedInode(
    const folly::Synchronized<Members>::LockedPtr& data,
    InodeNumber ino,
    UnloadedInode&& unloadedInode) {
  auto ret = data->unloadedInodes_.emplace(ino, std::move(unloadedInode));
  XCHECK(ret.second) << fmt::format(
      "failed to emplace inode number {}; is it already present in the InodeMap?",
      ino);
  totalInodeCount_.fetch_add(1, std::memory_order_relaxed);
  return ret.first->second;
}

inline void InodeMap::eraseUnloadedInode(
    const folly::Synchronized<Members>::LockedPtr& data,
    std::unordered_map<InodeNumber, UnloadedInode>::iterator iter) {
  data->unloadedInodes_.erase(iter);
  totalInodeCount_.fetch_sub(1, std::memory_order_relaxed);
}

void InodeMap::initializeRoot(
    const folly::Synchronized<Members>::LockedPtr& data,
    TreeInodePtr root) {
  XCHECK_EQ(data->loadedInodes_.size(), 0ul)
      << "cannot load InodeMap data over a populated instance";
  XCHECK_EQ(data->unloadedInodes_.size(), 0ul)
      << "cannot load InodeMap data over a populated instance";

  XCHECK(!root_);
  root_ = std::move(root);
  insertLoadedInode(data, root_.get());
  XDCHECK_EQ(1ul, data->numTreeInodes_);
  XDCHECK_EQ(0ul, data->numFileInodes_);
}

void InodeMap::initialize(TreeInodePtr root) {
  initializeRoot(data_.wlock(), std::move(root));
}

template <class... Args>
void InodeMap::initializeUnloadedInode(
    const folly::Synchronized<Members>::LockedPtr& data,
    InodeNumber parentIno,
    InodeNumber ino,
    Args&&... args) {
  insertUnloadedInode(
      data, ino, UnloadedInode(parentIno, std::forward<Args>(args)...));
}

void InodeMap::initializeFromTakeover(
    TreeInodePtr root,
    const SerializedInodeMap& takeover) {
  auto data = data_.wlock();
  initializeRoot(data, std::move(root));

  for (const auto& entry : *takeover.unloadedInodes()) {
    if (*entry.numFsReferences() < 0) {
      auto message = fmt::format(
          "inode number {} has a negative numFsReferences number",
          *entry.inodeNumber());
      XLOG(ERR, message);
      mount_->getServerState()->getErrorLogger().log(
          EdenErrorInfo::takeover(message)
              .withInode(*entry.inodeNumber())
              .withMountPoint(mount_->getPath().asString())
              .withErrorType("takeover_negative_refcount"));
      throw std::runtime_error(message);
    }

    std::optional<ObjectId> id;
    if (entry.hash().has_value()) {
      const std::string& value = entry.hash().value();
      if (value.empty()) {
        // LEGACY: Old versions of EdenFS sent the empty string to mean
        // materialized. When a BackingStore wants to support the empty ObjectId
        // as a valid identifier, remove this code path.
        id = std::nullopt;
      } else {
        id = ObjectId{value};
      }
    }
    initializeUnloadedInode(
        data,
        InodeNumber::fromThrift(*entry.parentInode()),
        InodeNumber::fromThrift(*entry.inodeNumber()),
        PathComponentPiece{*entry.name()},
        *entry.isUnlinked(),
        *entry.mode(),
        std::move(id),
        folly::to<uint32_t>(*entry.numFsReferences()),
        entry.lastFsRequestTime().has_value()
            ? EdenTimestamp::fromSerializedValue(*entry.lastFsRequestTime())
            : EdenTimestamp{mount_->getClock().getRealtime()});
  }

  XLOGF(
      DBG2,
      "InodeMap initialized mount {} from takeover, {} inodes registered",
      mount_->getPath(),
      data->unloadedInodes_.size());
}

void InodeMap::initializeFromOverlay(TreeInodePtr root, Overlay& overlay) {
  XCHECK(mount_->isWorkingCopyPersistent());

  XLOGF(DBG2, "Initializing InodeMap for {}", mount_->getPath());

  auto data = data_.wlock();
  initializeRoot(data, std::move(root));

  std::vector<std::tuple<AbsolutePath, InodeNumber>> pending;
  pending.emplace_back(mount_->getPath(), root_->getNodeId());

  while (!pending.empty()) {
    auto [path, dirInode] = std::move(pending.back());
    pending.pop_back();

    auto dirEntries = overlay.loadOverlayDir(dirInode);
    for (const auto& [name, dirent] : dirEntries) {
      auto entryPath = path + name;
      auto ino = dirent.getInodeNumber();

      if (dirent.isDirectory()) {
        // Do a quick check in the overlay to check if EdenFS has an overlay
        // entry for this Inode. The assumption is that files on disk must
        // have a corresponding Overlay entry. Note that since Overlay
        // entries are also created for files that aren't on disk, this will
        // load inodes with no corresponding on-disk files.
        if (!overlay.hasOverlayDir(ino)) {
          continue;
        }
        pending.emplace_back(std::move(entryPath), ino);
      }

      initializeUnloadedInode(
          data,
          dirInode,
          ino,
          name,
          false,
          dirent.getInitialMode(),
          dirent.getOptionalObjectId(),
          1,
          EdenTimestamp{mount_->getClock().getRealtime()});
    }
  }

  XLOGF(
      DBG2,
      "InodeMap initialized mount {} from overlay, {} inodes registered",
      mount_->getPath(),
      data->unloadedInodes_.size());
}

ImmediateFuture<InodePtr> InodeMap::lookupInode(
    InodeNumber number,
    const ObjectFetchContextPtr& context) {
  // Nearly every lookup is for an already loaded inode and mutates nothing, so
  // it must not serialize behind the write lock that starting a load needs.
  if (auto inode = lookupLoadedInode(number)) {
    return inode;
  }

  // Lock the data.
  // We hold it while doing most of our work below, but explicitly unlock it
  // before triggering inode loading or before fulfilling any Promises.
  auto data = data_.wlock();
  std::vector<InodeTraceEvent> startLoadEvents;

  // The inode may have finished loading between the two locks.
  auto loadedIter = data->loadedInodes_.find(number);
  if (loadedIter != data->loadedInodes_.end()) {
    auto inode = loadedIter->second.getPtr();
    stats_->increment(
        inode->isDir() ? &InodeMapStats::lookupTreeInodeHit
                       : &InodeMapStats::lookupBlobInodeHit);
    return inode;
  }

  // Look up the data in the unloadedInodes_ map.
  auto unloadedIter = data->unloadedInodes_.find(number);
  if (UNLIKELY(unloadedIter == data->unloadedInodes_.end())) {
    stats_->increment(&InodeMapStats::lookupInodeError);
    if (mount_->throwEstaleIfInodeIsMissing()) {
      XLOGF(DBG3, "NFS inode {} stale", number);
      // windows does not have ESTALE. We need some other error to turn into the
      // nfs stale error. For now let's just let it throw.
#ifndef _WIN32
      edenFsEventsLogger_->logEvent(NFSStaleError{});
      return ImmediateFuture<InodePtr>{folly::Try<InodePtr>{
          std::system_error{std::error_code{ESTALE, std::system_category()}}}};
#endif
    }
    // This generally shouldn't happen.  If a InodeNumber has been allocated we
    // should always know about it.  It's a bug if our caller calls us with an
    // invalid InodeNumber number.
    return EDEN_BUG_FUTURE(InodePtr)
        << fmt::format("InodeMap called with unknown inode number {}", number);
  }

  // Check to see if anyone else has already started loading this inode.
  auto* unloadedData = &unloadedIter->second;
  bool alreadyLoading = !unloadedData->promises.empty();

  if (!alreadyLoading) {
    startLoadEvents.push_back(
        createInodeLoadStartEvent(number, *unloadedData, data));
  }

  // Add a new entry to the promises list.
  unloadedData->promises.emplace_back();
  auto result = unloadedData->promises.back().getSemiFuture();

  // If someone else has already started loading this inode we are done.
  // The current loading attempt will signal our promise when it completes.
  if (alreadyLoading) {
    return result;
  }

  // Walk up through the parents until we find a loaded TreeInode.
  // Once we find one, we break out, release the lock, and then call
  // loadChildInode() on it.  When the loadChildInode() future finishes
  // we have it signal all pending promises for that inode.
  //
  // For parents we don't find, add a promise that will trigger the lookup on
  // its necessary child.
  //
  // (It might have been simpler to recursively call lookupInode() to get the
  // parent, but that would require releasing and re-acquiring the lock more
  // than necessary.)
  auto childInodeNumber = number;
  while (true) {
    // Check to see if this parent is loaded
    loadedIter = data->loadedInodes_.find(unloadedData->parent);
    if (loadedIter != data->loadedInodes_.end()) {
      // We found a loaded parent.
      // Grab copies of the arguments we need for startChildLookup(),
      // with the lock still held.
      InodePtr firstLoadedParent = loadedIter->second.getPtr();
      PathComponent requiredChildName = unloadedData->name;
      bool isUnlinked = unloadedData->isUnlinked;
      std::optional<ObjectId> optionalId = unloadedData->id;
      auto mode = unloadedData->mode;
      // Unlock data and publish load events before starting the child lookup
      data.unlock();
      for (auto& event : startLoadEvents) {
        mount_->publishInodeTraceEvent(std::move(event));
      }
      // Trigger the lookup, then return to our caller.
      startChildLookup(
          firstLoadedParent,
          requiredChildName,
          isUnlinked,
          childInodeNumber,
          optionalId,
          mode,
          context);
      return result;
    }

    // Look up the parent in unloadedInodes_
    unloadedIter = data->unloadedInodes_.find(unloadedData->parent);
    if (UNLIKELY(unloadedIter == data->unloadedInodes_.end())) {
      // This shouldn't happen.  We must know about the parent inode number if
      // we knew about the child.
      auto bug = EDEN_BUG_EXCEPTION()
          << "unknown parent inode " << unloadedData->parent << " (of "
          << unloadedData->name << ")";
      // Unlock our data before publishing any stored load start events and
      // calling inodeLoadFailed()
      data.unlock();
      for (auto& event : startLoadEvents) {
        mount_->publishInodeTraceEvent(std::move(event));
      }
      inodeLoadFailed(childInodeNumber, bug);
      return result;
    }

    auto* parentData = &unloadedIter->second;
    alreadyLoading = !parentData->promises.empty();

    if (!alreadyLoading) {
      startLoadEvents.push_back(
          createInodeLoadStartEvent(unloadedData->parent, *parentData, data));
    }

    // Add a new entry to the promises list.
    // It should kick off loading of the current child inode when
    // it is fulfilled.
    parentData->promises.emplace_back();
    setupParentLookupPromise(
        parentData->promises.back(),
        unloadedData->name,
        unloadedData->isUnlinked,
        childInodeNumber,
        unloadedData->id,
        unloadedData->mode,
        context);

    if (alreadyLoading) {
      // This parent is already being loaded.
      // We don't need to trigger any new loads ourself.
      data.unlock();
      for (auto& event : startLoadEvents) {
        mount_->publishInodeTraceEvent(std::move(event));
      }
      return result;
    }

    // Continue around the loop to look up our parent's parent
    childInodeNumber = unloadedData->parent;
    unloadedData = parentData;
  }
}

void InodeMap::setupParentLookupPromise(
    Promise<InodePtr>& promise,
    PathComponentPiece childName,
    bool isUnlinked,
    InodeNumber childInodeNumber,
    std::optional<ObjectId> id,
    mode_t mode,
    const ObjectFetchContextPtr& context) {
  promise.getFuture()
      .thenValue([name = PathComponent(childName),
                  this,
                  isUnlinked,
                  childInodeNumber,
                  id,
                  mode,
                  context = context.copy()](const InodePtr& inode) {
        startChildLookup(
            inode, name, isUnlinked, childInodeNumber, id, mode, context);
      })
      .thenError([this, childInodeNumber](const folly::exception_wrapper& ex) {
        // Fail all pending lookups on the child
        inodeLoadFailed(childInodeNumber, ex);
      });
}

void InodeMap::startChildLookup(
    const InodePtr& parent,
    PathComponentPiece childName,
    bool isUnlinked,
    InodeNumber childInodeNumber,
    std::optional<ObjectId> id,
    mode_t mode,
    const ObjectFetchContextPtr& context) {
  auto fault = mount_->getServerState()->getFaultInjector().checkTry(
      "inodemap-load", childInodeNumber);
  if (fault.hasException()) {
    return inodeLoadFailed(childInodeNumber, fault.exception());
  }
  auto treeInode = parent.asTreePtrOrNull();
  if (!treeInode) {
    auto bug = EDEN_BUG_EXCEPTION()
        << "parent inode " << parent->getNodeId() << " of (" << childName
        << ", " << childInodeNumber << ") does not refer to a tree";
    return inodeLoadFailed(childInodeNumber, bug);
  }

  if (isUnlinked) {
    treeInode->loadUnlinkedChildInode(childName, childInodeNumber, id, mode);
    return;
  }

  // Ask the TreeInode to load this child inode.
  //
  // (Inode lookups can also be triggered by TreeInode::getOrLoadChild().
  // In that case getOrLoadChild() will call startLoadingChildIfNotLoading() to
  // tell if it should continue the load itself, or if the load was already in
  // progress.)
  treeInode->loadChildInode(childName, childInodeNumber, context);
}

InodeMap::PromiseVector InodeMap::inodeLoadComplete(InodeBase* inode) {
  auto number = inode->getNodeId();
  // Since XLOG only evaluates the arguments if it is going to log, its cheaper
  // in the most common case to not save the inode log path to a local variable
  XLOGF(DBG5, "successfully loaded inode {}: {}", number, inode->getLogPath());

  PromiseVector promises;
  try {
    std::optional<InodeTraceEvent> endLoadEvent;
    {
      auto data = data_.wlock();
      auto it = data->unloadedInodes_.find(number);
      XCHECK(it != data->unloadedInodes_.end()) << fmt::format(
          "failed to find unloaded inode data when finishing load of inode {}: {}",
          number,
          inode->getLogPath());
      swap(promises, it->second.promises);

      inode->setChannelRefcount(it->second.numFsReferences);
      inode->restoreLastGcRefTime(it->second.lastGcRefTime);

      // Insert the entry into loadedInodes_ and remove it from unloadedInodes_
      insertLoadedInode(data, inode);
      // Before removing from unloadedInodes_, create an inode end event (for
      // which we need the data_ lock to read attributes) and publish the event
      // after releasing the data_ lock
      endLoadEvent = std::make_optional<InodeTraceEvent>(
          it->second.loadStartTime,
          number,
          it->second.getInodeType(),
          InodeEventType::LOAD,
          InodeEventProgress::END,
          it->second.name);
      eraseUnloadedInode(data, it);
    }
    mount_->publishInodeTraceEvent(std::move(endLoadEvent.value()));
    stats_->increment(
        inode->isDir() ? &InodeMapStats::lookupTreeInodeMiss
                       : &InodeMapStats::lookupBlobInodeMiss,
        promises.size());
    return promises;
  } catch (...) {
    auto ew = folly::exception_wrapper{std::current_exception()};
    auto outcome = ErrorLogOutcome::Disabled;
    ew.with_exception([&](const std::exception& e) {
      outcome = mount_->getServerState()->getErrorLogger().log(
          EdenErrorInfo::objectStore(e)
              .withInode(number.getRawValue())
              .withMountPoint(mount_->getPath().asString())
              .withErrorType("inode_load_complete_failed"));
    });
    if (outcome != ErrorLogOutcome::RateLimited) {
      XLOGF(ERR, "error marking inode {} loaded: {}", number, ew.what());
    }
    for (auto& promise : promises) {
      promise.setException(ew);
    }
    auto optionalFailEvent = createInodeLoadFailEvent(number);
    if (optionalFailEvent.has_value()) {
      mount_->publishInodeTraceEvent(std::move(optionalFailEvent.value()));
    }
    stats_->increment(&InodeMapStats::lookupInodeError, promises.size());
    return PromiseVector{};
  }
}

void InodeMap::inodeLoadFailed(
    InodeNumber number,
    const folly::exception_wrapper& ex,
    bool logAsError) {
  auto promises = extractPendingPromises(number);
  for (auto& promise : promises) {
    promise.setException(ex);
  }
  auto optionalFailEvent = createInodeLoadFailEvent(number);
  if (optionalFailEvent.has_value()) {
    mount_->publishInodeTraceEvent(std::move(optionalFailEvent.value()));
  }

  if (logAsError) {
    auto outcome = ErrorLogOutcome::Disabled;
    ex.with_exception([&](const std::exception& e) {
      outcome = mount_->getServerState()->getErrorLogger().log(
          EdenErrorInfo::objectStore(ErrorArg::fromExceptionWithoutTrace(e))
              .withInode(number.getRawValue())
              .withMountPoint(mount_->getPath().asString())
              .withErrorType("inode_loading_failed"));
    });
    if (outcome != ErrorLogOutcome::RateLimited) {
      XLOGF(
          ERR, "failed to load inode {}: {}", number, folly::exceptionStr(ex));
    }
  }
  stats_->increment(&InodeMapStats::lookupInodeError, promises.size());
}

InodeTraceEvent InodeMap::createInodeLoadStartEvent(
    InodeNumber number,
    UnloadedInode& unloadedData,
    const folly::Synchronized<Members>::LockedPtr& /* data */) {
  unloadedData.loadStartTime = std::chrono::system_clock::now();
  return InodeTraceEvent(
      unloadedData.loadStartTime,
      number,
      unloadedData.getInodeType(),
      InodeEventType::LOAD,
      InodeEventProgress::START,
      unloadedData.name);
}

std::optional<InodeTraceEvent> InodeMap::createInodeLoadFailEvent(
    InodeNumber number) {
  auto data = data_.rlock();
  auto it = data->unloadedInodes_.find(number);
  if (it == data->unloadedInodes_.end()) {
    XLOGF(
        ERR,
        "failed to find unloaded inode data when finishing load of inode {}",
        number);
    return std::nullopt;
  }
  return InodeTraceEvent(
      it->second.loadStartTime,
      number,
      it->second.getInodeType(),
      InodeEventType::LOAD,
      InodeEventProgress::FAIL,
      it->second.name);
}

InodeMap::PromiseVector InodeMap::extractPendingPromises(InodeNumber number) {
  PromiseVector promises;
  {
    auto data = data_.wlock();
    auto it = data->unloadedInodes_.find(number);
    XCHECK(it != data->unloadedInodes_.end()) << fmt::format(
        "failed to find unloaded inode data when finishing load of inode {}",
        number);
    swap(promises, it->second.promises);
  }
  return promises;
}

ImmediateFuture<TreeInodePtr> InodeMap::lookupTreeInode(
    InodeNumber number,
    const ObjectFetchContextPtr& context) {
  return lookupInode(number, context).thenValue([](const InodePtr& inode) {
    return inode.asTreePtr();
  });
}

ImmediateFuture<FileInodePtr> InodeMap::lookupFileInode(
    InodeNumber number,
    const ObjectFetchContextPtr& context) {
  return lookupInode(number, context).thenValue([](const InodePtr& inode) {
    return inode.asFilePtr();
  });
}

InodePtr InodeMap::lookupLoadedInode(InodeNumber number) {
  auto data = data_.rlock();
  auto it = data->loadedInodes_.find(number);
  if (it == data->loadedInodes_.end()) {
    return nullptr;
  }
  auto inode = it->second.getPtr();
  stats_->increment(
      inode->isDir() ? &InodeMapStats::lookupTreeInodeHit
                     : &InodeMapStats::lookupBlobInodeHit);
  return inode;
}

TreeInodePtr InodeMap::lookupLoadedTree(InodeNumber number) {
  auto inode = lookupLoadedInode(number);
  if (!inode) {
    return nullptr;
  }
  return inode.asTreePtr();
}

FileInodePtr InodeMap::lookupLoadedFile(InodeNumber number) {
  auto inode = lookupLoadedInode(number);
  if (!inode) {
    return nullptr;
  }
  return inode.asFilePtr();
}

std::optional<RelativePath> InodeMap::getPathForInode(InodeNumber inodeNumber) {
  auto data = data_.rlock();
  return getPathForInodeHelper(inodeNumber, data);
}

std::optional<RelativePath> InodeMap::getPathForInodeHelper(
    InodeNumber inodeNumber,
    const folly::Synchronized<Members>::RLockedPtr& data) {
  auto loadedIt = data->loadedInodes_.find(inodeNumber);
  if (loadedIt != data->loadedInodes_.cend()) {
    // If the inode is loaded, return its RelativePath
    return loadedIt->second->getPath();
  } else {
    auto unloadedIt = data->unloadedInodes_.find(inodeNumber);
    if (unloadedIt != data->unloadedInodes_.cend()) {
      if (unloadedIt->second.isUnlinked) {
        return std::nullopt;
      }
      // If the inode is not loaded, return its parent's path as long as it's
      // parent isn't the root
      auto parent = unloadedIt->second.parent;
      if (parent == kRootNodeId) {
        // The parent is the Eden mount root, just return its name (base case)
        return RelativePath(unloadedIt->second.name);
      }
      auto dir = getPathForInodeHelper(parent, data);
      if (!dir) {
        EDEN_BUG() << "unlinked parent inode " << parent
                   << "appears to contain non-unlinked child " << inodeNumber;
      }
      return *dir + unloadedIt->second.name;
    } else {
      throwSystemErrorExplicit(
          EINVAL, fmt::format("unknown inode number {}", inodeNumber));
    }
  }
}
void InodeMap::decFsRefcount(InodeNumber number, uint32_t count) {
  InodePtr inodePtr;
  {
    auto data = data_.wlock();
    inodePtr = decFsRefcountHelper(data, number, count);
  }
  // Now release our lock before decrementing the inode's FS reference
  // count and immediately releasing our pointer reference.
  if (inodePtr) {
    inodePtr->decFsRefcount(count);
  }
}

bool InodeMap::clearFsRefcount(InodeNumber number) {
  InodePtr inodePtr;
  bool wasReferenced = false;
  {
    auto data = data_.wlock();
    auto unloadedIter = data->unloadedInodes_.find(number);
    if (unloadedIter != data->unloadedInodes_.end()) {
      wasReferenced = unloadedIter->second.numFsReferences != 0;
    }
    inodePtr =
        decFsRefcountHelper(data, number, /*count=*/0, /*clearRefCount=*/true);
  }
  // Now release our lock before clearing the inode's FS reference
  // count and immediately releasing our pointer reference.
  if (inodePtr) {
    wasReferenced = inodePtr->clearFsRefcount();
  }
  return wasReferenced;
}

InodePtr InodeMap::decFsRefcountHelper(
    folly::Synchronized<Members>::LockedPtr& data,
    InodeNumber number,
    uint32_t count,
    bool clearRefCount) {
  if (folly::kIsWindows) {
    XDCHECK_EQ(count, 1u);
  }

  // First check in the loaded inode map
  auto loadedIter = data->loadedInodes_.find(number);
  if (loadedIter != data->loadedInodes_.end()) {
    auto inode = loadedIter->second.getPtr();
    stats_->increment(
        inode->isDir() ? &InodeMapStats::lookupTreeInodeHit
                       : &InodeMapStats::lookupBlobInodeHit);
    // Acquire an InodePtr, so that we are always holding a pointer reference
    // on the inode when we decrement the fs refcount.
    //
    // This ensures that onInodeUnreferenced() will be processed at some point
    // after decrementing the FS refcount to 0, even if there were no
    // outstanding pointer references before this.
    return inode;
  }

  // If it wasn't loaded, it should be in the unloaded map
  auto unloadedIter = data->unloadedInodes_.find(number);
  if (UNLIKELY(unloadedIter == data->unloadedInodes_.end())) {
    stats_->increment(&InodeMapStats::lookupInodeError);
    if (mount_->throwEstaleIfInodeIsMissing()) {
      // Userspace FS peers (NFS, FSKit) can send forgets that violate the
      // reference contract in ways a kernel client cannot; a misbehaving
      // peer must never be able to abort the daemon. There is no reply to
      // a FORGET, so tolerating means ignoring it.
      stats_->increment(&InodeMapStats::forgetUnknownInode);
      XLOG_EVERY_MS(WARN, 60000) << fmt::format(
          "ignoring FORGET for unknown inode {} from userspace FS peer",
          number);
      return nullptr;
    }
    EDEN_BUG() << "InodeMap::decFsRefcount() called on unknown inode number "
               << number;
  }

  // Decrement the reference count in the unloaded entry. Only FUSE counts
  // references, and InodeBase::incFsRefcount counts them only on Linux, so
  // the same test applies here. The PrjFS invalidation path arrives with the
  // 0/1 flag that InodeBase::decFsRefcount keeps for loaded inodes, and NFS
  // only ever clears through forgetStaleInodes(). An inode may be remembered
  // without a reference, for example while a load is pending, so clear
  // rather than subtract.
  auto& unloadedEntry = unloadedIter->second;
  if (clearRefCount || !folly::kIsLinux) {
    unloadedEntry.numFsReferences = 0;
  } else {
    XCHECK_GE(unloadedEntry.numFsReferences, count);
    unloadedEntry.numFsReferences -= count;
  }

  // Make sure that this inode isn't being loaded before removing it from the
  // unloadedInodes_ map.
  if (unloadedEntry.numFsReferences == 0 && unloadedEntry.promises.empty()) {
    // We can completely forget about this unloaded inode now.
    XLOGF(
        DBG5,
        "forgetting unloaded inode {}: {}:{}",
        number,
        unloadedEntry.parent,
        unloadedEntry.name);
    ++data->numForgottenInodes_;
    eraseUnloadedInode(data, unloadedIter);
  }
  return nullptr;
}

void InodeMap::forgetStaleInodes() {
  // Note this functionality is pretty similar to the periodic Inode unloading
  // which unloads old inodes (EdenServer::unloadInodes). The key difference is
  // that this code unloads UNLINKED inodes where as that code unloads LINKED
  // inodes. That codepath only considers unloading inodes reachable from the
  // root inode which means the inodes must be linked to be considered for
  // unloading. We might want to merge the two, however at the time of writing
  // linked inode unloading is broken. We should consider merging
  // both linked and unlinked inode unloading in the future: T100682833
#ifndef _WIN32
  // We do not track metadata for inodes on Windows. This was originally because
  // ProjFS does not send most requests after a file is first read to EdenFS.
  // If we support NFS on Windows, we need to keep track of inode metadata
  // on Windows to enable periodic inode unloading: T100720928.

  // TODO add a checkout decRef counter
  // TODO: this will unload by atime, atime is not updated by stat -- fix it

  XLOG(DBG2, "forgetting stale inodes");
  // We have to destroy InodePtrs outside of the data lock. These hold all the
  // InodePtrs we created.
  std::vector<InodePtr> toClearFSRef;
  std::vector<InodePtr> justToHoldBeyondScopeOfLock;
  auto cutoff = std::chrono::system_clock::now() -
      config_->getEdenConfig()->postCheckoutDelayToUnloadInodes.getValue();
  auto cutoff_ts = folly::to<timespec>(cutoff);
  std::vector<InodeNumber> unloadedInodesToClearFSRef;

  {
    auto data = data_.wlock();

    for (auto& inode : data->unloadedInodes_) {
      XLOGF(DBG9, "Considering forgetting unloaded inode {}", inode.first);
      if (inode.second.isUnlinked) {
        // We can't directly call decFsRefcountHelper here because it will
        // invalidate the iterator we are using for this for loop.
        unloadedInodesToClearFSRef.push_back(inode.first);
      } else {
        XLOGF(
            DBG9,
            "Not forgetting unloaded inode {} because inode is still linked",
            inode.first);
      }
    }

    for (auto& inodeNumber : unloadedInodesToClearFSRef) {
      auto inodePtr = decFsRefcountHelper(
          data,
          inodeNumber,
          /*count=*/0, // Doesn't matter what we set this to because we are
                       // going to clear the ref count.
          /*clearRefCount=*/true);
      XCHECK(!inodePtr)
          << "decFsRefcountHelper should not return a loaded inode that  "
          << "needs to be dereferenced for an inode we know to be unloaded.";
    }

    // we do this second because dereferencing a loaded inode will cause it to
    // be unloaded. Thus this will create lots of unloaded inodes. we don't want
    // to double decRef them, so we decref loaded inodes after unloaded ones.
    for (auto& inode : data->loadedInodes_) {
      XLOGF(DBG9, "Considering forgetting loaded inode {}", inode.first);
      auto inodePtr = decFsRefcountHelper(
          data,
          inode.first,
          /*count=*/0, // Doesn't matter what we set this to because we are
                       // going to clear the ref count.
          /*clearRefCount=*/true);
      if (inodePtr) {
        auto unlinked = inodePtr->isUnlinked();
        if (unlinked &&
            inode.second->getMetadata().timestamps.atime < cutoff_ts) {
          XLOGF(DBG9, "Will forget loaded inode {}", inode.first);
          toClearFSRef.push_back(inodePtr);
        } else {
          // even though we are not going to do anything with these inodes we
          // need to keep them around until we let go of the lock. It is not
          // safe to drop an inodePtr while holding the lock.
          if (!unlinked) {
            XLOGF(
                DBG9,
                "Not forgetting loaded inode {} because it is still linked",
                inode.first);
          } else {
            XLOGF(
                DBG9,
                "Not forgetting loaded inode {} because it was referenced {} ago",
                inode.first,
                durationStr(
                    config_->getEdenConfig()
                        ->postCheckoutDelayToUnloadInodes.getValue() -
                    std::chrono::nanoseconds{
                        inode.second->getMetadata()
                            .timestamps.atime.toTimespec()
                            .tv_nsec -
                        cutoff_ts.tv_nsec}));
          }
          justToHoldBeyondScopeOfLock.push_back(inodePtr);
        }
      }
    }
    numPeriodicallyUnloadedUnlinkedInodes_.fetch_add(
        toClearFSRef.size(), std::memory_order_relaxed);
  }

  for (auto& inodePtr : toClearFSRef) {
    XLOGF(DBG7, "forgetting NFS inode: {}", inodePtr->getNodeId());
    inodePtr->clearFsRefcount();
  }
  XLOG(DBG2, "forgetting stale inodes complete");
#endif
}

void InodeMap::setUnmounted() {
  auto data = data_.wlock();
  XDCHECK(!data->isUnmounted_);
  data->isUnmounted_ = true;
}

Future<SerializedInodeMap> InodeMap::shutdown(
    [[maybe_unused]] bool doTakeover) {
  // Record that we are in the process of shutting down.
  auto future = Future<folly::Unit>::makeEmpty();
  {
    auto data = data_.wlock();
    XCHECK(!data->shutdownPromise.has_value()) << fmt::format(
        "shutdown() invoked more than once on InodeMap for {}",
        mount_->getPath());
    data->shutdownPromise.emplace(Promise<Unit>{});
    future = data->shutdownPromise->getFuture();

    XLOGF(
        DBG3,
        "starting InodeMap::shutdown: loadedCount={} unloadedCount={}",
        data->loadedInodes_.size(),
        data->unloadedInodes_.size());
  }

  // If an error occurs during mount point initialization, shutdown() can be
  // called in some cases even if InodeMap::initialize() was never called.
  // Just return immediately in this case.
  if (!root_) {
    return folly::makeFuture(SerializedInodeMap{});
  }

  // Walk from the root of the tree down, finding all unreferenced inodes,
  // and immediately destroy them.
  //
  // Hold the mountpoint-wide rename lock in shared mode while doing the
  // walk.  We want to make sure that we walk *all* children.  While doing the
  // walk we want to make sure that an Inode that hasn't been processed yet
  // cannot be moved from the unprocessed part of the tree into a processed
  // part of the tree.
  {
    auto renameLock = mount_->acquireSharedRenameLock();
    root_->unloadChildrenNow();
  }

  // Also walk loadedInodes_ to immediately destroy all unreferenced unlinked
  // inodes.  (There may be unlinked inodes that have no outstanding pointer
  // references, but outstanding FS references.)
  //
  // We walk normal inodes via the root since it is easier to hold the parent
  // TreeInode's contents lock as we walk down from the root.  However, we
  // can't find unlinked inodes that way.  For unlinked inodes we don't need to
  // hold the parent's contents lock, so scanning loadedInodes_ for them is
  // straightforward.
  {
    // The simplest way to unload the inodes is to simply acquire InodePtrs
    // to them, then let the normal pointer release process be responsible for
    // unloading them.
    std::vector<InodePtr> inodesToUnload;
    auto data = data_.wlock();
    for (const auto& entry : data->loadedInodes_) {
      if (!entry.second->isPtrAcquireCountZero()) {
        continue;
      }
      if (!entry.second->isUnlinked()) {
        continue;
      }
      inodesToUnload.push_back(entry.second.getPtr());
    }
    // Release the lock, then release all of our InodePtrs to unload
    // the inodes.
    data.unlock();
    inodesToUnload.clear();
  }

  // Decrement our reference count on root_.
  // This method lets us manually drop our reference count while still
  // retaining our pointer.  When onInodeUnreferenced() is called for the root
  // we know that all inodes have been destroyed and we can complete shutdown.
  root_.manualDecRef();

  return std::move(future).thenValue([this, doTakeover](auto&&) {
    // TODO: This check could occur after the loadedInodes_ assertion below to
    // maximize coverage of any invariants that are broken during shutdown.
    if (!doTakeover) {
      return SerializedInodeMap{};
    }

    auto data = data_.wlock();
    XLOGF(
        DBG3,
        "InodeMap::shutdown after releasing inodesToClear: loadedCount={} unloadedCount={}",
        data->loadedInodes_.size(),
        data->unloadedInodes_.size());

    if (!data->loadedInodes_.empty()) {
      EDEN_BUG() << "After InodeMap::shutdown() finished, "
                 << data->loadedInodes_.size()
                 << " inodes still loaded; they must all "
                 << "have been unloaded for this to succeed!";
    }

    SerializedInodeMap result;
    result.unloadedInodes()->reserve(data->unloadedInodes_.size());
    for (const auto& [inodeNumber, entry] : data->unloadedInodes_) {
      SerializedInodeMapEntry serializedEntry;

      XLOGF(
          DBG5,
          "  serializing unloaded inode {} parent={} name={}",
          inodeNumber,
          entry.parent.get(),
          entry.name);

      serializedEntry.inodeNumber() = inodeNumber.get();
      serializedEntry.parentInode() = entry.parent.get();
      serializedEntry.name() = entry.name.asString();
      serializedEntry.isUnlinked() = entry.isUnlinked;
      serializedEntry.numFsReferences() = entry.numFsReferences;
      if (entry.id.has_value()) {
        serializedEntry.hash() = entry.id.value().asString();
      }
      // If entry.id is empty, the inode is materialized.
      serializedEntry.mode() = entry.mode;
      serializedEntry.lastFsRequestTime() =
          entry.lastGcRefTime.toSerializedValue();

      result.unloadedInodes()->emplace_back(std::move(serializedEntry));
    }

    return result;
  });
}

void InodeMap::shutdownComplete(
    folly::Synchronized<Members>::LockedPtr&& data) {
  // We manually dropped our reference count to the root inode in
  // shutdown().  Destroy it now, remove it from the loadedInodes, and call
  // resetNoDecRef() on our pointer to make sure it doesn't try to decrement the
  // reference count again when the pointer is destroyed. Note: we don't add
  // the root to unloadedInodes here as it has been freed and we don't want to
  // serialize the freed root during graceful shutdown for takeover.
  eraseLoadedInode(data, root_.get());
  delete root_.get();
  root_.resetNoDecRef();

  // Unlock data_ before fulfilling the shutdown promise, just in case the
  // promise invokes a callback that calls some of our other methods that
  // may need to acquire this lock.
  auto* shutdownPromise = &data->shutdownPromise.value();
  data.unlock();
  shutdownPromise->setValue();
}

bool InodeMap::isInodeRemembered(InodeNumber ino) const {
  return data_.rlock()->unloadedInodes_.contains(ino);
}

bool InodeMap::isInodeLoadedOrRemembered(InodeNumber ino) const {
  auto members = data_.rlock();
  return members->unloadedInodes_.contains(ino) ||
      members->loadedInodes_.contains(ino);
}

bool InodeMap::onLinkedInodeUnreferenced(InodeBase* inode) {
  auto data = data_.wlock();
  if (data->shutdownPromise.has_value() || inode->isUnlinked()) {
    return false;
  }

  // Linked inodes are kept loaded below, so they do not need ParentInodeInfo.
  // Avoid acquiring the parent contents lock here: the last InodePtr may be
  // dropped by code already holding that same lock.
  inode->decPtrAcquireCount();
  return true;
}

void InodeMap::onInodeUnreferenced(
    InodeBase* inode,
    ParentInodeInfo&& parentInfo) {
  XLOGF(
      DBG8,
      "inode {} unreferenced: {}",
      inode->getNodeId(),
      inode->getLogPath());
  // Acquire our lock.
  auto data = data_.wlock();

  // Decrement the Inode's acquire count
  auto acquireCount = inode->decPtrAcquireCount();
  if (acquireCount != 1) {
    // Someone else has already re-acquired a reference to this inode.
    // We can't destroy it yet.
    return;
  }

  // Decide if we should unload the inode now, or wait until later.
  bool unloadNow = false;
  bool shuttingDown = data->shutdownPromise.has_value();
  bool parentEntryUnavailable = parentInfo.parentEntryUnavailable();
  XDCHECK(shuttingDown || inode != root_.get());
  if (shuttingDown) {
    // Check to see if this was the root inode that got unloaded.
    // This indicates that the shutdown is complete.
    if (inode == root_.get()) {
      shutdownComplete(std::move(data));
      return;
    }

    // Always unload Inode objects immediately when shutting down.
    // We can't destroy the EdenMount until all inodes get unloaded.
    unloadNow = true;
  } else if (parentEntryUnavailable && inode->getFsRefcount() == 0) {
    // This inode has no usable parent entry and no outstanding FS references.
    // It can now be completely destroyed and forgotten about.
    unloadNow = true;
  } else {
    // In other cases:
    // - If the inode is materialized, we should never unload it.
    // - Otherwise, we have the option to unload it or not.
    //   For now we choose to always keep it loaded.
  }

  if (unloadNow) {
    unloadInode(
        inode,
        parentInfo.getParent().get(),
        parentInfo.getName(),
        parentEntryUnavailable,
        data);
    if (!parentEntryUnavailable) {
      const auto& parentContents = parentInfo.getParentContents();
      auto it = parentContents->entries.find(parentInfo.getName());
      XCHECK(it != parentContents->entries.end());
      auto released = it->second.clearInode();
      XCHECK_EQ(inode, released);
    }
  }

  // If we unloaded the inode, only delete it after we release our locks.
  // Deleting it may cause its parent TreeInode to become unreferenced, causing
  // another recursive call to onInodeUnreferenced(), which will need to
  // reacquire the lock.
  data.unlock();
  parentInfo.reset();
  if (unloadNow) {
    delete inode;
  }
}

InodeMapLock InodeMap::lockForUnload() {
  return InodeMapLock{data_.wlock()};
}

void InodeMap::unloadInode(
    InodeBase* inode,
    TreeInode* parent,
    PathComponentPiece name,
    bool isUnlinked,
    const InodeMapLock& lock) {
  return unloadInode(inode, parent, name, isUnlinked, lock.data_);
}

void InodeMap::unloadInode(
    InodeBase* inode,
    TreeInode* parent,
    PathComponentPiece name,
    bool isUnlinked,
    const folly::Synchronized<Members>::LockedPtr& data) {
  // Call updateOverlayForUnload() to update the overlay and compute
  // if we need to remember an UnloadedInode entry.
  auto unloadedEntry =
      updateOverlayForUnload(inode, parent, name, isUnlinked, data);
  if (unloadedEntry) {
    // Insert the unloaded entry
    XLOGF(
        DBG7, "inserting unloaded map entry for inode {}", inode->getNodeId());
    insertUnloadedInode(
        data, inode->getNodeId(), std::move(unloadedEntry.value()));
  }

  eraseLoadedInode(data, inode);
}

bool InodeMap::hasRememberedChildForUnload(
    const TreeInode& inode,
    const InodeMapLock& lock) const {
  return hasRememberedChildForUnload(inode, lock.data_);
}

bool InodeMap::hasRememberedChildForUnload(
    const TreeInode& inode,
    const folly::Synchronized<Members>::LockedPtr& data) const {
  // After unmount nothing is remembered, so unloading always forgets the
  // tree; see the matching check in updateOverlayForUnload().
  if (data->isUnmounted_) {
    return false;
  }
  // The caller has established that nobody can acquire this inode, matching
  // the exception to the lock hierarchy used by updateOverlayForUnload().
  const auto& contents = inode.getContentsUnchecked().unsafeGetUnlocked();
  for (const auto& [_, entry] : contents.entries.all()) {
    if (data->unloadedInodes_.contains(entry.getInodeNumber())) {
      return true;
    }
  }
  return false;
}

optional<InodeMap::UnloadedInode> InodeMap::updateOverlayForUnload(
    InodeBase* inode,
    TreeInode* parent,
    PathComponentPiece name,
    bool isUnlinked,
    const folly::Synchronized<Members>::LockedPtr& data) {
  auto fsCount = inode->getFsRefcount();
  if (isUnlinked && (data->isUnmounted_ || fsCount == 0)) {
    // Removing the overlay data is a btrfs unlink and inode eviction, hundreds
    // of microseconds under load; every caller deletes the inode after
    // releasing the locks held here, so the destructor does it then instead
    // of stalling every other InodeMap user on it.
    inode->removeOverlayDataOnDestruction();
  }
  // If the mount point has been unmounted, ignore any outstanding FS
  // refcounts on inodes that still existed before it was unmounted.
  // Everything is unreferenced by FS after an unmount operation, and we no
  // longer need to remember anything in the unloadedInodes_ map.
  if (data->isUnmounted_) {
    XLOGF(
        DBG5,
        "forgetting unreferenced inode {} after unmount: {}",
        inode->getNodeId(),
        inode->getLogPath());
    return std::nullopt;
  }

  // If the tree is unlinked and no longer referenced we can delete it from
  // the overlay and completely forget about it.
  if (isUnlinked && fsCount == 0) {
    XLOGF(
        DBG5,
        "forgetting unreferenced unlinked inode {}: {}",
        inode->getNodeId(),
        inode->getLogPath());
    return std::nullopt;
  }

  if (auto* asTree = dynamic_cast<TreeInode*>(inode)) {
    // Normally, acquiring the tree's contents lock while the InodeMap members
    // lock is held violates our lock hierarchy. However, since this TreeInode
    // is being unloaded, nobody else can reference it right now, so the lock is
    // guaranteed not held. Therefore, it's not necessary to synchronize, and
    // the contents can be directly accessed here.
    auto& treeContents = asTree->getContentsUnchecked().unsafeGetUnlocked();
    auto makeUnloadedTree = [&] {
      // TreeInode does not expose a mode; Eden directories use this mode when
      // represented in the unloaded inode map.
      return UnloadedInode(
          parent->getNodeId(),
          name,
          isUnlinked,
          S_IFDIR | 0755,
          treeContents.treeId,
          fsCount,
          asTree->getLastGcRefTime());
    };

    // If the fs refcount is non-zero we have to remember this inode.
    if (fsCount > 0) {
      XLOGF(
          DBG5,
          "unloading tree inode {} with Fs refcount={}: {}",
          inode->getNodeId(),
          fsCount,
          inode->getLogPath());
      return makeUnloadedTree();
    }

    // If any of this inode's children are in unloadedInodes_, then this
    // inode, as its parent, must not be forgotten.
    if (hasRememberedChildForUnload(*asTree, data)) {
      XLOGF(
          DBG5,
          "remembering inode {} ({}) because one of its children was remembered",
          asTree->getNodeId(),
          asTree->getLogPath());
      return makeUnloadedTree();
    }
    return std::nullopt;
  } else {
    // We have to remember files only if their FS refcount is non-zero
    if (fsCount > 0) {
      XLOGF(
          DBG5,
          "unloading file inode {} with FS refcount={}: {}",
          inode->getNodeId(),
          fsCount,
          inode->getLogPath());
      auto* asFile = boost::polymorphic_downcast<FileInode*>(inode);
      return UnloadedInode(asFile, parent, name, isUnlinked, fsCount);
    } else {
      XLOGF(
          DBG5,
          "forgetting unreferenced file inode {}: {}",
          inode->getNodeId(),
          inode->getLogPath());
      return std::nullopt;
    }
  }
}

bool InodeMap::startLoadingChildIfNotLoading(
    TreeInode* parent,
    PathComponentPiece name,
    InodeNumber childInode,
    mode_t mode,
    folly::Promise<InodePtr> promise) {
  bool isFirstPromise;
  std::optional<InodeTraceEvent> startLoadEvent;
  {
    auto data = data_.wlock();
    if (auto loaded = data->loadedInodes_.find(childInode);
        loaded != data->loadedInodes_.end()) {
      auto loadedAs = loaded->second->getLogPath();
      data.unlock();
      failLoadOfDuplicateEntry(
          parent, name, childInode, loadedAs, std::move(promise));
      return false;
    }
    UnloadedInode* unloadedData{nullptr};
    auto iter = data->unloadedInodes_.find(childInode);
    // The caller holds the parent's contents lock and its DirEntry has no
    // inode pointer. A loaded inode therefore belongs to a different entry.
    // Likewise, only the same linked entry may join an in-progress load.
    // A torn rename can leave two disk entries referring to the same inode;
    // do not attach the second entry to that inode or its load promises.
    if (iter != data->unloadedInodes_.end() &&
        !iter->second.matchesLinkedChild(parent->getNodeId(), name, mode)) {
      data.unlock();
      promise.setException(
          folly::make_exception_wrapper<std::system_error>(
              EIO,
              std::generic_category(),
              fmt::format(
                  "conflicting inode reference to inode {}", childInode)));
      return false;
    }
    if (iter == data->unloadedInodes_.end()) {
      InodeNumber parentNumber = parent->getNodeId();
      // T127459236: not all attributes of the UnloadedInode are set here. For
      // example, isUnlinked, id, and numFsReferences are set to default
      // values
      auto newUnloadedData = UnloadedInode(
          parentNumber,
          name,
          mode,
          EdenTimestamp{mount_->getClock().getRealtime()});
      unloadedData =
          &insertUnloadedInode(data, childInode, std::move(newUnloadedData));
    } else {
      unloadedData = &iter->second;
    }

    isFirstPromise = unloadedData->promises.empty();

    if (isFirstPromise) {
      startLoadEvent = std::make_optional<InodeTraceEvent>(
          createInodeLoadStartEvent(childInode, *unloadedData, data));
    }

    // Add the promise to the existing list for this inode.
    unloadedData->promises.push_back(std::move(promise));
  }
  if (startLoadEvent.has_value()) {
    mount_->publishInodeTraceEvent(std::move(startLoadEvent.value()));
  }
  // If this is the very first promise then tell the caller they need
  // to start the load operation.  Otherwise someone else (whoever added the
  // first promise) has already started loading the inode.
  return isFirstPromise;
}

void InodeMap::failLoadOfDuplicateEntry(
    TreeInode* parent,
    PathComponentPiece name,
    InodeNumber childInode,
    const std::string& loadedAs,
    folly::Promise<InodePtr> promise) {
  auto message = fmt::format(
      "inode {} is already loaded as {}; refusing to load it again as {}/{}",
      childInode,
      loadedAs,
      parent->getLogPath(),
      name.view());
  auto outcome = mount_->getServerState()->getErrorLogger().log(
      EdenErrorInfo::overlay(message, childInode.getRawValue())
          .withMountPoint(mount_->getPath().asString())
          .withErrorType("duplicate_inode_entry"));
  if (outcome != ErrorLogOutcome::RateLimited) {
    XLOGF(ERR, "{}", message);
  }
  stats_->increment(&InodeMapStats::lookupInodeError);
  promise.setException(InodeError(
      EIO, TreeInodePtr::newPtrFromExisting(parent), name, std::move(message)));
}

void InodeMap::inodeCreated(const InodePtr& inode) {
  XLOGF(
      DBG4,
      "created new inode {}: {}",
      inode->getNodeId(),
      inode->getLogPath());
  auto data = data_.wlock();
  insertLoadedInode(data, inode.get());
}

void InodeMap::recordPeriodicInodeUnload(size_t numInodesToUnload) {
  numPeriodicallyUnloadedLinkedInodes_.fetch_add(
      numInodesToUnload, std::memory_order_relaxed);
}

InodeMap::InodeCounts InodeMap::getInodeCounts() const {
  InodeCounts counts;
  auto data = data_.rlock();
  XDCHECK_EQ(
      data->numTreeInodes_ + data->numFileInodes_, data->loadedInodes_.size());
  counts.treeCount = data->numTreeInodes_;
  counts.fileCount = data->numFileInodes_;
  counts.unloadedInodeCount = data->unloadedInodes_.size();
  counts.forgottenInodeCount = data->numForgottenInodes_;
  counts.periodicUnlinkedUnloadInodeCount =
      numPeriodicallyUnloadedUnlinkedInodes_.load(std::memory_order_relaxed);
  counts.periodicLinkedUnloadInodeCount =
      numPeriodicallyUnloadedLinkedInodes_.load(std::memory_order_relaxed);
  return counts;
}

std::vector<InodeNumber> InodeMap::getReferencedInodes() const {
  std::vector<InodeNumber> inodes;
  {
    auto data = data_.rlock();

    for (auto& kv : data->loadedInodes_) {
      auto& loadedInode = kv.second;

      inodes.push_back(loadedInode->getNodeId());
    }

    for (const auto& [ino, unloadedInode] : data->unloadedInodes_) {
      if (unloadedInode.numFsReferences > 0) {
        inodes.push_back(ino);
      }
    }
  }

  return inodes;
}

std::vector<InodeMap::UnloadedInodeGcEntry> InodeMap::getUnloadedChildrenForGc(
    InodeNumber parent,
    const std::vector<UnloadedInodeGcCandidate>& candidates) const {
  std::vector<UnloadedInodeGcEntry> result;
  result.reserve(candidates.size());

  auto data = data_.rlock();
  for (const auto& candidate : candidates) {
    auto iter = data->unloadedInodes_.find(candidate.inodeNumber);
    if (iter == data->unloadedInodes_.end()) {
      continue;
    }

    const auto& inode = iter->second;
    if (inode.isUnlinked || !inode.promises.empty() || inode.parent != parent ||
        inode.name != candidate.name) {
      continue;
    }
    result.push_back(
        UnloadedInodeGcEntry{
            PathComponent{inode.name},
            inode.lastGcRefTime,
            inode.numFsReferences});
  }
  return result;
}
} // namespace facebook::eden
