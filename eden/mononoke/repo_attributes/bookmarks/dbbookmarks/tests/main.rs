/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Tests for the Bookmarks store.

mod log_allocation;
mod log_lock;
mod migration_controls;
mod migration_primitives;

use std::collections::HashMap;

use anyhow::Error;
use anyhow::Result;
use bookmarks::Bookmark;
use bookmarks::BookmarkCategory;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkKind;
use bookmarks::BookmarkMoveAlreadyProcessed;
use bookmarks::BookmarkPagination;
use bookmarks::BookmarkPrefix;
use bookmarks::BookmarkUpdateLog;
use bookmarks::BookmarkUpdateLogEntry;
use bookmarks::BookmarkUpdateLogId;
use bookmarks::BookmarkUpdateReason;
use bookmarks::Bookmarks;
use bookmarks::Freshness;
use bookmarks::MirrorBookmarkMove;
use context::CoreContext;
use dbbookmarks::SqlBookmarksBuilder;
use fbinit::FacebookInit;
use futures::stream::TryStreamExt;
use maplit::hashmap;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::Timestamp;
use mononoke_types_mocks::changesetid::FIVES_CSID;
use mononoke_types_mocks::changesetid::FOURS_CSID;
use mononoke_types_mocks::changesetid::ONES_CSID;
use mononoke_types_mocks::changesetid::SIXES_CSID;
use mononoke_types_mocks::changesetid::THREES_CSID;
use mononoke_types_mocks::changesetid::TWOS_CSID;
use mononoke_types_mocks::repo::REPO_ONE;
use mononoke_types_mocks::repo::REPO_TWO;
use mononoke_types_mocks::repo::REPO_ZERO;
use quickcheck_arbitrary_derive::Arbitrary;
use sql::mysql_async::Value;
use sql_construct::SqlConstruct;

fn create_bookmark_name(book: &str) -> BookmarkKey {
    BookmarkKey::new(book).unwrap()
}

fn create_prefix(book: &str) -> BookmarkPrefix {
    BookmarkPrefix::new(book).unwrap()
}

fn compare_log_entries(
    expected_entries: Vec<BookmarkUpdateLogEntry>,
    actual_entries: Vec<BookmarkUpdateLogEntry>,
) {
    assert_eq!(expected_entries.len(), actual_entries.len());
    for i in 0..expected_entries.len() {
        let expected = expected_entries.get(i).unwrap();
        let actual = actual_entries.get(i).unwrap();
        assert_eq!(expected.id, actual.id);
        assert_eq!(expected.repo_id, actual.repo_id);
        assert_eq!(expected.bookmark_name, actual.bookmark_name);
        assert_eq!(expected.to_changeset_id, actual.to_changeset_id);
        assert_eq!(expected.from_changeset_id, actual.from_changeset_id);
        assert_eq!(expected.reason, actual.reason);
    }
}

