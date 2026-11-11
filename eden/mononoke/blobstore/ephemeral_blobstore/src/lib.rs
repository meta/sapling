/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Ephemeral Blobstore
//!
//! Unlike other blobstores, this blobstore organises blobs into ephemeral
//! "bubbles".  Blobs placed within a bubble are deleted when the bubble
//! expires.  A bubble's lifespan can be extended at any time before it
//! expires.

mod bubble;
mod bubble_hg_mapping;
mod builder;
mod commit_graph;
mod error;
mod file;
mod handle;
mod store;
mod unlink;
mod view;

pub use crate::bubble::Bubble;
pub use crate::bubble::BubbleId;
pub use crate::bubble::StorageLocation;
pub use crate::bubble_hg_mapping::BubbleBonsaiHgMapping;
pub use crate::bubble_hg_mapping::bubble_hg_mapping_enabled;
pub use crate::builder::RepoEphemeralStoreBuilder;
pub use crate::commit_graph::EphemeralOnlyChangesetStorage;
pub use crate::error::EphemeralBlobstoreError;
pub use crate::handle::EphemeralHandle;
pub use crate::store::ArcRepoEphemeralStore;
pub use crate::store::DeleteBubbleOutcome;
pub use crate::store::ExtendBubbleTtlOutcome;
pub use crate::store::RepoEphemeralStore;
pub use crate::store::RepoEphemeralStoreArc;
pub use crate::store::RepoEphemeralStoreRef;
pub use crate::unlink::UnlinkPolicy;
pub use crate::unlink::UnlinkRetry;
pub use crate::view::EphemeralRepoView;
