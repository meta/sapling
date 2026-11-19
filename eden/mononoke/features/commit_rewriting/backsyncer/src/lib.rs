/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Backsyncer
//!
//! Library to sync commits from source repo to target repo by following bookmark update log
//! and doing commit rewrites. The main motivation for backsyncer is to keep "small repo" up to
//! date with "large repo" in a setup where all writes to small repo are redirected to large repo
//! in a push redirector.
//! More details can be found here - <https://fb.quip.com/tZ4yAaA3S4Mc>
//!
//! Target repo tails source repo's bookmark update log and backsync bookmark updates one by one.
//! The latest backsynced log id is stored in mutable_counters table. Backsync consists of the
//! following phases:
//!
//! 1) Given an entry from bookmark update log of a target repo,
//!    find commits to backsync from source repo into a target repo.
//! 2) Rewrite these commits and create rewritten commits in target repo
//! 3) In the same transaction try to update a bookmark in the source repo AND latest backsynced
//!    log id.

use std::collections::HashSet;
use std::iter::once;
use std::iter::repeat;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use anyhow::Error;
use anyhow::bail;
use anyhow::format_err;
use blobstore::Loadable;
use bonsai_git_mapping::BonsaiGitMapping;
use bonsai_globalrev_mapping::BonsaiGlobalrevMapping;
use bonsai_globalrev_mapping::BonsaiGlobalrevMappingEntry;
use bonsai_hg_mapping::BonsaiHgMapping;
use bookmarks::BookmarkCategory;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkKind;
use bookmarks::BookmarkPagination;
use bookmarks::BookmarkPrefix;
use bookmarks::BookmarkTransactionError;
use bookmarks::BookmarkUpdateLog;
use bookmarks::BookmarkUpdateLogArc;
use bookmarks::BookmarkUpdateLogEntry;
use bookmarks::BookmarkUpdateLogId;
use bookmarks::BookmarkUpdateLogRef;
use bookmarks::BookmarkUpdateReason;
use bookmarks::Bookmarks;
use bookmarks::BookmarksArc;
use bookmarks::BookmarksRef;
use bookmarks::Freshness;
use cloned::cloned;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphRef;
use commit_graph::CommitGraphWriter;
use context::CoreContext;
use cross_repo_sync::CandidateSelectionHint;
use cross_repo_sync::CommitSyncContext;
use cross_repo_sync::CommitSyncData;
use cross_repo_sync::CommitSyncOutcome;
use cross_repo_sync::find_toposorted_unsynced_ancestors;
use cross_repo_sync::get_bookmark_renamer;
use cross_repo_sync::sync_commit;
use filenodes::Filenodes;
use filestore::FilestoreConfig;
use futures::Future;
use futures::FutureExt;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future;
use futures::stream;
use futures_stats::TimedTryFutureExt;
use git_source_of_truth::GitSourceOfTruthConfig;
use metaconfig_types::RepoConfig;
use metaconfig_types::RepoConfigRef;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::Globalrev;
use mononoke_types::RepositoryId;
use mutable_counters::ArcMutableCounters;
use mutable_counters::MutableCounters;
use mutable_counters::MutableCountersArc;
use mutable_counters::SqlMutableCounters;
use mutable_counters::validate_counter_name;
use phases::Phases;
use pushrebase_mutation_mapping::PushrebaseMutationMapping;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use repo_bookmark_attrs::RepoBookmarkAttrs;
use repo_cross_repo::RepoCrossRepo;
use repo_derived_data::RepoDerivedData;
use repo_identity::RepoIdentity;
use repo_identity::RepoIdentityRef;
use repo_update_logger::BookmarkInfo;
use repo_update_logger::BookmarkOperation;
use repo_update_logger::CommitInfo;
use repo_update_logger::find_draft_ancestors;
use repo_update_logger::log_bookmark_operation;
use repo_update_logger::log_new_commits;
use scuba_ext::MononokeScubaSampleBuilder;
use sql_ext::Transaction;
use sql_ext::TransactionResult;
use sql_query_config::SqlQueryConfig;
use thiserror::Error;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;
use wireproto_handler::TargetRepoDbs;

const BOOKMARK_DISCOVERY_PAGE_SIZE_JUST_KNOB: &str =
    "scm/mononoke:backsyncer_bookmark_discovery_page_size";
const MAX_DISCOVERED_BOOKMARKS_JUST_KNOB: &str = "scm/mononoke:backsyncer_max_discovered_bookmarks";

#[derive(Clone)]
#[facet::container]
pub struct Repo(
    dyn BonsaiHgMapping,
    dyn BonsaiGitMapping,
    dyn BonsaiGlobalrevMapping,
    dyn PushrebaseMutationMapping,
    RepoCrossRepo,
    RepoBookmarkAttrs,
    dyn Bookmarks,
    dyn BookmarkUpdateLog,
    FilestoreConfig,
    dyn MutableCounters,
    dyn Phases,
    RepoBlobstore,
    RepoConfig,
    RepoDerivedData,
    RepoIdentity,
    CommitGraph,
    dyn CommitGraphWriter,
    dyn Filenodes,
    SqlQueryConfig,
    dyn GitSourceOfTruthConfig,
);

#[cfg(test)]
mod tests;

