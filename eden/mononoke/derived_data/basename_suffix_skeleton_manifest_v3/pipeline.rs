/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeSet;

use anyhow::Result;
use async_trait::async_trait;
use blobstore::Loadable;
use blobstore::Storable;
use cloned::cloned;
use context::CoreContext;
use derived_data_manager::DerivationContext;
use derived_data_manager::DerivationStagePayload;
use derived_data_manager::ManifestDerivable;
use derived_data_manager::ManifestDerivationInput;
use derived_data_manager::ManifestStageOutput;
use derived_data_manager::PreparedStageInputs;
use derived_data_manager::StageId;
use derived_data_manager::stage_outputs::fetch_stage_outputs_with_fallback;
use fbthrift::compact_protocol;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use manifest::Entry;
use manifest::TreeInfo;
use manifest::TreeInfoSubentries;
use mononoke_types::BlobstoreBytes;
use mononoke_types::BlobstoreValue;
use mononoke_types::BonsaiChangeset;
use mononoke_types::MPath;
use mononoke_types::NonRootMPath;
use mononoke_types::PipelineDerivableVariant;
use mononoke_types::ThriftConvert;
use mononoke_types::basename_suffix_skeleton_manifest_v3::BssmV3Directory;
use mononoke_types::basename_suffix_skeleton_manifest_v3::BssmV3Entry;
use mononoke_types::sharded_map_v2::LoadableShardedMapV2Node;
use mononoke_types::thrift::bssm::BssmPathChange;
use mononoke_types::thrift::bssm::BssmPreparedChanges;
use mononoke_types::thrift::bssm::BssmStageOutput;
use skeleton_manifest::RootSkeletonManifestId;

use crate::RootBssmV3DirectoryId;
use crate::derive::create_bssm_v3_directory;
use crate::derive::get_bssm_path_changes;

#[async_trait]
impl ManifestDerivable for RootBssmV3DirectoryId {
    type TreeId = BssmV3Directory;
    type Leaf = ();
    type TrieMapType = LoadableShardedMapV2Node<BssmV3Entry>;

    const PIPELINE_VARIANT: PipelineDerivableVariant = PipelineDerivableVariant::BssmV3;

    const HAS_PREPARE: bool = true;

    async fn prepare_batch(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsais: Vec<BonsaiChangeset>,
        stages: &[DerivationStagePayload],
    ) -> Result<Vec<PreparedStageInputs>> {
        let parent_ids = bonsais
            .iter()
            .flat_map(BonsaiChangeset::parents)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let parents = fetch_stage_outputs_with_fallback::<RootSkeletonManifestId>(
            ctx,
            derivation,
            &StageId::Manifest(MPath::ROOT),
            parent_ids,
        )
        .await?;
        let changes = stream::iter(bonsais)
            .map(|bonsai| {
                let parents = &parents;
                async move {
                    let skeletons = bonsai
                        .parents()
                        .map(|parent| match parents[&parent].clone().into_entry()? {
                            Some(Entry::Tree(tree)) => Ok(tree),
                            _ => anyhow::bail!("missing skeleton root for {parent}"),
                        })
                        .collect::<Result<_>>()?;
                    let changes = get_bssm_path_changes(
                        ctx,
                        derivation.blobstore().clone(),
                        &bonsai,
                        skeletons,
                    )
                    .await?;
                    Ok::<_, anyhow::Error>((bonsai.get_changeset_id(), changes))
                }
            })
            .buffered(100)
            .try_collect::<Vec<_>>()
            .await?;
        Ok(stages
            .iter()
            .map(DerivationStagePayload::stage_id)
            .map(|stage| PreparedStageInputs {
                changes: changes
                    .iter()
                    .map(|(csid, changes)| {
                        let changes = changes
                            .iter()
                            .filter(|(path, _)| stage.covers(path))
                            .map(|(path, change)| BssmPathChange {
                                path: path.clone().into_thrift(),
                                is_added: change.is_some(),
                            })
                            .collect();
                        (
                            *csid,
                            compact_protocol::serialize(&BssmPreparedChanges { changes }),
                        )
                    })
                    .collect(),
                stage,
            })
            .collect())
    }

    async fn load_root_manifest(
        &self,
        ctx: &CoreContext,
        derivation: &DerivationContext,
    ) -> Result<Self::TreeId> {
        Ok(self.0.load(ctx, derivation.blobstore()).await?)
    }

    async fn from_root_manifest(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        root: Self::TreeId,
    ) -> Result<Self> {
        Ok(Self(
            root.into_blob().store(ctx, derivation.blobstore()).await?,
        ))
    }

    async fn derive_manifest(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsai: BonsaiChangeset,
        input: ManifestDerivationInput<Self::TreeId, Self::Leaf, Self::TrieMapType>,
    ) -> Result<ManifestStageOutput<Self::TreeId, Self::Leaf, Self::TrieMapType>> {
        let blobstore = derivation.blobstore();
        let prepared: BssmPreparedChanges = compact_protocol::deserialize(
            derivation
                .prepared_input(&input.stage_id(), bonsai.get_changeset_id())?
                .clone(),
        )?;
        let changes = prepared
            .changes
            .into_iter()
            .map(|change| {
                Ok((
                    NonRootMPath::from_thrift(change.path)?,
                    change.is_added.then_some(()),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        input
            .derive(
                ctx.clone(),
                blobstore.clone(),
                changes,
                {
                    cloned!(ctx, blobstore);
                    move |info: TreeInfo<Self::TreeId, (), (), Self::TrieMapType>| {
                        cloned!(ctx, blobstore);
                        async move {
                            Ok((
                                (),
                                create_bssm_v3_directory(ctx, blobstore, info.subentries).await?,
                            ))
                        }
                    }
                },
                |_| async { Ok(((), ())) },
                |subentries| Self::create_subentries(ctx, derivation, subentries),
            )
            .await
    }

    async fn create_subentries(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        subentries: TreeInfoSubentries<Self::TreeId, Self::Leaf, (), Self::TrieMapType>,
    ) -> Result<Self::TrieMapType> {
        Ok(LoadableShardedMapV2Node::Inlined(
            create_bssm_v3_directory(ctx.clone(), derivation.blobstore().clone(), subentries)
                .await?
                .subentries,
        ))
    }

    fn encode_entry(entry: Option<Entry<Self::TreeId, Self::Leaf>>) -> BlobstoreBytes {
        let entry = entry
            .map(|entry| match entry {
                Entry::Tree(tree) => BssmV3Entry::Directory(tree),
                Entry::Leaf(()) => BssmV3Entry::File,
            })
            .map(ThriftConvert::into_thrift);
        BlobstoreBytes::from_bytes(compact_protocol::serialize(&BssmStageOutput { entry }))
    }

    fn decode_entry(bytes: BlobstoreBytes) -> Result<Option<Entry<Self::TreeId, Self::Leaf>>> {
        let output: BssmStageOutput = compact_protocol::deserialize(bytes.into_bytes())?;
        Ok(output
            .entry
            .map(BssmV3Entry::from_thrift)
            .transpose()?
            .map(|entry| match entry {
                BssmV3Entry::Directory(tree) => Entry::Tree(tree),
                BssmV3Entry::File => Entry::Leaf(()),
            }))
    }
}
