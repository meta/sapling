/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/service/ThriftGlobImpl.h"

#include <folly/coro/Collect.h>
#include <folly/coro/Invoke.h>
#include <folly/coro/Task.h>
#include <folly/coro/safe/NowTask.h>
#include <folly/logging/LogLevel.h>
#include <folly/logging/xlog.h>
#include <memory>
#include "eden/common/utils/UnboundedQueueExecutor.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/GlobNode.h"
#include "eden/fs/inodes/ServerState.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/model/RootId.h"
#include "eden/fs/service/gen-cpp2/eden_types.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/store/PathLoader.h"
#include "eden/fs/utils/EdenError.h"
#include "eden/fs/utils/GlobNodeImpl.h"
#include "eden/fs/utils/GlobResult.h"
#include "eden/fs/utils/GlobTree.h"

namespace facebook::eden {

namespace {
// Compile the list of globs into a tree
void compileGlobs(const std::vector<std::string>& globs, GlobNodeImpl& root) {
  try {
    for (auto& globString : globs) {
      try {
        root.parse(globString);
      } catch (const std::domain_error& exc) {
        throw newEdenError(
            EdenErrorType::ARGUMENT_ERROR,
            "Invalid glob (",
            exc.what(),
            "): ",
            globString);
      }
    }
  } catch (const std::system_error& exc) {
    throw newEdenError(exc);
  }
}

} // namespace

ThriftGlobImpl::ThriftGlobImpl(const GlobParams& params)
    : includeDotfiles_{*params.includeDotfiles()},
      prefetchFiles_{*params.prefetchFiles()},
      suppressFileList_{*params.suppressFileList()},
      wantDtype_{*params.wantDtype()},
      listOnlyFiles_{*params.listOnlyFiles()},
      rootIds_{*params.revisions()},
      searchRootUser_{*params.searchRoot()} {}

ThriftGlobImpl::ThriftGlobImpl(const PrefetchParams& params)
    : includeDotfiles_{true},
      prefetchFiles_{!*params.directoriesOnly()},
      suppressFileList_{!*params.returnPrefetchedFiles()},
      rootIds_{*params.revisions()},
      searchRootUser_{*params.searchRoot()} {}

folly::coro::now_task<std::unique_ptr<Glob>> ThriftGlobImpl::glob(
    std::shared_ptr<EdenMount> edenMount,
    std::shared_ptr<ServerState> serverState,
    std::vector<std::string> globs,
    const ObjectFetchContextPtr& fetchContext) {
  auto config = serverState->getEdenConfig();
  auto globMatchOptions = serverState->getGlobMatchOptions();
  size_t prefetchBlobBatchSize = config->prefetchBlobBatchSize.getValue();
  if (prefetchBlobBatchSize == 0) {
    XLOG_EVERY_MS(ERR, 60'000)
        << "thrift:prefetch-blob-batch-size must be positive";
    prefetchBlobBatchSize = 1;
  }
  bool dedupePrefetchFiles = config->globDedupePrefetchFiles.getValue();

  auto fileBlobsToPrefetch =
      prefetchFiles_ ? std::make_shared<PrefetchList>() : nullptr;

  // These ids must outlive the GlobResult created by evaluate as the
  // GlobResults will hold on to references to these ids
  auto originRootIds = std::make_unique<std::vector<RootId>>();

  // Globs will be evaluated against the specified commits or the current commit
  // if none are specified. The results will be collected here.
  std::vector<folly::coro::Task<void>> globTasks{};
  auto globResults = std::make_shared<ResultList>();

  RelativePath searchRoot;
  if (!(searchRootUser_.empty() || searchRootUser_ == ".")) {
    searchRoot = RelativePath{searchRootUser_};
  }
  std::shared_ptr<GlobTree> globTree = nullptr;
  std::shared_ptr<GlobNode> globNode = nullptr;

  if (!rootIds_.empty()) {
    // Note that we MUST reserve here, otherwise while emplacing we might
    // invalidate the earlier commitId references
    globTasks.reserve(rootIds_.size());
    originRootIds->reserve(rootIds_.size());
    auto caseSensitivity =
        serverState->getEdenConfig()->globUseMountCaseSensitivity.getValue()
        ? edenMount->getCheckoutConfig()->getCaseSensitive()
        : CaseSensitivity::Sensitive;
    globTree = std::make_shared<GlobTree>(
        bool(includeDotfiles_),
        caseSensitivity,
        serverState->getEdenConfig()->globRecursiveAsyncDepth.getValue(),
        globMatchOptions);
    compileGlobs(globs, *globTree);
    for (auto& rootId : rootIds_) {
      const RootId& originRootId = originRootIds->emplace_back(
          edenMount->getObjectStore()->parseRootId(rootId));

      globTasks.emplace_back(
          folly::coro::co_invoke(
              [edenMount,
               globTree,
               fetchContext = fetchContext.copy(),
               searchRoot,
               fileBlobsToPrefetch,
               globResults,
               &originRootId,
               suppressFileList =
                   suppressFileList_]() mutable -> folly::coro::Task<void> {
                auto rootTree =
                    co_await edenMount->getObjectStore()->co_getRootTree(
                        originRootId, fetchContext);
                auto tree = co_await resolveTree(
                    *edenMount->getObjectStore(),
                    fetchContext,
                    std::move(rootTree.tree),
                    searchRoot);
                co_await globTree->evaluate(
                    edenMount->getObjectStore(),
                    fetchContext,
                    RelativePathPiece(),
                    std::move(tree),
                    fileBlobsToPrefetch.get(),
                    suppressFileList ? nullptr : globResults.get(),
                    originRootId);
              }));
    }
  } else {
    bool includeDotfiles = includeDotfiles_;
    CaseSensitivity caseSensitive =
        serverState->getEdenConfig()->globUseMountCaseSensitivity.getValue()
        ? edenMount->getCheckoutConfig()->getCaseSensitive()
        : CaseSensitivity::Sensitive;
    uint32_t asyncDepth =
        serverState->getEdenConfig()->globRecursiveAsyncDepth.getValue();
    globNode = std::make_shared<GlobNode>(
        includeDotfiles, caseSensitive, asyncDepth, globMatchOptions);
    compileGlobs(globs, *globNode);
    const RootId& originRootId =
        originRootIds->emplace_back(edenMount->getCheckedOutRootId());
    globTasks.emplace_back(
        folly::coro::co_invoke(
            [fetchContext = fetchContext.copy(),
             globNode,
             edenMount,
             fileBlobsToPrefetch,
             globResults,
             &originRootId,
             searchRoot,
             suppressFileList =
                 suppressFileList_]() mutable -> folly::coro::Task<void> {
              auto inode =
                  co_await edenMount->co_getInodeSlow(searchRoot, fetchContext);
              co_await globNode->evaluate(
                  edenMount->getObjectStore(),
                  fetchContext,
                  RelativePathPiece(),
                  inode.asTreePtr(),
                  fileBlobsToPrefetch.get(),
                  suppressFileList ? nullptr : globResults.get(),
                  originRootId);
            }));
  }

  // Copy member fields into locals before the suspension point below.
  bool suppressFileList = suppressFileList_;
  bool listOnlyFiles = listOnlyFiles_;
  bool wantDtype = wantDtype_;
  size_t numRevisions = rootIds_.size();

  // When there are 0 or 1 revisions, every entry has the same origin hash.
  // Skip the per-file renderRootId() call and the resulting list, as no
  // caller can use it to distinguish between revisions.
  bool populateOriginHashes = numRevisions > 1;

  // Note: we use collectAllTryRange() rather than collectAllRange() here
  // because collectAllRange() sends cooperative cancellation to sibling
  // tasks on first failure, which could cause incomplete writes to
  // globResults/fileBlobsToPrefetch. collectAllTryRange() lets all tasks
  // run to completion before propagating errors.
  auto globTries =
      co_await folly::coro::collectAllTryRange(std::move(globTasks));

  fetchContext->throwIfCanceled();
  for (auto& t : globTries) {
    t.throwUnlessValue();
  }

  // Post-process: sort and dedup results
  std::vector<GlobResult> sortedResults;
  if (!suppressFileList) {
    std::swap(sortedResults, *globResults->wlock());
    std::sort(sortedResults.begin(), sortedResults.end());
    auto resultsNewEnd =
        std::unique(sortedResults.begin(), sortedResults.end());
    sortedResults.erase(resultsNewEnd, sortedResults.end());
  }

  // Deduplicate files as an optimization. The BackingStore does not
  // necessarily dedupe fetches (although SaplingBackingStore does
  // in the scmstore::FileStore, per batch).
  //
  // Note that normally duplicates are rare, and deduping a list of
  // millions of files can take 5s, so it typically is not worth it.
  if (dedupePrefetchFiles && fileBlobsToPrefetch) {
    auto fileBlobsToPrefetchLocked = fileBlobsToPrefetch->wlock();
    folly::F14FastSet<ObjectId> seen;
    size_t i = 0;
    while (i < fileBlobsToPrefetchLocked->size()) {
      if (seen.insert((*fileBlobsToPrefetchLocked)[i]).second) {
        ++i;
      } else {
        std::swap(
            (*fileBlobsToPrefetchLocked)[i], fileBlobsToPrefetchLocked->back());
        fileBlobsToPrefetchLocked->pop_back();
      }
    }
  }

  // Build the output
  auto out = std::make_unique<Glob>();

  if (!suppressFileList) {
    for (auto& entry : sortedResults) {
      if (!listOnlyFiles || entry.dtype != dtype_t::Dir) {
        out->matchingFiles()->emplace_back(std::move(entry.name));

        if (wantDtype) {
          out->dtypes()->emplace_back(static_cast<OsDtype>(entry.dtype));
        }

        if (populateOriginHashes) {
          out->originHashes()->emplace_back(
              edenMount->getObjectStore()->renderRootId(*entry.originId));
        }
      }
    }
  }

  // Prefetch blobs in parallel batches
  if (fileBlobsToPrefetch) {
    auto store = edenMount->getObjectStore();
    // All glob tasks have finished writing the list. Own its storage locally
    // so the batch ranges stay valid without holding a lock across co_await.
    std::vector<ObjectId> blobs;
    std::swap(blobs, *fileBlobsToPrefetch->wlock());
    auto range = folly::Range{blobs.data(), blobs.size()};

    std::vector<folly::coro::Task<void>> prefetchTasks;
    while (range.size() > prefetchBlobBatchSize) {
      auto curRange = range.subpiece(0, prefetchBlobBatchSize);
      range.advance(prefetchBlobBatchSize);
      prefetchTasks.emplace_back(
          folly::coro::co_invoke(
              [store, curRange, fetchContext = fetchContext.copy()]()
                  -> folly::coro::Task<void> {
                co_await folly::coro::co_reschedule_on_current_executor;
                fetchContext->throwIfCanceled();
                co_await store->prefetchBlobs(curRange, fetchContext);
              }));
    }
    if (!range.empty()) {
      prefetchTasks.emplace_back(
          folly::coro::co_invoke(
              [store, range, fetchContext = fetchContext.copy()]()
                  -> folly::coro::Task<void> {
                co_await folly::coro::co_reschedule_on_current_executor;
                fetchContext->throwIfCanceled();
                co_await store->prefetchBlobs(range, fetchContext);
              }));
    }
    // Use collectAllTryRange (not collectAllRange) to avoid cooperative
    // cancellation on failure. Prefetch errors are silently discarded
    // to match the futures glob implementation — partial prefetching
    // is still useful.
    co_await folly::coro::collectAllTryRange(std::move(prefetchTasks));
    // Batches skipped by a cancellation leave the prefetch incomplete, which
    // must not be reported as success.
    fetchContext->throwIfCanceled();
  }

  co_return out;
}

std::string ThriftGlobImpl::logString() {
  return fmt::format(
      "ThriftGlobImpl {{ includeDotFiles={}, prefetchFiles={}, suppressFileList={}, wantDtype={}, listOnlyFiles={}, rootIds={}, searchRootUser={} }}",
      includeDotfiles_,
      prefetchFiles_,
      suppressFileList_,
      wantDtype_,
      listOnlyFiles_,
      fmt::join(rootIds_, ", "),
      searchRootUser_);
}

std::string ThriftGlobImpl::logString(
    const std::vector<std::string>& globs) const {
  return fmt::format(
      "ThriftGlobImpl {{ globs={}, includeDotFiles={}, prefetchFiles={}, suppressFileList={}, wantDtype={}, listOnlyFiles={}, rootIds={}, searchRootUser={} }}",
      fmt::join(globs, ", "),
      includeDotfiles_,
      prefetchFiles_,
      suppressFileList_,
      wantDtype_,
      listOnlyFiles_,
      fmt::join(rootIds_, ", "),
      searchRootUser_);
}

} // namespace facebook::eden
