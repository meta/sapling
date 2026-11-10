/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;

use dbbookmarks::transaction::FindMaxBookmarkLogLockId;
use futures::FutureExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;

use super::*;

sql_ext::mononoke_queries! {
    write DropGuardTable() {
        none,
        "DROP TABLE bookmarks_update_log_lock"
    }
}

fn guard_knobs(enabled: bool) -> JustKnobsInMemory {
    JustKnobsInMemory::new(HashMap::from([(
        "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_guard_enabled".to_owned(),
        KnobVal::Bool(enabled),
    )]))
}

async fn guard_max(f: &TwoRepoFixture, repo_id: RepositoryId) -> Result<Option<u64>> {
    let rows =
        FindMaxBookmarkLogLockId::query(&f.conn, f.ctx.sql_query_telemetry(), &repo_id).await?;
    assert_eq!(rows.len(), 1);
    Ok(rows[0].0)
}

#[mononoke::fbinit_test]
async fn multi_repo_writers_allocate_repo_tickets(fb: FacebookInit) -> Result<()> {
    with_just_knobs_async(
        guard_knobs(true),
        async move {
            let f = TwoRepoFixture::new(fb)?;
            let first = BookmarkKey::new("heads/first")?;
            let second = BookmarkKey::new("heads/second")?;
            let mut initial = f.multi_txn();
            initial.create(
                f.repo_id_1,
                &first,
                ONES_CSID,
                BookmarkUpdateReason::TestMove,
            )?;
            initial.create(
                f.repo_id_1,
                &second,
                ONES_CSID,
                BookmarkUpdateReason::TestMove,
            )?;
            initial.create(
                f.repo_id_2,
                &first,
                ONES_CSID,
                BookmarkUpdateReason::TestMove,
            )?;
            assert!(initial.commit().await?.is_success());
            assert_eq!(guard_max(&f, f.repo_id_1).await?, Some(2));
            assert_eq!(guard_max(&f, f.repo_id_2).await?, Some(1));
            let mut txn = f.multi_txn();
            for key in [&first, &second] {
                txn.update(
                    f.repo_id_1,
                    key,
                    TWOS_CSID,
                    ONES_CSID,
                    BookmarkUpdateReason::TestMove,
                )?;
            }
            txn.update(
                f.repo_id_2,
                &first,
                TWOS_CSID,
                ONES_CSID,
                BookmarkUpdateReason::TestMove,
            )?;
            assert!(txn.commit().await?.is_success());
            assert_eq!(guard_max(&f, f.repo_id_1).await?, Some(4));
            assert_eq!(guard_max(&f, f.repo_id_2).await?, Some(2));
            let mut txn = f.multi_txn();
            txn.update(
                f.repo_id_1,
                &first,
                THREES_CSID,
                TWOS_CSID,
                BookmarkUpdateReason::TestMove,
            )?;
            assert!(txn.commit().await?.is_success());
            assert_eq!(guard_max(&f, f.repo_id_1).await?, Some(5));
            Ok(())
        }
        .boxed(),
    )
    .await
}

#[mononoke::fbinit_test]
async fn cas_failure_rolls_back_all_tickets(fb: FacebookInit) -> Result<()> {
    with_just_knobs_async(
        guard_knobs(true),
        async move {
            let f = TwoRepoFixture::new(fb)?;
            let key = BookmarkKey::new("heads/main")?;
            let mut initial = f.multi_txn();
            for repo_id in [f.repo_id_1, f.repo_id_2] {
                initial.create(repo_id, &key, ONES_CSID, BookmarkUpdateReason::TestMove)?;
            }
            assert!(initial.commit().await?.is_success());
            let mut txn = f.multi_txn();
            txn.update(
                f.repo_id_1,
                &key,
                TWOS_CSID,
                ONES_CSID,
                BookmarkUpdateReason::TestMove,
            )?;
            txn.update(
                f.repo_id_2,
                &key,
                TWOS_CSID,
                THREES_CSID,
                BookmarkUpdateReason::TestMove,
            )?;
            assert!(!txn.commit().await?.is_success());
            assert_eq!(guard_max(&f, f.repo_id_1).await?, Some(1));
            assert_eq!(guard_max(&f, f.repo_id_2).await?, Some(1));
            Ok(())
        }
        .boxed(),
    )
    .await
}

#[mononoke::fbinit_test]
async fn disabled_guard_skips_table_across_chunks_and_keeps_atomicity(
    fb: FacebookInit,
) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    DropGuardTable::query(&f.conn, f.ctx.sql_query_telemetry()).await?;
    with_just_knobs_async(
        guard_knobs(false),
        async {
            let mut txn = f.multi_txn();
            for index in 0..110 {
                let key = BookmarkKey::new(format!("heads/key-{index:03}"))?;
                for repo_id in [f.repo_id_1, f.repo_id_2] {
                    txn.create(repo_id, &key, ONES_CSID, BookmarkUpdateReason::TestMove)?;
                }
            }
            assert!(txn.commit().await?.is_success());
            let key = BookmarkKey::new("heads/key-000")?;
            let mut failed = f.multi_txn();
            failed.update(
                f.repo_id_1,
                &key,
                TWOS_CSID,
                ONES_CSID,
                BookmarkUpdateReason::TestMove,
            )?;
            failed.update(
                f.repo_id_2,
                &key,
                TWOS_CSID,
                THREES_CSID,
                BookmarkUpdateReason::TestMove,
            )?;
            assert!(!failed.commit().await?.is_success());
            for store in [&f.bookmarks_1, &f.bookmarks_2] {
                assert_eq!(f.get_bookmark(store, &key).await?, Some(ONES_CSID));
                let entries: Vec<_> = store
                    .read_next_bookmark_log_entries(
                        f.ctx.clone(),
                        BookmarkUpdateLogId(0),
                        200,
                        Freshness::MostRecent,
                    )
                    .try_collect()
                    .await?;
                assert_eq!(entries.len(), 110);
            }
            Ok(())
        }
        .boxed(),
    )
    .await
}
