/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/store/ObjectCache.h"
#include <gtest/gtest.h>
#include <chrono>
#include <csignal>
#include <cstdio>
#include <cstdlib>
#include <new>
#include <utility>

#ifndef _WIN32
#include <unistd.h>
#endif

using namespace folly::literals;
using namespace facebook::eden;

#ifndef _WIN32
namespace facebook::eden {
namespace {
struct ThrowingEvictionStats : FakeStats {
  Counter insertEviction{"do_not_export_throwing_eviction"};
};
} // namespace

template <>
ThrowingEvictionStats&
EdenStats::getStatsForCurrentThread<ThrowingEvictionStats>() {
  std::fputs("throwing during eviction\n", stderr);
  throw std::bad_alloc{};
}
} // namespace facebook::eden
#endif

namespace {

class CacheObject {
 public:
  const ObjectId& getObjectId() const {
    return id_;
  }

  size_t getSizeBytes() const {
    return size_;
  }

  void setSizeBytes(size_t size) {
    size_ = size;
  }

  CacheObject(ObjectId id, size_t size) : id_{std::move(id)}, size_{size} {}

 private:
  ObjectId id_;
  size_t size_;
};

const auto id3 = ObjectId::fromHex("0000000000000000000000000000000000000000");
const auto id3a = ObjectId::fromHex("0000000000000000000000000000000000000010");
const auto id3b = ObjectId::fromHex("0000000000000000000000000000000000000020");
const auto id3c = ObjectId::fromHex("0000000000000000000000000000000000000030");
const auto id4 = ObjectId::fromHex("0000000000000000000000000000000000000001");
const auto id5 = ObjectId::fromHex("0000000000000000000000000000000000000002");
const auto id6 = ObjectId::fromHex("0000000000000000000000000000000000000003");
const auto id9 = ObjectId::fromHex("0000000000000000000000000000000000000004");
const auto id11 = ObjectId::fromHex("0000000000000000000000000000000000000005");

// Each object's name corresponds to its length in bytes.

const auto object3 = std::make_shared<CacheObject>(id3, 3);
const auto object3a = std::make_shared<CacheObject>(id3a, 3);
const auto object3b = std::make_shared<CacheObject>(id3b, 3);
const auto object3c = std::make_shared<CacheObject>(id3c, 3);
const auto object4 = std::make_shared<CacheObject>(id4, 4);
const auto object5 = std::make_shared<CacheObject>(id5, 5);
const auto object6 = std::make_shared<CacheObject>(id6, 6);
const auto object9 = std::make_shared<CacheObject>(id9, 9);
const auto object11 = std::make_shared<CacheObject>(id11, 11);
} // namespace

/**
 * simple non-interest-handle test cases
 */

TEST(ObjectCache, testSimpleInsert) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          10, 1, makeRefPtr<EdenStats>());

  cache->insertSimple(object3->getObjectId(), object3);

  EXPECT_TRUE(cache->contains(object3->getObjectId()));
  EXPECT_EQ(object3, cache->getSimple(object3->getObjectId()));
}

TEST(ObjectCache, testMultipleInsert) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          10, 1, makeRefPtr<EdenStats>());

  cache->insertSimple(object3->getObjectId(), object3);
  cache->insertSimple(object3a->getObjectId(), object3a);
  cache->insertSimple(object3b->getObjectId(), object3b);

  EXPECT_TRUE(cache->contains(object3->getObjectId()));
  EXPECT_EQ(object3, cache->getSimple(object3->getObjectId()));
  EXPECT_TRUE(cache->contains(object3a->getObjectId()));
  EXPECT_EQ(object3a, cache->getSimple(object3a->getObjectId()));
  EXPECT_TRUE(cache->contains(object3b->getObjectId()));
  EXPECT_EQ(object3b, cache->getSimple(object3b->getObjectId()));
}

