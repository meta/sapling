/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/service/EdenServer.h"

#ifndef _WIN32
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>
#endif

#include <cerrno>
#include <chrono>
#include <csignal>
#include <cstring>
#include <functional>
#include <future>
#include <memory>
#include <system_error>
#include <thread>
#include <vector>

#include <folly/CancellationToken.h>
#include <folly/ExceptionWrapper.h>
#include <folly/Executor.h>
#include <folly/ScopeGuard.h>
#include <folly/io/IOBufQueue.h>
#include <folly/io/async/AsyncSocket.h>
#include <folly/io/async/EventBase.h>
#include <folly/test/TestUtils.h>
#include <gflags/gflags.h>
#include <gtest/gtest.h>
#include <thrift/lib/cpp/transport/TTransportException.h>
#include <thrift/lib/cpp2/async/HeaderClientChannel.h>

#include "eden/common/utils/FaultInjector.h"
#include "eden/common/utils/FileUtils.h"
#include "eden/common/utils/UnboundedQueueExecutor.h"
#include "eden/fs/config/CheckoutConfig.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/ServerState.h"
#include "eden/fs/model/RootId.h"
#include "eden/fs/nfs/MountdRpc.h"
#include "eden/fs/nfs/NfsServer.h"
#include "eden/fs/service/EdenServiceHandler.h"
#include "eden/fs/service/EdenStateDir.h"
#include "eden/fs/service/gen-cpp2/EdenServiceAsyncClient.h"
#include "eden/fs/takeover/TakeoverClient.h"
#ifdef __linux__
#include "eden/fs/privhelper/PrivHelper.h"
#include "eden/fs/store/BackingStore.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/testharness/FakeBackingStore.h"
#include "eden/fs/testharness/FakeFuse.h"
#include "eden/fs/testharness/FakePrivHelper.h"
#include "eden/fs/testharness/FakeTreeBuilder.h"
#include "eden/fs/testharness/TestMount.h"
#endif
#include "eden/fs/testharness/TestServer.h"
#include "eden/fs/testharness/TestServerState.h"

using namespace std::chrono_literals;
#ifdef __linux__
using namespace folly::string_piece_literals;
#endif

namespace facebook::eden {

namespace {

#ifndef _WIN32
class ScopedServerThread {
 public:
  explicit ScopedServerThread(EdenServer& server)
      : server_{server},
        exitFuture_{exitPromise_.get_future().share()},
        thread_{[this] {
          try {
            server_.serve();
            exitPromise_.set_value();
          } catch (...) {
            exitPromise_.set_exception(std::current_exception());
          }
        }} {}

  ~ScopedServerThread() {
    server_.stop();
    join();
  }

  bool waitForExit(std::chrono::milliseconds timeout) const {
    return exitFuture_.wait_for(timeout) == std::future_status::ready;
  }

  void join() {
    if (thread_.joinable()) {
      thread_.join();
    }
  }

  void throwIfServeFailed() const {
    exitFuture_.get();
  }

 private:
  EdenServer& server_;
  std::promise<void> exitPromise_;
  std::shared_future<void> exitFuture_;
  std::thread thread_;
};

std::future<TakeoverData> takeoverViaThread(
    AbsolutePathPiece socketPath,
    bool shouldThrowDuringTakeover,
    bool force = false,
    bool onlyIfIdle = false) {
  return std::async(
      std::launch::async,
      [path = AbsolutePath{socketPath},
       shouldThrowDuringTakeover,
       force,
       onlyIfIdle] {
        return takeoverMounts(
            path,
            /*takeoverReceiveTimeout=*/std::chrono::seconds{150},
            force,
            onlyIfIdle,
            shouldThrowDuringTakeover);
      });
}

void driveTakeoverSendFailureToCleanup(
    TestServer& testServer,
    EdenServer& server,
    size_t blockCount = 0) {
  // The old daemon intentionally writes takeover data to a client that has
  // already disconnected. Ignore SIGPIPE so the broken send is reported as a
  // socket exception instead of killing the test process.
  signal(SIGPIPE, SIG_IGN);

  ScopedServerThread serverThread{server};

  // Wait until the thrift server is fully initialized before attempting
  // takeover, otherwise the test races startup instead of the handoff path.
  testServer.waitUntilReady();

  auto& faultInjector = server.getServerState()->getFaultInjector();
  // Pause the old daemon after it sends the readiness ping. This lets the new
  // process reply and then disconnect before the takeover payload is sent.
  faultInjector.injectBlock("takeover", "ping_receive", blockCount);

  auto socketPath = EdenStateDir{server.getEdenDir()}.getTakeoverSocketPath();
  auto clientFuture = takeoverViaThread(
      socketPath,
      /*shouldThrowDuringTakeover=*/true);

  // Drive the server event loop until the takeover path reaches the injected
  // stall, which means mounts have already been stopped for handoff.
  bool takeoverBlocked = false;
  std::thread blockedWaiter([&] {
    takeoverBlocked = faultInjector.waitUntilBlocked("takeover", 5s);
    server.getMainEventBase()->runInEventBaseThread(
        [evb = server.getMainEventBase()] { evb->terminateLoopSoon(); });
  });
  server.getMainEventBase()->loop();
  blockedWaiter.join();

  ASSERT_TRUE(takeoverBlocked);

  // The new process now exits before takeover data transfer begins.
  bool clientSawException = false;
  try {
    (void)clientFuture.get();
  } catch (const std::exception&) {
    clientSawException = true;
  }
  ASSERT_TRUE(clientSawException);

  // Let the old daemon continue into the send path, then wait for the serve
  // loop to exit before calling performCleanup().
  faultInjector.removeFault("takeover", "ping_receive");
  faultInjector.unblock("takeover", "ping_receive");

  ASSERT_TRUE(serverThread.waitForExit(5s));
  serverThread.join();
  ASSERT_NO_THROW(serverThread.throwIfServeFailed());
}

std::unique_ptr<folly::IOBuf> buildMountdNullRpcRequest(uint32_t xid) {
  folly::IOBufQueue queue{folly::IOBufQueue::cacheChainLength()};
  folly::io::QueueAppender ser(&queue, 256);

  XdrTrait<uint32_t>::serialize(ser, 0); // fragment header placeholder
  rpc_msg_call call{
      xid,
      msg_type::CALL,
      call_body{
          kRPCVersion,
          kMountdProgNumber,
          kMountdProgVersion,
          static_cast<uint32_t>(mountProcs::null),
          opaque_auth{auth_flavor::AUTH_NONE, {}},
          opaque_auth{auth_flavor::AUTH_NONE, {}},
      },
  };
  XdrTrait<rpc_msg_call>::serialize(ser, call);

  auto len = static_cast<uint32_t>(queue.chainLength() - sizeof(uint32_t));
  auto buf = queue.move();
  const uint32_t fragmentHeader = htonl(len | 0x80000000);
  std::memcpy(buf->writableData(), &fragmentHeader, sizeof(fragmentHeader));
  return buf;
}

int connectSocket(const folly::SocketAddress& addr) {
  sockaddr_storage socketAddress{};
  auto len = addr.getAddress(&socketAddress);

  int fd = socket(addr.getFamily(), SOCK_STREAM, 0);
  if (fd == -1) {
    throw std::system_error(
        errno,
        std::generic_category(),
        "failed to create mountd client socket");
  }

  if (connect(fd, reinterpret_cast<const sockaddr*>(&socketAddress), len) !=
      0) {
    auto savedErrno = errno;
    close(fd);
    throw std::system_error(
        savedErrno, std::generic_category(), "failed to connect to mountd");
  }

  return fd;
}

bool pollForReply(int clientFd, int timeoutMs) {
  struct pollfd pfd{};
  pfd.fd = clientFd;
  pfd.events = POLLIN;
  return poll(&pfd, 1, timeoutMs) > 0 && (pfd.revents & POLLIN);
}

template <typename Predicate>
bool driveMainEventBaseUntil(
    EdenServer& server,
    Predicate&& predicate,
    std::chrono::milliseconds timeout = std::chrono::seconds{5}) {
  auto* evb = server.getMainEventBase();
  struct PollState : std::enable_shared_from_this<PollState> {
    PollState(
        folly::EventBase* evb,
        std::function<bool()> predicate,
        std::chrono::steady_clock::time_point deadline,
        std::chrono::milliseconds pollInterval)
        : evb{evb},
          predicate{std::move(predicate)},
          deadline{deadline},
          pollInterval{pollInterval} {}

    void check() {
      if (finished) {
        return;
      }
      if (predicate()) {
        reachedCondition = true;
        evb->terminateLoopSoon();
        return;
      }
      if (std::chrono::steady_clock::now() >= deadline) {
        evb->terminateLoopSoon();
        return;
      }
      auto state = this->shared_from_this();
      evb->runAfterDelay(
          [state] { state->check(); },
          static_cast<uint32_t>(pollInterval.count()));
    }

    folly::EventBase* evb;
    std::function<bool()> predicate;
    std::chrono::steady_clock::time_point deadline;
    std::chrono::milliseconds pollInterval;
    bool finished{false};
    bool reachedCondition{false};
  };

  constexpr auto kPollInterval = 10ms;
  auto state = std::make_shared<PollState>(
      evb,
      std::forward<Predicate>(predicate),
      std::chrono::steady_clock::now() + timeout,
      kPollInterval);
  evb->runInEventBaseThread([state] { state->check(); });
  evb->loop();
  state->finished = true;
  return state->reachedCondition;
}
#endif

} // namespace

class EdenServerTest : public ::testing::Test {
 protected:
  EdenServerTest() {
    GFLAGS_NAMESPACE::SetCommandLineOptionWithMode(
        "enable_fault_injection", "true", GFLAGS_NAMESPACE::SET_FLAGS_VALUE);
    testServer_ = std::make_unique<TestServer>();
  }

