/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![type_length_limit = "4522397"]
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::Context;
use anyhow::Error;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::format_err;
use backsyncer::BacksyncLimit;
use backsyncer::backsync_latest;
use backsyncer::open_backsyncer_dbs;
use blobstore::Loadable;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkUpdateReason;
use bookmarks::BookmarksRef;
use borrowed::borrowed;
use bulk_derivation::BulkDerivation;
use changesets_creation::save_changesets;
use cmdlib_cross_repo::repo_provider_from_mononoke_app;
use context::CoreContext;
use cross_repo_sync::CandidateSelectionHint;
use cross_repo_sync::CommitSyncContext;
use cross_repo_sync::CommitSyncData;
use cross_repo_sync::CommitSyncOutcome;
use cross_repo_sync::Repo as CrossRepo;
use cross_repo_sync::Syncers;
use cross_repo_sync::create_commit_syncers;
use cross_repo_sync::find_toposorted_unsynced_ancestors;
use cross_repo_sync::get_all_submodule_deps_from_repo_pair;
use cross_repo_sync::rewrite_commit;
use cross_repo_sync::unsafe_sync_commit;
use environment::MononokeEnvironment;
use fbinit::FacebookInit;
use futures::future;
use futures::future::TryFutureExt;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use import_tools::GitImportLfs;
use import_tools::GitimportPreferences;
use import_tools::GitimportTarget;
use import_tools::LfsServerUrlFormat;
use import_tools::ReuploadCommits;
use itertools::Itertools;
use live_commit_sync_config::CfgrLiveCommitSyncConfig;
use live_commit_sync_config::LiveCommitSyncConfig;
use maplit::hashset;
use mercurial_derivation::DeriveHgChangeset;
use mercurial_types::HgChangesetId;
use mercurial_types::NonRootMPath;
use mercurial_types::sharded_augmented_manifest::list_hg_leaf_entries;
use metaconfig_types::CommitSyncConfigVersion;
use metaconfig_types::DefaultSmallToLargeCommitSyncPathAction;
use metaconfig_types::RepoConfig;
use mononoke_app::MononokeApp;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::args::RepoArg;
use mononoke_app::args::RepoArgs;
use mononoke_app::monitoring::AliveService;
use mononoke_app::monitoring::MonitoringAppExtension;
use mononoke_types::BonsaiChangeset;
use mononoke_types::BonsaiChangesetMut;
use mononoke_types::ChangesetId;
use mononoke_types::DateTime;
use mononoke_types::RepositoryId;
use movers::CrossRepoMover;
use movers::DefaultAction;
use movers::Mover;
use movers::Movers;
use pushrebase::do_pushrebase_bonsai;
use pushredirect::PushRedirectionConfigArc;
use serde::Deserialize;
use serde::Serialize;
use synced_commit_mapping::SyncedCommitMappingRef;
use tokio::fs;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process;
use tokio::time;
use topo_sort::sort_topological;
use tracing::info;
use wireproto_handler::TargetRepoDbs;

mod cli;
mod repo;
mod tests;

use crate::cli::CheckAdditionalSetupStepsArgs;
use crate::cli::Commands::CheckAdditionalSetupSteps;
use crate::cli::Commands::Import;
use crate::cli::Commands::RecoverProcess;
use crate::cli::LfsImportArgs;
use crate::cli::LfsServerUrlFormatArg;
use crate::cli::MononokeRepoImportArgs;
use crate::cli::setup_import_args;
use crate::repo::Repo;

const LFS_SIMULTANEOUS_CONNECTION_LIMIT: usize = 20;

#[derive(Deserialize, Clone, Debug)]
struct GraphqlQueryObj {
    differential_commit_query: Vec<GraphqlCommitQueryObj>,
}
#[derive(Deserialize, Clone, Debug)]
struct GraphqlCommitQueryObj {
    results: GraphqlResultsObj,
}
#[derive(Deserialize, Clone, Debug)]
struct GraphqlResultsObj {
    nodes: Vec<GraphqlImportedObj>,
}
#[derive(Deserialize, Clone, Debug)]
struct GraphqlImportedObj {
    imported: bool,
}
#[derive(Debug, Serialize)]
struct GraphqlInputVariables {
    commit: String,
}
#[derive(Debug)]
struct CheckerFlags {
    phab_check_disabled: bool,
    x_repo_check_disabled: bool,
}
#[derive(Clone, Debug)]
struct ChangesetArgs {
    pub author: String,
    pub message: String,
    pub datetime: DateTime,
}
#[derive(Clone, Debug, PartialEq)]
struct RepoImportSetting {
    importing_bookmark: BookmarkKey,
    dest_bookmark: BookmarkKey,
}

#[derive(Clone)]
struct SmallRepoBackSyncVars {
    large_to_small_syncer: CommitSyncData<Repo>,
    target_repo_dbs: TargetRepoDbs,
    small_repo_bookmark: BookmarkKey,
    small_repo: Repo,
    maybe_call_sign: Option<String>,
    version: CommitSyncConfigVersion,
}

#[derive(Copy, Clone, Serialize, Deserialize, Debug, PartialEq)]
enum ImportStage {
    GitImport,
    RewritePaths,
    DeriveBonsais,
    MoveBookmark,
    MergeCommits,
    PushCommit,
}

/*
    Most fields can be found with 'repo_import --help'
*/
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct RecoveryFields {
    /// Indicates which stage we will recover from in case of recovery process
    import_stage: ImportStage,
    recovery_file_path: String,
    git_repo_path: String,
    git_merge_rev_id: String,
    dest_path: String,
    bookmark_suffix: String,
    batch_size: usize,
    move_bookmark_commits_done: usize,
    phab_check_disabled: bool,
    x_repo_check_disabled: bool,
    sleep_time: Duration,
    dest_bookmark_name: String,
    commit_author: String,
    commit_message: String,
    datetime: DateTime,
    mark_not_synced_mapping: Option<String>,
    /// Head of the imported commits
    imported_cs_id: Option<ChangesetId>,
    /// ChangesetId of the merged commit we make to merge the imported commits into dest_bookmark
    merged_cs_id: Option<ChangesetId>,
    /// ChangesetIds created after shifting the file paths of the gitimported commits
    shifted_bcs_ids: Option<Vec<ChangesetId>>,
    /// ChangesetIds of the gitimported commits
    gitimport_bcs_ids: Option<Vec<ChangesetId>>,
    git_merge_bcs_id: Option<ChangesetId>,
    /// Print the mapping of git commit id -> bonsai changeset id after importing
    /// all git commits.
    print_gitimport_map: bool,
}

