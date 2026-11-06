/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/inodes/CheckoutContext.h"

#include <folly/ScopeGuard.h>
#include <folly/logging/test/TestLogHandler.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <algorithm>
#include <thread>

#include "eden/common/utils/FaultInjector.h"
#include "eden/fs/testharness/FakeBackingStore.h"
#include "eden/fs/testharness/FakeTreeBuilder.h"
#include "eden/fs/testharness/TestMount.h"
#include "eden/fs/utils/EdenError.h"

using namespace facebook::eden;

TEST(CheckoutContextTest, errorsAreReturnedAsConflicts) {
  TestMount testMount{FakeTreeBuilder{}};
  CheckoutContext context{
      testMount.getEdenMount().get(),
      CheckoutMode::DRY_RUN,
      std::nullopt,
      __func__};

  context.addError(
      testMount.getRootInode().get(),
      "error.txt"_pc,
      folly::make_exception_wrapper<std::runtime_error>("checkout error"));

  auto conflicts = std::move(context.flush()).get();
  ASSERT_EQ(1, conflicts.size());
  EXPECT_EQ("error.txt", conflicts[0].path().value());
  EXPECT_EQ(ConflictType::ERROR, conflicts[0].type().value());
  EXPECT_THAT(
      conflicts[0].message().value(), testing::EndsWith(": checkout error"));
}

TEST(CheckoutContextTest, RenameLockAllowsCrossThreadRelease) {
  TestMount testMount{FakeTreeBuilder{}};

  auto renameLock = testMount.getEdenMount()->acquireRenameLock();
  std::thread releaser{
      [renameLock = std::move(renameLock)]() mutable { renameLock.unlock(); }};
  releaser.join();

  auto sharedLock = testMount.getEdenMount()->acquireSharedRenameLock();
  EXPECT_TRUE(sharedLock.owns_lock());
}

TEST(CheckoutContextTest, logsCheckoutErrorOnceAtBoundary) {
  auto logHandler = std::make_shared<folly::TestLogHandler>();
  auto* logCategory =
      folly::LoggerDB::get().getCategory("eden/fs/inodes/EdenMount");
  auto previousHandlers = logCategory->getHandlers();
  logCategory->replaceHandlers({logHandler});
  SCOPE_EXIT {
    logCategory->replaceHandlers(std::move(previousHandlers));
  };

  FakeTreeBuilder currentBuilder;
  currentBuilder.setFile("dir/file.txt", "current\n");
  TestMount testMount{RootId{"current"}, currentBuilder};
  auto targetBuilder = currentBuilder.clone();
  targetBuilder.replaceFile("dir/file.txt", "target\n");
  targetBuilder.finalize(testMount.getBackingStore(), true);
  testMount.getBackingStore()
      ->putCommit(RootId{"target"}, targetBuilder)
      ->setReady();

  testMount.getServerState()->getFaultInjector().injectError(
      "TreeInode::checkout",
      ".*",
      folly::make_exception_wrapper<std::runtime_error>(
          "intentional checkout error"),
      1);

  auto checkoutResult = testMount.checkout(
      RootId{"target"}, ObjectFetchContext::getNullContext(), __func__);
  testMount.drainServerExecutor();
  ASSERT_TRUE(checkoutResult.isReady());
  EXPECT_THROW(std::move(checkoutResult).get(), EdenError);
  EXPECT_TRUE(testMount.getEdenMount()->isCheckoutInProgress());

  const auto messages = logHandler->getMessageValues();
  EXPECT_EQ(
      1,
      std::count_if(messages.begin(), messages.end(), [](const auto& message) {
        return message.find("intentional checkout error") != std::string::npos;
      }));
}
