/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/inodes/VirtualInode.h"

#include "eden/common/utils/Match.h"
#include "eden/common/utils/StatTimes.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/inodes/AclState.h"
#include "eden/fs/inodes/ChildEntryAttributes.h"
#include "eden/fs/inodes/DirEntry.h"
#include "eden/fs/inodes/FileInode.h"
#include "eden/fs/inodes/InodeError.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/model/Tree.h"
#include "eden/fs/model/TreeAuxData.h"
#include "eden/fs/service/gen-cpp2/eden_types.h"
#include "eden/fs/store/ObjectStore.h"
#include "eden/fs/utils/EdenError.h"

#include <folly/coro/Collect.h>
#include <folly/coro/CurrentExecutor.h>
#include <folly/coro/Invoke.h>

namespace facebook::eden {

namespace {
std::optional<bool> preferKnownAclState(
    std::optional<bool> preferred,
    std::optional<bool> fallback) {
  // Unknown metadata cannot erase a known state for the same tree.
  return preferred.has_value() ? preferred : fallback;
}

std::optional<bool> extractHasACLState(const InodePtr& inode) {
  if (inode->getType() != dtype_t::Dir) {
    return false;
  }
  auto treePtr = inode.asTreePtrOrNull();
  if (!treePtr) {
    return std::nullopt;
  }
  return treePtr->hasACL();
}

void populateUnderAclAttribute(
    EntryAttributes& attributes,
    EntryAttributeFlags requestedAttributes,
    std::optional<bool> underAcl) {
  if (!requestedAttributes.contains(ENTRY_ATTRIBUTE_UNDER_ACL)) {
    return;
  }
  if (underAcl.has_value()) {
    attributes.underAcl = folly::Try<bool>{*underAcl};
  }
}

void populateAclInfoFromLocalAclState(
    EntryAttributes& attributes,
    EntryAttributeFlags requestedAttributes,
    std::optional<bool> underAcl) {
  if (!requestedAttributes.contains(ENTRY_ATTRIBUTE_ACLs)) {
    return;
  }
  // Local inode metadata only proves the known-negative case. When underAcl
  // is true, the specific ACL entries still require a backing-store lookup.
  if (underAcl == false) {
    attributes.aclInfo = folly::Try<EntryAclInfo>{EntryAclInfo{false, {}}};
  }
}

bool shouldPopulateLocalAclAttributes(
    const std::shared_ptr<ObjectStore>& objectStore) {
  return objectStore->getEdenConfig()
      ->enableLocalUnderAclComputation.getValue();
}

void populateLocalAclAttributes(
    EntryAttributes& attributes,
    EntryAttributeFlags requestedAttributes,
    std::optional<bool> underAcl,
    bool enabled) {
  if (!enabled) {
    return;
  }
  populateUnderAclAttribute(attributes, requestedAttributes, underAcl);
  populateAclInfoFromLocalAclState(attributes, requestedAttributes, underAcl);
}
} // namespace

VirtualInode::VirtualInode(InodePtr value)
    : variant_(std::move(value)),
      hasACL_(extractHasACLState(std::get<InodePtr>(variant_))) {
  auto adjusted = adjustRootAclState(
      std::get<InodePtr>(variant_)->getNodeId() == kRootNodeId,
      ancestorUnderAcl_,
      hasACL_);
  ancestorUnderAcl_ = adjusted.ancestorUnderAcl;
  hasACL_ = adjusted.hasACL;
}

VirtualInode::VirtualInode(UnmaterializedUnloadedBlobDirEntry value)
    : variant_(std::move(value)),
      hasACL_(std::get<UnmaterializedUnloadedBlobDirEntry>(variant_).hasACL()) {
}

VirtualInode::VirtualInode(TreePtr value, mode_t mode)
    : variant_(std::move(value)),
      treeMode_(mode),
      hasACL_(std::get<TreePtr>(variant_)->hasACL()) {}

VirtualInode::VirtualInode(TreeEntry value) {
  XCHECK(!value.isTree())
      << "TreeEntries which represent a tree should be resolved to a tree "
      << "before being constructed into VirtualInode";
  hasACL_ = value.hasACL();
  variant_ = std::move(value);
}

std::optional<bool> VirtualInode::isUnderAcl() const {
  return mergeAncestorAclState(ancestorUnderAcl_, hasACL_);
}

void VirtualInode::setHasACL(std::optional<bool> hasACL) {
  hasACL_ = preferKnownAclState(hasACL, hasACL_);
}

void VirtualInode::inheritAclFromAncestor(
    std::optional<bool> ancestorUnderAcl) {
  ancestorUnderAcl_ = ancestorUnderAcl;
}

VirtualInode VirtualInode::makeRestricted(
    const TreeEntry& entry,
    CaseSensitivity caseSensitivity) {
  return makeRestricted(
      entry.getObjectId(),
      modeFromTreeEntryType(entry.getType()),
      caseSensitivity);
}

VirtualInode VirtualInode::makeRestricted(
    const ObjectId& id,
    mode_t mode,
    CaseSensitivity caseSensitivity) {
  auto restrictedTree = std::make_shared<const Tree>(
      Tree::Restricted{}, Tree::container{caseSensitivity}, id);
  return VirtualInode{std::move(restrictedTree), mode};
}

InodePtr VirtualInode::asInodePtr() const {
  return std::get<InodePtr>(variant_);
}

// Helper template for std::visit calls below
template <class>
inline constexpr bool always_false_v = false;

dtype_t VirtualInode::getDtype() const {
  return match(
      variant_,
      [](const InodePtr& inode) { return inode->getType(); },
      [](const UnmaterializedUnloadedBlobDirEntry& entry) {
        return entry.getDtype();
      },
      [](const TreePtr&) { return dtype_t::Dir; },
      [](const TreeEntry& entry) { return entry.getDtype(); });
}

bool VirtualInode::isDirectory() const {
  return getDtype() == dtype_t::Dir;
}

std::optional<ObjectId> VirtualInode::getObjectId() const {
  return match(
      variant_,
      [](const InodePtr& inode) { return inode->getObjectId(); },
      [](const TreePtr& tree) -> std::optional<ObjectId> {
        return tree->getObjectId();
      },
      [](const auto& entry) -> std::optional<ObjectId> {
        return entry.getObjectId();
      });
}

bool VirtualInode::isMaterialized() const {
  return match(
      variant_,
      [](const InodePtr& inode) { return inode->isMaterialized(); },
      [](const TreePtr&) { return false; },
      [](const UnmaterializedUnloadedBlobDirEntry&) { return false; },
      [](const TreeEntry&) { return false; });
}

bool VirtualInode::isRestricted() const {
  return match(
      variant_,
      [](const InodePtr& inode) {
        auto tree = inode.asTreePtrOrNull();
        return tree ? tree->isRestricted() : false;
      },
      [](const TreePtr& tree) { return tree->isRestricted(); },
      [](const UnmaterializedUnloadedBlobDirEntry&) { return false; },
      [](const TreeEntry& entry) { return entry.isRestricted(); });
}

VirtualInode::ContainedType VirtualInode::testGetContainedType() const {
  return match(
      variant_,
      [](const InodePtr&) { return ContainedType::Inode; },
      [](const UnmaterializedUnloadedBlobDirEntry&) {
        return ContainedType::DirEntry;
      },
      [](const TreePtr&) { return ContainedType::Tree; },
      [](const TreeEntry&) { return ContainedType::TreeEntry; });
}

folly::coro::now_task<Hash32> VirtualInode::co_getBlake3(
    RelativePathPiece path,
    const std::shared_ptr<ObjectStore>& objectStore,
    const ObjectFetchContextPtr& fetchContext) const {
  // Ensure this is a regular file.
  // We intentionally want to refuse to compute the blake3 of symlinks
  const auto dtype = getDtype();
  if (dtype == dtype_t::Dir) {
    co_yield folly::coro::co_error(PathError(EISDIR, path));
  } else if (dtype == dtype_t::Symlink) {
    co_yield folly::coro::co_error(
        PathError(EINVAL, path, std::string_view{"file is a symlink"}));
  } else if (dtype != dtype_t::Regular) {
    co_yield folly::coro::co_error(PathError(
        EINVAL, path, std::string_view{"variant is of unhandled type"}));
  }

  // This is now guaranteed to be a dtype_t::Regular file. This means there's no
  // need for a Tree case, as Trees are always directories.
  //
  // std::get_if is used instead of match because coroutine lambda captures are
  // stored in the lambda object, not the coroutine frame. If the coroutine
  // suspends, match destroys the lambda temporaries, and resuming accesses
  // dangling captures.
  static_assert(
      std::variant_size_v<detail::VariantVirtualInode> == 4,
      "New variant type added to VariantVirtualInode - update co_getBlake3");
  if (auto* inode = std::get_if<InodePtr>(&variant_)) {
    co_return co_await inode->asFilePtr()->co_getBlake3(fetchContext);
  } else if (
      auto* entry =
          std::get_if<UnmaterializedUnloadedBlobDirEntry>(&variant_)) {
    co_return co_await objectStore->co_getBlobBlake3(
        entry->getObjectId(), fetchContext);
  } else if (auto* treeEntry = std::get_if<TreeEntry>(&variant_)) {
    const auto& hash = treeEntry->getContentBlake3();
    if (hash.has_value()) {
      co_return hash.value();
    }
    co_return co_await objectStore->co_getBlobBlake3(
        treeEntry->getObjectId(), fetchContext);
  } else {
    // TreePtr - directories cannot have blake3
    co_yield folly::coro::co_error(PathError(EISDIR, path));
  }
}

folly::coro::now_task<std::optional<Hash32>> VirtualInode::co_getDigestHash(
    RelativePathPiece path,
    const std::shared_ptr<ObjectStore>& objectStore,
    const ObjectFetchContextPtr& fetchContext) const {
  const auto dtype = getDtype();
  if (dtype == dtype_t::Symlink) {
    co_yield folly::coro::co_error(
        PathError(EINVAL, path, std::string_view{"file is a symlink"}));
  } else if (dtype != dtype_t::Regular && dtype != dtype_t::Dir) {
    co_yield folly::coro::co_error(PathError(
        EINVAL, path, std::string_view{"variant is of unhandled type"}));
  }

  // Use std::get_if instead of match because coroutine lambda captures are
  // stored in the lambda object, not the coroutine frame. If the coroutine
  // suspends, match destroys the lambda temporaries, and resuming accesses
  // dangling captures.
  static_assert(
      std::variant_size_v<detail::VariantVirtualInode> == 4,
      "New variant type added to VariantVirtualInode - update co_getDigestHash");

  if (dtype == dtype_t::Regular) {
    // DigestHash of a file is its Blake3 hash.
    co_return std::optional<Hash32>{
        co_await co_getBlake3(path, objectStore, fetchContext)};
  }

  // A restricted directory is an ACL-denied placeholder that still carries the
  // real ObjectId, so dispatching below would fetch the backing store's tree
  // aux data and leak a digest of contents the server refused to serve. Report
  // "no digest" instead.
  if (FOLLY_UNLIKELY(isRestricted())) {
    co_return std::nullopt;
  }

  if (auto* inode = std::get_if<InodePtr>(&variant_)) {
    co_return co_await inode->asTreePtr()->co_getDigestHash(fetchContext);
  } else if (
      auto* entry =
          std::get_if<UnmaterializedUnloadedBlobDirEntry>(&variant_)) {
    co_return co_await objectStore->co_getTreeDigestHash(
        entry->getObjectId(), fetchContext);
  } else if (auto* tree = std::get_if<TreePtr>(&variant_)) {
    co_return co_await objectStore->co_getTreeDigestHash(
        (*tree)->getObjectId(), fetchContext);
  } else if (auto* treeEntry = std::get_if<TreeEntry>(&variant_)) {
    co_return co_await objectStore->co_getTreeDigestHash(
        treeEntry->getObjectId(), fetchContext);
  } else {
    co_yield folly::coro::co_error(PathError(
        EINVAL, path, std::string_view{"variant is of unhandled type"}));
  }
}

folly::coro::now_task<Hash20> VirtualInode::co_getSHA1(
    RelativePathPiece path,
    const std::shared_ptr<ObjectStore>& objectStore,
    const ObjectFetchContextPtr& fetchContext) const {
  const auto dtype = getDtype();
  if (dtype == dtype_t::Dir) {
    co_yield folly::coro::co_error(PathError(EISDIR, path));
  } else if (dtype == dtype_t::Symlink) {
    co_yield folly::coro::co_error(
        PathError(EINVAL, path, std::string_view{"file is a symlink"}));
  } else if (dtype != dtype_t::Regular) {
    co_yield folly::coro::co_error(PathError(
        EINVAL, path, std::string_view{"variant is of unhandled type"}));
  }

  static_assert(
      std::variant_size_v<detail::VariantVirtualInode> == 4,
      "New variant type added to VariantVirtualInode - update co_getSHA1");
  if (auto* inode = std::get_if<InodePtr>(&variant_)) {
    co_return co_await inode->asFilePtr()->co_getSha1(fetchContext);
  } else if (
      auto* entry =
          std::get_if<UnmaterializedUnloadedBlobDirEntry>(&variant_)) {
    co_return co_await objectStore->co_getBlobSha1(
        entry->getObjectId(), fetchContext);
  } else if (auto* treeEntry = std::get_if<TreeEntry>(&variant_)) {
    const auto& hash = treeEntry->getContentSha1();
    if (hash.has_value()) {
      co_return hash.value();
    }
    co_return co_await objectStore->co_getBlobSha1(
        treeEntry->getObjectId(), fetchContext);
  } else {
    co_yield folly::coro::co_error(PathError(EISDIR, path));
  }
}

std::optional<TreeEntryType> VirtualInode::tryGetTreeEntryType() const {
  static_assert(
      std::variant_size_v<detail::VariantVirtualInode> == 4,
      "New variant type added to VariantVirtualInode - update tryGetTreeEntryType");
  if (auto* inode = std::get_if<InodePtr>(&variant_)) {
    if ((*inode)->getType() == dtype_t::Dir) {
      return TreeEntryType::TREE;
    }
#ifdef _WIN32
    return treeEntryTypeFromMode((*inode)->getInitialMode());
#else
    if (auto filePtr = inode->asFilePtrOrNull()) {
      // getMode() picks up chmod-updated executable bits.
      return treeEntryTypeFromMode(filePtr->getMode());
    }
    // Non-Dir InodePtr must be a FileInode; reaching here would indicate
    // a new InodePtr subtype not handled above.
    XLOG_EVERY_MS(DFATAL, 60'000)
        << "VirtualInode::tryGetTreeEntryType: non-Dir InodePtr without FilePtr";
    return std::nullopt;
#endif
  } else if (
      auto* entry =
          std::get_if<UnmaterializedUnloadedBlobDirEntry>(&variant_)) {
    return treeEntryTypeFromMode(entry->getInitialMode());
  } else if (std::holds_alternative<TreePtr>(variant_)) {
    return TreeEntryType::TREE;
  } else if (auto* treeEntry = std::get_if<TreeEntry>(&variant_)) {
    return treeEntry->getType();
  }
  return std::nullopt;
}

ImmediateFuture<BlobAuxData> VirtualInode::getBlobAuxData(
    RelativePathPiece path,
    const std::shared_ptr<ObjectStore>& objectStore,
    const ObjectFetchContextPtr& fetchContext,
    bool blake3Required) const {
  return match(
      variant_,
      [&](const InodePtr& inode) {
        return inode.asFilePtr()->getBlobAuxData(fetchContext, blake3Required);
      },
      [&](const TreePtr&) {
        return makeImmediateFuture<BlobAuxData>(PathError(EISDIR, path));
      },
      [&](auto& entry) {
        return objectStore->getBlobAuxData(
            entry.getObjectId(), fetchContext, blake3Required);
      });
}

std::optional<BlobAuxData> VirtualInode::tryGetCachedBlobAuxData(
    const std::shared_ptr<ObjectStore>& objectStore,
    const ObjectFetchContextPtr& fetchContext,
    bool blake3Required) const {
  static_assert(
      std::variant_size_v<detail::VariantVirtualInode> == 4,
      "New variant type added to VariantVirtualInode - update tryGetCachedBlobAuxData");
  if (auto* inode = std::get_if<InodePtr>(&variant_)) {
    // TreeInode-backed InodePtrs have no blob aux.
    if ((*inode)->getType() == dtype_t::Dir) {
      return std::nullopt;
    }
    return inode->asFilePtr()->tryGetCachedBlobAuxData(
        fetchContext, blake3Required);
  } else if (
      auto* entry =
          std::get_if<UnmaterializedUnloadedBlobDirEntry>(&variant_)) {
    return objectStore->getBlobAuxDataFromInMemoryCache(
        entry->getObjectId(), fetchContext, blake3Required);
  } else if (auto* treeEntry = std::get_if<TreeEntry>(&variant_)) {
    if (auto inlineAux = treeEntry->tryGetInlineBlobAuxData(blake3Required)) {
      return inlineAux;
    }
    return objectStore->getBlobAuxDataFromInMemoryCache(
        treeEntry->getObjectId(), fetchContext, blake3Required);
  }
  // TreePtr or unexpected: no blob aux available.
  return std::nullopt;
}

ImmediateFuture<std::optional<TreeAuxData>> VirtualInode::getTreeAuxData(
    const std::shared_ptr<ObjectStore>& objectStore,
    const ObjectFetchContextPtr& fetchContext) const {
  if (isRestricted()) {
    return std::optional<TreeAuxData>{std::nullopt};
  }
  return match(
      variant_,
      [&](const InodePtr& inode) {
        return inode.asTreePtr()->getTreeAuxData(fetchContext);
      },
      [&](const TreePtr& tree) {
        return objectStore->getTreeAuxData(tree->getObjectId(), fetchContext);
      },
      [&](auto& entry) {
        return objectStore->getTreeAuxData(entry.getObjectId(), fetchContext);
      });
}

folly::coro::now_task<std::optional<TreeAuxData>>
VirtualInode::co_getTreeAuxData(
    const std::shared_ptr<ObjectStore>& objectStore,
    const ObjectFetchContextPtr& fetchContext) const {
  static_assert(
      std::variant_size_v<detail::VariantVirtualInode> == 4,
      "New variant type added to VariantVirtualInode - update co_getTreeAuxData");
  if (isRestricted()) {
    co_return std::nullopt;
  }
  if (auto* inode = std::get_if<InodePtr>(&variant_)) {
    co_return co_await inode->asTreePtr()->co_getTreeAuxData(fetchContext);
  } else if (auto* tree = std::get_if<TreePtr>(&variant_)) {
    co_return co_await objectStore->co_getTreeAuxData(
        (*tree)->getObjectId(), fetchContext);
  } else if (
      auto* entry =
          std::get_if<UnmaterializedUnloadedBlobDirEntry>(&variant_)) {
    co_return co_await objectStore->co_getTreeAuxData(
        entry->getObjectId(), fetchContext);
  } else if (auto* treeEntry = std::get_if<TreeEntry>(&variant_)) {
    co_return co_await objectStore->co_getTreeAuxData(
        treeEntry->getObjectId(), fetchContext);
  }
  co_yield folly::coro::co_error(
      std::runtime_error("VirtualInode: unexpected variant type"));
}

namespace {

struct stat makeStatFromModeAndSize(
    mode_t mode,
    uint64_t size,
    struct timespec lastCheckoutTime) {
  struct stat st = {};
#ifndef _WIN32
  st.st_mode = static_cast<decltype(st.st_mode)>(mode);
  stMtime(st, lastCheckoutTime);
#else
  // Windows returns zero for st_mode and mtime; intentionally discard.
  (void)mode;
  (void)lastCheckoutTime;
#endif
  st.st_size = static_cast<decltype(st.st_size)>(size);
  return st;
}

struct BlobAuxBackedAttrs {
  ObjectId id;
  mode_t mode;
};
std::optional<BlobAuxBackedAttrs> getBlobAuxBackedAttrs(
    const detail::VariantVirtualInode& v) {
  if (auto* entry = std::get_if<UnmaterializedUnloadedBlobDirEntry>(&v)) {
    return BlobAuxBackedAttrs{entry->getObjectId(), entry->getInitialMode()};
  }
  if (auto* treeEntry = std::get_if<TreeEntry>(&v)) {
    return BlobAuxBackedAttrs{
        treeEntry->getObjectId(), modeFromTreeEntryType(treeEntry->getType())};
  }
  return std::nullopt;
}

bool shouldRequestTreeAuxDataForEntry(
    const std::optional<TreeEntryType>& entryType,
    EntryAttributeFlags entryAttributes,
    const bool isMaterialized) {
  return (entryType.value_or(TreeEntryType::SYMLINK) == TreeEntryType::TREE) &&
      entryAttributes.containsAnyOf(ENTRY_ATTRIBUTES_FROM_TREE_AUX) &&
      !isMaterialized;
}

bool shouldRequestStatForEntry(EntryAttributeFlags entryAttributes) {
  return entryAttributes.containsAnyOf(ENTRY_ATTRIBUTES_FROM_STAT);
}

void populateInvalidNonFileAttributes(
    EntryAttributes& attributes,
    EntryAttributeFlags requestedAttributes,
    int errorCode,
    RelativePathPiece path,
    std::optional<TreeEntryType> entryType,
    std::string_view additionalErrorContext) {
  // It's invalid to request sha1, size, and blake3 for non-file entries
  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_SHA1)) {
    attributes.sha1 =
        folly::Try<Hash20>{PathError{errorCode, path, additionalErrorContext}};
  }

  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_SIZE)) {
    attributes.size = folly::Try<uint64_t>{
        PathError{errorCode, path, additionalErrorContext}};
  }

  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_BLAKE3)) {
    attributes.blake3 =
        folly::Try<Hash32>{PathError{errorCode, path, additionalErrorContext}};
  }

  // Aux data specific to tree entries was requested, but the entry we're
  // processing is a symlink, socket, or other unsupported type.
  //
  // entryType is std::nullopt if the entry is a socket or other non-scm type
  if (entryType.value_or(TreeEntryType::SYMLINK) != TreeEntryType::TREE) {
    if (requestedAttributes.contains(ENTRY_ATTRIBUTE_DIGEST_SIZE)) {
      attributes.digestSize = folly::Try<uint64_t>{
          PathError{errorCode, path, additionalErrorContext}};
    }

    if (requestedAttributes.contains(ENTRY_ATTRIBUTE_DIGEST_HASH)) {
      attributes.digestHash = folly::Try<Hash32>{
          PathError{errorCode, path, additionalErrorContext}};
    }
  }
}

