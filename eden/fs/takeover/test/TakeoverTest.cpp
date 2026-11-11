/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include <folly/Exception.h>
#include <folly/Portability.h>
#include <folly/ScopeGuard.h>
#include <folly/futures/Future.h>
#include <folly/futures/Promise.h>
#include <folly/io/IOBufQueue.h>
#include <folly/portability/GFlags.h>
#include <folly/test/TestUtils.h>
#include <folly/testing/TestUtil.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <poll.h>
#include <thrift/lib/cpp2/protocol/Serializer.h>

#include <eden/fs/takeover/gen-cpp2/takeover_types.h>
#include "eden/common/utils/FutureUnixSocket.h"
#include "eden/common/utils/Pipe.h"
#include "eden/fs/takeover/TakeoverClient.h"
#include "eden/fs/takeover/TakeoverData.h"
#include "eden/fs/takeover/TakeoverHandler.h"
#include "eden/fs/takeover/TakeoverServer.h"

DECLARE_int64(maximumChunkSize);

using namespace facebook::eden;
using folly::EventBase;
using folly::Future;
using folly::makeFuture;
using folly::Promise;
using folly::test::TemporaryDirectory;
using std::string;
using ::testing::ElementsAre;
using ::testing::ElementsAreArray;
using namespace std::chrono_literals;

namespace {
constexpr int64_t kTestChunkSize = 64 * 1024;

/**
 * A TakeoverHandler that returns the TakeoverData object passed to its
 * constructor.
 */
class TestHandler : public TakeoverHandler {
 public:
  explicit TestHandler(TakeoverData&& data) : data_{std::move(data)} {}

  Future<TakeoverData> startTakeoverShutdown(bool force, bool onlyIfIdle)
      override {
    force_ = force;
    onlyIfIdle_ = onlyIfIdle;
    return makeFuture(std::move(data_));
  }

  void closeStorage() override {}

  bool wasForced() const {
    return force_;
  }

  bool wasOnlyIfIdle() const {
    return onlyIfIdle_;
  }

 private:
  TakeoverData data_;
  bool force_ = false;
  bool onlyIfIdle_ = false;
};

/**
 * A TakeoverHandler that throws an exception.
 */
class ErrorHandler : public TakeoverHandler {
 public:
  Future<TakeoverData> startTakeoverShutdown(
      bool /*force*/,
      bool /*onlyIfIdle*/) override {
    return makeFuture<TakeoverData>(
        std::logic_error("purposely failing for testing"));
  }
  void closeStorage() override {}
};

/**
 * Run takeoverMounts() in a separate thread, and return a Future that will
 * complete in the specified EventBase once takeoverMounts() finishes.
 */
Future<TakeoverData> takeoverViaEventBase(
    EventBase* evb,
    AbsolutePathPiece socketPath,
    const std::set<int32_t>& supportedVersions,
    const uint64_t supportedCapabilities,
    bool shouldThrowDuringTakeover = false,
    bool shouldPing = true,
    bool force = false,
    bool onlyIfIdle = false) {
  Promise<TakeoverData> promise;
  auto future = promise.getFuture();
  std::thread thread([path = AbsolutePath{socketPath},
                      supportedVersions = supportedVersions,
                      supportedCapabilities = supportedCapabilities,
                      shouldThrowDuringTakeover = shouldThrowDuringTakeover,
                      shouldPing = shouldPing,
                      force = force,
                      onlyIfIdle = onlyIfIdle,
                      promise = std::move(promise)]() mutable {
    promise.setWith([&] {
      return takeoverMounts(
          path,
          /*takeoverReceiveTimeout*/ std::chrono::seconds(150),
          force,
          onlyIfIdle,
          shouldThrowDuringTakeover,
          shouldPing,
          supportedVersions,
          supportedCapabilities);
    });
  });

  return future.via(evb).ensure(
      [t = std::move(thread)]() mutable { t.join(); });
}

/**
 * A helper class to terminate the EventBase loop if the test runs
 * for longer than we expect.  This ensures the test won't run forever if
 * something goes wrong.
 *
 * This is used by the loopWithTimeout() helper function below.
 */
class TestTimeout : public folly::AsyncTimeout {
 public:
  explicit TestTimeout(EventBase* evb)
      : folly::AsyncTimeout{evb}, eventBase_{evb} {}

  void timeoutExpired() noexcept override {
    ADD_FAILURE() << "test timeout expired";
    eventBase_->terminateLoopSoon();
  }

 private:
  EventBase* eventBase_{nullptr};
};

void loopWithTimeout(EventBase* evb, std::chrono::milliseconds timeout = 300s) {
  TestTimeout timeoutObject{evb};
  timeoutObject.scheduleTimeout(timeout);
  evb->loop();
}

/**
 * Create a TakeoverServer using the specified TakeoverHandler, then call
 * takeoverMounts() to receive the TakeoverData from it.  Returns a
 * Try<TakeoverData> with the result.
 */
folly::Try<TakeoverData> runTakeover(
    const TemporaryDirectory& tmpDir,
    TakeoverHandler* handler,
    const std::set<int32_t>& clientSupportedVersions =
        kSupportedTakeoverVersions,
    const std::set<int32_t>& serverSupportedVersions =
        kSupportedTakeoverVersions,
    const uint64_t clientSupportedCapabilities = kSupportedCapabilities,
    const uint64_t serverSupportedCapabilities = kSupportedCapabilities,
    bool force = false,
    bool onlyIfIdle = false) {
  // Ignore SIGPIPE so that sendmsg() will fail with an error code instead
  // of terminating the program if the remote side has closed the connection.
  signal(SIGPIPE, SIG_IGN);

  AbsolutePath socketPath =
      canonicalPath(tmpDir.path().string()) + "takeover"_pc;
  EventBase evb;

  FaultInjector faultInjector{/*enabled=*/false};
  TakeoverServer server(
      &evb,
      socketPath,
      handler,
      &faultInjector,
      serverSupportedVersions,
      serverSupportedCapabilities);

  auto future = takeoverViaEventBase(
                    &evb,
                    socketPath,
                    clientSupportedVersions,
                    clientSupportedCapabilities,
                    /*shouldThrowDuringTakeover=*/false,
                    /*shouldPing=*/true,
                    force,
                    onlyIfIdle)
                    .ensure([&] { evb.terminateLoopSoon(); });
  loopWithTimeout(&evb);
  if (!future.isReady()) {
    // This should generally only happen if we timed out.
    throw std::runtime_error("future is not ready");
  }
  return std::move(future).result();
}

void checkExpectedFile(int fd, AbsolutePathPiece path) {
  struct stat fdStat{};
  struct stat pathStat{};
  auto rc = fstat(fd, &fdStat);
  folly::checkUnixError(rc, "fstat failed");
  rc = stat(path.asString().c_str(), &pathStat);
  folly::checkUnixError(rc, "stat failed");

  EXPECT_EQ(fdStat.st_dev, pathStat.st_dev);
  EXPECT_EQ(fdStat.st_ino, pathStat.st_ino);
}
} // namespace

