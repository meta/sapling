/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use mononoke_app::MononokeApp;
use mononoke_app::args::RepoArg;
use mononoke_types::RepositoryId;

use crate::policy::RetiredBlobType;
use crate::policy::Selection;
use crate::scope;
use crate::scope::PlanRequest;
use crate::scope::RepoSelection;
use crate::state::StateDir;

/// Resolve the repositories and stores a run covers, and record the plan
#[derive(Parser)]
pub struct CommandArgs {
    /// Directory holding this run's plan, inventory and journals
    #[clap(long)]
    state_dir: PathBuf,

    /// Repositories to cover, by id
    #[clap(long = "repo-id")]
    repo_ids: Vec<i32>,

    /// File of repository ids to cover, one per line
    #[clap(long)]
    repo_ids_file: Option<PathBuf>,

    /// Repositories to cover, by name
    #[clap(long = "repo-name", short = 'R')]
    repo_names: Vec<String>,

    /// Cover every repository in the configuration
    #[clap(long, conflicts_with_all = ["repo_ids", "repo_ids_file", "repo_names"])]
    all_repos: bool,

    /// Retired types to delete
    #[clap(long = "retired-type", value_enum, required = true)]
    retired_types: Vec<RetiredBlobType>,

    /// Named storage configs to scan for every covered repository, in
    /// addition to its own storage
    #[clap(long = "extra-storage-config")]
    extra_storage_configs: Vec<String>,

    /// Hex digits of the hash used to partition each recipe's keys; 0 makes
    /// each cell a single partition
    #[clap(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(0..=2))]
    partition_digits: u8,
}

async fn read_repo_ids(path: &Path) -> Result<Vec<RepositoryId>> {
    let text = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.parse()
                .map(RepositoryId::new)
                .with_context(|| format!("{} holds {line:?}, not a repository id", path.display()))
        })
        .collect()
}

pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()> {
    let repos = if args.all_repos {
        RepoSelection::AllConfigured
    } else {
        let from_file = match &args.repo_ids_file {
            Some(path) => read_repo_ids(path).await?,
            None => Vec::new(),
        };
        let by_name = args
            .repo_names
            .iter()
            .map(|name| app.repo_id(&RepoArg::Name(name.clone())))
            .collect::<Result<Vec<_>>>()?;
        RepoSelection::Explicit(
            args.repo_ids
                .into_iter()
                .map(RepositoryId::new)
                .chain(from_file)
                .chain(by_name)
                .collect(),
        )
    };
    let request = PlanRequest::new(
        repos,
        Selection::new(args.retired_types)?,
        args.extra_storage_configs,
        args.partition_digits,
    )?;
    let resolved = scope::resolve(&app, request)?;
    StateDir::create(&args.state_dir, &resolved.plan).await?;
    println!("{}", scope::summarise(&resolved.plan));
    Ok(())
}
