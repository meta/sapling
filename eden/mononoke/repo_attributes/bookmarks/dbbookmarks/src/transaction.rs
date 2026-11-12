/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use bookmarks::BookmarkCategory;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkKind;
use bookmarks::BookmarkMoveAlreadyProcessed;
use bookmarks::BookmarkName;
use bookmarks::BookmarkTransaction;
use bookmarks::BookmarkTransactionError;
use bookmarks::BookmarkTransactionHook;
use bookmarks::BookmarkUpdateReason;
use bookmarks::MirrorBookmarkMove;
use context::CoreContext;
use context::PerfCounterType;
use futures::future;
use futures::future::BoxFuture;
use futures::future::FutureExt;
use mononoke_types::ChangesetId;
use mononoke_types::RepositoryId;
use mononoke_types::Timestamp;
use sql_ext::Connection;
use sql_ext::Transaction as SqlTransaction;
use sql_ext::mononoke_queries;
use stats::prelude::*;

use crate::store::SelectBookmark;
use crate::store::SelectBookmarkForUpdate;

const MAX_BOOKMARK_TRANSACTION_ATTEMPT_COUNT: usize = 10;

define_stats! {
    prefix = "mononoke.dbbookmarks";
    bookmarks_update_log_insert_success: timeseries(Rate, Sum),
    bookmarks_update_log_insert_success_attempt_count: timeseries(Rate, Average, Sum),
    bookmarks_update_log_insert_retry: timeseries(Rate, Sum),
    bookmarks_insert_retryable_error: timeseries(Rate, Sum),
    bookmarks_insert_retryable_error_attempt_count: timeseries(Rate, Average, Sum),
    bookmarks_insert_logic_error: timeseries(Rate, Sum),
    bookmarks_insert_logic_error_attempt_count: timeseries(Rate, Average, Sum),
    bookmarks_insert_other_error: timeseries(Rate, Sum),
    bookmarks_insert_other_error_attempt_count: timeseries(Rate, Average, Sum),
    bookmarks_insert_already_processed: timeseries(Rate, Sum),
}

