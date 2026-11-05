/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/portability/SysTypes.h>
#include <optional>
#include <variant>
#include "eden/fs/model/RootId.h"

namespace facebook::eden {

/**
 * In memory representation of the state of the SNAPSHOT file.
 */
class ParentCommit {
 public:
  struct WorkingCopyParentAndCheckedOutRevision;
  struct CheckoutInProgress;

  /* implicit */ ParentCommit(
      ParentCommit::WorkingCopyParentAndCheckedOutRevision state)
      : state_{std::move(state)} {}

  /* implicit */ ParentCommit(ParentCommit::CheckoutInProgress inProgress)
      : state_{std::move(inProgress)} {}

  /**
   * Returns true if a checkout is currently ongoing.
   */
  bool isCheckoutInProgress() const;

  /**
   * Since the parent commit might contain multiple RootId, allows choosing
   * which one should be preferred.
   *
   * In all the cases, when no checkout are ongoing, the current stable RootId
   * will always be used.
   */
  enum class RootIdPreference {
    /** During an update, prefer the destination RootId */
    To,
    /** During an update, prefer the originating RootId */
    From,
    /** During an update, no RootId are used */
    OnlyStable,
  };

  /**
   * Return the last checked out RootId.
   *
   * See the documentation of RootIdPreference for the behavior during
   * checkout.
   */
  std::optional<RootId> getLastCheckoutId(RootIdPreference preference) const;

  /**
   * Return the current reset RootId.
   *
   * In the case where a checkout is in progress, the destination commit is
   * returned.
   */
  RootId getWorkingCopyParent() const;

  struct CheckoutInProgress {
    RootId from;
    RootId to;
    pid_t pid;
  };

  /**
   * This is the steady state parent commit state.
   *
   * During a checkout operation both fields gets updated to the destination
   * commit, while a reset operation only updates the workingCopyParent field.
   */
  struct WorkingCopyParentAndCheckedOutRevision {
    RootId workingCopyParent;
    RootId checkedOut;
  };

  bool operator==(const ParentCommit& other) const;

 private:
  std::variant<WorkingCopyParentAndCheckedOutRevision, CheckoutInProgress>
      state_;
};

} // namespace facebook::eden
