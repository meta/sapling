/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include <folly/Exception.h>
#include <folly/File.h>
#include <folly/FileUtil.h>
#include <folly/Portability.h>
#include <folly/Range.h>
#include <folly/ScopeGuard.h>
#include <folly/SocketAddress.h>
#include <folly/Synchronized.h>
#include <folly/futures/Future.h>
#include <folly/io/Cursor.h>
#include <folly/io/FsUtil.h>
#include <folly/io/IOBuf.h>
#include <folly/io/async/EventBase.h>
#include <folly/io/async/EventBaseThread.h>
#include <folly/portability/Fcntl.h>
#include <folly/synchronization/Baton.h>
#include <folly/synchronization/SaturatingSemaphore.h>
#include <folly/test/TestUtils.h>
#include <folly/testing/TestUtil.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <cstring>
#include <functional>
#include <limits>
#include <optional>
#include <system_error>
#include <thread>
#include <unordered_map>

#ifdef __linux__
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <linux/securebits.h>
#include <sched.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include "eden/fs/privhelper/priority/LinuxMemoryPriority.h"
#include "eden/fs/utils/MountInfoTable.h"
#include "eden/fs/utils/Statmount.h"
#endif

#include "eden/common/telemetry/DynamicEvent.h"
#include "eden/common/testharness/ScopedEnvVar.h"
#include "eden/common/testharness/TempFile.h"
#include "eden/common/utils/UserInfo.h"
#include "eden/fs/privhelper/PrivHelper.h"
#include "eden/fs/privhelper/PrivHelperConn.h"
#include "eden/fs/privhelper/PrivHelperImpl.h"
#include "eden/fs/privhelper/PrivHelperRollback.h"
#include "eden/fs/privhelper/RestartSentinel.h"
#include "eden/fs/privhelper/test/PrivHelperTestServer.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"
#include "eden/fs/telemetry/IXplatLogger.h"
#include "eden/fs/telemetry/XplatKeys.h"

using namespace facebook::eden;
using namespace std::chrono_literals;
using facebook::eden::UserInfo;
using folly::checkUnixError;
using folly::EventBase;
using folly::EventBaseThread;
using folly::File;
using folly::Future;
using folly::Promise;
using folly::StringPiece;
using folly::Unit;
using folly::test::TemporaryDirectory;
using folly::test::TemporaryFile;
using std::string;
using testing::UnorderedElementsAre;

#ifdef __linux__
namespace {
void runInMountNamespace(
    const std::function<void()>& test,
    uid_t mappedUid = 0) {
  const auto uid = getuid();
  const auto gid = getgid();
  const auto pid = fork();
  if (pid < 0) {
    FAIL() << "fork failed: " << folly::errnoStr(errno);
  }
  if (pid == 0) {
    if (unshare(CLONE_NEWUSER | CLONE_NEWNS) != 0) {
      _exit(77);
    }
    try {
      checkUnixError(
          folly::writeFile(
              fmt::format("{} {} 1\n", mappedUid, uid), "/proc/self/uid_map")
              ? 0
              : -1);
      checkUnixError(
          folly::writeFile(StringPiece{"deny\n"}, "/proc/self/setgroups") ? 0
                                                                          : -1);
      checkUnixError(
          folly::writeFile(fmt::format("0 {} 1\n", gid), "/proc/self/gid_map")
              ? 0
              : -1);
      checkUnixError(
          mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr));
      test();
    } catch (const std::exception& ex) {
      ADD_FAILURE() << ex.what();
    }
    _exit(::testing::Test::HasFailure() ? 1 : 0);
  }
  int status;
  ASSERT_EQ(pid, waitpid(pid, &status, 0));
  if (WIFEXITED(status) && WEXITSTATUS(status) == 77) {
    GTEST_SKIP() << "user/mount namespaces are unavailable";
  }
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}
void installPrivHelperRollbackMarker() {
  checkUnixError(mount("tmpfs", "/etc", "tmpfs", 0, "size=1m"));
  folly::fs::create_directory("/etc/eden");
  File marker(
      kDisablePrivHelperHardeningPath,
      O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC,
      0600);
  ASSERT_TRUE(disablePrivHelperHardening());
}
} // namespace
#endif

TEST(TccDisclaimKillswitch, presentWhenFileExists) {
  TemporaryFile killswitch;
  EXPECT_TRUE(tccDisclaimKillswitchPresent(killswitch.path().c_str()));
}

TEST(TccDisclaimKillswitch, absentWhenFileDoesNotExist) {
  TemporaryDirectory dir;
  auto missing = (dir.path() / "disable-tcc-disclaim").string();
  EXPECT_FALSE(tccDisclaimKillswitchPresent(missing.c_str()));
}

/**
 * A PrivHelperServer implementation intended to be used in a separate thread in
 * the same process.
 *
 * This is different than PrivHelperTestServer which is intended to be used in a
 * separate forked process.
 */
class PrivHelperThreadedTestServer : public PrivHelperServer {
 public:
  Promise<File> setFuseMountResult(StringPiece path) {
    Promise<File> promise;
    {
      auto data = data_.wlock();
      auto results =
          data->fuseMountResults.emplace(path.str(), std::list<Future<File>>{});
      results.first->second.emplace_back(promise.getFuture());
    }
    return promise;
  }

  Promise<File> setFuseRetryMountResult(StringPiece path) {
    Promise<File> promise;
    data_.wlock()
        ->fuseMountResults.find(path.str())
        ->second.emplace_back(promise.getFuture());
    return promise;
  }

  Promise<Unit> setFuseUnmountResult(StringPiece path) {
    Promise<Unit> promise;
    {
      auto data = data_.wlock();
      auto results = data->fuseUnmountResults.emplace(
          path.str(), std::list<Future<Unit>>{});
      results.first->second.emplace_back(promise.getFuture());
    }
    return promise;
  }

  Promise<Unit> setBindMountResult(StringPiece path) {
    Promise<Unit> promise;
    {
      auto data = data_.wlock();
      auto results =
          data->bindMountResults.emplace(path.str(), std::list<Future<Unit>>{});
      results.first->second.emplace_back(promise.getFuture());
    }
    return promise;
  }

  Promise<Unit> setBindUnmountResult(StringPiece path) {
    Promise<Unit> promise;
    {
      auto data = data_.wlock();
      auto results = data->bindUnmountResults.emplace(
          path.str(), std::list<Future<Unit>>{});
      results.first->second.emplace_back(promise.getFuture());
    }
    return promise;
  }

  std::vector<string> getUnusedFuseUnmountResults() {
    return getUnusedResults(data_.rlock()->fuseUnmountResults);
  }

  std::vector<string> getUnusedBindUnmountResults() {
    return getUnusedResults(data_.rlock()->bindUnmountResults);
  }

  std::vector<File> getLogFileRequests() {
    auto data = data_.wlock();
    return std::move(data->logFiles);
  }

  std::vector<std::string> getRequestedVfsTypes() {
    auto data = data_.wlock();
    return std::move(data->requestedVfsTypes);
  }

  void setAfterSanityCheck(std::function<void()> callback) {
    data_.wlock()->afterSanityCheck = std::move(callback);
  }

 private:
  struct Data {
    std::unordered_map<string, std::list<Future<File>>> fuseMountResults;
    std::vector<std::string> requestedVfsTypes;
    std::unordered_map<string, std::list<Future<Unit>>> fuseUnmountResults;
    std::unordered_map<string, std::list<Future<Unit>>> bindMountResults;
    std::unordered_map<string, std::list<Future<Unit>>> bindUnmountResults;
    std::vector<File> logFiles;
    std::function<void()> afterSanityCheck;
  };

  void sanityCheckOpenedMountPoint(
      const std::string& mountPoint,
      int mountPointFd) override {
    PrivHelperServer::sanityCheckOpenedMountPoint(mountPoint, mountPointFd);
    auto callback = std::move(data_.wlock()->afterSanityCheck);
    if (callback) {
      callback();
    }
  }

  template <typename T>
  folly::Future<T> getResultFuture(
      std::unordered_map<string, std::list<Future<T>>>& map,
      StringPiece path) {
    auto iter = map.find(path.str());
    if (iter == map.end()) {
      throw std::runtime_error(
          folly::to<string>("no result available for ", path));
    }
    auto future = std::move(iter->second.front());
    iter->second.pop_front();
    if (iter->second.empty()) {
      map.erase(iter);
    }
    return future;
  }

  template <typename T>
  std::vector<string> getUnusedResults(
      const std::unordered_map<std::string, std::list<Future<T>>>& map) {
    std::vector<string> results;
    for (const auto& entry : map) {
      results.push_back(entry.first);
    }
    return results;
  }

  folly::File fuseMount(
      const char* mountPath,
      bool /*readOnly*/,
      const char* vfsType) override {
    Future<folly::File> future = Future<folly::File>::makeEmpty();
    {
      auto data = data_.wlock();
      data->requestedVfsTypes.emplace_back(vfsType);
      future = getResultFuture(data->fuseMountResults, mountPath);
    }
    return std::move(future).get(1s);
  }

  void unmount(const char* mountPath, UnmountOptions /* options */) override {
    auto future = getResultFuture(data_.wlock()->fuseUnmountResults, mountPath);
    std::move(future).get(1s);

    // fuseUnmount has the side effect of implicitly unmounting all contained
    // bind mounts, so let's make that appear to be the case here.
    // This loop is the C++20 suggested impl of erase_if, but inlined here for
    // environments that are not yet C++20.
    auto data = data_.wlock();
    auto mountPrefix = folly::to<std::string>(mountPath, "/");
    for (auto iter = data->bindUnmountResults.begin(),
              last = data->bindUnmountResults.end();
         iter != last;) {
      if (folly::StringPiece(iter->first).startsWith(mountPrefix)) {
        iter = data->bindUnmountResults.erase(iter);
      } else {
        ++iter;
      }
    }
  }

  void insecureBindMount(const char* /* clientPath */, const char* mountPath)
      override {
    auto future = getResultFuture(data_.wlock()->bindMountResults, mountPath);
    std::move(future).get(1s);
  }

  void bindMount(
      const char* /* clientPath */,
      const char* mountPath,
      folly::StringPiece mountRoot) override {
#ifndef __APPLE__
    static_cast<void>(openBindMountTarget(mountRoot, mountPath));
#else
    (void)mountRoot;
#endif
    auto future = getResultFuture(data_.wlock()->bindMountResults, mountPath);
    std::move(future).get(1s);
  }

  void insecureBindUnmount(const char* mountPath) override {
    auto future = getResultFuture(data_.wlock()->bindUnmountResults, mountPath);
    std::move(future).get(1s);
  }

  void bindUnmount(const char* mountPath, folly::StringPiece mountRoot)
      override {
#ifndef __APPLE__
    static_cast<void>(openBindMountTarget(mountRoot, mountPath));
#else
    (void)mountRoot;
#endif
    auto future = getResultFuture(data_.wlock()->bindUnmountResults, mountPath);
    std::move(future).get(1s);
  }

  bool useModernMountApi() const override {
    return false;
  }

  void setLogFile(folly::File logFile) override {
    auto data = data_.wlock();
    data->logFiles.push_back(std::move(logFile));
  }

  folly::Synchronized<Data> data_;
};

class PrivHelperFdUnmountTestServer : public PrivHelperServer {
 public:
  bool insecureBindUnmountCalled() const {
    return insecureBindUnmountCalled_.load();
  }

  void setUnmountError(int error) {
    unmountError_.store(error);
  }

  size_t fdUnmountCalls() const {
    return fdUnmountCalls_.load();
  }

 private:
  void unmount(const char* /* mountPath */, UnmountOptions /* options */)
      override {}

  void insecureBindUnmount(const char* /* mountPath */) override {
    insecureBindUnmountCalled_.store(true);
    throw std::runtime_error("unexpected path-based bind unmount fallback");
  }

#ifndef __APPLE__
  int umountBindMountByFd(const char* /* procFdPath */) override {
    ++fdUnmountCalls_;
    errno = unmountError_.load();
    return -1;
  }
#endif

  std::atomic<bool> insecureBindUnmountCalled_{false};
  std::atomic<int> unmountError_{EINVAL};
  std::atomic<size_t> fdUnmountCalls_{0};
};

namespace {

UnixSocket::Message makeLegacyMacFuseConfigRequest(
    uint32_t xid,
    uint32_t requestId,
    uint64_t value) {
  constexpr uint32_t kProtocolVersion = 1;
  constexpr uint32_t kMetadataLength = 8;
  constexpr size_t kRequestSize = 4 * sizeof(uint32_t) + sizeof(uint64_t);

  UnixSocket::Message request;
  request.data = folly::IOBuf(folly::IOBuf::CREATE, kRequestSize);
  folly::io::Appender appender(&request.data, kRequestSize);
  appender.write<uint32_t>(kProtocolVersion);
  appender.write<uint32_t>(kMetadataLength);
  appender.write<uint32_t>(xid);
  appender.write<uint32_t>(requestId);
  appender.write<uint64_t>(value);
  return request;
}

static_assert(PrivHelperConn::REQ_SET_DAEMON_TIMEOUT == 9);
static_assert(PrivHelperConn::REQ_SET_USE_EDENFS == 10);

// An arbitrary fixed point in time, so that the restart window can be aged
// without sleeping.
constexpr uint64_t kFakeNow = 1'700'000'000ull;

EdenFsRestartArgs makeRestartArgs(std::string sentinelPath) {
  EdenFsRestartArgs args;
  args.enabled = true;
  args.sentinelPath = std::move(sentinelPath);
  args.restartCount = 1;
  args.firstRestartEpochSec = kFakeNow;
  args.maxRestarts = 3;
  args.windowSeconds = 600;
  return args;
}

const std::vector<std::string> kSentinelArgv{
    "/usr/local/libexec/eden/edenfs",
    "--edenfs"};

/**
 * Restrict the sentinel to its owner, as the daemon writes it. Both
 * folly::writeFile and TemporaryFile create a file 0666 & ~umask, so under a
 * group-writable umask the privhelper would refuse a sentinel it should accept.
 */
void restrictSentinelToOwner(const std::string& path) {
  checkUnixError(::chmod(path.c_str(), 0600));
}

EdenFsRestartArgs roundTrip(const EdenFsRestartArgs& args) {
  auto msg = PrivHelperConn::serializeSetRestartArgsRequest(/*xid=*/42, args);
  folly::io::Cursor cursor{&msg.data};
  PrivHelperConn::parsePacket(cursor);

  EdenFsRestartArgs parsed;
  PrivHelperConn::parseSetRestartArgsRequest(cursor, parsed);
  return parsed;
}

std::string serializeRestartArgsRejection(const EdenFsRestartArgs& args) {
  try {
    PrivHelperConn::serializeSetRestartArgsRequest(/*xid=*/42, args);
  } catch (const std::invalid_argument& ex) {
    return ex.what();
  } catch (const std::exception& ex) {
    ADD_FAILURE() << "expected std::invalid_argument, got: " << ex.what();
    return {};
  }
  ADD_FAILURE() << "the serializer accepted oversized restart args";
  return {};
}

void appendLengthPrefixedString(
    folly::io::Appender& appender,
    folly::StringPiece value) {
  appender.write<uint32_t>(static_cast<uint32_t>(value.size()));
  appender.push(folly::ByteRange(value));
}

// What is left unread when the parser rejects the relaunch command: the four
// trailing counters.
constexpr size_t kBytesAfterRelaunchCommand =
    3 * sizeof(uint32_t) + sizeof(uint64_t);
// Rejecting the sentinel path leaves the argv and env counts as well.
constexpr size_t kBytesAfterSentinelPath =
    2 * sizeof(uint32_t) + kBytesAfterRelaunchCommand;

/**
 * An enabled restart-args body whose sentinel path declares
 * `sentinelPathLength` bytes without supplying any, whose relaunch command is
 * whatever `writeRelaunchCommand` appends, and whose four trailing counters
 * are zero. Encoded independently of the serializer under test.
 */
template <typename Fn>
folly::IOBuf makeRestartArgsBody(
    uint32_t sentinelPathLength,
    Fn writeRelaunchCommand) {
  constexpr size_t kBodySize = 256;
  folly::IOBuf body{folly::IOBuf::CREATE, kBodySize};
  folly::io::Appender appender{&body, kBodySize};
  appender.write<uint8_t>(1);
  appender.write<uint32_t>(sentinelPathLength);
  writeRelaunchCommand(appender);
  appender.write<uint32_t>(0);
  appender.write<uint64_t>(0);
  appender.write<uint32_t>(0);
  appender.write<uint32_t>(0);
  return body;
}

/** The errno the parser rejected `cursor` with, or 0 if it accepted it. */
int parseRestartArgsRejection(folly::io::Cursor& cursor) {
  EdenFsRestartArgs args;
  try {
    PrivHelperConn::parseSetRestartArgsRequest(cursor, args);
  } catch (const std::system_error& ex) {
    return ex.code().value();
  } catch (const std::exception& ex) {
    ADD_FAILURE() << "expected a std::system_error, got: " << ex.what();
    return 0;
  }
  ADD_FAILURE() << "the parser accepted a malformed message";
  return 0;
}

/**
 * Assert `body` is rejected without the parser ever sizing an allocation from
 * the length or count it declares.
 *
 * Only Cursor::readFixedString() sizes anything from the wire here, and it
 * reserves the declared length before draining the message looking for those
 * bytes. Finding `bytesLeftUnread` still there is what shows it was never
 * reached.
 */
void expectRejectedBeforeSizingAnything(
    const folly::IOBuf& body,
    size_t bytesLeftUnread) {
  folly::io::Cursor cursor{&body};
  EXPECT_EQ(EINVAL, parseRestartArgsRejection(cursor));
  EXPECT_EQ(bytesLeftUnread, cursor.totalLength());
}

} // namespace

TEST(PrivHelperConnRestartArgs, roundTripPreservesAwkwardValues) {
  auto expected =
      makeRestartArgs("/var/eden dir/.edenfs_restart_armed \xc3\xa9");
  // Above 2^32, to catch a truncated width on the wire.
  expected.firstRestartEpochSec = uint64_t{1} << 33;
  expected.relaunchArgv = {
      "/usr/local/libexec/eden/edenfs",
      "--edenfsctlPath=/opt/eden dir/edenfsctl",
      "--configPath=/home/us\xc3\xa9r/.edenrc",
      ""};
  // Duplicate keys are deliberate: the codec must not reorder or coalesce them.
  expected.relaunchEnv = {
      {"PATH", "/usr/bin:/bin"},
      {"EDENFS_EXTRA_ARGS", "--logging=eden=DBG2,eden.fs=DBG7"},
      {"EMPTY", ""},
      {"HOME", "/home/first"},
      {"HOME", "/home/last"}};

  EXPECT_EQ(expected, roundTrip(expected));
}

