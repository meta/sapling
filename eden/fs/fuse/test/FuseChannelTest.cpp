/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/fuse/FuseChannel.h"
#include "eden/fs/fuse/FuseTransport.h"
#include "eden/fs/fuse/IoUringFuseTransport.h"

#include <folly/Random.h>
#include <folly/ScopeGuard.h>
#include <folly/Synchronized.h>
#include <folly/executors/GlobalExecutor.h>
#include <folly/logging/xlog.h>
#include <folly/test/TestUtils.h>
#include <gtest/gtest.h>
#if EDEN_HAVE_FUSE_IO_URING
#include <fcntl.h>
#include <folly/File.h>
#include <sys/eventfd.h>
#include <sys/resource.h>
#include <sys/sysinfo.h>
#include <sys/utsname.h>
#endif
#include <atomic>
#include <cerrno>
#include <system_error>
#include <thread>
#include <utility>
#include <vector>

#include "eden/common/utils/EnumValue.h"
#include "eden/common/utils/ProcessInfoCache.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/config/ReloadableConfig.h"
#include "eden/fs/fuse/FuseDispatcher.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/telemetry/ErrorLogger.h"
#include "eden/fs/telemetry/test/CapturingXplatLogger.h"
#include "eden/fs/testharness/FakeFuse.h"
#include "eden/fs/testharness/TestDispatcher.h"

using namespace std::chrono_literals;
using folly::ByteRange;
using folly::Random;

namespace facebook::eden {
namespace {

folly::Logger straceLogger{"eden.strace"};

// Most of the tests wait on Futures to complete.
// Define a timeout just to make sure the tests don't hang if there's a problem
// and a future never completes.  1 second seems to be long enough for the tests
// to pass even when the system is under fairly heavy CPU load.
constexpr auto kTimeout = 1s;
constexpr size_t kTraceBusCapacity = 25000;

fuse_entry_out genRandomLookupResponse(uint64_t nodeid) {
  fuse_entry_out response{};
  response.nodeid = nodeid;
  response.generation = Random::rand64();
  response.entry_valid = Random::rand64();
  response.attr_valid = Random::rand64();
  response.entry_valid_nsec = Random::rand32();
  response.attr_valid_nsec = Random::rand32();
  response.attr.ino = nodeid;
  response.attr.size = Random::rand64();
  response.attr.blocks = Random::rand64();
  response.attr.atime = Random::rand64();
  response.attr.mtime = Random::rand64();
  response.attr.ctime = Random::rand64();
  response.attr.atimensec = Random::rand32();
  response.attr.mtimensec = Random::rand32();
  response.attr.ctimensec = Random::rand32();
  response.attr.mode = Random::rand32();
  response.attr.nlink = Random::rand32();
  response.attr.uid = Random::rand32();
  response.attr.gid = Random::rand32();
  response.attr.rdev = Random::rand32();
  response.attr.blksize = Random::rand32();
  response.attr.flags = Random::rand32();
  return response;
}

class ForgetRecordingDispatcher : public TestDispatcher {
 public:
  using TestDispatcher::TestDispatcher;
  using Forget = std::pair<uint64_t, uint64_t>;

  void forget(InodeNumber ino, unsigned long nlookup) override {
    forgets_.wlock()->emplace_back(ino.get(), nlookup);
  }

  std::vector<Forget> getForgets() const {
    return *forgets_.rlock();
  }

 private:
  folly::Synchronized<std::vector<Forget>> forgets_;
};

#if EDEN_HAVE_FUSE_IO_URING
class IoUringReader {
 public:
  IoUringReader(FuseChannel& channel, FakeFuse& fuse)
      : channel_{channel}, fuse_{fuse} {}

  ~IoUringReader() {
    stop();
    if (thread_.joinable()) {
      if (!stopped_.wait(kTimeout).isReady()) {
        ADD_FAILURE() << "io_uring /dev/fuse reader did not stop";
        if (fuse_.isStarted()) {
          fuse_.close();
        }
      }
      thread_.join();
    }
    XCHECK(waitForRequestsToFinish())
        << "pending requests still reference the reader transport";
  }

  void start() {
    // The first processSession worker reads /dev/fuse without creating a ring.
    thread_ = std::thread([this] {
      stoppedPromise_.setWith([this] {
        transport_.processSession(channel_);
        return folly::unit;
      });
    });
  }

  void stop() {
    channel_.takeoverStop();
    transport_.requestStopWakeup();
  }

  void join() {
    if (!stopped_.wait(kTimeout).isReady()) {
      throw std::runtime_error("io_uring /dev/fuse reader did not stop");
    }
    thread_.join();
    std::move(stopped_).get();
  }

  bool waitForRequestsToFinish() {
    const auto deadline = std::chrono::steady_clock::now() + kTimeout;
    while (channel_.hasPendingRequests() &&
           std::chrono::steady_clock::now() < deadline) {
      std::this_thread::yield();
    }
    return !channel_.hasPendingRequests();
  }

 private:
  FuseChannel& channel_;
  FakeFuse& fuse_;
  IoUringFuseTransport transport_{8};
  folly::Promise<folly::Unit> stoppedPromise_;
  folly::Future<folly::Unit> stopped_{stoppedPromise_.getFuture()};
  std::thread thread_;
};
#endif

class FuseChannelTest : public ::testing::Test {
 protected:
  std::unique_ptr<FuseChannel, FsChannelDeleter> createChannel(
      size_t numThreads = 2,
      uint32_t fuseMaxPages = 0,
      bool useIoUring = false,
      std::string ioUringKernelReleaseRegex = {},
      bool ioUringPreCreateQueues = false,
      size_t numInvalidationThreads = 4,
      bool handleKillPrivV2 = true) {
    auto testDispatcher =
        std::make_unique<ForgetRecordingDispatcher>(stats_.copy());
    dispatcher_ = testDispatcher.get();
    return makeFuseChannel(
        nullptr,
        fuse_.start(),
        mountPath_,
        folly::getUnsafeMutableGlobalCPUExecutor(),
        numThreads,
        std::move(testDispatcher),
        &straceLogger,
        std::make_shared<ProcessInfoCache>(),
        /*fsEventLogger=*/nullptr,
        /*edenFsEventsLogger=*/nullptr,
        /*errorLogger=*/
        errorLoggerOverride_ ? *errorLoggerOverride_ : noopErrorLogger_,
        std::chrono::seconds(60),
        /*notifications=*/nullptr,
        /*requireUtf8Path=*/true,
        /*maximumBackgroundRequests=*/12,
        /*maximumInFlightRequests=*/1000,
        /*highFuseRequestsLogInterval=*/std::chrono::minutes{10},
        /*longRunningFSRequestThreshold=*/std::chrono::minutes{5},
        /*useWriteBackCache=*/false,
        /*fuseTraceBusCapacity*/ kTraceBusCapacity,
        /*fuseBdiReadAheadKb=*/std::nullopt,
        /*fuseMaxPages=*/fuseMaxPages,
        /*handleKillPrivV2=*/handleKillPrivV2,
        /*useIoUring=*/useIoUring,
        std::move(ioUringKernelReleaseRegex),
        /*ioUringQueueDepth=*/8,
        /*ioUringDisableIoWait=*/true,
        /*ioUringSkipSelfWakeup=*/false,
        ioUringPreCreateQueues,
        numInvalidationThreads);
  }

