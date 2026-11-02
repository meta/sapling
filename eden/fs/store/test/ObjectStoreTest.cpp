/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include <folly/executors/GlobalExecutor.h>
#include <folly/test/TestUtils.h>
#include <gtest/gtest.h>
#include <re2/re2.h>

#include <array>
#include <stdexcept>

#include <fb303/ServiceData.h>
#include <folly/coro/GtestHelpers.h>

#include "eden/common/utils/ImmediateFuture.h"
#include "eden/common/utils/ProcessInfoCache.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/config/ReloadableConfig.h"
#include "eden/fs/model/TestOps.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/store/ObjectStore.h"
#include "eden/fs/store/TreeCache.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/testharness/FakeBackingStore.h"
#include "eden/fs/testharness/LoggingFetchContext.h"
#include "eden/fs/testharness/StoredObject.h"

using namespace folly::string_piece_literals;
using namespace std::chrono_literals;

namespace facebook::eden {
namespace {

constexpr size_t kTreeCacheMaximumSize = 1000; // bytes
constexpr size_t kTreeCacheMinimumEntries = 0;
constexpr folly::StringPiece kBlake3Key = "19700101-1111111111111111111111#";

std::shared_ptr<EdenFsEventsLogger> makeTestEdenFsEventsLogger() {
  return std::make_shared<EdenFsEventsLogger>(nullptr);
}

struct ObjectStoreTest : public ::testing::TestWithParam<CaseSensitivity> {
  using TreeAuxStats = std::array<int64_t, 3>;

  void SetUp() override {
    rawEdenConfig = EdenConfig::createTestEdenConfig();
    rawEdenConfig->inMemoryTreeCacheSize.setValue(
        kTreeCacheMaximumSize, ConfigSourceType::Default, true);
    rawEdenConfig->inMemoryTreeCacheMinimumItems.setValue(
        kTreeCacheMinimumEntries, ConfigSourceType::Default, true);
    // The tests rely on deterministic caching of small trees, so use a single
    // shard to avoid the tiny per-shard capacity evicting entries immediately.
    rawEdenConfig->treeCacheShards.setValue(1, ConfigSourceType::Default, true);
    auto edenConfig = std::make_shared<ReloadableConfig>(rawEdenConfig);
    stats = makeRefPtr<EdenStats>();
    treeCache = TreeCache::create(edenConfig, stats.copy());
    fakeBackingStore = std::make_shared<FakeBackingStore>();
    objectStore = ObjectStore::create(
        fakeBackingStore,
        treeCache,
        stats.copy(),
        std::make_shared<ProcessInfoCache>(),
        makeTestEdenFsEventsLogger(),
        std::make_shared<ReloadableConfig>(EdenConfig::createTestEdenConfig()),
        GetParam(),
        folly::getGlobalCPUExecutor());

    auto configWithBlake3Key = EdenConfig::createTestEdenConfig();
    configWithBlake3Key->blake3Key.setStringValue(
        kBlake3Key, ConfigVariables(), ConfigSourceType::UserConfig);
    fakeBackingStoreWithKeyedBlake3 =
        std::make_shared<FakeBackingStore>(nullptr, std::string(kBlake3Key));
    objectStoreWithBlake3Key = ObjectStore::create(
        fakeBackingStoreWithKeyedBlake3,
        treeCache,
        stats.copy(),
        std::make_shared<ProcessInfoCache>(),
        makeTestEdenFsEventsLogger(),
        std::make_shared<ReloadableConfig>(configWithBlake3Key),
        GetParam(),
        folly::getGlobalCPUExecutor());

    auto configWithTreeAuxPrefetching = EdenConfig::createTestEdenConfig();
    fakeBackingStoreWithTreeAuxPrefetching =
        std::make_shared<FakeBackingStore>();
    objectStoreWithTreeAuxPrefetching = ObjectStore::create(
        fakeBackingStoreWithTreeAuxPrefetching,
        treeCache,
        stats.copy(),
        std::make_shared<ProcessInfoCache>(),
        makeTestEdenFsEventsLogger(),
        std::make_shared<ReloadableConfig>(configWithTreeAuxPrefetching),
        GetParam(),
        folly::getGlobalCPUExecutor());

    readyBlobId = putReadyBlob("readyblob");
    readyTreeId = putReadyTree();
  }

  ObjectId putReadyBlob(folly::StringPiece data) {
    {
      auto [storedBlob, id] = fakeBackingStoreWithKeyedBlake3->putBlob(data);
      storedBlob->setReady();
    }

    {
      auto [storedBlob, id] = fakeBackingStore->putBlob(data);
      storedBlob->setReady();
    }

    auto [storedBlob, id] =
        fakeBackingStoreWithTreeAuxPrefetching->putBlob(data);
    storedBlob->setReady();
    return id;
  }

  ObjectId putReadyTree() {
    {
      auto* storedBlob = fakeBackingStoreWithKeyedBlake3->putTree({});
      storedBlob->setReady();
    }

    {
      StoredTree* storedTree = fakeBackingStore->putTree({});
      storedTree->setReady();
    }

    StoredTree* storedTree =
        fakeBackingStoreWithTreeAuxPrefetching->putTree({});
    storedTree->setReady();

    return storedTree->get().getObjectId();
  }

  ObjectId putReadyTree(const Tree::container& entries) {
    {
      auto* storedBlob = fakeBackingStoreWithKeyedBlake3->putTree(entries);
      storedBlob->setReady();
    }

    {
      StoredTree* storedTree = fakeBackingStore->putTree(entries);
      storedTree->setReady();
    }

    StoredTree* storedTree =
        fakeBackingStoreWithTreeAuxPrefetching->putTree(entries);
    storedTree->setReady();
    return storedTree->get().getObjectId();
  }

