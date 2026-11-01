/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use context::CoreContext;
use context::PerfCounterType;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use futures_retry::retry;
use itertools::Itertools;
use mercurial_types::HgChangesetId;
use mononoke_types::RepositoryId;
use sql_ext::Connection;
use sql_ext::SqlConnections;
use sql_ext::mononoke_queries;
use tracing::debug;

use crate::HgMutationStore;
use crate::entry::HgMutationEntry;

/// To avoid overloading the database with too many changesets in a single
/// select, we chunk selects of chains to this size.
const SELECT_CHAIN_CHUNK_SIZE: usize = 10;

pub struct SqlHgMutationStore {
    repo_id: RepositoryId,
    connections: SqlConnections,
    mutation_chain_limit: usize,
}

/// Convenience alias for the type of each row returned from the entries queries.
type EntryRow = (
    HgChangesetId, // Successor
    u64,           // Predecessor Count
    u64,           // Predecessor Index
    HgChangesetId, // Predecessor
    u64,           // Split Count
    String,        // Op
    String,        // User
    i64,           // Timestamp
    i32,           // Time Zone
    String,        // Extras
);

impl SqlHgMutationStore {
    pub fn new(
        repo_id: RepositoryId,
        connections: SqlConnections,
        mutation_chain_limit: usize,
    ) -> Self {
        Self {
            repo_id,
            connections,
            mutation_chain_limit,
        }
    }

    /// Store new mutation entries in the mutation store.
    ///
    /// Stores the mutation entries in the store, and records that
    /// entries for the changesets in `changeset_ids` have been stored.
    ///
    /// Initial commits are not expected to have entries, but we still
    /// need to record that they have been stored.
    async fn store(
        &self,
        ctx: &CoreContext,
        new_changeset_ids: HashSet<HgChangesetId>,
        entries: Vec<HgMutationEntry>,
    ) -> Result<()> {
        let mut db_csets = Vec::new();
        let mut db_entries = Vec::new();
        let mut db_preds = Vec::new();
        let mut db_splits = Vec::new();
        for changeset_id in new_changeset_ids.iter() {
            db_csets.push((&self.repo_id, changeset_id));
        }
        for entry in entries.iter() {
            db_entries.push((
                &self.repo_id,
                entry.successor(),
                entry.predecessors().len() as u64,
                entry.split().len() as u64,
                entry.op(),
                entry.user(),
                entry.timestamp(),
                entry.timezone(),
                entry.extra_json()?,
            ));
            for (index, predecessor) in entry.predecessors().enumerate() {
                db_preds.push((&self.repo_id, entry.successor(), index as u64, predecessor));
            }
            for (index, split_successor) in entry.split().enumerate() {
                db_splits.push((
                    &self.repo_id,
                    entry.successor(),
                    index as u64,
                    split_successor,
                ));
            }
        }

        // Sort the entries so that they are inserted in a stable
        // order.  This prevents deadlocks where one transaction is
        // adding commits "A, B, C" and gains the lock on A while
        // another transaction is adding "C, D, A" and gains the lock
        // on C.
        db_csets.sort_unstable();
        db_entries.sort_unstable();
        db_preds.sort_unstable();
        db_splits.sort_unstable();

        // Convert the numbers to references.
        let ref_db_entries: Vec<_> = db_entries
            .iter()
            .map(
                |&(
                    repo_id,
                    successor,
                    ref extra_pred_count,
                    ref split_count,
                    op,
                    user,
                    ref time,
                    ref tz,
                    ref extra,
                )| {
                    (
                        repo_id,
                        successor,
                        extra_pred_count,
                        split_count,
                        op,
                        user,
                        time,
                        tz,
                        extra.as_str(),
                    )
                },
            )
            .collect();
        let ref_db_preds: Vec<_> = db_preds
            .iter()
            .map(|&(repo_id, successor, ref index, predecessor)| {
                (repo_id, successor, index, predecessor)
            })
            .collect();
        let ref_db_splits: Vec<_> = db_splits
            .iter()
            .map(|&(repo_id, successor, ref index, split_successor)| {
                (repo_id, successor, index, split_successor)
            })
            .collect();

        ctx.perf_counters()
            .add_to_counter(PerfCounterType::SqlWrites, 4);

        let ((), count) = retry(
            |_| async {
                let txn = self
                    .connections
                    .write_connection
                    .start_transaction(ctx.sql_query_telemetry())
                    .await?;

                let (txn, _) =
                    AddChangesets::query_with_transaction(txn, db_csets.as_slice()).await?;
                let (txn, _) =
                    AddEntries::query_with_transaction(txn, ref_db_entries.as_slice()).await?;
                let (txn, _) =
                    AddPreds::query_with_transaction(txn, ref_db_preds.as_slice()).await?;
                let (txn, _) =
                    AddSplits::query_with_transaction(txn, ref_db_splits.as_slice()).await?;
                txn.commit().await?;

                anyhow::Ok(())
            },
            Duration::from_secs(3),
        )
        .exponential_backoff(1.2)
        .jitter(Duration::from_secs(1))
        .max_attempts(3)
        .await?;

        debug!(
            "Mutation store added {} entries for {} changesets after {count} attempts",
            db_entries.len(),
            db_csets.len()
        );
        ctx.perf_counters().add_to_counter(
            PerfCounterType::HgMutationStoreNumAdded,
            db_entries.len() as i64,
        );

        Ok(())
    }

