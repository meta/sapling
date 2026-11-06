/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/inodes/TreeInode.h"

#include <folly/ScopeGuard.h>
#include <folly/coro/GtestHelpers.h>
#include <gtest/gtest.h>
#include <algorithm>
#include <chrono>
#include <memory>
#include <system_error>

#include "eden/common/utils/CaseSensitivity.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/config/RestrictedContentMode.h"
#include "eden/fs/fuse/FuseDirList.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/FileInode.h"
#include "eden/fs/inodes/InodeMap.h"
#include "eden/fs/inodes/InodeMetadata.h"
#include "eden/fs/inodes/Overlay.h"
#include "eden/fs/inodes/VirtualInode.h"
#include "eden/fs/model/Tree.h"
#include "eden/fs/model/TreeAuxData.h"
#include "eden/fs/nfs/NfsDirList.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/store/ObjectStore.h"
#include "eden/fs/testharness/FakeBackingStore.h"
#include "eden/fs/testharness/FakeFuse.h"
#include "eden/fs/testharness/FakeTreeBuilder.h"
#include "eden/fs/testharness/TestMount.h"

using namespace facebook::eden;

namespace {

template <typename Fn>
void expectEacces(Fn&& fn) {
  try {
    fn();
    FAIL() << "Expected system_error with EACCES";
  } catch (const std::system_error& ex) {
    EXPECT_EQ(ex.code().value(), EACCES);
  }
}

// Helper to construct a restricted TreeInode and register it with the
// InodeMap. Uses TreeInodePtr::makeNew (handles 0→1 refcount transition)
// + InodeMap::inodeCreated (registers for inodePtrFromThis() lookups).
TreeInodePtr makeRestrictedInode(
    TestMount& testMount,
    PathComponentPiece name) {
  auto rootInode = testMount.getEdenMount()->getRootInode();
  auto ino = testMount.getEdenMount()->getOverlay()->allocateInodeNumber();
  auto inode = TreeInodePtr::makeNew(
      ino,
      rootInode,
      name,
      S_IFDIR | 0755,
      std::nullopt,
      DirContents{CaseSensitivity::Sensitive},
      std::nullopt,
      /*isRestricted=*/true);
  testMount.getEdenMount()->getInodeMap()->inodeCreated(inode);
  return inode;
}

TreeInodePtr makeTreeInodeChildWithoutParentEntry(
    TestMount& testMount,
    TreeInodePtr parent,
    PathComponentPiece name) {
  auto ino = testMount.getEdenMount()->getOverlay()->allocateInodeNumber();
  auto inode = TreeInodePtr::makeNew(
      ino,
      parent,
      name,
      S_IFDIR | 0755,
      std::nullopt,
      DirContents{CaseSensitivity::Sensitive},
      std::nullopt);
  testMount.getEdenMount()->getInodeMap()->inodeCreated(inode);
  return inode;
}

// The mount's ObjectStore snapshots acl:restricted-content-mode at
// construction, so it must be set on the EdenConfig before TestMount is built,
// not via updateEdenConfig().
void setOmittedMode(EdenConfig& config) {
  config.restrictedContentMode.setValue(
      RestrictedContentMode::Omitted, ConfigSourceType::CommandLine);
}

// Mount the given tree with acl:restricted-content-mode = omitted.
std::unique_ptr<TestMount> makeOmittedModeTestMount(FakeTreeBuilder& builder) {
  return std::make_unique<TestMount>(
      builder,
      /*startReady=*/true,
      /*enableActivityBuffer=*/true,
      kPathMapDefaultCaseSensitive,
      /*errorLogger=*/nullptr,
      setOmittedMode);
}

// Shared setup for the omittedMode_* tests: a parent directory with one
// normal child and one restricted child.
std::unique_ptr<TestMount> makeOmittedModeTestMount() {
  FakeTreeBuilder builder;
  builder.setFile("parent/normal.txt", "normal content");
  builder.setFile("parent/restricted_child/secret.txt", "secret content");
  builder.setDirIsRestricted("parent/restricted_child");
  return makeOmittedModeTestMount(builder);
}
} // namespace

CO_TEST(RestrictedTreeInode, normalTreeInodeAllowsReaddir) {
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "content");
  TestMount testMount{builder};

  auto rootInode = testMount.getEdenMount()->getRootInode();
  auto context = ObjectFetchContext::getNullContext();
  auto children =
      co_await rootInode->getChildren(context, /*loadInodes=*/false);

  auto iter =
      std::find_if(children.begin(), children.end(), [](const auto& entry) {
        return entry.first == "dir"_pc;
      });
  CO_ASSERT_NE(iter, children.end());
  CO_ASSERT_TRUE(iter->second.hasValue());
}

TEST(RestrictedTreeInode, restrictedFlagDeniesAccess) {
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "content");
  TestMount testMount{builder};

  auto restricted = makeRestrictedInode(testMount, "restricted"_pc);

  auto context = ObjectFetchContext::getNullContext();
  expectEacces(
      [&] { restricted->getOrFindChild("child"_pc, context, false).get(); });
}

TEST(RestrictedTreeInode, statReturnsZeroPermissions) {
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "content");
  TestMount testMount{builder};

  auto restricted = makeRestrictedInode(testMount, "restricted"_pc);

  auto context = ObjectFetchContext::getNullContext();
  auto st = restricted->stat(context).get();

  EXPECT_TRUE(S_ISDIR(st.st_mode));
  EXPECT_EQ(st.st_mode & 07777, 0);
}

TEST(RestrictedTreeInode, getOrLoadChildReturnsEACCES) {
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "content");
  TestMount testMount{builder};

  auto restricted = makeRestrictedInode(testMount, "restricted"_pc);

  auto context = ObjectFetchContext::getNullContext();
  expectEacces(
      [&] { restricted->getOrLoadChild("anything"_pc, context).get(); });
}

TEST(RestrictedTreeInode, mkdirReturnsEACCES) {
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "content");
  TestMount testMount{builder};

  auto restricted = makeRestrictedInode(testMount, "restricted"_pc);

  expectEacces([&] {
    restricted->mkdir("newdir"_pc, S_IFDIR | 0755, InvalidationRequired::No);
  });
}

TEST(RestrictedTreeInode, unlinkReturnsEACCES) {
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "content");
  TestMount testMount{builder};

  auto restricted = makeRestrictedInode(testMount, "restricted"_pc);

  auto context = ObjectFetchContext::getNullContext();
  expectEacces([&] {
    restricted->unlink("anything"_pc, InvalidationRequired::No, context).get();
  });
}

TEST(RestrictedTreeInode, symlinkReturnsEACCES) {
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "content");
  TestMount testMount{builder};

  auto restricted = makeRestrictedInode(testMount, "restricted"_pc);

  expectEacces([&] {
    restricted->symlink("link"_pc, "target", InvalidationRequired::No);
  });
}

TEST(RestrictedTreeInode, mknodReturnsEACCES) {
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "content");
  TestMount testMount{builder};

  auto restricted = makeRestrictedInode(testMount, "restricted"_pc);

  expectEacces([&] {
    restricted->mknod("file"_pc, S_IFREG | 0644, 0, InvalidationRequired::No);
  });
}

TEST(RestrictedTreeInode, setattrReturnsEACCES) {
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "content");
  TestMount testMount{builder};

  auto restricted = makeRestrictedInode(testMount, "restricted"_pc);

  auto context = ObjectFetchContext::getNullContext();
  expectEacces([&] { restricted->setattr(DesiredMetadata{}, context).get(); });
}

TEST(RestrictedTreeInode, lockContentsReadThrowsEACCES) {
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "content");
  TestMount testMount{builder};

  auto restricted = makeRestrictedInode(testMount, "restricted"_pc);

  expectEacces([&] { restricted->lockContentsRead(); });
}