async fn rewrite_file_paths(
    ctx: &CoreContext,
    repo: &Repo,
    movers: &Movers,
    gitimport_bcs_ids: &[ChangesetId],
    git_merge_bcs_id: &ChangesetId,
) -> Result<(Vec<ChangesetId>, Option<ChangesetId>), Error> {
    let mut remapped_parents: HashMap<ChangesetId, ChangesetId> = HashMap::new();
    let mut bonsai_changesets = vec![];

    let mut git_merge_shifted_bcs_id = None;

    let len = gitimport_bcs_ids.len();
    let gitimport_changesets = stream::iter(gitimport_bcs_ids.iter().map(|bcs_id| async move {
        let bcs = bcs_id.load(ctx, repo.repo_blobstore()).await?;
        Result::<_, Error>::Ok(bcs)
    }))
    .buffered(len)
    .try_collect::<Vec<_>>()
    .await?;

    for (index, bcs) in gitimport_changesets.iter().enumerate() {
        let bcs_id = bcs.get_changeset_id();

        // repo_import doesn't support submodule expansion
        let submodule_expansion_data = None;

        let rewritten_bcs_opt = rewrite_commit(
            ctx,
            bcs.clone().into_mut(),
            &remapped_parents,
            movers.clone(),
            repo,
            Default::default(),
            Default::default(),
            submodule_expansion_data,
        )
        .await?;

        if let Some(rewritten_bcs_mut) = rewritten_bcs_opt.rewritten {
            let rewritten_bcs = rewritten_bcs_mut.freeze()?;
            let rewritten_bcs_id = rewritten_bcs.get_changeset_id();
            remapped_parents.insert(bcs_id.clone(), rewritten_bcs_id);
            info!(
                "Commit {}/{}: Remapped {:?} => {:?}",
                (index + 1),
                len,
                bcs_id,
                rewritten_bcs_id,
            );
            if *git_merge_bcs_id == bcs_id {
                git_merge_shifted_bcs_id = Some(rewritten_bcs_id);
            }
            bonsai_changesets.push(rewritten_bcs);
        }
    }

    bonsai_changesets = sort_bcs(&bonsai_changesets)?;
    let bcs_ids = get_cs_ids(&bonsai_changesets);
    info!("Saving shifted bonsai changesets");
    save_changesets(ctx, repo, bonsai_changesets).await?;
    info!("Saved shifted bonsai changesets");
    Ok((bcs_ids, git_merge_shifted_bcs_id))
}

async fn find_mapping_version(
    ctx: &CoreContext,
    large_to_small_syncer: &CommitSyncData<Repo>,
    dest_bookmark: &BookmarkKey,
) -> Result<Option<CommitSyncConfigVersion>, Error> {
    let bookmark_val = large_to_small_syncer
        .get_large_repo()
        .bookmarks()
        .get(ctx.clone(), dest_bookmark, bookmarks::Freshness::MostRecent)
        .await?
        .ok_or_else(|| format_err!("{dest_bookmark} not found"))?;

    wait_until_backsynced_and_return_version(ctx, large_to_small_syncer, bookmark_val).await
}

async fn back_sync_commits_to_small_repo(
    ctx: &CoreContext,
    small_repo: &Repo,
    large_to_small_syncer: &CommitSyncData<Repo>,
    bcs_ids: &[ChangesetId],
    version: &CommitSyncConfigVersion,
) -> Result<Vec<ChangesetId>, Error> {
    info!(
        "Back syncing from large repo {} to small repo {} using {:?} mapping version",
        large_to_small_syncer.get_large_repo().name(),
        small_repo.name(),
        version
    );

    let mut synced_bcs_ids = vec![];
    for bcs_id in bcs_ids {
        let (unsynced_ancestors, _) =
            find_toposorted_unsynced_ancestors(ctx, large_to_small_syncer, bcs_id.clone(), None)
                .await?;
        for ancestor in unsynced_ancestors {
            // It is always safe to use `CandidateSelectionHint::Only` in
            // the large-to-small direction
            let maybe_synced_cs_id = unsafe_sync_commit(
                ctx,
                ancestor,
                large_to_small_syncer,
                CandidateSelectionHint::Only,
                CommitSyncContext::RepoImport,
                Some(version.clone()),
                false, // add_mapping_to_hg_extra
            )
            .await?;

            if let Some(synced_cs_id) = maybe_synced_cs_id {
                info!("Synced large repo cs: {} => {}", bcs_id, synced_cs_id);
                synced_bcs_ids.push(synced_cs_id);
            }
        }
    }

    info!("Finished back syncing shifted bonsais");
    Ok(synced_bcs_ids)
}

async fn wait_until_backsynced_and_return_version(
    ctx: &CoreContext,
    large_to_small_syncer: &CommitSyncData<Repo>,
    cs_id: ChangesetId,
) -> Result<Option<CommitSyncConfigVersion>, Error> {
    let sleep_time_secs = 10;

    info!(
        "waiting until {} is backsynced from {} to {}...",
        cs_id,
        large_to_small_syncer.get_source_repo().name(),
        large_to_small_syncer.get_target_repo().name(),
    );

    // There's an option of actually running the backsync here instead of waiting,
    // but I'd rather leave it to the backsyncer job.
    for _ in 1..10 {
        let maybe_sync_outcome = large_to_small_syncer
            .get_commit_sync_outcome(ctx, cs_id)
            .await?;

        match maybe_sync_outcome {
            Some(sync_outcome) => {
                use CommitSyncOutcome::*;

                let maybe_version = match sync_outcome {
                    RewrittenAs(_, version) => Some(version),
                    EquivalentWorkingCopyAncestor(_, version) => Some(version),
                    NotSyncCandidate(_) => None,
                };

                return Ok(maybe_version);
            }
            None => {
                info!("sleeping for {} secs", sleep_time_secs);
                time::sleep(time::Duration::from_secs(sleep_time_secs)).await;
            }
        }
    }

    Err(format_err!(
        "{} hasn't been synced from {} to {} for too long, aborting",
        cs_id,
        large_to_small_syncer.get_source_repo().name(),
        large_to_small_syncer.get_target_repo().name(),
    ))
}

async fn derive_bonsais_single_repo(
    ctx: &CoreContext,
    repo: &Repo,
    bcs_ids: &[ChangesetId],
) -> Result<(), Error> {
    let derived_data_types = &repo
        .repo_derived_data()
        .active_config()
        .types
        .iter()
        .copied()
        .collect::<Vec<_>>();
    repo.repo_derived_data()
        .manager()
        .derive_bulk_locally(ctx, bcs_ids, None, derived_data_types, None, None)
        .map_err(|e| e.into())
        .await
}

