/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Error;
use anyhow::format_err;
use backsyncer::BacksyncLimit;
use backsyncer::Repo;
use backsyncer::backsync_deleted_by_prefix;
use backsyncer::backsync_latest_by_prefix;
use backsyncer::backsync_once_by_prefix;
use backsyncer::open_backsyncer_dbs;
use blobrepo_hg::BlobRepoHg;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkPrefix;
use cloned::cloned;
use cmdlib_cross_repo::create_single_direction_commit_syncer;
use context::CoreContext;
use cross_repo_sync::CandidateSelectionHint;
use cross_repo_sync::CommitSyncContext;
use cross_repo_sync::CommitSyncData;
use cross_repo_sync::CommitSyncOutcome;
use cross_repo_sync::sync_commit;
use futures::future;
use futures::future::FutureExt;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use live_commit_sync_config::LiveCommitSyncConfig;
use mercurial_derivation::DeriveHgChangeset;
use mercurial_types::HgChangesetId;
use mononoke_app::MononokeApp;
use mononoke_types::ChangesetId;
use mononoke_types::RepositoryId;
use repo_identity::RepoIdentityRef;
use stats::prelude::*;
use tracing::debug;
use tracing::error;
use tracing::info;
use wireproto_handler::TargetRepoDbs;

use crate::cli::BacksyncerArgs;
use crate::cli::BacksyncerCommand;
use crate::cli::CommitsCommandArgs;

const PREFIX_POLLING_BOOKMARK_CONCURRENCY: usize = 10;

define_stats! {
    prefix = "mononoke.backsyncer";
    remaining_entries: dynamic_singleton_counter(
        "{}.{}.remaining_entries",
        (source_repo_name: String, target_repo_name: String)
    ),
    delay_secs: dynamic_singleton_counter(
        "{}.{}.delay_secs",
        (source_repo_name: String, target_repo_name: String)
    ),
    failed_bookmarks: dynamic_singleton_counter(
        "{}.{}.failed_bookmarks",
        (source_repo_name: String, target_repo_name: String)
    ),
    prefix_config_failure: dynamic_singleton_counter(
        "{}.{}.prefix_config_failure",
        (source_repo_name: String, target_repo_name: String)
    ),
}