TEST(Takeover, requiredCapabilities) {
  EXPECT_EQ(
      TakeoverData::computeCompatibleCapabilities(
          kSupportedCapabilities, kSupportedCapabilities),
      kSupportedCapabilities);
  EXPECT_THROW(
      TakeoverData::computeCompatibleCapabilities(
          kSupportedCapabilities & ~TakeoverCapabilities::NFS,
          kSupportedCapabilities),
      std::runtime_error);
}

TEST(Takeover, rejectsLegacyFuseConnectionInfoSize) {
  SerializedMountInfo serializedMount;
  serializedMount.mountPath() = "/mount";
  serializedMount.stateDirectory() = "/state";
  serializedMount.connInfo() = std::string(FUSE_COMPAT_22_INIT_OUT_SIZE, '\0');
  serializedMount.inodeMap() = SerializedInodeMap{};
  serializedMount.mountProtocol() = TakeoverMountProtocol::FUSE;

  SerializedTakeoverInfo serializedInfo;
  serializedInfo.mounts() = {std::move(serializedMount)};
  SerializedTakeoverResult serializedResult;
  serializedResult.takeoverData() = std::move(serializedInfo);

  folly::IOBufQueue buffer;
  folly::io::QueueAppender appender(&buffer, 0);
  appender.writeBE<uint32_t>(TakeoverData::kTakeoverProtocolVersionSeven);
  appender.writeBE<uint32_t>(sizeof(uint64_t));
  appender.writeBE<uint64_t>(kSupportedCapabilities);
  apache::thrift::CompactSerializer::serialize(serializedResult, &buffer);

  auto serialized = buffer.move();
  serialized->coalesce();
  UnixSocket::Message message{std::move(*serialized)};
  EXPECT_THROW_RE(
      TakeoverData::deserialize(message),
      std::runtime_error,
      "invalid FUSE connection info size");
}

TEST(Takeover, simple) {
  TemporaryDirectory tmpDir("eden_takeover_test");
  AbsolutePath tmpDirPath = canonicalPath(tmpDir.path().string());

  // Build the TakeoverData object to send
  TakeoverData serverData;

  auto lockFilePath = tmpDirPath + "lock"_pc;
  serverData.lockFile = folly::File{lockFilePath.view(), O_RDWR | O_CREAT};

  auto thriftSocketPath = tmpDirPath + "thrift"_pc;
  serverData.thriftSocket =
      folly::File{thriftSocketPath.view(), O_RDWR | O_CREAT};

  auto mountdSocketPath = tmpDirPath + "mountd"_pc;
  serverData.mountdServerSocket =
      folly::File{mountdSocketPath.view(), O_RDWR | O_CREAT};

  auto mount1Path = tmpDirPath + "mount1"_pc;
  auto client1Path = tmpDirPath + "client1"_pc;
  auto mount1FusePath = tmpDirPath + "fuse1"_pc;
  serverData.mountPoints.emplace_back(
      mount1Path,
      client1Path,
      FuseChannelData{
          folly::File{mount1FusePath.view(), O_RDWR | O_CREAT},
          fuse_init_out{}},
      SerializedInodeMap{});

  auto mount2Path = tmpDirPath + "mount2"_pc;
  auto client2Path = tmpDirPath + "client2"_pc;
  auto mount2FusePath = tmpDirPath + "fuse2"_pc;
  serverData.mountPoints.emplace_back(
      mount2Path,
      client2Path,
      FuseChannelData{
          folly::File{mount2FusePath.view(), O_RDWR | O_CREAT},
          fuse_init_out{}},
      SerializedInodeMap{});

  // Perform the takeover
  auto serverSendFuture = serverData.takeoverComplete.getFuture();
  TestHandler handler{std::move(serverData)};
  auto result = runTakeover(tmpDir, &handler);
  ASSERT_TRUE(serverSendFuture.hasValue());
  EXPECT_TRUE(result.hasValue());
  const auto& clientData = result.value();

  // Make sure the received lock file refers to the expected file.
  checkExpectedFile(clientData.lockFile.fd(), lockFilePath);
  // And the thrift socket FD
  checkExpectedFile(clientData.thriftSocket.fd(), thriftSocketPath);
  checkExpectedFile(clientData.mountdServerSocket->fd(), mountdSocketPath);

  // Make sure the received mount information is correct
  ASSERT_EQ(2, clientData.mountPoints.size());
  EXPECT_EQ(mount1Path, clientData.mountPoints.at(0).mountPath);
  EXPECT_EQ(client1Path, clientData.mountPoints.at(0).stateDirectory);
  auto& fuseChannelData0 =
      std::get<FuseChannelData>(clientData.mountPoints.at(0).channelInfo);
  checkExpectedFile(fuseChannelData0.fd.fd(), mount1FusePath);

  EXPECT_EQ(mount2Path, clientData.mountPoints.at(1).mountPath);
  EXPECT_EQ(client2Path, clientData.mountPoints.at(1).stateDirectory);
  auto& fuseChannelData1 =
      std::get<FuseChannelData>(clientData.mountPoints.at(1).channelInfo);
  checkExpectedFile(fuseChannelData1.fd.fd(), mount2FusePath);
}

