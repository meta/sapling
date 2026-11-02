/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use anyhow::anyhow;
use diff_service_client::DiffServiceClient;
use fbinit::FacebookInit;

use crate::DiffServiceClientArgs;

pub(crate) fn setup(_fb: FacebookInit) {}

pub(crate) fn connect(fb: FacebookInit, args: &DiffServiceClientArgs) -> Result<DiffServiceClient> {
    DiffServiceClient::from_host_port(
        fb,
        args.host
            .clone()
            .ok_or_else(|| anyhow!("--host is required in OSS mode"))?,
    )
}
