/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![feature(string_into_chars)]

mod async_requests;
mod commit_id;
#[cfg(fbcode_build)]
mod diff;
#[cfg(fbcode_build)]
mod facebook;
pub mod from_request;
mod history;
mod into_response;
#[cfg(fbcode_build)]
mod methods;
#[cfg(fbcode_build)]
mod request_observability;
#[cfg(fbcode_build)]
mod scuba_params;
#[cfg(fbcode_build)]
mod scuba_response;
#[cfg(fbcode_build)]
pub mod source_control_impl;
pub mod specifiers;

#[cfg(fbcode_build)]
pub(crate) use facebook as platform;
#[cfg(fbcode_build)]
pub use methods::commit_sparse_profile_info;
