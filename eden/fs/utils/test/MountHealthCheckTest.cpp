/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/utils/MountHealthCheck.h"

#include <folly/portability/GTest.h>

namespace facebook::eden {
namespace {

#ifdef __linux__
TEST(MountHealthCheckTest, reportsMissingKernelMountForNonEdenMount) {
  auto issue = checkRunningEdenMountHealth("/");

  ASSERT_TRUE(issue.has_value());
  EXPECT_EQ(
      EdenMountHealthIssueReason::DaemonRunningKernelMountMissing,
      issue->reason);
}
#endif

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
}

} // namespace
} // namespace facebook::eden
