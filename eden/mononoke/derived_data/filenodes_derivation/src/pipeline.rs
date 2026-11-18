/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use async_trait::async_trait;
use blobstore::Loadable;
use context::CoreContext;
use derived_data_manager::BonsaiDerivable;
use derived_data_manager::DerivationContext;
use derived_data_manager::DerivationStagePayload;
use derived_data_manager::PipelineDerivable;
use derived_data_manager::StageId;
use derived_data_manager::StageOutputs;
use derived_data_manager::stage_outputs::fetch_stage_outputs;
use derived_data_manager::stage_outputs::fetch_stage_outputs_with_fallback;
use derived_data_manager::stage_outputs::store_stage_outputs;
use fbthrift::compact_protocol;
use filenodes::FilenodeInfo;
use filenodes::FilenodeResult;
use filenodes::PreparedFilenode;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use mercurial_derivation::MappedHgChangesetId;
use mercurial_derivation::pipeline::HgStageOutput;
use mononoke_types::BlobstoreBytes;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use mononoke_types::PipelineDerivableVariant;
use mononoke_types::RepoPath;

use crate::FilenodesOnlyPublic;
use crate::derive::generate_all_filenodes;
use crate::derive::generate_filenodes_from_manifests;

fn encode(records: Option<Vec<PreparedFilenode>>) -> BlobstoreBytes {
    BlobstoreBytes::from_bytes(compact_protocol::serialize(
        &filenodes::thrift::FilenodesStageOutput {
            records: records.map(|records| {
                records
                    .into_iter()
                    .map(|record| filenodes::thrift::PreparedFilenode {
                        path: record.path.into_thrift(),
                        info: record.info.into_thrift(),
                    })
                    .collect()
            }),
        },
    ))
}

fn decode(bytes: BlobstoreBytes) -> Result<Option<Vec<PreparedFilenode>>> {
    let output: filenodes::thrift::FilenodesStageOutput =
        compact_protocol::deserialize(bytes.into_bytes())?;
    output
        .records
        .map(|records| {
            records
                .into_iter()
                .map(|record| {
                    Ok(PreparedFilenode {
                        path: RepoPath::from_thrift(record.path)?,
                        info: FilenodeInfo::from_thrift(record.info)?,
                    })
                })
                .collect()
        })
        .transpose()
}

fn covers(stage: &StageId, path: &RepoPath) -> bool {
    match (stage, path.mpath()) {
        (StageId::Manifest(prefix), None) => prefix.is_root(),
        (_, Some(path)) => stage.covers(path),
        _ => false,
    }
}

fn sort_records(records: &mut [PreparedFilenode]) {
    records.sort_by(|a, b| a.path.mpath().cmp(&b.path.mpath()));
}

#[async_trait]
impl PipelineDerivable for FilenodesOnlyPublic {
    const PIPELINE_DERIVABLE_VARIANT: PipelineDerivableVariant =
        PipelineDerivableVariant::FileNodes;
    const HAS_FINALIZE: bool = false;
    const FULL_DEPENDENCIES: &'static [DerivableType] = &[DerivableType::HgChangesets];
    type StageOutput = Option<Vec<PreparedFilenode>>;

    async fn derive_stage_batch(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsais: Vec<BonsaiChangeset>,
        payload: &DerivationStagePayload,
        _parents: HashMap<ChangesetId, Self::StageOutput>,
        mut dependency_outputs: HashMap<ChangesetId, HashMap<StageId, Self::StageOutput>>,
    ) -> Result<HashMap<ChangesetId, Self::StageOutput>> {
        let ids = bonsais
            .iter()
            .flat_map(|bonsai| std::iter::once(bonsai.get_changeset_id()).chain(bonsai.parents()))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let hg = fetch_stage_outputs_with_fallback::<MappedHgChangesetId>(
            ctx,
            derivation,
            &StageId::Finalize,
            ids,
        )
        .await?
        .into_iter()
        .map(|(csid, output)| match output {
            HgStageOutput::Finalize {
                manifest_id,
                hg_cs_id,
            } => Ok((csid, (manifest_id, hg_cs_id))),
            _ => bail!("expected finalized Hg changeset for {csid}"),
        })
        .collect::<Result<HashMap<_, _>>>()?;
        let generated = stream::iter(bonsais)
            .map(|bonsai| {
                let hg = &hg;
                async move {
                    let csid = bonsai.get_changeset_id();
                    let (root, linknode) = hg[&csid];
                    let parents = bonsai.parents().map(|parent| hg[&parent].0).collect();
                    let records = generate_filenodes_from_manifests(
                        ctx,
                        derivation,
                        root,
                        parents,
                        linknode,
                        Some(payload.clone()),
                    )
                    .await?;
                    Ok::<_, anyhow::Error>((csid, records))
                }
            })
            .buffered(100)
            .try_collect::<Vec<_>>()
            .await?;
        let mut outputs = HashMap::new();
        for (csid, records) in generated {
            let children = dependency_outputs
                .remove(&csid)
                .unwrap_or_default()
                .into_values()
                .collect::<Option<Vec<_>>>();
            let Some(children) = children else {
                outputs.insert(csid, None);
                continue;
            };
            let output = if derivation.publish_pipeline_outputs() {
                let (roots, records): (Vec<_>, Vec<_>) = records
                    .into_iter()
                    .partition(|record| record.path.is_root());
                if !records.is_empty()
                    && let FilenodeResult::Disabled =
                        derivation.filenodes()?.add_filenodes(ctx, records).await?
                {
                    None
                } else {
                    Some(roots)
                }
            } else {
                let mut records = children
                    .into_iter()
                    .flatten()
                    .chain(records)
                    .collect::<Vec<_>>();
                sort_records(&mut records);
                Some(records)
            };
            outputs.insert(csid, output);
        }
        Ok(outputs)
    }

