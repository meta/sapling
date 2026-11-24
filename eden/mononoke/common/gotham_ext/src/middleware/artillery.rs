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
#[cfg(fbcode_build)]
use context_prop_const::ContextpropCore::ArtilleryPropagatedContext;
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

    /// Continue the inbound trace from the decoded header. `None` means it held
    /// no usable context, so the caller roots instead.
    fn continued_trace(&self, context_bytes: &[u8]) -> Option<ArtilleryTraceGuard> {
        let guard = continue_trace_from_context(self.fb, context_bytes);
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

        let context_bytes = HeaderMap::try_borrow_from(state)
            .and_then(|headers| headers.get(ARTILLERY_TRACE_IDS_HEADER))
            .and_then(|value| value.to_str().ok())
            .and_then(|encoded| {
                STANDARD
                    .decode(encoded)
                    .inspect_err(|e| {
                        tracing::warn!(error = %e, "Artillery: ignoring malformed APC trace header")
                    })
                    .ok()
            });

        // Continuation wins: rooting over an inbound trace would sever it. It
        // also bypasses the policy, so an inbound trace is continued whatever
        // the policy says about rooting.
        let root_endpoint = ArtilleryRootEndpoint::try_borrow_from(state).and_then(|e| e.0);
        let guard = context_bytes
            .as_deref()
            .and_then(|bytes| self.continued_trace(bytes))
            .or_else(|| start_trace(self.fb, ROOT_OPERATION_NAME, root_endpoint?));

        #[cfg(fbcode_build)]
        if let Some(bytes) = &context_bytes
            && justknobs::eval(
                "scm/artillery:handle_http_requests_with_rctx",
                None,
                Some("bridge_agent_id"),
            )
            && let Ok(context) =
                fbthrift::compact_protocol::deserialize::<ArtilleryPropagatedContext, _, _>(
                    bytes.as_slice(),
                )
            && let Some(agent_id) = context.agent_id.filter(|id| (1..=256).contains(&id.len()))
        {
            request_data::agent_id::set_if_absent(&agent_id);
        }

        if let Some(guard) = guard {
            state.put(ArtilleryTraceGuardState(guard));
        }

        None
    }
}

#[cfg(all(test, fbcode_build))]
mod tests {
    use std::collections::HashMap;
    use std::pin::Pin;

    use artillery_http_ext::current_trace_id;
    use context_prop_const::ContextpropCore::ArtilleryPropagatedContextType;
    use futures::FutureExt;
    use futures::executor::block_on;
    use futures::future;
    use gotham::handler::Handler;
    use gotham::handler::HandlerFuture;
    use http::HeaderValue;
    use http::Request;
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::with_just_knobs;
    use mononoke_macros::mononoke;
    use request_context_ext::with_fresh_request_context;

    use super::*;
    use crate::handler::MononokeHttpHandler;
    use crate::handler::MononokeHttpHandlerBuilder;

    const AGENT: &str = "AGENT:claude_code";

    #[derive(Clone)]
    struct Echo;

    impl Handler for Echo {
        fn handle(self, state: State) -> Pin<Box<HandlerFuture>> {
            let mut response = Response::new(Body::default());
            if let Some(agent_id) = request_data::agent_id::get() {
                let value = agent_id.parse().expect("agent id should be a header value");
                response.headers_mut().insert("agent-id", value);
            }
            if current_trace_id().is_some() {
                let value = HeaderValue::from_static("1");
                response.headers_mut().insert("traced", value);
            }
            future::ready(Ok((state, response))).boxed()
        }
    }

    struct PresetAgentId;

    #[async_trait::async_trait]
    impl Middleware for PresetAgentId {
        async fn inbound(&self, _state: &mut State) -> Option<Response<Body>> {
            request_data::agent_id::set("AGENT:attested");
            None
        }
    }

    fn apc(agent_id: Option<&str>) -> String {
        let context = ArtilleryPropagatedContext {
            trace_id: 1,
            edge_id: 2,
            r#type: ArtilleryPropagatedContextType::Continuation,
            agent_id: agent_id.map(str::to_owned),
            ..Default::default()
        };
        STANDARD.encode(fbthrift::compact_protocol::serialize(&context))
    }