    /// Get an appropriate read connection for querying mutation information
    /// about some changesets.
    ///
    /// Checks if mutation information for all of the given changesets have been
    /// replicated to the read-only replica.  If so, returns a read connection
    /// to the replica.  If not all mutation information has been replicated, or
    /// if no mutation information for the changesets exists, returns a read
    /// connection to the master.
    ///
    /// Note that this only guarantees that mutation information for this
    /// commit and all of its predecessors are present.  For checking the
    /// successors of a commit, the replica may still lag behind.
    async fn read_connection_for_changesets(
        &self,
        ctx: &CoreContext,
        changeset_ids: &HashSet<HgChangesetId>,
    ) -> Result<(&Connection, PerfCounterType)> {
        // Check if the replica is up-to-date with respect to all changesets we
        // are interested in.
        let changeset_ids: Vec<_> = changeset_ids.iter().collect();
        if changeset_ids.is_empty() {
            // There are no interesting changesets, so just use the replica.
            return Ok((
                &self.connections.read_connection,
                PerfCounterType::SqlReadsReplica,
            ));
        }

        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlReadsReplica);
        let count = CountChangesets::query(
            &self.connections.read_connection,
            ctx.sql_query_telemetry(),
            &self.repo_id,
            changeset_ids.as_slice(),
        )
        .await?;

        if let Some((count,)) = count.into_iter().next() {
            if count as usize == changeset_ids.len() {
                // The replica knows all of the changesets, so use it.
                return Ok((
                    &self.connections.read_connection,
                    PerfCounterType::SqlReadsReplica,
                ));
            }
        }
        // The replica doesn't know all of the changesets, use the connection to
        // the master.
        Ok((
            &self.connections.read_master_connection,
            PerfCounterType::SqlReadsMaster,
        ))
    }

    /// Collect entries from the database into an entry set.
    async fn collect_entries<I>(
        &self,
        ctx: &CoreContext,
        connection: &Connection,
        sql_perf_counter: PerfCounterType,
        rows: I,
    ) -> Result<HashMap<HgChangesetId, HgMutationEntry>>
    where
        I: IntoIterator<Item = EntryRow>,
    {
        let mut entries = HashMap::new();
        let mut to_fetch_split = Vec::new();
        for (
            successor,
            pred_count,
            p_seq,
            p_predecessor,
            split_count,
            op,
            user,
            timestamp,
            tz,
            extra,
        ) in rows
        {
            if p_seq >= pred_count {
                continue;
            }
            let entry = match entries.entry(successor) {
                hash_map::Entry::Occupied(entry) => entry.into_mut(),
                hash_map::Entry::Vacant(entry) => {
                    let successor = entry.key().clone();
                    if split_count > 0 {
                        to_fetch_split.push(successor.clone());
                    }
                    entry.insert(HgMutationEntry::new(
                        successor,
                        Vec::with_capacity(pred_count as usize),
                        Vec::with_capacity(split_count as usize),
                        op,
                        user,
                        timestamp,
                        tz,
                        serde_json::from_str(&extra)?,
                    ))
                }
            };
            entry
                .add_predecessor(p_seq, p_predecessor)
                .with_context(|| {
                    format!("Failed to collect predecessor {p_seq} for {successor}")
                })?;
        }
        if !to_fetch_split.is_empty() {
            ctx.perf_counters().increment_counter(sql_perf_counter);
            let rows = SelectSplitsBySuccessor::query(
                connection,
                ctx.sql_query_telemetry(),
                &self.repo_id,
                to_fetch_split.as_slice(),
            )
            .await?;
            for (successor, seq, split_successor) in rows {
                if let Some(entry) = entries.get_mut(&successor) {
                    entry.add_split(seq, split_successor).with_context(|| {
                        format!("Failed to collect split successor {seq} for {successor}")
                    })?;
                }
            }
        }
        Ok(entries)
    }

    /// Fetch all predecessor entries for the entries in the entry set.
    async fn fetch_all_predecessors(
        &self,
        ctx: &CoreContext,
        connection: &Connection,
        sql_perf_counter: PerfCounterType,
        changesets: &HashSet<HgChangesetId>,
    ) -> Result<HashMap<HgChangesetId, HgMutationEntry>> {
        let chunks = changesets
            .iter()
            .chunks(SELECT_CHAIN_CHUNK_SIZE)
            .into_iter()
            .map(|chunk| chunk.copied().collect::<Vec<_>>())
            .collect::<Vec<_>>();

        let rows = stream::iter(chunks.into_iter().map(|changesets| async move {
            ctx.perf_counters().increment_counter(sql_perf_counter);
            SelectBySuccessorChain::query(
                connection,
                ctx.sql_query_telemetry(),
                &self.repo_id,
                &self.mutation_chain_limit,
                &changesets,
            )
            .await
            .with_context(|| format!("Error fetching mutation chains for: {changesets:?}"))
        }))
        .buffered(10)
        .try_collect::<Vec<_>>()
        .await?;

        let entries = self
            .collect_entries(
                ctx,
                connection,
                sql_perf_counter,
                rows.into_iter().flatten(),
            )
            .await?;

        Ok(entries)
    }
}

