/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::ops::Range;
use std::pin::pin;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use commit_graph::CommitGraphRef;
use context::CoreContext;
use futures::Stream;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future;
use futures::stream;
use futures_retry::retry;
use mononoke_types::ChangesetId;
use mutable_counters::MutableCountersRef;
use sql_commit_graph_storage::CommitGraphBulkFetcherRef;
use tracing::warn;

use crate::CommitOutcome;
use crate::RepoBackfill;

/// How many commits to read from the commit graph in one query.
const SQL_CHUNK_SIZE: u64 = 1_000;

/// How many commit-graph queries for parents to run at once.
const SQL_QUERIES_AT_ONCE: usize = 10;

/// How often a walk tries to read where to start, or to walk a window, before
/// it fails, and how long it waits before the first retry. Each later wait is
/// twice as long.
const WALK_RETRIES: WalkRetries = WalkRetries {
    attempts: 4,
    first_delay: Duration::from_secs(10),
};

#[derive(Clone, Copy, Debug)]
struct WalkRetries {
    attempts: usize,
    first_delay: Duration,
}

/// One task's share of a repo's windows. Windows are numbered by their lowest
/// id divided by the window size, and the task takes those whose number modulo
/// `count` is `index`, so that the tasks move up the repo together.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskShard {
    index: u64,
    count: NonZeroU64,
}

impl TaskShard {
    pub fn try_new(index: u64, count: u64) -> Result<Self> {
        let count = NonZeroU64::new(count).context("The task count must be at least 1")?;
        ensure!(
            index < count.get(),
            "The task index {index} must be lower than the task count {count}",
        );
        Ok(Self { index, count })
    }
}

/// The commit-graph ids a walk covers in a repo, both included. Without a
/// start id the walk starts at the repo's lowest id, and without an end id it
/// stops at the repo's newest commit when the walk starts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IdBounds {
    pub start_id: Option<u64>,
    pub end_id: Option<u64>,
}

/// What a task did with one window of commit-graph ids.
#[derive(Debug)]
pub struct WindowReport {
    /// The commit-graph ids the window covers.
    pub window: Range<u64>,
    /// What was done with each commit the window listed.
    pub outcomes: HashMap<ChangesetId, CommitOutcome>,
}

/// Walks one task's share of a repo's commits, oldest first, and derives them.
///
/// The walk goes up the repo's commit-graph ids from the start id to the end id
/// in windows of `window_size` ids, and the task takes its share of the windows
/// (see [`TaskShard`]). Commit-graph ids come from one counter shared by every
/// repo, so a small repo's windows are mostly empty. The task skips its windows
/// that hold no commits of the repo: it asks the replica for the repo's lowest
/// id at or above where it is, and goes to its first window at or above the one
/// holding that id. For each window it lists the commits and their parents
/// from the commit-graph replica and derives them with
/// [`RepoBackfill::derive_commits`], which derives their underived ancestors
/// too.
///
/// After each window, the task stores the id just above the window in a
/// mutable counter of the repo, and a new walk by the same task carries on
/// from there, so a restarted task redoes at most one window. The counter is
/// named after the task's index, the task count, the window size and the start
/// id, so changing any of the last three starts every task from the start id
/// again. Changing the end id does not.
pub struct RepoWalk {
    shard: TaskShard,
    window_size: NonZeroU64,
    bounds: IdBounds,
    retries: WalkRetries,
}

impl RepoWalk {
    pub fn new(shard: TaskShard, window_size: NonZeroU64, bounds: IdBounds) -> Self {
        Self {
            shard,
            window_size,
            bounds,
            retries: WALK_RETRIES,
        }
    }

