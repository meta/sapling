/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/inodes/TreeInode.h"

#include <boost/polymorphic_cast.hpp>
#include <folly/CppAttributes.h>
#include <folly/FileUtil.h>
#include <folly/MapUtil.h>
#include <folly/ScopeGuard.h>
#include <folly/chrono/Conv.h>
#include <folly/coro/Collect.h>
#include <folly/coro/CurrentExecutor.h>
#include <folly/coro/Invoke.h>
#include <folly/coro/Task.h>
#include <folly/futures/Future.h>
#include <folly/io/async/EventBase.h>
#include <folly/logging/xlog.h>
#include <sys/stat.h>
#include <cstring>
#include <vector>

#include "eden/common/telemetry/Tracing.h"
#include "eden/common/utils/Bug.h"
#include "eden/common/utils/CaseSensitivity.h"
#include "eden/common/utils/FaultInjector.h"
#include "eden/common/utils/ImmediateFuture.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/Synchronized.h"
#include "eden/common/utils/SystemError.h"
#include "eden/common/utils/TimeUtil.h"
#include "eden/common/utils/UnboundedQueueExecutor.h"
#include "eden/common/utils/XAttr.h"
#include "eden/fs/config/CheckoutConfig.h"
#include "eden/fs/fuse/FuseChannel.h"
#include "eden/fs/fuse/FuseDirList.h"
#include "eden/fs/inodes/AclState.h"
#include "eden/fs/inodes/CheckoutAction.h"
#include "eden/fs/inodes/CheckoutContext.h"
#include "eden/fs/inodes/ChildEntryAttributes.h"
#include "eden/fs/inodes/DeferredDiffEntry.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/FileInode.h"
#include "eden/fs/inodes/InodeError.h"
#include "eden/fs/inodes/InodeMap.h"
#include "eden/fs/inodes/InodeTable.h"
#include "eden/fs/inodes/Overlay.h"
#include "eden/fs/inodes/OverlayFile.h"
#include "eden/fs/inodes/OverlayFileAccess.h"
#include "eden/fs/inodes/ServerState.h"
#include "eden/fs/journal/Journal.h"
#include "eden/fs/model/Tree.h"
#include "eden/fs/model/TreeAuxData.h"
#include "eden/fs/model/TreeEntry.h"
#include "eden/fs/model/git/GitIgnoreStack.h"
#include "eden/fs/nfs/NfsDirList.h"
#include "eden/fs/nfs/Nfsd3.h"
#include "eden/fs/nfs/NfsdRpc.h"
#ifdef __linux__
#include "eden/fs/utils/MountInfoTable.h"
#endif
#include "eden/fs/prjfs/Enumerator.h"
#ifdef _WIN32
#include "eden/fs/prjfs/PrjfsChannel.h"
#endif
#include "eden/fs/service/ThriftUtil.h"
#include "eden/fs/service/gen-cpp2/eden_types.h"
#include "eden/fs/store/BackingStore.h"
#include "eden/fs/store/DiffCallback.h"
#include "eden/fs/store/DiffContext.h"
#include "eden/fs/store/ObjectStore.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/telemetry/LogEvent.h"
#include "eden/fs/utils/Clock.h"
#include "eden/fs/utils/NotImplemented.h"

using folly::ByteRange;
using folly::Future;
using folly::makeFuture;
using folly::StringPiece;
using folly::Unit;
using std::make_unique;
using std::shared_ptr;
using std::unique_ptr;
using std::vector;

namespace facebook::eden {

#ifndef _WIN32
struct GcBarrierTrie {
  GcBarrierTrie() : children{kPathMapDefaultCaseSensitive} {}

  const GcBarrierTrie* FOLLY_NULLABLE getChild(PathComponentPiece name) const {
    auto child = folly::get_ptr(children, name);
    return child ? child->get() : nullptr;
  }

  GcBarrierTrie* getOrCreateChild(PathComponentPiece name) {
    auto child = folly::get_ptr(children, name);
    if (child) {
      return child->get();
    }
    auto [iter, inserted] =
        children.emplace(name, std::make_unique<GcBarrierTrie>());
    (void)inserted;
    return iter->second.get();
  }

  void add(RelativePathPiece path) {
    auto* node = this;
    for (auto component : path.components()) {
      node = node->getOrCreateChild(component);
    }
    node->isMountRoot = true;
  }

  bool isMountRoot{false};
  PathMap<std::unique_ptr<GcBarrierTrie>> children;

  const GcBarrierTrie* FOLLY_NULLABLE
  getDescendant(RelativePathPiece path) const {
    auto* node = this;
    for (auto component : path.components()) {
      if (node->isMountRoot) {
        return node;
      }
      node = node->getChild(component);
      if (!node) {
        return nullptr;
      }
    }
    return node;
  }
};

#endif

namespace {
static constexpr PathComponentPiece kIgnoreFilename{".gitignore"};

/**
 * For case insensitive system, we need to use the casing of the file as
 * present in the SCM rather than the one used for lookup.
 */
static inline PathComponent copyCanonicalInodeName(
    const DirContents::const_iterator& iter) {
  return PathComponent(iter->first);
}

std::optional<bool> preferKnownAclState(
    std::optional<bool> preferred,
    std::optional<bool> fallback) {
  // Unknown metadata cannot erase a known state for the same tree.
  return preferred.has_value() ? preferred : fallback;
}

bool aclRootStateRequiresCheckoutWalk(
    AclRootState current,
    AclRootState target) {
  return (current == AclRootState::RestrictedAclRoot) !=
      (target == AclRootState::RestrictedAclRoot);
}

bool dirEntryMatchesTreeEntry(
    const DirEntry& dirEntry,
    const TreeEntry& treeEntry) {
  // Runs per entry on the inode-load path, so only cheap in-memory checks: a
  // proper object id comparison isn't cheap under FilteredFS (it can hit disk).
  return compareTreeEntryType(
             treeEntryTypeFromMode(dirEntry.getInitialMode()),
             treeEntry.getType()) &&
      !aclRootStateRequiresCheckoutWalk(
             dirEntry.aclRootState(), treeEntry.aclRootState());
}

bool canRefreshStaleAclRootState(
    const ObjectStore& objectStore,
    const DirEntry& dirEntry,
    const TreeEntry& treeEntry) {
  if (dirEntry.isRestricted() == treeEntry.isRestricted()) {
    return false;
  }
  if (!compareTreeEntryType(
          treeEntryTypeFromMode(dirEntry.getInitialMode()),
          treeEntry.getType())) {
    return false;
  }
  // The tree is authoritative for the ACL-root bit once it is known to
  // describe the same object; local content it cannot speak for is never
  // tightened.
  if (dirEntry.isMaterialized()) {
    return dirEntry.isRestricted();
  }
  return dirEntry.getObjectIdPtr() != nullptr &&
      objectStore.areObjectsKnownIdentical(
          dirEntry.getObjectId(), treeEntry.getObjectId());
}

// Returns true when a denial was cleared, which is what triggers the overlay
// write-back. A directory whose entries only gained denials is therefore not
// rewritten; when a write does happen it persists the whole DirContents,
// including an entry tightened in the same pass.
bool refreshStaleAclRootStates(
    const ObjectStore& objectStore,
    DirContents& dir,
    const Tree& tree) {
  bool clearedDenial = false;
  for (auto& entry : dir) {
    auto it = tree.find(entry.first);
    if (it == tree.cend()) {
      continue;
    }
    auto& dirEntry = entry.second;
    const auto& treeEntry = it->second;
    if (canRefreshStaleAclRootState(objectStore, dirEntry, treeEntry)) {
      if (dirEntry.isRestricted() && !treeEntry.isRestricted()) {
        clearedDenial = true;
      }
      dirEntry.setAclRootState(makeAclRootState(
          treeEntry.isRestricted(),
          preferKnownAclState(treeEntry.hasACL(), dirEntry.hasACL())));
    }
  }
  return clearedDenial;
}

} // namespace

/**
 * A helper class to track info about inode loads that we started while holding
 * the contents_ lock.
 *
 * Once we release the contents_ lock we need to call
 * registerInodeLoadComplete() for each load we started.  This structure
 * exists to remember the arguments for each call that we need to make.
 */
class TreeInode::IncompleteInodeLoad {
 public:
  IncompleteInodeLoad(
      TreeInode* inode,
      Future<unique_ptr<InodeBase>>&& future,
      PathComponentPiece name,
      InodeNumber number)
      : treeInode_{inode},
        number_{number},
        name_{name},
        future_{std::move(future)} {}

  IncompleteInodeLoad(IncompleteInodeLoad&&) = default;
  IncompleteInodeLoad& operator=(IncompleteInodeLoad&&) = default;

  ~IncompleteInodeLoad() {
    // Ensure that we always call registerInodeLoadComplete().
    //
    // Normally the caller should always explicitly call finish() after they
    // release the TreeInode's contents_ lock.  However if an exception occurs
    // this might not happen, so we call it ourselves.  We want to make sure
    // this happens even on exception code paths, since the InodeMap will
    // otherwise never be notified about the success or failure of this load
    // attempt, and requests for this inode would just be stuck forever.
    if (treeInode_) {
      XLOG(
          WARNING,
          "IncompleteInodeLoad destroyed without explicitly calling finish()");
      finish();
    }
  }

  void finish() {
    // Call treeInode_.release() here before registerInodeLoadComplete() to
    // reset treeInode_ to null.  Setting it to null makes it clear to the
    // destructor that finish() does not need to be called again.
    treeInode_.release()->registerInodeLoadComplete(future_, name_, number_);
  }

 private:
  struct NoopDeleter {
    void operator()(TreeInode*) const {}
  };

  // We store the TreeInode as a unique_ptr just to make sure it gets reset
  // to null in any IncompleteInodeLoad objects that are moved-away from.
  // We don't actually own the TreeInode and we don't destroy it.
  std::unique_ptr<TreeInode, NoopDeleter> treeInode_;
  InodeNumber number_;
  PathComponent name_;
  Future<unique_ptr<InodeBase>> future_;
};

// Returns true when the entry was listed before and is restricted now, the
// only visibility change this path can make.
bool maybeBackfillAclDirEntry(DirEntry& entry, const InodeBase* childInode) {
  auto* childTree = dynamic_cast<const TreeInode*>(childInode);
  if (!childTree) {
    return false;
  }

  // Normal parent metadata propagation happens on the tree-load path. This
  // only backfills stale or missing parent metadata after a child load.
  const bool wasRestricted = entry.isRestricted();
  entry.setAclRootState(makeAclRootState(
      childTree->isRestricted(),
      preferKnownAclState(childTree->hasACL(), entry.hasACL())));
  if (childTree->isRestricted()) {
    XLOGF(
        DBG1,
        "Backfilling restricted tree entry for {} (inode {}, object id {})",
        childTree->getLogPath(),
        childTree->getNodeId(),
        childTree->getObjectId() ? childTree->getObjectId()->toLogString()
                                 : "none");
  }
  return !wasRestricted && entry.isRestricted();
}

std::chrono::steady_clock::time_point initialLastPermissionCheck(
    bool isRestricted,
    TreeInode::InitialPermissionCheck initialPermissionCheck) {
  if (isRestricted &&
      initialPermissionCheck == TreeInode::InitialPermissionCheck::Unknown) {
    return std::chrono::steady_clock::time_point::min();
  }
  return std::chrono::steady_clock::now();
}

static ObjectFetchContext::DirEntryCounts countEntries(const DirContents& dir) {
  ObjectFetchContext::DirEntryCounts counts{};
  for (const auto& entry : dir) {
    if (entry.second.isDirectory()) {
      ++counts.dirs;
    } else {
      ++counts.files;
    }
  }
  return counts;
}

TreeInode::TreeInode(
    InodeNumber ino,
    TreeInodePtr parent,
    PathComponentPiece name,
    mode_t initialMode,
    std::shared_ptr<const Tree>&& tree)
    : TreeInode(
          ino,
          parent,
          name,
          initialMode,
          [&] {
            XCHECK(!tree->isRestricted());
            return saveDirFromTree(ino, tree.get(), parent->getMount());
          }(),
          tree->getObjectId(),
          tree->hasACL()) {}

TreeInode::TreeInode(
    InodeNumber ino,
    TreeInodePtr parent,
    PathComponentPiece name,
    mode_t initialMode,
    TreeDirContents&& dir,
    std::optional<ObjectId> treeId,
    std::optional<bool> hasACL)
    : TreeInode(
          ino,
          std::move(parent),
          name,
          initialMode,
          std::nullopt,
          std::move(dir.contents),
          std::move(treeId),
          /*isRestricted=*/false,
          hasACL,
          InitialPermissionCheck::Unknown,
          dir.entryCounts) {}

TreeInode::TreeInode(
    InodeNumber ino,
    TreeInodePtr parent,
    PathComponentPiece name,
    mode_t initialMode,
    const std::optional<InodeTimestamps>& initialTimestamps,
    DirContents&& dir,
    std::optional<ObjectId> treeId,
    bool isRestricted,
    std::optional<bool> hasACL,
    InitialPermissionCheck initialPermissionCheck,
    std::optional<ObjectFetchContext::DirEntryCounts> entryCounts)
    : Base(ino, initialMode, initialTimestamps, parent, name),
      loadTimeEntryCounts_(entryCounts ? *entryCounts : countEntries(dir)),
      contents_(std::in_place, std::move(dir), std::move(treeId)),
      aclRootState_{
          static_cast<uint8_t>(makeAclRootState(isRestricted, hasACL))},
      lastPermissionCheck_(
          initialLastPermissionCheck(isRestricted, initialPermissionCheck)) {
  XDCHECK_NE(ino, kRootNodeId);
  assertRestrictedPlaceholderInvariant();
  witnessLoadedSize();
}

TreeInode::TreeInode(EdenMount* mount, std::shared_ptr<const Tree>&& tree)
    : TreeInode(
          mount,
          saveDirFromTree(kRootNodeId, tree.get(), mount),
          tree->getObjectId(),
          tree->isRestricted(),
          tree->hasACL()) {}

TreeInode::TreeInode(
    EdenMount* mount,
    TreeDirContents&& dir,
    const std::optional<ObjectId>& treeId,
    bool isRestricted,
    std::optional<bool> hasACL)
    : TreeInode(
          mount,
          std::move(dir.contents),
          treeId,
          isRestricted,
          hasACL,
          InitialPermissionCheck::Unknown,
          dir.entryCounts) {}

TreeInode::TreeInode(
    EdenMount* mount,
    DirContents&& dir,
    const std::optional<ObjectId>& treeId,
    bool isRestricted,
    std::optional<bool> hasACL,
    InitialPermissionCheck initialPermissionCheck,
    std::optional<ObjectFetchContext::DirEntryCounts> entryCounts)
    : Base(mount),
      loadTimeEntryCounts_(entryCounts ? *entryCounts : countEntries(dir)),
      contents_(std::in_place, std::move(dir), treeId),
      aclRootState_{
          static_cast<uint8_t>(makeAclRootState(isRestricted, hasACL))},
      lastPermissionCheck_(
          initialLastPermissionCheck(isRestricted, initialPermissionCheck)) {
  assertRestrictedPlaceholderInvariant();
  witnessLoadedSize();
}

TreeInode::~TreeInode() = default;

void TreeInode::witnessLoadedSize() {
  auto* walkDetector = getMount()->getWalkDetector();
  if (!walkDetector) {
    return;
  }
  if (auto path = getPath()) {
    walkDetector->witnessDirectorySize(*path, loadTimeEntryCounts_);
  }
}

struct stat TreeInode::statWithCurrentRestrictionState() const {
  if (FOLLY_UNLIKELY(isRestricted())) {
    struct stat st{};
    st.st_ino = folly::to_narrow(getNodeId().get());
    st.st_mode = S_IFDIR; // directory with zero permission bits
    st.st_nlink = 2; // . and ..
    return st;
  }

  auto st = getMount()->initStatData();
  st.st_ino = folly::to_narrow(getNodeId().get());
  auto contents = contents_.rlock();

#ifndef _WIN32
  auto metadata = getMetadataLocked(contents->entries);
  metadata.applyToStat(st);
  if (UNLIKELY(S_ISREG(st.st_mode))) {
    // TODO(T159626416): Log the path of the tree that is being misinterpreted
    // as a regular file. We should only do this if these events are infrequent.
    getMount()->getServerState()->getEdenFsEventsLogger()->logEvent(
        InodeMetadataMismatch{
            st.st_mode,
            metadata.gid,
            metadata.uid,
            metadata.timestamps.atime.asRawRepresentation(),
            metadata.timestamps.ctime.asRawRepresentation(),
            metadata.timestamps.mtime.asRawRepresentation(),
        });
  }
#endif

  // For directories, nlink is the number of entries including the
  // "." and ".." links.
#ifndef _WIN32
  st.st_nlink = contents->entries.size() + 2;
#else
  st.st_nlink =
      folly::to_narrow(folly::to_signed(contents->entries.size() + 2));
#endif

  return st;
}

void TreeInode::throwRestrictedAccess() const {
  folly::throwSystemErrorExplicit(
      EACCES,
      fmt::format(
          "path ACL restriction: directory access denied for {} (inode {})",
          getLogPath(),
          getNodeId()));
}

RestrictedContentMode TreeInode::restrictedContentMode() const {
  return getObjectStore().getRestrictedContentMode();
}

// Parent listings need invalidating only where entries are hidden: FUSE and
// NFS in omitted mode. PrjFS enumerates the raw Tree and never hides any.
bool TreeInode::hidesRestrictedEntries() const {
#ifdef _WIN32
  return false;
#else
  return restrictedContentMode() == RestrictedContentMode::Omitted;
#endif
}

void TreeInode::assertRestrictedPlaceholderInvariant() const {
  if (!isRestricted()) {
    return;
  }

  // TODO: Make it impossible for a restricted TreeInode to enter this invalid
  // state by representing restricted placeholders separately from populated
  // contents, instead of validating after construction or state transitions.
  auto contents = getContentsUnchecked().rlock();
  XDCHECK(contents->entries.empty())
      << "restricted TreeInode must not expose child entries: "
      << getNodeId().get();
}

ImmediateFuture<folly::Unit> TreeInode::recheckPermissionIfExpired(
    const ObjectFetchContextPtr& fetchContext) {
  if (!isRestricted()) {
    return folly::unit;
  }

  auto treeId = getContentsUnchecked().rlock()->treeId;
  if (!treeId) {
    return folly::unit;
  }

  auto ttl = std::chrono::seconds{
      getObjectStore().getEdenConfig()->restrictedTreeTtlSeconds.getValue()};
  auto lastCheck = lastPermissionCheck_.load(std::memory_order_relaxed);
  while (true) {
    auto now = std::chrono::steady_clock::now();
    if (lastCheck != std::chrono::steady_clock::time_point::min() &&
        now - lastCheck < ttl) {
      return folly::unit;
    }
    if (lastPermissionCheck_.compare_exchange_weak(
            lastCheck, now, std::memory_order_relaxed)) {
      break;
    }
    if (!isRestricted()) {
      return folly::unit;
    }
  }

  auto expiredCheck =
      std::chrono::steady_clock::now() - ttl - std::chrono::nanoseconds{1};
  return getObjectStore()
      .checkPermissionIfExpired(*treeId, expiredCheck)
      .thenTry(
          [self = inodePtrFromThis(), fetchContext = fetchContext.copy()](
              folly::Try<bool> result) mutable -> ImmediateFuture<folly::Unit> {
            if (result.hasException()) {
              XLOGF(
                  WARN,
                  "check_permission failed for restricted inode {} ({}): {}",
                  self->getNodeId(),
                  self->getLogPath(),
                  folly::exceptionStr(result.exception()));
              return folly::unit;
            }
            if (!result.value()) {
              return folly::unit;
            }
            return self->transitionToUnrestricted(fetchContext);
          });
}

void TreeInode::recheckHiddenRestrictedChildren(
    const std::vector<PathComponent>& names,
    const ObjectFetchContextPtr& context) {
  for (const auto& name : names) {
    folly::futures::detachOn(
        getMount()->getServerThreadPool().get(),
        folly::makeSemiFuture()
            .deferValue([self = inodePtrFromThis(),
                         name,
                         context = context.copy()](folly::Unit) {
              return self->getOrLoadChildTree(name, context)
                  .thenValue([context = context.copy()](TreeInodePtr child) {
                    return child->recheckPermissionIfExpired(context);
                  })
                  .semi();
            })
            .deferError([self = inodePtrFromThis(),
                         name](folly::exception_wrapper&& ew) {
              XLOGF(
                  DBG3,
                  "skipping permission recheck for {}/{}: {}",
                  self->getLogPath(),
                  name,
                  folly::exceptionStr(ew));
            }));
  }
}

ImmediateFuture<folly::Unit> TreeInode::transitionToUnrestricted(
    const ObjectFetchContextPtr& fetchContext) {
  auto treeId = getContentsUnchecked().rlock()->treeId;
  if (!treeId) {
    return folly::unit;
  }

  return getObjectStore()
      .getTree(*treeId, fetchContext)
      .thenValue([self = inodePtrFromThis(),
                  savedTreeId = *treeId](std::shared_ptr<const Tree> tree) {
        auto newContentsResult = self->buildUnrestrictedDirContents(
            self->getNodeId(), *tree, [&] { return self->getLogPath(); });

        auto renameLock = self->getMount()->acquireRenameLock();

        {
          auto contents = self->getContentsUnchecked().wlock();
          if (!self->isRestricted()) {
            return; // already transitioned by concurrent recheck
          }
          if (contents->treeId != savedTreeId) {
            return; // treeId changed (checkout raced), stale fetch
          }
          // TreeInodeState::isMaterialized() is !treeId.has_value(); matching
          // savedTreeId means this directory is still unmaterialized.
          XDCHECK(contents->treeId.has_value());
          XDCHECK(!contents->isMaterialized());
          contents->entries = std::move(newContentsResult.contents);
          contents->treeId = tree->getObjectId();
          if (newContentsResult.refreshedStaleDeniedAclRootStates) {
            try {
              self->saveOverlayDir(
                  self->getNodeId(),
                  contents->entries,
                  /*isMaterialized=*/false);
            } catch (const std::exception& ex) {
              XLOGF(
                  WARN,
                  "failed to persist refreshed unrestricted overlay contents for inode {}: {}",
                  self->getNodeId(),
                  folly::exceptionStr(ex));
            }
          }
        }

        XLOGF(
            DBG1,
            "Transitioning restricted tree to unrestricted for {} (inode {}, object id {})",
            self->getLogPath(),
            self->getNodeId(),
            tree->getObjectId().toLogString());
        self->setAclRootState(makeAclRootState(
            /*isRestricted=*/false,
            preferKnownAclState(tree->hasACL(), self->hasACL())));

        auto loc = self->getLocationInfo(renameLock);
        if (loc.parent && !loc.unlinked) {
          auto parentContents = loc.parent->getContentsUnchecked().wlock();
          auto it = parentContents->entries.find(loc.name);
          if (it != parentContents->entries.end()) {
            it->second.setAclRootState(makeAclRootState(
                /*isRestricted=*/false,
                preferKnownAclState(self->hasACL(), it->second.hasACL())));
            // The parent DirEntry carries the persisted restricted bit used to
            // reload this child after restart. Self-healing the child inode is
            // not enough; rewrite the parent overlay entry so the old
            // restriction does not come back on the next mount.
            try {
              loc.parent->saveOverlayDir(
                  parentContents->entries, parentContents->isMaterialized());
            } catch (const std::exception& ex) {
              XLOGF(
                  WARN,
                  "failed to persist unrestricted parent overlay entry for {} (inode {}): {}",
                  loc.name,
                  self->getNodeId(),
                  folly::exceptionStr(ex));
            }
            if (self->hidesRestrictedEntries()) {
              // Omitted mode hid this directory from the parent's listing;
              // drop the kernel's cached listing so it shows up now.
              loc.parent->invalidateChannelDirCache(*parentContents).get();
            }
          }
        }

        {
          auto contents = self->getContentsUnchecked().wlock();
          self->invalidateChannelDirCache(*contents).get();
        }
      });
}

ImmediateFuture<struct stat> TreeInode::stat(
    const ObjectFetchContextPtr& context) {
  logAccess(*context);

  if (FOLLY_UNLIKELY(isRestricted())) {
    return recheckPermissionIfExpired(context).thenValue(
        [self = inodePtrFromThis()](folly::Unit) {
          return self->statWithCurrentRestrictionState();
        });
  }
  return statWithCurrentRestrictionState();
}

folly::coro::now_task<struct stat> TreeInode::co_stat(
    const ObjectFetchContextPtr& context) {
  // Hold an InodePtr to ourselves across the co_await for symmetry with
  // TreeInode::stat()'s [self = inodePtrFromThis()] capture and to keep
  // this TreeInode alive while recheckPermissionIfExpired runs.
  auto self = inodePtrFromThis();
  logAccess(*context);

  if (FOLLY_UNLIKELY(isRestricted())) {
    co_await recheckPermissionIfExpired(context).semi();
  }
  co_return statWithCurrentRestrictionState();
}

std::vector<PathComponent> TreeInode::getChildNames() const {
  auto contents = lockContentsRead();
  std::vector<PathComponent> names;
  names.reserve(contents->entries.size());
  for (const auto& entry : contents->entries.all()) {
    names.emplace_back(entry.first);
  }
  return names;
}

TreeInode::TraversalSnapshot TreeInode::getTraversalSnapshot() const {
  auto contents = lockContentsRead();
  return TraversalSnapshot{
      parseDirContents(contents->entries), contents->treeId};
}

std::optional<ImmediateFuture<VirtualInode>> TreeInode::rlockGetOrFindChild(
    const TreeInodeState& contents,
    PathComponentPiece name,
    const ObjectFetchContextPtr& context,
    bool loadInodes) {
  // Check if the child is already loaded and return it if so
  auto iter = contents.entries.find(name);
  if (iter == contents.entries.end()) {
    XLOGF(
        DBG7,
        "attempted to load non-existent entry \"{}\" in {}",
        name,
        getLogPath());
    return std::make_optional(
        ImmediateFuture<VirtualInode>{folly::Try<VirtualInode>{
            InodeError(ENOENT, inodePtrFromThis(), name)}});
  }

  // Check to see if the entry is already loaded
  auto& entry = iter->second;
  if (auto inodePtr = entry.getInodePtr()) {
    logAccess(*context);
    return VirtualInode{std::move(inodePtr)};
  }

  // The node is not loaded. If the caller requires that we load
  // Inodes, or the entry is materialized, go on and load the inode
  // by returning std::nullopt here.
  if (loadInodes || entry.isMaterialized()) {
    return std::nullopt;
  }

  logAccess(*context);
  // Note that a child's inode may be currently loading. If it's
  // currently being loaded there's no chance it's been
  // modified/materialized yet (it has to have been loaded prior),
  // so it's safe here to ignore the loading inode and instead
  // query the object store for information about the path.
  if (entry.isDirectory()) {
    if (entry.isRestricted()) {
      return VirtualInode::makeRestricted(
          entry.getObjectId(),
          entry.getInitialMode(),
          contents.entries.getCaseSensitivity());
    }
    // This is a directory, always get the tree corresponding to
    // the id
    return getObjectStore()
        .getTree(entry.getObjectId(), context)
        .thenValue([mode = entry.getInitialMode(), hasACL = entry.hasACL()](
                       std::shared_ptr<const Tree>&& tree) {
          auto virtualInode = VirtualInode(std::move(tree), mode);
          virtualInode.setHasACL(hasACL);
          return virtualInode;
        });
  }
  // This is a file, return the DirEntry if this was the last
  // path component. Note that because the entry is not loaded and
  // is not materialized, it's guaranteed to have a id set, and
  // the constructor of UnmaterializedUnloadedBlobDirEntry can be
  // called safely.
  return VirtualInode{UnmaterializedUnloadedBlobDirEntry(entry)};
}

std::pair<folly::SemiFuture<InodePtr>, TreeInode::LoadChildCleanUp>
TreeInode::loadChild(
    folly::Synchronized<TreeInodeState>::LockedPtr& contents,
    PathComponentPiece name,
    const ObjectFetchContextPtr& context) {
  logAccess(*context);
  auto inodeLoadFuture = Future<unique_ptr<InodeBase>>::makeEmpty();
  InodePtr childInodePtr;
  InodeMap::PromiseVector promises;

  // The entry is not loaded yet.  Ask the InodeMap about the
  // entry. The InodeMap will tell us if this inode is already in
  // the process of being loaded, or if we need to start loading it
  // now.
  auto iter = contents->entries.find(name);
  auto inodeName = copyCanonicalInodeName(iter);
  name = inodeName.piece();
  auto& entry = iter->second;
  folly::Promise<InodePtr> promise;
  auto returnFuture = promise.getSemiFuture();
  bool startLoad = startLoadingChild(name, entry, promise);
  auto childNumber = entry.getInodeNumber();
  if (startLoad) {
    // The inode is not already being loaded.  We have to start
    // loading it now.
    auto loadFuture = startLoadingInodeNoThrow(entry, name, context, false);
    if (loadFuture.isReady() && loadFuture.hasValue()) {
      // If we finished loading the inode immediately, just call
      // InodeMap::inodeLoadComplete() now, since we still have the
      // data_ lock.
      auto childInode = std::move(loadFuture).get();
      auto* childInodeRaw = CHECK_NOTNULL(childInode.get());
      if (maybeBackfillAclDirEntry(entry, childInodeRaw) &&
          hidesRestrictedEntries()) {
        // Omitted mode now hides this entry; drop our cached listing.
        invalidateChannelDirCache(*contents).get();
      }
      entry.setInode(childInodeRaw);
      promises = getInodeMap()->inodeLoadComplete(childInodeRaw);
      childInodePtr = InodePtr::takeOwnership(std::move(childInode));
    } else {
      inodeLoadFuture = std::move(loadFuture);
    }
  }
  return std::make_pair(
      std::move(returnFuture),
      LoadChildCleanUp{
          std::move(inodeLoadFuture),
          std::move(promises),
          std::move(childNumber),
          std::move(childInodePtr),
      });
}

void TreeInode::loadChildCleanUp(
    PathComponentPiece name,
    TreeInode::LoadChildCleanUp result) {
  if (result.inodeLoadFuture.valid()) {
    registerInodeLoadComplete(result.inodeLoadFuture, name, result.childNumber);
  } else {
    for (auto& promise : result.promises) {
      promise.setValue(result.childInodePtr);
    }
  }
}

ImmediateFuture<InodePtr> TreeInode::loadChildAndCleanUp(
    folly::Synchronized<TreeInodeState>::LockedPtr& contents,
    PathComponentPiece name,
    const ObjectFetchContextPtr& context) {
  auto result = loadChild(contents, name, context);
  // it's important the code between loadChild and loadChildCleanUp is no
  // throw. We need to perform the loadChildCleanUp now regardless of
  // exception.
  contents.unlock();
  loadChildCleanUp(name, std::move(result.second));
  return ImmediateFuture<InodePtr>{std::move(result.first)};
}

ImmediateFuture<VirtualInode> TreeInode::getOrFindChild(
    PathComponentPiece name,
    const ObjectFetchContextPtr& context,
    bool loadInodes) {
  TraceBlock block("getOrFindChild");

  auto doGetOrFindChild = [self = inodePtrFromThis(),
                           name = PathComponent{name},
                           context = context.copy(),
                           loadInodes,
                           block = std::move(block)](folly::Unit) mutable {
    // Explicit ACL check required: uses raw contents_ access via
    // tryRlockCheckBeforeUpdate, bypassing guarded lock accessors.
    // Placed inside the lambda so that recheckPermissionIfExpired has a
    // chance to clear the restriction before we throw EACCES.
    self->checkAccess();
#ifndef _WIN32
    if (name == kDotEdenName && self->getNodeId() != kRootNodeId) {
      return self->getMount()
          ->getInodeSlow(".eden/this-dir"_relpath, context)
          .thenValue(
              [](auto&& inode) { return VirtualInode{std::move(inode)}; });
    }
#endif // !_WIN32
    return tryRlockCheckBeforeUpdate<ImmediateFuture<VirtualInode>>(
               self->contents_,
               [&](const auto& contents)
                   -> std::optional<ImmediateFuture<VirtualInode>> {
                 return self->rlockGetOrFindChild(
                     contents, name, context, loadInodes);
               },
               [&](auto& contents) -> ImmediateFuture<VirtualInode> {
                 return self->loadChildAndCleanUp(contents, name, context)
                     .thenValue([](auto&& inode) {
                       return VirtualInode{std::move(inode)};
                     });
               })
        .ensure([b = std::move(block)]() mutable { b.close(); });
  };

  if (FOLLY_UNLIKELY(isRestricted())) {
    return recheckPermissionIfExpired(context).thenValue(
        std::move(doGetOrFindChild));
  }
  return doGetOrFindChild(folly::unit);
}

std::optional<VirtualInode> TreeInode::rlockCheckChild(
    const TreeInodeState& contents,
    PathComponentPiece name,
    const ObjectFetchContextPtr& context,
    bool loadInodes,
    std::optional<TreeInode::PendingDirFetch>& dirFetch) {
  auto iter = contents.entries.find(name);
  if (iter == contents.entries.end()) {
    XLOGF(
        DBG7,
        "attempted to load non-existent entry \"{}\" in {}",
        name,
        getLogPath());
    throw InodeError(ENOENT, inodePtrFromThis(), name);
  }

  auto& entry = iter->second;
  if (auto inodePtr = entry.getInodePtr()) {
    logAccess(*context);
    return VirtualInode{std::move(inodePtr)};
  }

  if (loadInodes || entry.isMaterialized()) {
    return std::nullopt;
  }

  logAccess(*context);
  if (entry.isDirectory()) {
    if (entry.isRestricted()) {
      return VirtualInode::makeRestricted(
          entry.getObjectId(),
          entry.getInitialMode(),
          contents.entries.getCaseSensitivity());
    }
    dirFetch = PendingDirFetch{
        entry.getObjectId(), entry.getInitialMode(), entry.hasACL()};
    return std::nullopt;
  }
  return VirtualInode{UnmaterializedUnloadedBlobDirEntry(entry)};
}

folly::coro::now_task<VirtualInode> TreeInode::co_getOrFindChild(
    PathComponentPiece name,
    const ObjectFetchContextPtr& context,
    bool loadInodes) {
  TraceBlock block("co_getOrFindChild");

  // If restricted, recheck permission before throwing EACCES — the restriction
  // may have expired.
  if (FOLLY_UNLIKELY(isRestricted())) {
    auto recheck = recheckPermissionIfExpired(context);
    if (recheck.isReady()) {
      std::move(recheck).getTry().throwUnlessValue();
    } else {
      co_await std::move(recheck).semi();
    }
  }
  // Explicit ACL check required: uses raw contents_.rlock() / contents_.wlock()
  // directly, bypassing guarded lock accessors.
  checkAccess();

#ifndef _WIN32
  if (name == kDotEdenName && getNodeId() != kRootNodeId) {
    auto inode =
        co_await getMount()->co_getInodeSlow(".eden/this-dir"_relpath, context);
    co_return VirtualInode{std::move(inode)};
  }
#endif // !_WIN32

  std::optional<PendingDirFetch> dirFetch;
  folly::SemiFuture<InodePtr> loadFuture =
      folly::SemiFuture<InodePtr>::makeEmpty();

  // First, acquire the rlock. If the check succeeds, acquiring a wlock is
  // unnecessary.
  {
    auto contents = contents_.rlock();
    auto result =
        rlockCheckChild(*contents, name, context, loadInodes, dirFetch);
    if (result.has_value()) {
      co_return std::move(result.value());
    }
  }

  if (!dirFetch.has_value()) {
    auto contents = contents_.wlock();
    // Check again - something may have raced between the locks.
    auto result =
        rlockCheckChild(*contents, name, context, loadInodes, dirFetch);
    if (result.has_value()) {
      co_return std::move(result.value());
    }

    if (!dirFetch.has_value()) {
      auto loadResult = loadChild(contents, name, context);
      // it's important the code between loadChild and loadChildCleanUp
      // is no throw. We need to perform the loadChildCleanUp now
      // regardless of exception.
      loadFuture = std::move(loadResult.first);
      contents.unlock();
      loadChildCleanUp(name, std::move(loadResult.second));
    }
  }

  // All locks released before suspension
  if (dirFetch.has_value()) {
    auto tree = co_await getObjectStore().co_getTree(dirFetch->treeId, context);
    auto virtualInode = VirtualInode(std::move(tree), dirFetch->mode);
    virtualInode.setHasACL(dirFetch->hasACL);
    co_return virtualInode;
  }

  auto inode = co_await std::move(loadFuture);
  co_return VirtualInode{std::move(inode)};
}

folly::coro::now_task<
    std::vector<std::pair<PathComponent, folly::Try<VirtualInode>>>>
TreeInode::getChildren(const ObjectFetchContextPtr& context, bool loadInodes) {
  auto self = inodePtrFromThis();

  {
    auto recheck = recheckPermissionIfExpired(context);
    if (recheck.isReady()) {
      std::move(recheck).getTry().throwUnlessValue();
    } else {
      co_await std::move(recheck).semi();
    }
  }

  std::vector<std::pair<PathComponent, folly::Try<VirtualInode>>> result;
  std::vector<folly::coro::Task<VirtualInode>> tasks;
  std::vector<size_t> taskIdx;
  std::vector<std::pair<PathComponent, TreeInode::LoadChildCleanUp>>
      inodeLoadCleanUps;

  auto store = getMount()->getObjectStore();

  {
    // SCOPE_EXIT must precede lockContentsWrite() so cleanups drain after
    // the lock is released, even on exception.
    SCOPE_EXIT {
      for (auto& cleanUp : inodeLoadCleanUps) {
        loadChildCleanUp(cleanUp.first, std::move(cleanUp.second));
      }
    };
    auto contents = lockContentsWrite();
    result.reserve(contents->entries.size());
    tasks.reserve(contents->entries.size());
    taskIdx.reserve(contents->entries.size());
    inodeLoadCleanUps.reserve(contents->entries.size());

    for (const auto& [name, _entry] :
         contents->entries.visible(restrictedContentMode())) {
      std::optional<PendingDirFetch> dirFetch;
      auto sync =
          rlockCheckChild(*contents, name, context, loadInodes, dirFetch);
      if (sync.has_value()) {
        // Already loaded, restricted, or otherwise synchronously representable.
        result.emplace_back(name, folly::Try<VirtualInode>{std::move(*sync)});
        continue;
      }

      taskIdx.push_back(result.size());
      result.emplace_back(
          name, folly::Try<VirtualInode>{folly::FutureNotReady{}});

      if (dirFetch.has_value()) {
        // Unloaded non-restricted directory: fetch its tree after the lock.
        // The tree id and mode were snapshotted from contents, so this
        // preserves getChildren()'s point-in-time view without holding the
        // contents lock while the object store work runs.
        tasks.emplace_back(
            folly::coro::co_invoke(
                [](std::shared_ptr<ObjectStore> s,
                   PendingDirFetch fetch,
                   ObjectFetchContextPtr ctx)
                    -> folly::coro::Task<VirtualInode> {
                  co_await folly::coro::co_reschedule_on_current_executor;
                  auto tree = co_await s->co_getTree(fetch.treeId, ctx);
                  auto virtualInode = VirtualInode{std::move(tree), fetch.mode};
                  virtualInode.setHasACL(fetch.hasACL);
                  co_return virtualInode;
                },
                store,
                *dirFetch,
                context.copy()));
      } else {
        // Materialized entry or loadInodes=true: load the child inode.
        // loadChild must run under the contents lock because it coordinates
        // with the inode map and updates entry load state. Only the returned
        // future is awaited after the cleanup runs outside the lock.
        auto childResult = loadChild(contents, name, context);
        // emplace_back must be no-throw so cleanup is guaranteed to run if
        // the tasks.emplace_back below throws.
        XCHECK_LT(inodeLoadCleanUps.size(), inodeLoadCleanUps.capacity());
        inodeLoadCleanUps.emplace_back(name, std::move(childResult.second));
        tasks.emplace_back(
            folly::coro::co_invoke(
                [](folly::SemiFuture<InodePtr> loadFut)
                    -> folly::coro::Task<VirtualInode> {
                  auto inode = co_await std::move(loadFut);
                  co_return VirtualInode{std::move(inode)};
                },
                std::move(childResult.first)));
      }
    }
  }

  if (!tasks.empty()) {
    auto tries = co_await folly::coro::collectAllTryRange(std::move(tasks));
    XDCHECK_EQ(tries.size(), taskIdx.size());
    for (size_t i = 0; i < tries.size(); ++i) {
      result[taskIdx[i]].second = std::move(tries[i]);
    }
  }
  co_return result;
}

folly::coro::now_task<
    std::vector<std::pair<PathComponent, folly::Try<EntryAttributes>>>>
TreeInode::co_getChildrenAttributes(
    EntryAttributeFlags requestedAttributes,
    RelativePath path,
    const std::shared_ptr<ObjectStore>& objectStore,
    timespec lastCheckoutTime,
    const ObjectFetchContextPtr& context,
    std::optional<bool> ancestorUnderAcl) {
  auto self = inodePtrFromThis();

  if (FOLLY_UNLIKELY(isRestricted())) {
    co_await recheckPermissionIfExpired(context).semi();
  }

  // Atomic snapshot under one wlock, same discipline as getChildren():
  // SCOPE_EXIT drains inodeLoadCleanUps after the lock is released; per-child
  // attribute tasks run in parallel via collectAllTryRange post-lock.
  std::vector<PathComponent> names;
  std::vector<folly::coro::Task<EntryAttributes>> tasks;
  std::vector<std::pair<PathComponent, TreeInode::LoadChildCleanUp>>
      inodeLoadCleanUps;

  {
    SCOPE_EXIT {
      for (auto& cleanUp : inodeLoadCleanUps) {
        loadChildCleanUp(cleanUp.first, std::move(cleanUp.second));
      }
    };
    auto contents = lockContentsWrite();
    names.reserve(contents->entries.size());
    tasks.reserve(contents->entries.size());
    inodeLoadCleanUps.reserve(contents->entries.size());
    auto adjusted = adjustRootAclState(
        getNodeId() == kRootNodeId, ancestorUnderAcl, hasACL());
    auto thisUnderAcl =
        mergeAncestorAclState(adjusted.ancestorUnderAcl, adjusted.hasACL);

    for (const auto& [name, _entry] :
         contents->entries.visible(restrictedContentMode())) {
      auto subPath = path + name;
      std::optional<PendingDirFetch> dirFetch;
      auto sync = rlockCheckChild(
          *contents, name, context, /*loadInodes=*/false, dirFetch);
      names.push_back(name);
      if (sync.has_value()) {
        tasks.emplace_back(coFetchEntryAttributesFromVI(
            std::move(*sync),
            thisUnderAcl,
            requestedAttributes,
            std::move(subPath),
            objectStore,
            lastCheckoutTime,
            context.copy()));
      } else if (dirFetch.has_value()) {
        tasks.emplace_back(coFetchTreeEntryAttributes(
            dirFetch->treeId,
            dirFetch->mode,
            dirFetch->hasACL,
            thisUnderAcl,
            requestedAttributes,
            std::move(subPath),
            objectStore,
            lastCheckoutTime,
            context.copy()));
      } else {
        auto childResult = loadChild(contents, name, context);
        XCHECK_LT(inodeLoadCleanUps.size(), inodeLoadCleanUps.capacity());
        inodeLoadCleanUps.emplace_back(name, std::move(childResult.second));
        tasks.emplace_back(coFetchLoadedInodeEntryAttributes(
            std::move(childResult.first),
            thisUnderAcl,
            requestedAttributes,
            std::move(subPath),
            objectStore,
            lastCheckoutTime,
            context.copy()));
      }
    }
  }

  auto tries = co_await folly::coro::collectAllTryRange(std::move(tasks));

  XCHECK_EQ(tries.size(), names.size());
  std::vector<std::pair<PathComponent, folly::Try<EntryAttributes>>> result;
  result.reserve(tries.size());
  for (size_t i = 0; i < tries.size(); ++i) {
    result.emplace_back(std::move(names.at(i)), std::move(tries[i]));
  }
  co_return result;
}

ImmediateFuture<InodePtr> TreeInode::getOrLoadChild(
    PathComponentPiece name,
    const ObjectFetchContextPtr& context) {
  // No co_invoke bridge: getOrLoadChild is called from FUSE/NFS handlers
  // without a coroutine executor.
  return getOrFindChild(name, context, true).thenValue([](auto&& virtualInode) {
    return virtualInode.asInodePtr();
  });
}

ImmediateFuture<InodePtr> TreeInode::getOrLoadChildIfExists(
    PathComponentPiece name,
    const ObjectFetchContextPtr& context) {
  bool dotEden = false;
#ifndef _WIN32
  dotEden = name == kDotEdenName && getNodeId() != kRootNodeId;
#endif
  if (FOLLY_UNLIKELY(dotEden || isRestricted())) {
    // Both have their own handling in getOrFindChild(), and neither is a
    // path where misses are common.
    return getOrLoadChild(name, context);
  }
  checkAccess();
  return tryRlockCheckBeforeUpdate<ImmediateFuture<InodePtr>>(
      contents_,
      [&](const auto& contents) -> std::optional<ImmediateFuture<InodePtr>> {
        auto iter = contents.entries.find(name);
        if (iter == contents.entries.end()) {
          return ImmediateFuture<InodePtr>{InodePtr{}};
        }
        if (auto inode = iter->second.getInodePtr()) {
          logAccess(*context);
          return ImmediateFuture<InodePtr>{std::move(inode)};
        }
        return std::nullopt;
      },
      [&](auto& contents) {
        return loadChildAndCleanUp(contents, name, context);
      });
}

folly::coro::now_task<InodePtr> TreeInode::co_getOrLoadChild(
    PathComponentPiece name,
    const ObjectFetchContextPtr& context) {
  auto virtualInode = co_await co_getOrFindChild(name, context, true);
  co_return virtualInode.asInodePtr();
}

ImmediateFuture<TreeInodePtr> TreeInode::getOrLoadChildTree(
    PathComponentPiece name,
    const ObjectFetchContextPtr& context) {
  // No co_invoke bridge: getOrLoadChildTree is called from FUSE/NFS handlers
  // without a coroutine executor.
  return getOrLoadChild(name, context).thenValue([](InodePtr child) {
    auto treeInode = child.asTreePtrOrNull();
    if (!treeInode) {
      return ImmediateFuture<TreeInodePtr>{
          folly::Try<TreeInodePtr>{InodeError(ENOTDIR, child)}};
    }
    return ImmediateFuture<TreeInodePtr>{std::move(treeInode)};
  });
}

folly::coro::now_task<TreeInodePtr> TreeInode::co_getOrLoadChildTree(
    PathComponentPiece name,
    const ObjectFetchContextPtr& context) {
  auto child = co_await co_getOrLoadChild(name, context);
  auto treeInode = child.asTreePtrOrNull();
  if (!treeInode) {
    throw InodeError(ENOTDIR, child);
  }
  co_return std::move(treeInode);
}

namespace {
/**
 * A helper class for performing a recursive path lookup.
 *
 * If needed we could probably optimize this more in the future.  As-is we are
 * likely performing a lot of avoidable memory allocations to bind and set
 * Future callbacks at each stage.  This should be possible to implement with
 * only a single allocation up front (but we might not be able to achieve that
 * using the Futures API, we might have to create more custom callback API).
 */
class LookupProcessor {
 public:
  explicit LookupProcessor(
      RelativePathPiece path,
      ObjectFetchContextPtr context)
      : path_{path},
        iterRange_{path_.components()},
        iter_{iterRange_.begin()},
        context_{std::move(context)} {}

