/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/inodes/ServerState.h"

#include <folly/executors/IOThreadPoolExecutor.h>
#include <folly/executors/thread_factory/NamedThreadFactory.h>
#include <folly/logging/xlog.h>
#include <gflags/gflags.h>
#include <utility>
#include <vector>

#include "eden/common/telemetry/SessionInfo.h"
#include "eden/common/utils/FaultInjector.h"
#include "eden/common/utils/UnboundedQueueExecutor.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/config/ReloadableConfig.h"
#include "eden/fs/inodes/InodeAccessLogger.h"
#include "eden/fs/model/git/TopLevelIgnores.h"
#include "eden/fs/nfs/NfsServer.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/telemetry/ErrorLogger.h"
#include "eden/fs/telemetry/IXplatLogger.h"
#include "eden/fs/utils/Clock.h"

DEFINE_bool(
    fault_injection_block_mounts,
    false,
    "Block mount attempts via the fault injection framework.  "
    "Requires --enable_fault_injection.");

namespace facebook::eden {
/** Throttle Ignore change checks, max of 1 per kUserIgnoreMinPollSeconds */
constexpr std::chrono::seconds kUserIgnoreMinPollSeconds{5};

/** Throttle Ignore change checks, max of 1 per kSystemIgnoreMinPollSeconds */
constexpr std::chrono::seconds kSystemIgnoreMinPollSeconds{5};

/** Number of threads in the dedicated preload I/O pool. Sized to drive a
 *  FUSE mount with parallel self-reads without starving other pools; kept in
 *  step with the default per-operation preload worker count. */
constexpr size_t kPreloadThreadPoolSize{32};

/** How often to sweep completed preload operations from the progress map. */
constexpr std::chrono::seconds kPreloadCleanupInterval{30};

/** How long a completed preload operation is retained for late polling
 *  before the periodic sweep evicts it. */
constexpr std::chrono::seconds kPreloadCompletionTtl{60};

/** How long a not-yet-done preload operation may go without observed
 *  progress before it's presumed stranded (by a bug) and reaped. */
constexpr std::chrono::hours kMaxStrandedLifetime{1};

ServerState::ServerState(
    UserInfo userInfo,
    EdenStatsPtr edenStats,
    [[maybe_unused]] SessionInfo
        sessionInfo, // NOLINT(performance-unnecessary-value-param)
    std::shared_ptr<PrivHelper> privHelper,
    std::shared_ptr<UnboundedQueueExecutor> threadPool,
    std::shared_ptr<UnboundedQueueExecutor> fsChannelThreadPool,
    std::shared_ptr<Clock> clock,
    std::shared_ptr<ProcessInfoCache> processInfoCache,
    std::shared_ptr<StructuredLogger> structuredLogger,
    std::shared_ptr<StructuredLogger> notificationsStructuredLogger,
    std::shared_ptr<ErrorLogger> errorLogger,
    std::shared_ptr<ReloadableConfig> reloadableConfig,
    const EdenConfig& initialConfig,
    folly::EventBase* nfsEventBase,
    std::shared_ptr<Notifier> notifier,
    bool enableFaultDetection,
    std::shared_ptr<InodeAccessLogger> inodeAccessLogger,
    std::shared_ptr<IXplatLogger> xplatLogger)
    : userInfo_{std::move(userInfo)},
      edenStats_{std::move(edenStats)},
      privHelper_{std::move(privHelper)},
      threadPool_{std::move(threadPool)},
      fsChannelThreadPool_{std::move(fsChannelThreadPool)},
      clock_{std::move(clock)},
      processInfoCache_{std::move(processInfoCache)},
      structuredLogger_{std::move(structuredLogger)},
      edenFsEventsLogger_{std::make_shared<EdenFsEventsLogger>(xplatLogger)},
      notificationsStructuredLogger_{std::move(notificationsStructuredLogger)},
      errorLogger_{std::move(errorLogger)},
      faultInjector_{std::make_unique<FaultInjector>(enableFaultDetection)},
      nfsEventBase_{nfsEventBase},
      nfs_{
          initialConfig.enableNfsServer.getValue()
              ? makeNfsServer(initialConfig)
              : nullptr},
      config_{std::move(reloadableConfig)},
      userIgnoreFileMonitor_{CachedParsedFileMonitor<GitIgnoreFileParser>{
          initialConfig.userIgnoreFile.getValue(),
          kUserIgnoreMinPollSeconds}},
      systemIgnoreFileMonitor_{CachedParsedFileMonitor<GitIgnoreFileParser>{
          initialConfig.systemIgnoreFile.getValue(),
          kSystemIgnoreMinPollSeconds}},
      notifier_{std::move(notifier)},
      inodeAccessLogger_{
          inodeAccessLogger ? std::move(inodeAccessLogger)
                            : std::make_shared<InodeAccessLogger>(
                                  config_,
                                  edenStats_.copy(),
                                  std::move(xplatLogger))} {
  // It would be nice if we eventually built a more generic mechanism for
  // defining faults to be configured on start up.  (e.g., loading this from
  // the EdenConfig).
  //
  // For now, blocking mounts and failing localstore opening are the main
  // things we want to be able to control on startup (since mounting and
  // opening localstore occurs automatically during startup).  Add a two-off
  // command
  // line flag to control this for now, until we build a more generic
  // mechanism.
  if (FLAGS_fault_injection_block_mounts) {
    faultInjector_->injectBlock("mount", ".*");
  }

  preloadCleanupScheduler_.addFunction(
      [this] { cleanupStalePreloadProgress(kPreloadCompletionTtl); },
      kPreloadCleanupInterval,
      "preload-cleanup");
  preloadCleanupScheduler_.start();
}

ServerState::~ServerState() {
  // Stop the cleanup scheduler before any of the members it touches are
  // destroyed (notably preloadProgressMap_). Executor shutdown is explicit via
  // shutdown(), since the final ServerState reference may be released on an
  // executor thread.
  shutdownPreloadCleanup();
}

void ServerState::shutdownPreloadCleanup() {
  folly::call_once(preloadCleanupShutdownOnceFlag_, [this] {
    preloadCleanupScheduler_.shutdown();
  });
}

void ServerState::shutdown() {
  shutdownPreloadCleanup();

  folly::call_once(executorShutdownOnceFlag_, [this] {
    // Synchronize with lazy initialization without creating the preload pool
    // solely to shut it down.
    folly::call_once(preloadThreadPoolOnceFlag_, [] {});
    if (preloadThreadPool_) {
      preloadThreadPool_->join();
    }

    if (fsChannelThreadPool_.get() != threadPool_.get()) {
      fsChannelThreadPool_->join();
    }
    threadPool_->join();
  });
}

std::shared_ptr<NfsServer> ServerState::makeNfsServer(
    const EdenConfig& config) const {
  return std::make_shared<NfsServer>(
      privHelper_.get(),
      nfsEventBase_,
      fsChannelThreadPool_,
      edenFsEventsLogger_,
      config.maxFsChannelInflightRequests.getValue(),
      config.highFsRequestsLogInterval.getValue(),
      config.longRunningFSRequestThreshold.getValue());
}

void ServerState::createNfsServerForTakeover() {
  if (nfs_) {
    return;
  }
  nfs_ = makeNfsServer(*getEdenConfig());
}

GlobMatchOptions ServerState::getGlobMatchOptions() {
  auto config = getEdenConfig();
  GlobMatchOptions options;
  options.enableFailureMemoization =
      config->globEnableFailureMemoization.getValue();
  options.maxMemoizedFailureStates =
      config->globMaxMemoizedFailureStates.getValue();
  options.maxBacktrackingSteps = config->globMaxBacktrackingSteps.getValue();
  // RefPtr is move-only, while std::function requires a copyable callable.
  auto stats = std::make_shared<EdenStatsPtr>(edenStats_.copy());
  options.limitReachedCallback = [stats =
                                      std::move(stats)](GlobMatchLimit limit) {
    if (limit == GlobMatchLimit::MemoizedFailureStates) {
      (*stats)->increment(&GlobStats::memoizedFailureStateLimitExceeded);
    } else {
      (*stats)->increment(&GlobStats::backtrackingStepLimitExceeded);
    }
  };
  return options;
}

const std::shared_ptr<folly::IOThreadPoolExecutor>&
ServerState::getPreloadThreadPool() const {
  // Most EdenFS processes never run a preload, so defer paying for the
  // thread pool's 32 threads until the first caller actually needs it.
  folly::call_once(preloadThreadPoolOnceFlag_, [this] {
    preloadThreadPool_ = std::make_shared<folly::IOThreadPoolExecutor>(
        kPreloadThreadPoolSize,
        std::make_shared<folly::NamedThreadFactory>("EdenPreload"));
  });
  return preloadThreadPool_;
}

void ServerState::cleanupStalePreloadProgress(std::chrono::seconds maxAge) {
  // An operation with no completionTime is still running (or stranded by a
  // bug). Age alone can't distinguish the two - large preloads can
  // legitimately run for hours, and progress counters may only advance at
  // coarse per-chunk granularity - so an operation is presumed stranded only
  // once a full kMaxStrandedLifetime passes with no sweep observing its
  // progress counters move. Reaping stranded entries bounds the map size;
  // progress queries for a reaped id report not-found.
  const auto kMaxStrandedLifetimeSec =
      std::chrono::duration_cast<std::chrono::seconds>(kMaxStrandedLifetime)
          .count();
  auto now = std::chrono::steady_clock::now();

  // Snapshot the map's entries under a brief read lock. Each entry is a
  // shared_ptr, so the copy is cheap; the reap-vs-keep evaluation below then
  // runs without holding the map lock, so a large sweep can't block
  // concurrent progress queries / removePreloadProgress calls.
  std::vector<std::pair<std::string, std::shared_ptr<PreloadOperation>>>
      snapshot;
  {
    auto map = preloadProgressMap_.rlock();
    snapshot.reserve(map->size());
    for (auto& [id, op] : *map) {
      snapshot.emplace_back(id, op);
    }
  }

  std::vector<std::string> toReap;
  for (auto& [id, op] : snapshot) {
    bool reap = false;
    if (auto ct = op->completionTime.rlock(); ct->has_value()) {
      reap = (now - **ct > maxAge);
    } else {
      auto ageSec =
          std::chrono::duration_cast<std::chrono::seconds>(now - op->startTime)
              .count();
      auto progress = op->processed.load(std::memory_order_relaxed) +
          op->prefetchProcessed.load(std::memory_order_relaxed);
      if (progress != op->lastSweepProgress.load(std::memory_order_relaxed)) {
        op->lastSweepProgress.store(progress, std::memory_order_relaxed);
        op->lastProgressAgeSec.store(ageSec, std::memory_order_relaxed);
      } else if (
          ageSec - op->lastProgressAgeSec.load(std::memory_order_relaxed) >
          kMaxStrandedLifetimeSec) {
        reap = true;
        XLOGF(
            WARN,
            "Reaping stalled preload operation {}: no progress in over {} "
            "minutes (preload {}/{}, prefetch {}/{}); progress queries for "
            "this id will now report not-found",
            id,
            std::chrono::duration_cast<std::chrono::minutes>(
                kMaxStrandedLifetime)
                .count(),
            op->processed.load(std::memory_order_relaxed),
            op->total.load(std::memory_order_relaxed),
            op->prefetchProcessed.load(std::memory_order_relaxed),
            op->prefetchTotal.load(std::memory_order_relaxed));
      }
    }
    if (reap) {
      toReap.push_back(std::move(id));
    }
  }

  if (!toReap.empty()) {
    auto map = preloadProgressMap_.wlock();
    for (auto& id : toReap) {
      map->erase(id);
    }
  }
}

folly::ReadMostlySharedPtr<const EdenConfig> ServerState::getEdenConfig() {
  return config_->getEdenConfig();
}

std::unique_ptr<TopLevelIgnores> ServerState::getTopLevelIgnores() {
  // Update EdenConfig to detect changes to the system or user ignore files
  auto edenConfig = getEdenConfig();

  // Get the potentially changed system/user ignore files
  auto userIgnoreFile = edenConfig->userIgnoreFile.getValue();
  auto systemIgnoreFile = edenConfig->systemIgnoreFile.getValue();

  // Get the userIgnoreFile
  GitIgnore userGitIgnore{};
  auto fcResult =
      userIgnoreFileMonitor_.wlock()->getFileContents(userIgnoreFile);
  if (fcResult.hasValue()) {
    userGitIgnore = fcResult.value();
  }

  // Get the systemIgnoreFile
  GitIgnore systemGitIgnore{};
  fcResult =
      systemIgnoreFileMonitor_.wlock()->getFileContents(systemIgnoreFile);
  if (fcResult.hasValue()) {
    systemGitIgnore = fcResult.value();
  }
  return std::make_unique<TopLevelIgnores>(
      std::move(userGitIgnore), std::move(systemGitIgnore));
}

} // namespace facebook::eden