TEST(RestrictedTreeInode, lockContentsWriteThrowsEACCES) {
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "content");
  TestMount testMount{builder};

  auto restricted = makeRestrictedInode(testMount, "restricted"_pc);

  expectEacces([&] { restricted->lockContentsWrite(); });
}

#ifndef _WIN32
TEST(RestrictedTreeInode, lastChildReferenceUnderRestrictedParentBypassesAcl) {
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "content");
  TestMount testMount{builder};

  auto restricted = makeRestrictedInode(testMount, "restricted"_pc);
  auto child =
      makeTreeInodeChildWithoutParentEntry(testMount, restricted, "child"_pc);
  auto childIno = child->getNodeId();
  child->incFsRefcount();

  EXPECT_NO_THROW(child.reset());
  EXPECT_NO_THROW(
      testMount.getEdenMount()->getInodeMap()->decFsRefcount(childIno, 1));

  // This synthetic child is still linked, so the InodeMap keeps it loaded via
  // onLinkedInodeUnreferenced(). The regression covered here is that dropping
  // the last references does not try to take the restricted parent's public
  // contents lock and throw EACCES.
  EXPECT_TRUE(
      testMount.getEdenMount()->getInodeMap()->isInodeLoadedOrRemembered(
          childIno));
}

TEST(
    RestrictedTreeInode,
    lastChildReferenceUnderRestrictedParentWithLoadedEntryBypassesAcl) {
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "content");
  TestMount testMount{builder};

  auto restricted = makeRestrictedInode(testMount, "restricted"_pc);
  auto child =
      makeTreeInodeChildWithoutParentEntry(testMount, restricted, "child"_pc);
  auto childIno = child->getNodeId();

  {
    auto contents = restricted->getContentsUnchecked().wlock();
    auto [entry, inserted] =
        contents->entries.emplace("child"_pc, S_IFDIR | 0755, childIno);
    ASSERT_TRUE(inserted);
    entry->second.setInode(child.get());
  }

  child->incFsRefcount();
  EXPECT_NO_THROW(child.reset());
  EXPECT_NO_THROW(
      testMount.getEdenMount()->getInodeMap()->decFsRefcount(childIno, 1));

  auto contents = restricted->getContentsUnchecked().rlock();
  auto entry = contents->entries.find("child"_pc);
  ASSERT_NE(entry, contents->entries.end());
  EXPECT_NE(nullptr, entry->second.getInode());
  EXPECT_EQ(childIno, entry->second.getInodeNumber());
}
#endif

TEST(RestrictedTreeInode, unrestricted_treeInodeIsNotRestricted) {
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "content");
  TestMount testMount{builder};

  auto dirInode = testMount.getTreeInode("dir"_relpath);
  EXPECT_FALSE(dirInode->isRestricted());
}

// --- End-to-end tests that go through the real inode loading pipeline ---

class RestrictedTreeInodeEndToEnd : public ::testing::Test {
 protected:
  void SetUp() override {
    FakeTreeBuilder builder;
    builder.setFile("restricted/secret.txt", "secret content");
    builder.setDirIsRestricted("restricted");
    testMount_ = std::make_unique<TestMount>(builder);
    auto restrictedObjectId =
        builder.getRoot()->get().find("restricted"_pc)->second.getObjectId();
    testMount_->getBackingStore()->setCheckPermissionResult(
        restrictedObjectId, false);
  }

  TreeInodePtr getRestrictedInode() {
    return testMount_->getTreeInode("restricted"_relpath);
  }

  std::unique_ptr<TestMount> testMount_;
};

TEST_F(
    RestrictedTreeInodeEndToEnd,
    loadingRestrictedDirCreatesRestrictedTreeInode) {
  auto restrictedInode = getRestrictedInode();
  EXPECT_TRUE(restrictedInode->isRestricted());
}

TEST_F(RestrictedTreeInodeEndToEnd, restrictedDirStatReturnsZeroPermissions) {
  auto restrictedInode = getRestrictedInode();
  auto context = ObjectFetchContext::getNullContext();
  auto st = restrictedInode->stat(context).get();

#ifndef _WIN32
  // Windows stat() doesn't set st_mode for directories (no metadata table).
  EXPECT_TRUE(S_ISDIR(st.st_mode));
  EXPECT_EQ(st.st_mode & 07777, 0);
#endif
}

TEST_F(RestrictedTreeInodeEndToEnd, restrictedDirGetOrFindChildReturnsEACCES) {
  auto restrictedInode = getRestrictedInode();
  auto context = ObjectFetchContext::getNullContext();
  expectEacces([&] {
    restrictedInode->getOrFindChild("secret.txt"_pc, context, false).get();
  });
}

TEST_F(RestrictedTreeInodeEndToEnd, restrictedDirLockContentsReadThrows) {
  auto restrictedInode = getRestrictedInode();
  expectEacces([&] { restrictedInode->lockContentsRead(); });
}

TEST(RestrictedTreeInode, parentListingIncludesRestrictedDir) {
  FakeTreeBuilder builder;
  builder.setFile("parent/normal.txt", "normal content");
  builder.setFile("parent/restricted_child/secret.txt", "secret content");
  builder.setDirIsRestricted("parent/restricted_child");
  TestMount testMount{builder};

  auto parentInode = testMount.getTreeInode("parent"_relpath);
  // Reach into entries to verify the DirEntry-level flag, not the inode.
  auto contents = parentInode->lockContentsRead();

  auto iter = contents->entries.find("restricted_child"_pc);
  ASSERT_NE(iter, contents->entries.end());
  EXPECT_TRUE(iter->second.isDirectory());
  EXPECT_TRUE(iter->second.isRestricted());
}

TEST(
    RestrictedTreeInode,
    fetchRestrictedTreeCachesRestrictionWhenParentMetadataMissing) {
  TestMount testMount;
  auto backingStore = testMount.getBackingStore();

  // Create a restricted child tree that would normally be discovered via
  // parent metadata before the child is loaded.
  auto [secretBlob, secretBlobId] = backingStore->putBlob("secret content");
  secretBlob->setReady();

  auto* restrictedTree = backingStore->putRestrictedTree({
      {"secret.txt", secretBlobId},
  });
  restrictedTree->setReady();
  auto restrictedTreeId = restrictedTree->get().getObjectId();

  // Build the parent entry directly so its metadata is missing even though a
  // direct child fetch still returns a restricted Tree. Cache that live result
  // on the DirEntry so future lookups can short-circuit before fetching again.
  Tree::container rootEntries{kPathMapDefaultCaseSensitive};
  rootEntries.emplace(
      "restricted"_pc,
      ObjectId{restrictedTreeId},
      TreeEntryType::TREE,
      /* isRestricted */ false,
      /* hasACL */ std::nullopt);
  auto* rootTree = backingStore->putTree(std::move(rootEntries));
  rootTree->setReady();
  backingStore->putCommit(RootId{"1"}, rootTree)->setReady();
  testMount.initialize(RootId{"1"});

  auto rootInode = testMount.getRootInode();
  {
    // Prove the synthetic setup actually starts with missing parent metadata:
    // the root DirEntry knows the child is a directory, but does not yet have
    // the restricted bit set.
    auto contents = rootInode->lockContentsRead();
    auto it = contents->entries.find("restricted"_pc);
    ASSERT_NE(it, contents->entries.end());
    EXPECT_FALSE(it->second.isRestricted());
  }

  // Nothing has looked up the child yet, so the restricted child tree has not
  // been fetched from the backing store.
  EXPECT_EQ(backingStore->getAccessCount(restrictedTreeId), 0);

  // The first lookup has to fetch the child tree. That fetch returns a
  // restricted Tree, so the resulting inode is restricted and the parent
  // DirEntry cache should be backfilled from the live fetch result.
  auto restrictedInode = testMount.getTreeInode("restricted"_relpath);
  ASSERT_TRUE(restrictedInode->isRestricted());
  EXPECT_EQ(backingStore->getAccessCount(restrictedTreeId), 1);

  {
    // Verify that the first lookup updated the parent-side cache, not just the
    // loaded child inode.
    auto contents = rootInode->lockContentsRead();
    auto it = contents->entries.find("restricted"_pc);
    ASSERT_NE(it, contents->entries.end());
    EXPECT_TRUE(it->second.isRestricted());
  }

  // Drop the loaded child and unload the parent's children so the next lookup
  // has to consult the parent DirEntry metadata again rather than reusing the
  // already-loaded restricted inode.
  restrictedInode.reset();
  rootInode->unloadChildrenNow();

  // The second lookup should now short-circuit from the cached restricted bit
  // on the parent DirEntry, so it still returns a restricted inode without
  // fetching the child tree a second time.
  auto reloadedInode = testMount.getTreeInode("restricted"_relpath);
  EXPECT_TRUE(reloadedInode->isRestricted());
  EXPECT_EQ(backingStore->getAccessCount(restrictedTreeId), 1);
}