    /// Derives this task's windows of `repo`, oldest first, and reports each
    /// window once it is derived and recorded as finished. Windows without
    /// commits of the repo are skipped and not reported. Reading where to
    /// start, finding the next window, and a window whose walk fails as a whole
    /// rather than for single commits, are retried with backoff. If one still
    /// fails, the walk stops with the error and the window is left unfinished,
    /// so the next walk starts with it.
    pub fn walk<'a, R>(
        &'a self,
        ctx: &'a CoreContext,
        repo: &'a R,
        backfill: &'a RepoBackfill,
    ) -> impl Stream<Item = Result<WindowReport>> + 'a
    where
        R: CommitGraphRef + CommitGraphBulkFetcherRef + MutableCountersRef + Sync,
    {
        stream::once(self.with_retries("read where to walk", || self.ids_left(ctx, repo)))
            .map_ok(move |ids| {
                walk_windows(
                    ids,
                    move |ids| self.next_window_with_retries(ctx, repo, ids),
                    move |window| self.walk_window_with_retries(ctx, repo, backfill, window),
                )
            })
            .try_flatten()
    }

    /// This task's lowest window among `ids` that holds commits of the repo,
    /// retried with backoff.
    async fn next_window_with_retries<R>(
        &self,
        ctx: &CoreContext,
        repo: &R,
        ids: Range<u64>,
    ) -> Result<Option<Range<u64>>>
    where
        R: CommitGraphBulkFetcherRef + Sync,
    {
        self.with_retries(&format!("find the next window in {ids:?}"), || {
            next_task_window(ids.clone(), self.window_size, self.shard, |ids| {
                lowest_id(ctx, repo, ids)
            })
        })
        .await
    }

    /// The ids this task has left to walk.
    async fn ids_left<R>(&self, ctx: &CoreContext, repo: &R) -> Result<Range<u64>>
    where
        R: CommitGraphBulkFetcherRef + MutableCountersRef,
    {
        let bounds = repo
            .commit_graph_bulk_fetcher()
            .repo_bounds(ctx, false)
            .await
            .context("Failed to read the repo's commit-graph ids")?;
        let start = self
            .bounds
            .start_id
            .map_or(bounds.start, |start_id| start_id.max(bounds.start));
        let end = self.bounds.end_id.map_or(bounds.end, |end_id| {
            end_id.saturating_add(1).min(bounds.end)
        });

        let counter = self.counter_name();
        let resume_from = repo
            .mutable_counters()
            .get_counter(ctx, &counter)
            .await
            .with_context(|| format!("Failed to read the counter {counter}"))?
            .map(u64::try_from)
            .transpose()
            .with_context(|| format!("The counter {counter} is negative"))?;

        let start = resume_from.map_or(start, |resume_from| resume_from.max(start));
        Ok(start..end)
    }

    async fn walk_window_with_retries<R>(
        &self,
        ctx: &CoreContext,
        repo: &R,
        backfill: &RepoBackfill,
        window: Range<u64>,
    ) -> Result<WindowReport>
    where
        R: CommitGraphRef + CommitGraphBulkFetcherRef + MutableCountersRef + Sync,
    {
        self.with_retries(&format!("walk the ids {window:?}"), || {
            self.walk_window(ctx, repo, backfill, window.clone())
        })
        .await
    }

    /// Runs `attempt` until it succeeds or `self.retries.attempts` attempts
    /// have failed, waiting longer before each retry. Each failed attempt is
    /// logged as a failure to do `what`.
    async fn with_retries<T, F, Fut>(&self, what: &str, mut attempt: F) -> Result<T>
    where
        F: FnMut() -> Fut + Send,
        Fut: Future<Output = Result<T>>,
        T: Send + 'static,
    {
        let attempts = self.retries.attempts;
        let (value, _attempts) = retry(|_| attempt(), self.retries.first_delay)
            .binary_exponential_backoff()
            .max_attempts(attempts)
            .inspect_err(|attempt, error| {
                warn!("Attempt {attempt} of {attempts} to {what} failed: {error:#}");
            })
            .await?;
        Ok(value)
    }

    async fn walk_window<R>(
        &self,
        ctx: &CoreContext,
        repo: &R,
        backfill: &RepoBackfill,
        window: Range<u64>,
    ) -> Result<WindowReport>
    where
        R: CommitGraphRef + CommitGraphBulkFetcherRef + MutableCountersRef,
    {
        // A paused repo's windows are not listed either, so that pausing it
        // also stops the walk's reads from the commit-graph replica. Each
        // attempt waits, so that a pause also holds back the retries of a
        // failed window.
        backfill.wait_while_paused().await;
        let cs_ids: Vec<_> = repo
            .commit_graph_bulk_fetcher()
            .oldest_first_stream(ctx, window.clone(), SQL_CHUNK_SIZE, false)
            .map_ok(|item| item.cs_id)
            .try_collect()
            .await
            .with_context(|| format!("Failed to list the commits with ids in {window:?}"))?;
        let commits = parents(ctx, repo, cs_ids).await?;
        let outcomes = backfill.derive_commits(ctx, commits).await?;

        let counter = self.counter_name();
        let finished = i64::try_from(window.end)
            .with_context(|| format!("The id {} does not fit a counter", window.end))?;
        repo.mutable_counters()
            .set_counter(ctx, &counter, finished, None)
            .await
            .with_context(|| format!("Failed to set the counter {counter} to {finished}"))?;

        Ok(WindowReport { window, outcomes })
    }

    fn counter_name(&self) -> String {
        // Without a start id the walk starts at the repo's lowest id, as it
        // would from id 0.
        format!(
            "augmented_manifest_backfill_task_{}_of_{}_window_{}_from_{}",
            self.shard.index,
            self.shard.count,
            self.window_size,
            self.bounds.start_id.unwrap_or(0),
        )
    }
}

/// Walks the windows that `next_window` finds among `ids`, oldest first, with
/// `walk_window`. It ends when no window is left, or at the first error without
/// looking for another window, so that no later window can record progress
/// past the one that failed.
fn walk_windows<'a, N, NFut, W, WFut>(
    ids: Range<u64>,
    next_window: N,
    walk_window: W,
) -> impl Stream<Item = Result<WindowReport>> + 'a
where
    N: FnMut(Range<u64>) -> NFut + 'a,
    NFut: Future<Output = Result<Option<Range<u64>>>> + 'a,
    W: FnMut(Range<u64>) -> WFut + 'a,
    WFut: Future<Output = Result<WindowReport>> + 'a,
{
    let end = ids.end;
    stream::try_unfold(
        (ids.start, next_window, walk_window),
        move |(from, mut next_window, mut walk_window)| async move {
            let Some(window) = next_window(from..end).await? else {
                return Ok(None);
            };
            let report = walk_window(window).await?;
            let from = report.window.end;
            Ok(Some((report, (from, next_window, walk_window))))
        },
    )
}

