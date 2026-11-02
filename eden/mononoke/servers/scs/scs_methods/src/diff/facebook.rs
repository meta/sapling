/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Context;
use anyhow::Result;
use diff_service_client::DiffServiceClient;
use fbinit::FacebookInit;
use metaconfig_types::RemoteDiffConfig;

pub(super) fn create_diff_service_client(
    fb: FacebookInit,
    remote_diff_config: Option<&RemoteDiffConfig>,
    repo_name: &str,
) -> Result<DiffServiceClient> {
    match remote_diff_config {
        Some(RemoteDiffConfig::HostPort(host_port)) => {
            DiffServiceClient::from_host_port(fb, host_port.clone()).with_context(|| {
                format!("Failed to create diff service client from host:port '{host_port}'")
            })
        }
        Some(RemoteDiffConfig::SmcTier(smc_tier)) => {
            DiffServiceClient::from_tier_name(fb, smc_tier.clone()).with_context(|| {
                format!("Failed to create diff service client from SMC tier '{smc_tier}'")
            })
        }
        Some(RemoteDiffConfig::ShardManagerTier(sm_tier)) => {
            DiffServiceClient::from_sm_tier_name(fb, sm_tier.clone(), repo_name.to_string())
                .with_context(|| {
                    format!(
                        "Failed to create diff service client from ShardManager tier '{sm_tier}'"
                    )
                })
        }
        None => {
            // Fallback to default Service Manager discovery
            DiffServiceClient::new_with_sm(fb, repo_name.to_string()).with_context(|| {
                format!("Failed to create diff service client for repo '{repo_name}'")
            })
        }
    }
}
