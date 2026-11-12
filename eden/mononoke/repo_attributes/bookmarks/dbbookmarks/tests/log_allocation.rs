/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;

use anyhow::Error;
use anyhow::Result;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkUpdateLog;
use bookmarks::BookmarkUpdateLogId;
use bookmarks::BookmarkUpdateReason;
use bookmarks::Bookmarks;
use bookmarks::Freshness;
use context::CoreContext;
use dbbookmarks::SqlBookmarksBuilder;
use dbbookmarks::transaction::FindMaxBookmarkLogLockId;
use dbbookmarks::transaction::find_next_bookmark_log_ids;
use dbbookmarks::transaction::find_next_bookmark_log_lock_id;
use fbinit::FacebookInit;
use futures::FutureExt;
use futures::TryStreamExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use mononoke_macros::mononoke;
use mononoke_types_mocks::changesetid::ONES_CSID;
use mononoke_types_mocks::changesetid::THREES_CSID;
use mononoke_types_mocks::changesetid::TWOS_CSID;
use mononoke_types_mocks::repo::REPO_ONE;
use mononoke_types_mocks::repo::REPO_ZERO;
use sql_construct::SqlConstruct;

fn by_name_knobs() -> JustKnobsInMemory {
    JustKnobsInMemory::new(HashMap::from([
        (
            "scm/mononoke_bookmark_log_migration:bookmark_log_ids_by_name".to_owned(),
            KnobVal::Bool(true),
        ),
        (
            "scm/mononoke_bookmark_log_migration:bookmark_log_guard_enabled".to_owned(),
            KnobVal::Bool(true),
        ),
    ]))
}

#[mononoke::fbinit_test]
async fn name_scoped_ids_reuse_numbers_and_order_recreation(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let store = SqlBookmarksBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
    let first = BookmarkKey::new("heads/first")?;
    let second = BookmarkKey::new("heads/second")?;
    let created = BookmarkKey::new("heads/created")?;
    let mut initial = store.create_transaction(ctx.clone());
    initial.force_set(&first, ONES_CSID, BookmarkUpdateReason::TestMove)?;
    initial.force_set(&second, ONES_CSID, BookmarkUpdateReason::TestMove)?;
    assert_eq!(initial.commit().await?, Some(1));

    with_just_knobs_async(
        by_name_knobs(),
        async {
            let mut txn = store.create_transaction(ctx.clone());
            txn.create(&created, ONES_CSID, BookmarkUpdateReason::TestMove)?;
            assert_eq!(txn.commit().await?, Some(1));
            let mut txn = store.create_transaction(ctx.clone());
            txn.force_set(&first, TWOS_CSID, BookmarkUpdateReason::TestMove)?;
            assert_eq!(txn.commit().await?, Some(2));
            let mut txn = store.create_transaction(ctx.clone());
            txn.update(
                &first,
                THREES_CSID,
                TWOS_CSID,
                BookmarkUpdateReason::TestMove,
            )?;
            assert_eq!(txn.commit().await?, Some(3));
            let mut txn = store.create_transaction(ctx.clone());
            txn.delete(&first, THREES_CSID, BookmarkUpdateReason::TestMove)?;
            assert_eq!(txn.commit().await?, Some(4));
            let mut txn = store.create_transaction(ctx.clone());
            txn.create(&first, ONES_CSID, BookmarkUpdateReason::TestMove)?;
            assert_eq!(txn.commit().await?, Some(5));
            let entries: Vec<_> = store
                .read_next_bookmark_log_entries_by_bookmark(
                    ctx.clone(),
                    first.clone(),
                    BookmarkUpdateLogId(0),
                    20,
                    Freshness::MostRecent,
                )
                .try_collect()
                .await?;
            assert_eq!(
                entries.iter().map(|entry| entry.id.0).collect::<Vec<_>>(),
                vec![1, 2, 3, 4, 5]
            );
            assert!(entries[3].to_changeset_id.is_none());
            assert_eq!(
                store
                    .get_raw(ctx.clone(), &second, Freshness::MostRecent)
                    .await?,
                Some((ONES_CSID, Some(2)))
            );
            let rows = FindMaxBookmarkLogLockId::query(
                store.write_connection(),
                ctx.sql_query_telemetry(),
                &REPO_ZERO,
            )
            .await?;
            assert_eq!(
                rows[0].0,
                Some(5),
                "guard tickets count the five guarded name-mode rows"
            );
            Ok(())
        }
        .boxed(),
    )
    .await
}