TEST(ObjectCache, testSizeOverflowInsert) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          10, 1, makeRefPtr<EdenStats>());

  cache->insertSimple(object3->getObjectId(), object3);
  cache->insertSimple(object3a->getObjectId(), object3a);
  cache->insertSimple(object3b->getObjectId(), object3b);
  cache->insertSimple(object3c->getObjectId(), object3c);

  EXPECT_FALSE(cache->contains(object3->getObjectId()));
  EXPECT_EQ(
      std::shared_ptr<CacheObject>{nullptr},
      cache->getSimple(object3->getObjectId()));
  EXPECT_TRUE(cache->contains(object3a->getObjectId()));
  EXPECT_EQ(object3a, cache->getSimple(object3a->getObjectId()));
  EXPECT_TRUE(cache->contains(object3b->getObjectId()));
  EXPECT_EQ(object3b, cache->getSimple(object3b->getObjectId()));
  EXPECT_TRUE(cache->contains(object3c->getObjectId()));
  EXPECT_EQ(object3c, cache->getSimple(object3c->getObjectId()));
}

TEST(ObjectCache, testLRUSimpleInsert) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          10, 1, makeRefPtr<EdenStats>());

  cache->insertSimple(object3->getObjectId(), object3);
  cache->insertSimple(object3a->getObjectId(), object3a);
  cache->insertSimple(object3b->getObjectId(), object3b);

  cache->getSimple(object3->getObjectId()); // object3 should not be evicted now

  cache->insertSimple(object3c->getObjectId(), object3c);

  EXPECT_TRUE(cache->contains(object3->getObjectId()));
  EXPECT_EQ(object3, cache->getSimple(object3->getObjectId()));
  EXPECT_FALSE(cache->contains(object3a->getObjectId()));
  EXPECT_EQ(
      std::shared_ptr<CacheObject>{nullptr},
      cache->getSimple(object3a->getObjectId()));
  EXPECT_TRUE(cache->contains(object3b->getObjectId()));
  EXPECT_EQ(object3b, cache->getSimple(object3b->getObjectId()));
  EXPECT_TRUE(cache->contains(object3c->getObjectId()));
  EXPECT_EQ(object3c, cache->getSimple(object3c->getObjectId()));
}

TEST(ObjectCache, testLargeInsert) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          10, 1, makeRefPtr<EdenStats>());

  cache->insertSimple(object11->getObjectId(), object11);

  EXPECT_TRUE(cache->contains(object11->getObjectId()));
  EXPECT_EQ(object11, cache->getSimple(object11->getObjectId()));
}

TEST(ObjectCache, testSizeOverflowLargeInsert) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          10, 1, makeRefPtr<EdenStats>());

  cache->insertSimple(object3->getObjectId(), object3);
  cache->insertSimple(object3a->getObjectId(), object3a);
  cache->insertSimple(object3b->getObjectId(), object3b);
  cache->insertSimple(object11->getObjectId(), object11);

  EXPECT_FALSE(cache->contains(object3->getObjectId()));
  EXPECT_EQ(
      std::shared_ptr<const CacheObject>{nullptr},
      cache->getSimple(object3->getObjectId()));
  EXPECT_FALSE(cache->contains(object3a->getObjectId()));
  EXPECT_EQ(
      std::shared_ptr<const CacheObject>{nullptr},
      cache->getSimple(object3a->getObjectId()));
  EXPECT_FALSE(cache->contains(object3b->getObjectId()));
  EXPECT_EQ(
      std::shared_ptr<const CacheObject>{nullptr},
      cache->getSimple(object3b->getObjectId()));
  EXPECT_TRUE(cache->contains(object11->getObjectId()));
  EXPECT_EQ(object11, cache->getSimple(object11->getObjectId()));
}