  ImmediateFuture<InodePtr> next(TreeInodePtr tree) {
    auto name = *iter_++;
    if (iter_ == iterRange_.end()) {
      return tree->getOrLoadChild(name, context_);
    } else {
      return tree->getOrLoadChildTree(name, context_)
          .thenValue(
              [this](TreeInodePtr tree) { return next(std::move(tree)); });
    }
  }

  folly::coro::now_task<InodePtr> co_next(TreeInodePtr tree) {
    auto name = *iter_++;
    if (iter_ == iterRange_.end()) {
      co_return co_await tree->co_getOrLoadChild(name, context_);
    } else {
      auto childTree = co_await tree->co_getOrLoadChildTree(name, context_);
      co_return co_await co_next(std::move(childTree));
    }
  }

 private:
  RelativePath path_;
  RelativePath::base_type::component_iterator_range iterRange_;
  RelativePath::base_type::component_iterator iter_;
  ObjectFetchContextPtr context_;
};
} // namespace

folly::coro::now_task<InodePtr> TreeInode::co_getChildRecursive(
    RelativePathPiece path,
    const ObjectFetchContextPtr& context) {
  auto pathStr = path.view();
  if (pathStr.empty()) {
    co_return inodePtrFromThis();
  }

  auto processor = std::make_unique<LookupProcessor>(path, context.copy());
  co_return co_await processor->co_next(inodePtrFromThis());
}

InodeNumber TreeInode::getChildInodeNumber(PathComponentPiece name) {
  auto contents = lockContentsRead();
  auto iter = contents->entries.find(name);
  if (iter == contents->entries.end()) {
    throw InodeError(ENOENT, inodePtrFromThis(), name);
  }

  auto& ent = iter->second;
  XDCHECK(
      !ent.getInode() || ent.getInode()->getNodeId() == ent.getInodeNumber())
      << fmt::format(
             "inode number mismatch: {} != {}",
             ent.getInode()->getNodeId(),
             ent.getInodeNumber());
  return ent.getInodeNumber();
}

void TreeInode::loadUnlinkedChildInode(
    PathComponentPiece name,
    InodeNumber number,
    std::optional<ObjectId> id,
    mode_t mode) {
  try {
    InodeMap::PromiseVector promises;
    InodePtr inodePtr;

    if (!S_ISDIR(mode)) {
      auto file = std::make_unique<FileInode>(
          number,
          inodePtrFromThis(),
          name,
          mode,
          std::nullopt,
          id ? &*id : nullptr);
      // Take ownership before registering the inode, so ptrAcquireCount_ is
      // already incremented when the inode becomes visible in loadedInodes_.
      inodePtr = InodePtr::takeOwnership(std::move(file));
      promises = getInodeMap()->inodeLoadComplete(inodePtr.get());
    } else {
      auto overlayContents = getOverlay()->loadOverlayDir(number);
      if (!id) {
        // If the inode is materialized, the overlay must have an entry
        // for the directory.
        // Note that the .value() call will throw if we couldn't
        // load the dir data; we'll catch and propagate that in
        // the containing try/catch block.
        if (!overlayContents.empty()) {
          // Should be impossible, but worth checking for
          // defensive purposes!
          throw std::runtime_error(
              "unlinked dir inode should have no children");
        }
      }

      auto tree = std::make_unique<TreeInode>(
          number,
          inodePtrFromThis(),
          name,
          mode,
          std::nullopt,
          std::move(overlayContents),
          id ? std::optional<ObjectId>{*id} : std::nullopt);
      // Take ownership before registering the inode, so ptrAcquireCount_ is
      // already incremented when the inode becomes visible in loadedInodes_.
      inodePtr = InodePtr::takeOwnership(std::move(tree));
      promises = getInodeMap()->inodeLoadComplete(inodePtr.get());
    }

    inodePtr->markUnlinkedAfterLoad();

    // Alert any waiters that the load is complete
    for (auto& promise : promises) {
      promise.setValue(inodePtr);
    }

  } catch (const std::exception& exc) {
    auto bug = EDEN_BUG_EXCEPTION()
        << "InodeMap requested to load inode " << number << "(" << name
        << " in " << getLogPath()
        << "), which has been unlinked, and we hit this "
        << "error while trying to load it from the overlay: " << exc.what();
    getInodeMap()->inodeLoadFailed(number, bug);
  }
}

void TreeInode::loadChildInode(
    PathComponentPiece name,
    InodeNumber number,
    const ObjectFetchContextPtr& context) {
  std::optional<PathComponent> inodeName;
  auto future = Future<unique_ptr<InodeBase>>::makeEmpty();
  // Set when the entry no longer refers to the requested inode. The load is
  // failed after the contents lock is released, since failing it runs the
  // continuations of everyone waiting for the inode.
  folly::exception_wrapper nameReused;
  {
    auto contents = lockContentsRead();
    auto iter = contents->entries.find(name);
    if (iter == contents->entries.end()) {
      auto bug = EDEN_BUG_EXCEPTION()
          << "InodeMap requested to load inode " << number
          << ", but there is no entry named \"" << name << "\" in "
          << getNodeId();
      getInodeMap()->inodeLoadFailed(number, bug);
      return;
    }

    inodeName = copyCanonicalInodeName(iter);
    name = inodeName->piece();
    auto& entry = iter->second;
    if (entry.getInodeNumber() != number) {
      // Removing an unloaded child leaves its InodeMap entry in place for the
      // FS references still held on it, so by the time the InodeMap asks for
      // the inode by name, the name can belong to a new inode. Loading that
      // one here would complete the load of an inode the InodeMap never
      // started.
      XLOGF(
          DBG3,
          "InodeMap requested to load inode {} ({} in {}), but that name now refers to inode {}",
          number,
          name,
          getNodeId(),
          entry.getInodeNumber());
      // An NFS client holding a handle to the old inode expects ESTALE, as
      // InodeMap::lookupInode reports for an inode it no longer knows.
      // Windows has neither NFS nor ESTALE.
#ifdef _WIN32
      const int reusedErrno = ENOENT;
#else
      const int reusedErrno =
          getMount()->throwEstaleIfInodeIsMissing() ? ESTALE : ENOENT;
#endif
      nameReused = folly::make_exception_wrapper<InodeError>(
          reusedErrno, inodePtrFromThis(), name);
    } else if (entry.getInode() != nullptr) {
      // InodeMap makes sure to only try loading each inode once, so this
      // entry should not already be loaded.
      auto bug = EDEN_BUG_EXCEPTION()
          << "InodeMap requested to load inode " << number << "(" << name
          << " in " << getNodeId() << "), which is already loaded";
      // Call inodeLoadFailed().  (Arguably we could call inodeLoadComplete()
      // if the existing inode has the same number as the one we were requested
      // to load.  However, it seems more conservative to just treat this as
      // failed and fail pending promises waiting on this inode.  This may
      // cause problems for anyone trying to access this child inode in the
      // future, but at least it shouldn't damage the InodeMap data structures
      // any further.)
      getInodeMap()->inodeLoadFailed(number, bug);
      return;
    } else {
      future = startLoadingInodeNoThrow(entry, name, context, false);
    }
  }
  if (nameReused) {
    getInodeMap()->inodeLoadFailed(number, nameReused, /*logAsError=*/false);
    return;
  }
  registerInodeLoadComplete(future, name, number);
}

void TreeInode::registerInodeLoadComplete(
    folly::Future<unique_ptr<InodeBase>>& future,
    PathComponentPiece name,
    InodeNumber number) {
  // This method should never be called with the contents_ lock held.  If the
  // future is already ready we will try to acquire the contents_ lock now.
  std::move(future)
      .thenValue([self = inodePtrFromThis(), childName = PathComponent{name}](
                     unique_ptr<InodeBase>&& childInode) {
        self->inodeLoadComplete(childName, std::move(childInode));
      })
      .thenError([self = inodePtrFromThis(),
                  number](const folly::exception_wrapper& ew) {
        self->getInodeMap()->inodeLoadFailed(number, ew);
      });
}

void TreeInode::inodeLoadComplete(
    PathComponentPiece childName,
    std::unique_ptr<InodeBase> childInode) {
  InodeMap::PromiseVector promises;
  InodePtr inodePtr;

  {
    auto contents = lockContentsWrite();
    auto iter = contents->entries.find(childName);
    if (iter == contents->entries.end() ||
        iter->second.getInodeNumber() != childInode->getNodeId()) {
      // The child was removed while this load was in flight. Removing an
      // unloaded child does not wait for a load in flight, and the name may
      // have been reused since, so the entry found here can belong to a
      // different inode. Fail the load instead of attaching this inode to an
      // entry that does not refer to it.
      XLOGF(
          ERR,
          "child {} in {} removed before it finished loading: loaded inode {}, entry now {}",
          childName,
          getLogPath(),
          childInode->getNodeId(),
          iter == contents->entries.end()
              ? std::string{"gone"}
              : folly::to<std::string>(iter->second.getInodeNumber().get()));
      throw InodeError(
          ENOENT,
          inodePtrFromThis(),
          childName,
          "inode removed before loading finished");
    }
    // This load completed after releasing the parent lock. Only cache the
    // restricted bit if the entry still describes the unloaded SCM child we
    // fetched, since it may have been materialized in the meantime.
    if (iter->second.isDirectory() && !iter->second.isMaterialized()) {
      if (auto* childTree = dynamic_cast<TreeInode*>(childInode.get())) {
        auto childTreeId = childTree->getObjectId();
        if (childTreeId &&
            iter->second.getObjectId().bytesEqual(*childTreeId)) {
          if (maybeBackfillAclDirEntry(iter->second, childInode.get()) &&
              hidesRestrictedEntries()) {
            // Omitted mode now hides this entry; drop our cached listing.
            invalidateChannelDirCache(*contents).get();
          }
        }
      }
    }
    iter->second.setInode(childInode.get());
    // Make sure that we are still holding the contents_ lock when
    // calling inodeLoadComplete().  This ensures that no-one can look up
    // the inode by name before it is also available in the InodeMap.
    // However, we must wait to fulfill pending promises until after
    // releasing our lock.
    promises = getInodeMap()->inodeLoadComplete(childInode.get());
    // Take ownership of the inode while still holding the contents_ lock.
    // This ensures the ptrAcquireCount_ is incremented before the lock is
    // released, preventing a race where the background unloader could see
    // ptrAcquireCount_ == 0 and unload this freshly-loaded inode.
    inodePtr = InodePtr::takeOwnership(std::move(childInode));
  }

  // Allow tests to verify that unloading during this window is safe, since
  // ptrAcquireCount_ was already incremented inside the lock above.
  getMount()->getServerState()->getFaultInjector().check(
      "inodeLoadComplete", childName.view());

  // Fulfill all of the pending promises after releasing our lock
  for (auto& promise : promises) {
    promise.setValue(inodePtr);
  }
}

Future<unique_ptr<InodeBase>> TreeInode::startLoadingInodeNoThrow(
    const DirEntry& entry,
    PathComponentPiece name,
    const ObjectFetchContextPtr& fetchContext,
    bool async) noexcept {
  // The callers of startLoadingInodeNoThrow() need to make sure that they
  // always call InodeMap::inodeLoadComplete() or InodeMap::inodeLoadFailed()
  // afterwards.
  //
  // It simplifies their logic to guarantee that we never throw an exception,
  // and always return a Future object.  Therefore we simply wrap
  // startLoadingInode() and convert any thrown exceptions into Future.
  return folly::makeFutureWith([&]() -> Future<unique_ptr<InodeBase>> {
    auto fut = startLoadingInode(entry, name, fetchContext, async);

    // Fast path: if the ImmediateFuture is already ready, return its value
    // directly without going through semi().via().
    if (fut.isReady()) {
      return std::move(fut).get();
    }

    return std::move(fut).semi().via(fetchContext->getDetachedExecutor());
  });
}

static std::vector<std::string> computeEntryDifferences(
    const DirContents& dir,
    const Tree& tree) {
  std::set<std::string> differences;
  for (const auto& entry : dir) {
    auto it = tree.find(entry.first);
    if (it == tree.cend()) {
      differences.insert(fmt::format("- {}", entry.first));
    } else if (!dirEntryMatchesTreeEntry(entry.second, it->second)) {
      differences.insert(fmt::format("~ {}", entry.first));
    }
  }
  for (const auto& entry : tree) {
    if (!dir.count(entry.first)) {
      differences.insert(fmt::format("+ {}", entry.first));
    }
  }
  return std::vector<std::string>{differences.begin(), differences.end()};
}

std::optional<std::vector<std::string>> findEntryDifferences(
    const DirContents& dir,
    const Tree& tree) {
  // Avoid allocations in the case where the tree and dir agree.
  if (dir.size() != tree.size()) {
    return computeEntryDifferences(dir, tree);
  }
  for (const auto& entry : dir) {
    auto it = tree.find(entry.first);
    if (it == tree.cend() ||
        !dirEntryMatchesTreeEntry(entry.second, it->second)) {
      return computeEntryDifferences(dir, tree);
    }
  }
  return std::nullopt;
}

ImmediateFuture<unique_ptr<InodeBase>> TreeInode::startLoadingInode(
    const DirEntry& entry,
    PathComponentPiece name,
    const ObjectFetchContextPtr& fetchContext,
    bool async) {
  XLOGF(
      DBG5,
      "starting to load inode {}: {} / \"{}\"",
      entry.getInodeNumber(),
      getLogPath(),
      name);
  XDCHECK(entry.getInode() == nullptr);
  if (!entry.isDirectory()) {
    // If this is a file we can just go ahead and create it now;
    // we don't need to load anything else.
    //
    // Eventually we may want to go ahead start loading some of the blob data
    // now, but we don't have to wait for it to be ready before marking the
    // inode loaded.
    return make_unique<FileInode>(
        entry.getInodeNumber(),
        inodePtrFromThis(),
        name,
        entry.getInitialMode(),
        std::nullopt,
        entry.getObjectIdPtr());
  }

  // Helper that optionally adds an async point before executing the given
  // function. When async=true, chains on makeNotReadyImmediateFuture() to
  // yield before calling func(). Works with functions returning either
  // plain values or ImmediateFutures (thenValue handles flattening).
  auto maybeAddAsyncPoint = [async](auto func) {
    auto base = async ? makeNotReadyImmediateFuture()
                      : ImmediateFuture<folly::Unit>{folly::unit};
    return std::move(base).thenValue(
        [f = std::move(func)](auto&&) mutable { return f(); });
  };

  if (!entry.isMaterialized()) {
    // If the entry was previously marked as restricted (server denied access
    // via ACL), short-circuit and return a restricted TreeInode without
    // attempting a tree fetch.
    if (entry.isRestricted()) {
      auto caseSensitivity =
          getMount()->getCheckoutConfig()->getCaseSensitive();
      return std::make_unique<TreeInode>(
          entry.getInodeNumber(),
          inodePtrFromThis(),
          name,
          entry.getInitialMode(),
          std::nullopt,
          DirContents{caseSensitivity},
          entry.getObjectId(),
          /*isRestricted=*/true,
          /*hasACL=*/true);
    }

    auto getTreeSpan = fetchContext->createSpan("getTree");

    auto getTreeFunc = [self = inodePtrFromThis(),
                        treeId = entry.getObjectId(),
                        fetchContext = fetchContext.copy()]() mutable {
      return self->getObjectStore().getTree(treeId, fetchContext);
    };

    return maybeAddAsyncPoint(std::move(getTreeFunc))
        .thenValue(
            [self = inodePtrFromThis(),
             childName = PathComponent{name},
             treeId = entry.getObjectId(),
             entryMode = entry.getInitialMode(),
             entryHasACL = entry.hasACL(),
             number = entry.getInodeNumber(),
             fetchContext = fetchContext.copy(),
             getTreeSpan = std::move(getTreeSpan),
             maybeAddAsyncPoint](std::shared_ptr<const Tree> tree) mutable
                -> ImmediateFuture<unique_ptr<InodeBase>> {
              // Tree has been loaded, end the getTree span
              getTreeSpan.reset();

              if (tree->isRestricted()) {
                auto caseSensitivity =
                    self->getMount()->getCheckoutConfig()->getCaseSensitive();
                auto restricted = std::make_unique<TreeInode>(
                    number,
                    std::move(self),
                    childName,
                    entryMode,
                    std::nullopt,
                    DirContents{caseSensitivity},
                    tree->getObjectId(),
                    /*isRestricted=*/true,
                    tree->hasACL(),
                    InitialPermissionCheck::JustDenied);
                return ImmediateFuture<unique_ptr<InodeBase>>{
                    std::move(restricted)};
              }

              auto loadOverlayDirSpan =
                  fetchContext->createSpan("loadOverlayDir");

              auto loadOverlayDirFunc =
                  [self = std::move(self),
                   childName = std::move(childName),
                   treeId,
                   entryMode,
                   entryHasACL,
                   number,
                   tree = std::move(tree),
                   loadOverlayDirSpan = std::move(
                       loadOverlayDirSpan)]() mutable -> unique_ptr<InodeBase> {
                auto dirContents = self->buildUnrestrictedDirContents(
                    number,
                    *tree,
                    [&] {
                      return fmt::format(
                          "{}/{}", self->getLogPath(), childName);
                    },
                    std::move(loadOverlayDirSpan));
                if (dirContents.refreshedStaleDeniedAclRootStates) {
                  // This path only loads non-materialized entries whose tree
                  // fetch succeeded as unrestricted.
                  try {
                    self->saveOverlayDir(
                        number,
                        dirContents.contents,
                        /*isMaterialized=*/false);
                  } catch (const std::exception& ex) {
                    XLOGF(
                        WARN,
                        "failed to persist refreshed unrestricted overlay contents for inode {}: {}",
                        number,
                        folly::exceptionStr(ex));
                  }
                }

                return std::make_unique<TreeInode>(
                    number,
                    std::move(self),
                    childName,
                    entryMode,
                    std::nullopt,
                    std::move(dirContents.contents),
                    treeId,
                    /*isRestricted=*/false,
                    preferKnownAclState(tree->hasACL(), entryHasACL),
                    InitialPermissionCheck::Unknown,
                    dirContents.entryCounts);
              };

              return maybeAddAsyncPoint(std::move(loadOverlayDirFunc));
            });
  }

  // The entry is materialized, so data must exist in the overlay.
  auto loadOverlayDirSpan = fetchContext->createSpan("loadOverlayDir");

  auto createInodeFunc =
      [self = inodePtrFromThis(),
       name = PathComponent{name},
       number = entry.getInodeNumber(),
       mode = entry.getInitialMode(),
       isRestricted = entry.isRestricted(),
       hasACL = entry.hasACL(),
       loadOverlayDirSpan =
           std::move(loadOverlayDirSpan)]() mutable -> unique_ptr<InodeBase> {
    auto overlayDir = self->loadOverlayDir(number);
    loadOverlayDirSpan.reset();

    return make_unique<TreeInode>(
        number,
        std::move(self),
        name,
        mode,
        std::nullopt,
        std::move(overlayDir),
        std::nullopt,
        isRestricted,
        hasACL);
  };

  return maybeAddAsyncPoint(std::move(createInodeFunc));
}

void TreeInode::materialize(const RenameLock* renameLock) {
  // Start timing how long the materialize event takes before adding to TraceBus
  auto startTime = std::chrono::system_clock::now();

  // If we don't have the rename lock yet, do a quick check first
  // to avoid acquiring it if we don't actually need to change anything.
  if (!renameLock) {
    auto contents = lockContentsRead();
    if (contents->isMaterialized()) {
      return;
    }
  }

  {
    // Acquire the rename lock now, if it wasn't passed in
    //
    // Only performing materialization state changes with the RenameLock held
    // makes reasoning about update ordering a bit simpler.  This guarantees
    // that materialization and dematerialization operations cannot be
    // interleaved.  We don't want it to be possible for a
    // materialization/dematerialization to interleave the order in which they
    // update the local overlay data and our parent directory's overlay data,
    // possibly resulting in an inconsistent state where the parent thinks we
    // are materialized but we don't think we are.
    RenameLock renameLock2;
    if (!renameLock) {
      renameLock2 = getMount()->acquireRenameLock();
      renameLock = &renameLock2;
    }

    // Write out our data in the overlay before we update our parent.  If we
    // crash partway through it's better if our parent does not say that we are
    // materialized yet even if we actually do have overlay data present,
    // rather than to have our parent indicate that we are materialized but we
    // don't have overlay data present.
    //
    // In the former case, our overlay data should still be identical to the
    // id mentioned in the parent, so that's fine and we'll still be able to
    // load data correctly the next time we restart.  However, if our parent
    // says we are materialized but we don't actually have overlay data present
    // we won't have any state indicating which source control id our
    // contents are from.
    {
      auto contents = lockContentsWrite();
      // Double check that we still need to be materialized
      if (contents->isMaterialized()) {
        return;
      }
      getMount()->publishInodeTraceEvent(InodeTraceEvent(
          startTime,
          getNodeId(),
          InodeType::TREE,
          InodeEventType::MATERIALIZE,
          InodeEventProgress::START,
          getLocationInfo(*renameLock).name));
      contents->setMaterialized();
      saveOverlayDir(contents->entries);
    }

    // Mark ourself materialized in our parent directory (if we have one)
    auto loc = getLocationInfo(*renameLock);
    if (loc.parent && !loc.unlinked) {
      loc.parent->childMaterialized(*renameLock, loc.name);
    }

    // Finished materializing so publish event to TraceBus
    getMount()->publishInodeTraceEvent(InodeTraceEvent(
        startTime,
        getNodeId(),
        InodeType::TREE,
        InodeEventType::MATERIALIZE,
        InodeEventProgress::END,
        loc.name));
  }
}

/* If we don't yet have an overlay entry for this portion of the tree,
 * populate it from the Tree.  In order to materialize a dir we have
 * to also materialize its parents. */
void TreeInode::childMaterialized(
    const RenameLock& renameLock,
    PathComponentPiece childName,
    bool writeOverlay) {
  auto startTime = std::chrono::system_clock::now();
  bool wasAlreadyMaterialized = false;
  {
    auto contents = lockContentsWrite();
    wasAlreadyMaterialized = contents->isMaterialized();
    if (!wasAlreadyMaterialized) {
      getMount()->publishInodeTraceEvent(InodeTraceEvent(
          startTime,
          getNodeId(),
          InodeType::TREE,
          InodeEventType::MATERIALIZE,
          InodeEventProgress::START,
          getLocationInfo(renameLock).name));
    }

    auto iter = contents->entries.find(childName);
    if (iter == contents->entries.end()) {
      // This should never happen.
      // We should only get called with legitimate children names.
      EDEN_BUG() << "error attempting to materialize " << childName << " in "
                 << getLogPath() << ": entry not present";
    }

    auto& childEntry = iter->second;
    if (contents->isMaterialized() && childEntry.isMaterialized()) {
      // Nothing to do
      return;
    }

    childEntry.setMaterialized();
    contents->setMaterialized();
    if (writeOverlay) {
      if (!wasAlreadyMaterialized) {
        // First materialization — write the full directory to create
        // the overlay file. Subsequent calls go through the WAL fast
        // path (or full save fallback) below.
        saveOverlayDir(contents->entries);
      } else {
        getOverlay()->materializeChild(
            getNodeId(), childName, contents->entries);
      }
    }
  }

  // Materialize parent and publish materialization event only if newly
  // materialized
  if (!wasAlreadyMaterialized) {
    // If we have a parent directory, ask our parent to materialize itself
    // and mark us materialized when it does so.
    auto location = getLocationInfo(renameLock);
    if (location.parent && !location.unlinked) {
      location.parent->childMaterialized(
          renameLock, location.name, writeOverlay);
    }

    getMount()->publishInodeTraceEvent(InodeTraceEvent(
        startTime,
        getNodeId(),
        InodeType::TREE,
        InodeEventType::MATERIALIZE,
        InodeEventProgress::END,
        location.name));
  }
}

void TreeInode::childDematerialized(
    const RenameLock& renameLock,
    PathComponentPiece childName,
    ObjectId childScmId,
    bool writeOverlay,
    bool isRestricted,
    std::optional<bool> hasACL) {
  auto startTime = std::chrono::system_clock::now();
  bool wasAlreadyMaterialized = false;
  {
    auto contents = lockContentsWrite();
    wasAlreadyMaterialized = contents->isMaterialized();
    if (!wasAlreadyMaterialized) {
      getMount()->publishInodeTraceEvent(InodeTraceEvent(
          startTime,
          getNodeId(),
          InodeType::TREE,
          InodeEventType::MATERIALIZE,
          InodeEventProgress::START,
          getLocationInfo(renameLock).name));
    }

    auto iter = contents->entries.find(childName);
    if (iter == contents->entries.end()) {
      // This should never happen.
      // We should only get called with legitimate children names.
      EDEN_BUG() << "error attempting to dematerialize " << childName << " in "
                 << getLogPath() << ": entry not present";
    }

    auto& childEntry = iter->second;
    const auto updatedAclRootState = makeAclRootState(
        isRestricted, preferKnownAclState(hasACL, childEntry.hasACL()));
    // Should this call ObjectStore::areObjectsKnownIdentical? No, even if IDs
    // are compatible, we want to migrate our inode to the new ID scheme, which
    // requires writing it to the overlay.
    if (!childEntry.isMaterialized() &&
        childEntry.getObjectId().bytesEqual(childScmId) &&
        childEntry.aclRootState() == updatedAclRootState) {
      // Nothing to do.  Our child's state and our own are both unchanged.
      return;
    }

    // Mark the child dematerialized.
    childEntry.setDematerialized(childScmId);
    childEntry.setAclRootState(updatedAclRootState);

    // Mark us materialized!
    //
    // Even though our child is dematerialized, we always materialize ourself
    // so we make sure we record the correct source control id for our child.
    // Currently dematerialization only happens on the checkout() flow.  Once
    // checkout finishes processing all of the children it will call
    // saveOverlayPostCheckout() on this directory, and here we will check to
    // see if we can dematerialize ourself.
    contents->setMaterialized();
    if (writeOverlay) {
      saveOverlayDir(contents->entries);
    }
  }

  // Materialize parent and publish materialization event only if newly
  // materialized
  if (!wasAlreadyMaterialized) {
    // We are newly materialized now.
    // If we have a parent directory, ask our parent to materialize itself
    // and mark us materialized when it does so.
    auto location = getLocationInfo(renameLock);
    if (location.parent && !location.unlinked) {
      location.parent->childMaterialized(
          renameLock, location.name, writeOverlay);
    }
    getMount()->publishInodeTraceEvent(InodeTraceEvent(
        startTime,
        getNodeId(),
        InodeType::TREE,
        InodeEventType::MATERIALIZE,
        InodeEventProgress::END,
        location.name));
  }
}

Overlay* TreeInode::getOverlay() const {
  return getMount()->getOverlay();
}

DirContents TreeInode::loadOverlayDir(InodeNumber inodeNumber) const {
  return getOverlay()->loadOverlayDir(inodeNumber);
}

void TreeInode::saveOverlayDir(const DirContents& contents, bool isMaterialized)
    const {
  return saveOverlayDir(getNodeId(), contents, isMaterialized);
}

void TreeInode::saveOverlayDir(
    InodeNumber inodeNumber,
    const DirContents& contents,
    bool isMaterialized) const {
  return getOverlay()->saveOverlayDir(inodeNumber, contents, isMaterialized);
}

TreeInode::TreeDirContents TreeInode::saveDirFromTree(
    InodeNumber inodeNumber,
    const Tree* tree,
    EdenMount* mount) {
  auto overlay = mount->getOverlay();
  auto dir = buildDirFromTree(
      tree, overlay, mount->getCheckoutConfig()->getCaseSensitive());

  // buildDirFromTree just allocated inode numbers; they should be saved.
  overlay->saveOverlayDir(inodeNumber, dir.contents, /*isMaterialized=*/false);

  return dir;
}

TreeInode::TreeDirContents TreeInode::buildDirFromTree(
    const Tree* tree,
    Overlay* overlay,
    CaseSensitivity caseSensitive) {
  XCHECK(tree);

  auto startInode = overlay->allocateInodeNumbers(tree->size());

  folly::fbvector<std::pair<PathComponent, DirEntry>> entries;
  entries.reserve(tree->size());

  ObjectFetchContext::DirEntryCounts entryCounts{};
  uint64_t inodeOffset = 0;
  for (const auto& treeEntry : *tree) {
    auto type = treeEntry.second.getType();
    if (type == TreeEntryType::TREE) {
      ++entryCounts.dirs;
    } else {
      ++entryCounts.files;
    }
    entries.emplace_back(
        treeEntry.first,
        DirEntry{
            modeFromTreeEntryType(type),
            InodeNumber{startInode.get() + inodeOffset++},
            treeEntry.second.getObjectId(),
            treeEntry.second.isRestricted(),
            treeEntry.second.hasACL()});
  }

  return TreeDirContents{
      DirContents{std::move(entries), caseSensitive}, entryCounts};
}

TreeInode::BuildUnrestrictedDirContentsResult
TreeInode::buildUnrestrictedDirContents(
    InodeNumber inodeNumber,
    const Tree& tree,
    folly::FunctionRef<std::string()> logPath,
    std::optional<MiniTracer::Span> loadOverlayDirSpan) {
  // Even if the inode is not materialized, it may have inode
  // numbers stored in the overlay.
  auto overlayDir = loadOverlayDir(inodeNumber);
  loadOverlayDirSpan.reset();

  if (!overlayDir.empty()) {
    auto refreshedStaleDeniedAclRootStates =
        refreshStaleAclRootStates(getObjectStore(), overlayDir, tree);

    if (auto differences = findEntryDifferences(overlayDir, tree)) {
      std::string diffString;
      for (const auto& diff : *differences) {
        diffString += diff;
        diffString += '\n';
      }
      XLOGF(
          ERR,
          "loaded inode {} (inode number {}) from overlay but the entries don't correspond with the tree.  Something is wrong!\n{}",
          logPath(),
          inodeNumber,
          diffString);
    }
    return BuildUnrestrictedDirContentsResult{
        std::move(overlayDir),
        refreshedStaleDeniedAclRootStates,
        /*entryCounts=*/std::nullopt};
  }

  auto dir = saveDirFromTree(inodeNumber, &tree, getMount());
  return BuildUnrestrictedDirContentsResult{
      std::move(dir.contents),
      /*refreshedStaleDeniedAclRootStates=*/false,
      dir.entryCounts};
}

FileInodePtr TreeInode::createImpl(
    folly::Synchronized<TreeInodeState>::LockedPtr contents,
    PathComponentPiece name,
    mode_t mode,
    [[maybe_unused]] ByteRange fileContents,
    InvalidationRequired invalidate,
    std::chrono::system_clock::time_point startTime) {
#ifndef _WIN32
  // This relies on the fact that the dotEdenInodeNumber field of EdenMount is
  // not defined until after EdenMount finishes configuring the .eden directory.
  if (getNodeId() == getMount()->getDotEdenInodeNumber()) {
    throw InodeError(EPERM, inodePtrFromThis(), name);
  }
#endif
  FileInodePtr inode;
  RelativePath targetName;

  // New scope to distinguish work done with the contents lock and to help
  // manage releasing it.
  {
    // Ensure that we always unlock contents at the end of this scope.
    // Even if an exception is thrown we need to make sure we release the
    // contents lock before the local inode variable gets destroyed.
    // If an error is thrown, destroying the inode may attempt to acquire the
    // parents contents lock, which will block if we are still holding it.
    // (T42835005).
    SCOPE_EXIT {
      contents.unlock();
    };

    // Make sure that an entry with this name does not already exist.
    //
    // In general FUSE should avoid calling create(), symlink(), or mknod() on
    // entries that already exist.  It performs its own check in the kernel
    // first to see if this entry exists.  However, this may race with a
    // checkout operation, so it is still possible that it calls us with an
    // entry that was in fact just created by a checkout operation.
    auto entIter = contents->entries.find(name);
    if (entIter != contents->entries.end()) {
      throw InodeError(EEXIST, this->inodePtrFromThis(), name);
    }

    auto myPath = getPath();
    // Make sure this directory has not been unlinked.
    // We have to check this after acquiring the contents_ lock; otherwise
    // we could race with rmdir() or rename() calls affecting us.
    if (!myPath.has_value()) {
      throw InodeError(ENOENT, inodePtrFromThis());
    }

    // Compute the target path, so we can record it in the journal below
    // after releasing the contents lock.
    targetName = myPath.value() + name;

#ifndef _WIN32
    // Claim a preallocated overlay file (and its reserved inode number) if
    // the pool has one; otherwise allocate a number and create the overlay
    // file here.
    auto prepared = getOverlay()->tryClaimPreparedFile(fileContents);
    auto childNumber =
        prepared ? prepared->first : getOverlay()->allocateInodeNumber();
#else
    // Generate an inode number for this new entry.
    auto childNumber = getOverlay()->allocateInodeNumber();
#endif
    getMount()->publishInodeTraceEvent(InodeTraceEvent(
        startTime,
        childNumber,
        InodeType::FILE,
        InodeEventType::MATERIALIZE,
        InodeEventProgress::START,
        targetName));

#ifndef _WIN32
    // Create the overlay file before we insert the file into our entries map.
    auto file = prepared
        ? std::move(prepared->second)
        : getOverlay()->createOverlayFile(childNumber, fileContents);
#endif

    auto now = getNow();
    auto inodeTimestamps = InodeTimestamps{now};

    // Record the new entry
    auto insertion = contents->entries.emplace(name, mode, childNumber);
    XCHECK(insertion.second)
        << "we already confirmed that this entry did not exist above";
    auto& entry = insertion.first->second;

    inode = FileInodePtr::makeNew(
        childNumber, this->inodePtrFromThis(), name, mode, inodeTimestamps);

    entry.setInode(inode.get());
    getInodeMap()->inodeCreated(inode);

    updateMtimeAndCtimeLocked(contents->entries, now);
#ifndef _WIN32
    getMount()->getServerState()->getFaultInjector().check(
        "createInodeSaveOverlay", name);
#endif

    getOverlay()->addChild(getNodeId(), *insertion.first, contents->entries);

#ifndef _WIN32
    if (getMount()
            ->getEdenConfig()
            ->experimentalOverlayReuseCreatedFds.getValue()) {
      const bool cached = getMount()->getOverlayFileAccess()->cacheCreatedFile(
          childNumber, std::move(file), fileContents.size());
      getMount()->getStats()->increment(
          cached ? &OverlayStats::createdFdCached
                 : &OverlayStats::createdFdAlreadyOpen);
    }
#endif

    // Once the overlay is fully updated, the inode is materialized so we can
    // publish this to TraceBus
    getMount()->publishInodeTraceEvent(InodeTraceEvent(
        startTime,
        childNumber,
        InodeType::FILE,
        InodeEventType::MATERIALIZE,
        InodeEventProgress::END,
        targetName));
  }

  if (InvalidationRequired::Yes == invalidate) {
    invalidateChannelEntryCache(*contents, name, std::nullopt)
        .throwUnlessValue();
    // Make sure that the directory cache is invalidated so a subsequent
    // readdir will see the added file.
    invalidateChannelDirCache(*contents).get();
  }

  getMount()->getJournal().recordCreated(targetName, inode->getType());

  return inode;
}

std::optional<ObjectId> TreeInode::getObjectId() const {
  auto state = getContentsUnchecked().rlock();
  return state->treeId;
}

folly::coro::now_task<std::optional<Hash32>> TreeInode::co_getDigestHash(
    const ObjectFetchContextPtr& fetchContext) {
  if (FOLLY_UNLIKELY(isRestricted())) {
    co_return std::nullopt;
  }
  logAccess(*fetchContext);
  ObjectId treeId;
  {
    auto state = lockContentsRead();
    if (state->isMaterialized()) {
      co_return std::nullopt;
    }
    // If a tree is not materialized, it should have an id value.
    treeId = state->treeId.value();
  }
  co_return co_await getObjectStore().co_getTreeDigestHash(
      treeId, fetchContext);
}

ImmediateFuture<std::optional<uint64_t>> TreeInode::getDigestSize(
    const ObjectFetchContextPtr& fetchContext) {
  if (FOLLY_UNLIKELY(isRestricted())) {
    return std::optional<uint64_t>(std::nullopt);
  }
  logAccess(*fetchContext);
  auto state = lockContentsRead();

  if (!state->isMaterialized()) {
    // If a tree is not materialized, it should have an id size.
    return getObjectStore()
        .getTreeDigestSize(state->treeId.value(), fetchContext)
        .thenValue(
            [](std::optional<uint64_t>&& size) { return std::move(size); });
  }
  return ImmediateFuture<std::optional<uint64_t>>{std::nullopt};
}

ImmediateFuture<std::optional<TreeAuxData>> TreeInode::getTreeAuxData(
    const ObjectFetchContextPtr& fetchContext) {
  if (FOLLY_UNLIKELY(isRestricted())) {
    return std::optional<TreeAuxData>(std::nullopt);
  }
  logAccess(*fetchContext);
  auto state = lockContentsRead();

  if (!state->isMaterialized()) {
    // If a tree is not materialized, it should have aux data.
    return getObjectStore()
        .getTreeAuxData(state->treeId.value(), fetchContext)
        .thenValue([](std::optional<TreeAuxData>&& treeAux) {
          return std::move(treeAux);
        });
  }
  return ImmediateFuture<std::optional<TreeAuxData>>{std::nullopt};
}

folly::coro::now_task<std::optional<TreeAuxData>> TreeInode::co_getTreeAuxData(
    const ObjectFetchContextPtr& fetchContext) {
  // Mirrors getTreeAuxData() — restricted directories must not expose
  // aux data, and materialized trees do not have backing-store aux data
  // available.
  if (FOLLY_UNLIKELY(isRestricted())) {
    co_return std::nullopt;
  }
  logAccess(*fetchContext);
  ObjectId treeId;
  {
    auto state = lockContentsRead();
    if (state->isMaterialized()) {
      co_return std::nullopt;
    }
    // If a tree is not materialized, it should have aux data.
    treeId = state->treeId.value();
  }
  co_return co_await getObjectStore().co_getTreeAuxData(treeId, fetchContext);
}

FileInodePtr TreeInode::symlink(
    PathComponentPiece name,
    folly::StringPiece symlinkTarget,
    InvalidationRequired invalidate) {
  // symlink creates a newly materialized file in createImpl. We count this as
  // an inode materialization event to publish to TraceBus, which we begin
  // timing here before the parent tree inode materializes
  auto startTime = std::chrono::system_clock::now();

  validatePathComponentLength(name);
  materialize();

  getMount()->getServerState()->getFaultInjector().check(
      "TreeInode::symlink", name);

  {
    // Acquire our contents lock
    auto contents = lockContentsWrite();
    const mode_t mode = S_IFLNK | 0770;
    return createImpl(
        std::move(contents),
        name,
        mode,
        ByteRange{symlinkTarget},
        invalidate,
        startTime);
  }
}

FileInodePtr TreeInode::mknod(
    PathComponentPiece name,
    mode_t mode,
    dev_t dev,
    InvalidationRequired invalidate) {
  // mknod creates a newly materialized file in createImpl. We count this as an
  // inode materialization event to publish to TraceBus, which we begin timing
  // here before the parent tree inode materializes
  auto startTime = std::chrono::system_clock::now();

  validatePathComponentLength(name);

  // Compute the effective name of the node they want to create.
  RelativePath targetName;
  FileInodePtr inode;

  bool supported = S_ISSOCK(mode) || S_ISREG(mode);
#ifdef __linux__
  supported = supported ||
      (S_ISFIFO(mode) && getMount()->getEdenConfig()->enableFifo.getValue());
#endif
  if (!supported) {
    throw InodeError(
        EPERM, inodePtrFromThis(), name, "unsupported file type for mknod");
  }

  // The dev parameter to mknod only applies to block and character devices,
  // which edenfs does not support today.  Therefore, we do not need to store
  // it.  If we add block device support in the future, makes sure dev makes it
  // into the FileInode and directory entry.
  (void)dev;

  materialize();

  {
    // Acquire our contents lock
    auto contents = lockContentsWrite();
    return createImpl(
        std::move(contents), name, mode, ByteRange{}, invalidate, startTime);
  }
}

TreeInodePtr TreeInode::mkdir(
    PathComponentPiece name,
    mode_t mode,
    InvalidationRequired invalidate) {
  // A new materialized subtree is created in mkdir. We count this as a new
  // materializion event to publish to TraceBus which we begin timing.
  auto startTime = std::chrono::system_clock::now();

#ifndef _WIN32
  if (getNodeId() == getMount()->getDotEdenInodeNumber()) {
    throw InodeError(EPERM, inodePtrFromThis(), name);
  }
#endif
  validatePathComponentLength(name);

  RelativePath targetName;
  // Compute the effective name of the node they want to create.
  materialize();

  TreeInodePtr newChild;
  {
    // Acquire our contents lock
    auto contents = lockContentsWrite();

    auto myPath = getPath();
    // Make sure this directory has not been unlinked.
    // We have to check this after acquiring the contents_ lock; otherwise
    // we could race with rmdir() or rename() calls affecting us.
    if (!myPath.has_value()) {
      throw InodeError(ENOENT, inodePtrFromThis());
    }
    // Compute the target path, so we can record it in the journal below.
    targetName = myPath.value() + name;

    auto entIter = contents->entries.find(name);
    if (entIter != contents->entries.end()) {
      throw InodeError(EEXIST, this->inodePtrFromThis(), name);
    }

    if (InvalidationRequired::Yes == invalidate) {
      invalidateChannelEntryCache(*contents, name, std::nullopt)
          .throwUnlessValue();
      invalidateChannelDirCache(*contents).get();
    }

#ifndef _WIN32
    // Claim an inode number whose empty overlay record is already on disk
    // if the pool has one; otherwise allocate a number and save the record
    // here.
    auto preparedDir = getOverlay()->tryClaimPreparedDir();
    auto childNumber =
        preparedDir ? *preparedDir : getOverlay()->allocateInodeNumber();
#else
    // Allocate an inode number
    auto childNumber = getOverlay()->allocateInodeNumber();
#endif
    getMount()->publishInodeTraceEvent(InodeTraceEvent(
        startTime,
        childNumber,
        InodeType::TREE,
        InodeEventType::MATERIALIZE,
        InodeEventProgress::START,
        targetName));

    // The mode passed in by the caller may not have the file type bits set.
    // Ensure that we mark this as a directory.
    mode = S_IFDIR | (07777 & mode);

    // Store the overlay entry for this dir
    DirContents emptyDir(getMount()->getCheckoutConfig()->getCaseSensitive());
#ifndef _WIN32
    if (!preparedDir) {
      saveOverlayDir(childNumber, emptyDir);
    }
#else
    saveOverlayDir(childNumber, emptyDir);
#endif

    // Add a new entry to contents_.entries
    auto emplaceResult = contents->entries.emplace(name, mode, childNumber);
    XCHECK(emplaceResult.second)
        << "directory contents should not have changed since the check above";
    auto& entry = emplaceResult.first->second;

    // Update timeStamps of newly created directory and current directory.
    auto now = getNow();
    newChild = TreeInodePtr::makeNew(
        childNumber,
        this->inodePtrFromThis(),
        name,
        mode,
        InodeTimestamps{now},
        std::move(emptyDir),
        std::nullopt);
    entry.setInode(newChild.get());
    getInodeMap()->inodeCreated(newChild);

    // Save our updated overlay data
    updateMtimeAndCtimeLocked(contents->entries, now);
    getOverlay()->addChild(
        getNodeId(), *emplaceResult.first, contents->entries);

    // Once the overlay is fully updated, the inode is materialized so we can
    // publish this to TraceBus
    getMount()->publishInodeTraceEvent(InodeTraceEvent(
        startTime,
        childNumber,
        InodeType::TREE,
        InodeEventType::MATERIALIZE,
        InodeEventProgress::END,
        targetName));
  }

  getMount()->getJournal().recordCreated(targetName, newChild->getType());

  return newChild;
}

ImmediateFuture<folly::Unit> TreeInode::unlink(
    PathComponentPiece name,
    InvalidationRequired invalidate,
    const ObjectFetchContextPtr& context) {
  return getOrLoadChild(name, context)
      .thenValue([self = inodePtrFromThis(),
                  childName = PathComponent{name},
                  invalidate,
                  context = context.copy()](const InodePtr& child) mutable {
        return self->removeImpl<FileInodePtr>(
            std::move(childName), child, invalidate, 1, context);
      });
}

ImmediateFuture<folly::Unit> TreeInode::rmdir(
    PathComponentPiece name,
    InvalidationRequired invalidate,
    const ObjectFetchContextPtr& context) {
  return getOrLoadChild(name, context)
      .thenValue([self = inodePtrFromThis(),
                  childName = PathComponent{name},
                  invalidate,
                  context = context.copy()](const InodePtr& child) mutable {
        return self->removeImpl<TreeInodePtr>(
            std::move(childName), child, invalidate, 1, context);
      });
}

void TreeInode::removeAllChildrenRecursively(
    InvalidationRequired invalidate,
    const ObjectFetchContextPtr& context,
    const RenameLock& renameLock) {
  // If this tree is not materialized and has no entries, we can return early
  // without materializing, since there's nothing to remove.
  {
    auto contents = lockContentsRead();
    if (!contents->isMaterialized() && contents->entries.empty()) {
      return;
    }
  }

  materialize(&renameLock);
#ifndef _WIN32
  if (getNodeId() == getMount()->getDotEdenInodeNumber()) {
    throw InodeError(EPERM, inodePtrFromThis());
  }
#endif

  std::vector<TreeInodePtr> loadedTreeNodes;
  // Step 1, collect children nodes who are tree and loaded
  {
    auto contents = lockContentsRead();
    for (auto& entry : contents->entries.all()) {
      if (auto asTreePtr = entry.second.asTreePtrOrNull()) {
        loadedTreeNodes.push_back(std::move(asTreePtr));
      }
    }
  }

  // Step 2, Clear contents in the child folders
  for (auto& treeNode : loadedTreeNodes) {
    treeNode->removeAllChildrenRecursively(invalidate, context, renameLock);
  }

  loadedTreeNodes.clear();

  // Step 3, Now all child nodes are removable, unless one of the directories
  // had a new entry added while the contents lock was not held.
  auto contents = lockContentsWrite();
  auto it = contents->entries.all().begin();
  while (it != contents->entries.end()) {
    auto inodeNum = it->second.getInodeNumber();
    bool isDir = it->second.isDirectory();
    bool isLoaded = it->second.getInode() != nullptr;
    if (isLoaded) {
      // If a treeInode is not empty, i.e. files were added to the tree
      // between step2 and step3, an exception will be thrown.

      // TODO: There's a race here: checkPreRemove acquires the child's
      // contents lock but then releases it after the check. Thus, there's a
      // window where the child can gain an entry being unlinked, which breaks
      // EdenFS's internal data model. This code should acquire the child's
      // contents lock and hold it across the unlink operation.
      //
      // TODO: Have checkPreRemove take a DirContents& to ensure the contents
      // lock is acquired by the parent, and encourage holding it across the
      // unlink operation.
      //
      // Be careful, here we obtains TreeInode* instead of TreeIndePtr to avoid
      // deference of TreeInodePtr, otherwise there could be a deadlock on
      // getting the parent location info when deference.
      if (TreeInode* asTreePtr = it->second.asTreeOrNull()) {
        int checkResult = checkPreRemove(*asTreePtr);
        if (checkResult != 0) {
          throw InodeError(checkResult, InodePtr::newPtrLocked(asTreePtr));
        }
      }

      auto inode = it->second.getInode();
      inode->markUnlinked(this, it->first, renameLock);
    }
    // Erase from contents must happen right after markUnlink
    it = contents->entries.erase(it);

    // A loaded child frees its own overlay state when it is unloaded, which
    // may not happen until the kernel drops its references to it. Freeing it
    // here would take that state away from an inode that is still in use.
    if (!isLoaded) {
      if (isDir) {
        getOverlay()->recursivelyRemoveOverlayDir(inodeNum);
      } else {
        getOverlay()->removeOverlayFile(inodeNum);
      }
    }
  }

  if (InvalidationRequired::Yes == invalidate) {
    invalidateChannelDirCache(*contents).get();
  }
  updateMtimeAndCtimeLocked(contents->entries, getNow());
  getOverlay()->removeChildren(getNodeId(), contents->entries);
}

InodePtr TreeInode::tryRemoveUnloadedChild(
    PathComponentPiece name,
    InvalidationRequired invalidate) {
#ifndef _WIN32
  if (getNodeId() == getMount()->getDotEdenInodeNumber()) {
    throw InodeError(EPERM, inodePtrFromThis());
  }
#endif
  {
    // Peek first so a missing or already loaded child does not materialize
    // this directory as a side effect.
    auto contents = lockContentsRead();
    auto it = contents->entries.find(name);
    if (it == contents->entries.end()) {
      throw InodeError(ENOENT, inodePtrFromThis(), name);
    }
    if (auto node = it->second.getInodePtr()) {
      return node;
    }
  }

  // Materializing needs the rename lock, which must be acquired before the
  // contents lock. Holding it also keeps the path recorded in the journal
  // accurate, as in removeImpl.
  auto renameLock = getMount()->acquireRenameLock();
  auto myPath = getPath();
  if (!myPath.has_value()) {
    throw InodeError(ENOENT, inodePtrFromThis());
  }
  materialize(&renameLock);

  std::optional<PathComponent> inodeName;
  dtype_t dtype;
  {
    auto contents = lockContentsWrite();

    auto it = contents->entries.find(name);
    if (it == contents->entries.end()) {
      throw InodeError(ENOENT, inodePtrFromThis(), name);
    }

    inodeName = copyCanonicalInodeName(it);
    auto inodeNumber = it->second.getInodeNumber();

    if (auto node = it->second.getInodePtr()) {
      // The child was loaded while the lock was not held. Fall back to the
      // slow path.
      return node;
    }

    // erase() invalidates the iterator.
    bool isDir = it->second.isDirectory();
    dtype = it->second.getDtype();

    contents->entries.erase(it);
    if (InvalidationRequired::Yes == invalidate) {
      invalidateChannelEntryCache(*contents, *inodeName, inodeNumber)
          .throwUnlessValue();
      invalidateChannelDirCache(*contents).get();
    }

    updateMtimeAndCtimeLocked(contents->entries, getNow());
    if (isDir) {
      getOverlay()->recursivelyRemoveOverlayDir(inodeNumber);
    } else {
      getOverlay()->removeOverlayFile(inodeNumber);
    }
    getOverlay()->removeChild(
        getNodeId(), inodeName->piece(), contents->entries);
  }

  getMount()->getJournal().recordRemoved(
      myPath.value() + inodeName->piece(), dtype);
  return nullptr;
}

ImmediateFuture<folly::Unit> TreeInode::removeRecursivelyNoFlushInvalidation(
    PathComponentPiece name,
    InvalidationRequired invalidate,
    const ObjectFetchContextPtr& context) {
  // Fast return if the node is unloaded and removed
  auto child = tryRemoveUnloadedChild(name, invalidate);
  if (!child) {
    return folly::unit;
  }

  if (child.asFilePtrOrNull()) {
    return inodePtrFromThis()->removeImpl<FileInodePtr>(
        PathComponent{name}, std::move(child), invalidate, 1, context);
  } else {
    {
      auto renameLock = inodePtrFromThis()->getMount()->acquireRenameLock();
      child.asTreePtr()->removeAllChildrenRecursively(
          invalidate, context, renameLock);
    }
    return inodePtrFromThis()->removeImpl<TreeInodePtr>(
        PathComponent{name}, std::move(child), invalidate, 1, context);
  }
}

ImmediateFuture<folly::Unit> TreeInode::removeRecursively(
    PathComponentPiece name,
    InvalidationRequired invalidate,
    const ObjectFetchContextPtr& context) {
  return this->removeRecursivelyNoFlushInvalidation(name, invalidate, context)
      .thenValue(
          [self = inodePtrFromThis(),
           invalidate](folly::Unit&&) -> ImmediateFuture<folly::Unit> {
            if (invalidate == InvalidationRequired::Yes) {
              return self->getMount()->flushInvalidations();
            }
            return folly::unit;
          });
}

template <typename InodePtrType>
ImmediateFuture<folly::Unit> TreeInode::removeImpl(
    PathComponent name,
    InodePtr childBasePtr,
    InvalidationRequired invalidate,
    unsigned int attemptNum,
    const ObjectFetchContextPtr& context) {
  // Make sure the child is of the desired type
  auto child = childBasePtr.asSubclassPtrOrNull<InodePtrType>();
  if (!child) {
    return ImmediateFuture<Unit>{folly::Try<Unit>{
        InodeError{InodePtrType::InodeType::WRONG_TYPE_ERRNO, childBasePtr}}};
  }

  // Verify that we can remove the child before we materialize ourself
  int checkResult = checkPreRemove(*child);
  if (checkResult != 0) {
    return ImmediateFuture<Unit>{
        folly::Try<Unit>{InodeError{checkResult, child}}};
  }

  // Acquire the rename lock since we need to update our child's location
  auto renameLock = getMount()->acquireRenameLock();

  // Get the path to the child, so we can update the journal later.
  // Make sure we only do this after we acquire the rename lock, so that the
  // path reported in the journal will be accurate.
  auto myPath = getPath();
  if (!myPath.has_value()) {
    // It appears we have already been unlinked.  It's possible someone other
    // thread has already renamed child to another location and unlinked us.
    // Just fail with ENOENT in this case.
    return ImmediateFuture<Unit>{
        folly::Try<Unit>{InodeError{ENOENT, inodePtrFromThis()}}};
  }
  auto targetName = myPath.value() + name;

  // The entry in question may have been renamed since we loaded the child
  // Inode pointer.  If this happens, that's fine, and we just want to go ahead
  // and try removing whatever is present with this name anyway.
  //
  // Therefore leave the child parameter for tryRemoveChild() as null, and let
  // it remove whatever it happens to find with this name.
  const InodePtrType nullChildPtr;
  int errnoValue = tryRemoveChild(renameLock, name, nullChildPtr, invalidate);
  if (errnoValue == 0) {
    // We successfully removed the child.
    // Record the change in the journal.
    getMount()->getJournal().recordRemoved(targetName, child->getType());

    return folly::unit;
  }

  // EBADF means that the child in question has been replaced since we looked
  // it up earlier, and the child inode now at this location is not loaded.
  if (errnoValue != EBADF) {
    return ImmediateFuture<Unit>{
        folly::Try<Unit>{InodeError{errnoValue, inodePtrFromThis(), name}}};
  }

  // Give up after 3 retries
  constexpr unsigned int kMaxRemoveRetries = 3;
  if (attemptNum > kMaxRemoveRetries) {
    throw InodeError(
        EIO,
        inodePtrFromThis(),
        name,
        "inode was removed/renamed after remove started");
  }

  // Note that we intentionally create childFuture() in a separate
  // statement before calling thenValue() on it, since we std::move()
  // the name into the lambda capture for thenValue().
  //
  // Pre-C++17 this has undefined behavior if they are both in the same
  // statement: argument evaluation order is undefined, so we could
  // create the lambda (and invalidate name) before calling
  // getOrLoadChildTree(name).  C++17 fixes this order to guarantee that
  // the left side of "." will always get evaluated before the right
  // side.
  auto childFuture = getOrLoadChild(name, context);
  return std::move(childFuture)
      .thenValue([self = inodePtrFromThis(),
                  childName = PathComponent{std::move(name)},
                  invalidate,
                  attemptNum,
                  context = context.copy()](const InodePtr& loadedChild) {
        return self->removeImpl<InodePtrType>(
            childName, loadedChild, invalidate, attemptNum + 1, context);
      });
}

template <typename InodePtrType>
int TreeInode::tryRemoveChild(
    const RenameLock& renameLock,
    PathComponentPiece name,
    InodePtrType child,
    InvalidationRequired invalidate) {
  materialize(&renameLock);

#ifndef _WIN32
  // prevent unlinking files in the .eden directory
  if (getNodeId() == getMount()->getDotEdenInodeNumber()) {
    return EPERM;
  }
#endif // !_WIN32

  // Lock our contents in write mode.
  // We will hold it for the duration of the unlink.
  std::unique_ptr<InodeBase> deletedInode;
  std::optional<PathComponent> inodeName;
  {
    auto contents = lockContentsWrite();

    // Make sure that this name still corresponds to the child inode we just
    // looked up.
    auto entIter = contents->entries.find(name);
    if (entIter == contents->entries.end()) {
      return ENOENT;
    }
    inodeName = copyCanonicalInodeName(entIter);
    name = inodeName->piece();
    auto& ent = entIter->second;
    if (!ent.getInode()) {
      // The inode in question is not loaded.  The caller will need to load it
      // and retry (if they want to retry).
      return EBADF;
    }
    if (child) {
      if (ent.getInode() != child.get()) {
        // This entry no longer refers to what the caller expected.
        return EBADF;
      }
    } else {
      // Make sure the entry being removed is the expected file/directory type.
      child = ent.getInodePtr().asSubclassPtrOrNull<InodePtrType>();
      if (!child) {
        return InodePtrType::InodeType::WRONG_TYPE_ERRNO;
      }
    }

    // Verify that the child is still in a good state to remove
    auto checkError = checkPreRemove(*child);
    if (checkError != 0) {
      return checkError;
    }

    // Flush the kernel cache for this entry if requested.
    // Since invalidation can fail on ProjectedFS, do it while holding the
    // TreeInode write lock and before updating the contents.
    if (InvalidationRequired::Yes == invalidate) {
      auto success =
          invalidateChannelEntryCache(*contents, name, ent.getInodeNumber());
      if (success.hasException()) {
        return EIO;
      }

      success = invalidateChannelDirCache(*contents).getTry();
      if (success.hasException()) {
        return EIO;
      }
    }

    // Inform the child it is now unlinked
    deletedInode = child->markUnlinked(this, name, renameLock);

    // Remove it from our entries list
    contents->entries.erase(entIter);

    // We want to update mtime and ctime of parent directory after removing the
    // child.
    updateMtimeAndCtimeLocked(contents->entries, getNow());
    getOverlay()->removeChild(getNodeId(), name, contents->entries);
  }
  deletedInode.reset();

  // We have successfully removed the entry.
  return 0;
}

int TreeInode::checkPreRemove(const TreeInode& child) {
  // Lock the child contents, and make sure they are empty
  auto childContents = child.lockContentsRead();
  if (!childContents->entries.empty()) {
    return ENOTEMPTY;
  }
  return 0;
}

int TreeInode::checkPreRemove(const FileInode& /* child */) {
  // Nothing to do
  return 0;
}

/**
 * A helper class that stores all locks required to perform a rename.
 *
 * This class helps acquire the locks in the correct order.
 */
class TreeInode::TreeRenameLocks {
 public:
  TreeRenameLocks() = default;
  ~TreeRenameLocks() {
    reset();
  }

