/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/config/MountProtocol.h"

namespace facebook::eden {

namespace {

constexpr auto mountProtocolStr = [] {
  std::array<folly::StringPiece, 4> mapping{};
  mapping[folly::to_underlying(MountProtocol::FUSE)] = "FUSE";
  mapping[folly::to_underlying(MountProtocol::PRJFS)] = "PrjFS";
  mapping[folly::to_underlying(MountProtocol::NFS)] = "NFS";
  mapping[folly::to_underlying(MountProtocol::FSKIT)] = "FSKit";
  return mapping;
}();

} // namespace

folly::Expected<MountProtocol, std::string>
FieldConverter<MountProtocol>::fromString(
    folly::StringPiece value,
    const std::map<std::string, std::string>& /*unused*/) const {
  for (auto protocol = 0ul; protocol < mountProtocolStr.size(); protocol++) {
    if (value.equals(
            mountProtocolStr[protocol], folly::AsciiCaseInsensitive())) {
      return static_cast<MountProtocol>(protocol);
    }
  }

  return folly::makeUnexpected(
      fmt::format("Failed to convert value '{}' to a MountProtocol.", value));
}

std::string FieldConverter<MountProtocol>::toDebugString(
    MountProtocol value) const {
  return mountProtocolStr[folly::to_underlying(value)].str();
}

} // namespace facebook::eden
