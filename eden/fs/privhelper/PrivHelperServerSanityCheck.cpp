/*
 * @lint-ignore-every LICENSELINT
 *
 * Copyright (c) Meta Platforms, Inc. and its affiliates.
 * Copyright (C) 2001-2007  Miklos Szeredi <miklos@szeredi.hu>
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#ifndef _WIN32

#include "eden/fs/privhelper/PrivHelperRollback.h"
#include "eden/fs/privhelper/PrivHelperServer.h"

#include <fcntl.h>
#include <folly/Conv.h>
#include <folly/File.h>
#include <folly/FileUtil.h>
#include <folly/ScopeGuard.h>
#include <folly/String.h>
#include <folly/logging/xlog.h>
#include <sys/mount.h>
#include <cerrno>
#include <string>
#include <unordered_map>
#include <unordered_set>

#include "eden/common/utils/ErrnoUtils.h"
#include "eden/common/utils/FSDetect.h"
#include "eden/common/utils/Throw.h"
#include "eden/fs/utils/MountInfoTable.h"

#ifdef __linux__

#include <linux/capability.h>
#include <linux/openat2.h>
#include <sys/fsuid.h>
#include <sys/prctl.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <array>

#endif

namespace facebook::eden {

namespace {

#ifdef __linux__
#ifndef SYS_faccessat2
#ifdef __NR_faccessat2
#define SYS_faccessat2 __NR_faccessat2
#elif defined(__x86_64__) || defined(__aarch64__)
#define SYS_faccessat2 439
#else
#error "faccessat2 syscall number is required"
#endif
#endif
#endif

#ifdef __linux__
bool isOldEdenMountFd(int fd) {
  std::string fdInfo;
  folly::checkUnixError(
      folly::readFile(fmt::format("/proc/self/fdinfo/{}", fd).c_str(), fdInfo)
          ? 0
          : -1,
      "cannot read mount fdinfo");
  std::vector<folly::StringPiece> lines;
  folly::split('\n', fdInfo, lines);
  std::string mountIdPrefix;
  for (auto line : lines) {
    if (line.startsWith("mnt_id:")) {
      mountIdPrefix = fmt::format(
          "{} ", folly::to<uint64_t>(folly::trimWhitespace(line.subpiece(7))));
      break;
    }
  }
  if (mountIdPrefix.empty()) {
    throw std::runtime_error("mount ID missing from fdinfo");
  }

  std::string mountInfo;
  folly::checkUnixError(
      folly::readFile("/proc/self/mountinfo", mountInfo) ? 0 : -1,
      "cannot read mountinfo");
  lines.clear();
  folly::split('\n', mountInfo, lines);
  for (auto line : lines) {
    if (!line.startsWith(mountIdPrefix)) {
      continue;
    }
    const auto separator = line.find(" - ");
    if (separator == folly::StringPiece::npos) {
      throw std::runtime_error("mountinfo entry has no field separator");
    }
    std::vector<folly::StringPiece> fields;
    folly::split(' ', line.subpiece(separator + 3), fields);
    return fields.size() >= 3 && is_edenfs_fs_type(fields.at(1));
  }
  return false;
}
#endif

/**
 * Determines whether the given mountPoint is contained in the mount table
 * and looks like it was previously mounted by EdenFS.
 */
bool isOldEdenMount(const std::string& mountPoint) {
#ifdef __linux__
  try {
    folly::File fd(mountPoint.c_str(), O_PATH | O_DIRECTORY | O_CLOEXEC);
    return isOldEdenMountFd(fd.fd());
  } catch (const std::exception& ex) {
    XLOGF(WARN, "Cannot identify mount {}: {}", mountPoint, ex.what());
    return false;
  }
#else
  struct statfs* buf;
  // MNT_NOWAIT returns the mount list the kernel keeps without asking any
  // filesystem for fresh statistics, so the unresponsive mount being checked
  // can't block it. The check only reads the mount point, type and source.
  int count = getmntinfo(&buf, MNT_NOWAIT);
  if (count == 0) {
    XLOGF(ERR, "getmntinfo failed: {}", folly::errnoStr(errno));
  } else {
    for (int i = 0; i < count; i++) {
      if (std::string(buf[i].f_mntonname) == mountPoint &&
          (is_edenfs_fs_type(buf[i].f_fstypename) ||
           is_edenfs_nfs_mount(buf[i].f_fstypename, buf[i].f_mntfromname))) {
        return true;
      }
    }
  }
#endif
  XLOGF(DBG4, "Could not verify that {} is an old EdenFS mount.", mountPoint);
  return false;
}