  TestServer& testServer() {
    return *testServer_;
  }

 private:
  std::unique_ptr<TestServer> testServer_;
};

TEST_F(EdenServerTest, StopCancelsAllActiveRequests) {
  auto& server = testServer().getServer();
  auto handler = server.getHandler();

  // Simulate active Thrift request
  folly::CancellationSource source;
  auto token = source.getToken();

  bool requestCancelled = false;
  folly::CancellationCallback callback(
      token, [&requestCancelled] { requestCancelled = true; });

  uint64_t testRequestId = 12345;
  handler->insertCancellationSource(
      testRequestId, std::move(source), "test_endpoint");

  EXPECT_FALSE(requestCancelled);
  EXPECT_EQ(handler->getActiveCancellationSourceCount(), 1);

  // Trigger server stop & cancel all active requests
  server.stop();

  // Active requests are now cancelled
  EXPECT_TRUE(requestCancelled);
  EXPECT_TRUE(token.isCancellationRequested());
}

TEST_F(EdenServerTest, StopIsIdempotent) {
  auto& server = testServer().getServer();

  // Multiple calls to stop should not crash
  server.stop();
  server.stop();
  server.stop();

  // Test passes if no crash occurs
  EXPECT_TRUE(true);
}

TEST(EdenServer, serverStateShutdownWaitsForFsChannelExecutor) {
  const auto fsChannelExecutor =
      std::make_shared<UnboundedQueueExecutor>(1, "TestFsChannel");
  auto serverState = createTestServerState(fsChannelExecutor);
  ASSERT_NE(serverState->getThreadPool().get(), fsChannelExecutor.get());

  std::promise<void> taskStartedPromise;
  auto taskStarted = taskStartedPromise.get_future();

  std::promise<void> releaseTaskPromise;
  auto releaseTask = releaseTaskPromise.get_future().share();

  fsChannelExecutor->add([taskStartedPromise = std::move(taskStartedPromise),
                          releaseTask]() mutable {
    taskStartedPromise.set_value();
    releaseTask.wait();
  });
  ASSERT_EQ(taskStarted.wait_for(5s), std::future_status::ready);

  std::promise<void> shutdownFinishedPromise;
  auto shutdownFinished = shutdownFinishedPromise.get_future();
  std::thread shutdownThread([&] {
    serverState->shutdown();
    shutdownFinishedPromise.set_value();
  });
  bool taskReleased = false;
  auto cleanup = folly::makeGuard([&] {
    if (!taskReleased) {
      releaseTaskPromise.set_value();
      taskReleased = true;
    }
    if (shutdownThread.joinable()) {
      shutdownThread.join();
    }
  });

  EXPECT_EQ(shutdownFinished.wait_for(250ms), std::future_status::timeout);

  releaseTaskPromise.set_value();
  taskReleased = true;

  EXPECT_EQ(shutdownFinished.wait_for(5s), std::future_status::ready);
}

#ifndef _WIN32
TEST_F(EdenServerTest, TakeoverSendFailureRecoversDuringCleanup) {
  auto& server = testServer().getServer();
  auto originalHandler = server.getHandler();
  ASSERT_NO_FATAL_FAILURE(
      driveTakeoverSendFailureToCleanup(testServer(), server));
  ASSERT_FALSE(server.performCleanup());
  ScopedServerThread serverThread{server};
  ASSERT_TRUE(driveMainEventBaseUntil(server, [&] {
    return server.getStatus() == EdenServer::RunState::RUNNING;
  }));
  EXPECT_NE(originalHandler, server.getHandler());
}

TEST_F(EdenServerTest, TakeoverSendFailureRecoveryReinitializesMountd) {
  TestServer::Options nfsOptions;
  nfsOptions.enableNfsServer = true;
  TestServer nfsTestServer{nfsOptions};
  auto& server = nfsTestServer.getServer();

  ASSERT_NO_FATAL_FAILURE(
      driveTakeoverSendFailureToCleanup(nfsTestServer, server));
  ASSERT_FALSE(server.performCleanup());

  ScopedServerThread serverThread{server};
  ASSERT_TRUE(driveMainEventBaseUntil(server, [&] {
    return server.getStatus() == EdenServer::RunState::RUNNING;
  }));

  std::thread recoveryEventBaseThread(
      [&server] { server.getMainEventBase()->loop(); });
  auto stopRecoveryLoop = folly::makeGuard([&] {
    server.getMainEventBase()->runInEventBaseThread(
        [&server] { server.getMainEventBase()->terminateLoopSoon(); });
    if (recoveryEventBaseThread.joinable()) {
      recoveryEventBaseThread.join();
    }
  });

  auto nfsServer = server.getServerState()->getNfsServer();
  ASSERT_TRUE(nfsServer);
  auto request = buildMountdNullRpcRequest(42);
  const auto requestBytes = request->coalesce();
  const int mountdFd = connectSocket(nfsServer->getMountdAddr());
  auto closeMountdFd = folly::makeGuard([mountdFd] { close(mountdFd); });

  ASSERT_EQ(
      static_cast<ssize_t>(requestBytes.size()),
      write(mountdFd, requestBytes.data(), requestBytes.size()));
  const auto hasReply = pollForReply(mountdFd, 1000);
  EXPECT_TRUE(hasReply)
      << "mountd should accept null RPCs after takeover recovery";

  if (hasReply) {
    uint8_t reply[256];
    EXPECT_GT(read(mountdFd, reply, sizeof(reply)), 0);
  }
}

TEST(EdenServerTakeoverTest, RefusedWhileALongRequestRunsUnlessForced) {
  GFLAGS_NAMESPACE::SetCommandLineOptionWithMode(
      "enable_fault_injection", "true", GFLAGS_NAMESPACE::SET_FLAGS_VALUE);
  TestServer::Options options;
  // No grace: any request in flight has already outlived it.
  options.takeoverRequestGrace = std::chrono::seconds{0};
  TestServer testServer{options};
  auto& server = testServer.getServer();
  auto handler = server.getHandler();
  auto socketPath = EdenStateDir{server.getEdenDir()}.getTakeoverSocketPath();

  ScopedServerThread serverThread{server};
  ASSERT_TRUE(driveMainEventBaseUntil(server, [&] {
    return server.getStatus() == EdenServer::RunState::RUNNING;
  }));

  // Simulate a Thrift request that has been running for a while.
  folly::CancellationSource source;
  auto token = source.getToken();
  handler->insertCancellationSource(
      /*requestId=*/4242, std::move(source), "globFiles");

  {
    auto clientFuture = takeoverViaThread(
        socketPath, /*shouldThrowDuringTakeover=*/false, /*force=*/false);
    ASSERT_TRUE(driveMainEventBaseUntil(server, [&] {
      return clientFuture.wait_for(0ms) == std::future_status::ready;
    }));
    EXPECT_THROW_RE(
        (void)clientFuture.get(), std::runtime_error, "refused.*globFiles");
    EXPECT_FALSE(token.isCancellationRequested());
    EXPECT_EQ(server.getStatus(), EdenServer::RunState::RUNNING);
  }

  {
    auto clientFuture = takeoverViaThread(
        socketPath, /*shouldThrowDuringTakeover=*/false, /*force=*/true);
    ASSERT_TRUE(driveMainEventBaseUntil(server, [&] {
      return clientFuture.wait_for(0ms) == std::future_status::ready;
    }));
    EXPECT_NO_THROW((void)clientFuture.get());
    EXPECT_TRUE(token.isCancellationRequested());
  }

  ASSERT_TRUE(serverThread.waitForExit(5s));
  serverThread.join();
  ASSERT_NO_THROW(serverThread.throwIfServeFailed());
  EXPECT_TRUE(server.performCleanup());
}

TEST(EdenServerTakeoverTest, RefusedAfterRecentActivityOnlyWhenAsked) {
  GFLAGS_NAMESPACE::SetCommandLineOptionWithMode(
      "enable_fault_injection", "true", GFLAGS_NAMESPACE::SET_FLAGS_VALUE);
  TestServer::Options options;
  options.takeoverRecentActivityWindow = std::chrono::minutes{10};
  TestServer testServer{options};
  auto& server = testServer.getServer();
  auto socketPath = EdenStateDir{server.getEdenDir()}.getTakeoverSocketPath();

  ScopedServerThread serverThread{server};
  ASSERT_TRUE(driveMainEventBaseUntil(server, [&] {
    return server.getStatus() == EdenServer::RunState::RUNNING;
  }));

  // A checkout just ran; nothing is in flight.
  server.noteActivity("checkOutRevision");

  {
    // A restart that asks for an idle daemon, as the restarter does, is
    // refused and names the operation.
    auto clientFuture = takeoverViaThread(
        socketPath,
        /*shouldThrowDuringTakeover=*/false,
        /*force=*/false,
        /*onlyIfIdle=*/true);
    ASSERT_TRUE(driveMainEventBaseUntil(server, [&] {
      return clientFuture.wait_for(0ms) == std::future_status::ready;
    }));
    EXPECT_THROW_RE(
        (void)clientFuture.get(),
        std::runtime_error,
        "in use recently.*checkOutRevision");
    EXPECT_EQ(server.getStatus(), EdenServer::RunState::RUNNING);
  }

  {
    // A plain graceful restart, as a person runs it, only cares about the
    // requests in flight and proceeds.
    auto clientFuture = takeoverViaThread(
        socketPath, /*shouldThrowDuringTakeover=*/false, /*force=*/false);
    ASSERT_TRUE(driveMainEventBaseUntil(server, [&] {
      return clientFuture.wait_for(0ms) == std::future_status::ready;
    }));
    EXPECT_NO_THROW((void)clientFuture.get());
  }

  ASSERT_TRUE(serverThread.waitForExit(5s));
  serverThread.join();
  ASSERT_NO_THROW(serverThread.throwIfServeFailed());
  EXPECT_TRUE(server.performCleanup());
}

namespace {

// Starts a takeover with one request in flight, and once the takeover has
// looked at the requests in flight, finishes that request and starts another
// one, which counts as arriving during the grace period. With `laterFinishes`
// the later request finishes once the takeover has polled again; otherwise it
// keeps running.
struct TakeoverWithALaterRequest {
  TakeoverWithALaterRequest(TestServer& testServer, bool laterFinishes)
      : server{testServer.getServer()},
        handler{server.getHandler()},
        socketPath{EdenStateDir{server.getEdenDir()}.getTakeoverSocketPath()},
        serverThread{server} {
    EXPECT_TRUE(driveMainEventBaseUntil(server, [&] {
      return server.getStatus() == EdenServer::RunState::RUNNING;
    }));
    handler->insertCancellationSource(
        /*requestId=*/1, std::move(earlier), "getScmStatusV2");

    auto& faultInjector = server.getServerState()->getFaultInjector();
    faultInjector.injectBlock("takeover", "wait_for_requests");
    if (laterFinishes) {
      faultInjector.injectBlock("takeover_wait", "poll");
    }
    blockedWaiter = std::thread([this, &faultInjector, laterFinishes] {
      takeoverBlocked = faultInjector.waitUntilBlocked("takeover", 5s);
      handler->insertCancellationSource(
          /*requestId=*/2, std::move(later), "globFiles");
      handler->removeCancellationSource(/*requestId=*/1);
      faultInjector.unblock("takeover", "wait_for_requests");
      if (laterFinishes) {
        // The takeover is about to poll again with the later request in
        // flight; let it finish first.
        pollBlocked = faultInjector.waitUntilBlocked("takeover_wait", 5s);
        handler->removeCancellationSource(/*requestId=*/2);
        faultInjector.unblock("takeover_wait", "poll");
      }
    });
    clientFuture = takeoverViaThread(
        socketPath, /*shouldThrowDuringTakeover=*/false, /*force=*/false);
    EXPECT_TRUE(driveMainEventBaseUntil(
        server,
        [&] { return clientFuture.wait_for(0ms) == std::future_status::ready; },
        10s));
    blockedWaiter.join();
    EXPECT_TRUE(takeoverBlocked);
  }

