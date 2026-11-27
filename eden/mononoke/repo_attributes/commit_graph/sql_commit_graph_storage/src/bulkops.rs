/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::collections::btree_map::Entry as BTreeMapEntry;
use std::ops::Range;
use std::ops::RangeInclusive;
use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::ensure;
use async_trait::async_trait;
use commit_graph_types::edges::ChangesetEdges;
use commit_graph_types::edges::EdgeType;
use commit_graph_types::edges::Parents;
use commit_graph_types::storage::CommitGraphStorage;
use commit_graph_types::storage::FetchedChangesetEdges;
use commit_graph_types::storage::Prefetch;
use context::CoreContext;
use futures::Stream;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use mononoke_types::ChangesetId;
use mononoke_types::RepositoryId;

use crate::SqlCommitGraphStorage;

#[facet::facet]
pub struct CommitGraphBulkFetcher {
    storage: Arc<SqlCommitGraphStorage>,
}

/// A stream item returned by the bulk fetcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BulkFetcherStreamItem {
    /// The changeset id of the item.
    pub cs_id: ChangesetId,
    /// The auto-increment id of the item in the commit graph sql table.
    pub sql_id: u64,
}

/// The changesets of a range of auto-increment ids: how many there are, and
/// the lowest and highest of their ids, or `None` when there are none.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IdsInRange {
    pub count: u64,
    pub ids: Option<RangeInclusive<u64>>,
}

/// The direction of the bulk fetcher stream.
#[derive(Clone, Copy)]
enum Direction {
    /// Iterate over changesets in increasing order of their auto-increment ids.
    OldestFirst,
    /// Iterate over changesets in decreasing order of their auto-increment ids.
    NewestFirst,
}

impl CommitGraphBulkFetcher {
    pub fn new(storage: Arc<SqlCommitGraphStorage>) -> Self {
        Self { storage }
    }

    /// Returns the bounds of the auto-increment ids of changesets in the repo.
    /// The bounds are returned as a half open interval [lo, hi).
    pub async fn repo_bounds(
        &self,
        ctx: &CoreContext,
        read_from_master: bool,
    ) -> Result<Range<u64>> {
        self.storage
            .repo_bounds(ctx, read_from_master)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Can't get repo bounds for an empty repo"))
    }

    /// Returns how many changesets in the repo have auto-increment ids in the
    /// half open interval [range.start, range.end), and the lowest and highest
    /// of those ids.
    pub async fn ids_in_range(
        &self,
        ctx: &CoreContext,
        range: Range<u64>,
        read_from_master: bool,
    ) -> Result<IdsInRange> {
        self.storage
            .ids_in_range(ctx, range, read_from_master)
            .await
    }

    /// Returns a stream of all changeset ids in the repo that have auto-increment ids
    /// in the half open interval [lower_bound, upper_bound).`sql_chunk_size` specifies the
    /// number of changesets to fetch from sql in a single query.
    ///
    /// The stream is in increasing order of the auto-increment ids of the changesets.
    pub fn oldest_first_stream(
        &self,
        ctx: &CoreContext,
        range: Range<u64>,
        sql_chunk_size: u64,
        read_from_master: bool,
    ) -> impl Stream<Item = Result<BulkFetcherStreamItem>> + use<> {
        self.stream_impl(
            ctx,
            range,
            sql_chunk_size,
            read_from_master,
            Direction::OldestFirst,
        )
    }

    /// Returns a stream of all changeset ids in the repo that have auto-increment ids
    /// in the half open interval [lower_bound, upper_bound). `sql_chunk_size` specifies the
    /// number of changesets to fetch from sql in a single query.
    ///
    /// The stream is in decreasing order of the auto-increment ids of the changesets.
    pub fn newest_first_stream(
        &self,
        ctx: &CoreContext,
        range: Range<u64>,
        sql_chunk_size: u64,
        read_from_master: bool,
    ) -> impl Stream<Item = Result<BulkFetcherStreamItem>> + use<> {
        self.stream_impl(
            ctx,
            range,
            sql_chunk_size,
            read_from_master,
            Direction::NewestFirst,
        )
    }

