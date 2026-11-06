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
#include <memory>
#include <optional>
#include <vector>
#include "eden/common/utils/ImmediateFuture.h"
#include "eden/fs/fuse/Invalidation.h"
#include "eden/fs/inodes/InodePtr.h"
#include "eden/fs/model/Tree.h"

namespace facebook::eden {

class Blob;
class CheckoutContext;
class ObjectStore;

struct CheckoutSubtreeResult {
  bool hadConflicts{false};
  /**
   * A dry run found local-only entries that would survive checkout to an
   * empty tree, so the directory could not be replaced with a file.
   */
  bool localOnlyRemains{false};
};

struct CheckoutActionResult {
  InvalidationRequired invalidationRequired{InvalidationRequired::No};
  bool hadConflicts{false};
  bool localOnlyRemains{false};
};

/**
 * A helper class representing an action that must be taken as part of a
 * checkout operation.
 *
 * The TreeInode is responsible for computing the list of CheckoutActions that
 * must be run in order to perform a checkout.  These actions are computed
 * while holding the TreeInode's contents_ lock, and then executed after
 * releasing the lock.
 *
 * A few actions can be done immediately while still holding the TreeInode's
 * contents lock.  In particular, this includes creating new entries for files
 * or directories that did not previously exist.  TreeInode is responsible for
 * performing these actions while still holding the contents_ lock.  No
 * CheckoutAction objects are ever created for these cases, since these actions
 * can be taken immediately.
 */
class CheckoutAction : public std::enable_shared_from_this<CheckoutAction> {
 public:
  /**
   * Create a CheckoutAction with an already loaded Inode object.
   */
  CheckoutAction(
      CheckoutContext* ctx,
      const Tree::value_type* oldScmEntry,
      const Tree::value_type* newScmEntry,
      InodePtr&& inode);

  /**
   * Create a CheckoutAction for an entry that exists only in the local
   * filesystem. Used for restriction conflicts and forced removal of
   * directories that must be replaced with files.
   */
  CheckoutAction(
      CheckoutContext* ctx,
      PathComponentPiece localEntryName,
      InodePtr&& inode);

  /**
   * Create a CheckoutAction where the Inode object in question is not loaded
   * yet.
   *
   * (This is a template function purely to avoid ambiguity with the
   * constructor type above.  ImmediateFuture<InodePtr> is implicitly
   * constructible from an InodePtr, but we want to prefer the constructor
   * above if we have an InodePtr.)
   */
  template <typename InodePtrType>
  CheckoutAction(
      CheckoutContext* ctx,
      const Tree::value_type* oldScmEntry,
      const Tree::value_type* newScmEntry,
      ImmediateFuture<InodePtrType> inodeFuture)
      : CheckoutAction(
            INTERNAL,
            ctx,
            oldScmEntry,
            newScmEntry,
            std::move(inodeFuture)) {}

  template <typename InodePtrType>
  CheckoutAction(
      CheckoutContext* ctx,
      PathComponentPiece localEntryName,
      ImmediateFuture<InodePtrType> inodeFuture)
      : CheckoutAction(INTERNAL, ctx, localEntryName, std::move(inodeFuture)) {}

  /*
   * CheckoutAction does not allow copying or moving.
   *
   * We hold a pointer to ourself while waiting on the data to load, so we
   * cannot allow the object to potentially move to another address.
   */
  CheckoutAction(CheckoutAction&& other) = delete;
  CheckoutAction& operator=(CheckoutAction&& other) = delete;

  ~CheckoutAction();

  PathComponentPiece getEntryName() const;

  /**
   * Remove local-only contents when this action removes a directory. Set by
   * a parent checkout that is emptying itself for a forced replacement with a
   * file, so that tracked subdirectories do the same.
   */
  void setRemoveLocalOnly() {
    removeLocalOnly_ = true;
  }

  /**
   * Run the CheckoutAction.
   *
   * If this completes successfully, the result indicates if the change
   * updated the parent directory's entries. Returns whether the caller is
   * responsible for invalidating the directory's inode cache in the kernel.
   */
  [[nodiscard]] folly::coro::now_task<CheckoutActionResult> co_run(
      CheckoutContext* ctx,
      ObjectStore* store);