pub(crate) async fn run_backsyncer(
    ctx: Arc<CoreContext>,
    app: Arc<MononokeApp>,

    large_repo: Repo,
    small_repo: Repo,
    cancellation_requested: Arc<AtomicBool>,
) -> Result<(), Error> {
    let args: BacksyncerArgs = app.args()?;
    let commit_sync_data =
        create_single_direction_commit_syncer(&ctx, &app, large_repo.clone(), small_repo.clone())
            .await?;

    info!(
        "syncing from repoid {:?} into repoid {:?}",
        large_repo.repo_identity().id(),
        small_repo.repo_identity().id(),
    );

    let live_commit_sync_config = commit_sync_data.live_commit_sync_config.clone();

    match args.command {
        BacksyncerCommand::Once => {
            let target_repo_dbs = Arc::new(
                open_backsyncer_dbs(commit_sync_data.get_target_repo())
                    .boxed()
                    .await?,
            );

            let (bookmark_prefix, common_bookmarks) = prefix_polling_config(
                &live_commit_sync_config,
                commit_sync_data.get_target_repo_id(),
            )?;
            let delay = backsync_once_by_prefix(
                ctx.as_ref().clone(),
                commit_sync_data,
                target_repo_dbs,
                bookmark_prefix,
                &common_bookmarks,
                PREFIX_POLLING_BOOKMARK_CONCURRENCY,
                cancellation_requested,
                CommitSyncContext::Backsyncer,
                false,
            )
            .await?;
            if delay.failed_bookmarks > 0 {
                return Err(format_err!(
                    "{} bookmark workers failed during one-shot backsync",
                    delay.failed_bookmarks,
                ));
            }
        }
        BacksyncerCommand::Forever => {
            let target_repo_dbs = Arc::new(
                open_backsyncer_dbs(commit_sync_data.get_target_repo())
                    .boxed()
                    .await?,
            );

            let sync_future = backsync_forever(
                ctx.as_ref(),
                commit_sync_data.clone(),
                target_repo_dbs.clone(),
                large_repo.repo_identity().name().to_string(),
                small_repo.repo_identity().name().to_string(),
                Arc::clone(&live_commit_sync_config),
                Arc::clone(&cancellation_requested),
            )
            .boxed();
            let deletion_future = backsync_deletions_forever(
                ctx.as_ref(),
                commit_sync_data,
                target_repo_dbs,
                large_repo.repo_identity().name().to_string(),
                small_repo.repo_identity().name().to_string(),
                live_commit_sync_config,
                cancellation_requested,
            )
            .map(Ok::<(), Error>)
            .boxed();
            future::try_join(sync_future, deletion_future).await?;
        }
        BacksyncerCommand::Commits(CommitsCommandArgs {
            input_file,
            batch_size: mb_batch_size,
        }) => {
            let inputfile = File::open(input_file)?;
            let file = BufReader::new(&inputfile);
            let batch_size = mb_batch_size.unwrap_or(100);

            let source_repo = commit_sync_data.get_source_repo().clone();

            let mut hg_cs_ids = vec![];
            for line in file.lines() {
                hg_cs_ids.push(HgChangesetId::from_str(&line?)?);
            }
            let total_to_backsync = hg_cs_ids.len();
            info!("backsyncing {} commits", total_to_backsync);

            let commit_sync_data = &commit_sync_data;

            // Before processing each commit, check if cancellation has
            // been requested and exit if that's the case.
            if cancellation_requested.load(Ordering::Relaxed) {
                info!("sync stopping due to cancellation request");
                return Ok(());
            }
            let f = stream::iter(hg_cs_ids.clone())
                .chunks(batch_size)
                .map(Result::<_, Error>::Ok)
                .and_then({
                    cloned!(ctx);
                    move |chunk| {
                        cloned!(ctx, source_repo);
                        async move {
                            source_repo
                                .get_hg_bonsai_mapping(ctx.as_ref().clone(), chunk)
                                .await
                        }
                    }
                })
                .try_fold(0, move |backsynced_so_far, hg_bonsai_mapping| {
                    hg_bonsai_mapping
                        .into_iter()
                        .map({
                            cloned!(ctx);
                            move |(_, bonsai)| {
                                cloned!(ctx);
                                async move {
                                    // Backsyncer is always used in the large-to-small direction,
                                    // therefore there can be at most one remapped candidate,
                                    // so `CandidateSelectionHint::Only` is a safe choice

                                    sync_commit(
                                        ctx.as_ref(),
                                        bonsai.clone(),
                                        commit_sync_data,
                                        CandidateSelectionHint::Only,
                                        CommitSyncContext::Backsyncer,
                                        false,
                                    )
                                    .await?;

                                    let maybe_sync_outcome = commit_sync_data
                                        .get_commit_sync_outcome(&ctx, bonsai)
                                        .await?;

                                    info!("{} backsynced as {:?}", bonsai, maybe_sync_outcome);

                                    let maybe_target_cs_id = extract_cs_id_from_sync_outcome(
                                        bonsai,
                                        maybe_sync_outcome,
                                    )?;

                                    derive_target_hg_changesets(
                                        &ctx,
                                        maybe_target_cs_id,
                                        commit_sync_data,
                                    )
                                    .await
                                }
                            }
                        })
                        .collect::<stream::futures_unordered::FuturesUnordered<_>>()
                        .try_fold(backsynced_so_far, {
                            move |backsynced_so_far, _| async move {
                                info!(
                                    "backsynced so far {} out of {}",
                                    backsynced_so_far + 1,
                                    total_to_backsync
                                );
                                Ok::<_, Error>(backsynced_so_far + 1)
                            }
                        })
                });

            f.await?;
        }
    }

    Ok(())
}

fn prefix_polling_config(
    live_commit_sync_config: &Arc<dyn LiveCommitSyncConfig>,
    target_repo_id: RepositoryId,
) -> Result<(BookmarkPrefix, Vec<BookmarkKey>), Error> {
    let common_config = live_commit_sync_config.get_common_config(target_repo_id)?;
    let small_repo_config = common_config
        .small_repos
        .get(&target_repo_id)
        .ok_or_else(|| {
            format_err!("small repo {target_repo_id} is missing from commit sync config")
        })?;
    Ok((
        BookmarkPrefix::new_ascii(small_repo_config.bookmark_prefix.clone()),
        common_config.common_pushrebase_bookmarks.clone(),
    ))
}