  TreeRenameLocks(const TreeRenameLocks&) = delete;
  TreeRenameLocks& operator=(const TreeRenameLocks&) = delete;
  TreeRenameLocks(TreeRenameLocks&&) = delete;
  TreeRenameLocks& operator=(TreeRenameLocks&&) = delete;

  void acquireLocks(
      RenameLock&& renameLock,
      TreeInode& srcTree,
      TreeInode& destTree,
      PathComponentPiece destName);

  /**
   * Reset the TreeRenameLocks to the empty state, releasing all locks that it
   * holds. The reference on the destination child is dropped last, after the
   * rename lock, so an unlinked destination is destroyed with no lock held.
   */
  void reset() {
    releaseAllButRename();
    renameLock_ = RenameLock{};
    destChildRef_.reset();
  }

  /**
   * Release all locks held by this TreeRenameLocks object except for the
   * mount point RenameLock. The reference on the destination child is kept
   * until reset().
   */
  void releaseAllButRename() {
    srcContentsLock_ = {};
    destContentsLock_ = {};
    destChildContentsLock_ = {};
    srcContents_ = nullptr;
    destContents_ = nullptr;
    destChildContents_ = nullptr;
    destChildIter_ = {};
  }

  const RenameLock& renameLock() const {
    return renameLock_;
  }

