/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

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
use mononoke_types::RepositoryId;
use mononoke_types_mocks::changesetid::ONES_CSID;
use mononoke_types_mocks::changesetid::THREES_CSID;
use mononoke_types_mocks::changesetid::TWOS_CSID;
use sql_construct::SqlConstruct;

use super::MultiRepoBookmarksTransaction;

#[mononoke::fbinit_test]
async fn mixed_repo_allocation_modes_commit_and_roll_back_atomically(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    for name_repo in 0..2 {
        let builder = SqlBookmarksBuilder::with_sqlite_in_memory()?;
        let repo_ids = [RepositoryId::new(1), RepositoryId::new(2)];
        let stores = repo_ids.map(|repo_id| builder.clone().with_repo_id(repo_id));
        let keys = [
            BookmarkKey::new("heads/a")?,
            BookmarkKey::new("heads/b")?,
            BookmarkKey::new("heads/new")?,
        ];
        for store in &stores {
            let mut seed = store.create_transaction(ctx.clone());
            for key in &keys[..2] {
                seed.force_set(key, ONES_CSID, BookmarkUpdateReason::TestMove)?;
            }
            assert_eq!(seed.commit().await?, Some(1));
        }
        let conn = stores[0].write_connection().clone();
        let knobs = JustKnobsInMemory::new(HashMap::from([(
            "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_guard_enabled".to_owned(),
            KnobVal::Bool(true),
        )]));
        with_just_knobs_async(
            knobs,
            async {
                // The in-memory JK fixture ignores repo switches. Inject only the
                // mode lookup; use the real commit/retry, SQL, guard and CAS path.
                let reads = [AtomicUsize::new(0), AtomicUsize::new(0)];
                let mode = |repo_id| {
                    let index = repo_ids
                        .iter()
                        .position(|id| *id == repo_id)
                        .expect("known repo");
                    reads[index].fetch_add(1, Ordering::Relaxed);
                    index == name_repo
                };
                let mut txn = MultiRepoBookmarksTransaction::new(ctx.clone(), conn.clone());
                for repo_id in repo_ids {
                    for key in &keys[..2] {
                        txn.update(
                            repo_id,
                            key,
                            TWOS_CSID,
                            ONES_CSID,
                            BookmarkUpdateReason::TestMove,
                        )?;
                    }
                    txn.create(repo_id, &keys[2], TWOS_CSID, BookmarkUpdateReason::TestMove)?;
                }
                assert!(
                    txn.commit_with_hooks_and_mode(Vec::new(), &mode)
                        .await?
                        .is_success()
                );
                let expected = std::array::from_fn::<_, 2, _>(|index| {
                    if index == name_repo {
                        [2, 3, 1]
                    } else {
                        [3, 4, 5]
                    }
                });
                for (index, store) in stores.iter().enumerate() {
                    assert_eq!(
                        reads[index].load(Ordering::Relaxed),
                        1,
                        "mode latched per repo"
                    );
                    for (key, id) in keys.iter().zip(expected[index]) {
                        assert_eq!(
                            store
                                .get_raw(ctx.clone(), key, Freshness::MostRecent)
                                .await?,
                            Some((TWOS_CSID, Some(id)))
                        );
                    }
                    let guards = FindMaxBookmarkLogLockId::query(
                        &conn,
                        ctx.sql_query_telemetry(),
                        &repo_ids[index],
                    )
                    .await?;
                    assert_eq!(guards, vec![(Some(3),)], "three guarded rows in each repo");
                }
                for losing_repo in 0..2 {
                    let mut failed = MultiRepoBookmarksTransaction::new(ctx.clone(), conn.clone());
                    for (index, repo_id) in repo_ids.iter().enumerate() {
                        failed.update(
                            *repo_id,
                            &keys[0],
                            THREES_CSID,
                            if index == losing_repo {
                                ONES_CSID
                            } else {
                                TWOS_CSID
                            },
                            BookmarkUpdateReason::TestMove,
                        )?;
                    }
                    assert!(
                        !failed
                            .commit_with_hooks_and_mode(Vec::new(), &mode)
                            .await?
                            .is_success()
                    );
                    for (index, store) in stores.iter().enumerate() {
                        for (key, id) in keys.iter().zip(expected[index]) {
                            assert_eq!(
                                store
                                    .get_raw(ctx.clone(), key, Freshness::MostRecent)
                                    .await?,
                                Some((TWOS_CSID, Some(id))),
                                "a CAS failure in either mode rolls back both repositories"
                            );
                        }
                        for (key, ids) in keys.iter().zip([
                            vec![1, expected[index][0]],
                            vec![2, expected[index][1]],
                            vec![expected[index][2]],
                        ]) {
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
                                ids
                            );
                            assert_eq!(
                                entries.last().expect("committed log").to_changeset_id,
                                Some(TWOS_CSID)
                            );
                        }
                        let guards = FindMaxBookmarkLogLockId::query(
                            &conn,
                            ctx.sql_query_telemetry(),
                            &repo_ids[index],
                        )
                        .await?;
                        assert_eq!(
                            guards,
                            vec![(Some(3),)],
                            "failed attempts commit no tickets"
                        );
                    }
                }
                for read in &reads {
                    assert_eq!(
                        read.load(Ordering::Relaxed),
                        3,
                        "one mode read per repo per attempt"
                    );
                }
                Ok::<_, Error>(())
            }
            .boxed(),
        )
        .await?;
    }
    Ok(())
}
