/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/nfs/rpc/RpcServer.h"

#ifndef _WIN32
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>
#endif

#include <chrono>
#include <cstring>
#include <thread>

#include <folly/executors/ManualExecutor.h>
#include <folly/io/IOBuf.h>
#include <folly/io/IOBufQueue.h>
#include <folly/testing/TestUtil.h>
#include <gtest/gtest.h>

#include "eden/fs/nfs/NfsdRpc.h"
#include "eden/fs/nfs/rpc/Rpc.h"
#include "eden/fs/utils/RequestPermitVendor.h"

namespace {

using namespace facebook::eden;

class TestServerProcessor : public RpcServerProcessor {};

class TestFastPathProcessor : public RpcServerProcessor {
 public:
  bool shouldFastPathRPCs() const override {
    return true;
  }
  bool isUnimplementedProc(uint32_t proc) const override {
    return proc >= 22;
  }
};

class TestEchoProcessor : public RpcServerProcessor {
 public:
  ImmediateFuture<folly::Unit> dispatchRpc(
      folly::io::Cursor /*deser*/,
      folly::io::QueueAppender ser,
      uint32_t xid,
      uint32_t /*progNumber*/,
      uint32_t /*progVersion*/,
      uint32_t procNumber,
      const std::optional<authsys_parms>& /*authSysCreds*/) override {
    serializeReply(ser, accept_stat::SUCCESS, xid);
    XdrTrait<uint32_t>::serialize(ser, procNumber);
    return folly::unit;
  }
};

std::unique_ptr<folly::IOBuf>
buildRpcRequestWithCred(uint32_t xid, uint32_t proc, opaque_auth cred) {
  folly::IOBufQueue queue{folly::IOBufQueue::cacheChainLength()};
  folly::io::QueueAppender ser(&queue, 256);

  XdrTrait<uint32_t>::serialize(ser, 0); // fragment header placeholder
  rpc_msg_call call{
      xid,
      msg_type::CALL,
      call_body{
          kRPCVersion,
          kNfsdProgNumber,
          kNfsd3ProgVersion,
          proc,
          std::move(cred),
          opaque_auth{auth_flavor::AUTH_NONE, {}},
      },
  };
  XdrTrait<rpc_msg_call>::serialize(ser, call);

  auto len = static_cast<uint32_t>(queue.chainLength() - sizeof(uint32_t));
  auto buf = queue.move();
  if (!buf) {
    ADD_FAILURE() << "serialized RPC request is empty";
    return nullptr;
  }
  auto* header = reinterpret_cast<uint32_t*>(buf->writableData());
  if (!header) {
    ADD_FAILURE() << "serialized RPC request has no writable data";
    return nullptr;
  }
  *header = folly::Endian::big(len | 0x80000000);
  return buf;
}

std::unique_ptr<folly::IOBuf> buildRpcRequest(uint32_t xid, uint32_t proc) {
  return buildRpcRequestWithCred(
      xid, proc, opaque_auth{auth_flavor::AUTH_NONE, {}});
}

opaque_auth makeAuthSysCred(const authsys_parms& creds) {
  folly::IOBufQueue queue{folly::IOBufQueue::cacheChainLength()};
  folly::io::QueueAppender ser(&queue, 256);
  XdrTrait<authsys_parms>::serialize(ser, creds);
  auto buf = queue.move();
  auto bytes = buf->coalesce();
  return opaque_auth{
      auth_flavor::AUTH_SYS, OpaqueBytes{bytes.begin(), bytes.end()}};
}

std::unique_ptr<folly::IOBuf> splitIntoTwoFragments(
    std::unique_ptr<folly::IOBuf> request) {
  auto bytes = request->coalesce();
  auto payload = bytes.subpiece(sizeof(uint32_t));
  auto firstLength = payload.size() / 2;
  auto secondLength = payload.size() - firstLength;

  folly::IOBufQueue queue{folly::IOBufQueue::cacheChainLength()};
  folly::io::QueueAppender appender(&queue, 256);
  appender.writeBE<uint32_t>(static_cast<uint32_t>(firstLength));
  appender.push(payload.data(), firstLength);
  appender.writeBE<uint32_t>(0x80000000u | static_cast<uint32_t>(secondLength));
  appender.push(payload.data() + firstLength, secondLength);
  return queue.move();
}

/**
 * Put an empty, non-final fragment in front of a single-fragment request.
 */
std::unique_ptr<folly::IOBuf> prependEmptyFragment(
    std::unique_ptr<folly::IOBuf> request) {
  auto bytes = request->coalesce();

  folly::IOBufQueue queue{folly::IOBufQueue::cacheChainLength()};
  folly::io::QueueAppender appender(&queue, 256);
  appender.writeBE<uint32_t>(0);
  appender.push(bytes.data(), bytes.size());
  return queue.move();
}

std::unique_ptr<folly::IOBuf> buildNullRpcRequest(uint32_t xid) {
  return buildRpcRequest(xid, 0);
}

struct RpcServerTest : ::testing::Test {
  std::shared_ptr<RpcServer> createTestServer(
      std::shared_ptr<RpcServerProcessor> proc,
      std::shared_ptr<folly::Executor> executor =
          folly::getUnsafeMutableGlobalCPUExecutor()) {
    return RpcServer::create(
        std::move(proc),
        &evb,
        std::move(executor),
        nullptr,
        /*maximumInFlightRequests=*/1000,
        /*highNfsRequestsLogInterval=*/std::chrono::minutes{10});
  }