    fn stream_impl(
        &self,
        ctx: &CoreContext,
        range: Range<u64>,
        sql_chunk_size: u64,
        read_from_master: bool,
        direction: Direction,
    ) -> impl Stream<Item = Result<BulkFetcherStreamItem>> + use<> {
        struct StreamState {
            storage: Arc<SqlCommitGraphStorage>,
            ctx: CoreContext,
            range: Range<u64>,
        }

        stream::try_unfold(
            StreamState {
                storage: self.storage.clone(),
                ctx: ctx.clone(),
                range,
            },
            move |state| async move {
                let StreamState {
                    storage,
                    ctx,
                    range,
                } = state;

                let (chunk_stream, new_lower_bound, new_upper_bound) = match direction {
                    Direction::OldestFirst => {
                        let chunk = storage
                            .fetch_oldest_changesets_in_range(
                                &ctx,
                                range.clone(),
                                sql_chunk_size,
                                read_from_master,
                            )
                            .await?;

                        if chunk.is_empty() {
                            return Ok(None);
                        }

                        let new_lower_bound = chunk
                            .iter()
                            .map(|(_cs_id, sql_id)| *sql_id + 1)
                            .max()
                            .unwrap_or(range.start);

                        let chunk_stream = stream::iter(chunk)
                            .map(|(cs_id, sql_id)| Ok(BulkFetcherStreamItem { cs_id, sql_id }));

                        (chunk_stream.left_stream(), new_lower_bound, range.end)
                    }
                    Direction::NewestFirst => {
                        let chunk = storage
                            .fetch_newest_changesets_in_range(
                                &ctx,
                                range.clone(),
                                sql_chunk_size,
                                read_from_master,
                            )
                            .await?;

                        if chunk.is_empty() {
                            return Ok(None);
                        }

                        let new_upper_bound = chunk
                            .iter()
                            .map(|(_cs_id, sql_id)| *sql_id)
                            .min()
                            .unwrap_or(range.end);

                        let chunk_stream = stream::iter(chunk)
                            .map(|(cs_id, sql_id)| Ok(BulkFetcherStreamItem { cs_id, sql_id }));

                        (chunk_stream.right_stream(), range.start, new_upper_bound)
                    }
                };

                anyhow::Ok(Some((
                    chunk_stream,
                    StreamState {
                        storage,
                        ctx,
                        range: new_lower_bound..new_upper_bound,
                    },
                )))
            },
        )
        .try_flatten()
    }

