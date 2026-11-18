/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::ensure;
use async_trait::async_trait;
use blobstore::Loadable;
use context::CoreContext;
use derived_data_manager::DerivationContext;
use derived_data_manager::DerivationStagePayload;
use derived_data_manager::ManifestDerivable;
use derived_data_manager::ManifestDerivationInput;
use derived_data_manager::ManifestStageDependencies;
use derived_data_manager::ManifestStageOutput;
use derived_data_manager::PipelineDerivable;
use derived_data_manager::StageId;
use derived_data_manager::manifest_partitions::create_sorted_subentries;
use derived_data_manager::stage_outputs::fetch_stage_outputs_with_fallback;
use fbthrift::compact_protocol;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future;
use futures::stream;
use futures::stream::BoxStream;
use manifest::Entry;
use manifest::TreeInfoSubentries;
use manifest::find_intersection_of_diffs;
use manifest::find_intersection_of_diffs_pruned;
use manifest::find_intersection_of_subentry_diffs;
use mononoke_types::BlobstoreBytes;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::FileUnodeId;
use mononoke_types::ManifestUnodeId;
use mononoke_types::NonRootMPath;
use mononoke_types::PipelineDerivableVariant;
use mononoke_types::SortedVectorTrieMap;
use mononoke_types::path::MPath;
use mononoke_types::thrift::unodes as unodes_thrift;

use crate::CopyInfoSource;
use crate::RootUnodeManifestId;
use crate::UnodeRenameSources;
use crate::derive::derive_unode_entry;
use crate::mapping::get_file_changes;
use crate::subtree_rename_sources;

#[async_trait]
impl ManifestDerivable for RootUnodeManifestId {
    type TreeId = ManifestUnodeId;
    type Leaf = FileUnodeId;
    type TrieMapType = SortedVectorTrieMap<Entry<Self::TreeId, Self::Leaf>>;

    const PIPELINE_VARIANT: PipelineDerivableVariant = PipelineDerivableVariant::Unodes;

    async fn load_root_manifest(
        &self,
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
    ) -> Result<ManifestUnodeId> {
        Ok(self.0)
    }

    async fn from_root_manifest(
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
        root: ManifestUnodeId,
    ) -> Result<Self> {
        Ok(Self(root))
    }

    async fn derive_manifest(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsai: BonsaiChangeset,
        input: ManifestDerivationInput<Self::TreeId, Self::Leaf, Self::TrieMapType>,
    ) -> Result<ManifestStageOutput<Self::TreeId, Self::Leaf, Self::TrieMapType>> {
        derive_unode_entry(
            ctx,
            derivation,
            bonsai.get_changeset_id(),
            input,
            get_file_changes(&bonsai),
        )
        .await
    }

    async fn create_subentries(
        ctx: &CoreContext,
        _derivation: &DerivationContext,
        subentries: TreeInfoSubentries<Self::TreeId, Self::Leaf, (), Self::TrieMapType>,
    ) -> Result<Self::TrieMapType> {
        create_sorted_subentries(ctx, subentries).await
    }

    fn encode_entry(entry: Option<Entry<ManifestUnodeId, FileUnodeId>>) -> BlobstoreBytes {
        let entry = match entry {
            Some(Entry::Tree(mf_unode_id)) => {
                unodes_thrift::UnodeStageOutput::manifest_unode_id(mf_unode_id.into_thrift())
            }
            Some(Entry::Leaf(file_unode_id)) => {
                unodes_thrift::UnodeStageOutput::file_unode_id(file_unode_id.into_thrift())
            }
            None => unodes_thrift::UnodeStageOutput::empty(unodes_thrift::UnodeStageOutputEmpty {
                ..Default::default()
            }),
        };
        BlobstoreBytes::from_bytes(compact_protocol::serialize(&entry))
    }

