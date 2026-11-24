/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! AsyncMethodRequestWorker is an object that provides everything that's needed
//! for processing the requests from the queue.
//!
//! In can grab requests from the queue, compute the result and update the
//! requests table with a response.
//! One important consideration to keep in mind - worker executes request "at least once"
//! but not exactly once i.e. the same request might be executed a few times.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Error;
use anyhow::Result;
use async_requests::AbandonedRequestAction;
use async_requests::AsyncMethodRequestQueue;
use async_requests::AsyncRequestsError;
use async_requests::ClaimedBy;
use async_requests::DequeuedRequest;
use async_requests::RequestId;
use async_requests::types::AsynchronousRequestParams;
use async_requests::types::RowId;
use async_stream::stream;
use async_trait::async_trait;
use cloned::cloned;
use context::CoreContext;
use executor_lib::RepoShardedProcessExecutor;
use futures::Stream;
use futures::future::Either;
use futures::future::abortable;
use futures::future::select;
use futures::pin_mut;
use futures::stream::StreamExt;
use futures_stats::TimedFutureExt;
use hostname::get_hostname;
use megarepo_api::MegarepoApi;
use mononoke_api::Mononoke;
use mononoke_api::MononokeRepo;
use mononoke_api::Repo;
use mononoke_app::MononokeReposManager;
use mononoke_macros::mononoke;
use mononoke_repos::MononokeRepos;
use mononoke_types::RepositoryId;
use mononoke_types::Timestamp;
use rand::RngExt as _;
use stats::define_stats;
use stats::prelude::*;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::methods::megarepo_async_request_compute;
use crate::scuba::log_repo_load_failure;
use crate::scuba::log_result;
use crate::scuba::log_retriable_error;
use crate::scuba::log_start;
use crate::stats::stats_loop;

/// Decrement the refcount for an on-demand loaded repo. When the count drops
/// to zero the repo is removed from memory via `remove_repo`.
/// This is a standalone function so it can be called both through `&self` and
/// after `self` has been partially moved.
fn release_ondemand_repo_impl(
    repo_id: Option<RepositoryId>,
    ondemand_repo_refs: &Mutex<HashMap<RepositoryId, OndemandRepoRef>>,
    remove_repo: impl FnOnce(&str),
) {
    let repo_id = match repo_id {
        Some(id) => id,
        None => return,
    };

    let mut refs = ondemand_repo_refs.lock().expect("poisoned mutex");
    if let Some(repo_ref) = refs.get_mut(&repo_id) {
        if repo_ref.count == 1 {
            let name = repo_ref.name.clone();
            info!("Removing on-demand repo {} (id={})", name, repo_id);
            // Keep the entry locked until the repo has been removed. Otherwise,
            // a concurrent request can mistake the still-present repo for a
            // pre-configured repo and use it without taking a reference.
            remove_repo(&name);
            refs.remove(&repo_id);
        } else {
            repo_ref.count -= 1;
        }
    }
}

struct OndemandRepoRef {
    name: String,
    count: usize,
}

/// Ensure the repo for a request is loaded. Returns true if this is an
/// on-demand repo that must be released after processing.
///
/// `repo_name` resolves an unloaded repo's name; `remove_repo` unloads it if
/// the load fails and this was its last request.
async fn ensure_ondemand_repo_loaded<R>(
    repos: &MononokeRepos<R>,
    ondemand_repo_refs: &Mutex<HashMap<RepositoryId, OndemandRepoRef>>,
    repo_id: Option<RepositoryId>,
    repo_name: impl FnOnce(RepositoryId) -> Result<String>,
    remove_repo: impl FnOnce(&str),
) -> Result<bool, Arc<Error>>
where
    R: Send + Sync + 'static,
{
    let repo_id = match repo_id {
        Some(id) => id,
        None => return Ok(false),
    };

    let repo_name = {
        let mut refs = ondemand_repo_refs.lock().expect("poisoned mutex");
        if let Some(repo_ref) = refs.get_mut(&repo_id) {
            repo_ref.count += 1;
            repo_ref.name.clone()
        } else {
            // Pre-configured repo — no on-demand management needed. The
            // worker's `Mononoke` is built over this same repo set, so this is
            // the lookup `Mononoke::raw_repo_by_id` does.
            if repos.get_by_id(repo_id.id()).is_some() {
                return Ok(false);
            }
            let repo_name = repo_name(repo_id).map_err(|e| {
                Arc::new(Error::msg(format!(
                    "No config found for repo_id {repo_id}: {e}"
                )))
            })?;
            // Registered under the refs lock so a concurrent release cannot
            // remove the repo before this request holds its reference.
            info!("Loading repo {} (id={}) on-demand", repo_name, repo_id);
            repos.add_lazy_repo(&repo_name, repo_id.id());
            refs.insert(
                repo_id,
                OndemandRepoRef {
                    name: repo_name.clone(),
                    count: 1,
                },
            );
            repo_name
        }
    };

    // Concurrent requests for the same repo share one detached build.
    let load_result = match repos.get(&repo_name).await {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(Error::msg(format!(
            "On-demand repo {repo_name} (id={repo_id}) was removed while loading"
        ))),
        Err(error) => Err(error),
    };

    // A failed load is released like a finished request: the last waiter
    // removes the repo, so the next request starts a fresh build.
    if let Err(error) = load_result {
        release_ondemand_repo_impl(Some(repo_id), ondemand_repo_refs, remove_repo);
        return Err(Arc::new(error));
    }

    Ok(true)
}