#[async_trait]
impl HgMutationStore for SqlHgMutationStore {
    fn repo_id(&self) -> RepositoryId {
        self.repo_id
    }

    async fn add_entries(
        &self,
        ctx: &CoreContext,
        new_changeset_ids: HashSet<HgChangesetId>,
        mut entries: Vec<HgMutationEntry>,
    ) -> Result<()> {
        if new_changeset_ids.is_empty() {
            // Nothing to do.
            return Ok(());
        }

        entries.retain(|entry| {
            // Filter out entries that are self-referential (these are
            // revive obsmarkers incorrectly converted to mutation entries).
            let successor = entry.successor();
            let self_referential = entry
                .predecessors()
                .any(|predecessor| predecessor == successor);
            !self_referential
        });

        // Store the new entries.
        self.store(ctx, new_changeset_ids, entries).await?;
        Ok(())
    }

    /// Get all predecessor information for the given changeset id, keyed by
    /// the successor changeset id.
    ///
    /// Returns all entries that describe the mutation history of the commits.
    /// keyed by the successor changeset ids.
    async fn all_predecessors_by_changeset(
        &self,
        ctx: &CoreContext,
        changeset_ids: HashSet<HgChangesetId>,
    ) -> Result<HashMap<HgChangesetId, Vec<HgMutationEntry>>> {
        if changeset_ids.is_empty() {
            // Nothing to fetch
            return Ok(HashMap::new());
        }

        let (connection, sql_perf_counter) = self
            .read_connection_for_changesets(ctx, &changeset_ids)
            .await?;
        let entries = self
            .fetch_all_predecessors(ctx, connection, sql_perf_counter, &changeset_ids)
            .await?;
        let changeset_count = changeset_ids.len();
        debug!(
            "Mutation store fetched {} entries for {} changesets",
            entries.len(),
            changeset_count,
        );
        ctx.perf_counters().add_to_counter(
            PerfCounterType::HgMutationStoreNumFetched,
            entries.len() as i64,
        );
        Ok(group_entries_by_successor(changeset_ids, entries))
    }
}

/// Group entries into groups of predecessors of the given changeset ids.
pub(crate) fn group_entries_by_successor(
    successor_ids: HashSet<HgChangesetId>,
    all_entries: HashMap<HgChangesetId, HgMutationEntry>,
) -> HashMap<HgChangesetId, Vec<HgMutationEntry>> {
    let mut mutation_history = HashMap::new();
    for successor_id in successor_ids {
        let mut entries = Vec::new();
        let mut processed = HashSet::new();
        let mut changeset_ids = vec![successor_id];
        while let Some(changeset_id) = changeset_ids.pop() {
            // See if we have an entry for this changeset_id.
            if let Some(entry) = all_entries.get(&changeset_id).cloned() {
                // Add all of this entry's predecessors to the queue of
                // additional changesets we will need to process.  Push
                // predecessors in reverse order so that we process them in
                // forwards order.
                for predecessor_id in entry.predecessors().rev() {
                    // Only enqueue the predecessor if:
                    // 1. There is an entry for it
                    if all_entries.contains_key(predecessor_id)
                        // 2. AND we haven't already processed this predecessor
                        && !processed.contains(predecessor_id)
                        // 3. AND its not the same as the successor
                        && successor_id != *predecessor_id
                    {
                        changeset_ids.push(predecessor_id.clone());
                    }
                }
                entries.push(entry);
            }
            processed.insert(changeset_id);
        }
        mutation_history.insert(successor_id, entries);
    }
    mutation_history
}