  EdenServer& server;
  std::shared_ptr<EdenServiceHandler> handler;
  AbsolutePath socketPath;
  ScopedServerThread serverThread;
  folly::CancellationSource earlier;
  folly::CancellationToken earlierToken{earlier.getToken()};
  folly::CancellationSource later;
  folly::CancellationToken laterToken{later.getToken()};
  std::thread blockedWaiter;
  bool takeoverBlocked{false};
  bool pollBlocked{false};
  std::future<TakeoverData> clientFuture;
};

} // namespace

TEST(EdenServerTakeoverTest, WaitsForRequestsThatArriveDuringTheGrace) {
  GFLAGS_NAMESPACE::SetCommandLineOptionWithMode(
      "enable_fault_injection", "true", GFLAGS_NAMESPACE::SET_FLAGS_VALUE);
  TestServer::Options options;
  options.takeoverRequestGrace = std::chrono::seconds{3};
  TestServer testServer{options};
  TakeoverWithALaterRequest takeover{testServer, /*laterFinishes=*/true};

  // The later request finished on its own, so the takeover went ahead and
  // cancelled nothing.
  EXPECT_TRUE(takeover.pollBlocked);
  EXPECT_NO_THROW((void)takeover.clientFuture.get());
  EXPECT_FALSE(takeover.earlierToken.isCancellationRequested());
  EXPECT_FALSE(takeover.laterToken.isCancellationRequested());

  ASSERT_TRUE(takeover.serverThread.waitForExit(5s));
  takeover.serverThread.join();
  ASSERT_NO_THROW(takeover.serverThread.throwIfServeFailed());
  EXPECT_TRUE(takeover.server.performCleanup());
}

TEST(
    EdenServerTakeoverTest,
    RefusedWhenARequestArrivingDuringTheGraceOutlivesIt) {
  GFLAGS_NAMESPACE::SetCommandLineOptionWithMode(
      "enable_fault_injection", "true", GFLAGS_NAMESPACE::SET_FLAGS_VALUE);
  TestServer::Options options;
  options.takeoverRequestGrace = std::chrono::seconds{1};
  TestServer testServer{options};
  TakeoverWithALaterRequest takeover{testServer, /*laterFinishes=*/false};

  EXPECT_THROW_RE(
      (void)takeover.clientFuture.get(),
      std::runtime_error,
      "refused.*globFiles");
  EXPECT_FALSE(takeover.laterToken.isCancellationRequested());
  EXPECT_EQ(takeover.server.getStatus(), EdenServer::RunState::RUNNING);

  // A forced takeover cancels the request and ends the server.
  auto forced = takeoverViaThread(
      takeover.socketPath,
      /*shouldThrowDuringTakeover=*/false,
      /*force=*/true);
  ASSERT_TRUE(driveMainEventBaseUntil(takeover.server, [&] {
    return forced.wait_for(0ms) == std::future_status::ready;
  }));
  EXPECT_NO_THROW((void)forced.get());
  EXPECT_TRUE(takeover.laterToken.isCancellationRequested());

  ASSERT_TRUE(takeover.serverThread.waitForExit(5s));
  takeover.serverThread.join();
  ASSERT_NO_THROW(takeover.serverThread.throwIfServeFailed());
  EXPECT_TRUE(takeover.server.performCleanup());
}

TEST(EdenServerTakeoverTest, CancelsRequestsThatArriveAfterTheGrace) {
  GFLAGS_NAMESPACE::SetCommandLineOptionWithMode(
      "enable_fault_injection", "true", GFLAGS_NAMESPACE::SET_FLAGS_VALUE);
  TestServer::Options options;
  options.takeoverRequestGrace = std::chrono::seconds{1};
  TestServer testServer{options};
  auto& server = testServer.getServer();
  auto handler = server.getHandler();
  auto socketPath = EdenStateDir{server.getEdenDir()}.getTakeoverSocketPath();

  ScopedServerThread serverThread{server};
  ASSERT_TRUE(driveMainEventBaseUntil(server, [&] {
    return server.getStatus() == EdenServer::RunState::RUNNING;
  }));

  // Nothing is in flight, so the takeover does not wait. Pause it once the
  // wait is over and start a request in that window.
  auto& faultInjector = server.getServerState()->getFaultInjector();
  faultInjector.injectBlock("takeover", "after_wait");
  folly::CancellationSource late;
  auto lateToken = late.getToken();
  bool takeoverBlocked = false;
  std::thread blockedWaiter([&] {
    takeoverBlocked = faultInjector.waitUntilBlocked("takeover", 5s);
    handler->insertCancellationSource(
        /*requestId=*/3, std::move(late), "globFiles");
    faultInjector.unblock("takeover", "after_wait");
  });

  auto clientFuture = takeoverViaThread(
      socketPath, /*shouldThrowDuringTakeover=*/false, /*force=*/false);
  ASSERT_TRUE(driveMainEventBaseUntil(
      server,
      [&] { return clientFuture.wait_for(0ms) == std::future_status::ready; },
      10s));
  blockedWaiter.join();
  EXPECT_TRUE(takeoverBlocked);
  EXPECT_NO_THROW((void)clientFuture.get());

  // Once the Thrift server has stopped and the grace period has passed again,
  // the request is cancelled rather than left to hold the stop to its
  // deadline.
  ASSERT_TRUE(driveMainEventBaseUntil(
      server, [&] { return lateToken.isCancellationRequested(); }, 10s));

  ASSERT_TRUE(serverThread.waitForExit(5s));
  serverThread.join();
  ASSERT_NO_THROW(serverThread.throwIfServeFailed());
  EXPECT_TRUE(server.performCleanup());
}

TEST(EdenServerTakeoverTest, DoesNotWaitWhenTheGracePeriodIsOff) {
  GFLAGS_NAMESPACE::SetCommandLineOptionWithMode(
      "enable_fault_injection", "true", GFLAGS_NAMESPACE::SET_FLAGS_VALUE);
  TestServer::Options options;
  options.takeoverRequestGrace = std::chrono::seconds{0};
  options.takeoverWaitsForRequests = false;
  TestServer testServer{options};
  auto& server = testServer.getServer();
  auto handler = server.getHandler();
  auto socketPath = EdenStateDir{server.getEdenDir()}.getTakeoverSocketPath();

  ScopedServerThread serverThread{server};
  ASSERT_TRUE(driveMainEventBaseUntil(server, [&] {
    return server.getStatus() == EdenServer::RunState::RUNNING;
  }));

  folly::CancellationSource source;
  auto token = source.getToken();
  handler->insertCancellationSource(
      /*requestId=*/4242, std::move(source), "globFiles");

  // With the kill switch off the takeover neither refuses nor cancels.
  auto clientFuture = takeoverViaThread(
      socketPath, /*shouldThrowDuringTakeover=*/false, /*force=*/false);
  ASSERT_TRUE(driveMainEventBaseUntil(server, [&] {
    return clientFuture.wait_for(0ms) == std::future_status::ready;
  }));
  EXPECT_NO_THROW((void)clientFuture.get());
  EXPECT_FALSE(token.isCancellationRequested());

  ASSERT_TRUE(serverThread.waitForExit(5s));
  serverThread.join();
  ASSERT_NO_THROW(serverThread.throwIfServeFailed());
  EXPECT_TRUE(server.performCleanup());
}

TEST(EdenServerTakeoverTest, WaitsForARecoveryMount) {
  GFLAGS_NAMESPACE::SetCommandLineOptionWithMode(
      "enable_fault_injection", "true", GFLAGS_NAMESPACE::SET_FLAGS_VALUE);
  TestServer::Options options;
  options.takeoverRequestGrace = std::chrono::seconds{5};
  TestServer testServer{options};
  auto& server = testServer.getServer();
  auto socketPath = EdenStateDir{server.getEdenDir()}.getTakeoverSocketPath();

  ScopedServerThread serverThread{server};
  ASSERT_TRUE(driveMainEventBaseUntil(server, [&] {
    return server.getStatus() == EdenServer::RunState::RUNNING;
  }));

  const auto mountPath = canonicalPath("/recovering");
  ASSERT_TRUE(server.beginRecoveryMount(mountPath));
  // The recovery mount finishes while the takeover waits for it.
  auto& faultInjector = server.getServerState()->getFaultInjector();
  faultInjector.injectBlock("takeover_wait", "poll");
  bool pollBlocked = false;
  std::thread blockedWaiter([&] {
    pollBlocked = faultInjector.waitUntilBlocked("takeover_wait", 5s);
    server.endRecoveryMount(mountPath);
    faultInjector.unblock("takeover_wait", "poll");
  });

  auto clientFuture = takeoverViaThread(
      socketPath, /*shouldThrowDuringTakeover=*/false, /*force=*/false);
  ASSERT_TRUE(driveMainEventBaseUntil(
      server,
      [&] { return clientFuture.wait_for(0ms) == std::future_status::ready; },
      10s));
  blockedWaiter.join();
  EXPECT_TRUE(pollBlocked);
  EXPECT_NO_THROW((void)clientFuture.get());

  ASSERT_TRUE(serverThread.waitForExit(5s));
  serverThread.join();
  ASSERT_NO_THROW(serverThread.throwIfServeFailed());
  EXPECT_TRUE(server.performCleanup());
}

TEST(EdenServerTakeoverTest, RefusedWhileARecoveryMountOutlivesTheGrace) {
  GFLAGS_NAMESPACE::SetCommandLineOptionWithMode(
      "enable_fault_injection", "true", GFLAGS_NAMESPACE::SET_FLAGS_VALUE);
  TestServer::Options options;
  options.takeoverRequestGrace = std::chrono::seconds{1};
  TestServer testServer{options};
  auto& server = testServer.getServer();
  auto socketPath = EdenStateDir{server.getEdenDir()}.getTakeoverSocketPath();

  ScopedServerThread serverThread{server};
  ASSERT_TRUE(driveMainEventBaseUntil(server, [&] {
    return server.getStatus() == EdenServer::RunState::RUNNING;
  }));

  const auto mountPath = canonicalPath("/recovering");
  ASSERT_TRUE(server.beginRecoveryMount(mountPath));
  // Forcing does not help: a mount cannot be cancelled.
  for (const bool force : {false, true}) {
    auto clientFuture = takeoverViaThread(
        socketPath, /*shouldThrowDuringTakeover=*/false, force);
    ASSERT_TRUE(driveMainEventBaseUntil(
        server,
        [&] { return clientFuture.wait_for(0ms) == std::future_status::ready; },
        10s));
    EXPECT_THROW_RE(
        (void)clientFuture.get(),
        std::runtime_error,
        "refused the graceful restart.*remounting /recovering");
    EXPECT_EQ(server.getStatus(), EdenServer::RunState::RUNNING);
  }

  // Once the recovery has mounted, a takeover goes ahead, and no recovery
  // mount can start after it.
  server.endRecoveryMount(mountPath);
  auto clientFuture = takeoverViaThread(
      socketPath, /*shouldThrowDuringTakeover=*/false, /*force=*/false);
  ASSERT_TRUE(driveMainEventBaseUntil(server, [&] {
    return clientFuture.wait_for(0ms) == std::future_status::ready;
  }));
  EXPECT_NO_THROW((void)clientFuture.get());
  EXPECT_FALSE(server.beginRecoveryMount(mountPath));

  ASSERT_TRUE(serverThread.waitForExit(5s));
  serverThread.join();
  ASSERT_NO_THROW(serverThread.throwIfServeFailed());
  EXPECT_TRUE(server.performCleanup());
}

TEST(EdenServerShutdownTest, StopsCleanlyWithAnIdleClientConnected) {
  TestServer::Options options;
  options.enableNfsServer = true;
  auto testServer = std::make_unique<TestServer>(options);
  auto& server = testServer->getServer();
  auto serverThread = std::make_unique<ScopedServerThread>(server);
  ASSERT_TRUE(driveMainEventBaseUntil(server, [&] {
    return server.getStatus() == EdenServer::RunState::RUNNING;
  }));

  // A header connection, as the Sapling clients open. The stop drains rocket
  // connections before serve() returns, but an idle header connection is left
  // alone until the server object is destroyed, so this one is still open
  // when the EdenServer goes away.
  folly::EventBase clientEventBase;
  auto socketPath = EdenStateDir{server.getEdenDir()}.getThriftSocketPath();
  apache::thrift::Client<EdenService> client{
      apache::thrift::HeaderClientChannel::newChannel(
          apache::thrift::HeaderClientChannel::WithoutRocketUpgrade{},
          folly::AsyncSocket::newSocket(
              &clientEventBase,
              folly::SocketAddress::makeFromPath(socketPath.view())))};
  EXPECT_EQ(client.sync_getPid(), getpid());

  server.stop();
  ASSERT_TRUE(serverThread->waitForExit(5s));
  ASSERT_NO_THROW(serverThread->throwIfServeFailed());
  serverThread.reset();
  ASSERT_TRUE(server.performCleanup());
  testServer.reset();

  EXPECT_THROW(
      client.sync_getPid(), apache::thrift::transport::TTransportException);
}

TEST_F(EdenServerTest, RepeatedTakeoverFailuresDoNotBreakShutdownFuture) {
  auto& server = testServer().getServer();
  auto& faultInjector = server.getServerState()->getFaultInjector();
  auto socketPath = EdenStateDir{server.getEdenDir()}.getTakeoverSocketPath();

  {
    // First reproduce the original handoff failure: the new process replies to
    // the readiness ping and then disconnects before takeover data transfer
    // completes. The daemon should recover and continue serving.
    ASSERT_NO_FATAL_FAILURE(
        driveTakeoverSendFailureToCleanup(testServer(), server, 1));
    EXPECT_FALSE(server.performCleanup());
  }

  {
    // Then trigger a second takeover failure after the server has already
    // recovered once. This exercises the path where TakeoverData has already
    // been prepared and must not leave takeoverComplete as a broken promise.
    ScopedServerThread serverThread{server};
    ASSERT_TRUE(driveMainEventBaseUntil(server, [&] {
      return server.getStatus() == EdenServer::RunState::RUNNING;
    }));

    faultInjector.injectError(
        "takeover",
        "post_prepare_data",
        folly::make_exception_wrapper<std::runtime_error>(
            "simulated takeover failure after data preparation"),
        1);

    auto clientFuture = takeoverViaThread(
        socketPath,
        /*shouldThrowDuringTakeover=*/false);

    ASSERT_TRUE(driveMainEventBaseUntil(server, [&] {
      return clientFuture.wait_for(0ms) == std::future_status::ready;
    }));

    bool clientSawException = false;
    try {
      (void)clientFuture.get();
    } catch (const std::exception&) {
      clientSawException = true;
    }
    ASSERT_TRUE(clientSawException);

    ASSERT_TRUE(serverThread.waitForExit(5s));
    serverThread.join();
    ASSERT_NO_THROW(serverThread.throwIfServeFailed());
    EXPECT_FALSE(server.performCleanup());
  }

  {
    // Finally verify the daemon is still healthy enough to restart cleanly
    // after both failed takeover attempts.
    ScopedServerThread serverThread{server};
    ASSERT_TRUE(driveMainEventBaseUntil(server, [&] {
      return server.getStatus() == EdenServer::RunState::RUNNING;
    }));

    server.stop();
    ASSERT_TRUE(serverThread.waitForExit(5s));
    serverThread.join();
    ASSERT_NO_THROW(serverThread.throwIfServeFailed());
    EXPECT_TRUE(server.performCleanup());
  }
}
#endif

#ifdef __linux__
TEST_F(EdenServerTest, GarbageCollectionReportsBusyWhileInhibited) {
  auto& server = testServer().getServer();
  TestMount mount{FakeTreeBuilder{}};
  auto inhibitor = mount.getEdenMount()->stealInodeGCLease();

  auto gc = server.garbageCollectInodes(
      *mount.getEdenMount(),
      mount.getRootInode(),
      std::chrono::system_clock::now(),
      ObjectFetchContext::getNullContext(),
      /*pressureBased=*/false);
  try {
    std::move(gc).get(10s);
    FAIL() << "expected inode GC admission to fail";
  } catch (const EdenError& error) {
    EXPECT_EQ(EBUSY, *error.errorCode());
  }
}
#endif

TEST_F(EdenServerTest, StopAllGarbageCollectionsDoesNotPoisonFutureGC) {
  auto& server = testServer().getServer();
#ifdef __linux__
  FakeTreeBuilder builder;
  builder.setFile("hello", "world");
  TestMount mount{builder};
  // The function argument is the snapshot for this GC run, even if reloadable
  // config has changed since the run was scheduled.
  mount.updateEdenConfig({{"experimental:enable-pressure-based-gc", "false"}});

  auto fuse = std::make_shared<FakeFuse>();
  mount.startFuseAndWait(fuse);

  auto entry =
      mount.getDispatcher()
          ->lookup(
              0, kRootNodeId, "hello"_pc, ObjectFetchContext::getNullContext())
          .get(0ms);
  ASSERT_NE(0, entry.nodeid);

  EXPECT_TRUE(server.stopAllGarbageCollections(
      /*maxRetries=*/0, /*retryInterval=*/std::chrono::seconds{0}));

  auto numInvalidated = server
                            .garbageCollectInodes(
                                *mount.getEdenMount(),
                                mount.getRootInode(),
                                std::chrono::system_clock::now() + 1h,
                                ObjectFetchContext::getNullContext(),
                                /*pressureBased=*/true)
                            .get(10s);
  EXPECT_GT(numInvalidated, 0u);

  mount.getEdenMount()->flushInvalidations().get(10s);
  fuse->close();
  mount.getEdenMount()->getFsChannelCompletionFuture().within(10s).getVia(
      mount.getServerExecutor().get());
#else
  EXPECT_TRUE(server.stopAllGarbageCollections(
      /*maxRetries=*/0, /*retryInterval=*/std::chrono::seconds{0}));
#endif
}

TEST_F(EdenServerTest, StopAllGarbageCollectionsWithStoppedEventBase) {
  auto& server = testServer().getServer();
  server.stop();

  ASSERT_EQ(EdenServer::RunState::SHUTTING_DOWN, server.getStatus());
  ASSERT_FALSE(server.getMainEventBase()->isRunning());
  EXPECT_TRUE(server.stopAllGarbageCollections(
      /*maxRetries=*/0, /*retryInterval=*/std::chrono::seconds{0}));
}

TEST_F(
    EdenServerTest,
    StopAllGarbageCollectionsFromWorkerWaitsForStoppedEventBase) {
  auto& server = testServer().getServer();
  server.stop();

  ASSERT_FALSE(server.getMainEventBase()->isRunning());
  auto stopResult = std::async(std::launch::async, [&server] {
    return server.stopAllGarbageCollections(
        /*maxRetries=*/0, /*retryInterval=*/std::chrono::seconds{0});
  });

  ASSERT_EQ(std::future_status::timeout, stopResult.wait_for(1s));
  server.getMainEventBase()->loopOnce();
  ASSERT_EQ(std::future_status::ready, stopResult.wait_for(5s));
  EXPECT_TRUE(stopResult.get());
}

#ifdef __linux__
namespace {

class SingleBackingStoreFactory : public BackingStoreFactory {
 public:
  explicit SingleBackingStoreFactory(std::shared_ptr<BackingStore> store)
      : store_{std::move(store)} {}