  std::shared_ptr<RpcServer> createTestServerWithManualExecutor(
      std::shared_ptr<RpcServerProcessor> proc) {
    manualExecutor_ = std::make_shared<folly::ManualExecutor>();
    return createTestServer(std::move(proc), manualExecutor_);
  }

#ifndef _WIN32
  /**
   * Create a connected socketpair, initialize the server with one end,
   * and return the client fd. Caller must close the returned fd.
   */
  int connectClient(RpcServer& server) {
    int fds[2];
    EXPECT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));
    server.initializeConnectedSocket(folly::File(fds[0], true));
    return fds[1];
  }

  /**
   * Write a serialized RPC request to the client fd and drive the
   * EventBase so it reads the bytes and processes any inline fast-path
   * replies (null, PROC_UNAVAIL, JUKEBOX) before dispatching to the
   * thread pool.
   */
  void sendRequest(int clientFd, std::unique_ptr<folly::IOBuf> request) {
    if (!request) {
      ADD_FAILURE() << "RPC request must not be null";
      return;
    }
    auto bytes = request->coalesce();
    ASSERT_EQ(
        static_cast<ssize_t>(bytes.size()),
        write(clientFd, bytes.data(), bytes.size()));
    evb.loopOnce();
  }

  /**
   * Poll for data on the client fd. Returns true if data is available
   * within @p timeoutMs milliseconds. On a Unix socketpair the reply
   * is local and arrives in microseconds; callers use a generous
   * timeout as a safety net so a broken test fails with a clear
   * message instead of hanging the test runner.
   */
  bool pollForReply(int clientFd, int timeoutMs) {
    struct pollfd pfd{};
    pfd.fd = clientFd;
    pfd.events = POLLIN;
    return poll(&pfd, 1, timeoutMs) > 0;
  }

  /**
   * Bind the server to a unix socket under @p tmpDir and return the bound
   * address. A unix socket exercises the same accept path as the TCP
   * socket used in production, and unlike loopback TCP it works in
   * sandboxed test environments.
   */
  folly::SocketAddress bindUnixSocket(
      RpcServer& server,
      const folly::test::TemporaryDirectory& tmpDir) {
    folly::SocketAddress bindAddr;
    bindAddr.setFromPath((tmpDir.path() / "rpc.sock").string());
    server.initialize(bindAddr);
    return server.getAddr();
  }

  /**
   * Connect a new client socket to the server's listening address and
   * return the fd. Caller must close the fd.
   */
  int connectToServer(const folly::SocketAddress& addr) {
    int fd = ::socket(addr.getFamily(), SOCK_STREAM, 0);
    EXPECT_GE(fd, 0) << folly::errnoStr(errno);
    sockaddr_storage ss{};
    auto len = addr.getAddress(&ss);
    EXPECT_EQ(0, ::connect(fd, reinterpret_cast<sockaddr*>(&ss), len))
        << "connect to " << addr.describe() << ": " << folly::errnoStr(errno);
    return fd;
  }

  /**
   * Pump the EventBase until done() holds or a bounded number of
   * iterations elapse. The EventBase is driven manually by the test
   * thread, so nothing else can make progress between pumps. The loop
   * exits as soon as the predicate holds; the brief sleep only yields
   * between non-blocking pumps, it does not gate correctness.
   */
  template <typename Done>
  void drive(Done&& done) {
    for (int i = 0; i < 1000 && !done(); ++i) {
      evb.loopOnce(EVLOOP_NONBLOCK);
      // @lint-ignore CLANGTIDY facebook-hte-BadCall-sleep_for
      std::this_thread::sleep_for(std::chrono::milliseconds(1));
    }
  }

  /**
   * Returns true once the peer has closed the connection: the fd is
   * readable and read() reports EOF.
   */
  bool sawEof(int fd) {
    if (!pollForReply(fd, 0)) {
      return false;
    }
    uint8_t byte;
    return read(fd, &byte, 1) == 0;
  }

  /**
   * Read a reply from the client fd. Assumes a single read() returns
   * the complete reply — safe on Unix socketpairs with small messages
   * but would need a loop for TCP or replies larger than 256 bytes.
   */
  std::vector<uint8_t> readReply(int clientFd) {
    uint8_t buf[256];
    auto nread = read(clientFd, buf, sizeof(buf));
    EXPECT_GT(nread, 0);
    return std::vector<uint8_t>(buf, buf + nread);
  }

  /**
   * Send an RPC request and wait for the reply on the EventBase.
   * Returns the raw reply bytes. Asserts that a reply arrives within
   * the timeout.
   */
  std::vector<uint8_t> sendAndReceive(
      int clientFd,
      std::unique_ptr<folly::IOBuf> request,
      const char* failMsg = "Expected an RPC reply") {
    sendRequest(clientFd, std::move(request));
    EXPECT_TRUE(pollForReply(clientFd, 1000)) << failMsg;
    return readReply(clientFd);
  }

  /**
   * Like sendAndReceive, for a request that the server dispatches on the
   * manual executor rather than answering inline on the EventBase.
   */
  std::vector<uint8_t> sendAndReceiveOnExecutor(
      int clientFd,
      std::unique_ptr<folly::IOBuf> request,
      const char* failMsg = "Expected an RPC reply") {
    sendRequest(clientFd, std::move(request));
    for (int i = 0; i < 20; ++i) {
      manualExecutor_->run();
      evb.loopOnce(EVLOOP_NONBLOCK);
    }
    EXPECT_TRUE(pollForReply(clientFd, 1000)) << failMsg;
    return readReply(clientFd);
  }

  /**
   * Read a big-endian uint32_t from a raw reply at the given byte offset.
   */
  static uint32_t readBigEndianU32(
      const std::vector<uint8_t>& data,
      size_t offset) {
    EXPECT_GE(data.size(), offset + 4);
    uint32_t val;
    memcpy(&val, data.data() + offset, sizeof(val));
    return folly::Endian::big(val);
  }

  /**
   * Clean up: close client fd, reset server, drain EventBase.
   */
  void cleanup(int clientFd, std::shared_ptr<RpcServer>& server) {
    close(clientFd);
    server.reset();
    evb.loopOnce();
  }
