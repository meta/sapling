/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include <folly/portability/Unistd.h>
#include <folly/synchronization/Baton.h>
#include <folly/test/TestUtils.h>
#include <gtest/gtest.h>
#include <atomic>
#include <csignal>
#include <cstdio>
#include <thread>

#include "eden/common/utils/ImmediateFuture.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/FileInode.h"
#include "eden/fs/inodes/InodeTable.h"
#include "eden/fs/inodes/Overlay.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/journal/Journal.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/testharness/FakeTreeBuilder.h"
#include "eden/fs/testharness/TestChecks.h"
#include "eden/fs/testharness/TestMount.h"

using namespace facebook::eden;
using namespace std::literals::chrono_literals;
using folly::StringPiece;

class UnlinkTest : public ::testing::Test {
 protected:
  void SetUp() override {
    // Set up a directory structure that we will use for most
    // of the tests below
    FakeTreeBuilder builder;
    builder.setFiles({
        {"dir/a.txt", "This is a.txt.\n"},
        {"dir/b.txt", "This is b.txt.\n"},
        {"dir/c.txt", "This is c.txt.\n"},
        {"readme.txt", "File in the root directory.\n"},
    });
    mount_.initialize(builder);
  }

  TestMount mount_;
};

TEST_F(UnlinkTest, enoent) {
  auto dir = mount_.getTreeInode("dir");
  auto unlinkFuture = dir->unlink(
                             "notpresent.txt"_pc,
                             InvalidationRequired::No,
                             ObjectFetchContext::getNullContext())
                          .semi()
                          .via(mount_.getServerExecutor().get());
  mount_.drainServerExecutor();
  ASSERT_TRUE(unlinkFuture.isReady());
  EXPECT_THROW_ERRNO(std::move(unlinkFuture).get(0ms), ENOENT);
}

TEST_F(UnlinkTest, notLoaded) {
  auto dir = mount_.getTreeInode("dir");
  auto childPath = "a.txt"_pc;

  // Remove the child when it has not been loaded yet.
  auto unlinkFuture = dir->unlink(
                             childPath,
                             InvalidationRequired::No,
                             ObjectFetchContext::getNullContext())
                          .semi()
                          .via(mount_.getServerExecutor().get());
  mount_.drainServerExecutor();
  ASSERT_TRUE(unlinkFuture.isReady());
  std::move(unlinkFuture).get(0ms);

  EXPECT_THROW_ERRNO(dir->getChildInodeNumber(childPath), ENOENT);
}

TEST_F(UnlinkTest, inodeAssigned) {
  auto dir = mount_.getTreeInode("dir");
  auto childPath = "a.txt"_pc;

  // Assign an inode number to the child without loading it.
  dir->getChildInodeNumber(childPath);
  auto unlinkFuture = dir->unlink(
                             childPath,
                             InvalidationRequired::No,
                             ObjectFetchContext::getNullContext())
                          .semi()
                          .via(mount_.getServerExecutor().get());
  mount_.drainServerExecutor();
  ASSERT_TRUE(unlinkFuture.isReady());
  std::move(unlinkFuture).get(0ms);

  EXPECT_THROW_ERRNO(dir->getChildInodeNumber(childPath), ENOENT);
}

TEST_F(UnlinkTest, loaded) {
  auto dir = mount_.getTreeInode("dir");
  auto childPath = "a.txt"_pc;

  // Load the child before removing it
  auto file = mount_.getFileInode("dir/a.txt");
  EXPECT_EQ(file->getNodeId(), dir->getChildInodeNumber(childPath));
  auto unlinkFuture = dir->unlink(
                             childPath,
                             InvalidationRequired::No,
                             ObjectFetchContext::getNullContext())
                          .semi()
                          .via(mount_.getServerExecutor().get());
  mount_.drainServerExecutor();
  ASSERT_TRUE(unlinkFuture.isReady());
  std::move(unlinkFuture).get(0ms);

  EXPECT_THROW_ERRNO(dir->getChildInodeNumber(childPath), ENOENT);
  // We should still be able to read from the FileInode
  EXPECT_FILE_INODE(file, "This is a.txt.\n", 0644);
}

