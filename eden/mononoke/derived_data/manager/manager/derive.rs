/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::future;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use async_recursion::async_recursion;
use blobstore::Loadable;
use borrowed::borrowed;
use cloned::cloned;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use derived_data_service_if::DerivationType;
use derived_data_service_if::DeriveRequest;
use derived_data_service_if::DeriveResponse;
use derived_data_service_if::DeriveUnderived;
use derived_data_service_if::DerivedDataType;
use derived_data_service_if::RequestStatus;
use futures::Future;
use futures::future::BoxFuture;
use futures::future::FutureExt;
use futures::future::Shared;
use futures::future::TryFutureExt;
use futures::future::try_join;
use futures::future::try_join_all;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use futures_stats::TimedFutureExt;
use futures_stats::TimedTryFutureExt;
use lock_ext::LockExt;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use mononoke_types::MPath;
use scuba_ext::FutureStatsScubaExt;
use tracing::Instrument;

use super::DerivationAssignment;
use super::DerivedDataManager;
use crate::context::DerivationContext;
use crate::derivable::BonsaiDerivable;
use crate::derivable::DerivableUntopologically;
use crate::derivable::DerivationDependencies;
use crate::derivable::PipelineDerivable;
use crate::error::DerivationError;
use crate::error::SharedDerivationError;

/// Trait to allow determination of rederivation.
pub trait Rederivation: Send + Sync + 'static {
    /// Determine whether a changeset needs rederivation of
    /// a particular derived data type.
    ///
    /// If this function returns `false`, then it will only be
    /// derived if it isn't already derived.
    fn needs_rederive(&self, derivable_type: DerivableType, csid: ChangesetId) -> bool;

    /// Marks a changeset as having been derived.  After this
    /// is called, `needs_rederive` should not return `true` for
    /// this changeset and derived data type.
    fn mark_derived(&self, derivable_type: DerivableType, csid: ChangesetId);
}

impl Rederivation for Mutex<HashSet<ChangesetId>> {
    fn needs_rederive(&self, _derivable_type: DerivableType, csid: ChangesetId) -> bool {
        self.with(|rederive| rederive.contains(&csid))
    }

    fn mark_derived(&self, _derivable_type: DerivableType, csid: ChangesetId) {
        self.with(|rederive| rederive.remove(&csid));
    }
}

impl Rederivation for Mutex<HashSet<(DerivableType, ChangesetId)>> {
    fn needs_rederive(&self, derivable_type: DerivableType, csid: ChangesetId) -> bool {
        self.with(|rederive| rederive.contains(&(derivable_type, csid)))
    }

    fn mark_derived(&self, derivable_type: DerivableType, csid: ChangesetId) {
        self.with(|rederive| rederive.remove(&(derivable_type, csid)));
    }
}

pub type VisitedDerivableTypesMapStatic<OkType, ErrType> =
    Arc<Mutex<HashMap<DerivableType, Shared<BoxFuture<'static, Result<OkType, ErrType>>>>>>;

pub type VisitedDerivableTypesMap<'a, OkType, ErrType> =
    Arc<Mutex<HashMap<DerivableType, Shared<BoxFuture<'a, Result<OkType, ErrType>>>>>>;

impl DerivedDataManager {
    #[async_recursion]
    /// Returns the appropriate manager to derive given changeset, either this
    /// manager, or some secondary manager in the chain.
    async fn get_manager<'s>(
        &'s self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> anyhow::Result<&'s DerivedDataManager> {
        Ok(if let Some(secondary) = &self.inner.secondary {
            if secondary
                .assigner
                .assign(ctx, vec![cs_id])
                .await?
                .secondary
                .is_empty()
            {
                self
            } else {
                secondary.manager.get_manager(ctx, cs_id).await?
            }
        } else {
            self
        })
    }

    pub fn derivation_context(
        &self,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> DerivationContext {
        self.inner
            .derivation_context
            .with_replaced_rederivation(rederivation)
    }

    pub async fn changeset_derivation_context(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> anyhow::Result<DerivationContext> {
        Ok(self
            .get_manager(ctx, cs_id)
            .await?
            .derivation_context(rederivation))
    }

    pub async fn check_derived<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
    ) -> Result<(), DerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        if self
            .fetch_derived::<Derivable>(ctx, csid, None)
            .await?
            .is_none()
        {
            return Err(
                anyhow!("expected {} already derived for {}", Derivable::NAME, csid).into(),
            );
        }
        Ok(())
    }

    /// Find which ancestors of `heads` are not yet derived, and necessary for
    /// the derivation of `heads` to complete, and derive them.
    /// The derivation will be batched. Unless otherwise configured here with `override_batch_size`, the
    /// batch_size will be read from the configuration for this derived data type.
    /// Batches can span merge commits and preserve topological order.
    /// Dependent types will be derived ahead of time.
    /// Return how many changesets were actually derived to derive the heads.
    pub async fn derive_heads<Derivable>(
        &self,
        ctx: CoreContext,
        heads: Vec<ChangesetId>,
        override_batch_size: Option<u64>,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<u64, SharedDerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        self.clone()
            .derive_heads_with_visited::<Derivable>(
                ctx,
                heads,
                override_batch_size,
                rederivation,
                Default::default(),
            )
            .await
    }