  std::shared_ptr<BackingStore> createBackingStore(
      BackingStoreType,
      const CreateParams&) override {
    return store_;
  }

 private:
  std::shared_ptr<BackingStore> store_;
};

} // namespace

// A server that can mount a checkout over a FakeFuse through EdenServer::mount,
// so the mount runs the production shutdown path from the FsChannel through
// mountFinished() to performCleanup().
class EdenServerMountTest : public ::testing::Test {
 protected:
  EdenServerMountTest() {
    GFLAGS_NAMESPACE::SetCommandLineOptionWithMode(
        "enable_fault_injection", "true", GFLAGS_NAMESPACE::SET_FLAGS_VALUE);
    FakeTreeBuilder builder;
    builder.setFile("hello", "world\n");
    backingStore_->putCommit(kCommit, builder.finalize(backingStore_, true))
        ->setReady();
    TestServer::Options options;
    options.backingStoreFactory = &factory_;
    testServer_ = std::make_unique<TestServer>(options);
  }

  TestServer& testServer() {
    return *testServer_;
  }

  // The checkout's config, read from its client directory; the first call
  // creates the client directory and the mount point.
  std::unique_ptr<CheckoutConfig> checkoutConfig() {
    auto clientDir = testServer_->getTmpDir() + "client"_pc;
    ensureDirectoryExists(clientDir + "local"_pc);
    mountPath_ = testServer_->getTmpDir() + "mount"_pc;
    ensureDirectoryExists(mountPath_);
    writeFile(
        clientDir + "config.toml"_pc,
        folly::ByteRange{folly::StringPiece{
            "[repository]\npath = \"/repo\"\ntype = \"hg\"\n"}})
        .value();
    auto config =
        CheckoutConfig::loadFromClientDirectory(mountPath_, clientDir);
    config->setCheckedOutCommit(kCommit);
    return config;
  }