TEST_F(UnlinkTest, modified) {
  auto dir = mount_.getTreeInode("dir");
  auto childPath = "a.txt"_pc;

  // Modify the child, so it is materialized before we remove it
  auto file = mount_.getFileInode("dir/a.txt");
  EXPECT_EQ(file->getNodeId(), dir->getChildInodeNumber(childPath));
  auto newContents = StringPiece{
      "new contents for the file\n"
      "testing testing\n"
      "123\n"
      "testing testing\n"};
  mount_.overwriteFile("dir/a.txt", newContents);

  // Now remove the child
  auto unlinkFuture = dir->unlink(
                             childPath,
                             InvalidationRequired::No,
                             ObjectFetchContext::getNullContext())
                          .semi()
                          .via(mount_.getServerExecutor().get());
  mount_.drainServerExecutor();
  ASSERT_TRUE(unlinkFuture.isReady());
  std::move(unlinkFuture).get(0ms);

  EXPECT_THROW_ERRNO(dir->getChildInodeNumber(childPath), ENOENT);
#ifndef _WIN32
  // We should still be able to read from the FileInode
  EXPECT_FILE_INODE(file, newContents, 0644);
#endif
}

TEST_F(UnlinkTest, created) {
  auto dir = mount_.getTreeInode("dir");
  auto childPath = "new.txt"_pc;
  auto contents =
      StringPiece{"This is a new file that does not exist in source control\n"};
  mount_.addFile("dir/new.txt", contents);
  auto file = mount_.getFileInode("dir/new.txt");

  // Now remove the child
  auto unlinkFuture = dir->unlink(
                             childPath,
                             InvalidationRequired::No,
                             ObjectFetchContext::getNullContext())
                          .semi()
                          .via(mount_.getServerExecutor().get());
  mount_.drainServerExecutor();
  ASSERT_TRUE(unlinkFuture.isReady());
  std::move(unlinkFuture).get(0ms);

  EXPECT_THROW_ERRNO(dir->getChildInodeNumber(childPath), ENOENT);
#ifndef _WIN32
  // We should still be able to read from the FileInode
  EXPECT_FILE_INODE(file, contents, 0644);
#endif
}

#ifndef _WIN32
namespace {
class RemoveLookupContext final : public ObjectFetchContext {
 public:
  Cause getCause() const override {
    // rlockGetOrFindChild pins the loaded child before logging this access.
    if (!lookupPosted_.exchange(true)) {
      lookup.post();
    }
    return Cause::Prefetch;
  }

  const std::unordered_map<std::string, std::string>* getRequestInfo()
      const override {
    return nullptr;
  }

  mutable folly::Baton<> lookup;

