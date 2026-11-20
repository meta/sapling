/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/inodes/GlobNode.h"
#include "eden/fs/utils/GlobResult.h"

#include <algorithm>
#include <atomic>
#include <utility>

#include <folly/Conv.h>
#include <folly/Range.h>
#include <folly/Try.h>
#include <folly/coro/BlockingWait.h>
#include <folly/coro/Collect.h>
#include <folly/coro/GtestHelpers.h>
#include <folly/coro/Invoke.h>
#include <folly/coro/Task.h>
#include <folly/test/TestUtils.h>
#include <folly/testing/TestUtil.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/model/TestOps.h"
#include "eden/fs/testharness/FakeBackingStore.h"
#include "eden/fs/testharness/FakeTreeBuilder.h"
#include "eden/fs/testharness/TestChecks.h"
#include "eden/fs/testharness/TestMount.h"

using namespace facebook::eden;
using namespace folly::string_piece_literals;

namespace {
/**
 * Issue a glob request.
 *
 * Note: This coroutine executes on the server executor which thus needs to be
 * manually drained for the returned Future to be ready.
 */
folly::coro::Task<std::vector<GlobResult>> evaluateGlob(
    TestMount& mount,
    GlobNode& globRoot,
    std::shared_ptr<GlobNode::PrefetchList> prefetchIds,
    const RootId& commitId) {
  auto rootInode = mount.getTreeInode(RelativePathPiece());
  auto objectStore = mount.getEdenMount()->getObjectStore();
  auto globResults =
      std::make_shared<folly::Synchronized<std::vector<GlobResult>>>();
  co_await globRoot.evaluate(
      std::move(objectStore),
      ObjectFetchContext::getNullContext(),
      RelativePathPiece(),
      rootInode,
      prefetchIds.get(),
      globResults.get(),
      commitId);
  std::vector<GlobResult> result;
  std::swap(result, *globResults->wlock());
  co_return result;
}

const RootId kZeroRootId{};

} // namespace

enum StartReady : bool {
  Defer = false,
  Start = true,
};

enum Prefetch : bool {
  NoPrefetch = false,
  PrefetchBlobs = true,
};

class GlobNodeTest : public ::testing::TestWithParam<
                         std::pair<enum StartReady, enum Prefetch>> {
 protected:
  void SetUp() override {
    // The file contents are coupled with AId, BId and WatId below.
    builder_.setFiles(
        {{"dir/a.txt", "a"},
         {"dir/sub/b.txt", "b"},
         {".watchmanconfig", "wat"}});
    mount_.initialize(builder_, /*startReady=*/GetParam().first);
    prefetchIds_ = nullptr;
  }

  std::vector<GlobResult> doGlob(
      folly::StringPiece pattern,
      bool includeDotfiles,
      const RootId& commitId) {
    GlobNode globRoot(
        /*includeDotfiles=*/includeDotfiles,
        mount_.getConfig()->getCaseSensitive());
    globRoot.parse(pattern);
    return doGlob(globRoot, commitId);
  }

  std::vector<GlobResult> doGlob(GlobNode& globRoot, const RootId& commitId) {
    globRoot.debugDump();

    if (shouldPrefetch()) {
      prefetchIds_ = std::make_shared<GlobNode::PrefetchList>();
    }

    if (!GetParam().first) {
      builder_.setAllReady();
    }
    return folly::coro::blockingWait(
        evaluateGlob(mount_, globRoot, prefetchIds_, commitId),
        mount_.getServerExecutor().get());
  }

  std::vector<GlobResult> doGlobIncludeDotFiles(
      folly::StringPiece pattern,
      const RootId& commitId) {
    return doGlob(pattern, true, commitId);
  }

  std::vector<GlobResult> doGlobExcludeDotFiles(
      folly::StringPiece pattern,
      const RootId& commitId) {
    return doGlob(pattern, false, commitId);
  }

  void addDotDirectory() {
    mount_.mkdir(".hidden");
    mount_.addFile(".hidden/regular.txt", "regular");
    mount_.addFile(".hidden/.dot", "dot");
  }

  bool shouldPrefetch() const {
    return GetParam().second;
  }

  std::vector<ObjectId> getPrefetchIds() const {
    return *prefetchIds_->rlock();
  }

  TestMount mount_;
  FakeTreeBuilder builder_;
  std::shared_ptr<GlobNode::PrefetchList> prefetchIds_;
};