void populateTreeAuxAttributes(
    EntryAttributes& attributes,
    EntryAttributeFlags requestedAttributes,
    const folly::Try<std::optional<TreeAuxData>>& treeAuxTry) {
  if (treeAuxTry.hasException()) {
    // We failed to get tree aux data. This shouldn't cause the
    // entire result to be an error. We can return whichever
    // attributes we successfully fetched.
    if (requestedAttributes.contains(ENTRY_ATTRIBUTE_DIGEST_HASH)) {
      attributes.digestHash = folly::Try<Hash32>{treeAuxTry.exception()};
    }

    if (requestedAttributes.contains(ENTRY_ATTRIBUTE_DIGEST_SIZE)) {
      attributes.digestSize = folly::Try<uint64_t>{treeAuxTry.exception()};
    }
  } else {
    // The tree aux data request didn't error out, but we may have received
    // "nullopt" as the result (indicating no tree aux data is currently
    // computed for this entry). If that's the case, set the entire attribute as
    // nullopt to trigger ATTRIBUTE_UNAVAILABLE errors when results are
    // processed.
    auto treeAux = treeAuxTry.value();
    if (requestedAttributes.contains(ENTRY_ATTRIBUTE_DIGEST_HASH)) {
      attributes.digestHash = treeAux.has_value()
          ? std::optional<folly::Try<Hash32>>{treeAux.value().digestHash}
          : std::nullopt;
    }
    if (requestedAttributes.contains(ENTRY_ATTRIBUTE_DIGEST_SIZE)) {
      attributes.digestSize = treeAux.has_value()
          ? std::optional<folly::Try<uint64_t>>{treeAux.value().digestSize}
          : std::nullopt;
    }
  }
}

