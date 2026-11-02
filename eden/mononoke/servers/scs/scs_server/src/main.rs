/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![type_length_limit = "2097152"]

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Error;
use async_trait::async_trait;
use clap::Parser;
use clap::ValueEnum;
use clientinfo::ClientEntryPoint;
use cmdlib_logging::ScribeLoggingArgs;
use environment::BookmarkCacheDerivedData;
use environment::BookmarkCacheKind;
use environment::BookmarkCacheOptions;
use executor_lib::RepoShardedProcess;
use executor_lib::RepoShardedProcessExecutor;
use executor_lib::args::ShardedExecutorArgs;
use fbinit::FacebookInit;
use megarepo_api::MegarepoApi;
use metaconfig_types::ShardedService;
use mononoke_api::CoreContext;
use mononoke_api::repo::Repo;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::MononokeReposManager;
use mononoke_app::args::HooksAppExtension;
use mononoke_app::args::RepoFilterAppExtension;
use mononoke_app::args::ShutdownTimeoutArgs;
use mononoke_app::args::WarmBookmarksCacheExtension;
use mononoke_thrift_server::DEFAULT_THRIFT_HOST;
use mononoke_thrift_server::LoadMonitorConfig;
use mononoke_thrift_server::ThriftServerBuilder;
use panichandler::Fate;
use scs_methods::source_control_impl::SourceControlServiceImpl;
use sharding_ext::RepoShard;
use source_control_services::make_SourceControlService_server;
use tokio::task;
use tracing::info;

#[cfg(fbcode_build)]
mod facebook;
mod monitoring;

#[cfg(fbcode_build)]
use facebook as platform;

const SERVICE_NAME: &str = "mononoke_scs_server";
const SM_CLEANUP_TIMEOUT_SECS: u64 = 60;

/// Mononoke Source Control Service Server
#[derive(Parser)]
struct ScsServerArgs {
    #[clap(flatten)]
    shutdown_timeout_args: ShutdownTimeoutArgs,
    #[clap(flatten)]
    scribe_logging_args: ScribeLoggingArgs,
    /// Enable trace logging of dependencies
    #[clap(long, default_value = "false")]
    trace: bool,
    /// Thrift host
    #[clap(long, short = 'H', default_value = DEFAULT_THRIFT_HOST)]
    host: String,
    /// Thrift port
    #[clap(long, short = 'p', default_value_t = 8367)]
    port: u16,
    /// Path for file in which to write the bound tcp address in rust std::net::SocketAddr format
    #[clap(long)]
    bound_address_file: Option<String>,
    #[clap(flatten)]
    sharded_executor_args: ShardedExecutorArgs,
    /// Max memory to use for the thrift server
    #[clap(long)]
    max_memory: Option<usize>,
    /// Deprecated: ThriftFactory is now always used. Kept for backwards compatibility.
    #[clap(long, value_enum, default_value_t = DeprecatedThriftServerMode::ThriftFactory, hide = true)]
    thift_server_mode: DeprecatedThriftServerMode,
    /// Thrift queue size
    #[clap(long, default_value = "0")]
    thrift_queue_size: usize,
    /// Thrift queue timeout in milliseconds
    #[clap(long, default_value = "500")]
    thrift_queue_timeout: u64,
    /// Number of Thrift workers
    #[clap(long, default_value = "1000")]
    thrift_workers_num: usize,
    /// Deprecated: FactoryGroup mode has been removed. Kept for backwards compatibility.
    #[clap(long, default_value = "1000", hide = true)]
    thrift_workers_num_fast: usize,
    /// Deprecated: FactoryGroup mode has been removed. Kept for backwards compatibility.
    #[clap(long, default_value = "5", hide = true)]
    thrift_workers_num_slow: usize,
    /// Some long-running requests are processed asynchronously by default. This flag disables that behavior; requests will fail.
    #[clap(long, default_value = "false")]
    disable_async_requests: bool,
    /// Sets the threshold for watchdog logging of top-level SCS methods. As a rule of thumb this should the same or lower than thrift_queue_timeout.
    #[clap(long, default_value = "500")]
    watchdog_method_max_poll: u64,
    /// Load all repos in the tier at startup, even if they are configured to be deep sharded.
    /// WARNING: Using this in any kind of production service will lead to the loading of potentially tens of thousands of repos,
    /// considerably slowing down the startup. This flag primarily exists for the purpose of local testing with --filter-repos argument
    #[clap(long, requires = "filter_repos")]
    load_all_repos_in_tier: bool,
}