TEST(ObjectCache, testDuplicateInsert) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          10, 1, makeRefPtr<EdenStats>());

  cache->insertSimple(object3->getObjectId(), object3);
  cache->insertSimple(object3a->getObjectId(), object3a);
  cache->insertSimple(object3b->getObjectId(), object3b);

  cache->insertSimple(
      object3->getObjectId(), object3); // object3 should not be evicted now

  cache->insertSimple(object3c->getObjectId(), object3c);

  EXPECT_TRUE(cache->contains(object3->getObjectId()));
  EXPECT_EQ(object3, cache->getSimple(object3->getObjectId()));
  EXPECT_FALSE(cache->contains(object3a->getObjectId()));
  EXPECT_EQ(
      std::shared_ptr<CacheObject>{nullptr},
      cache->getSimple(object3a->getObjectId()));
  EXPECT_TRUE(cache->contains(object3b->getObjectId()));
  EXPECT_EQ(object3b, cache->getSimple(object3b->getObjectId()));
  EXPECT_TRUE(cache->contains(object3c->getObjectId()));
  EXPECT_EQ(object3c, cache->getSimple(object3c->getObjectId()));
}

TEST(ObjectCache, testReinsert) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          10, 1, makeRefPtr<EdenStats>());

  cache->insertSimple(object3->getObjectId(), object3);
  cache->insertSimple(object3a->getObjectId(), object3a);
  cache->insertSimple(object3b->getObjectId(), object3b);
  cache->insertSimple(object3c->getObjectId(), object3c);
  cache->insertSimple(object3->getObjectId(), object3);

  EXPECT_TRUE(cache->contains(object3->getObjectId()));
  EXPECT_EQ(object3, cache->getSimple(object3->getObjectId()));
  EXPECT_FALSE(cache->contains(object3a->getObjectId()));
  EXPECT_EQ(
      std::shared_ptr<CacheObject>{nullptr},
      cache->getSimple(object3a->getObjectId()));
  EXPECT_TRUE(cache->contains(object3b->getObjectId()));
  EXPECT_EQ(object3b, cache->getSimple(object3b->getObjectId()));
  EXPECT_TRUE(cache->contains(object3c->getObjectId()));
  EXPECT_EQ(object3c, cache->getSimple(object3c->getObjectId()));
}

/**
 * Interest-handle test cases
 */

TEST(ObjectCache, interest_handle_evicts_oldest_on_insertion) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          create(10, 0, makeRefPtr<EdenStats>());
  cache->insertInterestHandle(object3->getObjectId(), object3);
  cache->insertInterestHandle(
      object4->getObjectId(),
      object4); // object4 is considered more recent than object3
  EXPECT_EQ(7, cache->getTotalSizeBytes());
  cache->insertInterestHandle(
      object5->getObjectId(), object5); // evicts object3
  EXPECT_EQ(9, cache->getTotalSizeBytes());
  EXPECT_EQ(nullptr, cache->getInterestHandle(id3).object)
      << "Inserting object5 should evict oldest (object3)";
  EXPECT_EQ(object4, cache->getInterestHandle(id4).object)
      << "But object4 still fits";
  cache->insertInterestHandle(
      object3->getObjectId(), object3); // evicts object5
  EXPECT_EQ(7, cache->getTotalSizeBytes());
  EXPECT_EQ(nullptr, cache->getInterestHandle(id5).object)
      << "Inserting object3 again evicts object5 because object4 was accessed";
  EXPECT_EQ(object4, cache->getInterestHandle(id4).object);
}

TEST(
    ObjectCache,
    interest_handle_inserting_large_object_evicts_multiple_small_objects) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          create(10, 0, makeRefPtr<EdenStats>());
  cache->insertInterestHandle(object3->getObjectId(), object3);
  cache->insertInterestHandle(object4->getObjectId(), object4);
  cache->insertInterestHandle(object9->getObjectId(), object9);
  EXPECT_FALSE(cache->getInterestHandle(id3).object);
  EXPECT_FALSE(cache->getInterestHandle(id4).object);
  EXPECT_EQ(object9, cache->getInterestHandle(id9).object);
}

TEST(
    ObjectCache,
    interest_handle_inserting_existing_object_moves_it_to_back_of_eviction_queue) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          create(8, 0, makeRefPtr<EdenStats>());
  cache->insertInterestHandle(object3->getObjectId(), object3);
  cache->insertInterestHandle(object4->getObjectId(), object4);
  cache->insertInterestHandle(object3->getObjectId(), object3);
  cache->insertInterestHandle(object5->getObjectId(), object5); // evicts 4

  EXPECT_EQ(object3, cache->getInterestHandle(id3).object);
  EXPECT_FALSE(cache->getInterestHandle(id4).object);
  EXPECT_EQ(object5, cache->getInterestHandle(id5).object);
}