async fn move_bookmark(
    ctx: &CoreContext,
    repo: &Repo,
    shifted_bcs_ids: &[ChangesetId],
    bookmark: &BookmarkKey,
    checker_flags: &CheckerFlags,
    maybe_call_sign: &Option<String>,
    maybe_small_repo_back_sync_vars: &Option<SmallRepoBackSyncVars>,
    recovery_fields: &mut RecoveryFields,
) -> Result<(), Error> {
    let batch_size = recovery_fields.batch_size;
    let sleep_time = recovery_fields.sleep_time;
    info!("Start moving the bookmark");
    if shifted_bcs_ids.is_empty() {
        return Err(format_err!("There is no bonsai changeset present"));
    }

    let first_csid = match shifted_bcs_ids.first() {
        Some(first) => first,
        None => {
            return Err(format_err!("There is no bonsai changeset present"));
        }
    };

    let maybe_old_csid = repo
        .bookmarks()
        .get(ctx.clone(), bookmark, bookmarks::Freshness::MostRecent)
        .await?;

    /* If the bookmark already exists, we should continue moving the
    bookmark from the last commit it points to */
    let mut old_csid = match maybe_old_csid {
        Some(ref id) => id,
        None => first_csid,
    };

    let mut transaction = repo.bookmarks().create_transaction(ctx.clone());
    if maybe_old_csid.is_none() {
        transaction.create(bookmark, old_csid.clone(), BookmarkUpdateReason::ManualMove)?;
        if transaction.commit().await?.is_none() {
            return Err(format_err!("Logical failure while creating {bookmark:?}"));
        }
        info!("Created bookmark {:?} pointing to {}", bookmark, old_csid);
    }

    let commits_done = recovery_fields.move_bookmark_commits_done;

    for chunk in shifted_bcs_ids
        .iter()
        .skip(commits_done)
        .enumerate()
        .chunks(batch_size)
        .into_iter()
    {
        transaction = repo.bookmarks().create_transaction(ctx.clone());
        let (shifted_index, curr_csid) = match chunk.last() {
            Some(tuple) => tuple,
            None => {
                return Err(format_err!("There is no bonsai changeset present"));
            }
        };
        transaction.update(
            bookmark,
            curr_csid.clone(),
            old_csid.clone(),
            BookmarkUpdateReason::ManualMove,
        )?;

        if transaction.commit().await?.is_none() {
            return Err(format_err!("Logical failure while setting {bookmark:?}"));
        }
        info!("Set bookmark {:?} to point to {:?}", bookmark, curr_csid);

        recovery_fields.move_bookmark_commits_done = commits_done + shifted_index;

        let check_repo = async move {
            let hg_csid = repo.derive_hg_changeset(ctx, curr_csid.clone()).await?;
            check_dependent_systems(ctx, checker_flags, hg_csid, sleep_time, maybe_call_sign)
                .await?;
            Result::<_, Error>::Ok(())
        };

        let check_small_repo = async move {
            let small_repo_back_sync_vars = match maybe_small_repo_back_sync_vars {
                Some(v) => v,
                None => return Ok(()),
            };

            info!("Back syncing bookmark movement to small repo");
            backsync_latest(
                ctx.clone(),
                small_repo_back_sync_vars.large_to_small_syncer.clone(),
                Arc::new(small_repo_back_sync_vars.target_repo_dbs.clone()),
                BacksyncLimit::NoLimit,
                Arc::new(AtomicBool::new(false)),
                CommitSyncContext::RepoImport,
                false,
                Box::new(future::ready(())),
            )
            .await?
            .1
            .await;
            let small_repo_cs_id = small_repo_back_sync_vars
                .small_repo
                .bookmarks()
                .get(
                    ctx.clone(),
                    &small_repo_back_sync_vars.small_repo_bookmark,
                    bookmarks::Freshness::MostRecent,
                )
                .await?
                .ok_or_else(|| {
                    format_err!(
                        "Couldn't extract backsynced changeset id from bookmark: {}",
                        small_repo_back_sync_vars.small_repo_bookmark
                    )
                })?;

            let small_repo_hg_csid = small_repo_back_sync_vars
                .small_repo
                .derive_hg_changeset(ctx, small_repo_cs_id)
                .await?;

            check_dependent_systems(
                ctx,
                checker_flags,
                small_repo_hg_csid,
                sleep_time,
                &small_repo_back_sync_vars.maybe_call_sign,
            )
            .await?;

            Result::<_, Error>::Ok(())
        };

        future::try_join(
            check_repo.map_err(|e| e.context("Error checking dependent systems")),
            check_small_repo
                .map_err(|e| e.context("Error checking dependent systems in small repository")),
        )
        .await?;
        old_csid = curr_csid;
    }
    info!("Finished moving the bookmark");
    Ok(())
}

async fn merge_imported_commit(
    ctx: &CoreContext,
    repo: &Repo,
    imported_cs_id: ChangesetId,
    dest_bookmark: &BookmarkKey,
    changeset_args: ChangesetArgs,
) -> Result<ChangesetId, Error> {
    info!(
        "Merging the imported commits into given bookmark, {}",
        dest_bookmark
    );
    let master_cs_id = match repo
        .bookmarks()
        .get(ctx.clone(), dest_bookmark, bookmarks::Freshness::MostRecent)
        .await?
    {
        Some(id) => id,
        None => {
            return Err(format_err!(
                "Couldn't extract changeset id from bookmark: {dest_bookmark}"
            ));
        }
    };
    let master_leaf_entries = get_leaf_entries(ctx, repo, master_cs_id).await?;

    let imported_leaf_entries = get_leaf_entries(ctx, repo, imported_cs_id).await?;

    let intersection: Vec<NonRootMPath> = imported_leaf_entries
        .intersection(&master_leaf_entries)
        .cloned()
        .collect();

    if !intersection.is_empty() {
        return Err(format_err!(
            "There are paths present in both parents: {intersection:?} ..."
        ));
    }

    info!("Done checking path conflicts");

    info!(
        "Creating a merge bonsai changeset with parents: {}, {}",
        master_cs_id, imported_cs_id
    );

    let ChangesetArgs {
        author,
        message,
        datetime,
    } = changeset_args;

    let merged_cs = BonsaiChangesetMut {
        parents: vec![master_cs_id, imported_cs_id],
        author: author.clone(),
        author_date: datetime,
        committer: Some(author.to_string()),
        committer_date: Some(datetime),
        message: message.to_string(),
        ..Default::default()
    }
    .freeze()?;

    let merged_cs_id = merged_cs.get_changeset_id();
    info!(
        "Created merge bonsai: {} and changeset: {:?}",
        merged_cs_id, merged_cs
    );

    save_changesets(ctx, repo, vec![merged_cs]).await?;
    info!("Finished merging");
    Ok(merged_cs_id)
}

async fn push_merge_commit(
    ctx: &CoreContext,
    repo: &Repo,
    merged_cs_id: ChangesetId,
    bookmark_to_merge_into: &BookmarkKey,
    repo_config: &RepoConfig,
) -> Result<ChangesetId, Error> {
    info!("Running pushrebase");

    let merged_cs = merged_cs_id.load(ctx, repo.repo_blobstore()).await?;
    let pushrebase_flags = &repo_config.pushrebase.flags;
    let pushrebase_hooks = bookmarks_movement::get_pushrebase_hooks(
        ctx,
        repo,
        bookmark_to_merge_into,
        &repo_config.pushrebase,
        None,
    )
    .await?;

    let pushrebase_res = do_pushrebase_bonsai(
        ctx,
        repo,
        pushrebase_flags,
        bookmark_to_merge_into,
        &hashset![merged_cs],
        &pushrebase_hooks,
        None,
    )
    .await?;

    let pushrebase_cs_id = pushrebase_res.head;
    info!("Finished pushrebasing to {}", pushrebase_cs_id);
    Ok(pushrebase_cs_id)
}

