/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::env;
use std::sync::OnceLock;

use artillery_http_ext::ArtilleryTraceGuard;
use artillery_http_ext::continue_trace_from_context;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fbinit::FacebookInit;
use gotham::helpers::http::Body;
use gotham::state::FromState;
use gotham::state::State;
use gotham_derive::StateData;
use http::HeaderMap;
use http::Response;

use crate::middleware::Middleware;

/// Inbound HTTP header carrying a base64(compact-thrift) `ArtilleryPropagatedContext`.
pub(crate) const ARTILLERY_TRACE_IDS_HEADER: &str = "propagated-artillery-trace-ids";

fn tw_job_name() -> Option<&'static str> {
    static TW_JOB_NAME: OnceLock<Option<String>> = OnceLock::new();
    TW_JOB_NAME
        .get_or_init(|| env::var("TW_JOB_NAME").ok())
        .as_deref()
}

pub(crate) fn artillery_http_tracing_enabled() -> bool {
    justknobs::eval(
        "scm/artillery:handle_http_requests_with_rctx",
        None,
        tw_job_name(),
    )
}

#[derive(StateData)]
pub(crate) struct ArtilleryTracingEnabled(pub(crate) bool);

/// Holds the continued-trace guard for the lifetime of the request.
#[derive(StateData)]
struct ArtilleryTraceGuardState(
    #[cfg_attr(
        fbcode_build,
        expect(
            dead_code,
            reason = "RAII guard — held for its Drop impl to keep the Artillery trace alive for the request lifetime"
        )
    )]
    ArtilleryTraceGuard,
);

/// Middleware that continues an inbound Artillery trace.
pub struct ArtilleryMiddleware {
    fb: FacebookInit,
}

impl ArtilleryMiddleware {
    pub fn new(fb: FacebookInit) -> Self {
        Self { fb }
    }
}

#[async_trait::async_trait]
impl Middleware for ArtilleryMiddleware {
    async fn inbound(&self, state: &mut State) -> Option<Response<Body>> {
        let enabled = ArtilleryTracingEnabled::try_borrow_from(state)
            .map(|e| e.0)
            .unwrap_or(false);
        if !enabled {
            return None;
        }

        let encoded = HeaderMap::try_borrow_from(state)
            .and_then(|headers| headers.get(ARTILLERY_TRACE_IDS_HEADER))
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)?;

        let context_bytes = match STANDARD.decode(encoded.as_bytes()) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(error = %e, "Artillery: ignoring malformed APC trace header");
                return None;
            }
        };

        if let Some(guard) = continue_trace_from_context(self.fb, &context_bytes) {
            state.put(ArtilleryTraceGuardState(guard));
        } else {
            tracing::warn!("Artillery: APC header decoded but trace continuation failed");
        }

        None
    }
}
