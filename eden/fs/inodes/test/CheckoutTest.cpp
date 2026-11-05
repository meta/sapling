/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include <folly/Conv.h>
#include <folly/ScopeGuard.h>
#include <folly/chrono/Conv.h>
#include <folly/container/Array.h>
#include <folly/coro/GtestHelpers.h>
#include <folly/coro/Task.h>
#include <folly/coro/Timeout.h>
#include <folly/executors/ManualExecutor.h>
#include <folly/portability/Unistd.h>
#include <folly/test/TestUtils.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <csignal>

#include "eden/common/utils/FaultInjector.h"
#include "eden/common/utils/FileUtils.h"
#include "eden/common/utils/StatTimes.h"
#include "eden/common/utils/TimeUtil.h"
#include "eden/fs/config/CheckoutConfig.h"
#include "eden/fs/inodes/EdenDispatcherFactory.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/FileInode.h"
#include "eden/fs/inodes/InodeMap.h"
#include "eden/fs/inodes/InodeTable.h"
#include "eden/fs/inodes/Overlay.h"
#include "eden/fs/inodes/ServerState.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/journal/Journal.h"
#include "eden/fs/model/git/TopLevelIgnores.h"
#ifdef _WIN32
#include "eden/fs/prjfs/PrjfsChannel.h"
#endif
#include "eden/fs/service/PrettyPrinters.h"
#include "eden/fs/service/gen-cpp2/eden_types.h"
#include "eden/fs/store/DiffContext.h"
#include "eden/fs/store/IObjectStore.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/store/ObjectStore.h"
#include "eden/fs/store/ScmStatusDiffCallback.h"
#include "eden/fs/store/TreeCache.h"
#include "eden/fs/testharness/FakeBackingStore.h"
#include "eden/fs/testharness/FakeTreeBuilder.h"
#include "eden/fs/testharness/InodeUnloader.h"
#include "eden/fs/testharness/TestChecks.h"
#include "eden/fs/testharness/TestMount.h"
#include "eden/fs/utils/EdenError.h"

using namespace facebook::eden;
using namespace std::chrono_literals;
using folly::StringPiece;
using std::optional;
using std::string;
using testing::UnorderedElementsAre;

namespace std {
template <typename Rep, typename Period>
inline void PrintTo(
    std::chrono::duration<Rep, Period> duration,
    ::std::ostream* os) {
  *os << facebook::eden::durationStr(duration);
}

inline void PrintTo(
    const std::chrono::system_clock::time_point& t,
    ::std::ostream* os) {
  auto ts = folly::to<struct timespec>(t);

  struct tm localTime;
  if (!localtime_r(&ts.tv_sec, &localTime)) {
    folly::throwSystemError("localtime_r failed");
  }

  std::array<char, 64> buf;
  if (strftime(buf.data(), buf.size(), "%FT%T", &localTime) == 0) {
    // errno is not necessarily set
    throw std::runtime_error("strftime failed");
  }

  *os << buf.data() << fmt::format(".{:09d}", ts.tv_nsec);
}
} // namespace std

/**
 * Parameterized fixture: runs every checkout test once with the futures
 * implementation (`useCoroutines=false`) and once with the coroutine
 * implementation (`useCoroutines=true`). Mirrors the pattern in
 * `DiffTestParam`.
 *
 * Tests opt into the coroutine variant for any `TestMount` they
 * construct by calling `applyParam(mount)` immediately after
 * construction (free helpers take a `bool useCoroutines` and call
 * `enableCoroutinesConfig(mount)` themselves — same shape as
 * `DiffTest`'s `useCoroutines_` constructor parameter).
 */
class CheckoutTest : public ::testing::TestWithParam<bool> {
 protected:
  /// Apply the parameterized coroutine variant config (if `GetParam()`)
  /// to the given mount via the existing `enableCoroutinesConfig`
  /// bundle (which includes `enable-phase7`).
  void applyParam(TestMount& mount) const {
    if (GetParam()) {
      enableCoroutinesConfig(mount);
    }
  }
};

namespace {

bool isExecutable([[maybe_unused]] int perms) {
#ifndef _WIN32
  return perms & S_IXUSR;
#else
  // Windows doesn't support the notion of executable files.
  return false;
#endif
}

/**
 * An enum to control behavior for many of the checkout tests.
 *
 * Whether or not inodes are loaded when checkout runs affects which code
 * paths we hit, but it should not affect the user-visible behavior.
 */
enum class LoadBehavior {
  // None of the inodes in question are explicitly loaded
  // before the checkout operation.
  NONE,
  // Assign an inode number for the parent directory, but do not load it yet.
  ASSIGN_PARENT_INODE,
  // Load the parent TreeInode object before starting the checkout.
  PARENT,
  // Load the parent TreeInode object, and assign an inode number to the
  // child in question, but do not load the child InodeBase.
  ASSIGN_INODE,
  // Load the InodeBase affected by the test before starting the checkout.
  INODE,
  // Walk the tree and load every inode
  ALL,
};

static constexpr auto kAllLoadTypes = folly::make_array(
    LoadBehavior::NONE,
    LoadBehavior::ASSIGN_PARENT_INODE,
    LoadBehavior::PARENT,
    LoadBehavior::ASSIGN_INODE,
    LoadBehavior::INODE,
    LoadBehavior::ALL);

// LoadTypes that can be used with tests that add a new file
static constexpr auto kAddLoadTypes = folly::make_array(
    LoadBehavior::NONE,
    LoadBehavior::ASSIGN_PARENT_INODE,
    LoadBehavior::PARENT,
    LoadBehavior::ALL);

} // namespace

template <>
struct fmt::formatter<LoadBehavior> : formatter<string_view> {
  template <typename FormatContext>
  auto format(LoadBehavior loadType, FormatContext& ctx) const {
    switch (loadType) {
      case LoadBehavior::NONE:
        return formatter<string_view>::format("NONE", ctx);
      case LoadBehavior::ASSIGN_PARENT_INODE:
        return formatter<string_view>::format("ASSIGN_PARENT_INODE", ctx);
      case LoadBehavior::PARENT:
        return formatter<string_view>::format("PARENT", ctx);
      case LoadBehavior::ASSIGN_INODE:
        return formatter<string_view>::format("ASSIGN_INODE", ctx);
      case LoadBehavior::INODE:
        return formatter<string_view>::format("INODE", ctx);
      case LoadBehavior::ALL:
        return formatter<string_view>::format("ALL", ctx);
    }
    return fmt::format_to(
        ctx.out(), "<unknown LoadBehavior {}>", static_cast<int>(loadType));
  }
};

namespace {

void loadInodes(
    TestMount& testMount,
    RelativePathPiece path,
    LoadBehavior loadType,
    std::optional<folly::StringPiece> expectedContents,
    [[maybe_unused]] mode_t expectedPerms) {
  switch (loadType) {
    case LoadBehavior::NONE:
      return;
    case LoadBehavior::ASSIGN_PARENT_INODE: {
      // Load the parent TreeInode but not the affected file
      testMount.getTreeInode(path.dirname());
      auto parentPath = path.dirname();
      auto grandparentInode = testMount.getTreeInode(parentPath.dirname());
      grandparentInode->getChildInodeNumber(parentPath.basename());
      return;
    }
    case LoadBehavior::PARENT:
      // Load the parent TreeInode but not the affected file
      testMount.getTreeInode(path.dirname());
      return;
    case LoadBehavior::ASSIGN_INODE: {
      auto parent = testMount.getTreeInode(path.dirname());
      parent->getChildInodeNumber(path.basename());
      return;
    }
    case LoadBehavior::INODE: {
      if (expectedContents.has_value()) {
        // The inode in question must be a file.  Load it and verify the
        // contents are what we expect.
        auto fileInode = testMount.getFileInode(path);
        EXPECT_FILE_INODE(fileInode, expectedContents.value(), expectedPerms);
      } else {
        // The inode might be a tree or a file.
        testMount.getInode(path);
      }
      return;
    }
    case LoadBehavior::ALL:
      testMount.loadAllInodes();
      return;
  }

  FAIL() << fmt::format("unknown load behavior: {}", loadType);
}

void loadInodes(
    TestMount& testMount,
    folly::StringPiece path,
    LoadBehavior loadType,
    folly::StringPiece expectedContents,
    mode_t expectedPerms = 0644) {
  loadInodes(
      testMount,
      RelativePathPiece{path},
      loadType,
      expectedContents,
      expectedPerms);
}

void loadInodes(
    TestMount& testMount,
    RelativePathPiece path,
    LoadBehavior loadType) {
  loadInodes(testMount, path, loadType, std::nullopt, 0644);
}

void loadInodes(
    TestMount& testMount,
    folly::StringPiece path,
    LoadBehavior loadType) {
  loadInodes(testMount, RelativePathPiece{path}, loadType, std::nullopt, 0644);
}

CheckoutConflict makeConflict(
    ConflictType type,
    StringPiece path,
    StringPiece message = "",
    Dtype dtype = Dtype::UNKNOWN) {
  CheckoutConflict conflict;
  conflict.type() = type;
  conflict.path() = path.str();
  conflict.message() = message.str();
  conflict.dtype() = dtype;
  return conflict;
}

void checkFileChangeJournalEntries(
    std::vector<FileChangeJournalDelta>& expected_journal,
    TestMount& mount) {
  std::vector<bool> results;
  size_t i = 0;
  mount.getEdenMount()->getJournal().forEachDelta(
      1,
      std::nullopt,
      [&](const FileChangeJournalDelta& current) -> bool {
        results.push_back(expected_journal[i].isSameAction(current));
        i++;
        return i < expected_journal.size();
      },
      [&](const RootUpdateJournalDelta& /*_current*/) -> bool { return true; });

  for (bool journal_result : results) {
    EXPECT_TRUE(journal_result);
  }
}

void testAddFile(
    folly::StringPiece newFilePath,
    LoadBehavior loadType,
    int perms = 0644,
    bool useCoroutines = false) {
  auto builder1 = FakeTreeBuilder();
  builder1.setFile("src/main.c", "int main() { return 0; }\n");
  builder1.setFile("src/test/test.c", "testy tests");
  TestMount testMount{builder1};
  if (useCoroutines) {
    enableCoroutinesConfig(testMount);
  }

  // Prepare a second tree, by starting with builder1 then adding the new file
  auto builder2 = builder1.clone();
  builder2.setFile(
      newFilePath, "this is the new file contents\n", isExecutable(perms));
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit(RootId{"2"}, builder2);
  commit2->setReady();

  loadInodes(testMount, newFilePath, loadType);
  testMount.drainServerExecutor();

  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"2"},
                                ObjectFetchContext::getNullContext(),
                                __func__)
                            .semi()
                            .via(executor);
  testMount.drainServerExecutor();
  ASSERT_TRUE(checkoutResult.isReady());
  auto result = std::move(checkoutResult).get();
  EXPECT_EQ(0, result.conflicts.size());

  // Confirm that the tree has been updated correctly.
  auto newInode = testMount.getFileInode(newFilePath);
  EXPECT_FILE_INODE(newInode, "this is the new file contents\n", perms);

  // Unmount and remount the mount point, and verify that the new file
  // still exists as expected.
  newInode.reset();
  testMount.remount();
  newInode = testMount.getFileInode(newFilePath);
  EXPECT_FILE_INODE(newInode, "this is the new file contents\n", perms);
}

void runAddFileTests(folly::StringPiece path, bool useCoroutines) {
  for (auto loadType : kAddLoadTypes) {
    SCOPED_TRACE(fmt::format("add {} load type {}", path, loadType));
    testAddFile(path, loadType, /*perms=*/0644, useCoroutines);
    testAddFile(path, loadType, 0755, useCoroutines);
  }
}

TEST_P(CheckoutTest, addFile) {
  // Test with file names that will be at the beginning of the directory,
  // in the middle of the directory, and at the end of the directory.
  // (The directory entries are processed in sorted order.)
  runAddFileTests("src/aaa.c", GetParam());
  runAddFileTests("src/ppp.c", GetParam());
  runAddFileTests("src/zzz.c", GetParam());
}

void testRemoveFile(
    folly::StringPiece filePath,
    LoadBehavior loadType,
    bool useCoroutines) {
  auto builder1 = FakeTreeBuilder();
  builder1.setFile("src/main.c", "int main() { return 0; }\n");
  builder1.setFile("src/test/test.c", "testy tests");
  builder1.setFile(filePath, "this file will be removed\n");
  TestMount testMount{builder1};
  if (useCoroutines) {
    enableCoroutinesConfig(testMount);
  }

  // Prepare a second tree, by starting with builder1 then removing the desired
  // file
  auto builder2 = builder1.clone();
  builder2.removeFile(filePath);
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit(RootId{"2"}, builder2);
  commit2->setReady();

  loadInodes(testMount, filePath, loadType, "this file will be removed\n");

  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"2"},
                                ObjectFetchContext::getNullContext(),
                                __func__)
                            .semi()
                            .via(executor)
                            .waitVia(executor);
  ASSERT_TRUE(checkoutResult.isReady());
  auto result = std::move(checkoutResult).get();
  EXPECT_EQ(0, result.conflicts.size());

  // Make sure the path doesn't exist any more.
  EXPECT_THROW_ERRNO(testMount.getInode(filePath), ENOENT);

  // Unmount and remount the mount point, and verify that the file removal
  // persisted across remount correctly.
  testMount.remount();
  EXPECT_THROW_ERRNO(testMount.getInode(filePath), ENOENT);
}

void runRemoveFileTests(folly::StringPiece path, bool useCoroutines) {
  // Modify just the file contents, but not the permissions
  for (auto loadType : kAllLoadTypes) {
    SCOPED_TRACE(fmt::format("remove {} load type {}", path, loadType));
    testRemoveFile(path, loadType, useCoroutines);
  }
}

// A file renamed within a directory keeps its object id. Checkout must not
// dematerialize the directory to a tree that holds that object under the old
// name: the directory would then claim to equal the tree, hiding the rename
// from status and from later checkouts.
TEST_P(CheckoutTest, renamedFileKeepsDirectoryMaterialized) {
  FakeTreeBuilder builder1;
  builder1.setFile("d/keep.txt", "k\n");
  builder1.setFile("d/x.txt", "same\n");
  TestMount testMount{builder1};
  applyParam(testMount);

  // Commit 2 changes the directory so checkout has to walk it.
  auto builder2 = builder1.clone();
  builder2.replaceFile("d/keep.txt", "k2\n");
  builder2.finalize(testMount.getBackingStore(), true);
  testMount.getBackingStore()->putCommit(RootId{"2"}, builder2)->setReady();

  auto dir = testMount.getTreeInode("d");
  dir->rename(
         "x.txt"_pc,
         dir,
         "y.txt"_pc,
         InvalidationRequired::No,
         ObjectFetchContext::getNullContext())
      .get();

  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"2"},
                                ObjectFetchContext::getNullContext(),
                                __func__)
                            .semi()
                            .via(executor);
  testMount.drainServerExecutor();
  ASSERT_TRUE(checkoutResult.isReady());
  EXPECT_EQ(0, std::move(checkoutResult).get().conflicts.size());

  EXPECT_EQ("k2\n", testMount.readFile("d/keep.txt"));
  EXPECT_TRUE(testMount.hasFileAt("d/y.txt"));
  EXPECT_FALSE(testMount.hasFileAt("d/x.txt"));

  ScmStatusDiffCallback callback;
  DiffContext diffContext{
      &callback,
      folly::CancellationToken{},
      ObjectFetchContext::getNullContext(),
      /*listIgnored=*/false,
      kPathMapDefaultCaseSensitive,
      testMount.getEdenMount()->getObjectStore(),
      std::make_unique<TopLevelIgnores>("", "")};
  auto rootTree =
      testMount.getEdenMount()
          ->getObjectStore()
          ->getRootTree(RootId{"2"}, ObjectFetchContext::getNullContext())
          .semi()
          .via(executor);
  testMount.drainServerExecutor();
  std::vector<std::shared_ptr<const Tree>> trees{
      std::move(rootTree).get().tree};
  auto diffFuture = testMount.getRootInode()
                        ->diff(
                            &diffContext,
                            RelativePathPiece{},
                            std::move(trees),
                            diffContext.getToplevelIgnore(),
                            false)
                        .semi()
                        .via(executor);
  testMount.drainServerExecutor();
  std::move(diffFuture).get(0ms);
  auto status = callback.extractStatus();

  EXPECT_TRUE(dir->isMaterialized());
  EXPECT_THAT(
      *status.entries(),
      UnorderedElementsAre(
          std::make_pair("d/x.txt", ScmFileStatus::REMOVED),
          std::make_pair("d/y.txt", ScmFileStatus::ADDED)));
}

#ifndef _WIN32
// Removing an unloaded, unmaterialized file during checkout only erases the
// parent's entry. The file's inode metadata record, created when the file
// was last loaded, has to be freed too or it outlives the inode.
TEST_P(CheckoutTest, removingUnloadedFileFreesInodeMetadata) {
  FakeTreeBuilder builder1;
  builder1.setFile("a.txt", "a\n");
  builder1.setFile("keep.txt", "k\n");
  TestMount testMount{builder1};
  applyParam(testMount);

  auto builder2 = builder1.clone();
  builder2.removeFile("a.txt");
  builder2.finalize(testMount.getBackingStore(), true);
  testMount.getBackingStore()->putCommit(RootId{"2"}, builder2)->setReady();

  // Loading the file creates its metadata record; unloading it afterwards
  // keeps the record so a later load sees the same timestamps.
  auto ino = testMount.getFileInode("a.txt")->getNodeId();
  auto* metadata = testMount.getEdenMount()->getInodeMetadataTable();
  ASSERT_TRUE(metadata->getOptional(ino).has_value());
  testMount.getRootInode()->unloadChildrenNow();

  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"2"},
                                ObjectFetchContext::getNullContext(),
                                __func__)
                            .semi()
                            .via(executor);
  testMount.drainServerExecutor();
  ASSERT_TRUE(checkoutResult.isReady());
  EXPECT_EQ(0, std::move(checkoutResult).get().conflicts.size());
  EXPECT_FALSE(testMount.hasFileAt("a.txt"));

  EXPECT_FALSE(metadata->getOptional(ino).has_value());
}
#endif