async fn get_leaf_entries(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
) -> Result<HashSet<NonRootMPath>, Error> {
    let hg_cs_id = repo.derive_hg_changeset(ctx, cs_id).await?;
    let hg_cs = hg_cs_id.load(ctx, repo.repo_blobstore()).await?;
    list_hg_leaf_entries(
        ctx,
        repo.repo_blobstore().clone(),
        repo.repo_identity().name(),
        hg_cs.manifestid(),
    )
    .map_ok(|(path, (_file_type, _filenode_id))| path)
    .try_collect::<HashSet<_>>()
    .await
}

async fn check_dependent_systems(
    _ctx: &CoreContext,
    checker_flags: &CheckerFlags,
    hg_csid: HgChangesetId,
    sleep_time: Duration,
    maybe_call_sign: &Option<String>,
) -> Result<(), Error> {
    // if a check is disabled, we have already passed the check
    let mut passed_phab_check = checker_flags.phab_check_disabled;
    let mut _passed_x_repo_check = checker_flags.x_repo_check_disabled;

    while !passed_phab_check {
        let call_sign = maybe_call_sign.as_ref().unwrap();
        passed_phab_check = phabricator_commit_check(call_sign, &hg_csid).await?;
        if !passed_phab_check {
            info!("Phabricator hasn't parsed commit: {:?}", hg_csid);
            time::sleep(sleep_time).await;
        }
    }

    Ok(())
}

async fn phabricator_commit_check(call_sign: &str, hg_csid: &HgChangesetId) -> Result<bool, Error> {
    let commit_id = format!("r{call_sign}{hg_csid}");
    let query = "query($commit: String!) {
                    differential_commit_query(query_params:{commits:[$commit]}) {
                        results {
                            nodes {
                                imported
                            }
                        }
                    }
                }";
    let variables = serde_json::to_string(&GraphqlInputVariables { commit: commit_id }).unwrap();
    let output = process::Command::new("jf")
        .arg("graphql")
        .arg("--query")
        .arg(query)
        .arg("--variables")
        .arg(variables)
        .output()
        .await?;
    if !output.status.success() {
        let e = format_err!(
            "Failed to fetch graphql commit: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return Err(e);
    }
    let query: GraphqlQueryObj = serde_json::from_str(&String::from_utf8_lossy(&output.stdout))?;
    let first_query = match query.differential_commit_query.first() {
        Some(first) => first,
        None => {
            return Err(format_err!(
                "No results were found when checking phabricator"
            ));
        }
    };
    let nodes = &first_query.results.nodes;
    let imported = match nodes.first() {
        Some(imp_obj) => imp_obj.imported,
        None => return Ok(false),
    };
    Ok(imported)
}

fn is_valid_bookmark_suffix(bookmark_suffix: &str) -> bool {
    let spec_chars = "./-_";
    bookmark_suffix
        .chars()
        .all(|c| c.is_alphanumeric() || spec_chars.contains(c))
}

fn sort_bcs(shifted_bcs: &[BonsaiChangeset]) -> Result<Vec<BonsaiChangeset>, Error> {
    let mut bcs_parents = HashMap::new();
    let mut id_bcs = HashMap::new();
    for bcs in shifted_bcs {
        let parents: Vec<_> = bcs.parents().collect();
        let bcs_id = bcs.get_changeset_id();
        bcs_parents.insert(bcs_id, parents);
        id_bcs.insert(bcs_id, bcs);
    }

    let sorted_commits = sort_topological(&bcs_parents).expect("loop in commit chain!");
    let mut sorted_bcs: Vec<BonsaiChangeset> = vec![];
    for csid in sorted_commits {
        match id_bcs.get(&csid) {
            Some(&bcs) => sorted_bcs.push(bcs.clone()),
            _ => {
                return Err(format_err!(
                    "Could not find mapping for changeset id {csid}"
                ));
            }
        }
    }
    Ok(sorted_bcs)
}

fn get_cs_ids(changesets: &[BonsaiChangeset]) -> Vec<ChangesetId> {
    let mut cs_ids = vec![];
    for bcs in changesets {
        cs_ids.push(bcs.get_changeset_id());
    }
    cs_ids
}

fn get_importing_bookmark(bookmark_suffix: &str) -> Result<BookmarkKey, Error> {
    BookmarkKey::new(format!("repo_import_{bookmark_suffix}"))
}

// Note: pushredirection only works from small repo to large repo.
//
// `repo_config_by_id` must be able to find a repo that has not been loaded
// yet: in tier mode `app.repo_configs()` only holds the repos opened so far.
async fn get_large_repo_config_if_pushredirected(
    ctx: &CoreContext,
    repo: &Repo,
    live_commit_sync_config: &CfgrLiveCommitSyncConfig,
    repo_config_by_id: impl Fn(RepositoryId) -> Result<RepoConfig, Error>,
) -> Result<Option<RepoConfig>, Error> {
    let repo_id = repo.repo_id();
    let enabled = live_commit_sync_config
        .push_redirector_enabled_for_public(ctx, repo_id)
        .await?;

    if enabled {
        let common_commit_sync_config = match live_commit_sync_config.get_common_config(repo_id) {
            Ok(config) => config,
            Err(e) => {
                return Err(format_err!(
                    "Failed to fetch common commit sync config: {e:#}"
                ));
            }
        };
        let large_repo_id = common_commit_sync_config.large_repo_id;
        let large_repo_config = repo_config_by_id(large_repo_id)
            .context("Couldn't fetch the large repo config we pushredirect into")?;
        return Ok(Some(large_repo_config));
    }
    Ok(None)
}

async fn get_large_repo_setting<R>(
    _ctx: &CoreContext,
    small_repo_setting: &RepoImportSetting,
    commit_sync_data: &CommitSyncData<R>,
) -> Result<RepoImportSetting, Error>
where
    R: CrossRepo,
{
    info!("Generating variables to import into large repo");

    let RepoImportSetting {
        importing_bookmark,
        dest_bookmark,
    } = small_repo_setting;

    let large_importing_bookmark =
        commit_sync_data
            .rename_bookmark(importing_bookmark).await?
            .ok_or_else(|| format_err!(
        "Bookmark {importing_bookmark:?} unexpectedly dropped in {commit_sync_data:?} when trying to generate large_importing_bookmark"
    ))?;
    info!(
        "Set large repo's importing bookmark to {}",
        large_importing_bookmark
    );
    let large_dest_bookmark = commit_sync_data
        .rename_bookmark(dest_bookmark).await?
        .ok_or_else(|| {
            format_err!(
        "Bookmark {dest_bookmark:?} unexpectedly dropped in {commit_sync_data:?} when trying to generate large_dest_bookmark"
    )
        })?;
    info!(
        "Set large repo's destination bookmark to {}",
        large_dest_bookmark
    );

    let large_repo_setting = RepoImportSetting {
        importing_bookmark: large_importing_bookmark,
        dest_bookmark: large_dest_bookmark,
    };
    info!("Finished generating the variables");
    Ok(large_repo_setting)
}