#endif // !_WIN32

  folly::EventBase evb;
  std::shared_ptr<folly::ManualExecutor> manualExecutor_;
};

TEST_F(RpcServerTest, takeover_before_initialize) {
  auto server = createTestServer(std::make_shared<TestServerProcessor>());

  auto takeover = server->takeoverStop();
  evb.drive();
  EXPECT_TRUE(takeover.isReady());
}

TEST_F(RpcServerTest, takeover_after_initialize) {
  auto server = createTestServer(std::make_shared<TestServerProcessor>());

  folly::SocketAddress addr;
  addr.setFromIpPort("::0", 0);
  server->initialize(addr);

  auto takeover = server->takeoverStop();
  evb.drive();
  EXPECT_TRUE(takeover.isReady());
}

TEST_F(RpcServerTest, takeover_from_takeover) {
  auto server = createTestServer(std::make_shared<TestServerProcessor>());

  folly::SocketAddress addr;
  addr.setFromIpPort("::0", 0);
  server->initialize(addr);

  auto takeover = server->takeoverStop();
  evb.drive();
  EXPECT_TRUE(takeover.isReady());

  server.reset();
  evb.drive();

  auto newServer = createTestServer(std::make_shared<TestServerProcessor>());
  newServer->initializeServerSocket(std::move(takeover).get());

  takeover = newServer->takeoverStop();
  evb.drive();
  EXPECT_TRUE(takeover.isReady());
}

#ifndef _WIN32
// Tests below use Unix socketpair/poll APIs not available on Windows.

class ShutdownRecordingProcessor : public RpcServerProcessor {
 public:
  void clientConnected() override {
    clientCount_.fetch_add(1, std::memory_order_release);
  }

  void onExtraConnection() override {
    extraConnectionCount_.fetch_add(1, std::memory_order_release);
  }

  void onExtraConnectionRefused() override {
    extraConnectionRefusedCount_.fetch_add(1, std::memory_order_release);
  }

  void onShutdown(RpcStopData data) override {
    lastReason_ = data.reason;
    shutdownCount_.fetch_add(1, std::memory_order_release);
  }

  int clientCount() const {
    return clientCount_.load(std::memory_order_acquire);
  }

  int extraConnectionCount() const {
    return extraConnectionCount_.load(std::memory_order_acquire);
  }

  int extraConnectionRefusedCount() const {
    return extraConnectionRefusedCount_.load(std::memory_order_acquire);
  }

  int shutdownCount() const {
    return shutdownCount_.load(std::memory_order_acquire);
  }

  std::atomic<int> clientCount_{0};
  std::atomic<int> extraConnectionCount_{0};
  std::atomic<int> extraConnectionRefusedCount_{0};
  std::atomic<int> shutdownCount_{0};
  std::optional<RpcStopReason> lastReason_;
};

class SingleClientShutdownRecordingProcessor
    : public ShutdownRecordingProcessor {
 public:
  bool acceptsMultipleConnections() const override {
    return false;
  }
};

