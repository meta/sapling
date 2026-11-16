/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Caching tests.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Result;
use async_trait::async_trait;
use context::CoreContext;
use fbinit::FacebookInit;
use maplit::hashset;
use mercurial_mutation::CachedHgMutationStore;
use mercurial_mutation::HgMutationEntry;
use mercurial_mutation::HgMutationStore;
use mercurial_mutation::SqlHgMutationStoreBuilder;
use mercurial_types::HgChangesetId;
use mercurial_types_mocks::nodehash::make_hg_cs_id;
use mononoke_macros::mononoke;
use mononoke_types::RepositoryId;
use mononoke_types_mocks::repo::REPO_ZERO;
use sql_construct::SqlConstruct;

use crate::basic::create_entries;
use crate::util::check_entries;
use crate::util::check_entries_following_splits;

struct CountedHgMutationStore {
    inner_store: Arc<dyn HgMutationStore>,
    add_entries: Arc<AtomicUsize>,
    all_predecessors: Arc<AtomicUsize>,
}

impl CountedHgMutationStore {
    fn new(
        inner_store: Arc<dyn HgMutationStore>,
        add_entries: Arc<AtomicUsize>,
        all_predecessors: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            inner_store,
            add_entries,
            all_predecessors,
        }
    }
}

#[async_trait]
impl HgMutationStore for CountedHgMutationStore {
    fn repo_id(&self) -> RepositoryId {
        self.inner_store.repo_id()
    }

    async fn add_entries(
        &self,
        ctx: &CoreContext,
        new_changeset_ids: HashSet<HgChangesetId>,
        entries: Vec<HgMutationEntry>,
    ) -> Result<()> {
        self.add_entries.fetch_add(1, Ordering::Relaxed);
        self.inner_store
            .add_entries(ctx, new_changeset_ids, entries)
            .await
    }

    async fn all_predecessors_by_changeset(
        &self,
        ctx: &CoreContext,
        changeset_ids: HashSet<HgChangesetId>,
    ) -> Result<HashMap<HgChangesetId, Vec<HgMutationEntry>>> {
        self.all_predecessors.fetch_add(1, Ordering::Relaxed);
        self.inner_store
            .all_predecessors_by_changeset(ctx, changeset_ids)
            .await
    }

    async fn all_predecessors_following_splits(
        &self,
        ctx: &CoreContext,
        changeset_id: HgChangesetId,
    ) -> Result<Vec<HgMutationEntry>> {
        self.all_predecessors.fetch_add(1, Ordering::Relaxed);
        self.inner_store
            .all_predecessors_following_splits(ctx, changeset_id)
            .await
    }
}

#[mononoke::fbinit_test]
async fn cached_split_member_keeps_legacy_empty_until_explicit_opt_in(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let entries = create_entries();
    let inner = SqlHgMutationStoreBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
    inner
        .add_entries(
            &ctx,
            (1..=7).map(make_hg_cs_id).collect(),
            entries.values().cloned().collect(),
        )
        .await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = CountedHgMutationStore::new(
        Arc::new(inner),
        Arc::new(AtomicUsize::new(0)),
        calls.clone(),
    );
    let cached = CachedHgMutationStore::new_test(Arc::new(counted));
    check_entries(&cached, &ctx, hashset![make_hg_cs_id(7)], &entries, &[]).await?;
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    for _ in 0..2 {
        check_entries_following_splits(
            &cached,
            &ctx,
            hashset![make_hg_cs_id(7)],
            &entries,
            &[2, 4, 5, 6],
        )
        .await?;
        assert_eq!(
            calls.load(Ordering::Relaxed),
            2,
            "the first split read fills its own key; the repeat reuses it"
        );
    }
    check_entries(&cached, &ctx, hashset![make_hg_cs_id(7)], &entries, &[]).await?;
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    Ok(())
}

