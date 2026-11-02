/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/inodes/InodeAccessLogger.h"

#include <folly/Random.h>
#include <folly/logging/xlog.h>
#include <folly/system/ThreadName.h>

#include <re2/re2.h>

#include "eden/common/utils/PathFuncs.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/InodeMap.h"
#include "eden/fs/store/ObjectStore.h"

#include "eden/common/telemetry/DynamicEvent.h"
#include "eden/common/telemetry/Stats.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/telemetry/IXplatLogger.h"
#include "eden/fs/telemetry/XplatKeys.h"

namespace facebook::eden {

namespace {
constexpr folly::StringPiece kHgDirectory{".hg"};
constexpr folly::StringPiece kEdenDirectory{".eden"};

constexpr folly::StringPiece kFbsource{"fbsource"};
// TODO(helsel): make this configurable in EdenConfig
const static RE2 kFbsourceFilter("xplat\\/toolchains\\/minimal_xcode");

} // namespace

InodeAccessLogger::InodeAccessLogger(
    std::shared_ptr<ReloadableConfig> reloadableConfig,
    EdenStatsPtr edenStats,
    std::shared_ptr<IXplatLogger> xplatLogger)
    : reloadableConfig_{std::move(reloadableConfig)},
      edenStats_{std::move(edenStats)},
      xplatLogger_{std::move(xplatLogger)} {
  workerThread_ = std::thread{[this] {
    folly::setThreadName("InodeAccessLoggerProcessor");
    processInodeAccessEvents();
  }};
}

InodeAccessLogger::~InodeAccessLogger() {
  state_.wlock()->workerThreadShouldStop = true;
  // Do one final post here to ensure the thread wakes up, sees that
  // workerThreadShouldStop, and returns from processInodeAccessEvents().
  // Otherwise, the workerThread_ would be stuck waiting on the semaphore in
  // the infinite work loop in processInodeAccessEvents and would never join
  sem_.post();
  workerThread_.join();
  XLOG(INFO, "InodeAccessLogger shut down");
}

bool InodeAccessLogger::filterDirectory(
    folly::StringPiece directory,
    folly::StringPiece repo) {
  // Don't log events from the .eden or .hg directories. startsWith has an
  // empty check so we should not go out of bounds here.
  if (directory.startsWith(kEdenDirectory) ||
      directory.startsWith(kHgDirectory)) {
    return true;
  }

  if (repo == kFbsource) {
    // Use PartialMatch and avoid a trailing .*
    return RE2::PartialMatch(directory.str(), kFbsourceFilter);
  }

  return false;
}

void InodeAccessLogger::logFileAccessViaXplat(
    folly::StringPiece repo,
    const std::string& directory,
    const std::string& filename,
    const std::string& source,
    const std::string& sourceDetail) {
  DynamicEvent event;
  event.addString(std::string{xplat_keys::kRepo}, repo.str());
  event.addString(std::string{xplat_keys::kDirectory}, directory);
  if (!filename.empty()) {
    event.addString(std::string{xplat_keys::kFilename}, filename);
  }
  event.addString(std::string{xplat_keys::kSource}, source);
  if (!sourceDetail.empty()) {
    event.addString(std::string{xplat_keys::kSourceDetail}, sourceDetail);
  }
  xplatLogger_->logEvent(std::string{xplat_keys::kFileAccessCategory}, event);
}

void InodeAccessLogger::processInodeAccessEvents() {
  std::vector<InodeAccess> work;

  for (;;) {
    work.clear();

    sem_.wait();

    {
      auto state = state_.wlock();
      if (state->workerThreadShouldStop) {
        // We may lose log events here if the work queue is not empty, but
        // these events are not important enough to block shutdown.
        return;
      }

      work.swap(state->work);
    }

    const auto denominator =
        reloadableConfig_->getEdenConfig()
            ->logFileAccessesSamplingDenominator.getValue();

    for (auto& event : work) {
      folly::StringPiece repo;
      std::optional<RelativePath> path;
      try {
        if (auto mount = event.edenMount.lock()) {
          auto repo_optional =
              mount->getObjectStore()->getBackingStore()->getRepoName();
          if (repo_optional == std::nullopt) {
            XLOG(
                DBG5,
                "InodeAccessLogger couldn't get repo name from backing store");
            continue;
          }
          repo = repo_optional.value();
          path = mount->getInodeMap()->getPathForInode(event.inodeNumber);
        } else {
          // The pointer has expired, just continue to the next event
          continue;
        }
      } catch (const std::exception& ex) {
        // getMount can throw if the mount is not known to the EdenFS instance
        // or if it is not safe for inode access.
        // getPathForInode can throw if the inode is invalid. Since we
        // process these events in an async queue, it is possible that the
        // inode is invalidated before we get to it. In this case, just
        // continue to the next event.
        XLOGF_EVERY_MS(
            WARN, 30000, "Error looking up inode path: {}", ex.what());
        path = std::nullopt;
      }

      // Don't log if the path does not exist or if getPathForInode threw an
      // error
      if (path == std::nullopt) {
        continue;
      }

      dtype_t dtype = event.dtype;
      std::string directory;

      if (dtype == dtype_t::Dir) {
        directory = path->asString();
      } else {
        directory = path->dirname().asString();
      }

      // Check if this directory matches one of the in place global or repo
      // specific filters
      if (filterDirectory(directory, repo)) {
        continue;
      }

      // This will be empty if denominator != 1, we only log filename if we're
      // logging 100% of file accesses.
      std::string filename;

      // Only log the filename if we're logging 100% of file accesses and the
      // path is not to a directory
      if (denominator == 1 && dtype != dtype_t::Dir) {
        filename = path->basename().asString();
      } else {
        // If we're not logging filenames, and the directory is empty (meaning
        // this was a top level file access), lets just not log it.
        if (directory.empty()) {
          continue;
        }
      }

      // TODO: Don't log files that match gitignore rules.

      std::string source;
      switch (event.cause) {
        case ObjectFetchContext::Unknown:
          source = "unknown";
          break;
        case ObjectFetchContext::Fs:
          source = "fs";
          break;
        case ObjectFetchContext::Thrift:
          source = "thrift";
          break;
        case ObjectFetchContext::Prefetch:
          source = "prefetch";
          break;
        case ObjectFetchContext::Glob:
          source = "glob";
          break;
        case ObjectFetchContext::InodeGc:
          source = "inode_gc";
          break;
      }

      std::string sourceDetail;
      if (event.causeDetail.has_value()) {
        sourceDetail = event.causeDetail.value();
      }

      // TODO: Instead of logging every access individually, store the events in
      // a memory bounded LRU cache and coalesce the events by directory,
      // source, source_detail, and filename (if applicable). Then, a periodic
      // task can flush this cache to Scuba. This would make it possible to
      // increase the InodeAccessesPercentage without overwhelming Scribe.

      // File accesses are logged exclusively via the XplatLogger Thrift path
      // (Compact Protocol + ScribeD RPC). OSS builds have no xplat logger, so
      // guard against a null logger and no-op rather than dropping through to a
      // removed backend.
      if (xplatLogger_ != nullptr) {
        edenStats_->increment(&TelemetryStats::fileAccessViaXplatLogger);
        logFileAccessViaXplat(repo, directory, filename, source, sourceDetail);
      }
    }
  }
}

void InodeAccessLogger::logInodeAccess(InodeAccess access) {
  auto config = reloadableConfig_->getEdenConfig();
  if (!config->logFileAccesses.getValue()) {
    return;
  }
  // Sampling is uniform, so deciding it here gives the same distribution of
  // logged events as deciding it on the worker, and an event that is dropped
  // costs neither the queue, the wakeup nor the path lookup.
  if (0 !=
      folly::Random::rand32(
          config->logFileAccessesSamplingDenominator.getValue())) {
    return;
  }
  bool wasIdle;
  {
    auto state = state_.wlock();
    wasIdle = state->work.empty();
    state->work.push_back(std::move(access));
  }
  // The worker drains the whole queue per wakeup, so only the event that
  // makes it non-empty needs to wake it.
  if (wasIdle) {
    sem_.post();
  }
}
} // namespace facebook::eden
