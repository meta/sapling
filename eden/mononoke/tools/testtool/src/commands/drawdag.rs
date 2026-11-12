/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! DrawDAG for Integration Tests
//!
//! A DrawDAG specification consists of an ASCII graph (either left-to-right
//! or bottom-to-top), and a series of comments that define actions that apply
//! to that graph.
//!
//! See documentation of `Action` for actions that affect the repository, and
//! `ChangeAction` for actions that change commits.
//!
//! Values that contain special characters can be surrounded by quotes.
//! Values that require binary data can prefix a hex string with `&`, e.g.
//! `&face` becomes a two byte string with the values `FA CE`.

use std::collections::HashMap;
use std::fmt::Display;
use std::io::Write;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bulk_derivation::BulkDerivation;
use clap::Parser;
use context::CoreContext;
use derived_data_manager::BonsaiDerivable;
use mercurial_derivation::MappedHgChangesetId;
use mercurial_derivation::RootHgAugmentedManifestId;
use mononoke_app::MononokeApp;
use mononoke_app::args::RepoArgs;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use phases::PhasesRef;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentityRef;
use tests_utils::drawdag::extend_from_dag_with_actions;
use tokio::io::AsyncReadExt;
use topo_sort::sort_topological;

use crate::repo::Repo;

/// Create commits from a drawn DAG.
#[derive(Parser)]
pub struct CommandArgs {
    #[clap(flatten)]
    repo_args: RepoArgs,

    /// Disable creation of default files in each commit
    #[clap(long)]
    no_default_files: bool,

    /// Derive all configured data types and persist phases from publishing bookmarks
    #[clap(long)]
    derive_all: bool,

    /// Print hashes in HG format instead of bonsai
    #[clap(long)]
    print_hg_hashes: bool,

    /// Skip augmented-manifest derivation, for tests that need a commit
    /// deliberately left without one (e.g. to exercise the fail-closed path).
    #[clap(long)]
    no_derive_hg_augmented: bool,
}

fn print_name_hash_pairs(pairs: impl IntoIterator<Item = (String, impl Display)>) -> Result<()> {
    for (name, id) in pairs.into_iter() {
        writeln!(std::io::stdout(), "{name}={id}")?;
    }
    Ok(())
}

pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()> {
    let ctx = app.new_basic_context();

    let repo: Repo = app
        .open_repo(&args.repo_args)
        .await
        .context("Failed to open repo")?;

    // Read DAG from stdin
    let mut input = String::new();
    tokio::io::stdin().read_to_string(&mut input).await?;
    if args.no_default_files {
        input.push_str("\n# default_files: false\n");
    }

    let (commits, dag) = extend_from_dag_with_actions(&ctx, &repo, &input).await?;

    let any_derivation_needed = args.derive_all | args.print_hg_hashes;
    if any_derivation_needed {
        let dag: HashMap<_, _> = dag
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().collect::<Vec<_>>()))
            .collect();
        let sorted = sort_topological(&dag).ok_or_else(|| anyhow!("Graph has a cycle"))?;
        let csids = sorted
            .into_iter()
            .map(|name| {
                commits
                    .get(&name)
                    .cloned()
                    .ok_or_else(|| anyhow!("No commit found for {name}"))
            })
            .collect::<Result<Vec<_>>>()?;

        if args.derive_all {
            derive_all(&ctx, &repo, &csids).await?;
        } else {
            // Mirror the creation-time augmented-manifest derivation for drawdag
            // commits so route_original_to_augmented_hg_manifest can serve them.
            // Unlike the best-effort serving paths this propagates errors (`?`): a
            // fixture that can't derive what it asked for should fail loudly.
            let augmented_enabled = repo
                .repo_derived_data()
                .active_config()
                .types
                .contains(&DerivableType::HgAugmentedManifests);
            let augmented_csids = (!args.no_derive_hg_augmented
                && augmented_enabled
                && justknobs::eval(
                    "scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset",
                    ctx.metadata()
                        .client_request_info()
                        .map(|c| c.correlator.as_str()),
                    Some(repo.repo_identity().name()),
                ))
            .then(|| csids.clone());
            derive::<MappedHgChangesetId>(&ctx, &repo, csids).await?;
            if let Some(csids) = augmented_csids {
                derive::<RootHgAugmentedManifestId>(&ctx, &repo, csids).await?;
            }
        }
    }

    if args.print_hg_hashes {
        let mapping: HashMap<_, _> = repo
            .bonsai_hg_mapping()
            .get(&ctx, commits.values().copied().collect::<Vec<_>>().into())
            .await?
            .into_iter()
            .map(|entry| (entry.bcs_id, entry.hg_cs_id))
            .collect();
        let commits = commits
            .into_iter()
            .map(|(name, id)| {
                mapping
                    .get(&id)
                    .ok_or_else(|| anyhow!("Couldn't translate {name}={id} to hg"))
                    .map(|hg_id| (name, hg_id))
            })
            .collect::<Result<Vec<_>>>()?;
        print_name_hash_pairs(commits)?;
    } else {
        print_name_hash_pairs(commits)?;
    }

    Ok(())
}

async fn derive<D: BonsaiDerivable>(
    ctx: &CoreContext,
    repo: &Repo,
    csids: Vec<ChangesetId>,
) -> Result<()> {
    let mgr = repo.repo_derived_data().manager();
    let rederivation = None;
    let override_batch_size = None;
    mgr.derive_heads::<D>(ctx.clone(), csids, override_batch_size, rederivation)
        .await
        .with_context(|| format!("Failed to derive {}", D::NAME))?;
    Ok(())
}

async fn derive_all(ctx: &CoreContext, repo: &Repo, csids: &[ChangesetId]) -> Result<()> {
    let derived_data_types = repo
        .repo_derived_data()
        .active_config()
        .types
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    repo.repo_derived_data()
        .manager()
        .derive_bulk_locally(ctx, csids, None, derived_data_types.as_slice(), None, None)
        .await?;
    repo.phases()
        .get_public(ctx, csids.to_vec(), false)
        .await
        .context("Failed to derive public phases")?;
    Ok(())
}