mononoke_queries! {
    write ReplaceBookmarks(
        values: (repo_id: RepositoryId, log_id: Option<u64>, name: BookmarkName, category: BookmarkCategory, changeset_id: ChangesetId)
    ) {
        none,
        "REPLACE INTO bookmarks (repo_id, log_id, name, category, changeset_id) VALUES {values}"
    }

    pub write InsertBookmarks(
        values: (repo_id: RepositoryId, log_id: Option<u64>, name: BookmarkName, category: BookmarkCategory, changeset_id: ChangesetId, kind: BookmarkKind)
    ) {
        insert_or_ignore,
        "{insert_or_ignore} INTO bookmarks (repo_id, log_id, name, category, changeset_id, hg_kind) VALUES {values}"
    }

    pub write UpdateBookmark(
        repo_id: RepositoryId,
        log_id: Option<u64>,
        name: BookmarkName,
        category: BookmarkCategory,
        old_id: ChangesetId,
        new_id: ChangesetId,
        >list kinds: BookmarkKind
    ) {
        none,
        "UPDATE bookmarks
         SET log_id = {log_id}, changeset_id = {new_id}
         WHERE repo_id = {repo_id}
           AND name = {name}
           AND category = {category}
           AND changeset_id = {old_id}
           AND hg_kind IN {kinds}"
    }

    write DeleteBookmark(repo_id: RepositoryId, name: BookmarkName, category: BookmarkCategory) {
        none,
        "DELETE FROM bookmarks
         WHERE repo_id = {repo_id}
           AND name = {name}
           AND category = {category}"
    }

    pub write DeleteBookmarkIf(repo_id: RepositoryId, name: BookmarkName, category: BookmarkCategory, changeset_id: ChangesetId) {
        none,
        "DELETE FROM bookmarks
         WHERE repo_id = {repo_id}
           AND name = {name}
           AND category = {category}
           AND changeset_id = {changeset_id}"
    }

    pub read FindMaxBookmarkLogId(repo_id: RepositoryId) -> (Option<u64>) {
        "SELECT MAX(id) FROM bookmarks_update_log WHERE repo_id = {repo_id}"
    }

    pub read FindMaxBookmarkLogIdsByName(
        repo_id: RepositoryId,
        >list names: BookmarkName
    ) -> (BookmarkName, u64) {
        "SELECT name, MAX(id) FROM bookmarks_update_log
         WHERE repo_id = {repo_id} AND name IN {names}
         GROUP BY name"
    }

    pub read FindMaxBookmarkLogLockId(repo_id: RepositoryId) -> (Option<u64>) {
        "SELECT MAX(id) FROM bookmarks_update_log_lock WHERE repo_id = {repo_id}"
    }

    pub write AddBookmarkLogLock(values: (repo_id: RepositoryId, id: u64)) {
        none,
        mysql("INSERT INTO bookmarks_update_log_lock (repo_id, id) VALUES {values}")
        sqlite("INSERT OR ROLLBACK INTO bookmarks_update_log_lock (repo_id, id) VALUES {values}")
    }

    pub write AddBookmarkLog(
        values: (
            id: u64,
            repo_id: RepositoryId,
            name: BookmarkName,
            category: BookmarkCategory,
            from_changeset_id: Option<ChangesetId>,
            to_changeset_id: Option<ChangesetId>,
            reason: BookmarkUpdateReason,
            timestamp: Timestamp,
        ),
    ) {
        none,
        mysql("INSERT INTO bookmarks_update_log
         (id, repo_id, name, category, from_changeset_id, to_changeset_id, reason, timestamp)
         VALUES {values}")
        sqlite("INSERT OR ROLLBACK INTO bookmarks_update_log
         (id, repo_id, name, category, from_changeset_id, to_changeset_id, reason, timestamp)
         VALUES {values}")
    }

    pub read SelectBookmarksForUpdate(
        repo_id: RepositoryId,
        >list names: BookmarkName
    ) -> (BookmarkName, BookmarkCategory, ChangesetId, BookmarkKind) {
        mysql("SELECT name, category, changeset_id, hg_kind
         FROM bookmarks
         WHERE repo_id = {repo_id}
           AND name IN {names}
         FOR UPDATE")
        sqlite("SELECT name, category, changeset_id, hg_kind
         FROM bookmarks
         WHERE repo_id = {repo_id}
           AND name IN {names}")
    }

    // Only for rows the caller has locked and CAS-checked in the same transaction.
    pub write MoveLockedBookmarks(
        values: (repo_id: RepositoryId, log_id: Option<u64>, name: BookmarkName, category: BookmarkCategory, changeset_id: ChangesetId, kind: BookmarkKind)
    ) {
        none,
        mysql("INSERT INTO bookmarks (repo_id, log_id, name, category, changeset_id, hg_kind)
         VALUES {values}
         ON DUPLICATE KEY UPDATE log_id = VALUES(log_id), changeset_id = VALUES(changeset_id)")
        sqlite("INSERT INTO bookmarks (repo_id, log_id, name, category, changeset_id, hg_kind)
         VALUES {values}
         ON CONFLICT(repo_id, name, category) DO UPDATE SET log_id = excluded.log_id, changeset_id = excluded.changeset_id")
    }
}

/// Bookmark-log producer whose migration controls apply to an attempt.
#[derive(Clone, Copy, Debug)]
pub enum BookmarkLogWriter {
    SingleRepo,
    MultiRepo,
}

/// Select this producer's guard activation using the decimal repository ID.
pub fn bookmark_log_guard_enabled(repo_id: RepositoryId, writer: BookmarkLogWriter) -> bool {
    let knob = match writer {
        BookmarkLogWriter::SingleRepo => {
            "scm/mononoke_bookmark_log_migration:bookmark_log_guard_enabled"
        }
        BookmarkLogWriter::MultiRepo => {
            "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_guard_enabled"
        }
    };
    let repo_key = repo_id.to_string();
    justknobs::eval(knob, Some(&repo_key), Some(&repo_key))
}

/// Read the next repo-scoped migration ticket before reading any actual log IDs.
/// The ticket is committed with the log rows; a competing ticket rolls back the attempt.
pub async fn find_next_bookmark_log_lock_id(
    txn: SqlTransaction,
    repo_id: RepositoryId,
) -> Result<(SqlTransaction, u64)> {
    let (txn, rows) = FindMaxBookmarkLogLockId::query_with_transaction(txn, &repo_id).await?;
    let next_id = match rows.as_slice() {
        [(None,)] => 1,
        [(Some(id),)] => id
            .checked_add(1)
            .ok_or_else(|| anyhow!("bookmark log migration ticket overflow for repo {repo_id}"))?,
        _ => {
            return Err(anyhow!(
                "unexpected migration ticket rows for repo {repo_id}: {rows:?}"
            ));
        }
    };
    Ok((txn, next_id))
}

/// Next IDs for either the legacy repository stream or independent name streams.
pub enum NextBookmarkLogId {
    Repo(u64),
    ByName(HashMap<BookmarkName, u64>),
}

impl NextBookmarkLogId {
    /// Take the next ID for a bookmark name; categories share the name stream.
    pub fn next(&mut self, name: &BookmarkName) -> Result<u64> {
        let next = match self {
            Self::Repo(next) => next,
            Self::ByName(ids) => ids
                .get_mut(name)
                .ok_or_else(|| anyhow!("missing next log ID for bookmark {name}"))?,
        };
        let id = *next;
        *next = next
            .checked_add(1)
            .ok_or_else(|| anyhow!("bookmark log ID overflow for {name}"))?;
        Ok(id)
    }
}

/// Select this producer's allocation mode using the decimal repository ID.
pub fn bookmark_log_ids_by_name(repo_id: RepositoryId, writer: BookmarkLogWriter) -> bool {
    let knob = match writer {
        BookmarkLogWriter::SingleRepo => {
            "scm/mononoke_bookmark_log_migration:bookmark_log_ids_by_name"
        }
        BookmarkLogWriter::MultiRepo => {
            "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_ids_by_name"
        }
    };
    let repo_key = repo_id.to_string();
    justknobs::eval(knob, Some(&repo_key), Some(&repo_key))
}

/// Reject name allocation without activation, using the caller's latched flags.
pub fn bookmark_log_guard_enabled_for_allocation(
    repo_id: RepositoryId,
    by_name: bool,
    enabled: bool,
) -> Result<bool> {
    if by_name && !enabled {
        return Err(anyhow!(
            "bookmark-name log allocation requires migration guard activation for repo {repo_id}"
        ));
    }
    Ok(enabled)
}

/// Read allocation state after capturing any required migration guard ticket.
pub async fn find_next_bookmark_log_ids(
    txn: SqlTransaction,
    repo_id: RepositoryId,
    names: &HashSet<BookmarkName>,
    by_name: bool,
) -> Result<(SqlTransaction, NextBookmarkLogId)> {
    if !by_name {
        let (txn, rows) = FindMaxBookmarkLogId::query_with_transaction(txn, &repo_id).await?;
        let next = match rows.as_slice() {
            [(None,)] => 1,
            [(Some(id),)] => id
                .checked_add(1)
                .ok_or_else(|| anyhow!("bookmark log ID overflow for repo {repo_id}"))?,
            _ => {
                return Err(anyhow!(
                    "unexpected max log ID rows for repo {repo_id}: {rows:?}"
                ));
            }
        };
        return Ok((txn, NextBookmarkLogId::Repo(next)));
    }

    let mut ids: HashMap<BookmarkName, u64> = names.iter().cloned().map(|name| (name, 1)).collect();
    if names.is_empty() {
        return Ok((txn, NextBookmarkLogId::ByName(ids)));
    }
    let names: Vec<_> = names.iter().cloned().collect();
    let (txn, rows) =
        FindMaxBookmarkLogIdsByName::query_with_transaction(txn, &repo_id, &names).await?;
    for (name, max_id) in rows {
        let next = ids.get_mut(&name).ok_or_else(|| {
            anyhow!("max log query returned unexpected bookmark {name} in repo {repo_id}")
        })?;
        *next = max_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("bookmark log ID overflow for {name} in repo {repo_id}"))?;
    }
    Ok((txn, NextBookmarkLogId::ByName(ids)))
}

struct NewUpdateLogEntry {
    /// The old bookmarked changeset (if known)
    old: Option<ChangesetId>,

    /// The new bookmarked changeset (or None if the bookmark is being
    /// deleted).
    new: Option<ChangesetId>,

    /// The reason for the update.
    reason: BookmarkUpdateReason,
}

impl NewUpdateLogEntry {
    fn new(
        old: Option<ChangesetId>,
        new: Option<ChangesetId>,
        reason: BookmarkUpdateReason,
    ) -> Result<NewUpdateLogEntry> {
        Ok(NewUpdateLogEntry { old, new, reason })
    }
}

struct SqlBookmarksTransactionPayload {
    /// The repository we are updating.
    repo_id: RepositoryId,

    /// Operations to force-set a bookmark to a changeset.
    force_sets: Vec<(BookmarkKey, ChangesetId, NewUpdateLogEntry)>,

    /// Operations to create a bookmark.
    creates: Vec<(
        BookmarkKey,
        ChangesetId,
        BookmarkKind,
        Option<NewUpdateLogEntry>,
    )>,

    /// Operations to update a bookmark from an old id to a new id, provided
    /// it has a matching kind.
    updates: Vec<(
        BookmarkKey,
        ChangesetId,
        ChangesetId,
        &'static [BookmarkKind],
        Option<NewUpdateLogEntry>,
    )>,

    /// Operations to force-delete a bookmark.
    force_deletes: Vec<(BookmarkKey, NewUpdateLogEntry)>,

    /// Operations to delete a bookmark with an old id.
    deletes: Vec<(BookmarkKey, ChangesetId, Option<NewUpdateLogEntry>)>,

    /// modern_sync batch mirror operations. Each applies a contiguous chain of
    /// source bookmark moves to a `*_shadow` replica as one compare-and-swap and
    /// one bookmarks_update_log row per move, reusing the source ids. The
    /// `BookmarkKind` is used only when the batch creates the bookmark. See
    /// `store_mirror_batches`.
    mirror_batches: Vec<(BookmarkKey, BookmarkKind, Vec<MirrorBookmarkMove>)>,
}

/// Structure representing the log entries to insert when executing a
/// SqlBookmarksTransactionPayload.
struct TransactionLogUpdates<'a> {
    next_log_id: NextBookmarkLogId,
    log_entries: Vec<(u64, &'a BookmarkKey, &'a NewUpdateLogEntry)>,
}

impl<'a> TransactionLogUpdates<'a> {
    fn new(next_log_id: NextBookmarkLogId) -> Self {
        Self {
            next_log_id,
            log_entries: Vec::new(),
        }
    }

    fn push_log_entry(
        &mut self,
        bookmark: &'a BookmarkKey,
        entry: &'a NewUpdateLogEntry,
    ) -> Result<u64> {
        let id = self.next_log_id.next(bookmark.name())?;
        self.log_entries.push((id, bookmark, entry));
        Ok(id)
    }
}

/// Check a fully-applied mirror batch for divergence. Only a replica that
/// stopped exactly at the batch's last move can be checked against that
/// move's changeset: if it sits at that log id but a different changeset,
/// it diverged, so fail hard instead of reporting success. A replica whose
/// log id is past the batch already applied later moves, so its changeset
/// is expected to differ and comparing it would report a false divergence.
fn check_mirror_replay_divergence(
    current: &[(ChangesetId, Option<u64>)],
    moves: &[MirrorBookmarkMove],
) -> Result<(), BookmarkTransactionError> {
    if let (Some(row), Some(last_move)) = (current.first(), moves.last()) {
        if row.1 == Some(last_move.log_id) && row.0 != last_move.new {
            return Err(BookmarkTransactionError::LogicError);
        }
    }
    Ok(())
}

/// Decide a lost bookmark-creation INSERT: a concurrent bookmark creation
/// committed while this batch was in flight (the pessimistic lock cannot
/// serialize creations because there is no row to lock). `fresh` is the bookmark row
/// re-read after the failed INSERT. Returns `Ok` when the winner applied
/// the whole batch, so the caller skips it as a replay; a batch that is
/// still unapplied, or applied with a different changeset, is a genuine
/// divergence and returns `LogicError`.
fn check_lost_insert_replay(
    fresh: &[(ChangesetId, Option<u64>)],
    moves: &[MirrorBookmarkMove],
) -> Result<(), BookmarkTransactionError> {
    let fresh_log_id = fresh.first().and_then(|row| row.1);
    let any_unapplied = match fresh_log_id {
        Some(id) => moves.iter().any(|m| m.log_id > id),
        None => true,
    };
    if any_unapplied {
        return Err(BookmarkTransactionError::LogicError);
    }
    check_mirror_replay_divergence(fresh, moves)
}

impl SqlBookmarksTransactionPayload {
    fn new(repo_id: RepositoryId) -> Self {
        SqlBookmarksTransactionPayload {
            repo_id,
            force_sets: Vec::new(),
            creates: Vec::new(),
            updates: Vec::new(),
            force_deletes: Vec::new(),
            deletes: Vec::new(),
            mirror_batches: Vec::new(),
        }
    }

    async fn store_log<'a>(
        &'a self,
        _ctx: &CoreContext,
        mut txn: SqlTransaction,
        log: &'a TransactionLogUpdates<'a>,
        next_lock_id: Option<u64>,
    ) -> Result<SqlTransaction> {
        let timestamp = Timestamp::now();

        for (offset, (id, bookmark, log_entry)) in log.log_entries.iter().enumerate() {
            let data = [(
                id,
                &self.repo_id,
                bookmark.name(),
                bookmark.category(),
                &log_entry.old,
                &log_entry.new,
                &log_entry.reason,
                &timestamp,
            )];
            txn = AddBookmarkLog::query_with_transaction(txn, &data[..])
                .await?
                .0;
            if let Some(next_lock_id) = next_lock_id {
                let lock_id = next_lock_id
                    .checked_add(offset.try_into()?)
                    .ok_or_else(|| anyhow!("bookmark log migration ticket range overflow"))?;
                txn = AddBookmarkLogLock::query_with_transaction(txn, &[(&self.repo_id, &lock_id)])
                    .await?
                    .0;
            }
        }
        Ok(txn)
    }

    async fn store_force_sets<'op, 'log: 'op>(
        &'log self,
        _ctx: &CoreContext,
        txn: SqlTransaction,
        log: &'op mut TransactionLogUpdates<'log>,
    ) -> Result<SqlTransaction, BookmarkTransactionError> {
        let mut data = Vec::new();
        for (bookmark, cs_id, log_entry) in self.force_sets.iter() {
            let log_id = log.push_log_entry(bookmark, log_entry)?;
            data.push((self.repo_id, Some(log_id), bookmark, cs_id));
        }
        let data = data
            .iter()
            .map(|(repo_id, log_id, bookmark, cs_id)| {
                (
                    repo_id,
                    log_id,
                    bookmark.name(),
                    bookmark.category(),
                    *cs_id,
                )
            })
            .collect::<Vec<_>>();
        let (txn, _) = ReplaceBookmarks::query_with_transaction(txn, data.as_slice()).await?;
        Ok(txn)
    }

    async fn store_creates<'op, 'log: 'op>(
        &'log self,
        _ctx: &CoreContext,
        txn: SqlTransaction,
        log: &'op mut TransactionLogUpdates<'log>,
    ) -> Result<SqlTransaction, BookmarkTransactionError> {
        let mut data = Vec::new();
        for (bookmark, cs_id, kind, maybe_log_entry) in self.creates.iter() {
            let log_id = maybe_log_entry
                .as_ref()
                .map(|log_entry| log.push_log_entry(bookmark, log_entry))
                .transpose()?;
            data.push((self.repo_id, log_id, bookmark, cs_id, kind))
        }
        let data = data
            .iter()
            .map(|(repo_id, log_id, bookmark, cs_id, kind)| {
                (
                    repo_id,
                    log_id,
                    bookmark.name(),
                    bookmark.category(),
                    *cs_id,
                    *kind,
                )
            })
            .collect::<Vec<_>>();
        let rows_to_insert = data.len() as u64;
        let (txn, result) = InsertBookmarks::query_with_transaction(txn, data.as_slice()).await?;
        if result.affected_rows() != rows_to_insert {
            return Err(BookmarkTransactionError::LogicError);
        }
        Ok(txn)
    }

    /// Apply modern_sync batch mirror moves to a `*_shadow` replica.
    ///
    /// Each batch is a contiguous chain of source bookmark moves for one
    /// bookmark, ordered by increasing log id. The replica must keep the same
    /// log ids as the source, so the moves reuse the source ids for both the
    /// compare-and-swap and the log rows. To stay idempotent when modern_sync
    /// replays a batch whose ack was lost -- and when a retry regroups the moves
    /// into a different batch -- the store applies only the moves the replica has
    /// not seen yet:
    ///
    /// Shadow replicas are read only and this path is their only writer, so the
    /// log id stored on the bookmark is always a source log id.
    ///
    /// 1. Read the bookmark's current changeset and log id with a pessimistic
    ///    lock (modern_sync mirror path only). When the row already exists,
    ///    the lock serializes concurrent replays of one chain: a replay that
    ///    arrives while another commits waits, then reads the winner's commit
    ///    and reports AlreadyProcessed instead of losing the compare-and-swap
    ///    below. Bookmark creation has no row to lock, so concurrent bookmark
    ///    creations can both reach the INSERT in step 3; the loser re-reads
    ///    there and treats a fully-applied batch as a replay instead of a
    ///    divergence.
    /// 2. Drop the moves whose log id the replica already stored. Because the
    ///    moves are ordered, the rest form a suffix. If none remain, the replica
    ///    already applied the whole chain, so return `AlreadyProcessed` and let
    ///    modern_sync advance its checkpoint. Compare the changeset only when
    ///    the replica stopped at this batch's last move; a replica that is
    ///    further ahead applied later moves, so a different changeset is
    ///    expected there rather than a sign of divergence.
    /// 3. Apply the suffix as one compare-and-swap from the first unseen move's
    ///    old changeset to the last move's new changeset, tagged with the last
    ///    move's log id. The CAS also enforces that the replica sits exactly at
    ///    the first unseen move's old changeset; if it does not, the replica has
    ///    diverged, so return `LogicError`. When the first unseen move is a
    ///    create, INSERT the bookmark instead; a lost INSERT re-reads the row
    ///    and applies the step-2 decision rather than failing outright (see
    ///    step 1).
    /// 4. Insert one bookmarks_update_log row per applied move, reusing each
    ///    move's source id, changesets, and reason.
    ///
    /// Returns the first applied move's log id, if any, for the caller's
    /// `first_id` bookkeeping.
    async fn store_mirror_batches(
        &self,
        mut txn: SqlTransaction,
    ) -> Result<(SqlTransaction, Option<u64>), BookmarkTransactionError> {
        let timestamp = Timestamp::now();
        let mut first_applied_id: Option<u64> = None;
        let mut any_applied = false;
        for (bookmark, create_kind, moves) in self.mirror_batches.iter() {
            let (txn_, current) = SelectBookmarkForUpdate::query_with_transaction(
                txn,
                &self.repo_id,
                bookmark.name(),
                bookmark.category(),
            )
            .await?;
            txn = txn_;
            let current_log_id = current.first().and_then(|row| row.1);

            let unapplied = match current_log_id {
                Some(id) => moves.iter().filter(|m| m.log_id > id).collect::<Vec<_>>(),
                None => moves.iter().collect::<Vec<_>>(),
            };
            let (first_unapplied, last) = match (unapplied.first(), unapplied.last()) {
                (Some(first), Some(last)) => (*first, *last),
                // The replica already applied this batch (a lost-ack replay),
                // so skip it after checking for divergence. If every batch
                // turns out already applied, the transaction returns
                // AlreadyProcessed below and the caller advances its
                // checkpoint.
                _ => {
                    check_mirror_replay_divergence(&current, moves)?;
                    continue;
                }
            };

            match first_unapplied.old {
                Some(old) => {
                    // Move the bookmark from the first unseen move's old
                    // changeset to the last move's new changeset. The CAS also
                    // enforces the replica sits exactly at `old`; if not, it has
                    // diverged.
                    let (txn_, result) = UpdateBookmark::query_with_transaction(
                        txn,
                        &self.repo_id,
                        &Some(last.log_id),
                        bookmark.name(),
                        bookmark.category(),
                        &old,
                        &last.new,
                        BookmarkKind::ALL_PUBLISHING,
                    )
                    .await?;
                    txn = txn_;
                    if result.affected_rows() != 1 {
                        return Err(BookmarkTransactionError::LogicError);
                    }
                }
                None => {
                    // The first unseen move is a create (its `old` is None),
                    // which happens when the bookmark has no row yet. INSERT
                    // OR IGNORE affects one row only if the bookmark is absent.
                    let create_log_id = Some(last.log_id);
                    let data = [(
                        &self.repo_id,
                        &create_log_id,
                        bookmark.name(),
                        bookmark.category(),
                        &last.new,
                        create_kind,
                    )];
                    let (txn_, result) =
                        InsertBookmarks::query_with_transaction(txn, &data[..]).await?;
                    txn = txn_;
                    if result.affected_rows() != 1 {
                        // Zero rows means the bookmark already exists. Usually
                        // that is a concurrent bookmark creation that committed
                        // first: the pessimistic lock above cannot serialize
                        // creations because there is no row to lock.
                        // Re-read and re-apply the already-applied check; only
                        // a batch that is still unapplied, or applied with a
                        // different changeset, is a genuine divergence.
                        let (txn_, fresh) = SelectBookmarkForUpdate::query_with_transaction(
                            txn,
                            &self.repo_id,
                            bookmark.name(),
                            bookmark.category(),
                        )
                        .await?;
                        txn = txn_;
                        check_lost_insert_replay(&fresh, moves)?;
                        continue;
                    }
                }
            }

            let owned = unapplied
                .iter()
                .map(|m| (m.log_id, m.old, Some(m.new), m.reason))
                .collect::<Vec<_>>();
            let data = owned
                .iter()
                .map(|(id, old, new, reason)| {
                    (
                        id,
                        &self.repo_id,
                        bookmark.name(),
                        bookmark.category(),
                        old,
                        new,
                        reason,
                        &timestamp,
                    )
                })
                .collect::<Vec<_>>();
            let (txn_, _) = AddBookmarkLog::query_with_transaction(txn, data.as_slice()).await?;
            txn = txn_;

            first_applied_id = first_applied_id.or(Some(first_unapplied.log_id));
            any_applied = true;
        }
        // AlreadyProcessed below rolls back the whole SQL transaction. That is
        // only safe when the transaction carries mirror batches and nothing
        // else, so a lost-ack replay cannot silently drop other bookmark ops.
        // The mirror path always builds its own transaction; assert the
        // invariant so a future change that mixes ops fails loudly in tests.
        debug_assert!(
            self.mirror_batches.is_empty()
                || (self.force_sets.is_empty()
                    && self.creates.is_empty()
                    && self.updates.is_empty()
                    && self.force_deletes.is_empty()
                    && self.deletes.is_empty()),
            "mirror batch transaction must not carry non-mirror bookmark ops"
        );
        if !any_applied && !self.mirror_batches.is_empty() {
            // Every batch was already applied by the replica, so signal the
            // caller to advance its checkpoint instead of retrying.
            return Err(BookmarkTransactionError::AlreadyProcessed);
        }
        Ok((txn, first_applied_id))
    }

    async fn store_updates<'op, 'log: 'op>(
        &'log self,
        _ctx: &CoreContext,
        mut txn: SqlTransaction,
        log: &'op mut TransactionLogUpdates<'log>,
    ) -> Result<SqlTransaction, BookmarkTransactionError> {
        for (bookmark, old_cs_id, new_cs_id, kinds, maybe_log_entry) in self.updates.iter() {
            let log_id = maybe_log_entry
                .as_ref()
                .map(|log_entry| log.push_log_entry(bookmark, log_entry))
                .transpose()?;

            if new_cs_id == old_cs_id && log_id.is_none() {
                // This is a no-op update.  Check if the bookmark already points to the correct
                // commit.  If it doesn't, abort the transaction. We need to make this a select
                // query instead of an update, since affected_rows() would otherwise return 0.
                let (txn_, result) = SelectBookmark::query_with_transaction(
                    txn,
                    &self.repo_id,
                    bookmark.name(),
                    bookmark.category(),
                )
                .await?;
                txn = txn_;
                if result.first().map(|row| row.0).as_ref() != Some(new_cs_id) {
                    return Err(BookmarkTransactionError::LogicError);
                }
            } else {
                let (txn_, result) = UpdateBookmark::query_with_transaction(
                    txn,
                    &self.repo_id,
                    &log_id,
                    bookmark.name(),
                    bookmark.category(),
                    old_cs_id,
                    new_cs_id,
                    kinds,
                )
                .await?;
                txn = txn_;
                if result.affected_rows() != 1 {
                    return Err(BookmarkTransactionError::LogicError);
                }
            }
        }
        Ok(txn)
    }

    async fn store_force_deletes<'op, 'log: 'op>(
        &'log self,
        _ctx: &CoreContext,
        mut txn: SqlTransaction,
        log: &'op mut TransactionLogUpdates<'log>,
    ) -> Result<SqlTransaction, BookmarkTransactionError> {
        for (bookmark, log_entry) in self.force_deletes.iter() {
            log.push_log_entry(bookmark, log_entry)?;
            let (txn_, _) = DeleteBookmark::query_with_transaction(
                txn,
                &self.repo_id,
                bookmark.name(),
                bookmark.category(),
            )
            .await?;
            txn = txn_;
        }
        Ok(txn)
    }

    async fn store_deletes<'op, 'log: 'op>(
        &'log self,
        _ctx: &CoreContext,
        mut txn: SqlTransaction,
        log: &'op mut TransactionLogUpdates<'log>,
    ) -> Result<SqlTransaction, BookmarkTransactionError> {
        for (bookmark, old_cs_id, maybe_log_entry) in self.deletes.iter() {
            maybe_log_entry
                .as_ref()
                .map(|log_entry| log.push_log_entry(bookmark, log_entry))
                .transpose()?;
            let (txn_, result) = DeleteBookmarkIf::query_with_transaction(
                txn,
                &self.repo_id,
                bookmark.name(),
                bookmark.category(),
                old_cs_id,
            )
            .await?;
            txn = txn_;
            if result.affected_rows() != 1 {
                return Err(BookmarkTransactionError::LogicError);
            }
        }
        Ok(txn)
    }

    // Scratch operations have no log entry; mirror batches reuse source IDs.
    fn logged_bookmark_names(&self) -> HashSet<BookmarkName> {
        self.force_sets
            .iter()
            .map(|(key, ..)| key.name().clone())
            .chain(
                self.creates
                    .iter()
                    .filter_map(|(key, _, _, entry)| entry.as_ref().map(|_| key.name().clone())),
            )
            .chain(
                self.updates
                    .iter()
                    .filter_map(|(key, _, _, _, entry)| entry.as_ref().map(|_| key.name().clone())),
            )
            .chain(self.force_deletes.iter().map(|(key, _)| key.name().clone()))
            .chain(
                self.deletes
                    .iter()
                    .filter_map(|(key, _, entry)| entry.as_ref().map(|_| key.name().clone())),
            )
            .collect()
    }

    /// Attempt to write a bookmark update log entry
    /// Returns the db transaction and the id of this entry in the bookmark update log.
    async fn attempt_write(
        &self,
        ctx: &CoreContext,
        txn: SqlTransaction,
    ) -> Result<(SqlTransaction, u64), BookmarkTransactionError> {
        let by_name = bookmark_log_ids_by_name(self.repo_id, BookmarkLogWriter::SingleRepo);
        let enabled = bookmark_log_guard_enabled(self.repo_id, BookmarkLogWriter::SingleRepo);
        let names = self.logged_bookmark_names();
        let (txn, next_lock_id) =
            if bookmark_log_guard_enabled_for_allocation(self.repo_id, by_name, enabled)? {
                let (txn, id) = find_next_bookmark_log_lock_id(txn, self.repo_id).await?;
                (txn, Some(id))
            } else {
                (txn, None)
            };
        let (mut txn, next_id) =
            find_next_bookmark_log_ids(txn, self.repo_id, &names, by_name).await?;
        let mut log = TransactionLogUpdates::new(next_id);

        txn = self.store_force_sets(ctx, txn, &mut log).await?;
        txn = self.store_creates(ctx, txn, &mut log).await?;
        txn = self.store_updates(ctx, txn, &mut log).await?;
        txn = self.store_force_deletes(ctx, txn, &mut log).await?;
        txn = self.store_deletes(ctx, txn, &mut log).await?;
        txn = self
            .store_log(ctx, txn, &log, next_lock_id)
            .await
            .map_err(BookmarkTransactionError::RetryableError)?;
        // modern_sync mirror batches insert their own log rows with source ids,
        // so they run after store_log rather than through it.
        let (txn, mirror_first_id) = self.store_mirror_batches(txn).await?;

        // Return the first log ID (used by callers for ensure_backsynced)
        let first_id = log
            .log_entries
            .first()
            .map(|(id, _, _)| *id)
            .or(mirror_first_id)
            .unwrap_or(0);
        Ok((txn, first_id))
    }
}

