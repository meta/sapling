/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/store/BlobAccess.h"
#include <folly/executors/GlobalExecutor.h>
#include <gtest/gtest.h>
#include <chrono>
#include "eden/common/utils/ProcessInfoCache.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/config/ReloadableConfig.h"
#include "eden/fs/store/ObjectStore.h"
#include "eden/fs/store/TreeCache.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/testharness/FakeBackingStore.h"
#include "eden/fs/testharness/LoggingFetchContext.h"

using namespace folly::literals;
using namespace std::chrono_literals;
using namespace facebook::eden;

namespace {
const auto id3 = ObjectId::fromHex("0000000000000000000000000000000000000000");
const auto id4 = ObjectId::fromHex("0000000000000000000000000000000000000001");
const auto id5 = ObjectId::fromHex("0000000000000000000000000000000000000002");
const auto id6 = ObjectId::fromHex("0000000000000000000000000000000000000003");

const auto blob3 = std::make_shared<Blob>("333"_sp);
const auto blob4 = std::make_shared<Blob>("4444"_sp);
const auto blob5 = std::make_shared<Blob>("55555"_sp);
const auto blob6 = std::make_shared<Blob>("666666"_sp);

constexpr size_t kTreeCacheMaximumSize = 1000; // bytes
constexpr size_t kTreeCacheMinimumEntries = 0;

struct BlobAccessTest : ::testing::Test {
  BlobAccessTest() : backingStore{std::make_shared<FakeBackingStore>()} {
    std::shared_ptr<EdenConfig> rawEdenConfig{
        EdenConfig::createTestEdenConfig()};
    rawEdenConfig->inMemoryTreeCacheSize.setValue(
        kTreeCacheMaximumSize, ConfigSourceType::Default, true);
    rawEdenConfig->inMemoryTreeCacheMinimumItems.setValue(
        kTreeCacheMinimumEntries, ConfigSourceType::Default, true);
    auto edenConfig = std::make_shared<ReloadableConfig>(rawEdenConfig);
    auto blobCache =
        BlobCache::create(10, 0, edenConfig, makeRefPtr<EdenStats>());
    auto treeCache = TreeCache::create(edenConfig, makeRefPtr<EdenStats>());

    objectStore = ObjectStore::create(
        backingStore,
        treeCache,
        makeRefPtr<EdenStats>(),
        std::make_shared<ProcessInfoCache>(),
        std::make_shared<EdenFsEventsLogger>(nullptr),
        edenConfig,
        kPathMapDefaultCaseSensitive,
        folly::getGlobalCPUExecutor());

    blobAccess = std::make_shared<BlobAccess>(objectStore, blobCache);

    backingStore->putBlob(id3, "333"_sp)->setReady();
    backingStore->putBlob(id4, "4444"_sp)->setReady();
    backingStore->putBlob(id5, "55555"_sp)->setReady();
    backingStore->putBlob(id6, "666666"_sp)->setReady();
  }

  std::shared_ptr<const Blob> getBlobBlocking(const ObjectId& id) {
    return blobAccess->getBlob(id, ObjectFetchContext::getNullContext())
        .get(0ms)
        .object;
  }

  LoggingFetchContext context;
  std::shared_ptr<FakeBackingStore> backingStore;
  std::shared_ptr<ObjectStore> objectStore;
  std::shared_ptr<BlobAccess> blobAccess;
};

} // namespace

TEST_F(BlobAccessTest, remembers_blobs) {
  auto blob1 = getBlobBlocking(id4);
  auto blob2 = getBlobBlocking(id4);

  EXPECT_EQ(blob1, blob2);
  EXPECT_EQ(4, blob1->getSize());
  EXPECT_EQ(1, backingStore->getAccessCount(id4));
}

TEST_F(BlobAccessTest, drops_blobs_when_size_is_exceeded) {
  auto blob0 = getBlobBlocking(id6);
  auto blob1 = getBlobBlocking(id5);
  auto blob2 = getBlobBlocking(id6);

  EXPECT_EQ(6, blob0->getSize());
  EXPECT_EQ(5, blob1->getSize());
  EXPECT_EQ(6, blob2->getSize());

  EXPECT_EQ(1, backingStore->getAccessCount(id5));
  EXPECT_EQ(2, backingStore->getAccessCount(id6));
}

TEST_F(BlobAccessTest, drops_oldest_blobs) {
  getBlobBlocking(id3);
  getBlobBlocking(id4);

  // Evicts id3
  getBlobBlocking(id5);
  EXPECT_EQ(1, backingStore->getAccessCount(id3));
  EXPECT_EQ(1, backingStore->getAccessCount(id4));
  EXPECT_EQ(1, backingStore->getAccessCount(id5));

  // Evicts id4 but not id5
  getBlobBlocking(id3);
  getBlobBlocking(id5);
  EXPECT_EQ(2, backingStore->getAccessCount(id3));
  EXPECT_EQ(1, backingStore->getAccessCount(id4));
  EXPECT_EQ(1, backingStore->getAccessCount(id5));

  // Evicts id3
  getBlobBlocking(id4);
  getBlobBlocking(id5);
  EXPECT_EQ(2, backingStore->getAccessCount(id3));
  EXPECT_EQ(2, backingStore->getAccessCount(id4));
  EXPECT_EQ(1, backingStore->getAccessCount(id5));
}

TEST_F(BlobAccessTest, does_not_keep_blob_larger_than_cache) {
  const auto id11 =
      ObjectId::fromHex("0000000000000000000000000000000000000004");
  backingStore->putBlob(id11, "11111111111"_sp)->setReady();

  auto blob1 = getBlobBlocking(id11);
  auto blob2 = getBlobBlocking(id11);

  EXPECT_EQ(11, blob1->getSize());
  EXPECT_EQ(11, blob2->getSize());
  EXPECT_EQ(2, backingStore->getAccessCount(id11));
}
