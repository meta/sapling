/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <fmt/core.h>
#include <folly/FBString.h>
#include <folly/Range.h>
#include <array>
#include <cstdint>
#include <iosfwd>

namespace folly {
class IOBuf;
}

namespace facebook::eden {

/**
 * Identifies tree and blob objects.
 * This identifier is a variable length string.
 *
 * It starts with a byte that holds a type, and can carry metadata about the
 * object after it. See EmbeddedMetadata.h.
 */
class ObjectId {
 public:
  // fbstring has more SSO space (23 bytes!) than std::string and thus can hold
  // 20-byte hashes inline.
  using Storage = folly::fbstring;

  /**
   * Create an empty object id
   */
  ObjectId() noexcept : bytes_{} {}

  explicit ObjectId(Storage fbs) noexcept : bytes_{std::move(fbs)} {}

  explicit ObjectId(folly::ByteRange bytes)
      : bytes_{constructFromByteRange(bytes)} {}

  /**
   * Compute the SHA1 hash of an IOBuf chain.
   */
  static ObjectId sha1(const folly::IOBuf& buf);

  /**
   * Compute the SHA1 hash of a std::string.
   */
  static ObjectId sha1(const std::string& str) {
    return sha1(folly::ByteRange{folly::StringPiece{str}});
  }

  /**
   * Compute the SHA1 hash of a ByteRange.
   */
  static ObjectId sha1(folly::ByteRange data);

  static ObjectId fromHex(folly::StringPiece hex) {
    return ObjectId{constructFromHex(hex)};
  }

  /**
   * Returns bytes content of the ObjectId
   */
  folly::ByteRange getBytes() const {
    return folly::ByteRange{folly::StringPiece{bytes_}};
  }

  char operator[](size_t pos) const {
    return bytes_[pos];
  }

  /**
   * Returns size of this ObjectId
   */
  size_t size() const {
    return bytes_.size();
  }

  /** @return [lowercase] hex representation of this ObjectId. */
  std::string toLogString() const {
    return asHexString();
  }

  /**
   * Returns the ObjectId with its uninterpreted bytes encoded in hexadecimal.
   * Primarily used in tests and toLogString().
   */
  std::string asHexString() const;

  /** @return bytes of this ObjectId. */
  std::string asString() const;

  /**
   * Computes a hash for this ObjectID.
   *
   * Short ObjectIDs hash to themselves as we assume the ObjectID itself has
   * high entropy. Long ObjectIDs are hashed by mixing the bits of the ID
   * together with a XOR operation. This is okay since we assume at least one
   * eight byte range in the ObjectID has high entropy and XORing with that
   * range will give us a decent hash.
   */
  size_t getHashCode() const noexcept;

  /**
   * Returns true if the two ObjectIds are equal, compared byte-by-byte. If
   * interested in whether two objects have the same contents, consider
   * ObjectStore::areObjectsKnownIdentical or BackingStore::compareObjectsById
   * instead.
   */
  bool bytesEqual(const ObjectId& that) const noexcept {
    return bytes_ == that.bytes_;
  }

  /**
   * Returns true if getBytes() < that.getBytes().
   *
   * Primarily intended for use by the std::less specialization.
   */
  bool bytesLess(const ObjectId& that) const noexcept {
    return bytes_ < that.bytes_;
  }

  /**
   * Equality comparison. Be careful. Two ObjectIds may compare different even
   * if they reference the same content. See the documentation for `bytesEqual`
   * for alternatives.
   */
  friend bool operator==(const ObjectId& lhs, const ObjectId& rhs) {
    return lhs.bytesEqual(rhs);
  }

  friend bool operator!=(const ObjectId& lhs, const ObjectId& rhs) {
    return !(lhs == rhs);
  }

 private:
  static Storage constructFromByteRange(folly::ByteRange bytes) {
    return Storage{(const char*)bytes.data(), bytes.size()};
  }
  static Storage constructFromHex(folly::StringPiece hex) {
    if (hex.size() % 2 != 0) {
      throwInvalidArgument(
          "incorrect data size for Hash constructor from string: ", hex.size());
    }
    folly::fbstring result;
    auto size = hex.size() / 2;
    result.reserve(size);
    for (size_t i = 0; i < size; i++) {
      result.push_back(hexByteAt(hex, i));
    }
    return result;
  }
  static constexpr char hexByteAt(folly::StringPiece hex, size_t index) {
    return (nibbleToHex(hex.data()[index * 2]) * 16) +
        nibbleToHex(hex.data()[(index * 2) + 1]);
  }
  static constexpr char nibbleToHex(char c) {
    if ('0' <= c && c <= '9') {
      return c - '0';
    } else if ('a' <= c && c <= 'f') {
      return 10 + c - 'a';
    } else if ('A' <= c && c <= 'F') {
      return 10 + c - 'A';
    } else {
      throwInvalidArgument(
          "invalid hex digit supplied to Hash constructor from string: ", c);
    }
  }

  [[noreturn]] static void throwInvalidArgument(
      const char* message,
      size_t number);

  Storage bytes_;
};

using ObjectIdRange = folly::Range<const ObjectId*>;

/**
 * The meaning of an ObjectId is defined by the BackingStore implementation.
 * Allow it to also define how object IDs are parsed and rendered at API
 * boundaries such as Thrift.
 */
class ObjectIdCodec {
 public:
  virtual ~ObjectIdCodec() = default;

  /**
   * Parse the string as an ObjectId.
   */
  virtual ObjectId parseObjectId(folly::StringPiece objectId) = 0;
  virtual std::string renderObjectId(const ObjectId& objectId) = 0;
};

} // namespace facebook::eden

namespace std {

template <>
struct less<facebook::eden::ObjectId> {
  bool operator()(
      const facebook::eden::ObjectId& lhs,
      const facebook::eden::ObjectId& rhs) const noexcept {
    return lhs.bytesLess(rhs);
  }
};

template <>
struct hash<facebook::eden::ObjectId> {
  size_t operator()(const facebook::eden::ObjectId& hash) const noexcept {
    return hash.getHashCode();
  }
};

} // namespace std

template <>
struct fmt::formatter<facebook::eden::ObjectId> {
  constexpr auto parse(fmt::format_parse_context& ctx) {
    return ctx.begin();
  }

  template <typename Context>
  auto format(const facebook::eden::ObjectId& id, Context& ctx) const {
    auto out = ctx.out();
    constexpr char hexValues[] = "0123456789abcdef";

    auto bytes = id.getBytes();
    for (unsigned char b : bytes) {
      *out++ = hexValues[b >> 4];
      *out++ = hexValues[b & 0x0f];
    }
    return out;
  }
};
