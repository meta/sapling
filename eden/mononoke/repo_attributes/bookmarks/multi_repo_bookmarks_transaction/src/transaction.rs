/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;

use anyhow::Result;
use anyhow::anyhow;
use bookmarks::BookmarkCategory;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkKind;
use bookmarks::BookmarkName;
use bookmarks::BookmarkTransactionError;
use bookmarks::BookmarkTransactionHook;
use bookmarks::BookmarkUpdateReason;
use context::CoreContext;
use dbbookmarks::transaction::AddBookmarkLog;
use dbbookmarks::transaction::AddBookmarkLogLock;
use dbbookmarks::transaction::BookmarkLogWriter;
use dbbookmarks::transaction::DeleteBookmarkIf;
use dbbookmarks::transaction::FindMaxBookmarkLogId;
use dbbookmarks::transaction::InsertBookmarks;
use dbbookmarks::transaction::MoveLockedBookmarks;
use dbbookmarks::transaction::SelectBookmarksForUpdate;
use dbbookmarks::transaction::UpdateBookmark;
use dbbookmarks::transaction::bookmark_log_guard_enabled;
use dbbookmarks::transaction::find_next_bookmark_log_lock_id;
use mononoke_types::ChangesetId;
use mononoke_types::RepositoryId;
use mononoke_types::Timestamp;
use sql_ext::Connection;
use sql_ext::Transaction as SqlTransaction;
use stats::prelude::*;

define_stats! {
    prefix = "mononoke.multi_repo_land.commit";
    success: timeseries(Rate, Sum),
    cas_failure: timeseries(Rate, Sum),
    retry: timeseries(Rate, Sum),
    retryable_error_exhausted: timeseries(Rate, Sum),
    other_error: timeseries(Rate, Sum),
    attempt_count: timeseries(Rate, Average, Sum),
}

/// Updates per locked-read + upsert pair; bounds statement size.
const UPDATE_CHUNK: usize = 100;
/// Log rows per insert statement.
const LOG_CHUNK: usize = 200;

/// A compare-and-set move; the only op kind that runs in chunks.
struct UpdateOp {
    repo_id: RepositoryId,
    bookmark: BookmarkKey,
    old_cs_id: ChangesetId,
    new_cs_id: ChangesetId,
    reason: BookmarkUpdateReason,
}

impl UpdateOp {
    fn chunk_key(&self) -> (RepositoryId, BookmarkCategory) {
        (self.repo_id, *self.bookmark.category())
    }

    fn push_log(&self, log: &mut TransactionLog) -> u64 {
        log.push(
            self.repo_id,
            &self.bookmark,
            Some(self.old_cs_id),
            Some(self.new_cs_id),
            self.reason,
        )
    }

    /// One CAS update; `LogicError` when the row is missing, moved or not publishing.
    async fn execute(
        &self,
        txn: SqlTransaction,
        log: &mut TransactionLog,
    ) -> Result<SqlTransaction, BookmarkTransactionError> {
        let log_id = self.push_log(log);
        let (txn, result) = UpdateBookmark::query_with_transaction(
            txn,
            &self.repo_id,
            &Some(log_id),
            self.bookmark.name(),
            self.bookmark.category(),
            &self.old_cs_id,
            &self.new_cs_id,
            BookmarkKind::ALL_PUBLISHING,
        )
        .await?;
        if result.affected_rows() != 1 {
            return Err(BookmarkTransactionError::LogicError);
        }
        Ok(txn)
    }
}

/// A bookmark operation to be executed atomically in a multi-repo transaction.
enum BookmarkOp {
    Update(UpdateOp),
    Create {
        repo_id: RepositoryId,
        bookmark: BookmarkKey,
        cs_id: ChangesetId,
        reason: BookmarkUpdateReason,
    },
    Delete {
        repo_id: RepositoryId,
        bookmark: BookmarkKey,
        old_cs_id: ChangesetId,
        reason: BookmarkUpdateReason,
    },
}