  CaseSensitivity getOppositeCaseSensitivity() const {
    return GetParam() == CaseSensitivity::Sensitive
        ? CaseSensitivity::Insensitive
        : CaseSensitivity::Sensitive;
  }

  TreeAuxStats getTreeAuxStats() {
    stats->flush();
    auto* serviceData = facebook::fb303::ServiceData::get();
    return {
        serviceData
            ->getCounterIfExists("object_store.get_tree_metadata.memory.sum.60")
            .value_or(0),
        serviceData
            ->getCounterIfExists(
                "object_store.get_tree_metadata.backing_store.sum.60")
            .value_or(0),
        serviceData
            ->getCounterIfExists("object_store.get_tree_metadata_failed.sum.60")
            .value_or(0),
    };
  }

  TreeAuxStats getTreeAuxStatsDelta(const TreeAuxStats& before) {
    auto after = getTreeAuxStats();
    return {
        after[0] - before[0],
        after[1] - before[1],
        after[2] - before[2],
    };
  }

  static void expectTreeAuxData(
      const TreeAuxData& expected,
      const std::optional<TreeAuxData>& actual) {
    ASSERT_TRUE(actual.has_value());
    EXPECT_EQ(expected.digestHash, actual->digestHash);
    EXPECT_EQ(expected.digestSize, actual->digestSize);
  }

  static void expectTreeAuxFetch(
      const LoggingFetchContext& context,
      size_t index,
      const ObjectId& expectedId,
      ObjectFetchContext::Origin expectedOrigin) {
    ASSERT_GT(context.requests.size(), index);
    const auto& request = context.requests[index];
    EXPECT_EQ(ObjectFetchContext::TreeAuxData, request.type);
    EXPECT_EQ(expectedId, request.id);
    EXPECT_EQ(expectedOrigin, request.origin);
  }

  RefPtr<LoggingFetchContext> loggingContext =
      makeRefPtr<LoggingFetchContext>();
  const ObjectFetchContextPtr& context =
      loggingContext.as<ObjectFetchContext>();
  std::shared_ptr<FakeBackingStore> fakeBackingStore;
  std::shared_ptr<FakeBackingStore> fakeBackingStoreWithKeyedBlake3;
  std::shared_ptr<FakeBackingStore> fakeBackingStoreWithTreeAuxPrefetching;
  std::shared_ptr<BackingStore> backingStoreWithKeyedBlake3;
  std::shared_ptr<EdenConfig> rawEdenConfig;
  std::shared_ptr<TreeCache> treeCache;
  EdenStatsPtr stats;
  std::shared_ptr<ObjectStore> objectStore;
  std::shared_ptr<ObjectStore> objectStoreWithBlake3Key;
  std::shared_ptr<ObjectStore> objectStoreWithTreeAuxPrefetching;