TEST(Takeover, noMounts) {
  TemporaryDirectory tmpDir("eden_takeover_test");
  AbsolutePath tmpDirPath = canonicalPath(tmpDir.path().string());

  // Build the TakeoverData object with no mount points
  TakeoverData serverData;
  auto lockFilePath = tmpDirPath + "lock"_pc;
  serverData.lockFile = folly::File{lockFilePath.view(), O_RDWR | O_CREAT};
  auto thriftSocketPath = tmpDirPath + "thrift"_pc;
  serverData.thriftSocket =
      folly::File{thriftSocketPath.view(), O_RDWR | O_CREAT};
  auto mountdSocketPath = tmpDirPath + "mountd"_pc;
  serverData.mountdServerSocket =
      folly::File{mountdSocketPath.view(), O_RDWR | O_CREAT};

  // Perform the takeover
  auto serverSendFuture = serverData.takeoverComplete.getFuture();
  TestHandler handler{std::move(serverData)};
  auto result = runTakeover(tmpDir, &handler);
  ASSERT_TRUE(serverSendFuture.hasValue());
  EXPECT_TRUE(result.hasValue());
  const auto& clientData = result.value();

  // Make sure the received lock file and thrift socket FD refer to the
  // expected files.
  checkExpectedFile(clientData.lockFile.fd(), lockFilePath);
  checkExpectedFile(clientData.thriftSocket.fd(), thriftSocketPath);
  checkExpectedFile(clientData.mountdServerSocket->fd(), mountdSocketPath);

  // Make sure the received mount information is empty
  EXPECT_EQ(0, clientData.mountPoints.size());
  EXPECT_FALSE(handler.wasForced());
}

TEST(Takeover, forcedTakeoverReachesHandler) {
  TemporaryDirectory tmpDir("eden_takeover_test");
  AbsolutePath tmpDirPath = canonicalPath(tmpDir.path().string());

  TakeoverData serverData;
  serverData.lockFile =
      folly::File{(tmpDirPath + "lock"_pc).view(), O_RDWR | O_CREAT};
  serverData.thriftSocket =
      folly::File{(tmpDirPath + "thrift"_pc).view(), O_RDWR | O_CREAT};
  serverData.mountdServerSocket =
      folly::File{(tmpDirPath + "mountd"_pc).view(), O_RDWR | O_CREAT};

  TestHandler handler{std::move(serverData)};
  auto result = runTakeover(
      tmpDir,
      &handler,
      kSupportedTakeoverVersions,
      kSupportedTakeoverVersions,
      kSupportedCapabilities,
      kSupportedCapabilities,
      /*force=*/true);
  EXPECT_TRUE(result.hasValue());
  EXPECT_TRUE(handler.wasForced());
  EXPECT_FALSE(handler.wasOnlyIfIdle());
}

TEST(Takeover, onlyIfIdleReachesHandler) {
  TemporaryDirectory tmpDir("eden_takeover_test");
  AbsolutePath tmpDirPath = canonicalPath(tmpDir.path().string());

  TakeoverData serverData;
  serverData.lockFile =
      folly::File{(tmpDirPath + "lock"_pc).view(), O_RDWR | O_CREAT};
  serverData.thriftSocket =
      folly::File{(tmpDirPath + "thrift"_pc).view(), O_RDWR | O_CREAT};
  serverData.mountdServerSocket =
      folly::File{(tmpDirPath + "mountd"_pc).view(), O_RDWR | O_CREAT};

  TestHandler handler{std::move(serverData)};
  auto result = runTakeover(
      tmpDir,
      &handler,
      kSupportedTakeoverVersions,
      kSupportedTakeoverVersions,
      kSupportedCapabilities,
      kSupportedCapabilities,
      /*force=*/false,
      /*onlyIfIdle=*/true);
  EXPECT_TRUE(result.hasValue());
  EXPECT_FALSE(handler.wasForced());
  EXPECT_TRUE(handler.wasOnlyIfIdle());
}