TEST(PrivHelperConnRestartArgs, roundTripPreservesAnEmptyRelaunchCommand) {
  auto expected = makeRestartArgs("/var/eden/.edenfs_restart_armed");
  ASSERT_TRUE(expected.relaunchArgv.empty());
  ASSERT_TRUE(expected.relaunchEnv.empty());

  EXPECT_EQ(expected, roundTrip(expected));
}

TEST(PrivHelperConnRestartArgs, serializerRejectsAnOversizedSentinelPath) {
  auto args = makeRestartArgs(
      std::string(PrivHelperConn::kMaxSentinelPathBytes + 1, 'a'));

  EXPECT_THAT(
      serializeRestartArgsRejection(args),
      ::testing::HasSubstr("sentinel path"));
}

TEST(PrivHelperConnRestartArgs, serializerRejectsTooManyArgvEntries) {
  auto args = makeRestartArgs("/var/eden/.edenfs_restart_armed");
  args.relaunchArgv.resize(PrivHelperConn::kMaxRelaunchArgvEntries + 1);

  EXPECT_THAT(
      serializeRestartArgsRejection(args),
      ::testing::HasSubstr("relaunch argv"));
}

TEST(PrivHelperConnRestartArgs, serializerRejectsTooManyEnvEntries) {
  auto args = makeRestartArgs("/var/eden/.edenfs_restart_armed");
  args.relaunchEnv.resize(PrivHelperConn::kMaxRelaunchEnvEntries + 1);

  EXPECT_THAT(
      serializeRestartArgsRejection(args),
      ::testing::HasSubstr("relaunch env"));
}

TEST(PrivHelperConnRestartArgs, rejectsASentinelPathBeyondItsByteLimit) {
  expectRejectedBeforeSizingAnything(
      makeRestartArgsBody(
          std::numeric_limits<uint32_t>::max(),
          [](folly::io::Appender& appender) {
            appender.write<uint32_t>(0); // argv count
            appender.write<uint32_t>(0); // env count
          }),
      kBytesAfterSentinelPath);
}

TEST(PrivHelperConnRestartArgs, rejectsAnArgvCountBeyondTheLimit) {
  expectRejectedBeforeSizingAnything(
      makeRestartArgsBody(
          0,
          [](folly::io::Appender& appender) {
            appender.write<uint32_t>(
                PrivHelperConn::kMaxRelaunchArgvEntries + 1);
          }),
      kBytesAfterRelaunchCommand);
}

TEST(PrivHelperConnRestartArgs, rejectsAnEnvCountBeyondTheLimit) {
  expectRejectedBeforeSizingAnything(
      makeRestartArgsBody(
          0,
          [](folly::io::Appender& appender) {
            appender.write<uint32_t>(0); // argv count
            appender.write<uint32_t>(
                PrivHelperConn::kMaxRelaunchEnvEntries + 1);
          }),
      kBytesAfterRelaunchCommand);
}

TEST(PrivHelperConnRestartArgs, rejectsAStringBeyondTheByteLimit) {
  expectRejectedBeforeSizingAnything(
      makeRestartArgsBody(
          0,
          [](folly::io::Appender& appender) {
            appender.write<uint32_t>(1); // argv count
            appender.write<uint32_t>(std::numeric_limits<uint32_t>::max());
          }),
      kBytesAfterRelaunchCommand);
}

TEST(PrivHelperConnRestartArgs, rejectsAStringLongerThanTheMessage) {
  expectRejectedBeforeSizingAnything(
      makeRestartArgsBody(
          0,
          [](folly::io::Appender& appender) {
            appender.write<uint32_t>(1); // argv count
            appender.write<uint32_t>(PrivHelperConn::kMaxRelaunchBytes / 2);
          }),
      kBytesAfterRelaunchCommand);
}

TEST(PrivHelperConnRestartArgs, rejectsARelaunchCommandOverTheByteLimit) {
  const std::string halfLimit(PrivHelperConn::kMaxRelaunchBytes / 2, 'a');
  auto body =
      makeRestartArgsBody(0, [&halfLimit](folly::io::Appender& appender) {
        appender.write<uint32_t>(2); // argv count
        appendLengthPrefixedString(appender, halfLimit);
        appendLengthPrefixedString(appender, halfLimit);
        appender.write<uint32_t>(1); // env count
        appender.write<uint32_t>(1); // env name length
      });
  folly::io::Cursor cursor{&body};

  EXPECT_EQ(EINVAL, parseRestartArgsRejection(cursor));
}

TEST(PrivHelperConnRestartArgs, serializerRejectsARelaunchCommandOverTheLimit) {
  // The budget spans argv and env together, so the environment is what tips
  // this one over.
  auto args = makeRestartArgs("/var/eden/.edenfs_restart_armed");
  const size_t half = PrivHelperConn::kMaxRelaunchBytes / 2;
  args.relaunchArgv = {std::string(half, 'a'), std::string(half, 'b')};
  args.relaunchEnv = {{"PATH", "/usr/bin"}};

  const auto error = serializeRestartArgsRejection(args);
  EXPECT_THAT(error, ::testing::HasSubstr("relaunch env name"));
  EXPECT_THAT(error, ::testing::HasSubstr("budget"));
}

TEST(PrivHelperRestartCounterEnv, absentIsZero) {
  ScopedEnvVar var{kEdenFsRestartCountEnv};
  var.unset();
  EXPECT_EQ(0, readEdenFsRestartCounterEnv(kEdenFsRestartCountEnv));
}

TEST(PrivHelperRestartCounterEnv, emptyIsZero) {
  ScopedEnvVar var{kEdenFsRestartCountEnv};
  var.set("");
  EXPECT_EQ(0, readEdenFsRestartCounterEnv(kEdenFsRestartCountEnv));
}

TEST(PrivHelperRestartCounterEnv, malformedIsZero) {
  ScopedEnvVar var{kEdenFsRestartCountEnv};
  var.set("three");
  EXPECT_EQ(0, readEdenFsRestartCounterEnv(kEdenFsRestartCountEnv));
}

TEST(PrivHelperRestartCounterEnv, readsAValueAbove32Bits) {
  // The epoch variable outgrows uint32 in 2106, so the reader is 64-bit.
  ScopedEnvVar var{kEdenFsFirstRestartAtEnv};
  var.set("4294967296");
  EXPECT_EQ(
      uint64_t{1} << 32, readEdenFsRestartCounterEnv(kEdenFsFirstRestartAtEnv));
}

TEST(PrivHelperConnCleanShutdown, roundTrip) {
  constexpr folly::StringPiece kReason{"graceful restart"};
  auto msg = PrivHelperConn::serializeNotifyCleanShutdownRequest(
      /*xid=*/7, kReason);
  folly::io::Cursor cursor{&msg.data};
  PrivHelperConn::parsePacket(cursor);

  std::string reason;
  PrivHelperConn::parseNotifyCleanShutdownRequest(cursor, reason);
  EXPECT_EQ(kReason, reason);
}

TEST(PrivHelperConnCleanShutdown, acceptsAReasonAtTheByteLimit) {
  constexpr size_t kReasonByteLimit = 4096;
  static_assert(
      PrivHelperConn::kMaxCleanShutdownReasonBytes == kReasonByteLimit);
  const std::string expected(kReasonByteLimit, 'a');
  auto msg = PrivHelperConn::serializeNotifyCleanShutdownRequest(
      /*xid=*/7, expected);
  folly::io::Cursor cursor{&msg.data};
  PrivHelperConn::parsePacket(cursor);

  std::string reason;
  PrivHelperConn::parseNotifyCleanShutdownRequest(cursor, reason);
  EXPECT_EQ(expected, reason);
}

TEST(PrivHelperConnCleanShutdown, truncatesAReasonBeyondTheByteLimit) {
  const std::string expected(PrivHelperConn::kMaxCleanShutdownReasonBytes, 'a');
  auto msg = PrivHelperConn::serializeNotifyCleanShutdownRequest(
      /*xid=*/7, expected + "b");
  folly::io::Cursor cursor{&msg.data};
  PrivHelperConn::parsePacket(cursor);

  std::string reason;
  PrivHelperConn::parseNotifyCleanShutdownRequest(cursor, reason);
  EXPECT_EQ(expected, reason);
}

TEST(
    PrivHelperConnCleanShutdown,
    rejectsAReasonBeyondTheByteLimitBeforeAllocation) {
  constexpr uint32_t kTrailingMarker = 0x12345678;
  folly::IOBuf body{folly::IOBuf::CREATE, 2 * sizeof(uint32_t)};
  folly::io::Appender appender{&body, 2 * sizeof(uint32_t)};
  appender.write<uint32_t>(PrivHelperConn::kMaxCleanShutdownReasonBytes + 1);
  appender.write<uint32_t>(kTrailingMarker);
  folly::io::Cursor cursor{&body};

  std::string reason;
  try {
    PrivHelperConn::parseNotifyCleanShutdownRequest(cursor, reason);
    ADD_FAILURE() << "the parser accepted an oversized shutdown reason";
  } catch (const std::system_error& ex) {
    EXPECT_EQ(EINVAL, ex.code().value());
  }
  EXPECT_EQ(sizeof(kTrailingMarker), cursor.totalLength());
}

TEST(PrivHelperNfsMount, onlyUnixSocketTransportsNeedFstypeOverride) {
  NFSMountOptions options;
  options.mountdAddr = folly::SocketAddress::makeFromPath("/tmp/mountd.sock");
  options.nfsdAddr = folly::SocketAddress::makeFromPath("/tmp/nfsd.sock");
  EXPECT_TRUE(needsFstypeOverride(options));

  options.mountdAddr = folly::SocketAddress{"127.0.0.1", 1};
  options.nfsdAddr = folly::SocketAddress{"127.0.0.1", 2049};
  EXPECT_FALSE(needsFstypeOverride(options));

  options.mountdAddr = folly::SocketAddress{"::1", 1};
  options.nfsdAddr = folly::SocketAddress{"::1", 2049};
  EXPECT_FALSE(needsFstypeOverride(options));
}

class RawPrivHelperClient : private UnixSocket::ReceiveCallback {
 public:
  explicit RawPrivHelperClient(File conn) {
    clientIoThread_.getEventBase()->runInEventBaseThreadAndWait(
        [this, conn = std::move(conn)]() mutable {
          conn_ = UnixSocket::makeUnique(
              clientIoThread_.getEventBase(), std::move(conn));
          conn_->setReceiveCallback(this);
        });
  }

  ~RawPrivHelperClient() override {
    clientIoThread_.getEventBase()->runInEventBaseThreadAndWait([this] {
      if (conn_) {
        conn_->clearReceiveCallback();
        conn_->closeNow();
        conn_.reset();
      }
    });
  }

  /** Sends without expecting a reply, as a one-way request does. */
  void send(UnixSocket::Message request) {
    clientIoThread_.getEventBase()->runInEventBaseThreadAndWait(
        [this, request = std::move(request)]() mutable {
          conn_->send(std::move(request));
        });
  }

  UnixSocket::Message sendAndRecv(UnixSocket::Message request) {
    Promise<UnixSocket::Message> promise;
    auto future = promise.getFuture();
    clientIoThread_.getEventBase()->runInEventBaseThreadAndWait(
        [this,
         request = std::move(request),
         promise = std::move(promise)]() mutable {
          responsePromise_ = std::move(promise);
          conn_->send(std::move(request));
        });
    return std::move(future).get(1s);
  }

 private:
  void messageReceived(UnixSocket::Message&& message) noexcept override {
    if (responsePromise_) {
      std::move(*responsePromise_).setValue(std::move(message));
      responsePromise_.reset();
    }
  }

  void eofReceived() noexcept override {
    setResponseException(
        folly::make_exception_wrapper<std::runtime_error>("privhelper exited"));
  }

  void socketClosed() noexcept override {
    setResponseException(
        folly::make_exception_wrapper<std::runtime_error>(
            "privhelper client socket closed"));
  }

  void receiveError(const folly::exception_wrapper& ew) noexcept override {
    setResponseException(ew);
  }

  void setResponseException(folly::exception_wrapper ew) noexcept {
    if (responsePromise_) {
      std::move(*responsePromise_).setException(std::move(ew));
      responsePromise_.reset();
    }
  }

  EventBaseThread clientIoThread_;
  UnixSocket::UniquePtr conn_;
  std::optional<Promise<UnixSocket::Message>> responsePromise_;
};

/**
 * An IXplatLogger that records logged events so tests can assert on the
 * telemetry the privhelper client emits. logEvent() is called from the
 * client's EventBase thread.
 */
class RecordingXplatLogger : public IXplatLogger {
 public:
  void logEvent(std::string_view category, const DynamicEvent& event) override {
    recordedEvents_.wlock()->emplace_back(std::string{category}, event);
    eventRecorded_.post();
  }

  std::vector<std::pair<std::string, DynamicEvent>> getEvents() const {
    return *recordedEvents_.rlock();
  }

  /**
   * Block until at least one event has been recorded. Returns false if none
   * was recorded within the timeout.
   */
  bool waitForEvent(std::chrono::milliseconds timeout) {
    return eventRecorded_.try_wait_for(timeout);
  }

 private:
  folly::Synchronized<std::vector<std::pair<std::string, DynamicEvent>>>
      recordedEvents_;
  // Saturating (multi-post safe), unlike folly::Baton.
  folly::SaturatingSemaphore<true /* MayBlock */> eventRecorded_;
};

class PrivHelperTest : public ::testing::Test {
 protected:
  void SetUp() override {
    File clientConn;
    File serverConn;
    PrivHelperConn::createConnPair(clientConn, serverConn);

    serverThread_ =
        std::thread([this, conn = std::move(serverConn)]() mutable noexcept {
          server_.initPartial(std::move(conn), getuid(), getgid());
          server_.run();
        });
    client_ = createTestPrivHelper(std::move(clientConn));
    client_->setEdenFsEventsLogger(
        std::make_shared<EdenFsEventsLogger>(xplatLogger_));
    clientIoThread_.getEventBase()->runInEventBaseThreadAndWait(
        [&] { client_->attachEventBase(clientIoThread_.getEventBase()); });
  }

  ~PrivHelperTest() override {
    cleanup();
  }

  void cleanup() {
    client_.reset();
    if (serverThread_.joinable()) {
      serverThread_.join();
    }
  }

  std::unique_ptr<PrivHelper> client_;
  PrivHelperThreadedTestServer server_;
  std::thread serverThread_;
  EventBaseThread clientIoThread_;
  std::shared_ptr<RecordingXplatLogger> xplatLogger_{
      std::make_shared<RecordingXplatLogger>()};
};

class PrivHelperFdUnmountTest : public ::testing::Test {
 protected:
  void SetUp() override {
    File clientConn;
    File serverConn;
    PrivHelperConn::createConnPair(clientConn, serverConn);

    serverThread_ =
        std::thread([this, conn = std::move(serverConn)]() mutable noexcept {
          server_.initPartial(std::move(conn), getuid(), getgid());
          server_.run();
        });
    client_ = createTestPrivHelper(std::move(clientConn));
    clientIoThread_.getEventBase()->runInEventBaseThreadAndWait(
        [&] { client_->attachEventBase(clientIoThread_.getEventBase()); });
  }

  ~PrivHelperFdUnmountTest() override {
    client_.reset();
    if (serverThread_.joinable()) {
      serverThread_.join();
    }
  }

  std::unique_ptr<PrivHelper> client_;
  PrivHelperFdUnmountTestServer server_;
  std::thread serverThread_;
  EventBaseThread clientIoThread_;
};

class PrivHelperRawProtocolTest : public ::testing::Test {
 protected:
  void SetUp() override {
    File clientConn;
    File serverConn;
    PrivHelperConn::createConnPair(clientConn, serverConn);

    serverThread_ =
        std::thread([this, conn = std::move(serverConn)]() mutable noexcept {
          server_.initPartial(std::move(conn), getuid(), getgid());
          server_.run();
        });
    client_.emplace(std::move(clientConn));
  }

  void TearDown() override {
    client_.reset();
    if (serverThread_.joinable()) {
      serverThread_.join();
    }
  }

  PrivHelperThreadedTestServer server_;
  std::thread serverThread_;
  std::optional<RawPrivHelperClient> client_;
};

TEST_F(PrivHelperTest, restartArgsValidationFailureCompletesTheFuture) {
  auto args = makeRestartArgs(
      std::string(PrivHelperConn::kMaxSentinelPathBytes + 1, 'a'));

  auto result = client_->setRestartArgs(args);

  EXPECT_THROW(std::move(result).get(), std::invalid_argument);
}

