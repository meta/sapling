/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/inodes/TreeInode.h"

#include "eden/fs/model/TreeAuxData.h"

#include <fb303/ServiceData.h>
#include <fmt/format.h>
#include <folly/Exception.h>
#include <folly/Random.h>
#include <folly/coro/GtestHelpers.h>
#include <folly/executors/ManualExecutor.h>
#include <folly/test/TestUtils.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <algorithm>
#include <array>
#include <optional>

#include "eden/common/utils/CaseSensitivity.h"
#include "eden/common/utils/FaultInjector.h"
#include "eden/fs/fuse/FuseDirList.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/FileInode.h"
#include "eden/fs/inodes/Overlay.h"
#include "eden/fs/model/Tree.h"
#include "eden/fs/model/TreeEntry.h"
#include "eden/fs/nfs/NfsDirList.h"
#include "eden/fs/prjfs/Enumerator.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/testharness/FakeBackingStore.h"
#include "eden/fs/testharness/FakeTreeBuilder.h"
#include "eden/fs/testharness/TestChecks.h"
#include "eden/fs/testharness/TestMount.h"
#include "eden/fs/testharness/TestUtil.h"

using namespace facebook::eden;
using namespace std::chrono_literals;

namespace {
constexpr auto materializationTimeoutLimit = 1000ms;

std::string testIdHex{
    "faceb00c"
    "deadbeef"
    "c00010ff"
    "1badb002"
    "8badf00d"};

ObjectId testId(testIdHex);

DirEntry makeDirEntry() {
  return DirEntry{S_IFREG | 0644, 1_ino, ObjectId{}};
}

Tree::value_type makeTreeEntry(folly::StringPiece name) {
  return {
      PathComponent{name}, TreeEntry{ObjectId{}, TreeEntryType::REGULAR_FILE}};
}
} // namespace

class TreeInodeTestBase : public ::testing::TestWithParam<bool> {
 protected:
  void maybeEnableCoroutines(TestMount& mount) {
    if (GetParam()) {
      enableCoroutinesConfig(mount);
    }
  }
};

TEST_P(TreeInodeTestBase, renameOrdersUnrelatedDestinationChildLock) {
  for (const bool reverseInodeOrder : {false, true}) {
    SCOPED_TRACE(
        reverseInodeOrder ? "child < src < dest" : "dest < src < child");
    FakeTreeBuilder builder;
    TestMount mount{builder};
    maybeEnableCoroutines(mount);

    auto root = mount.getEdenMount()->getRootInode();
    std::array directories{
        root->mkdir("one"_pc, S_IFDIR | 0755, InvalidationRequired::No),
        root->mkdir("two"_pc, S_IFDIR | 0755, InvalidationRequired::No),
        root->mkdir("three"_pc, S_IFDIR | 0755, InvalidationRequired::No)};
    // Preallocation makes creation order independent of inode-number order.
    std::sort(
        directories.begin(),
        directories.end(),
        [](const auto& a, const auto& b) {
          return a->getNodeId() < b->getNodeId();
        });
    auto dest = directories[reverseInodeOrder ? 2 : 0];
    auto src = directories[1];
    auto destChild = directories[reverseInodeOrder ? 0 : 2];
    const auto childPath = destChild->getPath();
    ASSERT_TRUE(childPath.has_value());
    root->rename(
            childPath->basename(),
            dest,
            "child"_pc,
            InvalidationRequired::No,
            ObjectFetchContext::getNullContext())
        .get(1s);
    auto sourceFile =
        src->mknod("file"_pc, S_IFREG | 0644, 0, InvalidationRequired::No);

    if (reverseInodeOrder) {
      ASSERT_LT(destChild->getNodeId(), src->getNodeId());
      ASSERT_LT(src->getNodeId(), dest->getNodeId());
    } else {
      ASSERT_LT(dest->getNodeId(), src->getNodeId());
      ASSERT_LT(src->getNodeId(), destChild->getNodeId());
    }

    EXPECT_THROW_ERRNO(
        src->rename(
               "file"_pc,
               dest,
               "child"_pc,
               InvalidationRequired::No,
               ObjectFetchContext::getNullContext())
            .get(1s),
        EISDIR);

    src->rename(
           "file"_pc,
           destChild,
           "moved"_pc,
           InvalidationRequired::No,
           ObjectFetchContext::getNullContext())
        .get(1s);
    EXPECT_EQ(
        sourceFile,
        destChild
            ->getOrLoadChild("moved"_pc, ObjectFetchContext::getNullContext())
            .get(1s)
            .asFilePtr());
    EXPECT_THROW_ERRNO(
        src->getOrLoadChild("file"_pc, ObjectFetchContext::getNullContext())
            .get(1s),
        ENOENT);
  }
}

TEST(TreeInode, loadTimeEntryCountsAreSnapshotAtLoad) {
  FakeTreeBuilder builder;
  builder.setFile("dir/a.txt", "a");
  builder.setFile("dir/b.txt", "b");
  builder.setFile("dir/sub/c.txt", "c");
  TestMount mount{builder};

  auto dir = mount.getTreeInode("dir");
  EXPECT_EQ(2u, dir->loadTimeEntryCounts().files);
  EXPECT_EQ(1u, dir->loadTimeEntryCounts().dirs);

  mount.addFile("dir/d.txt", "d");
  EXPECT_EQ(2u, dir->loadTimeEntryCounts().files);
  EXPECT_EQ(1u, dir->loadTimeEntryCounts().dirs);
}

TEST(TreeInode, loadReportsEntryCountsToWalkDetector) {
  FakeTreeBuilder builder;
  for (int i = 0; i < 5; ++i) {
    builder.setFile(fmt::format("dir/f{}.txt", i), "x");
  }
  builder.setFile("dir/sub/a.txt", "a");
  TestMount mount{builder};

  mount.getTreeInode("dir");
  auto reported = mount.getWalkDetector().directorySizes();
  auto dir = std::find_if(reported.begin(), reported.end(), [](auto& entry) {
    return entry.first == RelativePath{"dir"};
  });
  ASSERT_NE(dir, reported.end());
  EXPECT_EQ(5u, dir->second.files);
  EXPECT_EQ(1u, dir->second.dirs);
}

TEST(TreeInode, findEntryDifferencesWithSameEntriesReturnsNone) {
  DirContents dir(CaseSensitivity::Sensitive);
  dir.emplace("one"_pc, makeDirEntry());
  dir.emplace("two"_pc, makeDirEntry());
  Tree tree{
      {{makeTreeEntry("one"), makeTreeEntry("two")},
       CaseSensitivity::Sensitive},
      testId};

  EXPECT_FALSE(findEntryDifferences(dir, tree));
}

TEST(TreeInode, findEntryDifferencesIgnoresUnknownAclState) {
  DirContents dir(CaseSensitivity::Sensitive);
  dir.emplace(
      "one"_pc,
      DirEntry{
          S_IFREG | 0644,
          1_ino,
          ObjectId{},
          /*isRestricted=*/false,
          std::nullopt});

  Tree::container untrackedEntries{CaseSensitivity::Sensitive};
  untrackedEntries.emplace(
      "one"_pc,
      ObjectId{},
      TreeEntryType::REGULAR_FILE,
      /*isRestricted=*/false,
      /*hasACL=*/false);
  Tree untrackedTree{std::move(untrackedEntries), testId};
  EXPECT_FALSE(findEntryDifferences(dir, untrackedTree));

  Tree::container aclEntries{CaseSensitivity::Sensitive};
  aclEntries.emplace(
      "one"_pc,
      ObjectId{},
      TreeEntryType::REGULAR_FILE,
      /*isRestricted=*/false,
      /*hasACL=*/true);
  Tree aclTree{std::move(aclEntries), testId};
  EXPECT_FALSE(findEntryDifferences(dir, aclTree));
}

TEST(TreeInode, findEntryDifferencesReportsRestrictedMismatch) {
  DirContents dir(CaseSensitivity::Sensitive);
  dir.emplace(
      "one"_pc,
      DirEntry{
          S_IFREG | 0644,
          1_ino,
          ObjectId{},
          /*isRestricted=*/false,
          /*hasACL=*/false});
  Tree::container entries{CaseSensitivity::Sensitive};
  entries.emplace(
      "one"_pc,
      ObjectId{},
      TreeEntryType::REGULAR_FILE,
      /*isRestricted=*/true,
      /*hasACL=*/true);
  Tree tree{std::move(entries), testId};

  auto differences = findEntryDifferences(dir, tree);
  EXPECT_TRUE(differences);
  EXPECT_EQ((std::vector<std::string>{"~ one"}), *differences);
}

TEST(TreeInode, findEntryDifferencesIgnoresNonRestrictedAclChange) {
  // NoAcl vs AclRoot is a non-restricted ACL-root metadata change. Checkout
  // does not reconcile it, so the diagnostic must not report it either.
  DirContents dir(CaseSensitivity::Sensitive);
  dir.emplace(
      "one"_pc,
      DirEntry{
          S_IFREG | 0644,
          1_ino,
          ObjectId{},
          /*isRestricted=*/false,
          /*hasACL=*/false});
  Tree::container entries{CaseSensitivity::Sensitive};
  entries.emplace(
      "one"_pc,
      ObjectId{},
      TreeEntryType::REGULAR_FILE,
      /*isRestricted=*/false,
      /*hasACL=*/true);
  Tree tree{std::move(entries), testId};
  EXPECT_FALSE(findEntryDifferences(dir, tree));
}