TEST(
    ObjectCache,
    interest_handle_preserves_minimum_number_of_entries_despite_exceeding_size_limit) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          create(1, 3, makeRefPtr<EdenStats>());
  cache->insertInterestHandle(object3->getObjectId(), object3);
  cache->insertInterestHandle(object4->getObjectId(), object4);
  cache->insertInterestHandle(object5->getObjectId(), object5);

  EXPECT_EQ(12, cache->getTotalSizeBytes());
  EXPECT_TRUE(cache->getInterestHandle(id3).object);
  EXPECT_TRUE(cache->getInterestHandle(id4).object);
  EXPECT_TRUE(cache->getInterestHandle(id5).object);
}

TEST(ObjectCache, interest_handle_preserves_minimum_number_of_entries) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          create(1, 3, makeRefPtr<EdenStats>());
  cache->insertInterestHandle(object3->getObjectId(), object3);
  cache->insertInterestHandle(object4->getObjectId(), object4);
  cache->insertInterestHandle(object5->getObjectId(), object5);
  cache->insertInterestHandle(object6->getObjectId(), object6);

  EXPECT_EQ(15, cache->getTotalSizeBytes());
  EXPECT_FALSE(cache->getInterestHandle(id3).object);
  EXPECT_TRUE(cache->getInterestHandle(id4).object);
  EXPECT_TRUE(cache->getInterestHandle(id5).object);
  EXPECT_TRUE(cache->getInterestHandle(id6).object);
}

TEST(ObjectCache, interest_handle_can_forget_cached_entries) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          create(100, 0, makeRefPtr<EdenStats>());
  auto handle3 = cache->insertInterestHandle(
      id3,
      std::make_shared<CacheObject>(id3, 3),
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::WantHandle);
  auto handle4 = cache->insertInterestHandle(
      id4,
      std::make_shared<CacheObject>(id4, 4),
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::WantHandle);

  // The use of WantHandle causes these reset() calls to evict from the cache.
  handle3.reset();
  handle4.reset();

  EXPECT_FALSE(cache->getInterestHandle(id3).object);
  EXPECT_FALSE(cache->getInterestHandle(id4).object);
}

TEST(
    ObjectCache,
    interest_handle_can_forget_cached_entries_in_reverse_insertion_order) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          create(100, 0, makeRefPtr<EdenStats>());
  auto handle3 = cache->insertInterestHandle(
      id3,
      std::make_shared<CacheObject>(id3, 3),
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::WantHandle);
  auto handle4 = cache->insertInterestHandle(
      id4,
      std::make_shared<CacheObject>(id4, 4),
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::WantHandle);

  handle4.reset();
  handle3.reset();

  EXPECT_FALSE(cache->getInterestHandle(id3).object);
  EXPECT_FALSE(cache->getInterestHandle(id4).object);
}

TEST(ObjectCache, interest_handle_can_forget_cached_entry_in_middle) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          create(100, 0, makeRefPtr<EdenStats>());
  auto handle3 = cache->insertInterestHandle(
      id3,
      std::make_shared<CacheObject>(id3, 3),
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::WantHandle);
  auto handle4 = cache->insertInterestHandle(
      id4,
      std::make_shared<CacheObject>(id4, 4),
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::WantHandle);
  auto handle5 = cache->insertInterestHandle(
      id5,
      std::make_shared<CacheObject>(id5, 5),
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::WantHandle);

  handle4.reset();

  EXPECT_TRUE(cache->getInterestHandle(id3).object);
  EXPECT_FALSE(cache->getInterestHandle(id4).object);
  EXPECT_TRUE(cache->getInterestHandle(id5).object);
}

