/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![recursion_limit = "256"]
#![feature(never_type)]

use std::fs::File;
use std::io::Write;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use cached_config::ConfigHandle;
use clap::Parser;
use clientinfo::ClientEntryPoint;
use cloned::cloned;
use cmdlib_caching::CachelibSettings;
use connection_security_checker::ConnectionSecurityChecker;
use fbinit::FacebookInit;
use filestore::FilestoreConfig;
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
use gotham_ext::middleware::ScubaMiddleware;
use gotham_ext::middleware::ServerIdentityMiddleware;
use gotham_ext::middleware::TimerMiddleware;
use gotham_ext::middleware::TlsSessionDataMiddleware;
use gotham_ext::serve;
use http::header::HeaderValue;
use metaconfig_types::RepoConfig;
use metaconfig_types::RepoConfigSource;
use metaconfig_types::ShardedService;
use mononoke_app::MononokeApp;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::args::ReadonlyArgs;
use mononoke_app::args::RepoFilterAppExtension;
use mononoke_app::args::ShutdownTimeoutArgs;
use mononoke_app::args::TLSArgs;
use mononoke_app::args::parse_config_spec_to_path;
use mononoke_app::monitoring::AliveService;
use mononoke_app::monitoring::MonitoringAppExtension;
use mononoke_repos::MononokeRepos;
use repo_blobstore::RepoBlobstore;
use repo_identity::RepoIdentity;
use repo_permission_checker::RepoPermissionChecker;
use tokio::net::TcpListener;
use tracing::info;

use crate::lfs_server_context::LfsServerContext;
use crate::lfs_server_context::ServerUris;
use crate::lfs_server_context::get_bandwidth;
use crate::middleware::OdsMiddleware;
use crate::middleware::RequestContextMiddleware;
use crate::scuba::LfsScubaHandler;
use crate::service::build_router;

mod batch;
mod compression_sniff;
mod config;
mod download;
mod errors;
mod git_upload;
mod lfs_server_context;
mod middleware;
mod popularity;
mod scuba;
mod service;
mod upload;

const SERVICE_NAME: &str = "mononoke_lfs_server";

// Used to determine how many entries are in cachelib's HashTable. A smaller
// object size results in more entries and possibly higher idle memory usage.
// More info: https://fburl.com/wiki/i78i3uzk
const CACHE_OBJECT_SIZE: usize = 256 * 1024;

#[facet::container]
#[derive(Clone)]
pub struct Repo {
    #[facet]
    repo_identity: RepoIdentity,

    #[init(repo_identity.name().to_string())]
    name: String,

    #[facet]
    repo_config: RepoConfig,

    #[facet]
    repo_config_source: RepoConfigSource,

    #[facet]
    filestore_config: FilestoreConfig,

    #[facet]
    repo_blobstore: RepoBlobstore,

    #[facet]
    repo_permission_checker: dyn RepoPermissionChecker,
}