void populateStatAttributes(
    EntryAttributes& attributes,
    EntryAttributeFlags requestedAttributes,
    const folly::Try<struct stat>& statTry) {
  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_MTIME)) {
    attributes.mtime = statTry.hasException()
        ? folly::Try<timespec>{statTry.exception()}
        : folly::Try<timespec>{stMtime(statTry.value())};
  }

  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_MODE)) {
    attributes.mode = statTry.hasException()
        ? folly::Try<mode_t>{statTry.exception()}
        : folly::Try<mode_t>{statTry.value().st_mode};
  }
}

struct NonFileAttributePreamble {
  EntryAttributes attributes;
  bool isMaterialized{};
};

NonFileAttributePreamble setupNonFileAttributes(
    const VirtualInode& vi,
    EntryAttributeFlags requestedAttributes,
    RelativePathPiece path,
    const std::shared_ptr<ObjectStore>& objectStore,
    std::optional<TreeEntryType> entryType,
    int errorCode,
    std::string_view additionalErrorContext) {
  NonFileAttributePreamble result;
  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_SOURCE_CONTROL_TYPE)) {
    result.attributes.type =
        folly::Try<std::optional<TreeEntryType>>{entryType};
  }
  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_OBJECT_ID)) {
    result.attributes.objectId =
        folly::Try<std::optional<ObjectId>>{vi.getObjectId()};
    result.isMaterialized =
        !result.attributes.objectId.value().value().has_value();
  } else {
    result.isMaterialized = vi.isMaterialized();
  }
  populateLocalAclAttributes(
      result.attributes,
      requestedAttributes,
      vi.isUnderAcl(),
      shouldPopulateLocalAclAttributes(objectStore));
  populateInvalidNonFileAttributes(
      result.attributes,
      requestedAttributes,
      errorCode,
      path,
      entryType,
      additionalErrorContext);
  return result;
}

