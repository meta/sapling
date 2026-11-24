/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <string>

#include "eden/common/utils/PathFuncs.h"
#include "eden/fs/model/ObjectId.h"

namespace facebook::eden {

class BackingStore;

// FilteredObjectId types start at 0x10 so that they can be distinguished
// from SlOid types that start at 0x01 and extend until 0x02. In the
// future, this could help migrate SlOid-based ObjectIds to
// FilteredObjectIds. See comment below for more details on what objects of each
// type contain.
enum FilteredObjectIdType : uint8_t {
  // If the Object ID's type is 0x10, then it represents a blob object and is of
  // the form <blob_type_byte><ObjectId>
  OBJECT_TYPE_BLOB = 0x10,

  // If the Object ID's type is 0x11, then it represents a tree object and is of
  // the form <tree_type_byte><filter_set_id><path><ObjectId>
  OBJECT_TYPE_TREE = 0x11,

  // If the Object ID's type is 0x12, then it represents an *unfiltered* tree
  // object and is of the form <unfiltered_tree_type_byte><ObjectId>
  OBJECT_TYPE_UNFILTERED_TREE = 0x12,
};

std::string foidTypeToString(FilteredObjectIdType foidType);

/**
 * FilteredBackingStores need to keep track of a few extra pieces of state with
 * each ObjectId in order to properly filter objects across their lifetime.
 *
 * The first crucial piece of information they need is whether the given object
 * is a tree, blob, or unfiltered object. This is defined in the first byte of
 * the ObjectId (see FilteredObjectIdType above). The rest of the
 * FilteredObjectId (FOID for short) is different depending on the object's type
 * (tree, blob, or unfiltered).
 *
 * ============= Blob FOIDs =============
 *
 * By filtering trees directly, we get blob filtering for free! This is because
 * we process (and filter) the direct children of a tree whenever we process a
 * tree itself. Any filtered blobs are unreachable after their parent tree is
 * processed.
 *
 * This means Blob FOIDs don't need any extra information associated with them
 * besides the type byte mentioned above. Our Blob FOIDs are in the form:
 *
 * <foid_type_byte><ObjectId>
 *
 * The ObjectId mentioned above can be used in whatever BackingStore the
 * FilteredBackingStore is wrapped around. In most cases, this will be an
 * HgObjectID.
 *
 * ============= Tree FOIDs =============
 *
 * For trees, we need to keep track of what filter was active when the ObjectId
 * was created when the corresponding tree was fetched. This information is
 * variable length, so we use a VarInt to encode the length of the filter id.
 *
 * We also need to keep track of the path associated with the tree object so we
 * can determine whether the object needs to be filtered prior to fetching any
 * data associated with it. The path is variable length, so we use a VarInt to
 * encode the length of the path.
 *
 * Finally, like blobs, we include an ObjectId we can use in the BackingStore
 * the FilteredBackingStore wraps. ObjectIds are variable length, but we place
 * them at the end of the ObjectID. Therefore we should always know where they
 * end. This gives us the form:
 *
 * <foid_type_byte><VarInt><filter_set_id><varint><path><ObjectId>
 *
 * ========= Unfiltered Tree FOIDs =========
 *
 * To optimize the common case of not having to filter a tree or its
 * descendents, we also have a special type for unfiltered TREE objects. This
 * type is the exact same as a Blob FOID, except it has a different type byte.
 *
 * <foid_type_byte><ObjectId>
 *
 * Differentiating between partially-filtered vs recursively-unfiltered trees
 * allows us to avoid recursive descendent checks in checkout/diff when filter
 * changes occur in unrelated parts of the repository.
 */
class FilteredObjectId {
 public:
  /**
   * It doesn't make sense for a FilteredObjectId to be default constructed. At
   * a minimum, a wrapped ObjectId must be provided.
   */
  FilteredObjectId() = delete;

  /**
   * Construct a filtered blob or unfiltered tree object id.
   */
  explicit FilteredObjectId(
      const ObjectId& edenObjectId,
      FilteredObjectIdType objectType) {
    XCHECK_NE(objectType, FilteredObjectIdType::OBJECT_TYPE_TREE);
    if (objectType == FilteredObjectIdType::OBJECT_TYPE_BLOB) {
      value_ = serializeBlob(edenObjectId);
    } else {
      value_ = serializeUnfilteredTree(edenObjectId);
    }
    validate();
  }

  /**
   * Construct a filtered *tree* object id.
   */
  FilteredObjectId(
      RelativePathPiece path,
      std::string_view filterId,
      const ObjectId& edenObjectId)
      : value_{serializeTree(path, filterId, edenObjectId)} {
    validate();
  }

