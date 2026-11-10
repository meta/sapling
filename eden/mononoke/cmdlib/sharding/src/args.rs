/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Args;
use fbinit::FacebookInit;
use tokio::runtime::Handle;

use crate::RepoShardedProcess;
use crate::ShardedProcessExecutor;

/// Command line arguments for sharded executor
#[derive(Clone, Args, Debug)]
pub struct ShardedExecutorArgs {
    /// The name of the ShardManager service corresponding to this service's region.
    /// If this argument isn't provided, the service will operate in non-sharded mode.
    #[clap(long, requires = "sharded_scope_name")]
    pub sharded_service_name: Option<String>,
    /// The scope of the ShardManager service that this service corresponds to.
    #[clap(long, requires = "sharded_service_name")]
    pub sharded_scope_name: Option<String>,
    /// Serve the repos this file lists, one shard id per line, as if
    /// ShardManager assigned them; for tests and local deployments. Changes
    /// are applied within a second. `<path>.active` lists the shards that are
    /// set up, and `<path>.errors` those whose setup or execution failed.
    #[clap(long, conflicts_with = "sharded_service_name")]
    pub local_shardmanager_path: Option<PathBuf>,
}

impl ShardedExecutorArgs {
    /// Whether ShardManager or a local shard file assigns this service's repos.
    pub fn is_sharded(&self) -> bool {
        self.sharded_service_name.is_some() || self.local_shardmanager_path.is_some()
    }

    pub fn build_executor(
        self,
        fb: FacebookInit,
        runtime: Handle,
        process_fn: impl FnOnce() -> Arc<dyn RepoShardedProcess>,
        shard_healing: bool,
        cleanup_timeout_secs: u64,
    ) -> Result<Option<ShardedProcessExecutor>> {
        self.build_executor_with_health_check(
            fb,
            runtime,
            process_fn,
            shard_healing,
            cleanup_timeout_secs,
            None,
        )
    }

    pub fn build_executor_with_health_check(
        self,
        fb: FacebookInit,
        runtime: Handle,
        process_fn: impl FnOnce() -> Arc<dyn RepoShardedProcess>,
        shard_healing: bool,
        cleanup_timeout_secs: u64,
        health_check_fn: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    ) -> Result<Option<ShardedProcessExecutor>> {
        if let Some(path) = self.local_shardmanager_path {
            return Ok(Some(ShardedProcessExecutor::local(
                path,
                runtime,
                cleanup_timeout_secs,
                process_fn(),
            )));
        }
        if let Some((sharded_service_name, sharded_scope_name)) =
            self.sharded_service_name.zip(self.sharded_scope_name)
        {
            let process = process_fn();
            Ok(Some(ShardedProcessExecutor::new(
                fb,
                runtime,
                // The service name & scope needs to be 'static to satisfy SM contract
                Box::leak(Box::new(sharded_service_name)),
                Box::leak(Box::new(sharded_scope_name)),
                cleanup_timeout_secs,
                process,
                shard_healing,
                health_check_fn,
            )?))
        } else {
            Ok(None)
        }
    }
}