    fn decode_entry(bytes: BlobstoreBytes) -> Result<Option<Entry<ManifestUnodeId, FileUnodeId>>> {
        let thrift_output: unodes_thrift::UnodeStageOutput =
            compact_protocol::deserialize(bytes.into_bytes())?;
        Ok(match thrift_output {
            unodes_thrift::UnodeStageOutput::manifest_unode_id(id) => {
                Some(Entry::Tree(ManifestUnodeId::from_thrift(id)?))
            }
            unodes_thrift::UnodeStageOutput::file_unode_id(id) => {
                Some(Entry::Leaf(FileUnodeId::from_thrift(id)?))
            }
            unodes_thrift::UnodeStageOutput::empty(_) => None,
            unodes_thrift::UnodeStageOutput::UnknownField(x) => {
                return Err(anyhow!("unknown UnodeStageOutput variant {x}"));
            }
        })
    }
}

pub type UnodeStageOutput = <RootUnodeManifestId as PipelineDerivable>::StageOutput;

/// Load outputs for every commit in the batch and each parent, resolving missing external
/// parents from completed roots. Current commits must already have this exact stage output.
pub async fn fetch_stage_unodes(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    stage: &StageId,
    bonsais: &[BonsaiChangeset],
) -> Result<HashMap<ChangesetId, UnodeStageOutput>> {
    let csids: HashSet<_> = bonsais
        .iter()
        .map(BonsaiChangeset::get_changeset_id)
        .collect();
    let parents: HashSet<_> = bonsais
        .iter()
        .flat_map(BonsaiChangeset::parents)
        .filter(|parent| !csids.contains(parent))
        .collect();
    let (mut outputs, parents) = future::try_join(
        RootUnodeManifestId::fetch_stage_outputs(
            ctx,
            derivation,
            stage,
            csids.into_iter().collect(),
        ),
        fetch_stage_outputs_with_fallback::<RootUnodeManifestId>(
            ctx,
            derivation,
            stage,
            parents.into_iter().collect(),
        ),
    )
    .await?;
    for bonsai in bonsais {
        let csid = bonsai.get_changeset_id();
        ensure!(
            outputs.contains_key(&csid),
            "missing unode stage output for {csid} at {stage:?}"
        );
    }
    outputs.extend(parents);
    Ok(outputs)
}

pub async fn fetch_pipeline_unode_root(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    csid: ChangesetId,
) -> Result<ManifestUnodeId> {
    let stage = StageId::Manifest(MPath::ROOT);
    let output = fetch_stage_outputs_with_fallback::<RootUnodeManifestId>(
        ctx,
        derivation,
        &stage,
        vec![csid],
    )
    .await?
    .remove(&csid)
    .ok_or_else(|| anyhow!("missing unode root for {csid}"))?;
    match output.into_entry()? {
        Some(Entry::Tree(root)) => Ok(root),
        _ => Err(anyhow!("unode root for {csid} is not a tree")),
    }
}

/// New unodes in the canonical manifest, including both files and directories.
pub async fn find_new_canonical_unode_entries(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    csid: ChangesetId,
) -> Result<BoxStream<'static, Result<(MPath, Entry<ManifestUnodeId, FileUnodeId>)>>> {
    let bonsai = csid.load(ctx, derivation.blobstore()).await?;
    let root = derivation
        .fetch_dependency::<RootUnodeManifestId>(ctx, csid)
        .await?;
    let parents = stream::iter(bonsai.parents())
        .map(|parent| async move {
            let root = derivation
                .fetch_dependency::<RootUnodeManifestId>(ctx, parent)
                .await?;
            Ok::<_, Error>(*root.manifest_unode_id())
        })
        .buffered(100)
        .try_collect()
        .await?;
    Ok(find_intersection_of_diffs(
        ctx.clone(),
        derivation.blobstore().clone(),
        *root.manifest_unode_id(),
        parents,
    )
    .boxed())
}