#[mononoke::fbinit_test]
async fn test_simple_unconditional_set_get(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let name_correct = create_bookmark_name("book");
    let name_incorrect = create_bookmark_name("book2");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.force_set(&name_correct, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    assert_eq!(
        bookmarks
            .get_raw(ctx.clone(), &name_correct, bookmarks::Freshness::MostRecent)
            .await
            .unwrap(),
        Some((ONES_CSID, Some(1)))
    );
    assert_eq!(
        bookmarks
            .get_raw(
                ctx.clone(),
                &name_incorrect,
                bookmarks::Freshness::MostRecent
            )
            .await
            .unwrap(),
        None
    );

    compare_log_entries(
        bookmarks
            .read_next_bookmark_log_entries(
                ctx.clone(),
                BookmarkUpdateLogId(0),
                1,
                Freshness::MostRecent,
            )
            .try_collect::<Vec<_>>()
            .await
            .unwrap(),
        vec![BookmarkUpdateLogEntry {
            id: BookmarkUpdateLogId(1),
            repo_id: REPO_ZERO,
            bookmark_name: name_correct,
            to_changeset_id: Some(ONES_CSID),
            from_changeset_id: None,
            reason: BookmarkUpdateReason::TestMove,
            timestamp: Timestamp::now(),
        }],
    );
}

#[mononoke::fbinit_test]
async fn test_multi_unconditional_set_get(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key_1 = create_bookmark_name("book");
    let key_2 = create_bookmark_name("book2");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.force_set(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    txn.force_set(&key_2, TWOS_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    assert_eq!(
        bookmarks
            .get_raw(ctx.clone(), &key_1, bookmarks::Freshness::MostRecent)
            .await
            .unwrap(),
        Some((ONES_CSID, Some(1)))
    );

    assert_eq!(
        bookmarks
            .get_raw(ctx.clone(), &key_2, bookmarks::Freshness::MostRecent)
            .await
            .unwrap(),
        Some((TWOS_CSID, Some(2)))
    );
}

#[mononoke::fbinit_test]
async fn test_unconditional_set_same_bookmark(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key_1 = create_bookmark_name("book");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.force_set(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.force_set(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    assert_eq!(
        bookmarks
            .get_raw(ctx.clone(), &key_1, bookmarks::Freshness::MostRecent)
            .await
            .unwrap(),
        Some((ONES_CSID, Some(2)))
    );
}

#[mononoke::fbinit_test]
async fn test_simple_create(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key_1 = create_bookmark_name("book");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.create(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    assert_eq!(
        bookmarks
            .get_raw(ctx.clone(), &key_1, bookmarks::Freshness::MostRecent)
            .await
            .unwrap(),
        Some((ONES_CSID, Some(1)))
    );

    compare_log_entries(
        bookmarks
            .read_next_bookmark_log_entries(
                ctx.clone(),
                BookmarkUpdateLogId(0),
                1,
                Freshness::MostRecent,
            )
            .try_collect::<Vec<_>>()
            .await
            .unwrap(),
        vec![BookmarkUpdateLogEntry {
            id: BookmarkUpdateLogId(1),
            repo_id: REPO_ZERO,
            bookmark_name: key_1,
            to_changeset_id: Some(ONES_CSID),
            from_changeset_id: None,
            reason: BookmarkUpdateReason::TestMove,
            timestamp: Timestamp::now(),
        }],
    );
}

#[mononoke::fbinit_test]
async fn test_create_already_existing(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key_1 = create_bookmark_name("book");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.create(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.create(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_none());
}

#[mononoke::fbinit_test]
async fn test_create_change_same_bookmark(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key_1 = create_bookmark_name("book");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.create(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(
        txn.force_set(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
            .is_err()
    );

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.force_set(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(
        txn.create(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
            .is_err()
    );

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.force_set(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(
        txn.update(&key_1, TWOS_CSID, ONES_CSID, BookmarkUpdateReason::TestMove,)
            .is_err()
    );

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(&key_1, TWOS_CSID, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(
        txn.force_set(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
            .is_err()
    );

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(&key_1, TWOS_CSID, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(
        txn.force_delete(&key_1, BookmarkUpdateReason::TestMove)
            .is_err()
    );

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.force_delete(&key_1, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(
        txn.update(&key_1, TWOS_CSID, ONES_CSID, BookmarkUpdateReason::TestMove,)
            .is_err()
    );

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.delete(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(
        txn.update(&key_1, TWOS_CSID, ONES_CSID, BookmarkUpdateReason::TestMove,)
            .is_err()
    );

    let mut txn = bookmarks.create_transaction(ctx);
    txn.update(&key_1, TWOS_CSID, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(
        txn.delete(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
            .is_err()
    );
}

#[mononoke::fbinit_test]
async fn test_simple_update_bookmark(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key_1 = create_bookmark_name("book");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.create(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(&key_1, TWOS_CSID, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    assert_eq!(
        bookmarks
            .get_raw(ctx.clone(), &key_1, bookmarks::Freshness::MostRecent)
            .await
            .unwrap(),
        Some((TWOS_CSID, Some(2)))
    );

    compare_log_entries(
        bookmarks
            .read_next_bookmark_log_entries(
                ctx.clone(),
                BookmarkUpdateLogId(1),
                1,
                Freshness::MostRecent,
            )
            .try_collect::<Vec<_>>()
            .await
            .unwrap(),
        vec![BookmarkUpdateLogEntry {
            id: BookmarkUpdateLogId(2),
            repo_id: REPO_ZERO,
            bookmark_name: key_1,
            to_changeset_id: Some(TWOS_CSID),
            from_changeset_id: Some(ONES_CSID),
            reason: BookmarkUpdateReason::TestMove,
            timestamp: Timestamp::now(),
        }],
    );
}

#[mononoke::fbinit_test]
async fn test_noop_update(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key_1 = create_bookmark_name("book");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.create(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(&key_1, ONES_CSID, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    assert_eq!(
        bookmarks
            .get_raw(ctx.clone(), &key_1, bookmarks::Freshness::MostRecent)
            .await
            .unwrap(),
        Some((ONES_CSID, Some(2)))
    );
}

#[mononoke::fbinit_test]
async fn test_scratch_update_bookmark(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key_1 = create_bookmark_name("book");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.create_scratch(&key_1, ONES_CSID).unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update_scratch(&key_1, TWOS_CSID, ONES_CSID).unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    assert_eq!(
        bookmarks
            .get_raw(ctx.clone(), &key_1, bookmarks::Freshness::MostRecent)
            .await
            .unwrap(),
        Some((TWOS_CSID, None))
    );

    compare_log_entries(
        bookmarks
            .read_next_bookmark_log_entries(
                ctx.clone(),
                BookmarkUpdateLogId(1),
                1,
                Freshness::MostRecent,
            )
            .try_collect::<Vec<_>>()
            .await
            .unwrap(),
        vec![],
    );
}

#[mononoke::fbinit_test]
async fn test_update_non_existent_bookmark(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key_1 = create_bookmark_name("book");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(&key_1, TWOS_CSID, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_none());
}

#[mononoke::fbinit_test]
async fn test_mirror_batch_create_and_update_chain(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key = create_bookmark_name("book");

    // A brand-new bookmark. The first move creates it (old = None); the next two
    // moves update it. The store reuses the source log ids 1, 2, 3.
    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.mirror_batch(
        &key,
        BookmarkKind::PullDefaultPublishing,
        vec![
            MirrorBookmarkMove {
                log_id: 1,
                old: None,
                new: ONES_CSID,
                reason: BookmarkUpdateReason::TestMove,
            },
            MirrorBookmarkMove {
                log_id: 2,
                old: Some(ONES_CSID),
                new: TWOS_CSID,
                reason: BookmarkUpdateReason::TestMove,
            },
            MirrorBookmarkMove {
                log_id: 3,
                old: Some(TWOS_CSID),
                new: THREES_CSID,
                reason: BookmarkUpdateReason::TestMove,
            },
        ],
    )
    .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    // The bookmark points to the last move's changeset and carries the last
    // move's log id.
    assert_eq!(
        bookmarks
            .get_raw(ctx.clone(), &key, bookmarks::Freshness::MostRecent)
            .await
            .unwrap(),
        Some((THREES_CSID, Some(3)))
    );

    // One log row per move, each reusing the source log id, changesets, and
    // reason.
    compare_log_entries(
        bookmarks
            .read_next_bookmark_log_entries(
                ctx.clone(),
                BookmarkUpdateLogId(0),
                10,
                Freshness::MostRecent,
            )
            .try_collect::<Vec<_>>()
            .await
            .unwrap(),
        vec![
            BookmarkUpdateLogEntry {
                id: BookmarkUpdateLogId(1),
                repo_id: REPO_ZERO,
                bookmark_name: key.clone(),
                to_changeset_id: Some(ONES_CSID),
                from_changeset_id: None,
                reason: BookmarkUpdateReason::TestMove,
                timestamp: Timestamp::now(),
            },
            BookmarkUpdateLogEntry {
                id: BookmarkUpdateLogId(2),
                repo_id: REPO_ZERO,
                bookmark_name: key.clone(),
                to_changeset_id: Some(TWOS_CSID),
                from_changeset_id: Some(ONES_CSID),
                reason: BookmarkUpdateReason::TestMove,
                timestamp: Timestamp::now(),
            },
            BookmarkUpdateLogEntry {
                id: BookmarkUpdateLogId(3),
                repo_id: REPO_ZERO,
                bookmark_name: key,
                to_changeset_id: Some(THREES_CSID),
                from_changeset_id: Some(TWOS_CSID),
                reason: BookmarkUpdateReason::TestMove,
                timestamp: Timestamp::now(),
            },
        ],
    );
}

#[mononoke::fbinit_test]
async fn test_mirror_batch_idempotent_replay(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key = create_bookmark_name("book");

    let create_move = MirrorBookmarkMove {
        log_id: 1,
        old: None,
        new: ONES_CSID,
        reason: BookmarkUpdateReason::TestMove,
    };
    let update_move = MirrorBookmarkMove {
        log_id: 2,
        old: Some(ONES_CSID),
        new: TWOS_CSID,
        reason: BookmarkUpdateReason::TestMove,
    };

    // Apply the chain once.
    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.mirror_batch(
        &key,
        BookmarkKind::PullDefaultPublishing,
        vec![create_move.clone(), update_move.clone()],
    )
    .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    // Replay the identical batch (a lost-ack replay). Every move's log id is
    // already stored, so the store applies nothing and reports
    // AlreadyProcessed. This is the path that increments the
    // bookmarks_insert_already_processed counter.
    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.mirror_batch(
        &key,
        BookmarkKind::PullDefaultPublishing,
        vec![create_move.clone(), update_move.clone()],
    )
    .unwrap();
    let err = txn.commit().await.unwrap_err();
    assert!(err.is::<BookmarkMoveAlreadyProcessed>());

    // The replay left the bookmark untouched.
    assert_eq!(
        bookmarks
            .get_raw(ctx.clone(), &key, bookmarks::Freshness::MostRecent)
            .await
            .unwrap(),
        Some((TWOS_CSID, Some(2)))
    );

    // A retry that regroups the seen moves with one new move applies only the
    // unseen suffix, reusing its source log id.
    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.mirror_batch(
        &key,
        BookmarkKind::PullDefaultPublishing,
        vec![
            create_move,
            update_move,
            MirrorBookmarkMove {
                log_id: 3,
                old: Some(TWOS_CSID),
                new: THREES_CSID,
                reason: BookmarkUpdateReason::TestMove,
            },
        ],
    )
    .unwrap();
    assert!(txn.commit().await.unwrap().is_some());
    assert_eq!(
        bookmarks
            .get_raw(ctx.clone(), &key, bookmarks::Freshness::MostRecent)
            .await
            .unwrap(),
        Some((THREES_CSID, Some(3)))
    );
}

#[mononoke::fbinit_test]
async fn test_mirror_batch_replay_of_applied_prefix(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key = create_bookmark_name("book");

    let create_move = MirrorBookmarkMove {
        log_id: 1,
        old: None,
        new: ONES_CSID,
        reason: BookmarkUpdateReason::TestMove,
    };
    let second_move = MirrorBookmarkMove {
        log_id: 2,
        old: Some(ONES_CSID),
        new: TWOS_CSID,
        reason: BookmarkUpdateReason::TestMove,
    };

    // The replica applies three moves and stops at THREES with log id 3.
    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.mirror_batch(
        &key,
        BookmarkKind::PullDefaultPublishing,
        vec![
            create_move.clone(),
            second_move.clone(),
            MirrorBookmarkMove {
                log_id: 3,
                old: Some(TWOS_CSID),
                new: THREES_CSID,
                reason: BookmarkUpdateReason::TestMove,
            },
        ],
    )
    .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    // A retry regroups the seen moves into a shorter batch that stops before
    // the replica's current move. Every log id is already stored, so the store
    // reports AlreadyProcessed. The replica's changeset is expected to differ
    // from this batch's last move, so it must not count as divergence.
    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.mirror_batch(
        &key,
        BookmarkKind::PullDefaultPublishing,
        vec![create_move, second_move],
    )
    .unwrap();
    let err = txn.commit().await.unwrap_err();
    assert!(err.is::<BookmarkMoveAlreadyProcessed>());

    // The replay left the bookmark untouched.
    assert_eq!(
        bookmarks
            .get_raw(ctx.clone(), &key, bookmarks::Freshness::MostRecent)
            .await
            .unwrap(),
        Some((THREES_CSID, Some(3)))
    );
}

#[mononoke::fbinit_test]
async fn test_mirror_batch_concurrent_duplicate_replays(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key = create_bookmark_name("book");

    // Pre-create the bookmark so the racing batches take the update branch
    // (their first unseen move has `old` set), the path the pessimistic lock
    // covers. Concurrent bookmark creations serialize differently: the loser
    // of the INSERT re-reads and treats a fully-applied batch as a replay
    // (see store_mirror_batches).
    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.mirror_batch(
        &key,
        BookmarkKind::PullDefaultPublishing,
        vec![MirrorBookmarkMove {
            log_id: 1,
            old: None,
            new: ONES_CSID,
            reason: BookmarkUpdateReason::TestMove,
        }],
    )
    .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    let moves = vec![
        MirrorBookmarkMove {
            log_id: 2,
            old: Some(ONES_CSID),
            new: TWOS_CSID,
            reason: BookmarkUpdateReason::TestMove,
        },
        MirrorBookmarkMove {
            log_id: 3,
            old: Some(TWOS_CSID),
            new: THREES_CSID,
            reason: BookmarkUpdateReason::TestMove,
        },
    ];

    // Two concurrent replays of the identical chain: a retry racing the
    // attempt it replaces. One wins; the other must report AlreadyProcessed,
    // never a logic error (a silent None). This pins the serialized outcome
    // only: the single SQLite connection runs the two commits one after
    // another, and SQLite uses the plain SELECT anyway, so it cannot
    // exercise MySQL's row-lock concurrency. On MySQL the pessimistic lock in
    // store_mirror_batches serializes the two and the loser reads the
    // winner's commit. No MySQL-backed fixture exists for these tests.
    let mut txn_a = bookmarks.create_transaction(ctx.clone());
    txn_a
        .mirror_batch(&key, BookmarkKind::PullDefaultPublishing, moves.clone())
        .unwrap();
    let mut txn_b = bookmarks.create_transaction(ctx.clone());
    txn_b
        .mirror_batch(&key, BookmarkKind::PullDefaultPublishing, moves)
        .unwrap();
    let (res_a, res_b) = futures::join!(txn_a.commit(), txn_b.commit());

    let mut saw_success = false;
    let mut saw_already_processed = false;
    for res in [res_a, res_b] {
        match res {
            Ok(Some(_)) => saw_success = true,
            Err(err) if err.is::<BookmarkMoveAlreadyProcessed>() => {
                saw_already_processed = true;
            }
            other => panic!("unexpected concurrent replay outcome: {other:?}"),
        }
    }
    assert!(saw_success && saw_already_processed);
}

#[mononoke::fbinit_test]
async fn test_mirror_batch_concurrent_bookmark_creations(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key = create_bookmark_name("book");

    // No pre-create: the first move (old = None) takes the bookmark-creation
    // INSERT branch, the path concurrent bookmark creations race on.
    let moves = vec![
        MirrorBookmarkMove {
            log_id: 1,
            old: None,
            new: ONES_CSID,
            reason: BookmarkUpdateReason::TestMove,
        },
        MirrorBookmarkMove {
            log_id: 2,
            old: Some(ONES_CSID),
            new: TWOS_CSID,
            reason: BookmarkUpdateReason::TestMove,
        },
    ];

    // Two duplicate bookmark-creation batches. The winner creates the bookmark; the
    // loser must report AlreadyProcessed, never a logic error. This pins
    // the outcome only: SQLite runs the two commits one after another, so
    // the loser reads the winner's commit and takes the already-applied
    // path. Only truly concurrent transactions (MySQL) can lose the INSERT;
    // that re-check decision is covered by unit tests in transaction.rs.
    let mut txn_a = bookmarks.create_transaction(ctx.clone());
    txn_a
        .mirror_batch(&key, BookmarkKind::PullDefaultPublishing, moves.clone())
        .unwrap();
    assert!(txn_a.commit().await.unwrap().is_some());

    let mut txn_b = bookmarks.create_transaction(ctx.clone());
    txn_b
        .mirror_batch(&key, BookmarkKind::PullDefaultPublishing, moves)
        .unwrap();
    let err = txn_b.commit().await.unwrap_err();
    assert!(err.is::<BookmarkMoveAlreadyProcessed>());
}

#[mononoke::fbinit_test]
async fn test_mirror_batch_diverged_replica_logic_error(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key = create_bookmark_name("book");

    // The replica sits at ONES with log id 1.
    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.mirror_batch(
        &key,
        BookmarkKind::PullDefaultPublishing,
        vec![MirrorBookmarkMove {
            log_id: 1,
            old: None,
            new: ONES_CSID,
            reason: BookmarkUpdateReason::TestMove,
        }],
    )
    .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    // A later move whose old changeset (TWOS) does not match the replica's
    // current changeset (ONES). The compare-and-swap matches no row, so the
    // store reports a logic error and the commit returns None.
    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.mirror_batch(
        &key,
        BookmarkKind::PullDefaultPublishing,
        vec![MirrorBookmarkMove {
            log_id: 2,
            old: Some(TWOS_CSID),
            new: THREES_CSID,
            reason: BookmarkUpdateReason::TestMove,
        }],
    )
    .unwrap();
    assert!(txn.commit().await.unwrap().is_none());

    // The failed batch left the bookmark untouched.
    assert_eq!(
        bookmarks
            .get_raw(ctx.clone(), &key, bookmarks::Freshness::MostRecent)
            .await
            .unwrap(),
        Some((ONES_CSID, Some(1)))
    );
}

#[mononoke::fbinit_test]
async fn test_update_existing_bookmark_with_incorrect_commit(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key_1 = create_bookmark_name("book");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.create(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(&key_1, ONES_CSID, TWOS_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_none());
}

#[mononoke::fbinit_test]
async fn test_force_delete(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key_1 = create_bookmark_name("book");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.force_delete(&key_1, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    assert_eq!(
        bookmarks
            .get_raw(ctx.clone(), &key_1, bookmarks::Freshness::MostRecent,)
            .await
            .unwrap(),
        None
    );

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.create(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());
    assert_eq!(
        bookmarks
            .get_raw(ctx.clone(), &key_1, bookmarks::Freshness::MostRecent,)
            .await
            .unwrap(),
        Some((ONES_CSID, Some(2)))
    );

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.force_delete(&key_1, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    assert_eq!(
        bookmarks
            .get_raw(ctx.clone(), &key_1, bookmarks::Freshness::MostRecent,)
            .await
            .unwrap(),
        None
    );

    compare_log_entries(
        bookmarks
            .read_next_bookmark_log_entries(
                ctx.clone(),
                BookmarkUpdateLogId(2),
                1,
                Freshness::MostRecent,
            )
            .try_collect::<Vec<_>>()
            .await
            .unwrap(),
        vec![BookmarkUpdateLogEntry {
            id: BookmarkUpdateLogId(3),
            repo_id: REPO_ZERO,
            bookmark_name: key_1,
            to_changeset_id: None,
            from_changeset_id: None,
            reason: BookmarkUpdateReason::TestMove,
            timestamp: Timestamp::now(),
        }],
    );
}

#[mononoke::fbinit_test]
async fn test_delete(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key_1 = create_bookmark_name("book");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.delete(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_none());

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.create(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());
    assert_eq!(
        bookmarks
            .get_raw(ctx.clone(), &key_1, bookmarks::Freshness::MostRecent,)
            .await
            .unwrap(),
        Some((ONES_CSID, Some(1)))
    );

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.delete(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    compare_log_entries(
        bookmarks
            .read_next_bookmark_log_entries(
                ctx.clone(),
                BookmarkUpdateLogId(1),
                1,
                Freshness::MostRecent,
            )
            .try_collect::<Vec<_>>()
            .await
            .unwrap(),
        vec![BookmarkUpdateLogEntry {
            id: BookmarkUpdateLogId(2),
            repo_id: REPO_ZERO,
            bookmark_name: key_1,
            to_changeset_id: None,
            from_changeset_id: Some(ONES_CSID),
            reason: BookmarkUpdateReason::TestMove,
            timestamp: Timestamp::now(),
        }],
    );
}

#[mononoke::fbinit_test]
async fn test_delete_incorrect_hash(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key_1 = create_bookmark_name("book");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.create(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());
    assert_eq!(
        bookmarks
            .get_raw(ctx.clone(), &key_1, bookmarks::Freshness::MostRecent,)
            .await
            .unwrap(),
        Some((ONES_CSID, Some(1)))
    );

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.delete(&key_1, TWOS_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_none());
}

#[mononoke::fbinit_test]
async fn test_list_by_prefix(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key_1 = create_bookmark_name("book1");
    let key_2 = create_bookmark_name("book2");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.create(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    txn.create(&key_2, TWOS_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    let prefix = create_prefix("book");
    let key_1_prefix = create_prefix("book1");
    let key_2_prefix = create_prefix("book2");

    assert_eq!(
        bookmarks
            .list(
                ctx.clone(),
                Freshness::MostRecent,
                &prefix,
                BookmarkCategory::ALL,
                BookmarkKind::ALL,
                &BookmarkPagination::FromStart,
                u64::MAX
            )
            .try_collect::<HashMap<_, _>>()
            .await
            .unwrap(),
        hashmap! {
            Bookmark::new(key_1.clone(), BookmarkKind::PullDefaultPublishing) => ONES_CSID,
            Bookmark::new(key_2.clone(), BookmarkKind::PullDefaultPublishing) => TWOS_CSID
        }
    );

    assert_eq!(
        bookmarks
            .list(
                ctx.clone(),
                Freshness::MostRecent,
                &key_1_prefix,
                BookmarkCategory::ALL,
                BookmarkKind::ALL,
                &BookmarkPagination::FromStart,
                u64::MAX
            )
            .try_collect::<Vec<_>>()
            .await
            .unwrap(),
        vec![(
            Bookmark::new(key_1.clone(), BookmarkKind::PullDefaultPublishing),
            ONES_CSID
        )]
    );

    assert_eq!(
        bookmarks
            .list(
                ctx.clone(),
                Freshness::MostRecent,
                &key_2_prefix,
                BookmarkCategory::ALL,
                BookmarkKind::ALL,
                &BookmarkPagination::FromStart,
                u64::MAX
            )
            .try_collect::<Vec<_>>()
            .await
            .unwrap(),
        vec![(
            Bookmark::new(key_2.clone(), BookmarkKind::PullDefaultPublishing),
            TWOS_CSID
        )]
    );
}

#[mononoke::fbinit_test]
async fn test_create_different_repos(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let builder = SqlBookmarksBuilder::with_sqlite_in_memory().unwrap();
    let bookmarks_0 = builder.clone().with_repo_id(REPO_ZERO);
    let bookmarks_1 = builder.with_repo_id(REPO_ONE);
    let key_1 = create_bookmark_name("book");

    let mut txn = bookmarks_0.create_transaction(ctx.clone());
    txn.force_set(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.is_ok());

    // Updating value from another repo, should fail
    let mut txn = bookmarks_1.create_transaction(ctx.clone());
    txn.update(&key_1, TWOS_CSID, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_none());

    // Creating value should succeed
    let mut txn = bookmarks_1.create_transaction(ctx.clone());
    txn.create(&key_1, TWOS_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.is_ok());

    assert_eq!(
        bookmarks_0
            .get(ctx.clone(), &key_1, bookmarks::Freshness::MostRecent)
            .await
            .unwrap(),
        Some(ONES_CSID)
    );

    assert_eq!(
        bookmarks_1
            .get(ctx.clone(), &key_1, bookmarks::Freshness::MostRecent)
            .await
            .unwrap(),
        Some(TWOS_CSID)
    );

    // Force deleting should delete only from one repo
    let mut txn = bookmarks_1.create_transaction(ctx.clone());
    txn.force_delete(&key_1, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.is_ok());

    assert_eq!(
        bookmarks_0
            .get(ctx.clone(), &key_1, bookmarks::Freshness::MostRecent)
            .await
            .unwrap(),
        Some(ONES_CSID)
    );

    // delete should fail for another repo
    let mut txn = bookmarks_1.create_transaction(ctx.clone());
    txn.delete(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_none());
}

async fn fetch_single(
    fb: FacebookInit,
    bookmarks: &dyn BookmarkUpdateLog,
    id: BookmarkUpdateLogId,
) -> BookmarkUpdateLogEntry {
    let ctx = CoreContext::test_mock(fb);
    bookmarks
        .read_next_bookmark_log_entries(ctx, id, 1, Freshness::MostRecent)
        .try_collect::<Vec<_>>()
        .await
        .unwrap()
        .first()
        .unwrap()
        .clone()
}

#[mononoke::fbinit_test]
async fn test_log_correct_order(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key_1 = create_bookmark_name("book");
    let key_2 = create_bookmark_name("book2");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.force_set(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.is_ok());

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(&key_1, TWOS_CSID, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    txn.commit().await.unwrap();

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(
        &key_1,
        THREES_CSID,
        TWOS_CSID,
        BookmarkUpdateReason::TestMove,
    )
    .unwrap();
    txn.commit().await.unwrap();

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(
        &key_1,
        FOURS_CSID,
        THREES_CSID,
        BookmarkUpdateReason::TestMove,
    )
    .unwrap();
    txn.commit().await.unwrap();

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.force_set(&key_2, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.is_ok());

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(
        &key_1,
        FIVES_CSID,
        FOURS_CSID,
        BookmarkUpdateReason::TestMove,
    )
    .unwrap();
    txn.commit().await.unwrap();

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(
        &key_1,
        SIXES_CSID,
        FIVES_CSID,
        BookmarkUpdateReason::Pushrebase,
    )
    .unwrap();
    txn.commit().await.unwrap();

    let log_entry = fetch_single(fb, &bookmarks, BookmarkUpdateLogId(0)).await;
    assert_eq!(log_entry.to_changeset_id.unwrap(), ONES_CSID);

    let log_entry = fetch_single(fb, &bookmarks, BookmarkUpdateLogId(1)).await;
    assert_eq!(log_entry.to_changeset_id.unwrap(), TWOS_CSID);

    let log_entry = fetch_single(fb, &bookmarks, BookmarkUpdateLogId(2)).await;
    assert_eq!(log_entry.to_changeset_id.unwrap(), THREES_CSID);

    let log_entry = fetch_single(fb, &bookmarks, BookmarkUpdateLogId(3)).await;
    assert_eq!(log_entry.to_changeset_id.unwrap(), FOURS_CSID);

    let log_entry = fetch_single(fb, &bookmarks, BookmarkUpdateLogId(5)).await;
    assert_eq!(log_entry.to_changeset_id.unwrap(), FIVES_CSID);

    assert_eq!(
        bookmarks
            .read_next_bookmark_log_entries(
                ctx.clone(),
                BookmarkUpdateLogId(0),
                4,
                Freshness::MostRecent
            )
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .len(),
        4
    );

    assert_eq!(
        bookmarks
            .read_next_bookmark_log_entries(
                ctx.clone(),
                BookmarkUpdateLogId(0),
                8,
                Freshness::MostRecent
            )
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .len(),
        7
    );

    let entries = bookmarks
        .read_next_bookmark_log_entries(
            ctx.clone(),
            BookmarkUpdateLogId(0),
            6,
            Freshness::MostRecent,
        )
        .try_collect::<Vec<_>>()
        .await
        .unwrap();

    let cs_ids: Vec<_> = entries
        .into_iter()
        .map(|entry| entry.to_changeset_id.unwrap())
        .collect();
    assert_eq!(
        cs_ids,
        vec![
            ONES_CSID,
            TWOS_CSID,
            THREES_CSID,
            FOURS_CSID,
            ONES_CSID,
            FIVES_CSID
        ]
    );

    let entries = bookmarks
        .read_next_bookmark_log_entries_same_bookmark_and_reason(
            ctx.clone(),
            BookmarkUpdateLogId(0),
            6,
        )
        .try_collect::<Vec<_>>()
        .await
        .unwrap();

    // FOURS_CSID -> FIVES_CSID update is missing, because it has a different bookmark
    let cs_ids: Vec<_> = entries
        .into_iter()
        .map(|entry| entry.to_changeset_id.unwrap())
        .collect();
    assert_eq!(cs_ids, vec![ONES_CSID, TWOS_CSID, THREES_CSID, FOURS_CSID]);

    let entries = bookmarks
        .read_next_bookmark_log_entries_same_bookmark_and_reason(
            ctx.clone(),
            BookmarkUpdateLogId(5),
            6,
        )
        .try_collect::<Vec<_>>()
        .await
        .unwrap();

    // FIVES_CSID -> SIXES_CSID update is missing, because it has a different reason
    let cs_ids: Vec<_> = entries
        .into_iter()
        .map(|entry| entry.to_changeset_id.unwrap())
        .collect();
    assert_eq!(cs_ids, vec![FIVES_CSID]);

    let entries = bookmarks
        .read_next_bookmark_log_entries_by_bookmark(
            ctx.clone(),
            key_1.clone(),
            BookmarkUpdateLogId(0),
            10,
            Freshness::MostRecent,
        )
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.to_changeset_id.unwrap())
            .collect::<Vec<_>>(),
        vec![
            ONES_CSID,
            TWOS_CSID,
            THREES_CSID,
            FOURS_CSID,
            FIVES_CSID,
            SIXES_CSID,
        ]
    );
    assert!(
        entries
            .windows(2)
            .all(|entries| entries[0].id < entries[1].id)
    );

    let entries = bookmarks
        .read_next_bookmark_log_entries_by_bookmark(
            ctx.clone(),
            key_1.clone(),
            BookmarkUpdateLogId(3),
            2,
            Freshness::MostRecent,
        )
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(
        entries
            .into_iter()
            .map(|entry| entry.to_changeset_id.unwrap())
            .collect::<Vec<_>>(),
        vec![FOURS_CSID, FIVES_CSID]
    );
}

#[mononoke::fbinit_test]
async fn test_read_deleted_log_entries_by_prefix(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let prefixed_1 = create_bookmark_name("small/one");
    let prefixed_2 = create_bookmark_name("small/two");
    let escaped_prefix = create_bookmark_name("small_/escaped");
    let unrelated = create_bookmark_name("other/one");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.force_set(&prefixed_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    txn.force_set(&prefixed_2, TWOS_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    txn.force_set(&escaped_prefix, THREES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    txn.force_set(&unrelated, FOURS_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    txn.commit().await.unwrap();

    for bookmark in [&prefixed_1, &unrelated, &prefixed_2, &escaped_prefix] {
        let mut txn = bookmarks.create_transaction(ctx.clone());
        txn.force_delete(bookmark, BookmarkUpdateReason::TestMove)
            .unwrap();
        txn.commit().await.unwrap();
    }

    let entries = bookmarks
        .read_next_deleted_bookmark_log_entries_by_prefix(
            ctx.clone(),
            create_prefix("small/"),
            BookmarkUpdateLogId(0),
            BookmarkUpdateLogId(100),
            10,
            Freshness::MostRecent,
        )
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.bookmark_name.clone())
            .collect::<Vec<_>>(),
        vec![prefixed_1.clone(), prefixed_2.clone()]
    );
    assert!(entries.iter().all(|entry| entry.to_changeset_id.is_none()));

    let bounded_entries = bookmarks
        .read_next_deleted_bookmark_log_entries_by_prefix(
            ctx.clone(),
            create_prefix("small/"),
            BookmarkUpdateLogId(0),
            entries[0].id,
            10,
            Freshness::MostRecent,
        )
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(bounded_entries.len(), 1);
    assert_eq!(bounded_entries[0].bookmark_name, prefixed_1);

    let entries = bookmarks
        .read_next_deleted_bookmark_log_entries_by_prefix(
            ctx.clone(),
            create_prefix("small/"),
            entries[0].id,
            BookmarkUpdateLogId(100),
            1,
            Freshness::MostRecent,
        )
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].bookmark_name, prefixed_2);

    let entries = bookmarks
        .read_next_deleted_bookmark_log_entries_by_prefix(
            ctx,
            create_prefix("small_/"),
            BookmarkUpdateLogId(0),
            BookmarkUpdateLogId(100),
            10,
            Freshness::MostRecent,
        )
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].bookmark_name, escaped_prefix);
}

#[mononoke::fbinit_test]
async fn test_read_log_entry_many_repos(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let builder = SqlBookmarksBuilder::with_sqlite_in_memory().unwrap();
    let bookmarks_0 = builder.clone().with_repo_id(REPO_ZERO);
    let bookmarks_1 = builder.clone().with_repo_id(REPO_ONE);
    let bookmarks_2 = builder.with_repo_id(REPO_TWO);
    let key_1 = create_bookmark_name("book");

    let mut txn = bookmarks_0.create_transaction(ctx.clone());
    txn.force_set(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.is_ok());

    let mut txn = bookmarks_1.create_transaction(ctx.clone());
    txn.force_set(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.is_ok());

    assert_eq!(
        bookmarks_0
            .read_next_bookmark_log_entries(
                ctx.clone(),
                BookmarkUpdateLogId(0),
                1,
                Freshness::MostRecent
            )
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .len(),
        1
    );

    assert_eq!(
        bookmarks_1
            .read_next_bookmark_log_entries(
                ctx.clone(),
                BookmarkUpdateLogId(0),
                1,
                Freshness::MostRecent
            )
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .len(),
        1
    );

    assert_eq!(
        bookmarks_0
            .read_next_bookmark_log_entries(
                ctx.clone(),
                BookmarkUpdateLogId(1),
                1,
                Freshness::MostRecent
            )
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .len(),
        0
    );

    assert_eq!(
        bookmarks_2
            .read_next_bookmark_log_entries(
                ctx.clone(),
                BookmarkUpdateLogId(0),
                1,
                Freshness::MostRecent
            )
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .len(),
        0
    );
}

#[mononoke::test]
fn test_update_reason_conversion() -> Result<(), Error> {
    use BookmarkUpdateReason::*;
    let unusedreason = TestMove;

    // If you are adding a new variant here, please also add a test
    // for the new bookmark reason.
    match unusedreason {
        Backsyncer => {}
        Blobimport => {}
        ManualMove => {}
        MultiRepoLand => {}
        Push => {}
        Pushrebase => {}
        TestMove => {}
        XRepoSync => {}
        ApiRequest => {}
    };

    let reasons = vec![
        Backsyncer,
        Blobimport,
        ManualMove,
        MultiRepoLand,
        Push,
        Pushrebase,
        TestMove,
        XRepoSync,
        ApiRequest,
    ];

    for reason in reasons {
        let value = Value::from(reason);
        BookmarkUpdateReason::try_from(value)?;
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_list_bookmark_log_entries(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key_1 = create_bookmark_name("book");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.force_set(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.is_ok());

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(&key_1, TWOS_CSID, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    txn.commit().await.unwrap();

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(
        &key_1,
        THREES_CSID,
        TWOS_CSID,
        BookmarkUpdateReason::TestMove,
    )
    .unwrap();
    txn.commit().await.unwrap();

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(
        &key_1,
        FOURS_CSID,
        THREES_CSID,
        BookmarkUpdateReason::TestMove,
    )
    .unwrap();
    txn.commit().await.unwrap();

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(
        &key_1,
        FIVES_CSID,
        FOURS_CSID,
        BookmarkUpdateReason::TestMove,
    )
    .unwrap();
    txn.commit().await.unwrap();

    assert_eq!(
        bookmarks
            .list_bookmark_log_entries(ctx.clone(), key_1.clone(), 3, None, Freshness::MostRecent)
            .map_ok(|(_id, cs, rs, _ts)| (cs, rs))
            .try_collect::<Vec<_>>()
            .await
            .unwrap(),
        vec![
            (Some(FIVES_CSID), BookmarkUpdateReason::TestMove),
            (Some(FOURS_CSID), BookmarkUpdateReason::TestMove),
            (Some(THREES_CSID), BookmarkUpdateReason::TestMove),
        ]
    );

    let current_timestamp = Timestamp::now();
    let day_old_timestamp =
        Timestamp::from_timestamp_secs(current_timestamp.timestamp_seconds() - 86400);
    assert_eq!(
        bookmarks
            .list_bookmark_log_entries_ts_in_range(
                ctx.clone(),
                key_1.clone(),
                3,
                day_old_timestamp,
                current_timestamp,
            )
            .map_ok(|(_id, cs, rs, _ts)| (cs, rs))
            .try_collect::<Vec<_>>()
            .await
            .unwrap(),
        vec![
            (Some(FIVES_CSID), BookmarkUpdateReason::TestMove),
            (Some(FOURS_CSID), BookmarkUpdateReason::TestMove),
            (Some(THREES_CSID), BookmarkUpdateReason::TestMove),
        ]
    );

    assert_eq!(
        bookmarks
            .list_bookmark_log_entries(ctx.clone(), key_1, 3, Some(1), Freshness::MostRecent)
            .map_ok(|(_id, cs, rs, _ts)| (cs, rs))
            .try_collect::<Vec<_>>()
            .await
            .unwrap(),
        vec![
            (Some(FOURS_CSID), BookmarkUpdateReason::TestMove),
            (Some(THREES_CSID), BookmarkUpdateReason::TestMove),
            (Some(TWOS_CSID), BookmarkUpdateReason::TestMove),
        ]
    );
}

#[mononoke::fbinit_test]
async fn test_get_largest_log_id(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key_1 = create_bookmark_name("book");

    assert_eq!(
        bookmarks
            .get_largest_log_id(ctx.clone(), Freshness::MostRecent)
            .await
            .unwrap(),
        None
    );
    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.force_set(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();

    assert!(txn.commit().await.is_ok());
    assert_eq!(
        bookmarks
            .get_largest_log_id(ctx.clone(), Freshness::MostRecent)
            .await
            .unwrap(),
        Some(1)
    );

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(&key_1, TWOS_CSID, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    txn.commit().await.unwrap();

    assert_eq!(
        bookmarks
            .get_largest_log_id(ctx.clone(), Freshness::MostRecent)
            .await
            .unwrap(),
        Some(2)
    );

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(
        &key_1,
        THREES_CSID,
        TWOS_CSID,
        BookmarkUpdateReason::TestMove,
    )
    .unwrap();
    txn.commit().await.unwrap();

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(
        &key_1,
        FOURS_CSID,
        THREES_CSID,
        BookmarkUpdateReason::TestMove,
    )
    .unwrap();
    txn.commit().await.unwrap();

    assert_eq!(
        bookmarks
            .get_largest_log_id(ctx.clone(), Freshness::MostRecent)
            .await
            .unwrap(),
        Some(4)
    );
}

#[mononoke::fbinit_test]
async fn test_creating_publishing_bookmarks(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key_1 = create_bookmark_name("book");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.create_publishing(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());
    assert_eq!(
        bookmarks
            .list(
                ctx.clone(),
                Freshness::MostRecent,
                &BookmarkPrefix::empty(),
                BookmarkCategory::ALL,
                BookmarkKind::ALL,
                &BookmarkPagination::FromStart,
                u64::MAX
            )
            .try_collect::<HashMap<_, _>>()
            .await
            .unwrap(),
        hashmap! {
            Bookmark::new(key_1.clone(), BookmarkKind::Publishing) => ONES_CSID,
        }
    );

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(&key_1, TWOS_CSID, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    assert_eq!(
        bookmarks
            .list(
                ctx.clone(),
                Freshness::MostRecent,
                &BookmarkPrefix::empty(),
                BookmarkCategory::ALL,
                BookmarkKind::ALL,
                &BookmarkPagination::FromStart,
                u64::MAX
            )
            .try_collect::<HashMap<_, _>>()
            .await
            .unwrap(),
        hashmap! {
            Bookmark::new(key_1.clone(), BookmarkKind::Publishing) => TWOS_CSID,
        }
    );
}

#[mononoke::fbinit_test]
async fn test_pagination_ordering(fb: FacebookInit) {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()
        .unwrap()
        .with_repo_id(REPO_ZERO);
    let key_1 = create_bookmark_name("book1");
    let key_2 = create_bookmark_name("book2");
    let key_3 = create_bookmark_name("book3");

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.create_publishing(&key_1, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    txn.create_publishing(&key_2, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    txn.create_publishing(&key_3, ONES_CSID, BookmarkUpdateReason::TestMove)
        .unwrap();
    assert!(txn.commit().await.unwrap().is_some());

    // If the code breaks and these results become unordered then that will happen non
    // deterministically. Call list() many times to ensure that the bookmarks are actually
    // ordered.
    for _ in 0..10 {
        assert_eq!(
            bookmarks
                .list(
                    ctx.clone(),
                    Freshness::MostRecent,
                    &BookmarkPrefix::empty(),
                    BookmarkCategory::ALL,
                    BookmarkKind::ALL,
                    &BookmarkPagination::FromStart,
                    3
                )
                .try_collect::<Vec<(_, _)>>()
                .await
                .unwrap(),
            vec![
                (
                    Bookmark::new(key_1.clone(), BookmarkKind::Publishing),
                    ONES_CSID
                ),
                (
                    Bookmark::new(key_2.clone(), BookmarkKind::Publishing),
                    ONES_CSID
                ),
                (
                    Bookmark::new(key_3.clone(), BookmarkKind::Publishing),
                    ONES_CSID
                ),
            ]
        );

        assert_eq!(
            bookmarks
                .list(
                    ctx.clone(),
                    Freshness::MostRecent,
                    &BookmarkPrefix::empty(),
                    BookmarkCategory::ALL,
                    BookmarkKind::ALL,
                    &BookmarkPagination::After(key_1.clone().into_name()),
                    1
                )
                .try_collect::<Vec<(_, _)>>()
                .await
                .unwrap()[0],
            (
                Bookmark::new(key_2.clone(), BookmarkKind::Publishing),
                ONES_CSID
            )
        );
    }
}

#[mononoke::fbinit_test]
async fn bookmark_subscription_initialization(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
    let book1 = create_bookmark_name("book1");
    let book2 = create_bookmark_name("book2");
    let book3 = create_bookmark_name("book3");

    // Create some history we won't care about.

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.create_publishing(&book1, ONES_CSID, BookmarkUpdateReason::TestMove)?;
    assert!(txn.commit().await?.is_some());

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.force_delete(&book1, BookmarkUpdateReason::TestMove)?;
    assert!(txn.commit().await?.is_some());

    // Create some bookmarks now that we're going to keep.

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.create_publishing(&book1, ONES_CSID, BookmarkUpdateReason::TestMove)?;
    txn.create_publishing(&book2, TWOS_CSID, BookmarkUpdateReason::TestMove)?;
    assert!(txn.commit().await?.is_some());

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.create_publishing(&book3, THREES_CSID, BookmarkUpdateReason::TestMove)?;
    assert!(txn.commit().await?.is_some());

    let mut sub = bookmarks
        .create_subscription(&ctx, Freshness::MostRecent)
        .await?;

    sub.refresh(&ctx).await?;
    let bookmark_values = sub
        .bookmarks()
        .iter()
        .map(|(key, (cs_id, kind, _log_id))| (key.clone(), (*cs_id, *kind)))
        .collect::<HashMap<_, _>>();
    assert_eq!(
        bookmark_values,
        hashmap! {
            book1.clone() => (ONES_CSID, BookmarkKind::Publishing),
            book2.clone() => (TWOS_CSID, BookmarkKind::Publishing),
            book3.clone() => (THREES_CSID, BookmarkKind::Publishing),
        }
    );
    assert!(
        sub.bookmarks()
            .values()
            .all(|(_, _, log_id)| { (*log_id).is_some_and(|log_id| u64::from(log_id) > 0) })
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn bookmark_subscription_updates(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
    let book = create_bookmark_name("book");

    let mut sub = bookmarks
        .create_subscription(&ctx, Freshness::MostRecent)
        .await?;

    assert_eq!(*sub.bookmarks(), hashmap! {});

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.create_publishing(&book, ONES_CSID, BookmarkUpdateReason::TestMove)?;
    let create_log_id = BookmarkUpdateLogId(
        txn.commit()
            .await?
            .expect("bookmark creation should write an update-log entry"),
    );

    sub.refresh(&ctx).await?;
    assert_eq!(
        *sub.bookmarks(),
        hashmap! { book.clone() => (ONES_CSID, BookmarkKind::Publishing, Some(create_log_id))}
    );
    assert_eq!(
        sub.bookmarks()
            .get(&book)
            .and_then(|(_, _, log_id)| *log_id),
        Some(create_log_id)
    );

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.update(&book, TWOS_CSID, ONES_CSID, BookmarkUpdateReason::TestMove)?;
    let update_log_id = BookmarkUpdateLogId(
        txn.commit()
            .await?
            .expect("bookmark update should write an update-log entry"),
    );

    sub.refresh(&ctx).await?;
    assert_eq!(
        *sub.bookmarks(),
        hashmap! { book.clone() => (TWOS_CSID, BookmarkKind::Publishing, Some(update_log_id))}
    );
    assert_eq!(
        sub.bookmarks()
            .get(&book)
            .and_then(|(_, _, log_id)| *log_id),
        Some(update_log_id)
    );

    let mut txn = bookmarks.create_transaction(ctx.clone());
    txn.force_delete(&book, BookmarkUpdateReason::TestMove)?;
    assert!(txn.commit().await?.is_some());

    sub.refresh(&ctx).await?;
    assert_eq!(*sub.bookmarks(), hashmap! {});

    Ok(())
}

#[derive(Arbitrary, Clone, Copy, Debug)]
enum TestBookmark {
    Book1,
    Book2,
}

#[derive(Arbitrary, Clone, Copy, Debug)]
enum BookmarkOp {
    /// Set this bookmark.
    Set(ChangesetId),
    /// ForceSet this bookmark (this also changes the kind)
    ForceSet(ChangesetId),
    /// Delete this bookmark
    Delete,
}

/// Use Quickcheck to produce a test scenario of bookmark updates.
#[derive(Arbitrary, Clone, Copy, Debug)]
enum TestOp {
    /// Update one of our test bookmarks
    Bookmark(TestBookmark, BookmarkOp),
    /// Do nothing. This allows multiple refreshes to occur in sequence.
    Noop,
    /// Update the BookmarksSubscription and check that it returns the right bookmark values.
    Refresh,
}

/// Verify bookmark subscriptions using Quickcheck. We create a test scenario and verify that the
/// bookmark subscriptions returns the same data it would return if it was freshly created now (we
/// test that this satisfies our assumptions in separate tests for the bookmarks subscription).
#[mononoke::fbinit_test]
fn bookmark_subscription_quickcheck(_fb: FacebookInit) {
    #[tokio::main(flavor = "current_thread")]
    async fn check(fb: FacebookInit, mut ops: Vec<TestOp>) -> bool {
        async move {
            ops.push(TestOp::Refresh);

            let ctx = CoreContext::test_mock(fb);

            let bookmarks = SqlBookmarksBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);

            let book1 = create_bookmark_name("book1");
            let book2 = create_bookmark_name("book2");

            let mut book1_id = None;
            let mut book2_id = None;

            let mut sub = bookmarks
                .create_subscription(&ctx, Freshness::MostRecent)
                .await?;

            for op in ops {
                match op {
                    TestOp::Bookmark(book, op) => {
                        let current_cs_id = match book {
                            TestBookmark::Book1 => &mut book1_id,
                            TestBookmark::Book2 => &mut book2_id,
                        };

                        let book = match book {
                            TestBookmark::Book1 => &book1,
                            TestBookmark::Book2 => &book2,
                        };

                        let mut txn = bookmarks.create_transaction(ctx.clone());

                        match op {
                            BookmarkOp::Set(cs_id) => {
                                match *current_cs_id {
                                    Some(current_cs_id) => {
                                        txn.update(
                                            book,
                                            cs_id,
                                            current_cs_id,
                                            BookmarkUpdateReason::TestMove,
                                        )?;
                                    }
                                    None => {
                                        txn.create_publishing(
                                            book,
                                            cs_id,
                                            BookmarkUpdateReason::TestMove,
                                        )?;
                                    }
                                }

                                *current_cs_id = Some(cs_id);
                            }
                            BookmarkOp::ForceSet(cs_id) => {
                                txn.force_set(book, cs_id, BookmarkUpdateReason::TestMove)?;
                                *current_cs_id = Some(cs_id);
                            }
                            BookmarkOp::Delete => {
                                match *current_cs_id {
                                    Some(current_cs_id) => {
                                        txn.delete(
                                            book,
                                            current_cs_id,
                                            BookmarkUpdateReason::TestMove,
                                        )?;
                                    }
                                    None => {
                                        txn.force_delete(book, BookmarkUpdateReason::TestMove)?;
                                    }
                                }

                                *current_cs_id = None;
                            }
                        };

                        assert!(txn.commit().await?.is_some());
                    }
                    TestOp::Noop => {
                        // It's a noop
                    }
                    TestOp::Refresh => {
                        sub.refresh(&ctx).await?;

                        let control = bookmarks
                            .create_subscription(&ctx, Freshness::MostRecent)
                            .await?;

                        if control.bookmarks() != sub.bookmarks() {
                            return Ok(false);
                        }
                    }
                };
            }

            Result::<_, Error>::Ok(true)
        }
        .await
        .unwrap()
    }

    quickcheck::quickcheck(check as fn(FacebookInit, Vec<TestOp>) -> bool);
}