TEST_P(CheckoutTest, removeFile) {
  // Test with file names that will be at the beginning of the directory,
  // in the middle of the directory, and at the end of the directory.
  // (The directory entries are processed in sorted order.)
  runRemoveFileTests("src/aaa.c", GetParam());
  runRemoveFileTests("src/ppp.c", GetParam());
  runRemoveFileTests("src/zzz.c", GetParam());
}

void testModifyFile(
    folly::StringPiece path,
    LoadBehavior loadType,
    folly::StringPiece contents1,
    int perms1,
    folly::StringPiece contents2,
    int perms2,
    bool useCoroutines) {
  auto builder1 = FakeTreeBuilder();
  builder1.setFile("readme.txt", "just filling out the tree\n");
  builder1.setFile("a/test.txt", "test contents\n");
  builder1.setFile("a/b/dddd.c", "this is dddd.c\n");
  builder1.setFile("a/b/tttt.c", "this is tttt.c\n");
  builder1.setFile(path, contents1, isExecutable(perms1));
  TestMount testMount{builder1};
  if (useCoroutines) {
    enableCoroutinesConfig(testMount);
  }
  testMount.getClock().advance(9876min);

  // Prepare the second tree
  auto builder2 = builder1.clone();
  builder2.replaceFile(path, contents2, isExecutable(perms2));
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit("2", builder2);
  commit2->setReady();

  loadInodes(testMount, path, loadType, contents1, perms1);

  optional<struct stat> preStat;
  // If we were supposed to load this inode before the checkout,
  // also store its stat information so we can compare it after the checkout.
  if (loadType == LoadBehavior::INODE || loadType == LoadBehavior::ALL) {
    auto preInode = testMount.getFileInode(path);
    auto st = preInode->stat(ObjectFetchContext::getNullContext()).get(10ms);
    EXPECT_EQ(st.st_size, contents1.size());
    preStat = st;
  }

  testMount.getClock().advance(10min);
  [[maybe_unused]] auto checkoutStart = testMount.getClock().getTimePoint();
  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"2"},
                                ObjectFetchContext::getNullContext(),
                                __func__)
                            .semi()
                            .via(executor)
                            .waitVia(executor);
  ASSERT_TRUE(checkoutResult.isReady());
  auto result = std::move(checkoutResult).get();
  EXPECT_EQ(0, result.conflicts.size());

  // Make sure the path is updated as expected
  auto postInode = testMount.getFileInode(path);
  EXPECT_FILE_INODE(postInode, contents2, perms2);

  // Check the stat() information on the inode.
  auto postStat =
      postInode->stat(ObjectFetchContext::getNullContext()).get(10ms);
  EXPECT_EQ(postStat.st_size, contents2.size());
  // The timestamps should not be earlier than when the checkout started.
  // We don't populate timestamps in the FileInode yet on win32
#ifndef _WIN32
  EXPECT_GE(stAtimepoint(postStat), checkoutStart);
  EXPECT_GE(stMtimepoint(postStat), checkoutStart);
  EXPECT_GE(stCtimepoint(postStat), checkoutStart);
  if (preStat.has_value()) {
    EXPECT_GE(stAtimepoint(postStat), stAtimepoint(*preStat));
    EXPECT_GE(stMtimepoint(postStat), stMtimepoint(*preStat));
    EXPECT_GE(stCtimepoint(postStat), stCtimepoint(*preStat));
  }
#endif

  // Unmount and remount the mount point, and verify that the file changes
  // persisted across remount correctly.
  postInode.reset();
  testMount.remount();
  postInode = testMount.getFileInode(path);
  EXPECT_FILE_INODE(postInode, contents2, perms2);
}

void runModifyFileTests(folly::StringPiece path, bool useCoroutines) {
  // Modify just the file contents, but not the permissions
  for (auto loadType : kAllLoadTypes) {
    SCOPED_TRACE(
        fmt::format("contents change, path {} load type {}", path, loadType));
    testModifyFile(
        path,
        loadType,
        "contents v1",
        0644,
        "updated file contents\nextra stuff\n",
        0644,
        useCoroutines);
  }

  // Modify just the permissions, but not the contents
  for (auto loadType : kAllLoadTypes) {
    SCOPED_TRACE(
        fmt::format("mode change, path {} load type {}", path, loadType));
    testModifyFile(
        path, loadType, "unchanged", 0755, "unchanged", 0644, useCoroutines);
  }

  // Modify the contents and the permissions
  for (auto loadType : kAllLoadTypes) {
    SCOPED_TRACE(
        fmt::format(
            "contents+mode change, path {} load type {}", path, loadType));
    testModifyFile(
        path,
        loadType,
        "contents v1",
        0644,
        "executable contents",
        0755,
        useCoroutines);
  }
}

// Test with file names that will be at the beginning of the directory,
// in the middle of the directory, and at the end of the directory.

TEST_P(CheckoutTest, modifyFileBeginning) {
  runModifyFileTests("a/b/aaa.txt", GetParam());
}

TEST_P(CheckoutTest, modifyFileMiddle) {
  runModifyFileTests("a/b/mmm.txt", GetParam());
}

TEST_P(CheckoutTest, modifyFileEnd) {
  runModifyFileTests("a/b/zzz.txt", GetParam());
}

// Test performing a checkout with a modified file where the ObjectStore data is
// not immediately ready even though the inode is loaded.
TEST_P(CheckoutTest, modifyLoadedButNotReadyFileWithConflict) {
  TestMount mount;
  applyParam(mount);
  auto backingStore = mount.getBackingStore();

  auto builder1 = FakeTreeBuilder();
  StringPiece contents1 = "test contents\n";
  builder1.setFile("a/test.txt", contents1);

  auto builder2 = builder1.clone();
  StringPiece contents2 = "updated contents\n";
  builder2.replaceFile("a/test.txt", contents2);
  builder2.finalize(backingStore, /*setReady=*/true);
  auto commit2 = backingStore->putCommit("2", builder2);
  commit2->setReady();

  auto builder3 = builder1.clone();
  builder3.replaceFile("a/test.txt", "original conflicting contents\n");
  builder3.finalize(backingStore, /*setReady=*/true);
  auto commit3 = backingStore->putCommit("3", builder3);
  commit3->setReady();

  // Initialize the mount with the tree data from builder1
  mount.initialize(builder1, /*startReady=*/false);

  // Load a/test.txt
  auto blob1 = builder1.getStoredBlob("a/test.txt"_relpath);
  builder1.setReady("a");
  blob1->setReady();
  auto preInode = mount.getFileInode("a/test.txt");
  // Mark its blob as not ready again after loading it
  blob1->notReady();

  // Call resetParent() to make the mount point at commit3, even though
  // the file state is from commit1.  This will cause a conflict in a
  // non-materialized file.
  mount.getEdenMount()->resetParent(RootId{"3"});

  // Perform the checkout.
  auto checkoutFuture = mount.getEdenMount()->checkout(
      mount.getRootInode(),
      RootId{"2"},
      ObjectFetchContext::getNullContext(),
      __func__);

  // Trigger blob1 several times to allow the checkout to make forward progress
  // if it needs to access this blob, without necessarily completing all at
  // once.
  for (int n = 0; n < 5; ++n) {
    blob1->trigger();
  }

  // Mark builder1 as ready and confirm that the checkout completes
  builder1.setAllReady();
  auto executor = mount.getServerExecutor().get();
  auto waitedCheckoutFuture =
      std::move(checkoutFuture).semi().via(executor).waitVia(executor);
  ASSERT_TRUE(waitedCheckoutFuture.isReady());
  auto result = std::move(waitedCheckoutFuture).get();
  EXPECT_THAT(
      result.conflicts,
      UnorderedElementsAre(makeConflict(
          ConflictType::MODIFIED_MODIFIED, "a/test.txt", "", Dtype::REGULAR)));

  // Verify that the inode was not updated
  auto postInode = mount.getFileInode("a/test.txt");
  EXPECT_FILE_INODE(postInode, contents1, 0644);
}

void testModifyConflict(
    folly::StringPiece path,
    LoadBehavior loadType,
    CheckoutMode checkoutMode,
    folly::StringPiece contents1,
    int perms1,
    folly::StringPiece currentContents,
    int currentPerms,
    folly::StringPiece contents2,
    int perms2,
    bool useCoroutines) {
  // Prepare the tree to represent the current inode state
  auto workingDirBuilder = FakeTreeBuilder();
  workingDirBuilder.setFile("readme.txt", "just filling out the tree\n");
  workingDirBuilder.setFile("a/test.txt", "test contents\n");
  workingDirBuilder.setFile("a/b/dddd.c", "this is dddd.c\n");
  workingDirBuilder.setFile("a/b/tttt.c", "this is tttt.c\n");
  workingDirBuilder.setFile(path, currentContents, isExecutable(currentPerms));
  TestMount testMount{workingDirBuilder};
  if (useCoroutines) {
    enableCoroutinesConfig(testMount);
  }

  // Prepare the "before" tree
  auto builder1 = workingDirBuilder.clone();
  builder1.replaceFile(path, contents1, isExecutable(perms1));
  builder1.finalize(testMount.getBackingStore(), true);
  // Reset the EdenMount to point at the tree from builder1, even though the
  // contents are still from workingDirBuilder.  This lets us trigger the
  // desired conflicts.
  //
  // TODO: We should also do a test where we start from builder1 then use
  // EdenDispatcher APIs to modify the contents to the "current" state.
  // This will have a different behavior than when using
  // resetCommit(), as the files will be materialized this way.
  auto commit1 = testMount.getBackingStore()->putCommit("a", builder1);
  commit1->setReady();
  testMount.getEdenMount()->resetParent(RootId{"a"});

  // Prepare the destination tree
  auto builder2 = builder1.clone();
  builder2.replaceFile(path, contents2, isExecutable(perms2));
  builder2.replaceFile("a/b/dddd.c", "new dddd contents\n");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit("b", builder2);
  commit2->setReady();

  loadInodes(testMount, path, loadType, currentContents, currentPerms);

  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"b"},
                                ObjectFetchContext::getNullContext(),
                                __func__,
                                checkoutMode)
                            .semi()
                            .via(executor)
                            .waitVia(executor);
  ASSERT_TRUE(checkoutResult.isReady());
  auto result = std::move(checkoutResult).get();
  ASSERT_EQ(1, result.conflicts.size());

  EXPECT_EQ(path, *result.conflicts[0].path());
  EXPECT_EQ(ConflictType::MODIFIED_MODIFIED, *result.conflicts[0].type());

  const auto currentParent = testMount.getEdenMount()->getWorkingCopyParent();
  const auto configParent =
      testMount.getEdenMount()->getCheckoutConfig()->getParentCommit();
  // Make sure both the mount parent and the config parent information was
  // updated
  EXPECT_EQ(currentParent, configParent.getWorkingCopyParent());

  auto postInode = testMount.getFileInode(path);
  switch (checkoutMode) {
    case CheckoutMode::FORCE:
      // Make sure the path is updated as expected
      EXPECT_FILE_INODE(postInode, contents2, perms2);
      // Make sure the parent information has been updated
      EXPECT_EQ(currentParent, RootId{"b"});
      break;
    case CheckoutMode::DRY_RUN:
      // make sure the currentParent is still commit1
      EXPECT_EQ(currentParent, RootId{"a"});
      break;
    case CheckoutMode::NORMAL:
      // Make sure the path has not been changed
      EXPECT_FILE_INODE(postInode, currentContents, currentPerms);
      // Make sure the parent information has been updated
      EXPECT_EQ(currentParent, RootId{"b"});
      break;
  }

    // Unmount and remount the mount point, and verify the changes persisted
    // across the remount as expected.
#ifndef _WIN32
  // A Windows remount rebuilds the InMemory overlay from the ProjFS scan, and
  // TestMount has no projected disk state, so materialized changes revert.
  postInode.reset();
  testMount.remount();
  postInode = testMount.getFileInode(path);
  auto ddddPath = "a/b/dddd.c";
  auto ddddInode = testMount.getFileInode(ddddPath);
  switch (checkoutMode) {
    case CheckoutMode::FORCE:
      EXPECT_FILE_INODE(postInode, contents2, perms2);
      EXPECT_FILE_INODE(ddddInode, "new dddd contents\n", 0644);
      break;
    case CheckoutMode::DRY_RUN:
      EXPECT_FILE_INODE(postInode, currentContents, currentPerms);
      EXPECT_FILE_INODE(ddddInode, "this is dddd.c\n", 0644);
      break;
    case CheckoutMode::NORMAL:
      EXPECT_FILE_INODE(postInode, currentContents, currentPerms);
      EXPECT_FILE_INODE(ddddInode, "new dddd contents\n", 0644);
      break;
  }
#endif
}

void runModifyConflictTests(CheckoutMode checkoutMode, bool useCoroutines) {
  // Try with three separate path names, one that sorts first in the directory,
  // one in the middle, and one that sorts last.  This helps ensure that we
  // exercise all code paths in TreeInode::computeCheckoutActions()
  for (StringPiece path : {"a/b/aaa.txt", "a/b/mmm.txt", "a/b/zzz.tzt"}) {
    for (auto loadType : kAllLoadTypes) {
      SCOPED_TRACE(
          fmt::format(
              "path {} load type {} force={}",
              path,
              loadType,
              static_cast<int>(checkoutMode)));
      testModifyConflict(
          path,
          loadType,
          checkoutMode,
          "orig file contents.txt",
          0644,
          "current file contents.txt",
          0644,
          "new file contents.txt",
          0644,
          useCoroutines);
    }
  }
}

TEST_P(CheckoutTest, modifyConflictNormal) {
  runModifyConflictTests(CheckoutMode::NORMAL, GetParam());
}

TEST_P(CheckoutTest, modifyConflictDryRun) {
  runModifyConflictTests(CheckoutMode::DRY_RUN, GetParam());
}

TEST_P(CheckoutTest, modifyConflictForce) {
  runModifyConflictTests(CheckoutMode::FORCE, GetParam());
}

TEST_P(CheckoutTest, modifyThenRevert) {
  // Prepare a "before" tree
  auto srcBuilder = FakeTreeBuilder();
  srcBuilder.setFile("readme.txt", "just filling out the tree\n");
  srcBuilder.setFile("a/abc.txt", "foo\n");
  srcBuilder.setFile("a/test.txt", "test contents\n");
  srcBuilder.setFile("a/xyz.txt", "bar\n");
  TestMount testMount{srcBuilder};
  applyParam(testMount);
  auto originalCommit = testMount.getEdenMount()->getCheckedOutRootId();

  // Modify a file.
  // We use the "normal" dispatcher APIs here, which will materialize the file.
  testMount.overwriteFile("a/test.txt", "temporary edit\n");

  auto preInode = testMount.getFileInode("a/test.txt");
  EXPECT_FILE_INODE(preInode, "temporary edit\n", 0644);

  // Now perform a forced checkout to the current commit,
  // which should discard our edits.
  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                originalCommit,
                                ObjectFetchContext::getNullContext(),
                                __func__,
                                CheckoutMode::FORCE)
                            .semi()
                            .via(executor)
                            .waitVia(executor);
  ASSERT_TRUE(checkoutResult.isReady());
  // The checkout should report a/test.txt as a conflict
  EXPECT_THAT(
      std::move(checkoutResult).get().conflicts,
      UnorderedElementsAre(makeConflict(
          ConflictType::MODIFIED_MODIFIED, "a/test.txt", "", Dtype::REGULAR)));

  std::vector<FileChangeJournalDelta> expected_journal;
  expected_journal.emplace_back(
      std::forward<FileChangeJournalDelta>(FileChangeJournalDelta(
          RelativePathPiece("a/test.txt"),
          dtype_t::Regular,
          FileChangeJournalDelta::CHANGED)));

  checkFileChangeJournalEntries(expected_journal, testMount);

#ifndef _WIN32
  // The checkout operation updates files by replacing them, so
  // there should be a new inode at this location now, with the original
  // contents. On Windows, files are written directly into the Projected FS
  // cache, and thus the old data is gone.
  auto postInode = testMount.getFileInode("a/test.txt");
  EXPECT_FILE_INODE(postInode, "test contents\n", 0644);
  EXPECT_NE(preInode->getNodeId(), postInode->getNodeId());
  EXPECT_FILE_INODE(preInode, "temporary edit\n", 0644);
#endif
}

TEST_P(CheckoutTest, modifyThenCheckoutRevisionWithoutFile) {
  auto builder1 = FakeTreeBuilder();
  builder1.setFile("src/main.c", "// Some code.\n");
  TestMount testMount{RootId{"1"}, builder1};
  applyParam(testMount);

  auto builder2 = builder1.clone();
  builder2.setFile("src/test.c", "// Unit test.\n");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit("2", builder2);
  commit2->setReady();

  auto executor = testMount.getServerExecutor().get();
  auto checkoutTo2 = testMount.getEdenMount()
                         ->checkout(
                             testMount.getRootInode(),
                             RootId("2"),
                             ObjectFetchContext::getNullContext(),
                             __func__)
                         .semi()
                         .via(executor)
                         .waitVia(executor);
  ASSERT_TRUE(checkoutTo2.isReady());

  testMount.overwriteFile("src/test.c", "temporary edit\n");
  auto checkoutTo1 = testMount.getEdenMount()
                         ->checkout(
                             testMount.getRootInode(),
                             RootId("1"),
                             ObjectFetchContext::getNullContext(),
                             __func__,
                             CheckoutMode::FORCE)
                         .semi()
                         .via(executor)
                         .waitVia(executor);
  ASSERT_TRUE(checkoutTo1.isReady());

  EXPECT_THAT(
      std::move(checkoutTo1).get().conflicts,
      UnorderedElementsAre(makeConflict(
          ConflictType::MODIFIED_REMOVED, "src/test.c", "", Dtype::REGULAR)));

  std::vector<FileChangeJournalDelta> expected_journal;
  expected_journal.emplace_back(
      std::forward<FileChangeJournalDelta>(FileChangeJournalDelta(
          RelativePathPiece("src/test.c"),
          dtype_t::Regular,
          FileChangeJournalDelta::REMOVED)));

  checkFileChangeJournalEntries(expected_journal, testMount);
}

