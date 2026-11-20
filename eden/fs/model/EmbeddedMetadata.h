/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <cstdint>
#include <ctime>
#include <optional>

#include <folly/CppAttributes.h>
#include <folly/Range.h>

#include "eden/fs/model/Hash.h"
#include "eden/fs/model/ObjectId.h"

namespace facebook::eden {

/*
 * Every ObjectId starts the same way, whichever backing store created it:
 *
 *   <prefix byte> [<metadata block>] <the store's own bytes>
 *
 * The prefix byte is `X M tttttt`:
 * - t, the low six bits, is a type that the store picks. It tells the ids of
 *   one store from those of another, and the kinds of ids of one store apart.
 *   In use: 0x01 and 0x02 (SaplingObjectId), 0x10 to 0x12 (FilteredObjectId).
 * - M (0x40) says that a metadata block follows the prefix byte.
 * - X (0x80) is reserved and written as 0. An id that has it set is read as
 *   carrying no metadata, whatever its other bits are.
 *
 * The metadata block holds what the store knows about the object besides its
 * contents, so that EdenFS has it wherever it has the id:
 *
 *   <length> <flags> <fields>
 *
 * - length: a varint, the number of bytes that the flags and the fields take.
 *   The store's own bytes start after them.
 * - flags: a varint with one bit per field that is present.
 * - fields: the fields that are present, in the order of their flags, each at
 *   a fixed width. Integers are little endian.
 *
 * A field that is added later gets the next flag and is stored after the
 * fields that exist, so a reader that does not know it skips it.
 *
 * ObjectIds are stored in the overlay and handed over on a graceful restart,
 * so none of this can change once ids are written with it.
 */

/**
 * The metadata to put in an ObjectId. A field that has no value is left out.
 */
struct EmbeddedMetadata {
  /**
   * The size in bytes of a blob's contents, or of the encoding of a tree
   * that `blake3` is the hash of.
   */
  std::optional<uint64_t> size;

  /** The SHA-1 of a blob's contents. */
  std::optional<Hash20> sha1;

  /** The BLAKE3 of a blob's contents, or of a tree's encoding. */
  std::optional<Hash32> blake3;

  /**
   * The modification time the store records for the file, symlink or
   * directory.
   *
   * Unlike the other fields this is not derived from the object's contents.
   * A store that puts it in its ids makes two files with the same contents
   * and different modification times two objects, the way a filesystem keeps
   * the time in the inode rather than in the directory entry. Its
   * compareObjectsById must not report two such ids as identical.
   *
   * The id of a directory's entry in its parent directory and the id of the
   * directory's Tree (Tree::getObjectId) have to carry the same time: either
   * one may be read for it.
   *
   * tv_nsec has to be in [0, 1e9). The time is stored as nanoseconds since
   * the Unix epoch, which covers the years 1678 to 2262: the seconds
   * -9223372036 to 9223372035. A time with other seconds is stored as the
   * earliest or the latest time that fits.
   */
  std::optional<timespec> mtime;
};

/**
 * Builds an ObjectId: the prefix byte for `type`, which has to be below 0x40,
 * the metadata block if `metadata` has any field, then `storeBytes`.
 */
ObjectId makeObjectId(
    uint8_t type,
    const EmbeddedMetadata& metadata,
    folly::ByteRange storeBytes);

/**
 * Reads the metadata that an ObjectId carries. This borrows the bytes of the
 * ObjectId, which has to outlive it.
 *
 * Reading does no I/O, does not allocate and does not fail: an id without a
 * metadata block, or with one that is not well formed, carries nothing.
 */
class EmbeddedMetadataView {
 public:
  explicit EmbeddedMetadataView(const ObjectId& id) noexcept;

  /**
   * The type in the prefix byte. An empty id has none, and neither has an id
   * with the reserved bit set, whose other bits mean nothing yet.
   */
  std::optional<uint8_t> type() const noexcept {
    return type_;
  }

  std::optional<uint64_t> size() const noexcept;
  std::optional<Hash20> sha1() const noexcept;
  std::optional<Hash32> blake3() const noexcept;
  std::optional<timespec> mtime() const noexcept;

  /**
   * What follows the prefix byte and the metadata block.
   */
  folly::ByteRange storeBytes() const noexcept {
    return storeBytes_;
  }

 private:
  /**
   * Where the field with the given flag is stored, or nullptr if the id does
   * not have it.
   */
  const uint8_t* FOLLY_NULLABLE field(uint64_t flag) const noexcept;

  std::optional<uint8_t> type_;
  uint64_t flags_{0};
  const uint8_t* fields_{nullptr};
  folly::ByteRange storeBytes_;
};

} // namespace facebook::eden
