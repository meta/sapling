/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;

use anyhow::Error;
use dbbookmarks::transaction::FindMaxBookmarkLogLockId;
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

fn knobs(enabled: bool, by_name: bool, retired: bool) -> JustKnobsInMemory {
    JustKnobsInMemory::new(HashMap::from([
        (
            "scm/mononoke_bookmark_log_migration:bookmark_log_guard_enabled".to_owned(),
            KnobVal::Bool(true),
        ),
        (
            "scm/mononoke_bookmark_log_migration:bookmark_log_ids_by_name".to_owned(),
            KnobVal::Bool(true),
        ),
        (
            "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_guard_enabled".to_owned(),
            KnobVal::Bool(enabled),
        ),
        (
            "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_ids_by_name".to_owned(),
            KnobVal::Bool(by_name),
        ),
        (
            "scm/mononoke_bookmark_log_migration:bookmark_log_guard_retired".to_owned(),
            KnobVal::Bool(retired),
        ),
    ]))
}

#[mononoke::fbinit_test]
async fn activated_legacy_mrls_keeps_guard_even_when_retirement_is_enabled(
    fb: FacebookInit,
) -> Result<()> {
    for enabled in [false, true] {
        let f = TwoRepoFixture::new(fb)?;
        with_just_knobs_async(
            knobs(enabled, false, true),
            async {
                let mut txn = f.multi_txn();
                for repo_id in [f.repo_id_1, f.repo_id_2] {
                    for name in ["heads/a", "heads/b"] {
                        txn.create(
                            repo_id,
                            &BookmarkKey::new(name)?,
                            ONES_CSID,
                            BookmarkUpdateReason::TestMove,
                        )?;
                    }
                }
                assert!(txn.commit().await?.is_success());
                for repo_id in [f.repo_id_1, f.repo_id_2] {
                    let rows = FindMaxBookmarkLogLockId::query(
                        &f.conn,
                        f.ctx.sql_query_telemetry(),
                        &repo_id,
                    )
                    .await?;
                    assert_eq!(rows[0].0, if enabled { Some(2) } else { None });
                }
                Ok::<_, Error>(())
            }
            .boxed(),
        )
        .await?;
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn retired_mrls_skips_guard_across_interleaved_chunks_and_keeps_atomicity(
    fb: FacebookInit,
) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    DropGuardTable::query(&f.conn, f.ctx.sql_query_telemetry()).await?;
    with_just_knobs_async(
        knobs(true, true, true),
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
                assert!(
                    entries
                        .iter()
                        .all(|entry| entry.id == BookmarkUpdateLogId(1))
                );
            }
            Ok(())
        }
        .boxed(),
    )
    .await
}
