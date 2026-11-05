/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <eden/common/utils/SpawnedProcess.h>
#include <folly/File.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <atomic>
#include <functional>
#include <limits>
#include <map>
#include <optional>
#include <string>
#include <unordered_map>
#include <utility>
#include <vector>
#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/UnixSocket.h"
#include "eden/fs/privhelper/PrivHelperConn.h"
#include "eden/fs/privhelper/RestartSentinel.h"

namespace folly {
class EventBase;
class File;
class SocketAddress;
namespace io {
class Cursor;
}
} // namespace folly

namespace facebook::eden {

struct MountTableEntry;

struct FileAccessMonitorProcess {
  SpawnedProcess proc;

  std::string tmpOutputPath;
  std::string specifiedOutputPath;
  bool shouldUpload;

  FileAccessMonitorProcess(
      SpawnedProcess p,
      std::string tmpOutputPath,
      std::string specifiedOutputPath,
      bool shouldUpload)
      : proc(std::move(p)),
        tmpOutputPath(std::move(tmpOutputPath)),
        specifiedOutputPath(std::move(specifiedOutputPath)),
        shouldUpload(shouldUpload) {}
};

/**
 * Move the calling process into its own session (and therefore its own
 * process group), away from the process group it was spawned into.
 *
 * The privhelper must only exit when the EdenFS daemon's connection
 * closes: it unmounts the daemon's mounts on the way out. The daemon
 * detaches itself into a new process group during startup, but the
 * privhelper is spawned earlier and would otherwise remain in the group
 * of whatever launched `eden start`/`eden restart`. Tools that clean up
 * by killing the process group they spawned (e.g. agent command runners)
 * would then SIGKILL the privhelper — which cannot be caught — while the
 * daemon survives, leaving EdenFS unable to mount or unmount.
 *
 * Failure (e.g. the process is already a process-group leader, so there
 * is no foreign group to escape from) is logged and ignored.
 */
void detachFromParentProcessGroup();

/**
 * Whether an NFS mount with these options gets the macOS f_fstypename
 * override ("edenfs:") applied after mounting, while the override is on.
 *
 * macOS replaces f_mntfromname for Unix-domain socket transports, so only
 * those mounts need it. With the override off they are recognized by the
 * name of the nfsd socket instead, see is_edenfs_nfs_mount().
 */
bool needsFstypeOverride(const NFSMountOptions& options);

/*
 * PrivHelperServer runs the main loop for the privhelper server process.
 *
 * This processes requests received on the specified socket.
 * The server exits when the remote side of the socket is closed.
 *
 * See PrivHelperConn.h for the various message types.
 *
 * The uid and gid parameters specify the user and group ID of the unprivileged
 * process that will be making requests to us.
 */
class PrivHelperServer : private UnixSocket::ReceiveCallback {
 public:
  PrivHelperServer();
  virtual ~PrivHelperServer();

  /**
   * Initialize the PrivHelperServer.  This should be called prior to run().
   *
   * This calls folly::init().
   */
  virtual void init(folly::File socket, uid_t uid, gid_t gid);

  /**
   * Initialize the PrivHelperServer without calling folly::init().
   *
   * This can be used if folly::init() has already been called.
   */
  void initPartial(folly::File socket, uid_t uid, gid_t gid);

  /**
   * Run the PrivHelperServer main loop.
   */
  void run();

  bool nfsFstypeOverrideEnabled() const {
    return nfsFstypeOverride_.load(std::memory_order_relaxed);
  }

 private:
  // UnixSocket::ReceiveCallback methods
  void messageReceived(UnixSocket::Message&& message) noexcept override;
  void eofReceived() noexcept override;
  void socketClosed() noexcept override;
  void receiveError(const folly::exception_wrapper& ew) noexcept override;

  void processAndSendResponse(UnixSocket::Message&& message);
  UnixSocket::Message processMessage(
      PrivHelperConn::PrivHelperPacket& packet,
      folly::io::Cursor& cursor,
      UnixSocket::Message& request);
  UnixSocket::Message makeResponse();
  UnixSocket::Message makeResponse(folly::File file);