#[derive(Debug, Error)]
pub enum BacksyncError {
    #[error("BacksyncError::LogEntryNotFound: {latest_log_id} not found")]
    LogEntryNotFound { latest_log_id: u64 },
    #[error("BacksyncError::Other")]
    Other(#[from] Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BacksyncLimit {
    NoLimit,
    Limit(u64),
}

#[derive(Debug, Clone)]
pub struct BacksyncDelayInfo {
    pub delay_secs: i64,
    pub remaining_entries: u64,
    /// Bookmark workers whose backlog could not be measured because they failed.
    pub failed_bookmarks: u64,
}

/// Block until a specific bookmark transaction (identified by its log id) is confirmed to be
/// backsynced.
///
/// The backsyncer tw jobs are responsible for backsyncing bookmark transactions.
/// When they do, they update a mutable counter to the id found in the bookmark update log for this
/// transaction.
/// Wait until the matching bookmark counter indicates that backsyncing caught
/// up. A global cursor cannot prove name-local completion.
///
/// Note that we use some form of exponential backoff to avoid causing a thundering herd problem on
/// reading the mutable counters if the backsync is lagging
///
/// We also use a hard-coded timeout to avoid being stuck forever waiting for the backsync if it is
/// lagging. Not having this timeout has caused SEVs in the past, blocking lands.
pub async fn ensure_backsynced<R>(
    ctx: CoreContext,
    commit_sync_data: CommitSyncData<R>,
    target_repo_dbs: Arc<TargetRepoDbs>,
    bookmark: &BookmarkKey,
    log_id: BookmarkUpdateLogId,
) -> Result<(), Error>
where
    R: RepoLike + Send + Sync + Clone + 'static,
{
    let timeout = Duration::from_secs(justknobs::get_as::<u64>(
        "scm/mononoke:defer_to_backsyncer_for_backsync_timeout_seconds",
        None,
    ));

    let source_repo_id = commit_sync_data.get_source_repo().repo_identity().id();
    let counter_name = format_bookmark_counter(&source_repo_id, bookmark)?;
    let start_instant = Instant::now();

    let mut sleep_times = once(1)
        .chain(once(2))
        .chain(once(5))
        .chain(repeat(10))
        .map(Duration::from_secs);
    while start_instant.elapsed() < timeout {
        let counter: BookmarkUpdateLogId = target_repo_dbs
            .counters
            .get_counter(&ctx, &counter_name)
            .await?
            .unwrap_or(0)
            .try_into()?;
        if counter >= log_id {
            return Ok(());
        }
        tokio::time::sleep(
            sleep_times
                .next()
                .expect("sleep_times is an unbounded iterator"),
        )
        .await;
    }
    bail!("Timeout expired while waiting for backsyncing")
}

/// Backsync one explicitly selected source bookmark, including a stream whose
/// current source and target bookmarks are both absent.
///
/// Initialization races retry immediately for one-shot/eager callers. Errors
/// propagate; best-effort commit-only work remains chained in the returned
/// future and need not be awaited.
pub async fn backsync_bookmark_once<R>(
    ctx: CoreContext,
    commit_sync_data: CommitSyncData<R>,
    target_repo_dbs: Arc<TargetRepoDbs>,
    bookmark: BookmarkKey,
    limit: BacksyncLimit,
    cancellation_requested: Arc<AtomicBool>,
    sync_context: CommitSyncContext,
    disable_lease: bool,
    commit_only_backsync_future: Box<dyn Future<Output = ()> + Send + Unpin>,
) -> Result<
    (
        BacksyncDelayInfo,
        Box<dyn Future<Output = ()> + Send + Unpin>,
    ),
    Error,
>
where
    R: RepoLike + Send + Sync + Clone + 'static,
{
    let mut commit_only_backsync_future = commit_only_backsync_future;
    loop {
        if cancellation_requested.load(Ordering::Relaxed) {
            return Ok((
                BacksyncDelayInfo {
                    delay_secs: 0,
                    remaining_entries: 0,
                    failed_bookmarks: 0,
                },
                commit_only_backsync_future,
            ));
        }
        let (delay_info, future) = backsync_bookmark_through(
            ctx.clone(),
            commit_sync_data.clone(),
            target_repo_dbs.clone(),
            bookmark.clone(),
            limit,
            Arc::clone(&cancellation_requested),
            sync_context,
            disable_lease,
            commit_only_backsync_future,
        )
        .await?;
        commit_only_backsync_future = future;
        if let Some(delay_info) = delay_info {
            return Ok((delay_info, commit_only_backsync_future));
        }
    }
}

/// Read a bookmark cursor, or initialize it atomically from a legacy cursor.
///
/// The initial value is used only when the counter is absent. Once present,
/// the per-bookmark cursor is authoritative and is never fast-forwarded from
/// a newer legacy cursor.
///
/// A caller that loses the initialization race receives `None` and should
/// retry from the durable value on its next polling iteration.
pub async fn get_bookmark_counter_or_initialize(
    ctx: &CoreContext,
    counters: &ArcMutableCounters,
    counter_name: &str,
    initial_counter: BookmarkUpdateLogId,
) -> Result<Option<BookmarkUpdateLogId>, Error> {
    let Some(counter) = counters.get_counter(ctx, counter_name).await? else {
        let inserted = counters
            .set_counter_if_absent(ctx, counter_name, initial_counter.try_into()?)
            .await?;
        return Ok(inserted.then_some(initial_counter));
    };
    Ok(Some(counter.try_into()?))
}

/// Advance a bookmark cursor with compare-and-set semantics.
///
/// If another worker wins the race, update the caller's in-memory cursor to
/// the newly observed durable value. The caller can then skip any remaining
/// entries in its batch that are already covered.
pub async fn advance_bookmark_counter(
    ctx: &CoreContext,
    counters: &ArcMutableCounters,
    counter_name: &str,
    counter: &mut BookmarkUpdateLogId,
    target: BookmarkUpdateLogId,
) -> Result<(), Error> {
    while *counter < target {
        if counters
            .set_counter(
                ctx,
                counter_name,
                target.try_into()?,
                Some((*counter).try_into()?),
            )
            .await?
        {
            *counter = target;
            return Ok(());
        }

        let observed: BookmarkUpdateLogId = counters
            .get_counter(ctx, counter_name)
            .await?
            .ok_or_else(|| {
                format_err!(
                    "bookmark counter {counter_name} disappeared after a failed compare-and-set"
                )
            })?
            .try_into()?;
        if observed <= *counter {
            bail!(
                "failed to advance bookmark cursor {counter_name} to {target}; observed {observed}"
            );
        }
        *counter = observed;
    }
    Ok(())
}

/// Backsync all outstanding log entries for one bookmark.
///
/// Existing cursors remain authoritative. A missing cursor starts at zero only
/// if the mapped target has neither a current bookmark nor retained log history.
pub async fn backsync_latest_for_bookmark<R>(
    ctx: CoreContext,
    commit_sync_data: CommitSyncData<R>,
    target_repo_dbs: Arc<TargetRepoDbs>,
    bookmark: BookmarkKey,
    limit: BacksyncLimit,
    cancellation_requested: Arc<AtomicBool>,
    sync_context: CommitSyncContext,
    disable_lease: bool,
) -> Result<BacksyncDelayInfo, Error>
where
    R: RepoLike + Send + Sync + Clone + 'static,
{
    let (delay_info, future) = backsync_bookmark_through(
        ctx,
        commit_sync_data,
        target_repo_dbs,
        bookmark,
        limit,
        cancellation_requested,
        sync_context,
        disable_lease,
        Box::new(future::ready(())),
    )
    .await?;
    future.await;
    Ok(delay_info.unwrap_or(BacksyncDelayInfo {
        delay_secs: 0,
        remaining_entries: 1,
        failed_bookmarks: 0,
    }))
}

async fn get_backsync_bookmark_counter_or_initialize<R>(
    ctx: &CoreContext,
    commit_sync_data: &CommitSyncData<R>,
    target_repo_dbs: &TargetRepoDbs,
    bookmark: &BookmarkKey,
    counter_name: &str,
) -> Result<Option<BookmarkUpdateLogId>, Error>
where
    R: RepoLike + Send + Sync + Clone + 'static,
{
    if let Some(counter) = target_repo_dbs
        .counters
        .get_counter(ctx, counter_name)
        .await?
    {
        return Ok(Some(counter.try_into()?));
    }
    if let Some(target_bookmark) = commit_sync_data.rename_bookmark(bookmark).await? {
        let target_bookmark =
            BookmarkKey::with_name_and_category(target_bookmark.into_name(), *bookmark.category());
        let exists = target_repo_dbs
            .bookmarks
            .get(ctx.clone(), &target_bookmark, Freshness::MostRecent)
            .await?
            .is_some();
        let has_history = target_repo_dbs
            .bookmark_update_log
            .list_bookmark_log_entries(
                ctx.clone(),
                target_bookmark.clone(),
                1,
                None,
                Freshness::MostRecent,
            )
            .try_next()
            .await?
            .is_some();
        if exists || has_history {
            // A competing initializer may already have started processing.
            if target_repo_dbs
                .counters
                .get_counter(ctx, counter_name)
                .await?
                .is_some()
            {
                return Ok(None);
            }
            bail!(
                "missing backsync cursor {counter_name} for target bookmark {target_bookmark:?} with existing state or log history; explicit operator initialization/reconciliation required"
            );
        }
    }
    let inserted = target_repo_dbs
        .counters
        .set_counter_if_absent(ctx, counter_name, 0)
        .await?;
    Ok(inserted.then_some(BookmarkUpdateLogId(0)))
}

async fn backsync_bookmark_through<R>(
    ctx: CoreContext,
    commit_sync_data: CommitSyncData<R>,
    target_repo_dbs: Arc<TargetRepoDbs>,
    bookmark: BookmarkKey,
    limit: BacksyncLimit,
    cancellation_requested: Arc<AtomicBool>,
    sync_context: CommitSyncContext,
    disable_lease: bool,
    commit_only_backsync_future: Box<dyn Future<Output = ()> + Send + Unpin>,
) -> Result<
    (
        Option<BacksyncDelayInfo>,
        Box<dyn Future<Output = ()> + Send + Unpin>,
    ),
    Error,
>
where
    R: RepoLike + Send + Sync + Clone + 'static,
{
    let source_repo_id = commit_sync_data.get_source_repo().repo_identity().id();
    let counter_name = format_bookmark_counter(&source_repo_id, &bookmark)?;
    let Some(counter) = get_backsync_bookmark_counter_or_initialize(
        &ctx,
        &commit_sync_data,
        &target_repo_dbs,
        &bookmark,
        &counter_name,
    )
    .await?
    else {
        // The losing initializer must retry from the durable cursor. Daemon
        // polling returns a retry sentinel; one-shot callers retry immediately.
        return Ok((None, commit_only_backsync_future));
    };

    let log_entries_limit = match limit {
        BacksyncLimit::Limit(limit) => limit,
        BacksyncLimit::NoLimit => u64::MAX,
    };
    let entries: Vec<_> = commit_sync_data
        .get_source_repo()
        .bookmark_update_log()
        .read_next_bookmark_log_entries_by_bookmark(
            ctx.clone(),
            bookmark,
            counter,
            log_entries_limit,
            Freshness::MostRecent,
        )
        .boxed()
        .try_collect()
        .await?;
    let delay_info = BacksyncDelayInfo {
        delay_secs: entries
            .first()
            .map_or(0, |entry| entry.timestamp.since_seconds()),
        remaining_entries: entries.len() as u64,
        failed_bookmarks: 0,
    };
    if cancellation_requested.load(Ordering::Relaxed) || entries.is_empty() {
        return Ok((Some(delay_info), commit_only_backsync_future));
    }

    let future = sync_entries(
        ctx,
        &commit_sync_data,
        target_repo_dbs,
        entries,
        counter,
        &counter_name,
        cancellation_requested,
        sync_context,
        disable_lease,
        commit_only_backsync_future,
    )
    .await?;
    Ok((Some(delay_info), future))
}

pub async fn list_publishing_bookmarks<R>(
    ctx: &CoreContext,
    repo: &R,
    bookmark_prefix: &BookmarkPrefix,
) -> Result<HashSet<BookmarkKey>, Error>
where
    R: BookmarksRef + RepoIdentityRef,
{
    let bookmark_discovery_page_size = justknobs::get(BOOKMARK_DISCOVERY_PAGE_SIZE_JUST_KNOB, None);
    if bookmark_discovery_page_size <= 0 {
        bail!("{BOOKMARK_DISCOVERY_PAGE_SIZE_JUST_KNOB} must be positive");
    }
    let bookmark_discovery_page_size = bookmark_discovery_page_size.try_into()?;
    let max_discovered_bookmarks = justknobs::get(MAX_DISCOVERED_BOOKMARKS_JUST_KNOB, None);
    if max_discovered_bookmarks <= 0 {
        bail!("{MAX_DISCOVERED_BOOKMARKS_JUST_KNOB} must be positive");
    }
    let max_discovered_bookmarks = max_discovered_bookmarks.try_into()?;
    let mut bookmarks = HashSet::new();
    let mut discovered_bookmarks = 0;

    // Pagination is name-only, so list each category separately. This avoids
    // skipping another category with the same name at a page boundary.
    for category in BookmarkCategory::ALL {
        let mut pagination = BookmarkPagination::FromStart;
        loop {
            let bookmark_page = repo
                .bookmarks()
                .list(
                    ctx.clone(),
                    Freshness::MostRecent,
                    bookmark_prefix,
                    std::slice::from_ref(category),
                    BookmarkKind::ALL_PUBLISHING,
                    &pagination,
                    bookmark_discovery_page_size,
                )
                .try_collect::<Vec<_>>()
                .await?;
            let page_len = bookmark_page.len();
            discovered_bookmarks += page_len;
            if discovered_bookmarks > max_discovered_bookmarks {
                warn!(
                    "bookmark prefix {:?} in repo {} matched {} publishing bookmarks, exceeding {} configured by {}",
                    bookmark_prefix,
                    repo.repo_identity().name(),
                    discovered_bookmarks,
                    max_discovered_bookmarks,
                    MAX_DISCOVERED_BOOKMARKS_JUST_KNOB,
                );
                bail!(
                    "bookmark prefix {bookmark_prefix:?} in repo {} matched more than {max_discovered_bookmarks} publishing bookmarks",
                    repo.repo_identity().name(),
                );
            }
            let next_page = bookmark_page
                .last()
                .map(|(bookmark, _)| BookmarkPagination::After(bookmark.name().clone()));
            bookmarks.extend(
                bookmark_page
                    .into_iter()
                    .map(|(bookmark, _)| bookmark.into_key()),
            );

            if page_len < bookmark_discovery_page_size as usize {
                break;
            }
            pagination = next_page.ok_or_else(|| {
                format_err!("full bookmark discovery page did not contain a final bookmark")
            })?;
        }
    }

    Ok(bookmarks)
}

/// Discover source-side bookmark keys whose mapped target bookmark still
/// exists even though the source bookmark is absent.
///
/// Each repository is queried using its supplied prefix. `filter` is applied
/// to the queried keys, then `filter_map` normalizes the names
/// into the source repository's namespace before calculating the difference.
///
/// This only discovers candidates. Callers must process each candidate's
/// ordered bookmark-update stream before deleting anything in the target.
pub async fn discover_deleted_bookmarks<R, F, M>(
    ctx: &CoreContext,
    commit_sync_data: &CommitSyncData<R>,
    source: (RepositoryId, BookmarkPrefix),
    target: (RepositoryId, BookmarkPrefix),
    filter: F,
    filter_map: M,
) -> Result<HashSet<BookmarkKey>, Error>
where
    R: cross_repo_sync::Repo,
    F: Fn(RepositoryId, &BookmarkKey) -> bool,
    M: Fn(RepositoryId, BookmarkKey) -> Result<Option<BookmarkKey>, Error>,
{
    let source_repo_id = commit_sync_data.get_source_repo().repo_identity().id();
    let target_repo_id = commit_sync_data.get_target_repo().repo_identity().id();
    let (configured_source_repo_id, source_prefix) = source;
    let (configured_target_repo_id, target_prefix) = target;
    if configured_source_repo_id != source_repo_id || configured_target_repo_id != target_repo_id {
        bail!(
            "bookmark discovery repo pair {configured_source_repo_id} -> {configured_target_repo_id} does not match commit sync direction {source_repo_id} -> {target_repo_id}"
        );
    }

    let source_bookmarks =
        list_publishing_bookmarks(ctx, commit_sync_data.get_source_repo(), &source_prefix).await?;
    let target_bookmarks =
        list_publishing_bookmarks(ctx, commit_sync_data.get_target_repo(), &target_prefix).await?;

    let select_and_rewrite = |repo_id, bookmarks: HashSet<BookmarkKey>| {
        let mut rewritten = HashSet::new();
        for bookmark in bookmarks {
            if filter(repo_id, &bookmark) {
                if let Some(bookmark) = filter_map(repo_id, bookmark)? {
                    rewritten.insert(bookmark);
                }
            }
        }
        Ok::<_, Error>(rewritten)
    };
    let source_bookmarks = select_and_rewrite(source_repo_id, source_bookmarks)?;
    let target_bookmarks = select_and_rewrite(target_repo_id, target_bookmarks)?;

    Ok(target_bookmarks
        .difference(&source_bookmarks)
        .cloned()
        .collect())
}

async fn backsync_bookmarks<R>(
    ctx: CoreContext,
    commit_sync_data: CommitSyncData<R>,
    target_repo_dbs: Arc<TargetRepoDbs>,
    bookmarks: HashSet<BookmarkKey>,
    limit: BacksyncLimit,
    bookmark_concurrency: usize,
    cancellation_requested: Arc<AtomicBool>,
    sync_context: CommitSyncContext,
    disable_lease: bool,
    retry_initialization: bool,
) -> Result<BacksyncDelayInfo, Error>
where
    R: RepoLike + Send + Sync + Clone + 'static,
{
    let progress = stream::iter(bookmarks)
        .map(|bookmark| {
            cloned!(
                ctx,
                commit_sync_data,
                target_repo_dbs,
                cancellation_requested
            );
            async move {
                let result = if retry_initialization {
                    match backsync_bookmark_once(
                        ctx,
                        commit_sync_data,
                        target_repo_dbs,
                        bookmark.clone(),
                        limit,
                        cancellation_requested,
                        sync_context,
                        disable_lease,
                        Box::new(future::ready(())),
                    )
                    .await
                    {
                        Ok((delay, future)) => {
                            future.await;
                            Ok(delay)
                        }
                        Err(error) => Err(error),
                    }
                } else {
                    backsync_latest_for_bookmark(
                        ctx,
                        commit_sync_data,
                        target_repo_dbs,
                        bookmark.clone(),
                        limit,
                        cancellation_requested,
                        sync_context,
                        disable_lease,
                    )
                    .await
                };
                (bookmark, result)
            }
        })
        .buffer_unordered(bookmark_concurrency.max(1))
        .collect::<Vec<_>>()
        .await;

    let mut delay_info = BacksyncDelayInfo {
        delay_secs: 0,
        remaining_entries: 0,
        failed_bookmarks: 0,
    };
    for (bookmark, result) in progress {
        match result {
            Ok(bookmark_delay) => {
                delay_info.delay_secs = delay_info.delay_secs.max(bookmark_delay.delay_secs);
                delay_info.remaining_entries += bookmark_delay.remaining_entries;
            }
            Err(error) => {
                // One bad bookmark must not cancel successful in-flight work or
                // terminate the repo-pair loop. Its durable cursor is unchanged,
                // so the next polling iteration retries it.
                error!("Failed to backsync bookmark {:?}: {:#}", bookmark, error);
                // Keep `remaining_entries` limited to measured backlog rather
                // than inventing a count for a worker that returned no result.
                delay_info.failed_bookmarks += 1;
            }
        }
    }

    Ok(delay_info)
}

/// Backsync the current source bookmarks relevant to one small repository.
///
/// The configured prefix and common bookmarks form a single logical source-log
/// selector. Each candidate reads forward from its own durable cursor.
pub async fn backsync_latest_by_prefix<R>(
    ctx: CoreContext,
    commit_sync_data: CommitSyncData<R>,
    target_repo_dbs: Arc<TargetRepoDbs>,
    bookmark_prefix: BookmarkPrefix,
    common_bookmarks: &[BookmarkKey],
    limit: BacksyncLimit,
    bookmark_concurrency: usize,
    cancellation_requested: Arc<AtomicBool>,
    sync_context: CommitSyncContext,
    disable_lease: bool,
) -> Result<BacksyncDelayInfo, Error>
where
    R: RepoLike + Send + Sync + Clone + 'static,
{
    let mut bookmarks =
        list_publishing_bookmarks(&ctx, commit_sync_data.get_source_repo(), &bookmark_prefix)
            .await?;
    bookmarks.extend(common_bookmarks.iter().cloned());
    backsync_bookmarks(
        ctx,
        commit_sync_data,
        target_repo_dbs,
        bookmarks,
        limit,
        bookmark_concurrency,
        cancellation_requested,
        sync_context,
        disable_lease,
        false,
    )
    .await
}

/// Run one configured prefix/common/deletion pass, retrying initialization races.
///
/// Selection is the same scoped snapshot used by the daemon, not a historical
/// repo-wide commit-only sweep. Use the explicit Commits command for historical
/// commit sync, or `backsync_bookmark_once` when a particular source key is known.
pub async fn backsync_once_by_prefix<R>(
    ctx: CoreContext,
    commit_sync_data: CommitSyncData<R>,
    target_repo_dbs: Arc<TargetRepoDbs>,
    bookmark_prefix: BookmarkPrefix,
    common_bookmarks: &[BookmarkKey],
    bookmark_concurrency: usize,
    cancellation_requested: Arc<AtomicBool>,
    sync_context: CommitSyncContext,
    disable_lease: bool,
) -> Result<BacksyncDelayInfo, Error>
where
    R: RepoLike + Send + Sync + Clone + 'static,
{
    let mut bookmarks =
        list_publishing_bookmarks(&ctx, commit_sync_data.get_source_repo(), &bookmark_prefix)
            .await?;
    bookmarks.extend(common_bookmarks.iter().cloned());
    bookmarks.extend(
        deleted_bookmarks_by_prefix(&ctx, &commit_sync_data, bookmark_prefix, common_bookmarks)
            .await?,
    );
    backsync_bookmarks(
        ctx,
        commit_sync_data,
        target_repo_dbs,
        bookmarks,
        BacksyncLimit::NoLimit,
        bookmark_concurrency,
        cancellation_requested,
        sync_context,
        disable_lease,
        true,
    )
    .await
}

async fn deleted_bookmarks_by_prefix<R>(
    ctx: &CoreContext,
    commit_sync_data: &CommitSyncData<R>,
    bookmark_prefix: BookmarkPrefix,
    common_bookmarks: &[BookmarkKey],
) -> Result<HashSet<BookmarkKey>, Error>
where
    R: RepoLike + Send + Sync + Clone + 'static,
{
    // The configured large-repo prefix is stripped in the small repo, so
    // there is no narrower target-side prefix to query. The shared discovery
    // maximum remains a fail-closed bound for whole-target enumeration and
    // must be sized for an enabled small repo before rollout.
    let source_repo_id = commit_sync_data.get_source_repo_id();
    let target_repo_id = commit_sync_data.get_target_repo_id();
    let common_bookmarks = common_bookmarks.iter().cloned().collect::<HashSet<_>>();
    let target_to_source = get_bookmark_renamer(
        Arc::clone(commit_sync_data.get_live_commit_sync_config()),
        target_repo_id,
        source_repo_id,
    )
    .await?;
    // Common source bookmarks are always scheduled by the ordinary backsync
    // loop, including after deletion, so exclude them here.
    let bookmarks = discover_deleted_bookmarks(
        ctx,
        commit_sync_data,
        (source_repo_id, bookmark_prefix),
        (target_repo_id, BookmarkPrefix::empty()),
        |_, _| true,
        move |repo_id, bookmark| {
            if repo_id == source_repo_id {
                return Ok(Some(bookmark));
            }
            let category = *bookmark.category();
            let source_bookmark = target_to_source(&bookmark).ok_or_else(|| {
                format_err!(
                    "target bookmark {bookmark:?} did not map to source repo {source_repo_id}"
                )
            })?;
            let source_bookmark =
                BookmarkKey::with_name_and_category(source_bookmark.into_name(), category);
            Ok((!common_bookmarks.contains(&source_bookmark)).then_some(source_bookmark))
        },
    )
    .await?;
    Ok(bookmarks)
}

/// Backsync source bookmarks that are absent from the source bookmark table but
/// still present in the target bookmark table.
pub async fn backsync_deleted_by_prefix<R>(
    ctx: CoreContext,
    commit_sync_data: CommitSyncData<R>,
    target_repo_dbs: Arc<TargetRepoDbs>,
    bookmark_prefix: BookmarkPrefix,
    common_bookmarks: &[BookmarkKey],
    limit: BacksyncLimit,
    bookmark_concurrency: usize,
    cancellation_requested: Arc<AtomicBool>,
    sync_context: CommitSyncContext,
    disable_lease: bool,
) -> Result<BacksyncDelayInfo, Error>
where
    R: RepoLike + Send + Sync + Clone + 'static,
{
    let bookmarks =
        deleted_bookmarks_by_prefix(&ctx, &commit_sync_data, bookmark_prefix, common_bookmarks)
            .await?;
    // This difference only schedules exact per-bookmark log streams; it does
    // not directly delete target bookmarks. Each worker still reads and
    // applies every source-log entry after its durable bookmark cursor, so an
    // incomplete candidate snapshot cannot invent a deletion, and a delete
    // followed by recreation is replayed in order.
    backsync_bookmarks(
        ctx,
        commit_sync_data,
        target_repo_dbs,
        bookmarks,
        limit,
        bookmark_concurrency,
        cancellation_requested,
        sync_context,
        disable_lease,
        false,
    )
    .await
}

async fn sync_entries<R>(
    mut ctx: CoreContext,
    commit_sync_data: &CommitSyncData<R>,
    target_repo_dbs: Arc<TargetRepoDbs>,
    entries: Vec<BookmarkUpdateLogEntry>,
    mut counter: BookmarkUpdateLogId,
    counter_name: &str,
    cancellation_requested: Arc<AtomicBool>,
    sync_context: CommitSyncContext,
    disable_lease: bool,
    mut commit_only_backsync_future: Box<dyn Future<Output = ()> + Send + Unpin>,
) -> Result<Box<dyn Future<Output = ()> + Send + Unpin>, Error>
where
    R: RepoLike + Send + Sync + Clone + 'static,
{
    for entry in entries {
        // Before processing each entry, check if cancellation has
        // been requested and exit if that's the case.
        if cancellation_requested.load(Ordering::Relaxed) {
            info!("sync stopping due to cancellation request");
            return Ok(commit_only_backsync_future);
        }
        // A cursor only advances after this execution model has acknowledged
        // the corresponding row. If a competing replica moved it beyond the
        // entry, that replica also completed all preceding rows in this same
        // ordered stream.
        if counter >= entry.id {
            continue;
        }
        let mut scuba_sample = ctx.scuba().clone();
        let pc = ctx.fork_perf_counters();
        let mut scuba_log_tag = "Backsyncing".to_string();
        let (stats, new_commit_only_backsync_future) = do_sync_entry(
            ctx.clone(),
            commit_sync_data,
            &target_repo_dbs,
            entry,
            &mut counter,
            counter_name,
            sync_context,
            disable_lease,
            commit_only_backsync_future,
            &mut scuba_sample,
            &mut scuba_log_tag,
        )
        .try_timed()
        .await?;
        commit_only_backsync_future = new_commit_only_backsync_future;
        pc.insert_perf_counters(&mut scuba_sample);
        scuba_sample
            .add_future_stats(&stats)
            .log_with_msg(&scuba_log_tag, None);
    }
    Ok(commit_only_backsync_future)
}

// This function is the inner function for sync_entries and shouldn't be called by other callers.
// It encapsulates of what we consider as doing a single "backsyncing" for bookmark entry: an
// activity that we want to time and log.
async fn do_sync_entry<R>(
    ctx: CoreContext,
    commit_sync_data: &CommitSyncData<R>,
    target_repo_dbs: &Arc<TargetRepoDbs>,
    entry: BookmarkUpdateLogEntry,
    counter: &mut BookmarkUpdateLogId,
    counter_name: &str,
    sync_context: CommitSyncContext,
    disable_lease: bool,
    mut commit_only_backsync_future: Box<dyn Future<Output = ()> + Send + Unpin>,
    scuba_sample: &mut MononokeScubaSampleBuilder,
    scuba_log_tag: &mut String,
) -> Result<Box<dyn Future<Output = ()> + Send + Unpin>, Error>
where
    R: RepoLike + Send + Sync + Clone + 'static,
{
    let entry_id = entry.id;
    if *counter >= entry_id {
        return Ok(commit_only_backsync_future);
    }
    debug!("backsyncing {} ...", entry_id);

    if commit_sync_data
        .rename_bookmark(&entry.bookmark_name)
        .await?
        .is_none()
    {
        // For the bookmarks that don't remap to small repos we can skip. But it's
        // still valuable to have commit mapping ready for them. That's why we spawn
        // a commit backsync future that we don't wait for here. Each of such futures
        // waits for result of previous commit-only backsync so we don't duplicate
        // work unnecessarily.
        debug!("Renamed bookmark is None. No sync happening.");
        target_repo_dbs
            .counters
            .set_counter(
                &ctx,
                counter_name,
                entry.id.try_into()?,
                Some((*counter).try_into()?),
            )
            .await?;
        *counter = entry.id;
        if let Some(to_cs_id) = entry.to_changeset_id {
            commit_only_backsync_future = Box::new({
                cloned!(ctx, sync_context, to_cs_id, commit_sync_data);
                mononoke::spawn_task(async move {
                    commit_only_backsync_future.await;
                    let res = sync_commit(
                        &ctx,
                        to_cs_id.clone(),
                        &commit_sync_data,
                        // Backsyncer is always used in the large-to-small direction,
                        // therefore there can be at most one remapped candidate,
                        // so `CandidateSelectionHint::Only` is a safe choice
                        CandidateSelectionHint::Only,
                        sync_context,
                        disable_lease,
                    )
                    .await;
                    if let Err(err) = res {
                        error!(
                            "Failed to backsync {} pointing to {}: {}",
                            entry.bookmark_name, to_cs_id, err
                        );
                    }
                })
                .map(|_| ())
            });
        }

        return Ok(commit_only_backsync_future);
    }

    scuba_sample.add("backsyncer_bookmark_log_entry_id", u64::from(entry.id));

    let start_instant = Instant::now();

    if let Some(to_cs_id) = entry.to_changeset_id {
        let (_, unsynced_ancestors_versions) =
            find_toposorted_unsynced_ancestors(&ctx, commit_sync_data, to_cs_id, None).await?;

        if !unsynced_ancestors_versions.has_ancestor_with_a_known_outcome() {
            // Not a single ancestor of to_cs_id was ever synced.
            // That means that we can't figure out which commit sync mapping version
            // to use. In that case we just skip this entry and not sync it at all.
            // This seems the safest option (i.e. we won't rewrite a commit with
            // an incorrect version) but it also has a downside that the bookmark that points
            // to this commit is not going to be synced.
            warn!("skipping {}, entry id {}", entry.bookmark_name, entry.id);
            *scuba_log_tag = "Skipping entry because there are no synced ancestors".to_string();
            target_repo_dbs
                .counters
                .set_counter(
                    &ctx,
                    counter_name,
                    entry.id.try_into()?,
                    Some((*counter).try_into()?),
                )
                .await?;
            *counter = entry.id;
            return Ok(commit_only_backsync_future);
        }

        // Backsyncer is always used in the large-to-small direction,
        // therefore there can be at most one remapped candidate,
        // so `CandidateSelectionHint::Only` is a safe choice

        sync_commit(
            &ctx,
            to_cs_id,
            commit_sync_data,
            CandidateSelectionHint::Only,
            sync_context,
            disable_lease,
        )
        .await?;
    }

    let new_counter = entry.id;
    let maybe_log_id = backsync_bookmark(
        ctx.clone(),
        commit_sync_data,
        target_repo_dbs.clone(),
        Some(*counter),
        counter_name,
        &entry,
    )
    .await?;
    scuba_sample.add_opt(
        "from_csid",
        entry.from_changeset_id.map(|csid| csid.to_string()),
    );
    scuba_sample.add_opt(
        "to_csid",
        entry.to_changeset_id.map(|csid| csid.to_string()),
    );
    scuba_sample.add(
        "backsync_duration_ms",
        u64::try_from(start_instant.elapsed().as_millis()).unwrap_or(u64::MAX),
    );
    scuba_sample.add("backsync_previously_done", maybe_log_id.is_none());

    if let Some(_log_id) = maybe_log_id {
        *counter = new_counter;
    } else {
        debug!(
            "failed to backsync {}, most likely another process already synced it ",
            entry_id
        );
        // Transaction failed, it could be because another process already backsynced it
        // Verify that counter was moved and continue if that's the case

        let observed_counter: BookmarkUpdateLogId = target_repo_dbs
            .counters
            .get_counter(&ctx, counter_name)
            .await?
            .unwrap_or(0)
            .try_into()?;
        if observed_counter <= *counter {
            return Err(format_err!(
                "backsync transaction failed, but the counter didn't move forward. Was {}, became {}",
                *counter,
                observed_counter,
            ));
        } else {
            debug!(
                "verified that another process has already synced {}",
                entry_id
            );
            *counter = observed_counter;
        }
    }
    Ok(commit_only_backsync_future)
}

/// All "new" commits on this bookmark move. Use with care, creating a bookmark
/// means ALL ancestors are new.
async fn commits_added_by_bookmark_move(
    ctx: &CoreContext,
    repo: &impl RepoLike,
    from_cs_id: Option<ChangesetId>,
    to_cs_id: Option<ChangesetId>,
) -> Result<HashSet<ChangesetId>, Error> {
    match (from_cs_id, to_cs_id) {
        (_, None) => Ok(HashSet::new()),
        (None, Some(to_id)) => {
            repo.commit_graph()
                .ancestors_difference_stream(ctx, vec![to_id], vec![])
                .await?
                .try_collect()
                .await
        }
        (Some(from_id), Some(to_id)) => {
            // If needed, this can be optimised by using range_stream when from_id is
            // an ancestor of from_id
            repo.commit_graph()
                .ancestors_difference_stream(ctx, vec![to_id], vec![from_id])
                .await?
                .try_collect()
                .await
        }
    }
}

async fn backsync_bookmark<R>(
    ctx: CoreContext,
    commit_sync_data: &CommitSyncData<R>,
    target_repo_dbs: Arc<TargetRepoDbs>,
    prev_counter: Option<BookmarkUpdateLogId>,
    counter_name: &str,
    log_entry: &BookmarkUpdateLogEntry,
) -> Result<Option<BookmarkUpdateLogId>, Error>
where
    R: RepoLike + Send + Sync + Clone + 'static,
{
    let prev_counter: Option<i64> = prev_counter.map(|x| x.try_into()).transpose()?;
    let target_repo_id = commit_sync_data.get_target_repo().repo_identity().id();
    let counter_name = counter_name.to_owned();
    debug!("preparing to backsync {:?}", log_entry);

    let new_counter = log_entry.id;
    let bookmark = commit_sync_data
        .rename_bookmark(&log_entry.bookmark_name)
        .await?
        .map(|renamed| {
            BookmarkKey::with_name_and_category(
                renamed.into_name(),
                *log_entry.bookmark_name.category(),
            )
        });
    debug!("bookmark was renamed into {:?}", bookmark);
    let from_cs_id = log_entry.from_changeset_id;
    let to_cs_id = log_entry.to_changeset_id;

    let get_commit_sync_outcome = |maybe_cs_id: Option<ChangesetId>| {
        cloned!(ctx);
        async move {
            match maybe_cs_id {
                Some(cs_id) => {
                    let maybe_outcome = commit_sync_data
                        .get_commit_sync_outcome(&ctx, cs_id)
                        .await?;
                    match maybe_outcome {
                        Some(outcome) => Ok(Some((outcome, cs_id))),
                        None => Err(format_err!("{cs_id} hasn't been backsynced yet")),
                    }
                }
                None => Ok(None),
            }
        }
    };

    let get_remapped_cs_id =
        move |maybe_outcome: Option<(CommitSyncOutcome, ChangesetId)>| match maybe_outcome {
            Some((outcome, cs_id)) => {
                use CommitSyncOutcome::*;
                match outcome {
                    NotSyncCandidate(_) => Err(format_err!(
                        "invalid bookmark move: {cs_id:?} should not be synced to target repo"
                    )),
                    RewrittenAs(cs_id, _) | EquivalentWorkingCopyAncestor(cs_id, _) => {
                        Ok(Some(cs_id))
                    }
                }
            }
            None => Ok(None),
        };

    if let Some(bookmark) = bookmark {
        // Fetch sync outcome before transaction to keep transaction as short as possible
        let from_sync_outcome = get_commit_sync_outcome(from_cs_id).await?;
        let to_sync_outcome = get_commit_sync_outcome(to_cs_id).await?;
        debug!(
            "commit sync outcomes: from_cs: {:?}, to_cs: {:?}",
            from_sync_outcome, to_sync_outcome
        );

        let from_cs_id = get_remapped_cs_id(from_sync_outcome)?;
        let to_cs_id = get_remapped_cs_id(to_sync_outcome)?;
        let bookmark_info = BookmarkInfo {
            bookmark_name: bookmark.clone(),
            bookmark_kind: BookmarkKind::Publishing, // Only publishing bookmarks are back synced
            operation: BookmarkOperation::new(from_cs_id, to_cs_id)?,
            reason: BookmarkUpdateReason::Backsyncer,
        };

        if from_cs_id != to_cs_id {
            let target_repo = commit_sync_data.get_target_repo();
            // This CANNOT be done after getting the bookmark transaction, because it accesses SQL without a
            // transaction and that causes a deadlock that blocks the syncing.
            let globalrev_entries: Vec<BonsaiGlobalrevMappingEntry> = if target_repo
                .repo_config()
                .pushrebase
                .globalrev_config
                .as_ref()
                .map(|c| &c.publishing_bookmark)
                == Some(&bookmark)
            {
                let all_commits =
                    commits_added_by_bookmark_move(&ctx, target_repo, from_cs_id, to_cs_id).await?;
                let ctx = &ctx;
                let blobstore = target_repo.repo_blobstore();
                stream::iter(all_commits)
                    .map(|bcs_id| async move {
                        let cs = bcs_id.load(ctx, blobstore).await?;
                        // When pushrebasing into the large repo, this commit
                        // should've gotten a globalrev
                        // But if it doesn't it's better to skip assigning the globalrev
                        // than to break the backsync process. (The commit was already acknowledged
                        // and we have not much choice on whether to sync it).
                        let globalrev = Globalrev::from_bcs(&cs).ok();
                        if let Some(globalrev) = globalrev {
                            anyhow::Ok(Some(BonsaiGlobalrevMappingEntry { bcs_id, globalrev }))
                        } else {
                            anyhow::Ok(None)
                        }
                    })
                    .buffer_unordered(100)
                    .try_filter_map(|res| future::ready(Ok(res)))
                    .try_collect()
                    .await?
            } else {
                vec![]
            };
            let commits_to_log = async {
                match to_cs_id {
                    Some(to_cs_id) => {
                        let res = find_draft_ancestors(&ctx, target_repo, to_cs_id).await;
                        match res {
                            Ok(bcss) => bcss.iter().map(|bcs| CommitInfo::new(bcs, None)).collect(),
                            Err(err) => {
                                ctx.scuba().clone().log_with_msg(
                                    "Failed to find draft ancestors for logging",
                                    Some(format!("{err}")),
                                );
                                vec![]
                            }
                        }
                    }
                    None => vec![],
                }
            }
            .await;

            let mut bookmark_txn = target_repo_dbs.bookmarks.create_transaction(ctx.clone());
            debug!("syncing bookmark {} to {:?}", bookmark, to_cs_id);

            match (from_cs_id, to_cs_id) {
                (Some(from), Some(to)) => {
                    debug!(
                        "updating bookmark {:?} from {:?} to {:?}",
                        bookmark, from, to
                    );
                    bookmark_txn.update(&bookmark, to, from, BookmarkUpdateReason::Backsyncer)?;
                }
                (Some(from), None) => {
                    debug!(
                        "deleting bookmark {:?} with original position {:?}",
                        bookmark, from
                    );
                    bookmark_txn.delete(&bookmark, from, BookmarkUpdateReason::Backsyncer)?;
                }
                (None, Some(to)) => {
                    debug!("creating bookmark {:?} to point to {:?}", bookmark, to);
                    bookmark_txn.create(&bookmark, to, BookmarkUpdateReason::Backsyncer)?;
                }
                (None, None) => {
                    bail!("unexpected bookmark move");
                }
            };
            let new_counter = new_counter.try_into()?;
            let counter_name_for_txn = counter_name.clone();

            let txn_hook = Arc::new({
                move |ctx: CoreContext, txn: Transaction| {
                    cloned!(counter_name_for_txn, globalrev_entries);
                    async move {
                        // This is an abstraction leak: it only works because the
                        // mutable counters/globalrevs are stored in the same db as the
                        // bookmarks.
                        let txn_result = SqlMutableCounters::set_counter_on_txn(
                            &ctx,
                            target_repo_id,
                            &counter_name_for_txn,
                            new_counter,
                            prev_counter,
                            txn,
                        )
                        .await?;

                        let txn = match txn_result {
                            TransactionResult::Succeeded(txn) => Ok(txn),
                            TransactionResult::Failed => Err(BookmarkTransactionError::LogicError),
                        }?;

                        if !globalrev_entries.is_empty() {
                            bonsai_globalrev_mapping::add_globalrevs(
                                &ctx,
                                txn,
                                target_repo_id,
                                &globalrev_entries,
                            )
                            .await
                            .map_err(|err| BookmarkTransactionError::Other(err.into()))
                        } else {
                            Ok(txn)
                        }
                    }
                    .boxed()
                }
            });

            let res = bookmark_txn
                .commit_with_hooks(vec![txn_hook])
                .await?
                .map(|x| x.into());
            log_new_commits(
                &ctx,
                target_repo,
                Some((&bookmark, BookmarkKind::Publishing)),
                commits_to_log,
            )
            .await;
            log_bookmark_operation(&ctx, target_repo, &bookmark_info).await;
            return Ok(res);
        } else {
            debug!(
                "from_cs_id and to_cs_id are the same: {:?}. No sync happening for {:?}",
                from_cs_id, bookmark
            );
        }
    } else {
        debug!("Renamed bookmark is None. No sync happening.");
    }

    let maybe_log_id = if target_repo_dbs
        .counters
        .set_counter(&ctx, &counter_name, new_counter.try_into()?, prev_counter)
        .await?
    {
        Some(new_counter)
    } else {
        None
    };

    Ok(maybe_log_id)
}

pub async fn open_backsyncer_dbs(repo: &impl RepoLike) -> Result<TargetRepoDbs, Error> {
    Ok(TargetRepoDbs {
        bookmarks: repo.bookmarks_arc(),
        bookmark_update_log: repo.bookmark_update_log_arc(),
        counters: repo.mutable_counters_arc(),
    })
}

pub fn format_counter(repo_to_backsync_from: &RepositoryId) -> String {
    format!("backsync_from_{}", repo_to_backsync_from.id())
}

/// Format a human-readable mutable-counter name for one source bookmark.
///
/// The source repository and bookmark category keep otherwise identical
/// bookmark names in separate cursor domains.
pub fn format_bookmark_counter(
    repo_to_backsync_from: &RepositoryId,
    bookmark: &BookmarkKey,
) -> Result<String, Error> {
    let counter_name = format!(
        "backsync_by_bookmark_v1_{}_{}_{}",
        repo_to_backsync_from.id(),
        bookmark.category(),
        bookmark.as_str(),
    );
    validate_counter_name(&counter_name)?;
    Ok(counter_name)
}