bool isErrorSafeToIgnore(int err, bool isNFS, const std::string& mountPoint) {
  // Some remote filesystems like AFS and FUSE return ENOTCONN if the mount
  // is still in the kernel mount table but the socket is closed. Allow
  // mounting in that case if the hanging mount looks like it was
  // previously mounted by EdenFS.
  //
  // Other remote filesystems (mainly NFS) return a variety of errors when
  // mounts are hanging. We've currently observed EIO and ETIMEDOUT depending
  // on whether hard or soft NFS mounts are utilized.
  //
  // In all likelihood, this is a mount from a prior EdenFS
  // process that crashed without unmounting.
  return isErrnoFromHangingMount(err, isNFS) && isOldEdenMount(mountPoint);
}

/**
 * EdenFS should only be mounted over some filesystems.
 *
 * This is copied from fusermount.c:
 * https://github.com/libfuse/libfuse/blob/master/util/fusermount.c#L990
 */
void sanityCheckFs(const std::string& mountPoint, int mountPointFd = -1) {
#ifndef __APPLE__
  struct statfs fsBuf;
  const auto rc = mountPointFd >= 0 ? fstatfs(mountPointFd, &fsBuf)
                                    : statfs(mountPoint.c_str(), &fsBuf);
  if (rc < 0) {
    auto err = errno;
    throwf<std::domain_error>(
        "statfs failed for: {}: {}", mountPoint, folly::errnoStr(err));
  }

  constexpr typeof(fsBuf.f_type) allowedFs[] = {
      0x61756673 /* AUFS_SUPER_MAGIC */,
      0x00000187 /* AUTOFS_SUPER_MAGIC */,
      0xCA451A4E /* BCACHEFS_STATFS_MAGIC */,
      0x9123683E /* BTRFS_SUPER_MAGIC */,
      0x00C36400 /* CEPH_SUPER_MAGIC */,
      0xFF534D42 /* CIFS_MAGIC_NUMBER */,
      0x0000F15F /* ECRYPTFS_SUPER_MAGIC */,
      0X2011BAB0 /* EXFAT_SUPER_MAGIC */,
      0x0000EF53 /* EXT[234]_SUPER_MAGIC */,
      0xF2F52010 /* F2FS_SUPER_MAGIC */,
      0x65735546 /* FUSE_SUPER_MAGIC */,
      0x01161970 /* GFS2_MAGIC */,
      0x47504653 /* GPFS_SUPER_MAGIC */,
      0x0000482b /* HFSPLUS_SUPER_MAGIC */,
      0x000072B6 /* JFFS2_SUPER_MAGIC */,
      0x3153464A /* JFS_SUPER_MAGIC */,
      0x0BD00BD0 /* LL_SUPER_MAGIC */,
      0X00004D44 /* MSDOS_SUPER_MAGIC */,
      0x0000564C /* NCP_SUPER_MAGIC */,
      0x00006969 /* NFS_SUPER_MAGIC */,
      0x00003434 /* NILFS_SUPER_MAGIC */,
      0x5346544E /* NTFS_SB_MAGIC */,
      0x5346414f /* OPENAFS_SUPER_MAGIC */,
      0x794C7630 /* OVERLAYFS_SUPER_MAGIC */,
      0x52654973 /* REISERFS_SUPER_MAGIC */,
      0xFE534D42 /* SMB2_SUPER_MAGIC */,
      0x73717368 /* SQUASHFS_MAGIC */,
      0x01021994 /* TMPFS_MAGIC */,
      0x24051905 /* UBIFS_SUPER_MAGIC */,
      0x736675005346544e /* UFSD */,
      0x18031977 /* WEKA */,
      0x58465342 /* XFS_SB_MAGIC */,
      0x2FC12FC1 /* ZFS_SUPER_MAGIC */,
  };

  for (auto i = 0u; i < sizeof(allowedFs) / sizeof(allowedFs[0]); i++) {
    if (allowedFs[i] == fsBuf.f_type) {
      return;
    }
  }

  throwf<std::domain_error>(
      "Cannot mount over filesystem type: {}", fsBuf.f_type);
#else
  (void)mountPoint;
  (void)mountPointFd;
#endif
}

