/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use acl_manifest::RootAclManifestId;
use anyhow::Context;
use anyhow::Result;
use blobstore::StoreLoadable;
use context::CoreContext;
use derived_data::prefetch_content_metadata;
use derived_data_manager::DerivationContext;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future;
use futures::stream;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgManifestId;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;

use crate::mapping::RootHgAugmentedManifestId;

async fn lookup_mapped_root_hg_manifest_ids(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    csids: Vec<ChangesetId>,
) -> Result<HashMap<ChangesetId, HgManifestId>> {
    if csids.is_empty() {
        return Ok(HashMap::new());
    }

    let mappings = derivation_ctx
        .bonsai_hg_mapping()?
        .get(ctx, csids.into())
        .await?;
    let blobstore = Arc::clone(derivation_ctx.blobstore());

    stream::iter(mappings)
        .map(|entry| {
            let blobstore = Arc::clone(&blobstore);
            async move {
                let hg_manifest_id = entry
                    .hg_cs_id
                    .load(ctx, &blobstore)
                    .await
                    .with_context(|| {
                        format!(
                            "failed loading mapped HgChangeset {} for {}",
                            entry.hg_cs_id, entry.bcs_id,
                        )
                    })?
                    .manifestid();
                Ok((entry.bcs_id, hg_manifest_id))
            }
        })
        .buffer_unordered(100)
        .try_collect()
        .await
}

pub(crate) async fn lookup_mapped_root_hg_manifest_id(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    csid: ChangesetId,
) -> Result<Option<HgManifestId>> {
    Ok(
        lookup_mapped_root_hg_manifest_ids(ctx, derivation_ctx, vec![csid])
            .await?
            .remove(&csid),
    )
}

pub(crate) async fn derive_from_mapped_hg_manifest(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: &BonsaiChangeset,
    hg_manifest_id: HgManifestId,
    aug_parents: Vec<HgAugmentedManifestId>,
) -> Result<HgAugmentedManifestId> {
    let blobstore = derivation_ctx.blobstore();
    let csid = bonsai.get_changeset_id();
    let content_ids = bonsai
        .file_changes()
        .filter_map(|(_path, change)| change.simplify().map(|change| change.content_id()))
        .collect::<HashSet<_>>();
    let content_metadata_fut = prefetch_content_metadata(ctx, blobstore, content_ids);
    let acl_root_fut = derivation_ctx.fetch_dependency::<RootAclManifestId>(ctx, csid);
    let (content_metadata, acl_root) = future::try_join(content_metadata_fut, acl_root_fut).await?;
    let acl_root_overlay = crate::derive_hg_augmented_manifest::normalize_acl_root(&acl_root)?;

    crate::derive_hg_augmented_manifest::derive_from_hg_manifest_and_parents(
        ctx,
        blobstore,
        hg_manifest_id,
        aug_parents,
        &content_metadata,
        &derivation_ctx.restricted_paths(),
        acl_root_overlay,
    )
    .await
}

async fn derive_direct(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: &BonsaiChangeset,
    aug_parents: Vec<HgAugmentedManifestId>,
) -> Result<HgAugmentedManifestId> {
    let blobstore = derivation_ctx.blobstore();
    let csid = bonsai.get_changeset_id();

    let acl_root = derivation_ctx
        .fetch_dependency::<RootAclManifestId>(ctx, csid)
        .await?;

    let acl_root_overlay = crate::derive_hg_augmented_manifest::normalize_acl_root(&acl_root)?;

    crate::derive_hg_augmented_manifest::derive_augmented_manifest_from_bonsai_changeset(
        ctx,
        blobstore,
        bonsai,
        aug_parents,
        &derivation_ctx.restricted_paths(),
        acl_root_overlay,
    )
    .await
}

/// A commit with a Bonsai-to-Hg mapping builds from its mapped Hg manifest,
/// because an uploaded Hg changeset can differ from the one Mononoke would
/// derive, and the augmented manifest must match the Hg tree clients have.
pub(crate) async fn derive_single_root(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: BonsaiChangeset,
    parents: Vec<RootHgAugmentedManifestId>,
) -> Result<RootHgAugmentedManifestId> {
    let csid = bonsai.get_changeset_id();
    let aug_parents = parents
        .iter()
        .map(RootHgAugmentedManifestId::hg_augmented_manifest_id)
        .collect();
    let root = if let Some(hg_manifest_id) =
        lookup_mapped_root_hg_manifest_id(ctx, derivation_ctx, csid).await?
    {
        derive_from_mapped_hg_manifest(ctx, derivation_ctx, &bonsai, hg_manifest_id, aug_parents)
            .await?
    } else {
        derive_direct(ctx, derivation_ctx, &bonsai, aug_parents).await?
    };
    Ok(RootHgAugmentedManifestId::new(root))
}

pub(crate) async fn derive_batch_roots(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsais: Vec<BonsaiChangeset>,
) -> Result<HashMap<ChangesetId, RootHgAugmentedManifestId>> {
    let mut res: HashMap<ChangesetId, RootHgAugmentedManifestId> = HashMap::new();
    let mapped_hg_manifest_ids = lookup_mapped_root_hg_manifest_ids(
        ctx,
        derivation_ctx,
        bonsais
            .iter()
            .map(BonsaiChangeset::get_changeset_id)
            .collect(),
    )
    .await?;
    for bonsai in &bonsais {
        let csid = bonsai.get_changeset_id();
        let aug_parents: Vec<_> = derivation_ctx
            .fetch_unknown_parents::<RootHgAugmentedManifestId>(ctx, Some(&res), bonsai)
            .await?
            .iter()
            .map(RootHgAugmentedManifestId::hg_augmented_manifest_id)
            .collect();
        let root = if let Some(hg_manifest_id) = mapped_hg_manifest_ids.get(&csid).copied() {
            derive_from_mapped_hg_manifest(ctx, derivation_ctx, bonsai, hg_manifest_id, aug_parents)
                .await?
        } else {
            derive_direct(ctx, derivation_ctx, bonsai, aug_parents).await?
        };
        res.insert(csid, RootHgAugmentedManifestId::new(root));
    }
    Ok(res)
}