/// New unodes owned by this stage, at absolute paths.
pub async fn find_new_stage_entries(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    payload: &DerivationStagePayload,
    current: UnodeStageOutput,
    parents: Vec<UnodeStageOutput>,
) -> Result<Vec<(MPath, Entry<ManifestUnodeId, FileUnodeId>)>> {
    let path = payload
        .path()
        .ok_or_else(|| anyhow!("finalize has no unode entries"))?
        .clone();
    let owned = payload.clone();
    let prefix = path.clone();
    let keep = move |relative: &MPath| owned.owns(&prefix.join(relative.iter()));
    let entries = match current {
        ManifestStageOutput::Entry(entry) => {
            let parents = parents
                .into_iter()
                .map(ManifestStageOutput::into_entry)
                .collect::<Result<Vec<_>>>()?;
            let Some(entry) = entry.filter(|entry| !parents.contains(&Some(*entry))) else {
                return Ok(Vec::new());
            };
            let only_root = matches!(payload, DerivationStagePayload::Manifest(stage) if matches!(stage.deps, ManifestStageDependencies::Partitions(_)));
            match entry {
                Entry::Tree(tree) if !only_root => find_intersection_of_diffs_pruned(
                    ctx.clone(),
                    derivation.blobstore().clone(),
                    tree,
                    parents
                        .into_iter()
                        .flatten()
                        .filter_map(Entry::into_tree)
                        .collect(),
                    move |diff| keep(diff.path()),
                )
                .boxed(),
                entry => stream::iter([Ok((MPath::ROOT, entry))]).boxed(),
            }
        }
        ManifestStageOutput::Subentries(map) => find_intersection_of_subentry_diffs(
            ctx.clone(),
            derivation.blobstore().clone(),
            map.load().await?,
            future::try_join_all(
                parents
                    .into_iter()
                    .map(ManifestStageOutput::into_subentries),
            )
            .await?,
            keep,
        ),
    };
    entries
        .try_filter_map(|(relative, entry)| {
            let path = path.join(relative.iter());
            future::ready(Ok(payload.owns(&path).then_some((path, entry))))
        })
        .try_collect()
        .await
}

/// Resolve copy sources inside the stage from its parents; other sources require their completed roots.
pub async fn find_stage_unode_rename_sources(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    payload: &DerivationStagePayload,
    bonsai: &BonsaiChangeset,
    parent_stage_outputs: &HashMap<ChangesetId, UnodeStageOutput>,
) -> Result<UnodeRenameSources> {
    let stage = payload.stage_id();
    let mut lookups: HashMap<ChangesetId, Vec<(NonRootMPath, NonRootMPath, usize)>> =
        HashMap::new();
    for (to_path, change) in bonsai
        .file_changes()
        .filter(|(path, _)| payload.owns((*path).into()))
    {
        let Some((from_path, csid)) = change.copy_from() else {
            continue;
        };
        let parent_index = bonsai
            .parents()
            .position(|parent| parent == *csid)
            .ok_or_else(|| {
                anyhow!(
                    "bonsai changeset {} contains invalid copy from parent: {csid}",
                    bonsai.get_changeset_id()
                )
            })?;
        lookups
            .entry(*csid)
            .or_default()
            .push((from_path.clone(), to_path.clone(), parent_index));
    }
    let futures = lookups.into_iter().map(|(csid, paths)| {
        let stage = &stage;
        async move {
            let source = parent_stage_outputs
                .get(&csid)
                .cloned()
                .ok_or_else(|| anyhow!("missing unode stage output for parent {csid}"))?;
            let entries = source
                .find_parent_entries(
                    ctx.clone(),
                    derivation.blobstore().clone(),
                    stage,
                    paths.iter().map(|(path, ..)| path.clone()).collect(),
                    || async {
                        fetch_pipeline_unode_root(ctx, derivation, csid)
                            .await
                            .map(Some)
                    },
                )
                .await?;
            Ok::<Vec<_>, Error>(
                paths
                    .into_iter()
                    .filter_map(|(from_path, to_path, parent_index)| {
                        let unode_id = entries.get(&from_path).copied()?.into_leaf()?;
                        Some((
                            to_path,
                            CopyInfoSource {
                                parent_index,
                                from_path,
                                unode_id,
                            },
                        ))
                    })
                    .collect(),
            )
        }
    });
    let copy_info = future::try_join_all(futures)
        .await?
        .into_iter()
        .flatten()
        .collect();
    Ok(UnodeRenameSources {
        copy_info,
        subtree_ops: subtree_rename_sources(bonsai),
    })
}

