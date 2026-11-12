/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::Result;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkUpdateReason;
use context::CoreContext;
use dbbookmarks::SqlBookmarksBuilder;
use dbbookmarks::transaction::AddBookmarkLog;
use dbbookmarks::transaction::AddBookmarkLogLock;
use dbbookmarks::transaction::FindMaxBookmarkLogId;
use dbbookmarks::transaction::NextBookmarkLogId;
use dbbookmarks::transaction::find_next_bookmark_log_ids;
use dbbookmarks::transaction::find_next_bookmark_log_lock_id;
use fbinit::FacebookInit;
use mononoke_macros::mononoke;
use mononoke_types::Timestamp;
use mononoke_types_mocks::changesetid::ONES_CSID;
use mononoke_types_mocks::repo::REPO_ONE;
use mononoke_types_mocks::repo::REPO_ZERO;
use sql_construct::SqlConstruct;

#[mononoke::test]
fn next_ids_reject_missing_names_and_overflow() -> Result<()> {
    let first = BookmarkKey::new("heads/a")?;
    let second = BookmarkKey::new("heads/b")?;
    let mut by_repo = NextBookmarkLogId::Repo(7);
    assert_eq!(by_repo.next(first.name())?, 7);
    assert_eq!(by_repo.next(second.name())?, 8);
    let mut by_name = NextBookmarkLogId::ByName(HashMap::from([
        (first.name().clone(), 1),
        (second.name().clone(), 1),
    ]));
    assert_eq!(by_name.next(first.name())?, 1);
    assert_eq!(by_name.next(first.name())?, 2);
    assert_eq!(by_name.next(second.name())?, 1);
    assert!(
        by_name
            .next(BookmarkKey::new("heads/missing")?.name())
            .is_err()
    );
    assert!(
        NextBookmarkLogId::Repo(u64::MAX)
            .next(first.name())
            .is_err()
    );
    assert!(
        NextBookmarkLogId::ByName(HashMap::from([(first.name().clone(), u64::MAX)]))
            .next(first.name())
            .is_err()
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn shared_sql_primitives_scope_tickets_and_name_ids(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let store = SqlBookmarksBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
    let first = BookmarkKey::new("heads/Foo")?;
    let second = BookmarkKey::new("heads/foo")?;
    let missing = BookmarkKey::new("heads/missing")?;
    let txn = store
        .write_connection()
        .start_transaction(ctx.sql_query_telemetry())
        .await?;
    let (txn, ticket_zero) = find_next_bookmark_log_lock_id(txn, REPO_ZERO).await?;
    let (txn, ticket_one) = find_next_bookmark_log_lock_id(txn, REPO_ONE).await?;
    assert_eq!((ticket_zero, ticket_one), (1, 1));
    let timestamp = Timestamp::now();
    let from = None;
    let to = Some(ONES_CSID);
    let reason = BookmarkUpdateReason::TestMove;
    let rows = [
        (
            &7,
            &REPO_ZERO,
            first.name(),
            first.category(),
            &from,
            &to,
            &reason,
            &timestamp,
        ),
        (
            &3,
            &REPO_ZERO,
            second.name(),
            second.category(),
            &from,
            &to,
            &reason,
            &timestamp,
        ),
        (
            &20,
            &REPO_ONE,
            first.name(),
            first.category(),
            &from,
            &to,
            &reason,
            &timestamp,
        ),
    ];
    let (txn, _) = AddBookmarkLog::query_with_transaction(txn, &rows).await?;
    let (txn, _) = AddBookmarkLogLock::query_with_transaction(
        txn,
        &[(&REPO_ZERO, &ticket_zero), (&REPO_ONE, &ticket_one)],
    )
    .await?;
    txn.commit().await?;

    let txn = store
        .write_connection()
        .start_transaction(ctx.sql_query_telemetry())
        .await?;
    let (txn, ticket_zero) = find_next_bookmark_log_lock_id(txn, REPO_ZERO).await?;
    let (txn, ticket_one) = find_next_bookmark_log_lock_id(txn, REPO_ONE).await?;
    assert_eq!((ticket_zero, ticket_one), (2, 2));
    let names = [
        first.name().clone(),
        second.name().clone(),
        missing.name().clone(),
    ]
    .into_iter()
    .collect();
    let (txn, mut by_name) = find_next_bookmark_log_ids(txn, REPO_ZERO, &names, true).await?;
    let (txn, mut by_repo) = find_next_bookmark_log_ids(txn, REPO_ZERO, &names, false).await?;
    let (txn, mut other_repo) = find_next_bookmark_log_ids(txn, REPO_ONE, &names, true).await?;
    assert_eq!(by_name.next(first.name())?, 8);
    assert_eq!(by_name.next(second.name())?, 4);
    assert_eq!(by_name.next(missing.name())?, 1);
    assert_eq!(by_repo.next(first.name())?, 8);
    assert_eq!(by_repo.next(second.name())?, 9);
    assert_eq!(other_repo.next(first.name())?, 21);
    let (txn, mut empty) =
        find_next_bookmark_log_ids(txn, REPO_ZERO, &HashSet::new(), true).await?;
    assert!(empty.next(first.name()).is_err());
    txn.commit().await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn competing_ticket_rolls_back_shared_log_insert(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let store = SqlBookmarksBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
    let key = BookmarkKey::new("heads/a")?;
    let timestamp = Timestamp::now();
    let from = None;
    let to = Some(ONES_CSID);
    let reason = BookmarkUpdateReason::TestMove;
    let txn = store
        .write_connection()
        .start_transaction(ctx.sql_query_telemetry())
        .await?;
    let (txn, _) = AddBookmarkLogLock::query_with_transaction(txn, &[(&REPO_ZERO, &1)]).await?;
    txn.commit().await?;
    let txn = store
        .write_connection()
        .start_transaction(ctx.sql_query_telemetry())
        .await?;
    let data = [(
        &1,
        &REPO_ZERO,
        key.name(),
        key.category(),
        &from,
        &to,
        &reason,
        &timestamp,
    )];
    let (txn, _) = AddBookmarkLog::query_with_transaction(txn, &data).await?;
    assert!(
        AddBookmarkLogLock::query_with_transaction(txn, &[(&REPO_ZERO, &1)])
            .await
            .is_err()
    );
    let rows = FindMaxBookmarkLogId::query(
        store.write_connection(),
        ctx.sql_query_telemetry(),
        &REPO_ZERO,
    )
    .await?;
    assert_eq!(rows, vec![(None,)]);
    Ok(())
}
