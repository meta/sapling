/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include "eden/common/utils/DirType.h"
#include "eden/common/utils/GlobPath.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/fs/model/RootId.h"
#include "eden/fs/store/ObjectStore.h"

namespace facebook::eden {

struct GlobResult {
  GlobPath name;
  dtype_t dtype;
  // Currently this is the commit id for the commit to which this file
  // belongs. But should eden move away from commit ids this may become
  // the tree id of the root tree to which this file belongs.
  // This should never become a dangling reference because the caller
  // of Globresult::evaluate ensures that the ids have a lifetime that
  // exceeds that of the GlobResults returned.
  const RootId* originId;

  // Comparison operator for testing purposes
  bool operator==(const GlobResult& other) const noexcept {
    return name == other.name && dtype == other.dtype &&
        originId == other.originId;
  }
  bool operator!=(const GlobResult& other) const noexcept {
    return !(*this == other);
  }

  bool operator<(const GlobResult& other) const noexcept {
    auto nameCompare = name.compare(other.name);
    if (nameCompare != 0) {
      return nameCompare < 0;
    }
    if (dtype != other.dtype) {
      return dtype < other.dtype;
    }
    return originId < other.originId;
  }

  // originId should never become a dangling reference because the caller
  // of Globresult::evaluate ensures that the ids have a lifetime that
  // exceeds that of the GlobResults returned.
  GlobResult(RelativePathPiece name, dtype_t dtype, const RootId& originId)
      : name(name), dtype(dtype), originId(&originId) {}

  GlobResult(GlobPath&& name, dtype_t dtype, const RootId& originId) noexcept
      : name(std::move(name)), dtype(dtype), originId(&originId) {}

  GlobResult(
      RelativePath&& name,
      dtype_t dtype,
      const RootId& originId) noexcept
      : name(std::move(name).intoString()), dtype(dtype), originId(&originId) {}
};

using ResultList = folly::Synchronized<std::vector<GlobResult>>;

using PrefetchList = folly::Synchronized<std::vector<ObjectId>>;

} // namespace facebook::eden
