/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include <folly/ScopeGuard.h>
#include <folly/coro/BlockingWait.h>
#include <folly/coro/Collect.h>
#include <folly/coro/GtestHelpers.h>
#include <folly/coro/Task.h>
#include <folly/coro/Timeout.h>
#include <folly/executors/CPUThreadPoolExecutor.h>
#include <folly/executors/InlineExecutor.h>
#include <folly/logging/xlog.h>
#include <folly/testing/TestUtil.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <algorithm>
#include <array>
#include <memory>
#include <optional>
#include <string_view>
#include <utility>

#include "eden/common/utils/FaultInjector.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/config/ReloadableConfig.h"
#include "eden/fs/model/TestOps.h"
#include "eden/fs/store/BackingStoreLogger.h"
#include "eden/fs/store/ObjectStore.h"
#include "eden/fs/store/TreeCache.h"
#include "eden/fs/store/sl/SaplingBackingStore.h"
#include "eden/fs/store/sl/SaplingBackingStoreOptions.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/telemetry/ErrorLogger.h"
#include "eden/fs/telemetry/test/CapturingXplatLogger.h"
#include "eden/fs/testharness/HgRepo.h"
#include "eden/fs/testharness/TestConfigSource.h"
#include "eden/scm/lib/backingstore/include/SaplingBackingStoreError.h"

using namespace std::chrono_literals;

namespace facebook::eden {
namespace {
const auto kTestTimeout = 10s;

constexpr std::array<std::string_view, 2> kBlobFetchFaultPoints{
    "SaplingBackingStore::getBlobEnqueue",
    "SaplingBackingStore::co_getBlobEnqueue",
};

template <typename Future>
void expectRuntimeError(Future&& future, std::string_view message) {
  try {
    std::forward<Future>(future).get(kTestTimeout);
    FAIL() << "expected std::runtime_error";
  } catch (const std::runtime_error& ex) {
    EXPECT_EQ(message, ex.what());
  }
}

struct TestRepo {
  folly::test::TemporaryDirectory testDir{"eden_queued_hg_backing_store_test"};
  AbsolutePath testPath = canonicalPath(testDir.path().string());
  HgRepo repo{testPath + "repo"_pc};
  RootId commit1;
  Hash20 manifest1;

  TestRepo() {
    repo.hgInit(testPath + "cache"_pc, {}, /* isEagerRepo */ true);

    repo.mkdir("foo");
    repo.writeFile("foo/bar.txt", "bar\n");
    repo.mkdir("src");
    repo.writeFile("src/hello.txt", "world\n");
    repo.writeFile("foo.txt", "foo\n");
    repo.writeFile("bar.txt", "bar\n");
    repo.hg("add", "foo/bar.txt", "src/hello.txt", "foo.txt", "bar.txt");
    commit1 = repo.commit("Initial commit");
    manifest1 = repo.getManifestForCommit(commit1);
  }
};

std::vector<PathComponent> getTreeNames(
    const std::shared_ptr<const Tree>& tree) {
  std::vector<PathComponent> names;
  for (const auto& entry : *tree) {
    if (entry.second.isTree()) {
      names.emplace_back(entry.first);
    }
  }
  return names;
}

std::shared_ptr<EdenFsEventsLogger> makeTestEdenFsEventsLogger() {
  return std::make_shared<EdenFsEventsLogger>(nullptr);
}

struct SaplingBackingStoreTestBase : TestRepo, ::testing::Test {
  std::shared_ptr<EdenConfig> testEdenConfig =
      EdenConfig::createTestEdenConfig();
  std::shared_ptr<ReloadableConfig> edenConfig{
      std::make_shared<ReloadableConfig>(testEdenConfig)};
  EdenStatsPtr stats{makeRefPtr<EdenStats>()};
};

struct SaplingBackingStoreNoFaultInjectorTest : SaplingBackingStoreTestBase {
  FaultInjector faultInjector{/*enabled=*/false};
  // Use a real executor so coroutine tests don't trip the coro::Task
  // DCHECK on InlineExecutor (Task.h:470). See D98178331.
  folly::CPUThreadPoolExecutor executor{1};
  ErrorLogger noopErrorLogger{};

  std::shared_ptr<SaplingBackingStore> queuedBackingStore =
      std::make_shared<SaplingBackingStore>(
          repo.path(),
          kPathMapDefaultCaseSensitive,
          stats.copy(),
          &executor,
          edenConfig,
          std::make_unique<SaplingBackingStoreOptions>(),
          makeTestEdenFsEventsLogger(),
          /*errorLogger=*/noopErrorLogger,
          std::make_unique<BackingStoreLogger>(),
          &faultInjector);
};

struct SaplingBackingStoreWithFaultInjectorTest : SaplingBackingStoreTestBase {
  std::shared_ptr<TestConfigSource> testConfigSource{
      std::make_shared<TestConfigSource>(ConfigSourceType::SystemConfig)};
  FaultInjector faultInjector{/*enabled=*/true};
  // Use a real executor so coroutine tests don't trip the coro::Task
  // DCHECK on InlineExecutor (Task.h:470).
  folly::CPUThreadPoolExecutor executor{1};
  ErrorLogger noopErrorLogger{};

