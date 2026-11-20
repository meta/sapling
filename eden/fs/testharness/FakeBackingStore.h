/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/coro/Task.h>
#include <folly/coro/safe/NowTask.h>
#include <gtest/gtest_prod.h>
#include <ctime>
#include <initializer_list>
#include <memory>
#include <optional>
#include <stdexcept>
#include <string>
#include <unordered_map>

#include <folly/container/F14Map.h>
#include "eden/fs/model/Blob.h"
#include "eden/fs/model/Hash.h"
#include "eden/fs/model/Tree.h"
#include "eden/fs/model/TreeEntry.h"

#include "eden/fs/store/BackingStore.h"
#include "eden/fs/store/ImportPriority.h"
#include "eden/fs/store/WalkDetector.h"
#include "eden/fs/testharness/StoredObject.h"

namespace facebook::eden {

class FakeTreeBuilder;
class ServerState;

/**
 * The WalkDetector of a mount on a FakeBackingStore. Records what the mount
 * reports so tests can check it.
 */
class FakeWalkDetector final : public WalkDetector {
 public:
  void witnessFileRead(
      const ObjectId& id,
      ObjectFetchContext::Origin /*origin*/,
      OptionalProcessId /*pid*/,
      std::optional<ObjectFetchContext::DirEntryCounts> /*parentCounts*/)
      override {
    data_.wlock()->fileReads.push_back(id);
  }

  void witnessTreeRead(
      const ObjectId& id,
      const Tree& /*tree*/,
      ObjectFetchContext::Origin /*origin*/,
      OptionalProcessId /*pid*/) override {
    data_.wlock()->treeReads.push_back(id);
  }

  void witnessDirectorySize(
      RelativePathPiece path,
      ObjectFetchContext::DirEntryCounts counts) override {
    data_.wlock()->directorySizes.emplace_back(path.copy(), counts);
  }

  void workingCopyParentHint(const RootId& parent) override {
    data_.wlock()->parents.push_back(parent);
  }

  std::vector<ObjectId> fileReads() const {
    return data_.rlock()->fileReads;
  }

  std::vector<ObjectId> treeReads() const {
    return data_.rlock()->treeReads;
  }

  std::vector<std::pair<RelativePath, ObjectFetchContext::DirEntryCounts>>
  directorySizes() const {
    return data_.rlock()->directorySizes;
  }

  std::vector<RootId> parents() const {
    return data_.rlock()->parents;
  }

 private:
  struct Data {
    std::vector<ObjectId> fileReads;
    std::vector<ObjectId> treeReads;
    std::vector<std::pair<RelativePath, ObjectFetchContext::DirEntryCounts>>
        directorySizes;
    std::vector<RootId> parents;
  };
  folly::Synchronized<Data> data_;
};

/**
 * A BackingStore implementation for test code.
 */
class FakeBackingStore final : public BackingStore {
 public:
  struct TreeEntryData;

  explicit FakeBackingStore(
      std::shared_ptr<ServerState> serverState = nullptr,
      std::optional<std::string> blake3Key = std::nullopt);
  ~FakeBackingStore() override;

  /*
   * BackingStore APIs
   */

  ObjectComparison compareObjectsById(const ObjectId& one, const ObjectId& two)
      override {
    // FakeBackingStore does not provide any particular ID scheme requirements.
    // If IDs match, contents must, but not the other way around.
    if (one.bytesEqual(two)) {
      return ObjectComparison::Identical;
    }
    return ObjectComparison::Unknown;
  }

  ObjectComparison compareRootsById(const RootId& one, const RootId& two)
      override {
    return one == two ? ObjectComparison::Identical
                      : ObjectComparison::Different;
  }

  RootId parseRootId(folly::StringPiece rootId) override;
  std::string renderRootId(const RootId& rootId) override;
  ObjectId parseObjectId(folly::StringPiece objectId) override;
  std::string renderObjectId(const ObjectId& objectId) override;

  /**
   * Add a Blob to the backing store
   *
   * If an id is not explicitly given, one will be computed automatically.
   * (The test code may not use the same id scheme as a production
   * mercurial- or git-backed store, but it will be consistent for the
   * duration of the test.)
   */
  std::pair<StoredBlob*, ObjectId> putBlob(folly::StringPiece contents);
  StoredBlob* putBlob(ObjectId id, folly::StringPiece contents);

