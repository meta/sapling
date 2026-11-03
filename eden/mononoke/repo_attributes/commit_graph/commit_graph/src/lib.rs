/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Commit Graph
//!
//! The graph of all commits in the repository.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use borrowed::borrowed;
use buffered_commit_graph_storage::BufferedCommitGraphStorage;
use commit_graph_types::edges::ChangesetEdges;
use commit_graph_types::edges::ChangesetNode;
pub use commit_graph_types::edges::ChangesetParents;
pub use commit_graph_types::edges::ChangesetSubtreeSources;
use commit_graph_types::edges::EdgeType;
use commit_graph_types::edges::FirstParentLinear;
use commit_graph_types::edges::Parents;
use commit_graph_types::edges::ParentsAndSubtreeSources;
use commit_graph_types::frontier::AncestorsWithinDistance;
use commit_graph_types::frontier::ChangesetFrontier;
use commit_graph_types::frontier::ChangesetFrontierWithinDistance;
use commit_graph_types::segments::BoundaryChangesets;
pub use commit_graph_types::segments::ChangesetSegment;
use commit_graph_types::segments::SegmentDescription;
use commit_graph_types::segments::SegmentedSliceDescription;
pub use commit_graph_types::segments::SegmentedSliceWithBoundaries;
use commit_graph_types::storage::CommitGraphStorage;
use commit_graph_types::storage::Prefetch;
use commit_graph_types::storage::PrefetchTarget;
use context::CoreContext;
use futures::Future;
use futures::FutureExt;
use futures::StreamExt;
use futures::TryFutureExt;
use futures::TryStreamExt;
use futures::stream;
use futures::stream::BoxStream;
use futures::stream::FuturesUnordered;
use futures_watchdog::WatchdogExt;
use itertools::Itertools;
use memwrites_commit_graph_storage::MemWritesCommitGraphStorage;
use mononoke_types::ChangesetId;
use mononoke_types::ChangesetIdPrefix;
use mononoke_types::ChangesetIdsResolvedFromPrefix;
use mononoke_types::FIRST_GENERATION;
use mononoke_types::Generation;
use smallvec::smallvec;
use vec1::Vec1;

pub use crate::ancestors_stream::AncestorsStreamBuilder;
pub use crate::compat::ParentsFetcher;
pub use crate::linear::LinearAncestorsStreamBuilder;
pub use crate::writer::ArcCommitGraphWriter;
pub use crate::writer::BaseCommitGraphWriter;
pub use crate::writer::CommitGraphWriter;
pub use crate::writer::CommitGraphWriterArc;
pub use crate::writer::CommitGraphWriterRef;
pub use crate::writer::LoggingCommitGraphWriter;

mod ancestors_stream;
mod compat;
mod core;
mod frontier;
mod linear;
mod segments;
mod writer;

#[cfg(test)]
mod tests;

/// Commit Graph.
///
/// This contains the graph of all commits known to Mononoke for a particular
/// repository.  It provides methods for traversing the commit graph and
/// finding out graph-related information for the changesets contained
/// therein.
#[derive(Clone)]
#[facet::facet]
pub struct CommitGraph {
    /// By default, the commit graph performs operations over the normal commit graph that uses the commits' parents.
    parent_ops: CommitGraphOps<Parents>,
}

#[derive(Clone)]
pub struct CommitGraphOps<E: EdgeType> {
    /// The storage back-end where the commits are actually stored.
    storage: Arc<dyn CommitGraphStorage>,
    _phantom: std::marker::PhantomData<E>,
}

impl std::ops::Deref for CommitGraph {
    type Target = CommitGraphOps<Parents>;

    fn deref(&self) -> &Self::Target {
        &self.parent_ops
    }
}

impl CommitGraph {
    pub fn new(storage: Arc<dyn CommitGraphStorage>) -> Self {
        Self {
            parent_ops: CommitGraphOps {
                storage,
                _phantom: std::marker::PhantomData,
            },
        }
    }

    pub fn clone_with_replaced_storage(
        &self,
        replace_fn: impl FnOnce(Arc<dyn CommitGraphStorage>) -> Arc<dyn CommitGraphStorage>,
    ) -> Self {
        Self {
            parent_ops: CommitGraphOps {
                storage: replace_fn(self.parent_ops.storage.clone()),
                _phantom: std::marker::PhantomData,
            },
        }
    }

    pub fn with_memwrites_storage(self) -> Self {
        Self {
            parent_ops: CommitGraphOps {
                storage: Arc::new(MemWritesCommitGraphStorage::new(self.parent_ops.storage)),
                _phantom: std::marker::PhantomData,
            },
        }
    }

    pub fn parents_graph(&self) -> CommitGraphOps<Parents> {
        self.parent_ops.clone()
    }

    pub fn p1_linear_graph(&self) -> CommitGraphOps<FirstParentLinear> {
        CommitGraphOps {
            storage: self.parent_ops.storage.clone(),
            _phantom: std::marker::PhantomData,
        }
    }

    pub fn parents_and_subtree_sources_graph(&self) -> CommitGraphOps<ParentsAndSubtreeSources> {
        CommitGraphOps {
            storage: self.parent_ops.storage.clone(),
            _phantom: std::marker::PhantomData,
        }
    }