/// Mononoke LFS Server
#[derive(Parser)]
struct LfsServerArgs {
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
    /// Path for file in which to write the bound tcp address in rust std::net::SocketAddr format
    #[clap(long)]
    bound_address_file: Option<String>,
    /// The base URLs for this server
    #[clap(value_delimiter = ',')]
    self_urls: Vec<String>,
    /// The base URL for an upstream server
    #[clap(long)]
    upstream_url: Option<String>,
    /// Whether to always wait for an upstream response (primarily useful in testing)
    #[clap(long)]
    always_wait_for_upstream: bool,
    /// Path to config in configerator
    #[clap(long)]
    live_config: Option<String>,
    /// Whether or not to use test-friendly logging
    #[clap(long)]
    test_friendly_logging: bool,
    // A file to which to log TLS session data, including master secrets.
    // Use this for debugging with tcpdump.
    // Note that this compromises the secrecy of TLS sessions.
    #[clap(long)]
    tls_session_data_log_file: Option<String>,
    /// Whether to enable Mononoke-specific small git blob uploads
    #[clap(long)]
    git_blob_upload_allowed: bool,
    /// A limit (in bytes) to enforce for uploads.
    #[clap(long)]
    max_upload_size: Option<u64>,
    #[clap(flatten)]
    readonly: ReadonlyArgs,
    /// Whether to require the client-info header or not.
    #[clap(long, default_value = "false")]
    dont_require_client_info: bool,
    /// Hosts for which to force http scheme in generated hrefs (comma-separated)
    #[clap(long, value_delimiter = ',')]
    force_http_for_host: Vec<String>,
    /// Headers whose presence (with value "1") forces http scheme in generated hrefs (comma-separated)
    #[clap(long, value_delimiter = ',')]
    force_http_for_header: Vec<String>,
    /// Enable per-blob magic-byte sniffing on the download path. When on, the
    /// server pulls the first chunk of each blob, checks for a known
    /// already-compressed container format (zip/apk/zstd/png/jpeg/mp4/...),
    /// and bypasses gzip/zstd response compression for matches. Per-request
    /// behavior is additionally gated by the
    /// `scm/mononoke:lfs_server_compression_sniff_enabled` JustKnob, so a
    /// deployment can be built with this flag but kept off in production.
    #[clap(long)]
    enable_compression_sniff: bool,
}

#[derive(Clone)]
pub struct LfsRepos {
    pub(crate) repos: Arc<MononokeRepos<Repo>>,
}

impl LfsRepos {
    pub(crate) async fn new(app: &MononokeApp) -> Result<Self> {
        let repos_mgr = app
            .open_managed_repos(Some(ShardedService::LargeFilesService))
            .await?;
        let repos = repos_mgr.repos().clone();
        Ok(Self { repos })
    }

