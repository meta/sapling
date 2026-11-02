/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod land_service;

pub(crate) use land_service::LandServiceClient;
pub use land_service::override_certificate_paths;
