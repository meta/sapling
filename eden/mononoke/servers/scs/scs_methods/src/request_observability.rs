/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#[derive(Clone, Copy)]
pub(crate) enum RequestType {
    Normal,
    Stream,
}

#[derive(Clone, Copy)]
pub(crate) enum RequestEvent {
    Complete,
    StreamStarted,
    StreamChunk,
    StreamComplete,
    Cancelled,
}

#[derive(Clone, Copy)]
pub(crate) enum RequestOutcome {
    Success,
    RequestError,
    InternalError,
    OverloadError,
    PollError,
}