const DEQUEUE_STREAM_SLEEP_TIME: u64 = 1000;
// JustKnob controlling how many seconds an inprogress request may go without a
// keep-alive update before another worker reclaims it as abandoned. Tunable at
// runtime with no code push, and lowered by tests to exercise recovery quickly;
// the default (300s) lives in `just_knobs.json`. Must stay well above
// `KEEP_ALIVE_INTERVAL` so a healthy worker never reclaims its own requests.
const JK_ABANDONED_REQUEST_THRESHOLD_SECS: &str =
    "scm/mononoke:async_requests_abandoned_request_threshold_secs";
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(10);
const CONCURRENCY_LIMIT_BACKOFF_BASE: Duration = Duration::from_secs(15);
const CONCURRENCY_LIMIT_BACKOFF_MAX_JITTER_SECS: u64 = 15;

define_stats! {
    prefix = "async_requests.worker";
    dequeue_called: timeseries("dequeue.called"; Count),
    cleanup_error: timeseries("cleanup.error"; Count),
    cleanup_request_failed: timeseries("cleanup.request_failed"; Count),
    dequeue_error: timeseries("dequeue.error"; Count),
    process_aborted: timeseries("process.aborted"; Count),
    process_failed: timeseries("process.failed"; Count),
    requested: timeseries("requested"; Count),
}

#[derive(Clone)]
pub struct AsyncMethodRequestWorker {
    ctx: Arc<CoreContext>,
    repos_mgr: Arc<MononokeReposManager<Repo>>,
    mononoke: Arc<Mononoke<Repo>>,
    megarepo: Arc<MegarepoApi<Repo>>,
    name: String,
    queue: Arc<AsyncMethodRequestQueue>,
    will_exit: Arc<AtomicBool>,
    limit: Option<usize>,
    concurrency_limit: usize,
    /// Tracks the number of in-flight tasks using each on-demand loaded repo.
    /// Only call `repos_mgr.remove_repo()` when the count drops to zero.
    ondemand_repo_refs: Arc<Mutex<HashMap<RepositoryId, OndemandRepoRef>>>,
}

impl AsyncMethodRequestWorker {
    pub async fn new(
        request_limit: Option<usize>,
        jobs: usize,
        ctx: Arc<CoreContext>,
        queue: Arc<AsyncMethodRequestQueue>,
        repos_mgr: Arc<MononokeReposManager<Repo>>,
        mononoke: Arc<Mononoke<Repo>>,
        megarepo: Arc<MegarepoApi<Repo>>,
        will_exit: Arc<AtomicBool>,
    ) -> Result<Self, Error> {
        let name = {
            let tw_job_cluster = std::env::var("TW_JOB_CLUSTER");
            let tw_job_user = std::env::var("TW_JOB_USER");
            let tw_job_name = std::env::var("TW_JOB_NAME");
            let tw_task_id = std::env::var("TW_TASK_ID");
            match (tw_job_cluster, tw_job_user, tw_job_name, tw_task_id) {
                (Ok(tw_job_cluster), Ok(tw_job_user), Ok(tw_job_name), Ok(tw_task_id)) => {
                    format!("{tw_job_cluster}/{tw_job_user}/{tw_job_name}/{tw_task_id}")
                }
                _ => format!(
                    "async_requests_worker/{}",
                    get_hostname().unwrap_or_else(|_| "unknown_hostname".to_string())
                ),
            }
        };

        Ok(Self {
            ctx,
            repos_mgr,
            mononoke,
            megarepo,
            name,
            queue,
            will_exit,
            limit: request_limit,
            concurrency_limit: jobs,
            ondemand_repo_refs: Arc::new(Mutex::new(HashMap::new())),
        })
    }
}

