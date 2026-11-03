/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Commit Graph Storage
//!
//! Trait for the storage back-end for the commit graph.

use std::collections::HashMap;
use std::ops::Deref;
use std::ops::DerefMut;

use anyhow::Result;
use async_trait::async_trait;
use context::CoreContext;
use mononoke_types::ChangesetId;
use mononoke_types::ChangesetIdPrefix;
use mononoke_types::ChangesetIdsResolvedFromPrefix;
use mononoke_types::FIRST_GENERATION;
use mononoke_types::Generation;
use mononoke_types::RepositoryId;
use repo_identity::ArcRepoIdentity;
use vec1::Vec1;

use crate::edges::ChangesetEdges;

/// Indication of what changesets to prefetch.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum PrefetchTarget {
    /// Prefetch a linear range of changesets by following the first parent
    LinearAncestors {
        /// Prefetch as far back as this generation.
        generation: Generation,

        /// Prefetch up to this many steps.
        steps: u64,
    },
    /// Prefetch along the skip tree following exactly what the skew binary
    /// algorithm would do to get to the target generation.
    ExactSkipTreeAncestors {
        /// Prefetch as far back as this generation.
        generation: Generation,
    },
}

/// What a prefetch warms the caches with.
#[derive(Copy, Clone, Debug)]
pub enum PrefetchScope {
    /// Fetch the changesets that aren't cached yet, with the prefetch a
    /// normal read towards `target` would do below each of them.  Cached
    /// changesets cost one cache lookup each and fill nothing, so the cost
    /// is linear in the changesets passed and this suits the whole set a
    /// traversal is about to read.
    UncachedOnly(PrefetchTarget),
    /// Walk each first-parent chain through the cache to the first uncached
    /// ancestor and prefetch `steps` changesets from there, going no further
    /// back than `generation`.  Each step is a cache lookup per changeset,
    /// so this suits a few changesets known to be cached, to warm the cache
    /// beyond them.
    PastCached {
        /// Prefetch as far back as this generation.
        generation: Generation,

        /// Prefetch up to this many steps.
        steps: u64,
    },
}

impl PrefetchScope {
    /// The target the prefetch asks the backing store for.
    pub fn target(self) -> PrefetchTarget {
        match self {
            PrefetchScope::UncachedOnly(target) => target,
            PrefetchScope::PastCached { generation, steps } => {
                PrefetchTarget::LinearAncestors { generation, steps }
            }
        }
    }
}

/// Indication for additional changesets to be fetched for subsequent
/// traversals.
///
/// If efficient to do so, implementers should use this hint to fetch
/// additional edges that will be useful for skew-binary traversal
/// to the target generation.
#[derive(Copy, Clone, Debug)]
pub enum Prefetch {
    /// No prefetch is required.
    None,

    /// Prefetch is permitted with the given hint, but additional items are
    /// not to be returned.
    Hint(PrefetchTarget),

    /// Prefetch if possible, and included prefetched items in the result.
    Include(PrefetchTarget),
}

/// Number of first-parent steps `Prefetch::for_p1_linear_traversal` asks
/// for.  This is arbitrary, but is a balance between not overfetching for
/// the cache and reducing the number of sequential steps.
pub const P1_LINEAR_PREFETCH_STEPS: u64 = 128;

/// Number of first-parent steps a linear prefetch covers: the steps asked
/// for, capped by the step limit that bounds every prefetching query.
/// Callers that rely on a prefetch having covered a distance must use this
/// rather than the constant.
pub fn p1_linear_prefetch_steps() -> u64 {
    P1_LINEAR_PREFETCH_STEPS.min(justknobs::get_as::<u64>(
        "scm/mononoke:commit_graph_prefetch_step_limit",
        None,
    ))
}

impl Prefetch {
    /// Prepare prefetching for skew-binary traversal over the skip tree.
    pub fn for_exact_skip_tree_traversal(generation: Generation) -> Self {
        Prefetch::Hint(PrefetchTarget::ExactSkipTreeAncestors { generation })
    }

    /// Prepare prefetching for linear traversal of the p1 history.
    pub fn for_p1_linear_traversal() -> Self {
        Prefetch::Hint(PrefetchTarget::LinearAncestors {
            generation: FIRST_GENERATION,
            steps: p1_linear_prefetch_steps(),
        })
    }

    pub fn is_hint(&self) -> bool {
        matches!(self, Prefetch::Hint(..))
    }

    pub fn is_include(&self) -> bool {
        matches!(self, Prefetch::Include(..))
    }

    /// Indicate that prefetching should be included if it has been hinted.
    ///
    /// This is called when the caching layer determines that it is able to
    /// store any prefetched values, and so values for any prefetch hint
    /// should be included.
    pub fn include_hint(self) -> Prefetch {
        match self {
            Prefetch::None => Prefetch::None,
            Prefetch::Hint(target) | Prefetch::Include(target) => Prefetch::Include(target),
        }
    }

    /// Target to prefetch to, if prefetching should be included.
    ///
    /// If prefetching is merely hinted, this won't return the target, as
    /// prefetching should not be performed.
    pub fn target(self) -> Option<PrefetchTarget> {
        match self {
            Prefetch::None | Prefetch::Hint(..) => None,
            Prefetch::Include(target) => Some(target),
        }
    }
}