TEST(TreeInode, findEntryDifferencesIgnoresObjectIdDifference) {
  // Object ids are intentionally not compared: filtered ids for the same
  // underlying object differ byte-wise across filter changes.
  DirContents dir(CaseSensitivity::Sensitive);
  dir.emplace(
      "one"_pc,
      DirEntry{
          S_IFREG | 0644,
          1_ino,
          ObjectId("aaa"),
          /*isRestricted=*/false,
          /*hasACL=*/false});
  Tree::container entries{CaseSensitivity::Sensitive};
  entries.emplace(
      "one"_pc,
      ObjectId("bbb"),
      TreeEntryType::REGULAR_FILE,
      /*isRestricted=*/false,
      /*hasACL=*/false);
  Tree tree{std::move(entries), testId};
  EXPECT_FALSE(findEntryDifferences(dir, tree));
}

TEST(TreeInode, findEntryDifferencesReportsTypeChange) {
  DirContents dir(CaseSensitivity::Sensitive);
  dir.emplace(
      "one"_pc,
      DirEntry{
          S_IFREG | 0644,
          1_ino,
          ObjectId{},
          /*isRestricted=*/false,
          /*hasACL=*/false});
  Tree::container entries{CaseSensitivity::Sensitive};
  entries.emplace(
      "one"_pc,
      ObjectId{},
      TreeEntryType::TREE,
      /*isRestricted=*/false,
      /*hasACL=*/false);
  Tree tree{std::move(entries), testId};

  auto differences = findEntryDifferences(dir, tree);
  EXPECT_TRUE(differences);
  EXPECT_EQ((std::vector<std::string>{"~ one"}), *differences);
}

TEST(TreeInode, findEntryDifferencesReturnsAdditionsAndSubtractions) {
  DirContents dir(CaseSensitivity::Sensitive);
  dir.emplace("one"_pc, makeDirEntry());
  dir.emplace("two"_pc, makeDirEntry());
  Tree tree{
      {{makeTreeEntry("one"), makeTreeEntry("three")},
       CaseSensitivity::Sensitive},
      testId};

  auto differences = findEntryDifferences(dir, tree);
  EXPECT_TRUE(differences);
  EXPECT_EQ((std::vector<std::string>{"+ three", "- two"}), *differences);
}

TEST(TreeInode, findEntryDifferencesWithOneSubtraction) {
  DirContents dir(CaseSensitivity::Sensitive);
  dir.emplace("one"_pc, makeDirEntry());
  dir.emplace("two"_pc, makeDirEntry());
  Tree tree{{{makeTreeEntry("one")}, CaseSensitivity::Sensitive}, testId};

  auto differences = findEntryDifferences(dir, tree);
  EXPECT_TRUE(differences);
  EXPECT_EQ((std::vector<std::string>{"- two"}), *differences);
}

TEST(TreeInode, findEntryDifferencesWithOneAddition) {
  DirContents dir(CaseSensitivity::Sensitive);
  dir.emplace("one"_pc, makeDirEntry());
  dir.emplace("two"_pc, makeDirEntry());
  Tree tree{
      {{makeTreeEntry("one"), makeTreeEntry("two"), makeTreeEntry("three")},
       CaseSensitivity::Sensitive},
      testId};

  auto differences = findEntryDifferences(dir, tree);
  EXPECT_TRUE(differences);
  EXPECT_EQ((std::vector<std::string>{"+ three"}), *differences);
}

#ifndef _WIN32
TEST_P(TreeInodeTestBase, fuseReaddirReturnsSelfAndParentBeforeEntries) {
  // libfuse's documentation says returning . and .. is optional, but the FUSE
  // kernel module does not synthesize them, so not returning . and .. would be
  // a visible behavior change relative to a native filesystem.
  FakeTreeBuilder builder;
  builder.setFiles({{"file", ""}});
  TestMount mount{builder};
  maybeEnableCoroutines(mount);

  auto root = mount.getEdenMount()->getRootInode();
  auto result =
      root->fuseReaddir(
              FuseDirList{4096}, 0, ObjectFetchContext::getNullContext())
          .extract();

  ASSERT_EQ(4, result.size());
  EXPECT_EQ(".", result[0].name);
  EXPECT_EQ("..", result[1].name);
  // The remaining entries are listed in inode-number order, which depends
  // on allocation order during mount setup and is not guaranteed.
  std::vector<std::string> names{result[2].name, result[3].name};
  std::sort(names.begin(), names.end());
  EXPECT_EQ((std::vector<std::string>{".eden", "file"}), names);
}

TEST_P(TreeInodeTestBase, fuseReaddirOffsetsAreNonzero) {
  // fuseReaddir's offset parameter means "start here". 0 means start from the
  // beginning. To start after a particular entry, the offset given must be that
  // entry's offset. Therefore, no entries should have offset 0.
  FakeTreeBuilder builder;
  builder.setFiles({{"file", ""}});
  TestMount mount{builder};
  maybeEnableCoroutines(mount);

  auto root = mount.getEdenMount()->getRootInode();
  auto result =
      root->fuseReaddir(
              FuseDirList{4096}, 0, ObjectFetchContext::getNullContext())
          .extract();
  ASSERT_EQ(4, result.size());
  for (auto& entry : result) {
    EXPECT_NE(0, entry.offset);
  }
}

TEST_P(TreeInodeTestBase, fuseReaddirRespectsOffset) {
  FakeTreeBuilder builder;
  builder.setFiles({{"file", ""}});
  TestMount mount{builder};
  maybeEnableCoroutines(mount);

  auto root = mount.getEdenMount()->getRootInode();
  auto listFrom = [&](uint64_t offset) {
    return root
        ->fuseReaddir(
            FuseDirList{4096}, offset, ObjectFetchContext::getNullContext())
        .extract();
  };

  const auto all = listFrom(0);
  ASSERT_EQ(4, all.size());
  EXPECT_EQ(".", all[0].name);
  EXPECT_EQ("..", all[1].name);

  // Every entry's offset resumes the listing immediately after that entry,
  // regardless of the order the entries were assigned inode numbers in.
  for (size_t i = 0; i < all.size(); ++i) {
    const auto rest = listFrom(all[i].offset);
    ASSERT_EQ(all.size() - i - 1, rest.size());
    for (size_t j = 0; j < rest.size(); ++j) {
      EXPECT_EQ(all[i + 1 + j].name, rest[j].name);
    }
  }
}

TEST_P(TreeInodeTestBase, fuseReaddirIgnoresWildOffsets) {
  TestMount mount{FakeTreeBuilder{}};
  maybeEnableCoroutines(mount);

  auto root = mount.getEdenMount()->getRootInode();

  auto result = root->fuseReaddir(
                        FuseDirList{4096},
                        0xfaceb00c,
                        ObjectFetchContext::getNullContext())
                    .extract();
  EXPECT_EQ(0, result.size());
}

namespace {

struct ReaddirIndexCounters {
  int64_t hit;
  int64_t cached;
  int64_t droppedByGc;
};

ReaddirIndexCounters getReaddirIndexCounters(TestMount& mount) {
  mount.getServerState()->getStats()->flush();
  auto* data = facebook::fb303::ServiceData::get();
  auto get = [&](folly::StringPiece key) {
    return data->getCounterIfExists(key).value_or(0);
  };
  return {
      get("inodes.readdir_index_hit.sum"),
      get("inodes.readdir_index_cached.sum"),
      get("inodes.readdir_index_dropped_by_gc.sum")};
}

/**
 * Enough same-length names that a listing with a 4 KiB buffer takes several
 * readdir requests.
 */
std::vector<std::string> manyNames(size_t count = 300) {
  std::vector<std::string> names;
  names.reserve(count);
  for (size_t i = 0; i < count; ++i) {
    names.push_back(fmt::format("file{:03}", i));
  }
  return names;
}

/**
 * Lists `dir` from `offset` until a request returns nothing, counting how
 * often each name is returned. Returns the number of requests made.
 */
unsigned listToEnd(
    const TreeInodePtr& dir,
    off_t offset,
    std::map<std::string, unsigned>& seen) {
  unsigned requests = 0;
  for (;;) {
    auto result =
        dir->fuseReaddir(
               FuseDirList{4096}, offset, ObjectFetchContext::getNullContext())
            .extract();
    ++requests;
    if (result.empty()) {
      return requests;
    }
    for (auto& entry : result) {
      ++seen[entry.name];
    }
    offset = result.back().offset;
  }
}

} // namespace

TEST_P(TreeInodeTestBase, readdirIndexIsReusedByTheRequestsOfOneListing) {
  FakeTreeBuilder builder;
  const auto names = manyNames();
  for (const auto& name : names) {
    builder.setFile(name, "");
  }
  TestMount mount{builder};
  maybeEnableCoroutines(mount);
  auto root = mount.getEdenMount()->getRootInode();

  const auto before = getReaddirIndexCounters(mount);
  std::map<std::string, unsigned> seen;
  const auto requests = listToEnd(root, 0, seen);
  ASSERT_GT(requests, 2u);
  for (const auto& name : names) {
    EXPECT_EQ(1u, seen[name]) << name;
  }
  auto after = getReaddirIndexCounters(mount);
  EXPECT_EQ(1, after.cached - before.cached);
  // Every request after the first, including the empty one that ends the
  // listing, is served from the cached index.
  EXPECT_EQ(requests - 1, after.hit - before.hit);

  // The index is dropped when the listing ends, so the next listing caches
  // its own.
  seen.clear();
  listToEnd(root, 0, seen);
  after = getReaddirIndexCounters(mount);
  EXPECT_EQ(2, after.cached - before.cached);
}

