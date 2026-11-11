/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use mercurial_types::HgChangesetId;
use mononoke_types::RepositoryId;
use thiserror::Error;

use crate::bubble::BubbleId;

#[derive(Debug, Error, Clone)]
pub enum EphemeralBlobstoreError {
    /// The repository does not have an ephemeral blobstore.
    #[error("repo {0} does not have an ephemeral blobstore")]
    NoEphemeralBlobstore(RepositoryId),

    /// A new bubble could not be created.
    #[error("failed to create a new bubble")]
    CreateBubbleFailed,

    /// The requested bubble does not exist.  Either it was never created or has expired.
    #[error("bubble {0} does not exist, or has expired")]
    NoSuchBubble(BubbleId),

    /// An in-use bubble has expired.
    #[error("bubble {0} has expired")]
    BubbleExpired(BubbleId),

    /// The requested bubble could not be deleted.
    #[error("failed to delete bubble {0}")]
    DeleteBubbleFailed(BubbleId),

    /// The bubble deletion action is disabled
    #[error("bubble deletion is disabled")]
    DeleteBubbleDisabled,

    /// Failed to fetch labels associated with the bubble
    #[error("failed to fetch labels for bubble {0}")]
    FetchBubbleLabelsFailed(BubbleId),

    #[error("hg changeset {0} is already in bubble {1}")]
    HgChangesetInAnotherBubble(HgChangesetId, BubbleId),

    #[error("bubble hg mapping is disabled")]
    BubbleHgMappingDisabled,
}

impl EphemeralBlobstoreError {
    /// Returns true if this error indicates a bubble has expired or does not exist.
    ///
    /// Covers both `NoSuchBubble` (bubble expired at open time, from `open_bubble_raw`)
    /// and `BubbleExpired` (bubble expired mid-operation, from `check_unexpired`).
    pub fn is_bubble_expiry(&self) -> bool {
        matches!(
            self,
            EphemeralBlobstoreError::NoSuchBubble(_) | EphemeralBlobstoreError::BubbleExpired(_)
        )
    }
}