#ifdef __linux__
void checkMountPointWriteAccess(
    const std::string& mountPoint,
    int mountPointFd) {
  const auto rc = static_cast<int>(
      syscall(SYS_faccessat2, mountPointFd, "", W_OK, AT_EMPTY_PATH));
  if (rc == 0) {
    return;
  }

  const auto err = errno;
  throwf<std::domain_error>(
      "User:{} doesn't have write access to {}: {}",
      getuid(),
      mountPoint,
      folly::errnoStr(err));
}
#endif

} // namespace

#ifdef __linux__
folly::File PrivHelperServer::openPathAsUser(
    const std::string& path,
    int accessMode) const {
  const auto savedUid = static_cast<uid_t>(setfsuid(static_cast<uid_t>(-1)));
  const auto savedGid = static_cast<gid_t>(setfsgid(static_cast<gid_t>(-1)));
  const auto dumpable = prctl(PR_GET_DUMPABLE);
  folly::checkUnixError(dumpable, "cannot read privhelper dumpability");
  __user_cap_header_struct header{_LINUX_CAPABILITY_VERSION_3, 0};
  std::array<__user_cap_data_struct, _LINUX_CAPABILITY_U32S_3> savedCaps{};
  folly::checkUnixError(
      syscall(SYS_capget, &header, savedCaps.data()),
      "cannot read privhelper capabilities");
  SCOPE_EXIT {
    setfsuid(savedUid);
    setfsgid(savedGid);
    XCHECK_EQ(savedUid, static_cast<uid_t>(setfsuid(static_cast<uid_t>(-1))));
    XCHECK_EQ(savedGid, static_cast<gid_t>(setfsgid(static_cast<gid_t>(-1))));
    XCHECK_EQ(0, syscall(SYS_capset, &header, savedCaps.data()));
    // PR_SET_DUMPABLE only accepts 0 and 1; keep a kernel-only value of 2
    // non-dumpable rather than allowing user-readable core dumps.
    XCHECK_EQ(0, prctl(PR_SET_DUMPABLE, dumpable == 1 ? 1 : 0));
  };

  setfsgid(gid_);
  setfsuid(uid_);
  if (static_cast<uid_t>(setfsuid(static_cast<uid_t>(-1))) != uid_ ||
      static_cast<gid_t>(setfsgid(static_cast<gid_t>(-1))) != gid_) {
    folly::throwSystemErrorExplicit(
        EPERM, "cannot select filesystem credentials for user ", uid_);
  }
  if (uid_ != 0) {
    auto caps = savedCaps;
    folly::checkUnixError(syscall(SYS_capget, &header, caps.data()));
    // Securebits can disable setfsuid's automatic capability drop.
    caps[0].effective &=
        ~((1U << CAP_DAC_OVERRIDE) | (1U << CAP_DAC_READ_SEARCH));
    folly::checkUnixError(
        syscall(SYS_capset, &header, caps.data()),
        "cannot drop filesystem access overrides");
  }

  open_how how{};
  how.flags = O_PATH | O_DIRECTORY | O_CLOEXEC;
  how.resolve = RESOLVE_NO_MAGICLINKS;
  const auto fd = static_cast<int>(
      syscall(SYS_openat2, AT_FDCWD, path.c_str(), &how, sizeof(how)));
  folly::checkUnixError(fd, "user ", uid_, " cannot open ", path);
  folly::File file{fd, true};
  // AT_EACCESS preserves the scoped filesystem credentials and capabilities.
  // O_PATH checks ancestor traversal but does not check access to the leaf.
  folly::checkUnixError(
      syscall(SYS_faccessat2, fd, "", accessMode, AT_EMPTY_PATH | AT_EACCESS),
      "user ",
      uid_,
      " cannot access ",
      path);
  return file;
}
#endif