#[cfg(test)]
mod tests {
    use blobstore::Loadable;
    use derivation_queue_thrift::DerivationPriority;
    use derived_data_manager::ChildNameRange;
    use derived_data_manager::stage_outputs::StageOutput;
    use fbinit::FacebookInit;
    use mononoke_macros::mononoke;
    use tests_utils::CreateCommitContext;

    use super::*;
    use crate::tests::TestRepo;

    #[mononoke::fbinit_test]
    async fn stage_inputs_require_current_outputs_and_resolve_boundary_parents(
        fb: FacebookInit,
    ) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let a = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("dir/apple", "a")
            .add_file("dir/zebra", "z")
            .commit()
            .await?;
        let b = CreateCommitContext::new(&ctx, &repo, vec![a])
            .delete_file("dir/apple")
            .commit()
            .await?;
        let c = CreateCommitContext::new(&ctx, &repo, vec![a])
            .add_file("dir/zebra", "new z")
            .commit()
            .await?;
        let d = CreateCommitContext::new(&ctx, &repo, vec![b])
            .add_file("dir/apple", "new a")
            .commit()
            .await?;
        let manager = repo.repo_derived_data.manager();
        for csid in [c, d] {
            manager
                .derive::<RootUnodeManifestId>(&ctx, csid, None, DerivationPriority::LOW)
                .await?;
        }
        let derivation = manager.derivation_context(None);
        let bonsais = future::try_join_all(
            [b, c, d].map(async |csid| csid.load(&ctx, &repo.repo_blobstore).await),
        )
        .await?;
        for stage in [
            StageId::Manifest(MPath::try_from("dir")?),
            StageId::Subentries(MPath::try_from("dir")?, ChildNameRange::new(0, b'm')?),
            StageId::Manifest(MPath::try_from("missing")?),
        ] {
            let mut expected = HashMap::new();
            for csid in [a, b, c, d] {
                let root = derivation
                    .fetch_dependency::<RootUnodeManifestId>(&ctx, csid)
                    .await?;
                let output = RootUnodeManifestId::extract_stage_output_from_derived(
                    &ctx,
                    &derivation,
                    &root,
                    &stage,
                )
                .await?;
                expected.insert(csid, output);
            }
            let reference = RootUnodeManifestId::store_stage_outputs(
                &ctx,
                &derivation,
                &stage,
                HashMap::from([(d, expected[&d].clone())]),
            )
            .await?;
            let mut references: Vec<_> = reference.into_iter().collect();
            let inputs = derived_data_manager::StageInputs::new(&references)?;
            let supplied = manager.with_stage_inputs(inputs).derivation_context(None);
            // B is both a current commit and D's parent. Its canonical mapping must
            // not hide the missing stage output, even when that output would be empty.
            let error = fetch_stage_unodes(
                &ctx,
                &supplied,
                &stage,
                &[bonsais[0].clone(), bonsais[2].clone()],
            )
            .await
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(&format!("missing unode stage output for {b}"))
            );

            let reference = RootUnodeManifestId::store_stage_outputs(
                &ctx,
                &derivation,
                &stage,
                [b, c]
                    .map(|csid| (csid, expected[&csid].clone()))
                    .into_iter()
                    .collect(),
            )
            .await?;
            references.extend(reference);
            let inputs = derived_data_manager::StageInputs::new(&references)?;
            let supplied = manager.with_stage_inputs(inputs).derivation_context(None);
            // A is shared by B and C and has only a canonical mapping.
            let actual = fetch_stage_unodes(&ctx, &supplied, &stage, &bonsais).await?;
            assert_eq!(actual.len(), expected.len());
            for (csid, expected) in expected {
                assert!(
                    actual[&csid].equivalent(&expected).await?,
                    "stage {stage:?}, commit {csid}"
                );
            }
        }
        Ok(())
    }
}
