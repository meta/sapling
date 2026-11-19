/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/takeover/TakeoverData.h"

#include <cstring>
#include <memory>
#include <stdexcept>
#include <variant>

#include <fmt/format.h>
#include <folly/io/IOBuf.h>
#include <folly/logging/xlog.h>
#include "folly/Likely.h"

#include <thrift/lib/cpp2/protocol/Serializer.h>

#include "eden/common/utils/Bug.h"
#include "eden/common/utils/Throw.h"
#include "eden/common/utils/UnixSocket.h"

using apache::thrift::CompactSerializer;
using folly::IOBuf;
using std::string;

/**
 * Maximum size of a chunked message in bytes.
 * Message chunking happens after TakeoverData serialization.
 *
 * This 512 MB size is chosen based on the following calculations:
 * If we want to run a graceful restart for 30 million inodes, the TakeoverData
 * size would be approximately ~5GB. Multiplying this number by 2, we estimate
 * the maximum Takeover size to be ~10GB. With a chunk size of 0.5GB, this
 * results in 20 chunks. Since we use recursion to send TakeoverData, a
 * recursion depth of 20 is safe and won't cause a stack overflow.
 * The chunk size should be large enough to avoid the overhead of
 * sending multiple chunks, and smaller than 1 GB (the maximum data length that
 * we chose for transferring eden TakeoverData over UnixSocket).
 */
DEFINE_int64(
    maximumChunkSize,
    512 * 1024 * 1024, // 512 MB
    "Maximum size of a chunked message in bytes");