void PrivHelperServer::sanityCheckOpenedMountPoint(
    const std::string& mountPoint,
    int mountPointFd) {
  struct stat st{};
  if (fstat(mountPointFd, &st) < 0) {
    auto err = errno;
    throwf<std::domain_error>(
        "User:{} cannot stat {}: {}",
        getuid(),
        mountPoint,
        folly::errnoStr(err));
  }

  if (!S_ISDIR(st.st_mode)) {
    throwf<std::domain_error>("{} isn't a directory", mountPoint);
  }

  if (st.st_uid != uid_) {
    throwf<std::domain_error>(
        "User:{} isn't the owner of: {}", uid_, mountPoint);
  }

#ifdef __linux__
  if (!disablePrivHelperHardening()) {
    checkMountPointWriteAccess(mountPoint, mountPointFd);
  }
#endif
  sanityCheckFs(mountPoint, mountPointFd);
}

#ifdef __linux__
namespace {
uint64_t mountIdForFd(int fd) {
  std::string contents;
  folly::checkUnixError(
      folly::readFile(fmt::format("/proc/self/fdinfo/{}", fd).c_str(), contents)
          ? 0
          : -1,
      "cannot read mount descriptor identity");
  std::vector<folly::StringPiece> lines;
  folly::split('\n', contents, lines);
  for (const auto line : lines) {
    if (line.startsWith("mnt_id:")) {
      return folly::to<uint64_t>(folly::trimWhitespace(line.subpiece(7)));
    }
  }
  throw std::runtime_error("mount ID missing from fdinfo");
}
} // namespace

std::vector<MountTableEntry> PrivHelperServer::mountsForCleanup() const {
  auto mounts = getAllMounts({.includeMountId = true});
  if (mounts.hasError()) {
    folly::throwSystemErrorExplicit(mounts.error(), "cannot enumerate mounts");
  }
  return std::move(mounts).value();
}

int PrivHelperServer::unmountCleanupMount(int mountFd) {
  const auto path = fmt::format("/proc/self/fd/{}", mountFd);
  return umount2(path.c_str(), MNT_DETACH);
}
#endif

SanityCheckResult PrivHelperServer::insecureCleanupStaleBindMounts(
    const std::string& checkoutPath) {
  SanityCheckResult result{};
#ifdef __linux__
  auto mounts = getMountsUnderPath(checkoutPath);
  if (mounts.hasError()) {
    XLOGF(
        WARN,
        "Cannot enumerate legacy cleanup mounts under {}: {}",
        checkoutPath,
        folly::errnoStr(mounts.error()));
    return result;
  }
  result.staleRedirectionMountsFound = static_cast<uint32_t>(mounts->size());
  std::sort(mounts->begin(), mounts->end(), [](const auto& a, const auto& b) {
    return a.mountPoint > b.mountPoint;
  });
  for (const auto& mount : *mounts) {
    if (umount2(mount.mountPoint.c_str(), MNT_DETACH) == 0) {
      ++result.staleRedirectionMountsSucceeded;
    } else {
      ++result.staleRedirectionMountsFailed;
      XLOGF(
          WARN,
          "Cannot unmount legacy cleanup mount {}: {}",
          mount.mountPoint,
          folly::errnoStr(errno));
    }
  }
#else
  (void)checkoutPath;
#endif
  return result;
}

