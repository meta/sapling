/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <sys/stat.h>

#include <optional>

#include <folly/coro/safe/NowTask.h>

#include "eden/common/utils/ImmediateFuture.h"
#include "eden/common/utils/RefPtr.h"
#include "eden/fs/inodes/InodePtr.h"
#include "eden/fs/inodes/UnmaterializedUnloadedBlobDirEntry.h"
#include "eden/fs/model/BlobAuxData.h"
#include "eden/fs/model/EntryAttributeFlags.h"
#include "eden/fs/model/Hash.h"
#include "eden/fs/model/TreeAuxDataFwd.h"
#include "eden/fs/model/TreeEntry.h"
#include "eden/fs/model/TreeFwd.h"

namespace facebook::eden {

class ObjectStore;
class ObjectFetchContext;
using ObjectFetchContextPtr = RefPtr<ObjectFetchContext>;

namespace detail {
using VariantVirtualInode = std::
    variant<InodePtr, UnmaterializedUnloadedBlobDirEntry, TreePtr, TreeEntry>;
} // namespace detail

/**
 * VirtualInode allows read-only queries over a mount independent of the state
 * it's in. If a mount has loaded inodes, they will be queried. Otherwise,
 * source control objects will be fetched from the BackingStore, avoiding
 * needing to query the overlay and track loaded inodes.
 *
 * Note that "virtual" in VirtualInode refers to the fact that these objects are
 * inode-like, but may not reference an inode under the hood. They are unrelated
 * to the BSD vnode concept.
 */
class VirtualInode {
 public:
  explicit VirtualInode(InodePtr value);

  explicit VirtualInode(UnmaterializedUnloadedBlobDirEntry value);

  explicit VirtualInode(TreePtr value, mode_t mode);

  explicit VirtualInode(TreeEntry value);

  /**
   * Create a VirtualInode wrapping a restricted (ACL-denied) empty tree.
   */
  static VirtualInode makeRestricted(
      const TreeEntry& entry,
      CaseSensitivity caseSensitivity);

  /**
   * Overload for callers that have a DirEntry (ObjectId + mode) rather
   * than a model TreeEntry.
   */
  static VirtualInode makeRestricted(
      const ObjectId& id,
      mode_t mode,
      CaseSensitivity caseSensitivity);

  /**
   * Returns the contained InodePtr.
   *
   * If there is not one, throws a std::exception.
   */
  InodePtr asInodePtr() const;

  dtype_t getDtype() const;

  bool isDirectory() const;

  std::optional<ObjectId> getObjectId() const;

  bool isMaterialized() const;

  bool isRestricted() const;

  /**
   * Discover the contained data type.
   *
   * These functions should not be used outside of unit tests.
   * VirtualInode should "transparently" look like a file or directory to
   * most users.
   */
  enum class ContainedType {
    Inode,
    DirEntry, // aka UnmaterializedUnloadedBlobDirEntry
    Tree,
    TreeEntry,
  };
  ContainedType testGetContainedType() const;

  /**
   * Returns nullopt if the entry has a non source control file type.
   * Source Control types are currently limited to symlinks, executable files,
   * regular files and directories. So something like a FIFO or a socket would
   * fall into nullopt here.
   */
  std::optional<TreeEntryType> tryGetTreeEntryType() const;

  /**
   * Get the VirtualInode object for a child of this directory.
   *
   * Unlike TreeInode::getOrLoadChild, this method avoids loading the child's
   * inode if it is not already loaded, instead falling back to looking up the
   * object in the ObjectStore.
   */
  folly::coro::now_task<VirtualInode> co_getOrFindChild(
      PathComponentPiece childName,
      RelativePathPiece path,
      const std::shared_ptr<ObjectStore>& objectStore,
      const ObjectFetchContextPtr& fetchContext) const;

  folly::coro::now_task<Hash20> co_getSHA1(
      RelativePathPiece path,
      const std::shared_ptr<ObjectStore>& objectStore,
      const ObjectFetchContextPtr& fetchContext) const;

  folly::coro::now_task<Hash32> co_getBlake3(
      RelativePathPiece path,
      const std::shared_ptr<ObjectStore>& objectStore,
      const ObjectFetchContextPtr& fetchContext) const;

  folly::coro::now_task<std::optional<Hash32>> co_getDigestHash(
      RelativePathPiece path,
      const std::shared_ptr<ObjectStore>& objectStore,
      const ObjectFetchContextPtr& fetchContext) const;

  /**
   * Get all the requested attributes for a file entry in this tree. Available
   * attributes are currently:
   * - sha1
   * - file size
   * - source control type
   * - blake3 hash
   * - object id
   * - digest size
   * - digest hash
   * - mtime
   * - mode
   * Note: we return error values for attributes in some cases. See
   * fs/service/eden.thrift for more details on error conditions for each type.
   */
  folly::coro::now_task<EntryAttributes> co_getEntryAttributes(
      EntryAttributeFlags requestedAttributes,
      RelativePathPiece path,
      const std::shared_ptr<ObjectStore>& objectStore,
      timespec lastCheckoutTime,
      const ObjectFetchContextPtr& fetchContext) const;

