/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <optional>
#include <string>
#include <string_view>

namespace facebook::eden {

enum class EdenMountHealthIssueReason {
  DaemonRunningKernelMountMissing,
  DaemonRunningDotEdenMissing,
  DaemonRunningMountNotConnected,
  DaemonRunningMountTimedOut,
  DaemonRunningMountAccessError,
  // Configured in config.json but not served, and not unmounted on purpose.
  DaemonRunningCheckoutNotMounted,
};

std::string_view edenMountHealthIssueReasonString(
    EdenMountHealthIssueReason reason);

struct EdenMountHealthCheckIssue {
  EdenMountHealthIssueReason reason;
  std::string error;
};

// Checks a daemon-reported RUNNING mount against the kernel mount table and
// lightweight .eden path probes. Returns an issue only when the mount is
// clearly missing, disconnected, timing out, or returning path access errors.
std::optional<EdenMountHealthCheckIssue> checkRunningEdenMountHealth(
    const std::string& mountPath);

} // namespace facebook::eden
