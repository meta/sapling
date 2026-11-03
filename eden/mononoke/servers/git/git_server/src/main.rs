/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![recursion_limit = "256"]
#![feature(never_type)]
#![feature(cursor_split)]
#![feature(try_blocks)]

use std::fs::File;
use std::io::Write;
use std::net::ToSocketAddrs;
#[cfg(fbcode_build)]
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use clap::Parser;
use clientinfo::ClientEntryPoint;
use cloned::cloned;
use cmdlib_caching::CachelibSettings;
use connection_security_checker::ConnectionSecurityChecker;
use environment::BookmarkCacheDerivedData;
use environment::BookmarkCacheKind;
use environment::BookmarkCacheOptions;
use executor_lib::args::ShardedExecutorArgs;
use fbinit::FacebookInit;
use futures::TryFutureExt;
use futures::channel::oneshot;
use futures::future::try_select;
use futures::pin_mut;
use gotham_ext::handler::MononokeHttpHandler;
use gotham_ext::middleware::ArtilleryMiddleware;
use gotham_ext::middleware::LoadMiddleware;
use gotham_ext::middleware::LogMiddleware;
use gotham_ext::middleware::MetadataMiddleware;
use gotham_ext::middleware::PostResponseMiddleware;
use gotham_ext::middleware::RequestContextMiddleware;
use gotham_ext::middleware::ScubaMiddleware;
use gotham_ext::middleware::ServerIdentityMiddleware;
use gotham_ext::middleware::TimerMiddleware;
use gotham_ext::middleware::TlsSessionDataMiddleware;
use gotham_ext::serve;
use http::HeaderValue;
use metaconfig_parser::RepoConfigs;
use metaconfig_types::ShardedService;
use middleware::PushvarsParsingMiddleware;
use mononoke_api::Repo;
use mononoke_app::MononokeApp;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::MononokeReposManager;
use mononoke_app::ShutdownGracePeriod;
use mononoke_app::args::McrouterAppExtension;
use mononoke_app::args::ReadonlyArgs;
use mononoke_app::args::RepoFilterAppExtension;
use mononoke_app::args::ShutdownTimeoutArgs;
use mononoke_app::args::TLSArgs;
use mononoke_app::args::WarmBookmarksCacheExtension;
use mononoke_app::monitoring::AliveService;
use mononoke_app::monitoring::MonitoringAppExtension;
use ods_counters::OdsCounterManager;
use rate_limiting::RateLimitEnvironment;
use scuba_ext::MononokeScubaSampleBuilder;
#[cfg(fbcode_build)]
use shadow_forwarder::ShadowForwarderMiddleware;
use stats::prelude::*;
use tokio::net::TcpListener;
use tracing::info;

use crate::middleware::Ods3Middleware;
use crate::middleware::RequestContentEncodingMiddleware;
use crate::middleware::ResponseContentTypeMiddleware;
use crate::middleware::UploadPackRateLimitingMiddleware;
use crate::model::GitServerContext;
use crate::scuba::MononokeGitScubaHandler;
use crate::service::build_router;
use crate::sharding::MononokeGitServerProcess;
mod command;

mod errors;
mod middleware;
mod model;
mod profiling;
mod read;
mod scuba;
mod service;
mod sharding;
mod util;
mod write;

define_stats! {
    // Reads TW container fb303 counters for memory-based health check.
    // Same counter pattern used by rate_limiting for load shedding.
    prefix = "mononoke.git.server";
    container_memory: dynamic_singleton_counter("{}", (counter_name: String)),
}

const SERVICE_NAME: &str = "mononoke_git_server";
const SM_CLEANUP_TIMEOUT_SECS: u64 = 60;
/// Configerator path for rate limiting config
const CONFIGERATOR_RATE_LIMITING_CONFIG: &str = "scm/mononoke/ratelimiting/git_ratelimits";
/// JustKnob to enable vectored writes for HTTP/1.1 connections.
/// When enabled, hyper uses Queue strategy (zero-copy) instead of Flatten
/// (copies all body data into a single Vec), preventing multi-GB allocations
/// for large streaming responses like git packfiles.
const HTTP1_VECTORED_WRITES: &str = "scm/mononoke:http1_vectored_writes_enabled";
/// JustKnob to override the shutdown grace period (in seconds).
/// During SEVs, set this to a low value (e.g., 60) for fast rollback.
const SHUTDOWN_GRACE_PERIOD_OVERRIDE: &str = "scm/mononoke:git_server_shutdown_grace_period_secs";
// Used to determine how many entries are in cachelib's HashTable. A smaller
// object size results in more entries and possibly higher idle memory usage.
// More info: https://fburl.com/wiki/i78i3uzk
const CACHE_OBJECT_SIZE: usize = 256 * 1024;