impl BookmarkOp {
    fn repo_id(&self) -> RepositoryId {
        match self {
            Self::Update(update) => update.repo_id,
            Self::Create { repo_id, .. } | Self::Delete { repo_id, .. } => *repo_id,
        }
    }

    fn bookmark(&self) -> &BookmarkKey {
        match self {
            Self::Update(update) => &update.bookmark,
            Self::Create { bookmark, .. } | Self::Delete { bookmark, .. } => bookmark,
        }
    }

    /// Execute this operation within a SQL transaction.
    ///
    /// Pushes a log entry and runs the appropriate SQL query.
    /// Returns `LogicError` if the CAS check fails.
    async fn execute(
        &self,
        txn: SqlTransaction,
        log: &mut TransactionLog,
    ) -> Result<SqlTransaction, BookmarkTransactionError> {
        match self {
            Self::Update(update) => update.execute(txn, log).await,
            Self::Create {
                repo_id,
                bookmark,
                cs_id,
                reason,
            } => {
                let log_id = log.push(*repo_id, bookmark, None, Some(*cs_id), *reason);
                let data = [(
                    repo_id,
                    &Some(log_id),
                    bookmark.name(),
                    bookmark.category(),
                    cs_id,
                    &BookmarkKind::PullDefaultPublishing,
                )];
                let (txn, result) = InsertBookmarks::query_with_transaction(txn, &data[..]).await?;
                if result.affected_rows() != 1 {
                    return Err(BookmarkTransactionError::LogicError);
                }
                Ok(txn)
            }
            Self::Delete {
                repo_id,
                bookmark,
                old_cs_id,
                reason,
            } => {
                log.push(*repo_id, bookmark, Some(*old_cs_id), None, *reason);
                let (txn, result) = DeleteBookmarkIf::query_with_transaction(
                    txn,
                    repo_id,
                    bookmark.name(),
                    bookmark.category(),
                    old_cs_id,
                )
                .await?;
                if result.affected_rows() != 1 {
                    return Err(BookmarkTransactionError::LogicError);
                }
                Ok(txn)
            }
        }
    }
}

/// Accumulates log entries and assigns IDs sequentially per repo.
struct TransactionLog {
    next_log_ids: HashMap<RepositoryId, u64>,
    next_lock_ids: HashMap<RepositoryId, Option<u64>>,
    entries: Vec<LogEntry>,
}

struct LogEntry {
    id: u64,
    repo_id: RepositoryId,
    bookmark: BookmarkKey,
    old: Option<ChangesetId>,
    new: Option<ChangesetId>,
    reason: BookmarkUpdateReason,
}

impl TransactionLog {
    fn new(
        next_log_ids: HashMap<RepositoryId, u64>,
        next_lock_ids: HashMap<RepositoryId, Option<u64>>,
    ) -> Self {
        Self {
            next_log_ids,
            next_lock_ids,
            entries: Vec::new(),
        }
    }

    fn push(
        &mut self,
        repo_id: RepositoryId,
        bookmark: &BookmarkKey,
        old: Option<ChangesetId>,
        new: Option<ChangesetId>,
        reason: BookmarkUpdateReason,
    ) -> u64 {
        let next_id = self.next_log_ids.entry(repo_id).or_insert(1);
        let id = *next_id;
        *next_id += 1;

        self.entries.push(LogEntry {
            id,
            repo_id,
            bookmark: bookmark.clone(),
            old,
            new,
            reason,
        });
        id
    }

