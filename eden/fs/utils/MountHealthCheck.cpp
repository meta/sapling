/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/utils/MountHealthCheck.h"

#include <atomic>
#include <cerrno>
#include <optional>
#include <string>

#include <folly/String.h>
#include <folly/logging/xlog.h>
#include <folly/portability/SysStat.h>
#include <folly/portability/Unistd.h>

#ifdef __linux__
#include "eden/common/utils/FSDetect.h"
#include "eden/fs/utils/MountInfoTable.h"
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
    const std::string& mountPath) {
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
  return is_edenfs_mount(mountInfo->mountSource, mountInfo->fsType);
#else
  (void)mountPath;
  return true;
#endif
}

std::string makeExpectedMissingPath(const std::string& mountPath) {
  // Use a fresh negative lookup path each time so kernel/FUSE negative-entry
  // caching does not hide whether EdenFS can answer a lookup now.
  static std::atomic<uint64_t> nextHealthCheckPathId{0};
  auto id = nextHealthCheckPathId.fetch_add(1, std::memory_order_relaxed);
  return mountPath + "/.eden/edenfs-mount-health-" + std::to_string(id);
}

} // namespace

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
  }
  return "unknown";
}

std::optional<EdenMountHealthCheckIssue> checkRunningEdenMountHealth(
    const std::string& mountPath) {
  auto hasKernelMount = hasEdenMountInKernelMountTable(mountPath);
  if (hasKernelMount.has_value() && !hasKernelMount.value()) {
    return EdenMountHealthCheckIssue{
        EdenMountHealthIssueReason::DaemonRunningKernelMountMissing,
        "EdenFS mount is missing from the kernel mount table"};
  }
  // If the mount table lookup failed, keep going. The path probes below can
  // still identify a disconnected, missing, or hanging Eden mount.

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
