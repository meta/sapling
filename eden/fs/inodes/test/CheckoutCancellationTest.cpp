/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include <folly/CancellationToken.h>
#include <folly/executors/ManualExecutor.h>
#include <folly/futures/Future.h>
#include <folly/test/TestUtils.h>
#include <gtest/gtest.h>

#include "eden/common/utils/FaultInjector.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/RefPtr.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/config/ParentCommit.h"
#include "eden/fs/config/ReloadableConfig.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/telemetry/ErrorLogger.h"
#include "eden/fs/telemetry/test/CapturingXplatLogger.h"
#include "eden/fs/testharness/FakeBackingStore.h"
#include "eden/fs/testharness/FakeTreeBuilder.h"
#include "eden/fs/testharness/TestMount.h"

namespace facebook::eden {

class CheckoutCancellationTest : public ::testing::Test {};

/**
 * Test that when a checkout is cancelled, the mount enters an interrupted
 * checkout state and can be recovered.
 */
TEST_F(CheckoutCancellationTest, CheckoutLeavesInterruptedState) {
  auto builder1 = FakeTreeBuilder();
  builder1.setFile("file1.txt", "content1\n");
  builder1.setFile("file2.txt", "content2\n");
  builder1.setFile("dir/file3.txt", "content3\n");
  TestMount testMount{builder1, true, true};

  auto builder2 = builder1.clone();
  builder2.replaceFile("file1.txt", "modified content1\n");
  builder2.setFile("newfile.txt", "new content\n");
  builder2.removeFile("file2.txt");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit(RootId{"2"}, builder2);
  commit2->setReady();

  // Verify initial state - no checkout in progress
  auto initialParentCommit =
      testMount.getEdenMount()->getCheckoutConfig()->getParentCommit();
  EXPECT_FALSE(initialParentCommit.isCheckoutInProgress());
  EXPECT_EQ(RootId{"1"}, testMount.getEdenMount()->getCheckedOutRootId());

  // Block checkout with a fault
  folly::CancellationSource cancelSource;
  auto cancellationToken = cancelSource.getToken();

  testMount.getServerState()->getFaultInjector().injectBlockWithCancel(
      "checkout",
      ".*",
      cancellationToken,
      std::chrono::milliseconds{5000},
      0 // No expiration
  );

  testMount.drainServerExecutor();

  // Start the checkout operation - it will block on the fault
  auto checkoutFuture = testMount.checkout(
      RootId{"2"}, ObjectFetchContext::getNullContext(), __func__);
  testMount.drainServerExecutor();

  // Verify the future is not ready yet (blocked on fault)
  EXPECT_FALSE(checkoutFuture.isReady());

  // Trigger cancellation
  cancelSource.requestCancellation();

  // Poll until cancellation propagates through the coroutine system.
  auto deadline =
      std::chrono::steady_clock::now() + std::chrono::milliseconds(100);
  while (!checkoutFuture.isReady() &&
         std::chrono::steady_clock::now() < deadline) {
    testMount.drainServerExecutor();
  }

  // The checkout should have been cancelled
  EXPECT_TRUE(checkoutFuture.isReady());
  EXPECT_ANY_THROW(std::move(checkoutFuture).get());

  // Verify the mount is in interrupted checkout state
  EXPECT_TRUE(testMount.getEdenMount()->isCheckoutInProgress())
      << "After cancellation, checkout should be marked as in-progress "
      << "(interrupted state)";

  // Clean up - remove the fault
  testMount.getServerState()->getFaultInjector().removeFault("checkout", ".*");

  // Verify we can recover by performing another checkout
  auto recoveryFetchContext = ObjectFetchContext::getNullContext();
  auto recoveryFuture =
      testMount.checkout(RootId{"2"}, recoveryFetchContext, __func__);

  testMount.drainServerExecutor();
  EXPECT_TRUE(recoveryFuture.isReady());

  // Recovery checkout should succeed
  auto recoveryResult = std::move(recoveryFuture).get();
  EXPECT_EQ(0, recoveryResult.conflicts.size())
      << "Recovery checkout should complete without conflicts";

  // After recovery, checkout should no longer be in progress
  EXPECT_FALSE(testMount.getEdenMount()->isCheckoutInProgress())
      << "After recovery, checkout should be complete";

  // Verify we're now on commit 2
  EXPECT_EQ(RootId{"2"}, testMount.getEdenMount()->getCheckedOutRootId());

  // Verify file contents are correct
  auto file1Content = testMount.readFile("file1.txt");
  EXPECT_EQ("modified content1\n", file1Content);

  auto newFileContent = testMount.readFile("newfile.txt");
  EXPECT_EQ("new content\n", newFileContent);

  EXPECT_FALSE(testMount.hasFileAt("file2.txt"))
      << "file2.txt should have been removed";
}

/**
 * Test that cancellation works at the inodeCheckout stage (later in the
 * checkout flow, after diff computation and rename lock acquisition)
 */
TEST_F(CheckoutCancellationTest, CancellationAtInodeCheckoutStage) {
  auto builder1 = FakeTreeBuilder();
  builder1.setFile("file1.txt", "content1\n");
  builder1.setFile("file2.txt", "content2\n");
  builder1.setFile("dir/file3.txt", "content3\n");
  TestMount testMount{builder1, true, true};

  auto builder2 = builder1.clone();
  builder2.replaceFile("file1.txt", "modified content1\n");
  builder2.setFile("newfile.txt", "new content\n");
  builder2.removeFile("file2.txt");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit(RootId{"2"}, builder2);
  commit2->setReady();

  // Verify initial state - no checkout in progress
  auto initialParentCommit =
      testMount.getEdenMount()->getCheckoutConfig()->getParentCommit();
  EXPECT_FALSE(initialParentCommit.isCheckoutInProgress());
  EXPECT_EQ(RootId{"1"}, testMount.getEdenMount()->getCheckedOutRootId());

  // Block checkout at the inodeCheckout stage
  folly::CancellationSource cancelSource;
  auto cancellationToken = cancelSource.getToken();

  testMount.getServerState()->getFaultInjector().injectBlockWithCancel(
      "inodeCheckout",
      ".*",
      cancellationToken,
      std::chrono::milliseconds{5000},
      0 // No expiration
  );

  testMount.drainServerExecutor();

  // Start the checkout operation - it will block on the inodeCheckout fault
  auto checkoutFuture = testMount.checkout(
      RootId{"2"}, ObjectFetchContext::getNullContext(), __func__);
  testMount.drainServerExecutor();

  // Verify the future is not ready yet (blocked on inodeCheckout fault)
  EXPECT_FALSE(checkoutFuture.isReady());

  // Trigger cancellation
  cancelSource.requestCancellation();

  // Poll until cancellation propagates through the coroutine system.
  auto deadline =
      std::chrono::steady_clock::now() + std::chrono::milliseconds(100);
  while (!checkoutFuture.isReady() &&
         std::chrono::steady_clock::now() < deadline) {
    testMount.drainServerExecutor();
  }

  // The checkout should have been cancelled
  EXPECT_TRUE(checkoutFuture.isReady());
  EXPECT_ANY_THROW(std::move(checkoutFuture).get());

  // Verify the mount is in interrupted checkout state
  EXPECT_TRUE(testMount.getEdenMount()->isCheckoutInProgress())
      << "After cancellation at inodeCheckout stage, checkout should be marked "
      << "as in-progress (interrupted state)";

  // Clean up - remove the fault
  testMount.getServerState()->getFaultInjector().removeFault(
      "inodeCheckout", ".*");

  // Verify we can recover by performing another checkout
  auto recoveryFetchContext = ObjectFetchContext::getNullContext();
  auto recoveryFuture =
      testMount.checkout(RootId{"2"}, recoveryFetchContext, __func__);

  testMount.drainServerExecutor();
  EXPECT_TRUE(recoveryFuture.isReady());

  // Recovery checkout should succeed
  auto recoveryResult = std::move(recoveryFuture).get();
  EXPECT_EQ(0, recoveryResult.conflicts.size())
      << "Recovery checkout should complete without conflicts";

  // After recovery, checkout should no longer be in progress
  EXPECT_FALSE(testMount.getEdenMount()->isCheckoutInProgress())
      << "After recovery, checkout should be complete";

  // Verify we're now on commit 2
  EXPECT_EQ(RootId{"2"}, testMount.getEdenMount()->getCheckedOutRootId());

  // Verify file contents are correct
  auto file1Content = testMount.readFile("file1.txt");
  EXPECT_EQ("modified content1\n", file1Content);

  auto newFileContent = testMount.readFile("newfile.txt");
  EXPECT_EQ("new content\n", newFileContent);

  EXPECT_FALSE(testMount.hasFileAt("file2.txt"))
      << "file2.txt should have been removed";
}

TEST_F(CheckoutCancellationTest, CancelledCheckoutLogsError) {
  CapturingXplatLogger xplatLogger;
  auto config = EdenConfig::createTestEdenConfig();
  config->enableErrorLogging.setValue(true, ConfigSourceType::UserConfig);
  auto reloadableConfig = std::make_shared<ReloadableConfig>(config);
  auto errorLogger =
      std::make_shared<ErrorLogger>(reloadableConfig, &xplatLogger);

  auto builder1 = FakeTreeBuilder();
  builder1.setFile("file1.txt", "content1\n");
  TestMount testMount{
      builder1, true, true, kPathMapDefaultCaseSensitive, errorLogger};

  auto builder2 = builder1.clone();
  builder2.replaceFile("file1.txt", "modified\n");
  builder2.finalize(testMount.getBackingStore(), true);
  auto commit2 = testMount.getBackingStore()->putCommit(RootId{"2"}, builder2);
  commit2->setReady();

  folly::CancellationSource cancelSource;
  auto cancellationToken = cancelSource.getToken();

  testMount.getServerState()->getFaultInjector().injectBlockWithCancel(
      "checkout", ".*", cancellationToken, std::chrono::milliseconds{5000}, 0);

  testMount.drainServerExecutor();

  auto checkoutFuture = testMount.checkout(
      RootId{"2"}, ObjectFetchContext::getNullContext(), __func__);
  testMount.drainServerExecutor();

  cancelSource.requestCancellation();

  auto deadline =
      std::chrono::steady_clock::now() + std::chrono::milliseconds(100);
  while (!checkoutFuture.isReady() &&
         std::chrono::steady_clock::now() < deadline) {
    testMount.drainServerExecutor();
  }

  EXPECT_TRUE(checkoutFuture.isReady());
  EXPECT_ANY_THROW(std::move(checkoutFuture).get());

  ASSERT_GE(xplatLogger.events().size(), 1);
  const auto& strings = xplatLogger.events()[0].event.getStringMap();
  EXPECT_EQ(strings.at("component"), "object_store");
  EXPECT_EQ(strings.at("error_type"), "checkout_update_error");

  testMount.getServerState()->getFaultInjector().removeFault("checkout", ".*");
}

} // namespace facebook::eden