TEST(Takeover, manyMounts) {
  TemporaryDirectory tmpDir("eden_takeover_test");
  AbsolutePath tmpDirPath = canonicalPath(tmpDir.path().string());

  // Build the TakeoverData object
  TakeoverData serverData;
  auto lockFilePath = tmpDirPath + "lock"_pc;
  serverData.lockFile = folly::File{lockFilePath.view(), O_RDWR | O_CREAT};
  auto thriftSocketPath = tmpDirPath + "thrift"_pc;
  serverData.thriftSocket =
      folly::File{thriftSocketPath.view(), O_RDWR | O_CREAT};
  auto mountdSocketPath = tmpDirPath + "mountd"_pc;
  serverData.mountdServerSocket =
      folly::File{mountdSocketPath.view(), O_RDWR | O_CREAT};

  // Build info for 10,000 mounts
  // This exercises the code where we send more FDs than ControlMsg::kMaxFDs.
  //
  // Note that for this test to succeed your "ulimit -n" settings must be at
  // least twice this number.  We will end up with 2 FDs for each mount, since
  // we open one on the "server" side, and then the client receives a copy of
  // each FD.
  constexpr size_t numMounts = 10000;
  for (size_t n = 0; n < numMounts; ++n) {
    auto mountPath =
        tmpDirPath + RelativePathPiece{folly::to<string>("mounts/foo/test", n)};
    auto stateDirectory =
        tmpDirPath + RelativePathPiece{folly::to<string>("client", n)};
    auto fusePath =
        tmpDirPath + PathComponentPiece{folly::to<string>("fuse", n)};
    serverData.mountPoints.emplace_back(
        mountPath,
        stateDirectory,
        FuseChannelData{
            folly::File{fusePath.view(), O_RDWR | O_CREAT}, fuse_init_out{}},
        SerializedInodeMap{});
  }

  // Perform the takeover
  auto serverSendFuture = serverData.takeoverComplete.getFuture();
  TestHandler handler{std::move(serverData)};
  auto result = runTakeover(tmpDir, &handler);
  ASSERT_TRUE(serverSendFuture.hasValue());
  EXPECT_TRUE(result.hasValue());
  const auto& clientData = result.value();

  // Make sure the received lock file and thrift socket FDs are correct
  checkExpectedFile(clientData.lockFile.fd(), lockFilePath);
  checkExpectedFile(clientData.thriftSocket.fd(), thriftSocketPath);
  checkExpectedFile(clientData.mountdServerSocket->fd(), mountdSocketPath);

  // Make sure the received mount information is correct
  ASSERT_EQ(numMounts, clientData.mountPoints.size());
  for (size_t n = 0; n < numMounts; ++n) {
    const auto& mountInfo = clientData.mountPoints[n];
    auto expectedMountPath =
        tmpDirPath + RelativePathPiece{folly::to<string>("mounts/foo/test", n)};
    EXPECT_EQ(expectedMountPath, mountInfo.mountPath);

    auto expectedClientPath =
        tmpDirPath + RelativePathPiece{folly::to<string>("client", n)};
    EXPECT_EQ(expectedClientPath, mountInfo.stateDirectory);

    auto expectedFusePath =
        tmpDirPath + PathComponentPiece{folly::to<string>("fuse", n)};
    auto& fuseChannelData = std::get<FuseChannelData>(mountInfo.channelInfo);
    checkExpectedFile(fuseChannelData.fd.fd(), expectedFusePath);
  }
}

TEST(Takeover, chunksSerializedInodeMap) {
  const auto originalChunkSize = FLAGS_maximumChunkSize;
  SCOPE_EXIT {
    FLAGS_maximumChunkSize = originalChunkSize;
  };
  FLAGS_maximumChunkSize = kTestChunkSize;

  SerializedInodeMap inodeMap;
  constexpr int64_t numInodes = 1024;
  for (int64_t i = 0; i < numInodes; ++i) {
    SerializedInodeMapEntry entry;
    entry.inodeNumber() = i;
    entry.name() = folly::to<string>(
        "example_inode_name______________choose_a_big_name______________", i);
    entry.hash() = folly::to<string>(
        "example_inode_hash______________choose_a_big_hash______________", i);
    inodeMap.unloadedInodes()->emplace_back(std::move(entry));
  }
  TakeoverData data;
  data.mountPoints.emplace_back(
      canonicalPath("/mount"),
      canonicalPath("/state"),
      FuseChannelData{folly::File{}, fuse_init_out{}},
      std::move(inodeMap));

  UnixSocket::Message message;
  data.serialize(kSupportedCapabilities, message);
  EXPECT_GT(message.data.countChainElements(), 1);
  EXPECT_GT(message.data.computeChainDataLength(), kTestChunkSize);
  const auto* chunk = &message.data;
  do {
    EXPECT_LE(chunk->length(), kTestChunkSize);
    chunk = chunk->next();
  } while (chunk != &message.data);
}