    fn state(header: Option<&str>) -> State {
        let mut request = Request::new(Body::default());
        if let Some(header) = header {
            let value = header
                .parse()
                .expect("test header should be a header value");
            request
                .headers_mut()
                .insert(ARTILLERY_TRACE_IDS_HEADER, value);
        }
        State::from_request(request, "127.0.0.1:0".parse().expect("valid address"))
    }

    fn knobs(on: bool) -> JustKnobsInMemory {
        JustKnobsInMemory::new(HashMap::from([(
            "scm/artillery:handle_http_requests_with_rctx".to_owned(),
            KnobVal::Bool(on),
        )]))
    }

    fn handle(
        fb: FacebookInit,
        header: Option<&str>,
        builder: MononokeHttpHandlerBuilder,
    ) -> (Option<String>, bool) {
        let handler = builder.add(ArtilleryMiddleware::new(fb)).build(Echo);
        let Ok((_, response)) =
            with_just_knobs(knobs(true), || block_on(handler.handle(state(header))))
        else {
            panic!("handler should succeed");
        };
        let agent_id = response
            .headers()
            .get("agent-id")
            .map(|value| value.to_str().expect("agent id should be ASCII").to_owned());
        (agent_id, response.headers().contains_key("traced"))
    }

    #[mononoke::fbinit_test]
    fn agent_id_is_set_only_when_1_to_256_bytes(fb: FacebookInit) {
        let cases = [
            (None, None),
            (Some(apc(None)), None),
            (Some(apc(Some(""))), None),
            (Some(apc(Some("a"))), Some("a".to_owned())),
            (Some(apc(Some(&"a".repeat(256)))), Some("a".repeat(256))),
            (Some(apc(Some(&"a".repeat(257)))), None),
            (Some("%".to_owned()), None),
            (Some(STANDARD.encode([0xffu8])), None),
        ];
        for (header, expected) in cases {
            let builder = MononokeHttpHandler::builder();
            assert_eq!(handle(fb, header.as_deref(), builder).0, expected);
        }
    }

    #[mononoke::fbinit_test]
    fn existing_agent_id_is_not_overwritten(fb: FacebookInit) {
        let builder = MononokeHttpHandler::builder().add(PresetAgentId);
        let (agent_id, _) = handle(fb, Some(&apc(Some(AGENT))), builder);
        assert_eq!(agent_id.as_deref(), Some("AGENT:attested"));
    }

    #[mononoke::fbinit_test]
    fn bridge_gate_off_leaves_agent_id_unset(fb: FacebookInit) {
        let bridged = |tracing, on| {
            let mut s = state(Some(&apc(Some(AGENT))));
            s.put(ArtilleryTracingEnabled(tracing));
            with_just_knobs(knobs(on), || {
                block_on(with_fresh_request_context(async move {
                    ArtilleryMiddleware::new(fb).inbound(&mut s).await;
                    request_data::agent_id::get()
                }))
            })
        };
        assert_eq!(bridged(true, true).as_deref(), Some(AGENT));
        assert_eq!(bridged(true, false), None);
        assert_eq!(bridged(false, true), None);
    }

    #[mononoke::fbinit_test]
    fn agent_id_is_set_for_its_request_only(fb: FacebookInit) {
        let uncontinuable = ArtilleryPropagatedContext {
            agent_id: Some(AGENT.to_owned()),
            ..Default::default()
        };
        let uncontinuable = STANDARD.encode(fbthrift::compact_protocol::serialize(&uncontinuable));
        for (header, traced) in [(apc(Some(AGENT)), true), (uncontinuable, false)] {
            let seen = handle(fb, Some(&header), MononokeHttpHandler::builder());
            assert_eq!(seen, (Some(AGENT.to_owned()), traced));
            assert_eq!(request_data::agent_id::get(), None);
        }
    }
}
