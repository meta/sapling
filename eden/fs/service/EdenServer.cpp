/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/service/EdenServer.h"

#include <cpptoml.h>
#include <algorithm>
#include <atomic>
#include <chrono>

#include <sys/stat.h>
#include <fstream>
#include <functional>
#include <iterator>
#include <memory>
#include <optional>
#include <sstream>
#include <string>
#include <string_view>
#include <unordered_set>
#include <vector>

#include "eden/fs/service/HeartbeatManager.h"

#include <fb303/ServiceData.h>
#include <fmt/core.h>
#include <folly/Exception.h>
#include <folly/ExceptionWrapper.h>
#include <folly/FileUtil.h>
#include <folly/Indestructible.h>
#include <folly/ScopeGuard.h>
#include <folly/SocketAddress.h>
#include <folly/String.h>
#include <folly/portability/Unistd.h>
#include <thrift/lib/cpp/util/EnumUtils.h>

#include <folly/json/json.h>
#ifdef __APPLE__
#include <folly/Subprocess.h> // @manual
#endif
#ifdef __linux__
#include <folly/container/F14Map.h>
#include <sys/sysmacros.h>
#include "eden/common/utils/SpawnedProcess.h"
#include "eden/fs/service/CgroupFileCacheReclaimer.h"
#include "eden/fs/utils/MountInfoTable.h"
#endif
#include <folly/chrono/Conv.h>
#include <folly/coro/Invoke.h>
#include <folly/coro/Sleep.h>
#include <folly/coro/Task.h>
#include <folly/coro/WithCancellation.h>
#include <folly/executors/CPUThreadPoolExecutor.h>
#include <folly/executors/thread_factory/NamedThreadFactory.h>
#include <folly/futures/Future.h>
#include <folly/io/async/AsyncSignalHandler.h>
#include <folly/io/async/EventBaseManager.h>
#include <folly/io/async/HHWheelTimer.h>
#include <folly/logging/xlog.h>
#include <folly/portability/SysTypes.h>
#include <folly/stop_watch.h>
#include <gflags/gflags.h>
#include <thrift/lib/cpp/concurrency/ThreadManager.h>
#include <thrift/lib/cpp2/async/ServerPublisherStream.h>
#include <thrift/lib/cpp2/async/ServerStream.h>
#include <thrift/lib/cpp2/server/ParallelConcurrencyController.h>
#include <thrift/lib/cpp2/server/RoundRobinRequestPile.h>
#include <thrift/lib/cpp2/server/ThriftProcessor.h>
#include <thrift/lib/cpp2/server/ThriftServer.h>

#include <csignal>

#include "eden/common/telemetry/RequestMetricsScope.h"
#include "eden/common/telemetry/SessionInfo.h"
#include "eden/common/telemetry/StructuredLoggerFactory.h"
#include "eden/common/utils/Bug.h"
#include "eden/common/utils/EnumValue.h"
#include "eden/common/utils/FaultInjector.h"
#include "eden/common/utils/FileUtils.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/ProcessInfoCache.h"
#include "eden/common/utils/TimeUtil.h"
#include "eden/common/utils/UnboundedQueueExecutor.h"
#include "eden/common/utils/UserInfo.h"
#include "eden/fs/config/CheckoutConfig.h"
#include "eden/fs/config/MountProtocol.h"
#include "eden/fs/config/RestrictedContentMode.h"
#include "eden/fs/config/TomlConfig.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/InodeAccessLogger.h"
#include "eden/fs/inodes/InodeBase.h"
#include "eden/fs/inodes/InodeMap.h"
#include "eden/fs/inodes/InodeTimestamps.h"
#include "eden/fs/inodes/ServerState.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/journal/Journal.h"
#include "eden/fs/nfs/NfsServer.h"
#include "eden/fs/notifications/NullNotifier.h"
#include "eden/fs/privhelper/PinScan.h"
#include "eden/fs/privhelper/PrivHelper.h"
#include "eden/fs/privhelper/PrivHelperImpl.h"
#include "eden/fs/service/EdenCPUThreadPool.h"
#include "eden/fs/service/EdenServiceHandler.h"
#include "eden/fs/service/PinScanRunner.h"
#include "eden/fs/service/StartupLogger.h"
#include "eden/fs/service/StartupStatusSubscriber.h"
#include "eden/fs/service/ThriftStreamStartupStatusSubscriber.h"
#include "eden/fs/service/ThriftUtil.h"
#include "eden/fs/service/gen-cpp2/eden_types.h"
#include "eden/fs/store/BlobCache.h"
#include "eden/fs/store/ObjectStore.h"
#include "eden/fs/store/TreeCache.h"
#include "eden/fs/store/sl/SaplingBackingStore.h"
#include "eden/fs/takeover/TakeoverData.h"
#include "eden/fs/telemetry/EdenErrorInfoBuilder.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/telemetry/EdenStructuredLogger.h"
#include "eden/fs/telemetry/ErrorLogger.h"
#include "eden/fs/telemetry/IXplatLogger.h"
#include "eden/fs/telemetry/LogEvent.h"
#ifdef EDEN_HAVE_LOGGER
#include "eden/fs/telemetry/facebook/XplatLogger.h" // @manual
#include "eden/fs/telemetry/facebook/XplatTransforms.h" // @manual
#endif
#include "eden/fs/telemetry/XplatKeys.h"
#include "eden/fs/utils/Clock.h"
#include "eden/fs/utils/EdenError.h"
#include "eden/fs/utils/FsChannelTypes.h"
#include "eden/fs/utils/MountHealthCheck.h"
#include "eden/fs/utils/NfsSocket.h"
#include "eden/fs/utils/NotImplemented.h"
#include "eden/fs/utils/ProcUtil.h"

#ifdef EDEN_HAVE_PROCESS_ATTRIBUTION
#include "eden/common/utils/facebook/ProcessAttribution.h" // @manual
#endif

#ifdef EDEN_HAVE_SERVER_OBSERVER
#include "common/fb303/cpp/ThreadPoolExecutorCounters.h" // @manual
#include "eden/fs/service/facebook/ServerObserver.h" // @manual
#endif

#ifndef _WIN32
#include <sys/wait.h>
#include "eden/fs/fuse/FuseChannel.h"
#include "eden/fs/inodes/Overlay.h"
#include "eden/fs/nfs/Nfsd3.h"
#include "eden/fs/notifications/CommandNotifier.h"
#include "eden/fs/takeover/TakeoverClient.h"
#include "eden/fs/takeover/TakeoverServer.h"
#endif

#ifdef _WIN32
#include "eden/fs/notifications/WindowsNotifier.h" // @manual
#endif // !_WIN32

DEFINE_bool(
    takeover,
    false,
    "If another edenfs process is already running, "
    "attempt to gracefully takeover its mount points.");
DEFINE_bool(
    forceTakeover,
    false,
    "With --takeover, make the running edenfs cancel the Thrift requests it "
    "is serving instead of refusing the takeover while it is busy.");
DEFINE_bool(
    takeoverOnlyIfIdle,
    false,
    "With --takeover, let the running edenfs refuse the takeover while it has "
    "served a checkout or a mount recently, as well as while it is busy.");
DEFINE_bool(
    enable_fault_injection,
    false,
    "Enable the fault injection framework.");

// Still passed by antlir's eden_access tooling; has had no effect since the
// LocalStore was removed.
DEFINE_string(local_storage_engine_unsafe, "", "DEPRECATED - has no effect.");

DEFINE_int64(
    start_delay_minutes,
    10,
    "Initial delay before first background inode unload");
DEFINE_int64(
    unload_age_minutes,
    6 * 60,
    "Minimum age of the inodes to be unloaded in background");

using apache::thrift::ThriftServer;
using folly::Future;
using folly::makeFuture;
using folly::makeFutureWith;
using folly::StringPiece;
using folly::Unit;
using std::make_shared;
using std::optional;
using std::shared_ptr;
using std::string;
using namespace std::chrono_literals;

namespace {

using namespace facebook::eden;

/**
 * Run fn on evb and wait for it, or run it inline when already on evb. The
 * EventBase runs fn inside a noexcept task, so an exception from fn would end
 * the process; this rethrows it to the caller instead. The wait only returns
 * once fn has run: an EventBase that is being destroyed drains its queue by
 * running what is left in it.
 */
template <typename Fn>
void runInEventBaseThreadAndRethrow(folly::EventBase* evb, Fn&& fn) {
  folly::exception_wrapper error;
  evb->runImmediatelyOrRunInEventBaseThreadAndWait([&] {
    try {
      fn();
    } catch (...) {
      error = folly::exception_wrapper{std::current_exception()};
    }
  });
  if (error) {
    error.throw_exception();
  }
}

constexpr auto kMountHealthCheckTimeout = std::chrono::seconds{5};
constexpr auto kChannelStopRecoveryDelay = std::chrono::milliseconds{500};

// `source` of the eden_mount_health_issue row that reports a recovery's
// outcome.
constexpr std::string_view recoveryOutcomeSource(
    CheckoutRecoveryTrigger trigger) {
  switch (trigger) {
    case CheckoutRecoveryTrigger::HealthCheckTick:
      return "daemon_periodic";
    case CheckoutRecoveryTrigger::ChannelStop:
      return "daemon_channel_stop";
  }
  return "unknown";
}

using EmittedMountHealthIssues =
    folly::Synchronized<std::unordered_map<std::string, std::set<std::string>>>;

bool shouldLogMountHealthIssue(
    const std::shared_ptr<EmittedMountHealthIssues>& emittedMountHealthIssues,
    const std::string& mountPath,
    const std::string& reason) {
  auto emittedIssues = emittedMountHealthIssues->wlock();
  auto& emittedReasons = (*emittedIssues)[mountPath];
  return emittedReasons.insert(reason).second;
}

void clearMountHealthIssues(
    const std::shared_ptr<EmittedMountHealthIssues>& emittedMountHealthIssues,
    const std::string& mountPath) {
  auto emittedIssues = emittedMountHealthIssues->wlock();
  emittedIssues->erase(mountPath);
}

void pruneMountHealthIssues(
    const std::shared_ptr<EmittedMountHealthIssues>& emittedMountHealthIssues,
    const std::unordered_set<std::string>& retainedMountPaths) {
  auto emittedIssues = emittedMountHealthIssues->wlock();
  for (auto it = emittedIssues->begin(); it != emittedIssues->end();) {
    if (retainedMountPaths.find(it->first) == retainedMountPaths.end()) {
      it = emittedIssues->erase(it);
    } else {
      ++it;
    }
  }
}

struct MountHealthIssueLogContext {
  MountHealthIssueLogContext(
      std::weak_ptr<EdenFsEventsLogger> edenFsEventsLogger,
      std::shared_ptr<EmittedMountHealthIssues> emittedMountHealthIssues,
      EdenStatsPtr stats,
      std::string mountPath,
      std::string repoSource,
      std::weak_ptr<EdenMount> mount,
      std::shared_ptr<MountHealthProbeProgress> progress)
      : edenFsEventsLogger_{std::move(edenFsEventsLogger)},
        emittedMountHealthIssues{std::move(emittedMountHealthIssues)},
        stats{std::move(stats)},
        mountPath{std::move(mountPath)},
        repoSource{std::move(repoSource)},
        mount{std::move(mount)},
        progress{std::move(progress)} {}

  void count(StatsGroupBase::Counter MountHealthCheckStats::* counter) const {
    stats->increment(counter);
  }

  void log(
      std::string reason,
      std::string error,
      std::string causeDetail = {},
      std::optional<double> durationS = std::nullopt,
      bool deduplicate = true) const {
    if (deduplicate &&
        !shouldLogMountHealthIssue(
            emittedMountHealthIssues, mountPath, reason)) {
      return;
    }
    auto edenFsEventsLogger = edenFsEventsLogger_.lock();
    if (!edenFsEventsLogger) {
      return;
    }
    EdenMountHealthIssue event{
        "daemon_periodic",
        std::move(reason),
        std::string{mountPath},
        std::string{mountPath},
        "configured_checkout",
        std::string{repoSource},
        std::move(error),
        false,
        false};
    event.cause_detail = std::move(causeDetail);
    event.duration_s = durationS;
    edenFsEventsLogger->logEvent(event);
  }

  std::weak_ptr<EdenFsEventsLogger> edenFsEventsLogger_;
  std::shared_ptr<EmittedMountHealthIssues> emittedMountHealthIssues;
  EdenStatsPtr stats;
  std::string mountPath;
  std::string repoSource;
  // Weak, so that a stuck probe never keeps an unmounted checkout alive.
  std::weak_ptr<EdenMount> mount;
  std::shared_ptr<MountHealthProbeProgress> progress;
  // With a hang threshold, the detail of a probe that ran past the 5 s
  // timeout, taken then. Logged if the probe finishes before the threshold.
  mutable folly::Synchronized<std::string> slowProbeDetail;
  // Set once the probe was reported as hung.
  mutable std::atomic_bool hung{false};
};

using RunningMountHealthChecks = folly::Synchronized<std::unordered_map<
    std::string,
    std::shared_ptr<const MountHealthProbeProgress>>>;

/**
 * The probes holding health check threads while another probe waits, as a
 * JSON field.
 */
folly::dynamic describeProbesHoldingThreads(
    const RunningMountHealthChecks& runningChecks,
    const std::string& waitingMountPath,
    std::chrono::steady_clock::time_point now) {
  std::vector<
      std::pair<std::string, std::shared_ptr<const MountHealthProbeProgress>>>
      running;
  {
    auto checks = runningChecks.rlock();
    for (const auto& [mountPath, progress] : *checks) {
      if (mountPath != waitingMountPath && progress->started() &&
          progress->step() != MountHealthProbeStep::Done) {
        running.emplace_back(mountPath, progress);
      }
    }
  }
  // Reads /proc for each probe, so do it without holding the lock.
  folly::dynamic holders = folly::dynamic::array;
  for (const auto& [mountPath, progress] : running) {
    auto holder = describeProbeProgress(*progress, now);
    holder.update(describeProbeThread(*progress));
    holder["mount"] = mountPath;
    holders.push_back(std::move(holder));
  }
  return folly::dynamic::object("holders", std::move(holders));
}

/**
 * The checkout's load, as JSON fields: the requests EdenFS is working on and
 * the oldest one, the requests the kernel has queued, and whether a checkout
 * or an inode GC is running.
 */
folly::dynamic describeFsChannelLoad(
    EdenMount& mount,
    std::chrono::steady_clock::time_point now) {
  folly::dynamic detail = folly::dynamic::object(
      "checkout_in_progress", mount.isCheckoutInProgress());
#ifndef _WIN32
  auto toMs = [](std::chrono::steady_clock::duration duration) {
    return std::chrono::duration_cast<std::chrono::milliseconds>(duration)
        .count();
  };
  auto byStartTime = [](const auto& a, const auto& b) {
    return a.requestStartTime < b.requestStartTime;
  };
  if (auto* fuse = mount.getFuseChannel()) {
    if (auto waiting =
            readFuseConnectionWaiting(mount.getHealthCheckDevice())) {
      detail["fuse_waiting"] = *waiting;
    }
    auto requests = fuse->getOutstandingRequests();
    detail["outstanding"] = requests.size();
    auto oldest =
        std::min_element(requests.begin(), requests.end(), byStartTime);
    if (oldest != requests.end()) {
      detail["oldest_ms"] = toMs(now - oldest->requestStartTime);
      detail["oldest_op"] = fuseOpcodeName(oldest->request.opcode).str();
      detail["oldest_pid"] = oldest->request.pid;
    }
  } else if (auto* nfs = mount.getNfsdChannel()) {
    auto requests = nfs->getOutstandingRequests();
    detail["outstanding"] = requests.size();
    auto oldest =
        std::min_element(requests.begin(), requests.end(), byStartTime);
    if (oldest != requests.end()) {
      detail["oldest_ms"] = toMs(now - oldest->requestStartTime);
    }
  }
#else
  (void)now;
#endif
  if (auto gc = mount.getRunningInodeGC()) {
    detail["gc_kind"] = gc->pressureBased ? "pressure" : "periodic";
    detail["gc_age_ms"] =
        std::chrono::duration_cast<std::chrono::milliseconds>(gc->age).count();
  }
  return detail;
}

folly::CPUThreadPoolExecutor* getMountHealthCheckThreadPool() {
  // Process-lifetime so a stuck filesystem syscall cannot make EdenServer
  // teardown wait for the health-check worker thread.
  static folly::Indestructible<folly::CPUThreadPoolExecutor> executor(
      2, std::make_shared<folly::NamedThreadFactory>("MountHealthCheck"));
  return executor.get();
}

#ifdef __linux__
folly::CPUThreadPoolExecutor* getCgroupFileCacheReclaimThreadPool() {
  // memory.reclaim is synchronous, so keep it off Eden's request executors.
  static folly::Indestructible<folly::CPUThreadPoolExecutor> executor(
      1, std::make_shared<folly::NamedThreadFactory>("CgroupReclaim"));
  return executor.get();
}
#endif

/**
 * The JSON detail of a probe that ran too long: where it is, where its thread
 * waits, the checkout's load and the host's pressure.
 */
std::string describeStuckProbe(
    const MountHealthIssueLogContext& logContext,
    std::chrono::steady_clock::time_point now,
    const folly::dynamic& extraDetail = folly::dynamic::object) {
  auto description = describeProbeProgress(*logContext.progress, now);
  description.update(describeProbeThread(*logContext.progress));
  if (auto mount = logContext.mount.lock()) {
    description.update(describeFsChannelLoad(*mount, now));
  }
  description.update(describeSystemPressure());
  description.update(extraDetail);
  folly::json::serialization_opts sortedKeys;
  sortedKeys.sort_keys = true;
  return folly::json::serialize(description, sortedKeys);
}

void logMountHealthCheckTimeout(
    const MountHealthIssueLogContext& logContext,
    EdenMountHealthIssueReason reason,
    std::string error,
    const folly::dynamic& extraDetail = folly::dynamic::object) {
  try {
    const auto now = std::chrono::steady_clock::now();
    auto detail = describeStuckProbe(logContext, now, extraDetail);
    XLOGF(WARN, "{} for {}: {}", error, logContext.mountPath, detail);
    logContext.log(
        std::string{edenMountHealthIssueReasonString(reason)},
        std::move(error),
        std::move(detail),
        std::chrono::duration<double>{now - logContext.progress->queued()}
            .count());
  } catch (const std::exception& ex) {
    XLOGF(ERR, "EdenFS mount health timeout logging failed: {}", ex.what());
  } catch (...) {
    XLOG(ERR, "EdenFS mount health timeout logging failed");
  }
}

/**
 * Logs a probe that ran past the 5 s timeout and then finished, once per
 * probe rather than once per issue. `detail` was taken at the timeout.
 */
void logSlowMountHealthCheck(
    const MountHealthIssueLogContext& logContext,
    std::string detail) {
  try {
    const auto duration =
        std::chrono::steady_clock::now() - logContext.progress->queued();
    auto error = fmt::format(
        "EdenFS mount health check took {} ms",
        std::chrono::duration_cast<std::chrono::milliseconds>(duration)
            .count());
    XLOGF(WARN, "{} for {}: {}", error, logContext.mountPath, detail);
    logContext.log(
        std::string{edenMountHealthIssueReasonString(
            EdenMountHealthIssueReason::DaemonRunningMountSlow)},
        std::move(error),
        std::move(detail),
        std::chrono::duration<double>{duration}.count(),
        /*deduplicate=*/false);
  } catch (const std::exception& ex) {
    XLOGF(ERR, "EdenFS mount health slow probe logging failed: {}", ex.what());
  }
}

/**
 * Logs the end of a hang, once per hang, with how long the probe ran and what
 * it found.
 */
void logMountHealthCheckHangEnded(
    const MountHealthIssueLogContext& logContext,
    std::string_view result) {
  try {
    const auto duration =
        std::chrono::steady_clock::now() - logContext.progress->queued();
    const auto durationMs =
        std::chrono::duration_cast<std::chrono::milliseconds>(duration).count();
    auto error = fmt::format(
        "EdenFS mount health check hang ended after {} ms", durationMs);
    auto detail = folly::toJson(
        folly::dynamic::object("result", result)("probe_ms", durationMs));
    XLOGF(WARN, "{} for {}: {}", error, logContext.mountPath, detail);
    logContext.log(
        std::string{edenMountHealthIssueReasonString(
            EdenMountHealthIssueReason::DaemonRunningMountHangEnded)},
        std::move(error),
        std::move(detail),
        std::chrono::duration<double>{duration}.count(),
        /*deduplicate=*/false);
  } catch (const std::exception& ex) {
    XLOGF(ERR, "EdenFS mount health hang end logging failed: {}", ex.what());
  }
}

void scheduleMountHealthCheckStartTimeout(
    std::shared_ptr<std::atomic_bool> started,
    std::shared_ptr<std::atomic_bool> completed,
    std::shared_ptr<const MountHealthIssueLogContext> logContext,
    std::shared_ptr<const RunningMountHealthChecks> runningChecks) {
  folly::futures::detachOnGlobalCPUExecutor(
      folly::futures::sleep(kMountHealthCheckTimeout)
          .deferValue([started = std::move(started),
                       completed = std::move(completed),
                       logContext = std::move(logContext),
                       runningChecks = std::move(runningChecks)](auto&&) {
            if (started->load(std::memory_order_acquire)) {
              return;
            }
            if (completed->exchange(true)) {
              return;
            }
            logContext->count(&MountHealthCheckStats::notStarted);
            logMountHealthCheckTimeout(
                *logContext,
                EdenMountHealthIssueReason::DaemonRunningMountCheckNotStarted,
                "EdenFS mount health check did not start within timeout; "
                "worker pool may be saturated",
                describeProbesHoldingThreads(
                    *runningChecks,
                    logContext->mountPath,
                    std::chrono::steady_clock::now()));
          }));
}

void scheduleMountHealthCheckHangTimeout(
    std::shared_ptr<std::atomic_bool> completed,
    std::shared_ptr<const MountHealthIssueLogContext> logContext,
    std::chrono::nanoseconds delay) {
  folly::futures::detachOnGlobalCPUExecutor(
      folly::futures::sleep(
          std::chrono::duration_cast<folly::HighResDuration>(delay))
          .deferValue([completed = std::move(completed),
                       logContext = std::move(logContext)](auto&&) {
            // Set before claiming the probe, so a result that loses the race
            // sees it.
            logContext->hung.store(true, std::memory_order_release);
            if (completed->exchange(true)) {
              return;
            }
            logContext->count(&MountHealthCheckStats::hung);
            logMountHealthCheckTimeout(
                *logContext,
                EdenMountHealthIssueReason::DaemonRunningMountHung,
                "EdenFS mount health check hung");
          }));
}

void scheduleMountHealthCheckRuntimeTimeout(
    std::shared_ptr<std::atomic_bool> completed,
    std::shared_ptr<const MountHealthIssueLogContext> logContext,
    std::chrono::nanoseconds hangThreshold) {
  folly::futures::detachOnGlobalCPUExecutor(
      folly::futures::sleep(kMountHealthCheckTimeout)
          .deferValue([completed = std::move(completed),
                       logContext = std::move(logContext),
                       hangThreshold](auto&&) mutable {
            if (hangThreshold > kMountHealthCheckTimeout) {
              // A slow probe is reported when it finishes, or when it passes
              // the threshold.
              if (!completed->load(std::memory_order_acquire)) {
                try {
                  *logContext->slowProbeDetail.wlock() = describeStuckProbe(
                      *logContext, std::chrono::steady_clock::now());
                } catch (const std::exception& ex) {
                  XLOGF(
                      ERR,
                      "EdenFS mount health check could not describe a slow "
                      "probe: {}",
                      ex.what());
                }
                scheduleMountHealthCheckHangTimeout(
                    std::move(completed),
                    std::move(logContext),
                    hangThreshold - kMountHealthCheckTimeout);
              }
              return;
            }
            if (completed->exchange(true)) {
              return;
            }
            logContext->count(&MountHealthCheckStats::timedOut);
            logMountHealthCheckTimeout(
                *logContext,
                EdenMountHealthIssueReason::DaemonRunningMountTimedOut,
                "EdenFS mount health check timed out");
          }));
}

std::shared_ptr<Notifier> getPlatformNotifier(
    std::shared_ptr<ReloadableConfig> config,
    std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger,
    const std::string& version) {
#if defined(_WIN32)
  /*
   * If the E-Menu is disabled, we should create a Null Notifier
   * that no-ops when EdenFS attempts to send notifications
   * through it.
   */
  if (config->getEdenConfig()->enableEdenMenu.getValue()) {
    /*
     * The startTime we're passing will be slightly different than the actual
     * start time... However, this doesn't matter too much. We will already be
     * showing a slightly incorrect uptime because the E-Menu won't update the
     * uptime until the user re-clicks on the "About EdenFS" menu option
     */
    try {
      auto notifier = std::make_shared<WindowsNotifier>(
          config,
          edenFsEventsLogger,
          version,
          std::chrono::steady_clock::now());
      notifier->initialize();
      return notifier;
    } catch (const std::exception& ex) {
      auto reason = folly::exceptionStr(ex);
      XLOGF(WARN, "Couldn't start E-Menu: {}", reason);
      edenFsEventsLogger->logEvent(EMenuStartupFailure{reason.toStdString()});
    }
  }
  return std::make_shared<NullNotifier>(config);
#else
  (void)version;
  (void)edenFsEventsLogger;
  return std::make_shared<CommandNotifier>(config);
#endif // _WIN32
}

constexpr StringPiece kSlStorePrefix{"store.sapling"};
#ifndef _WIN32
constexpr StringPiece kFuseRequestPrefix{"fuse"};
#endif
#ifdef __APPLE__
constexpr StringPiece kNFSStatPrefix{"nfs"};
#endif
constexpr StringPiece kStateConfig{"config.toml"};

std::optional<std::string> getUnixDomainSocketPath(
    const folly::SocketAddress& address) {
  return AF_UNIX == address.getFamily() ? std::make_optional(address.getPath())
                                        : std::nullopt;
}

#ifdef __APPLE__
folly::Try<folly::dynamic> collectNFSUtilStats() {
  try {
    SpawnedProcess::Options opts;
    opts.pipeStdout();
    opts.pipeStderr();
    auto nfsStatProc =
        SpawnedProcess({"nfsstat", "-f", "JSON"}, std::move(opts));
    std::string nfsStatOutputString = nfsStatProc.communicate().first;
    nfsStatProc.waitOrTerminateOrKill(1s, 1s);
    return folly::Try<folly::dynamic>(folly::parseJson(nfsStatOutputString));
  } catch (const std::exception& e) {
    return folly::Try<folly::dynamic>(e);
  }
}
#endif

std::string getCounterNameForImportMetric(
    RequestMetricsScope::RequestStage stage,
    RequestMetricsScope::RequestMetric metric,
    std::optional<SaplingBackingStore::SaplingImportObject> object =
        std::nullopt) {
  if (object.has_value()) {
    // base prefix . stage . object . metric
    return folly::to<std::string>(
        kSlStorePrefix,
        ".",
        RequestMetricsScope::stringOfHgImportStage(stage),
        ".",
        SaplingBackingStore::stringOfSaplingImportObject(object.value()),
        ".",
        RequestMetricsScope::stringOfRequestMetric(metric));
  }
  // base prefix . stage . metric
  return folly::to<std::string>(
      kSlStorePrefix,
      ".",
      RequestMetricsScope::stringOfHgImportStage(stage),
      ".",
      RequestMetricsScope::stringOfRequestMetric(metric));
}

#ifndef _WIN32
std::string getCounterNameForFuseRequests(
    RequestMetricsScope::RequestStage stage,
    RequestMetricsScope::RequestMetric metric,
    const EdenMount* mount) {
  auto mountName = basename(mount->getPath().view());
  // prefix . mount . stage . metric
  return folly::to<std::string>(
      kFuseRequestPrefix,
      ".",
      mountName,
      ".",
      RequestMetricsScope::stringOfFuseRequestStage(stage),
      ".",
      RequestMetricsScope::stringOfRequestMetric(metric));
}
#endif

#ifdef __linux__
// **not safe to call this function from a fuse thread**
// this gets the kernels view of the number of pending requests, to do this, it
// stats the fuse mount root in the filesystem which could call into the FUSE
// daemon which could cause a deadlock.
size_t getNumberPendingFuseRequests(const EdenMount* mount) {
  constexpr StringPiece kFuseInfoDir{"/sys/fs/fuse/connections"};
  constexpr StringPiece kFusePendingRequestFile{"waiting"};

  auto mount_path = mount->getPath().c_str();
  struct stat file_metadata{};

  folly::checkUnixError(
      lstat(mount_path, &file_metadata),
      "unable to get FUSE device number for mount ",
      basename(mount->getPath().view()));

  auto pending_request_path = folly::to<std::string>(
      kFuseInfoDir,
      kDirSeparator,
      file_metadata.st_dev,
      kDirSeparator,
      kFusePendingRequestFile);

  auto pending_requests = readFile(canonicalPath(pending_request_path));
  return pending_requests.hasValue()
      ? folly::to<size_t>(pending_requests.value())
      : 0;
}
#endif // __linux__

std::shared_ptr<folly::Executor> makeDiffWorkThreads(
    std::shared_ptr<const EdenConfig>& edenConfig) {
  return std::make_shared<UnboundedQueueExecutor>(
      edenConfig->numDiffWorkThreads.getValue(), "DiffWorkThreadPool");
}

std::shared_ptr<folly::Executor> makePrefetchFilesV2Threads(
    bool usePrefetchExecutor,
    std::shared_ptr<const EdenConfig>& edenConfig) {
  if (usePrefetchExecutor) {
    return std::make_shared<UnboundedQueueExecutor>(
        edenConfig->numPrefetchThreads.getValue(), "PrefetchFilesV2ThreadPool");
  }
  return nullptr;
}

#ifndef _WIN32
bool shouldRunPeriodicInodeUnload(const EdenConfig& config) {
  return !config.enablePressureBasedGc.getValue() &&
      config.periodicUnloadIntervalMinutes.getValue() > 0;
}
#endif

} // namespace

