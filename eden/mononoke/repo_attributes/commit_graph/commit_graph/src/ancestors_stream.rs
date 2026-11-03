/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use borrowed::borrowed;
use commit_graph_types::edges::EdgeType;
use commit_graph_types::frontier::ChangesetFrontier;
use commit_graph_types::storage::Prefetch;
use commit_graph_types::storage::PrefetchScope;
use commit_graph_types::storage::p1_linear_prefetch_steps;
use context::CoreContext;
use futures::Future;
use futures::future;
use futures::stream;
use futures::stream::BoxStream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use futures_ext::stream::FbStreamExt;
use mononoke_types::ChangesetId;
use mononoke_types::FIRST_GENERATION;
use mononoke_types::Generation;

use crate::CommitGraphOps;

/// Number of runs of head generations for which the common frontier is
/// lowered concurrently ahead of the stream.
const LOWER_COMMON_CONCURRENCY: usize = 32;

/// Maximum number of heads whose chains are prefetched, and of head
/// generations the common frontier is lowered to, ahead of the stream.
/// The stream deals with any beyond these as it reaches them.
const MAX_PREPARED_HEADS: usize = 1024;

/// Bound on the changesets held in total by the copies of the common
/// frontier kept for head generations, based on the size of the common
/// frontier.  Each copy also keeps every changeset the descent has passed
/// below its target.
const LOWER_COMMON_MAX_CHANGESETS: usize = 1 << 20;

/// Number of first-parent steps prefetched below every head ahead of the
/// stream.  Heads are usually short stacks of draft commits, so this covers
/// most of them at a fraction of the rows a full-length prefetch would
/// return, and longer chains fall back to the stream's own prefetch as it
/// walks them.
const HEAD_CHAIN_PREFETCH_STEPS: u64 = 32;

/// Builder for a reverse topologically ordered stream of changesets that
/// are ancestors of any set of changesets (heads). This builder allows customizing
/// the stream by:
///
/// - excluding ancestors of a set of changesets (common).
///
/// - excluding changesets that satisfy a given property (if this property holds
///   for one changeset then it has to hold for all its parents).
///
/// - including only changesets that satisfy a given property (if this property doesn't
///   hold for one changeset then it mustn't hold for any of its parents).
///
/// - including only changesets that are descendants of any one changeset.
pub struct AncestorsStreamBuilder<E: EdgeType> {
    commit_graph: Arc<CommitGraphOps<E>>,
    ctx: CoreContext,
    heads: Vec<ChangesetId>,
    common: Vec<ChangesetId>,
    descendants_of: Option<ChangesetId>,
    property: Box<
        dyn Fn(ChangesetId) -> Pin<Box<dyn Future<Output = Result<bool>> + Send>> + Send + Sync,
    >,
}

impl<E: EdgeType> AncestorsStreamBuilder<E> {
    pub fn new(
        commit_graph: Arc<CommitGraphOps<E>>,
        ctx: CoreContext,
        heads: Vec<ChangesetId>,
    ) -> Self {
        Self {
            commit_graph,
            ctx,
            heads,
            common: vec![],
            descendants_of: None,
            property: Box::new(|_| Box::pin(future::ready(Ok(true)))),
        }
    }

    pub fn exclude_ancestors_of(mut self, common: Vec<ChangesetId>) -> Self {
        self.common.extend(common);
        self
    }

    pub fn descendants_of(mut self, descendants_of: ChangesetId) -> Self {
        self.descendants_of = Some(descendants_of);
        self
    }

    pub fn with<Property, Out>(mut self, other_property: Property) -> Self
    where
        Property: Fn(ChangesetId) -> Out + Send + Sync + 'static,
        Out: Future<Output = Result<bool>> + Send + 'static,
    {
        self.property = Box::new(move |cs_id| {
            let fut_property = (self.property)(cs_id);
            let fut_other_property = other_property(cs_id);

            Box::pin(async move {
                if !fut_property.await? {
                    Ok(false)
                } else {
                    fut_other_property.await
                }
            })
        });
        self
    }

    pub fn without<Property, Out>(mut self, other_property: Property) -> Self
    where
        Property: Fn(ChangesetId) -> Out + Send + Sync + 'static,
        Out: Future<Output = Result<bool>> + Send + 'static,
    {
        self.property = Box::new(move |cs_id| {
            let fut_property = (self.property)(cs_id);
            let fut_other_property = other_property(cs_id);

            Box::pin(async move {
                if !fut_property.await? {
                    Ok(false)
                } else {
                    Ok(!fut_other_property.await?)
                }
            })
        });
        self
    }

