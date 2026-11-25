/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/store/StatsFetchContext.h"
#include <fb303/ServiceData.h>
#include <gtest/gtest.h>
#include <string>
#include <type_traits>
#include <utility>
#include <vector>
#include "eden/fs/model/ObjectId.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/utils/SourceLocation.h"

namespace facebook::eden {

namespace {

ObjectId makeTestId(const char* hex) {
  return ObjectId::fromHex(hex);
}

} // namespace

TEST(StatsFetchContextTest, DidFetchTracksBytes) {
  StatsFetchContext ctx;
  ObjectId id = makeTestId("1234567890123456789012345678901234567890");

  ctx.didFetch(
      ObjectFetchContext::Blob, id, ObjectFetchContext::FromNetworkFetch, 1024);
  ctx.didFetch(
      ObjectFetchContext::Blob, id, ObjectFetchContext::FromNetworkFetch, 2048);

  EXPECT_EQ(
      2,
      ctx.countFetchesOfTypeAndOrigin(
          ObjectFetchContext::Blob, ObjectFetchContext::FromNetworkFetch));
  EXPECT_EQ(
      3072,
      ctx.countBytesFetchedOfTypeAndOrigin(
          ObjectFetchContext::Blob, ObjectFetchContext::FromNetworkFetch));
}

TEST(StatsFetchContextTest, DidFetchBatchAggregates) {
  StatsFetchContext ctx;

  ctx.didFetchBatch(
      ObjectFetchContext::Tree, ObjectFetchContext::FromDiskCache, 100, 50000);

  EXPECT_EQ(
      100,
      ctx.countFetchesOfTypeAndOrigin(
          ObjectFetchContext::Tree, ObjectFetchContext::FromDiskCache));
  EXPECT_EQ(
      50000,
      ctx.countBytesFetchedOfTypeAndOrigin(
          ObjectFetchContext::Tree, ObjectFetchContext::FromDiskCache));
}

TEST(StatsFetchContextTest, ComputeStatisticsIncludesBytes) {
  StatsFetchContext ctx;
  ObjectId id = makeTestId("1234567890123456789012345678901234567890");

  ctx.didFetch(
      ObjectFetchContext::Blob, id, ObjectFetchContext::FromMemoryCache, 100);
  ctx.didFetch(
      ObjectFetchContext::Blob, id, ObjectFetchContext::FromDiskCache, 200);
  ctx.didFetch(
      ObjectFetchContext::Blob, id, ObjectFetchContext::FromNetworkFetch, 300);

  auto stats = ctx.computeStatistics();
  EXPECT_EQ(600, stats.blob.totalBytes);
  EXPECT_EQ(3, stats.blob.accessCount);
}

TEST(StatsFetchContextTest, CopyConstructorPreservesBytes) {
  StatsFetchContext ctx1;
  ObjectId id = makeTestId("1234567890123456789012345678901234567890");
  ctx1.didFetch(
      ObjectFetchContext::Blob, id, ObjectFetchContext::FromNetworkFetch, 1000);
  ctx1.addPrefetchedBlobSize(64);

  StatsFetchContext ctx2(ctx1);

  EXPECT_EQ(
      1,
      ctx2.countFetchesOfTypeAndOrigin(
          ObjectFetchContext::Blob, ObjectFetchContext::FromNetworkFetch));
  EXPECT_EQ(
      1000,
      ctx2.countBytesFetchedOfTypeAndOrigin(
          ObjectFetchContext::Blob, ObjectFetchContext::FromNetworkFetch));
  EXPECT_EQ(64, ctx2.getPrefetchedBlobBytes());
}

TEST(StatsFetchContextTest, MergeAddsBytes) {
  StatsFetchContext ctx1, ctx2;
  ObjectId id1 = makeTestId("1111111111111111111111111111111111111111");
  ObjectId id2 = makeTestId("2222222222222222222222222222222222222222");

  ctx1.didFetch(
      ObjectFetchContext::Tree, id1, ObjectFetchContext::FromDiskCache, 500);
  ctx2.didFetch(
      ObjectFetchContext::Tree, id2, ObjectFetchContext::FromDiskCache, 700);
  ctx1.addPrefetchedBlobSize(10);
  ctx2.addPrefetchedBlobSize(30);

  ctx1.merge(ctx2);

  EXPECT_EQ(
      2,
      ctx1.countFetchesOfTypeAndOrigin(
          ObjectFetchContext::Tree, ObjectFetchContext::FromDiskCache));
  EXPECT_EQ(
      1200,
      ctx1.countBytesFetchedOfTypeAndOrigin(
          ObjectFetchContext::Tree, ObjectFetchContext::FromDiskCache));
  EXPECT_EQ(40, ctx1.getPrefetchedBlobBytes());
}

TEST(StatsFetchContextTest, DidFetchWithoutBytesStillWorks) {
  StatsFetchContext ctx;
  ObjectId id = makeTestId("1234567890123456789012345678901234567890");

  // Use the old API without bytes parameter
  ctx.didFetch(
      ObjectFetchContext::Blob, id, ObjectFetchContext::FromNetworkFetch);

  EXPECT_EQ(1, ctx.countFetchesOfType(ObjectFetchContext::Blob));
  // Bytes should be 0 when using the old API
  EXPECT_EQ(
      0,
      ctx.countBytesFetchedOfTypeAndOrigin(
          ObjectFetchContext::Blob, ObjectFetchContext::FromNetworkFetch));
}

TEST(StatsFetchContextTest, DidFetchFailedTracksFailures) {
  StatsFetchContext ctx;

  ctx.didFetchFailed(ObjectFetchContext::Blob, 5);
  ctx.didFetchFailed(ObjectFetchContext::Blob, 3);

  EXPECT_EQ(8, ctx.getFailureCount(ObjectFetchContext::Blob));
  EXPECT_EQ(0, ctx.getFailureCount(ObjectFetchContext::Tree));
}

TEST(StatsFetchContextTest, CopyConstructorPreservesFailures) {
  StatsFetchContext ctx1;
  ctx1.didFetchFailed(ObjectFetchContext::Blob, 10);

  StatsFetchContext ctx2(ctx1);

  EXPECT_EQ(10, ctx2.getFailureCount(ObjectFetchContext::Blob));
}

TEST(StatsFetchContextTest, CauseDetailRequiresExplicitLifetime) {
  using StackArray = char[sizeof("stack memory")];
  using ConstStackArray = const char[sizeof("stack memory")];
  static_assert(!std::is_constructible_v<
                ObjectFetchContext::StaticCauseDetail,
                StackArray&>);
  static_assert(!std::is_constructible_v<
                ObjectFetchContext::StaticCauseDetail,
                ConstStackArray&>);
  static_assert(
      !std::is_constructible_v<ObjectFetchContext::CauseDetail, StackArray&>);
  static_assert(!std::is_constructible_v<
                ObjectFetchContext::CauseDetail,
                ConstStackArray&>);
  static_assert(!std::is_constructible_v<
                ObjectFetchContext::CauseDetail,
                std::string_view>);
  StatsFetchContext ctx{
      std::nullopt,
      ObjectFetchContext::Cause::Thrift,
      ObjectFetchContext::StaticCauseDetail::fromLiteral("checkout caller"),
      nullptr};

  auto detail = ctx.getCauseDetail();
  ASSERT_TRUE(detail.has_value());
  EXPECT_EQ("checkout caller", detail.value());
}

TEST(StatsFetchContextTest, CauseDetailCanOwnDynamicString) {
  std::string dynamicDetail = "checkout caller";
  StatsFetchContext ctx{
      std::nullopt,
      ObjectFetchContext::Cause::Thrift,
      ObjectFetchContext::CauseDetail::fromOwnedString(dynamicDetail),
      nullptr};

  dynamicDetail = "mutated caller";

  auto detail = ctx.getCauseDetail();
  ASSERT_TRUE(detail.has_value());
  EXPECT_EQ("checkout caller", detail.value());
}

TEST(StatsFetchContextTest, StaticCauseDetailAcceptsSourceLocation) {
  const auto sourceLocation = EDEN_CURRENT_SOURCE_LOCATION;
  StatsFetchContext ctx{
      std::nullopt,
      ObjectFetchContext::Cause::Thrift,
      ObjectFetchContext::StaticCauseDetail::fromSourceLocation(sourceLocation),
      nullptr};

  EXPECT_EQ(ctx.getCauseDetail(), sourceLocation.function_name());
}

TEST(StatsFetchContextTest, CopiedCauseDetailOutlivesSourceContext) {
  ObjectFetchContext::CauseDetail copiedDetail;
  {
    StatsFetchContext ctx{
        std::nullopt,
        ObjectFetchContext::Cause::Thrift,
        ObjectFetchContext::CauseDetail::fromOwnedString("copied detail"),
        nullptr};
    copiedDetail = ctx.copyCauseDetail();
  }

  const auto detail = copiedDetail.asStringView();
  ASSERT_TRUE(detail.has_value());
  EXPECT_EQ("copied detail", *detail);
}

TEST(StatsFetchContextTest, MergeAddsFailures) {
  StatsFetchContext ctx1, ctx2;

  ctx1.didFetchFailed(ObjectFetchContext::Blob, 5);
  ctx2.didFetchFailed(ObjectFetchContext::Blob, 7);

  ctx1.merge(ctx2);

  EXPECT_EQ(12, ctx1.getFailureCount(ObjectFetchContext::Blob));
}

TEST(StatsFetchContextTest, MoveConstructorPreservesBytes) {
  StatsFetchContext ctx1{
      std::nullopt,
      ObjectFetchContext::Cause::Thrift,
      ObjectFetchContext::StaticCauseDetail::fromLiteral("move constructor"),
      nullptr};
  ObjectId id = makeTestId("1234567890123456789012345678901234567890");
  ctx1.didFetch(
      ObjectFetchContext::Blob, id, ObjectFetchContext::FromNetworkFetch, 1000);
  ctx1.didFetchFailed(ObjectFetchContext::Blob, 3);

  StatsFetchContext ctx2(std::move(ctx1));

  EXPECT_EQ(
      1,
      ctx2.countFetchesOfTypeAndOrigin(
          ObjectFetchContext::Blob, ObjectFetchContext::FromNetworkFetch));
  EXPECT_EQ(
      1000,
      ctx2.countBytesFetchedOfTypeAndOrigin(
          ObjectFetchContext::Blob, ObjectFetchContext::FromNetworkFetch));
  EXPECT_EQ(3, ctx2.getFailureCount(ObjectFetchContext::Blob));

  auto detail = ctx2.getCauseDetail();
  ASSERT_TRUE(detail.has_value());
  EXPECT_EQ("move constructor", detail.value());
}

TEST(StatsFetchContextTest, CopyConstructorPreservesOwnedCauseDetail) {
  auto ctx2 = [] {
    StatsFetchContext ctx1{
        std::nullopt,
        ObjectFetchContext::Cause::Thrift,
        ObjectFetchContext::CauseDetail::fromOwnedString("copy constructor"),
        nullptr};
    return StatsFetchContext{ctx1};
  }();

  auto detail = ctx2.getCauseDetail();
  ASSERT_TRUE(detail.has_value());
  EXPECT_EQ("copy constructor", detail.value());
}

TEST(StatsFetchContextTest, MoveConstructorPreservesOwnedCauseDetail) {
  StatsFetchContext ctx1{
      std::nullopt,
      ObjectFetchContext::Cause::Thrift,
      ObjectFetchContext::CauseDetail::fromOwnedString("move constructor"),
      nullptr};

  StatsFetchContext ctx2(std::move(ctx1));

  auto detail = ctx2.getCauseDetail();
  ASSERT_TRUE(detail.has_value());
  EXPECT_EQ("move constructor", detail.value());
}

TEST(StatsFetchContextTest, MoveAssignmentPreservesBytes) {
  StatsFetchContext ctx1;
  ObjectId id = makeTestId("1234567890123456789012345678901234567890");
  ctx1.didFetch(
      ObjectFetchContext::Blob, id, ObjectFetchContext::FromDiskCache, 2000);
  ctx1.didFetchFailed(ObjectFetchContext::Tree, 7);

  StatsFetchContext ctx2;
  ctx2 = std::move(ctx1);

  EXPECT_EQ(
      1,
      ctx2.countFetchesOfTypeAndOrigin(
          ObjectFetchContext::Blob, ObjectFetchContext::FromDiskCache));
  EXPECT_EQ(
      2000,
      ctx2.countBytesFetchedOfTypeAndOrigin(
          ObjectFetchContext::Blob, ObjectFetchContext::FromDiskCache));
  EXPECT_EQ(7, ctx2.getFailureCount(ObjectFetchContext::Tree));
}

TEST(StatsFetchContextTest, BytesTrackedPerObjectType) {
  StatsFetchContext ctx;
  ObjectId id = makeTestId("1234567890123456789012345678901234567890");

  ctx.didFetch(
      ObjectFetchContext::Blob, id, ObjectFetchContext::FromNetworkFetch, 100);
  ctx.didFetch(
      ObjectFetchContext::Tree, id, ObjectFetchContext::FromDiskCache, 200);

  EXPECT_EQ(
      100,
      ctx.countBytesFetchedOfTypeAndOrigin(
          ObjectFetchContext::Blob, ObjectFetchContext::FromNetworkFetch));
  EXPECT_EQ(
      200,
      ctx.countBytesFetchedOfTypeAndOrigin(
          ObjectFetchContext::Tree, ObjectFetchContext::FromDiskCache));
  // No cross-contamination between types/origins
  EXPECT_EQ(
      0,
      ctx.countBytesFetchedOfTypeAndOrigin(
          ObjectFetchContext::Blob, ObjectFetchContext::FromDiskCache));
  EXPECT_EQ(
      0,
      ctx.countBytesFetchedOfTypeAndOrigin(
          ObjectFetchContext::Tree, ObjectFetchContext::FromNetworkFetch));
}

TEST(StatsFetchContextTest, ComputeStatisticsForAllTypes) {
  StatsFetchContext ctx;
  ObjectId id = makeTestId("1234567890123456789012345678901234567890");

  ctx.didFetch(
      ObjectFetchContext::Tree, id, ObjectFetchContext::FromNetworkFetch, 400);
  ctx.didFetch(
      ObjectFetchContext::Tree, id, ObjectFetchContext::FromMemoryCache, 100);
  ctx.didFetch(
      ObjectFetchContext::BlobAuxData,
      id,
      ObjectFetchContext::FromDiskCache,
      50);

  auto stats = ctx.computeStatistics();

  EXPECT_EQ(500, stats.tree.totalBytes);
  EXPECT_EQ(2, stats.tree.accessCount);
  EXPECT_EQ(1, stats.tree.fetchCount);

  EXPECT_EQ(50, stats.blobAuxData.totalBytes);
  EXPECT_EQ(1, stats.blobAuxData.accessCount);

  EXPECT_EQ(0, stats.blob.totalBytes);
  EXPECT_EQ(0, stats.blob.accessCount);
}

TEST(StatsFetchContextTest, SetFetchedSourceCountsLocalFetchesPerType) {
  using LocalCounter = StatsGroupBase::Counter SaplingBackingStoreStats::*;
  const std::vector<std::pair<ObjectFetchContext::ObjectType, LocalCounter>>
      localCounters{
          {ObjectFetchContext::Blob, &SaplingBackingStoreStats::fetchBlobLocal},
          {ObjectFetchContext::BlobAuxData,
           &SaplingBackingStoreStats::fetchBlobAuxDataLocal},
          {ObjectFetchContext::Tree, &SaplingBackingStoreStats::fetchTreeLocal},
          {ObjectFetchContext::TreeAuxData,
           &SaplingBackingStoreStats::fetchTreeAuxDataLocal},
          {ObjectFetchContext::RootTree,
           &SaplingBackingStoreStats::getRootTreeLocal},
          {ObjectFetchContext::ManifestForRoot,
           &SaplingBackingStoreStats::importManifestForRootLocal},
          {ObjectFetchContext::PrefetchBlob,
           &SaplingBackingStoreStats::prefetchBlobLocal},
      };
  auto stats = makeRefPtr<EdenStats>();
  auto readCounters = [&] {
    stats->flush();
    std::vector<int64_t> values;
    values.reserve(localCounters.size());
    for (const auto& [type, counter] : localCounters) {
      values.push_back(
          fb303::ServiceData::get()
              ->getCounterIfExists(
                  std::string{stats->getName(counter)} + ".sum")
              .value_or(0));
    }
    return values;
  };
  StatsFetchContext ctx;

  const auto before = readCounters();
  for (const auto& [type, counter] : localCounters) {
    ctx.setFetchedSource(
        ObjectFetchContext::FetchedSource::Remote, type, stats.copy());
    ctx.setFetchedSource(
        ObjectFetchContext::FetchedSource::Unknown, type, stats.copy());
  }
  EXPECT_EQ(before, readCounters());

  for (size_t i = 0; i < localCounters.size(); ++i) {
    auto expected = readCounters();
    ++expected[i];
    ctx.setFetchedSource(
        ObjectFetchContext::FetchedSource::Local,
        localCounters[i].first,
        stats.copy());
    EXPECT_EQ(expected, readCounters());
  }
}

} // namespace facebook::eden
