/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use fbinit::FacebookInit;
use sharding_ext::RepoShard;
use tokio::runtime::Handle;
use tokio::sync::oneshot::Receiver;

pub mod args;
#[cfg(fbcode_build)]
mod facebook;
mod local;
#[cfg(not(fbcode_build))]
mod oss;

#[cfg(fbcode_build)]
pub use facebook::*;
pub use local::LocalShardExecutor;
#[cfg(not(fbcode_build))]
pub use oss::*;

/// Runs a process's repos as ShardManager assigns them, or as a local shard
/// file does (`--local-shardmanager-path`).
pub enum ShardedProcessExecutor {
    ShardManager(ShardManagerExecutor),
    Local(LocalShardExecutor),
}

impl ShardedProcessExecutor {
    /// An executor that ShardManager assigns repos to.
    pub fn new(
        fb: FacebookInit,
        runtime_handle: Handle,
        service_name: &'static str,
        service_scope: &'static str,
        timeout_secs: u64,
        process_handle: Arc<dyn RepoShardedProcess>,
        shard_healing: bool,
        health_check_fn: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    ) -> Result<Self> {
        Ok(Self::ShardManager(ShardManagerExecutor::new(
            fb,
            runtime_handle,
            service_name,
            service_scope,
            timeout_secs,
            process_handle,
            shard_healing,
            health_check_fn,
        )?))
    }

    /// An executor that the shard file at `path` assigns repos to.
    pub fn local(
        path: PathBuf,
        runtime_handle: Handle,
        timeout_secs: u64,
        process_handle: Arc<dyn RepoShardedProcess>,
    ) -> Self {
        Self::Local(LocalShardExecutor::new(
            path,
            runtime_handle,
            timeout_secs,
            process_handle,
        ))
    }

    /// Runs the assigned repos until the terminate signal is received.
    pub async fn block_and_execute(self, terminate_signal_receiver: Receiver<bool>) -> Result<()> {
        match self {
            Self::ShardManager(executor) => {
                executor.block_and_execute(terminate_signal_receiver).await
            }
            Self::Local(executor) => {
                executor
                    .run(async {
                        let _ = terminate_signal_receiver.await;
                    })
                    .await
            }
        }
    }
}

/// Trait outlining the method responsible for performing the initial bootstrapping
/// of the process with the context of the incoming repo. Implementer of this trait should
/// NOT contain any repo-specific state since one instance of RepoShardedProcess
/// caters all repos for the given job.
#[async_trait]
pub trait RepoShardedProcess: Send + Sync {
    /// Method responsible for performing the initial setup of the process in context of
    /// the provided repo. This method should ONLY contain code necessary to build
    /// state (in form of the struct that implements RepoShardedProcessExecutor trait)
    /// that is required to execute the job. The repo (or any related entity)
    /// should be included as part of the RepoShardedProcessExecutor state.
    async fn setup(&self, repo: &RepoShard) -> Result<Arc<dyn RepoShardedProcessExecutor>>;
}

/// Trait outlining the methods to be implemented by Mononoke jobs that require
/// sharding across different repos based on their resource requirements.
/// Implementer of this trait can store repo-specific state if required since there
/// exists a 1-1 mapping between a repo and a RepoShardedProcessExecutor.
#[async_trait]
pub trait RepoShardedProcessExecutor: Send + Sync {
    /// Callback for when the repo-process for the current job is ready to begin execution
    /// with the provided repo. The core process logic should exist within this method.
    /// The process execution can be one-shot, batched or long-running in nature.
    /// Correspondingly, the task associated with this method could return immediately,
    /// or after a while or just keep executing until the executor decides to terminate it.
    /// Depending on the execution mode, the implementer can choose to maintain some
    /// form of state (i.e. signal, oneshot, etc.) that can be used to interrupt the
    /// normal execution of the process when the Sharded Process Manager needs to move
    /// or terminate the process execution for the provided repo.
    async fn execute(&self) -> Result<()>;

    /// Callback for when the executing repo process for the current job is required to
    /// relinquish an existing repo assigned to it earlier. The timeout specified during
    /// creation determines the time for which the executing repo process will wait before
    /// performing a hard-cleanup of the repo associated task. Note that this
    /// method is responsible ONLY for signalling the termination of the execution
    /// of the repo for the given process. Once the control returns from this method,
    /// the executor waits for timeout seconds (specified during executor construction)
    /// before initiating forced termination of the executing process. If there
    /// are any book-keeping activities pending, they should be completed in the
    /// main execution method during this timeout period. The stop() method itself
    /// SHOULD return quickly (i.e. should not be long running).
    async fn stop(&self) -> Result<()>;
}
