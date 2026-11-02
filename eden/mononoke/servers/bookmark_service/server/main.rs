/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use bookmark_service_if_services::*;
use clap::Parser;
use environment::BookmarkCacheDerivedData;
use environment::BookmarkCacheKind;
use environment::BookmarkCacheOptions;
use executor_lib::RepoShardedProcess;
use executor_lib::RepoShardedProcessExecutor;
use executor_lib::args::ShardedExecutorArgs;
use fbinit::FacebookInit;
use metaconfig_types::ShardedService;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::MononokeReposManager;
use mononoke_app::args::RepoFilterAppExtension;
use mononoke_app::args::ShutdownTimeoutArgs;
use mononoke_app::monitoring::MonitoringAppExtension;
use mononoke_thrift_server::DEFAULT_THRIFT_HOST;
use mononoke_thrift_server::ThriftServerBuilder;
use sharding_ext::RepoShard;
use tokio::task;
use tracing::info;

use crate::service::BookmarkServiceRepo;

mod errors;
#[cfg(fbcode_build)]
mod facebook;
#[cfg(not(fbcode_build))]
mod oss;
mod service;

#[cfg(fbcode_build)]
use facebook as platform;
#[cfg(not(fbcode_build))]
use oss as platform;

const SERVICE_NAME: &str = "bookmark_service";
const SM_CLEANUP_TIMEOUT_SECS: u64 = 60;

/// BookmarkService Thrift Service
#[derive(Parser)]
struct MononokeBookmarkServiceArgs {
    #[clap(flatten)]
    shutdown_timeout_args: ShutdownTimeoutArgs,
    /// Thrift host
    #[clap(long, short = 'H', default_value = DEFAULT_THRIFT_HOST)]
    host: String,
    /// Thrift port
    #[clap(long, short, default_value_t = 8998)]
    port: u16,
    /// Path for file in which to write the bound tcp address in rust std::net::SocketAddr format
    #[clap(long)]
    bound_address_file: Option<String>,
    #[clap(flatten)]
    sharded_executor_args: ShardedExecutorArgs,
}

/// Struct representing the bookmark service process when sharding by
/// repo.
pub struct BookmarkServiceProcess {
    repos_mgr: Arc<MononokeReposManager<BookmarkServiceRepo>>,
}

impl BookmarkServiceProcess {
    fn new(repos_mgr: Arc<MononokeReposManager<BookmarkServiceRepo>>) -> Self {
        Self { repos_mgr }
    }
}

#[async_trait]
impl RepoShardedProcess for BookmarkServiceProcess {
    async fn setup(&self, repo: &RepoShard) -> Result<Arc<dyn RepoShardedProcessExecutor>> {
        let repo_name = repo.repo_name.as_str();
        info!("Setting up repo {}", repo_name);

        if self.repos_mgr.repos().get_by_name(repo_name).is_none() {
            self.repos_mgr
                .add_repo(repo_name)
                .await
                .with_context(|| format!("Failure in setting up repo {repo_name}"))?;
            info!("Completed repo {} setup", repo_name);
        } else {
            info!("Repo {} is already setup", repo_name);
        }
        Ok(Arc::new(BookmarkServiceProcessExecutor {
            repo_name: repo_name.to_string(),
            repos_mgr: self.repos_mgr.clone(),
        }))
    }
}

/// Struct representing the execution of the bookmark service for a
/// particular repo when sharding by repo.
pub struct BookmarkServiceProcessExecutor {
    repo_name: String,
    repos_mgr: Arc<MononokeReposManager<BookmarkServiceRepo>>,
}

#[async_trait]
impl RepoShardedProcessExecutor for BookmarkServiceProcessExecutor {
    async fn execute(&self) -> Result<()> {
        info!("Serving repo {}", &self.repo_name);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        let config = self
            .repos_mgr
            .repo_config(&self.repo_name)
            .with_context(|| {
                format!(
                    "Repo config not found for {} while stopping repo shard",
                    self.repo_name
                )
            })?;

        let is_deep_sharded = config
            .deep_sharding_config
            .and_then(|c| c.status.get(&ShardedService::BookmarkService).copied())
            .unwrap_or(false);
        if is_deep_sharded {
            self.repos_mgr.remove_repo(&self.repo_name);
            info!("No longer serving repo {}", &self.repo_name,);
        } else {
            info!(
                "Continuing serving repo {} because it's shallow-sharded.",
                &self.repo_name,
            );
        }

        Ok(())
    }
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<()> {
    let app = MononokeAppBuilder::new(fb)
        .with_bookmarks_cache(BookmarkCacheOptions {
            cache_kind: BookmarkCacheKind::Local,
            // The bookmark service must configure ALL warmers (Hg, Git, and any others)
            // because it exposes scoped bookmark queries (HgOnly, GitOnly, AllKinds).
            // Clients can request bookmarks for specific scopes, so all warmers must be ready.
            derived_data: BookmarkCacheDerivedData::AllKinds,
        })
        .with_app_extension(MonitoringAppExtension {})
        .with_app_extension(RepoFilterAppExtension {})
        .build::<MononokeBookmarkServiceArgs>()?;
    let args: MononokeBookmarkServiceArgs = app.args()?;

    let runtime = app.runtime();
    let exec = runtime.clone();

    let port = args.port;
    let host = args.host;
    let (sm_shutdown_sender, sm_shutdown_receiver) = tokio::sync::oneshot::channel::<bool>();

    let service_name = args
        .sharded_executor_args
        .sharded_service_name
        .as_ref()
        .map(|_| ShardedService::BookmarkService);
    let repos_mgr = Arc::new(runtime.block_on(app.open_managed_repos(service_name))?);

    let bookmark_service =
        runtime.block_on(service::BookmarkServiceImpl::new(&app, repos_mgr.clone()))?;

    let service = move |proto| make_BookmarkService_server(proto, bookmark_service.clone());

    let builder = ThriftServerBuilder::new(fb)
        .service_name(SERVICE_NAME)
        .host(&host)
        .port(port)
        .with_handle(exec.clone())
        .factory(exec, move || service);
    let mut server = platform::configure_server(builder, &app).build()?;

    // Write out the bound address if requested, this is helpful in tests when using automatic binding with :0
    if let Some(bound_addr_path) = &args.bound_address_file {
        server.write_bound_address(bound_addr_path)?;
    }

    if let Some(executor) = args.sharded_executor_args.build_executor(
        fb,
        runtime.clone(),
        || Arc::new(BookmarkServiceProcess::new(repos_mgr)),
        false, // disable shard (repo) level healing
        SM_CLEANUP_TIMEOUT_SECS,
    )? {
        // The Sharded Process Executor needs to branch off and execute
        // on its own dedicated task spawned off the common tokio runtime.
        runtime.spawn(executor.block_and_execute(sm_shutdown_receiver));
    }

    // Monitoring is provided by the `Fb303Module`, but we must still start
    // stats aggregation.
    app.start_stats_aggregation()?;

    let handle = server.into_shutdown_handle();
    app.wait_until_terminated(
        move || {
            let _ = sm_shutdown_sender.send(true);
        },
        args.shutdown_timeout_args.shutdown_grace_period,
        async {
            // Note that async blocks are lazy, so this isn't called until first poll
            let _ = task::spawn_blocking(move || {
                // Calling `stop` blocks until the service has completed all requests.
                handle.stop();
            })
            .await;
        },
        args.shutdown_timeout_args.shutdown_timeout,
        None,
    )?;
    info!("Exiting...");
    Ok(())
}