pub struct SqlBookmarksTransaction {
    write_connection: Connection,
    ctx: CoreContext,

    /// Bookmarks that have been seen already in this transaction.
    seen: HashSet<BookmarkKey>,

    /// Transaction updates.  A separate struct so that they can be
    /// moved into the future that will perform the database
    /// updates.
    payload: SqlBookmarksTransactionPayload,
}

impl SqlBookmarksTransaction {
    pub(crate) fn new(
        ctx: CoreContext,
        write_connection: Connection,
        repo_id: RepositoryId,
    ) -> Self {
        Self {
            write_connection,
            ctx,
            seen: HashSet::new(),
            payload: SqlBookmarksTransactionPayload::new(repo_id),
        }
    }

    pub fn check_not_seen(&mut self, bookmark: &BookmarkKey) -> Result<()> {
        if !self.seen.insert(bookmark.clone()) {
            return Err(anyhow!("{bookmark} bookmark was already used"));
        }
        Ok(())
    }
}

impl BookmarkTransaction for SqlBookmarksTransaction {
    fn update(
        &mut self,
        bookmark: &BookmarkKey,
        new_cs: ChangesetId,
        old_cs: ChangesetId,
        reason: BookmarkUpdateReason,
    ) -> Result<()> {
        self.check_not_seen(bookmark)?;
        let log = NewUpdateLogEntry::new(Some(old_cs), Some(new_cs), reason)?;
        self.payload.updates.push((
            bookmark.clone(),
            old_cs,
            new_cs,
            BookmarkKind::ALL_PUBLISHING,
            Some(log),
        ));
        Ok(())
    }

