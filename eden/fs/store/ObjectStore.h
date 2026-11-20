/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/Executor.h>
#include <folly/Synchronized.h>
#include <folly/coro/safe/NowTask.h>
#include <atomic>
#include <chrono>
#include <ctime>
#include <memory>
#include <unordered_map>

#include <folly/logging/xlog.h>
#include <folly/portability/GTestProd.h>

#include "eden/common/utils/CaseSensitivity.h"
#include "eden/common/utils/RefPtr.h"
#include "eden/fs/config/ReloadableConfig.h"
#include "eden/fs/config/RestrictedContentMode.h"
#include "eden/fs/model/BlobAuxData.h"
#include "eden/fs/model/Hash.h"
#include "eden/fs/model/RootId.h"
#include "eden/fs/model/TreeAuxData.h"
#include "eden/fs/model/TreeEntry.h"
#include "eden/fs/model/TreeFwd.h"
#include "eden/fs/store/BackingStore.h"
#include "eden/fs/store/IObjectStore.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/store/WalkDetector.h"
#include "eden/fs/utils/ShardedLruCache.h"

namespace facebook::eden {

class Blob;
class ReloadableConfig;
class EdenConfig;
class EdenStats;
class ProcessInfoCache;
class EdenFsEventsLogger;
class TreeCache;
enum class ObjectComparison : uint8_t;

using EdenStatsPtr = RefPtr<EdenStats>;

struct PidFetchCounts {
  // TODO(xavierd): Incrementing the count still requires multiple atomics: the
  // lock then the atomic. This could in theory be lowered based on the
  // observation that iterating through the map is safe from multiple threads as
  // long as no modification to it is done. Here, updating the count doesn't
  // modify the map, it merely modifies the stored value which wouldn't
  // invalidate iterators.
  //
  // A mechanism like RCU (https://en.wikipedia.org/wiki/Read-copy-update) like
  // from folly::AtomicReadMostlyMainPtr could be a way to achieve this.
  folly::Synchronized<std::unordered_map<ProcessId, std::atomic<uint64_t>>>
      map_;

  uint64_t recordProcessFetch(ProcessId pid) {
    // First, check if the pid is already in the map by taking the read lock.
    // Taking the read lock is much cheaper than the write lock as it's expected
    // that FS heavy application will live for a long time and thus the cost of
    // contending on the write lock will be minimized.
    {
      // It is safe to get a non-const reference to the value as the value is
      // atomic.
      auto rlock = map_.rlock();
      auto& map_lock = rlock.asNonConstUnsafe();
      if (auto fetch_count = map_lock.find(pid);
          fetch_count != map_lock.end()) {
        return fetch_count->second.fetch_add(1, std::memory_order_relaxed) + 1;
      }
    }

    // Then, if the pid isn't found, take the write lock and insert it.
    {
      auto map_lock = map_.wlock();
      auto [it, inserted] = map_lock->try_emplace(pid, 0);
      return it->second.fetch_add(1, std::memory_order_relaxed) + 1;
    }
  }

  void clear() {
    map_.wlock()->clear();
  }

