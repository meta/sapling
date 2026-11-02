/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Error;
use anyhow::Result;
use clap::Parser;
use cmdlib_logging::ScribeLoggingArgs;
use fbinit::FacebookInit;
use land_service_if_services::*;
use metaconfig_types::ShardedService;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::args::RepoFilterAppExtension;
use mononoke_app::args::ShutdownTimeoutArgs;
use mononoke_thrift_server::DEFAULT_THRIFT_HOST;
use mononoke_thrift_server::ThriftServerBuilder;
use tracing::info;

const SERVICE_NAME: &str = "mononoke_land_service";

mod errors;
#[cfg(fbcode_build)]
mod facebook;
mod land_service_impl;
#[cfg(not(fbcode_build))]
mod oss;
mod repo;
mod worker;

#[cfg(fbcode_build)]
use facebook as platform;
#[cfg(not(fbcode_build))]
use oss as platform;

#[derive(Debug, Parser)]
struct LandServiceServerArgs {
    #[clap(flatten)]
    shutdown_timeout_args: ShutdownTimeoutArgs,
    #[clap(flatten)]
    scribe_logging_args: ScribeLoggingArgs,
    /// Thrift host
    #[clap(long, short = 'H', default_value = DEFAULT_THRIFT_HOST)]
    host: String,
    /// Thrift port
    #[clap(long, short = 'p', default_value_t = 8485)]
    port: u16,
    /// Path for file in which to write the bound tcp address in rust std::net::SocketAddr format
    #[clap(long)]
    bound_address_file: Option<String>,
    #[clap(flatten)]
    platform: platform::ServerArgs,
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<(), Error> {
    let app = MononokeAppBuilder::new(fb)
        .with_app_extension(RepoFilterAppExtension {})
        .build::<LandServiceServerArgs>()?;

    let args: LandServiceServerArgs = app.args()?;

    let runtime = app.runtime();
    let exec = runtime.clone();
    let env = app.environment();

    let scuba_builder = env.scuba_sample_builder.clone();
    let repos_mgr =
        Arc::new(runtime.block_on(app.open_managed_repos(Some(ShardedService::LandService)))?);

    let platform_state = runtime.block_on(platform::ServiceState::new(&app, &args.platform))?;

    let land_service_server = land_service_impl::LandServiceImpl::new(
        fb,
        repos_mgr,
        scuba_builder,
        args.scribe_logging_args.get_scribe(fb)?,
        platform_state,
    );

    let service = { move |proto| make_LandService_server(proto, land_service_server.clone()) };

    let builder = ThriftServerBuilder::new(fb)
        .service_name(SERVICE_NAME)
        .host(&args.host)
        .port(args.port)
        .with_handle(exec.clone())
        .with_tls()
        .with_cancel_on_client_disconnect()
        .factory(exec, move || service);
    let mut server = platform::configure_server(builder, &app).build()?;

    info!("Listening on {}", server.bound_address()?);

    if let Some(bound_addr_path) = &args.bound_address_file {
        server.write_bound_address(bound_addr_path)?;
    }

    // Monitoring is provided by the `Fb303Module`, but we must still start
    // stats aggregation.
    app.start_stats_aggregation()?;

    let handle = server.into_shutdown_handle();
    app.wait_until_terminated(
        || {},
        args.shutdown_timeout_args.shutdown_grace_period,
        async {
            // Note that async blocks are lazy, so this isn't called until first poll
            let _ = tokio::task::spawn_blocking(move || {
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
