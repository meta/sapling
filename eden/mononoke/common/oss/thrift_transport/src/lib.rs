/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Loopback-only Thrift transport for OSS builds of Mononoke's services.
//!
//! **This is a reference transport, not a production one.** It lets
//! Mononoke's Thrift servers and clients build and run outside fbcode for
//! local development, testing and demonstration. It has no TLS, no
//! authentication, no authorization and no rate limiting, and it makes no
//! attempt to withstand a hostile or merely buggy peer. Both ends resolve
//! their address once, refuse it unless every result is a loopback address,
//! and only ever bind or connect to the addresses they checked: that is the
//! only thing keeping it off the network. Deploying Mononoke outside fbcode
//! needs a real transport in its place.
//!
//! On the wire, every message is a frame: a big-endian `u32` length followed
//! by that many bytes. A request is one frame holding a serialized Thrift
//! message. A unary reply is one frame. A streaming reply is the initial
//! response frame followed by stream frames, each tagged with a one-byte
//! kind, and a successful stream ends with an explicit end-of-stream frame.
//!
//! Known limits, acceptable for a reference transport:
//!
//! - Clients open one connection per call and have no timeouts.
//! - A frame's buffer is allocated from its length header, so each
//!   connection may hold up to [`MAX_FRAME_SIZE`] bytes.
//! - Sink, bidirectional and interaction RPCs are refused.

pub mod client;
mod framing;
mod loopback;
pub mod server;

pub use framing::MAX_FRAME_SIZE;