TEST_F(PrivHelperRawProtocolTest, legacyMacFuseConfigRequestsAreNoOps) {
  auto timeoutResponse = client_->sendAndRecv(makeLegacyMacFuseConfigRequest(
      1, PrivHelperConn::REQ_SET_DAEMON_TIMEOUT, 60'000'000'000));
  PrivHelperConn::parseEmptyResponse(
      PrivHelperConn::REQ_SET_DAEMON_TIMEOUT, timeoutResponse);

  auto useEdenFsResponse = client_->sendAndRecv(
      makeLegacyMacFuseConfigRequest(2, PrivHelperConn::REQ_SET_USE_EDENFS, 1));
  PrivHelperConn::parseEmptyResponse(
      PrivHelperConn::REQ_SET_USE_EDENFS, useEdenFsResponse);
}

#ifdef __APPLE__
TEST_F(
    PrivHelperRawProtocolTest,
    bindUnmountIsUnsupportedEvenForRegisteredPaths) {
  auto dir = makeTempDir();
  const auto path = dir.path().string();
  auto response = client_->sendAndRecv(
      PrivHelperConn::serializeTakeoverStartupRequest(1, path, {}));
  PrivHelperConn::parseEmptyResponse(
      PrivHelperConn::REQ_TAKEOVER_STARTUP, response);
  server_.setFuseUnmountResult(path).setValue();

  for (const auto& suffix : {"/buck-out", "/../victim"}) {
    response = client_->sendAndRecv(
        PrivHelperConn::serializeBindUnMountRequest(2, path + suffix));
    EXPECT_THROW_RE(
        PrivHelperConn::parseEmptyResponse(
            PrivHelperConn::REQ_UNMOUNT_BIND, response),
        std::system_error,
        "bind unmounts are not supported on macOS");
  }
}
#endif

TEST_F(PrivHelperTest, nfsFstypeOverrideCanBeTurnedOff) {
  EXPECT_TRUE(server_.nfsFstypeOverrideEnabled());
  client_->setNfsFstypeOverride(false).get(1s);
  EXPECT_FALSE(server_.nfsFstypeOverrideEnabled());
}

#ifdef __linux__
TEST_F(PrivHelperTest, fuseReadAheadRequiresARegisteredMount) {
  auto dir = makeTempDir();
  const auto path = dir.path().string();
  EXPECT_THROW_RE(
      client_->setFuseReadAhead(path, 1024).get(1s),
      std::exception,
      "No FUSE mount found");

  client_->takeoverStartup(path, {}).get(1s);
  server_.setFuseUnmountResult(path).setValue();
  // An ordinary registered directory has no BDI configuration to update.
  EXPECT_NO_THROW(client_->setFuseReadAhead(path, 1024).get(1s));

  client_->takeoverShutdown(path).get(1s);
  EXPECT_THROW_RE(
      client_->setFuseReadAhead(path, 1024).get(1s),
      std::exception,
      "No FUSE mount found");
}

TEST(PrivHelperMemoryPriorityTest, checksOwnershipBeforeWriting) {
  runInMountNamespace([&] {
    int pipeFds[2];
    checkUnixError(pipe(pipeFds));
    File reader(pipeFds[0], true);
    File writer(pipeFds[1], true);
    const auto pid = fork();
    if (pid < 0) {
      FAIL() << "fork failed: " << folly::errnoStr(errno);
    }
    if (pid == 0) {
      writer.close();
      char byte;
      _exit(folly::readNoInt(reader.fd(), &byte, 1) == 0 ? 0 : 1);
    }
    reader.close();
    SCOPE_EXIT {
      writer.close();
      int status;
      EXPECT_EQ(pid, waitpid(pid, &status, 0));
      EXPECT_TRUE(WIFEXITED(status));
      EXPECT_EQ(0, WEXITSTATUS(status));
    };

    LinuxMemoryPriority owned(1000, getuid());
    const auto original = owned.getPriorityForProcess(pid);
    ASSERT_TRUE(original.has_value());
    LinuxMemoryPriority otherOwner(1000, getuid() + 1);
    EXPECT_THROW_RE(
        otherOwner.setPriorityForProcess(pid),
        std::system_error,
        "is not owned by user");
    EXPECT_EQ(original, owned.getPriorityForProcess(pid));
    EXPECT_EQ(0, owned.setPriorityForProcess(pid));
    EXPECT_EQ(1000, owned.getPriorityForProcess(pid));

    installPrivHelperRollbackMarker();
    EXPECT_EQ(0, otherOwner.setPriorityForProcess(pid));
    EXPECT_EQ(1000, owned.getPriorityForProcess(pid));
  });
}

TEST(PrivHelperMemoryPriorityTest, allowsSettingItsOwnPriority) {
  EXPECT_EXIT(
      {
        LinuxMemoryPriority priority(1000, getuid() + 1);
        const auto result = priority.setPriorityForProcess(getpid());
        _exit(
            result == 0 && priority.getPriorityForProcess(getpid()) == 1000
                ? 0
                : 1);
      },
      ::testing::ExitedWithCode(0),
      "");
}

TEST_F(PrivHelperRawProtocolTest, famRequestsCannotCreateFilesOnLinux) {
  TemporaryDirectory dir;
  const auto outputPath = (dir.path() / "victim.txt").string();
  auto response = client_->sendAndRecv(
      PrivHelperConn::serializeStartFamRequest(
          1,
          {dir.path().string()},
          outputPath,
          outputPath,
          false,
          File("/dev/null", O_WRONLY)));
  EXPECT_THROW_RE(
      PrivHelperConn::parseStartFamResponse(response),
      std::exception,
      "unexpected privhelper message type");
  EXPECT_EQ(-1, access(outputPath.c_str(), F_OK));
  EXPECT_EQ(ENOENT, errno);

  response = client_->sendAndRecv(PrivHelperConn::serializeStopFamRequest(2));
  EXPECT_THROW_RE(
      PrivHelperConn::parseEmptyResponse(
          PrivHelperConn::REQ_STOP_FAM, response),
      std::exception,
      "unexpected privhelper message type");
}
#endif

class PrivHelperFamTestServer : public PrivHelperServer {
 public:
  using PrivHelperServer::processStartFam;
  using PrivHelperServer::processStopFam;

 private:
  AbsolutePath getFamBinaryPath() const override {
    return canonicalPath("/bin/echo");
  }
};

TEST(PrivHelperFamTest, writesToPassedDescriptorWithoutOpeningMetadataPath) {
  TemporaryDirectory dir;
  const auto outputPath = (dir.path() / "absent.txt").string();
  int pipeFds[2];
  checkUnixError(pipe(pipeFds));
  File reader(pipeFds[0], true);
  File writer(pipeFds[1], true);
  auto request = PrivHelperConn::serializeStartFamRequest(
      1, {"/monitored"}, outputPath, outputPath, false, std::move(writer));
  folly::io::Cursor cursor{&request.data};
  PrivHelperConn::parsePacket(cursor);

  PrivHelperFamTestServer server;
  server.processStartFam(cursor, request);
  std::string output;
  const auto readOk = folly::readFile(reader.fd(), output);
  server.processStopFam();

  ASSERT_TRUE(readOk);
  EXPECT_EQ(
      "--path-prefix /monitored --events NOTIFY_OPEN NOTIFY_CLOSE\n", output);
  EXPECT_EQ(-1, access(outputPath.c_str(), F_OK));
  EXPECT_EQ(ENOENT, errno);
}

TEST(PrivHelperFamTest, rejectsMissingOutputDescriptor) {
  TemporaryDirectory dir;
  const auto outputPath = (dir.path() / "absent.txt").string();
  auto request = PrivHelperConn::serializeStartFamRequest(
      1, {"/monitored"}, outputPath, outputPath, false, File{});
  request.files.clear();
  folly::io::Cursor cursor{&request.data};
  PrivHelperConn::parsePacket(cursor);

  PrivHelperFamTestServer server;
  EXPECT_THROW_RE(
      server.processStartFam(cursor, request),
      std::runtime_error,
      "expected 1 output file descriptor");
  EXPECT_EQ(-1, access(outputPath.c_str(), F_OK));
  EXPECT_EQ(ENOENT, errno);
}

#ifdef __linux__

class PrivHelperSanityTestServer : public PrivHelperServer {
 public:
  using PrivHelperServer::bindMount;
  using PrivHelperServer::openPathAsUser;
  using PrivHelperServer::processSetFuseReadAhead;
  void registerMount(const std::string& path) {
    registerMountPoint(path);
  }

  explicit PrivHelperSanityTestServer(uid_t owner) {
    uid_ = owner;
    gid_ = getgid();
  }

  void checkMount(const std::string& path, bool byFd) {
    if (byFd) {
      openAndSanityCheckMountPoint(path, SanityCheckOptions::forTakeover());
    } else {
      sanityCheckMountPoint(path, SanityCheckOptions::forTakeover());
    }
  }
};

TEST(PrivHelperSanityTest, rootProcessChecksTheServedOwnersMount) {
  TemporaryDirectory dir;
  runInMountNamespace([&] {
    ASSERT_EQ(0, getuid());
    PrivHelperSanityTestServer server(1);
    EXPECT_THROW(server.checkMount(dir.path().string(), false), std::exception);
    EXPECT_THROW(server.checkMount(dir.path().string(), true), std::exception);

    checkUnixError(
        mount("tmpfs", dir.path().c_str(), "tmpfs", MS_RDONLY, "size=1m"));
    PrivHelperSanityTestServer rootServer(0);
    EXPECT_NO_THROW(rootServer.checkMount(dir.path().string(), false));
    EXPECT_NO_THROW(rootServer.checkMount(dir.path().string(), true));

    installPrivHelperRollbackMarker();
    EXPECT_NO_THROW(server.checkMount(dir.path().string(), false));
    EXPECT_NO_THROW(server.checkMount(dir.path().string(), true));
  });
}

TEST(PrivHelperSanityTest, takeoverRollbackUsesLegacyPathResolution) {
  TemporaryDirectory dir;
  runInMountNamespace([&] {
    File directory(dir.path().c_str(), O_PATH | O_DIRECTORY | O_CLOEXEC);
    const auto path = fmt::format("/proc/self/fd/{}", directory.fd());
    PrivHelperSanityTestServer server(getuid());
    EXPECT_THROW(server.checkMount(path, true), std::system_error);

    installPrivHelperRollbackMarker();
    EXPECT_NO_THROW(server.checkMount(path, true));
  });
}

TEST(PrivHelperSanityTest, readAheadRollbackBypassesRegistration) {
  TemporaryDirectory dir;
  runInMountNamespace([&] {
    PrivHelperSanityTestServer server(getuid());
    const auto configure = [&] {
      auto request = PrivHelperConn::serializeSetFuseReadAheadRequest(
          1, dir.path().string(), 1024);
      folly::io::Cursor cursor{&request.data};
      PrivHelperConn::parsePacket(cursor);
      server.processSetFuseReadAhead(cursor);
    };
    EXPECT_THROW_RE(configure(), std::domain_error, "No FUSE mount found");

    installPrivHelperRollbackMarker();
    EXPECT_NO_THROW(configure());

    checkUnixError(unlink(kDisablePrivHelperHardeningPath));
    EXPECT_THROW_RE(configure(), std::domain_error, "No FUSE mount found");
  });
}

TEST(PrivHelperSanityTest, userPathResolutionFollowsOrdinarySymlinks) {
  TemporaryDirectory dir;
  const auto real = dir.path() / "real";
  folly::fs::create_directory(real);
  folly::fs::create_directory(real / "leaf");
  checkUnixError(symlink("real", (dir.path() / "link").c_str()));
  PrivHelperSanityTestServer server(getuid());
  auto fd = server.openPathAsUser(
      (dir.path() / "link" / "leaf").string(), R_OK | X_OK);
  struct stat actual{}, expected{};
  checkUnixError(fstat(fd.fd(), &actual));
  checkUnixError(stat((real / "leaf").c_str(), &expected));
  EXPECT_EQ(expected.st_ino, actual.st_ino);
  EXPECT_EQ(expected.st_dev, actual.st_dev);
  EXPECT_NE(0, fcntl(fd.fd(), F_GETFL) & O_PATH);
  EXPECT_NO_THROW(server.openPathAsUser("/", R_OK | X_OK));
}

TEST(PrivHelperSanityTest, userPathResolutionRejectsAnInaccessibleAncestor) {
  if (getuid() == 0) {
    GTEST_SKIP() << "requires a non-root user";
  }
  TemporaryDirectory dir;
  const auto blocked = dir.path() / "blocked";
  folly::fs::create_directory(blocked);
  folly::fs::create_directory(blocked / "leaf");
  checkUnixError(chmod(blocked.c_str(), 0000));
  SCOPE_EXIT {
    checkUnixError(chmod(blocked.c_str(), 0700));
  };
  PrivHelperSanityTestServer server(getuid());
  EXPECT_THROW(
      server.openPathAsUser((blocked / "leaf").string(), R_OK | X_OK),
      std::system_error);
  EXPECT_EQ(getuid(), geteuid());
}

TEST(PrivHelperSanityTest, bindMountClonesAnOPathSourceDescriptor) {
  TemporaryDirectory dir;
  runInMountNamespace([&] {
    checkUnixError(mount("tmpfs", dir.path().c_str(), "tmpfs", 0, "size=1m"));
    const auto source = dir.path() / "source";
    const auto root = dir.path() / "checkout";
    const auto target = root / "redirect";
    folly::fs::create_directory(source);
    folly::fs::create_directories(target);
    ASSERT_TRUE(
        folly::writeFile(StringPiece{"content"}, (source / "file").c_str()));

    PrivHelperSanityTestServer server(getuid());
    server.registerMount(root.string());
    server.bindMount(source.c_str(), target.c_str(), root.string());

    std::string contents;
    ASSERT_TRUE(folly::readFile((target / "file").c_str(), contents));
    EXPECT_EQ("content", contents);
    struct stat sourceStat{}, targetStat{};
    checkUnixError(stat(source.c_str(), &sourceStat));
    checkUnixError(stat(target.c_str(), &targetStat));
    EXPECT_EQ(sourceStat.st_ino, targetStat.st_ino);
    EXPECT_EQ(sourceStat.st_dev, targetStat.st_dev);
  });
}

class PrivHelperCleanupTestServer : public PrivHelperServer {
 public:
  mutable size_t snapshots{0};
  std::function<void(size_t)> afterSnapshot;

  SanityCheckResult cleanup(const std::string& path) {
    File root(path.c_str(), O_PATH | O_DIRECTORY | O_CLOEXEC);
    return cleanupStaleBindMounts(path, root.fd());
  }

 private:
  std::vector<MountTableEntry> mountsForCleanup() const override {
    auto mounts = PrivHelperServer::mountsForCleanup();
    const auto snapshot = ++snapshots;
    if (afterSnapshot) {
      afterSnapshot(snapshot);
    }
    return mounts;
  }
};

TEST(PrivHelperCleanupTest, cleansNestedMountsWithoutChangingTheCheckoutMount) {
  TemporaryDirectory dir;
  runInMountNamespace([&] {
    checkUnixError(mount("tmpfs", dir.path().c_str(), "tmpfs", 0, "size=1m"));
    const auto child = dir.path() / "child";
    folly::fs::create_directory(child);
    checkUnixError(mount("tmpfs", child.c_str(), "tmpfs", 0, "size=1m"));
    const auto nested = child / "nested";
    folly::fs::create_directory(nested);
    checkUnixError(mount("tmpfs", nested.c_str(), "tmpfs", 0, "size=1m"));
    ASSERT_TRUE(
        folly::writeFile(StringPiece{"root"}, (dir.path() / "marker").c_str()));
    ASSERT_TRUE(
        folly::writeFile(StringPiece{"child"}, (child / "marker").c_str()));

    PrivHelperCleanupTestServer server;
    const auto result = server.cleanup(dir.path().string());

    EXPECT_EQ(2, result.staleRedirectionMountsFound);
    EXPECT_EQ(2, result.staleRedirectionMountsSucceeded);
    EXPECT_EQ(0, result.staleRedirectionMountsFailed);
    EXPECT_EQ(2, server.snapshots);
    EXPECT_TRUE(folly::fs::exists(dir.path() / "marker"));
    EXPECT_FALSE(folly::fs::exists(child / "marker"));
  });
}

TEST(PrivHelperCleanupTest, siblingMountsUseBoundedValidationBatches) {
  for (const int count : {16, 65}) {
    TemporaryDirectory dir;
    runInMountNamespace([&] {
      checkUnixError(mount("tmpfs", dir.path().c_str(), "tmpfs", 0, "size=1m"));
      for (int i = 0; i < count; ++i) {
        const auto child = dir.path() / fmt::format("child-{}", i);
        folly::fs::create_directory(child);
        checkUnixError(mount("tmpfs", child.c_str(), "tmpfs", 0, "size=1m"));
      }
      PrivHelperCleanupTestServer server;

      const auto result = server.cleanup(dir.path().string());

      EXPECT_EQ(count, result.staleRedirectionMountsSucceeded);
      EXPECT_EQ(0, result.staleRedirectionMountsFailed);
      EXPECT_EQ(count == 16 ? 2 : 3, server.snapshots);
    });
  }
}

TEST(PrivHelperCleanupTest, stackedChildrenArePeeledBeforeTheirParent) {
  TemporaryDirectory dir;
  runInMountNamespace([&] {
    checkUnixError(mount("tmpfs", dir.path().c_str(), "tmpfs", 0, "size=1m"));
    const auto parent = dir.path() / "parent";
    folly::fs::create_directory(parent);
    checkUnixError(mount("tmpfs", parent.c_str(), "tmpfs", 0, "size=1m"));
    const auto child = parent / "child";
    folly::fs::create_directory(child);
    for (int i = 0; i < 2; ++i) {
      checkUnixError(mount("tmpfs", child.c_str(), "tmpfs", 0, "size=1m"));
    }
    PrivHelperCleanupTestServer server;

    const auto result = server.cleanup(dir.path().string());

    EXPECT_EQ(3, result.staleRedirectionMountsSucceeded);
    EXPECT_EQ(0, result.staleRedirectionMountsFailed);
    EXPECT_EQ(3, server.snapshots);
  });
}

TEST(PrivHelperCleanupTest, peelsStackedMountsAtTheSamePath) {
  TemporaryDirectory dir;
  runInMountNamespace([&] {
    checkUnixError(mount("tmpfs", dir.path().c_str(), "tmpfs", 0, "size=1m"));
    const auto child = dir.path() / "child";
    folly::fs::create_directory(child);
    for (int i = 0; i < 2; ++i) {
      checkUnixError(mount("tmpfs", child.c_str(), "tmpfs", 0, "size=1m"));
      ASSERT_TRUE(
          folly::writeFile(StringPiece{"mounted"}, (child / "marker").c_str()));
    }

    PrivHelperCleanupTestServer server;
    const auto result = server.cleanup(dir.path().string());

    EXPECT_EQ(2, result.staleRedirectionMountsSucceeded);
    EXPECT_EQ(0, result.staleRedirectionMountsFailed);
    EXPECT_FALSE(folly::fs::exists(child / "marker"));
  });
}

TEST(PrivHelperCleanupTest, detachedFuseDoesNotRequireFilesystemAttributes) {
  if (access("/dev/fuse", R_OK | W_OK) != 0) {
    GTEST_SKIP() << "/dev/fuse is unavailable";
  }
  TemporaryDirectory dir;
  runInMountNamespace([&] {
    checkUnixError(mount("tmpfs", dir.path().c_str(), "tmpfs", 0, "size=1m"));
    const auto child = dir.path() / "child";
    folly::fs::create_directory(child);
    File fuse("/dev/fuse", O_RDWR | O_CLOEXEC);
    const auto options = fmt::format(
        "fd={},rootmode=40755,user_id={},group_id={}",
        fuse.fd(),
        getuid(),
        getgid());
    checkUnixError(mount(
        "edenfs:",
        child.c_str(),
        "fuse",
        MS_NOSUID | MS_NODEV,
        options.c_str()));
    fuse.close();
    struct stat st{};
    ASSERT_EQ(-1, stat(child.c_str(), &st));
    ASSERT_EQ(ENOTCONN, errno);

    PrivHelperCleanupTestServer server;
    const auto result = server.cleanup(dir.path().string());

    EXPECT_EQ(1, result.staleRedirectionMountsSucceeded);
    EXPECT_EQ(0, result.staleRedirectionMountsFailed);
    EXPECT_EQ(0, stat(child.c_str(), &st));
  });
}

TEST(PrivHelperCleanupTest, removedCandidateDoesNotSelectTheEnclosingMount) {
  TemporaryDirectory dir;
  runInMountNamespace([&] {
    checkUnixError(mount("tmpfs", dir.path().c_str(), "tmpfs", 0, "size=1m"));
    const auto child = dir.path() / "child";
    folly::fs::create_directory(child);
    checkUnixError(mount("tmpfs", child.c_str(), "tmpfs", 0, "size=1m"));
    ASSERT_TRUE(
        folly::writeFile(StringPiece{"root"}, (dir.path() / "marker").c_str()));
    PrivHelperCleanupTestServer server;
    server.afterSnapshot = [&](size_t snapshot) {
      if (snapshot == 1) {
        checkUnixError(umount2(child.c_str(), MNT_DETACH));
      }
    };

    const auto result = server.cleanup(dir.path().string());

    EXPECT_EQ(0, result.staleRedirectionMountsSucceeded);
    EXPECT_EQ(1, result.staleRedirectionMountsFailed);
    EXPECT_TRUE(folly::fs::exists(dir.path() / "marker"));
  });
}

TEST(PrivHelperCleanupTest, ancestorSwapAfterPinningCannotRedirectCleanup) {
  TemporaryDirectory dir;
  runInMountNamespace([&] {
    checkUnixError(mount("tmpfs", dir.path().c_str(), "tmpfs", 0, "size=1m"));
    const auto original = dir.path() / "original";
    const auto moved = dir.path() / "moved";
    const auto victim = dir.path() / "victim";
    for (const auto& root : {original, victim}) {
      folly::fs::create_directories(root / "child");
      checkUnixError(
          mount("tmpfs", (root / "child").c_str(), "tmpfs", 0, "size=1m"));
      ASSERT_TRUE(
          folly::writeFile(
              StringPiece{"mounted"}, (root / "child" / "marker").c_str()));
    }
    PrivHelperCleanupTestServer server;
    server.afterSnapshot = [&](size_t snapshot) {
      if (snapshot == 2) {
        folly::fs::rename(original, moved);
        folly::fs::create_directory_symlink(victim, original);
      }
    };

    const auto result = server.cleanup(original.string());

    EXPECT_EQ(1, result.staleRedirectionMountsSucceeded);
    EXPECT_TRUE(folly::fs::exists(victim / "child" / "marker"));
    EXPECT_FALSE(folly::fs::exists(moved / "child" / "marker"));
  });
}

TEST(PrivHelperCleanupTest, descendantSymlinkCannotSelectAnOutsideMount) {
  TemporaryDirectory dir;
  runInMountNamespace([&] {
    checkUnixError(mount("tmpfs", dir.path().c_str(), "tmpfs", 0, "size=1m"));
    const auto checkout = dir.path() / "checkout";
    const auto inside = checkout / "inside";
    const auto victim = dir.path() / "victim";
    for (const auto& root : {inside, victim}) {
      folly::fs::create_directories(root / "child");
      checkUnixError(
          mount("tmpfs", (root / "child").c_str(), "tmpfs", 0, "size=1m"));
      ASSERT_TRUE(
          folly::writeFile(
              StringPiece{"mounted"}, (root / "child" / "marker").c_str()));
    }
    PrivHelperCleanupTestServer server;
    server.afterSnapshot = [&](size_t snapshot) {
      if (snapshot == 1) {
        folly::fs::rename(inside, checkout / "moved");
        folly::fs::create_directory_symlink(victim, inside);
      }
    };

    const auto result = server.cleanup(checkout.string());

    EXPECT_EQ(0, result.staleRedirectionMountsSucceeded);
    EXPECT_EQ(1, result.staleRedirectionMountsFailed);
    EXPECT_TRUE(folly::fs::exists(victim / "child" / "marker"));
  });
}

TEST(PrivHelperSanityTest, userPathResolutionDisablesDacOverrides) {
  TemporaryDirectory dir;
  runInMountNamespace(
      [&] {
        ASSERT_EQ(1, getuid());
        checkUnixError(
            mount("tmpfs", dir.path().c_str(), "tmpfs", 0, "size=1m"));
        checkUnixError(prctl(PR_SET_SECUREBITS, SECBIT_NO_SETUID_FIXUP));
        const auto blocked = dir.path() / "blocked";
        const auto leaf = blocked / "leaf";
        folly::fs::create_directories(leaf);
        checkUnixError(chmod(blocked.c_str(), 0000));
        EXPECT_NO_THROW(File(leaf.c_str(), O_PATH | O_DIRECTORY));

        PrivHelperSanityTestServer server(getuid());
        EXPECT_THROW(
            server.openPathAsUser(leaf.string(), R_OK | X_OK),
            std::system_error);
        EXPECT_THROW(
            server.openPathAsUser(blocked.string(), R_OK | X_OK),
            std::system_error);
        EXPECT_THROW(server.checkMount(leaf.string(), true), std::system_error);

        EXPECT_NO_THROW(File(leaf.c_str(), O_PATH | O_DIRECTORY));
      },
      1);
}
#endif

class PrivHelperStaleMountTestServer : public PrivHelperServer {
 public:
  using PrivHelperServer::detectAndUnmountStaleMount;
  bool unmounted{false};
  bool simulateCachedStat{false};
  bool simulateChildError{true};
  std::function<void()> beforeChildProbe;

 private:
  int statMountPoint(const char* path, struct stat* st) const override {
    if (StringPiece{path}.endsWith("/this-file-does-not-exist")) {
      if (beforeChildProbe) {
        beforeChildProbe();
      }
      errno = simulateChildError ? ENOTCONN : ENOENT;
      return -1;
    }
    return simulateCachedStat ? 0 : ::stat(path, st);
  }

  void unmount(const char*, UnmountOptions) override {
    unmounted = true;
  }
};

TEST(PrivHelperStaleMountTest, childProbeErrorDoesNotUnmountAnUnrelatedPath) {
  TemporaryDirectory dir;
  PrivHelperStaleMountTestServer server;
  EXPECT_FALSE(
      server.detectAndUnmountStaleMount(dir.path().string(), false, false));
  EXPECT_FALSE(server.unmounted);
}

#ifdef __linux__
TEST(PrivHelperStaleMountTest, identifiesMountWithoutStatmountSyscalls) {
  TemporaryDirectory dir;
  runInMountNamespace([&] {
    sock_filter filter[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_statmount, 1, 0),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_listmount, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | ENOSYS),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    sock_fprog program{static_cast<unsigned short>(std::size(filter)), filter};
    checkUnixError(prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0));
    checkUnixError(prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &program));
    checkUnixError(mount("edenfs:", dir.path().c_str(), "tmpfs", 0, "size=1m"));

    PrivHelperStaleMountTestServer server;
    EXPECT_TRUE(
        server.detectAndUnmountStaleMount(dir.path().string(), false, false));
    EXPECT_FALSE(server.unmounted);
  });
}