TEST_P(TreeInodeTestBase, readdirIndexIsRebuiltAfterTheDirectoryChanges) {
  FakeTreeBuilder builder;
  const auto names = manyNames();
  for (const auto& name : names) {
    builder.setFile(name, "");
  }
  TestMount mount{builder};
  maybeEnableCoroutines(mount);
  auto root = mount.getEdenMount()->getRootInode();

  const auto before = getReaddirIndexCounters(mount);
  auto firstPage =
      root->fuseReaddir(
              FuseDirList{4096}, 0, ObjectFetchContext::getNullContext())
          .extract();
  ASSERT_FALSE(firstPage.empty());
  EXPECT_EQ(1, getReaddirIndexCounters(mount).cached - before.cached);

  // Creating an entry mutates the map, so the rest of the listing cannot
  // trust the cached index and builds a new one.
  root->symlink("zzz"_pc, "target", InvalidationRequired::No);

  std::map<std::string, unsigned> seen;
  for (auto& entry : firstPage) {
    ++seen[entry.name];
  }
  listToEnd(root, firstPage.back().offset, seen);
  for (const auto& name : names) {
    EXPECT_EQ(1u, seen[name]) << name;
  }
  const auto after = getReaddirIndexCounters(mount);
  EXPECT_EQ(2, after.cached - before.cached);
}

TEST_P(TreeInodeTestBase, readdirIndexIsRebuiltAfterRenameOverAnEntry) {
  // Renaming over an existing entry replaces that entry's DirEntry rather
  // than inserting or erasing one; it still has to count as a mutation of
  // the destination directory for the cached index to be rebuilt.
  FakeTreeBuilder builder;
  const auto names = manyNames();
  for (const auto& name : names) {
    builder.setFile("dir/" + name, "");
  }
  builder.setFile("other/src", "");
  TestMount mount{builder};
  maybeEnableCoroutines(mount);
  auto dir = mount.getTreeInode("dir"_relpath);
  auto other = mount.getTreeInode("other"_relpath);

  const auto before = getReaddirIndexCounters(mount);
  auto firstPage =
      dir->fuseReaddir(
             FuseDirList{4096}, 0, ObjectFetchContext::getNullContext())
          .extract();
  ASSERT_FALSE(firstPage.empty());
  EXPECT_EQ(1, getReaddirIndexCounters(mount).cached - before.cached);

  // Entries were given inode numbers in name order when `dir` was loaded, so
  // the last name is on a later page.
  const auto& replaced = names.back();
  auto fut = other
                 ->rename(
                     "src"_pc,
                     dir,
                     PathComponentPiece{replaced},
                     InvalidationRequired::No,
                     ObjectFetchContext::getNullContext())
                 .semi()
                 .via(mount.getServerExecutor().get());
  mount.drainServerExecutor();
  std::move(fut).get(0ms);

  std::map<std::string, unsigned> seen;
  for (auto& entry : firstPage) {
    ++seen[entry.name];
  }
  listToEnd(dir, firstPage.back().offset, seen);
  for (const auto& name : names) {
    if (name == replaced) {
      continue;
    }
    EXPECT_EQ(1u, seen[name]) << name;
  }
  EXPECT_EQ(2, getReaddirIndexCounters(mount).cached - before.cached);
}

TEST_P(TreeInodeTestBase, readdirIndexIsRebuiltAfterConflictRepair) {
  FakeTreeBuilder builder;
  const auto names = manyNames();
  for (const auto& name : names) {
    builder.setFile("dir/" + name, "");
  }
  builder.setFile("original", "original");
  TestMount mount{
      builder,
      true,
      true,
      kPathMapDefaultCaseSensitive,
      nullptr,
      [](EdenConfig& config) {
        config.overlayOnlineFsck.setValue(true, ConfigSourceType::CommandLine);
      }};
  maybeEnableCoroutines(mount);
  auto original = mount.getFileInode("original");
  auto dir = mount.getTreeInode("dir");
  const auto& repaired = names.back();
  {
    auto contents = dir->getContentsUnchecked().wlock();
    auto& entry = contents->entries.find(PathComponentPiece{repaired})->second;
    entry = DirEntry{
        entry.getInitialMode(),
        original->getNodeId(),
        entry.getObjectId(),
        entry.aclRootState()};
  }
  const auto before = getReaddirIndexCounters(mount);
  auto firstPage =
      dir->fuseReaddir(
             FuseDirList{4096}, 0, ObjectFetchContext::getNullContext())
          .extract();
  ASSERT_FALSE(firstPage.empty());
  EXPECT_EQ(1, getReaddirIndexCounters(mount).cached - before.cached);

  auto child = mount.getFileInode("dir/" + repaired);
  EXPECT_NE(original->getNodeId(), child->getNodeId());
  std::map<std::string, unsigned> seen;
  listToEnd(dir, firstPage.back().offset, seen);
  EXPECT_EQ(1u, seen[repaired]);
  EXPECT_EQ(2, getReaddirIndexCounters(mount).cached - before.cached);
}

TEST_P(TreeInodeTestBase, readdirIndexIsDroppedByInodeGc) {
  FakeTreeBuilder builder;
  const auto names = manyNames();
  for (const auto& name : names) {
    builder.setFile(name, "");
  }
  TestMount mount{builder};
  maybeEnableCoroutines(mount);
  auto root = mount.getEdenMount()->getRootInode();

  const auto before = getReaddirIndexCounters(mount);
  auto firstPage =
      root->fuseReaddir(
              FuseDirList{4096}, 0, ObjectFetchContext::getNullContext())
          .extract();
  ASSERT_FALSE(firstPage.empty());
  EXPECT_EQ(1, getReaddirIndexCounters(mount).cached - before.cached);

  // The listing is abandoned here; a GC pass over the directory frees the
  // index it left behind.
  root->unloadChildrenNow();
  auto after = getReaddirIndexCounters(mount);
  EXPECT_EQ(1, after.droppedByGc - before.droppedByGc);

  // Resuming the listing works, it just has to build the index again.
  std::map<std::string, unsigned> seen;
  for (auto& entry : firstPage) {
    ++seen[entry.name];
  }
  listToEnd(root, firstPage.back().offset, seen);
  for (const auto& name : names) {
    EXPECT_EQ(1u, seen[name]) << name;
  }
  after = getReaddirIndexCounters(mount);
  EXPECT_EQ(2, after.cached - before.cached);
}

TEST_P(TreeInodeTestBase, readdirIndexCacheCanBeDisabled) {
  FakeTreeBuilder builder;
  const auto names = manyNames();
  for (const auto& name : names) {
    builder.setFile(name, "");
  }
  TestMount mount{builder};
  maybeEnableCoroutines(mount);
  mount.updateEdenConfig({
      {"experimental:readdir-index-cache", "false"},
  });
  auto root = mount.getEdenMount()->getRootInode();

  const auto before = getReaddirIndexCounters(mount);
  std::map<std::string, unsigned> seen;
  ASSERT_GT(listToEnd(root, 0, seen), 2u);
  for (const auto& name : names) {
    EXPECT_EQ(1u, seen[name]) << name;
  }
  const auto after = getReaddirIndexCounters(mount);
  EXPECT_EQ(0, after.cached - before.cached);
  EXPECT_EQ(0, after.hit - before.hit);
}

TEST_P(TreeInodeTestBase, nfsReaddirEofIsCorrect) {
  FakeTreeBuilder builder;
  builder.setFiles({{"foo", ""}, {"bar", ""}, {"baz", ""}});
  TestMount mount{builder};
  maybeEnableCoroutines(mount);

  auto root = mount.getEdenMount()->getRootInode();

  uint32_t kMaxCount = 4096;

  // Assert correct EOF behavior across a range of buffer sizes, including when
  // we have only enough buffer space to return all but the final directory
  // entry.
  std::unordered_set<size_t> listingSizesReturned{};
  for (uint32_t bufSize = kNfsDirListInitialOverhead; bufSize <= kMaxCount;
       ++bufSize) {
    auto [list, isEof] = root->nfsReaddir(
        NfsDirList{bufSize, nfsv3Procs::readdir},
        0,
        ObjectFetchContext::getNullContext());

    auto listingSize = list.extractList<entry3>().list.size();
    listingSizesReturned.insert(listingSize);

    // We should return EOF iff we're returning the entire directory listing,
    // which has six entries: ".", "..", ".eden", "foo", "bar", and "baz".
    ASSERT_EQ(listingSize == 6, isEof);
  }

  // To prevent a regression of S298201 we especially want to cover the case
  // where there's exactly one more directory entry we couldn't fit into the
  // response.  If we know that we've returned at least one response without an
  // entire directory listing (buffer too small), and at least one with an
  // entire listing (buffer big enough) while iterating over buffer sizes above,
  // then we know we've covered that case.
  ASSERT_NE(listingSizesReturned.contains(5), 0);
  ASSERT_NE(listingSizesReturned.contains(6), 0);
}