namespace facebook::eden {
#ifndef _WIN32
namespace {

/**
 * Determines the mount protocol for the mount point encoded in the mountInfo.
 */
TakeoverMountProtocol getTakeoverMountProtocol(
    const TakeoverData::MountInfo& mountInfo) {
  if (std::holds_alternative<FuseChannelData>(mountInfo.channelInfo)) {
    return TakeoverMountProtocol::FUSE;
  } else if (std::holds_alternative<NfsChannelData>(mountInfo.channelInfo)) {
    return TakeoverMountProtocol::NFS;
  }
  throwf<std::runtime_error>(
      "unrecognized mount protocol {} for mount: {}",
      mountInfo.channelInfo.index(),
      mountInfo.mountPath);
}

} // namespace
#endif

MountProtocol TakeoverData::MountInfo::getMountProtocol() const {
  if (std::holds_alternative<FuseChannelData>(channelInfo)) {
    return MountProtocol::FUSE;
  } else if (std::holds_alternative<NfsChannelData>(channelInfo)) {
    return MountProtocol::NFS;
  } else if (std::holds_alternative<ProjFsChannelData>(channelInfo)) {
    return MountProtocol::PRJFS;
  }

  throwf<std::runtime_error>(
      "unrecognized mount protocol {} for mount: {}",
      channelInfo.index(),
      mountPath);
}

#ifndef _WIN32

const std::set<int32_t> kSupportedTakeoverVersions{
    TakeoverData::kTakeoverProtocolVersionSeven};

namespace {
// The capabilities of protocol version 7. Both peers must have all of them.
constexpr uint64_t kRequiredCapabilities = TakeoverCapabilities::FUSE |
    TakeoverCapabilities::MOUNT_TYPES | TakeoverCapabilities::PING |
    TakeoverCapabilities::THRIFT_SERIALIZATION | TakeoverCapabilities::NFS |
    TakeoverCapabilities::RESULT_TYPE_SERIALIZATION |
    TakeoverCapabilities::ORDERED_FDS | TakeoverCapabilities::OPTIONAL_MOUNTD |
    TakeoverCapabilities::CAPABILITY_MATCHING |
    TakeoverCapabilities::INCLUDE_HEADER_SIZE |
    TakeoverCapabilities::CHUNKED_MESSAGE;
} // namespace

const uint64_t kSupportedCapabilities = kRequiredCapabilities;

std::optional<int32_t> TakeoverData::computeCompatibleVersion(
    const std::set<int32_t>& versions,
    const std::set<int32_t>& supported) {
  std::optional<int32_t> best;

  for (auto& version : versions) {
    if (best.has_value() && best.value() > version) {
      // No better than the current best
      continue;
    }
    if (std::find(supported.begin(), supported.end(), version) ==
        supported.end()) {
      // Not supported
      continue;
    }
    best = version;
  }
  return best;
}

uint64_t TakeoverData::computeCompatibleCapabilities(
    const uint64_t capabilities,
    const uint64_t supported) {
  auto compatible = supported & capabilities;
  if ((compatible & kRequiredCapabilities) != kRequiredCapabilities) {
    throw std::runtime_error{
        "The client and the server do not share a common takeover protocol "
        "implementation."};
  }
  return compatible;
}

std::vector<FileDescriptorType> TakeoverData::generateGeneralFdOrder() {
  if (UNLIKELY(injectedFdOrderForTesting.has_value())) {
    return injectedFdOrderForTesting.value();
  }

  std::vector<FileDescriptorType> fileDescriptorOrder{
      FileDescriptorType::LOCK_FILE, FileDescriptorType::THRIFT_SOCKET};
  if (mountdServerSocket != std::nullopt) {
    fileDescriptorOrder.push_back(FileDescriptorType::MOUNTD_SOCKET);
  }
  return fileDescriptorOrder;
}

void TakeoverData::serializeFd(
    FileDescriptorType type,
    std::vector<folly::File>& files) {
  folly::File* fileToSerialize{nullptr};
  switch (type) {
    case FileDescriptorType::LOCK_FILE:
      fileToSerialize = &lockFile;
      break;
    case FileDescriptorType::THRIFT_SOCKET:
      fileToSerialize = &thriftSocket;
      break;
    case FileDescriptorType::MOUNTD_SOCKET:
      fileToSerialize = &mountdServerSocket.value();
      break;
    default:
      throwf<std::runtime_error>(
          "Unexpected FileDescriptorType {}", fmt::underlying(type));
  }

  XLOGF(
      DBG7,
      "serializing file type: {} fd: {}",
      fmt::underlying(type),
      fileToSerialize->fd());
  files.push_back(std::move(*fileToSerialize));
}

void TakeoverData::deserializeFd(FileDescriptorType type, folly::File& file) {
  XLOGF(
      DBG7,
      "deserializing file type: {} fd: {}",
      fmt::underlying(type),
      file.fd());
  switch (type) {
    case FileDescriptorType::LOCK_FILE:
      lockFile = std::move(file);
      return;
    case FileDescriptorType::THRIFT_SOCKET:
      thriftSocket = std::move(file);
      return;
    case FileDescriptorType::MOUNTD_SOCKET:
      mountdServerSocket = std::move(file);
      return;
    default:
      throwf<std::runtime_error>(
          "Unexpected FileDescriptorType {}", fmt::underlying(type));
  }
}

void TakeoverData::serialize(
    uint64_t protocolCapabilities,
    UnixSocket::Message& msg) {
  // The order is serialized into the message, so it must be computed first.
  generalFDOrder = generateGeneralFdOrder();
  msg.data = serialize(protocolCapabilities);
  for (auto& fdType : generalFDOrder) {
    serializeFd(fdType, msg.files);
  }

  for (auto& mount : mountPoints) {
    if (auto fuseData = std::get_if<FuseChannelData>(&mount.channelInfo)) {
      msg.files.push_back(std::move(fuseData->fd));
    } else if (auto nfsData = std::get_if<NfsChannelData>(&mount.channelInfo)) {
      msg.files.push_back(std::move(nfsData->nfsdSocketFd));
    } else {
      throw std::runtime_error("Unexpected Channel Type");
    }
  }
}

IOBuf TakeoverData::serialize(uint64_t protocolCapabilities) {
  return serializeThrift(protocolCapabilities);
}

folly::IOBuf TakeoverData::serializeError(
    uint64_t protocolCapabilities,
    const folly::exception_wrapper& ew) {
  return serializeErrorThrift(protocolCapabilities, ew);
}

bool TakeoverData::isPing(const IOBuf* buf) {
  if (buf->length() == sizeof(uint32_t)) {
    folly::io::Cursor cursor(buf);
    auto messageType = cursor.readBE<uint32_t>();
    return messageType == MessageType::PING;
  }
  return false;
}

folly::IOBuf TakeoverData::serializePing() {
  IOBuf buf(IOBuf::CREATE, kHeaderLength);
  folly::io::Appender app(&buf, 0);
  app.writeBE<uint32_t>(static_cast<uint32_t>(MessageType::PING));
  XLOGF(DBG8, "Serialized ping message");
  return buf;
}

bool TakeoverData::isFirstChunk(const IOBuf* buf) {
  if (buf->length() == sizeof(uint32_t)) {
    folly::io::Cursor cursor(buf);
    auto messageType = cursor.readBE<uint32_t>();
    return messageType == MessageType::FIRST_CHUNK;
  }
  return false;
}

folly::IOBuf TakeoverData::serializeFirstChunk() {
  IOBuf buf(IOBuf::CREATE, kHeaderLength);
  folly::io::Appender app(&buf, 0);
  app.writeBE<uint32_t>(static_cast<uint32_t>(MessageType::FIRST_CHUNK));
  XLOGF(DBG8, "Serialized first chunk message");
  return buf;
}

bool TakeoverData::isLastChunk(const IOBuf* buf) {
  if (buf->length() == sizeof(uint32_t)) {
    folly::io::Cursor cursor(buf);
    auto messageType = cursor.readBE<uint32_t>();
    return messageType == MessageType::LAST_CHUNK;
  }
  return false;
}

folly::IOBuf TakeoverData::serializeLastChunk() {
  IOBuf buf(IOBuf::CREATE, kHeaderLength);
  folly::io::Appender app(&buf, 0);
  app.writeBE<uint32_t>(static_cast<uint32_t>(MessageType::LAST_CHUNK));
  XLOGF(DBG8, "Serialized last chunk message");
  return buf;
}

TakeoverData TakeoverData::deserialize(UnixSocket::Message& msg) {
  // Consumes the header; the payload format does not depend on the
  // capabilities it carries.
  TakeoverData::getProtocolCapabilities(&msg.data);

  auto data = TakeoverData::deserializeThrift(&msg.data);
  const auto mountPointFilesOffset = data.generalFDOrder.size();

  if (data.mountPoints.size() + mountPointFilesOffset != msg.files.size()) {
    throw_<std::runtime_error>(
        "received ",
        data.mountPoints.size(),
        " mount paths, but ",
        msg.files.size(),
        " FDs (including the lock file FD)");
  }
  uint32_t filesIndex = 0;
  for (auto fileType : data.generalFDOrder) {
    data.deserializeFd(fileType, msg.files.at(filesIndex));
    ++filesIndex;
  }
  for (size_t n = 0; n < data.mountPoints.size(); ++n) {
    auto& mountInfo = data.mountPoints[n];
    if (auto fuseData = std::get_if<FuseChannelData>(&mountInfo.channelInfo)) {
      fuseData->fd = std::move(msg.files[n + mountPointFilesOffset]);
    } else if (
        auto nfsData = std::get_if<NfsChannelData>(&mountInfo.channelInfo)) {
      nfsData->nfsdSocketFd = std::move(msg.files[n + mountPointFilesOffset]);
    } else {
      throw std::runtime_error("Unexpected Channel Type");
    }
  }
  return data;
}

uint64_t TakeoverData::getProtocolCapabilities(IOBuf* buf) {
  ProtocolCapabilitiesAndTrimSize result =
      getProtocolCapabilitiesWithoutTrim(buf);

  // trim the buffer to the real data
  buf->trimStart(result.trimSize);

  return result.protocolCapabilities;
}

TakeoverData::ProtocolCapabilitiesAndTrimSize
TakeoverData::getProtocolCapabilitiesWithoutTrim(IOBuf* buf) {
  // We need to probe the data to see which version we have
  folly::io::Cursor cursor(buf);

  auto version = cursor.readBE<uint32_t>();

  // We don't trim the buffer here but the caller should know the trimSize
  // and trim the buffer to get to the real data if needed.
  uint32_t trimSize = 0;

  switch (version) {
    case kTakeoverProtocolVersionNeverSupported:
      // An error reported before capabilities were negotiated.
      trimSize = sizeof(uint32_t);
      return {0, trimSize};
    case kTakeoverProtocolVersionSeven: {
      // for now the size of the header should just be 8 because it only
      // includes the size of the capabilities.
      std::uint32_t header_size = cursor.readBE<uint32_t>();
      if (header_size != sizeof(uint64_t)) {
        throw std::runtime_error(
            fmt::format(
                "Invalid takeover header size {:x}, expected {:x}. version: {:x}",
                header_size,
                sizeof(uint64_t),
                version));
      }

      uint64_t capabilities = cursor.readBE<uint64_t>();

      // The header contains the version, header size and header size
      // bytes (currently header size bytes equals 8 and only contain the
      // capabilities). Then the trimSize is the size of the version, header
      // size and capabilities.
      trimSize = sizeof(uint32_t) + sizeof(uint32_t) + header_size;

      return {capabilities, trimSize};
    }
    default:
      throw std::runtime_error(
          fmt::format(
              "Unrecognized TakeoverData response starting with {:x}",
              version));
  }
}

void TakeoverData::serializeHeader(
    uint64_t protocolCapabilities,
    folly::IOBufQueue& buf) {
  folly::io::QueueAppender appender(&buf, 0);
  if (protocolCapabilities == 0) {
    // An error reported before capabilities were negotiated carries only the
    // version word, so that any peer can decode it.
    appender.writeBE<uint32_t>(
        static_cast<uint32_t>(kTakeoverProtocolVersionNeverSupported));
    return;
  }
  appender.writeBE<uint32_t>(
      static_cast<uint32_t>(kTakeoverProtocolVersionSeven));
  appender.writeBE<uint32_t>(static_cast<uint32_t>(sizeof(uint64_t)));
  appender.writeBE<uint64_t>(protocolCapabilities);
}

IOBuf TakeoverData::serializeThrift(uint64_t protocolCapabilities) {
  folly::IOBufQueue bufQ{folly::IOBufQueue::cacheChainLength()};

  serializeHeader(protocolCapabilities, bufQ);

  std::vector<SerializedMountInfo> serializedMounts;
  serializedMounts.reserve(mountPoints.size());
  for (const auto& mount : mountPoints) {
    SerializedMountInfo serializedMount;

    serializedMount.mountPath() = mount.mountPath.asString();
    serializedMount.stateDirectory() = mount.stateDirectory.asString();

    if (auto fuseChannelInfo =
            std::get_if<FuseChannelData>(&mount.channelInfo)) {
      // Stuffing the fuse connection information in as a binary
      // blob because we know that the endianness of the target
      // machine must match the current system for a graceful
      // takeover, and it saves us from re-encoding an operating
      // system specific struct into a thrift file.
      serializedMount.connInfo() = std::string{
          reinterpret_cast<const char*>(&fuseChannelInfo->connInfo),
          sizeof(fuseChannelInfo->connInfo)};
    }

    serializedMount.inodeMap() = mount.inodeMap;

    serializedMount.mountProtocol() = getTakeoverMountProtocol(mount);

    serializedMounts.emplace_back(std::move(serializedMount));
  }

  SerializedTakeoverInfo serialized;
  serialized.mounts() = std::move(serializedMounts);
  serialized.fileDescriptors() = generalFDOrder;
  SerializedTakeoverResult result;
  result.takeoverData() = serialized;

  CompactSerializer::serialize(result, &bufQ);

  // At this point, bufQ can be a chain of multiple IOBufs because of how
  // serializeHeader and the mounts' serialization are implemented.
  // To efficiently split it into chunks later, we need to coalesce all the
  // chains into one. This simplifies the process of splitting the data into
  // chunks of a maximum size (FLAGS_maximumChunkSize).

  // Move the buffer queue to an IOBuf, and leaving buffer queue empty.
  auto tempBuf = bufQ.move();
  // Coalesce the chains within the IOBuf into a single chain.
  tempBuf->coalesce();
  // Append the now single-chain IOBuf back to the buffer queue.
  bufQ.append(std::move(tempBuf));

  auto chunk = bufQ.splitAtMost(FLAGS_maximumChunkSize);
  while (bufQ.chainLength() > 0) {
    chunk->appendToChain(bufQ.splitAtMost(FLAGS_maximumChunkSize));
  }
  XLOGF(
      INFO,
      "Serialized {} bytes TakeoverData in {} chunks",
      chunk->computeChainDataLength(),
      chunk->countChainElements());
  return std::move(*chunk);
}

folly::IOBuf TakeoverData::serializeErrorThrift(
    uint64_t protocolCapabilities,
    const folly::exception_wrapper& ew) {
  folly::IOBufQueue bufQ;

  serializeHeader(protocolCapabilities, bufQ);

  auto exceptionClassName = ew.class_name();
  folly::StringPiece what = ew ? ew.get_exception()->what() : "";

  SerializedTakeoverResult serialized;
  serialized.errorReason() =
      folly::to<std::string>(exceptionClassName, ": ", what);

  CompactSerializer::serialize(serialized, &bufQ);

  return std::move(*bufQ.move());
}

TakeoverData TakeoverData::deserializeThrift(IOBuf* buf) {
  auto serialized =
      CompactSerializer::deserialize<SerializedTakeoverResult>(buf);
  switch (serialized.getType()) {
    case SerializedTakeoverResult::Type::errorReason:
      throw std::runtime_error(serialized.errorReason().as_const().value());

    case SerializedTakeoverResult::Type::takeoverData: {
      auto takeoverData =
          deserializeThriftMounts(*(serialized.takeoverData()->mounts()));
      takeoverData.generalFDOrder =
          *(serialized.takeoverData()->fileDescriptors());
      return takeoverData;
    }
    case SerializedTakeoverResult::Type::__EMPTY__:
      // This case triggers when there are no mounts to pass between
      // the processes; we allow for it here and return an empty
      // TakeoverData instance.
      return TakeoverData{};
  }
  throw std::runtime_error(
      "impossible enum variant for SerializedTakeoverResult");
}

TakeoverData TakeoverData::deserializeThriftMounts(
    std::vector<SerializedMountInfo>& serializedMounts) {
  TakeoverData data;
  for (auto& serializedMount : serializedMounts) {
    switch (*serializedMount.mountProtocol()) {
      case TakeoverMountProtocol::UNKNOWN:
        throw std::runtime_error("Unknown Mount Protocol");
      case TakeoverMountProtocol::FUSE: {
        if (serializedMount.connInfo()->size() != sizeof(fuse_init_out)) {
          throwf<std::runtime_error>(
              "invalid FUSE connection info size {} for mount {}, expected {}",
              serializedMount.connInfo()->size(),
              *serializedMount.mountPath(),
              sizeof(fuse_init_out));
        }
        fuse_init_out connInfo{};
        std::memcpy(
            &connInfo, serializedMount.connInfo()->data(), sizeof(connInfo));
        data.mountPoints.emplace_back(
            canonicalPath(*serializedMount.mountPath()),
            canonicalPath(*serializedMount.stateDirectory()),
            FuseChannelData{folly::File{}, connInfo},
            std::move(*serializedMount.inodeMap()));
        break;
      }
      case TakeoverMountProtocol::NFS:
        data.mountPoints.emplace_back(
            canonicalPath(*serializedMount.mountPath()),
            canonicalPath(*serializedMount.stateDirectory()),
            NfsChannelData{folly::File{}},
            std::move(*serializedMount.inodeMap()));
        break;
      default:
        throw std::runtime_error(
            "impossible enum variant for TakeoverMountProtocol");
    }
  }
  return data;
}
#endif

} // namespace facebook::eden