TEST_P(CheckoutTest, createUntrackedFileAndCheckoutAsTrackedFile) {
  auto builder1 = FakeTreeBuilder();
  builder1.setFile("src/main.c", "// Some code.\n");
  TestMount testMount{RootId{"1"}, builder1};
  applyParam(testMount);

  auto builder2 = builder1.clone();
  builder2.setFile("src/test.c", "// Unit test.\n");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit("2", builder2);
  commit2->setReady();

  auto executor = testMount.getServerExecutor().get();
  auto checkoutTo1 = testMount.getEdenMount()
                         ->checkout(
                             testMount.getRootInode(),
                             RootId("1"),
                             ObjectFetchContext::getNullContext(),
                             __func__)
                         .semi()
                         .via(executor)
                         .waitVia(executor);
  ASSERT_TRUE(checkoutTo1.isReady());

  testMount.addFile("src/test.c", "temporary edit\n");
  auto checkoutTo2 = testMount.getEdenMount()
                         ->checkout(
                             testMount.getRootInode(),
                             RootId("2"),
                             ObjectFetchContext::getNullContext(),
                             __func__)
                         .semi()
                         .via(executor)
                         .waitVia(executor);
  ASSERT_TRUE(checkoutTo2.isReady());

  EXPECT_THAT(
      std::move(checkoutTo2).get().conflicts,
      UnorderedElementsAre(makeConflict(
          ConflictType::UNTRACKED_ADDED, "src/test.c", "", Dtype::REGULAR)));

  std::vector<FileChangeJournalDelta> expected_journal;
#ifndef _WIN32
  expected_journal.emplace_back(
      std::forward<FileChangeJournalDelta>(FileChangeJournalDelta(
          RelativePathPiece("src/test.c"),
          dtype_t::Regular,
          FileChangeJournalDelta::CHANGED)));
#else
  expected_journal.emplace_back(
      std::forward<FileChangeJournalDelta>(FileChangeJournalDelta(
          RelativePathPiece("src/test.c"),
          dtype_t::Regular,
          FileChangeJournalDelta::CREATED)));
#endif

  checkFileChangeJournalEntries(expected_journal, testMount);
}

/*
 * This is similar to createUntrackedFileAndCheckoutAsTrackedFile, except it
 * exercises the case where the code must traverse into an untracked directory
 * and mark its contents UNTRACKED_ADDED, as appropriate.
 */
TEST_P(
    CheckoutTest,
    createUntrackedFileAsOnlyDirectoryEntryAndCheckoutAsTrackedFile) {
  auto builder1 = FakeTreeBuilder();
  builder1.setFile("src/main.c", "// Some code.\n");
  TestMount testMount{RootId("1"), builder1};
  applyParam(testMount);

  auto builder2 = builder1.clone();
  builder2.setFile("src/test/test.c", "// Unit test.\n");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit("2", builder2);
  commit2->setReady();

  auto executor = testMount.getServerExecutor().get();
  auto checkoutTo1 = testMount.getEdenMount()
                         ->checkout(
                             testMount.getRootInode(),
                             RootId("1"),
                             ObjectFetchContext::getNullContext(),
                             __func__)
                         .semi()
                         .via(executor)
                         .waitVia(executor);
  ASSERT_TRUE(checkoutTo1.isReady());

  testMount.mkdir("src/test");
  testMount.addFile("src/test/test.c", "temporary edit\n");
  auto checkoutTo2 = testMount.getEdenMount()
                         ->checkout(
                             testMount.getRootInode(),
                             RootId("2"),
                             ObjectFetchContext::getNullContext(),
                             __func__)
                         .semi()
                         .via(executor)
                         .waitVia(executor);
  ASSERT_TRUE(checkoutTo2.isReady());

  EXPECT_THAT(
      std::move(checkoutTo2).get().conflicts,
      UnorderedElementsAre(makeConflict(
          ConflictType::UNTRACKED_ADDED,
          "src/test/test.c",
          "",
          Dtype::REGULAR)));

  std::vector<FileChangeJournalDelta> expected_journal;
#ifndef _WIN32
  expected_journal.emplace_back(
      std::forward<FileChangeJournalDelta>(FileChangeJournalDelta(
          RelativePathPiece("src/test/test.c"),
          dtype_t::Regular,
          FileChangeJournalDelta::CHANGED)));
#else
  expected_journal.emplace_back(
      std::forward<FileChangeJournalDelta>(FileChangeJournalDelta(
          RelativePathPiece("src/test/test.c"),
          dtype_t::Regular,
          FileChangeJournalDelta::CREATED)));
#endif

  checkFileChangeJournalEntries(expected_journal, testMount);
}

void testAddSubdirectory(
    folly::StringPiece newDirPath,
    LoadBehavior loadType,
    bool useCoroutines) {
  auto builder1 = FakeTreeBuilder();
  builder1.setFile("src/main.c", "int main() { return 0; }\n");
  builder1.setFile("src/test/test.c", "testy tests");
  TestMount testMount{builder1};
  if (useCoroutines) {
    enableCoroutinesConfig(testMount);
  }

  // Prepare a second tree, by starting with builder1 then adding
  // the new directory
  auto builder2 = builder1.clone();
  RelativePathPiece newDir{newDirPath};
  builder2.setFile(newDir + "doc.txt"_pc, "docs\n");
  builder2.setFile(newDir + "file1.c"_pc, "src\n");
  builder2.setFile(newDir + "include/file1.h"_relpath, "header\n");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit("2", builder2);
  commit2->setReady();

  loadInodes(testMount, newDirPath, loadType);

  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId("2"),
                                ObjectFetchContext::getNullContext(),
                                __func__)
                            .semi()
                            .via(executor)
                            .waitVia(executor);
  ASSERT_TRUE(checkoutResult.isReady());
  auto result = std::move(checkoutResult).get();
  EXPECT_EQ(0, result.conflicts.size());

  // Confirm that the tree has been updated correctly.
  EXPECT_FILE_INODE(
      testMount.getFileInode(newDir + "doc.txt"_pc), "docs\n", 0644);
  EXPECT_FILE_INODE(
      testMount.getFileInode(newDir + "file1.c"_pc), "src\n", 0644);
  EXPECT_FILE_INODE(
      testMount.getFileInode(newDir + "include/file1.h"_relpath),
      "header\n",
      0644);
}

TEST_P(CheckoutTest, addSubdirectory) {
  // Test with multiple paths to exercise the case where the modification is at
  // the start of the directory listing, at the end, and in the middle.
  for (const auto& path : {"src/aaa", "src/ppp", "src/zzz"}) {
    for (auto loadType : kAddLoadTypes) {
      SCOPED_TRACE(fmt::format("path {} load type {}", path, loadType));
      testAddSubdirectory(path, loadType, GetParam());
    }
  }
}

void testRemoveSubdirectory(LoadBehavior loadType, bool useCoroutines) {
  // Build the destination source control tree first
  auto destBuilder = FakeTreeBuilder();
  destBuilder.setFile("src/main.c", "int main() { return 0; }\n");
  destBuilder.setFile("src/test/test.c", "testy tests");

  // Prepare the source tree by adding a new subdirectory (which will be
  // removed when we checkout from the src to the dest tree).
  auto srcBuilder = destBuilder.clone();
  RelativePathPiece path{"src/todelete"};
  srcBuilder.setFile(path + "doc.txt"_pc, "docs\n");
  srcBuilder.setFile(path + "file1.c"_pc, "src\n");
  srcBuilder.setFile(path + "include/file1.h"_relpath, "header\n");

  TestMount testMount{srcBuilder};
  if (useCoroutines) {
    enableCoroutinesConfig(testMount);
  }
  destBuilder.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit("2", destBuilder);
  commit2->setReady();

  loadInodes(testMount, path, loadType);

  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId("2"),
                                ObjectFetchContext::getNullContext(),
                                __func__)
                            .semi()
                            .via(executor)
                            .waitVia(executor);
  ASSERT_TRUE(checkoutResult.isReady());
  auto results = std::move(checkoutResult).get();
  EXPECT_EQ(0, results.conflicts.size());

  // Confirm that the tree no longer exists.
  // None of the files should exist.
  EXPECT_THROW_ERRNO(testMount.getFileInode(path + "doc.txt"_pc), ENOENT);
  EXPECT_THROW_ERRNO(testMount.getFileInode(path + "file1.c"_pc), ENOENT);
  EXPECT_THROW_ERRNO(
      testMount.getFileInode(path + "include/file1.h"_relpath), ENOENT);
  // The two directories should have been removed too
  EXPECT_THROW_ERRNO(testMount.getTreeInode(path + "include"_relpath), ENOENT);
  EXPECT_THROW_ERRNO(testMount.getTreeInode(path), ENOENT);
}

// Remove a subdirectory with no conflicts or untracked files left behind
TEST_P(CheckoutTest, removeSubdirectorySimple) {
  for (auto loadType : kAllLoadTypes) {
    SCOPED_TRACE(fmt::format(" load type {}", loadType));
    testRemoveSubdirectory(loadType, GetParam());
  }
}

TEST_P(CheckoutTest, checkoutModifiesDirectoryDuringLoad) {
  auto builder1 = FakeTreeBuilder{};
  builder1.setFile("dir/sub/file.txt", "contents");
  TestMount testMount{builder1, false};
  applyParam(testMount);
  builder1.setReady("");
  builder1.setReady("dir");

  // Prepare a second commit, pointing dir/sub to a different tree.
  auto builder2 = FakeTreeBuilder{};
  builder2.setFile("dir/sub/differentfile.txt", "differentcontents");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit("2", builder2);
  commit2->setReady();

  // Begin loading "dir/sub".
  auto inodeFuture =
      testMount.getEdenMount()
          ->getInodeSlow(
              "dir/sub"_relpath, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();
  EXPECT_FALSE(inodeFuture.isReady());

  // Checkout to a revision where the contents of "dir/sub" have changed.
  auto checkoutResult = testMount.getEdenMount()->checkout(
      testMount.getRootInode(),
      RootId{"2"},
      ObjectFetchContext::getNullContext(),
      __func__);

  // The checkout ought to wait until the load completes.
  EXPECT_FALSE(checkoutResult.isReady());

  // Finish loading.
  builder1.setReady("dir/sub");
  testMount.drainServerExecutor();
  EXPECT_TRUE(inodeFuture.isReady());

  auto executor = testMount.getServerExecutor().get();
  auto waitedCheckoutResult =
      std::move(checkoutResult).semi().via(executor).waitVia(executor);
  ASSERT_TRUE(waitedCheckoutResult.isReady());
  auto result = std::move(waitedCheckoutResult).get();
  EXPECT_EQ(0, result.conflicts.size());

  auto inode = std::move(inodeFuture).get().asTreePtr();
  EXPECT_EQ(0, inode->lockContentsRead()->entries.count("file.txt"_pc));
  EXPECT_EQ(
      1, inode->lockContentsRead()->entries.count("differentfile.txt"_pc));
}

TEST_P(CheckoutTest, checkoutCaseChanged) {
  auto builder1 = FakeTreeBuilder{};
  builder1.setFile("root", "root");
  TestMount testMount{builder1};
  applyParam(testMount);

  auto lowerBuilder = builder1.clone();
  lowerBuilder.setFile("dir/file1", "lower one");
  lowerBuilder.setFile("dir/file2", "lower two");
  lowerBuilder.finalize(testMount.getBackingStore(), true);
  auto lowerCommit = testMount.getBackingStore()->putCommit("2", lowerBuilder);
  lowerCommit->setReady();

  auto upperBuilder = builder1.clone();
  upperBuilder.setFile("DIR/FILE1", "upper one");
  upperBuilder.setFile("DIR/FILE2", "upper two");
  upperBuilder.finalize(testMount.getBackingStore(), true);
  auto upperCommit = testMount.getBackingStore()->putCommit("3", upperBuilder);
  upperCommit->setReady();

  auto executor = testMount.getServerExecutor().get();
  auto checkoutToLowerResult = testMount.getEdenMount()
                                   ->checkout(
                                       testMount.getRootInode(),
                                       RootId{"2"},
                                       ObjectFetchContext::getNullContext(),
                                       __func__)
                                   .semi()
                                   .via(executor)
                                   .getVia(executor);
  EXPECT_EQ(checkoutToLowerResult.conflicts.size(), 0);

  auto checkoutToUpperResult = testMount.getEdenMount()
                                   ->checkout(
                                       testMount.getRootInode(),
                                       RootId{"3"},
                                       ObjectFetchContext::getNullContext(),
                                       __func__)
                                   .semi()
                                   .via(executor)
                                   .getVia(executor);
  EXPECT_EQ(checkoutToUpperResult.conflicts.size(), 0);

  auto file1 = testMount.getFileInode("DIR/FILE1"_relpath);
  auto file2 = testMount.getFileInode("DIR/FILE2"_relpath);

  EXPECT_FILE_INODE(file1, "upper one", 0644);
  EXPECT_FILE_INODE(file2, "upper two", 0644);

  EXPECT_EQ(*file1->getPath(), "DIR/FILE1"_relpath);
  EXPECT_EQ(*file2->getPath(), "DIR/FILE2"_relpath);

  if (testMount.getEdenMount()->getCheckoutConfig()->getCaseSensitive() ==
      CaseSensitivity::Insensitive) {
    EXPECT_FILE_INODE(
        testMount.getFileInode("dir/file1"_relpath), "upper one", 0644);
    EXPECT_FILE_INODE(
        testMount.getFileInode("dir/file2"_relpath), "upper two", 0644);
  }
}

TEST_P(CheckoutTest, checkoutCaseChangedTreeInsensitive) {
  auto builder1 = FakeTreeBuilder{};
  builder1.setFile("root", "root");
  builder1.setFile("dir/file1", "lower one");
  builder1.setFile("dir/file2", "lower two");
  TestMount testMount{builder1, true, true, CaseSensitivity::Insensitive};
  applyParam(testMount);

  // Same content, directory renamed to a different case (DIR/FILE1,
  // DIR/FILE2). On a case-insensitive mount this is a case-rename of the
  // existing directory.
  auto upperBuilder = FakeTreeBuilder{};
  upperBuilder.setFile("root", "root");
  upperBuilder.setFile("DIR/FILE1", "lower one");
  upperBuilder.setFile("DIR/FILE2", "lower two");
  upperBuilder.finalize(testMount.getBackingStore(), true);
  auto upperCommit = testMount.getBackingStore()->putCommit("2", upperBuilder);
  upperCommit->setReady();

  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"2"},
                                ObjectFetchContext::getNullContext(),
                                __func__)
                            .semi()
                            .via(executor)
                            .getVia(executor);
  EXPECT_EQ(checkoutResult.conflicts.size(), 0);

  EXPECT_FILE_INODE(
      testMount.getFileInode("DIR/FILE1"_relpath), "lower one", 0644);
  EXPECT_FILE_INODE(
      testMount.getFileInode("DIR/FILE2"_relpath), "lower two", 0644);
  EXPECT_EQ(
      *testMount.getFileInode("DIR/FILE1"_relpath)->getPath(),
      "DIR/FILE1"_relpath);
  // Old-case access still resolves on case-insensitive mounts.
  EXPECT_FILE_INODE(
      testMount.getFileInode("dir/file1"_relpath), "lower one", 0644);
}

TEST_P(CheckoutTest, checkoutRememberedChildWithDifferentCaseInsensitive) {
  FakeTreeBuilder builder;
  TestMount testMount{builder, true, true, CaseSensitivity::Insensitive};
  applyParam(testMount);
  testMount.addFile("Foo", "local\n");

  auto child = testMount.getFileInode("Foo");
  auto childNumber = child->getNodeId();
  child->incFsRefcount();
  child.reset();
  auto* inodeMap = testMount.getEdenMount()->getInodeMap();
  SCOPE_EXIT {
    inodeMap->decFsRefcount(childNumber);
  };
  testMount.getRootInode()->unloadChildrenNow();
  ASSERT_TRUE(inodeMap->isInodeRemembered(childNumber));
  {
    auto contents = testMount.getRootInode()->lockContentsRead();
    auto it = contents->entries.find("foo"_pc);
    ASSERT_NE(it, contents->entries.end());
    ASSERT_EQ(it->first, "Foo"_pc);
    ASSERT_EQ(it->second.getInode(), nullptr);
  }

  FakeTreeBuilder targetBuilder;
  targetBuilder.setFile("foo", "tracked\n");
  targetBuilder.finalize(testMount.getBackingStore(), true);
  testMount.getBackingStore()->putCommit("2", targetBuilder)->setReady();

  auto executor = testMount.getServerExecutor().get();
  auto result = testMount.getEdenMount()
                    ->checkout(
                        testMount.getRootInode(),
                        RootId{"2"},
                        ObjectFetchContext::getNullContext(),
                        __func__,
                        CheckoutMode::FORCE)
                    .semi()
                    .via(executor)
                    .getVia(executor);
  ASSERT_EQ(result.conflicts.size(), 1);
  EXPECT_EQ(*result.conflicts[0].type(), ConflictType::UNTRACKED_ADDED);
  EXPECT_FILE_INODE(testMount.getFileInode("foo"), "tracked\n", 0644);
}

TEST_P(CheckoutTest, checkoutCaseChangedTreeToFileInsensitive) {
  auto builder1 = FakeTreeBuilder{};
  builder1.setFile("root", "root");
  builder1.setFile("dir/file1", "lower one");
  TestMount testMount{builder1, true, true, CaseSensitivity::Insensitive};
  applyParam(testMount);

  // Build a "to" commit where `dir/` is replaced by a file named `DIR`
  // (different case). On a case-insensitive mount the existing `dir/`
  // tree must be unlinked and the file added at the new case.
  auto toBuilder = FakeTreeBuilder{};
  toBuilder.setFile("root", "root");
  toBuilder.setFile("DIR", "now a file");
  toBuilder.finalize(testMount.getBackingStore(), true);
  auto toCommit = testMount.getBackingStore()->putCommit("2", toBuilder);
  toCommit->setReady();

  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"2"},
                                ObjectFetchContext::getNullContext(),
                                __func__)
                            .semi()
                            .via(executor)
                            .getVia(executor);
  EXPECT_EQ(checkoutResult.conflicts.size(), 0);

  EXPECT_FILE_INODE(testMount.getFileInode("DIR"_relpath), "now a file", 0644);
  // Old name resolves to the same inode on case-insensitive mounts.
  EXPECT_FILE_INODE(testMount.getFileInode("dir"_relpath), "now a file", 0644);
}

