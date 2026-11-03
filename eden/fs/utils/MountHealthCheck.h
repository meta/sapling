/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <atomic>
#include <chrono>
#include <cstdint>
#include <mutex>
#include <optional>
#include <string>
#include <string_view>

#include <folly/Synchronized.h>

namespace facebook::eden {

enum class EdenMountHealthIssueReason {
  DaemonRunningKernelMountMissing,
  DaemonRunningDotEdenMissing,
  DaemonRunningMountNotConnected,
  DaemonRunningMountTimedOut,
  DaemonRunningMountAccessError,
  // Configured in config.json but not served, and not unmounted on purpose.
  DaemonRunningCheckoutNotMounted,
  // Not served, and no longer recovered because it kept stopping soon after
  // each recovery.
  DaemonRunningCheckoutFlapping,
};

std::string_view edenMountHealthIssueReasonString(
    EdenMountHealthIssueReason reason);

struct EdenMountHealthCheckIssue {
  EdenMountHealthIssueReason reason;
  std::string error;
};

enum class MountHealthProbeStep : uint8_t {
  Queued,
  Starting,
  MountTable,
  StatDotEden,
  LookupChild,
  Done,
};

std::string_view mountHealthProbeStepName(MountHealthProbeStep step);

/**
 * What a probe is doing, so that a timeout on another thread can report where
 * a stuck probe is. Only the probe thread writes it.
 */
class MountHealthProbeProgress {
 public:
  using Clock = std::chrono::steady_clock;

  struct CurrentStep {
    MountHealthProbeStep step;
    Clock::time_point entered;
  };

  explicit MountHealthProbeProgress(Clock::time_point queued);

  /** Called on the probe thread when it picks up the probe. */
  void start(Clock::time_point now = Clock::now());
  void enter(MountHealthProbeStep step, Clock::time_point now = Clock::now());

  MountHealthProbeStep step() const {
    return step_.lock()->step;
  }
  /** The step and when the probe entered it, read together. */
  CurrentStep currentStep() const {
    return *step_.lock();
  }
  Clock::time_point queued() const {
    return queued_;
  }
  /** Unset until the probe starts. */
  std::optional<Clock::time_point> started() const;
  /** The OS id of the probe thread, or 0 until the probe starts. */
  uint64_t threadId() const {
    return threadId_.load(std::memory_order_acquire);
  }

 private:
  const Clock::time_point queued_;
  std::atomic<Clock::rep> startedTicks_{0};
  std::atomic<uint64_t> threadId_{0};
  folly::Synchronized<CurrentStep, std::mutex> step_;
};

// Checks a daemon-reported RUNNING mount against the kernel mount table and
// lightweight .eden path probes. Returns an issue only when the mount is
// clearly missing, disconnected, timing out, or returning path access errors.
std::optional<EdenMountHealthCheckIssue> checkRunningEdenMountHealth(
    const std::string& mountPath,
    MountHealthProbeProgress* progress = nullptr);

} // namespace facebook::eden