SanityCheckResult PrivHelperServer::cleanupStaleBindMounts(
    const std::string& checkoutPath,
    int checkoutFd) {
  SanityCheckResult result{};
#ifdef __linux__
  if (disablePrivHelperHardening()) {
    XLOGF(
        WARN,
        "Using legacy path-based redirection cleanup under {}",
        checkoutPath);
    return insecureCleanupStaleBindMounts(checkoutPath);
  }
  try {
    // Use the pinned directory's current name only to discover candidates.
    const auto fdPath = fmt::format("/proc/self/fd/{}", checkoutFd);
    std::array<char, PATH_MAX> pathBuffer{};
    const auto pathLength =
        readlink(fdPath.c_str(), pathBuffer.data(), pathBuffer.size());
    folly::checkUnixError(pathLength, "cannot read pinned checkout path");
    if (static_cast<size_t>(pathLength) == pathBuffer.size()) {
      throw std::runtime_error("pinned checkout path is too long");
    }
    const auto prefix = std::string(pathBuffer.data(), pathLength) + "/";
    const auto checkoutId = mountIdForFd(checkoutFd);
    auto allMounts = mountsForCleanup();
    std::vector<MountTableEntry> mounts;
    std::unordered_map<uint64_t, std::string> remaining;
    for (auto& mount : allMounts) {
      if (folly::StringPiece(mount.mountPoint).startsWith(prefix)) {
        remaining.emplace(mount.mountId, mount.mountPoint);
        mounts.push_back(std::move(mount));
      }
    }
    result.staleRedirectionMountsFound = static_cast<uint32_t>(mounts.size());
    std::sort(mounts.begin(), mounts.end(), [](const auto& a, const auto& b) {
      return a.mountPoint > b.mountPoint;
    });
    struct PinnedMount {
      folly::File fd;
      uint64_t id;
      std::string path;
      bool validated{false};
    };
    std::vector<PinnedMount> batch;
    std::unordered_map<uint64_t, size_t> batchIds;
    std::unordered_set<std::string> batchPaths;
    const auto flush = [&] {
      if (batch.empty()) {
        batchPaths.clear();
        return;
      }
      // IDs cannot be reused while pinned. Validate the whole batch in one
      // scan.
      for (const auto& mount : mountsForCleanup()) {
        const auto match = batchIds.find(mount.mountId);
        if (match != batchIds.end()) {
          auto& pinned = batch.at(match->second);
          pinned.validated = mount.mountPoint == pinned.path;
        }
      }
      for (const auto& pinned : batch) {
        remaining.erase(pinned.id);
        if (!pinned.validated) {
          XLOGF(
              WARN, "Skipping cleanup mount {}: identity changed", pinned.path);
          continue;
        }
        if (unmountCleanupMount(pinned.fd.fd()) == 0) {
          ++result.staleRedirectionMountsSucceeded;
        } else {
          XLOGF(
              WARN,
              "Cannot unmount cleanup mount {}: {}",
              pinned.path,
              folly::errnoStr(errno));
        }
      }
      batch.clear();
      batchIds.clear();
      batchPaths.clear();
    };
    for (const auto& mount : mounts) {
      // Bound open FDs and peel stacked children before progressing to a
      // parent.
      constexpr size_t kMaxPinnedMounts = 64;
      if (batch.size() == kMaxPinnedMounts ||
          batchPaths.contains(mount.mountPoint)) {
        flush();
      }
      batchPaths.insert(mount.mountPoint);
      try {
        const auto relative = mount.mountPoint.substr(prefix.size());
        open_how how{};
        how.flags = O_PATH | O_DIRECTORY | O_CLOEXEC;
        how.resolve = RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS;
        const auto fd = static_cast<int>(syscall(
            SYS_openat2, checkoutFd, relative.c_str(), &how, sizeof(how)));
        folly::checkUnixError(
            fd, "cannot pin cleanup mount ", mount.mountPoint);
        folly::File file{fd, true};
        const auto id = mountIdForFd(fd);
        const auto discovered = remaining.find(id);
        if (id == checkoutId || discovered == remaining.end() ||
            discovered->second != mount.mountPoint) {
          throw std::runtime_error("cleanup mount identity changed");
        }
        batchIds.emplace(id, batch.size());
        batch.push_back(PinnedMount{std::move(file), id, mount.mountPoint});
      } catch (const std::exception& ex) {
        XLOGF(
            WARN, "Skipping cleanup mount {}: {}", mount.mountPoint, ex.what());
      }
    }
    flush();
  } catch (const std::exception& ex) {
    XLOGF(
        WARN,
        "Skipping redirection cleanup under {}: {}",
        checkoutPath,
        ex.what());
  }
  result.staleRedirectionMountsFailed = result.staleRedirectionMountsFound -
      result.staleRedirectionMountsSucceeded;
#else
  (void)checkoutPath;
  (void)checkoutFd;
#endif
  return result;
}