TEST(Takeover, manyInodes) {
  const auto originalChunkSize = FLAGS_maximumChunkSize;
  SCOPE_EXIT {
    if (folly::kIsSanitizeThread) {
      FLAGS_maximumChunkSize = originalChunkSize;
    }
  };
  if (folly::kIsSanitizeThread) {
    FLAGS_maximumChunkSize = kTestChunkSize;
  }

  TemporaryDirectory tmpDir("eden_takeover_test");
  AbsolutePath tmpDirPath = canonicalPath(tmpDir.path().string());

  // Build the TakeoverData object
  TakeoverData serverData;
  auto lockFilePath = tmpDirPath + "lock"_pc;
  serverData.lockFile = folly::File{lockFilePath.view(), O_RDWR | O_CREAT};
  auto thriftSocketPath = tmpDirPath + "thrift"_pc;
  serverData.thriftSocket =
      folly::File{thriftSocketPath.view(), O_RDWR | O_CREAT};
  auto mountdSocketPath = tmpDirPath + "mountd"_pc;
  serverData.mountdServerSocket =
      folly::File{mountdSocketPath.view(), O_RDWR | O_CREAT};

  // The non-TSan payload exceeds UnixSocket's 1GB message limit. TSan uses a
  // smaller payload and chunk cap to cover the same multi-message handoff.
  constexpr int64_t numInodes = folly::kIsSanitizeThread ? 4096 : 7000000;

  auto mountPath =
      tmpDirPath + RelativePathPiece{folly::to<string>("mounts/foo/test")};
  auto stateDirectory =
      tmpDirPath + RelativePathPiece{folly::to<string>("client")};
  auto fusePath = tmpDirPath + PathComponentPiece{folly::to<string>("fuse")};
  SerializedInodeMap inodeMap;
  inodeMap.unloadedInodes()->reserve(numInodes);
  for (int64_t i = 0; i < numInodes; ++i) {
    SerializedInodeMapEntry entry;
    entry.inodeNumber() = i;
    entry.parentInode() = 0;
    // The long name and hash make the non-TSan message larger than 1 GB.
    entry.name() = folly::to<string>(
        "example_inode_name______________choose_a_big_name______________", i);
    entry.isUnlinked() = false;
    entry.numFsReferences() = 1;
    entry.hash() = folly::to<string>(
        "example_inode_hash______________choose_a_big_hash______________", i);
    entry.mode() = 0644;
    inodeMap.unloadedInodes()->emplace_back(std::move(entry));
  }
  serverData.mountPoints.emplace_back(
      mountPath,
      stateDirectory,
      FuseChannelData{
          folly::File{fusePath.view(), O_RDWR | O_CREAT}, fuse_init_out{}},
      std::move(inodeMap));

  // Perform the takeover
  auto serverSendFuture = serverData.takeoverComplete.getFuture();
  TestHandler handler{std::move(serverData)};
  auto result = runTakeover(tmpDir, &handler);
  ASSERT_TRUE(serverSendFuture.hasValue());
  ASSERT_FALSE(serverSendFuture.value().has_value());
  ASSERT_TRUE(result.hasValue());
  const auto& clientData = result.value();

  // Make sure the received lock file and thrift socket FDs are correct
  checkExpectedFile(clientData.lockFile.fd(), lockFilePath);
  checkExpectedFile(clientData.thriftSocket.fd(), thriftSocketPath);
  checkExpectedFile(clientData.mountdServerSocket->fd(), mountdSocketPath);

  // Make sure the received mount information is only for one mount
  ASSERT_EQ(1, clientData.mountPoints.size());

  // Make sure the received mount information is correct
  const auto& mountInfo = clientData.mountPoints[0];
  auto expectedMountPath =
      tmpDirPath + RelativePathPiece{folly::to<string>("mounts/foo/test")};
  EXPECT_EQ(expectedMountPath, mountInfo.mountPath);

  auto expectedClientPath =
      tmpDirPath + RelativePathPiece{folly::to<string>("client")};
  EXPECT_EQ(expectedClientPath, mountInfo.stateDirectory);
  auto expectedFusePath =
      tmpDirPath + PathComponentPiece{folly::to<string>("fuse")};
  auto& fuseChannelData = std::get<FuseChannelData>(mountInfo.channelInfo);
  checkExpectedFile(fuseChannelData.fd.fd(), expectedFusePath);
  const auto& unloadedInodes = *mountInfo.inodeMap.unloadedInodes();
  ASSERT_EQ(numInodes, unloadedInodes.size());
  const auto checkInode = [&](int64_t i) {
    const auto& entry = unloadedInodes[i];
    EXPECT_EQ(i, *entry.inodeNumber());
    EXPECT_EQ(0, *entry.parentInode());
    EXPECT_EQ(
        folly::to<string>(
            "example_inode_name______________choose_a_big_name______________",
            i),
        *entry.name());
    EXPECT_FALSE(*entry.isUnlinked());
    EXPECT_EQ(1, *entry.numFsReferences());
    ASSERT_TRUE(entry.hash().has_value());
    EXPECT_EQ(
        folly::to<string>(
            "example_inode_hash______________choose_a_big_hash______________",
            i),
        *entry.hash());
    EXPECT_EQ(0644, *entry.mode());
  };
  if (folly::kIsSanitizeThread) {
    for (int64_t i = 0; i < numInodes; ++i) {
      checkInode(i);
    }
  } else {
    checkInode(0);
    checkInode(numInodes - 1);
  }
}

TEST(Takeover, error) {
  TemporaryDirectory tmpDir("eden_takeover_test");
  ErrorHandler handler;
  auto result = runTakeover(tmpDir, &handler);
  EXPECT_THROW_RE(
      result.value(),
      std::runtime_error,
      "logic_error: purposely failing for testing");
}

