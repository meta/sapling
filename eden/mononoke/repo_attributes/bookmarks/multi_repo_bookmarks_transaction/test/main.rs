/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Result;
use anyhow::anyhow;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkTransactionError;
use bookmarks::BookmarkTransactionHook;
use bookmarks::BookmarkUpdateLog;
use bookmarks::BookmarkUpdateLogId;
use bookmarks::BookmarkUpdateReason;
use bookmarks::Bookmarks;
use bookmarks::Freshness;
use context::CoreContext;
use dbbookmarks::SqlBookmarksBuilder;
use dbbookmarks::store::SqlBookmarks;
use fbinit::FacebookInit;
use futures::future::FutureExt;
use futures::stream::TryStreamExt;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::RepositoryId;
use mononoke_types_mocks::changesetid::ONES_CSID;
use mononoke_types_mocks::changesetid::THREES_CSID;
use mononoke_types_mocks::changesetid::TWOS_CSID;
use multi_repo_bookmarks_transaction::MultiRepoBookmarksTransaction;
use multi_repo_bookmarks_transaction::MultiRepoBookmarksTransactionResult;
use multi_repo_bookmarks_transaction::retry_commit_loop;
use sql_construct::SqlConstruct;
use sql_ext::Connection;

mod guard_chunks;
mod log_lock;
mod writer_interop;

/// Test fixture providing two repos that share the same underlying DB.
struct TwoRepoFixture {
    ctx: CoreContext,
    conn: Connection,
    repo_id_1: RepositoryId,
    repo_id_2: RepositoryId,
    bookmarks_1: SqlBookmarks,
    bookmarks_2: SqlBookmarks,
}

impl TwoRepoFixture {
    fn new(fb: FacebookInit) -> Result<Self> {
        let ctx = CoreContext::test_mock(fb);
        let repo_id_1 = RepositoryId::new(1);
        let repo_id_2 = RepositoryId::new(2);
        let builder = SqlBookmarksBuilder::with_sqlite_in_memory()?;
        let bookmarks_1 = builder.clone().with_repo_id(repo_id_1);
        let bookmarks_2 = builder.with_repo_id(repo_id_2);
        let conn = bookmarks_1.write_connection().clone();
        Ok(Self {
            ctx,
            conn,
            repo_id_1,
            repo_id_2,
            bookmarks_1,
            bookmarks_2,
        })
    }

    fn multi_txn(&self) -> MultiRepoBookmarksTransaction {
        MultiRepoBookmarksTransaction::new(self.ctx.clone(), self.conn.clone())
    }

    /// Create a bookmark in the given repo via a standard single-repo transaction.
    async fn set_bookmark(
        &self,
        bookmarks: &SqlBookmarks,
        key: &BookmarkKey,
        cs_id: ChangesetId,
    ) -> Result<()> {
        let mut txn = bookmarks.create_transaction(self.ctx.clone());
        txn.force_set(key, cs_id, BookmarkUpdateReason::TestMove)?;
        assert!(txn.commit().await.unwrap().is_some());
        Ok(())
    }

    /// Read a bookmark value from the given repo.
    async fn get_bookmark(
        &self,
        bookmarks: &SqlBookmarks,
        key: &BookmarkKey,
    ) -> Result<Option<ChangesetId>> {
        bookmarks
            .get(self.ctx.clone(), key, Freshness::MostRecent)
            .await
    }
}