fn get_config_by_repoid(
    app: &MononokeApp,
    repo_id: RepositoryId,
) -> Result<(String, RepoConfig), Error> {
    // Route through `get_or_load_repo_config_by_id` so split-loaded repos
    // (only present in the per-tier RepoSpec manifest) resolve correctly.
    app.configs()
        .get_or_load_repo_config_by_id(repo_id.id())
        .with_context(|| format!("unknown repoid {repo_id:?}"))
}

async fn get_pushredirected_vars(
    app: &MononokeApp,
    ctx: &CoreContext,
    repo: &Repo,
    repo_import_setting: &RepoImportSetting,
    large_repo_config: &RepoConfig,
    live_commit_sync_config: CfgrLiveCommitSyncConfig,
) -> Result<(Repo, RepoImportSetting, Syncers<Repo>), Error> {
    let large_repo_id = large_repo_config.repoid;

    let repo_args = RepoArgs::from_repo_id(large_repo_id.id());
    let large_repo: Repo = app.open_repo(&repo_args).await?;

    let common_commit_sync_config = live_commit_sync_config.get_common_config(large_repo_id)?;

    if common_commit_sync_config.small_repos.len() > 1 {
        return Err(format_err!(
            "Currently repo_import tool doesn't support backsyncing into multiple small repos for large repo {:?}, name: {}",
            large_repo_id,
            large_repo.name()
        ));
    }

    let live_commit_sync_config = Arc::new(live_commit_sync_config);

    let repo_provider = repo_provider_from_mononoke_app(app);

    let repo_arc = Arc::new(repo.clone());
    let large_repo_arc = Arc::new(large_repo.clone());

    let submodule_deps =
        get_all_submodule_deps_from_repo_pair(ctx, repo_arc, large_repo_arc, repo_provider).await?;

    let syncers = create_commit_syncers(
        ctx,
        repo.clone(),
        large_repo.clone(),
        submodule_deps,
        live_commit_sync_config,
    )?;

    let large_repo_import_setting =
        get_large_repo_setting(ctx, repo_import_setting, &syncers.small_to_large).await?;
    Ok((large_repo, large_repo_import_setting, syncers))
}

async fn save_importing_state(recovery_fields: &RecoveryFields) -> Result<(), Error> {
    let mut proc_recovery_file = fs::File::create(&recovery_fields.recovery_file_path).await?;
    let serialized = serde_json::to_string_pretty(&recovery_fields)?;
    proc_recovery_file.write_all(serialized.as_bytes()).await?;
    Ok(())
}

async fn fetch_recovery_state(
    _ctx: &CoreContext,
    saved_recovery_file_paths: &str,
) -> Result<RecoveryFields, Error> {
    info!("Fetching the recovery stage for importing");
    let mut saved_proc_recovery_file = fs::File::open(saved_recovery_file_paths).await?;
    let mut serialized = String::new();
    saved_proc_recovery_file
        .read_to_string(&mut serialized)
        .await?;
    let recovery_fields: RecoveryFields = serde_json::from_str(&serialized)?;
    info!(
        "Fetched the recovery stage for importing.\nStarting from stage: {:?}",
        recovery_fields.import_stage
    );
    Ok(recovery_fields)
}

struct CombinedMover {
    movers: Vec<Arc<dyn Mover>>,
}

impl CombinedMover {
    fn new(movers: Vec<Arc<dyn Mover>>) -> Self {
        CombinedMover { movers }
    }
}

impl Mover for CombinedMover {
    fn move_path(&self, source_path: &NonRootMPath) -> Result<Option<NonRootMPath>, Error> {
        let mut path = source_path.clone();
        for mover in &self.movers {
            let maybe_path = mover.move_path(&path)?;
            path = match maybe_path {
                Some(moved_path) => moved_path,
                None => return Ok(None),
            };
        }
        Ok(Some(path))
    }

    fn conflicts_with(&self, path: &NonRootMPath) -> Result<bool, Error> {
        let mut path = path.clone();
        for mover in &self.movers {
            if mover.conflicts_with(&path)? {
                return Ok(true);
            }
            let maybe_path = mover.move_path(&path)?;
            path = match maybe_path {
                Some(moved_path) => moved_path,
                None => return Ok(false),
            };
        }
        Ok(false)
    }
}

