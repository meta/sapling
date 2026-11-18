/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use blobstore::Storable;
use cloned::cloned;
use context::CoreContext;
use derived_data_manager::DerivationContext;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use itertools::Either;
use manifest::Entry;
use manifest::LeafInfo;
use manifest::TreeInfo;
use manifest::TreeInfoSubentries;
use manifest::derive_manifest;
use mononoke_types::BlobstoreValue;
use mononoke_types::BonsaiChangeset;
use mononoke_types::CaseConflictSkeletonManifestId;
use mononoke_types::NonRootMPath;
use mononoke_types::TrieMap;
use mononoke_types::case_conflict_skeleton_manifest::CaseConflictSkeletonManifest;
use mononoke_types::case_conflict_skeleton_manifest::CcsmEntry;
use mononoke_types::sharded_map_v2::LoadableShardedMapV2Node;
use mononoke_types::sharded_map_v2::ShardedMapV2Node;

use crate::mapping::RootCaseConflictSkeletonManifestId;
use crate::path::CcsmPath;

pub(crate) async fn get_ccsm_path_changes(
    bcs: &BonsaiChangeset,
) -> Vec<(NonRootMPath, Option<()>)> {
    bcs.file_changes()
        .flat_map(|(path, file_change)| {
            CcsmPath::transform(path.clone())
                .map(|path| (path.into_raw(), file_change.simplify().map(|_| ())))
        })
        .collect()
}

async fn empty_directory_id(
    ctx: &CoreContext,
    blobstore: &impl KeyedBlobstore,
) -> Result<CaseConflictSkeletonManifestId> {
    let leaf = CaseConflictSkeletonManifest::empty();
    leaf.into_blob().store(ctx, blobstore).await
}

fn mf_entry_to_ccsm_entry(entry: Entry<CaseConflictSkeletonManifest, ()>) -> CcsmEntry {
    match entry {
        Entry::Leaf(()) => CcsmEntry::File,
        Entry::Tree(entry) => CcsmEntry::Directory(entry),
    }
}

pub(crate) async fn create_case_conflict_skeleton_manifest(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    subentries: TreeInfoSubentries<
        CaseConflictSkeletonManifest,
        (),
        (),
        LoadableShardedMapV2Node<CcsmEntry>,
    >,
) -> Result<CaseConflictSkeletonManifest> {
    let subentries: TrieMap<_> = subentries
        .into_iter()
        .map(|(prefix, entry_or_map)| match entry_or_map {
            Either::Left((_maybe_ctx, entry)) => {
                (prefix, Either::Left(mf_entry_to_ccsm_entry(entry)))
            }
            Either::Right(map) => (prefix, Either::Right(map)),
        })
        .collect();

    let subentries =
        ShardedMapV2Node::from_entries_and_partial_maps(&ctx, &blobstore, subentries).await?;

    Ok(CaseConflictSkeletonManifest { subentries })
}

pub(crate) async fn inner_derive(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    parents: Vec<CaseConflictSkeletonManifest>,
    changes: Vec<(NonRootMPath, Option<()>)>,
) -> Result<Option<CaseConflictSkeletonManifest>> {
    type Leaf = ();
    type LeafChange = ();
    type TreeId = CaseConflictSkeletonManifest;
    type Ctx = ();

    derive_manifest(
        ctx.clone(),
        blobstore.clone(),
        parents,
        changes,
        {
            cloned!(ctx, blobstore);
            move |info: TreeInfo<TreeId, Leaf, Ctx, LoadableShardedMapV2Node<CcsmEntry>>| {
                cloned!(ctx, blobstore);
                async move {
                    let directory =
                        create_case_conflict_skeleton_manifest(ctx, blobstore, info.subentries)
                            .await?;
                    Ok(((), directory))
                }
            }
        },
        move |_leaf_info: LeafInfo<Leaf, LeafChange>| async move { anyhow::Ok(((), ())) },
    )
    .await
}

pub(crate) async fn derive_single(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: BonsaiChangeset,
    parents: Vec<RootCaseConflictSkeletonManifestId>,
) -> Result<RootCaseConflictSkeletonManifestId> {
    let blobstore = derivation_ctx.blobstore();
    let changes = get_ccsm_path_changes(&bonsai).await;
    let parents = stream::iter(parents)
        .map(|parent| async move { parent.0.load(ctx, blobstore).await })
        .buffered(100)
        .try_collect::<Vec<_>>()
        .await?;

    let root_directory = inner_derive(ctx, blobstore, parents, changes).await?;

    Ok(RootCaseConflictSkeletonManifestId(match root_directory {
        Some(directory) => {
            let blob = directory.into_blob();
            blob.store(ctx, blobstore)
                .await
                .context("failed to store CaseConflictSkeletonManifest blob")?
        }
        None => empty_directory_id(ctx, blobstore).await?,
    }))
}
