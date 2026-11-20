/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/model/EmbeddedMetadata.h"

#include <cstring>
#include <limits>

#include <folly/Varint.h>
#include <folly/lang/Bits.h>
#include <folly/logging/xlog.h>

namespace facebook::eden {

namespace {

constexpr uint8_t kReservedBit = 0x80;
constexpr uint8_t kMetadataBit = 0x40;
constexpr uint8_t kTypeMask = 0x3f;

// The fields of a metadata block, in the order they are stored. These values
// are part of the id format.
enum Flag : uint64_t {
  kHasSize = 1 << 0, // 8 bytes
  kHasSha1 = 1 << 1, // 20 bytes
  kHasBlake3 = 1 << 2, // 32 bytes
  kHasMtime = 1 << 3, // 8 bytes
};

/**
 * The bytes taken by the fields that `flags` names and this code knows.
 */
constexpr size_t fieldsLength(uint64_t flags) {
  return (flags & kHasSize ? sizeof(uint64_t) : 0) +
      (flags & kHasSha1 ? Hash20::RAW_SIZE : 0) +
      (flags & kHasBlake3 ? Hash32::RAW_SIZE : 0) +
      (flags & kHasMtime ? sizeof(int64_t) : 0);
}

constexpr int64_t kNsPerSec = 1'000'000'000;

int64_t toNanoseconds(timespec ts) noexcept {
  XDCHECK(ts.tv_nsec >= 0 && ts.tv_nsec < kNsPerSec);
  constexpr int64_t kMaxSec = std::numeric_limits<int64_t>::max() / kNsPerSec;
  if (ts.tv_sec >= kMaxSec) {
    return std::numeric_limits<int64_t>::max();
  }
  if (ts.tv_sec < -kMaxSec) {
    return std::numeric_limits<int64_t>::min();
  }
  return int64_t{ts.tv_sec} * kNsPerSec + ts.tv_nsec;
}

timespec fromNanoseconds(int64_t ns) noexcept {
  int64_t sec = ns / kNsPerSec;
  int64_t nsec = ns % kNsPerSec;
  if (nsec < 0) {
    // tv_nsec counts forward from tv_sec.
    nsec += kNsPerSec;
    --sec;
  }
  timespec ts{};
  ts.tv_sec = sec;
  ts.tv_nsec = nsec;
  return ts;
}

uint64_t readInt(const uint8_t* field) noexcept {
  uint64_t value;
  memcpy(&value, field, sizeof(value));
  return folly::Endian::little(value);
}

void append(folly::fbstring& out, const void* data, size_t size) {
  out.append(static_cast<const char*>(data), size);
}

void appendInt(folly::fbstring& out, uint64_t value) {
  const auto little = folly::Endian::little(value);
  append(out, &little, sizeof(little));
}

} // namespace

ObjectId makeObjectId(
    uint8_t type,
    const EmbeddedMetadata& metadata,
    folly::ByteRange storeBytes) {
  XCHECK_LT(type, kMetadataBit);
  const uint64_t flags = (metadata.size ? kHasSize : 0) |
      (metadata.sha1 ? kHasSha1 : 0) | (metadata.blake3 ? kHasBlake3 : 0) |
      (metadata.mtime ? kHasMtime : 0);

  folly::fbstring bytes;
  if (flags == 0) {
    bytes.reserve(1 + storeBytes.size());
    bytes.push_back(static_cast<char>(type));
  } else {
    uint8_t flagsVarint[folly::kMaxVarintLength64];
    const size_t flagsSize = folly::encodeVarint(flags, flagsVarint);
    const size_t length = flagsSize + fieldsLength(flags);
    uint8_t lengthVarint[folly::kMaxVarintLength64];
    const size_t lengthSize = folly::encodeVarint(length, lengthVarint);

    bytes.reserve(1 + lengthSize + length + storeBytes.size());
    bytes.push_back(static_cast<char>(type | kMetadataBit));
    append(bytes, lengthVarint, lengthSize);
    append(bytes, flagsVarint, flagsSize);
    if (metadata.size) {
      appendInt(bytes, *metadata.size);
    }
    if (metadata.sha1) {
      append(bytes, metadata.sha1->getBytes().data(), Hash20::RAW_SIZE);
    }
    if (metadata.blake3) {
      append(bytes, metadata.blake3->getBytes().data(), Hash32::RAW_SIZE);
    }
    if (metadata.mtime) {
      appendInt(bytes, static_cast<uint64_t>(toNanoseconds(*metadata.mtime)));
    }
  }
  append(bytes, storeBytes.data(), storeBytes.size());
  return ObjectId{std::move(bytes)};
}

EmbeddedMetadataView::EmbeddedMetadataView(const ObjectId& id) noexcept {
  auto bytes = id.getBytes();
  if (bytes.empty()) {
    return;
  }
  const uint8_t prefix = bytes.front();
  bytes.advance(1);
  storeBytes_ = bytes;
  if (prefix & kReservedBit) {
    return;
  }
  type_ = prefix & kTypeMask;
  if (!(prefix & kMetadataBit)) {
    return;
  }

  const auto length = folly::tryDecodeVarint(bytes);
  if (!length || *length > bytes.size()) {
    return;
  }
  auto block = bytes.subpiece(0, *length);
  const auto flags = folly::tryDecodeVarint(block);
  if (!flags || fieldsLength(*flags) > block.size()) {
    return;
  }
  flags_ = *flags;
  fields_ = block.data();
  storeBytes_ = bytes.subpiece(*length);
}

const uint8_t* FOLLY_NULLABLE
EmbeddedMetadataView::field(uint64_t flag) const noexcept {
  if (!(flags_ & flag)) {
    return nullptr;
  }
  // The fields with a lower flag are stored before it.
  return fields_ + fieldsLength(flags_ & (flag - 1));
}

std::optional<uint64_t> EmbeddedMetadataView::size() const noexcept {
  const auto* size = field(kHasSize);
  return size ? std::optional{readInt(size)} : std::nullopt;
}

std::optional<Hash20> EmbeddedMetadataView::sha1() const noexcept {
  const auto* sha1 = field(kHasSha1);
  return sha1 ? std::optional{Hash20{folly::ByteRange{sha1, Hash20::RAW_SIZE}}}
              : std::nullopt;
}

std::optional<Hash32> EmbeddedMetadataView::blake3() const noexcept {
  const auto* blake3 = field(kHasBlake3);
  return blake3
      ? std::optional{Hash32{folly::ByteRange{blake3, Hash32::RAW_SIZE}}}
      : std::nullopt;
}

std::optional<timespec> EmbeddedMetadataView::mtime() const noexcept {
  const auto* mtime = field(kHasMtime);
  return mtime
      ? std::optional{fromNanoseconds(static_cast<int64_t>(readInt(mtime)))}
      : std::nullopt;
}

} // namespace facebook::eden