    /// Lowers copies of the common frontier to the generation of every head
    /// ahead of the stream, so that it can swap them in when it reaches
    /// those generations instead of lowering the frontier there itself.
    /// Lowering there is where the stream otherwise misses the cache once
    /// or twice per head, one head after another.
    ///
    /// The head generations are split into contiguous runs.  One frontier is
    /// first lowered through the first generation of each run, since every
    /// run would otherwise repeat that descent, and the runs then lower their
    /// copy from generation to generation concurrently.  The first-parent
    /// chain below each lowered frontier is prefetched too, down to the next
    /// head generation, because the stream walks down it one generation at a
    /// time while it is inside a stack of commits.
    async fn lower_common_to_head_generations(
        commit_graph: &CommitGraphOps<E>,
        ctx: &CoreContext,
        heads: &ChangesetFrontier,
        common: &ChangesetFrontier,
    ) -> Result<HashMap<Generation, ChangesetFrontier>> {
        if common.is_empty() || heads.is_empty() {
            return Ok(HashMap::new());
        }

        // The stream lowers the frontier itself for any head generations
        // beyond the ones kept here.
        let common_size: usize = common.values().map(HashSet::len).sum();
        let max_generations =
            MAX_PREPARED_HEADS.min(LOWER_COMMON_MAX_CHANGESETS / common_size.max(1));
        let generations: Vec<Generation> =
            heads.keys().rev().take(max_generations).copied().collect();
        if generations.is_empty() {
            return Ok(HashMap::new());
        }
        let run_len = generations.len().div_ceil(LOWER_COMMON_CONCURRENCY);

        let mut frontier = common.clone();
        let mut runs = Vec::with_capacity(LOWER_COMMON_CONCURRENCY);
        for run in generations.chunks(run_len) {
            commit_graph
                .lower_frontier_incrementally(ctx, &mut frontier, run[0])
                .await?;
            runs.push((run.to_vec(), frontier.clone()));
        }

        stream::iter(runs)
            .map(|(run, mut frontier)| async move {
                let linear_steps = p1_linear_prefetch_steps();
                let mut lowered = Vec::with_capacity(run.len());
                for (index, generation) in run.iter().enumerate() {
                    if index > 0 {
                        commit_graph
                            .lower_frontier_incrementally(ctx, &mut frontier, *generation)
                            .await?;
                    }
                    // Lowering to a next generation that is within the linear
                    // prefetch distance prefetches the chain by itself.
                    let next = run.get(index + 1).copied();
                    let next_is_near =
                        next.is_some_and(|next| generation.value() - next.value() <= linear_steps);
                    if !next_is_near && let Some((_, cs_ids)) = frontier.last_key_value() {
                        let chain_start: Vec<_> = cs_ids.iter().copied().collect();
                        commit_graph
                            .storage
                            .prefetch_many_edges(
                                ctx,
                                &chain_start,
                                PrefetchScope::PastCached {
                                    generation: next.unwrap_or(FIRST_GENERATION),
                                    steps: linear_steps,
                                },
                            )
                            .await?;
                    }
                    lowered.push((*generation, frontier.clone()));
                }
                anyhow::Ok(lowered)
            })
            .buffer_unordered(LOWER_COMMON_CONCURRENCY)
            .try_concat()
            .await
            .map(|lowered| lowered.into_iter().collect())
    }

    pub async fn build(self) -> Result<BoxStream<'static, Result<ChangesetId>>> {
        struct AncestorsStreamState<E: EdgeType> {
            commit_graph: Arc<CommitGraphOps<E>>,
            ctx: CoreContext,
            heads: ChangesetFrontier,
            common: ChangesetFrontier,
            prefetch_common: bool,
            common_at_head_generations: HashMap<Generation, ChangesetFrontier>,
            descendants_of: Option<(ChangesetId, Generation)>,
            property: Box<
                dyn Fn(ChangesetId) -> Pin<Box<dyn Future<Output = Result<bool>> + Send>>
                    + Send
                    + Sync,
            >,
        }

        let heads = match self.descendants_of {
            Some(descendants_of) => {
                stream::iter(self.heads)
                    .map(anyhow::Ok)
                    .try_filter_map(|head| {
                        borrowed!(self.commit_graph: &CommitGraphOps<E>, self.ctx);
                        async move {
                            match commit_graph.is_ancestor(ctx, descendants_of, head).await? {
                                true => Ok(Some(head)),
                                false => Ok(None),
                            }
                        }
                    })
                    .try_collect()
                    .await?
            }
            None => self.heads,
        };

