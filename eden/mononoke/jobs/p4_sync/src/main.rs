/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Perforce -> Mononoke mirror tailer.
//!
//! Registers with ShardManager and holds the shards of the repos it has a depot for; nothing
//! talks to Perforce yet.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use executor_lib::args::ShardedExecutorArgs;
use fbinit::FacebookInit;
use mononoke_app::MononokeApp;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::args::RepoFilterAppExtension;
use mononoke_app::args::TLSArgs;
use mononoke_app::monitoring::AliveService;
use mononoke_app::monitoring::MonitoringAppExtension;
use p4_source::P4Args;

mod repo;
mod sharded;

use crate::repo::Repo;

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

    /// The depot a repo mirrors, as `<repo>=<depot root>` (e.g. `p4/test1=//depot/`); the
    /// shard of any other repo is held and nothing is imported into it. Until the repo config
    /// says which depot it mirrors.
    #[clap(long = "repo-depot", value_parser = parse_repo_depot)]
    repo_depots: Vec<(String, String)>,
}

fn parse_repo_depot(arg: &str) -> Result<(String, String)> {
    let (repo, depot) = arg
        .split_once('=')
        .with_context(|| format!("expected <repo>=<depot root>, got {arg}"))?;
    anyhow::ensure!(
        !repo.is_empty() && depot.starts_with("//") && depot.ends_with('/'),
        "expected <repo>=//<depot>/, got {arg}"
    );
    Ok((repo.to_owned(), depot.to_owned()))
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
    let depots: HashMap<_, _> = args.repo_depots.into_iter().collect();
    let settings = sharded::Settings {
        exit_file: args.exit_file,
        depots,
    };
    sharded::run(app, args.sharded_executor_args, settings).await
}

#[cfg(test)]
mod tests;