/// Wrapper for `ChangesetEdges` indicating why it was fetched.
///
/// This is used to populate the memcache cache entries for prefetches.  In
/// the memcache cache, we store prefetched edges alongside the changeset that
/// they were fetched for, so that memcache hits also benefit from caching the
/// prefetch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FetchedChangesetEdges {
    /// These edges were fetched directly.
    ///
    /// When cached, they will be stored in cachelib and memcache by the key
    /// of the target changeset.
    Fetched { edges: ChangesetEdges },
    /// These edges were prefetched.
    ///
    /// When cached, they will be stored in cachelib directly, but in memcache
    /// they will be stored alongside the edges for the target they were
    /// originally prefetched for.
    Prefetched {
        edges: ChangesetEdges,

        /// The changeset these edges were prefetched for.  These edges will
        /// be stored alongside the edges for this changeset in memcache.
        cs_id: ChangesetId,
    },
}

impl Deref for FetchedChangesetEdges {
    type Target = ChangesetEdges;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Fetched { edges } | Self::Prefetched { edges, .. } => edges,
        }
    }
}

impl DerefMut for FetchedChangesetEdges {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Fetched { edges } | Self::Prefetched { edges, .. } => edges,
        }
    }
}

impl From<ChangesetEdges> for FetchedChangesetEdges {
    fn from(edges: ChangesetEdges) -> Self {
        Self::Fetched { edges }
    }
}

impl From<FetchedChangesetEdges> for ChangesetEdges {
    fn from(fetched: FetchedChangesetEdges) -> Self {
        fetched.into_edges()
    }
}

impl FetchedChangesetEdges {
    pub fn new(prefetch_target_cs_id: Option<ChangesetId>, edges: ChangesetEdges) -> Self {
        match prefetch_target_cs_id {
            Some(cs_id) if cs_id != edges.node().cs_id => Self::Prefetched { cs_id, edges },
            _ => Self::Fetched { edges },
        }
    }

    pub fn edges(&self) -> &ChangesetEdges {
        match self {
            FetchedChangesetEdges::Fetched { edges }
            | FetchedChangesetEdges::Prefetched { edges, .. } => edges,
        }
    }

    pub fn into_edges(self) -> ChangesetEdges {
        match self {
            FetchedChangesetEdges::Fetched { edges }
            | FetchedChangesetEdges::Prefetched { edges, .. } => edges,
        }
    }

    pub fn prefetched_for(&self) -> Option<ChangesetId> {
        match self {
            Self::Fetched { .. } => None,
            Self::Prefetched { cs_id, .. } => Some(*cs_id),
        }
    }
}

/// Commit Graph Storage.
#[async_trait]
pub trait CommitGraphStorage: Send + Sync {
    /// The repository identity (id and name).
    fn repo_identity(&self) -> &ArcRepoIdentity;

    /// The repository this commit graph storage is for.
    ///
    /// Convenience method - delegates to repo_identity().id()
    fn repo_id(&self) -> RepositoryId {
        self.repo_identity().id()
    }

    /// The repository name this commit graph storage is for.
    ///
    /// Convenience method - delegates to repo_identity().name()
    fn repo_name(&self) -> &str {
        self.repo_identity().name()
    }

    /// Add a new changeset to the commit graph.
    ///
    /// Returns true if a new changeset was inserted, or false if the
    /// changeset already existed.
    async fn add(&self, ctx: &CoreContext, edges: ChangesetEdges) -> Result<bool>;

    /// Add many changesets at once. Used for low level stuff like backfilling.
    async fn add_many(&self, ctx: &CoreContext, many_edges: Vec1<ChangesetEdges>) -> Result<usize>;

    /// Returns the changeset graph edges for this changeset, or an error if
    /// this changeset is missing from the commit graph.
    async fn fetch_edges(&self, ctx: &CoreContext, cs_id: ChangesetId) -> Result<ChangesetEdges>;

    /// Returns the changeset graph edges for this changeset, or None if
    /// it doesn't exist in the commit graph.
    async fn maybe_fetch_edges(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<Option<ChangesetEdges>>;

    /// Returns the changeset graph edges for multiple changesets, or an error
    /// if any of the changesets are missing from the commit graph.
    ///
    /// Prefetch indicates that this request is part of a larger request
    /// involving commits down to a particular generation number, and so
    /// prefetching more nodes into any internal caches would be beneficial.
    async fn fetch_many_edges(
        &self,
        ctx: &CoreContext,
        cs_ids: &[ChangesetId],
        prefetch: Prefetch,
    ) -> Result<HashMap<ChangesetId, FetchedChangesetEdges>>;

    /// Same as fetch_many_edges but doesn't return an error if any of
    /// the changesets are missing from the commit graph and instead
    /// only returns edges for found changesets.
    async fn maybe_fetch_many_edges(
        &self,
        ctx: &CoreContext,
        cs_ids: &[ChangesetId],
        prefetch: Prefetch,
    ) -> Result<HashMap<ChangesetId, FetchedChangesetEdges>>;

    /// Warm any caches this storage has with edges below `cs_ids`, as far as
    /// `scope` says.  Storages without caches do nothing.
    async fn prefetch_many_edges(
        &self,
        _ctx: &CoreContext,
        _cs_ids: &[ChangesetId],
        _scope: PrefetchScope,
    ) -> Result<()> {
        Ok(())
    }

    /// Find all changeset ids with a given prefix.
    async fn find_by_prefix(
        &self,
        _ctx: &CoreContext,
        _cs_prefix: ChangesetIdPrefix,
        _limit: usize,
    ) -> Result<ChangesetIdsResolvedFromPrefix>;

    // Fetch all children of a changeset.
    async fn fetch_children(
        &self,
        _ctx: &CoreContext,
        _cs_id: ChangesetId,
    ) -> Result<Vec<ChangesetId>>;
}
