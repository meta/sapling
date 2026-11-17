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
use artillery_http_ext::start_trace;
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

/// Operation name for traces rooted here rather than continued. Distinct by
/// design: it marks requests that arrived with no inbound context.
const ROOT_OPERATION_NAME: &str = "mononoke_http_request_root";

#[derive(StateData)]
pub(crate) struct ArtilleryTracingEnabled(pub(crate) bool);

/// Endpoint label chosen by the server's `RootTraceSelector`, or `None` if this
/// request was not selected for rooting. Bound to the `mononoke_endpoint` Luna
/// sample key so the policy scopes and rates roots per endpoint.
#[derive(StateData)]
pub(crate) struct ArtilleryRootEndpoint(pub(crate) Option<&'static str>);

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

/// Middleware that continues an inbound Artillery trace, and — when
/// `scm/artillery:root_trace_when_absent` is on — roots a fresh one for
/// requests that arrive without inbound context.
pub struct ArtilleryMiddleware {
    fb: FacebookInit,
}

impl ArtilleryMiddleware {
    pub fn new(fb: FacebookInit) -> Self {
        Self { fb }
    }

    /// Continue the inbound trace if there is one. `None` covers all the ways
    /// there is nothing to continue — no header, a malformed header, or a
    /// decode that produced no usable context — so the caller roots instead.
    fn continued_trace(&self, state: &State) -> Option<ArtilleryTraceGuard> {
        let encoded = HeaderMap::try_borrow_from(state)
            .and_then(|headers| headers.get(ARTILLERY_TRACE_IDS_HEADER))
            .and_then(|value| value.to_str().ok())?;

        let context_bytes = match STANDARD.decode(encoded.as_bytes()) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(error = %e, "Artillery: ignoring malformed APC trace header");
                return None;
            }
        };

        let guard = continue_trace_from_context(self.fb, &context_bytes);
        if guard.is_none() {
            tracing::warn!("Artillery: APC header decoded but trace continuation failed");
        }
        guard
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

        // Continuation wins: rooting over an inbound trace would sever it. It
        // also bypasses the policy, so an inbound trace is continued whatever
        // the policy says about rooting.
        let root_endpoint = ArtilleryRootEndpoint::try_borrow_from(state).and_then(|e| e.0);
        let guard = self
            .continued_trace(state)
            .or_else(|| start_trace(self.fb, ROOT_OPERATION_NAME, root_endpoint?));

        if let Some(guard) = guard {
            state.put(ArtilleryTraceGuardState(guard));
        }

        None
    }
}