#[mononoke::fbinit_test]
async fn add_entries_and_fetch_predecessors_with_caching(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let sql_store = SqlHgMutationStoreBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);

    let add_entries = Arc::new(AtomicUsize::new(0));
    let all_predecessors = Arc::new(AtomicUsize::new(0));
    let counted_store = CountedHgMutationStore::new(
        Arc::new(sql_store),
        add_entries.clone(),
        all_predecessors.clone(),
    );
    let store = CachedHgMutationStore::new_test(Arc::new(counted_store));

    // Add the initial set of entries.
    let mut entries = create_entries();
    store
        .add_entries(
            &ctx,
            hashset![make_hg_cs_id(6), make_hg_cs_id(7)],
            entries.values().cloned().collect(),
        )
        .await?;
    assert_eq!(add_entries.load(Ordering::Relaxed), 1);
    check_entries(
        &store,
        &ctx,
        hashset![make_hg_cs_id(6)],
        &entries,
        &[2, 4, 5, 6],
    )
    .await?;
    // First time query, cache miss. Counters should go up.
    assert_eq!(all_predecessors.load(Ordering::Relaxed), 1);

    // Querying for different changeset, counters should again go up.
    check_entries(&store, &ctx, hashset![make_hg_cs_id(4)], &entries, &[2, 4]).await?;
    assert_eq!(all_predecessors.load(Ordering::Relaxed), 2);

    check_entries(
        &store,
        &ctx,
        hashset![make_hg_cs_id(4), make_hg_cs_id(6)],
        &entries,
        &[2, 4, 5, 6],
    )
    .await?;
    // Querying for known changesets, counters should remain the same.
    assert_eq!(all_predecessors.load(Ordering::Relaxed), 2);

    // Querying for different changeset, counters should again go up.
    check_entries(&store, &ctx, hashset![make_hg_cs_id(1)], &entries, &[]).await?;
    check_entries(&store, &ctx, hashset![make_hg_cs_id(7)], &entries, &[]).await?;
    assert_eq!(all_predecessors.load(Ordering::Relaxed), 4);

    // Add one new entry.
    let new_entry = HgMutationEntry::new(
        make_hg_cs_id(8),
        vec![make_hg_cs_id(6)],
        vec![],
        String::from("amend"),
        String::from("testuser"),
        0,
        0,
        vec![],
    );
    // Entries always bypass the cache, so the counters should always go up
    store
        .add_entries(&ctx, hashset![make_hg_cs_id(8)], vec![new_entry.clone()])
        .await?;
    assert_eq!(add_entries.load(Ordering::Relaxed), 2);
    entries.insert(8, new_entry);

    // Adding a new entry shouldn't change the result for an existing changeset.
    // The value should still be returned from cache and the counters should
    // remain the same.
    check_entries(&store, &ctx, hashset![make_hg_cs_id(7)], &entries, &[]).await?;
    assert_eq!(all_predecessors.load(Ordering::Relaxed), 4);

    check_entries(
        &store,
        &ctx,
        hashset![make_hg_cs_id(8)],
        &entries,
        &[2, 4, 5, 6, 8],
    )
    .await?;

    // Add an entry with a gap.
    let new_entries = vec![
        HgMutationEntry::new(
            make_hg_cs_id(9),
            vec![make_hg_cs_id(6)],
            vec![],
            String::from("rebase"),
            String::from("testuser"),
            0,
            0,
            vec![],
        ),
        HgMutationEntry::new(
            make_hg_cs_id(10),
            vec![make_hg_cs_id(9)],
            vec![],
            String::from("amend"),
            String::from("testuser"),
            0,
            0,
            vec![],
        ),
    ];

    // Entries always bypass the cache, so the counters should always go up
    store
        .add_entries(&ctx, hashset![make_hg_cs_id(10)], new_entries.clone())
        .await?;
    assert_eq!(add_entries.load(Ordering::Relaxed), 3);
    entries.extend((9..).zip(new_entries));

    check_entries(
        &store,
        &ctx,
        hashset![make_hg_cs_id(10)],
        &entries,
        &[2, 4, 5, 6, 9, 10],
    )
    .await?;

    // Add a more complex fold with some gaps.  This is somewhat artificial
    // as this kind of entry is unlikely to happen in normal use, however
    // it will exercise some edge cases.
    //
    // Current graph is:
    //
    //       3 -.               .-> 7
    //           \             /
    //   1 --> 2 --> 4 -->  5 --> 6 --> 8
    //                             \
    //                              '-> 9 --> 10
    //
    // We will add:
    //
    //     10  ------------> 13
    //      7  ----------/
    //      8  --> 11 --/
    //             12 -'

    let new_entries = vec![
        HgMutationEntry::new(
            make_hg_cs_id(11),
            vec![make_hg_cs_id(8)],
            vec![],
            String::from("amend"),
            String::from("testuser"),
            0,
            0,
            vec![],
        ),
        HgMutationEntry::new(
            make_hg_cs_id(13),
            vec![
                make_hg_cs_id(10),
                make_hg_cs_id(7),
                make_hg_cs_id(11),
                make_hg_cs_id(12),
            ],
            vec![],
            String::from("combine"),
            String::from("testuser"),
            0,
            0,
            vec![],
        ),
    ];

    store
        .add_entries(&ctx, hashset![make_hg_cs_id(13)], new_entries.clone())
        .await?;
    entries.extend(vec![11, 13].into_iter().zip(new_entries));

    check_entries(
        &store,
        &ctx,
        hashset![make_hg_cs_id(13)],
        &entries,
        &[2, 4, 5, 6, 8, 9, 10, 11, 13],
    )
    .await?;

    // Extend history backwards.  The original client was missing some earlier
    // data.  A new client sends commits with additional history for commit 1.
    //
    //   14 --> 15 --> 1 --> 16 --> 17
    //
    // Note that the fast-path addition of mutation data won't process this,
    // but that's a reasonable trade-off.  We add two additional successors to
    // ensure we use the slow path.
    let new_entries = vec![
        HgMutationEntry::new(
            make_hg_cs_id(15),
            vec![make_hg_cs_id(14)],
            vec![],
            String::from("amend"),
            String::from("testuser"),
            0,
            0,
            vec![],
        ),
        HgMutationEntry::new(
            make_hg_cs_id(1),
            vec![make_hg_cs_id(15)],
            vec![],
            String::from("amend"),
            String::from("testuser"),
            0,
            0,
            vec![],
        ),
        HgMutationEntry::new(
            make_hg_cs_id(16),
            vec![make_hg_cs_id(1)],
            vec![],
            String::from("amend"),
            String::from("testuser"),
            0,
            0,
            vec![],
        ),
        HgMutationEntry::new(
            make_hg_cs_id(17),
            vec![make_hg_cs_id(16)],
            vec![],
            String::from("amend"),
            String::from("testuser"),
            0,
            0,
            vec![],
        ),
    ];

    store
        .add_entries(&ctx, hashset![make_hg_cs_id(17)], new_entries.clone())
        .await?;
    entries.extend(vec![15, 1, 16, 17].into_iter().zip(new_entries));

    check_entries(
        &store,
        &ctx,
        hashset![make_hg_cs_id(17)],
        &entries,
        &[1, 15, 16, 17],
    )
    .await?;
    // The mutation history for 4 was fetched earlier so the history will
    // be served through cache.
    // Thus, instead of getting [1, 2, 4, 15] as the history we get [2, 4].
    // The new values would take place once the cache expires.
    check_entries(&store, &ctx, hashset![make_hg_cs_id(4)], &entries, &[2, 4]).await?;

    Ok(())
}
