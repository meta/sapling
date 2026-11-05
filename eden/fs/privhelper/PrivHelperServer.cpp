/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#ifndef _WIN32

#include "eden/fs/privhelper/PrivHelperServer.h"
#include "eden/fs/privhelper/PinScan.h"
#include "eden/fs/privhelper/PrivHelperConn.h"
#include "eden/fs/privhelper/PrivHelperRollback.h"

#include <boost/algorithm/string/predicate.hpp>
#include <fcntl.h>
#include <folly/Exception.h>
#include <folly/Expected.h>
#include <folly/File.h>
#include <folly/FileUtil.h>
#include <folly/SocketAddress.h>
#include <folly/String.h>
#include <folly/Utility.h>
#include <folly/init/Init.h>
#include <folly/io/Cursor.h>
#include <folly/io/IOBuf.h>
#include <folly/io/async/AsyncSignalHandler.h>
#include <folly/io/async/EventBase.h>
#include <folly/logging/LogConfigParser.h>
#include <folly/logging/LoggerDB.h>
#include <folly/logging/xlog.h>
#include <folly/portability/Unistd.h>
#include <folly/system/ThreadName.h>
#ifndef __APPLE__
#include <linux/openat2.h>
#endif
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#ifndef __APPLE__
#include <sys/syscall.h>
#endif
#include <sys/types.h>
#include <algorithm>
#include <chrono>
#include <csignal>
#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/Throw.h"
#include "eden/fs/privhelper/NfsMountRpc.h"
#include "eden/fs/privhelper/RestartSentinel.h"
#include "eden/fs/privhelper/priority/ProcessPriority.h"
#include "eden/fs/utils/MountInfoTable.h"

#ifdef __APPLE__
#include <CoreFoundation/CoreFoundation.h> // @manual
#include <IOKit/kext/KextManager.h> // @manual
#include <sys/ioccom.h> // @manual
#endif

using folly::checkUnixError;
using folly::IOBuf;
using folly::throwSystemError;
using folly::io::Appender;
using folly::io::Cursor;
using folly::io::RWPrivateCursor;
using std::string;

