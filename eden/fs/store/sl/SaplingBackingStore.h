/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/ExceptionWrapper.h>
#include <folly/Expected.h>
#include <folly/Function.h>
#include <folly/Range.h>
#include <folly/Synchronized.h>
#include <folly/coro/Task.h>
#include <folly/coro/safe/NowTask.h>
#include <folly/portability/GTestProd.h>
#include <sys/types.h>
#include <atomic>
#include <memory>
#include <vector>

#include "eden/common/telemetry/RequestMetricsScope.h"
#include "eden/common/telemetry/TraceBus.h"
#include "eden/common/utils/RefPtr.h"
#include "eden/fs/eden-config.h"
#include "eden/fs/model/Hash.h"
#include "eden/fs/store/BackingStore.h"
#include "eden/fs/store/ImportPriority.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/store/sl/SaplingBackingStoreOptions.h"
#include "eden/fs/store/sl/SaplingImportRequestQueue.h"
#include "eden/fs/telemetry/ActivityBuffer.h"
#include "eden/scm/lib/backingstore/src/ffi.rs.h"

namespace sapling {
using NodeId = facebook::eden::Hash20;
using FetchCause = facebook::eden::ObjectFetchContext::Cause;
using RepoPath = facebook::eden::RelativePathPiece;
using RootId = facebook::eden::RootId;
using ObjectId = facebook::eden::ObjectId;
using ObjectFetchContextPtr = facebook::eden::ObjectFetchContextPtr;
using SlOid = facebook::eden::SlOid;
using SlOidView = facebook::eden::SlOidView;

struct SaplingRequest {
  // This field is typically borrowed from a SaplingImportRequest - be
  // cognizant of lifetimes.
  SlOidView oid;

  FetchCause cause;
  ObjectFetchContextPtr context;
  // TODO: sapling::FetchMode mode;
  // TODO: sapling::ClientRequestInfo cri;

  SaplingRequest(
      SlOidView oid_,
      FetchCause cause_,
      ObjectFetchContextPtr context_)
      : oid(oid_), cause(cause_), context(std::move(context_)) {}
};
} // namespace sapling

namespace facebook::eden {

class BackingStoreLogger;
class ReloadableConfig;
class UnboundedQueueExecutor;
class EdenStats;
class ErrorLogger;
class SaplingImportRequest;
class EdenFsEventsLogger;
class FaultInjector;
template <typename T>
class RefPtr;
class ObjectFetchContext;
using ObjectFetchContextPtr = RefPtr<ObjectFetchContext>;

struct HgImportTraceEvent : TraceEventBase {
  enum EventType : uint8_t {
    QUEUE,
    START,
    FINISH,
  };

  enum ResourceType : uint8_t {
    BLOB,
    TREE,
    BLOB_AUX,
    TREE_AUX,
    BLOB_BATCH,
  };

  static HgImportTraceEvent queue(
      uint64_t unique,
      ResourceType resourceType,
      const SlOid& slOid,
      ImportPriority::Class priority,
      ObjectFetchContext::Cause cause,
      OptionalProcessId pid) {
    return HgImportTraceEvent{
        unique, QUEUE, resourceType, slOid, priority, cause, pid, std::nullopt};
  }

  static HgImportTraceEvent start(
      uint64_t unique,
      ResourceType resourceType,
      const SlOid& slOid,
      ImportPriority::Class priority,
      ObjectFetchContext::Cause cause,
      OptionalProcessId pid) {
    return HgImportTraceEvent{
        unique, START, resourceType, slOid, priority, cause, pid, std::nullopt};
  }

  static HgImportTraceEvent finish(
      uint64_t unique,
      ResourceType resourceType,
      const SlOid& slOid,
      ImportPriority::Class priority,
      ObjectFetchContext::Cause cause,
      OptionalProcessId pid,
      ObjectFetchContext::FetchedSource fetchedSource) {
    return HgImportTraceEvent{
        unique,
        FINISH,
        resourceType,
        slOid,
        priority,
        cause,
        pid,
        fetchedSource};
  }

