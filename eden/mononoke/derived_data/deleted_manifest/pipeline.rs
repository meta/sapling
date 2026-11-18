/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;

use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use blobstore::Loadable;
use bytes::Bytes;
use context::CoreContext;
use derived_data_manager::ChildNameRange;
use derived_data_manager::DerivationContext;
use derived_data_manager::DerivationStagePayload;
use derived_data_manager::ManifestStageDependencies;
use derived_data_manager::PipelineDerivable;
use derived_data_manager::StageId;
use derived_data_manager::StageOutputs;
use derived_data_manager::manifest_pipeline::derive_manifest_batch;
use derived_data_manager::stage_outputs::fetch_stage_outputs;
use derived_data_manager::stage_outputs::store_stage_outputs;
use fbthrift::compact_protocol;
use itertools::Either;
use manifest::Entry;
use manifest::PathTree;
use mononoke_types::BlobstoreBytes;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::DeletedManifestV2Id;
use mononoke_types::FileUnodeId;
use mononoke_types::MPathElement;
use mononoke_types::ManifestUnodeId;
use mononoke_types::PipelineDerivableVariant;
use mononoke_types::ThriftConvert;
use mononoke_types::deleted_manifest_common::DeletedManifestCommon;
use mononoke_types::path::MPath;
use mononoke_types::sharded_map::ShardedMapEdge;
use mononoke_types::sharded_map::ShardedMapNode;
use mononoke_types::thrift::deleted_manifest as dm_thrift;
use mononoke_types::unode::UnodeEntry;
use multimap::MultiMap;
use unodes::UnodeStageOutput;
use unodes::fetch_stage_unodes;

use crate::RootDeletedManifestV2Id;
use crate::derive::DeletedManifestChildren;
use crate::derive::DeletedManifestDeriver;
use crate::derive::DeletedManifestSubentries;
use crate::derive::KnownDeletedManifest;
use crate::derive::UnodeChildren;
use crate::mapping::RootDeletedManifestIdCommon;
use crate::mapping_v2::format_key;

fn stage_path(stage: &StageId) -> Result<&MPath> {
    match stage {
        StageId::Manifest(path) | StageId::Subentries(path, _) => Ok(path),
        StageId::Finalize => anyhow::bail!("deleted manifests have no finalize stage"),
    }
}