    /// Write all accumulated log entries into the SQL transaction.
    async fn write(self, mut txn: SqlTransaction) -> Result<SqlTransaction> {
        let timestamp = Timestamp::now();
        let mut next_lock_ids = self.next_lock_ids;
        for chunk in self.entries.chunks(LOG_CHUNK) {
            let mut lock_data = Vec::with_capacity(chunk.len());
            for entry in chunk {
                let ticket = next_lock_ids.get_mut(&entry.repo_id).ok_or_else(|| {
                    anyhow!("missing migration guard state for repo {}", entry.repo_id)
                })?;
                if let Some(next_id) = ticket {
                    lock_data.push((entry.repo_id, *next_id));
                    *next_id = next_id
                        .checked_add(1)
                        .ok_or_else(|| anyhow!("bookmark log migration ticket range overflow"))?;
                }
            }
            let data: Vec<_> = chunk
                .iter()
                .map(|entry| {
                    (
                        &entry.id,
                        &entry.repo_id,
                        entry.bookmark.name(),
                        entry.bookmark.category(),
                        &entry.old,
                        &entry.new,
                        &entry.reason,
                        &timestamp,
                    )
                })
                .collect();
            txn = AddBookmarkLog::query_with_transaction(txn, &data[..])
                .await?
                .0;
            if !lock_data.is_empty() {
                let locks: Vec<_> = lock_data.iter().map(|(repo, id)| (repo, id)).collect();
                txn = AddBookmarkLogLock::query_with_transaction(txn, &locks[..])
                    .await?
                    .0;
            }
        }
        Ok(txn)
    }
}

/// Result of a multi-repo bookmark transaction.
pub enum MultiRepoBookmarksTransactionResult {
    /// All bookmark updates succeeded.
    Success,
    /// One or more CAS operations failed. No bookmarks were moved.
    CasFailure,
}

impl MultiRepoBookmarksTransactionResult {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }
}

/// A transaction that atomically moves bookmarks across multiple repositories.
///
/// All repos MUST share the same MySQL shard (same write_connection).
/// The transaction accumulates bookmark operations across repos and commits
/// them all in a single SQL transaction.
pub struct MultiRepoBookmarksTransaction {
    ctx: CoreContext,
    write_connection: Connection,
    /// Track (repo_id, bookmark) pairs to prevent duplicates.
    seen: HashSet<(RepositoryId, BookmarkKey)>,
    ops: Vec<BookmarkOp>,
}

impl MultiRepoBookmarksTransaction {
    pub fn new(ctx: CoreContext, write_connection: Connection) -> Self {
        Self {
            ctx,
            write_connection,
            seen: HashSet::new(),
            ops: Vec::new(),
        }
    }

    /// Add an operation, ensuring each (repo_id, bookmark) pair is used at most once.
    fn push(&mut self, op: BookmarkOp) -> Result<()> {
        if !self.seen.insert((op.repo_id(), op.bookmark().clone())) {
            return Err(anyhow!(
                "({}, {}) bookmark was already used in this transaction",
                op.repo_id(),
                op.bookmark()
            ));
        }
        self.ops.push(op);
        Ok(())
    }

    pub fn update(
        &mut self,
        repo_id: RepositoryId,
        bookmark: &BookmarkKey,
        new_cs_id: ChangesetId,
        old_cs_id: ChangesetId,
        reason: BookmarkUpdateReason,
    ) -> Result<()> {
        self.push(BookmarkOp::Update(UpdateOp {
            repo_id,
            bookmark: bookmark.clone(),
            old_cs_id,
            new_cs_id,
            reason,
        }))
    }

    pub fn create(
        &mut self,
        repo_id: RepositoryId,
        bookmark: &BookmarkKey,
        cs_id: ChangesetId,
        reason: BookmarkUpdateReason,
    ) -> Result<()> {
        self.push(BookmarkOp::Create {
            repo_id,
            bookmark: bookmark.clone(),
            cs_id,
            reason,
        })
    }

    pub fn delete(
        &mut self,
        repo_id: RepositoryId,
        bookmark: &BookmarkKey,
        old_cs_id: ChangesetId,
        reason: BookmarkUpdateReason,
    ) -> Result<()> {
        self.push(BookmarkOp::Delete {
            repo_id,
            bookmark: bookmark.clone(),
            old_cs_id,
            reason,
        })
    }

    /// Commit all ops atomically. Retries `RetryableError` up to
    /// `scm/mononoke:multi_repo_bookmark_max_retry_attempts`.
    pub async fn commit(self) -> Result<MultiRepoBookmarksTransactionResult> {
        self.commit_with_hooks(Vec::new()).await
    }

