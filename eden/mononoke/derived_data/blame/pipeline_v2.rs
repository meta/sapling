/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use context::CoreContext;
use derived_data_manager::BonsaiDerivable;
use derived_data_manager::DerivationContext;
use derived_data_manager::DerivationStagePayload;
use derived_data_manager::PipelineDerivable;
use derived_data_manager::StageId;
use derived_data_manager::StageOutputs;
use derived_data_manager::stage_outputs::fetch_stage_outputs;
use derived_data_manager::stage_outputs::store_stage_outputs;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use manifest::Entry;
use mononoke_macros::mononoke;
use mononoke_types::BlobstoreBytes;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::PipelineDerivableVariant;
use mononoke_types::blame_v2::load_blame_with_prefix;
use mononoke_types::path::MPath;
use unodes::RootUnodeManifestId;
use unodes::fetch_stage_unodes;
use unodes::find_new_canonical_unode_entries;
use unodes::find_new_stage_entries;
use unodes::find_stage_unode_rename_sources;

use crate::DEFAULT_BLAME_FILESIZE_LIMIT;
use crate::RootBlameV2;
use crate::derive_v2::create_blame_v2;
use crate::format_key;

/// Concurrency for per-file blame writes, matching the canonical
/// `derive_blame_v2` buffering.
const BLAME_BUFFER_SIZE: usize = 256;

fn use_normal_mapping(stage_path: &MPath) -> bool {
    stage_path.is_root()
        && justknobs::eval(
            "scm/mononoke:derived_data_pipeline_terminal_stage_prod_mapping",
            None,
            Some("blame"),
        )
}

/// Prefix for pipeline blame blob writes/reads, gated on the same prod-mapping
/// JustKnob as the terminal mapping. After the flip the pipeline writes the
/// canonical content-addressed key (empty prefix, becoming source of truth);
/// before it, blame is namespaced. Applies at every stage (blame blobs are
/// keyed by file unode, so there is no cross-stage key collision).
fn pipeline_blame_key_prefix() -> &'static str {
    if justknobs::eval(
        "scm/mononoke:derived_data_pipeline_terminal_stage_prod_mapping",
        None,
        Some("blame"),
    ) {
        ""
    } else {
        "pipeline."
    }
}

#[async_trait]
impl PipelineDerivable for RootBlameV2 {
    const PIPELINE_DERIVABLE_VARIANT: PipelineDerivableVariant = PipelineDerivableVariant::BlameV2;

    const HAS_FINALIZE: bool = false;

    type StageOutput = ();

    async fn derive_stage_batch(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsais: Vec<BonsaiChangeset>,
        payload: &DerivationStagePayload,
        _parents: HashMap<ChangesetId, Self::StageOutput>,
        _dependency_outputs: HashMap<ChangesetId, HashMap<StageId, Self::StageOutput>>,
    ) -> Result<HashMap<ChangesetId, Self::StageOutput>> {
        let stage = payload.stage_id();
        if matches!(stage, StageId::Finalize) {
            anyhow::bail!("{} has no finalize derive", Self::NAME);
        }

        let filesize_limit = derivation
            .config()
            .blame_filesize_limit
            .unwrap_or(DEFAULT_BLAME_FILESIZE_LIMIT);

        // Namespace pipeline blame blobs until the prod-mapping flip.
        let blame_key_prefix = pipeline_blame_key_prefix();

        let unode_outputs = fetch_stage_unodes(ctx, derivation, &stage, &bonsais).await?;

        let mut results = HashMap::new();

        for bonsai in &bonsais {
            let csid = bonsai.get_changeset_id();

            let own_output = unode_outputs[&csid].clone();
            let parent_stage_outputs = bonsai
                .parents()
                .map(|parent| (parent, unode_outputs[&parent].clone()))
                .collect();

            let renames = Arc::new(
                find_stage_unode_rename_sources(
                    ctx,
                    derivation,
                    payload,
                    bonsai,
                    &parent_stage_outputs,
                )
                .await?,
            );
            let changed = find_new_stage_entries(
                ctx,
                derivation,
                payload,
                own_output,
                parent_stage_outputs.into_values().collect(),
            )
            .await?
            .into_iter()
            .filter_map(|(path, entry)| {
                Some((path.into_optional_non_root_path()?, entry.into_leaf()?))
            });

            // Write the per-file blame concurrently, mirroring the canonical
            // `derive_blame_v2` (spawn each blame computation, buffered).
            // `create_blame_v2` looks up the rename for each file internally.
            stream::iter(changed)
                .map(|(abs_path, file_unode_id)| {
                    let ctx = ctx.clone();
                    let derivation = derivation.clone();
                    let renames = renames.clone();
                    async move {
                        mononoke::spawn_task(async move {
                            create_blame_v2(
                                &ctx,
                                &derivation,
                                derivation.blobstore(),
                                renames,
                                csid,
                                abs_path,
                                file_unode_id,
                                filesize_limit,
                                blame_key_prefix,
                            )
                            .await
                        })
                        .await?
                    }
                })
                .buffered(BLAME_BUFFER_SIZE)
                .try_for_each(|_| async { Ok::<_, Error>(()) })
                .await?;

            results.insert(csid, ());
        }

        Ok(results)
    }

