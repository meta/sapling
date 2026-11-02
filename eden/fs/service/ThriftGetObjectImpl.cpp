/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/service/ThriftGetObjectImpl.h"

#include "folly/Try.h"

#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/model/BlobAuxData.h"
#include "eden/fs/model/ObjectId.h"
#include "eden/fs/service/ThriftUtil.h"

namespace facebook::eden {
namespace detail {
folly::Try<BlobAuxData> transformToTryAuxData(
    folly::Try<std::optional<BlobAuxData>> auxData,
    std::shared_ptr<EdenMount> edenMount,
    ObjectId id) {
  if (auxData.hasException()) {
    return folly::Try<BlobAuxData>{std::move(auxData).exception()};
  }
  return transformToTryAuxData(auxData.value(), std::move(edenMount), id);
}
} // namespace detail

BlobMetadataWithOrigin transformToBlobMetadataFromOrigin(
    folly::Try<BlobAuxData> auxData,
    DataFetchOrigin origin) {
  BlobMetadataOrError auxDataOrError;
  if (auxData.hasValue()) {
    ScmBlobMetadata thriftMetadata;
    thriftMetadata.size() = auxData.value().size;
    thriftMetadata.contentsSha1() = thriftHash20(auxData.value().sha1);
    auxDataOrError.metadata() = std::move(thriftMetadata);
  } else {
    auxDataOrError.error() = newEdenError(auxData.exception());
  }
  BlobMetadataWithOrigin result;
  result.metadata() = std::move(auxDataOrError);
  result.origin() = std::move(origin);
  return result;
}

ScmTreeWithOrigin transformToTreeFromOrigin(
    std::shared_ptr<EdenMount> edenMount,
    const ObjectId& id,
    const folly::Try<std::shared_ptr<const Tree>>& tree,
    DataFetchOrigin origin) {
  ScmTreeOrError treeOrError;
  if (tree.hasValue()) {
    if (!tree.value()) {
      treeOrError.error() = newEdenError(
          ENOENT,
          EdenErrorType::POSIX_ERROR,
          "no tree found for id ",
          edenMount->getObjectStore()->renderObjectId(id));
    } else {
      treeOrError.treeEntries().ensure();
      for (const auto& entry : *(tree.value())) {
        const auto& [name, treeEntry] = entry;
        treeOrError.treeEntries()->emplace_back();
        auto& out = treeOrError.treeEntries()->back();
        out.name() = name.asString();
        out.mode() = modeFromTreeEntryType(treeEntry.getType());
        out.id() = edenMount->getObjectStore()->renderObjectId(
            treeEntry.getObjectId());
      }

      if (tree.value()->getAuxData()) {
        XLOGF(DBG7, "Tree id={} returned aux data", id);
      } else {
        XLOGF(DBG7, "Tree id={} does not return aux data", id);
      }
    }
  } else {
    treeOrError.error() = newEdenError(tree.exception());
  }
  ScmTreeWithOrigin result;
  result.scmTreeData() = std::move(treeOrError);
  result.origin() = std::move(origin);
  if (tree.hasValue() && tree.value() && tree.value()->getAuxData()) {
    TreeAux treeAux;

    DigestSizeOrError digestSize;
    digestSize.digestSize() = tree.value()->getAuxData()->digestSize;
    treeAux.digestSize() = std::move(digestSize);

    DigestHashOrError digestHash;
    digestHash.digestHash() =
        thriftHash32(tree.value()->getAuxData()->digestHash.value());
    treeAux.digestHash() = std::move(digestHash);

    result.treeAux() = std::move(treeAux);
  }
  return result;
}

} // namespace facebook::eden
