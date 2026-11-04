/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod enqueue;
mod inspect;
mod nuke;
mod repair;
mod summary;
mod unsafe_evict;

use anyhow::Result;
use bonsai_git_mapping::BonsaiGitMapping;
use bonsai_globalrev_mapping::BonsaiGlobalrevMapping;
use bonsai_hg_mapping::BonsaiHgMapping;
use bonsai_p4_mapping::BonsaiP4Mapping;
use bonsai_svnrev_mapping::BonsaiSvnrevMapping;
use bookmarks::Bookmarks;
use clap::Parser;
use clap::Subcommand;
use commit_graph::CommitGraph;
use enqueue::EnqueueArgs;
use ephemeral_blobstore::RepoEphemeralStore;
use inspect::InspectArgs;
use metaconfig_types::RepoConfig;
use metaconfig_types::RepoConfigRef;
use mononoke_app::MononokeApp;
use mononoke_app::args::RepoArgs;
use nuke::NukeArgs;
use repair::RepairArgs;
use repo_blobstore::RepoBlobstore;
use repo_derivation_queues::RepoDerivationQueues;
use repo_identity::RepoIdentity;
use summary::SummaryArgs;
use unsafe_evict::UnsafeEvictArgs;

/// Query and manage the derivation queue
#[derive(Parser)]
pub struct CommandArgs {
    #[clap(flatten)]
    repo: RepoArgs,

    /// Name of the config to query the derivation queue of.
    #[clap(short, long)]
    config_name: Option<String>,

    #[clap(subcommand)]
    subcommand: DerivationQueueSubcommand,
}

#[derive(Subcommand)]
pub enum DerivationQueueSubcommand {
    /// Enqueue a derivation request for a commit
    Enqueue(EnqueueArgs),
    /// Display a summary of the items in the derivation queue.
    Summary(SummaryArgs),
    /// Evict an item (referenced by root cs_id and derived data type) from the derivation queue. WARNING: can leave dependent items in the queue stuck
    UnsafeEvict(UnsafeEvictArgs),
    /// Inspect the Zelos DAG state of a specific item in the derivation queue
    Inspect(InspectArgs),
    /// Delete every item in the derivation queue for this (repo, config). WARNING: nukes the entire queue. Pause the derivation service first.
    UnsafeNuke(NukeArgs),
    /// Derive and remove isolated stranded items (dry run unless --apply).
    Repair(RepairArgs),
}

#[facet::container]
pub struct Repo {
    #[facet]
    commit_graph: CommitGraph,

    #[facet]
    repo_blobstore: RepoBlobstore,

    #[facet]
    repo_ephemeral_store: RepoEphemeralStore,

    #[facet]
    repo_derivation_queues: RepoDerivationQueues,

    #[facet]
    repo_identity: RepoIdentity,

    #[facet]
    repo_config: RepoConfig,

    #[facet]
    bonsai_hg_mapping: dyn BonsaiHgMapping,

    #[facet]
    bonsai_git_mapping: dyn BonsaiGitMapping,

    #[facet]
    bonsai_globalrev_mapping: dyn BonsaiGlobalrevMapping,

    #[facet]
    bonsai_svnrev_mapping: dyn BonsaiSvnrevMapping,

    #[facet]
    bonsai_p4_mapping: dyn BonsaiP4Mapping,

    #[facet]
    bookmarks: dyn Bookmarks,
}

pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()> {
    let ctx = app.new_basic_context();
    let repo: Repo = app.open_repo(&args.repo).await?;

    let config_name = args
        .config_name
        .as_ref()
        .unwrap_or_else(|| &repo.repo_config().derived_data_config.enabled_config_name);

    match args.subcommand {
        DerivationQueueSubcommand::Enqueue(args) => {
            enqueue::enqueue(&ctx, &repo, config_name, args).await
        }
        DerivationQueueSubcommand::Summary(args) => {
            summary::summary(&ctx, &repo, config_name, args).await
        }
        DerivationQueueSubcommand::UnsafeEvict(args) => {
            unsafe_evict::unsafe_evict(&ctx, &repo, config_name, args).await
        }
        DerivationQueueSubcommand::Inspect(args) => {
            inspect::inspect(&ctx, &repo, config_name, args).await
        }
        DerivationQueueSubcommand::UnsafeNuke(args) => {
            nuke::nuke(&ctx, &repo, config_name, args).await
        }
        DerivationQueueSubcommand::Repair(args) => {
            repair::repair(&ctx, &repo, config_name, args).await
        }
    }
}