  ObjectId readyBlobId;
  ObjectId readyTreeId;
};

class ThrowingWalkDetector final : public WalkDetector {
 public:
  void witnessFileRead(
      const ObjectId&,
      ObjectFetchContext::Origin,
      OptionalProcessId,
      std::optional<ObjectFetchContext::DirEntryCounts>) override {
    throw std::runtime_error("walk detector failure");
  }
  void witnessTreeRead(
      const ObjectId&,
      const Tree&,
      ObjectFetchContext::Origin,
      OptionalProcessId) override {
    throw std::runtime_error("walk detector failure");
  }
  void witnessDirectorySize(
      RelativePathPiece,
      ObjectFetchContext::DirEntryCounts) override {}
  void workingCopyParentHint(const RootId&) override {}
};

} // namespace

TEST_P(ObjectStoreTest, fetches_survive_a_throwing_walk_detector) {
  auto store = ObjectStore::create(
      fakeBackingStore,
      treeCache,
      stats.copy(),
      std::make_shared<ProcessInfoCache>(),
      makeTestEdenFsEventsLogger(),
      std::make_shared<ReloadableConfig>(EdenConfig::createTestEdenConfig()),
      GetParam(),
      folly::getGlobalCPUExecutor(),
      std::make_shared<ThrowingWalkDetector>());
  EXPECT_NE(store->getBlob(readyBlobId, context).get(0ms), nullptr);
  EXPECT_NE(store->getTree(readyTreeId, context).get(0ms), nullptr);
}

TEST_P(ObjectStoreTest, inode_gc_fetches_are_not_witnessed) {
  auto detector = std::make_shared<FakeWalkDetector>();
  auto store = ObjectStore::create(
      fakeBackingStore,
      treeCache,
      stats.copy(),
      std::make_shared<ProcessInfoCache>(),
      makeTestEdenFsEventsLogger(),
      std::make_shared<ReloadableConfig>(EdenConfig::createTestEdenConfig()),
      GetParam(),
      folly::getGlobalCPUExecutor(),
      detector);

  auto gcContext = ObjectFetchContext::getNullInodeGcContext();
  store->getTree(readyTreeId, gcContext).get(0ms);
  store->getBlob(readyBlobId, gcContext).get(0ms);
  EXPECT_TRUE(detector->treeReads().empty());
  EXPECT_TRUE(detector->fileReads().empty());

  store->getBlob(readyBlobId, ObjectFetchContext::getNullFsContext()).get(0ms);
  EXPECT_EQ(std::vector<ObjectId>{readyBlobId}, detector->fileReads());
}

TEST_P(ObjectStoreTest, getBlob_tracks_backing_store_read) {
  objectStore->getBlob(readyBlobId, context).get(0ms);
  ASSERT_EQ(1, loggingContext->requests.size());
  auto& request = loggingContext->requests[0];
  EXPECT_EQ(ObjectFetchContext::Blob, request.type);
  EXPECT_EQ(readyBlobId, request.id);
  EXPECT_EQ(ObjectFetchContext::FromNetworkFetch, request.origin);
}

TEST_P(ObjectStoreTest, getTree_tracks_backing_store_read) {
  objectStore->getTree(readyTreeId, context).get(0ms);
  ASSERT_EQ(1, loggingContext->requests.size());
  auto& request = loggingContext->requests[0];
  EXPECT_EQ(ObjectFetchContext::Tree, request.type);
  EXPECT_EQ(readyTreeId, request.id);
  EXPECT_EQ(ObjectFetchContext::FromNetworkFetch, request.origin);
}

TEST_P(ObjectStoreTest, getTree_tracks_second_read_from_cache) {
  objectStore->getTree(readyTreeId, context).get(0ms);
  objectStore->getTree(readyTreeId, context).get(0ms);
  ASSERT_EQ(2, loggingContext->requests.size());
  auto& request = loggingContext->requests[1];
  EXPECT_EQ(ObjectFetchContext::Tree, request.type);
  EXPECT_EQ(readyTreeId, request.id);
  EXPECT_EQ(ObjectFetchContext::FromMemoryCache, request.origin);
}

CO_TEST_P(
    ObjectStoreTest,
    getTreeAuxData_adapterMatchesCoroutineForBackingStoreAndMemoryCache) {
  const ObjectId adapterId{"adapter_tree_aux"};
  const ObjectId coroutineId{"coroutine_tree_aux"};
  const TreeAuxData expected{Hash32::blake3("tree aux data"_sp), 123};
  fakeBackingStore->putTreeAuxData(
      adapterId, std::make_shared<TreeAuxData>(expected));
  fakeBackingStore->putTreeAuxData(
      coroutineId, std::make_shared<TreeAuxData>(expected));

  auto adapterContext = makeRefPtr<LoggingFetchContext>();
  auto coroutineContext = makeRefPtr<LoggingFetchContext>();

  auto before = getTreeAuxStats();
  auto adapterResult =
      co_await objectStore
          ->getTreeAuxData(adapterId, adapterContext.as<ObjectFetchContext>())
          .semi();
  EXPECT_EQ(TreeAuxStats({0, 1, 0}), getTreeAuxStatsDelta(before));

  before = getTreeAuxStats();
  auto coroutineResult = co_await objectStore->co_getTreeAuxData(
      coroutineId, coroutineContext.as<ObjectFetchContext>());
  EXPECT_EQ(TreeAuxStats({0, 1, 0}), getTreeAuxStatsDelta(before));

  expectTreeAuxData(expected, adapterResult);
  expectTreeAuxData(expected, coroutineResult);
  EXPECT_EQ(1, adapterContext->requests.size());
  EXPECT_EQ(1, coroutineContext->requests.size());
  expectTreeAuxFetch(
      *adapterContext, 0, adapterId, ObjectFetchContext::FromNetworkFetch);
  expectTreeAuxFetch(
      *coroutineContext, 0, coroutineId, ObjectFetchContext::FromNetworkFetch);

  before = getTreeAuxStats();
  adapterResult =
      co_await objectStore
          ->getTreeAuxData(adapterId, adapterContext.as<ObjectFetchContext>())
          .semi();
  EXPECT_EQ(TreeAuxStats({1, 0, 0}), getTreeAuxStatsDelta(before));

  before = getTreeAuxStats();
  coroutineResult = co_await objectStore->co_getTreeAuxData(
      coroutineId, coroutineContext.as<ObjectFetchContext>());
  EXPECT_EQ(TreeAuxStats({1, 0, 0}), getTreeAuxStatsDelta(before));

  expectTreeAuxData(expected, adapterResult);
  expectTreeAuxData(expected, coroutineResult);
  EXPECT_EQ(2, adapterContext->requests.size());
  EXPECT_EQ(2, coroutineContext->requests.size());
  expectTreeAuxFetch(
      *adapterContext, 1, adapterId, ObjectFetchContext::FromMemoryCache);
  expectTreeAuxFetch(
      *coroutineContext, 1, coroutineId, ObjectFetchContext::FromMemoryCache);
}

CO_TEST_P(ObjectStoreTest, getTreeAuxData_adapterMatchesCoroutineWhenAbsent) {
  const ObjectId adapterId{"adapter_missing_tree_aux"};
  const ObjectId coroutineId{"coroutine_missing_tree_aux"};
  fakeBackingStore->putTreeAuxData(adapterId, nullptr);
  fakeBackingStore->putTreeAuxData(coroutineId, nullptr);

  auto adapterContext = makeRefPtr<LoggingFetchContext>();
  auto coroutineContext = makeRefPtr<LoggingFetchContext>();

  auto before = getTreeAuxStats();
  auto adapterResult =
      co_await objectStore
          ->getTreeAuxData(adapterId, adapterContext.as<ObjectFetchContext>())
          .semi();
  EXPECT_EQ(TreeAuxStats({0, 0, 2}), getTreeAuxStatsDelta(before));

  before = getTreeAuxStats();
  auto coroutineResult = co_await objectStore->co_getTreeAuxData(
      coroutineId, coroutineContext.as<ObjectFetchContext>());
  EXPECT_EQ(TreeAuxStats({0, 0, 2}), getTreeAuxStatsDelta(before));

  EXPECT_FALSE(adapterResult.has_value());
  EXPECT_FALSE(coroutineResult.has_value());
  EXPECT_TRUE(adapterContext->requests.empty());
  EXPECT_TRUE(coroutineContext->requests.empty());
}

CO_TEST_P(
    ObjectStoreTest,
    getTreeAuxData_adapterMatchesCoroutineExceptionPropagation) {
  const ObjectId missingId{"missing_tree_aux"};
  auto adapterContext = makeRefPtr<LoggingFetchContext>();
  auto coroutineContext = makeRefPtr<LoggingFetchContext>();

  auto before = getTreeAuxStats();
  bool adapterCaught = false;
  std::string adapterError;
  try {
    co_await objectStore
        ->getTreeAuxData(missingId, adapterContext.as<ObjectFetchContext>())
        .semi();
  } catch (const std::domain_error& error) {
    adapterCaught = true;
    adapterError = error.what();
  }
  EXPECT_TRUE(adapterCaught);
  EXPECT_EQ(TreeAuxStats({0, 0, 1}), getTreeAuxStatsDelta(before));

  before = getTreeAuxStats();
  bool coroutineCaught = false;
  std::string coroutineError;
  try {
    co_await objectStore->co_getTreeAuxData(
        missingId, coroutineContext.as<ObjectFetchContext>());
  } catch (const std::domain_error& error) {
    coroutineCaught = true;
    coroutineError = error.what();
  }
  EXPECT_TRUE(coroutineCaught);
  EXPECT_EQ(
      "tree aux data 6d697373696e675f747265655f617578 not found", adapterError);
  EXPECT_EQ(adapterError, coroutineError);
  EXPECT_EQ(TreeAuxStats({0, 0, 1}), getTreeAuxStatsDelta(before));
  EXPECT_TRUE(adapterContext->requests.empty());
  EXPECT_TRUE(coroutineContext->requests.empty());
}

TEST_P(ObjectStoreTest, getTree_doesNotCacheRestrictedTree) {
  auto* tree =
      fakeBackingStore->putRestrictedTree(ObjectId{"restricted_tree"}, {});
  tree->setReady();
  auto treeId = tree->get().getObjectId();

  objectStore->getTree(treeId, context).get(0ms);
  objectStore->getTree(treeId, context).get(0ms);

  ASSERT_EQ(2, loggingContext->requests.size());
  EXPECT_EQ(
      ObjectFetchContext::FromNetworkFetch, loggingContext->requests[0].origin);
  EXPECT_EQ(
      ObjectFetchContext::FromNetworkFetch, loggingContext->requests[1].origin);
}

TEST_P(ObjectStoreTest, getTree_cachesTreeWithRestrictedChildWithinTtl) {
  Tree::container entries{kPathMapDefaultCaseSensitive};
  entries.emplace(
      "restricted"_pc,
      TreeEntry{
          ObjectId{"restricted_child"},
          TreeEntryType::TREE,
          /*isRestricted=*/true,
          /*hasACL=*/true});
  auto parentTreeId = putReadyTree(entries);

  objectStore->getTree(parentTreeId, context).get(0ms);
  objectStore->getTree(parentTreeId, context).get(0ms);

  ASSERT_EQ(2, loggingContext->requests.size());
  EXPECT_EQ(
      ObjectFetchContext::FromNetworkFetch, loggingContext->requests[0].origin);
  EXPECT_EQ(
      ObjectFetchContext::FromMemoryCache, loggingContext->requests[1].origin);
}

TEST_P(
    ObjectStoreTest,
    treeCache_doesNotCacheTreeWithRestrictedChildWhenTtlIsZero) {
  Tree::container entries{kPathMapDefaultCaseSensitive};
  entries.emplace(
      "restricted"_pc,
      TreeEntry{
          ObjectId{"restricted_child"},
          TreeEntryType::TREE,
          /*isRestricted=*/true,
          /*hasACL=*/true});
  auto* parentTree = fakeBackingStore->putTree(entries);
  parentTree->setReady();
  auto parentTreeId = parentTree->get().getObjectId();
  rawEdenConfig->restrictedTreeTtlSeconds.setValue(
      0, ConfigSourceType::Default, true);

  treeCache->insert(parentTreeId, parentTree->getFuture().get());

  EXPECT_EQ(nullptr, treeCache->get(parentTreeId));
}

TEST_P(ObjectStoreTest, getTree_cachesTreeWithAccessibleAclChild) {
  Tree::container entries{kPathMapDefaultCaseSensitive};
  entries.emplace(
      "acl"_pc,
      TreeEntry{
          ObjectId{"acl_child"},
          TreeEntryType::TREE,
          /*isRestricted=*/false,
          /*hasACL=*/true});
  auto parentTreeId = putReadyTree(entries);

  objectStore->getTree(parentTreeId, context).get(0ms);
  objectStore->getTree(parentTreeId, context).get(0ms);

  ASSERT_EQ(2, loggingContext->requests.size());
  EXPECT_EQ(
      ObjectFetchContext::FromNetworkFetch, loggingContext->requests[0].origin);
  EXPECT_EQ(
      ObjectFetchContext::FromMemoryCache, loggingContext->requests[1].origin);
}

TEST_P(ObjectStoreTest, getTree_prefetchBypassesTreeCache) {
  auto prefetchContext = ObjectFetchContext::getNullPrefetchContext();

  objectStore->getTree(readyTreeId, prefetchContext).get(0ms);
  objectStore->getTree(readyTreeId, prefetchContext).get(0ms);

  EXPECT_FALSE(treeCache->contains(readyTreeId));
  EXPECT_EQ(2, fakeBackingStore->getAccessCount(readyTreeId));

  objectStore->getTree(readyTreeId, context).get(0ms);

  EXPECT_TRUE(treeCache->contains(readyTreeId));
}

TEST_P(ObjectStoreTest, getTree_prefetchUsesTreeCacheWhenBypassDisabled) {
  auto config = EdenConfig::createTestEdenConfig();
  config->treeCacheBypassGlobAndPrefetch.setValue(
      false, ConfigSourceType::Default, true);
  auto cachingObjectStore = ObjectStore::create(
      fakeBackingStore,
      treeCache,
      stats.copy(),
      std::make_shared<ProcessInfoCache>(),
      makeTestEdenFsEventsLogger(),
      std::make_shared<ReloadableConfig>(config),
      GetParam(),
      folly::getGlobalCPUExecutor());
  auto prefetchContext = ObjectFetchContext::getNullPrefetchContext();

  cachingObjectStore->getTree(readyTreeId, prefetchContext).get(0ms);
  cachingObjectStore->getTree(readyTreeId, prefetchContext).get(0ms);

  EXPECT_TRUE(treeCache->contains(readyTreeId));
  EXPECT_EQ(1, fakeBackingStore->getAccessCount(readyTreeId));
}

TEST_P(ObjectStoreTest, getRootTree_seedsCacheWithRestrictedChild) {
  Tree::container entries{kPathMapDefaultCaseSensitive};
  entries.emplace(
      "restricted"_pc,
      TreeEntry{
          ObjectId{"restricted_child"},
          TreeEntryType::TREE,
          /*isRestricted=*/true,
          /*hasACL=*/true});
  auto* rootTree = fakeBackingStore->putTree(entries);
  rootTree->setReady();
  RootId rootId{"restricted_root"};
  auto* commit = fakeBackingStore->putCommit(rootId, rootTree);
  commit->setReady();

  auto rootResult = objectStore->getRootTree(rootId, context).get(0ms);
  EXPECT_NE(nullptr, treeCache->get(rootResult.treeId));

  objectStore->getTree(rootResult.treeId, context).get(0ms);

  ASSERT_EQ(1, loggingContext->requests.size());
  EXPECT_EQ(
      ObjectFetchContext::FromMemoryCache, loggingContext->requests[0].origin);
}

TEST_P(ObjectStoreTest, getTree_prefetch_missing_aux_data) {
  // FakeBackingStore provides a tree with no aux data
  auto tree1 =
      objectStoreWithTreeAuxPrefetching->getTree(readyTreeId, context).get(0ms);
  EXPECT_EQ(tree1->getAuxData(), nullptr);

  // Clear in-memory cache so that we are guaranteed to hit local caches
  treeCache->clear();

  // Issue a getTree that triggers tree aux prefetching. We will attempt to fall
  // back to BackingStore->getTreeAux() which will fail.
  auto tree2 =
      objectStoreWithTreeAuxPrefetching->getTree(readyTreeId, context).get(0ms);

  EXPECT_EQ(tree2->getAuxData(), nullptr);
}

TEST_P(ObjectStoreTest, getBlobSize_tracks_backing_store_read) {
  objectStore->getBlobSize(readyBlobId, context).get(0ms);
  ASSERT_EQ(1, loggingContext->requests.size());
  auto& request = loggingContext->requests[0];
  EXPECT_EQ(ObjectFetchContext::BlobAuxData, request.type);
  EXPECT_EQ(readyBlobId, request.id);
  EXPECT_EQ(ObjectFetchContext::FromNetworkFetch, request.origin);
}

TEST_P(ObjectStoreTest, getBlobSize_tracks_second_read_from_cache) {
  objectStore->getBlobSize(readyBlobId, context).get(0ms);
  objectStore->getBlobSize(readyBlobId, context).get(0ms);
  ASSERT_EQ(2, loggingContext->requests.size());
  auto& request = loggingContext->requests[1];
  EXPECT_EQ(ObjectFetchContext::BlobAuxData, request.type);
  EXPECT_EQ(readyBlobId, request.id);
  EXPECT_EQ(ObjectFetchContext::FromMemoryCache, request.origin);
}

TEST_P(ObjectStoreTest, getBlobSizeFromBackingStore) {
  auto data = "A"_sp;
  ObjectId id = putReadyBlob(data);

  size_t expectedSize = data.size();
  size_t size = objectStore->getBlobSize(id, context).get();
  EXPECT_EQ(expectedSize, size);
}

TEST_P(ObjectStoreTest, getBlobSizeNotFound) {
  ObjectId id;

  EXPECT_THROW_RE(
      objectStore->getBlobSize(id, context).get(),
      std::domain_error,
      "blob .* not found");
}

TEST_P(ObjectStoreTest, getBlobSha1) {
  auto data = "A"_sp;
  ObjectId id = putReadyBlob(data);

  Hash20 expectedSha1 = Hash20::sha1(data);
  Hash20 sha1 = objectStore->getBlobSha1(id, context).get();
  EXPECT_EQ(expectedSha1.toString(), sha1.toString());
}

CO_TEST_P(ObjectStoreTest, getBlobBlake3) {
  auto data = "A"_sp;
  ObjectId id = putReadyBlob(data);

  Hash32 expectedBlake3 = Hash32::blake3(data);
  Hash32 blake3 = co_await objectStore->co_getBlobBlake3(id, context);
  EXPECT_EQ(expectedBlake3.toString(), blake3.toString());
}

CO_TEST_P(ObjectStoreTest, getBlobKeyedBlake3) {
  auto data = "A"_sp;
  ObjectId id = putReadyBlob(data);

  Hash32 expectedBlake3 =
      Hash32::keyedBlake3(folly::ByteRange{kBlake3Key}, data);
  Hash32 blake3 =
      co_await objectStoreWithBlake3Key->co_getBlobBlake3(id, context);
  EXPECT_EQ(expectedBlake3.toString(), blake3.toString());
}

TEST_P(ObjectStoreTest, getBlobSha1NotFound) {
  ObjectId id;

  EXPECT_THROW_RE(
      objectStore->getBlobSha1(id, context).get(),
      std::domain_error,
      "blob .* not found");
}

CO_TEST_P(ObjectStoreTest, getBlobBlake3NotFound) {
  ObjectId id;

  bool caught = false;
  try {
    co_await objectStore->co_getBlobBlake3(id, context);
  } catch (const std::domain_error& e) {
    caught = true;
    EXPECT_TRUE(RE2::PartialMatch(e.what(), "blob .* not found"));
  }
  EXPECT_TRUE(caught);
}

CO_TEST_P(
    ObjectStoreTest,
    get_size_and_sha1_and_blake3_only_imports_blob_once) {
  objectStore->getBlobSize(readyBlobId, context).get(0ms);
  objectStore->getBlobSha1(readyBlobId, context).get(0ms);
  co_await objectStore->co_getBlobBlake3(readyBlobId, context);

  EXPECT_EQ(1, fakeBackingStore->getAccessCount(readyBlobId));
}

class PidFetchContext final : public ObjectFetchContext {
 public:
  explicit PidFetchContext(ProcessId pid) : ObjectFetchContext{}, pid_{pid} {}

