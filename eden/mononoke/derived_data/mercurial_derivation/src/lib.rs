/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod acl_overlay_manifest;
pub mod augmented_backfill;
mod augmented_derivation;
mod augmented_pipeline;
pub mod derive_hg_augmented_manifest;
pub mod derive_hg_changeset;
pub mod derive_hg_manifest;
// Rich, sharded-backed adapter types for the no-Hg Bonsai-direct augmented
// path (aug-manifest-v2 Option 1).
mod indexed_augmented_manifest;
mod mapping;
pub mod pipeline;
pub mod upload_augmented_manifest;

/// Access labels for the components allowed to read Hg manifests in a repo
/// that reads augmented manifests. See `CoreContext::with_access_label`.
pub mod access_labels {
    /// Hg changeset derivation through the derived data manager.
    pub const HG_CHANGESET_DERIVATION: &str = "hg_changeset_derivation";
    /// The stages of the Hg changeset derivation pipeline.
    pub const HG_CHANGESET_PIPELINE: &str = "hg_changeset_pipeline";
    /// The backfill of augmented manifests from Hg manifests.
    pub const AUGMENTED_BACKFILL_FROM_HG: &str = "augmented_backfill_from_hg";
}

pub use derive_hg_changeset::DeriveHgChangeset;
pub use derive_hg_changeset::derive_hg_augmented_manifest_at_creation;
pub use derive_hg_changeset::derive_hg_changeset;
pub use derive_hg_changeset::get_manifest_entry_from_bonsai;
pub use derive_hg_changeset::get_manifest_from_bonsai;
pub use mapping::MappedHgChangesetId;
pub use mapping::RootHgAugmentedManifestId;