  UnixSocket::Message processMountMsg(folly::io::Cursor& cursor);
  UnixSocket::Message processMountNfsMsg(folly::io::Cursor& cursor);
  UnixSocket::Message processUnmountMsg(folly::io::Cursor& cursor);
  UnixSocket::Message processNfsUnmountMsg(folly::io::Cursor& cursor);
  UnixSocket::Message processBindMountMsg(folly::io::Cursor& cursor);
  UnixSocket::Message processBindUnMountMsg(folly::io::Cursor& cursor);
  UnixSocket::Message processTakeoverShutdownMsg(folly::io::Cursor& cursor);
  UnixSocket::Message processTakeoverStartupMsg(folly::io::Cursor& cursor);
  UnixSocket::Message processSetLogFileMsg(
      folly::io::Cursor& cursor,
      UnixSocket::Message& request);
  UnixSocket::Message processLegacyMacFuseConfigRequest(
      folly::io::Cursor& cursor);
  std::string findMatchingMountPrefix(folly::StringPiece path);
  struct RegisteredMount {
#ifndef __APPLE__
    folly::File rootFd;
#endif
  };
#ifndef __APPLE__
  struct FuseMountResult {
    folly::File fuseDev;
    RegisteredMount registeredMount;
  };
  struct CheckedMountPoint {
    folly::File targetFd;
    SanityCheckResult sanityResult;
  };
#endif
  RegisteredMount openRegisteredMount(const std::string& mountPath);
  void registerMountPoint(
      const std::string& mountPath,
      RegisteredMount registeredMount);
  void unmountRegisteredMount(
      const std::string& mountPath,
      const RegisteredMount& registeredMount,
      UnmountOptions options);

  UnixSocket::Message processGetPid();
  UnixSocket::Message processGetNamespaceInfo(folly::io::Cursor& cursor);
  UnixSocket::Message processSetMemoryPriorityForProcess(
      folly::io::Cursor& cursor);

  void unmountStaleMount(const std::string& mountPoint, int mountFd);

 protected:
  // Clean up descendants of the already-authorized checkout directory.
  SanityCheckResult cleanupStaleBindMounts(
      const std::string& checkoutPath,
      int checkoutFd);
  SanityCheckResult insecureCleanupStaleBindMounts(
      const std::string& checkoutPath);
#ifdef __linux__
  virtual std::vector<MountTableEntry> mountsForCleanup() const;
  virtual int unmountCleanupMount(int mountFd);
#endif

  virtual int statMountPoint(const char* path, struct stat* st) const;

  // Uses stat to determine if there's a stale mount point at the given path. If
  // there is, force unmounts it. Returns true if a stale mount was found and
  // unmounted.
  bool detectAndUnmountStaleMount(
      const std::string& mountPoint,
      bool isNFS,
      bool isHardMount);

  /**
   * How detectAndUnmountStaleMount probes the mount point.
   */
  struct StaleMountCheck {
    bool isNFS;
    bool isHardMount;
  };

  /**
   * What sanityCheckMountPoint and openAndSanityCheckMountPoint do around the
   * ownership and access checks. Only the factories can build one, so every
   * call site names the kind of mount it is checking.
   */
  class SanityCheckOptions {
   public:
    /**
     * The mount the daemon inherited across a graceful restart. No stale
     * mount probe: the daemon is already serving the mount when it sends the
     * takeover startup request, so there is nothing stale to detect.
     * Redirection bind mounts are left alone: the kernel preserves live ones
     * (e.g. buck-out) across the restart, so detaching them would unmount
     * user state.
     */
    static SanityCheckOptions forTakeover() {
      return SanityCheckOptions(
          /*staleMountCheck=*/std::nullopt,
          /*performBindMountCleanup=*/false);
    }

    /** A fresh FUSE mount. */
    static SanityCheckOptions forFuseMount() {
      return SanityCheckOptions(
          StaleMountCheck{/*isNFS=*/false, /*isHardMount=*/false},
          /*performBindMountCleanup=*/true);
    }