TEST_P(CheckoutTest, checkoutCaseChangedTreeNotEmptyInsensitive) {
  auto builder1 = FakeTreeBuilder{};
  builder1.setFile("root", "root");
  builder1.setFile("dir/file1", "lower one");
  TestMount testMount{builder1, true, true, CaseSensitivity::Insensitive};
  applyParam(testMount);

  // Materialize the directory by creating an UNTRACKED file inside it.
  // This file is not in any commit; the case-rename below must navigate
  // its presence without losing data.
  testMount.addFile("dir/untracked.txt", "untracked");

  auto upperBuilder = FakeTreeBuilder{};
  upperBuilder.setFile("root", "root");
  upperBuilder.setFile("DIR/file1", "lower one");
  upperBuilder.finalize(testMount.getBackingStore(), true);
  auto upperCommit = testMount.getBackingStore()->putCommit("2", upperBuilder);
  upperCommit->setReady();

  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"2"},
                                ObjectFetchContext::getNullContext(),
                                __func__)
                            .semi()
                            .via(executor)
                            .getVia(executor);
  // Don't assert on conflict shape — varies by platform/dispatcher.
  (void)checkoutResult;

  // Untracked file survives the case rename.
  EXPECT_FILE_INODE(
      testMount.getFileInode("dir/untracked.txt"_relpath), "untracked", 0644);
}

#ifndef _WIN32
TEST_P(CheckoutTest, checkoutRemovingDirectoryDeletesOverlayFile) {
  auto builder1 = FakeTreeBuilder{};
  builder1.setFile("dir/sub/file.txt", "contents");
  TestMount testMount{builder1};
  applyParam(testMount);

  // Prepare a second commit, removing dir/sub.
  auto builder2 = FakeTreeBuilder{};
  builder2.setFile("dir/tree/differentfile.txt", "differentcontents");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit("2", builder2);
  commit2->setReady();

  // Load "dir/sub".
  auto subTree = testMount.getTreeInode("dir/sub"_relpath);
  auto subInodeNumber = subTree->getNodeId();
  auto fileInodeNumber =
      testMount.getFileInode("dir/sub/file.txt"_relpath)->getNodeId();
  subTree.reset();

  // Allocated inode numbers are saved during takeover.
  testMount.remountGracefully();

  EXPECT_TRUE(testMount.hasOverlayDir(subInodeNumber));
  EXPECT_TRUE(testMount.hasMetadata(subInodeNumber));
  EXPECT_TRUE(testMount.hasMetadata(fileInodeNumber));

  // Checkout to a revision without "dir/sub".
  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId("2"),
                                ObjectFetchContext::getNullContext(),
                                __func__)
                            .semi()
                            .via(executor)
                            .getVia(executor);
  EXPECT_EQ(0, checkoutResult.conflicts.size());

  // The checkout kicked off an async deletion of a subtree - wait for it to
  // complete.
  testMount.getEdenMount()->getOverlay()->flushPendingAsync().get(60s);

  EXPECT_FALSE(testMount.hasOverlayDir(subInodeNumber));
  EXPECT_FALSE(testMount.hasMetadata(subInodeNumber));
  EXPECT_FALSE(testMount.hasMetadata(fileInodeNumber));
}

TEST_P(CheckoutTest, checkoutUpdatesUnlinkedStatusForLoadedTrees) {
  // This test is designed to stress the logic in
  // TreeInode::processCheckoutEntry that decides whether it's necessary to load
  // a TreeInode in order to continue.  It tests that unlinked status is
  // properly updated for tree inodes that have are referenced after a takeover.

  auto builder1 = FakeTreeBuilder{};
  builder1.setFile("dir/sub/file.txt", "contents");
  TestMount testMount{builder1};
  applyParam(testMount);

  // Prepare a second commit, removing dir/sub.
  auto builder2 = FakeTreeBuilder{};
  builder2.setFile("dir/tree/differentfile.txt", "differentcontents");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit("2", builder2);
  commit2->setReady();

  // Load "dir/sub" on behalf of a FUSE connection.
  auto subTree = testMount.getTreeInode("dir/sub"_relpath);
  auto subInodeNumber = subTree->getNodeId();
  subTree->incFsRefcount();
  subTree.reset();

  testMount.remountGracefully();

  // Checkout to a revision without "dir/sub" even though it's still referenced
  // by FUSE.
  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId("2"),
                                ObjectFetchContext::getNullContext(),
                                __func__)
                            .semi()
                            .via(executor)
                            .getVia(executor);
  EXPECT_EQ(0, checkoutResult.conflicts.size());

  // Try to load the same tree by its inode number. This will fail if the
  // unlinked bit wasn't set correctly.
  subTree = testMount.getEdenMount()
                ->getInodeMap()
                ->lookupTreeInode(
                    subInodeNumber, ObjectFetchContext::getNullContext())
                .get(1ms);
  {
    auto subTreeContents = subTree->lockContentsRead();
    EXPECT_TRUE(subTree->isUnlinked());
    // Unlinked inodes are considered materialized?
    EXPECT_TRUE(subTreeContents->isMaterialized());
  }

  auto dirTree = testMount.getTreeInode("dir"_relpath);
  auto dirContents = dirTree->lockContentsRead();
  EXPECT_FALSE(dirContents->isMaterialized());
}

TEST_P(CheckoutTest, checkoutRemembersInodeNumbersAfterCheckoutAndTakeover) {
  auto builder1 = FakeTreeBuilder{};
  builder1.setFile("dir/sub/file1.txt", "contents1");
  TestMount testMount{builder1};
  applyParam(testMount);

  // Prepare a second commit, changing dir/sub.
  auto builder2 = FakeTreeBuilder{};
  builder2.setFile("dir/sub/file2.txt", "contents2");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit("2", builder2);
  commit2->setReady();

  testMount.drainServerExecutor();

  // Load "dir/sub" on behalf of a FUSE connection.
  auto subTree = testMount.getTreeInode("dir/sub"_relpath);
  auto dirInodeNumber = subTree->getParentRacy()->getNodeId();
  auto subInodeNumber = subTree->getNodeId();
  subTree->incFsRefcount();
  subTree.reset();

  // Checkout to a revision with a new dir/sub tree.  The old data should be
  // removed from the overlay.
  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId("2"),
                                ObjectFetchContext::getNullContext(),
                                __func__)
                            .semi()
                            .via(executor)
                            .getVia(executor);
  EXPECT_EQ(0, checkoutResult.conflicts.size());

  testMount.remountGracefully();

  // Try to load the same tree by its inode number and verify its parents have
  // the same inode numbers.
  auto subTreeFut =
      testMount.getEdenMount()
          ->getInodeMap()
          ->lookupTreeInode(
              subInodeNumber, ObjectFetchContext::getNullContext())
          .semi()
          .via(executor);
  testMount.drainServerExecutor();

  subTree = std::move(subTreeFut).get(1ms);
  EXPECT_EQ(dirInodeNumber, subTree->getParentRacy()->getNodeId());
  EXPECT_EQ(subInodeNumber, subTree->getNodeId());

  auto subTree2 = testMount.getTreeInode("dir/sub"_relpath);
  EXPECT_EQ(dirInodeNumber, subTree2->getParentRacy()->getNodeId());
  EXPECT_EQ(subInodeNumber, subTree2->getNodeId());

  testMount.getEdenMount()->getInodeMap()->decFsRefcount(subInodeNumber);
  subTree.reset();
  subTree2.reset();

  subTree = testMount.getTreeInode("dir/sub"_relpath);
  EXPECT_EQ(dirInodeNumber, subTree->getParentRacy()->getNodeId());
  EXPECT_EQ(subInodeNumber, subTree->getNodeId());
}

std::vector<SetPathObjectIdObjectAndPath> getObjects(
    RelativePathPiece path,
    folly::StringPiece objectId,
    facebook::eden::ObjectType type) {
  SetPathObjectIdObjectAndPath objectAndPath;
  objectAndPath.path = RelativePath{path};
  objectAndPath.id = ObjectId{objectId};
  objectAndPath.type = type;
  std::vector<SetPathObjectIdObjectAndPath> objects{objectAndPath};
  return objects;
}

void runTestSetPathObjectId(
    folly::StringPiece file,
    folly::StringPiece pathToSet,
    RelativePathPiece expectedFile,
    bool useCoroutines) {
  auto builder1 = FakeTreeBuilder{};
  builder1.setFile("dir/dir2/dir3/file.txt", "contents");
  TestMount testMount{builder1, false};
  if (useCoroutines) {
    enableCoroutinesConfig(testMount);
  }
  builder1.setReady("");
  builder1.setReady("dir");
  builder1.setReady("dir/dir2");
  builder1.setReady("dir/dir2/dir3");

  // Prepare a second commit, pointing dir/sub to a different tree.
  auto builder2 = FakeTreeBuilder{};
  builder2.setFile(file, "differentcontents");
  builder2.finalize(testMount.getBackingStore(), true);
  auto storeTree = builder2.getStoredTree(RelativePathPiece{});
  auto commit2 = testMount.getBackingStore()->putCommit(
      storeTree->get().getObjectId().asString(), builder2);
  commit2->setReady();

  // Insert file2 to pathToSet
  auto setPathObjectIdResultAndTimesAndTimes =
      testMount.getEdenMount()
          ->setPathsToObjectIds(
              getObjects(
                  RelativePathPiece{pathToSet},
                  storeTree->get().getObjectId().asString(),
                  facebook::eden::ObjectType::TREE),
              facebook::eden::CheckoutMode::NORMAL,
              ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());

  testMount.drainServerExecutor();
  auto result = std::move(setPathObjectIdResultAndTimesAndTimes).get();
  EXPECT_EQ(0, result.result.conflicts()->size());

  // Confirm that the tree has been updated correctly.
  EXPECT_FILE_INODE(
      testMount.getFileInode(expectedFile), "differentcontents", 0644);
}

TEST_P(CheckoutTest, testSetPathObjectIdSimple) {
  runTestSetPathObjectId(
      "differentdir/differentfile.txt",
      "dir",
      "dir/differentdir/differentfile.txt"_relpath,
      GetParam());
}

TEST_P(CheckoutTest, testSetPathObjectIdNewDir) {
  runTestSetPathObjectId(
      "differentdir/differentfile.txt",
      "dir2",
      "dir2/differentdir/differentfile.txt"_relpath,
      GetParam());
}

TEST_P(CheckoutTest, testSetPathObjectIdSetOnRoot) {
  runTestSetPathObjectId(
      "differentdir/differentfile.txt",
      "",
      "differentdir/differentfile.txt"_relpath,
      GetParam());
}

TEST_P(CheckoutTest, testSetPathObjectIdMultipleLevelFolder) {
  runTestSetPathObjectId(
      "differentdir/differentfile.txt",
      "dir/dir2/dir3",
      "dir/dir2/dir3/differentdir/differentfile.txt"_relpath,
      GetParam());
}

TEST_P(CheckoutTest, testSetPathObjectIdMultipleLevelFolderAndNewDir) {
  runTestSetPathObjectId(
      "differentdir/differentfile.txt",
      "dir/dir2/dir4",
      "dir/dir2/dir4/differentdir/differentfile.txt"_relpath,
      GetParam());
}

TEST_P(CheckoutTest, testSetPathObjectIdConflict) {
  auto builder1 = FakeTreeBuilder{};
  builder1.setFile("dir/dir2/dir3/file.txt", "contents");
  TestMount testMount{builder1, false};
  applyParam(testMount);
  builder1.setReady("");
  builder1.setReady("dir");
  builder1.setReady("dir/dir2");
  builder1.setReady("dir/dir2/dir3");

  // Prepare a second commit, pointing dir/sub to a different tree.
  auto builder2 = FakeTreeBuilder{};
  builder2.setFile("file.txt", "differentcontents");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit("2", builder2);
  commit2->setReady();
  auto storeTree = builder2.getStoredTree(RelativePathPiece{});

  // Insert file2 to pathToSet
  RelativePathPiece path{"dir/dir2/dir3"};
  SetPathObjectIdParams params;
  params.type() = facebook::eden::ObjectType::TREE;
  auto setPathObjectIdResultAndTimes =
      testMount.getEdenMount()
          ->setPathsToObjectIds(
              getObjects(
                  path,
                  storeTree->get().getObjectId().asString(),
                  facebook::eden::ObjectType::TREE),
              facebook::eden::CheckoutMode::NORMAL,
              ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());

  testMount.drainServerExecutor();

  auto result = std::move(setPathObjectIdResultAndTimes).get();
  ASSERT_TRUE(
      apache::thrift::is_non_optional_field_set_manually_or_by_serializer(
          result.result.conflicts()));
  EXPECT_EQ(1, result.result.conflicts()->size());
  EXPECT_THAT(
      std::move(result).result.conflicts().value(),
      UnorderedElementsAre(makeConflict(
          ConflictType::UNTRACKED_ADDED,
          "dir/dir2/dir3/file.txt",
          "",
          Dtype::REGULAR)));
}

TEST_P(CheckoutTest, testSetPathObjectIdLastCheckoutTime) {
  TestMount testMount;
  applyParam(testMount);
  auto builder1 = FakeTreeBuilder();
  builder1.setFile("dir/file.txt", "contents");
  builder1.finalize(testMount.getBackingStore(), true);
  builder1.setReady("");
  builder1.setReady("dir");
  auto commit = testMount.getBackingStore()->putCommit("1", builder1);
  commit->setReady();

  auto sec = std::chrono::seconds{50000};
  auto nsec = std::chrono::nanoseconds{10000};
  auto duration = sec + nsec;
  std::chrono::system_clock::time_point currentTime(
      std::chrono::duration_cast<std::chrono::system_clock::duration>(
          duration));

  testMount.initialize(RootId("1"), currentTime);
  const auto& edenMount = testMount.getEdenMount();
  struct timespec lastCheckoutTime =
      edenMount->getLastCheckoutTime().toTimespec();

  // Check if EdenMount is updating lastCheckoutTime correctly
  EXPECT_EQ(sec.count(), lastCheckoutTime.tv_sec);
  EXPECT_EQ(nsec.count(), lastCheckoutTime.tv_nsec);

  // Check if FileInode is updating lastCheckoutTime correctly
  auto fileInode = testMount.getFileInode("dir/file.txt");
  auto stFile = fileInode->getMetadata().timestamps;
  EXPECT_EQ(sec.count(), stFile.atime.toTimespec().tv_sec);
  EXPECT_EQ(nsec.count(), stFile.atime.toTimespec().tv_nsec);
  EXPECT_EQ(sec.count(), stFile.ctime.toTimespec().tv_sec);
  EXPECT_EQ(nsec.count(), stFile.ctime.toTimespec().tv_nsec);
  EXPECT_EQ(sec.count(), stFile.mtime.toTimespec().tv_sec);
  EXPECT_EQ(nsec.count(), stFile.mtime.toTimespec().tv_nsec);

  // Prepare a second commit, pointing dir/sub to a different tree.
  auto builder2 = FakeTreeBuilder{};
  builder2.setFile("file2.txt", "differentcontents");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit("2", builder2);
  commit2->setReady();
  auto storeTree = builder2.getStoredTree(RelativePathPiece{});

  auto sec2 = std::chrono::seconds{60000};
  auto nsec2 = std::chrono::nanoseconds{20000};
  auto duration2 = sec2 + nsec2;
  std::chrono::system_clock::time_point currentTime2(
      std::chrono::duration_cast<std::chrono::system_clock::duration>(
          duration2));
  testMount.getClock().set(currentTime2);

  // Insert file2 to dir2
  RelativePathPiece path{"dir2"};
  SetPathObjectIdParams params;
  params.type() = facebook::eden::ObjectType::TREE;
  auto setPathObjectIdResultAndTimes =
      testMount.getEdenMount()
          ->setPathsToObjectIds(
              getObjects(
                  path,
                  storeTree->get().getObjectId().asString(),
                  facebook::eden::ObjectType::TREE),
              facebook::eden::CheckoutMode::NORMAL,
              ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());

  testMount.drainServerExecutor();

  auto result = std::move(setPathObjectIdResultAndTimes).get();
  EXPECT_EQ(0, result.result.conflicts()->size());

  struct timespec updatedLastCheckoutTime =
      edenMount->getLastCheckoutTime().toTimespec();

  // Check if EdenMount is updating lastCheckoutTime correctly
  EXPECT_EQ(sec2.count(), updatedLastCheckoutTime.tv_sec);
  EXPECT_EQ(nsec2.count(), updatedLastCheckoutTime.tv_nsec);

  // Now get the new file and the timestamps should be updated.
  auto fileInode2 = testMount.getFileInode("dir2/file2.txt");
  auto stFile2 = fileInode2->getMetadata().timestamps;
  EXPECT_EQ(sec2.count(), stFile2.atime.toTimespec().tv_sec);
  EXPECT_EQ(nsec2.count(), stFile2.atime.toTimespec().tv_nsec);
  EXPECT_EQ(sec2.count(), stFile2.ctime.toTimespec().tv_sec);
  EXPECT_EQ(nsec2.count(), stFile2.ctime.toTimespec().tv_nsec);
  EXPECT_EQ(sec2.count(), stFile2.mtime.toTimespec().tv_sec);
  EXPECT_EQ(nsec2.count(), stFile2.mtime.toTimespec().tv_nsec);
}

