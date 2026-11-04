/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Perforce -> Mononoke mirror tailer.
//!
//! Skeleton: registers with ShardManager, holds the shards it is assigned and
//! idles. Nothing talks to Perforce yet; the importer replaces
//! `sharded::P4SyncProcessExecutor::execute`.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use executor_lib::args::ShardedExecutorArgs;
use fbinit::FacebookInit;
use metaconfig_types::RepoConfig;
use mononoke_app::MononokeApp;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::args::RepoFilterAppExtension;
use mononoke_app::args::TLSArgs;
use mononoke_app::monitoring::AliveService;
use mononoke_app::monitoring::MonitoringAppExtension;
use p4_source::P4Args;
use repo_identity::RepoIdentity;

mod sharded;

/// Perforce -> Mononoke mirror tailer
#[derive(Parser)]
struct P4SyncArgs {
    #[clap(flatten)]
    sharded_executor_args: ShardedExecutorArgs,

    /// Update ODS counters
    #[clap(long)]
    log_to_ods: bool,

    /// Path to file to check for in order to exit
    #[clap(long)]
    exit_file: PathBuf,

    /// TLS parameters for this service
    #[clap(flatten)]
    tls_params: Option<TLSArgs>,

    #[clap(flatten, next_help_heading = "PERFORCE OPTIONS")]
    p4_args: P4Args,
}

#[facet::container]
#[derive(Clone)]
pub struct Repo {
    #[facet]
    repo_identity: RepoIdentity,
    #[facet]
    repo_config: RepoConfig,
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<()> {
    let app = MononokeAppBuilder::new(fb)
        .with_default_scuba_dataset("mononoke_p4_sync")
        .with_app_extension(MonitoringAppExtension {})
        .with_app_extension(RepoFilterAppExtension {})
        .build::<P4SyncArgs>()?;
    app.run_with_monitoring_and_logging(async_main, "p4_sync", AliveService)
}

async fn async_main(app: MononokeApp) -> Result<()> {
    let args = app.args::<P4SyncArgs>()?;
    let p4 = &args.p4_args;
    tracing::info!(
        p4_binary = %p4.p4_binary.display(),
        p4_port = %p4.p4_port,
        p4_user = %p4.p4_user,
        p4_password_file = %p4.p4_password_file.display(),
        log_to_ods = args.log_to_ods,
        tls = args.tls_params.is_some(),
        "p4_sync skeleton: Perforce flags received, not used yet",
    );
    sharded::run(app, args.sharded_executor_args, args.exit_file).await
}