  std::shared_ptr<SaplingBackingStore> queuedBackingStore =
      std::make_shared<SaplingBackingStore>(
          repo.path(),
          kPathMapDefaultCaseSensitive,
          stats.copy(),
          &executor,
          edenConfig,
          std::make_unique<SaplingBackingStoreOptions>(),
          makeTestEdenFsEventsLogger(),
          /*errorLogger=*/noopErrorLogger,
          std::make_unique<BackingStoreLogger>(),
          &faultInjector);
};

struct SaplingBackingStoreWithFaultInjectorIgnoreConfigTest
    : SaplingBackingStoreTestBase {
  std::shared_ptr<TestConfigSource> testConfigSource{
      std::make_shared<TestConfigSource>(ConfigSourceType::SystemConfig)};
  FaultInjector faultInjector{/*enabled=*/true};
  // Use CPUThreadPoolExecutor — InlineExecutor is forbidden for coro::Task
  // (DCHECK on InlineExecutor at Task.h:470). See D98178331.
  folly::CPUThreadPoolExecutor executor{1};
  ErrorLogger noopErrorLogger{};

  std::shared_ptr<SaplingBackingStore> queuedBackingStore =
      std::make_shared<SaplingBackingStore>(
          repo.path(),
          kPathMapDefaultCaseSensitive,
          stats.copy(),
          &executor,
          edenConfig,
          std::make_unique<SaplingBackingStoreOptions>(),
          makeTestEdenFsEventsLogger(),
          /*errorLogger=*/noopErrorLogger,
          std::make_unique<BackingStoreLogger>(),
          &faultInjector);
};

template <typename Future, typename Duration>
std::string completionError(Future&& future, Duration timeout) {
  try {
    (void)std::forward<Future>(future).get(timeout);
    return "completed with a value";
  } catch (const folly::FutureTimeout&) {
    return "never completed";
  } catch (const std::exception& ex) {
    return ex.what();
  }
}
} // namespace

TEST_F(SaplingBackingStoreNoFaultInjectorTest, getTree) {
  auto tree1 = folly::coro::blockingWait(
      folly::coro::timeout(
          queuedBackingStore->co_getRootTree(
              commit1, ObjectFetchContext::getNullContext()),
          kTestTimeout));

  auto [tree2, origin2] =
      queuedBackingStore
          ->getTree(tree1.treeId, ObjectFetchContext::getNullContext())
          .get(kTestTimeout);

  EXPECT_TRUE(*tree1.tree == *tree2);
}

TEST_F(
    SaplingBackingStoreNoFaultInjectorTest,
    checkPermissionAcceptsObjectIdWithPath) {
  testEdenConfig->restrictedTreeTtlSeconds.setValue(
      0, ConfigSourceType::UserConfig, true);
  auto objectStore = ObjectStore::create(
      queuedBackingStore,
      TreeCache::create(edenConfig, stats.copy()),
      stats.copy(),
      nullptr,
      nullptr,
      edenConfig,
      CaseSensitivity::Sensitive,
      folly::getKeepAliveToken(&executor));
  auto rootTree =
      objectStore->getRootTree(commit1, ObjectFetchContext::getNullContext())
          .get(kTestTimeout);
  auto rootTreeId = SlOid{rootTree.treeId};
  auto idWithPath = SlOid{rootTreeId.node(), RelativePathPiece{"src"}}.oid();

  EXPECT_TRUE(objectStore
                  ->checkPermissionIfExpired(
                      idWithPath, std::chrono::steady_clock::now())
                  .get(kTestTimeout));
}

TEST_F(
    SaplingBackingStoreNoFaultInjectorTest,
    getTreeBatchConvertsPermissionDeniedToRestrictedTree) {
  auto id = ObjectId::fromHex("0123456789012345678901234567890123456789");
  auto denied = folly::Try<TreePtr>{
      folly::make_exception_wrapper<sapling::SaplingBackingStoreError>(
          "permission denied",
          sapling::BackingStoreErrorKind::PermissionDenied,
          std::nullopt)};

  auto converted = queuedBackingStore->convertPermissionDeniedToRestrictedTree(
      std::move(denied), id);

  ASSERT_FALSE(converted.hasException());
  auto tree = converted.value();
  ASSERT_NE(nullptr, tree);
  EXPECT_TRUE(tree->isRestricted());
  EXPECT_EQ(id, tree->getObjectId());
}

TEST_F(SaplingBackingStoreWithFaultInjectorTest, getTree) {
  auto tree1 = folly::coro::blockingWait(
      folly::coro::timeout(
          queuedBackingStore->co_getRootTree(
              commit1, ObjectFetchContext::getNullContext()),
          kTestTimeout));

  auto [tree2, origin2] =
      queuedBackingStore
          ->getTree(tree1.treeId, ObjectFetchContext::getNullContext())
          .get(kTestTimeout);

  EXPECT_TRUE(*tree1.tree == *tree2);
}

TEST_F(SaplingBackingStoreNoFaultInjectorTest, getBlob) {
  auto tree = folly::coro::blockingWait(
      folly::coro::timeout(
          queuedBackingStore->co_getRootTree(
              commit1, ObjectFetchContext::getNullContext()),
          kTestTimeout));

  for (auto& [name, entry] : *tree.tree) {
    if (entry.isTree()) {
      continue;
    }
    if (name == "foo.txt") {
      auto [blob, origin] =
          queuedBackingStore
              ->getBlob(
                  entry.getObjectId(), ObjectFetchContext::getNullContext())
              .get(kTestTimeout);

      EXPECT_EQ(blob->getContents().cloneAsValue().moveToFbString(), "foo\n");
    } else if (name == "bar.txt") {
      auto [blob, origin] =
          queuedBackingStore
              ->getBlob(
                  entry.getObjectId(), ObjectFetchContext::getNullContext())
              .get(kTestTimeout);

      EXPECT_EQ(blob->getContents().cloneAsValue().moveToFbString(), "bar\n");
    }
  }
}

TEST_F(SaplingBackingStoreWithFaultInjectorTest, getBlob) {
  auto tree = folly::coro::blockingWait(
      folly::coro::timeout(
          queuedBackingStore->co_getRootTree(
              commit1, ObjectFetchContext::getNullContext()),
          kTestTimeout));

  for (auto& [name, entry] : *tree.tree) {
    if (entry.isTree()) {
      continue;
    }
    if (name == "foo.txt") {
      auto [blob, origin] =
          queuedBackingStore
              ->getBlob(
                  entry.getObjectId(), ObjectFetchContext::getNullContext())
              .get(kTestTimeout);

      EXPECT_EQ(blob->getContents().cloneAsValue().moveToFbString(), "foo\n");
    } else if (name == "bar.txt") {
      auto [blob, origin] =
          queuedBackingStore
              ->getBlob(
                  entry.getObjectId(), ObjectFetchContext::getNullContext())
              .get(kTestTimeout);

      EXPECT_EQ(blob->getContents().cloneAsValue().moveToFbString(), "bar\n");
    }
  }
}

TEST_F(
    SaplingBackingStoreWithFaultInjectorTest,
    blobFetchCheckpointsAreReachable) {
  const auto rootTree = folly::coro::blockingWait(
      folly::coro::timeout(
          queuedBackingStore->co_getRootTree(
              commit1, ObjectFetchContext::getNullContext()),
          kTestTimeout));

  std::optional<SlOid> blobOid;
  for (const auto& [name, entry] : *rootTree.tree) {
    if (name == "foo.txt" && !entry.isTree()) {
      blobOid.emplace(entry.getObjectId());
      break;
    }
  }
  ASSERT_TRUE(blobOid.has_value());
  const auto key = blobOid->node().toString();

  faultInjector.injectBlock(kBlobFetchFaultPoints[0], key, 1);
  auto future = queuedBackingStore->getBlobEnqueue(
      *blobOid,
      ObjectFetchContext::getNullContext(),
      SaplingImportRequest::FetchType::Fetch);
  ASSERT_TRUE(
      faultInjector.waitUntilBlocked(kBlobFetchFaultPoints[0], kTestTimeout));
  EXPECT_EQ(
      1,
      faultInjector.unblockWithError(
          kBlobFetchFaultPoints[0],
          key,
          std::runtime_error("future blob checkpoint reached")));
  expectRuntimeError(std::move(future), "future blob checkpoint reached");

  faultInjector.injectBlock(kBlobFetchFaultPoints[1], key, 1);
  folly::CPUThreadPoolExecutor pool{1};
  auto coroFuture =
      folly::coro::co_withExecutor(
          &pool,
          folly::coro::co_invoke(
              [&]() -> folly::coro::Task<BackingStore::GetBlobResult> {
                co_return co_await queuedBackingStore->co_getBlobEnqueue(
                    *blobOid,
                    ObjectFetchContext::getNullContext(),
                    SaplingImportRequest::FetchType::Fetch);
              }))
          .start();
  auto cleanup = folly::makeGuard([&] {
    faultInjector.removeFault(kBlobFetchFaultPoints[1], key);
    faultInjector.unblockWithError(
        kBlobFetchFaultPoints[1],
        key,
        folly::make_exception_wrapper<std::runtime_error>("test cleanup"));
    EXPECT_TRUE(std::move(coroFuture).wait(kTestTimeout))
        << "co_getBlobEnqueue did not finish after cleanup";
  });

  ASSERT_TRUE(
      faultInjector.waitUntilBlocked(kBlobFetchFaultPoints[1], kTestTimeout));
  EXPECT_EQ(
      1,
      faultInjector.unblockWithError(
          kBlobFetchFaultPoints[1],
          key,
          std::runtime_error("coroutine blob checkpoint reached")));
  expectRuntimeError(
      std::move(coroFuture), "coroutine blob checkpoint reached");
  cleanup.dismiss();
}

// Duplicate requests with same nodeID in one request batch will crash Eden with
// `folly::PromiseAlreadySatisfied` exception. It caused S463588 in the past.
// More info on the investication doc
// https://fburl.com/gdoc/7mgp3fhe
// This test is to make sure we don't have duplicate requests in one batch.
TEST_F(
    SaplingBackingStoreNoFaultInjectorTest,
    sameRequestsDifferentFetchCause) {
  auto treeId =
      SlOid{
          queuedBackingStore
              ->getManifestNode(ObjectId::fromHex(commit1.value()))
              .value(),
          RelativePathPiece{}}
          .oid();

  SlOid proxyHash = SlOid{treeId};

  auto fsRequestContext = ObjectFetchContext::getNullFsContext();
  auto prefetchRequestContext = ObjectFetchContext::getNullPrefetchContext();
  auto fsRequest =
      SaplingImportRequest::makeTreeImportRequest(proxyHash, fsRequestContext);
  auto prefetchRequest = SaplingImportRequest::makeTreeImportRequest(
      proxyHash, prefetchRequestContext);

  auto executor = std::make_shared<folly::CPUThreadPoolExecutor>(1);
  auto treeFuture = via(executor.get(), [&]() {
    queuedBackingStore->getTreeBatch(
        std::vector{fsRequest, prefetchRequest}, sapling::FetchMode::LocalOnly);
  });

  std::move(treeFuture).get();
  auto tree1 = fsRequest->getPromise<TreePtr>()->getFuture().get();

  ASSERT_THAT(
      getTreeNames(tree1),
      ::testing::ElementsAre(PathComponent{"foo"}, PathComponent{"src"}));
}

TEST_F(SaplingBackingStoreWithFaultInjectorIgnoreConfigTest, getTreeBatch) {
  // force a reload
  updateTestEdenConfig(
      testConfigSource,
      edenConfig,
      {
          {"hg:filtered-paths", "['foo']"},
      });

  auto tree1Id =
      SlOid{
          queuedBackingStore
              ->getManifestNode(ObjectId::fromHex(commit1.value()))
              .value(),
          RelativePathPiece{}}
          .oid();

  SlOid proxyHash = SlOid{tree1Id};

  auto requestContext = ObjectFetchContext::getNullContext();
  auto request =
      SaplingImportRequest::makeTreeImportRequest(proxyHash, requestContext);

  auto executor = std::make_shared<folly::CPUThreadPoolExecutor>(1);
  auto tree1fut = via(executor.get(), [&]() {
    queuedBackingStore->getTreeBatch(
        std::vector{request}, sapling::FetchMode::LocalOnly);
  });

  std::move(tree1fut).get();
  auto tree1 = request->getPromise<TreePtr>()->getFuture().get();

  ASSERT_THAT(
      getTreeNames(tree1),
      ::testing::ElementsAre(PathComponent{"foo"}, PathComponent{"src"}));
}

TEST_F(SaplingBackingStoreWithFaultInjectorTest, getTreeBatch) {
  {
    updateTestEdenConfig(
        testConfigSource,
        edenConfig,
        {
            {"hg:filtered-paths", "['a/b', 'c/d']"},
        });
  }
  faultInjector.injectBlock("SaplingBackingStore::getTreeBatch", ".*");
  auto tree1Id =
      SlOid{
          queuedBackingStore
              ->getManifestNode(ObjectId::fromHex(commit1.value()))
              .value(),
          RelativePathPiece{}}
          .oid();

  SlOid proxyHash = SlOid{tree1Id};

  auto requestContext = ObjectFetchContext::getNullContext();
  auto request =
      SaplingImportRequest::makeTreeImportRequest(proxyHash, requestContext);

  auto executor = std::make_shared<folly::CPUThreadPoolExecutor>(1);
  auto tree1fut = via(executor.get(), [&]() {
    // this will block until we unblock the fault.
    queuedBackingStore->getTreeBatch(
        std::vector{request}, sapling::FetchMode::LocalOnly);
  });

  // TODO: We should rewrite SaplingBackingStore with futures so that this is
  // more testable: T171328733.
  ASSERT_TRUE(
      faultInjector.waitUntilBlocked("SaplingBackingStore::getTreeBatch", 10s));

  // force a reload
  updateTestEdenConfig(
      testConfigSource,
      edenConfig,
      {
          {"hg:filtered-paths", "['e/f', 'g/h']"},
      });

  faultInjector.removeFault("SaplingBackingStore::getTreeBatch", ".*");
  ASSERT_EQ(
      faultInjector.unblock("SaplingBackingStore::getTreeBatch", ".*"), 1);

  std::move(tree1fut).get(10s);
  auto tree1 = request->getPromise<TreePtr>()->getFuture().get(10s);

  ASSERT_THAT(
      getTreeNames(tree1),
      ::testing::ElementsAre(PathComponent{"foo"}, PathComponent{"src"}));
}

TEST_F(SaplingBackingStoreNoFaultInjectorTest, prefetchBlobsWithDuplicates) {
  auto tree = folly::coro::blockingWait(
      folly::coro::timeout(
          queuedBackingStore->co_getRootTree(
              commit1, ObjectFetchContext::getNullContext()),
          kTestTimeout));

  std::vector<ObjectId> blobIds;
  for (auto& [name, entry] : *tree.tree) {
    if (!entry.isTree()) {
      blobIds.push_back(entry.getObjectId());
      blobIds.push_back(entry.getObjectId());
    }
  }

  ASSERT_FALSE(blobIds.empty());

  auto prefetchResult =
      queuedBackingStore
          ->prefetchBlobs(
              folly::range(blobIds), ObjectFetchContext::getNullContext())
          .get(kTestTimeout);

  EXPECT_EQ(prefetchResult, folly::unit);
}

TEST_F(
    SaplingBackingStoreNoFaultInjectorTest,
    prefetchBlobsWithDuplicatesResolvesAllCallbacks) {
  auto tree = folly::coro::blockingWait(
      folly::coro::timeout(
          queuedBackingStore->co_getRootTree(
              commit1, ObjectFetchContext::getNullContext()),
          kTestTimeout));

  ObjectId firstBlobId;
  for (auto& [name, entry] : *tree.tree) {
    if (!entry.isTree()) {
      firstBlobId = entry.getObjectId();
      break;
    }
  }

  ASSERT_NE(firstBlobId.size(), 0);

  auto proxyHash = SlOid{firstBlobId};

  std::vector<sapling::SaplingRequest> requests;
  requests.reserve(3);
  for (size_t i = 0; i < 3; ++i) {
    requests.emplace_back(
        proxyHash,
        ObjectFetchContext::Cause::Unknown,
        ObjectFetchContext::getNullContext().copy());
  }

  std::vector<bool> callbackInvoked(3, false);
  std::atomic<size_t> callbackCount{0};

  // Call SaplingBackingStore::nativeGetBlobBatch() directly.
  queuedBackingStore->nativeGetBlobBatch(
      folly::range(requests),
      sapling::FetchMode::AllowRemote,
      true,
      [&](size_t index, folly::Try<std::unique_ptr<folly::IOBuf>> content) {
        ASSERT_LT(index, 3) << "Callback index out of range: " << index;
        callbackInvoked[index] = true;
        callbackCount++;
        if (content.hasException()) {
          ADD_FAILURE() << "Callback " << index << " received exception: "
                        << content.exception().what();
        }
      });

  EXPECT_EQ(callbackCount.load(), 3)
      << "Expected all 3 callbacks to be invoked for duplicate blob IDs";

  for (size_t i = 0; i < callbackInvoked.size(); ++i) {
    EXPECT_TRUE(callbackInvoked[i])
        << "Callback for duplicate blob at index " << i << " was not invoked.";
  }
}

TEST(SaplingBackingStoreObjectId, round_trip_object_IDs) {
  Hash20 testId{folly::StringPiece{"0123456789abcdef0123456789abcdef01234567"}};

  {
    ObjectId with_path = SlOid{testId, RelativePathPiece{"foo/bar/baz"}}.oid();
    EXPECT_EQ(
        "0123456789abcdef0123456789abcdef01234567:foo/bar/baz",
        SaplingBackingStore::staticRenderObjectId(with_path));

    EXPECT_EQ(
        with_path,
        SaplingBackingStore::staticParseObjectId(
            SaplingBackingStore::staticRenderObjectId(with_path)));
  }

  {
    ObjectId id_only = SlOid{testId}.oid();
    EXPECT_EQ(
        "0123456789abcdef0123456789abcdef01234567",
        SaplingBackingStore::staticRenderObjectId(id_only));

    EXPECT_EQ(
        id_only,
        SaplingBackingStore::staticParseObjectId(
            SaplingBackingStore::staticRenderObjectId(id_only)));
  }
}

TEST_F(SaplingBackingStoreNoFaultInjectorTest, testCompareRootsById) {
  // In Sapling/Mercurial, RootIds are commit hashes which are bijective.
  // Create two different commit hashes
  auto rootId1 = RootId{"1234567890abcdef1234567890abcdef12345678"};
  auto rootId2 = RootId{"fedcba0987654321fedcba0987654321fedcba09"};
  auto rootId3 = RootId{"1234567890abcdef1234567890abcdef12345678"};

  // Same RootId should be identical
  EXPECT_EQ(
      queuedBackingStore->compareRootsById(rootId1, rootId1),
      ObjectComparison::Identical);

  // Same commit hash should be identical
  EXPECT_EQ(
      queuedBackingStore->compareRootsById(rootId1, rootId3),
      ObjectComparison::Identical);

  // Different commit hashes should be different
  EXPECT_EQ(
      queuedBackingStore->compareRootsById(rootId1, rootId2),
      ObjectComparison::Different);
  EXPECT_EQ(
      queuedBackingStore->compareRootsById(rootId2, rootId1),
      ObjectComparison::Different);
}

CO_TEST_F(
    SaplingBackingStoreWithFaultInjectorTest,
    coGetRootTreeFaultInjection) {
  // Coroutine variant of getRootTreeFutureChainCanBePausedAndResumed.
  //
  // This test deterministically reproduces the shutdown race for the
  // coroutine implementation of co_getRootTree. We use fault injection to
  // pause the coroutine mid-execution, then destroy the backing store while
  // it's suspended. The coroutine's lambda must capture a shared_ptr (not
  // a raw pointer) to keep the object alive across the suspension point.
  //
  // Pattern: CO_TEST_F + collectAll for concurrent coroutine testing.
  // co_getRootTree is a now_task (lazy, inline), so we can't co_await it
  // directly and also interact with it while suspended. Instead we use
  // collectAll to run two tasks concurrently on the CO_TEST_F's executor:
  //   1. getRootTreeTask — calls co_getRootTree, suspends at fault injection
  //   2. lifetimeCheckTask — verifies the object is alive, then unblocks
  auto weak = std::weak_ptr<SaplingBackingStore>(queuedBackingStore);

  faultInjector.injectBlock("SaplingBackingStore::getRootTree", ".*");

  // Task 1: Wraps co_getRootTree (a now_task) in a Task via co_invoke.
  // No shared_ptr capture here — co_getRootTree internally captures
  // shared_from_this(), so the caller doesn't need to manage lifetime.
  // This mirrors how the futures test calls getRootTree() directly.
  auto getRootTreeTask = folly::coro::co_invoke(
      [&]() -> folly::coro::Task<BackingStore::GetRootTreeResult> {
        co_return co_await queuedBackingStore->co_getRootTree(
            commit1, ObjectFetchContext::getNullContext());
      });

  // Task 2: Runs after task 1 suspends at the fault injection point.
  // Verifies co_getRootTree's internal shared_from_this() keeps the
  // object alive, then unblocks so task 1 can complete.
  auto lifetimeCheckTask =
      folly::coro::co_invoke([&]() -> folly::coro::Task<void> {
        // Yield to let task 1 start and suspend at the fault injection point.
        // After this, task 1's co_checkAsync has registered as blocked.
        co_await folly::coro::co_reschedule_on_current_executor;

        // Confirm task 1 is suspended at the fault injection point.
        EXPECT_TRUE(faultInjector.waitUntilBlocked(
            "SaplingBackingStore::getRootTree", 0ms));

        // Drop the test fixture's shared_ptr — the only external reference.
        // co_getRootTree internally captured shared_from_this(), so the
        // object stays alive. If co_getRootTree used raw `this` instead,
        // the object would be destroyed here and this assertion would fail.
        queuedBackingStore.reset();
        EXPECT_FALSE(weak.expired());

        // Unblock the fault so task 1 can resume and complete.
        faultInjector.unblock("SaplingBackingStore::getRootTree", ".*");
      });

  // Run both tasks concurrently. collectAll requires Task<T> (not now_task),
  // which is why we wrapped co_getRootTree with co_invoke above.
  auto [result, _] = co_await folly::coro::collectAll(
      std::move(getRootTreeTask), std::move(lifetimeCheckTask));

  // The coroutine completed successfully — the tree was fetched.
  EXPECT_NE(result.tree, nullptr);
}

TEST_F(
    SaplingBackingStoreWithFaultInjectorTest,
    getTreeEnqueueFutureChainCanBePausedAndResumed) {
  // This test verifies that getTreeEnqueue captures shared_from_this() instead
  // of raw `this`, keeping the object alive while futures are in-flight. If
  // someone reverts to raw `this`, the weak_ptr would expire after reset() and
  // the continuation would access freed memory.

  auto rootTree = folly::coro::blockingWait(
      folly::coro::timeout(
          queuedBackingStore->co_getRootTree(
              commit1, ObjectFetchContext::getNullContext()),
          kTestTimeout));
  SlOid treeOid{rootTree.treeId};

  auto weak = std::weak_ptr<SaplingBackingStore>(queuedBackingStore);

  faultInjector.injectBlock("SaplingBackingStore::getTreeEnqueue", ".*");

  auto future = queuedBackingStore->getTreeEnqueue(
      treeOid, ObjectFetchContext::getNullContext());

  EXPECT_FALSE(future.isReady());

  // Drop the test fixture's shared_ptr. The lambdas in the future chain hold
  // shared_ptr copies via shared_from_this(), keeping the object alive.
  queuedBackingStore.reset();

  // The object is still alive because the lambdas captured
  // shared_from_this(). If someone reverts to raw `this`, this fails
  // because the object was destroyed by reset() above.
  EXPECT_FALSE(weak.expired());

  faultInjector.unblock("SaplingBackingStore::getTreeEnqueue", ".*");

  // The future should complete without crashing.
  std::move(future).getTry(kTestTimeout);
}

TEST_F(
    SaplingBackingStoreWithFaultInjectorTest,
    coGetTreeEnqueueCoroutineKeepsObjectAlive) {
  // Verify that co_getTreeEnqueue captures shared_from_this(), keeping the
  // object alive while the coroutine is suspended. Removing the
  // shared_from_this() capture would cause a use-after-free on resumption.

  // Get a valid tree ObjectId from the repo so we can construct a real SlOid.
  auto rootTree = folly::coro::blockingWait(
      folly::coro::timeout(
          queuedBackingStore->co_getRootTree(
              commit1, ObjectFetchContext::getNullContext()),
          kTestTimeout));
  SlOid treeOid{rootTree.treeId};

  auto baselineUseCount = queuedBackingStore.use_count();

  faultInjector.injectBlock("SaplingBackingStore::co_getTreeEnqueue", ".*");

  // Use CPUThreadPoolExecutor — InlineExecutor is forbidden for coro::Task
  // (DCHECK in debug builds at Task.h:470).
  folly::CPUThreadPoolExecutor pool(1);
  auto future =
      folly::coro::co_withExecutor(
          &pool,
          folly::coro::co_invoke(
              [&]() -> folly::coro::Task<BackingStore::GetTreeResult> {
                co_return co_await queuedBackingStore->co_getTreeEnqueue(
                    treeOid, ObjectFetchContext::getNullContext());
              }))
          .start();

  // Ensure the coroutine is unblocked before test exit to prevent deadlock
  // in the CPUThreadPoolExecutor destructor.
  SCOPE_EXIT {
    faultInjector.removeFault("SaplingBackingStore::co_getTreeEnqueue", ".*");
    faultInjector.unblockWithError(
        "SaplingBackingStore::co_getTreeEnqueue",
        ".*",
        folly::make_exception_wrapper<std::runtime_error>("test cleanup"));
    EXPECT_TRUE(std::move(future).wait(kTestTimeout))
        << "co_getTreeEnqueue did not finish after cleanup";
  };

  ASSERT_TRUE(faultInjector.waitUntilBlocked(
      "SaplingBackingStore::co_getTreeEnqueue",
      std::chrono::milliseconds(5000)));

  EXPECT_FALSE(future.isReady());

  // The coroutine captures shared_from_this(), so the reference count should
  // increase while the coroutine is suspended. If someone removes the
  // shared_from_this() capture, this assertion will fail, catching a
  // use-after-free regression.
  EXPECT_GT(queuedBackingStore.use_count(), baselineUseCount);
}

struct SaplingBackingStoreErrorLoggingTest : SaplingBackingStoreTestBase {
  FaultInjector faultInjector{/*enabled=*/false};
  folly::InlineExecutor executor = folly::InlineExecutor::instance();
  CapturingXplatLogger xplatLogger;
  std::shared_ptr<ReloadableConfig> errorLoggerConfig{
      std::make_shared<ReloadableConfig>(testEdenConfig)};
  ErrorLogger errorLogger{errorLoggerConfig, &xplatLogger};

