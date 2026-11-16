/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Split-member mutation history regressions.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use context::CoreContext;
use fbinit::FacebookInit;
use maplit::hashset;
use mercurial_mutation::CachedHgMutationStore;
use mercurial_mutation::HgMutationEntry;
use mercurial_mutation::HgMutationStore;
use mercurial_mutation::SqlHgMutationStore;
use mercurial_mutation::SqlHgMutationStoreBuilder;
use mercurial_types_mocks::nodehash::make_hg_cs_id;
use mononoke_macros::mononoke;
use mononoke_types_mocks::repo::REPO_ZERO;
use sql_construct::SqlConstruct;

use crate::util::check_entries_following_splits as check_entries;

fn entry(successor: u64, predecessors: &[u64], split: &[u64], op: &str) -> HgMutationEntry {
    HgMutationEntry::new(
        make_hg_cs_id(successor),
        predecessors.iter().copied().map(make_hg_cs_id).collect(),
        split.iter().copied().map(make_hg_cs_id).collect(),
        op.to_owned(),
        "split-test-user".to_owned(),
        123456789,
        -3600,
        vec![("test-extra".to_owned(), "preserved".to_owned())],
    )
}

fn split_history() -> HashMap<usize, HgMutationEntry> {
    [
        (2, entry(2, &[1], &[], "amend")),
        (5, entry(5, &[2], &[3, 4], "split")),
        (6, entry(6, &[3], &[], "rebase")),
        (7, entry(7, &[6], &[], "metaedit")),
        (8, entry(8, &[4], &[], "importstack")),
    ]
    .into_iter()
    .collect()
}

async fn store_history(
    ctx: &CoreContext,
    limit: usize,
    entries: &HashMap<usize, HgMutationEntry>,
) -> Result<SqlHgMutationStore> {
    let store = SqlHgMutationStoreBuilder::with_sqlite_in_memory()?
        .with_mutation_limit(limit)
        .with_repo_id(REPO_ZERO);
    store
        .add_entries(
            ctx,
            (1..=8).map(make_hg_cs_id).collect(),
            entries.values().cloned().collect(),
        )
        .await?;
    Ok(store)
}

#[mononoke::fbinit_test]
async fn intermediate_split_member_returns_canonical_entry(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let entries = split_history();
    let store = store_history(&ctx, 20, &entries).await?;
    for member in [3, 4] {
        check_entries(
            &store,
            &ctx,
            hashset![make_hg_cs_id(member)],
            &entries,
            &[2, 5],
        )
        .await?;
    }
    let first = store
        .all_predecessors_following_splits(&ctx, make_hg_cs_id(3))
        .await?;
    let second = store
        .all_predecessors_following_splits(&ctx, make_hg_cs_id(4))
        .await?;
    assert_eq!(first, second);
    assert!(first.iter().all(|entry| {
        *entry.successor() != make_hg_cs_id(3) && *entry.successor() != make_hg_cs_id(4)
    }));
    Ok(())
}