    fn mirror_batch(
        &mut self,
        bookmark: &BookmarkKey,
        kind: BookmarkKind,
        moves: Vec<MirrorBookmarkMove>,
    ) -> Result<()> {
        self.check_not_seen(bookmark)?;
        self.payload
            .mirror_batches
            .push((bookmark.clone(), kind, moves));
        Ok(())
    }

    fn update_scratch(
        &mut self,
        bookmark: &BookmarkKey,
        new_cs: ChangesetId,
        old_cs: ChangesetId,
    ) -> Result<()> {
        self.check_not_seen(bookmark)?;
        self.payload.updates.push((
            bookmark.clone(),
            old_cs,
            new_cs,
            &[BookmarkKind::Scratch],
            None, // Scratch bookmark updates are not logged.
        ));
        Ok(())
    }

    fn create(
        &mut self,
        bookmark: &BookmarkKey,
        new_cs: ChangesetId,
        reason: BookmarkUpdateReason,
    ) -> Result<()> {
        self.check_not_seen(bookmark)?;
        let log = NewUpdateLogEntry::new(None, Some(new_cs), reason)?;

        self.payload.creates.push((
            bookmark.clone(),
            new_cs,
            BookmarkKind::PullDefaultPublishing,
            Some(log),
        ));

        Ok(())
    }