  OptionalProcessId getClientPid() const override {
    return pid_;
  }

  Cause getCause() const override {
    return Cause::Unknown;
  }

  const std::unordered_map<std::string, std::string>* FOLLY_NULLABLE
  getRequestInfo() const override {
    return nullptr;
  }

 private:
  ProcessId pid_;
};

TEST_P(ObjectStoreTest, test_process_access_counts) {
  auto pid0 = ProcessId(10000);
  ObjectFetchContextPtr pidContext0 = makeRefPtr<PidFetchContext>(pid0);
  auto pid1 = ProcessId(10001);
  ObjectFetchContextPtr pidContext1 = makeRefPtr<PidFetchContext>(pid1);

  // first fetch increments fetch count for pid0
  objectStore->getBlob(readyBlobId, pidContext0).get(0ms);
  EXPECT_EQ(1, objectStore->getPidFetches().rlock()->at(pid0));

  // local fetch also increments fetch count for pid0
  objectStore->getBlob(readyBlobId, pidContext0).get(0ms);
  EXPECT_EQ(2, objectStore->getPidFetches().rlock()->at(pid0));

  // increments fetch count for pid1
  objectStore->getBlob(readyBlobId, pidContext1).get(0ms);
  EXPECT_EQ(2, objectStore->getPidFetches().rlock()->at(pid0));
  EXPECT_EQ(1, objectStore->getPidFetches().rlock()->at(pid1));
}

class FetchContext final : public ObjectFetchContext {
 public:
  FetchContext() = default;