folly::coro::now_task<EntryAttributes> co_getEntryAttributesForNonFile(
    const VirtualInode& vi,
    EntryAttributeFlags requestedAttributes,
    RelativePathPiece path,
    const std::shared_ptr<ObjectStore>& objectStore,
    timespec lastCheckoutTime,
    const ObjectFetchContextPtr& fetchContext,
    std::optional<TreeEntryType> entryType,
    int errorCode,
    std::string_view additionalErrorContext) {
  auto [attributes, isMat] = setupNonFileAttributes(
      vi,
      requestedAttributes,
      path,
      objectStore,
      entryType,
      errorCode,
      additionalErrorContext);

  std::optional<folly::Try<struct stat>> statTry;
  std::optional<folly::Try<std::optional<TreeAuxData>>> treeAuxTry;
  std::vector<folly::coro::Task<void>> tasks;

  if (shouldRequestStatForEntry(requestedAttributes)) {
    tasks.emplace_back(
        // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
        folly::coro::co_invoke(
            [&vi,
             &statTry,
             &lastCheckoutTime,
             objectStore = objectStore,
             fetchContext = fetchContext.copy()]() -> folly::coro::Task<void> {
              statTry = co_await co_awaitTry(
                  vi.co_stat(lastCheckoutTime, objectStore, fetchContext));
            }));
  }

  if (shouldRequestTreeAuxDataForEntry(entryType, requestedAttributes, isMat)) {
    tasks.emplace_back(
        // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
        folly::coro::co_invoke(
            [&vi,
             &treeAuxTry,
             objectStore = objectStore,
             fetchContext = fetchContext.copy()]() -> folly::coro::Task<void> {
              treeAuxTry = co_await co_awaitTry(
                  vi.co_getTreeAuxData(objectStore, fetchContext));
            }));
  }

  if (!tasks.empty()) {
    co_await folly::coro::collectAllRange(std::move(tasks));
  }

  if (statTry.has_value()) {
    populateStatAttributes(attributes, requestedAttributes, *statTry);
  }

  if (treeAuxTry.has_value()) {
    populateTreeAuxAttributes(attributes, requestedAttributes, *treeAuxTry);
  }

  co_return attributes;
}

