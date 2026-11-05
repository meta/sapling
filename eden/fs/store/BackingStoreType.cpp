/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "BackingStoreType.h"
#include "eden/common/utils/Throw.h"

namespace facebook::eden {

BackingStoreType toBackingStoreType(std::string_view type) {
  if (type == "hg") {
    return BackingStoreType::HG;
  } else if (type == "filteredhg") {
    return BackingStoreType::FILTEREDHG;
  } else if (type == "recas") {
    return BackingStoreType::RECAS;
  } else if (type == "http") {
    return BackingStoreType::HTTP;
  } else if (type.empty()) {
    return BackingStoreType::EMPTY;
  } else {
    throw_<std::domain_error>("unsupported backing store type: ", type);
  }
}

std::string_view toBackingStoreString(BackingStoreType type) {
  switch (type) {
    case BackingStoreType::HG:
      return "hg";
    case BackingStoreType::FILTEREDHG:
      return "filteredhg";
    case BackingStoreType::RECAS:
      return "recas";
    case BackingStoreType::HTTP:
      return "http";
    case BackingStoreType::EMPTY:
      return "";
    default:
      throw_<std::domain_error>("unsupported backing store type");
  }
}

} // namespace facebook::eden
