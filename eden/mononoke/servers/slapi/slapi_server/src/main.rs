/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use cache_warmup::CacheWarmupKind;
use cache_warmup::cache_warmup;
use clap::Parser;
use clientinfo::ClientEntryPoint;
use cloned::cloned;
use cmdlib_logging::ScribeLoggingArgs;
use environment::BookmarkCacheDerivedData;
use environment::BookmarkCacheKind;
use environment::BookmarkCacheOptions;
use executor_lib::RepoShardedProcess;
use executor_lib::RepoShardedProcessExecutor;
use executor_lib::args::ShardedExecutorArgs;
use fbinit::FacebookInit;
use futures::channel::oneshot;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use metaconfig_types::RepoConfigRef;
use metaconfig_types::ShardedService;
use mononoke_api::CoreContext;
use mononoke_api::Repo;
use mononoke_app::MononokeApp;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::MononokeReposManager;
use mononoke_app::args::HooksAppExtension;
use mononoke_app::args::McrouterAppExtension;
use mononoke_app::args::ReadonlyArgs;
use mononoke_app::args::RepoFilterAppExtension;
use mononoke_app::args::ShutdownTimeoutArgs;
use mononoke_app::args::TLSArgs;
use mononoke_app::args::WarmBookmarksCacheExtension;
use mononoke_app::monitoring::MonitoringAppExtension;
use mononoke_app::monitoring::ReadyFlagService;
use openssl::ssl::AlpnError;
use repo_identity::RepoIdentityRef;
use scuba_ext::MononokeScubaSampleBuilder;
use sharding_ext::RepoShard;
use tracing::Instrument;
use tracing::error;
use tracing::info;
use tracing::warn;

const SM_CLEANUP_TIMEOUT_SECS: u64 = 120;

// We will select the first protocol supported by the server which is also supported by the client.
// Order of preferences: h2, http/1.1.
pub const ALPN_MONONOKE_PROTOS_OFFERS: &[u8] = b"\x02h2\x08http/1.1";

/// Mononoke Server
#[derive(Parser)]
struct MononokeServerArgs {
    #[clap(flatten)]
    shutdown_timeout_args: ShutdownTimeoutArgs,
    #[clap(flatten)]
    scribe_logging_args: ScribeLoggingArgs,
    /// TCP address to listen to in format `host:port
    #[clap(long)]
    listening_host_port: String,
    /// Path for file in which to write the bound tcp address in rust std::net::SocketAddr format
    #[clap(long)]
    bound_address_file: Option<PathBuf>,
    /// If provided the thrift server will start on this port
    #[clap(long, short = 'p')]
    thrift_port: Option<String>,
    /// TLS parameters for this service
    #[clap(flatten)]
    tls_args: TLSArgs,
    /// Top level Mononoke tier where CSLB publishes routing table
    #[clap(long)]
    cslb_config: Option<String>,
    #[clap(flatten)]
    readonly: ReadonlyArgs,
    #[clap(flatten)]
    sharded_executor_args: ShardedExecutorArgs,
    /// Path to a file with land service client certificate
    #[clap(long)]
    land_service_client_cert: Option<String>,
    /// Path to a file with land service client private key
    #[clap(long, requires = "land_service_client_cert")]
    land_service_client_private_key: Option<String>,
    /// Configerator path to the MononokeRateLimits config to load
    #[clap(long, default_value = "scm/mononoke/ratelimiting/ratelimits")]
    rate_limit_config_path: Option<String>,
    /// RIM workload backend ID for EdenAPI rate limiting
    #[clap(long)]
    rim_backend: Option<i32>,
    /// Report measured CPU microseconds to cpu_<region> in rim_backend.
    #[clap(long, requires = "rim_backend")]
    rim_regional_resources: bool,
    /// Mark this instance as a shadow tier. Shadow tiers never forward
    /// shadow traffic, preventing forwarding loops.
    #[clap(long, default_value_t = false)]
    shadow_tier: bool,
    /// Switch cryptocat into test mode so CATs minted by `mononoke-testtool
    /// cat-mint` verify locally against an in-process test keychain — no
    /// TokenService traffic, real MAC math against test keys.
    ///
    /// MUST NOT be set on production tiers. Intended only for integration
    /// tests that need to exercise positive CAT auth paths.
    #[clap(long, default_value_t = false, hide = true)]
    dangerously_skipping_cat_verification_for_tests: bool,
}

/// Struct representing the Mononoke server process when sharding by repo.
pub struct MononokeServerProcess {
    fb: FacebookInit,
    scuba: MononokeScubaSampleBuilder,
    repos_mgr: Arc<MononokeReposManager<Repo>>,
}

impl MononokeServerProcess {
    fn new(
        fb: FacebookInit,
        repos_mgr: Arc<MononokeReposManager<Repo>>,
        scuba: MononokeScubaSampleBuilder,
    ) -> Self {
        Self {
            fb,
            repos_mgr,
            scuba,
        }
    }