namespace facebook::eden {

// Constants
static constexpr folly::StringPiece kFamBinaryPath{
    "/usr/local/libexec/eden/edenfs_fam/SCMFileAccessMonitor.app/Contents/MacOS/SCMFileAccessMonitor"};

namespace {

#ifndef __APPLE__
constexpr int kRegisteredMountRootOpenFlags = O_PATH | O_DIRECTORY | O_CLOEXEC;
#endif

#ifndef __APPLE__
#ifndef OPEN_TREE_CLONE
#define OPEN_TREE_CLONE 1
#endif
#ifndef OPEN_TREE_CLOEXEC
#define OPEN_TREE_CLOEXEC O_CLOEXEC
#endif
#ifndef MOVE_MOUNT_F_EMPTY_PATH
#define MOVE_MOUNT_F_EMPTY_PATH 0x00000004
#endif
#ifndef MOVE_MOUNT_T_EMPTY_PATH
#define MOVE_MOUNT_T_EMPTY_PATH 0x00000040
#endif
#ifndef FSOPEN_CLOEXEC
#define FSOPEN_CLOEXEC 0x00000001
#endif
#ifndef FSCONFIG_SET_FLAG
#define FSCONFIG_SET_FLAG 0
#endif
#ifndef FSCONFIG_SET_STRING
#define FSCONFIG_SET_STRING 1
#endif
#ifndef FSCONFIG_CMD_CREATE
#define FSCONFIG_CMD_CREATE 6
#endif
#ifndef FSMOUNT_CLOEXEC
#define FSMOUNT_CLOEXEC 0x00000001
#endif
#ifndef MOUNT_ATTR_RDONLY
#define MOUNT_ATTR_RDONLY 0x00000001
#endif
#ifndef MOUNT_ATTR_NODEV
#define MOUNT_ATTR_NODEV 0x00000004
#endif
#ifndef MOUNT_ATTR_NOSUID
#define MOUNT_ATTR_NOSUID 0x00000002
#endif

std::string bindMountTargetSuffix(
    folly::StringPiece mountRoot,
    folly::StringPiece mountPath) {
  auto suffix = mountPath.subpiece(mountRoot.size());
  if (suffix.empty()) {
    return ".";
  }
  XCHECK_EQ('/', suffix.front());
  suffix.advance(1);
  return suffix.empty() ? "." : suffix.str();
}

void fsConfigSet(
    int fsFd,
    unsigned int command,
    const char* key,
    const char* value,
    int aux = 0) {
  const auto rc =
      static_cast<int>(syscall(SYS_fsconfig, fsFd, command, key, value, aux));
  checkUnixError(
      rc,
      "failed to configure filesystem context option `",
      key ? key : "<create>",
      "`");
}

void fsConfigFlag(int fsFd, folly::StringPiece key) {
  const auto keyStr = key.str();
  fsConfigSet(fsFd, FSCONFIG_SET_FLAG, keyStr.c_str(), nullptr);
}

void fsConfigString(
    int fsFd,
    folly::StringPiece key,
    folly::StringPiece value) {
  const auto keyStr = key.str();
  const auto valueStr = value.str();
  fsConfigSet(fsFd, FSCONFIG_SET_STRING, keyStr.c_str(), valueStr.c_str());
}

void fsConfigCommaSeparatedOptions(int fsFd, folly::StringPiece mountOptions) {
  std::vector<folly::StringPiece> options;
  folly::split(',', mountOptions, options, /*ignoreEmpty=*/true);
  for (const auto& option : options) {
    const auto separator = option.find('=');
    if (separator == folly::StringPiece::npos) {
      fsConfigFlag(fsFd, option);
      continue;
    }
    fsConfigString(
        fsFd, option.subpiece(0, separator), option.subpiece(separator + 1));
  }
}

folly::File fsOpen(folly::StringPiece fsType) {
  const auto fsTypeStr = fsType.str();
  const auto fsFd =
      static_cast<int>(syscall(SYS_fsopen, fsTypeStr.c_str(), FSOPEN_CLOEXEC));
  checkUnixError(fsFd, "failed to open filesystem context for `", fsType, "`");
  return folly::File{fsFd, /*ownsFd=*/true};
}

folly::File fsMount(int fsFd, bool readOnly) {
  unsigned int mountAttrs = MOUNT_ATTR_NOSUID;
  if (!disablePrivHelperHardening()) {
    mountAttrs |= MOUNT_ATTR_NODEV;
  }
  if (readOnly) {
    mountAttrs |= MOUNT_ATTR_RDONLY;
  }

  const auto mountFd =
      static_cast<int>(syscall(SYS_fsmount, fsFd, FSMOUNT_CLOEXEC, mountAttrs));
  checkUnixError(mountFd, "failed to create detached mount");
  return folly::File{mountFd, /*ownsFd=*/true};
}

void moveDetachedMount(
    const folly::File& detachedMount,
    const folly::File& targetFd,
    const char* mountPath) {
  const auto rc = static_cast<int>(syscall(
      SYS_move_mount,
      detachedMount.fd(),
      "",
      targetFd.fd(),
      "",
      MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_EMPTY_PATH));
  checkUnixError(rc, "failed to attach detached mount over `", mountPath, "`");
}

int linuxUnmountFlags(UnmountOptions options, bool noFollow) {
  int umountFlags = (noFollow ? UMOUNT_NOFOLLOW : 0) | MNT_DETACH;

  // Only "force" is checked because as this is implemented, we only plan to
  // add "--no-force" as an option. The other options are not checked until
  // we need to support valid use cases for them.
  if (!options.detach || options.expire) {
    XLOGF(
        DFATAL,
        "Unsupported unmount option provided: 'detach'{}",
        options.detach);
  }
  if (options.expire) {
    XLOGF(
        DFATAL,
        "Unsupported unmount option provided: 'expire'{}",
        options.expire);
  }
  if (options.force) {
    umountFlags |= MNT_FORCE;
  }
  return umountFlags;
}

#endif

void logUnmountError(const char* mountPath, int errnum) {
  // EINVAL simply means the path is no longer mounted. This can happen if it
  // was already manually unmounted by a separate process.
  if (errnum != EINVAL) {
    XLOGF(
        WARNING, "error unmounting {}: {}", mountPath, folly::errnoStr(errnum));
  }
}

} // namespace

PrivHelperServer::RegisteredMount PrivHelperServer::openRegisteredMount(
    const std::string& mountPath) {
#ifdef __APPLE__
  (void)mountPath;
  return RegisteredMount{};
#else
  const auto rootFd = open(mountPath.c_str(), kRegisteredMountRootOpenFlags);
  checkUnixError(
      rootFd, "failed to open registered mount root `", mountPath, "`");
  XLOGF(DBG2, "Opened registered mount root `{}` by fd", mountPath);
  return RegisteredMount{folly::File{rootFd, /*ownsFd=*/true}};
#endif
}

void PrivHelperServer::registerMountPoint(const std::string& mountPath) {
  mountPoints_.erase(mountPath);
  mountPoints_.emplace(mountPath, openRegisteredMount(mountPath));
}

void PrivHelperServer::registerMountPoint(
    const std::string& mountPath,
    RegisteredMount registeredMount) {
  XLOGF(DBG2, "Registered mount root `{}` from existing mount fd", mountPath);
  mountPoints_.erase(mountPath);
  mountPoints_.emplace(mountPath, std::move(registeredMount));
}

void PrivHelperServer::unmountRegisteredMount(
    const std::string& mountPath,
    const RegisteredMount& registeredMount,
    UnmountOptions options) {
#ifdef __APPLE__
  (void)registeredMount;
#else
  if (useModernMountApi()) {
    const auto procFdPath =
        fmt::format("/proc/self/fd/{}", registeredMount.rootFd.fd());
    // Filesystem errors must not restore authority to the original pathname.
    XLOGF(
        DBG2,
        "Unmounting registered mount `{}` through `{}`",
        mountPath,
        procFdPath);
    const auto rc =
        umount2(procFdPath.c_str(), linuxUnmountFlags(options, false));
    if (rc != 0) {
      logUnmountError(mountPath.c_str(), errno);
    }
    return;
  }
#endif
  unmount(mountPath.c_str(), options);
}

folly::File PrivHelperServer::openBindMountTarget(
    folly::StringPiece mountRoot,
    folly::StringPiece mountPath) {
#ifdef __APPLE__
  (void)mountRoot;
  (void)mountPath;
  throw std::runtime_error("this system does not support bind mounts");
#else
  const auto mountPoint = mountPoints_.find(mountRoot.str());
  if (mountPoint == mountPoints_.end()) {
    throwf<std::domain_error>("No FUSE mount found for {}", mountRoot);
  }

  const auto suffix = bindMountTargetSuffix(mountRoot, mountPath);
  struct open_how targetHow{};
  targetHow.flags = O_PATH | O_DIRECTORY | O_CLOEXEC;
  // Resolve the target relative to the registered mount root in the same
  // kernel operation that returns the fd. This rejects ".." escapes and
  // symlink components without a separate check-then-open window.
  targetHow.resolve = RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS;
  auto targetFdNum = static_cast<int>(syscall(
      SYS_openat2,
      mountPoint->second.rootFd.fd(),
      suffix.c_str(),
      &targetHow,
      sizeof(targetHow)));
  checkUnixError(
      targetFdNum, "failed to open bind mount target `", mountPath, "`");
  return folly::File{targetFdNum, /*ownsFd=*/true};
#endif
}

namespace {
uint64_t currentEpochSeconds() {
  return static_cast<uint64_t>(
      std::chrono::duration_cast<std::chrono::seconds>(
          std::chrono::system_clock::now().time_since_epoch())
          .count());
}

/** Set key in env, replacing every entry already there for it. */
void setEnv(
    std::vector<std::pair<std::string, std::string>>& env,
    folly::StringPiece key,
    std::string value) {
  // Every match goes, not just the first: spawnEdenFs() applies the pairs in
  // order, so a duplicate left behind would overwrite what this sets.
  const auto removed =
      std::remove_if(env.begin(), env.end(), [key](const auto& entry) {
        return entry.first == key;
      });
  env.erase(removed, env.end());
  env.emplace_back(key.str(), std::move(value));
}

bool spawnEdenFs(
    const AbsolutePath& binary,
    const std::vector<std::string>& argv,
    const std::vector<std::pair<std::string, std::string>>& env) {
  SpawnedProcess::Options opts;
  opts.executablePath(binary);
  opts.nullStdin();
  opts.resetIds();

  auto& environment = opts.environment();
  environment.clear();
  for (const auto& [key, value] : env) {
    environment.set(key, value);
  }

  try {
    SpawnedProcess proc(argv, std::move(opts));
    XLOGF(
        INFO,
        "relaunched edenfs as pid {}; waiting for startup to finish",
        proc.pid());
    const auto status = proc.waitOrTerminateOrKill(
        kSupervisorStartupTimeout, kRestartTerminationTimeout);
    if (status.state() != ProcessStatus::Exited || status.exitStatus() != 0) {
      throw std::runtime_error(
          folly::to<std::string>("relaunched edenfs process ", status.str()));
    }
    return true;
  } catch (const std::exception& ex) {
    XLOGF(ERR, "failed to relaunch edenfs: {}", folly::exceptionStr(ex));
    return false;
  }
}
} // namespace

PrivHelperServer::PrivHelperServer()
    : spawnEdenFs_{spawnEdenFs}, now_{currentEpochSeconds} {}

PrivHelperServer::~PrivHelperServer() = default;

void detachFromParentProcessGroup() {
  if (setsid() == static_cast<pid_t>(-1)) {
    XLOGF(
        WARN,
        "privhelper failed to detach into a new session: {}",
        folly::errnoStr(errno));
  }
}

void PrivHelperServer::init(folly::File socket, uid_t uid, gid_t gid) {
  initPartial(std::move(socket), uid, gid);
}

void PrivHelperServer::initPartial(folly::File socket, uid_t uid, gid_t gid) {
  // Make sure init() is only called once.
  XCHECK_EQ(uid_, std::numeric_limits<uid_t>::max());
  XCHECK_EQ(gid_, std::numeric_limits<gid_t>::max());

  // Set our thread name to make it easier to distinguish
  // the privhelper process from the main EdenFS process.  Setting the thread
  // name for the main thread also changes the process name reported
  // /proc/PID/comm (and therefore by ps).
  //
  // Note that the process name is limited to 15 bytes on Linux, so our process
  // name shows up only as "edenfs_privhelp"
  folly::setThreadName("edenfs_privhelper");

  // eventBase_ is a unique_ptr only so that we can delay constructing it until
  // init() is called.  We want to avoid creating it in the constructor since
  // the constructor is often called before we fork, and the EventBase
  // NotificationQueue code checks to ensure that it isn't used across a fork.
  eventBase_ = std::make_unique<folly::EventBase>();
  conn_ = UnixSocket::makeUnique(eventBase_.get(), std::move(socket));
  uid_ = uid;
  gid_ = gid;
  sentinel_.emplace(uid);

  folly::checkPosixError(chdir("/"), "privhelper failed to chdir(/)");
}

#ifdef __APPLE__
namespace {

constexpr folly::StringPiece kNfsExtensionPath =
    "/System/Library/Extensions/nfs.kext";

bool shouldLoadNfsKext() {
  if (access(kNfsExtensionPath.str().c_str(), F_OK) != 0) {
    XLOGF(
        DBG3,
        "Kernel extension does not exist at '{}', skipping",
        kNfsExtensionPath);
    return false;
  }

  struct vfsconf vfc;
  return getvfsbyname("nfs", &vfc) != 0;
}

bool tryLoadKext(const std::string& kextPathString) {
  CFStringRef kextPath = CFStringCreateWithCString(
      kCFAllocatorDefault, kextPathString.c_str(), kCFStringEncodingUTF8);
  SCOPE_EXIT {
    CFRelease(kextPath);
  };

  CFURLRef kextUrl = CFURLCreateWithFileSystemPath(
      kCFAllocatorDefault, kextPath, kCFURLPOSIXPathStyle, true);
  SCOPE_EXIT {
    CFRelease(kextUrl);
  };

  auto ret = KextManagerLoadKextWithURL(kextUrl, NULL);

  if (ret != kOSReturnSuccess) {
    XLOGF(ERR, "Failed to load {}: error code {}", kextPathString, ret);
    return false;
  }

  return true;
}

bool loadNfsKext() {
  return tryLoadKext(kNfsExtensionPath.str());
}

} // namespace
#endif

#ifndef __APPLE__
namespace {
folly::File openLinuxFuseDevice() {
  // We manually call open() here rather than using the folly::File()
  // constructor just so we can emit a slightly more helpful message on error.
  const char* devName = "/dev/fuse";
  const int fd = folly::openNoInt(devName, O_RDWR | O_CLOEXEC);
  if (fd < 0) {
    if (errno == ENODEV || errno == ENOENT) {
      throwSystemError(
          "failed to open ",
          devName,
          ": make sure the fuse kernel module is loaded");
    } else {
      throwSystemError("failed to open ", devName);
    }
  }
  return folly::File{fd, /*ownsFd=*/true};
}

std::string makeLinuxNfsMountOptions(const NFSMountOptions& options) {
  // Since each mount point will have its own NFS server, we need to manually
  // specify it.
  folly::StringPiece noReaddirplusStr = ",nordirplus,";
  if (options.useReaddirplus) {
    noReaddirplusStr = ",";
  }

  // Check if we should use a soft or hard mount.
  // https://linux.die.net/man/5/nfs
  folly::StringPiece softOptionStr = "hard";
  if (options.useSoftMount) {
    softOptionStr = "soft";
  }

  return fmt::format(
      "addr={},vers=3,proto=tcp,port={},mountvers=3,mountproto=tcp,mountport={},"
      "noresvport,nolock{}{},retrans={},timeo={},rsize={},wsize={}",
      options.nfsdAddr.getAddressStr(),
      options.nfsdAddr.getPort(),
      options.mountdAddr.getPort(),
      noReaddirplusStr,
      softOptionStr,
      options.retransmitAttempts,
      options.retransmitTimeoutTenthSeconds,
      options.readIOSize,
      options.writeIOSize);
}

/**
 * Configure FUSE read-ahead by writing to
 * /sys/class/bdi/{major}:{minor}/read_ahead_kb
 */
void configureFuseReadAhead(
    const char* mountPath,
    std::optional<int> mountFd,
    uint32_t readAheadKb) {
  auto result = getMountInfoForPath(mountPath);
  if (result.hasError()) {
    XLOGF(
        WARN,
        "Cannot determine device for {}: {}; skipping BDI config",
        mountPath,
        folly::errnoStr(result.error()));
    return;
  }
  if (!result.value().has_value()) {
    XLOGF(WARN, "No mount found for {}, skipping BDI config", mountPath);
    return;
  }
  const auto& info = result.value().value();
  if (mountFd) {
    if (info.fsType != "fuse" &&
        !folly::StringPiece(info.fsType).startsWith("fuse.")) {
      throwf<std::domain_error>("{} is not a FUSE mount", mountPath);
    }

    // Cached metadata avoids requiring a response from the FUSE daemon.
    struct statx st{};
    checkUnixError(
        statx(*mountFd, "", AT_EMPTY_PATH | AT_STATX_DONT_SYNC, STATX_INO, &st),
        "stat registered mount ",
        mountPath);
    if (st.stx_dev_major != info.devMajor ||
        st.stx_dev_minor != info.devMinor) {
      throwf<std::domain_error>(
          "{} no longer matches the registered mount", mountPath);
    }
  }

  auto bdiPath = fmt::format(
      "/sys/class/bdi/{}:{}/read_ahead_kb", info.devMajor, info.devMinor);

  auto valueStr = fmt::format("{}", readAheadKb);
  if (!folly::writeFile(valueStr, bdiPath.c_str())) {
    throwSystemError(
        fmt::format("Failed to write {} to {}", valueStr, bdiPath));
  }

  XLOGF(
      DBG2,
      "Configured FUSE read-ahead to {} KB for {}:{}",
      readAheadKb,
      info.devMajor,
      info.devMinor);
}
} // namespace
#endif

folly::File PrivHelperServer::fuseMount(
    const char* mountPath,
    bool readOnly,
    [[maybe_unused]] const char* vfsType) {
#ifdef __APPLE__
  (void)mountPath;
  (void)readOnly;
  folly::throwSystemErrorExplicit(
      ENOTSUP, "FUSE mounts are not supported on macOS");
#else
  auto fuseDev = openLinuxFuseDevice();

  // Prepare the flags and options to pass to mount(2).
  // We currently don't allow these to be customized by the unprivileged
  // requester.  We could add this functionality in the future if we have a
  // need for it, but we would need to validate their changes are safe.
  const int rootMode = S_IFDIR;
  auto mountOpts = fmt::format(
      "allow_other,default_permissions,"
      "rootmode={:o},user_id={},group_id={},fd={}",
      rootMode,
      uid_,
      gid_,
      fuseDev.fd());

  // Bind redirections inherit their source flags independently of this mount.
  int mountFlags = MS_NOSUID;
  if (!disablePrivHelperHardening()) {
    mountFlags |= MS_NODEV;
  }
  if (readOnly) {
    mountFlags |= MS_RDONLY;
  }
  int rc = mount(
      kEdenFsMountSource, mountPath, vfsType, mountFlags, mountOpts.c_str());
  checkUnixError(rc, "failed to mount");
  return fuseDev;
#endif
}

#ifndef __APPLE__
PrivHelperServer::FuseMountResult PrivHelperServer::fuseMountByFd(
    folly::File targetFd,
    const char* mountPath,
    bool readOnly,
    const char* vfsType) {
  XLOGF(
      DBG1,
      "Mounting `{}` via fd-native FUSE with type `{}`; readOnly={}",
      mountPath,
      vfsType,
      readOnly);
  auto fuseDev = openLinuxFuseDevice();

  const int rootMode = S_IFDIR;
  auto mountOpts = fmt::format(
      "allow_other,default_permissions,"
      "rootmode={:o},user_id={},group_id={},fd={}",
      rootMode,
      uid_,
      gid_,
      fuseDev.fd());

  // fsopen(2) resolves only the base filesystem type. Unlike mount(2), it does
  // not turn a dotted type such as "fuse.edenfs" into a FUSE subtype, so pass
  // the suffix explicitly to keep the advertised type identical on both paths.
  folly::StringPiece fsType{vfsType};
  folly::StringPiece subtype;
  if (const auto dot = fsType.find('.'); dot != folly::StringPiece::npos) {
    subtype = fsType.subpiece(dot + 1);
    fsType = fsType.subpiece(0, dot);
  }
  auto fsFd = fsOpen(fsType);
  fsConfigString(fsFd.fd(), "source", kEdenFsMountSource);
  if (!subtype.empty()) {
    fsConfigString(fsFd.fd(), "subtype", subtype);
  }
  fsConfigCommaSeparatedOptions(fsFd.fd(), mountOpts);
  fsConfigSet(fsFd.fd(), FSCONFIG_CMD_CREATE, nullptr, nullptr);

  auto mountFd = fsMount(fsFd.fd(), readOnly);
  // Attach the detached mount to the already-open target fd, then keep that
  // mount fd as the registered root for future bind redirects.
  moveDetachedMount(mountFd, targetFd, mountPath);
  return FuseMountResult{
      std::move(fuseDev),
      RegisteredMount{std::move(mountFd)},
  };
}
#endif

bool needsFstypeOverride(const NFSMountOptions& options) {
  return options.nfsdAddr.getFamily() == AF_UNIX;
}

#ifdef __APPLE__
typedef char fstypename_t[MFSTYPENAMELEN];
#define FSIOC_SET_FSTYPENAME_OVERRIDE _IOW('A', 10, fstypename_t)
#define FSCTL_SET_FSTYPENAME_OVERRIDE IOCBASECMD(FSIOC_SET_FSTYPENAME_OVERRIDE)
#endif

void PrivHelperServer::nfsMount(
    std::string mountPath,
    NFSMountOptions options) {
#ifdef __APPLE__
  if (shouldLoadNfsKext()) {
    XLOG(DBG3, "Apple nfs.kext is not loaded. Attempting to load");
    loadNfsKext();
  }

  // Hold the attribute list set below.
  auto attrsBuf = folly::IOBufQueue{folly::IOBufQueue::cacheChainLength()};
  folly::io::QueueAppender attrSer{&attrsBuf, 1024};

  // Holds the NFS_MATTR_* flags. Each set flag will have a corresponding
  // structure serialized in the attribute list, the order of serialization
  // must follow the increasing order of their associated flags.
  uint32_t mattrFlags = 0;

  // Check if we should use a soft or hard mount. If soft is desired, set the
  // flag value to NFS_MFLAG_SOFT so it's used in the bitwise OR operation.
  std::string softStr = "hard";
  uint32_t soft_flag = 0;
  if (options.useSoftMount) {
    softStr = "soft";
    soft_flag = NFS_MFLAG_SOFT;
  }

  // unmask_dumbtimer indicates whether we should mask/unmask the dumbtimer
  // value bit in the nfs_matter_flags (i.e. nfs_flag_set) struct.
  // dumbtimer_flag indicates the actual value that should be masked/unmasked
  // depending on the value of unmask_dumbtimer.
  //
  // This separate mask value is needed because dumbtimer is not always passed
  // as a mount option, and therefore the value bit may be invalid.
  uint32_t dumbtimer_flag = 0;
  uint32_t unmask_dumbtimer = 0;
  std::string dumbtimerStr = "nodumbtimer,";
  if (options.dumbtimer.has_value()) {
    unmask_dumbtimer = NFS_MFLAG_DUMBTIMER;
    if (options.dumbtimer.value()) {
      dumbtimer_flag = NFS_MFLAG_DUMBTIMER;
      dumbtimerStr = "dumbtimer,";
    }
  }

  // Check if we should enable readdirplus. If so, set readdirplus to
  // NFS_MFLAG_RDIRPLUS.
  std::string rdirplusStr = "nordirplus";
  uint32_t readdirplus_flag = 0;
  if (options.useReaddirplus) {
    rdirplusStr = "rdirplus";
    readdirplus_flag = NFS_MFLAG_RDIRPLUS;
  }

  /*
   * The flag set does the following:
   *
   * Makes the client use any source port
   * Enables or disables rdirplus (readdirplus) based on EdenConfig value
   * Sets the mount type to soft/hard (but make it interruptible) based on an
   *   EdenConfig value. While in theory we would always want the mount to be
   *   soft, the macOS kernel gives up on a soft-mount request at most 30s
   *   (NFS_MAXTIMEO / 2) after EdenFS stops answering promptly, which in some
   *   cases is too short for files to be fetched, thus make it configurable.
   * Possibly specifies dumbtimer behavior, iff an EdenConfig value is
   *   explicitly set.
   * Suppresses macOS kernel JUKEBOX retry logging (NFS_MFLAG_MUTEJUKEBOX),
   *   since EdenFS uses NFS3ERR_JUKEBOX intentionally for backpressure.
   *
   * See `man mount_nfs` for more options.
   */
  mattrFlags |= NFS_MATTR_FLAGS;
  nfs_mattr_flags flags{
      NFS_MATTR_BITMAP_LEN,
      NFS_MFLAG_RESVPORT | NFS_MFLAG_RDIRPLUS | NFS_MFLAG_SOFT |
          NFS_MFLAG_INTR | NFS_MFLAG_MUTEJUKEBOX | unmask_dumbtimer,
      NFS_MATTR_BITMAP_LEN,
      NFS_MFLAG_INTR | NFS_MFLAG_MUTEJUKEBOX | readdirplus_flag | soft_flag |
          dumbtimer_flag};
  XdrTrait<nfs_mattr_flags>::serialize(attrSer, flags);

  mattrFlags |= NFS_MATTR_NFS_VERSION;
  XdrTrait<nfs_mattr_nfs_version>::serialize(attrSer, 3);

  mattrFlags |= NFS_MATTR_READ_SIZE;
  mattrFlags |= NFS_MATTR_WRITE_SIZE;
  XdrTrait<nfs_mattr_rsize>::serialize(attrSer, options.readIOSize);
  XdrTrait<nfs_mattr_wsize>::serialize(attrSer, options.writeIOSize);

  std::string dsizeStr = "";
  if (options.directoryReadSize.has_value()) {
    dsizeStr = fmt::format("dsize={},", options.directoryReadSize.value());
    mattrFlags |= NFS_MATTR_READDIR_SIZE;
    XdrTrait<nfs_mattr_readdirsize>::serialize(
        attrSer, options.directoryReadSize.value());
  }

  std::string readAheadStr = "";
  readAheadStr = fmt::format("readahead={},", options.readAheadSize);
  mattrFlags |= NFS_MATTR_READAHEAD;
  XdrTrait<nfs_mattr_readahead>::serialize(attrSer, options.readAheadSize);

  // For NFSv2/v3 mounts, perform all file locking operations locally on the NFS
  // client (in the VFS layer) instead of on the NFS server.  This option can
  // provide file locking support on an NFS file system for which the server
  // does not support file locking. However, because the file locking is only
  // performed on the client, the NFS server and other NFS clients will have no
  // knowledge of the locks.
  mattrFlags |= NFS_MATTR_LOCK_MODE;
  XdrTrait<nfs_mattr_lock_mode>::serialize(
      attrSer, nfs_lock_mode::NFS_LOCK_MODE_LOCAL);

  auto mountdFamily = options.mountdAddr.getFamily();
  auto nfsdFamily = options.nfsdAddr.getFamily();
  if (mountdFamily != nfsdFamily) {
    throwf<std::runtime_error>(
        "The mountd and nfsd socket must be of the same type: mountd=\"{}\", nfsd=\"{}\"",
        options.mountdAddr.describe(),
        options.nfsdAddr.describe());
  }

  mattrFlags |= NFS_MATTR_SOCKET_TYPE;
  nfs_mattr_socket_type socketType;
  switch (nfsdFamily) {
    case AF_INET:
      socketType = "tcp4";
      break;
    case AF_INET6:
      socketType = "tcp6";
      break;
    case AF_UNIX:
      socketType = "ticotsord";
      break;
    default:
      throwf<std::runtime_error>("Unknown socket family: {}", nfsdFamily);
  }
  XdrTrait<nfs_mattr_socket_type>::serialize(attrSer, socketType);

  if (options.nfsdAddr.isFamilyInet()) {
    mattrFlags |= NFS_MATTR_NFS_PORT;
    XdrTrait<nfs_mattr_nfs_port>::serialize(
        attrSer, options.nfsdAddr.getPort());

    mattrFlags |= NFS_MATTR_MOUNT_PORT;
    XdrTrait<nfs_mattr_mount_port>::serialize(
        attrSer, options.mountdAddr.getPort());
  }

  // NOTE: This code currently has a bug that prevents timeouts from being set
  // to non-multiples of 10 deciseconds (i.e. a timeout of 25 deciseconds will
  // result in a mount with a timeout of 20 deciseconds). This should be
  // investigated and fixed in the future.
  //
  // Initial RPC request timeout value in tenths of a second, so 10 = 1s.
  std::string retransTimeoutStr = "";
  retransTimeoutStr =
      fmt::format("timeo={},", options.retransmitTimeoutTenthSeconds);
  mattrFlags |= NFS_MATTR_REQUEST_TIMEOUT;
  int32_t retransSeconds = options.retransmitTimeoutTenthSeconds / 10;
  uint32_t retransRemainderNanoseconds =
      (options.retransmitTimeoutTenthSeconds % 10) * 100000000;
  XdrTrait<nfs_mattr_request_timeout>::serialize(
      attrSer,
      nfstime32{/*seconds=*/retransSeconds,
                /*nseconds=*/retransRemainderNanoseconds});

  // Indicates the max RPC retransmissions for soft mounts
  std::string retransAttemptsStr = "";
  retransAttemptsStr = fmt::format("retrans={},", options.retransmitAttempts);
  mattrFlags |= NFS_MATTR_SOFT_RETRY_COUNT;
  XdrTrait<nfs_mattr_soft_retry_count>::serialize(
      attrSer, options.retransmitAttempts);

  std::string deadTimeoutStr = "";
  deadTimeoutStr = fmt::format("deadtimeout={}", options.deadTimeoutSeconds);
  mattrFlags |= NFS_MATTR_DEAD_TIMEOUT;
  XdrTrait<nfs_mattr_dead_timeout>::serialize(
      attrSer,
      nfstime32{/*seconds=*/options.deadTimeoutSeconds, /*nseconds=*/0});

  mattrFlags |= NFS_MATTR_FS_LOCATIONS;
  auto path = canonicalPath(mountPath);
  auto componentIterator = path.components();
  std::vector<std::string> components;
  for (const auto component : componentIterator) {
    components.push_back(std::string(component.value()));
  }
  nfs_fs_server server{"edenfs", {}, std::nullopt};
  if (options.nfsdAddr.isFamilyInet()) {
    server.nfss_address.push_back(options.nfsdAddr.getAddressStr());
  } else {
    server.nfss_address.push_back(options.nfsdAddr.getPath());
  }
  nfs_fs_location location{{server}, components};
  nfs_mattr_fs_locations locations{{location}, std::nullopt};
  XdrTrait<nfs_mattr_fs_locations>::serialize(attrSer, locations);

  mattrFlags |= NFS_MATTR_MNTFLAGS;
  // These are non-NFS specific and will be also passed directly to mount(2)
  nfs_mattr_mntflags mountFlags = MNT_NOSUID;
  if (!disablePrivHelperHardening()) {
    mountFlags |= MNT_NODEV;
  }
  if (options.readOnly) {
    mountFlags |= MNT_RDONLY;
  }
  XdrTrait<nfs_mattr_mntflags>::serialize(attrSer, mountFlags);

  mattrFlags |= NFS_MATTR_MNTFROM;
  nfs_mattr_mntfrom serverName = "edenfs:";
  XdrTrait<nfs_mattr_mntfrom>::serialize(attrSer, serverName);

  if (options.nfsdAddr.getFamily() == AF_UNIX) {
    mattrFlags |= NFS_MATTR_LOCAL_NFS_PORT;
    XdrTrait<std::string>::serialize(attrSer, options.nfsdAddr.getPath());

    mattrFlags |= NFS_MATTR_LOCAL_MOUNT_PORT;
    XdrTrait<std::string>::serialize(attrSer, options.mountdAddr.getPath());
  }

  auto mountBuf = folly::IOBufQueue{folly::IOBufQueue::cacheChainLength()};
  folly::io::QueueAppender ser(&mountBuf, 1024);

  nfs_mattr mattr{NFS_MATTR_BITMAP_LEN, mattrFlags, attrsBuf.move()};

  nfs_mount_args args{
      /*args_version*/ 88,
      /*args_length*/ 0,
      /*xdr_args_version*/ NFS_XDRARGS_VERSION_0,
      /*nfs_mount_attrs*/ std::move(mattr),
  };

  auto argsLength = XdrTrait<nfs_mount_args>::serializedSize(args);
  args.args_length = folly::to_narrow(argsLength);

  XdrTrait<nfs_mount_args>::serialize(ser, args);

  auto buf = mountBuf.move();
  buf->coalesce();

  XLOGF(
      DBG1,
      "Mounting {} via NFS with opts: mountaddr={},addr={},rsize={},wsize={},{},{},vers=3,{}{}{}{}{}{}",
      mountPath,
      options.mountdAddr.describe(),
      options.nfsdAddr.describe(),
      options.readIOSize,
      options.writeIOSize,
      softStr,
      rdirplusStr,
      dumbtimerStr,
      dsizeStr,
      readAheadStr,
      retransTimeoutStr,
      retransAttemptsStr,
      deadTimeoutStr);

  int rc = mount("nfs", mountPath.c_str(), mountFlags, (void*)buf->data());
  checkUnixError(rc, "failed to mount");

  /*
   * The fsctl syscall is completely undocumented, but it does contain a way to
   * override the f_fstypename returned by statfs. This allows watchman to
   * properly detect the filesystem as EdenFS and not NFS (watchman refuses to
   * watch an NFS filesystem).
   *
   * Only mounts that lose their "edenfs:" mount source need it, because macOS
   * guards the syscall: a setuid-root privhelper leaves an EXC_GUARD crash
   * report on every mount, and on macOS 27.2 a non-root privhelper gets EPERM.
   */
  if (nfsFstypeOverride_.load(std::memory_order_relaxed) &&
      needsFstypeOverride(options)) {
    rc = fsctl(
        mountPath.c_str(), FSCTL_SET_FSTYPENAME_OVERRIDE, (void*)"edenfs:", 0);
    if (rc != 0) {
      unmount(mountPath.c_str(), {});
      checkUnixError(rc, "failed to fsctl");
    }
  }

#else
  if (!options.mountdAddr.isFamilyInet() || !options.nfsdAddr.isFamilyInet()) {
    folly::throwSystemErrorExplicit(
        EINVAL,
        fmt::format(
            "only inet addresses are supported: mountdAddr=\"{}\", nfsdAddr=\"{}\"",
            options.mountdAddr.describe(),
            options.nfsdAddr.describe()));
  }
  auto mountOpts = makeLinuxNfsMountOptions(options);

  // Bind redirections inherit their source flags independently of this mount.
  int mountFlags = MS_NOSUID;
  if (!disablePrivHelperHardening()) {
    mountFlags |= MS_NODEV;
  }
  if (options.readOnly) {
    mountFlags |= MS_RDONLY;
  }
  auto source = fmt::format("edenfs:{}", mountPath);
  XLOGF(DBG1, "Mounting {} va NFS with opts: {}", source, mountOpts);

  int rc = mount(
      source.c_str(), mountPath.c_str(), "nfs", mountFlags, mountOpts.c_str());
  checkUnixError(rc, "failed to mount");
#endif
}

#ifndef __APPLE__
PrivHelperServer::RegisteredMount PrivHelperServer::nfsMountByFd(
    folly::File targetFd,
    const std::string& mountPath,
    const NFSMountOptions& options) {
  if (!options.mountdAddr.isFamilyInet() || !options.nfsdAddr.isFamilyInet()) {
    folly::throwSystemErrorExplicit(
        EINVAL,
        fmt::format(
            "only inet addresses are supported: mountdAddr=\"{}\", nfsdAddr=\"{}\"",
            options.mountdAddr.describe(),
            options.nfsdAddr.describe()));
  }

  auto mountOpts = makeLinuxNfsMountOptions(options);
  auto source = fmt::format("edenfs:{}", mountPath);
  XLOGF(
      DBG1,
      "Mounting {} via fd-native NFS with opts: {}; readOnly={}",
      source,
      mountOpts,
      options.readOnly);

  auto fsFd = fsOpen("nfs");
  fsConfigString(fsFd.fd(), "source", source);
  fsConfigCommaSeparatedOptions(fsFd.fd(), mountOpts);
  fsConfigSet(fsFd.fd(), FSCONFIG_CMD_CREATE, nullptr, nullptr);

  auto mountFd = fsMount(fsFd.fd(), options.readOnly);
  moveDetachedMount(mountFd, targetFd, mountPath.c_str());
  return RegisteredMount{std::move(mountFd)};
}
#endif

void PrivHelperServer::insecureBindMount(
    const char* clientPath,
    const char* mountPath) {
#ifdef __APPLE__
  (void)clientPath;
  (void)mountPath;
  throw std::runtime_error("this system does not support bind mounts");
#else
  // This path is kept only for the root-controlled rollback marker. mount(2)
  // resolves mountPath by string, so it cannot close the target-path race.
  const int rc =
      mount(clientPath, mountPath, /*type*/ nullptr, MS_BIND, /*data*/ nullptr);
  checkUnixError(
      rc, "failed to bind mount `", clientPath, "` over `", mountPath, "`");
#endif
}

void PrivHelperServer::bindMount(
    const char* clientPath,
    const char* mountPath,
    folly::StringPiece mountRoot) {
#ifdef __APPLE__
  (void)clientPath;
  (void)mountPath;
  (void)mountRoot;
  throw std::runtime_error("this system does not support bind mounts");
#else
  auto targetFd = openBindMountTarget(mountRoot, mountPath);
  auto sourceFd = openPathAsUser(clientPath, R_OK | X_OK);
  XLOGF(
      DBG2,
      "Moving detached bind mount from `{}` to `{}` by fd",
      clientPath,
      mountPath);

  // Clone the source mount into a detached tree, then attach it to the target
  // fd. The target path is not re-resolved by string during the attach.
  auto treeFdNum = static_cast<int>(syscall(
      SYS_open_tree,
      sourceFd.fd(),
      "",
      AT_EMPTY_PATH | OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC));
  checkUnixError(
      treeFdNum, "failed to clone bind mount source `", clientPath, "`");
  folly::File treeFd{treeFdNum, /*ownsFd=*/true};

  const auto rc = static_cast<int>(syscall(
      SYS_move_mount,
      treeFd.fd(),
      "",
      targetFd.fd(),
      "",
      MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_EMPTY_PATH));
  checkUnixError(
      rc,
      "failed to move detached bind mount `",
      clientPath,
      "` over `",
      mountPath,
      "`");
#endif
}

bool PrivHelperServer::useModernMountApi() const {
#ifdef __APPLE__
  return false;
#else
  return !disablePrivHelperHardening();
#endif
}

void PrivHelperServer::unmountStaleMount(
    const std::string& mountPoint,
    [[maybe_unused]] int mountFd) {
#ifdef __linux__
  if (mountFd >= 0) {
    const auto procFdPath = fmt::format("/proc/self/fd/{}", mountFd);
    // A disconnected filesystem can reject stat(), but its pinned mount can
    // still be unmounted. Never fall back to the caller's mutable pathname.
    checkUnixError(
        umount2(procFdPath.c_str(), linuxUnmountFlags({}, false)),
        "failed to unmount stale mount ",
        mountPoint);
  } else
#endif
  {
    unmount(mountPoint.c_str(), {});
  }
  mountPoints_.erase(mountPoint);
  XLOGF(INFO, "Successfully unmounted stale mount {}", mountPoint);
}

void PrivHelperServer::unmount(
    const char* mountPath,
    [[maybe_unused]] UnmountOptions options) {
#ifdef __APPLE__
  auto rc = ::unmount(mountPath, MNT_FORCE);
#else
  // UMOUNT_NOFOLLOW prevents us from following symlinks.
  // This is needed for security, to ensure that we are only unmounting mount
  // points that we originally mounted.  (The processUnmountMsg() call checks
  // to ensure that the path requested matches one that we know about.)
  //
  // MNT_FORCE asks Linux to remove this mount even if it is still "busy"--if
  // there are other processes with open file handles, or in case we failed to
  // unmount some of the bind mounts contained inside it for some reason.
  // This helps ensure that the unmount actually succeeds.
  // This is the same behavior as "umount --force".
  //
  // MNT_DETACH asks Linux to remove the mount from the filesystem immediately.
  // This is the same behavior as "umount --lazy".
  // This is required for the unmount to succeed in some cases, particularly if
  // something has gone wrong and a bind mount still exists inside this mount
  // for some reason.
  //
  // For now we always do forced unmount during shutdown. This helps ensure
  // that edenfs does not get stuck waiting on unmounts to complete.
  // But we also allow non-force unmount via 'edenfsctl rm --no-force' for
  // a more flexible behavior if needed.
  //
  // In the future it might be nice to provide more smarter unmount options.
  const auto umountFlags = linuxUnmountFlags(options, true);
  const auto rc = umount2(mountPath, umountFlags);
#endif
  if (rc != 0) {
    logUnmountError(mountPath, errno);
  }
}

UnixSocket::Message PrivHelperServer::processTakeoverStartupMsg(
    Cursor& cursor) {
  string mountPath;
  std::vector<string> bindMounts;
  PrivHelperConn::parseTakeoverStartupRequest(cursor, mountPath, bindMounts);
  XLOGF(
      DBG3,
      "takeover startup for \"{}\"; {} bind mounts",
      mountPath,
      bindMounts.size());

  // Skip stale bind mount cleanup on takeover: the kernel preserves live
  // redirections (e.g. buck-out) across a graceful restart, so unmounting
  // them here would destroy legitimate user state.
  //
  // Skip the stale mount check too: the daemon is already serving this mount
  // when it sends the takeover startup request, so there is nothing stale to
  // detect.
#ifndef __APPLE__
  auto checkedMount = openAndSanityCheckMountPoint(
      mountPath, SanityCheckOptions::forTakeover());
  auto sanityResult = checkedMount.sanityResult;
  // Ancestors can change after validation, so retain the checked directory.
  registerMountPoint(
      mountPath, RegisteredMount{std::move(checkedMount.targetFd)});
#else
  auto sanityResult =
      sanityCheckMountPoint(mountPath, SanityCheckOptions::forTakeover());
  registerMountPoint(mountPath);
#endif
  auto response = makeResponse();
  response.data.unshare();
  folly::io::Appender appender(&response.data, 0);
  PrivHelperConn::serializeSanityCheckResult(appender, sanityResult);
  return response;
}

UnixSocket::Message PrivHelperServer::processMountMsg(Cursor& cursor) {
  string mountPath;
  bool readOnly;
  string vfsType;
  PrivHelperConn::parseMountRequest(cursor, mountPath, readOnly, vfsType);
  if (!disablePrivHelperHardening() && vfsType != "fuse" &&
      vfsType != "fuse.edenfs") {
    folly::throwSystemErrorExplicit(EINVAL, "unsupported FUSE filesystem type");
  }
  XLOGF(DBG3, "mount \"{}\"", mountPath);

#ifndef __APPLE__
  if (useModernMountApi()) {
    auto checkedMount = openAndSanityCheckMountPoint(
        mountPath, SanityCheckOptions::forFuseMount());
    auto mountResult = fuseMountByFd(
        std::move(checkedMount.targetFd),
        mountPath.c_str(),
        readOnly,
        vfsType.c_str());
    registerMountPoint(mountPath, std::move(mountResult.registeredMount));

    auto response = makeResponse(std::move(mountResult.fuseDev));
    response.data.unshare();
    folly::io::Appender appender(&response.data, 0);
    PrivHelperConn::serializeSanityCheckResult(
        appender, checkedMount.sanityResult);
    return response;
  }
  if (disablePrivHelperHardening()) {
    XLOGF(
        WARNING,
        "Using legacy path-based FUSE mount for `{}` because `{}` is present",
        mountPath,
        kDisablePrivHelperHardeningPath);
  }
#endif

  auto sanityResult =
      sanityCheckMountPoint(mountPath, SanityCheckOptions::forFuseMount());
  auto fuseDev = fuseMount(mountPath.c_str(), readOnly, vfsType.c_str());
  registerMountPoint(mountPath);

  auto response = makeResponse(std::move(fuseDev));
  response.data.unshare();
  folly::io::Appender appender(&response.data, 0);
  PrivHelperConn::serializeSanityCheckResult(appender, sanityResult);
  return response;
}

UnixSocket::Message PrivHelperServer::processMountNfsMsg(Cursor& cursor) {
  string mountPath;
  NFSMountOptions options;
  PrivHelperConn::parseMountNfsRequest(cursor, mountPath, options);
  XLOGF(DBG3, "mount.nfs \"{}\"", mountPath);

#ifndef __APPLE__
  if (useModernMountApi()) {
    auto checkedMount = openAndSanityCheckMountPoint(
        mountPath,
        SanityCheckOptions::forNfsMount(/*isHardMount=*/!options.useSoftMount));
    registerMountPoint(
        mountPath,
        nfsMountByFd(std::move(checkedMount.targetFd), mountPath, options));

    auto response = makeResponse();
    response.data.unshare();
    folly::io::Appender appender(&response.data, 0);
    PrivHelperConn::serializeSanityCheckResult(
        appender, checkedMount.sanityResult);
    return response;
  }
  if (disablePrivHelperHardening()) {
    XLOGF(
        WARNING,
        "Using legacy path-based NFS mount for `{}` because `{}` is present",
        mountPath,
        kDisablePrivHelperHardeningPath);
  }
#endif

  auto sanityResult = sanityCheckMountPoint(
      mountPath,
      SanityCheckOptions::forNfsMount(/*isHardMount=*/!options.useSoftMount));
  nfsMount(mountPath, std::move(options));
  registerMountPoint(mountPath);

  auto response = makeResponse();
  response.data.unshare();
  folly::io::Appender appender(&response.data, 0);
  PrivHelperConn::serializeSanityCheckResult(appender, sanityResult);
  return response;
}

UnixSocket::Message PrivHelperServer::processUnmountMsg(Cursor& cursor) {
  string mountPath;
  UnmountOptions options;
  PrivHelperConn::parseUnmountRequest(cursor, mountPath, options);
  XLOGF(DBG3, "unmount \"{}\"", mountPath);

  const auto it = mountPoints_.find(mountPath);
  if (it == mountPoints_.end()) {
    throwf<std::domain_error>("No FUSE mount found for {}", mountPath);
  }

  unmountRegisteredMount(mountPath, it->second, options);
  mountPoints_.erase(it);
  return makeResponse();
}

UnixSocket::Message PrivHelperServer::processNfsUnmountMsg(Cursor& cursor) {
  string mountPath;
  PrivHelperConn::parseNfsUnmountRequest(cursor, mountPath);
  XLOGF(DBG3, "unmount \"{}\"", mountPath);

  const auto it = mountPoints_.find(mountPath);
  if (it == mountPoints_.end()) {
    throwf<std::domain_error>("No NFS mount found for {}", mountPath);
  }

  unmountRegisteredMount(mountPath, it->second, {});
  mountPoints_.erase(it);
  return makeResponse();
}

UnixSocket::Message PrivHelperServer::processTakeoverShutdownMsg(
    Cursor& cursor) {
  string mountPath;
  PrivHelperConn::parseTakeoverShutdownRequest(cursor, mountPath);
  XLOGF(DBG3, "takeover shutdown \"{}\"", mountPath);

  const auto it = mountPoints_.find(mountPath);
  if (it == mountPoints_.end()) {
    throwf<std::domain_error>("No mount found for {}", mountPath);
  }

  mountPoints_.erase(it);
  return makeResponse();
}

std::string PrivHelperServer::findMatchingMountPrefix(folly::StringPiece path) {
  for (const auto& entry : mountPoints_) {
    const auto& mountPoint = entry.first;
    if (boost::starts_with(path, mountPoint + "/")) {
      return mountPoint;
    }
  }
  throwf<std::domain_error>("No FUSE mount found for {}", path);
}

UnixSocket::Message PrivHelperServer::processBindMountMsg(Cursor& cursor) {
  string clientPath;
  string mountPath;
  PrivHelperConn::parseBindMountRequest(cursor, clientPath, mountPath);
  XLOGF(DBG3, "bind mount \"{}\"", mountPath);

  // findMatchingMountPrefix will throw if mountPath doesn't match
  // any known mount.  We perform this check so that we're not a
  // vector for mounting things in arbitrary places.
  auto mountRoot = findMatchingMountPrefix(mountPath);

#ifndef __APPLE__
  if (disablePrivHelperHardening()) {
    XLOGF(
        WARNING,
        "Using legacy path-based bind mount for `{}` because `{}` is present",
        mountPath,
        kDisablePrivHelperHardeningPath);
    // The rollback marker intentionally restores the old path-based behavior.
    insecureBindMount(clientPath.c_str(), mountPath.c_str());
    return makeResponse();
  }
#endif

  bindMount(clientPath.c_str(), mountPath.c_str(), mountRoot);
  return makeResponse();
}

UnixSocket::Message PrivHelperServer::processBindUnMountMsg(Cursor& cursor) {
  string mountPath;
  PrivHelperConn::parseBindUnMountRequest(cursor, mountPath);
  XLOGF(DBG3, "bind unmount \"{}\"", mountPath);

  // findMatchingMountPrefix will throw if mountPath doesn't match
  // any known mount.  We perform this check so that we're not a
  // vector for arbitrarily unmounting things.
  auto mountRoot = findMatchingMountPrefix(mountPath);

#ifndef __APPLE__
  if (!disablePrivHelperHardening()) {
    bindUnmount(mountPath.c_str(), mountRoot);
    return makeResponse();
  }
#endif

#ifndef __APPLE__
  XLOGF(
      WARNING,
      "Using legacy path-based bind unmount for `{}` because `{}` is present",
      mountPath,
      kDisablePrivHelperHardeningPath);
#endif
  // The rollback marker intentionally restores the old path-based behavior.
  insecureBindUnmount(mountPath.c_str());

  return makeResponse();
}

UnixSocket::Message PrivHelperServer::processSetLogFileMsg(
    folly::io::Cursor& cursor,
    UnixSocket::Message& request) {
  XLOG(DBG3, "set log file");
  PrivHelperConn::parseSetLogFileRequest(cursor);
  if (request.files.size() != 1) {
    throwf<std::runtime_error>(
        "expected to receive 1 file descriptor with setLogFile() request. "
        "received {}",
        request.files.size());
  }

  setLogFile(std::move(request.files[0]));

  return makeResponse();
}

void PrivHelperServer::setLogFile(folly::File logFile) {
  // Replace stdout and stderr with the specified file descriptor
  folly::checkUnixError(dup2(logFile.fd(), STDOUT_FILENO));
  folly::checkUnixError(dup2(logFile.fd(), STDERR_FILENO));
}

UnixSocket::Message PrivHelperServer::processLegacyMacFuseConfigRequest(
    folly::io::Cursor& cursor) {
  XLOG(DBG3, "legacy macOS FUSE configuration request");
  PrivHelperConn::parseLegacyMacFuseConfigRequest(cursor);
  return makeResponse();
}

UnixSocket::Message PrivHelperServer::processGetPid() {
  XLOG(DBG3, "get privhelper pid");

  pid_t pid = getpid();
  auto response = makeResponse();
  response.data.unshare();
  folly::io::Appender cursor{&response.data, 0};
  cursor.writeBE<pid_t>(pid);
  return response;
}

UnixSocket::Message PrivHelperServer::processGetNamespaceInfo(
    folly::io::Cursor& cursor) {
  XLOG(DBG3, "get namespace info");

  pid_t daemonPid = cursor.readBE<pid_t>();

  auto statIno = [](const char* path) -> ino_t {
    struct stat st = {};
    if (::stat(path, &st) != 0) {
      throwf<std::runtime_error>(
          "failed to stat {}: {}", path, folly::errnoStr(errno));
    }
    return st.st_ino;
  };

  auto rootIno = statIno("/proc/1/ns/mnt");
  auto selfMountIno = statIno("/proc/self/ns/mnt");
  auto selfPidIno = statIno("/proc/self/ns/pid");

  auto daemonMountNsPath = fmt::format("/proc/{}/ns/mnt", daemonPid);
  auto daemonMountIno = statIno(daemonMountNsPath.c_str());

  auto daemonPidNsPath = fmt::format("/proc/{}/ns/pid", daemonPid);
  auto daemonPidIno = statIno(daemonPidNsPath.c_str());

  auto response = makeResponse();
  response.data.unshare();
  folly::io::Appender appender{&response.data, 0};
  appender.writeBE<pid_t>(getpid());
  appender.writeBE<uint64_t>(rootIno);
  appender.writeBE<uint64_t>(selfMountIno);
  appender.writeBE<uint64_t>(selfPidIno);
  appender.writeBE<uint64_t>(daemonMountIno);
  appender.writeBE<uint64_t>(daemonPidIno);
  return response;
}

AbsolutePath PrivHelperServer::getFamBinaryPath() const {
  return canonicalPath(kFamBinaryPath);
}

UnixSocket::Message PrivHelperServer::processStartFam(
    folly::io::Cursor& cursor,
    UnixSocket::Message& request) {
  std::vector<std::string> paths;
  string tmpOutputPath;
  string specifiedOutputPath;
  bool shouldUpload;

  PrivHelperConn::parseStartFamRequest(
      cursor, paths, tmpOutputPath, specifiedOutputPath, shouldUpload);
  if (request.files.size() != 1) {
    throwf<std::runtime_error>(
        "expected 1 output file descriptor with startFam() request; received {}",
        request.files.size());
  }

  // sanity check to make sure we have at least one path
  if (paths.empty()) {
    XLOG(
        ERR,
        "Empty list of paths: At least one path should be provided to start FAM");
    throwf<std::runtime_error>("expected at least one path to start FAM");
  }

  for (const auto& path : paths) {
    XLOGF(DBG3, "FAM monitoring path with prefix \"{}\"", path);
  }
  XLOGF(DBG3, "FAM logging events to \"{}\"", tmpOutputPath);
  XLOGF(DBG3, "FAM output file will be moved to \"{}\"", specifiedOutputPath);

  auto opts = SpawnedProcess::Options();
  opts.dup2(
      FileDescriptor(
          request.files[0].release(), FileDescriptor::FDType::Generic),
      STDOUT_FILENO);
  opts.executablePath(getFamBinaryPath());
  std::vector<std::string> argv = {
      "SCMFileAccessMonitor",
      "--path-prefix",
      paths[0],
      "--events",
      "NOTIFY_OPEN",
      "NOTIFY_CLOSE",
  };

  famProcess_ = std::make_unique<FileAccessMonitorProcess>(
      SpawnedProcess(argv, std::move(opts)),
      std::move(tmpOutputPath),
      std::move(specifiedOutputPath),
      shouldUpload);

  pid_t pid = famProcess_->proc.pid();

  auto response = makeResponse();
  response.data.unshare();
  folly::io::Appender cursorResp{&response.data, 0};
  cursorResp.writeBE<pid_t>(pid);
  return response;
}

UnixSocket::Message PrivHelperServer::processStopFam() {
  if (!famProcess_) {
    XLOG(ERR, "FAM process is not running or is not managed by edenfs");
    throwf<std::runtime_error>("FAM process is not running");
  }

  string tmpOutputPath = std::move(famProcess_->tmpOutputPath);
  string specifiedOutputPath = std::move(famProcess_->specifiedOutputPath);
  bool shouldUpload = famProcess_->shouldUpload;

  pid_t pid = famProcess_->proc.pid();
  // SIGTERM should be enough to terminate the process immediately, but we'll
  // also try to kill it if it doesn't exit after a short time, e.g. 500ms.
  auto status =
      famProcess_->proc.terminateOrKill(std::chrono::milliseconds(500));
  if (famProcess_->proc.terminated()) {
    XLOGF(DBG3, "FAM process pid: {} terminated", pid);
  } else {
    XLOGF(ERR, "Failed to terminate FAM pid: {}", pid);
    XLOGF(ERR, "FAM process status: {}", status.str());

    // We failed to terminate the process, there is no need to keep it lingering
    famProcess_.reset();
    throwf<std::runtime_error>("Failed to terminate FAM pid: {}", pid);
  }

  famProcess_.reset();

  auto response = makeResponse();
  response.data.unshare();
  folly::io::Appender cursor{&response.data, 0};
  PrivHelperConn::serializeStopFamResponse(
      cursor, tmpOutputPath, specifiedOutputPath, shouldUpload);
  return response;
}

UnixSocket::Message PrivHelperServer::processSetMemoryPriorityForProcess(
    folly::io::Cursor& cursor) {
  XLOG(DBG3, "set memory priority for process");
  pid_t pid;
  int priority;
  PrivHelperConn::parseSetMemoryPriorityForProcessRequest(
      cursor, pid, priority);

  setMemoryPriorityForProcess(pid, priority);

  return makeResponse();
}

UnixSocket::Message PrivHelperServer::processSetFuseReadAhead(
    folly::io::Cursor& cursor) {
  std::string mountPath;
  uint32_t readAheadKb;
  PrivHelperConn::parseSetFuseReadAheadRequest(cursor, mountPath, readAheadKb);
#ifndef __APPLE__
  if (disablePrivHelperHardening()) {
    XLOGF(
        WARN,
        "Using legacy read-ahead configuration for `{}` because privhelper hardening is disabled",
        mountPath);
    configureFuseReadAhead(mountPath.c_str(), std::nullopt, readAheadKb);
    return makeResponse();
  }

  const auto it = mountPoints_.find(mountPath);
  if (it == mountPoints_.end()) {
    throwf<std::domain_error>("No FUSE mount found for {}", mountPath);
  }
  configureFuseReadAhead(
      mountPath.c_str(), it->second.rootFd.fd(), readAheadKb);
#else
  (void)mountPath;
  (void)readAheadKb;
  XLOG(WARN, "FUSE BDI read-ahead configuration is not supported on macOS");
#endif
  return makeResponse();
}

UnixSocket::Message PrivHelperServer::processSetNfsFstypeOverride(
    folly::io::Cursor& cursor) {
  bool enabled = true;
  PrivHelperConn::parseSetNfsFstypeOverrideRequest(cursor, enabled);
  nfsFstypeOverride_.store(enabled, std::memory_order_relaxed);
#ifdef __APPLE__
  XLOGF(
      INFO,
      "NFS filesystem type override {}",
      enabled ? "enabled" : "disabled");
#else
  XLOGF(
      DBG2,
      "NFS filesystem type override {} (no effect on this platform)",
      enabled ? "enabled" : "disabled");
#endif
  return makeResponse();
}

void PrivHelperServer::setMemoryPriorityForProcess(pid_t pid, int priority) {
  auto processPriority = ProcessPriority{priority, uid_};

  if (processPriority.setPrioritiesForProcess(pid)) {
    throwf<std::runtime_error>(
        "failed to set memory priority for process {}", pid);
  }
}

namespace {
/// Get the file system ID, or an errno value on error
folly::Expected<unsigned long, int> getFSID(const char* path) {
  struct statvfs data;
  if (statvfs(path, &data) != 0) {
    return folly::makeUnexpected(errno);
  }
  return folly::makeExpected<int>(data.f_fsid);
}

#ifndef __APPLE__
folly::Expected<unsigned long, int> getFSID(int fd) {
  struct statvfs data{};
  if (fstatvfs(fd, &data) != 0) {
    return folly::makeUnexpected(errno);
  }
  return folly::makeExpected<int>(data.f_fsid);
}
#endif

void waitForUnmount(
    const char* mountPath,
    folly::Expected<unsigned long, int> origFSID) {
  // Empirically, the unmount may not be complete when umount2() returns.
  // To work around this, we repeatedly invoke statvfs() on the bind mount
  // until it fails or returns a different filesystem ID.
  //
  // Give up after 2 seconds even if the unmount does not appear complete.
  constexpr auto timeout = std::chrono::seconds(2);
  const auto endTime = std::chrono::steady_clock::now() + timeout;
  while (true) {
    const auto fsid = getFSID(mountPath);
    if (!fsid.hasValue()) {
      // Assume the file system is unmounted if the statvfs() call failed.
      break;
    }
    if (origFSID.hasValue() && origFSID.value() != fsid.value()) {
      // The unmount has succeeded if the filesystem ID is different now.
      break;
    }

    if (std::chrono::steady_clock::now() > endTime) {
      XLOGF(
          WARNING,
          "error unmounting {}: mount did not go away after successful unmount call",
          mountPath);
      break;
    }
    sched_yield();
  }
}

} // namespace

#ifndef __APPLE__
int PrivHelperServer::umountBindMountByFd(const char* procFdPath) {
  return umount2(procFdPath, MNT_DETACH);
}

PrivHelperServer::BindUnmountResult PrivHelperServer::bindUnmountByFd(
    const folly::File& targetFd,
    const char* mountPath) {
  const auto procFdPath = fmt::format("/proc/self/fd/{}", targetFd.fd());
  // umount2() still takes a path, but this procfs path refers to the already
  // open target fd. Do not pass UMOUNT_NOFOLLOW here: the procfd magic link is
  // the mechanism that converts the fd back to a kernel path for umount2().
  XLOGF(DBG2, "Unmounting bind mount `{}` through `{}`", mountPath, procFdPath);
  const auto rc = umountBindMountByFd(procFdPath.c_str());
  if (rc != 0 && errno == EINVAL) {
    XLOGF(DBG2, "Bind mount `{}` is already unmounted", mountPath);
    return BindUnmountResult::AlreadyUnmounted;
  }
  checkUnixError(
      rc,
      "failed to unmount bind mount `",
      mountPath,
      "` through `",
      procFdPath,
      "`");
  return BindUnmountResult::Unmounted;
}
#endif

void PrivHelperServer::insecureBindUnmount(const char* mountPath) {
  // Check the current filesystem information for this path,
  // so we can confirm that it has been unmounted afterwards.
  const auto origFSID = getFSID(mountPath);

  unmount(mountPath, {});
  waitForUnmount(mountPath, origFSID);
}

void PrivHelperServer::bindUnmount(
    [[maybe_unused]] const char* mountPath,
    folly::StringPiece mountRoot) {
#ifndef __APPLE__
  auto targetFd = openBindMountTarget(mountRoot, mountPath);
  const auto origFSID = getFSID(targetFd.fd());
  const auto unmountResult = bindUnmountByFd(targetFd, mountPath);
  if (unmountResult == BindUnmountResult::Unmounted) {
    // The privileged unmount already used targetFd. This best-effort
    // completion check still stats mountPath because targetFd keeps reporting
    // the detached filesystem until the fd is closed.
    waitForUnmount(mountPath, origFSID);
    return;
  }
  if (unmountResult == BindUnmountResult::AlreadyUnmounted) {
    return;
  }
#else
  (void)mountRoot;
  folly::throwSystemErrorExplicit(
      ENOTSUP, "bind unmounts are not supported on macOS");
#endif
}

UnixSocket::Message PrivHelperServer::processSetRestartArgsMsg(Cursor& cursor) {
  EdenFsRestartArgs args;
  PrivHelperConn::parseSetRestartArgsRequest(cursor, args);
  XLOGF(
      INFO,
      "received edenfs restart configuration: enabled={}, sentinel={}, restartCount={}, maxRestarts={}, windowSeconds={}",
      args.enabled,
      args.sentinelPath,
      args.restartCount,
      args.maxRestarts,
      args.windowSeconds);
  sentinel_->setConfig(std::move(args));
  return makeResponse();
}

UnixSocket::Message PrivHelperServer::processNotifyCleanShutdownMsg(
    Cursor& cursor) {
  std::string reason;
  PrivHelperConn::parseNotifyCleanShutdownRequest(cursor, reason);
  XLOGF(INFO, "edenfs reported a deliberate shutdown: {}", reason);
  sentinel_->noteCleanShutdown();
  // One-way request: processAndSendResponse() discards this.
  return makeResponse();
}

std::optional<AbsolutePath> PrivHelperServer::findSiblingEdenFs(
    AbsolutePathPiece dir) {
  auto sibling = dir + "edenfs"_relpath;

  struct stat st{};
  if (lstat(sibling.c_str(), &st) != 0 || !S_ISREG(st.st_mode)) {
    return std::nullopt;
  }
  // A searchable directory passes X_OK, so the regular-file check above cannot
  // be dropped in favour of this one.
  if (::access(sibling.c_str(), X_OK) != 0) {
    return std::nullopt;
  }
  return sibling;
}

AbsolutePath PrivHelperServer::resolveEdenFsBinary(
    const RestartSentinel::RelaunchCommand& command) const {
  const auto executable = executablePath();
  const auto dir = executable.dirname();
  if (const auto sibling = findSiblingEdenFs(dir)) {
    return *sibling;
  }

  // Dev and test installs have no sibling to find, so fall back to the command
  // edenfsctl recorded.
  if (!command.argv.empty()) {
    XLOGF(
        WARN,
        "no edenfs binary next to the privhelper in {}; falling back to {} from the recorded start command",
        dir,
        command.argv[0]);
    return canonicalPath(command.argv[0]);
  }

  throwf<std::runtime_error>(
      "no edenfs binary in {}, and no recorded start command to fall back to",
      dir);
}

void PrivHelperServer::validateRestartOwner() const {
  const auto realUid = getuid();
  const auto realGid = getgid();
  if (realUid == 0) {
    throw std::runtime_error("real uid is root");
  }
  if (realUid != uid_ || realGid != gid_) {
    throwf<std::runtime_error>(
        "real uid/gid {}/{} do not match the privhelper owner {}/{}",
        realUid,
        realGid,
        uid_,
        gid_);
  }
}

std::optional<PrivHelperServer::RestartPlan>
PrivHelperServer::prepareRestart() {
  if (!sentinel_.has_value() || !sentinel_->enabled()) {
    return std::nullopt;
  }
  const auto state = sentinel_->disarmState();
  // Defensive: the guard above already rules out the one case that is nullopt.
  if (!state.has_value()) {
    return std::nullopt;
  }
  switch (*state) {
    case RestartSentinel::DisarmState::Armed:
      break;
    case RestartSentinel::DisarmState::ShutdownAnnounced:
      XLOG(INFO, "edenfs shut down on purpose; not restarting");
      return std::nullopt;
    case RestartSentinel::DisarmState::Unknown:
      XLOG(
          WARN,
          "cannot tell whether edenfs meant to shut down; not restarting");
      return std::nullopt;
  }

  // Obtained before producing a plan so launchRestart() is unreachable without
  // a command.
  auto command = sentinel_->relaunchCommand();
  if (!command.has_value()) {
    return std::nullopt;
  }

  AbsolutePath binary;
  try {
    binary = resolveEdenFsBinary(*command);
  } catch (const std::exception& ex) {
    XLOGF(ERR, "refusing to restart edenfs: {}", folly::exceptionStr(ex));
    return std::nullopt;
  }

  if (!sentinel_->admitRestartAttempt(now_())) {
    return std::nullopt;
  }
  return RestartPlan{
      std::move(binary),
      std::move(*command),
      sentinel_->restartCount(),
      sentinel_->firstRestartEpochSec()};
}

bool PrivHelperServer::launchRestart(const RestartPlan& plan) const {
  // The recorded environment can already carry these keys: edenfsctl preserves
  // every EDEN-prefixed variable it was started with.
  auto env = plan.command.env;
  setEnv(
      env, kEdenFsRestartCountEnv, folly::to<std::string>(plan.restartCount));
  setEnv(
      env,
      kEdenFsFirstRestartAtEnv,
      folly::to<std::string>(plan.firstRestartEpochSec));

  try {
    validateRestartOwner();
  } catch (const std::exception& ex) {
    XLOGF(
        ERR,
        "refusing to restart edenfs, resetting IDs would not select its owner: {}",
        folly::exceptionStr(ex));
    return false;
  }

  if (!spawnEdenFs_(plan.binary, plan.command.argv, env)) {
    return false;
  }
  return true;
}

void PrivHelperServer::run() {
  // Log and ignore signals that would otherwise terminate the process.
  // We should only exit when the daemon's connection closes, so that we
  // can umount all outstanding mount points before we exit. The privhelper
  // binary detaches into its own session at startup, which keeps terminal
  // signals (e.g. Ctrl-C) and process-group-wide kills away from us, but
  // anything signaling this process directly must not terminate it either.
  class LogAndIgnoreSignalHandler : public folly::AsyncSignalHandler {
   public:
    using AsyncSignalHandler::AsyncSignalHandler;

    void signalReceived(int sig) noexcept override {
      XLOGF(
          WARN,
          "privhelper received signal {} ({}); ignoring; the privhelper "
          "exits only when the EdenFS daemon connection closes",
          sig,
          strsignal(sig));
    }
  };
  LogAndIgnoreSignalHandler signalHandler{eventBase_.get()};
  for (int sig : {SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2}) {
    signalHandler.registerSignalHandler(sig);
  }

  conn_->setReceiveCallback(this);
  eventBase_->loop();

  // We terminate the event loop when the socket has been closed.
  // This normally means the parent process exited, so we can clean up and exit
  // too.
  XLOG(DBG5, "privhelper process exiting");

  if (peerExited_) {
    if (const auto plan = prepareRestart()) {
      if (launchRestart(*plan)) {
        return;
      }
    }
  }

  // Unmount all active mount points
  cleanupMountPoints();
}

void PrivHelperServer::messageReceived(UnixSocket::Message&& message) noexcept {
  try {
    processAndSendResponse(std::move(message));
  } catch (const std::exception& ex) {
    XLOGF(
        ERR,
        "error processing privhelper request: {}",
        folly::exceptionStr(ex));
  }
}

void PrivHelperServer::processAndSendResponse(UnixSocket::Message&& message) {
  Cursor cursor{&message.data};
  PrivHelperConn::PrivHelperPacket packet = PrivHelperConn::parsePacket(cursor);
  const PrivHelperConn::MsgType requestType{packet.metadata.msg_type};

  UnixSocket::Message response;
  try {
    response = processMessage(packet, cursor, message);
  } catch (const std::exception& ex) {
    XLOGF(
        ERR,
        "error processing privhelper request: {}",
        folly::exceptionStr(ex));
    packet.metadata.msg_type = PrivHelperConn::RESP_ERROR;
    response = makeResponse();
    Appender appender(&response.data, 1024);
    PrivHelperConn::serializeErrorResponse(appender, ex);
  }

  // The client sends a one-way request without registering a transaction ID,
  // so any reply -- including an error reply -- would be unmatched and raise an
  // EDEN_BUG on the client.
  if (PrivHelperConn::isOneWayRequest(requestType)) {
    return;
  }

  // Put the version, transaction ID, and message type in the response.
  // The makeResponse() APIs ensure there is enough headroom for the packet.
  // Version info is important, since we may fall back to an older version
  // in order to process a request.
  size_t packetSize = sizeof(packet);
  if (response.data.headroom() >= packetSize) {
    response.data.prepend(packetSize);
  } else {
    // This is unexpected, but go ahead and allocate more room just in case this
    // ever does occur.
    XLOG(
        WARN,
        "insufficient headroom for privhelper response packet; making more space");
    auto body = std::make_unique<IOBuf>(std::move(response.data));
    response.data = IOBuf(IOBuf::CREATE, packetSize);
    response.data.append(packetSize);
    response.data.prependChain(std::move(body));
  }

  RWPrivateCursor respCursor(&response.data);

  PrivHelperConn::serializeResponsePacket(packet, respCursor);

  conn_->send(std::move(response));
}

UnixSocket::Message PrivHelperServer::makeResponse() {
  // 1024 bytes is enough for most responses.  If the response is longer
  // we will allocate more room later.
  constexpr size_t kDefaultBufferSize = 1024;

  UnixSocket::Message msg;
  msg.data = IOBuf(IOBuf::CREATE, kDefaultBufferSize);

  // Leave enough headroom for the response packet that includes the transaction
  // ID and message type (and any additional metadata).
  msg.data.advance(sizeof(PrivHelperConn::PrivHelperPacket));
  return msg;
}

UnixSocket::Message PrivHelperServer::makeResponse(folly::File file) {
  auto response = makeResponse();
  response.files.push_back(std::move(file));
  return response;
}

UnixSocket::Message PrivHelperServer::processMessage(
    PrivHelperConn::PrivHelperPacket& packet,
    Cursor& cursor,
    UnixSocket::Message& request) {
  // TODO(T185426586): In the future, we can use packet.header.version to
  // decide how to handle each request. Each request handler can implement
  // different handler logic for each known version (if needed).
  PrivHelperConn::MsgType msgType{packet.metadata.msg_type};
  XLOGF(
      DBG7,
      "Processing message of type {} for protocol version v{}",
      msgType,
      packet.header.version);
  switch (msgType) {
    case PrivHelperConn::REQ_MOUNT_FUSE:
      return processMountMsg(cursor);
    case PrivHelperConn::REQ_MOUNT_NFS:
      return processMountNfsMsg(cursor);
    case PrivHelperConn::REQ_MOUNT_BIND:
      return processBindMountMsg(cursor);
    case PrivHelperConn::REQ_UNMOUNT_FUSE:
      return processUnmountMsg(cursor);
    case PrivHelperConn::REQ_UNMOUNT_NFS:
      return processNfsUnmountMsg(cursor);
    case PrivHelperConn::REQ_TAKEOVER_SHUTDOWN:
      return processTakeoverShutdownMsg(cursor);
    case PrivHelperConn::REQ_TAKEOVER_STARTUP:
      return processTakeoverStartupMsg(cursor);
    case PrivHelperConn::REQ_SET_LOG_FILE:
      return processSetLogFileMsg(cursor, request);
    case PrivHelperConn::REQ_UNMOUNT_BIND:
#ifdef __APPLE__
      if (!disablePrivHelperHardening()) {
        folly::throwSystemErrorExplicit(
            ENOTSUP, "bind unmounts are not supported on macOS");
      }
#endif
      return processBindUnMountMsg(cursor);
    case PrivHelperConn::REQ_SET_DAEMON_TIMEOUT:
    case PrivHelperConn::REQ_SET_USE_EDENFS:
      return processLegacyMacFuseConfigRequest(cursor);
    case PrivHelperConn::REQ_GET_PID:
      return processGetPid();
    case PrivHelperConn::REQ_GET_NAMESPACE_INFO:
      return processGetNamespaceInfo(cursor);
#ifdef __APPLE__
    case PrivHelperConn::REQ_START_FAM:
      return processStartFam(cursor, request);
    case PrivHelperConn::REQ_STOP_FAM:
      return processStopFam();
#else
    case PrivHelperConn::REQ_START_FAM:
    case PrivHelperConn::REQ_STOP_FAM:
      break;
#endif
    case PrivHelperConn::REQ_SET_MEMORY_PRIORITY_FOR_PROCESS:
      return processSetMemoryPriorityForProcess(cursor);
    case PrivHelperConn::REQ_SET_FUSE_READ_AHEAD:
      return processSetFuseReadAhead(cursor);
    case PrivHelperConn::REQ_SET_NFS_FSTYPE_OVERRIDE:
      return processSetNfsFstypeOverride(cursor);
    case PrivHelperConn::REQ_SET_RESTART_ARGS:
      return processSetRestartArgsMsg(cursor);
    case PrivHelperConn::REQ_NOTIFY_CLEAN_SHUTDOWN:
      return processNotifyCleanShutdownMsg(cursor);
    case PrivHelperConn::MSG_TYPE_NONE:
    case PrivHelperConn::RESP_ERROR:
      break;
  }

  throwf<std::runtime_error>(
      "unexpected privhelper message type: {}", folly::to_underlying(msgType));
}

void PrivHelperServer::eofReceived() noexcept {
  peerExited_ = true;
  eventBase_->terminateLoopSoon();
}

void PrivHelperServer::socketClosed() noexcept {
  eventBase_->terminateLoopSoon();
}

void PrivHelperServer::receiveError(
    const folly::exception_wrapper& ew) noexcept {
  XLOGF(ERR, "receive error in privhelper server: {}", ew.what());
  eventBase_->terminateLoopSoon();
}

void PrivHelperServer::cleanupMountPoints() {
  for (const auto& entry : mountPoints_) {
    const auto& mountPoint = entry.first;
    try {
      unmountRegisteredMount(mountPoint, entry.second, {});
    } catch (const std::exception& ex) {
      XLOGF(
          ERR,
          "error unmounting \"{}\": {}",
          mountPoint,
          folly::exceptionStr(ex));
    }
  }

  mountPoints_.clear();
}

} // namespace facebook::eden

#endif