    pub fn stream_topological<E: EdgeType>(
        &self,
        ctx: &CoreContext,
        range: RangeInclusive<u64>,
        sql_chunk_size: u64,
        read_from_master: bool,
    ) -> Result<impl Stream<Item = Result<ChangesetEdges>> + use<E>> {
        struct TopologicalState {
            storage: Arc<SqlCommitGraphStorage>,
            ctx: CoreContext,
            range: RangeInclusive<u64>,
            sql_chunk_size: u64,
            read_from_master: bool,
            /// Next position to fetch from (None if we've reached the end)
            next_fetch_pos: Option<u64>,
            /// High water mark - commits below this are considered processed
            high_water_mark: u64,
            /// Commits above the high water mark that have been emitted already
            emitted: BTreeSet<u64>,
            /// Commits that are waiting to be emitted, and what parents they are waiting for
            waiting_commits: BTreeMap<u64, (ChangesetId, ChangesetEdges, HashSet<u64>)>,
            /// A map from parent that hasn't been encountered yet to the children that are waiting for it
            waiting_children: HashMap<u64, Vec<u64>>,
            /// The changeset ids from the previous chunk so we don't need to refetch them
            previous_chunk_sql_ids: HashMap<ChangesetId, u64>,
        }

        impl TopologicalState {
            /// Recursively emit all descendants that are now ready.
            fn emit_descendants(&mut self, sql_id: u64, to_emit: &mut Vec<Result<ChangesetEdges>>) {
                let mut stack = Vec::new();
                stack.push(sql_id);

                // Continue processing all descendants that are now emissable.
                while let Some(emitted_sql_id) = stack.pop() {
                    if let Some(waiting_children) = self.waiting_children.remove(&emitted_sql_id) {
                        for child_sql_id in waiting_children {
                            if let BTreeMapEntry::Occupied(mut entry) =
                                self.waiting_commits.entry(child_sql_id)
                            {
                                let (_, _, pending_parents) = entry.get_mut();
                                pending_parents.remove(&emitted_sql_id);
                                if pending_parents.is_empty() {
                                    // We can now process this child.
                                    let (child_sql_id, (_child_cs_id, child_edges, _)) =
                                        entry.remove_entry();
                                    to_emit.push(Ok(child_edges));
                                    self.emitted.insert(child_sql_id);
                                    stack.push(child_sql_id);
                                }
                            }
                        }
                    }
                }
            }
        }

        let initial_state = TopologicalState {
            storage: self.storage.clone(),
            ctx: ctx.clone(),
            range: range.clone(),
            sql_chunk_size,
            read_from_master,
            next_fetch_pos: Some(*range.start()),
            high_water_mark: *range.start(),
            emitted: BTreeSet::new(),
            waiting_commits: BTreeMap::new(),
            waiting_children: HashMap::new(),
            previous_chunk_sql_ids: HashMap::new(),
        };

        Ok(stream::try_unfold(initial_state, |mut state| async move {
            let Some(start_id) = state.next_fetch_pos else {
                ensure!(
                    state.waiting_commits.is_empty(),
                    "Stream completed with {} commits waiting",
                    state.waiting_commits.len()
                );
                return anyhow::Ok(None);
            };

            let chunk = state
                .storage
                .fetch_many_edges_in_id_range(
                    &state.ctx,
                    start_id,
                    *state.range.end(),
                    state.sql_chunk_size,
                    state.read_from_master,
                )
                .await?;

            // Convert and sort the chunk by sql_id
            let mut chunk: Vec<(u64, ChangesetId, ChangesetEdges)> = chunk
                .into_iter()
                .map(|(cs_id, (sql_id, edges))| (sql_id, cs_id, edges))
                .collect();
            chunk.sort_by_key(|(sql_id, _, _)| *sql_id);

            // Update next fetch position
            state.next_fetch_pos = chunk.last().map(|(max_id, _, _)| max_id + 1);

            let chunk_sql_ids = chunk
                .iter()
                .map(|(sql_id, cs_id, _)| (*cs_id, *sql_id))
                .collect::<HashMap<_, _>>();

            // Process commits until we can emit some or run out of commits to process
            let mut to_process = VecDeque::from(chunk);
            let mut to_emit = Vec::new();

            // Collect all missing parents from all changesets in the chunk
            let all_missing_parents: Vec<ChangesetId> = to_process
                .iter()
                .flat_map(|(_, _, edges)| {
                    edges.parents::<E>().map(|node| node.cs_id).filter(|cs_id| {
                        !chunk_sql_ids.contains_key(cs_id)
                            && !state.previous_chunk_sql_ids.contains_key(cs_id)
                    })
                })
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();

            // Fetch all missing parent SQL IDs in a single call
            let missing_parent_sql_ids = if all_missing_parents.is_empty() {
                Default::default()
            } else {
                state
                    .storage
                    .fetch_many_ids(&state.ctx, &all_missing_parents, state.read_from_master)
                    .await?
            };

            while let Some((sql_id, cs_id, edges)) = to_process.pop_front() {
                let parents: Vec<ChangesetId> =
                    edges.parents::<E>().map(|node| node.cs_id).collect();
                let parent_sql_ids: Vec<u64> = parents
                    .iter()
                    .map(|p_cs_id| {
                        missing_parent_sql_ids
                            .get(p_cs_id)
                            .or_else(|| chunk_sql_ids.get(p_cs_id))
                            .or_else(|| state.previous_chunk_sql_ids.get(p_cs_id))
                            .copied()
                            .ok_or_else(|| anyhow!("could not find parent {p_cs_id} of {cs_id}"))
                    })
                    .collect::<Result<_>>()?;

                // Work out which parents have not been emitted yet.
                let pending_parent_sql_ids: Vec<u64> = parent_sql_ids
                    .iter()
                    .filter(|p_sql_id| {
                        **p_sql_id >= state.high_water_mark && !state.emitted.contains(*p_sql_id)
                    })
                    .copied()
                    .collect();
                if pending_parent_sql_ids.is_empty() {
                    // All parents have been emitted.  Emit this commit.
                    to_emit.push(Ok(edges));
                    state.emitted.insert(sql_id);

                    state.emit_descendants(sql_id, &mut to_emit);
                } else {
                    // Queue this commit until its parents are emitted
                    for p in pending_parent_sql_ids.iter() {
                        state.waiting_children.entry(*p).or_default().push(sql_id);
                    }
                    state.waiting_commits.insert(
                        sql_id,
                        (cs_id, edges, pending_parent_sql_ids.into_iter().collect()),
                    );
                }
            }

            if state.next_fetch_pos.is_none() {
                // We have reached the end of the stream.  Also emit any commits that have parents we haven't seen yet.
                let sql_ids: Vec<u64> = state
                    .waiting_children
                    .keys()
                    .copied()
                    .filter(|p| !state.waiting_commits.contains_key(p))
                    .collect();
                for sql_id in sql_ids {
                    state.emit_descendants(sql_id, &mut to_emit);
                }
            }

            // Update the high-water mark to the minimum of the next fetch pos or the lowest waiting commit.
            state.high_water_mark = match state.waiting_commits.keys().next() {
                Some(&lowest_waiting_sql_id) => state
                    .next_fetch_pos
                    .map_or(lowest_waiting_sql_id, |next_fetch| {
                        std::cmp::min(next_fetch, lowest_waiting_sql_id)
                    }),
                None => state.next_fetch_pos.unwrap_or(*state.range.end()),
            };
            state.emitted = state.emitted.split_off(&state.high_water_mark);
            state.previous_chunk_sql_ids = chunk_sql_ids;

            let emit_csids = to_emit
                .iter()
                .map(|e| e.as_ref().unwrap().node().cs_id)
                .collect::<Vec<_>>();

            Ok(Some((stream::iter(to_emit), state)))
        })
        .try_flatten())
    }

