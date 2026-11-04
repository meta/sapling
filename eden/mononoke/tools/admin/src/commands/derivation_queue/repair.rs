/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use anyhow::anyhow;
use clap::Args;
use commit_id::parse_commit_id;
use context::CoreContext;
use mononoke_app::args::MultiDerivedDataArgs;
use repo_derivation_queues::RepairOptions;
use repo_derivation_queues::RepoDerivationQueuesRef;

use super::Repo;

/// Derives missing batch heads locally, then removes isolated queue items.
/// Never reconstructs dependency edges. Do not run concurrently with unsafe-nuke
/// or unsafe-evict.
#[derive(Args)]
pub struct RepairArgs {
    #[clap(flatten)]
    derived_data: MultiDerivedDataArgs,

    /// Restrict to these batch roots. Omit to discover stranded items.
    #[clap(long, short = 'i')]
    commit_id: Vec<String>,

    /// Derive missing data and remove completed items. Otherwise only inspect and report.
    #[clap(long)]
    apply: bool,

    /// Refuse to proceed if more than this many candidates are selected.
    #[clap(long, default_value_t = 100)]
    limit: usize,
}

pub async fn repair(
    ctx: &CoreContext,
    repo: &Repo,
    config_name: &str,
    args: RepairArgs,
) -> Result<()> {
    let queue = repo
        .repo_derivation_queues()
        .queue(config_name)
        .ok_or_else(|| anyhow!("Missing derivation queue for config {config_name}"))?;
    let mut roots = Vec::new();
    for id in &args.commit_id {
        roots.push(parse_commit_id(ctx, repo, id).await?);
    }
    let options = RepairOptions {
        derived_data_types: args
            .derived_data
            .resolve_types(queue.derived_data_manager().config())?,
        roots,
        apply: args.apply,
        limit: args.limit,
    };
    let report = queue.repair(ctx, options).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
