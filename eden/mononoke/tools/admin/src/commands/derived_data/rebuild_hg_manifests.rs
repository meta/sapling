/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Rebuild the Hg trees of commits from their augmented manifests and compare
//! them with the stored ones. Nothing is written unless `--store-missing` is
//! given. It needs `--cache-mode` set to `disabled` or `local-only`, so that a
//! tree deleted from storage but still in the shared cache is not taken as
//! stored.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use blobstore::PutBehaviour;
use bonsai_git_mapping::BonsaiGitMapping;
use bonsai_globalrev_mapping::BonsaiGlobalrevMapping;
use bonsai_hg_mapping::BonsaiHgMapping;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bonsai_p4_mapping::BonsaiP4Mapping;
use bonsai_svnrev_mapping::BonsaiSvnrevMapping;
use clap::Args;
use commit_id::parse_commit_id;
use context::CoreContext;
use environment::Caching;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use mercurial_derivation::rebuild_hg_manifests::HgTreeRebuilder;
use mercurial_derivation::rebuild_hg_manifests::RebuildAction;
use mercurial_derivation::rebuild_hg_manifests::TreeOutcome;
use mercurial_derivation::rebuild_hg_manifests::TreeScope;
use mercurial_derivation::rebuild_hg_manifests::commit_hg_roots;
use mononoke_app::MononokeApp;
use mononoke_app::args::RepoArgs;
use mononoke_types::ChangesetId;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use restricted_paths::RestrictedPaths;
use restricted_paths::RestrictedPathsRef;

const MAX_CONCURRENT_RESOLVES: usize = 10;

#[facet::container]
pub(super) struct Repo {
    #[facet]
    repo_blobstore: RepoBlobstore,
    #[facet]
    restricted_paths: RestrictedPaths,
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
}

#[derive(Args)]
pub(super) struct RebuildHgManifestsArgs {
    /// Commits to check, as Bonsai, Hg or other commit ids.
    #[clap(long = "commit-id", short = 'i')]
    commit_ids: Vec<String>,

    /// File with one commit id per line, checked after those given with
    /// `--commit-id`.
    #[clap(long)]
    commit_ids_file: Option<PathBuf>,

    /// Rebuild every tree of each commit, not only the trees that differ from
    /// the tree at the same path in every parent.
    #[clap(long)]
    whole_tree: bool,

    /// Store each rebuilt tree that has no stored Hg tree, and record the
    /// restriction of each consistent tree at a writable restriction root.
    /// While restricted-path recording is switched off, every tree at such a
    /// root except a differing one, which already fails the command, is
    /// reported, a missing one is not stored, and the command fails, so a rerun
    /// once recording is back on records and stores them.
    /// Without this, nothing is written. It also needs the default `IfAbsent`
    /// put behaviour, so that no stored tree is overwritten.
    #[clap(long)]
    store_missing: bool,
}

pub(super) async fn rebuild_hg_manifests(
    ctx: &CoreContext,
    app: &MononokeApp,
    repo_args: &RepoArgs,
    args: RebuildHgManifestsArgs,
) -> Result<()> {
    if matches!(app.environment().caching, Caching::Enabled(_)) {
        bail!("this command needs --cache-mode=disabled or --cache-mode=local-only");
    }
    if args.store_missing && app.blobstore_options().put_behaviour != PutBehaviour::IfAbsent {
        bail!("--store-missing needs the IfAbsent put behaviour");
    }
    let repo: Repo = app.open_repo(repo_args).await?;
    let repo = &repo;
    let mut commit_ids = args.commit_ids;
    if let Some(path) = &args.commit_ids_file {
        let text = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        commit_ids.extend(
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(String::from),
        );
    }
    let cs_ids: Vec<ChangesetId> = stream::iter(&commit_ids)
        .map(|commit_id| async move {
            parse_commit_id(ctx, repo, commit_id)
                .await
                .with_context(|| format!("resolving commit id {commit_id:?}"))
        })
        .buffered(MAX_CONCURRENT_RESOLVES)
        .try_collect()
        .await?;
    if cs_ids.is_empty() {
        bail!("no commits given");
    }
    let scope = if args.whole_tree {
        TreeScope::Whole
    } else {
        TreeScope::NewInCommit
    };
    let action = if args.store_missing {
        RebuildAction::StoreMissing
    } else {
        RebuildAction::Compare
    };

    let rebuilder = HgTreeRebuilder::new(
        repo.repo_blobstore().clone(),
        repo.restricted_paths().config_based().clone(),
    );
    let (mut wrong, mut failed) = (0, 0);
    for cs_id in cs_ids {
        match rebuild_commit(ctx, repo, &rebuilder, cs_id, scope, action).await {
            Ok(counts) => {
                wrong += [
                    TreeOutcome::Different,
                    TreeOutcome::Inconsistent,
                    TreeOutcome::RestrictionNotRecorded,
                ]
                .iter()
                .filter_map(|outcome| counts.get(outcome))
                .sum::<usize>();
                println!("{cs_id}: {counts:?}");
            }
            Err(err) => {
                failed += 1;
                println!("{cs_id}: failed: {err:#}");
            }
        }
    }

    if wrong > 0 || failed > 0 {
        bail!(
            "{wrong} rebuilt trees differ from the stored ones or from their own records, or could not have their restriction recorded, and {failed} commits failed"
        );
    }
    Ok(())
}

/// Rebuild one commit's trees, printing each that is not identical, and count
/// the outcomes.
async fn rebuild_commit(
    ctx: &CoreContext,
    repo: &Repo,
    rebuilder: &HgTreeRebuilder<RepoBlobstore>,
    cs_id: ChangesetId,
    scope: TreeScope,
    action: RebuildAction,
) -> Result<BTreeMap<TreeOutcome, usize>> {
    let roots =
        commit_hg_roots(ctx, repo.repo_blobstore(), repo.bonsai_hg_mapping(), cs_id).await?;
    rebuilder
        .rebuild(ctx, roots, scope, action)
        .try_fold(BTreeMap::new(), |mut counts, tree| {
            if tree.outcome != TreeOutcome::Identical {
                println!(
                    "{cs_id} {:?} {} {:?}",
                    tree.path.to_string(),
                    tree.tree,
                    tree.outcome
                );
            }
            *counts.entry(tree.outcome).or_insert(0usize) += 1;
            async move { Ok(counts) }
        })
        .await
}