TEST(Takeover, sendFailureReturnsTakeoverDataForRecovery) {
  // The server intentionally sends takeover data to a client that has already
  // disconnected. Ignore SIGPIPE so the failed send is surfaced as a normal
  // socket error that the test can assert on, rather than terminating the
  // process.
  signal(SIGPIPE, SIG_IGN);

  TemporaryDirectory tmpDir("eden_takeover_test");
  AbsolutePath tmpDirPath = canonicalPath(tmpDir.path().string());
  AbsolutePath socketPath = tmpDirPath + "takeover"_pc;

  EventBase evb;
  FaultInjector faultInjector{/*enabled=*/true};
  // Pause the server after it sends the readiness ping so the client can reply
  // and then exit before takeover data transfer begins.
  faultInjector.injectBlock("takeover", "ping_receive");

  // Build a minimal takeover payload with the FDs the old daemon would need in
  // order to recover if the handoff fails.
  TakeoverData serverData;

  auto lockFilePath = tmpDirPath + "lock"_pc;
  serverData.lockFile = folly::File{lockFilePath.view(), O_RDWR | O_CREAT};

  auto thriftSocketPath = tmpDirPath + "thrift"_pc;
  serverData.thriftSocket =
      folly::File{thriftSocketPath.view(), O_RDWR | O_CREAT};

  auto mountdSocketPath = tmpDirPath + "mountd"_pc;
  serverData.mountdServerSocket =
      folly::File{mountdSocketPath.view(), O_RDWR | O_CREAT};

  auto mountPath =
      tmpDirPath + RelativePathPiece{folly::to<string>("mounts/foo/test")};
  auto clientPath = tmpDirPath + RelativePathPiece{folly::to<string>("client")};
  auto fusePath = tmpDirPath + PathComponentPiece{folly::to<string>("fuse")};
  serverData.mountPoints.emplace_back(
      mountPath,
      clientPath,
      FuseChannelData{
          folly::File{fusePath.view(), O_RDWR | O_CREAT}, fuse_init_out{}},
      SerializedInodeMap{});

  auto serverSendFuture = serverData.takeoverComplete.getFuture();
  TestHandler handler{std::move(serverData)};
  TakeoverServer server(
      &evb,
      socketPath,
      &handler,
      &faultInjector,
      kSupportedTakeoverVersions,
      kSupportedCapabilities);

  // Have the new process reply to the readiness ping and then fail before it
  // starts receiving takeover data.
  auto clientFuture = takeoverViaEventBase(
                          &evb,
                          socketPath,
                          kSupportedTakeoverVersions,
                          kSupportedCapabilities,
                          /*shouldThrowDuringTakeover=*/true)
                          .ensure([&] { evb.terminateLoopSoon(); });
  loopWithTimeout(&evb);
  // The client-side failure should have completed before we inspect it.
  ASSERT_TRUE(clientFuture.isReady());

  EXPECT_THROW_RE(
      std::move(clientFuture).get(),
      std::runtime_error,
      "simulated takeover error");

  // Wait until the server is blocked at the injected pause before letting it
  // continue into the send path.
  ASSERT_TRUE(faultInjector.waitUntilBlocked("takeover", 1s));

  faultInjector.removeFault("takeover", "ping_receive");
  faultInjector.unblock("takeover", "ping_receive");

  // Now the old daemon attempts to send takeover data to a client that has
  // already gone away.  takeoverComplete should return the original
  // TakeoverData for recovery rather than surfacing the send error.
  auto serverResultFuture = std::move(serverSendFuture).via(&evb).ensure([&] {
    evb.terminateLoopSoon();
  });
  loopWithTimeout(&evb);
  // The event loop should only exit once the server has resolved
  // takeoverComplete with its recovery result.
  ASSERT_TRUE(serverResultFuture.isReady());

  auto serverResult = std::move(serverResultFuture).getTry();
  ASSERT_TRUE(serverResult.hasValue());
  ASSERT_TRUE(serverResult->has_value());

  auto recoveryData = std::move(serverResult).value().value();
  // Check that the old daemon got back the metadata it needs to rebuild the
  // mount and that the serialized mount list itself is intact.
  ASSERT_TRUE(recoveryData.mountdServerSocket.has_value());
  ASSERT_EQ(recoveryData.mountPoints.size(), 1);
  EXPECT_EQ(recoveryData.mountPoints[0].mountPath, mountPath);
  EXPECT_EQ(recoveryData.mountPoints[0].stateDirectory, clientPath);

  // The recovery payload must also preserve the original FDs rather than
  // leaving them moved into the failed send attempt.
  checkExpectedFile(recoveryData.lockFile.fd(), lockFilePath);
  checkExpectedFile(recoveryData.thriftSocket.fd(), thriftSocketPath);
  checkExpectedFile(recoveryData.mountdServerSocket->fd(), mountdSocketPath);

  auto* fuseData =
      std::get_if<FuseChannelData>(&recoveryData.mountPoints[0].channelInfo);
  ASSERT_NE(fuseData, nullptr);
  checkExpectedFile(fuseData->fd.fd(), fusePath);
}

TEST(Takeover, computeCompatibleVersion) {
  const std::set<int32_t> noVersions;
  const std::set<int32_t> oneVersion{1};
  const std::set<int32_t> newVersion{1, 2};
  const std::set<int32_t> newerVersion{2, 3};
  const std::set<int32_t> newestVersion{3, 4};
  const std::set<int32_t> laundryList{1, 2, 3, 4};

  // Check that computeCompatibleVersion is doing the right things.
  EXPECT_EQ(
      TakeoverData::computeCompatibleVersion(noVersions, oneVersion),
      std::nullopt);

  EXPECT_EQ(
      TakeoverData::computeCompatibleVersion(oneVersion, oneVersion).value(),
      1);

  EXPECT_EQ(
      TakeoverData::computeCompatibleVersion(oneVersion, newVersion).value(),
      1);

  EXPECT_EQ(
      TakeoverData::computeCompatibleVersion(newVersion, newerVersion).value(),
      2);

  EXPECT_EQ(
      TakeoverData::computeCompatibleVersion(newerVersion, newestVersion)
          .value(),
      3);

  EXPECT_EQ(
      TakeoverData::computeCompatibleVersion(newVersion, newestVersion),
      std::nullopt);

  EXPECT_EQ(
      TakeoverData::computeCompatibleVersion(newestVersion, laundryList)
          .value(),
      4);

  // Try it with the parameters flipped; we should still have the
  // same output.
  EXPECT_EQ(
      TakeoverData::computeCompatibleVersion(laundryList, newestVersion)
          .value(),
      4);
}

TEST(Takeover, errorVersionMismatch) {
  TemporaryDirectory tmpDir("eden_takeover_test");
  ErrorHandler handler;
  auto result = runTakeover(
      tmpDir,
      &handler,
      std::set<int32_t>{TakeoverData::kTakeoverProtocolVersionNeverSupported},
      kSupportedTakeoverVersions,
      0,
      kSupportedCapabilities);
  EXPECT_THROW_RE(
      result.value(),
      std::runtime_error,
      "The client and the server do not share a common "
      "takeover protocol implementation.");
}