/**
 * Sync version of co_getEntryAttributesForNonFile. Mirrors its dispatch
 * but resolves entirely from cached
 * state. Returns nullopt if any requested attribute would need an async
 * fetch.
 */
std::optional<EntryAttributes> tryGetEntryAttributesForNonFileSync(
    const VirtualInode& vi,
    EntryAttributeFlags requestedAttributes,
    RelativePathPiece path,
    const std::shared_ptr<ObjectStore>& objectStore,
    timespec lastCheckoutTime,
    const ObjectFetchContextPtr& fetchContext,
    std::optional<TreeEntryType> entryType,
    int errorCode,
    std::string_view additionalErrorContext) {
  auto [attributes, isMat] = setupNonFileAttributes(
      vi,
      requestedAttributes,
      path,
      objectStore,
      entryType,
      errorCode,
      additionalErrorContext);

  if (shouldRequestStatForEntry(requestedAttributes)) {
    auto statOpt =
        vi.tryGetCachedStat(lastCheckoutTime, objectStore, fetchContext);
    if (!statOpt) {
      return std::nullopt;
    }
    folly::Try<struct stat> statTry{*statOpt};
    populateStatAttributes(attributes, requestedAttributes, statTry);
  }

  if (shouldRequestTreeAuxDataForEntry(entryType, requestedAttributes, isMat) &&
      !vi.isRestricted()) {
    // Sync peek the ObjectStore in-memory cache for tree aux data. If the
    // entry has no ObjectId or the cache is cold, bail.
    auto oid = vi.getObjectId();
    if (!oid) {
      return std::nullopt;
    }
    auto treeAux =
        objectStore->getTreeAuxDataFromInMemoryCache(*oid, fetchContext);
    if (!treeAux) {
      return std::nullopt;
    }
    folly::Try<std::optional<TreeAuxData>> treeAuxTry{std::move(treeAux)};
    populateTreeAuxAttributes(attributes, requestedAttributes, treeAuxTry);
  }

  return attributes;
}
} // namespace

namespace {
void populateBlobAuxAttributes(
    EntryAttributes& attributes,
    EntryAttributeFlags requestedAttributes,
    const folly::Try<BlobAuxData>& blobAuxTry) {
  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_SHA1)) {
    attributes.sha1 = blobAuxTry.hasException()
        ? folly::Try<Hash20>(blobAuxTry.exception())
        : folly::Try<Hash20>(blobAuxTry.value().sha1);
  }

  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_BLAKE3)) {
    if (blobAuxTry.hasException()) {
      attributes.blake3 = folly::Try<Hash32>(blobAuxTry.exception());
    } else {
      attributes.blake3 = blobAuxTry.value().blake3.has_value()
          ? std::optional<folly::Try<Hash32>>(blobAuxTry.value().blake3.value())
          : std::nullopt;
    }
  }

  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_SIZE)) {
    attributes.size = blobAuxTry.hasException()
        ? folly::Try<uint64_t>(blobAuxTry.exception())
        : folly::Try<uint64_t>(blobAuxTry.value().size);
  }

  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_DIGEST_SIZE)) {
    attributes.digestSize = blobAuxTry.hasException()
        ? folly::Try<uint64_t>(blobAuxTry.exception())
        : folly::Try<uint64_t>(blobAuxTry.value().size);
  }

  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_DIGEST_HASH)) {
    if (blobAuxTry.hasException()) {
      attributes.digestHash = folly::Try<Hash32>(blobAuxTry.exception());
    } else {
      attributes.digestHash = blobAuxTry.value().blake3.has_value()
          ? std::optional<folly::Try<Hash32>>(blobAuxTry.value().blake3.value())
          : std::nullopt;
    }
  }
}

bool shouldRequestBlobAuxDataForEntry(EntryAttributeFlags entryAttributes) {
  return entryAttributes.containsAnyOf(ENTRY_ATTRIBUTES_FROM_BLOB_AUX);
}

// Dispatch info for non-Regular dtypes — what to pass to the per-arm
// NonFile helper (futures / coro / sync). Returns nullopt for Regular.
struct NonRegularDispatch {
  std::optional<TreeEntryType> entryType;
  int errorCode;
  std::string errorContext;
};

std::optional<NonRegularDispatch> getNonRegularDtypeDispatch(dtype_t dtype) {
  auto nonSourceControl = [&] {
    return NonRegularDispatch{
        std::nullopt,
        EINVAL,
        fmt::format(
            "file is a non-source-control type: {}",
            folly::to_underlying(dtype))};
  };
  switch (dtype) {
    case dtype_t::Regular:
      return std::nullopt;
    case dtype_t::Dir:
      return NonRegularDispatch{TreeEntryType::TREE, EISDIR, {}};
    case dtype_t::Symlink:
      return NonRegularDispatch{
          TreeEntryType::SYMLINK, EINVAL, "file is a symlink"};
    case dtype_t::Unknown:
    case dtype_t::Fifo:
    case dtype_t::Char:
    case dtype_t::Socket:
#ifndef _WIN32
    case dtype_t::Block:
    case dtype_t::Whiteout:
#endif
      return nonSourceControl();
  }
  // Unreachable: the switch enumerates every dtype_t. Defensive fallback in
  // case a new enum value is added.
  return nonSourceControl();
}

// Shared regular-file attribute assembly. Each fetch path (futures, coro,
// sync) computes its own per-attribute Try values; this helper applies
// them to a fresh EntryAttributes, filtered by requestedAttributes. Pass
// nullopt for blobAuxTry / statTry to skip those attribute classes.
EntryAttributes assembleRegularFileAttributes(
    EntryAttributeFlags requestedAttributes,
    folly::Try<std::optional<TreeEntryType>> entryTypeTry,
    folly::Try<std::optional<ObjectId>> objectIdTry,
    std::optional<folly::Try<BlobAuxData>> blobAuxTry,
    std::optional<folly::Try<struct stat>> statTry,
    std::optional<bool> underAcl,
    bool populateAcl) {
  auto attributes = EntryAttributes{};
  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_SOURCE_CONTROL_TYPE)) {
    attributes.type = std::move(entryTypeTry);
  }
  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_OBJECT_ID)) {
    attributes.objectId = std::move(objectIdTry);
  }
  populateLocalAclAttributes(
      attributes, requestedAttributes, underAcl, populateAcl);
  if (blobAuxTry.has_value()) {
    populateBlobAuxAttributes(attributes, requestedAttributes, *blobAuxTry);
  }
  if (statTry.has_value()) {
    populateStatAttributes(attributes, requestedAttributes, *statTry);
  }
  return attributes;
}
} // namespace