 private:
  mutable std::atomic<bool> lookupPosted_{false};
};

[[noreturn]] void removeReplacedChild(bool directory) {
  // Setup failures must not be mistaken for the expected retry deadlock.
  alarm(30);
  FakeTreeBuilder builder;
  if (directory) {
    builder.mkdir("parent/victim");
  } else {
    builder.setFile("parent/victim", "contents\n");
  }
  TestMount mount{builder};
  auto parent = mount.getTreeInode("parent");
  auto original = mount.getInode("parent/victim"_relpath);
  auto context = makeRefPtr<RemoveLookupContext>();
  auto renameLock = mount.getEdenMount()->acquireRenameLock();
  folly::Try<folly::Unit> result;
  std::thread remover([&] {
    result = folly::makeTryWith([&] {
      auto future = directory ? parent->rmdir(
                                    "victim"_pc,
                                    InvalidationRequired::No,
                                    context.as<ObjectFetchContext>())
                              : parent->unlink(
                                    "victim"_pc,
                                    InvalidationRequired::No,
                                    context.as<ObjectFetchContext>());
      std::move(future).get(1s);
      return folly::unit;
    });
  });
  if (!context->lookup.try_wait_for(10s)) {
    _exit(1);
  }

  {
    auto contents = parent->lockContentsWrite();
    auto& entry = contents->entries.at("victim"_pc);
    auto replacement = DirEntry{
        entry.getInitialMode(),
        mount.getEdenMount()->getOverlay()->allocateInodeNumber(),
        entry.getObjectId()};
    original->markUnlinked(parent.get(), "victim"_pc, renameLock);
    entry = std::move(replacement);
  }

  std::fputs("ready replacement installed\n", stderr);
  std::signal(SIGALRM, [](int) { _exit(2); });
  alarm(5);
  renameLock.unlock();
  remover.join();
  _exit(result.hasValue() ? 0 : 1);
}

void expectRemoveReplacedChild(bool directory) {
  GTEST_FLAG_SET(death_test_style, "threadsafe");
  // Debug/sanitized ImmediateFuture always defers, so the outer guard is gone
  // before retry. Only immediate continuations reproduce the self-deadlock.
  ASSERT_EXIT(
      removeReplacedChild(directory),
      ::testing::ExitedWithCode(detail::kImmediateFutureAlwaysDefer ? 0 : 2),
      "ready replacement installed");
}
} // namespace

TEST(RemoveReplacedChild, unlinkReloadsReadyFile) {
  expectRemoveReplacedChild(false);
}

TEST(RemoveReplacedChild, rmdirReloadsReadyDirectory) {
  expectRemoveReplacedChild(true);
}
#endif

class RemoveRecursivelyTest : public ::testing::Test {
 protected:
  void SetUp() override {
    FakeTreeBuilder builder;
    builder.setFiles({
        {"dir/a.txt", "This is a.txt.\n"},
        {"dir/keep.txt", "This is keep.txt.\n"},
        {"other/sub/b.txt", "This is b.txt.\n"},
        {"readme.txt", "File in the root directory.\n"},
    });
    mount_.initialize(builder);
  }

  void removeRecursively(const TreeInodePtr& dir, PathComponentPiece name) {
    auto future = dir->removeRecursively(
                         name,
                         InvalidationRequired::No,
                         ObjectFetchContext::getNullContext())
                      .semi()
                      .via(mount_.getServerExecutor().get());
    mount_.drainServerExecutor();
    std::move(future).get(0ms);
  }

  bool journalRecordsRemoval(
      JournalDelta::SequenceNumber since,
      RelativePathPiece path) {
    auto delta = mount_.getEdenMount()->getJournal().accumulateRange(since);
    if (!delta) {
      return false;
    }
    auto it = delta->changedFilesInOverlay.find(RelativePath{path});
    return it != delta->changedFilesInOverlay.end() &&
        it->second.existedBefore && !it->second.existedAfter;
  }

  TestMount mount_;
};

// Removing a child whose inode is not loaded takes a fast path in
// TreeInode::tryRemoveUnloadedChild. Like the loaded path it must
// materialize the parent, so the removal survives a reload, and record the
// removal in the journal.
TEST_F(RemoveRecursivelyTest, unloadedFileInUnmaterializedDir) {
  auto& journal = mount_.getEdenMount()->getJournal();
  auto testStart = journal.observeLatest().value().sequenceID;

  auto dir = mount_.getTreeInode("dir");
  removeRecursively(dir, "a.txt"_pc);
  EXPECT_THROW_ERRNO(dir->getChildInodeNumber("a.txt"_pc), ENOENT);
  EXPECT_TRUE(dir->getContentsUnchecked().rlock()->isMaterialized());
  EXPECT_TRUE(journalRecordsRemoval(testStart, "dir/a.txt"_relpath));
  dir.reset();

  // On Windows the overlay is reconciled with the on-disk PrjFS state on
  // every start, and a materialized directory that is missing from disk is
  // reset to its source control tree. TestMount puts nothing on disk, so a
  // remount cannot show whether the removal was persisted there.
#ifndef _WIN32
  mount_.remount();
  EXPECT_FALSE(mount_.hasFileAt("dir/a.txt"));
  EXPECT_TRUE(mount_.hasFileAt("dir/keep.txt"));
  EXPECT_TRUE(mount_.getTreeInode("dir")
                  ->getContentsUnchecked()
                  .rlock()
                  ->isMaterialized());
#endif
}

