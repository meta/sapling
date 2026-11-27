/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! The families Strider can audit. Only this module names specific families.

#[cfg(test)]
mod tests;

use std::num::NonZeroUsize;
use std::sync::Arc;

use deleted_manifest::RootDeletedManifestV2Id;
use mononoke_types::BlobstoreKey;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ContentId;
use mononoke_types::ContentMetadataV2;
use mononoke_types::DerivableType;
use mononoke_types::audit::CommitInfo;
use mononoke_types::audit::LoadContext;
use mononoke_types::audit::LoadError;
use mononoke_types::audit::RecomputeContentMetadata;
use mononoke_types::audit::TaggedContext;
use mononoke_types::audit::WalkStream;
use mononoke_types::audit::Walkable;
use repo_derived_data::RepoDerivedDataRef;
use unodes::RootUnodeManifestId;

/// Every family Strider can audit, each registered once.
pub fn families<R>() -> Vec<Family<R>>
where
    R: RepoDerivedDataRef + RecomputeContentMetadata + Send + Sync + 'static,
{
    vec![
        Family::of::<BonsaiChangeset>(),
        Family::of::<RootDeletedManifestV2Id>(),
        Family::of::<RootUnodeManifestId>(),
    ]
}

/// A selectable family: its `--family` name, its derived data type and its
/// enumeration.
pub struct Family<R> {
    pub name: &'static str,
    pub derived_type: Option<DerivableType>,
    walk_new: fn(LoadContext, Arc<R>, Arc<CommitInfo>, NonZeroUsize) -> WalkStream<R>,
}

impl<R> Family<R> {
    pub(crate) fn of<W: Walkable<R>>() -> Self {
        Self {
            name: W::NAME,
            derived_type: W::DERIVED_TYPE,
            walk_new: W::walk_new,
        }
    }

    pub fn walk_new(
        &self,
        ctx: LoadContext,
        repo: Arc<R>,
        commit: Arc<CommitInfo>,
        max_loads: NonZeroUsize,
    ) -> WalkStream<R> {
        (self.walk_new)(ctx, repo, commit, max_loads)
    }
}

/// How a repo recomputes old-format content metadata for `core`: with the
/// filestore's computation, which stores nothing, unlike its `get_metadata`.
pub(crate) async fn recompute_content_metadata(
    ctx: &TaggedContext,
    content: ContentId,
) -> Result<ContentMetadataV2, LoadError> {
    filestore::compute_metadata(ctx.blobstore(), ctx, content)
        .await
        .map_err(LoadError::Unreadable)?
        .ok_or_else(|| LoadError::Missing(content.blobstore_key()))
}
