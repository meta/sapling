/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;

use anyhow::Result;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkKind;
use bookmarks::BookmarkUpdateLog;
use bookmarks::BookmarkUpdateLogId;
use bookmarks::BookmarkUpdateReason;
use bookmarks::Bookmarks;
use bookmarks::Freshness;
use bookmarks::MirrorBookmarkMove;
use context::CoreContext;
use dbbookmarks::SqlBookmarksBuilder;
use dbbookmarks::store::SqlBookmarks;
use dbbookmarks::transaction::AddBookmarkLog;
use dbbookmarks::transaction::AddBookmarkLogLock;
use dbbookmarks::transaction::FindMaxBookmarkLogLockId;
use dbbookmarks::transaction::UpdateBookmark;
use dbbookmarks::transaction::find_next_bookmark_log_lock_id;
use fbinit::FacebookInit;
use futures::FutureExt;
use futures::TryStreamExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use mononoke_macros::mononoke;
use mononoke_types::RepositoryId;
use mononoke_types::Timestamp;
use mononoke_types_mocks::changesetid::ONES_CSID;
use mononoke_types_mocks::changesetid::THREES_CSID;
use mononoke_types_mocks::changesetid::TWOS_CSID;
use mononoke_types_mocks::repo::REPO_ONE;
use mononoke_types_mocks::repo::REPO_ZERO;
use sql_construct::SqlConstruct;

sql::queries! {
    write DropGuardTable() {
        none,
        "DROP TABLE bookmarks_update_log_lock"
    }
}

fn guard_knobs(enabled: bool) -> JustKnobsInMemory {
    JustKnobsInMemory::new(HashMap::from([(
        "scm/mononoke_bookmark_log_migration:bookmark_log_guard_enabled".to_owned(),
        KnobVal::Bool(enabled),
    )]))
}

async fn guard_max(
    ctx: &CoreContext,
    store: &SqlBookmarks,
    repo_id: RepositoryId,
) -> Result<Option<u64>> {
    let rows = FindMaxBookmarkLogLockId::query(
        store.write_connection(),
        ctx.sql_query_telemetry(),
        &repo_id,
    )
    .await?;
    assert_eq!(rows.len(), 1);
    Ok(rows[0].0)
}

#[mononoke::fbinit_test]
async fn tickets_are_scoped_by_repo(fb: FacebookInit) -> Result<()> {
    with_just_knobs_async(
        guard_knobs(true),
        async move {
            let ctx = CoreContext::test_mock(fb);
            let builder = SqlBookmarksBuilder::with_sqlite_in_memory()?;
            let first = builder.clone().with_repo_id(REPO_ZERO);
            let second = builder.with_repo_id(REPO_ONE);
            let key = BookmarkKey::new("heads/main")?;
            for (store, repo_id) in [(&first, REPO_ZERO), (&second, REPO_ONE)] {
                let mut txn = store.create_transaction(ctx.clone());
                txn.force_set(&key, ONES_CSID, BookmarkUpdateReason::TestMove)?;
                assert_eq!(txn.commit().await?, Some(1));
                assert_eq!(guard_max(&ctx, store, repo_id).await?, Some(1));
            }
            Ok(())
        }
        .boxed(),
    )
    .await
}

#[mononoke::fbinit_test]
async fn failed_ticket_rolls_back_bookmark_and_log(fb: FacebookInit) -> Result<()> {
    with_just_knobs_async(
        guard_knobs(true),
        async move {
            let ctx = CoreContext::test_mock(fb);
            let store = SqlBookmarksBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
            let first = BookmarkKey::new("heads/first")?;
            let second = BookmarkKey::new("heads/second")?;
            let mut txn = store.create_transaction(ctx.clone());
            txn.force_set(&first, ONES_CSID, BookmarkUpdateReason::TestMove)?;
            assert_eq!(txn.commit().await?, Some(1));

            // SQLite cannot interleave two transactions on this connection. Capture the
            // stale ticket separately, then simulate its failed insert after a winner.
            let txn = store
                .write_connection()
                .start_transaction(ctx.sql_query_telemetry())
                .await?;
            let (txn, stale_ticket) = find_next_bookmark_log_lock_id(txn, REPO_ZERO).await?;
            assert_eq!(stale_ticket, 2);
            txn.commit().await?;
            let mut winner = store.create_transaction(ctx.clone());
            winner.force_set(&second, TWOS_CSID, BookmarkUpdateReason::TestMove)?;
            assert_eq!(winner.commit().await?, Some(2));

            let txn = store
                .write_connection()
                .start_transaction(ctx.sql_query_telemetry())
                .await?;
            let id = 3;
            let (txn, updated) = UpdateBookmark::query_with_transaction(
                txn,
                &REPO_ZERO,
                &Some(id),
                first.name(),
                first.category(),
                &ONES_CSID,
                &THREES_CSID,
                BookmarkKind::ALL_PUBLISHING,
            )
            .await?;
            assert_eq!(updated.affected_rows(), 1);
            let timestamp = Timestamp::now();
            let from = Some(ONES_CSID);
            let to = Some(THREES_CSID);
            let rows = [(
                &id,
                &REPO_ZERO,
                first.name(),
                first.category(),
                &from,
                &to,
                &BookmarkUpdateReason::TestMove,
                &timestamp,
            )];
            let (txn, _) = AddBookmarkLog::query_with_transaction(txn, &rows).await?;
            assert!(
                AddBookmarkLogLock::query_with_transaction(txn, &[(&REPO_ZERO, &stale_ticket)])
                    .await
                    .is_err()
            );
            assert_eq!(
                store
                    .get(ctx.clone(), &first, Freshness::MostRecent)
                    .await?,
                Some(ONES_CSID)
            );
            let logs: Vec<_> = store
                .read_next_bookmark_log_entries(
                    ctx.clone(),
                    BookmarkUpdateLogId(0),
                    10,
                    Freshness::MostRecent,
                )
                .try_collect()
                .await?;
            assert_eq!(
                logs.len(),
                2,
                "the rejected attempt must not publish its log row"
            );
            assert_eq!(guard_max(&ctx, &store, REPO_ZERO).await?, Some(2));
            let mut retry = store.create_transaction(ctx.clone());
            retry.update(
                &first,
                THREES_CSID,
                ONES_CSID,
                BookmarkUpdateReason::TestMove,
            )?;
            assert_eq!(retry.commit().await?, Some(3));
            assert_eq!(guard_max(&ctx, &store, REPO_ZERO).await?, Some(3));
            Ok(())
        }
        .boxed(),
    )
    .await
}