TEST(PrivHelperStaleMountTest, ancestorReplacementCannotRedirectUnmount) {
  TemporaryDirectory dir;
  runInMountNamespace([&] {
    checkUnixError(mount("tmpfs", dir.path().c_str(), "tmpfs", 0, "size=1m"));
    const auto original = dir.path() / "original";
    const auto moved = dir.path() / "moved";
    const auto victim = dir.path() / "victim";
    folly::fs::create_directories(original / "mount");
    folly::fs::create_directories(victim / "mount");
    checkUnixError(
        mount("edenfs:", (original / "mount").c_str(), "tmpfs", 0, "size=1m"));
    checkUnixError(
        mount("victim", (victim / "mount").c_str(), "tmpfs", 0, "size=1m"));
    ASSERT_TRUE(
        folly::writeFile(
            StringPiece{"eden"}, (original / "mount" / "marker").c_str()));
    ASSERT_TRUE(
        folly::writeFile(
            StringPiece{"victim"}, (victim / "mount" / "marker").c_str()));

    PrivHelperStaleMountTestServer server;
    server.beforeChildProbe = [&] {
      folly::fs::rename(original, moved);
      folly::fs::create_directory_symlink(victim, original);
    };
    EXPECT_TRUE(server.detectAndUnmountStaleMount(
        (original / "mount").string(), false, false));
    EXPECT_FALSE(server.unmounted);
    EXPECT_FALSE(folly::fs::exists(moved / "mount" / "marker"));
    EXPECT_TRUE(folly::fs::exists(victim / "mount" / "marker"));
  });
}

TEST(PrivHelperStaleMountTest, unmountsDisconnectedFuseThroughDescriptor) {
  if (access("/dev/fuse", R_OK | W_OK) != 0) {
    GTEST_SKIP() << "/dev/fuse is unavailable";
  }
  TemporaryDirectory dir;
  runInMountNamespace([&] {
    for (bool cachedStat : {false, true}) {
      File connection("/dev/fuse", O_RDWR | O_CLOEXEC);
      const auto options = fmt::format(
          "fd={},rootmode=40755,user_id={},group_id={}",
          connection.fd(),
          getuid(),
          getgid());
      checkUnixError(mount(
          "edenfs:",
          dir.path().c_str(),
          "fuse",
          MS_NOSUID | MS_NODEV,
          options.c_str()));
      connection.close();
      struct stat st{};
      ASSERT_EQ(-1, stat(dir.path().c_str(), &st));
      ASSERT_EQ(ENOTCONN, errno);

      PrivHelperStaleMountTestServer server;
      server.simulateCachedStat = cachedStat;
      server.simulateChildError = false;
      EXPECT_TRUE(
          server.detectAndUnmountStaleMount(dir.path().string(), false, false));
      EXPECT_FALSE(server.unmounted);
      EXPECT_EQ(0, stat(dir.path().c_str(), &st));
    }
  });
}

TEST(PrivHelperStaleMountTest, rollbackAllowsLegacyChildProbeUnmount) {
  TemporaryDirectory dir;
  runInMountNamespace([&] {
    installPrivHelperRollbackMarker();
    PrivHelperStaleMountTestServer server;
    EXPECT_TRUE(
        server.detectAndUnmountStaleMount(dir.path().string(), false, false));
    EXPECT_TRUE(server.unmounted);
  });
}
#endif

TEST_F(PrivHelperRawProtocolTest, cleanShutdownNotificationIsNotAnswered) {
  client_->send(
      PrivHelperConn::serializeNotifyCleanShutdownRequest(/*xid=*/1, "stop"));

  // Nothing came back for the notification, so this reply is the next one on
  // the wire. Answering a one-way request would make it arrive here instead,
  // and parseEmptyResponse() rejects the mismatched type.
  auto response = client_->sendAndRecv(
      makeLegacyMacFuseConfigRequest(2, PrivHelperConn::REQ_SET_USE_EDENFS, 1));
  PrivHelperConn::parseEmptyResponse(
      PrivHelperConn::REQ_SET_USE_EDENFS, response);
}

TEST_F(PrivHelperTest, fuseMount) {
  auto mountPoint = makeTempDir("bar");
  auto path = mountPoint.path().string();

  // Prepare a promise to use as the result for trying to mount mountPoint
  auto filePromise = server_.setFuseMountResult(path);

  // Call fuseMount() this should return a future that is not ready yet,
  // since we have not fulfilled the promise.
  auto result = client_->fuseMount(path, false, "fuse");
  EXPECT_FALSE(result.isReady());

  // Create a temporary file to respond with
  TemporaryFile tempFile;
  struct stat origStat;
  checkUnixError(
      fstat(tempFile.fd(), &origStat), "failed to stat temporary file");

  // Fulfill the response.
  filePromise.setValue(File(tempFile.fd(), /* ownsFD */ false));

  // The response should complete quickly now.
  auto resultFile = std::move(result).get(1s);

  // The resulting file object should refer to the same underlying file,
  // even though the file descriptor should different since it was passed over
  // a Unix socket.
  EXPECT_NE(tempFile.fd(), resultFile.fd());
  struct stat resultStat;
  checkUnixError(
      fstat(resultFile.fd(), &resultStat), "failed to stat result file");
  EXPECT_EQ(origStat.st_dev, resultStat.st_dev);
  EXPECT_EQ(origStat.st_ino, resultStat.st_ino);

  auto vfsTypes = server_.getRequestedVfsTypes();
  EXPECT_EQ(1, vfsTypes.size());
  EXPECT_EQ("fuse", vfsTypes.at(0));

  // When we shut down the privhelper server it remembers that /foo/bar was
  // unmounted and will try to unmount it.  This will fail since we have not
  // registered a response for the unmount.  This will cause an error message to
  // be logged, but this is fine.
  //
  // We could register a result for the unmount operation here, but seems nice
  // for now to test that the privhelper server gracefully handles the exception
  // from the unmount operation.
}

TEST_F(PrivHelperTest, stalledRequestIsLoggedAndStillSucceeds) {
  auto mountPoint = makeTempDir("bar");
  auto path = mountPoint.path().string();

  client_->setRequestStallThresholdForTest(50ms);

  auto filePromise = server_.setFuseMountResult(path);
  auto result = client_->fuseMount(path, false, "fuse");
  EXPECT_FALSE(result.isReady());

  // Hold the response until the stall watchdog has fired and recorded its
  // event. The watchdog is scheduled for 50ms; the generous timeout only
  // bounds how long we wait on a starved host.
  ASSERT_TRUE(xplatLogger_->waitForEvent(10s));

  TemporaryFile tempFile;
  filePromise.setValue(File(tempFile.fd(), /* ownsFD */ false));

  // The stall watchdog is log-only: the request must still succeed.
  auto resultFile = std::move(result).get(1s);
  EXPECT_GE(resultFile.fd(), 0);

  auto events = xplatLogger_->getEvents();
  ASSERT_EQ(1u, events.size());
  EXPECT_EQ(std::string{xplat_keys::kEventsCategory}, events[0].first);
  const auto& strings = events[0].second.getStringMap();
  EXPECT_EQ(
      "privhelper_request_stall", strings.at(std::string{xplat_keys::kType}));
  EXPECT_EQ("fuse_mount", strings.at(std::string{xplat_keys::kMethod}));
  const auto& doubles = events[0].second.getDoubleMap();
  EXPECT_GE(doubles.at(std::string{xplat_keys::kDuration}), 0.05);
}

TEST_F(PrivHelperTest, fuseMountCustomVfsType) {
  auto mountPoint = makeTempDir("bar");
  auto path = mountPoint.path().string();

  // Prepare a promise to use as the result for trying to mount mountPoint
  auto filePromise = server_.setFuseMountResult(path);

  // Call fuseMount() this should return a future that is not ready yet,
  // since we have not fulfilled the promise.
  auto result = client_->fuseMount(path, false, "fuse.edenfs");
  EXPECT_FALSE(result.isReady());

  // Create a temporary file to respond with
  TemporaryFile tempFile;
  struct stat origStat;
  checkUnixError(
      fstat(tempFile.fd(), &origStat), "failed to stat temporary file");

  // Fulfill the response.
  filePromise.setValue(File(tempFile.fd(), /* ownsFD */ false));

  // The response should complete quickly now.
  auto resultFile = std::move(result).get(1s);

  // The resulting file object should refer to the same underlying file,
  // even though the file descriptor should different since it was passed over
  // a Unix socket.
  EXPECT_NE(tempFile.fd(), resultFile.fd());
  struct stat resultStat;
  checkUnixError(
      fstat(resultFile.fd(), &resultStat), "failed to stat result file");
  EXPECT_EQ(origStat.st_dev, resultStat.st_dev);
  EXPECT_EQ(origStat.st_ino, resultStat.st_ino);

  auto vfsTypes = server_.getRequestedVfsTypes();
  EXPECT_EQ(1, vfsTypes.size());
  EXPECT_EQ("fuse.edenfs", vfsTypes.at(0));

  // When we shut down the privhelper server it remembers that /foo/bar was
  // unmounted and will try to unmount it.  This will fail since we have not
  // registered a response for the unmount.  This will cause an error message to
  // be logged, but this is fine.
  //
  // We could register a result for the unmount operation here, but seems nice
  // for now to test that the privhelper server gracefully handles the exception
  // from the unmount operation.
}

TEST_F(PrivHelperTest, fuseMountPermissions) {
  if (!folly::kIsApple && getuid() != 0) {
    auto path = "/root/bar";
    EXPECT_THROW_RE(
        client_->fuseMount(path, false, "fuse").get(),
        std::exception,
        folly::to<std::string>(
            "std::domain_error: User:",
            getuid(),
            " cannot open ",
            path,
            ": Permission denied"));
  }
}

TEST_F(PrivHelperTest, fuseMountError) {
  auto tempdir = makeTempDir();
  auto path = tempdir.path().string();
  // Test calling fuseMount() with a mount path that is not registered.
  // This will throw an error in the privhelper server thread.  Make sure the
  // error message is raised in the client correctly.
  EXPECT_THROW_RE(
      client_->fuseMount(path, false, "fuse").get(),
      std::exception,
      fmt::format("no result available for {}", path));
}

TEST_F(PrivHelperTest, multiplePendingFuseMounts) {
  auto abcMountPoint = makeTempDir("abc");
  auto abcPath = abcMountPoint.path().string();
  auto defMountPoint = makeTempDir("def");
  auto defPath = defMountPoint.path().string();
  auto barMountPoint = makeTempDir("bar");
  auto barPath = barMountPoint.path().string();

  // Prepare several promises for various mount points
  auto abcPromise = server_.setFuseMountResult(abcPath);
  auto defPromise = server_.setFuseMountResult(defPath);
  auto barPromise = server_.setFuseMountResult(barPath);

  // Also set up unmount results for when the privhelper tries to unmount these
  // mount points during cleanup.
  server_.setFuseUnmountResult(abcPath).setValue();
  server_.setFuseUnmountResult(defPath).setValue();
  server_.setFuseUnmountResult(barPath).setValue();

  // Make several fuseMount() calls
  auto abcResult = client_->fuseMount(abcPath, false, "fuse");
  auto defResult = client_->fuseMount(defPath, false, "fuse");
  auto foobarResult = client_->fuseMount(barPath, false, "fuse");
  EXPECT_FALSE(abcResult.isReady());
  EXPECT_FALSE(defResult.isReady());
  EXPECT_FALSE(foobarResult.isReady());

  // Fulfill the response promises
  // We fulfill them in a different order than the order of the requests here.
  // This shouldn't affect the behavior of the code.
  TemporaryFile tempFile;
  barPromise.setValue(File(tempFile.fd(), /* ownsFD */ false));
  abcPromise.setValue(File(tempFile.fd(), /* ownsFD */ false));
  defPromise.setValue(File(tempFile.fd(), /* ownsFD */ false));

  // The responses should be available in the client now.
  auto results =
      folly::collectUnsafe(abcResult, defResult, foobarResult).get(1s);
  (void)results;

  // Destroy the privhelper
  cleanup();

  // All of the unmount results should have been used.
  EXPECT_THAT(server_.getUnusedFuseUnmountResults(), UnorderedElementsAre());
}

