/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/testharness/FakeTreeBuilder.h"

#include <stdexcept>
#include "eden/fs/model/Blob.h"
#include "eden/fs/model/Tree.h"
#include "eden/fs/model/TreeEntry.h"
#include "eden/fs/testharness/FakeBackingStore.h"
#include "eden/fs/testharness/StoredObject.h"

using std::make_unique;
using std::string;

namespace facebook::eden {

FakeTreeBuilder::FakeTreeBuilder() = default;

/**
 * Create a clone of this FakeTreeBuilder.
 *
 * The clone has the same path data (stored in root_) but is not finalized,
 * regardless of whether the original FakeTreeBuilder was finalized or not.
 */
FakeTreeBuilder::FakeTreeBuilder(ExplicitClone, const FakeTreeBuilder* orig)
    : root_{CLONE, orig->root_} {}

FakeTreeBuilder FakeTreeBuilder::clone() const {
  return FakeTreeBuilder{CLONE, this};
}

void FakeTreeBuilder::setFiles(
    const std::initializer_list<FileInfo>& fileArgs) {
  for (const auto& arg : fileArgs) {
    setFileImpl(
        arg.path,
        folly::ByteRange{folly::StringPiece{arg.contents}},
        false,
        arg.executable ? TreeEntryType::EXECUTABLE_FILE
                       : TreeEntryType::REGULAR_FILE);
  }
}

void FakeTreeBuilder::mkdir(RelativePathPiece path) {
  // Use getDirEntry() to create a directory at this location if one
  // does not already exist.
  getDirEntry(path, true);
}

void FakeTreeBuilder::setMtime(RelativePathPiece path, timespec mtime) {
  XCHECK(!finalizedRoot_);
  auto* entry = getEntry(path);
  XCHECK(!entry->objectId.has_value());
  entry->mtime = mtime;
}

void FakeTreeBuilder::setDirIsRestricted(RelativePathPiece path) {
  XCHECK(!finalizedRoot_);
  auto* entry = getEntry(path);
  XCHECK(entry->type == TreeEntryType::TREE)
      << "setDirIsRestricted can only be called on directories";
  entry->isRestricted = true;
  entry->hasACL = true;
}

void FakeTreeBuilder::setDirHasAcl(RelativePathPiece path) {
  auto* entry = getEntry(path);
  XCHECK(entry->type == TreeEntryType::TREE)
      << "setDirHasAcl can only be called on directories";
  entry->hasACL = true;
}

void FakeTreeBuilder::setDirHasAclUnknown(RelativePathPiece path) {
  auto* entry = getEntry(path);
  XCHECK(entry->type == TreeEntryType::TREE)
      << "setDirHasAclUnknown can only be called on directories";
  entry->hasACL = std::nullopt;
}

void FakeTreeBuilder::setFileImpl(
    RelativePathPiece path,
    folly::ByteRange contents,
    bool replace,
    TreeEntryType type,
    std::optional<ObjectId> objectId) {
  XCHECK(!finalizedRoot_);

  auto dir = getDirEntry(path.dirname(), true);
  auto name = path.basename();

  auto info = EntryInfo{type};
  info.contents = folly::StringPiece{contents}.str();
  info.objectId = objectId;

  if (replace) {
    auto iter = dir->entries->find(name);
    if (iter == dir->entries->end()) {
      throwf<std::runtime_error>(
          "while building fake tree: expected to replace entry at {} "
          "but no entry present with this name",
          path);
    }
    iter->second = std::move(info);
  } else {
    auto ret = dir->entries->emplace(name, std::move(info));
    if (!ret.second) {
      throwf<std::runtime_error>(
          "while building fake tree: an entry already exists at {}", path);
    }
  }
}

void FakeTreeBuilder::removeFile(
    RelativePathPiece path,
    bool removeEmptyParents) {
  XCHECK(!finalizedRoot_);

  auto parentPath = path.dirname();
  auto dir = getDirEntry(parentPath, false);
  auto name = path.basename();
  auto iter = dir->entries->find(name);
  if (iter == dir->entries->end()) {
    throwf<std::runtime_error>(
        "while building fake tree: expected to remove entry at {} "
        "but no entry present with this name",
        path);
  }
  dir->entries->erase(iter);

  if (removeEmptyParents && dir->entries->empty()) {
    removeFile(parentPath, true);
  }
}

void FakeTreeBuilder::setReady(RelativePathPiece path) {
  XCHECK(finalizedRoot_) << "call finalize before setReady";

  if (path.empty()) {
    finalizedRoot_->setReady();
    return;
  }

  auto* parent = getStoredTree(path.dirname());
  const auto& entry = parent->get().find(path.basename())->second;
  if (entry.isTree()) {
    store_->getStoredTree(entry.getObjectId())->setReady();
  } else {
    store_->getStoredBlob(entry.getObjectId())->setReady();
  }
}

void FakeTreeBuilder::setAllReady() {
  XCHECK(finalizedRoot_);
  setAllReadyUnderTree(finalizedRoot_);
}

void FakeTreeBuilder::setAllReadyUnderTree(RelativePathPiece path) {
  auto tree = getStoredTree(path);
  return setAllReadyUnderTree(tree);
}

void FakeTreeBuilder::setAllReadyUnderTree(StoredTree* tree) {
  tree->setReady();
  for (const auto& entry : tree->get()) {
    if (entry.second.isTree()) {
      auto* child = store_->getStoredTree(entry.second.getObjectId());
      setAllReadyUnderTree(child);
    } else {
      auto* child = store_->getStoredBlob(entry.second.getObjectId());
      child->setReady();
    }
  }
}

void FakeTreeBuilder::triggerError(
    RelativePathPiece path,
    folly::exception_wrapper ew) {
  XCHECK(finalizedRoot_);

  if (path.empty()) {
    finalizedRoot_->triggerError(std::move(ew));
    return;
  }

  auto* parent = getStoredTree(path.dirname());
  const auto& entry = parent->get().find(path.basename())->second;
  if (entry.isTree()) {
    store_->getStoredTree(entry.getObjectId())->triggerError(std::move(ew));
  } else {
    store_->getStoredBlob(entry.getObjectId())->triggerError(std::move(ew));
  }
}

StoredTree* FakeTreeBuilder::finalize(
    std::shared_ptr<FakeBackingStore> store,
    bool setReady) {
  XCHECK(!finalizedRoot_);
  XCHECK(!store_);
  store_ = std::move(store);

  finalizedRoot_ = root_.finalizeTree(this, setReady);
  return finalizedRoot_;
}

StoredTree* FakeTreeBuilder::getRoot() const {
  XCHECK(finalizedRoot_);
  return finalizedRoot_;
}

FakeTreeBuilder::EntryInfo* FakeTreeBuilder::getEntry(RelativePathPiece path) {
  if (path.empty()) {
    return &root_;
  }

  auto* parent = getDirEntry(path.dirname(), false);
  auto iter = parent->entries->find(path.basename());
  if (iter == parent->entries->end()) {
    throwf<std::runtime_error>("tried to look up non-existent entry {}", path);
  }
  return &iter->second;
}

FakeTreeBuilder::EntryInfo* FakeTreeBuilder::getDirEntry(
    RelativePathPiece path,
    bool create) {
  EntryInfo* parent = &root_;

  for (auto name : path.components()) {
    auto iter = parent->entries->find(name);
    if (iter == parent->entries->end()) {
      if (!create) {
        throwf<std::runtime_error>(
            "tried to look up non-existent directory ", path);
      }
      auto ret = parent->entries->emplace(name, EntryInfo{TreeEntryType::TREE});
      XCHECK(ret.second);
      parent = &ret.first->second;
    } else {
      parent = &iter->second;
      if (parent->type != TreeEntryType::TREE) {
        throwf<std::runtime_error>(
            "tried to look up directory {} but {} is not a directory",
            path,
            name);
      }
    }
  }

  return parent;
}

StoredTree* FakeTreeBuilder::getStoredTree(RelativePathPiece path) {
  XCHECK(finalizedRoot_);

  StoredTree* current = finalizedRoot_;
  for (auto name : path.components()) {
    const auto& entry = current->get().find(name)->second;
    if (!entry.isTree()) {
      throwf<std::runtime_error>(
          "tried to look up stored tree {} but {} is not a tree", path, name);
    }

    current = store_->getStoredTree(entry.getObjectId());
  }

  return current;
}

StoredBlob* FakeTreeBuilder::getStoredBlob(RelativePathPiece path) {
  auto* parent = getStoredTree(path.dirname());
  const auto& entry = parent->get().find(path.basename())->second;
  if (entry.isTree()) {
    throwf<std::runtime_error>(
        "tried to look up stored blob at {} but it is a tree rather than a blob",
        path);
  }
  return store_->getStoredBlob(entry.getObjectId());
}

FakeTreeBuilder::EntryInfo::EntryInfo(TreeEntryType fileType) : type(fileType) {
  if (type == TreeEntryType::TREE) {
    entries = make_unique<PathMap<EntryInfo>>(kPathMapDefaultCaseSensitive);
  }
}

FakeTreeBuilder::EntryInfo::EntryInfo(ExplicitClone, const EntryInfo& orig)
    : type(orig.type),
      contents(orig.contents),
      objectId(orig.objectId),
      mtime(orig.mtime),
      isRestricted(orig.isRestricted),
      hasACL(orig.hasACL) {
  if (orig.entries) {
    entries = make_unique<PathMap<EntryInfo>>(kPathMapDefaultCaseSensitive);
    for (const auto& e : *orig.entries) {
      auto ret = entries->emplace(e.first, EntryInfo{CLONE, e.second});
      XCHECK(ret.second, fmt::format("failed to insert {}", e.first));
    }
  }
}

StoredTree* FakeTreeBuilder::EntryInfo::finalizeTree(
    FakeTreeBuilder* builder,
    bool setReady) const {
  XCHECK(type == TreeEntryType::TREE);

  Tree::container treeEntries{kPathMapDefaultCaseSensitive};
  for (const auto& e : *entries) {
    const auto& entryInfo = e.second;
    ObjectId oid;
    if (entryInfo.type == TreeEntryType::TREE) {
      auto* storedTree = entryInfo.finalizeTree(builder, setReady);
      oid = storedTree->get().getObjectId();
    } else {
      auto [storedBlob, id] = entryInfo.finalizeBlob(builder, setReady);
      oid = id;
    }
    treeEntries.emplace(
        e.first,
        std::move(oid),
        entryInfo.type,
        std::nullopt,
        std::nullopt,
        std::nullopt,
        entryInfo.isRestricted,
        entryInfo.hasACL);
  }

  if (builder == nullptr || builder->store_ == nullptr) {
    throw std::logic_error(
        "FakeTreeBuilder::finalize requires a backing store");
  }
  auto* storedTree = mtime
      ? builder->store_->maybePutTree(treeEntries, hasACL, *mtime).first
      : builder->store_->maybePutTree(treeEntries, hasACL).first;
  if (storedTree == nullptr) {
    throw std::logic_error("FakeTreeBuilder::finalize failed to store tree");
  }
  if (setReady) {
    storedTree->setReady();
  }
  return storedTree;
}

std::pair<StoredBlob*, ObjectId> FakeTreeBuilder::EntryInfo::finalizeBlob(
    FakeTreeBuilder* builder,
    bool setReady) const {
  XCHECK(type != TreeEntryType::TREE);
  auto [storedBlob, id, inserted] = objectId
      ? builder->store_->maybePutBlob(objectId.value(), contents)
      : mtime ? builder->store_->maybePutBlob(contents, *mtime)
              : builder->store_->maybePutBlob(contents);
  if (setReady) {
    storedBlob->setReady();
  }
  return {storedBlob, id};
}
} // namespace facebook::eden
