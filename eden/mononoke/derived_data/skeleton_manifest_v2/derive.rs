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
use derived_data_manager::ManifestDerivationInput;
use derived_data_manager::ManifestStageOutput;
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
use mononoke_types::NonRootMPath;
use mononoke_types::SkeletonManifestV2Id;
use mononoke_types::TrieMap;
use mononoke_types::sharded_map_v2::LoadableShardedMapV2Node;
use mononoke_types::sharded_map_v2::ShardedMapV2Node;
use mononoke_types::skeleton_manifest_v2::SkeletonManifestV2;
use mononoke_types::skeleton_manifest_v2::SkeletonManifestV2Entry;

use crate::RootSkeletonManifestV2Id;

pub(crate) fn get_file_changes(bcs: &BonsaiChangeset) -> Vec<(NonRootMPath, Option<()>)> {
    bcs.file_changes()
        .map(|(path, file_change)| (path.clone(), file_change.simplify().map(|_| ())))
        .collect()
}

async fn empty_manifest_id(
    ctx: &CoreContext,
    blobstore: &impl KeyedBlobstore,
) -> Result<SkeletonManifestV2Id> {
    let empty_manifest = SkeletonManifestV2::empty();
    empty_manifest.into_blob().store(ctx, blobstore).await
}

fn mf_entry_to_skeleton_manifest_v2_entry(
    entry: Entry<SkeletonManifestV2, ()>,
) -> SkeletonManifestV2Entry {
    match entry {
        Entry::Leaf(()) => SkeletonManifestV2Entry::File,
        Entry::Tree(dir) => SkeletonManifestV2Entry::Directory(dir),
    }
}

pub(crate) async fn create_skeleton_manifest_v2(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    subentries: TreeInfoSubentries<
        SkeletonManifestV2,
        (),
        (),
        LoadableShardedMapV2Node<SkeletonManifestV2Entry>,
    >,
) -> Result<SkeletonManifestV2> {
    let subentries: TrieMap<_> = subentries
        .into_iter()
        .map(|(prefix, entry_or_map)| match entry_or_map {
            Either::Left((_maybe_ctx, entry)) => (
                prefix,
                Either::Left(mf_entry_to_skeleton_manifest_v2_entry(entry)),
            ),
            Either::Right(map) => (prefix, Either::Right(map)),
        })
        .collect();

    let subentries =
        ShardedMapV2Node::from_entries_and_partial_maps(&ctx, &blobstore, subentries).await?;

    Ok(SkeletonManifestV2 { subentries })
}

pub(crate) async fn inner_derive(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    parents: Vec<SkeletonManifestV2>,
    changes: Vec<(NonRootMPath, Option<()>)>,
) -> Result<Option<SkeletonManifestV2>> {
    type Leaf = ();
    type LeafChange = ();
    type TreeId = SkeletonManifestV2;
    type Ctx = ();

    derive_manifest(
        ctx.clone(),
        blobstore.clone(),
        parents,
        changes,
        {
            cloned!(ctx, blobstore);
            move |info: TreeInfo<
                TreeId,
                Leaf,
                Ctx,
                LoadableShardedMapV2Node<SkeletonManifestV2Entry>,
            >| {
                cloned!(ctx, blobstore);
                async move {
                    let manifest =
                        create_skeleton_manifest_v2(ctx, blobstore, info.subentries).await?;
                    Ok(((), manifest))
                }
            }
        },
        move |_leaf_info: LeafInfo<Leaf, LeafChange>| async move { anyhow::Ok(((), ())) },
    )
    .await
}

pub(crate) async fn derive_skeleton_manifest_v2_entry(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    input: ManifestDerivationInput<
        SkeletonManifestV2,
        (),
        LoadableShardedMapV2Node<SkeletonManifestV2Entry>,
    >,
    changes: Vec<(NonRootMPath, Option<()>)>,
) -> Result<
    ManifestStageOutput<SkeletonManifestV2, (), LoadableShardedMapV2Node<SkeletonManifestV2Entry>>,
> {
    type Leaf = ();
    type LeafChange = ();
    type TreeId = SkeletonManifestV2;
    type Ctx = ();

    let blobstore = derivation_ctx.blobstore();
    input
        .derive(
            ctx.clone(),
            blobstore.clone(),
            changes,
            {
                cloned!(ctx, blobstore);
                move |info: TreeInfo<
                    TreeId,
                    Leaf,
                    Ctx,
                    LoadableShardedMapV2Node<SkeletonManifestV2Entry>,
                >| {
                    cloned!(ctx, blobstore);
                    async move {
                        let manifest =
                            create_skeleton_manifest_v2(ctx, blobstore, info.subentries).await?;
                        Ok(((), manifest))
                    }
                }
            },
            move |_leaf_info: LeafInfo<Leaf, LeafChange>| async move { anyhow::Ok(((), ())) },
            |subentries| async move {
                Ok(LoadableShardedMapV2Node::Inlined(
                    create_skeleton_manifest_v2(ctx.clone(), blobstore.clone(), subentries)
                        .await?
                        .subentries,
                ))
            },
        )
        .await
}

pub(crate) async fn derive_single(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: BonsaiChangeset,
    parents: Vec<RootSkeletonManifestV2Id>,
) -> Result<RootSkeletonManifestV2Id> {
    let blobstore = derivation_ctx.blobstore();
    let changes = get_file_changes(&bonsai);
    let parents = stream::iter(parents)
        .map(|parent| async move { parent.0.load(ctx, blobstore).await })
        .buffered(100)
        .try_collect::<Vec<_>>()
        .await?;

    let root_manifest = inner_derive(ctx, blobstore, parents, changes).await?;

    Ok(RootSkeletonManifestV2Id(match root_manifest {
        Some(manifest) => {
            let blob = manifest.into_blob();
            blob.store(ctx, blobstore)
                .await
                .context("failed to store SkeletonManifestV2 blob")?
        }
        None => empty_manifest_id(ctx, blobstore).await?,
    }))
}
