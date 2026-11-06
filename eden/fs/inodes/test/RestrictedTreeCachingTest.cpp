/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include <folly/coro/GtestHelpers.h>
#include <gtest/gtest.h>

#include "eden/common/utils/DirType.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/Overlay.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/model/Tree.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/store/ObjectStore.h"
#include "eden/fs/testharness/FakeBackingStore.h"
#include "eden/fs/testharness/FakeTreeBuilder.h"
#include "eden/fs/testharness/TestMount.h"

using namespace facebook::eden;
using namespace std::chrono_literals;

namespace {

/**
 * Test fixture for restricted-tree caching and TTL-based permission rechecks.
 *
 * Tests that need TTL=0 use initMountWithTtl(builder, 0) which sets the
 * config directly on the shared EdenConfig before any ReloadableConfig
 * snapshots are created.
 */
class RestrictedTreeCachingTest : public ::testing::Test {
 protected:
  void initMount(FakeTreeBuilder& builder) {
    testMount_ = std::make_unique<TestMount>(builder);
  }

  void initMountWithTtl(FakeTreeBuilder& builder, uint64_t ttlSeconds) {
    testMount_ = std::make_unique<TestMount>(builder);
    testMount_->updateEdenConfig(
        {{"acl:restricted-tree-ttl-seconds", std::to_string(ttlSeconds)}});
  }

  /**
   * Get the ObjectId of the "restricted" tree entry from the root tree.
   * This is the same ObjectId used as treeId in the restricted TreeInode,
   * and is what gets passed to checkPermission during TTL rechecks.
   */
  ObjectId getRestrictedTreeObjectId(FakeTreeBuilder& builder) {
    auto* rootTree = builder.getRoot();
    auto it = rootTree->get().find("restricted"_pc);
    EXPECT_NE(it, rootTree->get().cend());
    return it->second.getObjectId();
  }

  const ObjectStore& getObjectStore() {
    return *testMount_->getEdenMount()->getObjectStore();
  }

  /**
   * Return a time point guaranteed to be past the configured TTL, so
   * checkPermissionIfExpired will call through to the backing store.
   */
  std::chrono::steady_clock::time_point expiredLastCheck() {
    auto ttl = std::chrono::seconds{testMount_->getEdenMount()
                                        ->getEdenConfig()
                                        ->restrictedTreeTtlSeconds.getValue()};
    return std::chrono::steady_clock::now() - ttl - 1s;
  }