  std::shared_ptr<EdenMount> mountCheckout() {
    auto& server = testServer_->getServer();
    auto config = checkoutConfig();

    auto* privHelper =
        dynamic_cast<FakePrivHelper*>(server.getServerState()->getPrivHelper());
    privHelper->registerMount(mountPath_, fuse_);

    // The mount's ImmediateFuture defers its work until it is driven; run it
    // on the server's pool so the FUSE handshake below can happen meanwhile.
    auto mountFuture = server.mount(std::move(config), /*readOnly=*/false)
                           .semi()
                           .via(server.getServerState()->getThreadPool().get());
    // The mount opens the FUSE device partway through its start; answer the
    // FUSE_INIT it then sends, as the kernel would.
    auto deadline = std::chrono::steady_clock::now() + 10s;
    while (!fuse_->isStarted()) {
      if (std::chrono::steady_clock::now() > deadline) {
        throw std::runtime_error("the mount never opened its FUSE device");
      }
      std::this_thread::sleep_for(10ms);
    }
    fuse_->sendInitRequest();
    fuse_->recvResponse();
    return std::move(mountFuture).get(10s);
  }

  const AbsolutePath& mountPath() const {
    return mountPath_;
  }

 private:
  static inline const RootId kCommit{"1"};

  std::shared_ptr<FakeBackingStore> backingStore_{
      std::make_shared<FakeBackingStore>()};
  SingleBackingStoreFactory factory_{backingStore_};
  std::unique_ptr<TestServer> testServer_;
  AbsolutePath mountPath_;
  std::shared_ptr<FakeFuse> fuse_{std::make_shared<FakeFuse>()};
};

// `eden rm` unmounts a checkout, and `eden stop` arrives while the mount is
// still shutting down: performCleanup() finds the mount in mountPoints_ and
// waits on its unmount promise.
TEST_F(EdenServerMountTest, StopWhileAMountIsShuttingDown) {
  auto& server = testServer().getServer();
  ScopedServerThread serverThread{server};
  ASSERT_TRUE(driveMainEventBaseUntil(server, [&] {
    return server.getStatus() == EdenServer::RunState::RUNNING;
  }));
  auto mount = mountCheckout();

  // Hold the mount in SHUTTING_DOWN: its shutdown has started, and the
  // continuation that erases it from mountPoints_ has not run.
  auto& faultInjector = server.getServerState()->getFaultInjector();
  faultInjector.injectBlock("mountShutdown", ".*");
  auto unmountFuture = server.unmount(mountPath(), UnmountOptions{});
  ASSERT_TRUE(driveMainEventBaseUntil(
      server, [&] { return mount->getState() == MountState::SHUTTING_DOWN; }));
  ASSERT_TRUE(faultInjector.waitUntilBlocked("mountShutdown", 5s));

  server.stop();
  ASSERT_TRUE(serverThread.waitForExit(5s));
  serverThread.join();
  ASSERT_NO_THROW(serverThread.throwIfServeFailed());
  faultInjector.removeFault("mountShutdown", ".*");
  faultInjector.unblock("mountShutdown", ".*");

  // unmountAll() waits on the same promise the shutdown's continuation
  // fulfills, so the stop waits for the shutdown and exits cleanly.
  EXPECT_TRUE(server.performCleanup());
  EXPECT_NO_THROW(std::move(unmountFuture).get(1s));
}

// Unmount recovery remounts a checkout that config.json lists and
// mountPoints_ does not, unless the intentional-unmount marker is set, so
// addToMountPoints() clears the marker once the mount is in mountPoints_. A
// mount that fails before that point, here because the path is already
// mounted, leaves the marker alone.
TEST_F(EdenServerMountTest, RefusedMountLeavesTheIntentionalMarker) {
  auto& server = testServer().getServer();
  auto mount = mountCheckout();
  mount->getCheckoutConfig()->setIntentionallyUnmountedFlag();

  EXPECT_THROW_RE(
      (void)server.mount(checkoutConfig(), /*readOnly=*/false),
      EdenError,
      "already mounted");
  EXPECT_TRUE(mount->getCheckoutConfig()->isIntentionallyUnmounted());
  EXPECT_EQ(MountState::RUNNING, mount->getState());
}

// An unmount that finds the mount right after its registration writes the
// marker and cancels the start. The clear runs under the mountPoints_ lock
// that unmount() takes to find the entry, so it cannot run after that
// unmount's write: the marker survives the cancelled start, and unmount
// recovery leaves the checkout alone.
TEST_F(EdenServerMountTest, UnmountDuringMountStartKeepsItsMarker) {
  auto& server = testServer().getServer();
  auto& faultInjector = server.getServerState()->getFaultInjector();
  faultInjector.injectBlock("mountRegistered", ".*");
  auto config = checkoutConfig();
  // The same client directory, to read the marker from.
  auto checkout = checkoutConfig();

  // mount() blocks at the fault until the unmount below has found the entry.
  auto mountFuture = folly::Future<std::shared_ptr<EdenMount>>::makeEmpty();
  std::thread mounter([&] {
    mountFuture = server.mount(std::move(config), /*readOnly=*/false)
                      .semi()
                      .via(server.getServerState()->getThreadPool().get());
  });
  ASSERT_TRUE(faultInjector.waitUntilBlocked("mountRegistered", 5s));
  auto unmountFuture = server.unmount(mountPath(), UnmountOptions{});
  EXPECT_TRUE(checkout->isIntentionallyUnmounted());
  faultInjector.removeFault("mountRegistered", ".*");
  faultInjector.unblock("mountRegistered", ".*");
  mounter.join();

  // The cancelled start is torn down on the main event base.
  ASSERT_TRUE(
      driveMainEventBaseUntil(server, [&] { return mountFuture.isReady(); }));
  EXPECT_THROW(std::move(mountFuture).get(), EdenMountCancelled);
  std::move(unmountFuture)
      .via(server.getMainEventBase())
      .getVia(server.getMainEventBase());
  EXPECT_TRUE(checkout->isIntentionallyUnmounted());
  EXPECT_TRUE(server.getAllMountPoints().empty());
}

TEST_F(EdenServerMountTest, FailedUnmountClearsIntentionalMarker) {
  auto& server = testServer().getServer();
  auto mount = mountCheckout();
  server.getServerState()->getFaultInjector().injectError(
      "unmount",
      ".*",
      folly::make_exception_wrapper<std::runtime_error>("unmount failed"),
      1);

  EXPECT_THROW_RE(
      server.unmount(mountPath(), UnmountOptions{})
          .via(server.getMainEventBase())
          .getVia(server.getMainEventBase()),
      std::runtime_error,
      "unmount failed");
  EXPECT_FALSE(mount->getCheckoutConfig()->isIntentionallyUnmounted());
  EXPECT_EQ(MountState::RUNNING, mount->getState());
}

TEST_F(EdenServerMountTest, DroppedUnmountCallbackDoesNotReschedule) {
  class DroppingExecutor : public folly::Executor {
   public:
    void add(folly::Func task) override {
      if (stopped) {
        ++addedAfterStop;
      } else {
        tasks.push_back(std::move(task));
      }
    }

    void drain() {
      while (!tasks.empty()) {
        auto ready = std::move(tasks);
        tasks.clear();
        for (auto& task : ready) {
          task();
        }
      }
    }

    void stop() {
      stopped = true;
      tasks.clear();
    }

    bool stopped{false};
    size_t addedAfterStop{0};
    std::vector<folly::Func> tasks;
  } executor;

  auto& server = testServer().getServer();
  auto mount = mountCheckout();
  auto& faultInjector = server.getServerState()->getFaultInjector();
  faultInjector.injectBlock("mountShutdown", ".*");
  auto unmount = server.unmount(mountPath(), UnmountOptions{}).via(&executor);
  executor.drain();
  ASSERT_TRUE(driveMainEventBaseUntil(
      server, [&] { return mount->getState() == MountState::SHUTTING_DOWN; }));
  ASSERT_TRUE(faultInjector.waitUntilBlocked("mountShutdown", 5s));

  faultInjector.removeFault("mountShutdown", ".*");
  faultInjector.unblock("mountShutdown", ".*");
  ASSERT_TRUE(driveMainEventBaseUntil(
      server, [&] { return server.getAllMountPoints().empty(); }));
  EXPECT_TRUE(unmount.isReady());

  // A stopped pool discards queued callbacks rather than executing them.
  // Destroying one must not enqueue another callback onto that same pool.
  executor.stop();
  EXPECT_EQ(0, executor.addedAfterStop);
  ASSERT_TRUE(unmount.isReady());
  EXPECT_NO_THROW(std::move(unmount).get());
}
#endif

TEST(EdenServerMountHealthTest, OnlyRunningMountsAreProbed) {
  EXPECT_TRUE(EdenServer::shouldProbeMountHealth(MountState::RUNNING));

  // Probing any other state asks whether the kernel agrees about a mount it
  // was never told about, or has already forgotten, which reports a
  // DaemonRunningKernelMountMissing that is guaranteed to be false.
  for (auto state :
       {MountState::UNINITIALIZED,
        MountState::INITIALIZING,
        MountState::INITIALIZED,
        MountState::STARTING,
        MountState::FUSE_ERROR,
        MountState::INIT_ERROR,
        MountState::SHUTTING_DOWN,
        MountState::SHUT_DOWN,
        MountState::DESTROYING}) {
    EXPECT_FALSE(EdenServer::shouldProbeMountHealth(state))
        << "MountState " << static_cast<int>(state) << " must not be probed";
  }
}

} // namespace facebook::eden
