/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/store/BlobCache.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/config/ReloadableConfig.h"

namespace facebook::eden {

static constexpr folly::StringPiece kBlobCacheMemory{"blob_cache.memory"};
static constexpr folly::StringPiece kBlobCacheItems{"blob_cache.items"};

ObjectCache<Blob, ObjectCacheFlavor::InterestHandle, BlobCacheStats>::GetResult
BlobCache::get(const ObjectId& id, Interest interest) {
  if (!enabled_) {
    interest = Interest::None;
  }
  auto handle = getInterestHandle(id, interest);
  if (handle.object) {
    stats_->increment(&ObjectStoreStats::getBlobFromMemory);
  }
  return handle;
}

BlobInterestHandle
BlobCache::insert(ObjectId id, ObjectPtr blob, Interest interest) {
  if (!enabled_) {
    interest = Interest::None;
  }
  return insertInterestHandle(std::move(id), std::move(blob), interest);
}

BlobCache::BlobCache(
    PrivateTag,
    std::shared_ptr<ReloadableConfig> config,
    EdenStatsPtr stats)
    // Copies config rather than moving it: MSVC initializes the shared_ptr
    // parameter before it evaluates the arguments to its left, which read it.
    : BlobCache{
          PrivateTag{},
          config->getEdenConfig()->inMemoryBlobCacheSize.getValue(),
          config->getEdenConfig()->inMemoryBlobCacheMinimumItems.getValue(),
          config,
          std::move(stats)} {}

BlobCache::BlobCache(
    PrivateTag,
    size_t maximumSize,
    size_t minimumCount,
    std::shared_ptr<ReloadableConfig> config,
    EdenStatsPtr stats)
    : ObjectCache<
          Blob,
          ObjectCacheFlavor::InterestHandle,
          BlobCacheStats>{maximumSize, minimumCount, stats.copy()},
      enabled_{config->getEdenConfig()->enableInMemoryBlobCaching.getValue()},
      stats_{std::move(stats)} {
  registerStats();
  if (!enabled_) {
    XLOG(DBG2, "In-memory blob caching is disabled due to configuration");
  }
}

BlobCache::~BlobCache() {
  auto counters = fb303::ServiceData::get()->getDynamicCounters();
  counters->unregisterCallback(kBlobCacheMemory);
  counters->unregisterCallback(kBlobCacheItems);
}

void BlobCache::registerStats() {
  auto counters = fb303::ServiceData::get()->getDynamicCounters();
  counters->registerCallback(
      kBlobCacheMemory, [this] { return getTotalSizeBytes(); });
  counters->registerCallback(
      kBlobCacheItems, [this] { return getObjectCount(); });
}

} // namespace facebook::eden