TEST_P(CheckoutTest, testSetPathObjectIdCheckoutSingleFile) {
  // Start with an empty mount
  auto builder1 = FakeTreeBuilder{};
  TestMount testMount{builder1, false};
  applyParam(testMount);

  std::string contents = "content";
  testMount.getBackingStore()->putBlob(ObjectId{"2"}, contents)->setReady();

  RelativePathPiece path{"dir/dir2/dir3/file.txt"};
  auto gcLease = testMount.getEdenMount()->tryStartInodeGC();
  ASSERT_TRUE(gcLease.has_value());
  auto gcToken = gcLease->getCancellationToken();

  auto setPathObjectIdResultAndTimes =
      testMount.getEdenMount()
          ->setPathsToObjectIds(
              getObjects(path, "2", facebook::eden::ObjectType::REGULAR_FILE),
              facebook::eden::CheckoutMode::NORMAL,
              ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());

  testMount.drainServerExecutor();

  auto result = std::move(setPathObjectIdResultAndTimes).get();
  EXPECT_EQ(0, result.result.conflicts()->size());
  EXPECT_TRUE(gcToken.isCancellationRequested());
  gcLease.reset();
  EXPECT_TRUE(testMount.getEdenMount()->tryStartInodeGC());

  // Confirm that the blob has been updated correctly.
  EXPECT_FILE_INODE(testMount.getFileInode(path), contents, 0644);
}

TEST_P(CheckoutTest, testSetPathObjectIdCheckoutMultipleFiles) {
  // Start with an empty mount
  auto builder1 = FakeTreeBuilder{};
  TestMount testMount{builder1, false};
  applyParam(testMount);

  std::string contents = "content";
  std::string contents2 = "content";
  testMount.getBackingStore()->putBlob(ObjectId{"1"}, contents)->setReady();
  testMount.getBackingStore()->putBlob(ObjectId{"2"}, contents)->setReady();

  RelativePathPiece path{"dir/dir2/dir3/file.txt"};
  RelativePathPiece path2{"dir/dir2/dir3/file2.txt"};

  auto setPathObjectIdResultAndTimes =
      testMount.getEdenMount()
          ->setPathsToObjectIds(
              getObjects(path, "1", facebook::eden::ObjectType::REGULAR_FILE),
              facebook::eden::CheckoutMode::NORMAL,
              ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());

  testMount.drainServerExecutor();

  auto result = std::move(setPathObjectIdResultAndTimes).get();
  EXPECT_EQ(0, result.result.conflicts()->size());

  // Confirm that the blob has been updated correctly.
  EXPECT_FILE_INODE(testMount.getFileInode(path), contents, 0644);

  auto setPathObjectIdResultAndTimes2 =
      testMount.getEdenMount()
          ->setPathsToObjectIds(
              getObjects(path2, "2", facebook::eden::ObjectType::REGULAR_FILE),
              facebook::eden::CheckoutMode::NORMAL,
              ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());

  testMount.drainServerExecutor();

  auto result2 = std::move(setPathObjectIdResultAndTimes2).get();
  EXPECT_EQ(0, result2.result.conflicts()->size());

  EXPECT_FILE_INODE(testMount.getFileInode(path), contents, 0644);
  EXPECT_FILE_INODE(testMount.getFileInode(path2), contents2, 0644);
}

#endif

template <typename Unloader>
struct CheckoutUnloadTest : ::testing::Test {
  Unloader unloader;
};

#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
TYPED_TEST_CASE(CheckoutUnloadTest, InodeUnloaderTypes);
#pragma clang diagnostic pop

#ifndef _WIN32
TYPED_TEST(
    CheckoutUnloadTest,
    unloadAndCheckoutRemembersInodeNumbersForFuseReferencedInodes) {
  auto builder1 = FakeTreeBuilder{};
  builder1.setFile("root/a/b/c/file1.txt", "before1");
  builder1.setFile("root/d/e/f/file2.txt", "before2");
  builder1.setFile("root/g/h/i/file3.txt", "before3");
  TestMount testMount{builder1};

  // Prepare a second commit that modifies all of the files.
  auto builder2 = FakeTreeBuilder{};
  builder2.setFile("root/a/b/c/file1.txt", "after1");
  builder2.setFile("root/d/e/f/file2.txt", "after2");
  builder2.setFile("root/g/h/i/file3.txt", "after3");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit("2", builder2);
  commit2->setReady();

  auto abcfile1 = testMount.getFileInode("root/a/b/c/file1.txt"_relpath);
  auto abcfile1InodeNumber = abcfile1->getNodeId();
  auto abcInodeNumber = abcfile1->getParentRacy()->getNodeId();
  abcfile1->incFsRefcount();
  abcfile1.reset();

  auto deffile2 = testMount.getFileInode("root/d/e/f/file2.txt"_relpath);
  auto deffile2InodeNumber = deffile2->getNodeId();
  auto defInodeNumber = deffile2->getParentRacy()->getNodeId();
  deffile2->getParentRacy()->incFsRefcount();
  deffile2.reset();

  auto ghifile3 = testMount.getFileInode("root/g/h/i/file3.txt"_relpath);
  auto ghifile3InodeNumber = ghifile3->getNodeId();
  auto ghiInodeNumber = ghifile3->getParentRacy()->getNodeId();
  ghifile3.reset();

  auto unloaded =
      this->unloader.unload(*testMount.getTreeInode("root"_relpath));
  // Everything was unloaded.
  EXPECT_EQ(12, unloaded);

  // But FUSE still has references to root/a/b/c/file1.txt and root/d/e/f.

  // Check out to a commit that changes all of these files.
  // Inode numbers for unreferenced files should be forgotten.
  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"2"},
                                ObjectFetchContext::getNullContext(),
                                __func__)
                            .semi()
                            .via(executor)
                            .getVia(executor);
  EXPECT_EQ(0, checkoutResult.conflicts.size());

  // Verify inode numbers for referenced inodes are the same.

  // Files always change inode numbers during a checkout.
  EXPECT_NE(
      abcfile1InodeNumber,
      testMount.getFileInode("root/a/b/c/file1.txt"_relpath)->getNodeId());

  EXPECT_EQ(
      abcInodeNumber,
      testMount.getTreeInode("root/a/b/c"_relpath)->getNodeId());

  // Files always change inode numbers during a checkout.
  EXPECT_NE(
      deffile2InodeNumber,
      testMount.getFileInode("root/d/e/f/file2.txt"_relpath)->getNodeId());

  EXPECT_EQ(
      defInodeNumber,
      testMount.getTreeInode("root/d/e/f"_relpath)->getNodeId());

  // Files always change inode numbers during a checkout.
  EXPECT_NE(
      ghifile3InodeNumber,
      testMount.getFileInode("root/g/h/i/file3.txt"_relpath)->getNodeId());

  // This tree never had its FUSE refcount incremented, so its inode number has
  // been forgotten.
  EXPECT_NE(
      ghiInodeNumber,
      testMount.getTreeInode("root/g/h/i"_relpath)->getNodeId());

  // Replaced files should be unlinked.

  auto edenMount = testMount.getEdenMount();

  abcfile1 = edenMount->getInodeMap()
                 ->lookupFileInode(
                     abcfile1InodeNumber, ObjectFetchContext::getNullContext())
                 .get(1ms);
  EXPECT_TRUE(abcfile1->isUnlinked());

  // Referenced but modified directories are not unlinked - they're updated in
  // place.

  auto def = edenMount->getInodeMap()
                 ->lookupTreeInode(
                     defInodeNumber, ObjectFetchContext::getNullContext())
                 .get(1ms);
  EXPECT_FALSE(def->isUnlinked());
}
#endif

CO_TEST_P(CheckoutTest, diffFailsOnInProgressCheckout) {
  auto builder1 = FakeTreeBuilder();
  builder1.setFile("src/main.c", "// Some code.\n");
  TestMount testMount{RootId{"1"}, builder1};
  applyParam(testMount);
  testMount.getServerState()->getFaultInjector().injectBlock("checkout", ".*");

  // Block checkout so the checkout is "in progress"
  auto executor = testMount.getServerExecutor().get();
  auto checkoutTo1 = testMount.getEdenMount()
                         ->checkout(
                             testMount.getRootInode(),
                             RootId{"1"},
                             ObjectFetchContext::getNullContext(),
                             __func__)
                         .semi()
                         .via(executor);
  testMount.drainServerExecutor();
  CO_ASSERT_TRUE(
      testMount.getServerState()->getFaultInjector().waitUntilBlocked(
          "checkout", 5s));
  EXPECT_FALSE(checkoutTo1.isReady());

  // Call getStatus and make sure it fails.
  auto commitId = RootId{"1"};

  bool caught = false;
  try {
    co_await folly::coro::timeout(
        testMount.getEdenMount()->co_diff(
            testMount.getRootInode(),
            commitId,
            folly::CancellationToken{},
            ObjectFetchContext::getNullContext()),
        60s);
  } catch (const EdenError& exception) {
    caught = true;
    CO_ASSERT_EQ(*exception.errorType(), EdenErrorType::CHECKOUT_IN_PROGRESS);
  }
  CO_ASSERT_TRUE(caught)
      << "diff should have failed with EdenErrorType::CHECKOUT_IN_PROGRESS";

  // Unblock checkout
  testMount.getServerState()->getFaultInjector().unblock("checkout", ".*");

  auto waitedCheckoutTo1 = std::move(checkoutTo1).waitVia(executor);
  EXPECT_TRUE(waitedCheckoutTo1.isReady());

  // Try to diff again just to make sure we don't block again.
  co_await folly::coro::timeout(
      testMount.getEdenMount()->co_diff(
          testMount.getRootInode(),
          commitId,
          folly::CancellationToken{},
          ObjectFetchContext::getNullContext()),
      60s);
}

CO_TEST_P(CheckoutTest, droppedCheckoutFutureRestoresParentStateOnError) {
  auto builder1 = FakeTreeBuilder();
  builder1.setFile("src/main.c", "// Some code.\n");
  TestMount testMount{RootId{"1"}, builder1};
  applyParam(testMount);
  testMount.getServerState()->getFaultInjector().injectBlock("checkout", ".*");

  auto executor = testMount.getServerExecutor().get();
  {
    auto checkout = testMount.getEdenMount()
                        ->checkout(
                            testMount.getRootInode(),
                            RootId{"1"},
                            ObjectFetchContext::getNullContext(),
                            __func__)
                        .semi()
                        .via(executor);
    testMount.drainServerExecutor();
    CO_ASSERT_TRUE(
        testMount.getServerState()->getFaultInjector().waitUntilBlocked(
            "checkout", 5s));
    EXPECT_FALSE(checkout.isReady());
  }

  testMount.getServerState()->getFaultInjector().unblockWithError(
      "checkout",
      ".*",
      folly::make_exception_wrapper<std::runtime_error>("dropped checkout"));
  testMount.drainServerExecutor();
  testMount.getServerState()->getFaultInjector().removeFault("checkout", ".*");

  auto recoveryCheckout = testMount.getEdenMount()
                              ->checkout(
                                  testMount.getRootInode(),
                                  RootId{"1"},
                                  ObjectFetchContext::getNullContext(),
                                  __func__)
                              .semi()
                              .via(executor);
  testMount.drainServerExecutor();
  EXPECT_NO_THROW(std::move(recoveryCheckout).getVia(executor));

  // Diffing after the recovered checkout should succeed.
  co_await folly::coro::timeout(
      testMount.getEdenMount()->co_diff(
          testMount.getRootInode(),
          RootId{"1"},
          folly::CancellationToken{},
          ObjectFetchContext::getNullContext()),
      60s);
}

TEST_P(
    CheckoutTest,
    conflict_when_directory_containing_modified_file_is_removed) {
  auto builder1 = FakeTreeBuilder{};
  builder1.setFile("d1/sub/one.txt", "one");
  builder1.setFile("d2/two.txt", "two");
  TestMount testMount{builder1};
  applyParam(testMount);

  // Prepare a second tree without one directory.
  auto builder2 = FakeTreeBuilder{};
  builder2.setFile("d2/two.txt", "two");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit("2", builder2);
  commit2->setReady();

  testMount.overwriteFile("d1/sub/one.txt", "new contents");

  auto gcLease = testMount.getEdenMount()->tryStartInodeGC();
  ASSERT_TRUE(gcLease.has_value());
  auto gcToken = gcLease->getCancellationToken();

  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"2"},
                                ObjectFetchContext::getNullContext(),
                                __func__,
                                CheckoutMode::DRY_RUN)
                            .semi()
                            .via(executor)
                            .waitVia(executor);
  ASSERT_TRUE(checkoutResult.isReady());
  auto result = std::move(checkoutResult).get();
  ASSERT_EQ(1, result.conflicts.size());
  EXPECT_FALSE(gcToken.isCancellationRequested());
  gcLease.reset();
  EXPECT_TRUE(testMount.getEdenMount()->tryStartInodeGC());

  {
    auto& conflict = result.conflicts[0];
    EXPECT_EQ("d1/sub/one.txt", *conflict.path());
    EXPECT_EQ(ConflictType::MODIFIED_REMOVED, *conflict.type());
    EXPECT_EQ("", *conflict.message());
  }
}

TEST_P(CheckoutTest, checkoutFailsOnInProgressCheckout) {
  auto builder1 = FakeTreeBuilder();
  builder1.setFile("src/main.c", "// Some code.\n");
  TestMount testMount{RootId("1"), builder1};
  applyParam(testMount);
  testMount.getServerState()->getFaultInjector().injectBlock("checkout", ".*");

  auto builder2 = builder1.clone();
  builder2.setFile("src/test.c", "// Unit test.\n");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit("2", builder2);
  commit2->setReady();

  auto gcLease = testMount.getEdenMount()->tryStartInodeGC();
  ASSERT_TRUE(gcLease.has_value());
  auto gcToken = gcLease->getCancellationToken();

  // Block checkout so the checkout is "in progress"
  auto executor = testMount.getServerExecutor().get();
  auto checkout1 = testMount.getEdenMount()
                       ->checkout(
                           testMount.getRootInode(),
                           RootId{"2"},
                           ObjectFetchContext::getNullContext(),
                           __func__)
                       .semi()
                       .via(executor);
  testMount.drainServerExecutor();
  ASSERT_TRUE(testMount.getServerState()->getFaultInjector().waitUntilBlocked(
      "checkout", 5s));
  EXPECT_FALSE(checkout1.isReady());
  EXPECT_TRUE(gcToken.isCancellationRequested());
  EXPECT_FALSE(testMount.getEdenMount()->tryStartInodeGC());
  gcLease.reset();
  EXPECT_FALSE(testMount.getEdenMount()->tryStartInodeGC());

  // Run another checkout and make sure it fails
  try {
    testMount.getEdenMount()
        ->checkout(
            testMount.getRootInode(),
            RootId{"2"},
            ObjectFetchContext::getNullContext(),
            __func__,
            CheckoutMode::NORMAL)
        .semi()
        .via(executor)
        .getVia(executor);
    FAIL() << "checkout should have failed with "
              "EdenErrorType::CHECKOUT_IN_PROGRESS";
  } catch (const EdenError& exception) {
    ASSERT_EQ(*exception.errorType(), EdenErrorType::CHECKOUT_IN_PROGRESS);
  }

  // Unblock original checkout and make sure it completes
  testMount.getServerState()->getFaultInjector().unblock("checkout", ".*");

  EXPECT_NO_THROW(std::move(checkout1).getVia(executor));

  auto gcAfterCheckout = testMount.getEdenMount()->tryStartInodeGC();
  ASSERT_TRUE(gcAfterCheckout.has_value());
  auto gcAfterCheckoutToken = gcAfterCheckout->getCancellationToken();

  // Try to checkout again just to make sure we don't block again.
  testMount.getServerState()->getFaultInjector().removeFault("checkout", ".*");
  auto checkout2 = testMount.getEdenMount()
                       ->checkout(
                           testMount.getRootInode(),
                           RootId("1"),
                           ObjectFetchContext::getNullContext(),
                           __func__)
                       .semi()
                       .via(executor)
                       .waitVia(executor);
  EXPECT_TRUE(checkout2.isReady());
  EXPECT_NO_THROW(std::move(checkout2).get());
  EXPECT_TRUE(gcAfterCheckoutToken.isCancellationRequested());
  EXPECT_FALSE(testMount.getEdenMount()->tryStartInodeGC());
  gcAfterCheckout.reset();
  EXPECT_TRUE(testMount.getEdenMount()->tryStartInodeGC());
}

TEST_P(CheckoutTest, dryRunAfterInterruptedCheckoutKeepsInProgressMarker) {
  auto builder1 = FakeTreeBuilder();
  builder1.setFile("src/main.c", "// Some code.\n");
  TestMount testMount{RootId{"1"}, builder1};
  applyParam(testMount);

  auto builder2 = builder1.clone();
  builder2.replaceFile("src/main.c", "// New code.\n");
  builder2.finalize(testMount.getBackingStore(), true);
  testMount.getBackingStore()->putCommit("2", builder2)->setReady();

  auto checkoutTo2 = [&](CheckoutMode mode) {
    auto executor = testMount.getServerExecutor().get();
    return testMount.getEdenMount()
        ->checkout(
            testMount.getRootInode(),
            RootId{"2"},
            ObjectFetchContext::getNullContext(),
            __func__,
            mode)
        .semi()
        .via(executor)
        .getVia(executor);
  };

  // Fail the NORMAL checkout after it has written the in-progress marker.
  testMount.getServerState()->getFaultInjector().injectError(
      "TreeInode::checkout",
      ".*",
      folly::make_exception_wrapper<std::runtime_error>("checkout failed"),
      1);
  EXPECT_THROW(checkoutTo2(CheckoutMode::NORMAL), EdenError);
  EXPECT_TRUE(testMount.getEdenMount()
                  ->getCheckoutConfig()
                  ->getParentCommit()
                  .isCheckoutInProgress());

  // Sapling sends a DRY_RUN before resuming the interrupted checkout.
  EXPECT_NO_THROW(checkoutTo2(CheckoutMode::DRY_RUN));
  EXPECT_TRUE(testMount.getEdenMount()->isCheckoutInProgress());
  EXPECT_TRUE(testMount.getEdenMount()
                  ->getCheckoutConfig()
                  ->getParentCommit()
                  .isCheckoutInProgress());

  testMount.remount();
  EXPECT_TRUE(testMount.getEdenMount()->isCheckoutInProgress());

  EXPECT_NO_THROW(checkoutTo2(CheckoutMode::NORMAL));
  EXPECT_FALSE(testMount.getEdenMount()->isCheckoutInProgress());
  EXPECT_FALSE(testMount.getEdenMount()
                   ->getCheckoutConfig()
                   ->getParentCommit()
                   .isCheckoutInProgress());
}