    fn create_publishing(
        &mut self,
        bookmark: &BookmarkKey,
        new_cs: ChangesetId,
        reason: BookmarkUpdateReason,
    ) -> Result<()> {
        self.check_not_seen(bookmark)?;
        let log = NewUpdateLogEntry::new(None, Some(new_cs), reason)?;
        self.payload.creates.push((
            bookmark.clone(),
            new_cs,
            BookmarkKind::Publishing,
            Some(log),
        ));
        Ok(())
    }

    fn create_scratch(&mut self, bookmark: &BookmarkKey, new_cs: ChangesetId) -> Result<()> {
        self.check_not_seen(bookmark)?;
        self.payload.creates.push((
            bookmark.clone(),
            new_cs,
            BookmarkKind::Scratch,
            None, // Scratch bookmark updates are not logged.
        ));
        Ok(())
    }

    fn force_set(
        &mut self,
        bookmark: &BookmarkKey,
        new_cs: ChangesetId,
        reason: BookmarkUpdateReason,
    ) -> Result<()> {
        self.check_not_seen(bookmark)?;
        let log = NewUpdateLogEntry::new(None, Some(new_cs), reason)?;
        self.payload
            .force_sets
            .push((bookmark.clone(), new_cs, log));
        Ok(())
    }

    fn delete(
        &mut self,
        bookmark: &BookmarkKey,
        old_cs: ChangesetId,
        reason: BookmarkUpdateReason,
    ) -> Result<()> {
        self.check_not_seen(bookmark)?;
        let log = NewUpdateLogEntry::new(Some(old_cs), None, reason)?;
        self.payload
            .deletes
            .push((bookmark.clone(), old_cs, Some(log)));
        Ok(())
    }