    /// Commit all ops atomically, running `hooks` inside the same SQL
    /// transaction so their rows land with the bookmark moves or not at all.
    ///
    /// This mirrors the single-repo `BookmarkTransaction::commit_with_hooks`,
    /// including running the hooks before the bookmark writes so both paths
    /// take row locks in the same order.
    ///
    /// Hooks are re-run on every retry attempt, so their writes must be safe
    /// to replay. A `LogicError` from a hook is translated into `Other`, since
    /// only a bookmark write can genuinely lose a CAS race and reporting one
    /// as contention would retry against a fault that never clears.
    pub async fn commit_with_hooks(
        self,
        hooks: Vec<BookmarkTransactionHook>,
    ) -> Result<MultiRepoBookmarksTransactionResult> {
        // No ops means no bookmark moved, so there is nothing for a hook to
        // record either — returning before running them is deliberate.
        if self.ops.is_empty() {
            return Ok(MultiRepoBookmarksTransactionResult::Success);
        }

        let max_attempts: u64 =
            justknobs::get_as::<u64>("scm/mononoke:multi_repo_bookmark_max_retry_attempts", None)
                .max(1);

        let Self {
            ctx,
            write_connection,
            ops,
            ..
        } = self;

        retry_commit_loop(&ctx, max_attempts, || {
            attempt_commit(&ctx, &write_connection, &ops, &hooks)
        })
        .await
    }
}

/// Run each hook in turn, threading the transaction through.
async fn run_transaction_hooks(
    ctx: &CoreContext,
    mut txn: SqlTransaction,
    hooks: &[BookmarkTransactionHook],
) -> Result<SqlTransaction, BookmarkTransactionError> {
    for hook in hooks {
        txn = hook(ctx.clone(), txn).await.map_err(|err| match err {
            // A hook has no bookmark to lose a CAS race on, so `LogicError`
            // from one is a bug. Reporting it as-is would surface to the caller
            // as a contended land and be retried forever against a fault that
            // is not contention, so translate it into the honest answer.
            BookmarkTransactionError::LogicError => BookmarkTransactionError::Other(anyhow!(
                "bookmark transaction hook reported a CAS failure, which only \
                 bookmark writes can do"
            )),
            other => other,
        })?;
    }
    Ok(txn)
}

/// Run a single SQL transaction attempt for the multi-repo commit.
///
/// Returns `Ok(Success)` on success, `Err(LogicError)` if any CAS check
/// failed (entire transaction rolled back), `Err(RetryableError(_))` for
/// transient SQL errors that the caller should retry, and
/// `Err(Other(_))` for non-retryable infrastructure errors.
async fn attempt_commit(
    ctx: &CoreContext,
    write_connection: &Connection,
    ops: &[BookmarkOp],
    hooks: &[BookmarkTransactionHook],
) -> Result<MultiRepoBookmarksTransactionResult, BookmarkTransactionError> {
    let repo_ids: HashSet<_> = ops.iter().map(|op| op.repo_id()).collect();

    let txn = write_connection
        .start_transaction(ctx.sql_query_telemetry())
        .await
        .map_err(BookmarkTransactionError::Other)?;

    // Hooks keep whatever error class they report, so a transient failure here
    // is retried rather than being flattened into a fatal one.
    let mut txn = run_transaction_hooks(ctx, txn, hooks).await?;

    let mut next_lock_ids = HashMap::new();
    for &repo_id in &repo_ids {
        let enabled = bookmark_log_guard_enabled(repo_id, BookmarkLogWriter::MultiRepo);
        let next_lock_id = if enabled {
            let (next_txn, id) = find_next_bookmark_log_lock_id(txn, repo_id).await?;
            txn = next_txn;
            Some(id)
        } else {
            None
        };
        next_lock_ids.insert(repo_id, next_lock_id);
    }
    let (txn, next_log_ids) = find_next_log_ids(txn, &repo_ids)
        .await
        .map_err(BookmarkTransactionError::Other)?;
    let mut log = TransactionLog::new(next_log_ids, next_lock_ids);

    let txn = execute_ops(txn, ops, &mut log).await?;

    let txn = log
        .write(txn)
        .await
        .map_err(BookmarkTransactionError::RetryableError)?;
    txn.commit()
        .await
        .map_err(BookmarkTransactionError::Other)?;
    Ok(MultiRepoBookmarksTransactionResult::Success)
}