  /**
   * This function should only be used when the caller knows the underlying
   * bytes from the passed in ObjectId is in the form of a FilteredObjectId.
   */
  static FilteredObjectId fromObjectId(const ObjectId& id) {
    XLOGF(
        DBG9, "Constructing FilteredObjectId from ObjectId {}", id.asString());
    return FilteredObjectId{id.getBytes()};
  }

  explicit FilteredObjectId(std::string str) : value_{std::move(str)} {
    validate();
  }

  explicit FilteredObjectId(folly::ByteRange bytes)
      : value_{constructFromByteRange(bytes)} {
    validate();
  }

  ~FilteredObjectId() = default;

  FilteredObjectId(const FilteredObjectId& other) = default;
  FilteredObjectId& operator=(const FilteredObjectId& other) = default;

  FilteredObjectId(FilteredObjectId&& other) noexcept
      : value_{std::exchange(other.value_, std::string{})} {}

  FilteredObjectId& operator=(FilteredObjectId&& other) noexcept {
    value_ = std::exchange(other.value_, std::string{});
    return *this;
  }

  /*
   * Returns the path portion of the *tree* FilteredObjectId. NOTE: This
   * function will throw an exception if it is called on a Blob FOID!
   */
  RelativePathPiece path() const;

  /*
   * Returns the filter portion of the *tree* FilteredObjectId. NOTE: This
   * function will throw an exception if it is called on a Blob FOID!
   */
  folly::StringPiece filter() const;

  /*
   * Returns the object portion of the FilteredObjectId. NOTE: This function
   * works for BOTH Blob and Tree FOIDs.
   */
  ObjectId object() const;

  /*
   * Returns the type of the FilteredObjectId. NOTE: This function works for
   * BOTH Blob and Tree FOIDs.
   */
  FilteredObjectIdType objectType() const;

  bool operator==(const FilteredObjectId&) const;
  bool operator<(const FilteredObjectId&) const;

  const std::string& getValue() const {
    return value_;
  }

  /**
   * Return whether the given string looks like a rendered FilteredObjectId.
   * Checks that the prefix before the first ':' parses as a valid
   * FilteredObjectIdType value.
   */
  static bool isFilteredObjectIdString(folly::StringPiece objectId);

  /**
   * Render a FilteredObjectId for consumption by users. Takes a string that
   * corresponds to the pre-rendered underlying ObjectId.
   * The format of rendered FilteredObjectIds varies by FilteredObjectIdType.
   * For blobs and unfiltered trees, the format is simple:
   *
   * [type]:[underlyingObject]
   *
   * - Type is a stringified int that represents the type of the
   * FilteredObjectId
   * - UnderlyingObject is the ObjectId rendered by the underlyingBackingStore
   *
   * For trees, the format is more complicated:
   *
   * [type]:[filterLength]:[filter][pathLength]:[path][underlyingObject]
   *
   * - Type is a stringified int that represents the type of the
   * FilteredObjectId
   * - FilterLength is the number of characters in the stringified filter
   * - Filter is the stringified filter
   * - PathLength is the number of characters in the stringified path
   * - Path is the stringified path
   * - UnderlyingObject is the ObjectId rendered by the underlyingBackingStore
   */
  static std::string renderFilteredObjectId(
      const FilteredObjectId& object,
      std::string underlyingObjectString);

  /**
   * Parse a rendered FilteredObjectId back into a FilteredObjectId.
   *
   * The renderFilteredObjectId method documentation details how arguments to
   * this method are expected to be formatted.
   */
  static FilteredObjectId parseFilteredObjectId(
      std::string_view object,
      std::shared_ptr<BackingStore> underlyingBackingStore);

 private:
  static std::string constructFromByteRange(folly::ByteRange bytes) {
    auto v = std::string{(const char*)bytes.data(), bytes.size()};
    return v;
  }

  /**
   * Serialize the tree path, filter, and object data into a buffer that
   * will be stored in the Overlay.
   */
  static std::string serializeTree(
      RelativePathPiece path,
      std::string_view filterId,
      const ObjectId&);

  /**
   * Serialize the blob object data into a buffer that will be stored in the
   * Overlay.
   */
  static std::string serializeBlob(const ObjectId& object);

  /**
   * Serialize the unfiltered tree object data into a buffer that will be
   * stored in the Overlay.
   */
  static std::string serializeUnfilteredTree(const ObjectId& object);

  /**
   * Validate data found in value_.
   *
   * The value_ member variable should already contain the serialized data,
   * (as returned by serialize()).
   *
   * Note there will be an exception being thrown if `value_` is invalid.
   */
  void validate();

  /**
   * The serialized data as written in the Overlay.
   */
  std::string value_;
};

} // namespace facebook::eden
