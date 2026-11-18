/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Inline execution of the derivation pipeline over a set of changesets.
//!
//! The pipeline splits derivation of a single derived-data type into
//! path-scoped stages derived in dependency order (deepest subtree first),
//! optionally followed by a finalize stage. This crate turns a
//! [`DerivationPipelineConfig`] plus a topologically-ordered list of changesets
//! into batches (split at chokepoints) and drives each stage/type via
//! [`bulk_derivation::derive_stage_batch`].
//!
//! It sits above `bulk_derivation` (which it calls) and is shared by the
//! derivation-pipeline test harness and the async-requests backfill worker so
//! there is a single, tested orchestration path.

use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::Result;
use blobstore::Loadable;
use bulk_derivation::BulkDerivation;
use context::CoreContext;
use derivation_pipeline_utils::Batch;
use derivation_pipeline_utils::topo_sort_types;
use derived_data_manager::DerivationError;
use derived_data_manager::DerivationStagePayload;
use derived_data_manager::DerivedDataManager;
use derived_data_manager::ManifestStagePayload;
use derived_data_manager::StageInputs;
use derived_data_manager::StageOutputs;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use metaconfig_types::DerivationPipelineConfig;
use metaconfig_types::DerivationPipelineStageConfig;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use mononoke_types::MPath;
use repo_blobstore::RepoBlobstore;

/// Bounded concurrency for loading bonsais during chokepoint classification.
const LOAD_BONSAIS_CONCURRENCY: usize = 100;

/// The order in which a pipeline run visits stages and types.
///
/// Stages are deepest-first: config validation guarantees each dependency is
/// exactly one path element deeper, so path-depth-descending is a valid
/// topological order over the stage DAG. Types are in dependency order so that,
/// within a stage, a type is derived only after the types it depends on.
pub struct PipelineOrder {
    pub sorted_stages: Vec<MPath>,
    pub sorted_types: Vec<DerivableType>,
}

/// Compute the deepest-first stage order and the dependency-topological type
/// order for `types`. Pure; performs no I/O.
pub fn plan_stages_and_types(
    manager: &DerivedDataManager,
    config: &DerivationPipelineConfig,
    types: &[DerivableType],
) -> Result<PipelineOrder> {
    let mut sorted_stages: Vec<MPath> = config.stages.keys().cloned().collect();
    sorted_stages.sort_by(|a, b| {
        b.num_components()
            .cmp(&a.num_components())
            .then_with(|| a.cmp(b))
    });
    Ok(PipelineOrder {
        sorted_stages,
        sorted_types: topo_sort_types(types.iter().copied().collect(), |ty| {
            manager.dependency_types(ty)
        })?,
    })
}

/// Split an already-forward-topological (parents-first) changeset list into
/// chokepoint-aware batches of at most `config.batch_size`.
///
/// IMPORTANT: `cs_ids` MUST be in forward topological order — every parent
/// before its children. The pipeline derives each batch's stages assuming all
/// out-of-batch parents are already derived; a reverse-ordered input would
/// derive children before their parents.
pub async fn plan_batches(
    ctx: &CoreContext,
    blobstore: &RepoBlobstore,
    config: &DerivationPipelineConfig,
    cs_ids: Vec<ChangesetId>,
) -> Result<Vec<Batch>> {
    let batch_size = config.batch_size.get() as usize;
    let raw_batches: Vec<Vec<ChangesetId>> = cs_ids.chunks(batch_size).map(<[_]>::to_vec).collect();
    let bonsais = stream::iter(cs_ids)
        .map(|cs_id| async move { anyhow::Ok((cs_id, cs_id.load(ctx, blobstore).await?)) })
        .buffer_unordered(LOAD_BONSAIS_CONCURRENCY)
        .try_collect::<HashMap<_, _>>()
        .await?;
    Ok(derivation_pipeline_utils::split_batches_at_chokepoints(
        raw_batches,
        &bonsais,
        config,
    ))
}

/// Bounded concurrency for deriving independent same-depth stages within one
/// phase. Stages of equal depth never depend on each other (config validation
/// guarantees a dependency is exactly one level deeper), so they can be derived
/// concurrently. This horizontal (per-subtree) parallelism is the only
/// parallelism available for serial (non-predecessor-derivable) types, whose
/// slices cannot be fanned out vertically.
const STAGE_PHASE_CONCURRENCY: usize = 16;

