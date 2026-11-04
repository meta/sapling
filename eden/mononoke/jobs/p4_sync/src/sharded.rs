/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use async_trait::async_trait;
use executor_lib::RepoShardedProcess;
use executor_lib::RepoShardedProcessExecutor;
use executor_lib::args::ShardedExecutorArgs;
use mononoke_app::MononokeApp;
use mononoke_app::args::RepoArgs;
use repo_identity::RepoIdentityRef;
use sharding_ext::RepoShard;

use crate::Repo;

const SM_CLEANUP_TIMEOUT_SECS: u64 = 120;
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(10);

pub(crate) struct Settings {
    pub(crate) exit_file: PathBuf,
    pub(crate) depots: HashMap<String, String>,
}

fn exit_file_present(exit_file: &Path) -> bool {
    exit_file.exists()
}

pub(crate) struct P4SyncProcess {
    app: Arc<MononokeApp>,
    settings: Settings,
}

#[async_trait]
impl RepoShardedProcess for P4SyncProcess {
    async fn setup(&self, shard: &RepoShard) -> Result<Arc<dyn RepoShardedProcessExecutor>> {
        if exit_file_present(&self.settings.exit_file) {
            tracing::warn!("Exit file detected, refusing shard {}", shard);
            return Err(anyhow!("Exit file detected, refusing shard {shard}"));
        }
        if !self.settings.depots.contains_key(&shard.repo_name) {
            tracing::warn!(
                "No --repo-depot for {}: holding its shard, importing nothing",
                shard.repo_name
            );
        }

        let repo_args = RepoArgs::from_repo_name(shard.repo_name.clone());
        let repo: Repo = self.app.open_repo(&repo_args).await?;

        Ok(Arc::new(P4SyncProcessExecutor {
            repo,
            exit_file: self.settings.exit_file.clone(),
            cancellation_requested: Arc::new(AtomicBool::new(false)),
        }))
    }
}

pub(crate) struct P4SyncProcessExecutor {
    repo: Repo,
    exit_file: PathBuf,
    cancellation_requested: Arc<AtomicBool>,
}

#[async_trait]
impl RepoShardedProcessExecutor for P4SyncProcessExecutor {
    async fn execute(&self) -> Result<()> {
        let identity = self.repo.repo_identity();
        tracing::info!(
            "p4_sync skeleton holding shard for repo {} (id {})",
            identity.name(),
            identity.id()
        );

        while !self.cancellation_requested.load(Ordering::Relaxed) {
            if exit_file_present(&self.exit_file) {
                tracing::info!(
                    "Exit file detected, releasing shard for repo {}",
                    identity.name()
                );
                break;
            }
            tokio::time::sleep(IDLE_POLL_INTERVAL).await;
        }

        tracing::info!(
            "p4_sync skeleton released shard for repo {}",
            identity.name()
        );
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.cancellation_requested.store(true, Ordering::Relaxed);
        Ok(())
    }
}

pub(crate) async fn run(
    app: MononokeApp,
    sharded_executor_args: ShardedExecutorArgs,
    settings: Settings,
) -> Result<()> {
    let process = Arc::new(P4SyncProcess {
        app: Arc::new(app),
        settings,
    });

    let Some(executor) = sharded_executor_args.build_executor(
        process.app.fb,
        process.app.runtime().clone(),
        || process.clone(),
        true, // enable shard (repo) level healing
        SM_CLEANUP_TIMEOUT_SECS,
    )?
    else {
        bail!("p4_sync can only run sharded: pass --sharded-service-name and --sharded-scope-name")
    };

    tracing::info!("Running sharded p4_sync loop");
    let (_sender, receiver) = tokio::sync::oneshot::channel::<bool>();
    executor.block_and_execute(receiver).await
}
