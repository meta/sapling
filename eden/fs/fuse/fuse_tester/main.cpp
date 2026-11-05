/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include <boost/filesystem.hpp>
#include <folly/Exception.h>
#include <folly/init/Init.h>
#include <folly/io/async/EventBase.h>
#include <folly/io/async/EventBaseThread.h>
#include <folly/logging/LogConfigParser.h>
#include <folly/logging/xlog.h>
#include <gflags/gflags.h>
#include <sysexits.h>
#include <csignal>

#include "eden/common/utils/EnumValue.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/ProcessInfoCache.h"
#include "eden/common/utils/UserInfo.h"
#include "eden/fs/fuse/FuseChannel.h"
#include "eden/fs/fuse/FuseDispatcher.h"
#include "eden/fs/privhelper/PrivHelper.h"
#include "eden/fs/privhelper/PrivHelperImpl.h"
#include "eden/fs/store/ObjectStore.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/telemetry/ErrorLogger.h"

using namespace facebook::eden;
using namespace std::chrono_literals;
using folly::exceptionStr;
using std::string;

DEFINE_int32(numFuseThreads, 4, "The number of FUSE worker threads");

namespace {
constexpr size_t kTraceBusCapacity = 25000;
class TestDispatcher : public FuseDispatcher {
 public:
  TestDispatcher(EdenStatsPtr stats, const UserInfo& identity)
      : FuseDispatcher(std::move(stats)), identity_(identity) {}

  ImmediateFuture<Attr> getattr(
      InodeNumber ino,
      const ObjectFetchContextPtr& /*context*/) override {
    if (ino == kRootNodeId) {
      struct stat st = {};
      st.st_ino = ino.get();
      st.st_mode = S_IFDIR | 0755;
      st.st_nlink = 2;
      st.st_uid = identity_.getUid();
      st.st_gid = identity_.getGid();
      st.st_blksize = 512;
      st.st_blocks = 1;
      return Attr(st, /* timeout */ 0);
    }
    folly::throwSystemErrorExplicit(ENOENT);
  }

  UserInfo identity_;
};

void ensureEmptyDirectory(AbsolutePathPiece path) {
  boost::filesystem::path boostPath(path.view().begin(), path.view().end());

  XLOGF(INFO, "boost path: {}", boostPath.native());
  if (!boost::filesystem::create_directories(boostPath)) {
    // This directory already existed.  Make sure it is empty.
    if (!boost::filesystem::is_empty(boostPath)) {
      throwf<std::runtime_error>(
          "{} does not refer to an empty directory", path);
    }
  }
}
} // namespace

int main(int argc, char** argv) {
  // Make sure to run this before any flag values are read.
  const folly::Init init(&argc, &argv);
  if (argc != 2) {
    fprintf(stderr, "usage: test_mount PATH\n");
    return EX_NOPERM;
  }

  auto sigresult = signal(SIGPIPE, SIG_IGN);
  if (sigresult == SIG_ERR) {
    folly::throwSystemError("error ignoring SIGPIPE");
  }

  // Determine the desired user and group ID.
  if (geteuid() != 0) {
    fprintf(stderr, "error: fuse_tester must be started as root\n");
    return EX_NOPERM;
  }
  folly::checkPosixError(chdir("/"), "failed to chdir(/)");

  // Fork the privhelper process, then drop privileges.
  auto identity = UserInfo::lookup();
  auto privHelper = startOrConnectToPrivHelper(identity, argc, argv);
  identity.dropPrivileges();

  auto mountPath = normalizeBestEffort(argv[1]);
  try {
    ensureEmptyDirectory(mountPath);
  } catch (const std::exception& ex) {
    fprintf(stderr, "error with mount path: %s\n", exceptionStr(ex).c_str());
    return EX_DATAERR;
  }

  auto loggingConfig = folly::parseLogConfig("eden=DBG2;eden.fs.fuse=DBG7");
  folly::LoggerDB::get().updateConfig(loggingConfig);

  // For simplicity, start a separate EventBaseThread to drive the privhelper
  // I/O.  We only really need this for the initial fuseMount() call.  We could
  // run an EventBase in the current thread until the fuseMount() completes,
  // but using EventBaseThread is simpler for now.
  folly::EventBaseThread evbt;
  evbt.getEventBase()->runInEventBaseThreadAndWait(
      [&] { privHelper->attachEventBase(evbt.getEventBase()); });
  auto fuseDevice =
      privHelper
          ->fuseMount(
              mountPath.value(), /* readOnly= */ false, /*vfsName*/ "fuse")
          .get(100ms);

  EdenStatsPtr stats;
  auto dispatcher =
      std::make_unique<TestDispatcher>(std::move(stats), identity);

  ErrorLogger noopErrorLogger{};

  folly::Logger straceLogger{"eden.strace"};

  auto channel = makeFuseChannel(
      privHelper.get(),
      std::move(fuseDevice),
      mountPath,
      folly::getUnsafeMutableGlobalCPUExecutor(),
      FLAGS_numFuseThreads,
      std::move(dispatcher),
      &straceLogger,
      std::make_shared<ProcessInfoCache>(),
      /*edenFsEventsLogger=*/nullptr,
      /*errorLogger=*/noopErrorLogger,
      std::chrono::seconds(60),
      /*notifications=*/nullptr,
      /*requireUtf8Path=*/true,
      /*maximumBackgroundRequests=*/12 /* the default on Linux */,
      /*maximumInFlightRequests=*/(size_t)1000,
      /*highFuseRequestsLogInterval=*/
      std::chrono::minutes{10},
      /*longRunningFSRequestThreshold=*/std::chrono::minutes{5},
      /*useWriteBackCache=*/false,
      /*fuseTraceBusCapacity=*/kTraceBusCapacity);

  XLOG(INFO, "Starting FUSE...");
  auto completionFuture = channel->initialize().get();
  XLOG(INFO, "FUSE started");

  auto stopDataPtr = std::move(completionFuture).get();
  auto& stopData = dynamic_cast<FuseChannel::StopData&>(*stopDataPtr);
  XLOGF(INFO, "FUSE channel done; stop_reason={}", enumValue(stopData.reason));

  return EX_OK;
}