TEST_F(RpcServerTest, extra_connection_is_refused_for_single_client_server) {
  auto proc = std::make_shared<SingleClientShutdownRecordingProcessor>();
  auto server = createTestServer(proc);

  folly::test::TemporaryDirectory tmpDir;
  auto addr = bindUnixSocket(*server, tmpDir);

  // The kernel's connection, established once at mount time.
  int kernelFd = connectToServer(addr);
  drive([&] { return proc->clientCount() == 1; });
  ASSERT_EQ(1, proc->clientCount());

  // Some other local process — anything able to reach the socket — connects
  // and immediately disconnects. The server refuses the connection (the
  // client observes EOF), and the processor is not told to shut down: only
  // the kernel's connection controls the server's lifetime.
  int scannerFd = connectToServer(addr);
  drive([&] { return sawEof(scannerFd); });
  EXPECT_TRUE(sawEof(scannerFd)) << "extra connection should be refused";
  close(scannerFd);
  EXPECT_EQ(1, proc->clientCount());
  EXPECT_EQ(1, proc->extraConnectionCount());
  EXPECT_EQ(1, proc->extraConnectionRefusedCount());
  EXPECT_EQ(0, proc->shutdownCount());

  // EOF on the kernel's connection still stops the server: that is how a
  // real unmount is detected.
  close(kernelFd);
  drive([&] { return proc->shutdownCount() > 0; });
  EXPECT_EQ(1, proc->shutdownCount());
  EXPECT_EQ(RpcStopReason::UNMOUNT, proc->lastReason_);

  server.reset();
  evb.loopOnce();
}

TEST_F(RpcServerTest, extra_connection_is_accepted_for_multi_client_server) {
  // Mountd-style servers carry each exchange on its own connection, so a
  // second connection must still be accepted and stay open.
  auto proc = std::make_shared<ShutdownRecordingProcessor>();
  auto server = createTestServer(proc);

  folly::test::TemporaryDirectory tmpDir;
  auto addr = bindUnixSocket(*server, tmpDir);

  int first = connectToServer(addr);
  drive([&] { return proc->clientCount() == 1; });
  ASSERT_EQ(1, proc->clientCount());
  int second = connectToServer(addr);
  drive([&] { return proc->clientCount() == 2; });
  ASSERT_EQ(2, proc->clientCount());
  EXPECT_EQ(1, proc->extraConnectionCount());
  EXPECT_EQ(0, proc->extraConnectionRefusedCount());

  // Neither connection was closed by the server.
  EXPECT_FALSE(pollForReply(first, 0));
  EXPECT_FALSE(pollForReply(second, 0));

  close(second);
  close(first);
  server.reset();
  evb.loopOnce();
}

TEST_F(RpcServerTest, null_rpc_bypasses_thread_pool) {
  auto server = createTestServerWithManualExecutor(
      std::make_shared<TestFastPathProcessor>());
  auto clientFd = connectClient(*server);

  auto reply = sendAndReceive(
      clientFd,
      buildNullRpcRequest(42),
      "Null RPC reply should arrive without needing the thread pool");

  EXPECT_EQ(readBigEndianU32(reply, 4), 42u); // xid
  EXPECT_EQ(readBigEndianU32(reply, 8), 1u); // msg_type::REPLY
  cleanup(clientFd, server);
}

TEST_F(RpcServerTest, proc_unavail_fast_path) {
  auto server = createTestServerWithManualExecutor(
      std::make_shared<TestFastPathProcessor>());
  auto clientFd = connectClient(*server);

  // Send an unknown proc (99) which isUnimplementedProc returns true for.
  auto reply = sendAndReceive(
      clientFd,
      buildRpcRequest(55, /*proc=*/99),
      "PROC_UNAVAIL reply should arrive without needing the thread pool");

  EXPECT_EQ(readBigEndianU32(reply, 4), 55u); // xid
  EXPECT_EQ(readBigEndianU32(reply, 8), 1u); // msg_type::REPLY
  // accept_stat::PROC_UNAVAIL = 3, at offset 24
  EXPECT_EQ(readBigEndianU32(reply, 24), 3u); // accept_stat::PROC_UNAVAIL
  cleanup(clientFd, server);
}

TEST_F(RpcServerTest, normal_proc_not_fast_pathed) {
  auto server = createTestServerWithManualExecutor(
      std::make_shared<TestFastPathProcessor>());
  auto clientFd = connectClient(*server);

  sendRequest(clientFd, buildRpcRequest(77, /*proc=*/1));

  // No inline reply — proc=1 is neither null nor unimplemented, so it
  // should be dispatched to the thread pool, not fast-pathed.
  EXPECT_FALSE(pollForReply(clientFd, 100))
      << "Normal proc should not get an inline reply";

  // The dispatch pipeline has multiple hops through the ManualExecutor
  // and EventBase. Alternate cranking both until the reply arrives.
  for (int i = 0; i < 20; ++i) {
    manualExecutor_->run();
    evb.loopOnce(EVLOOP_NONBLOCK);
  }

  EXPECT_TRUE(pollForReply(clientFd, 1000))
      << "Reply should arrive after cranking the thread pool";

  cleanup(clientFd, server);
}

class FastPathShutdownRecordingProcessor : public ShutdownRecordingProcessor {
 public:
  bool shouldFastPathRPCs() const override {
    return true;
  }
};

