/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::ops::Bound;

use anyhow::Result;
use anyhow::anyhow;
use borrowed::borrowed;
use commit_graph_types::edges::ChangesetNode;
use commit_graph_types::edges::EdgeType;
use commit_graph_types::frontier::ChangesetFrontier;
use commit_graph_types::frontier::ChangesetFrontierWithinDistance;
use commit_graph_types::storage::Prefetch;
use commit_graph_types::storage::PrefetchScope;
use commit_graph_types::storage::PrefetchTarget;
use commit_graph_types::storage::p1_linear_prefetch_steps;
use context::CoreContext;
use context::PerfCounterType;
use futures::Future;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future;
use futures::stream;
use futures_watchdog::WatchdogExt;
use mononoke_types::ChangesetId;
use mononoke_types::FIRST_GENERATION;
use mononoke_types::Generation;

use crate::CommitGraphOps;

impl<E: EdgeType> CommitGraphOps<E> {
    /// Obtain a frontier of changesets from a single changeset id, which must
    /// exist.
    pub(crate) async fn single_frontier(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<ChangesetFrontier> {
        let generation = self.changeset_generation(ctx, cs_id).await?;
        Ok(ChangesetFrontier::new_single(cs_id, generation))
    }

    /// Obtain a frontier of changesets from a list of changeset ids, which
    /// must all exist.
    pub(crate) async fn frontier(
        &self,
        ctx: &CoreContext,
        cs_ids: Vec<ChangesetId>,
    ) -> Result<ChangesetFrontier> {
        let all_edges = self
            .storage
            .fetch_many_edges(ctx, &cs_ids, Prefetch::None)
            .await?;

        cs_ids
            .into_iter()
            .map(|cs_id| {
                Ok((
                    cs_id,
                    all_edges
                        .get(&cs_id)
                        .ok_or_else(|| anyhow!("Missing changeset in commit graph: {cs_id}"))?
                        .node()
                        .generation::<E>(),
                ))
            })
            .collect::<Result<_>>()
    }

    /// Obtain a frontier of changesets from a list of changeset ids. This frontier
    /// enforces that at any point all changesets inside of it will be reachable
    /// from the original list of changesets by traversing no more than `distance`
    /// edges.
    pub(crate) async fn frontier_within_distance(
        &self,
        ctx: &CoreContext,
        cs_ids: Vec<ChangesetId>,
        distance: u64,
    ) -> Result<ChangesetFrontierWithinDistance> {
        let all_edges = self
            .storage
            .fetch_many_edges(
                ctx,
                &cs_ids,
                Prefetch::Hint(PrefetchTarget::LinearAncestors {
                    generation: FIRST_GENERATION,
                    steps: distance + 1,
                }),
            )
            .await?;

        cs_ids
            .into_iter()
            .map(|cs_id| {
                Ok((
                    cs_id,
                    all_edges
                        .get(&cs_id)
                        .ok_or_else(|| anyhow!("Missing changeset in commit graph: {cs_id}"))?
                        .node()
                        .generation::<E>(),
                    distance,
                ))
            })
            .collect::<Result<_>>()
    }

    /// Pops the highest generation changesets of a frontier, returning any that
    /// satisfy a property and lowering the rest of them to either their immediate
    /// parents or their lowest skip tree edge that doesn't satisfy the property.
    /// Repeatedly calling this function and concatenating the output will result
    /// in the frontier of changesets satisfying the property.
    ///
    /// Returns None if the frontier is empty, and otherwise the changesets that
    /// satisfy the property along with the nodes newly added to the frontier.
    pub(crate) async fn lower_frontier_step<Property, Out>(
        &self,
        ctx: &CoreContext,
        frontier: &mut ChangesetFrontier,
        property: Property,
        prefetch: Prefetch,
    ) -> Result<Option<(Vec<ChangesetId>, Vec<ChangesetNode>)>>
    where
        Property: Fn(&ChangesetNode) -> Out + Send + Sync,
        Out: Future<Output = Result<bool>>,
    {
        match frontier.pop_last() {
            None => Ok(None),
            Some((_, cs_ids)) => {
                let cs_ids = cs_ids.into_iter().collect::<Vec<_>>();
                let frontier_edges = self
                    .storage
                    .fetch_many_edges(ctx, &cs_ids, prefetch)
                    .await?;

                let decisions = stream::iter(frontier_edges)
                    .map(|(_cs_id, edges)| {
                        borrowed!(property);
                        async move {
                            if property(edges.node()).await? {
                                return anyhow::Ok((Some(edges.node().cs_id), vec![]));
                            }

                            let next_nodes = match edges
                                .lowest_skip_tree_edge_with::<E, _, _>(|node| {
                                    borrowed!(property);
                                    async move { Ok(!property(node).await?) }
                                })
                                .await?
                                .copied()
                            {
                                Some(ancestor) => vec![ancestor],
                                None => edges.parents::<E>().copied().collect(),
                            };
                            anyhow::Ok((None, next_nodes))
                        }
                    })
                    .buffer_unordered(100)
                    .try_collect::<Vec<(Option<ChangesetId>, Vec<ChangesetNode>)>>()
                    .await?;

                let mut property_frontier: Vec<_> = Default::default();
                let mut added = Vec::new();

                for (property_match, next_nodes) in decisions {
                    if let Some(cs_id) = property_match {
                        property_frontier.push(cs_id);
                    }
                    for node in next_nodes {
                        if frontier
                            .entry(node.generation::<E>())
                            .or_default()
                            .insert(node.cs_id)
                        {
                            added.push(node);
                        }
                    }
                }

                Ok(Some((property_frontier, added)))
            }
        }
    }

    /// Lower a frontier so that it contains the highest ancestors of the
    /// frontier that have a generation number less than or equal to
    /// `generation`.
    pub(crate) async fn lower_frontier(
        &self,
        ctx: &CoreContext,
        frontier: &mut ChangesetFrontier,
        target_generation: Generation,
    ) -> Result<()> {
        self.lower_frontier_impl(ctx, frontier, target_generation, false)
            .await
    }

    /// Same as `lower_frontier`, for callers that will keep lowering the same
    /// frontier in small steps afterwards (e.g. one generation at a time
    /// while streaming ancestors).
    ///
    /// Small steps prefetch along first parents so that the steps that follow
    /// are cache hits.  Large steps still prefetch the exact skip tree path,
    /// which is what the skew binary traversal visits.
    pub(crate) async fn lower_frontier_incrementally(
        &self,
        ctx: &CoreContext,
        frontier: &mut ChangesetFrontier,
        target_generation: Generation,
    ) -> Result<()> {
        self.lower_frontier_impl(ctx, frontier, target_generation, true)
            .await
    }

    async fn lower_frontier_impl(
        &self,
        ctx: &CoreContext,
        frontier: &mut ChangesetFrontier,
        target_generation: Generation,
        incremental: bool,
    ) -> Result<()> {
        let linear_steps = p1_linear_prefetch_steps();
        let prefetch_target = |generation: Generation| {
            if incremental && generation.value() - target_generation.value() <= linear_steps {
                PrefetchTarget::LinearAncestors {
                    generation: FIRST_GENERATION,
                    steps: linear_steps,
                }
            } else {
                PrefetchTarget::ExactSkipTreeAncestors {
                    generation: target_generation,
                }
            }
        };
        let prefetch_from = |generation: Generation| Prefetch::Hint(prefetch_target(generation));

        let batch_prefetch = justknobs::eval(
            "scm/mononoke:commit_graph_pull_optimizations",
            ctx.client_request_info().map(|c| c.correlator.as_str()),
            Some(self.storage.repo_name()),
        );
        // The number of changesets above the target, and the ones among them
        // that haven't been batch prefetched, are kept up to date from what
        // each step pops and adds.  Scanning the frontier for them instead
        // would cost its size above the target on every step that grows it.
        let mut above_target = 0;
        let mut above_target_before = 0;
        let mut unbatched: Vec<(ChangesetId, Generation)> = Vec::new();
        if batch_prefetch {
            unbatched.extend(
                frontier
                    .range((Bound::Excluded(target_generation), Bound::Unbounded))
                    .flat_map(|(generation, cs_ids)| {
                        cs_ids.iter().map(|cs_id| (*cs_id, *generation))
                    }),
            );
            above_target = unbatched.len();
        }

        loop {
            tokio::task::consume_budget().await;

            let (generation, popped) = match frontier.last_key_value() {
                None => return Ok(()),
                Some((generation, _)) if *generation <= target_generation => {
                    return Ok(());
                }
                Some((generation, cs_ids)) => (*generation, cs_ids.len()),
            };

            // Whenever the frontier holds more changesets above the target
            // than it did before the previous step, prefetch the descent of
            // the ones not prefetched this way before, all at once.  Below a
            // merge of unrelated histories the frontier holds many changesets
            // at different generations, and lowering them one generation at
            // a time would otherwise miss the cache once for each.  Only the
            // caches are warmed, and only for changesets that aren't cached:
            // the frontier can hold thousands of changesets at every step,
            // so looking past the cached ones would fill far more than the
            // traversal reads.  The edges themselves are fetched by the
            // steps that pop them.
            if batch_prefetch {
                if above_target > 1 && above_target > above_target_before {
                    // Changesets popped since they were added have been
                    // fetched by the step that popped them.
                    let cs_ids: Vec<_> = unbatched
                        .drain(..)
                        .filter(|(cs_id, generation)| {
                            frontier
                                .get(generation)
                                .is_some_and(|cs_ids| cs_ids.contains(cs_id))
                        })
                        .map(|(cs_id, _)| cs_id)
                        .collect();
                    ctx.perf_counters().add_to_counter(
                        PerfCounterType::CommitGraphBatchPrefetchScanned,
                        cs_ids.len() as i64,
                    );
                    if !cs_ids.is_empty() {
                        self.storage
                            .prefetch_many_edges(
                                ctx,
                                &cs_ids,
                                PrefetchScope::UncachedOnly(prefetch_target(generation)),
                            )
                            .await?;
                    }
                }
                above_target_before = above_target;
            }

            let prefetch = prefetch_from(generation);

            let step = self
                .lower_frontier_step(
                    ctx,
                    frontier,
                    move |node| future::ready(Ok(node.generation::<E>() < target_generation)),
                    prefetch,
                )
                .watched()
                .await?;

            if batch_prefetch && let Some((_, added)) = step {
                above_target -= popped;
                for node in added {
                    let generation = node.generation::<E>();
                    if generation > target_generation {
                        above_target += 1;
                        unbatched.push((node.cs_id, generation));
                    }
                }
            }
        }
    }

    /// Lower the highest generation changesets of a frontier
    /// to their immediate parents.
    pub(crate) async fn lower_frontier_highest_generation(
        &self,
        ctx: &CoreContext,
        frontier: &mut ChangesetFrontier,
    ) -> Result<()> {
        if let Some((_, cs_ids)) = frontier.pop_last() {
            let cs_ids = cs_ids.into_iter().collect::<Vec<_>>();
            let frontier_edges = self
                .storage
                .fetch_many_edges(ctx, &cs_ids, Prefetch::for_p1_linear_traversal())
                .await?;

            for cs_id in cs_ids {
                let edges = frontier_edges
                    .get(&cs_id)
                    .ok_or_else(|| anyhow!("Missing changeset in commit graph: {cs_id}"))?;

                for parent in edges.parents::<E>() {
                    frontier
                        .entry(parent.generation::<E>())
                        .or_default()
                        .insert(parent.cs_id);
                }
            }
        }
        Ok(())
    }

    /// Minimize a frontier by removing all changesets that are ancestors of other changesets
    /// in the frontier.
    pub async fn minimize_frontier(
        &self,
        ctx: &CoreContext,
        frontier: Vec<ChangesetId>,
    ) -> Result<Vec<ChangesetId>> {
        // Process the frontier generation by generation starting from the highest,
        // removing changesets that are ancestors of a higher generation changeset.

        let mut processed_frontier = ChangesetFrontier::new();
        let mut remaining_frontier = self.frontier(ctx, frontier).await?;
        let mut minimal_frontier = vec![];

        while let Some((generation, cs_ids)) = remaining_frontier.pop_last() {
            // Lower the frontier of the previously processed generations to the current
            // generation. Any changeset that's contained in this frontier is an ancestor
            // of a higher generation changeset and should be removed.
            self.lower_frontier(ctx, &mut processed_frontier, generation)
                .await?;

            let new_cs_ids = cs_ids
                .iter()
                .copied()
                .filter(|cs_id| !processed_frontier.highest_generation_contains(*cs_id, generation))
                .collect::<Vec<_>>();

            minimal_frontier.extend(new_cs_ids.clone());
            processed_frontier.extend(new_cs_ids.into_iter().map(|cs_id| (cs_id, generation)))
        }

        Ok(minimal_frontier)
    }
}