    fn force_delete(&mut self, bookmark: &BookmarkKey, reason: BookmarkUpdateReason) -> Result<()> {
        self.check_not_seen(bookmark)?;
        let log = NewUpdateLogEntry::new(None, None, reason)?;
        self.payload.force_deletes.push((bookmark.clone(), log));
        Ok(())
    }

    fn delete_scratch(&mut self, bookmark: &BookmarkKey, old_cs: ChangesetId) -> Result<()> {
        self.check_not_seen(bookmark)?;
        self.payload.deletes.push((
            bookmark.clone(),
            old_cs,
            None, // Scratch bookmark updates are not logged.
        ));
        Ok(())
    }

    fn commit(self: Box<Self>) -> BoxFuture<'static, Result<Option<u64>>> {
        self.commit_with_hooks(vec![Arc::new(|_ctx, txn| future::ok(txn).boxed())])
    }

    fn commit_with_hooks(
        self: Box<Self>,
        txn_hooks: Vec<BookmarkTransactionHook>,
    ) -> BoxFuture<'static, Result<Option<u64>>> {
        let Self {
            ctx,
            payload,
            write_connection,
            ..
        } = *self;

        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlWrites);

        async move {
            let mut attempt = 0;
            let result: Result<(sql_ext::Transaction, u64), _> = loop {
                attempt += 1;

                let mut txn = write_connection
                    .start_transaction(ctx.sql_query_telemetry())
                    .await?;

                txn = match run_transaction_hooks(&ctx, txn, &txn_hooks).await {
                    Ok(txn) => txn,
                    Err(BookmarkTransactionError::RetryableError(_))
                        if attempt < MAX_BOOKMARK_TRANSACTION_ATTEMPT_COUNT =>
                    {
                        continue;
                    }
                    Err(err) => break Err(err),
                };

                match payload.attempt_write(&ctx, txn).await {
                    Err(BookmarkTransactionError::RetryableError(_))
                        if attempt < MAX_BOOKMARK_TRANSACTION_ATTEMPT_COUNT =>
                    {
                        continue;
                    }
                    result => break result,
                }
            };

            // The number of `RetryableError`'s that were encountered
            let mut retryable_errors = attempt as i64 - 1;
            let result = match result {
                Ok((txn, log_id)) => {
                    STATS::bookmarks_update_log_insert_success.add_value(1);
                    STATS::bookmarks_update_log_insert_success_attempt_count
                        .add_value(attempt as i64);
                    txn.commit().await?;
                    Ok(Some(log_id))
                }
                Err(BookmarkTransactionError::LogicError) => {
                    // Logic error signifies that the transaction was rolled
                    // back, which likely means that bookmark has moved since
                    // our pushrebase finished. We need to retry the pushrebase
                    // Attempt count means one more than the number of `RetryableError`
                    // we hit before seeing this.
                    STATS::bookmarks_insert_logic_error.add_value(1);
                    STATS::bookmarks_insert_logic_error_attempt_count.add_value(attempt as i64);
                    Ok(None)
                }
                Err(BookmarkTransactionError::AlreadyProcessed) => {
                    // A modern_sync mirror move was already applied on the
                    // replica (a lost-ack replay). The transaction is rolled
                    // back. Report a distinct error so higher layers map it to a
                    // response code that tells modern_sync to advance its
                    // checkpoint instead of retrying forever.
                    STATS::bookmarks_insert_already_processed.add_value(1);
                    Err(BookmarkMoveAlreadyProcessed.into())
                }
                Err(BookmarkTransactionError::RetryableError(err)) => {
                    // Attempt count for `RetryableError` should always be equal
                    // to the MAX_BOOKMARK_TRANSACTION_ATTEMPT_COUNT, and hitting
                    // this error here basically means that this number of attempts
                    // was not enough, or the error was misclassified
                    STATS::bookmarks_insert_retryable_error.add_value(1);
                    STATS::bookmarks_insert_retryable_error_attempt_count.add_value(attempt as i64);
                    retryable_errors += 1;
                    Err(err)
                }
                Err(BookmarkTransactionError::Other(err)) => {
                    // `Other` error captures what we consider an "infrastructure"
                    // error, e.g. xdb went down during this transaction.
                    // Attempt count > 1 means the before we hit this error,
                    // we hit `RetryableError` a attempt count - 1 times.
                    STATS::bookmarks_insert_other_error.add_value(1);
                    STATS::bookmarks_insert_other_error_attempt_count.add_value(attempt as i64);
                    Err(err)
                }
            };
            STATS::bookmarks_update_log_insert_retry.add_value(retryable_errors);
            result
        }
        .boxed()
    }
}