TEST_F(RemoveRecursivelyTest, unloadedDirInUnmaterializedDir) {
  auto& journal = mount_.getEdenMount()->getJournal();
  auto testStart = journal.observeLatest().value().sequenceID;

  auto dir = mount_.getTreeInode("other");
  removeRecursively(dir, "sub"_pc);
  EXPECT_THROW_ERRNO(dir->getChildInodeNumber("sub"_pc), ENOENT);
  EXPECT_TRUE(dir->getContentsUnchecked().rlock()->isMaterialized());
  EXPECT_TRUE(journalRecordsRemoval(testStart, "other/sub"_relpath));
  dir.reset();

  // The remount is guarded for the reason given in
  // unloadedFileInUnmaterializedDir.
#ifndef _WIN32
  mount_.remount();
  EXPECT_FALSE(mount_.hasFileAt("other/sub/b.txt"));
  EXPECT_THROW_ERRNO(mount_.getTreeInode("other/sub"), ENOENT);
  EXPECT_TRUE(mount_.getTreeInode("other")
                  ->getContentsUnchecked()
                  .rlock()
                  ->isMaterialized());
#endif
}

// Removing an unloaded child can race with an in-flight load of that same
// child: the fast path in TreeInode::tryRemoveUnloadedChild erases the entry
// while the load is still running. If the name is used again before the load
// finishes, TreeInode::inodeLoadComplete must not attach the inode it just
// loaded to the unrelated entry that now holds the name.
TEST(RemoveDuringLoadTest, loadFinishingAfterRemovalDoesNotClobberNewEntry) {
  FakeTreeBuilder builder;
  builder.setFile("dir/a.txt", "This is a.txt.\n");
  TestMount mount{builder, /*startReady=*/false};
  auto context = ObjectFetchContext::getNullContext();

  auto root = mount.getEdenMount()->getRootInode();
  auto loadFuture = root->getOrLoadChild("dir"_pc, context)
                        .semi()
                        .via(mount.getServerExecutor().get());
  mount.drainServerExecutor();
  ASSERT_FALSE(loadFuture.isReady());

  auto removeFuture =
      root->removeRecursively("dir"_pc, InvalidationRequired::No, context)
          .semi()
          .via(mount.getServerExecutor().get());
  mount.drainServerExecutor();
  std::move(removeFuture).get(0ms);

  auto recreated =
      root->mkdir("dir"_pc, S_IFDIR | 0755, InvalidationRequired::No);
  auto recreatedNumber = recreated->getNodeId();
  recreated.reset();

  builder.setReady("dir");
  mount.drainServerExecutor();
  ASSERT_TRUE(loadFuture.isReady());

  EXPECT_THROW_ERRNO(std::move(loadFuture).get(0ms), ENOENT);
  EXPECT_EQ(recreatedNumber, mount.getTreeInode("dir")->getNodeId());
}

