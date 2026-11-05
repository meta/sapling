/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include <cstring>
#include <string>

#ifdef __linux__
#include <grp.h>
#include <sys/prctl.h>

#include <cerrno>

#include <folly/String.h>
#endif // __linux__

#include <folly/Exception.h>
#include <folly/File.h>
#include <folly/FileUtil.h>
#include <folly/init/Init.h>
#include <folly/logging/LogConfigParser.h>
#include <folly/logging/xlog.h>
#include <folly/portability/Unistd.h>
#include "eden/common/utils/UserInfo.h"
#include "eden/fs/privhelper/PinScan.h"
#include "eden/fs/privhelper/PrivHelperFlags.h"
#include "eden/fs/privhelper/PrivHelperRollback.h"
#include "eden/fs/privhelper/PrivHelperServer.h"

using namespace facebook::eden;

DEFINE_int32(
    privhelper_uid,
    facebook::eden::UserInfo::kDefaultNobodyUid,
    "The uid of the owner of this eden instance");

DEFINE_int32(
    privhelper_gid,
    facebook::eden::UserInfo::kDefaultNobodyGid,
    "The gid of the owner of this eden instance");

int main(int argc, char** argv) {
#if defined(__linux__) || defined(__APPLE__)
  // One-shot mode used by EdenFS pressure GC to discover pinned inodes.
  // Handled before folly::Init so no flag or environment parsing happens on
  // this path: as a mode of a setuid binary it is invocable by any local
  // user, so it takes no input and reports only pins on the caller's own
  // mounts (see runScanPinsMode).
  //
  // The flag spelling matters: privhelper binaries that predate this mode
  // reject an unknown --flag with a clean exit(1) from gflags, whereas a
  // bare positional argument would fall through into server startup and
  // abort on the missing --privhelper_fd.
  if (argc == 2 && strcmp(argv[1], "--scan-pins") == 0) {
    return facebook::eden::runScanPinsMode();
  }
#endif // __linux__ || __APPLE__

  const folly::Init init(&argc, &argv);

  auto loggingConfig = folly::parseLogConfig(
      "WARN:default, eden=DBG2; default:stream=stderr,async=false");
  folly::LoggerDB::get().updateConfig(loggingConfig);

  // Escape the process group of whatever launched EdenFS, so that
  // process-group-wide cleanup (e.g. by agent command runners that
  // launched `eden restart`) cannot SIGKILL the privhelper out from under
  // a running daemon. See detachFromParentProcessGroup() for details.
  detachFromParentProcessGroup();

  PrivHelperServer server;
  try {
    // Redirect stdin
    folly::File devNullIn("/dev/null", O_RDONLY);
    auto retcode = folly::dup2NoInt(devNullIn.fd(), STDIN_FILENO);
    folly::checkUnixError(retcode, "failed to redirect stdin");

    folly::File serverConn(FLAGS_privhelper_fd, true);

    const auto realUid = getuid();
    const auto realGid = getgid();
    const auto cliUid = static_cast<uid_t>(FLAGS_privhelper_uid);
    const auto cliGid = static_cast<gid_t>(FLAGS_privhelper_gid);
    const auto owner = resolvePrivHelperOwner(realUid, realGid, cliUid, cliGid);

#ifdef __linux__
    // access() and faccessat2() use real IDs and supplementary groups.
    // Retain effective root for mount operations while checking the owner's
    // authority, including when launched through sudo.
    const auto hardeningDisabled = disablePrivHelperHardening();
    if (hardeningDisabled) {
      XLOG(WARN, "Privhelper hardening disabled; retaining launch credentials");
    }
    if (realUid == 0 && owner.uid != 0 && !hardeningDisabled) {
      const auto passwd = UserInfo::getPasswdUid(owner.uid);
      folly::checkUnixError(
          initgroups(passwd.pwd.pw_name, owner.gid),
          "failed to initialize privhelper owner groups");
      folly::checkUnixError(
          setregid(owner.gid, static_cast<gid_t>(-1)),
          "failed to set privhelper real gid");
      folly::checkUnixError(
          setreuid(owner.uid, static_cast<uid_t>(-1)),
          "failed to set privhelper real uid");
    }

    // Credential changes clear dumpability. Enable core dumps after setting
    // the owner so privhelper crashes can reach coredumper.
    if (prctl(PR_SET_DUMPABLE, 1, 0, 0, 0) != 0) {
      XLOGF(
          WARNING,
          "failed to mark privhelper dumpable: {}",
          folly::errnoStr(errno));
    }
#endif

    server.init(std::move(serverConn), owner.uid, owner.gid);
    server.run();
    return 0;
  } catch (const std::exception& ex) {
    XLOGF(ERR, "error inside mount helper: {}", folly::exceptionStr(ex));
  } catch (...) {
    XLOG(ERR, "invalid type thrown inside mount helper");
  }

  return 1;
}