/// A run of consecutive updates to one repo and category takes two statements
/// per chunk instead of one round trip per bookmark; everything else runs one
/// op at a time as before. Log ids stay in op order either way.
async fn execute_ops(
    mut txn: SqlTransaction,
    ops: &[BookmarkOp],
    log: &mut TransactionLog,
) -> Result<SqlTransaction, BookmarkTransactionError> {
    let mut rest = ops;
    while let Some((first, _)) = rest.split_first() {
        let run_len = match first {
            BookmarkOp::Update(first) => rest
                .iter()
                .map_while(|op| match op {
                    BookmarkOp::Update(update) if update.chunk_key() == first.chunk_key() => {
                        Some(())
                    }
                    _ => None,
                })
                .count(),
            BookmarkOp::Create { .. } | BookmarkOp::Delete { .. } => 1,
        };
        let (run, tail) = rest.split_at(run_len);
        let updates: Vec<&UpdateOp> = run
            .iter()
            .filter_map(|op| match op {
                BookmarkOp::Update(update) => Some(update),
                BookmarkOp::Create { .. } | BookmarkOp::Delete { .. } => None,
            })
            .collect();
        if updates.len() > 1 {
            for chunk in updates.chunks(UPDATE_CHUNK) {
                txn = execute_update_chunk(txn, chunk, log).await?;
            }
        } else {
            for op in run {
                txn = op.execute(txn, log).await?;
            }
        }
        rest = tail;
    }
    Ok(txn)
}

/// Lock the chunk's rows, check every CAS in the application, then move them
/// in one statement. The same predicate as the per-row `UpdateBookmark`: a
/// missing row, a moved head or a non-publishing kind is a `LogicError`. Rows
/// are looked up by name alone and written back under the category they are
/// stored with; the key's category only breaks a tie between stored rows.
async fn execute_update_chunk(
    txn: SqlTransaction,
    chunk: &[&UpdateOp],
    log: &mut TransactionLog,
) -> Result<SqlTransaction, BookmarkTransactionError> {
    let repo_id = chunk
        .first()
        .map(|update| update.repo_id)
        .ok_or_else(|| BookmarkTransactionError::Other(anyhow!("empty update chunk")))?;
    let mut seen: HashSet<&BookmarkName> = HashSet::new();
    for update in chunk {
        if !seen.insert(update.bookmark.name()) {
            return Err(BookmarkTransactionError::Other(anyhow!(
                "bookmark {} appears twice in one update chunk",
                update.bookmark
            )));
        }
    }
    let log_ids: Vec<u64> = chunk.iter().map(|update| update.push_log(log)).collect();
    let names: Vec<BookmarkName> = chunk
        .iter()
        .map(|update| update.bookmark.name().clone())
        .collect();
    let (txn, rows) =
        SelectBookmarksForUpdate::query_with_transaction(txn, &repo_id, &names[..]).await?;
    let mut current: HashMap<BookmarkName, Vec<(BookmarkCategory, ChangesetId, BookmarkKind)>> =
        HashMap::new();
    for (name, category, cs_id, kind) in rows {
        current
            .entry(name)
            .or_default()
            .push((category, cs_id, kind));
    }

    let mut values = Vec::with_capacity(chunk.len());
    for (update, log_id) in chunk.iter().zip(log_ids) {
        let stored = current
            .get(update.bookmark.name())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let row = match stored {
            [row] => Some(row),
            _ => stored
                .iter()
                .find(|(category, ..)| category == update.bookmark.category()),
        };
        match row {
            Some((category, cs_id, kind))
                if *cs_id == update.old_cs_id && BookmarkKind::ALL_PUBLISHING.contains(kind) =>
            {
                values.push((
                    repo_id,
                    Some(log_id),
                    update.bookmark.name().clone(),
                    *category,
                    update.new_cs_id,
                    *kind,
                ));
            }
            _ => return Err(BookmarkTransactionError::LogicError),
        }
    }
    let refs: Vec<_> = values
        .iter()
        .map(|(repo_id, log_id, name, category, cs_id, kind)| {
            (repo_id, log_id, name, category, cs_id, kind)
        })
        .collect();
    // Rows are locked and checked above, so the affected count carries no
    // information here (MySQL reports 2 per updated row).
    let (txn, _) = MoveLockedBookmarks::query_with_transaction(txn, &refs[..]).await?;
    Ok(txn)
}