async fn backsync_forever(
    ctx: &CoreContext,
    commit_sync_data: CommitSyncData<Repo>,
    target_repo_dbs: Arc<TargetRepoDbs>,
    source_repo_name: String,
    target_repo_name: String,
    live_commit_sync_config: Arc<dyn LiveCommitSyncConfig>,
    cancellation_requested: Arc<AtomicBool>,
) -> Result<(), Error> {
    let target_repo_id = commit_sync_data.get_target_repo_id();
    let mut prefix_config_failures = PrefixPollingFailureState::default();
    let mut prefix_polling_failures = PrefixPollingFailureState::default();
    let mut previous_failed_bookmarks = 0;

    loop {
        // Before initiating loop, check if cancellation has been
        // requested. If yes, then exit early.
        if cancellation_requested.load(Ordering::Relaxed) {
            info!("sync stopping due to cancellation request");
            return Ok(());
        }
        // We only care about public pushes because draft pushes are not in the bookmark
        // update log at all.
        let enabled = live_commit_sync_config
            .push_redirector_enabled_for_public(ctx, target_repo_id)
            .await?;

        if enabled {
            let paused = justknobs::eval(
                "scm/mononoke:cross_repo_pause_backsyncer",
                None,
                Some(&target_repo_name),
            );

            if paused {
                STATS::failed_bookmarks.set_value(
                    ctx.fb,
                    0,
                    (source_repo_name.clone(), target_repo_name.clone()),
                );
                STATS::prefix_config_failure.set_value(
                    ctx.fb,
                    0,
                    (source_repo_name.clone(), target_repo_name.clone()),
                );
                prefix_config_failures.reset();
                prefix_polling_failures.reset();
                previous_failed_bookmarks = 0;
                info!("Backsyncer paused by JustKnob for {}", target_repo_name);
                tokio::time::sleep(Duration::new(1, 0)).await;
            } else {
                let (bookmark_prefix, common_bookmarks) =
                    match prefix_polling_config(&live_commit_sync_config, target_repo_id) {
                        Ok(config) => {
                            prefix_config_failures.record_recovery(&format!(
                                "Prefix-polling config for {target_repo_name}"
                            ));
                            STATS::prefix_config_failure.set_value(
                                ctx.fb,
                                0,
                                (source_repo_name.clone(), target_repo_name.clone()),
                            );
                            config
                        }
                        Err(error) => {
                            STATS::prefix_config_failure.set_value(
                                ctx.fb,
                                1,
                                (source_repo_name.clone(), target_repo_name.clone()),
                            );
                            prefix_config_failures.record_failure(
                                ctx,
                                &source_repo_name,
                                &target_repo_name,
                                &format!("Loading prefix-polling config for {target_repo_name}"),
                                &error,
                            );
                            tokio::time::sleep(Duration::new(1, 0)).await;
                            continue;
                        }
                    };

                let delay_info = match backsync_latest_by_prefix(
                    ctx.clone(),
                    commit_sync_data.clone(),
                    target_repo_dbs.clone(),
                    bookmark_prefix,
                    &common_bookmarks,
                    BacksyncLimit::NoLimit,
                    PREFIX_POLLING_BOOKMARK_CONCURRENCY,
                    Arc::clone(&cancellation_requested),
                    CommitSyncContext::Backsyncer,
                    false,
                )
                .await
                {
                    Ok(delay_info) => {
                        prefix_polling_failures
                            .record_recovery(&format!("Prefix polling for {target_repo_name}"));
                        delay_info
                    }
                    Err(error) => {
                        prefix_polling_failures.record_failure(
                            ctx,
                            &source_repo_name,
                            &target_repo_name,
                            &format!("Prefix polling for {target_repo_name}"),
                            &error,
                        );
                        tokio::time::sleep(Duration::new(1, 0)).await;
                        continue;
                    }
                };

                STATS::failed_bookmarks.set_value(
                    ctx.fb,
                    delay_info.failed_bookmarks as i64,
                    (source_repo_name.clone(), target_repo_name.clone()),
                );

                let delay = Delay {
                    delay_secs: delay_info.delay_secs,
                    remaining_entries: delay_info.remaining_entries,
                };
                log_delay(ctx, &delay, &source_repo_name, &target_repo_name);

                if delay_info.failed_bookmarks == 0 && previous_failed_bookmarks > 0 {
                    info!(
                        "Bookmark workers recovered for {} -> {}",
                        source_repo_name, target_repo_name,
                    );
                }
                if delay_info.failed_bookmarks > 0 {
                    if previous_failed_bookmarks == 0 {
                        error!(
                            "{} bookmark workers failed for {} -> {}; retrying after a one-second delay",
                            delay_info.failed_bookmarks, source_repo_name, target_repo_name,
                        );
                    } else {
                        debug!(
                            "{} bookmark workers still failing for {} -> {}",
                            delay_info.failed_bookmarks, source_repo_name, target_repo_name,
                        );
                    }
                    previous_failed_bookmarks = delay_info.failed_bookmarks;
                    tokio::time::sleep(Duration::new(1, 0)).await;
                } else if delay.remaining_entries == 0 {
                    previous_failed_bookmarks = 0;
                    debug!("no entries remained");
                    tokio::time::sleep(Duration::new(1, 0)).await;
                } else {
                    previous_failed_bookmarks = 0;
                    debug!(
                        "backsyncing {} remaining entries (delay: {}s)",
                        delay.remaining_entries, delay.delay_secs
                    );
                }
            }
        } else {
            STATS::failed_bookmarks.set_value(
                ctx.fb,
                0,
                (source_repo_name.clone(), target_repo_name.clone()),
            );
            STATS::prefix_config_failure.set_value(
                ctx.fb,
                0,
                (source_repo_name.clone(), target_repo_name.clone()),
            );
            prefix_config_failures.reset();
            prefix_polling_failures.reset();
            previous_failed_bookmarks = 0;
            debug!("push redirector is disabled");
            let delay = Delay::no_delay();
            log_delay(ctx, &delay, &source_repo_name, &target_repo_name);
            tokio::time::sleep(Duration::new(1, 0)).await;
        }
    }
}