TEST_P(GlobNodeTest, starTxt) {
  auto matches = doGlobIncludeDotFiles("*.txt", kZeroRootId);
  EXPECT_TRUE(matches.empty());
  if (shouldPrefetch()) {
    EXPECT_TRUE(getPrefetchIds().empty());
  }
}

// The ids FakeBackingStore generates: its type byte (0x3f), then the SHA-1 of
// the contents.
// id of "a"
const ObjectId AId =
    ObjectId::fromHex("3f86f7e437faa5a7fce15d1ddcb9eaeaea377667b8");
// id of "b"
const ObjectId BId =
    ObjectId::fromHex("3fe9d71f5ee7c92d6dc9e92ffdad17b8bd49418f98");
// id of "wat"
const ObjectId WatId =
    ObjectId::fromHex("3fa3bbe1a8f2f025b8b6c5b66937763bb2b9bebdf2");

TEST_P(GlobNodeTest, matchFilesByExtensionRecursively) {
  auto matches = doGlobIncludeDotFiles("**/*.txt", kZeroRootId);

  std::vector<GlobResult> expect{
      GlobResult("dir/a.txt"_relpath, dtype_t::Regular, kZeroRootId),
      GlobResult("dir/sub/b.txt"_relpath, dtype_t::Regular, kZeroRootId),
  };
  EXPECT_EQ(expect, matches);

  if (shouldPrefetch()) {
    std::vector<ObjectId> expectIds{AId, BId};
    EXPECT_EQ(expectIds, getPrefetchIds());
  }
}

TEST_P(GlobNodeTest, star) {
  auto matches = doGlobIncludeDotFiles("*", kZeroRootId);

  std::vector<GlobResult> expect{
      GlobResult(".eden"_relpath, dtype_t::Dir, kZeroRootId),
      GlobResult(".watchmanconfig"_relpath, dtype_t::Regular, kZeroRootId),
      GlobResult("dir"_relpath, dtype_t::Dir, kZeroRootId)};
  EXPECT_EQ(expect, matches);

  if (shouldPrefetch()) {
    std::vector<ObjectId> expectIds{WatId};
    EXPECT_EQ(expectIds, getPrefetchIds());
  }
}

TEST_P(GlobNodeTest, starExcludeDot) {
  auto matches = doGlobExcludeDotFiles("*", kZeroRootId);

  std::vector<GlobResult> expect{
      GlobResult("dir"_relpath, dtype_t::Dir, kZeroRootId)};
  EXPECT_EQ(expect, matches);
}

TEST_P(GlobNodeTest, starStarExcludeDot) {
  auto matches = doGlobExcludeDotFiles("dir/sub/**/sub/b.txt", kZeroRootId);

  std::vector<GlobResult> expect;
  EXPECT_EQ(expect, matches);
}

TEST_P(GlobNodeTest, starStarRootExcludeDot) {
  auto matches = doGlobExcludeDotFiles("**/root", kZeroRootId);

  std::vector<GlobResult> expect;
  EXPECT_EQ(expect, matches);
}

TEST_P(GlobNodeTest, starStarStarStarExcludeDot) {
  addDotDirectory();
  auto matches = doGlobExcludeDotFiles("**/**", kZeroRootId);

  std::vector<GlobResult> expect{
      GlobResult("dir"_relpath, dtype_t::Dir, kZeroRootId),
      GlobResult("dir/a.txt"_relpath, dtype_t::Regular, kZeroRootId),
      GlobResult("dir/sub"_relpath, dtype_t::Dir, kZeroRootId),
      GlobResult("dir/sub/b.txt"_relpath, dtype_t::Regular, kZeroRootId),
  };
  std::sort(matches.begin(), matches.end());
  EXPECT_EQ(expect, matches);
}

