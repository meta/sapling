/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#ifdef _WIN32

#include <optional>

#include <folly/portability/Windows.h>

#include <winioctl.h> // @manual

#include "eden/common/utils/DirType.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/PathMap.h"
#include "eden/fs/inodes/overlay/gen-cpp2/overlay_types.h"
#include "eden/fs/model/ObjectId.h"
#include "eden/fs/model/TreeEntry.h"

namespace facebook::eden {

/**
 * Properties of a file or directory entry in a PrjFS virtualization root.
 *
 * TODO(mshroyer): Maybe factor out the overlay and scm-related properties used
 * by Windows FSCK.
 */
struct FsckFileState {
  bool onDisk = false;
  // populatedOrFullOrTomb is true if:
  //  - a file is full, hydrated or tomstoned
  //  - a directory is full or a dirty placeholder or a descendant is
  //  populatedOrFullOrTomb
  bool populatedOrFullOrTomb = false;
  // diskEmptyPlaceholder is true if:
  //  - a file is virtual or a placeholder
  //  - a directory is a placeholder and has no children (placeholder or
  //  otherwise)

  bool renamedPlaceholder = false;

  bool diskEmptyPlaceholder = false;
  bool directoryIsFull = false;
  bool diskTombstone = false;
  dtype_t diskDtype = dtype_t::Unknown;

  bool inOverlay = false;
  dtype_t overlayDtype = dtype_t::Unknown;
  std::optional<ObjectId> overlayId = std::nullopt;
  std::optional<overlay::OverlayEntry> overlayEntry = std::nullopt;

  bool inScm = false;
  dtype_t scmDtype = dtype_t::Unknown;
  std::optional<TreeEntryType> scmType = std::nullopt;
  std::optional<ObjectId> scmId = std::nullopt;

  bool shouldExist = false;
  dtype_t desiredDtype = dtype_t::Unknown;
  std::optional<ObjectId> desiredId = std::nullopt;
};

/**
 * Gets the state of entries on disk in a PrjFS virtualization root.
 *
 * Call with queryOnDiskEntriesOnly=true to use on a virtualization root while
 * the virtualization provider is running.  This ensures the flag
 * FIND_FIRST_EX_ON_DISK_ENTRIES_ONLY is provided to FindFirstFileExW, which
 * will prevent us from visiting and materializing virtual directory entries.
 */
PathMap<FsckFileState> getPrjfsOnDiskChildrenState(
    AbsolutePathPiece root,
    RelativePathPiece path,
    bool fsckRenamedFiles,
    bool queryOnDiskEntriesOnly);

} // namespace facebook::eden

#endif // defined _WIN32