async fn backsync_deletions_forever(
    ctx: &CoreContext,
    commit_sync_data: CommitSyncData<Repo>,
    target_repo_dbs: Arc<TargetRepoDbs>,
    source_repo_name: String,
    target_repo_name: String,
    live_commit_sync_config: Arc<dyn LiveCommitSyncConfig>,
    cancellation_requested: Arc<AtomicBool>,
) {
    let target_repo_id = commit_sync_data.get_target_repo_id();
    let mut failures = PrefixPollingFailureState::default();
    let mut previous_failed_bookmarks = 0;

    loop {
        if cancellation_requested.load(Ordering::Relaxed) {
            info!("deleted-bookmark sync stopping due to cancellation request");
            return;
        }

        let enabled = match live_commit_sync_config
            .push_redirector_enabled_for_public(ctx, target_repo_id)
            .await
        {
            Ok(enabled) => enabled,
            Err(error) => {
                failures.record_failure(
                    ctx,
                    &source_repo_name,
                    &target_repo_name,
                    &format!("Checking push-redirection state for {target_repo_name}"),
                    &error,
                );
                tokio::time::sleep(Duration::new(1, 0)).await;
                continue;
            }
        };
        let paused = justknobs::eval(
            "scm/mononoke:cross_repo_pause_backsyncer",
            None,
            Some(&target_repo_name),
        );
        if !enabled || paused {
            failures.reset();
            previous_failed_bookmarks = 0;
            tokio::time::sleep(Duration::new(1, 0)).await;
            continue;
        }

        let (bookmark_prefix, common_bookmarks) =
            match prefix_polling_config(&live_commit_sync_config, target_repo_id) {
                Ok(config) => config,
                Err(error) => {
                    failures.record_failure(
                        ctx,
                        &source_repo_name,
                        &target_repo_name,
                        &format!("Loading deletion-polling config for {target_repo_name}"),
                        &error,
                    );
                    tokio::time::sleep(Duration::new(1, 0)).await;
                    continue;
                }
            };

        let delay_info = match backsync_deleted_by_prefix(
            ctx.clone(),
            commit_sync_data.clone(),
            target_repo_dbs.clone(),
            bookmark_prefix,
            &common_bookmarks,
            BacksyncLimit::NoLimit,
            PREFIX_POLLING_BOOKMARK_CONCURRENCY,
            Arc::clone(&cancellation_requested),
            CommitSyncContext::Backsyncer,
            false,
        )
        .await
        {
            Ok(delay_info) => {
                failures.record_recovery(&format!("Deletion polling for {target_repo_name}"));
                delay_info
            }
            Err(error) => {
                failures.record_failure(
                    ctx,
                    &source_repo_name,
                    &target_repo_name,
                    &format!("Deletion polling for {target_repo_name}"),
                    &error,
                );
                tokio::time::sleep(Duration::new(1, 0)).await;
                continue;
            }
        };

        if delay_info.failed_bookmarks == 0 && previous_failed_bookmarks > 0 {
            info!(
                "Deleted-bookmark workers recovered for {} -> {}",
                source_repo_name, target_repo_name,
            );
        }
        if delay_info.failed_bookmarks > 0 {
            if previous_failed_bookmarks == 0 {
                error!(
                    "{} deleted-bookmark workers failed for {} -> {}; retrying after a one-second delay",
                    delay_info.failed_bookmarks, source_repo_name, target_repo_name,
                );
            }
            previous_failed_bookmarks = delay_info.failed_bookmarks;
            tokio::time::sleep(Duration::new(1, 0)).await;
        } else if delay_info.remaining_entries == 0 {
            previous_failed_bookmarks = 0;
            tokio::time::sleep(Duration::new(1, 0)).await;
        } else {
            previous_failed_bookmarks = 0;
        }
    }
}