  /**
   * Add a blob to the backing store, or return the StoredBlob already present
   * with this id.
   *
   * The boolean in the return value is true if a new StoredBlob was created by
   * this call, or false if a StoredBlob already existed with this id.
   */
  std::tuple<StoredBlob*, ObjectId, bool> maybePutBlob(
      folly::StringPiece contents);
  std::tuple<StoredBlob*, ObjectId, bool> maybePutBlob(
      ObjectId id,
      folly::StringPiece contents);

  static Blob makeBlob(folly::StringPiece contents);

  /**
   * Helper functions for building a tree.
   *
   * Example usage:
   *
   *   store->putTree({
   *       {"test.txt", testBlob, 0644},
   *       {"runme.sh", runmeBlob, 0755},
   *       {"subdir", subdirTree, 0755},
   *   });
   */
  StoredTree* putTree(const std::initializer_list<TreeEntryData>& entries);
  StoredTree* putTree(
      ObjectId id,
      const std::initializer_list<TreeEntryData>& entries);
  StoredTree* putTree(Tree::container entries);
  StoredTree* putTree(ObjectId id, Tree::container entries);

  StoredTree* putRestrictedTree(
      const std::initializer_list<TreeEntryData>& entries);
  StoredTree* putRestrictedTree(
      ObjectId id,
      const std::initializer_list<TreeEntryData>& entries);

  /**
   * Replace an existing tree with an empty restricted response under the same
   * object ID, simulating access revocation between fetches.
   */
  StoredTree* replaceTreeWithRestricted(ObjectId id) {
    auto data = data_.wlock();
    auto it = data->trees.find(id);
    if (it == data->trees.end()) {
      throw std::domain_error{"tree not found"};
    }
    it->second = std::make_unique<StoredTree>(Tree{
        Tree::Restricted{},
        Tree::container{kPathMapDefaultCaseSensitive},
        std::move(id)});
    return it->second.get();
  }

  /**
   * Add a tree to the backing store, or return the StoredTree already present
   * with this id.
   *
   * The boolean in the return value is true if a new StoredTree was created by
   * this call, or false if a StoredTree already existed with this id.
   */
  std::pair<StoredTree*, bool> maybePutTree(
      const std::initializer_list<TreeEntryData>& entries);
  std::pair<StoredTree*, bool> maybePutTree(
      Tree::container entries,
      std::optional<bool> hasACL = std::nullopt);

  /**
   * Add the blob of a file, or the tree of a directory, whose modification
   * time the store records. The id carries the time (see
   * EmbeddedMetadata.h), so the same contents with another time are another
   * object.
   */
  std::tuple<StoredBlob*, ObjectId, bool> maybePutBlob(
      folly::StringPiece contents,
      timespec mtime);
  std::pair<StoredTree*, bool> maybePutTree(
      Tree::container entries,
      std::optional<bool> hasACL,
      timespec mtime);

  /**
   * Add a mapping from a commit ID to a root tree id.
   */
  StoredId* putCommit(const RootId& commitId, const StoredTree* tree);
  StoredId* putCommit(const RootId& commitId, ObjectId treeId);
  StoredId* putCommit(const RootId& commitId, const FakeTreeBuilder& builder);
  StoredId* putCommit(
      folly::StringPiece commitStr,
      const FakeTreeBuilder& builder);

  /**
   * Look up a StoredTree.
   *
   * Throws an error if the specified id does not exist.  Never returns null.
   */
  StoredTree* getStoredTree(ObjectId id);

  /**
   * Look up a StoredBlob.
   *
   * Throws an error if the specified id does not exist.  Never returns null.
   */
  StoredBlob* getStoredBlob(ObjectId id);

  /**
   * Manually clear the list of outstanding requests to avoid cycles during
   * TestMount destruction.
   */
  void discardOutstandingRequests();

  /**
   * Returns the number of times this id has been queried by either getTree,
   * getBlob, or getTreeForCommit.
   */
  size_t getAccessCount(const ObjectId& id) const;

  // TODO(T119221752): Implement for all BackingStore subclasses
  int64_t dropAllPendingRequestsFromQueue() override {
    XLOG(
        WARN,
        "dropAllPendingRequestsFromQueue() is not implemented for FakeBackingStore");
    return 0;
  }

  void setRepoName(std::string name) {
    repoName_ = std::move(name);
  }

  std::optional<folly::StringPiece> getRepoName() override {
    if (repoName_.has_value()) {
      return folly::StringPiece{repoName_.value()};
    }
    return std::nullopt;
  }

  std::vector<ObjectId> getAuxDataLookups() const {
    return data_.rlock()->auxDataLookups;
  }

  std::shared_ptr<WalkDetector> createWalkDetector(
      AbsolutePathPiece /*mountPath*/) override {
    return std::make_shared<FakeWalkDetector>();
  }