TEST_F(PrivHelperTest, bindMounts) {
  auto abcMountPoint = makeTempDir("abc");
  auto abcPath = abcMountPoint.path().string();
  TemporaryFile tempFile;

  folly::fs::create_directory(abcMountPoint.path() / "foo");
  folly::fs::create_directory(abcMountPoint.path() / "bar");
  folly::fs::create_directory(abcMountPoint.path() / "buck-out");
  folly::fs::create_directory(abcMountPoint.path() / "foo" / "buck-out");
  folly::fs::create_directory(abcMountPoint.path() / "bar" / "buck-out");

  // Prepare promises for the mount calls
  server_.setFuseMountResult(abcPath).setValue(File(tempFile.fd(), false));
  server_.setBindMountResult(abcPath + "/buck-out").setValue();
  server_.setBindMountResult(abcPath + "/foo/buck-out").setValue();
  server_.setBindMountResult(abcPath + "/bar/buck-out").setValue();

  auto userMountPoint = makeTempDir("user");
  auto userPath = userMountPoint.path().string();

  folly::fs::create_directory(userMountPoint.path() / "somerepo");
  folly::fs::create_directory(userMountPoint.path() / "somerepo" / "buck-out");

  server_.setFuseMountResult(userPath + "/somerepo")
      .setValue(File(tempFile.fd(), false));
  server_.setBindMountResult(userPath + "/somerepo/buck-out").setValue();

  folly::fs::create_directory(userMountPoint.path() / "somerepo2");
  server_.setFuseMountResult(userPath + "/somerepo2")
      .setValue(File(tempFile.fd(), false));

  // Prepare promises for the unmount calls
  server_.setFuseUnmountResult(abcPath).setValue();
  server_.setBindUnmountResult(abcPath + "/buck-out").setValue();
  server_.setBindUnmountResult(abcPath + "/foo/buck-out").setValue();
  server_.setBindUnmountResult(abcPath + "/bar/buck-out").setValue();
  server_.setFuseUnmountResult(userPath + "/somerepo").setValue();
  server_.setFuseUnmountResult(userPath + "/somerepo2").setValue();
  // Leave the promise for somerepo/buck-out unfulfilled for now.
  auto somerepoBuckOutUnmountPromise =
      server_.setBindUnmountResult(userPath + "/somerepo/buck-out");

  // Prepare some extra unmount promises that we don't expect to be used,
  // just to verify that cleanup happens as expected.
  server_.setFuseUnmountResult("/never/actually/mounted").setValue();
  server_.setBindUnmountResult("/bind/never/actually/mounted").setValue();

  // Mount everything
  client_->fuseMount(userPath + "/somerepo", false, "fuse").get(1s);
  client_->bindMount("/bind/mount/source", userPath + "/somerepo/buck-out")
      .get(1s);

  client_->fuseMount(abcPath, false, "fuse").get(1s);
  client_->bindMount("/bind/mount/source", abcPath + "/buck-out").get(1s);
  client_->bindMount("/bind/mount/source", abcPath + "/foo/buck-out").get(1s);
  client_->fuseMount(userPath + "/somerepo2", false, "fuse").get(1s);
  client_->bindMount("/bind/mount/source", abcPath + "/bar/buck-out").get(1s);

  // Manually unmount /somerepo.
  // This will finish even though somerepoBuckOutUnmountPromise is still
  // outstanding because the privhelper and the OS don't care about relative
  // ordering of these two operations.
  auto unmountResult = client_->fuseUnmount(userPath + "/somerepo", {});
  std::move(unmountResult).get(1s);

  // Clean up this promise: no one is waiting on its results, but we just
  // want to make sure that it doesn't generate a BrokenPromise error
  // when the destructors run.
  somerepoBuckOutUnmountPromise.setValue();

  // Now shut down the privhelper.  It should clean up the remaining mount
  // points.  The only leftover results should be the extra ones we
  // intentionally added.
  cleanup();
  EXPECT_THAT(
      server_.getUnusedFuseUnmountResults(),
      UnorderedElementsAre("/never/actually/mounted"));
  EXPECT_THAT(
      server_.getUnusedBindUnmountResults(),
      UnorderedElementsAre("/bind/never/actually/mounted"));
}

TEST_F(PrivHelperTest, bindMountRejectsEscapedMountPath) {
#ifdef __APPLE__
  GTEST_SKIP() << "Linux-specific openat2 validation";
#else
  auto mountRoot = makeTempDir("mount-root");
  auto source = makeTempDir("source");
  folly::fs::create_directory(mountRoot.path() / "registered");
  folly::fs::create_directory(mountRoot.path() / "target");

  const auto registeredPath = (mountRoot.path() / "registered").string();
  const auto escapedPath = registeredPath + "/../target";

  client_->takeoverStartup(registeredPath, {}).get(1s);
  server_.setFuseUnmountResult(registeredPath).setValue();

  server_.setBindMountResult(escapedPath).setValue();
  EXPECT_THROW_RE(
      client_->bindMount(source.path().string(), escapedPath).get(1s),
      std::exception,
      "Invalid cross-device link");

  server_.setBindUnmountResult(escapedPath).setValue();
  EXPECT_THROW_RE(
      client_->bindUnMount(escapedPath).get(1s),
      std::exception,
      "Invalid cross-device link");

#endif
}

/*
 * The privhelper control descriptor arrives across an exec without
 * FD_CLOEXEC; startOrConnectToPrivHelper has to restore it.
 */
TEST(PrivHelperCloExec, adoptedClientDescriptorIsCloseOnExec) {
  folly::File clientConn;
  folly::File serverConn;
  PrivHelperConn::createConnPair(clientConn, serverConn);

  // Reproduce what an exec leaves behind: the flag cleared.
  const int fd = clientConn.fd();
  folly::checkUnixError(fcntl(fd, F_SETFD, 0), "clearing FD_CLOEXEC");
  ASSERT_EQ(0, fcntl(fd, F_GETFD) & FD_CLOEXEC)
      << "test setup failed to clear FD_CLOEXEC";

  const auto fdArg = folly::to<std::string>(fd);
  std::vector<const char*> argv{"edenfs", "--privhelper_fd", fdArg.c_str()};
  auto helper = startOrConnectToPrivHelper(
      UserInfo::lookup(),
      static_cast<int>(argv.size()),
      const_cast<char**>(argv.data()));
  ASSERT_NE(nullptr, helper);

  EXPECT_NE(0, fcntl(fd, F_GETFD) & FD_CLOEXEC)
      << "the adopted privhelper descriptor is inheritable; it will leak into "
         "every child EdenFS spawns and delay the privhelper's EOF";

  // startOrConnectToPrivHelper took ownership of the descriptor.
  clientConn.release();
}

TEST_F(PrivHelperTest, bindMountRejectsSymlinkComponent) {
#ifdef __APPLE__
  GTEST_SKIP() << "Linux-specific openat2 validation";
#else
  auto mountRoot = makeTempDir("mount-root");
  auto source = makeTempDir("source");
  auto outside = makeTempDir("outside");
  folly::fs::create_directory(mountRoot.path() / "registered");
  folly::fs::create_directories(outside.path() / "target");
  folly::fs::create_directory_symlink(
      outside.path(), mountRoot.path() / "registered" / "link");

  const auto registeredPath = (mountRoot.path() / "registered").string();
  const auto symlinkPath = registeredPath + "/link/target";

  client_->takeoverStartup(registeredPath, {}).get(1s);
  server_.setFuseUnmountResult(registeredPath).setValue();

  server_.setBindMountResult(symlinkPath).setValue();
  EXPECT_THROW_RE(
      client_->bindMount(source.path().string(), symlinkPath).get(1s),
      std::exception,
      "Too many levels of symbolic links");

  server_.setBindUnmountResult(symlinkPath).setValue();
  EXPECT_THROW_RE(
      client_->bindUnMount(symlinkPath).get(1s),
      std::exception,
      "Too many levels of symbolic links");

#endif
}

TEST_F(PrivHelperTest, bindMountRejectsSymlinkHiddenByDotDot) {
#ifdef __APPLE__
  GTEST_SKIP() << "Linux-specific openat2 validation";
#else
  auto mountRoot = makeTempDir("mount-root");
  auto source = makeTempDir("source");
  auto outside = makeTempDir("outside");
  folly::fs::create_directory(mountRoot.path() / "registered");
  folly::fs::create_directory(mountRoot.path() / "registered" / "target");
  folly::fs::create_directories(outside.path() / "child");
  folly::fs::create_directory(outside.path() / "target");
  folly::fs::create_directory_symlink(
      outside.path() / "child", mountRoot.path() / "registered" / "link");

  const auto registeredPath = (mountRoot.path() / "registered").string();
  const auto symlinkDotDotPath = registeredPath + "/link/../target";

  client_->takeoverStartup(registeredPath, {}).get(1s);
  server_.setFuseUnmountResult(registeredPath).setValue();

  server_.setBindMountResult(symlinkDotDotPath).setValue();
  EXPECT_THROW_RE(
      client_->bindMount(source.path().string(), symlinkDotDotPath).get(1s),
      std::exception,
      "Too many levels of symbolic links");

  server_.setBindUnmountResult(symlinkDotDotPath).setValue();
  EXPECT_THROW_RE(
      client_->bindUnMount(symlinkDotDotPath).get(1s),
      std::exception,
      "Too many levels of symbolic links");

#endif
}

TEST_F(PrivHelperTest, bindMountAllowsSymlinkAncestorOfRegisteredMount) {
#ifdef __APPLE__
  GTEST_SKIP() << "Linux-specific openat2 validation";
#else
  auto realRoot = makeTempDir("real-root");
  auto linkRoot = makeTempDir("link-root");
  auto source = makeTempDir("source");
  folly::fs::create_directories(realRoot.path() / "registered");
  folly::fs::create_directories(realRoot.path() / "registered" / "redirected");
  folly::fs::create_directory_symlink(
      realRoot.path(), linkRoot.path() / "link");

  const auto registeredPath =
      (linkRoot.path() / "link" / "registered").string();
  const auto bindPath = registeredPath + "/redirected";

  client_->takeoverStartup(registeredPath, {}).get(1s);
  server_.setFuseUnmountResult(registeredPath).setValue();
  server_.setBindMountResult(bindPath).setValue();

  client_->bindMount(source.path().string(), bindPath).get(1s);
#endif
}

TEST_F(
    PrivHelperTest,
    bindMountRejectsRetargetedSymlinkAncestorOfRegisteredMount) {
#ifdef __APPLE__
  GTEST_SKIP() << "Linux-specific openat2 validation";
#else
  auto realRoot = makeTempDir("real-root");
  auto alternateRoot = makeTempDir("alternate-root");
  auto linkRoot = makeTempDir("link-root");
  auto source = makeTempDir("source");
  folly::fs::create_directories(realRoot.path() / "registered");
  folly::fs::create_directories(
      alternateRoot.path() / "registered" / "redirected");
  folly::fs::create_directory_symlink(
      realRoot.path(), linkRoot.path() / "link");

  const auto registeredPath =
      (linkRoot.path() / "link" / "registered").string();
  const auto bindPath = registeredPath + "/redirected";

  client_->takeoverStartup(registeredPath, {}).get(1s);
  server_.setFuseUnmountResult(registeredPath).setValue();

  folly::fs::remove(linkRoot.path() / "link");
  folly::fs::create_directory_symlink(
      alternateRoot.path(), linkRoot.path() / "link");

  server_.setBindMountResult(bindPath).setValue();
  EXPECT_THROW_RE(
      client_->bindMount(source.path().string(), bindPath).get(1s),
      std::exception,
      "No such file or directory");
#endif
}

TEST_F(PrivHelperTest, takeoverKeepsValidatedRootWhenAncestorIsReplaced) {
#ifdef __APPLE__
  GTEST_SKIP() << "Linux-specific registered mount descriptors";
#else
  if (getuid() == 0) {
    GTEST_SKIP() << "root bypasses mount ownership validation";
  }
  auto root = makeTempDir("takeover-root");
  auto source = makeTempDir("source");
  const auto ancestor = root.path() / "ancestor";
  const auto moved = root.path() / "moved";
  const auto replacement = root.path() / "replacement";
  folly::fs::create_directories(ancestor / "registered" / "original-only");
  folly::fs::create_directories(
      replacement / "registered" / "replacement-only");

  const auto registeredPath = (ancestor / "registered").string();
  server_.setAfterSanityCheck([ancestor, moved, replacement] {
    folly::fs::rename(ancestor, moved);
    folly::fs::create_directory_symlink(replacement, ancestor);
  });
  client_->takeoverStartup(registeredPath, {}).get(1s);
  server_.setFuseUnmountResult(registeredPath).setValue();

  const auto originalPath = registeredPath + "/original-only";
  server_.setBindMountResult(originalPath).setValue();
  EXPECT_NO_THROW(
      client_->bindMount(source.path().string(), originalPath).get(1s));

  const auto replacementPath = registeredPath + "/replacement-only";
  server_.setBindMountResult(replacementPath).setValue();
  EXPECT_THROW_RE(
      client_->bindMount(source.path().string(), replacementPath).get(1s),
      std::exception,
      "No such file or directory");
#endif
}

TEST_F(PrivHelperFdUnmountTest, bindUnmountAllowsAlreadyUnmountedTarget) {
#ifdef __APPLE__
  GTEST_SKIP() << "Linux-specific procfd bind unmount validation";
#else
  auto mountRoot = makeTempDir("mount-root");
  folly::fs::create_directory(mountRoot.path() / "registered");
  folly::fs::create_directory(mountRoot.path() / "registered" / "not-mounted");

  const auto registeredPath = (mountRoot.path() / "registered").string();
  const auto notMountedPath = registeredPath + "/not-mounted";

  client_->takeoverStartup(registeredPath, {}).get(1s);

  client_->bindUnMount(notMountedPath).get(1s);
  EXPECT_FALSE(server_.insecureBindUnmountCalled());
#endif
}

TEST_F(PrivHelperFdUnmountTest, bindUnmountErrorsDoNotRestorePathAuthority) {
#ifdef __APPLE__
  GTEST_SKIP() << "Linux-specific procfd bind unmount validation";
#else
  auto root = makeTempDir();
  folly::fs::create_directory(root.path() / "child");
  const auto path = root.path().string();
  client_->takeoverStartup(path, {}).get(1s);

  for (const auto error : {EIO, ENOENT, ENOTCONN}) {
    server_.setUnmountError(error);
    EXPECT_THROW(
        client_->bindUnMount(path + "/child").get(1s), std::system_error);
  }
  EXPECT_EQ(3, server_.fdUnmountCalls());
  EXPECT_FALSE(server_.insecureBindUnmountCalled());
#endif
}

#ifdef __linux__
class PrivHelperRegisteredUnmountTestServer : public PrivHelperServer {
 public:
  void registerMount(const std::string& path) {
    registerMountPoint(path);
  }
};

TEST(PrivHelperUnmountTest, disconnectedMountKeepsAuthorityAfterAncestorSwap) {
  if (access("/dev/fuse", R_OK | W_OK) != 0) {
    GTEST_SKIP() << "/dev/fuse is unavailable";
  }
  TemporaryDirectory dir;
  runInMountNamespace([&] {
    checkUnixError(mount("tmpfs", dir.path().c_str(), "tmpfs", 0, "size=1m"));
    const auto original = dir.path() / "original";
    const auto moved = dir.path() / "moved";
    const auto victim = dir.path() / "victim";
    folly::fs::create_directories(original / "mount");
    folly::fs::create_directories(victim / "mount");
    checkUnixError(
        mount("victim", (victim / "mount").c_str(), "tmpfs", 0, "size=1m"));
    ASSERT_TRUE(
        folly::writeFile(
            StringPiece{"victim"}, (victim / "mount" / "marker").c_str()));
    File fuse("/dev/fuse", O_RDWR | O_CLOEXEC);
    const auto options = fmt::format(
        "fd={},rootmode=40755,user_id={},group_id={}",
        fuse.fd(),
        getuid(),
        getgid());
    checkUnixError(mount(
        "edenfs:",
        (original / "mount").c_str(),
        "fuse",
        MS_NOSUID | MS_NODEV,
        options.c_str()));

    File clientConn;
    File serverConn;
    PrivHelperConn::createConnPair(clientConn, serverConn);
    PrivHelperRegisteredUnmountTestServer server;
    const auto path = (original / "mount").string();
    server.registerMount(path);
    std::thread thread([&server, conn = std::move(serverConn)]() mutable {
      server.initPartial(std::move(conn), getuid(), getgid());
      server.run();
    });
    SCOPE_EXIT {
      thread.join();
    };
    RawPrivHelperClient client(std::move(clientConn));
    fuse.close();
    folly::fs::rename(original, moved);
    folly::fs::create_directory_symlink(victim, original);

    auto response = client.sendAndRecv(
        PrivHelperConn::serializeUnmountRequest(2, path, {}));
    PrivHelperConn::parseEmptyResponse(
        PrivHelperConn::REQ_UNMOUNT_FUSE, response);
    EXPECT_TRUE(folly::fs::exists(victim / "mount" / "marker"));
    struct stat st{};
    EXPECT_EQ(0, stat((moved / "mount").c_str(), &st));
  });
}
#endif