struct DynamicGracePeriod;

impl ShutdownGracePeriod for DynamicGracePeriod {
    fn resolve(&self) -> Duration {
        Duration::from_secs(justknobs::get_as::<u64>(
            SHUTDOWN_GRACE_PERIOD_OVERRIDE,
            None,
        ))
    }
}

/// URL pattern used by the upstream LFS server to serve a single object by SHA256.
#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum UpstreamLfsUrlFormat {
    /// Dewey-style: `GET {server}/{sha256}`
    #[default]
    Dewey,
    /// Mononoke git-LFS: `GET {server}/{repo}/download_sha256/{sha256}`
    MononokeGitLfs,
}

/// Mononoke Git Server
#[derive(Parser)]
struct GitServerArgs {
    /// Shutdown timeout args for this service
    #[clap(flatten)]
    shutdown_timeout_args: ShutdownTimeoutArgs,
    /// TLS parameters for this service
    #[clap(flatten)]
    tls_params: Option<TLSArgs>,
    /// The host to listen on locally
    #[clap(long, default_value = "127.0.0.1")]
    listen_host: String,
    /// The port to listen on locally
    #[clap(long, default_value = "8001")]
    listen_port: String,
    // Use this for debugging with tcpdump.
    // Note that this compromises the secrecy of TLS sessions.
    #[clap(long)]
    tls_session_data_log_file: Option<String>,
    /// Path for file in which to write the bound tcp address in rust std::net::SocketAddr format
    #[clap(long)]
    bound_address_file: Option<String>,
    /// Args for sharding of repos on Mononoke Git Server
    #[clap(flatten)]
    sharded_executor_args: ShardedExecutorArgs,
    /// Flag determining if the server should be read-only
    #[clap(flatten)]
    readonly: ReadonlyArgs,
    /// Flag determining if the server should skip enforcing authorization
    #[clap(long)]
    skip_authorization: bool,
    /// Whether or not to use test-friendly logging
    #[clap(long)]
    test_friendly_logging: bool,
    /// LFS server URL to fetch LFS files from over HTTP. When unset, the git
    /// server falls back to internal mode (resolves pointers from the local
    /// Mononoke filestore by SHA256 alias). Mutually exclusive with
    /// `--internal-lfs`.
    #[clap(long, conflicts_with = "internal_lfs")]
    upstream_lfs_server: Option<String>,
    /// URL pattern that the upstream LFS server uses to serve raw objects by SHA256.
    /// Defaults to the Dewey-style `GET {server}/{sha256}`. Use `mononoke-git-lfs` for
    /// the `GET {server}/{repo}/download_sha256/{sha256}` shape served by Mononoke LFS.
    #[clap(long, value_enum, default_value_t = UpstreamLfsUrlFormat::Dewey)]
    upstream_lfs_url_format: UpstreamLfsUrlFormat,
    /// Explicitly request internal mode (resolve LFS pointers from the local
    /// Mononoke filestore by SHA256 alias). Internal mode is also the default
    /// when `--upstream-lfs-server` is not set; this flag is mainly useful for
    /// documentation or to force a clap error if `--upstream-lfs-server` is
    /// also passed by mistake. Mutually exclusive with `--upstream-lfs-server`.
    #[clap(long, default_value_t = false)]
    internal_lfs: bool,
    /// How many times to retry fetching LFS files from the server
    /// before deciding that the file is missing.
    #[clap(long, default_value_t = 5)]
    lfs_import_max_attempts: u32,
    /// Address (host:port) of the Multi-Repo Land Service for push
    /// diversion. When set, pushes to repos matching the configured marker
    /// are landed through this service instead of the normal bookmark
    /// movement path. If not set, SMC tier lookup is used in production.
    #[clap(long)]
    multi_repo_land_service_address: Option<String>,
    /// Mark this instance as a shadow tier. Shadow tiers never forward
    /// shadow traffic, preventing forwarding loops.
    #[clap(long, default_value_t = false)]
    shadow_tier: bool,
    /// On per-push gitimport failure, persist `bonsai_git_mapping` rows
    /// for commits already fully processed so retries can resume
    /// incrementally. Also surfaces the real underlying error to Scuba
    /// instead of the SendError cascade. See
    /// `GitimportPreferences::persist_partial_mappings`.
    #[clap(long, default_value_t = false)]
    persist_partial_mappings: bool,
}

