/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/utils/MountHealthCheck.h"

#include <array>
#include <atomic>
#include <cerrno>
#include <optional>
#include <string>
#include <vector>

#include <folly/Conv.h>
#include <folly/FileUtil.h>
#include <folly/String.h>
#include <folly/logging/xlog.h>
#include <folly/portability/SysStat.h>
#include <folly/portability/Unistd.h>
#include <folly/system/ThreadId.h>

#ifdef __linux__
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>

#include "eden/common/utils/FSDetect.h"
#include "eden/fs/utils/MountInfoTable.h"

#ifndef STATX_ATTR_MOUNT_ROOT
#define STATX_ATTR_MOUNT_ROOT 0x00002000
#endif
#endif

namespace facebook::eden {
namespace {

bool isNotConnectedErrno(int err) {
  return err == ENOTCONN || err == ENXIO;
}

std::optional<int> lstatError(const std::string& path) {
  struct stat st{};
  if (lstat(path.c_str(), &st) == 0) {
    return std::nullopt;
  }
  return errno;
}

std::optional<EdenMountHealthCheckIssue> issueForPathAccessError(
    const std::string& path,
    int err) {
  if (isNotConnectedErrno(err)) {
    return EdenMountHealthCheckIssue{
        EdenMountHealthIssueReason::DaemonRunningMountNotConnected,
        path + ": " + folly::errnoStr(err)};
  }
  if (err == ETIMEDOUT) {
    return EdenMountHealthCheckIssue{
        EdenMountHealthIssueReason::DaemonRunningMountTimedOut,
        path + ": " + folly::errnoStr(err)};
  }

  XLOGF(
      WARN,
      "Unexpected EdenFS mount health check lstat error for {}: {}",
      path,
      folly::errnoStr(err));
  return EdenMountHealthCheckIssue{
      EdenMountHealthIssueReason::DaemonRunningMountAccessError,
      path + ": " + folly::errnoStr(err)};
}

std::optional<bool> hasEdenMountInKernelMountTable(
    const std::string& mountPath,
    MountHealthProbeProgress* progress) {
#ifdef __linux__
  MountInfoOptions options;
  options.includeMountSource = true;
  auto result = getMountInfoForPath(mountPath.c_str(), options);
  if (result.hasError()) {
    XLOGF(
        WARN,
        "Failed to read kernel mount table for {}: {}",
        mountPath,
        folly::errnoStr(result.error()));
    // The mount table check is inconclusive. Continue with the .eden probes
    // instead of reporting a mount-health issue from the failed lookup itself.
    return std::nullopt;
  }

  const auto& mountInfo = result.value();
  if (!mountInfo.has_value()) {
    return false;
  }
  if (!is_edenfs_mount(mountInfo->mountSource, mountInfo->fsType)) {
    return false;
  }
  if (progress) {
    progress->recordDevice(
        makedev(mountInfo->devMajor, mountInfo->devMinor), /*byStatx=*/false);
  }
  return true;
#else
  (void)mountPath;
  (void)progress;
  return true;
#endif
}

std::optional<std::string> readTaskFile(uint64_t threadId, const char* name) {
#ifdef __linux__
  auto path = folly::to<std::string>("/proc/self/task/", threadId, "/", name);
  std::string contents;
  if (folly::readFile(path.c_str(), contents)) {
    return contents;
  }
#else
  (void)threadId;
  (void)name;
#endif
  return std::nullopt;
}

std::optional<std::chrono::nanoseconds> readRunQueueWait(uint64_t threadId) {
  auto schedstat = readTaskFile(threadId, "schedstat");
  if (!schedstat) {
    return std::nullopt;
  }
  return detail::parseSchedstatRunQueueWait(*schedstat);
}

std::string makeExpectedMissingPath(const std::string& mountPath) {
  // Use a fresh negative lookup path each time so kernel/FUSE negative-entry
  // caching does not hide whether EdenFS can answer a lookup now.
  static std::atomic<uint64_t> nextHealthCheckPathId{0};
  auto id = nextHealthCheckPathId.fetch_add(1, std::memory_order_relaxed);
  return mountPath + "/.eden/edenfs-mount-health-" + std::to_string(id);
}

} // namespace

std::optional<uint64_t> mountRootDevice(const std::string& path) {
#ifdef __linux__
  struct statx stx{};
  if (statx(
          AT_FDCWD,
          path.c_str(),
          AT_SYMLINK_NOFOLLOW | AT_STATX_DONT_SYNC,
          0,
          &stx) != 0) {
    return std::nullopt;
  }
  if (!(stx.stx_attributes_mask & STATX_ATTR_MOUNT_ROOT) ||
      !(stx.stx_attributes & STATX_ATTR_MOUNT_ROOT)) {
    return std::nullopt;
  }
  return makedev(stx.stx_dev_major, stx.stx_dev_minor);
#else
  (void)path;
  return std::nullopt;
#endif
}

std::string_view edenMountHealthIssueReasonString(
    EdenMountHealthIssueReason reason) {
  switch (reason) {
    case EdenMountHealthIssueReason::DaemonRunningKernelMountMissing:
      return "daemon_running_kernel_mount_missing";
    case EdenMountHealthIssueReason::DaemonRunningDotEdenMissing:
      return "daemon_running_dot_eden_missing";
    case EdenMountHealthIssueReason::DaemonRunningMountNotConnected:
      return "daemon_running_mount_not_connected";
    case EdenMountHealthIssueReason::DaemonRunningMountTimedOut:
      return "daemon_running_mount_timed_out";
    case EdenMountHealthIssueReason::DaemonRunningMountAccessError:
      return "daemon_running_mount_access_error";
    case EdenMountHealthIssueReason::DaemonRunningCheckoutNotMounted:
      return "daemon_running_checkout_not_mounted";
    case EdenMountHealthIssueReason::DaemonRunningCheckoutFlapping:
      return "daemon_running_checkout_flapping";
    case EdenMountHealthIssueReason::DaemonRunningMountCheckNotStarted:
      return "daemon_running_mount_check_not_started";
    case EdenMountHealthIssueReason::DaemonRunningMountHung:
      return "daemon_running_mount_hung";
    case EdenMountHealthIssueReason::DaemonRunningMountSlow:
      return "daemon_running_mount_slow";
    case EdenMountHealthIssueReason::DaemonRunningMountHangEnded:
      return "daemon_running_mount_hang_ended";
  }
  return "unknown";
}

std::string_view mountHealthProbeStepName(MountHealthProbeStep step) {
  switch (step) {
    case MountHealthProbeStep::Queued:
      return "queued";
    case MountHealthProbeStep::Starting:
      return "starting";
    case MountHealthProbeStep::StatRoot:
      return "stat_root";
    case MountHealthProbeStep::MountTable:
      return "mount_table";
    case MountHealthProbeStep::StatDotEden:
      return "stat_dot_eden";
    case MountHealthProbeStep::LookupChild:
      return "lookup_child";
    case MountHealthProbeStep::Done:
      return "done";
  }
  return "unknown";
}

MountHealthProbeProgress::MountHealthProbeProgress(Clock::time_point queued)
    : queued_{queued},
      step_{CurrentStep{MountHealthProbeStep::Queued, queued}} {}

void MountHealthProbeProgress::start(Clock::time_point now) {
  const auto threadId = folly::getOSThreadID();
  if (auto runQueueWait = readRunQueueWait(threadId)) {
    startRunQueueWaitNs_.store(
        runQueueWait->count(), std::memory_order_release);
  }
  threadId_.store(threadId, std::memory_order_release);
  startedTicks_.store(
      now.time_since_epoch().count(), std::memory_order_release);
  enter(MountHealthProbeStep::Starting, now);
}

void MountHealthProbeProgress::enter(
    MountHealthProbeStep step,
    Clock::time_point now) {
  *step_.lock() = CurrentStep{step, now};
}

std::optional<MountHealthProbeProgress::Clock::time_point>
MountHealthProbeProgress::started() const {
  auto ticks = startedTicks_.load(std::memory_order_acquire);
  if (ticks == 0) {
    return std::nullopt;
  }
  return Clock::time_point{Clock::duration{ticks}};
}

std::optional<std::chrono::nanoseconds>
MountHealthProbeProgress::startRunQueueWait() const {
  auto ns = startRunQueueWaitNs_.load(std::memory_order_acquire);
  if (ns < 0) {
    return std::nullopt;
  }
  return std::chrono::nanoseconds{ns};
}

folly::dynamic describeProbeProgress(
    const MountHealthProbeProgress& progress,
    MountHealthProbeProgress::Clock::time_point now) {
  auto toMs = [](MountHealthProbeProgress::Clock::duration duration) {
    return std::chrono::duration_cast<std::chrono::milliseconds>(duration)
        .count();
  };
  const auto current = progress.currentStep();
  folly::dynamic detail =
      folly::dynamic::object("step", mountHealthProbeStepName(current.step))(
          "step_ms", toMs(now - current.entered));
  if (auto started = progress.started()) {
    detail["queue_wait_ms"] = toMs(*started - progress.queued());
    detail["running_ms"] = toMs(now - *started);
  }
  return detail;
}

namespace detail {

std::optional<char> parseTaskStatState(std::string_view stat) {
  // The command name in parentheses may itself contain spaces and ')'.
  auto commEnd = stat.rfind(')');
  if (commEnd == std::string_view::npos || commEnd + 2 >= stat.size()) {
    return std::nullopt;
  }
  return stat[commEnd + 2];
}

PressureAvg10 parsePressure(std::string_view pressure) {
  PressureAvg10 result;
  std::vector<std::string_view> lines;
  folly::split('\n', pressure, lines);
  for (auto line : lines) {
    std::vector<std::string_view> fields;
    folly::split(' ', line, fields);
    constexpr std::string_view kAvg10 = "avg10=";
    if (fields.size() < 2 || !fields[1].starts_with(kAvg10)) {
      continue;
    }
    auto value = folly::tryTo<double>(fields[1].substr(kAvg10.size()));
    if (!value.hasValue()) {
      continue;
    }
    if (fields[0] == "some") {
      result.some = value.value();
    } else if (fields[0] == "full") {
      result.full = value.value();
    }
  }
  return result;
}

std::optional<std::chrono::nanoseconds> parseSchedstatRunQueueWait(
    std::string_view schedstat) {
  std::vector<std::string_view> fields;
  folly::split(' ', folly::trimWhitespace(schedstat), fields);
  if (fields.size() < 2) {
    return std::nullopt;
  }
  auto ns = folly::tryTo<int64_t>(fields[1]);
  if (!ns.hasValue()) {
    return std::nullopt;
  }
  return std::chrono::nanoseconds{ns.value()};
}

} // namespace detail

std::optional<ThreadKernelState> readThreadKernelState(uint64_t threadId) {
  auto stat = readTaskFile(threadId, "stat");
  if (!stat) {
    return std::nullopt;
  }
  auto state = detail::parseTaskStatState(*stat);
  if (!state) {
    return std::nullopt;
  }
  ThreadKernelState result;
  result.state = *state;
  result.wchan = readTaskFile(threadId, "wchan").value_or("");
  result.runQueueWait = readRunQueueWait(threadId);
  return result;
}

folly::dynamic describeProbeThread(const MountHealthProbeProgress& progress) {
  folly::dynamic detail = folly::dynamic::object;
  const auto threadId = progress.threadId();
  if (threadId == 0 || progress.step() == MountHealthProbeStep::Done) {
    return detail;
  }
  auto state = readThreadKernelState(threadId);
  if (!state) {
    return detail;
  }
  detail["thread_state"] = std::string(1, state->state);
  detail["wchan"] = state->wchan;
  const auto startRunQueueWait = progress.startRunQueueWait();
  if (startRunQueueWait && state->runQueueWait) {
    detail["runq_wait_ms"] =
        std::chrono::duration_cast<std::chrono::milliseconds>(
            *state->runQueueWait - *startRunQueueWait)
            .count();
  }
  return detail;
}

std::optional<uint64_t> readFuseConnectionWaiting(uint64_t device) {
#ifdef __linux__
  if (device == 0) {
    return std::nullopt;
  }
  // FUSE names each connection's directory after its device minor.
  auto path = folly::to<std::string>(
      "/sys/fs/fuse/connections/", minor(device), "/waiting");
  std::string contents;
  if (!folly::readFile(path.c_str(), contents)) {
    return std::nullopt;
  }
  auto waiting = folly::tryTo<uint64_t>(folly::trimWhitespace(contents));
  if (!waiting.hasValue()) {
    return std::nullopt;
  }
  return waiting.value();
#else
  (void)device;
  return std::nullopt;
#endif
}

folly::dynamic describeSystemPressure() {
  folly::dynamic result = folly::dynamic::object;
#ifdef __linux__
  constexpr std::array<const char*, 3> kResources{"cpu", "memory", "io"};
  for (const auto* resource : kResources) {
    std::string contents;
    if (!folly::readFile(
            folly::to<std::string>("/proc/pressure/", resource).c_str(),
            contents)) {
      continue;
    }
    auto pressure = detail::parsePressure(contents);
    if (pressure.some) {
      result[folly::to<std::string>("host_", resource, "_some")] =
          *pressure.some;
    }
    if (pressure.full) {
      result[folly::to<std::string>("host_", resource, "_full")] =
          *pressure.full;
    }
  }
#endif
  return result;
}

std::optional<EdenMountHealthCheckIssue> checkRunningEdenMountHealth(
    const std::string& mountPath,
    MountHealthProbeProgress* progress,
    MountHealthCheckOptions options) {
  auto enter = [progress](MountHealthProbeStep step) {
    if (progress) {
      progress->enter(step);
    }
  };

  bool mountedByStatx = false;
  if (options.useStatx && options.knownDevice != 0) {
    enter(MountHealthProbeStep::StatRoot);
    mountedByStatx = mountRootDevice(mountPath) == options.knownDevice;
    if (mountedByStatx && progress) {
      progress->recordDevice(options.knownDevice, /*byStatx=*/true);
    }
  }
  // Anything else, including a missing mount, is decided by the mount table.
  if (!mountedByStatx) {
    enter(MountHealthProbeStep::MountTable);
    auto hasKernelMount = hasEdenMountInKernelMountTable(mountPath, progress);
    if (hasKernelMount.has_value() && !hasKernelMount.value()) {
      return EdenMountHealthCheckIssue{
          EdenMountHealthIssueReason::DaemonRunningKernelMountMissing,
          "EdenFS mount is missing from the kernel mount table"};
    }
  }
  // If the mount table lookup failed, keep going. The path probes below can
  // still identify a disconnected, missing, or hanging Eden mount.

  enter(MountHealthProbeStep::StatDotEden);
  auto dotEdenPath = mountPath + "/.eden";
  auto dotEdenError = lstatError(dotEdenPath);
  if (dotEdenError.has_value()) {
    auto err = dotEdenError.value();
    if (err == ENOENT) {
      return EdenMountHealthCheckIssue{
          EdenMountHealthIssueReason::DaemonRunningDotEdenMissing,
          dotEdenPath + ": " + folly::errnoStr(err)};
    }
    return issueForPathAccessError(dotEdenPath, err);
  }

  // Since .eden was readable, probe a child path that should not exist. A
  // healthy mount should answer ENOENT; disconnected or hanging mounts can
  // surface as ENOTCONN/ENXIO/ETIMEDOUT.
  enter(MountHealthProbeStep::LookupChild);
  auto expectedMissingPath = makeExpectedMissingPath(mountPath);
  auto expectedMissingError = lstatError(expectedMissingPath);
  if (!expectedMissingError.has_value()) {
    // The probe path unexpectedly exists, so this check is inconclusive.
    return std::nullopt;
  }

  auto err = expectedMissingError.value();
  if (err == ENOENT) {
    // The mount handled lookup normally and reported the probe path missing.
    return std::nullopt;
  }
  return issueForPathAccessError(expectedMissingPath, err);
}

} // namespace facebook::eden