TEST(RestrictedTreeInode, nestedRestrictedDirBlocksAccess) {
  FakeTreeBuilder builder;
  builder.setFile("parent/normal.txt", "normal content");
  builder.setFile("parent/restricted_child/secret.txt", "secret content");
  builder.setDirIsRestricted("parent/restricted_child");
  TestMount testMount{builder};

  auto restrictedInode =
      testMount.getTreeInode("parent/restricted_child"_relpath);
  EXPECT_TRUE(restrictedInode->isRestricted());
  testMount.getBackingStore()->setCheckPermissionResult(
      restrictedInode->getObjectId().value(), false);

  auto context = ObjectFetchContext::getNullContext();
  expectEacces([&] {
    restrictedInode->getOrFindChild("secret.txt"_pc, context, false).get();
  });
}

TEST_F(RestrictedTreeInodeEndToEnd, getObjectIdReturnsTrueId) {
  auto restrictedInode = getRestrictedInode();
  EXPECT_TRUE(restrictedInode->getObjectId().has_value());
}

#ifndef _WIN32
TEST_F(RestrictedTreeInodeEndToEnd, getMetadataBypassesAcl) {
  auto restrictedInode = getRestrictedInode();

  auto metadata = restrictedInode->getMetadata();

  EXPECT_TRUE(S_ISDIR(metadata.mode));
}

TEST_F(
    RestrictedTreeInodeEndToEnd,
    getInodeSlowRejectsDotEdenUnderRestrictedDir) {
  auto context = ObjectFetchContext::getNullContext();

  expectEacces([&] {
    auto lookup = testMount_->getEdenMount()->getInodeSlow(
        "restricted/.eden"_relpath, context);
    std::move(lookup).get();
  });
}

TEST(RestrictedTreeInode, getInodeSlowAllowsDotEdenUnderUnrestrictedDir) {
  FakeTreeBuilder builder;
  builder.setFile("visible/file.txt", "content");
  TestMount testMount{builder};

  auto context = ObjectFetchContext::getNullContext();
  auto throughVisible =
      testMount.getEdenMount()->getInodeSlow("visible/.eden"_relpath, context);
  auto dotEdenThisDir =
      testMount.getEdenMount()->getInodeSlow(".eden/this-dir"_relpath, context);
  auto throughVisibleInode = std::move(throughVisible).get();
  auto dotEdenThisDirInode = std::move(dotEdenThisDir).get();

  EXPECT_EQ(throughVisibleInode->getNodeId(), dotEdenThisDirInode->getNodeId());
}
#endif

TEST(RestrictedTreeInode, renameFromRestrictedDirReturnsEACCES) {
  // Renaming FROM a restricted directory should fail because the source
  // parent's checkAccess() fires before any lock acquisition.
  FakeTreeBuilder builder;
  builder.setFile("restricted/file.txt", "content");
  builder.setDirIsRestricted("restricted");
  builder.setFile("dest/other.txt", "other");
  TestMount testMount{builder};

  auto restricted = testMount.getTreeInode("restricted"_relpath);
  auto dest = testMount.getTreeInode("dest"_relpath);

  expectEacces([&] {
    restricted
        ->rename(
            "file.txt"_pc,
            dest,
            "moved.txt"_pc,
            InvalidationRequired::No,
            ObjectFetchContext::getNullContext())
        .get();
  });
}

TEST(RestrictedTreeInode, renameIntoRestrictedDirReturnsEACCES) {
  // Renaming INTO a restricted directory should fail because the destination
  // parent is checked via materialize() and TreeRenameLocks::acquireLocks(),
  // both of which call lockContentsWrite() -> checkAccess().
  FakeTreeBuilder builder;
  builder.setFile("src/file.txt", "content");
  builder.setFile("restricted/existing.txt", "secret");
  builder.setDirIsRestricted("restricted");
  TestMount testMount{builder};

  auto src = testMount.getTreeInode("src"_relpath);
  auto restricted = testMount.getTreeInode("restricted"_relpath);

  expectEacces([&] {
    src->rename(
           "file.txt"_pc,
           restricted,
           "moved.txt"_pc,
           InvalidationRequired::No,
           ObjectFetchContext::getNullContext())
        .get();
  });
}

CO_TEST(RestrictedTreeInode, getChildrenOnRestrictedTreeReturnsEACCES) {
  FakeTreeBuilder builder;
  builder.setFile("restricted/secret.txt", "secret");
  builder.setDirIsRestricted("restricted");
  TestMount testMount{builder};

  auto edenMount = testMount.getEdenMount();
  auto vi = testMount.getVirtualInode("restricted"_relpath);
  auto context = ObjectFetchContext::getNullContext();

  auto result = co_await folly::coro::co_awaitTry(vi.getChildren(
      "restricted"_relpath, edenMount->getObjectStore(), context));
  CO_ASSERT_TRUE(result.hasException());
  EXPECT_TRUE(result.hasException<std::system_error>());
  if (auto* ex = result.exception().get_exception<std::system_error>()) {
    EXPECT_EQ(ex->code().value(), EACCES);
  }
}

CO_TEST(
    RestrictedTreeInode,
    getChildrenSkipsBackingStoreFetchForRestrictedChild) {
  // Parent is unrestricted; one child entry is restricted. The coro path
  // must hand back a synthesized restricted VirtualInode for that child
  // instead of fetching its tree from the backing store.
  FakeTreeBuilder builder;
  builder.setFile("parent/normal/file.txt", "ok");
  builder.setFile("parent/restricted_child/secret.txt", "secret");
  builder.setDirIsRestricted("parent/restricted_child");
  TestMount testMount{builder};

  auto edenMount = testMount.getEdenMount();
  auto vi = testMount.getVirtualInode("parent"_relpath);
  auto context = ObjectFetchContext::getNullContext();

  auto results = co_await vi.getChildren(
      "parent"_relpath, edenMount->getObjectStore(), context);
  bool sawRestrictedChild = false;
  bool sawNormalChild = false;
  for (auto& [name, tryVi] : results) {
    if (name == "restricted_child"_pc) {
      sawRestrictedChild = true;
      CO_ASSERT_TRUE(tryVi.hasValue());
      // Reading children of the synthesized restricted VirtualInode must
      // surface EACCES — it is a real restricted view, not the underlying
      // tree contents.
      auto childResult =
          co_await folly::coro::co_awaitTry(tryVi.value().getChildren(
              "parent/restricted_child"_relpath,
              edenMount->getObjectStore(),
              context));
      EXPECT_TRUE(childResult.hasException<std::system_error>());
      if (auto* ex =
              childResult.exception().get_exception<std::system_error>()) {
        EXPECT_EQ(ex->code().value(), EACCES);
      }
    } else if (name == "normal"_pc) {
      sawNormalChild = true;
      CO_ASSERT_TRUE(tryVi.hasValue());
    }
  }
  EXPECT_TRUE(sawRestrictedChild);
  EXPECT_TRUE(sawNormalChild);
}