    /// Add a new changeset to the commit graph.
    ///
    /// Returns true if a new changeset was inserted, or false if the
    /// changeset already existed.
    pub(crate) async fn add(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
        parents: ChangesetParents,
        subtree_sources: ChangesetSubtreeSources,
    ) -> Result<bool> {
        let parent_edges = if subtree_sources.is_empty() {
            self.storage
                .fetch_many_edges(ctx, &parents, Prefetch::None)
                .await?
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect()
        } else {
            let parents_and_sources = parents
                .iter()
                .chain(subtree_sources.iter())
                .copied()
                .collect::<Vec<_>>();
            self.storage
                .fetch_many_edges(ctx, &parents_and_sources, Prefetch::None)
                .await?
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect()
        };

        self.storage
            .add(
                ctx,
                self.build_edges(ctx, cs_id, parents, subtree_sources, &parent_edges)
                    .await?,
            )
            .await
    }

    /// Add many new changesets to the commit graph. Changesets should
    /// be sorted in topological order.
    ///
    /// Returns the number of newly added changesets to the commit graph.
    pub(crate) async fn add_many(
        &self,
        ctx: &CoreContext,
        changesets: Vec1<(ChangesetId, ChangesetParents, ChangesetSubtreeSources)>,
    ) -> Result<usize> {
        // Find all parents that should already exist in the commit graph
        // and fetch their edges.
        let changesets_set: HashSet<ChangesetId> = changesets
            .iter()
            .map(|(cs_id, _, _)| cs_id)
            .copied()
            .collect();
        let parents_to_fetch = changesets
            .iter()
            .flat_map(|(_, parents, subtree_sources)| parents.iter().chain(subtree_sources.iter()))
            .filter(|cs_id| !changesets_set.contains(cs_id))
            .copied()
            .collect::<Vec<_>>();
        let mut edges_map: HashMap<ChangesetId, ChangesetEdges> = self
            .storage
            .fetch_many_edges(ctx, &parents_to_fetch, Prefetch::None)
            .await
            .with_context(|| "during commit_graph::add_many (fetch_many_edges)")?
            .into_iter()
            .map(|(k, v)| (k, v.into()))
            .collect();

        // We use buffered storage here to be able to do all the writes in parallel.
        // We need to create a new CommitGraph wrapper to work with the buffered storage.
        let buffered_storage =
            Arc::new(BufferedCommitGraphStorage::new(self.storage.clone(), 10000));
        let graph = CommitGraph::new(buffered_storage.clone());
        for (cs_id, parents, subtree_sources) in changesets {
            let edges = graph
                .build_edges(ctx, cs_id, parents, subtree_sources, &edges_map)
                .await?;
            edges_map.insert(cs_id, edges.clone());
            buffered_storage.add(ctx, edges).await?;
        }
        buffered_storage.flush(ctx).await
    }

    /// Find all changeset ids with a given prefix.
    pub async fn find_by_prefix(
        &self,
        ctx: &CoreContext,
        cs_prefix: ChangesetIdPrefix,
        limit: usize,
    ) -> Result<ChangesetIdsResolvedFromPrefix> {
        self.storage.find_by_prefix(ctx, cs_prefix, limit).await
    }

    /// Returns true if the changeset exists.
    pub async fn exists(&self, ctx: &CoreContext, cs_id: ChangesetId) -> Result<bool> {
        let edges = self.storage.maybe_fetch_edges(ctx, cs_id).await?;
        Ok(edges.is_some())
    }

    /// Returns the parents of many changesets.
    pub async fn many_changeset_parents(
        &self,
        ctx: &CoreContext,
        cs_ids: &[ChangesetId],
    ) -> Result<HashMap<ChangesetId, ChangesetParents>> {
        let fetched_edges = self
            .storage
            .fetch_many_edges(ctx, cs_ids, Prefetch::None)
            .await?;
        Ok(fetched_edges
            .into_iter()
            .map(|(cs_id, fetched_edges)| {
                (
                    cs_id,
                    fetched_edges
                        .edges()
                        .parents::<Parents>()
                        .map(|parents| parents.cs_id)
                        .collect(),
                )
            })
            .collect())
    }