    async fn extract_stage_output_from_derived(
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
        derived: &Self,
        _stage: &StageId,
    ) -> Result<Self::StageOutput> {
        Ok(match derived {
            Self::Disabled => None,
            Self::Present { .. } => Some(Vec::new()),
        })
    }

    async fn extract_stage_output_from_root(
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
        root: Self::StageOutput,
        stage: &StageId,
    ) -> Result<Self::StageOutput> {
        Ok(root.map(|records| {
            records
                .into_iter()
                .filter(|record| covers(stage, &record.path))
                .collect()
        }))
    }

    async fn store_stage_outputs(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        stage: &StageId,
        outputs: HashMap<ChangesetId, Self::StageOutput>,
    ) -> Result<Option<StageOutputs>> {
        if stage.is_terminal() && derivation.publish_pipeline_outputs() {
            let roots = outputs
                .into_iter()
                .flat_map(|(csid, records)| {
                    records.into_iter().flatten().map(move |record| {
                        Ok((
                            csid,
                            Self::Present {
                                root_filenode: Some(record.try_into()?),
                            },
                        ))
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Self::store_mapping_batch(ctx, derivation, roots).await?;
            return Ok(None);
        }
        store_stage_outputs::<Self, _, _>(
            ctx,
            derivation,
            stage,
            outputs,
            |_| None,
            |output| async { Ok(encode(output)) },
        )
        .await
    }

    async fn fetch_stage_outputs(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        stage: &StageId,
        csids: Vec<ChangesetId>,
    ) -> Result<HashMap<ChangesetId, Self::StageOutput>> {
        if stage.is_terminal() && derivation.publish_pipeline_outputs() {
            return stream::iter(csids)
                .map(|csid| async move {
                    Ok::<_, anyhow::Error>(Self::fetch(ctx, derivation, csid).await?.map(
                        |derived| {
                            (
                                csid,
                                match derived {
                                    Self::Disabled => None,
                                    Self::Present { .. } => Some(Vec::new()),
                                },
                            )
                        },
                    ))
                })
                .buffered(100)
                .try_filter_map(|output| async { Ok(output) })
                .try_collect()
                .await;
        }
        fetch_stage_outputs::<Self, _, _>(
            ctx,
            derivation,
            stage,
            csids,
            |_| None,
            |_, bytes| async { decode(bytes) },
        )
        .await
    }

    async fn verify_stage(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        csid: ChangesetId,
        stage: &StageId,
    ) -> Result<bool> {
        let actual = Self::fetch_stage_outputs(ctx, derivation, stage, vec![csid])
            .await?
            .remove(&csid)
            .context("missing filenodes stage output")?;
        let Some(actual) = actual else {
            return Ok(matches!(
                Self::fetch(ctx, derivation, csid).await?,
                Some(Self::Disabled)
            ));
        };
        let bonsai = csid.load(ctx, derivation.blobstore()).await?;
        let mut expected = generate_all_filenodes(ctx, derivation, &bonsai)
            .await?
            .into_iter()
            .filter(|record| covers(stage, &record.path))
            .collect::<Vec<_>>();
        sort_records(&mut expected);
        Ok(actual == expected)
    }
}