  HgImportTraceEvent(
      uint64_t unique,
      EventType eventType,
      ResourceType resourceType,
      const SlOid& slOid,
      ImportPriority::Class priority,
      ObjectFetchContext::Cause cause,
      OptionalProcessId pid,
      std::optional<ObjectFetchContext::FetchedSource> fetchedSource);

  // Simple accessor that hides the internal memory representation of paths.
  std::string getPath() const {
    return path.get();
  }

  // Unique per request, but is consistent across the three stages of an import:
  // queue, start, and finish. Used to correlate events to a request.
  uint64_t unique;
  // Always null-terminated, and saves space in the trace event structure.
  // TODO: Replace with a single pointer to a reference-counted string to save 8
  // bytes in this struct.
  std::shared_ptr<char[]> path;
  // The HG manifest node ID.
  Hash20 manifestNodeId;
  EventType eventType;
  ResourceType resourceType;
  ImportPriority::Class importPriority;
  ObjectFetchContext::Cause importCause;
  OptionalProcessId pid;
  std::optional<ObjectFetchContext::FetchedSource> fetchedSource;
};

/**
 * A Sapling backing store implementation that will put incoming blob/tree
 * import requests into a job queue, then a pool of workers will work on
 * fulfilling these requests via different methods (reading from hgcache,
 * Mononoke, debugimporthelper, etc.).
 */
class SaplingBackingStore final
    : public BackingStore,
      public std::enable_shared_from_this<SaplingBackingStore> {
 public:
  using ImportRequestsList = std::vector<std::shared_ptr<SaplingImportRequest>>;
  using ImportRequestsMap = std::
      map<sapling::NodeId, std::pair<ImportRequestsList, RequestMetricsScope>>;

  SaplingBackingStore(
      AbsolutePathPiece repository,
      CaseSensitivity caseSensitive,
      EdenStatsPtr stats,
      UnboundedQueueExecutor* serverThreadPool,
      std::shared_ptr<ReloadableConfig> config,
      std::unique_ptr<SaplingBackingStoreOptions> runtimeOptions,
      std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger,
      ErrorLogger& errorLogger,
      std::unique_ptr<BackingStoreLogger> logger,
      FaultInjector* FOLLY_NONNULL faultInjector);

  /**
   * Create a SaplingBackingStore suitable for use in unit tests. Pass any
   * executor (InlineExecutor for synchronous tests, CPUThreadPoolExecutor
   * for coroutine tests that need a real executor to avoid the coro::Task
   * DCHECK on InlineExecutor).
   */
  SaplingBackingStore(
      AbsolutePathPiece repository,
      CaseSensitivity caseSensitive,
      EdenStatsPtr stats,
      folly::Executor* executor,
      std::shared_ptr<ReloadableConfig> config,
      std::unique_ptr<SaplingBackingStoreOptions> runtimeOptions,
      std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger,
      ErrorLogger& errorLogger,
      std::unique_ptr<BackingStoreLogger> logger,
      FaultInjector* FOLLY_NONNULL faultInjector);

  ~SaplingBackingStore() override;

  /**
   * Objects that can be imported from Hg
   */
  enum SaplingImportObject {
    BLOB,
    TREE,
    BLOB_AUX,
    TREE_AUX,
    BATCHED_BLOB,
    BATCHED_TREE,
    BATCHED_BLOB_AUX,
    BATCHED_TREE_AUX,
    PREFETCH
  };
  constexpr static std::array<SaplingImportObject, 9> saplingImportObjects{
      SaplingImportObject::BLOB,
      SaplingImportObject::TREE,
      SaplingImportObject::BLOB_AUX,
      SaplingImportObject::TREE_AUX,
      SaplingImportObject::BATCHED_BLOB,
      SaplingImportObject::BATCHED_TREE,
      SaplingImportObject::BATCHED_BLOB_AUX,
      SaplingImportObject::BATCHED_TREE_AUX,
      SaplingImportObject::PREFETCH};

  static folly::StringPiece stringOfSaplingImportObject(
      SaplingImportObject object);

  ActivityBuffer<HgImportTraceEvent>& getActivityBuffer() {
    return activityBuffer_;
  }

  TraceBus<HgImportTraceEvent>& getTraceBus() const {
    return *traceBus_;
  }

  /**
   * Flush any pending writes to disk.
   *
   * As a side effect, this also reloads the current state of Mercurial's
   * cache, picking up any writes done by Mercurial.
   */
  void flush();

  static void flushCounters() {
    sapling::sapling_flush_counters();
  }

  /**
   * Mirrors telemetry:enable-scribe-logging into the Rust tracing-to-Scuba
   * sink, which cannot read EdenConfig itself. Process wide.
   */
  static void setScribeLoggingEnabled(bool enabled) {
    sapling::sapling_backingstore_set_scribe_logging_enabled(enabled);
  }

  ObjectComparison compareObjectsById(const ObjectId& one, const ObjectId& two)
      override;

  ObjectComparison compareRootsById(const RootId& one, const RootId& two)
      override;

  RootId parseRootId(folly::StringPiece rootId) override;
  std::string renderRootId(const RootId& rootId) override;
  std::string displayRootId(const RootId& rootId) override;
  ObjectId parseObjectId(folly::StringPiece objectId) override {
    return staticParseObjectId(objectId);
  }
  std::string renderObjectId(const ObjectId& objectId) override {
    return staticRenderObjectId(objectId);
  }

  static ObjectId staticParseObjectId(folly::StringPiece objectId);
  static std::string staticRenderObjectId(const ObjectId& objectId);

  std::optional<Hash20> getManifestNode(const ObjectId& commitId);

  /**
   * calculates `metric` for `object` imports that are `stage`.
   *    ex. SaplingBackingStore::getImportMetrics(
   *          RequestMetricsScope::HgImportStage::PENDING,
   *          SaplingBackingStore::SaplingImportObject::BLOB,
   *          RequestMetricsScope::Metric::COUNT,
   *        )
   *    calculates the number of blob imports that are pending
   */
  size_t getImportMetric(
      RequestMetricsScope::RequestStage stage,
      SaplingImportObject object,
      RequestMetricsScope::RequestMetric metric) const;

  void startRecordingFetch() override;
  std::unordered_set<std::string> stopRecordingFetch() override;

  ImmediateFuture<folly::Unit> importManifestForRoot(
      const RootId& rootId,
      const Hash20& manifestId,
      const ObjectFetchContextPtr& context) override;

  folly::coro::now_task<folly::Unit> co_importManifestForRoot(
      const RootId& rootId,
      const Hash20& manifestId,
      const ObjectFetchContextPtr& context) override;

  void periodicManagementTask() override;

  std::optional<folly::StringPiece> getRepoName() override {
    return repoName_;
  }

  std::vector<HgImportTraceEvent> getOutstandingHgEvents() const {
    auto lockedEventsMap = outstandingHgEvents_.rlock();
    std::vector<HgImportTraceEvent> events;
    for (const auto& eventMap : *lockedEventsMap) {
      events.push_back(eventMap.second);
    }
    return events;
  }

  int64_t dropAllPendingRequestsFromQueue() override;

  ObjectId stripObjectId(const ObjectId& id) const override;

  /**
   * Get statistics about the hgcache (Sapling disk cache). The error side
   * (a message) is distinct from a successful result reporting "no cache
   * configured" (see HgCacheStats::cache_path_configured and the per-cache
   * *_state fields) - callers must not conflate a genuine lookup failure
   * with a cache that simply isn't configured.
   */
  folly::Expected<sapling::HgCacheStats, std::string> getCacheStats() const;

 private:
  FOLLY_GTEST_FRIEND_TEST(
      SaplingBackingStoreNoFaultInjectorTest,
      cachingPolicyConstruction);
  FOLLY_GTEST_FRIEND_TEST(SaplingBackingStoreNoFaultInjectorTest, getTree);
  FOLLY_GTEST_FRIEND_TEST(SaplingBackingStoreWithFaultInjectorTest, getTree);
  FOLLY_GTEST_FRIEND_TEST(SaplingBackingStoreNoFaultInjectorTest, getBlob);
  FOLLY_GTEST_FRIEND_TEST(SaplingBackingStoreWithFaultInjectorTest, getBlob);
  FOLLY_GTEST_FRIEND_TEST(
      SaplingBackingStoreNoFaultInjectorTest,
      dropAllPendingRequestsWithAuxRequestsQueued);
  FOLLY_GTEST_FRIEND_TEST(
      SaplingBackingStoreNoFaultInjectorTest,
      dropAllPendingRequestsThenRefetchAuxData);
  FOLLY_GTEST_FRIEND_TEST(
      SaplingBackingStoreNoFaultInjectorTest,
      sameRequestsDifferentFetchCause);
  FOLLY_GTEST_FRIEND_TEST(
      SaplingBackingStoreNoFaultInjectorTest,
      prefetchBlobsWithDuplicates);
  FOLLY_GTEST_FRIEND_TEST(
      SaplingBackingStoreNoFaultInjectorTest,
      prefetchBlobsWithDuplicatesResolvesAllCallbacks);
  FOLLY_GTEST_FRIEND_TEST(
      SaplingBackingStoreWithFaultInjectorTest,
      getTreeBatch);
  FOLLY_GTEST_FRIEND_TEST(
      SaplingBackingStoreWithFaultInjectorIgnoreConfigTest,
      getTreeBatch);
  FOLLY_GTEST_FRIEND_TEST(
      SaplingBackingStoreNoFaultInjectorTest,
      getTreeBatchConvertsPermissionDeniedToRestrictedTree);
  FOLLY_GTEST_FRIEND_TEST(
      SaplingBackingStoreWithFaultInjectorTest,
      getTreeEnqueueFutureChainCanBePausedAndResumed);
  FOLLY_GTEST_FRIEND_TEST(
      SaplingBackingStoreWithFaultInjectorTest,
      coGetTreeEnqueueCoroutineKeepsObjectAlive);
  FOLLY_GTEST_FRIEND_TEST(
      SaplingBackingStoreWithFaultInjectorTest,
      blobFetchCheckpointsAreReachable);
  FOLLY_GTEST_FRIEND_TEST(
      SaplingBackingStoreWithFaultInjectorTest,
      coGetRootTreeFaultInjection);
  friend class EdenServiceHandler;

  // Forbidden copy constructor and assignment operator
  SaplingBackingStore(const SaplingBackingStore&) = delete;
  SaplingBackingStore& operator=(const SaplingBackingStore&) = delete;

  /**
   * Import the manifest for the specified revision using mercurial
   * treemanifest data.
   */
  folly::Future<TreePtr> importTreeManifest(
      const ObjectId& commitId,
      const ObjectFetchContextPtr& context,
      const ObjectFetchContext::ObjectType type);

  folly::Future<TreePtr> importTreeManifestImpl(
      Hash20 manifestNode,
      const ObjectFetchContextPtr& context,
      const ObjectFetchContext::ObjectType type);

  folly::coro::now_task<TreePtr> co_importTreeManifestImpl(
      Hash20 manifestNode,
      const ObjectFetchContextPtr& context,
      const ObjectFetchContext::ObjectType type);

  folly::Try<TreePtr> importTreeManifestSync(
      Hash20 manifestNode,
      const ObjectFetchContextPtr& context,
      const ObjectFetchContext::ObjectType type);

  folly::coro::now_task<GetRootTreeResult> co_getRootTree(
      const RootId& rootId,
      const ObjectFetchContextPtr& context) override;
  ImmediateFuture<std::shared_ptr<TreeEntry>> getTreeEntryForObjectId(
      const ObjectId& /* objectId */,
      TreeEntryType /* treeEntryType */,
      const ObjectFetchContextPtr& /* context */) override {
    throw std::domain_error("unimplemented");
  }

  void getTreeBatch(
      const ImportRequestsList& requests,
      sapling::FetchMode fetch_mode);

  folly::SemiFuture<GetTreeResult> getTree(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) override;

  folly::coro::now_task<GetTreeResult> co_getTree(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) override;

  folly::Try<TreePtr> getTreeFromBackingStore(
      const RelativePath& path,
      SlOidView slOid,
      ObjectFetchContextPtr context,
      const ObjectFetchContext::ObjectType type);

  /**
   * Imports the tree identified by the given hash from the hg cache.
   * Returns nullptr if not found.
   */
  TreePtr getTreeLocal(SlOidView oid, const ObjectFetchContextPtr& context);

  /**
   * Imports the tree identified by the given hash from the remote store.
   * Returns nullptr if not found.
   */
  folly::Try<TreePtr> getTreeRemote(
      SlOidView oid,
      const ObjectFetchContextPtr& context);

  /**
   * Fetch a single tree from Sapling Rust store. "Not found" is propagated as
   * nullptr to avoid exception overhead.
   */
  folly::Try<facebook::eden::TreePtr> getNativeTree(
      SlOidView slOid,
      const ObjectFetchContextPtr& context,
      sapling::FetchMode fetch_mode);

  /**
   * Create a tree fetch request and enqueue it to the
   * SaplingImportRequestQueue
   *
   * For latency sensitive context, the caller is responsible for checking
   * if the tree is present locally, as this function will always push the
   * request at the end of the queue.
   */
  ImmediateFuture<GetTreeResult> getTreeEnqueue(
      const SlOid& slOid,
      const ObjectFetchContextPtr& context);

  folly::coro::now_task<GetTreeResult> co_getTreeEnqueue(
      const SlOid& slOid,
      const ObjectFetchContextPtr& context);

  folly::SemiFuture<GetTreeAuxResult> getTreeAuxData(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) override;
  folly::coro::now_task<GetTreeAuxResult> co_getTreeAuxData(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) override;

  /**
   * Create a tree aux data fetch request and enqueue it to the
   * SaplingImportRequestQueue
   *
   * For latency sensitive context, the caller is responsible for checking if
   * the tree aux data is present locally, as this function will always push
   * the request at the end of the queue.
   */
  ImmediateFuture<GetTreeAuxResult> getTreeAuxDataEnqueue(
      const SlOid& slOid,
      const ObjectFetchContextPtr& context);

  /**
   * Fetch multiple aux data at once.
   *
   * This function returns when all the aux data have been fetched.
   */
  void getTreeAuxDataBatch(
      const ImportRequestsList& requests,
      sapling::FetchMode fetch_mode);

  /**
   * Reads tree aux data from hg cache.
   */
  folly::Try<TreeAuxDataPtr> getLocalTreeAuxData(SlOidView id);

  folly::SemiFuture<GetBlobResult> getBlob(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) override;

  folly::coro::Task<GetBlobResult> co_getBlob(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) override;

  /**
   * Import multiple blobs at once. The vector parameters have to be the same
   * length. Promises passed in will be resolved if a blob is successfully
   * imported. Otherwise the promise will be left untouched.
   */
  void getBlobBatch(
      const ImportRequestsList& requests,
      sapling::FetchMode fetchMode);

  /**
   * Batch fetch blobs directly from lower level store. "Not found" is
   * propagated as an exception.
   *
   * Returns the number of successful blob requests served locally and
   * remotely.
   */
  sapling::BatchFetchStats nativeGetBlobBatch(
      folly::Range<const sapling::SaplingRequest*> requests,
      sapling::FetchMode fetch_mode,
      bool allow_ignore_result,
      folly::FunctionRef<
          void(size_t, folly::Try<std::unique_ptr<folly::IOBuf>>)> resolve);

  /**
   * Create a blob fetch request and enqueue it to the SaplingImportRequestQueue
   *
   * For latency sensitive context, the caller is responsible for checking if
   * the blob is present locally, as this function will always push the request
   * at the end of the queue.
   */
  ImmediateFuture<GetBlobResult> getBlobEnqueue(
      const SlOid& slOid,
      const ObjectFetchContextPtr& context,
      const SaplingImportRequest::FetchType fetch_type);

  folly::coro::now_task<GetBlobResult> co_getBlobEnqueue(
      const SlOid& slOid,
      const ObjectFetchContextPtr& context,
      const SaplingImportRequest::FetchType fetch_type);

  /**
   * Imports the blob identified by the given hash from the backing store.
   * If localOnly is set to true, only fetch the blob from local (memory or
   * disk) store.
   *
   * Returns nullptr if not found.
   */
  folly::Try<BlobPtr> getBlobFromBackingStore(
      SlOidView slOid,
      const ObjectFetchContextPtr& context,
      sapling::FetchMode fetchMode);

  /**
   * Imports the blob identified by the given hash from the hg cache.
   * Returns nullptr if not found.
   */
  folly::Try<BlobPtr> getBlobLocal(
      SlOidView slOid,
      const ObjectFetchContextPtr& context) {
    return getBlobFromBackingStore(
        std::move(slOid), context, sapling::FetchMode::LocalOnly);
  }

  /**
   * Imports the blob identified by the given hash from the remote store.
   * Returns nullptr if not found.
   */
  folly::Try<BlobPtr> getBlobRemote(
      SlOidView slOid,
      const ObjectFetchContextPtr& context) {
    return getBlobFromBackingStore(
        std::move(slOid), context, sapling::FetchMode::RemoteOnly);
  }

  folly::coro::now_task<GetBlobAuxResult> co_getBlobAuxData(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) override;

  /**
   * Create a blob aux data fetch request and enqueue it to the
   * SaplingImportRequestQueue
   *
   * For latency sensitive context, the caller is responsible for checking if
   * the blob aux data is present locally, as this function will always push
   * the request at the end of the queue.
   */
  folly::coro::now_task<GetBlobAuxResult> co_getBlobAuxDataEnqueue(
      const SlOid& slOid,
      const ObjectFetchContextPtr& context);

  /**
   * Fetch multiple aux data at once.
   *
   * This function returns when all the aux data have been fetched.
   */
  void getBlobAuxDataBatch(
      const ImportRequestsList& requests,
      sapling::FetchMode fetch_mode);

  /**
   * Reads blob aux data from hg cache.
   */
  folly::Try<BlobAuxDataPtr> getLocalBlobAuxData(SlOidView id);

  /**
   * DEPRECATED: use co_prefetchBlobs directly. Futures wrapper kept for
   * non-coroutine callers; remove once all callers have migrated.
   */
  [[nodiscard]] virtual folly::SemiFuture<folly::Unit> prefetchBlobs(
      ObjectIdRange ids,
      const ObjectFetchContextPtr& context) override;

  folly::coro::now_task<folly::Unit> co_prefetchBlobs(
      ObjectIdRange ids,
      const ObjectFetchContextPtr& context) override;

  /**
   * A detector backed by the Rust walk detector for `mountPath`. It follows
   * `backingstore.walk-mode` and the other walk settings as the store reloads
   * them, so it exists even while the mode is off.
   */
  std::shared_ptr<WalkDetector> createWalkDetector(
      AbsolutePathPiece mountPath) override;

  void processBlobImportRequests(
      std::vector<std::shared_ptr<SaplingImportRequest>>&& requests);
  void processTreeImportRequests(
      std::vector<std::shared_ptr<SaplingImportRequest>>&& requests);
  void processBlobAuxImportRequests(
      std::vector<std::shared_ptr<SaplingImportRequest>>&& requests);
  void processTreeAuxImportRequests(
      std::vector<std::shared_ptr<SaplingImportRequest>>&& requests);

  void setPrefetchBlobCounters(
      ObjectFetchContextPtr context,
      ObjectFetchContext::FetchedSource fetchedSource,
      ObjectFetchContext::FetchResult fetchResult,
      folly::stop_watch<std::chrono::milliseconds> watch);
  void setFetchBlobCounters(
      ObjectFetchContextPtr context,
      ObjectFetchContext::FetchedSource fetchedSource,
      ObjectFetchContext::FetchResult fetchResult,
      folly::stop_watch<std::chrono::milliseconds> watch);
  void setBlobCounters(
      ObjectFetchContextPtr context,
      SaplingImportRequest::FetchType fetchType,
      ObjectFetchContext::FetchedSource fetchedSource,
      ObjectFetchContext::FetchResult fetchResult,
      folly::stop_watch<std::chrono::milliseconds> watch);

  ImmediateFuture<bool> checkPermission(const ObjectId& manifestId) override;
  folly::coro::now_task<std::vector<folly::Try<std::vector<EntryAcl>>>>
  co_getPathAcls(
      const RootId& rootId,
      const std::vector<std::string>& paths,
      const ObjectFetchContextPtr& context) override;

  /**
   * The worker runloop function.
   */
  void processRequest();

  /**
   * Logs a backing store fetch to scuba if the path being fetched is in the
   * configured paths to log. The path is obtained from the ObjectId.
   */
  void logBackingStoreFetch(
      const ObjectFetchContext& context,
      folly::Range<SlOidView*> slOids,
      ObjectFetchContext::ObjectType type);

  /**
   * Logs a backing store fetch miss to the structured error logger, tagged with
   * the given fetch type (e.g. "blob", "tree", "blob_aux"). Only invoked on the
   * cold failure path.
   */
  void logFetchMiss(
      const folly::exception_wrapper& ex,
      folly::StringPiece fetchType);

 private:
  /**
   * gets the watches timing `object` imports that are `stage`
   *    ex. SaplingBackingStore::getImportWatches(
   *          RequestMetricsScope::HgImportStage::PENDING,
   *          SaplingBackingStore::SaplingImportObject::BLOB,
   *        )
   *    gets the watches timing blob imports that are pending
   */
  RequestMetricsScope::LockedRequestWatchList& getImportWatches(
      RequestMetricsScope::RequestStage stage,
      SaplingImportObject object) const;

  TreePtr makeRestrictedTree(ObjectId id) const;
  folly::Try<TreePtr> convertPermissionDeniedToRestrictedTree(
      folly::Try<TreePtr> content,
      ObjectId id) const;

  /**
   * Gets the watches timing pending `object` imports
   *   ex. SaplingBackingStore::getPendingImportWatches(
   *          SaplingBackingStore::SaplingImportObject::BLOB,
   *        )
   *    gets the watches timing pending blob imports
   */
  RequestMetricsScope::LockedRequestWatchList& getPendingImportWatches(
      SaplingImportObject object) const;

  /**
   * Gets the watches timing live `object` imports
   *   ex. SaplingBackingStore::getLiveImportWatches(
   *          SaplingBackingStore::SaplingImportObject::BLOB,
   *        )
   *    gets the watches timing live blob imports
   */
  RequestMetricsScope::LockedRequestWatchList& getLiveImportWatches(
      SaplingImportObject object) const;

  template <typename T>
  std::pair<ImportRequestsMap, std::vector<sapling::SaplingRequest>>
  prepareRequests(
      const ImportRequestsList& importRequests,
      const SaplingImportObject& requestType);

  ObjectFetchContext::Cause getHighestPriorityFetchCause(
      const ImportRequestsList& importRequestsForId) const;

  /**
   * Processes hg events from the trace bus by subscribing it.
   * Adds/Updates/Removes event to the outstanding hg events based on event
   * type-
   *   If queued, it will be added to the outstanding hg events.
   *   If started, it will update the existing queued event.
   *   If finished, it will remove the event from outstanding hg events.
   * And, adds event to the activity buffer.
   */
  void processHgEvent(const HgImportTraceEvent& event);

  /**
   * isRecordingFetch_ indicates if SaplingBackingStore is recording paths
   * for fetched files. Initially we don't record paths. When
   * startRecordingFetch() is called, isRecordingFetch_ is set to true and
   * recordFetch() will record the input path. When stopRecordingFetch() is
   * called, isRecordingFetch_ is set to false and recordFetch() no longer
   * records the input path.
   */
  std::atomic<bool> isRecordingFetch_{false};
  folly::Synchronized<std::unordered_set<std::string>> fetchedFilePaths_;

  EdenStatsPtr stats_;

  bool dogfoodingHost();

  /**
   * Reference to the eden config, may be a null pointer in unit tests.
   */
  std::shared_ptr<ReloadableConfig> config_;

  // The main server thread pool; we push the Futures back into
  // this pool to run their completion code to avoid clogging
  // the importer pool. Queuing in this pool can never block (which would risk
  // deadlock) or throw an exception when full (which would incorrectly fail the
  // load).
  folly::Executor* serverThreadPool_;

  /**
   * The import request queue. This queue is unbounded. This queue
   * implementation will ensure enqueue operation never blocks.
   */
  SaplingImportRequestQueue queue_;

  /**
   * The worker thread pool. These threads will be running `processRequest`
   * forever to process incoming import requests
   */
  std::vector<std::thread> threads_;

  std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger_;
  ErrorLogger& errorLogger_;

  /**
   * Logger for backing store imports
   */
  std::unique_ptr<BackingStoreLogger> logger_;

  FaultInjector& faultInjector_;

  // Track metrics for queued imports
  mutable RequestMetricsScope::LockedRequestWatchList pendingImportBlobWatches_;
  mutable RequestMetricsScope::LockedRequestWatchList
      pendingImportBlobAuxWatches_;
  mutable RequestMetricsScope::LockedRequestWatchList pendingImportTreeWatches_;
  mutable RequestMetricsScope::LockedRequestWatchList
      pendingImportTreeAuxWatches_;
  mutable RequestMetricsScope::LockedRequestWatchList
      pendingImportPrefetchWatches_;

  // Track metrics for imports currently fetching data from hg
  mutable RequestMetricsScope::LockedRequestWatchList liveImportBlobWatches_;
  mutable RequestMetricsScope::LockedRequestWatchList liveImportTreeWatches_;
  mutable RequestMetricsScope::LockedRequestWatchList liveImportBlobAuxWatches_;
  mutable RequestMetricsScope::LockedRequestWatchList liveImportTreeAuxWatches_;
  mutable RequestMetricsScope::LockedRequestWatchList
      liveImportPrefetchWatches_;

  // Track metrics for the number of live batches
  mutable RequestMetricsScope::LockedRequestWatchList liveBatchedBlobWatches_;
  mutable RequestMetricsScope::LockedRequestWatchList liveBatchedTreeWatches_;
  mutable RequestMetricsScope::LockedRequestWatchList
      liveBatchedBlobAuxWatches_;
  mutable RequestMetricsScope::LockedRequestWatchList
      liveBatchedTreeAuxWatches_;

  std::unique_ptr<SaplingBackingStoreOptions> runtimeOptions_;

  folly::Synchronized<std::unordered_map<uint64_t, HgImportTraceEvent>>
      outstandingHgEvents_;

  ActivityBuffer<HgImportTraceEvent> activityBuffer_;

  // The traceBus_ and hgTraceHandle_ should be last so any internal subscribers
  // can capture [this].
  std::shared_ptr<TraceBus<HgImportTraceEvent>> traceBus_;

  // Handle for TraceBus subscription.
  TraceSubscriptionHandle<HgImportTraceEvent> hgTraceHandle_;

  std::unique_ptr<sapling::BackingStore, void (*)(sapling::BackingStore*)>
      store_;
  std::string repoName_;
  HgObjectIdFormat objectIdFormat_;
  CaseSensitivity caseSensitive_;
};

} // namespace facebook::eden
