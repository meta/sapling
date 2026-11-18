/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::num::NonZeroU32;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Error;
use anyhow::Result;
use async_requests::AsyncMethodRequestQueue;
use async_requests::AsyncRequestsError;
use async_requests::types::RowId;
use async_requests::types::Token;
use bulk_derivation::BulkDerivation;
use commit_graph::CommitGraphRef;
use commit_graph::SegmentedSliceWithBoundaries;
use context::CoreContext;
use derived_data_manager::DerivedDataManager;
use enabled_derived_data_types::EnabledDerivedDataTypesRef;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use futures_retry::retry;
use futures_stats::TimedTryFutureExt;
use metaconfig_types::DerivationPipelineConfig;
use mononoke_api::Mononoke;
use mononoke_api::Repo;
use mononoke_api::RepoContext;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use mononoke_types::RepositoryId;
use repo_authorization::AuthorizationContext;
use repo_blobstore::RepoBlobstore;
use repo_derived_data::RepoDerivedData;
use repo_derived_data::RepoDerivedDataRef;
use source_control as thrift;
use throttledblob::ThrottleOptions;
use throttledblob::ThrottledBlob;
use tracing::info;
use tracing::warn;

/// JustKnob for write QPS throttling during backfill derivation
const JK_BACKFILL_WRITE_QPS: &str = "scm/mononoke:derived_data_backfill_write_qps";
/// JustKnob for write bytes/s throttling during backfill derivation
const JK_BACKFILL_WRITE_BYTES_S: &str = "scm/mononoke:derived_data_backfill_write_bytes_s";
/// JustKnob for read QPS throttling during backfill derivation
const JK_BACKFILL_READ_QPS: &str = "scm/mononoke:derived_data_backfill_read_qps";
/// JustKnob for read bytes/s throttling during backfill derivation
const JK_BACKFILL_READ_BYTES_S: &str = "scm/mononoke:derived_data_backfill_read_bytes_s";

/// JustKnob for chunk size when deriving slices of commits
const JK_BACKFILL_SEGMENT_CHUNK_SIZE: &str =
    "scm/mononoke:derived_data_backfill_segment_chunk_size";

/// JustKnob for per-chunk retry limit during slice derivation
const JK_BACKFILL_CHUNK_RETRY_LIMIT: &str = "scm/mononoke:derived_data_backfill_chunk_retry_limit";

/// JustKnob (repo-scoped) gating use of the derivation pipeline for slice
/// backfill. Off by default; when off, slices derive via the canonical path so
/// enabling/rolling back is instant and per-repo.
const JK_BACKFILL_USE_PIPELINE: &str = "scm/mononoke:derived_data_backfill_use_pipeline";
const CHUNK_RETRY_BASE_BACKOFF: Duration = Duration::from_secs(1);
const CHUNK_RETRY_MAX_BACKOFF: Duration = Duration::from_secs(30);

/// JustKnob gating opportunistic boundary completion: when enabled, each
/// boundary's predecessor derivation races against the boundary being derived
/// by another task (e.g. a concurrent slice), so a slow predecessor derivation
/// is abandoned as soon as it becomes redundant.
const JK_BACKFILL_OPPORTUNISTIC_BOUNDARIES: &str =
    "scm/mononoke:derived_data_backfill_opportunistic_boundary_completion";

/// Poll interval for noticing that a boundary changeset has been derived by
/// another task (e.g. a concurrently-running slice), so we can abandon its
/// now-redundant predecessor derivation.
const BOUNDARY_DERIVED_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Get a DerivedDataManager with optional read/write throttling applied.
/// If JustKnobs are not set or are zero, returns the manager unchanged.
fn get_throttled_manager(manager: &DerivedDataManager) -> Result<DerivedDataManager> {
    let write_qps = justknobs::get_as::<i64>(JK_BACKFILL_WRITE_QPS, None);
    let write_bytes = justknobs::get_as::<i64>(JK_BACKFILL_WRITE_BYTES_S, None);
    let read_qps = justknobs::get_as::<i64>(JK_BACKFILL_READ_QPS, None);
    let read_bytes = justknobs::get_as::<i64>(JK_BACKFILL_READ_BYTES_S, None);

    if write_qps <= 0 && write_bytes <= 0 && read_qps <= 0 && read_bytes <= 0 {
        return Ok(manager.clone());
    }

    let options = ThrottleOptions {
        write_qps: NonZeroU32::new(write_qps.max(0) as u32),
        write_bytes: NonZeroUsize::new(write_bytes.max(0) as usize),
        read_qps: NonZeroU32::new(read_qps.max(0) as u32),
        read_bytes: NonZeroUsize::new(read_bytes.max(0) as usize),
        ..Default::default()
    };

    info!(
        "Applying throttle: write_qps={:?}, write_bytes/s={:?}, read_qps={:?}, read_bytes/s={:?}",
        options.write_qps, options.write_bytes, options.read_qps, options.read_bytes,
    );

    let repo_blobstore = manager.repo_blobstore().clone();
    let throttled_blobstore =
        RepoBlobstore::new_with_wrapped_inner_blobstore(repo_blobstore, |inner| {
            Arc::new(ThrottledBlob::new(inner, options))
        });
    Ok(manager.with_replaced_blobstore(throttled_blobstore))
}

/// Returns a manager with the given derived data type enabled in its config, and
/// optionally deriving under `mapping_key_prefix` instead of the config's own prefix.
///
/// Enabling the type in-memory is what lets a backfill run before the type is in the
/// repo config. Overriding the prefix serves the other half of a version rollout: the
/// new data lands in a fresh key namespace (`derived_root_<ty>.<prefix><csid>`) that
/// production, still reading the config's prefix, cannot see. The prefix must be
/// applied to *every* manager in the campaign — including the one used for slicing,
/// which decides what counts as already-derived — or the run silently mixes namespaces.
///
/// Returns a clone of the existing manager when there is nothing to change.
fn with_type_enabled(
    manager: &DerivedDataManager,
    derived_data_type: DerivableType,
    mapping_key_prefix: Option<&str>,
) -> DerivedDataManager {
    let type_already_enabled = manager.config().types.contains(&derived_data_type);
    // `mapping_key_prefix()` treats absent and empty as the same thing, so compare
    // against the effective prefix rather than the map entry.
    let prefix_already_set = mapping_key_prefix.is_none_or(|prefix| {
        manager
            .config()
            .mapping_key_prefixes
            .get(&derived_data_type)
            .map_or("", String::as_str)
            == prefix
    });
    if type_already_enabled && prefix_already_set {
        return manager.clone();
    }
    let mut config = manager.config().clone();
    config.types.insert(derived_data_type);
    if let Some(prefix) = mapping_key_prefix {
        config
            .mapping_key_prefixes
            .insert(derived_data_type, prefix.to_string());
    }
    manager.with_replaced_config(manager.config_name(), config)
}

/// Get the derived data manager for an optional config name.
/// If `config_name` is provided, returns the manager for that specific configuration.
/// Otherwise returns the default manager.
fn resolve_manager<'a>(
    repo_derived_data: &'a RepoDerivedData,
    config_name: Option<&str>,
) -> Result<&'a DerivedDataManager, AsyncRequestsError> {
    match config_name {
        Some(name) => repo_derived_data
            .manager_for_config(name)
            .map_err(AsyncRequestsError::request),
        None => Ok(repo_derived_data.manager()),
    }
}

/// Returns true if this derived data type requires slices to be chained
/// serially (each slice depends on the previous). Types that support
/// derive_from_predecessor can derive boundaries independently, allowing
/// parallel slice processing.
fn requires_serial_slice_processing(derived_data_type: DerivableType) -> bool {
    derived_data_type
        .into_derivable_untopologically_variant()
        .is_err()
}

