/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::path::PathBuf;

use anyhow::Result;
use bonsai_hg_mapping::BonsaiHgMapping;
use bookmarks::BookmarkUpdateLog;
use bookmarks::Bookmarks;
use clap::Parser;
use commit_graph::CommitGraph;
use executor_lib::args::ShardedExecutorArgs;
use fbinit::FacebookInit;
use metaconfig_types::RepoConfig;
use mononoke_app::MononokeApp;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::MononokeService;
use mononoke_app::args::RepoFilterAppExtension;
use mononoke_app::args::TLSArgs;
use mononoke_app::monitoring::AliveService;
use mononoke_app::monitoring::MonitoringAppExtension;
use mutable_counters::MutableCounters;
use repo_blobstore::RepoBlobstore;
use repo_derived_data::RepoDerivedData;
use repo_identity::RepoIdentity;

mod bul_util;
mod commands;
mod sender;
mod stat;
mod sync;

#[derive(Parser)]
struct ModernSyncArgs {
    #[clap(flatten)]
    sharded_executor_args: ShardedExecutorArgs,

    #[clap(long)]
    /// Update ODS counters
    log_to_ods: bool,

    #[clap(long)]
    /// Path to file to check for in order to exit
    exit_file: PathBuf,

    #[clap(long)]
    /// Group bookmark update log entries into bigger batches
    flatten_bul: bool,

    #[clap(long)]
    /// Bookmark to sync (default: master)
    bookmark: String,

    #[clap(flatten, next_help_heading = "EDENAPI OPTIONS")]
    edenapi_args: EdenAPIArgs,
}

#[derive(Parser)]
struct EdenAPIArgs {
    #[clap(long, hide = true)] // To be used only in integration tests
    pub dest_socket: Option<u64>,

    /// TLS parameters for this service
    #[clap(flatten)]
    tls_params: Option<TLSArgs>,

    #[clap(long, default_value = "http://fwdproxy:8080")]
    http_proxy_host: Option<String>,

    #[clap(
        long,
        default_value = ".fbcdn.net,.facebook.com,.thefacebook.com,.tfbnw.net,.fb.com,.fburl.com,.facebook.net,.sb.fbsbx.com,localhost"
    )]
    http_no_proxy: Option<String>,
}

#[facet::container]
#[derive(Clone)]
pub struct Repo {
    #[facet]
    pub mutable_counters: dyn MutableCounters,
    #[facet]
    repo_identity: RepoIdentity,
    #[facet]
    bookmark_update_log: dyn BookmarkUpdateLog,
    #[facet]
    commit_graph: CommitGraph,
    #[facet]
    repo_derived_data: RepoDerivedData,
    #[facet]
    repo_blobstore: RepoBlobstore,
    #[facet]
    pub repo_config: RepoConfig,
    #[facet]
    bookmarks: dyn Bookmarks,
    #[facet]
    bonsai_hg_mapping: dyn BonsaiHgMapping,
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<()> {
    let subcommands = commands::subcommands();
    let app = MononokeAppBuilder::new(fb)
        .with_default_service(MononokeService::MODERN_SYNC)
        .with_default_scuba_dataset("mononoke_modern_sync")
        .with_app_extension(MonitoringAppExtension {})
        .with_app_extension(RepoFilterAppExtension {})
        .build_with_subcommands::<ModernSyncArgs>(subcommands)?;
    app.run_with_monitoring_and_logging(async_main, "modern_sync", AliveService)
}

async fn async_main(app: MononokeApp) -> Result<()> {
    commands::dispatch(app).await
}