TEST_P(GlobNodeTest, qmarkExcludeDot) {
  auto matches = doGlobExcludeDotFiles("?watchmanconfig", kZeroRootId);

  std::vector<GlobResult> expect;
  EXPECT_EQ(expect, matches);
}

TEST_P(GlobNodeTest, dotDirectoryStarExcludeDot) {
  addDotDirectory();
  auto matches = doGlobExcludeDotFiles(".hidden/*", kZeroRootId);

  std::vector<GlobResult> expect{
      GlobResult(".hidden/regular.txt"_relpath, dtype_t::Regular, kZeroRootId)};
  EXPECT_EQ(expect, matches);
}

TEST_P(GlobNodeTest, dotDirectoryStarStarExcludeDot) {
  addDotDirectory();
  auto matches = doGlobExcludeDotFiles(".hidden/**", kZeroRootId);

  std::vector<GlobResult> expect{
      GlobResult(".hidden/regular.txt"_relpath, dtype_t::Regular, kZeroRootId)};
  EXPECT_EQ(expect, matches);
}

TEST_P(GlobNodeTest, dotDirectoryQmarkExcludeDot) {
  addDotDirectory();
  auto matches = doGlobExcludeDotFiles(".hidden/?dot", kZeroRootId);

  std::vector<GlobResult> expect;
  EXPECT_EQ(expect, matches);
}

TEST_P(GlobNodeTest, dotDirectoryRecursiveTxtExcludeDot) {
  addDotDirectory();
  auto matches = doGlobExcludeDotFiles("**/*.txt", kZeroRootId);

  std::vector<GlobResult> expect{
      GlobResult("dir/a.txt"_relpath, dtype_t::Regular, kZeroRootId),
      GlobResult("dir/sub/b.txt"_relpath, dtype_t::Regular, kZeroRootId),
  };
  EXPECT_EQ(expect, matches);
}

TEST_P(GlobNodeTest, dotDirectoryStarIncludeDot) {
  addDotDirectory();
  auto matches = doGlobIncludeDotFiles(".hidden/*", kZeroRootId);

  std::vector<GlobResult> expect{
      GlobResult(".hidden/.dot"_relpath, dtype_t::Regular, kZeroRootId),
      GlobResult(".hidden/regular.txt"_relpath, dtype_t::Regular, kZeroRootId),
  };
  EXPECT_EQ(expect, matches);
}

TEST_P(GlobNodeTest, starStarBeginning) {
  auto matches = doGlobExcludeDotFiles("**/sub/b.txt", kZeroRootId);

  std::vector<GlobResult> expect{
      GlobResult("dir/sub/b.txt"_relpath, dtype_t::Regular, kZeroRootId),
  };
  EXPECT_EQ(expect, matches);
}

#ifndef _WIN32
TEST_P(GlobNodeTest, recursiveTxtWithChanges) {
  // Ensure that we enumerate things from the overlay
  mount_.addFile("root.txt", "added\n");
  mount_.addSymlink("sym.txt", "root.txt");
  // The mode change doesn't directly impact the results, but
  // does cause us to materialize this entry.  We just want
  // to make sure that it continues to show up after the change.
  builder_.setReady("dir");
  builder_.setReady("dir/a.txt");
  mount_.chmod("dir/a.txt", 0777);

  auto matches = doGlobIncludeDotFiles("**/*.txt", kZeroRootId);

  std::vector<GlobResult> expect{
      GlobResult("root.txt"_relpath, dtype_t::Regular, kZeroRootId),
      GlobResult("sym.txt"_relpath, dtype_t::Symlink, kZeroRootId),
      GlobResult("dir/a.txt"_relpath, dtype_t::Regular, kZeroRootId),
      GlobResult("dir/sub/b.txt"_relpath, dtype_t::Regular, kZeroRootId),
  };
  EXPECT_EQ(expect, matches);

  if (shouldPrefetch()) {
    std::vector<ObjectId> expectIds{
        // No root.txt, as it is in the overlay
        // No sym.txt, as it is in the overlay
        // No AId as we chmod'd the file and thus materialized it
        BId};
    EXPECT_EQ(expectIds, getPrefetchIds());
  }
}
#endif

