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
use fbinit::FacebookInit;
use futures::FutureExt;
use futures::TryStreamExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use mononoke_macros::mononoke;
use mononoke_types_mocks::changesetid::ONES_CSID;
use mononoke_types_mocks::changesetid::TWOS_CSID;
use mononoke_types_mocks::repo::REPO_ZERO;
use sql_construct::SqlConstruct;

sql::queries! {
    write DropGuardTable() {
        none,
        "DROP TABLE bookmarks_update_log_lock"
    }
}

fn knobs(enabled: bool, by_name: bool, retired: bool) -> JustKnobsInMemory {
    JustKnobsInMemory::new(HashMap::from([
        (
            "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_guard_enabled".to_owned(),
            KnobVal::Bool(true),
        ),
        (
            "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_ids_by_name".to_owned(),
            KnobVal::Bool(true),
        ),
        (
            "scm/mononoke_bookmark_log_migration:bookmark_log_guard_enabled".to_owned(),
            KnobVal::Bool(enabled),
        ),
        (
            "scm/mononoke_bookmark_log_migration:bookmark_log_ids_by_name".to_owned(),
            KnobVal::Bool(by_name),
        ),
        (
            "scm/mononoke_bookmark_log_migration:bookmark_log_guard_retired".to_owned(),
            KnobVal::Bool(retired),
        ),
    ]))
}

#[mononoke::fbinit_test]
async fn only_activated_name_mode_can_retire_the_guard(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    for enabled in [false, true] {
        for by_name in [false, true] {
            for retired in [false, true] {
                let store = SqlBookmarksBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
                with_just_knobs_async(
                    knobs(enabled, by_name, retired),
                    async {
                        if by_name && !enabled {
                            let key = BookmarkKey::new("heads/a")?;
                            let mut txn = store.create_transaction(ctx.clone());
                            txn.create(&key, ONES_CSID, BookmarkUpdateReason::TestMove)?;
                            assert!(txn.commit().await.is_err());
                            assert_eq!(
                                store.get(ctx.clone(), &key, Freshness::MostRecent).await?,
                                None
                            );
                            return Ok::<_, Error>(());
                        }
                        for name in ["heads/a", "heads/b"] {
                            let mut txn = store.create_transaction(ctx.clone());
                            txn.create(
                                &BookmarkKey::new(name)?,
                                ONES_CSID,
                                BookmarkUpdateReason::TestMove,
                            )?;
                            assert!(txn.commit().await?.is_some());
                        }
                        let rows = FindMaxBookmarkLogLockId::query(
                            store.write_connection(),
                            ctx.sql_query_telemetry(),
                            &REPO_ZERO,
                        )
                        .await?;
                        let guarded = enabled && !(by_name && retired);
                        assert_eq!(rows[0].0, if guarded { Some(2) } else { None });
                        let second = store
                            .get_raw(
                                ctx.clone(),
                                &BookmarkKey::new("heads/b")?,
                                Freshness::MostRecent,
                            )
                            .await?;
                        assert_eq!(second, Some((ONES_CSID, Some(if by_name { 1 } else { 2 }))));
                        Ok::<_, Error>(())
                    }
                    .boxed(),
                )
                .await?;
            }
        }
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn retired_name_mode_neither_reads_nor_writes_the_guard(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let store = SqlBookmarksBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
    DropGuardTable::query(&store.write_connection().inner).await?;
    with_just_knobs_async(
        knobs(true, true, true),
        async {
            let key = BookmarkKey::new("heads/a")?;
            let mut txn = store.create_transaction(ctx.clone());
            txn.create(&key, ONES_CSID, BookmarkUpdateReason::TestMove)?;
            assert_eq!(txn.commit().await?, Some(1));
            let mut txn = store.create_transaction(ctx.clone());
            txn.delete(&key, ONES_CSID, BookmarkUpdateReason::TestMove)?;
            assert_eq!(txn.commit().await?, Some(2));
            let mut txn = store.create_transaction(ctx.clone());
            txn.create(&key, TWOS_CSID, BookmarkUpdateReason::TestMove)?;
            assert_eq!(txn.commit().await?, Some(3));
            let entries: Vec<_> = store
                .read_next_bookmark_log_entries_by_bookmark(
                    ctx.clone(),
                    key,
                    BookmarkUpdateLogId(0),
                    10,
                    Freshness::MostRecent,
                )
                .try_collect()
                .await?;
            assert_eq!(
                entries.iter().map(|e| e.id.0).collect::<Vec<_>>(),
                vec![1, 2, 3]
            );
            Ok(())
        }
        .boxed(),
    )
    .await
}

#[mononoke::fbinit_test]
async fn guard_restoration_precedes_allocation_rollback(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let store = SqlBookmarksBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
    // Sequential fixture phases test restoring capture and insertion, not draining
    // concurrent production transactions or validating a live rollback protocol.
    for (by_name, retired, name, expected_id, expected_ticket) in [
        (true, false, "heads/a", 1, 1),
        (true, true, "heads/b", 1, 1),
        (true, false, "heads/a", 2, 2),
        (false, false, "heads/c", 3, 3),
    ] {
        with_just_knobs_async(
            knobs(true, by_name, retired),
            async {
                let mut txn = store.create_transaction(ctx.clone());
                txn.force_set(
                    &BookmarkKey::new(name)?,
                    ONES_CSID,
                    BookmarkUpdateReason::TestMove,
                )?;
                assert_eq!(txn.commit().await?, Some(expected_id));
                let rows = FindMaxBookmarkLogLockId::query(
                    store.write_connection(),
                    ctx.sql_query_telemetry(),
                    &REPO_ZERO,
                )
                .await?;
                assert_eq!(rows[0].0, Some(expected_ticket));
                Ok::<_, Error>(())
            }
            .boxed(),
        )
        .await?;
    }
    Ok(())
}
