/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use blobstore::Loadable;
use commit_graph::BaseCommitGraphWriter;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphWriter;
use commit_graph::ParentsFetcher;
use commit_graph_types::edges::ChangesetEdges;
use commit_graph_types::edges::Parents;
use commit_graph_types::storage::CommitGraphStorage;
use commit_graph_types::storage::FetchedChangesetEdges;
use commit_graph_types::storage::Prefetch;
use commit_graph_types::storage::PrefetchScope;
use context::CoreContext;
use memwrites_commit_graph_storage::MemWritesCommitGraphStorage;
use mononoke_types::ChangesetId;
use mononoke_types::ChangesetIdPrefix;
use mononoke_types::ChangesetIdsResolvedFromPrefix;
use mononoke_types::RepositoryId;
use repo_blobstore::RepoBlobstore;
use smallvec::ToSmallVec;
use sql_ext::SqlConnections;
use sql_ext::mononoke_queries;
use vec1::Vec1;
use vec1::vec1;

use crate::bubble::BubbleId;

mononoke_queries! {
    read SelectChangesets(
        repo_id: RepositoryId,
        bubble_id: BubbleId,
        >list cs_id: ChangesetId
    ) -> (ChangesetId) {
        "SELECT cs_id
          FROM ephemeral_bubble_changeset_mapping
          WHERE repo_id = {repo_id} AND bubble_id = {bubble_id} AND cs_id IN {cs_id}"
    }

    read SelectChangesetsInRange(repo_id: RepositoryId, min_id: ChangesetId, max_id: ChangesetId, limit: usize) -> (ChangesetId) {
        "
        SELECT cs_id
        FROM ephemeral_bubble_changeset_mapping
        WHERE repo_id = {repo_id} AND {min_id} <= cs_id AND cs_id <= {max_id}
        ORDER BY cs_id ASC
        LIMIT {limit}
        "
    }

    write InsertChangeset(
        values: (repo_id: RepositoryId, cs_id: ChangesetId, bubble_id: BubbleId, r#gen: u64)
    ) {
        insert_or_ignore,
        "{insert_or_ignore} INTO ephemeral_bubble_changeset_mapping
         (repo_id, cs_id, bubble_id, gen)
         VALUES {values}"
    }

    write InsertChangesetParents(
        values: (
            repo_id: RepositoryId,
            bubble_id: BubbleId,
            cs_id: ChangesetId,
            parent_index: u32,
            parent_cs_id: ChangesetId,
        )
    ) {
        insert_or_ignore,
        "{insert_or_ignore} INTO ephemeral_bubble_changeset_parents
         (repo_id, bubble_id, cs_id, parent_index, parent_cs_id)
         VALUES {values}"
    }

    read SelectChangesetParents(
        repo_id: RepositoryId,
        bubble_id: BubbleId,
        cs_id: ChangesetId,
    ) -> (u32, ChangesetId) {
        "SELECT parent_index, parent_cs_id
         FROM ephemeral_bubble_changeset_parents
         WHERE repo_id = {repo_id} AND bubble_id = {bubble_id} AND cs_id = {cs_id}
         ORDER BY parent_index ASC"
    }
}

/// A commit graph storage that allows fetching snapshot changesets, as well
/// as persistent changesets.
/// Since initially there will be a single snapshot per bubble, there's no
/// need to optimise anything on this struct. As the need arises, we can tweak
/// this, for example by having an extra table that stores parent information
/// to avoid looking at the blobstore.
#[derive(Clone)]
pub struct EphemeralCommitGraphStorage {
    /// Storage containing only the ephemeral bubble changesets.
    ephemeral_only_storage: Arc<EphemeralOnlyChangesetStorage>,
    /// A storage that wraps the persistent commit graph storage and writes
    /// new changesets to memory.
    mem_writes_storage: Arc<MemWritesCommitGraphStorage>,
    /// Another view of the MemWrites storage to allow traversing the commit graph
    /// (through CommitGraphWriter::add_recursive) to create ChangesetEdges.
    mem_writes_commit_graph_writer: BaseCommitGraphWriter,
}

/// A storage that allows fetching snapshot changesets.
#[derive(Clone)]
pub struct EphemeralOnlyChangesetStorage {
    repo_id: RepositoryId,
    bubble_id: BubbleId,
    repo_blobstore: RepoBlobstore,
    connections: SqlConnections,
}

impl EphemeralCommitGraphStorage {
    pub(crate) fn new(
        repo_id: RepositoryId,
        bubble_id: BubbleId,
        repo_blobstore: RepoBlobstore,
        connections: SqlConnections,
        persistent_storage: Arc<dyn CommitGraphStorage>,
    ) -> Self {
        let mem_writes_storage = Arc::new(MemWritesCommitGraphStorage::new(persistent_storage));
        Self {
            ephemeral_only_storage: Arc::new(EphemeralOnlyChangesetStorage::new(
                repo_id,
                bubble_id,
                repo_blobstore,
                connections,
            )),
            mem_writes_storage: mem_writes_storage.clone(),
            mem_writes_commit_graph_writer: BaseCommitGraphWriter::new(CommitGraph::new(
                mem_writes_storage,
            )),
        }
    }
}

impl EphemeralOnlyChangesetStorage {
    pub(crate) fn new(
        repo_id: RepositoryId,
        bubble_id: BubbleId,
        repo_blobstore: RepoBlobstore,
        connections: SqlConnections,
    ) -> Self {
        Self {
            repo_id,
            bubble_id,
            repo_blobstore,
            connections,
        }
    }

    async fn add(&self, ctx: &CoreContext, edges: &ChangesetEdges) -> Result<bool> {
        let cs_id = edges.node().cs_id;
        let parent_data: Vec<(u32, ChangesetId)> = edges
            .parents::<Parents>()
            .enumerate()
            .map(|(idx, n)| (idx as u32, n.cs_id))
            .collect();

        let txn = self
            .connections
            .write_connection
            .start_transaction(ctx.sql_query_telemetry())
            .await?;

        let (txn, result) = InsertChangeset::query_with_transaction(
            txn,
            &[(
                &self.repo_id,
                &cs_id,
                &self.bubble_id,
                &edges.node().generation::<Parents>().value(),
            )],
        )
        .await?;

        let txn = if parent_data.is_empty() {
            txn
        } else {
            let parent_rows: Vec<_> = parent_data
                .iter()
                .map(|(idx, parent_cs_id)| {
                    (&self.repo_id, &self.bubble_id, &cs_id, idx, parent_cs_id)
                })
                .collect();
            let (txn, _) =
                InsertChangesetParents::query_with_transaction(txn, parent_rows.as_slice()).await?;
            txn
        };

        txn.commit().await?;

        Ok(result.last_insert_id().is_some())
    }

    pub async fn known_changesets(
        &self,
        ctx: &CoreContext,
        cs_ids: Vec<ChangesetId>,
    ) -> Result<Vec<ChangesetId>> {
        let found_cs_ids = SelectChangesets::query(
            &self.connections.read_connection,
            ctx.sql_query_telemetry(),
            &self.repo_id,
            &self.bubble_id,
            &cs_ids,
        )
        .await?
        .into_iter()
        .map(|(cs_id,)| cs_id)
        .collect::<HashSet<_>>();

        let (mut known_ids, missing_ids): (Vec<_>, Vec<_>) = cs_ids
            .into_iter()
            .partition(|cs_id| found_cs_ids.contains(cs_id));

        if !missing_ids.is_empty() {
            let found_in_master = SelectChangesets::query(
                &self.connections.read_master_connection,
                ctx.sql_query_telemetry(),
                &self.repo_id,
                &self.bubble_id,
                &missing_ids,
            )
            .await?
            .into_iter()
            .map(|(cs_id,)| cs_id);

            known_ids.extend(found_in_master);
        }

        Ok(known_ids)
    }

    async fn find_by_prefix(
        &self,
        ctx: &CoreContext,
        cs_prefix: ChangesetIdPrefix,
        limit: usize,
    ) -> Result<ChangesetIdsResolvedFromPrefix> {
        let fetched_ids = SelectChangesetsInRange::query(
            &self.connections.read_connection,
            ctx.sql_query_telemetry(),
            &self.repo_id,
            &cs_prefix.min_bound(),
            &cs_prefix.max_bound(),
            &(limit + 1),
        )
        .await?
        .into_iter()
        .map(|(cs_id,)| cs_id)
        .collect::<Vec<_>>();

        Ok(ChangesetIdsResolvedFromPrefix::from_vec_and_limit(
            fetched_ids,
            limit,
        ))
    }
}

#[async_trait]
impl ParentsFetcher for EphemeralOnlyChangesetStorage {
    async fn fetch_parents(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<Vec<ChangesetId>> {
        // Fast path: parent edges are stored in SQL for any bubble changeset
        // written after the parents table was introduced. This avoids loading
        // the bonsai blob and mirrors how the production commit graph works.
        let rows = SelectChangesetParents::query(
            &self.connections.read_connection,
            ctx.sql_query_telemetry(),
            &self.repo_id,
            &self.bubble_id,
            &cs_id,
        )
        .await?;
        if !rows.is_empty() {
            return Ok(rows
                .into_iter()
                .map(|(_idx, parent_cs_id)| parent_cs_id)
                .collect());
        }
        // Transition fallback for bubbles created before the parents table
        // existed. Removed once such bubbles have aged out.
        let cs = cs_id
            .load(ctx, &self.repo_blobstore)
            .await
            .with_context(|| {
                format!(
                    "Failed to load bonsai changeset with id {cs_id} (in EphemeralCommitGraphStorage)"
                )
            })?;
        Ok(cs.parents().collect())
    }

    async fn fetch_subtree_sources(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<Vec<ChangesetId>> {
        let cs = cs_id.load(ctx, &self.repo_blobstore).await?;
        let parents = cs.parents().collect::<HashSet<_>>();
        Ok(cs
            .subtree_changes()
            .values()
            .filter_map(|change| {
                change
                    .change_source()
                    .and_then(|(cs_id, _)| (!parents.contains(&cs_id)).then_some(cs_id))
            })
            .collect())
    }
}

#[async_trait]
impl CommitGraphStorage for EphemeralCommitGraphStorage {
    fn repo_identity(&self) -> &repo_identity::ArcRepoIdentity {
        self.mem_writes_storage.repo_identity()
    }

    async fn add(&self, ctx: &CoreContext, edges: ChangesetEdges) -> Result<bool> {
        let modified = self.ephemeral_only_storage.add(ctx, &edges).await?;
        self.mem_writes_commit_graph_writer
            .add_recursive(
                ctx,
                self.ephemeral_only_storage.clone(),
                vec1![(
                    edges.node().cs_id,
                    edges.parents::<Parents>().map(|node| node.cs_id).collect(),
                    edges.subtree_sources().map(|node| node.cs_id).collect(),
                )],
            )
            .await?;

        Ok(modified)
    }

    async fn add_many(&self, ctx: &CoreContext, many_edges: Vec1<ChangesetEdges>) -> Result<usize> {
        let mut modified = 0;
        for edges in many_edges {
            modified += self.add(ctx, edges).await? as usize;
        }
        Ok(modified)
    }

    async fn fetch_edges(&self, ctx: &CoreContext, cs_id: ChangesetId) -> Result<ChangesetEdges> {
        Ok(self
            .fetch_many_edges(ctx, &[cs_id], Prefetch::None)
            .await?
            .remove(&cs_id)
            .ok_or_else(|| anyhow!("Missing changeset {cs_id} in ephemeral commit graph storage"))?
            .into())
    }

    async fn maybe_fetch_edges(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<Option<ChangesetEdges>> {
        Ok(self
            .fetch_many_edges(ctx, &[cs_id], Prefetch::None)
            .await?
            .remove(&cs_id)
            .map(|edges| edges.into()))
    }

    async fn fetch_many_edges(
        &self,
        ctx: &CoreContext,
        cs_ids: &[ChangesetId],
        prefetch: Prefetch,
    ) -> Result<HashMap<ChangesetId, FetchedChangesetEdges>> {
        let fetched_edges = self.maybe_fetch_many_edges(ctx, cs_ids, prefetch).await?;

        let unfetched_ids = cs_ids
            .iter()
            .filter(|cs_id| !fetched_edges.contains_key(cs_id))
            .copied()
            .collect::<Vec<_>>();

        if let Some(cs_id) = unfetched_ids.first() {
            return Err(anyhow::anyhow!(
                "Changeset {cs_id} not found in ephemeral commit graph storage",
            ));
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
            .mem_writes_storage
            .maybe_fetch_many_edges(ctx, cs_ids, prefetch)
            .await?;

        let unfetched_ids = cs_ids
            .iter()
            .filter(|cs_id| !fetched_edges.contains_key(cs_id))
            .copied()
            .collect::<Vec<_>>();

        if !unfetched_ids.is_empty() {
            let known_ids = self
                .ephemeral_only_storage
                .known_changesets(ctx, unfetched_ids)
                .await?;

            for cs_id in known_ids {
                let parents = self
                    .ephemeral_only_storage
                    .fetch_parents(ctx, cs_id)
                    .await?;
                let subtree_sources = self
                    .ephemeral_only_storage
                    .fetch_subtree_sources(ctx, cs_id)
                    .await?;
                self.mem_writes_commit_graph_writer
                    .add_recursive(
                        ctx,
                        self.ephemeral_only_storage.clone(),
                        vec1![(cs_id, parents.to_smallvec(), subtree_sources)],
                    )
                    .await?;
                fetched_edges.insert(
                    cs_id,
                    self.mem_writes_storage
                        .fetch_edges(ctx, cs_id)
                        .await?
                        .into(),
                );
            }
        }

        Ok(fetched_edges)
    }

    async fn prefetch_many_edges(
        &self,
        ctx: &CoreContext,
        cs_ids: &[ChangesetId],
        scope: PrefetchScope,
    ) -> Result<()> {
        self.mem_writes_storage
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
            self.ephemeral_only_storage
                .find_by_prefix(ctx, cs_prefix, limit),
            self.mem_writes_storage
                .find_by_prefix(ctx, cs_prefix, limit)
        )? {
            (ephemeral_only_matches @ ChangesetIdsResolvedFromPrefix::TooMany(_), _) => {
                Ok(ephemeral_only_matches)
            }
            (_, mem_writes_matches @ ChangesetIdsResolvedFromPrefix::TooMany(_)) => {
                Ok(mem_writes_matches)
            }
            (ephemeral_only_matches, mem_writes_matches) => {
                Ok(ChangesetIdsResolvedFromPrefix::from_vec_and_limit(
                    ephemeral_only_matches
                        .to_vec()
                        .into_iter()
                        .chain(mem_writes_matches.to_vec())
                        .collect::<HashSet<_>>()
                        .into_iter()
                        .collect(),
                    limit,
                ))
            }
        }
    }

    async fn fetch_children(
        &self,
        _ctx: &CoreContext,
        _cs_id: ChangesetId,
    ) -> Result<Vec<ChangesetId>> {
        // The parents SQL table makes a fetch_children implementation possible
        // in principle, but a bubble that predates the table would silently
        // return an empty child set instead of failing, which could mislead
        // callers. Fail loudly until we are confident the table is populated
        // for all live bubbles.
        Err(anyhow!(
            "fetch_children is not supported on ephemeral commit graph storage"
        ))
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;
    use std::time::Duration;

    use blobstore::BlobstoreEnumerableWithUnlink;
    use commit_graph_types::edges::ChangesetEdgesMut;
    use commit_graph_types::edges::ChangesetNode;
    use commit_graph_types::edges::ChangesetNodeParents;
    use context::CoreContext;
    use fbinit::FacebookInit;
    use memblob::Memblob;
    use metaconfig_types::BubbleDeletionMode;
    use mononoke_macros::mononoke;
    use mononoke_types::Generation;
    use mononoke_types_mocks::changesetid::FIVES_CSID;
    use mononoke_types_mocks::changesetid::ONES_CSID;
    use mononoke_types_mocks::changesetid::THREES_CSID;
    use mononoke_types_mocks::changesetid::TWOS_CSID;
    use mononoke_types_mocks::repo::REPO_ZERO;
    use repo_blobstore::RepoBlobstore;
    use scuba_ext::MononokeScubaSampleBuilder;
    use sql_construct::SqlConstruct;

    use super::*;
    use crate::Bubble;
    use crate::RepoEphemeralStore;
    use crate::builder::RepoEphemeralStoreBuilder;

    fn bootstrap(fb: FacebookInit) -> Result<(CoreContext, RepoBlobstore, RepoEphemeralStore)> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = Arc::new(Memblob::default()) as Arc<dyn BlobstoreEnumerableWithUnlink>;
        let repo_blobstore = RepoBlobstore::new(
            Arc::new(Memblob::default()),
            None,
            REPO_ZERO,
            MononokeScubaSampleBuilder::with_discard(),
        );
        let eph = RepoEphemeralStoreBuilder::with_sqlite_in_memory()?.build(
            REPO_ZERO,
            blobstore,
            Duration::from_secs(30 * 24 * 3600),
            Duration::from_secs(6 * 3600),
            BubbleDeletionMode::MarkAndDelete,
        );
        Ok((ctx, repo_blobstore, eph))
    }

    fn ephemeral_only_storage(
        bubble: &Bubble,
        repo_blobstore: RepoBlobstore,
    ) -> EphemeralOnlyChangesetStorage {
        EphemeralOnlyChangesetStorage::new(
            REPO_ZERO,
            bubble.bubble_id(),
            repo_blobstore,
            bubble.sql_connections().clone(),
        )
    }

    fn edges_with_parents(
        cs_id: ChangesetId,
        generation: u64,
        parents: Vec<ChangesetId>,
    ) -> ChangesetEdges {
        let mk_node = |cs_id, r#gen| {
            ChangesetNode::new(
                cs_id,
                Generation::new(r#gen),
                Generation::new(r#gen),
                0,
                0,
                0,
            )
        };
        let parent_nodes: ChangesetNodeParents = parents
            .into_iter()
            .map(|p| mk_node(p, generation.saturating_sub(1)))
            .collect();
        ChangesetEdgesMut {
            node: mk_node(cs_id, generation),
            parents: parent_nodes,
            subtree_sources: Vec::new(),
            merge_ancestor_or_root: None,
            skip_tree_parent: None,
            skip_tree_skew_ancestor: None,
            p1_linear_skew_ancestor: None,
            subtree_or_merge_ancestor: None,
            subtree_source_parent: None,
            subtree_source_skew_ancestor: None,
        }
        .freeze()
    }

    #[mononoke::fbinit_test]
    async fn fetch_parents_from_sql_for_multi_commit_bubble(fb: FacebookInit) -> Result<()> {
        let (ctx, repo_blobstore, eph) = bootstrap(fb)?;
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        let storage = ephemeral_only_storage(&bubble, repo_blobstore);

        // Two bubble changesets: D (parent C) and E (parent D), where D and E
        // are bubble-owned and C is a "base" living outside the bubble.
        let base_c = ONES_CSID;
        let d = TWOS_CSID;
        let e = THREES_CSID;

        storage
            .add(&ctx, &edges_with_parents(d, 5, vec![base_c]))
            .await?;
        storage
            .add(&ctx, &edges_with_parents(e, 6, vec![d]))
            .await?;

        // Reads return parents from SQL — no bonsai blob ever gets loaded
        // because none was written to the bubble blobstore.
        assert_eq!(
            storage.fetch_parents(&ctx, e).await?,
            vec![d],
            "E's parents should be [D] from the parents SQL table"
        );
        assert_eq!(
            storage.fetch_parents(&ctx, d).await?,
            vec![base_c],
            "D's parents should be [base_c] from the parents SQL table"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn fetch_parents_preserves_parent_order_for_merges(fb: FacebookInit) -> Result<()> {
        let (ctx, repo_blobstore, eph) = bootstrap(fb)?;
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        let storage = ephemeral_only_storage(&bubble, repo_blobstore);

        // Merge commit M with two parents, in order [P1, P2].
        let p1 = ONES_CSID;
        let p2 = TWOS_CSID;
        let m = THREES_CSID;

        storage
            .add(&ctx, &edges_with_parents(m, 10, vec![p1, p2]))
            .await?;

        assert_eq!(
            storage.fetch_parents(&ctx, m).await?,
            vec![p1, p2],
            "Merge parents must come back in p1, p2 order (preserved via parent_index)"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn delete_bubble_clears_parents_table(fb: FacebookInit) -> Result<()> {
        let (ctx, repo_blobstore, eph) = bootstrap(fb)?;
        let bubble = eph.create_bubble(&ctx, None, vec![]).await?;
        let bubble_id = bubble.bubble_id();
        let storage = ephemeral_only_storage(&bubble, repo_blobstore);

        let d = ONES_CSID;
        let e = TWOS_CSID;
        storage
            .add(&ctx, &edges_with_parents(d, 1, vec![FIVES_CSID]))
            .await?;
        storage
            .add(&ctx, &edges_with_parents(e, 2, vec![d]))
            .await?;

        // Sanity: parents are queryable before deletion.
        assert_eq!(
            storage.fetch_parents(&ctx, e).await?,
            vec![d],
            "parents should be readable before delete_bubble"
        );

        eph.delete_bubble(&ctx, bubble_id).await?;

        // After deletion the parents rows for this bubble should be gone.
        // Probe via the SQL query directly since the higher-level fetch is
        // intentionally not implemented yet (returns Err).
        let rows = SelectChangesetParents::query(
            &storage.connections.read_connection,
            ctx.sql_query_telemetry(),
            &storage.repo_id,
            &storage.bubble_id,
            &e,
        )
        .await?;
        assert!(
            rows.is_empty(),
            "parents rows for the deleted bubble should be gone"
        );
        Ok(())
    }
}