TEST(
    ObjectCache,
    interest_handle_duplicate_insertion_with_interest_forgets_on_last_drop) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          create(100, 0, makeRefPtr<EdenStats>());
  auto object = std::make_shared<CacheObject>(id3, 3);
  auto weak = std::weak_ptr<CacheObject>{object};
  auto handle1 = cache->insertInterestHandle(
      object->getObjectId(),
      object,
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::WantHandle);
  auto handle2 = cache->insertInterestHandle(
      object->getObjectId(),
      object,
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::WantHandle);
  object.reset();

  EXPECT_TRUE(weak.lock());
  handle1.reset();
  EXPECT_TRUE(weak.lock());
  handle2.reset();
  EXPECT_FALSE(weak.lock());
}

TEST(
    ObjectCache,
    interest_handle_does_not_forget_object_until_last_handle_is_forgotten) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          create(100, 0, makeRefPtr<EdenStats>());
  cache->insertInterestHandle(
      id6,
      std::make_shared<CacheObject>(id6, 6),
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::UnlikelyNeededAgain);
  auto result1 = cache->getInterestHandle(
      id6,
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::WantHandle);
  auto result2 = cache->getInterestHandle(
      id6,
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::WantHandle);
  EXPECT_TRUE(result1.object);
  EXPECT_TRUE(result2.object);
  EXPECT_EQ(result1.object, result2.object);

  auto weak = std::weak_ptr<const CacheObject>{result1.object};
  result1.object.reset();
  result2.object.reset();
  EXPECT_TRUE(weak.lock());

  result1.interestHandle.reset();
  EXPECT_TRUE(weak.lock());

  result2.interestHandle.reset();
  EXPECT_FALSE(weak.lock());
}

TEST(ObjectCache, interest_handle_redundant_inserts_are_ignored) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          create(10, 0, makeRefPtr<EdenStats>());
  auto object = std::make_shared<CacheObject>(ObjectId{}, 9);
  cache->insertInterestHandle(object->getObjectId(), object);
  EXPECT_EQ(9, cache->getTotalSizeBytes());
  cache->insertInterestHandle(object->getObjectId(), object);
  EXPECT_EQ(9, cache->getTotalSizeBytes());
  cache->insertInterestHandle(object->getObjectId(), object);
  EXPECT_EQ(9, cache->getTotalSizeBytes());
}

TEST(
    ObjectCache,
    interest_handle_redundant_insert_does_not_invalidate_handles) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          create(10, 0, makeRefPtr<EdenStats>());
  auto handle3 = cache->insertInterestHandle(
      object3->getObjectId(),
      object3,
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::WantHandle);
  cache->insertInterestHandle(
      object3->getObjectId(),
      object3,
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::WantHandle);
  EXPECT_TRUE(handle3.getObject());
}

TEST(
    ObjectCache,
    interest_handle_fetching_object_from_handle_moves_to_back_of_eviction_queue) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          create(10, 0, makeRefPtr<EdenStats>());
  auto handle3 = cache->insertInterestHandle(
      id3,
      std::make_shared<CacheObject>(id3, 3),
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::WantHandle);
  auto handle4 = cache->insertInterestHandle(
      id4,
      std::make_shared<CacheObject>(id4, 4),
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::WantHandle);

  // Normally, inserting object5 would cause object3 to get evicted since it was
  // the first one inserted. Access object3 through its interest handle.
  EXPECT_TRUE(handle3.getObject());
  cache->insertInterestHandle(object5->getObjectId(), object5);
  EXPECT_TRUE(handle3.getObject());
  EXPECT_EQ(nullptr, handle4.getObject());
}

TEST(ObjectCache, interest_handle_can_return_object_even_if_it_was_evicted) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          create(10, 0, makeRefPtr<EdenStats>());
  // Insert multiple objects that are never collected. Also, don't ask for
  // scoped interest.
  auto handle3 = cache->insertInterestHandle(object3->getObjectId(), object3);
  auto handle4 = cache->insertInterestHandle(object4->getObjectId(), object4);
  auto handle5 = cache->insertInterestHandle(object5->getObjectId(), object5);

  EXPECT_FALSE(cache->getInterestHandle(id3).object)
      << "Inserting object5 evicts object3";
  EXPECT_EQ(object3, handle3.getObject())
      << "Object accessible even though it's been evicted";
  EXPECT_EQ(object4, handle4.getObject());
  EXPECT_EQ(object5, handle5.getObject());
}

