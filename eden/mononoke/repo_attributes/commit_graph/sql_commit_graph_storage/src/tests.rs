/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use commit_graph::BaseCommitGraphWriter;
use commit_graph::ChangesetParents;
use commit_graph::ChangesetSubtreeSources;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphWriter;
use commit_graph_testlib::counting_writes_storage::CountingWritesCommitGraphStorage;
use commit_graph_testlib::lagging_replica_storage::LaggingReplicaCommitGraphStorage;
use commit_graph_testlib::shuffling_storage::ShufflingCommitGraphStorage;
use commit_graph_testlib::utils::cs_id_name;
use commit_graph_testlib::utils::from_dag;
use commit_graph_testlib::utils::name_cs_id;
use commit_graph_testlib::utils::test_repo_identity;
use commit_graph_testlib::*;
use commit_graph_types::storage::CommitGraphStorage;
use commit_graph_types::storage::Prefetch;
use commit_graph_types::storage::PrefetchTarget;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::FutureExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use mononoke_macros::mononoke;
use mononoke_types::Generation;
use mononoke_types::RepositoryId;
use rendezvous::RendezVousOptions;
use sql_construct::SqlConstruct;
use vec1::vec1;

use crate::SqlCommitGraphStorage;
use crate::SqlCommitGraphStorageBuilder;

impl CommitGraphStorageTest for SqlCommitGraphStorage {}

async fn run_test<Fut>(
    fb: FacebookInit,
    test_function: impl FnOnce(CoreContext, Arc<dyn CommitGraphStorageTest>) -> Fut,
) -> Result<()>
where
    Fut: Future<Output = Result<()>>,
{
    let ctx = CoreContext::test_mock(fb);
    let storage = Arc::new(
        SqlCommitGraphStorageBuilder::with_sqlite_in_memory()
            .unwrap()
            .build(RendezVousOptions::for_test(), test_repo_identity()),
    );
    test_function(ctx, storage).await
}

impl_commit_graph_tests!(run_test);

async fn run_test_write_stored_changesets<Fut>(
    fb: FacebookInit,
    test_function: impl FnOnce(CoreContext, Arc<dyn CommitGraphStorageTest>) -> Fut,
) -> Result<()>
where
    Fut: Future<Output = Result<()>>,
{
    with_skip_stored_changesets(false, fb, test_function).await
}

async fn with_skip_stored_changesets<Fut>(
    skip_stored_changesets: bool,
    fb: FacebookInit,
    test_function: impl FnOnce(CoreContext, Arc<dyn CommitGraphStorageTest>) -> Fut,
) -> Result<()>
where
    Fut: Future<Output = Result<()>>,
{
    with_just_knobs_async(
        JustKnobsInMemory::new(HashMap::from([(
            "scm/mononoke:commit_graph_skip_stored_changesets".to_string(),
            KnobVal::Bool(skip_stored_changesets),
        )])),
        run_test(fb, test_function).boxed_local(),
    )
    .await
}

/// The shared suite with `commit_graph_skip_stored_changesets` switched off.
mod write_stored_changesets {
    use super::*;

    impl_commit_graph_tests!(run_test_write_stored_changesets);
}

