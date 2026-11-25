/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use fbinit::FacebookInit;
use tracing::Subscriber;
use tracing_subscriber::Layer;
use tracing_subscriber::registry::LookupSpan;

/// Mirrors `tracing_artillery::action::LOG_TO_REQUEST_BLOCK`, which is not
/// built in OSS. Nothing consumes the directive there.
pub const LOG_TO_REQUEST_BLOCK: &str = "log_to_request_block";

pub struct ArtilleryTraceGuard;

pub fn tracing_layer<S>(_fb: FacebookInit) -> Option<Box<dyn Layer<S> + Send + Sync + 'static>>
where
    S: Subscriber + for<'a> LookupSpan<'a> + Send + Sync,
{
    None
}

pub fn continue_trace_from_context(
    _fb: FacebookInit,
    _context_bytes: &[u8],
) -> Option<ArtilleryTraceGuard> {
    None
}

pub fn start_trace(
    _fb: FacebookInit,
    _operation_name: &'static str,
    _endpoint: &'static str,
) -> Option<ArtilleryTraceGuard> {
    None
}

pub fn current_trace_id() -> Option<String> {
    None
}