fn use_normal_mapping(stage: &StageId) -> bool {
    matches!(stage, StageId::Manifest(path) if path.is_root())
        && justknobs::eval(
            "scm/mononoke:derived_data_pipeline_terminal_stage_prod_mapping",
            None,
            Some("deleted_manifests"),
        )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeletedManifestStageOutput {
    Entry(Option<DeletedManifestV2Id>),
    Subentries(DeletedManifestSubentries),
}

impl DeletedManifestStageOutput {
    fn into_entry(self) -> Result<Option<DeletedManifestV2Id>> {
        match self {
            Self::Entry(entry) => Ok(entry),
            Self::Subentries(_) => anyhow::bail!("expected a deleted manifest entry"),
        }
    }

    fn into_subentries(self) -> Result<DeletedManifestSubentries> {
        match self {
            Self::Subentries(children) => Ok(children),
            Self::Entry(_) => anyhow::bail!("expected deleted manifest children"),
        }
    }
}

fn map_parts(
    map: DeletedManifestSubentries,
) -> BTreeMap<Bytes, Either<DeletedManifestV2Id, ShardedMapEdge<DeletedManifestV2Id>>> {
    match map {
        ShardedMapNode::Terminal { values } => values
            .into_iter()
            .map(|(name, id)| (Bytes::from(name.into_vec()), Either::Left(id)))
            .collect(),
        ShardedMapNode::Intermediate {
            prefix,
            value,
            edges,
            ..
        } => value
            .into_iter()
            .map(|id| (Bytes::copy_from_slice(&prefix), Either::Left(id)))
            .chain(edges.into_iter().map(|(byte, edge)| {
                let mut name = prefix.to_vec();
                name.push(byte);
                (Bytes::from(name), Either::Right(edge))
            }))
            .collect(),
    }
}

async fn select_children(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    map: DeletedManifestSubentries,
    range: ChildNameRange,
) -> Result<DeletedManifestSubentries> {
    let entries = map_parts(map)
        .into_iter()
        .filter(|(name, _)| name.first().is_some_and(|byte| range.contains_byte(*byte)))
        .collect();
    ShardedMapNode::from_entries(ctx, derivation.blobstore(), entries).await
}

async fn unode_children(output: UnodeStageOutput) -> Result<Option<UnodeChildren>> {
    let children = output
        .into_subentries()
        .await?
        .into_iter()
        .map(|(name, entry)| Ok((MPathElement::from_smallvec(name)?, to_unode_entry(&entry))))
        .collect::<Result<UnodeChildren>>()?;
    Ok((!children.is_empty()).then_some(children))
}

/// Unode stage entry -> deleted manifest's `UnodeEntry`.
fn to_unode_entry(entry: &Entry<ManifestUnodeId, FileUnodeId>) -> UnodeEntry {
    match entry {
        Entry::Tree(id) => UnodeEntry::Directory(*id),
        Entry::Leaf(id) => UnodeEntry::File(*id),
    }
}

#[async_trait]
impl PipelineDerivable for RootDeletedManifestV2Id {
    const PIPELINE_DERIVABLE_VARIANT: PipelineDerivableVariant =
        PipelineDerivableVariant::DeletedManifests;

    const HAS_FINALIZE: bool = false;

    type StageOutput = DeletedManifestStageOutput;

    async fn derive_stage_batch(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsais: Vec<BonsaiChangeset>,
        payload: &DerivationStagePayload,
        parents: HashMap<ChangesetId, Self::StageOutput>,
        dependency_outputs: HashMap<ChangesetId, HashMap<StageId, Self::StageOutput>>,
    ) -> Result<HashMap<ChangesetId, Self::StageOutput>> {
        let stage = payload.stage_id();
        let stage_path = stage_path(&stage)?;
        let blobstore = derivation.blobstore();

        let unode_outputs = fetch_stage_unodes(ctx, derivation, &stage, &bonsais).await?;

        derive_manifest_batch(
            bonsais,
            parents,
            dependency_outputs,
            |bonsai, parents, dependencies| {
                let unode_outputs = &unode_outputs;
                async move {
                    let cs_id = bonsai.get_changeset_id();

                    let current_unode = unode_outputs[&cs_id].clone();
                    let parent_unode_outputs = bonsai
                        .parents()
                        .map(|parent| (parent, unode_outputs[&parent].clone()))
                        .collect::<HashMap<_, _>>()
                        .into_iter();
                    let parent_dms = bonsai.parents().zip(parents);
                    let changes = bonsai
                        .file_changes()
                        .filter(|(path, _)| match payload {
                            DerivationStagePayload::Subentries { range, .. } => {
                                range.contains_path(stage_path, path)
                            }
                            _ => true,
                        })
                        .filter_map(|(path, _)| path.remove_prefix_component(stage_path))
                        .map(|path| (path, ()))
                        .collect::<PathTree<_>>();
                    Ok(match payload {
                        DerivationStagePayload::Subentries { .. } => {
                            let parents = parent_dms
                                .map(|(csid, output)| Ok((output.into_subentries()?, vec![csid])))
                                .collect::<Result<_>>()?;
                            let parent_unodes = futures::future::try_join_all(
                                parent_unode_outputs.map(|(csid, output)| async move {
                                    Ok::<_, anyhow::Error>(
                                        unode_children(output)
                                            .await?
                                            .map(|children| (children, vec![csid])),
                                    )
                                }),
                            )
                            .await?
                            .into_iter()
                            .flatten()
                            .collect();
                            let children = DeletedManifestDeriver::derive_children(
                                ctx,
                                blobstore,
                                cs_id,
                                changes,
                                DeletedManifestChildren {
                                    parents,
                                    parent_unodes,
                                    current_unode: unode_children(current_unode).await?,
                                },
                            )
                            .await?;
                            DeletedManifestStageOutput::Subentries(children)
                        }
                        DerivationStagePayload::Manifest(manifest) => {
                            let parents = parent_dms
                                .map(|(csid, output)| Ok(output.into_entry()?.map(|id| (id, csid))))
                                .collect::<Result<Vec<_>>>()?
                                .into_iter()
                                .flatten()
                                .collect();
                            let parent_unodes: MultiMap<_, _> = parent_unode_outputs
                                .map(|(csid, output)| {
                                    Ok(output
                                        .into_entry()?
                                        .map(|entry| (to_unode_entry(&entry), csid)))
                                })
                                .collect::<Result<Vec<_>>>()?
                                .into_iter()
                                .flatten()
                                .collect();
                            let current = current_unode.into_entry()?.as_ref().map(to_unode_entry);
                            let known_entries = match &manifest.deps {
                                ManifestStageDependencies::Children(_) => dependencies
                                    .into_iter()
                                    .map(|(stage, output)| {
                                        let StageId::Manifest(path) = stage else {
                                            anyhow::bail!("expected a manifest child stage");
                                        };
                                        Ok((
                                            path.remove_prefix_component(stage_path),
                                            KnownDeletedManifest::Entry(output.into_entry()?),
                                        ))
                                    })
                                    .collect::<Result<_>>()?,
                                ManifestStageDependencies::Partitions(_) => {
                                    let parts = dependencies
                                        .into_values()
                                        .map(|output| Ok(map_parts(output.into_subentries()?)))
                                        .collect::<Result<Vec<_>>>()?
                                        .into_iter()
                                        .flatten()
                                        .collect();
                                    let children =
                                        ShardedMapNode::from_entries(ctx, blobstore, parts).await?;
                                    HashMap::from([(
                                        MPath::ROOT,
                                        KnownDeletedManifest::Children(children),
                                    )])
                                }
                            };
                            let entry = DeletedManifestDeriver::derive_subtree(
                                ctx,
                                blobstore,
                                cs_id,
                                changes,
                                parents,
                                current,
                                parent_unodes,
                                known_entries,
                                stage_path.is_root(),
                            )
                            .await?;
                            DeletedManifestStageOutput::Entry(entry)
                        }
                        DerivationStagePayload::Finalize => unreachable!("checked stage path"),
                    })
                }
            },
        )
        .await
    }

    async fn extract_stage_output_from_derived(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        derived: &RootDeletedManifestV2Id,
        stage: &StageId,
    ) -> Result<Self::StageOutput> {
        let stage_path = stage_path(stage)?;
        // Walk the canonical deleted manifest down to `stage_path` and return the
        // node id there, whether or not it is itself deleted. `DeletedManifestOps::find_entry`
        // can't be used: it only yields nodes with a linknode set, so it would
        // return `None` for a live directory node that merely has deleted
        // descendants (and for the root), whereas the stage output is that node.
        let blobstore = derivation.blobstore();
        let mut current_id = *derived.id();
        for element in stage_path {
            let node = current_id.load(ctx, blobstore).await?;
            match node.lookup(ctx, blobstore, element).await? {
                Some(child_id) => current_id = child_id,
                None => {
                    return Ok(match stage {
                        StageId::Subentries(..) => {
                            DeletedManifestStageOutput::Subentries(ShardedMapNode::default())
                        }
                        _ => DeletedManifestStageOutput::Entry(None),
                    });
                }
            }
        }
        match stage {
            StageId::Subentries(_, range) => Ok(DeletedManifestStageOutput::Subentries(
                select_children(
                    ctx,
                    derivation,
                    current_id.load(ctx, blobstore).await?.subentries,
                    *range,
                )
                .await?,
            )),
            _ => Ok(DeletedManifestStageOutput::Entry(Some(current_id))),
        }
    }

    async fn extract_stage_output_from_root(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        root: Self::StageOutput,
        stage: &StageId,
    ) -> Result<Self::StageOutput> {
        let id = root
            .into_entry()?
            .ok_or_else(|| anyhow!("expected deleted manifest root"))?;
        Self::extract_stage_output_from_derived(
            ctx,
            derivation,
            &RootDeletedManifestV2Id::new(id),
            stage,
        )
        .await
    }

    async fn store_stage_outputs(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        stage: &StageId,
        outputs: HashMap<ChangesetId, Self::StageOutput>,
    ) -> Result<Option<StageOutputs>> {
        let use_normal_mapping = use_normal_mapping(stage);
        store_stage_outputs::<Self, _, _>(
            ctx,
            derivation,
            stage,
            outputs,
            |csid| use_normal_mapping.then(|| format_key(derivation, csid)),
            async |output| {
                if use_normal_mapping {
                    let id = output
                        .into_entry()?
                        .ok_or_else(|| anyhow!("terminal stage output is None"))?;
                    return Ok(RootDeletedManifestV2Id::new(id).into());
                }
                let bytes = match output {
                    DeletedManifestStageOutput::Subentries(children) => {
                        compact_protocol::serialize(children.into_thrift())
                    }
                    DeletedManifestStageOutput::Entry(entry) => {
                        let output = match entry {
                            Some(id) => dm_thrift::DeletedManifestStageOutput::deleted_manifest_id(
                                id.into_thrift(),
                            ),
                            None => dm_thrift::DeletedManifestStageOutput::empty(
                                dm_thrift::DeletedManifestStageOutputEmpty {
                                    ..Default::default()
                                },
                            ),
                        };
                        compact_protocol::serialize(output)
                    }
                };
                Ok(BlobstoreBytes::from_bytes(bytes))
            },
        )
        .await
    }

    async fn fetch_stage_outputs(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        stage: &StageId,
        cs_ids: Vec<ChangesetId>,
    ) -> Result<HashMap<ChangesetId, Self::StageOutput>> {
        let use_normal_mapping = use_normal_mapping(stage);
        fetch_stage_outputs::<Self, _, _>(
            ctx,
            derivation,
            stage,
            cs_ids,
            |csid| use_normal_mapping.then(|| format_key(derivation, csid)),
            async |cs_id, blob| {
                if use_normal_mapping {
                    let root: RootDeletedManifestV2Id = blob.try_into()?;
                    return Ok(DeletedManifestStageOutput::Entry(Some(*root.id())));
                }
                if matches!(stage, StageId::Subentries(..)) {
                    let children = ShardedMapNode::from_thrift(compact_protocol::deserialize(
                        blob.into_bytes(),
                    )?)?;
                    return Ok(DeletedManifestStageOutput::Subentries(children));
                }
                let thrift_output: dm_thrift::DeletedManifestStageOutput =
                    compact_protocol::deserialize(blob.into_bytes())?;
                let output = match thrift_output {
                    dm_thrift::DeletedManifestStageOutput::deleted_manifest_id(id) => {
                        Some(DeletedManifestV2Id::from_thrift(id)?)
                    }
                    dm_thrift::DeletedManifestStageOutput::empty(_) => None,
                    dm_thrift::DeletedManifestStageOutput::UnknownField(x) => {
                        return Err(anyhow!(
                            "unknown DeletedManifestStageOutput variant {x} for {cs_id}"
                        ));
                    }
                };
                Ok(DeletedManifestStageOutput::Entry(output))
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use fbinit::FacebookInit;
    use mononoke_macros::mononoke;
    use mononoke_types::deleted_manifest_v2::DeletedManifestV2;
    use repo_derived_data::RepoDerivedDataRef;

    use super::*;
    use crate::test_utils::TestRepo;

    #[mononoke::fbinit_test]
    async fn partitions_preserve_native_map_hashes(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let derivation = repo.repo_derived_data().manager().derivation_context(None);
        let id = DeletedManifestV2::new(None, ShardedMapNode::default()).id();
        for size in [0, 1, 1999, 2000, 2001, 6000] {
            for letters in [b"a".as_slice(), b"az".as_slice()] {
                let entries = (0..size)
                    .map(|n| {
                        (
                            Bytes::from(format!(
                                "{}{n:05}",
                                char::from(letters[n % letters.len()])
                            )),
                            Either::Left(id),
                        )
                    })
                    .chain((size > 0).then(|| (Bytes::from_static(b"a"), Either::Left(id))))
                    .collect();
                let expected =
                    ShardedMapNode::from_entries(&ctx, derivation.blobstore(), entries).await?;
                let mut partitions = Vec::new();
                for range in derived_data_manager::partition_ranges(b"dgjnqtw") {
                    partitions
                        .push(select_children(&ctx, &derivation, expected.clone(), range).await?);
                }
                let actual = ShardedMapNode::from_entries(
                    &ctx,
                    derivation.blobstore(),
                    partitions.into_iter().flat_map(map_parts).collect(),
                )
                .await?;
                assert_eq!(actual, expected, "size {size}, letters {letters:?}");
                assert_eq!(
                    DeletedManifestV2::new(None, actual).id(),
                    DeletedManifestV2::new(None, expected).id()
                );
            }
        }
        Ok(())
    }
}