#[mononoke::fbinit_test]
async fn test_multi_repo_update_success(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    let bookmark = BookmarkKey::new("master")?;

    f.set_bookmark(&f.bookmarks_1, &bookmark, ONES_CSID).await?;
    f.set_bookmark(&f.bookmarks_2, &bookmark, ONES_CSID).await?;

    let mut txn = f.multi_txn();
    txn.update(
        f.repo_id_1,
        &bookmark,
        TWOS_CSID,
        ONES_CSID,
        BookmarkUpdateReason::TestMove,
    )?;
    txn.update(
        f.repo_id_2,
        &bookmark,
        TWOS_CSID,
        ONES_CSID,
        BookmarkUpdateReason::TestMove,
    )?;

    let result = txn.commit().await?;
    assert!(result.is_success());

    assert_eq!(
        f.get_bookmark(&f.bookmarks_1, &bookmark).await?,
        Some(TWOS_CSID)
    );
    assert_eq!(
        f.get_bookmark(&f.bookmarks_2, &bookmark).await?,
        Some(TWOS_CSID)
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_cas_failure_rolls_back_all(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    let bookmark = BookmarkKey::new("master")?;

    f.set_bookmark(&f.bookmarks_1, &bookmark, ONES_CSID).await?;
    f.set_bookmark(&f.bookmarks_2, &bookmark, ONES_CSID).await?;

    // R1: correct old value, R2: wrong old value (THREES instead of ONES)
    let mut txn = f.multi_txn();
    txn.update(
        f.repo_id_1,
        &bookmark,
        TWOS_CSID,
        ONES_CSID,
        BookmarkUpdateReason::TestMove,
    )?;
    txn.update(
        f.repo_id_2,
        &bookmark,
        TWOS_CSID,
        THREES_CSID,
        BookmarkUpdateReason::TestMove,
    )?;

    let result = txn.commit().await?;
    assert!(!result.is_success());

    // Both should be unchanged
    assert_eq!(
        f.get_bookmark(&f.bookmarks_1, &bookmark).await?,
        Some(ONES_CSID)
    );
    assert_eq!(
        f.get_bookmark(&f.bookmarks_2, &bookmark).await?,
        Some(ONES_CSID)
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_rejects_duplicate_bookmark_in_same_repo(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    let bookmark = BookmarkKey::new("master")?;

    let mut txn = f.multi_txn();
    txn.update(
        f.repo_id_1,
        &bookmark,
        TWOS_CSID,
        ONES_CSID,
        BookmarkUpdateReason::TestMove,
    )?;

    let result = txn.update(
        f.repo_id_1,
        &bookmark,
        TWOS_CSID,
        ONES_CSID,
        BookmarkUpdateReason::TestMove,
    );
    assert!(result.is_err());
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_same_bookmark_name_different_repos(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    let bookmark = BookmarkKey::new("master")?;

    let mut txn = f.multi_txn();
    txn.update(
        f.repo_id_1,
        &bookmark,
        TWOS_CSID,
        ONES_CSID,
        BookmarkUpdateReason::TestMove,
    )?;
    txn.update(
        f.repo_id_2,
        &bookmark,
        TWOS_CSID,
        ONES_CSID,
        BookmarkUpdateReason::TestMove,
    )?;
    // Both accepted — different repo IDs
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_update_log_written_for_all_repos(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    let bookmark = BookmarkKey::new("master")?;

    f.set_bookmark(&f.bookmarks_1, &bookmark, ONES_CSID).await?;
    f.set_bookmark(&f.bookmarks_2, &bookmark, ONES_CSID).await?;

    let mut txn = f.multi_txn();
    txn.update(
        f.repo_id_1,
        &bookmark,
        TWOS_CSID,
        ONES_CSID,
        BookmarkUpdateReason::TestMove,
    )?;
    txn.update(
        f.repo_id_2,
        &bookmark,
        THREES_CSID,
        ONES_CSID,
        BookmarkUpdateReason::TestMove,
    )?;
    assert!(txn.commit().await?.is_success());

    // Each repo's log has 2 entries: the force_set then the multi-repo update.
    let log_1: Vec<_> = f
        .bookmarks_1
        .read_next_bookmark_log_entries(
            f.ctx.clone(),
            BookmarkUpdateLogId(0),
            10,
            Freshness::MostRecent,
        )
        .try_collect()
        .await?;
    assert_eq!(log_1.len(), 2);
    assert_eq!(log_1[1].from_changeset_id, Some(ONES_CSID));
    assert_eq!(log_1[1].to_changeset_id, Some(TWOS_CSID));
    assert_eq!(log_1[1].repo_id, f.repo_id_1);

    let log_2: Vec<_> = f
        .bookmarks_2
        .read_next_bookmark_log_entries(
            f.ctx.clone(),
            BookmarkUpdateLogId(0),
            10,
            Freshness::MostRecent,
        )
        .try_collect()
        .await?;
    assert_eq!(log_2.len(), 2);
    assert_eq!(log_2[1].from_changeset_id, Some(ONES_CSID));
    assert_eq!(log_2[1].to_changeset_id, Some(THREES_CSID));
    assert_eq!(log_2[1].repo_id, f.repo_id_2);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_mixed_create_update_delete(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    let master = BookmarkKey::new("master")?;
    let release = BookmarkKey::new("release")?;
    let feature = BookmarkKey::new("feature")?;

    f.set_bookmark(&f.bookmarks_1, &master, ONES_CSID).await?;
    f.set_bookmark(&f.bookmarks_2, &release, ONES_CSID).await?;

    let mut txn = f.multi_txn();
    txn.update(
        f.repo_id_1,
        &master,
        TWOS_CSID,
        ONES_CSID,
        BookmarkUpdateReason::TestMove,
    )?;
    txn.create(
        f.repo_id_2,
        &feature,
        THREES_CSID,
        BookmarkUpdateReason::TestMove,
    )?;
    txn.delete(
        f.repo_id_2,
        &release,
        ONES_CSID,
        BookmarkUpdateReason::TestMove,
    )?;
    assert!(txn.commit().await?.is_success());

    assert_eq!(
        f.get_bookmark(&f.bookmarks_1, &master).await?,
        Some(TWOS_CSID)
    );
    assert_eq!(
        f.get_bookmark(&f.bookmarks_2, &feature).await?,
        Some(THREES_CSID)
    );
    assert_eq!(f.get_bookmark(&f.bookmarks_2, &release).await?, None);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_create_failure_rolls_back(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    let bookmark = BookmarkKey::new("master")?;

    f.set_bookmark(&f.bookmarks_1, &bookmark, ONES_CSID).await?;
    f.set_bookmark(&f.bookmarks_2, &bookmark, ONES_CSID).await?;

    // Update R1 + create R2 "master" (already exists) => should roll back both
    let mut txn = f.multi_txn();
    txn.update(
        f.repo_id_1,
        &bookmark,
        TWOS_CSID,
        ONES_CSID,
        BookmarkUpdateReason::TestMove,
    )?;
    txn.create(
        f.repo_id_2,
        &bookmark,
        THREES_CSID,
        BookmarkUpdateReason::TestMove,
    )?;

    assert!(!txn.commit().await?.is_success());
    assert_eq!(
        f.get_bookmark(&f.bookmarks_1, &bookmark).await?,
        Some(ONES_CSID)
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_retry_on_retryable_error(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let attempts = std::cell::Cell::new(0u64);

    let result = retry_commit_loop(&ctx, 5, || {
        attempts.set(attempts.get() + 1);
        let attempt = attempts.get();
        async move {
            if attempt == 1 {
                Err(BookmarkTransactionError::RetryableError(anyhow!(
                    "simulated transient failure"
                )))
            } else {
                Ok(MultiRepoBookmarksTransactionResult::Success)
            }
        }
    })
    .await?;

    assert!(result.is_success());
    assert_eq!(attempts.get(), 2, "should succeed on second attempt");
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_retryable_error_exhausted(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let attempts = std::cell::Cell::new(0u64);

    let result = retry_commit_loop(&ctx, 3, || {
        attempts.set(attempts.get() + 1);
        async move {
            Err::<MultiRepoBookmarksTransactionResult, _>(BookmarkTransactionError::RetryableError(
                anyhow!("simulated permanent transient failure"),
            ))
        }
    })
    .await;

    let err = match result {
        Ok(_) => panic!("expected Err after exhausting retries"),
        Err(e) => e,
    };
    assert_eq!(
        attempts.get(),
        3,
        "should attempt exactly max_attempts times"
    );
    let err_msg = format!("{err:#}");
    assert!(
        err_msg.contains("exhausted 3 retry attempts"),
        "error should describe exhaustion: {err_msg}"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_other_error_not_retried(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let attempts = std::cell::Cell::new(0u64);

    let result = retry_commit_loop(&ctx, 5, || {
        attempts.set(attempts.get() + 1);
        async move {
            Err::<MultiRepoBookmarksTransactionResult, _>(BookmarkTransactionError::Other(anyhow!(
                "non-retryable infrastructure failure"
            )))
        }
    })
    .await;

    assert!(result.is_err(), "Other error must propagate");
    assert_eq!(attempts.get(), 1, "Other error must not retry");
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_logic_error_translates_to_cas_failure(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let attempts = std::cell::Cell::new(0u64);

    let result = retry_commit_loop(&ctx, 5, || {
        attempts.set(attempts.get() + 1);
        async move {
            Err::<MultiRepoBookmarksTransactionResult, _>(BookmarkTransactionError::LogicError)
        }
    })
    .await?;

    assert!(!result.is_success(), "LogicError should map to CasFailure");
    assert_eq!(attempts.get(), 1, "LogicError must not retry");
    Ok(())
}

/// A hook that counts its invocations and fails the first `fail_first`
/// attempts, so tests can observe both re-runs and failure handling.
fn counting_hook(fail_first: usize) -> (BookmarkTransactionHook, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let hook: BookmarkTransactionHook = Arc::new(move |_ctx, txn| {
        let seen = counter.fetch_add(1, Ordering::SeqCst);
        async move {
            if seen < fail_first {
                Err(BookmarkTransactionError::RetryableError(anyhow!(
                    "simulated transient hook failure"
                )))
            } else {
                Ok(txn)
            }
        }
        .boxed()
    });
    (hook, calls)
}

#[mononoke::fbinit_test]
async fn test_commit_with_hooks_runs_hook_and_moves_bookmarks(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    let bookmark = BookmarkKey::new("master")?;
    f.set_bookmark(&f.bookmarks_1, &bookmark, ONES_CSID).await?;

    let mut txn = f.multi_txn();
    txn.update(
        f.repo_id_1,
        &bookmark,
        TWOS_CSID,
        ONES_CSID,
        BookmarkUpdateReason::TestMove,
    )?;

    let (hook, calls) = counting_hook(0);
    let result = txn.commit_with_hooks(vec![hook]).await?;

    assert!(result.is_success());
    assert_eq!(calls.load(Ordering::SeqCst), 1, "hook should run once");
    assert_eq!(
        f.get_bookmark(&f.bookmarks_1, &bookmark).await?,
        Some(TWOS_CSID),
        "bookmark should move when the hook succeeds",
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_hook_failure_aborts_the_bookmark_move(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    let bookmark = BookmarkKey::new("master")?;
    f.set_bookmark(&f.bookmarks_1, &bookmark, ONES_CSID).await?;

    let mut txn = f.multi_txn();
    txn.update(
        f.repo_id_1,
        &bookmark,
        TWOS_CSID,
        ONES_CSID,
        BookmarkUpdateReason::TestMove,
    )?;

    // Fails on every attempt, so the retry budget is exhausted.
    let (hook, calls) = counting_hook(usize::MAX);
    let result = txn.commit_with_hooks(vec![hook]).await;

    assert!(result.is_err(), "a failing hook must fail the commit");
    assert!(
        calls.load(Ordering::SeqCst) > 1,
        "a retryable hook failure should be retried",
    );
    // The hook shares the bookmark move's transaction, so aborting it must
    // leave the bookmark where it was.
    assert_eq!(
        f.get_bookmark(&f.bookmarks_1, &bookmark).await?,
        Some(ONES_CSID),
        "bookmark must not move when the hook fails",
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_hooks_rerun_on_each_attempt(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    let bookmark = BookmarkKey::new("master")?;
    f.set_bookmark(&f.bookmarks_1, &bookmark, ONES_CSID).await?;

    let mut txn = f.multi_txn();
    txn.update(
        f.repo_id_1,
        &bookmark,
        TWOS_CSID,
        ONES_CSID,
        BookmarkUpdateReason::TestMove,
    )?;

    let (hook, calls) = counting_hook(1);
    let result = txn.commit_with_hooks(vec![hook]).await?;

    assert!(result.is_success());
    // This is why hook writes must be idempotent.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "hook should run again on the retried attempt",
    );
    assert_eq!(
        f.get_bookmark(&f.bookmarks_1, &bookmark).await?,
        Some(TWOS_CSID),
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_hook_logic_error_is_not_reported_as_a_cas_failure(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    let bookmark = BookmarkKey::new("master")?;
    f.set_bookmark(&f.bookmarks_1, &bookmark, ONES_CSID).await?;

    let mut txn = f.multi_txn();
    txn.update(
        f.repo_id_1,
        &bookmark,
        TWOS_CSID,
        ONES_CSID,
        BookmarkUpdateReason::TestMove,
    )?;

    // A buggy hook claiming a CAS failure must not be laundered into
    // `CasFailure`, which the caller reads as "someone else won the race"
    // and retries against a fault that never clears.
    let hook: BookmarkTransactionHook =
        Arc::new(|_ctx, _txn| async { Err(BookmarkTransactionError::LogicError) }.boxed());
    let result = txn.commit_with_hooks(vec![hook]).await;

    let err = match result {
        Err(err) => err,
        Ok(_) => panic!("a hook reporting LogicError must fail the commit"),
    };
    assert!(
        format!("{err:#}").contains("only bookmark writes can do"),
        "the error should name the real problem, got: {err:#}",
    );
    assert_eq!(
        f.get_bookmark(&f.bookmarks_1, &bookmark).await?,
        Some(ONES_CSID),
        "bookmark must not move",
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_hooks_not_run_when_there_are_no_ops(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;

    let (hook, calls) = counting_hook(0);
    let result = f.multi_txn().commit_with_hooks(vec![hook]).await?;

    assert!(result.is_success());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no bookmark moved, so there is nothing for a hook to record",
    );
    Ok(())
}

async fn log_entries(
    f: &TwoRepoFixture,
    bookmarks: &SqlBookmarks,
) -> Result<Vec<bookmarks::BookmarkUpdateLogEntry>> {
    bookmarks
        .read_next_bookmark_log_entries(
            f.ctx.clone(),
            BookmarkUpdateLogId(0),
            10_000,
            Freshness::MostRecent,
        )
        .try_collect()
        .await
}

// 250 updates span three chunks; every bookmark moves and the log keeps
// one contiguous id per op in op order.
#[mononoke::fbinit_test]
async fn test_batched_updates_span_chunks(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    let keys: Vec<BookmarkKey> = (0..250)
        .map(|i| BookmarkKey::new(format!("b{i:03}")))
        .collect::<Result<_>>()?;
    for key in &keys {
        f.set_bookmark(&f.bookmarks_1, key, ONES_CSID).await?;
    }
    let mut txn = f.multi_txn();
    for key in &keys {
        txn.update(
            f.repo_id_1,
            key,
            TWOS_CSID,
            ONES_CSID,
            BookmarkUpdateReason::TestMove,
        )?;
    }
    assert!(txn.commit().await?.is_success());

    for key in [&keys[0], &keys[99], &keys[100], &keys[249]] {
        assert_eq!(f.get_bookmark(&f.bookmarks_1, key).await?, Some(TWOS_CSID));
    }
    let log = log_entries(&f, &f.bookmarks_1).await?;
    assert_eq!(log.len(), 500);
    for (i, entry) in log[250..].iter().enumerate() {
        assert_eq!(entry.id, BookmarkUpdateLogId(251 + i as u64));
        assert_eq!(entry.bookmark_name, keys[i]);
        assert_eq!(entry.from_changeset_id, Some(ONES_CSID));
        assert_eq!(entry.to_changeset_id, Some(TWOS_CSID));
    }
    Ok(())
}

// A stale baseline in the last chunk rolls back the chunks already applied.
#[mononoke::fbinit_test]
async fn test_batched_cas_failure_rolls_back_earlier_chunks(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    let keys: Vec<BookmarkKey> = (0..150)
        .map(|i| BookmarkKey::new(format!("b{i:03}")))
        .collect::<Result<_>>()?;
    for key in &keys {
        f.set_bookmark(&f.bookmarks_1, key, ONES_CSID).await?;
    }
    let mut txn = f.multi_txn();
    for (i, key) in keys.iter().enumerate() {
        let old = if i == 149 { THREES_CSID } else { ONES_CSID };
        txn.update(
            f.repo_id_1,
            key,
            TWOS_CSID,
            old,
            BookmarkUpdateReason::TestMove,
        )?;
    }
    assert!(!txn.commit().await?.is_success());

    for key in [&keys[0], &keys[50], &keys[149]] {
        assert_eq!(f.get_bookmark(&f.bookmarks_1, key).await?, Some(ONES_CSID));
    }
    assert_eq!(log_entries(&f, &f.bookmarks_1).await?.len(), 150);
    Ok(())
}

// A scratch bookmark is not publishing, so it fails the CAS as it did per row.
#[mononoke::fbinit_test]
async fn test_scratch_bookmark_in_batch_is_cas_failure(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    let a = BookmarkKey::new("a")?;
    let b = BookmarkKey::new("b")?;
    let c = BookmarkKey::new("c")?;
    f.set_bookmark(&f.bookmarks_1, &a, ONES_CSID).await?;
    f.set_bookmark(&f.bookmarks_1, &c, ONES_CSID).await?;
    let mut scratch = f.bookmarks_1.create_transaction(f.ctx.clone());
    scratch.create_scratch(&b, ONES_CSID)?;
    assert!(scratch.commit().await?.is_some());

    let mut txn = f.multi_txn();
    for key in [&a, &b, &c] {
        txn.update(
            f.repo_id_1,
            key,
            TWOS_CSID,
            ONES_CSID,
            BookmarkUpdateReason::TestMove,
        )?;
    }
    assert!(!txn.commit().await?.is_success());
    assert_eq!(f.get_bookmark(&f.bookmarks_1, &a).await?, Some(ONES_CSID));
    assert_eq!(f.get_bookmark(&f.bookmarks_1, &c).await?, Some(ONES_CSID));
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_missing_bookmark_in_batch_is_cas_failure(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    let a = BookmarkKey::new("a")?;
    let b = BookmarkKey::new("b")?;
    let c = BookmarkKey::new("c")?;
    f.set_bookmark(&f.bookmarks_1, &a, ONES_CSID).await?;
    f.set_bookmark(&f.bookmarks_1, &c, ONES_CSID).await?;

    let mut txn = f.multi_txn();
    for key in [&a, &b, &c] {
        txn.update(
            f.repo_id_1,
            key,
            TWOS_CSID,
            ONES_CSID,
            BookmarkUpdateReason::TestMove,
        )?;
    }
    assert!(!txn.commit().await?.is_success());
    assert_eq!(f.get_bookmark(&f.bookmarks_1, &a).await?, Some(ONES_CSID));
    assert_eq!(f.get_bookmark(&f.bookmarks_1, &b).await?, None);
    Ok(())
}

// Batched runs and single ops interleave without disturbing per-repo log order.
#[mononoke::fbinit_test]
async fn test_batched_and_single_ops_keep_log_order(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    let u1 = BookmarkKey::new("u1")?;
    let u2 = BookmarkKey::new("u2")?;
    let u3 = BookmarkKey::new("u3")?;
    let n1 = BookmarkKey::new("n1")?;
    let v1 = BookmarkKey::new("v1")?;
    let v2 = BookmarkKey::new("v2")?;
    for key in [&u1, &u2, &u3] {
        f.set_bookmark(&f.bookmarks_1, key, ONES_CSID).await?;
    }
    for key in [&v1, &v2] {
        f.set_bookmark(&f.bookmarks_2, key, ONES_CSID).await?;
    }

    let mut txn = f.multi_txn();
    let r = BookmarkUpdateReason::TestMove;
    txn.update(f.repo_id_1, &u1, TWOS_CSID, ONES_CSID, r)?;
    txn.update(f.repo_id_1, &u2, TWOS_CSID, ONES_CSID, r)?;
    txn.create(f.repo_id_1, &n1, THREES_CSID, r)?;
    txn.update(f.repo_id_1, &u3, TWOS_CSID, ONES_CSID, r)?;
    txn.update(f.repo_id_2, &v1, TWOS_CSID, ONES_CSID, r)?;
    txn.update(f.repo_id_2, &v2, TWOS_CSID, ONES_CSID, r)?;
    assert!(txn.commit().await?.is_success());

    let log_1 = log_entries(&f, &f.bookmarks_1).await?;
    let names_1: Vec<&BookmarkKey> = log_1[3..].iter().map(|e| &e.bookmark_name).collect();
    assert_eq!(names_1, vec![&u1, &u2, &n1, &u3]);
    assert_eq!(
        log_1[3..].iter().map(|e| e.id.0).collect::<Vec<_>>(),
        vec![4, 5, 6, 7]
    );
    let log_2 = log_entries(&f, &f.bookmarks_2).await?;
    let names_2: Vec<&BookmarkKey> = log_2[2..].iter().map(|e| &e.bookmark_name).collect();
    assert_eq!(names_2, vec![&v1, &v2]);
    assert_eq!(
        f.get_bookmark(&f.bookmarks_1, &n1).await?,
        Some(THREES_CSID)
    );
    Ok(())
}