#[mononoke::fbinit_test]
async fn case_distinct_names_have_independent_sequences(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let store = SqlBookmarksBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
    with_just_knobs_async(
        by_name_knobs(),
        async {
            let upper = BookmarkKey::new("heads/Foo")?;
            let lower = BookmarkKey::new("heads/foo")?;
            for changeset in [ONES_CSID, TWOS_CSID] {
                let mut txn = store.create_transaction(ctx.clone());
                txn.force_set(&upper, changeset, BookmarkUpdateReason::TestMove)?;
                txn.force_set(&lower, changeset, BookmarkUpdateReason::TestMove)?;
                assert!(txn.commit().await?.is_some());
            }
            for key in [&upper, &lower] {
                let entries: Vec<_> = store
                    .read_next_bookmark_log_entries_by_bookmark(
                        ctx.clone(),
                        key.clone(),
                        BookmarkUpdateLogId(0),
                        10,
                        Freshness::MostRecent,
                    )
                    .try_collect()
                    .await?;
                assert_eq!(
                    entries.iter().map(|entry| entry.id.0).collect::<Vec<_>>(),
                    vec![1, 2]
                );
                assert!(entries.iter().all(|entry| &entry.bookmark_name == key));
            }
            Ok(())
        }
        .boxed(),
    )
    .await
}

#[mononoke::fbinit_test]
async fn repo_and_name_allocation_state_can_share_a_transaction(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let builder = SqlBookmarksBuilder::with_sqlite_in_memory()?;
    let first = builder.clone().with_repo_id(REPO_ZERO);
    let second = builder.with_repo_id(REPO_ONE);
    let a = BookmarkKey::new("heads/a")?;
    let b = BookmarkKey::new("heads/b")?;
    for store in [&first, &second] {
        let mut txn = store.create_transaction(ctx.clone());
        txn.create(&a, ONES_CSID, BookmarkUpdateReason::TestMove)?;
        txn.create(&b, ONES_CSID, BookmarkUpdateReason::TestMove)?;
        assert!(txn.commit().await?.is_some());
    }
    let txn = first
        .write_connection()
        .start_transaction(ctx.sql_query_telemetry())
        .await?;
    let (txn, _) = find_next_bookmark_log_lock_id(txn, REPO_ZERO).await?;
    let (txn, _) = find_next_bookmark_log_lock_id(txn, REPO_ONE).await?;
    let names = [a.name().clone(), b.name().clone()].into_iter().collect();
    let (txn, mut by_name) = find_next_bookmark_log_ids(txn, REPO_ZERO, &names, true).await?;
    let (txn, mut by_repo) = find_next_bookmark_log_ids(txn, REPO_ONE, &names, false).await?;
    assert_eq!(by_name.next(a.name())?, 2);
    assert_eq!(by_name.next(b.name())?, 3);
    assert_eq!(by_repo.next(a.name())?, 3);
    assert_eq!(by_repo.next(b.name())?, 4);
    txn.commit().await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn name_allocation_requires_guard_activation(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    for retired in [false, true] {
        let store = SqlBookmarksBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
        let knobs = JustKnobsInMemory::new(HashMap::from([
            (
                "scm/mononoke_bookmark_log_migration:bookmark_log_guard_enabled".to_owned(),
                KnobVal::Bool(false),
            ),
            (
                "scm/mononoke_bookmark_log_migration:bookmark_log_ids_by_name".to_owned(),
                KnobVal::Bool(true),
            ),
            (
                "scm/mononoke_bookmark_log_migration:bookmark_log_guard_retired".to_owned(),
                KnobVal::Bool(retired),
            ),
        ]));
        with_just_knobs_async(
            knobs,
            async {
                let key = BookmarkKey::new("heads/main")?;
                let mut txn = store.create_transaction(ctx.clone());
                txn.create(&key, ONES_CSID, BookmarkUpdateReason::TestMove)?;
                let error = txn
                    .commit()
                    .await
                    .expect_err("name allocation needs activation");
                assert!(format!("{error:#}").contains("requires migration guard activation"));
                assert_eq!(
                    store.get(ctx.clone(), &key, Freshness::MostRecent).await?,
                    None
                );
                let entries: Vec<_> = store
                    .read_next_bookmark_log_entries(
                        ctx.clone(),
                        BookmarkUpdateLogId(0),
                        10,
                        Freshness::MostRecent,
                    )
                    .try_collect()
                    .await?;
                assert!(entries.is_empty());
                Ok::<_, Error>(())
            }
            .boxed(),
        )
        .await?;
    }
    Ok(())
}