    pub fn derive_heads_with_visited<Derivable>(
        self,
        ctx: CoreContext,
        heads: Vec<ChangesetId>,
        override_batch_size: Option<u64>,
        rederivation: Option<Arc<dyn Rederivation>>,
        visited: VisitedDerivableTypesMapStatic<u64, SharedDerivationError>,
    ) -> impl Future<Output = Result<u64, SharedDerivationError>>
    where
        Derivable: BonsaiDerivable,
    {
        let derivation_future = {
            cloned!(visited, ctx, heads);
            async move {
                Derivable::Dependencies::derive_heads(
                    self.clone(),
                    ctx.clone(),
                    heads.clone(),
                    override_batch_size,
                    rederivation.clone(),
                    visited,
                )
                .await
                .context("failed to derive dependent types")?;

                let derivation_ctx = self.derivation_context(rederivation);
                let batch_size =
                    override_batch_size.unwrap_or(derivation_ctx.batch_size::<Derivable>());
                if batch_size == 0 {
                    return Err(anyhow!("derivation batch size must be greater than zero").into());
                }
                let rederivation = derivation_ctx.rederivation.clone();

                let mut gap_slices: Vec<Vec<ChangesetId>> = Vec::new();
                let mut count: u64 = 0;
                let mut current_heads = heads;

                loop {
                    let last_derived = self
                        .commit_graph()
                        .ancestors_frontier_with(&ctx, current_heads.clone(), |csid| {
                            borrowed!(ctx, derivation_ctx);
                            async move {
                                Ok(derivation_ctx
                                    .fetch_derived::<Derivable>(ctx, csid)
                                    .await?
                                    .is_some())
                            }
                        })
                        .await
                        .map_err(Into::<DerivationError>::into)?;

                    let (slices_stream, external_parents) = self
                        .commit_graph()
                        .ancestors_difference_segment_slices_with_external_parents(
                            &ctx,
                            current_heads,
                            last_derived,
                            batch_size,
                        )
                        .await
                        .map_err(Into::<DerivationError>::into)?;

                    // Check for gaps: external parents that should be
                    // derived but aren't (monotonicity violation).
                    let gap_parents: Vec<_> = stream::iter(external_parents)
                        .map(|cs_id| {
                            borrowed!(ctx, derivation_ctx);
                            async move {
                                let derived = derivation_ctx
                                    .fetch_derived::<Derivable>(ctx, cs_id)
                                    .await?
                                    .is_some();
                                anyhow::Ok((cs_id, derived))
                            }
                        })
                        .buffered(100)
                        .try_filter_map(|(cs_id, derived)| async move {
                            Ok(if derived { None } else { Some(cs_id) })
                        })
                        .try_collect()
                        .await?;

                    if gap_parents.is_empty() {
                        // No gaps — stream current slices (which fill the
                        // deepest gap), then process accumulated slices
                        // from earlier iterations in reverse order.
                        let batches_stream = slices_stream
                            .chain(stream::iter(gap_slices.into_iter().rev().map(Ok)))
                            // Segment boundaries need not force a blob and mapping flush.
                            // Combine them into bounded batches, preserving their order.
                            .map_ok(|slice| stream::iter(slice.into_iter().map(Ok::<_, Error>)))
                            .try_flatten()
                            .try_chunks(
                                batch_size
                                    .try_into()
                                    .context("derivation batch size exceeds usize")?,
                            )
                            .map_err(|error| error.1);
                        let mut batches_stream = std::pin::pin!(batches_stream);
                        while let Some(batch) = batches_stream.try_next().await? {
                            count += batch.len() as u64;
                            self.derive_exactly_batch::<Derivable>(
                                &ctx,
                                batch,
                                rederivation.clone(),
                            )
                            .await?;
                        }
                        break;
                    }

                    // Gaps found — collect and defer these slices.
                    let mut slices: Vec<Vec<ChangesetId>> = slices_stream.try_collect().await?;
                    slices.reverse();
                    gap_slices.extend(slices);

                    tracing::warn!(
                        "Derivation of {} found {} gap parent(s) not derived \
                         due to broken monotonicity, filling gaps",
                        Derivable::NAME,
                        gap_parents.len(),
                    );
                    current_heads = gap_parents;
                }

                Ok(count)
            }
        }
        .map_err(|e: DerivationError| SharedDerivationError::from(e))
        .boxed();

        let mut visited = visited.lock().unwrap();

        visited
            .entry(Derivable::VARIANT)
            .or_insert(derivation_future.shared())
            .clone()
    }