/// The lowest commit-graph id of the repo's commits among `ids`, read from the
/// replica.
async fn lowest_id<R>(ctx: &CoreContext, repo: &R, ids: Range<u64>) -> Result<Option<u64>>
where
    R: CommitGraphBulkFetcherRef,
{
    let first =
        pin!(
            repo.commit_graph_bulk_fetcher()
                .oldest_first_stream(ctx, ids.clone(), 1, false)
        )
        .try_next()
        .await
        .with_context(|| format!("Failed to read the lowest commit-graph id in {ids:?}"))?;
    Ok(first.map(|item| item.sql_id))
}

/// Maps each of `cs_ids` to its parents.
async fn parents<R>(
    ctx: &CoreContext,
    repo: &R,
    cs_ids: Vec<ChangesetId>,
) -> Result<HashMap<ChangesetId, Vec<ChangesetId>>>
where
    R: CommitGraphRef,
{
    stream::iter(cs_ids.chunks(SQL_CHUNK_SIZE as usize))
        .map(|chunk| repo.commit_graph().many_changeset_parents(ctx, chunk))
        .buffer_unordered(SQL_QUERIES_AT_ONCE)
        .try_fold(HashMap::new(), |mut all, parents| {
            all.extend(
                parents
                    .into_iter()
                    .map(|(cs_id, parents)| (cs_id, parents.into_vec())),
            );
            future::ok(all)
        })
        .await
        .context("Failed to read the parents of commits")
}

/// This task's lowest window among the commit-graph ids in `ids` that holds
/// commits of the repo, or `None` if it has no such window left.
///
/// `lowest_id` returns the lowest id of the repo's commits in the ids it is
/// given. From the lowest id at or above the start, the search jumps to the
/// first window of this task at or above the window holding that id, and
/// repeats from that window's start until the window holds the id. Windows it
/// jumps over hold no commits of the repo, and as ids only grow, they never
/// will.
async fn next_task_window<F, Fut>(
    ids: Range<u64>,
    window_size: NonZeroU64,
    shard: TaskShard,
    mut lowest_id: F,
) -> Result<Option<Range<u64>>>
where
    F: FnMut(Range<u64>) -> Fut,
    Fut: Future<Output = Result<Option<u64>>>,
{
    let mut from = ids.start;
    while let Some(id) = lowest_id(from..ids.end).await? {
        let window_start = (id - id % window_size.get()).max(ids.start);
        let Some(window) = task_windows(window_start..ids.end, window_size, shard).next() else {
            return Ok(None);
        };
        if window.contains(&id) {
            return Ok(Some(window));
        }
        from = window.start;
    }
    Ok(None)
}