#[mononoke::fbinit_test]
async fn split_successor_and_non_split_history_are_unchanged(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let entries = split_history();
    let store = store_history(&ctx, 20, &entries).await?;
    check_entries(&store, &ctx, hashset![make_hg_cs_id(5)], &entries, &[2, 5]).await?;
    check_entries(&store, &ctx, hashset![make_hg_cs_id(2)], &entries, &[2]).await?;
    check_entries(&store, &ctx, hashset![make_hg_cs_id(1)], &entries, &[]).await?;
    check_entries(&store, &ctx, hashset![], &entries, &[]).await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn split_then_rebase_then_metaedit_preserves_history(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let entries = split_history();
    let store = store_history(&ctx, 20, &entries).await?;
    check_entries(
        &store,
        &ctx,
        hashset![make_hg_cs_id(7)],
        &entries,
        &[2, 5, 6, 7],
    )
    .await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn imported_split_member_preserves_history(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let entries = split_history();
    let store = store_history(&ctx, 20, &entries).await?;
    check_entries(
        &store,
        &ctx,
        hashset![make_hg_cs_id(8)],
        &entries,
        &[2, 5, 8],
    )
    .await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn canonical_importstack_and_freeform_split_entries_preserve_history(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    for op in ["importstack", "custom-rewrite"] {
        let mut entries = split_history();
        entries.insert(5, entry(5, &[2], &[3, 4], op));
        let store =
            CachedHgMutationStore::new_test(Arc::new(store_history(&ctx, 20, &entries).await?));
        for _ in 0..2 {
            for root in [3, 4, 5] {
                check_entries(
                    &store,
                    &ctx,
                    hashset![make_hg_cs_id(root)],
                    &entries,
                    &[2, 5],
                )
                .await?;
            }
            check_entries(
                &store,
                &ctx,
                hashset![make_hg_cs_id(7)],
                &entries,
                &[2, 5, 6, 7],
            )
            .await?;
            check_entries(
                &store,
                &ctx,
                hashset![make_hg_cs_id(8)],
                &entries,
                &[2, 5, 8],
            )
            .await?;
            check_entries(
                &store,
                &ctx,
                hashset![
                    make_hg_cs_id(3),
                    make_hg_cs_id(4),
                    make_hg_cs_id(5),
                    make_hg_cs_id(7),
                    make_hg_cs_id(8)
                ],
                &entries,
                &[2, 5, 6, 7, 8],
            )
            .await?;
        }
        for (limit, expected) in [
            (1, vec![7]),
            (2, vec![6, 7]),
            (3, vec![5, 6, 7]),
            (4, vec![2, 5, 6, 7]),
        ] {
            let limited = store_history(&ctx, limit, &entries).await?;
            check_entries(
                &limited,
                &ctx,
                hashset![make_hg_cs_id(7)],
                &entries,
                &expected,
            )
            .await?;
            check_entries(
                &limited,
                &ctx,
                hashset![make_hg_cs_id(3)],
                &entries,
                if limit == 1 { &[5] } else { &[2, 5] },
            )
            .await?;
        }
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn split_history_respects_chain_limit(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let entries = split_history();
    for (limit, expected) in [
        (1, vec![7]),
        (2, vec![6, 7]),
        (3, vec![5, 6, 7]),
        (4, vec![2, 5, 6, 7]),
    ] {
        let store = store_history(&ctx, limit, &entries).await?;
        check_entries(
            &store,
            &ctx,
            hashset![make_hg_cs_id(7)],
            &entries,
            &expected,
        )
        .await?;
        let member_entries = store
            .all_predecessors_following_splits(&ctx, make_hg_cs_id(3))
            .await?;
        assert_eq!(member_entries.len(), limit.min(2));
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn repeated_splits_and_multiple_roots_do_not_duplicate_entries(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let mut entries = split_history();
    entries.insert(10, entry(10, &[3], &[9], "split"));
    let store = store_history(&ctx, 20, &entries).await?;
    check_entries(
        &store,
        &ctx,
        hashset![make_hg_cs_id(9)],
        &entries,
        &[2, 5, 10],
    )
    .await?;
    check_entries(
        &store,
        &ctx,
        hashset![
            make_hg_cs_id(3),
            make_hg_cs_id(4),
            make_hg_cs_id(5),
            make_hg_cs_id(9),
            make_hg_cs_id(10)
        ],
        &entries,
        &[2, 5, 10],
    )
    .await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn overlapping_splits_direct_entries_and_cycles_are_deduplicated(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let mut entries = split_history();
    entries.insert(9, entry(9, &[1], &[], "amend"));
    entries.insert(10, entry(10, &[9], &[3], "split"));
    let store = store_history(&ctx, 20, &entries).await?;
    check_entries(
        &store,
        &ctx,
        hashset![make_hg_cs_id(3)],
        &entries,
        &[2, 5, 9, 10],
    )
    .await?;

    let mut entries = split_history();
    entries.insert(3, entry(3, &[1], &[], "amend"));
    let store = store_history(&ctx, 20, &entries).await?;
    check_entries(
        &store,
        &ctx,
        hashset![make_hg_cs_id(3)],
        &entries,
        &[2, 3, 5],
    )
    .await?;

    let mut entries = split_history();
    entries.insert(2, entry(2, &[3], &[], "amend"));
    let store = store_history(&ctx, 20, &entries).await?;
    check_entries(&store, &ctx, hashset![make_hg_cs_id(3)], &entries, &[2, 5]).await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn later_split_rows_cannot_change_the_first_entry(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let entries = split_history();
    let store = store_history(&ctx, 20, &entries).await?;
    store
        .add_entries(
            &ctx,
            hashset![make_hg_cs_id(9)],
            vec![entry(5, &[2], &[3, 4, 9], "split")],
        )
        .await?;
    check_entries(&store, &ctx, hashset![make_hg_cs_id(5)], &entries, &[2, 5]).await?;
    check_entries(&store, &ctx, hashset![make_hg_cs_id(9)], &entries, &[]).await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn singleton_split_lookup_matches_bounded_input_graph(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    for shape in 0..3 {
        let mut entries = split_history();
        entries.insert(9, entry(9, &[2], &[], "amend"));
        entries.insert(10, entry(10, &[9, 5], &[3], "importstack"));
        entries.insert(11, entry(11, &[7, 8], &[], "fold"));
        entries.insert(12, entry(12, &[10, 6], &[], "fold"));
        entries.insert(13, entry(13, &[2, 12], &[], "rebase"));
        if shape >= 1 {
            entries.insert(3, entry(3, &[9], &[], "amend"));
        }
        if shape == 2 {
            entries.insert(2, entry(2, &[12], &[], "amend"));
        }
        for limit in 1..=4 {
            let inner = Arc::new(
                SqlHgMutationStoreBuilder::with_sqlite_in_memory()?
                    .with_mutation_limit(limit)
                    .with_repo_id(REPO_ZERO),
            );
            inner
                .add_entries(
                    &ctx,
                    (1..=13).map(make_hg_cs_id).collect(),
                    entries.values().cloned().collect(),
                )
                .await?;
            for root in 1..=13 {
                let root = make_hg_cs_id(root);
                let cached = CachedHgMutationStore::new_test(inner.clone());
                let mut frontier = HashSet::from([root]);
                let mut expected = HashSet::new();
                for _ in 0..limit {
                    let level: Vec<_> = entries
                        .values()
                        .filter(|record| {
                            frontier.contains(record.successor())
                                || record.split().any(|member| frontier.contains(member))
                        })
                        .collect();
                    frontier = level
                        .iter()
                        .flat_map(|record| record.predecessors().copied())
                        .collect();
                    expected.extend(level.into_iter().cloned());
                }
                for store in [
                    inner.as_ref() as &dyn HgMutationStore,
                    &cached as &dyn HgMutationStore,
                ] {
                    for _ in 0..2 {
                        let history = store.all_predecessors_following_splits(&ctx, root).await?;
                        let actual: HashSet<_> = history.iter().cloned().collect();
                        assert_eq!(history.len(), actual.len());
                        assert_eq!(
                            actual, expected,
                            "singleton root {root}, shape {shape}, limit {limit} matches bounded query reachability"
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn cached_split_history_matches_uncached_history(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let entries = split_history();
    let store = CachedHgMutationStore::new_test(Arc::new(store_history(&ctx, 20, &entries).await?));
    for _ in 0..2 {
        check_entries(&store, &ctx, hashset![make_hg_cs_id(3)], &entries, &[2, 5]).await?;
        check_entries(
            &store,
            &ctx,
            hashset![make_hg_cs_id(7)],
            &entries,
            &[2, 5, 6, 7],
        )
        .await?;
    }
    Ok(())
}