  /**
   * Configure the TreeAuxData returned for an object ID. Passing nullptr
   * simulates aux data that is absent from the backing store.
   */
  void putTreeAuxData(ObjectId id, TreeAuxDataPtr treeAuxData);

  /**
   * Configure the result of checkPermission for a specific manifest ID.
   * If not configured, checkPermission defaults to true (fail-open).
   */
  void setCheckPermissionResult(const ObjectId& id, bool allowed);

  /**
   * Get the number of times checkPermission was called for a specific ID.
   */
  size_t getCheckPermissionCount(const ObjectId& id) const;

  ImmediateFuture<bool> checkPermission(const ObjectId& manifestId) override;

 private:
  struct Data {
    std::unordered_map<RootId, std::unique_ptr<StoredId>> commits;
    std::unordered_map<ObjectId, std::unique_ptr<StoredTree>> trees;
    std::unordered_map<ObjectId, std::unique_ptr<StoredBlob>> blobs;

    std::unordered_map<RootId, size_t> commitAccessCounts;
    std::unordered_map<ObjectId, size_t> accessCounts;
    folly::F14FastMap<ObjectId, TreeAuxDataPtr> treeAuxData;
    std::vector<ObjectId> auxDataLookups;
    folly::F14FastMap<ObjectId, bool> permissionResults;
    folly::F14FastMap<ObjectId, size_t> permissionCheckCounts;
  };

  static Tree::container buildTreeEntries(
      const std::initializer_list<TreeEntryData>& entryArgs);
  static ObjectId computeTreeId(
      const Tree::container& sortedEntries,
      const std::optional<timespec>& mtime = std::nullopt);
  StoredTree* putTreeImpl(
      ObjectId id,
      Tree::container&& sortedEntries,
      bool isRestricted = false,
      std::optional<bool> hasACL = std::nullopt);
  std::pair<StoredTree*, bool> maybePutTreeImpl(
      ObjectId id,
      Tree::container&& sortedEntries,
      bool isRestricted = false,
      std::optional<bool> hasACL = std::nullopt);

  FRIEND_TEST(FakeBackingStoreTest, getNonExistent);
  FRIEND_TEST(FakeBackingStoreTest, getBlob);
  FRIEND_TEST(FakeBackingStoreTest, getTree);
  FRIEND_TEST(FakeBackingStoreTest, getRootTree);

  ImmediateFuture<GetRootTreeResult> getRootTree(
      const RootId& commitID,
      const ObjectFetchContextPtr& context);
  folly::coro::now_task<GetRootTreeResult> co_getRootTree(
      const RootId& rootId,
      const ObjectFetchContextPtr& context) override;
  ImmediateFuture<std::shared_ptr<TreeEntry>> getTreeEntryForObjectId(
      const ObjectId& /* commitID */,
      TreeEntryType /* treeEntryType */,
      const ObjectFetchContextPtr& /* context */) override;

  folly::SemiFuture<GetTreeResult> getTree(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) override;
  folly::coro::now_task<GetTreeResult> co_getTree(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) override;
  folly::coro::now_task<GetTreeAuxResult> co_getTreeAuxData(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) override;
  folly::SemiFuture<GetBlobResult> getBlob(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) override;
  folly::coro::Task<GetBlobResult> co_getBlob(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) override;
  folly::coro::now_task<GetBlobAuxResult> co_getBlobAuxData(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) override;

  std::shared_ptr<ServerState> serverState_;
  folly::Synchronized<Data> data_;
  std::optional<std::string> blake3Key_;
  std::optional<std::string> repoName_;
};

enum class FakeBlobType {
  REGULAR_FILE,
  EXECUTABLE_FILE,
  SYMLINK,
};

/**
 * A small helper struct for use with FakeBackingStore::putTree()
 *
 * This mainly exists to allow putTree() to be called conveniently with
 * initialier-list arguments.
 */
struct FakeBackingStore::TreeEntryData {
  // blob
  TreeEntryData(
      folly::StringPiece name,
      const ObjectId& id,
      FakeBlobType type = FakeBlobType::REGULAR_FILE);
  // blob
  TreeEntryData(
      folly::StringPiece name,
      const std::pair<StoredBlob*, ObjectId>& blob,
      FakeBlobType type = FakeBlobType::REGULAR_FILE);
  // tree
  TreeEntryData(folly::StringPiece name, const Tree& tree);
  // tree
  TreeEntryData(folly::StringPiece name, const StoredTree* tree);

  Tree::value_type entry;
};
} // namespace facebook::eden
