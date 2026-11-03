/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use commit_graph_types::edges::ChangesetEdges;
use commit_graph_types::storage::CommitGraphStorage;
use commit_graph_types::storage::FetchedChangesetEdges;
use commit_graph_types::storage::Prefetch;
use commit_graph_types::storage::PrefetchScope;
use context::CoreContext;
use mononoke_types::ChangesetId;
use mononoke_types::ChangesetIdPrefix;
use mononoke_types::ChangesetIdsResolvedFromPrefix;
use rand::seq::SliceRandom;
use repo_identity::ArcRepoIdentity;
use vec1::Vec1;

use crate::CommitGraphStorageTest;

/// A storage adapter that shuffles entries before adding them.
/// This is useful for testing that operations work correctly regardless of insertion order.
pub struct ShufflingCommitGraphStorage {
    inner: Arc<dyn CommitGraphStorage>,
}

impl ShufflingCommitGraphStorage {
    pub fn new(inner: Arc<dyn CommitGraphStorage>) -> Self {
        Self { inner }
    }
}

impl CommitGraphStorageTest for ShufflingCommitGraphStorage {}

#[async_trait]
impl CommitGraphStorage for ShufflingCommitGraphStorage {
    fn repo_identity(&self) -> &ArcRepoIdentity {
        self.inner.repo_identity()
    }

    async fn add(&self, ctx: &CoreContext, edges: ChangesetEdges) -> Result<bool> {
        self.inner.add(ctx, edges).await
    }

    async fn add_many(
        &self,
        ctx: &CoreContext,
        mut many_edges: Vec1<ChangesetEdges>,
    ) -> Result<usize> {
        many_edges.shuffle(&mut rand::rng());
        self.inner.add_many(ctx, many_edges).await
    }

    async fn fetch_edges(&self, ctx: &CoreContext, cs_id: ChangesetId) -> Result<ChangesetEdges> {
        self.inner.fetch_edges(ctx, cs_id).await
    }

    async fn maybe_fetch_edges(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<Option<ChangesetEdges>> {
        self.inner.maybe_fetch_edges(ctx, cs_id).await
    }

    async fn fetch_many_edges(
        &self,
        ctx: &CoreContext,
        cs_ids: &[ChangesetId],
        prefetch: Prefetch,
    ) -> Result<HashMap<ChangesetId, FetchedChangesetEdges>> {
        self.inner.fetch_many_edges(ctx, cs_ids, prefetch).await
    }

    async fn maybe_fetch_many_edges(
        &self,
        ctx: &CoreContext,
        cs_ids: &[ChangesetId],
        prefetch: Prefetch,
    ) -> Result<HashMap<ChangesetId, FetchedChangesetEdges>> {
        self.inner
            .maybe_fetch_many_edges(ctx, cs_ids, prefetch)
            .await
    }

    async fn prefetch_many_edges(
        &self,
        ctx: &CoreContext,
        cs_ids: &[ChangesetId],
        scope: PrefetchScope,
    ) -> Result<()> {
        self.inner.prefetch_many_edges(ctx, cs_ids, scope).await
    }

    async fn find_by_prefix(
        &self,
        ctx: &CoreContext,
        cs_prefix: ChangesetIdPrefix,
        limit: usize,
    ) -> Result<ChangesetIdsResolvedFromPrefix> {
        self.inner.find_by_prefix(ctx, cs_prefix, limit).await
    }

    async fn fetch_children(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<Vec<ChangesetId>> {
        self.inner.fetch_children(ctx, cs_id).await
    }
}