TEST(
    ObjectCache,
    interest_handle_dropping_does_not_evict_if_item_has_been_reloaded_after_clear) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          create(10, 0, makeRefPtr<EdenStats>());
  auto handle3 = cache->insertInterestHandle(
      object3->getObjectId(),
      object3,
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::WantHandle);
  cache->clear();
  cache->insertInterestHandle(object3->getObjectId(), object3);
  handle3.reset();
  EXPECT_TRUE(cache->contains(id3));
}

TEST(
    ObjectCache,
    dropping_interest_handle_does_not_evict_if_item_has_been_reloaded_after_eviction) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          create(10, 0, makeRefPtr<EdenStats>());
  auto handle3 = cache->insertInterestHandle(
      object3->getObjectId(),
      object3,
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::WantHandle);
  cache->insertInterestHandle(object4->getObjectId(), object4);
  cache->insertInterestHandle(object5->getObjectId(), object5);
  auto handle3again = cache->insertInterestHandle(
      object3->getObjectId(),
      object3,
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::WantHandle);
  handle3.reset();
  EXPECT_TRUE(cache->contains(id3));
}

/**
 * Multi-shard test cases
 */

TEST(ObjectCache, multi_shard_basic_operations) {
  // Create cache with 4 shards, large enough to hold all test objects
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          400, 40, makeRefPtr<EdenStats>(), 4);

  // Insert objects and verify they can be retrieved
  cache->insertSimple(object3->getObjectId(), object3);
  cache->insertSimple(object4->getObjectId(), object4);
  cache->insertSimple(object5->getObjectId(), object5);

  EXPECT_TRUE(cache->contains(object3->getObjectId()));
  EXPECT_TRUE(cache->contains(object4->getObjectId()));
  EXPECT_TRUE(cache->contains(object5->getObjectId()));
  EXPECT_EQ(object3, cache->getSimple(object3->getObjectId()));
  EXPECT_EQ(object4, cache->getSimple(object4->getObjectId()));
  EXPECT_EQ(object5, cache->getSimple(object5->getObjectId()));
}

TEST(ObjectCache, multi_shard_total_size_aggregation) {
  // Create cache with 4 shards, large enough to avoid evictions
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          400, 40, makeRefPtr<EdenStats>(), 4);

  cache->insertSimple(object3->getObjectId(), object3);
  cache->insertSimple(object4->getObjectId(), object4);
  cache->insertSimple(object5->getObjectId(), object5);

  // Total size should be sum across all shards
  EXPECT_EQ(12, cache->getTotalSizeBytes());
}

TEST(ObjectCache, multi_shard_object_count_aggregation) {
  // Create cache with 4 shards, large enough to avoid evictions
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          400, 40, makeRefPtr<EdenStats>(), 4);

  cache->insertSimple(object3->getObjectId(), object3);
  cache->insertSimple(object4->getObjectId(), object4);
  cache->insertSimple(object5->getObjectId(), object5);

  // Total count should be sum across all shards
  EXPECT_EQ(3, cache->getObjectCount());
}

TEST(ObjectCache, multi_shard_clear) {
  // Create cache with 4 shards
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          400, 40, makeRefPtr<EdenStats>(), 4);

  cache->insertSimple(object3->getObjectId(), object3);
  cache->insertSimple(object4->getObjectId(), object4);
  cache->insertSimple(object5->getObjectId(), object5);

  EXPECT_EQ(3, cache->getObjectCount());

  cache->clear();

  // All objects should be gone across all shards
  EXPECT_EQ(0, cache->getObjectCount());
  EXPECT_EQ(0, cache->getTotalSizeBytes());
  EXPECT_FALSE(cache->contains(object3->getObjectId()));
  EXPECT_FALSE(cache->contains(object4->getObjectId()));
  EXPECT_FALSE(cache->contains(object5->getObjectId()));
}

