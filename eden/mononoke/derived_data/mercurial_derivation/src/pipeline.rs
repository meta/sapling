/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use blobrepo_common::changed_files::compute_changed_files;
use blobstore::Loadable;
use bonsai_hg_mapping::BonsaiHgMappingEntry;
use context::CoreContext;
use derived_data_manager::DerivationContext;
use derived_data_manager::DerivationStagePayload;
use derived_data_manager::ManifestStageOutput;
use derived_data_manager::PipelineDerivable;
use derived_data_manager::StageId;
use derived_data_manager::StageOutputs;
use derived_data_manager::manifest_partitions::create_sorted_subentries;
use derived_data_manager::manifest_partitions::decode_partition_reference;
use derived_data_manager::manifest_partitions::select_subentries;
use derived_data_manager::manifest_partitions::store_sorted_subentries;
use derived_data_manager::manifest_pipeline::derive_manifest_batch;
use derived_data_manager::stage_outputs::StageOutput;
use derived_data_manager::stage_outputs::fetch_stage_outputs;
use derived_data_manager::stage_outputs::fetch_stage_outputs_with_fallback;
use derived_data_manager::stage_outputs::store_stage_outputs;
use fbthrift::compact_protocol;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use manifest::Entry;
use manifest::ManifestOps;
use manifest::Traced;
use mercurial_types::HgChangesetId;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestId;
use mercurial_types::HgNodeHash;
use mercurial_types::subtree::HgSubtreeChanges;
use mononoke_types::BlobstoreBytes;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::FileType;
use mononoke_types::NonRootMPath;
use mononoke_types::PipelineDerivableVariant;
use mononoke_types::ThriftConvert;
use mononoke_types::path::MPath;

use crate::access_labels;
use crate::derive_hg_changeset::generate_hg_changeset;
use crate::derive_hg_changeset::get_manifest_entry_from_bonsai;
use crate::derive_hg_manifest::HgManifestOutput;
use crate::derive_hg_manifest::ParentIndex;
use crate::derive_hg_manifest::prepare_manifest_input;
use crate::mapping::HgChangesetDeriveOptions;
use crate::mapping::MappedHgChangesetId;
use crate::mapping::get_subtree_change_sources;

#[derive(Clone, Debug)]
pub enum HgStageOutput {
    Manifest(HgManifestOutput),
    Finalize {
        manifest_id: HgManifestId,
        hg_cs_id: HgChangesetId,
    },
}

#[async_trait]
impl StageOutput for HgStageOutput {
    async fn equivalent(&self, other: &Self) -> Result<bool> {
        match (self, other) {
            (Self::Manifest(a), Self::Manifest(b)) => a.equivalent(b).await,
            (
                Self::Finalize {
                    manifest_id: a,
                    hg_cs_id: ac,
                },
                Self::Finalize {
                    manifest_id: b,
                    hg_cs_id: bc,
                },
            ) => Ok(a == b && ac == bc),
            _ => Ok(false),
        }
    }
}

impl HgStageOutput {
    fn into_manifest(self) -> Result<HgManifestOutput> {
        match self {
            Self::Manifest(output) => Ok(output),
            Self::Finalize { .. } => anyhow::bail!("expected manifest stage output"),
        }
    }

    fn root_manifest(&self) -> Option<HgManifestId> {
        match self {
            Self::Manifest(ManifestStageOutput::Entry(Some(Entry::Tree(tree)))) => {
                Some(*tree.untraced())
            }
            Self::Finalize { manifest_id, .. } => Some(*manifest_id),
            _ => None,
        }
    }

    fn hg_cs_id(&self) -> Option<HgChangesetId> {
        match self {
            Self::Finalize { hg_cs_id, .. } => Some(*hg_cs_id),
            Self::Manifest(_) => None,
        }
    }
}