// Exercises the InodePtr branch — the dominant production path after a
// mount has loaded inodes.
CO_TEST(RestrictedTreeInode, getChildren_inodePtrBranchReturnsEntries) {
  FakeTreeBuilder builder;
  builder.setFile("dir/a.txt", "a");
  builder.setFile("dir/b.txt", "b");
  TestMount testMount{builder};

  auto edenMount = testMount.getEdenMount();
  auto dirInode = testMount.getTreeInode("dir"_relpath);
  TestMount::loadAllInodes(dirInode);
  VirtualInode vi{InodePtr{dirInode}};
  auto context = ObjectFetchContext::getNullContext();

  auto results = co_await vi.getChildren(
      "dir"_relpath, edenMount->getObjectStore(), context);
  EXPECT_EQ(results.size(), 2);
  for (auto& [name, tryVi] : results) {
    CO_ASSERT_TRUE(tryVi.hasValue());
  }
}

// Catches a regression where the early isDirectory() guard drifts out of
// sync with the variant arms.
CO_TEST(RestrictedTreeInode, getChildren_onFileReturnsENOTDIR) {
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "content");
  TestMount testMount{builder};

  auto edenMount = testMount.getEdenMount();
  auto vi = testMount.getVirtualInode("dir/file.txt"_relpath);
  auto context = ObjectFetchContext::getNullContext();

  auto result = co_await folly::coro::co_awaitTry(vi.getChildren(
      "dir/file.txt"_relpath, edenMount->getObjectStore(), context));
  CO_ASSERT_TRUE(result.hasException());
  EXPECT_TRUE(result.hasException<std::system_error>());
  if (auto* ex = result.exception().get_exception<std::system_error>()) {
    EXPECT_EQ(ex->code().value(), ENOTDIR);
  }
}

CO_TEST(
    RestrictedTreeInode,
    co_getChildrenAttributes_onRestrictedTreeReturnsEACCES) {
  FakeTreeBuilder builder;
  builder.setFile("restricted/secret.txt", "secret");
  builder.setDirIsRestricted("restricted");
  TestMount testMount{builder};

  auto edenMount = testMount.getEdenMount();
  auto vi = testMount.getVirtualInode("restricted"_relpath);
  auto context = ObjectFetchContext::getNullContext();
  auto attrs = ENTRY_ATTRIBUTE_SOURCE_CONTROL_TYPE | ENTRY_ATTRIBUTE_SIZE;

  auto result = co_await folly::coro::co_awaitTry(vi.co_getChildrenAttributes(
      attrs,
      RelativePath{"restricted"},
      edenMount->getObjectStore(),
      edenMount->getLastCheckoutTime().toTimespec(),
      context));
  CO_ASSERT_TRUE(result.hasException());
  EXPECT_TRUE(result.hasException<std::system_error>());
  if (auto* ex = result.exception().get_exception<std::system_error>()) {
    EXPECT_EQ(ex->code().value(), EACCES);
  }
}

// Dominant production path after a mount has loaded inodes.
CO_TEST(
    RestrictedTreeInode,
    co_getChildrenAttributes_inodePtrBranchReturnsAttrs) {
  FakeTreeBuilder builder;
  builder.setFile("dir/a.txt", "a");
  builder.setFile("dir/b.txt", "b");
  TestMount testMount{builder};

  auto edenMount = testMount.getEdenMount();
  auto dirInode = testMount.getTreeInode("dir"_relpath);
  TestMount::loadAllInodes(dirInode);
  VirtualInode vi{InodePtr{dirInode}};
  auto context = ObjectFetchContext::getNullContext();
  auto attrs = ENTRY_ATTRIBUTE_SOURCE_CONTROL_TYPE | ENTRY_ATTRIBUTE_SIZE;

  auto results = co_await vi.co_getChildrenAttributes(
      attrs,
      RelativePath{"dir"},
      edenMount->getObjectStore(),
      edenMount->getLastCheckoutTime().toTimespec(),
      context);
  EXPECT_EQ(results.size(), 2);
  for (auto& [name, tryAttrs] : results) {
    CO_ASSERT_TRUE(tryAttrs.hasValue());
  }
}

CO_TEST(RestrictedTreeInode, co_getChildrenAttributes_onFileReturnsENOTDIR) {
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "content");
  TestMount testMount{builder};

  auto edenMount = testMount.getEdenMount();
  auto vi = testMount.getVirtualInode("dir/file.txt"_relpath);
  auto context = ObjectFetchContext::getNullContext();
  auto attrs = ENTRY_ATTRIBUTE_SOURCE_CONTROL_TYPE | ENTRY_ATTRIBUTE_SIZE;

  auto result = co_await folly::coro::co_awaitTry(vi.co_getChildrenAttributes(
      attrs,
      RelativePath{"dir/file.txt"},
      edenMount->getObjectStore(),
      edenMount->getLastCheckoutTime().toTimespec(),
      context));
  CO_ASSERT_TRUE(result.hasException());
  EXPECT_TRUE(result.hasException<std::system_error>());
  if (auto* ex = result.exception().get_exception<std::system_error>()) {
    EXPECT_EQ(ex->code().value(), ENOTDIR);
  }
}

// Covers the file-inline, restricted-child, and non-restricted-tree per-child
// dispatch arms in one shot.
CO_TEST(
    RestrictedTreeInode,
    co_getChildrenAttributes_treePtrBranchReturnsMixedAttrs) {
  FakeTreeBuilder builder;
  builder.setFile("parent/normal/file.txt", "ok");
  builder.setFile("parent/leaf.txt", "leaf");
  builder.setFile("parent/restricted_child/secret.txt", "secret");
  builder.setDirIsRestricted("parent/restricted_child");
  TestMount testMount{builder};

  auto edenMount = testMount.getEdenMount();
  auto vi = testMount.getVirtualInode("parent"_relpath);
  auto context = ObjectFetchContext::getNullContext();
  auto attrs = ENTRY_ATTRIBUTE_SOURCE_CONTROL_TYPE;

  auto results = co_await vi.co_getChildrenAttributes(
      attrs,
      RelativePath{"parent"},
      edenMount->getObjectStore(),
      edenMount->getLastCheckoutTime().toTimespec(),
      context);
  bool sawNormal = false;
  bool sawLeaf = false;
  bool sawRestricted = false;
  for (auto& [name, tryAttrs] : results) {
    if (name == "normal"_pc) {
      sawNormal = true;
      CO_ASSERT_TRUE(tryAttrs.hasValue());
    } else if (name == "leaf.txt"_pc) {
      sawLeaf = true;
      CO_ASSERT_TRUE(tryAttrs.hasValue());
    } else if (name == "restricted_child"_pc) {
      sawRestricted = true;
      CO_ASSERT_TRUE(tryAttrs.hasValue());
    }
  }
  EXPECT_TRUE(sawNormal);
  EXPECT_TRUE(sawLeaf);
  EXPECT_TRUE(sawRestricted);
}

TEST(RestrictedTreeInode, isRestricted_reflectsVariant) {
  FakeTreeBuilder builder;
  builder.setFile("normal/file.txt", "ok");
  builder.setFile("restricted/secret.txt", "secret");
  builder.setDirIsRestricted("restricted");
  TestMount testMount{builder};

  // Unloaded directories (TreePtr variant).
  EXPECT_TRUE(testMount.getVirtualInode("restricted"_relpath).isRestricted());
  EXPECT_FALSE(testMount.getVirtualInode("normal"_relpath).isRestricted());

  // Loaded directories (InodePtr variant).
  auto restrictedInode = makeRestrictedInode(testMount, "loaded_restricted"_pc);
  EXPECT_TRUE(VirtualInode{InodePtr{restrictedInode}}.isRestricted());
  auto normalInode = testMount.getTreeInode("normal"_relpath);
  EXPECT_FALSE(VirtualInode{InodePtr{normalInode}}.isRestricted());
}