TEST(ObjectCache, multi_shard_eviction_with_minimum_entry_count) {
  // Create cache with 4 shards, verify minimum entry count prevents eviction
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          40, 4, makeRefPtr<EdenStats>(), 4);

  // Insert an 11-byte object - even though it exceeds per-shard limit (10
  // bytes), minimum entry count ensures it stays cached
  cache->insertSimple(object11->getObjectId(), object11);

  // The large object should be in cache
  EXPECT_TRUE(cache->contains(object11->getObjectId()));
  EXPECT_EQ(11, cache->getTotalSizeBytes());
}

TEST(ObjectCache, multi_shard_interest_handle_basic) {
  // Create cache with 4 shards, verify interest handles work correctly
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          create(400, 4, makeRefPtr<EdenStats>(), 4);

  auto handle3 = cache->insertInterestHandle(
      object3->getObjectId(),
      object3,
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          Interest::WantHandle);

  // Object should be retrievable
  EXPECT_TRUE(cache->getInterestHandle(id3).object);

  // Drop the handle - object should remain due to minimum entry count
  handle3.reset();

  // Still present due to minimum entry count
  EXPECT_TRUE(cache->getInterestHandle(id3).object);
}

TEST(ObjectCache, multi_shard_size_limit_enforcement) {
  // Create cache with 2 shards and max size of 10x object size
  // Each shard gets 5x object size as its limit
  constexpr size_t objectSize = 3;
  constexpr size_t numShards = 2;
  constexpr size_t maxSize = 10 * objectSize;
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          maxSize, 0, makeRefPtr<EdenStats>(), numShards);

  // Insert 100 objects of size 3 each
  for (size_t i = 0; i < 100; ++i) {
    auto id = ObjectId::sha1(
        folly::StringPiece{reinterpret_cast<const char*>(&i), sizeof(i)});
    auto obj = std::make_shared<CacheObject>(id, objectSize);
    cache->insertSimple(id, obj);
  }

  // Total cache size should be between 5x and 10x the object size
  // 5x if all objects go to one shard (that shard holds ~5 objects)
  // 10x if objects are evenly distributed (each shard holds ~5 objects)
  size_t totalSize = cache->getTotalSizeBytes();
  EXPECT_GE(totalSize, 5 * objectSize)
      << "Cache should hold at least 5 objects (worst case: all in one shard)";
  EXPECT_LE(totalSize, 10 * objectSize)
      << "Cache should not exceed size limit (best case: evenly distributed)";
}

TEST(ObjectCache, evictionSubtractsTheSizeRecordedAtInsert) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          10, 0, makeRefPtr<EdenStats>());
  auto growingObject = std::make_shared<CacheObject>(id3, 3);

  cache->insertSimple(id3, growingObject);
  growingObject->setSizeBytes(100);
  cache->insertSimple(id9, object9);

  EXPECT_FALSE(cache->contains(id3));
  EXPECT_EQ(9, cache->getTotalSizeBytes());
}

TEST(ObjectCache, evictedObjectIsDestroyedAfterTheLockIsReleased) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          10, 0, makeRefPtr<EdenStats>());
  // The deleter locks the cache. It can only run once the eviction has
  // released the lock; a deleter run under the lock would deadlock here.
  size_t objectCountSeenByDeleter = 0;
  cache->insertSimple(
      id3,
      std::shared_ptr<const CacheObject>{
          new CacheObject{id3, 3}, [&](const CacheObject* object) {
            objectCountSeenByDeleter = cache->getObjectCount();
            delete object;
          }});

  cache->insertSimple(id9, object9);

  EXPECT_EQ(1, objectCountSeenByDeleter);
}

