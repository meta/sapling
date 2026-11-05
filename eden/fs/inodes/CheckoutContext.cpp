/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/inodes/CheckoutContext.h"

#include <folly/logging/xlog.h>
#include <optional>

#include "eden/fs/config/CheckoutConfig.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/Overlay.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/store/ObjectStore.h"

using std::vector;

namespace facebook::eden {

CheckoutContext::CheckoutContext(
    EdenMount* mount,
    CheckoutMode checkoutMode,
    OptionalProcessId clientPid,
    folly::StringPiece thriftMethodName,
    std::shared_ptr<std::atomic<uint64_t>> checkoutProgress,
    const std::unordered_map<std::string, std::string>* requestInfo,
    folly::CancellationToken cancellationToken)
    : checkoutMode_{checkoutMode},
      mount_{mount},
      fetchContext_{makeRefPtr<StatsFetchContext>(
          clientPid,
          ObjectFetchContext::Cause::Thrift,
          // The caller may be backed by request or coroutine-local storage,
          // while this context remains alive for the asynchronous checkout.
          ObjectFetchContext::CauseDetail::fromOwnedString(
              thriftMethodName.str()),
          requestInfo)},
      checkoutProgress_{std::move(checkoutProgress)} {
  if (mount_->getEdenConfig()->thriftCheckoutTimeTracing.getValue()) {
    fetchContext_->setTimeTracer(std::make_shared<MiniTracer>());
  }
  fetchContext_->setCancellationToken(std::move(cancellationToken));
  mount_->getOverlay()->enterCheckoutDeferral();
}

CheckoutContext::~CheckoutContext() {
  mount_->getOverlay()->exitCheckoutDeferral();
}

void CheckoutContext::inhibitInodeGC() {
  XCHECK(!inodeGCLease_);
  inodeGCLease_.emplace(mount_->stealInodeGCLease());
}

void CheckoutContext::start(
    RenameLock&& renameLock,
    EdenMount::ParentLock::LockedPtr&& parentLock,
    const RootId& oldSnapshot,
    RootId newSnapshot,
    std::shared_ptr<const Tree> toTree) {
  renameLock_ = std::move(renameLock);

  // Only update the parent if it is not a dry run.
  if (!isDryRun()) {
    std::optional<RootId> oldParent;
    if (parentLock) {
      XCHECK(
          std::holds_alternative<ParentCommitState::CheckoutInProgress>(
              parentLock->checkoutState));
      oldParent = oldSnapshot;
      // Update the in-memory snapshot ID
      parentLock->checkedOutRootId = newSnapshot;
      parentLock->workingCopyParentRootId = newSnapshot;
      parentLock->checkedOutRootTree = std::move(toTree);
    }

    auto config = mount_->getCheckoutConfig();

    // Save the new snapshot hash to the config
    if (!oldParent.has_value()) {
      config->setCheckedOutCommit(newSnapshot);
    } else {
      config->setCheckoutInProgress(oldParent.value(), newSnapshot);
    }
    // P1373448241
    static const std::string_view kEmptyOldParent = "<none>";
    XLOGF(
        DBG1,
        "updated snapshot for {} from {} to {}",
        config->getMountPath(),
        oldParent.has_value()
            ? mount_->getObjectStore()->displayRootId(*oldParent)
            : kEmptyOldParent,
        mount_->getObjectStore()->displayRootId(newSnapshot));
  }
}

ImmediateFuture<vector<CheckoutConflict>> CheckoutContext::finish(
    const RootId& newSnapshot) {
  finalizeBeforeFlush(newSnapshot);
  return flush();
}

ImmediateFuture<vector<CheckoutConflict>> CheckoutContext::flush() {
  if (!isDryRun()) {
    // If we have a FUSE channel, flush all invalidations we sent to the kernel
    // as part of the checkout operation.  This will ensure that other processes
    // will see up-to-date data once we return.
    //
    // We do this after releasing the rename lock since some of the invalidation
    // operations may be blocked waiting on FUSE unlink() and rename()
    // operations complete.
    return mount_->flushInvalidations().thenValue(
        [this](auto&&) { return std::move(*conflicts_.wlock()); });
  }

  // Return conflicts_ via a move operation.  We don't need them any more, and
  // can give ownership directly to our caller.
  return std::move(*conflicts_.wlock());
}

folly::coro::now_task<vector<CheckoutConflict>> CheckoutContext::co_finish(
    const RootId& newSnapshot) {
  finalizeBeforeFlush(newSnapshot);
  co_return co_await co_flush();
}

folly::coro::now_task<vector<CheckoutConflict>> CheckoutContext::co_flush() {
  if (!isDryRun()) {
    co_await mount_->co_flushInvalidations();
  }
  co_return std::move(*conflicts_.wlock());
}

void CheckoutContext::finalizeBeforeFlush(const RootId& newSnapshot) {
  // Only start() of a checkout that is not a dry run writes the in-progress
  // marker. A dry run must leave the marker of an earlier interrupted checkout
  // in place.
  if (!isDryRun()) {
    mount_->getCheckoutConfig()->setCheckedOutCommit(newSnapshot);
  }

  std::shared_ptr<MiniTracer> tracer = fetchContext_->getTimeTracer();
  if (tracer && tracer->elapsed() > std::chrono::seconds(1)) {
    XLOGF(INFO, "Checkout time summary:\n{}", tracer->summarize());
  }

  // Release the rename lock.
  // This allows any filesystem unlink() or rename() operations to proceed.
  renameLock_.unlock();
}

void CheckoutContext::addConflict(
    ConflictType type,
    RelativePathPiece path,
    dtype_t dtype) {
  // Errors should be added using addError()
  XCHECK(
      type != ConflictType::ERROR,
      fmt::format("attempted to add error using addConflict(): {}", path));

  CheckoutConflict conflict;
  conflict.path() = std::string{path.value()};
  conflict.type() = type;
  conflict.dtype() = static_cast<Dtype>(dtype);
  conflicts_.wlock()->push_back(std::move(conflict));
}

void CheckoutContext::addConflict(
    ConflictType type,
    TreeInode* parent,
    PathComponentPiece name,
    dtype_t dtype) {
  // During checkout, updated files and directories are first unlinked before
  // being removed and/or replaced in the DirContents of their parent
  // TreeInode. In between these two, calling addConflict would lead to an
  // unlinked path, thus getPath cannot be used.
  //
  // During checkout, the RenameLock is held without being released, preventing
  // files from being renamed or removed.
  auto parentPath = parent->getUnsafePath();

  addConflict(type, parentPath + name, dtype);
}

void CheckoutContext::addConflict(ConflictType type, InodeBase* inode) {
  // See above for why getUnsafePath must be used.
  auto path = inode->getUnsafePath();
  addConflict(type, path, inode->getType());
}

void CheckoutContext::addError(
    TreeInode* parent,
    PathComponentPiece name,
    const folly::exception_wrapper& ew) {
  // See above for why getUnsafePath must be used.
  auto parentPath = parent->getUnsafePath();

  auto path = parentPath + name;
  CheckoutConflict conflict;
  conflict.path() = path.value();
  conflict.type() = ConflictType::ERROR;
  conflict.message() = folly::exceptionStr(ew).toStdString();
  conflicts_.wlock()->push_back(std::move(conflict));
}

void CheckoutContext::increaseCheckoutCounter(int64_t inc) const {
  if (checkoutProgress_) {
    checkoutProgress_->fetch_add(inc, std::memory_order_relaxed);
  }
}
} // namespace facebook::eden
