/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Write;

use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use commit_graph_types::edges::ChangesetEdges;
use commit_graph_types::edges::Parents;
use commit_graph_types::storage::CommitGraphStorage;
use commit_graph_types::storage::FetchedChangesetEdges;
use commit_graph_types::storage::Prefetch;
use context::CoreContext;
use mononoke_types::ChangesetId;
use mononoke_types::ChangesetIdPrefix;
use mononoke_types::ChangesetIdsResolvedFromPrefix;
use parking_lot::RwLock;
use repo_identity::ArcRepoIdentity;
use vec1::Vec1;

/// In-memory commit graph storage.
pub struct InMemoryCommitGraphStorage {
    repo_identity: ArcRepoIdentity,
    changesets: RwLock<BTreeMap<ChangesetId, ChangesetEdges>>,
    children: RwLock<BTreeMap<ChangesetId, HashSet<ChangesetId>>>,
}

impl InMemoryCommitGraphStorage {
    pub fn new(repo_identity: ArcRepoIdentity) -> Self {
        InMemoryCommitGraphStorage {
            repo_identity,
            changesets: Default::default(),
            children: Default::default(),
        }
    }

    pub fn drain(&self) -> Vec<ChangesetEdges> {
        let mut changesets = self.changesets.write();
        let many_edges = changesets.values().cloned().collect();
        changesets.clear();
        many_edges
    }

    pub fn len(&self) -> usize {
        self.changesets.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.changesets.read().is_empty()
    }
}

#[async_trait]
impl CommitGraphStorage for InMemoryCommitGraphStorage {
    fn repo_identity(&self) -> &ArcRepoIdentity {
        &self.repo_identity
    }

    async fn add(&self, _ctx: &CoreContext, edges: ChangesetEdges) -> Result<bool> {
        {
            let mut children = self.children.write();
            for parent in edges.parents::<Parents>() {
                children
                    .entry(parent.cs_id)
                    .or_default()
                    .insert(edges.node().cs_id);
            }
        }

        Ok(self
            .changesets
            .write()
            .insert(edges.node().cs_id, edges)
            .is_none())
    }

    async fn add_many(
        &self,
        _ctx: &CoreContext,
        many_edges: Vec1<ChangesetEdges>,
    ) -> Result<usize> {
        let mut added = 0;

        {
            let mut changesets = self.changesets.write();
            let mut children = self.children.write();
            for edges in many_edges {
                if changesets
                    .insert(edges.node().cs_id, edges.clone())
                    .is_none()
                {
                    added += 1;

                    for parent in edges.parents::<Parents>() {
                        children
                            .entry(parent.cs_id)
                            .or_default()
                            .insert(edges.node().cs_id);
                    }
                }
            }
        }

        Ok(added)
    }

    async fn fetch_edges(&self, ctx: &CoreContext, cs_id: ChangesetId) -> Result<ChangesetEdges> {
        self.maybe_fetch_edges(ctx, cs_id).await?.ok_or_else(|| {
            anyhow!("Missing changeset from in-memory commit graph storage: {cs_id}")
        })
    }

    async fn maybe_fetch_edges(
        &self,
        _ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<Option<ChangesetEdges>> {
        Ok(self.changesets.read().get(&cs_id).cloned())
    }

    async fn fetch_many_edges(
        &self,
        ctx: &CoreContext,
        cs_ids: &[ChangesetId],
        prefetch: Prefetch,
    ) -> Result<HashMap<ChangesetId, FetchedChangesetEdges>> {
        let edges = self.maybe_fetch_many_edges(ctx, cs_ids, prefetch).await?;
        let missing_changesets: Vec<_> = cs_ids
            .iter()
            .filter(|cs_id| !edges.contains_key(cs_id))
            .collect();

        if !missing_changesets.is_empty() {
            Err(anyhow!(
                "Missing changesets from in-memory commit graph storage: {}",
                missing_changesets
                    .into_iter()
                    .fold(String::new(), |mut acc, cs_id| {
                        let _ = write!(acc, "{cs_id}, ");
                        acc
                    })
            ))
        } else {
            Ok(edges)
        }
    }

    async fn maybe_fetch_many_edges(
        &self,
        _ctx: &CoreContext,
        cs_ids: &[ChangesetId],
        _prefetch: Prefetch,
    ) -> Result<HashMap<ChangesetId, FetchedChangesetEdges>> {
        let mut result = HashMap::with_capacity(cs_ids.len());
        let changesets = self.changesets.read();
        for cs_id in cs_ids {
            if let Some(edges) = changesets.get(cs_id) {
                result.insert(*cs_id, edges.clone().into());
            }
        }
        Ok(result)
    }

    async fn maybe_fetch_many_edges_from_replica(
        &self,
        ctx: &CoreContext,
        cs_ids: &[ChangesetId],
        prefetch: Prefetch,
    ) -> Result<HashMap<ChangesetId, FetchedChangesetEdges>> {
        self.maybe_fetch_many_edges(ctx, cs_ids, prefetch).await
    }

    async fn find_by_prefix(
        &self,
        _ctx: &CoreContext,
        cs_prefix: ChangesetIdPrefix,
        limit: usize,
    ) -> Result<ChangesetIdsResolvedFromPrefix> {
        let changesets = self.changesets.read();
        let (min, max) = (cs_prefix.min_bound(), cs_prefix.max_bound());
        let matches: Vec<_> = changesets
            .range(min..=max)
            .take(limit.saturating_add(1))
            .map(|(cs_id, _)| *cs_id)
            .collect();
        Ok(ChangesetIdsResolvedFromPrefix::from_vec_and_limit(
            matches, limit,
        ))
    }

    async fn fetch_children(
        &self,
        _ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<Vec<ChangesetId>> {
        Ok(self
            .children
            .read()
            .get(&cs_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect())
    }
}