TEST_P(CheckoutTest, overlappingCheckoutInhibitorsBlockInodeGC) {
  FakeTreeBuilder builder;
  TestMount testMount{builder};
  applyParam(testMount);
  auto mount = testMount.getEdenMount();

  for (bool releaseFirstInhibitorFirst : {false, true}) {
    std::optional<EdenMount::InodeGCLease> first{mount->stealInodeGCLease()};
    std::optional<EdenMount::InodeGCLease> second{mount->stealInodeGCLease()};
    EXPECT_FALSE(mount->tryStartInodeGC());

    if (releaseFirstInhibitorFirst) {
      first.reset();
    } else {
      second.reset();
    }
    EXPECT_FALSE(mount->tryStartInodeGC());

    first.reset();
    second.reset();
    EXPECT_TRUE(mount->tryStartInodeGC());
  }
}

TEST_P(
    CheckoutTest,
    changing_hash_scheme_does_not_conflict_if_contents_are_same) {
  TestMount mount;
  applyParam(mount);
  auto backingStore = mount.getBackingStore();

  folly::ByteRange contents = folly::StringPiece{"test contents\n"};

  auto builder1 = FakeTreeBuilder();
  builder1.setFile("a/test.txt"_relpath, contents, false, ObjectId{"object1"});

  auto builder2 = FakeTreeBuilder();
  builder2.setFile("a/test.txt"_relpath, contents, false, ObjectId{"object2"});
  builder2.finalize(backingStore, /*setReady=*/true);
  auto commit2 = backingStore->putCommit("2", builder2);
  commit2->setReady();

  // Initialize the mount with the tree data from builder1
  mount.initialize(RootId{"1"}, builder1);

  auto executor = mount.getServerExecutor().get();

  // Load a/test.txt
  auto preInode = mount.getFileInode("a/test.txt");

  // At this point, the working copy references the hash scheme used in commit1.

  auto result = mount.getEdenMount()
                    ->checkout(
                        mount.getRootInode(),
                        RootId{"2"},
                        ObjectFetchContext::getNullContext(),
                        __func__)
                    .semi()
                    .via(executor)
                    .getVia(executor);
  EXPECT_EQ(0, result.conflicts.size());

  // Call resetParent() to make the mount point back at commit1, even though
  // the file state is from commit2.  They have the same contents, so a second
  // checkout also should not produce conflicts.
  mount.getEdenMount()->resetParent(RootId{"1"});

  result = mount.getEdenMount()
               ->checkout(
                   mount.getRootInode(),
                   RootId{"2"},
                   ObjectFetchContext::getNullContext(),
                   __func__)
               .semi()
               .via(executor)
               .getVia(executor);
  EXPECT_EQ(0, result.conflicts.size());
}

#ifdef _WIN32
using ActionMap = std::
    unordered_map<RelativePathPiece, std::function<void(RelativePathPiece)>>;
constexpr size_t kTraceBusCapacity = 25000;

class FakePrjfsChannel final : public PrjfsChannel {
 public:
  FakePrjfsChannel(ActionMap actions, const std::shared_ptr<EdenMount>& mount)
      : PrjfsChannel(
            mount->getPath(),
            EdenDispatcherFactory::makePrjfsDispatcher(mount.get()),
            mount->getServerState()->getReloadableConfig(),
            &mount->getStraceLogger(),
            mount->getServerState()->getEdenFsEventsLogger(),
            mount->getServerState()->getFaultInjector(),
            mount->getServerState()->getProcessInfoCache(),
            mount->getCheckoutConfig()->getRepoGuid(),
            true,
            nullptr,
            mount->getInvalidationThreadPool()),
        actions_{std::move(actions)} {}

  static void initializeFakePrjfsChannel(
      ActionMap actions,
      std::shared_ptr<EdenMount> mount) {
    auto channel = std::unique_ptr<FakePrjfsChannel, FsChannelDeleter>(
        new FakePrjfsChannel(std::move(actions), mount));
    channel->initialize();
    mount->setTestFsChannel(std::move(channel));
  }

  folly::Try<folly::Unit> removeCachedFile(RelativePathPiece path) override {
    if (auto it = actions_.find(path); it != actions_.end()) {
      it->second(path);
    }
    return PrjfsChannel::removeCachedFile(path);
  }

 private:
  ActionMap actions_;
};

TEST_P(CheckoutTest, concurrent_crawl_during_checkout) {
  TestMount mount;
  applyParam(mount);
  auto backingStore = mount.getBackingStore();

  auto builder1 = FakeTreeBuilder();
  builder1.setFile("a/1.txt"_relpath, "content1", false);

  mount.initialize(RootId{"1"}, builder1);

  auto builder2 = builder1.clone();
  builder2.setFile("a/2.txt"_relpath, "content2", false);
  builder2.finalize(backingStore, /*setReady=*/true);
  auto commit2 = backingStore->putCommit("2", builder2);
  commit2->setReady();

  ActionMap actions;
  actions["a"_relpath] = [&](RelativePathPiece path) {
    auto oneTxt = mount.getEdenMount()->getPath() + path + "1.txt"_pc;
    // Read a file to force removeCachedFile to fail.
    readFile(oneTxt).throwUnlessValue();
  };

  FakePrjfsChannel::initializeFakePrjfsChannel(
      std::move(actions), mount.getEdenMount());

  auto fut = mount.getEdenMount()
                 ->checkout(
                     mount.getRootInode(),
                     RootId{"2"},
                     ObjectFetchContext::getNullContext(),
                     __func__)
                 .semi()
                 .via(mount.getServerExecutor().get());

  // Several executors are involved in checkout, some of which aren't the
  // server executor, thus we need to loop several times to make sure they all
  // executed and pushed work to each other.
  while (!fut.isReady()) {
    mount.drainServerExecutor();
  }
  auto result = std::move(fut).get(0ms);
  EXPECT_THAT(result.conflicts, UnorderedElementsAre());

  mount.getEdenMount()->getPrjfsChannel()->unmount({}).get();
}

TEST_P(CheckoutTest, concurrent_file_to_directory_during_checkout) {
  TestMount mount;
  applyParam(mount);
  auto backingStore = mount.getBackingStore();

  auto builder1 = FakeTreeBuilder();
  builder1.setFile("a/1.txt"_relpath, "content1", false);
  builder1.setFile("b.txt"_relpath, "content1", false);

  mount.initialize(RootId{"1"}, builder1);

  auto builder2 = builder1.clone();
  builder2.removeFile("b.txt"_relpath);
  builder2.finalize(backingStore, /*setReady=*/true);
  auto commit2 = backingStore->putCommit("2", builder2);
  commit2->setReady();

  auto bTxt = mount.getEdenMount()->getPath() + "b.txt"_pc;

  ActionMap actions;
  actions["b.txt"_relpath] = [&](RelativePathPiece) {
    // Replace the file by a directory.
    removeFileWithAbsolutePath(bTxt);
    ensureDirectoryExists(bTxt);
    writeFile(bTxt + "2.txt"_pc, folly::StringPiece{"2"}).throwUnlessValue();
  };

  FakePrjfsChannel::initializeFakePrjfsChannel(
      std::move(actions), mount.getEdenMount());

  readFile(bTxt).throwUnlessValue();

  auto fut = mount.getEdenMount()
                 ->checkout(
                     mount.getRootInode(),
                     RootId{"2"},
                     ObjectFetchContext::getNullContext(),
                     __func__)
                 .semi()
                 .via(mount.getServerExecutor().get());

  // Several executors are involved in checkout, some of which aren't the
  // server executor, thus we need to loop several times to make sure they all
  // executed and pushed work to each other.
  while (!fut.isReady()) {
    mount.drainServerExecutor();
  }
  auto result = std::move(fut).get(0ms);
  EXPECT_THAT(
      result.conflicts,
      UnorderedElementsAre(makeConflict(
          ConflictType::MODIFIED_REMOVED, "b.txt", "", Dtype::REGULAR)));

  mount.getEdenMount()->getPrjfsChannel()->unmount({}).get();
}

TEST_P(CheckoutTest, concurrent_new_file_during_checkout) {
  TestMount mount;
  applyParam(mount);
  auto backingStore = mount.getBackingStore();

  auto builder1 = FakeTreeBuilder();
  builder1.setFile("a/1.txt"_relpath, "content1", false);
  builder1.setFile("b.txt"_relpath, "content1", false);

  mount.initialize(RootId{"1"}, builder1);

  auto builder2 = builder1.clone();
  builder2.setFile("a/2.txt"_relpath, "content2", false);
  builder2.finalize(backingStore, /*setReady=*/true);
  auto commit2 = backingStore->putCommit("2", builder2);
  commit2->setReady();

  auto twoTxt = mount.getEdenMount()->getPath() + "a/2.txt"_relpath;

  ActionMap actions;
  actions["a/2.txt"_relpath] = [&](RelativePathPiece) {
    // Create a directory
    removeFileWithAbsolutePath(twoTxt);
    ensureDirectoryExists(twoTxt);
    writeFile(twoTxt + "a"_pc, folly::StringPiece{"A"}).throwUnlessValue();
  };

  FakePrjfsChannel::initializeFakePrjfsChannel(
      std::move(actions), mount.getEdenMount());

  readFile(mount.getEdenMount()->getPath() + "a/1.txt"_relpath)
      .throwUnlessValue();

  auto fut = mount.getEdenMount()
                 ->checkout(
                     mount.getRootInode(),
                     RootId{"2"},
                     ObjectFetchContext::getNullContext(),
                     __func__)
                 .semi()
                 .via(mount.getServerExecutor().get());

  // Several executors are involved in checkout, some of which aren't the
  // server executor, thus we need to loop several times to make sure they all
  // executed and pushed work to each other.
  while (!fut.isReady()) {
    mount.drainServerExecutor();
  }
  auto result = std::move(fut).get(0ms);
  EXPECT_THAT(
      result.conflicts,
      UnorderedElementsAre(makeConflict(
          ConflictType::UNTRACKED_ADDED, "a/2.txt", "", Dtype::REGULAR)));

  mount.getEdenMount()->getPrjfsChannel()->unmount({}).get();
}

TEST_P(CheckoutTest, concurrent_recreation_during_checkout) {
  TestMount mount;
  applyParam(mount);
  auto backingStore = mount.getBackingStore();

  auto builder1 = FakeTreeBuilder();
  builder1.setFile("a/1.txt"_relpath, "content1", false);
  builder1.setFile("b.txt"_relpath, "content1", false);

  mount.initialize(RootId{"1"}, builder1);

  auto builder2 = builder1.clone();
  builder2.setFile("a/2.txt"_relpath, "content2", false);
  builder2.finalize(backingStore, /*setReady=*/true);
  auto commit2 = backingStore->putCommit("2", builder2);
  commit2->setReady();

  auto oneTxt = mount.getEdenMount()->getPath() + "a/1.txt"_relpath;

  ActionMap actions;
  actions["a/1.txt"_relpath] = [&](RelativePathPiece) {
    // Create a directory
    removeFileWithAbsolutePath(oneTxt);
    ensureDirectoryExists(oneTxt);
    writeFile(oneTxt + "a"_pc, folly::StringPiece{"A"}).throwUnlessValue();
  };

  FakePrjfsChannel::initializeFakePrjfsChannel(
      std::move(actions), mount.getEdenMount());

  readFile(mount.getEdenMount()->getPath() + "a/1.txt"_relpath)
      .throwUnlessValue();
  mount.deleteFile("a/1.txt");

  auto fut = mount.getEdenMount()
                 ->checkout(
                     mount.getRootInode(),
                     RootId{"2"},
                     ObjectFetchContext::getNullContext(),
                     __func__,
                     CheckoutMode::FORCE)
                 .semi()
                 .via(mount.getServerExecutor().get());

  // Several executors are involved in checkout, some of which aren't the
  // server executor, thus we need to loop several times to make sure they all
  // executed and pushed work to each other.
  while (!fut.isReady()) {
    mount.drainServerExecutor();
  }
  auto result = std::move(fut).get(0ms);
  EXPECT_THAT(
      result.conflicts,
      UnorderedElementsAre(
          makeConflict(
              ConflictType::REMOVED_MODIFIED, "a/1.txt", "", Dtype::REGULAR),
          makeConflict(
              ConflictType::MODIFIED_MODIFIED, "a/1.txt", "", Dtype::REGULAR)));

  std::vector<FileChangeJournalDelta> expected_journal;
  // Only one changed here, the journal joins the two adjacent changes
  expected_journal.emplace_back(
      std::forward<FileChangeJournalDelta>(FileChangeJournalDelta(
          RelativePathPiece("a/1.txt"),
          dtype_t::Regular,
          FileChangeJournalDelta::CHANGED)));
  expected_journal.emplace_back(
      std::forward<FileChangeJournalDelta>(FileChangeJournalDelta(
          RelativePathPiece("a/1.txt"),
          dtype_t::Regular,
          FileChangeJournalDelta::CREATED)));

  checkFileChangeJournalEntries(expected_journal, mount);

  mount.getEdenMount()->getPrjfsChannel()->unmount({}).get();
}

#endif

#ifndef _WIN32

TEST_P(CheckoutTest, overlayWritesDuringCheckout) {
  // To trigger the N+1 overlay write problem, subdirectories must change
  // materialization state during checkout. If they stay materialized,
  // they don't trigger an overlay write for their parent.
  //
  // Setup: materialize each sub by modifying its file, then FORCE checkout
  // to a commit with different file content. This causes children to change
  // from materialized to unmaterialized, exposing the excessive overlay writes
  // of the parent.
  auto builder1 = FakeTreeBuilder();
  builder1.setFile("parent/sub1/file1.txt", "original1\n");
  builder1.setFile("parent/sub2/file2.txt", "original2\n");
  builder1.setFile("parent/sub3/file3.txt", "original3\n");
  builder1.setFile("parent/sub4/file4.txt", "original4\n");
  builder1.setFile("parent/sub5/file5.txt", "original5\n");
  TestMount testMount{builder1};
  applyParam(testMount);

  // Materialize each subdirectory by modifying its file
  testMount.overwriteFile("parent/sub1/file1.txt", "local1\n");
  testMount.overwriteFile("parent/sub2/file2.txt", "local2\n");
  testMount.overwriteFile("parent/sub3/file3.txt", "local3\n");
  testMount.overwriteFile("parent/sub4/file4.txt", "local4\n");
  testMount.overwriteFile("parent/sub5/file5.txt", "local5\n");

  // Prepare a second tree with different file content
  auto builder2 = builder1.clone();
  builder2.replaceFile("parent/sub1/file1.txt", "modified1\n");
  builder2.replaceFile("parent/sub2/file2.txt", "modified2\n");
  builder2.replaceFile("parent/sub3/file3.txt", "modified3\n");
  builder2.replaceFile("parent/sub4/file4.txt", "modified4\n");
  builder2.replaceFile("parent/sub5/file5.txt", "modified5\n");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit(RootId{"2"}, builder2);
  commit2->setReady();

  // Record the overlay write count before checkout
  auto& stats = testMount.getServerState()->getStats();
  stats->flush();
  auto data = facebook::fb303::ServiceData::get();
  constexpr folly::StringPiece key =
      "overlay.save_overlay_dir_successful.sum.60";
  auto initialWrites = data->getCounter(key);

  // FORCE checkout to overwrite local modifications so subs can dematerialize
  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"2"},
                                ObjectFetchContext::getNullContext(),
                                __func__,
                                CheckoutMode::FORCE)
                            .semi()
                            .via(executor);
  testMount.drainServerExecutor();
  ASSERT_TRUE(checkoutResult.isReady());
  auto result = std::move(checkoutResult).get();

  // Count the overlay writes that occurred during checkout.
  stats->flush();
  auto checkoutWrites = data->getCounter(key) - initialWrites;

  // The read-side WAL flush is deferred during checkout (see
  // enterCheckoutDeferral), so WAL and non-WAL configs perform the same number
  // of checkout-time overlay writes.
  constexpr int64_t expected = 7;
  EXPECT_EQ(expected, checkoutWrites)
      << "Overlay writes during checkout: " << checkoutWrites;

  // Verify files have new content
  EXPECT_FILE_INODE(
      testMount.getFileInode("parent/sub1/file1.txt"), "modified1\n", 0644);
  EXPECT_FILE_INODE(
      testMount.getFileInode("parent/sub2/file2.txt"), "modified2\n", 0644);
  EXPECT_FILE_INODE(
      testMount.getFileInode("parent/sub3/file3.txt"), "modified3\n", 0644);
  EXPECT_FILE_INODE(
      testMount.getFileInode("parent/sub4/file4.txt"), "modified4\n", 0644);
  EXPECT_FILE_INODE(
      testMount.getFileInode("parent/sub5/file5.txt"), "modified5\n", 0644);

  // Remount and verify persistence, showing the overlay was written correctly.
  testMount.remount();
  EXPECT_FILE_INODE(
      testMount.getFileInode("parent/sub1/file1.txt"), "modified1\n", 0644);
  EXPECT_FILE_INODE(
      testMount.getFileInode("parent/sub2/file2.txt"), "modified2\n", 0644);
  EXPECT_FILE_INODE(
      testMount.getFileInode("parent/sub3/file3.txt"), "modified3\n", 0644);
  EXPECT_FILE_INODE(
      testMount.getFileInode("parent/sub4/file4.txt"), "modified4\n", 0644);
  EXPECT_FILE_INODE(
      testMount.getFileInode("parent/sub5/file5.txt"), "modified5\n", 0644);
}