    async fn add_repo(&self, repo_name: &str, scuba: &MononokeScubaSampleBuilder) -> Result<()> {
        // Check if the input repo is already initialized. This can happen if the repo is a
        // shallow-sharded repo, in which case it would already be initialized during service startup.
        if self.repos_mgr.repos().get_by_name(repo_name).is_none() {
            // The input repo is a deep-sharded repo, so it needs to be added now.
            let repo = self.repos_mgr.add_repo(repo_name).await?;
            let cache_warmup_params = repo.repo_config().cache_warmup.clone();
            let ctx = CoreContext::new_with_scuba(self.fb, scuba.clone());
            cache_warmup(
                &ctx,
                &repo,
                cache_warmup_params,
                CacheWarmupKind::MononokeServer,
            )
            .await
            .with_context(|| format!("Error while warming up cache for repo {repo_name}"))?;
            info!("Completed repo {} setup in Mononoke service", repo_name);
        } else {
            info!("Repo {} is already setup in Mononoke service", repo_name);
        }
        Ok(())
    }
}

#[async_trait]
impl RepoShardedProcess for MononokeServerProcess {
    async fn setup(&self, repo: &RepoShard) -> anyhow::Result<Arc<dyn RepoShardedProcessExecutor>> {
        let repo_name = repo.repo_name.clone();
        info!("Setting up repo {} in Mononoke service", repo_name);
        self.add_repo(&repo_name, &self.scuba.clone())
            .await
            .with_context(|| {
                format!("Failure in setting up repo {repo_name} in Mononoke service")
            })?;

        Ok(Arc::new(MononokeServerProcessExecutor {
            repo_name,
            repos_mgr: self.repos_mgr.clone(),
        }))
    }
}

/// Struct representing the execution of the Mononoke server for a particular
/// repo when sharding by repo.
pub struct MononokeServerProcessExecutor {
    repo_name: String,
    repos_mgr: Arc<MononokeReposManager<Repo>>,
}

impl MononokeServerProcessExecutor {
    fn remove_repo(&self, repo_name: &str) -> Result<()> {
        let config = self.repos_mgr.repo_config(repo_name).with_context(|| {
            format!("Failure in remove repo {repo_name}. The config for repo doesn't exist")
        })?;
        // Check if the current repo is a deep-sharded or shallow-sharded repo. If the
        // repo is deep-sharded, then remove it since SM wants some other host to serve it.
        // If repo is shallow-sharded, then keep it since regardless of SM sharding, shallow
        // sharded repos need to be present on each host.
        let is_deep_sharded = config
            .deep_sharding_config
            .and_then(|c| c.status.get(&ShardedService::SaplingRemoteApi).copied())
            .unwrap_or(false);

        if is_deep_sharded {
            self.repos_mgr.remove_repo(repo_name);
            info!("No longer serving repo {} in Mononoke service.", repo_name,);
        } else {
            info!(
                "Continuing serving repo {} in Mononoke service because it's shallow-sharded.",
                repo_name,
            );
        }
        Ok(())
    }
}