TEST_P(TreeInodeTestBase, nfsReaddirDropsTheIndexAtEof) {
  // NFS clients stop at eof rather than sending the empty request that ends
  // a FUSE listing, so eof is where the cached index has to go.
  FakeTreeBuilder builder;
  const auto names = manyNames();
  for (const auto& name : names) {
    builder.setFile(name, "");
  }
  TestMount mount{builder};
  maybeEnableCoroutines(mount);
  auto root = mount.getEdenMount()->getRootInode();

  auto listAll = [&] {
    std::map<std::string, unsigned> seen;
    uint64_t cookie = 0;
    for (;;) {
      auto [list, isEof] = root->nfsReaddir(
          NfsDirList{4096, nfsv3Procs::readdir},
          cookie,
          ObjectFetchContext::getNullContext());
      const auto entries = list.extractList<entry3>().list;
      for (const auto& entry : entries) {
        ++seen[entry.name];
      }
      if (isEof) {
        return seen;
      }
      if (entries.empty()) {
        ADD_FAILURE() << "empty page before eof";
        return seen;
      }
      cookie = entries.back().cookie;
    }
  };

  const auto before = getReaddirIndexCounters(mount);
  auto seen = listAll();
  for (const auto& name : names) {
    EXPECT_EQ(1u, seen[name]) << name;
  }
  EXPECT_EQ(1, getReaddirIndexCounters(mount).cached - before.cached);

  // Had the index lingered past eof, this listing would be served from it
  // instead of caching one of its own.
  listAll();
  EXPECT_EQ(2, getReaddirIndexCounters(mount).cached - before.cached);
}

namespace {

// 500 is big enough for ~9 entries
constexpr size_t kDirListBufferSize = 500;
constexpr size_t kDirListNameSize = 25;
constexpr unsigned kModificationCountPerIteration = 4;

void runConcurrentModificationAndReaddirIteration(
    const std::vector<std::string>& names,
    bool useCoroutines) {
  std::unordered_set<std::string> modified;

  struct Collision : std::exception {};

  auto randomName = [&]() -> PathComponent {
    // + 1 to avoid collisions with existing names.
    std::array<char, kDirListNameSize + 1> name;
    for (char& c : name) {
      c = folly::Random::rand32('a', 'z' + 1);
    }
    return PathComponent{std::string_view{name.data(), name.size()}};
  };

  // Selects a random name from names and adds it to modified, throwing
  // Collision if it's already been used.
  auto pickName = [&]() -> PathComponentPiece {
    const auto& name = names[folly::Random::rand32(names.size())];
    if (modified.contains(name)) {
      throw Collision{};
    }
    modified.insert(name);
    // Returning PathComponentPiece is safe because name is a reference into
    // names.
    return PathComponentPiece{name};
  };

  FakeTreeBuilder builder;
  for (const auto& name : names) {
    builder.setFile(name, name);
  }
  TestMount mount{builder};
  if (useCoroutines) {
    enableCoroutinesConfig(mount);
  }
  auto root = mount.getEdenMount()->getRootInode();

  FileOffset lastOffset = 0;

  std::unordered_map<std::string, unsigned> seen;

  for (;;) {
    auto result = root->fuseReaddir(
                          FuseDirList{kDirListBufferSize},
                          lastOffset,
                          ObjectFetchContext::getNullContext())
                      .extract();
    if (result.empty()) {
      break;
    }
    lastOffset = result.back().offset;
    for (auto& entry : result) {
      ++seen[entry.name];
    }

    for (unsigned j = 0; j < kModificationCountPerIteration; ++j) {
      try {
        switch (folly::Random::rand32(3)) {
          case 0: // create
            root->symlink(
                randomName(), "symlink-target", InvalidationRequired::No);
            break;
          case 1: { // unlink
            auto fut = root->unlink(
                               pickName(),
                               InvalidationRequired::No,
                               ObjectFetchContext::getNullContext())
                           .semi()
                           .via(mount.getServerExecutor().get());
            mount.drainServerExecutor();
            std::move(fut).get(0ms);
            break;
          }
          case 2: { // rename
            auto fut = root->rename(
                               pickName(),
                               root,
                               pickName(),
                               InvalidationRequired::No,
                               ObjectFetchContext::getNullContext())
                           .semi()
                           .via(mount.getServerExecutor().get());
            mount.drainServerExecutor();
            std::move(fut).get(0ms);
            break;
          }
        }
      } catch (const Collision&) {
        // Just skip, no big deal.
      }
    }
  }

  // Verify all unmodified files were read.
  for (auto& name : names) {
    // If modified, it is not guaranteed to be returned by fuseReaddir.
    if (modified.contains(name)) {
      continue;
    }

    EXPECT_EQ(1, seen[name])
        << "unmodified entries should be returned by fuseReaddir exactly once, but "
        << name << " wasn't";
  }
}
} // namespace

TEST_P(TreeInodeTestBase, fuzzConcurrentModificationAndReaddir) {
  std::vector<std::string> names;
  for (char c = 'a'; c <= 'z'; ++c) {
    names.emplace_back(kDirListNameSize, c);
  }

  auto minimumTime = 500ms;
  unsigned minimumIterations = 5;

  auto end = std::chrono::steady_clock::now() + minimumTime;
  unsigned iterations = 0;
  while (std::chrono::steady_clock::now() < end ||
         iterations < minimumIterations) {
    runConcurrentModificationAndReaddirIteration(names, GetParam());
    ++iterations;
  }
  std::cout << "Ran " << iterations << " iterations" << std::endl;
}
#endif

TEST_P(TreeInodeTestBase, create) {
  FakeTreeBuilder builder;
  builder.setFile("somedir/foo.txt", "test\n");
  TestMount mount{builder};
  maybeEnableCoroutines(mount);

  // Test creating a new file
  auto somedir = mount.getTreeInode("somedir"_relpath);
  auto resultInode = somedir->mknod(
      "newfile.txt"_pc, S_IFREG | 0740, 0, InvalidationRequired::No);

  EXPECT_EQ(mount.getFileInode("somedir/newfile.txt"_relpath), resultInode);

#ifndef _WIN32 // getPermissions are not a part of Inode on Windows
  EXPECT_FILE_INODE(resultInode, "", 0740);
#endif
}

TEST_P(TreeInodeTestBase, createExists) {
  FakeTreeBuilder builder;
  builder.setFile("somedir/foo.txt", "test\n");
  TestMount mount{builder};
  maybeEnableCoroutines(mount);

  // Test creating a new file
  auto somedir = mount.getTreeInode("somedir"_relpath);

  EXPECT_THROW_ERRNO(
      somedir->mknod("foo.txt"_pc, S_IFREG | 0600, 0, InvalidationRequired::No),
      EEXIST);
#ifndef _WIN32 // getPermissions are not a part of Inode on Windows
  EXPECT_FILE_INODE(
      mount.getFileInode("somedir/foo.txt"_relpath), "test\n", 0644);
#endif
}

#ifndef _WIN32

TEST_P(TreeInodeTestBase, createOverlayWriteError) {
  FakeTreeBuilder builder;
  builder.setFile("somedir/foo.txt", "test\n");
  TestMount mount{builder};
  maybeEnableCoroutines(mount);
  mount.getServerState()->getFaultInjector().injectError(
      "createInodeSaveOverlay",
      "newfile.txt",
      folly::makeSystemErrorExplicit(ENOSPC, "too many cat videos"));

  auto somedir = mount.getTreeInode("somedir"_relpath);

  EXPECT_THROW_ERRNO(
      somedir->mknod(
          "newfile.txt"_pc, S_IFREG | 0600, 0, InvalidationRequired::No),
      ENOSPC);
}

#endif

TEST_P(TreeInodeTestBase, removeRecursively) {
  FakeTreeBuilder builder;
  builder.setFile("somedir/foo.txt", "foo\n");
  builder.setFile("somedir/bar.txt", "bar\n");
  builder.setFile("somedir/baz.txt", "baz\n");
  builder.setFile("somedir/otherdir/foo.txt", "test\n");
  TestMount mount{builder};
  maybeEnableCoroutines(mount);

  auto root = mount.getEdenMount()->getRootInode();
  root->removeRecursively(
          "somedir"_pc,
          InvalidationRequired::No,
          ObjectFetchContext::getNullContext())
      .get(0ms);

  EXPECT_THROW_ERRNO(mount.getTreeInode("somedir"_relpath), ENOENT);
}

TEST_P(TreeInodeTestBase, removeRecursivelyNotReady) {
  FakeTreeBuilder builder;
  builder.setFile("somedir/foo.txt", "foo\n");
  builder.setFile("somedir/bar.txt", "bar\n");
  builder.setFile("somedir/baz.txt", "baz\n");
  builder.setFile("somedir/otherdir/foo.txt", "test\n");
  TestMount mount;
  mount.initialize(builder, false);
  maybeEnableCoroutines(mount);

  auto root = mount.getEdenMount()->getRootInode();
  auto fut = root->getOrLoadChildTree(
                     "somedir"_pc, ObjectFetchContext::getNullContext())
                 .thenValue([root](TreeInodePtr&&) {
                   return root->removeRecursively(
                       "somedir"_pc,
                       InvalidationRequired::No,
                       ObjectFetchContext::getNullContext());
                 });
  EXPECT_FALSE(fut.isReady());

  builder.setAllReady();
  // The tree fetch resumes on the server executor once the backing store
  // answers.
  mount.drainServerExecutor();
  std::move(fut).get(0ms);

  EXPECT_THROW_ERRNO(mount.getTreeInode("somedir"_relpath), ENOENT);
}

