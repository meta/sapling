/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use commit_graph_types::edges::ChangesetEdges;
use commit_graph_types::storage::CommitGraphStorage;
use commit_graph_types::storage::FetchedChangesetEdges;
use commit_graph_types::storage::Prefetch;
use commit_graph_types::storage::PrefetchScope;
use context::CoreContext;
use in_memory_commit_graph_storage::InMemoryCommitGraphStorage;
use mononoke_types::ChangesetId;
use mononoke_types::ChangesetIdPrefix;
use mononoke_types::ChangesetIdsResolvedFromPrefix;
use repo_identity::ArcRepoIdentity;
use vec1::Vec1;

#[cfg(test)]
mod tests;

pub struct BufferedCommitGraphStorage {
    in_memory_storage: InMemoryCommitGraphStorage,
    persistent_storage: Arc<dyn CommitGraphStorage>,
    /// The maximum number of changeset edges
    /// allowed to be stored in memory.
    max_in_memory_size: usize,
}

impl BufferedCommitGraphStorage {
    pub fn new(persistent_storage: Arc<dyn CommitGraphStorage>, max_in_memory_size: usize) -> Self {
        Self {
            in_memory_storage: InMemoryCommitGraphStorage::new(
                persistent_storage.repo_identity().clone(),
            ),
            persistent_storage,
            max_in_memory_size,
        }
    }

    /// Flushes all changeset edges from the in memory storage
    /// to the persistent storage. Returns the number of added
    /// edges to the persistent storage.
    pub async fn flush(&self, ctx: &CoreContext) -> Result<usize> {
        match Vec1::try_from_vec(self.in_memory_storage.drain()) {
            Ok(many_edges) => self.persistent_storage.add_many(ctx, many_edges).await,
            _ => Ok(0),
        }
    }
}

#[async_trait]
impl CommitGraphStorage for BufferedCommitGraphStorage {
    fn repo_identity(&self) -> &ArcRepoIdentity {
        self.persistent_storage.repo_identity()
    }

    async fn add(&self, ctx: &CoreContext, edges: ChangesetEdges) -> Result<bool> {
        if self.in_memory_storage.len() + 1 > self.max_in_memory_size {
            self.flush(ctx).await?;
        }
        self.in_memory_storage.add(ctx, edges).await
    }

    async fn add_many(&self, ctx: &CoreContext, many_edges: Vec1<ChangesetEdges>) -> Result<usize> {
        if self.in_memory_storage.len() + many_edges.len() > self.max_in_memory_size {
            self.flush(ctx).await?;
        }
        self.in_memory_storage.add_many(ctx, many_edges).await
    }

    async fn fetch_edges(&self, ctx: &CoreContext, cs_id: ChangesetId) -> Result<ChangesetEdges> {
        match self.in_memory_storage.maybe_fetch_edges(ctx, cs_id).await? {
            Some(edges) => Ok(edges),
            None => self.persistent_storage.fetch_edges(ctx, cs_id).await,
        }
    }

    async fn maybe_fetch_edges(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<Option<ChangesetEdges>> {
        match self.in_memory_storage.maybe_fetch_edges(ctx, cs_id).await? {
            Some(edges) => Ok(Some(edges)),
            None => self.persistent_storage.maybe_fetch_edges(ctx, cs_id).await,
        }
    }

    async fn fetch_many_edges(
        &self,
        ctx: &CoreContext,
        cs_ids: &[ChangesetId],
        prefetch: Prefetch,
    ) -> Result<HashMap<ChangesetId, FetchedChangesetEdges>> {
        let mut fetched_edges = self
            .in_memory_storage
            .maybe_fetch_many_edges(ctx, cs_ids, prefetch)
            .await?;

        let unfetched_ids = cs_ids
            .iter()
            .filter(|cs_id| !fetched_edges.contains_key(cs_id))
            .copied()
            .collect::<Vec<_>>();

        if !unfetched_ids.is_empty() {
            fetched_edges.extend(
                self.persistent_storage
                    .fetch_many_edges(ctx, unfetched_ids.as_slice(), prefetch)
                    .await?,
            )
        }

        Ok(fetched_edges)
    }

    async fn maybe_fetch_many_edges(
        &self,
        ctx: &CoreContext,
        cs_ids: &[ChangesetId],
        prefetch: Prefetch,
    ) -> Result<HashMap<ChangesetId, FetchedChangesetEdges>> {
        let mut fetched_edges = self
            .in_memory_storage
            .maybe_fetch_many_edges(ctx, cs_ids, prefetch)
            .await?;

        let unfetched_ids = cs_ids
            .iter()
            .filter(|cs_id| !fetched_edges.contains_key(cs_id))
            .copied()
            .collect::<Vec<_>>();

        if !unfetched_ids.is_empty() {
            fetched_edges.extend(
                self.persistent_storage
                    .maybe_fetch_many_edges(ctx, unfetched_ids.as_slice(), prefetch)
                    .await?,
            )
        }

        Ok(fetched_edges)
    }

    async fn prefetch_many_edges(
        &self,
        ctx: &CoreContext,
        cs_ids: &[ChangesetId],
        scope: PrefetchScope,
    ) -> Result<()> {
        self.persistent_storage
            .prefetch_many_edges(ctx, cs_ids, scope)
            .await
    }

    async fn find_by_prefix(
        &self,
        ctx: &CoreContext,
        cs_prefix: ChangesetIdPrefix,
        limit: usize,
    ) -> Result<ChangesetIdsResolvedFromPrefix> {
        match futures::try_join!(
            self.in_memory_storage.find_by_prefix(ctx, cs_prefix, limit),
            self.persistent_storage
                .find_by_prefix(ctx, cs_prefix, limit)
        )? {
            (in_memory_matches @ ChangesetIdsResolvedFromPrefix::TooMany(_), _) => {
                Ok(in_memory_matches)
            }
            (_, persistent_matches @ ChangesetIdsResolvedFromPrefix::TooMany(_)) => {
                Ok(persistent_matches)
            }
            (in_memory_matches, persistent_matches) => {
                Ok(ChangesetIdsResolvedFromPrefix::from_vec_and_limit(
                    in_memory_matches
                        .to_vec()
                        .into_iter()
                        .chain(persistent_matches.to_vec())
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect(),
                    limit,
                ))
            }
        }
    }

    async fn fetch_children(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<Vec<ChangesetId>> {
        Ok(self
            .in_memory_storage
            .fetch_children(ctx, cs_id)
            .await?
            .into_iter()
            .chain(self.persistent_storage.fetch_children(ctx, cs_id).await?)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect())
    }
}
