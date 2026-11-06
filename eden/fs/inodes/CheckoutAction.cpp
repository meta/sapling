/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/inodes/CheckoutAction.h"

#include <folly/coro/Collect.h>
#include <folly/coro/Invoke.h>
#include <folly/coro/Task.h>
#include <folly/logging/xlog.h>
#include <cerrno>

#include "eden/fs/inodes/CheckoutContext.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/FileInode.h"
#include "eden/fs/inodes/InodeBase.h"
#include "eden/fs/inodes/InodeError.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/journal/Journal.h"
#include "eden/fs/model/Tree.h"
#include "eden/fs/model/TreeEntry.h"
#include "eden/fs/service/gen-cpp2/eden_types.h"
#include "eden/fs/store/ObjectStore.h"
#include "eden/fs/telemetry/EdenStats.h"

using folly::exception_wrapper;

namespace facebook::eden {

namespace {
/**
 * The commit transition journaled at the end of a checkout only covers tracked
 * paths, so removing an untracked file has to be recorded on its own.
 * replaceFileEntry() requests invalidation only after unlinking the entry.
 */
void journalRemovedFile(
    EdenMount* mount,
    const std::optional<RelativePath>& path,
    dtype_t dtype,
    const CheckoutActionResult& result) {
  if (path && result.invalidationRequired == InvalidationRequired::Yes) {
    mount->getJournal().recordRemoved(*path, dtype);
  }
}
} // namespace

CheckoutAction::CheckoutAction(
    CheckoutContext* ctx,
    const Tree::value_type* oldScmEntry,
    const Tree::value_type* newScmEntry,
    InodePtr&& inode)
    : ctx_(ctx), inode_(std::move(inode)) {
  XDCHECK(oldScmEntry || newScmEntry);
  if (oldScmEntry) {
    oldScmEntry_ = *oldScmEntry;
  }
  if (newScmEntry) {
    newScmEntry_ = *newScmEntry;
  }
}

CheckoutAction::CheckoutAction(
    CheckoutContext* ctx,
    PathComponentPiece localEntryName,
    InodePtr&& inode)
    : ctx_(ctx),
      inode_(std::move(inode)),
      localEntryName_(PathComponent{localEntryName}) {}

CheckoutAction::CheckoutAction(
    InternalConstructor,
    CheckoutContext* ctx,
    const Tree::value_type* oldScmEntry,
    const Tree::value_type* newScmEntry,
    ImmediateFuture<InodePtr> inodeFuture)
    : ctx_(ctx), inodeFuture_(std::move(inodeFuture)) {
  XDCHECK(oldScmEntry || newScmEntry);
  if (oldScmEntry) {
    oldScmEntry_ = *oldScmEntry;
  }
  if (newScmEntry) {
    newScmEntry_ = *newScmEntry;
  }
}

CheckoutAction::CheckoutAction(
    InternalConstructor,
    CheckoutContext* ctx,
    PathComponentPiece localEntryName,
    ImmediateFuture<InodePtr> inodeFuture)
    : ctx_(ctx),
      inodeFuture_(std::move(inodeFuture)),
      localEntryName_(PathComponent{localEntryName}) {}

CheckoutAction::~CheckoutAction() = default;

PathComponentPiece CheckoutAction::getEntryName() const {
  if (oldScmEntry_.has_value()) {
    return oldScmEntry_.value().first;
  }
  if (newScmEntry_.has_value()) {
    return newScmEntry_.value().first;
  }
  XDCHECK(localEntryName_.has_value());
  return localEntryName_.value();
}

folly::coro::now_task<CheckoutActionResult> CheckoutAction::co_run(
    CheckoutContext* ctx,
    ObjectStore* store) {
  ctx->throwIfCanceled();

  std::vector<folly::coro::Task<folly::Unit>> loadTasks;
  try {
    auto self = shared_from_this();

    if (oldScmEntry_.has_value()) {
      const auto& oldEntry = oldScmEntry_.value();
      if (oldEntry.second.isTree()) {
        auto getTreeSpan = ctx->createSpan("getTree");
        loadTasks.push_back(
            folly::coro::co_invoke(
                [self,
                 store,
                 id = oldEntry.second.getObjectId(),
                 ctx,
                 span = std::move(
                     getTreeSpan)]() mutable -> folly::coro::Task<folly::Unit> {
                  co_await folly::coro::co_reschedule_on_current_executor;
                  try {
                    auto tree =
                        co_await store->co_getTree(id, ctx->getFetchContext());
                    self->setOldTree(std::move(tree));
                  } catch (const std::exception&) {
                    self->error(
                        "error getting old tree",
                        folly::exception_wrapper{std::current_exception()});
                  }
                  co_return folly::unit;
                }));
      } else {
        loadTasks.push_back(
            folly::coro::co_invoke(
                [self, store, id = oldEntry.second.getObjectId(), ctx]()
                    -> folly::coro::Task<folly::Unit> {
                  co_await folly::coro::co_reschedule_on_current_executor;
                  try {
                    auto sha1 = co_await store->co_getBlobSha1(
                        id, ctx->getFetchContext());
                    self->setOldBlob(std::move(sha1));
                  } catch (const std::exception&) {
                    self->error(
                        "error getting old blob Sha1",
                        folly::exception_wrapper{std::current_exception()});
                  }
                  co_return folly::unit;
                }));
      }
    }

    if (newScmEntry_.has_value()) {
      const auto& newEntry = newScmEntry_.value();
      if (newEntry.second.isTree()) {
        if (!newEntry.second.isRestricted()) {
          auto getTreeSpan = ctx->createSpan("getTree");
          loadTasks.push_back(
              folly::coro::co_invoke(
                  [self,
                   store,
                   id = newEntry.second.getObjectId(),
                   ctx,
                   span = std::move(getTreeSpan)]() mutable
                      -> folly::coro::Task<folly::Unit> {
                    co_await folly::coro::co_reschedule_on_current_executor;
                    try {
                      auto tree = co_await store->co_getTree(
                          id, ctx->getFetchContext());
                      self->setNewTree(std::move(tree));
                    } catch (const std::exception&) {
                      self->error(
                          "error getting new tree",
                          folly::exception_wrapper{std::current_exception()});
                    }
                    co_return folly::unit;
                  }));
        }
      } else {
        // We don't actually compare the new blob to anything, so we don't
        // need to fetch it. This just marks that the new inode will be a
        // file.
        setNewBlob();
      }
    }

    if (!inode_) {
      XCHECK(inodeFuture_.valid());
      loadTasks.push_back(
          folly::coro::co_invoke(
              [self, future = std::move(inodeFuture_)]() mutable
                  -> folly::coro::Task<folly::Unit> {
                co_await folly::coro::co_reschedule_on_current_executor;
                try {
                  auto inode = co_await std::move(future).semi();
                  self->setInode(std::move(inode));
                } catch (const std::exception&) {
                  self->error(
                      "error getting inode",
                      folly::exception_wrapper{std::current_exception()});
                }
                co_return folly::unit;
              }));
    }
  } catch (const std::exception&) {
    error(
        "error preparing to load data for checkout action",
        folly::exception_wrapper{std::current_exception()});
  }

  auto loadResults =
      co_await folly::coro::collectAllTryRange(std::move(loadTasks));
  for (auto& tryResult : loadResults) {
    if (tryResult.hasException()) {
      error(
          "error loading data for checkout action",
          std::move(tryResult.exception()));
    }
  }

  if (auto errors = errors_.copy(); !errors.empty()) {
    XLOG(
        ERR,
        "multiple errors while attempting to load data for checkout action:");
    for (const auto& ew : errors) {
      XLOGF(ERR, "CheckoutAction error: {}", folly::exceptionStr(ew));
    }
    errors.front().throw_exception();
  }

  handleOldTreeRestriction();
  co_return co_await co_doAction();
}

void CheckoutAction::setOldTree(std::shared_ptr<const Tree> tree) {
  XCHECK(!oldTree_);
  XCHECK(!oldBlobSha1_);
  oldTree_ = std::move(tree);
}

void CheckoutAction::setOldBlob(Hash20 blobSha1) {
  XCHECK(!oldTree_);
  XCHECK(!oldBlobSha1_);
  oldBlobSha1_ = std::move(blobSha1);
}

void CheckoutAction::setNewTree(std::shared_ptr<const Tree> tree) {
  XCHECK(!newTree_);
  XCHECK(!newBlobMarker_);
  newTree_ = std::move(tree);
}

void CheckoutAction::setNewBlob() {
  XCHECK(!newTree_);
  XCHECK(!newBlobMarker_);
  newBlobMarker_ = true;
}

void CheckoutAction::setInode(InodePtr inode) {
  XCHECK(!inode_);
  inode_ = std::move(inode);
}

void CheckoutAction::error(
    folly::StringPiece msg,
    folly::exception_wrapper&& ew) {
  XLOGF(ERR, "error performing checkout action: {}: {}", msg, ew);
  errors_.wlock()->push_back(std::move(ew));
}

void CheckoutAction::handleOldTreeRestriction() {
  auto treeInode = inode_.asTreePtrOrNull();
  const bool oldTreeBecameRestricted = oldScmEntry_ &&
      oldScmEntry_->second.isTree() && oldTree_ && oldTree_->isRestricted() &&
      treeInode && !treeInode->isRestricted();
  // The destination-restricted condition is load bearing, not a narrowing
  // heuristic. A restricted tree fetches as empty, so this is the exact
  // transition that reaches the restricted-destination local-only handling:
  // the now-restricted old tree is taken as an authoritative empty source, and
  // NORMAL removes the live entries before installing the placeholder, wiping
  // locally modified files. A non-restricted destination, or a removal of the
  // entry outright, instead runs through the existing conflict detection and
  // non-empty-directory safeguards, which already refuse to discard that data.
  const bool destinationIsRestricted =
      (newScmEntry_ && newScmEntry_->second.isTree() &&
       newScmEntry_->second.isRestricted()) ||
      (newTree_ && newTree_->isRestricted());
  if (!oldTreeBecameRestricted || !destinationIsRestricted) {
    return;
  }

  if (ctx_->forceUpdate()) {
    oldTree_.reset();
    oldScmEntry_.reset();
    return;
  }

  throw InodeError(
      EACCES, inode_, "old source tree became restricted during checkout");
}

folly::coro::now_task<CheckoutActionResult> CheckoutAction::co_doAction() {
  bool conflictWasAddedToCtx = co_await co_hasConflict();

  // Note that even if we know we are not going to apply the changes, we
  // must still run hasConflict() first because we rely on its side-effects.
  if (conflictWasAddedToCtx && !ctx_->forceUpdate()) {
    // Since we aren't doing another checkoutUpdateEntry, the checkout
    // from this inode won't be executed if it is a tree. In that case, we
    // add that to our "completed" checkout for all of its descendants
    auto treeInode = inode_.asTreeOrNull();
    auto increase = treeInode ? treeInode->getInMemoryDescendants() : 0;
    ctx_->increaseCheckoutCounter(1 + increase);
    co_return CheckoutActionResult{
        InvalidationRequired::No, /*hadConflicts=*/true};
  }

  if (!oldScmEntry_ && !newScmEntry_) {
    auto treeInode = inode_.asTreePtrOrNull();
    if (!ctx_->isDryRun() && (!treeInode || treeInode->isRestricted())) {
      auto parent = inode_->getParent(ctx_->renameLock());
      std::optional<RelativePath> removedPath;
      if (!treeInode) {
        removedPath = inode_->getPath();
      }
      auto dtype = inode_->getType();
      auto* mount = inode_->getMount();
      auto result = co_await parent->co_checkoutUpdateEntry(
          ctx_,
          getEntryName(),
          std::move(inode_),
          nullptr,
          nullptr,
          std::nullopt,
          /*removeLocalOnly=*/false);
      journalRemovedFile(mount, removedPath, dtype, result);
      result.hadConflicts |= conflictWasAddedToCtx;
      co_return result;
    }
    if (!treeInode) {
      co_return CheckoutActionResult{
          InvalidationRequired::No, conflictWasAddedToCtx};
    }
    auto result = co_await treeInode->co_checkout(
        ctx_, nullptr, nullptr, /*reportLocalOnlyAsConflicts=*/true);
    bool hadConflicts = result.hadConflicts || conflictWasAddedToCtx;
    if (!ctx_->isDryRun()) {
      auto parent = inode_->getParent(ctx_->renameLock());
      auto actionResult = co_await parent->co_checkoutUpdateEntry(
          ctx_,
          getEntryName(),
          std::move(inode_),
          nullptr,
          nullptr,
          std::nullopt,
          /*removeLocalOnly=*/false);
      actionResult.hadConflicts |= hadConflicts;
      co_return actionResult;
    }
    co_return CheckoutActionResult{InvalidationRequired::No, hadConflicts};
  }

  // Call TreeInode::checkoutUpdateEntry() to actually do the work.
  //
  // Note that we are moving most of our state into the
  // checkoutUpdateEntry() arguments.  We have to be slightly careful
  // here: getEntryName() returns a PathComponentPiece that is pointing
  // into a PathComponent owned either by oldScmEntry_ or newScmEntry_.
  // Therefore don't move these scm entries, to make sure we don't
  // invalidate the PathComponentPiece data.
  auto parent = inode_->getParent(ctx_->renameLock());
  auto result = co_await parent->co_checkoutUpdateEntry(
      ctx_,
      getEntryName(),
      std::move(inode_),
      std::move(oldTree_),
      std::move(newTree_),
      newScmEntry_,
      removeLocalOnly_);
  result.hadConflicts |= conflictWasAddedToCtx;
  co_return result;
}

std::optional<bool> CheckoutAction::checkSyncConflict() {
  if (oldTree_) {
    auto treeInode = inode_.asTreePtrOrNull();
    if (!treeInode) {
      // This was a directory, but has been replaced with a file on disk
      ctx_->addConflict(
          newScmEntry_ ? ConflictType::MODIFIED_MODIFIED
                       : ConflictType::MODIFIED_REMOVED,
          inode_.get());
      return true;
    }

    // TODO: check for permissions changes

    // We don't check if this tree is unmodified from the old tree or not here.
    // We simply apply the checkout to the tree in this case, so that we report
    // conflicts for individual leaf inodes that were modified, and not for the
    // parent directories.
    return false;
  } else if (oldBlobSha1_) {
    auto fileInode = inode_.asFilePtrOrNull();
    if (!fileInode) {
      // The local directory's children must be compared with the destination.
      return false;
    }

    // Caller must perform the async FileInode::isSameAs check.
    return std::nullopt;
  }

  XDCHECK(!oldScmEntry_) << "Both oldTree_ and oldBlob_ are nullptr, "
                            "so this file should not have an oldScmEntry_.";

  if (!newScmEntry_) {
    if (inode_.asTreePtrOrNull()) {
      return false;
    }
    if (!ctx_->isDryRun()) {
      return false;
    }
    ctx_->addConflict(ConflictType::VISIBLE_RESTRICTED, inode_.get());
    return true;
  }

  auto localIsFile = inode_.asFilePtrOrNull() != nullptr;
  if (localIsFile) {
    // This entry is a file that did not exist in the old source control tree,
    // but it exists as a tracked file or directory in the new tree.
    ctx_->addConflict(ConflictType::UNTRACKED_ADDED, inode_.get());
    return true;
  } else {
    // This entry is a directory that did not exist in the old source control
    // tree. We must traverse the directory for UNTRACKED_ADDED and
    // MODIFIED_MODIFIED conflicts. Returning false signals that we must
    // recurse into this directory to continue to look for conflicts.
    return false;
  }
}

bool CheckoutAction::classifyFileContentConflict(bool isSame) {
  if (isSame) {
    // no conflict
    return false;
  }

  // The file contents or mode bits are different:
  // - If the file exists in the new tree but differs from what is
  //   currently in the working copy, then this is a MODIFIED_MODIFIED
  //   conflict.
  // - If the file does not exist in the new tree, then this is a
  //   MODIFIED_REMOVED conflict.
  auto conflictType = newScmEntry_ ? ConflictType::MODIFIED_MODIFIED
                                   : ConflictType::MODIFIED_REMOVED;
  ctx_->addConflict(conflictType, inode_.get());
  return true;
}

bool CheckoutAction::classifyFileDestinationConflict(bool isSameAsDestination) {
  if (isSameAsDestination) {
    if (!ctx_->isDryRun()) {
      inode_->getMount()->getStats()->increment(
          &CheckoutStats::avoidedDestinationConflicts);
    }
    return false;
  }
  return classifyFileContentConflict(false);
}

folly::coro::now_task<bool> CheckoutAction::co_hasConflict() {
  if (auto syncResult = checkSyncConflict()) {
    co_return *syncResult;
  }

  auto fileInode = inode_.asFilePtrOrNull();
  bool isSameAsSource = co_await fileInode->co_isSameAs(
      oldScmEntry_.value().second.getObjectId(),
      oldBlobSha1_.value(),
      oldScmEntry_.value().second.getType(),
      ctx_->getFetchContext());
  if (isSameAsSource || !newBlobMarker_) {
    co_return classifyFileContentConflict(isSameAsSource);
  }

  const auto& destination = newScmEntry_.value().second;
  bool isSameAsDestination = co_await fileInode->co_isSameAs(
      destination.getObjectId(),
      destination.getType(),
      ctx_->getFetchContext());
  co_return classifyFileDestinationConflict(isSameAsDestination);
}
} // namespace facebook::eden