TEST(Takeover, nfs) {
  TemporaryDirectory tmpDir("eden_takeover_test");
  AbsolutePath tmpDirPath = canonicalPath(tmpDir.path().string());

  // Build the TakeoverData object to send
  TakeoverData serverData;

  auto lockFilePath = tmpDirPath + "lock"_pc;
  serverData.lockFile = folly::File{lockFilePath.view(), O_RDWR | O_CREAT};

  auto thriftSocketPath = tmpDirPath + "thrift"_pc;
  serverData.thriftSocket =
      folly::File{thriftSocketPath.view(), O_RDWR | O_CREAT};

  auto mountdSocketPath = tmpDirPath + "mountd"_pc;
  serverData.mountdServerSocket =
      folly::File{mountdSocketPath.view(), O_RDWR | O_CREAT};

  auto mount1Path = tmpDirPath + "mount1"_pc;
  auto client1Path = tmpDirPath + "client1"_pc;
  auto mount1FusePath = tmpDirPath + "fuse1"_pc;
  serverData.mountPoints.emplace_back(
      mount1Path,
      client1Path,
      FuseChannelData{
          folly::File{mount1FusePath.view(), O_RDWR | O_CREAT},
          fuse_init_out{}},
      SerializedInodeMap{});

  auto mount2Path = tmpDirPath + "mount2"_pc;
  auto client2Path = tmpDirPath + "client2"_pc;
  auto mount2NfsPath = tmpDirPath + "nfs"_pc;
  serverData.mountPoints.emplace_back(
      mount2Path,
      client2Path,
      NfsChannelData{folly::File{mount2NfsPath.view(), O_RDWR | O_CREAT}},
      SerializedInodeMap{});

  // Perform the takeover
  auto serverSendFuture = serverData.takeoverComplete.getFuture();
  TestHandler handler{std::move(serverData)};
  auto result = runTakeover(tmpDir, &handler);
  ASSERT_TRUE(serverSendFuture.hasValue());
  EXPECT_TRUE(result.hasValue());
  const auto& clientData = result.value();

  // Make sure the received lock file refers to the expected file.
  checkExpectedFile(clientData.lockFile.fd(), lockFilePath);
  // And the thrift socket FD
  checkExpectedFile(clientData.thriftSocket.fd(), thriftSocketPath);
  checkExpectedFile(clientData.mountdServerSocket->fd(), mountdSocketPath);

  // Make sure the received mount information is correct
  ASSERT_EQ(2, clientData.mountPoints.size());
  EXPECT_EQ(mount1Path, clientData.mountPoints.at(0).mountPath);
  EXPECT_EQ(client1Path, clientData.mountPoints.at(0).stateDirectory);
  auto& fuseChannelData =
      std::get<FuseChannelData>(clientData.mountPoints.at(0).channelInfo);
  checkExpectedFile(fuseChannelData.fd.fd(), mount1FusePath);

  EXPECT_EQ(mount2Path, clientData.mountPoints.at(1).mountPath);
  EXPECT_EQ(client2Path, clientData.mountPoints.at(1).stateDirectory);
  auto& nfsChannelData =
      std::get<NfsChannelData>(clientData.mountPoints.at(1).channelInfo);
  checkExpectedFile(nfsChannelData.nfsdSocketFd.fd(), mount2NfsPath);
}

TEST(Takeover, mixedupFdOrder) {
  TemporaryDirectory tmpDir("eden_takeover_test");
  AbsolutePath tmpDirPath = canonicalPath(tmpDir.path().string());

  // Build the TakeoverData object to send
  TakeoverData serverData;
  serverData.injectedFdOrderForTesting = std::vector<FileDescriptorType>{
      FileDescriptorType::MOUNTD_SOCKET,
      FileDescriptorType::LOCK_FILE,
      FileDescriptorType::THRIFT_SOCKET};

  auto lockFilePath = tmpDirPath + "lock"_pc;
  serverData.lockFile = folly::File{lockFilePath.view(), O_RDWR | O_CREAT};

  auto thriftSocketPath = tmpDirPath + "thrift"_pc;
  serverData.thriftSocket =
      folly::File{thriftSocketPath.view(), O_RDWR | O_CREAT};

  auto mountdSocketPath = tmpDirPath + "mountd"_pc;
  serverData.mountdServerSocket =
      folly::File{mountdSocketPath.view(), O_RDWR | O_CREAT};

  auto mount1Path = tmpDirPath + "mount1"_pc;
  auto client1Path = tmpDirPath + "client1"_pc;
  auto mount1FusePath = tmpDirPath + "fuse1"_pc;
  serverData.mountPoints.emplace_back(
      mount1Path,
      client1Path,
      FuseChannelData{
          folly::File{mount1FusePath.view(), O_RDWR | O_CREAT},
          fuse_init_out{}},
      SerializedInodeMap{});

  // Perform the takeover
  auto serverSendFuture = serverData.takeoverComplete.getFuture();
  TestHandler handler{std::move(serverData)};
  auto result = runTakeover(tmpDir, &handler);
  ASSERT_TRUE(serverSendFuture.hasValue());
  EXPECT_TRUE(result.hasValue());
  const auto& clientData = result.value();

  // Make sure the received lock file refers to the expected file.
  checkExpectedFile(clientData.lockFile.fd(), lockFilePath);
  // And the thrift socket FD
  checkExpectedFile(clientData.thriftSocket.fd(), thriftSocketPath);
  checkExpectedFile(clientData.mountdServerSocket->fd(), mountdSocketPath);

  // Make sure the received mount information is correct
  ASSERT_EQ(1, clientData.mountPoints.size());
  EXPECT_EQ(mount1Path, clientData.mountPoints.at(0).mountPath);
  EXPECT_EQ(client1Path, clientData.mountPoints.at(0).stateDirectory);
  auto& fuseChannelData0 =
      std::get<FuseChannelData>(clientData.mountPoints.at(0).channelInfo);
  checkExpectedFile(fuseChannelData0.fd.fd(), mount1FusePath);
}

