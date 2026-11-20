/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/store/sl/SaplingObjectId.h"

#include <fmt/core.h>
#include <folly/logging/xlog.h>

#include "eden/common/utils/Throw.h"

using folly::ByteRange;
using folly::Endian;
using folly::StringPiece;
using std::string;

namespace facebook::eden {

// SlOid prefix length (type byte and 20 byte node)
constexpr size_t slOidLenSansPath = Hash20::RAW_SIZE + 1;

SaplingObjectId::SaplingObjectId(const Hash20& slHash, RelativePathPiece path) {
  value_.reserve(slOidLenSansPath + path.view().size());
  value_.push_back(TYPE_HG_ID_WITH_PATH);
  value_.append((const char*)slHash.getBytes().data(), slHash.RAW_SIZE);
  value_.append(path.view());
}

SaplingObjectId::SaplingObjectId(
    const Hash20& slHash,
    RelativePathPiece dir,
    PathComponentPiece name) {
  value_.reserve(slOidLenSansPath + dir.view().size() + 1 + name.view().size());
  value_.push_back(TYPE_HG_ID_WITH_PATH);
  value_.append((const char*)slHash.getBytes().data(), slHash.RAW_SIZE);
  if (!dir.empty()) {
    value_.append(dir.view());
    value_.push_back(kDirSeparator);
  }
  value_.append(name.view());
}

SaplingObjectId::SaplingObjectId(const Hash20& slHash) {
  value_.reserve(slOidLenSansPath);
  value_.push_back(TYPE_HG_ID_NO_PATH);
  value_.append((const char*)slHash.getBytes().data(), slHash.RAW_SIZE);
}

SaplingObjectId::SaplingObjectId(const SaplingObjectIdView& view)
    : value_(
          reinterpret_cast<const char*>(view.data().data()),
          view.data().size()) {}

SaplingObjectId::SaplingObjectId(folly::StringPiece value, bool validate)
    : value_{value} {
  if (validate) {
    validateSlOid(value_);
  } else {
    XDCHECK((validateSlOid(value_), true));
  }
}

SaplingObjectId::SaplingObjectId(const ObjectId& oid) : value_{oid.getBytes()} {
  validateSlOid(value_);
}

ObjectId SaplingObjectId::oid() && {
  return ObjectId{std::move(value_)};
}

bool SaplingObjectId::hasValidType(const ObjectId& oid) {
  folly::ByteRange bytes = oid.getBytes();
  // A type byte, then the 20 byte node.
  return bytes.size() >= 21 &&
      (bytes[0] == TYPE_HG_ID_WITH_PATH || bytes[0] == TYPE_HG_ID_NO_PATH);
}

RelativePathPiece SaplingObjectId::path() const noexcept {
  XDCHECK((validateSlOid(value_), true));
  if (value_.empty() || value_[0] == TYPE_HG_ID_NO_PATH) {
    return RelativePathPiece{};
  } else {
    // value_ was built with a known good RelativePath, or validated on
    // construction. We can skip the sanity check here.
    return RelativePathPiece{
        std::string_view{value_}.substr(slOidLenSansPath),
        detail::SkipPathSanityCheck{}};
  }
}

const Hash20& SaplingObjectId::node() const noexcept {
  XDCHECK((validateSlOid(value_), true));
  if (value_.empty()) {
    return kZeroHash;
  } else {
    return *reinterpret_cast<const Hash20*>(value_.data() + 1);
  }
}

SaplingObjectIdView SaplingObjectId::view() const {
  return SaplingObjectIdView{data()};
}

bool SaplingObjectId::operator==(const SaplingObjectId& otherHash) const {
  return value_ == otherHash.value_;
}

bool SaplingObjectId::operator<(const SaplingObjectId& otherHash) const {
  return value_ < otherHash.value_;
}

void validateSlOid(folly::StringPiece value) {
  if (value.empty()) {
    // Special case - empty value is okay.
    return;
  }

  auto type = value[0];
  switch (type) {
    case SaplingObjectId::TYPE_HG_ID_WITH_PATH:
      if (value.size() < slOidLenSansPath) {
        throwf<std::invalid_argument>(
            "Invalid SaplingObjectId size for TYPE_HG_ID_WITH_PATH: size {}",
            value.size());
      }
      // Validate the path.
      (void)RelativePathPiece{std::string_view{value}.substr(slOidLenSansPath)};
      break;
    case SaplingObjectId::TYPE_HG_ID_NO_PATH:
      if (value.size() != slOidLenSansPath) {
        throwf<std::invalid_argument>(
            "Invalid SaplingObjectId size for TYPE_HG_ID_NO_PATH: size {}",
            value.size());
      }
      break;
    default:
      throwf<std::invalid_argument>(
          "Unknown SaplingObjectId type: size {}, type {}", value.size(), type);
  }
}

SaplingObjectIdView::SaplingObjectIdView(const ObjectId& oid)
    : value_{oid.getBytes()} {
  validateSlOid(folly::StringPiece(value_));
}

SaplingObjectIdView::SaplingObjectIdView(const SaplingObjectId& slOid)
    : value_{slOid.data()} {}

SaplingObjectIdView::SaplingObjectIdView(folly::ByteRange data) : value_{data} {
  validateSlOid(folly::StringPiece(value_));
}

RelativePathPiece SaplingObjectIdView::path() const noexcept {
  if (value_.empty() || value_[0] == SaplingObjectId::TYPE_HG_ID_NO_PATH) {
    return RelativePathPiece{};
  } else {
    // value_ was validated on construction. We can skip the sanity check here.
    return RelativePathPiece{
        folly::StringPiece(value_).subpiece(21), detail::SkipPathSanityCheck{}};
  }
}

const Hash20& SaplingObjectIdView::node() const noexcept {
  if (value_.empty()) {
    return kZeroHash;
  } else {
    return *reinterpret_cast<const Hash20*>(value_.data() + 1);
  }
}

} // namespace facebook::eden
