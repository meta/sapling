/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod observability;
mod request;
mod thrift_cache;

pub(crate) use observability::observe_request;
pub(crate) use request::RequestState;
pub(crate) use request::request_authorization_context;
pub(crate) use request::service_authorization_context;
pub(crate) use request::set_nocache_header;
pub(crate) use thrift_cache::ThriftCachePolicy;
#[cfg(test)]
pub(crate) use thrift_cache::fallback_request_primary_identity_types;
#[cfg(test)]
pub(crate) use thrift_cache::is_kcb_covered;
#[cfg(test)]
pub(crate) use thrift_cache::resolve_request_primary_identity_types;
#[cfg(test)]
pub(crate) use thrift_cache::should_set_nocache_for_identity_types;