TEST(Takeover, missingFdOrder) {
  TemporaryDirectory tmpDir("eden_takeover_test");
  AbsolutePath tmpDirPath = canonicalPath(tmpDir.path().string());

  // Build the TakeoverData object to send
  TakeoverData serverData;
  serverData.injectedFdOrderForTesting = std::vector<FileDescriptorType>{};

  auto lockFilePath = tmpDirPath + "lock"_pc;
  serverData.lockFile = folly::File{lockFilePath.view(), O_RDWR | O_CREAT};

  auto thriftSocketPath = tmpDirPath + "thrift"_pc;
  serverData.thriftSocket =
      folly::File{thriftSocketPath.view(), O_RDWR | O_CREAT};

  auto mountdSocketPath = tmpDirPath + "mountd"_pc;
  serverData.mountdServerSocket =
      folly::File{mountdSocketPath.view(), O_RDWR | O_CREAT};

  auto mount1Path = tmpDirPath + "mount1"_pc;
  auto client1Path = tmpDirPath + "client1"_pc;
  auto mount1FusePath = tmpDirPath + "fuse1"_pc;
  serverData.mountPoints.emplace_back(
      mount1Path,
      client1Path,
      FuseChannelData{
          folly::File{mount1FusePath.view(), O_RDWR | O_CREAT},
          fuse_init_out{}},
      SerializedInodeMap{});

  // Perform the takeover
  auto serverSendFuture = serverData.takeoverComplete.getFuture();
  TestHandler handler{std::move(serverData)};
  auto result = runTakeover(tmpDir, &handler);
  ASSERT_TRUE(serverSendFuture.hasValue());
  EXPECT_TRUE(result.hasValue());
  const auto& clientData = result.value();

  // Make sure we didn't receive any files because the fd order was empty
  EXPECT_EQ(clientData.lockFile.fd(), -1);
  EXPECT_EQ(clientData.thriftSocket.fd(), -1);
  EXPECT_FALSE(clientData.mountdServerSocket.has_value());
}

TEST(Takeover, nfsNotEnabled) {
  TemporaryDirectory tmpDir("eden_takeover_test");
  AbsolutePath tmpDirPath = canonicalPath(tmpDir.path().string());

  // Build the TakeoverData object to send
  TakeoverData serverData;

  auto lockFilePath = tmpDirPath + "lock"_pc;
  serverData.lockFile = folly::File{lockFilePath.view(), O_RDWR | O_CREAT};

  auto thriftSocketPath = tmpDirPath + "thrift"_pc;
  serverData.thriftSocket =
      folly::File{thriftSocketPath.view(), O_RDWR | O_CREAT};

  serverData.mountdServerSocket = std::nullopt;

  auto mount1Path = tmpDirPath + "mount1"_pc;
  auto client1Path = tmpDirPath + "client1"_pc;
  auto mount1FusePath = tmpDirPath + "fuse1"_pc;
  serverData.mountPoints.emplace_back(
      mount1Path,
      client1Path,
      FuseChannelData{
          folly::File{mount1FusePath.view(), O_RDWR | O_CREAT},
          fuse_init_out{}},
      SerializedInodeMap{});

  // Perform the takeover
  auto serverSendFuture = serverData.takeoverComplete.getFuture();
  TestHandler handler{std::move(serverData)};
  auto result = runTakeover(tmpDir, &handler);
  ASSERT_TRUE(serverSendFuture.hasValue());
  EXPECT_TRUE(result.hasValue());
  const auto& clientData = result.value();

  // Make sure the received lock file refers to the expected file.
  checkExpectedFile(clientData.lockFile.fd(), lockFilePath);
  // And the thrift socket FD
  checkExpectedFile(clientData.thriftSocket.fd(), thriftSocketPath);
  EXPECT_EQ(clientData.mountdServerSocket, std::nullopt);

  // Make sure the received mount information is correct
  ASSERT_EQ(1, clientData.mountPoints.size());
  EXPECT_EQ(mount1Path, clientData.mountPoints.at(0).mountPath);
  EXPECT_EQ(client1Path, clientData.mountPoints.at(0).stateDirectory);
  auto& fuseChannelData0 =
      std::get<FuseChannelData>(clientData.mountPoints.at(0).channelInfo);
  checkExpectedFile(fuseChannelData0.fd.fd(), mount1FusePath);
}

TEST(Takeover, passedSocketsRemainReadable) {
  // Like a graceful restart, the sender closes every copy of the socket it
  // passed, and its end of the channel, before the receiver reads the message.
  // On macOS that closure schedules unp_gc.
  auto previousHandler = signal(SIGPIPE, SIG_IGN);
  SCOPE_EXIT {
    signal(SIGPIPE, previousHandler);
  };
  SocketPair channel;
  SocketPair conn;
  EventBase evb;
  FutureUnixSocket receiver{&evb, folly::File{channel.read.release(), true}};
  InFlightSocketPin pin{receiver.getRawFd()};
  {
    FutureUnixSocket sender{&evb, folly::File{channel.write.release(), true}};
    std::vector<folly::File> files;
    files.emplace_back(conn.read.release(), true);
    sender
        .send(
            UnixSocket::Message{
                folly::IOBuf{folly::IOBuf::COPY_BUFFER, "x", 1},
                std::move(files)})
        .getVia(&evb);
  }
  if (folly::kIsApple) {
    // An unreachable socket queued on itself only closes after unp_gc has
    // flushed every socket in that GC pass. Its peer's EOF signals completion.
    SocketPair gc;
    auto gcFd = gc.write.fd();
    FutureUnixSocket gcSender{&evb, folly::File{gc.write.release(), true}};
    std::vector<folly::File> files;
    files.emplace_back(gc.read.release(), true);
    gcSender
        .send(
            UnixSocket::Message{
                folly::IOBuf{folly::IOBuf::COPY_BUFFER, "x", 1},
                std::move(files)})
        .getVia(&evb);
    // Schedule GC after the cycle is queued, even if the sender's GC ran early.
    {
      SocketPair trigger;
    }
    struct pollfd event{gcFd, POLLIN, 0};
    ASSERT_EQ(1, poll(&event, 1, 5000));
    char byte;
    ASSERT_EQ(0, read(gcFd, &byte, sizeof(byte)));
  }

  auto msg = receiver.receive(5s).getVia(&evb);
  ASSERT_EQ(1, msg.files.size());
  ASSERT_EQ(5, write(conn.write.fd(), "hello", 5));
  char buf[5];
  ASSERT_EQ(5, read(msg.files[0].fd(), buf, sizeof(buf)));
  EXPECT_EQ("hello", std::string_view(buf, sizeof(buf)));
}
