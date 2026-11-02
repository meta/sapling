/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/store/sl/SaplingBackingStore.h"

#include <algorithm>
#include <chrono>
#include <stdexcept>
#include <string>
#include <thread>
#include <utility>
#include <variant>

#include <fmt/format.h>
#include <re2/re2.h>

#include <folly/Executor.h>
#include <folly/Range.h>
#include <folly/String.h>
#include <folly/coro/Collect.h>
#include <folly/executors/CPUThreadPoolExecutor.h>
#include <folly/executors/task_queue/UnboundedBlockingQueue.h>
#include <folly/executors/thread_factory/InitThreadFactory.h>
#include <folly/futures/Future.h>
#include <folly/logging/xlog.h>
#include <folly/system/ThreadName.h>
#include <gflags/gflags.h>

#include "eden/common/telemetry/RequestMetricsScope.h"
#include "eden/common/utils/Bug.h"
#include "eden/common/utils/CaseSensitivity.h"
#include "eden/common/utils/EnumValue.h"
#include "eden/common/utils/FaultInjector.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/Throw.h"
#include "eden/common/utils/UnboundedQueueExecutor.h"
#include "eden/fs/config/ReloadableConfig.h"
#include "eden/fs/model/Blob.h"
#include "eden/fs/model/BlobAuxData.h"
#include "eden/fs/model/TreeAuxData.h"
#include "eden/fs/service/ThriftUtil.h"
#include "eden/fs/store/BackingStoreLogger.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/store/StatsFetchContext.h"
#include "eden/fs/store/sl/SaplingImportRequest.h"
#include "eden/fs/store/sl/SaplingObjectId.h"
#include "eden/fs/store/sl/SaplingWalkDetector.h"
#include "eden/fs/telemetry/EdenErrorInfoBuilder.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/telemetry/ErrorLogger.h"
#include "eden/fs/telemetry/LogEvent.h"
#include "eden/fs/utils/StaticAssert.h"
#ifdef EDEN_HAVE_SERVER_OBSERVER
#include "common/fb303/cpp/ThreadPoolExecutorCounters.h" // @manual
#endif

DEFINE_bool(
    hg_fetch_missing_trees,
    true,
    "Set this parameter to \"no\" to disable fetching missing treemanifest "
    "trees from the remote mercurial server.  This is generally only useful "
    "for testing/debugging purposes");