/// The windows a task walks, oldest first, among the commit-graph ids in
/// `ids`. Windows are cut at multiples of the window size, so that each window
/// keeps its task when the repo grows, and those at the ends of `ids` are cut
/// short.
fn task_windows(
    ids: Range<u64>,
    window_size: NonZeroU64,
    shard: TaskShard,
) -> impl Iterator<Item = Range<u64>> {
    let size = window_size.get();
    let Range { start, end } = ids;
    let windows = (start < end).then(|| start / size..=(end - 1) / size);
    windows
        .into_iter()
        .flatten()
        .filter(move |window| *window % shard.count == shard.index)
        .map(move |window| {
            let lowest = window * size;
            lowest.max(start)..lowest.saturating_add(size).min(end)
        })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use bonsai_hg_mapping::BonsaiHgMapping;
    use bookmarks::Bookmarks;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphWriter;
    use fbinit::FacebookInit;
    use filestore::FilestoreConfig;
    use mercurial_derivation::RootHgAugmentedManifestId;
    use mononoke_macros::mononoke;
    use mononoke_types::DerivableType;
    use mononoke_types::RepositoryId;
    use mutable_counters::MutableCounters;
    use repo_blobstore::RepoBlobstore;
    use repo_derived_data::RepoDerivedData;
    use repo_derived_data::RepoDerivedDataRef;
    use repo_identity::RepoIdentity;
    use sql_commit_graph_storage::CommitGraphBulkFetcher;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::CreateCommitContext;
    use throttledblob::ThrottleOptions;
    use tokio::time::Instant;

    use super::*;
    use crate::Failure;
    use crate::repo_backfill::NoReplicationLag;
    use crate::repo_backfill::set_switches;

    fn shard(index: u64, count: u64) -> TaskShard {
        TaskShard::try_new(index, count).expect("the test's shard should be valid")
    }

    fn size(size: u64) -> NonZeroU64 {
        NonZeroU64::new(size).expect("the test's window size should not be zero")
    }

    fn windows(ids: Range<u64>, window_size: u64, shard: TaskShard) -> Vec<Range<u64>> {
        task_windows(ids, size(window_size), shard).collect()
    }

    /// The windows a task walks among `ids` when the repo's commits have the
    /// ids in `commits`, and how many times it read the lowest id of a range.
    async fn windows_with_commits(
        ids: Range<u64>,
        window_size: u64,
        shard: TaskShard,
        commits: &BTreeSet<u64>,
    ) -> Result<(Vec<Range<u64>>, usize)> {
        let reads = Cell::new(0);
        let lowest_id = |ids: Range<u64>| {
            reads.set(reads.get() + 1);
            future::ok(commits.range(ids).next().copied())
        };
        let mut windows = Vec::new();
        let mut from = ids.start;
        while let Some(window) =
            next_task_window(from..ids.end, size(window_size), shard, lowest_id).await?
        {
            from = window.end;
            windows.push(window);
        }
        Ok((windows, reads.get()))
    }

    #[mononoke::test]
    async fn a_task_skips_empty_stretches_of_a_sparse_repo_with_few_reads() -> Result<()> {
        // Given a repo with five commits among 100 million ids, in windows of
        // 1,000 ids shared by 50 tasks. Task 2 takes the windows whose number
        // modulo 50 is 2, about 2,000 of them. The commit with id 2,123 is in
        // its window 2, the commits with ids 7,102,500 and 7,102,999 are in
        // its window 7,102, and the others are in other tasks' windows 5,000
        // and 99,950.
        let commits = BTreeSet::from([2_123, 5_000_042, 7_102_500, 7_102_999, 99_950_001]);

        // When task 2 walks the repo.
        let (windows, reads) =
            windows_with_commits(0..100_000_000, 1_000, shard(2, 50), &commits).await?;

        // Then it walked only its two windows with commits, and read the
        // lowest id of a range once to reach the first window, twice to jump
        // over the other task's commit to the second, and twice to find that
        // nothing is left after the last other task's commit.
        assert_eq!(windows, vec![2_000..3_000, 7_102_000..7_103_000]);
        assert_eq!(reads, 5, "the task should read the lowest id five times");
        Ok(())
    }

    #[mononoke::test]
    async fn a_task_walks_every_window_of_its_own_that_has_commits() -> Result<()> {
        // Given a repo with commits in every window from id 1 to id 19, in
        // windows of 2 ids shared by 3 tasks.
        let commits: BTreeSet<u64> = (1..20).collect();

        // When each task walks the repo, it walks every window of its own and
        // no window of another task's.
        for index in 0..3 {
            let (walked, _reads) =
                windows_with_commits(1..20, 2, shard(index, 3), &commits).await?;
            assert_eq!(walked, windows(1..20, 2, shard(index, 3)), "task {index}");
        }
        Ok(())
    }

    #[mononoke::test]
    async fn a_task_finds_windows_only_within_its_ids() -> Result<()> {
        // Given commits with ids 5 and 25 in windows of 10 ids, one task.
        let commits = BTreeSet::from([5, 25]);

        // When the task walks the ids 3 to 20, its window [0, 10) is cut short
        // at id 3, and it does not see the commit above the end.
        let (walked, _reads) = windows_with_commits(3..20, 10, shard(0, 1), &commits).await?;
        assert_eq!(walked, vec![3..10]);

        // And when it starts at id 6, above the first commit, nothing is left.
        let (walked, _reads) = windows_with_commits(6..20, 10, shard(0, 1), &commits).await?;
        assert_eq!(walked, Vec::<Range<u64>>::new());
        Ok(())
    }

    #[mononoke::test]
    fn rejects_a_task_index_outside_the_task_count() {
        assert!(TaskShard::try_new(2, 2).is_err(), "index 2 of 2 tasks");
        assert!(TaskShard::try_new(0, 0).is_err(), "zero tasks");
        assert!(TaskShard::try_new(1, 2).is_ok(), "index 1 of 2 tasks");
    }

    #[mononoke::test]
    fn tasks_take_turns_at_windows_oldest_first_up_to_the_end() {
        // Ids 1 to 7 in windows of 2 are [1, 2), [2, 4), [4, 6) and [6, 8),
        // the first cut short at the lowest id. Two tasks take every other
        // window.
        assert_eq!(windows(1..8, 2, shard(0, 2)), vec![1..2, 4..6]);
        assert_eq!(windows(1..8, 2, shard(1, 2)), vec![2..4, 6..8]);
        assert_eq!(windows(1..8, 2, shard(0, 1)), vec![1..2, 2..4, 4..6, 6..8]);
        // A window that the end cuts short ends there.
        assert_eq!(windows(1..7, 2, shard(1, 2)), vec![2..4, 6..7]);
    }

    #[mononoke::test]
    fn a_task_keeps_its_windows_when_the_repo_grows() {
        // Windows are cut at multiples of the window size, so new ids above
        // add windows at the end without moving the others between tasks.
        assert_eq!(windows(1..12, 2, shard(0, 2)), vec![1..2, 4..6, 8..10]);
        assert_eq!(windows(1..12, 2, shard(1, 2)), vec![2..4, 6..8, 10..12]);
    }

    #[mononoke::test]
    fn a_task_with_no_window_of_its_own_has_nothing_to_do() {
        // Two windows, [1, 2) and [2, 4), for three tasks: the third has none.
        assert_eq!(windows(1..4, 2, shard(2, 3)), Vec::<Range<u64>>::new());
        assert_eq!(windows(5..5, 2, shard(0, 1)), Vec::<Range<u64>>::new());
    }

    #[mononoke::test]
    async fn a_walk_starts_no_window_after_one_that_failed() -> Result<()> {
        // Given three windows, of which the first fails.
        let walked = RefCell::new(Vec::new());
        let walk_window = |window: Range<u64>| {
            walked.borrow_mut().push(window.clone());
            let result = if window.start == 1 {
                Err(anyhow::anyhow!("the window failed"))
            } else {
                Ok(WindowReport {
                    window,
                    outcomes: HashMap::new(),
                })
            };
            future::ready(result)
        };

        // When they are walked, and the caller reads on after the error.
        let next_window = |ids| future::ok(task_windows(ids, size(1), shard(0, 1)).next());
        let results: Vec<_> = walk_windows(1..4, next_window, walk_window).collect().await;

        // Then the walk ended with the error, without starting another window
        // that could record progress past the failed one.
        assert!(
            matches!(results[..], [Err(_)]),
            "the walk should end with the error"
        );
        assert_eq!(walked.into_inner(), vec![1..2]);
        Ok(())
    }

    #[facet::container]
    struct TestRepo(
        dyn BonsaiHgMapping,
        dyn Bookmarks,
        CommitGraph,
        dyn CommitGraphWriter,
        CommitGraphBulkFetcher,
        FilestoreConfig,
        dyn MutableCounters,
        RepoBlobstore,
        RepoDerivedData,
        RepoIdentity,
    );

    fn backfill(repo: &TestRepo) -> RepoBackfill {
        RepoBackfill::new(
            repo.repo_derived_data().manager().clone(),
            ThrottleOptions::default(),
            Arc::new(NoReplicationLag),
        )
    }

    /// Creates commits that have no parents and returns them in the order of
    /// their commit-graph ids, which in an empty test repo start at 1.
    async fn roots(ctx: &CoreContext, repo: &TestRepo, count: usize) -> Result<Vec<ChangesetId>> {
        stream::iter(0..count)
            .then(|n| {
                CreateCommitContext::new_root(ctx, repo)
                    .add_file("file", format!("root {n}"))
                    .commit()
            })
            .try_collect()
            .await
    }

    async fn child(
        ctx: &CoreContext,
        repo: &TestRepo,
        parent: ChangesetId,
        content: &str,
    ) -> Result<ChangesetId> {
        CreateCommitContext::new(ctx, repo, vec![parent])
            .add_file("file", content)
            .commit()
            .await
    }

    async fn derive(ctx: &CoreContext, repo: &TestRepo, cs_id: ChangesetId) -> Result<()> {
        repo.repo_derived_data()
            .manager()
            .derive_heads::<RootHgAugmentedManifestId>(ctx.clone(), vec![cs_id], None, None)
            .await?;
        Ok(())
    }

    async fn has_augmented_manifest(
        ctx: &CoreContext,
        repo: &TestRepo,
        cs_id: ChangesetId,
    ) -> Result<bool> {
        Ok(repo
            .repo_derived_data()
            .manager()
            .fetch_derived::<RootHgAugmentedManifestId>(ctx, cs_id, None)
            .await?
            .is_some())
    }

    /// A commit's outcome, in a form tests can compare.
    #[derive(Debug, Eq, PartialEq)]
    enum Outcome {
        AlreadyDerived,
        Derived(u64),
        /// Blocked in the repo config at this commit.
        Blocked(ChangesetId),
        Failed,
    }

    type Summary = Vec<(Range<u64>, HashMap<ChangesetId, Outcome>)>;

    fn summary(reports: Vec<WindowReport>) -> Summary {
        reports
            .into_iter()
            .map(|report| {
                let outcomes = report
                    .outcomes
                    .into_iter()
                    .map(|(cs_id, outcome)| {
                        let outcome = match outcome {
                            CommitOutcome::AlreadyDerived => Outcome::AlreadyDerived,
                            CommitOutcome::Derived { count } => Outcome::Derived(count),
                            CommitOutcome::Failed(Failure::Blocked { commit, .. }) => {
                                Outcome::Blocked(commit)
                            }
                            CommitOutcome::Failed(_) => Outcome::Failed,
                        };
                        (cs_id, outcome)
                    })
                    .collect();
                (report.window, outcomes)
            })
            .collect()
    }

    fn derived_once(cs_ids: &[ChangesetId]) -> HashMap<ChangesetId, Outcome> {
        cs_ids
            .iter()
            .map(|cs_id| (*cs_id, Outcome::Derived(1)))
            .collect()
    }

    /// A walk that retries a failed window at once.
    fn walk(shard: TaskShard, window_size: u64, bounds: IdBounds) -> RepoWalk {
        RepoWalk {
            retries: WalkRetries {
                first_delay: Duration::ZERO,
                ..WALK_RETRIES
            },
            ..RepoWalk::new(shard, size(window_size), bounds)
        }
    }

    /// The whole repo, from its lowest id to its newest commit.
    const ALL: IdBounds = IdBounds {
        start_id: None,
        end_id: None,
    };

    async fn walk_all(ctx: &CoreContext, repo: &TestRepo, walk: &RepoWalk) -> Result<Summary> {
        Ok(summary(
            walk.walk(ctx, repo, &backfill(repo)).try_collect().await?,
        ))
    }

    #[mononoke::fbinit_test]
    async fn a_task_derives_its_windows_oldest_first_up_to_the_newest_commit(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given seven commits with ids 1 to 7.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let commits = roots(&ctx, &repo, 7).await?;

        // When task 0 of 2 walks the repo in windows of 2 ids.
        let walk = walk(shard(0, 2), 2, ALL);
        let reports = walk_all(&ctx, &repo, &walk).await?;

        // Then it derived its windows, [1, 2) and then [4, 6), and nothing in
        // the other task's windows.
        assert_eq!(
            reports,
            vec![
                (1..2, derived_once(&commits[0..1])),
                (4..6, derived_once(&commits[3..5])),
            ],
        );
        assert!(
            !has_augmented_manifest(&ctx, &repo, commits[6]).await?,
            "the commit with id 7 is in the other task's window",
        );

        // And having reached the newest commit, the task has nothing left to
        // do.
        assert_eq!(walk_all(&ctx, &repo, &walk).await?, vec![]);
        Ok(())
    }

    /// A repo whose commits have the commit-graph ids in `ids`, between 1 and
    /// `last`. Another repo's commits, which
    /// share the commit-graph table, take the other ids from 1 to `last`. The
    /// repo's commits are returned in the order of their ids.
    async fn sparse_repo(
        fb: FacebookInit,
        ctx: &CoreContext,
        ids: &[u64],
        last: u64,
    ) -> Result<(TestRepo, Vec<ChangesetId>)> {
        let mut factory = TestRepoFactory::new(fb)?;
        let repo: TestRepo = factory.with_id(RepositoryId::new(1)).build().await?;
        let other: TestRepo = factory
            .with_id(RepositoryId::new(2))
            .with_name("other")
            .build()
            .await?;
        let mut commits = Vec::new();
        for id in 1..=last {
            let owner = if ids.contains(&id) { &repo } else { &other };
            let cs_id = CreateCommitContext::new_root(ctx, owner)
                .add_file("file", format!("id {id}"))
                .commit()
                .await?;
            if ids.contains(&id) {
                commits.push(cs_id);
            }
        }
        Ok((repo, commits))
    }

    #[mononoke::fbinit_test]
    async fn a_task_skips_the_empty_windows_of_a_sparse_repo(fb: FacebookInit) -> Result<()> {
        // Given a repo whose commits have the ids 1, 12, 15 and 24 among the
        // ids 1 to 25, and two tasks walking windows of 2 ids. Task 0 takes
        // the windows [0, 2), [4, 6) and so on, which hold the commits with ids
        // 1, 12 and 24, and task 1 takes [2, 4), [6, 8) and so on, which hold
        // the commit with id 15.
        let ctx = CoreContext::test_mock(fb);
        let (repo, commits) = sparse_repo(fb, &ctx, &[1, 12, 15, 24], 25).await?;

        // When task 0 walks the repo.
        let task_0 = walk(shard(0, 2), 2, ALL);
        let reports = walk_all(&ctx, &repo, &task_0).await?;

        // Then it walked only its windows with commits, and derived their
        // commits.
        assert_eq!(
            reports,
            vec![
                (1..2, derived_once(&commits[0..1])),
                (12..14, derived_once(&commits[1..2])),
                (24..25, derived_once(&commits[3..4])),
            ],
        );
        assert!(
            !has_augmented_manifest(&ctx, &repo, commits[2]).await?,
            "the commit with id 15 is in the other task's window",
        );

        // And when task 1 walks the repo, it derives only the commit in its
        // window.
        let task_1 = walk(shard(1, 2), 2, ALL);
        assert_eq!(
            walk_all(&ctx, &repo, &task_1).await?,
            vec![(14..16, derived_once(&commits[2..3]))],
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn a_task_that_skipped_empty_windows_resumes_after_its_last_finished_window(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given a repo whose commits have the ids 1, 12 and 24 among the ids 1
        // to 25, and a single task walking windows of 2 ids that finished two
        // windows and stopped.
        let ctx = CoreContext::test_mock(fb);
        let (repo, commits) = sparse_repo(fb, &ctx, &[1, 12, 24], 25).await?;
        let walk = walk(shard(0, 1), 2, ALL);
        let backfill = backfill(&repo);
        let finished: Vec<_> = walk
            .walk(&ctx, &repo, &backfill)
            .take(2)
            .try_collect()
            .await?;
        assert_eq!(
            finished
                .iter()
                .map(|report| report.window.clone())
                .collect::<Vec<_>>(),
            vec![1..2, 12..14],
        );

        // When a new walk by the same task starts.
        let reports = walk_all(&ctx, &repo, &walk).await?;

        // Then it goes on above the second window, skipping the empty ones.
        assert_eq!(reports, vec![(24..25, derived_once(&commits[2..3]))]);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn a_walk_covers_the_ids_from_the_start_id_to_the_end_id(fb: FacebookInit) -> Result<()> {
        // Given six commits with ids 1 to 6.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let commits = roots(&ctx, &repo, 6).await?;

        // When a single task walks the repo from id 2 to id 4.
        let bounds = IdBounds {
            start_id: Some(2),
            end_id: Some(4),
        };
        let walk = walk(shard(0, 1), 10, bounds);
        let reports = walk_all(&ctx, &repo, &walk).await?;

        // Then it derived ids 2 to 4 only.
        assert_eq!(reports, vec![(2..5, derived_once(&commits[1..4]))]);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn a_window_lists_only_its_own_commits(fb: FacebookInit) -> Result<()> {
        // Given a derived commit (id 1), an unrelated commit (id 2), an
        // underived child of the first (id 3) and a child of that child
        // (id 4).
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let [base, unrelated] = roots(&ctx, &repo, 2).await?[..] else {
            unreachable!("two roots were created");
        };
        derive(&ctx, &repo, base).await?;
        let parent = child(&ctx, &repo, base, "parent").await?;
        let draft = child(&ctx, &repo, parent, "draft").await?;

        // When task 0 of 2 walks the repo in windows of 2 ids, which are
        // [1, 2) and [4, 5), before task 1 walks [2, 4).
        let walk = walk(shard(0, 2), 2, ALL);
        let reports = walk_all(&ctx, &repo, &walk).await?;

        // Then the window [4, 5) listed only its own commit, whose derivation
        // also derived its parent in the other task's window.
        assert_eq!(
            reports,
            vec![
                (1..2, HashMap::from([(base, Outcome::AlreadyDerived)])),
                (4..5, HashMap::from([(draft, Outcome::Derived(2))])),
            ],
        );
        assert!(
            has_augmented_manifest(&ctx, &repo, parent).await?,
            "the parent should be derived as the draft's ancestor",
        );
        assert!(
            !has_augmented_manifest(&ctx, &repo, unrelated).await?,
            "the commit with id 2 is in the other task's window",
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn a_walk_records_commits_blocked_in_config_in_their_own_windows(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given a derived commit (id 1), a child of it whose derivation the
        // repo config blocks (id 2), a stack of two commits on that child
        // (ids 3 and 4), and another child of the first commit (id 5).
        let ctx = CoreContext::test_mock(fb);
        let mut factory = TestRepoFactory::new(fb)?;
        let repo: TestRepo = factory.build().await?;
        let base = roots(&ctx, &repo, 1).await?[0];
        derive(&ctx, &repo, base).await?;
        let blocked = child(&ctx, &repo, base, "blocked").await?;
        let middle = child(&ctx, &repo, blocked, "middle").await?;
        let top = child(&ctx, &repo, middle, "top").await?;
        let other = child(&ctx, &repo, base, "other").await?;
        let repo: TestRepo = factory
            .with_config_override(|config| {
                config
                    .derived_data_config
                    .blocked_derivation
                    .insert(blocked, None);
            })
            .build()
            .await?;

        // When a single task walks the repo in windows of 1 id.
        let walk = walk(shard(0, 1), 1, ALL);
        let reports = walk_all(&ctx, &repo, &walk).await?;

        // Then the blocked commit was recorded as blocked in its own window,
        // each commit above it was recorded as blocked at it in its own
        // window, and the other commit was derived.
        assert_eq!(
            reports,
            vec![
                (1..2, HashMap::from([(base, Outcome::AlreadyDerived)])),
                (2..3, HashMap::from([(blocked, Outcome::Blocked(blocked))])),
                (3..4, HashMap::from([(middle, Outcome::Blocked(blocked))])),
                (4..5, HashMap::from([(top, Outcome::Blocked(blocked))])),
                (5..6, derived_once(&[other])),
            ],
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn tasks_leave_no_commit_out_when_the_repo_grows_between_runs(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given six commits with ids 1 to 6, and two tasks walking windows of
        // 1 id, of which the first finished two windows and the second none.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let commits = roots(&ctx, &repo, 6).await?;
        let tasks = [0, 1].map(|index| walk(shard(index, 2), 1, ALL));
        let backfill = backfill(&repo);
        let _: Vec<_> = tasks[0]
            .walk(&ctx, &repo, &backfill)
            .take(2)
            .try_collect()
            .await?;

        // When a commit is added and both tasks walk to the end.
        child(&ctx, &repo, commits[5], "new").await?;
        for task in &tasks {
            walk_all(&ctx, &repo, task).await?;
        }

        // Then every one of the first six commits was derived.
        for (id, cs_id) in (1..).zip(&commits) {
            assert!(
                has_augmented_manifest(&ctx, &repo, *cs_id).await?,
                "the commit with id {id} should be derived",
            );
        }
        Ok(())
    }

    /// A repo where augmented manifests are not enabled for derivation, so
    /// that deriving any window fails as a whole.
    async fn repo_that_cannot_derive(fb: FacebookInit) -> Result<TestRepo> {
        Ok(TestRepoFactory::new(fb)?
            .with_config_override(|config| {
                let derived_data = &mut config.derived_data_config;
                derived_data
                    .get_active_config_mut()
                    .expect("the test repo should have an enabled derived data config")
                    .types
                    .remove(&DerivableType::HgAugmentedManifests);
                derived_data
                    .extra_types_available_for_read
                    .insert(DerivableType::HgAugmentedManifests);
            })
            .build()
            .await?)
    }

    #[mononoke::fbinit_test]
    async fn a_window_that_fails_is_not_recorded_as_finished(fb: FacebookInit) -> Result<()> {
        // Given a repo where augmented manifests are not enabled for
        // derivation, so that deriving any window fails as a whole.
        let ctx = CoreContext::test_mock(fb);
        let repo = repo_that_cannot_derive(fb).await?;
        roots(&ctx, &repo, 2).await?;
        let walk = walk(shard(0, 1), 1, ALL);

        // When a task walks its two windows, and its caller keeps reading
        // after an error.
        let results: Vec<_> = walk.walk(&ctx, &repo, &backfill(&repo)).collect().await;

        // Then the walk stopped at the first window's error, and the task has
        // no finished window to resume from.
        assert!(
            matches!(results[..], [Err(_)]),
            "the walk should end with the first window's error",
        );
        assert_eq!(
            repo.mutable_counters()
                .get_counter(&ctx, &walk.counter_name())
                .await?,
            None,
            "no window should be recorded as finished",
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn a_walk_from_another_start_id_does_not_resume_from_the_first(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given four commits with ids 1 to 4, and a task that walked from
        // id 3 to the end.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let commits = roots(&ctx, &repo, 4).await?;
        let from_three = IdBounds {
            start_id: Some(3),
            end_id: None,
        };
        walk_all(&ctx, &repo, &walk(shard(0, 1), 1, from_three)).await?;

        // When the same task walks the whole repo.
        let reports = walk_all(&ctx, &repo, &walk(shard(0, 1), 1, ALL)).await?;

        // Then it starts at the lowest id rather than where the first walk
        // stopped.
        assert_eq!(
            reports,
            vec![
                (1..2, derived_once(&commits[0..1])),
                (2..3, derived_once(&commits[1..2])),
                (3..4, HashMap::from([(commits[2], Outcome::AlreadyDerived)])),
                (4..5, HashMap::from([(commits[3], Outcome::AlreadyDerived)])),
            ],
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn a_task_resumes_after_its_last_finished_window(fb: FacebookInit) -> Result<()> {
        // Given four commits with ids 1 to 4.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let commits = roots(&ctx, &repo, 4).await?;
        let walk = walk(shard(0, 1), 1, ALL);

        // When a task finishes two windows and stops.
        let backfill = backfill(&repo);
        let finished: Vec<_> = walk
            .walk(&ctx, &repo, &backfill)
            .take(2)
            .try_collect()
            .await?;
        assert_eq!(
            finished
                .iter()
                .map(|r| r.window.clone())
                .collect::<Vec<_>>(),
            vec![1..2, 2..3],
        );

        // Then a new walk by the same task goes on above them.
        let reports = walk_all(&ctx, &repo, &walk).await?;
        assert_eq!(
            reports,
            vec![
                (3..4, derived_once(&commits[2..3])),
                (4..5, derived_once(&commits[3..4])),
            ],
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn a_paused_repo_walks_no_window_until_resumed(fb: FacebookInit) -> Result<()> {
        // Given a repo whose only commit is derived, so that its window has
        // nothing to derive, and a backfill paused for this repo and resumed a
        // minute later.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let root = roots(&ctx, &repo, 1).await?[0];
        derive(&ctx, &repo, root).await?;
        let walk = walk(shard(0, 1), 10, ALL);
        let backfill = backfill(&repo);
        tokio::time::pause();
        set_switches(5, true);
        let start = Instant::now();
        let resume = async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            set_switches(5, false);
        };

        // When a single task walks the repo.
        let (reports, ()) = tokio::join!(
            walk.walk(&ctx, &repo, &backfill)
                .map_ok(|report| (report, start.elapsed()))
                .try_collect::<Vec<_>>(),
            resume
        );

        // Then the window was walked only once the repo was resumed, so a
        // paused repo's windows are not even listed.
        let (reports, times): (Vec<_>, Vec<_>) = reports?.into_iter().unzip();
        assert_eq!(
            summary(reports),
            vec![(1..2, HashMap::from([(root, Outcome::AlreadyDerived)]))]
        );
        assert!(
            times[0] >= Duration::from_secs(60),
            "the window should be walked after resuming, not after {:?}",
            times[0]
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn a_paused_repo_does_not_retry_a_failed_window_until_resumed(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given a repo whose windows fail as a whole, a walk that retries a
        // failed window after 10, 20 and 40 seconds, and a backfill paused for
        // this repo one second after the walk starts, once the window's first
        // attempt has failed, and resumed after 1,000 seconds.
        let ctx = CoreContext::test_mock(fb);
        let repo = repo_that_cannot_derive(fb).await?;
        roots(&ctx, &repo, 1).await?;
        let walk = RepoWalk {
            retries: WalkRetries {
                first_delay: Duration::from_secs(10),
                ..WALK_RETRIES
            },
            ..RepoWalk::new(shard(0, 1), size(10), ALL)
        };
        let backfill = backfill(&repo);
        tokio::time::pause();
        set_switches(5, false);
        let start = Instant::now();
        let pause_and_resume = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            set_switches(5, true);
            tokio::time::sleep(Duration::from_secs(999)).await;
            set_switches(5, false);
        };

        // When a single task walks the repo.
        let (results, ()) = tokio::join!(
            async {
                let results: Vec<_> = walk.walk(&ctx, &repo, &backfill).collect().await;
                (results, start.elapsed())
            },
            pause_and_resume
        );

        // Then the walk ended with the window's error only after the repo was
        // resumed, because the retries waited while it was paused, rather
        // than after the 70 seconds that the retries take on their own.
        let (results, elapsed) = results;
        assert!(
            matches!(results[..], [Err(_)]),
            "the walk should end with the window's error"
        );
        assert!(
            elapsed >= Duration::from_secs(1_000),
            "the retries should wait while the repo is paused, but the walk ended after {elapsed:?}",
        );
        Ok(())
    }
}
