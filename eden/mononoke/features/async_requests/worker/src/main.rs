/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Result;
use async_requests::AsyncMethodRequestQueue;
use async_requests::QueueRepoFilter;
use async_requests::QueueRequestTypeFilter;
use async_requests_client::open_blobstore;
use async_requests_client::open_sql_connection;
use async_requests_types::BACKFILL_REQUEST_TYPES;
use async_trait::async_trait;
use blobstore::Blobstore;
use clap::Parser;
use cmdlib_logging::ScribeLoggingArgs;
use context::CoreContext;
use context::SessionContainer;
use environment::BookmarkCacheDerivedData;
use environment::BookmarkCacheKind;
use environment::BookmarkCacheOptions;
use executor_lib::RepoShardedProcess;
use executor_lib::RepoShardedProcessExecutor;
use executor_lib::args::ShardedExecutorArgs;
use fbinit::FacebookInit;
use megarepo_api::MegarepoApi;
use metaconfig_types::ShardedService;
use mononoke_api::Mononoke;
use mononoke_api::Repo;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::MononokeReposManager;
use mononoke_app::args::HooksAppExtension;
use mononoke_app::args::RepoFilterAppExtension;
use mononoke_app::args::ShutdownTimeoutArgs;
use mononoke_app::args::WarmBookmarksCacheExtension;
use mononoke_app::monitoring::AliveService;
use mononoke_app::monitoring::MonitoringAppExtension;
use mononoke_types::RepositoryId;
use requests_table::LongRunningRequestsQueue;
use requests_table::SqlLongRunningRequestsQueue;
use sharding_ext::RepoShard;
use tracing::info;
use worker_lib::worker::AsyncMethodRequestWorker;

const SERVICE_NAME: &str = "async_requests_worker";

/// Build a QueueRequestTypeFilter that excludes backfill request types.
/// Backfill requests are handled by the dedicated backfill_worker.
fn backfill_exclude_filter() -> QueueRequestTypeFilter {
    QueueRequestTypeFilter::Except(
        BACKFILL_REQUEST_TYPES
            .iter()
            .map(|s| requests_table::RequestType(s.to_string()))
            .collect(),
    )
}

const SM_CLEANUP_TIMEOUT_SECS: u64 = 60;

/// Processes the megarepo async requests
#[derive(Parser)]
struct AsyncRequestsWorkerArgs {
    #[clap(flatten)]
    pub sharded_executor_args: ShardedExecutorArgs,

    #[clap(flatten)]
    shutdown_timeout_args: ShutdownTimeoutArgs,
    #[clap(flatten)]
    scribe_logging_args: ScribeLoggingArgs,
    /// The number of requests to process before exiting
    #[clap(long)]
    request_limit: Option<usize>,
    /// The number of requests / jobs to be processed concurrently
    #[clap(long, short = 'j', default_value = "1")]
    jobs: usize,
    /// Deprecated: the unsharded worker now always processes all repos.
    /// Kept for backward compatibility with existing callers.
    #[clap(long, hide = true)]
    process_global_queue: bool,
}

pub struct WorkerProcess {
    ctx: Arc<CoreContext>,
    args: Arc<AsyncRequestsWorkerArgs>,
    repos_mgr: Arc<MononokeReposManager<Repo>>,
    mononoke: Arc<Mononoke<Repo>>,
    megarepo: Arc<MegarepoApi<Repo>>,
    sql_connection: Arc<SqlLongRunningRequestsQueue>,
    blobstore: Arc<dyn blobstore::Blobstore>,
    will_exit: Arc<AtomicBool>,
}

impl WorkerProcess {
    pub(crate) fn new(
        ctx: Arc<CoreContext>,
        args: Arc<AsyncRequestsWorkerArgs>,
        repos_mgr: Arc<MononokeReposManager<Repo>>,
        mononoke: Arc<Mononoke<Repo>>,
        megarepo: Arc<MegarepoApi<Repo>>,
        sql_connection: Arc<SqlLongRunningRequestsQueue>,
        blobstore: Arc<dyn blobstore::Blobstore>,
        will_exit: Arc<AtomicBool>,
    ) -> Self {
        Self {
            ctx,
            args,
            repos_mgr,
            mononoke,
            megarepo,
            sql_connection,
            blobstore,
            will_exit,
        }
    }
}

#[async_trait]
impl RepoShardedProcess for WorkerProcess {
    async fn setup(&self, repo: &RepoShard) -> Result<Arc<dyn RepoShardedProcessExecutor>> {
        let repo_name = repo.repo_name.as_str();
        info!("Setting up repo {}", repo_name);

        let repo = self
            .repos_mgr
            .add_repo(repo_name)
            .await
            .with_context(|| format!("Failure in setting up repo {repo_name}"))?;
        let repos = vec![repo.repo_identity.id()];
        info!("Completed setup for repo {} ({:?})", repo_name, repos);

        let queue = Arc::new(AsyncMethodRequestQueue::new_with_request_type_filter(
            self.sql_connection.clone(),
            self.blobstore.clone(),
            QueueRepoFilter::Only(repos),
            backfill_exclude_filter(),
        ));

        let executor = AsyncMethodRequestWorker::new(
            self.args.request_limit,
            self.args.jobs,
            self.ctx.clone(),
            queue,
            self.repos_mgr.clone(),
            self.mononoke.clone(),
            self.megarepo.clone(),
            self.will_exit.clone(),
        )
        .await?;
        Ok(Arc::new(executor))
    }
}

