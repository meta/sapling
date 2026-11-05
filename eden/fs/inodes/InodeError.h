/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include "eden/common/utils/PathFuncs.h"
#include "eden/fs/inodes/InodePtr.h"
#include "eden/fs/inodes/PathError.h"

namespace facebook::eden {

/**
 * A subclass of PathErrorBase referring to a specific inode.
 *
 * The main advantage of this class is that it can include the Inode path in
 * the error message.  However, it avoids computing the path until the error
 * message is actually needed.  If the error is caught and handled without
 * looking at the error message, then the path never needs to be computed.
 */
class InodeError : public PathErrorBase {
 public:
  InodeError(int errnum, InodePtr inode)
      : PathErrorBase(errnum), inode_(std::move(inode)) {}
  InodeError(int errnum, TreeInodePtr inode, PathComponentPiece child);
  InodeError(int errnum, InodePtr inode, std::string message)
      : PathErrorBase(errnum, std::move(message)), inode_(std::move(inode)) {}
  InodeError(
      int errnum,
      TreeInodePtr inode,
      PathComponentPiece child,
      std::string&& message);
  ~InodeError() override = default;

  InodeError(InodeError const&) = default;
  InodeError& operator=(InodeError const&) = default;
  InodeError(InodeError&&) = default;
  InodeError& operator=(InodeError&&) = default;

 protected:
  std::string computePath() const noexcept override;

 private:
  InodePtr inode_;
  std::optional<PathComponent> child_;
};

/** Missing or malformed overlay file data, distinct from ordinary I/O errors.
 */
class OverlayFileCorruption : public InodeError {
 public:
  using InodeError::InodeError;
};
} // namespace facebook::eden
