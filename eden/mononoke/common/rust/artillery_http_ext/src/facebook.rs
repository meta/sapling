/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::OnceLock;

use fbinit::FacebookInit;
use trace_context::ObservabilityFlags;
use trace_context::TraceContextStore;
use trace_objects::Block;
use trace_objects::Timestamp;
use tracer::ManagedTraceScopeGuard;
use tracer::SampleValue;
use tracer::TraceStartParams;
use tracer::Tracer;
use tracer::deserialize_artillery_propagated_context;
use tracer::sample_key_from_name;
use tracing::Subscriber;
use tracing_artillery::ArtillerySubscriberBuilder;
use tracing_artillery::ObsFlagsFilter;
pub use tracing_artillery::action::LOG_TO_REQUEST_BLOCK;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::registry::LookupSpan;

use crate::TRACING_TARGET;

const ARTILLERY_POLICY: &str = "mononoke_scm";

/// Luna sample key carrying the endpoint that is starting the trace, so the
/// policy can scope and rate roots per endpoint without a code change.
const LUNA_KEY_ENDPOINT: &str = "mononoke_endpoint";

/// `sample_key_from_name` interns under a global write lock on first call for
/// a name, so resolve once per process rather than per request.
fn endpoint_sample_key() -> i64 {
    static KEY: OnceLock<i64> = OnceLock::new();
    *KEY.get_or_init(|| sample_key_from_name(LUNA_KEY_ENDPOINT))
}

/// Building a tracer reads the policy out of Configerator synchronously, so it
/// must happen once at first use rather than per request — `start_trace` is on
/// the path of every request that arrives without inbound context, which is
/// nearly all of them. This is the Rust SDK's own guidance: `ArtilleryLayer`
/// exists so that `continue_trace_with` can take a tracer "built once, by the
/// caller" instead of calling `Tracer::new` per request.
fn tracer(fb: FacebookInit) -> &'static Tracer {
    static TRACER: OnceLock<Tracer> = OnceLock::new();
    TRACER.get_or_init(|| Tracer::new(fb, ARTILLERY_POLICY))
}

pub struct ArtilleryTraceGuard {
    #[expect(
        dead_code,
        reason = "RAII guard — held for its Drop impl to keep the Artillery trace active for the request lifetime"
    )]
    trace: ManagedTraceScopeGuard,

    /// Ended on drop rather than left to the SDK, which ends it only once the
    /// last `TraceContext` reference goes. Spawned tasks inherit that context,
    /// so background work outliving the request would hold the block open and
    /// the trace would be built with its end equal to its start.
    request_block: Option<Block>,
}

impl ArtilleryTraceGuard {
    /// Must be called on the thread that started or continued the trace, while
    /// its context is still current.
    fn new(fb: FacebookInit, trace: ManagedTraceScopeGuard) -> Self {
        let request_block = TraceContextStore::get_default(fb)
            .get_current_trace_context()
            .and_then(|context| context.get_request_block().upgrade());

        Self {
            trace,
            request_block,
        }
    }
}

impl Drop for ArtilleryTraceGuard {
    fn drop(&mut self) {
        if let Some(block) = &self.request_block {
            block.end_block(Timestamp::now());
        }
    }
}

/// Continue an inbound Artillery trace from the `propagated-artillery-trace-ids`
/// HTTP header, if present. Returned guard MUST be held for the duration of the request.
pub fn continue_trace_from_context(
    fb: FacebookInit,
    context_bytes: &[u8],
) -> Option<ArtilleryTraceGuard> {
    let ctx = deserialize_artillery_propagated_context(context_bytes)?;
    tracer(fb)
        .continue_managed_trace(&ctx)
        .ok()
        .map(|trace| ArtilleryTraceGuard::new(fb, trace))
}

/// Start a fresh Artillery trace for a request that arrived without inbound
/// context. Returned guard MUST be held for the duration of the request.
///
/// `endpoint` is bound to the `mononoke_endpoint` Luna sample key, so rates are
/// the policy's decision; `None` means it declined.
pub fn start_trace(
    fb: FacebookInit,
    operation_name: &'static str,
    endpoint: &'static str,
) -> Option<ArtilleryTraceGuard> {
    let key = endpoint_sample_key();
    let mut params = TraceStartParams::new();
    params.with_operation_name(operation_name);
    params.with_luna_sample_value(move |k| {
        if k == key {
            SampleValue::String(endpoint.to_owned())
        } else {
            SampleValue::None
        }
    });
    tracer(fb)
        .start_managed_trace(&params)
        .ok()
        .map(|trace| ArtilleryTraceGuard::new(fb, trace))
}

/// Scoped to [`TRACING_TARGET`]: an unscoped layer would put every span in the
/// process onto the trace, against the `mononoke_scm` byte budget.
pub fn tracing_layer<S>(fb: FacebookInit) -> Option<Box<dyn Layer<S> + Send + Sync + 'static>>
where
    S: Subscriber + for<'a> LookupSpan<'a> + Send + Sync,
{
    let layer = ArtillerySubscriberBuilder::new(fb)
        .build()
        .with_filter(ObsFlagsFilter::new(
            fb,
            ObservabilityFlags::ARTILLERY_TRACING_CUSTOM_INSTRUMENTATION,
        ))
        .with_filter(Targets::new().with_target(TRACING_TARGET, tracing::Level::INFO));

    Some(Box::new(layer))
}

pub fn current_trace_id() -> Option<String> {
    let trace_id = service_tracing::get_trace_id();
    if trace_id.is_empty() {
        None
    } else {
        Some(trace_id)
    }
}

// Tests live in `lib.rs` rather than here: this module is behind
// `#[cfg(fbcode_build)]`, so a test placed here does not exist in every build
// configuration, and its test-only deps read as unused in the ones where the
// module is compiled out.
