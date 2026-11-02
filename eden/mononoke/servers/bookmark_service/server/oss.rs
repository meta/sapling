/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use metadata::Metadata;
use mononoke_app::MononokeApp;
use mononoke_thrift_server::RequestContext;
use mononoke_thrift_server::ThriftServerBuilder;
use scuba_ext::MononokeScubaSampleBuilder;

use crate::errors::ThriftError;

pub(crate) fn create_metadata(_req_ctxt: &RequestContext) -> Result<Metadata, ThriftError> {
    Ok(Metadata::default())
}

pub(crate) fn add_scuba_params(
    _req_ctxt: &RequestContext,
    _scuba: &mut MononokeScubaSampleBuilder,
) -> Result<(), ThriftError> {
    Ok(())
}

pub(crate) fn configure_server(
    builder: ThriftServerBuilder,
    _app: &MononokeApp,
) -> ThriftServerBuilder {
    builder
}