CO_TEST(
    RestrictedTreeInode,
    co_getDigestHash_restrictedTreePtrWithholdsDigestHash) {
  FakeTreeBuilder builder;
  builder.setFile("restricted/secret.txt", "secret");
  builder.setDirIsRestricted("restricted");
  TestMount testMount{builder};

  auto restricted = testMount.getVirtualInode("restricted"_relpath);
  CO_ASSERT_EQ(
      VirtualInode::ContainedType::Tree, restricted.testGetContainedType());
  CO_ASSERT_TRUE(restricted.isRestricted());

  auto result = co_await folly::coro::co_awaitTry(restricted.co_getDigestHash(
      "restricted"_relpath,
      testMount.getEdenMount()->getObjectStore(),
      ObjectFetchContext::getNullContext()));
  CO_ASSERT_TRUE(result.hasValue());
  EXPECT_FALSE(result.value().has_value());
}

TEST(RestrictedTreeInode, tryGetEntryAttributesSync_restrictedDirWithholdsAux) {
  FakeTreeBuilder builder;
  builder.setFile("normal/file.txt", "ok");
  builder.setFile("restricted/secret.txt", "secret");
  builder.setDirIsRestricted("restricted");
  TestMount testMount{builder};

  auto edenMount = testMount.getEdenMount();
  auto objectStore = edenMount->getObjectStore();
  auto context = ObjectFetchContext::getNullContext();
  auto checkoutTime = edenMount->getLastCheckoutTime().toTimespec();
  auto attrs = ENTRY_ATTRIBUTE_DIGEST_HASH | ENTRY_ATTRIBUTE_DIGEST_SIZE;

  auto restrictedVi = testMount.getVirtualInode("restricted"_relpath);
  EXPECT_TRUE(restrictedVi.isRestricted());
  auto restrictedResult = restrictedVi.tryGetEntryAttributesSync(
      attrs, "restricted"_relpath, objectStore, checkoutTime, context);
  EXPECT_TRUE(restrictedResult.has_value());
  if (restrictedResult.has_value()) {
    EXPECT_FALSE(restrictedResult->digestHash.has_value());
    EXPECT_FALSE(restrictedResult->digestSize.has_value());
  }

  auto normalVi = testMount.getVirtualInode("normal"_relpath);
  EXPECT_FALSE(normalVi.isRestricted());
  auto normalResult = normalVi.tryGetEntryAttributesSync(
      attrs, "normal"_relpath, objectStore, checkoutTime, context);
  if (normalResult.has_value()) {
    EXPECT_TRUE(normalResult->digestHash.has_value());
  }
}

// ============================================================================
// Coroutine getChildren ACL parity (regression for TreeInode recheck gate).
// Ensures recheckPermissionIfExpired runs before the contents lock so
// getChildren does not stick on stale EACCES after a TTL-expired permission
// grant.
// ============================================================================

CO_TEST(RestrictedTreeInode, getChildren_returnsEntriesOnUnrestrictedRoot) {
  // Smoke test for the success path: recheck short-circuits on a
  // non-restricted directory and entries are returned without EACCES.
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "content");
  TestMount testMount{builder};

  auto rootInode = testMount.getEdenMount()->getRootInode();
  auto context = ObjectFetchContext::getNullContext();

  auto results = co_await rootInode->getChildren(context);
  bool sawDir = false;
  for (auto& [name, tryVi] : results) {
    if (name == "dir"_pc) {
      sawDir = true;
      CO_ASSERT_TRUE(tryVi.hasValue());
    }
  }
  EXPECT_TRUE(sawDir);
}

// Exercises the wlock + loadChild + inodeLoadCleanUps + SCOPE_EXIT
// discipline in TreeInode::getChildren that the loadInodes=false default
// never reaches (rlockCheckChild returns an inline VirtualInode for
// unmaterialized entries). loadInodes=true forces every non-loaded entry
// through loadChild, which queues a LoadChildCleanUp drained by SCOPE_EXIT
// after the lock releases.
CO_TEST(RestrictedTreeInode, getChildren_loadInodesTrueExercisesLoadChild) {
  FakeTreeBuilder builder;
  builder.setFile("dir/a.txt", "a");
  builder.setFile("dir/b.txt", "b");
  builder.setFile("dir/c.txt", "c");
  TestMount testMount{builder};

  auto dirInode = testMount.getTreeInode("dir"_relpath);
  auto context = ObjectFetchContext::getNullContext();

  auto results = co_await dirInode->getChildren(context, /*loadInodes=*/true);
  EXPECT_EQ(results.size(), 3);
  for (auto& [name, tryVi] : results) {
    CO_ASSERT_TRUE(tryVi.hasValue());
    EXPECT_TRUE(tryVi.value().asInodePtr() != nullptr);
  }
}

// Exercises the EACCES-on-lockContentsWrite path. The SCOPE_EXIT is already
// registered when lockContentsWrite() throws — this verifies the empty
// inodeLoadCleanUps unwind branch is benign.
CO_TEST(RestrictedTreeInode, getChildren_restrictedThrowsEACCES) {
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "content");
  TestMount testMount{builder};

  auto restricted = makeRestrictedInode(testMount, "restricted"_pc);
  auto context = ObjectFetchContext::getNullContext();

  auto result = co_await folly::coro::co_awaitTry(
      restricted->getChildren(context, /*loadInodes=*/false));
  CO_ASSERT_TRUE(result.hasException());
  auto* err = result.tryGetExceptionObject<std::system_error>();
  CO_ASSERT_NE(err, nullptr);
  EXPECT_EQ(err->code().value(), EACCES);
}

// ============================================================================
// acl:restricted-content-mode = omitted
//
// Under omitted mode, restricted ACL roots are hidden from enumeration
// (readdir, getChildren, co_getChildrenAttributes) while explicit lookup
// by name keeps returning EACCES on access.
// ============================================================================

TEST(RestrictedTreeInode, omittedMode_objectStoreSnapshotsMode) {
  auto testMount = makeOmittedModeTestMount();

  EXPECT_EQ(
      RestrictedContentMode::Omitted,
      testMount->getEdenMount()->getObjectStore()->getRestrictedContentMode());
}

CO_TEST(RestrictedTreeInode, omittedMode_getChildrenOmitsRestrictedChild) {
  auto testMount = makeOmittedModeTestMount();

  auto parentInode = testMount->getTreeInode("parent"_relpath);
  auto context = ObjectFetchContext::getNullContext();
  auto children =
      co_await parentInode->getChildren(context, /*loadInodes=*/false);

  auto restrictedIter =
      std::find_if(children.begin(), children.end(), [](const auto& entry) {
        return entry.first == "restricted_child"_pc;
      });
  EXPECT_EQ(restrictedIter, children.end());

  auto normalIter =
      std::find_if(children.begin(), children.end(), [](const auto& entry) {
        return entry.first == "normal.txt"_pc;
      });
  EXPECT_NE(normalIter, children.end());
}

CO_TEST(RestrictedTreeInode, omittedMode_coGetChildrenOmitsRestrictedChild) {
  auto testMount = makeOmittedModeTestMount();

  auto edenMount = testMount->getEdenMount();
  auto vi = testMount->getVirtualInode("parent"_relpath);
  auto context = ObjectFetchContext::getNullContext();

  auto results = co_await vi.getChildren(
      "parent"_relpath, edenMount->getObjectStore(), context);
  bool sawRestrictedChild = false;
  bool sawNormalChild = false;
  for (auto& [name, tryVi] : results) {
    if (name == "restricted_child"_pc) {
      sawRestrictedChild = true;
    } else if (name == "normal.txt"_pc) {
      sawNormalChild = true;
    }
  }
  EXPECT_FALSE(sawRestrictedChild);
  EXPECT_TRUE(sawNormalChild);
}

