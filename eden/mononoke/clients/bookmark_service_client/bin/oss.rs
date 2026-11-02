/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use anyhow::anyhow;
use bookmark_service_client::BookmarkServiceClient;
use fbinit::FacebookInit;

use crate::BookmarkServiceClientArgs;

pub(crate) fn setup(_fb: FacebookInit) {}

pub(crate) fn connect(
    fb: FacebookInit,
    args: &BookmarkServiceClientArgs,
) -> Result<BookmarkServiceClient> {
    BookmarkServiceClient::from_host_port(
        fb,
        args.host
            .clone()
            .ok_or_else(|| anyhow!("--host is required in OSS mode"))?,
    )
}