  FuseChannel::StopFuture performInit(
      FuseChannel* channel,
      uint32_t majorVersion = FUSE_KERNEL_VERSION,
      uint32_t minorVersion = FUSE_KERNEL_MINOR_VERSION,
      uint32_t maxReadahead = 0,
      uint32_t flags = 0,
      uint32_t flags2 = 0) {
    auto initFuture = channel->initialize();
    EXPECT_FALSE(initFuture.isReady());

    // Send the INIT packet
    struct fuse_init_in initArg = {};
    initArg.major = majorVersion;
    initArg.minor = minorVersion;
    initArg.max_readahead = maxReadahead;
    initArg.flags = flags;
#ifdef FUSE_INIT_EXT
    initArg.flags2 = flags2;
#else
    (void)flags2;
#endif
    auto reqID = fuse_.sendRequest(FUSE_INIT, FUSE_ROOT_ID, initArg);

    // Wait for the INIT response
    auto response = fuse_.recvResponse();
    EXPECT_EQ(reqID, response.header.unique);
    EXPECT_EQ(0, response.header.error);
    EXPECT_EQ(
        sizeof(fuse_out_header) + sizeof(fuse_init_out), response.header.len);
    EXPECT_EQ(sizeof(fuse_init_out), response.body.size());

    // The init future should be ready very shortly after we receive the INIT
    // response.  The FuseChannel initialization thread makes the future ready
    // shortly after sending the INIT response.
    return std::move(initFuture).get(kTimeout);
  }

  void expectForgetsWithoutReply(
      FuseChannel& channel,
      const std::vector<ForgetRecordingDispatcher::Forget>& expected) {
    // With one worker, this lookup starts after the FORGET handler finishes.
    auto lookupId = fuse_.sendLookup(FUSE_ROOT_ID, "after-forget");
    auto lookup = dispatcher_->waitForLookup(lookupId, kTimeout);
    EXPECT_EQ(expected, dispatcher_->getForgets());
    lookup.promise.setValue(genRandomLookupResponse(99));

    auto response = fuse_.recvResponse();
    EXPECT_EQ(lookupId, response.header.unique);
    EXPECT_EQ(0, response.header.error);
    EXPECT_EQ(sizeof(fuse_entry_out), response.body.size());
    EXPECT_FALSE(channel.isStopRequested());
  }

  void expectSingleForgetWithoutReply(FuseChannel& channel) {
    const fuse_forget_in arg{.nlookup = 3};
    fuse_.sendRequest(FUSE_FORGET, 17, arg);
    expectForgetsWithoutReply(channel, {{17, 3}});
  }

  void expectBatchForgetWithoutReply(FuseChannel& channel) {
    const struct {
      fuse_batch_forget_in header;
      fuse_forget_one entries[2];
    } arg{
        .header = {.count = 2, .dummy = 0},
        .entries = {
            {.nodeid = 17, .nlookup = 3}, {.nodeid = 29, .nlookup = 5}}};
    static_assert(
        sizeof(arg) ==
        sizeof(fuse_batch_forget_in) + 2 * sizeof(fuse_forget_one));
    fuse_.sendRequest(FUSE_BATCH_FORGET, 0, arg);
    expectForgetsWithoutReply(channel, {{17, 3}, {29, 5}});
  }