/// Resolves once `csid` has `derived_data_type` derived by any means, polling
/// periodically. Used to race against predecessor derivation: if a concurrent
/// slice derives the boundary first, we can abandon the slower
/// `derive_from_predecessor` call. Transient lookup errors are ignored and
/// retried, since predecessor derivation remains the guaranteed backstop.
async fn wait_until_derived(
    manager: &DerivedDataManager,
    ctx: &CoreContext,
    csid: ChangesetId,
    derived_data_type: DerivableType,
) {
    loop {
        tokio::time::sleep(BOUNDARY_DERIVED_POLL_INTERVAL).await;
        if let Ok(true) =
            BulkDerivation::is_derived(manager, ctx, csid, None, derived_data_type).await
        {
            return;
        }
    }
}

/// Compute derive_boundaries request - derives boundary changesets using predecessor derivation
pub(crate) async fn compute_derive_boundaries(
    ctx: &CoreContext,
    mononoke: Arc<Mononoke<Repo>>,
    params: thrift::DeriveBoundariesParams,
) -> Result<thrift::DeriveBoundariesResponse, AsyncRequestsError> {
    let repo_id = RepositoryId::new(
        params
            .repo_id
            .try_into()
            .map_err(|e| AsyncRequestsError::request(anyhow::anyhow!("Invalid repo_id: {e}")))?,
    );

    let repo = mononoke
        .repo_by_id(ctx.clone(), repo_id)
        .await
        .map_err(AsyncRequestsError::internal)?
        .ok_or_else(|| {
            AsyncRequestsError::request(anyhow::anyhow!("Repo not found: {}", params.repo_id))
        })?
        .with_authorization_context(AuthorizationContext::new_bypass_access_control())
        .build()
        .await
        .map_err(AsyncRequestsError::internal)?;

    let derived_data_type =
        DerivableType::from_name(&params.derived_data_type).map_err(AsyncRequestsError::request)?;

    let boundary_cs_ids: Vec<ChangesetId> = params
        .boundary_cs_ids
        .iter()
        .map(ChangesetId::from_bytes)
        .collect::<Result<Vec<_>, _>>()
        .map_err(AsyncRequestsError::request)?;

    info!(
        "Deriving {} boundary changesets for repo {} type {:?} config_name {:?} mapping_key_prefix {:?}",
        boundary_cs_ids.len(),
        params.repo_id,
        derived_data_type,
        params.config_name,
        params.mapping_key_prefix,
    );

    let derived_count = Arc::new(AtomicUsize::new(0));
    let base_manager = resolve_manager(
        repo.repo().repo_derived_data(),
        params.config_name.as_deref(),
    )?;
    let manager = get_throttled_manager(base_manager).map_err(AsyncRequestsError::internal)?;
    let manager = with_type_enabled(
        &manager,
        derived_data_type,
        params.mapping_key_prefix.as_deref(),
    );
    let concurrency = params.concurrency.max(1) as usize;
    let use_predecessor = params.use_predecessor_derivation;
    let opportunistic = justknobs::eval(JK_BACKFILL_OPPORTUNISTIC_BOUNDARIES, None, None);

    stream::iter(boundary_cs_ids)
        .map(Ok::<_, Error>)
        .try_for_each_concurrent(concurrency, |csid| {
            let manager = manager.clone();
            let ctx = ctx.clone();
            let derived_count = derived_count.clone();
            async move {
                if use_predecessor {
                    let derive = BulkDerivation::unsafe_derive_untopologically(
                        &manager,
                        &ctx,
                        csid,
                        None, // rederivation
                        derived_data_type,
                    );
                    if opportunistic {
                        // Race predecessor derivation against the boundary becoming
                        // derived by another task (e.g. a concurrently-running slice
                        // whose head is this boundary). Whichever happens first
                        // satisfies the boundary; if it's derived elsewhere we drop
                        // the now-redundant (and possibly slow) predecessor future.
                        tokio::select! {
                            biased;
                            res = derive => {
                                res?;
                            }
                            _ = wait_until_derived(&manager, &ctx, csid, derived_data_type) => {}
                        }
                    } else {
                        derive.await?;
                    }
                } else {
                    manager
                        .derive_bulk_locally(
                            &ctx,
                            &[csid],
                            None, // rederivation
                            &[derived_data_type],
                            None, // override_batch_size
                            None, // override_concurrency
                        )
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                }
                derived_count.fetch_add(1, Ordering::SeqCst);
                Ok::<_, Error>(())
            }
        })
        .await
        .map_err(AsyncRequestsError::internal)?;

    let count = derived_count.load(Ordering::SeqCst) as i64;
    info!("Derived {} boundary changesets", count);

    Ok(thrift::DeriveBoundariesResponse {
        derived_count: count,
        error_message: None,
        ..Default::default()
    })
}

/// Decide whether slice backfill should derive `derived_data_type` via the
/// derivation pipeline for this repo. Returns the repo's pipeline config when
/// all of the following hold, else `None` (canonical path):
///   - the repo has a pipeline config whose managed `types` include the type,
///   - the type actually supports pipeline derivation, and
///   - the repo-scoped rollout JustKnob is enabled.
fn should_use_pipeline<'a>(
    manager: &'a DerivedDataManager,
    derived_data_type: DerivableType,
) -> Option<&'a DerivationPipelineConfig> {
    let config = manager.pipeline_config()?;
    if !config.types.contains(&derived_data_type) {
        return None;
    }
    if derived_data_type.into_pipeline_derivable_variant().is_err() {
        return None;
    }
    if !justknobs::eval(JK_BACKFILL_USE_PIPELINE, None, Some(manager.repo_name())) {
        return None;
    }
    Some(config)
}

/// Derive a slice's segments via the derivation pipeline instead of the
/// canonical path. Each segment is enumerated, flipped to forward-topological
/// order, split into chokepoint-aware batches, then derived stage-by-stage
/// (deepest first) via the shared `derivation_pipeline_driver`. Segments are
/// processed sequentially to preserve topological ordering, and each segment's
/// batches carry the same per-chunk retry/backoff as the canonical path.
///
/// Returns the number of changesets derived across all segments.
async fn derive_slice_pipeline(
    ctx: &CoreContext,
    manager: &DerivedDataManager,
    pipeline_config: &DerivationPipelineConfig,
    derived_data_type: DerivableType,
    segments: Vec<(ChangesetId, ChangesetId)>,
) -> Result<i64, AsyncRequestsError> {
    let order = derivation_pipeline_driver::plan_stages_and_types(
        manager,
        pipeline_config,
        &[derived_data_type],
    )
    .map_err(AsyncRequestsError::internal)?;
    // Reuse the (possibly throttled) manager's blobstore and commit graph.
    let commit_graph = manager.commit_graph();
    let blobstore = manager.repo_blobstore();

    let max_attempts =
        (justknobs::get_as::<i64>(JK_BACKFILL_CHUNK_RETRY_LIMIT, None).max(0) as usize) + 1;

    let mut derived_count: i64 = 0;
    for (base, head) in segments {
        // `range_stream` yields the segment in forward topological order
        // (parents before children), which is exactly what the pipeline needs:
        // each batch is derived assuming its out-of-batch parents are already
        // derived.
        let segment_cs_ids: Vec<ChangesetId> = commit_graph
            .range_stream(ctx, base, head)
            .await
            .map_err(AsyncRequestsError::internal)?
            .collect()
            .await;
        let count = segment_cs_ids.len() as i64;

        let batches = derivation_pipeline_driver::plan_batches(
            ctx,
            blobstore,
            pipeline_config,
            segment_cs_ids,
        )
        .await
        .map_err(AsyncRequestsError::internal)?;

        retry(
            |_attempt| {
                derivation_pipeline_driver::run_pipeline_batches(
                    manager,
                    ctx,
                    pipeline_config,
                    &order.sorted_stages,
                    &order.sorted_types,
                    &batches,
                    |_| {},
                )
            },
            CHUNK_RETRY_BASE_BACKOFF,
        )
        .binary_exponential_backoff()
        .max_interval(CHUNK_RETRY_MAX_BACKOFF)
        .max_attempts(max_attempts)
        .inspect_err(|attempt, err| {
            warn!(
                "Pipeline slice derivation failed (attempt {}/{}, type {:?}): {:#}",
                attempt, max_attempts, derived_data_type, err,
            );
        })
        .await
        .map(|(_result, _attempt)| ())
        .map_err(AsyncRequestsError::internal)?;

        derived_count += count;
    }

    Ok(derived_count)
}