#ifndef _WIN32

TEST_P(TreeInodeTestBase, setattr) {
  FakeTreeBuilder builder;
  builder.setFile("somedir/foo.txt", "test\n");
  TestMount mount{builder};
  maybeEnableCoroutines(mount);
  auto somedir = mount.getTreeInode("somedir"_relpath);

  EXPECT_FALSE(somedir->isMaterialized());
  DesiredMetadata emptyMetadata{};
  somedir->setattr(emptyMetadata, ObjectFetchContext::getNullContext());
  EXPECT_FALSE(somedir->isMaterialized());

  auto oldauxData = somedir->getMetadata();
  DesiredMetadata sameMetadata{
      std::nullopt,
      oldauxData.mode,
      oldauxData.uid,
      oldauxData.gid,
      oldauxData.timestamps.atime.toTimespec(),
      oldauxData.timestamps.mtime.toTimespec()};
  somedir->setattr(sameMetadata, ObjectFetchContext::getNullContext());
  EXPECT_FALSE(somedir->isMaterialized());

  DesiredMetadata newMetadata{
      std::nullopt,
      oldauxData.mode,
      oldauxData.uid + 1,
      oldauxData.gid + 1,
      oldauxData.timestamps.atime.toTimespec(),
      oldauxData.timestamps.mtime.toTimespec()};
  somedir->setattr(newMetadata, ObjectFetchContext::getNullContext());
  EXPECT_TRUE(somedir->isMaterialized());
}

TEST_P(TreeInodeTestBase, addNewMaterializationsToInodeTraceBus) {
  folly::UnboundedQueue<InodeTraceEvent, true, true, false> queue;
  FakeTreeBuilder builder;
  builder.setFiles(
      {{"somedir/sub/foo.txt", "test\n"}, {"dir2/bar.txt", "test 2\n"}});
  TestMount mount{builder};
  maybeEnableCoroutines(mount);
  auto& trace_bus = mount.getEdenMount()->getInodeTraceBus();

  auto somedir = mount.getTreeInode("somedir"_relpath);
  auto sub = mount.getTreeInode("somedir/sub"_relpath);
  auto dir2 = mount.getTreeInode("dir2"_relpath);

  // Detect inode materialization events and add events to synchronized queue
  auto handle = trace_bus.subscribeFunction(
      fmt::format(
          "treeInodeTest-{}", mount.getEdenMount()->getPath().basename()),
      [&](const InodeTraceEvent& event) {
        if (event.eventType == InodeEventType::MATERIALIZE) {
          queue.enqueue(event);
        }
      });

  // Wait for any initial materialization events to complete
  while (queue.try_dequeue_for(materializationTimeoutLimit).has_value()) {
  };

  // Test removing an inode (in this case a tree inode which also materializes
  // that tree inode before removing it)
  somedir->getOrLoadChildTree("sub"_pc, ObjectFetchContext::getNullContext())
      .thenValue([somedir](TreeInodePtr&&) {
        return somedir->removeRecursively(
            "sub"_pc,
            InvalidationRequired::No,
            ObjectFetchContext::getNullContext());
      })
      .get(0ms);
  EXPECT_TRUE(isInodeMaterializedInQueue(
      queue, InodeEventProgress::START, sub->getNodeId()));
  EXPECT_TRUE(isInodeMaterializedInQueue(
      queue, InodeEventProgress::START, somedir->getNodeId()));
  EXPECT_TRUE(isInodeMaterializedInQueue(
      queue, InodeEventProgress::END, somedir->getNodeId()));
  EXPECT_TRUE(isInodeMaterializedInQueue(
      queue, InodeEventProgress::END, sub->getNodeId()));

  // Test creating a directory
  auto newdir =
      somedir->mkdir("newdir"_pc, S_IFREG | 0740, InvalidationRequired::No);
  EXPECT_TRUE(isInodeMaterializedInQueue(
      queue, InodeEventProgress::START, newdir->getNodeId()));
  EXPECT_TRUE(isInodeMaterializedInQueue(
      queue, InodeEventProgress::END, newdir->getNodeId()));

  // Test creating a file (on an already materialized parent)
  auto newfile = newdir->mknod(
      "newfile.txt"_pc, S_IFREG | 0740, 0, InvalidationRequired::No);
  EXPECT_TRUE(isInodeMaterializedInQueue(
      queue, InodeEventProgress::START, newfile->getNodeId()));
  EXPECT_TRUE(isInodeMaterializedInQueue(
      queue, InodeEventProgress::END, newfile->getNodeId()));

  // Test creating a file (on an unmaterialized parent)
  auto newfile2 = dir2->mknod(
      "newfile2.txt"_pc, S_IFREG | 0740, 0, InvalidationRequired::No);
  EXPECT_TRUE(isInodeMaterializedInQueue(
      queue, InodeEventProgress::START, dir2->getNodeId()));
  EXPECT_TRUE(isInodeMaterializedInQueue(
      queue, InodeEventProgress::END, dir2->getNodeId()));
  EXPECT_TRUE(isInodeMaterializedInQueue(
      queue, InodeEventProgress::START, newfile2->getNodeId()));
  EXPECT_TRUE(isInodeMaterializedInQueue(
      queue, InodeEventProgress::END, newfile2->getNodeId()));

  // Test creating a symlink
  auto symlink = newdir->symlink(
      "symlink.txt"_pc, "newfile.txt", InvalidationRequired::No);
  EXPECT_TRUE(isInodeMaterializedInQueue(
      queue, InodeEventProgress::START, symlink->getNodeId()));
  EXPECT_TRUE(isInodeMaterializedInQueue(
      queue, InodeEventProgress::END, symlink->getNodeId()));

  // Ensure we do not count any other materializations a second time
  EXPECT_FALSE(queue.try_dequeue_for(materializationTimeoutLimit).has_value());
}

// Awaits getChildren on the mount's server executor, the same executor EdenFS
// schedules this work on. blockingWait's executor overload both schedules the
// task and drives the executor until it completes, so the now_task is awaited
// to completion in place rather than detached onto a future.
std::vector<std::pair<PathComponent, folly::Try<VirtualInode>>>
getChildrenOnServerExecutor(
    TestMount& mount,
    const TreeInodePtr& dir,
    bool loadInodes) {
  return folly::coro::blockingWait(
      dir->getChildren(ObjectFetchContext::getNullContext(), loadInodes),
      mount.getServerExecutor().get());
}

TEST_P(TreeInodeTestBase, getOrFindChildrenSimple) {
  FakeTreeBuilder builder;
  builder.setFile("somedir/foo.txt", "test\n");
  TestMount mount{builder};
  maybeEnableCoroutines(mount);
  auto somedir = mount.getTreeInode("somedir"_relpath);

  auto result = getChildrenOnServerExecutor(mount, somedir, false);
  EXPECT_EQ(1, result.size());
  EXPECT_THAT(result, testing::Contains(testing::Key("foo.txt"_pc)));
  for (auto& [_name, child] : result) {
    ASSERT_TRUE(child.hasValue());
  }
}

TEST_P(TreeInodeTestBase, getOrFindChildrenLoadInodes) {
  FakeTreeBuilder builder;
  builder.setFile("somedir/bar.txt", "test\n");
  builder.setFile("somedir/foo.txt", "test\n");
  TestMount mount{builder};
  maybeEnableCoroutines(mount);
  auto somedir = mount.getTreeInode("somedir"_relpath);

  somedir->unloadChildrenNow();
  auto result = getChildrenOnServerExecutor(mount, somedir, true);

  EXPECT_EQ(2, result.size());
  EXPECT_THAT(result, testing::Contains(testing::Key("bar.txt"_pc)));
  EXPECT_THAT(result, testing::Contains(testing::Key("foo.txt"_pc)));
  for (auto& [_name, child] : result) {
    ASSERT_TRUE(child.hasValue());
  }
}

TEST_P(TreeInodeTestBase, getOrFindChildrenMaterializedLoadedChild) {
  FakeTreeBuilder builder;
  builder.setFile("somedir/foo.txt", "test\n");
  TestMount mount{builder};
  maybeEnableCoroutines(mount);
  auto somedir = mount.getTreeInode("somedir"_relpath);
  somedir->mknod("newfile.txt"_pc, S_IFREG | 0740, 0, InvalidationRequired::No);
  EXPECT_TRUE(somedir->isMaterialized());

  auto result = getChildrenOnServerExecutor(mount, somedir, false);

  EXPECT_EQ(2, result.size());
  EXPECT_THAT(result, testing::Contains(testing::Key("foo.txt"_pc)));
  EXPECT_THAT(result, testing::Contains(testing::Key("newfile.txt"_pc)));
  for (auto& [_name, child] : result) {
    ASSERT_TRUE(child.hasValue());
  }
}