/// With `commit_graph_skip_stored_changesets`, commit graph writes skip
/// changesets that are already stored, and write only the new ones.
async fn stored_changesets_are_not_written(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let counting = Arc::new(CountingWritesCommitGraphStorage::new(storage.clone()));
    let writer = BaseCommitGraphWriter::new(CommitGraph::new(counting.clone()));
    let entry = |name: &str, parents: &[&str]| {
        (
            name_cs_id(name),
            parents
                .iter()
                .map(|parent| name_cs_id(parent))
                .collect::<ChangesetParents>(),
            ChangesetSubtreeSources::new(),
        )
    };

    writer
        .add_many(&ctx, vec1![entry("A", &[]), entry("B", &["A"])])
        .await?;
    let before = counting.written();
    assert_eq!(
        writer
            .add_many(&ctx, vec1![entry("A", &[]), entry("B", &["A"])])
            .await?,
        0,
        "A and B are already stored"
    );
    let (b, b_parents, b_sources) = entry("B", &["A"]);
    assert!(
        !writer.add(&ctx, b, b_parents, b_sources).await?,
        "B is already stored"
    );
    assert_eq!(
        counting.written(),
        before,
        "nothing already stored is written"
    );

    assert_eq!(
        writer
            .add_many(&ctx, vec1![entry("B", &["A"]), entry("C", &["B"])])
            .await?,
        1,
        "only C is new"
    );
    assert_eq!(counting.written(), before + 1, "only C is written");
    assert_eq!(
        storage
            .fetch_edges(&ctx, name_cs_id("C"))
            .await?
            .parents::<commit_graph_types::edges::Parents>()
            .map(|node| node.cs_id)
            .collect::<Vec<_>>(),
        vec![name_cs_id("B")],
        "C is built on the stored B"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_stored_changesets_are_not_written(fb: FacebookInit) -> Result<()> {
    with_skip_stored_changesets(true, fb, stored_changesets_are_not_written).await
}

/// A stored changeset that a lagging replica misses is written again, which
/// changes nothing: the counts and the stored edges are as without the miss.
async fn lagging_replica_rewrites_stored_changesets(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let writer = BaseCommitGraphWriter::new(CommitGraph::new(Arc::new(
        LaggingReplicaCommitGraphStorage::new(storage.clone()),
    )));
    let entry = |name: &str, parents: &[&str]| {
        (
            name_cs_id(name),
            parents
                .iter()
                .map(|parent| name_cs_id(parent))
                .collect::<ChangesetParents>(),
            ChangesetSubtreeSources::new(),
        )
    };

    assert_eq!(
        writer
            .add_many(&ctx, vec1![entry("A", &[]), entry("B", &["A"])])
            .await?,
        2
    );
    assert_eq!(
        writer
            .add_many(&ctx, vec1![entry("A", &[]), entry("B", &["A"])])
            .await?,
        0,
        "A and B are already stored"
    );
    let (b, b_parents, b_sources) = entry("B", &["A"]);
    assert!(
        !writer.add(&ctx, b, b_parents, b_sources).await?,
        "B is already stored"
    );
    assert_eq!(
        writer
            .add_many(&ctx, vec1![entry("B", &["A"]), entry("C", &["B"])])
            .await?,
        1,
        "only C is new"
    );
    assert_eq!(
        storage
            .fetch_edges(&ctx, name_cs_id("C"))
            .await?
            .parents::<commit_graph_types::edges::Parents>()
            .map(|node| node.cs_id)
            .collect::<Vec<_>>(),
        vec![name_cs_id("B")],
        "C is built on the stored B"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_lagging_replica_rewrites_stored_changesets(fb: FacebookInit) -> Result<()> {
    with_skip_stored_changesets(true, fb, lagging_replica_rewrites_stored_changesets).await
}

#[mononoke::fbinit_test]
pub async fn test_max_id_with_empty_graph(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let storage = Arc::new(
        SqlCommitGraphStorageBuilder::with_sqlite_in_memory()
            .unwrap()
            .build(RendezVousOptions::for_test(), test_repo_identity()),
    );
    let graph = from_dag(&ctx, r##""##, storage.clone()).await?;
    assert_eq!(storage.max_id(&ctx, false).await?, Some(0));
    Ok(())
}

#[mononoke::fbinit_test]
pub async fn test_lower_level_api(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let storage = Arc::new(
        SqlCommitGraphStorageBuilder::with_sqlite_in_memory()
            .unwrap()
            .build(RendezVousOptions::for_test(), test_repo_identity()),
    );

    let graph = from_dag(
        &ctx,
        r##"
             A-B-C-D-E-F-G-H-I-J
         "##,
        storage.clone(),
    )
    .await?;

    assert_eq!(storage.max_id(&ctx, false).await?, Some(10));
    assert_eq!(storage.max_id(&ctx, true).await?, Some(10));

    assert_eq!(
        storage.max_id_in_range(&ctx, 1, 10, 10, false).await?,
        Some(10),
    );
    assert_eq!(
        storage.max_id_in_range(&ctx, 2, 10, 5, false).await?,
        Some(6),
    );
    assert_eq!(
        storage.max_id_in_range(&ctx, 4, 7, 100, false).await?,
        Some(7),
    );

    assert_eq!(
        storage
            .fetch_many_cs_ids_in_id_range(&ctx, 1, 10, 10, false)
            .await?,
        ["A", "B", "C", "D", "E", "F", "G", "H", "I", "J"]
            .into_iter()
            .map(name_cs_id)
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        storage
            .fetch_many_cs_ids_in_id_range(&ctx, 1, 10, 5, false)
            .await?,
        ["A", "B", "C", "D", "E"]
            .into_iter()
            .map(name_cs_id)
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        storage
            .fetch_many_cs_ids_in_id_range(&ctx, 4, 6, 100, false)
            .await?,
        ["D", "E", "F"]
            .into_iter()
            .map(name_cs_id)
            .collect::<Vec<_>>(),
    );

    let all_edges = storage
        .fetch_many_edges(
            &ctx,
            &["A", "B", "C", "D", "E", "F", "G", "H", "I", "J"]
                .into_iter()
                .map(name_cs_id)
                .collect::<Vec<_>>(),
            Prefetch::None,
        )
        .await?;

    assert_eq!(
        storage
            .fetch_many_edges_in_id_range(&ctx, 1, 10, 10, false)
            .await?
            .into_iter()
            .map(|(k, (_, v))| (k, v))
            .collect::<HashMap<_, _>>(),
        ["A", "B", "C", "D", "E", "F", "G", "H", "I", "J"]
            .into_iter()
            .map(|id| {
                let cs_id = name_cs_id(id);
                (cs_id, all_edges.get(&cs_id).unwrap().clone().into())
            })
            .collect::<HashMap<_, _>>(),
    );
    assert_eq!(
        storage
            .fetch_many_edges_in_id_range(&ctx, 1, 10, 5, false)
            .await?
            .into_iter()
            .map(|(k, (_, v))| (k, v))
            .collect::<HashMap<_, _>>(),
        ["A", "B", "C", "D", "E"]
            .into_iter()
            .map(|id| {
                let cs_id = name_cs_id(id);
                (cs_id, all_edges.get(&cs_id).unwrap().clone().into())
            })
            .collect::<HashMap<_, _>>(),
    );
    assert_eq!(
        storage
            .fetch_many_edges_in_id_range(&ctx, 4, 6, 100, false)
            .await?
            .into_iter()
            .map(|(k, (_, v))| (k, v))
            .collect::<HashMap<_, _>>(),
        ["D", "E", "F"]
            .into_iter()
            .map(|id| {
                let cs_id = name_cs_id(id);
                (cs_id, all_edges.get(&cs_id).unwrap().clone().into())
            })
            .collect::<HashMap<_, _>>(),
    );

    Ok(())
}

#[mononoke::fbinit_test]
pub async fn test_exact_skip_tree_prefetch_crosses_skip_tree_roots(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let storage = Arc::new(
        SqlCommitGraphStorageBuilder::with_sqlite_in_memory()
            .unwrap()
            .build(RendezVousOptions::for_test(), test_repo_identity()),
    );

    // M merges two histories that have no common ancestor, which makes it a
    // skip tree root.  The skew binary traversal continues through the
    // parents of such roots, so prefetching its path must return them, but
    // it only follows the first parent further: W is never returned.
    from_dag(
        &ctx,
        r"
         A-B-M-N
            /
         W-X
         ",
        storage.clone(),
    )
    .await?;

    let prefetched_names = |generation: u64| {
        let storage = storage.clone();
        let ctx = ctx.clone();
        async move {
            let edges = storage
                .fetch_many_edges(
                    &ctx,
                    &[name_cs_id("N")],
                    Prefetch::Include(PrefetchTarget::ExactSkipTreeAncestors {
                        generation: Generation::new(generation),
                    }),
                )
                .await?;
            let mut names: Vec<_> = edges.keys().map(|cs_id| cs_id_name(*cs_id)).collect();
            names.sort();
            anyhow::Ok(names)
        }
    };

    assert_eq!(prefetched_names(1).await?, ["A", "B", "M", "N", "X"]);
    assert_eq!(prefetched_names(2).await?, ["B", "M", "N", "X"]);

    Ok(())
}

#[mononoke::fbinit_test]
pub async fn test_batched_prefetches_keep_their_targets(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    // No free connections and a long delay, so that concurrent requests are
    // batched together.
    let storage = Arc::new(
        SqlCommitGraphStorageBuilder::with_sqlite_in_memory()
            .unwrap()
            .build(
                RendezVousOptions {
                    free_connections: 0,
                    max_delay: Duration::from_millis(100),
                    max_threshold: 100,
                    cap_batch_at_threshold: true,
                },
                test_repo_identity(),
            ),
    );

    from_dag(&ctx, r"A-B-C-D-E-F-G-H", storage.clone()).await?;

    let prefetched_names = |name: &'static str, generation: u64| {
        let storage = storage.clone();
        let ctx = ctx.clone();
        async move {
            let edges = storage
                .fetch_many_edges(
                    &ctx,
                    &[name_cs_id(name)],
                    Prefetch::Include(PrefetchTarget::LinearAncestors {
                        generation: Generation::new(generation),
                        steps: 10,
                    }),
                )
                .await?;
            let mut names: Vec<_> = edges.keys().map(|cs_id| cs_id_name(*cs_id)).collect();
            names.sort();
            anyhow::Ok(names)
        }
    };

    // Each request's prefetch stops at its own generation rather than at
    // the generation of the request that opened the batch.
    let (from_h, from_d) = futures::try_join!(prefetched_names("H", 6), prefetched_names("D", 1))?;
    assert_eq!(from_h, ["F", "G", "H"]);
    assert_eq!(from_d, ["A", "B", "C", "D"]);

    Ok(())
}
