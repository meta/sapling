/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![cfg_attr(fbcode_build, feature(string_into_chars))]

mod async_requests;
mod commit_id;
mod diff;
#[cfg(fbcode_build)]
mod facebook;
pub mod from_request;
mod history;
mod into_response;
mod methods;
#[cfg(not(fbcode_build))]
mod oss;
mod request_observability;
mod scuba_params;
mod scuba_response;
pub mod source_control_impl;
pub mod specifiers;

#[cfg(fbcode_build)]
pub(crate) use facebook as platform;
pub use methods::commit_sparse_profile_info;
#[cfg(not(fbcode_build))]
pub(crate) use oss as platform;