    /// Returns the subtree sources of a single changeset.
    pub async fn changeset_subtree_sources(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<ChangesetSubtreeSources> {
        let edges = self.storage.fetch_edges(ctx, cs_id).await?;
        Ok(edges
            .subtree_sources()
            .map(|subtree_source| subtree_source.cs_id)
            .collect())
    }

    /// Returns the generation number of many changesets.
    pub async fn many_changeset_generations(
        &self,
        ctx: &CoreContext,
        cs_ids: &[ChangesetId],
    ) -> Result<HashMap<ChangesetId, Generation>> {
        let fetched_edges = self
            .storage
            .fetch_many_edges(ctx, cs_ids, Prefetch::None)
            .await?;
        Ok(fetched_edges
            .into_iter()
            .map(|(cs_id, fetched_edges)| (cs_id, fetched_edges.node().generation::<Parents>()))
            .collect())
    }

    /// Returns the linear depth of a single changeset. Linear depth is calculated as the
    /// number of ancestors of the commit if the commit graph consisted only of the first
    /// parents (i.e. if merges were ignored).
    pub async fn changeset_linear_depth(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<u64> {
        let edges = self.storage.fetch_edges(ctx, cs_id).await?;
        Ok(edges.node().skip_tree_depth::<FirstParentLinear>())
    }

    /// Returns the linear depth of many changesets. Linear depth is calculated as the
    /// number of ancestors of the commit if the commit graph consisted only of the first
    /// parents (i.e. if merges were ignored).
    pub async fn many_changeset_linear_depths(
        &self,
        ctx: &CoreContext,
        cs_ids: &[ChangesetId],
    ) -> Result<HashMap<ChangesetId, u64>> {
        let fetched_edges = self
            .storage
            .fetch_many_edges(ctx, cs_ids, Prefetch::None)
            .await?;
        Ok(fetched_edges
            .into_iter()
            .map(|(cs_id, fetched_edges)| {
                (
                    cs_id,
                    fetched_edges.node().skip_tree_depth::<FirstParentLinear>(),
                )
            })
            .collect())
    }

    /// Return only the changesets that are found in the commit graph.
    pub async fn known_changesets(
        &self,
        ctx: &CoreContext,
        cs_ids: Vec<ChangesetId>,
    ) -> Result<Vec<ChangesetId>> {
        let edges = self
            .storage
            .maybe_fetch_many_edges(ctx, &cs_ids, Prefetch::None)
            .await?;
        Ok(edges.into_keys().collect())
    }

    /// Returns the children of a single changeset.
    pub async fn changeset_children(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<Vec<ChangesetId>> {
        self.storage.fetch_children(ctx, cs_id).await
    }

    /// Returns the union of descendants of `cs_ids`.
    pub async fn descendants(
        &self,
        ctx: &CoreContext,
        cs_ids: Vec<ChangesetId>,
    ) -> Result<Vec<ChangesetId>> {
        let mut visited: HashSet<ChangesetId> = cs_ids.iter().copied().collect();
        let mut descendants: Vec<ChangesetId> = cs_ids.clone();

        // We will add a future for every traversed changeset to futs to
        // fetch its children.
        let mut futs: FuturesUnordered<_> = Default::default();

        // Add children of initial changesets.
        for cs_id in cs_ids {
            futs.push(self.changeset_children(ctx, cs_id));
        }

        while let Some(result) = futs.next().await {
            let children = result?;

            for child in children {
                // If we haven't traversed this changeset yet, add it to the output
                // and add a future to fetch its children to futs.
                if visited.insert(child) {
                    descendants.push(child);
                    futs.push(self.changeset_children(ctx, child));
                }
            }
        }

        Ok(descendants)
    }

    /// Returns true if the changesets that are ancestors of `descendant`
    /// excluding strict ancestors of `ancestor` form a linear stack i.e.
    /// none of them have more than one parent and `ancestor` is an ancestor
    /// of `descendant`.
    pub async fn is_linear_stack(
        &self,
        ctx: &CoreContext,
        ancestor: ChangesetId,
        descendant: ChangesetId,
    ) -> Result<bool> {
        let (ancestor_edges, descendant_edges) = futures::try_join!(
            self.parent_ops.storage.fetch_edges(ctx, ancestor),
            self.parent_ops.storage.fetch_edges(ctx, descendant),
        )?;

        let target_gen = ancestor_edges.node().generation::<Parents>();

        if descendant_edges.node().generation::<Parents>() < target_gen {
            return Ok(false);
        }

        if descendant_edges.parent_count() > 1 {
            return Ok(false);
        }

        if let Some(merge_ancestor) = descendant_edges.merge_ancestor_or_root::<Parents>()
            && merge_ancestor.generation::<Parents>() > FIRST_GENERATION
            && merge_ancestor.generation::<Parents>() >= target_gen
        {
            return Ok(false);
        }

        let mut frontier = ChangesetFrontier::new_single(
            descendant,
            descendant_edges.node().generation::<Parents>(),
        );

        self.lower_frontier(ctx, &mut frontier, target_gen).await?;
        Ok(frontier.highest_generation_contains(ancestor, target_gen))
    }
}

impl<E: EdgeType> CommitGraphOps<E> {
    pub(crate) async fn changeset_node(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<ChangesetNode> {
        let edges = self.storage.fetch_edges(ctx, cs_id).await?;
        Ok(*edges.node())
    }

    /// Returns the parents of a single changeset.
    pub async fn changeset_parents(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<ChangesetParents> {
        let edges = self.storage.fetch_edges(ctx, cs_id).await?;
        Ok(edges.parents::<E>().map(|parent| parent.cs_id).collect())
    }

    /// Returns the generation number of a single changeset.
    pub async fn changeset_generation(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<Generation> {
        let edges = self.storage.fetch_edges(ctx, cs_id).await?;
        Ok(edges.node().generation::<E>())
    }

    /// Returns a frontier for the ancestors of heads
    /// that satisfy a given property.
    ///
    /// Note: The property needs to be monotonic i.e. if the
    /// property holds for one changeset then it has to hold
    /// for all its parents.
    pub async fn ancestors_frontier_with<'a, MonotonicProperty, Out>(
        &'a self,
        ctx: &'a CoreContext,
        heads: Vec<ChangesetId>,
        monotonic_property: MonotonicProperty,
    ) -> Result<Vec<ChangesetId>>
    where
        MonotonicProperty: Fn(ChangesetId) -> Out + Send + Sync + 'a,
        Out: Future<Output = Result<bool>>,
    {
        let mut ancestors_frontier = vec![];
        let mut frontier = self.frontier(ctx, heads).await?;

        let monotonic_property = move |node: &ChangesetNode| {
            borrowed!(monotonic_property);
            monotonic_property(node.cs_id)
        };

        while let Some((ancestors_frontier_extension, _)) = self
            .lower_frontier_step(ctx, &mut frontier, &monotonic_property, Prefetch::None)
            .await?
        {
            ancestors_frontier.extend(ancestors_frontier_extension);
        }

        Ok(ancestors_frontier.into_iter().collect())
    }

    /// Returns true if the ancestor changeset is an ancestor of the descendant
    /// changeset.
    ///
    /// Ancestry is inclusive: a commit is its own ancestor.
    pub async fn is_ancestor(
        &self,
        ctx: &CoreContext,
        ancestor: ChangesetId,
        descendant: ChangesetId,
    ) -> Result<bool> {
        let (mut frontier, target_gen) = futures::try_join!(
            self.single_frontier(ctx, descendant),
            self.changeset_generation(ctx, ancestor)
        )?;
        debug_assert!(!frontier.is_empty(), "frontier should contain descendant");
        self.lower_frontier(ctx, &mut frontier, target_gen).await?;
        Ok(frontier.highest_generation_contains(ancestor, target_gen))
    }

    /// Returns true if the ancestor changeset is an ancestor of any of
    /// the descendant changesets.
    ///
    /// Ancestry is inclusive: a commit is its own ancestor.
    pub async fn is_ancestor_of_any(
        &self,
        ctx: &CoreContext,
        ancestor: ChangesetId,
        descendants: Vec<ChangesetId>,
    ) -> Result<bool> {
        let (mut frontier, target_gen) = futures::try_join!(
            self.frontier(ctx, descendants),
            self.changeset_generation(ctx, ancestor)
        )?;
        self.lower_frontier(ctx, &mut frontier, target_gen).await?;
        Ok(frontier.highest_generation_contains(ancestor, target_gen))
    }

    /// Given a descendant commit and a list of candidate ancestor commits,
    /// returns only those candidates that are actual ancestors of the
    /// descendant, ordered by generation from highest to lowest. Uses
    /// progressive frontier lowering to share traversal work across all
    /// candidates — more efficient than N individual `is_ancestor` calls.
    ///
    /// Ancestry is inclusive: a commit is its own ancestor.
    pub async fn filter_ancestors(
        &self,
        ctx: &CoreContext,
        descendant: ChangesetId,
        candidates: Vec<ChangesetId>,
    ) -> Result<Vec<ChangesetId>> {
        if candidates.is_empty() {
            return Ok(vec![]);
        }

        // Fetch generations for descendant and all candidates in one batch.
        // fetch_many_edges returns a map, so duplicate candidates are
        // naturally deduplicated without an extra allocation.
        let descendant_gen = self.changeset_generation(ctx, descendant).await?;
        let all_edges = self
            .storage
            .fetch_many_edges(ctx, &candidates, Prefetch::None)
            .await?;

        // Sort candidates by generation descending (highest first)
        // so we can progressively lower the frontier.
        // Skip candidates with generation > descendant — they can't
        // be ancestors.
        let mut candidates_with_gen: Vec<(ChangesetId, Generation)> = candidates
            .into_iter()
            .filter_map(|cs_id| {
                let generation = all_edges.get(&cs_id)?.node().generation::<E>();
                if generation > descendant_gen {
                    return None;
                }
                Some((cs_id, generation))
            })
            .collect();
        candidates_with_gen.sort_by_key(|candidate| Reverse(candidate.1));

        // Build frontier from descendant
        let mut frontier = self.single_frontier(ctx, descendant).await?;
        let mut results = Vec::new();

        // Progressively lower frontier, checking each candidate
        for (cs_id, generation) in candidates_with_gen {
            self.lower_frontier(ctx, &mut frontier, generation).await?;
            if frontier.highest_generation_contains(cs_id, generation) {
                results.push(cs_id);
            }
        }

        Ok(results)
    }

    /// Returns a stream of all ancestors of any changeset in heads,
    /// excluding any ancestor of any changeset in common, in reverse
    /// topological order.
    pub async fn ancestors_difference_stream(
        &self,
        ctx: &CoreContext,
        heads: Vec<ChangesetId>,
        common: Vec<ChangesetId>,
    ) -> Result<BoxStream<'static, Result<ChangesetId>>> {
        AncestorsStreamBuilder::new(Arc::new(self.clone()), ctx.clone(), heads)
            .exclude_ancestors_of(common)
            .build()
            .await
    }

    /// Returns all ancestors of any changeset in heads, excluding any
    /// ancestor of any changeset in common, in reverse topological order.
    pub async fn ancestors_difference(
        &self,
        ctx: &CoreContext,
        heads: Vec<ChangesetId>,
        common: Vec<ChangesetId>,
    ) -> Result<Vec<ChangesetId>> {
        self.ancestors_difference_stream(ctx, heads, common)
            .await?
            .try_collect()
            .await
    }

    /// Returns all ancestors of any changeset in `heads` that are reachable
    /// by taking no more than `max_distance` edges from some changeset in `heads`,
    /// as well as the boundary changesets which are the changesets for which
    /// the shortest distance to them is exactly `max_distance`.
    /// NOTE: The ancestors property of AncestorsWithinDistance represents the ancestors
    /// which are inside the boundary. The boundary ancestors are represented via the
    /// boundaries field.
    pub async fn ancestors_within_distance(
        &self,
        ctx: &CoreContext,
        heads: Vec<ChangesetId>,
        max_distance: u64,
    ) -> Result<AncestorsWithinDistance> {
        let cs_id_and_distance = self
            .ancestors_within_distance_stream(ctx, heads, max_distance)
            .await?
            .try_collect::<Vec<_>>()
            .await?;

        let (boundary, ancestors): (Vec<_>, Vec<_>) = cs_id_and_distance
            .iter()
            .partition(|(_, distance)| *distance == max_distance);

        Ok(AncestorsWithinDistance {
            ancestors: ancestors.into_iter().map(|(cs_id, _)| cs_id).collect(),
            boundaries: boundary.into_iter().map(|(cs_id, _)| cs_id).collect(),
        })
    }

    /// Returns a stream of all ancestors of any changeset in `heads` that are
    /// reachable by taking no more than `max_distance` edges from some changeset
    /// in `heads`. For each changeset we returns its changeset id alongside
    /// the minimum distance it takes to reach the changeset.
    pub async fn ancestors_within_distance_stream(
        &self,
        ctx: &CoreContext,
        heads: Vec<ChangesetId>,
        max_distance: u64,
    ) -> Result<BoxStream<'static, Result<(ChangesetId, u64)>>> {
        let frontier = self
            .frontier_within_distance(ctx, heads, max_distance)
            .await?;

        struct AncestorsWithinDistanceState<E: EdgeType> {
            commit_graph: CommitGraphOps<E>,
            ctx: CoreContext,
            frontier: ChangesetFrontierWithinDistance,
        }

        Ok(stream::try_unfold(
            Box::new(AncestorsWithinDistanceState {
                commit_graph: self.clone(),
                ctx: ctx.clone(),
                frontier,
            }),
            move |mut state| async move {
                let AncestorsWithinDistanceState {
                    commit_graph,
                    ctx,
                    frontier,
                } = &mut *state;

                if let Some((_generation, cs_ids_and_remaining_distance)) = frontier.pop_last() {
                    let output_cs_ids = cs_ids_and_remaining_distance
                        .iter()
                        .map(|(cs_id, remaining_distance)| (*cs_id, max_distance - *remaining_distance))
                        .collect::<Vec<_>>();

                    let max_remaining_distance = cs_ids_and_remaining_distance.values().copied()
                        .max()
                        .unwrap_or_default();

                    let cs_ids_to_lower = cs_ids_and_remaining_distance
                        .iter()
                        .filter(|(_, distance)| **distance >= 1)
                        .map(|(cs_id, _)| *cs_id)
                        .collect::<Vec<_>>();

                    let all_edges = commit_graph
                        .storage
                        .fetch_many_edges(
                            ctx,
                            &cs_ids_to_lower,
                            Prefetch::Hint(PrefetchTarget::LinearAncestors {
                                generation: FIRST_GENERATION,
                                steps: max_remaining_distance + 1,
                            }),
                        )
                        .await?;

                    for (cs_id, edges) in all_edges.into_iter() {
                        let distance = *cs_ids_and_remaining_distance
                            .get(&cs_id)
                            .ok_or_else(|| anyhow!("missing distance for changeset {cs_id} (in CommitGraph::ancestors_within_distance)"))?;
                        for parent in edges.parents::<E>() {
                            let parent_distance = frontier
                                .entry(parent.generation::<E>())
                                .or_default()
                                .entry(parent.cs_id)
                                .or_default();
                            *parent_distance = std::cmp::max(*parent_distance, distance - 1);
                        }
                    }

                    anyhow::Ok(Some((stream::iter(output_cs_ids).map(Ok), state)))
                } else {
                    Ok(None)
                }
            },
        )
        .try_flatten()
        .boxed())
    }

    /// Returns a stream of all changesets that are both descendants of
    /// start_id and ancestors of end_id, in topological order.
    pub async fn range_stream(
        &self,
        ctx: &CoreContext,
        start_id: ChangesetId,
        end_id: ChangesetId,
    ) -> Result<BoxStream<'static, ChangesetId>> {
        let range: Vec<_> =
            AncestorsStreamBuilder::new(Arc::new(self.clone()), ctx.clone(), vec![end_id])
                .descendants_of(start_id)
                .build()
                .await?
                .try_collect()
                .await?;

        Ok(stream::iter(range.into_iter().rev()).boxed())
    }

    /// Returns all of the highest generation changesets that
    /// are ancestors of both u and v, sorted by changeset id.
    pub async fn common_base(
        &self,
        ctx: &CoreContext,
        u: ChangesetId,
        v: ChangesetId,
    ) -> Result<Vec<ChangesetId>> {
        let (mut u_frontier, mut v_frontier) =
            futures::try_join!(self.single_frontier(ctx, u), self.single_frontier(ctx, v))?;

        loop {
            let u_gen = match u_frontier.last_key_value() {
                Some((r#gen, _)) => *r#gen,
                // if u_frontier is empty then there are no common ancestors.
                None => return Ok(vec![]),
            };

            // lower v_frontier to the highest generation of u_frontier
            self.lower_frontier(ctx, &mut v_frontier, u_gen)
                .watched()
                .await?;

            // Check if the highest generation of u_frontier intersects with v_frontier
            // and return the intersection if so.
            let mut intersection = u_frontier.highest_generation_intersection(&v_frontier);
            if !intersection.is_empty() {
                intersection.sort();
                return Ok(intersection);
            }

            let u_highest_generation_edges = match u_frontier
                .last_key_value()
                .and_then(|(_, cs_ids)| cs_ids.iter().next())
            {
                Some(cs_id) => self.storage.fetch_edges(ctx, *cs_id).watched().await?,
                None => return Ok(vec![]),
            };

            // Try to lower u_frontier to the generation of one of its
            // highest generation changesets' skip tree skew ancestor.
            // This is optimized for the case where u_frontier has only
            // one changeset, but is correct in all cases.
            if let Some(ancestor) = u_highest_generation_edges.skip_tree_skew_ancestor::<E>() {
                let mut lowered_u_frontier = u_frontier.clone();
                let mut lowered_v_frontier = v_frontier.clone();

                self.lower_frontier(ctx, &mut lowered_u_frontier, ancestor.generation::<E>())
                    .watched()
                    .await?;
                self.lower_frontier(ctx, &mut lowered_v_frontier, ancestor.generation::<E>())
                    .watched()
                    .await?;

                // If the two lowered frontier are disjoint then it's safe to lower,
                // otherwise there might be a higher generation common ancestor.
                if lowered_u_frontier.is_disjoint(&lowered_v_frontier) {
                    u_frontier = lowered_u_frontier;
                    v_frontier = lowered_v_frontier;

                    continue;
                }
            }

            // If we could lower u_frontier using the skip tree skew ancestor
            // lower only the highest generation instead.
            self.lower_frontier_highest_generation(ctx, &mut u_frontier)
                .watched()
                .await?;
        }
    }

    /// Slices ancestors of heads into a sequence of slices for processing.
    ///
    /// Each slice contains a frontier of changesets within a generation range, returning
    /// (slice_start, slice_frontier) corresponds to the frontier that has generations numbers
    /// within [slice_start..(slice_start + slice_size)].
    ///
    /// Useful for any type of processing that needs to happen on ancestors of changesets first.
    /// By processing slices one by one we avoid traversing the entire history all at once.
    ///
    /// The returned slices consist only of frontiers which haven't been processed yet
    /// (determined by the provided needs_processing function). Slicing stops once we
    /// reach a frontier with all its changesets processed.
    pub async fn slice_ancestors<NeedsProcessing, Out>(
        &self,
        ctx: &CoreContext,
        heads: Vec<ChangesetId>,
        needs_processing: NeedsProcessing,
        slice_size: u64,
    ) -> Result<Vec<(Generation, Vec<ChangesetId>)>>
    where
        NeedsProcessing: Fn(Vec<ChangesetId>) -> Out,
        Out: Future<Output = Result<HashSet<ChangesetId>>>,
    {
        let mut frontier = self.frontier(ctx, heads).await?;

        let max_gen = match frontier.last_key_value() {
            Some((r#gen, _)) => r#gen,
            None => return Ok(vec![]),
        };

        // The start of the slice is largest number in the sequence
        // 1, slice_size + 1, 2 * slice_size + 1 ...
        let mut slice_start = ((max_gen.value() - 1) / slice_size) * slice_size + 1;

        let mut slices = vec![];

        // Loop over slices in decreasing order of start generation.
        loop {
            let needed_cs_ids = needs_processing(frontier.changesets()).await?;
            frontier = frontier
                .into_flat_iter()
                .filter(|(cs_id, _)| needed_cs_ids.contains(cs_id))
                .collect();

            if frontier.is_empty() {
                break;
            }

            // Only push changesets that are in this slice's range.
            // Any remaining changesets will be pushed in the next iterations.
            slices.push((
                Generation::new(slice_start),
                frontier
                    .changesets_in_range(
                        Generation::new(slice_start)..Generation::new(slice_start + slice_size),
                    )
                    // Sort to make the output deterministic.
                    .sorted()
                    .collect(),
            ));

            if slice_start > 1 {
                // Lower the frontier to the end of the next slice (current slice_start - 1).
                self.lower_frontier(ctx, &mut frontier, Generation::new(slice_start - 1))
                    .await?;
                slice_start -= slice_size;
            } else {
                break;
            }
        }

        Ok(slices.into_iter().rev().collect())
    }

    /// Slices ancestors of `heads` excluding ancestors of `common` into a sequence
    /// of topologically ordered segmented slices for processing.
    ///
    /// Returns a vector of slices, each containing its segments and boundary changesets.
    /// A boundary changeset is any changeset that is a parent of another changeset in
    /// another slice.
    ///
    /// Each slice consists of a sequence of topologically ordered segments, and every segment
    /// is represented using the changeset ids of its head and its base. Each slice is guaranteed
    /// to have size equal to `slice_size` except possibly the last one which may be smaller.
    pub async fn segmented_slice_ancestors(
        &self,
        ctx: &CoreContext,
        heads: Vec<ChangesetId>,
        common: Vec<ChangesetId>,
        slice_size: u64,
    ) -> Result<Vec<SegmentedSliceWithBoundaries>> {
        let (slices, _) = self
            .segmented_slice_ancestors_with_external_parents(ctx, heads, common, slice_size)
            .await?;
        Ok(slices)
    }

    /// Like `segmented_slice_ancestors`, but also returns the external parents:
    /// segment parents outside all segments (with `location: None`). These are
    /// parents in the `common` frontier that the caller assumed are derived.
    pub async fn segmented_slice_ancestors_with_external_parents(
        &self,
        ctx: &CoreContext,
        heads: Vec<ChangesetId>,
        common: Vec<ChangesetId>,
        slice_size: u64,
    ) -> Result<(Vec<SegmentedSliceWithBoundaries>, Vec<ChangesetId>)> {
        let segments = self
            .ancestors_difference_segments(ctx, heads, common)
            .await?;

        // Sort the segments in dfs order to try to minimize the number of boundary changesets.
        let segments = Self::dfs_order_segments(ctx, segments);

        // Go through the segments and try to add each to the current slice if the total
        // number of changesets in the slice wouldn't exceed `slice_size`. Otherwise,
        // split the current segment into two parts such that the first has `slice_size`
        // - `current_slice_size` changesets and the second has the rest, then add the first
        // part to the current slice and continue from the second part.

        let mut slices = vec![];
        let mut external_parents = HashSet::new();

        let mut current_segments = vec![];
        let mut current_slice_heads: BTreeMap<ChangesetId, u64> = Default::default();
        let mut current_slice_size = 0;
        let mut current_slice_boundaries: BoundaryChangesets = Default::default();

        for mut segment in segments {
            loop {
                // Current slice is full. Add it to the list of slices and create a new one.
                if current_slice_size == slice_size {
                    slices.push(SegmentedSliceWithBoundaries {
                        slice: SegmentedSliceDescription {
                            segments: std::mem::take(&mut current_segments),
                        },
                        boundaries: std::mem::take(&mut current_slice_boundaries),
                    });
                    current_slice_heads.clear();
                    current_slice_size = 0;
                }

                // Go through all parents of the current segment and check if they are
                // contained in another slice. If so, add them to boundary changesets.
                for parent in segment.parents {
                    if parent.location.is_none() {
                        external_parents.insert(parent.cs_id);
                    }
                    if let Some(location) = parent.location {
                        // Parent is part of another slice if its location is either relative
                        // to a head of a segment belonging to another slice or to a segment
                        // belonging to the current slice but the distance is greater than
                        // that segment length (which would mean that it belonged to the
                        // first part of a segment that was split)
                        match current_slice_heads.get(&location.head) {
                            Some(length) if location.distance < *length => {}
                            _ => {
                                current_slice_boundaries.insert(parent.cs_id);
                            }
                        }
                    }
                }

                if current_slice_size + segment.length <= slice_size {
                    // Adding the current segment wouldn't cause the current slice to exceed
                    // `slice_size`.
                    current_segments.push(SegmentDescription {
                        head: segment.head,
                        base: segment.base,
                    });
                    current_slice_heads.insert(segment.head, segment.length);
                    current_slice_size += segment.length;

                    break;
                } else {
                    // Split the current segment into two parts.
                    let split_cs_ids = self
                        .locations_to_changeset_ids(
                            ctx,
                            segment.head,
                            current_slice_size + segment.length - slice_size - 1,
                            2,
                        )
                        .await?;

                    let (split_base, split_head) = match split_cs_ids.as_slice() {
                        [split_base, split_head] => (*split_base, *split_head),
                        _ => {
                            bail!(
                                "Programmer error: split_cs_ids must have exactly two changeset ids"
                            )
                        }
                    };

                    // Add the first part to the current slice.
                    current_segments.push(SegmentDescription {
                        head: split_head,
                        base: segment.base,
                    });

                    // Continue loop using the second part of the segment.
                    segment = ChangesetSegment {
                        head: segment.head,
                        base: split_base,
                        length: current_slice_size + segment.length - slice_size,
                        parents: smallvec![],
                    };

                    // Current slice is full. Add it to the list of slices and create a new one.
                    slices.push(SegmentedSliceWithBoundaries {
                        slice: SegmentedSliceDescription {
                            segments: std::mem::take(&mut current_segments),
                        },
                        boundaries: std::mem::take(&mut current_slice_boundaries),
                    });
                    current_slice_heads.clear();
                    current_slice_size = 0;

                    // `split_head` is the parent of `split_base`, which is the base
                    // of the second part and now begins the *upcoming* slice. It is
                    // therefore a boundary of that upcoming slice, not the slice we
                    // just flushed. Because we clear the second part's `parents`
                    // above, the boundary-detection loop won't rediscover it, so we
                    // record it here -- after the flush -- so it lands in the
                    // upcoming slice's boundary set. Each slice's boundaries must
                    // contain the parent of its base commit; otherwise a backfill
                    // derive_slice would have no dependency on the request that
                    // derives that parent and could run before it is derived.
                    current_slice_boundaries.insert(split_head);
                }
            }
        }

        // Make sure to add the last slice to the list of slices.
        if current_slice_size > 0 {
            slices.push(SegmentedSliceWithBoundaries {
                slice: SegmentedSliceDescription {
                    segments: current_segments,
                },
                boundaries: current_slice_boundaries,
            });
        }

        Ok((slices, external_parents.into_iter().collect()))
    }

    /// Runs the given `process` closure on all of the given changesets in local topological
    /// order, running as many of them concurrently as possible.
    ///
    /// Note: local topological order here means that all parents of a changeset that are
    /// contained in the input `cs_ids` are processed before itself, but if two changesets
    /// are ancestors of each other and some of the changesets in the path between them are
    /// not given in `cs_ids`, they are not guaranteed to be processed in topological order.
    pub async fn process_topologically<Process, Fut>(
        &self,
        ctx: &CoreContext,
        cs_ids: Vec<ChangesetId>,
        process: Process,
    ) -> Result<()>
    where
        Process: Fn(ChangesetId) -> Fut,
        Fut: Future<Output = Result<()>> + Send,
    {
        // For each changeset in `cs_ids` this stores all other changesets that are in
        // `cs_ids` that are immediate children of it.
        let mut rdeps: HashMap<ChangesetId, HashSet<ChangesetId>> = Default::default();

        // For each changeset in `cs_ids` this stores the number of other changesets in
        // `cs_ids` that are immediate parents of it.
        let mut deps_count: HashMap<ChangesetId, usize> = Default::default();

        let all_edges = self
            .storage
            .fetch_many_edges(ctx, &cs_ids, Prefetch::None)
            .await?;

        let cs_ids = cs_ids.into_iter().collect::<HashSet<_>>();

        for (cs_id, edges) in all_edges.iter() {
            for parent in edges.parents::<E>() {
                if cs_ids.contains(&parent.cs_id) {
                    rdeps.entry(parent.cs_id).or_default().insert(*cs_id);
                    *deps_count.entry(*cs_id).or_default() += 1;
                }
            }
        }

        // futs contain a future produced by `process` for all changesets that have
        // no dependencies left. All executing concurrently.
        let mut futs: FuturesUnordered<_> = Default::default();

        for cs_id in cs_ids {
            if !deps_count.contains_key(&cs_id) {
                futs.push(process(cs_id).map_ok(move |()| cs_id).boxed());
            }
        }

        while let Some(result) = futs.next().await {
            let cs_id = result?;

            // After we finish process a changeset, we go through it's reverse dependencies
            // and subtract one from their dependency count. If the count reaches zero, we
            // add a new future to futs to begin processing it.
            let children = rdeps.get(&cs_id).into_iter().flatten();
            for child in children {
                let entry = deps_count.get_mut(child).ok_or_else(|| {
                    anyhow!("deps_count for a child can't be 0 (in process_topologically)")
                })?;
                *entry -= 1;
                if *entry == 0 {
                    futs.push(process(*child).map_ok(move |()| *child).boxed());
                }
            }
        }

        Ok(())
    }

    /// Returns the collection of changesets from the input changesets collection that do not have their parents in the input collection.
    pub async fn find_boundary(
        &self,
        ctx: &CoreContext,
        cs_ids: Vec<ChangesetId>,
    ) -> Result<Vec<ChangesetId>> {
        // Find all the parents of the input changesets.
        let changesets_with_parents = self
            .storage
            .fetch_many_edges(ctx, cs_ids.as_slice(), Prefetch::None)
            .await?
            .into_iter()
            .map(|(cs_id, edges)| {
                (
                    cs_id,
                    edges.edges().parents::<E>().cloned().collect::<Vec<_>>(),
                )
            })
            .collect::<HashMap<_, _>>();
        let boundary = cs_ids
            .into_iter()
            .filter(|cs_id| {
                // If a changeset has a parent that is not in the input collection, then its part of the boundary
                changesets_with_parents.get(cs_id).is_some_and(|parents| {
                    parents
                        .iter()
                        .any(|parent| !changesets_with_parents.contains_key(&parent.cs_id))
                })
            })
            .collect();
        Ok(boundary)
    }
}
