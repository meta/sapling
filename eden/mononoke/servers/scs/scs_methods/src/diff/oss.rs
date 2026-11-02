/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use anyhow::bail;
use diff_service_client::DiffServiceClient;
use fbinit::FacebookInit;
use metaconfig_types::RemoteDiffConfig;

pub(super) fn create_diff_service_client(
    fb: FacebookInit,
    remote_diff_config: Option<&RemoteDiffConfig>,
    _repo_name: &str,
) -> Result<DiffServiceClient> {
    match remote_diff_config {
        Some(RemoteDiffConfig::HostPort(host_port)) => {
            DiffServiceClient::from_host_port(fb, host_port.clone())
        }
        Some(RemoteDiffConfig::SmcTier(_)) => {
            bail!("SMC diff-service addresses are unavailable in OSS builds")
        }
        Some(RemoteDiffConfig::ShardManagerTier(_)) => {
            bail!("ShardManager diff-service addresses are unavailable in OSS builds")
        }
        None => bail!("OSS remote diff requires an explicit host:port address"),
    }
}