/// Execute `do_attempt` with retry-on-transient-error semantics.
///
/// Generic over the attempt closure so unit tests can drive the retry
/// machinery directly without needing a SQL fault-injection hook.
#[doc(hidden)]
pub async fn retry_commit_loop<F, Fut>(
    ctx: &CoreContext,
    max_attempts: u64,
    mut do_attempt: F,
) -> Result<MultiRepoBookmarksTransactionResult>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<MultiRepoBookmarksTransactionResult, BookmarkTransactionError>>,
{
    let mut attempt = 0u64;
    loop {
        attempt += 1;
        match do_attempt().await {
            Ok(result) => {
                STATS::attempt_count.add_value(attempt as i64);
                STATS::success.add_value(1);
                return Ok(result);
            }
            Err(BookmarkTransactionError::LogicError) => {
                STATS::attempt_count.add_value(attempt as i64);
                STATS::cas_failure.add_value(1);
                return Ok(MultiRepoBookmarksTransactionResult::CasFailure);
            }
            Err(BookmarkTransactionError::AlreadyProcessed) => {
                // Multi-repo transactions do not use modern_sync mirror log ids,
                // so this variant is unreachable here. Treat it as an error.
                STATS::attempt_count.add_value(attempt as i64);
                STATS::other_error.add_value(1);
                return Err(anyhow!(
                    "Multi-repo bookmark transaction returned AlreadyProcessed unexpectedly"
                ));
            }
            Err(BookmarkTransactionError::RetryableError(err)) if attempt < max_attempts => {
                STATS::retry.add_value(1);
                ctx.scuba()
                    .clone()
                    .add("log_tag", "multi_repo_commit_retryable_error")
                    .add("attempt", attempt as i64)
                    .add("error", format!("{err:#}"))
                    .unsampled()
                    .log();
                continue;
            }
            Err(BookmarkTransactionError::RetryableError(err)) => {
                STATS::attempt_count.add_value(attempt as i64);
                STATS::retryable_error_exhausted.add_value(1);
                return Err(err.context(format!(
                    "Multi-repo bookmark transaction exhausted {max_attempts} retry attempts"
                )));
            }
            Err(BookmarkTransactionError::Other(err)) => {
                STATS::attempt_count.add_value(attempt as i64);
                STATS::other_error.add_value(1);
                return Err(err);
            }
        }
    }
}

/// Find the next bookmark update log ID for each repo within the transaction.
async fn find_next_log_ids(
    mut txn: SqlTransaction,
    repo_ids: &HashSet<RepositoryId>,
) -> Result<(SqlTransaction, HashMap<RepositoryId, u64>)> {
    let mut next_ids = HashMap::new();
    for &repo_id in repo_ids {
        let (txn_, max_id_entries) =
            FindMaxBookmarkLogId::query_with_transaction(txn, &repo_id).await?;
        txn = txn_;
        let next_id = match &max_id_entries[..] {
            [(None,)] => 1,
            [(Some(max_existing),)] => *max_existing + 1,
            _ => {
                return Err(anyhow!(
                    "FindMaxBookmarkLogId returned multiple entries for repo {repo_id}: {max_id_entries:?}"
                ));
            }
        };
        next_ids.insert(repo_id, next_id);
    }
    Ok((txn, next_ids))
}