TEST_P(CheckoutTest, backgroundOverlayCleanupDuringCheckout) {
  // Verify that overlay data for unloaded directories is cleaned up
  // asynchronously by the GC thread during checkout.
  auto builder1 = FakeTreeBuilder{};
  builder1.setFile("dir/sub/file.txt", "contents");
  TestMount testMount{builder1};
  applyParam(testMount);
  testMount.updateEdenConfig(
      {{"experimental:background-overlay-cleanup-during-checkout", "true"}});

  // Prepare a second commit where dir/sub is replaced with a different tree.
  auto builder2 = FakeTreeBuilder{};
  builder2.setFile("dir/sub2/other.txt", "other");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit("2", builder2);
  commit2->setReady();

  // Load "dir/sub" so it gets overlay data, then unload it.
  auto subTree = testMount.getTreeInode("dir/sub"_relpath);
  auto subInodeNumber = subTree->getNodeId();
  subTree.reset();

  // Remount so "dir/sub" is unloaded but has overlay data.
  testMount.remountGracefully();

  EXPECT_TRUE(testMount.hasOverlayDir(subInodeNumber));

  // Checkout to the second commit.
  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId("2"),
                                ObjectFetchContext::getNullContext(),
                                __func__)
                            .semi()
                            .via(executor)
                            .getVia(executor);
  EXPECT_EQ(0, checkoutResult.conflicts.size());

  // Wait for background GC to complete.
  testMount.getEdenMount()->getOverlay()->flushPendingAsync().get(60s);

  // Overlay data should be cleaned up by the background GC thread.
  EXPECT_FALSE(testMount.hasOverlayDir(subInodeNumber));
}

TEST(Checkout, forceCheckoutRemovesLoadedRestrictedTree) {
  auto currentBuilder = FakeTreeBuilder{};
  currentBuilder.setFile("project/notes/readme.md", "current note\n");
  currentBuilder.setFile(
      "project/notes/restricted_child/secret.txt", "secret\n");
  currentBuilder.setDirIsRestricted("project/notes/restricted_child");
  TestMount testMount{RootId{"current"}, currentBuilder};

  auto restrictedTree =
      testMount.getTreeInode("project/notes/restricted_child"_relpath);
  ASSERT_TRUE(restrictedTree->isRestricted());
  auto restrictedInodeNumber = restrictedTree->getNodeId();
  restrictedTree->incFsRefcount();
  restrictedTree.reset();

  auto targetBuilder = FakeTreeBuilder{};
  targetBuilder.setFile("project/notes/readme.md", "target note\n");
  targetBuilder.finalize(testMount.getBackingStore(), true);
  auto targetCommit =
      testMount.getBackingStore()->putCommit(RootId{"target"}, targetBuilder);
  targetCommit->setReady();

  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"target"},
                                ObjectFetchContext::getNullContext(),
                                __func__,
                                CheckoutMode::FORCE)
                            .semi()
                            .via(executor);
  testMount.drainServerExecutor();
  ASSERT_TRUE(checkoutResult.isReady());
  EXPECT_EQ(0, checkoutResult.value().conflicts.size());

  restrictedTree =
      testMount.getEdenMount()
          ->getInodeMap()
          ->lookupTreeInode(
              restrictedInodeNumber, ObjectFetchContext::getNullContext())
          .get(1ms);
  EXPECT_TRUE(restrictedTree->isUnlinked());

  EXPECT_THROW_ERRNO(
      testMount.getTreeInode("project/notes/restricted_child"_relpath), ENOENT);
  testMount.getEdenMount()->getInodeMap()->decFsRefcount(restrictedInodeNumber);
}

static void runDryRunRemovesLoadedRestrictedTreeWithoutDeadlock() {
  alarm(10);

  auto currentBuilder = FakeTreeBuilder{};
  currentBuilder.setFile("project/notes/readme.md", "current note\n");
  currentBuilder.setFile(
      "project/notes/restricted_child/secret.txt", "secret\n");
  currentBuilder.setDirIsRestricted("project/notes/restricted_child");
  TestMount testMount{RootId{"current"}, currentBuilder};

  auto restrictedTree =
      testMount.getTreeInode("project/notes/restricted_child"_relpath);
  if (!restrictedTree->isRestricted()) {
    _exit(1);
  }
  restrictedTree->incFsRefcount();
  // Leave the child loaded with only the parent DirEntry's raw pointer and an
  // fs refcount, matching a kernel-referenced inode with no InodePtr owners.
  auto* restrictedTreeRaw = restrictedTree.get();
  restrictedTree.reset();

  auto parent = testMount.getTreeInode("project/notes"_relpath);
  {
    auto contents = parent->lockContentsWrite();
    auto childPtr = InodePtr::newPtrLocked(restrictedTreeRaw);
    (void)contents;
    (void)childPtr;
  }
  _exit(0);
}

TEST(Checkout, dryRunRemovesLoadedRestrictedTreeWithoutDeadlock) {
  ASSERT_EXIT(
      runDryRunRemovesLoadedRestrictedTreeWithoutDeadlock(),
      ::testing::ExitedWithCode(0),
      "");
}

TEST(Checkout, forceCheckoutReplacesLoadedRestrictedTreeWithFile) {
  auto currentBuilder = FakeTreeBuilder{};
  currentBuilder.setFile("project/notes/readme.md", "current note\n");
  currentBuilder.setFile(
      "project/notes/restricted_child/secret.txt", "secret\n");
  currentBuilder.setDirIsRestricted("project/notes/restricted_child");
  auto targetBuilder = currentBuilder.clone();
  targetBuilder.replaceFile("project/notes/restricted_child", "replacement\n");
  TestMount testMount{RootId{"current"}, currentBuilder};

  auto restrictedTree =
      testMount.getTreeInode("project/notes/restricted_child"_relpath);
  ASSERT_TRUE(restrictedTree->isRestricted());
  auto restrictedInodeNumber = restrictedTree->getNodeId();
  restrictedTree->incFsRefcount();
  restrictedTree.reset();

  targetBuilder.finalize(testMount.getBackingStore(), true);
  auto targetCommit =
      testMount.getBackingStore()->putCommit(RootId{"target"}, targetBuilder);
  targetCommit->setReady();

  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"target"},
                                ObjectFetchContext::getNullContext(),
                                __func__,
                                CheckoutMode::FORCE)
                            .semi()
                            .via(executor);
  testMount.drainServerExecutor();
  ASSERT_TRUE(checkoutResult.isReady());
  EXPECT_EQ(0, checkoutResult.value().conflicts.size());

  restrictedTree =
      testMount.getEdenMount()
          ->getInodeMap()
          ->lookupTreeInode(
              restrictedInodeNumber, ObjectFetchContext::getNullContext())
          .get(1ms);
  EXPECT_TRUE(restrictedTree->isUnlinked());
  EXPECT_FILE_INODE(
      testMount.getFileInode("project/notes/restricted_child"_relpath),
      "replacement\n",
      0644);

  testMount.getEdenMount()->getInodeMap()->decFsRefcount(restrictedInodeNumber);
}

TEST_P(CheckoutTest, checkoutUpdatesLoadedRestrictedChildPlaceholderMetadata) {
  auto currentBuilder = FakeTreeBuilder{};
  currentBuilder.setFile("project/notes/readme.md", "current note\n");
  currentBuilder.setFile(
      "project/notes/restricted_child/secret.txt", "current secret\n");
  currentBuilder.setDirIsRestricted("project/notes/restricted_child");
  TestMount testMount{RootId{"current"}, currentBuilder};
  applyParam(testMount);

  auto parent = testMount.getTreeInode("project/notes"_relpath);
  auto restrictedTree =
      testMount.getTreeInode("project/notes/restricted_child"_relpath);
  ASSERT_TRUE(restrictedTree->isRestricted());
  auto oldRestrictedTreeId = restrictedTree->getObjectId();
  ASSERT_TRUE(oldRestrictedTreeId.has_value());

  auto targetBuilder = currentBuilder.clone();
  targetBuilder.replaceFile(
      "project/notes/restricted_child/secret.txt", "target secret\n");
  targetBuilder.finalize(testMount.getBackingStore(), true);
  auto targetRestrictedTreeId =
      targetBuilder.getStoredTree("project/notes/restricted_child"_relpath)
          ->get()
          .getObjectId();
  ASSERT_NE(*oldRestrictedTreeId, targetRestrictedTreeId);

  auto targetCommit =
      testMount.getBackingStore()->putCommit(RootId{"target"}, targetBuilder);
  targetCommit->setReady();

  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"target"},
                                ObjectFetchContext::getNullContext(),
                                __func__,
                                CheckoutMode::NORMAL)
                            .semi()
                            .via(executor);
  testMount.drainServerExecutor();
  ASSERT_TRUE(checkoutResult.isReady());
  EXPECT_EQ(0, std::move(checkoutResult).get().conflicts.size());

  {
    auto contents = parent->lockContentsRead();
    auto it = contents->entries.find("restricted_child"_pc);
    ASSERT_NE(it, contents->entries.end());
    EXPECT_TRUE(it->second.isRestricted());
    EXPECT_EQ(targetRestrictedTreeId, it->second.getObjectId());
  }

  auto updatedRestrictedTreeId = restrictedTree->getObjectId();
  ASSERT_TRUE(updatedRestrictedTreeId.has_value());
  EXPECT_EQ(targetRestrictedTreeId, *updatedRestrictedTreeId);
}

static void runCheckoutWithNonEmptyRestrictedPlaceholder() {
  auto currentBuilder = FakeTreeBuilder{};
  currentBuilder.setFile("restricted_child/secret.txt", "current secret\n");
  currentBuilder.setDirIsRestricted("restricted_child");
  TestMount testMount{RootId{"current"}, currentBuilder};

  auto parent = testMount.getRootInode();
  auto restrictedTree = testMount.getTreeInode("restricted_child"_relpath);
  ASSERT_TRUE(restrictedTree->isRestricted());
  auto oldRestrictedTreeId = restrictedTree->getObjectId();
  ASSERT_TRUE(oldRestrictedTreeId.has_value());
  auto oldRestrictedAclRootState = restrictedTree->aclRootState();

  ObjectId oldParentTreeId;
  AclRootState oldParentAclRootState;
  {
    auto contents = parent->lockContentsRead();
    auto it = contents->entries.find("restricted_child"_pc);
    ASSERT_NE(it, contents->entries.end());
    oldParentTreeId = it->second.getObjectId();
    oldParentAclRootState = it->second.aclRootState();
  }

  const auto staleInodeNumber =
      testMount.getEdenMount()->getOverlay()->allocateInodeNumber();
  {
    auto contents = restrictedTree->getContentsUnchecked().wlock();
    auto [_, inserted] = contents->entries.emplace(
        "stale.txt"_pc, S_IFREG | 0644, staleInodeNumber);
    ASSERT_TRUE(inserted);
  }

  auto targetBuilder = currentBuilder.clone();
  targetBuilder.replaceFile("restricted_child/secret.txt", "target secret\n");
  targetBuilder.finalize(testMount.getBackingStore(), true);
  auto targetRestrictedTreeId =
      targetBuilder.getStoredTree("restricted_child"_relpath)
          ->get()
          .getObjectId();
  ASSERT_NE(*oldRestrictedTreeId, targetRestrictedTreeId);

  auto targetCommit =
      testMount.getBackingStore()->putCommit(RootId{"target"}, targetBuilder);
  targetCommit->setReady();

  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"target"},
                                ObjectFetchContext::getNullContext(),
                                __func__,
                                CheckoutMode::NORMAL)
                            .semi()
                            .via(executor);
  testMount.drainServerExecutor();
  ASSERT_TRUE(checkoutResult.isReady());
  EXPECT_EQ(0, std::move(checkoutResult).get().conflicts.size());

  {
    auto contents = parent->lockContentsRead();
    auto it = contents->entries.find("restricted_child"_pc);
    ASSERT_NE(it, contents->entries.end());
    EXPECT_EQ(oldParentTreeId, it->second.getObjectId());
    EXPECT_EQ(oldParentAclRootState, it->second.aclRootState());
  }

  auto updatedRestrictedTreeId = restrictedTree->getObjectId();
  ASSERT_TRUE(updatedRestrictedTreeId.has_value());
  EXPECT_EQ(*oldRestrictedTreeId, *updatedRestrictedTreeId);
  EXPECT_EQ(oldRestrictedAclRootState, restrictedTree->aclRootState());
  auto restrictedContents = restrictedTree->getContentsUnchecked().rlock();
  EXPECT_EQ(1, restrictedContents->entries.size());
  EXPECT_NE(
      restrictedContents->entries.end(),
      restrictedContents->entries.find("stale.txt"_pc));
}

TEST(Checkout, checkoutRefusesNonEmptyRestrictedPlaceholder) {
#ifdef NDEBUG
  runCheckoutWithNonEmptyRestrictedPlaceholder();
#else
  GTEST_FLAG_SET(death_test_style, "threadsafe");
  ASSERT_DEATH(
      runCheckoutWithNonEmptyRestrictedPlaceholder(), "placeholderIsEmpty");
#endif
}

#ifndef _WIN32
TEST_P(CheckoutTest, forceCheckoutKeepsRememberedRestrictedChildOpaque) {
  auto currentBuilder = FakeTreeBuilder{};
  currentBuilder.setFile("restricted_child/secret.txt", "current secret\n");
  currentBuilder.setDirIsRestricted("restricted_child");
  TestMount testMount{RootId{"current"}, currentBuilder};
  applyParam(testMount);

  auto restrictedTree = testMount.getTreeInode("restricted_child"_relpath);
  ASSERT_TRUE(restrictedTree->isRestricted());
  auto currentRestrictedTreeId = restrictedTree->getObjectId();
  ASSERT_TRUE(currentRestrictedTreeId.has_value());
  auto restrictedInodeNumber = restrictedTree->getNodeId();

  // First update the loaded restricted placeholder. This creates the legacy
  // child overlay record that survives takeover in the production failure.
  auto backingStore = testMount.getBackingStore();
  auto intermediateBuilder = currentBuilder.clone();
  intermediateBuilder.replaceFile(
      "restricted_child/secret.txt", "intermediate secret\n");
  intermediateBuilder.finalize(backingStore, true);
  auto* intermediateRootTree =
      intermediateBuilder.getStoredTree(RelativePathPiece{});
  const auto intermediateEntry =
      intermediateRootTree->get().find("restricted_child"_pc);
  ASSERT_NE(intermediateEntry, intermediateRootTree->get().cend());
  ASSERT_TRUE(intermediateEntry->second.isRestricted());
  auto oldRestrictedTreeId = intermediateEntry->second.getObjectId();
  ASSERT_NE(*currentRestrictedTreeId, oldRestrictedTreeId);

  auto intermediateCommit =
      backingStore->putCommit(RootId{"intermediate"}, intermediateBuilder);
  intermediateCommit->setReady();
  auto executor = testMount.getServerExecutor().get();
  auto intermediateCheckout = testMount.getEdenMount()
                                  ->checkout(
                                      testMount.getRootInode(),
                                      RootId{"intermediate"},
                                      ObjectFetchContext::getNullContext(),
                                      __func__,
                                      CheckoutMode::FORCE)
                                  .semi()
                                  .via(executor);
  testMount.drainServerExecutor();
  ASSERT_TRUE(intermediateCheckout.isReady());
  EXPECT_EQ(0, std::move(intermediateCheckout).get().conflicts.size());
  ASSERT_TRUE(testMount.hasOverlayDir(restrictedInodeNumber));
  auto intermediatePlaceholderId = restrictedTree->getObjectId();
  ASSERT_TRUE(intermediatePlaceholderId.has_value());
  ASSERT_EQ(oldRestrictedTreeId, *intermediatePlaceholderId);

  // Retain the kernel reference and inode number across takeover, then remount
  // with the legacy overlay directory present and the child inode unloaded.
  restrictedTree->incFsRefcount();
  restrictedTree.reset();
  testMount.remountGracefully();

  auto parent = testMount.getRootInode();
  auto* inodeMap = testMount.getEdenMount()->getInodeMap();
  SCOPE_EXIT {
    inodeMap->decFsRefcount(restrictedInodeNumber);
  };

  ASSERT_TRUE(inodeMap->isInodeRemembered(restrictedInodeNumber));
  ASSERT_TRUE(testMount.hasOverlayDir(restrictedInodeNumber));
  ASSERT_EQ(nullptr, inodeMap->lookupLoadedTree(restrictedInodeNumber).get());
  {
    auto contents = parent->lockContentsRead();
    auto it = contents->entries.find("restricted_child"_pc);
    ASSERT_NE(it, contents->entries.end());
    ASSERT_EQ(nullptr, it->second.getInode());
    ASSERT_EQ(restrictedInodeNumber, it->second.getInodeNumber());
    ASSERT_TRUE(it->second.isRestricted());
    ASSERT_EQ(oldRestrictedTreeId, it->second.getObjectId());
  }

  auto targetBuilder = intermediateBuilder.clone();
  targetBuilder.finalize(backingStore, true);
  auto* targetRootTree = targetBuilder.getStoredTree(RelativePathPiece{});
  const auto targetEntry = targetRootTree->get().find("restricted_child"_pc);
  ASSERT_NE(targetEntry, targetRootTree->get().cend());
  ASSERT_TRUE(targetEntry->second.isRestricted());
  auto targetRestrictedTreeId = targetEntry->second.getObjectId();
  auto targetAclRootState = targetEntry->second.aclRootState();
  ASSERT_EQ(oldRestrictedTreeId, targetRestrictedTreeId);
  ASSERT_EQ(intermediateEntry->second.aclRootState(), targetAclRootState);
  const auto restrictedTreeAccessCount =
      backingStore->getAccessCount(oldRestrictedTreeId);

  auto targetCommit = backingStore->putCommit(RootId{"target"}, targetBuilder);
  targetCommit->setReady();

  executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"target"},
                                ObjectFetchContext::getNullContext(),
                                __func__,
                                CheckoutMode::FORCE)
                            .semi()
                            .via(executor);
  testMount.drainServerExecutor();
  ASSERT_TRUE(checkoutResult.isReady());
  EXPECT_EQ(0, std::move(checkoutResult).get().conflicts.size());
  EXPECT_EQ(
      restrictedTreeAccessCount,
      backingStore->getAccessCount(targetRestrictedTreeId));

  {
    auto contents = parent->lockContentsRead();
    auto it = contents->entries.find("restricted_child"_pc);
    ASSERT_NE(it, contents->entries.end());
    EXPECT_NE(nullptr, it->second.getInode());
    EXPECT_EQ(restrictedInodeNumber, it->second.getInodeNumber());
    EXPECT_TRUE(it->second.isRestricted());
    EXPECT_EQ(targetAclRootState, it->second.aclRootState());
    EXPECT_EQ(targetRestrictedTreeId, it->second.getObjectId());
  }
  restrictedTree = inodeMap->lookupLoadedTree(restrictedInodeNumber);
  ASSERT_NE(nullptr, restrictedTree.get());
  auto updatedRestrictedTreeId = restrictedTree->getObjectId();
  ASSERT_TRUE(updatedRestrictedTreeId.has_value());
  EXPECT_EQ(targetRestrictedTreeId, *updatedRestrictedTreeId);
  EXPECT_TRUE(restrictedTree->isRestricted());
  EXPECT_EQ(targetAclRootState, restrictedTree->aclRootState());
  {
    auto restrictedContents = restrictedTree->getContentsUnchecked().rlock();
    EXPECT_TRUE(restrictedContents->entries.empty());
  }
}
#endif