  Cause getCause() const override {
    return Cause::Unknown;
  }

  const std::unordered_map<std::string, std::string>* FOLLY_NULLABLE
  getRequestInfo() const override {
    return nullptr;
  }

  void didFetch(
      ObjectType,
      const ObjectId&,
      Origin origin,
      uint64_t /*bytes*/ = 0) override {
    ++fetchCount_;
    origin_ = origin;
  }

  uint64_t getFetchCount() const {
    return fetchCount_;
  }

  Origin getFetchedOrigin() const {
    return origin_;
  }

 private:
  std::atomic<uint64_t> fetchCount_{0};
  std::atomic<Origin> origin_{Origin::NotFetched};
};

TEST_P(ObjectStoreTest, blobs_with_same_objectid_are_equal) {
  auto context = makeRefPtr<FetchContext>();

  auto objectId = putReadyBlob("foo");

  auto fut = objectStore->areBlobsEqual(
      objectId, objectId, context.as<ObjectFetchContext>());
  EXPECT_TRUE(std::move(fut).get(0ms));
  EXPECT_EQ(context->getFetchCount(), 0);
}

TEST_P(ObjectStoreTest, different_blobs_arent_equal) {
  auto context = makeRefPtr<FetchContext>();

  auto one = putReadyBlob("foo");
  auto two = putReadyBlob("bar");

  auto fut =
      objectStore->areBlobsEqual(one, two, context.as<ObjectFetchContext>());
  EXPECT_FALSE(std::move(fut).get(0ms));
  EXPECT_EQ(context->getFetchCount(), 2);
}

TEST_P(
    ObjectStoreTest,
    blobs_with_different_objectid_but_same_content_are_equal) {
  auto context = makeRefPtr<FetchContext>();

  auto one = putReadyBlob("foo");
  auto two = ObjectId{"not_a_constant_id"};
  auto storedBlob = fakeBackingStore->putBlob(two, "foo");
  storedBlob->setReady();

  auto fut =
      objectStore->areBlobsEqual(one, two, context.as<ObjectFetchContext>());
  EXPECT_TRUE(std::move(fut).get(0ms));
  EXPECT_EQ(context->getFetchCount(), 2);
}

TEST_P(ObjectStoreTest, get_tree_with_different_sensitivities) {
  auto context1 = makeRefPtr<FetchContext>();
  auto context2 = makeRefPtr<FetchContext>();
  auto context3 = makeRefPtr<FetchContext>();
  auto context4 = makeRefPtr<FetchContext>();

  // construct an object store with the opposite case sensitivity that shares
  // the same treecache
  auto oppositeSensitivityObjectStore = ObjectStore::create(
      fakeBackingStore,
      treeCache,
      stats.copy(),
      std::make_shared<ProcessInfoCache>(),
      makeTestEdenFsEventsLogger(),
      std::make_shared<ReloadableConfig>(EdenConfig::createTestEdenConfig()),
      getOppositeCaseSensitivity(),
      folly::getGlobalCPUExecutor());

  auto blobOne = putReadyBlob("foo content");
  auto blobTwo = putReadyBlob("bar content");

  // put a tree with the correct case sensitivity
  auto treeId = putReadyTree(
      {{{PathComponent{"Bar"},
         TreeEntry{ObjectId{blobOne}, TreeEntryType::EXECUTABLE_FILE}}},
       GetParam()});

  // put a tree with the opposite case sensitivity
  auto oppositeSensitivityTreeId = putReadyTree(
      {{{PathComponent{"foo"},
         TreeEntry{std::move(blobOne), TreeEntryType::REGULAR_FILE}},
        {PathComponent{"Baz"},
         TreeEntry{ObjectId{readyTreeId}, TreeEntryType::TREE}}},
       getOppositeCaseSensitivity()});

  // fetch the "GetParam() case sensitivity" tree from the "GetParam() case
  // sensitivity" object store. this should populate the treecache
  auto treeFromMatchingObjectStoreResult =
      objectStore->getTree(treeId, context1.as<ObjectFetchContext>()).get(0ms);

  EXPECT_EQ(context1->getFetchedOrigin(), ObjectFetchContext::FromNetworkFetch);
  EXPECT_EQ(
      treeFromMatchingObjectStoreResult->getCaseSensitivity(), GetParam());

  // fetch the "getOppositeCaseSensitivity() case sensitivity" tree from the
  // "GetParam() case sensitivity" object store. this should populate the
  // treecache
  auto oppositeSensitivityTreeFromMatchingObjectStoreResult =
      objectStore
          ->getTree(
              oppositeSensitivityTreeId, context2.as<ObjectFetchContext>())
          .get(0ms);

  EXPECT_EQ(context2->getFetchedOrigin(), ObjectFetchContext::FromNetworkFetch);
  EXPECT_EQ(
      oppositeSensitivityTreeFromMatchingObjectStoreResult
          ->getCaseSensitivity(),
      GetParam());

  // get the "GetParam() case sensitivity" from the
  // "getOppositeCaseSensitivity() case sensitivity" object store. this should
  // not fetch from the backing store and should instead be served from the
  // treecache populated by the first object store's call to getTree
  auto treeFromOppositeObjectStoreResult =
      oppositeSensitivityObjectStore
          ->getTree(treeId, context3.as<ObjectFetchContext>())
          .get(0ms);

  EXPECT_EQ(context3->getFetchedOrigin(), ObjectFetchContext::FromMemoryCache);
  EXPECT_EQ(
      treeFromOppositeObjectStoreResult->getCaseSensitivity(),
      getOppositeCaseSensitivity());

  // get the "getOppositeCaseSensitivity() case sensitivity" from the
  // "getOppositeCaseSensitivity() case sensitivity" object store. this should
  // not fetch from the backing store and should instead be served from the
  // treecache populated by the first object store's call to getTree
  auto oppositeSensitivityTreeFromOppositeObjectStoreResult =
      oppositeSensitivityObjectStore
          ->getTree(
              oppositeSensitivityTreeId, context4.as<ObjectFetchContext>())
          .get(0ms);

  EXPECT_EQ(context4->getFetchedOrigin(), ObjectFetchContext::FromMemoryCache);
  EXPECT_EQ(
      oppositeSensitivityTreeFromOppositeObjectStoreResult
          ->getCaseSensitivity(),
      getOppositeCaseSensitivity());
}

CO_TEST_P(ObjectStoreTest, co_getBlob_tracks_backing_store_read) {
  co_await objectStore->co_getBlob(readyBlobId, context);
  EXPECT_EQ(1, loggingContext->requests.size());
  auto& request = loggingContext->requests[0];
  EXPECT_EQ(ObjectFetchContext::Blob, request.type);
  EXPECT_EQ(readyBlobId, request.id);
  EXPECT_EQ(ObjectFetchContext::FromNetworkFetch, request.origin);
}

CO_TEST_P(ObjectStoreTest, co_test_process_access_counts) {
  auto pid0 = ProcessId(10000);
  ObjectFetchContextPtr pidContext0 = makeRefPtr<PidFetchContext>(pid0);
  auto pid1 = ProcessId(10001);
  ObjectFetchContextPtr pidContext1 = makeRefPtr<PidFetchContext>(pid1);

  // first fetch increments fetch count for pid0
  co_await objectStore->co_getBlob(readyBlobId, pidContext0);
  EXPECT_EQ(1, objectStore->getPidFetches().rlock()->at(pid0));

  // local fetch also increments fetch count for pid0
  co_await objectStore->co_getBlob(readyBlobId, pidContext0);
  EXPECT_EQ(2, objectStore->getPidFetches().rlock()->at(pid0));

  // increments fetch count for pid1
  co_await objectStore->co_getBlob(readyBlobId, pidContext1);
  EXPECT_EQ(2, objectStore->getPidFetches().rlock()->at(pid0));
  EXPECT_EQ(1, objectStore->getPidFetches().rlock()->at(pid1));
}

CO_TEST_P(ObjectStoreTest, co_getBlobSha1) {
  auto data = "A"_sp;
  ObjectId id = putReadyBlob(data);

  Hash20 expectedSha1 = Hash20::sha1(data);
  Hash20 sha1 = co_await objectStore->co_getBlobSha1(id, context);
  EXPECT_EQ(expectedSha1.toString(), sha1.toString());
}

CO_TEST_P(ObjectStoreTest, co_getBlobSha1NotFound) {
  ObjectId id;

  bool caught = false;
  try {
    co_await objectStore->co_getBlobSha1(id, context);
  } catch (const std::domain_error& e) {
    caught = true;
    EXPECT_TRUE(RE2::PartialMatch(e.what(), "blob .* not found"));
  }
  EXPECT_TRUE(caught);
}

CO_TEST_P(
    ObjectStoreTest,
    co_get_size_and_sha1_and_blake3_only_imports_blob_once) {
  co_await objectStore->co_getBlobAuxData(readyBlobId, context);
  co_await objectStore->co_getBlobSha1(readyBlobId, context);
  co_await objectStore->co_getBlobBlake3(readyBlobId, context);

  EXPECT_EQ(1, fakeBackingStore->getAccessCount(readyBlobId));
}

INSTANTIATE_TEST_SUITE_P(
    ObjectStoreTest,
    ObjectStoreTest,
    ::testing::Values(
        CaseSensitivity::Sensitive,
        CaseSensitivity::Insensitive));

namespace {

// Separate fixture so we can control the TTL config value per test.
struct ObjectStoreCheckPermissionTest : public ::testing::Test {
  void SetUp() override {
    rawEdenConfig = EdenConfig::createTestEdenConfig();
    auto edenConfig = std::make_shared<ReloadableConfig>(rawEdenConfig);
    stats = makeRefPtr<EdenStats>();
    treeCache = TreeCache::create(edenConfig, stats.copy());
    fakeBackingStore = std::make_shared<FakeBackingStore>();
    objectStore = ObjectStore::create(
        fakeBackingStore,
        treeCache,
        stats.copy(),
        std::make_shared<ProcessInfoCache>(),
        makeTestEdenFsEventsLogger(),
        edenConfig,
        CaseSensitivity::Sensitive,
        folly::getGlobalCPUExecutor());
  }