CO_TEST(
    RestrictedTreeInode,
    omittedMode_coGetChildrenAttributesOmitsRestrictedChild) {
  auto testMount = makeOmittedModeTestMount();

  auto edenMount = testMount->getEdenMount();
  auto vi = testMount->getVirtualInode("parent"_relpath);
  auto context = ObjectFetchContext::getNullContext();
  auto attrs = ENTRY_ATTRIBUTE_SOURCE_CONTROL_TYPE;

  auto results = co_await vi.co_getChildrenAttributes(
      attrs,
      RelativePath{"parent"},
      edenMount->getObjectStore(),
      edenMount->getLastCheckoutTime().toTimespec(),
      context);
  bool sawRestrictedChild = false;
  bool sawNormalChild = false;
  for (auto& [name, tryAttrs] : results) {
    if (name == "restricted_child"_pc) {
      sawRestrictedChild = true;
    } else if (name == "normal.txt"_pc) {
      sawNormalChild = true;
    }
  }
  EXPECT_FALSE(sawRestrictedChild);
  EXPECT_TRUE(sawNormalChild);
}

#ifndef _WIN32
TEST(RestrictedTreeInode, omittedMode_fuseReaddirOmitsRestrictedChild) {
  auto testMount = makeOmittedModeTestMount();

  auto parentInode = testMount->getTreeInode("parent"_relpath);
  auto result =
      parentInode
          ->fuseReaddir(
              FuseDirList{4096}, 0, ObjectFetchContext::getNullContext())
          .extract();

  auto restrictedIter =
      std::find_if(result.begin(), result.end(), [](const auto& entry) {
        return entry.name == "restricted_child";
      });
  EXPECT_EQ(restrictedIter, result.end());

  auto normalIter =
      std::find_if(result.begin(), result.end(), [](const auto& entry) {
        return entry.name == "normal.txt";
      });
  EXPECT_NE(normalIter, result.end());
}

TEST(RestrictedTreeInode, omittedMode_fuseReaddirResumesAcrossOmittedEntry) {
  // The restricted child's name sorts between two normal siblings, so its
  // (omitted) inode number falls between theirs and resumed readdir calls
  // cross the hole it leaves in the offset sequence.
  FakeTreeBuilder builder;
  builder.setFile("parent/a.txt", "a");
  builder.setFile("parent/m_restricted/secret.txt", "secret");
  builder.setFile("parent/z.txt", "z");
  builder.setDirIsRestricted("parent/m_restricted");
  auto testMount = makeOmittedModeTestMount(builder);

  auto parentInode = testMount->getTreeInode("parent"_relpath);
  auto context = ObjectFetchContext::getNullContext();

  auto full = parentInode->fuseReaddir(FuseDirList{4096}, 0, context).extract();
  std::vector<std::string> fullNames;
  fullNames.reserve(full.size());
  for (const auto& entry : full) {
    fullNames.push_back(entry.name);
  }
  const std::vector<std::string> expectedFull{".", "..", "a.txt", "z.txt"};
  EXPECT_EQ(expectedFull, fullNames);

  // Resuming from each entry's offset must return exactly the remaining
  // visible entries: omission may not shift offsets between calls, re-list
  // earlier entries, skip later ones, or surface the omitted entry.
  for (size_t i = 0; i < full.size(); ++i) {
    auto rest =
        parentInode->fuseReaddir(FuseDirList{4096}, full[i].offset, context)
            .extract();
    std::vector<std::string> restNames;
    restNames.reserve(rest.size());
    for (const auto& entry : rest) {
      restNames.push_back(entry.name);
    }
    const std::vector<std::string> expectedRest(
        fullNames.begin() + i + 1, fullNames.end());
    EXPECT_EQ(expectedRest, restNames)
        << "mismatch resuming from offset of " << fullNames[i];
  }
}

CO_TEST(RestrictedTreeInode, omittedMode_readdirIndexReflectsPermissionGrant) {
  // Granting access clears the restricted bit in place, which leaves the
  // entries' mutation count alone. An index cached by a listing that was
  // abandoned before its end therefore stays usable, and must list the
  // entry that just became visible.
  FakeTreeBuilder builder;
  builder.setFile("parent/a.txt", "a");
  builder.setFile("parent/b.txt", "b");
  builder.setFile("parent/c.txt", "c");
  builder.setFile("parent/z_restricted/secret.txt", "secret");
  builder.setDirIsRestricted("parent/z_restricted");
  auto testMount = makeOmittedModeTestMount(builder);

  auto parentInode = testMount->getTreeInode("parent"_relpath);
  auto restrictedInode = testMount->getTreeInode("parent/z_restricted"_relpath);
  auto context = ObjectFetchContext::getNullContext();
  CO_ASSERT_TRUE(restrictedInode->isRestricted());

  // Too small for ".", "..", "a.txt", "b.txt" and "c.txt", so the listing
  // ends early and leaves its index behind.
  auto firstPage =
      parentInode->fuseReaddir(FuseDirList{128}, 0, context).extract();
  CO_ASSERT_FALSE(firstPage.empty());
  CO_ASSERT_LT(firstPage.size(), 5u);

  testMount->getBackingStore()->setCheckPermissionResult(
      restrictedInode->getObjectId().value(), true);
  co_await restrictedInode->co_stat(context);
  CO_ASSERT_FALSE(restrictedInode->isRestricted());

  auto relisted =
      parentInode->fuseReaddir(FuseDirList{4096}, 0, context).extract();
  EXPECT_NE(
      std::find_if(
          relisted.begin(),
          relisted.end(),
          [](const auto& entry) { return entry.name == "z_restricted"; }),
      relisted.end());
}

TEST(
    RestrictedTreeInode,
    omittedMode_nfsReaddirReportsEofWithTrailingOmittedEntry) {
  // NFS is the only consumer of readdirImpl's EOF bit, and the planned
  // batched enumeration-time permission refresh will rework this loop, so
  // pin now that a drain whose trailing entry is omitted still reports EOF.
  //
  // Inode numbers are allocated in name order, so z_restricted's inode
  // number (and thus readdir offset) is the largest — the omitted entry
  // is the trailing one in the index.
  FakeTreeBuilder builder;
  builder.setFile("parent/a.txt", "a");
  builder.setFile("parent/z_restricted/secret.txt", "secret");
  builder.setDirIsRestricted("parent/z_restricted");
  auto testMount = makeOmittedModeTestMount(builder);

  auto parentInode = testMount->getTreeInode("parent"_relpath);
  auto [list, isEof] = parentInode->nfsReaddir(
      NfsDirList{4096, nfsv3Procs::readdir},
      0,
      ObjectFetchContext::getNullContext());

  EXPECT_TRUE(isEof);
  std::vector<std::string> names;
  for (const auto& entry : list.extractList<entry3>().list) {
    names.push_back(entry.name);
  }
  const std::vector<std::string> expected{".", "..", "a.txt"};
  EXPECT_EQ(expected, names);
}
#endif

TEST(RestrictedTreeInode, omittedMode_explicitLookupStillReturnsEACCES) {
  // Regression guard for the omission diff: omitted mode must only affect
  // enumeration. Explicit lookup by name still resolves the restricted child,
  // and reading its contents still fails with EACCES.
  auto testMount = makeOmittedModeTestMount();

  auto parentInode = testMount->getTreeInode("parent"_relpath);
  auto context = ObjectFetchContext::getNullContext();
  auto child =
      parentInode
          ->getOrFindChild("restricted_child"_pc, context, /*loadInodes=*/false)
          .get();
  EXPECT_TRUE(child.isRestricted());

  auto restrictedInode =
      testMount->getTreeInode("parent/restricted_child"_relpath);
  testMount->getBackingStore()->setCheckPermissionResult(
      restrictedInode->getObjectId().value(), false);
  expectEacces([&] {
    restrictedInode->getOrFindChild("secret.txt"_pc, context, false).get();
  });
}

