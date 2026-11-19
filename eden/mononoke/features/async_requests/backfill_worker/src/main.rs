/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use anyhow::Result;
use async_requests::AsyncMethodRequestQueue;
use async_requests::QueueRepoFilter;
use async_requests::QueueRequestTypeFilter;
use async_requests_client::open_blobstore;
use async_requests_client::open_sql_connection;
use async_requests_types::BACKFILL_REQUEST_TYPES;
use clap::Parser;
use cmdlib_logging::ScribeLoggingArgs;
use context::SessionContainer;
use environment::BookmarkCacheDerivedData;
use environment::BookmarkCacheKind;
use environment::BookmarkCacheOptions;
use executor_lib::RepoShardedProcessExecutor;
use fbinit::FacebookInit;
use megarepo_api::MegarepoApi;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::MononokeService;
use mononoke_app::args::HooksAppExtension;
use mononoke_app::args::ShutdownTimeoutArgs;
use mononoke_app::args::WarmBookmarksCacheExtension;
use mononoke_app::monitoring::AliveService;
use mononoke_app::monitoring::MonitoringAppExtension;
use requests_table::RequestType;
use tracing::info;
use worker_lib::worker::AsyncMethodRequestWorker;

const SERVICE_NAME: &str = "backfill_worker";

/// Processes only derived data backfill async requests.
///
/// Unlike the general async_requests worker, this binary:
/// - Does not use ShardManager / repo sharding
/// - Only processes backfill request types (derive_*)
/// - Loads repos on-demand, with a configurable set pre-loaded at startup
#[derive(Parser)]
struct BackfillWorkerArgs {
    #[clap(flatten)]
    shutdown_timeout_args: ShutdownTimeoutArgs,
    #[clap(flatten)]
    scribe_logging_args: ScribeLoggingArgs,
    /// The number of requests to process before exiting
    #[clap(long)]
    request_limit: Option<usize>,
    /// The number of concurrent executors for processing requests
    #[clap(long, short = 'j', default_value = "1")]
    jobs: usize,
    /// Comma-separated list of repo names to pre-load at startup
    /// and keep in memory (e.g. fbsource,configerator)
    #[clap(long, value_delimiter = ',')]
    preload_repos: Vec<String>,
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<()> {
    let app = MononokeAppBuilder::new(fb)
        .with_default_service(MononokeService::BACKFILL_WORKER)
        .with_bookmarks_cache(BookmarkCacheOptions {
            cache_kind: BookmarkCacheKind::Local,
            derived_data: BookmarkCacheDerivedData::NoDerivation,
        })
        .with_app_extension(WarmBookmarksCacheExtension {})
        .with_app_extension(HooksAppExtension {})
        .with_app_extension(MonitoringAppExtension {})
        .build::<BackfillWorkerArgs>()?;

    let args: Arc<BackfillWorkerArgs> = Arc::new(app.args()?);
    let env = app.environment();
    let runtime = app.runtime().clone();
    let session = SessionContainer::new_with_defaults(env.fb);
    let ctx = Arc::new(session.new_context(env.scuba_sample_builder.clone()));

    // Only load the repos specified via --preload-repos. Other repos will be
    // loaded on-demand when a backfill request arrives for them.
    let repos_mgr =
        runtime.block_on(app.open_named_managed_repos(args.preload_repos.clone(), None))?;
    let repos_mgr = Arc::new(repos_mgr);

    let mononoke = Arc::new(repos_mgr.make_mononoke_api()?);
    let megarepo = Arc::new(MegarepoApi::new(&app, mononoke.clone())?);

    let sql_connection = Arc::new(runtime.block_on(open_sql_connection(fb, &app))?);
    let blobstore = runtime.block_on(open_blobstore(fb, &app))?;
    let will_exit = Arc::new(AtomicBool::new(false));

    app.start_monitoring(app.runtime(), SERVICE_NAME, AliveService)?;
    app.start_stats_aggregation()?;

    // Build the queue with backfill-only type filter and no repo restriction.
    let backfill_types: Vec<RequestType> = BACKFILL_REQUEST_TYPES
        .iter()
        .map(|s| RequestType(s.to_string()))
        .collect();
    let queue = Arc::new(AsyncMethodRequestQueue::new_with_request_type_filter(
        sql_connection,
        blobstore,
        QueueRepoFilter::Except(vec![]),
        QueueRequestTypeFilter::Only(backfill_types),
    ));

    let executor = runtime.block_on(AsyncMethodRequestWorker::new(
        args.request_limit,
        args.jobs,
        ctx.clone(),
        queue,
        repos_mgr,
        mononoke,
        megarepo,
        will_exit.clone(),
    ))?;

    info!(
        "Starting backfill worker with {} executors, pre-loaded repos: {:?}",
        args.jobs, args.preload_repos
    );
    runtime.spawn(async move { executor.execute().await });

    app.wait_until_terminated(
        move || will_exit.store(true, Ordering::Relaxed),
        args.shutdown_timeout_args.shutdown_grace_period,
        async {
            info!("Shutdown");
        },
        args.shutdown_timeout_args.shutdown_timeout,
        None,
    )?;

    Ok(())
}
