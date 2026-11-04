/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/utils/MountHealthCheck.h"

#include <folly/portability/GTest.h>
#include <folly/system/ThreadId.h>

namespace facebook::eden {
namespace {

#ifdef __linux__
TEST(MountHealthCheckTest, reportsMissingKernelMountForNonEdenMount) {
  MountHealthProbeProgress progress{std::chrono::steady_clock::now()};
  progress.start();
  auto issue = checkRunningEdenMountHealth("/", &progress);

  ASSERT_TRUE(issue.has_value());
  EXPECT_EQ(
      EdenMountHealthIssueReason::DaemonRunningKernelMountMissing,
      issue->reason);
  EXPECT_EQ(MountHealthProbeStep::MountTable, progress.step());
}
#endif

TEST(MountHealthCheckTest, describesAStuckProbe) {
  using namespace std::chrono_literals;
  const auto queued = MountHealthProbeProgress::Clock::now();
  MountHealthProbeProgress progress{queued};
  progress.start(queued + 2s);
  progress.enter(MountHealthProbeStep::LookupChild, queued + 3s);

  const folly::dynamic expected =
      folly::dynamic::object("step", "lookup_child")("step_ms", 5000)(
          "queue_wait_ms", 2000)("running_ms", 6000);
  EXPECT_EQ(expected, describeProbeProgress(progress, queued + 8s));
}

TEST(MountHealthCheckTest, describesAProbeThatNeverStarted) {
  using namespace std::chrono_literals;
  const auto queued = MountHealthProbeProgress::Clock::now();
  MountHealthProbeProgress progress{queued};

  const folly::dynamic expected =
      folly::dynamic::object("step", "queued")("step_ms", 5000);
  EXPECT_EQ(expected, describeProbeProgress(progress, queued + 5s));
}

TEST(MountHealthCheckTest, parsesTaskStatState) {
  EXPECT_EQ('D', detail::parseTaskStatState("42 (a) b) D 1 42 42 0 -1"));
  EXPECT_EQ(std::nullopt, detail::parseTaskStatState("42 (truncated"));
}

TEST(MountHealthCheckTest, parsesPressure) {
  auto pressure = detail::parsePressure(
      "some avg10=1.50 avg60=0.10 avg300=0.00 total=12\n"
      "full avg10=0.25 avg60=0.00 avg300=0.00 total=3\n");
  EXPECT_EQ(1.5, pressure.some);
  EXPECT_EQ(0.25, pressure.full);
}

TEST(MountHealthCheckTest, parsesSchedstatRunQueueWait) {
  EXPECT_EQ(
      std::chrono::nanoseconds{2500000},
      detail::parseSchedstatRunQueueWait("9100 2500000 7\n"));
  EXPECT_EQ(std::nullopt, detail::parseSchedstatRunQueueWait("9100"));
}

#ifdef __linux__
TEST(MountHealthCheckTest, readsTheStateOfARunningThread) {
  auto state = readThreadKernelState(folly::getOSThreadID());

  ASSERT_TRUE(state.has_value());
  EXPECT_EQ('R', state->state);
}
#endif

TEST(MountHealthCheckTest, startedProbeRecordsItsThread) {
  MountHealthProbeProgress progress{std::chrono::steady_clock::now()};
  EXPECT_FALSE(progress.started().has_value());
  EXPECT_EQ(0, progress.threadId());

  progress.start();
  EXPECT_TRUE(progress.started().has_value());
  EXPECT_EQ(folly::getOSThreadID(), progress.threadId());
  EXPECT_EQ(MountHealthProbeStep::Starting, progress.step());
}

TEST(MountHealthCheckTest, reasonStringsMatchTelemetryContract) {
  EXPECT_EQ(
      "daemon_running_kernel_mount_missing",
      edenMountHealthIssueReasonString(
          EdenMountHealthIssueReason::DaemonRunningKernelMountMissing));
  EXPECT_EQ(
      "daemon_running_dot_eden_missing",
      edenMountHealthIssueReasonString(
          EdenMountHealthIssueReason::DaemonRunningDotEdenMissing));
  EXPECT_EQ(
      "daemon_running_mount_not_connected",
      edenMountHealthIssueReasonString(
          EdenMountHealthIssueReason::DaemonRunningMountNotConnected));
  EXPECT_EQ(
      "daemon_running_mount_timed_out",
      edenMountHealthIssueReasonString(
          EdenMountHealthIssueReason::DaemonRunningMountTimedOut));
  EXPECT_EQ(
      "daemon_running_mount_access_error",
      edenMountHealthIssueReasonString(
          EdenMountHealthIssueReason::DaemonRunningMountAccessError));
  EXPECT_EQ(
      "daemon_running_checkout_not_mounted",
      edenMountHealthIssueReasonString(
          EdenMountHealthIssueReason::DaemonRunningCheckoutNotMounted));
  EXPECT_EQ(
      "daemon_running_checkout_flapping",
      edenMountHealthIssueReasonString(
          EdenMountHealthIssueReason::DaemonRunningCheckoutFlapping));
  EXPECT_EQ(
      "daemon_running_mount_check_not_started",
      edenMountHealthIssueReasonString(
          EdenMountHealthIssueReason::DaemonRunningMountCheckNotStarted));
}

TEST(MountHealthCheckTest, probeStepNamesMatchTelemetryContract) {
  EXPECT_EQ("queued", mountHealthProbeStepName(MountHealthProbeStep::Queued));
  EXPECT_EQ(
      "starting", mountHealthProbeStepName(MountHealthProbeStep::Starting));
  EXPECT_EQ(
      "mount_table",
      mountHealthProbeStepName(MountHealthProbeStep::MountTable));
  EXPECT_EQ(
      "stat_dot_eden",
      mountHealthProbeStepName(MountHealthProbeStep::StatDotEden));
  EXPECT_EQ(
      "lookup_child",
      mountHealthProbeStepName(MountHealthProbeStep::LookupChild));
  EXPECT_EQ("done", mountHealthProbeStepName(MountHealthProbeStep::Done));
}

} // namespace
} // namespace facebook::eden