  FakeFuse fuse_;
  EdenStatsPtr stats_ = makeRefPtr<EdenStats>();
  ErrorLogger noopErrorLogger_{};
  // Error-logging tests inject capturingErrorLogger_ via errorLoggerOverride_
  // so they can inspect what was sent to XplatLogger.
  CapturingXplatLogger xplatLogger_;
  std::shared_ptr<EdenConfig> edenConfig_ = EdenConfig::createTestEdenConfig();
  std::shared_ptr<ReloadableConfig> reloadableConfig_ =
      std::make_shared<ReloadableConfig>(edenConfig_);
  ErrorLogger capturingErrorLogger_{reloadableConfig_, &xplatLogger_};
  ErrorLogger* errorLoggerOverride_ = nullptr;
  ForgetRecordingDispatcher* dispatcher_{};
  AbsolutePath mountPath_{canonicalPath("/fake/mount/path")};
};

} // namespace

TEST_F(FuseChannelTest, testDestroyNeverInitialized) {
  // Create a FuseChannel and then destroy it without ever calling initialize()
  auto channel = createChannel();
}

TEST_F(FuseChannelTest, testInitDestroy) {
  // Initialize the FuseChannel then immediately invoke its destructor
  // without explicitly requesting it to stop or receiving a close on the fUSE
  // device.
  auto channel = createChannel();
  performInit(channel.get());
}

TEST_F(FuseChannelTest, requestMetricsDuringWorkerInitialization) {
  constexpr size_t kWorkerCount = 32;
  auto channel = createChannel(kWorkerCount);

  std::atomic<bool> keepPolling{true};
  std::atomic<bool> observedActiveRequest{false};
  std::atomic<size_t> pollCount{0};
  std::thread statsThread{[&] {
    while (keepPolling.load(std::memory_order_acquire)) {
      if (channel->getRequestMetric(RequestMetricsScope::COUNT) > 0) {
        observedActiveRequest.store(true, std::memory_order_release);
      }
      pollCount.fetch_add(1, std::memory_order_release);
    }
  }};
  SCOPE_EXIT {
    keepPolling.store(false, std::memory_order_release);
    statsThread.join();
  };

  const auto waitForPollAfter = [&](size_t previousCount) {
    const auto deadline = std::chrono::steady_clock::now() + kTimeout;
    while (pollCount.load(std::memory_order_acquire) <= previousCount &&
           std::chrono::steady_clock::now() < deadline) {
      std::this_thread::yield();
    }
    return pollCount.load(std::memory_order_acquire) > previousCount;
  };
  ASSERT_TRUE(waitForPollAfter(0));

  // Exercise metric reads while workers publish their per-thread watch lists.
  // TSan verifies publication is race-free, while the assertion below requires
  // the polling thread to observe an active request in non-sanitized builds.
  performInit(channel.get());

  auto requestId = fuse_.sendLookup(FUSE_ROOT_ID, "worker-ready");
  auto request = dispatcher_->waitForLookup(requestId);

  const auto deadline = std::chrono::steady_clock::now() + kTimeout;
  while (!observedActiveRequest.load(std::memory_order_acquire) &&
         std::chrono::steady_clock::now() < deadline) {
    std::this_thread::yield();
  }
  ASSERT_TRUE(observedActiveRequest.load(std::memory_order_acquire));

  request.promise.setValue(genRandomLookupResponse(2));
  EXPECT_EQ(requestId, fuse_.recvResponse().header.unique);
}

TEST_F(FuseChannelTest, testDestroyWithPendingInit) {
  // Create a FuseChannel, call initialize(), and then destroy the FuseChannel
  // without ever having seen the INIT request from the kernel.
  auto channel = createChannel();
  auto initFuture = channel->initialize();
  EXPECT_FALSE(initFuture.isReady());
}

TEST_F(FuseChannelTest, testInitDestroyRace) {
  // Send an INIT request and immediately destroy the FuseChannelTest
  // without waiting for initialization to complete.
  auto channel = createChannel();
  auto initFuture = channel->initialize();
  fuse_.sendInitRequest();
  channel.reset();

  // Wait for the initialization future to complete.
  // It's fine if it fails if the channel was destroyed before initialization
  // completed, or its fine if it succeeded first too.
  initFuture.wait(kTimeout);
}

TEST_F(FuseChannelTest, testInitUnmount) {
  auto channel = createChannel();
  auto completeFuture = performInit(channel.get());

  // Close the FakeFuse so that FuseChannel will think the mount point has been
  // unmounted.
  fuse_.close();

  // Wait for the FuseChannel to signal that it has finished.
  auto stopData = std::move(completeFuture).get(kTimeout);
  auto* fuseStopData = dynamic_cast<FuseChannel::StopData*>(stopData.get());
  EXPECT_EQ(fuseStopData->reason, FuseChannel::StopReason::UNMOUNTED);
  EXPECT_FALSE(fuseStopData->fuseDevice);
}

TEST_F(FuseChannelTest, testTakeoverStop) {
  const uint32_t minorVersion = Random::rand32();
  const uint32_t maxReadahead = Random::rand32();
  constexpr uint32_t flags = FUSE_ASYNC_READ;
  auto channel = createChannel();
  auto completeFuture = performInit(
      channel.get(), FUSE_KERNEL_VERSION, minorVersion, maxReadahead, flags);

  channel->takeoverStop();

  // Wait for the FuseChannel to signal that it has finished.
  auto stopData = std::move(completeFuture).get(kTimeout);
  auto* fuseStopData = dynamic_cast<FuseChannel::StopData*>(stopData.get());
  EXPECT_EQ(fuseStopData->reason, FuseChannel::StopReason::TAKEOVER);
  // We should have received the FUSE device and valid settings information
  EXPECT_TRUE(fuseStopData->fuseDevice);
  EXPECT_EQ(FUSE_KERNEL_VERSION, fuseStopData->fuseSettings.major);
  EXPECT_EQ(minorVersion, fuseStopData->fuseSettings.minor);
  EXPECT_EQ(maxReadahead, fuseStopData->fuseSettings.max_readahead);
  EXPECT_EQ(flags, fuseStopData->fuseSettings.flags);
}

TEST_F(FuseChannelTest, unmountAfterTakeoverStop) {
  auto channel = createChannel();
  auto completeFuture = performInit(channel.get());

  auto lookupId = fuse_.sendLookup(FUSE_ROOT_ID, "pending");
  auto lookup = dispatcher_->waitForLookup(lookupId, kTimeout);

  channel->takeoverStop();
  channel->logUnmountEventAndExit();

  lookup.promise.setValue(genRandomLookupResponse(9));
  auto response = fuse_.recvResponse();
  EXPECT_EQ(lookupId, response.header.unique);

  auto stopData = std::move(completeFuture).get(kTimeout);
  auto* fuseStopData = dynamic_cast<FuseChannel::StopData*>(stopData.get());
  ASSERT_NE(nullptr, fuseStopData);
  EXPECT_EQ(FuseChannel::StopReason::UNMOUNTED, fuseStopData->reason);
  EXPECT_FALSE(fuseStopData->fuseDevice);
  EXPECT_TRUE(fuseStopData->isUnmounted());
}

TEST_F(FuseChannelTest, stopTimeIsWhenTheUnmountArrived) {
  auto channel = createChannel();
  auto completeFuture = performInit(channel.get());
  auto lookupId = fuse_.sendLookup(FUSE_ROOT_ID, "pending");
  auto lookup = dispatcher_->waitForLookup(lookupId, kTimeout);

  const auto beforeUnmount = std::chrono::steady_clock::now();
  channel->logUnmountEventAndExit();
  const auto afterUnmount = std::chrono::steady_clock::now();
  // The channel stops only once the pending lookup is answered.
  lookup.promise.setValue(genRandomLookupResponse(9));
  EXPECT_EQ(lookupId, fuse_.recvResponse().header.unique);

  auto stopData = std::move(completeFuture).get(kTimeout);
  ASSERT_TRUE(stopData->stopRequestedAt.has_value());
  EXPECT_GE(*stopData->stopRequestedAt, beforeUnmount);
  EXPECT_LE(*stopData->stopRequestedAt, afterUnmount);
  EXPECT_EQ("fuse_unmounted", stopData->getStopReason().name);
  EXPECT_TRUE(stopData->getStopReason().unexpected);
}

TEST_F(FuseChannelTest, testTakeoverBufferSizeFollowsNegotiatedMaxWrite) {
  // fuse:max-pages is 0, so a fresh mount would get the minimum buffer, but
  // the inherited connection lets the kernel send 1 MB writes.
  auto channel = createChannel(/*numThreads=*/2, /*fuseMaxPages=*/0);
  fuse_init_out connInfo = {};
  connInfo.major = FUSE_KERNEL_VERSION;
  connInfo.minor = FUSE_KERNEL_MINOR_VERSION;
  connInfo.max_write = 256 * 4096;

  auto completeFuture = channel->initializeFromTakeover(connInfo);

  EXPECT_EQ(connInfo.max_write + 4096, channel->getTransportBufferSize());

  channel->takeoverStop();
  auto stopData = std::move(completeFuture).get(kTimeout);
  auto* fuseStopData = dynamic_cast<FuseChannel::StopData*>(stopData.get());
  ASSERT_NE(nullptr, fuseStopData);
  EXPECT_EQ(FuseChannel::StopReason::TAKEOVER, fuseStopData->reason);
}

#if EDEN_HAVE_FUSE_IO_URING
std::string getRunningKernelReleaseForTest() {
  struct utsname uts = {};
  if (uname(&uts) != 0) {
    return {};
  }
  return uts.release;
}

TEST_F(FuseChannelTest, testInitNegotiatesIoUringOnlyOnAllowedKernel) {
  auto channel = createChannel(
      /*numThreads=*/2,
      /*fuseMaxPages=*/0,
      /*useIoUring=*/true,
      /*ioUringKernelReleaseRegex=*/"^" + getRunningKernelReleaseForTest());
  if (folly::StringPiece{channel->getDesiredTransportName()} !=
      kIoUringFuseTransportName) {
    GTEST_SKIP()
        << "FUSE io_uring transport is not available in this test environment";
  }

  auto completeFuture = performInit(
      channel.get(),
      FUSE_KERNEL_VERSION,
      FUSE_KERNEL_MINOR_VERSION,
      0,
      FUSE_INIT_EXT,
      static_cast<uint32_t>(FUSE_OVER_IO_URING >> 32));

  channel->takeoverStop();
  auto stopData = std::move(completeFuture).get(kTimeout);
  auto* fuseStopData = dynamic_cast<FuseChannel::StopData*>(stopData.get());
  ASSERT_NE(nullptr, fuseStopData);
  EXPECT_TRUE(fuseStopData->fuseSettings.flags & FUSE_INIT_EXT);
  EXPECT_TRUE(
      fuseStopData->fuseSettings.flags2 &
      static_cast<uint32_t>(FUSE_OVER_IO_URING >> 32));
}

TEST_F(FuseChannelTest, transportNameDuringInitialization) {
  auto channel = createChannel(
      /*numThreads=*/2,
      /*fuseMaxPages=*/0,
      /*useIoUring=*/true,
      /*ioUringKernelReleaseRegex=*/"^" + getRunningKernelReleaseForTest());
  if (folly::StringPiece{channel->getDesiredTransportName()} !=
      kIoUringFuseTransportName) {
    GTEST_SKIP()
        << "FUSE io_uring transport is not available in this test environment";
  }

  std::atomic<bool> keepPolling{true};
  std::atomic<const char*> observedName{nullptr};
  std::thread statusThread{[&] {
    // Do not synchronize initialization with this reader: listMounts() can
    // query the name without waiting for the FUSE INIT handshake to finish.
    while (keepPolling.load(std::memory_order_relaxed)) {
      observedName.store(
          channel->getTransportName(), std::memory_order_release);
    }
  }};
  SCOPE_EXIT {
    keepPolling.store(false, std::memory_order_relaxed);
    statusThread.join();
  };
  const auto waitForName = [&](const char* expected) {
    const auto deadline = std::chrono::steady_clock::now() + kTimeout;
    while (std::chrono::steady_clock::now() < deadline) {
      const auto* name = observedName.load(std::memory_order_acquire);
      if (name && folly::StringPiece{name} == expected) {
        return true;
      }
      std::this_thread::yield();
    }
    return false;
  };
  ASSERT_TRUE(waitForName(kDevFuseTransportName));

  performInit(
      channel.get(),
      FUSE_KERNEL_VERSION,
      FUSE_KERNEL_MINOR_VERSION,
      0,
      FUSE_INIT_EXT,
      static_cast<uint32_t>(FUSE_OVER_IO_URING >> 32));

  ASSERT_TRUE(waitForName(kIoUringFuseTransportName));
}

TEST_F(FuseChannelTest, testInitDoesNotNegotiateIoUringOnDisallowedKernel) {
  auto channel = createChannel(
      /*numThreads=*/2,
      /*fuseMaxPages=*/0,
      /*useIoUring=*/true,
      /*ioUringKernelReleaseRegex=*/
      "^not-" + getRunningKernelReleaseForTest());
  auto completeFuture = performInit(
      channel.get(),
      FUSE_KERNEL_VERSION,
      FUSE_KERNEL_MINOR_VERSION,
      0,
      FUSE_INIT_EXT,
      static_cast<uint32_t>(FUSE_OVER_IO_URING >> 32));

  channel->takeoverStop();
  auto stopData = std::move(completeFuture).get(kTimeout);
  auto* fuseStopData = dynamic_cast<FuseChannel::StopData*>(stopData.get());
  ASSERT_NE(nullptr, fuseStopData);
  EXPECT_FALSE(
      fuseStopData->fuseSettings.flags2 &
      static_cast<uint32_t>(FUSE_OVER_IO_URING >> 32));
}

// Lowers RLIMIT_MEMLOCK for the duration of a test and restores it after.
// Only the soft limit is touched, so this never needs privileges to undo.
class MemlockSoftLimitGuard {
 public:
  explicit MemlockSoftLimitGuard(rlim_t softLimit) {
    if (getrlimit(RLIMIT_MEMLOCK, &original_) != 0) {
      return;
    }
    rlimit lowered = original_;
    lowered.rlim_cur = softLimit;
    lowered_ = setrlimit(RLIMIT_MEMLOCK, &lowered) == 0;
  }