  DirContents* srcContents() {
    return srcContents_;
  }

  DirContents* destContents() {
    return destContents_;
  }

  TreeInodeState& srcInodeState() {
    return *srcContentsLock_;
  }

  TreeInodeState& dstInodeState() {
    return *destContentsLock_;
  }

  const PathMap<DirEntry>::iterator& destChildIter() const {
    return destChildIter_;
  }
  InodeBase* destChild() const {
    XDCHECK(destChildExists());
    return destChildIter_->second.getInode();
  }

  bool destChildExists() const {
    return destChildIter_ != destContents_->end();
  }
  bool destChildIsDirectory() const {
    XDCHECK(destChildExists());
    return destChildIter_->second.isDirectory();
  }
  bool destChildIsEmpty() const {
    XDCHECK(destChildContents_);
    return destChildContents_->empty();
  }

 private:
  bool isBeforeInLockOrder(const TreeInode& a, const TreeInode& b) const;
  void lockSource(TreeInode& srcTree);
  void lockDestination(TreeInode& destTree);
  TreeInode* FOLLY_NULLABLE findDestChild(PathComponentPiece destName);
  void lockDestChild(TreeInode& destChildTree);

  /**
   * The mountpoint-wide rename lock.
   */
  RenameLock renameLock_;

  /**
   * Locks for the contents of the source and destination directories.
   * If the source and destination directories are the same, only
   * srcContentsLock_ is set.  However, srcContents_ and destContents_ above are
   * always both set, so that destContents_ can be used regardless of whether
   * the source and destination are both the same directory or not.
   */
  folly::Synchronized<TreeInodeState>::LockedPtr srcContentsLock_;
  folly::Synchronized<TreeInodeState>::LockedPtr destContentsLock_;
  folly::Synchronized<TreeInodeState>::LockedPtr destChildContentsLock_;

  /**
   * Pointers to the source and destination directory contents.
   *
   * These may both point to the same contents when the source and destination
   * directory are the same.
   */
  DirContents* srcContents_{nullptr};
  DirContents* destContents_{nullptr};
  DirContents* destChildContents_{nullptr};

  /**
   * An iterator pointing to the destination child entry in
   * destContents_->entries.
   * This may point to destContents_->entries.end() if the destination child
   * does not exist.
   */
  PathMap<DirEntry>::iterator destChildIter_;

