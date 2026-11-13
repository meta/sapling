/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/store/ObjectStore.h"

#include <folly/Conv.h>
#include <folly/coro/Collect.h>
#include <folly/coro/Invoke.h>
#include <folly/coro/Task.h>
#include <folly/futures/Future.h>

#include <stdexcept>

#include "eden/common/utils/Bug.h"
#include "eden/common/utils/ImmediateFuture.h"
#include "eden/common/utils/ProcessInfoCache.h"
#include "eden/common/utils/Throw.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/config/ReloadableConfig.h"
#include "eden/fs/model/Blob.h"
#include "eden/fs/model/Tree.h"
#include "eden/fs/model/TreeAuxData.h"
#include "eden/fs/store/BackingStore.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/store/TreeCache.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/telemetry/LogEvent.h"
#include "eden/fs/telemetry/TaskTrace.h"

using folly::Future;
using folly::makeFuture;
using std::shared_ptr;
using std::string;
using std::unique_ptr;

namespace facebook::eden {

namespace {
constexpr uint64_t kImportPriorityDeprioritizeAmount = 1;
} // namespace

std::shared_ptr<ObjectStore> ObjectStore::create(
    shared_ptr<BackingStore> backingStore,
    shared_ptr<TreeCache> treeCache,
    EdenStatsPtr stats,
    std::shared_ptr<ProcessInfoCache> processInfoCache,
    std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger,
    std::shared_ptr<ReloadableConfig> edenConfig,
    CaseSensitivity caseSensitive,
    folly::Executor::KeepAlive<> executor,
    std::shared_ptr<WalkDetector> walkDetector) {
  return std::shared_ptr<ObjectStore>{new ObjectStore{
      std::move(backingStore),
      std::move(treeCache),
      std::move(stats),
      processInfoCache,
      edenFsEventsLogger,
      edenConfig,
      caseSensitive,
      std::move(executor),
      std::move(walkDetector)}};
}

ObjectStore::ObjectStore(
    shared_ptr<BackingStore> backingStore,
    shared_ptr<TreeCache> treeCache,
    EdenStatsPtr stats,
    std::shared_ptr<ProcessInfoCache> processInfoCache,
    std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger,
    std::shared_ptr<ReloadableConfig> edenConfig,
    CaseSensitivity caseSensitive,
    folly::Executor::KeepAlive<> executor,
    std::shared_ptr<WalkDetector> walkDetector)
    : blobAuxDataCache_{
          edenConfig->getEdenConfig()->metadataCacheShards.getValue(),
          edenConfig->getEdenConfig()->metadataCacheSize.getValue()},
      treeAuxDataCache_{
          edenConfig->getEdenConfig()->metadataCacheShards.getValue(),
          edenConfig->getEdenConfig()->metadataCacheSize.getValue()},
      treeCache_{std::move(treeCache)},
      backingStore_{std::move(backingStore)},
      executor_{std::move(executor)},
      walkDetector_{std::move(walkDetector)},
      stats_{std::move(stats)},
      pidFetchCounts_{std::make_unique<PidFetchCounts>()},
      processInfoCache_(processInfoCache),
      edenFsEventsLogger_(edenFsEventsLogger),
      edenConfig_(edenConfig),
      caseSensitive_{caseSensitive},
      restrictedContentMode_{
          edenConfig->getEdenConfig()->restrictedContentMode.getValue()} {
  XCHECK(backingStore_);
  XCHECK(stats_);
}

ObjectStore::~ObjectStore() = default;

void ObjectStore::witnessBlobFetch(
    const ObjectId& id,
    const BackingStore::GetBlobResult& result,
    const ObjectFetchContext& context) const {
  if (!walkDetector_ ||
      !ObjectFetchContext::shouldTriggerWalkDetection(context.getCause())) {
    return;
  }
  // Walk detection is best effort and must not fail the read it observed.
  try {
    walkDetector_->witnessFileRead(
        id,
        result.origin,
        context.getClientPid(),
        context.getParentDirEntryCounts());
  } catch (const std::exception& ex) {
    XLOGF_EVERY_MS(
        WARN, 60000, "walk detector failed on blob {}: {}", id, ex.what());
  }
}

void ObjectStore::witnessTreeFetch(
    const ObjectId& id,
    const Tree& tree,
    ObjectFetchContext::Origin origin,
    const ObjectFetchContext& context) const {
  // A restricted placeholder has no entries and was not a directory read.
  if (!walkDetector_ || tree.isRestricted() ||
      !ObjectFetchContext::shouldTriggerWalkDetection(context.getCause())) {
    return;
  }
  // Walk detection is best effort and must not fail the read it observed.
  try {
    walkDetector_->witnessTreeRead(id, tree, origin, context.getClientPid());
  } catch (const std::exception& ex) {
    XLOGF_EVERY_MS(
        WARN, 60000, "walk detector failed on tree {}: {}", id, ex.what());
  }
}

void ObjectStore::updateProcessFetch(
    const ObjectFetchContext& fetchContext) const {
  if (auto pid = fetchContext.getClientPid()) {
    auto fetch_count = pidFetchCounts_->recordProcessFetch(pid.value());
    auto threshold =
        edenConfig_->getEdenConfig()->fetchHeavyThreshold.getValue();
    // indicate heavy event when fetch_count reaches multiple of threshold
    if (fetch_count && threshold && (fetch_count % threshold) == 0) {
      sendFetchHeavyEvent(pid.value(), fetch_count);
    }
  }
}

void ObjectStore::sendFetchHeavyEvent(ProcessId pid, uint64_t fetch_count)
    const {
  if (!processInfoCache_ || !edenFsEventsLogger_) {
    return;
  }
  auto processInfo = processInfoCache_->getProcessInfo(pid.get());
  if (processInfo) {
    auto& processName = processInfo->name;
    std::replace(processName.begin(), processName.end(), '\0', ' ');
    XLOGF(
        WARN,
        "Heavy fetches ({}) from process {}(pid={})",
        fetch_count,
        processName,
        pid);
    auto repoName = backingStore_->getRepoName();
    std::optional<uint64_t> loadedInodes = [repoName]() {
      auto counterValue = fb303::ServiceData::get()->getCounterIfExists(
          fmt::format("inodemap.{}.loaded", repoName.value_or("")));
      return counterValue.has_value()
          ? std::optional<uint64_t>(static_cast<uint64_t>(counterValue.value()))
          : std::nullopt;
    }();

    edenFsEventsLogger_->logEvent(
        FetchHeavy{
            std::move(processName),
            pid,
            fetch_count,
            loadedInodes,
            std::move(processInfo->attribution)});
  } else {
    XLOGF(WARN, "Heavy fetches ({}) from pid {})", fetch_count, pid);
  }
}

void ObjectStore::deprioritizeWhenFetchHeavy(
    ObjectFetchContext& context) const {
  if (auto pid = context.getClientPid()) {
    auto fetch_count = pidFetchCounts_->getCountByPid(pid.value());
    auto threshold =
        edenConfig_->getEdenConfig()->fetchHeavyThreshold.getValue();
    if (threshold && fetch_count >= threshold) {
      context.deprioritize(kImportPriorityDeprioritizeAmount);
    }
  }
}

RootId ObjectStore::parseRootId(folly::StringPiece rootId) {
  return backingStore_->parseRootId(rootId);
}

std::string ObjectStore::renderRootId(const RootId& rootId) {
  return backingStore_->renderRootId(rootId);
}

std::string ObjectStore::displayRootId(const RootId& rootId) {
  return backingStore_->displayRootId(rootId);
}

ObjectId ObjectStore::parseObjectId(folly::StringPiece objectId) {
  return backingStore_->parseObjectId(objectId);
}

std::string ObjectStore::renderObjectId(const ObjectId& objectId) {
  return backingStore_->renderObjectId(objectId);
}

namespace {
/**
 * The passed in Tree may differ in case sensitivity from the ObjectStore's
 * case sensitivity. In that case, the Tree is copied and its case sensitivity
 * is switched.
 *
 * In practice, this conversion is extremely rare due to most mounts being
 * created with the default case sensitivity.
 *
 * TODO(xavierd): Is this ugly? Yes, but this will allow incrementally
 * converting the BackingStore+TreeCache to care about case
 * sensitivity separately.
 */
std::shared_ptr<const Tree> changeCaseSensitivity(
    std::shared_ptr<const Tree> tree,
    CaseSensitivity caseSensitive) {
  if (tree->getCaseSensitivity() == caseSensitive) {
    return tree;
  } else {
    auto treeEntries = Tree::container{
        tree->cbegin(), tree->cend(), caseSensitive}; // Explicit copy.
    return tree->withNewId(std::move(treeEntries), tree->getObjectId());
  }
}

} // namespace

template <typename T>
ImmediateFuture<T> ObjectStore::startInline(folly::coro::Task<T> task) const {
  return ImmediateFuture<T>{
      folly::coro::co_withExecutor(executor_, std::move(task))
          .startInlineUnsafe()};
}

ImmediateFuture<ObjectStore::GetRootTreeResult> ObjectStore::getRootTree(
    const RootId& rootId,
    const ObjectFetchContextPtr& context) const {
  return startInline(
      // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
      folly::coro::co_invoke(
          [self = shared_from_this()](auto rootId, auto context)
              -> folly::coro::Task<GetRootTreeResult> {
            co_return co_await self->co_getRootTree(
                std::move(rootId), std::move(context));
          },
          RootId{rootId},
          context.copy()));
}

folly::coro::now_task<ObjectStore::GetRootTreeResult>
ObjectStore::co_getRootTree(
    const RootId& rootId,
    const ObjectFetchContextPtr& context) const {
  XLOGF(DBG3, "getRootTree({})", rootId);
  DurationScope<EdenStats> statScope{stats_, &ObjectStoreStats::getRootTree};

  try {
    auto result = co_await backingStore_->co_getRootTree(rootId, context);
    stats_->increment(&ObjectStoreStats::getRootTreeFromBackingStore);
    auto tree = changeCaseSensitivity(std::move(result.tree), caseSensitive_);
    treeCache_->insert(result.treeId, tree);
    co_return GetRootTreeResult{std::move(tree), result.treeId};
  } catch (...) {
    stats_->increment(&ObjectStoreStats::getRootTreeFailed);
    XLOGF(DBG4, "unable to find root tree {}", rootId.value());
    throw;
  }
}

ImmediateFuture<std::shared_ptr<TreeEntry>>
ObjectStore::getTreeEntryForObjectId(
    const ObjectId& objectId,
    TreeEntryType treeEntryType,
    const ObjectFetchContextPtr& context) const {
  XLOGF(DBG3, "getTreeEntryForRootId({})", objectId);

  return backingStore_
      ->getTreeEntryForObjectId(objectId, treeEntryType, context)
      .thenValue(
          [](std::shared_ptr<TreeEntry> treeEntry) { return treeEntry; });
}

ImmediateFuture<shared_ptr<const Tree>> ObjectStore::getTree(
    const ObjectId& id,
    const ObjectFetchContextPtr& fetchContext) const {
  // DEPRECATED: use co_getTree directly. Kept only because IObjectStore
  // still declares the ImmediateFuture virtual; delete once all callers
  // (checkout, diff, glob, readdir, inode loading) are migrated to coroutines.
  return startInline(
      // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
      folly::coro::co_invoke(
          [self = shared_from_this()](auto&&... args)
              -> folly::coro::Task<std::shared_ptr<const Tree>> {
            co_return co_await self->co_getTree(
                std::forward<decltype(args)>(args)...);
          },
          ObjectId{id},
          fetchContext.copy()));
}

folly::coro::now_task<std::shared_ptr<const Tree>> ObjectStore::co_getTree(
    const ObjectId& id,
    const ObjectFetchContextPtr& fetchContext) const {
  TaskTraceBlock block{"ObjectStore::getTree"};
  DurationScope<EdenStats> statScope{stats_, &ObjectStoreStats::getTree};
  folly::stop_watch<std::chrono::milliseconds> watch;
  // TODO: We should consider checking if we have in flight BackingStore
  // requests on this layer instead of only in the BackingStore. Consider the
  // case in which thread A and thread B both request a Tree at the same time.
  // Let's say thread A checks the LocalStore, then thread B checks the
  // LocalStore, gets the file from the BackingStore (making a request to the
  // server), then writes the Tree to the LocalStore. Now when thread A checks
  // for in flight requests in the BackingStore, it will not see any since
  // thread B has completely finished, so thread A will make a duplicate
  // request. If we were to mark here that we got a request on this layer, then
  // we could avoid that case.
  const bool bypassCaches = bypassInMemoryTreeCaches(*fetchContext);
  if (!bypassCaches) {
    if (auto maybeTree = treeCache_->get(id)) {
      stats_->increment(&ObjectStoreStats::getTreeFromMemory);
      fetchContext->didFetch(
          ObjectFetchContext::Tree, id, ObjectFetchContext::FromMemoryCache);
      updateProcessFetch(*fetchContext);
      stats_->addDuration(
          &ObjectStoreStats::getTreeMemoryDuration, watch.elapsed());
      co_return changeCaseSensitivity(std::move(maybeTree), caseSensitive_);
    }
  }
  deprioritizeWhenFetchHeavy(*fetchContext);
  auto result = co_await getTreeImpl(id, fetchContext, watch);
  TaskTraceBlock block2{"ObjectStore::getTree::thenValue"};
  if (!bypassCaches) {
    maybeCacheTreeAuxInMemCache(id, result);
  }
  auto tree = changeCaseSensitivity(std::move(result.tree), caseSensitive_);
  if (!bypassCaches) {
    treeCache_->insert(tree->getObjectId(), tree);
  }
  fetchContext->didFetch(
      ObjectFetchContext::Tree, id, result.origin, tree->getSizeBytes());
  witnessTreeFetch(id, *tree, result.origin, *fetchContext);
  updateProcessFetch(*fetchContext);
  co_return tree;
}

void ObjectStore::maybeCacheTreeAuxInMemCache(
    const ObjectId& id,
    const BackingStore::GetTreeResult& treeResult) const {
  if (treeResult.tree && treeResult.tree->getAuxData() &&
      edenConfig_->getEdenConfig()
          ->warmTreeAuxMemCacheIfTreeFromBackingStore.getValue()) {
    stats_->increment(
        &ObjectStoreStats::prewarmTreeAuxMemCacheForTreeFromBackingStore);
    treeAuxDataCache_.store(id, *treeResult.tree->getAuxData());
  }
}

bool ObjectStore::bypassInMemoryTreeCaches(
    const ObjectFetchContext& context) const {
  const auto cause = context.getCause();
  if (cause != ObjectFetchContext::Cause::Prefetch &&
      cause != ObjectFetchContext::Cause::Glob) {
    return false;
  }
  return edenConfig_->getEdenConfig()
      ->treeCacheBypassGlobAndPrefetch.getValue();
}

folly::coro::now_task<BackingStore::GetTreeResult> ObjectStore::getTreeImpl(
    const ObjectId& id,
    const ObjectFetchContextPtr& context,
    folly::stop_watch<std::chrono::milliseconds> watch) const {
  try {
    auto result = co_await backingStore_->co_getTree(id, context);
    stats_->increment(&ObjectStoreStats::getTreeFromBackingStore);
    stats_->addDuration(
        &ObjectStoreStats::getTreeBackingstoreDuration, watch.elapsed());
    co_return result;
  } catch (const std::exception&) {
    stats_->increment(&ObjectStoreStats::getTreeFailed);
    XLOGF(DBG4, "unable to find tree {}", id);
    throw;
  }
}

ImmediateFuture<std::optional<TreeAuxData>> ObjectStore::getTreeAuxData(
    const ObjectId& id,
    const ObjectFetchContextPtr& fetchContext) const {
  return startInline(
      // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
      folly::coro::co_invoke(
          [self = shared_from_this()](ObjectId oid, ObjectFetchContextPtr ctx)
              -> folly::coro::Task<std::optional<TreeAuxData>> {
            co_return co_await self->co_getTreeAuxData(oid, ctx);
          },
          ObjectId{id},
          fetchContext.copy()));
}

folly::coro::now_task<BackingStore::GetTreeAuxResult>
ObjectStore::co_getTreeAuxDataImpl(
    const ObjectId& id,
    const ObjectFetchContextPtr& context,
    folly::stop_watch<std::chrono::milliseconds> watch) const {
  try {
    auto result = co_await backingStore_->co_getTreeAuxData(id, context);
    if (result.treeAux) {
      stats_->increment(&ObjectStoreStats::getTreeAuxDataFromBackingStore);
      stats_->addDuration(
          &ObjectStoreStats::getTreeAuxDataBackingstoreDuration,
          watch.elapsed());
      co_return result;
    }
    stats_->increment(&ObjectStoreStats::getTreeAuxDataFailed);
    co_return BackingStore::GetTreeAuxResult{
        nullptr, ObjectFetchContext::Origin::NotFetched};
  } catch (const std::exception&) {
    stats_->increment(&ObjectStoreStats::getTreeAuxDataFailed);
    XLOGF(DBG4, "unable to find aux data for {}", id);
    throw;
  }
}

folly::coro::now_task<std::optional<TreeAuxData>>
ObjectStore::co_getTreeAuxData(
    const ObjectId& id,
    const ObjectFetchContextPtr& fetchContext) const {
  DurationScope<EdenStats> statScope{stats_, &ObjectStoreStats::getTreeAuxData};
  folly::stop_watch<std::chrono::milliseconds> watch;

  auto inMemoryCacheTreeAuxData =
      getTreeAuxDataFromInMemoryCache(id, fetchContext);
  if (inMemoryCacheTreeAuxData) {
    stats_->increment(&ObjectStoreStats::getTreeAuxDataFromMemory);
    stats_->addDuration(
        &ObjectStoreStats::getTreeAuxDataMemoryDuration, watch.elapsed());
    co_return std::move(inMemoryCacheTreeAuxData).value();
  }

  deprioritizeWhenFetchHeavy(*fetchContext);

  auto result = co_await co_getTreeAuxDataImpl(id, fetchContext, watch);
  if (!result.treeAux) {
    stats_->increment(&ObjectStoreStats::getTreeAuxDataFailed);
    XLOGF(DBG4, "unable to find aux data for {}", id);
    co_return std::nullopt;
  }
  auto auxData = std::move(result.treeAux);
  treeAuxDataCache_.store(id, *auxData);
  fetchContext->didFetch(ObjectFetchContext::TreeAuxData, id, result.origin);
  updateProcessFetch(*fetchContext);
  co_return *auxData;
}

folly::coro::now_task<std::optional<Hash32>> ObjectStore::co_getTreeDigestHash(
    const ObjectId& id,
    const ObjectFetchContextPtr& context) const {
  auto auxData = co_await co_getTreeAuxData(id, context);
  co_return auxData.has_value() ? auxData->digestHash : std::nullopt;
}

ImmediateFuture<std::optional<uint64_t>> ObjectStore::getTreeDigestSize(
    const ObjectId& id,
    const ObjectFetchContextPtr& context) const {
  return getTreeAuxData(id, context)
      .thenValue([](const std::optional<TreeAuxData>& auxData) {
        return auxData.has_value()
            ? std::optional<uint64_t>(auxData->digestSize)
            : std::nullopt;
      });
}

folly::coro::now_task<folly::Unit> ObjectStore::prefetchBlobs(
    ObjectIdRange ids,
    const ObjectFetchContextPtr& fetchContext) const {
  if (ids.empty()) {
    co_return folly::unit;
  }
  co_await backingStore_->co_prefetchBlobs(ids, fetchContext);
  co_return folly::unit;
}

ObjectId ObjectStore::stripObjectId(const ObjectId& id) const {
  return backingStore_->stripObjectId(id);
}

ImmediateFuture<shared_ptr<const Blob>> ObjectStore::getBlob(
    const ObjectId& id,
    const ObjectFetchContextPtr& fetchContext) const {
  DurationScope<EdenStats> statScope{stats_, &ObjectStoreStats::getBlob};

  deprioritizeWhenFetchHeavy(*fetchContext);

  return ImmediateFuture<BackingStore::GetBlobResult>{
      getBlobImpl(id, fetchContext)}
      .thenValue(
          [self = shared_from_this(),
           statScope = std::move(statScope),
           id,
           fetchContext =
               fetchContext.copy()](BackingStore::GetBlobResult result)
              -> std::shared_ptr<const Blob> {
            self->updateProcessFetch(*fetchContext);
            fetchContext->didFetch(
                ObjectFetchContext::Blob,
                id,
                result.origin,
                result.blob->getSize());
            self->witnessBlobFetch(id, result, *fetchContext);
            return std::move(result.blob);
          });
}

folly::coro::now_task<std::shared_ptr<const Blob>> ObjectStore::co_getBlob(
    const ObjectId& id,
    const ObjectFetchContextPtr& fetchContext) const {
  DurationScope<EdenStats> statScope{stats_, &ObjectStoreStats::getBlob};
  deprioritizeWhenFetchHeavy(*fetchContext);
  auto result = co_await co_getBlobImpl(id, fetchContext);
  updateProcessFetch(*fetchContext);
  fetchContext->didFetch(
      ObjectFetchContext::Blob, id, result.origin, result.blob->getSize());
  witnessBlobFetch(id, result, *fetchContext);
  co_return std::move(result.blob).unwrap();
}

folly::SemiFuture<BackingStore::GetBlobResult> ObjectStore::getBlobImpl(
    const ObjectId& id,
    const ObjectFetchContextPtr& context) const {
  return ImmediateFuture{backingStore_->getBlob(id, context)}
      .thenValue(
          [self = shared_from_this(), id](BackingStore::GetBlobResult result) {
            self->stats_->increment(&ObjectStoreStats::getBlobFromBackingStore);
            return result;
          })
      .thenError(
          [self = shared_from_this(), id](const folly::exception_wrapper& ew)
              -> ImmediateFuture<BackingStore::GetBlobResult> {
            self->stats_->increment(&ObjectStoreStats::getBlobFailed);
            XLOGF(DBG4, "unable to find blob {}", id);
            return makeImmediateFuture<BackingStore::GetBlobResult>(ew);
          })
      .semi();
}

folly::coro::now_task<BackingStore::GetBlobResult> ObjectStore::co_getBlobImpl(
    const ObjectId& id,
    const ObjectFetchContextPtr& context) const {
  try {
    auto result = co_await backingStore_->co_getBlob(id, context);
    stats_->increment(&ObjectStoreStats::getBlobFromBackingStore);
    co_return result;
  } catch (const std::exception&) {
    stats_->increment(&ObjectStoreStats::getBlobFailed);
    XLOGF(DBG4, "unable to find blob {}", id);
    throw;
  }
}

std::optional<BlobAuxData> ObjectStore::getBlobAuxDataFromInMemoryCache(
    const ObjectId& id,
    const ObjectFetchContextPtr& context,
    bool blake3Required) const {
  auto cached = blobAuxDataCache_.get(id);
  if (!cached || (blake3Required && !cached->blake3)) {
    return std::nullopt;
  }
  context->didFetch(
      ObjectFetchContext::BlobAuxData, id, ObjectFetchContext::FromMemoryCache);
  updateProcessFetch(*context);
  return cached;
}

// TODO: This code is "identical" to the blob code. Though it is small today, it
// might make sense to refactor into some kind of template and/or helper
// methods.
std::optional<TreeAuxData> ObjectStore::getTreeAuxDataFromInMemoryCache(
    const ObjectId& id,
    const ObjectFetchContextPtr& context) const {
  auto ret = treeAuxDataCache_.get(id);
  if (ret) {
    context->didFetch(
        ObjectFetchContext::TreeAuxData,
        id,
        ObjectFetchContext::FromMemoryCache);

    updateProcessFetch(*context);
  }

  return ret;
}

ImmediateFuture<BlobAuxData> ObjectStore::getBlobAuxData(
    const ObjectId& id,
    const ObjectFetchContextPtr& fetchContext,
    bool blake3Needed) const {
  // DEPRECATED: use co_getBlobAuxData directly. Kept only because
  // EdenServiceHandler.cpp, VirtualInode.cpp, FileInode.cpp, and getBlobSize
  // still consume ImmediateFuture chains;
  // delete once those paths are migrated to coroutines.
  return startInline(
      // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
      folly::coro::co_invoke(
          [self = shared_from_this()](
              ObjectId oid, ObjectFetchContextPtr ctx, bool b3)
              -> folly::coro::Task<BlobAuxData> {
            co_return co_await self->co_getBlobAuxData(oid, ctx, b3);
          },
          ObjectId{id},
          fetchContext.copy(),
          blake3Needed));
}

folly::coro::now_task<BlobAuxData> ObjectStore::co_getBlobAuxData(
    const ObjectId& id,
    const ObjectFetchContextPtr& fetchContext,
    bool blake3Needed) const {
  DurationScope<EdenStats> statScope{stats_, &ObjectStoreStats::getBlobAuxData};
  folly::stop_watch<std::chrono::milliseconds> watch;

  // Check in-memory cache
  auto inMemoryCacheBlobAuxData =
      getBlobAuxDataFromInMemoryCache(id, fetchContext);
  if (inMemoryCacheBlobAuxData) {
    if (blake3Needed && !inMemoryCacheBlobAuxData->blake3) {
      auto blob = co_await co_getBlob(id, fetchContext);
      auto blake3 = computeBlake3(*blob);
      inMemoryCacheBlobAuxData->blake3.emplace(blake3);
      blobAuxDataCache_.store(id, *inMemoryCacheBlobAuxData);
      stats_->increment(&ObjectStoreStats::getBlobAuxDataFromBlob);
      stats_->addDuration(
          &ObjectStoreStats::getBlobAuxDataFromBlobDuration, watch.elapsed());
    } else {
      stats_->increment(&ObjectStoreStats::getBlobAuxDataFromMemory);
      stats_->addDuration(
          &ObjectStoreStats::getBlobAuxDataMemoryDuration, watch.elapsed());
    }
    co_return std::move(inMemoryCacheBlobAuxData).value();
  }

  deprioritizeWhenFetchHeavy(*fetchContext);

  auto result = co_await getBlobAuxDataImpl(id, fetchContext, watch);
  if (!result.blobAux) {
    stats_->increment(&ObjectStoreStats::getBlobAuxDataFailed);
    XLOGF(DBG4, "unable to find aux data for {}", id);
    throwf<std::domain_error>("aux data {} not found", id);
  }

  auto auxData = std::move(result.blobAux);
  if (blake3Needed && !auxData->blake3) {
    auto blob = co_await co_getBlob(id, fetchContext);
    auto blake3 = computeBlake3(*blob);
    auto auxDataCopy = *auxData;
    auxDataCopy.blake3.emplace(blake3);
    blobAuxDataCache_.store(id, auxDataCopy);
    fetchContext->didFetch(ObjectFetchContext::BlobAuxData, id, result.origin);
    updateProcessFetch(*fetchContext);
    co_return auxDataCopy;
  } else {
    blobAuxDataCache_.store(id, *auxData);
    fetchContext->didFetch(ObjectFetchContext::BlobAuxData, id, result.origin);
    updateProcessFetch(*fetchContext);
    co_return *auxData;
  }
}

folly::coro::now_task<BackingStore::GetBlobAuxResult>
ObjectStore::getBlobAuxDataImpl(
    const ObjectId& id,
    const ObjectFetchContextPtr& context,
    folly::stop_watch<std::chrono::milliseconds> watch) const {
  auto resultTry = co_await folly::coro::co_awaitTry(
      backingStore_->co_getBlobAuxData(id, context));
  if (resultTry.hasException()) {
    stats_->increment(&ObjectStoreStats::getBlobAuxDataFailed);
    XLOGF(DBG4, "unable to find aux data for {}", id);
    co_yield folly::coro::co_error(std::move(resultTry).exception());
  }

  auto result = std::move(resultTry).value();
  if (result.blobAux && result.blobAux->sha1 != kZeroHash) {
    stats_->increment(&ObjectStoreStats::getBlobAuxDataFromBackingStore);
    stats_->addDuration(
        &ObjectStoreStats::getBlobAuxDataBackingstoreDuration, watch.elapsed());
    co_return result;
  }

  auto blobResultTry =
      co_await folly::coro::co_awaitTry(co_getBlobImpl(id, context));
  if (blobResultTry.hasException()) {
    stats_->increment(&ObjectStoreStats::getBlobAuxDataFailed);
    XLOGF(DBG4, "unable to find aux data for {}", id);
    co_yield folly::coro::co_error(std::move(blobResultTry).exception());
  }

  auto blobResult = std::move(blobResultTry).value();
  if (blobResult.blob) {
    stats_->increment(&ObjectStoreStats::getBlobAuxDataFromBlob);

    std::optional<Hash32> blake3;
    if (result.blobAux && result.blobAux->blake3.has_value()) {
      blake3 = result.blobAux->blake3.value();
    }

    stats_->addDuration(
        &ObjectStoreStats::getBlobAuxDataFromBlobDuration, watch.elapsed());

    co_return BackingStore::GetBlobAuxResult{
        std::make_shared<BlobAuxData>(
            Hash20::sha1(blobResult.blob->getContents()),
            std::move(blake3),
            blobResult.blob->getSize()),
        blobResult.origin};
  }

  stats_->increment(&ObjectStoreStats::getBlobAuxDataFailed);
  co_return BackingStore::GetBlobAuxResult{
      nullptr, ObjectFetchContext::Origin::NotFetched};
}

ImmediateFuture<uint64_t> ObjectStore::getBlobSize(
    const ObjectId& id,
    const ObjectFetchContextPtr& context) const {
  return getBlobAuxData(id, context).thenValue([](const BlobAuxData& auxData) {
    return auxData.size;
  });
}

folly::coro::now_task<uint64_t> ObjectStore::co_getBlobSize(
    const ObjectId& id,
    const ObjectFetchContextPtr& context) const {
  auto auxData = co_await co_getBlobAuxData(id, context);
  co_return auxData.size;
}

// DEPRECATED: use co_getBlobSha1 directly. Kept only because
// FileInode::getSha1, VirtualInode::getSHA1, CheckoutAction::loadAction,
// FileInode::isSameAs, FileInode::startMaterializingData, and
// ObjectStore::areBlobsEqual still consume ImmediateFuture chains;
// delete once those paths are migrated to coroutines.
ImmediateFuture<Hash20> ObjectStore::getBlobSha1(
    const ObjectId& id,
    const ObjectFetchContextPtr& context) const {
  return startInline(
      // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
      folly::coro::co_invoke(
          [self = shared_from_this()](
              ObjectId id,
              ObjectFetchContextPtr context) -> folly::coro::Task<Hash20> {
            co_return co_await self->co_getBlobSha1(id, context);
          },
          ObjectId{id},
          context.copy()));
}

Hash32 ObjectStore::computeBlake3(const Blob& blob) const {
  const auto& content = blob.getContents();
  // This should maybe be read at startup and saved in a member variable, but in
  // practice this key should never change. Hold the snapshot while hashing: a
  // reference into a temporary snapshot dangles once a reload drops it.
  const auto edenConfig = edenConfig_->getEdenConfig();
  const auto& maybeBlakeKey = edenConfig->blake3Key.getValue();
  return maybeBlakeKey ? Hash32::keyedBlake3(
                             folly::ByteRange{folly::StringPiece{
                                 maybeBlakeKey->data(), maybeBlakeKey->size()}},
                             content)
                       : Hash32::blake3(content);
}

folly::coro::now_task<Hash20> ObjectStore::co_getBlobSha1(
    const ObjectId& id,
    const ObjectFetchContextPtr& context) const {
  auto auxData = co_await co_getBlobAuxData(id, context);
  co_return auxData.sha1;
}

folly::coro::now_task<Hash32> ObjectStore::co_getBlobBlake3(
    const ObjectId& id,
    const ObjectFetchContextPtr& context) const {
  auto auxData =
      co_await co_getBlobAuxData(id, context, true /* blake3Needed */);
  if (auxData.blake3) {
    co_return *auxData.blake3;
  }

  EDEN_BUG() << fmt::format("Blake3 hash is not defined for id={}", id);
}

ImmediateFuture<bool> ObjectStore::areBlobsEqual(
    const ObjectId& one,
    const ObjectId& two,
    const ObjectFetchContextPtr& context) const {
  if (areObjectsKnownIdentical(one, two)) {
    return true;
  }

  // If Mercurial eventually switches to using blob IDs that are solely
  // based on the file contents (as opposed to file contents + history)
  // then we could drop this extra load of the blob SHA-1, and rely only
  // on the blob ID comparison instead.
  // TODO: replace with blake3 as its faster
  return collectAllSafe(getBlobSha1(one, context), getBlobSha1(two, context))
      .thenValue([](const std::tuple<Hash20, Hash20>& sha1s) {
        return std::get<0>(sha1s) == std::get<1>(sha1s);
      });
}

folly::coro::now_task<bool> ObjectStore::co_areBlobsEqual(
    const ObjectId& one,
    const ObjectId& two,
    const ObjectFetchContextPtr& context) const {
  if (areObjectsKnownIdentical(one, two)) {
    co_return true;
  }
  // Parallel SHA-1 fetches via co_getBlobSha1 (already a coroutine).
  auto results = co_await folly::coro::collectAll(
      folly::coro::co_invoke(
          [this, one, ctx = context.copy()]() -> folly::coro::Task<Hash20> {
            co_return co_await co_getBlobSha1(one, ctx);
          }),
      folly::coro::co_invoke(
          [this, two, ctx = context.copy()]() -> folly::coro::Task<Hash20> {
            co_return co_await co_getBlobSha1(two, ctx);
          }));
  co_return std::get<0>(results) == std::get<1>(results);
}

ObjectComparison ObjectStore::compareObjectsById(
    const ObjectId& one,
    const ObjectId& two) const {
  return backingStore_->compareObjectsById(one, two);
}

ObjectComparison ObjectStore::compareRootsById(
    const RootId& one,
    const RootId& two) const {
  return backingStore_->compareRootsById(one, two);
}

bool ObjectStore::areObjectsKnownIdentical(
    const ObjectId& one,
    const ObjectId& two) const {
  return backingStore_->compareObjectsById(one, two) ==
      ObjectComparison::Identical;
}

ImmediateFuture<bool> ObjectStore::checkPermissionIfExpired(
    const ObjectId& manifestId,
    std::chrono::steady_clock::time_point lastCheck) const {
  stats_->increment(&ObjectStoreStats::checkPermission);
  auto ttl = std::chrono::seconds{
      getEdenConfig()->restrictedTreeTtlSeconds.getValue()};
  if (std::chrono::steady_clock::now() - lastCheck < ttl) {
    return false; // TTL not expired, still restricted
  }
  stats_->increment(&ObjectStoreStats::checkPermissionFromBackingStore);
  return backingStore_->checkPermission(manifestId);
}

folly::coro::now_task<std::vector<folly::Try<std::vector<EntryAcl>>>>
ObjectStore::co_getPathAcls(
    const RootId& rootId,
    const std::vector<std::string>& paths,
    const ObjectFetchContextPtr& context) const {
  co_return co_await backingStore_->co_getPathAcls(rootId, paths, context);
}

} // namespace facebook::eden