async fn run_transaction_hooks(
    ctx: &CoreContext,
    mut txn: sql_ext::Transaction,
    txn_hooks: &Vec<BookmarkTransactionHook>,
) -> Result<sql_ext::Transaction, BookmarkTransactionError> {
    for txn_hook in txn_hooks {
        txn = txn_hook(ctx.clone(), txn).await?;
    }
    Ok(txn)
}

#[cfg(test)]
pub(crate) async fn insert_bookmarks(
    ctx: &CoreContext,
    conn: &Connection,
    rows: impl IntoIterator<Item = (&RepositoryId, &BookmarkKey, &ChangesetId, &BookmarkKind)>,
) -> Result<()> {
    let none = None;
    let rows = rows
        .into_iter()
        .map(|(r, b, c, k)| (r, &none, b.name(), b.category(), c, k))
        .collect::<Vec<_>>();
    InsertBookmarks::query(conn, ctx.sql_query_telemetry(), rows.as_slice()).await?;
    Ok(())
}

#[cfg(test)]
mod test {
    use mononoke_macros::mononoke;
    use mononoke_types_mocks::changesetid::ONES_CSID;
    use mononoke_types_mocks::changesetid::THREES_CSID;
    use mononoke_types_mocks::changesetid::TWOS_CSID;

