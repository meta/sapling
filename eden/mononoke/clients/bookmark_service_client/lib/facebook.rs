/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod client;

pub(crate) const DEFAULT_TIER: &str = "mononoke.bookmark_service";

pub(crate) use client::Client;
pub(crate) use client::StatusClient;
pub(crate) use client::from_host_port;
pub(crate) use client::get_service_client;
pub(crate) use client::status;