TEST_F(RpcServerTest, failed_inline_reply_stops_reading_requests) {
  auto proc = std::make_shared<FastPathShutdownRecordingProcessor>();
  auto server = createTestServerWithManualExecutor(proc);
  auto clientFd = connectClient(*server);

  // The client sends a null RPC, answered inline, and a request for the thread
  // pool, then disconnects without reading. Both requests arrive in one read;
  // writing the null RPC's reply fails, and the socket reports the error to
  // the connection handler before the write returns.
  folly::IOBufQueue requests;
  requests.append(buildNullRpcRequest(1));
  requests.append(buildRpcRequest(2, /*proc=*/1));
  auto requestBuf = requests.move();
  auto bytes = requestBuf->coalesce();
  ASSERT_EQ(
      static_cast<ssize_t>(bytes.size()),
      write(clientFd, bytes.data(), bytes.size()));
  close(clientFd);

  drive([&] { return proc->shutdownCount() > 0; });
  EXPECT_EQ(0u, manualExecutor_->drain());
  evb.loopOnce(EVLOOP_NONBLOCK);

  EXPECT_EQ(1, proc->shutdownCount());
  EXPECT_EQ(RpcStopReason::ERROR, proc->lastReason_);

  server.reset();
  evb.loopOnce();
}

TEST_F(RpcServerTest, request_split_into_two_fragments) {
  auto server =
      createTestServerWithManualExecutor(std::make_shared<TestEchoProcessor>());
  auto clientFd = connectClient(*server);

  auto reply = sendAndReceiveOnExecutor(
      clientFd, splitIntoTwoFragments(buildRpcRequest(77, /*proc=*/1)));
  EXPECT_EQ(readBigEndianU32(reply, 4), 77u); // xid
  EXPECT_EQ(readBigEndianU32(reply, 8), 1u); // msg_type::REPLY
  EXPECT_EQ(readBigEndianU32(reply, 24), 0u); // accept_stat::SUCCESS
  EXPECT_EQ(readBigEndianU32(reply, 28), 1u); // echoed procedure number

  cleanup(clientFd, server);
}

TEST_F(RpcServerTest, request_with_an_empty_first_fragment) {
  auto server =
      createTestServerWithManualExecutor(std::make_shared<TestEchoProcessor>());
  auto clientFd = connectClient(*server);

  auto reply = sendAndReceiveOnExecutor(
      clientFd, prependEmptyFragment(buildRpcRequest(79, /*proc=*/2)));
  EXPECT_EQ(readBigEndianU32(reply, 4), 79u); // xid
  EXPECT_EQ(readBigEndianU32(reply, 8), 1u); // msg_type::REPLY
  EXPECT_EQ(readBigEndianU32(reply, 24), 0u); // accept_stat::SUCCESS
  EXPECT_EQ(readBigEndianU32(reply, 28), 2u); // echoed procedure number

  cleanup(clientFd, server);
}

TEST_F(RpcServerTest, null_request_split_into_two_fragments_with_fast_path) {
  auto server = createTestServerWithManualExecutor(
      std::make_shared<TestFastPathProcessor>());
  auto clientFd = connectClient(*server);

  auto reply =
      sendAndReceive(clientFd, splitIntoTwoFragments(buildNullRpcRequest(78)));
  EXPECT_EQ(readBigEndianU32(reply, 4), 78u); // xid
  EXPECT_EQ(readBigEndianU32(reply, 8), 1u); // msg_type::REPLY
  EXPECT_EQ(readBigEndianU32(reply, 24), 0u); // accept_stat::SUCCESS

  cleanup(clientFd, server);
}

class TestJukeboxProcessor : public RpcServerProcessor {
 public:
  bool shouldFastPathRPCs() const override {
    return true;
  }

  InlineRejectResult tryInlineReject() override {
    if (rejectAll_.load()) {
      return {true, nullptr};
    }
    return {};
  }

  void serializeInlineReject(
      uint32_t /*proc*/,
      uint32_t xid,
      folly::io::QueueAppender& ser) override {
    serializeReply(ser, accept_stat::SUCCESS, xid);
    GETATTR3res res;
    res.tag = nfsstat3::NFS3ERR_JUKEBOX;
    XdrTrait<GETATTR3res>::serialize(ser, res);
  }

  std::atomic<bool> rejectAll_{true};
};

class TestPermitProcessor : public RpcServerProcessor {
 public:
  explicit TestPermitProcessor(size_t capacity) : vendor_(capacity) {}

  bool shouldFastPathRPCs() const override {
    return true;
  }

  bool isUnimplementedProc(uint32_t proc) const override {
    return proc >= 22;
  }

  InlineRejectResult tryInlineReject() override {
    auto permit = vendor_.tryAcquirePermit();
    if (!permit) {
      return {true, nullptr};
    }
    return {false, std::move(permit)};
  }

  void serializeInlineReject(
      uint32_t /*proc*/,
      uint32_t xid,
      folly::io::QueueAppender& ser) override {
    serializeReply(ser, accept_stat::SUCCESS, xid);
    GETATTR3res res;
    res.tag = nfsstat3::NFS3ERR_JUKEBOX;
    XdrTrait<GETATTR3res>::serialize(ser, res);
  }

  RequestPermitVendor& vendor() {
    return vendor_;
  }

 private:
  RequestPermitVendor vendor_;
};

