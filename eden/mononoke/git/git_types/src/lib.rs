/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![cfg_attr(fbcode_build, feature(error_generic_member_access))]

pub mod mode;

pub mod thrift {
    pub use git_types_thrift::*;
}

mod commit;
mod compacted_delta_manifest;
mod delta_manifest_ops;
mod delta_manifest_v2;
mod delta_manifest_v3;
mod derive_commit;
mod derive_delta_manifest_v2;
mod derive_delta_manifest_v3;
mod errors;
pub mod git_lfs;
pub mod mutation;
mod object;
mod packfile;
mod store;
pub mod tree;

use std::io::Write;

use anyhow::Context;
use anyhow::Result;
pub use delta_manifest_ops::ObjectKind as DeltaObjectKind;
use gix_hash::ObjectId;
use gix_hash::oid;
use gix_object::Object;
use gix_object::WriteTo;
pub use object::ObjectContent;
pub use object::ObjectKind;
pub use object::test_util;
use sha1::Digest;
use sha1::Sha1;

pub use crate::commit::MappedGitCommitId;
pub use crate::compacted_delta_manifest::ArcCgdmChangesetDivider;
pub use crate::compacted_delta_manifest::CGDMCommitPackfileItems;
pub use crate::compacted_delta_manifest::CGDMComponents;
pub use crate::compacted_delta_manifest::CGDMComponentsReloader;
pub use crate::compacted_delta_manifest::CGDMComponentsReloaderArc;
pub use crate::compacted_delta_manifest::CGDMComponentsReloaderRef;
pub use crate::compacted_delta_manifest::CGDMDividedChangesets;
pub use crate::compacted_delta_manifest::CGDMGroup;
pub use crate::compacted_delta_manifest::CgdmChangesetDivider;
pub use crate::compacted_delta_manifest::CgdmChangesetDividerArc;
pub use crate::compacted_delta_manifest::CgdmChangesetDividerRef;
pub use crate::compacted_delta_manifest::CompactedGitDeltaManifest;
pub use crate::compacted_delta_manifest::ComponentInfo;
pub use crate::compacted_delta_manifest::DummyCgdmChangesetDivider;
pub use crate::delta_manifest_ops::GitDeltaManifestEntryOps;
pub use crate::delta_manifest_ops::GitDeltaManifestOps;
pub use crate::delta_manifest_ops::ObjectDeltaOps;
pub use crate::delta_manifest_ops::fetch_git_delta_manifest;
pub use crate::delta_manifest_v2::GDMV2Entry;
pub use crate::delta_manifest_v2::GDMV2ObjectEntry;
pub use crate::delta_manifest_v2::GitDeltaManifestV2;
pub use crate::delta_manifest_v3::GDMV3Chunk;
pub use crate::delta_manifest_v3::GitDeltaManifestV3;
pub use crate::derive_delta_manifest_v2::RootGitDeltaManifestV2Id;
pub use crate::derive_delta_manifest_v3::RootGitDeltaManifestV3Id;
pub use crate::errors::GitError;
pub use crate::packfile::BaseObject;
pub use crate::packfile::GitPackfileBaseItem;
pub use crate::packfile::PackfileItem;
pub use crate::store::GitIdentifier;
pub use crate::store::HeaderState;
pub use crate::store::fetch_git_object;
pub use crate::store::fetch_git_object_bytes;
pub use crate::store::fetch_non_blob_git_object;
pub use crate::store::fetch_non_blob_git_object_bytes;
pub use crate::store::fetch_packfile_base_item;
pub use crate::store::fetch_packfile_base_item_if_exists;
pub use crate::store::upload_non_blob_git_object;
pub use crate::store::upload_packfile_base_item;
pub use crate::tree::GitLeaf;
pub use crate::tree::GitTreeId;

/// Free function responsible for writing Git object data to a Vec
/// in loose format
pub fn git_object_bytes_with_hash(git_object: &Object) -> Result<(Vec<u8>, ObjectId)> {
    let mut object_bytes = git_object.loose_header().into_vec();
    git_object.write_to(object_bytes.by_ref())?;

    let mut hasher = Sha1::new();
    hasher.update(&object_bytes);
    let hash_bytes = hasher.finalize();
    let hash = oid::try_from_bytes(hash_bytes.as_ref())
        .context("Failed to convert packfile item hash to Git Object ID")?
        .into();

    Ok((object_bytes, hash))
}