#[async_trait]
impl RepoShardedProcessExecutor for AsyncMethodRequestWorker {
    /// Start async request worker.
    /// If limit is set the worker will process a preset number of requests and
    /// return. If the limit is None the worker will be running continuously. The
    /// will_exit atomic bool is a flag to prevent the worker from grabbing new
    /// items from the queue and gracefully terminate.
    async fn execute(&self) -> Result<()> {
        // Start the stats logger loop
        let (stats, stats_abort_handle) = abortable({
            cloned!(self.ctx, self.queue);
            async move { stats_loop(&ctx, &queue).await }
        });
        let _stats = mononoke::spawn_task(stats);

        // Build stream that pools all the queues
        let request_stream = self
            .request_stream(&self.ctx, self.queue.clone(), self.will_exit.clone())
            .boxed();

        let request_stream = if let Some(limit) = self.limit {
            request_stream.take(limit).left_stream()
        } else {
            request_stream.right_stream()
        };

        info!("Worker initialization complete, starting request processing loop.",);

        request_stream
            .for_each_concurrent(Some(self.concurrency_limit), |dequeued| async move {
                let worker = self.clone();
                let ctx = CoreContext::clone(&self.ctx);
                let run_detached = dequeued.params.is_long_running();
                let handle = mononoke::spawn_task(worker.compute_and_mark_completed(
                    ctx,
                    dequeued.id,
                    dequeued.repo_id,
                    dequeued.params,
                    dequeued.root_request_id,
                    dequeued.created_by,
                ));
                if !run_detached {
                    if let Err(e) = handle.await {
                        warn!("Error spawning request: {:?}", e);
                    }
                }
            })
            .await;

        info!("Worker exiting");

        stats_abort_handle.abort();

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("Worker stopping");

        Ok(())
    }
}

impl AsyncMethodRequestWorker {
    pub fn request_stream(
        &self,
        ctx: &CoreContext,
        queue: Arc<AsyncMethodRequestQueue>,
        will_exit: Arc<AtomicBool>,
    ) -> impl Stream<Item = DequeuedRequest> + use<> {
        let claimed_by = ClaimedBy(self.name.clone());
        let sleep_time = Duration::from_millis(DEQUEUE_STREAM_SLEEP_TIME);
        let abandoned_threshold_secs = justknobs::get(JK_ABANDONED_REQUEST_THRESHOLD_SECS, None);
        Self::request_stream_inner(
            ctx.clone(),
            claimed_by,
            queue,
            will_exit,
            sleep_time,
            abandoned_threshold_secs,
        )
    }

    fn request_stream_inner(
        ctx: CoreContext,
        claimed_by: ClaimedBy,
        queue: Arc<AsyncMethodRequestQueue>,
        will_exit: Arc<AtomicBool>,
        sleep_time: Duration,
        abandoned_threshold_secs: i64,
    ) -> impl Stream<Item = DequeuedRequest> {
        stream! {
            loop {
                STATS::dequeue_called.add_value(1);

                if let Err(e) =
                    Self::cleanup_abandoned_requests(&ctx, &queue, abandoned_threshold_secs).await
                {
                    STATS::cleanup_error.add_value(1);
                    warn!("error while cleaning up abandoned requests, skipping: {}", e);
                };

                if will_exit.load(Ordering::Relaxed) {
                    break;
                }

                match queue.dequeue(&ctx, &claimed_by).await {
                    Err(e) => {
                        STATS::dequeue_error.add_value(1);
                        warn!("error while dequeueing, skipping: {:?}", e);
                        tokio::time::sleep(sleep_time).await;
                    }
                    Ok(Some(dequeued)) => {
                        yield dequeued;
                    }
                    Ok(None) => {
                        // No requests in the queues, sleep before trying again.
                        debug!("nothing to do, sleeping",);
                        tokio::time::sleep(sleep_time).await;
                    }
                }
            }
        }
    }