  void setTtlSeconds(uint64_t seconds) {
    rawEdenConfig->restrictedTreeTtlSeconds.setValue(
        seconds, ConfigSourceType::UserConfig, true);
  }

  int64_t getCounter(folly::StringPiece key) {
    stats->flush();
    return facebook::fb303::ServiceData::get()
        ->getCounterIfExists(key)
        .value_or(0);
  }

  std::shared_ptr<EdenConfig> rawEdenConfig;
  std::shared_ptr<FakeBackingStore> fakeBackingStore;
  std::shared_ptr<TreeCache> treeCache;
  EdenStatsPtr stats;
  std::shared_ptr<ObjectStore> objectStore;
};

constexpr folly::StringPiece kCheckPermissionCounter =
    "object_store.check_permission.sum.60";
constexpr folly::StringPiece kCheckPermissionBackingStoreCounter =
    "object_store.check_permission.backing_store.sum.60";

} // namespace

TEST_F(
    ObjectStoreCheckPermissionTest,
    ttl_not_expired_short_circuits_to_false) {
  // 5 minute TTL, lastCheck is "now" — TTL not expired.
  setTtlSeconds(300);
  ObjectId id{"manifest_id_1"};
  // Configure backing store to return true; if we delegated, we'd see true.
  fakeBackingStore->setCheckPermissionResult(id, true);
  auto initialChecks = getCounter(kCheckPermissionCounter);
  auto initialBackingStoreChecks =
      getCounter(kCheckPermissionBackingStoreCounter);

  auto now = std::chrono::steady_clock::now();
  auto result = objectStore->checkPermissionIfExpired(id, now).get(0ms);

  // TTL has not expired so result is false (still restricted) and the
  // backing store was not consulted.
  EXPECT_FALSE(result);
  EXPECT_EQ(0, fakeBackingStore->getCheckPermissionCount(id));
  EXPECT_EQ(1, getCounter(kCheckPermissionCounter) - initialChecks);
  EXPECT_EQ(
      0,
      getCounter(kCheckPermissionBackingStoreCounter) -
          initialBackingStoreChecks);
}

TEST_F(
    ObjectStoreCheckPermissionTest,
    ttl_expired_delegates_to_backing_store_true) {
  // Zero TTL means every call goes through to the backing store.
  setTtlSeconds(0);
  ObjectId id{"manifest_id_2"};
  fakeBackingStore->setCheckPermissionResult(id, true);
  auto initialChecks = getCounter(kCheckPermissionCounter);
  auto initialBackingStoreChecks =
      getCounter(kCheckPermissionBackingStoreCounter);

  auto now = std::chrono::steady_clock::now();
  auto result = objectStore->checkPermissionIfExpired(id, now).get(0ms);

  EXPECT_TRUE(result);
  EXPECT_EQ(1, fakeBackingStore->getCheckPermissionCount(id));
  EXPECT_EQ(1, getCounter(kCheckPermissionCounter) - initialChecks);
  EXPECT_EQ(
      1,
      getCounter(kCheckPermissionBackingStoreCounter) -
          initialBackingStoreChecks);
}

TEST_F(
    ObjectStoreCheckPermissionTest,
    ttl_expired_delegates_to_backing_store_false) {
  // TTL of 1 second with lastCheck far in the past — TTL expired.
  setTtlSeconds(1);
  ObjectId id{"manifest_id_3"};
  fakeBackingStore->setCheckPermissionResult(id, false);

  auto longAgo = std::chrono::steady_clock::now() - std::chrono::hours(1);
  auto result = objectStore->checkPermissionIfExpired(id, longAgo).get(0ms);

  EXPECT_FALSE(result);
  EXPECT_EQ(1, fakeBackingStore->getCheckPermissionCount(id));
}

} // namespace facebook::eden
