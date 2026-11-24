/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(feature = "fb")]
mod fb;
#[cfg(not(feature = "fb"))]
mod oss;
#[cfg(feature = "fb")]
pub use fb::remote_loader::get_remote_configs;
#[cfg(feature = "fb")]
pub use fb::remote_loader::maybe_set_http_cat_header;
#[cfg(feature = "fb")]
pub use fb::remote_loader::read_local_config;
#[cfg(feature = "fb")]
pub use fb::remote_loader::resolve_client_identity;
#[cfg(feature = "fb")]
pub use fb::remote_loader::should_fetch_configs;
#[cfg(not(feature = "fb"))]
pub use oss::get_remote_configs;
#[cfg(not(feature = "fb"))]
pub use oss::read_local_config;
#[cfg(not(feature = "fb"))]
pub use oss::should_fetch_configs;
