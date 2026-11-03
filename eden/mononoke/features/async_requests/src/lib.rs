/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![cfg_attr(fbcode_build, feature(error_generic_member_access))]

pub mod types;

mod error;
pub use error::AsyncRequestsError;

mod queue;
pub use queue::AbandonedRequestAction;
pub use queue::AsyncMethodRequestQueue;
pub use queue::ClaimedBy;
pub use queue::DequeuedRequest;
pub use queue::OrphanScanBatch;
pub use queue::PollError;
pub use queue::QueueRepoFilter;
pub use queue::QueueRequestTypeFilter;
pub use queue::RequestId;
pub use queue::RowId;
pub use requests_table::LongRunningRequestEntry;

pub mod tokens {
    pub use crate::types::DeriveBackfillRepoToken;
    pub use crate::types::DeriveBackfillToken;
    pub use crate::types::DeriveBoundariesToken;
    pub use crate::types::DeriveSliceToken;
    pub use crate::types::MegarepoAddBranchingTargetToken;
    pub use crate::types::MegarepoAddTargetToken;
    pub use crate::types::MegarepoChangeTargetConfigToken;
    pub use crate::types::MegarepoRemergeSourceToken;
    pub use crate::types::MegarepoSyncChangesetToken;
}
