/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;

use anyhow::Error;
use anyhow::Result;
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
use mononoke_macros::mononoke;
use mononoke_types::BlobstoreBytes;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::PipelineDerivableVariant;
use mononoke_types::path::MPath;
use unodes::fetch_stage_unodes;
use unodes::find_new_canonical_unode_entries;
use unodes::find_new_stage_entries;

use crate::RootFastlog;
use crate::fastlog_impl::create_new_batch_with_prefix;
use crate::fastlog_impl::fetch_fastlog_batch_by_unode_id_with_prefix;
use crate::fastlog_impl::fetch_unode_parents;
use crate::fastlog_impl::save_fastlog_batch_by_unode_id_with_prefix;
use crate::mapping::format_key;

/// Concurrency for per-unode fastlog batch writes within a single commit,
/// matching the canonical `derive_single` buffering.
const FASTLOG_BUFFER_SIZE: usize = 100;

fn use_normal_mapping(stage_path: &MPath) -> bool {
    stage_path.is_root()
        && justknobs::eval(
            "scm/mononoke:derived_data_pipeline_terminal_stage_prod_mapping",
            None,
            Some("fastlog"),
        )
}

/// Prefix for pipeline fastlog batch blob writes/reads, gated on the same
/// prod-mapping JustKnob as the terminal mapping. After the flip the pipeline
/// writes the canonical content-addressed key (empty prefix, becoming source of
/// truth); before it, batches are namespaced. Applies at every stage (fastlog
/// batches are keyed by unode hash, so there is no cross-stage key collision).
fn pipeline_fastlog_key_prefix() -> &'static str {
    if justknobs::eval(
        "scm/mononoke:derived_data_pipeline_terminal_stage_prod_mapping",
        None,
        Some("fastlog"),
    ) {
        ""
    } else {
        "pipeline."
    }
}

#[async_trait]
impl PipelineDerivable for RootFastlog {
    const PIPELINE_DERIVABLE_VARIANT: PipelineDerivableVariant = PipelineDerivableVariant::Fastlog;

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

        // Namespace pipeline fastlog batches until the prod-mapping flip.
        let fastlog_key_prefix = pipeline_fastlog_key_prefix();

        let unode_outputs = fetch_stage_unodes(ctx, derivation, &stage, &bonsais).await?;

        let mut results = HashMap::new();

        // Process commits in the given topological order: per-commit batches are
        // written before moving on, so an intra-batch ancestor's batch is
        // available when chaining its descendant within stage S.
        for bonsai in &bonsais {
            let csid = bonsai.get_changeset_id();

            let changed = find_new_stage_entries(
                ctx,
                derivation,
                payload,
                unode_outputs[&csid].clone(),
                bonsai
                    .parents()
                    .map(|parent| unode_outputs[&parent].clone())
                    .collect(),
            )
            .await?
            .into_iter()
            .map(|(_, entry)| entry);

            // Write the per-unode batches concurrently, chaining onto parent
            // unode batches (pipeline prefix first, canonical fallback).
            stream::iter(changed)
                .map(|entry| {
                    let ctx = ctx.clone();
                    let derivation = derivation.clone();
                    async move {
                        mononoke::spawn_task(async move {
                            let blobstore = derivation.blobstore();
                            let parents = fetch_unode_parents(&ctx, blobstore, entry).await?;
                            let batch = create_new_batch_with_prefix(
                                &ctx,
                                blobstore,
                                parents,
                                csid,
                                fastlog_key_prefix,
                            )
                            .await?;
                            save_fastlog_batch_by_unode_id_with_prefix(
                                &ctx,
                                blobstore,
                                entry,
                                batch,
                                fastlog_key_prefix,
                            )
                            .await
                        })
                        .await?
                    }
                })
                .buffered(FASTLOG_BUFFER_SIZE)
                .try_for_each(|_| async { Ok::<_, Error>(()) })
                .await?;

            results.insert(csid, ());
        }

        Ok(results)
    }

    async fn extract_stage_output_from_derived(
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
        _derived: &RootFastlog,
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
        let use_normal_mapping =
            matches!(stage, StageId::Manifest(path) if use_normal_mapping(path));
        store_stage_outputs::<Self, _, _>(
            ctx,
            derivation,
            stage,
            outputs,
            |csid| use_normal_mapping.then(|| format_key(derivation, csid)),
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
        // Fastlog is verified once per commit at the root stage (which runs
        // last, so all stages' batches are present); non-root stages are no-ops.
        if !stage_path.is_root() {
            return Ok(true);
        }

        let changed = find_new_canonical_unode_entries(ctx, derivation, csid)
            .await?
            .try_collect::<Vec<_>>()
            .await?;

        // Data equality: per changed unode, the pipeline-side batch blob must
        // match the canonical batch blob.
        let pipeline_prefix = pipeline_fastlog_key_prefix();
        let blobstore = derivation.blobstore();
        for (_path, entry) in changed {
            let (pipeline_batch, canonical_batch) = futures::future::try_join(
                fetch_fastlog_batch_by_unode_id_with_prefix(
                    ctx,
                    blobstore,
                    &entry,
                    pipeline_prefix,
                ),
                fetch_fastlog_batch_by_unode_id_with_prefix(ctx, blobstore, &entry, ""),
            )
            .await?;
            match (pipeline_batch, canonical_batch) {
                (Some(p), Some(c)) if p == c => {}
                _ => return Ok(false),
            }
        }

        Ok(true)
    }
}
