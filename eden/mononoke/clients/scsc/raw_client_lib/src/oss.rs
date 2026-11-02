/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use oss_thrift_transport::client::connect;
use source_control_clients::make_SourceControlService;

use crate::ScsClient;

pub(crate) fn from_host_port(host_port: &str) -> anyhow::Result<ScsClient> {
    Ok(ScsClient::new(
        connect::<make_SourceControlService>(host_port)?,
        None,
    ))
}