namespace facebook::eden {

namespace {
// 100,000 hg object fetches in a short term is plausible.
constexpr size_t kTraceBusCapacity = 100000;
static_assert(CheckSize<HgImportTraceEvent, 72>());
// TraceBus is double-buffered, so the following capacity should be doubled.
// 10 MB overhead per backing repo is tolerable.
static_assert(
    CheckEqual<7200000, kTraceBusCapacity * sizeof(HgImportTraceEvent)>());
ObjectId hashFromRootId(const RootId& root) {
  return ObjectId::fromHex(root.value());
}

std::unique_ptr<SaplingBackingStoreOptions> computeRuntimeOptions(
    std::unique_ptr<SaplingBackingStoreOptions> options) {
  // No options are currently set. See D64436672 for an example on how to add
  // this back if the mechanism is needed in the future.
  return options;
}

void reportPrefetchStats(
    const ObjectFetchContextPtr& context,
    const sapling::BatchFetchStats& fetchStats,
    uint64_t totalBytes,
    size_t failureCount) {
  if (!context->shouldCollectStats()) {
    return;
  }
  auto* statsCtx = dynamic_cast<StatsFetchContext*>(context.get());
  if (statsCtx == nullptr) {
    return;
  }

  uint64_t totalFetches = fetchStats.local_count + fetchStats.remote_count;
  if (totalFetches > 0) {
    // Proportionally attribute bytes to local vs remote when available.
    // With IGNORE_RESULT, totalBytes may be 0 but counts are still valid.
    // Every current caller passes totalBytes=0 (byte-level attribution for
    // this path isn't wired up yet), so this branch is inert today; it's
    // kept so a future caller with real byte counts doesn't need to
    // reintroduce the split logic.
    uint64_t localBytes = 0;
    uint64_t remoteBytes = 0;
    if (totalBytes > 0) {
      localBytes = totalBytes * fetchStats.local_count / totalFetches;
      remoteBytes = totalBytes - localBytes;
    }

    if (fetchStats.local_count > 0) {
      statsCtx->didFetchBatch(
          ObjectFetchContext::Blob,
          ObjectFetchContext::FromDiskCache,
          fetchStats.local_count,
          localBytes);
    }
    if (fetchStats.remote_count > 0) {
      statsCtx->didFetchBatch(
          ObjectFetchContext::Blob,
          ObjectFetchContext::FromNetworkFetch,
          fetchStats.remote_count,
          remoteBytes);
    }
  }

  // Track failed fetches for statistics
  if (failureCount > 0) {
    statsCtx->didFetchFailed(ObjectFetchContext::Blob, failureCount);
  }
}

} // namespace

HgImportTraceEvent::HgImportTraceEvent(
    uint64_t unique,
    EventType eventType,
    ResourceType resourceType,
    const SlOid& slOid,
    ImportPriority::Class priority,
    ObjectFetchContext::Cause cause,
    OptionalProcessId pid,
    std::optional<ObjectFetchContext::FetchedSource> fetchedSource)
    : unique{unique},
      manifestNodeId{slOid.node()},
      eventType{eventType},
      resourceType{resourceType},
      importPriority{priority},
      importCause{cause},
      pid{pid},
      fetchedSource{fetchedSource} {
  auto hgPath = slOid.path().view();
  // TODO: If SlOid (and correspondingly ObjectId) used an immutable,
  // refcounted string, we wouldn't need to allocate here.
  path.reset(new char[hgPath.size() + 1]);
  memcpy(path.get(), hgPath.data(), hgPath.size());
  path[hgPath.size()] = 0;
}

SaplingBackingStore::SaplingBackingStore(
    AbsolutePathPiece repository,
    CaseSensitivity caseSensitive,
    EdenStatsPtr stats,
    UnboundedQueueExecutor* serverThreadPool,
    std::shared_ptr<ReloadableConfig> config,
    std::unique_ptr<SaplingBackingStoreOptions> runtimeOptions,
    std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger,
    ErrorLogger& errorLogger,
    std::unique_ptr<BackingStoreLogger> logger,
    FaultInjector* FOLLY_NONNULL faultInjector)
    : stats_(stats.copy()),
      config_(config),
      serverThreadPool_(serverThreadPool),
      queue_(std::move(config)),
      edenFsEventsLogger_{std::move(edenFsEventsLogger)},
      errorLogger_(errorLogger),
      logger_(std::move(logger)),
      faultInjector_{*faultInjector},
      runtimeOptions_(computeRuntimeOptions(std::move(runtimeOptions))),
      activityBuffer_{
          config_->getEdenConfig()->hgActivityBufferSize.getValue()},
      traceBus_{TraceBus<HgImportTraceEvent>::create(
          "hg",
          config_->getEdenConfig()->HgTraceBusCapacity.getValue())},
      store_{
          sapling::sapling_backingstore_new(
              rust::Str{repository.view().data(), repository.view().size()},
              config_->getEdenConfig()->backingstoreWalkMode.getValue(),
              edenFsEventsLogger_)
              .into_raw(),
          [](sapling::BackingStore* backingStore) {
            auto box = rust::Box<sapling::BackingStore>::from_raw(backingStore);
          }},
      objectIdFormat_{config_->getEdenConfig()->hgObjectIdFormat.getValue()},
      caseSensitive_{caseSensitive} {
  try {
    repoName_ =
        std::string(sapling::sapling_backingstore_get_name(*store_.get()));
  } catch (const rust::Error& error) {
    XLOGF(DBG2, "Error while repo name from backingstore: {}", error.what());
  }

  uint8_t numberThreads =
      config_->getEdenConfig()->numBackingstoreThreads.getValue();
  if (!numberThreads) {
    XLOG(
        WARN,
        "SaplingBackingStore configured to use 0 threads. Invalid, using one thread instead");
    numberThreads = 1;
  }
  threads_.reserve(numberThreads);
  for (uint16_t i = 0; i < numberThreads; i++) {
    threads_.emplace_back(&SaplingBackingStore::processRequest, this);
  }

  hgTraceHandle_ = traceBus_->subscribeFunction(
      folly::to<std::string>("hg-activitybuffer-", getRepoName().value_or("")),
      [this](const HgImportTraceEvent& event) { this->processHgEvent(event); });
}

/**
 * Create an SaplingBackingStore suitable for use in unit tests. It uses an
 * inline executor to process loaded objects rather than the thread pools used
 * in production Eden.
 */
SaplingBackingStore::SaplingBackingStore(
    AbsolutePathPiece repository,
    CaseSensitivity caseSensitive,
    EdenStatsPtr stats,
    folly::Executor* executor,
    std::shared_ptr<ReloadableConfig> config,
    std::unique_ptr<SaplingBackingStoreOptions> runtimeOptions,
    std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger,
    ErrorLogger& errorLogger,
    std::unique_ptr<BackingStoreLogger> logger,
    FaultInjector* FOLLY_NONNULL faultInjector)
    : stats_(std::move(stats)),
      config_(config),
      serverThreadPool_(executor),
      queue_(std::move(config)),
      edenFsEventsLogger_{std::move(edenFsEventsLogger)},
      errorLogger_(errorLogger),
      logger_(std::move(logger)),
      faultInjector_{*faultInjector},
      runtimeOptions_(std::move(runtimeOptions)),
      activityBuffer_{
          config_->getEdenConfig()->hgActivityBufferSize.getValue()},
      traceBus_{TraceBus<HgImportTraceEvent>::create(
          "hg",
          config_->getEdenConfig()->HgTraceBusCapacity.getValue())},
      store_{
          sapling::sapling_backingstore_new(
              rust::Str{repository.view().data(), repository.view().size()},
              config_->getEdenConfig()->backingstoreWalkMode.getValue(),
              edenFsEventsLogger_)
              .into_raw(),
          [](sapling::BackingStore* backingStore) {
            auto box = rust::Box<sapling::BackingStore>::from_raw(backingStore);
          }},
      objectIdFormat_{config_->getEdenConfig()->hgObjectIdFormat.getValue()},
      caseSensitive_{caseSensitive} {
  try {
    repoName_ =
        std::string(sapling::sapling_backingstore_get_name(*store_.get()));
  } catch (const rust::Error& error) {
    XLOGF(DBG2, "Error while repo name from backingstore: {}", error.what());
  }

  uint8_t numberThreads =
      config_->getEdenConfig()->numBackingstoreThreads.getValue();
  if (!numberThreads) {
    XLOG(
        WARN,
        "SaplingBackingStore configured to use 0 threads. Invalid, using one thread instead");
    numberThreads = 1;
  }
  threads_.reserve(numberThreads);
  for (uint16_t i = 0; i < numberThreads; i++) {
    threads_.emplace_back(&SaplingBackingStore::processRequest, this);
  }

  hgTraceHandle_ = traceBus_->subscribeFunction(
      folly::to<std::string>("hg-activitybuffer-", getRepoName().value_or("")),
      [this](const HgImportTraceEvent& event) { this->processHgEvent(event); });
}

SaplingBackingStore::~SaplingBackingStore() {
  queue_.stop();
  for (auto& thread : threads_) {
    thread.join();
  }
}

void SaplingBackingStore::processHgEvent(const HgImportTraceEvent& event) {
  switch (event.eventType) {
    case HgImportTraceEvent::QUEUE:
      // Create a new queued event
    case HgImportTraceEvent::START:
      // Override the queued event with start event
      outstandingHgEvents_.wlock()->insert_or_assign(event.unique, event);
      break;
    case HgImportTraceEvent::FINISH:
      outstandingHgEvents_.wlock()->erase(event.unique);
      break;
    default:
      EDEN_BUG() << "Unknown Hg trace event type: "
                 << enumValue(event.eventType);
  }
  activityBuffer_.addEvent(event);
}

void SaplingBackingStore::setPrefetchBlobCounters(
    ObjectFetchContextPtr context,
    ObjectFetchContext::FetchedSource fetchedSource,
    ObjectFetchContext::FetchResult fetchResult,
    folly::stop_watch<std::chrono::milliseconds> watch) {
  if (fetchResult == ObjectFetchContext::FetchResult::Failure) {
    stats_->increment(&SaplingBackingStoreStats::prefetchBlobFailure);
    return;
  }
  stats_->addDuration(&SaplingBackingStoreStats::prefetchBlob, watch.elapsed());

  if (fetchResult == ObjectFetchContext::FetchResult::Success) {
    stats_->increment(&SaplingBackingStoreStats::prefetchBlobSuccess);
  } else {
    EDEN_BUG() << "Unknown fetch request result: " << enumValue(fetchResult);
  }

  context->setFetchedSource(
      fetchedSource,
      ObjectFetchContext::ObjectType::PrefetchBlob,
      stats_.copy());
}

void SaplingBackingStore::setFetchBlobCounters(
    ObjectFetchContextPtr context,
    ObjectFetchContext::FetchedSource fetchedSource,
    ObjectFetchContext::FetchResult fetchResult,
    folly::stop_watch<std::chrono::milliseconds> watch) {
  if (fetchResult == ObjectFetchContext::FetchResult::Failure) {
    stats_->increment(&SaplingBackingStoreStats::fetchBlobFailure);
    if (dogfoodingHost()) {
      stats_->increment(&SaplingBackingStoreStats::fetchBlobFailureDogfooding);
    }
    return;
  }

  stats_->addDuration(&SaplingBackingStoreStats::fetchBlob, watch.elapsed());

  if (fetchResult == ObjectFetchContext::FetchResult::Success) {
    stats_->increment(&SaplingBackingStoreStats::fetchBlobSuccess);
  } else {
    EDEN_BUG() << "Unknown fetch request result: " << enumValue(fetchResult);
  }

  context->setFetchedSource(
      fetchedSource, ObjectFetchContext::ObjectType::Blob, stats_.copy());

  if (dogfoodingHost()) {
    stats_->increment(&SaplingBackingStoreStats::fetchBlobSuccessDogfooding);
  }
}

void SaplingBackingStore::setBlobCounters(
    ObjectFetchContextPtr context,
    SaplingImportRequest::FetchType fetchType,
    ObjectFetchContext::FetchedSource fetchedSource,
    ObjectFetchContext::FetchResult fetchResult,
    folly::stop_watch<std::chrono::milliseconds> watch) {
  switch (fetchType) {
    case SaplingImportRequest::FetchType::Prefetch:
      setPrefetchBlobCounters(
          context.copy(), fetchedSource, fetchResult, watch);
      break;

    case SaplingImportRequest::FetchType::Fetch:
      setFetchBlobCounters(context.copy(), fetchedSource, fetchResult, watch);
      break;
  }
}

void SaplingBackingStore::processBlobImportRequests(
    std::vector<std::shared_ptr<SaplingImportRequest>>&& requests) {
  XLOGF(DBG4, "Processing blob import batch size={}", requests.size());

  for (auto& request : requests) {
    auto* blobImport = request->getRequest<SaplingImportRequest::BlobImport>();

    // TODO: We could reduce the number of lock acquisitions by adding a batch
    // publish method.
    traceBus_->publish(
        HgImportTraceEvent::start(
            request->getUnique(),
            HgImportTraceEvent::BLOB,
            blobImport->id,
            request->getPriority().getClass(),
            request->getCause(),
            request->getPid()));

    XLOGF(DBG4, "Processing blob request for {}", blobImport->id);
  }

  getBlobBatch(requests, sapling::FetchMode::AllowRemote);
}

void SaplingBackingStore::getBlobBatch(
    const ImportRequestsList& importRequests,
    sapling::FetchMode fetchMode) {
  auto preparedRequests = prepareRequests<SaplingImportRequest::BlobImport>(
      importRequests, SaplingImportObject::BLOB);
  auto importRequestsMap = std::move(preparedRequests.first);
  auto requests = std::move(preparedRequests.second);
  folly::stop_watch<std::chrono::milliseconds> batchWatch;

  nativeGetBlobBatch(
      folly::range(requests),
      fetchMode,
      /*allow_ignore_result=*/false,
      // store_->getBlobBatch is blocking, hence we can take these by reference.
      [&](size_t index, folly::Try<std::unique_ptr<folly::IOBuf>> content) {
        if (content.hasException()) {
          XLOGF(
              DBG4,
              "Failed to import {} from EdenAPI (batch {}/{}): {}",
              requests[index].oid,
              index,
              requests.size(),
              content.exception().what().toStdString());

          logFetchMiss(content.exception(), "blob");
          if (edenFsEventsLogger_) {
            edenFsEventsLogger_->logEvent(
                FetchMiss{
                    repoName_,
                    FetchMiss::Blob,
                    content.exception().what().toStdString(),
                    false, // isRetry
                    dogfoodingHost()});
          }
        } else {
          XLOGF(
              DBG4,
              "Imported {} from EdenAPI (batch: {}/{})",
              requests[index].oid,
              index,
              requests.size());
        }

        const auto& nodeId = requests[index].oid.node();
        XLOGF(DBG9, "Imported Blob node={}", nodeId);
        auto& [importRequestList, watch] = importRequestsMap[nodeId];
        auto result = content.hasException()
            ? folly::Try<BlobPtr>{content.exception()}
            : content.value()
            ? folly::Try{std::make_shared<BlobPtr::element_type>(
                  *content.value())}
            : folly::Try<BlobPtr>{std::runtime_error{fmt::format(
                  "unexpected getBlobBatch nullptr result for {}",
                  requests[index].oid)}};
        for (auto& importRequest : importRequestList) {
          importRequest->getPromise<BlobPtr>()->setWith(
              [&]() -> folly::Try<BlobPtr> { return result; });

          setBlobCounters(
              importRequest->getContext().copy(),
              importRequest->getFetchType(),
              ObjectFetchContext::FetchedSource::Unknown,
              content.hasException() ? ObjectFetchContext::FetchResult::Failure
                                     : ObjectFetchContext::FetchResult::Success,
              batchWatch);
        }

        // Make sure that we're stopping this watch.
        watch.reset();
      });
}

std::shared_ptr<WalkDetector> SaplingBackingStore::createWalkDetector(
    AbsolutePathPiece mountPath) {
  // The detector passes the Rust store back on every report. Aliasing it to
  // this object keeps the store alive for as long as the detector exists.
  std::shared_ptr<sapling::BackingStore> store{
      shared_from_this(), store_.get()};
  return std::make_shared<SaplingWalkDetector>(
      std::move(store),
      sapling::sapling_backingstore_walk_detector(
          *store_.get(),
          rust::Str{mountPath.view().data(), mountPath.view().size()}));
}

folly::Try<BlobPtr> SaplingBackingStore::getBlobFromBackingStore(
    SlOidView slOid,
    const ObjectFetchContextPtr& /*context*/,
    sapling::FetchMode fetchMode) {
  auto node = slOid.node();

  using GetBlobResult = folly::Try<BlobPtr>;

  XLOGF(DBG7, "Importing blob node={} from hgcache", node);
  auto result = sapling_backingstore_get_blob(
      *store_.get(),
      rust::Slice<const uint8_t>{node.getBytes().data(), node.RAW_SIZE},
      fetchMode);

  if (result.error != nullptr) {
    return GetBlobResult{std::move(*result.error)};
  }

  if (result.data != nullptr) {
    return GetBlobResult{
        std::make_shared<BlobPtr::element_type>(std::move(*result.data))};
  } else {
    return GetBlobResult{nullptr};
  }
}

void SaplingBackingStore::processTreeImportRequests(
    std::vector<std::shared_ptr<SaplingImportRequest>>&& requests) {
  for (auto& request : requests) {
    auto* treeImport = request->getRequest<SaplingImportRequest::TreeImport>();

    // TODO: We could reduce the number of lock acquisitions by adding a batch
    // publish method.
    traceBus_->publish(
        HgImportTraceEvent::start(
            request->getUnique(),
            HgImportTraceEvent::TREE,
            treeImport->id,
            request->getPriority().getClass(),
            request->getCause(),
            request->getPid()));

    XLOGF(DBG4, "Processing tree request for {}", treeImport->id);
  }

  getTreeBatch(requests, sapling::FetchMode::AllowRemote);
}

void SaplingBackingStore::getTreeBatch(
    const ImportRequestsList& importRequests,
    sapling::FetchMode fetch_mode) {
  if (importRequests.empty()) {
    return;
  }

  folly::stop_watch<std::chrono::milliseconds> batchWatch;

  auto preparedRequests = prepareRequests<SaplingImportRequest::TreeImport>(
      importRequests, SaplingImportObject::TREE);
  auto importRequestsMap = std::move(preparedRequests.first);
  auto requests = std::move(preparedRequests.second);

  XLOGF(
      DBG7,
      "Import batch of trees with size: {}, first path: {}",
      requests.size(),
      requests[0].oid.path());

  std::vector<sapling::Request> raw_requests;
  raw_requests.reserve(requests.size());
  for (auto& request : requests) {
    raw_requests.emplace_back(
        sapling::Request{
            request.oid.rustData(),
            request.cause,
        });
  }

  auto callback = [&](size_t index, folly::Try<TreePtr> content) mutable {
    // Convert PermissionDenied errors to restricted trees, matching the
    // single-fetch path in getNativeTree.
    content = convertPermissionDeniedToRestrictedTree(
        std::move(content), SaplingObjectId{requests[index].oid}.oid());

    if (content.hasException()) {
      XLOGF(
          DBG4,
          "Failed to import {} from EdenAPI (batch tree {}/{}): {}",
          requests[index].oid,
          index,
          requests.size(),
          content.exception().what().toStdString());
      stats_->increment(&SaplingBackingStoreStats::fetchTreeFailure);
      logFetchMiss(content.exception(), "tree");
      if (dogfoodingHost()) {
        stats_->increment(
            &SaplingBackingStoreStats::fetchTreeFailureDogfooding);
      }
    } else {
      XLOGF(
          DBG4,
          "Imported {} from EdenAPI (batch tree: {}/{})",
          requests[index].oid,
          index,
          requests.size());
      stats_->increment(&SaplingBackingStoreStats::fetchTreeSuccess);
      if (dogfoodingHost()) {
        stats_->increment(
            &SaplingBackingStoreStats::fetchTreeSuccessDogfooding);
      }
    }

    stats_->addDuration(
        &SaplingBackingStoreStats::fetchTree, batchWatch.elapsed());

    const auto& nodeId = requests[index].oid.node();
    XLOGF(DBG9, "Imported Tree node={}", nodeId);
    auto& [importRequestList, watch] = importRequestsMap[nodeId];
    for (auto& importRequest : importRequestList) {
      importRequest->getPromise<TreePtr>()->setValue(content);
    }

    // Make sure that we're stopping this watch.
    watch.reset();
  };

  faultInjector_.check("SaplingBackingStore::getTreeBatch", "");

  sapling_backingstore_get_tree_batch(
      *store_.get(),
      rust::Slice<const sapling::Request>{
          raw_requests.data(), raw_requests.size()},
      fetch_mode,
      objectIdFormat_,
      caseSensitive_ == facebook::eden::CaseSensitivity::Sensitive,
      std::make_shared<sapling::GetTreeBatchResolver>(callback));
}

template <typename T>
std::pair<
    SaplingBackingStore::ImportRequestsMap,
    std::vector<sapling::SaplingRequest>>
SaplingBackingStore::prepareRequests(
    const ImportRequestsList& importRequests,
    const SaplingImportObject& requestType) {
  // TODO: extract each ClientRequestInfo from importRequests into a
  // sapling::ClientRequestInfo and pass them with the corresponding
  // sapling::NodeId

  // Group requests by slOid to ensure no duplicates in fetch requests.
  ImportRequestsMap importRequestsMap;
  for (const auto& importRequest : importRequests) {
    auto nodeId = importRequest->getRequest<T>()->id.node();

    // Look for and log duplicates.
    const auto& importRequestsEntry = importRequestsMap.find(nodeId);
    if (importRequestsEntry != importRequestsMap.end()) {
      XLOGF(
          DBG9,
          "Duplicate {} fetch request with slOid: {}",
          stringOfSaplingImportObject(requestType),
          nodeId);
      auto& importRequestList = importRequestsEntry->second.first;

      // Only look for mismatched requests if logging level is high enough.
      // Make sure this level is the same as the XLOG_IF statement below.
      if (XLOG_IS_ON(DBG9)) {
        // Log requests that do not have the same hash (ObjectId).
        // This happens when two paths (file or directory) have same content.
        for (const auto& priorRequest : importRequestList) {
          XLOGF_IF(
              DBG9,
              UNLIKELY(
                  priorRequest->template getRequest<T>()->id !=
                  importRequest->getRequest<T>()->id),
              "{} requests have the same sl hash but different id (ObjectId). "
              "This should not happen. Previous request: slOid.node='{}', slOid.path='{}'; "
              "current request: slOid.node ='{}', slOid.path='{}'.",
              stringOfSaplingImportObject(requestType),
              priorRequest->template getRequest<T>()->id.node(),
              priorRequest->template getRequest<T>()->id.path(),
              importRequest->getRequest<T>()->id.node(),
              importRequest->getRequest<T>()->id.path());
        }
      }

      importRequestList.emplace_back(importRequest);
    } else {
      std::vector<std::shared_ptr<SaplingImportRequest>> requests(
          {importRequest});
      switch (requestType) {
        case SaplingImportObject::TREE:
          importRequestsMap.emplace(
              nodeId, make_pair(requests, &liveBatchedTreeWatches_));
          break;
        case SaplingImportObject::TREE_AUX:
          importRequestsMap.emplace(
              nodeId, make_pair(requests, &liveBatchedTreeAuxWatches_));
          break;
        case SaplingImportObject::BLOB:
          importRequestsMap.emplace(
              nodeId, make_pair(requests, &liveBatchedBlobWatches_));
          break;
        case SaplingImportObject::BLOB_AUX:
          importRequestsMap.emplace(
              nodeId, make_pair(requests, &liveBatchedBlobAuxWatches_));
          break;
        // The following types cannot get here. It is just for completeness
        case SaplingImportObject::BATCHED_TREE:
        case SaplingImportObject::BATCHED_TREE_AUX:
        case SaplingImportObject::BATCHED_BLOB:
        case SaplingImportObject::BATCHED_BLOB_AUX:
        case SaplingImportObject::PREFETCH:
          break;
      }
    }
  }

  // Indexable vector of nodeIds - required by sapling::BackingStore API.
  // In addition, we pass the fetchCause for each request. If we have multiple
  // fetchCauses for the same nodeID, we will take the highest priority one.
  //
  // NOTE: Currently, backingstore ignores the fetchCause of the request. In the
  // future, backingstore may use different fetchCauses to change the fetch
  // behavior.
  std::vector<sapling::SaplingRequest> requests;
  for (const auto& importRequestsIdPair : importRequestsMap) {
    const ImportRequestsList& importRequestsForId =
        importRequestsIdPair.second.first;
    ObjectFetchContext::Cause fetchCause =
        getHighestPriorityFetchCause(importRequestsForId);
    requests.emplace_back(
        importRequestsForId[0]->getRequest<T>()->id,
        fetchCause,
        importRequestsForId[0]->getContext().copy());
  }

  return std::make_pair(std::move(importRequestsMap), std::move(requests));
}

// The priority is defined in ObjectFetchContext::Cause
// FS -> Thrift -> Glob -> Prefetch -> InodeGc -> Unknown
ObjectFetchContext::Cause SaplingBackingStore::getHighestPriorityFetchCause(
    const ImportRequestsList& importRequestsForId) const {
  ObjectFetchContext::Cause highestPriorityCause =
      ObjectFetchContext::Cause::Unknown;
  for (const auto& request : importRequestsForId) {
    if (request) {
      highestPriorityCause =
          std::max(highestPriorityCause, request->getCause());
    }
  }
  return highestPriorityCause;
}

void SaplingBackingStore::processBlobAuxImportRequests(
    std::vector<std::shared_ptr<SaplingImportRequest>>&& requests) {
  folly::stop_watch<std::chrono::milliseconds> watch;

  for (auto& request : requests) {
    auto* blobAuxImport =
        request->getRequest<SaplingImportRequest::BlobAuxImport>();

    // TODO: We could reduce the number of lock acquisitions by adding a batch
    // publish method.
    traceBus_->publish(
        HgImportTraceEvent::start(
            request->getUnique(),
            HgImportTraceEvent::BLOB_AUX,
            blobAuxImport->id,
            request->getPriority().getClass(),
            request->getCause(),
            request->getPid()));

    XLOGF(DBG4, "Processing blob aux request for {}", blobAuxImport->id);
  }

  getBlobAuxDataBatch(requests, sapling::FetchMode::AllowRemote);

  {
    for (auto& request : requests) {
      auto* promise = request->getPromise<BlobAuxDataPtr>();
      if (promise->isFulfilled()) {
        stats_->addDuration(
            &SaplingBackingStoreStats::fetchBlobAuxData, watch.elapsed());
        stats_->increment(&SaplingBackingStoreStats::fetchBlobAuxDataSuccess);
        continue;
      }

      // The code waiting on the promise will fallback to fetching the Blob to
      // compute the blob aux data. We can't trigger a blob fetch here without
      // the risk of running into a deadlock: if all import thread are in this
      // code path, there are no free importer to fetch blobs.
      stats_->increment(&SaplingBackingStoreStats::fetchBlobAuxDataFailure);
      promise->setValue(nullptr);
    }
  }
}

void SaplingBackingStore::processTreeAuxImportRequests(
    std::vector<std::shared_ptr<SaplingImportRequest>>&& requests) {
  folly::stop_watch<std::chrono::milliseconds> watch;

  for (auto& request : requests) {
    auto* treeAuxImport =
        request->getRequest<SaplingImportRequest::TreeAuxImport>();

    // TODO: We could reduce the number of lock acquisitions by adding a batch
    // publish method.
    traceBus_->publish(
        HgImportTraceEvent::start(
            request->getUnique(),
            HgImportTraceEvent::TREE_AUX,
            treeAuxImport->id,
            request->getPriority().getClass(),
            request->getCause(),
            request->getPid()));

    XLOGF(DBG4, "Processing tree aux request for {}", treeAuxImport->id);
  }

  getTreeAuxDataBatch(requests, sapling::FetchMode::AllowRemote);

  {
    for (auto& request : requests) {
      auto* promise = request->getPromise<TreeAuxDataPtr>();
      if (promise->isFulfilled()) {
        stats_->addDuration(
            &SaplingBackingStoreStats::fetchTreeAuxData, watch.elapsed());
        stats_->increment(&SaplingBackingStoreStats::fetchTreeAuxDataSuccess);
        continue;
      }

      stats_->increment(&SaplingBackingStoreStats::fetchTreeAuxDataFailure);
      promise->setValue(nullptr);
    }
  }
}

void SaplingBackingStore::getTreeAuxDataBatch(
    const ImportRequestsList& importRequests,
    sapling::FetchMode fetch_mode) {
  auto preparedRequests = prepareRequests<SaplingImportRequest::TreeAuxImport>(
      importRequests, SaplingImportObject::TREE_AUX);
  auto importRequestsMap = std::move(preparedRequests.first);
  auto requests = std::move(preparedRequests.second);

  XLOGF(DBG7, "Import tree aux data with size: {}", requests.size());

  std::vector<sapling::Request> raw_requests;
  raw_requests.reserve(requests.size());
  for (auto& request : requests) {
    raw_requests.push_back(
        sapling::Request{
            request.oid.rustData(),
            request.cause,
        });
  }

  // store_.getTreeAuxDataBatch is blocking, hence we can take these by
  // reference.
  auto callback =
      [&](size_t index,
          folly::Try<std::shared_ptr<sapling::TreeAuxData>> auxTry) {
        if (auxTry.hasException()) {
          XLOGF(
              DBG6,
              "Failed to import aux data {} from EdenAPI (batch {}/{}): {}",
              requests[index].oid,
              index,
              requests.size(),
              auxTry.exception().what().toStdString());
        } else {
          XLOGF(
              DBG6,
              "Imported aux data {} from EdenAPI (batch: {}/{})",
              requests[index].oid,
              index,
              requests.size());
        }

        if (auxTry.hasException()) {
          logFetchMiss(auxTry.exception(), "tree_aux");
          if (edenFsEventsLogger_) {
            edenFsEventsLogger_->logEvent(
                FetchMiss{
                    repoName_,
                    FetchMiss::TreeAuxData,
                    auxTry.exception().what().toStdString(),
                    false, // isRetry
                    dogfoodingHost()});
          }

          return;
        }

        const auto& nodeId = requests[index].oid.node();
        XLOGF(DBG9, "Imported TreeAuxData={}", nodeId);
        auto& [importRequestList, watch] = importRequestsMap[nodeId];
        folly::Try<TreeAuxDataPtr> result;
        if (auxTry.hasException()) {
          result = folly::Try<TreeAuxDataPtr>{auxTry.exception()};
        } else {
          auto& aux = auxTry.value();
          result = folly::Try{std::make_shared<TreeAuxDataPtr::element_type>(
              Hash32{std::move(aux->digest_hash)}, aux->digest_size)};
        }
        for (auto& importRequest : importRequestList) {
          importRequest->getPromise<TreeAuxDataPtr>()->setWith(
              [&]() -> folly::Try<TreeAuxDataPtr> { return result; });
        }

        // Make sure that we're stopping this watch.
        watch.reset();
      };

  sapling_backingstore_get_tree_aux_batch(
      *store_.get(),
      rust::Slice<const sapling::Request>{
          raw_requests.data(), raw_requests.size()},
      fetch_mode,
      std::make_shared<sapling::GetTreeAuxBatchResolver>(callback));
}

void SaplingBackingStore::getBlobAuxDataBatch(
    const ImportRequestsList& importRequests,
    sapling::FetchMode fetch_mode) {
  auto preparedRequests = prepareRequests<SaplingImportRequest::BlobAuxImport>(
      importRequests, SaplingImportObject::BLOB_AUX);
  auto importRequestsMap = std::move(preparedRequests.first);
  auto requests = std::move(preparedRequests.second);

  auto count = requests.size();

  XLOGF(DBG7, "Import blob aux data with size: {}", requests.size());

  std::vector<sapling::Request> raw_requests;
  raw_requests.reserve(count);
  for (auto& request : requests) {
    raw_requests.push_back(
        sapling::Request{
            request.oid.rustData(),
            request.cause,
        });
  }
  // store_.getBlobAuxDataBatch is blocking, hence we can take these by
  // reference.
  auto callback =
      [&](size_t index,
          folly::Try<std::shared_ptr<sapling::FileAuxData>> auxTry) {
        if (auxTry.hasException()) {
          XLOGF(
              DBG4,
              "Failed to import aux data {} from EdenAPI (batch {}/{}): {}",
              requests[index].oid,
              index,
              requests.size(),
              auxTry.exception().what().toStdString());
        } else {
          XLOGF(
              DBG4,
              "Imported aux data {} from EdenAPI (batch: {}/{})",
              requests[index].oid,
              index,
              requests.size());
        }

        if (auxTry.hasException()) {
          logFetchMiss(auxTry.exception(), "blob_aux");
          if (fetch_mode != sapling::FetchMode::RemoteOnly) {
            if (edenFsEventsLogger_) {
              edenFsEventsLogger_->logEvent(
                  FetchMiss{
                      repoName_,
                      FetchMiss::BlobAuxData,
                      auxTry.exception().what().toStdString(),
                      false, // isRetry
                      dogfoodingHost()});
            }
          }

          return;
        }

        const auto& nodeId = requests[index].oid.node();
        XLOGF(DBG9, "Imported BlobAuxData={}", nodeId);
        auto& [importRequestList, watch] = importRequestsMap[nodeId];
        folly::Try<BlobAuxDataPtr> result;
        if (auxTry.hasException()) {
          result = folly::Try<BlobAuxDataPtr>{auxTry.exception()};
        } else {
          auto& aux = auxTry.value();
          result = folly::Try{std::make_shared<BlobAuxDataPtr::element_type>(
              Hash20{std::move(aux->content_sha1)},
              Hash32{std::move(aux->content_blake3)},
              aux->total_size)};
        }
        for (auto& importRequest : importRequestList) {
          importRequest->getPromise<BlobAuxDataPtr>()->setWith(
              [&]() -> folly::Try<BlobAuxDataPtr> { return result; });
        }

        // Make sure that we're stopping this watch.
        watch.reset();
      };

  sapling_backingstore_get_file_aux_batch(
      *store_.get(),
      rust::Slice<const sapling::Request>{
          raw_requests.data(), raw_requests.size()},
      fetch_mode,
      std::make_shared<sapling::GetFileAuxBatchResolver>(callback));
}

void SaplingBackingStore::processRequest() {
  folly::setThreadName("hgqueue");
  for (;;) {
    auto requests = queue_.dequeue();

    if (requests.empty()) {
      break;
    }

    const auto& first = requests.at(0);

    if (first->isType<SaplingImportRequest::BlobImport>()) {
      processBlobImportRequests(std::move(requests));
    } else if (first->isType<SaplingImportRequest::TreeImport>()) {
      processTreeImportRequests(std::move(requests));
    } else if (first->isType<SaplingImportRequest::BlobAuxImport>()) {
      processBlobAuxImportRequests(std::move(requests));
    } else if (first->isType<SaplingImportRequest::TreeAuxImport>()) {
      processTreeAuxImportRequests(std::move(requests));
    } else {
      XLOGF(DFATAL, "Unknown import request type: {}", first->getType());
    }
  }
}

ObjectComparison SaplingBackingStore::compareObjectsById(
    const ObjectId& one,
    const ObjectId& two) {
  // This is by far the common case, so check it first:
  if (one.bytesEqual(two)) {
    return ObjectComparison::Identical;
  }

  // Now parse the object IDs and read their rev hashes.
  auto oneProxy = SlOid{one};
  auto twoProxy = SlOid{two};

  // If the rev hashes are the same, we know the contents are the same.
  if (oneProxy.node() == twoProxy.node()) {
    return ObjectComparison::Identical;
  }

  // If rev hashes differ, and hg IDs aren't bijective, then we don't know
  // whether the IDs refer to the same contents or not.
  //
  // Mercurial's blob ids also include history aux data, so there may be
  // multiple different blob ids for the same file contents.
  return ObjectComparison::Unknown;
}

ObjectComparison SaplingBackingStore::compareRootsById(
    const RootId& one,
    const RootId& two) {
  // In Mercurial/Sapling, RootIds are commit hashes, which are bijective.
  // One commit hash always refers to exactly one commit.
  return one == two ? ObjectComparison::Identical : ObjectComparison::Different;
}

RootId SaplingBackingStore::parseRootId(folly::StringPiece rootId) {
  // rootId can be 20-byte binary or 40-byte hex. Canonicalize, unconditionally
  // returning 40-byte hex.
  return RootId{hash20FromThrift(rootId).toString()};
}

std::string SaplingBackingStore::renderRootId(const RootId& rootId) {
  // In memory, root IDs are stored as 40-byte hex. Thrift clients generally
  // expect 20-byte binary for Mercurial commit hashes, so re-encode that way.
  auto& value = rootId.value();
  if (value.size() == 40) {
    return folly::unhexlify(value);
  } else {
    XCHECK_EQ(0u, value.size());
    // Default-constructed RootId is the Mercurial null hash.
    return folly::unhexlify(kZeroHash.toString());
  }
}

std::string SaplingBackingStore::displayRootId(const RootId& rootId) {
  // Root IDs are stored as 40-byte hex (the commit hash).
  return fmt::format("commit={}", rootId.value());
}

ObjectId SaplingBackingStore::staticParseObjectId(folly::StringPiece objectId) {
  if (objectId.size() == 40) {
    return SlOid{Hash20{objectId}}.oid();
  }

  if (objectId.size() < 41) {
    throwf<std::invalid_argument>("sl object ID too short: {}", objectId);
  }

  if (objectId[40] != ':') {
    throwf<std::invalid_argument>(
        "missing separator colon in sl object ID: {}", objectId);
  }

  Hash20 node{objectId.subpiece(0, 40)};
  RelativePathPiece path{objectId.subpiece(41)};
  return SlOid{node, path}.oid();
}

std::string SaplingBackingStore::staticRenderObjectId(
    const ObjectId& objectId) {
  auto slOid = SlOid{objectId};
  if (slOid.path().empty()) {
    return slOid.node().toString();
  }
  return fmt::format("{}:{}", slOid.node(), slOid.path());
}

folly::SemiFuture<BackingStore::GetTreeAuxResult>
SaplingBackingStore::getTreeAuxData(
    const ObjectId& id,
    const ObjectFetchContextPtr& context) {
  DurationScope<EdenStats> scope{
      stats_, &SaplingBackingStoreStats::getTreeAuxData};

  SlOidView slOid{id};

  logBackingStoreFetch(
      *context,
      folly::Range{&slOid, 1},
      ObjectFetchContext::ObjectType::TreeAuxData);

  auto auxData = getLocalTreeAuxData(slOid);
  if (auxData.hasValue() && auxData.value()) {
    stats_->increment(&SaplingBackingStoreStats::fetchTreeAuxDataSuccess);
    stats_->increment(&SaplingBackingStoreStats::fetchTreeAuxDataLocal);
    return folly::makeSemiFuture(
        GetTreeAuxResult{
            std::move(auxData.value()),
            ObjectFetchContext::Origin::FromDiskCache});
  }

  return getTreeAuxDataEnqueue(slOid, context)
      .ensure([scope = std::move(scope)] {})
      .semi();
}

folly::coro::now_task<BackingStore::GetTreeAuxResult>
SaplingBackingStore::co_getTreeAuxData(
    const ObjectId& id,
    const ObjectFetchContextPtr& context) {
  // Move the DurationScope into a folly::makeGuard so the duration is
  // recorded when the guard goes out of scope at the end of this coroutine
  // frame (success OR exception). This is the direct coroutine port of the
  // futures path's `.ensure([scope = std::move(scope)] {})`.
  auto scopeGuard = folly::makeGuard(
      [scope = DurationScope<EdenStats>{
           stats_, &SaplingBackingStoreStats::getTreeAuxData}]() mutable {});

  SlOidView slOid{id};

  logBackingStoreFetch(
      *context,
      folly::Range{&slOid, 1},
      ObjectFetchContext::ObjectType::TreeAuxData);

  auto auxData = getLocalTreeAuxData(slOid);
  if (auxData.hasValue() && auxData.value()) {
    stats_->increment(&SaplingBackingStoreStats::fetchTreeAuxDataSuccess);
    stats_->increment(&SaplingBackingStoreStats::fetchTreeAuxDataLocal);
    co_return GetTreeAuxResult{
        std::move(auxData.value()), ObjectFetchContext::Origin::FromDiskCache};
  }

  co_return co_await getTreeAuxDataEnqueue(slOid, context).semi();
}

ImmediateFuture<BackingStore::GetTreeAuxResult>
SaplingBackingStore::getTreeAuxDataEnqueue(
    const SlOid& slOid,
    const ObjectFetchContextPtr& context) {
  auto self = shared_from_this();
  auto getTreeAuxFuture = makeImmediateFutureWith([&] {
    XLOGF(DBG4, "making tree aux data import request for {}", slOid);
    auto requestContext = context.copy();
    auto request =
        SaplingImportRequest::makeTreeAuxImportRequest(slOid, requestContext);
    auto unique = request->getUnique();

    auto importTracker = std::make_unique<RequestMetricsScope>(
        &self->pendingImportTreeAuxWatches_);
    self->traceBus_->publish(
        HgImportTraceEvent::queue(
            unique,
            HgImportTraceEvent::TREE_AUX,
            slOid,
            context->getPriority().getClass(),
            context->getCause(),
            context->getClientPid()));

    return self->queue_.enqueueTreeAux(std::move(request))
        .ensure([self,
                 unique,
                 slOid,
                 context = context.copy(),
                 importTracker = std::move(importTracker)]() {
          self->traceBus_->publish(
              HgImportTraceEvent::finish(
                  unique,
                  HgImportTraceEvent::TREE_AUX,
                  slOid,
                  context->getPriority().getClass(),
                  context->getCause(),
                  context->getClientPid(),
                  context->getFetchedSource()));
        });
  });

  return std::move(getTreeAuxFuture)
      .thenTry([self, slOid](folly::Try<TreeAuxDataPtr>&& result) {
        self->queue_.markImportAsFinished<TreeAuxDataPtr::element_type>(
            slOid, result);
        auto treeAux = std::move(result).value();
        return GetTreeAuxResult{
            std::move(treeAux), ObjectFetchContext::Origin::FromNetworkFetch};
      });
}

folly::Try<TreeAuxDataPtr> SaplingBackingStore::getLocalTreeAuxData(
    SlOidView slOid) {
  auto node = slOid.node();

  XLOGF(DBG7, "Importing tree aux data node={} from hgcache", node);

  using GetTreeAuxDataResult = folly::Try<TreeAuxDataPtr>;

  auto result = sapling_backingstore_get_tree_aux(
      *store_.get(),
      rust::Slice<const uint8_t>{node.getBytes().data(), node.RAW_SIZE},
      sapling::FetchMode::LocalOnly);

  if (result.error != nullptr) {
    return GetTreeAuxDataResult{std::move(*result.error)};
  }

  if (result.data != nullptr) {
    return GetTreeAuxDataResult{
        std::make_shared<TreeAuxDataPtr::element_type>(TreeAuxData{
            Hash32{std::move(result.data->digest_hash)},
            result.data->digest_size})};
  } else {
    return GetTreeAuxDataResult{nullptr};
  }
}

folly::SemiFuture<BackingStore::GetTreeResult> SaplingBackingStore::getTree(
    const ObjectId& id,
    const ObjectFetchContextPtr& context) {
  DurationScope<EdenStats> scope{stats_, &SaplingBackingStoreStats::getTree};

  SlOidView slOid{id};

  logBackingStoreFetch(
      *context, folly::Range{&slOid, 1}, ObjectFetchContext::ObjectType::Tree);

  if (auto tree = getTreeLocal(slOid, context)) {
    XLOGF(DBG5, "imported tree {} from hgcache", slOid);
    stats_->increment(&SaplingBackingStoreStats::fetchTreeSuccess);
    if (dogfoodingHost()) {
      stats_->increment(&SaplingBackingStoreStats::fetchTreeSuccessDogfooding);
    }
    stats_->increment(&SaplingBackingStoreStats::fetchTreeLocal);
    return folly::makeSemiFuture(
        GetTreeResult{
            std::move(tree), ObjectFetchContext::Origin::FromDiskCache});
  }

  return getTreeEnqueue(slOid, context)
      .ensure([scope = std::move(scope)] {})
      .semi();
}

folly::coro::now_task<BackingStore::GetTreeResult>
SaplingBackingStore::co_getTree(
    const ObjectId& id,
    const ObjectFetchContextPtr& context) {
  DurationScope<EdenStats> scope{stats_, &SaplingBackingStoreStats::getTree};

  SlOidView slOid{id};

  logBackingStoreFetch(
      *context, folly::Range{&slOid, 1}, ObjectFetchContext::ObjectType::Tree);

  if (auto tree = getTreeLocal(slOid, context)) {
    XLOGF(DBG5, "imported tree {} from hgcache", slOid);
    stats_->increment(&SaplingBackingStoreStats::fetchTreeSuccess);
    if (dogfoodingHost()) {
      stats_->increment(&SaplingBackingStoreStats::fetchTreeSuccessDogfooding);
    }
    stats_->increment(&SaplingBackingStoreStats::fetchTreeLocal);
    co_return GetTreeResult{
        std::move(tree), ObjectFetchContext::Origin::FromDiskCache};
  }
  co_return co_await co_getTreeEnqueue(slOid, context);
}

ImmediateFuture<BackingStore::GetTreeResult>
SaplingBackingStore::getTreeEnqueue(
    const SlOid& slOid,
    const ObjectFetchContextPtr& context) {
  auto self = shared_from_this();
  return faultInjector_
      .checkAsync(
          "SaplingBackingStore::getTreeEnqueue", slOid.node().toString())
      .thenValue(
          [self, slOid = slOid, context = context.copy()](auto&&) mutable {
            auto getTreeFuture = makeImmediateFutureWith([&] {
              auto requestContext = context.copy();
              auto request = SaplingImportRequest::makeTreeImportRequest(
                  slOid, requestContext);
              uint64_t unique = request->getUnique();

              auto importTracker = std::make_unique<RequestMetricsScope>(
                  &self->pendingImportTreeWatches_);
              self->traceBus_->publish(
                  HgImportTraceEvent::queue(
                      unique,
                      HgImportTraceEvent::TREE,
                      slOid,
                      context->getPriority().getClass(),
                      context->getCause(),
                      context->getClientPid()));

              return self->queue_.enqueueTree(std::move(request))
                  .ensure([self,
                           unique,
                           slOid,
                           context = context.copy(),
                           importTracker = std::move(importTracker)]() {
                    self->traceBus_->publish(
                        HgImportTraceEvent::finish(
                            unique,
                            HgImportTraceEvent::TREE,
                            slOid,
                            context->getPriority().getClass(),
                            context->getCause(),
                            context->getClientPid(),
                            context->getFetchedSource()));
                  });
            });

            return std::move(getTreeFuture)
                .thenTry([self, slOid](folly::Try<TreePtr>&& result) {
                  self->queue_.markImportAsFinished<TreePtr::element_type>(
                      slOid, result);
                  auto tree = std::move(result).value();
                  return GetTreeResult{
                      std::move(tree),
                      ObjectFetchContext::Origin::FromNetworkFetch};
                });
          });
}

folly::coro::now_task<BackingStore::GetTreeResult>
SaplingBackingStore::co_getTreeEnqueue(
    const SlOid& slOid,
    const ObjectFetchContextPtr& context) {
  auto self = shared_from_this();
  co_await self->faultInjector_.co_checkAsync(
      "SaplingBackingStore::co_getTreeEnqueue", slOid.node().toString());
  XLOGF(DBG4, "making tree import request for {}", slOid);
  auto requestContext = context.copy();
  auto request =
      SaplingImportRequest::makeTreeImportRequest(slOid, requestContext);
  uint64_t unique = request->getUnique();

  auto importTracker =
      std::make_unique<RequestMetricsScope>(&self->pendingImportTreeWatches_);

  self->traceBus_->publish(
      HgImportTraceEvent::queue(
          unique,
          HgImportTraceEvent::TREE,
          slOid,
          context->getPriority().getClass(),
          context->getCause(),
          context->getClientPid()));
  auto guard = folly::makeGuard([&unique, &slOid, &context, self] {
    self->traceBus_->publish(
        HgImportTraceEvent::finish(
            unique,
            HgImportTraceEvent::TREE,
            slOid,
            context->getPriority().getClass(),
            context->getCause(),
            context->getClientPid(),
            context->getFetchedSource()));
  });

  folly::Try<TreePtr> result;
  try {
    auto tree = co_await self->queue_.co_enqueueTree(std::move(request));
    result = folly::Try<TreePtr>{tree};

    self->queue_.markImportAsFinished<TreePtr::element_type>(slOid, result);

    co_return BackingStore::GetTreeResult{
        std::move(tree), ObjectFetchContext::Origin::FromNetworkFetch};
  } catch (const std::exception&) {
    result.emplaceException(std::current_exception());
    self->queue_.markImportAsFinished<TreePtr::element_type>(slOid, result);
    throw;
  }
}

TreePtr SaplingBackingStore::getTreeLocal(
    SlOidView slOid,
    const ObjectFetchContextPtr& context) {
  auto tree = getNativeTree(slOid, context, sapling::FetchMode::LocalOnly);

  if (tree.hasValue()) {
    return std::move(tree).value();
  } else {
    return nullptr;
  }
}

folly::Try<TreePtr> SaplingBackingStore::getTreeRemote(
    SlOidView slOid,
    const ObjectFetchContextPtr& context) {
  return getNativeTree(
      slOid,
      context,
      sapling::FetchMode::RemoteOnly /*, sapling::ClientRequestInfo(context)*/);
}

TreePtr SaplingBackingStore::makeRestrictedTree(ObjectId id) const {
  XLOGF(DBG7, "Creating restricted tree for path ACL restriction: {}", id);
  return std::make_shared<Tree>(
      Tree::Restricted{}, Tree::container{caseSensitive_}, std::move(id));
}

folly::Try<TreePtr>
SaplingBackingStore::convertPermissionDeniedToRestrictedTree(
    folly::Try<TreePtr> content,
    ObjectId id) const {
  if (content.hasException()) {
    if (auto* err = content.exception()
                        .get_exception<sapling::SaplingBackingStoreError>()) {
      if (err->kind() == sapling::BackingStoreErrorKind::PermissionDenied) {
        return folly::Try<TreePtr>{makeRestrictedTree(std::move(id))};
      }
    }
  }
  return content;
}

folly::Try<facebook::eden::TreePtr> SaplingBackingStore::getNativeTree(
    SlOidView slOid,
    const ObjectFetchContextPtr& /*context*/,
    sapling::FetchMode fetch_mode) {
  XLOGF(DBG7, "Importing tree {} from hgcache", slOid);
  return folly::makeTryWith([&] {
    auto node = slOid.node();

    sapling::TreeBuilder tb =
        sapling::TreeBuilder{slOid, caseSensitive_, objectIdFormat_};

    auto result = sapling_backingstore_get_tree(
        *store_.get(),
        rust::Slice<const uint8_t>{node.data(), node.RAW_SIZE},
        tb,
        fetch_mode);

    if (result.error != nullptr) {
      if (result.error->kind() ==
          sapling::BackingStoreErrorKind::PermissionDenied) {
        return makeRestrictedTree(SaplingObjectId{slOid}.oid());
      }
      throw std::move(*result.error);
    }

    return tb.build();
  });
}

folly::SemiFuture<BackingStore::GetBlobResult> SaplingBackingStore::getBlob(
    const ObjectId& id,
    const ObjectFetchContextPtr& context) {
  DurationScope<EdenStats> scope{stats_, &SaplingBackingStoreStats::getBlob};

  SlOidView slOid{id};

  logBackingStoreFetch(
      *context, folly::Range{&slOid, 1}, ObjectFetchContext::ObjectType::Blob);

  auto blob = getBlobLocal(slOid, context);
  if (blob.hasValue() && blob.value()) {
    stats_->increment(&SaplingBackingStoreStats::fetchBlobSuccess);
    if (dogfoodingHost()) {
      stats_->increment(&SaplingBackingStoreStats::fetchBlobSuccessDogfooding);
    }
    stats_->increment(&SaplingBackingStoreStats::fetchBlobLocal);
    return folly::makeSemiFuture(
        GetBlobResult{
            std::move(blob.value()),
            ObjectFetchContext::Origin::FromDiskCache});
  }

  return getBlobEnqueue(slOid, context, SaplingImportRequest::FetchType::Fetch)
      .ensure([scope = std::move(scope)] {})
      .semi();
}

folly::coro::Task<BackingStore::GetBlobResult> SaplingBackingStore::co_getBlob(
    const ObjectId& id,
    const ObjectFetchContextPtr& context) {
  DurationScope<EdenStats> scope{stats_, &SaplingBackingStoreStats::getBlob};

  SlOidView slOid{id};

  logBackingStoreFetch(
      *context, folly::Range{&slOid, 1}, ObjectFetchContext::ObjectType::Blob);

  auto blob = getBlobLocal(slOid, context);
  if (blob.hasValue() && blob.value()) {
    stats_->increment(&SaplingBackingStoreStats::fetchBlobSuccess);
    if (dogfoodingHost()) {
      stats_->increment(&SaplingBackingStoreStats::fetchBlobSuccessDogfooding);
    }
    stats_->increment(&SaplingBackingStoreStats::fetchBlobLocal);
    co_return BackingStore::GetBlobResult{
        std::move(blob.value()), ObjectFetchContext::Origin::FromDiskCache};
  }
  co_return co_await co_getBlobEnqueue(
      slOid, context, SaplingImportRequest::FetchType::Fetch);
}

ImmediateFuture<BackingStore::GetBlobResult>
SaplingBackingStore::getBlobEnqueue(
    const SlOid& slOid,
    const ObjectFetchContextPtr& context,
    const SaplingImportRequest::FetchType fetch_type) {
  auto self = shared_from_this();
  auto enqueue = [](const std::shared_ptr<SaplingBackingStore>& self,
                    const SlOid& slOid,
                    const ObjectFetchContextPtr& context,
                    SaplingImportRequest::FetchType fetch_type) {
    auto getBlobFuture = makeImmediateFutureWith([&] {
      XLOGF(DBG4, "making blob import request for {}", slOid);
      auto requestContext = context.copy();
      auto request =
          SaplingImportRequest::makeBlobImportRequest(slOid, requestContext);
      request->setFetchType(fetch_type);
      auto unique = request->getUnique();
      std::unique_ptr<RequestMetricsScope> importTracker;
      switch (fetch_type) {
        case SaplingImportRequest::FetchType::Fetch:
          importTracker = std::make_unique<RequestMetricsScope>(
              &self->pendingImportBlobWatches_);
          break;
        case SaplingImportRequest::FetchType::Prefetch:
          importTracker = std::make_unique<RequestMetricsScope>(
              &self->pendingImportPrefetchWatches_);
          break;
      }
      self->traceBus_->publish(
          HgImportTraceEvent::queue(
              unique,
              HgImportTraceEvent::BLOB,
              slOid,
              context->getPriority().getClass(),
              context->getCause(),
              context->getClientPid()));

      return self->queue_.enqueueBlob(std::move(request))
          .ensure([self,
                   unique,
                   slOid,
                   context = context.copy(),
                   importTracker = std::move(importTracker)]() {
            self->traceBus_->publish(
                HgImportTraceEvent::finish(
                    unique,
                    HgImportTraceEvent::BLOB,
                    slOid,
                    context->getPriority().getClass(),
                    context->getCause(),
                    context->getClientPid(),
                    context->getFetchedSource()));
          });
    });

    return std::move(getBlobFuture)
        .thenTry([self, slOid](folly::Try<BlobPtr>&& result) {
          self->queue_.markImportAsFinished<BlobPtr::element_type>(
              slOid, result);
          auto blob = std::move(result).value();
          return GetBlobResult{
              std::move(blob), ObjectFetchContext::Origin::FromNetworkFetch};
        });
  };

  // Enqueue eagerly unless fault injection is enabled. Under
  // kImmediateFutureAlwaysDefer (debug/sanitized builds) even a disabled
  // checkAsync() is not ready, so chaining on it would defer the enqueue until
  // the caller consumes the future.
  if (LIKELY(!faultInjector_.isEnabled())) {
    return enqueue(self, slOid, context, fetch_type);
  }
  return faultInjector_
      .checkAsync("SaplingBackingStore::getBlobEnqueue", slOid.node())
      .thenValue(
          [enqueue, self, slOid = slOid, context = context.copy(), fetch_type](
              auto&&) { return enqueue(self, slOid, context, fetch_type); });
}

folly::coro::now_task<BackingStore::GetBlobResult>
SaplingBackingStore::co_getBlobEnqueue(
    const SlOid& slOid,
    const ObjectFetchContextPtr& context,
    const SaplingImportRequest::FetchType fetch_type) {
  auto self = shared_from_this();
  if (UNLIKELY(self->faultInjector_.isEnabled())) {
    co_await self->faultInjector_.co_checkAsync(
        "SaplingBackingStore::co_getBlobEnqueue", slOid.node());
  }
  XLOGF(DBG4, "making blob import request for {}", slOid);
  auto requestContext = context.copy();
  auto request =
      SaplingImportRequest::makeBlobImportRequest(slOid, requestContext);
  request->setFetchType(fetch_type);
  auto unique = request->getUnique();
  std::unique_ptr<RequestMetricsScope> importTracker;
  switch (fetch_type) {
    case SaplingImportRequest::FetchType::Fetch:
      importTracker = std::make_unique<RequestMetricsScope>(
          &self->pendingImportBlobWatches_);
      break;
    case SaplingImportRequest::FetchType::Prefetch:
      importTracker = std::make_unique<RequestMetricsScope>(
          &self->pendingImportPrefetchWatches_);
      break;
  }

  self->traceBus_->publish(
      HgImportTraceEvent::queue(
          unique,
          HgImportTraceEvent::BLOB,
          slOid,
          context->getPriority().getClass(),
          context->getCause(),
          context->getClientPid()));
  // Setup guard to publish 'finish' event when current scope is destroyed
  auto guard = folly::makeGuard([&unique, &slOid, &context, self] {
    self->traceBus_->publish(
        HgImportTraceEvent::finish(
            unique,
            HgImportTraceEvent::BLOB,
            slOid,
            context->getPriority().getClass(),
            context->getCause(),
            context->getClientPid(),
            context->getFetchedSource()));
  });

  folly::Try<BlobPtr> result;
  try {
    auto blob = co_await self->queue_.co_enqueueBlob(std::move(request));
    result = folly::Try<BlobPtr>{blob};

    self->queue_.markImportAsFinished<BlobPtr::element_type>(slOid, result);

    co_return BackingStore::GetBlobResult{
        std::move(blob), ObjectFetchContext::Origin::FromNetworkFetch};
  } catch (const std::exception&) {
    result.emplaceException(std::current_exception());
    self->queue_.markImportAsFinished<BlobPtr::element_type>(slOid, result);
    throw;
  }
}

folly::coro::now_task<BackingStore::GetBlobAuxResult>
SaplingBackingStore::co_getBlobAuxData(
    const ObjectId& id,
    const ObjectFetchContextPtr& context) {
  DurationScope<EdenStats> scope{
      stats_, &SaplingBackingStoreStats::getBlobAuxData};

  SlOidView slOid{id};

  logBackingStoreFetch(
      *context,
      folly::Range{&slOid, 1},
      ObjectFetchContext::ObjectType::BlobAuxData);

  auto auxData = getLocalBlobAuxData(slOid);
  if (auxData.hasValue() && auxData.value()) {
    stats_->increment(&SaplingBackingStoreStats::fetchBlobAuxDataSuccess);
    stats_->increment(&SaplingBackingStoreStats::fetchBlobAuxDataLocal);
    co_return GetBlobAuxResult{
        std::move(auxData.value()), ObjectFetchContext::Origin::FromDiskCache};
  }

  co_return co_await co_getBlobAuxDataEnqueue(slOid, context);
}

folly::coro::now_task<BackingStore::GetBlobAuxResult>
SaplingBackingStore::co_getBlobAuxDataEnqueue(
    const SlOid& slOid,
    const ObjectFetchContextPtr& context) {
  if (!config_->getEdenConfig()->fetchHgAuxMetadata.getValue()) {
    co_return BackingStore::GetBlobAuxResult{
        nullptr, ObjectFetchContext::Origin::NotFetched};
  }

  auto self = shared_from_this();
  XLOGF(DBG4, "making blob meta import request for {}", slOid);
  auto requestContext = context.copy();
  auto request =
      SaplingImportRequest::makeBlobAuxImportRequest(slOid, requestContext);
  auto unique = request->getUnique();

  auto importTracker = std::make_unique<RequestMetricsScope>(
      &self->pendingImportBlobAuxWatches_);

  self->traceBus_->publish(
      HgImportTraceEvent::queue(
          unique,
          HgImportTraceEvent::BLOB_AUX,
          slOid,
          context->getPriority().getClass(),
          context->getCause(),
          context->getClientPid()));

  auto guard = folly::makeGuard([&unique, &slOid, &context, self] {
    self->traceBus_->publish(
        HgImportTraceEvent::finish(
            unique,
            HgImportTraceEvent::BLOB_AUX,
            slOid,
            context->getPriority().getClass(),
            context->getCause(),
            context->getClientPid(),
            context->getFetchedSource()));
  });

  auto result = co_await folly::coro::co_awaitTry(
      self->queue_.co_enqueueBlobAux(std::move(request)));

  self->queue_.markImportAsFinished<BlobAuxDataPtr::element_type>(
      slOid, result);

  if (result.hasException()) {
    co_yield folly::coro::co_error(std::move(result).exception());
  }

  co_return BackingStore::GetBlobAuxResult{
      std::move(result).value(), ObjectFetchContext::Origin::FromNetworkFetch};
}

folly::Try<BlobAuxDataPtr> SaplingBackingStore::getLocalBlobAuxData(
    SlOidView slOid) {
  auto node = slOid.node();

  using GetBlobAuxDataResult = folly::Try<BlobAuxDataPtr>;

  XLOGF(DBG7, "Importing blob aux data node={} from hgcache", node);
  auto result = sapling_backingstore_get_file_aux(
      *store_.get(),
      rust::Slice<const uint8_t>{node.getBytes().data(), node.RAW_SIZE},
      sapling::FetchMode::LocalOnly);

  if (result.error != nullptr) {
    return GetBlobAuxDataResult{std::move(*result.error)};
  }

  if (result.data != nullptr) {
    return GetBlobAuxDataResult{
        std::make_shared<BlobAuxDataPtr::element_type>(BlobAuxData{
            Hash20{std::move(result.data->content_sha1)},
            Hash32{std::move(result.data->content_blake3)},
            result.data->total_size})};
  } else {
    return GetBlobAuxDataResult{nullptr};
  }
}

folly::coro::now_task<BackingStore::GetRootTreeResult>
SaplingBackingStore::co_getRootTree(
    const RootId& rootId,
    const ObjectFetchContextPtr& context) {
  // increase ref count to keep alive during eden shutdown and changing executor
  auto self = shared_from_this();
  folly::stop_watch<std::chrono::milliseconds> watch;
  ObjectId commitId = hashFromRootId(rootId);

  co_await faultInjector_.co_checkAsync(
      "SaplingBackingStore::getRootTree", commitId.asHexString());

  auto result = co_await co_withExecutor(
      serverThreadPool_,
      folly::coro::co_invoke(
          [self, commitId, context = context.copy(), watch]()
              -> folly::coro::Task<GetRootTreeResult> {
            auto manifestNode = self->getManifestNode(commitId);

            if (!manifestNode.has_value()) {
              throw std::runtime_error(
                  "Manifest node could not be found for commitId");
            }

            XLOGF(
                DBG3,
                "commit {} has manifest node {}",
                commitId,
                manifestNode.value());

            auto rootTree = self->importTreeManifestSync(
                                    *std::move(manifestNode),
                                    context,
                                    ObjectFetchContext::ObjectType::RootTree)
                                .value();

            self->stats_->addDuration(
                &SaplingBackingStoreStats::getRootTree, watch.elapsed());

            co_return GetRootTreeResult{rootTree, rootTree->getObjectId()};
          }));

  co_return std::move(result);
}

std::optional<Hash20> SaplingBackingStore::getManifestNode(
    const ObjectId& commitId) {
  auto node = commitId.getBytes();

  XLOGF(
      DBG7,
      "Importing manifest node={} from backingstore",
      folly::hexlify(node));
  try {
    std::array<uint8_t, 20> manifestId = sapling_backingstore_get_manifest(
        *store_.get(), rust::Slice<const uint8_t>{node.data(), node.size()});
    return Hash20(std::move(manifestId));
  } catch (const rust::Error& error) {
    XLOGF(
        DBG2,
        "Error while getting manifest node={} from backingstore: {}",
        folly::hexlify(node),
        error.what());
    errorLogger_.log(
        EdenErrorInfo::backingStore(error)
            .withRepoName(repoName_)
            .withIsDogfoodingHost(dogfoodingHost())
            .withErrorType("manifest_resolution_failure"));
    return std::nullopt;
  }
}

folly::Try<TreePtr> SaplingBackingStore::importTreeManifestSync(
    Hash20 manifestNode,
    const ObjectFetchContextPtr& context,
    const ObjectFetchContext::ObjectType type) {
  // Record that we are at the root for this node
  RelativePathPiece path{};
  auto hgObjectIdFormat = config_->getEdenConfig()->hgObjectIdFormat.getValue();

  SlOid slOid;

  switch (hgObjectIdFormat) {
    case HgObjectIdFormat::WithPath:
      slOid = SlOid{manifestNode, path};
      break;

    case HgObjectIdFormat::HashOnly:
      slOid = SlOid{manifestNode};
      break;
  }

  auto tree =
      getTreeFromBackingStore(path.copy(), slOid.view(), context.copy(), type);
  bool success = tree.hasValue();

  // record stats
  switch (type) {
    case ObjectFetchContext::ObjectType::Tree:
      // getTree never gets here. We add this case only for completeness
      stats_->increment(
          success ? &SaplingBackingStoreStats::fetchTreeSuccess
                  : &SaplingBackingStoreStats::fetchTreeFailure);
      break;
    case ObjectFetchContext::ObjectType::RootTree:
      stats_->increment(
          success ? &SaplingBackingStoreStats::getRootTreeSuccess
                  : &SaplingBackingStoreStats::getRootTreeFailure);
      break;
    case ObjectFetchContext::ObjectType::ManifestForRoot:
      stats_->increment(
          success ? &SaplingBackingStoreStats::importManifestForRootSuccess
                  : &SaplingBackingStoreStats::importManifestForRootFailure);
      break;
      // The following types cannot get here. It is just for completeness
    case ObjectFetchContext::TreeAuxData:
    case ObjectFetchContext::Blob:
    case ObjectFetchContext::BlobAuxData:
    case ObjectFetchContext::ObjectType::PrefetchBlob:
    case ObjectFetchContext::kObjectTypeEnumMax:
      break;
  }
  if (dogfoodingHost()) {
    stats_->increment(
        success ? &SaplingBackingStoreStats::fetchTreeSuccessDogfooding
                : &SaplingBackingStoreStats::fetchTreeFailureDogfooding);
  }

  if (tree.hasValue()) {
    XLOGF(DBG4, "imported tree {}", slOid);
  }
  return tree;
}

folly::Future<TreePtr> SaplingBackingStore::importTreeManifestImpl(
    Hash20 manifestNode,
    const ObjectFetchContextPtr& context,
    const ObjectFetchContext::ObjectType type) {
  auto tree = importTreeManifestSync(manifestNode, context, type);
  return folly::makeFuture(std::move(tree));
}

folly::coro::now_task<TreePtr> SaplingBackingStore::co_importTreeManifestImpl(
    Hash20 manifestNode,
    const ObjectFetchContextPtr& context,
    const ObjectFetchContext::ObjectType type) {
  auto tree = importTreeManifestSync(manifestNode, context, type);
  co_return std::move(tree).value();
}

folly::Try<TreePtr> SaplingBackingStore::getTreeFromBackingStore(
    const RelativePath& path,
    SlOidView slOid,
    ObjectFetchContextPtr context,
    const ObjectFetchContext::ObjectType type) {
  using GetTreeResult = folly::Try<TreePtr>;

  sapling::FetchMode fetch_mode = sapling::FetchMode::AllowRemote;
  // For root trees we will try getting the tree locally first.  This allows
  // us to catch when Mercurial might have just written a tree to the store,
  // and refresh the store so that the store can pick it up.  We don't do
  // this for all trees, as it would cause a lot of additional work on every
  // cache miss, and just doing it for root trees is sufficient to detect the
  // scenario where Mercurial just wrote a brand new tree.
  if (path.empty()) {
    fetch_mode = sapling::FetchMode::LocalOnly;
  }
  GetTreeResult tree = getNativeTree(slOid, context, fetch_mode);
  if (tree.hasValue() && !tree.value() &&
      fetch_mode == sapling::FetchMode::LocalOnly) {
    // Mercurial might have just written the tree to the store. Refresh the
    // store and try again, this time allowing remote fetches.
    flush();
    fetch_mode = sapling::FetchMode::AllowRemote;
    tree = getNativeTree(slOid, context, fetch_mode);
  }

  if (tree.hasValue()) {
    if (!tree.value()) {
      tree = GetTreeResult{std::runtime_error{
          fmt::format("no tree found for {} (path={})", slOid.node(), path)}};
    }

    switch (fetch_mode) {
      case sapling::FetchMode::LocalOnly:
        context->setFetchedSource(
            ObjectFetchContext::FetchedSource::Local, type, stats_.copy());
        break;
      case sapling::FetchMode::RemoteOnly:
        context->setFetchedSource(
            ObjectFetchContext::FetchedSource::Remote, type, stats_.copy());
        break;
      case sapling::FetchMode::AllowRemote:
        context->setFetchedSource(
            ObjectFetchContext::FetchedSource::Unknown, type, stats_.copy());
        break;
    }
  }

  return tree;
}

folly::SemiFuture<folly::Unit> SaplingBackingStore::prefetchBlobs(
    ObjectIdRange ids,
    const ObjectFetchContextPtr& context) {
  // NOLINTNEXTLINE(facebook-folly-coro-return-captures-local-var)
  return folly::coro::co_invoke(
             [self = shared_from_this()](
                 auto ids, auto context) -> folly::coro::Task<folly::Unit> {
               co_return co_await self->co_prefetchBlobs(
                   std::move(ids), std::move(context));
             },
             ids,
             context.copy())
      .semi();
}

folly::coro::now_task<folly::Unit> SaplingBackingStore::co_prefetchBlobs(
    ObjectIdRange ids,
    const ObjectFetchContextPtr& context) {
  std::vector<SlOidView> slOids;
  slOids.reserve(ids.size());
  for (const auto& id : ids) {
    slOids.emplace_back(id);
  }

  if (slOids.empty()) {
    co_return folly::unit;
  }

  logBackingStoreFetch(
      *context,
      folly::Range{slOids.data(), slOids.size()},
      ObjectFetchContext::ObjectType::Blob);

  std::vector<sapling::SaplingRequest> requests;
  requests.reserve(ids.size());
  for (size_t i = 0; i < ids.size(); i++) {
    requests.emplace_back(slOids[i], context->getCause(), context.copy());
  }

  co_await folly::coro::co_reschedule_on_current_executor;

  auto importTracker = RequestMetricsScope{&pendingImportPrefetchWatches_};

  auto unique = generateUniqueID();
  traceBus_->publish(
      HgImportTraceEvent::start(
          unique,
          HgImportTraceEvent::BLOB_BATCH,
          slOids[0],
          context->getPriority().getClass(),
          context->getCause(),
          context->getClientPid()));

  folly::stop_watch<std::chrono::milliseconds> watch;
  XLOGF(DBG4, "Batch fetching {} blobs from Sapling", requests.size());

  size_t failureCount = 0;
  auto fetchStats = nativeGetBlobBatch(
      folly::range(requests),
      sapling::FetchMode::AllowRemote,
      true, // IGNORE_RESULT
      [&](size_t index, folly::Try<std::unique_ptr<folly::IOBuf>> content) {
        if (content.hasException()) {
          failureCount++;
          XLOGF(
              ERR,
              "Failed to batch import {} from Sapling: {}",
              requests[index].oid,
              content.exception().what().toStdString());
        }
      });
  reportPrefetchStats(context, fetchStats, /*totalBytes=*/0, failureCount);

  traceBus_->publish(
      HgImportTraceEvent::finish(
          unique,
          HgImportTraceEvent::BLOB_BATCH,
          slOids[0],
          context->getPriority().getClass(),
          context->getCause(),
          context->getClientPid(),
          context->getFetchedSource()));

  stats_->increment(
      &SaplingBackingStoreStats::prefetchBlobFailure, failureCount);
  stats_->increment(
      &SaplingBackingStoreStats::prefetchBlobSuccess,
      requests.size() - failureCount);
  stats_->addDuration(&SaplingBackingStoreStats::prefetchBlob, watch.elapsed());
  co_return folly::unit;
}

sapling::BatchFetchStats SaplingBackingStore::nativeGetBlobBatch(
    folly::Range<const sapling::SaplingRequest*> requests,
    sapling::FetchMode fetch_mode,
    bool allow_ignore_result,
    folly::FunctionRef<void(size_t, folly::Try<std::unique_ptr<folly::IOBuf>>)>
        resolve) {
  auto count = requests.size();
  if (count == 0) {
    return sapling::BatchFetchStats{};
  }

  auto resolver =
      std::make_shared<sapling::GetBlobBatchResolver>(std::move(resolve));

  XLOGF(
      DBG7,
      "Import blobs with size: {}, first oid: {}",
      count,
      requests[0].oid);

  std::vector<sapling::Request> raw_requests;
  raw_requests.reserve(count);
  for (auto& request : requests) {
    raw_requests.push_back(
        sapling::Request{
            request.oid.rustData(),
            request.cause,
        });
  }

  return sapling_backingstore_get_blob_batch(
      *store_.get(),
      rust::Slice<const sapling::Request>{
          raw_requests.data(), raw_requests.size()},
      fetch_mode,
      allow_ignore_result,
      std::move(resolver));
}

ImmediateFuture<bool> SaplingBackingStore::checkPermission(
    const ObjectId& manifestId) {
  auto hgId = SlOidView{manifestId}.node().getBytes();
  auto result = sapling_backingstore_check_permission(
      *store_.get(), rust::Slice<const uint8_t>{hgId.data(), hgId.size()});

  if (result.error != nullptr) {
    return makeImmediateFuture<bool>(std::runtime_error(result.error->what()));
  }
  return result.has_access;
}

folly::coro::now_task<std::vector<folly::Try<std::vector<EntryAcl>>>>
SaplingBackingStore::co_getPathAcls(
    const RootId& rootId,
    const std::vector<std::string>& paths,
    const ObjectFetchContextPtr& context) {
  auto self = shared_from_this();
  // TODO(T272514471): Plumb ObjectFetchContext through the Sapling FFI so path
  // ACL lookups are attributed in fetch traces like other backing-store calls.
  (void)context;
  co_await self->faultInjector_.co_checkAsync(
      "SaplingBackingStore::getPathAcls", rootId.value());
  co_return co_await folly::coro::co_withExecutor(
      self->serverThreadPool_,
      folly::coro::co_invoke(
          [self, rootId = rootId, paths = std::vector<std::string>(paths)]()
              -> folly::coro::Task<
                  std::vector<folly::Try<std::vector<EntryAcl>>>> {
            rust::Vec<rust::String> rustPaths;
            rustPaths.reserve(paths.size());
            std::copy(
                paths.begin(), paths.end(), std::back_inserter(rustPaths));

            auto result = sapling_backingstore_check_path_permissions(
                *self->store_.get(),
                rust::Str{rootId.value().data(), rootId.value().size()},
                std::move(rustPaths));
            if (result.error != nullptr) {
              throw std::runtime_error(result.error->what());
            }

            std::vector<folly::Try<std::vector<EntryAcl>>> aclInfos;
            aclInfos.reserve(paths.size());
            if (result.data.size() != paths.size()) {
              throw std::runtime_error(
                  fmt::format(
                      "path ACL lookup returned {} result(s) for {} path(s)",
                      result.data.size(),
                      paths.size()));
            }

            for (size_t index = 0; index < paths.size(); ++index) {
              const auto& pathAclInfo = result.data[index];
              std::string responsePath{pathAclInfo.path};
              if (responsePath != paths[index]) {
                throw std::runtime_error(
                    fmt::format(
                        "path ACL lookup returned response for {} at index {} instead of {}",
                        responsePath,
                        index,
                        paths[index]));
              }

              std::string error{pathAclInfo.error};
              if (!error.empty()) {
                aclInfos.emplace_back(std::runtime_error(error));
                continue;
              }

              std::vector<EntryAcl> entries;
              entries.reserve(pathAclInfo.entries.size());
              for (const auto& entry : pathAclInfo.entries) {
                entries.push_back(
                    EntryAcl{
                        std::string{entry.restriction_root},
                        std::string{entry.repo_region_acl},
                        std::string{entry.permission_request_group}});
              }
              aclInfos.emplace_back(std::move(entries));
            }
            co_return aclInfos;
          }));
}

void SaplingBackingStore::logBackingStoreFetch(
    const ObjectFetchContext& context,
    folly::Range<SlOidView*> slOids,

    ObjectFetchContext::ObjectType type) {
  // Copy the value out of the snapshot: a reference into it would dangle as
  // soon as the temporary snapshot is destroyed, and a config reload frees
  // the RE2 it points at.
  const auto logFetchPathRegex =
      config_->getEdenConfig()->logObjectFetchPathRegex.getValue();

  if (logFetchPathRegex) {
    for (auto& slOid : slOids) {
      auto path = slOid.path();

      if (RE2::PartialMatch(
              re2::StringPiece{path.view().data(), path.view().size()},
              **logFetchPathRegex)) {
        logger_->logImport(context, path, type);
      }
    }
  }

  if (type != ObjectFetchContext::ObjectType::Tree &&
      isRecordingFetch_.load(std::memory_order_relaxed) &&
      ObjectFetchContext::shouldTriggerWalkDetection(context.getCause())) {
    auto guard = fetchedFilePaths_.wlock();
    for (auto& slOid : slOids) {
      guard->emplace(slOid.path().view());
    }
  }
}

size_t SaplingBackingStore::getImportMetric(
    RequestMetricsScope::RequestStage stage,
    SaplingImportObject object,
    RequestMetricsScope::RequestMetric metric) const {
  return RequestMetricsScope::getMetricFromWatches(
      metric, getImportWatches(stage, object));
}

RequestMetricsScope::LockedRequestWatchList&
SaplingBackingStore::getImportWatches(
    RequestMetricsScope::RequestStage stage,
    SaplingImportObject object) const {
  switch (stage) {
    case RequestMetricsScope::RequestStage::PENDING:
      return getPendingImportWatches(object);
    case RequestMetricsScope::RequestStage::LIVE:
      return getLiveImportWatches(object);
  }
  EDEN_BUG() << "unknown sapling import stage " << enumValue(stage);
}

RequestMetricsScope::LockedRequestWatchList&
SaplingBackingStore::getPendingImportWatches(SaplingImportObject object) const {
  switch (object) {
    case SaplingImportObject::BLOB:
    case SaplingImportObject::BATCHED_BLOB:
      return pendingImportBlobWatches_;
    case SaplingImportObject::TREE:
    case SaplingImportObject::BATCHED_TREE:
      return pendingImportTreeWatches_;
    case SaplingImportObject::BLOB_AUX:
    case SaplingImportObject::BATCHED_BLOB_AUX:
      return pendingImportBlobAuxWatches_;
    case SaplingImportObject::TREE_AUX:
    case SaplingImportObject::BATCHED_TREE_AUX:
      return pendingImportTreeAuxWatches_;
    case SaplingImportObject::PREFETCH:
      return pendingImportPrefetchWatches_;
  }
  EDEN_BUG() << "unknown sapling import object type "
             << static_cast<int>(object);
}

RequestMetricsScope::LockedRequestWatchList&
SaplingBackingStore::getLiveImportWatches(SaplingImportObject object) const {
  switch (object) {
    case SaplingImportObject::BLOB:
      return liveImportBlobWatches_;
    case SaplingImportObject::TREE:
      return liveImportTreeWatches_;
    case SaplingImportObject::BLOB_AUX:
      return liveImportBlobAuxWatches_;
    case SaplingImportObject::TREE_AUX:
      return liveImportTreeAuxWatches_;
    case SaplingImportObject::PREFETCH:
      return liveImportPrefetchWatches_;
    case SaplingImportObject::BATCHED_BLOB:
      return liveBatchedBlobWatches_;
    case SaplingImportObject::BATCHED_TREE:
      return liveBatchedTreeWatches_;
    case SaplingImportObject::BATCHED_BLOB_AUX:
      return liveBatchedBlobAuxWatches_;
    case SaplingImportObject::BATCHED_TREE_AUX:
      return liveBatchedTreeAuxWatches_;
  }
  EDEN_BUG() << "unknown sapling import object " << enumValue(object);
}

folly::StringPiece SaplingBackingStore::stringOfSaplingImportObject(
    SaplingImportObject object) {
  switch (object) {
    case SaplingImportObject::BLOB:
      return "blob";
    case SaplingImportObject::TREE:
      return "tree";
    case SaplingImportObject::BLOB_AUX:
      return "blobmeta";
    case SaplingImportObject::TREE_AUX:
      return "treemeta";
    case SaplingImportObject::BATCHED_BLOB:
      return "batched_blob";
    case SaplingImportObject::BATCHED_TREE:
      return "batched_tree";
    case SaplingImportObject::BATCHED_BLOB_AUX:
      return "batched_blobmeta";
    case SaplingImportObject::BATCHED_TREE_AUX:
      return "batched_treemeta";
    case SaplingImportObject::PREFETCH:
      return "prefetch";
  }
  EDEN_BUG() << "unknown sapling import object " << enumValue(object);
}

void SaplingBackingStore::startRecordingFetch() {
  isRecordingFetch_.store(true, std::memory_order_relaxed);
}

std::unordered_set<std::string> SaplingBackingStore::stopRecordingFetch() {
  isRecordingFetch_.store(false, std::memory_order_relaxed);
  std::unordered_set<std::string> paths;
  std::swap(paths, *fetchedFilePaths_.wlock());
  return paths;
}

ImmediateFuture<folly::Unit> SaplingBackingStore::importManifestForRoot(
    const RootId& rootId,
    const Hash20& manifestId,
    const ObjectFetchContextPtr& context) {
  // This method is used when the client informs us about a target manifest
  // that it is about to update to, for the scenario when a manifest has
  // just been created.  Since the manifest has just been created locally, and
  // aux data is only available remotely, there will be no aux data available
  // to prefetch.
  //
  // When the local store is populated with aux data for newly-created
  // manifests then we can update this so that is true when appropriate.
  /**
   * Import the root manifest for the specified revision using mercurial
   * treemanifest data.  This is called when the root manifest is provided
   * to EdenFS directly by the hg client.
   */
  folly::stop_watch<std::chrono::milliseconds> watch;
  auto commitId = hashFromRootId(rootId);
  return importTreeManifestImpl(
             manifestId,
             context,
             ObjectFetchContext::ObjectType::ManifestForRoot)
      .thenValue([self = shared_from_this(), commitId, manifestId, watch](
                     TreePtr rootTree) {
        XLOGF(
            DBG3,
            "imported mercurial commit {} with manifest {} as tree {}",
            commitId,
            manifestId,
            rootTree->getObjectId());
        self->stats_->addDuration(
            &SaplingBackingStoreStats::importManifestForRoot, watch.elapsed());
      });
}

folly::coro::now_task<folly::Unit>
SaplingBackingStore::co_importManifestForRoot(
    const RootId& rootId,
    const Hash20& manifestId,
    const ObjectFetchContextPtr& context) {
  auto self = shared_from_this();
  // See importManifestForRoot for rationale.
  folly::stop_watch<std::chrono::milliseconds> watch;
  auto commitId = hashFromRootId(rootId);
  TreePtr rootTree = co_await co_importTreeManifestImpl(
      manifestId, context, ObjectFetchContext::ObjectType::ManifestForRoot);
  XLOGF(
      DBG3,
      "imported mercurial commit {} with manifest {} as tree {}",
      commitId,
      manifestId,
      rootTree->getObjectId());
  self->stats_->addDuration(
      &SaplingBackingStoreStats::importManifestForRoot, watch.elapsed());
  co_return folly::unit;
}

void SaplingBackingStore::periodicManagementTask() {
  flush();
}

namespace {
template <typename ResponseType>
void dropImportRequest(std::shared_ptr<SaplingImportRequest>& request) {
  auto* promise = request->getPromise<ResponseType>();
  if (promise != nullptr) {
    if (!promise->isFulfilled()) {
      promise->setException(std::runtime_error("Request forcibly dropped"));
    }
  }
}
} // namespace

int64_t SaplingBackingStore::dropAllPendingRequestsFromQueue() {
  auto requestVec = queue_.combineAndClearRequestQueues();
  for (auto& request : requestVec) {
    if (request->isType<SaplingImportRequest::BlobImport>()) {
      XLOG(DBG7, "Dropping blob request");
      dropImportRequest<BlobPtr>(request);
    } else if (request->isType<SaplingImportRequest::TreeImport>()) {
      XLOG(DBG7, "Dropping tree request");
      dropImportRequest<TreePtr>(request);
    } else if (request->isType<SaplingImportRequest::BlobAuxImport>()) {
      XLOG(DBG7, "Dropping blob aux data request");
      dropImportRequest<BlobAuxDataPtr>(request);
    } else if (request->isType<SaplingImportRequest::TreeAuxImport>()) {
      XLOG(DBG7, "Dropping tree aux data request");
      dropImportRequest<TreeAuxDataPtr>(request);
    }
  }
  return requestVec.size();
}

ObjectId SaplingBackingStore::stripObjectId(const ObjectId& id) const {
  if (id.getBytes().size() < 21) {
    // Empty or proxy hash - don't do anything.
    return id;
  }
  // Strip the path from the object id.
  // TODO(muirdm): replace with SlOid once I land D87098195.
  return ObjectId{id.getBytes().subpiece(0, 21)};
}

void SaplingBackingStore::flush() {
  XLOG(DBG7, "Flushing backing store");
  sapling_backingstore_flush(*store_.get());
}

folly::Expected<sapling::HgCacheStats, std::string>
SaplingBackingStore::getCacheStats() const {
  auto result = sapling::sapling_backingstore_get_cache_stats(*store_.get());
  if (result.error != nullptr) {
    return folly::makeUnexpected(std::string(result.error->what()));
  }
  if (!result.data) {
    // The FFI contract always pairs a null error with non-null data; this
    // is defensive against that contract changing underneath us.
    return folly::makeUnexpected(
        std::string("cache stats lookup returned no data and no error"));
  }
  return *result.data;
}

bool SaplingBackingStore::dogfoodingHost() {
  return sapling_dogfooding_host(*store_.get());
}

void SaplingBackingStore::logFetchMiss(
    const folly::exception_wrapper& ex,
    folly::StringPiece fetchType) {
  ex.with_exception([&](const std::exception& e) {
    errorLogger_.log(
        EdenErrorInfo::backingStore(ErrorArg::fromExceptionWithoutTrace(e))
            .withRepoName(repoName_)
            .withFetchType(fetchType.str())
            .withIsDogfoodingHost(dogfoodingHost())
            .withErrorType("fetch_miss"));
  });
}

} // namespace facebook::eden
