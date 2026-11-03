/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::future::Future;
use std::sync::Arc;

use anyhow::Result;
use commit_graph_testlib::utils::from_dag;
use commit_graph_testlib::utils::name_cs_id;
use commit_graph_testlib::utils::test_repo_identity;
use commit_graph_testlib::*;
use commit_graph_types::storage::CommitGraphStorage;
use commit_graph_types::storage::Prefetch;
use commit_graph_types::storage::PrefetchScope;
use commit_graph_types::storage::PrefetchTarget;
use context::CoreContext;
use fbinit::FacebookInit;
use mononoke_macros::mononoke;
use mononoke_types::FIRST_GENERATION;
use rendezvous::RendezVousOptions;
use sql_commit_graph_storage::SqlCommitGraphStorageBuilder;
use sql_construct::SqlConstruct;

use crate::CachingCommitGraphStorage;

impl CommitGraphStorageTest for CachingCommitGraphStorage {
    fn flush(&self) {
        if let Some(mock) = self.memcache.mock_store() {
            mock.flush();
        }
        if let Some(mock) = self.cachelib.mock_store() {
            mock.flush();
        }
    }
}

async fn run_test<Fut>(
    fb: FacebookInit,
    test_function: impl FnOnce(CoreContext, Arc<dyn CommitGraphStorageTest>) -> Fut,
) -> Result<()>
where
    Fut: Future<Output = Result<()>>,
{
    let ctx = CoreContext::test_mock(fb);
    let storage = Arc::new(CachingCommitGraphStorage::mocked(Arc::new(
        SqlCommitGraphStorageBuilder::with_sqlite_in_memory()
            .unwrap()
            .build(RendezVousOptions::for_test(), test_repo_identity()),
    )));
    test_function(ctx, storage.clone()).await?;
    assert!(storage.cachelib.mock_store().unwrap().stats().hits > 0);
    Ok(())
}

impl_commit_graph_tests!(run_test);

#[mononoke::fbinit_test]
pub async fn test_prefetch_many_edges_warms_beyond_cached_edges(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let storage = Arc::new(CachingCommitGraphStorage::mocked(Arc::new(
        SqlCommitGraphStorageBuilder::with_sqlite_in_memory()
            .unwrap()
            .build(RendezVousOptions::for_test(), test_repo_identity()),
    )));
    from_dag(&ctx, r"A-B-C-D-E-F-G-H", storage.clone()).await?;
    storage.flush();

    let cachelib = storage.cachelib.mock_store().unwrap();
    let cached = |name: &str| {
        cachelib
            .get(&storage.cache_key(&name_cs_id(name)))
            .is_some()
    };

    storage.fetch_edges(&ctx, name_cs_id("H")).await?;
    assert!(cached("H"));
    assert!(!cached("G"));

    // A prefetch hint has no effect once the changeset itself is cached, since
    // nothing is fetched from the backing store.
    storage
        .fetch_many_edges(
            &ctx,
            &[name_cs_id("H")],
            Prefetch::for_p1_linear_traversal(),
        )
        .await?;
    assert!(!cached("G"));

    // H is cached already, so the chain is fetched from G.  The test step
    // limit is 4, so the fetch covers G, F, E and D however many steps are
    // asked for.
    storage
        .prefetch_many_edges(
            &ctx,
            &[name_cs_id("H")],
            PrefetchScope::PastCached {
                generation: FIRST_GENERATION,
                steps: 10,
            },
        )
        .await?;
    assert!(cached("D"));
    assert!(!cached("C"));

    // Asking again fetches nothing: the walk checks as far as one fetch
    // fills, so it doesn't find the uncached chain beyond D and fetch it on
    // every call.
    let sets = cachelib.stats().sets;
    storage
        .prefetch_many_edges(
            &ctx,
            &[name_cs_id("H")],
            PrefetchScope::PastCached {
                generation: FIRST_GENERATION,
                steps: 10,
            },
        )
        .await?;
    assert_eq!(cachelib.stats().sets, sets);
    assert!(!cached("C"));

    Ok(())
}

#[mononoke::fbinit_test]
pub async fn test_prefetch_many_edges_uncached_only_leaves_cached_edges_alone(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let storage = Arc::new(CachingCommitGraphStorage::mocked(Arc::new(
        SqlCommitGraphStorageBuilder::with_sqlite_in_memory()
            .unwrap()
            .build(RendezVousOptions::for_test(), test_repo_identity()),
    )));
    from_dag(&ctx, r"A-B-C-D-E-F-G-H", storage.clone()).await?;
    storage.flush();

    let cachelib = storage.cachelib.mock_store().unwrap();
    let cached = |name: &str| {
        cachelib
            .get(&storage.cache_key(&name_cs_id(name)))
            .is_some()
    };
    let uncached_only = PrefetchScope::UncachedOnly(PrefetchTarget::LinearAncestors {
        generation: FIRST_GENERATION,
        steps: 10,
    });

    storage.fetch_edges(&ctx, name_cs_id("H")).await?;
    assert!(cached("H"));
    assert!(!cached("G"));

    // H is cached, so nothing is fetched or filled for it, however much of
    // the chain below it is uncached.
    let sets = cachelib.stats().sets;
    storage
        .prefetch_many_edges(&ctx, &[name_cs_id("H")], uncached_only)
        .await?;
    assert_eq!(cachelib.stats().sets, sets);
    assert!(!cached("G"));

    // G isn't cached, so it is fetched with its prefetch.  The test step
    // limit is 4, so the fill is exactly G, F, E and D: H is left alone.
    storage
        .prefetch_many_edges(&ctx, &[name_cs_id("H"), name_cs_id("G")], uncached_only)
        .await?;
    assert_eq!(cachelib.stats().sets, sets + 4);
    assert!(cached("D"));
    assert!(!cached("C"));

    // Everything asked for is cached now, so asking again fills nothing.
    let sets = cachelib.stats().sets;
    storage
        .prefetch_many_edges(&ctx, &[name_cs_id("H"), name_cs_id("G")], uncached_only)
        .await?;
    assert_eq!(cachelib.stats().sets, sets);
    assert!(!cached("C"));

    Ok(())
}
