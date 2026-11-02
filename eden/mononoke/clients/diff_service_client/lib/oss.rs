/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use context::CoreContext;
use diff_service_if_clients::DiffService;
use diff_service_if_clients::make_DiffService;
use fb303_core::fb303_status;
use fbinit::FacebookInit;
use oss_thrift_transport::client::ClientTransport;
use oss_thrift_transport::client::connect;

pub(crate) type Client = Arc<dyn DiffService + Send + Sync>;

/// The OSS transport has no `Status` interface, so a server is alive if it
/// accepts connections.
pub(crate) type StatusClient = ClientTransport;

pub(crate) fn from_host_port(
    _fb: FacebookInit,
    host_port: String,
) -> Result<(Client, StatusClient)> {
    Ok((
        connect::<make_DiffService>(&host_port)?,
        ClientTransport::new(&host_port)?,
    ))
}

pub(crate) fn get_service_client(
    client: &Client,
    _ctx: Option<&CoreContext>,
) -> Result<Arc<dyn DiffService + Send + Sync>> {
    Ok(client.clone())
}

pub(crate) async fn status(transport: &StatusClient) -> Result<fb303_status> {
    transport
        .probe()
        .await
        .context("diff service is not accepting connections")?;
    Ok(fb303_status::ALIVE)
}