TEST_P(TreeInodeTestBase, getOrFindChildrenMaterializedUnloadedChild) {
  FakeTreeBuilder builder;
  builder.setFile("somedir/foo.txt", "test\n");
  builder.setFile("somedir/zoo.txt", "test\n");
  TestMount mount{builder};
  maybeEnableCoroutines(mount);
  auto somedir = mount.getTreeInode("somedir"_relpath);
  {
    somedir->mknod(
        "newfile.txt"_pc, S_IFREG | 0740, 0, InvalidationRequired::No);
  }

  somedir->unloadChildrenNow();
  auto result = getChildrenOnServerExecutor(mount, somedir, false);

  EXPECT_EQ(3, result.size());
  EXPECT_THAT(result, testing::Contains(testing::Key("foo.txt"_pc)));
  EXPECT_THAT(result, testing::Contains(testing::Key("newfile.txt"_pc)));
  EXPECT_THAT(result, testing::Contains(testing::Key("zoo.txt"_pc)));
  for (auto& [_name, child] : result) {
    ASSERT_TRUE(child.hasValue());
  }
}

TEST_P(TreeInodeTestBase, getOrFindChildrenRemovedChild) {
  FakeTreeBuilder builder;
  builder.setFile("somedir/foo.txt", "test\n");
  TestMount mount{builder};
  maybeEnableCoroutines(mount);
  auto somedir = mount.getTreeInode("somedir"_relpath);
  somedir->mknod("newfile.txt"_pc, S_IFREG | 0740, 0, InvalidationRequired::No);

  auto fut = somedir
                 ->unlink(
                     "foo.txt"_pc,
                     InvalidationRequired::No,
                     ObjectFetchContext::getNullContext())
                 .semi()
                 .via(mount.getServerExecutor().get());
  mount.drainServerExecutor();
  std::move(fut).get(0ms);

  auto result = getChildrenOnServerExecutor(mount, somedir, false);

  EXPECT_EQ(1, result.size());
  EXPECT_THAT(
      result, testing::Not(testing::Contains(testing::Key("foo.txt"_pc))));
  EXPECT_THAT(result, testing::Contains(testing::Key("newfile.txt"_pc)));
  for (auto& [_name, child] : result) {
    ASSERT_TRUE(child.hasValue());
  }
}

TEST_P(TreeInodeTestBase, buildDirFromTree) {
  // Set up a mount with a known directory structure
  FakeTreeBuilder builder;
  builder.setFiles({
      {"dir/a.txt", "content_a"},
      {"dir/b.txt", "content_b"},
      {"dir/c.txt", "content_c"},
  });
  TestMount mount{builder};
  maybeEnableCoroutines(mount);

  // Load the directory inode — this exercises buildDirFromTree internally
  auto dir = mount.getTreeInode("dir"_relpath);
  auto contents = dir->lockContentsRead();

  // Verify all entries are present
  EXPECT_EQ(3, contents->entries.size());
  EXPECT_NE(contents->entries.end(), contents->entries.find("a.txt"_pc));
  EXPECT_NE(contents->entries.end(), contents->entries.find("b.txt"_pc));
  EXPECT_NE(contents->entries.end(), contents->entries.find("c.txt"_pc));

  // Verify entries are not materialized (they come from source control)
  EXPECT_FALSE(contents->entries.at("a.txt"_pc).isMaterialized());
  EXPECT_FALSE(contents->entries.at("b.txt"_pc).isMaterialized());
  EXPECT_FALSE(contents->entries.at("c.txt"_pc).isMaterialized());

  // Verify each entry has a unique inode number
  auto inoA = contents->entries.at("a.txt"_pc).getInodeNumber();
  auto inoB = contents->entries.at("b.txt"_pc).getInodeNumber();
  auto inoC = contents->entries.at("c.txt"_pc).getInodeNumber();
  EXPECT_NE(inoA, inoB);
  EXPECT_NE(inoA, inoC);
  EXPECT_NE(inoB, inoC);

  // Verify mode bits are correct for regular files
  EXPECT_EQ(S_IFREG | 0644, contents->entries.at("a.txt"_pc).getInitialMode());
}

TEST_P(TreeInodeTestBase, buildDirFromTreePropagatesIsRestricted) {
  FakeTreeBuilder builder;
  builder.setFile("restricted_dir/file.txt", "content");
  builder.setDirIsRestricted("restricted_dir");
  builder.setFile("normal_dir/file.txt", "content");
  TestMount testMount{builder};
  maybeEnableCoroutines(testMount);

  auto rootInode = testMount.getEdenMount()->getRootInode();
  auto contents = rootInode->lockContentsRead();

  auto restrictedIter = contents->entries.find("restricted_dir"_pc);
  ASSERT_NE(restrictedIter, contents->entries.end());
  EXPECT_TRUE(restrictedIter->second.isDirectory());
  EXPECT_TRUE(restrictedIter->second.isRestricted());

  auto normalIter = contents->entries.find("normal_dir"_pc);
  ASSERT_NE(normalIter, contents->entries.end());
  EXPECT_TRUE(normalIter->second.isDirectory());
  EXPECT_FALSE(normalIter->second.isRestricted());
}

TEST(DirEntry, isRestrictedBitField) {
  DirEntry restrictedEntry(
      S_IFDIR | 0755,
      43_ino,
      ObjectId("def"),
      /*isRestricted=*/true);
  EXPECT_TRUE(restrictedEntry.isRestricted());

  DirEntry defaultEntry(S_IFDIR | 0755, 44_ino, ObjectId("ghi"));
  EXPECT_FALSE(defaultEntry.isRestricted());

  DirEntry materialized(S_IFDIR | 0755, 45_ino);
  EXPECT_FALSE(materialized.isRestricted());

  DirEntry mutableEntry(S_IFDIR | 0755, 46_ino, ObjectId("jkl"));
  EXPECT_FALSE(mutableEntry.isRestricted());
  mutableEntry.setRestricted(true);
  EXPECT_TRUE(mutableEntry.isRestricted());
  mutableEntry.setRestricted(false);
  EXPECT_FALSE(mutableEntry.isRestricted());
}

TEST(DirEntry, aclRootStateCompatibilityHelpers) {
  auto expectAclRootState = [](InodeNumber ino,
                               const char* id,
                               bool isRestricted,
                               std::optional<bool> hasACL,
                               AclRootState aclRootState,
                               std::optional<bool> expectedHasACL) {
    DirEntry entry(S_IFDIR | 0755, ino, ObjectId(id), isRestricted, hasACL);
    EXPECT_EQ(entry.aclRootState(), aclRootState);
    EXPECT_EQ(entry.hasACL(), expectedHasACL);
    EXPECT_EQ(entry.isRestricted(), isRestricted);
  };

  expectAclRootState(
      47_ino, "mno", false, std::nullopt, AclRootState::Unknown, std::nullopt);
  expectAclRootState(48_ino, "pqr", false, false, AclRootState::NoAcl, false);
  expectAclRootState(49_ino, "stu", false, true, AclRootState::AclRoot, true);
  expectAclRootState(
      50_ino, "vwx", true, true, AclRootState::RestrictedAclRoot, true);

  DirEntry restricted(
      S_IFDIR | 0755,
      50_ino,
      ObjectId("vwx"),
      /*isRestricted=*/true,
      /*hasACL=*/true);
  EXPECT_EQ(restricted.aclRootState(), AclRootState::RestrictedAclRoot);
  EXPECT_EQ(restricted.hasACL(), std::optional<bool>{true});
  EXPECT_TRUE(restricted.isRestricted());

  restricted.setRestricted(false);
  EXPECT_EQ(restricted.aclRootState(), AclRootState::AclRoot);
  EXPECT_EQ(restricted.hasACL(), std::optional<bool>{true});
  EXPECT_FALSE(restricted.isRestricted());

  DirEntry staleRestricted(
      S_IFDIR | 0755,
      51_ino,
      ObjectId("yz0"),
      /*isRestricted=*/true,
      /*hasACL=*/true);
  staleRestricted.setHasACL(false);
  EXPECT_EQ(staleRestricted.aclRootState(), AclRootState::RestrictedAclRoot);
  EXPECT_EQ(staleRestricted.hasACL(), std::optional<bool>{true});
  EXPECT_TRUE(staleRestricted.isRestricted());

  staleRestricted.setAclRootState(
      makeAclRootState(/*isRestricted=*/false, /*hasACL=*/false));
  EXPECT_EQ(staleRestricted.aclRootState(), AclRootState::NoAcl);
  EXPECT_EQ(staleRestricted.hasACL(), std::optional<bool>{false});
  EXPECT_FALSE(staleRestricted.isRestricted());
}

TEST_P(TreeInodeTestBase, childMaterializedSkipsOverlayWrite) {
  FakeTreeBuilder builder;
  builder.setFiles({
      {"dir/a.txt", "content_a"},
      {"dir/b.txt", "content_b"},
  });
  TestMount mount{builder};
  maybeEnableCoroutines(mount);

  // Materialize "dir" by writing a file — this writes the overlay
  mount.overwriteFile("dir/a.txt", "modified\n");

  auto dir = mount.getTreeInode("dir"_relpath);
  auto dirIno = dir->getNodeId();
  auto* overlay = mount.getEdenMount()->getOverlay();

  // dir is now materialized and overlay has its data.
  // "a.txt" should be materialized, "b.txt" should not.
  ASSERT_TRUE(dir->isMaterialized());
  {
    auto overlayDir = overlay->loadOverlayDir(dirIno);
    EXPECT_TRUE(overlayDir.at("a.txt"_pc).isMaterialized());
    EXPECT_FALSE(overlayDir.at("b.txt"_pc).isMaterialized());
  }

  // Call childMaterialized for "b.txt" with writeOverlay=false
  auto renameLock = mount.getEdenMount()->acquireRenameLock();
  dir->childMaterialized(renameLock, "b.txt"_pc, /*writeOverlay=*/false);

  // In-memory: b.txt is now materialized
  {
    auto contents = dir->lockContentsRead();
    EXPECT_TRUE(contents->entries.at("b.txt"_pc).isMaterialized());
  }

  // On-disk overlay was NOT updated — b.txt should still show as
  // non-materialized in the persisted overlay data
  {
    auto overlayDir = overlay->loadOverlayDir(dirIno);
    EXPECT_FALSE(overlayDir.at("b.txt"_pc).isMaterialized());
  }
}

