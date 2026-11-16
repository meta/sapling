/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::FutureExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs;
use justknobs::test_helpers::with_just_knobs_async;
use mercurial_types::HgChangesetId;
use mercurial_types_mocks::nodehash::make_hg_cs_id;
use mononoke_macros::mononoke;
use mononoke_types::RepositoryId;
use mononoke_types_mocks::repo::REPO_ZERO;
use sql_construct::SqlConstruct;

use super::CachedHgMutationStore;
use super::HgMutationCacheEntry;
use super::KeyGen;
use super::SplitCacheRequest;
use super::get_cache_key;
use super::get_split_cache_key;
use super::thrift;
use crate::HgMutationEntry;
use crate::HgMutationStore;
use crate::SqlHgMutationStoreBuilder;

fn entry(successor: u64, predecessors: &[u64], split: &[u64], op: &str) -> HgMutationEntry {
    HgMutationEntry::new(
        make_hg_cs_id(successor),
        predecessors.iter().copied().map(make_hg_cs_id).collect(),
        split.iter().copied().map(make_hg_cs_id).collect(),
        op.to_owned(),
        "cache-version-test".to_owned(),
        0,
        0,
        vec![],
    )
}

struct CountingStore {
    inner: Arc<dyn HgMutationStore>,
    legacy_calls: Mutex<Vec<usize>>,
    split_calls: Mutex<Vec<HgChangesetId>>,
}

#[async_trait]
impl HgMutationStore for CountingStore {
    fn repo_id(&self) -> RepositoryId {
        self.inner.repo_id()
    }

    async fn add_entries(
        &self,
        ctx: &CoreContext,
        ids: HashSet<HgChangesetId>,
        entries: Vec<HgMutationEntry>,
    ) -> Result<()> {
        self.inner.add_entries(ctx, ids, entries).await
    }

    async fn all_predecessors_by_changeset(
        &self,
        ctx: &CoreContext,
        ids: HashSet<HgChangesetId>,
    ) -> Result<HashMap<HgChangesetId, Vec<HgMutationEntry>>> {
        self.legacy_calls
            .lock()
            .expect("fixture calls")
            .push(ids.len());
        self.inner.all_predecessors_by_changeset(ctx, ids).await
    }

    async fn all_predecessors_following_splits(
        &self,
        ctx: &CoreContext,
        id: HgChangesetId,
    ) -> Result<Vec<HgMutationEntry>> {
        self.split_calls.lock().expect("fixture calls").push(id);
        self.inner.all_predecessors_following_splits(ctx, id).await
    }
}

async fn fixture(ctx: &CoreContext) -> Result<Arc<CountingStore>> {
    let inner = SqlHgMutationStoreBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
    inner
        .add_entries(
            ctx,
            (1..=7).map(make_hg_cs_id).collect(),
            vec![
                entry(2, &[1], &[], "amend"),
                entry(5, &[2], &[3, 4], "split"),
                entry(6, &[3], &[], "rebase"),
                entry(7, &[6], &[], "metaedit"),
            ],
        )
        .await?;
    Ok(Arc::new(CountingStore {
        inner: Arc::new(inner),
        legacy_calls: Mutex::new(Vec::new()),
        split_calls: Mutex::new(Vec::new()),
    }))
}

#[mononoke::test]
fn legacy_keys_are_unchanged_and_split_keys_are_separate_in_both_tiers() {
    const { assert!(thrift::MC_CODEVER == 0) };
    let keygen = KeyGen::new("scm.mononoke.hg_mutation_store", 0, 7);
    for id in [3, 7, 5] {
        let cs = make_hg_cs_id(id);
        let legacy = get_cache_key(REPO_ZERO, &cs);
        assert_eq!(legacy, format!("{}.{}", REPO_ZERO.prefix(), cs));
        let split = get_split_cache_key(REPO_ZERO, &cs);
        assert_eq!(split, format!("split1.{}.{}", REPO_ZERO.prefix(), cs));
        assert_ne!(legacy, split);
        assert_ne!(keygen.key(&legacy), keygen.key(&split));
    }
}

