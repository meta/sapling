/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#[cfg(fbcode_build)]
mod facebook;
#[cfg(not(fbcode_build))]
mod oss;

#[cfg(fbcode_build)]
pub use facebook::ArtilleryTraceGuard;
#[cfg(fbcode_build)]
pub use facebook::continue_trace_from_context;
#[cfg(fbcode_build)]
pub use facebook::current_trace_id;
#[cfg(fbcode_build)]
pub use facebook::start_trace;
#[cfg(not(fbcode_build))]
pub use oss::ArtilleryTraceGuard;
#[cfg(not(fbcode_build))]
pub use oss::continue_trace_from_context;
#[cfg(not(fbcode_build))]
pub use oss::current_trace_id;
#[cfg(not(fbcode_build))]
pub use oss::start_trace;

#[cfg(test)]
mod tests {
    use fbinit::FacebookInit;
    use mononoke_macros::mononoke;

    use super::*;

    /// Kept in step with `gotham_ext`'s `ROOT_OPERATION_NAME`.
    const ROOT_OPERATION_NAME_FOR_TEST: &str = "mononoke_http_request_root";

    /// Exercises the start path. Under `fbcode_build` that goes through the
    /// Artillery cxx bridge; otherwise it hits the OSS stub. Asserts nothing
    /// about the outcome either way: sampling is the policy's decision and the
    /// policy lives in configerator, out of reach of a unit test. What is being
    /// checked is that the start path cannot be made to panic.
    #[mononoke::fbinit_test]
    fn start_trace_for_land_endpoint_is_infallible(fb: FacebookInit) {
        let _ = start_trace(fb, ROOT_OPERATION_NAME_FOR_TEST, "land_stack");
    }
}