    /// Find which ancestors of `csid` are not yet derived, and necessary for
    /// the derivation of `csid` to complete, and derive them.
    async fn derive_underived<Derivable>(
        &self,
        ctx: &CoreContext,
        target_csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<DerivationOutcome<Derivable>, SharedDerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        let count = self
            .derive_heads::<Derivable>(ctx.clone(), vec![target_csid], None, rederivation.clone())
            .await?;

        let derivation_ctx = self.derivation_context(rederivation);

        let derived = Derivable::fetch(ctx, &derivation_ctx, target_csid)
            .await
            .map_err(DerivationError::from)?
            .ok_or_else(|| {
                DerivationError::from(anyhow!(
                    "We just derived it! Fetching it should not return None"
                ))
            })?;
        Ok(DerivationOutcome { derived, count })
    }

    /// Count how many ancestors of `csid` are not yet derived.
    pub async fn count_underived<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<u64, DerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        self.get_manager(ctx, csid)
            .await?
            .count_underived_impl::<Derivable>(ctx, csid, rederivation)
            .await
    }

    async fn count_underived_impl<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<u64, DerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        self.check_enabled::<Derivable>()?;
        let last_derived = self
            .commit_graph()
            .ancestors_frontier_with(ctx, vec![csid], |csid| {
                borrowed!(self as ddm, ctx);
                cloned!(rederivation);
                async move {
                    Ok(ddm
                        .fetch_derived::<Derivable>(ctx, csid, rederivation)
                        .await?
                        .is_some())
                }
            })
            .await
            .map_err(Into::<DerivationError>::into)?;
        let underived_count = self
            .commit_graph()
            .ancestors_difference_segments(ctx, vec![csid], last_derived)
            .await?
            .into_iter()
            .map(|segment| segment.length)
            .sum();
        Ok(underived_count)
    }

    /// Derive or retrieve derived data for a changeset.
    pub async fn derive<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
        priority: DerivationPriority,
    ) -> Result<Derivable, SharedDerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        if let Some(value) = self.fetch_derived(ctx, csid, rederivation.clone()).await? {
            return Ok(value);
        }

        self.check_blocked_derivation::<Derivable>(&[csid])?;

        if let Some(value) = self
            .derive_remotely(ctx, csid, rederivation.clone(), priority)
            .map_err(SharedDerivationError::from)
            .await?
        {
            Ok(value)
        } else {
            self.derive_locally(ctx, csid, rederivation).await
        }
    }

    /// Derive or retrieve derived data for a changeset without requiring data to be
    /// derived for the parents of the changeset.
    pub async fn unsafe_derive_untopologically<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<Derivable, DerivationError>
    where
        Derivable: DerivableUntopologically,
    {
        if let Some(value) = self.fetch_derived(ctx, csid, rederivation.clone()).await? {
            return Ok(value);
        }

        let derivation_ctx = self.derivation_context(rederivation.clone());

        let bonsai = csid
            .load(ctx, derivation_ctx.blobstore())
            .await
            .map_err(Error::from)?;

        let ctx = ctx.clone_and_reset();
        let ctx = self.set_derivation_session_class(ctx)?;

        let mut derived_data_scuba = self.derived_data_scuba::<Derivable>(&ctx);
        derived_data_scuba.add_changeset(&bonsai);

        derived_data_scuba.log_derivation_start(&ctx);

        let predecessor_checks =
            Derivable::PredecessorDependencies::check_dependencies(&ctx, &derivation_ctx, csid)
                .await;
        // If predecessor derived data types are not derived yet, let's derive them
        if let Err(e) = predecessor_checks {
            Derivable::PredecessorDependencies::derive_predecessors(
                self,
                &ctx,
                csid,
                rederivation.clone(),
                &mut HashSet::new(),
            )
            .await
            .context("failed to derive predecessors")
            .context(e)?
        };

        let (derive_stats, derived) =
            Derivable::unsafe_derive_untopologically(&ctx, &derivation_ctx, bonsai)
                .timed()
                .await;
        derivation_ctx.flush(&ctx).await?;

        derived_data_scuba.log_derivation_end(&ctx, &(derive_stats), derived.as_ref().err());

        let derived = derived?;

        let (persist_stats, persisted) = async {
            derived
                .clone()
                .store_mapping(&ctx, &derivation_ctx, csid)
                .await?;
            derivation_ctx.flush(&ctx).await?;
            if let Some(rederivation) = rederivation {
                rederivation.mark_derived(Derivable::VARIANT, csid);
            }
            Ok(())
        }
        .timed()
        .await;

        derived_data_scuba.log_mapping_insertion(
            &ctx,
            None,
            &persist_stats,
            persisted.as_ref().err(),
        );

        persisted?;

        Ok(derived)
    }

    // Derive remotely if possible.
    //
    // Returns `None` if remote derivation is not enabled, failed or timed out, and
    // local derivation should be attempted.
    pub async fn derive_remotely<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
        priority: DerivationPriority,
    ) -> Result<Option<Derivable>, DerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        if let Some(client) = self.derivation_service_client() {
            let mut attempt = 0;
            let started = Instant::now();
            // Total time to wait for remote derivation before giving up (and maybe deriving locally).
            let overall_timeout = Duration::from_millis(justknobs::get_as::<u64>(
                "scm/mononoke_timeouts:remote_derivation_client_timeout_ms",
                None,
            ));
            // The maximum number of times to try remote derivation before giving up.
            const RETRY_ATTEMPTS_LIMIT: u32 = 10;
            // How long to wait before retrying a failed request to the remote derivation service.
            let retry_delay = Duration::from_millis(justknobs::get_as::<u64>(
                "scm/mononoke_timeouts:remote_derivation_client_retry_delay_ms",
                None,
            ));
            let request = DeriveRequest {
                repo_name: self.repo_name().to_string(),
                derived_data_type: DerivedDataType {
                    type_name: Derivable::NAME.to_string(),
                },
                changeset_id: csid.as_ref().to_vec(),
                bubble_id: self.bubble_id().map(|bubble_id| bubble_id.into()),
                config_name: self.config_name(),
                derivation_type: DerivationType::derive_underived(DeriveUnderived {}),
                priority,
            };
            let mut derived_data_scuba = self.derived_data_scuba::<Derivable>(ctx);
            derived_data_scuba.add_changeset_id(csid);

            // Each attempt holds one stream open until its terminal item carries the
            // derivation result.
            let derivation_error = loop {
                if justknobs::eval(
                    "scm/mononoke:derived_data_disable_remote_derivation",
                    None,
                    Some(self.repo_name()),
                ) {
                    // Remote derivation has been disabled, fall back to local derivation.
                    return Ok(None);
                }

                let elapsed = started.elapsed();
                if elapsed >= overall_timeout {
                    derived_data_scuba.log_remote_derivation_end(
                        ctx,
                        Some(format!(
                            "Remote derivation timed out after {overall_timeout:?}"
                        )),
                    );
                    break DerivationError::Timeout(Derivable::NAME, overall_timeout);
                }

                // Fast path: someone derived it (here or elsewhere) already.
                if let Some(data) = self.fetch_derived(ctx, csid, rederivation.clone()).await? {
                    return Ok(Some(data));
                }

                derived_data_scuba.log_remote_derivation_start(ctx);
                let remaining = overall_timeout.saturating_sub(elapsed);
                match tokio::time::timeout(remaining, client.derive_streaming(ctx, &request)).await
                {
                    Ok(Ok(DeriveResponse { data, status })) => match (status, data) {
                        (RequestStatus::SUCCESS, Some(data)) => {
                            derived_data_scuba.log_remote_derivation_end(ctx, None);
                            return Ok(Some(Derivable::from_thrift(data)?));
                        }
                        (RequestStatus::SUCCESS, None) => {
                            derived_data_scuba.log_remote_derivation_end(
                                ctx,
                                Some("Request succeeded but derived data is None".to_string()),
                            );
                            return Ok(None);
                        }
                        // The streaming server only emits a SUCCESS terminal item; any
                        // other status is unexpected.
                        (status, _) => {
                            derived_data_scuba.log_remote_derivation_end(
                                ctx,
                                Some(format!("Unexpected streaming response status: {status:?}")),
                            );
                            return Ok(None);
                        }
                    },
                    // Retry on any error: stream/RPC failure or a derivation failure on the stream.
                    Ok(Err(e)) => {
                        if attempt >= RETRY_ATTEMPTS_LIMIT {
                            derived_data_scuba
                                .log_remote_derivation_end(ctx, Some(format!("{e:#}")));
                            break DerivationError::Failed(Derivable::NAME, attempt, e);
                        }
                        attempt += 1;
                        tokio::time::sleep(retry_delay).await;
                    }
                    // Overall budget exhausted while waiting on the stream.
                    Err(_elapsed) => {
                        derived_data_scuba.log_remote_derivation_end(
                            ctx,
                            Some(format!(
                                "Remote derivation timed out after {overall_timeout:?}"
                            )),
                        );
                        break DerivationError::Timeout(Derivable::NAME, overall_timeout);
                    }
                }
            };

            // Derivation has failed or timed out.
            Err(derivation_error)
        } else {
            // Derivation is not enabled, perform local derivation.
            Ok(None)
        }
    }

    pub async fn derive_locally<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<Derivable, SharedDerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        self.get_manager(ctx, csid)
            .map_err(|e| SharedDerivationError::from(DerivationError::from(e)))
            .await?
            .derive_impl::<Derivable>(ctx, csid, rederivation)
            .await
    }

    async fn derive_impl<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<Derivable, SharedDerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        self.check_enabled::<Derivable>()?;

        let pc = ctx.clone().fork_perf_counters();

        let (stats, res) = self
            .derive_underived(ctx, csid, rederivation)
            .timed()
            .fuse()
            .await;
        self.log_slow_derivation(ctx, csid, &stats, &pc, &res);
        Ok(res?.derived)
    }

    /// Derive data for exactly all underived changesets in a batch.
    ///
    /// The provided batch of changesets must be in topological
    /// order. The caller must have arranged for the dependencies
    /// and ancestors of the batch to have already been derived. If
    /// any dependency or ancestor is not already derived, an error
    /// will be returned.
    pub async fn derive_exactly_underived_batch<Derivable>(
        &self,
        ctx: &CoreContext,
        csids: Vec<ChangesetId>,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<Duration, DerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        let derived = self
            .fetch_derived_batch::<Derivable>(ctx, csids.clone(), rederivation.clone())
            .await?;
        let underived = csids
            .into_iter()
            .filter(|csid| !derived.contains_key(csid))
            .collect();
        self.derive_exactly_batch::<Derivable>(ctx, underived, rederivation)
            .await
    }

    #[async_recursion]
    /// Derive data for exactly a batch of changesets.
    ///
    /// The provided batch of changesets must be in topological
    /// order.
    ///
    /// The difference between "derive_exactly" and "derive", is that for
    /// deriving exactly, the caller must have arranged for the dependencies
    /// and ancestors of the batch to have already been derived. If
    /// any dependency or ancestor is not already derived, an error
    /// will be returned.
    pub async fn derive_exactly_batch<Derivable>(
        &self,
        ctx: &CoreContext,
        csids: Vec<ChangesetId>,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<Duration, DerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        async {
            let (csids, secondary_derivation) = if let Some(secondary_data) = &self.inner.secondary
            {
                let DerivationAssignment { primary, secondary } =
                    secondary_data.assigner.assign(ctx, csids).await?;
                (primary, {
                    cloned!(rederivation);
                    async move {
                        secondary_data
                            .manager
                            .derive_exactly_batch::<Derivable>(ctx, secondary, rederivation)
                            .await
                    }
                    .left_future()
                })
            } else {
                (csids, future::ready(Ok(Duration::ZERO)).right_future())
            };
            self.check_enabled::<Derivable>()?;
            self.check_blocked_derivation::<Derivable>(&csids)?;
            let mut derivation_ctx = self.derivation_context(rederivation.clone());

            // Enable write batching, so that writes are stored in memory
            // before being flushed.
            derivation_ctx.enable_write_batching();
            let derivation_ctx_ref = &derivation_ctx;

            let mut scuba = ctx.scuba().clone();
            scuba
                .add("stack_size", csids.len())
                .add("derived_data", Derivable::NAME);
            if let (Some(first), Some(last)) = (csids.first(), csids.last()) {
                scuba
                    .add("first_csid", first.to_string())
                    .add("last_csid", last.to_string());
            }

            // Load all of the bonsais for this batch.
            let bonsais = stream::iter(csids.iter().cloned().map(|csid| async move {
                let bonsai = csid.load(ctx, derivation_ctx_ref.blobstore()).await?;
                Ok::<_, Error>(bonsai)
            }))
            .buffered(100)
            .try_collect::<Vec<_>>()
            .await?;

            // Dependency checks: check topological order and determine heads
            // and highest ancestors of the batch.
            let mut seen = HashSet::new();
            let mut heads = HashSet::new();
            let mut ancestors = HashMap::new();
            for bonsai in bonsais.iter() {
                let csid = bonsai.get_changeset_id();
                if ancestors.contains_key(&csid) {
                    return Err(anyhow!("batch not in topological order at {csid}").into());
                }
                for parent in bonsai.parents() {
                    if !seen.contains(&parent) {
                        ancestors.entry(parent).or_insert(csid);
                    }
                    heads.remove(&parent);
                }
                seen.insert(csid);
                heads.insert(csid);
            }

            // Dependency checks: all ancestors should have this derived
            // data type derived
            stream::iter(ancestors)
            .map(|(csid, child)| {
                derivation_ctx_ref
                    .fetch_dependency::<Derivable>(ctx, csid)
                    .map_err(move |err| {
                        err.context(format!(
                            "failed to fetch dependency for deriving {} for {} - data was not derived for {}",
                            Derivable::NAME,
                            child,
                            csid
                        ))
                    })
            })
            .buffered(100)
            .try_for_each(|_| async { Ok(()) })
            .await
            .with_context(|| {
                format!(
                    "a batch ancestor does not have '{}' derived",
                    Derivable::NAME
                )
            })
            .context(concat!(
                "derive exactly batch pre-condition not satisfied: ",
                "all ancestors' and dependencies' data must already have been derived",
            ))?;

            // All heads should have their dependent data types derived.
            // Let's check if that's the case
            stream::iter(heads)
                .map(|csid| async move {
                    Derivable::Dependencies::check_dependencies(ctx, derivation_ctx_ref, csid).await
                })
                .buffered(100)
                .try_for_each(|_| async { Ok(()) })
                .await
                .context("a batch dependency has not been derived")?;

            let ctx = ctx.clone_and_reset();
            let ctx = self.set_derivation_session_class(ctx.clone())?;
            borrowed!(ctx);

            let csid_range = if let (Some(first), Some(last)) = (bonsais.first(), bonsais.last()) {
                let first_csid = first.get_changeset_id();
                let last_csid = last.get_changeset_id();
                tracing::debug!(
                    "derive exactly batch from {} to {}",
                    first_csid,
                    last_csid,
                );
                Some((first_csid, last_csid))
            } else {
                None
            };

            let mut derived_data_scuba = self.derived_data_scuba::<Derivable>(ctx);
            derived_data_scuba.add_changesets(&bonsais);
            derived_data_scuba.log_batch_derivation_start(ctx);
            let (overall_stats, result) = async {
                let derivation_ctx_ref = &derivation_ctx;
                let (batch_duration, derived) = {
                    let (stats, derived) =
                        Derivable::derive_batch(ctx, derivation_ctx_ref, bonsais)
                            .try_timed()
                            .await
                            .with_context(|| {
                                if let Some((first, last)) = csid_range {
                                    format!(
                                        "failed to derive {} batch (start:{}, end:{})",
                                        Derivable::NAME,
                                        first,
                                        last
                                    )
                                } else {
                                    format!("failed to derive empty {} batch", Derivable::NAME)
                                }
                            })?;
                    (stats.completion_time, derived)
                };

                // Flush the blobstore.  If it has been set up to cache writes, these
                // must be flushed before we write the mapping.
                derivation_ctx
                    .flush(ctx)
                    .try_timed()
                    .await?
                    .log_future_stats(scuba.clone(), "Flushed derived blobs", None);

                let mut derivation_ctx = self.derivation_context(rederivation.clone());
                derivation_ctx.enable_write_batching();
                // Write all mapping values, and flush the blobstore to ensure they
                // are persisted.
                let (persist_stats, persisted) = async {
                    let derivation_ctx_ref = &derivation_ctx;
                    let csids = if justknobs::eval(
                        "scm/mononoke:derived_data_use_store_mapping_batch",
                        None,
                        None,
                    ) {
                        let csids: Vec<_> = derived.keys().copied().collect();
                        Derivable::store_mapping_batch(
                            ctx,
                            derivation_ctx_ref,
                            derived.into_iter().collect(),
                        )
                        .await?;
                        csids
                    } else {
                        stream::iter(derived.into_iter())
                            .map(|(csid, derived)| async move {
                                derived
                                    .store_mapping(ctx, derivation_ctx_ref, csid)
                                    .await?;
                                Ok::<_, Error>(csid)
                            })
                            .buffer_unordered(100)
                            .try_collect::<Vec<_>>()
                            .await?
                    };

                    derivation_ctx.flush(ctx).await?;
                    if let Some(rederivation) = rederivation {
                        for csid in csids {
                            rederivation.mark_derived(Derivable::VARIANT, csid);
                        }
                    }
                    Ok::<_, Error>(())
                }
                .timed()
                .await;

                derived_data_scuba.log_mapping_insertion(
                    ctx,
                    None,
                    &persist_stats,
                    persisted.as_ref().err(),
                );

                persisted?;

                scuba
                    .add_future_stats(&persist_stats)
                    .log_with_msg("Flushed mapping", None);

                Ok(batch_duration)
            }
            .timed()
            .await;

            derived_data_scuba.log_batch_derivation_end(ctx, &overall_stats, result.as_ref().err());

            let batch_duration = result?;

            Ok(batch_duration + secondary_derivation.await?)
        }
        .instrument(tracing::info_span!("derive", repo = %self.repo_name(), ddt = %Derivable::NAME))
        .await
    }

    /// Derive data for a specific stage of a batch of changesets.
    ///
    /// This method orchestrates derivation pipeline, where a derived data type
    /// is computed in multiple stages (e.g., subtrees of a manifest) that can
    /// run independently across machines.
    ///
    /// The provided batch of changesets must be in topological order.
    /// Parent stage outputs are fetched for parents outside the batch;
    /// the trait implementation handles ordering within the batch.
    ///
    /// Types with cross-type derived-data dependencies are supported; the
    /// dependency data must already be derived for the batch (the derivation
    /// pipeline co-manages dependencies in the same pipeline so they are
    /// derived in lockstep).
    pub async fn derive_stage_batch<Derivable>(
        &self,
        ctx: &CoreContext,
        csids: Vec<ChangesetId>,
        payload: &crate::stage_payload::DerivationStagePayload,
    ) -> Result<Duration, DerivationError>
    where
        Derivable: PipelineDerivable,
    {
        // Map the payload to its stage identity and dependency stages. Both
        // Manifest and Finalize are dispatched to the trait implementation; a
        // type that does not support a given stage fails inside its own trait
        // methods (fetch/derive/store), not here.
        let (stage, dep_paths): (crate::stage_payload::StageId, Vec<MPath>) = match payload {
            crate::stage_payload::DerivationStagePayload::Manifest(manifest_payload) => {
                let stage_path = manifest_payload.path.clone();
                let dep_paths = manifest_payload
                    .deps
                    .iter()
                    .map(|element| stage_path.join(std::iter::once(element)))
                    .collect();
                (
                    crate::stage_payload::StageId::Manifest(stage_path),
                    dep_paths,
                )
            }
            crate::stage_payload::DerivationStagePayload::Finalize => {
                // The finalize stage typically depends on the root manifest
                // stage output (e.g. hg changeset generation needs the root
                // manifest id), so resolve it as a dependency — it lands in
                // `dependency_outputs` keyed at `MPath::ROOT`.
                (crate::stage_payload::StageId::Finalize, vec![MPath::ROOT])
            }
        };
        let stage_display = match &stage {
            crate::stage_payload::StageId::Manifest(path) => path.to_string(),
            crate::stage_payload::StageId::Finalize => "finalize".to_string(),
        };

        async {
            self.check_enabled::<Derivable>()?;

            let mut derivation_ctx = self.derivation_context(None);
            derivation_ctx.enable_write_batching();
            let derivation_ctx_ref = &derivation_ctx;

            let mut derived_data_scuba = self.derived_data_scuba::<Derivable>(ctx);
            derived_data_scuba.add_stage_id(&stage_display);

            let ctx = ctx.clone_and_reset();
            let ctx = self.set_derivation_session_class(ctx)?;
            borrowed!(ctx);

            let (overall_stats, result) = async {
                let setup_start = Instant::now();

                // Load all bonsais for this batch.
                let bonsais = stream::iter(csids.iter().copied().map(async |csid| {
                    let bonsai = csid.load(ctx, derivation_ctx_ref.blobstore()).await?;
                    Ok::<_, Error>(bonsai)
                }))
                .buffered(100)
                .try_collect::<Vec<_>>()
                .await?;

                derived_data_scuba.add_changesets(&bonsais);

                // Collect parent csids that are outside the batch.
                let batch_set: HashSet<ChangesetId> = csids.iter().copied().collect();
                let mut external_parent_csids: Vec<ChangesetId> = vec![];
                for bonsai in bonsais.iter() {
                    for parent in bonsai.parents() {
                        if !batch_set.contains(&parent) {
                            external_parent_csids.push(parent);
                        }
                    }
                }
                external_parent_csids.sort();
                external_parent_csids.dedup();

                // Kick off all fetches concurrently: parents + each dependency stage.
                let parent_fetch = Derivable::fetch_stage_outputs(
                    ctx,
                    derivation_ctx_ref,
                    &stage,
                    external_parent_csids.clone(),
                );

                let dep_fetches = dep_paths.iter().map(async |dep_path| {
                    let outputs = Derivable::fetch_stage_outputs(
                        ctx,
                        derivation_ctx_ref,
                        &crate::stage_payload::StageId::Manifest(dep_path.clone()),
                        csids.clone(),
                    )
                    .await?;
                    Ok::<_, Error>((dep_path.clone(), outputs))
                });

                let (fetched_parents, dep_results) =
                    try_join(parent_fetch, try_join_all(dep_fetches)).await?;

                // Resolve parent stage outputs. For parents missing a stage
                // output (transitionary period where the stage config changed),
                // fetch the full derived value and extract the stage output.
                // The caller must ensure that either the stage output or the
                // full derived value exists for every external parent.
                let fallback: HashMap<ChangesetId, Derivable::StageOutput> =
                    stream::iter(
                        external_parent_csids
                            .into_iter()
                            .filter(|csid| !fetched_parents.contains_key(csid))
                            .map(async |parent_csid| {
                                let derived = derivation_ctx_ref
                                    .fetch_dependency::<Derivable>(ctx, parent_csid)
                                    .await?;
                                let stage_output = Derivable::extract_stage_output_from_derived(
                                    ctx,
                                    derivation_ctx_ref,
                                    &derived,
                                    &stage,
                                )
                                .await?;
                                Ok::<_, Error>((parent_csid, stage_output))
                            }),
                    )
                    .buffered(100)
                    .try_collect()
                    .await?;
                let parents: HashMap<ChangesetId, Derivable::StageOutput> =
                    fetched_parents.into_iter().chain(fallback).collect();

                // Build dependency outputs map from the concurrent fetch results.
                let mut dependency_outputs: HashMap<
                    ChangesetId,
                    HashMap<MPath, Derivable::StageOutput>,
                > = Default::default();
                for (dep_path, dep_outputs) in dep_results {
                    for &csid in &csids {
                        let output = dep_outputs
                            .get(&csid)
                            .ok_or_else(|| {
                                DerivationError::from(anyhow!(
                                    "missing dependency stage output for path {dep_path}, changeset {csid}",
                                ))
                            })?;
                        dependency_outputs
                            .entry(csid)
                            .or_default()
                            .insert(dep_path.clone(), output.clone());
                    }
                }

                derived_data_scuba.log_batch_derivation_start(ctx);

                let setup_duration = setup_start.elapsed();
                derived_data_scuba.add_setup_duration(setup_duration);

                let derivation_ctx_ref = &derivation_ctx;
                let (batch_duration, derived) = {
                    let (stats, derived) = Derivable::derive_stage_batch(
                        ctx,
                        derivation_ctx_ref,
                        bonsais,
                        payload,
                        parents,
                        dependency_outputs,
                    )
                    .try_timed()
                    .await
                    .with_context(|| {
                        format!(
                            "failed to derive {} stage '{}' batch",
                            Derivable::NAME,
                            stage_display,
                        )
                    })?;
                    (stats.completion_time, derived)
                };

                // Flush blobstore before persisting outputs.
                derivation_ctx.flush(ctx).await?;

                // Store stage outputs with a fresh write-batching context.
                let mut store_ctx = self.derivation_context(None);
                store_ctx.enable_write_batching();

                Derivable::store_stage_outputs(
                    ctx,
                    &store_ctx,
                    &stage,
                    derived.clone(),
                )
                .await
                .with_context(|| {
                        format!(
                            "failed to store {} stage '{}' outputs",
                            Derivable::NAME,
                            stage_display,
                        )
                    })?;

                store_ctx.flush(ctx).await?;

                Ok(batch_duration)
            }
            .timed()
            .await;

            derived_data_scuba.log_batch_derivation_end(ctx, &overall_stats, result.as_ref().err());

            Ok(result?)
        }
        .instrument(tracing::info_span!(
            "derive_stage",
            repo = %self.repo_name(),
            ddt = %Derivable::NAME,
            stage = %stage_display,
        ))
        .await
    }

    /// Check if a specific derivation pipeline stage has been derived for a changeset.
    pub async fn is_stage_derived<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        stage: &crate::stage_payload::StageId,
    ) -> Result<bool, DerivationError>
    where
        Derivable: PipelineDerivable,
    {
        let derivation_ctx = self.derivation_context(None);
        let outputs =
            Derivable::fetch_stage_outputs(ctx, &derivation_ctx, stage, vec![csid]).await?;
        Ok(outputs.contains_key(&csid))
    }

    /// Verify that a stage output is consistent with the canonical derived
    /// value, delegating to the type's `verify_stage` implementation.
    pub async fn verify_stage_output<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        stage: &crate::stage_payload::StageId,
    ) -> Result<bool, DerivationError>
    where
        Derivable: PipelineDerivable,
    {
        let derivation_ctx = self.derivation_context(None);
        Ok(Derivable::verify_stage(ctx, &derivation_ctx, csid, stage).await?)
    }

    /// Fetch derived data for a changeset if it has previously been derived.
    pub async fn fetch_derived<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<Option<Derivable>, DerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        self.get_manager(ctx, csid)
            .await?
            .fetch_derived_impl::<Derivable>(ctx, csid, rederivation)
            .await
    }

    async fn fetch_derived_impl<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<Option<Derivable>, DerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        self.check_readable::<Derivable>()?;
        let derivation_ctx = self.derivation_context(rederivation);
        let derived = derivation_ctx.fetch_derived::<Derivable>(ctx, csid).await?;
        Ok(derived)
    }

    /// Fetch the underlying value of derived data for a changeset if it has previously been derived
    /// directly without going through the mapping.
    pub async fn fetch_derived_direct<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<Option<Derivable::Value>, DerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        self.get_manager(ctx, csid)
            .await?
            .fetch_derived_direct_impl::<Derivable>(ctx, csid, rederivation)
            .await
    }

    async fn fetch_derived_direct_impl<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<Option<Derivable::Value>, DerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        self.check_readable::<Derivable>()?;
        let derivation_ctx = self.derivation_context(rederivation);
        let derived = derivation_ctx
            .fetch_derived_direct::<Derivable>(ctx, csid)
            .await?;
        Ok(derived)
    }

    #[async_recursion]
    /// Fetch derived data for a batch of changesets if they have previously
    /// been derived.
    ///
    /// Returns a hashmap from changeset id to the derived data.  Changesets
    /// for which the data has not previously been derived are omitted.
    pub async fn fetch_derived_batch<Derivable>(
        &self,
        ctx: &CoreContext,
        csids: Vec<ChangesetId>,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<HashMap<ChangesetId, Derivable>, DerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        let (csids, secondary_derivation) = if let Some(secondary_data) = &self.inner.secondary {
            let DerivationAssignment { primary, secondary } =
                secondary_data.assigner.assign(ctx, csids).await?;
            (primary, {
                cloned!(rederivation);
                async move {
                    secondary_data
                        .manager
                        .fetch_derived_batch::<Derivable>(ctx, secondary, rederivation)
                        .await
                }
                .left_future()
            })
        } else {
            (csids, future::ready(Ok(HashMap::new())).right_future())
        };
        self.check_readable::<Derivable>()?;
        let derivation_ctx = self.derivation_context(rederivation);
        let mut derived = derivation_ctx
            .fetch_derived_batch::<Derivable>(ctx, csids)
            .await?;
        derived.extend(secondary_derivation.await?);
        Ok(derived)
    }
}

pub(super) struct DerivationOutcome<Derivable> {
    /// The derived data.
    pub(super) derived: Derivable,

    /// Number of changesets that were derived.
    pub(super) count: u64,
}