/// Compute derive_slice request - derives a slice of commits (segments defined by head..base ranges)
pub(crate) async fn compute_derive_slice(
    ctx: &CoreContext,
    mononoke: Arc<Mononoke<Repo>>,
    params: thrift::DeriveSliceParams,
) -> Result<thrift::DeriveSliceResponse, AsyncRequestsError> {
    let repo_id = RepositoryId::new(
        params
            .repo_id
            .try_into()
            .map_err(|e| AsyncRequestsError::request(anyhow::anyhow!("Invalid repo_id: {e}")))?,
    );

    let repo = mononoke
        .repo_by_id(ctx.clone(), repo_id)
        .await
        .map_err(AsyncRequestsError::internal)?
        .ok_or_else(|| {
            AsyncRequestsError::request(anyhow::anyhow!("Repo not found: {}", params.repo_id))
        })?
        .with_authorization_context(AuthorizationContext::new_bypass_access_control())
        .build()
        .await
        .map_err(AsyncRequestsError::internal)?;

    let derived_data_type =
        DerivableType::from_name(&params.derived_data_type).map_err(AsyncRequestsError::request)?;

    info!(
        "Deriving slice with {} segments for repo {} type {:?} config_name {:?} mapping_key_prefix {:?}",
        params.segments.len(),
        params.repo_id,
        derived_data_type,
        params.config_name,
        params.mapping_key_prefix,
    );

    let base_manager = resolve_manager(
        repo.repo().repo_derived_data(),
        params.config_name.as_deref(),
    )?;
    let manager = get_throttled_manager(base_manager).map_err(AsyncRequestsError::internal)?;
    let manager = with_type_enabled(
        &manager,
        derived_data_type,
        params.mapping_key_prefix.as_deref(),
    );

    let segment_chunk_size =
        justknobs::get_as::<i64>(JK_BACKFILL_SEGMENT_CHUNK_SIZE, None) as usize;

    // Derive each segment by explicitly enumerating all changesets from base
    // to head, then deriving them in batches.  Segments are processed
    // sequentially to preserve topological ordering — a segment may depend on
    // changesets derived by a preceding segment in the same slice.
    let commit_graph = repo.repo().commit_graph();
    let mut derived_count: i64 = 0;

    let segments: Vec<(ChangesetId, ChangesetId)> = params
        .segments
        .iter()
        .map(|seg| {
            let base = ChangesetId::from_bytes(&seg.base)?;
            let head = ChangesetId::from_bytes(&seg.head)?;
            anyhow::Ok((base, head))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(AsyncRequestsError::request)?;

    // Pipeline path: when the repo+type is pipeline-enabled (and the rollout JK
    // is on), derive each segment via the derivation pipeline instead of the
    // canonical batch path.
    if let Some(pipeline_config) = should_use_pipeline(&manager, derived_data_type) {
        info!(
            "Deriving slice via derivation pipeline for repo {} type {:?}",
            params.repo_id, derived_data_type,
        );
        let derived_count =
            derive_slice_pipeline(ctx, &manager, pipeline_config, derived_data_type, segments)
                .await?;
        return Ok(thrift::DeriveSliceResponse {
            derived_count,
            error_message: None,
            ..Default::default()
        });
    }

    let max_attempts =
        (justknobs::get_as::<i64>(JK_BACKFILL_CHUNK_RETRY_LIMIT, None).max(0) as usize) + 1;

    for (base, head) in segments {
        let segment_cs_ids: Vec<ChangesetId> = commit_graph
            .range_stream(ctx, base, head)
            .await
            .map_err(AsyncRequestsError::internal)?
            .collect()
            .await;

        for chunk in segment_cs_ids.chunks(segment_chunk_size) {
            retry(
                |_attempt| {
                    BulkDerivation::derive_exactly_underived_batch(
                        &manager,
                        ctx,
                        chunk,
                        None,
                        derived_data_type,
                    )
                },
                CHUNK_RETRY_BASE_BACKOFF,
            )
            .binary_exponential_backoff()
            .max_interval(CHUNK_RETRY_MAX_BACKOFF)
            .max_attempts(max_attempts)
            .inspect_err(|attempt, err| {
                warn!(
                    "Chunk derivation failed (attempt {}/{}, repo {}, type {:?}): {:#}",
                    attempt, max_attempts, params.repo_id, derived_data_type, err,
                );
            })
            .await
            .map(|(_result, _attempt)| ())
            .map_err(AsyncRequestsError::internal)?;
        }
        derived_count += segment_cs_ids.len() as i64;
    }

    Ok(thrift::DeriveSliceResponse {
        derived_count,
        error_message: None,
        ..Default::default()
    })
}

/// Compute derive_backfill_repo request - computes slices and boundaries for a single repo and enqueues sub-requests.
pub(crate) async fn compute_derive_backfill_repo(
    ctx: &CoreContext,
    mononoke: Arc<Mononoke<Repo>>,
    queue: &AsyncMethodRequestQueue,
    params: thrift::DeriveBackfillRepoParams,
    root_request_id: RowId,
    created_by: Option<String>,
) -> Result<thrift::DeriveBackfillRepoResponse, AsyncRequestsError> {
    let repo_id = RepositoryId::new(
        params
            .repo_id
            .try_into()
            .map_err(|e| AsyncRequestsError::request(anyhow::anyhow!("Invalid repo_id: {e}")))?,
    );

    let repo = mononoke
        .repo_by_id(ctx.clone(), repo_id)
        .await
        .map_err(AsyncRequestsError::internal)?
        .ok_or_else(|| {
            AsyncRequestsError::request(anyhow::anyhow!("Repo not found: {}", params.repo_id))
        })?
        .with_authorization_context(AuthorizationContext::new_bypass_access_control())
        .build()
        .await
        .map_err(AsyncRequestsError::internal)?;

    let cs_ids: Vec<ChangesetId> = params
        .cs_ids
        .iter()
        .map(ChangesetId::from_bytes)
        .collect::<Result<Vec<_>, _>>()
        .map_err(AsyncRequestsError::request)?;

    let derived_data_type =
        DerivableType::from_name(&params.derived_data_type).map_err(AsyncRequestsError::request)?;

    let slice_size = params.slice_size.max(1) as u64;

    // MarkTypeEnabled records enablement for the reconcile tool, which writes the type
    // into config but has no way to write a mapping key prefix. Auto-enabling a
    // prefixed backfill would therefore land a config that reads the *unprefixed*
    // namespace, hiding everything just derived. Reject the combination rather than
    // enable a repo into data it cannot see.
    if params.auto_enable && params.mapping_key_prefix.is_some() {
        return Err(AsyncRequestsError::request(anyhow::anyhow!(
            "auto_enable cannot be combined with mapping_key_prefix ({:?}): the enablement \
             path does not write mapping_key_prefixes, so the enabled config would point at \
             a different key namespace than the backfill. Enable the type in config manually \
             with the same prefix instead.",
            params.mapping_key_prefix,
        )));
    }

    let enqueued_row_ids = process_repo_backfill(
        ctx,
        &repo,
        queue,
        derived_data_type,
        cs_ids,
        slice_size,
        params.reslice,
        params.boundaries_concurrency,
        params.num_boundary_requests,
        params.config_name.as_deref(),
        params.mapping_key_prefix.as_deref(),
        &repo_id,
        &root_request_id,
        created_by.as_deref(),
    )
    .await?;

    let total_sub_requests = enqueued_row_ids.len() as i64;

    // If auto-enable is requested, enqueue a MarkTypeEnabled node that depends on
    // the union of this repo's boundary + slice leaves. It runs (a trivial DB
    // write recording the type as enabled) only once every leaf succeeds; a
    // failed leaf cascades to it, leaving the repo unenabled. Empty deps (repo
    // had nothing to derive) => immediately eligible, so enablement is still
    // recorded. Uses the same IfAbsent enqueue path as slices
    // (`enqueue_with_dependencies_and_root`, keyed on
    // (root_request_id, request_type, repo_id, args_blobstore_key)), so a
    // re-enqueue under the same campaign is an idempotent no-op.
    if params.auto_enable {
        let mark_params = thrift::MarkTypeEnabledParams {
            repo_id: params.repo_id,
            derived_data_type: params.derived_data_type.clone(),
            ..Default::default()
        };
        queue
            .enqueue_with_dependencies_and_root(
                ctx,
                Some(&repo_id),
                mark_params,
                &enqueued_row_ids,
                &root_request_id,
                created_by.as_deref(),
            )
            .await
            .map_err(AsyncRequestsError::internal)?;
        tracing::debug!(
            "Enqueued MarkTypeEnabled for repo {} type {:?} depending on {} leaves",
            repo_id.id(),
            derived_data_type,
            enqueued_row_ids.len(),
        );
    }

    Ok(thrift::DeriveBackfillRepoResponse {
        total_sub_requests,
        error_message: None,
        ..Default::default()
    })
}

/// Concurrency bound for enqueueing per-repo sub-requests in the
/// "schedule everything at once" path. This only bounds cheap SQL inserts (not
/// derivation), but avoids the previously unbounded fan-out.
const REPO_ENQUEUE_CONCURRENCY: usize = 100;

/// Enqueue a single `DeriveBackfillRepo` sub-request for `entry`, tying it to
/// the backfill root so all of its descendant boundary/slice requests are
/// tracked under `root_request_id`. Shared by the fan-out-all path and the
/// concurrency-limited scheduler.
pub(crate) async fn enqueue_repo_backfill(
    ctx: &CoreContext,
    queue: &AsyncMethodRequestQueue,
    params: &thrift::DeriveBackfillParams,
    entry: &thrift::DeriveBackfillRepoEntry,
    root_request_id: &RowId,
    created_by: Option<&str>,
) -> Result<(), AsyncRequestsError> {
    let repo_id = RepositoryId::new(
        entry
            .repo_id
            .try_into()
            .map_err(|e| AsyncRequestsError::request(anyhow::anyhow!("Invalid repo_id: {e}")))?,
    );

    let repo_params = thrift::DeriveBackfillRepoParams {
        repo_id: entry.repo_id,
        derived_data_type: params.derived_data_type.clone(),
        cs_ids: entry.cs_ids.clone(),
        slice_size: params.slice_size,
        boundaries_concurrency: params.boundaries_concurrency,
        num_boundary_requests: params.num_boundary_requests,
        config_name: params.config_name.clone(),
        reslice: params.reslice,
        auto_enable: params.auto_enable.unwrap_or(false),
        mapping_key_prefix: params.mapping_key_prefix.clone(),
        ..Default::default()
    };

    queue
        .enqueue_with_root(
            ctx,
            Some(&repo_id),
            repo_params,
            root_request_id,
            created_by,
        )
        .await
        .map_err(AsyncRequestsError::internal)?;

    Ok(())
}

/// Compute mark_type_enabled request - records, via a trivial DB write, that a
/// derived data type is now enabled for a single repo.
///
/// This runs only after the repo's backfill leaves have all succeeded (the node
/// is cascade-dependent on them; a failed leaf cascades to `failed` and this
/// never runs, leaving the repo unenabled for a later campaign to retry). The
/// write is idempotent via the facet's upsert, so a re-run is a no-op.
///
/// No config precondition is asserted here: a missing type config surfaces as a
/// backfill derivation failure that cascades to this node (see design section
/// 5.2), so an explicit check would be redundant.
pub(crate) async fn compute_mark_type_enabled(
    ctx: &CoreContext,
    mononoke: Arc<Mononoke<Repo>>,
    _queue: &AsyncMethodRequestQueue,
    params: thrift::MarkTypeEnabledParams,
    root_request_id: RowId,
    _created_by: Option<String>,
) -> Result<thrift::MarkTypeEnabledResponse, AsyncRequestsError> {
    let repo_id = RepositoryId::new(
        params
            .repo_id
            .try_into()
            .map_err(|e| AsyncRequestsError::request(anyhow::anyhow!("Invalid repo_id: {e}")))?,
    );

    let repo = mononoke
        .repo_by_id(ctx.clone(), repo_id)
        .await
        .map_err(AsyncRequestsError::internal)?
        .ok_or_else(|| {
            AsyncRequestsError::request(anyhow::anyhow!("Repo not found: {}", params.repo_id))
        })?
        .with_authorization_context(AuthorizationContext::new_bypass_access_control())
        .build()
        .await
        .map_err(AsyncRequestsError::internal)?;

    let derived_data_type =
        DerivableType::from_name(&params.derived_data_type).map_err(AsyncRequestsError::request)?;

    tracing::debug!(
        "MarkTypeEnabled: marking type {:?} enabled for repo {} (root_request_id {})",
        derived_data_type,
        repo_id.id(),
        root_request_id.0,
    );

    // INTEGRATION (M1): calls the `enabled_derived_data_types` facet owned by
    // milestone M1 (`repo_attributes/enabled_derived_data_types/`). The facet
    // accessor `enabled_derived_data_types()` (generated by `#[facet::facet]`)
    // and the `EnabledDerivedDataTypesRef` import below resolve once M1 is
    // stacked underneath. Assumed signature:
    //   async fn mark_enabled(&self, ctx: &CoreContext, repo_id: RepositoryId,
    //       ddt: DerivableType, root_request_id: Option<u64>) -> anyhow::Result<()>
    // The write is an idempotent upsert (INSERT OR IGNORE / ON DUPLICATE KEY).
    repo.repo()
        .enabled_derived_data_types()
        .mark_enabled(ctx, repo_id, derived_data_type, Some(root_request_id.0))
        .await
        .map_err(AsyncRequestsError::internal)?;

    Ok(thrift::MarkTypeEnabledResponse {
        repo_id: params.repo_id,
        derived_data_type: params.derived_data_type,
        enabled: true,
        error_message: None,
        ..Default::default()
    })
}

/// Handles a DeriveBackfill request. With `repo_concurrency == 0` (the default)
/// it enqueues a DeriveBackfillRepo sub-request for every repo at once. With
/// `repo_concurrency > 0` (and fewer than that many repos would be trivial) it
/// runs the sliding-window scheduler, keeping at most `repo_concurrency` repos
/// deriving at a time.
pub(crate) async fn compute_derive_backfill(
    ctx: &CoreContext,
    _mononoke: Arc<Mononoke<Repo>>,
    queue: &AsyncMethodRequestQueue,
    params: thrift::DeriveBackfillParams,
    root_request_id: RowId,
    created_by: Option<String>,
) -> Result<thrift::DeriveBackfillResponse, AsyncRequestsError> {
    if params.repo_entries.is_empty() {
        return Err(AsyncRequestsError::request(anyhow::anyhow!(
            "repo_entries must not be empty"
        )));
    }

    // Validate derived_data_type upfront
    DerivableType::from_name(&params.derived_data_type).map_err(AsyncRequestsError::request)?;

    let total_sub_requests = params.repo_entries.len() as i64;
    let created_by = created_by.as_deref();

    let repo_concurrency = params.repo_concurrency.unwrap_or(0);
    if repo_concurrency > 0 && (repo_concurrency as usize) < params.repo_entries.len() {
        info!(
            "DeriveBackfill scheduling {} repos with repo_concurrency={}",
            params.repo_entries.len(),
            repo_concurrency,
        );
        crate::backfill_scheduler::schedule_repo_backfills(
            ctx,
            queue,
            &params,
            &root_request_id,
            created_by,
            repo_concurrency as usize,
        )
        .await?;
    } else {
        stream::iter(&params.repo_entries)
            .map(Ok::<_, AsyncRequestsError>)
            .try_for_each_concurrent(REPO_ENQUEUE_CONCURRENCY, |entry| {
                enqueue_repo_backfill(ctx, queue, &params, entry, &root_request_id, created_by)
            })
            .await?;

        info!(
            "DeriveBackfill enqueued {} DeriveBackfillRepo sub-requests across {} repos",
            total_sub_requests,
            params.repo_entries.len(),
        );
    }

    Ok(thrift::DeriveBackfillResponse {
        total_sub_requests,
        error_message: None,
        ..Default::default()
    })
}

/// Computes segmented slices and their boundaries for a backfill operation.
///
/// This function performs Phase 1 of the backfill process:
/// 1. Filters out already-derived changesets (unless reslice is enabled)
/// 2. Computes the derived frontier to exclude already-processed ancestors
/// 3. Calls segmented_slice_ancestors to partition the commit graph into processable slices
///
/// Returns a vector of slices with their associated boundary changesets, or
/// an empty vector if there's nothing to process.
async fn compute_slices_and_boundaries(
    ctx: &CoreContext,
    inner_repo: &Repo,
    manager: &DerivedDataManager,
    derived_data_type: DerivableType,
    cs_ids: Vec<ChangesetId>,
    slice_size: u64,
    reslice: bool,
    repo_id: &RepositoryId,
) -> Result<Vec<SegmentedSliceWithBoundaries>, AsyncRequestsError> {
    // Determine whether to filter out already-derived changesets.
    // We skip filtering when reslice=true: treat all commits as underived for
    // slicing purposes.
    let should_filter_derived = !reslice;

    // Filter to only underived changesets (unless skip_filtering is set)
    let mut cs_ids = cs_ids;
    if should_filter_derived {
        cs_ids = manager
            .pending(ctx, &cs_ids, None, derived_data_type)
            .await
            .map_err(AsyncRequestsError::internal)?;
        if cs_ids.is_empty() {
            info!(
                "All changesets already derived for repo {}, nothing to enqueue",
                repo_id.id()
            );
            return Ok(vec![]);
        }
        info!("{} changesets still underived", cs_ids.len());
    }

    // Find the derived frontier
    // We only compute the frontier when filtering derived changesets
    let excluded_ancestors = if should_filter_derived {
        let (frontier_stats, frontier) = inner_repo
            .commit_graph()
            .ancestors_frontier_with(ctx, cs_ids.clone(), |cs_id| {
                let manager = manager.clone();
                async move {
                    Ok(manager
                        .is_derived(ctx, cs_id, None, derived_data_type)
                        .await?)
                }
            })
            .try_timed()
            .await
            .map_err(AsyncRequestsError::internal)?;
        info!(
            "Computed derived frontier ({} changesets) in {}ms",
            frontier.len(),
            frontier_stats.completion_time.as_millis(),
        );
        frontier
    } else {
        vec![]
    };

    // Compute segmented slices and boundary changesets
    let (slices_stats, slices) = inner_repo
        .commit_graph()
        .segmented_slice_ancestors(ctx, cs_ids, excluded_ancestors, slice_size)
        .try_timed()
        .await
        .map_err(AsyncRequestsError::internal)?;

    let total_boundaries: usize = slices.iter().map(|s| s.boundaries.len()).sum();
    info!(
        "Computed {} slices with {} boundary changesets in {}ms",
        slices.len(),
        total_boundaries,
        slices_stats.completion_time.as_millis(),
    );

    Ok(slices)
}

/// Enqueues boundary and slice derivation requests with proper dependencies.
///
/// 1. For parallel types: Creates N boundary derivation requests (distributed across workers)
/// 2. Creates slice derivation requests with proper dependencies:
///    - Parallel types: Each slice depends on the boundary requests containing its boundaries
///    - Serial types: Each slice depends on the previous slice (topological order)
///
/// # Parallelization strategy
/// - Parallel types can derive boundaries independently using predecessor derivation
/// - Boundaries are split into multiple requests to allow concurrent processing
/// - Each slice only waits for the specific boundary requests it needs
async fn enqueue_boundary_and_slice_requests(
    ctx: &CoreContext,
    queue: &AsyncMethodRequestQueue,
    slices: Vec<SegmentedSliceWithBoundaries>,
    derived_data_type: DerivableType,
    num_boundary_requests: i32,
    boundaries_concurrency: i32,
    config_name: Option<&str>,
    mapping_key_prefix: Option<&str>,
    repo_id: &RepositoryId,
    root_request_id: &RowId,
    created_by: Option<&str>,
) -> Result<Vec<RowId>, AsyncRequestsError> {
    let serial_slices = requires_serial_slice_processing(derived_data_type);
    let total_boundaries: usize = slices.iter().map(|s| s.boundaries.len()).sum();

    // For parallel types, enqueue boundary derivation so slices can proceed
    // independently. We build a mapping from so that we can set up the dependencies
    // between slice requests and the boundary requests they need.
    let boundary_to_request: HashMap<ChangesetId, RowId> = if !serial_slices && total_boundaries > 0
    {
        // Collect all unique boundaries
        let boundaries_vec: Vec<ChangesetId> = slices
            .iter()
            .flat_map(|s| s.boundaries.iter())
            .cloned()
            .collect();
        let num_requests = (num_boundary_requests as usize).min(boundaries_vec.len());
        let chunk_size = boundaries_vec.len().div_ceil(num_requests);

        let mapping = stream::iter(
            boundaries_vec
                .chunks(chunk_size)
                .map(|chunk| chunk.to_vec()),
        )
        .then(|chunk| async move {
            let boundary_cs_bytes: Vec<Vec<u8>> =
                chunk.iter().map(|cs_id| cs_id.as_ref().to_vec()).collect();

            let boundary_params = thrift::DeriveBoundariesParams {
                repo_id: repo_id.id() as i64,
                derived_data_type: derived_data_type.name().to_string(),
                boundary_cs_ids: boundary_cs_bytes,
                concurrency: boundaries_concurrency,
                use_predecessor_derivation: true,
                config_name: config_name.map(|s| s.to_string()),
                mapping_key_prefix: mapping_key_prefix.map(|s| s.to_string()),
                ..Default::default()
            };

            let boundary_token = queue
                .enqueue_with_root(
                    ctx,
                    Some(repo_id),
                    boundary_params,
                    root_request_id,
                    created_by,
                )
                .await
                .map_err(AsyncRequestsError::internal)?;

            // Map each boundary in this chunk to this request
            let row_id = boundary_token.id();
            Ok::<_, AsyncRequestsError>(
                chunk
                    .into_iter()
                    .map(|boundary| (boundary, row_id))
                    .collect::<Vec<_>>(),
            )
        })
        .try_collect::<Vec<_>>()
        .await?
        .into_iter()
        .flatten()
        .collect();

        info!(
            "Created {} boundary requests for {} total boundaries (avg {:.1} boundaries per request)",
            num_requests,
            boundaries_vec.len(),
            boundaries_vec.len() as f64 / num_requests as f64,
        );

        mapping
    } else {
        HashMap::new()
    };

    // Enqueue slice derivation requests.
    // Serial types: each slice depends on the previous (topological order).
    // Parallel types: each slice depends only on the boundary requests containing its boundaries.
    let mut prev_slice_row_id: Option<RowId> = None;
    let mut slice_row_ids: Vec<RowId> = Vec::with_capacity(slices.len());
    for slice_with_boundaries in slices.iter() {
        let segments: Vec<thrift::DeriveSliceSegment> = slice_with_boundaries
            .slice
            .segments
            .iter()
            .map(|seg| thrift::DeriveSliceSegment {
                head: seg.head.as_ref().to_vec(),
                base: seg.base.as_ref().to_vec(),
                ..Default::default()
            })
            .collect();

        let slice_params = thrift::DeriveSliceParams {
            repo_id: repo_id.id() as i64,
            derived_data_type: derived_data_type.name().to_string(),
            segments,
            config_name: config_name.map(|s| s.to_string()),
            mapping_key_prefix: mapping_key_prefix.map(|s| s.to_string()),
            ..Default::default()
        };

        let depends_on: Vec<RowId> = if serial_slices {
            prev_slice_row_id.iter().cloned().collect()
        } else {
            // Get the boundary changesets required for this slice, and look up
            // the request ids in the mapping we built above.
            slice_with_boundaries
                .boundaries
                .iter()
                .filter_map(|boundary| boundary_to_request.get(boundary).cloned())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect()
        };

        let slice_token = queue
            .enqueue_with_dependencies_and_root(
                ctx,
                Some(repo_id),
                slice_params,
                &depends_on,
                root_request_id,
                created_by,
            )
            .await
            .map_err(AsyncRequestsError::internal)?;
        let slice_row_id = slice_token.id();
        slice_row_ids.push(slice_row_id);
        prev_slice_row_id = Some(slice_row_id);
    }

    // The union of all boundary + slice RowIds enqueued for this repo. A
    // downstream MarkTypeEnabled node depends on this whole set: GDMV3 is
    // parallel/untopological, so a failed boundary with no dependent slice must
    // still cascade (design C3). Boundary RowIds are the unique request ids from
    // the boundary map; slice RowIds are collected above.
    let unique_boundary_row_ids: Vec<RowId> = boundary_to_request
        .values()
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    info!(
        "Enqueued {} sub-requests for repo {} ({} boundary + {} slices)",
        unique_boundary_row_ids.len() + slice_row_ids.len(),
        repo_id.id(),
        unique_boundary_row_ids.len(),
        slice_row_ids.len(),
    );

    let mut enqueued_row_ids = unique_boundary_row_ids;
    enqueued_row_ids.extend(slice_row_ids);
    Ok(enqueued_row_ids)
}

/// Process backfill for a single repo by computing slices and enqueueing sub-requests.
///
/// This is the main entry point for backfill processing, which coordinates two phases:
/// 1. Compute slices and boundaries (via compute_slices_and_boundaries)
/// 2. Enqueue boundary and slice requests with proper dependencies (via enqueue_boundary_and_slice_requests)
///
/// Returns the number of sub-requests enqueued for this repo.
async fn process_repo_backfill(
    ctx: &CoreContext,
    repo: &RepoContext<Repo>,
    queue: &AsyncMethodRequestQueue,
    derived_data_type: DerivableType,
    cs_ids: Vec<ChangesetId>,
    slice_size: u64,
    reslice: bool,
    boundaries_concurrency: i32,
    num_boundary_requests: i32,
    config_name: Option<&str>,
    mapping_key_prefix: Option<&str>,
    repo_id: &RepositoryId,
    root_request_id: &RowId,
    created_by: Option<&str>,
) -> Result<Vec<RowId>, AsyncRequestsError> {
    let inner_repo = repo.repo();
    let manager = resolve_manager(inner_repo.repo_derived_data(), config_name)?;
    // Slicing decides which changesets count as already-derived, so it has to look at
    // the same namespace the sub-requests will write to.
    let manager = with_type_enabled(manager, derived_data_type, mapping_key_prefix);

    info!(
        "DeriveBackfill for repo {} type {:?}: {} changesets, slice_size {}, mapping_key_prefix {:?}",
        repo_id.id(),
        derived_data_type,
        cs_ids.len(),
        slice_size,
        mapping_key_prefix,
    );

    // Phase 1: Compute slices and boundaries
    let slices = compute_slices_and_boundaries(
        ctx,
        inner_repo,
        &manager,
        derived_data_type,
        cs_ids,
        slice_size,
        reslice,
        repo_id,
    )
    .await?;

    if slices.is_empty() {
        info!("Nothing to enqueue for repo {}", repo_id.id());
        return Ok(vec![]);
    }

    // Phase 2: Enqueue boundary and slice requests with proper dependencies
    enqueue_boundary_and_slice_requests(
        ctx,
        queue,
        slices,
        derived_data_type,
        num_boundary_requests,
        boundaries_concurrency,
        config_name,
        mapping_key_prefix,
        repo_id,
        root_request_id,
        created_by,
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::num::NonZeroU64;

    use bonsai_hg_mapping::BonsaiHgMapping;
    use bookmarks::Bookmarks;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphWriter;
    use context::CoreContext;
    use derivation_queue_thrift::DerivationPriority;
    use fbinit::FacebookInit;
    use filestore::FilestoreConfig;
    use fsnodes::RootFsnodeId;
    use futures::FutureExt;
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::with_just_knobs_async;
    use metaconfig_types::DerivationPipelineStageConfig;
    use mononoke_macros::mononoke;
    use mononoke_types::MPath;
    use repo_blobstore::RepoBlobstore;
    use repo_derived_data::RepoDerivedData;
    use repo_derived_data::RepoDerivedDataRef;
    use repo_identity::RepoIdentity;
    use tests_utils::CreateCommitContext;

    use super::*;

    /// A minimal single-stage (terminal ROOT only) pipeline config listing the
    /// given types. Not validated (`DerivationPipelineConfig::validate` is not
    /// called) — just enough to drive `should_use_pipeline` / the driver.
    fn root_only_pipeline_config(
        types: impl IntoIterator<Item = DerivableType>,
        batch_size: u64,
    ) -> DerivationPipelineConfig {
        DerivationPipelineConfig {
            types: types.into_iter().collect(),
            bookmarks: vec![],
            stages: HashMap::from([(MPath::ROOT, DerivationPipelineStageConfig::new(vec![]))]),
            batch_size: NonZeroU64::new(batch_size).expect("batch size must be non-zero"),
        }
    }

    /// Evaluate `should_use_pipeline` with the repo-scoped rollout JustKnob
    /// forced to `jk_enabled`.
    async fn use_pipeline_with_jk(
        manager: &DerivedDataManager,
        derived_data_type: DerivableType,
        jk_enabled: bool,
    ) -> bool {
        with_just_knobs_async(
            JustKnobsInMemory::new(HashMap::from([(
                JK_BACKFILL_USE_PIPELINE.to_string(),
                KnobVal::Bool(jk_enabled),
            )])),
            async move { should_use_pipeline(manager, derived_data_type).is_some() }.boxed(),
        )
        .await
    }

    /// A pipeline config with a terminal ROOT stage depending on two sibling
    /// stages `dir1` and `dir2`. Deriving ROOT is one phase; `dir1`+`dir2` are
    /// an earlier phase of two mutually-independent stages (the parallelized
    /// case).
    fn multi_stage_pipeline_config(
        types: impl IntoIterator<Item = DerivableType>,
        batch_size: u64,
    ) -> Result<DerivationPipelineConfig> {
        let dir1 = MPath::new("dir1")?;
        let dir2 = MPath::new("dir2")?;
        let stages = HashMap::from([
            (
                MPath::ROOT,
                DerivationPipelineStageConfig::new(vec![dir1.clone(), dir2.clone()]),
            ),
            (dir1, DerivationPipelineStageConfig::new(vec![])),
            (dir2, DerivationPipelineStageConfig::new(vec![])),
        ]);
        Ok(DerivationPipelineConfig {
            types: types.into_iter().collect(),
            bookmarks: vec![],
            stages,
            batch_size: NonZeroU64::new(batch_size).expect("batch size must be non-zero"),
        })
    }

    async fn verify_pipeline_root_matches_canonical(
        manager: &DerivedDataManager,
        ctx: &CoreContext,
        cs_ids: &[ChangesetId],
        derived_data_type: DerivableType,
    ) -> Result<()> {
        let canonical = with_type_enabled(manager, derived_data_type, Some("canonical-test."));
        canonical
            .derive_bulk_locally(ctx, cs_ids, None, &[derived_data_type], None, None)
            .await?;
        let expected =
            BulkDerivation::fetch_derived_batch(&canonical, ctx, cs_ids, None, derived_data_type)
                .await?;
        let actual =
            BulkDerivation::fetch_derived_batch(manager, ctx, cs_ids, None, derived_data_type)
                .await?;
        assert_eq!(expected.len(), cs_ids.len());
        assert_eq!(actual, expected, "pipeline roots diverged from canonical");
        Ok(())
    }

    /// Run one pipeline slice over `(base, head)` with terminal mapping publication
    /// enabled and the per-chunk retry-limit JK pinned, returning the derived count.
    async fn run_pipeline_slice(
        ctx: &CoreContext,
        manager: &DerivedDataManager,
        config: &DerivationPipelineConfig,
        derived_data_type: DerivableType,
        base: ChangesetId,
        head: ChangesetId,
    ) -> Result<i64> {
        with_just_knobs_async(
            JustKnobsInMemory::new(HashMap::from([
                (JK_BACKFILL_CHUNK_RETRY_LIMIT.to_string(), KnobVal::Int(0)),
                (
                    "scm/mononoke:derived_data_pipeline_terminal_stage_prod_mapping".to_string(),
                    KnobVal::Bool(true),
                ),
            ])),
            derive_slice_pipeline(ctx, manager, config, derived_data_type, vec![(base, head)])
                .boxed(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e:#}"))
    }

    #[facet::container]
    struct TestRepo(
        dyn BonsaiHgMapping,
        dyn Bookmarks,
        CommitGraph,
        dyn CommitGraphWriter,
        RepoDerivedData,
        RepoBlobstore,
        FilestoreConfig,
        RepoIdentity,
    );

    // INTEGRATION (M1): these two tests exercise the facet-level contract that
    // `compute_mark_type_enabled` relies on -- an idempotent upsert followed by a
    // read-back. They are written directly against M1's in-memory
    // `TestEnabledDerivedDataTypes` facet impl rather than against
    // `compute_mark_type_enabled` itself, because the compute fn needs a full
    // `Mononoke<Repo>` whose container carries the `enabled_derived_data_types`
    // facet -- container + `test_repo_factory` wiring that M1 owns. They compile
    // and run once M1 is stacked underneath and its `TestEnabledDerivedDataTypes`
    // + `mark_enabled` / `get_enabled_types` (`Staleness`) API land as assumed;
    // until then this module references symbols not yet present in this worktree.
    mod mark_type_enabled_tests {
        use enabled_derived_data_types::EnabledDerivedDataTypes;
        use enabled_derived_data_types::Staleness;
        use enabled_derived_data_types::TestEnabledDerivedDataTypes;

        use super::*;

        #[mononoke::fbinit_test]
        async fn mark_type_enabled_writes_entry(fb: FacebookInit) -> Result<()> {
            let ctx = CoreContext::test_mock(fb);
            let facet = TestEnabledDerivedDataTypes::new();
            let repo_id = RepositoryId::new(42);
            let ddt = DerivableType::GitDeltaManifestsV3;

            facet
                .mark_enabled(&ctx, repo_id, ddt, Some(7))
                .await
                .expect("mark_enabled should succeed");

            let enabled = facet
                .get_enabled_types(&ctx, repo_id, Staleness::MostRecent)
                .await
                .expect("get_enabled_types should succeed");
            assert_eq!(
                enabled,
                vec![ddt],
                "the marked type should be the single enabled entry",
            );
            Ok(())
        }

        #[mononoke::fbinit_test]
        async fn mark_type_enabled_is_idempotent(fb: FacebookInit) -> Result<()> {
            let ctx = CoreContext::test_mock(fb);
            let facet = TestEnabledDerivedDataTypes::new();
            let repo_id = RepositoryId::new(42);
            let ddt = DerivableType::GitDeltaManifestsV3;

            facet.mark_enabled(&ctx, repo_id, ddt, Some(7)).await?;
            // Second mark must succeed (idempotent upsert) and not duplicate.
            facet.mark_enabled(&ctx, repo_id, ddt, Some(7)).await?;

            let enabled = facet
                .get_enabled_types(&ctx, repo_id, Staleness::MostRecent)
                .await?;
            assert_eq!(
                enabled.len(),
                1,
                "re-marking an already-enabled type must leave exactly one entry",
            );
            assert_eq!(enabled, vec![ddt]);
            Ok(())
        }
    }

    /// `wait_until_derived` must observe a changeset transitioning to derived
    /// (by any path) and resolve. This is the only non-trivial bit of the
    /// opportunistic-boundary race: the `select!` itself is plumbing, but this
    /// poll has to actually detect real derivation against a real manager.
    #[mononoke::fbinit_test]
    async fn wait_until_derived_resolves_after_derivation(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let cs_id = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("a", "a")
            .commit()
            .await?;
        let manager = repo.repo_derived_data().manager();

        // Underived to begin with.
        assert!(
            !BulkDerivation::is_derived(manager, &ctx, cs_id, None, DerivableType::Fsnodes).await?,
            "changeset should start underived",
        );

        // Derive concurrently with the waiter. The waiter must observe the
        // transition and resolve; if it never did, this join would hang and the
        // test would time out.
        tokio::join!(
            async {
                repo.repo_derived_data()
                    .derive::<RootFsnodeId>(&ctx, cs_id, DerivationPriority::LOW)
                    .await
                    .expect("derivation should succeed");
            },
            wait_until_derived(manager, &ctx, cs_id, DerivableType::Fsnodes),
        );

        assert!(
            BulkDerivation::is_derived(manager, &ctx, cs_id, None, DerivableType::Fsnodes).await?,
            "changeset should be derived after the waiter resolved",
        );
        Ok(())
    }

    /// `with_type_enabled` is the only thing keeping a prefixed backfill out of
    /// production's key namespace, and every manager in a campaign has to agree on
    /// the prefix. Assert through `DerivationContext::mapping_key_prefix` — the
    /// accessor `format_key` actually calls — rather than the config map, since an
    /// absent entry and an empty one are indistinguishable to derivation.
    #[mononoke::fbinit_test]
    async fn with_type_enabled_applies_mapping_key_prefix(fb: FacebookInit) -> Result<()> {
        let repo: TestRepo = test_repo_factory::build_empty(fb)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let base = repo.repo_derived_data().manager();

        // No prefix requested: the type is enabled, the namespace is untouched.
        let plain = with_type_enabled(base, DerivableType::Fsnodes, None);
        assert!(plain.config().types.contains(&DerivableType::Fsnodes));
        assert_eq!(
            plain
                .derivation_context(None)
                .mapping_key_prefix::<RootFsnodeId>(),
            "",
        );

        // Prefix requested: derivation writes into the fresh namespace.
        let prefixed = with_type_enabled(base, DerivableType::Fsnodes, Some("r5."));
        assert!(prefixed.config().types.contains(&DerivableType::Fsnodes));
        assert_eq!(
            prefixed
                .derivation_context(None)
                .mapping_key_prefix::<RootFsnodeId>(),
            "r5.",
        );

        // Only the backfilled type is renamespaced; siblings keep their own keys.
        assert_eq!(prefixed.config().mapping_key_prefixes.len(), 1);

        // The type already being enabled must not short-circuit the prefix. Repos
        // partway through a version rollout already carry the type, and returning
        // early there would silently derive into the config's namespace instead.
        let rederived = with_type_enabled(&prefixed, DerivableType::Fsnodes, Some("r6."));
        assert_eq!(
            rederived
                .derivation_context(None)
                .mapping_key_prefix::<RootFsnodeId>(),
            "r6.",
        );
        Ok(())
    }

    /// The gating decision must require all of: a repo pipeline config, the type
    /// listed in it, the type being pipeline-derivable, and the rollout JK on.
    #[mononoke::fbinit_test]
    async fn should_use_pipeline_gating(fb: FacebookInit) -> Result<()> {
        let repo: TestRepo = test_repo_factory::build_empty(fb)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let base_manager = repo.repo_derived_data().manager();

        // No pipeline config on the repo: never use the pipeline, even with the
        // JK on.
        assert!(
            !use_pipeline_with_jk(base_manager, DerivableType::Fsnodes, true).await,
            "no pipeline config should disable the pipeline path",
        );

        // Pipeline config present and listing Fsnodes (a pipeline-derivable type).
        let manager = base_manager.with_replaced_pipeline_config(Some(root_only_pipeline_config(
            [DerivableType::Fsnodes],
            1,
        )));
        assert!(
            use_pipeline_with_jk(&manager, DerivableType::Fsnodes, true).await,
            "config + eligible type + JK on should enable the pipeline path",
        );
        assert!(
            !use_pipeline_with_jk(&manager, DerivableType::Fsnodes, false).await,
            "JK off should disable the pipeline path (safe rollback)",
        );
        assert!(
            !use_pipeline_with_jk(&manager, DerivableType::Unodes, true).await,
            "a type absent from the config's types should not use the pipeline",
        );

        // A type present in the config but not pipeline-derivable is rejected.
        let ci_manager = base_manager.with_replaced_pipeline_config(Some(
            root_only_pipeline_config([DerivableType::ChangesetInfo], 1),
        ));
        assert!(
            !use_pipeline_with_jk(&ci_manager, DerivableType::ChangesetInfo, true).await,
            "a non-pipeline-derivable type should not use the pipeline",
        );

        Ok(())
    }

    /// `derive_slice_pipeline` must derive an entire segment via the pipeline,
    /// in parents-first order. `range_stream` already yields that order; a stray
    /// reverse would derive a child before its in-batch parent and fail to find
    /// the parent's stage output — this test guards that ordering.
    #[mononoke::fbinit_test]
    async fn derive_slice_pipeline_derives_linear_stack(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let manager = repo.repo_derived_data().manager().clone();

        // Linear stack c1 <- c2 <- c3.
        let c1 = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("a", "1")
            .commit()
            .await?;
        let c2 = CreateCommitContext::new(&ctx, &repo, vec![c1])
            .add_file("b", "2")
            .commit()
            .await?;
        let c3 = CreateCommitContext::new(&ctx, &repo, vec![c2])
            .add_file("c", "3")
            .commit()
            .await?;

        // batch_size large enough that the whole segment is a single batch, so
        // parents-first ordering is exercised intra-batch: fsnodes derives each
        // commit from its parent's stage output, which for a mis-ordered batch
        // would not yet exist for an in-batch parent.
        let config = root_only_pipeline_config([DerivableType::Fsnodes], 10);
        let derived =
            run_pipeline_slice(&ctx, &manager, &config, DerivableType::Fsnodes, c1, c3).await?;

        // `range_stream(c1, c3)` is inclusive, so the whole c1..=c3 stack is
        // derived.
        assert_eq!(
            derived, 3,
            "all three commits in the segment should be derived"
        );
        verify_pipeline_root_matches_canonical(
            &manager,
            &ctx,
            &[c1, c2, c3],
            DerivableType::Fsnodes,
        )
        .await
    }

    /// Multi-stage pipeline through the backfill entry point for a serial type
    /// (fsnodes). `batch_size` 2 over 3 commits forces multiple batches (so
    /// cross-batch parent resolution is exercised), and the `dir1`/`dir2` phase
    /// derives two independent stages concurrently. Output must match canonical.
    #[mononoke::fbinit_test]
    async fn derive_slice_pipeline_multi_stage(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let manager = repo.repo_derived_data().manager().clone();

        let c1 = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("dir1/a", "1")
            .add_file("dir2/b", "1")
            .commit()
            .await?;
        let c2 = CreateCommitContext::new(&ctx, &repo, vec![c1])
            .add_file("dir1/a", "2")
            .add_file("top", "r")
            .commit()
            .await?;
        let c3 = CreateCommitContext::new(&ctx, &repo, vec![c2])
            .add_file("dir2/b", "3")
            .commit()
            .await?;

        let config = multi_stage_pipeline_config([DerivableType::Fsnodes], 2)?;
        let derived =
            run_pipeline_slice(&ctx, &manager, &config, DerivableType::Fsnodes, c1, c3).await?;
        assert_eq!(derived, 3, "the whole c1..=c3 stack should be derived");
        verify_pipeline_root_matches_canonical(
            &manager,
            &ctx,
            &[c1, c2, c3],
            DerivableType::Fsnodes,
        )
        .await
    }

    /// A cross-stage copy (a file copied from the `dir1` stage into the `dir2`
    /// stage) is a chokepoint: `plan_batches` must isolate that commit into its
    /// own single-commit batch. Verify the backfill pipeline still produces
    /// canonical output with such a commit in the slice.
    #[mononoke::fbinit_test]
    async fn derive_slice_pipeline_cross_stage_copy(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let manager = repo.repo_derived_data().manager().clone();

        let c1 = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("dir1/a", "1")
            .add_file("dir2/b", "1")
            .commit()
            .await?;
        // Copy dir1/a -> dir2/c: the source (dir1) and dest (dir2) are in
        // different stages, so c2 is a cross-stage-copy chokepoint.
        let c2 = CreateCommitContext::new(&ctx, &repo, vec![c1])
            .add_file_with_copy_info("dir2/c", "1", (c1, "dir1/a"))
            .commit()
            .await?;
        let c3 = CreateCommitContext::new(&ctx, &repo, vec![c2])
            .add_file("dir1/a", "2")
            .commit()
            .await?;

        let config = multi_stage_pipeline_config([DerivableType::Fsnodes], 2)?;
        let derived =
            run_pipeline_slice(&ctx, &manager, &config, DerivableType::Fsnodes, c1, c3).await?;
        assert_eq!(
            derived, 3,
            "the whole stack including the chokepoint should be derived"
        );
        verify_pipeline_root_matches_canonical(
            &manager,
            &ctx,
            &[c1, c2, c3],
            DerivableType::Fsnodes,
        )
        .await
    }
}
