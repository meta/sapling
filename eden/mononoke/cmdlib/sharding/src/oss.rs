/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Result;
use anyhow::bail;
use fbinit::FacebookInit;
use tokio::runtime::Handle;
use tokio::sync::oneshot::Receiver;

use crate::RepoShardedProcess;

/// ShardManager isn't available outside fbcode builds, so none can be made.
pub enum ShardManagerExecutor {}

impl ShardManagerExecutor {
    pub fn new(
        _fb: FacebookInit,
        _runtime_handle: Handle,
        _service_name: &'static str,
        _service_scope: &'static str,
        _timeout_secs: u64,
        _bp_handle: Arc<dyn RepoShardedProcess>,
        _shard_healing: bool,
        _health_check_fn: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    ) -> Result<Self> {
        bail!("ShardManager is not available in this build; use --local-shardmanager-path")
    }

    pub async fn block_and_execute(self, _terminate_signal_receiver: Receiver<bool>) -> Result<()> {
        match self {}
    }
}
