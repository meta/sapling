/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include "eden/fs/config/FieldConverter.h"

namespace facebook::eden {

enum class MountProtocol {
  FUSE,
  PRJFS,
  NFS,
  FSKIT,
};

constexpr MountProtocol kMountProtocolDefault =
    folly::kIsWindows ? MountProtocol::PRJFS : MountProtocol::FUSE;

template <>
class FieldConverter<MountProtocol> {
 public:
  folly::Expected<MountProtocol, std::string> fromString(
      folly::StringPiece value,
      const std::map<std::string, std::string>& convData) const;

  std::string toDebugString(MountProtocol value) const;
};

} // namespace facebook::eden