int PrivHelperServer::statMountPoint(const char* path, struct stat* st) const {
  // This probes mount health; it does not authorize any path-based operation.
  // @lint-ignore CLANGTIDY facebook-hte-BadCall-stat
  return stat(path, st);
}

bool PrivHelperServer::detectAndUnmountStaleMount(
    const std::string& mountPoint,
    bool isNFS,
    bool isHardMount) {
  const auto hardeningDisabled = disablePrivHelperHardening();
  folly::File mountFd;
  std::string probePath = mountPoint;
#ifdef __linux__
  if (!hardeningDisabled) {
    const auto fd = open(mountPoint.c_str(), O_PATH | O_DIRECTORY | O_CLOEXEC);
    if (fd < 0) {
      throwf<std::domain_error>(
          "User:{} cannot open {}: {}",
          uid_,
          mountPoint,
          folly::errnoStr(errno));
    }
    mountFd = folly::File(fd, true);
    // Probes, identity lookup, and unmount must refer to this same mount even
    // when a caller replaces an ancestor of mountPoint.
    probePath = fmt::format("/proc/self/fd/{}", fd);
  }
#endif
  struct stat st;
  // Stat the mount point to determine its status. If the errno matches certain
  // values, then the mount is likely hanging. We'll try to unmount it before
  // performing further sanity checks. On any other error, we throw.

  if (statMountPoint(probePath.c_str(), &st) < 0) {
    auto err = errno;
    XLOGF(
        WARN,
        "Error when sanity checking mount {}: {}. Checking for stale mounts.",
        mountPoint,
        folly::errnoStr(err));

    // Avoids running on hard NFS mounts since IO into hard mounts can hang
    // forever instead of returning an error.
    if (!isHardMount && isErrorSafeToIgnore(err, isNFS, probePath)) {
      XLOGF(
          INFO,
          "Found a stale mount {}: {}. Attempting to unmount it",
          mountPoint,
          folly::errnoStr(err));
      unmountStaleMount(mountPoint, mountFd.fd());
      return true;
    } else {
      throwf<std::domain_error>(
          "User:{} cannot stat {}: {}",
          getuid(),
          mountPoint,
          folly::errnoStr(err));
    }
  }

  // Sometimes stat will not return this error even if the mount is
  // hanging because the stat'd path is cached by the kernel. We check for this
  // by attempting to stat a non-existent file under a non-existent folder.
  if (!isHardMount) {
    // Check in case the mount point is cached in the kernel.
    XLOG(DBG4, "Double checking whether a stale mount is present.");
    std::string test_path =
        probePath + "/this-folder-does-not-exist/this-file-does-not-exist";
    struct stat test_st;

    if (statMountPoint(test_path.c_str(), &test_st) < 0) {
      auto err = errno;
      const auto safeToIgnore = hardeningDisabled
          ? isErrnoFromHangingMount(err, isNFS)
          : isErrorSafeToIgnore(err, isNFS, probePath);
      if (safeToIgnore) {
        XLOGF(
            INFO,
            "Found a stale mount {}: {}. Attempting to unmount it",
            mountPoint,
            folly::errnoStr(err));
        unmountStaleMount(mountPoint, mountFd.fd());
        return true;
      }
    }
    XLOGF(DBG4, "Mount {} is not stale.", mountPoint);
  }

  // On Linux/FUSE, it's possible that statfs will return an error if the mount
  // is stale, but stat won't. Try statfs as well to catch this case.
#ifdef __linux__
  struct statfs fsBuf;
  if (!isNFS && statfs(probePath.c_str(), &fsBuf) < 0) {
    auto err = errno;
    if (isErrorSafeToIgnore(err, isNFS, probePath)) {
      XLOGF(
          INFO,
          "Found a stale mount {}: {}. Attempting to unmount it",
          mountPoint,
          folly::errnoStr(err));
      unmountStaleMount(mountPoint, mountFd.fd());
      return true;
    } else {
      throwf<std::domain_error>(
          "statfs failed for: {}: {}", mountPoint, folly::errnoStr(err));
    }
  }
#endif
  return false;
}