TEST_F(RpcServerTest, jukebox_rejects_non_exempt_inline) {
  auto server = createTestServerWithManualExecutor(
      std::make_shared<TestJukeboxProcessor>());
  auto clientFd = connectClient(*server);

  auto reply = sendAndReceive(
      clientFd,
      buildRpcRequest(100, /*proc=*/1),
      "JUKEBOX reply should arrive without the thread pool");

  EXPECT_EQ(readBigEndianU32(reply, 4), 100u);
  EXPECT_EQ(readBigEndianU32(reply, 8), 1u); // msg_type::REPLY
  EXPECT_EQ(readBigEndianU32(reply, 24), 0u); // accept_stat::SUCCESS

  // Verify the NFS-level JUKEBOX error in the response body.
  // Skip fragment header (4) + RPC reply envelope (24) = 28 bytes to reach
  // the NFS response. The first field is the nfsstat3 tag.
  ASSERT_GE(reply.size(), 32u) << "Reply too short for NFS status";
  uint32_t nfsStat = readBigEndianU32(reply, 28);
  EXPECT_EQ(nfsStat, static_cast<uint32_t>(nfsstat3::NFS3ERR_JUKEBOX));

  cleanup(clientFd, server);
}

TEST_F(RpcServerTest, jukebox_rejects_fsinfo_when_rate_limited) {
  auto server = createTestServerWithManualExecutor(
      std::make_shared<TestJukeboxProcessor>());
  auto clientFd = connectClient(*server);

  // FSINFO (proc=19) is now subject to JUKEBOX backpressure like any
  // other implemented proc.
  auto reply = sendAndReceive(
      clientFd,
      buildRpcRequest(200, /*proc=*/19),
      "FSINFO should be JUKEBOX-rejected when rate limited");

  EXPECT_EQ(readBigEndianU32(reply, 4), 200u);
  EXPECT_EQ(readBigEndianU32(reply, 8), 1u); // msg_type::REPLY
  EXPECT_EQ(readBigEndianU32(reply, 24), 0u); // accept_stat::SUCCESS

  // Verify the NFS-level JUKEBOX error in the response body.
  ASSERT_GE(reply.size(), 32u) << "Reply too short for NFS status";
  uint32_t nfsStat = readBigEndianU32(reply, 28);
  EXPECT_EQ(nfsStat, static_cast<uint32_t>(nfsstat3::NFS3ERR_JUKEBOX));

  cleanup(clientFd, server);
}

TEST_F(RpcServerTest, permit_exhausted_rejects_normal_proc) {
  auto proc = std::make_shared<TestPermitProcessor>(1);
  auto server = createTestServerWithManualExecutor(proc);
  auto clientFd = connectClient(*server);

  // Saturate the permit vendor — hold the only permit.
  auto held = proc->vendor().tryAcquirePermit();
  ASSERT_NE(held, nullptr);

  // Send a normal proc. With permits exhausted, it should be
  // JUKEBOX-rejected inline.
  auto reply = sendAndReceive(
      clientFd,
      buildRpcRequest(100, /*proc=*/1),
      "JUKEBOX reject should arrive inline when permits exhausted");

  EXPECT_EQ(readBigEndianU32(reply, 4), 100u); // xid
  EXPECT_EQ(readBigEndianU32(reply, 24), 0u); // accept_stat::SUCCESS
  ASSERT_GE(reply.size(), 32u);
  EXPECT_EQ(
      readBigEndianU32(reply, 28),
      static_cast<uint32_t>(nfsstat3::NFS3ERR_JUKEBOX));

  cleanup(clientFd, server);
}

TEST_F(RpcServerTest, permit_exhausted_still_fast_paths_null) {
  auto proc = std::make_shared<TestPermitProcessor>(1);
  auto server = createTestServerWithManualExecutor(proc);
  auto clientFd = connectClient(*server);

  // Saturate permits.
  auto held = proc->vendor().tryAcquirePermit();
  ASSERT_NE(held, nullptr);

  // Null RPCs bypass rate limiting — fast-pathed before the permit check.
  auto reply = sendAndReceive(
      clientFd,
      buildNullRpcRequest(101),
      "Null RPC should be fast-pathed even with permits exhausted");

  EXPECT_EQ(readBigEndianU32(reply, 4), 101u); // xid
  EXPECT_EQ(readBigEndianU32(reply, 8), 1u); // msg_type::REPLY
  EXPECT_EQ(readBigEndianU32(reply, 24), 0u); // accept_stat::SUCCESS
  // Reply should be short — just the RPC envelope, no NFS body.
  // Specifically, it should NOT contain NFS3ERR_JUKEBOX.
  EXPECT_LT(reply.size(), 32u)
      << "Null reply should be just the RPC envelope, not a JUKEBOX response";

  cleanup(clientFd, server);
}