TEST_P(TreeInodeTestBase, childMaterializedWritesOverlayByDefault) {
  FakeTreeBuilder builder;
  builder.setFiles({
      {"dir/a.txt", "content_a"},
      {"dir/b.txt", "content_b"},
  });
  TestMount mount{builder};
  maybeEnableCoroutines(mount);

  // Materialize "dir" by writing a file
  mount.overwriteFile("dir/a.txt", "modified\n");

  auto dir = mount.getTreeInode("dir"_relpath);
  auto dirIno = dir->getNodeId();
  auto* overlay = mount.getEdenMount()->getOverlay();

  // Call childMaterialized for "b.txt" with default writeOverlay=true
  auto renameLock = mount.getEdenMount()->acquireRenameLock();
  dir->childMaterialized(renameLock, "b.txt"_pc);

  // Both in-memory and overlay should show b.txt as materialized
  {
    auto contents = dir->lockContentsRead();
    EXPECT_TRUE(contents->entries.at("b.txt"_pc).isMaterialized());
  }
  {
    auto overlayDir = overlay->loadOverlayDir(dirIno);
    EXPECT_TRUE(overlayDir.at("b.txt"_pc).isMaterialized());
  }
}

TEST_P(TreeInodeTestBase, childDematerializedSkipsOverlayWrite) {
  FakeTreeBuilder builder;
  builder.setFiles({
      {"dir/a.txt", "content_a"},
      {"dir/b.txt", "content_b"},
  });
  TestMount mount{builder};
  maybeEnableCoroutines(mount);

  // Materialize "dir" and "b.txt" by writing to b.txt
  mount.overwriteFile("dir/b.txt", "modified\n");

  auto dir = mount.getTreeInode("dir"_relpath);
  auto dirIno = dir->getNodeId();
  auto* overlay = mount.getEdenMount()->getOverlay();

  // b.txt should be materialized in both memory and overlay
  ASSERT_TRUE(dir->isMaterialized());
  {
    auto overlayDir = overlay->loadOverlayDir(dirIno);
    EXPECT_TRUE(overlayDir.at("b.txt"_pc).isMaterialized());
  }

  // Call childDematerialized for "b.txt" with writeOverlay=false
  auto renameLock = mount.getEdenMount()->acquireRenameLock();
  dir->childDematerialized(
      renameLock,
      "b.txt"_pc,
      ObjectId{"b_hash"},
      /*writeOverlay=*/false);

  // In-memory: b.txt should now be dematerialized
  {
    auto contents = dir->lockContentsRead();
    EXPECT_FALSE(contents->entries.at("b.txt"_pc).isMaterialized());
  }

  // On-disk overlay was NOT updated — b.txt should still show as
  // materialized in the persisted overlay data
  {
    auto overlayDir = overlay->loadOverlayDir(dirIno);
    EXPECT_TRUE(overlayDir.at("b.txt"_pc).isMaterialized());
  }
}

#endif // _WIN32

namespace {
StoredId* addReadyCommit(
    TestMount& testMount,
    const RootId& rootId,
    FakeTreeBuilder& builder) {
  builder.finalize(testMount.getBackingStore(), true);
  auto* commit = testMount.getBackingStore()->putCommit(rootId, builder);
  if (commit) {
    commit->setReady();
  }
  return commit;
}

CheckoutResult checkoutTo(TestMount& testMount, RootId rootId) {
  auto checkoutResult = testMount.checkout(
      std::move(rootId), ObjectFetchContext::getNullContext(), __func__);
  testMount.drainServerExecutor();
  return std::move(checkoutResult).get();
}

void expectAclDirState(TestMount& testMount, AclRootState expected) {
  auto rootInode = testMount.getEdenMount()->getRootInode();
  auto contents = rootInode->lockContentsRead();
  auto aclIter = contents->entries.find("acl_dir"_pc);
  ASSERT_NE(aclIter, contents->entries.end());
  EXPECT_TRUE(aclIter->second.isDirectory());
  EXPECT_EQ(aclIter->second.aclRootState(), expected);
}
} // namespace

TEST_P(TreeInodeTestBase, checkoutPropagatesIsRestricted) {
  // Start with a tree that has no ACL directories.
  FakeTreeBuilder builder1;
  builder1.setFile("src/main.c", "int main() { return 0; }\n");
  builder1.setFile("normal_dir/file.txt", "content");
  TestMount testMount{builder1};
  maybeEnableCoroutines(testMount);

  // Create a second commit that adds an ACL directory.
  auto builder2 = builder1.clone();
  builder2.setFile("acl_dir/file.txt", "acl content");
  builder2.setDirIsRestricted("acl_dir");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit(RootId{"2"}, builder2);
  commit2->setReady();

  auto checkoutResult = testMount.checkout(
      RootId{"2"}, ObjectFetchContext::getNullContext(), __func__);
  testMount.drainServerExecutor();
  ASSERT_TRUE(checkoutResult.isReady());
  auto result = std::move(checkoutResult).get();
  EXPECT_EQ(0, result.conflicts.size());

  // Verify the new acl_dir entry has isRestricted set.
  auto rootInode = testMount.getEdenMount()->getRootInode();
  auto contents = rootInode->lockContentsRead();

  auto aclIter = contents->entries.find("acl_dir"_pc);
  ASSERT_NE(aclIter, contents->entries.end());
  EXPECT_TRUE(aclIter->second.isDirectory());
  EXPECT_TRUE(aclIter->second.isRestricted());
}

TEST_P(TreeInodeTestBase, checkoutRemovesRestrictionWhenAclRemoved) {
  // Start with a tree that has an ACL directory.
  FakeTreeBuilder builder1;
  builder1.setFile("src/main.c", "int main() { return 0; }\n");
  builder1.setFile("acl_dir/file.txt", "acl content");
  builder1.setDirIsRestricted("acl_dir");
  TestMount testMount{builder1};
  maybeEnableCoroutines(testMount);

  // Create a second commit where acl_dir exists but without isRestricted.
  // Build from scratch rather than cloning since clone preserves isRestricted.
  FakeTreeBuilder builder2;
  builder2.setFile("src/main.c", "int main() { return 0; }\n");
  builder2.setFile("acl_dir/file.txt", "acl content modified");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit(RootId{"2"}, builder2);
  commit2->setReady();

  auto checkoutResult = testMount.checkout(
      RootId{"2"}, ObjectFetchContext::getNullContext(), __func__);
  testMount.drainServerExecutor();
  ASSERT_TRUE(checkoutResult.isReady());
  auto result = std::move(checkoutResult).get();
  EXPECT_EQ(0, result.conflicts.size());

  // Verify acl_dir no longer has isRestricted set.
  auto rootInode = testMount.getEdenMount()->getRootInode();
  auto contents = rootInode->lockContentsRead();

  auto aclIter = contents->entries.find("acl_dir"_pc);
  ASSERT_NE(aclIter, contents->entries.end());
  EXPECT_TRUE(aclIter->second.isDirectory());
  EXPECT_FALSE(aclIter->second.isRestricted());
  EXPECT_EQ(aclIter->second.aclRootState(), AclRootState::NoAcl);
}

TEST_P(TreeInodeTestBase, checkoutClearsRestrictionButPreservesAclRoot) {
  FakeTreeBuilder builder1;
  builder1.setFile("src/main.c", "int main() { return 0; }\n");
  builder1.setFile("acl_dir/file.txt", "acl content");
  builder1.setDirIsRestricted("acl_dir");
  TestMount testMount{builder1};
  maybeEnableCoroutines(testMount);

  FakeTreeBuilder builder2;
  builder2.setFile("src/main.c", "int main() { return 0; }\n");
  builder2.setFile("acl_dir/file.txt", "acl content modified");
  builder2.setDirHasAcl("acl_dir");
  builder2.finalize(testMount.getBackingStore(), true);
  auto* commit2 = testMount.getBackingStore()->putCommit(RootId{"2"}, builder2);
  if (commit2 == nullptr) {
    ADD_FAILURE() << "failed to create target commit";
    return;
  }
  commit2->setReady();

  auto checkoutResult = testMount.checkout(
      RootId{"2"}, ObjectFetchContext::getNullContext(), __func__);
  testMount.drainServerExecutor();
  ASSERT_TRUE(checkoutResult.isReady());
  auto result = std::move(checkoutResult).get();
  EXPECT_EQ(0, result.conflicts.size());

  auto rootInode = testMount.getEdenMount()->getRootInode();
  auto contents = rootInode->lockContentsRead();
  auto aclIter = contents->entries.find("acl_dir"_pc);
  ASSERT_NE(aclIter, contents->entries.end());
  EXPECT_FALSE(aclIter->second.isRestricted());
  EXPECT_EQ(aclIter->second.aclRootState(), AclRootState::AclRoot);
}

