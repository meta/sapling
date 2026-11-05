/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#ifndef _WIN32

#include "eden/fs/utils/MountInfoTable.h"

#ifdef __linux__

#include <folly/FileUtil.h>

#include <unistd.h>
#include <cerrno>

#include "eden/fs/utils/Statmount.h"

namespace facebook::eden {

namespace {

constexpr size_t kStatmountBufSize = 4096;
constexpr size_t kListmountBufSize = 1024;

bool shouldUseProcMountInfo(int error) {
  return error == ENOSYS || error == EOPNOTSUPP || error == EOVERFLOW;
}

// statmount(2) fails with ENOENT for a mount unmounted after listmount(2)
// returned its id.
bool isUnmountedSinceListing(int error) {
  return error == ENOENT;
}

folly::Expected<std::vector<MountTableEntry>, int> readProcMountInfo(
    MountInfoOptions options) {
  std::string contents;
  if (!folly::readFile("/proc/self/mountinfo", contents)) {
    return folly::makeUnexpected(errno);
  }
  return parseProcMountInfo(contents, options);
}

uint64_t getStatmountMask(MountInfoOptions options) {
  auto mask = STATMOUNT_SB_BASIC | STATMOUNT_MNT_ROOT | STATMOUNT_MNT_POINT |
      STATMOUNT_FS_TYPE;
  if (options.includeMountId) {
    mask |= STATMOUNT_MNT_BASIC;
  }
  if (options.includeMountSource) {
    mask |= STATMOUNT_SB_SOURCE;
  }
  if (options.includeMountOptions) {
    mask |= STATMOUNT_MNT_OPTS;
  }
  return mask;
}

/**
 * Call listmount(2) to enumerate all mount IDs under LSMT_ROOT.
 * Returns an error code on failure (ENOSYS for unsupported kernels,
 * or the errno from the failed syscall).
 */
folly::Expected<std::vector<uint64_t>, int> listAllMountIds() {
  struct mnt_id_req req{};
  req.size = MNT_ID_REQ_SIZE_VER0;
  req.mnt_id = LSMT_ROOT;
  req.param = 0;

  std::vector<uint64_t> ids;
  std::vector<uint64_t> page(kListmountBufSize);
  while (true) {
    auto lastRequestedMountId = req.param;
    long ret = syscall(__NR_listmount, &req, page.data(), page.size(), 0);
    if (ret < 0) {
      return folly::makeUnexpected(errno);
    }

    auto numReturned = static_cast<size_t>(ret);
    size_t numAdded = 0;
    for (size_t index = 0; index < numReturned; ++index) {
      auto mountId = page[index];
      if (mountId == lastRequestedMountId) {
        continue;
      }
      ids.push_back(mountId);
      ++numAdded;
    }

    if (numReturned < kListmountBufSize) {
      break;
    }

    req.param = page[numReturned - 1];
    if (numAdded == 0 || req.param == lastRequestedMountId) {
      return folly::makeUnexpected(ELOOP);
    }
  }
  return ids;
}

/**
 * Call statmount(2) for a single mount ID.
 * Returns ENOSYS for unsupported kernels, EOPNOTSUPP for missing requested
 * fields, or the errno from a failed syscall.
 */
folly::Expected<MountTableEntry, int> statmountById(
    uint64_t mntId,
    MountInfoOptions options) {
  // Allocate buffer for statmount result including variable-length strings
  std::vector<char> buf(kStatmountBufSize);
  auto* sm = reinterpret_cast<struct statmount*>(buf.data());

  struct mnt_id_req req{};
  req.size = MNT_ID_REQ_SIZE_VER0;
  req.mnt_id = mntId;
  req.param = getStatmountMask(options);

  const auto ret = syscall(__NR_statmount, &req, sm, buf.size(), 0);
  if (ret < 0) {
    // EOVERFLOW does not return the required buffer size. Use mountinfo for
    // these entries instead of retrying with an unknown size.
    return folly::makeUnexpected(errno);
  }

  return detail::parseStatmount(*sm, options);
}

} // namespace

namespace detail {

folly::Expected<MountTableEntry, int> parseStatmount(
    const struct statmount& sm,
    MountInfoOptions options) {
  // Kernels can support statmount but omit newer fields such as SB_SOURCE
  // and MNT_OPTS. Do not treat missing metadata as empty mount information.
  const auto requestedMask = getStatmountMask(options);
  if ((sm.mask & requestedMask) != requestedMask) {
    return folly::makeUnexpected(EOPNOTSUPP);
  }

  MountTableEntry info;
  info.mountId = options.includeMountId ? sm.mnt_id_old : 0;
  info.devMajor = sm.sb_dev_major;
  info.devMinor = sm.sb_dev_minor;
  info.mountRoot = sm.str + sm.mnt_root;
  info.mountPoint = sm.str + sm.mnt_point;
  info.fsType = sm.str + sm.fs_type;

  if (options.includeMountSource) {
    info.mountSource = sm.str + sm.sb_source;
  }
  if (options.includeMountOptions) {
    info.mountOptions = sm.str + sm.mnt_opts;
  }

  return info;
}

} // namespace detail

folly::Expected<std::optional<MountTableEntry>, int> getMountInfoForPath(
    const char* path,
    MountInfoOptions options) {
  auto fromProc =
      [&]() -> folly::Expected<std::optional<MountTableEntry>, int> {
    auto mounts = readProcMountInfo(options);
    if (mounts.hasError()) {
      return folly::makeUnexpected(mounts.error());
    }
    for (auto& mount : mounts.value()) {
      if (mount.mountPoint == path) {
        return std::move(mount);
      }
    }
    return std::nullopt;
  };

  auto idsResult = listAllMountIds();
  if (idsResult.hasError()) {
    if (shouldUseProcMountInfo(idsResult.error())) {
      return fromProc();
    }
    return folly::makeUnexpected(idsResult.error());
  }

  for (auto id : idsResult.value()) {
    auto infoResult = statmountById(id, options);
    if (infoResult.hasError()) {
      if (isUnmountedSinceListing(infoResult.error())) {
        continue;
      }
      if (shouldUseProcMountInfo(infoResult.error())) {
        return fromProc();
      }
      return folly::makeUnexpected(infoResult.error());
    }
    if (infoResult.value().mountPoint == path) {
      return std::move(infoResult.value());
    }
  }
  return std::nullopt;
}

folly::Expected<std::vector<MountTableEntry>, int> getAllMounts(
    MountInfoOptions options) {
  std::vector<MountTableEntry> result;

  auto idsResult = listAllMountIds();
  if (idsResult.hasError()) {
    if (shouldUseProcMountInfo(idsResult.error())) {
      return readProcMountInfo(options);
    }
    return folly::makeUnexpected(idsResult.error());
  }

  result.reserve(idsResult.value().size());
  for (auto id : idsResult.value()) {
    auto infoResult = statmountById(id, options);
    if (infoResult.hasError()) {
      if (isUnmountedSinceListing(infoResult.error())) {
        continue;
      }
      if (shouldUseProcMountInfo(infoResult.error())) {
        return readProcMountInfo(options);
      }
      return folly::makeUnexpected(infoResult.error());
    }
    result.push_back(std::move(infoResult.value()));
  }
  return result;
}

folly::Expected<std::vector<MountTableEntry>, int> getMountsUnderPath(
    const std::string& prefix,
    MountInfoOptions options) {
  std::vector<MountTableEntry> result;
  std::string prefixWithSlash = prefix + "/";

  auto mountsResult = getAllMounts(options);
  if (mountsResult.hasError()) {
    return folly::makeUnexpected(mountsResult.error());
  }

  for (auto& mount : mountsResult.value()) {
    if (mount.mountPoint.compare(0, prefixWithSlash.size(), prefixWithSlash) ==
        0) {
      result.push_back(std::move(mount));
    }
  }
  return result;
}

} // namespace facebook::eden

#endif // __linux__
#endif // _WIN32