CO_TEST(
    RestrictedTreeInode,
    omittedMode_checkoutStillProcessesHiddenRestrictedRoot) {
  // The all() side of DirEntries: an entry hidden from enumeration is still
  // walked by checkout, so it picks up the new commit's unrestricted content.
  auto testMount = makeOmittedModeTestMount();
  auto parentInode = testMount->getTreeInode("parent"_relpath);
  auto context = ObjectFetchContext::getNullContext();
  auto findChild = [](const auto& children) {
    return std::find_if(children.begin(), children.end(), [](const auto& e) {
      return e.first == "restricted_child"_pc;
    });
  };

  auto before = co_await parentInode->getChildren(context, false);
  EXPECT_EQ(findChild(before), before.end());

  FakeTreeBuilder builder2;
  builder2.setFile("parent/normal.txt", "normal content");
  builder2.setFile("parent/restricted_child/public.txt", "now public");
  builder2.finalize(testMount->getBackingStore(), true);
  testMount->getBackingStore()->putCommit(RootId{"2"}, builder2)->setReady();

  testMount->drainServerExecutor();
  auto checkoutFuture = testMount->checkout(RootId{"2"}, context, __func__);
  testMount->drainServerExecutor();
  CO_ASSERT_TRUE(checkoutFuture.isReady());
  EXPECT_EQ(0, std::move(checkoutFuture).get().conflicts.size());

  auto after = co_await parentInode->getChildren(context, false);
  auto iter = findChild(after);
  CO_ASSERT_NE(iter, after.end());
  EXPECT_FALSE(iter->second.value().isRestricted());
  EXPECT_EQ(
      "now public", testMount->readFile("parent/restricted_child/public.txt"));
}

#ifdef __linux__
constexpr auto kTimeout = std::chrono::seconds{10};

namespace {
void stopFakeFuse(TestMount& testMount, FakeFuse& fuse) {
  fuse.close();
  testMount.getEdenMount()
      ->getFsChannelCompletionFuture()
      .within(kTimeout)
      .getVia(testMount.getServerExecutor().get());
}

bool sawInodeInvalidation(FakeFuse& fuse, InodeNumber ino) {
  const auto responses = fuse.getAllResponses();
  return std::any_of(
      responses.begin(), responses.end(), [ino](const auto& response) {
        return response.header.error == FUSE_NOTIFY_INVAL_INODE &&
            reinterpret_cast<const fuse_notify_inval_inode_out*>(
                response.body.data())
                ->ino == ino.get();
      });
}
} // namespace

CO_TEST(
    RestrictedTreeInode,
    omittedMode_transitionToUnrestrictedInvalidatesParentDirCache) {
  auto testMount = makeOmittedModeTestMount();
  auto fuse = std::make_shared<FakeFuse>();
  testMount->startFuseAndWait(fuse);
  SCOPE_EXIT {
    stopFakeFuse(*testMount, *fuse);
  };

  auto parentInode = testMount->getTreeInode("parent"_relpath);
  auto restrictedInode =
      testMount->getTreeInode("parent/restricted_child"_relpath);
  CO_ASSERT_TRUE(restrictedInode->isRestricted());
  testMount->getBackingStore()->setCheckPermissionResult(
      restrictedInode->getObjectId().value(), true);

  // The first stat rechecks permission and transitions to unrestricted.
  co_await restrictedInode->co_stat(ObjectFetchContext::getNullContext());
  EXPECT_FALSE(restrictedInode->isRestricted());

  testMount->getEdenMount()->flushInvalidations().get(kTimeout);
  // The parent's cached listing must be dropped too.
  EXPECT_TRUE(sawInodeInvalidation(*fuse, parentInode->getNodeId()));
}

TEST(
    RestrictedTreeInode,
    omittedMode_loadTimeRestrictionInvalidatesParentDirCache) {
  // The parent entry says unrestricted but the fetched child tree is
  // restricted, so loading the child backfills the parent entry and, in
  // omitted mode, hides it from the parent's listing.
  TestMount testMount{/*enableActivityBuffer=*/true,
                      kPathMapDefaultCaseSensitive,
                      /*errorLogger=*/nullptr,
                      setOmittedMode};
  auto backingStore = testMount.getBackingStore();
  auto [secretBlob, secretBlobId] = backingStore->putBlob("secret content");
  secretBlob->setReady();
  auto* restrictedTree =
      backingStore->putRestrictedTree({{"secret.txt", secretBlobId}});
  restrictedTree->setReady();

  Tree::container parentEntries{kPathMapDefaultCaseSensitive};
  parentEntries.emplace(
      "restricted"_pc,
      ObjectId{restrictedTree->get().getObjectId()},
      TreeEntryType::TREE,
      /* isRestricted */ false,
      /* hasACL */ std::nullopt);
  auto* parentTree = backingStore->putTree(std::move(parentEntries));
  parentTree->setReady();
  Tree::container rootEntries{kPathMapDefaultCaseSensitive};
  rootEntries.emplace(
      "parent"_pc,
      ObjectId{parentTree->get().getObjectId()},
      TreeEntryType::TREE);
  auto* rootTree = backingStore->putTree(std::move(rootEntries));
  rootTree->setReady();
  backingStore->putCommit(RootId{"1"}, rootTree)->setReady();
  testMount.initialize(RootId{"1"});
  ASSERT_EQ(
      RestrictedContentMode::Omitted,
      testMount.getEdenMount()->getObjectStore()->getRestrictedContentMode());

  auto fuse = std::make_shared<FakeFuse>();
  testMount.startFuseAndWait(fuse);
  SCOPE_EXIT {
    stopFakeFuse(testMount, *fuse);
  };

  auto parentInode = testMount.getTreeInode("parent"_relpath);
  auto restrictedInode = testMount.getTreeInode("parent/restricted"_relpath);
  ASSERT_TRUE(restrictedInode->isRestricted());

  testMount.getEdenMount()->flushInvalidations().get(kTimeout);
  // The entry just vanished from the listing; the cache must be dropped.
  EXPECT_TRUE(sawInodeInvalidation(*fuse, parentInode->getNodeId()));
}
#endif // __linux__

#ifndef _WIN32
namespace {
// Reads the restricted bit of parent/restricted_child straight out of the
// parent's DirContents, bypassing the omitted-mode listing filter.
bool entryIsRestricted(
    const TreeInodePtr& parentInode,
    PathComponentPiece name) {
  auto contents = parentInode->lockContentsRead();
  return contents->entries.find(name)->second.isRestricted();
}

ObjectId entryObjectId(
    const TreeInodePtr& parentInode,
    PathComponentPiece name) {
  auto contents = parentInode->lockContentsRead();
  return contents->entries.find(name)->second.getObjectId();
}

bool restrictedChildIsHidden(const TreeInodePtr& parentInode) {
  return entryIsRestricted(parentInode, "restricted_child"_pc);
}

ObjectId restrictedChildId(const TreeInodePtr& parentInode) {
  return entryObjectId(parentInode, "restricted_child"_pc);
}

bool listsRestrictedChild(const TreeInodePtr& parentInode) {
  auto context = ObjectFetchContext::getNullContext();
  auto result =
      parentInode->fuseReaddir(FuseDirList{4096}, 0, context).extract();
  return std::any_of(result.begin(), result.end(), [](const auto& entry) {
    return entry.name == "restricted_child";
  });
}
} // namespace

TEST(
    RestrictedTreeInode,
    omittedMode_backgroundRefreshRechecksHiddenRestrictedChild) {
  auto testMount = makeOmittedModeTestMount();
  auto rootInode = testMount->getEdenMount()->getRootInode();
  auto parentInode = testMount->getTreeInode("parent"_relpath);
  auto context = ObjectFetchContext::getNullContext();
  auto childId = restrictedChildId(parentInode);
  auto* backingStore = testMount->getBackingStore().get();
  backingStore->setCheckPermissionResult(childId, true);

  // The kernel holds the parent's listing, so the walk has something to
  // refresh.
  parentInode->incFsRefcount();

  EXPECT_FALSE(listsRestrictedChild(parentInode));
  EXPECT_EQ(0, backingStore->getCheckPermissionCount(childId));

  rootInode->recheckHiddenRestrictedDescendants(context);
  testMount->drainServerExecutor();

  EXPECT_EQ(1, backingStore->getCheckPermissionCount(childId));
  EXPECT_FALSE(restrictedChildIsHidden(parentInode));
  EXPECT_TRUE(listsRestrictedChild(parentInode));
}

