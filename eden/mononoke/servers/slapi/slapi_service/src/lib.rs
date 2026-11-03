/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![feature(associated_type_defaults)]

pub mod context;
mod errors;
pub mod handlers;
pub mod middleware;
mod scuba;
pub mod utils;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[cfg(fbcode_build)]
use anyhow::Context;
use anyhow::Error;
#[cfg(fbcode_build)]
use backend_if::RimBackend;
use cached_config::ConfigStore;
use clientinfo::ClientEntryPoint;
use fbinit::FacebookInit;
#[cfg(fbcode_build)]
use fbwhoami::FbWhoAmI;
use gotham::router::Router;
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
use http::HeaderValue;
use metaconfig_types::CommonConfig;
use mononoke_api::Mononoke;
use rate_limiting::RateLimitEnvironment;
use scuba_ext::MononokeScubaSampleBuilder;
#[cfg(fbcode_build)]
use shadow_forwarder::ShadowForwarderMiddleware;

use crate::context::ServerContext;
use crate::handlers::build_router;
use crate::middleware::OdsMiddleware;
use crate::middleware::RequestDumperMiddleware;
use crate::middleware::ThrottleMiddleware;
use crate::scuba::SaplingRemoteApiScubaHandler;

pub type SaplingRemoteApi = MononokeHttpHandler<Router>;

pub fn build<R: Send + Sync + Clone + 'static>(
    fb: FacebookInit,
    scuba: MononokeScubaSampleBuilder,
    mononoke: Arc<Mononoke<R>>,
    will_exit: Arc<AtomicBool>,
    test_friendly_logging: bool,
    tls_session_data_log_path: Option<&Path>,
    rate_limiter: Option<RateLimitEnvironment>,
    common_config: &CommonConfig,
    readonly: bool,
    mtls_disabled: bool,
    config_store: &ConfigStore,
    is_shadow_tier: bool,
    tls_ca_path: Option<&Path>,
    rim_backend: Option<i32>,
    rim_regional_resources: bool,
) -> Result<SaplingRemoteApi, Error> {
    #[cfg(fbcode_build)]
    let rim = rim_backend
        .map(|backend| -> Result<_, Error> {
            let backend = RimBackend(backend);
            let config = crate::utils::rim_rate_limiter::workload_config(config_store)
                .context("Loading RIM workload config")?;
            anyhow::ensure!(
                config.get().allocation_backends.contains(&backend.0),
                "RIM backend must use workload allocations"
            );
            let cpu_resource = if rim_regional_resources {
                let region = FbWhoAmI::get()
                    .context("Reading CPU capacity region")?
                    .region_datacenter_prefix
                    .clone()
                    .filter(|region| !region.is_empty())
                    .context("Regional resources require a serving region")?;
                format!("cpu_{region}")
            } else {
                "cpu".to_owned()
            };
            crate::utils::rim_rate_limiter::init(backend);
            Ok(crate::utils::rim_rate_limiter::RimRateLimiter {
                backend,
                config,
                cpu_resource,
            })
        })
        .transpose()?;

    #[cfg(not(fbcode_build))]
    let _ = (
        config_store,
        is_shadow_tier,
        tls_ca_path,
        rim_backend,
        rim_regional_resources,
    );

    let ctx = ServerContext::new(mononoke, will_exit);

    let log_middleware = if test_friendly_logging {
        LogMiddleware::test_friendly()
    } else {
        LogMiddleware::tracing()
    };

    // Set up the router and handler for serving HTTP requests, along with custom middleware.
    // The middleware added here does not implement Gotham's usual Middleware trait; instead,
    // it uses the custom Middleware API defined in the gotham_ext crate. Native Gotham
    // middleware is set up during router setup in build_router.
    let router = build_router(ctx);

    let handler = MononokeHttpHandler::builder()
        .add(TlsSessionDataMiddleware::new(tls_session_data_log_path)?)
        .add(MetadataMiddleware::new(
            fb,
            common_config.internal_identity.clone(),
            ClientEntryPoint::SaplingRemoteApi,
            mtls_disabled,
            true,
        ))
        .add(ServerIdentityMiddleware::new(HeaderValue::from_static(
            "edenapi_server",
        )))
        .add(PostResponseMiddleware::default());

    // Shadow forwarder runs before RequestContext/ThrottleMiddleware so it
    // captures all requests (including rate-limited ones) for a complete
    // shadow traffic picture. This is intentional: SLAPI wants shadow
    // traffic to reflect the full request mix. (Git server intentionally
    // places it after rate limiting — see git_server/src/main.rs.)
    // Only available in fbcode builds.
    #[cfg(fbcode_build)]
    let handler = handler.add(ShadowForwarderMiddleware::new(
        config_store,
        "scm/mononoke/shadow_traffic/slapi",
        is_shadow_tier,
        tls_ca_path,
    )?);

    let handler = handler
        .add(RequestContextMiddleware::new(
            fb,
            scuba.clone(),
            rate_limiter,
            readonly,
        ))
        .add(ArtilleryMiddleware::new(fb))
        .add(RequestDumperMiddleware::new(
            fb,
            common_config.edenapi_dumper_scuba_table.clone(),
        ))
        .add(<ScubaMiddleware<SaplingRemoteApiScubaHandler>>::new(scuba))
        .add(ThrottleMiddleware::new(
            #[cfg(fbcode_build)]
            rim,
        ))
        .add(LoadMiddleware::new())
        .add(log_middleware)
        .add(OdsMiddleware::new())
        .add(TimerMiddleware::new())
        .build(router);

    Ok(handler)
}