/// Wrap an untraced Entry with `Traced::generate` on both arms. Used when
/// reconstructing a stage output from a non-pipelined derivation: lineage is
/// not recoverable from a SQL-only or post-hoc load, so every leaf and tree
/// is tagged as "generated" (no specific parent index).
pub(crate) fn wrap_entry_traced(
    entry: Entry<HgManifestId, (FileType, HgFileNodeId)>,
) -> Entry<Traced<ParentIndex, HgManifestId>, Traced<ParentIndex, (FileType, HgFileNodeId)>> {
    match entry {
        Entry::Tree(m) => Entry::Tree(Traced::generate(m)),
        Entry::Leaf(l) => Entry::Leaf(Traced::generate(l)),
    }
}

async fn fetch_parent_root(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    csid: ChangesetId,
) -> Result<Option<Traced<ParentIndex, HgManifestId>>> {
    let root = fetch_stage_outputs_with_fallback::<MappedHgChangesetId>(
        ctx,
        derivation,
        &StageId::Manifest(MPath::ROOT),
        vec![csid],
    )
    .await?
    .remove(&csid)
    .ok_or_else(|| anyhow!("missing Hg root for {csid}"))?
    .into_manifest()?
    .into_entry()?
    .and_then(Entry::into_tree)
    .ok_or_else(|| anyhow!("Hg root for {csid} is not a tree"))?;
    Ok(Some(root))
}

#[async_trait]
impl PipelineDerivable for MappedHgChangesetId {
    const PIPELINE_DERIVABLE_VARIANT: PipelineDerivableVariant =
        PipelineDerivableVariant::HgChangesets;
    const HAS_FINALIZE: bool = true;
    type StageOutput = HgStageOutput;

    async fn derive_stage_batch(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsais: Vec<BonsaiChangeset>,
        payload: &DerivationStagePayload,
        parents: HashMap<ChangesetId, Self::StageOutput>,
        dependency_outputs: HashMap<ChangesetId, HashMap<StageId, Self::StageOutput>>,
    ) -> Result<HashMap<ChangesetId, Self::StageOutput>> {
        let ctx = &ctx.with_access_label(access_labels::HG_CHANGESET_PIPELINE);
        if matches!(payload, DerivationStagePayload::Finalize) {
            return finalize_stage_batch(ctx, derivation, bonsais, parents, dependency_outputs)
                .await;
        }
        let stage = payload.stage_id();
        derive_manifest_batch(
            bonsais,
            parents,
            dependency_outputs,
            |bonsai, parents, dependencies| {
                let stage = &stage;
                async move {
                    let input = prepare_manifest_input(
                        stage,
                        parents
                            .into_iter()
                            .map(HgStageOutput::into_manifest)
                            .collect::<Result<_>>()?,
                        dependencies
                            .into_iter()
                            .map(|(stage, output)| Ok((stage, output.into_manifest()?)))
                            .collect::<Result<_>>()?,
                    )?;
                    get_manifest_entry_from_bonsai(
                        ctx.clone(),
                        derivation.blobstore().clone(),
                        derivation.restricted_paths(),
                        bonsai,
                        input,
                        |csid| fetch_parent_root(ctx, derivation, csid),
                    )
                    .await
                    .map(HgStageOutput::Manifest)
                }
            },
        )
        .await
    }

    async fn extract_stage_output_from_derived(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        derived: &MappedHgChangesetId,
        stage: &StageId,
    ) -> Result<Self::StageOutput> {
        let ctx = &ctx.with_access_label(access_labels::HG_CHANGESET_PIPELINE);
        let hg_cs_id = derived.hg_changeset_id();
        let envelope = hg_cs_id.load(ctx, derivation.blobstore()).await?;
        let manifest_id = envelope.manifestid();
        let path = match stage {
            StageId::Manifest(path) | StageId::Subentries(path, _) => path,
            StageId::Finalize => {
                return Ok(HgStageOutput::Finalize {
                    manifest_id,
                    hg_cs_id,
                });
            }
        };
        let entry = manifest_id
            .find_entry(ctx.clone(), derivation.blobstore().clone(), path.clone())
            .await?
            .map(wrap_entry_traced);
        let output = if let StageId::Subentries(_, range) = stage {
            let entries = select_subentries(ctx, derivation.blobstore(), entry, *range).await?;
            ManifestStageOutput::Subentries(create_sorted_subentries(ctx, entries).await?.into())
        } else {
            ManifestStageOutput::Entry(entry)
        };
        Ok(HgStageOutput::Manifest(output))
    }