async fn repo_import(
    app: &MononokeApp,
    ctx: CoreContext,
    mut repo: Repo,
    recovery_fields: &mut RecoveryFields,
    env: &MononokeEnvironment,
    no_merge: bool,
    git_command_path: Option<String>,
    lfs_args: LfsImportArgs,
) -> Result<(), Error> {
    let arg_git_repo_path = recovery_fields.git_repo_path.clone();
    let path = Path::new(&arg_git_repo_path);
    let dest_path_prefix = NonRootMPath::new(&recovery_fields.dest_path)?;
    let importing_bookmark = get_importing_bookmark(&recovery_fields.bookmark_suffix)?;
    if !is_valid_bookmark_suffix(&recovery_fields.bookmark_suffix) {
        return Err(format_err!(
            "The bookmark suffix contains invalid character(s).
                    You can only use alphanumeric and \"./-_\" characters"
        ));
    }

    let dest_bookmark = BookmarkKey::new(&recovery_fields.dest_bookmark_name)?;
    let changeset_args = ChangesetArgs {
        author: recovery_fields.commit_author.clone(),
        message: recovery_fields.commit_message.clone(),
        datetime: recovery_fields.datetime,
    };
    let mut repo_import_setting = RepoImportSetting {
        importing_bookmark,
        dest_bookmark,
    };

    let mut repo_config = repo.repo_config().clone();

    let mut call_sign = repo_config.phabricator_callsign.clone();
    if !recovery_fields.phab_check_disabled && call_sign.is_none() {
        return Err(format_err!(
            "The repo ({}) we import to doesn't have a callsign. \
                     Make sure the callsign for the repo is set in configerator: \
                     e.g CF/../source/scm/mononoke/repos/repos/hg.cinc",
            repo.name()
        ));
    }
    let checker_flags = CheckerFlags {
        phab_check_disabled: recovery_fields.phab_check_disabled,
        x_repo_check_disabled: recovery_fields.x_repo_check_disabled,
    };

    let live_commit_sync_config =
        CfgrLiveCommitSyncConfig::new(&env.config_store, repo.push_redirection_config_arc())?;

    check_megarepo_large_repo_import_requirements(
        &ctx,
        &repo,
        &live_commit_sync_config,
        &repo_import_setting.dest_bookmark,
        &dest_path_prefix,
        recovery_fields.mark_not_synced_mapping.as_deref(),
    )
    .await?;

    // Check if the import target is a small repo that is pushredirected to a
    // large repo.  In that case we will import to the large repo and then
    // backsync to the small repo.
    let maybe_large_repo_config =
        get_large_repo_config_if_pushredirected(&ctx, &repo, &live_commit_sync_config, |id| {
            app.repo_config(&RepoArg::Id(id)).map(|(_, config)| config)
        })
        .await?;
    let mut maybe_small_repo_back_sync_vars = None;
    let mover = Arc::new(CrossRepoMover::new(
        HashMap::new(),
        DefaultAction::PrependPrefix(dest_path_prefix),
    )?);

    let mut movers: Vec<Arc<dyn Mover>> = vec![mover.clone()];

    if let Some(large_repo_config) = maybe_large_repo_config {
        let (large_repo, large_repo_import_setting, syncers) = get_pushredirected_vars(
            app,
            &ctx,
            &repo,
            &repo_import_setting,
            &large_repo_config,
            live_commit_sync_config.clone(),
        )
        .await?;

        let target_repo_dbs = open_backsyncer_dbs(&repo).await?;

        let maybe_version = find_mapping_version(
            &ctx,
            &syncers.large_to_small,
            &large_repo_import_setting.dest_bookmark,
        )
        .await?;
        let version = maybe_version.ok_or_else(|| {
            format_err!(
                "cannot import into large repo {} because can't find mapping for {} bookmark",
                large_repo.name(),
                large_repo_import_setting.dest_bookmark,
            )
        })?;

        movers.push(
            syncers
                .small_to_large
                .get_movers_by_version(&version)
                .await?
                .mover,
        );

        maybe_small_repo_back_sync_vars = Some(SmallRepoBackSyncVars {
            large_to_small_syncer: syncers.large_to_small.clone(),
            target_repo_dbs,
            small_repo_bookmark: repo_import_setting.importing_bookmark.clone(),
            small_repo: repo.clone(),
            maybe_call_sign: call_sign.clone(),
            version,
        });

        repo_import_setting = large_repo_import_setting;
        repo = large_repo;
        repo_config = large_repo_config;
        call_sign = repo_config.phabricator_callsign.clone();
        if !recovery_fields.phab_check_disabled && call_sign.is_none() {
            return Err(format_err!(
                "Repo ({}) we push-redirect to doesn't have a callsign. \
                         Make sure the callsign for the repo is set in configerator: \
                         e.g CF/../source/scm/mononoke/repos/repos/hg.cinc",
                repo.name()
            ));
        }
    }

    let combined_mover = Arc::new(CombinedMover::new(movers));

    let combined_movers = Movers {
        mover: combined_mover.clone(),
        reverse_mover: Arc::new(InvalidReverseMover),
    };

    // Importing process starts here
    if recovery_fields.import_stage == ImportStage::GitImport {
        // LFS resolution mode (only relevant when the destination repo's
        // config has `git_lfs_interpret_pointers = true`). Mirrors the
        // gitimport / git_server precedence: pass `--lfs-server URL` to opt
        // into upstream HTTP fetches; otherwise resolve from the local
        // Mononoke filestore by SHA256 alias. The `--internal-lfs` flag is
        // documentary — its absence already selects internal mode when
        // `--lfs-server` is unset.
        let lfs = if repo.repo_config().git_configs.git_lfs_interpret_pointers {
            match lfs_args.lfs_server {
                Some(lfs_server) => {
                    let url_format = match lfs_args.lfs_server_url_format {
                        LfsServerUrlFormatArg::Dewey => LfsServerUrlFormat::LegacyDewey,
                        LfsServerUrlFormatArg::MononokeGitLfs => {
                            LfsServerUrlFormat::MononokeGitLfs {
                                repo_name: repo.name().to_string(),
                            }
                        }
                    };
                    GitImportLfs::new(
                        lfs_server,
                        url_format,
                        lfs_args.allow_dangling_lfs_pointers,
                        lfs_args.lfs_import_max_attempts,
                        Some(LFS_SIMULTANEOUS_CONNECTION_LIMIT),
                        lfs_args.tls_args,
                    )?
                }
                None => GitImportLfs::new_internal(
                    repo.repo_blobstore().boxed(),
                    lfs_args.allow_dangling_lfs_pointers,
                ),
            }
        } else {
            GitImportLfs::new_disabled()
        };

        // Import without submodules.
        let mut prefs = GitimportPreferences {
            submodules: false,
            lfs,
            ..Default::default()
        };

        if let Some(ref p) = git_command_path {
            prefs.git_command_path = PathBuf::from(p);
        }

        let target = GitimportTarget::full();
        info!("Started importing git commits to Mononoke");
        let uploader = import_direct::DirectUploader::new(repo.clone(), ReuploadCommits::Never);
        let import_map =
            import_tools::gitimport(&ctx, path, Arc::new(uploader), &target, &prefs).await?;
        info!("Added commits to Mononoke");

        if recovery_fields.print_gitimport_map {
            info!("Gitimport Map (git commit id -> bonsai changeset id):\n{import_map:#?}");
        }

        let git_merge_oid = {
            let mut child = process::Command::new(&prefs.git_command_path)
                .current_dir(path)
                .env_clear()
                .kill_on_drop(false)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .arg("rev-parse")
                .arg("--verify")
                .arg("--end-of-options")
                .arg(format!("{}^{{commit}}", recovery_fields.git_merge_rev_id))
                .spawn()?;
            let stdout = BufReader::new(child.stdout.take().context("stdout not set up")?);
            let mut lines = stdout.lines();
            if let Some(line) = lines.next_line().await? {
                gix_hash::ObjectId::from_hex(line.as_bytes())
                    .context("Parsing git rev-parse output")?
            } else {
                bail!("No lines returned by git rev-parse");
            }
        };

        let git_merge_bcs_id = match import_map.get(&git_merge_oid) {
            Some(a) => a.clone(),
            None => return Err(format_err!("Git commit doesn't exist")),
        };

        let gitimport_bcs_ids: Vec<ChangesetId> = import_map.values().cloned().collect();

        recovery_fields.git_merge_bcs_id = Some(git_merge_bcs_id);
        recovery_fields.import_stage = ImportStage::RewritePaths;
        recovery_fields.gitimport_bcs_ids = Some(gitimport_bcs_ids);
        save_importing_state(recovery_fields).await?;
    }

    if recovery_fields.import_stage == ImportStage::RewritePaths {
        let gitimport_bcs_ids = recovery_fields
            .gitimport_bcs_ids
            .as_ref()
            .ok_or_else(|| format_err!("gitimported changeset ids are not found"))?;
        let git_merge_bcs_id = recovery_fields
            .git_merge_bcs_id
            .as_ref()
            .ok_or_else(|| format_err!("gitimported changeset ids are not found"))?;

        let (shifted_bcs_ids, git_merge_shifted_bcs_id) = rewrite_file_paths(
            &ctx,
            &repo,
            &combined_movers,
            gitimport_bcs_ids,
            git_merge_bcs_id,
        )
        .await?;

        let imported_cs_id = match git_merge_shifted_bcs_id {
            Some(bcs_id) => bcs_id,
            None => {
                return Err(format_err!(
                    "There is no bonsai changeset corresponding for the git commit to be merged"
                ));
            }
        };

        recovery_fields.import_stage = ImportStage::DeriveBonsais;
        recovery_fields.imported_cs_id = Some(imported_cs_id.clone());
        recovery_fields.shifted_bcs_ids = Some(shifted_bcs_ids);
        save_importing_state(recovery_fields).await?;
    }

    let shifted_bcs_ids = recovery_fields
        .shifted_bcs_ids
        .as_ref()
        .ok_or_else(|| format_err!("Changeset ids after rewriting the file paths are not found"))?
        .clone();

    if recovery_fields.import_stage == ImportStage::DeriveBonsais {
        let derive_changesets = derive_bonsais_single_repo(&ctx, &repo, &shifted_bcs_ids);

        let backsync_and_derive_changesets = {
            borrowed!(ctx, shifted_bcs_ids, maybe_small_repo_back_sync_vars);

            async move {
                let vars = match maybe_small_repo_back_sync_vars {
                    Some(vars) => {
                        info!("Backsyncing changesets");
                        vars
                    }
                    None => return Ok(()),
                };

                let small_repo = &vars.small_repo;
                let synced_bcs_ids = back_sync_commits_to_small_repo(
                    ctx,
                    small_repo,
                    &vars.large_to_small_syncer,
                    shifted_bcs_ids,
                    &vars.version,
                )
                .await?;

                derive_bonsais_single_repo(ctx, small_repo, &synced_bcs_ids).await?;
                Ok(())
            }
        };

        let mark_not_synced_changesets = {
            borrowed!(ctx, repo, shifted_bcs_ids: &[ChangesetId]);
            let mark_not_synced_mapping = &recovery_fields.mark_not_synced_mapping;

            async move {
                if let Some(version) = mark_not_synced_mapping {
                    let version = CommitSyncConfigVersion(version.to_string());
                    let repo_id = repo.repo_identity().id();
                    let mapping = repo.synced_commit_mapping();

                    for cs_id in shifted_bcs_ids {
                        mapping
                            .insert_large_repo_commit_version(ctx, repo_id, *cs_id, &version)
                            .await?;
                    }
                }
                Ok(())
            }
        };

        info!("Start deriving data types");
        future::try_join3(
            derive_changesets,
            backsync_and_derive_changesets,
            mark_not_synced_changesets,
        )
        .await?;
        info!("Finished deriving data types");

        recovery_fields.import_stage = ImportStage::MoveBookmark;
        save_importing_state(recovery_fields).await?;
    }

    let imported_cs_id = recovery_fields
        .imported_cs_id
        .ok_or_else(|| format_err!("Imported changeset id is not found"))?;

    if recovery_fields.import_stage == ImportStage::MoveBookmark {
        move_bookmark(
            &ctx,
            &repo,
            &shifted_bcs_ids,
            &repo_import_setting.importing_bookmark,
            &checker_flags,
            &call_sign,
            &maybe_small_repo_back_sync_vars,
            recovery_fields,
        )
        .await?;

        recovery_fields.import_stage = ImportStage::MergeCommits;
        save_importing_state(recovery_fields).await?;
    }

    if no_merge {
        return Err(format_err!(
            "Done everything but actual merge. Please resume without --no-merge flag"
        ));
    }

    if recovery_fields.import_stage == ImportStage::MergeCommits {
        let maybe_merged_cs_id = Some(
            merge_imported_commit(
                &ctx,
                &repo,
                imported_cs_id.clone(),
                &repo_import_setting.dest_bookmark,
                changeset_args,
            )
            .await?,
        );

        recovery_fields.import_stage = ImportStage::PushCommit;
        recovery_fields.merged_cs_id = maybe_merged_cs_id;
        save_importing_state(recovery_fields).await?;
    }

    let merged_cs_id = recovery_fields
        .merged_cs_id
        .ok_or_else(|| format_err!("Changeset id for the merged commit is not found"))?;
    let pushrebased_cs_id = push_merge_commit(
        &ctx,
        &repo,
        merged_cs_id,
        &repo_import_setting.dest_bookmark,
        &repo_config,
    )
    .await?;

    let old_csid = repo
        .bookmarks()
        .get(
            ctx.clone(),
            &repo_import_setting.importing_bookmark,
            bookmarks::Freshness::MostRecent,
        )
        .await?
        .expect("The importing_bookmark should be set");

    let mut transaction = repo.bookmarks().create_transaction(ctx.clone());
    transaction.update(
        &repo_import_setting.importing_bookmark,
        pushrebased_cs_id.clone(),
        old_csid.clone(),
        BookmarkUpdateReason::ManualMove,
    )?;

    if transaction.commit().await?.is_none() {
        return Err(format_err!(
            "Logical failure while setting {:?} to the merge commit",
            repo_import_setting.importing_bookmark,
        ));
    }

    info!(
        "Set bookmark {:?} to the merge commit: {:?}",
        &repo_import_setting.importing_bookmark, pushrebased_cs_id
    );

    Ok(())
}