/// Deprecated: ThriftFactory is now always used. This enum is kept only so that
/// existing CLI invocations (e.g. from TW specs) that pass `--thift-server-mode`
/// continue to parse without error.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum DeprecatedThriftServerMode {
    Default,
    ThriftFactory,
    FactoryGroup,
}

/// Struct representing the Source Control Service process when sharding by
/// repo.
pub struct ScsServerProcess {
    repos_mgr: Arc<MononokeReposManager<Repo>>,
}

impl ScsServerProcess {
    fn new(repos_mgr: Arc<MononokeReposManager<Repo>>) -> Self {
        Self { repos_mgr }
    }
}

#[async_trait]
impl RepoShardedProcess for ScsServerProcess {
    async fn setup(&self, repo: &RepoShard) -> anyhow::Result<Arc<dyn RepoShardedProcessExecutor>> {
        let repo_name = repo.repo_name.as_str();
        info!("Setting up repo {} in SCS service", repo_name);
        // Check if the input repo is already initialized. This can happen if the repo is a
        // shallow-sharded repo, in which case it would already be initialized during service startup.
        if self.repos_mgr.repos().get_by_name(repo_name).is_none() {
            // The input repo is a deep-sharded repo, so it needs to be added now.
            let added = if lazy_repo_build_enabled() {
                self.repos_mgr
                    .add_repo_for_service(repo_name, Some(ShardedService::SourceControlService))
                    .await
            } else {
                self.repos_mgr.add_repo(repo_name).await.map(|_| ())
            };
            added.with_context(|| {
                format!("Failure in setting up repo {repo_name} in SCS service")
            })?;
            info!("Completed repo {} setup in SCS service", repo_name);
        } else {
            info!("Repo {} is already setup in SCS service", repo_name);
        }
        Ok(Arc::new(ScsServerProcessExecutor {
            repo_name: repo_name.to_string(),
            repos_mgr: self.repos_mgr.clone(),
        }))
    }
}

/// Whether `setup` may register a repo unbuilt, for its first request to build.
fn lazy_repo_build_enabled() -> bool {
    justknobs::eval("scm/mononoke:enable_scs_lazy_repo_build", None, None)
}

/// Struct representing the execution of the source control service for a
/// particular repo when sharding by repo.
pub struct ScsServerProcessExecutor {
    repo_name: String,
    repos_mgr: Arc<MononokeReposManager<Repo>>,
}