TEST(
    RestrictedTreeInode,
    omittedMode_backgroundRefreshSkipsTreeTheKernelDoesNotHold) {
  auto testMount = makeOmittedModeTestMount();
  auto rootInode = testMount->getEdenMount()->getRootInode();
  auto parentInode = testMount->getTreeInode("parent"_relpath);
  auto context = ObjectFetchContext::getNullContext();
  auto childId = restrictedChildId(parentInode);
  auto* backingStore = testMount->getBackingStore().get();
  backingStore->setCheckPermissionResult(childId, true);

  ASSERT_EQ(0, parentInode->debugGetFsRefcount());
  rootInode->recheckHiddenRestrictedDescendants(context);
  testMount->drainServerExecutor();
  EXPECT_EQ(0, backingStore->getCheckPermissionCount(childId));
  EXPECT_TRUE(restrictedChildIsHidden(parentInode));

  // The refcount is the only thing that was holding the check back.
  parentInode->incFsRefcount();
  rootInode->recheckHiddenRestrictedDescendants(context);
  testMount->drainServerExecutor();
  EXPECT_EQ(1, backingStore->getCheckPermissionCount(childId));
  EXPECT_FALSE(restrictedChildIsHidden(parentInode));
}

TEST(
    RestrictedTreeInode,
    omittedMode_backgroundRefreshLeavesDeniedChildHidden) {
  auto testMount = makeOmittedModeTestMount();
  auto rootInode = testMount->getEdenMount()->getRootInode();
  auto parentInode = testMount->getTreeInode("parent"_relpath);
  auto context = ObjectFetchContext::getNullContext();
  auto childId = restrictedChildId(parentInode);
  auto* backingStore = testMount->getBackingStore().get();
  backingStore->setCheckPermissionResult(childId, false);
  parentInode->incFsRefcount();

  rootInode->recheckHiddenRestrictedDescendants(context);
  testMount->drainServerExecutor();
  EXPECT_EQ(1, backingStore->getCheckPermissionCount(childId));
  EXPECT_TRUE(restrictedChildIsHidden(parentInode));
  EXPECT_FALSE(listsRestrictedChild(parentInode));

  // Within the TTL a second walk schedules no new check.
  rootInode->recheckHiddenRestrictedDescendants(context);
  testMount->drainServerExecutor();
  EXPECT_EQ(1, backingStore->getCheckPermissionCount(childId));
  EXPECT_TRUE(restrictedChildIsHidden(parentInode));
}

TEST(
    RestrictedTreeInode,
    omittedMode_backgroundRefreshSkipsLoadedRestrictedDirTheKernelHolds) {
  // An explicit lookup loads a restricted directory and hands the kernel a
  // reference to it. The walk must step over it, not read its contents, or
  // the rest of the mount after it in traversal order is never visited.
  FakeTreeBuilder builder;
  // Distinct contents so the two restricted trees get distinct ObjectIds.
  builder.setFile("early_restricted/secret.txt", "early secret content");
  builder.setDirIsRestricted("early_restricted");
  builder.setFile("parent/normal.txt", "normal content");
  builder.setFile("parent/restricted_child/secret.txt", "secret content");
  builder.setDirIsRestricted("parent/restricted_child");
  auto testMount = makeOmittedModeTestMount(builder);
  auto rootInode = testMount->getEdenMount()->getRootInode();
  auto parentInode = testMount->getTreeInode("parent"_relpath);
  auto earlyInode = testMount->getTreeInode("early_restricted"_relpath);
  ASSERT_TRUE(earlyInode->isRestricted());
  auto context = ObjectFetchContext::getNullContext();
  auto earlyId = entryObjectId(rootInode, "early_restricted"_pc);
  auto childId = restrictedChildId(parentInode);
  ASSERT_NE(earlyId, childId);
  auto* backingStore = testMount->getBackingStore().get();
  backingStore->setCheckPermissionResult(earlyId, false);
  backingStore->setCheckPermissionResult(childId, true);

  rootInode->incFsRefcount();
  earlyInode->incFsRefcount();
  parentInode->incFsRefcount();

  rootInode->recheckHiddenRestrictedDescendants(context);
  testMount->drainServerExecutor();

  // The still-denied directory was rechecked by its parent and stays hidden.
  EXPECT_EQ(1, backingStore->getCheckPermissionCount(earlyId));
  EXPECT_TRUE(entryIsRestricted(rootInode, "early_restricted"_pc));
  EXPECT_TRUE(earlyInode->isRestricted());
  // The walk went on past it and refreshed the directory further along.
  EXPECT_EQ(1, backingStore->getCheckPermissionCount(childId));
  EXPECT_FALSE(restrictedChildIsHidden(parentInode));
  EXPECT_TRUE(listsRestrictedChild(parentInode));
}
#endif // _WIN32

CO_TEST(RestrictedTreeInode, asyncDigestOfRestrictedDirectory) {
  FakeTreeBuilder builder;
  builder.setFile("restricted/secret.txt", "secret");
  builder.setDirIsRestricted("restricted");
  TestMount testMount{builder};

  auto edenMount = testMount.getEdenMount();
  auto objectStore = edenMount->getObjectStore();
  auto context = ObjectFetchContext::getNullContext();
  auto checkoutTime = edenMount->getLastCheckoutTime().toTimespec();
  auto attrs = ENTRY_ATTRIBUTE_DIGEST_HASH | ENTRY_ATTRIBUTE_DIGEST_SIZE;

  auto restricted = testMount.getVirtualInode("restricted"_relpath);
  CO_ASSERT_EQ(
      VirtualInode::ContainedType::Tree, restricted.testGetContainedType());
  CO_ASSERT_TRUE(restricted.isRestricted());
  auto restrictedId = restricted.getObjectId();
  CO_ASSERT_TRUE(restrictedId.has_value());

  Hash32 digest;
  testMount.getBackingStore()->putTreeAuxData(
      *restrictedId, std::make_shared<TreeAuxData>(digest, 1234));

  auto syncResult = restricted.tryGetEntryAttributesSync(
      attrs, "restricted"_relpath, objectStore, checkoutTime, context);
  CO_ASSERT_TRUE(syncResult.has_value());
  EXPECT_FALSE(syncResult->digestHash.has_value());
  EXPECT_FALSE(syncResult->digestSize.has_value());

  auto coroResult = co_await restricted.co_getEntryAttributes(
      attrs, "restricted"_relpath, objectStore, checkoutTime, context);
  EXPECT_FALSE(coroResult.digestSize.has_value());
  EXPECT_FALSE(coroResult.digestHash.has_value());

  auto futureResult =
      restricted
          .getEntryAttributes(
              attrs, "restricted"_relpath, objectStore, checkoutTime, context)
          .get();
  EXPECT_FALSE(futureResult.digestSize.has_value());
  EXPECT_FALSE(futureResult.digestHash.has_value());
}

CO_TEST(RestrictedTreeInode, asyncDigestOfRestrictedDirectoryWithoutAuxData) {
  FakeTreeBuilder builder;
  builder.setFile("restricted/secret.txt", "secret");
  builder.setDirIsRestricted("restricted");
  TestMount testMount{builder};

  auto edenMount = testMount.getEdenMount();
  auto restricted = testMount.getVirtualInode("restricted"_relpath);
  CO_ASSERT_TRUE(restricted.isRestricted());

  auto coroResult = co_await restricted.co_getEntryAttributes(
      ENTRY_ATTRIBUTE_DIGEST_HASH,
      "restricted"_relpath,
      edenMount->getObjectStore(),
      edenMount->getLastCheckoutTime().toTimespec(),
      ObjectFetchContext::getNullContext());
  EXPECT_FALSE(coroResult.digestHash.has_value());
}