async fn check_additional_setup_steps(
    app: &MononokeApp,
    ctx: CoreContext,
    repo: Repo,
    check_additional_setup_steps_args: &CheckAdditionalSetupStepsArgs,
    env: &MononokeEnvironment,
) -> Result<(), Error> {
    let bookmark_suffix = check_additional_setup_steps_args.bookmark_suffix.as_str();
    if !is_valid_bookmark_suffix(bookmark_suffix) {
        return Err(format_err!(
            "The bookmark suffix contains invalid character(s).
                    You can only use alphanumeric and \"./-_\" characters"
        ));
    }
    let importing_bookmark = get_importing_bookmark(bookmark_suffix)?;
    info!(
        "The importing bookmark name is: {}. \
        Make sure to notify Phabricator oncall to track this bookmark!",
        importing_bookmark
    );
    let dest_bookmark_name = check_additional_setup_steps_args.dest_bookmark.as_str();
    let dest_bookmark = BookmarkKey::new(dest_bookmark_name)?;
    info!(
        "The destination bookmark name is: {}. \
        If the bookmark doesn't exist already, make sure to notify Phabricator oncall to track it!",
        dest_bookmark
    );

    let repo_import_setting = RepoImportSetting {
        importing_bookmark,
        dest_bookmark,
    };

    let (_, repo_config) = get_config_by_repoid(app, repo.repo_id())?;

    let call_sign = repo_config.phabricator_callsign;
    let phab_check_disabled = check_additional_setup_steps_args.disable_phabricator_check;
    if !phab_check_disabled && call_sign.is_none() {
        return Err(format_err!(
            "The repo ({}) we import to doesn't have a callsign for checking the commits on Phabricator. \
                     Make sure the callsign for the repo is set in configerator: \
                     e.g CF/../source/scm/mononoke/repos/repos/hg.cinc",
            repo.name()
        ));
    }

    let live_commit_sync_config =
        CfgrLiveCommitSyncConfig::new(&env.config_store, repo.push_redirection_config_arc())?;

    let maybe_large_repo_config =
        get_large_repo_config_if_pushredirected(&ctx, &repo, &live_commit_sync_config, |id| {
            app.repo_config(&RepoArg::Id(id)).map(|(_, config)| config)
        })
        .await?;
    if let Some(large_repo_config) = maybe_large_repo_config {
        let (large_repo, large_repo_import_setting, _syncers) = get_pushredirected_vars(
            app,
            &ctx,
            &repo,
            &repo_import_setting,
            &large_repo_config,
            live_commit_sync_config,
        )
        .await?;
        info!(
            "The repo we import {} into pushredirects to another repo {}. \
            The importing bookmark of the pushredirected repo is {} and \
            the destination bookmark is {}. If they don't exist already,
            make sure to notify Phabricator oncall to track these bookmarks as well!",
            repo.name(),
            large_repo.name(),
            large_repo_import_setting.importing_bookmark,
            large_repo_import_setting.dest_bookmark,
        );

        let large_repo_call_sign = large_repo_config.phabricator_callsign;
        if !phab_check_disabled && large_repo_call_sign.is_none() {
            return Err(format_err!(
                "Repo ({}) we push-redirect to doesn't have a callsign for checking the commits on Phabricator. \
                         Make sure the callsign for the repo is set in configerator: \
                         e.g CF/../source/scm/mononoke/repos/repos/hg.cinc",
                large_repo.name()
            ));
        }
    } else {
        info!("There is no additional setup step needed!");
    }
    Ok(())
}

