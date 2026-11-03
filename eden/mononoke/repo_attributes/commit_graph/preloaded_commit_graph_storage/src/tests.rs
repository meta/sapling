/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;
use commit_graph::BaseCommitGraphWriter;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphWriter;
use commit_graph_testlib::utils::from_dag;
use commit_graph_testlib::utils::name_cs_id;
use commit_graph_testlib::utils::test_repo_identity;
use commit_graph_types::edges::ChangesetEdges;
use commit_graph_types::storage::CommitGraphStorage;
use commit_graph_types::storage::FetchedChangesetEdges;
use commit_graph_types::storage::Prefetch;
use commit_graph_types::storage::PrefetchScope;
use context::CoreContext;
use context::PerfCounterType;
use fbinit::FacebookInit;
use fbthrift::compact_protocol;
use futures::FutureExt;
use in_memory_commit_graph_storage::InMemoryCommitGraphStorage;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use maplit::hashmap;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::ChangesetIdPrefix;
use mononoke_types::ChangesetIdsResolvedFromPrefix;
use mononoke_types::FIRST_GENERATION;
use reloader::Reloader;
use repo_identity::ArcRepoIdentity;
use vec1::Vec1;

use crate::ExtendablePreloadedEdges;
use crate::PreloadedCommitGraphStorage;
use crate::deserialize_preloaded_edges;

impl PreloadedCommitGraphStorage {
    /// Constructs PreloadedCommitGraphStorage from a fixed list of edges
    /// for unit tests.
    pub fn from_edges(
        edges_vec: Vec<ChangesetEdges>,
        persistent_storage: Arc<dyn CommitGraphStorage>,
    ) -> Result<Arc<Self>> {
        let mut extendable_preloaded_edges: ExtendablePreloadedEdges = Default::default();

        for edges in edges_vec {
            extendable_preloaded_edges.add(edges)?;
        }

        // Serialize the preloaded edges and then deserialize,
        // to make sure no information is lost.
        let bytes = compact_protocol::serialize(
            &extendable_preloaded_edges
                .into_preloaded_edges()
                .to_thrift()?,
        );
        let preloaded_edges = deserialize_preloaded_edges(bytes)?;

        Ok(Arc::new(Self {
            preloaded_edges: Reloader::fixed(preloaded_edges),
            persistent_storage,
        }))
    }
}

/// Records the changesets it is asked to fetch in batches and to prefetch,
/// by prefetch scope.
struct RecordingStorage {
    inner: Arc<dyn CommitGraphStorage>,
    fetched: Mutex<Vec<ChangesetId>>,
    uncached_only_prefetched: Mutex<Vec<ChangesetId>>,
    past_cached_prefetched: Mutex<Vec<ChangesetId>>,
}

impl RecordingStorage {
    fn new(inner: Arc<dyn CommitGraphStorage>) -> Self {
        Self {
            inner,
            fetched: Mutex::new(Vec::new()),
            uncached_only_prefetched: Mutex::new(Vec::new()),
            past_cached_prefetched: Mutex::new(Vec::new()),
        }
    }

    fn fetched(&self) -> Vec<ChangesetId> {
        self.fetched.lock().unwrap().clone()
    }

    fn uncached_only_prefetched(&self) -> Vec<ChangesetId> {
        self.uncached_only_prefetched.lock().unwrap().clone()
    }

    fn past_cached_prefetched(&self) -> Vec<ChangesetId> {
        self.past_cached_prefetched.lock().unwrap().clone()
    }
}

#[async_trait]
impl CommitGraphStorage for RecordingStorage {
    fn repo_identity(&self) -> &ArcRepoIdentity {
        self.inner.repo_identity()
    }

    async fn add(&self, ctx: &CoreContext, edges: ChangesetEdges) -> Result<bool> {
        self.inner.add(ctx, edges).await
    }