folly::coro::now_task<EntryAttributes> VirtualInode::co_getEntryAttributes(
    EntryAttributeFlags requestedAttributes,
    RelativePathPiece path,
    const std::shared_ptr<ObjectStore>& objectStore,
    timespec lastCheckoutTime,
    const ObjectFetchContextPtr& fetchContext) const {
  if (auto dispatch = getNonRegularDtypeDispatch(getDtype())) {
    co_return co_await co_getEntryAttributesForNonFile(
        *this,
        requestedAttributes,
        path,
        objectStore,
        lastCheckoutTime,
        fetchContext,
        dispatch->entryType,
        dispatch->errorCode,
        dispatch->errorContext);
  }

  std::optional<folly::Try<std::optional<TreeEntryType>>> entryTypeTry;
  std::optional<folly::Try<BlobAuxData>> blobAuxTry;
  std::optional<folly::Try<struct stat>> statTry;

  std::vector<folly::coro::Task<void>> tasks;

  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_SOURCE_CONTROL_TYPE)) {
    entryTypeTry =
        folly::Try<std::optional<TreeEntryType>>{tryGetTreeEntryType()};
  }

  if (shouldRequestBlobAuxDataForEntry(requestedAttributes)) {
    tasks.emplace_back(
        // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
        folly::coro::co_invoke(
            [this,
             &blobAuxTry,
             path,
             objectStore = objectStore,
             fetchContext = fetchContext.copy(),
             blake3Required = requestedAttributes.containsAnyOf(
                 ENTRY_ATTRIBUTE_BLAKE3 |
                 ENTRY_ATTRIBUTE_DIGEST_HASH)]() -> folly::coro::Task<void> {
              blobAuxTry = co_await folly::coro::co_awaitTry(
                  getBlobAuxData(
                      path, objectStore, fetchContext, blake3Required)
                      .semi());
            }));
  }

  if (shouldRequestStatForEntry(requestedAttributes)) {
    tasks.emplace_back(
        // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
        folly::coro::co_invoke(
            [this,
             &statTry,
             &lastCheckoutTime,
             objectStore = objectStore,
             fetchContext = fetchContext.copy()]() -> folly::coro::Task<void> {
              statTry = co_await co_awaitTry(
                  co_stat(lastCheckoutTime, objectStore, fetchContext));
            }));
  }

  if (!tasks.empty()) {
    co_await folly::coro::collectAllRange(std::move(tasks));
  }

  std::optional<ObjectId> objectId;
  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_OBJECT_ID)) {
    objectId = getObjectId();
  }

  co_return assembleRegularFileAttributes(
      requestedAttributes,
      entryTypeTry.has_value()
          ? std::move(*entryTypeTry)
          : folly::Try<std::optional<TreeEntryType>>{std::nullopt},
      folly::Try<std::optional<ObjectId>>{std::move(objectId)},
      std::move(blobAuxTry),
      std::move(statTry),
      isUnderAcl(),
      shouldPopulateLocalAclAttributes(objectStore));
}

std::optional<EntryAttributes> VirtualInode::tryGetEntryAttributesSync(
    EntryAttributeFlags requestedAttributes,
    RelativePathPiece path,
    const std::shared_ptr<ObjectStore>& objectStore,
    timespec lastCheckoutTime,
    const ObjectFetchContextPtr& fetchContext) const {
  if (auto dispatch = getNonRegularDtypeDispatch(getDtype())) {
    return tryGetEntryAttributesForNonFileSync(
        *this,
        requestedAttributes,
        path,
        objectStore,
        lastCheckoutTime,
        fetchContext,
        dispatch->entryType,
        dispatch->errorCode,
        dispatch->errorContext);
  }

  // Regular file path.
  std::optional<folly::Try<BlobAuxData>> blobAuxTry;
  // Cache the blob aux peek so stat synthesis below can reuse it instead
  // of peeking the same variant twice.
  std::optional<BlobAuxData> blobAuxHint;
  if (shouldRequestBlobAuxDataForEntry(requestedAttributes)) {
    bool blake3Required = requestedAttributes.containsAnyOf(
        ENTRY_ATTRIBUTE_BLAKE3 | ENTRY_ATTRIBUTE_DIGEST_HASH);
    // All-or-nothing per attribute class: if blake3 is required but the
    // cached entry lacks it, bail to async even if other attribute classes
    // (e.g. stat, which only needs size) could be filled. Partial sync
    // resolution would complicate the orchestrator without a measured win.
    blobAuxHint =
        tryGetCachedBlobAuxData(objectStore, fetchContext, blake3Required);
    if (!blobAuxHint) {
      return std::nullopt;
    }
    blobAuxTry = folly::Try<BlobAuxData>{*blobAuxHint};
  }

  std::optional<folly::Try<struct stat>> statTry;
  if (shouldRequestStatForEntry(requestedAttributes)) {
    auto st = tryGetCachedStat(
        lastCheckoutTime, objectStore, fetchContext, blobAuxHint);
    if (!st) {
      return std::nullopt;
    }
    statTry = folly::Try<struct stat>{*st};
  }

  return assembleRegularFileAttributes(
      requestedAttributes,
      folly::Try<std::optional<TreeEntryType>>{tryGetTreeEntryType()},
      folly::Try<std::optional<ObjectId>>{getObjectId()},
      std::move(blobAuxTry),
      std::move(statTry),
      isUnderAcl(),
      shouldPopulateLocalAclAttributes(objectStore));
}

folly::coro::now_task<struct stat> VirtualInode::co_stat(
    const struct timespec& lastCheckoutTime,
    const std::shared_ptr<ObjectStore>& objectStore,
    const ObjectFetchContextPtr& fetchContext) const {
  static_assert(
      std::variant_size_v<detail::VariantVirtualInode> == 4,
      "New variant type added to VariantVirtualInode - update co_stat");
  // Route InodePtr through the child inode's co_stat to preserve the
  // logAccess side effect.
  if (auto* inode = std::get_if<InodePtr>(&variant_)) {
    co_return co_await (*inode)->co_stat(fetchContext);
  }
  if (auto sync =
          tryGetCachedStat(lastCheckoutTime, objectStore, fetchContext)) {
    co_return *sync;
  }
  auto info = getBlobAuxBackedAttrs(variant_);
  if (!info) {
    co_yield folly::coro::co_error(
        std::runtime_error("VirtualInode: unexpected variant type"));
  }
  auto auxData =
      co_await objectStore->co_getBlobAuxData(info->id, fetchContext);
  co_return makeStatFromModeAndSize(info->mode, auxData.size, lastCheckoutTime);
}