TEST_P(GlobNodeTest, matchGlobDirectoryAndDirectoryChild) {
  GlobNode globRoot(
      /*includeDotfiles=*/false, mount_.getConfig()->getCaseSensitive());
  globRoot.parse("dir/*");
  globRoot.parse("dir/*/*");

  auto matches = doGlob(globRoot, kZeroRootId);
  std::vector<GlobResult> expect{
      GlobResult("dir/a.txt"_relpath, dtype_t::Regular, kZeroRootId),
      GlobResult("dir/sub"_relpath, dtype_t::Dir, kZeroRootId),
      GlobResult("dir/sub/b.txt"_relpath, dtype_t::Regular, kZeroRootId),
  };
  EXPECT_EQ(expect, matches);
}

TEST_P(GlobNodeTest, matchGlobDirectoryAndDirectoryRecursiveChildren) {
  GlobNode globRoot(
      /*includeDotfiles=*/false, mount_.getConfig()->getCaseSensitive());
  globRoot.parse("dir/*");
  globRoot.parse("dir/*/**");

  auto matches = doGlob(globRoot, kZeroRootId);
  std::vector<GlobResult> expect{
      GlobResult("dir/a.txt"_relpath, dtype_t::Regular, kZeroRootId),
      GlobResult("dir/sub"_relpath, dtype_t::Dir, kZeroRootId),
      GlobResult("dir/sub/b.txt"_relpath, dtype_t::Regular, kZeroRootId),
  };
  EXPECT_EQ(expect, matches);
}

TEST_P(GlobNodeTest, matchLiteralDirectoryAndDirectoryChild) {
  GlobNode globRoot(
      /*includeDotfiles=*/false, mount_.getConfig()->getCaseSensitive());
  globRoot.parse("dir");
  globRoot.parse("dir/a.txt");

  auto matches = doGlob(globRoot, kZeroRootId);
  std::vector<GlobResult> expect{
      GlobResult("dir"_relpath, dtype_t::Dir, kZeroRootId),
      GlobResult("dir/a.txt"_relpath, dtype_t::Regular, kZeroRootId),
  };
  EXPECT_EQ(expect, matches);
}

TEST_P(GlobNodeTest, matchLiteralDirectoryAndDirectoryRecursiveChildren) {
  GlobNode globRoot(
      /*includeDotfiles=*/false, mount_.getConfig()->getCaseSensitive());
  globRoot.parse("dir");
  globRoot.parse("dir/**");

  auto matches = doGlob(globRoot, kZeroRootId);
  std::vector<GlobResult> expect{
      GlobResult("dir"_relpath, dtype_t::Dir, kZeroRootId),
      GlobResult("dir/a.txt"_relpath, dtype_t::Regular, kZeroRootId),
      GlobResult("dir/sub"_relpath, dtype_t::Dir, kZeroRootId),
      GlobResult("dir/sub/b.txt"_relpath, dtype_t::Regular, kZeroRootId),
  };
  EXPECT_EQ(expect, matches);
}

const std::pair<enum StartReady, enum Prefetch> combinations[] = {
    {StartReady::Start, Prefetch::NoPrefetch},
    {StartReady::Start, Prefetch::PrefetchBlobs},
    {StartReady::Defer, Prefetch::NoPrefetch},
    {StartReady::Defer, Prefetch::PrefetchBlobs},
};

#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
INSTANTIATE_TEST_CASE_P(Glob, GlobNodeTest, ::testing::ValuesIn(combinations));
#pragma clang diagnostic pop