 private:
  enum InternalConstructor {
    INTERNAL,
  };
  CheckoutAction(
      InternalConstructor,
      CheckoutContext* ctx,
      const Tree::value_type* oldScmEntry,
      const Tree::value_type* newScmEntry,
      ImmediateFuture<InodePtr> inodeFuture);
  CheckoutAction(
      InternalConstructor,
      CheckoutContext* ctx,
      PathComponentPiece localEntryName,
      ImmediateFuture<InodePtr> inodeFuture);

  void setOldTree(std::shared_ptr<const Tree> tree);
  void setOldBlob(Hash20 blobSha1);
  void setNewTree(std::shared_ptr<const Tree> tree);
  void setNewBlob();
  void setInode(InodePtr inode);
  void error(folly::StringPiece msg, folly::exception_wrapper&& ew);

  /**
   * Reject a newly restricted old SCM tree before it can be mistaken for an
   * empty source directory, throwing InodeError(EACCES). FORCE checkout
   * instead mutates this action to treat the old side as opaque, dropping
   * oldTree_ and oldScmEntry_ before continuing with the restricted
   * destination.
   */
  void handleOldTreeRestriction();

  folly::coro::now_task<bool> co_hasConflict();

  /**
   * Synchronous portion of `co_hasConflict()`. Handles the tree-side check,
   * the blob nullptr check, and the no-old-entry tail. Returns the final
   * conflict result when no async work is required, or `std::nullopt` when
   * the caller must perform the async `FileInode::isSameAs` check and then
   * pass the result to `classifyFileContentConflict()`.
   */
  std::optional<bool> checkSyncConflict();

  /**
   * Classify the conflict (if any) given the result of
   * `FileInode::isSameAs`. Returns true iff a conflict was added to the
   * context.
   */
  bool classifyFileContentConflict(bool isSame);

  /**
   * Classify the result of comparing a file with the checkout destination.
   */
  bool classifyFileDestinationConflict(bool isSameAsDestination);

  /**
   * Return whether the directory's contents have changed and the
   * inode's readdir cache must be flushed.
   */
  [[nodiscard]] folly::coro::now_task<CheckoutActionResult> co_doAction();

  /**
   * The context for the in-progress checkout operation.
   */
  CheckoutContext* const ctx_{nullptr};

  /**
   * The TreeEntry in the old Tree that we are moving away from.
   *
   * This will be none if the entry did not exist in the old Tree.
   */
  std::optional<Tree::value_type> oldScmEntry_;

  /**
   * The TreeEntry in the new Tree that we are checking out.
   *
   * This will be none if the entry is deleted in the new Tree.
   */
  std::optional<Tree::value_type> newScmEntry_;

  /**
   * A Future that will be invoked when the inode is loaded.
   *
   * This may be unset if the inode was already available when the
   * CheckoutAction was created (in which case inode_ will be non-null).
   */
  ImmediateFuture<InodePtr> inodeFuture_ =
      ImmediateFuture<InodePtr>::makeEmpty();

  /**
   * A reference count tracking number of outstanding futures still
   * running as part of the process to load all of our data.
   *
   * When all futures complete (successfully or not) this will drop to zero,
   * at which point allDataReady() will be invoked to complete the action.
   */
  std::atomic<uint32_t> numLoadsPending_{0};

  /*
   * Data that we have to load to perform the checkout action.
   *
   * Only one each oldTree_ and oldBlob_ will be loaded,
   * and the same goes for newTree_ and newBlob_.
   *
   * Note that for trees we download the full tree. For the old blob we
   * only download the sha1 as this is all we will need. We don't actually ever
   * need the data from new blob.  So we just record if the destination is
   * a new blob, and not bother loading the blob data itself.
   */
  InodePtr inode_;
  /**
   * The local-only entry name when this action has no old or new SCM entry.
   */
  std::optional<PathComponent> localEntryName_;
  std::shared_ptr<const Tree> oldTree_;
  std::optional<Hash20> oldBlobSha1_;
  std::shared_ptr<const Tree> newTree_;
  bool newBlobMarker_ = false;
  bool removeLocalOnly_ = false;

  /**
   * The errors that occurred while loading the data needed to perform the
   * checkout action. The load continuations complete on different executor
   * threads, so concurrent failures append under the lock. The lock is only
   * held for the push and for copying the vector out, never while formatting
   * an error or calling into an inode, so it nests under every other lock.
   */
  folly::Synchronized<std::vector<folly::exception_wrapper>> errors_;
};
} // namespace facebook::eden