    /** A fresh NFS mount. Hard mounts skip the probes that can hang. */
    static SanityCheckOptions forNfsMount(bool isHardMount) {
      return SanityCheckOptions(
          StaleMountCheck{/*isNFS=*/true, isHardMount},
          /*performBindMountCleanup=*/true);
    }

    /** The stale mount probe to run first, or nullopt to run none. */
    const std::optional<StaleMountCheck>& staleMountCheck() const {
      return staleMountCheck_;
    }

    /**
     * Whether stale redirection bind mounts under the checkout are detached
     * after the checkout path passes the ownership and access checks.
     */
    bool performBindMountCleanup() const {
      return performBindMountCleanup_;
    }

   private:
    SanityCheckOptions(
        std::optional<StaleMountCheck> staleMountCheck,
        bool performBindMountCleanup)
        : staleMountCheck_(staleMountCheck),
          performBindMountCleanup_(performBindMountCleanup) {}

    std::optional<StaleMountCheck> staleMountCheck_;
    bool performBindMountCleanup_;
  };

  /**
   * Verify that the user has the right credentials to mount/unmount this path.
   *
   * This will check that the user has RW access to every path component
   * leading to the mount point. A std::domain_error exception will be raised
   * if the user doesn't have access to the mount point.
   */
  SanityCheckResult sanityCheckMountPoint(
      const std::string& mountPoint,
      const SanityCheckOptions& options);
#ifndef __APPLE__
  CheckedMountPoint openAndSanityCheckMountPoint(
      const std::string& mountPoint,
      const SanityCheckOptions& options);
#endif

 private:
  // These methods are virtual so we can override them during unit tests
  virtual folly::File
  fuseMount(const char* mountPath, bool readOnly, const char* vfsType);
  virtual void nfsMount(std::string mountPath, NFSMountOptions options);
  virtual void unmount(const char* mountPath, UnmountOptions options);
  // Both clientPath and mountPath must be existing directories.
  virtual void insecureBindMount(const char* clientPath, const char* mountPath);
  virtual bool useModernMountApi() const;
#ifndef __APPLE__
  FuseMountResult fuseMountByFd(
      folly::File targetFd,
      const char* mountPath,
      bool readOnly,
      const char* vfsType);
  RegisteredMount nfsMountByFd(
      folly::File targetFd,
      const std::string& mountPath,
      const NFSMountOptions& options);
#endif

 protected:
  void registerMountPoint(const std::string& mountPath);
  virtual void bindMount(
      const char* clientPath,
      const char* mountPath,
      folly::StringPiece mountRoot);
#ifdef __linux__
  // Supplementary groups must already belong to the served user: setuid
  // launches inherit them, and PrivHelperMain initializes them for root
  // launches.
  folly::File openPathAsUser(const std::string& path, int accessMode) const;
#endif

  UnixSocket::Message processStartFam(
      folly::io::Cursor& cursor,
      UnixSocket::Message& request);
  UnixSocket::Message processStopFam();
  UnixSocket::Message processSetFuseReadAhead(folly::io::Cursor& cursor);
  UnixSocket::Message processSetNfsFstypeOverride(folly::io::Cursor& cursor);
  virtual AbsolutePath getFamBinaryPath() const;

  // Virtual so tests can replace a pathname after its descriptor is checked.
  virtual void sanityCheckOpenedMountPoint(
      const std::string& mountPoint,
      int mountPointFd);

  folly::File openBindMountTarget(
      folly::StringPiece mountRoot,
      folly::StringPiece mountPath);

  // The daemon's credentials, as supplied to init().
  uid_t uid_{std::numeric_limits<uid_t>::max()};
  gid_t gid_{std::numeric_limits<gid_t>::max()};

  // Virtual so that a test can observe whether a code path skipped it.
  virtual void cleanupMountPoints();