TEST(GlobNodeTest, matchingDirectoryDoesNotLoadTree) {
  auto mount = TestMount{};
  auto builder = FakeTreeBuilder{};
  builder.setFiles({{"dir/subdir/file", ""}});
  mount.initialize(builder, /*startReady=*/false);
  builder.setReady("dir");
  EXPECT_FALSE(
      mount.getEdenMount()
          ->getInodeSlow(
              "dir/subdir"_relpath, ObjectFetchContext::getNullContext())
          .semi()
          .isReady())
      << "Loading dir/subdir should hang indefinitely";

  for (folly::StringPiece pattern : {"dir/*"_sp, "dir/subdir"_sp}) {
    SCOPED_TRACE(folly::to<std::string>("pattern = ", pattern));
    GlobNode globRoot(
        /*includeDotfiles=*/false, mount.getConfig()->getCaseSensitive());
    globRoot.parse("dir/*");
    globRoot.debugDump();

    auto matches = folly::coro::blockingWait(
        evaluateGlob(mount, globRoot, /*prefetchIds=*/nullptr, kZeroRootId),
        mount.getServerExecutor().get());

    EXPECT_FALSE(
        mount.getEdenMount()
            ->getInodeSlow(
                "dir/subdir"_relpath, ObjectFetchContext::getNullContext())
            .semi()
            .isReady())
        << "dir/subdir should still be unloaded after evaluating glob";
    EXPECT_EQ(
        (std::vector<GlobResult>{
            GlobResult("dir/subdir"_relpath, dtype_t::Dir, kZeroRootId),
        }),
        matches);
  }
}

namespace {
// Runs the glob evaluation, capturing its result (including any exception) into
// evalResult and signalling completion via evalDone.
folly::coro::Task<void> co_runGlobEvaluation(
    GlobNode& globRoot,
    std::shared_ptr<ObjectStore> objectStore,
    TreeInodePtr rootInode,
    ResultList* globResults,
    const RootId& commitId,
    folly::Try<folly::Unit>& evalResult,
    std::atomic<bool>& evalDone) {
  evalResult = co_await folly::coro::co_awaitTry(globRoot.evaluate(
      std::move(objectStore),
      ObjectFetchContext::getNullContext(),
      RelativePathPiece(),
      rootInode,
      /*fileBlobsToPrefetch=*/nullptr,
      globResults,
      commitId));
  evalDone.store(true);
}

// Drives the mount's ManualExecutor while injecting a fault on dir/a/b, then
// asserts the evaluation does not complete until dir/b and dir/c also finish
// loading before releasing the rest of the tree.
folly::coro::Task<void> co_driveTreeLoadErrorFault(
    TestMount& mount,
    FakeTreeBuilder& builder,
    std::atomic<bool>& evalDone) {
  // Let the evaluation start loading subtrees.
  mount.drainServerExecutor();
  co_await folly::coro::co_reschedule_on_current_executor;
  EXPECT_FALSE(evalDone.load())
      << "glob should not finish when some subtrees are not read";

  // Cause dir/a/b to fail to load
  builder.triggerError("dir/a/b", std::runtime_error("cosmic radiation"));
  mount.drainServerExecutor();
  co_await folly::coro::co_reschedule_on_current_executor;

  // We still haven't allowed the rest of the trees to finish loading,
  // so the glob shouldn't be finished yet.
  //
  // This test case is checking for a regression where the glob evaluation would
  // complete early when an error occurred processing one TreeInode, even
  // though work was still being done to process the glob on other subtrees.
  // Completion of the glob evaluation signals the caller that they can destroy
  // the GlobNode, but this isn't safe if there is still work in progress to
  // evaluate it, even if that work will eventually get discarded due to the
  // original error.
  EXPECT_FALSE(evalDone.load())
      << "glob should not finish early when still waiting on some trees";

  // Mark all of the remaining trees ready, which should allow the glob
  // evaluation to complete.
  builder.setAllReady();
  while (!evalDone.load()) {
    mount.drainServerExecutor();
    co_await folly::coro::co_reschedule_on_current_executor;
  }
}
} // namespace