#[mononoke::fbinit_test]
async fn both_cache_tiers_keep_default_and_singleton_histories_independent(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    for memcache_only in [false, true] {
        let counted = fixture(&ctx).await?;
        let mut cached = CachedHgMutationStore::new_test(counted.clone());
        if memcache_only {
            cached.cachelib = caching_ext::CachelibHandler::create_noop();
        } else {
            cached.memcache = caching_ext::MemcacheHandler::create_noop();
        }
        // Neither store path consults a caller-policy knob. The handler owns that policy.
        let poisoned = JustKnobsInMemory::new(HashMap::from([(
            "scm/mononoke:enable_split_member_mutation_lookup".to_owned(),
            KnobVal::Int(1),
        )]));
        for _ in 0..2 {
            let legacy = with_just_knobs_async(
                poisoned.clone(),
                cached
                    .all_predecessors_by_changeset(
                        &ctx,
                        [3, 7].map(make_hg_cs_id).into_iter().collect(),
                    )
                    .boxed(),
            )
            .await?;
            assert!(legacy[&make_hg_cs_id(3)].is_empty());
            assert_eq!(
                legacy[&make_hg_cs_id(7)],
                vec![
                    entry(7, &[6], &[], "metaedit"),
                    entry(6, &[3], &[], "rebase")
                ]
            );
            for (id, expected_len) in [(3, 2), (7, 4), (5, 2)] {
                let actual = with_just_knobs_async(
                    poisoned.clone(),
                    cached
                        .all_predecessors_following_splits(&ctx, make_hg_cs_id(id))
                        .boxed(),
                )
                .await?;
                assert_eq!(actual.len(), expected_len);
            }
        }
        assert_eq!(counted.legacy_calls.lock().expect("calls").as_slice(), &[2]);
        assert_eq!(
            counted.split_calls.lock().expect("calls").as_slice(),
            &[3, 7, 5].map(make_hg_cs_id)
        );
        let absent = JustKnobsInMemory::new(HashMap::new());
        assert!(
            with_just_knobs_async(
                absent,
                cached
                    .all_predecessors(&ctx, HashSet::from([make_hg_cs_id(3)]))
                    .boxed()
            )
            .await?
            .is_empty()
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn legacy_positive_empty_partial_values_remain_warm_in_each_tier(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let site_key = "scm/mononoke_memcache_sitevers:hg_mutation_store";
    for memcache_only in [false, true] {
        let counted = fixture(&ctx).await?;
        let mut cached = with_just_knobs(
            JustKnobsInMemory::new(HashMap::from([(site_key.to_owned(), KnobVal::Int(7))])),
            || CachedHgMutationStore::new_test(counted.clone()),
        );
        if memcache_only {
            cached.cachelib = caching_ext::CachelibHandler::create_noop();
        } else {
            cached.memcache = caching_ext::MemcacheHandler::create_noop();
        }
        let keygen = KeyGen::new("scm.mononoke.hg_mutation_store", 0, 7);
        let old_values = [
            (2, vec![entry(2, &[1], &[], "amend")]),
            (3, vec![]),
            (7, vec![entry(7, &[6], &[], "metaedit")]),
        ];
        for (id, values) in &old_values {
            let cs = make_hg_cs_id(*id);
            let old = HgMutationCacheEntry::from_entries(values.clone(), REPO_ZERO, cs);
            let raw = get_cache_key(REPO_ZERO, &cs);
            if memcache_only {
                cached
                    .memcache
                    .set(keygen.key(&raw), super::memcache_serialize(&old))
                    .await?;
            } else {
                cached
                    .cachelib
                    .set_cached(&raw, &old, Some(Duration::from_hours(1)))?;
            }
        }
        let poisoned_site =
            JustKnobsInMemory::new(HashMap::from([(site_key.to_owned(), KnobVal::Bool(false))]));
        for _ in 0..2 {
            let legacy = with_just_knobs_async(
                poisoned_site.clone(),
                cached
                    .all_predecessors_by_changeset(
                        &ctx,
                        [2, 3, 7].map(make_hg_cs_id).into_iter().collect(),
                    )
                    .boxed(),
            )
            .await?;
            for (id, expected) in &old_values {
                assert_eq!(&legacy[&make_hg_cs_id(*id)], expected);
            }
            assert!(counted.legacy_calls.lock().expect("calls").is_empty());
            assert_eq!(
                with_just_knobs_async(
                    poisoned_site.clone(),
                    cached
                        .all_predecessors_following_splits(&ctx, make_hg_cs_id(3))
                        .boxed()
                )
                .await?
                .len(),
                2
            );
            assert_eq!(
                with_just_knobs_async(
                    poisoned_site.clone(),
                    cached
                        .all_predecessors_following_splits(&ctx, make_hg_cs_id(7))
                        .boxed()
                )
                .await?
                .len(),
                4
            );
        }
        assert_eq!(
            counted.split_calls.lock().expect("calls").as_slice(),
            &[3, 7].map(make_hg_cs_id)
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn legacy_chunks_and_singleton_fills_do_not_cross_pollute(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let counted = fixture(&ctx).await?;
    let cached = CachedHgMutationStore::new_test(counted.clone());
    let ids = [make_hg_cs_id(3), make_hg_cs_id(7)]
        .into_iter()
        .chain((100..1101).map(make_hg_cs_id))
        .collect::<HashSet<_>>();
    let result = cached
        .all_predecessors_by_changeset(&ctx, ids.clone())
        .await?;
    assert_eq!(result.len(), ids.len());
    let calls = counted.legacy_calls.lock().expect("calls").clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls.iter().sum::<usize>(), ids.len());
    assert_eq!(
        cached
            .all_predecessors_following_splits(&ctx, make_hg_cs_id(3))
            .await?
            .len(),
        2
    );
    assert_eq!(
        counted.split_calls.lock().expect("calls").as_slice(),
        &[make_hg_cs_id(3)]
    );
    let request = SplitCacheRequest {
        ctx: &ctx,
        store: &cached,
        changeset_id: make_hg_cs_id(3),
    };
    use caching_ext::KeyedEntityStore;
    assert!(request.get_from_db(HashSet::new()).await?.is_empty());
    assert!(
        request
            .get_from_db([3, 7].map(make_hg_cs_id).into_iter().collect())
            .await
            .is_err()
    );
    assert!(
        request
            .get_from_db(HashSet::from([make_hg_cs_id(7)]))
            .await
            .is_err()
    );
    assert_eq!(counted.split_calls.lock().expect("calls").len(), 1);
    Ok(())
}

#[mononoke::fbinit_test]
async fn empty_split_histories_stay_cached_across_late_backfill(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let counted = fixture(&ctx).await?;
    let cached = CachedHgMutationStore::new_test(counted.clone());
    assert!(
        cached
            .all_predecessors_following_splits(&ctx, make_hg_cs_id(9))
            .await?
            .is_empty()
    );
    counted
        .add_entries(
            &ctx,
            HashSet::from([make_hg_cs_id(9)]),
            vec![entry(9, &[8], &[], "amend")],
        )
        .await?;
    assert!(
        cached
            .all_predecessors_following_splits(&ctx, make_hg_cs_id(9))
            .await?
            .is_empty()
    );
    assert_eq!(
        counted
            .inner
            .all_predecessors_following_splits(&ctx, make_hg_cs_id(9))
            .await?,
        vec![entry(9, &[8], &[], "amend")]
    );
    assert_eq!(
        counted.split_calls.lock().expect("calls").as_slice(),
        &[make_hg_cs_id(9)]
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn legacy_order_duplicates_cycles_and_late_split_policy_are_preserved(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let store = SqlHgMutationStoreBuilder::with_sqlite_in_memory()?
        .with_mutation_limit(4)
        .with_repo_id(REPO_ZERO);
    store
        .add_entries(
            &ctx,
            (40..=43).map(make_hg_cs_id).collect(),
            vec![
                entry(40, &[41, 42], &[], "fold"),
                entry(41, &[42], &[], "amend"),
                entry(42, &[43], &[], "rebase"),
            ],
        )
        .await?;
    let grouped = store
        .all_predecessors_by_changeset(&ctx, HashSet::from([make_hg_cs_id(40)]))
        .await?;
    assert_eq!(
        grouped[&make_hg_cs_id(40)]
            .iter()
            .map(|r| *r.successor())
            .collect::<Vec<_>>(),
        [40, 41, 42, 42].map(make_hg_cs_id)
    );
    assert_eq!(
        store
            .all_predecessors(&ctx, HashSet::from([make_hg_cs_id(40)]))
            .await?
            .len(),
        3
    );
    assert_eq!(
        store
            .all_predecessors_following_splits(&ctx, make_hg_cs_id(40))
            .await?
            .len(),
        3
    );
    let cycle = SqlHgMutationStoreBuilder::with_sqlite_in_memory()?
        .with_mutation_limit(3)
        .with_repo_id(REPO_ZERO);
    cycle
        .add_entries(
            &ctx,
            [30, 31].map(make_hg_cs_id).into_iter().collect(),
            vec![
                entry(30, &[31], &[], "amend"),
                entry(31, &[30], &[], "rebase"),
            ],
        )
        .await?;
    assert_eq!(
        cycle
            .all_predecessors_by_changeset(&ctx, HashSet::from([make_hg_cs_id(30)]))
            .await?[&make_hg_cs_id(30)]
            .iter()
            .map(|r| *r.successor())
            .collect::<Vec<_>>(),
        [30, 31].map(make_hg_cs_id)
    );
    assert_eq!(
        cycle
            .all_predecessors_following_splits(&ctx, make_hg_cs_id(30))
            .await?
            .len(),
        2
    );
    let late =
        Arc::new(SqlHgMutationStoreBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO));
    late.add_entries(
        &ctx,
        (1..=5).map(make_hg_cs_id).collect(),
        vec![entry(2, &[1], &[], "amend"), entry(5, &[2], &[3], "split")],
    )
    .await?;
    late.add_entries(
        &ctx,
        HashSet::from([make_hg_cs_id(5)]),
        vec![entry(5, &[2, 9], &[3, 4], "importstack")],
    )
    .await?;
    let cached = CachedHgMutationStore::new_test(late.clone());
    for s in [
        late.as_ref() as &dyn HgMutationStore,
        &cached as &dyn HgMutationStore,
    ] {
        let default = s
            .all_predecessors(&ctx, HashSet::from([make_hg_cs_id(5)]))
            .await?;
        assert!(default.contains(&entry(5, &[2], &[3, 4], "split")));
        assert!(
            s.all_predecessors_following_splits(&ctx, make_hg_cs_id(4))
                .await?
                .is_empty()
        );
        let opt_in = s
            .all_predecessors_following_splits(&ctx, make_hg_cs_id(5))
            .await?;
        assert!(opt_in.contains(&entry(5, &[2], &[3], "split")));
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn one_keygen_and_clones_keep_the_constructor_site_version(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let inner = fixture(&ctx).await?;
    let key = "scm/mononoke_memcache_sitevers:hg_mutation_store";
    let cached = with_just_knobs(
        JustKnobsInMemory::new(HashMap::from([(key.to_owned(), KnobVal::Int(7))])),
        || CachedHgMutationStore::new_test(inner),
    );
    with_just_knobs(
        JustKnobsInMemory::new(HashMap::from([(key.to_owned(), KnobVal::Bool(false))])),
        || {
            let cloned = cached.clone();
            for s in [&cached, &cloned] {
                let cs = make_hg_cs_id(3);
                let split = SplitCacheRequest {
                    ctx: &ctx,
                    store: s,
                    changeset_id: cs,
                };
                use caching_ext::EntityStore;
                let old = KeyGen::new("scm.mononoke.hg_mutation_store", 0, 7);
                assert_eq!(
                    EntityStore::<HgMutationCacheEntry>::keygen(&(&ctx, s))
                        .key(get_cache_key(REPO_ZERO, &cs)),
                    old.key(get_cache_key(REPO_ZERO, &cs))
                );
                assert_eq!(
                    EntityStore::<HgMutationCacheEntry>::keygen(&split)
                        .key(get_split_cache_key(REPO_ZERO, &cs)),
                    old.key(get_split_cache_key(REPO_ZERO, &cs))
                );
            }
        },
    );
    Ok(())
}