  std::unique_ptr<TestMount> testMount_;
};

// --- TTL + permission recheck tests ---

TEST_F(RestrictedTreeCachingTest, ttlExpired_permissionDenied_staysRestricted) {
  FakeTreeBuilder builder;
  builder.setFile("restricted/secret.txt", "secret content");
  builder.setDirIsRestricted("restricted");
  initMount(builder);

  auto restrictedObjectId = getRestrictedTreeObjectId(builder);
  auto* backingStore = testMount_->getBackingStore().get();
  backingStore->setCheckPermissionResult(restrictedObjectId, false);

  // Verify checkPermissionIfExpired with expired TTL calls through to
  // backing store's checkPermission
  auto result =
      getObjectStore()
          .checkPermissionIfExpired(restrictedObjectId, expiredLastCheck())
          .get();

  EXPECT_FALSE(result);
  EXPECT_EQ(backingStore->getCheckPermissionCount(restrictedObjectId), 1);
}

TEST_F(RestrictedTreeCachingTest, ttlExpired_permissionGranted_returnsTrue) {
  FakeTreeBuilder builder;
  builder.setFile("restricted/secret.txt", "secret content");
  builder.setDirIsRestricted("restricted");
  initMount(builder);

  auto restrictedObjectId = getRestrictedTreeObjectId(builder);
  auto* backingStore = testMount_->getBackingStore().get();
  backingStore->setCheckPermissionResult(restrictedObjectId, true);

  // TTL expired → should call checkPermission → returns true
  auto result =
      getObjectStore()
          .checkPermissionIfExpired(restrictedObjectId, expiredLastCheck())
          .get();

  EXPECT_TRUE(result);
  EXPECT_EQ(backingStore->getCheckPermissionCount(restrictedObjectId), 1);
}

TEST_F(RestrictedTreeCachingTest, ttlNotExpired_noRecheck) {
  FakeTreeBuilder builder;
  builder.setFile("restricted/secret.txt", "secret content");
  builder.setDirIsRestricted("restricted");
  initMount(builder);

  auto restrictedObjectId = getRestrictedTreeObjectId(builder);
  auto* backingStore = testMount_->getBackingStore().get();
  backingStore->setCheckPermissionResult(restrictedObjectId, true);

  // TTL NOT expired (lastCheck=now()) → should NOT call checkPermission.
  // Default TTL is 65 seconds, so now()-now() < 65s → returns false
  // without calling the backing store.
  auto result = getObjectStore()
                    .checkPermissionIfExpired(
                        restrictedObjectId, std::chrono::steady_clock::now())
                    .get();

  EXPECT_FALSE(result);
  EXPECT_EQ(backingStore->getCheckPermissionCount(restrictedObjectId), 0);
}

// --- Stat behavior tests ---

TEST_F(
    RestrictedTreeCachingTest,
    restrictedInode_deniedStatRecheckIsThrottled) {
  FakeTreeBuilder builder;
  builder.setFile("restricted/secret.txt", "secret content");
  builder.setDirIsRestricted("restricted");
  initMount(builder);

  auto restrictedObjectId = getRestrictedTreeObjectId(builder);
  auto* backingStore = testMount_->getBackingStore().get();
  backingStore->setCheckPermissionResult(restrictedObjectId, false);

  auto restrictedInode = testMount_->getTreeInode("restricted"_relpath);
  ASSERT_TRUE(restrictedInode->isRestricted());

  // stat on restricted inode returns S_IFDIR with zero permission bits.
  // The first access rechecks permission. A denied result keeps the inode
  // restricted and throttles subsequent rechecks for the configured TTL.
  auto context = ObjectFetchContext::getNullContext();
  auto st = restrictedInode->stat(context).get();

#ifndef _WIN32
  EXPECT_TRUE(S_ISDIR(st.st_mode));
  EXPECT_EQ(st.st_mode & 07777, 0);
#endif

  EXPECT_TRUE(restrictedInode->isRestricted());
  EXPECT_EQ(backingStore->getCheckPermissionCount(restrictedObjectId), 1);

  backingStore->setCheckPermissionResult(restrictedObjectId, true);
  auto st2 = restrictedInode->stat(context).get();
#ifndef _WIN32
  EXPECT_TRUE(S_ISDIR(st2.st_mode));
  EXPECT_EQ(st2.st_mode & 07777, 0);
#endif

  EXPECT_TRUE(restrictedInode->isRestricted());
  EXPECT_EQ(backingStore->getCheckPermissionCount(restrictedObjectId), 1);
}

CO_TEST_F(
    RestrictedTreeCachingTest,
    restrictedInode_firstStatTransitionsToUnrestricted) {
  FakeTreeBuilder builder;
  builder.setFile("restricted/secret.txt", "secret content");
  builder.setDirIsRestricted("restricted");
  initMount(builder);

  auto restrictedObjectId = getRestrictedTreeObjectId(builder);
  auto* backingStore = testMount_->getBackingStore().get();
  backingStore->setCheckPermissionResult(restrictedObjectId, true);

  auto restrictedInode = testMount_->getTreeInode("restricted"_relpath);
  CO_ASSERT_TRUE(restrictedInode->isRestricted());

  auto context = ObjectFetchContext::getNullContext();
  auto st = co_await restrictedInode->co_stat(context);
#ifndef _WIN32
  EXPECT_TRUE(S_ISDIR(st.st_mode));
  EXPECT_NE(st.st_mode & 07777, 0);
#endif

  EXPECT_FALSE(restrictedInode->isRestricted());
  EXPECT_EQ(backingStore->getCheckPermissionCount(restrictedObjectId), 1);

  auto children = co_await restrictedInode->getChildren(context, false);
  CO_ASSERT_EQ(children.size(), 1u);
  EXPECT_EQ(children[0].first, "secret.txt"_pc);
  EXPECT_TRUE(children[0].second.hasValue());
}

TEST_F(
    RestrictedTreeCachingTest,
    restrictedInode_freshDeniedTreeFetchIsThrottled) {
  testMount_ = std::make_unique<TestMount>();
  auto backingStore = testMount_->getBackingStore();

  auto [secretBlob, secretBlobId] = backingStore->putBlob("secret content");
  secretBlob->setReady();

  auto* restrictedTree = backingStore->putRestrictedTree({
      {"secret.txt", secretBlobId},
  });
  restrictedTree->setReady();
  auto restrictedTreeId = restrictedTree->get().getObjectId();

  Tree::container rootEntries{kPathMapDefaultCaseSensitive};
  rootEntries.emplace(
      "restricted"_pc,
      ObjectId{restrictedTreeId},
      TreeEntryType::TREE,
      /*isRestricted=*/false,
      /*hasACL=*/std::nullopt);
  auto* rootTree = backingStore->putTree(std::move(rootEntries));
  rootTree->setReady();
  backingStore->putCommit(RootId{"1"}, rootTree)->setReady();
  testMount_->initialize(RootId{"1"});

  backingStore->setCheckPermissionResult(restrictedTreeId, true);

  auto restrictedInode = testMount_->getTreeInode("restricted"_relpath);
  ASSERT_TRUE(restrictedInode->isRestricted());
  EXPECT_EQ(backingStore->getAccessCount(restrictedTreeId), 1);
  EXPECT_EQ(backingStore->getCheckPermissionCount(restrictedTreeId), 0);

  auto context = ObjectFetchContext::getNullContext();
  auto st = restrictedInode->stat(context).get();
#ifndef _WIN32
  EXPECT_TRUE(S_ISDIR(st.st_mode));
  EXPECT_EQ(st.st_mode & 07777, 0);
#endif

  EXPECT_TRUE(restrictedInode->isRestricted());
  EXPECT_EQ(backingStore->getCheckPermissionCount(restrictedTreeId), 0);
}

CO_TEST_F(
    RestrictedTreeCachingTest,
    restrictedInode_statTransitionsToUnrestricted) {
  FakeTreeBuilder builder;
  builder.setFile("restricted/secret.txt", "secret content");
  builder.setDirIsRestricted("restricted");
  initMountWithTtl(builder, 0);

  auto restrictedObjectId = getRestrictedTreeObjectId(builder);
  auto* backingStore = testMount_->getBackingStore().get();

  // First: checkPermission returns false → stays restricted
  backingStore->setCheckPermissionResult(restrictedObjectId, false);
  auto restrictedInode = testMount_->getTreeInode("restricted"_relpath);
  CO_ASSERT_TRUE(restrictedInode->isRestricted());

  auto context = ObjectFetchContext::getNullContext();
  auto st1 = co_await restrictedInode->co_stat(context);
#ifndef _WIN32
  EXPECT_TRUE(S_ISDIR(st1.st_mode));
  EXPECT_EQ(st1.st_mode & 07777, 0);
#endif
  EXPECT_TRUE(restrictedInode->isRestricted());
  EXPECT_EQ(backingStore->getCheckPermissionCount(restrictedObjectId), 1);

  // Second: checkPermission returns true → transitions to unrestricted
  backingStore->setCheckPermissionResult(restrictedObjectId, true);
  auto st2 = co_await restrictedInode->co_stat(context);
#ifndef _WIN32
  EXPECT_TRUE(S_ISDIR(st2.st_mode));
  EXPECT_NE(st2.st_mode & 07777, 0);
#endif
  EXPECT_FALSE(restrictedInode->isRestricted());
  EXPECT_EQ(backingStore->getCheckPermissionCount(restrictedObjectId), 2);

  // Can now read children through the unrestricted directory
  auto children = co_await restrictedInode->getChildren(context, false);
  CO_ASSERT_EQ(children.size(), 1u);
  EXPECT_EQ(children[0].first, "secret.txt"_pc);
  EXPECT_TRUE(children[0].second.hasValue());
}

TEST_F(
    RestrictedTreeCachingTest,
    restrictedInode_statTransitionUpdatesParentOverlay) {
  FakeTreeBuilder builder;
  builder.setFile("restricted/secret.txt", "secret content");
  builder.setDirIsRestricted("restricted");
  initMountWithTtl(builder, 0);

  auto restrictedObjectId = getRestrictedTreeObjectId(builder);
  auto* backingStore = testMount_->getBackingStore().get();
  backingStore->setCheckPermissionResult(restrictedObjectId, true);

  auto restrictedInode = testMount_->getTreeInode("restricted"_relpath);
  ASSERT_TRUE(restrictedInode->isRestricted());

  auto context = ObjectFetchContext::getNullContext();
  restrictedInode->stat(context).get();
  ASSERT_FALSE(restrictedInode->isRestricted());

  auto rootInode = testMount_->getEdenMount()->getRootInode();
  auto rootOverlay = testMount_->getEdenMount()->getOverlay()->loadOverlayDir(
      rootInode->getNodeId());
  auto it = rootOverlay.find("restricted"_pc);
  ASSERT_NE(it, rootOverlay.end());

  EXPECT_FALSE(it->second.isRestricted());
}

TEST_F(
    RestrictedTreeCachingTest,
    restrictedInode_statTransitionRefreshesStaleChildOverlay) {
  FakeTreeBuilder builder;
  builder.setFile("restricted/child/file.txt", "secret content");
  builder.setDirIsRestricted("restricted");
  builder.setDirHasAcl("restricted/child");
  initMountWithTtl(builder, 0);

  auto restrictedObjectId = getRestrictedTreeObjectId(builder);
  auto* backingStore = testMount_->getBackingStore().get();
  backingStore->setCheckPermissionResult(restrictedObjectId, true);

  auto restrictedInode = testMount_->getTreeInode("restricted"_relpath);
  ASSERT_TRUE(restrictedInode->isRestricted());

  auto* overlay = testMount_->getEdenMount()->getOverlay();
  const auto& restrictedTree =
      backingStore->getStoredTree(restrictedObjectId)->get();
  auto childIt = restrictedTree.find("child"_pc);
  ASSERT_NE(childIt, restrictedTree.cend());
  ASSERT_FALSE(childIt->second.isRestricted());
  ASSERT_EQ(std::optional<bool>{true}, childIt->second.hasACL());

  auto staleChildInode = overlay->allocateInodeNumber();
  DirContents staleContents{kPathMapDefaultCaseSensitive};
  staleContents.emplace(
      "child"_pc,
      DirEntry{
          dtype_to_mode(
              mode_to_dtype(modeFromTreeEntryType(childIt->second.getType()))),
          staleChildInode,
          childIt->second.getObjectId(),
          /*isRestricted=*/true,
          /*hasACL=*/true});
  overlay->saveOverlayDir(
      restrictedInode->getNodeId(), staleContents, /*isMaterialized=*/false);

  auto context = ObjectFetchContext::getNullContext();
  restrictedInode->stat(context).get();
  ASSERT_FALSE(restrictedInode->isRestricted());

  {
    auto contents = restrictedInode->lockContentsRead();
    auto it = contents->entries.find("child"_pc);
    ASSERT_NE(it, contents->entries.end());
    EXPECT_EQ(staleChildInode, it->second.getInodeNumber());
    EXPECT_FALSE(it->second.isRestricted());
    EXPECT_EQ(std::optional<bool>{true}, it->second.hasACL());
    EXPECT_FALSE(it->second.isMaterialized());
  }

  auto healedOverlay = overlay->loadOverlayDir(restrictedInode->getNodeId());
  auto healedIt = healedOverlay.find("child"_pc);
  ASSERT_NE(healedIt, healedOverlay.end());
  EXPECT_EQ(staleChildInode, healedIt->second.getInodeNumber());
  EXPECT_FALSE(healedIt->second.isRestricted());
  EXPECT_EQ(std::optional<bool>{true}, healedIt->second.hasACL());

  EXPECT_EQ(
      "secret content", testMount_->readFile("restricted/child/file.txt"));
}

TEST_F(
    RestrictedTreeCachingTest,
    restrictedInode_statTransitionLeavesMismatchedOverlayRestricted) {
  FakeTreeBuilder builder;
  builder.setFile("restricted/child/file.txt", "secret content");
  builder.setDirIsRestricted("restricted");
  initMountWithTtl(builder, 0);

  auto restrictedObjectId = getRestrictedTreeObjectId(builder);
  auto* backingStore = testMount_->getBackingStore().get();
  backingStore->setCheckPermissionResult(restrictedObjectId, true);

  auto restrictedInode = testMount_->getTreeInode("restricted"_relpath);
  ASSERT_TRUE(restrictedInode->isRestricted());

  auto* overlay = testMount_->getEdenMount()->getOverlay();
  auto staleChildInode = overlay->allocateInodeNumber();
  DirContents staleContents{kPathMapDefaultCaseSensitive};
  staleContents.emplace(
      "different"_pc,
      DirEntry{
          S_IFDIR | 0755,
          staleChildInode,
          ObjectId{"different-child"},
          /*isRestricted=*/true,
          /*hasACL=*/true});
  overlay->saveOverlayDir(
      restrictedInode->getNodeId(), staleContents, /*isMaterialized=*/false);

  auto context = ObjectFetchContext::getNullContext();
  restrictedInode->stat(context).get();
  ASSERT_FALSE(restrictedInode->isRestricted());

  {
    auto contents = restrictedInode->lockContentsRead();
    auto it = contents->entries.find("different"_pc);
    ASSERT_NE(it, contents->entries.end());
    EXPECT_EQ(staleChildInode, it->second.getInodeNumber());
    EXPECT_TRUE(it->second.isRestricted());
  }

  auto loadedOverlay = overlay->loadOverlayDir(restrictedInode->getNodeId());
  auto loadedIt = loadedOverlay.find("different"_pc);
  ASSERT_NE(loadedIt, loadedOverlay.end());
  EXPECT_TRUE(loadedIt->second.isRestricted());
}

TEST_F(
    RestrictedTreeCachingTest,
    unrestrictedParentLoad_refreshesNewlyRestrictedChildEntry) {
  FakeTreeBuilder builder;
  builder.setFile("parent/child/file.txt", "secret content");
  builder.setDirIsRestricted("parent/child");
  initMount(builder);

  auto* overlay = testMount_->getEdenMount()->getOverlay();
  auto rootInode = testMount_->getEdenMount()->getRootInode();
  auto parentIno = [&] {
    auto contents = rootInode->lockContentsRead();
    auto it = contents->entries.find("parent"_pc);
    EXPECT_NE(it, contents->entries.end());
    return it->second.getInodeNumber();
  }();

  const auto& parentTree = builder.getStoredTree("parent"_relpath)->get();
  auto childIt = parentTree.find("child"_pc);
  ASSERT_NE(childIt, parentTree.cend());
  ASSERT_TRUE(childIt->second.isRestricted());
  const auto childObjectId = childIt->second.getObjectId();

  // Overlay written while the child was still readable.
  auto staleChildInode = overlay->allocateInodeNumber();
  DirContents staleContents{kPathMapDefaultCaseSensitive};
  staleContents.emplace(
      "child"_pc,
      DirEntry{
          dtype_to_mode(
              mode_to_dtype(modeFromTreeEntryType(childIt->second.getType()))),
          staleChildInode,
          childObjectId,
          /*isRestricted=*/false,
          /*hasACL=*/std::nullopt});
  overlay->saveOverlayDir(parentIno, staleContents, /*isMaterialized=*/false);

  auto parentInode = testMount_->getTreeInode("parent"_relpath);
  {
    auto contents = parentInode->lockContentsRead();
    auto it = contents->entries.find("child"_pc);
    ASSERT_NE(it, contents->entries.end());
    EXPECT_EQ(staleChildInode, it->second.getInodeNumber());
    EXPECT_TRUE(it->second.isRestricted());
    EXPECT_FALSE(it->second.isMaterialized());
  }

  // A newly discovered denial is not written back; the overlay record is
  // left alone.
  auto persistedOverlay = overlay->loadOverlayDir(parentIno);
  auto persistedIt = persistedOverlay.find("child"_pc);
  ASSERT_NE(persistedIt, persistedOverlay.end());
  EXPECT_EQ(staleChildInode, persistedIt->second.getInodeNumber());
  EXPECT_FALSE(persistedIt->second.isRestricted());

  // The parent tree alone carries the restriction; no child fetch is needed.
  EXPECT_EQ(0, testMount_->getBackingStore()->getAccessCount(childObjectId));
  EXPECT_TRUE(testMount_->getTreeInode("parent/child"_relpath)->isRestricted());
}

// --- Checkout tests ---

TEST_F(
    RestrictedTreeCachingTest,
    checkout_restrictedToUnrestricted_differentContent) {
  // Create commit 1: "foo" is restricted with one file
  FakeTreeBuilder builder1;
  builder1.setFile("foo/file.txt", "version1");
  builder1.setDirIsRestricted("foo");
  initMount(builder1);

  // Verify "foo" is restricted in commit 1 via parent's DirEntry
  {
    auto rootInode = testMount_->getEdenMount()->getRootInode();
    auto contents = rootInode->lockContentsRead();
    auto it = contents->entries.find("foo"_pc);
    ASSERT_NE(it, contents->entries.end());
    ASSERT_TRUE(it->second.isRestricted());
  }

  // Create commit 2: different content in "foo", NOT restricted.
  FakeTreeBuilder builder2;
  builder2.setFile("foo/file.txt", "version2");
  builder2.finalize(testMount_->getBackingStore(), true);
  auto commit2 =
      testMount_->getBackingStore()->putCommit(RootId{"2"}, builder2);
  commit2->setReady();

  // Perform checkout
  testMount_->drainServerExecutor();
  auto checkoutFuture = testMount_->checkout(
      RootId{"2"}, ObjectFetchContext::getNullContext(), __func__);
  testMount_->drainServerExecutor();
  ASSERT_TRUE(checkoutFuture.isReady());
  auto checkoutResult = std::move(checkoutFuture).get();

  EXPECT_EQ(0, checkoutResult.conflicts.size());

  // After checkout, "foo" should be accessible (not restricted)
  auto fooInodeAfter = testMount_->getTreeInode("foo"_relpath);
  EXPECT_FALSE(fooInodeAfter->isRestricted());

  // Can read file contents through the unrestricted directory
  try {
    auto content = testMount_->readFile("foo/file.txt");
    EXPECT_EQ("version2", content);
  } catch (const std::exception& ex) {
    FAIL() << "readFile threw: " << ex.what();
  }
}

} // namespace