SanityCheckResult PrivHelperServer::sanityCheckMountPoint(
    const std::string& mountPoint,
    const SanityCheckOptions& options) {
  XLOGF(INFO, "Sanity checking mount {}", mountPoint);
  if ((disablePrivHelperHardening() ? getuid() : uid_) == 0) {
    XLOG(INFO, "Skipping sanity check for root user.");
    return SanityCheckResult{};
  }

  SanityCheckResult result{};
  if (const auto& staleMountCheck = options.staleMountCheck()) {
    result.staleCheckoutMountUnmounted = detectAndUnmountStaleMount(
        mountPoint, staleMountCheck->isNFS, staleMountCheck->isHardMount);
  }

  if (access(mountPoint.c_str(), W_OK) < 0) {
    auto err = errno;
    throwf<std::domain_error>(
        "User:{} doesn't have write access to {}: {}",
        getuid(),
        mountPoint,
        folly::errnoStr(err));
  }

  folly::File file;
  try {
    file = folly::File(mountPoint.c_str(), O_RDONLY);
  } catch (const std::system_error& e) {
    throwf<std::domain_error>(
        "User:{} cannot open {}: {}",
        getuid(),
        mountPoint,
        folly::errnoStr(e.code().value()));
  }
  sanityCheckOpenedMountPoint(mountPoint, file.fd());
  if (options.performBindMountCleanup()) {
    // Only clean up mounts under a checkout after the checkout path itself has
    // passed the ownership and access checks.
    auto cleanupResult = cleanupStaleBindMounts(mountPoint, file.fd());
    result.staleRedirectionMountsFound =
        cleanupResult.staleRedirectionMountsFound;
    result.staleRedirectionMountsSucceeded =
        cleanupResult.staleRedirectionMountsSucceeded;
    result.staleRedirectionMountsFailed =
        cleanupResult.staleRedirectionMountsFailed;
  }
  return result;
}

#ifndef __APPLE__
PrivHelperServer::CheckedMountPoint
PrivHelperServer::openAndSanityCheckMountPoint(
    const std::string& mountPoint,
    const SanityCheckOptions& options) {
  if (disablePrivHelperHardening()) {
    XLOGF(
        WARN,
        "Using legacy mount target validation for `{}` because privhelper hardening is disabled",
        mountPoint);
    auto sanityResult = sanityCheckMountPoint(mountPoint, options);
    return CheckedMountPoint{
        folly::File(mountPoint.c_str(), O_PATH | O_DIRECTORY | O_CLOEXEC),
        sanityResult};
  }

  XLOGF(INFO, "Sanity checking mount {}", mountPoint);
  if (uid_ == 0) {
    XLOG(INFO, "Skipping sanity check for root user.");
    auto targetFd = openPathAsUser(mountPoint, F_OK);
    return CheckedMountPoint{std::move(targetFd), SanityCheckResult{}};
  }

  SanityCheckResult result{};
  if (const auto& staleMountCheck = options.staleMountCheck()) {
    result.staleCheckoutMountUnmounted = detectAndUnmountStaleMount(
        mountPoint, staleMountCheck->isNFS, staleMountCheck->isHardMount);
  }

  auto targetFd = openPathAsUser(mountPoint, R_OK | W_OK | X_OK);
  sanityCheckOpenedMountPoint(mountPoint, targetFd.fd());
  if (options.performBindMountCleanup()) {
    // Only clean up mounts under a checkout after the checkout path itself has
    // passed the ownership and access checks.
    auto cleanupResult = cleanupStaleBindMounts(mountPoint, targetFd.fd());
    result.staleRedirectionMountsFound =
        cleanupResult.staleRedirectionMountsFound;
    result.staleRedirectionMountsSucceeded =
        cleanupResult.staleRedirectionMountsSucceeded;
    result.staleRedirectionMountsFailed =
        cleanupResult.staleRedirectionMountsFailed;
  }
  return CheckedMountPoint{std::move(targetFd), result};
}
#endif
} // namespace facebook::eden

#endif