#[async_trait]
impl RepoShardedProcessExecutor for MononokeServerProcessExecutor {
    async fn execute(&self) -> anyhow::Result<()> {
        info!("Serving repo {} in Mononoke service", &self.repo_name,);

        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.remove_repo(&self.repo_name)
            .with_context(|| format!("Failure in stopping repo {}", self.repo_name))
    }
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<()> {
    let app = MononokeAppBuilder::new(fb)
        .with_default_scuba_dataset("mononoke_test_perf")
        .with_entry_point(ClientEntryPoint::SaplingRemoteApi)
        .with_bookmarks_cache(BookmarkCacheOptions {
            cache_kind: BookmarkCacheKind::Local,
            derived_data: BookmarkCacheDerivedData::HgOnly,
        })
        .with_app_extension(WarmBookmarksCacheExtension {})
        .with_app_extension(McrouterAppExtension {})
        .with_app_extension(MonitoringAppExtension {})
        .with_app_extension(HooksAppExtension {})
        .with_app_extension(RepoFilterAppExtension {})
        .build::<MononokeServerArgs>()?;
    let args: MononokeServerArgs = app.args()?;

    if args.dangerously_skipping_cat_verification_for_tests {
        cats::enable_test_mode();
    }

    // No Thrift ServiceFramework here to register `ContextPropModule`, so the
    // handlers outbound calls need are installed by hand, before tracing starts.
    #[cfg(fbcode_build)]
    contextprop::install_thrift_handlers();

    let runtime = app.runtime().clone();

    let cslb_config = args.cslb_config.clone();
    let rate_limit_config_path = args.rate_limit_config_path.clone();
    info!("Starting up");

    #[cfg(fbcode_build)]
    if let (Some(land_service_cert_path), Some(land_service_key_path)) = (
        &args.land_service_client_cert,
        &args.land_service_client_private_key,
    ) {
        pushrebase_client::land_service_override_certificate_paths(
            land_service_cert_path,
            land_service_key_path,
            &args.tls_args.tls_ca,
        );
    }

    let configs = app.repo_configs();

    let acceptor = {
        let mut builder = secure_utils::SslConfig::new(
            &args.tls_args.tls_ca,
            &args.tls_args.tls_certificate,
            &args.tls_args.tls_private_key,
            args.tls_args.tls_ticket_seeds,
        )
        .tls_acceptor_builder(())
        .context("Failed to instantiate TLS Acceptor builder")?;

        builder.set_alpn_protos(ALPN_MONONOKE_PROTOS_OFFERS)?;
        builder.set_alpn_select_callback(|_, list| {
            openssl::ssl::select_next_proto(ALPN_MONONOKE_PROTOS_OFFERS, list)
                .ok_or(AlpnError::NOACK)
        });

        if args.tls_args.disable_mtls {
            warn!("MTLS has been disabled!");
            builder.set_verify(openssl::ssl::SslVerifyMode::NONE)
        }

        builder.build()
    };

    info!("Creating repo listeners");

    let scribe = args.scribe_logging_args.get_scribe(fb)?;
    let host_port = args.listening_host_port;
    let bound_addr_file = args.bound_address_file;

    let service = ReadyFlagService::new();
    let (terminate_sender, terminate_receiver) = oneshot::channel::<()>();
    let will_exit = Arc::new(AtomicBool::new(false));
    let (sm_shutdown_sender, sm_shutdown_receiver) = tokio::sync::oneshot::channel::<bool>();

    let env = app.environment();
    let scuba = env.scuba_sample_builder.clone();
    // Service name is used for shallow or deep sharding. If sharding itself is disabled, provide
    // service name as None while opening repos.
    let service_name = args
        .sharded_executor_args
        .is_sharded()
        .then_some(ShardedService::SaplingRemoteApi);
    app.start_monitoring(&runtime, "mononoke_server", service.clone())?;
    app.start_stats_aggregation()?;

    let repo_listeners = {
        cloned!(will_exit, env, runtime, service_name);
        move |app: MononokeApp| async move {
            let common = configs.common.clone();
            let repos_mgr = app.open_managed_repos::<Repo>(service_name).await?;
            let mononoke = Arc::new(repos_mgr.make_mononoke_api()?);
            info!("Built Mononoke");

            info!("Warming up cache");
            stream::iter(mononoke.repos())
                .map(|repo| {
                    let repo_name = repo.repo_identity().name().to_string();
                    let cache_warmup_params = repo.repo_config().cache_warmup.clone();
                    cloned!(scuba);
                    async move {
                        let ctx = CoreContext::new_with_scuba(fb, scuba);
                        cache_warmup(
                            &ctx,
                            &repo,
                            cache_warmup_params,
                            CacheWarmupKind::MononokeServer,
                        )
                        .instrument(tracing::info_span!("cache warmup", repo = %repo_name))
                        .await
                        .with_context(|| {
                            format!("Error while warming up cache for repo {repo_name}")
                        })
                    }
                })
                // Repo cache warmup can be quite expensive, let's limit to 40
                // at a time.
                .buffer_unordered(40)
                .try_collect::<()>()
                .await?;
            info!("Cache warmup completed");
            let repos_mgr = Arc::new(repos_mgr);
            if let Some(executor) = args.sharded_executor_args.build_executor(
                app.fb,
                runtime.clone(),
                || {
                    Arc::new(MononokeServerProcess::new(
                        app.fb,
                        repos_mgr.clone(),
                        scuba.clone(),
                    ))
                },
                false, // disable shard (repo) level healing
                SM_CLEANUP_TIMEOUT_SECS,
            )? {
                // The Sharded Process Executor needs to branch off and execute
                // on its own dedicated task spawned off the common tokio runtime.
                runtime.spawn(executor.block_and_execute(sm_shutdown_receiver));
            }
            repo_listener::create_repo_listeners(
                fb,
                common,
                mononoke.clone(),
                host_port,
                acceptor,
                service,
                terminate_receiver,
                &env.config_store,
                scribe,
                &scuba,
                will_exit,
                cslb_config,
                bound_addr_file,
                env.acl_provider.as_ref(),
                args.readonly.readonly,
                args.tls_args.disable_mtls,
                args.shadow_tier,
                Some(Path::new(&args.tls_args.tls_ca)),
                rate_limit_config_path,
                args.rim_backend,
                args.rim_regional_resources,
            )
            .await
        }
    };

    app.run_until_terminated(
        repo_listeners,
        move || {
            let _ = sm_shutdown_sender.send(true);
            will_exit.store(true, Ordering::Relaxed)
        },
        args.shutdown_timeout_args.shutdown_grace_period,
        async {
            if let Err(err) = terminate_sender.send(()) {
                error!("could not send termination signal: {:?}", err);
            }
            repo_listener::wait_for_connections_closed().await;
        },
        args.shutdown_timeout_args.shutdown_timeout,
        None,
    )
}
