/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use bookmarks_types::BookmarkKey;
use context::CoreContext;
use land_service_if_clients::LandService;
use land_service_if_clients::make_LandService;
use metaconfig_types::RepoConfigRef;
use oss_thrift_transport::client::connect;

pub(crate) struct LandServiceClient {
    client: Arc<dyn LandService + Send + Sync>,
}

impl LandServiceClient {
    pub(crate) async fn from_tier(
        _ctx: &CoreContext,
        tier: String,
        _repo: &impl RepoConfigRef,
    ) -> Result<Self> {
        bail!(
            "land service tier address `{tier}` is unavailable in OSS builds; configure a loopback host:port address"
        )
    }

    pub(crate) fn from_host_port(_ctx: &CoreContext, host_port: String) -> Result<Self> {
        Ok(Self {
            client: connect::<make_LandService>(&host_port)
                .with_context(|| format!("failed to create land service client for {host_port}"))?,
        })
    }

    pub(crate) fn get_service_client(
        &self,
        _ctx: &CoreContext,
        _repo: &str,
        _bookmark: &BookmarkKey,
    ) -> Result<Arc<dyn LandService + Send + Sync>> {
        Ok(self.client.clone())
    }
}
