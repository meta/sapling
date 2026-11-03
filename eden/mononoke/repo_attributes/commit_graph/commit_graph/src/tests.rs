/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Result;
use caching_commit_graph_storage::CachingCommitGraphStorage;
use caching_ext::CacheHandlerFactory;
use commit_graph_testlib::utils::name_cs_id;
use commit_graph_testlib::utils::test_repo_identity;
use commit_graph_types::storage::CommitGraphStorage;
use commit_graph_types::storage::Prefetch;
use context::CoreContext;
use context::PerfCounterType;
use fbinit::FacebookInit;
use futures::FutureExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use maplit::hashmap;
use mononoke_macros::mononoke;
use mononoke_types::Generation;
use rendezvous::RendezVousOptions;
use sql_commit_graph_storage::SqlCommitGraphStorageBuilder;
use sql_construct::SqlConstruct;

use crate::BaseCommitGraphWriter;
use crate::CommitGraph;
use crate::CommitGraphWriter;

/// Adds `names` as a first-parent chain, the first of them as a root.
async fn add_chain(
    ctx: &CoreContext,
    writer: &BaseCommitGraphWriter,
    names: &[&str],
) -> Result<()> {
    let mut parent = None;
    for name in names {
        writer
            .add(
                ctx,
                name_cs_id(name),
                parent.into_iter().collect(),
                Default::default(),
            )
            .await?;
        parent = Some(name_cs_id(name));
    }
    Ok(())
}

/// The backing store reads that fetching the edges of `name` takes: none
/// when they are cached, one otherwise.
async fn reads_to_fetch(
    ctx: &CoreContext,
    storage: &CachingCommitGraphStorage,
    name: &str,
) -> Result<i64> {
    let before = ctx
        .perf_counters()
        .get_counter(PerfCounterType::SqlReadsReplica);
    storage.fetch_edges(ctx, name_cs_id(name)).await?;
    Ok(ctx
        .perf_counters()
        .get_counter(PerfCounterType::SqlReadsReplica)
        - before)
}

#[mononoke::fbinit_test]
async fn test_batch_prefetch_fills_only_what_the_lowering_reads(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let sql_storage = Arc::new(
        SqlCommitGraphStorageBuilder::with_sqlite_in_memory()?
            .build(RendezVousOptions::for_test(), test_repo_identity()),
    );
    // Two first-parent chains joined by a merge, A to K and N to X with
    // M = merge(K, X), written straight to the backing store so that the
    // caches start empty.
    let writer = BaseCommitGraphWriter::new(CommitGraph::new(sql_storage.clone()));
    add_chain(
        &ctx,
        &writer,
        &["A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K"],
    )
    .await?;
    add_chain(
        &ctx,
        &writer,
        &["N", "O", "P", "Q", "R", "S", "T", "U", "V", "W", "X"],
    )
    .await?;
    writer
        .add(
            &ctx,
            name_cs_id("M"),
            [name_cs_id("K"), name_cs_id("X")].into_iter().collect(),
            Default::default(),
        )
        .await?;
    let storage = Arc::new(CachingCommitGraphStorage::new(
        sql_storage,
        CacheHandlerFactory::Mocked,
    ));
    let graph = CommitGraph::new(storage.clone());

    with_just_knobs_async(
        JustKnobsInMemory::new(hashmap! {
            "scm/mononoke:commit_graph_pull_optimizations".to_string() => KnobVal::Bool(true),
            "scm/mononoke:commit_graph_prefetch_step_limit".to_string() => KnobVal::Int(4),
        }),
        async {
            // Fetching M with the linear prefetch caches M, K, J and I.
            storage
                .fetch_many_edges(
                    &ctx,
                    &[name_cs_id("M")],
                    Prefetch::for_p1_linear_traversal(),
                )
                .await?;
            assert_eq!(reads_to_fetch(&ctx, &storage, "I").await?, 0);

            // Lowering M to generation 9 pops M, then K and X, then J and W.
            // K and X enter the frontier together, so their descent is
            // batch prefetched.  X isn't cached, so the prefetch fetches X,
            // W, V and U.
            let mut frontier = graph.frontier(&ctx, vec![name_cs_id("M")]).await?;
            graph
                .lower_frontier_incrementally(&ctx, &mut frontier, Generation::new(9))
                .await?;
            assert_eq!(
                ctx.perf_counters()
                    .get_counter(PerfCounterType::CommitGraphBatchPrefetchScanned),
                2
            );
            assert_eq!(reads_to_fetch(&ctx, &storage, "U").await?, 0);
            assert_eq!(reads_to_fetch(&ctx, &storage, "T").await?, 1);
            // FIXME: K is cached and nothing the lowering reads is below I,
            // yet the batch prefetch walks down from K to the first uncached
            // changeset and fills H, G, F and E from there.
            assert_eq!(reads_to_fetch(&ctx, &storage, "E").await?, 0);
            assert_eq!(reads_to_fetch(&ctx, &storage, "D").await?, 1);
            Ok(())
        }
        .boxed(),
    )
    .await
}
