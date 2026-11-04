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
#include <folly/dynamic.h>

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
  // No health check thread picked up the probe in time.
  DaemonRunningMountCheckNotStarted,
  // The probe ran past `mount:mount-health-check-hang-threshold`.
  DaemonRunningMountHung,
  // The probe ran past the 5 s timeout but finished before the hang
  // threshold.
  DaemonRunningMountSlow,
  // A probe reported as hung finished.
  DaemonRunningMountHangEnded,
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
  StatRoot,
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
  /** The probe thread's total CPU wait when the probe started, if known. */
  std::optional<std::chrono::nanoseconds> startRunQueueWait() const;

  /** The EdenFS mount's device, once the probe found it, or 0. */
  uint64_t device() const {
    return device_.load(std::memory_order_acquire);
  }
  /** Whether statx(2) confirmed the mount, so the mount table was not read. */
  bool mountCheckedByStatx() const {
    return mountCheckedByStatx_.load(std::memory_order_acquire);
  }
  void recordDevice(uint64_t device, bool byStatx) {
    device_.store(device, std::memory_order_release);
    mountCheckedByStatx_.store(byStatx, std::memory_order_release);
  }

 private:
  const Clock::time_point queued_;
  std::atomic<int64_t> startRunQueueWaitNs_{-1};
  std::atomic<Clock::rep> startedTicks_{0};
  std::atomic<uint64_t> threadId_{0};
  std::atomic<uint64_t> device_{0};
  std::atomic<bool> mountCheckedByStatx_{false};
  folly::Synchronized<CurrentStep, std::mutex> step_;
};

struct MountHealthCheckOptions {
  /** See `mount:mount-health-check-statx`. */
  bool useStatx{false};
  /** The EdenFS mount's device from an earlier probe, or 0 if unknown. */
  uint64_t knownDevice{0};
};

/**
 * The probe's step and timing as JSON fields, for reporting a probe that ran
 * too long.
 */
folly::dynamic describeProbeProgress(
    const MountHealthProbeProgress& progress,
    MountHealthProbeProgress::Clock::time_point now);

/**
 * The kernel's view of a thread of this process: its run state, the kernel
 * function it sleeps in ("0" while running), and its total time waiting for a
 * CPU, if known. Linux only.
 */
struct ThreadKernelState {
  char state{'?'};
  std::string wchan;
  std::optional<std::chrono::nanoseconds> runQueueWait;
};

std::optional<ThreadKernelState> readThreadKernelState(uint64_t threadId);

/**
 * Where a running probe's thread waits, as JSON fields. Empty when the probe
 * is not running or the platform has no /proc.
 */
folly::dynamic describeProbeThread(const MountHealthProbeProgress& progress);

/**
 * The host's 10 second CPU, memory and IO pressure (PSI) averages as JSON
 * fields. Empty without /proc/pressure. Linux only.
 */
folly::dynamic describeSystemPressure();

/**
 * The number of requests the kernel has queued or sent to EdenFS on the FUSE
 * connection of the mount on `device`, from /sys/fs/fuse/connections. Unset if
 * the connection is unknown or the file cannot be read.
 */
std::optional<uint64_t> readFuseConnectionWaiting(uint64_t device);

namespace detail {

/** The run state from /proc/<pid>/task/<tid>/stat. */
std::optional<char> parseTaskStatState(std::string_view stat);
/** The avg10 values of a PSI file such as /proc/pressure/memory. */
struct PressureAvg10 {
  std::optional<double> some;
  std::optional<double> full;
};
PressureAvg10 parsePressure(std::string_view pressure);

/** The CPU wait from /proc/<pid>/task/<tid>/schedstat. */
std::optional<std::chrono::nanoseconds> parseSchedstatRunQueueWait(
    std::string_view schedstat);

} // namespace detail

/**
 * The device of the filesystem mounted at `path`, from statx(2) without
 * syncing attributes, so FUSE sends no request. Unset if `path` is not a mount
 * root, or the kernel cannot tell. Linux only.
 */
std::optional<uint64_t> mountRootDevice(const std::string& path);

// Checks a daemon-reported RUNNING mount against the kernel mount table and
// lightweight .eden path probes. Returns an issue only when the mount is
// clearly missing, disconnected, timing out, or returning path access errors.
std::optional<EdenMountHealthCheckIssue> checkRunningEdenMountHealth(
    const std::string& mountPath,
    MountHealthProbeProgress* progress = nullptr,
    MountHealthCheckOptions options = {});

} // namespace facebook::eden