/// Collect the repo IDs for all repos known to the config. These are
/// the repos that ShardManager will assign to dedicated per-repo
/// executors (on this or other worker instances), so the catch-all
/// executor should exclude them.
fn configured_repo_ids(repos_mgr: &MononokeReposManager<Repo>) -> Vec<RepositoryId> {
    repos_mgr
        .configs()
        .repo_configs()
        .iter()
        .map(|(_, config)| config.repoid)
        .collect()
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<()> {
    let app = MononokeAppBuilder::new(fb)
        .with_bookmarks_cache(BookmarkCacheOptions {
            cache_kind: BookmarkCacheKind::Local,
            derived_data: BookmarkCacheDerivedData::NoDerivation,
        })
        .with_app_extension(WarmBookmarksCacheExtension {})
        .with_app_extension(HooksAppExtension {})
        .with_app_extension(MonitoringAppExtension {})
        .with_app_extension(RepoFilterAppExtension {})
        .build::<AsyncRequestsWorkerArgs>()?;

    let args: Arc<AsyncRequestsWorkerArgs> = Arc::new(app.args()?);
    let env = app.environment();
    let runtime = app.runtime().clone();
    let session = SessionContainer::new_with_defaults(env.fb);
    let ctx = Arc::new(session.new_context(env.scuba_sample_builder.clone()));

    let sharded_args = args.sharded_executor_args.clone();
    let repos_mgr = if sharded_args.is_sharded() {
        let service_name = Some(ShardedService::AsyncRequestsWorker);
        runtime.block_on(app.open_managed_repos(service_name))
    } else {
        runtime.block_on(app.open_managed_repos(None))
    }?;
    let repos_mgr = Arc::new(repos_mgr);
    let mononoke = Arc::new(repos_mgr.make_mononoke_api()?);
    let megarepo = Arc::new(MegarepoApi::new(&app, mononoke.clone())?);

    let sql_connection = Arc::new(runtime.block_on(open_sql_connection(fb, &app))?);
    let blobstore = runtime.block_on(open_blobstore(fb, &app))?;
    let will_exit = Arc::new(AtomicBool::new(false));
    let (sm_shutdown_sender, sm_shutdown_receiver) = tokio::sync::oneshot::channel::<bool>();

    app.start_monitoring(app.runtime(), SERVICE_NAME, AliveService)?;
    app.start_stats_aggregation()?;

    if let Some(executor) = args.sharded_executor_args.clone().build_executor(
        app.fb,
        runtime.clone(),
        || {
            Arc::new(WorkerProcess::new(
                ctx.clone(),
                args.clone(),
                repos_mgr.clone(),
                mononoke.clone(),
                megarepo.clone(),
                sql_connection.clone(),
                blobstore.clone(),
                will_exit.clone(),
            ))
        },
        true, // enable shard (repo) level healing
        SM_CLEANUP_TIMEOUT_SECS,
    )? {
        info!("Starting sharded process");
        // The Sharded Process Executor needs to branch off and execute
        // on its own dedicated task spawned off the common tokio runtime.
        runtime.spawn(executor.block_and_execute(sm_shutdown_receiver));

        // Start a catch-all executor for repos not in the config.
        // Configured repos are handled by ShardManager per-repo executors
        // (on this or other worker instances).
        let excluded_repos = configured_repo_ids(&repos_mgr);
        info!(
            "Starting catch-all executor (excluding {} configured repos)",
            excluded_repos.len()
        );
        run_worker_queue(
            &runtime,
            ctx.clone(),
            args.clone(),
            repos_mgr.clone(),
            mononoke.clone(),
            megarepo.clone(),
            sql_connection.clone(),
            blobstore.clone(),
            QueueRepoFilter::Except(excluded_repos),
            will_exit.clone(),
        )?;

        app.wait_until_terminated(
            move || {
                let _ = sm_shutdown_sender.send(true);
                will_exit.store(true, Ordering::Relaxed)
            },
            args.shutdown_timeout_args.shutdown_grace_period,
            async {
                info!("Shutdown");
            },
            args.shutdown_timeout_args.shutdown_timeout,
            None,
        )?;
    } else {
        info!("Starting unsharded executor for all repos");
        run_worker_queue(
            &runtime,
            ctx.clone(),
            args.clone(),
            repos_mgr.clone(),
            mononoke.clone(),
            megarepo.clone(),
            sql_connection.clone(),
            blobstore.clone(),
            QueueRepoFilter::Except(vec![]),
            will_exit.clone(),
        )?;

        app.wait_until_terminated(
            move || {
                let _ = sm_shutdown_sender.send(true);
                will_exit.store(true, Ordering::Relaxed)
            },
            args.shutdown_timeout_args.shutdown_grace_period,
            async {
                info!("Shutdown");
            },
            args.shutdown_timeout_args.shutdown_timeout,
            None,
        )?;
    }

    Ok(())
}

fn run_worker_queue(
    runtime: &tokio::runtime::Handle,
    ctx: Arc<CoreContext>,
    args: Arc<AsyncRequestsWorkerArgs>,
    repos_mgr: Arc<MononokeReposManager<Repo>>,
    mononoke: Arc<Mononoke<Repo>>,
    megarepo: Arc<MegarepoApi<Repo>>,
    sql_connection: Arc<dyn LongRunningRequestsQueue>,
    blobstore: Arc<dyn Blobstore>,
    repo_filter: QueueRepoFilter,
    will_exit: Arc<AtomicBool>,
) -> Result<()> {
    let executor = {
        let queue = Arc::new(AsyncMethodRequestQueue::new_with_request_type_filter(
            sql_connection,
            blobstore,
            repo_filter,
            backfill_exclude_filter(),
        ));

        runtime.block_on(AsyncMethodRequestWorker::new(
            args.request_limit,
            args.jobs,
            ctx.clone(),
            queue.clone(),
            repos_mgr,
            mononoke.clone(),
            megarepo.clone(),
            will_exit.clone(),
        ))?
    };
    runtime.spawn(async move { executor.execute().await });
    Ok(())
}