std::optional<struct stat> VirtualInode::tryGetCachedStat(
    const struct timespec& lastCheckoutTime,
    const std::shared_ptr<ObjectStore>& objectStore,
    const ObjectFetchContextPtr& fetchContext,
    const std::optional<BlobAuxData>& blobAuxHint) const {
  static_assert(
      std::variant_size_v<detail::VariantVirtualInode> == 4,
      "New variant type added to VariantVirtualInode - update tryGetCachedStat");

  if (auto* inode = std::get_if<InodePtr>(&variant_)) {
    if ((*inode)->getType() != dtype_t::Regular &&
        (*inode)->getType() != dtype_t::Symlink) {
      return std::nullopt;
    }
    return inode->asFilePtr()->tryGetCachedStat();
  }

  if (auto* tree = std::get_if<TreePtr>(&variant_)) {
    (void)tree;
    return makeStatFromModeAndSize(treeMode_, 0U, lastCheckoutTime);
  }

  std::optional<BlobAuxData> blobAux = blobAuxHint;
  if (!blobAux) {
    blobAux = tryGetCachedBlobAuxData(
        objectStore, fetchContext, /*blake3Required=*/false);
  }
  if (!blobAux) {
    return std::nullopt;
  }
  // At this point variant_ must be UnmaterializedUnloadedBlobDirEntry
  // or TreeEntry (InodePtr + TreePtr handled above; static_assert at the
  // tryGetCachedBlobAuxData call site pins the variant set to 4).
  auto info = getBlobAuxBackedAttrs(variant_);
  XDCHECK(info.has_value())
      << "VirtualInode::tryGetCachedStat: variant fell through to blob-aux-backed "
         "branch but getBlobAuxBackedAttrs returned nullopt";
  if (!info) {
    return std::nullopt;
  }
  return makeStatFromModeAndSize(info->mode, blobAux->size, lastCheckoutTime);
}

namespace {
VirtualInode applyAncestorAcl(
    VirtualInode child,
    std::optional<bool> ancestorUnderAcl) {
  child.inheritAclFromAncestor(ancestorUnderAcl);
  return child;
}

folly::Try<VirtualInode> applyAncestorAcl(
    folly::Try<VirtualInode> child,
    std::optional<bool> ancestorUnderAcl) {
  if (child.hasValue()) {
    child.value().inheritAclFromAncestor(ancestorUnderAcl);
  }
  return child;
}

folly::coro::now_task<
    std::vector<std::pair<PathComponent, folly::Try<VirtualInode>>>>
getChildrenHelper(
    const TreePtr& tree,
    const std::shared_ptr<ObjectStore>& objectStore,
    const ObjectFetchContextPtr& fetchContext) {
  // Async entries get a placeholder Try; the matching task back-fills
  // by index after collectAllTryRange so result preserves iteration order.
  std::vector<std::pair<PathComponent, folly::Try<VirtualInode>>> result;
  result.reserve(tree->size());
  std::vector<folly::coro::Task<VirtualInode>> tasks;
  std::vector<size_t> taskIdx;

  for (auto& child :
       visibleEntries(*tree, objectStore->getRestrictedContentMode())) {
    const auto* treeEntry = &child.second;
    if (treeEntry->isTree()) {
      if (treeEntry->isRestricted()) {
        // Restricted child: synthesize a placeholder; never fetch its contents.
        result.emplace_back(
            child.first,
            folly::Try<VirtualInode>{VirtualInode::makeRestricted(
                *treeEntry, tree->getCaseSensitivity())});
      } else {
        taskIdx.push_back(result.size());
        result.emplace_back(
            child.first, folly::Try<VirtualInode>{folly::FutureNotReady{}});
        tasks.emplace_back(
            // @lint-ignore CLANGTIDY
            // facebook-folly-coro-return-captures-local-var
            folly::coro::co_invoke(
                [](ObjectId oid,
                   mode_t mode,
                   std::shared_ptr<ObjectStore> store,
                   ObjectFetchContextPtr ctx,
                   std::optional<bool> hasACL)
                    -> folly::coro::Task<VirtualInode> {
                  co_await folly::coro::co_reschedule_on_current_executor;
                  auto childTree = co_await store->co_getTree(oid, ctx);
                  auto virtualInode = VirtualInode{std::move(childTree), mode};
                  virtualInode.setHasACL(hasACL);
                  co_return virtualInode;
                },
                treeEntry->getObjectId(),
                modeFromTreeEntryType(treeEntry->getType()),
                objectStore,
                fetchContext.copy(),
                treeEntry->hasACL()));
      }
    } else {
      result.emplace_back(
          child.first, folly::Try<VirtualInode>{VirtualInode{*treeEntry}});
    }
  }

  if (!tasks.empty()) {
    auto tries = co_await folly::coro::collectAllTryRange(std::move(tasks));
    XCHECK_EQ(tries.size(), taskIdx.size());
    for (size_t i = 0; i < tries.size(); ++i) {
      result.at(taskIdx.at(i)).second = std::move(tries[i]);
    }
  }
  co_return result;
}
} // namespace

folly::coro::now_task<
    std::vector<std::pair<PathComponent, folly::Try<VirtualInode>>>>
VirtualInode::getChildren(
    RelativePathPiece path,
    const std::shared_ptr<ObjectStore>& objectStore,
    const ObjectFetchContextPtr& fetchContext) {
  if (!isDirectory()) {
    co_yield folly::coro::co_error(PathError(ENOTDIR, path));
  }

  static_assert(
      std::variant_size_v<detail::VariantVirtualInode> == 4,
      "New variant type added to VariantVirtualInode - update getChildren");
  if (auto* inode = std::get_if<InodePtr>(&variant_)) {
    auto children = co_await inode->asTreePtr()->getChildren(
        fetchContext, /*loadInodes=*/false);
    for (auto& child : children) {
      child.second = applyAncestorAcl(std::move(child.second), isUnderAcl());
    }
    co_return children;
  } else if (auto* tree = std::get_if<TreePtr>(&variant_)) {
    // Restricted unloaded tree denies enumeration outright.
    if ((*tree)->isRestricted()) {
      co_yield folly::coro::co_error(PathError(EACCES, path));
    }
    auto children =
        co_await getChildrenHelper(*tree, objectStore, fetchContext);
    for (auto& child : children) {
      child.second = applyAncestorAcl(std::move(child.second), isUnderAcl());
    }
    co_return children;
  } else {
    // File variants (UnmaterializedUnloadedBlobDirEntry / TreeEntry) — the
    // !isDirectory() guard above has already returned ENOTDIR; this is a
    // defensive fallthrough kept honest by the static_assert above.
    co_yield folly::coro::co_error(PathError(ENOTDIR, path));
  }
}

folly::coro::now_task<
    std::vector<std::pair<PathComponent, folly::Try<EntryAttributes>>>>