    async fn extract_stage_output_from_derived(
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
        _derived: &RootBlameV2,
        _stage: &StageId,
    ) -> Result<Self::StageOutput> {
        Ok(())
    }

    async fn extract_stage_output_from_root(
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
        _root: (),
        _stage: &StageId,
    ) -> Result<()> {
        Ok(())
    }

    async fn store_stage_outputs(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        stage: &StageId,
        outputs: HashMap<ChangesetId, Self::StageOutput>,
    ) -> Result<Option<StageOutputs>> {
        if matches!(stage, StageId::Manifest(path) if use_normal_mapping(path)) {
            let mut unode_outputs = RootUnodeManifestId::fetch_stage_outputs(
                ctx,
                derivation,
                stage,
                outputs.keys().copied().collect(),
            )
            .await?;
            let roots = outputs
                .into_keys()
                .map(|csid| {
                    let output = unode_outputs.remove(&csid).ok_or_else(|| {
                        anyhow!("terminal stage for {csid}: missing unode root stage output")
                    })?;
                    let Some(Entry::Tree(root)) = output.into_entry()? else {
                        anyhow::bail!("terminal stage for {csid}: expected unode root");
                    };
                    Ok((csid, RootUnodeManifestId(root)))
                })
                .collect::<Result<HashMap<_, _>>>()?;
            return store_stage_outputs::<Self, _, _>(
                ctx,
                derivation,
                stage,
                roots,
                |csid| Some(format_key(derivation, csid)),
                async |root| Ok(root.into()),
            )
            .await;
        }
        store_stage_outputs::<Self, _, _>(
            ctx,
            derivation,
            stage,
            outputs,
            |_| None,
            async |()| Ok(BlobstoreBytes::empty()),
        )
        .await
    }

    async fn fetch_stage_outputs(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        stage: &StageId,
        cs_ids: Vec<ChangesetId>,
    ) -> Result<HashMap<ChangesetId, Self::StageOutput>> {
        let use_normal_mapping =
            matches!(stage, StageId::Manifest(path) if use_normal_mapping(path));
        fetch_stage_outputs::<Self, _, _>(
            ctx,
            derivation,
            stage,
            cs_ids,
            |csid| use_normal_mapping.then(|| format_key(derivation, csid)),
            async |_, _| Ok(()),
        )
        .await
    }

    async fn verify_stage(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        csid: ChangesetId,
        stage: &StageId,
    ) -> Result<bool> {
        if matches!(stage, StageId::Subentries(..)) {
            return Ok(true);
        }
        let StageId::Manifest(stage_path) = stage else {
            anyhow::bail!("{} has no finalize stage", Self::NAME);
        };
        // Blame is verified once per commit at the root stage (which runs last,
        // so all stages' blame is present); non-root stages are no-ops.
        if !stage_path.is_root() {
            return Ok(true);
        }

        let changed = find_new_canonical_unode_entries(ctx, derivation, csid)
            .await?
            .try_filter_map(|(path, entry)| async move {
                Ok(path.into_optional_non_root_path().and(entry.into_leaf()))
            })
            .try_collect::<Vec<_>>()
            .await?;

        // Data equality: per changed file, the pipeline-side blame blob must
        // match the canonical blame blob.
        let pipeline_prefix = pipeline_blame_key_prefix();
        for file_unode_id in changed {
            let (pipeline_blame, canonical_blame) = futures::future::try_join(
                load_blame_with_prefix(ctx, derivation.blobstore(), file_unode_id, pipeline_prefix),
                load_blame_with_prefix(ctx, derivation.blobstore(), file_unode_id, ""),
            )
            .await?;
            match (pipeline_blame, canonical_blame) {
                (Some(p), Some(c)) if p == c => {}
                _ => return Ok(false),
            }
        }

        Ok(true)
    }
}