  ~MemlockSoftLimitGuard() {
    if (lowered_) {
      setrlimit(RLIMIT_MEMLOCK, &original_);
    }
  }

  bool lowered() const {
    return lowered_;
  }

 private:
  rlimit original_ = {};
  bool lowered_{false};
};

// The failure this whole path exists for: a single ring fits, so io_uring
// looks available, but the full set of queues does not. Eden must notice
// before it answers FUSE_INIT and mount over /dev/fuse instead -- once the
// reply advertises io_uring there is no way back.
TEST_F(FuseChannelTest, testInitFallsBackToDevFuseWhenQueuesDoNotFit) {
  auto channel = createChannel(
      /*numThreads=*/2,
      /*fuseMaxPages=*/0,
      /*useIoUring=*/true,
      /*ioUringKernelReleaseRegex=*/"^" + getRunningKernelReleaseForTest(),
      /*ioUringPreCreateQueues=*/true);
  // Runs the single-ring probe and memoizes it as available, exactly as it
  // would be on a host with enough headroom for one ring at mount time.
  if (folly::StringPiece{channel->getDesiredTransportName()} !=
      kIoUringFuseTransportName) {
    GTEST_SKIP()
        << "FUSE io_uring transport is not available in this test environment";
  }

  MemlockSoftLimitGuard memlockGuard{0};
  if (!memlockGuard.lowered()) {
    GTEST_SKIP() << "could not lower RLIMIT_MEMLOCK";
  }
  // CAP_IPC_LOCK bypasses RLIMIT_MEMLOCK entirely, so confirm the limit
  // actually bites here before relying on it to force the failure.
  folly::File devNull{"/dev/null", O_RDWR};
  if (!IoUringFuseTransport::getMaybeSetupError(8, devNull.fd())) {
    GTEST_SKIP() << "RLIMIT_MEMLOCK is not enforced in this environment";
  }

  auto completeFuture = performInit(
      channel.get(),
      FUSE_KERNEL_VERSION,
      FUSE_KERNEL_MINOR_VERSION,
      0,
      FUSE_INIT_EXT,
      static_cast<uint32_t>(FUSE_OVER_IO_URING >> 32));

  EXPECT_FALSE(channel->usesIoUringTransport());
  EXPECT_STREQ(kDevFuseTransportName, channel->getTransportName());

  channel->takeoverStop();
  auto stopData = std::move(completeFuture).get(kTimeout);
  auto* fuseStopData = dynamic_cast<FuseChannel::StopData*>(stopData.get());
  ASSERT_NE(nullptr, fuseStopData);
  // The kernel must not have been told io_uring was in play.
  EXPECT_FALSE(
      fuseStopData->fuseSettings.flags2 &
      static_cast<uint32_t>(FUSE_OVER_IO_URING >> 32));
}

// The other half: when the queues do fit, pre-creating them must not change
// what gets negotiated.
TEST_F(FuseChannelTest, testInitNegotiatesIoUringWhenQueuesArePreCreated) {
  auto channel = createChannel(
      /*numThreads=*/2,
      /*fuseMaxPages=*/0,
      /*useIoUring=*/true,
      /*ioUringKernelReleaseRegex=*/"^" + getRunningKernelReleaseForTest(),
      /*ioUringPreCreateQueues=*/true);
  if (folly::StringPiece{channel->getDesiredTransportName()} !=
      kIoUringFuseTransportName) {
    GTEST_SKIP()
        << "FUSE io_uring transport is not available in this test environment";
  }

  // One queue per logical CPU is about to be created for real, so skip rather
  // than fail where the environment cannot house them.
  rlimit memlock = {};
  const auto queueCount = static_cast<rlim_t>(std::max(get_nprocs_conf(), 1));
  if (getrlimit(RLIMIT_MEMLOCK, &memlock) == 0 &&
      memlock.rlim_cur != RLIM_INFINITY &&
      memlock.rlim_cur < queueCount * 64 * 1024) {
    GTEST_SKIP() << "RLIMIT_MEMLOCK is too low to pre-create " << queueCount
                 << " io_uring queues";
  }

  auto completeFuture = performInit(
      channel.get(),
      FUSE_KERNEL_VERSION,
      FUSE_KERNEL_MINOR_VERSION,
      0,
      FUSE_INIT_EXT,
      static_cast<uint32_t>(FUSE_OVER_IO_URING >> 32));

  EXPECT_TRUE(channel->usesIoUringTransport());
  EXPECT_STREQ(kIoUringFuseTransportName, channel->getTransportName());

  channel->takeoverStop();
  auto stopData = std::move(completeFuture).get(kTimeout);
  auto* fuseStopData = dynamic_cast<FuseChannel::StopData*>(stopData.get());
  ASSERT_NE(nullptr, fuseStopData);
  EXPECT_TRUE(
      fuseStopData->fuseSettings.flags2 &
      static_cast<uint32_t>(FUSE_OVER_IO_URING >> 32));
}

TEST_F(FuseChannelTest, testTakeoverKeepsDevFuseWithoutNegotiatedIoUring) {
  auto channel = createChannel(/*numThreads=*/2, /*fuseMaxPages=*/0, true);
  fuse_init_out connInfo = {};
  connInfo.major = FUSE_KERNEL_VERSION;
  connInfo.minor = FUSE_KERNEL_MINOR_VERSION;

  auto completeFuture = channel->initializeFromTakeover(connInfo);

  EXPECT_FALSE(channel->usesIoUringTransport());
  EXPECT_STREQ(kDevFuseTransportName, channel->getTransportName());

  channel->takeoverStop();
  auto stopData = std::move(completeFuture).get(kTimeout);
  auto* fuseStopData = dynamic_cast<FuseChannel::StopData*>(stopData.get());
  ASSERT_NE(nullptr, fuseStopData);
  EXPECT_EQ(FuseChannel::StopReason::TAKEOVER, fuseStopData->reason);
}

TEST_F(FuseChannelTest, testTakeoverRestoresNegotiatedIoUringTransport) {
  auto channel = createChannel(/*numThreads=*/2, /*fuseMaxPages=*/0, false);
  fuse_init_out connInfo = {};
  connInfo.major = FUSE_KERNEL_VERSION;
  connInfo.minor = FUSE_KERNEL_MINOR_VERSION;
  connInfo.flags = FUSE_INIT_EXT;
  connInfo.flags2 = static_cast<uint32_t>(FUSE_OVER_IO_URING >> 32);

  auto completeFuture = channel->initializeFromTakeover(connInfo);

  EXPECT_TRUE(channel->usesIoUringTransport());
  EXPECT_STREQ(kIoUringFuseTransportName, channel->getTransportName());

  channel->takeoverStop();
  auto stopData = std::move(completeFuture).get(kTimeout);
  auto* fuseStopData = dynamic_cast<FuseChannel::StopData*>(stopData.get());
  ASSERT_NE(nullptr, fuseStopData);
  EXPECT_EQ(FuseChannel::StopReason::TAKEOVER, fuseStopData->reason);
}
#endif

TEST_F(FuseChannelTest, testInitUnmountRace) {
  auto channel = createChannel();
  auto completeFuture = performInit(channel.get());

  // Close the FakeFuse so that FuseChannel will think the mount point has been
  // unmounted.  We then immediately destroy the FuseChannel without waiting
  // for the session complete future, so that destruction and unmounting race.
  fuse_.close();
  channel.reset();

  // Wait for the session complete future now.
  auto stopData = std::move(completeFuture).get(kTimeout);
  auto* fuseStopData = dynamic_cast<FuseChannel::StopData*>(stopData.get());
  if (fuseStopData->reason == FuseChannel::StopReason::UNMOUNTED) {
    EXPECT_FALSE(fuseStopData->fuseDevice);
  } else if (fuseStopData->reason == FuseChannel::StopReason::DESTRUCTOR) {
    EXPECT_TRUE(fuseStopData->fuseDevice);
  } else {
    FAIL() << "unexpected FuseChannel stop reason: "
           << enumValue(fuseStopData->reason);
  }
}

#if EDEN_HAVE_FUSE_IO_URING
TEST_F(FuseChannelTest, ioUringSubmitAndWaitErrorPolicy) {
  EXPECT_TRUE(IoUringFuseTransport::isTransientSubmitAndWaitError(-EINTR));
  EXPECT_TRUE(IoUringFuseTransport::isTransientSubmitAndWaitError(-EAGAIN));
  EXPECT_FALSE(IoUringFuseTransport::isTransientSubmitAndWaitError(-EIO));

  EXPECT_TRUE(
      IoUringFuseTransport::shouldRetrySubmitAndWaitError(-EINTR, false));
  EXPECT_TRUE(
      IoUringFuseTransport::shouldRetrySubmitAndWaitError(-EAGAIN, false));
  EXPECT_FALSE(
      IoUringFuseTransport::shouldRetrySubmitAndWaitError(-EINTR, true));

  EXPECT_TRUE(
      IoUringFuseTransport::shouldIgnoreSubmitAndWaitError(-EINTR, true));
  EXPECT_TRUE(
      IoUringFuseTransport::shouldIgnoreSubmitAndWaitError(-EAGAIN, true));
  EXPECT_FALSE(
      IoUringFuseTransport::shouldIgnoreSubmitAndWaitError(-EINTR, false));
}

TEST_F(FuseChannelTest, ioUringCqeErrorPolicy) {
  EXPECT_TRUE(IoUringFuseTransport::shouldIgnoreCqeError(-EINTR));
  EXPECT_TRUE(IoUringFuseTransport::shouldIgnoreCqeError(-EOPNOTSUPP));
  EXPECT_TRUE(IoUringFuseTransport::shouldIgnoreCqeError(-EAGAIN));
  EXPECT_FALSE(IoUringFuseTransport::shouldIgnoreCqeError(-ENOTCONN));
  EXPECT_FALSE(IoUringFuseTransport::shouldIgnoreCqeError(-ECANCELED));
  EXPECT_FALSE(IoUringFuseTransport::shouldIgnoreCqeError(-EIO));

  EXPECT_TRUE(
      IoUringFuseTransport::shouldIgnoreCqeErrorDuringShutdown(-ECANCELED));
  EXPECT_TRUE(
      IoUringFuseTransport::shouldIgnoreCqeErrorDuringShutdown(-ENOTCONN));
  EXPECT_FALSE(
      IoUringFuseTransport::shouldIgnoreCqeErrorDuringShutdown(-EINTR));

  EXPECT_FALSE(IoUringFuseTransport::shouldIgnoreCqeError(-ECANCELED, false));
  EXPECT_TRUE(IoUringFuseTransport::shouldIgnoreCqeError(-ECANCELED, true));
  EXPECT_FALSE(IoUringFuseTransport::shouldIgnoreCqeError(-ENOTCONN, false));
  EXPECT_TRUE(IoUringFuseTransport::shouldIgnoreCqeError(-ENOTCONN, true));
  EXPECT_TRUE(IoUringFuseTransport::shouldIgnoreCqeError(-EINTR, false));
}

// Any valid registrable fd works for these tests; the ring never issues real
// FUSE commands, so /dev/null stands in for the /dev/fuse device.
TEST_F(FuseChannelTest, ioUringDisableIoWaitAppliesNoIoWait) {
  folly::File devNull{"/dev/null", O_RDWR};
  constexpr uint32_t kQueueDepth = 4;
  if (IoUringFuseTransport::getMaybeSetupError(kQueueDepth, devNull.fd())) {
    GTEST_SKIP() << "io_uring setup unavailable in this environment";
  }

  IoUringFuseTransport transport{kQueueDepth, /*disableIoWait=*/true};
  IoUringFuseTransport::RingQueue queue;
  transport.createQueueRing(queue, devNull.fd());

  if (!(queue.ring.features & IORING_FEAT_NO_IOWAIT)) {
    GTEST_SKIP() << "kernel lacks IORING_FEAT_NO_IOWAIT";
  }
  EXPECT_TRUE(queue.ring.int_flags & IORING_ENTER_NO_IOWAIT);
}

TEST_F(FuseChannelTest, ioUringDefaultDoesNotDisableIoWait) {
  folly::File devNull{"/dev/null", O_RDWR};
  constexpr uint32_t kQueueDepth = 4;
  if (IoUringFuseTransport::getMaybeSetupError(kQueueDepth, devNull.fd())) {
    GTEST_SKIP() << "io_uring setup unavailable in this environment";
  }

  IoUringFuseTransport transport{kQueueDepth};
  IoUringFuseTransport::RingQueue queue;
  transport.createQueueRing(queue, devNull.fd());

  EXPECT_FALSE(queue.ring.int_flags & IORING_ENTER_NO_IOWAIT);
}

TEST_F(FuseChannelTest, ioUringRequestStopWakeupBeforeRingPoolPublished) {
  constexpr uint32_t kQueueDepth = 4;
  IoUringFuseTransport transport{kQueueDepth};
  // requestStopWakeup() can run on the shutdown thread before any worker
  // thread has called initializeSession()/initializeRingPool(), i.e. before
  // ringPool_ is published. It must be a safe no-op in that case rather than
  // racing the plain unique_ptr read.
  EXPECT_NO_THROW(transport.requestStopWakeup());
}

TEST_F(FuseChannelTest, ioUringStopWakeupBeforeEventFdPublished) {
  folly::File devNull{"/dev/null", O_RDWR};
  constexpr uint32_t kQueueDepth = 4;
  if (IoUringFuseTransport::getMaybeSetupError(kQueueDepth, devNull.fd())) {
    GTEST_SKIP() << "io_uring setup unavailable in this environment";
  }

  IoUringFuseTransport transport{kQueueDepth};
  IoUringFuseTransport::RingQueue queue;

  // Simulate requestStopWakeup() racing ahead of createQueueRing(): the
  // queue's eventfd hasn't been published yet, so this must record the
  // pending wakeup instead of silently dropping it.
  transport.requestQueueStopWakeup(queue);

  transport.createQueueRing(queue, devNull.fd());

  // createQueueRing() must self-notify the freshly published eventfd,
  // otherwise the worker could block indefinitely in
  // io_uring_submit_and_wait() waiting for a wakeup that was already
  // requested.
  eventfd_t value = 0;
  ASSERT_EQ(0, eventfd_read(queue.eventFd.load(), &value));
  EXPECT_EQ(1, value);
}
#endif

TEST_F(FuseChannelTest, testInitErrorClose) {
  // Close the FUSE device while the FuseChannel is waiting on the INIT request
  auto channel = createChannel();
  auto initFuture = channel->initialize();
  fuse_.close();

  EXPECT_THROW_RE(
      std::move(initFuture).get(kTimeout),
      FuseDeviceUnmountedDuringInitialization,
      "FUSE mount \"/fake/mount/path\" was unmounted before we "
      "received the INIT packet");
}

TEST_F(FuseChannelTest, testInitErrorWrongPacket) {
  // Send a packet other than FUSE_INIT while the FuseChannel is waiting on the
  // INIT request
  auto channel = createChannel();
  auto initFuture = channel->initialize();

  // Use a fuse_init_in body, but FUSE_LOOKUP as the opcode
  struct fuse_init_in initArg = {};
  fuse_.sendRequest(FUSE_LOOKUP, FUSE_ROOT_ID, initArg);

  EXPECT_THROW_RE(
      std::move(initFuture).get(kTimeout),
      std::runtime_error,
      "expected to receive FUSE_INIT for \"/fake/mount/path\" "
      "but got FUSE_LOOKUP");
}

TEST_F(FuseChannelTest, testInitErrorOldVersion) {
  auto channel = createChannel();
  auto initFuture = channel->initialize();

  // Send 2.7 as the FUSE version, which is too old
  struct fuse_init_in initArg = {};
  initArg.major = 2;
  initArg.minor = 7;
  initArg.max_readahead = 0;
  initArg.flags = 0;
  fuse_.sendRequest(FUSE_INIT, FUSE_ROOT_ID, initArg);

  EXPECT_THROW_RE(
      std::move(initFuture).get(kTimeout),
      std::runtime_error,
      "Unsupported FUSE kernel version 2.7 while initializing "
      "\"/fake/mount/path\"");
}

TEST_F(FuseChannelTest, testInitErrorShortPacket) {
  auto channel = createChannel();
  auto initFuture = channel->initialize();

  // Send a short message
  uint32_t body = 5;
  static_assert(
      sizeof(body) < sizeof(struct fuse_init_in),
      "we intend to send a body shorter than a fuse_init_in struct");
  fuse_.sendRequest(FUSE_INIT, FUSE_ROOT_ID, body);

  EXPECT_THROW_RE(
      std::move(initFuture).get(kTimeout),
      std::runtime_error,
      "received partial FUSE_INIT packet on mount \"/fake/mount/path\": "
      "size=44");
  static_assert(
      sizeof(fuse_in_header) + sizeof(uint32_t) == 44,
      "validate the size in our error message check");
}

TEST_F(FuseChannelTest, testDestroyWithPendingRequests) {
  auto channel = createChannel();
  auto completeFuture = performInit(channel.get());

  // Send several lookup requests
  auto id1 = fuse_.sendLookup(FUSE_ROOT_ID, "foobar");
  auto id2 = fuse_.sendLookup(FUSE_ROOT_ID, "some_file.txt");
  auto id3 = fuse_.sendLookup(FUSE_ROOT_ID, "main.c");

  auto req1 = dispatcher_->waitForLookup(id1);
  auto req2 = dispatcher_->waitForLookup(id2);
  auto req3 = dispatcher_->waitForLookup(id3);

  // Destroy the channel object
  channel.reset();

  // The completion future still should not be ready, since the lookup
  // requests are still pending.
  EXPECT_FALSE(completeFuture.isReady());

  auto checkLookupResponse = [](const FakeFuse::Response& response,
                                uint64_t requestID,
                                fuse_entry_out expected) {
    EXPECT_EQ(requestID, response.header.unique);
    EXPECT_EQ(0, response.header.error);
    EXPECT_EQ(
        sizeof(fuse_out_header) + sizeof(fuse_entry_out), response.header.len);
    EXPECT_EQ(
        ByteRange(
            reinterpret_cast<const uint8_t*>(&expected), sizeof(expected)),
        ByteRange(response.body.data(), response.body.size()));
  };

  // Respond to the lookup requests
  auto response1 = genRandomLookupResponse(9);
  req1.promise.setValue(response1);
  auto received = fuse_.recvResponse();
  checkLookupResponse(received, id1, response1);

  // We don't have to respond in order; respond to request 3 before 2
  auto response3 = genRandomLookupResponse(19);
  req3.promise.setValue(response3);
  received = fuse_.recvResponse();
  checkLookupResponse(received, id3, response3);

  // The completion future still shouldn't be ready since there is still 1
  // request outstanding.
  EXPECT_FALSE(completeFuture.isReady());

  auto response2 = genRandomLookupResponse(12);
  req2.promise.setValue(response2);
  received = fuse_.recvResponse();
  checkLookupResponse(received, id2, response2);

  // The completion future should be ready now that the last request
  // is done.
  EXPECT_TRUE(completeFuture.isReady());
  std::move(completeFuture).get(kTimeout);
}

TEST_F(FuseChannelTest, unexpectedErrnoIsLogged) {
  // EIO is a genuine failure, so it should be logged to edenfs_errors.
  edenConfig_->enableErrorLogging.setValue(true, ConfigSourceType::UserConfig);
  errorLoggerOverride_ = &capturingErrorLogger_;
  auto channel = createChannel();
  performInit(channel.get());

  auto id = fuse_.sendLookup(FUSE_ROOT_ID, "foo");
  auto req = dispatcher_->waitForLookup(id);
  req.promise.setException(
      std::system_error(EIO, std::generic_category(), "injected I/O error"));
  auto received = fuse_.recvResponse();
  EXPECT_EQ(id, received.header.unique);
  EXPECT_NE(0, received.header.error);

  EXPECT_EQ(1, xplatLogger_.events().size());
}

TEST_F(FuseChannelTest, expectedErrnoIsNotLogged) {
  // ENOENT is a normal, expected FUSE response, so it should NOT be logged.
  edenConfig_->enableErrorLogging.setValue(true, ConfigSourceType::UserConfig);
  errorLoggerOverride_ = &capturingErrorLogger_;
  auto channel = createChannel();
  performInit(channel.get());

  auto id = fuse_.sendLookup(FUSE_ROOT_ID, "foo");
  auto req = dispatcher_->waitForLookup(id);
  req.promise.setException(
      std::system_error(ENOENT, std::generic_category(), "no such file"));
  auto received = fuse_.recvResponse();
  EXPECT_EQ(id, received.header.unique);
  EXPECT_NE(0, received.header.error);

  EXPECT_TRUE(xplatLogger_.events().empty());
}

TEST_F(FuseChannelTest, devFuseForgetHasNoReply) {
  auto channel = createChannel(/*numThreads=*/1);
  auto completeFuture = performInit(channel.get());

  expectSingleForgetWithoutReply(*channel);
  EXPECT_FALSE(completeFuture.isReady());
  channel->takeoverStop();
  std::move(completeFuture).get(kTimeout);
}

TEST_F(FuseChannelTest, devFuseBatchForgetHasNoReply) {
  auto channel = createChannel(/*numThreads=*/1);
  auto completeFuture = performInit(channel.get());

  expectBatchForgetWithoutReply(*channel);
  EXPECT_FALSE(completeFuture.isReady());
  channel->takeoverStop();
  std::move(completeFuture).get(kTimeout);
}

#if EDEN_HAVE_FUSE_IO_URING
TEST_F(FuseChannelTest, ioUringWorkerCountIncludesDevFuseReader) {
  IoUringFuseTransport transport{8};
  const auto cpuCount = get_nprocs_conf();
  const size_t queueCount = cpuCount > 0 ? static_cast<size_t>(cpuCount) : 2;
  EXPECT_EQ(queueCount + 1, transport.getWorkerThreadCount(2));
}

TEST_F(FuseChannelTest, ioUringDevFuseForgetHasNoReply) {
  auto channel = createChannel(/*numThreads=*/1);
  IoUringReader reader{*channel, fuse_};
  reader.start();

  expectSingleForgetWithoutReply(*channel);
  ASSERT_TRUE(reader.waitForRequestsToFinish());
  reader.stop();
  reader.join();
}

TEST_F(FuseChannelTest, ioUringDevFuseBatchForgetHasNoReply) {
  auto channel = createChannel(/*numThreads=*/1);
  IoUringReader reader{*channel, fuse_};
  reader.start();

  expectBatchForgetWithoutReply(*channel);
  ASSERT_TRUE(reader.waitForRequestsToFinish());
  reader.stop();
  reader.join();
}

TEST_F(FuseChannelTest, ioUringDevFuseIdleStopRestoresFlags) {
  auto channel = createChannel(/*numThreads=*/1);
  const int originalFlags = fcntl(channel->getFuseDeviceFd(), F_GETFL);
  ASSERT_GE(originalFlags, 0);
  IoUringReader reader{*channel, fuse_};
  reader.start();
  expectForgetsWithoutReply(*channel, {});
  ASSERT_TRUE(reader.waitForRequestsToFinish());
  EXPECT_NE(0, fcntl(channel->getFuseDeviceFd(), F_GETFL) & O_NONBLOCK);

  reader.stop();
  reader.join();

  EXPECT_TRUE(fuse_.isStarted());
  EXPECT_EQ(originalFlags, fcntl(channel->getFuseDeviceFd(), F_GETFL));
}

TEST_F(FuseChannelTest, ioUringDevFuseStopBeforeStart) {
  auto channel = createChannel(/*numThreads=*/1);
  const int originalFlags = fcntl(channel->getFuseDeviceFd(), F_GETFL);
  ASSERT_GE(originalFlags, 0);
  IoUringReader reader{*channel, fuse_};
  reader.stop();

  reader.start();
  reader.join();

  EXPECT_TRUE(channel->isStopRequested());
  EXPECT_TRUE(fuse_.isStarted());
  EXPECT_EQ(originalFlags, fcntl(channel->getFuseDeviceFd(), F_GETFL));
}

TEST_F(FuseChannelTest, ioUringDevFuseEofRestoresFlags) {
  auto channel = createChannel(/*numThreads=*/1);
  const int originalFlags = fcntl(channel->getFuseDeviceFd(), F_GETFL);
  ASSERT_GE(originalFlags, 0);
  IoUringReader reader{*channel, fuse_};
  reader.start();
  expectForgetsWithoutReply(*channel, {});
  ASSERT_TRUE(reader.waitForRequestsToFinish());
  EXPECT_NE(0, fcntl(channel->getFuseDeviceFd(), F_GETFL) & O_NONBLOCK);

  fuse_.close();
  reader.join();

  EXPECT_TRUE(channel->isStopRequested());
  EXPECT_EQ(originalFlags, fcntl(channel->getFuseDeviceFd(), F_GETFL));
}

TEST_F(FuseChannelTest, ioUringDevFuseReplyAfterReaderStops) {
  auto channel = createChannel(/*numThreads=*/1);
  IoUringReader reader{*channel, fuse_};
  reader.start();
  const auto lookupId = fuse_.sendLookup(FUSE_ROOT_ID, "deferred");
  auto lookup = dispatcher_->waitForLookup(lookupId, kTimeout);
  ASSERT_TRUE(channel->hasPendingRequests());

  reader.stop();
  reader.join();
  EXPECT_TRUE(channel->hasPendingRequests());

  const auto expected = genRandomLookupResponse(99);
  lookup.promise.setValue(expected);
  const auto response = fuse_.recvResponse();
  EXPECT_EQ(lookupId, response.header.unique);
  EXPECT_EQ(0, response.header.error);
  EXPECT_EQ(
      ByteRange(reinterpret_cast<const uint8_t*>(&expected), sizeof(expected)),
      ByteRange(response.body.data(), response.body.size()));
  EXPECT_TRUE(reader.waitForRequestsToFinish());
}
#endif

TEST_F(FuseChannelTest, interruptLookups) {
  auto channel = createChannel();
  auto completeFuture = performInit(channel.get());

  // Send a bunch of lookup requests followed immediately by an interrupt
  // request that cancels the corresponding lookup request. We are trying
  // to exercise the codepaths here where handling of the interrupt request
  // may be running concurrently with the launching of the original request
  // on a different thread.

  for (int i = 0; i < 5000; ++i) {
    auto requestId = fuse_.sendLookup(FUSE_ROOT_ID, "foo");

    fuse_interrupt_in interruptData{};
    interruptData.unique = requestId;

    (void)fuse_.sendRequest(FUSE_INTERRUPT, FUSE_ROOT_ID, interruptData);

    // For now FuseChannel never actually interrupts requests, so the
    // dispatcher will definitely receive the request.
    // We may need to change this code in the future if we do add true
    // interrupt support to FuseChannel.
    auto req = dispatcher_->waitForLookup(requestId);

    auto nodeId = 5 + i * 7;
    auto response = genRandomLookupResponse(nodeId);
    req.promise.setValue(response);

    auto received = fuse_.recvResponse();
    EXPECT_EQ(requestId, received.header.unique);
  }
}

TEST_F(FuseChannelTest, formatting_inode) {
  FuseChannel::InvalidationEntry entry(
      InodeNumber(10), PathComponentPiece{"test"});
  EXPECT_EQ("(inode 10, child \"test\")", fmt::to_string(entry));
}

TEST_F(FuseChannelTest, formatting_dir) {
  FuseChannel::InvalidationEntry entry(InodeNumber(20), 10, 100);
  EXPECT_EQ("(inode 20, offset 10, length 100)", fmt::to_string(entry));
}

TEST_F(FuseChannelTest, formatting_unknown) {
  FuseChannel::InvalidationEntry entry(InodeNumber(20), 10, 100);
  const auto originalType = entry.type;
  entry.type = static_cast<FuseChannel::InvalidationType>(11);
  EXPECT_EQ("(unknown invalidation type 11 inode 20)", fmt::to_string(entry));
  // change the type back so the destructor doesn't throw
  entry.type = originalType;
}

#ifdef FUSE_MAX_PAGES
TEST_F(FuseChannelTest, testMaxPagesNegotiation) {
  constexpr uint32_t configuredMaxPages = 128;
  auto channel =
      createChannel(/*numThreads=*/2, /*fuseMaxPages=*/configuredMaxPages);

  // The kernel must advertise FUSE_MAX_PAGES capability for EdenFS to set it
  auto completeFuture = performInit(
      channel.get(),
      FUSE_KERNEL_VERSION,
      FUSE_KERNEL_MINOR_VERSION,
      /*maxReadahead=*/0,
      /*flags=*/FUSE_MAX_PAGES);

  channel->takeoverStop();

  auto stopData = std::move(completeFuture).get(kTimeout);
  auto* fuseStopData = dynamic_cast<FuseChannel::StopData*>(stopData.get());
  EXPECT_EQ(fuseStopData->reason, FuseChannel::StopReason::TAKEOVER);
  EXPECT_TRUE(fuseStopData->fuseDevice);

  // Verify FUSE_MAX_PAGES flag is set in the response
  EXPECT_TRUE(fuseStopData->fuseSettings.flags & FUSE_MAX_PAGES)
      << "FUSE_MAX_PAGES flag should be set when fuseMaxPages is configured";

  // Verify max_pages matches the configured value
  EXPECT_EQ(configuredMaxPages, fuseStopData->fuseSettings.max_pages);
}

TEST_F(FuseChannelTest, testMaxPagesDefaultDisabled) {
  // Default fuseMaxPages=0 should NOT set FUSE_MAX_PAGES flag
  auto channel = createChannel(/*numThreads=*/2, /*fuseMaxPages=*/0);

  // Even if the kernel advertises FUSE_MAX_PAGES, EdenFS should not set it
  // when fuseMaxPages is 0 (use kernel default)
  auto completeFuture = performInit(
      channel.get(),
      FUSE_KERNEL_VERSION,
      FUSE_KERNEL_MINOR_VERSION,
      /*maxReadahead=*/0,
      /*flags=*/FUSE_MAX_PAGES);

  channel->takeoverStop();

  auto stopData = std::move(completeFuture).get(kTimeout);
  auto* fuseStopData = dynamic_cast<FuseChannel::StopData*>(stopData.get());
  EXPECT_EQ(fuseStopData->reason, FuseChannel::StopReason::TAKEOVER);
  EXPECT_TRUE(fuseStopData->fuseDevice);

  // Verify FUSE_MAX_PAGES flag is NOT set in the response
  EXPECT_FALSE(fuseStopData->fuseSettings.flags & FUSE_MAX_PAGES)
      << "FUSE_MAX_PAGES flag should not be set when fuseMaxPages is 0";

  // max_pages should be 0 (kernel will use its default of 32)
  EXPECT_EQ(0, fuseStopData->fuseSettings.max_pages);
}

TEST_F(FuseChannelTest, testMaxPagesClampedAt256) {
  // Values above 256 should be capped at 256 (kernel maximum)
  auto channel = createChannel(/*numThreads=*/2, /*fuseMaxPages=*/512);

  auto completeFuture = performInit(
      channel.get(),
      FUSE_KERNEL_VERSION,
      FUSE_KERNEL_MINOR_VERSION,
      /*maxReadahead=*/0,
      /*flags=*/FUSE_MAX_PAGES);

  channel->takeoverStop();

  auto stopData = std::move(completeFuture).get(kTimeout);
  auto* fuseStopData = dynamic_cast<FuseChannel::StopData*>(stopData.get());
  EXPECT_EQ(fuseStopData->reason, FuseChannel::StopReason::TAKEOVER);
  EXPECT_TRUE(fuseStopData->fuseDevice);

  EXPECT_TRUE(fuseStopData->fuseSettings.flags & FUSE_MAX_PAGES);
  EXPECT_EQ(256, fuseStopData->fuseSettings.max_pages);
}

TEST_F(FuseChannelTest, testMaxPagesKernelDoesNotSupport) {
  // EdenFS wants max_pages but kernel doesn't advertise FUSE_MAX_PAGES
  auto channel = createChannel(/*numThreads=*/2, /*fuseMaxPages=*/128);

  // Send init with flags that do NOT include FUSE_MAX_PAGES
  auto completeFuture = performInit(
      channel.get(),
      FUSE_KERNEL_VERSION,
      FUSE_KERNEL_MINOR_VERSION,
      /*maxReadahead=*/0,
      /*flags=*/0);

  channel->takeoverStop();

  auto stopData = std::move(completeFuture).get(kTimeout);
  auto* fuseStopData = dynamic_cast<FuseChannel::StopData*>(stopData.get());
  EXPECT_EQ(fuseStopData->reason, FuseChannel::StopReason::TAKEOVER);
  EXPECT_TRUE(fuseStopData->fuseDevice);

  // Kernel didn't advertise FUSE_MAX_PAGES, so EdenFS must not set it
  EXPECT_FALSE(fuseStopData->fuseSettings.flags & FUSE_MAX_PAGES)
      << "FUSE_MAX_PAGES should not be set when kernel doesn't support it";
  EXPECT_EQ(0, fuseStopData->fuseSettings.max_pages);
}
#endif

#ifdef FUSE_ALLOW_IDMAP
TEST_F(FuseChannelTest, testAllowIdmapNegotiation) {
  auto channel = createChannel();

  // FUSE_ALLOW_IDMAP is bit 40, which lives in flags2 (bits 32..63 shifted
  // down by 32). FUSE_INIT_EXT must be set in flags for the kernel to
  // inspect flags2.
  auto completeFuture = performInit(
      channel.get(),
      FUSE_KERNEL_VERSION,
      FUSE_KERNEL_MINOR_VERSION,
      /*maxReadahead=*/0,
      /*flags=*/FUSE_INIT_EXT,
      /*flags2=*/static_cast<uint32_t>(FUSE_ALLOW_IDMAP >> 32));

  channel->takeoverStop();

  auto stopData = std::move(completeFuture).get(kTimeout);
  auto* fuseStopData = dynamic_cast<FuseChannel::StopData*>(stopData.get());
  EXPECT_EQ(fuseStopData->reason, FuseChannel::StopReason::TAKEOVER);
  EXPECT_TRUE(fuseStopData->fuseDevice);

  EXPECT_TRUE(
      fuseStopData->fuseSettings.flags2 &
      static_cast<uint32_t>(FUSE_ALLOW_IDMAP >> 32))
      << "FUSE_ALLOW_IDMAP should be set when kernel advertises it";
}

TEST_F(FuseChannelTest, testAllowIdmapNotSetWhenKernelLacksSupport) {
  auto channel = createChannel();

  // Kernel does not advertise FUSE_ALLOW_IDMAP (flags2=0)
  auto completeFuture = performInit(channel.get());

  channel->takeoverStop();

  auto stopData = std::move(completeFuture).get(kTimeout);
  auto* fuseStopData = dynamic_cast<FuseChannel::StopData*>(stopData.get());
  EXPECT_EQ(fuseStopData->reason, FuseChannel::StopReason::TAKEOVER);
  EXPECT_TRUE(fuseStopData->fuseDevice);

  EXPECT_FALSE(
      fuseStopData->fuseSettings.flags2 &
      static_cast<uint32_t>(FUSE_ALLOW_IDMAP >> 32))
      << "FUSE_ALLOW_IDMAP should not be set when kernel doesn't support it";
}
#endif

#ifdef FUSE_HANDLE_KILLPRIV_V2
TEST_F(FuseChannelTest, testHandleKillPrivV2Negotiation) {
  auto channel = createChannel();

  auto completeFuture = performInit(
      channel.get(),
      FUSE_KERNEL_VERSION,
      FUSE_KERNEL_MINOR_VERSION,
      /*maxReadahead=*/0,
      /*flags=*/FUSE_HANDLE_KILLPRIV_V2);

  channel->takeoverStop();

  auto stopData = std::move(completeFuture).get(kTimeout);
  auto* fuseStopData = dynamic_cast<FuseChannel::StopData*>(stopData.get());
  EXPECT_EQ(fuseStopData->reason, FuseChannel::StopReason::TAKEOVER);
  EXPECT_TRUE(fuseStopData->fuseSettings.flags & FUSE_HANDLE_KILLPRIV_V2);
}

TEST_F(FuseChannelTest, testHandleKillPrivV2Disabled) {
  auto channel = createChannel(
      /*numThreads=*/2,
      /*fuseMaxPages=*/0,
      /*useIoUring=*/false,
      /*ioUringKernelReleaseRegex=*/{},
      /*ioUringPreCreateQueues=*/false,
      /*numInvalidationThreads=*/4,
      /*handleKillPrivV2=*/false);

  // The kernel advertises the flag, so only the config keeps it unset.
  auto completeFuture = performInit(
      channel.get(),
      FUSE_KERNEL_VERSION,
      FUSE_KERNEL_MINOR_VERSION,
      /*maxReadahead=*/0,
      /*flags=*/FUSE_HANDLE_KILLPRIV_V2);

  channel->takeoverStop();

  auto stopData = std::move(completeFuture).get(kTimeout);
  auto* fuseStopData = dynamic_cast<FuseChannel::StopData*>(stopData.get());
  EXPECT_EQ(fuseStopData->reason, FuseChannel::StopReason::TAKEOVER);
  EXPECT_FALSE(fuseStopData->fuseSettings.flags & FUSE_HANDLE_KILLPRIV_V2);
}
#endif

TEST_F(FuseChannelTest, zeroInvalidationThreadsUsesOneWorker) {
  auto channelPtr = createChannel(
      /*numThreads=*/2,
      /*fuseMaxPages=*/0,
      /*useIoUring=*/false,
      /*ioUringKernelReleaseRegex=*/{},
      /*ioUringPreCreateQueues=*/false,
      /*numInvalidationThreads=*/0);
  if (!channelPtr) {
    FAIL() << "createChannel returned null";
  }
  auto& channel = *channelPtr;

  EXPECT_EQ(1, channel.numInvalidationThreads_);
}

TEST_F(FuseChannelTest, excessiveInvalidationThreadsAreCapped) {
  auto channel = createChannel(
      /*numThreads=*/2,
      /*fuseMaxPages=*/0,
      /*useIoUring=*/false,
      /*ioUringKernelReleaseRegex=*/{},
      /*ioUringPreCreateQueues=*/false,
      /*numInvalidationThreads=*/1'000);

  EXPECT_EQ(64, channel->numInvalidationThreads_);
}

} // namespace facebook::eden