  /**
   * Sync peek: build a complete EntryAttributes value from in-memory state,
   * returning nullopt if any requested attribute needs an async fetch.
   *
   * Pure peek: no logAccess side effect — an
   * intentional trade-off vs the async path to keep the cache-hot
   * children-attributes fanout cheap. The async fallback still logs.
   */
  std::optional<EntryAttributes> tryGetEntryAttributesSync(
      EntryAttributeFlags requestedAttributes,
      RelativePathPiece path,
      const std::shared_ptr<ObjectStore>& objectStore,
      timespec lastCheckoutTime,
      const ObjectFetchContextPtr& fetchContext) const;

  /**
   * Emulate stat in a way that works for source control.
   *
   * Will just run stat on the Inode if possible, otherwise returns a stat
   * structure with the st_mode and st_size data from the
   * ObjectStore/DirEntry/TreeEntry, and the st_mtim set to the passed in
   * lastCheckoutTime.
   */
  folly::coro::now_task<struct stat> co_stat(
      const struct timespec& lastCheckoutTime,
      const std::shared_ptr<ObjectStore>& objectStore,
      const ObjectFetchContextPtr& fetchContext) const;

  std::optional<struct stat> tryGetCachedStat(
      const struct timespec& lastCheckoutTime,
      const std::shared_ptr<ObjectStore>& objectStore,
      const ObjectFetchContextPtr& fetchContext,
      const std::optional<BlobAuxData>& blobAuxHint = std::nullopt) const;

  /**
   * Retrieves VirtualInode for each of the children of this
   * directory.
   */
  folly::coro::now_task<
      std::vector<std::pair<PathComponent, folly::Try<VirtualInode>>>>
  getChildren(
      RelativePathPiece path,
      const std::shared_ptr<ObjectStore>& objectStore,
      const ObjectFetchContextPtr& fetchContext);

  /**
   * Collect all requested attributes for all of the children
   * of a directory. All available attributes are currently:
   * - sha1
   * - file size
   * - source control type
   * - blake3 hash
   * - object id
   * - digest size
   * - digest hash
   * - mtime
   * - mode
   * Note: we return error values for attributes in some cases. See
   * fs/service/eden.thrift for more details on error conditions for each type.
   */
  folly::coro::now_task<
      std::vector<std::pair<PathComponent, folly::Try<EntryAttributes>>>>
  co_getChildrenAttributes(
      EntryAttributeFlags requestedAttributes,
      RelativePath path,
      const std::shared_ptr<ObjectStore>& objectStore,
      timespec lastCheckoutTime,
      const ObjectFetchContextPtr& fetchContext);

  folly::coro::now_task<std::string> co_getBlob(
      const std::shared_ptr<ObjectStore>& objectStore,
      const ObjectFetchContextPtr& fetchContext) const;

  folly::coro::now_task<std::optional<TreeAuxData>> co_getTreeAuxData(
      const std::shared_ptr<ObjectStore>& objectStore,
      const ObjectFetchContextPtr& fetchContext) const;

  std::optional<bool> isUnderAcl() const;

  std::optional<bool> hasACL() const {
    return hasACL_;
  }

  void setHasACL(std::optional<bool> hasACL);

  void inheritAclFromAncestor(std::optional<bool> ancestorUnderAcl);

  std::optional<BlobAuxData> tryGetCachedBlobAuxData(
      const std::shared_ptr<ObjectStore>& objectStore,
      const ObjectFetchContextPtr& fetchContext,
      bool blake3Required) const;

 private:
  /**
   * Helper function for getChildrenAttributes
   */
  ImmediateFuture<BlobAuxData> getBlobAuxData(
      RelativePathPiece path,
      const std::shared_ptr<ObjectStore>& objectStore,
      const ObjectFetchContextPtr& fetchContext,
      bool blake3Required = false) const;

  /**
   * Helper function for getChildrenAttributes
   */
  ImmediateFuture<std::optional<TreeAuxData>> getTreeAuxData(
      const std::shared_ptr<ObjectStore>& objectStore,
      const ObjectFetchContextPtr& fetchContext) const;

  /**
   * The main object this encapsulates
   */
  detail::VariantVirtualInode variant_;

  /**
   * The mode_t iff this contains a Tree
   *
   * The Tree's TreeEntry tells us about the mode_t of a tree, it must be saved
   * here for return in the stat() call.
   */
  mode_t treeMode_{0};
  std::optional<bool> hasACL_{std::nullopt};
  std::optional<bool> ancestorUnderAcl_{std::nullopt};
};

} // namespace facebook::eden