TEST_F(PrivHelperTest, takeoverShutdown) {
  auto abcMountPoint = makeTempDir("abc");
  auto abcPath = abcMountPoint.path().string();
  TemporaryFile tempFile;

  folly::fs::create_directory(abcMountPoint.path() / "foo");
  folly::fs::create_directory(abcMountPoint.path() / "bar");
  folly::fs::create_directory(abcMountPoint.path() / "buck-out");
  folly::fs::create_directory(abcMountPoint.path() / "foo" / "buck-out");
  folly::fs::create_directory(abcMountPoint.path() / "bar" / "buck-out");

  // Prepare promises for the mount calls
  server_.setFuseMountResult(abcPath).setValue(File(tempFile.fd(), false));
  server_.setBindMountResult(abcPath + "/buck-out").setValue();
  server_.setBindMountResult(abcPath + "/foo/buck-out").setValue();
  server_.setBindMountResult(abcPath + "/bar/buck-out").setValue();

  auto userMountPoint = makeTempDir("user");
  auto userPath = userMountPoint.path().string();

  folly::fs::create_directory(userMountPoint.path() / "somerepo");

  server_.setFuseMountResult(userPath + "/somerepo")
      .setValue(File(tempFile.fd(), false));

  folly::fs::create_directory(userMountPoint.path() / "somerepo2");
  folly::fs::create_directory(userMountPoint.path() / "somerepo2" / "buck-out");
  server_.setFuseMountResult(userPath + "/somerepo2")
      .setValue(File(tempFile.fd(), false));
  server_.setBindMountResult(userPath + "/somerepo2/buck-out").setValue();

  // Set up unmount promises
  server_.setFuseUnmountResult(abcPath).setValue();
  server_.setBindUnmountResult(abcPath + "/buck-out").setValue();
  server_.setBindUnmountResult(abcPath + "/foo/buck-out").setValue();
  server_.setBindUnmountResult(abcPath + "/bar/buck-out").setValue();
  server_.setFuseUnmountResult(userPath + "/somerepo").setValue();
  server_.setFuseUnmountResult(userPath + "/somerepo2").setValue();
  server_.setBindUnmountResult(userPath + "/somerepo2/buck-out").setValue();

  // Mount everything
  client_->fuseMount(abcPath, false, "fuse").get(1s);
  client_->bindMount("/bind/mount/source", abcPath + "/buck-out").get(1s);
  client_->bindMount("/bind/mount/source", abcPath + "/foo/buck-out").get(1s);
  client_->bindMount("/bind/mount/source", abcPath + "/bar/buck-out").get(1s);
  client_->fuseMount(userPath + "/somerepo", false, "fuse").get(1s);
  client_->fuseMount(userPath + "/somerepo2", false, "fuse").get(1s);
  client_->bindMount("/bind/mount/source", userPath + "/somerepo2/buck-out")
      .get(1s);

  // Indicate that /mnt/abc and /mnt/somerepo are being taken over.
  client_->takeoverShutdown(abcPath).get(1s);
  client_->takeoverShutdown(userPath + "/somerepo").get(1s);

  // Destroy the privhelper.
  // /mnt/somerepo2 should be unmounted, but /mnt/abc and /mnt/somerepo
  // should not be.
  cleanup();

  EXPECT_THAT(
      server_.getUnusedFuseUnmountResults(),
      UnorderedElementsAre(abcPath, userPath + "/somerepo"));
  EXPECT_THAT(
      server_.getUnusedBindUnmountResults(),
      UnorderedElementsAre(
          abcPath + "/buck-out",
          abcPath + "/foo/buck-out",
          abcPath + "/bar/buck-out"));
}

TEST_F(PrivHelperTest, takeoverStartup) {
  auto abcMountPoint = makeTempDir("abc");
  auto abcPath = abcMountPoint.path().string();
  TemporaryFile tempFile;

  folly::fs::create_directories(abcMountPoint.path() / "foo" / "buck-out");
  folly::fs::create_directories(
      abcMountPoint.path() / "xyz" / "test" / "buck-out");

  // Indicate that we are taking over some mount points.
  client_
      ->takeoverStartup(
          abcPath, {abcPath + "/foo/buck-out", abcPath + "/xyz/test/buck-out"})
      .get(1s);

  auto myrepoMountPoint = makeTempDir("myrepo");
  auto myrepoPath = myrepoMountPoint.path().string();
  client_->takeoverStartup(myrepoPath, {}).get(1s);

  auto repoXMountPoint = makeTempDir("repo_x");
  auto repoXPath = repoXMountPoint.path().string();
  client_->takeoverStartup(repoXPath, {repoXPath + "/y"}).get(1s);

  // Manually mount one other mount point.
  auto xyzMountPoint = makeTempDir("xyz");
  auto xyzPath = xyzMountPoint.path().string();
  folly::fs::create_directory(xyzMountPoint.path() / "buck-out");
  server_.setFuseMountResult(xyzPath).setValue(File(tempFile.fd(), false));
  server_.setBindMountResult(xyzPath + "/buck-out").setValue();
  client_->fuseMount(xyzPath, false, "fuse").get(1s);
  client_->bindMount("/bind/mount/source", xyzPath + "/buck-out").get(1s);

  // Manually unmount /mnt/repo_x
  server_.setFuseUnmountResult(repoXPath).setValue();
  server_.setBindUnmountResult(repoXPath + "/y").setValue();
  client_->fuseUnmount(repoXPath, {}).get(1s);
  EXPECT_THAT(server_.getUnusedFuseUnmountResults(), UnorderedElementsAre());
  EXPECT_THAT(server_.getUnusedBindUnmountResults(), UnorderedElementsAre());

  // Re-register the unmount results for repo_x just to confirm that they are
  // not reused on shutdown.
  server_.setFuseUnmountResult(repoXPath).setValue();
  server_.setBindUnmountResult(repoXPath + "/y").setValue();

  // Register results for the other unmount operations that should occur.
  server_.setFuseUnmountResult(abcPath).setValue();
  server_.setBindUnmountResult(abcPath + "/foo/buck-out").setValue();
  server_.setBindUnmountResult(abcPath + "/xyz/test/buck-out").setValue();
  server_.setFuseUnmountResult(xyzPath).setValue();
  server_.setBindUnmountResult(xyzPath + "/buck-out").setValue();
  server_.setFuseUnmountResult(myrepoPath).setValue();

  // Shut down the privhelper.  It should unmount the registered mount points.
  cleanup();
  EXPECT_THAT(
      server_.getUnusedFuseUnmountResults(), UnorderedElementsAre(repoXPath));
  EXPECT_THAT(
      server_.getUnusedBindUnmountResults(),
      UnorderedElementsAre(repoXPath + "/y"));
}

TEST_F(PrivHelperTest, detachEventBase) {
  auto barMountPoint = makeTempDir("bar");
  auto barPath = barMountPoint.path().string();
  // Perform one call using the current EventBase
  TemporaryFile tempFile;
  auto filePromise = server_.setFuseMountResult(barPath);
  auto result = client_->fuseMount(barPath, false, "fuse");
  EXPECT_FALSE(result.isReady());
  filePromise.setValue(File(tempFile.fd(), /* ownsFD */ false));
  auto resultFile = std::move(result).get(1s);

  // Detach the PrivHelper from the clientIoThread_'s EventBase, and perform a
  // call using a separate local EventBase
  clientIoThread_.getEventBase()->runInEventBaseThreadAndWait(
      [&] { client_->detachEventBase(); });

  {
    EventBase evb;
    client_->attachEventBase(&evb);

    auto newMountPoint = makeTempDir("new");
    auto newPath = newMountPoint.path().string();

    filePromise = server_.setFuseMountResult(newPath);
    server_.setFuseUnmountResult(newPath).setValue();
    result = client_->fuseMount(newPath, false, "fuse");
    // The result should not be immediately ready since we have not fulfilled
    // the promise yet.  It will only be ready if something unexpected failed.
    if (result.isReady()) {
      ADD_FAILURE() << "unmount request was immediately ready";
      // Call get() so it will throw if the command failed.
      std::move(result).get();
      return;
    }

    bool success = false;
    std::move(result)
        .thenValue([&success](folly::File) { success = true; })
        .ensure([&evb] { evb.terminateLoopSoon(); });

    filePromise.setValue(File(tempFile.fd(), /* ownsFD */ false));
    evb.loopForever();
    EXPECT_TRUE(success);

    // The PrivHelper will be automatically detached from this EventBase
    // when it goes out of scope and is destroyed.
  }

  // Re-attach the PrivHelper to the clientIoThread_'s EventBase
  clientIoThread_.getEventBase()->runInEventBaseThreadAndWait(
      [&] { client_->attachEventBase(clientIoThread_.getEventBase()); });

  // Perform another call with the clientIoThread_ EventBase
  auto unmountPromise = server_.setFuseUnmountResult(barPath);
  auto unmountResult = client_->fuseUnmount(barPath, {});
  EXPECT_FALSE(unmountResult.isReady());
  unmountPromise.setValue();
  std::move(unmountResult).get(1s);
}

TEST_F(PrivHelperTest, setLogFile) {
  // Call setLogFile()
  TemporaryFile tempFile0;
  client_->setLogFile(File{tempFile0.fd(), /* ownsFD */ false}).get(1s);

  // Detach from the clientIoThread_ and call all setLogFileBlocking()
  TemporaryFile tempFile1;
  clientIoThread_.getEventBase()->runInEventBaseThreadAndWait(
      [&] { client_->detachEventBase(); });
  client_->setLogFileBlocking(File{tempFile1.fd(), /* ownsFD */ false});

  // Confirm that the server received both requests
  auto logFiles = server_.getLogFileRequests();
  ASSERT_EQ(2, logFiles.size());

  struct stat s1;
  folly::checkUnixError(fstat(logFiles[0].fd(), &s1));
  struct stat s2;
  folly::checkUnixError(fstat(tempFile0.fd(), &s2));
  EXPECT_EQ(s1.st_dev, s2.st_dev);
  EXPECT_EQ(s1.st_ino, s2.st_ino);

  folly::checkUnixError(fstat(logFiles[1].fd(), &s1));
  folly::checkUnixError(fstat(tempFile1.fd(), &s2));
  EXPECT_EQ(s1.st_dev, s2.st_dev);
  EXPECT_EQ(s1.st_ino, s2.st_ino);
}