  SaplingBackingStoreErrorLoggingTest() {
    testEdenConfig->enableErrorLogging.setValue(
        true, ConfigSourceType::UserConfig);
  }

  std::shared_ptr<SaplingBackingStore> queuedBackingStore =
      std::make_shared<SaplingBackingStore>(
          repo.path(),
          kPathMapDefaultCaseSensitive,
          stats.copy(),
          &executor,
          edenConfig,
          std::make_unique<SaplingBackingStoreOptions>(),
          makeTestEdenFsEventsLogger(),
          /*errorLogger=*/errorLogger,
          std::make_unique<BackingStoreLogger>(),
          &faultInjector);
};

TEST_F(SaplingBackingStoreErrorLoggingTest, manifestResolutionFailureIsLogged) {
  auto bogusCommit =
      ObjectId::fromHex("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
  auto result = queuedBackingStore->getManifestNode(bogusCommit);
  EXPECT_FALSE(result.has_value());

  ASSERT_EQ(xplatLogger.events().size(), 1);
  const auto& strings = xplatLogger.events()[0].event.getStringMap();
  EXPECT_EQ(strings.at("component"), "backing_store");
  EXPECT_EQ(strings.at("error_type"), "manifest_resolution_failure");
}

TEST_F(
    SaplingBackingStoreNoFaultInjectorTest,
    dropAllPendingRequestsWithAuxRequestsQueued) {
  queuedBackingStore->queue_.stop();
  for (auto& thread : queuedBackingStore->threads_) {
    thread.join();
  }
  queuedBackingStore->threads_.clear();
  auto context = ObjectFetchContext::getNullContext();
  SlOid blobOid{
      Hash20{folly::StringPiece{"1111111111111111111111111111111111111111"}},
      RelativePathPiece{"lean/check/blobOid"}};
  SlOid treeOid{
      Hash20{folly::StringPiece{"2222222222222222222222222222222222222222"}},
      RelativePathPiece{"lean/check/treeOid"}};
  SlOid blobAuxOid{
      Hash20{folly::StringPiece{"3333333333333333333333333333333333333333"}},
      RelativePathPiece{"lean/check/blobAuxOid"}};
  SlOid treeAuxOid{
      Hash20{folly::StringPiece{"4444444444444444444444444444444444444444"}},
      RelativePathPiece{"lean/check/treeAuxOid"}};

  auto blobFuture = queuedBackingStore->getBlobEnqueue(
      blobOid, context, SaplingImportRequest::FetchType::Fetch);
  auto blobDuplicateFuture = queuedBackingStore->getBlobEnqueue(
      blobOid, context, SaplingImportRequest::FetchType::Fetch);
  auto treeFuture = queuedBackingStore->queue_.enqueueTree(
      SaplingImportRequest::makeTreeImportRequest(treeOid, context));
  auto treeAuxFuture =
      queuedBackingStore->getTreeAuxDataEnqueue(treeAuxOid, context);
  auto blobAuxFuture = queuedBackingStore->queue_.enqueueBlobAux(
      SaplingImportRequest::makeBlobAuxImportRequest(blobAuxOid, context));

  EXPECT_EQ(4, queuedBackingStore->dropAllPendingRequestsFromQueue());

  EXPECT_EQ(
      "Request forcibly dropped",
      completionError(std::move(blobFuture), kTestTimeout));
  EXPECT_EQ(
      "Request forcibly dropped",
      completionError(std::move(blobDuplicateFuture), kTestTimeout));
  EXPECT_EQ(
      "Request forcibly dropped",
      completionError(std::move(treeFuture), kTestTimeout));
  EXPECT_EQ(
      "Request forcibly dropped",
      completionError(std::move(treeAuxFuture), kTestTimeout));
  EXPECT_EQ(
      "Request forcibly dropped",
      completionError(std::move(blobAuxFuture), kTestTimeout));
}

TEST_F(
    SaplingBackingStoreNoFaultInjectorTest,
    dropAllPendingRequestsThenRefetchAuxData) {
  queuedBackingStore->queue_.stop();
  for (auto& thread : queuedBackingStore->threads_) {
    thread.join();
  }
  queuedBackingStore->threads_.clear();
  auto context = ObjectFetchContext::getNullContext();
  SlOid blobOid{
      Hash20{folly::StringPiece{"5555555555555555555555555555555555555555"}},
      RelativePathPiece{"lean/check/blobOid"}};
  SlOid treeAuxOid{
      Hash20{folly::StringPiece{"6666666666666666666666666666666666666666"}},
      RelativePathPiece{"lean/check/treeAuxOid"}};

  auto firstBlobFuture = queuedBackingStore->getBlobEnqueue(
      blobOid, context, SaplingImportRequest::FetchType::Fetch);
  EXPECT_EQ(1, queuedBackingStore->dropAllPendingRequestsFromQueue());
  EXPECT_EQ(
      "Request forcibly dropped",
      completionError(std::move(firstBlobFuture), kTestTimeout));
  auto secondBlobFuture = queuedBackingStore->getBlobEnqueue(
      blobOid, context, SaplingImportRequest::FetchType::Fetch);
  EXPECT_EQ(1, queuedBackingStore->dropAllPendingRequestsFromQueue());
  EXPECT_EQ(
      "Request forcibly dropped",
      completionError(std::move(secondBlobFuture), kTestTimeout));

  auto firstTreeAuxFuture =
      queuedBackingStore->getTreeAuxDataEnqueue(treeAuxOid, context);
  EXPECT_EQ(1, queuedBackingStore->dropAllPendingRequestsFromQueue());
  EXPECT_EQ(
      "Request forcibly dropped",
      completionError(std::move(firstTreeAuxFuture), kTestTimeout));
  auto secondTreeAuxFuture =
      queuedBackingStore->getTreeAuxDataEnqueue(treeAuxOid, context);
  EXPECT_EQ(1, queuedBackingStore->dropAllPendingRequestsFromQueue());
  EXPECT_EQ(
      "Request forcibly dropped",
      completionError(std::move(secondTreeAuxFuture), kTestTimeout));
}

} // namespace facebook::eden
