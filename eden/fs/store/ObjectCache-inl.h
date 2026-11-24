/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include <folly/MapUtil.h>
#include <folly/logging/xlog.h>
#include <utility>

#include "eden/common/utils/IDGen.h"
#include "eden/fs/store/ObjectCache.h"

namespace facebook::eden {

template <typename ObjectType, typename ObjectCacheStats>
ObjectInterestHandle<ObjectType, ObjectCacheStats>::ObjectInterestHandle(
    std::weak_ptr<ObjectCache<
        ObjectType,
        ObjectCacheFlavor::InterestHandle,
        ObjectCacheStats>> objectCache,
    ObjectId id,
    std::weak_ptr<const ObjectType> object,
    uint64_t generation) noexcept
    : objectCache_{std::move(objectCache)},
      id_{std::move(id)},
      object_{std::move(object)},
      cacheItemGeneration_{generation} {}

template <typename ObjectType, typename ObjectCacheStats>
void ObjectInterestHandle<ObjectType, ObjectCacheStats>::reset() noexcept {
  if (auto objectCache = objectCache_.lock()) {
    objectCache->dropInterestHandle(id_, cacheItemGeneration_);
  }
  objectCache_.reset();
}

template <typename ObjectType, typename ObjectCacheStats>
std::shared_ptr<const ObjectType>
ObjectInterestHandle<ObjectType, ObjectCacheStats>::getObject() const {
  auto objectCache = objectCache_.lock();
  if (objectCache) {
    // UnlikelyNeededAgain because there's no need to create a new interest
    // handle nor bump the refcount.
    auto object = objectCache
                      ->getInterestHandle(
                          id_,
                          ObjectCache<
                              ObjectType,
                              ObjectCacheFlavor::InterestHandle,
                              ObjectCacheStats>::Interest::UnlikelyNeededAgain)
                      .object;
    if (object) {
      return object;
    }
  }

  // If the object is no longer in cache, at least see if it's still in memory.
  return object_.lock();
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
std::shared_ptr<ObjectCache<ObjectType, Flavor, ObjectCacheStats>>
ObjectCache<ObjectType, Flavor, ObjectCacheStats>::create(
    size_t maximumCacheSizeBytes,
    size_t minimumEntryCount,
    EdenStatsPtr stats,
    size_t shards) {
  // Allow make_shared with private constructor.
  struct OC : ObjectCache<ObjectType, Flavor, ObjectCacheStats> {
    OC(size_t x, size_t y, EdenStatsPtr stats, size_t shards)
        : ObjectCache<ObjectType, Flavor, ObjectCacheStats>{
              x,
              y,
              std::move(stats),
              shards} {}
  };
  return std::make_shared<OC>(
      maximumCacheSizeBytes, minimumEntryCount, std::move(stats), shards);
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
ObjectCache<ObjectType, Flavor, ObjectCacheStats>::ObjectCache(
    size_t maximumCacheSizeBytes,
    size_t minimumEntryCount,
    EdenStatsPtr stats,
    size_t shards)
    : stats_{std::move(stats)} {
  XCHECK_GT(shards, 0u);

  // Split the maximum cache size evenly among shards.
  size_t perShardMaxSize = maximumCacheSizeBytes / shards;

  // Split the minimum entry count among shards, but ensure each shard
  // gets at least 1 if the total is > 0.
  size_t perShardMinCount = minimumEntryCount / shards;
  if (minimumEntryCount > 0 && perShardMinCount == 0) {
    perShardMinCount = 1;
  }

  shards_.reserve(shards);
  for (size_t i = 0; i < shards; ++i) {
    shards_.emplace_back(std::in_place, perShardMaxSize, perShardMinCount);
  }
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
template <ObjectCacheFlavor F>
typename std::enable_if_t<
    F == ObjectCacheFlavor::InterestHandle,
    typename ObjectCache<ObjectType, Flavor, ObjectCacheStats>::GetResult>
ObjectCache<ObjectType, Flavor, ObjectCacheStats>::getInterestHandle(
    const ObjectId& id,
    Interest interest) {
  XLOGF(DBG6, "ObjectCache::getInterestHandle {}", id);
  // Acquires ObjectCache's lock upon destruction by calling dropInterestHandle,
  // so ensure that, if an exception is thrown below, the ~ObjectInterestHandle
  // runs after the lock is released.

  if (interest == Interest::None) {
    return GetResult{};
  }
  EvictedObjects evicted;
  auto state = getShard(id).lock();
  return getInterestHandleCore(state, id, interest, evicted);
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
template <ObjectCacheFlavor F>
typename std::enable_if_t<
    F == ObjectCacheFlavor::InterestHandle,
    typename ObjectCache<ObjectType, Flavor, ObjectCacheStats>::GetResult>
ObjectCache<ObjectType, Flavor, ObjectCacheStats>::getInterestHandleCore(
    LockedState& state,
    const ObjectId& id,
    Interest interest,
    EvictedObjects& evicted) noexcept {
  ObjectInterestHandle<ObjectType, ObjectCacheStats> interestHandle;
  auto item = getImpl(id, *state, evicted);
  if (!item) {
    return GetResult{};
  }

  switch (interest) {
    case Interest::UnlikelyNeededAgain:
      interestHandle.object_ = item->object;
      break;
    case Interest::WantHandle:
      interestHandle = ObjectInterestHandle<ObjectType, ObjectCacheStats>{
          this->shared_from_this(), id, item->object, item->generation};
      ++item->referenceCount;
      break;
    case Interest::LikelyNeededAgain:
      interestHandle.object_ = item->object;
      // Bump the reference count without allocating an interest handle - this
      // will cause the reference count to never reach zero, avoiding early
      // eviction.
      //
      // TODO: One possible optimization here is to set a bit (reference count
      // to UINT64_MAX) after which new interest handles never need to be
      // created.
      ++item->referenceCount;
      break;
    case Interest::None:
      break;
  }
  return GetResult{item->object, std::move(interestHandle)};
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
template <ObjectCacheFlavor F>
typename std::enable_if_t<
    F == ObjectCacheFlavor::Simple,
    typename ObjectCache<ObjectType, Flavor, ObjectCacheStats>::ObjectPtr>
ObjectCache<ObjectType, Flavor, ObjectCacheStats>::getSimple(
    const ObjectId& id) {
  XLOGF(DBG6, "ObjectCache::getSimple {}", id);
  EvictedObjects evicted;
  auto state = getShard(id).lock();

  if (auto item = getImpl(id, *state, evicted)) {
    return item->object;
  }
  return nullptr;
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
typename ObjectCache<ObjectType, Flavor, ObjectCacheStats>::CacheItem*
ObjectCache<ObjectType, Flavor, ObjectCacheStats>::getImpl(
    const ObjectId& id,
    State& state,
    EvictedObjects& evicted) {
  XLOGF(DBG6, "ObjectCache::getImpl {}", id);
  auto* item = folly::get_ptr(state.items, id);
  if (item && isExpired(*item)) {
    XLOGF(DBG6, "ObjectCache::getImpl expired {}", id);
    evictItem(state, *item, evicted);
    item = nullptr;
  }
  if (!item) {
    XLOG(DBG6, "ObjectCache::getImpl missed");
    stats_->increment(&ObjectCacheStats::getMiss);

  } else {
    XLOG(DBG6, "ObjectCache::getImpl hit");

    // TODO: Should we avoid promoting if interest is UnlikelyNeededAgain?
    // For now, we'll try not to be too clever.
    state.evictionQueue.splice(
        state.evictionQueue.end(),
        state.evictionQueue,
        state.evictionQueue.iterator_to(*item));
    stats_->increment(&ObjectCacheStats::getHit);
  }

  return item;
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
template <ObjectCacheFlavor F>
typename std::enable_if_t<
    F == ObjectCacheFlavor::InterestHandle,
    ObjectInterestHandle<ObjectType, ObjectCacheStats>>
ObjectCache<ObjectType, Flavor, ObjectCacheStats>::insertInterestHandle(
    ObjectId id,
    ObjectPtr object,
    Interest interest) {
  XLOGF(DBG6, "ObjectCache::insertInterestHandle {}", id);
  // Acquires ObjectCache's lock upon destruction by calling dropInterestHandle,
  // so ensure that, if an exception is thrown below, the ~ObjectInterestHandle
  // runs after the lock is released.
  auto preProcessRes = preProcessInterestHandle<F>(id, object, interest);

  if (interest == Interest::None) {
    return std::move(preProcessRes.interestHandle);
  }

  XLOGF(
      DBG6,
      " creating entry with generation={}",
      preProcessRes.cacheItemGeneration);

  auto size = object->getSizeBytes();
  EvictedObjects evicted;
  auto state = getShard(id).lock();
  return insertInterestHandleCore(
      std::move(id),
      std::move(object),
      size,
      interest,
      state,
      preProcessRes.cacheItemGeneration,
      std::move(preProcessRes.interestHandle),
      evicted);
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
template <ObjectCacheFlavor F>
typename std::enable_if_t<
    F == ObjectCacheFlavor::InterestHandle,
    typename ObjectCache<ObjectType, Flavor, ObjectCacheStats>::
        PreProcessInterestHandleResult>
ObjectCache<ObjectType, Flavor, ObjectCacheStats>::preProcessInterestHandle(
    ObjectId id,
    ObjectPtr object,
    Interest interest) {
  ObjectInterestHandle<ObjectType, ObjectCacheStats> interestHandle{};
  auto cacheItemGeneration = generateUniqueID();

  if (interest == Interest::WantHandle) {
    // This can throw, so do it before inserting into items.
    interestHandle = ObjectInterestHandle<ObjectType, ObjectCacheStats>{
        this->shared_from_this(),
        std::move(id),
        std::move(object),
        cacheItemGeneration};
  } else {
    interestHandle.object_ = std::move(object);
  }

  return {std::move(interestHandle), cacheItemGeneration};
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
template <ObjectCacheFlavor F>
typename std::enable_if_t<
    F == ObjectCacheFlavor::InterestHandle,
    ObjectInterestHandle<ObjectType, ObjectCacheStats>>
ObjectCache<ObjectType, Flavor, ObjectCacheStats>::insertInterestHandleCore(
    ObjectId id,
    ObjectPtr object,
    size_t size,
    Interest interest,
    LockedState& state,
    uint64_t cacheItemGeneration,
    ObjectInterestHandle<ObjectType, ObjectCacheStats> interestHandle,
    EvictedObjects& evicted) {
  auto [item, inserted] = insertImpl(
      std::move(id), std::move(object), size, kNeverExpires, *state, evicted);
  switch (interest) {
    case Interest::UnlikelyNeededAgain:
    case Interest::None:
      break;
    case Interest::WantHandle:
    case Interest::LikelyNeededAgain:
      ++item->referenceCount;
      break;
  }
  if (inserted) { // new entry we need to set the generation number
    item->generation = cacheItemGeneration;
    // This can evict the new entry itself, so item must not be used after it.
    evictUntilFits(*state, evicted);
  } else {
    XLOGF(DBG6, "duplicate entry, using generation {}", item->generation);
    // Inserting duplicate entry - use its generation.
    interestHandle.cacheItemGeneration_ = item->generation;
    // note we can skip eviction here because we didn't insert anything new,
    // so the cache size has not changed as a result of this operation.
    return interestHandle;
  }
  return interestHandle;
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
template <ObjectCacheFlavor F>
typename std::enable_if_t<F == ObjectCacheFlavor::Simple, void>
ObjectCache<ObjectType, Flavor, ObjectCacheStats>::insertSimple(
    ObjectId id,
    ObjectCache<ObjectType, Flavor, ObjectCacheStats>::ObjectPtr object,
    Clock::time_point expiresAt) {
  XLOGF(DBG6, "ObjectCache::insertSimple {}", id);
  auto size = object->getSizeBytes();
  EvictedObjects evicted;
  auto state = getShard(id).lock();
  if (insertImpl(
          std::move(id), std::move(object), size, expiresAt, *state, evicted)
          .second) {
    evictUntilFits(*state, evicted);
  }
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
std::pair<
    typename ObjectCache<ObjectType, Flavor, ObjectCacheStats>::CacheItem*,
    bool>
ObjectCache<ObjectType, Flavor, ObjectCacheStats>::insertImpl(
    ObjectId id,
    ObjectPtr object,
    size_t size,
    Clock::time_point expiresAt,
    State& state,
    EvictedObjects& evicted) {
  XLOGF(DBG6, "ObjectCache::insertImpl {}", id);

  // An expired entry gives way to the fresh object rather than being renewed.
  if (auto* expired = folly::get_ptr(state.items, id);
      expired && isExpired(*expired)) {
    evictItem(state, *expired, evicted);
  }

  ObjectId key = id;

  // the following should be no except

  auto [iter, inserted] = state.items.try_emplace(
      std::move(key),
      CacheItem{std::move(id), std::move(object), size, expiresAt});

  auto* itemPtr = &iter->second;
  if (inserted) {
    try {
      state.evictionQueue.push_back(*itemPtr);
    } catch (const std::exception&) {
      state.items.erase(iter);
      throw;
    }
    state.totalSize += size;
  } else {
    state.evictionQueue.splice(
        state.evictionQueue.end(),
        state.evictionQueue,
        state.evictionQueue.iterator_to(*itemPtr));
  }
  return std::make_pair(itemPtr, inserted);
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
bool ObjectCache<ObjectType, Flavor, ObjectCacheStats>::contains(
    const ObjectId& id) const {
  auto state = getShard(id).lock();
  auto* item = folly::get_ptr(state->items, id);
  return item != nullptr && !isExpired(*item);
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
void ObjectCache<ObjectType, Flavor, ObjectCacheStats>::clear() {
  XLOG(DBG6, "ObjectCache::clear");
  for (auto& shard : shards_) {
    auto state = shard.lock();
    state->totalSize = 0;
    state->evictionQueue.clear();
    state->items.clear();
  }
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
size_t ObjectCache<ObjectType, Flavor, ObjectCacheStats>::getTotalSizeBytes()
    const {
  size_t totalSize = 0;
  for (auto& shard : shards_) {
    auto state = shard.lock();
    totalSize += state->totalSize;
  }
  return totalSize;
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
size_t ObjectCache<ObjectType, Flavor, ObjectCacheStats>::getObjectCount()
    const {
  size_t objectCount = 0;
  for (auto& shard : shards_) {
    auto state = shard.lock();
    objectCount += state->items.size();
  }
  return objectCount;
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
typename ObjectCache<ObjectType, Flavor, ObjectCacheStats>::Stats
ObjectCache<ObjectType, Flavor, ObjectCacheStats>::getStats(
    const std::map<std::string, int64_t>& counters) const {
  Stats stats;

  // Aggregate stats from all shards.
  for (auto& shard : shards_) {
    auto state = shard.lock();
    stats.objectCount += state->items.size();
    stats.totalSizeInBytes += state->totalSize;
  }

  auto getCounterValue = [&counters](const std::string_view& name) -> int64_t {
    std::string_view kStatsCountSuffix{".count"};
    auto it = counters.find(fmt::format("{}{}", name, kStatsCountSuffix));
    if (it != counters.end()) {
      return it->second;
    } else {
      return 0;
    }
  };

  stats.hitCount = getCounterValue(stats_->getName(&ObjectCacheStats::getHit));
  stats.missCount =
      getCounterValue(stats_->getName(&ObjectCacheStats::getMiss));
  stats.evictionCount =
      getCounterValue(stats_->getName(&ObjectCacheStats::insertEviction));
  stats.dropCount =
      getCounterValue(stats_->getName(&ObjectCacheStats::objectDrop));
  return stats;
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
void ObjectCache<ObjectType, Flavor, ObjectCacheStats>::dropInterestHandle(
    const ObjectId& id,
    uint64_t generation) noexcept {
  XLOGF(DBG6, "dropInterestHandle {} generation={}", id, generation);
  EvictedObjects evicted;
  auto state = getShard(id).lock();

  auto* item = folly::get_ptr(state->items, id);
  if (!item) {
    // Cached item already evicted.
    return;
  }

  if (generation != item->generation) {
    // Item was evicted and re-added between creating and dropping the
    // interest handle.
    return;
  }

  if (item->referenceCount == 0) {
    XLOGF(
        WARN,
        "Reference count on item for {} was already zero: an exception must have been thrown during get()",
        id);
    return;
  }

  if (--item->referenceCount == 0) {
    stats_->increment(&ObjectCacheStats::objectDrop);
    evictItem(*state, *item, evicted);
  }
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
void ObjectCache<ObjectType, Flavor, ObjectCacheStats>::evictUntilFits(
    State& state,
    EvictedObjects& evicted) {
  XLOGF(
      DBG6,
      "ObjectCache::evictUntilFits state.totalSize={}, maximumCacheSizeBytes={}, evictionQueue.size()={}, minimumEntryCount={}",
      state.totalSize,
      state.maximumCacheSizeBytes,
      state.evictionQueue.size(),
      state.minimumEntryCount);
  while (state.totalSize > state.maximumCacheSizeBytes &&
         state.evictionQueue.size() > state.minimumEntryCount) {
    evictOne(state, evicted);
  }
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
void ObjectCache<ObjectType, Flavor, ObjectCacheStats>::evictOne(
    State& state,
    EvictedObjects& evicted) {
  evictItem(state, state.evictionQueue.front(), evicted);
  stats_->increment(&ObjectCacheStats::insertEviction);
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
void ObjectCache<ObjectType, Flavor, ObjectCacheStats>::evictItem(
    State& state,
    CacheItem& item,
    EvictedObjects& evicted) {
  XLOGF(
      DBG6,
      "ObjectCache::evictItem evicting {} generation={}",
      item.id,
      item.generation);
  // Growing the buffer is the only step that can throw, so it happens before
  // the entry is touched; a failure leaves the cache consistent.
  evicted.push_back(item.object);
  state.evictionQueue.erase(state.evictionQueue.iterator_to(item));
  state.totalSize -= item.size;
  state.items.erase(item.id);
}

template <
    typename ObjectType,
    ObjectCacheFlavor Flavor,
    typename ObjectCacheStats>
void ObjectCache<ObjectType, Flavor, ObjectCacheStats>::invalidate(
    const ObjectId& id) noexcept {
  XLOGF(DBG6, "ObjectCache::invalidate {}", id);
  EvictedObjects evicted;
  auto state = getShard(id).lock();

  if (auto item = getImpl(id, *state, evicted)) {
    evictItem(*state, *item, evicted);
  }
};
} // namespace facebook::eden