    async fn cleanup_abandoned_requests(
        ctx: &CoreContext,
        queue: &AsyncMethodRequestQueue,
        abandoned_threshold_secs: i64,
    ) -> Result<(), AsyncRequestsError> {
        let now = Timestamp::now();
        let abandoned_timestamp =
            Timestamp::from_timestamp_secs(now.timestamp_seconds() - abandoned_threshold_secs);
        let requests = queue
            .find_abandoned_requests(ctx, abandoned_timestamp)
            .await?;
        if !requests.is_empty() {
            ctx.scuba().clone().log_with_msg(
                "Find requests to abandon",
                Some(format!("{}", requests.len())),
            );
        }

        for req_id in requests {
            match queue
                .mark_abandoned_request_as_new(ctx, req_id.clone(), abandoned_timestamp)
                .await?
            {
                AbandonedRequestAction::Requeued => {
                    ctx.scuba()
                        .clone()
                        .add("request_id", req_id.0.0)
                        .log_with_msg("Abandoned request", None);
                }
                AbandonedRequestAction::Failed => {
                    STATS::cleanup_request_failed.add_value(1);
                    warn!(
                        "[{}] abandoned request is out of retries, failing it",
                        req_id.0.0
                    );
                    ctx.scuba()
                        .clone()
                        .add("request_id", req_id.0.0)
                        .log_with_msg("Abandoned request out of retries", None);
                }
                AbandonedRequestAction::Skipped => {}
            }
        }
        Ok(())
    }

    /// Ensure the repo for this request is loaded. Returns true if this is
    /// an on-demand repo that should be released after processing via
    /// `release_ondemand_repo`.
    async fn ensure_repo_loaded(&self, repo_id: Option<RepositoryId>) -> Result<bool, Arc<Error>> {
        let repos_mgr = &self.repos_mgr;
        ensure_ondemand_repo_loaded(
            repos_mgr.repos(),
            &self.ondemand_repo_refs,
            repo_id,
            |repo_id| {
                let (repo_name, _) = repos_mgr
                    .configs()
                    .get_or_load_repo_config_by_id(repo_id.id())?;
                Ok(repo_name)
            },
            |repo_name| repos_mgr.remove_repo(repo_name),
        )
        .await
    }

    /// Decrement the refcount for an on-demand repo. When it reaches zero,
    /// remove the repo from memory.
    fn release_ondemand_repo(&self, repo_id: Option<RepositoryId>) {
        release_ondemand_repo_impl(repo_id, &self.ondemand_repo_refs, |repo_name| {
            self.repos_mgr.remove_repo(repo_name)
        });
    }

    fn concurrency_limit_backoff() -> Duration {
        let jitter_secs = rand::rng().random_range(0..=CONCURRENCY_LIMIT_BACKOFF_MAX_JITTER_SECS);
        CONCURRENCY_LIMIT_BACKOFF_BASE + Duration::from_secs(jitter_secs)
    }