        let descendants_of = match self.descendants_of {
            Some(descendants_of) => Some((
                descendants_of,
                self.commit_graph
                    .changeset_generation(&self.ctx, descendants_of)
                    .await?,
            )),
            None => None,
        };

        let prefetch = justknobs::eval(
            "scm/mononoke:commit_graph_pull_optimizations",
            self.ctx
                .client_request_info()
                .map(|c| c.correlator.as_str()),
            Some(self.commit_graph.storage.repo_name()),
        );

        let mut head_ids = heads.clone();
        head_ids.truncate(MAX_PREPARED_HEADS);
        let (heads, common) = futures::try_join!(
            self.commit_graph.frontier(&self.ctx, heads),
            self.commit_graph.frontier(&self.ctx, self.common)
        )?;

        let common_at_head_generations = if prefetch {
            // The stream walks down the first-parent chains of all the heads,
            // so warm them all at once rather than one head at a time as the
            // traversal reaches each of them.
            let (_, common_at_head_generations) = futures::try_join!(
                self.commit_graph.storage.prefetch_many_edges(
                    &self.ctx,
                    &head_ids,
                    PrefetchScope::PastCached {
                        generation: FIRST_GENERATION,
                        steps: HEAD_CHAIN_PREFETCH_STEPS,
                    },
                ),
                Self::lower_common_to_head_generations(
                    &self.commit_graph,
                    &self.ctx,
                    &heads,
                    &common
                ),
            )?;
            common_at_head_generations
        } else {
            HashMap::new()
        };

        Ok(stream::try_unfold(
            Box::new(AncestorsStreamState {
                commit_graph: self.commit_graph,
                ctx: self.ctx,
                heads,
                common,
                prefetch_common: prefetch,
                common_at_head_generations,
                descendants_of,
                property: self.property,
            }),
            move |mut state| async move {
                let AncestorsStreamState {
                    commit_graph,
                    ctx,
                    heads,
                    common,
                    prefetch_common,
                    common_at_head_generations,
                    descendants_of,
                    property,
                } = &mut *state;

                if let Some((generation, cs_ids)) = heads.pop_last() {
                    if let Some(lowered) = common_at_head_generations.remove(&generation) {
                        *common = lowered;
                    } else if *prefetch_common {
                        commit_graph
                            .lower_frontier_incrementally(ctx, common, generation)
                            .await?;
                    } else {
                        commit_graph.lower_frontier(ctx, common, generation).await?;
                    }

                    let mut cs_ids_not_excluded = vec![];
                    for cs_id in cs_ids {
                        if !common.highest_generation_contains(cs_id, generation)
                            && property(cs_id).await?
                        {
                            cs_ids_not_excluded.push(cs_id)
                        }
                    }

                    let all_edges = commit_graph
                        .storage
                        .fetch_many_edges(
                            ctx,
                            &cs_ids_not_excluded,
                            Prefetch::for_p1_linear_traversal(),
                        )
                        .await?;

                    for (_cs_id, edges) in all_edges.into_iter() {
                        for parent in edges.parents::<E>() {
                            if let Some((descendants_of, descendants_of_gen)) = descendants_of {
                                // There is no need to query ancestry if the skip tree parent's generation number
                                // is greater than or equal to the generation number of descendants_of. This is
                                // because the skip tree parent is the common ancestor of all parents, and since
                                // the current changeset is a descendant of descendants_of, all of its parents
                                // will also be descendants of it.
                                if !edges
                                    .skip_tree_parent::<E>()
                                    .is_some_and(|skip_tree_parent| {
                                        skip_tree_parent.generation::<E>() >= *descendants_of_gen
                                    })
                                    && !commit_graph
                                        .is_ancestor(ctx, *descendants_of, parent.changeset_id())
                                        .await?
                                {
                                    continue;
                                }
                            }
                            heads
                                .entry(parent.generation::<E>())
                                .or_default()
                                .insert(parent.cs_id);
                        }
                    }

                    anyhow::Ok(Some((stream::iter(cs_ids_not_excluded).map(Ok), state)))
                } else {
                    Ok(None)
                }
            },
        )
        .try_flatten()
        .yield_periodically()
        .boxed())
    }
}