  uint64_t getCountByPid(ProcessId pid) {
    auto rl = map_.rlock();
    auto fetch_count = rl->find(pid);
    if (fetch_count != rl->end()) {
      return fetch_count->second;
    } else {
      return 0;
    }
  }
};

/**
 * ObjectStore is a content-addressed store for eden object data.
 *
 * The ObjectStore class itself is primarily a wrapper around two other
 * underlying storage types:
 * - An in memory cache, for fast repeated queries,
 * - BackingStore, which represents the authoritative source for the object
 *   data.  The BackingStore is generally more expensive to query for object
 *   data, and may not be available during offline operation.
 *
 * ObjectStore also takes care of updating various stats and counters.
 */
class ObjectStore : public IObjectStore,
                    public RootIdCodec,
                    public ObjectIdCodec,
                    public std::enable_shared_from_this<ObjectStore> {
 public:
  /**
   * `walkDetector` is the mount's walk detector, which gets told about the
   * blobs and trees fetched from the backing store, or nullptr when the
   * backing store does not detect walks.
   */
  static std::shared_ptr<ObjectStore> create(
      std::shared_ptr<BackingStore> backingStore,
      std::shared_ptr<TreeCache> treeCache,
      EdenStatsPtr stats,
      std::shared_ptr<ProcessInfoCache> processInfoCache,
      std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger,
      std::shared_ptr<ReloadableConfig> edenConfig,
      CaseSensitivity caseSensitive,
      folly::Executor::KeepAlive<> executor,
      std::shared_ptr<WalkDetector> walkDetector = nullptr);
  ~ObjectStore() override;

  /**
   * When pid of fetchContext is available, this function updates
   * pidFetchCounts_. If the current process needs to be logged as
   * a fetch-heavy process, it sends a FetchHeavy event to Scuba.
   */
  void updateProcessFetch(const ObjectFetchContext& fetchContext) const;

  /**
   * send a FetchHeavy log event to Scuba. If either processInfoCache_
   * or edenFsEventsLogger_ is nullptr, this function does nothing.
   */
  void sendFetchHeavyEvent(ProcessId pid, uint64_t fetch_count) const;

  /**
   * Check fetch count of the process using this fetchContext before using
   * the fetchContext in BackingStore. if fetchHeavyThreshold in edenConfig_ is
   * exceeded, deprioritize the fetchContext by 1.
   *
   * Note: Normally, one fetchContext is created for only one fetch request,
   * so deprioritize() should only be called once by one thread, but that is
   * not strictly guaranteed. See comments before deprioritize() for more
   * information
   */
  void deprioritizeWhenFetchHeavy(ObjectFetchContext& context) const;

  /**
   * Each BackingStore implementation defines its interpretation of root IDs.
   * This function gives the BackingStore a chance to parse and canonicalize the
   * root ID at API boundaries such as Thrift.
   */
  RootId parseRootId(folly::StringPiece rootId) override;

  /**
   * Each BackingStore defines the meaning and encoding of its root ID. Give it
   * the chance to render a root ID to Thrift.
   */
  std::string renderRootId(const RootId& rootId) override;

  /**
   * Returns a human-readable string representation of the given RootId for
   * display purposes.
   */
  std::string displayRootId(const RootId& rootId);

  /**
   * Each BackingStore implementation defines its interpretation of object IDs.
   * This function gives the BackingStore a chance to parse and canonicalize the
   * object ID at API boundaries such as Thrift.
   */
  ObjectId parseObjectId(folly::StringPiece objectId) override;

  /**
   * Each BackingStore defines the meaning and encoding of its object ID. Give
   * it the chance to render a object ID to Thrift.
   */
  std::string renderObjectId(const ObjectId& objectId) override;

  /**
   * Get a root Tree.
   *
   * This returns a Future object that will produce the root Tree when it is
   * ready.  It may result in a std::domain_error if the specified commit ID
   * does not exist, or possibly other exceptions on error.
   *
   * DEPRECATED: use co_getRootTree directly. Futures wrapper kept for
   * non-coroutine callers; remove once all callers have migrated.
   */
  ImmediateFuture<GetRootTreeResult> getRootTree(
      const RootId& rootId,
      const ObjectFetchContextPtr& context) const override;

  folly::coro::now_task<GetRootTreeResult> co_getRootTree(
      const RootId& rootId,
      const ObjectFetchContextPtr& context) const;

  /**
   * Get a TreeEntry by ID
   *
   * This returns a Future object that will produce the TreeEntry when it is
   * ready. It may result in a std::domain_error if the specified tree ID does
   * not exist, or possibly other exceptions on error.
   */
  ImmediateFuture<std::shared_ptr<TreeEntry>> getTreeEntryForObjectId(
      const ObjectId& objectId,
      TreeEntryType treeEntryType,
      const ObjectFetchContextPtr& context) const;

  /**
   * Get a Tree by ID.
   *
   * This returns an ImmediateFuture object that will produce the Tree when it
   * is ready.  It may result in a std::domain_error if the specified tree ID
   * does not exist, or possibly other exceptions on error.
   */
  ImmediateFuture<TreePtr> getTree(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) const override;

  folly::coro::now_task<std::shared_ptr<const Tree>> co_getTree(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) const;

  /**
   * Get aux data about a tree.
   *
   * This returns an ImmediateFuture object that will produce optional
   * TreeAuxData when it is ready.  Nullopt is returned when TreeAuxData is
   * missing from all sources (including the BackingStore). This may return
   * other exceptions when TreeAuxData is available but other errors occurred.
   */
  ImmediateFuture<std::optional<TreeAuxData>> getTreeAuxData(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) const;

  folly::coro::now_task<std::optional<TreeAuxData>> co_getTreeAuxData(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) const;

  /**
   * Get aux data about a Tree from EdenFS's in memory TreeAuxData cache.
   *
   * This returns an ImmediateFuture object that will produce the TreeAuxData
   * when it is ready.  It may result in a std::domain_error if the specified
   * tree does not exist, or possibly other exceptions on error.
   */
  std::optional<TreeAuxData> getTreeAuxDataFromInMemoryCache(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) const;

  folly::coro::now_task<std::optional<Hash32>> co_getTreeDigestHash(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) const;

  /**
   * Returns the digest size of the tree with the given ID.
   */
  ImmediateFuture<std::optional<uint64_t>> getTreeDigestSize(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) const;

  /**
   * Prefetch all the blobs represented by the HashRange.
   *
   * The caller is responsible for making sure that the HashRange stays valid
   * for as long as the returned coroutine.
   */
  folly::coro::now_task<folly::Unit> prefetchBlobs(
      ObjectIdRange ids,
      const ObjectFetchContextPtr& context) const;

  /**
   * Strip the ObjectId to a smaller representation for memory optimization.
   * For example, in SaplingBackingStore, this strips the path portion of the
   * ObjectId, keeping only the hash bytes.
   */
  ObjectId stripObjectId(const ObjectId& id) const;

  /**
   * The modification time that `id` carries (see EmbeddedMetadata.h), if it
   * carries one. Does no I/O.
   */
  std::optional<timespec> getMtime(const ObjectId& id) const;

  /**
   * Get a Blob by ID.
   *
   * This returns a Future object that will produce the Blob when it is ready.
   * It may result in a std::domain_error if the specified blob ID does not
   * exist, or possibly other exceptions on error.
   */
  ImmediateFuture<std::shared_ptr<const Blob>> getBlob(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) const override;

  folly::coro::now_task<std::shared_ptr<const Blob>> co_getBlob(
      const ObjectId& id,
      const ObjectFetchContextPtr& fetchContext) const override;

  /**
   * Get aux data about a Blob.
   *
   * This returns an ImmediateFuture object that will produce the BlobAuxData
   * when it is ready.  It may result in a std::domain_error if the specified
   * blob does not exist, or possibly other exceptions on error.
   */
  ImmediateFuture<BlobAuxData> getBlobAuxData(
      const ObjectId& id,
      const ObjectFetchContextPtr& context,
      bool blake3Needed = false) const;

  folly::coro::now_task<BlobAuxData> co_getBlobAuxData(
      const ObjectId& id,
      const ObjectFetchContextPtr& context,
      bool blake3Needed = false) const;

  /**
   * Sync peek of the BlobAuxData in-memory cache.
   *
   * Reads the cache directly. On a satisfying hit, registers didFetch +
   * updateProcessFetch on `context` and returns the value. On bail
   * (cache miss, or blake3-missing when blake3Required=true) returns
   * nullopt with zero side effects, so a subsequent caller invoking
   * getBlobAuxData() records the fetch exactly once.
   *
   * blake3Required=false matches the historical behavior used by the
   * async getBlobAuxData / co_getBlobAuxData fast paths. blake3Required=true
   * is used by sync peek callers that need to bail when the cached entry
   * lacks a blake3 hash.
   */
  std::optional<BlobAuxData> getBlobAuxDataFromInMemoryCache(
      const ObjectId& id,
      const ObjectFetchContextPtr& context,
      bool blake3Required = false) const;

  /**
   * Returns the size of the contents of the blob with the given ID.
   */
  ImmediateFuture<uint64_t> getBlobSize(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) const;

  folly::coro::now_task<uint64_t> co_getBlobSize(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) const;

  /**
   * Returns the SHA-1 hash of the contents of the blob with the given ID.
   */
  ImmediateFuture<Hash20> getBlobSha1(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) const;

  folly::coro::now_task<Hash20> co_getBlobSha1(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) const;

  /** Returns the Blake3 hash of the contents of the blob. */
  folly::coro::now_task<Hash32> co_getBlobBlake3(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) const;

  /**
   * Compares the objects.
   *
   * Returns true when the objects either refer to the same object
   * (areObjectsKnownIdentical), or if their content are the same.
   */
  ImmediateFuture<bool> areBlobsEqual(
      const ObjectId& one,
      const ObjectId& two,
      const ObjectFetchContextPtr& context) const;

  folly::coro::now_task<bool> co_areBlobsEqual(
      const ObjectId& one,
      const ObjectId& two,
      const ObjectFetchContextPtr& context) const;

  /**
   * Check whether the caller has access to the given manifest ID,
   * but only if the TTL since lastCheck has expired. Returns true
   * if access is allowed, false if denied or TTL not yet expired.
   * The TTL is controlled by the acl:restricted-tree-ttl-seconds config.
   */
  ImmediateFuture<bool> checkPermissionIfExpired(
      const ObjectId& manifestId,
      std::chrono::steady_clock::time_point lastCheck) const;

  folly::coro::now_task<std::vector<folly::Try<std::vector<EntryAcl>>>>
  co_getPathAcls(
      const RootId& rootId,
      const std::vector<std::string>& paths,
      const ObjectFetchContextPtr& context) const;

  /**
   * Get the BackingStore used by this ObjectStore
   */
  const std::shared_ptr<BackingStore>& getBackingStore() const {
    return backingStore_;
  }

  /**
   * Get the EdenConfig used by this ObjectStore
   */
  folly::ReadMostlySharedPtr<const EdenConfig> getEdenConfig() const {
    return edenConfig_->getEdenConfig();
  }

  RestrictedContentMode getRestrictedContentMode() const {
    return restrictedContentMode_;
  }

  /**
   * Get the TreeCache used by this ObjectStore
   */
  const std::shared_ptr<TreeCache>& getTreeCache() const {
    return treeCache_;
  }

  /**
   * Convenience wrapper around BackingStore::compareObjectsById.  See
   * `BackingStorecompareObjectsById`'s documentation for more details.
   */
  ObjectComparison compareObjectsById(const ObjectId& one, const ObjectId& two)
      const;

  /**
   * Convenience wrapper around BackingStore::compareRootsById.  See
   * `BackingStore::compareRootsById`'s documentation for more details.
   */
  ObjectComparison compareRootsById(const RootId& one, const RootId& two) const;

  /**
   * Convenience wrapper around compareObjectsById for the common case that the
   * caller wants to know if two IDs refer to the same object.
   *
   * Returns true only if the objects are known identical. If they are known
   * different or if the BackingStore can't determine if they're identical,
   * returns false.
   *
   * This function is used to short-circuit deep comparisons of objects.
   */
  bool areObjectsKnownIdentical(const ObjectId& one, const ObjectId& two) const;

  folly::Synchronized<std::unordered_map<ProcessId, std::atomic<uint64_t>>>&
  getPidFetches() {
    return pidFetchCounts_->map_;
  }

  void clearFetchCounts() {
    pidFetchCounts_->clear();
  }

 private:
  FOLLY_GTEST_FRIEND_TEST(ObjectStoreTest, caching_policies_anything);
  FOLLY_GTEST_FRIEND_TEST(ObjectStoreTest, caching_policies_no_caching);
  FOLLY_GTEST_FRIEND_TEST(ObjectStoreTest, caching_policies_blob);
  FOLLY_GTEST_FRIEND_TEST(ObjectStoreTest, caching_policies_trees);
  FOLLY_GTEST_FRIEND_TEST(ObjectStoreTest, caching_policies_blob_aux_data);
  FOLLY_GTEST_FRIEND_TEST(
      ObjectStoreTest,
      caching_policies_trees_and_blob_aux_data);
  // Forbidden constructor. Use create().
  ObjectStore(
      std::shared_ptr<BackingStore> backingStore,
      std::shared_ptr<TreeCache> treeCache,
      EdenStatsPtr stats,
      std::shared_ptr<ProcessInfoCache> processInfoCache,
      std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger,
      std::shared_ptr<ReloadableConfig> edenConfig,
      CaseSensitivity caseSensitive,
      folly::Executor::KeepAlive<> executor,
      std::shared_ptr<WalkDetector> walkDetector);

  /**
   * Report a blob or tree that came back from the backing store to the walk
   * detector, for the fetch causes that count as walking.
   */
  void witnessBlobFetch(
      const ObjectId& id,
      const BackingStore::GetBlobResult& result,
      const ObjectFetchContext& context) const;
  void witnessTreeFetch(
      const ObjectId& id,
      const Tree& tree,
      ObjectFetchContext::Origin origin,
      const ObjectFetchContext& context) const;

  // Forbidden copy constructor and assignment operator

  /**
   * Starts a fetch coroutine on the calling thread and wraps its result.
   *
   * A fetch answered from an in-memory cache or the local store completes
   * before this returns, so the ImmediateFuture is ready and the FUSE and
   * NFS dispatchers reply without an executor hop. Only a fetch that has to
   * wait on the backing store's import queue resumes on executor_.
   *
   * folly calls this start "unsafe" because the coroutine reports executor_
   * as its executor while its synchronous prefix runs on the caller's
   * thread. The fetch paths do not depend on which thread that prefix runs
   * on; the SemiFuture-returning backing store methods already run the same
   * local lookups on the caller's thread.
   */
  template <typename T>
  ImmediateFuture<T> startInline(folly::coro::Task<T> task) const;
  ObjectStore(ObjectStore const&) = delete;
  ObjectStore& operator=(ObjectStore const&) = delete;

  Hash32 computeBlake3(const Blob& blob) const;

  folly::coro::now_task<BackingStore::GetTreeResult> getTreeImpl(
      const ObjectId& id,
      const ObjectFetchContextPtr& context,
      folly::stop_watch<std::chrono::milliseconds> watch) const;
  void maybeCacheTreeAuxInMemCache(
      const ObjectId& id,
      const BackingStore::GetTreeResult& treeResult) const;
  bool bypassInMemoryTreeCaches(const ObjectFetchContext& context) const;

  folly::coro::now_task<BackingStore::GetTreeAuxResult> co_getTreeAuxDataImpl(
      const ObjectId& id,
      const ObjectFetchContextPtr& context,
      folly::stop_watch<std::chrono::milliseconds> watch) const;

  folly::SemiFuture<BackingStore::GetBlobResult> getBlobImpl(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) const;

  folly::coro::now_task<BackingStore::GetBlobResult> co_getBlobImpl(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) const;

  folly::coro::now_task<BackingStore::GetBlobAuxResult> getBlobAuxDataImpl(
      const ObjectId& id,
      const ObjectFetchContextPtr& context,
      folly::stop_watch<std::chrono::milliseconds> watch) const;

  /**
   * During status and checkout, it's common to look up the SHA-1 for a given
   * blob ID. To avoid needing to hit RocksDB, keep a bounded in-memory cache of
   * the sizes and SHA-1s of blobs we've seen. Each node is somewhere around 50
   * bytes (20+28 + LRU overhead) and we store auxDataCacheSize entries (as
   * defined in EdenConfig.h), which EvictingCacheMap divides in two for some
   * reason. At the time of this comment, EvictingCacheMap does not store its
   * nodes densely, so there may also be some jemalloc tracking overhead and
   * some internal fragmentation depending on whether the node fits cleanly
   * into one of jemalloc's size classes.
   */
  mutable ShardedLruCache<BlobAuxData> blobAuxDataCache_;

  mutable ShardedLruCache<TreeAuxData> treeAuxDataCache_;

  /**
   * During glob, we need to read a lot of trees, but we avoid loading inodes,
   * so this means we go to RocksDB for each tree read. To avoid needing to hit
   * RocksDB, keep a bounded in-memory cache of the trees we've seen.
   * This cache will also be read from the first time we load a tree inode.
   * This cache is shared across all object stores, and has a fixed memory
   * size. (The one size limit violation is if there are very large trees,
   * the cache is allowed to retain a small fixed number of these in cache, and
   * violate the fixed size. This generally, should be rare as no trees should
   * approach the size limit of the cache.)
   *
   * TODO: treeCache_ is a shared across all object stores. We should explore
   * having two tree caches (case sensitive and case insensitive) and give the
   * ObjectStore the corresponding tree cache at creation time.
   */
  const std::shared_ptr<TreeCache> treeCache_;

  /*
   * The BackingStore.
   *
   * Multiple ObjectStores may share the same BackingStore.
   */
  std::shared_ptr<BackingStore> backingStore_;
  /** Resumes fetches that had to wait on the backing store. */
  folly::Executor::KeepAlive<> executor_;
  /** The mount's walk detector; nullptr when the store detects no walks. */
  std::shared_ptr<WalkDetector> walkDetector_;

  EdenStatsPtr const stats_;

  /* number of fetches for each process collected
   * from the beginning of the eden daemon progress */
  std::unique_ptr<PidFetchCounts> pidFetchCounts_;

  /* process name cache and events logger used for
   * sending fetch heavy events, set to nullptr if not
   * initialized by create()
   */
  std::shared_ptr<ProcessInfoCache> processInfoCache_;
  std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger_;
  std::shared_ptr<ReloadableConfig> edenConfig_;

  // Is this ObjectStore case sensitive? This only matters for methods returning
  // Tree.
  CaseSensitivity caseSensitive_;

  // Read once at construction: a live mount's listing policy must not flip
  // under the kernel's caches.
  const RestrictedContentMode restrictedContentMode_;
};

} // namespace facebook::eden