TEST_F(RpcServerTest, permit_held_during_request_processing) {
  auto proc = std::make_shared<TestPermitProcessor>(1);
  auto server = createTestServerWithManualExecutor(proc);
  auto clientFd = connectClient(*server);

  // Send a normal proc. It should acquire the single permit and
  // dispatch to the ManualExecutor.
  sendRequest(clientFd, buildRpcRequest(200, /*proc=*/1));

  // The request is in the ManualExecutor queue. The permit should
  // still be held -- not released until the request completes.
  EXPECT_EQ(proc->vendor().available(), 0u)
      << "Permit should be held while request is in-flight";

  // A second request should be JUKEBOX-rejected because the permit
  // is held by the first request.
  auto reply = sendAndReceive(
      clientFd,
      buildRpcRequest(201, /*proc=*/1),
      "Second request should be JUKEBOX-rejected");

  EXPECT_EQ(readBigEndianU32(reply, 4), 201u);
  ASSERT_GE(reply.size(), 32u);
  EXPECT_EQ(
      readBigEndianU32(reply, 28),
      static_cast<uint32_t>(nfsstat3::NFS3ERR_JUKEBOX));

  // Crank the executor to complete the first request.
  for (int i = 0; i < 20; ++i) {
    manualExecutor_->run();
    evb.loopOnce(EVLOOP_NONBLOCK);
  }

  // Permit should now be released.
  EXPECT_EQ(proc->vendor().available(), 1u)
      << "Permit should be released after request completes";

  cleanup(clientFd, server);
}

TEST_F(RpcServerTest, permit_exhausted_still_fast_paths_unimplemented) {
  auto proc = std::make_shared<TestPermitProcessor>(1);
  auto server = createTestServerWithManualExecutor(proc);
  auto clientFd = connectClient(*server);

  // Saturate permits.
  auto held = proc->vendor().tryAcquirePermit();
  ASSERT_NE(held, nullptr);

  // Unimplemented procs bypass rate limiting — fast-pathed as PROC_UNAVAIL
  // before the permit check.
  auto reply = sendAndReceive(
      clientFd,
      buildRpcRequest(102, /*proc=*/99),
      "Unimplemented proc should be fast-pathed even with permits exhausted");

  EXPECT_EQ(readBigEndianU32(reply, 4), 102u); // xid
  EXPECT_EQ(readBigEndianU32(reply, 8), 1u); // msg_type::REPLY
  // accept_stat::PROC_UNAVAIL = 3
  EXPECT_EQ(readBigEndianU32(reply, 24), 3u);

  cleanup(clientFd, server);
}

class TestTimingProcessor : public RpcServerProcessor {
 public:
  bool shouldFastPathRPCs() const override {
    return false;
  }

  void onRequestComplete(const RpcRequestTimeline& t) override {
    lastTimeline_ = t;
    completedCount_++;
  }

  std::optional<RpcRequestTimeline> lastTimeline_;
  int completedCount_{0};
};

TEST_F(RpcServerTest, phase_timing_records_all_phases) {
  auto proc = std::make_shared<TestTimingProcessor>();
  auto server = createTestServerWithManualExecutor(proc);
  auto clientFd = connectClient(*server);

  // Send a null RPC through the full pipeline (shouldFastPathRPCs=false).
  sendRequest(clientFd, buildNullRpcRequest(99));

  // Crank the ManualExecutor and EventBase to complete the pipeline.
  for (int i = 0; i < 20; ++i) {
    manualExecutor_->run();
    evb.loopOnce(EVLOOP_NONBLOCK);
  }

  ASSERT_TRUE(pollForReply(clientFd, 1000))
      << "Reply should arrive after cranking the thread pool";
  readReply(clientFd);

  // Drive EventBase once more to ensure the WriteCallback has fired.
  evb.loopOnce(EVLOOP_NONBLOCK);

  // onRequestComplete should have been called exactly once.
  ASSERT_EQ(proc->completedCount_, 1);

  auto& t = *proc->lastTimeline_;

  // All five timestamps should be populated.
  ASSERT_TRUE(t.requestReceived.has_value()) << "requestReceived not set";
  ASSERT_TRUE(t.dispatched.has_value()) << "dispatched not set";
  ASSERT_TRUE(t.handlerStart.has_value()) << "handlerStart not set";
  ASSERT_TRUE(t.handlerDone.has_value()) << "handlerDone not set";
  ASSERT_TRUE(t.responseSent.has_value()) << "responseSent not set";

  // Timestamps should be in chronological order.
  EXPECT_LE(*t.requestReceived, *t.dispatched);
  EXPECT_LE(*t.dispatched, *t.handlerStart);
  EXPECT_LE(*t.handlerStart, *t.handlerDone);
  EXPECT_LE(*t.handlerDone, *t.responseSent);

  cleanup(clientFd, server);
}

/**
 * Records the parsed AUTH_SYS credentials that the RpcServer hands to
 * checkAuthentication and dispatchRpc.
 */
class CredsRecordingProcessor : public RpcServerProcessor {
 public:
  explicit CredsRecordingProcessor(bool parseCreds = true)
      : parseCreds_{parseCreds} {}

  bool shouldParseAuthSysCreds() override {
    return parseCreds_;
  }

  auth_stat checkAuthentication(
      const call_body& callBody,
      const std::optional<authsys_parms>& authSysCreds) override {
    checkAuthCreds_ = authSysCreds;
    return RpcServerProcessor::checkAuthentication(callBody, authSysCreds);
  }

  ImmediateFuture<folly::Unit> dispatchRpc(
      folly::io::Cursor /*deser*/,
      folly::io::QueueAppender ser,
      uint32_t xid,
      uint32_t /*progNumber*/,
      uint32_t /*progVersion*/,
      uint32_t /*procNumber*/,
      const std::optional<authsys_parms>& authSysCreds) override {
    dispatchCreds_ = authSysCreds;
    serializeReply(ser, accept_stat::SUCCESS, xid);
    return folly::unit;
  }

