/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include <folly/ThreadLocal.h>
#include <folly/synchronization/Baton.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <chrono>
#include <thread>
#include <utility>

#include "eden/common/utils/ProcessInfoCache.h"
#include "eden/fs/utils/ProcessAccessLog.h"

using ::testing::Contains;
using ::testing::ElementsAre;
using ::testing::Eq;
using ::testing::Key;
using ::testing::UnorderedElementsAre;
using namespace facebook::eden;
using namespace std::chrono_literals;

TEST(ProcessAccessLog, emptyLogHasNoAccesses) {
  auto log = ProcessAccessLog{std::make_shared<ProcessInfoCache>()};
  EXPECT_THAT(log.getAccessCounts(10s), ElementsAre());
}

TEST(ProcessAccessLog, accessIncrementsAccessCount) {
  auto pid = pid_t{42};
  auto log = ProcessAccessLog{std::make_shared<ProcessInfoCache>()};

  log.recordAccess(pid, ProcessAccessLog::AccessType::FsChannelRead);
  log.recordAccess(pid, ProcessAccessLog::AccessType::FsChannelWrite);
  log.recordAccess(pid, ProcessAccessLog::AccessType::FsChannelOther);
  log.recordAccess(
      pid, ProcessAccessLog::AccessType::FsChannelBackingStoreImport);

  auto ac = AccessCounts{};
  ac.fsChannelTotal() = 3;
  ac.fsChannelReads() = 1;
  ac.fsChannelWrites() = 1;
  ac.fsChannelBackingStoreImports() = 1;

  EXPECT_THAT(log.getAccessCounts(10s), Contains(std::pair{pid, ac}));
}

TEST(ProcessAccessLog, accessAddsProcessToProcessInfoCache) {
  auto pid = pid_t{1};
  auto processInfoCache = std::make_shared<ProcessInfoCache>();
  auto log = ProcessAccessLog{processInfoCache};
  log.recordAccess(pid, ProcessAccessLog::AccessType::FsChannelOther);
  EXPECT_THAT(processInfoCache->getAllProcessInfos(), Contains(Key(Eq(pid))));
}

// Each log has its own bucket on every thread, so one thread can record
// into several logs and a log destroyed under it does not take the
// others' counts with it.
TEST(ProcessAccessLog, oneThreadRecordingIntoTwoLogs) {
  auto processInfoCache = std::make_shared<ProcessInfoCache>();
  auto first = std::make_unique<ProcessAccessLog>(processInfoCache);
  auto second = std::make_unique<ProcessAccessLog>(processInfoCache);

  std::thread worker([&] {
    first->recordAccess(1, ProcessAccessLog::AccessType::FsChannelRead);
    second->recordAccess(2, ProcessAccessLog::AccessType::FsChannelRead);
    EXPECT_THAT(first->getAccessCounts(10s), UnorderedElementsAre(Key(1)));
    EXPECT_THAT(second->getAccessCounts(10s), UnorderedElementsAre(Key(2)));

    first.reset();
    second->recordAccess(3, ProcessAccessLog::AccessType::FsChannelRead);
    EXPECT_THAT(
        second->getAccessCounts(10s), UnorderedElementsAre(Key(2), Key(3)));

    // A log created after `first` was destroyed, possibly at the same address,
    // starts with an empty bucket on this thread.
    auto third = std::make_unique<ProcessAccessLog>(processInfoCache);
    third->recordAccess(4, ProcessAccessLog::AccessType::FsChannelRead);
    EXPECT_THAT(third->getAccessCounts(10s), UnorderedElementsAre(Key(4)));
    EXPECT_THAT(
        second->getAccessCounts(10s), UnorderedElementsAre(Key(2), Key(3)));
  });
  worker.join();
}

// folly's pthread emulation on Windows never runs thread-exit destructors, so
// `~ExitBlocker` would never run there and the test would wait forever.
#ifndef _WIN32

namespace {

// Held by the exiting thread between folly unlinking its thread-local entries
// and destroying them, which is the window in which another thread can destroy
// the log.
struct ExitBlocker {
  folly::Baton<>* exiting;
  folly::Baton<>* destroying;

  ~ExitBlocker() {
    exiting->post();
    destroying->wait();
    // Give the main thread time to enter `~ProcessAccessLog`.
    // @lint-ignore CLANGTIDY facebook-hte-BadCall-sleep_for
    std::this_thread::sleep_for(std::chrono::milliseconds(50));
  }
};

using ExitBlockerPtr = folly::ThreadLocalPtr<
    ExitBlocker,
    ThreadLocalBucketTag,
    ThreadLocalBucketAccessMode>;

// Thread-local entries are destroyed in slot order, and folly assigns a slot
// on first use, reusing freed slots. Taking this slot at static initialization,
// before any log exists, gives it the lowest slot, so an exiting thread runs
// `~ExitBlocker` before it destroys any log's bucket.
struct ExitBlockerSlot {
  ExitBlockerPtr ptr;
  ExitBlockerSlot() {
    ptr.get();
  }
};
// @lint-ignore CLANGTIDY facebook-avoid-non-const-global-variables
ExitBlockerSlot exitBlocker;

} // namespace

// A thread that exits while another thread destroys the log it recorded into
// must merge its bucket into a live log. With the default thread-local access
// mode this is a use-after-free that ASAN reports.
TEST(ProcessAccessLog, threadExitRacingWithLogDestruction) {
  auto processInfoCache = std::make_shared<ProcessInfoCache>();
  auto log = std::make_unique<ProcessAccessLog>(processInfoCache);
  folly::Baton<> exiting;
  folly::Baton<> destroying;

  std::thread worker([&] {
    exitBlocker.ptr.reset(std::make_unique<ExitBlocker>(&exiting, &destroying));
    log->recordAccess(1, ProcessAccessLog::AccessType::FsChannelRead);
  });
  exiting.wait();
  destroying.post();
  log.reset();
  worker.join();

  EXPECT_THAT(processInfoCache->getAllProcessInfos(), Contains(Key(Eq(1))));
}

#endif // !_WIN32