  /**
   * A reference on the loaded destination child, if any.
   *
   * doRename() unlinks the destination child while destChildContentsLock_ is
   * still held on it. Once unlinked, the inode is destroyed by whoever drops
   * the last InodePtr, which may be another thread. This reference keeps it
   * alive until reset() has released every lock, so that its destruction,
   * which may do overlay I/O, happens outside them.
   */
  InodePtr destChildRef_;
};

ImmediateFuture<Unit> TreeInode::rename(
    PathComponentPiece name,
    TreeInodePtr destParent,
    PathComponentPiece destName,
    InvalidationRequired invalidate,
    const ObjectFetchContextPtr& context,
    bool noReplace) {
#ifndef _WIN32
  if (getNodeId() == getMount()->getDotEdenInodeNumber()) {
    return ImmediateFuture<Unit>{
        folly::Try<Unit>{InodeError{EPERM, inodePtrFromThis(), name}}};
  }
  if (destParent->getNodeId() == getMount()->getDotEdenInodeNumber()) {
    return ImmediateFuture<Unit>{
        folly::Try<Unit>{InodeError{EPERM, destParent, destName}}};
  }
#endif
  // Fail-fast ACL check on source parent. Uses raw lock access via
  // TreeRenameLocks. Destination parent is checked through
  // destParent->materialize() and TreeRenameLocks::acquireLocks(),
  // both of which call lockContentsWrite() -> checkAccess().
  checkAccess();
  validatePathComponentLength(destName);

  bool needSrc = false;
  bool needDest = false;
  {
    auto renameLock = getMount()->acquireRenameLock();
    materialize(&renameLock);
    if (destParent.get() != this) {
      destParent->materialize(&renameLock);
    }

    // Acquire the locks required to do the rename
    TreeRenameLocks locks;
    locks.acquireLocks(std::move(renameLock), *this, *destParent, destName);

    // Look up the source entry.  The destination entry info was already
    // loaded by TreeRenameLocks::acquireLocks().
    auto srcIter = locks.srcContents()->find(name);
    if (srcIter == locks.srcContents()->end()) {
      // The source path does not exist.  Fail the rename.
      return ImmediateFuture<Unit>{
          folly::Try<Unit>{InodeError{ENOENT, inodePtrFromThis(), name}}};
    }
    DirEntry& srcEntry = srcIter->second;

    if (noReplace && locks.destChildExists()) {
      return ImmediateFuture<Unit>{
          folly::Try<Unit>{InodeError{EEXIST, destParent, destName}}};
    }

    // Perform as much input validation as possible now, before starting inode
    // loads that might be necessary.

    // Validate invalid file/directory replacement
    if (srcEntry.isDirectory()) {
      // The source is a directory.
      // The destination must not exist, or must be an empty directory,
      // or the exact same directory.
      if (locks.destChildExists()) {
        if (!locks.destChildIsDirectory()) {
          XLOGF(
              DBG4,
              "attempted to rename directory {}/{} over file {}/{}",
              getLogPath(),
              name,
              destParent->getLogPath(),
              destName);
          return ImmediateFuture<Unit>{
              folly::Try<Unit>{InodeError{ENOTDIR, destParent, destName}}};
        }
        // An unloaded destination has no contents to inspect yet; it is
        // loaded below (needDest) and this check runs again on the retry.
        if (locks.destChild() != nullptr &&
            locks.destChild() != srcEntry.getInode() &&
            !locks.destChildIsEmpty()) {
          XLOGF(
              DBG4,
              "attempted to rename directory {}/{} over non-empty directory {}/{}",
              getLogPath(),
              name,
              destParent->getLogPath(),
              destName);
          return ImmediateFuture<Unit>{
              folly::Try<Unit>{InodeError{ENOTEMPTY, destParent, destName}}};
        }
      }
    } else {
      // The source is not a directory.
      // The destination must not exist, or must not be a directory.
      if (locks.destChildExists() && locks.destChildIsDirectory()) {
        XLOGF(
            DBG4,
            "attempted to rename file {}/{} over directory {}/{}",
            getLogPath(),
            name,
            destParent->getLogPath(),
            destName);
        return ImmediateFuture<Unit>{
            folly::Try<Unit>{InodeError{EISDIR, destParent, destName}}};
      }
    }

    // Make sure the destination directory is not unlinked.
    if (destParent->isUnlinked()) {
      XLOGF(
          DBG4,
          "attempted to rename file {}/{} into deleted directory {} ( as {} )",
          getLogPath(),
          name,
          destParent->getLogPath(),
          destName);
      return ImmediateFuture<Unit>{
          folly::Try<Unit>{InodeError{ENOENT, destParent}}};
    }

    // Check to see if we need to load the source or destination inodes
    needSrc = !srcEntry.getInode();
    needDest = locks.destChildExists() && !locks.destChild();

    // If we don't have to load anything now, we can immediately perform the
    // rename.
    if (!needSrc && !needDest) {
      return doRename(
          std::move(locks), name, srcIter, destParent, destName, invalidate);
    }

    // If we are still here we have to load either the source or destination,
    // or both.  Release the locks before we try loading them.
    //
    // (We could refactor getOrLoadChild() a little bit so that we could start
    // the loads with the locks still held, rather than releasing them just for
    // getOrLoadChild() to re-acquire them temporarily.  This isn't terribly
    // important for now, though.)
  }

  // Once we finish the loads, we have to re-run all the rename() logic.
  // Other renames or unlinks may have occurred in the meantime, so all of the
  // validation above has to be redone.
  //
  // Hold the loaded InodePtr(s) alive until the retried rename() completes.
  // Once InodeMap::shutdown() has begun, an unreferenced inode is unloaded as
  // soon as its pointer count reaches zero; dropping the pointer the load just
  // delivered would let the inode be unloaded before the retry reacquires the
  // locks, so the retry would reload and drop it again, livelocking the rename
  // against shutdown.
  auto onLoadFinished = [self = inodePtrFromThis(),
                         nameCopy = name.copy(),
                         destParent,
                         destNameCopy = destName.copy(),
                         invalidate,
                         noReplace,
                         context = context.copy()](InodePtr loaded) mutable {
    return self
        ->rename(
            nameCopy, destParent, destNameCopy, invalidate, context, noReplace)
        .ensure([loaded = std::move(loaded)] {});
  };

  if (needSrc && needDest) {
    auto srcFuture = getOrLoadChild(name, context);
    auto destFuture = destParent->getOrLoadChild(destName, context);

    return std::move(srcFuture).thenValue(
        [destFuture = std::move(destFuture),
         onLoadFinished = std::move(onLoadFinished)](InodePtr src) mutable {
          return std::move(destFuture)
              .thenValue(std::move(onLoadFinished))
              .ensure([src = std::move(src)] {});
        });
  } else if (needSrc) {
    return getOrLoadChild(name, context).thenValue(std::move(onLoadFinished));
  } else {
    XCHECK(needDest);
    return destParent->getOrLoadChild(destName, context)
        .thenValue(std::move(onLoadFinished));
  }
}

namespace {
bool isAncestor(const RenameLock& renameLock, TreeInode* a, TreeInode* b) {
  auto parent = b->getParent(renameLock);
  while (parent) {
    if (parent.get() == a) {
      return true;
    }
    parent = parent->getParent(renameLock);
  }
  return false;
}
} // namespace

/**
 * Order ancestors before descendants and keep disjoint subtrees together.
 * Compare the first differing ancestors, since preallocation and earlier
 * renames can give a child a lower inode number than its parent. The rename
 * lock keeps the hierarchy stable while comparing and acquiring locks.
 * The caller keeps both inodes alive through references or their parent's
 * contents lock; each inode in turn retains its ancestors. Do not create an
 * InodePtr from the destination child, which may have no pointer references.
 */
bool TreeInode::TreeRenameLocks::isBeforeInLockOrder(
    const TreeInode& a,
    const TreeInode& b) const {
  const auto getDepth = [this](const TreeInode& inode) {
    size_t depth = 0;
    for (auto parent = inode.getParent(renameLock_); parent;
         parent = parent->getParent(renameLock_)) {
      ++depth;
    }
    return depth;
  };

  const auto aDepth = getDepth(a);
  const auto bDepth = getDepth(b);
  const auto* aAncestor = &a;
  const auto* bAncestor = &b;
  for (auto depth = aDepth; depth > bDepth; --depth) {
    aAncestor = aAncestor->getParent(renameLock_).get();
  }
  for (auto depth = bDepth; depth > aDepth; --depth) {
    bAncestor = bAncestor->getParent(renameLock_).get();
  }
  if (aAncestor == bAncestor) {
    return aDepth < bDepth;
  }

  while (true) {
    auto* aParent = aAncestor->getParent(renameLock_).get();
    auto* bParent = bAncestor->getParent(renameLock_).get();
    if (aParent == bParent) {
      return aAncestor->getNodeId() < bAncestor->getNodeId();
    }
    aAncestor = aParent;
    bAncestor = bParent;
  }
}

ImmediateFuture<Unit> TreeInode::doRename(
    TreeRenameLocks&& locks,
    PathComponentPiece srcName,
    PathMap<DirEntry>::iterator srcIter,
    TreeInodePtr destParent,
    PathComponentPiece destName,
    InvalidationRequired invalidate) {
  DirEntry& srcEntry = srcIter->second;

  // If the source and destination refer to exactly the same file,
  // then just succeed immediately.  Nothing needs to be done in this case.
  // On a case-insensitive mount the destination lookup also finds the source
  // entry when only the spelling differs; that rename still has to change the
  // stored name.
  const bool sameEntry =
      locks.destChildExists() && srcEntry.getInode() == locks.destChild();
  if (sameEntry && destName.view() == locks.destChildIter()->first.view()) {
    return folly::unit;
  }

  // If we are doing a directory rename, sanity check that the destination
  // directory is not a child of the source directory.  The Linux kernel
  // generally should avoid invoking FUSE APIs with an invalid rename like
  // this, but we want to check in case rename() gets invoked via some other
  // non-FUSE mechanism.
  //
  // We don't have to worry about the source being a child of the destination
  // directory.  That will have already been caught by the earlier check that
  // ensures the destination directory is non-empty.
  if (srcEntry.isDirectory()) {
    // Our caller has already verified that the source is also a
    // directory here.
    auto* srcTreeInode =
        boost::polymorphic_downcast<TreeInode*>(srcEntry.getInode());
    if (srcTreeInode == destParent.get() ||
        isAncestor(locks.renameLock(), srcTreeInode, destParent.get())) {
      return ImmediateFuture<Unit>{
          folly::Try<Unit>{InodeError{EINVAL, destParent, destName}}};
    }
  }

  // If the rename occurred outside of a FUSE request (unlikely), make sure to
  // invalidate the kernel caches.
  if (InvalidationRequired::Yes == invalidate) {
    invalidateChannelEntryCache(
        locks.srcInodeState(), srcName, srcIter->second.getInodeNumber())
        .throwUnlessValue();
    destParent
        ->invalidateChannelEntryCache(
            locks.dstInodeState(), destName, std::nullopt)
        .throwUnlessValue();

    invalidateChannelDirCache(locks.srcInodeState()).get();
    if (destParent.get() != this) {
      destParent->invalidateChannelDirCache(locks.dstInodeState()).get();
    }
  }

  // Success.
  // Update the destination with the source data (this copies in the id if
  // it happens to be set).
  // The child is used after the contents locks are released below, when an
  // unload or FORGET could otherwise destroy it. Hold a reference until then.
  auto childInode = srcEntry.getInodePtr();
  const bool replacesDestChild = locks.destChildExists() && !sameEntry;
  if (sameEntry) {
    auto entry = std::move(srcIter->second);
    locks.destContents()->erase(srcIter);
    auto ret = locks.destContents()->emplace(destName, std::move(entry));
    XCHECK(ret.second);
  } else if (replacesDestChild) {
    // The reference held by locks keeps the destination alive, so
    // markUnlinked() never hands ownership back here; locks.reset() below
    // destroys the inode once every lock is released.
    locks.destChild()->markUnlinked(
        destParent.get(), destName, locks.renameLock());
    // Tests block here to drop their own references to the unlinked
    // destination while the rename still holds its contents lock.
    getMount()->getServerState()->getFaultInjector().check(
        "TreeInode::doRename", destName);

    // On a case-insensitive mount the existing entry may be spelled
    // differently from destName. The entry is re-inserted under destName so
    // the listing agrees with the child's location and the overlay record.
    auto entry = std::move(srcIter->second);
    locks.destContents()->erase(locks.destChildIter());
    auto ret = locks.destContents()->emplace(destName, std::move(entry));
    XCHECK(ret.second);
  } else {
    auto ret =
        locks.destContents()->emplace(destName, std::move(srcIter->second));
    XCHECK(ret.second);
  }

  // If the source and destination directory are the same, then modifying the
  // destination entries may have invalidated our source entry iterator, so we
  // have to look it up again.
  if (!sameEntry && destParent.get() == this) {
    srcIter = locks.srcContents()->find(srcName);
  }

  // Inform the child inode that it has been moved
  childInode->updateLocation(destParent, destName, locks.renameLock());

  // Now remove the source information
  if (!sameEntry) {
    locks.srcContents()->erase(srcIter);
  }

  auto now = getNow();
  updateMtimeAndCtimeLocked(*locks.srcContents(), now);
  if (destParent.get() != this) {
    destParent->updateMtimeAndCtimeLocked(*locks.destContents(), now);
  }

  getOverlay()->renameChild(
      getNodeId(),
      destParent->getNodeId(),
      srcName,
      destName,
      *locks.srcContents(),
      *locks.destContents());

  // Release the TreeInode locks before we write a journal entry.
  // We keep holding the mount point rename lock for now though.  This ensures
  // that rename and deletion events do show up in the journal in the correct
  // order.
  locks.releaseAllButRename();

  // Add a journal entry
  auto srcPath = getPath();
  auto destPath = destParent->getPath();
  if (srcPath.has_value() && destPath.has_value()) {
    if (replacesDestChild) {
      getMount()->getJournal().recordReplaced(
          srcPath.value() + srcName,
          destPath.value() + destName,
          childInode->getType());
    } else {
      getMount()->getJournal().recordRenamed(
          srcPath.value() + srcName,
          destPath.value() + destName,
          childInode->getType());
    }
  }

  // Releasing the locks also drops the reference the locks held on the
  // destination child, which destroys it now that it is unlinked.
  locks.reset();

  return folly::unit;
}

/**
 * Acquire the locks necessary for a rename operation.
 *
 * We acquire multiple locks here:
 *   A) Mountpoint rename lock
 *   B) Source directory contents_ lock
 *   C) Destination directory contents_ lock
 *   E) Destination child contents_ (assuming the destination name
 *      refers to an existing directory).
 *
 * This function ensures the locks are held with the proper ordering.
 * Since we hold the rename lock first, we can acquire multiple TreeInode
 * contents_ locks at once, but we must still ensure that we acquire locks on
 * ancestor TreeInodes before any of their descendants. Disjoint subtrees are
 * ordered by the inode numbers of their first differing ancestors.
 *
 * Moving directories can reverse the order of the same inode locks across
 * renames, so TSan may still report lock-order cycles. The mountpoint rename
 * lock serializes those operations, preventing them from deadlocking each
 * other. Ancestor-before-descendant ordering remains necessary to avoid
 * deadlocks with operations that do not acquire the rename lock.
 */
void TreeInode::TreeRenameLocks::acquireLocks(
    RenameLock&& renameLock,
    TreeInode& srcTree,
    TreeInode& destTree,
    PathComponentPiece destName) {
  // Store the mountpoint-wide rename lock.
  renameLock_ = std::move(renameLock);

  if (&srcTree == &destTree) {
    // If the source and destination directories are the same,
    // then there is really only one parent directory to lock.
    srcContentsLock_ = srcTree.lockContentsWrite();
    srcContents_ = &srcContentsLock_->entries;
    destContents_ = &srcContentsLock_->entries;
    // Look up the destination child entry, and lock it if it is a directory
    if (auto* destChildTree = findDestChild(destName)) {
      lockDestChild(*destChildTree);
    }
    return;
  }

  if (isBeforeInLockOrder(srcTree, destTree)) {
    lockSource(srcTree);
    lockDestination(destTree);
    if (auto* destChildTree = findDestChild(destName)) {
      lockDestChild(*destChildTree);
    }
    return;
  }

  lockDestination(destTree);
  auto* destChildTree = findDestChild(destName);
  if (destChildTree == nullptr) {
    lockSource(srcTree);
    return;
  }
  if (destChildTree == &srcTree) {
    lockDestChild(*destChildTree);
    XCHECK_NE(destChildContents_, nullptr);
    srcContents_ = destChildContents_;
    return;
  }

  if (isBeforeInLockOrder(*destChildTree, srcTree)) {
    lockDestChild(*destChildTree);
    lockSource(srcTree);
  } else {
    lockSource(srcTree);
    lockDestChild(*destChildTree);
  }
}

void TreeInode::TreeRenameLocks::lockSource(TreeInode& srcTree) {
  srcContentsLock_ = srcTree.lockContentsWrite();
  srcContents_ = &srcContentsLock_->entries;
}

void TreeInode::TreeRenameLocks::lockDestination(TreeInode& destTree) {
  destContentsLock_ = destTree.lockContentsWrite();
  destContents_ = &destContentsLock_->entries;
}

TreeInode* FOLLY_NULLABLE
TreeInode::TreeRenameLocks::findDestChild(PathComponentPiece destName) {
  // Look up the destination child entry
  destChildIter_ = destContents_->find(destName);
  if (!destChildExists() || destChild() == nullptr) {
    return nullptr;
  }
  destChildRef_ = InodePtr::newPtrLocked(destChild());
  if (destChildIsDirectory()) {
    return boost::polymorphic_downcast<TreeInode*>(destChild());
  }
  return nullptr;
}

void TreeInode::TreeRenameLocks::lockDestChild(TreeInode& destChildTree) {
  destChildContentsLock_ = destChildTree.lockContentsWrite();
  destChildContents_ = &destChildContentsLock_->entries;
}

namespace {

/**
 * Index the entries past `minOffset` by inode number. A request building an
 * index for itself passes its own offset, so the request that ends a
 * listing, which finds nothing past its offset, sorts nothing and allocates
 * nothing.
 *
 * Restricted entries are indexed too. Granting access to one clears its
 * restricted bit in place, which leaves entries.mutationCount() alone, so an
 * index that left the entry out would keep hiding it from listings that
 * should now show it.
 */
ReaddirIndex buildReaddirIndex(const DirEntries& entries, off_t minOffset) {
  ReaddirIndex index{entries.mutationCount(), minOffset, {}};
  if (minOffset == 0) {
    index.entries.reserve(entries.size());
  }
  for (const auto& mapEntry : entries.all()) {
    const auto inodeNumber = mapEntry.second.getInodeNumber();
    if (static_cast<off_t>(inodeNumber.get() + 2) > minOffset) {
      index.entries.emplace_back(inodeNumber, &mapEntry);
    }
  }
  std::sort(
      index.entries.begin(),
      index.entries.end(),
      [](const auto& a, const auto& b) { return a.first < b.first; });
  return index;
}

/**
 * A cached index serves a request at `offset` only while the map has not
 * been mutated since the index was built (its entry pointers would
 * otherwise dangle) and it covers everything past `offset`.
 */
bool isReaddirIndexCurrent(
    const ReaddirIndex& index,
    const TreeInodeState& state,
    off_t offset) {
  return index.mutationCount == state.entries.mutationCount() &&
      offset >= index.minOffset;
}

/**
 * The first indexed entry a request at `offset` lists. Omitted entries are
 * skipped here as well as during emission, so that a request with nothing
 * but omitted entries left reaches the end of the index and is recognized as
 * the end of the listing.
 */
auto firstIndexedAfter(
    const ReaddirIndex& index,
    RestrictedContentMode mode,
    off_t offset) {
  auto it = std::lower_bound(
      index.entries.begin(),
      index.entries.end(),
      offset,
      [](const auto& indexed, off_t off) {
        return static_cast<off_t>(indexed.first.get() + 2) <= off;
      });
  if (mode == RestrictedContentMode::Omitted) {
    it = std::find_if(it, index.entries.end(), [](const auto& indexed) {
      return !indexed.second->second.isRestricted();
    });
  }
  return it;
}

/**
 * Emit the indexed entries from `it` on, in offset order, until `add`
 * declines one. Returns whether the index was exhausted.
 */
template <typename Iter, typename Fn>
bool emitReaddirIndex(
    const ReaddirIndex& index,
    RestrictedContentMode mode,
    Iter it,
    Fn& add) {
  const bool omit = mode == RestrictedContentMode::Omitted;
  for (; it != index.entries.end(); ++it) {
    const auto& [name, entry] = *it->second;
    XDCHECK(entry.getInodeNumber() == it->first);
    if (omit && entry.isRestricted()) {
      continue;
    }
    if (!add(name.view(), entry, static_cast<off_t>(it->first.get() + 2))) {
      return false;
    }
  }
  return true;
}

} // namespace

template <typename Fn>
bool TreeInode::readdirImpl(
    off_t off,
    const ObjectFetchContextPtr& context,
    Fn add) {
  /*
   * Implementing readdir correctly in the presence of concurrent modifications
   * to the directory is nontrivial. This function will be called multiple
   * times. The off_t value given is either 0, on the first read, or the value
   * corresponding to the last entry's offset. (Or an arbitrary entry's offset
   * value, given seekdir and telldir).
   *
   * POSIX compliance requires that, given a sequence of readdir calls across
   * an entire directory stream, each unmodified entry is returned exactly
   * once. Entries that are added or removed between readdir calls may be
   * returned, but don't have to be.
   *
   * Thus, off_t as an index into an ordered list of entries is not sufficient.
   * If an entry is unlinked, the next readdir will skip entries.
   *
   * One option might be to populate off_t with a hash of the entry name. off_t
   * has 63 usable bits (minus the 0 value which is reserved for the initial
   * request). 63 bits of SpookyHashV2 is probably sufficient in practice, but
   * it would be possible to create a directory containing collisions, causing
   * duplicate entries or an infinite loop. Also it's unclear how to handle
   * the entry at `off` being removed before the next readdir. (How do you find
   * where to restart in the stream?).
   *
   * Today, Eden does not support hard links. Therefore, in the short term, we
   * can store inode numbers in off_t and treat them as an index into an
   * inode-sorted list of entries. Building that list costs a sort of the
   * whole directory, so a listing that spans several requests caches it in
   * TreeInode::readdirIndex_ for the later ones and drops it once the
   * listing ends; the map's mutation count says whether it is still valid.
   *
   * In the long term, especially when Eden's tree directory structure is stored
   * in SQLite or something similar, we should maintain a seekdir/readdir cookie
   * index and use said cookies to enumerate entries.
   *
   * - https://oss.oracle.com/pipermail/btrfs-devel/2008-January/000463.html
   * - https://yarchive.net/comp/linux/readdir_nonatomicity.html
   * - https://lwn.net/Articles/544520/
   */
  if (off < 0) {
    XLOGF(ERR, "Negative readdir offsets are illegal, off = {}", off);
    folly::throwSystemErrorExplicit(EINVAL);
  }

  recheckPermissionIfExpired(context).get();

  updateAtime();

  // Possible offset values are:
  //   0: start at the beginning
  //   1: start after .
  //   2: start after ..
  //   2+N: start after inode N

  if (off == 0) {
    if (!add(".", DirEntry{dtype_to_mode(dtype_t::Dir), getNodeId()}, 1)) {
      return false;
    }
  }
  if (off <= 1) {
    // It's okay to query the parent without the rename lock held because, if
    // readdir is racing with rename, the results are unspecified anyway.
    // http://pubs.opengroup.org/onlinepubs/007908799/xsh/readdir.html
    auto parent = getParentRacy();
    // For the root of the mount point, just add its own inode ID as its parent.
    // FUSE seems to overwrite the parent inode number on the root dir anyway.
    auto parentNodeId = parent ? parent->getNodeId() : getNodeId();
    if (!add("..", DirEntry{dtype_to_mode(dtype_t::Dir), parentNodeId}, 2)) {
      return false;
    }
  }

  // Offsets are ino + 2, not list positions, so omitting an entry never
  // shifts the offsets of the rest across resumed readdir calls.
  const auto mode = restrictedContentMode();
  const bool cacheIndex =
      getMount()->getEdenConfig()->experimentalReaddirIndexCache.getValue();
  const auto& stats = getMount()->getStats();

  auto dir = lockContentsRead();
  std::shared_ptr<const ReaddirIndex> cached;
  if (cacheIndex) {
    // Building the index costs a sort of the whole directory, so a listing
    // that takes several requests shares one index between them. The read
    // lock held here keeps entries.mutationCount() stable, so an index that
    // matches it can be used for the rest of this request.
    cached = readdirIndex_.copy();
    if (cached && isReaddirIndexCurrent(*cached, *dir, off)) {
      stats->increment(&TreeInodeStats::readdirIndexHit);
      const auto first = firstIndexedAfter(*cached, mode, off);
      if (first == cached->entries.end()) {
        // Nothing left to list past `off`: the listing is over.
        readdirIndex_.wlock()->reset();
        return true;
      }
      return emitReaddirIndex(*cached, mode, first, add);
    }
  }

  // Only entries past `off` are indexed, so emission starts at the front.
  auto index = buildReaddirIndex(dir->entries, off);
  const bool finished =
      emitReaddirIndex(index, mode, index.entries.begin(), add);
  if (cacheIndex) {
    if (!finished) {
      // More requests will follow; let them reuse this index.
      *readdirIndex_.wlock() =
          std::make_shared<const ReaddirIndex>(std::move(index));
      stats->increment(&TreeInodeStats::readdirIndexCached);
    } else if (cached) {
      // The cached index failed isReaddirIndexCurrent above and this listing
      // is over, so nothing will use it again.
      readdirIndex_.wlock()->reset();
    }
  }
  return finished;
}

#ifndef _WIN32
FuseDirList TreeInode::fuseReaddir(
    FuseDirList&& list,
    off_t off,
    const ObjectFetchContextPtr& context) {
  readdirImpl(
      off,
      context,
      [&list](StringPiece name, const DirEntry& entry, uint64_t offset) {
        return list.add(
            name, entry.getInodeNumber().get(), entry.getDtype(), offset);
      });

  return std::move(list);
}

#endif // _WIN32

std::tuple<NfsDirList, bool> TreeInode::nfsReaddir(
    NfsDirList&& list,
    off_t off,
    const ObjectFetchContextPtr& context) {
  updateAtime();
  bool isEof = readdirImpl(
      off,
      context,
      [&list](StringPiece name, const DirEntry& entry, uint64_t offset) {
        return list.add(name, entry.getInodeNumber(), offset);
      });
  if (isEof) {
    // NFS clients stop at eof without the empty request that ends a FUSE
    // listing, so this is where a cached index is dropped.
    readdirIndex_.wlock()->reset();
  }

  return {std::move(list), isEof};
}

InodeMap* TreeInode::getInodeMap() const {
  return getMount()->getInodeMap();
}

std::weak_ptr<InodeMap> TreeInode::getInodeMapWeak() const {
  return getMount()->getInodeMapWeak();
}

/*
On each level of the level order traversal, we search for a gitignore file, and
if it exists, we load it. This gitignore file is owned by a `std::<unique_ptr>`
on each level, and the file contents are stored within a `GitIgnoreStack`. Lower
levels of the tree are passed raw pointers to their parent's `GitIgnoreStack`,
and store this pointer after loading the file contents of that level. In other
words, a `GitIgnoreStack` contains a `GitIgnoreStack*` to the parent's
`GitIgnoreStack`, and the current level's gitignore file contents (if a
gitignore file exists). We are allowed to pass raw pointers derived from the
`std::<unique_ptr>` because the raw pointers are passed only to the childrens'
recursive calls and we are sure that the children calls will finish before we
return from the parent call.
*/
ImmediateFuture<Unit> TreeInode::diff(
    DiffContext* context,
    RelativePathPiece currentPath,
    std::vector<shared_ptr<const Tree>> trees,
    const GitIgnoreStack* parentIgnore,
    bool isIgnored) {
  return ImmediateFuture{
      // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
      folly::coro::co_invoke(
          [self = inodePtrFromThis()](
              DiffContext* context,
              RelativePath currentPath,
              std::vector<shared_ptr<const Tree>> trees,
              const GitIgnoreStack* parentIgnore,
              bool isIgnored) -> folly::coro::Task<Unit> {
            co_return co_await self->co_diff(
                context,
                currentPath,
                std::move(trees),
                parentIgnore,
                isIgnored);
          },
          context,
          currentPath.copy(),
          std::move(trees),
          parentIgnore,
          isIgnored)
          .semi()};
}

folly::coro::now_task<folly::Unit> TreeInode::co_diff(
    DiffContext* context,
    RelativePathPiece currentPath,
    std::vector<shared_ptr<const Tree>> trees,
    const GitIgnoreStack* parentIgnore,
    bool isIgnored) {
  auto self = inodePtrFromThis();
  if (context->isCancelled()) {
    XLOGF(
        DBG7,
        "diff() on directory {} cancelled due to client request no longer being active",
        getLogPath());
    co_return folly::unit;
  }

  // Status is a use of this directory as far as inode GC is concerned, so a
  // directory it keeps walking stays loaded like one the kernel keeps using.
  markGcRef();

  InodePtr inode;
  auto gitignoreInodeFuture = ImmediateFuture<InodePtr>::makeEmpty();
  vector<IncompleteInodeLoad> pendingLoads;
  {
    auto contents = lockContentsWrite();

    XLOGF(
        DBG7,
        "diff() on directory {} ({}, {}) vs {}",
        getLogPath(),
        getNodeId(),
        (contents->isMaterialized() ? "materialized"
                                    : contents->treeId->toLogString()),
        (trees.size() == 1 ? trees[0]->getObjectId().toLogString()
                           : "null tree"));

    if (!contents->isMaterialized()) {
      for (auto& tree : trees) {
        if (getObjectStore().areObjectsKnownIdentical(
                contents->treeId.value(), tree->getObjectId())) {
          co_return folly::unit;
        }
      }
    }

    if (isIgnored) {
      co_await co_computeDiff(
          std::move(contents),
          context,
          currentPath,
          std::move(trees),
          nullptr,
          isIgnored);
      co_return folly::unit;
    }

    DirEntry* gitignoreEntry = nullptr;
    auto iter = contents->entries.find(kIgnoreFilename);
    if (iter != contents->entries.end()) {
      gitignoreEntry = &iter->second;
      if (gitignoreEntry->isDirectory()) {
        XLOGF(DBG4, "Ignoring .gitignore directory in {}", getLogPath());
        gitignoreEntry = nullptr;
      }
    }

    if (!gitignoreEntry) {
      co_await co_computeDiff(
          std::move(contents),
          context,
          currentPath,
          std::move(trees),
          make_unique<GitIgnoreStack>(parentIgnore),
          isIgnored);
      co_return folly::unit;
    }

    XLOGF(DBG7, "Loading ignore file for {}", getLogPath());
    inode = gitignoreEntry->getInodePtr();
    if (!inode) {
      gitignoreInodeFuture = loadChildLocked(
          kIgnoreFilename,
          *gitignoreEntry,
          pendingLoads,
          context->getFetchContext());
    }
  }

  for (auto& load : pendingLoads) {
    load.finish();
  }

  if (!inode) {
    inode = co_await std::move(gitignoreInodeFuture).semi();
  }

  co_await co_loadGitIgnoreThenDiff(
      std::move(inode),
      context,
      currentPath,
      std::move(trees),
      parentIgnore,
      isIgnored);
  co_return folly::unit;
}

folly::coro::now_task<folly::Unit> TreeInode::co_loadGitIgnoreThenDiff(
    InodePtr gitignoreInode,
    DiffContext* context,
    RelativePathPiece currentPath,
    std::vector<shared_ptr<const Tree>> trees,
    const GitIgnoreStack* parentIgnore,
    bool isIgnored) {
  // prevent destruction across co_await suspension points
  auto self = inodePtrFromThis();
  auto loadGitIgnoreThenDiffSpan = context->createSpan("loadGitIgnoreThenDiff");
  std::string ignoreFileContents;
  try {
    auto fileInode = gitignoreInode.asFileOrNull();
    if (!fileInode) {
      XLOGF(
          WARN,
          "co_loadGitIgnoreThenDiff() invoked with a non-file inode: {}",
          gitignoreInode->getLogPath());
      throw InodeError(EISDIR, gitignoreInode);
    }
#ifndef _WIN32
    if (fileInode->getType() == dtype_t::Symlink) {
      throw InodeError(EMLINK, gitignoreInode);
    }
#endif
    ignoreFileContents =
        co_await fileInode->co_readAll(context->getFetchContext());
  } catch (const std::exception& ex) {
    XLOGF(WARN, "error reading ignore file: {}", folly::exceptionStr(ex));
  }

  co_await co_computeDiff(
      lockContentsWrite(),
      context,
      currentPath,
      std::move(trees),
      make_unique<GitIgnoreStack>(parentIgnore, ignoreFileContents),
      isIgnored);
  co_return folly::unit;
}

/*
This algorithm starts at the root `TreeInode` of the working directory and the
root source control commit `Tree`, traversing the trees in a level order
traversal. Per level of the tree, we loop over the children entries and
recursively process each entry as either added, removed, modified, ignored, or
hidden. We can also recognize that there has been no change and skip over that
child. We process children by constructing and collecting `DeferredDiffEntry`
objects using the children `TreeInode` objects and manually `run()`ning these.
If we are processing a file, we record this
added/removed/modified/ignored/hidden state in a callback, and extract this
collected information after the original `diff()` call has been completed.

In the case in which the working directory entry is not materialized, but it has
possibly been modified (for example, if a unmaterialized directory was moved, or
if we're calling diff with a source control commit that is far away from the
working directory parent), we can make an optimization. Since unmaterialized
inode entries still hold their commit hash, we can directly compare the working
directory entry's corresponding source control entry with the queried source
control commit's entry instead of materializing the inode entry to continue
through the working directory vs source control commit logic. We do this by
entering a different code path that acts similarly to the previously described
algorithm, but runs purely recursively instead of using `DeferredDiffEntries`
due to the fact that we do not need to worry about recursive lock holding since
the only time a lock is held in this path is when we load gitignore files.
*/
std::vector<std::unique_ptr<DeferredDiffEntry>>
TreeInode::prepareDeferredDiffEntries(
    folly::Synchronized<TreeInodeState>::LockedPtr contentsLock,
    DiffContext* context,
    RelativePathPiece currentPath,
    const std::vector<shared_ptr<const Tree>>& trees,
    GitIgnoreStack* ignore,
    bool isIgnored) {
  XDCHECK(isIgnored || ignore != nullptr)
      << "the ignore stack is required if this directory is not ignored";

  std::vector<std::unique_ptr<DeferredDiffEntry>> deferredEntries;
  auto self = inodePtrFromThis();
  // Grab the contents_ lock, and loop to find children that might be
  // different.  In this first pass we primarily build the list of children to
  // examine, but we wait until after we release our contents_ lock to actually
  // examine any children InodeBase objects.
  std::vector<IncompleteInodeLoad> pendingLoads;
  {
    // Move the contents lock into a variable inside this scope so it
    // will be released at the end of this scope.
    //
    // Even though diffing conceptually seems like a read-only operation, we
    // need a write lock since we may have to load child inodes, affecting
    // their entry state.
    //
    // This lock is held while we are processing the child
    // entries of the tree. The lock is released once we have constructed all of
    // the needed `DeferredDiffEntry` objects, and then these are manually ran,
    // which themselves call `diff()` with their internal `TreeInode`. This is
    // done so we are not recursively holding the `contents` lock, which could
    // lead to deadlock.
    auto contents = std::move(contentsLock);

    auto processUntracked = [&](PathComponentPiece name, DirEntry* inodeEntry) {
      auto processUntrackedSpan = context->createSpan("processUntracked");

      bool entryIgnored = isIgnored;
      auto fileType = inodeEntry->isDirectory() ? GitIgnore::TYPE_DIR
                                                : GitIgnore::TYPE_FILE;
      auto entryPath = currentPath + name;
      if (!isIgnored) {
        auto ignoreStatus =
            ignore->match(entryPath, fileType, context->getGlobMatchOptions());
        if (ignoreStatus == GitIgnore::HIDDEN) {
          // Completely skip over hidden entries.
          // This is used for reserved directories like .hg and .eden
          XLOGF(DBG9, "diff: hidden entry: {}", entryPath);
          return;
        }
        entryIgnored = (ignoreStatus == GitIgnore::EXCLUDE);
      }

      if (!entryIgnored) {
        XLOGF(DBG8, "diff: untracked file: {}", entryPath);
        context->callback->addedPath(entryPath, inodeEntry->getDtype());
      } else if (context->listIgnored) {
        XLOGF(DBG9, "diff: ignored file: {}", entryPath);
        context->callback->ignoredPath(entryPath, inodeEntry->getDtype());
      } else {
        // Don't bother reporting this ignored file since
        // listIgnored is false.
      }

      if (inodeEntry->isDirectory()) {
        if (!entryIgnored || context->listIgnored) {
          if (auto childPtr = inodeEntry->getInodePtr()) {
            deferredEntries.emplace_back(
                DeferredDiffEntry::createUntrackedEntry(
                    context,
                    entryPath,
                    std::move(childPtr),
                    ignore,
                    entryIgnored));
          } else if (inodeEntry->isMaterialized()) {
            auto loadChildLockedSpan = context->createSpan("loadChildLocked");
            auto inodeFuture =
                self->loadChildLocked(
                        name,
                        *inodeEntry,
                        pendingLoads,
                        context->getFetchContext())
                    .ensure([span = std::move(loadChildLockedSpan)]() {});
            deferredEntries.emplace_back(
                DeferredDiffEntry::createUntrackedEntry(
                    context,
                    entryPath,
                    std::move(inodeFuture),
                    ignore,
                    entryIgnored));
          } else {
            // This entry is present locally but not in the source control tree.
            // The current Inode is not materialized so do not load inodes and
            // instead use the source control differ.

            // Collect this future to complete with other
            // deferred entries.
            deferredEntries.emplace_back(
                DeferredDiffEntry::createAddedScmEntry(
                    context, entryPath, inodeEntry->getObjectId()));
          }
        }
      }
    };

    auto processRemoved = [&](const Tree::value_type& scmEntry) {
      XLOGF(DBG5, "diff: removed file: {}", currentPath + scmEntry.first);
      context->callback->removedPath(
          currentPath + scmEntry.first, scmEntry.second.getDtype());
      if (scmEntry.second.isTree()) {
        deferredEntries.emplace_back(
            DeferredDiffEntry::createRemovedScmEntry(
                context,
                currentPath + scmEntry.first,
                scmEntry.second.getObjectId()));
      }
    };

    auto processBothPresent = [&](PathComponentPiece componentPath,
                                  std::vector<TreeEntry> scmEntries,
                                  DirEntry* inodeEntry) {
      auto processBothPresentSpan = context->createSpan("processBothPresent");

      XCHECK_GT(scmEntries.size(), 0ull);

      // We only need to know the ignored status if this is a directory.
      // If this is a regular file on disk and in source control, then it
      // is always included since it is already tracked in source control.
      bool entryIgnored = isIgnored;
      auto entryPath = currentPath + componentPath;
      if (!isIgnored && (inodeEntry->isDirectory() || scmEntries[0].isTree())) {
        auto fileType = inodeEntry->isDirectory() ? GitIgnore::TYPE_DIR
                                                  : GitIgnore::TYPE_FILE;
        auto ignoreStatus =
            ignore->match(entryPath, fileType, context->getGlobMatchOptions());
        if (ignoreStatus == GitIgnore::HIDDEN) {
          // This is rather unexpected.  We don't expect to find entries in
          // source control using reserved hidden names.
          // Treat this as ignored for now.
          entryIgnored = true;
        } else if (ignoreStatus == GitIgnore::EXCLUDE) {
          entryIgnored = true;
        } else {
          entryIgnored = false;
        }
      }

      if (inodeEntry->getInode()) {
        // This inode is already loaded.
        auto childInodePtr = inodeEntry->getInodePtr();
        deferredEntries.emplace_back(
            DeferredDiffEntry::createModifiedEntry(
                context,
                entryPath,
                std::move(scmEntries),
                std::move(childInodePtr),
                ignore,
                entryIgnored));
      } else if (inodeEntry->isMaterialized()) {
        // This inode is not loaded but is materialized.
        // We'll have to load it to confirm if it is the same or different.
        auto loadChildLockedSpan = context->createSpan("loadChildLocked");
        auto inodeFuture =
            self->loadChildLocked(
                    componentPath,
                    *inodeEntry,
                    pendingLoads,
                    context->getFetchContext())
                .ensure([span = std::move(loadChildLockedSpan)]() {});
        deferredEntries.emplace_back(
            DeferredDiffEntry::createModifiedEntry(
                context,
                entryPath,
                std::move(scmEntries),
                std::move(inodeFuture),
                ignore,
                entryIgnored));
      } else {
        // If the inode is neither loaded nor materialized, then the inode
        // points at source control objects. At this point we check to see if
        // it's an exact match with any source control object. Otherwise we
        // just mark it as a diff against the first object we have.
        bool exactMatch = false;
        for (const auto& scmEntry : scmEntries) {
          if (
              // Eventually the mode will come from inode metadata storage,
              // not from the directory entry.  However, any
              // source-control-visible metadata changes will cause the inode to
              // be materialized, and the previous path will be taken.
              //
              // On Windows, ignore executable type for comparison
              compareTreeEntryType(
                  treeEntryTypeFromMode(inodeEntry->getInitialMode()),
                  scmEntry.getType()) &&
              getObjectStore().areObjectsKnownIdentical(
                  inodeEntry->getObjectId(), scmEntry.getObjectId())) {
            exactMatch = true;
            break;
          }
        }

        const auto& scmEntry = scmEntries[0];

        if (exactMatch) {
          // This file or directory is unchanged.  We can skip it.
          XLOGF(DBG9, "diff: unchanged unloaded file: {}", entryPath);
        } else if (inodeEntry->isDirectory() && scmEntry.isTree()) {
          // This is a modified directory. Since it is not materialized we can
          // directly compare the source control objects.

          context->callback->modifiedPath(entryPath, inodeEntry->getDtype());
          // Collect this future to complete with other deferred entries.
          deferredEntries.emplace_back(
              DeferredDiffEntry::createModifiedScmEntry(
                  context,
                  entryPath,
                  scmEntry.getObjectId(),
                  inodeEntry->getObjectId()));
        } else if (inodeEntry->isDirectory()) {
          if (entryIgnored) {
            if (context->listIgnored) {
              XLOGF(DBG6, "diff: file --> ignored directory: {}", entryPath);
              context->callback->ignoredPath(entryPath, inodeEntry->getDtype());
            }
          } else {
            XLOGF(DBG6, "diff: file --> untracked directory: {}", entryPath);
            context->callback->addedPath(entryPath, inodeEntry->getDtype());
          }
          context->callback->removedPath(entryPath, scmEntry.getDtype());
          if (!entryIgnored || context->listIgnored) {
            deferredEntries.emplace_back(
                DeferredDiffEntry::createAddedScmEntry(
                    context, entryPath, inodeEntry->getObjectId()));
          }
        } else if (scmEntry.isTree()) {
          // This used to be a directory in the source control state,
          // but is now a file or symlink.  Report the new file, then add a
          // deferred entry to report the entire source control Tree as
          // removed.
          if (entryIgnored) {
            if (context->listIgnored) {
              XLOGF(DBG6, "diff: directory --> ignored file: {}", entryPath);
              context->callback->ignoredPath(entryPath, inodeEntry->getDtype());
            }
          } else {
            XLOGF(DBG6, "diff: directory --> untracked file: {}", entryPath);
            context->callback->addedPath(entryPath, inodeEntry->getDtype());
          }
          context->callback->removedPath(entryPath, scmEntry.getDtype());
          deferredEntries.emplace_back(
              DeferredDiffEntry::createRemovedScmEntry(
                  context, entryPath, scmEntry.getObjectId()));
        } else {
          // This file corresponds to a different blob id, or has a
          // different mode.
          //
          // Ideally we should be able to assume that the file is
          // modified--if two blobs have different ids we should be able
          // to assume that their contents are different.  Unfortunately this
          // is not the case for now with our mercurial blob IDs, since the
          // mercurial blob data includes the path name and past history
          // information.
          //
          // TODO: Once we build a new backing store and can replace our
          // janky hashing scheme for mercurial data, we should be able just
          // immediately assume the file is different here, without checking.
          //
          // On Windows: ignore executable type for comparison.
          if (!compareTreeEntryType(
                  treeEntryTypeFromMode(inodeEntry->getInitialMode()),
                  scmEntry.getType())) {
            // The mode is definitely modified
            XLOGF(
                DBG5, "diff: file modified due to mode change: {}", entryPath);
            context->callback->modifiedPath(entryPath, inodeEntry->getDtype());
          } else {
            // TODO: Hopefully at some point we will track file sizes in the
            // parent TreeInode::Entry and the TreeEntry.  Once we have file
            // sizes, we could check for differing file sizes first, and
            // avoid loading the blob if they are different.
            deferredEntries.emplace_back(
                DeferredDiffEntry::createModifiedEntry(
                    context,
                    entryPath,
                    scmEntry,
                    inodeEntry->getObjectId(),
                    inodeEntry->getDtype()));
          }
        }
      }
    };

    // Walk through the source control tree entries and our inode entries to
    // look for differences.
    //
    // This code relies on the fact that the source control entries and our
    // inode entries are both sorted in the same order.
    std::vector<Tree::const_iterator> scEnds;
    std::vector<Tree::const_iterator> scIters;
    scEnds.reserve(trees.size());
    scIters.reserve(trees.size());

    for (auto& tree : trees) {
      scEnds.push_back(tree->cend());
      scIters.push_back(tree->cbegin());
    }
    auto& inodeEntries = contents->entries.all();
    auto inodeIter = inodeEntries.begin();
    while (true) {
      context->throwIfCanceled();

      const Tree::key_type* earliestPath =
          inodeIter != inodeEntries.end() ? &inodeIter->first : nullptr;
      DirContents::iterator* matchingInodeIter =
          inodeIter != inodeEntries.end() ? &inodeIter : nullptr;

      std::vector<Tree::const_iterator*> matchingScIters;

      // Find the earliest path in all the iterators, and record which iterators
      // have that path.
      for (size_t i = 0; i < scEnds.size(); i++) {
        auto& scEnd = scEnds[i];
        auto& scIter = scIters[i];
        if (scIter != scEnd) {
          if (!earliestPath) {
            earliestPath = &scIter->first;
            matchingScIters.push_back(&scIter);
          } else {
            auto compare = comparePathPiece(
                scIter->first, *earliestPath, context->getCaseSensitive());

            if (compare == CompareResult::BEFORE) {
              // If we find an earlier path, reset our current state.
              earliestPath = &scIter->first;
              matchingInodeIter = nullptr;
              matchingScIters.clear();
              matchingScIters.push_back(&scIter);
            } else if (compare == CompareResult::AFTER) {
              // If we find a later path, ignore it.
            } else {
              // If the path matches the earliest path we've seen, record it.
              matchingScIters.push_back(&scIter);
            }
          }
        }
      }

      // If there are no matches, then we've finished the entire walk.
      if (!matchingInodeIter && matchingScIters.empty()) {
        break;
      }

      if (!matchingInodeIter) { // If the inode doesn't have this path...
        if (matchingScIters.size() == scIters.size()) { // ...but all trees do..
          // ...then this entry is considered removed.
          processRemoved(**matchingScIters[0]);
        } else { // ...but not all trees do...
          // ...then this entry is considered unchanged, since some tree matches
          // the inode.
        }
      } else { // If the inode has this path...
        if (matchingScIters.empty()) { // ...but no trees do...
          // ...then the entry is considered untracked.
          processUntracked(inodeIter->first, &inodeIter->second);
        } else { // ...and some trees do as well...
          // ...then we need to compare this entry with the trees that have it.
          std::vector<TreeEntry> matchingTrees{};
          matchingTrees.reserve(matchingScIters.size());
          for (auto& scIter : matchingScIters) {
            matchingTrees.push_back((*scIter)->second);
          }
          processBothPresent(
              inodeIter->first, matchingTrees, &inodeIter->second);
        }
      }

      // Move everything forward that had the earliest path.
      if (matchingInodeIter) {
        ++(*matchingInodeIter);
      }
      for (auto& scIter : matchingScIters) {
        ++(*scIter);
      }
    }
  }

  // Finish setting up any load operations we started while holding the
  // contents_ lock above.
  for (auto& load : pendingLoads) {
    load.finish();
  }

  return deferredEntries;
}

folly::coro::now_task<folly::Unit> TreeInode::co_computeDiff(
    folly::Synchronized<TreeInodeState>::LockedPtr contentsLock,
    DiffContext* context,
    RelativePathPiece currentPath,
    std::vector<shared_ptr<const Tree>> trees,
    std::unique_ptr<GitIgnoreStack> ignore,
    bool isIgnored) {
  auto self = inodePtrFromThis();
  auto computeDiffSpan = context->createSpan("computeDiff");

  auto deferredEntries = prepareDeferredDiffEntries(
      std::move(contentsLock),
      context,
      currentPath,
      trees,
      ignore.get(),
      isIgnored);

  std::vector<folly::coro::Task<folly::Unit>> deferredTasks;
  deferredTasks.reserve(deferredEntries.size());
  for (auto& entry : deferredEntries) {
    deferredTasks.push_back(
        folly::coro::co_invoke(
            [entryPtr = entry.get()]() -> folly::coro::Task<folly::Unit> {
              co_return co_await entryPtr->co_run();
            }));
  }

  co_await getMount()->getServerState()->getFaultInjector().co_checkAsync(
      "TreeInode::computeDiff", currentPath.view());

  auto results =
      co_await folly::coro::collectAllTryRange(std::move(deferredTasks));

  // Call diffError() for any jobs that failed.
  for (size_t n = 0; n < results.size(); ++n) {
    auto& result = results[n];
    if (result.hasException()) {
      XLOGF(
          WARN,
          "exception processing diff for {}: {}",
          deferredEntries[n]->getPath(),
          folly::exceptionStr(result.exception()));
      context->callback->diffError(
          deferredEntries[n]->getPath(), result.exception());
    }
  }
  co_return folly::unit;
}

struct TreeInode::CheckoutSetup {
  std::optional<MiniTracer::Span> checkoutSpan;
  std::vector<std::shared_ptr<CheckoutAction>> actions;
  bool shouldInvalidateDirectory{false};
  bool hadConflicts{false};
  bool localOnlyRemains{false};
};

struct TreeInode::CheckoutFinalizeState {
  bool shouldInvalidateDirectory{false};
  bool hadConflicts{false};
  bool localOnlyRemains{false};
};

TreeInode::CheckoutSetup TreeInode::beginCheckout(
    CheckoutContext* ctx,
    const std::shared_ptr<const Tree>& fromTree,
    const std::shared_ptr<const Tree>& toTree,
    bool reportLocalOnlyAsConflicts,
    bool removeLocalOnly) {
  XLOGF(
      DBG4,
      "checkout: starting update of {}: {} --> {}",
      getLogPath(),
      (fromTree ? fromTree->getObjectId().toLogString() : "<none>"),
      (toTree ? toTree->getObjectId().toLogString() : "<none>"));

  CheckoutSetup setup;
  setup.checkoutSpan = ctx->createSpan("TreeInode::checkout");

  ctx->throwIfCanceled();

  std::vector<IncompleteInodeLoad> pendingLoads;

  // This default to true on Windows to always make sure that the directory is
  // a placeholder and is safe to be dematerialized. On Windows, adding a
  // placeholder to a directory is idempotent and won't fail on a directory
  // that is already a placeholder.
  setup.shouldInvalidateDirectory =
      getMount()->getEdenConfig()->alwaysInvalidateDirectory.getValue();

  computeCheckoutActions(
      ctx,
      fromTree.get(),
      toTree.get(),
      setup.actions,
      pendingLoads,
      setup.shouldInvalidateDirectory,
      setup.hadConflicts,
      setup.localOnlyRemains,
      reportLocalOnlyAsConflicts,
      removeLocalOnly);

  // Wire up the callbacks for any pending inode loads we started
  for (auto& load : pendingLoads) {
    load.finish();
  }

  return setup;
}

folly::Try<TreeInode::CheckoutFinalizeState>
TreeInode::processCheckoutActionResults(
    bool shouldInvalidateDirectory,
    bool hadConflicts,
    bool localOnlyRemains,
    std::vector<folly::Try<CheckoutActionResult>>& actionResults) {
  CheckoutFinalizeState state;
  state.shouldInvalidateDirectory = shouldInvalidateDirectory;
  state.hadConflicts = hadConflicts;
  state.localOnlyRemains = localOnlyRemains;

  for (auto& result : actionResults) {
    if (result.hasException()) {
      // Fail the checkout at the top level and leave the mount in the
      // interrupted checkout state.
      return folly::Try<CheckoutFinalizeState>{result.exception()};
    }
    state.hadConflicts |= result.value().hadConflicts;
    state.localOnlyRemains |= result.value().localOnlyRemains;
    state.shouldInvalidateDirectory |=
        (result.value().invalidationRequired == InvalidationRequired::Yes);
  }

  return folly::Try<CheckoutFinalizeState>{state};
}

folly::coro::now_task<CheckoutSubtreeResult> TreeInode::co_checkout(
    CheckoutContext* ctx,
    std::shared_ptr<const Tree> fromTree,
    std::shared_ptr<const Tree> toTree,
    bool reportLocalOnlyAsConflicts,
    bool removeLocalOnly) {
  co_await folly::coro::co_reschedule_on_current_executor;

  auto self = inodePtrFromThis();
  auto setup = beginCheckout(
      ctx, fromTree, toTree, reportLocalOnlyAsConflicts, removeLocalOnly);

  SCOPE_EXIT {
    ctx->increaseCheckoutCounter(1);
  };

  co_await self->getMount()->getServerState()->getFaultInjector().co_checkAsync(
      "TreeInode::checkout", getLogPath(), ctx->isDryRun());

  std::vector<folly::coro::Task<CheckoutActionResult>> actionTasks;
  actionTasks.reserve(setup.actions.size());
  for (const auto& action : setup.actions) {
    actionTasks.push_back(
        folly::coro::co_invoke(
            [action, ctx, store = &getObjectStore()]()
                -> folly::coro::Task<CheckoutActionResult> {
              co_await folly::coro::co_reschedule_on_current_executor;
              co_return co_await action->co_run(ctx, store);
            }));
  }

  auto actionResults =
      co_await folly::coro::collectAllTryRange(std::move(actionTasks));

  auto finalizeStateTry = processCheckoutActionResults(
      setup.shouldInvalidateDirectory,
      setup.hadConflicts,
      setup.localOnlyRemains,
      actionResults);
  if (finalizeStateTry.hasException()) {
    finalizeStateTry.exception().throw_exception();
  }
  auto finalizeState = std::move(finalizeStateTry).value();

  if (finalizeState.shouldInvalidateDirectory) {
    InvalidationSnapshot snapshot;
    {
      auto contents = self->contents_.wlock();
      self->updateMtimeAndCtimeLocked(contents->entries, self->getNow());
      snapshot = self->prepareInvalidateDirCache(*contents);
    }
    // Lock released; safe to await the async tail. On non-Windows builds
    // co_finishInvalidateDirCache is a no-op coroutine.
    auto invalidationResult = co_await folly::coro::co_awaitTry(
        self->co_finishInvalidateDirCache(std::move(snapshot)));
    if (invalidationResult.hasException()) {
      auto location = self->getLocationInfo(ctx->renameLock());
      ctx->addError(
          location.parent.get(), location.name, invalidationResult.exception());
    }
  }

  self->saveOverlayPostCheckout(ctx, toTree.get());
  XLOGF(DBG4, "checkout: finished update of {}", self->getLogPath());

  co_return CheckoutSubtreeResult{
      finalizeState.hadConflicts, finalizeState.localOnlyRemains};
}

bool TreeInode::canShortCircuitCheckout(
    CheckoutContext* ctx,
    const ObjectId& treeId,
    AclRootState aclRootState,
    const Tree* fromTree,
    const Tree* toTree) {
  if (toTree &&
      aclRootStateRequiresCheckoutWalk(aclRootState, toTree->aclRootState())) {
    return false;
  }

  if (ctx->isDryRun()) {
    // In a dry-run update we only care about checking for conflicts
    // with the fromTree state.  Since we aren't actually performing any
    // updates we can bail out early as long as there are no conflicts.
    if (fromTree) {
      return ctx->getObjectStore()->areObjectsKnownIdentical(
          treeId, fromTree->getObjectId());
    } else {
      // There is no fromTree.  If we are already in the desired destination
      // state we don't have conflicts.  Otherwise we have to continue and
      // check for conflicts.
      return !toTree ||
          ctx->getObjectStore()->areObjectsKnownIdentical(
              treeId, toTree->getObjectId());
    }
  }

  // For non-dry-run updates we definitely have to keep going if we aren't in
  // the desired destination state.
  if (!toTree ||
      !ctx->getObjectStore()->areObjectsKnownIdentical(
          treeId, toTree->getObjectId())) {
    // If the objects are known different or not known identical, we must take
    // the slow path.
    return false;
  }

  // If we still here we are already in the desired destination state.
  // If there is no fromTree then the only possible conflicts are
  // UNTRACKED_ADDED conflicts, but since we are already in the desired
  // destination state these aren't really conflicts and are automatically
  // resolved.
  if (!fromTree) {
    return true;
  }

  // TODO: If we are doing a force update we should probably short circuit in
  // this case, even if there are conflicts.  For now we don't short circuit
  // just so we can report the conflicts even though we ignore them and perform
  // the update anyway.  However, none of our callers need the conflict list.
  // In the future we should probably just change the checkout API to never
  // return conflict information for force update operations.

  // Allow short circuiting if we are also the same as the fromTree state.
  return ctx->getObjectStore()->areObjectsKnownIdentical(
      treeId, fromTree->getObjectId());
}

void TreeInode::computeCheckoutActions(
    CheckoutContext* ctx,
    const Tree* fromTree,
    const Tree* toTree,
    vector<std::shared_ptr<CheckoutAction>>& actions,
    vector<IncompleteInodeLoad>& pendingLoads,
    bool& wasDirectoryListModified,
    bool& hadConflicts,
    bool& localOnlyRemains,
    bool reportLocalOnlyAsConflicts,
    bool removeLocalOnly) {
  auto computeActionsSpan = ctx->createSpan("computeCheckoutActions");
  XDCHECK(!removeLocalOnly || (!toTree && !ctx->isDryRun()));

  // Grab the contents_ lock for the duration of this function. Checkout is an
  // internal operation and may need to populate a restricted placeholder.
  auto contents = getContentsUnchecked().wlock();

  // If we are the same as some known source control Tree, check to see if we
  // can quickly tell if we have nothing to do for this checkout operation and
  // can return early.
  if (!reportLocalOnlyAsConflicts && contents->treeId.has_value() &&
      canShortCircuitCheckout(
          ctx, contents->treeId.value(), aclRootState(), fromTree, toTree)) {
    // Non-restriction ACL-root metadata does not need a checkout walk.
    // Restricted placeholders are different because their children are hidden.
    bool hasStaleChildAclRootState = false;
    if (toTree) {
      for (auto& [name, entry] : contents->entries.all()) {
        auto toEntry = toTree->find(name);
        if (toEntry != toTree->end() &&
            aclRootStateRequiresCheckoutWalk(
                entry.aclRootState(), toEntry->second.aclRootState())) {
          hasStaleChildAclRootState = true;
          break;
        }
      }
    }
    if (!hasStaleChildAclRootState) {
      ctx->increaseCheckoutCounter(this->getInMemoryDescendants());
      return;
    }
  }

  // Walk through fromTree and toTree, and call the above helper functions as
  // appropriate.
  //
  // Note that we completely ignore entries in our current contents_ that don't
  // appear in either fromTree or toTree.  These are untracked in both the old
  // and new trees.
  auto diffLoop = [&](auto& dirEntries) {
    Tree::container emptyEntries{
        getMount()->getCheckoutConfig()->getCaseSensitive()};
    auto oldIter = fromTree ? fromTree->cbegin() : emptyEntries.cbegin();
    auto oldEnd = fromTree ? fromTree->cend() : emptyEntries.cend();
    auto newIter = toTree ? toTree->cbegin() : emptyEntries.cbegin();
    auto newEnd = toTree ? toTree->cend() : emptyEntries.cend();
    while (true) {
      std::shared_ptr<CheckoutAction> action;

      if (oldIter == oldEnd) {
        if (newIter == newEnd) {
          // All Done
          break;
        }

        // This entry is present in the new tree but not the old one.
        action = processCheckoutEntry(
            ctx,
            *contents,
            dirEntries,
            nullptr,
            &*newIter,
            pendingLoads,
            wasDirectoryListModified,
            hadConflicts);
        ++newIter;
      } else if (newIter == newEnd) {
        // This entry is present in the old tree but not the old one.
        action = processCheckoutEntry(
            ctx,
            *contents,
            dirEntries,
            &*oldIter,
            nullptr,
            pendingLoads,
            wasDirectoryListModified,
            hadConflicts);
        ++oldIter;
      } else {
        auto compare = comparePathPiece(
            oldIter->first,
            newIter->first,
            getMount()->getCheckoutConfig()->getCaseSensitive());

        if (compare == CompareResult::BEFORE) {
          action = processCheckoutEntry(
              ctx,
              *contents,
              dirEntries,
              &*oldIter,
              nullptr,
              pendingLoads,
              wasDirectoryListModified,
              hadConflicts);
          ++oldIter;
        } else if (compare == CompareResult::AFTER) {
          action = processCheckoutEntry(
              ctx,
              *contents,
              dirEntries,
              nullptr,
              &*newIter,
              pendingLoads,
              wasDirectoryListModified,
              hadConflicts);
          ++newIter;
        } else {
          action = processCheckoutEntry(
              ctx,
              *contents,
              dirEntries,
              &*oldIter,
              &*newIter,
              pendingLoads,
              wasDirectoryListModified,
              hadConflicts);
          ++oldIter;
          ++newIter;
        }
      }

      if (action) {
        if (removeLocalOnly) {
          // Tracked subdirectories must empty themselves the same way before
          // they can be removed.
          action->setRemoveLocalOnly();
        }
        actions.push_back(std::move(action));
      }
    }
  };

  diffLoop(contents->entries);
  // A dry run removes nothing, so any local-only entry would survive checkout
  // to an empty tree and block replacing this directory with a file.
  const bool localOnlyBlocksRemoval =
      ctx->isDryRun() && !toTree && !reportLocalOnlyAsConflicts;
  if (reportLocalOnlyAsConflicts || removeLocalOnly || localOnlyBlocksRemoval) {
    auto existsInTree = [](const Tree* tree, PathComponentPiece name) {
      return tree && tree->find(name) != tree->cend();
    };

    for (auto it = contents->entries.all().begin();
         it != contents->entries.end();
         ++it) {
      if (existsInTree(fromTree, it->first) ||
          existsInTree(toTree, it->first)) {
        continue;
      }
      if (localOnlyBlocksRemoval) {
        localOnlyRemains = true;
        break;
      }
      auto action =
          processLocalOnlyCheckoutEntry(ctx, it, pendingLoads, hadConflicts);
      if (action) {
        actions.push_back(std::move(action));
      }
    }
  }
  // A checkout may insert and erase many entries; fold them now instead of
  // leaving lookups and iteration on the two-region path until the next
  // mutation.
  contents->entries.compact();
}

template <typename Contents>
std::shared_ptr<CheckoutAction> TreeInode::processCheckoutEntry(
    CheckoutContext* ctx,
    TreeInodeState& state,
    Contents& contents,
    const Tree::value_type* oldScmEntry,
    const Tree::value_type* newScmEntry,
    std::vector<IncompleteInodeLoad>& pendingLoads,
    bool& wasDirectoryListModified,
    bool& hadConflicts) {
  auto ret = processCheckoutEntryImpl(
      ctx,
      state,
      contents,
      oldScmEntry,
      newScmEntry,
      pendingLoads,
      wasDirectoryListModified,
      hadConflicts);
  if (!ret) {
    const auto& name = oldScmEntry ? oldScmEntry->first : newScmEntry->first;
    if (auto it = contents.find(name); it != contents.end()) {
      if (auto treeInode = it->second.asTreeOrNull()) {
        // If we didn't get a checkout action for this entry but still were able
        // to find a treeInode representing it, it means we won't recurse on it
        // so we increase our "completed" checkout count by its descendants.
        auto increase = treeInode ? treeInode->getInMemoryDescendants() : 0;
        ctx->increaseCheckoutCounter(1 + increase);
      }
    }
  }
  return ret;
}

std::shared_ptr<CheckoutAction> TreeInode::processLocalOnlyCheckoutEntry(
    CheckoutContext* ctx,
    DirContents::iterator it,
    std::vector<IncompleteInodeLoad>& pendingLoads,
    bool& hadConflicts) {
  auto& name = it->first;
  auto& entry = it->second;

  if (auto* child = entry.getInode()) {
    if (child->isDir()) {
      auto childPtr = entry.getInodePtr();
      return std::make_shared<CheckoutAction>(ctx, name, std::move(childPtr));
    }
    if (!ctx->isDryRun()) {
      auto childPtr = entry.getInodePtr();
      return std::make_shared<CheckoutAction>(ctx, name, std::move(childPtr));
    }
    ctx->addConflict(ConflictType::VISIBLE_RESTRICTED, child);
    hadConflicts = true;
    return nullptr;
  }

  if (entry.isDirectory()) {
    auto inodeFuture =
        loadChildLocked(name, entry, pendingLoads, ctx->getFetchContext());
    return std::make_shared<CheckoutAction>(ctx, name, std::move(inodeFuture));
  }

  if (!ctx->isDryRun()) {
    auto inodeFuture =
        loadChildLocked(name, entry, pendingLoads, ctx->getFetchContext());
    return std::make_shared<CheckoutAction>(ctx, name, std::move(inodeFuture));
  }

  ctx->addConflict(
      ConflictType::VISIBLE_RESTRICTED, this, name, entry.getDtype());
  hadConflicts = true;
  return nullptr;
}

namespace {
/**
 * Build a DirEntry from a source-control TreeEntry, allocating a fresh
 * inode number from the given overlay.  Used by the checkout code paths.
 */
DirEntry dirEntryFromScmEntry(const TreeEntry& scmEntry, Overlay* overlay) {
  return DirEntry{
      modeFromTreeEntryType(scmEntry.getType()),
      overlay->allocateInodeNumber(),
      scmEntry.getObjectId(),
      scmEntry.isRestricted(),
      scmEntry.hasACL()};
}
} // namespace

template <typename Contents>
folly::Try<folly::Unit> TreeInode::removeOrReplaceCheckoutEntryLocked(
    CheckoutContext* ctx,
    TreeInodeState& state,
    Contents& contents,
    typename Contents::iterator it,
    const InodePtr& loadedChild,
    const Tree::value_type* newScmEntry,
    bool& wasDirectoryListModified) {
  if (ctx->isDryRun()) {
    return folly::Try<folly::Unit>{folly::unit};
  }

  const auto oldEntryName = it->first;
  const auto oldEntryInodeNumber = it->second.getInodeNumber();
  const auto oldEntryIsDirectory = it->second.isDirectory();
  auto success =
      invalidateChannelEntryCache(state, oldEntryName, oldEntryInodeNumber);
  if (success.hasException()) {
    return success;
  }

  if (loadedChild) {
    loadedChild->markUnlinked(this, oldEntryName, ctx->renameLock());
  }
  contents.erase(it);
  if (newScmEntry) {
    auto [_it, inserted] = contents.emplace(
        newScmEntry->first,
        dirEntryFromScmEntry(newScmEntry->second, getOverlay()));
    XDCHECK(inserted);
  }

  wasDirectoryListModified = true;
  if (oldEntryIsDirectory) {
    if (getMount()
            ->getEdenConfig()
            ->backgroundOverlayCleanupDuringCheckout.getValue()) {
      getOverlay()->recursivelyRemoveOverlayDirBackground(oldEntryInodeNumber);
    } else {
      getOverlay()->recursivelyRemoveOverlayDir(oldEntryInodeNumber);
    }
  } else if (!loadedChild) {
    // A loaded child frees its overlay state when it is destroyed. An
    // unloaded one was never materialized, so it has no overlay file, but
    // it may still have a metadata record from an earlier load, and nothing
    // else will free it.
    getOverlay()->freeInodeMetadata(oldEntryInodeNumber);
  }

  return folly::Try<folly::Unit>{folly::unit};
}

CheckoutActionResult TreeInode::removeOrReplaceRestrictedCheckoutEntry(
    CheckoutContext* ctx,
    const InodePtr& inode,
    const std::optional<Tree::value_type>& newScmEntry) {
  auto treeInode = inode.asTreePtrOrNull();
  XDCHECK(treeInode && treeInode->isRestricted());

  auto currentName = inode->getLocationInfo(ctx->renameLock()).name;
  auto contents = getContentsUnchecked().wlock();
  auto it = contents->entries.find(currentName.piece());
  if (it == contents->entries.end() || it->second.getInode() != inode.get()) {
    EDEN_BUG() << "entry changed while holding rename lock during checkout: "
               << inode->getLogPath();
  }

  bool wasDirectoryListModified = false;
  auto descendants = treeInode->getInMemoryDescendants();
  auto success = removeOrReplaceCheckoutEntryLocked(
      ctx,
      *contents,
      contents->entries,
      it,
      inode,
      newScmEntry ? &*newScmEntry : nullptr,
      wasDirectoryListModified);
  if (success.hasException()) {
    ctx->addError(this, currentName.piece(), success.exception());
    return CheckoutActionResult{
        InvalidationRequired::No, /*hadConflicts=*/true};
  }

  ctx->increaseCheckoutCounter(1 + descendants);
  return CheckoutActionResult{
      wasDirectoryListModified ? InvalidationRequired::Yes
                               : InvalidationRequired::No};
}

template <typename Contents>
std::shared_ptr<CheckoutAction> TreeInode::processCheckoutEntryImpl(
    CheckoutContext* ctx,
    TreeInodeState& state,
    Contents& contents,
    const Tree::value_type* oldScmEntry,
    const Tree::value_type* newScmEntry,
    vector<IncompleteInodeLoad>& pendingLoads,
    bool& wasDirectoryListModified,
    bool& hadConflicts) {
  XLOGF(
      DBG5,
      "processCheckoutEntryImpl({}): {} -> {}",
      getLogPath(),
      (oldScmEntry ? oldScmEntry->second.toLogString(oldScmEntry->first)
                   : "(null)"),
      (newScmEntry ? newScmEntry->second.toLogString(newScmEntry->first)
                   : "(null)"));
  // At most one of oldScmEntry and newScmEntry may be null.
  XDCHECK(oldScmEntry || newScmEntry);

  const bool scmEntryAclStateCanSkipCheckoutWalk = oldScmEntry && newScmEntry &&
      !aclRootStateRequiresCheckoutWalk(oldScmEntry->second.aclRootState(),
                                        newScmEntry->second.aclRootState());
  const bool scmEntriesMatch = oldScmEntry && newScmEntry &&
      // TODO: This is technically incorrect for files that go from SYMLINK to
      // REGULAR (or vice versa).
      //
      // On Windows: Filter executable type for comparison.
      compareTreeEntryType(oldScmEntry->second.getType(),
                           newScmEntry->second.getType()) &&
      scmEntryAclStateCanSkipCheckoutWalk &&
      getObjectStore().areObjectsKnownIdentical(
          oldScmEntry->second.getObjectId(), newScmEntry->second.getObjectId());

  // Look to see if we have a child entry with this name.
  const auto& name = oldScmEntry ? oldScmEntry->first : newScmEntry->first;
  auto it = contents.find(name);
  // If we aren't doing a force checkout, we don't need to do anything for
  // entries that are identical between the old and new source control trees.
  //
  // Restricted placeholders are the exception: their children are hidden from
  // contents_, so an absent matching child still needs to be repopulated when
  // transitioning back to unrestricted.
  const bool liveEntryAclStateCanSkipCheckoutWalk = !newScmEntry ||
      it == contents.end() ||
      !aclRootStateRequiresCheckoutWalk(
          it->second.aclRootState(), newScmEntry->second.aclRootState());
  if (!ctx->forceUpdate() && scmEntriesMatch &&
      liveEntryAclStateCanSkipCheckoutWalk &&
      (!isRestricted() || it != contents.end())) {
    // TODO: Should we perhaps fall through anyway to report conflicts for
    // locally modified files?
    return nullptr;
  }

  if (it == contents.end()) {
    return processAbsentCheckoutEntry(
        ctx,
        state,
        contents,
        oldScmEntry,
        newScmEntry,
        wasDirectoryListModified,
        hadConflicts);
  }

  auto& entry = it->second;
  const bool destinationIsRestrictedTree = newScmEntry &&
      newScmEntry->second.isTree() && newScmEntry->second.isRestricted();

  if (auto childPtr = entry.getInodePtr()) {
    if (auto treeInode = childPtr.asTreePtrOrNull();
        treeInode && treeInode->isRestricted()) {
      const bool removeOrReplaceWithNonTree = oldScmEntry &&
          oldScmEntry->second.isTree() &&
          (!newScmEntry || !newScmEntry->second.isTree());

      if (destinationIsRestrictedTree) {
        // Restricted children are opaque to checkout. If the destination is
        // still restricted, update the loaded placeholder and parent entry
        // metadata without walking the child.
        // TODO: Make this repair path unnecessary by preventing restricted
        // TreeInodes from retaining stale checkout contents. A loaded
        // restricted child should only be an empty placeholder whose tree id
        // and ACL state match the parent DirEntry, so checkout should never
        // need to repair it here.
        if (!ctx->isDryRun()) {
          if (treeInode->updateRestrictedPlaceholder(
                  newScmEntry->second.getObjectId(),
                  newScmEntry->second.aclRootState())) {
            entry.setDematerialized(newScmEntry->second.getObjectId());
            entry.setAclRootState(newScmEntry->second.aclRootState());
            wasDirectoryListModified = true;
          }
        }
        return nullptr;
      }

      if (removeOrReplaceWithNonTree) {
        // Restricted children are opaque to checkout. They cannot have visible
        // local modifications, so remove or replace the parent entry without
        // fetching or walking the restricted tree.
        auto descendants = treeInode->getInMemoryDescendants();
        auto success = removeOrReplaceCheckoutEntryLocked(
            ctx,
            state,
            contents,
            it,
            childPtr,
            newScmEntry,
            wasDirectoryListModified);
        if (success.hasException()) {
          ctx->addError(this, name, success.exception());
          hadConflicts = true;
          return nullptr;
        }
        ctx->increaseCheckoutCounter(1 + descendants);
        return nullptr;
      }
    }

    // If the inode is already loaded, create a CheckoutAction to process it
    return std::make_shared<CheckoutAction>(
        ctx, oldScmEntry, newScmEntry, std::move(childPtr));
  }

  // If a load for this entry is in progress, then we have to wait for the
  // load to finish.  Loading the inode ourself will wait for the existing
  // attempt to finish.
  // We also have to load the inode if it is materialized so we can
  // check its contents to see if there are conflicts or not.
  // On Windows, we need to invalidate ProjectedFS on-disk state.
  if (entry.isMaterialized() ||
      getInodeMap()->isInodeRemembered(entry.getInodeNumber())) {
    XLOGF(DBG6, "must load child: inode={} child={}", getNodeId(), name);
    // This child is potentially modified (or has saved state that must be
    // updated), but is not currently loaded. Start loading it and create a
    // CheckoutAction to process it once it is loaded.
    auto inodeFuture =
        loadChildLocked(it->first, entry, pendingLoads, ctx->getFetchContext());
    if (entry.isDirectory() && !entry.isMaterialized() &&
        entry.isRestricted() && oldScmEntry && oldScmEntry->second.isTree() &&
        destinationIsRestrictedTree) {
      // Omitting oldScmEntry keeps the placeholder opaque and avoids fetching
      // the denied source tree.
      return std::make_shared<CheckoutAction>(
          ctx, nullptr, newScmEntry, std::move(inodeFuture));
    }
    return std::make_shared<CheckoutAction>(
        ctx, oldScmEntry, newScmEntry, std::move(inodeFuture));
  } else {
    XLOGF(DBG6, "not loading child: inode={} child={}", getNodeId(), name);
  }

  // Check for conflicts
  auto conflictType = ConflictType::ERROR;
  if (!oldScmEntry) {
    conflictType = ConflictType::UNTRACKED_ADDED;
  } else if (
      newScmEntry &&
      getObjectStore().areObjectsKnownIdentical(
          entry.getObjectId(), newScmEntry->second.getObjectId())) {
    // On Windows: Filter executable type for comparison.
    if (compareTreeEntryType(
            oldScmEntry->second.getType(), newScmEntry->second.getType()) &&
        !aclRootStateRequiresCheckoutWalk(
            entry.aclRootState(), newScmEntry->second.aclRootState())) {
      // The inode already matches the checkout destination. So do nothing.
      return nullptr;
    }
    // The types don't match, so we should fall through and update the
    // entry. An example is when a file goes from REGULAR -> EXECUTABLE.
  } else {
    switch (getObjectStore().compareObjectsById(
        entry.getObjectId(), oldScmEntry->second.getObjectId())) {
      case ObjectComparison::Unknown: {
        // We don't know if the files are different or not. The only way to know
        // for sure is to load the inode.
        auto inodeFuture = loadChildLocked(
            it->first, entry, pendingLoads, ctx->getFetchContext());
        return std::make_shared<CheckoutAction>(
            ctx, oldScmEntry, newScmEntry, std::move(inodeFuture));
      }
      case ObjectComparison::Identical:
        // We know the objects are identical, so there are no conflicts.
        // Now fall through and possibly recurse.
        break;
      case ObjectComparison::Different:
        // We know the objects are different, so report a conflict.
        conflictType = newScmEntry ? ConflictType::MODIFIED_MODIFIED
                                   : ConflictType::MODIFIED_REMOVED;
        break;
    }
  }

  if (conflictType != ConflictType::ERROR) {
    // If this is a directory we unfortunately have to load it and recurse into
    // it just so we can accurately report the list of files with conflicts.
    if (entry.isDirectory()) {
      auto inodeFuture = loadChildLocked(
          it->first, entry, pendingLoads, ctx->getFetchContext());
      return std::make_shared<CheckoutAction>(
          ctx, oldScmEntry, newScmEntry, std::move(inodeFuture));
    }

    // Report the conflict, and then bail out if we aren't doing a force update
    ctx->addConflict(conflictType, this, name, entry.getDtype());
    hadConflicts = true;
    if (!ctx->forceUpdate()) {
      return nullptr;
    }
  }

  // Bail out now if we aren't actually supposed to apply changes.
  if (ctx->isDryRun()) {
    return nullptr;
  }

  // We are removing or replacing an entry - attempt to invalidate it while the
  // write lock is held and before the contents are updated.
  auto success = removeOrReplaceCheckoutEntryLocked(
      ctx,
      state,
      contents,
      it,
      InodePtr{},
      newScmEntry,
      wasDirectoryListModified);
  if (success.hasException()) {
    if (folly::kIsWindows) {
      // On Windows, reads aren't being done on the inodes, but on the Trees
      // directly, when a file/directory is looked up, the dispatcher will
      // first return the data to ProjectedFS and then in the background update
      // the inodes hierarchy to ensure that the fsRefcount is set.
      //
      // Unfortunately, this means that the inode hierarchy can be slightly out
      // of date. This is one case. A recursive `grep` running concurrently
      // with checkout would populate the working copy without immediately
      // loading inodes. In that case, the invalidateChannelEntryCache will
      // fail with an ENOTEMPTY error. Let's catch this and recurse down as if
      // the directory was already loaded.
      if (auto* exc =
              success.template tryGetExceptionObject<std::system_error>();
          exc && isEnotempty(*exc)) {
        XLOGF(
            DBG6,
            "loading child inode after invalidation failed: inode={} child={}",
            getNodeId(),
            name);
        auto inodeFuture = loadChildLocked(
            it->first, entry, pendingLoads, ctx->getFetchContext());
        return std::make_shared<CheckoutAction>(
            ctx, oldScmEntry, newScmEntry, std::move(inodeFuture));
      }
    }
    ctx->addError(this, name, success.exception());
    hadConflicts = true;
    return nullptr;
  }

  // TODO: contents have changed: we probably should propagate
  // this information up to our caller so it can mark us
  // materialized if necessary.

  return nullptr;
}

template <typename Contents>
std::shared_ptr<CheckoutAction> TreeInode::processAbsentCheckoutEntry(
    CheckoutContext* ctx,
    TreeInodeState& state,
    Contents& contents,
    const Tree::value_type* oldScmEntry,
    const Tree::value_type* newScmEntry,
    bool& wasDirectoryListModified,
    bool& hadConflicts) {
  const auto& name = oldScmEntry ? oldScmEntry->first : newScmEntry->first;
  const auto dtype = oldScmEntry ? oldScmEntry->second.getDtype()
                                 : newScmEntry->second.getDtype();
  bool contentsUpdated = false;

  if (isRestricted() && oldScmEntry) {
    // Restricted directories are represented as empty placeholders. Missing
    // children from the old tree are hidden, not locally removed.
    if (newScmEntry && !ctx->isDryRun()) {
      contentsUpdated = true;
    }
  } else if (!oldScmEntry) {
    // This is a new entry being added, that did not exist in the old tree
    // and does not currently exist in the filesystem.  Go ahead and add it
    // now.
    if (!ctx->isDryRun()) {
      contentsUpdated = true;
    }
  } else if (!newScmEntry) {
    // This file exists in the old tree, but is being removed in the new
    // tree.  It has already been removed from the local filesystem, so
    // we are already in the desired state.
    //
    // We can proceed, but we still flag this as a conflict.
    ctx->addConflict(
        ConflictType::MISSING_REMOVED, this, oldScmEntry->first, dtype);
  } else {
    // The file was removed locally, but modified in the new tree.
    ctx->addConflict(
        ConflictType::REMOVED_MODIFIED, this, oldScmEntry->first, dtype);
    hadConflicts = true;
    if (ctx->forceUpdate()) {
      XDCHECK(!ctx->isDryRun());
      contentsUpdated = true;
    }
  }

  if (contentsUpdated) {
    // Contents have changed and they need to be written out to the
    // overlay.  We should not do that here since this code runs per
    // entry. Today this is reconciled in saveOverlayPostCheckout()
    // after this inode processes all of its checkout actions. But we
    // do want to invalidate the kernel's dcache and inode caches.
    wasDirectoryListModified = true;

    auto success = invalidateChannelEntryCache(state, name, std::nullopt);
    if (success.hasValue()) {
      auto [it, inserted] = contents.emplace(
          newScmEntry->first,
          dirEntryFromScmEntry(newScmEntry->second, getOverlay()));
      XDCHECK(inserted);
    } else {
      if (folly::kIsWindows) {
        if (auto* exc =
                success.template tryGetExceptionObject<std::system_error>();
            exc && isEnotempty(*exc)) {
          XLOGF(
              DBG6,
              "entry was created on disk while checkout is in progress: {}/{}",
              getLogPath(),
              name);
          if (oldScmEntry) {
            ctx->addConflict(
                ConflictType::MODIFIED_MODIFIED, this, name, dtype);
          } else {
            ctx->addConflict(ConflictType::UNTRACKED_ADDED, this, name, dtype);
          }
          hadConflicts = true;
          return nullptr;
        }
      }
      ctx->addError(this, name, success.exception());
      hadConflicts = true;
    }
  }

  // Nothing else to do when there is no local inode.
  return nullptr;
}

// Explicit template instantiations for DirContents.
template std::shared_ptr<CheckoutAction> TreeInode::processCheckoutEntry(
    CheckoutContext*,
    TreeInodeState&,
    DirContents&,
    const Tree::value_type*,
    const Tree::value_type*,
    std::vector<IncompleteInodeLoad>&,
    bool&,
    bool&);
template std::shared_ptr<CheckoutAction> TreeInode::processCheckoutEntryImpl(
    CheckoutContext*,
    TreeInodeState&,
    DirContents&,
    const Tree::value_type*,
    const Tree::value_type*,
    std::vector<IncompleteInodeLoad>&,
    bool&,
    bool&);
template std::shared_ptr<CheckoutAction> TreeInode::processAbsentCheckoutEntry(
    CheckoutContext*,
    TreeInodeState&,
    DirContents&,
    const Tree::value_type*,
    const Tree::value_type*,
    bool&,
    bool&);

namespace {
/**
 * Get this Inode's name.
 */
PathComponent getInodeName(CheckoutContext* ctx, const InodePtr& inode) {
  return inode->getLocationInfo(ctx->renameLock()).name;
}
} // namespace

struct TreeInode::RestrictionTransitionPrep {
  // Tri-state result. On Abort, the caller returns
  // CheckoutActionResult{InvalidationRequired::No, hadConflicts} (the helper
  // may have already called ctx->addError). On Proceed, `currentName` is set
  // and the caller must call finalizeRestrictionTransition.
  enum class State { NoChange, Proceed, Abort };
  State state{State::Abort};
  std::optional<PathComponent> currentName;
  bool oldRestricted{false};
  bool hadConflicts{false};
};

struct TreeInode::DirectoryRemovalResult {
  // If `caseInsensitiveDirRefreshTree` is set, the caller must recurse into
  // treeInode->checkout(ctx, nullptr, refreshTree) instead of using
  // `actionResult`, OR-ing `hadConflicts` into the recursion's result.
  CheckoutActionResult actionResult{InvalidationRequired::No};
  std::shared_ptr<const Tree> caseInsensitiveDirRefreshTree;
  bool hadConflicts{false};
};

CheckoutActionResult TreeInode::replaceFileEntry(
    CheckoutContext* ctx,
    PathComponentPiece name,
    const InodePtr& inode,
    const std::optional<Tree::value_type>& newScmEntry) {
  std::unique_ptr<InodeBase> deletedInode;
  auto contents = lockContentsWrite();

  // The CheckoutContext should be holding the rename lock, so the entry
  // at this name should still be the specified inode.
  auto it = contents->entries.find(name);
  if (it == contents->entries.end()) {
    EDEN_BUG() << "entry removed while holding rename lock during checkout: "
               << inode->getLogPath();
  }
  if (it->second.getInode() != inode.get()) {
    EDEN_BUG() << "entry changed while holding rename lock during checkout: "
               << inode->getLogPath();
  }

  // Tell the OS to invalidate its cache for this entry. For case
  // insensitive mounts, we need to invalidate the current name, hence
  // using it->first instead of name.
  auto success = invalidateChannelEntryCache(
      *contents, it->first, it->second.getInodeNumber());
  if (success.hasException()) {
    getMount()->getServerState()->getEdenFsEventsLogger()->logEvent(
        CheckoutUpdateError{
            inode->getLogPath(), success.exception().what().toStdString()});
    if (folly::kIsWindows) {
      if (auto* exc = success.tryGetExceptionObject<std::system_error>();
          exc && isEnotempty(*exc)) {
        XLOGF(
            DBG6,
            "entry changed on disk from a file to a non-empty directory while checkout is in progress: {}",
            inode->getLogPath());
        if (newScmEntry) {
          ctx->addConflict(
              ConflictType::MODIFIED_MODIFIED,
              this,
              it->first,
              it->second.getDtype());
        } else {
          ctx->addConflict(
              ConflictType::MODIFIED_REMOVED,
              this,
              it->first,
              it->second.getDtype());
        }
        return CheckoutActionResult{
            InvalidationRequired::No, /*hadConflicts=*/true};
      }
    }
    ctx->addError(this, it->first, success.exception());
    return CheckoutActionResult{
        InvalidationRequired::No, /*hadConflicts=*/true};
  }

  // This is a file, so we can simply unlink it, and replace/remove the
  // entry as desired.
  deletedInode = inode->markUnlinked(this, it->first, ctx->renameLock());
  contents->entries.erase(it);

  if (newScmEntry) {
    auto [_it, inserted] = contents->entries.emplace(
        newScmEntry->first,
        dirEntryFromScmEntry(newScmEntry->second, getOverlay()));
    XDCHECK(inserted);
  }

  // We don't save our own overlay data right now: we'll wait to do that
  // until the checkout operation finishes touching all of our children in
  // checkout().
  return CheckoutActionResult{InvalidationRequired::Yes};
}

bool TreeInode::updateRestrictedPlaceholder(
    const ObjectId& treeId,
    AclRootState aclRootState) {
  XDCHECK(aclRootState == AclRootState::RestrictedAclRoot);

  const bool childIsRestricted = isRestricted();
  XDCHECK(childIsRestricted);
  if (UNLIKELY(!childIsRestricted)) {
    XLOGF(
        ERR,
        "Refusing restricted placeholder update for {} (inode {}): child is "
        "unrestricted (ACL state {}, target ACL state {}, target object id {})",
        getLogPath(),
        getNodeId(),
        static_cast<int>(this->aclRootState()),
        static_cast<int>(aclRootState),
        treeId.toLogString());
    return false;
  }

  bool stateChanged = this->aclRootState() != aclRootState;
  {
    auto contents = getContentsUnchecked().wlock();
    const bool placeholderIsEmpty = contents->entries.empty();
    XDCHECK(placeholderIsEmpty);
    if (UNLIKELY(!placeholderIsEmpty)) {
      XLOGF(
          ERR,
          "Refusing restricted placeholder update for {} (inode {}): child "
          "contains {} entries (materialized {}, ACL state {}, current object "
          "id {}, target object id {})",
          getLogPath(),
          getNodeId(),
          contents->entries.size(),
          contents->isMaterialized(),
          static_cast<int>(this->aclRootState()),
          contents->treeId ? contents->treeId->toLogString() : "none",
          treeId.toLogString());
      return false;
    }
    stateChanged = stateChanged || contents->isMaterialized() ||
        !contents->treeId.has_value() || !contents->treeId->bytesEqual(treeId);
    contents->treeId = treeId;
    if (stateChanged) {
      saveOverlayDir(contents->entries, /*isMaterialized=*/false);
    }
  }

  setAclRootState(aclRootState);
  assertRestrictedPlaceholderInvariant();
  return true;
}

TreeInode::RestrictionTransitionPrep TreeInode::prepareRestrictionTransition(
    CheckoutContext* ctx,
    const TreeInodePtr& treeInode,
    const Tree::value_type& replacementEntry,
    bool newRestricted) {
  bool oldRestricted = false;
  {
    auto contents = getContentsUnchecked().rlock();
    auto it = contents->entries.find(replacementEntry.first);
    if (it != contents->entries.end()) {
      oldRestricted = it->second.isRestricted();
    }
  }

  RestrictionTransitionPrep prep;
  prep.oldRestricted = oldRestricted;
  if (newRestricted == oldRestricted) {
    prep.state = RestrictionTransitionPrep::State::NoChange;
    return prep;
  }

  prep.currentName = getInodeName(ctx, treeInode);
  if (!ctx->isDryRun()) {
    auto contents = getContentsUnchecked().wlock();
    auto it = contents->entries.find(prep.currentName->piece());
    if (it == contents->entries.end() ||
        it->second.getInode() != treeInode.get() ||
        it->second.isRestricted() != oldRestricted) {
      prep.state = RestrictionTransitionPrep::State::Abort;
      return prep;
    }

    auto invalidateResult = invalidateChannelEntryCache(
        *contents, prep.currentName->piece(), treeInode->getNodeId());
    if (invalidateResult.hasException()) {
      ctx->addError(
          this, prep.currentName->piece(), invalidateResult.exception());
      prep.state = RestrictionTransitionPrep::State::Abort;
      prep.hadConflicts = true;
      return prep;
    }
  }

  prep.state = RestrictionTransitionPrep::State::Proceed;
  return prep;
}

CheckoutActionResult TreeInode::finalizeRestrictionTransition(
    CheckoutContext* ctx,
    const TreeInodePtr& treeInode,
    PathComponentPiece currentName,
    bool newRestricted,
    CheckoutSubtreeResult result) {
  if (ctx->isDryRun() ||
      (newRestricted && result.hadConflicts && !ctx->forceUpdate())) {
    return CheckoutActionResult{InvalidationRequired::No, result.hadConflicts};
  }

  treeInode->setRestricted(newRestricted);
  auto newAclRootState = treeInode->aclRootState();

  auto contents = getContentsUnchecked().wlock();
  auto it = contents->entries.find(currentName);
  if (it == contents->entries.end() ||
      it->second.getInode() != treeInode.get()) {
    return CheckoutActionResult{InvalidationRequired::No, result.hadConflicts};
  }
  it->second.setAclRootState(newAclRootState);
  return CheckoutActionResult{InvalidationRequired::Yes, result.hadConflicts};
}

TreeInode::DirectoryRemovalResult TreeInode::finalizeDirectoryRemoval(
    CheckoutContext* ctx,
    const TreeInodePtr& treeInode,
    std::shared_ptr<const Tree> newTree,
    const std::optional<Tree::value_type>& newScmEntry,
    PathComponentPiece localName,
    bool hadConflicts) {
  DirectoryRemovalResult result;
  result.hadConflicts = hadConflicts;

  // Now we can attempt to delete treeInode!
  // The ordering of invalidateChannelEntryCache and tryRemoveChild
  // is important on Windows here. We need to attempt to clear the
  // filesystem data before we delete the inode because the kernel is
  // the source of truth. If invalidating fails, we do not want to
  // actually delete the inode from eden's state. Note: On NFS and
  // FUSE, EdenFS is the source of truth. So in theory one might want
  // to change eden first and then FUSE or NFS. On FUSE this doesn't
  // really matter, if we invalidated fuse and then removing in eden
  // fails, its fine we did the extra invalidation. On NFS our
  // invalidation relies on data changing in EdenFS and the kernel
  // noticing and clearing its own caches. So we would really want
  // tryRemoveChild to happen first. But thankfully
  // invalidateChannelEntryCache doesn't do anything on NFS anyways.
  // so it does not matter these are out of order.
  if (invalidateChannelEntryCache(
          *lockContentsWrite(), localName, treeInode->getNodeId())
          .hasException()) {
    if (newTree) {
      XCHECK_EQ(
          getMount()->getCheckoutConfig()->getCaseSensitive(),
          CaseSensitivity::Insensitive);
      XCHECK_NE(newScmEntry->first, localName);
      // Because invalidateChannelEntryCache can only fail on Windows
      // and PrjFS, the mount must be case-insensitive. Moreover,
      // newScmEntry->first and name are different, so the case of
      // the directory changed. Unfortunately, we couldn't remove the
      // directory from the disk, and thus we are unable to actually
      // change the case. This can be due to the directory containing
      // an untracked file for instance. We can however fallback to
      // updating the directory itself to the newTree. This behavior
      // is consistent with vanilla Mercurial.
      result.caseInsensitiveDirRefreshTree = std::move(newTree);
      return result;
    }
    ctx->addConflict(ConflictType::DIRECTORY_NOT_EMPTY, treeInode.get());
    result.hadConflicts = true;
    result.actionResult =
        CheckoutActionResult{InvalidationRequired::No, result.hadConflicts};
    return result;
  }

  if (tryRemoveChild(
          ctx->renameLock(), localName, treeInode, InvalidationRequired::No) !=
      0) {
    ctx->addConflict(ConflictType::DIRECTORY_NOT_EMPTY, treeInode.get());
    result.hadConflicts = true;
    result.actionResult =
        CheckoutActionResult{InvalidationRequired::Yes, result.hadConflicts};
    return result;
  }

  // If the entry does not exist at the new commit we can stop here.
  // no need to add anything back to our parent's contents.
  if (!newScmEntry) {
    result.actionResult =
        CheckoutActionResult{InvalidationRequired::Yes, result.hadConflicts};
    return result;
  }

  // On case insensitive mounts, a change of casing would lead to a
  // removal of this TreeInode followed by the insertion of the
  // different cased TreeInode.
  if (newScmEntry->second.isTree()) {
    XDCHECK_EQ(
        getMount()->getCheckoutConfig()->getCaseSensitive(),
        CaseSensitivity::Insensitive);
  }

  bool inserted = false;
  {
    auto contents = lockContentsWrite();
    auto ret = contents->entries.emplace(
        newScmEntry->first,
        dirEntryFromScmEntry(newScmEntry->second, getOverlay()));
    inserted = ret.second;
  }

  if (!inserted) {
    // Hmm.  Someone else already created a new entry in
    // this location before we had a chance to add our new
    // entry.  We don't block new file or directory
    // creations during a checkout operation, so this is
    // possible.  Just report an error in this case.
    ctx->addError(
        this,
        localName,
        InodeError(
            EEXIST,
            inodePtrFromThis(),
            localName,
            "new file created with this name while checkout operation "
            "was in progress"));
    result.hadConflicts = true;
  }

  // Make sure that we invalidate the directory in TreeInode::checkout.
  result.actionResult =
      CheckoutActionResult{InvalidationRequired::Yes, result.hadConflicts};
  return result;
}

folly::coro::now_task<CheckoutActionResult> TreeInode::co_checkoutUpdateEntry(
    CheckoutContext* ctx,
    PathComponentPiece name,
    InodePtr inode,
    std::shared_ptr<const Tree> oldTree,
    std::shared_ptr<const Tree> newTree,
    const std::optional<Tree::value_type>& newScmEntry,
    bool removeLocalOnly) {
  auto treeInode = inode.asTreePtrOrNull();
  if (!treeInode) {
    ctx->increaseCheckoutCounter(1);
    if (ctx->isDryRun()) {
      co_return CheckoutActionResult{InvalidationRequired::No};
    }
    co_return replaceFileEntry(ctx, name, inode, newScmEntry);
  }

  if (treeInode->isRestricted() &&
      (!newScmEntry || !newScmEntry->second.isTree())) {
    co_return removeOrReplaceRestrictedCheckoutEntry(ctx, inode, newScmEntry);
  }

  if (newScmEntry && newScmEntry->second.isTree()) {
    XDCHECK(newScmEntry.has_value());
    if (!newScmEntry->second.isRestricted()) {
      XDCHECK(newTree);
    }

    if (getMount()->getCheckoutConfig()->getCaseSensitive() ==
            CaseSensitivity::Insensitive &&
        newScmEntry->first != getInodeName(ctx, treeInode)) {
      // Case insensitive name change — fall through to remove-and-readd.
    } else {
      const auto& replacementEntry = *newScmEntry;
      bool newRestricted = replacementEntry.second.isRestricted() ||
          (newTree && newTree->isRestricted());

      auto prep = prepareRestrictionTransition(
          ctx, treeInode, replacementEntry, newRestricted);
      using PrepState = RestrictionTransitionPrep::State;
      if (prep.state == PrepState::NoChange) {
        if (newRestricted && prep.oldRestricted) {
          ctx->increaseCheckoutCounter(1 + treeInode->getInMemoryDescendants());
          if (!ctx->isDryRun()) {
            // Use the metadata source that established the restriction.
            auto restrictedTreeId = replacementEntry.second.getObjectId();
            auto restrictedAclRootState =
                replacementEntry.second.aclRootState();
            auto restrictedHasACL = replacementEntry.second.hasACL();
            if (!replacementEntry.second.isRestricted()) {
              XCHECK(newTree);
              XCHECK(newTree->isRestricted());
              restrictedTreeId = newTree->getObjectId();
              restrictedAclRootState = newTree->aclRootState();
              restrictedHasACL = newTree->hasACL();
            }
            if (treeInode->updateRestrictedPlaceholder(
                    restrictedTreeId, restrictedAclRootState)) {
              auto currentName = getInodeName(ctx, treeInode);
              childDematerialized(
                  ctx->renameLock(),
                  currentName.piece(),
                  restrictedTreeId,
                  /*writeOverlay=*/false,
                  /*isRestricted=*/true,
                  restrictedHasACL);
            }
          }
          co_return CheckoutActionResult{InvalidationRequired::No};
        }
        // Ordinary dir->dir checkout still recurses in place.
        auto result = co_await treeInode->co_checkout(
            ctx, std::move(oldTree), std::move(newTree));
        co_return CheckoutActionResult{
            InvalidationRequired::No, result.hadConflicts};
      }
      if (prep.state == PrepState::Abort) {
        co_return CheckoutActionResult{
            InvalidationRequired::No, prep.hadConflicts};
      }

      auto currentName = std::move(*prep.currentName);
      XLOGF(
          DBG3,
          "co_checkoutUpdateEntry({}): restriction transition for {}: {} -> {}",
          getLogPath(),
          name,
          prep.oldRestricted,
          newRestricted);

      std::shared_ptr<const Tree> checkoutToTree;
      if (!newRestricted) {
        XDCHECK(newTree);
        checkoutToTree = std::move(newTree);
      }
      auto result = co_await treeInode->co_checkout(
          ctx,
          std::move(oldTree),
          std::move(checkoutToTree),
          /*reportLocalOnlyAsConflicts=*/newRestricted);

      co_return finalizeRestrictionTransition(
          ctx, treeInode, currentName.piece(), newRestricted, result);
    }
  }

  // Need to remove this directory (and possibly replace with a file or a
  // case-renamed directory). First recursively unlink everything in it.
  if (ctx->forceUpdate() && newScmEntry && !newScmEntry->second.isTree() &&
      getMount()->getEdenConfig()->forceCheckoutRemovesLocalOnly.getValue()) {
    // Local-only contents would otherwise block the replacement.
    removeLocalOnly = true;
  }
  auto checkoutResult = co_await treeInode->co_checkout(
      ctx,
      std::move(oldTree),
      nullptr,
      /*reportLocalOnlyAsConflicts=*/false,
      removeLocalOnly);
  auto hadConflicts = checkoutResult.hadConflicts;

  if (ctx->isDryRun()) {
    if (checkoutResult.localOnlyRemains && newScmEntry &&
        !newScmEntry->second.isTree()) {
      ctx->addConflict(ConflictType::DIRECTORY_NOT_EMPTY, treeInode.get());
      hadConflicts = true;
    }
    co_return CheckoutActionResult{
        InvalidationRequired::No,
        hadConflicts,
        checkoutResult.localOnlyRemains};
  }

  const auto& localName = getInodeName(ctx, treeInode);
  auto result = finalizeDirectoryRemoval(
      ctx, treeInode, std::move(newTree), newScmEntry, localName, hadConflicts);
  if (result.caseInsensitiveDirRefreshTree) {
    auto refreshResult = co_await treeInode->co_checkout(
        ctx, nullptr, std::move(result.caseInsensitiveDirRefreshTree));
    co_return CheckoutActionResult{
        InvalidationRequired::No, hadConflicts || refreshResult.hadConflicts};
  }
  co_return result.actionResult;
}

#ifdef _WIN32
namespace {
/**
 * Test if the passed in InodeNumber is known by the InodeMap.
 */
bool needDecFsRefcount(InodeMap& inodeMap, InodeNumber ino) {
  return inodeMap.isInodeLoadedOrRemembered(ino);
}
} // namespace
#endif

namespace {
/**
 * Whether the NFS GC invalidation of a directory may clear this child's FS
 * reference. Pinned inodes and directories whose subtree contains a pin are
 * kept, and so are all directories when pin information is unavailable; see
 * TreeInode::handleChildrenNotAccessedRecently.
 */
bool nfsGcMayClearChild(
    const DirEntry& entry,
    const std::shared_ptr<const folly::F14FastSet<InodeNumber>>& pinnedInodes,
    const folly::F14FastSet<InodeNumber>& pinnedChildren) {
  const auto ino = entry.getInodeNumber();
  if (pinnedInodes && pinnedInodes->count(ino)) {
    return false;
  }
  if (entry.isDirectory()) {
    return pinnedInodes && !pinnedChildren.count(ino);
  }
  return true;
}
} // namespace

#ifndef _WIN32
std::optional<std::pair<AbsolutePath, mode_t>>
TreeInode::nfsPrepareDirInvalidationLocked(TreeInodeState& state) {
  if (!getMount()->getNfsdChannel()) {
    return std::nullopt;
  }
  const auto path = getPath();
  if (!path.has_value()) {
    return std::nullopt;
  }
  return std::make_pair(
      getMount()->getPath() + *path, getMetadataLocked(state.entries).mode);
}

bool TreeInode::nfsInvalidateDirCacheLocked(
    TreeInodeState& state,
    std::optional<NfsInvalidationSource> source) {
  auto* channel = getMount()->getNfsdChannel();
  auto target = nfsPrepareDirInvalidationLocked(state);
  if (!channel || !target) {
    return false;
  }
  channel->invalidate(std::move(target->first), target->second, source);
  return true;
}

std::optional<NfsGcPreparedInvalidation> TreeInode::nfsPrepareGcInvalidation(
    TreeInodeState& state,
    const std::shared_ptr<const folly::F14FastSet<InodeNumber>>& pinnedInodes,
    folly::F14FastSet<InodeNumber> pinnedChildren) {
  auto target = nfsPrepareDirInvalidationLocked(state);
  if (!target) {
    return std::nullopt;
  }

  // The contents lock is held by invalidateChildrenNotMaterialized. Which
  // children to clear is decided when the chmod reaches EdenFS, not here:
  // the chmod may wait in the invalidation queue, and a child the client
  // looks up meanwhile is referenced again by that lookup.
  auto stats = getMount()->getStats().copy();
  auto forget = [self = inodePtrFromThis(),
                 inodeMapWeak = getInodeMapWeak(),
                 stats = std::move(stats),
                 pinnedInodes,
                 pinnedChildren = std::move(pinnedChildren)]() -> uint64_t {
    auto inodeMap = inodeMapWeak.lock();
    if (!inodeMap) {
      XLOG(WARN, "InodeMap is killed before GC completes");
      return 0;
    }
    // The client is about to forget the directory's names. Drop the
    // children's references with it, as a FUSE FORGET would; whatever the
    // client looks up again from here on is referenced anew by that lookup.
    uint64_t numCleared = 0;
    auto contents = self->getContentsUnchecked().rlock();
    for (const auto& entry : contents->entries.all()) {
      if (!nfsGcMayClearChild(entry.second, pinnedInodes, pinnedChildren)) {
        continue;
      }
      const auto ino = entry.second.getInodeNumber();
      stats->increment(&NfsStats::nfsInvalidationGcClearFsRefcountAttempt);
      if (!entry.second.getInode() && !inodeMap->isInodeRemembered(ino)) {
        stats->increment(&NfsStats::nfsInvalidationGcClearFsRefcountSkipped);
        continue;
      }
      XLOGF(DBG9, "GC invalidated inode {}", ino);
      if (inodeMap->clearFsRefcount(ino)) {
        numCleared++;
        stats->increment(&NfsStats::nfsInvalidationGcClearFsRefcountCleared);
      }
    }
    return numCleared;
  };
  // Read without the parents' locks: a rename racing with this walk only
  // changes which ancestors' request times the chmod leaves alone, and each
  // step moves up one level, so the walk ends at the root.
  std::vector<InodeNumber> lineage{getNodeId()};
  for (auto parent = getParentRacy(); parent;
       parent = parent->getParentRacy()) {
    lineage.push_back(parent->getNodeId());
  }
  return NfsGcPreparedInvalidation{
      std::move(target->first),
      target->second,
      std::move(forget),
      std::move(lineage)};
}
#endif

folly::Try<folly::Unit> TreeInode::invalidateChannelEntryCache(
    TreeInodeState&,
    PathComponentPiece name,
    [[maybe_unused]] std::optional<InodeNumber> ino) {
  auto faultTry = getMount()->getServerState()->getFaultInjector().checkTry(
      "invalidateChannelEntryCache", name);
  if (faultTry.hasException()) {
    return folly::Try<folly::Unit>{faultTry.exception()};
  }

#ifndef _WIN32
  if (auto* fuseChannel = getMount()->getFuseChannel()) {
    fuseChannel->invalidateEntry(getNodeId(), name);
  }
  // For NFS, the entry cache is flushed when the directory mtime is changed.
  // Directly invalidating an entry is not possible.

#else
  if (auto* fsChannel = getMount()->getPrjfsChannel()) {
    const auto path = getPath();
    if (path.has_value()) {
      // Try to remove the file first, and then call decFsRefcount if needed.
      // When no inode number is passed in, we still need to invalidate the
      // ProjectedFS file, as tombstones are a special kind of placeholder
      // that EdenFS doesn't have inodes for.
      auto ret = fsChannel->removeCachedFile(path.value() + name);
      if (ret.hasValue()) {
        auto& inodeMap = *getInodeMap();
        if (ino && needDecFsRefcount(inodeMap, *ino)) {
          // At this point, the file is now virtual, that is no placeholder or
          // full file are present on disk. If at this point, the file is being
          // looked up, EdenFS will service the lookup in
          // PrjfsDispatcherImpl::lookup, and then try to increment the
          // refcount. The refcount increment is guarantee to happen after the
          // decrement below due to the increment needing to traverse the inode
          // hierarchy and thus acquiring the content lock. The same content
          // lock that is held in this function.
          getInodeMap()->decFsRefcount(*ino);
        }
      }
      return ret;
    }
  }
#endif

  return folly::Try<folly::Unit>{folly::unit};
}

ImmediateFuture<folly::Unit> TreeInode::invalidateChannelDirCache(
    TreeInodeState& state) {
#ifndef _WIN32
  if (auto* fuseChannel = getMount()->getFuseChannel()) {
    // FUSE_NOTIFY_INVAL_ENTRY is the appropriate invalidation function
    // when an entry is removed or modified. But when new entries are
    // added, the inode itself must be invalidated.
    fuseChannel->invalidateInode(getNodeId(), 0, 0);
  } else {
    // Does nothing when the mount has no NFS channel either.
    nfsInvalidateDirCacheLocked(state);
  }
#else
  (void)state;
  if (auto* fsChannel = getMount()->getPrjfsChannel()) {
    const auto path = getPath();
    if (path.has_value()) {
      // Invalidation may block, thus in order to not starve the server thread
      // pool during checkout let's move it to a separate thread.
      return ImmediateFuture{folly::via(
                                 getMount()->getInvalidationThreadPool().get(),
                                 [fsChannel, path = std::move(path).value()]() {
                                   // Don't call decFsRefcount here as we're
                                   // adding a placeholder, and thus this
                                   // TreeInode need to be kept in the InodeMap
                                   // until invalidateChannelEntryCache is
                                   // called on it.
                                   return fsChannel->addDirectoryPlaceholder(
                                       path);
                                 })
                                 .semi()};
    }
  }
#endif

  return folly::unit;
}

TreeInode::InvalidationSnapshot TreeInode::prepareInvalidateDirCache(
    TreeInodeState& state) {
  InvalidationSnapshot snapshot;
#ifndef _WIN32
  // Linux: do the FUSE/NFS sync work inline. The caller holds the wlock
  // which protects access to `state` for the NFS mode lookup.
  if (auto* fuseChannel = getMount()->getFuseChannel()) {
    fuseChannel->invalidateInode(getNodeId(), 0, 0);
  } else {
    // Does nothing when the mount has no NFS channel either.
    nfsInvalidateDirCacheLocked(state);
  }
#else
  (void)state;
  if (auto* fsChannel = getMount()->getPrjfsChannel()) {
    snapshot.prjfsChannel = fsChannel;
    snapshot.windowsPath = getPath();
  }
#endif
  return snapshot;
}

folly::coro::now_task<folly::Unit> TreeInode::co_finishInvalidateDirCache(
    [[maybe_unused]] InvalidationSnapshot snapshot) {
#ifdef _WIN32
  if (snapshot.prjfsChannel && snapshot.windowsPath.has_value()) {
    // Invalidation may block, so dispatch to the dedicated invalidation
    // thread pool. `prjfsChannel` and `windowsPath` were captured in the
    // snapshot under the caller's contents_ wlock, so the channel
    // pointer does not need to be re-resolved after the suspension.
    co_await folly::coro::co_withExecutor(
        getMount()->getInvalidationThreadPool().get(),
        folly::coro::co_invoke(
            [fsChannel = snapshot.prjfsChannel,
             path = std::move(
                 *snapshot.windowsPath)]() -> folly::coro::Task<folly::Unit> {
              auto result = fsChannel->addDirectoryPlaceholder(path);
              if (result.hasException()) {
                co_yield folly::coro::co_error(std::move(result).exception());
              }
              co_return folly::unit;
            }));
  }
#endif
  co_return folly::unit;
}

void TreeInode::saveOverlayPostCheckout(
    CheckoutContext* ctx,
    const Tree* tree) {
  auto saveOverlaySpan = ctx->createSpan("saveOverlayPostCheckout");

  if (ctx->isDryRun()) {
    // If this is a dry run, then we do not want to update the parents or make
    // any sort of unnecessary writes to the overlay, so we bail out.
    return;
  }

  bool isMaterialized;
  bool stateChanged;
  {
    auto contents = getContentsUnchecked().wlock();

    // Check to see if we need to be materialized or not.
    //
    // If we can confirm that we are identical to the source control Tree we
    // do not need to be materialized.
    auto tryToDematerialize = [&]() -> std::optional<ObjectId> {
      // If the new tree does not exist in source control, we must be
      // materialized, since there is no source control Tree to refer to.
      if (!tree) {
        return std::nullopt;
      }

      // If we have a different number of entries we must be different from
      // the Tree, and therefore must be materialized.
      if (tree->size() != contents->entries.size()) {
        return std::nullopt;
      }

      // This code relies on the fact that our contents->entries PathMap sorts
      // paths in the same order as Tree's entry list.
      auto inodeIter = contents->entries.all().begin();
      auto scmIter = tree->begin();
      for (; scmIter != tree->end(); ++inodeIter, ++scmIter) {
        // If any of our children are materialized, we need to be materialized
        // too to record the fact that we have materialized children.
        //
        // If our children are materialized this means they are likely
        // different from the new source control state.  (This is not a 100%
        // guarantee though, as writes may still be happening concurrently to
        // the checkout operation.)  Even if the child is still identical to
        // its source control state we still want to make sure we are
        // materialized if the child is.
        if (inodeIter->second.isMaterialized()) {
          return std::nullopt;
        }

        // A renamed child keeps its object id, so the id alone can match a
        // different entry of the Tree at the same position. The name has
        // to match as well, or reloading from the Tree would change the
        // directory.
        if (inodeIter->first != scmIter->first) {
          return std::nullopt;
        }

        // If the child is not materialized, it is the same as some source
        // control object.  However, if it isn't the same as the object in our
        // Tree, we have to materialize ourself.
        switch (getObjectStore().compareObjectsById(
            inodeIter->second.getObjectId(), scmIter->second.getObjectId())) {
          case ObjectComparison::Unknown:
            // Assume the child is different, and leave materialized.
            return std::nullopt;
          case ObjectComparison::Identical:
            // The IDs refer to the same object, so we can dematerialize. Even
            // if the IDs don't match exactly, we'll silently migrate to the
            // new ID scheme here.
            inodeIter->second.setHasACL(preferKnownAclState(
                scmIter->second.hasACL(), inodeIter->second.hasACL()));
            break;
          case ObjectComparison::Different:
            // The objects differ, so we can't dematerialize.
            return std::nullopt;
        }
      }

      // TODO: This check should be removed and instead a
      // std::optional<ObjectId> should be passed to
      // TreeInode::saveoverlayPostCheckout. The issue is that setPathRootId
      // synthesizes a fake Tree and then calls checkout, which might notice
      // that the previous fake Tree and the current fake Tree have the same
      // id, which will incorrectly dematerialized this inode. The fake id
      // cannot be reconstituted from the backing store, so this makes the
      // directory structure unreadable. The correct long-term fix is to
      // remove getObjectId() from Tree and pass around ObjectIds explicitly if
      // known.
      if (tree->getObjectId().size() == 0) {
        return std::nullopt;
      }

      // If we're still here we are identical to the source control Tree.
      // We can be dematerialized and marked identical to the input Tree.
      return tree->getObjectId();
    };

    auto oldId = contents->treeId;
    auto oldState = aclRootState();
    auto newState = oldState;
    if (tree) {
      newState = makeAclRootState(
          tree->isRestricted(), preferKnownAclState(tree->hasACL(), hasACL()));
    }
    auto newId = tryToDematerialize();
    contents->treeId = newId;
    isMaterialized = contents->isMaterialized();
    // If our tree id changed, even if it references the same contents, we
    // must tell the parent so it can update its id. Therefore, don't use
    // BackingStore::areObjectsKnownIdentical here.
    if (oldId.has_value() && newId.has_value()) {
      stateChanged = !oldId->bytesEqual(*newId);
    } else if (!oldId.has_value() && !contents->treeId.has_value()) {
      stateChanged = false;
    } else {
      stateChanged = true;
    }
    stateChanged = stateChanged || oldState != newState;

    XLOGF(
        DBG4,
        "saveOverlayPostCheckout({}, {}): oldId={} newId={} isMaterialized={}",
        getLogPath(),
        fmt::ptr(tree),
        (oldId ? oldId.value().toLogString() : "none"),
        (contents->treeId ? contents->treeId.value().toLogString() : "none"),
        isMaterialized);

    // Update the overlay to include the new entries, even if dematerialized.
    saveOverlayDir(contents->entries, isMaterialized);
    if (tree) {
      setAclRootState(newState);
    }
  }

  if (stateChanged) {
    // If our state changed, tell our parent.
    //
    // We pass writeOverlay=false because each directory's overlay is written
    // once by its own saveOverlayPostCheckout() call. The in-memory
    // materialization state is still propagated up the tree so that each
    // ancestor knows it's materialized, but the overlay writes are deferred
    // until each ancestor's own saveOverlayPostCheckout() runs.
    //
    // If we get an error during checkout (or eden crashes) we can be in an
    // inconsistent state where the parent has updated in-memory state that has
    // not been persisted to the overlay. I think this is okay since the user
    // must continue the interrupted checkout, which will re-checkout the parent
    // directory.
    auto loc = getLocationInfo(ctx->renameLock());
    if (loc.parent && !loc.unlinked) {
      if (isMaterialized) {
        loc.parent->childMaterialized(
            ctx->renameLock(), loc.name, /*writeOverlay=*/false);
      } else {
        if (tree == nullptr) {
          return;
        }
        loc.parent->childDematerialized(
            ctx->renameLock(),
            loc.name,
            tree->getObjectId(),
            /*writeOverlay=*/false,
            tree->isRestricted(),
            tree->hasACL());
      }
    }
  }
}

bool TreeInode::startLoadingChild(
    PathComponentPiece name,
    DirEntry& entry,
    folly::Promise<InodePtr>& promise) {
  try {
    auto result = getInodeMap()->startLoadingChildIfNotLoading(
        this, name, entry.getInodeNumber(), entry.getInitialMode(), promise);
#ifndef _WIN32
    if (result == InodeMap::ChildLoadResult::Conflict &&
        getOverlay()->usesOnlineFsck()) {
      // A torn rename can leave A/x and B/y pointing to inode 123. After A/x
      // is loaded, give conflicting B/y a fresh inode: deleting B/y must not
      // delete A/x's data. Unseen aliases still follow normal deletion; after
      // deleting A/x, a later load of B/y may find its payload missing.
      auto oldNumber = entry.getInodeNumber();
      auto newNumber = getOverlay()->allocateInodeNumber();
      auto mode = entry.getInitialMode();
      auto acl = entry.aclRootState();
      if (entry.isMaterialized()) {
        if (entry.isDirectory()) {
          getOverlay()->saveOverlayDir(
              newNumber,
              DirContents{getMount()->getCheckoutConfig()->getCaseSensitive()});
        } else {
          getOverlay()->createOverlayFile(newNumber, folly::ByteRange{});
        }
      }

      // SCM content can be loaded normally under the new inode number.
      auto replacement = entry.isMaterialized()
          ? DirEntry{mode, newNumber, acl}
          : DirEntry{mode, newNumber, entry.getObjectId(), acl};
      auto original = std::move(entry);
      entry = std::move(replacement);

      // Assigning through the entry reference does not mutate the PathMap's
      // count, but the cached readdir index contains the old inode number.
      readdirIndex_.wlock()->reset();

      // Only an unloaded entry's number changes, so no runtime inode location
      // needs the rename lock. Persist the binding before publishing the inode.
      const auto& contents = contents_.unsafeGetUnlocked();
      try {
        getMount()->getServerState()->getFaultInjector().check(
            "recoverConflictingBindingSaveOverlay", name);
        saveOverlayDir(contents.entries, contents.isMaterialized());
      } catch (...) {
        // A retry must repair and persist the binding again before loading it.
        entry = std::move(original);
        throw;
      }
      getMount()->getStats()->increment(&OverlayStats::onlineFsckInodeRecovery);
      XLOGF(
          WARN,
          "Recovered conflicting overlay entry {}/{}: inode {} -> {}",
          getNodeId(),
          name,
          oldNumber,
          newNumber);
      result = getInodeMap()->startLoadingChildIfNotLoading(
          this, name, newNumber, mode, promise);
    }
#endif
    if (result == InodeMap::ChildLoadResult::Conflict) {
      throw std::system_error{
          EIO,
          std::generic_category(),
          fmt::format(
              "conflicting inode reference to inode {}",
              entry.getInodeNumber())};
    }
    return result == InodeMap::ChildLoadResult::Started;
  } catch (const std::exception&) {
    promise.setException(folly::exception_wrapper{std::current_exception()});
    return false;
  }
}

ImmediateFuture<InodePtr> TreeInode::loadChildLocked(
    PathComponentPiece name,
    DirEntry& entry,
    std::vector<IncompleteInodeLoad>& pendingLoads,
    const ObjectFetchContextPtr& fetchContext) {
  XDCHECK(!entry.getInode());

  auto [promise, future] = folly::makePromiseContract<InodePtr>();

  bool startLoad;
  {
    auto span = fetchContext->createSpan("startLoadingChildIfNotLoading");
    startLoad = startLoadingChild(name, entry, promise);
  }
  if (startLoad) {
    auto loadFuture = startLoadingInodeNoThrow(entry, name, fetchContext, true);
    pendingLoads.emplace_back(
        this, std::move(loadFuture), name, entry.getInodeNumber());
  }

  return ImmediateFuture{std::move(future)};
}

namespace {
/**
 * WARNING: predicate and shouldKeep are called while the InodeMap and
 * TreeInode contents locks are held.
 */
template <
    typename Recurse,
    typename Predicate,
    typename ShouldKeep,
    typename ShouldCancel>
size_t unloadChildrenIf(
    TreeInode* const self,
    InodeMap* const inodeMap,
    std::vector<TreeInodePtr>& treeChildren,
    Recurse&& recurse,
    Predicate&& predicate,
    ShouldKeep&& shouldKeep,
    ShouldCancel&& shouldCancel) {
  size_t unloadCount = 0;

  if (self->isRestricted()) {
    return unloadCount;
  }

  // Recurse into children here. Children hold strong references to their
  // parent trees, so unloading children can cause the parent to become
  // unreferenced.
  for (auto& child : treeChildren) {
    if (shouldCancel()) {
      break;
    }
    unloadCount += recurse(*child);
  }

  // Release the treeChildren refcounts.
  treeChildren.clear();

  // Unload children whose reference count is zero.
  std::vector<unique_ptr<InodeBase>> toDelete;
  {
    auto contents = self->getContentsUnchecked().wlock();
    auto inodeMapLock = inodeMap->lockForUnload();

    // A listing that stopped before its end, or an NFS listing, leaves its
    // readdir index behind until the directory changes; GC is the backstop
    // that bounds how long that memory is held.
    self->dropReaddirIndex();

    for (auto& entry : contents->entries.all()) {
      if (shouldCancel()) {
        break;
      }
      auto* entryInode = entry.second.getInode();
      if (!entryInode) {
        continue;
      }

      // Check isPtrAcquireCountZero() first. It's a single load instruction
      // on x86 and if the predicate calls getFuseRefcount(), it will assert
      // if isPtrAcquireCountZero() is false.
      if (entryInode->isPtrAcquireCountZero() && predicate(entryInode)) {
        if (shouldKeep(entryInode, inodeMapLock)) {
          continue;
        }

        // If it's a tree and it has a loaded child, its refcount will never
        // be zero because the child holds a reference to its parent.

        // Allocate space in the vector. This can throw std::bad_alloc.
        toDelete.emplace_back();

        // Forget other pointer references to this inode.
        (void)entry.second.clearInode(); // clearInode will not throw.
        inodeMap->unloadInode(
            entryInode, self, entry.first, false, inodeMapLock);

        // If unloadInode threw, we'll leak the entryInode, but it's no big
        // deal. This assignment cannot throw.
        toDelete.back() = unique_ptr<InodeBase>{entryInode};
      }
    }
  }

  unloadCount += toDelete.size();
  // Outside of the locks, deallocate all of the inodes scheduled to be
  // deleted.
  toDelete.clear();

  return unloadCount;
}

template <typename ShouldCancel>
std::vector<TreeInodePtr> getTreeChildren(
    TreeInode* self,
    ShouldCancel&& shouldCancel) {
  std::vector<TreeInodePtr> treeChildren;
  {
    auto contents = self->getContentsUnchecked().rlock();
    for (auto& entry : contents->entries.all()) {
      if (shouldCancel()) {
        break;
      }
      if (!entry.second.getInode()) {
        continue;
      }

      // This has the side effect of incrementing the reference counts of
      // all of the children. When that goes back to zero,
      // InodeMap::onInodeUnreferenced will be called on the entry.
      if (auto asTree = entry.second.asTreePtrOrNull()) {
        treeChildren.emplace_back(std::move(asTree));
      }
    }
  }
  return treeChildren;
}

} // namespace

void TreeInode::dropReaddirIndex() {
  if (std::exchange(*readdirIndex_.wlock(), nullptr)) {
    getMount()->getStats()->increment(&TreeInodeStats::readdirIndexDroppedByGc);
  }
}

size_t TreeInode::unloadChildrenNow() {
  auto neverCancel = [] { return false; };
  auto treeChildren = getTreeChildren(this, neverCancel);
  return unloadChildrenIf(
      this,
      getInodeMap(),
      treeChildren,
      [](TreeInode& child) { return child.unloadChildrenNow(); },
      [](InodeBase*) { return true; },
      [](InodeBase*, const InodeMapLock&) { return false; },
      neverCancel);
}

void TreeInode::recheckHiddenRestrictedDescendants(
    const ObjectFetchContextPtr& context) {
  // A restricted directory has no readable contents and no loaded children:
  // there is nothing to walk below it, and it is itself rechecked by its
  // parent through the hidden list. Reading its contents would throw EACCES.
  if (isRestricted()) {
    return;
  }
  // What goes stale here is the kernel's cached listing of this directory, so
  // a directory the kernel does not hold has nothing to refresh and does not
  // justify a check_permission call. The filter is on the parent: a hidden
  // child is absent from the listing, so it never gets a dentry of its own.
  // The refcount read races with the kernel, which only costs a tick.
  if (debugGetFsRefcount() != 0) {
    std::vector<PathComponent> hidden;
    {
      auto contents = lockContentsRead();
      for (const auto& entry : contents->entries.all()) {
        if (entry.second.isRestricted()) {
          hidden.emplace_back(entry.first);
        }
      }
    }
    // Enqueued after the contents lock is released: the pool threads take that
    // same lock to load the child, so they would park until this call returned.
    recheckHiddenRestrictedChildren(hidden, context);
  }

  auto neverCancel = [] { return false; };
  for (const auto& child : getTreeChildren(this, neverCancel)) {
    child->recheckHiddenRestrictedDescendants(context);
  }
}

/**
 * Whether the GC sweep may unload an inode nobody holds: it must have no
 * filesystem reference and, when a cutoff is given, not have been referenced
 * since it. An inode the kernel released reads as never referenced, so it is
 * always eligible; see InodeBase::lastGcRefTime_.
 */
static bool isUnreferencedForSweep(
    const InodeBase* inode,
    const std::optional<EdenTimestamp>& cutoff) {
  if (inode->getFsRefcount() != 0) {
    return false;
  }
  return !cutoff || inode->getLastGcRefTime() < *cutoff;
}

size_t TreeInode::unloadChildrenUnreferencedByFs(
    const folly::CancellationToken& cancellationToken,
    std::optional<EdenTimestamp> cutoff) {
  auto shouldCancel = [&] {
    return cancellationToken.isCancellationRequested();
  };
  auto treeChildren = getTreeChildren(this, shouldCancel);
  return unloadChildrenIf(
      this,
      getInodeMap(),
      treeChildren,
      [&cancellationToken, cutoff](TreeInode& child) {
        return child.unloadChildrenUnreferencedByFs(cancellationToken, cutoff);
      },
      [cutoff](InodeBase* child) {
        return isUnreferencedForSweep(child, cutoff);
      },
      [](InodeBase*, const InodeMapLock&) { return false; },
      shouldCancel);
}

TreeInode::InodeGCUnloadResult
TreeInode::unloadChildrenUnreferencedByFsForInodeGC(
    const folly::CancellationToken& cancellationToken,
    std::optional<EdenTimestamp> cutoff) {
  InodeGCUnloadResult result;
  auto shouldCancel = [&] {
    return cancellationToken.isCancellationRequested();
  };
  auto treeChildren = getTreeChildren(this, shouldCancel);
  result.unloaded = unloadChildrenIf(
      this,
      getInodeMap(),
      treeChildren,
      [&cancellationToken, &result, cutoff](TreeInode& child) {
        auto childResult = child.unloadChildrenUnreferencedByFsForInodeGC(
            cancellationToken, cutoff);
        result.zeroFsRefTreesRetained += childResult.zeroFsRefTreesRetained;
        return childResult.unloaded;
      },
      [cutoff](InodeBase* child) {
        return isUnreferencedForSweep(child, cutoff);
      },
      [inodeMap = getInodeMap(), &result](
          InodeBase* child, const InodeMapLock& lock) {
        auto* tree = dynamic_cast<TreeInode*>(child);
        if (!tree || !inodeMap->hasRememberedChildForUnload(*tree, lock)) {
          return false;
        }
        result.zeroFsRefTreesRetained++;
        return true;
      },
      shouldCancel);
  return result;
}

namespace {
using NamedTreeInode = std::pair<PathComponent, TreeInodePtr>;

ImmediateFuture<std::vector<NamedTreeInode>> getLoadedOrRememberedTreeChildren(
    TreeInode* self,
    InodeMap* const inodeMap,
    const ObjectFetchContextPtr& context,
    const folly::CancellationToken& cancellationToken) {
  std::vector<ImmediateFuture<NamedTreeInode>> res;
  std::vector<InodeMap::UnloadedInodeGcCandidate> unloadedCandidates;
  {
    auto contents = self->getContentsUnchecked().rlock();
    for (auto& entry : contents->entries.all()) {
      if (cancellationToken.isCancellationRequested()) {
        break;
      }
      if (!entry.second.isDirectory()) {
        continue;
      }

      if (auto treePtr = entry.second.asTreePtrOrNull()) {
        res.emplace_back(
            std::make_pair(PathComponent{entry.first}, std::move(treePtr)));
        continue;
      }

      // In invalidateChildrenNotMaterialized we want to walk all the directory
      // inodes that are present on disk so we can have a chance to invalidate
      // them. Since inodes can be unloaded but still have an fs refcount set,
      // we need to make sure to load them so we can crawl them.
      unloadedCandidates.push_back(
          {entry.second.getInodeNumber(), PathComponent{entry.first}});
    }
  }

  // TODO(xavierd): We could use VirtualInode here to avoid loading inodes
  // unnecessarily.
  //
  // Look up unloaded children in bounded batches so one wide directory cannot
  // hold the InodeMap lock for its entire entry list.
  constexpr size_t kUnloadedChildLookupBatchSize = 1'024;
  std::vector<InodeMap::UnloadedInodeGcCandidate> lookupBatch;
  for (size_t begin = 0; begin < unloadedCandidates.size();
       begin += kUnloadedChildLookupBatchSize) {
    if (cancellationToken.isCancellationRequested()) {
      break;
    }
    auto end = std::min(
        begin + kUnloadedChildLookupBatchSize, unloadedCandidates.size());
    lookupBatch.assign(
        std::make_move_iterator(unloadedCandidates.begin() + begin),
        std::make_move_iterator(unloadedCandidates.begin() + end));
    auto unloadedChildren =
        inodeMap->getUnloadedChildrenForGc(self->getNodeId(), lookupBatch);
    for (auto& child : unloadedChildren) {
      if (cancellationToken.isCancellationRequested()) {
        break;
      }
      auto name = std::move(child.name);
      res.push_back(self->getOrLoadChildTree(name, context)
                        .thenTry([name, mount = self->getMount()](
                                     folly::Try<TreeInodePtr>&& tree) {
                          if (tree.hasException()) {
                            mount->recordInodeGCTreeLoadFailure();
                          }
                          return std::make_pair(name, std::move(tree).value());
                        }));
    }
  }
  return collectAllSafe(std::move(res));
}

/**
 * Check for early termination conditions for garbage collection.
 */
bool shouldCancelGC(
    const folly::CancellationToken& cancellationToken,
    const EdenMount* mount = nullptr) {
  if (mount != nullptr &&
      mount->getState() == EdenMount::State::SHUTTING_DOWN) {
    return true;
  }
  if (cancellationToken.isCancellationRequested()) {
    return true;
  }
  return false;
}

/**
 * Process tree children recursively and return a vector of results.
 * Applies the given processor function to each child tree.
 */
template <typename Func>
ImmediateFuture<std::vector<
    typename std::invoke_result_t<Func, PathComponentPiece, TreeInodePtr>::
        value_type>>
processTreeChildren(
    TreeInode* self,
    InodeMap* inodeMap,
    const ObjectFetchContextPtr& context,
    const folly::CancellationToken& cancellationToken,
    Func&& childProcessor) {
  return getLoadedOrRememberedTreeChildren(
             self, inodeMap, context, cancellationToken)
      .thenValue([childProcessor = std::forward<Func>(childProcessor),
                  cancellationToken](
                     const std::vector<NamedTreeInode>& treeChildren) mutable {
        using ResultType = typename std::
            invoke_result_t<Func, PathComponentPiece, TreeInodePtr>::value_type;

        // Check for cancellation before processing children
        if (shouldCancelGC(cancellationToken)) {
          return ImmediateFuture<std::vector<ResultType>>(
              std::vector<ResultType>());
        }

        std::vector<ImmediateFuture<ResultType>> futures;
        futures.reserve(treeChildren.size());
        for (auto& [name, tree] : treeChildren) {
          if (shouldCancelGC(cancellationToken)) {
            break;
          }
          futures.push_back(childProcessor(name.piece(), tree));
        }

        // Cancellation stops new work, but the child walks already started
        // hold inode references and may have chmods in flight, so they are
        // joined before the GC lease is released.
        return collectAllSafe(std::move(futures));
      });
}

} // namespace

ImmediateFuture<uint64_t /* numInvalidated */>
TreeInode::handleChildrenNotAccessedRecently(
    std::chrono::system_clock::time_point cutoff,
    const ObjectFetchContextPtr& context,
    [[maybe_unused]] bool pressureBased,
    folly::CancellationToken cancellationToken,
    [[maybe_unused]] std::shared_ptr<const folly::F14FastSet<InodeNumber>>
        pinnedInodes) {
  if (getMount()->getNfsdChannel()) {
    return invalidateChildrenNotMaterializedNFS(
               cutoff, context, cancellationToken, std::move(pinnedInodes))
        .thenValue([](NfsGcResult result) { return result.numInvalidated; });

  } else if (getMount()->getPrjfsChannel()) {
    return invalidateChildrenNotMaterializedPrjFS(
        cutoff, context, cancellationToken);
  }
#ifndef _WIN32
  if (pressureBased) {
    // Pressure-based GC: actively invalidate old FUSE dcache entries.
    // This triggers FORGET from the kernel, which decrements fsRefcount
    // and allows subsequent unloading.
    return invalidateChildrenNotAccessedRecentlyFuse(
        cutoff, context, cancellationToken, std::move(pinnedInodes));
  }
  // Legacy FUSE path: passively unload inodes that are no longer referenced.
  // FUSE decreases the FS ref count by itself. On FUSE, we don't invalidate
  // any inode as the first step of GC. However, we can unload not recently
  // used inodes to save eden resident memory.
  auto unloaded = unloadChildrenLastAccessedBefore(
      folly::to<timespec>(cutoff), cancellationToken);
  if (unloaded) {
    XLOGF(
        DBG6,
        "Unloaded {} inodes in background from mount {}",
        unloaded,
        getMount()->getPath());
  }
#endif

  // number of invalidations on Linux is zero
  return ImmediateFuture<uint64_t>{0ULL};
}

#ifndef _WIN32
namespace {
constexpr size_t kFuseGcDirectoryScanBatchSize = 1'024;
constexpr size_t kMaxQueuedFuseGcInvalidations = 1'024;

folly::Expected<std::shared_ptr<GcBarrierTrie>, int> getGcBarrierTrie(
    EdenMount* mount) {
  auto gcBarrier = std::make_shared<GcBarrierTrie>();
  for (const auto& vcsDirectory :
       mount->getEdenConfig()->vcsDirectories.getValue()) {
    gcBarrier->add(vcsDirectory);
  }

#ifdef __linux__
  const auto& mountPath = mount->getPath().asString();
  auto mountedPaths = getMountsUnderPath(mountPath);
  if (mountedPaths.hasError()) {
    return folly::makeUnexpected(mountedPaths.error());
  }

  auto mountPathPrefix = mountPath + "/";
  for (const auto& mountedPath : mountedPaths.value()) {
    if (mountedPath.mountPoint.compare(
            0, mountPathPrefix.size(), mountPathPrefix) != 0) {
      continue;
    }
    gcBarrier->add(
        RelativePathPiece{
            mountedPath.mountPoint.substr(mountPathPrefix.size())});
  }
#endif

  return gcBarrier;
}
} // namespace

ImmediateFuture<uint64_t> TreeInode::invalidateChildrenNotAccessedRecentlyFuse(
    std::chrono::system_clock::time_point cutoff,
    const ObjectFetchContextPtr& context,
    const folly::CancellationToken& cancellationToken,
    std::shared_ptr<const folly::F14FastSet<InodeNumber>> pinnedInodes) {
  auto gcBarrier = getGcBarrierTrie(getMount());
  if (gcBarrier.hasError()) {
    XLOGF(
        WARN,
        "Skipping active FUSE GC for {} because mount table enumeration failed: {} ({})",
        getMount()->getPath(),
        gcBarrier.error(),
        std::strerror(gcBarrier.error()));
    return ImmediateFuture<uint64_t>{0ULL};
  }

  auto path = getPath();
  if (!path.has_value()) {
    XLOGF(
        DBG4,
        "Skipping active FUSE GC for unlinked tree inode {}",
        getLogPath());
    return ImmediateFuture<uint64_t>{0ULL};
  }
  auto* currentGcBarrier = gcBarrier.value()->getDescendant(*path);
  auto fuseChannel = getMount()->getFuseChannelShared();
  if (!fuseChannel) {
    return ImmediateFuture<uint64_t>{0ULL};
  }
  auto invalidationExecutor = getMount()->getInodeGCInvalidationExecutor();

  return invalidateChildrenNotAccessedRecentlyFuseImpl(
             cutoff,
             context,
             cancellationToken,
             gcBarrier.value(),
             currentGcBarrier,
             pinnedInodes,
             fuseChannel.get(),
             folly::getKeepAliveToken(invalidationExecutor.get()))
      .thenValue([](FuseGcResult result) { return result.numInvalidated; })
      .ensure([fuseChannel = std::move(fuseChannel),
               invalidationExecutor = std::move(invalidationExecutor)] {});
}

ImmediateFuture<TreeInode::FuseGcResult>
TreeInode::invalidateChildrenNotAccessedRecentlyFuseImpl(
    std::chrono::system_clock::time_point cutoff,
    const ObjectFetchContextPtr& context,
    const folly::CancellationToken& cancellationToken,
    const std::shared_ptr<const GcBarrierTrie>& gcBarrier,
    const GcBarrierTrie* FOLLY_NULLABLE currentGcBarrier,
    const std::shared_ptr<const folly::F14FastSet<InodeNumber>>& pinnedInodes,
    FuseChannel* fuseChannel,
    folly::Executor::KeepAlive<> invalidationExecutor) {
  if (shouldCancelGC(cancellationToken, getMount())) {
    // This subtree was not examined, so report it as possibly-pinned to keep
    // ancestors from invalidating its entry.
    return FuseGcResult{0, /*containsPin=*/true};
  }
  if (currentGcBarrier != nullptr) {
    if (currentGcBarrier->isMountRoot) {
      return FuseGcResult{0, /*containsPin=*/false};
    }
  }

  // First, recursively process child tree inodes (bottom-up invalidation).
  return processTreeChildren(
             this,
             getInodeMap(),
             context,
             cancellationToken,
             [cutoff,
              gcBarrier,
              currentGcBarrier,
              pinnedInodes,
              context = context.copy(),
              cancellationToken,
              fuseChannel,
              invalidationExecutor](
                 PathComponentPiece name, TreeInodePtr tree) {
               const GcBarrierTrie* FOLLY_NULLABLE childGcBarrier = nullptr;
               if (currentGcBarrier != nullptr) {
                 childGcBarrier = currentGcBarrier->getChild(name);
               }
               return tree
                   ->invalidateChildrenNotAccessedRecentlyFuseImpl(
                       cutoff,
                       context,
                       cancellationToken,
                       gcBarrier,
                       childGcBarrier,
                       pinnedInodes,
                       fuseChannel,
                       invalidationExecutor)
                   .thenValue([ino = tree->getNodeId()](FuseGcResult result) {
                     return std::make_pair(ino, result);
                   });
             })
      .semi()
      .via(std::move(invalidationExecutor))
      .thenValue([self = inodePtrFromThis(),
                  cutoff,
                  cancellationToken,
                  gcBarrier,
                  currentGcBarrier,
                  pinnedInodes,
                  fuseChannel](
                     const std::vector<std::pair<InodeNumber, FuseGcResult>>&
                         childResults) {
        // Keep the trie alive while this continuation uses raw pointers into
        // it.
        (void)gcBarrier;
        bool containsPin =
            pinnedInodes && pinnedInodes->count(self->getNodeId());
        uint64_t numInvalidated = 0;
        folly::F14FastSet<InodeNumber> pinnedChildren;
        for (const auto& [childIno, childResult] : childResults) {
          numInvalidated += childResult.numInvalidated;
          if (childResult.containsPin) {
            containsPin = true;
            pinnedChildren.insert(childIno);
          }
        }
        if (shouldCancelGC(cancellationToken)) {
          return FuseGcResult{numInvalidated, containsPin};
        }

        uint64_t numSkippedParentNoFsRef = 0;
        uint64_t numSkippedChildNoFsRef = 0;
        uint64_t numSkippedPinned = 0;
        std::optional<PathComponent> lastExamined;
        std::vector<PathComponent> staleEntriesToInvalidate;
        std::vector<InodeMap::UnloadedInodeGcCandidate> unloadedCandidates;
        bool reachedEnd = false;
        while (!reachedEnd && !shouldCancelGC(cancellationToken)) {
          staleEntriesToInvalidate.clear();
          unloadedCandidates.clear();
          const bool parentHasFsRef = self->debugGetFsRefcount() != 0;
          {
            auto contents = self->getContentsUnchecked().rlock();
            if (!lastExamined) {
              staleEntriesToInvalidate.reserve(
                  std::min(
                      contents->entries.size(), kFuseGcDirectoryScanBatchSize));
            }
            auto entry = contents->entries.all().begin();
            if (lastExamined) {
              entry = contents->entries.find(lastExamined->piece());
              if (entry != contents->entries.end()) {
                ++entry;
              } else {
                entry = contents->entries.lower_bound(lastExamined->piece());
              }
            }

            size_t numExamined = 0;
            for (; entry != contents->entries.end() &&
                 numExamined < kFuseGcDirectoryScanBatchSize;
                 ++entry, ++numExamined) {
              lastExamined = PathComponent{entry->first};
              if (shouldCancelGC(cancellationToken)) {
                break;
              }

              const GcBarrierTrie* FOLLY_NULLABLE childGcBarrier = nullptr;
              if (currentGcBarrier != nullptr) {
                childGcBarrier =
                    currentGcBarrier->getChild(entry->first.piece());
              }
              if (childGcBarrier) {
                continue;
              }

              // Never invalidate the entry of a directory that is pinned as
              // some process's cwd/root or whose subtree contains such a pin
              // (see FuseGcResult::containsPin), and without pin information
              // leave all directory entries alone. Pins are always
              // directories, so file entries need no checks.
              if (entry->second.isDirectory()) {
                const auto childIno = entry->second.getInodeNumber();
                if (!pinnedInodes || pinnedChildren.count(childIno) ||
                    pinnedInodes->count(childIno)) {
                  numSkippedPinned++;
                  continue;
                }
              }

              auto* entryInode = entry->second.getInode();
              if (!entryInode) {
                if (!entry->second.isDirectory()) {
                  unloadedCandidates.push_back(
                      {entry->second.getInodeNumber(),
                       PathComponent{entry->first}});
                }
                continue;
              }

              auto lastGcRefTime = std::chrono::system_clock::from_time_t(
                  entryInode->getLastGcRefTime().toTimespec().tv_sec);
              if (lastGcRefTime >= cutoff) {
                continue;
              }

              // These refcount checks are racy best-effort optimizations. An
              // invalidation cannot produce a FORGET if the kernel has already
              // dropped either the parent or child reference.
              if (!parentHasFsRef) {
                numSkippedParentNoFsRef++;
                continue;
              }
              if (entryInode->debugGetFsRefcount() == 0) {
                numSkippedChildNoFsRef++;
                continue;
              }
              staleEntriesToInvalidate.emplace_back(entry->first);
            }
            reachedEnd = entry == contents->entries.end();
          }

          auto unloadedChildren = self->getInodeMap()->getUnloadedChildrenForGc(
              self->getNodeId(), unloadedCandidates);
          for (auto& child : unloadedChildren) {
            if (shouldCancelGC(cancellationToken)) {
              return FuseGcResult{numInvalidated, containsPin};
            }
            auto lastGcRefTime = std::chrono::system_clock::from_time_t(
                child.lastGcRefTime.toTimespec().tv_sec);
            if (lastGcRefTime >= cutoff) {
              continue;
            }
            if (!parentHasFsRef) {
              numSkippedParentNoFsRef++;
              continue;
            }
            if (child.numFsReferences == 0) {
              numSkippedChildNoFsRef++;
              continue;
            }
            staleEntriesToInvalidate.push_back(std::move(child.name));
          }

          // Queue backpressure can block, so submit only after releasing the
          // inode contents lock. Each queued invalidation causes the kernel to
          // drop its dcache entry and asynchronously send FORGET.
          for (const auto& name : staleEntriesToInvalidate) {
            if (!fuseChannel->invalidateEntryWithQueueLimit(
                    self->getNodeId(),
                    name.piece(),
                    kMaxQueuedFuseGcInvalidations,
                    cancellationToken)) {
              return FuseGcResult{numInvalidated, containsPin};
            }
            numInvalidated++;
          }
        }

        if (numInvalidated > 0) {
          XLOGF(
              DBG9,
              "FUSE GC invalidated {} entries under {}",
              numInvalidated,
              self->getLogPath());
        }
        if (numSkippedParentNoFsRef > 0 || numSkippedChildNoFsRef > 0 ||
            numSkippedPinned > 0) {
          XLOGF(
              DBG9,
              "FUSE GC skipped invalidating entries under {}: parentNoFsRef={}, childNoFsRef={}, pinned={}",
              self->getLogPath(),
              numSkippedParentNoFsRef,
              numSkippedChildNoFsRef,
              numSkippedPinned);
        }

        return FuseGcResult{numInvalidated, containsPin};
      });
}
#endif

namespace {
/**
 * One directory's contribution to the NFS GC walk. `done` is set when the
 * directory queued its own invalidation; see Nfsd3::invalidateWithQueueLimit
 * for what it completes with.
 */
struct NfsGcStep {
  uint64_t numInvalidated{0};
  bool invalidated{false};
  bool containsPin{false};
  std::optional<folly::SemiFuture<std::optional<uint64_t>>> done;
};
} // namespace

ImmediateFuture<NfsGcResult> TreeInode::invalidateChildrenNotMaterializedNFS(
    std::chrono::system_clock::time_point cutoff,
    const ObjectFetchContextPtr& context,
    folly::CancellationToken cancellationToken,
    std::shared_ptr<const folly::F14FastSet<InodeNumber>> pinnedInodes) {
  if (shouldCancelGC(cancellationToken, getMount())) {
    // This subtree was not examined, so report it as possibly pinned to keep
    // ancestors from clearing its reference.
    return NfsGcResult{0, false, /*containsPin=*/true};
  }
  if (getNodeId() == getMount()->getDotEdenInodeNumber()) {
    // EdenFS's own directory, which tools resolve constantly: nothing to
    // gain from forgetting it or its handful of entries. Report it as done
    // and pinned, so the root goes on without clearing its reference.
    return NfsGcResult{0, /*invalidated=*/true, /*containsPin=*/true};
  }

  auto childResults = processTreeChildren(
      this,
      getInodeMap(),
      context,
      cancellationToken,
      [cutoff, context = context.copy(), cancellationToken, pinnedInodes](
          PathComponentPiece /*name*/, TreeInodePtr tree) {
        return tree
            ->invalidateChildrenNotMaterializedNFS(
                cutoff, context, cancellationToken, pinnedInodes)
            .thenValue([ino = tree->getNodeId()](NfsGcResult result) {
              return std::make_pair(ino, result);
            });
      });

  // Deciding whether to invalidate this directory and queuing the
  // invalidation run on the GC invalidation executor, as the FUSE walk does:
  // queuing may wait for the invalidation queue to drain, which must not hold
  // up the threads that serve requests. The continuation is attached to the
  // folly::Future from via() rather than an ImmediateFuture, which would run
  // it inline on the calling thread whenever the hop had already completed.
  // The decision captures the executor to keep it alive for as long as the
  // continuation, which the keep-alive token via() is given does not do.
  std::shared_ptr<UnboundedQueueExecutor> invalidationExecutor;
#ifndef _WIN32
  invalidationExecutor = getMount()->getInodeGCInvalidationExecutor();
#endif
  auto decide =
      [self = inodePtrFromThis(),
       cutoff,
       cancellationToken,
       pinnedInodes = std::move(pinnedInodes),
       invalidationExecutor](
          const std::vector<std::pair<InodeNumber, NfsGcResult>>& childResults)
      -> NfsGcStep {
    NfsGcStep step;
    step.containsPin = pinnedInodes && pinnedInodes->count(self->getNodeId());
    // Check for cancellation before processing results
    if (shouldCancelGC(cancellationToken)) {
      step.containsPin = true;
      return step;
    }

    bool allDescendantsInvalidated = true;
    folly::F14FastSet<InodeNumber> pinnedChildren;
    for (const auto& [childIno, childResult] : childResults) {
      step.numInvalidated += childResult.numInvalidated;
      if (!childResult.invalidated) {
        allDescendantsInvalidated = false;
      }
      if (childResult.containsPin) {
        step.containsPin = true;
        pinnedChildren.insert(childIno);
      }
    }

    if (!self->getPath().has_value()) {
      // This directory was removed, no need to do anything.
      step.invalidated = true;
      return step;
    }

    auto contents = self->lockContentsWrite();
    if (pinnedInodes && !step.containsPin) {
      // Pinned directories reported themselves above; pinned files are
      // only visible here.
      for (auto& entry : contents->entries.all()) {
        if (!entry.second.isDirectory() &&
            pinnedInodes->count(entry.second.getInodeNumber())) {
          step.containsPin = true;
          break;
        }
      }
    }
    if (!allDescendantsInvalidated) {
      // If any of the children are not invalidated, we should skip
      // invalidation of this directory.
      return step;
    }
    // A materialized directory is reclaimed like any other: its state is in
    // the overlay, so an unloaded child reloads from there, as on FUSE.

    // if cutoff is max, we should invalidate everything, so we don't
    // need to check the last fs request time
    bool shouldInvalidate =
        (cutoff == std::chrono::system_clock::time_point::max());
    if (!shouldInvalidate) {
      auto lastGcRefTime = std::chrono::system_clock::from_time_t(
          self->getLastGcRefTime().toTimespec().tv_sec);
      // As we didn't update parent's last fs request time when children
      // are accessed via the fs channel dispatcher, we need to check
      // the children's last fs request time here.
      for (auto& entry : contents->entries.all()) {
        auto* entryInode = entry.second.getInode();
        if (!entryInode) {
          continue;
        }
        auto childLastGcRefTime = std::chrono::system_clock::from_time_t(
            entryInode->getLastGcRefTime().toTimespec().tv_sec);
        if (lastGcRefTime < childLastGcRefTime) {
          lastGcRefTime = childLastGcRefTime;
        }
      }
      shouldInvalidate = (lastGcRefTime < cutoff);
      XLOGF(
          DBG9,
          "For path: {}, last fs request time: {}, cutoff: {}, shouldInvalidate by GC is {}",
          self->getPath().value().asString(),
          self->getLastGcRefTime().toTimespec().tv_sec,
          cutoff.time_since_epoch().count(),
          shouldInvalidate);
    }
    if (!shouldInvalidate) {
      return step;
    }

    // Attempt to invalidate the directory, and then clear the FS
    // references of all of its children. The call order here is
    // recursively bottom-up. The directory itself is cleared later by its
    // parent's invalidation.

    // Check for cancellation before invalidation
    if (shouldCancelGC(cancellationToken)) {
      step.numInvalidated = 0;
      return step;
    }

    // The invalidation exists to clear the children's FS references. A
    // child that is neither loaded nor remembered has none, and a
    // directory child is left alone without pin information, so when
    // no child would be cleared the chmod would do nothing. Skip it, but
    // let the parent proceed as if this directory had been invalidated
    // so it can clear this directory's own reference.
    auto* inodeMap = self->getInodeMap();
    bool anyChildReferenced = false;
    for (auto& entry : contents->entries.all()) {
      if (!nfsGcMayClearChild(entry.second, pinnedInodes, pinnedChildren)) {
        continue;
      }
      if (entry.second.getInode() ||
          inodeMap->isInodeRemembered(entry.second.getInodeNumber())) {
        anyChildReferenced = true;
        break;
      }
    }
    if (!anyChildReferenced) {
      step.invalidated = true;
      return step;
    }
#ifndef _WIN32
    auto prepared = self->nfsPrepareGcInvalidation(
        *contents, pinnedInodes, std::move(pinnedChildren));
    if (!prepared) {
      return step;
    }
    // Queue with the contents lock released: the channel bounds how many
    // GC invalidations may be queued at once, so this may wait.
    contents.unlock();
    auto* channel = self->getMount()->getNfsdChannel();
    if (!channel) {
      return step;
    }
    const auto maxQueued = self->getMount()
                               ->getEdenConfig()
                               ->nfsMaxQueuedGcInvalidations.getValue();
    if (auto done = channel->invalidateWithQueueLimit(
            std::move(prepared->path),
            prepared->mode,
            std::move(prepared->forget),
            std::move(prepared->lineage),
            maxQueued,
            cancellationToken)) {
      step.done = std::move(done);
    }
#endif
    return step;
  };

#ifndef _WIN32
  ImmediateFuture<NfsGcStep> stepFuture =
      std::move(childResults)
          .semi()
          .via(folly::getKeepAliveToken(invalidationExecutor.get()))
          .thenValue(std::move(decide));
#else
  ImmediateFuture<NfsGcStep> stepFuture =
      std::move(childResults).thenValue(std::move(decide));
#endif

  return std::move(stepFuture)
      .thenValue([](NfsGcStep&& step) -> ImmediateFuture<NfsGcResult> {
        NfsGcResult result{
            step.numInvalidated, step.invalidated, step.containsPin};
        if (!step.done) {
          return result;
        }
        // Wait for this directory's own chmod rather than for the whole
        // queue to drain, so chmods of unrelated directories can be in
        // flight at once when the channel has several invalidation threads.
        // A cancelled walk waits too: the chmod was accepted, its forget
        // callback holds inode references, and the SETATTR it turns into
        // needs the channel to still be serving.
        // The directory counts as invalidated only if its chmod reached
        // EdenFS and the children were cleared, and it contributes the
        // number of children whose FS reference was cleared, matching what
        // the FUSE pass counts. A broken future means the channel stopped
        // first.
        return ImmediateFuture<std::optional<uint64_t>>{std::move(*step.done)}
            .thenTry([result](folly::Try<std::optional<uint64_t>>&& cleared) {
              auto finished = result;
              if (cleared.hasException()) {
                // The channel stopped first, or forget threw.
                XLOGF(
                    WARN,
                    "NFS GC invalidation did not complete: {}",
                    cleared.exception().what());
              }
              if (cleared.hasValue() && cleared->has_value()) {
                finished.numInvalidated += **cleared;
                finished.invalidated = true;
              } else {
                finished.invalidated = false;
              }
              return finished;
            });
      });
}

ImmediateFuture<uint64_t /* numInvalidated */>
TreeInode::invalidateChildrenNotMaterializedPrjFS(
    std::chrono::system_clock::time_point cutoff,
    const ObjectFetchContextPtr& context,
    folly::CancellationToken cancellationToken) {
  if (shouldCancelGC(cancellationToken, getMount())) {
    return 0ULL;
  }

  return processTreeChildren(
             this,
             getInodeMap(),
             context,
             cancellationToken,
             [cutoff, context = context.copy(), cancellationToken](
                 PathComponentPiece /*name*/, TreeInodePtr tree) {
               return tree->invalidateChildrenNotMaterializedPrjFS(
                   cutoff, context, cancellationToken);
             })
      .thenValue(
          [self = inodePtrFromThis(), cutoff, cancellationToken](
              const std::vector<uint64_t>& invalidations) -> uint64_t {
            // Check for cancellation before processing results
            if (shouldCancelGC(cancellationToken)) {
              return 0ULL;
            }

            uint64_t numInvalidated = 0;

            for (auto invalidation : invalidations) {
              numInvalidated += invalidation;
            }

            std::vector<InodePtr> deletedInodes;
            {
              AbsolutePath selfPath;
              if (auto path = self->getPath()) {
                selfPath = self->getMount()->getPath() + path.value();
              } else {
                // This directory was removed, no need to do anything.
                return numInvalidated;
              }

              auto contents = self->lockContentsWrite();
              auto* inodeMap = self->getInodeMap();
              for (auto& entry : contents->entries.all()) {
                if (entry.second.isMaterialized()) {
                  continue;
                }

                if (auto inode = entry.second.getInodePtr()) {
                  deletedInodes.push_back(std::move(inode));
                }

                auto inodeNumber = entry.second.getInodeNumber();
                if (!inodeMap->isInodeLoadedOrRemembered(inodeNumber)) {
                  continue;
                }

                // If we're attempting to invalidate everything, don't bother
                // checking the disk.
                if (cutoff != std::chrono::system_clock::time_point::max()) {
                  // Let's focus only on files as directories will get their
                  // atime updated when we query the atime of the files
                  // contained in it.
                  if (!entry.second.isDirectory()) {
#ifdef _WIN32
                    auto entryPath = selfPath + entry.first;
                    auto wEntryPath = entryPath.wide();
                    struct __stat64 buf;

                    // TODO: If the file isn't on disk this will lay a
                    // placeholder on disk and at the same time force it to not
                    // be invalidated due to its atime being newer than the
                    // cutoff.
                    if (_wstat64(wEntryPath.c_str(), &buf) < 0) {
                      continue;
                    }

                    auto atime =
                        std::chrono::system_clock::from_time_t(buf.st_atime);
                    if (atime > cutoff) {
                      // That file has been touched too recently, continue.
                      continue;
                    }
#else
                    // This function should not get called from non-windows
                    // platforms
                    continue;
#endif
                  }
                }

                // TODO: In the case where the file becomes materialized on disk
                // now, invalidateChannelEntryCache will happily remove it,
                // leading to a potential loss of user data. To avoid this, we
                // could try not passing PRJ_UPDATE_ALLOW_DIRTY_DATA and dealing
                // with the side effects to close that race.

                // Here, we rely on invalidateChannelEntryCache failing for
                // non-empty directories to guarantee that we're not losing user
                // data in the case where a user writes a file in a directory
                // that we're attempting to invalidate. For directories with not
                // invalidated children due to being read too recently, we also
                // rely on invalidateChannelEntryCache failing.
                auto invalidateResult = self->invalidateChannelEntryCache(
                    *contents, entry.first, inodeNumber);
                if (invalidateResult.hasException()) {
                  XLOGF(
                      DBG5,
                      "Couldn't invalidate: {}/{}: {}",
                      self->getLogPath(),
                      entry.first,
                      invalidateResult.exception());
                } else {
                  numInvalidated++;
                }
              }
            }

            return numInvalidated;
          });
}

void TreeInode::updateAtime() {
  if (FOLLY_UNLIKELY(isRestricted())) {
    return;
  }
  auto lock = lockContentsWrite();
  InodeBaseMetadata::updateAtimeLocked(lock->entries);
}

void TreeInode::forceMetadataUpdate() {
  // Restricted inodes have synthetic zeroed metadata (from stat()), so
  // timestamp updates are meaningless. Silent skip also prevents EACCES
  // from propagating through the NFS invalidation Thrift path.
  if (FOLLY_UNLIKELY(isRestricted())) {
    return;
  }
  auto contents = lockContentsWrite();
  InodeBaseMetadata::updateMtimeAndCtimeLocked(contents->entries, getNow());
}

#ifndef _WIN32
ImmediateFuture<folly::Unit> TreeInode::ensureMaterialized(
    const ObjectFetchContextPtr& fetchContext,
    bool followSymlink) {
  std::vector<ImmediateFuture<folly::Unit>> childFutures;
  std::vector<PathComponent> names;
  {
    auto contents = lockContentsRead();
    names.reserve(contents->entries.size());
    for (auto& entry : contents->entries.all()) {
      names.emplace_back(entry.first);
    }
  }

  childFutures.reserve(names.size());
  for (auto& name : names) {
    childFutures.emplace_back(
        getOrLoadChild(name, fetchContext)
            .thenValue([fetchContext = fetchContext.copy(),
                        followSymlink](InodePtr inodePtr) {
              return inodePtr->ensureMaterialized(fetchContext, followSymlink);
            }));
  }

  return collectAll(std::move(childFutures)).unit();
}
#endif

#ifndef _WIN32
size_t TreeInode::unloadChildrenLastAccessedBefore(
    const timespec& cutoff,
    const folly::CancellationToken& cancellationToken) {
  auto shouldCancel = [&] {
    return cancellationToken.isCancellationRequested();
  };
  // Unloading children by criteria is a bit of an intricate operation. The
  // InodeMap and tree's contents lock must be held simultaneously when
  // checking if an inode's refcount is zero. But the child's lock cannot be
  // acquired after the InodeMap's lock is.
  //
  // Yet the child's lock must be acquired to read the atime of an inode.
  //
  // So the strategy is to acquire a set of strong InodePtrs while the
  // parent's contents lock is held. Then check atime with those strong
  // pointers, remembering which InodeNumbers we intend to unload.
  //
  // Then reacquire the parent's contents lock and the inodemap lock and
  // determine which inodes can be deleted.

  // Get the list of inodes in the directory by holding contents lock.
  // TODO: Better yet, this shouldn't use atime at all and instead keep an
  // internal system_clock::time_point in InodeBase that updates upon any
  // interesting access.
  std::vector<FileInodePtr> fileChildren;
  std::vector<TreeInodePtr> treeChildren;
  {
    auto contents = lockContentsRead();
    for (auto& entry : contents->entries.all()) {
      if (shouldCancel()) {
        break;
      }
      if (!entry.second.getInode()) {
        continue;
      }

      // This has the side effect of incrementing the reference counts of all
      // of the children. When that goes back to zero,
      // InodeMap::onInodeUnreferenced will be called on the entry.
      if (auto asFile = entry.second.asFilePtrOrNull()) {
        fileChildren.emplace_back(std::move(asFile));
      } else if (auto asTree = entry.second.asTreePtrOrNull()) {
        treeChildren.emplace_back(std::move(asTree));
      } else {
        EDEN_BUG() << "entry " << entry.first << " was neither a tree nor file";
      }
    }
  }

  // Now that the parent's lock is released, filter the inodes by age (i.e.
  // atime). Hold InodeNumbers because all we need to check is the identity of
  // the child's inode. This might need to be rethought when we support hard
  // links.
  std::unordered_set<InodeNumber> toUnload;

  // Use lastGcRefTime_ which is updated on every FUSE/NFS access.
  // This avoids acquiring the inode's state lock (which getMetadata() does)
  // and provides a more accurate signal than atime for GC decisions.
  auto shouldUnload = [&](const auto& inode) {
    return inode->getLastGcRefTime().toTimespec() < cutoff;
  };

  for (const auto& inode : fileChildren) {
    if (shouldUnload(inode)) {
      toUnload.insert(inode->getNodeId());
    }
  }
  for (const auto& inode : treeChildren) {
    if (shouldUnload(inode)) {
      toUnload.insert(inode->getNodeId());
    }
  }

  // We no longer need pointers to the child inodes - release them. Beware
  // that this may deallocate inode instances for the children and clear them
  // from InodeMap and contents table as a natural side effect of their
  // refcounts going to zero.
  //
  // unloadChildrenIf below will clear treeChildren.
  fileChildren.clear();

  return unloadChildrenIf(
      this,
      getInodeMap(),
      treeChildren,
      [&](TreeInode& child) {
        return child.unloadChildrenLastAccessedBefore(
            cutoff, cancellationToken);
      },
      [&](InodeBase* child) { return toUnload.count(child->getNodeId()) != 0; },
      [](InodeBase*, const InodeMapLock&) { return false; },
      shouldCancel);
}

InodeMetadata TreeInode::getMetadata() const {
  auto lock = getContentsUnchecked().rlock();
  return getMetadataLocked(lock->entries);
}

InodeMetadata TreeInode::getMetadataLocked(const DirContents&) const {
  return getMount()->getInodeMetadataTable()->getOrThrow(getNodeId());
}

#endif

uint64_t TreeInode::getInMemoryDescendants() {
  int64_t inMemoryDescendants =
      inMemoryDescendants_.load(std::memory_order_relaxed);
  if (inMemoryDescendants < 0) {
    // maybe make inMemoryDescendants_ 0?
    return 0;
  }
  return static_cast<uint64_t>(inMemoryDescendants);
}

void TreeInode::increaseInMemoryDescendants(int64_t inc) {
  inMemoryDescendants_.fetch_add(inc, std::memory_order_relaxed);
}

ImmediateFuture<struct stat> TreeInode::setattr(
    const DesiredMetadata& desired,
    const ObjectFetchContextPtr& /*fetchContext*/) {
  // Explicit ACL check required: the Windows path below bypasses
  // lockContentsWrite(), so this is the only guard on that platform.
  checkAccess();
#ifndef _WIN32
  struct stat result(getMount()->initStatData());
  result.st_ino = getNodeId().get();

  // Ideally, we would like to take the lock once for this function
  // call, but we cannot hold the lock while we materialize, so we
  // have to take the lock twice.
  {
    auto contents = lockContentsWrite();
    auto existing = getMetadataLocked(contents->entries);

    if (existing.shouldShortCircuitMetadataUpdate(desired)) {
      existing.applyToStat(result);
      XLOG(DBG7, "Skipping materialization because setattr is a noop");
      return result;
    }
  }
  // The attributes actually changed so we need to mark this directory as
  // modified.
  materialize();

  // We do not have size field for directories and currently TreeInode does
  // not have any field like FileInode::state_::mode to set the mode. May be
  // in the future if needed we can add a mode Field to TreeInode::contents_
  // but for now we are simply setting the mode to (S_IFDIR | 0755).

  // Set timeStamps, mode in the result.
  auto contents = lockContentsWrite();
  auto metadata = getMount()->getInodeMetadataTable()->modifyOrThrow(
      getNodeId(),
      [&](auto& metadata) { metadata.updateFromDesired(getClock(), desired); });
  metadata.applyToStat(result);

  // Update Journal
  updateJournal();
  return result;
#else
  (void)desired;
  // Inode metadata table is not on Windows
  return makeImmediateFutureWith([]() -> struct stat { NOT_IMPLEMENTED(); });
#endif
}

#ifndef _WIN32
ImmediateFuture<std::vector<std::string>> TreeInode::listxattr() {
  // TODO: Re-evaluate if we should return a valid list of attributes now that
  // appledouble files can be turned off via an EdenFS config option.
  return std::vector<std::string>{};
}
ImmediateFuture<std::string> TreeInode::getxattr(
    folly::StringPiece name,
    const ObjectFetchContextPtr& context) {
  if (name == kXattrDigestHash) {
    return ImmediateFuture<std::string>{
        // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
        folly::coro::co_invoke(
            [](TreeInodePtr self, ObjectFetchContextPtr context)
                -> folly::coro::Task<std::string> {
              auto hash = co_await self->co_getDigestHash(context);
              if (!hash.has_value()) {
                throw InodeError(kENOATTR, self);
              }
              co_return hash->toString();
            },
            inodePtrFromThis(),
            context.copy())
            .semi()};
  }
  return makeImmediateFuture<std::string>(
      InodeError(kENOATTR, inodePtrFromThis()));
}
#endif

} // namespace facebook::eden