  /** Everything prepareRestart() resolved as root. */
  struct RestartPlan {
    AbsolutePath binary;
    RestartSentinel::RelaunchCommand command;
    // The restart budget already charged for this attempt.
    uint32_t restartCount{0};
    uint64_t firstRestartEpochSec{0};
  };

  /**
   * Spawns a new edenfs. Overridable so that tests can exercise the restart
   * decision without launching anything. Returns false unless the daemon's
   * startup process exits successfully.
   */
  using SpawnEdenFsFn = std::function<bool(
      const AbsolutePath& binary,
      const std::vector<std::string>& argv,
      const std::vector<std::pair<std::string, std::string>>& env)>;
  SpawnEdenFsFn spawnEdenFs_;

  // Seconds since the epoch. Overridable so that tests can age the restart
  // window without sleeping.
  std::function<uint64_t()> now_;

  // Whether edenfs should be relaunched, and what to relaunch. Constructed by
  // initPartial(), which is where the daemon's uid becomes known.
  std::optional<RestartSentinel> sentinel_;

  UnixSocket::Message processSetRestartArgsMsg(folly::io::Cursor& cursor);
  UnixSocket::Message processNotifyCleanShutdownMsg(folly::io::Cursor& cursor);

  /**
   * Decide whether this exit looks like a crash worth answering with a
   * relaunch, and do everything about it that needs root: taking the relaunch
   * command, resolving the binary and charging the circuit breaker.
   *
   * Returns the plan to launch, or nullopt to leave edenfs down.
   */
  std::optional<RestartPlan> prepareRestart();

  /**
   * Validate the child credentials and relaunch edenfs from a plan. Returns
   * whether the replacement daemon finished starting.
   */
  bool launchRestart(const RestartPlan& plan) const;

  /**
   * The edenfs binary installed in `dir`, or nullopt when there is none: it
   * must be a regular, executable file, so a symlinked leaf is rejected.
   */
  static std::optional<AbsolutePath> findSiblingEdenFs(AbsolutePathPiece dir);

  /**
   * Path to the edenfs binary to relaunch: the one installed next to this
   * privhelper, which keeps both on the same version, falling back to argv[0]
   * of `command` when there is no sibling. Throws when neither is usable.
   *
   * Virtual because a unit test has no sibling edenfs to point at.
   */
  virtual AbsolutePath resolveEdenFsBinary(
      const RestartSentinel::RelaunchCommand& command) const;

  /**
   * Verify that resetting the child IDs will select the privhelper's owner.
   * Throws when the real IDs are root or do not match that owner.
   *
   * Virtual so tests can supply controlled credentials.
   */
  virtual void validateRestartOwner() const;

 private:
#ifndef __APPLE__
  enum class BindUnmountResult {
    Unmounted,
    AlreadyUnmounted,
  };

  BindUnmountResult bindUnmountByFd(
      const folly::File& targetFd,
      const char* mountPath);
  virtual int umountBindMountByFd(const char* procFdPath);
#endif
  virtual void insecureBindUnmount(const char* mountPath);
  virtual void bindUnmount(const char* mountPath, folly::StringPiece mountRoot);
  virtual void setLogFile(folly::File logFile);
  virtual void setMemoryPriorityForProcess(pid_t pid, int priority);

  std::unique_ptr<folly::EventBase> eventBase_;
  UnixSocket::UniquePtr conn_;
  std::unique_ptr<FileAccessMonitorProcess> famProcess_;
  // Whether the daemon closed the socket, as opposed to the loop ending for
  // another reason: a receive error also ends the loop but leaves the daemon
  // running, and relaunching then would put two daemons on the same mounts.
  //
  // Written from eofReceived() and read after the loop exits, both on the
  // EventBase thread, so it needs no synchronization.
  bool peerExited_{false};

  // Whether nfsMount() overrides the filesystem type of mounts served over a
  // Unix domain socket. Atomic only so tests can read it from another thread.
  std::atomic<bool> nfsFstypeOverride_{true};

  // The privhelper server only has a single thread,
  // so we don't need to lock the following state
  std::map<std::string, RegisteredMount> mountPoints_;
};

} // namespace facebook::eden