#[derive(Clone)]
pub struct GitRepos {
    pub(crate) repo_mgr: Arc<MononokeReposManager<Repo>>,
}

impl GitRepos {
    pub(crate) async fn new(repo_mgr: Arc<MononokeReposManager<Repo>>) -> Result<Self> {
        Ok(Self { repo_mgr })
    }

    /// Builds the repo first if it was assigned without being built.
    pub(crate) async fn get(&self, repo_name: &str) -> Result<Option<Arc<Repo>>> {
        if !lazy_repo_build_enabled() {
            return Ok(self.repo_mgr.repos().get_by_name(repo_name));
        }
        self.repo_mgr.repos().get(repo_name).await
    }

    pub(crate) fn repo_configs(&self) -> Arc<RepoConfigs> {
        self.repo_mgr.configs().repo_configs()
    }
}

/// Whether an unbuilt repo may be built on its first request. `setup` reads
/// this too: with it off nothing builds a placeholder, so none may be created.
pub(crate) fn lazy_repo_build_enabled() -> bool {
    justknobs::eval("scm/mononoke:enable_git_server_lazy_repo_build", None, None)
}

/// Construct a memory-based health check for ShardManager.
/// Reports unhealthy at 70% container memory (avg.60), recovers below 50%.
/// Triggers SM to drain shards before load shedding (80%/90% on avg.10).
// TODO(prashantpal): Move thresholds to configerator.
fn construct_memory_health_check(
    fb: FacebookInit,
    scuba: MononokeScubaSampleBuilder,
) -> Option<Arc<dyn Fn() -> bool + Send + Sync>> {
    let was_unhealthy = AtomicBool::new(false);
    Some(Arc::new(move || -> bool {
        if !justknobs::eval(
            "scm/mononoke:git_server_enable_memory_health_check",
            None,
            None,
        ) {
            return true;
        }
        let util_pct = match STATS::container_memory
            .get_value(fb, (String::from("container_memory_usage_percent.avg.60"),))
        {
            Some(val) => val as f64,
            None => return !was_unhealthy.load(Ordering::SeqCst),
        };
        // No state transition — return current state
        let previously_unhealthy = was_unhealthy.load(Ordering::SeqCst);
        if previously_unhealthy && util_pct >= 50.0 {
            return false;
        }
        if !previously_unhealthy && util_pct <= 70.0 {
            return true;
        }
        // State transition — log to scuba
        let healthy = previously_unhealthy; // was unhealthy → now healthy, and vice versa
        was_unhealthy.store(!healthy, Ordering::SeqCst);
        let state = if healthy { "recovered" } else { "unhealthy" };
        info!("SM health check: {} — memory {:.1}%", state, util_pct);
        let mut s = scuba.clone();
        s.add("sm_health_check_state", state);
        s.add("container_memory_pct", util_pct as i64);
        s.log_with_msg("SM Health Check State Change", None);
        healthy
    }))
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<(), Error> {
    let cachelib_settings = CachelibSettings {
        expected_item_size_bytes: Some(CACHE_OBJECT_SIZE),
        ..Default::default()
    };

    let app = MononokeAppBuilder::new(fb)
        .with_default_scuba_dataset("mononoke_git_server")
        .with_entry_point(ClientEntryPoint::MononokeGitServer)
        .with_bookmarks_cache(BookmarkCacheOptions {
            cache_kind: BookmarkCacheKind::Local,
            derived_data: BookmarkCacheDerivedData::GitOnly, // Although derivation is already done at push time, commits landed through SCS still need WBC
        })
        .with_app_extension(WarmBookmarksCacheExtension {})
        .with_app_extension(McrouterAppExtension {})
        .with_app_extension(MonitoringAppExtension {})
        .with_app_extension(RepoFilterAppExtension {})
        .with_cachelib_settings(cachelib_settings)
        .build::<GitServerArgs>()?;

    let args: GitServerArgs = app.args()?;

    let listen_host = args.listen_host.clone();
    let listen_port = args.listen_port.clone();
    let bound_addr_path = args.bound_address_file.clone();

    let addr = format!("{listen_host}:{listen_port}");
    let common_config = app.repo_configs().common.clone();
    let tls_acceptor = args
        .tls_params
        .clone()
        .map(|tls_params| {
            secure_utils::SslConfig::new(
                tls_params.tls_ca,
                tls_params.tls_certificate,
                tls_params.tls_private_key,
                tls_params.tls_ticket_seeds,
            )
            .build_tls_acceptor(())
        })
        .transpose()?;
    let acl_provider = app.environment().acl_provider.clone();
    let scuba = app.environment().scuba_sample_builder.clone();
    let common = app.repo_configs().common.clone();
    let tls_session_data_log = args.tls_session_data_log_file.clone();
    let enforce_authorization = !args.skip_authorization;
    let log_middleware = if args.test_friendly_logging {
        LogMiddleware::test_friendly()
    } else {
        LogMiddleware::tracing()
    };
    let will_exit = Arc::new(AtomicBool::new(false));
    let (sm_shutdown_sender, sm_shutdown_receiver) = tokio::sync::oneshot::channel::<bool>();
    let runtime = app.runtime().clone();
    // Service name is used for shallow or deep sharding. If sharding itself is disabled, provide
    // service name as None while opening repos.
    let service_name = args
        .sharded_executor_args
        .sharded_service_name
        .as_ref()
        .map(|_| ShardedService::MononokeGitServer);
    app.start_monitoring(app.runtime(), SERVICE_NAME, AliveService)?;
    app.start_stats_aggregation()?;
    let rate_limiter = {
        let handle = app
            .environment()
            .config_store
            .get_config_handle_DEPRECATED(CONFIGERATOR_RATE_LIMITING_CONFIG.to_string())
            .ok();

        let ods_counter_manager = OdsCounterManager::new(fb);

        handle.and_then(|handle| {
            common_config.loadlimiter_category.clone().map(|category| {
                RateLimitEnvironment::new_with_runtime(
                    fb,
                    category,
                    handle,
                    ods_counter_manager,
                    runtime.clone(),
                )
            })
        })
    };

    let requests_counter = Arc::new(AtomicI64::new(0));
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = {
        cloned!(requests_counter, acl_provider);
        let tls_args = args.tls_params.clone();
        move |app: MononokeApp| async move {
            let repos_mgr = Arc::new(app.open_managed_repos(service_name).await?);
            let repos = GitRepos::new(repos_mgr.clone())
                .await
                .context(Error::msg("Error opening repos"))?;

            let addr = addr
                .to_socket_addrs()
                .context(Error::msg("Invalid Listener Address"))?
                .next()
                .ok_or_else(|| Error::msg("Invalid Socket Address"))?;

            let listener = TcpListener::bind(&addr)
                .await
                .context(Error::msg("Could not start TCP listener"))?;

            // We use the listen_host rather than the ip of listener.local_addr()
            // because the certs user passed will be referencing listen_host
            let bound_addr = format!("{}:{}", listen_host, listener.local_addr()?.port());
            // `--internal-lfs` defaults to true whenever `--upstream-lfs-server`
            // is unset, so passing no LFS flags at all behaves like
            // `--internal-lfs`. (When the user passes `--internal-lfs`
            // explicitly, args.internal_lfs is already true; clap's
            // `conflicts_with` keeps both from being set at once.)
            let internal_lfs = args.internal_lfs || args.upstream_lfs_server.is_none();
            let git_server_context = GitServerContext::new(
                fb,
                repos,
                enforce_authorization,
                args.upstream_lfs_server,
                args.upstream_lfs_url_format,
                internal_lfs,
                tls_args,
                acl_provider.clone(),
                args.multi_repo_land_service_address,
                args.persist_partial_mappings,
            );

            let router = build_router(git_server_context);

            let capture_session_data = tls_session_data_log.is_some();
            let mut git_scuba = scuba.clone();
            let perf_sampling =
                justknobs::get_as::<u64>("scm/mononoke:git_server_perf_log_sampling", None);
            git_scuba.sampled(perf_sampling.try_into()?);
            let handler = MononokeHttpHandler::builder()
                .add(TlsSessionDataMiddleware::new(tls_session_data_log)?)
                .add(ServerIdentityMiddleware::new(HeaderValue::from_static(
                    "mononoke_git_server",
                )))
                .add(MetadataMiddleware::new(
                    fb,
                    common.internal_identity.clone(),
                    ClientEntryPoint::MononokeGitServer,
                    false,
                    false,
                ))
                .add(RequestContentEncodingMiddleware {})
                .add(RequestContextMiddleware::new(
                    fb,
                    git_scuba,
                    None,
                    args.readonly.readonly,
                ))
                .add(ArtilleryMiddleware::new(fb))
                .add(UploadPackRateLimitingMiddleware::new(
                    scuba.clone(),
                    rate_limiter,
                ))
                .add(PushvarsParsingMiddleware {})
                .add(ResponseContentTypeMiddleware {})
                .add(PostResponseMiddleware::default());

            // Shadow forwarder runs after RequestContext and rate limiting,
            // so only non-rate-limited requests are forwarded to shadow.
            // This is intentional: git server only shadows non-throttled
            // traffic. (SLAPI intentionally places it before rate limiting
            // — see slapi_service/src/lib.rs.)
            // Only available in fbcode builds.
            #[cfg(fbcode_build)]
            let handler = handler.add(ShadowForwarderMiddleware::new(
                &app.environment().config_store,
                "scm/mononoke/shadow_traffic/git",
                args.shadow_tier,
                args.tls_params.as_ref().map(|t| Path::new(&t.tls_ca)),
            )?);

            let health_check_fn = construct_memory_health_check(app.fb, scuba.clone());

            let handler = handler
                .add(LoadMiddleware::new_with_requests_counter(requests_counter))
                .add(log_middleware)
                .add(Ods3Middleware::new())
                .add(<ScubaMiddleware<MononokeGitScubaHandler>>::new(scuba)) // We want request summary logging to remain unsampled (for now)
                .add(TimerMiddleware::new())
                .build(router);

            info!("Listening on {}", bound_addr);
            // Write out the bound address if requested, this is helpful in tests when using automatic binding with :0
            if let Some(bound_addr_path) = bound_addr_path {
                let mut writer = File::create(bound_addr_path)?;
                writer.write_all(bound_addr.as_bytes())?;
                writer.write_all(b"\n")?;
            }

            if let Some(executor) = args
                .sharded_executor_args
                .build_executor_with_health_check(
                    app.fb,
                    runtime.clone(),
                    || Arc::new(MononokeGitServerProcess::new(app.fb, repos_mgr)),
                    false, // disable shard (repo) level healing
                    SM_CLEANUP_TIMEOUT_SECS,
                    health_check_fn,
                )?
            {
                // The Sharded Process Executor needs to branch off and execute
                // on its own dedicated task spawned off the common tokio runtime.
                runtime.spawn(executor.block_and_execute(sm_shutdown_receiver));
            }

            let serve = async move {
                let http1_vectored_writes =
                    justknobs::eval(HTTP1_VECTORED_WRITES, None, Some(SERVICE_NAME));

                if let Some(tls_acceptor) = tls_acceptor {
                    let connection_security_checker =
                        ConnectionSecurityChecker::new(acl_provider.as_ref(), &common).await?;

                    serve::https(
                        listener,
                        tls_acceptor,
                        capture_session_data,
                        connection_security_checker,
                        handler,
                        http1_vectored_writes,
                    )
                    .await
                } else {
                    serve::http(listener, handler, http1_vectored_writes).await
                }
            };
            pin_mut!(serve);
            try_select(
                serve,
                shutdown_rx.map_err(|err| anyhow!("Cancelled channel: {err}")),
            )
            .await
            .map_err(|e| futures::future::Either::factor_first(e).0)?;
            Ok(())
        }
    };
    let shutdown_grace_period = DynamicGracePeriod;
    app.run_until_terminated(
        server,
        move || {
            will_exit.store(true, Ordering::SeqCst);
            let _ = sm_shutdown_sender.send(true);
        },
        shutdown_grace_period,
        async move {
            let _ = shutdown_tx.send(());
            // Currently we kill off in-flight requests as soon as we've closed the listener.
            // If this is a problem in prod, this would be the point at which to wait
            // for all connections to shut down.
            // To do this properly, we'd need to track the `Connection` futures that Gotham
            // gets from Hyper, tell them to gracefully shutdown, then wait for them to complete
        },
        args.shutdown_timeout_args.shutdown_timeout,
        // TODO
        Some(requests_counter),
    )?;

    info!("Exiting...");
    Ok(())
}