TEST_P(
    CheckoutTest,
    checkoutUpdatesLoadedRestrictedChildWhenOnlyFetchedTreeIsRestricted) {
  auto currentBuilder = FakeTreeBuilder{};
  currentBuilder.setFile("restricted_child/secret.txt", "current secret\n");
  currentBuilder.setDirIsRestricted("restricted_child");
  TestMount testMount{RootId{"current"}, currentBuilder};
  applyParam(testMount);

  auto parent = testMount.getRootInode();
  auto restrictedTree = testMount.getTreeInode("restricted_child"_relpath);
  ASSERT_TRUE(restrictedTree->isRestricted());
  auto oldRestrictedTreeId = restrictedTree->getObjectId();
  ASSERT_TRUE(oldRestrictedTreeId.has_value());

  auto backingStore = testMount.getBackingStore();
  auto [secretBlob, secretBlobId] = backingStore->putBlob("target secret\n");
  secretBlob->setReady();

  auto* targetRestrictedTree = backingStore->putRestrictedTree({
      {"secret.txt", secretBlobId},
  });
  targetRestrictedTree->setReady();
  auto targetRestrictedTreeId = targetRestrictedTree->get().getObjectId();
  ASSERT_NE(*oldRestrictedTreeId, targetRestrictedTreeId);

  Tree::container rootEntries{kPathMapDefaultCaseSensitive};
  rootEntries.emplace(
      "restricted_child"_pc,
      ObjectId{targetRestrictedTreeId},
      TreeEntryType::TREE,
      /* isRestricted */ false,
      /* hasACL */ std::nullopt);
  auto* targetRootTree = backingStore->putTree(std::move(rootEntries));
  targetRootTree->setReady();
  backingStore->putCommit(RootId{"target"}, targetRootTree)->setReady();

  const auto targetEntry = targetRootTree->get().find("restricted_child"_pc);
  ASSERT_NE(targetEntry, targetRootTree->get().cend());
  ASSERT_FALSE(targetEntry->second.isRestricted());
  ASSERT_TRUE(targetRestrictedTree->get().isRestricted());

  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"target"},
                                ObjectFetchContext::getNullContext(),
                                __func__,
                                CheckoutMode::NORMAL)
                            .semi()
                            .via(executor);
  testMount.drainServerExecutor();
  ASSERT_TRUE(checkoutResult.isReady());
  EXPECT_EQ(0, std::move(checkoutResult).get().conflicts.size());

  {
    auto contents = parent->lockContentsRead();
    auto it = contents->entries.find("restricted_child"_pc);
    ASSERT_NE(it, contents->entries.end());
    EXPECT_TRUE(it->second.isRestricted());
    EXPECT_EQ(
        targetRestrictedTree->get().aclRootState(), it->second.aclRootState());
    EXPECT_EQ(targetRestrictedTreeId, it->second.getObjectId());
  }

  auto updatedRestrictedTreeId = restrictedTree->getObjectId();
  ASSERT_TRUE(updatedRestrictedTreeId.has_value());
  EXPECT_EQ(targetRestrictedTreeId, *updatedRestrictedTreeId);
  EXPECT_TRUE(restrictedTree->isRestricted());
  EXPECT_EQ(
      targetRestrictedTree->get().aclRootState(),
      restrictedTree->aclRootState());
  auto restrictedContents = restrictedTree->getContentsUnchecked().rlock();
  EXPECT_TRUE(restrictedContents->entries.empty());
}

#ifndef _WIN32
TEST_P(
    CheckoutTest,
    forceCheckoutRemovesChildThatBecomesRestrictedAfterPlanning) {
  auto currentBuilder = FakeTreeBuilder{};
  currentBuilder.setFile("outer/child/file.txt", "base\n");
  TestMount testMount{RootId{"current"}, currentBuilder};
  applyParam(testMount);

  auto backingStore = testMount.getBackingStore();
  auto targetBuilder = currentBuilder.clone();
  targetBuilder.setDirIsRestricted("outer");
  targetBuilder.finalize(backingStore, true);
  backingStore->putCommit(RootId{"target"}, targetBuilder)->setReady();

  auto outerTreeId =
      currentBuilder.getStoredTree("outer"_relpath)->get().getObjectId();
  auto childTreeId =
      currentBuilder.getStoredTree("outer/child"_relpath)->get().getObjectId();

  // Persist a remembered child whose parent metadata remains stale-allowed.
  auto child = testMount.getTreeInode("outer/child"_relpath);
  ASSERT_FALSE(child->isRestricted());
  auto childInodeNumber = child->getNodeId();
  child->incFsRefcount();
  child.reset();
  testMount.remountGracefully();

  auto* inodeMap = testMount.getEdenMount()->getInodeMap();
  SCOPE_EXIT {
    inodeMap->decFsRefcount(childInodeNumber);
  };
  ASSERT_TRUE(inodeMap->isInodeRemembered(childInodeNumber));

  auto outer = testMount.getTreeInode("outer"_relpath);
  {
    auto contents = outer->lockContentsRead();
    const auto& entry = contents->entries.at("child"_pc);
    ASSERT_EQ(nullptr, entry.getInode());
    ASSERT_FALSE(entry.isRestricted());
  }

  testMount.getTreeCache()->clear();
  backingStore->replaceTreeWithRestricted(outerTreeId)->setReady();
  backingStore->replaceTreeWithRestricted(childTreeId)->setReady();

  // FORCE drops the newly restricted old outer tree, then selects the stale
  // child as local-only before its load discovers the restriction.
  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"target"},
                                ObjectFetchContext::getNullContext(),
                                __func__,
                                CheckoutMode::FORCE)
                            .semi()
                            .via(executor);
  testMount.drainServerExecutor();
  EXPECT_EQ(0, std::move(checkoutResult).get().conflicts.size());

  EXPECT_TRUE(outer->isRestricted());
  child = inodeMap
              ->lookupTreeInode(
                  childInodeNumber, ObjectFetchContext::getNullContext())
              .get(1ms);
  ASSERT_TRUE(child->getObjectId().has_value());
  EXPECT_EQ(childTreeId, *child->getObjectId());
}
#endif

TEST_P(CheckoutTest, checkoutToRestrictedTreeConflictsOnModifiedTrackedFile) {
  auto currentBuilder = FakeTreeBuilder{};
  currentBuilder.setFile("regular/file.txt", "base\n");
  TestMount testMount{RootId{"current"}, currentBuilder};
  applyParam(testMount);

  auto targetBuilder = currentBuilder.clone();
  targetBuilder.replaceFile("regular/file.txt", "target\n");
  targetBuilder.setDirIsRestricted("regular");
  targetBuilder.finalize(testMount.getBackingStore(), true);
  auto targetCommit =
      testMount.getBackingStore()->putCommit(RootId{"target"}, targetBuilder);
  targetCommit->setReady();

  testMount.overwriteFile("regular/file.txt", "local\n");

  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"target"},
                                ObjectFetchContext::getNullContext(),
                                __func__,
                                CheckoutMode::DRY_RUN)
                            .semi()
                            .via(executor)
                            .waitVia(executor);
  ASSERT_TRUE(checkoutResult.isReady());

  EXPECT_THAT(
      std::move(checkoutResult).get().conflicts,
      UnorderedElementsAre(makeConflict(
          ConflictType::MODIFIED_REMOVED,
          "regular/file.txt",
          "",
          Dtype::REGULAR)));
}

TEST_P(
    CheckoutTest,
    checkoutToRestrictedTreeWithConflictDoesNotRestrictModifiedTrackedFile) {
  auto currentBuilder = FakeTreeBuilder{};
  currentBuilder.setFile("regular/file.txt", "base\n");
  TestMount testMount{RootId{"current"}, currentBuilder};
  applyParam(testMount);

  auto targetBuilder = currentBuilder.clone();
  targetBuilder.replaceFile("regular/file.txt", "target\n");
  targetBuilder.setDirIsRestricted("regular");
  targetBuilder.finalize(testMount.getBackingStore(), true);
  auto targetCommit =
      testMount.getBackingStore()->putCommit(RootId{"target"}, targetBuilder);
  targetCommit->setReady();

  testMount.overwriteFile("regular/file.txt", "local\n");

  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"target"},
                                ObjectFetchContext::getNullContext(),
                                __func__,
                                CheckoutMode::NORMAL)
                            .semi()
                            .via(executor)
                            .waitVia(executor);
  ASSERT_TRUE(checkoutResult.isReady());

  EXPECT_THAT(
      std::move(checkoutResult).get().conflicts,
      UnorderedElementsAre(makeConflict(
          ConflictType::MODIFIED_REMOVED,
          "regular/file.txt",
          "",
          Dtype::REGULAR)));
  EXPECT_FALSE(testMount.getTreeInode("regular"_relpath)->isRestricted());
}

TEST_P(CheckoutTest, checkoutToRestrictedTreeConflictsOnUntrackedFile) {
  auto currentBuilder = FakeTreeBuilder{};
  currentBuilder.setFile("existing/file.txt", "base\n");
  TestMount testMount{RootId{"current"}, currentBuilder};
  applyParam(testMount);

  auto targetBuilder = currentBuilder.clone();
  targetBuilder.setFile("local_only_restricted/server.txt", "target\n");
  targetBuilder.setDirIsRestricted("local_only_restricted");
  targetBuilder.finalize(testMount.getBackingStore(), true);
  auto targetCommit =
      testMount.getBackingStore()->putCommit(RootId{"target"}, targetBuilder);
  targetCommit->setReady();

  testMount.mkdir("local_only_restricted");
  testMount.addFile("local_only_restricted/local.txt", "local\n");

  auto executor = testMount.getServerExecutor().get();
  auto dryRunResult = testMount.getEdenMount()
                          ->checkout(
                              testMount.getRootInode(),
                              RootId{"target"},
                              ObjectFetchContext::getNullContext(),
                              __func__,
                              CheckoutMode::DRY_RUN)
                          .semi()
                          .via(executor)
                          .waitVia(executor);
  ASSERT_TRUE(dryRunResult.isReady());
  EXPECT_THAT(
      std::move(dryRunResult).get().conflicts,
      UnorderedElementsAre(makeConflict(
          ConflictType::VISIBLE_RESTRICTED,
          "local_only_restricted/local.txt",
          "",
          Dtype::REGULAR)));
  EXPECT_FALSE(
      testMount.getTreeInode("local_only_restricted"_relpath)->isRestricted());

  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"target"},
                                ObjectFetchContext::getNullContext(),
                                __func__,
                                CheckoutMode::NORMAL)
                            .semi()
                            .via(executor)
                            .waitVia(executor);
  ASSERT_TRUE(checkoutResult.isReady());

  EXPECT_TRUE(std::move(checkoutResult).get().conflicts.empty());
  auto restrictedInode =
      testMount.getTreeInode("local_only_restricted"_relpath);
  EXPECT_TRUE(restrictedInode->isRestricted());
  EXPECT_TRUE(restrictedInode->getContentsUnchecked().rlock()->entries.empty());
}

#endif

/*
 * Test checkout behavior when an ignored, untracked symlink (pointing to a
 * directory) has the same name as a tracked directory in the destination
 * commit.
 *
 * The scenario:
 * 1. Commit 1 has a .gitignore that ignores "src/link"
 * 2. The user creates "src/link" as a symlink pointing to a directory
 *    (ignored, untracked)
 * 3. Commit 2 adds "src/link/" as a tracked directory (containing files)
 * 4. Checkout from commit 1 to commit 2
 *
 * Current behavior: EdenFS reports MODIFIED_MODIFIED because a local
 * FileInode (symlink) exists where the destination tree has a directory.
 * Sapling then fails with "file metadata for <path> not found at source
 * commit" because the symlink was never tracked.
 *
 * Expected behavior: The ignored symlink should be silently replaced by the
 * tracked directory, matching git's behavior of overwriting ignored files.
 */
#ifndef _WIN32
TEST_P(CheckoutTest, ignoredSymlinkReplacedByDirectoryInDestination) {
  // Commit 1: has a .gitignore ignoring "src/link"
  auto builder1 = FakeTreeBuilder();
  builder1.setFile("src/main.c", "// Some code.\n");
  builder1.setFile("src/.gitignore", "link\n");
  TestMount testMount{RootId{"1"}, builder1};
  applyParam(testMount);

  // Commit 2: adds "src/link/" as a tracked directory with a file inside.
  // The symlink name is NOT tracked in commit 1, only as a directory in
  // commit 2.
  auto builder2 = builder1.clone();
  builder2.setFile("src/link/file.md", "# Content\n");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit("2", builder2);
  commit2->setReady();

  // Create the ignored symlink locally, pointing to a real directory
  testMount.mkdir("target_dir");
  testMount.addFile("target_dir/file.txt", "stuff\n");
  testMount.addSymlink("src/link", "target_dir");

  auto executor = testMount.getServerExecutor().get();
  auto checkoutResult = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"2"},
                                ObjectFetchContext::getNullContext(),
                                __func__)
                            .semi()
                            .via(executor);
  testMount.drainServerExecutor();
  ASSERT_TRUE(checkoutResult.isReady());
  auto result = std::move(checkoutResult).get();

  // The symlink is untracked (not in the old tree), so the conflict type
  // should be UNTRACKED_ADDED, not MODIFIED_MODIFIED.
  EXPECT_THAT(
      result.conflicts,
      UnorderedElementsAre(makeConflict(
          ConflictType::UNTRACKED_ADDED, "src/link", "", Dtype::LINK)));
}
#endif

TEST_P(CheckoutTest, directoryReplacedByFileAndRemovedIsModifiedRemoved) {
  auto builder1 = FakeTreeBuilder();
  builder1.setFile("readme.txt", "readme\n");
  builder1.setFile("d/x.txt", "x\n");
  TestMount mount{RootId{"1"}, builder1};
  applyParam(mount);
  auto builder2 = FakeTreeBuilder();
  builder2.setFile("readme.txt", "readme\n");
  builder2.finalize(mount.getBackingStore(), true);
  mount.getBackingStore()->putCommit("2", builder2)->setReady();

  mount.deleteFile("d/x.txt");
  mount.rmdir("d");
  mount.addFile("d", "local file\n");

  auto executor = mount.getServerExecutor().get();
  auto result = mount.getEdenMount()
                    ->checkout(
                        mount.getRootInode(),
                        RootId{"2"},
                        ObjectFetchContext::getNullContext(),
                        __func__,
                        CheckoutMode::NORMAL)
                    .semi()
                    .via(executor)
                    .getVia(executor);
  EXPECT_THAT(
      result.conflicts,
      UnorderedElementsAre(makeConflict(
          ConflictType::MODIFIED_REMOVED, "d", "", Dtype::REGULAR)));
  EXPECT_EQ("local file\n", mount.readFile("d"));
}

} // namespace

// TODO:
// - remove subdirectory
//   - with no untracked/ignored files, it should get removed entirely
//   - remove subdirectory with untracked files
// - add/modify/replace symlink
//
// - change file type:
//   regular -> directory
//   regular -> symlink
//   symlink -> regular
//   symlink -> directory
//   directory -> regular
//   - also with error due to untracked files in directory
//   directory -> symlink
//   - also with error due to untracked files in directory
//
// - conflict handling, with and without --clean
//   - modify file, with removed conflict
//   - modify file, with changed file type conflict
//   - modify file, with a parent directory replaced with a file/symlink
//   - add file, with untracked file/directory/symlink already there
//   - add file, with a parent directory replaced with a file/symlink
//   - remove file, with modify conflict
//   - remove file, with remove conflict
//   - remove file, with a parent directory replaced with a file/symlink

INSTANTIATE_TEST_SUITE_P(
    CheckoutTestVariants,
    CheckoutTest,
    ::testing::Values(false, true),
    [](const ::testing::TestParamInfo<bool>& info) {
      return info.param ? "Coroutines" : "Futures";
    });

TEST_P(CheckoutTest, restrictedDirEntryInsertedOnFileToDirReplace) {
  auto builder1 = FakeTreeBuilder{};
  builder1.setFile("dir/foo", "file contents");

  auto builder2 = FakeTreeBuilder{};
  builder2.setFile("dir/foo/inside.txt", "inside contents");
  builder2.setDirIsRestricted("dir/foo");

  TestMount testMount{builder1};
  applyParam(testMount);

  builder2.finalize(testMount.getBackingStore(), /*setReady=*/true);
  auto commit2 = testMount.getBackingStore()->putCommit(RootId{"2"}, builder2);
  commit2->setReady();

  // Pre-condition.
  auto dir = testMount.getTreeInode("dir"_relpath);
  {
    auto contents = dir->lockContentsRead();
    auto it = contents->entries.find("foo"_pc);
    ASSERT_NE(it, contents->entries.end());
    EXPECT_FALSE(it->second.isRestricted())
        << "pre-condition: builder1 entry should not be restricted";
  }

  auto executor = testMount.getServerExecutor().get();
  auto checkoutFuture = testMount.getEdenMount()
                            ->checkout(
                                testMount.getRootInode(),
                                RootId{"2"},
                                ObjectFetchContext::getNullContext(),
                                __func__)
                            .semi()
                            .via(executor);
  testMount.drainServerExecutor();
  ASSERT_TRUE(checkoutFuture.isReady());
  auto result = std::move(checkoutFuture).get();
  EXPECT_EQ(0, result.conflicts.size());

  // Post-condition.
  {
    auto contents = dir->lockContentsRead();
    auto it = contents->entries.find("foo"_pc);
    ASSERT_NE(it, contents->entries.end());
    EXPECT_TRUE(it->second.isRestricted())
        << "inserted entry must inherit scmEntry.isRestricted()";
  }
}