VirtualInode::co_getChildrenAttributes(
    EntryAttributeFlags requestedAttributes,
    RelativePath path,
    const std::shared_ptr<ObjectStore>& objectStore,
    timespec lastCheckoutTime,
    const ObjectFetchContextPtr& fetchContext) {
  if (!isDirectory()) {
    co_yield folly::coro::co_error(PathError(ENOTDIR, path));
  }

  static_assert(
      std::variant_size_v<detail::VariantVirtualInode> == 4,
      "New variant type added to VariantVirtualInode - "
      "update co_getChildrenAttributes");

  if (auto* inode = std::get_if<InodePtr>(&variant_)) {
    co_return co_await inode->asTreePtr()->co_getChildrenAttributes(
        requestedAttributes,
        std::move(path),
        objectStore,
        lastCheckoutTime,
        fetchContext,
        ancestorUnderAcl_);
  }

  // Past the !isDirectory() guard, the static_assert above pins the variant
  // to {InodePtr, TreePtr, UnmaterializedUnloadedBlobDirEntry, TreeEntry};
  // the latter two are non-directories so we must hold a TreePtr here.
  auto* tree = std::get_if<TreePtr>(&variant_);
  XCHECK(tree != nullptr);

  if ((*tree)->isRestricted()) {
    co_yield folly::coro::co_error(PathError(EACCES, path));
  }

  std::vector<PathComponent> names;
  std::vector<folly::coro::Task<EntryAttributes>> tasks;
  names.reserve((*tree)->size());
  tasks.reserve((*tree)->size());

  for (auto& child :
       visibleEntries(**tree, objectStore->getRestrictedContentMode())) {
    auto subPath = path + child.first;
    names.push_back(child.first);
    const auto& treeEntry = child.second;
    if (treeEntry.isTree()) {
      if (treeEntry.isRestricted()) {
        // Restricted child: synthesize a placeholder; never fetch its contents.
        tasks.emplace_back(coFetchEntryAttributesFromVI(
            VirtualInode::makeRestricted(
                treeEntry, (*tree)->getCaseSensitivity()),
            isUnderAcl(),
            requestedAttributes,
            std::move(subPath),
            objectStore,
            lastCheckoutTime,
            fetchContext.copy()));
      } else {
        tasks.emplace_back(coFetchTreeEntryAttributes(
            treeEntry.getObjectId(),
            modeFromTreeEntryType(treeEntry.getType()),
            treeEntry.hasACL(),
            isUnderAcl(),
            requestedAttributes,
            std::move(subPath),
            objectStore,
            lastCheckoutTime,
            fetchContext.copy()));
      }
    } else {
      tasks.emplace_back(coFetchEntryAttributesFromVI(
          VirtualInode{treeEntry},
          isUnderAcl(),
          requestedAttributes,
          std::move(subPath),
          objectStore,
          lastCheckoutTime,
          fetchContext.copy()));
    }
  }

  auto tries = co_await folly::coro::collectAllTryRange(std::move(tasks));

  std::vector<std::pair<PathComponent, folly::Try<EntryAttributes>>> result;
  result.reserve(tries.size());
  XCHECK_EQ(tries.size(), names.size())
      << "Missing/too many attributes for the names.";
  for (size_t i = 0; i < tries.size(); ++i) {
    result.emplace_back(std::move(names.at(i)), std::move(tries[i]));
  }
  co_return result;
}

namespace {

/**
 * Coroutine helper for getOrFindChild when the current node is a Tree.
 */
folly::coro::now_task<VirtualInode> co_getOrFindChildHelper(
    TreePtr tree,
    PathComponentPiece childName,
    RelativePathPiece path,
    const std::shared_ptr<ObjectStore>& objectStore,
    const ObjectFetchContextPtr& fetchContext) {
  if (tree->isRestricted()) {
    co_yield folly::coro::co_error(
        std::system_error(EACCES, std::generic_category()));
  }
  // Lookup the next child
  const auto it = tree->find(childName);
  if (it == tree->cend()) {
    // Note that the path printed below is the requested path that is being
    // walked, childName may appear anywhere in the path.
    XLOGF(
        DBG7,
        "attempted to find non-existent TreeEntry \"{}\" in {}",
        childName,
        path);
    co_yield folly::coro::co_error(
        std::system_error(ENOENT, std::generic_category()));
  }
  // Always descend if the treeEntry is a Tree
  const auto* treeEntry = &it->second;
  if (treeEntry->isTree()) {
    if (treeEntry->isRestricted()) {
      co_return VirtualInode::makeRestricted(
          *treeEntry, tree->getCaseSensitivity());
    }
    auto treeResult = co_await objectStore->co_getTree(
        treeEntry->getObjectId(), fetchContext);
    auto mode = modeFromTreeEntryType(treeEntry->getType());
    auto virtualInode = VirtualInode{std::move(treeResult), mode};
    virtualInode.setHasACL(treeEntry->hasACL());
    co_return virtualInode;
  } else {
    // This is a file, return the TreeEntry for it
    co_return VirtualInode{*treeEntry};
  }
}

} // namespace

folly::coro::now_task<VirtualInode> VirtualInode::co_getOrFindChild(
    PathComponentPiece childName,
    RelativePathPiece path,
    const std::shared_ptr<ObjectStore>& objectStore,
    const ObjectFetchContextPtr& fetchContext) const {
  if (!isDirectory()) {
    co_yield folly::coro::co_error(PathError(ENOTDIR, path));
  }

  // Use std::get_if instead of match to avoid potential issues with
  // coroutine lambdas and std::visit
  if (auto* inode = std::get_if<InodePtr>(&variant_)) {
    auto child = co_await inode->asTreePtr()->co_getOrFindChild(
        childName, fetchContext, false);
    co_return applyAncestorAcl(std::move(child), isUnderAcl());
  } else if (auto* tree = std::get_if<TreePtr>(&variant_)) {
    auto child = co_await co_getOrFindChildHelper(
        *tree, childName, path, objectStore, fetchContext);
    co_return applyAncestorAcl(std::move(child), isUnderAcl());
  } else {
    // These represent files in VirtualInode, and can't be descended
    co_yield folly::coro::co_error(PathError(
        ENOTDIR, path, std::string_view{"variant is of unhandled type"}));
  }
}

folly::coro::now_task<std::string> VirtualInode::co_getBlob(
    const std::shared_ptr<ObjectStore>& objectStore,
    const ObjectFetchContextPtr& fetchContext) const {
  // std::get_if is used instead of match because coroutine lambda captures are
  // stored in the lambda object, not the coroutine frame. If the coroutine
  // suspends, match destroys the lambda temporaries, and resuming accesses
  // dangling captures.
  static_assert(
      std::variant_size_v<detail::VariantVirtualInode> == 4,
      "New variant type added to VariantVirtualInode - update co_getBlob");
  if (auto* inode = std::get_if<InodePtr>(&variant_)) {
    auto content = co_await inode->asFilePtr()->co_readAll(fetchContext);
    co_return std::move(content);
  } else if (
      auto* entry =
          std::get_if<UnmaterializedUnloadedBlobDirEntry>(&variant_)) {
    auto blob =
        co_await objectStore->co_getBlob(entry->getObjectId(), fetchContext);
    co_return blob->asString();
  } else if (auto* treeEntry = std::get_if<TreeEntry>(&variant_)) {
    auto blob = co_await objectStore->co_getBlob(
        treeEntry->getObjectId(), fetchContext);
    co_return blob->asString();
  } else {
    // TreePtr - directories cannot be read as blobs
    co_yield folly::coro::co_error(
        std::system_error(EISDIR, std::generic_category()));
  }
}

} // namespace facebook::eden