namespace facebook::eden {

namespace {

ProcessInfoCache::ReadFuncConfig makeProcessInfoReadConfig(
    [[maybe_unused]] const EdenConfig& edenConfig) {
  ProcessInfoCache::ReadFuncConfig config;
#ifdef EDEN_HAVE_PROCESS_ATTRIBUTION
  if (edenConfig.attributeClientProcesses.getValue()) {
    config.attribution = attributeProcessToAgent;
  }
#endif
  return config;
}

} // namespace

#ifdef __linux__
class CgroupFileCacheReclaimState {
 public:
  CgroupFileCacheReclaimer reclaimer;
  std::atomic_bool enabled{false};
  std::atomic_bool inFlight{false};
  // Latched once the kernel has shown it lacks the memory.reclaim interface
  // this needs; that cannot change until the next boot.
  std::atomic_bool kernelUnsupported{false};
};
#endif

class EdenServer::ThriftServerEventHandler
    : public apache::thrift::server::TServerEventHandler,
      public folly::AsyncSignalHandler {
 public:
  explicit ThriftServerEventHandler(EdenServer* edenServer)
      : AsyncSignalHandler{nullptr},
        edenServer_{edenServer},
        stoppingFromSignal_{false} {}

  void preServe(const folly::SocketAddress* address) override {
    if (edenServer_->getServerState()
            ->getEdenConfig()
            ->thriftUseCustomPermissionChecking.getValue()) {
      if (auto path = getUnixDomainSocketPath(*address)) {
        folly::checkUnixError(
            chmod(path->c_str(), 0777), "failed to chmod ", *path, " to 777");
      }
    }

    // preServe() will be called from the thrift server thread once when it is
    // about to start serving.
    //
    // Register for SIGINT and SIGTERM.  We do this in preServe() so we can use
    // the thrift server's EventBase to process the signal callbacks.
    auto eventBase = folly::EventBaseManager::get()->getEventBase();
    attachEventBase(eventBase);
    registerSignalHandler(SIGINT);
    registerSignalHandler(SIGTERM);
#ifndef _WIN32
    registerSignalHandler(SIGCHLD);
#endif
    runningPromise_.setValue();
  }

  void signalReceived(int sig) noexcept override {
    switch (sig) {
#ifndef _WIN32
      case SIGCHLD:
        XLOG(DBG4, "got SIGCHLD");
        {
          // Clean up zombie processes (ex. `sl debugrefreshconfig`).
          int status;
          pid_t pid;
          while ((pid = waitpid(-1, &status, WNOHANG)) > 0) {
            XLOGF(DBG5, "waited pid {} status {}", pid, status);
          }
        }
        return;
#endif
      case SIGINT:
      case SIGTERM:
        if (stoppingFromSignal_) {
          XLOGF(INFO, "already stopping due to signal {}", sig);
        } else {
          stoppingFromSignal_ = true;

          XLOGF(INFO, "stopping due to signal {}", sig);

          auto shutdownTimeout = edenServer_->getServerState()
                                     ->getEdenConfig()
                                     ->sigtermShutdownTimeout.getValue();

          if (sig == SIGINT || shutdownTimeout.count() == 0) {
            // Unregister signal handler if either:
            //   - sig is SIGINT (this way, the process exits immediately when
            //   you mash ctrl-c).
            //   - shutdownTimeout is disabled.
            //
            // Unregistering the signal handler causes us to exit immediately
            // via default signal handling the next time we get the signal.
            XLOGF(INFO, "unregistering signal handler for signal {}", sig);
            unregisterSignalHandler(sig);
          } else {
            // Otherwise we are using shutdownTimeout.

            // Schedule cancellation of active requests, leave some time for
            // requests to complete.
            auto cancellationOffset =
                edenServer_->getServerState()
                    ->getEdenConfig()
                    ->cancellationOffsetBeforeShutdown.getValue();
            auto cancelDelay = shutdownTimeout - cancellationOffset;
            auto delayMs = std::max(
                std::chrono::milliseconds(0),
                std::chrono::duration_cast<std::chrono::milliseconds>(
                    cancelDelay));

            XLOGF(
                DBG3,
                "Scheduling cancellation in {} milliseconds",
                delayMs.count());
            edenServer_->getMainEventBase()->runAfterDelay(
                [handler = edenServer_->getHandler()] {
                  XLOG(DBG3, "Cancelling all active requests before timeout");
                  handler->cancelAllActiveRequests(
                      "Shutdown timeout approaching");
                },
                static_cast<uint32_t>(delayMs.count()));

            // Schedule a call to _exit() after the timeout.
            XLOGF(
                INFO,
                "scheduling _exit() if not done shutting down in {}",
                durationToString(shutdownTimeout));
            edenServer_->getMainEventBase()->runAfterDelay(
                [sig, shutdownTimeout] {
                  XLOGF(
                      INFO,
                      "_exit()ing after {} timeout on signal {}",
                      durationToString(shutdownTimeout),
                      sig);
                  folly::LoggerDB::get().flushAllHandlers();
                  _exit(1);
                },
                std::chrono::duration_cast<std::chrono::milliseconds>(
                    shutdownTimeout)
                    .count());
          }

#ifndef _WIN32
          // Record the signal that caused daemon to
          // stop. The next edenFS startup record this
          // signal in the silent daemon exit logger
          edenServer_->createDaemonExitSignalFile(sig);
#endif
          // Stop the server.
          edenServer_->stop();
        }
    }
  }

  /**
   * Return a Future that will be fulfilled once the thrift server is bound to
   * its socket and is ready to accept connections.
   */
  folly::SemiFuture<Unit> getThriftRunningFuture() {
    return runningPromise_.getSemiFuture();
  }

 private:
  EdenServer* edenServer_{nullptr};
  folly::Promise<Unit> runningPromise_;
  bool stoppingFromSignal_;
};

static constexpr folly::StringPiece kFsChannelTaskCount{"fs.task.count"};
static constexpr folly::StringPiece kEdenBugCount{"bug.count"};
static constexpr folly::StringPiece kMemoryVmRssBytes{"memory_vm_rss_bytes"};
#ifdef __APPLE__
static constexpr folly::StringPiece kMemoryCompressedBytes{
    "memory_compressed_bytes"};
static constexpr folly::StringPiece kMemoryFootprintBytes{
    "memory_footprint_bytes"};
#endif

EdenServer::EdenServer(
    std::vector<std::string> originalCommandLine,
    UserInfo userInfo,
    EdenStatsPtr edenStats,
    SessionInfo sessionInfo,
    std::unique_ptr<PrivHelper> privHelper,
    std::shared_ptr<const EdenConfig> edenConfig,
    ActivityRecorderFactory activityRecorderFactory,
    BackingStoreFactory* backingStoreFactory,
    std::shared_ptr<StartupStatusChannel> startupStatusChannel,
    std::string version)
    : originalCommandLine_{std::move(originalCommandLine)},
      edenDir_{edenConfig->edenDir.getValue()},
      activityRecorderFactory_{std::move(activityRecorderFactory)},
      backingStoreFactory_{backingStoreFactory},
      config_{std::make_shared<ReloadableConfig>(edenConfig)},
      mountPoints_{std::make_shared<folly::Synchronized<MountMap>>(
          MountMap{kPathMapDefaultCaseSensitive})},
      emittedMountHealthIssues_{std::make_shared<EmittedMountHealthIssues>()},
      // Store a pointer to the EventBase that will be used to drive
      // the main thread.  The runServer() code will end up driving this
      // EventBase.
      mainEventBase_{folly::EventBaseManager::get()->getEventBase()},
      structuredLogger_{
          makeDefaultStructuredLogger<EdenStructuredLogger, EdenStatsPtr>(
              edenConfig->scribeLogger.getValue(),
              edenConfig->scribeCategory.getValue(),
              sessionInfo,
              edenStats.copy())},
      notificationsStructuredLogger_{
          makeDefaultStructuredLogger<EdenStructuredLogger, EdenStatsPtr>(
              edenConfig->scribeLogger.getValue(),
              edenConfig->notificationsScribeCategory.getValue(),
              sessionInfo,
              edenStats.copy())},
#ifdef EDEN_HAVE_LOGGER
      xplatLogger_{std::make_shared<XplatLogger>(
          EdenTelemetryIdentity::fromSessionInfo(sessionInfo),
          edenStats.copy(),
          config_)},
#endif
      errorLogger_{std::make_shared<ErrorLogger>(
          config_,
#ifdef EDEN_HAVE_LOGGER
          xplatLogger_.get(),
#else
          nullptr,
#endif
          edenStats.copy())},
      edenFsEventsLogger_{std::make_shared<EdenFsEventsLogger>(
#ifdef EDEN_HAVE_LOGGER
          xplatLogger_
#else
          nullptr
#endif
          )},
      serverState_{make_shared<ServerState>(
          std::move(userInfo),
          std::move(edenStats),
          std::move(sessionInfo),
          std::move(privHelper),
          std::make_shared<EdenCPUThreadPool>(
              edenConfig->edenCpuPoolNumThreads.getValue()),
          std::make_shared<UnboundedQueueExecutor>(
              edenConfig->numFsChannelThreads.getValue(),
              "FsChannelThreadPool"),
          std::make_shared<UnixClock>(),
          std::make_shared<ProcessInfoCache>(
              makeProcessInfoReadConfig(*edenConfig)),
          structuredLogger_,
          notificationsStructuredLogger_,
          errorLogger_,
          config_,
          *edenConfig,
          nfsEventBaseThread_.getEventBase(),
          getPlatformNotifier(config_, edenFsEventsLogger_, version),
          FLAGS_enable_fault_injection,
          nullptr, // inodeAccessLogger — use default
#ifdef EDEN_HAVE_LOGGER
          [this]() {
            registerXplatTransforms();
            return xplatLogger_;
          }()
#else
          nullptr
#endif
              )},
      restartArmer_{serverState_->getPrivHelper(), config_, edenDir_},
#ifdef __linux__
      cgroupFileCacheReclaimState_{
          std::make_shared<CgroupFileCacheReclaimState>()},
#endif
      heartbeatManager_{std::make_shared<HeartbeatManager>(
          edenDir_,
          serverState_->getEdenFsEventsLogger())},
      blobCache_{BlobCache::create(
          serverState_->getReloadableConfig(),
          serverState_->getStats().copy())},
      treeCache_{TreeCache::create(
          serverState_->getReloadableConfig(),
          serverState_->getStats().copy())},
      version_{std::move(version)},
      thriftUseResourcePools_{edenConfig->thriftUseResourcePools.getValue()},
      thriftUseSerialExecution_{
          edenConfig->thriftUseSerialExecution.getValue()},
      diffWorkExecutor_{makeDiffWorkThreads(edenConfig)},
      thriftUsePrefetchExecutor_{
          edenConfig->thriftUsePrefetchExecutor.getValue()},
      prefetchFilesV2Executor_{
          makePrefetchFilesV2Threads(thriftUsePrefetchExecutor_, edenConfig)},
      runningMountHealthChecks_{std::make_shared<
          std::decay_t<decltype(*runningMountHealthChecks_)>>()},
      progressManager_{
          std::make_unique<folly::Synchronized<EdenServer::ProgressManager>>()},
      startupStatusChannel_{std::move(startupStatusChannel)},
      lastPressureBasedGcTimes_{kPathMapDefaultCaseSensitive} {
  // The Rust tracing sink lives outside EdenConfig, so hand it the switch
  // here and again on every reload.
  SaplingBackingStore::setScribeLoggingEnabled(
      edenConfig->enableScribeLogging.getValue());

  auto counters = fb303::ServiceData::get()->getDynamicCounters();

  registerInodePopulationReportsCallback();

  for (auto stage : RequestMetricsScope::requestStages) {
    for (auto metric : RequestMetricsScope::requestMetrics) {
      for (auto object : SaplingBackingStore::saplingImportObjects) {
        auto counterName = getCounterNameForImportMetric(stage, metric, object);
        counters->registerCallback(counterName, [this, stage, object, metric] {
          auto individual_counters = this->collectSaplingBackingStoreCounters(
              [stage, object, metric](const SaplingBackingStore& store) {
                return store.getImportMetric(stage, object, metric);
              });
          return RequestMetricsScope::aggregateMetricCounters(
              metric, individual_counters);
        });
      }
      auto summaryCounterName = getCounterNameForImportMetric(stage, metric);
      counters->registerCallback(summaryCounterName, [this, stage, metric] {
        std::vector<size_t> individual_counters;
        for (auto object : SaplingBackingStore::saplingImportObjects) {
          auto more_counters = this->collectSaplingBackingStoreCounters(
              [stage, object, metric](const SaplingBackingStore& store) {
                return store.getImportMetric(stage, object, metric);
              });
          individual_counters.insert(
              individual_counters.end(),
              more_counters.begin(),
              more_counters.end());
        }
        return RequestMetricsScope::aggregateMetricCounters(
            metric, individual_counters);
      });
    }
  }

  counters->registerCallback(kFsChannelTaskCount, [this] {
    return this->getServerState()->getFsChannelThreadPool()->getTaskQueueSize();
  });

  counters->registerCallback(
      kEdenBugCount, [] { return static_cast<size_t>(getEdenBugCount()); });

  counters->registerCallback(kMemoryVmRssBytes, [] {
    auto memoryStats = facebook::eden::proc_util::readMemoryStats();
    if (memoryStats) {
      return memoryStats->resident;
    } else {
      return (size_t)0;
    }
  });

#ifdef __APPLE__
  // On macOS, export extra memory counters
  counters->registerCallback(kMemoryCompressedBytes, [] {
    auto memoryStats = facebook::eden::proc_util::readMemoryStats();
    if (memoryStats && memoryStats->compressed) {
      return memoryStats->compressed.value();
    } else {
      return (size_t)0;
    }
  });

  counters->registerCallback(kMemoryFootprintBytes, [] {
    auto memoryStats = facebook::eden::proc_util::readMemoryStats();
    if (memoryStats && memoryStats->footprint) {
      return memoryStats->footprint.value();
    } else {
      return (size_t)0;
    }
  });

  // On macOS, export the NFS clients/servers counters
  if (config_->getEdenConfig()->updateNFSStatsInterval.getValue() > 0ms) {
    auto result = collectNFSUtilStats();
    if (result.hasValue()) {
      for (const auto& nfsStatsCounter : kNfsStatsToEdenStatsMap_) {
        auto counterName = mapCounterNameForNFSStat(nfsStatsCounter);
        if (counterName.has_value()) {
          counters->registerCallback(
              counterName.value(), [this, nfsStatsCounter] {
                auto result =
                    this->getNFSStatCounterValue(nfsStatsCounter.first);
                if (result.has_value()) {
                  return result.value();
                } else {
                  return 0LL;
                }
              });
        } else {
          // This is not an error, just log it and continue.
          // This only happen on registration time, not during runtime per
          // counter. It notify us that we have a NFS counter in the map that
          // is not reported by Apple.
          XLOGF(
              DFATAL,
              "macOS doesn't report any stat for: {}",
              nfsStatsCounter.first);
        }
      }
    } else {
      auto error = result.exception();
      // This is not a fatal error, just log it and continue.
      // This only happen on registration time, not during runtime per
      // counter.
      XLOGF(
          ERR,
          "Failed to collect NFS clients/servers counters: {}",
          error.what());
    }
  }
#endif
}

#ifdef EDEN_HAVE_LOGGER
void EdenServer::registerXplatTransforms() {
  if (!xplatLogger_) {
    return;
  }
  // Register XplatLogger transforms for all EdenFS Scuba tables.
  // This must happen before any logging call sites fire.
  // Add new registerTransform() calls here as new tables are onboarded.
  //
  // Bind the shared telemetry identity into each transform via a capturing
  // lambda so the generic core stays identity-agnostic.
  const auto& identity = xplatLogger_->getTelemetryIdentity();
  const auto periodicUsernameProvider =
      xplatLogger_->getPeriodicUsernameProvider();
  xplatLogger_->registerTransform(
      std::string{xplat_keys::kFileAccessCategory},
      "GeneratedEdenfsFileAccessesLoggerConfig",
      [identity](const DynamicEvent& event) {
        return fileAccessTransform(identity, event);
      });
  xplatLogger_->registerTransform(
      std::string{xplat_keys::kEventsCategory},
      "GeneratedEdenfsEventsLoggerConfig",
      [identity, periodicUsernameProvider](const DynamicEvent& event) {
        return edenfsEventsTransform(
            identity, event, periodicUsernameProvider.get());
      });
  xplatLogger_->registerTransform(
      std::string{xplat_keys::kErrorsCategory},
      "GeneratedEdenfsErrorsLoggerConfig",
      [identity](const DynamicEvent& event) {
        return errorsTransform(identity, event);
      });
}
#endif

EdenServer::~EdenServer() {
  // Each Thrift connection's processor holds the ServerState through its
  // ThriftPermissionChecker, and the Thrift server drops its connections only
  // when it is destroyed. Doing that first keeps everything the ServerState
  // tears down on its way out, such as the NFS event base, alive until the
  // last connection has let go.
  server_.reset();
#ifdef __linux__
  cgroupFileCacheReclaimState_->enabled.store(false, std::memory_order_release);
#endif

  auto counters = fb303::ServiceData::get()->getDynamicCounters();

  unregisterInodePopulationReportsCallback();

  for (auto stage : RequestMetricsScope::requestStages) {
    for (auto metric : RequestMetricsScope::requestMetrics) {
      for (auto object : SaplingBackingStore::saplingImportObjects) {
        auto counterName = getCounterNameForImportMetric(stage, metric, object);
        counters->unregisterCallback(counterName);
      }
      auto summaryCounterName = getCounterNameForImportMetric(stage, metric);
      counters->unregisterCallback(summaryCounterName);
    }
  }
  counters->unregisterCallback(kFsChannelTaskCount);
  counters->unregisterCallback(kEdenBugCount);
#ifdef __APPLE__
  counters->unregisterCallback(kMemoryCompressedBytes);
  counters->unregisterCallback(kMemoryFootprintBytes);

  for (const auto& nfsStatsCounter : kNfsStatsToEdenStatsMap_) {
    auto counterName = mapCounterNameForNFSStat(nfsStatsCounter);
    if (counterName.has_value()) {
      counters->unregisterCallback(counterName.value());
    }
  }
#endif
}

void EdenServer::setStartupProgressCallback(StartupProgressCallback callback) {
  progressManager_->wlock()->startupProgressCallback = std::move(callback);
}

#ifdef __APPLE__
std::optional<std::string> EdenServer::mapCounterNameForNFSStat(
    std::pair<std::string, std::string> nfsStatsCounter) {
  auto result = this->getNFSStatCounterValue(nfsStatsCounter.first);
  if (result.has_value()) {
    return fmt::format(
        fmt::runtime(kNFSStatPrefix.str() + ".{}"),
        nfsStatsCounter.second); // e.g. "nfs.requests"
  } else {
    return std::nullopt;
  }
}

std::optional<long long> EdenServer::getNFSStatCounterValue(
    std::string nfsStatsCounterMacOSName) {
  if (!this->updateNFSStatsIfNeeded()) {
    // Unable to collect NFS stats
    return std::nullopt;
  }
  try {
    std::vector<std::string> tokens;
    folly::split('.', nfsStatsCounterMacOSName, tokens);
    return nfsStatOutput_[tokens[0]][tokens[1]][tokens[2]].asInt();
  } catch (const std::exception&) {
    return std::nullopt;
  }
}

bool EdenServer::updateNFSStatsIfNeeded() {
  auto now = std::chrono::steady_clock::now();
  auto last = lastTimeUpdatedNfsStat_.wlock();
  if (now >=
      *last + config_->getEdenConfig()->updateNFSStatsInterval.getValue()) {
    auto result = collectNFSUtilStats();
    *last = std::chrono::steady_clock::now();
    if (!result.hasValue()) {
      // Unable to collect NFS stats
      return false;
    }
    nfsStatOutput_ = result.value();
  }
  return true;
}
#endif

namespace cursor_helper {

// https://vt100.net/docs/vt510-rm/CPL.html
// The cursor is moved to the start of the nth preceding line
std::string move_cursor_up(size_t n) {
  return fmt::format("\x1b\x5b{}F", n);
}

// https://vt100.net/docs/vt510-rm/ED.html
// Erases characters from the cursor through to the end of the display
std::string clear_to_bottom() {
  return "\x1b\x5bJ";
}
} // namespace cursor_helper

std::string_view describeFsckProgressPhase(
    OverlayChecker::Progress::Phase phase) {
  switch (phase) {
    case OverlayChecker::Progress::Phase::WaitingForFsckSlot:
      return "waiting for fsck slot";
    case OverlayChecker::Progress::Phase::FsckSlotAcquired:
      return "acquired fsck slot";
    case OverlayChecker::Progress::Phase::RecoveringWal:
      return "recovering WAL files";
    case OverlayChecker::Progress::Phase::Scanning:
      return "scanning";
    case OverlayChecker::Progress::Phase::ScanComplete:
      return "finished scan";
    case OverlayChecker::Progress::Phase::Repairing:
      return "repairing";
    case OverlayChecker::Progress::Phase::Complete:
      return "complete";
  }
  return "unknown";
}

std::string formatFsckProgressStatus(
    OverlayChecker::Progress::Phase phase,
    std::optional<uint16_t> progress10Pct,
    std::string_view localDir) {
  if (progress10Pct.has_value()) {
    return fmt::format(
        "fsck {} {}% on {}",
        describeFsckProgressPhase(phase),
        *progress10Pct * 10,
        localDir);
  }
  return fmt::format(
      "fsck {} on {}", describeFsckProgressPhase(phase), localDir);
}

void EdenServer::ProgressManager::updateProgressState(
    size_t progressIndex,
    const OverlayChecker::Progress& progress) {
  if (progressIndex < totalInProgress) {
    progresses[progressIndex].fsckPhase = progress.phase;
    progresses[progressIndex].fsckProgress10Pct = progress.progress10Pct;
    progresses[progressIndex].fsckStarted = true;
  }
}

void EdenServer::ProgressManager::finishProgress(size_t progressIndex) {
  progresses[progressIndex].mountFinished = true;

  totalFinished++;
  totalInProgress--;
}

void EdenServer::ProgressManager::markFailed(size_t progressIndex) {
  progresses[progressIndex].mountFailed = true;

  totalFailed++;
  totalInProgress--;
}

void EdenServer::ProgressManager::printProgresses(
    const std::shared_ptr<StartupLogger>& logger,
    std::optional<std::string_view> errorMessage) {
  std::string prepare;
  std::string content;

  if (totalLinesPrinted) {
    prepare = cursor_helper::move_cursor_up(totalLinesPrinted);
    totalLinesPrinted = 0;
  }
  prepare += cursor_helper::clear_to_bottom();

  // we intentionally don't include the lines here in totalLinesPrinted so that
  // they won't be erased next time.
  if (errorMessage.has_value()) {
    content += errorMessage.value();
  }

  size_t printedFinished = 0;
  size_t printedFailed = 0;
  size_t printedInProgress = 0;
  for (auto& it : progresses) {
    if (it.mountFinished) {
      content += fmt::format("Successfully remounted {}\n", it.mountPath);
      printedFinished++;
    } else if (it.mountFailed) {
      content += fmt::format("Failed to remount {}\n", it.mountPath);
      printedFailed++;
    } else if (!it.fsckStarted) {
      content += fmt::format("Remounting {}\n", it.mountPath);
      printedInProgress++;
    } else {
      if (it.fsckProgress10Pct.has_value()) {
        content += fmt::format(
            "[{:21}] {:>3}%: fsck {} on {}\n",
            std::string(*it.fsckProgress10Pct * 2, '=') + ">",
            *it.fsckProgress10Pct * 10,
            describeFsckProgressPhase(it.fsckPhase),
            it.localDir);
      } else {
        content += fmt::format(
            "fsck {} on {}\n",
            describeFsckProgressPhase(it.fsckPhase),
            it.localDir);
      }
      printedInProgress++;
    }
    totalLinesPrinted++;
    if (totalLinesPrinted == kMaxProgressLines) {
      content += fmt::format(
          "and {} finished, {} failed, {} in progress...",
          totalFinished - printedFinished,
          totalFailed - printedFailed,
          totalInProgress - printedInProgress);
      break;
    }
  }
  if (errorMessage.has_value()) {
    logger->warn(prepare + content);
  } else {
    logger->logVerbose(prepare + content);
  }
  totalLinesPrinted++;
}

void EdenServer::ProgressManager::manageProgress(
    const std::shared_ptr<StartupLogger>& logger,
    size_t progressIndex,
    const OverlayChecker::Progress& progress) {
  updateProgressState(progressIndex, progress);
  if (progressIndex < progresses.size() && startupProgressCallback) {
    const auto& currentProgress = progresses[progressIndex];
    startupProgressCallback(formatFsckProgressStatus(
        currentProgress.fsckPhase,
        currentProgress.fsckProgress10Pct,
        currentProgress.localDir));
  }
  printProgresses(logger);
}
size_t EdenServer::ProgressManager::registerEntry(
    std::string&& mountPath,
    std::string&& localDir) {
  auto progressIndex = progresses.size();
  progresses.emplace_back(std::move(mountPath), std::move(localDir));
  totalInProgress++;
  return progressIndex;
}

folly::SemiFuture<Unit> EdenServer::unmountAll() {
  std::vector<folly::SemiFuture<Unit>> futures;
  {
    const auto mountPoints = mountPoints_->wlock();
    for (auto& entry : *mountPoints) {
      auto& info = entry.second;

      // Note: capturing the shared_ptr<EdenMount> here in the thenTry() lambda
      // is important to ensure that the EdenMount object cannot be destroyed
      // before EdenMount::unmount() completes.
      auto mount = info.edenMount;
      auto future = mount->unmount({}).defer(
          [mount, unmountFuture = info.unmountPromise.getFuture()](
              auto&& result) mutable {
            if (result.hasValue()) {
              return std::move(unmountFuture);
            } else {
              XLOGF(
                  ERR,
                  "Failed to perform unmount for \"{}\": {}",
                  mount->getPath(),
                  result.exception().what());
              return makeFuture<Unit>(result.exception());
            }
          });
      futures.push_back(std::move(future));
    }
  }
  // Use collectAll() rather than collect() to wait for all of the unmounts
  // to complete, and only check for errors once everything has finished.
  return folly::collectAll(futures).deferValue(
      [](const std::vector<folly::Try<Unit>>& results) {
        for (const auto& result : results) {
          result.throwUnlessValue();
        }
      });
}

#ifndef _WIN32
Future<TakeoverData> EdenServer::stopMountsForTakeover(
    folly::Promise<std::optional<TakeoverData>>&& takeoverPromise) {
  std::vector<Future<optional<TakeoverData::MountInfo>>> futures;
  std::vector<AbsolutePath> mountPaths;
  {
    const auto mountPoints = mountPoints_->wlock();
    for (auto& [mountPath, info] : *mountPoints) {
      try {
        info.takeoverPromise.emplace();
        auto future = info.takeoverPromise->getFuture();

        FsChannel* fsChannel = info.edenMount->getFsChannel();
        if (!fsChannel) {
          return EDEN_BUG_FUTURE(TakeoverData)
              << "Takeover isn't (yet) supported during mount initialization."
              << "Mount State "
              << folly::to_underlying(info.edenMount->getState());
        }

        XLOGF(DBG7, "Calling takeoverStop on {} channel", fsChannel->getName());
        if (fsChannel->takeoverStop()) {
          // Success! Takeover has begun.
        } else {
          return EDEN_BUG_FUTURE(TakeoverData)
              << "Takeover isn't (yet) supported for " << fsChannel->getName()
              << " mounts. Mount state: "
              << folly::to_underlying(info.edenMount->getState());
        }

        futures.emplace_back(
            std::move(future).thenValue(
                [self = this,
                 edenMount = info.edenMount](TakeoverData::MountInfo takeover)
                    -> Future<optional<TakeoverData::MountInfo>> {
                  auto fuseChannelInfo =
                      std::get_if<FuseChannelData>(&takeover.channelInfo);
                  auto nfsChannelInfo =
                      std::get_if<NfsChannelData>(&takeover.channelInfo);
                  if (!fuseChannelInfo && !nfsChannelInfo) {
                    return std::nullopt;
                  }
                  auto& fd = fuseChannelInfo ? fuseChannelInfo->fd
                                             : nfsChannelInfo->nfsdSocketFd;
                  if (!fd) {
                    return std::nullopt;
                  }
                  return self->serverState_->getPrivHelper()
                      ->takeoverShutdown(edenMount->getPath().view())
                      .thenValue(
                          [takeover = std::move(takeover)](auto&&) mutable {
                            return std::move(takeover);
                          });
                }));
      } catch (...) {
        auto ew = folly::exception_wrapper{std::current_exception()};
        auto outcome = ErrorLogOutcome::Disabled;
        ew.with_exception([&](const std::exception& ex) {
          outcome = serverState_->getErrorLogger().log(
              EdenErrorInfo::takeover(ex)
                  .withMountPoint(std::string(mountPath.view()))
                  .withMountStatus(
                      fmt::format(
                          "{}",
                          folly::to_underlying(info.edenMount->getState())))
                  .withErrorType("takeover_shutdown_failed"));
        });
        if (outcome != ErrorLogOutcome::RateLimited) {
          XLOGF(
              ERR,
              "Error while stopping \"{}\" for takeover: {}",
              mountPath,
              ew);
        }
        futures.push_back(
            makeFuture<optional<TakeoverData::MountInfo>>(std::move(ew)));
      }
      mountPaths.emplace_back(mountPath);
    }
  }
  // Use collectAll() rather than collect() to wait for all of the unmounts
  // to complete, and only check for errors once everything has finished.
  // We should not be using .via(&InlineExecutor::instance()) here, this is
  // unsafe and deadlock prone. See eden/fs/docs/Futures.md for more details.
  return folly::collectAll(futures)
      .via(&folly::InlineExecutor::instance())
      .thenValue([takeoverPromise = std::move(takeoverPromise),
                  mountPaths = std::move(mountPaths),
                  serverState = serverState_](
                     std::vector<folly::Try<optional<TakeoverData::MountInfo>>>
                         results) mutable {
        TakeoverData data;
        data.takeoverComplete = std::move(takeoverPromise);
        data.mountPoints.reserve(results.size());
        for (size_t i = 0; i < results.size(); ++i) {
          auto& result = results[i];
          const auto& path = mountPaths[i];
          // If something went wrong shutting down a mount point,
          // log the error but continue trying to perform graceful takeover
          // of the other mount points.
          if (!result.hasValue()) {
            auto outcome = ErrorLogOutcome::Disabled;
            result.exception().with_exception([&](const std::exception& ex) {
              outcome = serverState->getErrorLogger().log(
                  EdenErrorInfo::takeover(
                      ErrorArg::fromExceptionWithoutTrace(ex))
                      .withMountPoint(path.asString())
                      .withErrorType("takeover_shutdown_failed"));
            });
            if (outcome != ErrorLogOutcome::RateLimited) {
              XLOGF(
                  ERR,
                  "error stopping \"{}\" during takeover shutdown: {}",
                  path,
                  result.exception().what());
            }
            continue;
          }

          // result might be a successful Try with an empty Optional.
          // This could happen if the mount point was unmounted while we were
          // in the middle of stopping it for takeover.  Just skip this mount
          // in this case.
          if (!result.value().has_value()) {
            XLOGF(WARN, "\"{}\" was unmounted during takeover shutdown", path);
            continue;
          }

          data.mountPoints.emplace_back(std::move(result.value().value()));
        }
        return data;
      });
}
#endif

void EdenServer::startPeriodicTasks() {
  auto config = serverState_->getReloadableConfig()->getEdenConfig();
  updatePeriodicTaskIntervals(*config);

#ifndef _WIN32
  // Schedule a periodic job to unload unused inodes based on the last access
  // time. currently Eden does not have accurate timestamp tracking for inodes,
  // so using unloadChildrenNow just to validate the behaviour. We will have to
  // modify current unloadChildrenNow function to unload inodes based on the
  // last access time.
  if (shouldRunPeriodicInodeUnload(*config)) {
    scheduleInodeUnload(std::chrono::minutes(FLAGS_start_delay_minutes));
  }
#endif

  backingStoreTask_.updateInterval(1min);
}

void EdenServer::updatePeriodicTaskIntervals(const EdenConfig& config) {
  // Update all periodic tasks whose interval is
  // controlled by EdenConfig settings.

  reloadConfigTask_.updateInterval(
      std::chrono::duration_cast<std::chrono::milliseconds>(
          config.configReloadInterval.getValue()));

  // The checkValidityTask_ isn't really needed on Windows, since the lock file
  // cannot be removed while we are holding it.
#ifndef _WIN32
  checkValidityTask_.updateInterval(
      std::chrono::duration_cast<std::chrono::milliseconds>(
          config.checkValidityInterval.getValue()));
#endif

  // Pressure-based GC ticks frequently so it can react quickly to sudden inode
  // growth. Each tick uses the pressure policy to decide which mounts are due.
  if (config.enableGc.getValue()) {
    if (config.enablePressureBasedGc.getValue()) {
      gcTask_.updateInterval(
          std::chrono::duration_cast<std::chrono::milliseconds>(
              config.pressureBasedGcTickPeriod.getValue()));
    } else {
      gcTask_.updateInterval(
          std::chrono::duration_cast<std::chrono::milliseconds>(
              config.gcPeriod.getValue()));
    }
  } else {
    gcTask_.updateInterval(0s);
  }

#ifdef __linux__
  const auto enableCgroupFileCacheReclaim =
      config.enableCgroupFileCacheReclaim.getValue();
  cgroupFileCacheReclaimState_->enabled.store(
      enableCgroupFileCacheReclaim, std::memory_order_release);
  if (enableCgroupFileCacheReclaim) {
    cgroupFileCacheReclaimTask_.updateInterval(
        std::chrono::duration_cast<std::chrono::milliseconds>(
            config.cgroupFileCacheReclaimInterval.getValue()));
  } else {
    cgroupFileCacheReclaimTask_.updateInterval(0s);
  }
#endif

  refreshRestrictedRootsTask_.updateInterval(
      std::chrono::duration_cast<std::chrono::milliseconds>(
          config.restrictedRootRefreshInterval.getValue()));

  mountHealthCheckTask_.updateInterval(
      std::chrono::duration_cast<std::chrono::milliseconds>(
          config.mountHealthCheckInterval.getValue()));

#ifndef _WIN32
  updateEdenHeartbeatFileTask_.updateInterval(
      std::chrono::duration_cast<std::chrono::milliseconds>(
          config.updateEdenHeartbeatFileInterval.getValue()));
#endif
}

void EdenServer::scheduleCallbackOnMainEventBase(
    std::chrono::milliseconds timeout,
    std::function<void()> fn) {
  // If we don't care about calling fn_ when the callback is canceled, we
  // could just use scheduleTimeoutFn and not have to do a wrapper class.
  // We don't want to run fn_ on cancel because we could be running in the
  // middle of destruction.
  struct Wrapper : folly::HHWheelTimer::Callback {
    explicit Wrapper(std::function<void()> f) : fn_(std::move(f)) {}
    void timeoutExpired() noexcept override {
      // Delete this even under exceptions.
      std::unique_ptr<Wrapper> self{this};

      XLOG(DBG3, "Callback expired, running function");
      try {
        fn_();
      } catch (std::exception const& e) {
        XLOG(
            ERR,
            "HHWheelTimerBase timeout callback threw an exception: {}",
            e.what());
      } catch (...) {
        XLOG(ERR, "HHWheelTimerBase timeout callback threw a non-exception.");
      }
    }
    // The callback will be canceled if the timer is destroyed. Or we use
    // deduplication We don't want to run fn during destruction.
    void callbackCanceled() noexcept override {
      // Delete this even under exceptions.
      std::unique_ptr<Wrapper> self{this};
      XLOG(DBG3, "Callback cancelled, NOT running function");
    }
    std::function<void()> fn_;
  };

  mainEventBase_->runInEventBaseThread(
      [timeout, evb = mainEventBase_, fn = std::move(fn)]() mutable {
        // I hate using raw pointers, but we have to to schedule the callback.
        Wrapper* w = new Wrapper(std::move(fn));
        // Note callback will be run in the mainEventBase_ Thread.
        // TODO: would be nice to deduplicate these function calls. We need the
        // same type of callback to use the same wrapper object ... singletons?
        // The main difference with scheduleTimeoutFn is whether the callback is
        // invoked when the EventBase is destroyed: scheduleTimeoutFn will call
        // it, this function won't. See comment above the wrapper class.
        evb->timer().scheduleTimeout(w, timeout);
      });
}

size_t EdenServer::enumerateInProgressCheckouts() {
  size_t numActive = 0;
  auto mountPoints = mountPoints_->rlock();
  for (auto& entry : *mountPoints) {
    auto& info = entry.second;
    numActive += info.edenMount->isCheckoutInProgress() ? 1 : 0;
  }
  return numActive;
}

#ifndef _WIN32
void EdenServer::unloadInodes() {
  auto config = serverState_->getReloadableConfig()->getEdenConfig();
  if (!shouldRunPeriodicInodeUnload(*config)) {
    XLOG(
        DBG4,
        "Skipping periodic inode unload because it is disabled or pressure-based GC is enabled");
    auto interval = config->periodicUnloadIntervalMinutes.getValue();
    if (interval > 0) {
      scheduleInodeUnload(std::chrono::minutes(interval));
    }
    return;
  }

  auto mounts = getMountPoints();

  if (!mounts.empty()) {
    auto cutoff = std::chrono::system_clock::now() -
        std::chrono::minutes(FLAGS_unload_age_minutes);
    auto cutoff_ts = folly::to<timespec>(cutoff);
    for (auto& mountHandle : mounts) {
      auto& mount = mountHandle.getEdenMount();
      auto& rootInode = mountHandle.getRootInode();
      auto unloaded = rootInode->unloadChildrenLastAccessedBefore(cutoff_ts);
      if (unloaded) {
        XLOGF(
            DBG6,
            "Unloaded {} inodes in background from mount {}",
            unloaded,
            mount.getPath());
      }
      mount.getInodeMap()->recordPeriodicInodeUnload(unloaded);
    }
  }
  scheduleInodeUnload(
      std::chrono::minutes(config->periodicUnloadIntervalMinutes.getValue()));
}

void EdenServer::scheduleInodeUnload(std::chrono::milliseconds timeout) {
  mainEventBase_->timer().scheduleTimeoutFn(
      [this] {
        XLOG(DBG4, "Beginning periodic inode unload");
        unloadInodes();
      },
      timeout);
}

#endif // !_WIN32

ImmediateFuture<Unit> EdenServer::recover(TakeoverData&& data) {
  handler_->disarmDelayedCancellation();
  return recoverImpl(std::move(data))
      .ensure(
          // Mark the server state as RUNNING once we finish setting up the
          // mount points. Even if an error occurs we still transition to the
          // running state.
          [this] {
            {
              auto state = runningState_.wlock();
              state->shutdownFuture =
                  folly::Future<std::optional<TakeoverData>>::makeEmpty();
              state->state = RunState::RUNNING;
            }
            // The failed takeover already disarmed us, and the privhelper is
            // still holding cleanShutdownNotified_. Without re-arming, this
            // recovered daemon would never be restarted again.
            restartArmer_.clearArmed();
            armPrivHelperRestart();
            // Recoveries cancelled while the takeover was under way, among
            // others, are not mounted.
            recoverCheckoutsNotServedSoon();
          });
}

ImmediateFuture<Unit> EdenServer::recoverImpl(TakeoverData&& takeoverData) {
  auto thriftRunningFuture = createThriftServer();

  const auto takeoverPath = edenDir_.getTakeoverSocketPath();

  // Recover the eden lock file and the thrift server socket.
  auto oldDaemonPid = edenDir_.takeoverLock(std::move(takeoverData.lockFile));
  if (oldDaemonPid.has_value()) {
    oldDaemonPid_ = oldDaemonPid.value();
  }
  server_->useExistingSocket(takeoverData.thriftSocket.release());

  if (auto nfsServer = serverState_->getNfsServer();
      nfsServer && takeoverData.mountdAcceptsPaused) {
    XLOG(DBG7, "Resuming mountd accepts after takeover recovery");
    runInEventBaseThreadAndRethrow(
        nfsServer->getEventBase(), [&] { nfsServer->resumeMountdAccepting(); });
  }

  // Remount our mounts from our prepared takeoverData
  std::vector<ImmediateFuture<Unit>> mountFutures;
  mountFutures = prepareMountsTakeover(
      std::make_unique<ForegroundStartupLogger>(startupStatusChannel_),
      std::move(takeoverData.mountPoints));

  // Return a future that will complete only when all mount points have
  // started and the thrift server is also running.
  mountFutures.emplace_back(std::move(thriftRunningFuture));
  return collectAll(std::move(mountFutures)).unit();
}

void EdenServer::serve() const {
  getServer()->serve();
}

std::string EdenServer::getEdenHeartbeatFileNameStr() const {
  return heartbeatManager_->getHeartbeatFileName();
}

std::optional<std::string> EdenServer::getOldEdenHeartbeatFileNameStr() const {
  if (oldDaemonPid_.has_value()) {
    return edenDir_.getHeartbeatFileNamePrefix().toString() +
        oldDaemonPid_.value();
  } else {
    return std::nullopt;
  }
}

#ifndef _WIN32
void EdenServer::createOrUpdateEdenHeartbeatFile() {
  heartbeatManager_->createOrUpdateHeartbeatFile();
}

void EdenServer::createDaemonExitSignalFile(int signal) {
  heartbeatManager_->createDaemonExitSignalFile(signal);
}

void EdenServer::removeEdenHeartbeatFile() const {
  heartbeatManager_->removeHeartbeatFile();
}

bool EdenServer::checkForPreviousHeartbeat(bool takeover) {
  auto config = serverState_->getReloadableConfig()->getEdenConfig();
  return heartbeatManager_->checkForPreviousHeartbeat(
      takeover,
      getOldEdenHeartbeatFileNameStr(),
      config->silentDaemonExitLogShow.getValue(),
      serverState_->getThreadPool().get());
}

#endif

Future<Unit> EdenServer::prepare(std::shared_ptr<StartupLogger> logger) {
  return prepareImpl(std::move(logger))
      .ensure(
          // Mark the server state as RUNNING once we finish setting up the
          // mount points. Even if an error occurs we still transition to the
          // running state. The prepare() code will log an error with more
          // details if we do fail to set up some of the mount points.
          //
          // SHUTTING_DOWN is terminal: a stop that arrived mid-startup has
          // already disarmed the privhelper, and returning to RUNNING would let
          // the arm that follows relaunch a daemon the user stopped. Takeover
          // recovery, the one way back out, does not come through here.
          [this] {
            {
              auto state = runningState_.wlock();
              if (state->state == RunState::SHUTTING_DOWN) {
                return;
              }
              state->state = RunState::RUNNING;
            }
            if (startedByTakeover_) {
              recoverCheckoutsNotServedSoon();
            }
          });
}

namespace {
bool shouldThrowDuringTakeover(const EdenConfig& config) {
  return config.clientThrowsDuringTakeover.getValue();
}

std::string describeLongRunningRequests(
    const std::vector<EdenServiceHandler::LongRunningRequest>& requests,
    ProcessInfoCache& processInfoCache) {
  std::string description;
  for (const auto& request : requests) {
    if (!description.empty()) {
      description += ", ";
    }
    std::string client = "an unknown client";
    if (request.clientPid) {
      auto pid = request.clientPid.value().get();
      auto name = processInfoCache.getProcessName(pid);
      client = name ? fmt::format("pid {} ({})", pid, *name)
                    : fmt::format("pid {}", pid);
    }
    description += fmt::format(
        "{} from {} running for {}{}",
        request.method,
        client,
        durationToString(request.age),
        request.cancelable ? "" : " (cannot be cancelled)");
  }
  return description;
}

std::chrono::seconds getTakeoverTimeoutSeconds(const EdenConfig& config) {
  return std::chrono::duration_cast<std::chrono::seconds>(
      config.takeoverReceiveTimeout.getValue());
}

#ifndef _WIN32
Future<TakeoverData> recoverPreparedTakeoverFromError(
    folly::exception_wrapper ew,
    TakeoverData&& takeover) {
  auto takeoverPromise = std::move(takeover.takeoverComplete);
  takeoverPromise.setValue(std::move(takeover));
  return makeFuture<TakeoverData>(std::move(ew));
}
#endif
} // namespace

Future<Unit> EdenServer::prepareImpl(std::shared_ptr<StartupLogger> logger) {
  bool doingTakeover = false;
  auto edenDirLockResult = edenDir_.acquireLock();
  if (!edenDirLockResult.first) {
    // Another edenfs process is already running.
    //
    // If --takeover was specified, fall through and attempt to gracefully
    // takeover mount points from the existing daemon.
    //
    // If --takeover was not specified, fail now.
    if (!FLAGS_takeover) {
      throwf<std::runtime_error>(
          "another instance of Eden appears to be running for {}",
          edenDir_.getPath());
    }
    doingTakeover = true;
  }
  startedByTakeover_ = doingTakeover;
  auto thriftRunningFuture = createThriftServer();
  // Start the PrivHelper client, using our main event base to drive its I/O
  serverState_->getPrivHelper()->setEdenFsEventsLogger(edenFsEventsLogger_);
  serverState_->getPrivHelper()->attachEventBase(mainEventBase_);
#ifndef _WIN32
  {
    // Sent whether it is on or off: a privhelper that outlives this daemon
    // keeps the last value it was told, so a reverted setting has to reach it.
    const bool overrideFstype =
        serverState_->getEdenConfig()->nfsOverrideFstype.getValue();
    folly::futures::detachOn(
        mainEventBase_,
        serverState_->getPrivHelper()
            ->setNfsFstypeOverride(overrideFstype)
            .thenError([overrideFstype](folly::exception_wrapper&& ew) {
              // An older privhelper rejects the request and keeps the
              // override on, which only matters when it was meant to go off.
              if (overrideFstype) {
                XLOGF(
                    DBG2,
                    "the privhelper does not support the NFS filesystem type override setting: {}",
                    ew.what());
              } else {
                XLOGF(
                    WARN,
                    "the privhelper did not turn the NFS filesystem type override off: {}",
                    ew.what());
              }
            })
            .semi());
  }
#endif

  startPeriodicTasks();

#ifndef _WIN32
  // If we are gracefully taking over from an existing edenfs process,
  // receive its lock, thrift socket, and mount points now.
  // This will shut down the old process.
  const auto takeoverPath = edenDir_.getTakeoverSocketPath();
  TakeoverData takeoverData;
#endif
  if (doingTakeover) {
#ifndef _WIN32
    logger->log(
        "Requesting existing edenfs process to gracefully "
        "transfer its mount points...");
    folly::stop_watch<std::chrono::microseconds> takeoverReceiveWatch;
    SCOPE_EXIT {
      serverState_->getStats()->addDuration(
          &TakeoverStats::receive, takeoverReceiveWatch.elapsed());
    };
    auto edenConfig = serverState_->getEdenConfig();
    try {
      takeoverData = takeoverMounts(
          takeoverPath,
          getTakeoverTimeoutSeconds(*edenConfig),
          FLAGS_forceTakeover,
          FLAGS_takeoverOnlyIfIdle,
          shouldThrowDuringTakeover(*edenConfig));
    } catch (...) {
      auto ew = folly::exception_wrapper{std::current_exception()};
      ew.with_exception([&](const std::exception& ex) {
        serverState_->getErrorLogger().log(
            EdenErrorInfo::takeover(ErrorArg::fromExceptionWithoutTrace(ex))
                .withErrorType("takeover_receive_failed"));
      });
      ew.throw_exception();
    }
    serverState_->getStats()->increment(&TakeoverStats::receiveSuccess);
    logger->log(
        "Received takeover information for ",
        takeoverData.mountPoints.size(),
        " mount points");

    // Take over the eden lock file and the thrift server socket.
    auto oldDaemonPid = edenDir_.takeoverLock(std::move(takeoverData.lockFile));
    if (oldDaemonPid.has_value()) {
      oldDaemonPid_ = oldDaemonPid.value();
    }
    server_->useExistingSocket(takeoverData.thriftSocket.release());

    // The kernel keeps the NFS mounts the old daemon handed over, and only an
    // NfsServer in this process can serve them, so build one even when the
    // config has turned the NFS server off: the alternative is a daemon that
    // dereferences a null NfsServer, or a mount nothing serves.
    const auto nfsMounts = std::count_if(
        takeoverData.mountPoints.begin(),
        takeoverData.mountPoints.end(),
        [](const TakeoverData::MountInfo& info) {
          return info.getMountProtocol() == MountProtocol::NFS;
        });
    if (nfsMounts > 0 && !serverState_->getNfsServer()) {
      logger->warn(
          "experimental:enable-nfs-server is false, but the previous edenfs "
          "handed over ",
          nfsMounts,
          " NFS mount(s); serving them with an NFS server until the next "
          "full restart");
      serverState_->createNfsServerForTakeover();
    }
#else
    NOT_IMPLEMENTED();
#endif // !_WIN32
  } else {
    // Remove any old thrift socket from a previous (now dead) edenfs daemon.
    prepareThriftAddress();
  }

  if (auto nfsServer = serverState_->getNfsServer()) {
    // The sockets belong to the NFS EventBase, so they are set up there.
    auto* nfsEventBase = nfsServer->getEventBase();
    const bool useUnixSocket =
        serverState_->getEdenConfig()->useUnixSocket.getValue();
#ifndef _WIN32
    if (doingTakeover && takeoverData.mountdServerSocket.has_value()) {
      XLOG(DBG7, "Initializing mountd from existing socket");
      bool mountdIsUnix = false;
      runInEventBaseThreadAndRethrow(nfsEventBase, [&] {
        nfsServer->initialize(
            std::move(takeoverData.mountdServerSocket.value()));
        mountdIsUnix = nfsServer->getMountdAddr().getFamily() == AF_UNIX;
      });
      // Mounts follow the running mountd's transport, so a changed
      // nfs:use-uds takes effect at the next full restart, not here.
      if (mountdIsUnix != useUnixSocket) {
        XLOGF(
            INFO,
            "nfs:use-uds is {} but the inherited mountd is {}; mounts stay {} until the next full restart",
            useUnixSocket,
            mountdIsUnix ? "unix" : "tcp",
            mountdIsUnix ? "unix" : "tcp");
      }
    } else {
#endif
      XLOG(DBG7, "Initializing mountd from scratch");
      std::optional<AbsolutePath> unixSocketPath;
      if (useUnixSocket) {
        // Every mount of this daemon follows the mountd's transport, so a
        // socket path that would not fit anywhere means TCP for all of them,
        // and so does a directory map that cannot be read to size them.
        std::optional<std::string> why;
        std::vector<std::string> checkoutIds;
        try {
          // The map has to outlive the loop: items() refers to it.
          const auto dirs =
              CheckoutConfig::loadClientDirectoryMap(edenDir_.getPath());
          for (const auto& client : dirs.items()) {
            checkoutIds.push_back(client.second.asString());
          }
        } catch (const std::exception& ex) {
          why = fmt::format(
              "the directory map could not be read to size the socket paths: {}",
              ex.what());
        }
        if (!why) {
          why = whyNfsCannotUseUnixSockets(
              edenDir_.getMountdSocketPath(),
              edenDir_.getClientsDir(),
              checkoutIds);
        }
        if (why) {
          logger->warn(
              "Serving NFS over TCP although nfs:use-uds is set: ", *why);
          XLOGF(
              WARN,
              "Serving NFS over TCP although nfs:use-uds is set: {}",
              *why);
        } else {
          unixSocketPath = edenDir_.getMountdSocketPath();
        }
      }
      runInEventBaseThreadAndRethrow(nfsEventBase, [&] {
        nfsServer->initialize(
            makeNfsSocket(std::move(unixSocketPath)),
            serverState_->getEdenConfig()->registerMountd.getValue());
      });
#ifndef _WIN32
    }
#endif
  }

#ifndef _WIN32
  // Start listening for graceful takeover requests
  takeoverServer_.reset(new TakeoverServer(
      getMainEventBase(),
      takeoverPath,
      this,
      &serverState_->getFaultInjector()));
  takeoverServer_->start();
#endif // !_WIN32

  return via(
      mainEventBase_,
      [this,
       logger,
       doingTakeover,
#ifndef _WIN32
       takeoverData = std::move(takeoverData),
#endif
       thriftRunningFuture = std::move(thriftRunningFuture)]() mutable {
        // Create fsck concurrency semaphore before either mount path.
        // A value of 0 means unlimited concurrency (no semaphore).
        auto maxConcurrentFsck = serverState_->getReloadableConfig()
                                     ->getEdenConfig()
                                     ->fsckMaxConcurrentMounts.getValue();
        if (maxConcurrentFsck > 0) {
          fsckSemaphore_ = std::make_unique<folly::LifoSem>(maxConcurrentFsck);
        }

        std::vector<ImmediateFuture<Unit>> mountFutures;
        if (doingTakeover) {
#ifndef _WIN32
          mountFutures = prepareMountsTakeover(
              logger, std::move(takeoverData.mountPoints));
#else
          NOT_IMPLEMENTED();
#endif // !_WIN32
        } else {
          mountFutures = prepareMounts(logger);
        }

        // Return a future that will complete only when all mount points have
        // started and the thrift server is also running.
        mountFutures.emplace_back(std::move(thriftRunningFuture));
        return collectAll(std::move(mountFutures)).unit().semi();
      });
}

std::shared_ptr<cpptoml::table> EdenServer::parseConfig() {
  auto configPath = edenDir_.getPath() + RelativePathPiece{kStateConfig};

#ifdef _WIN32
  std::ifstream inputFile(configPath.wide());
#else
  std::ifstream inputFile(configPath.c_str());
#endif
  if (!inputFile.is_open()) {
    if (errno != ENOENT) {
      folly::throwSystemErrorExplicit(
          errno, "unable to open EdenFS config ", configPath.view());
    }
    // No config file, assume an empty table.
    return cpptoml::make_table();
  }
  return cpptoml::parser(inputFile).parse();
}

void EdenServer::saveConfig(const cpptoml::table& root) {
  auto configPath = edenDir_.getPath() + RelativePathPiece{kStateConfig};

  std::ostringstream stream;
  stream << root;

  writeFileAtomic(configPath, folly::StringPiece(stream.str())).value();
}

std::vector<ImmediateFuture<Unit>> EdenServer::prepareMountsTakeover(
    shared_ptr<StartupLogger> logger,
    std::vector<TakeoverData::MountInfo>&& takeoverMounts) {
  // Trigger remounting of existing mount points
  // If doingTakeover is true, use the mounts received in TakeoverData
  std::vector<ImmediateFuture<Unit>> mountFutures;

  if (folly::kIsWindows) {
    NOT_IMPLEMENTED();
  }

  for (auto& info : takeoverMounts) {
    const auto stateDirectory = info.stateDirectory;
    auto mountFuture =
        makeImmediateFutureWith([&] {
          auto initialConfig = CheckoutConfig::loadFromClientDirectory(
              AbsolutePathPiece{info.mountPath},
              AbsolutePathPiece{info.stateDirectory});

          return mount(
              std::move(initialConfig), false, [](auto) {}, std::move(info));
        })
            .thenTry(
                [logger,
                 serverState = serverState_,
                 mountPath = info.mountPath](
                    folly::Try<std::shared_ptr<EdenMount>>&& result)
                    -> ImmediateFuture<folly::Unit> {
                  if (result.hasValue()) {
                    logger->log("Successfully took over mount ", mountPath);
                    return folly::unit;
                  } else {
                    incrementStartupMountFailures();
                    logger->warn(
                        "Failed to perform takeover for ",
                        mountPath,
                        ": ",
                        result.exception().what());
                    result.exception().with_exception(
                        [&](const std::exception& ex) {
                          serverState->getErrorLogger().log(
                              EdenErrorInfo::takeover(
                                  ErrorArg::fromExceptionWithoutTrace(ex))
                                  .withMountPoint(mountPath.asString())
                                  .withErrorType("takeover_remount_failed"));
                        });
                    return makeImmediateFuture<Unit>(
                        std::move(result).exception());
                  }
                });
    mountFutures.push_back(std::move(mountFuture));
  }

  return mountFutures;
}

std::vector<ImmediateFuture<Unit>> EdenServer::prepareMounts(
    shared_ptr<StartupLogger> logger) {
  std::vector<ImmediateFuture<Unit>> mountFutures;
  folly::dynamic dirs = folly::dynamic::object();
  try {
    dirs = CheckoutConfig::loadClientDirectoryMap(edenDir_.getPath());
  } catch (...) {
    auto ew = folly::exception_wrapper{std::current_exception()};
    incrementStartupMountFailures();
    logger->warn(
        "Could not parse config.json file: ",
        ew.what(),
        "\nSkipping remount step.");
    mountFutures.emplace_back(std::move(ew));
    return mountFutures;
  }

  if (dirs.empty()) {
    logger->log("No mount points currently configured.");
    return mountFutures;
  }
  logger->log("Remounting ", dirs.size(), " mount points...");

  for (const auto& client : dirs.items()) {
    const auto mountPath = client.first.asString();
    std::optional<size_t> progressIndex;
    auto mountFuture = makeImmediateFutureWith([&] {
      auto edenClientPath =
          edenDir_.getCheckoutStateDir(client.second.stringPiece());
      auto initialConfig = CheckoutConfig::loadFromClientDirectory(
          canonicalPath(mountPath), edenClientPath);
      auto index = progressManager_->wlock()->registerEntry(
          client.first.asString(), initialConfig->getOverlayPath().c_str());
      progressIndex = index;

      return mount(
          std::move(initialConfig), false, [this, logger, index](auto percent) {
            progressManager_->wlock()->manageProgress(logger, index, percent);
          });
    });

    // The factory runs eagerly, so the optional index is ready to capture.
    // Handle failures outside it to include config and mount construction
    // errors.
    mountFutures.push_back(
        std::move(mountFuture)
            .thenTry([this, logger, mountPath, progressIndex](
                         folly::Try<std::shared_ptr<EdenMount>>&& result) {
              if (result.hasValue()) {
                auto wl = progressManager_->wlock();
                wl->finishProgress(progressIndex.value());
                wl->printProgresses(logger);
                return ImmediateFuture<folly::Unit>{std::in_place};
              } else {
                incrementStartupMountFailures();
                auto errorMessage = fmt::format(
                    "Failed to remount {}: {}\n",
                    mountPath,
                    result.exception().what());
                auto wl = progressManager_->wlock();
                if (progressIndex.has_value()) {
                  wl->markFailed(progressIndex.value());
                }
                wl->printProgresses(logger, errorMessage);
                return makeImmediateFuture<Unit>(std::move(result).exception());
              }
            }));
  }
  return mountFutures;
}

void EdenServer::incrementStartupMountFailures() {
  // Increment a counter to track if there were any errors remounting checkouts
  // during startup.
  fb303::fbData->incrementCounter("startup_mount_failures");
}

void EdenServer::closeStorage() {
  // Destroy the backing stores.
  backingStores_.wlock()->clear();
}

bool EdenServer::performCleanup() {
  bool takeover = false;

  // Unforced takeovers may still be waiting for Thrift requests to finish.
  // Each wait holds this server and the main event base, so end them before
  // anything is torn down: cancellation wakes each one, and its continuation
  // runs on the main event base. The timer that cancels requests once the
  // grace period expires has nothing left to do either, since the Thrift
  // server has joined its requests by now.
  takeoverWaitCancellation_.requestCancellation();
  while (pendingTakeoverWaits_ != 0) {
    mainEventBase_->loopOnce();
  }
  takeoverWaitCancellation_ = folly::CancellationSource{};
  handler_->disarmDelayedCancellation();

  folly::stop_watch<> shutdown;
  bool shutdownSuccess = true;
  SCOPE_EXIT {
    auto shutdownTimeInSeconds =
        std::chrono::duration<double>{shutdown.elapsed()}.count();
    serverState_->getEdenFsEventsLogger()->logEvent(
        DaemonStop{shutdownTimeInSeconds, takeover, shutdownSuccess});
  };

  auto shutdownFuture =
      folly::SemiFuture<std::optional<TakeoverData>>::makeEmpty();
  {
    auto state = runningState_.wlock();
    takeover = state->shutdownFuture.valid();
    if (takeover) {
      shutdownFuture = std::move(state->shutdownFuture);
    }
    XDCHECK_EQ(state->state, RunState::SHUTTING_DOWN);
    // A no-op today: every path that reaches performCleanup has already
    // transitioned. Routed through the helper so that a future path which has
    // not still picks up whatever the helper does.
    markShuttingDownLocked(*state, "cleanup");
  }

#ifdef EDEN_HAVE_SERVER_OBSERVER
  // Stop the server observer publish thread. If this is a takeover and the
  // takeover fails, the recovery code will restart the thread again
  fb303::ThreadCachedServiceData::get()->stopPublishThread();
#endif

  if (!takeover) {
    shutdownFuture = performNormalShutdown().deferValue(
        [](auto&&) -> std::optional<TakeoverData> { return std::nullopt; });
  }

  XCHECK(shutdownFuture.valid())
      << "shutdownFuture should not be empty during cleanup";

  // Drive the main event base until shutdownFuture completes
  XCHECK_EQ(mainEventBase_, folly::EventBaseManager::get()->getEventBase());
  auto shutdownResult =
      std::move(shutdownFuture).via(mainEventBase_).getTryVia(mainEventBase_);

#ifndef _WIN32
  shutdownSuccess = !shutdownResult.hasException();

  // We must check if the shutdownResult contains TakeoverData, and if so
  // we must recover
  if (shutdownResult.hasValue()) {
    auto&& shutdownValue = shutdownResult.value();
    if (shutdownValue.has_value()) {
      // shutdownValue only contains a value if a takeover was not successful.
      shutdownSuccess = false;
      XLOG(INFO, "edenfs encountered a takeover error, attempting to recover");
      // We do not wait here for the remounts to succeed, and instead will
      // let runServer() drive the mainEventBase loop to finish this call
      folly::futures::detachOn(
          getServerState()->getThreadPool().get(),
          recover(std::move(shutdownValue).value()).semi());
      // Recovery resumes this EdenServer, so its executors must remain usable.
      return false;
    }
  }
#endif

  auto shutdownServerState =
      folly::makeGuard([this] { serverState_->shutdown(); });
  closeStorage();
  // Stop the privhelper process.
  shutdownPrivhelper();
  shutdownResult.throwUnlessValue();

  return true;
}

folly::SemiFuture<Unit> EdenServer::performNormalShutdown() {
#ifndef _WIN32
  takeoverServer_.reset();
#endif // !_WIN32

  // Clean up all the server mount points
  return unmountAll();
}

void EdenServer::shutdownPrivhelper() {
  // Explicitly stop the privhelper process so we can verify that it
  // exits normally.
  // stop() returns the privhelper's wait() status: >0 = exit code,
  // <0 = negated kill signal.
  const auto privhelperExitCode = serverState_->getPrivHelper()->stop();
  if (privhelperExitCode != 0) {
    int64_t exitCode = 0;
    int64_t exitSignal = 0;
    if (privhelperExitCode > 0) {
      exitCode = privhelperExitCode;
      XLOGF(ERR, "privhelper process exited with unexpected code {}", exitCode);
    } else {
      exitSignal = -privhelperExitCode;
      XLOGF(ERR, "privhelper process was killed by signal {}", exitSignal);
    }
    serverState_->getEdenFsEventsLogger()->logEvent(
        PrivhelperShutdown{exitCode, exitSignal});
  }
}

void EdenServer::addToMountPoints(std::shared_ptr<EdenMount> edenMount) {
  auto& mountPath = edenMount->getPath();

  {
    const auto mountPoints = mountPoints_->wlock();
    const auto ret = mountPoints->emplace(mountPath, EdenMountInfo(edenMount));
    if (!ret.second) {
      throw newEdenError(
          EEXIST,
          EdenErrorType::POSIX_ERROR,
          fmt::format("mount point \"{}\" is already mounted", mountPath));
    }
  }
}

void EdenServer::registerStats(std::shared_ptr<EdenMount> edenMount) {
  auto counters = fb303::ServiceData::get()->getDynamicCounters();
  counters->registerCallback(
      edenMount->getCounterName(CounterName::INODEMAP_LOADED), [edenMount] {
        auto counts = edenMount->getInodeMap()->getInodeCounts();
        return counts.fileCount + counts.treeCount;
      });
  counters->registerCallback(
      edenMount->getCounterName(CounterName::INODEMAP_UNLOADED), [edenMount] {
        return edenMount->getInodeMap()->getInodeCounts().unloadedInodeCount;
      });
  counters->registerCallback(
      edenMount->getCounterName(CounterName::PERIODIC_INODE_UNLOAD),
      [edenMount] {
        return edenMount->getInodeMap()
            ->getInodeCounts()
            .periodicLinkedUnloadInodeCount;
      });
  counters->registerCallback(
      edenMount->getCounterName(CounterName::PERIODIC_UNLINKED_INODE_UNLOAD),
      [edenMount] {
        return edenMount->getInodeMap()
            ->getInodeCounts()
            .periodicUnlinkedUnloadInodeCount;
      });
  counters->registerCallback(
      edenMount->getCounterName(CounterName::JOURNAL_MEMORY),
      [edenMount] { return edenMount->getJournal().estimateMemoryUsage(); });
  counters->registerCallback(
      edenMount->getCounterName(CounterName::JOURNAL_ENTRIES), [edenMount] {
        auto stats = edenMount->getJournal().getStats();
        return stats ? stats->entryCount : 0;
      });
  counters->registerCallback(
      edenMount->getCounterName(CounterName::JOURNAL_DURATION), [edenMount] {
        auto stats = edenMount->getJournal().getStats();
        return stats ? stats->getDurationInSeconds() : 0;
      });
  counters->registerCallback(
      edenMount->getCounterName(CounterName::JOURNAL_MAX_FILES_ACCUMULATED),
      [edenMount] {
        auto stats = edenMount->getJournal().getStats();
        return stats ? stats->maxFilesAccumulated : 0;
      });
#ifndef _WIN32
  if (auto* channel = edenMount->getFuseChannel()) {
    for (auto metric : RequestMetricsScope::requestMetrics) {
      counters->registerCallback(
          getCounterNameForFuseRequests(
              RequestMetricsScope::RequestStage::LIVE, metric, edenMount.get()),
          [edenMount, metric, channel] {
            return channel->getRequestMetric(metric);
          });
    }
  } else if (edenMount->getNfsdChannel()) {
    // TODO(xavierd): Add requestMetrics for NFS.
  }
#endif
#ifdef __linux__
  if (edenMount->getFuseChannel()) {
    counters->registerCallback(
        getCounterNameForFuseRequests(
            RequestMetricsScope::RequestStage::PENDING,
            RequestMetricsScope::RequestMetric::COUNT,
            edenMount.get()),
        [edenMount] {
          try {
            return getNumberPendingFuseRequests(edenMount.get());
          } catch (const std::exception&) {
            return size_t{0};
          }
        });
  }
#endif // __linux__
}

void EdenServer::unregisterStats(EdenMount* edenMount) {
  auto counters = fb303::ServiceData::get()->getDynamicCounters();
  counters->unregisterCallback(
      edenMount->getCounterName(CounterName::INODEMAP_LOADED));
  counters->unregisterCallback(
      edenMount->getCounterName(CounterName::INODEMAP_UNLOADED));
  counters->unregisterCallback(
      edenMount->getCounterName(CounterName::PERIODIC_INODE_UNLOAD));
  counters->unregisterCallback(
      edenMount->getCounterName(CounterName::PERIODIC_UNLINKED_INODE_UNLOAD));
  counters->unregisterCallback(
      edenMount->getCounterName(CounterName::JOURNAL_MEMORY));
  counters->unregisterCallback(
      edenMount->getCounterName(CounterName::JOURNAL_ENTRIES));
  counters->unregisterCallback(
      edenMount->getCounterName(CounterName::JOURNAL_DURATION));
  counters->unregisterCallback(
      edenMount->getCounterName(CounterName::JOURNAL_MAX_FILES_ACCUMULATED));
#ifndef _WIN32
  if (edenMount->getFuseChannel()) {
    for (auto metric : RequestMetricsScope::requestMetrics) {
      counters->unregisterCallback(getCounterNameForFuseRequests(
          RequestMetricsScope::RequestStage::LIVE, metric, edenMount));
    }
  } else if (edenMount->getNfsdChannel()) {
    // TODO(xavierd): Unregister NFS metrics
  }
#endif
#ifdef __linux__
  if (edenMount->getFuseChannel()) {
    counters->unregisterCallback(getCounterNameForFuseRequests(
        RequestMetricsScope::RequestStage::PENDING,
        RequestMetricsScope::RequestMetric::COUNT,
        edenMount));
  }
#endif // __linux__
}

void EdenServer::registerInodePopulationReportsCallback() {
  getServerState()->getNotifier()->registerInodePopulationReportCallback(
      [mountPoints = mountPoints_]() -> std::vector<InodePopulationReport> {
        std::vector<InodePopulationReport> inodePopulationReports;

        {
          const auto lockedMountPoints = mountPoints->rlock();
          for (const auto& [_, info] : *lockedMountPoints) {
            const auto& mount = info.edenMount;

            auto counts = mount->getInodeMap()->getInodeCounts();

            inodePopulationReports.push_back(
                {mount->getCheckoutConfig()->getMountPath().c_str(),
                 counts.fileCount + counts.treeCount +
                     counts.unloadedInodeCount});
          }
        }

        return inodePopulationReports;
      });
}

void EdenServer::unregisterInodePopulationReportsCallback() {
  getServerState()->getNotifier()->registerInodePopulationReportCallback(
      nullptr);
}

Future<Unit> EdenServer::performTakeoverStart(
    [[maybe_unused]] std::shared_ptr<EdenMount> edenMount,
    [[maybe_unused]] TakeoverData::MountInfo&& info) {
#ifndef _WIN32
  auto mountPath = std::move(info.mountPath);

  auto future = completeTakeoverStart(edenMount, std::move(info));
  return std::move(future).thenValue(
      [this,
       edenMount = std::move(edenMount),
       mountPath = std::move(mountPath)](auto&&) mutable {
        // Daemon-managed bind mounts are vestigial, but left in the privhelper
        // protocol in case we change our mind.
        std::vector<std::string> bindMounts;
        return serverState_->getPrivHelper()
            ->takeoverStartup(mountPath.view(), bindMounts)
            .thenValue([edenMount = std::move(edenMount)](auto&&) {
              if (auto* channel = edenMount->getFuseChannel()) {
                channel->maybeSetFuseReadAhead();
              }
            });
      });
#else
  NOT_IMPLEMENTED();
#endif
}

Future<Unit> EdenServer::completeTakeoverStart(
    [[maybe_unused]] std::shared_ptr<EdenMount> edenMount,
    [[maybe_unused]] TakeoverData::MountInfo&& info) {
  if (auto channelData = std::get_if<FuseChannelData>(&info.channelInfo)) {
    // Start up the fuse workers.
    return edenMount->takeoverFuse(std::move(*channelData));
  } else if (
      auto nfsMountInfo = std::get_if<NfsChannelData>(&info.channelInfo)) {
    return edenMount->takeoverNfs(std::move(*nfsMountInfo));
  } else {
    return folly::makeFuture<Unit>(std::runtime_error(
        fmt::format(
            "Unsupported ChannelInfo Type: {}", info.channelInfo.index())));
  }
}

ImmediateFuture<std::shared_ptr<EdenMount>> EdenServer::mount(
    std::unique_ptr<CheckoutConfig> initialConfig,
    bool readOnly,
    OverlayChecker::ProgressCallback&& progressCallback,
    optional<TakeoverData::MountInfo>&& optionalTakeover) {
  folly::stop_watch<> mountStopWatch;

  initialConfig->clearIntentionallyUnmountedFlag();

  auto bsType = toBackingStoreType(initialConfig->getRepoType());
  XLOGF(
      INFO, "Creating backing store of type: {}", toBackingStoreString(bsType));
  auto backingStore =
      getBackingStore(bsType, initialConfig->getRepoSource(), *initialConfig);

  std::shared_ptr<WalkDetector> walkDetector;
  if (serverState_->getReloadableConfig()
          ->getEdenConfig()
          ->enableWalkDetector.getValue()) {
    walkDetector =
        backingStore->createWalkDetector(initialConfig->getMountPath());
  }
  auto objectStore = ObjectStore::create(
      backingStore,
      treeCache_,
      getStats().copy(),
      serverState_->getProcessInfoCache(),
      serverState_->getEdenFsEventsLogger(),
      serverState_->getReloadableConfig(),
      initialConfig->getCaseSensitive(),
      folly::getKeepAliveToken(serverState_->getThreadPool().get()),
      walkDetector);
  auto journal = std::make_unique<Journal>(
      getStats().copy(), &serverState_->getFaultInjector());

  // Create the EdenMount object and insert the mount into the mountPoints_ map.
  auto edenMount = EdenMount::create(
      std::move(initialConfig),
      std::move(objectStore),
      blobCache_,
      serverState_,
      std::move(journal),
      getStats().copy(),
      std::nullopt,
      std::move(walkDetector));
  addToMountPoints(edenMount);

  if (fsckSemaphore_) {
    edenMount->getOverlay()->setFsckSemaphore(fsckSemaphore_.get());
  }

  registerStats(edenMount);

  const bool doTakeover = optionalTakeover.has_value();

  auto initFuture = edenMount->initialize(
      std::move(progressCallback),
      doTakeover ? std::make_optional(optionalTakeover->inodeMap)
                 : std::nullopt,
      doTakeover ? std::make_optional(optionalTakeover->getMountProtocol())
                 : std::nullopt);

  // Now actually begin starting the mount point
  return std::move(initFuture)
      .thenError([this, edenMount](folly::exception_wrapper ew) {
        XLOGF(
            ERR, "error initializing {}: {}", edenMount->getPath(), ew.what());
        mountFinished(edenMount.get(), std::nullopt);
        return makeImmediateFuture<folly::Unit>(std::move(ew));
      })
      .thenValue([this,
                  doTakeover = doTakeover,
                  readOnly,
                  edenMount,
                  mountStopWatch,
                  optionalTakeover =
                      std::move(optionalTakeover)](folly::Unit) mutable {
        return (optionalTakeover ? performTakeoverStart(
                                       edenMount, std::move(*optionalTakeover))
                                 : edenMount->startFsChannel(readOnly))
            .thenError([this, edenMount](folly::exception_wrapper ew) {
              // Call mountFinished() if an error occurred during FUSE
              // initialization.
              mountFinished(edenMount.get(), std::nullopt);
              return makeFuture<folly::Unit>(std::move(ew));
            })
            .thenValue([edenMount, doTakeover = doTakeover, this](
                           folly::Unit) mutable {
              registerStats(edenMount);
              // The checkout root is this mount's root now, so the health
              // check can confirm it with statx from its first probe.
              if (auto device =
                      mountRootDevice(edenMount->getPath().asString())) {
                edenMount->setHealthCheckDevice(*device);
              }

              // Now that we've started the workers, arrange to call
              // mountFinished once the pool is torn down.
              auto finishFuture =
                  edenMount->getFsChannelCompletionFuture().thenTry(
                      [this, edenMount](
                          folly::Try<TakeoverData::MountInfo>&& takeover) {
                        std::optional<TakeoverData::MountInfo> optTakeover;
                        if (takeover.hasValue()) {
                          optTakeover = std::move(takeover.value());
                        }
                        unregisterStats(edenMount.get());
                        mountFinished(
                            edenMount.get(),
                            std::move(optTakeover),
                            /*fsChannelCompleted=*/true);
                      });

              if (doTakeover) {
                // The bind mounts are already mounted in the takeover case
                return makeFuture<std::shared_ptr<EdenMount>>(
                    std::move(edenMount));
              } else {
                // Perform all of the bind mounts associated with the
                // client.  We don't need to do this for the takeover
                // case as they are already mounted.
                return serverState_->getFaultInjector()
                    .checkAsync("mountBindMounts", edenMount->getPath().view())
                    .semi()
                    .deferValue([edenMount](auto&&) {
                      return edenMount->performBindMounts();
                    })
                    .deferValue([edenMount](auto&&) { return edenMount; })
                    .deferError([edenMount](
                                    const folly::exception_wrapper& ew) {
                      XLOG(
                          ERR,
                          "Error while performing bind mounts, will continue with mount anyway: {}",
                          ew.what());
                      return edenMount;
                    })
                    .via(getServerState()->getThreadPool().get());
              }
            })
            .thenTry([this, mountStopWatch, doTakeover, edenMount](auto&& t) {
              // A successful mount ends the outage, unless its channel has
              // already stopped again. Earlier issues no longer apply, so the
              // next outage is reported again.
              if (t.hasValue() && !edenMount->getFsChannelStop()) {
                resetCheckoutRecovery(edenMount->getPath());
                clearMountHealthIssues(
                    emittedMountHealthIssues_, edenMount->getPath().asString());
              }
              auto* fsChannel = edenMount->getFsChannel();
              auto inodeCatalogType =
                  edenMount->getCheckoutConfig()->getInodeCatalogType();
              std::optional<std::string> fsTransportName;
#ifdef __linux__
              if (auto* fuseChannel = dynamic_cast<FuseChannel*>(fsChannel)) {
                fsTransportName = fuseChannel->getTransportName();
              }
#endif
              serverState_->getEdenFsEventsLogger()->logEvent(
                  FinishedMount{
                      std::string{toBackingStoreString(
                          edenMount->getCheckoutConfig()
                              ->getRepoBackingStoreType())},
                      edenMount->getCheckoutConfig()->getRepoType(),
                      std::string{basename(
                          edenMount->getCheckoutConfig()->getRepoSource())},
                      fsChannel ? fsChannel->getName() : "unknown",
                      std::move(fsTransportName),
                      doTakeover,
                      std::chrono::duration<double>{mountStopWatch.elapsed()}
                          .count(),
                      !t.hasException(),
                      edenMount->getOverlay()->hadCleanStartup(),
                      inodeCatalogType.has_value()
                          ? static_cast<int64_t>(inodeCatalogType.value())
                          : 0});
              return makeFuture(std::move(t));
            });
      });
}

namespace {
// Best effort: failing to write the marker must not fail the unmount.
void markIntentionallyUnmounted(const CheckoutConfig& config) {
  try {
    config.setIntentionallyUnmountedFlag();
  } catch (const std::exception& ex) {
    XLOGF(
        WARN,
        "Could not mark {} as intentionally unmounted: {}",
        config.getMountPath(),
        ex.what());
  }
}
} // namespace

folly::SemiFuture<Unit> EdenServer::unmount(
    AbsolutePathPiece mountPath,
    UnmountOptions options) {
  auto future = Future<Unit>::makeEmpty();
  auto mount = std::shared_ptr<EdenMount>{};
  auto unmount = folly::makeSemiFutureWith([&] {
    {
      const auto mountPoints = mountPoints_->wlock();
      const auto it = mountPoints->find(mountPath);
      if (it == mountPoints->end()) {
        return folly::makeSemiFuture<Unit>(std::out_of_range(
            fmt::format("no such mount point {}", mountPath)));
      }
      future = it->second.unmountPromise.getFuture();
      mount = it->second.edenMount;
    }

    // Written before teardown starts, so that unmount recovery never
    // sees this checkout unmounted without the marker.
    markIntentionallyUnmounted(*mount->getCheckoutConfig());
    serverState_->getFaultInjector().check("unmount", mountPath.view());
    return mount->unmount(options);
  });

  // Keep the mount alive through the channel unmount, then return the final
  // shutdown promise without deferred callbacks that could outlive the pool.
  return std::move(unmount).defer(
      [this,
       path = mountPath.copy(),
       mount = std::move(mount),
       future = std::move(future)](
          folly::Try<Unit>&& result) mutable -> Future<Unit> {
        if (result.hasException()) {
          if (mount) {
            bool stillMounted = false;
            {
              const auto mountPoints = mountPoints_->rlock();
              const auto it = mountPoints->find(mount->getPath());
              stillMounted =
                  it != mountPoints->end() && it->second.edenMount == mount;
            }
            if (stillMounted) {
              mount->getCheckoutConfig()->clearIntentionallyUnmountedFlag();
            }
          }
          XLOGF(
              ERR,
              "Failed to perform unmount for \"{}\": {}",
              path,
              result.exception().what());
          return folly::makeFuture<Unit>(std::move(result));
        }
        return std::move(future);
      });
}

void EdenServer::mountFinished(
    EdenMount* edenMount,
    std::optional<TakeoverData::MountInfo> takeover,
    bool fsChannelCompleted) {
  const auto& mountPath = edenMount->getPath();
  const auto stop = edenMount->getFsChannelStop();
  const std::string stopReason =
      stop ? std::string{stop->reason.name} : "mount_failed";
  XLOGF(INFO, "mount point \"{}\" stopped ({})", mountPath, stopReason);
  recordCheckoutStop(
      mountPath,
      stopReason,
      stop ? std::make_optional(stop->time) : std::nullopt);

  // Although it's uncommon for a GC to be running during unmount, we should
  // confirm any running GC exited before proceeding. Typically, GC stops on
  // the first cancellation request attempt, but if not, we allow additional
  // retries to ensure a clean unmount.
  // In most cases, this function returns immediately, but in the case of a
  // long-running or blocked GC, it may take a few (maximum 3) seconds to
  // complete. If GC won't stop after 3 seconds, the unmount will proceed
  // anyway, and probably the shutting down edenfs process will be killed by a
  // crash instead of clean exit. We can send a SIGKILL signal or throw an
  // exception here, but both of them has same results.
  stopAllGarbageCollections(
      /*maxRetries=*/3, /*retryInterval=*/std::chrono::seconds(1));

  auto shutdownFuture = folly::SemiFuture<SerializedInodeMap>::makeEmpty();
  bool doTakeover = false;
  {
    const auto mountPoints = mountPoints_->wlock();
    const auto it = mountPoints->find(mountPath);
    XCHECK(it != mountPoints->end());

    doTakeover = it->second.takeoverPromise.has_value();
    // Shutdown the EdenMount, while holding the mountPoints_ lock. This is
    // done to avoid a race between checking isSafeForInodeAccess and the
    // InodeMap being destroyed. In that race, the rootInode may be a nullptr
    // thus leading to a nullptr dereference.
    shutdownFuture = edenMount->shutdown(doTakeover);
  }
  // `takeover` is set for every channel that stopped cleanly; the takeover
  // promise says whether this is a takeover.
  const bool recoverAfterStop =
      fsChannelCompleted &&
      shouldRecoverAfterChannelStop(
          *edenMount, stop && stop->reason.unexpected, doTakeover);

  serverState_->getFaultInjector()
      .checkAsync("mountShutdown", mountPath.view())
      .semi()
      .via(getMainEventBase())
      .thenValue([shutdownFuture = std::move(shutdownFuture)](auto&&) mutable {
        return std::move(shutdownFuture);
      })
      .thenTry([this,
                mountPath = mountPath,
                clientDirectory =
                    edenMount->getCheckoutConfig()->getClientDirectory(),
                recoverAfterStop = recoverAfterStop,
                takeoverData = std::move(takeover)](
                   folly::Try<SerializedInodeMap>&& result) mutable {
        // Erase the EdenMount from mountPoints_ before fulfilling the
        // promises. Fulfilling unmountPromise releases the unmountV2
        // response on another thread, and a caller that was told the
        // unmount succeeded must not still see the mount in listMounts():
        // `eden rm` relies on that to delete the checkout's configuration
        // without leaving the daemon serving a mount that no on-disk
        // configuration describes.
        //
        // The promises leave the entry only now. While the mount shuts
        // down, unmount() and unmountAll() still find the entry and take
        // futures from its unmountPromise; a SharedPromise moved out
        // earlier leaves an empty one behind, and the futures taken from
        // that one break when the entry is erased.
        std::shared_ptr<EdenMount> removedMount;
        folly::SharedPromise<Unit> unmountPromise;
        std::optional<folly::Promise<TakeoverData::MountInfo>> takeoverPromise;
        {
          const auto mountPoints = mountPoints_->wlock();
          const auto it = mountPoints->find(mountPath);
          if (it != mountPoints->end()) {
            removedMount = std::move(it->second.edenMount);
            unmountPromise = std::move(it->second.unmountPromise);
            takeoverPromise = std::move(it->second.takeoverPromise);
            mountPoints->erase(it);
          }
        }
        if (recoverAfterStop) {
          // The short delay lets a concurrent `eden unmount` or `eden rm`
          // write its marker first.
          scheduleCallbackOnMainEventBase(
              kChannelStopRecoveryDelay, [this, mountPath, clientDirectory] {
                recoverCheckoutAfterChannelStop(mountPath, clientDirectory);
              });
        }
        if (takeoverPromise) {
          takeoverPromise.value().setWith([&]() mutable {
            takeoverData.value().inodeMap = std::move(result.value());
            return std::move(takeoverData.value());
          });
        }
        unmountPromise.setTry(
            folly::makeTryWith([result = std::move(result)]() {
              result.throwUnlessValue();
              return Unit{};
            }));
      });
}

std::vector<EdenMountHandle> EdenServer::getMountPoints() const {
  std::vector<EdenMountHandle> results;
  {
    const auto mountPoints = mountPoints_->rlock();
    for (const auto& [path, mountInfo] : *mountPoints) {
      auto& mount = mountInfo.edenMount;
      // Avoid returning mount points that are still initializing and are
      // not ready to perform inode operations yet.
      if (!mount->isSafeForInodeAccess()) {
        continue;
      }
      results.emplace_back(mount, mount->getRootInodeUnchecked());
    }
  }
  return results;
}

EdenServer::MountList EdenServer::getAllMountPoints() const {
  MountList results;
  {
    const auto mountPoints = mountPoints_->rlock();
    for (const auto& entry : *mountPoints) {
      results.emplace_back(entry.second.edenMount);
    }
  }
  return results;
}

EdenMountHandle EdenServer::getMount(AbsolutePathPiece mountPath) const {
  const auto mountPoints = mountPoints_->rlock();
  const auto it = mountPoints->find(mountPath);
  if (it == mountPoints->end()) {
    throw newEdenError(
        ENOENT,
        EdenErrorType::POSIX_ERROR,
        fmt::format(
            "mount point \"{}\" is not known to this eden instance",
            mountPath));
  }
  auto mount = it->second.edenMount;
  if (!mount->isSafeForInodeAccess()) {
    throw newEdenError(
        EBUSY,
        EdenErrorType::POSIX_ERROR,
        fmt::format("mount point \"{}\" is still initializing", mountPath));
  }
  return {mount, mount->getRootInodeUnchecked()};
}

namespace {

CheckoutResult finishCheckoutPostProcessing(
    EdenServer* server,
    CheckoutResult&& result,
    CheckoutMode checkoutMode,
    bool isNfs,
    AbsolutePath mountPath) {
  server->getServerState()->getNotifier()->signalCheckout(
      server->enumerateInProgressCheckouts());
  if (checkoutMode == CheckoutMode::DRY_RUN) {
    return std::move(result);
  }

  // In NFSv3 the kernel never tells us when its safe to unload
  // inodes ("safe" meaning all file handles to the inode have been
  // closed).
  //
  // To avoid unbounded memory and disk use we need to periodically
  // clean them up. Checkout will likely create a lot of stale innodes
  // so we run a delayed cleanup after checkout.
  if (isNfs &&
      server->getServerState()
          ->getReloadableConfig()
          ->getEdenConfig()
          ->unloadUnlinkedInodes.getValue()) {
    // During whole Eden Process shutdown, this function can only be
    // run before the mount is destroyed. This is because the function
    // is either run before the server event base is destroyed or it
    // is not run at all, and the server event base is destroyed
    // before the mountPoints. Since the function must be run before
    // the eventbase is destroyed and the eventbase is destroyed
    // before the mountPoints, this function can only be called before
    // the mount points are destroyed during normal destruction.
    // However, the mount point might have been unmounted before this
    // function is run outside of shutdown.
    auto delay = server->getServerState()
                     ->getReloadableConfig()
                     ->getEdenConfig()
                     ->postCheckoutDelayToUnloadInodes.getValue();
    XLOGF(
        DBG9,
        "Scheduling unlinked inode cleanup for mount {} in {} seconds.",
        mountPath,
        durationStr(delay));
    server->scheduleCallbackOnMainEventBase(
        std::chrono::duration_cast<std::chrono::milliseconds>(delay),
        [server, mountPath = std::move(mountPath)]() {
          try {
            // TODO: This might be a pretty expensive operation to run
            // on an EventBase. Maybe we should debounce onto a
            // different executor.
            auto mountHandle = server->getMount(mountPath);
            mountHandle.getEdenMount().forgetStaleInodes();
          } catch (EdenError& err) {
            // This is an expected error if the mount has been
            // unmounted before this callback ran.
            if (err.errorCode() == ENOENT) {
              XLOGF(
                  DBG3,
                  "Callback to clear inodes: Mount cannot be found. {}",
                  *err.message());
            } else {
              throw;
            }
          }
        });
  }
  return std::move(result);
}

} // namespace

ImmediateFuture<CheckoutResult> EdenServer::checkOutRevision(
    AbsolutePathPiece mountPath,
    std::string& rootId,
    std::optional<folly::StringPiece> rootHgManifest,
    const ObjectFetchContextPtr& fetchContext,
    StringPiece callerName,
    CheckoutMode checkoutMode) {
  auto mountHandle = getMount(mountPath);
  auto& edenMount = mountHandle.getEdenMount();
  auto root = edenMount.getObjectStore()->parseRootId(rootId);
  if (rootHgManifest.has_value()) {
    // The hg client has told us what the root manifest is.
    //
    // This is useful when a commit has just been created.  We won't be able to
    // ask the import helper to map the commit to its root manifest because it
    // won't know about the new commit until it reopens the repo.  Instead,
    // import the manifest for this commit directly.
    auto rootManifest = hash20FromThrift(rootHgManifest.value());
    edenMount.getObjectStore()
        ->getBackingStore()
        ->importManifestForRoot(root, rootManifest, fetchContext)
        .get();
  }

  bool isNfs = edenMount.isNfsdChannel();

  // the +1 is so we count the current checkout that hasn't quite started yet
  getServerState()->getNotifier()->signalCheckout(
      enumerateInProgressCheckouts() + 1);

  // Inode loading during checkout runs on the diff work executor instead of
  // the immediate executor.
  fetchContext->setDetachedExecutor(diffWorkExecutor_.get());

  // Start from a not-ready future so that the synchronous part of
  // edenMount.checkout runs on the diff work executor rather than the Thrift
  // worker thread.
  auto checkoutFuture =
      makeNotReadyImmediateFuture()
          .thenValue([&edenMount,
                      rootInode = mountHandle.getRootInode(),
                      root,
                      fetchContext = fetchContext.copy(),
                      callerName = callerName.str(),
                      checkoutMode](auto&&) mutable {
            return edenMount.checkout(
                std::move(rootInode),
                root,
                fetchContext,
                callerName,
                checkoutMode);
          })
          .thenValue([this, checkoutMode, isNfs, mountPath = mountPath.copy()](
                         CheckoutResult&& result) {
            return finishCheckoutPostProcessing(
                this, std::move(result), checkoutMode, isNfs, mountPath);
          });

  // This is an UnboundedQueueExecutor, which is guaranteed to never block
  // nor throw (except OOM), nor execute inline from `add()`.
  //
  // Thrift documentation states that "it is almost never a good idea to send
  // work off Thrift’s CPU worker". However, it also notes that "user code may
  // delegate the rest of the work to other thread pools, thus shifting load
  // off Thrift’s CPU Workers thread and letting the thread pick up new
  // incoming work" but warns that "if there is work offloaded to other thread
  // pools, there should be backpressure mechanism that would block Thrift CPU
  // Workers thread when those internal thread pools are overloaded."
  //
  // Without backpressure, EdenFS would see an increase in memory pressure.
  // potential slowdowns, and/or OOMS. EdenFS only allows one checkout at a
  // time (https://fburl.com/code/a6eoy8wu), which acts as the aforementioned
  // backpressure mechanism.
  checkoutFuture =
      std::move(checkoutFuture).semi().via(diffWorkExecutor_.get());

  return std::move(checkoutFuture).ensure([mountHandle] {});
}

folly::coro::now_task<CheckoutResult> EdenServer::co_checkOutRevision(
    AbsolutePathPiece mountPath,
    std::string& rootId,
    std::optional<folly::StringPiece> rootHgManifest,
    const ObjectFetchContextPtr& fetchContext,
    StringPiece callerName,
    CheckoutMode checkoutMode) {
  auto mountHandle = getMount(mountPath);
  auto& edenMount = mountHandle.getEdenMount();
  auto root = edenMount.getObjectStore()->parseRootId(rootId);
  if (rootHgManifest.has_value()) {
    // The hg client has told us what the root manifest is.
    //
    // This is useful when a commit has just been created.  We won't be able to
    // ask the import helper to map the commit to its root manifest because it
    // won't know about the new commit until it reopens the repo.  Instead,
    // import the manifest for this commit directly.
    auto rootManifest = hash20FromThrift(rootHgManifest.value());
    co_await edenMount.getObjectStore()
        ->getBackingStore()
        ->co_importManifestForRoot(root, rootManifest, fetchContext);
  }

  bool isNfs = edenMount.isNfsdChannel();

  // the +1 is so we count the current checkout that hasn't quite started yet
  getServerState()->getNotifier()->signalCheckout(
      enumerateInProgressCheckouts() + 1);

  // Helper that runs the checkout body. Extracted into a Task so it can be
  // scheduled on the diff work executor.
  //
  // Captures are explicit (no [&]) so the lambda owns mountHandle and
  // doesn't alias stack locals if the Task is later detached.
  auto checkoutBody =
      [mountHandle,
       root,
       fetchContext = fetchContext.copy(),
       callerName = callerName.str(),
       checkoutMode]() mutable -> folly::coro::Task<CheckoutResult> {
    co_return co_await mountHandle.getEdenMount().co_checkout(
        mountHandle.getRootInode(),
        root,
        fetchContext,
        callerName,
        checkoutMode);
  };

  fetchContext->setDetachedExecutor(diffWorkExecutor_.get());
  // Pin the checkout body to the diff work executor to isolate long
  // checkouts from thrift worker thread starvation.
  auto result = co_await folly::coro::co_withExecutor(
      diffWorkExecutor_.get(), checkoutBody());

  co_return finishCheckoutPostProcessing(
      this, std::move(result), checkoutMode, isNfs, AbsolutePath{mountPath});
}

shared_ptr<BackingStore> EdenServer::getBackingStore(
    BackingStoreType type,
    StringPiece name,
    const CheckoutConfig& config) {
  BackingStoreKey key{type, name.str()};
  auto lockedStores = backingStores_.wlock();
  const auto it = lockedStores->find(key);
  if (it != lockedStores->end()) {
    return it->second;
  }

  const auto store = backingStoreFactory_->createBackingStore(
      type,
      BackingStoreFactory::CreateParams{
          name, serverState_.get(), getStats(), config});
  lockedStores->emplace(key, store);
  return store;
}

std::unordered_set<std::shared_ptr<BackingStore>>
EdenServer::getBackingStores() {
  std::unordered_set<std::shared_ptr<BackingStore>> backingStores{};
  auto lockedStores = this->backingStores_.rlock();
  for (auto entry : *lockedStores) {
    backingStores.emplace(std::move(entry.second));
  }
  return backingStores;
}

std::unordered_set<shared_ptr<SaplingBackingStore>>
EdenServer::getSaplingBackingStores() {
  std::unordered_set<std::shared_ptr<SaplingBackingStore>>
      saplingBackingStores{};
  {
    auto lockedStores = this->backingStores_.rlock();
    for (const auto& entry : *lockedStores) {
      if (auto saplingBackingStore =
              std::dynamic_pointer_cast<SaplingBackingStore>(entry.second)) {
        saplingBackingStores.emplace(std::move(saplingBackingStore));
      }
    }
  }
  return saplingBackingStores;
}

std::vector<size_t> EdenServer::collectSaplingBackingStoreCounters(
    const std::function<size_t(const SaplingBackingStore&)>&
        getCounterFromStore) {
  std::vector<size_t> counters;
  for (const auto& store : this->getSaplingBackingStores()) {
    counters.emplace_back(getCounterFromStore(*store));
  }
  return counters;
}

folly::SemiFuture<Unit> EdenServer::createThriftServer() {
  auto edenConfig = config_->getEdenConfig();
  server_ = make_shared<ThriftServer>();
#ifdef EDEN_HAVE_SERVER_OBSERVER
  server_->setObserver(createServerObserver(
      kServiceName, edenConfig->thriftServerObserverSamplingRate.getValue()));
  // The server observer that is set up above collects its stats via a
  // ThreadCachedServiceData object rather than directly via the ServiceData
  // object for efficiency's sake. However, it does require periodically calling
  // publishStats() in order to flush the stats data cached in each thread to
  // the underlying ServiceData object.
  fb303::ThreadCachedServiceData::get()->startPublishThread(
      std::chrono::duration_cast<std::chrono::milliseconds>(
          edenConfig->thriftServerObserverPublishInterval.getValue()));
#endif
  server_->setMaxRequests(edenConfig->thriftMaxRequests.getValue());

  // Set up the CPU worker threads
  size_t thriftCpuThreads = edenConfig->thriftNumWorkers.getValue();
  if (!thriftUseResourcePools_) {
    server_->setNumCPUWorkerThreads(thriftCpuThreads);
  } else {
    server_->requireResourcePools();
    auto executor = std::make_shared<folly::CPUThreadPoolExecutor>(
        std::make_pair(thriftCpuThreads, thriftCpuThreads),
        folly::CPUThreadPoolExecutor::makeThrottledLifoSemQueue(
            std::chrono::microseconds{0}),
        std::make_shared<folly::NamedThreadFactory>("EdenThrift"));
    auto requestPile = std::make_unique<apache::thrift::RoundRobinRequestPile>(
        apache::thrift::RoundRobinRequestPile::Options{});
    std::unique_ptr<apache::thrift::ConcurrencyControllerInterface>
        concurrencyController;
    auto mode = apache::thrift::ParallelConcurrencyController::
        RequestExecutionMode::Parallel;
    if (thriftUseSerialExecution_) {
      XLOG(INFO, "Using Serial Execution for Thrift Server");
      mode = apache::thrift::ParallelConcurrencyController::
          RequestExecutionMode::Serial;
    } else {
      XLOG(INFO, "Using Resource Pools for Thrift Server");
    }

    concurrencyController =
        std::make_unique<apache::thrift::ParallelConcurrencyController>(
            *requestPile, *executor, mode);

    server_->resourcePoolSet().setResourcePool(
        apache::thrift::ResourcePoolHandle::defaultAsync(),
        std::move(requestPile),
        std::move(executor),
        std::move(concurrencyController),
        apache::thrift::concurrency::NORMAL);
  }
  server_->setCPUWorkerThreadName("EdenThrift");

  server_->setQueueTimeout(
      std::chrono::floor<std::chrono::milliseconds>(
          edenConfig->thriftQueueTimeout.getValue()));
  server_->setAllowCheckUnimplementedExtraInterfaces(false);

  // Setting this allows us to only do stopListening() on the stop() call
  // and delay thread-pool join (stop cpu workers + stop workers) until
  // server object destruction. This specifically matters in the takeover
  // shutdown code path.
  server_->setStopWorkersOnStopListening(false);

  // The server joins its in-flight requests when it stops. Returning from
  // serve() while they still run, which Thrift's
  // leakOutstandingRequestsWhenServerStops() permits, lets them reach the
  // server and the mounts after shutdown has torn them down (S532385). A
  // request that outlives this wait aborts the daemon instead.
  server_->setWorkersJoinTimeout(
      std::chrono::duration_cast<std::chrono::seconds>(
          edenConfig->thriftWorkersJoinTimeout.getValue()));

  handler_ = make_shared<EdenServiceHandler>(originalCommandLine_, this);
  server_->setInterface(handler_);

  // Get the path to the thrift socket.
  auto thriftSocketPath = edenDir_.getThriftSocketPath();
  folly::SocketAddress thriftAddress;
  thriftAddress.setFromPath(thriftSocketPath.view());
  server_->setAddress(thriftAddress);
  serverState_->setSocketPath(thriftSocketPath);

  serverEventHandler_ = make_shared<ThriftServerEventHandler>(this);
  server_->setServerEventHandler(serverEventHandler_);
  return serverEventHandler_->getThriftRunningFuture();
}

void EdenServer::prepareThriftAddress() const {
  // If we are serving on a local Unix socket, remove any old socket file
  // that may be left over from a previous instance.
  // We have already acquired the mount point lock at this time, so we know
  // that any existing socket is unused and safe to remove.
  const auto& path = getUnixDomainSocketPath(server_->getAddress());
  if (!path) {
    return;
  }

  auto sock = folly::AsyncServerSocket::UniquePtr(new folly::AsyncServerSocket);
  const auto addr = folly::SocketAddress::makeFromPath(*path);

  const size_t maxTries = folly::kIsWindows ? 3 : 1;

  auto tries = 1;
  while (true) {
    const int rc = unlink(path->c_str());
    if (rc != 0 && errno != ENOENT) {
      // This might happen if we don't have permission to remove the file.
      folly::throwSystemError(
          "unable to remove old Eden thrift socket ", *path);
    }

    try {
      sock->bind(addr);
      break;
    } catch (const std::system_error&) {
      if (tries == maxTries) {
        throw;
      }

      // On Windows, a race condition exist where an attempt to connect to a
      // non-existing socket will create it on-disk, preventing bind(2) from
      // succeeding, leading to the Thrift server failing to start.
      //
      // Since at this point we're holding the lock, no other edenfs process
      // should be attempting to bind to the socket, and thus it's safe to try
      // to remove it, and retry.

      tries++;
    }
  }

  server_->useExistingSocket(std::move(sock));
}

void EdenServer::armPrivHelperRestart() {
  uint64_t generation;
  {
    const auto state = runningState_.rlock();
    if (state->state == RunState::SHUTTING_DOWN) {
      XLOG(INFO, "not arming the privhelper: edenfs is already shutting down");
      return;
    }
    generation = state->restartArmGeneration;
  }

  restartArmer_.arm();

  auto state = runningState_.wlock();
  if (state->state == RunState::SHUTTING_DOWN ||
      state->restartArmGeneration != generation) {
    XLOGF(
        WARN,
        "removing the privhelper restart sentinel: {}",
        state->state == RunState::SHUTTING_DOWN
            ? "EdenFS is shutting down"
            : "the restart-arm generation changed");

    restartArmer_.removeSentinel();
  }
}

void EdenServer::markShuttingDownLocked(
    RunStateData& state,
    folly::StringPiece reason) {
  if (state.state == RunState::SHUTTING_DOWN) {
    return;
  }
  state.state = RunState::SHUTTING_DOWN;
  ++state.restartArmGeneration;

  // Deliberately not behind armed(): an in-flight arm can have created its
  // sentinel before the setRestartArgs response sets that flag.
  restartArmer_.removeSentinel();

  // The notification does wait for the flag. It is one-way, so a privhelper
  // too old to know the request would answer it, and the unmatched transaction
  // ID raises EDEN_BUG on the client.
  if (!restartArmer_.armed()) {
    return;
  }

  // Safe to send while holding the lock: the client only enqueues the message
  // onto its EventBase and returns.
  serverState_->getPrivHelper()->notifyCleanShutdown(reason);
}

void EdenServer::stop() {
  {
    auto state = runningState_.wlock();
    if (state->state == RunState::SHUTTING_DOWN) {
      // If we are already shutting down, we don't want to continue down this
      // code path in case we are doing a graceful restart. That could result in
      // a race which could trigger a failure in performCleanup.
      XLOG(INFO, "stop was called while server was already shutting down");
      return;
    }
    markShuttingDownLocked(*state, "stop");
  }

  handler_->logOutstandingThriftRequests("EdenServer::stop() called");
  handler_->cancelAllActiveRequests("EdenServer::stop() called");
  clearStartupStatusPublishers();
  shutdownSubscribers();
  server_->stop();
}

namespace {
// `eden restart` tells a refused takeover from a failed one by these words,
// which reach it through the new daemon's startup log. The CLI's
// _TAKEOVER_REFUSED_MARKER matches them.
constexpr std::string_view kTakeoverRefusedPhrase =
    "refused the graceful restart";
} // namespace

void EdenServer::noteActivity(std::string_view what) {
  recentActivity_.wlock()->emplace(
      RecentActivity{std::string{what}, std::chrono::steady_clock::now()});
}

bool EdenServer::beginRecoveryMount(const AbsolutePath& mountPath) {
  auto state = runningState_.wlock();
  if (state->state != RunState::RUNNING) {
    return false;
  }
  state->recoveryMounts[mountPath] = std::chrono::steady_clock::now();
  return true;
}

void EdenServer::endRecoveryMount(const AbsolutePath& mountPath) {
  runningState_.wlock()->recoveryMounts.erase(mountPath);
}

std::optional<std::string> EdenServer::refuseTakeoverForRecoveryMounts(
    const RunStateData& state,
    std::string_view why) {
  if (state.recoveryMounts.empty()) {
    return std::nullopt;
  }
  const auto now = std::chrono::steady_clock::now();
  std::string description;
  auto oldest = std::chrono::steady_clock::duration::zero();
  for (const auto& [mountPath, startedAt] : state.recoveryMounts) {
    if (!description.empty()) {
      description += ", ";
    }
    description += fmt::format(
        "remounting {} for {}", mountPath, durationToString(now - startedAt));
    oldest = std::max(oldest, now - startedAt);
  }
  XLOGF(WARN, "Refusing the graceful restart, {}: {}", why, description);
  serverState_->getStats()->increment(&TakeoverStats::sendFailure);
  edenFsEventsLogger_->logEvent(
      TakeoverRefused{
          description, std::chrono::duration<double>{oldest}.count()});
  return fmt::format(
      "EdenFS is recovering a checkout and {}: {}. Retry later.",
      kTakeoverRefusedPhrase,
      description);
}

folly::Future<TakeoverData> EdenServer::startTakeoverShutdown(
    bool force,
    bool onlyIfIdle) {
#ifndef _WIN32
  {
    auto state = runningState_.rlock();
    if (state->state != RunState::RUNNING) {
      serverState_->getStats()->increment(&TakeoverStats::sendFailure);
      return makeFuture<TakeoverData>(std::runtime_error(
          folly::to<string>(
              "can only perform graceful restart when running normally; "
              "current state is ",
              enumValue(state->state))));
    }
  }
  auto config = serverState_->getEdenConfig();
  if (force || !config->takeoverWaitsForRequests.getValue()) {
    return startTakeoverShutdownNow(force);
  }
  if (onlyIfIdle) {
    const auto window = config->takeoverRecentActivityWindow.getValue();
    const auto activity = *recentActivity_.rlock();
    const auto age = activity
        ? std::chrono::steady_clock::now() - activity->when
        : std::chrono::steady_clock::duration::max();
    if (age < window) {
      auto description =
          fmt::format("{} ran {} ago", activity->what, durationToString(age));
      XLOGF(
          WARN,
          "Refusing the graceful restart, EdenFS was in use recently: {}",
          description);
      serverState_->getStats()->increment(&TakeoverStats::sendFailure);
      edenFsEventsLogger_->logEvent(
          TakeoverRefused{
              description, std::chrono::duration<double>{age}.count()});
      return makeFuture<TakeoverData>(std::runtime_error(
          fmt::format(
              "EdenFS was in use recently and {}: {}. Retry later, or force "
              "the restart.",
              kTakeoverRefusedPhrase,
              description)));
    }
  }
  // Nothing changes while the requests in flight get their chance to finish;
  // the main event base keeps serving in the meantime.
  ++pendingTakeoverWaits_;
  folly::SemiFuture<folly::Unit> waited;
  try {
    waited = folly::coro::co_withExecutor(
                 getMainEventBase(),
                 folly::coro::co_withCancellation(
                     takeoverWaitCancellation_.getToken(),
                     co_waitForInFlightRequests()))
                 .start();
  } catch (...) {
    --pendingTakeoverWaits_;
    throw;
  }
  return std::move(waited)
      .via(getMainEventBase())
      .thenTry([this](folly::Try<folly::Unit>&& waited) {
        --pendingTakeoverWaits_;
        if (waited.hasException<folly::OperationCancelled>()) {
          throw std::runtime_error(
              "EdenFS stopped while the graceful restart was waiting for "
              "Thrift requests to finish");
        }
        waited.throwUnlessValue();
        return startTakeoverShutdownNow(/*force=*/false);
      });
#else
  NOT_IMPLEMENTED();
#endif
}

folly::coro::Task<void> EdenServer::co_waitForInFlightRequests() {
  const auto grace =
      serverState_->getEdenConfig()->takeoverRequestGrace.getValue();
  auto refuse =
      [this](
          const std::vector<EdenServiceHandler::LongRunningRequest>& requests,
          std::string_view why) {
        auto description = describeLongRunningRequests(
            requests, *serverState_->getProcessInfoCache());
        XLOGF(WARN, "Refusing the graceful restart, {}: {}", why, description);
        serverState_->getStats()->increment(&TakeoverStats::sendFailure);
        // getRequestsRunningLongerThan() lists the oldest request first.
        edenFsEventsLogger_->logEvent(
            TakeoverRefused{
                description,
                std::chrono::duration<double>{requests.front().age}.count()});
        throw std::runtime_error(
            fmt::format(
                "EdenFS is busy and {}: {}. Retry later, or force the restart "
                "to cancel the requests that support cancellation.",
                kTakeoverRefusedPhrase,
                description));
      };

  // A request that has already outlived the grace period is not worth
  // waiting for.
  auto outlived = handler_->getRequestsRunningLongerThan(grace);
  if (!outlived.empty()) {
    refuse(outlived, "requests have outlived the grace period");
  }
  auto inFlight =
      handler_->getRequestsRunningLongerThan(std::chrono::nanoseconds{0});
  auto recoveryMounts = [this] {
    return runningState_.rlock()->recoveryMounts.size();
  };
  // Lets a test change the requests in flight once they have been looked at.
  co_await serverState_->getFaultInjector().co_checkAsync(
      "takeover", "wait_for_requests");
  if (!inFlight.empty() || recoveryMounts() != 0) {
    XLOGF(
        INFO,
        "Graceful restart: giving {} Thrift requests and {} checkout recoveries up to {} to finish",
        inFlight.size(),
        recoveryMounts(),
        durationToString(grace));
    const auto deadline = std::chrono::steady_clock::now() + grace;
    const auto interval = std::min(
        std::chrono::duration_cast<folly::HighResDuration>(grace),
        folly::HighResDuration{std::chrono::seconds{1}});
    // The daemon keeps serving while it waits, so every poll looks at all the
    // requests in flight: a checkout that started during the wait must not be
    // cut off by the restart either. The deadline bounds the wait all the
    // same.
    do {
      // Lets a test change the requests in flight between two polls.
      co_await serverState_->getFaultInjector().co_checkAsync(
          "takeover_wait", "poll");
      co_await folly::coro::sleep(interval);
      inFlight =
          handler_->getRequestsRunningLongerThan(std::chrono::nanoseconds{0});
      if (std::chrono::steady_clock::now() >= deadline) {
        if (!inFlight.empty()) {
          refuse(inFlight, "requests did not finish within the grace period");
        }
        if (auto refusal = refuseTakeoverForRecoveryMounts(
                *runningState_.rlock(),
                "checkout recoveries did not finish within the grace period")) {
          throw std::runtime_error(*refusal);
        }
      }
    } while (!inFlight.empty() || recoveryMounts() != 0);
  }
  // Lets a test start a request once the wait is over, before the Thrift
  // server stops.
  co_await serverState_->getFaultInjector().co_checkAsync(
      "takeover", "after_wait");
}

folly::Future<TakeoverData> EdenServer::startTakeoverShutdownNow(bool force) {
#ifndef _WIN32
  // Track how long the takeover send takes and whether it succeeds.
  folly::stop_watch<std::chrono::microseconds> takeoverSendWatch;

  // Make sure we aren't already shutting down, then update our state
  // to indicate that we should perform mount point takeover shutdown
  // once runServer() returns.
  folly::Promise<std::optional<TakeoverData>> takeoverPromise;
  folly::File thriftSocket;
  {
    auto state = runningState_.wlock();
    if (state->state != RunState::RUNNING) {
      // We are either still in the process of starting,
      // or already shutting down.
      serverState_->getStats()->increment(&TakeoverStats::sendFailure);
      return makeFuture<TakeoverData>(std::runtime_error(
          folly::to<string>(
              "can only perform graceful restart when running normally; "
              "current state is ",
              enumValue(state->state))));
    }

    // A recovery mount registers under this lock while RUNNING, so none can
    // start once this check passes.
    if (auto refusal = refuseTakeoverForRecoveryMounts(
            *state, "a checkout recovery is mounting")) {
      return makeFuture<TakeoverData>(std::runtime_error(*refusal));
    }

    {
      const auto mountPoints = mountPoints_->rlock();
      for (const auto& entry : *mountPoints) {
        const auto& mount = entry.second.edenMount;
        if (!mount->isSafeForInodeAccess()) {
          serverState_->getStats()->increment(&TakeoverStats::sendFailure);
          return makeFuture<TakeoverData>(std::runtime_error(
              fmt::format(
                  "can only perform graceful restart when all mount points are "
                  "initialized; mount {} is in state {}",
                  mount->getPath(),
                  apache::thrift::util::enumNameSafe(mount->getState()))));
          // TODO(xavierd): There is still a potential race after this check if
          // a mount is initiated at this point. Injecting a block below and
          // starting a mount would manifest it. In practice, this should be
          // fairly rare. Moving this further (in stopMountsForTakeover for
          // instance) to avoid this race requires EdenFS to being able to
          // gracefully handle failures and recover in these cases by restarting
          // several components after they have been already shutdown.
        }
      }
    }

    // We should confirm that no garbage collection is running during graceful
    // restart.
    if (!stopAllGarbageCollections(
            /*maxRetries=*/3, /*retryInterval=*/std::chrono::seconds(1))) {
      // We are still waiting for the GC to finish. This is unexpected and
      // should not happen. We should not proceed with the graceful restart.
      serverState_->getStats()->increment(&TakeoverStats::sendFailure);
      return makeFuture<TakeoverData>(std::runtime_error(
          "cannot run graceful restart while garbage collection is running"));
    }
    // Note: If anything fails after this point, and we fallback to the old
    // eden, we don't need to worry about updating the gcTask_ interval.
    // Updating gcTask_ interval is part of the reloadConfig() periodic task
    // which is running every few minutes.

    // Make a copy of the thrift server socket so we can transfer it to the
    // new edenfs process.  Our local thrift will close its own socket when
    // we stop the server.  The easiest way to avoid completely closing the
    // server socket for now is simply by duplicating the socket to a new
    // fd. We will transfer this duplicated FD to the new edenfs process.
    const int takeoverThriftSocket = dup(server_->getListenSocket());
    folly::checkUnixError(
        takeoverThriftSocket,
        "error duplicating thrift server socket during graceful takeover");
    thriftSocket = folly::File{takeoverThriftSocket, /* ownsFd */ true};

    // The TakeoverServer will fulfill shutdownFuture with std::nullopt
    // upon successfully sending the TakeoverData, or with the TakeoverData
    // if the takeover was unsuccessful.
    XCHECK(!state->shutdownFuture.valid());
    state->shutdownFuture = takeoverPromise.getFuture();
    markShuttingDownLocked(*state, "graceful restart");
  }

  return serverState_->getFaultInjector()
      .checkAsync("takeover", "server_shutdown")
      .semi()
      .via(getMainEventBase())
      .thenValue([this, force](auto&&) {
        // The Thrift server cannot stop while a request or a stream keeps a
        // connection busy. A forced takeover cancels everything. An unforced
        // one has waited for the requests that were in flight, so it only
        // ends the trace streams, which stay open until their client leaves,
        // and arms a cancellation for the requests that arrived during the
        // wait: one of those may be long too, and left alone it would hold
        // the Thrift stop to its deadline, which ends the process.
        if (force) {
          handler_->cancelAllActiveRequests("forced graceful restart");
        } else {
          handler_->closeTraceStreams();
          auto config = serverState_->getEdenConfig();
          if (config->takeoverWaitsForRequests.getValue()) {
            handler_->cancelActiveRequestsAfter(
                config->takeoverRequestGrace.getValue(),
                "graceful restart: the request grace period expired");
          }
        }
        shutdownSubscribers();

        // Stop the thrift server. In the future, we'd like to
        // keep listening and instead start internally queueing thrift calls
        // to pass with the takeover data below, while waiting here for
        // currently processing thrift calls to finish.
        server_->stop();
      })
      .thenTry([this, takeoverPromise = std::move(takeoverPromise)](
                   auto&& t) mutable {
        if (t.hasException()) {
          takeoverPromise.setException(t.exception());
          t.throwUnlessValue();
        }
        return stopMountsForTakeover(std::move(takeoverPromise));
      })
      .thenTry(
          [this, socket = std::move(thriftSocket)](
              folly::Try<TakeoverData>&& result) mutable
              -> folly::Future<TakeoverData> {
            if (result.hasException()) {
              return makeFuture<TakeoverData>(result.exception());
            }

            auto takeover = std::move(result).value();
            takeover.lockFile = edenDir_.extractLock();
            takeover.thriftSocket = std::move(socket);
            // Mountd is stopped on the NFS EventBase, which owns its
            // sockets; the rest of the preparation stays on the main one.
            auto& nfsServer = this->getServerState()->getNfsServer();
            return via(nfsServer ? nfsServer->getEventBase()
                                 : getMainEventBase())
                .thenValue(
                    [this](auto&&)
                        -> folly::SemiFuture<std::optional<folly::File>> {
                      if (auto& takeoverServer =
                              this->getServerState()->getNfsServer()) {
                        return takeoverServer->takeoverStop().deferValue(
                            [](folly::File file) {
                              return std::make_optional<folly::File>(
                                  std::move(file));
                            });
                      } else {
                        return std::nullopt;
                      }
                    })
                .via(getMainEventBase())
                .thenTry(
                    [this, takeover = std::move(takeover)](
                        folly::Try<std::optional<folly::File>>&&
                            mountdSocket) mutable
                        -> folly::Future<TakeoverData> {
                      takeover.mountdAcceptsPaused =
                          this->getServerState()->getNfsServer() != nullptr;
                      if (mountdSocket.hasException()) {
                        return recoverPreparedTakeoverFromError(
                            mountdSocket.exception(), std::move(takeover));
                      }

                      if (mountdSocket->has_value()) {
                        XLOGF(
                            DBG7,
                            "Got mountd Socket for takeover {}",
                            mountdSocket->value().fd());
                      }
                      takeover.mountdServerSocket =
                          std::move(mountdSocket).value();

                      auto postPrepareResult =
                          serverState_->getFaultInjector().checkTry(
                              "takeover", "post_prepare_data");
                      if (postPrepareResult.hasException()) {
                        return recoverPreparedTakeoverFromError(
                            postPrepareResult.exception(), std::move(takeover));
                      }
                      return makeFuture<TakeoverData>(std::move(takeover));
                    });
          })
      .thenTry([this,
                takeoverSendWatch](folly::Try<TakeoverData>&& result) mutable {
        auto& stats = serverState_->getStats();
        stats->addDuration(&TakeoverStats::send, takeoverSendWatch.elapsed());
        if (result.hasException()) {
          stats->increment(&TakeoverStats::sendFailure);
        }
        return result;
      });
#else
  NOT_IMPLEMENTED();
#endif // !_WIN32
}

void EdenServer::shutdownSubscribers() {
  // TODO: Set a flag in handler_ to reject future subscription requests.
  // Alternatively, have them seamless transfer through takeovers.

  handler_->beginStreamJournalChangedShutdown();

  // If we have any subscription sessions from watchman, we want to shut
  // those down now, otherwise they will block the server_->stop() call
  // below
  XLOG(DBG1, "cancel all subscribers prior to stopping thrift");
  std::vector<std::shared_ptr<EdenMount>> mounts;
  {
    auto mountPoints = mountPoints_->rlock();
    mounts.reserve(mountPoints->size());
    for (auto& entry : *mountPoints) {
      mounts.push_back(entry.second.edenMount);
    }
  }
  for (auto& mount : mounts) {
    mount->getJournal().cancelAllSubscribers();
  }
}

const EdenStatsPtr& EdenServer::getStats() const {
  return serverState_->getStats();
}

void EdenServer::flushStatsNow() {
  serverState_->getStats()->flush();

  // Sapling ODS counters are buffered somewhat in memory - flush them now.
  SaplingBackingStore::flushCounters();
}

#ifdef __linux__
void EdenServer::scheduleCgroupFileCacheReclaim() {
  auto state = cgroupFileCacheReclaimState_;
  if (state->kernelUnsupported.load(std::memory_order_acquire)) {
    return;
  }
  // memory.reclaim is synchronous, and a pass that outlasts the interval must
  // not pile further passes up behind it.
  if (state->inFlight.exchange(true, std::memory_order_acq_rel)) {
    return;
  }
  // Cleared by the pass once it has run; released here only if it is never
  // handed to the pass.
  auto releaseInFlight = folly::makeGuard(
      [state] { state->inFlight.store(false, std::memory_order_release); });

  const auto config = serverState_->getEdenConfig();
  const CgroupFileCacheReclaimOptions options{
      .targetBytes = config->cgroupFileCacheTargetBytes.getValue(),
      .maxReclaimBytes = config->cgroupFileCacheMaxReclaimBytes.getValue(),
  };
  // Either systemd mode places the daemon in a cgroup named edenfs*, so not
  // being in one while a mode is configured means the daemon was started
  // some other way and is worth noticing.
  const bool isolationConfigured = config->systemdCgroupIsolation.getValue() ||
      config->systemdManagedLifecycle.getValue();

  getCgroupFileCacheReclaimThreadPool()->add(
      [state,
       stats = serverState_->getStats().copy(),
       options,
       isolationConfigured,
       releaseInFlight = std::move(releaseInFlight)] {
        if (!state->enabled.load(std::memory_order_acquire)) {
          return;
        }
        try {
          folly::stop_watch<std::chrono::milliseconds> watch;
          const auto result = state->reclaimer.reclaim(options);
          if (result.requestedBytes == 0) {
            return;
          }
          const auto reclaimed =
              result.fileCacheBytesBefore > result.fileCacheBytesAfter
              ? result.fileCacheBytesBefore - result.fileCacheBytesAfter
              : 0;
          stats->increment(&CgroupFileCacheStats::reclaimedBytes, reclaimed);
          // Passes that drop a lot are rare enough to be worth seeing in the
          // default log; the steady-state trickle is not.
          constexpr uint64_t kNotableReclaimBytes = 1ULL << 30;
          const auto message = fmt::format(
              "Reclaimed {} of {} requested bytes of cgroup file cache in "
              "{} ms; active_file + inactive_file went from {} to {} bytes",
              reclaimed,
              result.requestedBytes,
              watch.elapsed().count(),
              result.fileCacheBytesBefore,
              result.fileCacheBytesAfter);
          if (reclaimed >= kNotableReclaimBytes) {
            XLOG(INFO, message);
          } else {
            XLOG(DBG2, message);
          }
        } catch (const NotEdenFsCgroupError& ex) {
          if (isolationConfigured) {
            stats->increment(&CgroupFileCacheStats::reclaimFailures);
            XLOGF_EVERY_MS(
                ERR,
                60'000,
                "systemd cgroup isolation is configured but EdenFS is not "
                "running in an EdenFS cgroup; not reclaiming file cache: {}",
                ex.what());
          } else {
            XLOGF_EVERY_MS(
                DBG2, 60'000, "Not reclaiming file cache: {}", ex.what());
          }
        } catch (const UnsupportedKernelError& ex) {
          state->kernelUnsupported.store(true, std::memory_order_release);
          stats->increment(&CgroupFileCacheStats::reclaimFailures);
          XLOGF(
              ERR,
              "Disabling cgroup file-cache reclaim until the next restart: {}",
              ex.what());
        } catch (const std::exception& ex) {
          stats->increment(&CgroupFileCacheStats::reclaimFailures);
          XLOGF_EVERY_MS(
              ERR,
              60'000,
              "Unable to reclaim the EdenFS cgroup file cache: {}",
              folly::exceptionStr(ex));
        }
      });
}
#endif

void EdenServer::refreshBackingStore() {
  std::vector<shared_ptr<BackingStore>> backingStores;
  {
    auto lockedStores = backingStores_.wlock();
    backingStores.reserve(lockedStores->size());
    for (auto& [key, backingStore] : *lockedStores) {
      backingStores.emplace_back(backingStore);
    }
  }

  for (auto& store : backingStores) {
    store->periodicManagementTask();
  }
}

namespace {
/**
 * Directories pinned as process cwds/roots, which pressure GC must not
 * invalidate (their ancestors are protected by the GC traversal itself; see
 * TreeInode::FuseGcResult). nullptr means pin information is unavailable, in
 * which case no directory entries are invalidated at all.
 */
using PinnedInodeSet = std::shared_ptr<const folly::F14FastSet<InodeNumber>>;

ImmediateFuture<uint64_t> garbageCollectInodesWithLease(
    EdenMount& mount,
    TreeInodePtr inode,
    std::chrono::system_clock::time_point cutoff,
    const ObjectFetchContextPtr& context,
    bool pressureBased,
    EdenMount::InodeGCLease lease,
    folly::CancellationToken shutdownToken,
    std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger,
    PinnedInodeSet pinnedInodes,
    bool pinScanFailed) {
  struct InodeGCResult {
    uint64_t numInvalidated;
    size_t numUnloaded;
    size_t zeroFsRefTreesRetained;
    /** The run was cancelled before or during its sweep, which the sweep
     * then cut short, so what it reclaimed says nothing about the run. */
    bool cancelled;
  };

  folly::stop_watch<> inodeGCRuntime;

  auto mountPath = mount.getPath();
  auto inodeCountsBeforeGC = mount.getInodeMap()->getInodeCounts();
  auto totalNumberOfInodesBeforeGC = inodeCountsBeforeGC.fileCount +
      inodeCountsBeforeGC.treeCount + inodeCountsBeforeGC.unloadedInodeCount;
  XLOGF(
      DBG4,
      "Starting {} GC for: {} total number of inodes {}",
      pressureBased ? "pressure-based" : "config-based",
      mountPath,
      totalNumberOfInodesBeforeGC);
  auto gcToken = folly::cancellation_token_merge(
      shutdownToken, lease.getCancellationToken());
  auto keepRememberedParentTreesLoaded =
      pressureBased && mount.getFuseChannel() != nullptr;
  // The sweep that follows applies the same cutoff, so an inode without a
  // kernel reference that a status or Thrift request used recently is kept
  // instead of being unloaded on every run.
  std::optional<EdenTimestamp> sweepCutoff;
  if (mount.getServerState()->getEdenConfig()->gcSweepHonorsCutoff.getValue()) {
    sweepCutoff = EdenTimestamp{folly::to<timespec>(cutoff)};
  }
  return inode
      // First step of garbage collection varies by platform (e.g., Linux,
      // macOS, Windows)
      ->handleChildrenNotAccessedRecently(
          cutoff, context, pressureBased, gcToken, std::move(pinnedInodes))
      // Wait for queued filesystem invalidations to drain before the unload
      // sweep. On FUSE, invalidation-triggered FORGETs may arrive too late for
      // this GC cycle if we start unloading immediately.
      .thenTry(
          [&mount, gcToken](folly::Try<uint64_t>&& invalidatedTry)
              -> ImmediateFuture<uint64_t> {
            if (invalidatedTry.hasException()) {
              return makeFuture<uint64_t>(invalidatedTry.exception());
            }
            if (gcToken.isCancellationRequested()) {
              return invalidatedTry.value();
            }
            return mount.flushInvalidations().thenValue(
                [numInvalidated = invalidatedTry.value()](folly::Unit) {
                  return numInvalidated;
                });
          })
      // Second step of garbage collection deletes all the unreferenced inodes
      .thenTry(
          [inode, gcToken, keepRememberedParentTreesLoaded, sweepCutoff](
              folly::Try<uint64_t>&& invalidatedTry) -> InodeGCResult {
            size_t numUnloaded = 0;
            size_t zeroFsRefTreesRetained = 0;
            bool cancelled = gcToken.isCancellationRequested();
            if (!cancelled) {
              if (keepRememberedParentTreesLoaded) {
                auto unloadResult =
                    inode->unloadChildrenUnreferencedByFsForInodeGC(
                        gcToken, sweepCutoff);
                numUnloaded = unloadResult.unloaded;
                zeroFsRefTreesRetained = unloadResult.zeroFsRefTreesRetained;
              } else {
                numUnloaded =
                    inode->unloadChildrenUnreferencedByFs(gcToken, sweepCutoff);
              }
              // The sweep stops early once cancelled, so a partial count
              // must not be judged either.
              cancelled = gcToken.isCancellationRequested();
            }
            return {
                invalidatedTry.value(),
                numUnloaded,
                zeroFsRefTreesRetained,
                cancelled};
          })
      .thenTry([inodeGCRuntime,
                edenFsEventsLogger = std::move(edenFsEventsLogger),
                &mount,
                mountPath,
                inodeMap = mount.getInodeMap(),
                forgottenBeforeGC = inodeCountsBeforeGC.forgottenInodeCount,
                totalNumberOfInodesBeforeGC,
                pressureBased,
                pinScanFailed](folly::Try<InodeGCResult> resultTry) {
        auto runtime = std::chrono::duration<double>{inodeGCRuntime.elapsed()};

        bool success = resultTry.hasValue();
        int64_t numInvalidated =
            success ? folly::to_signed(resultTry.value().numInvalidated) : 0;
        size_t numUnloaded = success ? resultTry.value().numUnloaded : 0;
        size_t zeroFsRefTreesRetained =
            success ? resultTry.value().zeroFsRefTreesRetained : 0;
        auto inodeCountsAfterGC = inodeMap->getInodeCounts();
        // Remembered inodes forgotten while the run was in progress. The
        // count is mount-wide, but attributable to the run: on NFS only GC
        // clears FS references, and on FUSE the sweep erases zero-reference
        // inodes rather than remembering them, so nothing is counted twice.
        // A FORGET the kernel sends for other reasons meanwhile, or
        // InodeMap::forgetStaleInodes() running, only makes the run look
        // healthier, never stalled.
        auto numForgotten =
            inodeCountsAfterGC.forgottenInodeCount - forgottenBeforeGC;
        auto totalNumberOfInodesAfterGC = inodeCountsAfterGC.fileCount +
            inodeCountsAfterGC.treeCount +
            inodeCountsAfterGC.unloadedInodeCount;
        auto inodeDelta = folly::to_signed(totalNumberOfInodesBeforeGC) -
            folly::to_signed(totalNumberOfInodesAfterGC);

        edenFsEventsLogger->logEvent(
            WorkingCopyGc{
                runtime.count(), numInvalidated, success, inodeDelta});
        auto shouldLogAtDbg2 = runtime > std::chrono::seconds{5} ||
            numInvalidated > 10'000 || inodeDelta > 10'000 ||
            zeroFsRefTreesRetained > 10'000;
        auto logMessage = [&] {
          return fmt::format(
              "{} GC for: {}, completed in: {} seconds, "
              "invalidated: {}, unloaded: {}, forgotten: {}, "
              "inodes before: {}, inodes after: {}, "
              "fsRefCount==0 trees retained: {}",
              pressureBased ? "Pressure-based" : "Config-based",
              mountPath,
              runtime.count(),
              numInvalidated,
              numUnloaded,
              numForgotten,
              totalNumberOfInodesBeforeGC,
              totalNumberOfInodesAfterGC,
              zeroFsRefTreesRetained);
        };
        if (shouldLogAtDbg2) {
          XLOG(DBG2) << logMessage();
        } else {
          XLOG(DBG4) << logMessage();
        }

        // A cancelled run skipped or cut short its sweep, so it has nothing
        // to judge.
        if (success && pressureBased && !resultTry.value().cancelled) {
          mount.recordPressureGcOutcome(
              resultTry.value().numInvalidated,
              numUnloaded + numForgotten,
              pinScanFailed);
        }

        return resultTry.value().numInvalidated;
      })
      // The lease outlives the outcome recording so a run that starts after
      // this one cannot have its backoff cleared by this run's result.
      .ensure([lease = std::move(lease)] {});
}

#if defined(__linux__) || defined(__APPLE__)
struct PinScanData {
  PinScanReport report;
  folly::F14FastMap<std::string, uint64_t> devByMountPoint;
};

/**
 * Whether pressure-based GC on this mount consults the pin scan: FUSE mounts
 * on Linux, NFS mounts on macOS.
 */
bool usesPinScan(EdenMount& mount) {
#ifdef __linux__
  return mount.getFuseChannel() != nullptr;
#else
  return mount.getNfsdChannel() != nullptr;
#endif
}

/**
 * Run the privhelper's pin scan and map this daemon's mounts to the devices
 * it reports. Returns std::nullopt if the scan could not be completed, in
 * which case pressure GC must not invalidate any directory entries.
 */
std::optional<PinScanData> runPinnedInodeScan(
    const folly::CancellationToken& cancellationToken,
    const EdenFsEventsLogger& edenFsEventsLogger,
    std::chrono::nanoseconds timeout) {
  std::string helperPath = FLAGS_privhelper_path;
  if (helperPath.empty()) {
    helperPath =
        (executablePath().dirname() + "edenfs_privhelper"_relpath).asString();
  }
  auto report = runPinScan(
      helperPath,
      cancellationToken,
      std::chrono::duration_cast<std::chrono::milliseconds>(timeout));
  if (!report) {
    if (report.error().reason != "cancelled") {
      edenFsEventsLogger.logEvent(report.error());
    }
    return std::nullopt;
  }
  PinScanData data;
  data.report = std::move(*report);

#ifdef __linux__
  auto mounts = getAllMounts();
#else
  auto mounts = listMountsForPinScan();
#endif
  if (mounts.hasError()) {
    XLOGF(
        WARN,
        "unable to map mounts to devices for pin scan: {}",
        folly::errnoStr(mounts.error()));
    edenFsEventsLogger.logEvent(
        PinScanFailure{"mounts_unreadable", folly::errnoStr(mounts.error())});
    return std::nullopt;
  }
  for (const auto& mount : mounts.value()) {
#ifdef __linux__
    data.devByMountPoint[mount.mountPoint] =
        makedev(mount.devMajor, mount.devMinor);
#else
    data.devByMountPoint[mount.mountPoint] = mount.dev;
#endif
  }
  return data;
}

/**
 * Build the pinned inode set for one mount from scan results. Returns
 * nullptr (meaning "pins unknown, do not invalidate directories") if the
 * scan failed or the mount cannot be mapped to a device.
 */
PinnedInodeSet buildPinnedInodeSet(
    EdenMount& mount,
    const PinScanData* scan,
    const EdenFsEventsLogger& edenFsEventsLogger) {
  if (scan == nullptr) {
    return nullptr;
  }
  auto devIter = scan->devByMountPoint.find(mount.getPath().asString());
  if (devIter == scan->devByMountPoint.end() ||
      !scan->report.scannedDevices.count(devIter->second)) {
    // The scanner applies its own mount table filter, so a mount it did not
    // cover has unknown pins; a mismatch must fail safe, not read as "no
    // pins".
    XLOGF_EVERY_MS(
        WARN,
        60'000,
        "pin scan did not cover mount {}; skipping directory invalidation",
        mount.getPath());
    edenFsEventsLogger.logEvent(
        PinScanFailure{"mount_not_covered", mount.getPath().asString()});
    return nullptr;
  }
  auto pins = std::make_shared<folly::F14FastSet<InodeNumber>>();
  auto pinsIter = scan->report.pinsByDevice.find(devIter->second);
  if (pinsIter != scan->report.pinsByDevice.end()) {
    for (auto ino : pinsIter->second) {
      pins->insert(InodeNumber{ino});
    }
  }
  return pins;
}
#endif // __linux__ || __APPLE__

} // namespace

ImmediateFuture<uint64_t> EdenServer::garbageCollectInodes(
    EdenMount& mount,
    TreeInodePtr inode,
    std::chrono::system_clock::time_point cutoff,
    const ObjectFetchContextPtr& context,
    bool pressureBased) {
  auto lease = mount.tryStartInodeGC(pressureBased);
  if (!lease) {
    XLOGF(
        DBG6,
        "Not running GC for: {}, GC is already running or checkout holds its lease",
        mount.getPath());
    return ImmediateFuture<uint64_t>{folly::Try<uint64_t>{newEdenError(
        EBUSY,
        EdenErrorType::POSIX_ERROR,
        "inode GC is already running or inhibited by checkout")}};
  }

  PinnedInodeSet pinnedInodes;
  bool pinScanFailed = false;
#if defined(__linux__) || defined(__APPLE__)
  auto config = serverState_->getReloadableConfig()->getEdenConfig();
  if (pressureBased && usesPinScan(mount) &&
      config->pressureBasedGcScanPins.getValue()) {
    const auto& edenFsEventsLogger = *serverState_->getEdenFsEventsLogger();
    auto scan = runPinnedInodeScan(
        folly::cancellation_token_merge(
            gcCancelSource_.rlock()->getToken(), lease->getCancellationToken()),
        edenFsEventsLogger,
        config->pressureBasedGcPinScanTimeout.getValue());
    pinnedInodes = buildPinnedInodeSet(
        mount, scan ? &scan.value() : nullptr, edenFsEventsLogger);
    pinScanFailed = pinnedInodes == nullptr;
  }
#endif
  return garbageCollectInodesWithLease(
      mount,
      std::move(inode),
      cutoff,
      context,
      pressureBased,
      std::move(*lease),
      gcCancelSource_.rlock()->getToken(),
      serverState_->getEdenFsEventsLogger(),
      std::move(pinnedInodes),
      pinScanFailed);
}

void EdenServer::garbageCollectAllMounts() {
  auto config = serverState_->getReloadableConfig()->getEdenConfig();
  auto cutoffConfig =
      std::chrono::duration_cast<std::chrono::system_clock::duration>(
          config->gcCutoff.getValue());
  auto shorterCutoffConfig =
      std::chrono::duration_cast<std::chrono::system_clock::duration>(
          config->aggressiveGcCutoff.getValue());
  auto pressureBasedGc = config->enablePressureBasedGc.getValue();
  auto steadyNow = std::chrono::steady_clock::now();
  std::chrono::system_clock::time_point cutoff;

  auto mountPoints = getMountPoints();
  if (pressureBasedGc) {
    PathMap<std::chrono::steady_clock::time_point, AbsolutePath>
        activePressureBasedGcTimes{kPathMapDefaultCaseSensitive};
    for (auto& mountHandle : mountPoints) {
      auto& mount = mountHandle.getEdenMount();
      auto lastGcTime = lastPressureBasedGcTimes_.find(mount.getPath());
      if (lastGcTime != lastPressureBasedGcTimes_.end()) {
        activePressureBasedGcTimes.emplace(mount.getPath(), lastGcTime->second);
      }
    }
    lastPressureBasedGcTimes_ = std::move(activePressureBasedGcTimes);
  } else {
    lastPressureBasedGcTimes_.clear();
  }

  struct DueMountGC {
    EdenMountHandle mountHandle;
    std::chrono::system_clock::time_point cutoff;
    EdenMount::InodeGCLease lease;
  };
  std::vector<DueMountGC> dueMounts;
  for (auto& mountHandle : mountPoints) {
    auto& mount = mountHandle.getEdenMount();
    if (pressureBasedGc) {
      // Use the pressure-based policy to compute a dynamic cutoff based on
      // the current inode count for this mount.
      auto policy = mount.getInodePressurePolicy();
      auto inodeCount = mount.getInodeMap()->getTotalInodeCountFast();
      auto gcPeriod = policy->getGcPeriod(inodeCount);
      if (mount.isPressureGcBackedOff()) {
        // Pressure GC is not reclaiming the inodes it invalidates (e.g.
        // EdenFS is tracking FS refcounts the kernel no longer holds, so
        // invalidations produce no FORGETs), a run was cancelled for
        // repeated tree-load failures, or the pin scan failed and the run
        // could not touch directories. Rerunning at the pressure-derived
        // rate is wasted work, so fall back to the regular GC cadence until
        // a run makes progress again.
        gcPeriod = std::max(
            gcPeriod,
            std::chrono::duration_cast<std::chrono::seconds>(
                config->gcPeriod.getValue()));
      }
      auto lastGcTime = lastPressureBasedGcTimes_.find(mount.getPath());
      if (lastGcTime != lastPressureBasedGcTimes_.end() &&
          steadyNow - lastGcTime->second < gcPeriod) {
        XLOGF(
            DBG6,
            "Skipping pressure-based GC for: {}, next run in {} seconds",
            mount.getPath(),
            std::chrono::duration_cast<std::chrono::seconds>(
                gcPeriod - (steadyNow - lastGcTime->second))
                .count());
        continue;
      }
      auto gcCutoffDuration = policy->getGcCutoff(inodeCount);

      if constexpr (folly::kIsWindows) {
        // On Windows, PrjFS GC uses on-disk atime which is only updated
        // ~hourly. A cutoff shorter than 1 hour would incorrectly treat
        // recently-used files as stale.
        gcCutoffDuration =
            std::max(gcCutoffDuration, std::chrono::seconds{3600});
      }

      cutoff = std::chrono::system_clock::now() - gcCutoffDuration;
    } else {
      auto inodeCountsBeforeGc = mount.getInodeMap()->getInodeCounts();
      auto totalNumberOfInodesBeforeGc = inodeCountsBeforeGc.fileCount +
          inodeCountsBeforeGc.treeCount +
          inodeCountsBeforeGc.unloadedInodeCount;
      auto aggressiveGcThreshold = config->aggressiveGcThreshold.getValue();
      // If aggressiveGcThreshold is set to 0, we will not run GC with shorter
      // cutoff.
      if (aggressiveGcThreshold > 0 &&
          totalNumberOfInodesBeforeGc > aggressiveGcThreshold) {
        // If the number of inodes is above the threshold, we want to run
        // GC with shorter cutoff.
        cutoff = std::chrono::system_clock::now() - shorterCutoffConfig;
      } else {
        cutoff = std::chrono::system_clock::now() - cutoffConfig;
      }
    }

    auto lease = mount.tryStartInodeGC(pressureBasedGc);
    if (!lease) {
      XLOGF(
          DBG6,
          "Skipping GC for: {}, GC is already running or checkout holds its lease",
          mount.getPath());
      continue;
    }
    if (pressureBasedGc) {
      lastPressureBasedGcTimes_[mount.getPath()] = steadyNow;
    }

    dueMounts.push_back(DueMountGC{mountHandle, cutoff, std::move(*lease)});
  }
  if (dueMounts.empty()) {
    return;
  }

  auto shutdownToken = gcCancelSource_.rlock()->getToken();
  auto edenFsEventsLogger = serverState_->getEdenFsEventsLogger();
  auto scanPins = pressureBasedGc && config->pressureBasedGcScanPins.getValue();
  auto pinScanTimeout = config->pressureBasedGcPinScanTimeout.getValue();
  auto* threadPool = getServerState()->getThreadPool().get();
  // Discover pinned directories once for all due mounts, then launch each
  // mount's GC. Both the pin scan (a subprocess with a deadline) and the GC
  // runs happen on the thread pool; only the scheduling decisions above run
  // on the main EventBase.
  folly::via(
      threadPool,
      [dueMounts = std::move(dueMounts),
       pressureBasedGc,
       scanPins,
       pinScanTimeout,
       cutoffConfig,
       shutdownToken = std::move(shutdownToken),
       edenFsEventsLogger = std::move(edenFsEventsLogger),
       threadPool]() mutable {
        // Consumed only where pin scans exist.
        (void)scanPins;
        (void)pinScanTimeout;
        (void)cutoffConfig;
#if defined(__linux__) || defined(__APPLE__)
        std::optional<PinScanData> scan;
        if (scanPins) {
          bool anyMountUsesPins = false;
          for (auto& dueMount : dueMounts) {
            anyMountUsesPins = anyMountUsesPins ||
                usesPinScan(dueMount.mountHandle.getEdenMount());
          }
          if (anyMountUsesPins) {
            scan = runPinnedInodeScan(
                shutdownToken, *edenFsEventsLogger, pinScanTimeout);
          }
        }
#endif
        for (auto& dueMount : dueMounts) {
          auto& mount = dueMount.mountHandle.getEdenMount();
          auto cutoff = dueMount.cutoff;
          PinnedInodeSet pinnedInodes;
          bool pinScanFailed = false;
#if defined(__linux__) || defined(__APPLE__)
          if (scanPins && usesPinScan(mount)) {
            pinnedInodes = buildPinnedInodeSet(
                mount, scan ? &scan.value() : nullptr, *edenFsEventsLogger);
            pinScanFailed = pinnedInodes == nullptr;
          }
          if (pinScanFailed && mount.getNfsdChannel() != nullptr) {
            // NFS gives EdenFS no reference for a file a process holds open;
            // the pins were how this run would have known to keep it. Without
            // them the pressure cutoff, seconds old at the high end, would
            // forget files in active use and hand their users ESTALE. Keep to
            // the regular cutoff, forgetting only what the periodic GC would.
            cutoff = std::chrono::system_clock::now() - cutoffConfig;
            XLOGF_EVERY_MS(
                WARN,
                60'000,
                "pin scan failed; pressure GC for {} uses the regular "
                "garbage-collection-cutoff this run",
                mount.getPath());
          }
#endif
          folly::via(
              threadPool,
              [mountHandle = dueMount.mountHandle,
               cutoff,
               pressureBasedGc,
               lease = std::move(dueMount.lease),
               shutdownToken,
               edenFsEventsLogger,
               pinnedInodes = std::move(pinnedInodes),
               pinScanFailed]() mutable {
                return garbageCollectInodesWithLease(
                           mountHandle.getEdenMount(),
                           mountHandle.getRootInode(),
                           cutoff,
                           ObjectFetchContext::getNullInodeGcContext(),
                           pressureBasedGc,
                           std::move(lease),
                           std::move(shutdownToken),
                           std::move(edenFsEventsLogger),
                           std::move(pinnedInodes),
                           pinScanFailed)
                    .semi();
              })
              .ensure([mountHandle = dueMount.mountHandle] {});
        }
      });
}

bool EdenServer::stopAllGarbageCollections(
    uint8_t maxRetries,
    std::chrono::seconds retryInterval) {
  // First stop the periodic GC - must run on EventBase thread
  // This should be cheap, so we just block on this to finish.
  auto stopPeriodicGC = [this]() {
    gcTask_.updateInterval(std::chrono::seconds(0));
  };
  if (mainEventBase_->inRunningEventBaseThread() ||
      mainEventBase_ ==
          folly::EventBaseManager::get()->getExistingEventBase()) {
    stopPeriodicGC();
  } else {
    mainEventBase_->runInEventBaseThreadAndWait(std::move(stopPeriodicGC));
  }

  // By default, folly futures and thread pools do not provide a built-in way
  // to forcibly stop a running task. We can cancel any running GC and wait
  // for them to stop. This is not guaranteed to work if the GC is stuck in
  // some operation, but it should work in most cases.
  gcCancelSource_.wlock()->requestCancellation();
  XLOGF(DBG1, "Cancel request sent to all ongoing garbage collections");

  bool isGCRunning = isInodeGCRunningForAnyMount();
  uint8_t currentAttempts = 0;

  while (isGCRunning && currentAttempts < maxRetries) {
    std::this_thread::sleep_for(retryInterval);
    isGCRunning = isInodeGCRunningForAnyMount();
    currentAttempts++;
  }

  const auto gcStopped = !isGCRunning;
  *gcCancelSource_.wlock() = folly::CancellationSource{};
  return gcStopped;
}

bool EdenServer::isInodeGCRunningForAnyMount() const {
  auto mountPoints = getMountPoints();
  for (auto& mountHandle : mountPoints) {
    if (mountHandle.getEdenMount().isInodeGCRunning()) {
      return true;
    }
  }
  return false;
}

void EdenServer::scheduleRunningMountHealthCheck(
    const AbsolutePath& mountPath,
    std::string repoSource,
    std::weak_ptr<EdenMount> mount) {
  auto mountPathString = std::string{mountPath.view()};
  auto runningMountHealthChecks = runningMountHealthChecks_;
  auto progress = std::make_shared<MountHealthProbeProgress>(
      /*queued=*/std::chrono::steady_clock::now());
  {
    auto runningChecks = runningMountHealthChecks->wlock();
    if (!runningChecks->emplace(mountPathString, progress).second) {
      return;
    }
  }
  // Keep the mount marked in-flight until the worker returns. The timeout logs
  // a stuck probe but cannot interrupt the blocked syscall; clearing this on
  // timeout would let later ticks enqueue duplicate stuck probes.

  auto started = make_shared<std::atomic_bool>(false);
  auto completed = make_shared<std::atomic_bool>(false);
  auto emittedMountHealthIssues = emittedMountHealthIssues_;
  auto logContext = make_shared<const MountHealthIssueLogContext>(
      serverState_->getEdenFsEventsLogger(),
      emittedMountHealthIssues,
      serverState_->getStats().copy(),
      std::move(mountPathString),
      std::move(repoSource),
      std::move(mount),
      std::move(progress));
  logContext->count(&MountHealthCheckStats::probes);

  scheduleMountHealthCheckStartTimeout(
      started, completed, logContext, runningMountHealthChecks);

  // Only tests turn on fault injection. Otherwise this stays empty, so a probe
  // never holds ServerState and never becomes its last owner.
  auto faultInjectionState = serverState_->getFaultInjector().isEnabled()
      ? std::weak_ptr<ServerState>{serverState_}
      : std::weak_ptr<ServerState>{};

  auto config = serverState_->getEdenConfig();
  const auto hangThreshold = config->mountHealthCheckHangThreshold.getValue();
  MountHealthCheckOptions options;
  options.useStatx = config->mountHealthCheckStatx.getValue();
  if (auto edenMount = logContext->mount.lock()) {
    options.knownDevice = edenMount->getHealthCheckDevice();
  }

  // Run the potentially blocking filesystem probe on a dedicated pool. The
  // runtime timeout is armed after the worker starts so executor queue delay
  // and a hung EdenFS mount can be reported separately.
  folly::via(
      getMountHealthCheckThreadPool(),
      [started,
       completed,
       logContext,
       options,
       hangThreshold,
       serverState = std::move(faultInjectionState)]() {
        started->store(true, std::memory_order_release);
        if (completed->load(std::memory_order_acquire)) {
          return std::optional<EdenMountHealthCheckIssue>{};
        }
        auto& progress = *logContext->progress;
        progress.start();
        scheduleMountHealthCheckRuntimeTimeout(
            completed, logContext, hangThreshold);
        if (auto state = serverState.lock()) {
          state->getFaultInjector().check(
              "mountHealthCheck", logContext->mountPath);
        }
        auto issue = checkRunningEdenMountHealth(
            logContext->mountPath, &progress, options);
        progress.enter(MountHealthProbeStep::Done);
        return issue;
      })
      .thenTry(
          [completed,
           emittedMountHealthIssues,
           logContext,
           runningMountHealthChecks](
              folly::Try<std::optional<EdenMountHealthCheckIssue>>&& result) {
            const auto alreadyCompleted = completed->exchange(true);
            {
              auto runningChecks = runningMountHealthChecks->wlock();
              runningChecks->erase(logContext->mountPath);
            }
            if (auto device = logContext->progress->device()) {
              if (auto edenMount = logContext->mount.lock()) {
                edenMount->setHealthCheckDevice(device);
              }
            }
            // A hung probe's late result is still handled, so that a
            // healthy one clears the hung report.
            const bool hangEnded = alreadyCompleted &&
                logContext->hung.load(std::memory_order_acquire);
            if (alreadyCompleted && !hangEnded) {
              return;
            }
            if (hangEnded) {
              logContext->count(&MountHealthCheckStats::hangEnded);
              std::string_view outcome = "healthy";
              if (result.hasException()) {
                outcome = "error";
              } else if (result->has_value()) {
                outcome = edenMountHealthIssueReasonString((*result)->reason);
              }
              logMountHealthCheckHangEnded(*logContext, outcome);
            } else {
              if (logContext->progress->mountCheckedByStatx()) {
                logContext->count(&MountHealthCheckStats::statxMountCheck);
              }
              auto slowProbeDetail =
                  std::move(*logContext->slowProbeDetail.wlock());
              if (!slowProbeDetail.empty()) {
                logContext->count(&MountHealthCheckStats::slow);
                logSlowMountHealthCheck(
                    *logContext, std::move(slowProbeDetail));
              }
            }
            if (result.hasException()) {
              XLOGF(
                  WARN,
                  "EdenFS mount health check failed: {}",
                  result.exception().what());
              return;
            }
            const auto& issue = result.value();
            if (!issue.has_value()) {
              logContext->count(&MountHealthCheckStats::healthy);
              clearMountHealthIssues(
                  emittedMountHealthIssues, logContext->mountPath);
              return;
            }
            logContext->count(&MountHealthCheckStats::issue);
            logContext->log(
                std::string{edenMountHealthIssueReasonString(issue->reason)},
                issue->error);
          });
}

bool EdenServer::shouldProbeMountHealth(MountState state) {
  switch (state) {
    case MountState::RUNNING:
      return true;
    case MountState::UNINITIALIZED:
    case MountState::INITIALIZING:
    case MountState::INITIALIZED:
    case MountState::STARTING:
      // Has not asked the kernel to mount yet.
      return false;
    case MountState::FUSE_ERROR:
    case MountState::INIT_ERROR:
      // Never finished mounting, so the kernel was never told about it.
      return false;
    case MountState::SHUTTING_DOWN:
    case MountState::SHUT_DOWN:
    case MountState::DESTROYING:
      // Already torn down.
      return false;
  }
  // MountState is a Thrift enum, so a value outside the enumerators above is
  // representable. Nothing useful can be probed about a state we do not know.
  return false;
}

void EdenServer::checkMountHealth() {
  // Health checks only apply to mounts the daemon believes it is serving, so
  // the live mount map -- not config.json -- is the authoritative input. Both
  // the paths to prune against and the paths to probe are collected under a
  // single lock, so they cannot describe two different snapshots.
  struct ProbeTarget {
    AbsolutePath mountPath;
    std::string repoSource;
    std::weak_ptr<EdenMount> mount;
  };
  std::unordered_set<std::string> registeredMountPaths;
  std::vector<ProbeTarget> probeTargets;
  {
    const auto mountPoints = mountPoints_->rlock();
    registeredMountPaths.reserve(mountPoints->size());
    probeTargets.reserve(mountPoints->size());
    for (const auto& [mountPath, mountInfo] : *mountPoints) {
      // Prune against every registered mount, not just the probed ones.
      // Dropping the dedup state of a mount that is skipped below would let an
      // already-reported issue be emitted a second time once that mount
      // reaches RUNNING.
      registeredMountPaths.insert(std::string{mountPath.view()});

      // Only probe mounts EdenFS is actually serving; see
      // shouldProbeMountHealth for why the other states are skipped. Skipping
      // loses nothing: those states are transient, so such a mount is either
      // probed on a later tick once it reaches RUNNING, or it leaves
      // mountPoints_ and is pruned above.
      const auto& mount = mountInfo.edenMount;
      if (!shouldProbeMountHealth(mount->getState())) {
        continue;
      }
      probeTargets.push_back(
          ProbeTarget{
              mountPath,
              mount->getCheckoutConfig()->getRepoSource(),
              mount->getWeakMount()});
    }
  }
  // config.json is read on this thread, as accidental unmount recovery does;
  // the probes below are the part that can block.
  auto notServed = findCheckoutsNotServed();
  for (const auto& [mountPath, config] : notServed) {
    registeredMountPaths.insert(std::string{mountPath.view()});
  }
  const auto edenConfig = serverState_->getEdenConfig();
  pruneMountHealthIssues(emittedMountHealthIssues_, registeredMountPaths);

  // The probe holds only a weak reference to the mount, so no strong
  // reference is held across the asynchronous check.
  for (auto& target : probeTargets) {
    scheduleRunningMountHealthCheck(
        target.mountPath,
        std::move(target.repoSource),
        std::move(target.mount));
  }

  for (auto& [mountPath, config] : notServed) {
    // Recovery refuses a flapping checkout, so it is reported instead.
    const bool flapping = isCheckoutFlapLimited(mountPath);
    if (!flapping && edenConfig->mountHealthCheckRecovery.getValue()) {
      // The outcome is reported once the recovery succeeds or gives up.
      startCheckoutRecovery(
          mountPath,
          std::move(config),
          CheckoutRecoveryTrigger::HealthCheckTick);
      continue;
    }
    const auto issueReason = flapping
        ? EdenMountHealthIssueReason::DaemonRunningCheckoutFlapping
        : EdenMountHealthIssueReason::DaemonRunningCheckoutNotMounted;
    const auto reason =
        std::string{edenMountHealthIssueReasonString(issueReason)};
    if (!shouldLogMountHealthIssue(
            emittedMountHealthIssues_, mountPath.asString(), reason)) {
      continue;
    }
    getStats()->increment(
        flapping ? &CheckoutRecoveryStats::flapLimited
                 : &CheckoutRecoveryStats::notServed);
    auto stopReason = getCheckoutStopReason(mountPath);
    serverState_->getEdenFsEventsLogger()->logEvent(
        EdenMountHealthIssue{
            "daemon_periodic",
            reason,
            mountPath.asString(),
            mountPath.asString(),
            "configured_checkout",
            config->getRepoSource(),
            stopReason.empty() ? "not_mounted_since_daemon_start" : stopReason,
            false,
            false});
  }
}

std::unique_ptr<CheckoutConfig> EdenServer::loadCheckoutNotServed(
    const AbsolutePath& mountPath,
    const AbsolutePath& clientDirectory) {
  {
    const auto mountPoints = mountPoints_->rlock();
    if (mountPoints->find(mountPath) != mountPoints->end()) {
      return nullptr;
    }
  }

  // This mount point is not currently mounted, but it was configured
  // in config.json.  This means that the client was unmounted.

  auto nfsServer = serverState_->getNfsServer();
  if (nfsServer && nfsServer->isMountRegistered(mountPath)) {
    // This mount point is registered with the NFS server, so it is
    // probably in the process of being unmounted.
    return nullptr;
  }

  auto config =
      CheckoutConfig::loadFromClientDirectory(mountPath, clientDirectory);
  if (config->isIntentionallyUnmounted()) {
    XLOGF(DBG5, "Mount point {} is intentionally unmounted.", mountPath);
    return nullptr;
  }
  if (config->getInodeCatalogType() == InodeCatalogType::LegacyEphemeral) {
    // An ephemeral overlay does not survive an unmount, so a remount would
    // not bring back the checkout's local state.
    XLOGF(DBG5, "Mount point {} has an ephemeral overlay.", mountPath);
    return nullptr;
  }
  return config;
}

bool EdenServer::shouldRecoverAfterChannelStop(
    const EdenMount& edenMount,
    bool unexpectedStop,
    bool takeover) {
  if (!serverState_->getEdenConfig()->recoverOnChannelStop.getValue()) {
    return false;
  }
  StatsGroupBase::Counter CheckoutRecoveryStats::* skipped = nullptr;
  if (takeover) {
    skipped = &CheckoutRecoveryStats::channelStopSkippedTakeover;
  } else if (edenMount.isUnmountStarted()) {
    skipped = &CheckoutRecoveryStats::channelStopSkippedUnmountRequested;
  } else if (runningState_.rlock()->state != RunState::RUNNING) {
    skipped = &CheckoutRecoveryStats::channelStopSkippedDaemonNotRunning;
  } else if (!unexpectedStop) {
    skipped = &CheckoutRecoveryStats::channelStopSkippedExpectedStop;
  }
  if (skipped) {
    getStats()->increment(skipped);
    return false;
  }
  return true;
}

void EdenServer::recoverCheckoutAfterChannelStop(
    const AbsolutePath& mountPath,
    const AbsolutePath& clientDirectory) {
  if (runningState_.rlock()->state != RunState::RUNNING) {
    getStats()->increment(
        &CheckoutRecoveryStats::channelStopSkippedDaemonNotRunning);
    return;
  }
  try {
    auto config = loadCheckoutNotServed(mountPath, clientDirectory);
    if (!config) {
      getStats()->increment(
          &CheckoutRecoveryStats::channelStopSkippedNotNeeded);
      return;
    }
    // Retries, if any, come from the mount health check.
    if (!startCheckoutRecovery(
            mountPath,
            std::move(config),
            CheckoutRecoveryTrigger::ChannelStop)) {
      getStats()->increment(
          &CheckoutRecoveryStats::channelStopSkippedThrottled);
    }
  } catch (const std::exception& ex) {
    XLOGF(
        WARN,
        "Could not recover {} after its channel stopped: {}",
        mountPath,
        folly::exceptionStr(ex));
  }
}

std::vector<std::pair<AbsolutePath, std::unique_ptr<CheckoutConfig>>>
EdenServer::findCheckoutsNotServed() {
  std::vector<std::pair<AbsolutePath, std::unique_ptr<CheckoutConfig>>> result;
  if (runningState_.rlock()->state != RunState::RUNNING) {
    return result;
  }
  folly::dynamic dirs = folly::dynamic::object();
  try {
    dirs = CheckoutConfig::loadClientDirectoryMap(edenDir_.getPath());
  } catch (const std::exception& ex) {
    XLOGF(WARN, "Could not parse config.json: {}", folly::exceptionStr(ex));
    return result;
  }
  std::unordered_set<AbsolutePath> configuredPaths;
  for (const auto& client : dirs.items()) {
    try {
      auto mountPath = canonicalPath(client.first.stringPiece());
      configuredPaths.insert(mountPath);
      if (auto config = loadCheckoutNotServed(
              mountPath,
              edenDir_.getCheckoutStateDir(client.second.stringPiece()))) {
        result.emplace_back(std::move(mountPath), std::move(config));
      }
    } catch (const std::exception& ex) {
      XLOGF(
          WARN,
          "Could not check whether {} is mounted: {}",
          client.first.asString(),
          folly::exceptionStr(ex));
    }
  }
  pruneCheckoutRecoveries(configuredPaths);
  return result;
}

void EdenServer::recoverCheckoutsNotServedSoon() {
  const auto config = serverState_->getEdenConfig();
  if (config->mountHealthCheckInterval.getValue() <=
          std::chrono::nanoseconds{0} ||
      !config->mountHealthCheckRecovery.getValue()) {
    return;
  }
  scheduleCallbackOnMainEventBase(std::chrono::milliseconds{0}, [this] {
    for (auto& [mountPath, checkoutConfig] : findCheckoutsNotServed()) {
      // The next tick reports a flapping checkout.
      if (!isCheckoutFlapLimited(mountPath)) {
        startCheckoutRecovery(
            mountPath,
            std::move(checkoutConfig),
            CheckoutRecoveryTrigger::HealthCheckTick);
      }
    }
  });
}

std::string EdenServer::getCheckoutStopReason(const AbsolutePath& mountPath) {
  auto states = checkoutRecoveryStates_.rlock();
  auto it = states->find(mountPath);
  return it == states->end() ? std::string{} : it->second.stopReason;
}

bool EdenServer::isCheckoutFlapLimited(const AbsolutePath& mountPath) const {
  auto states = checkoutRecoveryStates_.rlock();
  auto it = states->find(mountPath);
  return it != states->end() && it->second.flaps >= kMaxCheckoutFlaps;
}

bool EdenServer::startCheckoutRecovery(
    const AbsolutePath& mountPath,
    std::unique_ptr<CheckoutConfig> initialConfig,
    CheckoutRecoveryTrigger trigger) {
  auto attempt = beginCheckoutRecovery(mountPath);
  if (!attempt) {
    return false;
  }

  XLOGF(
      INFO,
      "Mount point {} is not currently mounted, attempting to remount.",
      mountPath);

  AccidentalUnmountRecovery event{
      /*error=*/"",
      /*success=*/false,
      /*repo_source=*/initialConfig->getRepoSource(),
      /*mount_path=*/mountPath.asString(),
      /*trigger=*/trigger,
      /*attempt=*/attempt->number,
      /*duration=*/0.0,
      /*mount_protocol=*/
      FieldConverter<MountProtocol>{}.toDebugString(
          serverState_->getEdenConfig()->enableNfsServer.getValue()
              ? initialConfig->getMountProtocol()
              : kMountProtocolDefault),
      /*stop_reason=*/attempt->stopReason.empty()
          ? "not_mounted_since_daemon_start"
          : attempt->stopReason};
  folly::via(
      getServerState()->getThreadPool().get(),
      [this,
       initialConfig = std::move(initialConfig),
       edenFsEventsLogger = serverState_->getEdenFsEventsLogger(),
       mountPath = mountPath,
       event = std::move(event),
       stoppedAt = attempt->stoppedAt]() mutable {
        // A graceful restart may have started since this attempt was queued.
        if (!beginRecoveryMount(mountPath)) {
          cancelCheckoutRecovery(mountPath);
          getStats()->increment(&CheckoutRecoveryStats::cancelled);
          XLOGF(
              INFO,
              "Not remounting {}: EdenFS is no longer running",
              mountPath);
          return folly::makeSemiFuture();
        }
        getStats()->increment(&CheckoutRecoveryStats::attempt);
        folly::stop_watch<> stopWatch;
        return makeImmediateFutureWith([&] {
                 return mount(std::move(initialConfig), /*readOnly=*/false);
               })
            .thenTry([this,
                      mountPath,
                      edenFsEventsLogger,
                      event = std::move(event),
                      stoppedAt,
                      stopWatch](
                         folly::Try<std::shared_ptr<EdenMount>>&&
                             result) mutable {
              endRecoveryMount(mountPath);
              bool success = result.hasValue();
              // End the attempt before counting it: a channel stop seen
              // after the count must be judged as a flap, not as part of
              // this attempt.
              const bool gaveUp = endCheckoutRecovery(mountPath, success);
              getStats()->increment(
                  success ? &CheckoutRecoveryStats::success
                          : &CheckoutRecoveryStats::failure);
              event.gave_up = gaveUp;
              if (gaveUp) {
                getStats()->increment(&CheckoutRecoveryStats::gaveUp);
                XLOGF(
                    WARN,
                    "Giving up recovering {} after {} failed attempts",
                    mountPath,
                    kMaxCheckoutRecoveryAttempts);
              }
              if (success || gaveUp) {
                edenFsEventsLogger->logEvent(
                    EdenMountHealthIssue{
                        std::string{recoveryOutcomeSource(event.trigger)},
                        std::string{edenMountHealthIssueReasonString(
                            EdenMountHealthIssueReason::
                                DaemonRunningCheckoutNotMounted)},
                        mountPath.asString(),
                        mountPath.asString(),
                        "configured_checkout",
                        event.repo_source,
                        event.stop_reason,
                        /*attempted_repair=*/true,
                        success});
              }
              event.success = success;
              if (success) {
                event.mount_protocol =
                    FieldConverter<MountProtocol>{}.toDebugString(
                        result.value()->shouldBeOrIsNfsChannel()
                            ? MountProtocol::NFS
                            : kMountProtocolDefault);
              }
              event.error =
                  success ? "" : result.exception().what().toStdString();
              event.duration =
                  std::chrono::duration<double>{stopWatch.elapsed()}.count();
              if (stoppedAt) {
                event.outage_age_ms =
                    std::chrono::duration_cast<std::chrono::milliseconds>(
                        std::chrono::steady_clock::now() - *stoppedAt)
                        .count();
              }
              edenFsEventsLogger->logEvent(event);
              if (success) {
                XLOGF(
                    DBG3,
                    "Automatically remounted mount: {}",
                    mountPath.value());
                return ImmediateFuture<folly::Unit>{std::in_place};
              } else {
                auto errorMessage = fmt::format(
                    "Failed to remount {}: {}\n",
                    mountPath.value(),
                    result.exception().what());
                XLOG(ERR, errorMessage);
                return makeImmediateFuture<Unit>(std::move(result).exception());
              }
            })
            .semi();
      });
  return true;
}

std::optional<EdenServer::CheckoutRecoveryAttempt>
EdenServer::beginCheckoutRecovery(const AbsolutePath& mountPath) {
  auto states = checkoutRecoveryStates_.wlock();
  auto& state = (*states)[mountPath];
  // Skip while a remount is pending, after giving up, or while flapping.
  if (state.inFlight || state.gaveUp || state.flaps >= kMaxCheckoutFlaps) {
    return std::nullopt;
  }
  state.inFlight = true;
  state.stoppedDuringRecovery = false;
  return CheckoutRecoveryAttempt{
      state.failedAttempts + 1,
      state.stopReason,
      state.stopReason.empty() ? std::nullopt
                               : std::make_optional(state.stoppedAt)};
}

void EdenServer::addCheckoutFlap(
    const AbsolutePath& mountPath,
    CheckoutRecoveryState& state) {
  if (++state.flaps == kMaxCheckoutFlaps) {
    XLOGF(
        WARN,
        "{} stopped {} times soon after being recovered; not recovering it again",
        mountPath,
        kMaxCheckoutFlaps);
  }
}

void EdenServer::recordCheckoutStop(
    const AbsolutePath& mountPath,
    std::string reason,
    std::optional<std::chrono::steady_clock::time_point> channelStoppedAt) {
  auto states = checkoutRecoveryStates_.wlock();
  auto& state = (*states)[mountPath];
  if (state.inFlight) {
    // A recovery attempt's failed mount must not replace the original cause.
    if (!channelStoppedAt) {
      return;
    }
    // The remounted channel started and stopped again: a new outage.
    state.stoppedDuringRecovery = true;
  }
  const auto stoppedAt =
      channelStoppedAt.value_or(std::chrono::steady_clock::now());
  if (state.recoveredAt) {
    if (stoppedAt - *state.recoveredAt < kCheckoutFlapWindow) {
      addCheckoutFlap(mountPath, state);
    } else {
      state.flaps = 0;
    }
    state.recoveredAt.reset();
  }
  state.stopReason = std::move(reason);
  state.stoppedAt = stoppedAt;
}

bool EdenServer::endCheckoutRecovery(
    const AbsolutePath& mountPath,
    bool success) {
  auto states = checkoutRecoveryStates_.wlock();
  auto it = states->try_emplace(mountPath).first;
  auto& state = it->second;
  if (success) {
    // Only the flap count outlives a successful recovery.
    CheckoutRecoveryState recovered;
    recovered.flaps = state.flaps;
    if (state.stoppedDuringRecovery) {
      // The remounted channel already stopped again: a new outage, and a
      // flap.
      recovered.stopReason = std::move(state.stopReason);
      recovered.stoppedAt = state.stoppedAt;
      addCheckoutFlap(mountPath, recovered);
    } else {
      recovered.recoveredAt = std::chrono::steady_clock::now();
    }
    state = std::move(recovered);
    return false;
  }
  if (state.resetRequested) {
    // Mounted some other way while this attempt ran, so its failure (usually
    // "already mounted") does not count.
    if (!state.stoppedDuringRecovery) {
      states->erase(it);
      return false;
    }
    // That mount's channel already stopped again: keep the new outage.
    CheckoutRecoveryState newOutage;
    newOutage.stopReason = std::move(state.stopReason);
    newOutage.stoppedAt = state.stoppedAt;
    state = std::move(newOutage);
    return false;
  }
  state.inFlight = false;
  ++state.failedAttempts;
  if (state.failedAttempts >= kMaxCheckoutRecoveryAttempts) {
    state.gaveUp = true;
    return true;
  }
  return false;
}

void EdenServer::cancelCheckoutRecovery(const AbsolutePath& mountPath) {
  {
    auto states = checkoutRecoveryStates_.wlock();
    auto it = states->find(mountPath);
    if (it == states->end()) {
      return;
    }
    if (!it->second.resetRequested) {
      it->second.inFlight = false;
      return;
    }
  }
  // Mounted some other way while this attempt was queued, which
  // endCheckoutRecovery() does not count as a failure.
  endCheckoutRecovery(mountPath, /*success=*/false);
}

void EdenServer::resetCheckoutRecovery(const AbsolutePath& mountPath) {
  auto states = checkoutRecoveryStates_.wlock();
  auto it = states->find(mountPath);
  if (it == states->end()) {
    return;
  }
  if (it->second.inFlight) {
    // Let endCheckoutRecovery() apply the reset. A recovery attempt's own
    // successful mount() also gets here.
    it->second.resetRequested = true;
    return;
  }
  states->erase(it);
}

void EdenServer::pruneCheckoutRecoveries(
    const std::unordered_set<AbsolutePath>& configuredPaths) {
  auto states = checkoutRecoveryStates_.wlock();
  for (auto it = states->begin(); it != states->end();) {
    // An in-flight entry is left for its attempt to finish.
    if (!it->second.inFlight && !configuredPaths.contains(it->first)) {
      it = states->erase(it);
    } else {
      ++it;
    }
  }
}

void EdenServer::refreshRestrictedRoots() {
  static auto context = ObjectFetchContext::getNullContextWithCauseDetail(
      ObjectFetchContext::StaticCauseDetail::fromLiteral(
          "EdenServer::refreshRestrictedRoots"));
  std::vector<EdenMountHandle> omittedMounts;
  for (auto& mountHandle : getMountPoints()) {
    if (mountHandle.getObjectStore().getRestrictedContentMode() ==
        RestrictedContentMode::Omitted) {
      omittedMounts.push_back(mountHandle);
    }
  }
  if (omittedMounts.empty()) {
    return;
  }
  // The walk visits every loaded TreeInode, so its cost grows with the mount
  // (about 100ms at 400k loaded inodes), which is too slow for the main
  // EventBase this task runs on. Run it on the server pool, one round at a
  // time: a tick that arrives while the previous round is still walking is
  // skipped rather than queued behind it.
  if (restrictedRootRefreshInFlight_->exchange(true)) {
    return;
  }
  // Shared by every scheduled walk; the flag clears when the last one is done,
  // whether it returned or threw.
  auto inFlight = std::shared_ptr<void>(
      nullptr,
      [flag = restrictedRootRefreshInFlight_](void*) { flag->store(false); });
  for (auto& mountHandle : omittedMounts) {
    mountHandle.getEdenMount().getServerThreadPool()->add([mountHandle,
                                                           inFlight] {
      try {
        mountHandle.getRootInode()->recheckHiddenRestrictedDescendants(context);
      } catch (const std::exception& ex) {
        XLOGF(
            WARN,
            "restricted root refresh failed for {}: {}",
            mountHandle.getEdenMount().getPath(),
            ex.what());
      }
    });
  }
}

void EdenServer::reloadConfig() {
  // Reload the config, then get the new values.
  serverState_->getReloadableConfig()->maybeReload();
  auto config = serverState_->getReloadableConfig()->getEdenConfig();

  SaplingBackingStore::setScribeLoggingEnabled(
      config->enableScribeLogging.getValue());

  // Update all periodic tasks that are controlled by config settings.
  // This should be cheap, so for now we just block on this to finish rather
  // than returning a Future.  We could change this to return a Future if we
  // found a reason to do so in the future.
  mainEventBase_->runImmediatelyOrRunInEventBaseThreadAndWait(
      [this, config = std::move(config)] {
        updatePeriodicTaskIntervals(*config);
      });

  // Rebuild cached InodePressurePolicy for each mount.
  for (auto& handle : getMountPoints()) {
    handle.getEdenMount().updateInodePressurePolicy();
  }
}

void EdenServer::checkLockValidity() {
  if (edenDir_.isLockValid()) {
    return;
  }

  // Exit if our lock file no longer looks valid.
  // This ensures EdenFS process quits if someone deletes the `.eden` state
  // directory or moves it to another location.  Otherwise an EdenFS process
  // could continue running indefinitely in the background even though its state
  // directory no longer exists.
  XLOG(ERR, "Stopping EdenFS: on-disk lock file is no longer valid");

  // Attempt an orderly shutdown for now.  Since our state directory may have
  // been deleted we might not really be able to shut down normally, but for now
  // we'll try.  We potentially could try more aggressive measures (exit() or
  // _exit()) if we find that trying to stop normally here ever causes problems.
  stop();
}

apache::thrift::ServerStream<std::string>
EdenServer::createStartupStatusThriftStream() {
  return ThriftStreamStartupStatusSubscriber::createStartupStatusThriftStream(
      startupStatusChannel_);
}

void EdenServer::clearStartupStatusPublishers() {
  startupStatusChannel_->startupCompleted();
}

} // namespace facebook::eden