/// Derive a single stage for every type. Types are derived in dependency order
/// so that a type depending on another at the same stage (e.g. blame on unodes)
/// sees its dependency's stage output already available. Takes owned inputs so the
/// returned future is `Send + 'static` and can run concurrently within a phase
/// (a borrowed `&CoreContext` is not `Send`-general-enough for the worker's
/// `tokio::spawn`).
async fn derive_stage(
    manager: DerivedDataManager,
    ctx: CoreContext,
    stage: (MPath, DerivationPipelineStageConfig),
    sorted_types: Vec<DerivableType>,
    commits: Vec<ChangesetId>,
    mut references: Vec<StageOutputs>,
) -> Result<Vec<StageOutputs>, DerivationError> {
    let inherited = references.len();
    let payload = ManifestStagePayload::from_config(stage.0, &stage.1);
    let partitions = payload
        .partitions()
        .into_iter()
        .map(|range| DerivationStagePayload::Subentries {
            path: payload.path.clone(),
            range,
        })
        .collect::<Vec<_>>();
    let payload = DerivationStagePayload::Manifest(payload);
    for derivable_type in sorted_types {
        let variant = derivable_type.into_pipeline_derivable_variant()?;
        let inputs = StageInputs::new(&references)?;
        let supplied = manager.with_stage_inputs(inputs);
        let results = futures::future::try_join_all(partitions.iter().map(|partition| {
            bulk_derivation::derive_stage_batch(
                &supplied,
                &ctx,
                commits.clone(),
                partition,
                variant,
            )
        }))
        .await?;
        references.extend(results.into_iter().filter_map(|result| result.outputs));
        let inputs = StageInputs::new(&references)?;
        let result = bulk_derivation::derive_stage_batch(
            &manager.with_stage_inputs(inputs),
            &ctx,
            commits.clone(),
            &payload,
            variant,
        )
        .await?;
        references.extend(result.outputs);
    }
    Ok(references.split_off(inherited))
}

/// Execute a planned pipeline, reporting each batch's outputs to `completed`.
///
/// Batches run sequentially, with independent stages at each depth running
/// concurrently. Outputs are retained until their last parent or copy-source
/// consumer; `completed` can collect them separately for verification.
///
/// `batches` must be forward-topological (see [`plan_batches`]) and
/// `sorted_stages` deepest-first (see [`plan_stages_and_types`]).
pub async fn run_pipeline_batches(
    manager: &DerivedDataManager,
    ctx: &CoreContext,
    config: &DerivationPipelineConfig,
    sorted_stages: &[MPath],
    sorted_types: &[DerivableType],
    batches: &[Batch],
    mut completed: impl FnMut(&[StageOutputs]) + Send,
) -> Result<(), DerivationError> {
    let batch_ids: HashMap<_, _> = batches
        .iter()
        .enumerate()
        .flat_map(|(id, batch)| batch.commits.iter().map(move |csid| (*csid, id)))
        .collect();
    let mut dependencies = Vec::with_capacity(batches.len());
    let mut remaining_consumers = vec![0; batches.len()];
    for (id, batch) in batches.iter().enumerate() {
        let inputs = stream::iter(batch.commits.iter().copied())
            .map(|csid| async move { csid.load(ctx, manager.repo_blobstore()).await })
            .buffer_unordered(LOAD_BONSAIS_CONCURRENCY)
            .try_fold(HashSet::new(), |mut inputs, bonsai| {
                inputs.extend(
                    bonsai
                        .parents()
                        .chain(bonsai.subtree_sources())
                        .filter_map(|csid| batch_ids.get(&csid).copied())
                        .filter(|input| *input != id),
                );
                futures::future::ready(Ok(inputs))
            })
            .await
            .map_err(anyhow::Error::from)?;
        for &input in &inputs {
            remaining_consumers[input] += 1;
        }
        dependencies.push(inputs);
    }

    let mut outputs: HashMap<usize, Vec<StageOutputs>> = HashMap::new();
    for (id, (batch, dependencies)) in batches.iter().zip(dependencies).enumerate() {
        let mut references = Vec::new();
        for dependency in dependencies {
            remaining_consumers[dependency] -= 1;
            if remaining_consumers[dependency] == 0 {
                references.extend(outputs.remove(&dependency).unwrap());
            } else {
                references.extend(outputs[&dependency].iter().cloned());
            }
        }
        let inherited = references.len();
        // `sorted_stages` is deepest-first, so equal-depth stages are contiguous;
        // each chunk is one phase of mutually-independent stages. Build each
        // stage's payload synchronously (borrowing `config`), then hand
        // fully-owned inputs to the concurrent futures so the buffered stream is
        // `Send + 'static`.
        for phase in sorted_stages.chunk_by(|a, b| a.num_components() == b.num_components()) {
            let stage_futures = phase
                .iter()
                .map(|stage_path| {
                    let stage = (stage_path.clone(), config.stages[stage_path].clone());
                    derive_stage(
                        manager.clone(),
                        ctx.clone(),
                        stage,
                        sorted_types.to_vec(),
                        batch.commits.clone(),
                        references.clone(),
                    )
                })
                .collect::<Vec<_>>();
            let phase_outputs = stream::iter(stage_futures)
                .buffer_unordered(STAGE_PHASE_CONCURRENCY)
                .try_collect::<Vec<_>>()
                .await?;
            references.extend(phase_outputs.into_iter().flatten());
        }

        // Finalize after every manifest stage of the batch completes.
        for &derivable_type in sorted_types {
            let variant = derivable_type.into_pipeline_derivable_variant()?;
            if !bulk_derivation::pipeline_has_finalize(variant) {
                continue;
            }
            let inputs = StageInputs::new(&references)?;
            let result = bulk_derivation::derive_stage_batch(
                &manager.with_stage_inputs(inputs),
                ctx,
                batch.commits.clone(),
                &DerivationStagePayload::Finalize,
                variant,
            )
            .await?;
            references.extend(result.outputs);
        }
        let batch_outputs = references.split_off(inherited);
        completed(&batch_outputs);
        if remaining_consumers[id] != 0 {
            outputs.insert(id, batch_outputs);
        }
    }
    Ok(())
}
