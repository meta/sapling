/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <cstdint>
#include <exception>
#include <string>

namespace facebook::eden {

enum BackingStoreType : uint8_t { EMPTY, HG, RECAS, HTTP, FILTEREDHG };

BackingStoreType toBackingStoreType(std::string_view type);

std::string_view toBackingStoreString(BackingStoreType type);

} // namespace facebook::eden
