/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/model/EmbeddedMetadata.h"

#include <limits>
#include <string>
#include <utility>

#include <gtest/gtest.h>

using namespace facebook::eden;
using folly::ByteRange;
using folly::StringPiece;

namespace {

constexpr uint8_t kType = 0x05;

ByteRange bytesOf(StringPiece string) {
  return ByteRange{string};
}

ObjectId idOf(const std::string& bytes) {
  return ObjectId{folly::fbstring{bytes}};
}

EmbeddedMetadata allFields() {
  EmbeddedMetadata metadata;
  metadata.size = 0x0102030405060708;
  metadata.sha1 = Hash20::sha1(bytesOf("sha1"));
  metadata.blake3 = Hash32::blake3(bytesOf("blake3"));
  metadata.mtime = timespec{1700000000, 123456789};
  return metadata;
}

// timespec has no operator==.
std::optional<std::pair<int64_t, int64_t>> parts(
    const std::optional<timespec>& ts) {
  if (!ts) {
    return std::nullopt;
  }
  return std::pair<int64_t, int64_t>{ts->tv_sec, ts->tv_nsec};
}

void expectReadsBack(const EmbeddedMetadata& metadata) {
  const auto id = makeObjectId(kType, metadata, bytesOf("store bytes"));
  const EmbeddedMetadataView view{id};
  EXPECT_EQ(kType, view.type());
  EXPECT_EQ(metadata.size, view.size());
  EXPECT_EQ(metadata.sha1, view.sha1());
  EXPECT_EQ(metadata.blake3, view.blake3());
  EXPECT_EQ(parts(metadata.mtime), parts(view.mtime()));
  EXPECT_EQ(bytesOf("store bytes"), view.storeBytes());
}

void expectNoMetadata(const ObjectId& id) {
  const EmbeddedMetadataView view{id};
  EXPECT_FALSE(view.size().has_value());
  EXPECT_FALSE(view.sha1().has_value());
  EXPECT_FALSE(view.blake3().has_value());
  EXPECT_FALSE(view.mtime().has_value());
}

} // namespace

TEST(EmbeddedMetadata, layout) {
  // Ids are stored, so their bytes must not change.
  EmbeddedMetadata metadata;
  metadata.size = 0x0102;
  metadata.mtime = timespec{1, 2};

  const auto id = makeObjectId(kType, metadata, bytesOf("rest"));

  // The type with the metadata bit.
  std::string expected = "\x45";
  // The length: one byte of flags and two fields of 8 bytes.
  expected.push_back(17);
  // The flags: size and mtime.
  expected.push_back(0x09);
  expected.append("\x02\x01\x00\x00\x00\x00\x00\x00", 8);
  // 1'000'000'002 nanoseconds.
  expected.append("\x02\xca\x9a\x3b\x00\x00\x00\x00", 8);
  expected.append("rest");
  EXPECT_EQ(expected, id.asString());
}

TEST(EmbeddedMetadata, layoutWithEveryField) {
  EmbeddedMetadata metadata;
  metadata.size = 0x0102030405060708;
  metadata.sha1 = Hash20{"000102030405060708090a0b0c0d0e0f10111213"};
  metadata.blake3 = Hash32{
      "202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f"};
  // Half a second before the epoch: -500'000'000 nanoseconds.
  metadata.mtime = timespec{-1, 500000000};

  const auto id = makeObjectId(kType, metadata, bytesOf("rest"));

  std::string expected = "\x45";
  // The length: one byte of flags, then 8 + 20 + 32 + 8 bytes of fields.
  expected.push_back(69);
  expected.push_back(0x0f);
  expected.append("\x08\x07\x06\x05\x04\x03\x02\x01", 8);
  for (char byte = 0x00; byte <= 0x13; ++byte) {
    expected.push_back(byte);
  }
  for (char byte = 0x20; byte <= 0x3f; ++byte) {
    expected.push_back(byte);
  }
  expected.append("\x00\x9b\x32\xe2\xff\xff\xff\xff", 8);
  expected.append("rest");
  EXPECT_EQ(expected, id.asString());
}

TEST(EmbeddedMetadata, idWithoutMetadataHasNoBlock) {
  const auto id = makeObjectId(kType, EmbeddedMetadata{}, bytesOf("rest"));

  EXPECT_EQ("\x05rest", id.asString());
  expectNoMetadata(id);
  EXPECT_EQ(bytesOf("rest"), EmbeddedMetadataView{id}.storeBytes());
}

