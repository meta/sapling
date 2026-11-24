/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/store/filter/FilteredObjectId.h"

#include <folly/Conv.h>
#include <folly/Varint.h>
#include <folly/logging/xlog.h>

#include "eden/common/utils/EnumValue.h"
#include "eden/common/utils/Throw.h"
#include "eden/fs/store/BackingStore.h"

using folly::ByteRange;
using folly::Endian;
using folly::StringPiece;
using std::string;

namespace facebook::eden {

std::string foidTypeToString(FilteredObjectIdType foidType) {
  switch (foidType) {
    case FilteredObjectIdType::OBJECT_TYPE_BLOB:
      return "blob";
    case FilteredObjectIdType::OBJECT_TYPE_TREE:
      return "tree";
    case FilteredObjectIdType::OBJECT_TYPE_UNFILTERED_TREE:
      return "unfiltered_tree";
  }
  XLOGF(FATAL, "Invalid FilteredObjectIdType: {}", enumValue(foidType));
}

bool FilteredObjectId::isFilteredObjectIdString(folly::StringPiece objectId) {
  auto colonPos = objectId.find(':');
  if (colonPos == folly::StringPiece::npos) {
    return false;
  }
  auto prefix = objectId.subpiece(0, colonPos);
  auto parsed = folly::tryTo<uint8_t>(prefix);
  if (parsed.hasError()) {
    return false;
  }
  auto val = parsed.value();
  return val == FilteredObjectIdType::OBJECT_TYPE_BLOB ||
      val == FilteredObjectIdType::OBJECT_TYPE_TREE ||
      val == FilteredObjectIdType::OBJECT_TYPE_UNFILTERED_TREE;
}

namespace {
std::string serializeBlobOrUnfilteredTree(
    const ObjectId& object,
    FilteredObjectIdType objectType) {
  // If we're dealing with a blob or unfiltered-tree FilteredObjectId, we only
  // need to serialize two components: <type_byte><ObjectId>
  std::string buf;
  buf.reserve(1 + sizeof(object));
  auto oType = folly::to_underlying(objectType);

  buf.append(reinterpret_cast<const char*>(&oType), sizeof(objectType));
  buf.append(object.getBytes().begin(), object.getBytes().end());
  return buf;
}
} // namespace

std::string FilteredObjectId::serializeBlob(const ObjectId& object) {
  return serializeBlobOrUnfilteredTree(
      object, FilteredObjectIdType::OBJECT_TYPE_BLOB);
}

std::string FilteredObjectId::serializeUnfilteredTree(const ObjectId& object) {
  return serializeBlobOrUnfilteredTree(
      object, FilteredObjectIdType::OBJECT_TYPE_UNFILTERED_TREE);
}

std::string FilteredObjectId::serializeTree(
    RelativePathPiece path,
    std::string_view filterId,
    const ObjectId& object) {
  std::string buf;
  // We serialize trees as
  // <type_byte><varint><filter_set_id><varint><path><ObjectId>
  size_t pathLen = path.value().length();
  uint8_t pathVarint[folly::kMaxVarintLength64] = {};
  size_t pathVarintLen = folly::encodeVarint(pathLen, pathVarint);

  size_t filterLen = filterId.length();
  uint8_t filterVarint[folly::kMaxVarintLength64] = {};
  size_t filterVarintLen = folly::encodeVarint(filterLen, filterVarint);
  auto objectType = FilteredObjectIdType::OBJECT_TYPE_TREE;

  buf.reserve(
      sizeof(objectType) + pathVarintLen + pathLen + filterVarintLen +
      filterLen + sizeof(object));
  buf.append(reinterpret_cast<const char*>(&objectType), sizeof(objectType));
  buf.append(reinterpret_cast<const char*>(filterVarint), filterVarintLen);
  buf.append(filterId);
  buf.append(reinterpret_cast<const char*>(pathVarint), pathVarintLen);
  buf.append(path.value().begin(), path.value().end());
  buf.append(object.asString());
  return buf;
}

RelativePathPiece FilteredObjectId::path() const {
  if (value_.front() != FilteredObjectIdType::OBJECT_TYPE_TREE) {
    throwf<std::invalid_argument>(
        "Cannot determine path of non-tree FilteredObjectId: {}", value_);
  }

  // Skip the first byte of data that contains the type
  folly::Range r(value_.data(), value_.size());
  r.advance(sizeof(FilteredObjectIdType::OBJECT_TYPE_TREE));

  // Skip the variable length filter id. decodeVarint() advances the
  // range for us, so we don't need to skip the VarInt after reading it.
  size_t varintSize = folly::decodeVarint(r);
  r.advance(varintSize);
  varintSize = folly::decodeVarint(r);

  StringPiece data{r.begin(), varintSize};
  // value_ was built with a known good RelativePath, thus we don't need
  // to recheck it when deserializing.
  return RelativePathPiece{data, detail::SkipPathSanityCheck{}};
}

StringPiece FilteredObjectId::filter() const {
  if (value_.front() != FilteredObjectIdType::OBJECT_TYPE_TREE) {
    // We don't know the filter of non-tree objects. Throw.
    throwf<std::invalid_argument>(
        "Cannot determine filter for non-tree FilteredObjectId: {}", value_);
  }

  // Skip the first byte of data that contains the type
  folly::Range r(value_.data(), value_.size());
  r.advance(sizeof(FilteredObjectIdType::OBJECT_TYPE_TREE));

  // Determine the location/size of the filter
  size_t varintSize = folly::decodeVarint(r);

  // decodeVarint advances the range for us, so we can use the current
  // start of the range.
  StringPiece data{r.begin(), varintSize};
  return data;
}

ObjectId FilteredObjectId::object() const {
  switch (value_.front()) {
    case FilteredObjectIdType::OBJECT_TYPE_TREE: {
      // Skip the first byte of data that contains the type
      folly::Range r(value_.data(), value_.size());
      r.advance(sizeof(FilteredObjectIdType::OBJECT_TYPE_TREE));

      // Determine the location/size of the filter and skip it
      size_t varintSize = folly::decodeVarint(r);
      r.advance(varintSize);

      // Determine the location/size of the path and skip it
      varintSize = folly::decodeVarint(r);
      r.advance(varintSize);

      // Parse the ObjectId bytes and use them to create an ObjectId
      ObjectId object = ObjectId{r};
      return object;
    }

    case FilteredObjectIdType::OBJECT_TYPE_UNFILTERED_TREE:
      static_assert(
          sizeof(FilteredObjectIdType::OBJECT_TYPE_UNFILTERED_TREE) ==
          sizeof(FilteredObjectIdType::OBJECT_TYPE_BLOB));
      [[fallthrough]];
    case FilteredObjectIdType::OBJECT_TYPE_BLOB: {
      folly::Range r(value_.data(), value_.size());
      r.advance(sizeof(FilteredObjectIdType::OBJECT_TYPE_BLOB));
      ObjectId object = ObjectId{r};
      return object;
    }
  }
  // Unknown FilteredObjectId type. Throw.
  throwf<std::runtime_error>(
      "Unknown FilteredObjectId type: {}", value_.data()[0]);
}

// Since some FilteredObjectIds are created without validation, we should
// validate that we return a valid type.
FilteredObjectIdType FilteredObjectId::objectType() const {
  switch (value_.front()) {
    case FilteredObjectIdType::OBJECT_TYPE_TREE:
      return FilteredObjectIdType::OBJECT_TYPE_TREE;
    case FilteredObjectIdType::OBJECT_TYPE_BLOB:
      return FilteredObjectIdType::OBJECT_TYPE_BLOB;
    case FilteredObjectIdType::OBJECT_TYPE_UNFILTERED_TREE:
      return FilteredObjectIdType::OBJECT_TYPE_UNFILTERED_TREE;
  }
  // Unknown FilteredObjectId type. Throw.
  throwf<std::runtime_error>(
      "Unknown FilteredObjectId type: {}", value_.front());
}

// It's possible that FilteredObjectIds with different filterIds evaluate to
// the same underlying object. However, that's not for the FilteredObjectId
// implementation to decide. This implementation strictly checks if the FOID
// contents are byte-wise equal.
bool FilteredObjectId::operator==(const FilteredObjectId& otherId) const {
  return value_ == otherId.value_;
}

// The comment above for == also applies here.
bool FilteredObjectId::operator<(const FilteredObjectId& otherId) const {
  return value_ < otherId.value_;
}

void FilteredObjectId::validate() {
  ByteRange infoBytes = folly::Range{value_.data(), value_.size()};
  XLOG(DBG9, value_);

  if (UNLIKELY(infoBytes.empty())) {
    XLOG(ERR, "Invalid empty FilteredObjectId");
    throw std::invalid_argument("Invalid empty FilteredObjectId");
  }

  // Ensure the type byte is valid
  auto typeByte = infoBytes.front();
  if (typeByte != FilteredObjectIdType::OBJECT_TYPE_BLOB &&
      typeByte != FilteredObjectIdType::OBJECT_TYPE_TREE &&
      typeByte != FilteredObjectIdType::OBJECT_TYPE_UNFILTERED_TREE) {
    auto msg = fmt::format(
        "Invalid FilteredObjectId type byte {}. Value_ = {}", typeByte, value_);
    XLOG(ERR, msg);
    throw std::invalid_argument(msg);
  }
  static_assert(
      sizeof(FilteredObjectIdType::OBJECT_TYPE_UNFILTERED_TREE) ==
          sizeof(FilteredObjectIdType::OBJECT_TYPE_BLOB) &&
      sizeof(FilteredObjectIdType::OBJECT_TYPE_BLOB) ==
          sizeof(FilteredObjectIdType::OBJECT_TYPE_TREE));
  infoBytes.advance(sizeof(FilteredObjectIdType::OBJECT_TYPE_TREE));

  // Validating the wrapped ObjectId is impossible since we don't know what
  // it should contain. Therefore, we simply return if we're validating a
  // filtered blob Id.
  if (typeByte == FilteredObjectIdType::OBJECT_TYPE_BLOB ||
      typeByte == FilteredObjectIdType::OBJECT_TYPE_UNFILTERED_TREE) {
    return;
  }

  // For trees, we can actually perform some validation. We can ensure the
  // varints describing the filterid and path are valid
  auto expectedSize = folly::tryDecodeVarint(infoBytes);
  if (UNLIKELY(!expectedSize)) {
    auto msg = fmt::format(
        "failed to decode filter id VarInt when validating FilteredObjectId {}: {}",
        value_,
        fmt::underlying(expectedSize.error()));
    throw std::invalid_argument(msg);
  }
  infoBytes.advance(*expectedSize);

  expectedSize = folly::tryDecodeVarint(infoBytes);
  if (UNLIKELY(!expectedSize)) {
    auto msg = fmt::format(
        "failed to decode path length VarInt when validating FilteredObjectId {}: {}",
        value_,
        fmt::underlying(expectedSize.error()));
    throw std::invalid_argument(msg);
  }
  // Reject an overlong path length, as for the filter above.
  infoBytes.advance(*expectedSize);
}

std::string FilteredObjectId::renderFilteredObjectId(
    const FilteredObjectId& object,
    std::string underlyingObjectString) {
  // Render the type as an integer (currently ranges from 16 - 18)
  auto foidType = folly::to_underlying(object.objectType());
  auto typeString = folly::to<std::string>(foidType);

  // Blobs and unfiltered tree ids have no filter or path information
  if (foidType == FilteredObjectIdType::OBJECT_TYPE_BLOB ||
      foidType == FilteredObjectIdType::OBJECT_TYPE_UNFILTERED_TREE) {
    return fmt::format("{}:{}", typeString, std::move(underlyingObjectString));
  }

  // Trees have filter and path information. We need to render these as well.
  // As mentioned in the method docs, we need to provide lengths for each
  // component of the ID so that the parse method can correctly parse the
  // rendered IDs back into the original FilteredObjectId.
  auto objectPath = object.path().asString();
  auto renderedId = fmt::format(
      "{}:{}:{}{}:{}{}",
      typeString,
      object.filter().size(),
      object.filter().str(),
      objectPath.size(),
      objectPath,
      std::move(underlyingObjectString));
  XLOGF(DBG8, "Rendered FilteredObjectId: {}", renderedId);
  return renderedId;
}

FilteredObjectId FilteredObjectId::parseFilteredObjectId(
    std::string_view object,
    std::shared_ptr<BackingStore> underlyingBackingStore) {
  // Parse the foid type and convert it to an int. This also asserts that the
  // rendered object we're parsing
  auto foidTypeEndIdx = object.find(':');
  if (foidTypeEndIdx == string::npos) {
    throwf<std::invalid_argument>(
        "Cannot parse invalid FilteredObjectId: {}", object);
  }
  auto typeInt = folly::to<decltype(FilteredObjectIdType::OBJECT_TYPE_BLOB)>(
      object.substr(0, foidTypeEndIdx));
  auto foidType = static_cast<FilteredObjectIdType>(typeInt);

  if (foidType == FilteredObjectIdType::OBJECT_TYPE_BLOB ||
      foidType == FilteredObjectIdType::OBJECT_TYPE_UNFILTERED_TREE) {
    // Blobs and unfiltered tree ids have no filter or path information.
    // The remainder of the string is the underlying object id.
    auto underlyingObjectStartIdx = foidTypeEndIdx + 1;
    return FilteredObjectId(
        underlyingBackingStore->parseObjectId(
            object.substr(underlyingObjectStartIdx)),
        foidType);
  }

  // Guards against future additions to FilteredObjectIdType
  XDCHECK_EQ(foidType, FilteredObjectIdType::OBJECT_TYPE_TREE);

  // Tree objects have filter and path information we must extract. We first
  // extract the filter length from the string.
  auto filterLenStartIdx = foidTypeEndIdx + 1;
  auto filterLenEndIdx = object.find(':', filterLenStartIdx);
  if (filterLenEndIdx == string::npos) {
    throwf<std::invalid_argument>(
        "Cannot parse invalid FilteredObjectId: {}", object);
  }
  auto filterLength = folly::to<size_t>(
      object.substr(filterLenStartIdx, filterLenEndIdx - filterLenStartIdx));

  // We can then extract the filter itself using the filter length info. The
  // length is compared with the remaining bytes before it is added to an
  // index, so a length near SIZE_MAX cannot wrap the end index back into the
  // string.
  auto filterStartIdx = filterLenEndIdx + 1;
  if (filterLength > object.size() - filterStartIdx) {
    throwf<std::invalid_argument>(
        "Cannot parse invalid FilteredObjectId: {}", object);
  }
  auto filter = object.substr(filterStartIdx, filterLength);
  auto filterEndIdx /* also pathLenStartIdx */ = filterStartIdx + filterLength;

  // We now have enough info to determine the path length and extract it.
  auto pathLenEndIdx = object.find(':', filterEndIdx);
  if (pathLenEndIdx == string::npos) {
    throwf<std::invalid_argument>(
        "Cannot parse invalid FilteredObjectId: {}", object);
  }
  auto pathLength = folly::to<size_t>(
      object.substr(filterEndIdx, pathLenEndIdx - filterEndIdx));

  // We now can extract the path itself
  auto pathStartIdx = pathLenEndIdx + 1;
  if (pathLength > object.size() - pathStartIdx) {
    throwf<std::invalid_argument>(
        "Cannot parse invalid FilteredObjectId: {}", object);
  }
  auto path = RelativePath{object.substr(pathStartIdx, pathLength)};
  auto pathEndIdx = pathStartIdx + pathLength;

  // Render the underlying object using the underlyingBackingStore
  auto underlyingObject =
      underlyingBackingStore->parseObjectId(object.substr(pathEndIdx));

  return FilteredObjectId(path, filter, underlyingObject);
}

} // namespace facebook::eden