fn extract_cs_id_from_sync_outcome(
    source_cs_id: ChangesetId,
    maybe_sync_outcome: Option<CommitSyncOutcome>,
) -> Result<Option<ChangesetId>, Error> {
    use CommitSyncOutcome::*;

    match maybe_sync_outcome {
        Some(RewrittenAs(cs_id, _)) => Ok(Some(cs_id)),
        Some(NotSyncCandidate(_)) => Ok(None),
        Some(EquivalentWorkingCopyAncestor(cs_id, _)) => Ok(Some(cs_id)),
        None => Err(format_err!(
            "sync outcome is not available for {source_cs_id}"
        )),
    }
}

async fn derive_target_hg_changesets(
    ctx: &CoreContext,
    maybe_target_cs_id: Option<ChangesetId>,
    commit_sync_data: &CommitSyncData<Repo>,
) -> Result<(), Error> {
    match maybe_target_cs_id {
        Some(target_cs_id) => {
            let hg_cs_id = commit_sync_data
                .get_target_repo()
                .derive_hg_changeset(ctx, target_cs_id)
                .await?;
            info!("Hg cs id {} derived for {}", hg_cs_id, target_cs_id);
            Ok(())
        }
        None => Ok(()),
    }
}

struct Delay {
    delay_secs: i64,
    remaining_entries: u64,
}

impl Delay {
    fn no_delay() -> Self {
        Self {
            delay_secs: 0,
            remaining_entries: 0,
        }
    }
}

fn log_delay(ctx: &CoreContext, delay: &Delay, source_repo_name: &str, target_repo_name: &str) {
    STATS::remaining_entries.set_value(
        ctx.fb,
        delay.remaining_entries as i64,
        (source_repo_name.to_owned(), target_repo_name.to_owned()),
    );
    STATS::delay_secs.set_value(
        ctx.fb,
        delay.delay_secs,
        (source_repo_name.to_owned(), target_repo_name.to_owned()),
    );
}

fn record_prefix_polling_stall(ctx: &CoreContext, source_repo_name: &str, target_repo_name: &str) {
    // The precise prefix-scoped backlog is unavailable on an error path.
    // Mark the pair as failed and its backlog as non-empty, but retain the
    // last measured delay rather than replacing it with a misleading zero.
    STATS::failed_bookmarks.set_value(
        ctx.fb,
        1,
        (source_repo_name.to_owned(), target_repo_name.to_owned()),
    );
    STATS::remaining_entries.set_value(
        ctx.fb,
        1,
        (source_repo_name.to_owned(), target_repo_name.to_owned()),
    );
}

#[derive(Default)]
struct PrefixPollingFailureState {
    consecutive_failures: u64,
}

impl PrefixPollingFailureState {
    fn record_failure(
        &mut self,
        ctx: &CoreContext,
        source_repo_name: &str,
        target_repo_name: &str,
        operation: &str,
        error: &Error,
    ) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        record_prefix_polling_stall(ctx, source_repo_name, target_repo_name);
        if self.consecutive_failures.is_power_of_two() {
            error!(
                "{} failed {} consecutive time(s); retrying after a one-second delay: {:#}",
                operation, self.consecutive_failures, error
            );
        } else {
            debug!(
                "{} remains unavailable after {} consecutive failure(s): {:#}",
                operation, self.consecutive_failures, error
            );
        }
    }

    fn record_recovery(&mut self, operation: &str) {
        if self.consecutive_failures > 0 {
            info!(
                "{} recovered after {} consecutive failure(s)",
                operation, self.consecutive_failures
            );
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.consecutive_failures = 0;
    }
}