TEST_P(TreeInodeTestBase, checkoutIgnoresNonRestrictedAclRootMetadataChange) {
  FakeTreeBuilder builder1;
  builder1.setFile("src/main.c", "int main() { return 0; }\n");
  builder1.setFile("acl_dir/file.txt", "acl content");
  TestMount testMount{builder1};
  maybeEnableCoroutines(testMount);

  {
    auto rootInode = testMount.getEdenMount()->getRootInode();
    auto contents = rootInode->lockContentsWrite();
    auto aclIter = contents->entries.find("acl_dir"_pc);
    ASSERT_NE(aclIter, contents->entries.end());
    aclIter->second.setAclRootState(AclRootState::AclRoot);
  }
  expectAclDirState(testMount, AclRootState::AclRoot);

  FakeTreeBuilder builder2;
  builder2.setFile("src/main.c", "int main() { return 0; }\n");
  builder2.setFile("acl_dir/file.txt", "acl content");
  ASSERT_NE(nullptr, addReadyCommit(testMount, RootId{"2"}, builder2));

  auto result = checkoutTo(testMount, RootId{"2"});
  EXPECT_EQ(0, result.conflicts.size());
  expectAclDirState(testMount, AclRootState::AclRoot);
}

TEST_P(TreeInodeTestBase, checkoutAddsRestrictionWhenAclAdded) {
  // Start with a tree where acl_dir does not have isRestricted.
  FakeTreeBuilder builder1;
  builder1.setFile("src/main.c", "int main() { return 0; }\n");
  builder1.setFile("acl_dir/file.txt", "acl content");
  TestMount testMount{builder1};
  maybeEnableCoroutines(testMount);

  // Create a second commit where acl_dir has isRestricted set and content
  // changes.
  auto builder2 = builder1.clone();
  builder2.replaceFile("acl_dir/file.txt", "acl content modified");
  builder2.setDirIsRestricted("acl_dir");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit(RootId{"2"}, builder2);
  commit2->setReady();

  auto checkoutResult = testMount.checkout(
      RootId{"2"}, ObjectFetchContext::getNullContext(), __func__);
  testMount.drainServerExecutor();
  ASSERT_TRUE(checkoutResult.isReady());
  auto result = std::move(checkoutResult).get();
  EXPECT_EQ(0, result.conflicts.size());

  // Verify acl_dir now has isRestricted set.
  auto rootInode = testMount.getEdenMount()->getRootInode();
  auto contents = rootInode->lockContentsRead();

  auto aclIter = contents->entries.find("acl_dir"_pc);
  ASSERT_NE(aclIter, contents->entries.end());
  EXPECT_TRUE(aclIter->second.isDirectory());
  EXPECT_TRUE(aclIter->second.isRestricted());
}

namespace {
CheckoutConflict makeConflict(
    ConflictType type,
    folly::StringPiece path,
    folly::StringPiece message = "",
    Dtype dtype = Dtype::UNKNOWN) {
  CheckoutConflict conflict;
  conflict.type() = type;
  conflict.path() = path.str();
  conflict.message() = message.str();
  conflict.dtype() = dtype;
  return conflict;
}
} // namespace

TEST_P(
    TreeInodeTestBase,
    checkoutAddsRestrictionConflictsWithDirtyUnrestrictedDir) {
  // Start with a tree where acl_dir does not have isRestricted.
  FakeTreeBuilder builder1;
  builder1.setFile("src/main.c", "int main() { return 0; }\n");
  builder1.setFile("acl_dir/file.txt", "acl content");
  TestMount testMount{builder1};
  maybeEnableCoroutines(testMount);

  // Materialize acl_dir by writing to a file inside it. This makes
  // acl_dir's subtree "dirty" from the checkout pre-check's perspective.
  testMount.overwriteFile("acl_dir/file.txt", "local modification");

  // Create a second commit where acl_dir has isRestricted set AND content
  // changes, to force checkout to process the acl_dir entry.
  auto builder2 = builder1.clone();
  builder2.replaceFile("acl_dir/file.txt", "acl content modified");
  builder2.setDirIsRestricted("acl_dir");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit(RootId{"2"}, builder2);
  commit2->setReady();

  auto checkoutResult = testMount.checkout(
      RootId{"2"},
      ObjectFetchContext::getNullContext(),
      __func__,
      CheckoutMode::NORMAL);
  testMount.drainServerExecutor();
  ASSERT_TRUE(checkoutResult.isReady());
  auto result = std::move(checkoutResult).get();

  // Expect the pre-check to surface the modified tracked file inside the
  // restricted destination as a file-level MODIFIED_REMOVED conflict.
  EXPECT_THAT(
      result.conflicts,
      ::testing::UnorderedElementsAre(makeConflict(
          ConflictType::MODIFIED_REMOVED,
          "acl_dir/file.txt",
          "",
          Dtype::REGULAR)));
}

TEST_P(
    TreeInodeTestBase,
    checkoutForceAddsRestrictionOverDirtyUnrestrictedDir) {
  // Same dirty setup as the non-force test.
  FakeTreeBuilder builder1;
  builder1.setFile("src/main.c", "int main() { return 0; }\n");
  builder1.setFile("acl_dir/file.txt", "acl content");
  TestMount testMount{builder1};
  maybeEnableCoroutines(testMount);

  testMount.overwriteFile("acl_dir/file.txt", "local modification");

  auto builder2 = builder1.clone();
  builder2.replaceFile("acl_dir/file.txt", "acl content modified");
  builder2.setDirIsRestricted("acl_dir");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit(RootId{"2"}, builder2);
  commit2->setReady();

  // Force mode should report the conflict AND still apply the transition
  // (matches the force-path semantics in processCheckoutEntryImpl).
  auto checkoutResult = testMount.checkout(
      RootId{"2"},
      ObjectFetchContext::getNullContext(),
      __func__,
      CheckoutMode::FORCE);
  testMount.drainServerExecutor();
  ASSERT_TRUE(checkoutResult.isReady());
  auto result = std::move(checkoutResult).get();

  EXPECT_THAT(
      result.conflicts,
      ::testing::Contains(makeConflict(
          ConflictType::MODIFIED_REMOVED,
          "acl_dir/file.txt",
          "",
          Dtype::REGULAR)));

  // Verify acl_dir is now restricted — force mode drove the swap through.
  auto rootInode = testMount.getEdenMount()->getRootInode();
  auto contents = rootInode->lockContentsRead();

  auto aclIter = contents->entries.find("acl_dir"_pc);
  ASSERT_NE(aclIter, contents->entries.end());
  EXPECT_TRUE(aclIter->second.isDirectory());
  EXPECT_TRUE(aclIter->second.isRestricted());
}

CO_TEST(TreeInodeTest, co_statOnFileInode) {
  FakeTreeBuilder builder;
  builder.setFiles({{"dir/file.txt", "contents"}});
  TestMount mount{builder};
  // No maybeEnableCoroutines: direct co_stat call.

  auto fileInode = mount.getFileInode("dir/file.txt"_relpath);
  auto st = co_await fileInode->co_stat(ObjectFetchContext::getNullContext());
  EXPECT_GT(st.st_size, 0);
}

CO_TEST(TreeInodeTest, co_getTreeAuxDataOnTreeInode) {
  FakeTreeBuilder builder;
  builder.setFiles({{"dir/file.txt", "contents"}});
  TestMount mount{builder};
  // No maybeEnableCoroutines: direct co_getTreeAuxData call.

  auto dirInode = mount.getTreeInode("dir"_relpath);
  // FakeBackingStore does not implement getTreeAuxData, so the underlying
  // backing-store call throws std::domain_error. The futures path
  // (getTreeAuxData) propagates this exception; the coroutine path mirrors
  // that behavior.
  auto result = co_await folly::coro::co_awaitTry(
      dirInode->co_getTreeAuxData(ObjectFetchContext::getNullContext()));
  EXPECT_TRUE(result.hasException());
  EXPECT_TRUE(result.hasException<std::domain_error>());
}

INSTANTIATE_TEST_SUITE_P(
    TreeInodeTestVariants,
    TreeInodeTestBase,
    ::testing::Bool(),
    [](const ::testing::TestParamInfo<bool>& info) {
      return info.param ? "Coroutines" : "Futures";
    });

TEST(TreeInode, getOrLoadChildIfExists) {
  FakeTreeBuilder builder;
  builder.setFile("dir/present.txt", "x");
  TestMount mount{builder};
  auto dir = mount.getTreeInode("dir");
  auto ctx = ObjectFetchContext::getNullContext();
  auto run = [&mount](auto&& future) {
    auto fut = std::forward<decltype(future)>(future).semi().via(
        mount.getServerExecutor().get());
    mount.drainServerExecutor();
    return std::move(fut).get(0ms);
  };

  EXPECT_FALSE(run(dir->getOrLoadChildIfExists("missing.txt"_pc, ctx)));

  // The first call loads the child; the second finds it loaded.
  auto loaded = run(dir->getOrLoadChildIfExists("present.txt"_pc, ctx));
  ASSERT_TRUE(loaded);
  EXPECT_EQ(RelativePath{"dir/present.txt"}, loaded->getPath().value());
  EXPECT_EQ(
      loaded.get(),
      run(dir->getOrLoadChildIfExists("present.txt"_pc, ctx)).get());
}