#ifndef _WIN32
// Clearing a directory must leave the overlay state of a loaded child alone.
// The child can still be in use, by another thread or by the kernel, and like
// every other removal path it frees its own overlay state when it is
// unloaded.
TEST(RemoveAllChildrenTest, loadedChildKeepsItsOverlayStateUntilUnloaded) {
  FakeTreeBuilder builder;
  builder.setFile("dir/sub/file.txt", "This is file.txt.\n");
  TestMount mount{builder};
  mount.overwriteFile("dir/sub/file.txt", "This is the new file.txt.\n");

  auto dir = mount.getTreeInode("dir");
  auto sub = mount.getTreeInode("dir/sub");
  auto subNumber = sub->getNodeId();
  auto* metadata = mount.getEdenMount()->getInodeMetadataTable();
  auto* overlay = mount.getEdenMount()->getOverlay();
  ASSERT_TRUE(overlay->hasOverlayDir(subNumber));
  ASSERT_TRUE(metadata->getOptional(subNumber).has_value());

  {
    auto renameLock = mount.getEdenMount()->acquireRenameLock();
    dir->removeAllChildrenRecursively(
        InvalidationRequired::No,
        ObjectFetchContext::getNullContext(),
        renameLock);
  }

  EXPECT_TRUE(overlay->hasOverlayDir(subNumber));
  EXPECT_TRUE(metadata->getOptional(subNumber).has_value());
  EXPECT_NO_THROW(sub->getMetadata());

  sub.reset();
  // The overlay data is removed on the overlay's GC thread.
  overlay->flushPendingAsync().get();
  EXPECT_FALSE(overlay->hasOverlayDir(subNumber));
  EXPECT_FALSE(metadata->getOptional(subNumber).has_value());
}

// The same holds for a loaded file: it stays readable, and its overlay file
// and metadata record are freed only when it is unloaded.
TEST(RemoveAllChildrenTest, loadedFileKeepsItsOverlayStateUntilUnloaded) {
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "This is file.txt.\n");
  TestMount mount{builder};
  mount.overwriteFile("dir/file.txt", "This is the new file.txt.\n");

  auto dir = mount.getTreeInode("dir");
  auto file = mount.getFileInode("dir/file.txt");
  auto fileNumber = file->getNodeId();
  auto* metadata = mount.getEdenMount()->getInodeMetadataTable();
  auto* overlay = mount.getEdenMount()->getOverlay();
  ASSERT_TRUE(overlay->hasOverlayFile(fileNumber));
  ASSERT_TRUE(metadata->getOptional(fileNumber).has_value());

  {
    auto renameLock = mount.getEdenMount()->acquireRenameLock();
    dir->removeAllChildrenRecursively(
        InvalidationRequired::No,
        ObjectFetchContext::getNullContext(),
        renameLock);
  }

  EXPECT_TRUE(overlay->hasOverlayFile(fileNumber));
  EXPECT_TRUE(metadata->getOptional(fileNumber).has_value());
  EXPECT_FILE_INODE(file, "This is the new file.txt.\n", 0644);

  file.reset();
  overlay->flushPendingAsync().get();
  EXPECT_FALSE(overlay->hasOverlayFile(fileNumber));
  EXPECT_FALSE(metadata->getOptional(fileNumber).has_value());
}
#endif

// TODO: It would be nice to adds some tests for concurrent load+unlink
// However, loading a FileInode does not wait for the file data to be loaded
// from the ObjectStore, so we currently don't have a good way to test
// various interleavings of the two operations.

// TODO
// - concurrent rename+unlink.  We can block the rename on the destination
//   directory load.  This doesn't really test all corner cases, but is better
//   than nothing.

// TODO rmdir tests:
//
// not empty
//
// not present
// not materialized, completely unloaded
// not materialized, inode assigned
// not materialized, loaded
// materialized, does not exist in source control
// materialized, modified from source control
//
// async:
// - concurrent load+rmdir
// - concurrent rename+rmdir
// - concurrent rmdir+rmdir
//
// - concurrent rename+rmdir+rmdir:
//   1. make sure a/b/c/ is not ready yet.
//   2. start rename(a/b/c --> other_dir/c)
//   3. start rmdir(a/b/c)
//   4. start rmdir(a/b/c)
//   5. make a/b/c ready
//
// - concurrent rename+rmdir+rmdir:
//   1. make sure neither a/b nor a/b/c/ are ready yet.
//   2. start rename(a/b/c --> other_dir/c).then(rmdir a/b)
//   3. start rmdir(a/b/c)
//   4. make a/b/c ready
//   This should hopefully trigger the rmdir(a/b) to succeed before
//   rmdir(a/b/c) completes.
//
// - attempt to create child in subdir after rmdir
// - attempt to mkdir child in subdir after rmdir