CO_TEST(GlobNodeTest, treeLoadError) {
  auto mount = TestMount{};
  auto builder = FakeTreeBuilder{};
  builder.setFiles({
      {"dir/a/foo.txt", "foo"},
      {"dir/a/b/a.txt", "foo"},
      {"dir/a/b/b.txt", "foo"},
      {"dir/b/a/a.txt", "foo"},
      {"dir/b/a/b.txt", "foo"},
      {"dir/c/a/a.txt", "foo"},
      {"dir/c/x.txt", "foo"},
      {"dir/c/y.txt", "foo"},
  });
  mount.initialize(builder, /*startReady=*/false);
  builder.setReady("dir");
  builder.setReady("dir/a");

  // Disable async rescheduling of the recursive descent: the fault-injection
  // choreography below assumes subtree loads are requested synchronously, so
  // that the dir/a/b load is already pending when the error is triggered.
  GlobNode globRoot(
      /*includeDotfiles=*/false,
      mount.getConfig()->getCaseSensitive(),
      /*recursiveAsyncDepth=*/0);
  globRoot.parse("dir/**/a.txt");

  auto rootInode = mount.getTreeInode(RelativePathPiece());
  auto objectStore = mount.getEdenMount()->getObjectStore();
  auto globResults =
      std::make_shared<folly::Synchronized<std::vector<GlobResult>>>();

  folly::Try<folly::Unit> evalResult;
  std::atomic<bool> evalDone{false};

  // Run the evaluation concurrently with a controller that drives the
  // ManualExecutor and injects a fault while the evaluation is in flight.
  co_await folly::coro::collectAll(
      co_runGlobEvaluation(
          globRoot,
          std::move(objectStore),
          rootInode,
          globResults.get(),
          kZeroRootId,
          evalResult,
          evalDone),
      co_driveTreeLoadErrorFault(mount, builder, evalDone));

  EXPECT_TRUE(evalResult.hasException());
  EXPECT_THAT(
      evalResult.exception().what().toStdString(),
      testing::HasSubstr("cosmic radiation"));
}

TEST_P(GlobNodeTest, testCommitIdSet) {
  const RootId randomId{"37ce5515c1b313ce722366c31c10db0883fff7e0"};

  auto matches = doGlobIncludeDotFiles("**/*.txt", randomId);

  std::vector<GlobResult> expect{
      GlobResult("dir/a.txt"_relpath, dtype_t::Regular, randomId),
      GlobResult("dir/sub/b.txt"_relpath, dtype_t::Regular, randomId),
  };
  EXPECT_EQ(expect, matches);

  if (shouldPrefetch()) {
    std::vector<ObjectId> expectIds{AId, BId};
    EXPECT_EQ(expectIds, getPrefetchIds());
  }
}

TEST(GlobNodeTest, testCaseInsensitive) {
  auto mount = TestMount{CaseSensitivity::Insensitive};
  auto builder = FakeTreeBuilder{};
  builder.setFiles({{"case/MIXEDcase", "a"}, {"Foo/Bar", ""}, {"Foo/Baz", ""}});
  mount.initialize(builder, /*startReady=*/true);

  GlobNode globRoot(
      /*includeDotfiles=*/false, mount.getConfig()->getCaseSensitive());
  globRoot.parse("Case");
  globRoot.parse("CASE/MixedCase");
  globRoot.parse("CASE/MixedCase");
  globRoot.parse("f*/b?z");

  auto matches = folly::coro::blockingWait(
      evaluateGlob(mount, globRoot, /*prefetchIds=*/nullptr, kZeroRootId),
      mount.getServerExecutor().get());

  std::vector<GlobResult> expect{
      GlobResult("case"_relpath, dtype_t::Dir, kZeroRootId),
      GlobResult("case/MIXEDcase"_relpath, dtype_t::Regular, kZeroRootId),
      GlobResult("Foo/Baz"_relpath, dtype_t::Regular, kZeroRootId),
  };
  EXPECT_EQ(expect, matches);
}