    pub(crate) fn get(&self, repo_name: &str) -> Option<Arc<Repo>> {
        self.repos.get_by_name(repo_name)
    }
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<(), Error> {
    let cachelib_settings = CachelibSettings {
        expected_item_size_bytes: Some(CACHE_OBJECT_SIZE),
        ..Default::default()
    };

    let app = MononokeAppBuilder::new(fb)
        .with_entry_point(ClientEntryPoint::LfsServer)
        .with_app_extension(MonitoringAppExtension {})
        .with_app_extension(RepoFilterAppExtension {})
        .with_cachelib_settings(cachelib_settings)
        .build::<LfsServerArgs>()?;

    let args: LfsServerArgs = app.args()?;

    let config_store = app.config_store();
    let acl_provider = app.environment().acl_provider.clone();

    let listen_host = args.listen_host.clone();
    let listen_port = args.listen_port.clone();
    let bound_addr_path = args.bound_address_file.clone();

    let git_blob_upload_allowed = args.git_blob_upload_allowed;

    let addr = format!("{listen_host}:{listen_port}");

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

    let tls_session_data_log = args.tls_session_data_log_file.clone();

    let scuba_logger = app.environment().scuba_sample_builder.clone();

    let will_exit = Arc::new(AtomicBool::new(false));

    let config_handle = match &args.live_config {
        Some(spec) => config_store.get_config_handle_DEPRECATED(parse_config_spec_to_path(spec)?),
        None => Ok(ConfigHandle::default()),
    };

    let config_handle = config_handle.context(Error::msg("Failed to load configuration"))?;

    let max_upload_size: Option<u64> = args.max_upload_size;

    let self_urls = args.self_urls;
    let upstream_url = args.upstream_url;
    let always_wait_for_upstream = args.always_wait_for_upstream;
    let force_http_for_host = args.force_http_for_host;
    let force_http_for_header = args.force_http_for_header;
    let log_middleware = if args.test_friendly_logging {
        LogMiddleware::test_friendly()
    } else {
        LogMiddleware::tracing()
    };

    app.start_monitoring(app.runtime(), SERVICE_NAME, AliveService)?;
    app.start_stats_aggregation()?;

    let common = &app.repo_configs().common;
    let internal_identity = common.internal_identity.clone();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = {
        cloned!(acl_provider, common, will_exit);
        move |app| async move {
            let repos = LfsRepos::new(&app)
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

            // For tests we use one empty string self_url, map it to None
            let self_urls =
                if self_urls.is_empty() || (self_urls.len() == 1 && self_urls[0].is_empty()) {
                    None
                } else {
                    Some(self_urls)
                };

            let self_urls = if let Some(self_urls) = self_urls {
                self_urls
            } else {
                let protocol = if tls_acceptor.is_some() {
                    "https://"
                } else {
                    "http://"
                };
                vec![protocol.to_owned() + &bound_addr]
            };

            let server_uris = ServerUris::new(
                self_urls,
                upstream_url,
                force_http_for_host,
                force_http_for_header,
            )?;

            let bandwidth = get_bandwidth();

            let ctx = LfsServerContext::new(
                repos,
                server_uris,
                always_wait_for_upstream,
                max_upload_size,
                will_exit,
                config_handle.clone(),
                &args.tls_params,
                bandwidth,
                args.enable_compression_sniff,
            )?;
            let enforce_authentication = ctx.get_config().enforce_authentication();

            let router = build_router(fb, ctx, scuba_logger.clone(), git_blob_upload_allowed);

            let capture_session_data = tls_session_data_log.is_some();

            let handler = MononokeHttpHandler::builder()
                .add(TlsSessionDataMiddleware::new(tls_session_data_log)?)
                .add(MetadataMiddleware::new(
                    fb,
                    internal_identity,
                    ClientEntryPoint::LfsServer,
                    false,
                    !args.dont_require_client_info,
                ))
                .add(PostResponseMiddleware::with_config(config_handle))
                .add(<ScubaMiddleware<LfsScubaHandler>>::new(scuba_logger))
                .add(RequestContextMiddleware::new(
                    fb,
                    enforce_authentication,
                    args.readonly.readonly,
                ))
                .add(ArtilleryMiddleware::new(fb))
                .add(LoadMiddleware::new())
                .add(log_middleware)
                .add(ServerIdentityMiddleware::new(HeaderValue::from_static(
                    "mononoke-lfs",
                )))
                .add(OdsMiddleware::new())
                .add(TimerMiddleware::new())
                .build(router);

            info!("Listening on {}", bound_addr);

            // Write out the bound address if requested, this is helpful in tests when using automatic binding with :0
            if let Some(bound_addr_path) = bound_addr_path {
                let mut writer = File::create(bound_addr_path)?;
                writer.write_all(bound_addr.as_bytes())?;
                writer.write_all(b"\n")?;
            }

            let serve = async move {
                if let Some(tls_acceptor) = tls_acceptor {
                    let connection_security_checker =
                        ConnectionSecurityChecker::new(acl_provider.as_ref(), &common).await?;

                    serve::https(
                        listener,
                        tls_acceptor,
                        capture_session_data,
                        connection_security_checker,
                        handler,
                        true, // http1_vectored_writes
                    )
                    .await
                } else {
                    serve::http(listener, handler, true).await
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

    app.run_until_terminated(
        server,
        move || will_exit.store(true, Ordering::Relaxed),
        args.shutdown_timeout_args.shutdown_grace_period,
        async move {
            let _ = shutdown_tx.send(());
            // Currently we kill off in-flight requests as soon as we've closed the listener.
            // If this is a problem in prod, this would be the point at which to wait
            // for all connections to shut down.
            // To do this properly, we'd need to track the `Connection` futures that Gotham
            // gets from Hyper, tell them to gracefully shutdown, then wait for them to complete
        },
        args.shutdown_timeout_args.shutdown_timeout,
        None,
    )?;

    info!("Exiting...");
    Ok(())
}