    pub async fn fetch_commit_count(&self, ctx: &CoreContext, id: RepositoryId) -> Result<u64> {
        self.storage.fetch_commit_count(ctx, id).await
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use commit_graph_testlib::shuffling_storage::ShufflingCommitGraphStorage;
    use commit_graph_testlib::utils::from_dag;
    use commit_graph_testlib::utils::from_dag_batch;
    use commit_graph_testlib::utils::name_cs_id;
    use commit_graph_testlib::utils::test_repo_identity;
    use context::CoreContext;
    use fbinit::FacebookInit;
    use mononoke_macros::mononoke;
    use mononoke_types::ChangesetId;
    use mononoke_types::RepositoryId;
    use rendezvous::RendezVousOptions;
    use sql_construct::SqlConstruct;

    use super::*;
    use crate::SqlCommitGraphStorageBuilder;

    async fn assert_stream(
        stream: impl Stream<Item = Result<BulkFetcherStreamItem>>,
        expected: Vec<(&str, u64)>,
    ) -> Result<()> {
        let actual = stream
            .map_ok(|item| (item.cs_id, item.sql_id))
            .try_collect::<Vec<_>>()
            .await?;

        let expected = expected
            .into_iter()
            .map(|(name, sql_id)| (name_cs_id(name), sql_id))
            .collect::<Vec<_>>();

        assert_eq!(actual, expected);

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_commit_graph_bulk_fetcher(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let storage = Arc::new(
            SqlCommitGraphStorageBuilder::with_sqlite_in_memory()
                .unwrap()
                .build(RendezVousOptions::for_test(), test_repo_identity()),
        );
        let graph = from_dag(
            &ctx,
            r"
            A-B-C-D-E-F-G-H-I-J-K
            ",
            storage.clone(),
        )
        .await?;

        let bulk_fetcher = CommitGraphBulkFetcher::new(storage.clone());

        assert_eq!(bulk_fetcher.repo_bounds(&ctx, false).await?, 1..12);

        assert_stream(
            bulk_fetcher.oldest_first_stream(&ctx, 1..12, 3, false),
            vec![
                ("A", 1),
                ("B", 2),
                ("C", 3),
                ("D", 4),
                ("E", 5),
                ("F", 6),
                ("G", 7),
                ("H", 8),
                ("I", 9),
                ("J", 10),
                ("K", 11),
            ],
        )
        .await?;

        assert_stream(
            bulk_fetcher.oldest_first_stream(&ctx, 5..8, 5, false),
            vec![("E", 5), ("F", 6), ("G", 7)],
        )
        .await?;

        assert_stream(
            bulk_fetcher.oldest_first_stream(&ctx, 6..6, 5, false),
            vec![],
        )
        .await?;

        assert_stream(
            bulk_fetcher.newest_first_stream(&ctx, 1..12, 2, false),
            vec![
                ("K", 11),
                ("J", 10),
                ("I", 9),
                ("H", 8),
                ("G", 7),
                ("F", 6),
                ("E", 5),
                ("D", 4),
                ("C", 3),
                ("B", 2),
                ("A", 1),
            ],
        )
        .await?;

        assert_stream(
            bulk_fetcher.newest_first_stream(&ctx, 0..6, 4, false),
            vec![("E", 5), ("D", 4), ("C", 3), ("B", 2), ("A", 1)],
        )
        .await?;

        assert_stream(
            bulk_fetcher.newest_first_stream(&ctx, 2..10, 2, false),
            vec![
                ("I", 9),
                ("H", 8),
                ("G", 7),
                ("F", 6),
                ("E", 5),
                ("D", 4),
                ("C", 3),
                ("B", 2),
            ],
        )
        .await?;

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_repo_bounds_empty_repo(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let storage = Arc::new(
            SqlCommitGraphStorageBuilder::with_sqlite_in_memory()
                .unwrap()
                .build(RendezVousOptions::for_test(), test_repo_identity()),
        );

        // Don't add any commits — the repo is empty.
        let bulk_fetcher = CommitGraphBulkFetcher::new(storage.clone());

        // Before the fix, this would panic during SQL row deserialization
        // because min(id)/max(id) return NULL for an empty table, and the
        // query return type was (u64, u64) which can't represent NULL.
        let result = bulk_fetcher.repo_bounds(&ctx, false).await;
        assert!(
            result.is_err(),
            "repo_bounds on an empty repo should return an error, not panic"
        );
        assert!(
            format!("{:#}", result.unwrap_err()).contains("empty repo"),
            "error message should mention empty repo"
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_stream_topological(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let sql_storage = Arc::new(
            SqlCommitGraphStorageBuilder::with_sqlite_in_memory()
                .unwrap()
                .build(RendezVousOptions::for_test(), test_repo_identity()),
        );
        let storage = Arc::new(ShufflingCommitGraphStorage::new(sql_storage.clone()));

        // Create a complex DAG with branches and merges to test topological ordering
        // This graph has 28 commits with multiple branches and merge points
        let _graph = from_dag_batch(
            &ctx,
            r"
            A-B-C-D-E-F-G-H-I-J-K-L-M-N-O-P-Q-R-S-T
             \     \       /     \       /
              U-V-W-X-----Y       Z-AA-BB
            ",
            storage.clone(),
        )
        .await?;

        let bulk_fetcher = CommitGraphBulkFetcher::new(sql_storage);

        // Test with different subset ranges of the graph to ensure topological
        // ordering works correctly on various slices
        let test_ranges = vec![
            (1..=28, 28, "full graph"),
            (1..=15, 15, "first half"),
            (10..=28, 19, "last two thirds"),
            (5..=20, 16, "middle section"),
        ];

        for (range, expected_count, description) in test_ranges {
            // Use a small chunk size (3) to ensure multiple iterations through the streaming code
            let stream =
                bulk_fetcher.stream_topological::<Parents>(&ctx, range.clone(), 3, false)?;
            let result_with_edges: Vec<(ChangesetId, Vec<ChangesetId>)> = stream
                .map_ok(|edges| {
                    let cs_id = edges.node().cs_id;
                    let parents: Vec<ChangesetId> =
                        edges.parents::<Parents>().map(|node| node.cs_id).collect();
                    (cs_id, parents)
                })
                .try_collect()
                .await?;

            assert_eq!(
                result_with_edges.len(),
                expected_count,
                "Expected {expected_count} commits for range {range:?} ({description})"
            );

            let mut emitted_positions = HashMap::new();
            for (pos, (cs_id, _)) in result_with_edges.iter().enumerate() {
                emitted_positions.insert(*cs_id, pos);
            }

            // Verify topological order: all parents must be emitted before children
            for (pos, (cs_id, parents)) in result_with_edges.iter().enumerate() {
                for parent_cs_id in parents {
                    if let Some(parent_pos) = emitted_positions.get(parent_cs_id) {
                        assert!(
                            *parent_pos < pos,
                            "Parent {parent_cs_id:?} at position {parent_pos} should appear before child {cs_id:?} at position {pos} (range: {range:?}, {description})"
                        );
                    }
                }
            }
        }

        Ok(())
    }
}
