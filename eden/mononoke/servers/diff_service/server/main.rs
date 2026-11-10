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
use clap::Parser;
use diff_service_if_services::make_DiffService_server;
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

use crate::repo::Repo;

mod errors;
#[cfg(fbcode_build)]
mod facebook;
mod operations;
#[cfg(not(fbcode_build))]
mod oss;
mod repo;
mod service;
mod utils;

#[cfg(fbcode_build)]
use facebook as platform;
#[cfg(not(fbcode_build))]
use oss as platform;

const SERVICE_NAME: &str = "diff_service";
const SM_CLEANUP_TIMEOUT_SECS: u64 = 60;

#[derive(Parser)]
struct DiffServiceArgs {
    #[clap(flatten)]
    shutdown_timeout_args: ShutdownTimeoutArgs,
    #[clap(long, short = 'H', default_value = DEFAULT_THRIFT_HOST)]
    host: String,
    #[clap(long, short, default_value_t = 8999)]
    port: u16,
    #[clap(long)]
    bound_address_file: Option<String>,
    #[clap(flatten)]
    sharded_executor_args: ShardedExecutorArgs,
    /// Max memory to use for the server (enables memory-based admission control)
    #[clap(long)]
    max_memory: Option<usize>,
}

pub struct DiffServiceProcess {
    repos_mgr: Arc<MononokeReposManager<Repo>>,
}

impl DiffServiceProcess {
    fn new(repos_mgr: Arc<MononokeReposManager<Repo>>) -> Self {
        Self { repos_mgr }
    }
}

#[async_trait]
impl RepoShardedProcess for DiffServiceProcess {
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

        info!("Repo {} is ready", repo_name);

        Ok(Arc::new(DiffServiceProcessExecutor {
            repo_name: repo_name.to_string(),
            repos_mgr: self.repos_mgr.clone(),
        }))
    }
}

pub struct DiffServiceProcessExecutor {
    repo_name: String,
    repos_mgr: Arc<MononokeReposManager<Repo>>,
}

#[async_trait]
impl RepoShardedProcessExecutor for DiffServiceProcessExecutor {
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
            .and_then(|c| c.status.get(&ShardedService::DiffService).copied())
            .unwrap_or(false);

        if is_deep_sharded {
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
        .with_default_scuba_dataset("mononoke_diff_service")
        .with_app_extension(MonitoringAppExtension {})
        .with_app_extension(RepoFilterAppExtension {})
        .build::<DiffServiceArgs>()?;

    let args: DiffServiceArgs = app.args()?;
    let runtime = app.runtime();

    let port = args.port;
    let host = args.host;
    let (sm_shutdown_sender, sm_shutdown_receiver) = tokio::sync::oneshot::channel::<bool>();

    if let Some(max_memory) = args.max_memory {
        memory::set_max_memory(max_memory);
    }

    let service_name = args
        .sharded_executor_args
        .is_sharded()
        .then_some(ShardedService::DiffService);

    // Held until after wait_until_terminated: dropping the manager aborts the
    // config reconcile loop, which is the only thing that rebuilds repos on
    // config change. Without ShardManager (local runs) nothing else holds it.
    let repos_mgr = Arc::new(runtime.block_on(app.open_managed_repos(service_name))?);
    let mononoke = Arc::new(repos_mgr.make_mononoke_api()?);

    let platform_state = runtime.block_on(platform::ServiceState::new(&app))?;

    let diff_service = runtime.block_on(service::DiffServiceImpl::new(
        &app,
        mononoke.clone(),
        platform_state,
    ))?;

    let service = move |proto| make_DiffService_server(proto, diff_service.clone());

    let thrift_factory = runtime.block_on(platform::ServerFactory::new(fb, SERVICE_NAME))?;

    let builder = ThriftServerBuilder::new(fb)
        .service_name(SERVICE_NAME)
        .host(&host)
        .port(port)
        .with_handle(runtime.clone())
        .factory(thrift_factory.executor(), move || service);
    let mut server = platform::configure_server(builder, &app).build()?;

    if let Some(bound_addr_path) = &args.bound_address_file {
        server.write_bound_address(bound_addr_path)?;
    }

    if let Some(executor) = args.sharded_executor_args.build_executor(
        fb,
        runtime.clone(),
        || Arc::new(DiffServiceProcess::new(repos_mgr.clone())),
        false,
        SM_CLEANUP_TIMEOUT_SECS,
    )? {
        runtime.spawn(executor.block_and_execute(sm_shutdown_receiver));
    }

    app.start_stats_aggregation()?;

    let handle = server.into_shutdown_handle();
    app.wait_until_terminated(
        move || {
            let _ = sm_shutdown_sender.send(true);
        },
        args.shutdown_timeout_args.shutdown_grace_period,
        async {
            // First: shut down the Thrift factory. This stops accepting new
            // requests and waits for currently running Rust futures to complete.
            // This must happen BEFORE stopping the server handle below,
            // otherwise the C++ worker drain timeout (20s) fires while Rust
            // futures are still running, causing XLOG(FATAL) → abort.
            thrift_factory.shutdown().await;
            // Second: stop the server. By now all Rust futures have
            // completed, so workers should drain quickly.
            let _ = task::spawn_blocking(move || {
                handle.stop();
            })
            .await;
        },
        args.shutdown_timeout_args.shutdown_timeout,
        None,
    )?;
    drop(repos_mgr);
    info!("Exiting...");
    Ok(())
}