TEST(PrivHelperSessionTest, detachesFromParentProcessGroup) {
  // The privhelper binary calls detachFromParentProcessGroup() at startup
  // so that killing the process group it was spawned into (as agent
  // command runners do when cleaning up an `eden restart` invocation)
  // cannot take the privhelper down with it. Verify in a forked child
  // that the call moves the process into its own session and group.
  // The child only makes raw syscalls, so forking with test threads
  // running is safe.
  pid_t childPid = fork();
  folly::checkUnixError(childPid, "fork failed");
  if (childPid == 0) {
    if (getpgid(0) == getpid()) {
      // The child must start out in its parent's process group for this
      // test to prove anything.
      _exit(2);
    }
    detachFromParentProcessGroup();
    if (getpgid(0) != getpid()) {
      _exit(3);
    }
    if (getsid(0) != getpid()) {
      _exit(4);
    }
    _exit(0);
  }
  int status = 0;
  folly::checkUnixError(waitpid(childPid, &status, 0), "waitpid failed");
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST_F(PrivHelperTest, cleanShutdownNotificationLeavesTheConnectionUsable) {
  client_->notifyCleanShutdown("stop");

  // A privhelper that does not act on this request -- every Linux one, and any
  // build too old to know the type -- must still not reply to it. A later
  // request completing proves nothing crashed and the stream is intact.
  EXPECT_EQ(getpid(), std::move(client_->getServerPid()).get(1s));
}

TEST(
    PrivHelperClientLifetime,
    destroyingTheClientLeavesRequestsQueuedOnItsEventBaseSafeToRun) {
  File clientConn;
  File serverConn;
  PrivHelperConn::createConnPair(clientConn, serverConn);

  PrivHelperThreadedTestServer server;
  std::thread serverThread(
      [&server, conn = std::move(serverConn)]() mutable noexcept {
        server.initPartial(std::move(conn), getuid(), getgid());
        server.run();
      });

  // Declared before the client so that it outlives it, leaving the request
  // queued against a client that is already gone.
  EventBase eventBase;
  auto pid = Future<pid_t>::makeEmpty();
  {
    auto client = createTestPrivHelper(std::move(clientConn));
    client->attachEventBase(&eventBase);
    pid = client->getServerPid();
  }

  // Destroying the client does not drain the queue, and does not have to: the
  // request holds the session alive, so running it late is defined.
  EXPECT_EQ(1u, eventBase.getNotificationQueueSize());

  eventBase.loopOnce(EVLOOP_NONBLOCK);
  EXPECT_EQ(0u, eventBase.getNotificationQueueSize());
  EXPECT_THROW_RE(
      std::move(pid).get(1s),
      std::runtime_error,
      "cannot send new requests on closed privhelper connection");

  serverThread.join();
}

TEST(PrivHelperConnectionLossTest, serverDeathFailsRequestsWithoutDeadlock) {
  // This test models the privhelper process dying (e.g. killed under memory
  // pressure) while the daemon is running: the server end of the socket
  // closes and the client sees EOF.
  EventBaseThread ioThread;
  File clientConn;
  File serverConn;
  PrivHelperConn::createConnPair(clientConn, serverConn);
  auto client = createTestPrivHelper(std::move(clientConn));
  ioThread.getEventBase()->runInEventBaseThreadAndWait(
      [&] { client->attachEventBase(ioThread.getEventBase()); });

  // Issue a request that the server never answers, and make sure it has
  // been written before the connection drops, so this test pins the EOF
  // path rather than the send-failure path covered below.
  auto pending = client->fuseUnmount("/never/answered", {});
  ioThread.getEventBase()->runInEventBaseThreadAndWait([] {});

  // The privhelper process dies.
  serverConn.close();

  // The pending request fails with the connection error rather than
  // hanging. (folly::FutureTimeout is a std::logic_error, so this assertion
  // also proves the future was actually fulfilled.)
  EXPECT_THROW(std::move(pending).get(5s), std::runtime_error);

  // New requests fail fast instead of queueing against a dead connection.
  EXPECT_THROW(
      client->fuseUnmount("/other/mount", {}).get(5s), std::runtime_error);

  // The closed connection no longer has a file descriptor to report.
  EXPECT_EQ(-1, client->getRawClientFd());

  // The EventBase thread survived processing the EOF.
  folly::Baton<> alive;
  ioThread.getEventBase()->runInEventBaseThread([&] { alive.post(); });
  EXPECT_TRUE(alive.try_wait_for(5s));
}

TEST(PrivHelperConnectionLossTest, sendFailureFailsRequestsWithoutDeadlock) {
  // Same failure family as above, but through the other entry point: the
  // send itself fails synchronously while the connection still looks open,
  // which invokes the error callbacks from inside send().
  EventBaseThread ioThread;
  File clientConn;
  File serverConn;
  PrivHelperConn::createConnPair(clientConn, serverConn);
  auto client = createTestPrivHelper(std::move(clientConn));
  ioThread.getEventBase()->runInEventBaseThreadAndWait(
      [&] { client->attachEventBase(ioThread.getEventBase()); });

  // Shut down only the client's sending side: the client never sees EOF,
  // but its next send fails with EPIPE.
  folly::checkUnixError(
      ::shutdown(client->getRawClientFd(), SHUT_WR), "shutdown failed");

  auto pending = client->fuseUnmount("/never/answered", {});
  EXPECT_THROW(std::move(pending).get(5s), std::runtime_error);
  EXPECT_THROW(
      client->fuseUnmount("/other/mount", {}).get(5s), std::runtime_error);

  folly::Baton<> alive;
  ioThread.getEventBase()->runInEventBaseThread([&] { alive.post(); });
  EXPECT_TRUE(alive.try_wait_for(5s));
}

TEST(PrivHelperConnectionLossTest, unexpectedExitLogsOneEvent) {
  EventBaseThread ioThread;
  File clientConn;
  File serverConn;
  PrivHelperConn::createConnPair(clientConn, serverConn);
  auto client = createTestPrivHelper(std::move(clientConn));
  auto recorder = std::make_shared<RecordingXplatLogger>();
  client->setEdenFsEventsLogger(std::make_shared<EdenFsEventsLogger>(recorder));
  ioThread.getEventBase()->runInEventBaseThreadAndWait(
      [&] { client->attachEventBase(ioThread.getEventBase()); });

  auto pending = client->fuseUnmount("/never/answered", {});
  ioThread.getEventBase()->runInEventBaseThreadAndWait([] {});

  // Drain the request from the server side before closing: closing a
  // socket with unread data produces ECONNRESET on the client instead of
  // a clean EOF.
  char buf[4096];
  while (recv(serverConn.fd(), buf, sizeof(buf), MSG_DONTWAIT) > 0) {
  }

  // The privhelper process dies.
  serverConn.close();
  EXPECT_THROW(std::move(pending).get(5s), std::runtime_error);

  // Exactly one privhelper_exit event is logged, even though tearing down
  // the connection triggers multiple socket callbacks (EOF, socket closed).
  auto events = recorder->getEvents();
  ASSERT_EQ(1ul, events.size());
  const auto& strings = events[0].second.getStringMap();
  EXPECT_EQ("privhelper_exit", strings.at("type"));
  EXPECT_EQ("eof", strings.at("reason"));
}

TEST(PrivHelperConnectionLossTest, cleanShutdownLogsNoEvent) {
  EventBaseThread ioThread;
  File clientConn;
  File serverConn;
  PrivHelperConn::createConnPair(clientConn, serverConn);
  auto recorder = std::make_shared<RecordingXplatLogger>();
  {
    auto client = createTestPrivHelper(std::move(clientConn));
    client->setEdenFsEventsLogger(
        std::make_shared<EdenFsEventsLogger>(recorder));
    ioThread.getEventBase()->runInEventBaseThreadAndWait(
        [&] { client->attachEventBase(ioThread.getEventBase()); });
    ioThread.getEventBase()->runInEventBaseThreadAndWait(
        [&] { client->detachEventBase(); });
  }

  // Locally-initiated teardown is not an unexpected privhelper exit.
  EXPECT_EQ(0ul, recorder->getEvents().size());
}

/**
 * An IXplatLogger that holds the caller in logEvent() until the test releases
 * it. The client logs the privhelper exit after its status leaves RUNNING. The
 * test can then act on the client at a known point in that window.
 */
class BlockingXplatLogger : public IXplatLogger {
 public:
  void logEvent(std::string_view /*category*/, const DynamicEvent& /*event*/)
      override {
    entered_.post();
    released_.wait();
  }

  bool waitUntilEntered(std::chrono::milliseconds timeout) {
    return entered_.try_wait_for(timeout);
  }

  void release() {
    released_.post();
  }

 private:
  folly::SaturatingSemaphore<true /* MayBlock */> entered_;
  folly::SaturatingSemaphore<true /* MayBlock */> released_;
};

class PrivHelperExitRaceTest : public ::testing::Test {
 protected:
  void SetUp() override {
    File clientConn;
    PrivHelperConn::createConnPair(clientConn, serverConn_);
    client_ = createTestPrivHelper(std::move(clientConn));
    client_->setEdenFsEventsLogger(
        std::make_shared<EdenFsEventsLogger>(logger_));
    ioThread_.getEventBase()->runInEventBaseThreadAndWait(
        [&] { client_->attachEventBase(ioThread_.getEventBase()); });
  }

  /**
   * Close the server end of the connection. Then destroy the client while its
   * EventBase thread is in the privhelper-exit handling. The EventBase stays
   * alive.
   */
  void destroyClientDuringPrivhelperExit() {
    serverConn_.close();
    ASSERT_TRUE(logger_->waitUntilEntered(30s))
        << "the client did not start to handle the privhelper exit";
    client_.reset();
    logger_->release();

    folly::Baton<> alive;
    ioThread_.getEventBase()->runInEventBaseThread([&] { alive.post(); });
    EXPECT_TRUE(alive.try_wait_for(30s));
  }

  // Declared before client_, so that the EventBase outlives the client.
  EventBaseThread ioThread_;
  File serverConn_;
  std::shared_ptr<BlockingXplatLogger> logger_{
      std::make_shared<BlockingXplatLogger>()};
  std::unique_ptr<PrivHelper> client_;
};

TEST_F(PrivHelperExitRaceTest, clientDestroyedDuringPrivhelperExit) {
  destroyClientDuringPrivhelperExit();
}

TEST_F(PrivHelperExitRaceTest, requestInFlightFailsWithTheExitError) {
  // The request is larger than the socket buffer. The server does not read.
  // Part of the request thus stays in the client's send queue.
  auto pending =
      client_->fuseUnmount(std::string(8 * 1024 * 1024, 'x'), UnmountOptions{});
  ioThread_.getEventBase()->runInEventBaseThreadAndWait([] {});

  destroyClientDuringPrivhelperExit();

  EXPECT_THROW_RE(
      std::move(pending).get(30s), std::runtime_error, "privhelper process");
}

/**
 * The sentinel is created by the daemon's unprivileged user and examined by a
 * root privhelper, so these cases are all about what a file planted at the name
 * can make that examination conclude.
 */
class PrivHelperSentinelTest : public ::testing::Test {
 protected:
  using DisarmState = RestartSentinel::DisarmState;

  void SetUp() override {
    dir_ = std::make_unique<TemporaryDirectory>("edenfs_sentinel");
    sentinel_.setConfig(makeRestartArgs(sentinelPath()));
  }

  std::string sentinelPath() const {
    return (dir_->path() / "sentinel").string();
  }

  /** Empty, owned by us and only ours to write, as the daemon writes it. */
  void writeSentinel() {
    ASSERT_TRUE(folly::writeFile(std::string{}, sentinelPath().c_str()));
    restrictSentinelToOwner(sentinelPath());
  }

  void expectDisarmed() {
    EXPECT_EQ(DisarmState::ShutdownAnnounced, sentinel_.disarmState());
  }

  RestartSentinel sentinel_{getuid()};
  std::unique_ptr<TemporaryDirectory> dir_;
};

TEST_F(PrivHelperSentinelTest, anEmptyFileTheDaemonOwnsIsArmed) {
  writeSentinel();

  EXPECT_EQ(DisarmState::Armed, sentinel_.disarmState());
}

TEST_F(PrivHelperSentinelTest, aSymlinkIsNotArmed) {
  const auto target = (dir_->path() / "target").string();
  ASSERT_TRUE(folly::writeFile(std::string{}, target.c_str()));
  checkUnixError(::symlink(target.c_str(), sentinelPath().c_str()));

  expectDisarmed();
}

TEST_F(PrivHelperSentinelTest, aFifoIsNotArmed) {
  // Without O_NONBLOCK and the regular-file check, opening this would block a
  // root process that still owes the mounts a cleanup.
  checkUnixError(::mkfifo(sentinelPath().c_str(), 0600));

  expectDisarmed();
}

TEST_F(PrivHelperSentinelTest, aDirectoryIsNotArmed) {
  checkUnixError(::mkdir(sentinelPath().c_str(), 0700));

  expectDisarmed();
}

TEST_F(PrivHelperSentinelTest, aSentinelOwnedByAnotherUserIsNotArmed) {
  writeSentinel();
  RestartSentinel otherOwner{getuid() + 1};
  otherOwner.setConfig(makeRestartArgs(sentinelPath()));

  EXPECT_EQ(DisarmState::ShutdownAnnounced, otherOwner.disarmState());
}

TEST_F(PrivHelperSentinelTest, aGroupWritableSentinelIsNotArmed) {
  writeSentinel();
  checkUnixError(::chmod(sentinelPath().c_str(), 0660));

  expectDisarmed();
}

TEST_F(PrivHelperSentinelTest, aMissingSentinelIsNotArmed) {
  expectDisarmed();
}

TEST_F(PrivHelperSentinelTest, aNameThatCannotBeExaminedIsUnknown) {
  // A unix socket is the one thing that fails the open without settling what
  // is at the name, so it is the only route to Unknown from a resolvable path.
  const auto path = sentinelPath();
  ASSERT_LT(path.size(), sizeof(sockaddr_un::sun_path));
  sockaddr_un addr{};
  addr.sun_family = AF_UNIX;
  std::memcpy(addr.sun_path, path.c_str(), path.size() + 1);
  const int socketFd = ::socket(AF_UNIX, SOCK_STREAM, 0);
  checkUnixError(socketFd);
  const folly::File socket{socketFd, /*ownsFd=*/true};
  checkUnixError(
      ::bind(
          socket.fd(), reinterpret_cast<const sockaddr*>(&addr), sizeof(addr)));

  EXPECT_EQ(DisarmState::Unknown, sentinel_.disarmState());
}

TEST_F(PrivHelperSentinelTest, aMarkerFromAnotherGenerationIsNotArmed) {
  // Generations share the state directory and the name prefix, differing only
  // in pid and token, so only the exact configured leaf may arm.
  const auto foreign =
      (dir_->path() / ".edenfs_restart_armed.999.00000000deadbeef").string();
  ASSERT_TRUE(folly::writeFile(std::string{}, foreign.c_str()));
  restrictSentinelToOwner(foreign);

  // The neighbour is a marker root would arm on, so the leaf is the only thing
  // separating the two verdicts below.
  sentinel_.setConfig(makeRestartArgs(foreign));
  ASSERT_EQ(DisarmState::Armed, sentinel_.disarmState());

  // This generation's own marker was never created.
  sentinel_.setConfig(makeRestartArgs(
      (dir_->path() / ".edenfs_restart_armed.1234.000000000000000a").string()));

  expectDisarmed();
}

/**
 * The relaunch command as the daemon hands it over in the restart arguments.
 * No sentinel file is involved: these cases are about the configuration alone.
 */
class PrivHelperRelaunchCommandTest : public ::testing::Test {
 protected:
  /** The duplicated key is deliberate: order is part of the contract. */
  static std::vector<std::pair<std::string, std::string>> relaunchEnv() {
    return {
        {"PATH", "/usr/bin"}, {"HOME", "/home/first"}, {"HOME", "/home/last"}};
  }

  void configure(
      std::vector<std::string> argv,
      std::vector<std::pair<std::string, std::string>> env) {
    auto args = makeRestartArgs("/unused");
    args.relaunchArgv = std::move(argv);
    args.relaunchEnv = std::move(env);
    sentinel_.setConfig(std::move(args));
  }

  RestartSentinel sentinel_{getuid()};
};

TEST_F(PrivHelperRelaunchCommandTest, servesTheConfiguredCommandInOrder) {
  configure(kSentinelArgv, relaunchEnv());

  const auto command = sentinel_.relaunchCommand();
  ASSERT_TRUE(command.has_value());
  EXPECT_EQ(kSentinelArgv, command->argv);
  EXPECT_EQ(relaunchEnv(), command->env);
}

TEST_F(PrivHelperRelaunchCommandTest, hasNoCommandBeforeAnyConfiguration) {
  const RestartSentinel unconfigured{getuid()};

  EXPECT_EQ(std::nullopt, unconfigured.relaunchCommand());
}

TEST_F(PrivHelperRelaunchCommandTest, freshArgsReplaceTheCommand) {
  configure(kSentinelArgv, relaunchEnv());
  const std::vector<std::string> argv{"/opt/eden/edenfs", "--foreground"};
  const std::vector<std::pair<std::string, std::string>> env{
      {"HOME", "/home/second"}};

  configure(argv, env);

  const auto command = sentinel_.relaunchCommand();
  ASSERT_TRUE(command.has_value());
  EXPECT_EQ(argv, command->argv);
  EXPECT_EQ(env, command->env);
}

TEST_F(PrivHelperRelaunchCommandTest, rejectsAConfigurationWithNoArgv) {
  configure({}, relaunchEnv());

  EXPECT_EQ(std::nullopt, sentinel_.relaunchCommand());
}

TEST_F(PrivHelperRelaunchCommandTest, servesACommandWithNoEnvironment) {
  configure(kSentinelArgv, {});

  const auto command = sentinel_.relaunchCommand();
  ASSERT_TRUE(command.has_value());
  EXPECT_EQ(kSentinelArgv, command->argv);
  EXPECT_TRUE(command->env.empty());
}

class PrivHelperBreakerTest : public ::testing::Test {
 protected:
  void SetUp() override {
    configure(makeRestartArgs("/unused"));
  }

  void configure(EdenFsRestartArgs args) {
    config_ = args;
    sentinel_.setConfig(std::move(args));
  }

  /** A budget already spent up to its limit. */
  void configureAtTheLimit() {
    auto args = makeRestartArgs("/unused");
    args.restartCount = args.maxRestarts;
    configure(std::move(args));
  }

  EdenFsRestartArgs config_;
  RestartSentinel sentinel_{getuid()};
};

TEST_F(PrivHelperBreakerTest, admitsAndChargesAnAttemptWithinBudget) {
  EXPECT_TRUE(sentinel_.admitRestartAttempt(kFakeNow));
  EXPECT_EQ(2, sentinel_.restartCount());
}

TEST_F(PrivHelperBreakerTest, refusesAtTheLimit) {
  configureAtTheLimit();

  EXPECT_FALSE(sentinel_.admitRestartAttempt(kFakeNow));
  EXPECT_EQ(config_.maxRestarts, sentinel_.restartCount());
}

TEST_F(PrivHelperBreakerTest, holdsTheCountToTheWindowEdge) {
  configureAtTheLimit();

  EXPECT_FALSE(sentinel_.admitRestartAttempt(kFakeNow + config_.windowSeconds));
}

TEST_F(PrivHelperBreakerTest, decaysTheCountAfterTheWindow) {
  configureAtTheLimit();

  EXPECT_TRUE(
      sentinel_.admitRestartAttempt(kFakeNow + config_.windowSeconds + 1));
  EXPECT_EQ(1, sentinel_.restartCount());
}

TEST_F(PrivHelperBreakerTest, aBackwardsClockStartsAFreshWindow) {
  configureAtTheLimit();

  EXPECT_TRUE(sentinel_.admitRestartAttempt(kFakeNow - 60));
  EXPECT_EQ(1, sentinel_.restartCount());
}

TEST_F(PrivHelperBreakerTest, aZeroWindowStillBoundsTheCount) {
  auto args = makeRestartArgs("/unused");
  args.windowSeconds = 0;
  args.restartCount = args.maxRestarts;
  configure(std::move(args));

  EXPECT_FALSE(sentinel_.admitRestartAttempt(kFakeNow));
}

TEST_F(PrivHelperBreakerTest, aLimitTheDaemonInflatedIsStillBounded) {
  auto args = makeRestartArgs("/unused");
  args.maxRestarts = std::numeric_limits<uint32_t>::max();
  args.restartCount = 0;
  configure(std::move(args));

  constexpr uint32_t kGenerous = 1000;
  uint32_t admitted = 0;
  while (admitted < kGenerous && sentinel_.admitRestartAttempt(kFakeNow)) {
    ++admitted;
  }

  EXPECT_LT(admitted, kGenerous);
}

/** Exposes the binary resolution, which is protected. */
class PrivHelperBinaryTestServer : public PrivHelperServer {
 public:
  using PrivHelperServer::findSiblingEdenFs;
  using PrivHelperServer::resolveEdenFsBinary;
};

class PrivHelperBinaryTest : public ::testing::Test {
 protected:
  void SetUp() override {
    dir_ = std::make_unique<TemporaryDirectory>("edenfs_libexec");
  }

  AbsolutePath dirPath() const {
    return canonicalPath(dir_->path().string());
  }

  void createFile(const std::string& path, mode_t mode = 0755) {
    ASSERT_TRUE(folly::writeFile(std::string{"binary"}, path.c_str()));
    checkUnixError(::chmod(path.c_str(), mode));
  }

  /**
   * resolveEdenFsBinary probes the directory holding the test binary, which
   * this fixture does not own and cannot override, so its fallback branches
   * are only reachable while nothing named edenfs is installed there.
   */
  static testing::AssertionResult noEdenFsNextToTheTestBinary() {
    const auto executable = executablePath();
    const auto sibling =
        PrivHelperBinaryTestServer::findSiblingEdenFs(executable.dirname());
    if (sibling.has_value()) {
      return testing::AssertionFailure()
          << "the test binary is installed next to an edenfs at "
          << sibling->view();
    }
    return testing::AssertionSuccess();
  }

  PrivHelperBinaryTestServer server_;
  std::unique_ptr<TemporaryDirectory> dir_;
};

TEST_F(PrivHelperBinaryTest, findsTheSibling) {
  createFile((dir_->path() / "edenfs").string());

  EXPECT_EQ(dirPath() + "edenfs"_relpath, server_.findSiblingEdenFs(dirPath()));
}

TEST_F(PrivHelperBinaryTest, findsNothingWhenThereIsNoSibling) {
  EXPECT_EQ(std::nullopt, server_.findSiblingEdenFs(dirPath()));
}

TEST_F(PrivHelperBinaryTest, rejectsASymlinkedSibling) {
  // The leaf is the one path component an attacker who cannot write the
  // install directory itself could still repoint.
  const auto target = (dir_->path() / "elsewhere").string();
  createFile(target);
  checkUnixError(
      ::symlink(target.c_str(), (dir_->path() / "edenfs").string().c_str()));

  EXPECT_EQ(std::nullopt, server_.findSiblingEdenFs(dirPath()));
}

TEST_F(PrivHelperBinaryTest, rejectsADirectorySibling) {
  // 0755 so that access(X_OK) alone would accept it: a searchable directory
  // passes the executable check.
  checkUnixError(::mkdir((dir_->path() / "edenfs").string().c_str(), 0755));

  EXPECT_EQ(std::nullopt, server_.findSiblingEdenFs(dirPath()));
}

TEST_F(PrivHelperBinaryTest, rejectsANonExecutableSibling) {
  createFile((dir_->path() / "edenfs").string(), 0644);

  EXPECT_EQ(std::nullopt, server_.findSiblingEdenFs(dirPath()));
}

TEST_F(PrivHelperBinaryTest, acceptsASymlinkedAncestor) {
  const auto real = (dir_->path() / "real").string();
  checkUnixError(::mkdir(real.c_str(), 0700));
  createFile(real + "/edenfs");
  const auto link = (dir_->path() / "link").string();
  checkUnixError(::symlink(real.c_str(), link.c_str()));

  const auto linkDir = canonicalPath(link);
  EXPECT_EQ(linkDir + "edenfs"_relpath, server_.findSiblingEdenFs(linkDir));
}

TEST_F(PrivHelperBinaryTest, fallsBackToTheRecordedCommand) {
  ASSERT_TRUE(noEdenFsNextToTheTestBinary());

  RestartSentinel::RelaunchCommand command;
  command.argv = kSentinelArgv;

  EXPECT_EQ(
      canonicalPath(kSentinelArgv[0]), server_.resolveEdenFsBinary(command));
}

TEST_F(PrivHelperBinaryTest, throwsWhenThereIsNothingToRelaunch) {
  ASSERT_TRUE(noEdenFsNextToTheTestBinary());

  EXPECT_THROW(
      server_.resolveEdenFsBinary(RestartSentinel::RelaunchCommand{}),
      std::runtime_error);
}

class PrivHelperRestartOwnerTestServer : public PrivHelperServer {
 public:
  using PrivHelperServer::validateRestartOwner;
};

class PrivHelperRestartOwnerTest : public ::testing::Test {
 protected:
  void initWithOwner(uid_t uid, gid_t gid) {
    File clientConn;
    File serverConn;
    PrivHelperConn::createConnPair(clientConn, serverConn);
    clientConn_ = std::move(clientConn);
    server_.initPartial(std::move(serverConn), uid, gid);
  }

  PrivHelperRestartOwnerTestServer server_;
  File clientConn_;
};

TEST_F(PrivHelperRestartOwnerTest, acceptsMatchingNonRootRealIds) {
  if (getuid() == 0) {
    GTEST_SKIP() << "requires a non-root real uid";
  }
  initWithOwner(getuid(), getgid());

  EXPECT_NO_THROW(server_.validateRestartOwner());
}

TEST_F(PrivHelperRestartOwnerTest, refusesARootRealUid) {
  if (getuid() != 0) {
    GTEST_SKIP() << "requires a root real uid";
  }
  initWithOwner(getuid(), getgid());

  EXPECT_THROW(server_.validateRestartOwner(), std::runtime_error);
}

TEST_F(PrivHelperRestartOwnerTest, refusesAnOwnerMismatch) {
  initWithOwner(getuid() + 1, getgid() + 1);

  EXPECT_THROW(server_.validateRestartOwner(), std::runtime_error);
}

/** Exposes the two disarm channels and the state they meet in. */
class PrivHelperDisarmTestServer : public PrivHelperServer {
 public:
  using PrivHelperServer::processNotifyCleanShutdownMsg;
  using PrivHelperServer::processSetRestartArgsMsg;
  using PrivHelperServer::sentinel_;
};

/**
 * The two disarm channels are deliberately redundant: the notification fails
 * when the event loop is wedged, and the sentinel unlink fails essentially
 * never. Each has to disarm on its own.
 */
class PrivHelperDisarmTest : public ::testing::Test {
 protected:
  using DisarmState = RestartSentinel::DisarmState;

  void SetUp() override {
    sentinelFile_ = std::make_unique<TemporaryFile>("edenfs_restart_armed");
    restrictSentinelToOwner(sentinelFile_->path().string());
    // Nothing calls initPartial() here, which is what would otherwise
    // construct the sentinel from the daemon's uid.
    server_.sentinel_.emplace(getuid());
    deliverRestartArgs();
  }

  void deliverRestartArgs() {
    deliverRestartArgs(sentinelFile_->path().string());
  }

  void deliverRestartArgs(std::string sentinelPath) {
    auto msg = PrivHelperConn::serializeSetRestartArgsRequest(
        /*xid=*/1, makeRestartArgs(std::move(sentinelPath)));
    folly::io::Cursor cursor{&msg.data};
    PrivHelperConn::parsePacket(cursor);
    server_.processSetRestartArgsMsg(cursor);
  }

  void deliverCleanShutdown() {
    auto msg = PrivHelperConn::serializeNotifyCleanShutdownRequest(
        /*xid=*/2, "stop");
    folly::io::Cursor cursor{&msg.data};
    PrivHelperConn::parsePacket(cursor);
    server_.processNotifyCleanShutdownMsg(cursor);
  }

  void expectSentinelPathRejected(const std::string& sentinelPath) {
    SCOPED_TRACE(sentinelPath);
    deliverRestartArgs(sentinelPath);
    EXPECT_EQ(DisarmState::Unknown, server_.sentinel_->disarmState());
  }

  std::string sentinelDir() const {
    return sentinelFile_->path().parent_path().string();
  }

  PrivHelperDisarmTestServer server_;
  std::unique_ptr<TemporaryFile> sentinelFile_;
};

TEST_F(PrivHelperDisarmTest, restartArgsArm) {
  ASSERT_TRUE(server_.sentinel_->enabled());
  EXPECT_EQ(DisarmState::Armed, server_.sentinel_->disarmState());
}

TEST_F(PrivHelperDisarmTest, theNotificationAloneDisarms) {
  deliverCleanShutdown();

  EXPECT_EQ(DisarmState::ShutdownAnnounced, server_.sentinel_->disarmState());
}

TEST_F(PrivHelperDisarmTest, removingTheSentinelAloneDisarms) {
  sentinelFile_.reset();

  EXPECT_EQ(DisarmState::ShutdownAnnounced, server_.sentinel_->disarmState());
}

TEST_F(PrivHelperDisarmTest, freshRestartArgsReArm) {
  // A daemon that resends its configuration has recovered from a failed
  // takeover, and would otherwise stay permanently un-restartable behind the
  // flag its aborted shutdown set.
  deliverCleanShutdown();
  ASSERT_EQ(DisarmState::ShutdownAnnounced, server_.sentinel_->disarmState());

  deliverRestartArgs();

  EXPECT_EQ(DisarmState::Armed, server_.sentinel_->disarmState());
}

TEST_F(PrivHelperDisarmTest, freshRestartArgsDropTheResolvedDirectory) {
  ASSERT_EQ(DisarmState::Armed, server_.sentinel_->disarmState());

  const TemporaryDirectory elsewhere{"edenfs_restart_elsewhere"};
  deliverRestartArgs((elsewhere.path() / "absent").string());

  EXPECT_EQ(DisarmState::ShutdownAnnounced, server_.sentinel_->disarmState());
}

TEST_F(PrivHelperDisarmTest, aSentinelPathRootCannotResolveIsUnknown) {
  deliverRestartArgs("not/absolute");

  EXPECT_EQ(DisarmState::Unknown, server_.sentinel_->disarmState());
}

TEST_F(PrivHelperDisarmTest, aLeafThatAlwaysResolvesIsUnknown) {
  // "." and ".." resolve whatever the directory holds, so a sentinel named
  // either could never be reported gone.
  expectSentinelPathRejected(sentinelDir() + "/.");
  expectSentinelPathRejected(sentinelDir() + "/..");
}

TEST_F(PrivHelperDisarmTest, aSentinelPathWithAnEmbeddedNulIsUnknown) {
  // The syscalls stop at the NUL, so they would act on a prefix of the path
  // that was validated.
  expectSentinelPathRejected(sentinelFile_->path().string() + '\0');
}

TEST_F(PrivHelperDisarmTest, aServerThatNeverReceivedRestartArgsHasNoState) {
  const RestartSentinel unconfigured{getuid()};

  EXPECT_EQ(std::nullopt, unconfigured.disarmState());
}

class PrivHelperRealSpawnTestServer : public PrivHelperServer {
 public:
  bool launchExecutable(folly::StringPiece binary) {
    const RestartPlan plan{
        canonicalPath(binary),
        RestartSentinel::RelaunchCommand{{"edenfs"}, {}},
        1,
        kFakeNow};
    return launchRestart(plan);
  }

 private:
  void validateRestartOwner() const override {}
};

/**
 * Checks whether this environment can spawn a process with reset user and
 * group IDs. Returns false for EPERM, EACCES, or EINVAL from the spawn.
 * Other spawn errors propagate.
 */
bool canSpawnWithResetIds() {
  SpawnedProcess::Options opts;
  opts.nullStdin();
  opts.resetIds();
  try {
    SpawnedProcess proc({"/usr/bin/true"}, std::move(opts));
    // Whatever the probe exited with, the reset was permitted. Report the
    // oddity rather than skipping over it.
    const auto status = proc.wait();
    EXPECT_EQ(0, status.exitStatus()) << "the reset-ids probe " << status.str();
    return true;
  } catch (const std::system_error& ex) {
    // POSIX_SPAWN_RESETIDS is all this probe asks for beyond a plain spawn, so
    // a setuid/setgid the kernel refuses arrives as one of these.
    const auto errorCode = ex.code().value();
    if (ex.code().category() == std::generic_category() &&
        (errorCode == EPERM || errorCode == EACCES || errorCode == EINVAL)) {
      return false;
    }
    throw;
  }
}

TEST(PrivHelperRestartLaunchTest, aChildThatExitsNonzeroDidNotFinishStarting) {
  if (!canSpawnWithResetIds()) {
    GTEST_SKIP() << "this environment forbids spawning with reset ids";
  }
  PrivHelperRealSpawnTestServer server;

  EXPECT_FALSE(server.launchExecutable("/usr/bin/false"));
}

TEST(PrivHelperRestartLaunchTest, aChildThatExitsZeroFinishedStarting) {
  if (!canSpawnWithResetIds()) {
    GTEST_SKIP() << "this environment forbids spawning with reset ids";
  }
  PrivHelperRealSpawnTestServer server;

  EXPECT_TRUE(server.launchExecutable("/usr/bin/true"));
}

/**
 * A PrivHelperServer that records what the restart path would have done rather
 * than resolving a real binary or launching anything.
 */
class PrivHelperRestartTestServer : public PrivHelperServer {
 public:
  struct Spawn {
    AbsolutePath binary;
    std::vector<std::string> argv;
    std::vector<std::pair<std::string, std::string>> env;
  };

  PrivHelperRestartTestServer() {
    spawnEdenFs_ =
        [this](
            const AbsolutePath& binary,
            const std::vector<std::string>& argv,
            const std::vector<std::pair<std::string, std::string>>& env) {
          spawns.wlock()->push_back(Spawn{binary, argv, env});
          return spawnSucceeds.load();
        };
    now_ = [this] { return now.load(); };
  }

  using PrivHelperServer::launchRestart;
  using PrivHelperServer::prepareRestart;
  using PrivHelperServer::sentinel_;

  size_t spawnCount() {
    return spawns.rlock()->size();
  }

  std::atomic<uint64_t> now{0};
  std::atomic<bool> spawnSucceeds{true};
  std::atomic<bool> restartOwnerValid{true};
  std::atomic<bool> cleanupRan{false};
  // Every attempt, including the ones spawnSucceeds turned into a failure.
  folly::Synchronized<std::vector<Spawn>> spawns;

 private:
  AbsolutePath resolveEdenFsBinary(
      const RestartSentinel::RelaunchCommand& /* command */) const override {
    return canonicalPath("/fake/libexec/eden/edenfs");
  }

  void validateRestartOwner() const override {
    if (!restartOwnerValid.load()) {
      throw std::runtime_error("real uid is root");
    }
  }

  void cleanupMountPoints() override {
    cleanupRan.store(true);
  }
};

/**
 * Exercises the restart decision directly, without a socket or an event loop.
 *
 * The privhelper is armed by default once it has restart args: an empty
 * sentinel on disk, and the command to relaunch inside the args. Each test here
 * removes exactly one of the reasons to restart and checks that nothing is
 * launched.
 */
class PrivHelperRestartDecisionTest : public ::testing::Test {
 protected:
  void SetUp() override {
    sentinelFile_ = std::make_unique<TemporaryFile>("edenfs_restart_armed");
    restrictSentinelToOwner(sentinelPath());
    server_.now.store(kFakeNow);
    // Nothing calls initPartial() here, so the sentinel's owner has to be
    // declared by hand for the ownership check to pass.
    server_.sentinel_.emplace(getuid());
    configure(restartArgs());
  }

  std::string sentinelPath() const {
    return sentinelFile_->path().string();
  }

  EdenFsRestartArgs restartArgs() const {
    auto args = makeRestartArgs(sentinelPath());
    args.relaunchArgv = kSentinelArgv;
    args.relaunchEnv = {{"PATH", "/usr/bin"}, {"HOME", "/home/test"}};
    return args;
  }

  void configure(EdenFsRestartArgs args) {
    server_.sentinel_->setConfig(std::move(args));
  }

  void removeSentinel() {
    sentinelFile_.reset();
  }

  /**
   * Runs the same two-step sequence run() does, or nullopt when the plan step
   * already decided to leave edenfs down.
   */
  std::optional<bool> restart() {
    const auto plan = server_.prepareRestart();
    if (!plan) {
      return std::nullopt;
    }
    return server_.launchRestart(*plan);
  }

  void expectNoRestart() {
    EXPECT_EQ(std::nullopt, restart());
    EXPECT_EQ(0, server_.spawnCount());
  }

  PrivHelperRestartTestServer server_;
  std::unique_ptr<TemporaryFile> sentinelFile_;
};

TEST_F(PrivHelperRestartDecisionTest, restartsAfterACrashWithAnEmptySentinel) {
  std::string contents;
  ASSERT_TRUE(folly::readFile(sentinelPath().c_str(), contents));
  ASSERT_TRUE(contents.empty());

  EXPECT_EQ(true, restart());

  ASSERT_EQ(1, server_.spawnCount());
  EXPECT_EQ(kSentinelArgv, server_.spawns.rlock()->at(0).argv);
}

TEST_F(PrivHelperRestartDecisionTest, doesNotRestartWithoutRestartArgs) {
  server_.sentinel_.emplace(getuid());
  expectNoRestart();
}

TEST_F(PrivHelperRestartDecisionTest, doesNotRestartWhenDisabled) {
  auto args = restartArgs();
  args.enabled = false;
  configure(std::move(args));

  expectNoRestart();
}

TEST_F(PrivHelperRestartDecisionTest, cleanShutdownNotificationDisarms) {
  server_.sentinel_->noteCleanShutdown();
  expectNoRestart();
}

TEST_F(PrivHelperRestartDecisionTest, removingTheSentinelDisarms) {
  removeSentinel();
  expectNoRestart();
}

TEST_F(PrivHelperRestartDecisionTest, aSentinelAnyoneCouldHaveWrittenDisarms) {
  checkUnixError(::chmod(sentinelPath().c_str(), 0660));
  expectNoRestart();
}

TEST_F(PrivHelperRestartDecisionTest, doesNotRestartWithoutARelaunchCommand) {
  auto args = restartArgs();
  args.relaunchArgv.clear();
  configure(std::move(args));

  expectNoRestart();
}

TEST_F(PrivHelperRestartDecisionTest, doesNotRestartOnceTheBreakerIsTripped) {
  auto args = restartArgs();
  args.restartCount = args.maxRestarts;
  configure(std::move(args));

  expectNoRestart();
}

TEST_F(PrivHelperRestartDecisionTest, refusesToRestartWhenOwnerIsInvalid) {
  server_.restartOwnerValid.store(false);

  EXPECT_EQ(false, restart());
  EXPECT_EQ(0, server_.spawnCount());
}

TEST_F(PrivHelperRestartDecisionTest, reportsAFailedSpawn) {
  server_.spawnSucceeds.store(false);

  EXPECT_EQ(false, restart());
  EXPECT_EQ(1, server_.spawnCount());
}

TEST_F(PrivHelperRestartDecisionTest, relaysTheRestartBudgetToTheNewDaemon) {
  ASSERT_EQ(true, restart());

  const auto spawns = *server_.spawns.rlock();
  ASSERT_EQ(1, spawns.size());
  EXPECT_THAT(
      spawns[0].env,
      UnorderedElementsAre(
          std::pair<std::string, std::string>{"PATH", "/usr/bin"},
          std::pair<std::string, std::string>{"HOME", "/home/test"},
          std::pair<std::string, std::string>{"EDENFS_RESTART_COUNT", "2"},
          std::pair<std::string, std::string>{
              "EDENFS_FIRST_RESTART_AT", folly::to<std::string>(kFakeNow)}));
}

TEST_F(PrivHelperRestartDecisionTest, replacesARecordedRestartBudget) {
  auto args = restartArgs();
  args.relaunchEnv = {
      {"EDENFS_RESTART_COUNT", "99"}, {"EDENFS_FIRST_RESTART_AT", "1"}};
  configure(std::move(args));

  ASSERT_EQ(true, restart());

  const auto spawns = *server_.spawns.rlock();
  ASSERT_EQ(1, spawns.size());
  EXPECT_THAT(
      spawns[0].env,
      UnorderedElementsAre(
          std::pair<std::string, std::string>{"EDENFS_RESTART_COUNT", "2"},
          std::pair<std::string, std::string>{
              "EDENFS_FIRST_RESTART_AT", folly::to<std::string>(kFakeNow)}));
}

/**
 * Drives a real server through run(), so that closing the client socket
 * reproduces the death of edenfs.
 */
class PrivHelperRestartRunTest : public ::testing::Test {
 protected:
  void SetUp() override {
    sentinel_ = std::make_unique<TemporaryFile>("edenfs_restart_armed");
    // Empty: its existence is the whole signal now. TemporaryFile creates
    // 0666 & ~umask, which the validating open rejects.
    writeSentinel("");
    restrictSentinelToOwner(sentinelPath());
    server_.now.store(kFakeNow);

    File clientConn;
    File serverConn;
    PrivHelperConn::createConnPair(clientConn, serverConn);
    rawClientConn_ = clientConn.dup();
    serverThread_ =
        std::thread([this, conn = std::move(serverConn)]() mutable noexcept {
          server_.initPartial(std::move(conn), getuid(), getgid());
          server_.run();
        });
    client_ = createTestPrivHelper(std::move(clientConn));
    clientIoThread_.getEventBase()->runInEventBaseThreadAndWait(
        [&] { client_->attachEventBase(clientIoThread_.getEventBase()); });
  }

  ~PrivHelperRestartRunTest() override {
    killTheDaemon();
  }

  void armTheServer() {
    auto args = makeRestartArgs(sentinelPath());
    // The command travels in the arguments now, so a relaunch has nothing to
    // spawn without it.
    args.relaunchArgv = kSentinelArgv;
    std::move(client_->setRestartArgs(args)).get(1s);
  }

  /** Closes the connection, as a dying daemon would, and waits for run(). */
  void killTheDaemon() {
    rawClientConn_.close();
    client_.reset();
    if (serverThread_.joinable()) {
      serverThread_.join();
    }
  }

  void triggerReceiveError() {
    const std::string malformedHeader(16, '\0');
    ASSERT_EQ(
        malformedHeader.size(),
        folly::writeFull(
            rawClientConn_.fd(),
            malformedHeader.data(),
            malformedHeader.size()));
    serverThread_.join();
  }

  std::string sentinelPath() const {
    return sentinel_->path().string();
  }

  void writeSentinel(const std::string& contents) {
    ASSERT_TRUE(folly::writeFile(contents, sentinelPath().c_str()));
  }

  std::unique_ptr<PrivHelper> client_;
  File rawClientConn_;
  PrivHelperRestartTestServer server_;
  std::thread serverThread_;
  EventBaseThread clientIoThread_;
  std::unique_ptr<TemporaryFile> sentinel_;
};

TEST_F(PrivHelperRestartRunTest, crashRestartsAndLeavesTheMountsAlone) {
  armTheServer();
  killTheDaemon();

  EXPECT_EQ(1, server_.spawnCount());
  // The new daemon detects and replaces the stale mounts itself.
  EXPECT_FALSE(server_.cleanupRan.load());
}

TEST_F(PrivHelperRestartRunTest, receiveErrorDoesNotRestart) {
  armTheServer();
  triggerReceiveError();

  EXPECT_EQ(0, server_.spawnCount());
  EXPECT_TRUE(server_.cleanupRan.load());
}

TEST_F(PrivHelperRestartRunTest, cleanShutdownSkipsTheRestart) {
  armTheServer();
  client_->notifyCleanShutdown("stop");
  killTheDaemon();

  EXPECT_EQ(0, server_.spawnCount());
  EXPECT_TRUE(server_.cleanupRan.load());
}

TEST_F(PrivHelperRestartRunTest, aRemovedSentinelSkipsTheRestart) {
  // What a SIGKILL leaves behind: the daemon never announced a shutdown, so
  // the removed sentinel is the only thing saying the kill was deliberate.
  armTheServer();
  ASSERT_EQ(0, ::unlink(sentinelPath().c_str()));
  killTheDaemon();

  EXPECT_EQ(0, server_.spawnCount());
  EXPECT_TRUE(server_.cleanupRan.load());
}

TEST_F(PrivHelperRestartRunTest, anUnarmedPrivhelperStillCleansUp) {
  killTheDaemon();

  EXPECT_EQ(0, server_.spawnCount());
  EXPECT_TRUE(server_.cleanupRan.load());
}

TEST_F(PrivHelperRestartRunTest, aFailedSpawnCleansUp) {
  server_.spawnSucceeds.store(false);
  armTheServer();
  killTheDaemon();

  EXPECT_EQ(1, server_.spawnCount());
  EXPECT_TRUE(server_.cleanupRan.load());
}

TEST_F(PrivHelperRestartRunTest, anInvalidRestartOwnerStillCleansUp) {
  server_.restartOwnerValid.store(false);
  armTheServer();
  killTheDaemon();

  EXPECT_EQ(0, server_.spawnCount());
  EXPECT_TRUE(server_.cleanupRan.load());
}