    /// Params into stored response. Doesn't mark it as "in progress" (as this is done during dequeueing).
    /// Returns true if the result was successfully stored. Returns false if we
    /// lost the race (the request table was updated).
    async fn compute_and_mark_completed(
        self,
        ctx: CoreContext,
        req_id: RequestId,
        repo_id: Option<RepositoryId>,
        params: AsynchronousRequestParams,
        root_request_id: Option<RowId>,
        created_by: Option<String>,
    ) {
        // Prepare the request context before doing any work, so that all
        // logging carries the request fields.
        let target = match params.target() {
            Ok(target) => target,
            Err(err) => {
                STATS::process_failed.add_value(1);
                error!("Error getting target: {:?}", err);
                return;
            }
        };
        let ctx = self.prepare_ctx(
            &ctx,
            &req_id,
            &target,
            root_request_id,
            created_by.as_deref(),
        );

        // Load the repo on-demand if needed
        let is_ondemand = match self.ensure_repo_loaded(repo_id).await {
            Ok(v) => v,
            Err(err) => {
                STATS::process_failed.add_value(1);
                error!("[{}] Failed to load repo for request: {:?}", &req_id.0, err);
                log_repo_load_failure(&ctx, repo_id, err.as_ref());
                if let Err(e) = self.queue.requeue(&ctx, req_id).await {
                    error!("Failed to requeue after repo load failure: {:?}", e);
                }
                return;
            }
        };
        let cleanup_repo_id = if is_ondemand { repo_id } else { None };

        // Check concurrency limit for this request type. If exceeded,
        // hold the claim briefly before requeueing so the same hot request
        // does not immediately churn across the whole worker fleet.
        match self.queue.concurrency_limit_reached(&ctx, &req_id.1).await {
            Ok(true) => {
                let row_id = req_id.0;
                let backoff = Self::concurrency_limit_backoff();
                info!(
                    "[{}] Concurrency limit reached for {}, backing off for {:?}",
                    &row_id, &req_id.1.0, backoff,
                );
                ctx.scuba()
                    .clone()
                    .add("backoff_ms", backoff.as_millis() as i64)
                    .log_with_msg("Request throttled by concurrency limit", None);
                tokio::time::sleep(backoff).await;
                if let Err(requeue_err) = self.queue.requeue(&ctx, req_id).await {
                    error!(
                        "[{}] Failed to requeue request after concurrency limit: {:?}",
                        &row_id, requeue_err
                    );
                }
                self.release_ondemand_repo(cleanup_repo_id);
                return;
            }
            Err(e) => {
                error!("[{}] Failed to check concurrency limit: {:?}", &req_id.0, e);
            }
            _ => {}
        }

        log_start(&ctx);

        // Save refs for cleanup after self is partially moved.
        let ondemand_repo_refs = self.ondemand_repo_refs.clone();
        let repos_mgr_for_cleanup = self.repos_mgr.clone();
        // Do the actual work.
        STATS::requested.add_value(1);
        let is_long_running = params.is_long_running();
        let work_fut = megarepo_async_request_compute(
            &ctx,
            self.mononoke,
            &self.megarepo,
            &self.queue,
            &req_id.0,
            root_request_id,
            params,
            created_by,
        )
        .timed();

        // Start the loop that would keep saying that request is still being
        // processed
        let (keep_alive, keep_alive_abort_handle) = abortable({
            cloned!(ctx, req_id, self.queue);
            async move { Self::keep_alive_loop(&ctx, &req_id, &queue).await }
        });

        let keep_alive = mononoke::spawn_task(keep_alive);

        pin_mut!(work_fut);
        pin_mut!(keep_alive);
        match select(work_fut, keep_alive).await {
            Either::Left(((stats, result), _)) => {
                // We completed the request - let's mark it as complete
                keep_alive_abort_handle.abort();
                info!(
                    "[{}] request complete, saving result (processed: {})",
                    &req_id.0,
                    result.is_ok()
                );

                // Save the result.
                match result {
                    Ok(work_result) => {
                        let complete_result = self
                            .queue
                            .complete(&ctx, &req_id, work_result.clone())
                            .await;
                        log_result(ctx.clone(), &stats, &work_result, &complete_result);
                        match complete_result {
                            Ok(updated) => {
                                info!("[{}] result saved (updated: {})", &req_id.0, updated);
                            }
                            Err(err) => {
                                error!("[{}] failed to save success result: {:?}", &req_id.0, err);
                            }
                        };
                    }
                    Err(err) => {
                        let err_result = self.queue.retry(&ctx, &req_id).await;
                        match err_result {
                            Ok(will_retry) => {
                                if will_retry {
                                    info!(
                                        "[{}] worker failed to process request, will retry: {:?}",
                                        &req_id.0, err
                                    );
                                } else {
                                    // Retries exhausted — failure has been cascaded
                                    // to any dependent requests atomically.
                                    info!(
                                        "[{}] worker failed to process request, maximum retry attempts reached, failure cascaded: {:?}",
                                        &req_id.0, err
                                    );
                                }
                            }
                            Err(err) => {
                                error!(
                                    "[{}] failed to process retry attempt: {:?}",
                                    &req_id.0, err
                                );
                            }
                        }

                        log_retriable_error(ctx.clone(), &stats, err);
                    }
                }
            }
            Either::Right((_, _)) => {
                // We haven't completed the request, and failed to update inprogress
                // timestamp. Most likely another worker completed it, or an abort
                // marked it done.
                STATS::process_aborted.add_value(1);
                info!("[{}] was completed by other worker, stopping", &req_id.0);

                // If this is the long-lived backfill scheduler, being dropped here
                // means the root was aborted (an abort marks it ready, which is what
                // ended the keep-alive loop). Since a dropped future can't run its
                // own async cleanup, re-run the child abort here so any repos the
                // scheduler enqueued in a race with the abort are aborted too. This
                // is identical to the CLI `backfill-abort` child pass and is
                // idempotent if the CLI already cleaned them up.
                if is_long_running {
                    match self.queue.abort_children_by_root_id(&ctx, &req_id.0).await {
                        Ok((failed, aborted, skipped)) => info!(
                            "[{}] aborted backfill scheduler: cleaned up children ({} failed, {} aborted, {} skipped)",
                            &req_id.0, failed, aborted, skipped,
                        ),
                        Err(err) => error!(
                            "[{}] failed to clean up children of aborted backfill scheduler: {:?}",
                            &req_id.0, err,
                        ),
                    }
                }
            }
        }

        // Release on-demand repo (decrement refcount, remove when zero).
        release_ondemand_repo_impl(cleanup_repo_id, &ondemand_repo_refs, |repo_name| {
            repos_mgr_for_cleanup.remove_repo(repo_name)
        });
    }