mononoke_queries! {
    write AddChangesets(
        values: (
            repo_id: RepositoryId,
            changeset_id: HgChangesetId,
        )
    ) {
        insert_or_ignore,
        "{insert_or_ignore} INTO hg_mutation_changesets (
            repo_id,
            changeset_id
        ) VALUES {values}"
    }

    write AddEntries(
        values: (
            repo_id: RepositoryId,
            successor: HgChangesetId,
            pred_count: u64,
            split_count: u64,
            op: str,
            user: str,
            timestamp: i64,
            tz: i32,
            extra: str,
        )
    ) {
        insert_or_ignore,
        "{insert_or_ignore} INTO hg_mutation_info (
            repo_id,
            successor,
            pred_count,
            split_count,
            op,
            user,
            timestamp,
            tz,
            extra
        ) VALUES {values}"
    }

    write AddPreds(
        values: (
            repo_id: RepositoryId,
            successor: HgChangesetId,
            seq: u64,
            predecessor: HgChangesetId,
        )
    ) {
        insert_or_ignore,
        "{insert_or_ignore} INTO hg_mutation_preds (
            repo_id,
            successor,
            seq,
            predecessor
        ) VALUES {values}"
    }

    write AddSplits(
        values: (
            repo_id: RepositoryId,
            successor: HgChangesetId,
            seq: u64,
            split_successor: HgChangesetId,
        )
    ) {
        insert_or_ignore,
        "{insert_or_ignore} INTO hg_mutation_splits (
            repo_id,
            successor,
            seq,
            split_successor
        ) VALUES {values}"
    }

    read CountChangesets(repo_id: RepositoryId, >list cs_id: &HgChangesetId) -> (i64) {
        "SELECT
            COUNT(*)
        FROM
            hg_mutation_changesets
        WHERE repo_id = {repo_id} AND changeset_id IN {cs_id}"
    }

    read SelectBySuccessor(repo_id: RepositoryId, >list cs_id: HgChangesetId) -> (
        HgChangesetId,
        u64,
        u64,
        HgChangesetId,
        u64,
        String,
        String,
        i64,
        i32,
        String
    ) {
        "SELECT
            m.successor,
            m.pred_count, p.seq, p.predecessor,
            m.split_count,
            m.op, m.user, m.timestamp, m.tz, m.extra
        FROM
            hg_mutation_info m JOIN hg_mutation_preds p
            ON m.repo_id = p.repo_id AND m.successor = p.successor
        WHERE m.repo_id = {repo_id} AND m.successor IN {cs_id}
        ORDER BY m.successor, p.seq ASC"
    }

    read SelectBySuccessorChain(repo_id: RepositoryId, mut_lim: usize, >list cs_id: HgChangesetId) -> (
        HgChangesetId,
        u64,
        u64,
        HgChangesetId,
        u64,
        String,
        String,
        i64,
        i32,
        String
    ) {
        "WITH RECURSIVE mp AS (
            SELECT
                m.successor,
                m.pred_count, p.seq, p.predecessor,
                m.split_count,
                m.op, m.user, m.timestamp, m.tz, m.extra,
                1 AS step
            FROM
                hg_mutation_info m JOIN hg_mutation_preds p
                ON m.repo_id = p.repo_id AND m.successor = p.successor
            WHERE m.repo_id = {repo_id} AND m.successor IN {cs_id}
            UNION ALL
            SELECT
                m.successor,
                m.pred_count, p.seq, p.predecessor,
                m.split_count,
                m.op, m.user, m.timestamp, m.tz, m.extra,
                mp.step + 1
            FROM
                mp INNER JOIN hg_mutation_info m
                ON m.successor = mp.predecessor
                JOIN hg_mutation_preds p
                ON m.repo_id = p.repo_id AND m.successor = p.successor
            WHERE m.repo_id = {repo_id} AND mp.step < {mut_lim}
        )
        SELECT
            mp.successor,
            mp.pred_count, mp.seq, mp.predecessor,
            mp.split_count,
            mp.op, mp.user, mp.timestamp, mp.tz, mp.extra
        FROM mp
        ORDER BY mp.successor, mp.seq ASC"
    }

    read SelectSplitsBySuccessor(repo_id: RepositoryId, >list cs_id: HgChangesetId) -> (
        HgChangesetId,
        u64,
        HgChangesetId,
    ) {
        "SELECT successor, seq, split_successor
        FROM hg_mutation_splits
        WHERE repo_id = {repo_id} AND successor IN {cs_id}
        ORDER BY successor, seq ASC"
    }
}