    use super::*;

    fn bookmark_creation_moves() -> Vec<MirrorBookmarkMove> {
        vec![
            MirrorBookmarkMove {
                log_id: 1,
                old: None,
                new: ONES_CSID,
                reason: BookmarkUpdateReason::TestMove,
            },
            MirrorBookmarkMove {
                log_id: 2,
                old: Some(ONES_CSID),
                new: TWOS_CSID,
                reason: BookmarkUpdateReason::TestMove,
            },
        ]
    }

    #[mononoke::test]
    fn lost_insert_replay_of_fully_applied_batch_is_ok() {
        // The winner applied the whole batch. The loser skips it as a replay.
        let fresh = vec![(TWOS_CSID, Some(2))];
        check_lost_insert_replay(&fresh, &bookmark_creation_moves())
            .expect("a fully-applied batch must be treated as a replay");
    }

    #[mononoke::test]
    fn lost_insert_replay_past_batch_is_ok() {
        // The winner applied this batch and moved on. The loser's changeset
        // is expected to differ, so it must not count as divergence.
        let fresh = vec![(THREES_CSID, Some(3))];
        check_lost_insert_replay(&fresh, &bookmark_creation_moves())
            .expect("a batch applied plus later moves must be treated as a replay");
    }

    #[mononoke::test]
    fn lost_insert_replay_of_partial_prefix_fails() {
        // The winner applied only the first move (a retry regrouped the
        // batch). The rest is still unapplied, so this is not a replay.
        let fresh = vec![(ONES_CSID, Some(1))];
        assert!(matches!(
            check_lost_insert_replay(&fresh, &bookmark_creation_moves()),
            Err(BookmarkTransactionError::LogicError)
        ));
    }

    #[mononoke::test]
    fn lost_insert_replay_with_diverged_changeset_fails() {
        // The replica sits at this batch's last log id but a different
        // changeset: genuine divergence.
        let fresh = vec![(THREES_CSID, Some(2))];
        assert!(matches!(
            check_lost_insert_replay(&fresh, &bookmark_creation_moves()),
            Err(BookmarkTransactionError::LogicError)
        ));
    }

    #[mononoke::test]
    fn lost_insert_replay_with_missing_row_fails() {
        // The row vanished between the INSERT and the re-read. Nothing is
        // applied, so this cannot be a replay.
        let fresh: Vec<(ChangesetId, Option<u64>)> = vec![];
        assert!(matches!(
            check_lost_insert_replay(&fresh, &bookmark_creation_moves()),
            Err(BookmarkTransactionError::LogicError)
        ));
    }
}