#[mononoke::fbinit_test]
async fn scratch_empty_and_mirror_do_not_allocate_tickets(fb: FacebookInit) -> Result<()> {
    with_just_knobs_async(
        guard_knobs(true),
        async move {
            let ctx = CoreContext::test_mock(fb);
            let store = SqlBookmarksBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
            let key = BookmarkKey::new("scratch")?;
            let mut txn = store.create_transaction(ctx.clone());
            txn.create_scratch(&key, ONES_CSID)?;
            assert_eq!(txn.commit().await?, Some(0));
            assert_eq!(
                store.create_transaction(ctx.clone()).commit().await?,
                Some(0)
            );
            assert_eq!(guard_max(&ctx, &store, REPO_ZERO).await?, None);
            let mut txn = store.create_transaction(ctx.clone());
            txn.mirror_batch(
                &BookmarkKey::new("heads/mirror")?,
                BookmarkKind::Publishing,
                vec![MirrorBookmarkMove {
                    log_id: 42,
                    old: None,
                    new: ONES_CSID,
                    reason: BookmarkUpdateReason::TestMove,
                }],
            )?;
            assert_eq!(txn.commit().await?, Some(42));
            assert_eq!(guard_max(&ctx, &store, REPO_ZERO).await?, None);
            Ok(())
        }
        .boxed(),
    )
    .await
}

#[mononoke::fbinit_test]
async fn disabled_guard_neither_reads_nor_writes_the_table(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let store = SqlBookmarksBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
    DropGuardTable::query(&store.write_connection().inner).await?;
    with_just_knobs_async(
        guard_knobs(false),
        async {
            let key = BookmarkKey::new("heads/main")?;
            let mut txn = store.create_transaction(ctx.clone());
            txn.create(&key, ONES_CSID, BookmarkUpdateReason::TestMove)?;
            assert_eq!(txn.commit().await?, Some(1));
            let mut txn = store.create_transaction(ctx.clone());
            txn.update(&key, TWOS_CSID, ONES_CSID, BookmarkUpdateReason::TestMove)?;
            assert_eq!(txn.commit().await?, Some(2));
            let mut txn = store.create_transaction(ctx.clone());
            txn.delete(&key, TWOS_CSID, BookmarkUpdateReason::TestMove)?;
            assert_eq!(txn.commit().await?, Some(3));
            assert_eq!(
                store.get(ctx.clone(), &key, Freshness::MostRecent).await?,
                None
            );
            let mut txn = store.create_transaction(ctx.clone());
            txn.create_scratch(&BookmarkKey::new("scratch")?, ONES_CSID)?;
            assert_eq!(txn.commit().await?, Some(0));
            assert_eq!(
                store.create_transaction(ctx.clone()).commit().await?,
                Some(0)
            );
            let mut txn = store.create_transaction(ctx.clone());
            txn.mirror_batch(
                &BookmarkKey::new("heads/mirror")?,
                BookmarkKind::Publishing,
                vec![MirrorBookmarkMove {
                    log_id: 42,
                    old: None,
                    new: ONES_CSID,
                    reason: BookmarkUpdateReason::TestMove,
                }],
            )?;
            assert_eq!(txn.commit().await?, Some(42));
            Ok(())
        }
        .boxed(),
    )
    .await
}