    async fn keep_alive_loop(
        ctx: &CoreContext,
        req_id: &RequestId,
        queue: &AsyncMethodRequestQueue,
    ) {
        loop {
            let mut scuba = ctx.scuba().clone();
            ctx.perf_counters().insert_perf_counters(&mut scuba);

            let res = queue.update_in_progress_timestamp(ctx, req_id).await;
            match res {
                Ok(res) => {
                    // Weren't able to update inprogress timestamp - that probably means
                    // that request was completed by someone else. Exiting
                    if !res {
                        scuba.log_with_msg(
                            "Race while updating inprogress timestamp, exiting keep-alive loop",
                            None,
                        );
                        break;
                    }
                    scuba.log_with_msg("Updated inprogress timestamp", None);
                }
                Err(err) => {
                    error!(
                        "[{}] failed to update inprogress timestamp: {:?}",
                        req_id.0, err
                    );
                    scuba.log_with_msg(
                        "Failed to update inprogress timestamp",
                        Some(format!("{err:?}")),
                    );
                }
            }
            tokio::time::sleep(KEEP_ALIVE_INTERVAL).await;
        }
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use anyhow::Error;
    use fbinit::FacebookInit;
    use futures::FutureExt;
    use futures::future::BoxFuture;
    use mononoke_api::RepositoryId;
    use mononoke_macros::mononoke;
    use mononoke_repos::RepoLoader;
    use requests_table::RequestType;
    use source_control as thrift;

    use super::*;

    // A representative threshold for exercising `request_stream_inner` directly.
    const ABANDONED_REQUEST_THRESHOLD_SECS: i64 = 5 * 60;

    const ONDEMAND_REPO: &str = "repo";

    fn ondemand_repo_id() -> RepositoryId {
        RepositoryId::new(1)
    }

    struct TestRepo;

    /// Counts loads, fails the first `fail_first` of them, and holds each one
    /// until the test lets it finish.
    struct Gate {
        calls: AtomicUsize,
        fail_first: usize,
        started: tokio::sync::Notify,
        release: tokio::sync::Semaphore,
    }

    impl Gate {
        fn held(fail_first: usize) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                fail_first,
                started: tokio::sync::Notify::new(),
                release: tokio::sync::Semaphore::new(0),
            })
        }

        fn open(fail_first: usize) -> Arc<Self> {
            let gate = Self::held(fail_first);
            gate.let_through();
            gate
        }

        fn let_through(&self) {
            self.release
                .add_permits(tokio::sync::Semaphore::MAX_PERMITS);
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        async fn load(&self) -> Result<TestRepo> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            self.release
                .acquire()
                .await
                .expect("the gate is never closed")
                .forget();
            if call < self.fail_first {
                Err(Error::msg("repo load failed"))
            } else {
                Ok(TestRepo)
            }
        }
    }

    struct GatedLoader(Arc<Gate>);

    impl RepoLoader<TestRepo> for GatedLoader {
        fn load(&self, _repo_name: String) -> BoxFuture<'static, Result<TestRepo>> {
            let gate = self.0.clone();
            async move { gate.load().await }.boxed()
        }
    }

    /// A real `MononokeRepos` whose repo is built by a `Gate`, plus the
    /// worker's refcounts.
    struct Ondemand {
        repos: MononokeRepos<TestRepo>,
        refs: Mutex<HashMap<RepositoryId, OndemandRepoRef>>,
    }

    impl Ondemand {
        fn new(gate: &Arc<Gate>) -> Arc<Self> {
            Arc::new(Self {
                repos: MononokeRepos::new_lazy(Arc::new(GatedLoader(gate.clone()))),
                refs: Mutex::new(HashMap::new()),
            })
        }
    }

    impl Ondemand {
        fn request(self: &Arc<Self>) -> tokio::task::JoinHandle<Result<bool, Arc<Error>>> {
            let ondemand = self.clone();
            mononoke::spawn_task(async move {
                ensure_ondemand_repo_loaded(
                    &ondemand.repos,
                    &ondemand.refs,
                    Some(ondemand_repo_id()),
                    |_| Ok(ONDEMAND_REPO.to_string()),
                    |repo_name| ondemand.repos.remove(repo_name),
                )
                .await
            })
        }

        fn release(&self) {
            release_ondemand_repo_impl(Some(ondemand_repo_id()), &self.refs, |repo_name| {
                self.repos.remove(repo_name)
            });
        }

        fn is_loaded(&self) -> bool {
            self.repos.get_by_name(ONDEMAND_REPO).is_some()
        }

        fn is_registered(&self) -> bool {
            self.repos.iter_names().any(|name| name == ONDEMAND_REPO)
        }

        fn refcount(&self) -> Option<usize> {
            self.refs
                .lock()
                .expect("poisoned mutex")
                .get(&ondemand_repo_id())
                .map(|repo_ref| repo_ref.count)
        }
    }

    /// Lets every other task on the test's single-threaded runtime run until
    /// it blocks.
    async fn settle() {
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
    }

    #[mononoke::test]
    async fn test_ondemand_requests_share_one_inflight_load() -> Result<()> {
        let gate = Gate::held(0);
        let ondemand = Ondemand::new(&gate);

        let first = ondemand.request();
        gate.started.notified().await;
        let second = ondemand.request();
        settle().await;

        assert!(
            !second.is_finished(),
            "a request must wait for the in-flight load, neither skipping nor finishing it"
        );
        assert!(
            !ondemand.is_loaded(),
            "the repo must not be served mid-load"
        );
        assert_eq!(
            ondemand.refcount(),
            Some(2),
            "both requests hold a reference"
        );

        gate.let_through();
        assert!(
            first
                .await?
                .expect("the first request should load the repo")
        );
        assert!(
            second
                .await?
                .expect("the second request should share the load")
        );
        assert!(ondemand.is_loaded());
        assert_eq!(gate.calls(), 1, "two concurrent requests must load once");
        Ok(())
    }

    #[mononoke::test]
    async fn test_ondemand_failed_load_reaches_every_waiter_then_is_retried() -> Result<()> {
        let gate = Gate::held(1);
        let ondemand = Ondemand::new(&gate);

        let first = ondemand.request();
        gate.started.notified().await;
        let second = ondemand.request();
        settle().await;
        assert!(
            !second.is_finished(),
            "the second request waits on the first load"
        );

        gate.let_through();
        let errors = [
            first
                .await?
                .expect_err("the first request should see the failure"),
            second
                .await?
                .expect_err("the second request should see the same failure"),
        ];
        for error in &errors {
            assert!(
                format!("{error:#}").contains("repo load failed"),
                "every waiter should get the load's own error, got: {error:#}"
            );
        }
        assert_eq!(
            gate.calls(),
            1,
            "both waiters must share the one failed load"
        );
        assert_eq!(
            ondemand.refcount(),
            None,
            "every waiter must drop its reference"
        );
        assert!(
            !ondemand.is_registered(),
            "a failed load must not leave the repo behind"
        );

        assert!(
            ondemand
                .request()
                .await?
                .expect("the next request should load afresh and succeed")
        );
        assert_eq!(
            gate.calls(),
            2,
            "the failure must not be cached past its waiters"
        );
        Ok(())
    }

    #[mononoke::test]
    async fn test_ondemand_repo_is_unloaded_by_last_release_and_reloaded_after() -> Result<()> {
        let gate = Gate::open(0);
        let ondemand = Ondemand::new(&gate);

        assert!(ondemand.request().await?.expect("load should succeed"));
        assert!(
            ondemand
                .request()
                .await?
                .expect("loaded repo should be shared")
        );
        assert_eq!(ondemand.refcount(), Some(2));
        assert_eq!(gate.calls(), 1, "a loaded repo must not be loaded again");

        ondemand.release();
        assert!(ondemand.is_loaded(), "a repo still in use must stay loaded");

        ondemand.release();
        assert!(
            !ondemand.is_registered(),
            "the last release should unload the repo"
        );
        assert_eq!(ondemand.refcount(), None);
        assert_eq!(gate.calls(), 1, "releasing must not load anything");

        assert!(ondemand.request().await?.expect("reload should succeed"));
        assert_eq!(
            gate.calls(),
            2,
            "a request after the unload must load again"
        );
        Ok(())
    }

    #[mononoke::test]
    async fn test_ondemand_load_survives_its_first_request_being_dropped() -> Result<()> {
        let gate = Gate::held(0);
        let ondemand = Ondemand::new(&gate);

        let first = ondemand.request();
        gate.started.notified().await;
        first.abort();
        assert!(
            first
                .await
                .expect_err("the request was aborted")
                .is_cancelled()
        );

        let second = ondemand.request();
        settle().await;
        gate.let_through();
        assert!(second.await?.expect("the next request should get the repo"));
        assert_eq!(
            gate.calls(),
            1,
            "a dropped request's load must finish rather than restart"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_request_stream_simple(fb: FacebookInit) -> Result<(), Error> {
        let repo_id = RepositoryId::new(0);
        let q = Arc::new(AsyncMethodRequestQueue::new_test_in_memory().unwrap());
        let ctx = CoreContext::test_mock(fb);

        let params = thrift::MegarepoSyncChangesetParams {
            cs_id: vec![],
            source_name: "name".to_string(),
            target: thrift::MegarepoTarget {
                repo_id: Some(repo_id.id() as i64),
                bookmark: "book".to_string(),
                ..Default::default()
            },
            target_location: vec![],
            ..Default::default()
        };
        q.enqueue(&ctx, Some(&repo_id), params).await?;

        let will_exit = Arc::new(AtomicBool::new(false));
        let s = AsyncMethodRequestWorker::request_stream_inner(
            ctx.clone(),
            ClaimedBy("name".to_string()),
            q,
            will_exit.clone(),
            Duration::from_millis(100),
            ABANDONED_REQUEST_THRESHOLD_SECS,
        );

        let s = mononoke::spawn_task(s.collect::<Vec<_>>());
        tokio::time::sleep(Duration::from_secs(1)).await;
        will_exit.store(true, Ordering::Relaxed);
        let res = s.await?;
        assert_eq!(res.len(), 1);
        assert_eq!(
            res[0].id.1,
            RequestType("megarepo_sync_changeset".to_string())
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_request_stream_clear_abandoned(fb: FacebookInit) -> Result<(), Error> {
        let repo_id = RepositoryId::new(0);
        let q = Arc::new(AsyncMethodRequestQueue::new_test_in_memory().unwrap());
        let ctx = CoreContext::test_mock(fb);

        let params = thrift::MegarepoSyncChangesetParams {
            cs_id: vec![],
            source_name: "name".to_string(),
            target: thrift::MegarepoTarget {
                repo_id: Some(repo_id.id() as i64),
                bookmark: "book".to_string(),
                ..Default::default()
            },
            target_location: vec![],
            ..Default::default()
        };
        q.enqueue(&ctx, Some(&repo_id), params).await?;

        // Grab it from the queue...
        let dequeued = q.dequeue(&ctx, &ClaimedBy("name".to_string())).await?;
        assert!(dequeued.is_some());

        // ... and check that the queue is empty now...
        let will_exit = Arc::new(AtomicBool::new(false));
        let s = AsyncMethodRequestWorker::request_stream_inner(
            ctx.clone(),
            ClaimedBy("name".to_string()),
            q.clone(),
            will_exit.clone(),
            Duration::from_millis(100),
            ABANDONED_REQUEST_THRESHOLD_SECS,
        );

        let s = mononoke::spawn_task(s.collect::<Vec<_>>());
        tokio::time::sleep(Duration::from_secs(1)).await;
        will_exit.store(true, Ordering::Relaxed);
        let res = s.await?;
        assert_eq!(res, vec![]);

        // ... now make it "abandoned", and make sure we reclaim it
        tokio::time::sleep(Duration::from_secs(1)).await;
        let will_exit = Arc::new(AtomicBool::new(false));
        let s = AsyncMethodRequestWorker::request_stream_inner(
            ctx.clone(),
            ClaimedBy("name".to_string()),
            q,
            will_exit.clone(),
            Duration::from_millis(100),
            1, // 1 second
        );

        let s = mononoke::spawn_task(s.collect::<Vec<_>>());
        tokio::time::sleep(Duration::from_secs(1)).await;
        will_exit.store(true, Ordering::Relaxed);
        let res = s.await?;
        assert_eq!(res.len(), 1);

        Ok(())
    }
}