    async fn extract_stage_output_from_root(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        root: Self::StageOutput,
        stage: &StageId,
    ) -> Result<Self::StageOutput> {
        let id = root
            .hg_cs_id()
            .ok_or_else(|| anyhow!("expected finalized Hg changeset"))?;
        Self::extract_stage_output_from_derived(
            ctx,
            derivation,
            &MappedHgChangesetId::new(id),
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
        // Labelled because this flushes the Hg trees the stage wrote.
        let ctx = &ctx.with_access_label(access_labels::HG_CHANGESET_PIPELINE);
        if matches!(stage, StageId::Finalize) && derivation.publish_pipeline_outputs() {
            let entries = outputs
                .into_iter()
                .map(|(bcs_id, output)| {
                    Ok(BonsaiHgMappingEntry {
                        bcs_id,
                        hg_cs_id: output
                            .hg_cs_id()
                            .ok_or_else(|| anyhow!("missing final Hg changeset for {bcs_id}"))?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            derivation.flush(ctx).await?;
            return derivation
                .bonsai_hg_mapping()?
                .bulk_add(ctx, &entries)
                .await
                .map(|_| None);
        }
        store_stage_outputs::<Self, _, _>(
            ctx,
            derivation,
            stage,
            outputs,
            |_| None,
            |output| encode_stage_output(ctx, derivation, output),
        )
        .await
    }

    async fn fetch_stage_outputs(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        stage: &StageId,
        cs_ids: Vec<ChangesetId>,
    ) -> Result<HashMap<ChangesetId, Self::StageOutput>> {
        let ctx = &ctx.with_access_label(access_labels::HG_CHANGESET_PIPELINE);
        if matches!(stage, StageId::Finalize) && derivation.publish_pipeline_outputs() {
            let entries = derivation
                .bonsai_hg_mapping()?
                .get(ctx, cs_ids.into())
                .await?;
            return stream::iter(entries.into_iter().map(|entry| async move {
                let manifest_id = entry
                    .hg_cs_id
                    .load(ctx, derivation.blobstore())
                    .await?
                    .manifestid();
                Ok((
                    entry.bcs_id,
                    HgStageOutput::Finalize {
                        manifest_id,
                        hg_cs_id: entry.hg_cs_id,
                    },
                ))
            }))
            .buffer_unordered(100)
            .try_collect()
            .await;
        }
        fetch_stage_outputs::<Self, _, _>(
            ctx,
            derivation,
            stage,
            cs_ids,
            |_| None,
            |csid, bytes| decode_stage_output(ctx, derivation, stage, csid, bytes),
        )
        .await
    }
}

/// Derive the finalize stage for a batch: assemble each commit's hg changeset
/// from its root manifest (resolved as the finalize stage's dependency) and its
/// parents' hg changeset ids (the finalize chain), and write the hg envelope.
async fn finalize_stage_batch(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    bonsais: Vec<BonsaiChangeset>,
    parents: HashMap<ChangesetId, HgStageOutput>,
    dependency_outputs: HashMap<ChangesetId, HashMap<StageId, HgStageOutput>>,
) -> Result<HashMap<ChangesetId, HgStageOutput>> {
    let blobstore = derivation.blobstore();
    let derivation_opts = HgChangesetDeriveOptions {
        set_committer_field: derivation.config().hg_set_committer_extra,
    };

    // Root manifest id of any commit: from the root manifest stage — its
    // dependency output if the commit is in this batch, otherwise the parent's
    // finalize output (which carries the root manifest entry). All available
    // before the serial hashing pass.
    let manifest_id_of = |id: &ChangesetId| -> Option<HgManifestId> {
        let out = dependency_outputs
            .get(id)
            .and_then(|deps| deps.get(&StageId::Manifest(MPath::ROOT)))
            .or_else(|| parents.get(id))?;
        out.root_manifest()
    };

    // Compute each commit's changed-file list in parallel. It depends only on
    // manifest ids (all known up front), not on the parent hg-changeset chain,
    // so it runs ahead of the serial hashing pass below. Resolve the manifest
    // ids synchronously first (borrowing the dependency/parent maps), then fan
    // out the manifest diffs over owned inputs.
    let file_inputs: Vec<(
        ChangesetId,
        HgManifestId,
        Option<HgManifestId>,
        Option<HgManifestId>,
    )> = bonsais
        .iter()
        .map(|bonsai| {
            let cs_id = bonsai.get_changeset_id();
            let manifest_id = manifest_id_of(&cs_id)
                .ok_or_else(|| anyhow!("finalize for {cs_id}: missing root manifest"))?;
            let mut parent_mfs = bonsai.parents().map(|p| manifest_id_of(&p));
            let mf_p1 = parent_mfs.next().flatten();
            let mf_p2 = parent_mfs.next().flatten();
            Ok((cs_id, manifest_id, mf_p1, mf_p2))
        })
        .collect::<Result<_>>()?;

    let mut files_by_cs: HashMap<ChangesetId, Vec<NonRootMPath>> = stream::iter(file_inputs)
        .map(|(cs_id, manifest_id, mf_p1, mf_p2)| async move {
            let files =
                compute_changed_files(ctx.clone(), blobstore.clone(), manifest_id, mf_p1, mf_p2)
                    .await?;
            anyhow::Ok((cs_id, files))
        })
        .buffer_unordered(100)
        .try_collect()
        .await?;

    let batch_ids: HashSet<_> = bonsais
        .iter()
        .map(BonsaiChangeset::get_changeset_id)
        .collect();
    let source_ids = bonsais
        .iter()
        .flat_map(|bonsai| bonsai.subtree_changes().values())
        .filter_map(|change| change.change_source().map(|(csid, _)| csid))
        .filter(|csid| !batch_ids.contains(csid))
        .collect::<HashSet<_>>();
    let mut source_mapping: HashMap<_, _> = MappedHgChangesetId::fetch_stage_outputs(
        ctx,
        derivation,
        &StageId::Finalize,
        source_ids.into_iter().collect(),
    )
    .await?
    .into_iter()
    .filter_map(|(csid, output)| {
        output
            .hg_cs_id()
            .map(|id| (csid, MappedHgChangesetId::new(id)))
    })
    .collect();
    let mut results: HashMap<ChangesetId, HgStageOutput> = HashMap::new();
    for bonsai in bonsais {
        let cs_id = bonsai.get_changeset_id();
        let manifest_id = manifest_id_of(&cs_id)
            .ok_or_else(|| anyhow!("finalize for {cs_id}: missing root manifest dependency"))?;
        let parent_hg_cs_ids = bonsai
            .parents()
            .map(|parent| {
                results
                    .get(&parent)
                    .or_else(|| parents.get(&parent))
                    .and_then(HgStageOutput::hg_cs_id)
                    .ok_or_else(|| anyhow!("finalize parent {parent} of {cs_id} has no hg_cs_id"))
            })
            .collect::<Result<Vec<_>>>()?;
        let files = files_by_cs
            .remove(&cs_id)
            .ok_or_else(|| anyhow!("finalize for {cs_id}: changed files were not computed"))?;
        let sources = get_subtree_change_sources(ctx, derivation, &bonsai, &source_mapping).await?;
        let subtree_changes =
            HgSubtreeChanges::from_bonsai_subtree_changes(bonsai.subtree_changes(), sources)?;
        let (hg_cs_id, _) = generate_hg_changeset(
            ctx,
            blobstore,
            bonsai,
            manifest_id,
            parent_hg_cs_ids,
            files,
            subtree_changes,
            &derivation_opts,
        )
        .await?;
        source_mapping.insert(cs_id, MappedHgChangesetId::new(hg_cs_id));
        results.insert(
            cs_id,
            HgStageOutput::Finalize {
                manifest_id,
                hg_cs_id,
            },
        );
    }
    Ok(results)
}

async fn encode_stage_output(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    output: HgStageOutput,
) -> Result<BlobstoreBytes> {
    if let HgStageOutput::Manifest(ManifestStageOutput::Subentries(map)) = output {
        store_sorted_subentries(ctx, derivation.blobstore(), map, |entry| {
            encode_entry_output(HgStageOutput::Manifest(ManifestStageOutput::Entry(Some(
                entry,
            ))))
        })
        .await
    } else {
        encode_entry_output(output)
    }
}

fn encode_entry_output(output: HgStageOutput) -> Result<BlobstoreBytes> {
    let output = match output {
        HgStageOutput::Manifest(ManifestStageOutput::Subentries(_)) => {
            anyhow::bail!("expected Hg entry")
        }
        HgStageOutput::Manifest(ManifestStageOutput::Entry(Some(Entry::Tree(tree)))) => {
            mercurial_thrift::HgManifestStageOutput::tree(mercurial_thrift::HgManifestStageTree {
                manifest_id: tree.into_untraced().into_nodehash().into_thrift(),
            })
        }
        HgStageOutput::Manifest(ManifestStageOutput::Entry(Some(Entry::Leaf(leaf)))) => {
            let (file_type, filenode_id) = leaf.into_untraced();
            mercurial_thrift::HgManifestStageOutput::leaf(mercurial_thrift::HgManifestStageLeaf {
                file_type: file_type.into_thrift(),
                filenode_id: filenode_id.into_nodehash().into_thrift(),
            })
        }
        HgStageOutput::Manifest(ManifestStageOutput::Entry(None)) => {
            mercurial_thrift::HgManifestStageOutput::empty(
                mercurial_thrift::HgManifestStageOutputEmpty {},
            )
        }
        HgStageOutput::Finalize {
            manifest_id,
            hg_cs_id,
        } => mercurial_thrift::HgManifestStageOutput::terminal(
            mercurial_thrift::HgManifestStageTerminal {
                hg_cs_id: hg_cs_id.into_nodehash().into_thrift(),
                manifest_id: manifest_id.into_nodehash().into_thrift(),
            },
        ),
    };
    Ok(BlobstoreBytes::from_bytes(compact_protocol::serialize(
        &output,
    )))
}

async fn decode_stage_output(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    stage: &StageId,
    csid: ChangesetId,
    bytes: BlobstoreBytes,
) -> Result<HgStageOutput> {
    if matches!(stage, StageId::Subentries(..)) {
        let map = decode_partition_reference(
            ctx,
            derivation.blobstore().clone(),
            derivation.stage_inputs().clone(),
            bytes,
            move |bytes| {
                decode_entry_output(csid, bytes)?
                    .into_manifest()?
                    .into_entry()?
                    .ok_or_else(|| anyhow!("empty entry in Hg partition"))
            },
        )?;
        return Ok(HgStageOutput::Manifest(ManifestStageOutput::Subentries(
            map,
        )));
    }
    decode_entry_output(csid, bytes)
}

fn decode_entry_output(csid: ChangesetId, bytes: BlobstoreBytes) -> Result<HgStageOutput> {
    let output = match compact_protocol::deserialize(bytes.into_bytes())? {
        mercurial_thrift::HgManifestStageOutput::tree(tree) => {
            ManifestStageOutput::Entry(Some(Entry::Tree(Traced::generate(HgManifestId::new(
                HgNodeHash::from_thrift(tree.manifest_id)?,
            )))))
        }
        mercurial_thrift::HgManifestStageOutput::leaf(leaf) => {
            ManifestStageOutput::Entry(Some(Entry::Leaf(Traced::generate((
                FileType::from_thrift(leaf.file_type)?,
                HgFileNodeId::from_thrift(leaf.filenode_id)?,
            )))))
        }
        mercurial_thrift::HgManifestStageOutput::empty(_) => ManifestStageOutput::Entry(None),
        mercurial_thrift::HgManifestStageOutput::terminal(terminal) => {
            return Ok(HgStageOutput::Finalize {
                hg_cs_id: HgChangesetId::new(HgNodeHash::from_thrift(terminal.hg_cs_id)?),
                manifest_id: HgManifestId::new(HgNodeHash::from_thrift(terminal.manifest_id)?),
            });
        }
        mercurial_thrift::HgManifestStageOutput::UnknownField(field) => {
            anyhow::bail!("unknown Hg stage output variant {field} for {csid}")
        }
    };
    Ok(HgStageOutput::Manifest(output))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use derived_data_manager::ChildNameRange;
    use derived_data_manager::StageInputs;
    use derived_data_manager::manifest_partitions::PartitionOutput;
    use mononoke_macros::mononoke;
    use mononoke_types::SortedVectorTrieMap;
    use mononoke_types::hash::Blake2;
    use mononoke_types::sha1_hash::Sha1;
    use repo_blobstore::RepoBlobstore;
    use repo_blobstore::RepoBlobstoreRef;

    use super::*;

    #[facet::container]
    struct TestRepo(RepoBlobstore);

    #[mononoke::fbinit_test]
    async fn unchanged_partition_reuses_snapshot_after_parent_retagging(
        fb: fbinit::FacebookInit,
    ) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let store = repo.repo_blobstore();
        let tree = HgManifestId::new(HgNodeHash::new(Sha1::from_byte_array([1; 20])));
        let map = SortedVectorTrieMap::new(
            [(
                b"alpha".as_slice().into(),
                Entry::Tree(Traced::generate(tree)),
            )]
            .into_iter()
            .collect(),
        );
        let parent: PartitionOutput<_> = map.into();
        let reference = store_sorted_subentries(&ctx, store, parent.clone(), |entry| {
            encode_entry_output(HgStageOutput::Manifest(ManifestStageOutput::Entry(Some(
                entry,
            ))))
        })
        .await?;
        let decodes = Arc::new(AtomicUsize::new(0));
        let mut parent = decode_partition_reference(
            &ctx,
            store.clone(),
            Arc::new(StageInputs::default()),
            reference.clone(),
            {
                let decodes = decodes.clone();
                move |bytes| {
                    decodes.fetch_add(1, Ordering::Relaxed);
                    decode_entry_output(ChangesetId::new(Blake2::from_byte_array([2; 32])), bytes)?
                        .into_manifest()?
                        .into_entry()?
                        .ok_or_else(|| anyhow!("empty test entry"))
                }
            },
        )?;
        let stage = StageId::Subentries(MPath::new("dir")?, ChildNameRange::new(0, b'm')?);
        for _ in 0..3 {
            let input = prepare_manifest_input(
                &stage,
                vec![ManifestStageOutput::Subentries(parent)],
                HashMap::new(),
            )?;
            let output = input
                .derive(
                    ctx.clone(),
                    store.clone(),
                    vec![(NonRootMPath::new("elsewhere")?, Some(1u8))],
                    |_| async {
                        Err::<((), Traced<ParentIndex, HgManifestId>), _>(anyhow!(
                            "unexpected tree derivation"
                        ))
                    },
                    |_| async {
                        Err::<((), Traced<ParentIndex, (FileType, HgFileNodeId)>), _>(anyhow!(
                            "unexpected leaf derivation"
                        ))
                    },
                    |children| create_sorted_subentries(&ctx, children),
                )
                .await?;
            let ManifestStageOutput::Subentries(output) = output else {
                anyhow::bail!("expected partition output");
            };
            parent = output;
            assert_eq!(
                store_sorted_subentries(&ctx, store, parent.clone(), |_| {
                    panic!("unchanged Hg partition was encoded again")
                })
                .await?,
                reference
            );
        }
        assert_eq!(decodes.load(Ordering::Relaxed), 0);
        let entry = parent.clone().load().await?.into_iter().next().unwrap().1;
        assert_eq!(entry.into_tree().unwrap().id(), Some(&ParentIndex(0)));
        assert_eq!(decodes.load(Ordering::Relaxed), 1);
        Ok(())
    }
}