TEST(EmbeddedMetadata, anySetOfFieldsReadsBack) {
  const auto all = allFields();
  for (int fields = 0; fields < 16; ++fields) {
    SCOPED_TRACE(fields);
    EmbeddedMetadata metadata;
    if (fields & 1) {
      metadata.size = all.size;
    }
    if (fields & 2) {
      metadata.sha1 = all.sha1;
    }
    if (fields & 4) {
      metadata.blake3 = all.blake3;
    }
    if (fields & 8) {
      metadata.mtime = all.mtime;
    }
    expectReadsBack(metadata);
  }
}

TEST(EmbeddedMetadata, mtimeAtTheEndsOfTheRange) {
  EmbeddedMetadata metadata;
  // Half a second before the epoch.
  metadata.mtime = timespec{-1, 500000000};
  expectReadsBack(metadata);

  // The first and the last second that fit.
  metadata.mtime = timespec{-9223372036, 0};
  expectReadsBack(metadata);
  metadata.mtime = timespec{9223372035, 999999999};
  expectReadsBack(metadata);

  // One second further, or much further, reads back as the earliest or the
  // latest time that fits: the smallest and the largest int64_t in
  // nanoseconds.
  const std::pair<int64_t, int64_t> earliest{-9223372037, 145224192};
  const std::pair<int64_t, int64_t> latest{9223372036, 854775807};
  const std::pair<int64_t, std::pair<int64_t, int64_t>> outOfRange[] = {
      {-9223372037, earliest},
      {std::numeric_limits<int64_t>::min() / 1000, earliest},
      {9223372036, latest},
      {std::numeric_limits<int64_t>::max() / 1000, latest},
  };
  for (const auto& [seconds, expected] : outOfRange) {
    timespec ts{};
    ts.tv_sec = seconds;
    metadata.mtime = ts;
    const auto id = makeObjectId(kType, metadata, ByteRange{});
    EXPECT_EQ(expected, parts(EmbeddedMetadataView{id}.mtime())) << seconds;
  }
}

TEST(EmbeddedMetadata, idsWithoutABlockCarryNothing) {
  expectNoMetadata(ObjectId{});
  EXPECT_FALSE(EmbeddedMetadataView{ObjectId{}}.type().has_value());
  EXPECT_TRUE(EmbeddedMetadataView{ObjectId{}}.storeBytes().empty());

  const auto withBlock =
      makeObjectId(kType, allFields(), bytesOf("rest")).asString();
  // The same bytes with the reserved bit set, and without the metadata bit.
  auto reserved = withBlock;
  reserved[0] = static_cast<char>(0x80 | 0x40 | kType);
  auto plain = withBlock;
  plain[0] = static_cast<char>(kType);

  for (const auto& id : {idOf(reserved), idOf(plain)}) {
    expectNoMetadata(id);
    // Everything after the prefix byte is the store's.
    EXPECT_EQ(id.getBytes().subpiece(1), EmbeddedMetadataView{id}.storeBytes());
  }
  EXPECT_FALSE(EmbeddedMetadataView{idOf(reserved)}.type().has_value());
  EXPECT_EQ(kType, EmbeddedMetadataView{idOf(plain)}.type());
}

TEST(EmbeddedMetadata, malformedBlockCarriesNothing) {
  // The length says 9 bytes follow, and there is 1.
  expectNoMetadata(idOf(std::string{"\x45\x09\x01", 3}));
  // The flags promise a SHA-1, and the block has room for the flags only.
  expectNoMetadata(idOf(std::string{"\x45\x01\x02rest", 7}));
  // The length is cut off: its only byte says another one follows.
  expectNoMetadata(idOf(std::string{"\x45\x80", 2}));
}

TEST(EmbeddedMetadata, fieldsThisVersionDoesNotKnowAreSkipped) {
  // A size, then 3 bytes of a field whose flag (0x80) this version does not
  // know. That flag makes the flags a varint of two bytes.
  std::string bytes = "\x45";
  bytes.push_back(2 + 8 + 3);
  bytes.append("\x81\x01", 2);
  bytes.append("\x07\x00\x00\x00\x00\x00\x00\x00", 8);
  bytes.append("???");
  bytes.append("rest");
  const auto id = idOf(bytes);

  const EmbeddedMetadataView view{id};
  EXPECT_EQ(7, view.size());
  EXPECT_FALSE(view.mtime().has_value());
  EXPECT_EQ(bytesOf("rest"), view.storeBytes());
}
