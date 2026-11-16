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

fn by_name_knobs() -> JustKnobsInMemory {
    JustKnobsInMemory::new(HashMap::from([
        (
            "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_ids_by_name".to_owned(),
            KnobVal::Bool(true),
        ),
        (
            "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_guard_enabled".to_owned(),
            KnobVal::Bool(true),
        ),
    ]))
}

#[mononoke::fbinit_test]
async fn per_name_updates_keep_multi_repo_atomicity(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    let first = BookmarkKey::new("heads/first")?;
    let second = BookmarkKey::new("heads/second")?;
    f.set_bookmark(&f.bookmarks_1, &first, ONES_CSID).await?;
    f.set_bookmark(&f.bookmarks_1, &second, ONES_CSID).await?;
    f.set_bookmark(&f.bookmarks_2, &first, ONES_CSID).await?;
    with_just_knobs_async(
        by_name_knobs(),
        async {
            let mut txn = f.multi_txn();
            txn.update(
                f.repo_id_1,
                &first,
                TWOS_CSID,
                ONES_CSID,
                BookmarkUpdateReason::TestMove,
            )?;
            txn.update(
                f.repo_id_1,
                &second,
                TWOS_CSID,
                ONES_CSID,
                BookmarkUpdateReason::TestMove,
            )?;
            txn.update(
                f.repo_id_2,
                &first,
                TWOS_CSID,
                ONES_CSID,
                BookmarkUpdateReason::TestMove,
            )?;
            assert!(txn.commit().await?.is_success());
            assert_eq!(
                f.bookmarks_1
                    .get_raw(f.ctx.clone(), &first, Freshness::MostRecent)
                    .await?,
                Some((TWOS_CSID, Some(2)))
            );
            assert_eq!(
                f.bookmarks_1
                    .get_raw(f.ctx.clone(), &second, Freshness::MostRecent)
                    .await?,
                Some((TWOS_CSID, Some(3)))
            );
            assert_eq!(
                f.bookmarks_2
                    .get_raw(f.ctx.clone(), &first, Freshness::MostRecent)
                    .await?,
                Some((TWOS_CSID, Some(2)))
            );
            let mut failed = f.multi_txn();
            failed.update(
                f.repo_id_1,
                &first,
                THREES_CSID,
                TWOS_CSID,
                BookmarkUpdateReason::TestMove,
            )?;
            failed.update(
                f.repo_id_2,
                &first,
                THREES_CSID,
                ONES_CSID,
                BookmarkUpdateReason::TestMove,
            )?;
            assert!(!failed.commit().await?.is_success());
            assert_eq!(
                f.get_bookmark(&f.bookmarks_1, &first).await?,
                Some(TWOS_CSID)
            );
            let guards =
                FindMaxBookmarkLogLockId::query(&f.conn, f.ctx.sql_query_telemetry(), &f.repo_id_1)
                    .await?;
            assert_eq!(guards[0].0, Some(2));
            Ok(())
        }
        .boxed(),
    )
    .await
}

#[mononoke::fbinit_test]
async fn name_ids_and_repo_tickets_survive_interleaved_log_chunks(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    with_just_knobs_async(
        by_name_knobs(),
        async {
            let mut txn = f.multi_txn();
            for index in 0..110 {
                let key = BookmarkKey::new(format!("heads/key-{index:03}"))?;
                for repo_id in [f.repo_id_1, f.repo_id_2] {
                    txn.create(repo_id, &key, ONES_CSID, BookmarkUpdateReason::TestMove)?;
                }
            }
            assert!(txn.commit().await?.is_success());
            for (repo_id, store) in [(f.repo_id_1, &f.bookmarks_1), (f.repo_id_2, &f.bookmarks_2)] {
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
                let guards =
                    FindMaxBookmarkLogLockId::query(&f.conn, f.ctx.sql_query_telemetry(), &repo_id)
                        .await?;
                assert_eq!(guards[0].0, Some(110));
            }
            Ok(())
        }
        .boxed(),
    )
    .await
}

#[mononoke::fbinit_test]
async fn name_allocation_requires_guard_activation(fb: FacebookInit) -> Result<()> {
    for retired in [false, true] {
        let f = TwoRepoFixture::new(fb)?;
        let knobs = JustKnobsInMemory::new(HashMap::from([
            (
                "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_guard_enabled".to_owned(),
                KnobVal::Bool(false),
            ),
            (
                "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_ids_by_name".to_owned(),
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
                let mut txn = f.multi_txn();
                for repo_id in [f.repo_id_1, f.repo_id_2] {
                    txn.create(repo_id, &key, ONES_CSID, BookmarkUpdateReason::TestMove)?;
                }
                let error = txn
                    .commit()
                    .await
                    .err()
                    .expect("name allocation needs activation");
                assert!(format!("{error:#}").contains("requires migration guard activation"));
                for store in [&f.bookmarks_1, &f.bookmarks_2] {
                    assert_eq!(f.get_bookmark(store, &key).await?, None);
                    let entries: Vec<_> = store
                        .read_next_bookmark_log_entries(
                            f.ctx.clone(),
                            BookmarkUpdateLogId(0),
                            10,
                            Freshness::MostRecent,
                        )
                        .try_collect()
                        .await?;
                    assert!(entries.is_empty());
                }
                Ok::<_, Error>(())
            }
            .boxed(),
        )
        .await?;
    }
    Ok(())
}