    async fn add_many(&self, ctx: &CoreContext, many_edges: Vec1<ChangesetEdges>) -> Result<usize> {
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
        self.fetched.lock().unwrap().extend_from_slice(cs_ids);
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
        let recorded = match scope {
            PrefetchScope::UncachedOnly(_) => &self.uncached_only_prefetched,
            PrefetchScope::PastCached { .. } => &self.past_cached_prefetched,
        };
        recorded.lock().unwrap().extend_from_slice(cs_ids);
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

async fn test_equivalent_storages(
    ctx: &CoreContext,
    first_storage: Arc<dyn CommitGraphStorage>,
    second_storage: Arc<dyn CommitGraphStorage>,
    cs_ids: Vec<ChangesetId>,
) -> Result<()> {
    for cs_id in cs_ids.iter() {
        assert_eq!(
            first_storage.maybe_fetch_edges(ctx, *cs_id).await?,
            second_storage.maybe_fetch_edges(ctx, *cs_id).await?,
        );
        assert_eq!(
            first_storage.fetch_edges(ctx, *cs_id).await?,
            second_storage.fetch_edges(ctx, *cs_id).await?,
        );
    }

    assert_eq!(
        first_storage
            .maybe_fetch_many_edges(ctx, &cs_ids, Prefetch::None)
            .await?,
        second_storage
            .maybe_fetch_many_edges(ctx, &cs_ids, Prefetch::None)
            .await?
    );
    assert_eq!(
        first_storage
            .fetch_many_edges(ctx, &cs_ids, Prefetch::None)
            .await?,
        second_storage
            .fetch_many_edges(ctx, &cs_ids, Prefetch::None)
            .await?
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_preloaded_commit_graph_storage(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let underlying_storage = Arc::new(InMemoryCommitGraphStorage::new(test_repo_identity()));
    let _ = from_dag(
        &ctx,
        r"
            A-B-C-D-G-H-I
             \     /
              E---F
        ",
        underlying_storage.clone(),
    )
    .await?;

    let edges_vec = vec![
        underlying_storage
            .fetch_edges(&ctx, name_cs_id("A"))
            .await?,
        underlying_storage
            .fetch_edges(&ctx, name_cs_id("B"))
            .await?,
        underlying_storage
            .fetch_edges(&ctx, name_cs_id("C"))
            .await?,
        underlying_storage
            .fetch_edges(&ctx, name_cs_id("D"))
            .await?,
        underlying_storage
            .fetch_edges(&ctx, name_cs_id("E"))
            .await?,
    ];

    let preloaded_storage =
        PreloadedCommitGraphStorage::from_edges(edges_vec.clone(), underlying_storage.clone())?;

    let graph = CommitGraph::new(preloaded_storage.clone());
    let graph_writer = BaseCommitGraphWriter::new(graph.clone());

    graph_writer
        .add(&ctx, name_cs_id("J"), [name_cs_id("I")].into(), vec![])
        .await?;

    // Test that fetching from the preloaded storage is equivalent
    // to fetching directly from the underlying storage.
    test_equivalent_storages(
        &ctx,
        underlying_storage.clone(),
        preloaded_storage,
        ["A", "B", "C", "D", "E", "F", "G", "H", "I", "J"]
            .into_iter()
            .map(name_cs_id)
            .collect(),
    )
    .await?;

    let preloaded_storage_with_empty_underlying_storage = PreloadedCommitGraphStorage::from_edges(
        edges_vec.clone(),
        Arc::new(InMemoryCommitGraphStorage::new(test_repo_identity())),
    )?;

    // Test that fetching any of the preloaded edges doesn't
    // need to be fetched from the underlying storage by using
    // an empty underlying storage.
    test_equivalent_storages(
        &ctx,
        underlying_storage,
        preloaded_storage_with_empty_underlying_storage.clone(),
        ["A", "B", "C", "D", "E"]
            .into_iter()
            .map(name_cs_id)
            .collect(),
    )
    .await?;

    assert_eq!(
        preloaded_storage_with_empty_underlying_storage
            .maybe_fetch_edges(&ctx, name_cs_id("F"))
            .await?,
        None
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_preloaded_prefetch_forwarding(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let underlying_storage = Arc::new(InMemoryCommitGraphStorage::new(test_repo_identity()));
    let _ = from_dag(&ctx, r"A-B-C-D-E-F", underlying_storage.clone()).await?;

    let mut edges_vec = Vec::new();
    for name in ["A", "B", "C", "D"] {
        edges_vec.push(
            underlying_storage
                .fetch_edges(&ctx, name_cs_id(name))
                .await?,
        );
    }
    let recording_storage = Arc::new(RecordingStorage::new(underlying_storage));
    let preloaded_storage =
        PreloadedCommitGraphStorage::from_edges(edges_vec, recording_storage.clone())?;

    preloaded_storage
        .prefetch_many_edges(
            &ctx,
            &[name_cs_id("D"), name_cs_id("F")],
            PrefetchScope::PastCached {
                generation: FIRST_GENERATION,
                steps: 10,
            },
        )
        .await?;
    // F isn't preloaded, so prefetching below it is up to the underlying
    // storage.  D and everything below it are served from the preloaded
    // edges, so it isn't forwarded.
    assert_eq!(
        recording_storage.past_cached_prefetched(),
        vec![name_cs_id("F")]
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_preloaded_batch_prefetch(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let underlying_storage = Arc::new(InMemoryCommitGraphStorage::new(test_repo_identity()));
    // Every commit on the main line merges in an unrelated two-commit
    // branch, so lowering a frontier below each merge adds a changeset
    // above the target.
    let _ = from_dag(
        &ctx,
        r"
                A-B-C-D-E-F
                 / / / / /
              L-G / / / /
               M-H / / /
                N-I / /
                 O-J /
                  P-K
        ",
        underlying_storage.clone(),
    )
    .await?;

    let mut edges_vec = Vec::new();
    for name in [
        "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P",
    ] {
        edges_vec.push(
            underlying_storage
                .fetch_edges(&ctx, name_cs_id(name))
                .await?,
        );
    }
    let persistent_storage = Arc::new(RecordingStorage::new(underlying_storage));
    let preloaded_storage = Arc::new(RecordingStorage::new(
        PreloadedCommitGraphStorage::from_edges(edges_vec, persistent_storage.clone())?,
    ));
    let graph = CommitGraph::new(preloaded_storage.clone());

    with_just_knobs_async(
        JustKnobsInMemory::new(hashmap! {
            "scm/mononoke:commit_graph_pull_optimizations".to_string() => KnobVal::Bool(true),
            "scm/mononoke:commit_graph_prefetch_step_limit".to_string() => KnobVal::Int(4),
        }),
        async {
            assert!(
                graph
                    .is_ancestor(&ctx, name_cs_id("A"), name_cs_id("F"))
                    .await?
            );
            // Lowering F to A's generation fetches F, E, D, C and B one
            // step at a time and then G, H, I, J and K in one step.  E and
            // K, D and J, C and I, and B and H are batch prefetched as each
            // merge adds its branch to the frontier, which only warms
            // caches for changesets that aren't cached, never past them:
            // every one of them is preloaded, so none reaches the
            // persistent storage.
            assert_eq!(
                ctx.perf_counters()
                    .get_counter(PerfCounterType::CommitGraphBatchPrefetchScanned),
                8
            );
            assert_eq!(preloaded_storage.fetched().len(), 10);
            let mut prefetched = preloaded_storage.uncached_only_prefetched();
            prefetched.sort();
            let mut expected: Vec<_> = ["B", "C", "D", "E", "H", "I", "J", "K"]
                .into_iter()
                .map(name_cs_id)
                .collect();
            expected.sort();
            assert_eq!(prefetched, expected);
            assert_eq!(preloaded_storage.past_cached_prefetched(), vec![]);
            assert_eq!(persistent_storage.uncached_only_prefetched(), vec![]);
            assert_eq!(persistent_storage.past_cached_prefetched(), vec![]);
            Ok(())
        }
        .boxed(),
    )
    .await
}
