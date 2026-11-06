/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use async_trait::async_trait;
use clap::Parser;
use executor_lib::RepoShardedProcess;
use executor_lib::RepoShardedProcessExecutor;
use mononoke_app::MononokeApp;
use mononoke_app::args::SourceRepoArgs;
use sharding_ext::RepoShard;

use crate::ModernSyncArgs;
use crate::Repo;
use crate::sync::ExecutionType;

const SM_CLEANUP_TIMEOUT_SECS: u64 = 120;

/// Replays bookmark's moves
#[derive(Parser)]
pub struct CommandArgs {
    #[clap(long = "start-id", help = "Start id for the sync [default: 0]")]
    start_id: Option<u64>,
    #[clap(long, help = "Print sent items without actually syncing")]
    dry_run: bool,
}

pub struct ModernSyncProcess {
    app: Arc<MononokeApp>,
    sync_args: Arc<CommandArgs>,
    exit_file: PathBuf,
    cancellation_requested: Arc<AtomicBool>,
}

impl ModernSyncProcess {
    fn new(
        app: Arc<MononokeApp>,
        sync_args: Arc<CommandArgs>,
        exit_file: PathBuf,
        cancellation_requested: Arc<AtomicBool>,
    ) -> Self {
        Self {
            app,
            sync_args,
            exit_file,
            cancellation_requested,
        }
    }
}

#[async_trait]
impl RepoShardedProcess for ModernSyncProcess {
    async fn setup(&self, repo: &RepoShard) -> anyhow::Result<Arc<dyn RepoShardedProcessExecutor>> {
        if fs::metadata(self.exit_file.clone()).is_ok() {
            tracing::warn!("Exit file detected, stopping sync");
            self.cancellation_requested.store(true, Ordering::Relaxed);
            return Err(anyhow!("Exit file detected, stopping sync"));
        }

        // we cannot trust the repo arguments from app arguments, we need to parse them from the shard
        let source_repo_name = repo.repo_name.clone();
        let target_repo_name = match repo.target_repo_name.clone() {
            Some(repo_name) => repo_name,
            None => {
                let details = format!(
                    "Only source repo name {source_repo_name} provided, target repo name missing in {repo}"
                );
                bail!("{details}")
            }
        };

        tracing::info!(
            "Setting up sharded sync from repo {} to repo {}",
            source_repo_name,
            target_repo_name,
        );

        let ctx =
            crate::sync::build_context(self.app.clone(), &source_repo_name, self.sync_args.dry_run);

        let source_repo_args = SourceRepoArgs::with_name(source_repo_name.clone());
        let repo: Repo = self.app.open_repo_unredacted(&source_repo_args).await?;
        let config = repo.repo_config.modern_sync_config.clone().ok_or(anyhow!(
            "No modern sync config found for repo {source_repo_name}"
        ))?;

        // Check that we can connect to the target repo
        let app_args = self.app.args::<ModernSyncArgs>()?;
        _ = crate::sync::build_edenfs_client(ctx, &app_args, &target_repo_name, &config, &repo)
            .await
            .with_context(|| format!("checking that we can connect to {target_repo_name}"))?;

        Ok(Arc::new(ModernSyncProcessExecutor {
            app: self.app.clone(),
            sync_args: self.sync_args.clone(),
            source_repo_args,
            dest_repo_name: target_repo_name,
            exit_file: self.exit_file.clone(),
            cancellation_requested: self.cancellation_requested.clone(),
        }))
    }
}

pub struct ModernSyncProcessExecutor {
    app: Arc<MononokeApp>,
    sync_args: Arc<CommandArgs>,
    source_repo_args: SourceRepoArgs,
    dest_repo_name: String,
    exit_file: PathBuf,
    cancellation_requested: Arc<AtomicBool>,
}

#[async_trait]
impl RepoShardedProcessExecutor for ModernSyncProcessExecutor {
    async fn execute(&self) -> Result<()> {
        crate::sync::sync(
            self.app.clone(),
            self.sync_args.start_id,
            self.source_repo_args.clone(),
            self.dest_repo_name.clone(),
            ExecutionType::Tail,
            self.sync_args.dry_run,
            self.exit_file.clone(),
            None,
            None,
            self.cancellation_requested.clone(),
        )
        .await?;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        Ok(())
    }
}

pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()> {
    let args = Arc::new(args);
    let app_args = &app.args::<ModernSyncArgs>()?;
    let exit_file = app_args.exit_file.clone();
    let cancellation_requested = Arc::new(AtomicBool::new(false));

    let process = Arc::new(ModernSyncProcess::new(
        Arc::new(app),
        args.clone(),
        exit_file,
        cancellation_requested,
    ));

    if let Some(executor) = app_args.sharded_executor_args.clone().build_executor(
        process.app.fb,
        process.app.runtime().clone(),
        || process.clone(),
        true, // enable shard (repo) level healing
        SM_CLEANUP_TIMEOUT_SECS,
    )? {
        tracing::info!("Running sharded sync loop");
        let (sender, receiver) = tokio::sync::oneshot::channel::<bool>();
        executor.block_and_execute(receiver).await?;
        drop(sender);
    } else {
        bail!("can only run sharded")
    }

    Ok(())
}