  std::optional<authsys_parms> checkAuthCreds_;
  std::optional<authsys_parms> dispatchCreds_;

 private:
  bool parseCreds_;
};

struct RpcServerCredsTest : RpcServerTest {
  /**
   * Send a request and crank the ManualExecutor and EventBase until the
   * reply arrives. Everything runs on the test thread, so the processor's
   * recorded credentials can be read without synchronization.
   */
  std::vector<uint8_t> sendAndCrank(
      int clientFd,
      std::unique_ptr<folly::IOBuf> request) {
    sendRequest(clientFd, std::move(request));
    for (int i = 0; i < 20; ++i) {
      manualExecutor_->run();
      evb.loopOnce(EVLOOP_NONBLOCK);
    }
    EXPECT_TRUE(pollForReply(clientFd, 1000)) << "Expected an RPC reply";
    return readReply(clientFd);
  }
};

TEST_F(RpcServerCredsTest, authsys_creds_passed_to_processor) {
  auto proc = std::make_shared<CredsRecordingProcessor>();
  auto server = createTestServerWithManualExecutor(proc);
  auto clientFd = connectClient(*server);

  authsys_parms creds{/*stamp=*/7, "testhost", /*uid=*/0, /*gid=*/0, {0, 20}};
  auto reply = sendAndCrank(
      clientFd, buildRpcRequestWithCred(1, 1, makeAuthSysCred(creds)));
  EXPECT_EQ(readBigEndianU32(reply, 24), 0u); // accept_stat::SUCCESS

  ASSERT_TRUE(proc->checkAuthCreds_.has_value());
  EXPECT_EQ(proc->checkAuthCreds_->uid, 0u);
  ASSERT_TRUE(proc->dispatchCreds_.has_value());
  EXPECT_EQ(proc->dispatchCreds_->uid, 0u);
  EXPECT_EQ(proc->dispatchCreds_->gid, 0u);
  EXPECT_EQ(proc->dispatchCreds_->machinename, "testhost");
  EXPECT_EQ(proc->dispatchCreds_->gids, (std::vector<uint32_t>{0, 20}));

  cleanup(clientFd, server);
}

TEST_F(RpcServerCredsTest, auth_none_yields_nullopt_creds) {
  auto proc = std::make_shared<CredsRecordingProcessor>();
  auto server = createTestServerWithManualExecutor(proc);
  auto clientFd = connectClient(*server);

  auto reply = sendAndCrank(clientFd, buildRpcRequest(2, 1));
  // Characterization: AUTH_NONE requests are still dispatched, the default
  // checkAuthentication accepts everything.
  EXPECT_EQ(readBigEndianU32(reply, 24), 0u); // accept_stat::SUCCESS
  EXPECT_EQ(proc->checkAuthCreds_, std::nullopt);
  EXPECT_EQ(proc->dispatchCreds_, std::nullopt);

  cleanup(clientFd, server);
}

TEST_F(RpcServerCredsTest, parse_skipped_when_processor_declines_creds) {
  auto proc = std::make_shared<CredsRecordingProcessor>(/*parseCreds=*/false);
  auto server = createTestServerWithManualExecutor(proc);
  auto clientFd = connectClient(*server);

  // Even though the request carries a valid AUTH_SYS credential, the
  // processor declined the parse, so both hooks must see nullopt.
  authsys_parms creds{/*stamp=*/7, "testhost", /*uid=*/0, /*gid=*/0, {0}};
  auto reply = sendAndCrank(
      clientFd, buildRpcRequestWithCred(4, 1, makeAuthSysCred(creds)));
  EXPECT_EQ(readBigEndianU32(reply, 24), 0u); // accept_stat::SUCCESS
  EXPECT_EQ(proc->checkAuthCreds_, std::nullopt);
  EXPECT_EQ(proc->dispatchCreds_, std::nullopt);

  cleanup(clientFd, server);
}

TEST_F(RpcServerCredsTest, malformed_authsys_creds_still_dispatched) {
  auto proc = std::make_shared<CredsRecordingProcessor>();
  auto server = createTestServerWithManualExecutor(proc);
  auto clientFd = connectClient(*server);

  // A 3-byte AUTH_SYS body cannot even hold the stamp field.
  opaque_auth truncated{auth_flavor::AUTH_SYS, {1, 2, 3}};
  auto reply = sendAndCrank(
      clientFd, buildRpcRequestWithCred(3, 1, std::move(truncated)));
  // Characterization: malformed credentials parse to nullopt and the request
  // is still dispatched rather than rejected with an auth error.
  EXPECT_EQ(readBigEndianU32(reply, 8), 1u); // msg_type::REPLY
  EXPECT_EQ(readBigEndianU32(reply, 24), 0u); // accept_stat::SUCCESS
  EXPECT_EQ(proc->dispatchCreds_, std::nullopt);

  cleanup(clientFd, server);
}

#endif // !_WIN32

} // namespace