#[async_trait]
impl RepoShardedProcessExecutor for ScsServerProcessExecutor {
    async fn execute(&self) -> anyhow::Result<()> {
        info!("Serving repo {} in SCS service", &self.repo_name,);
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        let config = self
            .repos_mgr
            .repo_config(&self.repo_name)
            .with_context(|| {
                format!(
                    "Failure in stopping repo {}. The config for repo doesn't exist",
                    self.repo_name
                )
            })?;
        // Check if the current repo is a deep-sharded or shallow-sharded repo. If the
        // repo is deep-sharded, then remove it since SM wants some other host to serve it.
        // If repo is shallow-sharded, then keep it since regardless of SM sharding, shallow
        // sharded repos need to be present on each host.
        let is_deep_sharded = config
            .deep_sharding_config
            .and_then(|c| c.status.get(&ShardedService::SourceControlService).copied())
            .unwrap_or(false);
        if is_deep_sharded {
            self.repos_mgr.remove_repo(&self.repo_name);
            info!("No longer serving repo {} in SCS service.", &self.repo_name,);
        } else {
            info!(
                "Continuing serving repo {} in SCS service because it's shallow-sharded.",
                &self.repo_name,
            );
        }
        Ok(())
    }
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<(), Error> {
    panichandler::set_panichandler(Fate::Abort);

    let app = MononokeAppBuilder::new(fb)
        .with_entry_point(ClientEntryPoint::ScsServer)
        .with_bookmarks_cache(BookmarkCacheOptions {
            cache_kind: BookmarkCacheKind::Local,
            derived_data: BookmarkCacheDerivedData::AllKinds,
        })
        .with_app_extension(WarmBookmarksCacheExtension {})
        .with_app_extension(HooksAppExtension {})
        .with_app_extension(RepoFilterAppExtension {})
        .build::<ScsServerArgs>()?;

    let args: ScsServerArgs = app.args()?;
    let runtime = app.runtime();
    let env = app.environment();

    let scuba_builder = env.scuba_sample_builder.clone();
    // Service name is used for shallow or deep sharding.
    let service_name = if args.load_all_repos_in_tier {
        None
    } else {
        Some(ShardedService::SourceControlService)
    };
    // Held until after wait_until_terminated: dropping the manager aborts the
    // config reconcile loop, which is the only thing that rebuilds repos on
    // config change. Without ShardManager (local runs) nothing else holds it.
    let repos_mgr = Arc::new(runtime.block_on(app.open_managed_repos(service_name))?);
    let mononoke = Arc::new(repos_mgr.make_mononoke_api()?);
    let megarepo_api = Arc::new(MegarepoApi::new(&app, mononoke.clone())?);

    let (sm_shutdown_sender, sm_shutdown_receiver) = tokio::sync::oneshot::channel::<bool>();

    if let Some(max_memory) = args.max_memory {
        memory::set_max_memory(max_memory);
    }

    let async_requests_queue_client = if args.disable_async_requests {
        None
    } else {
        let queue_client = runtime.block_on(async_requests_client::build(fb, &app, None))?;
        Some(Arc::new(queue_client))
    };

    let git_source_of_truth_config =
        runtime.block_on(platform::create_git_source_of_truth_config(fb))?;

    let source_control_server = runtime.block_on(SourceControlServiceImpl::new(
        fb,
        &app,
        mononoke.clone(),
        megarepo_api,
        scuba_builder,
        args.scribe_logging_args.get_scribe(fb)?,
        app.configs(),
        async_requests_queue_client,
        git_source_of_truth_config,
        args.watchdog_method_max_poll,
    ))?;

    let monitoring_forever = {
        let monitoring_ctx = CoreContext::new(fb);
        monitoring::monitoring_stats_submitter(monitoring_ctx, mononoke)
    };
    runtime.spawn(monitoring_forever);

    let service = {
        move |proto| make_SourceControlService_server(proto, source_control_server.thrift_server())
    };

    let factory = runtime.block_on(platform::ServerFactory::new(
        fb,
        args.thrift_workers_num,
        args.thrift_queue_size,
        Duration::from_millis(args.thrift_queue_timeout),
    ));

    let builder = ThriftServerBuilder::new(fb)
        .service_name(SERVICE_NAME)
        .host(&args.host)
        .port(args.port)
        .with_handle(runtime.clone())
        .with_tls()
        .with_cancel_on_client_disconnect()
        .with_profile_module()
        .with_load_monitor(LoadMonitorConfig {
            memory_stats_ttl: Duration::from_millis(50),
            cgroup_stats_ttl: Duration::from_millis(50),
            counters_update_interval: Duration::from_millis(100),
            cgroup_root_path: "/cgroup2".to_string(),
        });
    let mut server = platform::configure_server(builder, &app)
        .factory(factory.executor(), move || service)
        .build()?;

    info!("Listening on {}", server.bound_address()?);

    // Write out the bound address if requested, this is helpful in tests when using automatic binding with :0
    if let Some(bound_addr_path) = &args.bound_address_file {
        server.write_bound_address(bound_addr_path)?;
    }

    if let Some(executor) = args.sharded_executor_args.build_executor(
        fb,
        runtime.clone(),
        || Arc::new(ScsServerProcess::new(repos_mgr.clone())),
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
            let _ = task::spawn_blocking(move || {
                // Calling `stop` blocks until the service has completed all requests.
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