#ifndef _WIN32
TEST(
    ObjectCacheDeathTest,
    insertionExceptionReleasesInterestHandleAfterUnlock) {
  GTEST_FLAG_SET(death_test_style, "threadsafe");
  using Cache = ObjectCache<
      CacheObject,
      ObjectCacheFlavor::InterestHandle,
      ThrowingEvictionStats>;
  // FIXME: Expect ExitedWithCode(0) once insertion releases the shard lock
  // before destroying its interest handle during exception unwinding.
  ASSERT_EXIT(
      {
        std::signal(SIGALRM, SIG_DFL);
        alarm(10);
        auto cache = Cache::create(10, 0, makeRefPtr<EdenStats>());
        cache->insertInterestHandle(id3, object3);
        EXPECT_THROW(
            cache->insertInterestHandle(
                id9, object9, Cache::Interest::WantHandle),
            std::bad_alloc);
        EXPECT_FALSE(cache->contains(id9));
        EXPECT_EQ(0, cache->getObjectCount());
        EXPECT_EQ(0, cache->getTotalSizeBytes());
        std::_Exit(::testing::Test::HasFailure() ? 1 : 0);
      },
      ::testing::KilledBySignal(SIGALRM),
      "throwing during eviction");
}
#endif

TEST(ObjectCache, objectWithFutureExpiryIsServed) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          10, 0, makeRefPtr<EdenStats>());

  cache->insertSimple(
      id3, object3, std::chrono::steady_clock::now() + std::chrono::hours{1});

  EXPECT_TRUE(cache->contains(id3));
  EXPECT_EQ(object3, cache->getSimple(id3));
}

TEST(ObjectCache, expiredObjectIsRemovedOnLookup) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          10, 0, makeRefPtr<EdenStats>());

  cache->insertSimple(
      id3, object3, std::chrono::steady_clock::now() - std::chrono::hours{1});

  EXPECT_FALSE(cache->contains(id3));
  EXPECT_EQ(nullptr, cache->getSimple(id3));
  EXPECT_EQ(0, cache->getObjectCount());
  EXPECT_EQ(0, cache->getTotalSizeBytes());
}

TEST(ObjectCache, expiredObjectIsReplacedOnInsert) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          10, 0, makeRefPtr<EdenStats>());
  auto stale = std::make_shared<CacheObject>(id3, 3);
  auto fresh = std::make_shared<CacheObject>(id3, 4);

  cache->insertSimple(
      id3, stale, std::chrono::steady_clock::now() - std::chrono::hours{1});
  cache->insertSimple(
      id3, fresh, std::chrono::steady_clock::now() + std::chrono::hours{1});

  EXPECT_EQ(fresh, cache->getSimple(id3));
  EXPECT_EQ(4, cache->getTotalSizeBytes());
}

TEST(ObjectCache, duplicateInsertKeepsTheLiveEntryAndItsExpiry) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::Simple, FakeStats>::create(
          10, 0, makeRefPtr<EdenStats>());
  auto first = std::make_shared<CacheObject>(id3, 3);
  auto second = std::make_shared<CacheObject>(id3, 3);

  cache->insertSimple(
      id3, first, std::chrono::steady_clock::now() + std::chrono::hours{1});
  cache->insertSimple(
      id3, second, std::chrono::steady_clock::now() - std::chrono::hours{1});

  EXPECT_EQ(first, cache->getSimple(id3));
}

TEST(ObjectCache, interest_handle_object_larger_than_cache_is_not_kept) {
  using Cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>;
  for (auto interest :
       {Cache::Interest::WantHandle,
        Cache::Interest::LikelyNeededAgain,
        Cache::Interest::UnlikelyNeededAgain}) {
    SCOPED_TRACE(static_cast<int>(interest));
    auto cache = Cache::create(10, 0, makeRefPtr<EdenStats>());

    auto handle = cache->insertInterestHandle(id11, object11, interest);

    EXPECT_FALSE(cache->contains(id11));
    EXPECT_EQ(0, cache->getTotalSizeBytes());
    EXPECT_EQ(object11, handle.getObject());
  }
}

TEST(ObjectCache, interest_handle_object_at_maximum_size_is_kept) {
  auto cache =
      ObjectCache<CacheObject, ObjectCacheFlavor::InterestHandle, FakeStats>::
          create(10, 0, makeRefPtr<EdenStats>());
  auto object10 = std::make_shared<CacheObject>(id11, 10);

  cache->insertInterestHandle(id11, object10);

  EXPECT_EQ(object10, cache->getInterestHandle(id11).object);
  EXPECT_EQ(10, cache->getTotalSizeBytes());
}