/// Check if the import target is a large repo.  If so, the import destination must not be
/// mapped to any small repos, and the caller needs to provide the name of the mapping to use
/// for marking the commits as not-sync.
///
/// If the destination is mapped to a small repo, the repo should be imported to that repo
/// instead.
///
/// It is not currently supported to import to a destination which maps to multiple repos,
/// partially maps to a small repo, or otherwise crosses megarepo boundaries.
async fn check_megarepo_large_repo_import_requirements(
    ctx: &CoreContext,
    repo: &Repo,
    live_commit_sync_config: &dyn LiveCommitSyncConfig,
    dest_bookmark: &BookmarkKey,
    dest_path_prefix: &NonRootMPath,
    mb_mark_not_synced_mapping: Option<&str>,
) -> Result<(), Error> {
    let dest_cs_id = repo
        .bookmarks()
        .get(ctx.clone(), dest_bookmark, bookmarks::Freshness::MostRecent)
        .await?
        .ok_or_else(|| anyhow!("Bookmark not found: {dest_bookmark}"))?;
    if let Some(version) = repo
        .synced_commit_mapping()
        .get_large_repo_commit_version(ctx, repo.repo_identity().id(), dest_cs_id)
        .await?
    {
        let commit_sync_config = live_commit_sync_config
            .get_commit_sync_config_by_version_if_exists(repo.repo_identity().id(), &version)
            .await?
            .ok_or_else(|| anyhow!("Couldn't find commit sync config version {version}"))?;
        for (small_repo_id, small_repo_config) in commit_sync_config.small_repos.iter() {
            match &small_repo_config.default_action {
                DefaultSmallToLargeCommitSyncPathAction::Preserve => {
                    // This small repo is overlaid with the large repo
                    // without a prefix.  We can't handle this case.
                    return Err(anyhow!(
                        "Not possible to import to a large repo with an unprefixed small repo"
                    ));
                }
                DefaultSmallToLargeCommitSyncPathAction::PrependPrefix(prefix) => {
                    if dest_path_prefix.is_prefix_of(prefix)
                        || prefix.is_prefix_of(dest_path_prefix)
                    {
                        return Err(anyhow!(
                            "Small repo {small_repo_id} default prefix {prefix} overlaps with import destination {dest_path_prefix}"
                        ));
                    }
                }
            }

            for (small_repo_prefix, large_repo_prefix) in small_repo_config.map.iter() {
                if dest_path_prefix.is_prefix_of(large_repo_prefix)
                    || large_repo_prefix.is_prefix_of(dest_path_prefix)
                {
                    return Err(anyhow!(
                        "Small repo {small_repo_id} mapped prefix {small_repo_prefix} -> {large_repo_prefix} overlaps with import destination {dest_path_prefix}"
                    ));
                }
            }
        }

        // If we are importing into a large repo, we need to mark all the imported as
        // "not-synced", which means we need the name of a mapping that contains only
        // the large repo.
        let mark_not_synced_mapping = mb_mark_not_synced_mapping.ok_or_else(|| {
            anyhow!(concat!(
                "You are importing into a large repo without a large-only mapping.  ",
                "Please specify one with '--mark-not-synced-mapping'.",
            ))
        })?;

        // We also need to check if the provided mapping actually exists
        let mark_not_synced_mapping = CommitSyncConfigVersion(mark_not_synced_mapping.to_string());
        let mark_not_synced_mapping_config = live_commit_sync_config
            .get_commit_sync_config_by_version_if_exists(
                repo.repo_identity().id(),
                &mark_not_synced_mapping,
            )
            .await?
            .ok_or_else(|| {
                anyhow!("Couldn't find commit sync config version {mark_not_synced_mapping}",)
            })?;

        // And that it's indeed a large-only mapping
        if !mark_not_synced_mapping_config.small_repos.is_empty() {
            return Err(anyhow!(
                "The provided mapping {mark_not_synced_mapping} is not a large-only mapping"
            ));
        }
    } else if mb_mark_not_synced_mapping.is_some() {
        return Err(anyhow!(concat!(
            "You specified '--mark-not-synced-mapping' but are not importing into a ",
            "large repo.  This is invalid.",
        )));
    }

    Ok(())
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<(), Error> {
    let app = MononokeAppBuilder::new(fb)
        .with_app_extension(MonitoringAppExtension {})
        .build::<MononokeRepoImportArgs>()?;

    app.run_with_monitoring_and_logging(async_main, "repo_import", AliveService)
}

async fn async_main(app: MononokeApp) -> Result<(), Error> {
    let args: MononokeRepoImportArgs = app.args()?;
    let env = app.environment();
    let ctx = app.new_basic_context();

    let repo: Repo = app.open_repo(&args.repo).await?;
    info!("using repo \"{}\" repoid {:?}", repo.name(), repo.repo_id());
    let mut recovery_fields = match args.command {
        Some(CheckAdditionalSetupSteps(check_additional_setup_steps_args)) => {
            check_additional_setup_steps(&app, ctx, repo, &check_additional_setup_steps_args, env)
                .await?;
            return Ok(());
        }
        Some(RecoverProcess(recover_process_args)) => {
            fetch_recovery_state(&ctx, recover_process_args.saved_recovery_file_path.as_str())
                .await?
        }
        Some(Import(import_args)) => setup_import_args(import_args),
        _ => return Err(format_err!("Invalid subcommand")),
    };

    match repo_import(
        &app,
        ctx,
        repo,
        &mut recovery_fields,
        env,
        args.no_merge,
        args.git_command_path,
        args.lfs_args,
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(e) => {
            save_importing_state(&recovery_fields).await?;
            Err(e)
        }
    }
}

pub(crate) struct InvalidReverseMover;

impl Mover for InvalidReverseMover {
    fn move_path(&self, _source_path: &NonRootMPath) -> Result<Option<NonRootMPath>, Error> {
        Err(anyhow!(
            "Reverse mover should never be called for repo_import tool"
        ))
    }

    fn conflicts_with(&self, _path: &NonRootMPath) -> Result<bool, Error> {
        Err(anyhow!(
            "Reverse mover should never be called for repo_import tool"
        ))
    }
}
