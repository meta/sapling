/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Error;
use bookmarks::BookmarkCategory;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkUpdateLogId;
use bookmarks::BookmarkUpdateLogRef;
use bookmarks::BookmarkUpdateReason;
use bookmarks::Freshness;
use context::CoreContext;
use cross_repo_sync::CommitSyncContext;
use fbinit::FacebookInit;
use futures::TryStreamExt;
use futures::future;
use mononoke_macros::mononoke;
use tests_utils::CreateCommitContext;
use tests_utils::resolve_cs_id;

use super::BookmarkRenamerType;
use super::MoverType;
use super::delete_bookmark;
use super::init_repos;
use super::move_bookmark;
use crate::BacksyncLimit;
use crate::backsync_latest_for_bookmark;
use crate::format_bookmark_counter;
use crate::format_counter;
use crate::get_bookmark_counter_or_initialize;

#[mononoke::fbinit_test]
async fn high_global_cursor_does_not_skip_pending_bookmark_work(
    fb: FacebookInit,
) -> Result<(), Error> {
    let (data, dbs) = init_repos(fb, MoverType::Noop, BookmarkRenamerType::Noop).await?;
    let ctx = CoreContext::test_mock(fb);
    let dbs = Arc::new(dbs);
    let bookmark = BookmarkKey::new("anotherbookmark")?;
    let source_id = data.get_source_repo_id();
    let counter = format_bookmark_counter(&source_id, &bookmark)?;
    let global = format_counter(&source_id);
    assert!(
        dbs.counters
            .set_counter(&ctx, &global, 1000, Some(0))
            .await?
    );

    backsync_latest_for_bookmark(
        ctx.clone(),
        data.clone(),
        dbs.clone(),
        bookmark.clone(),
        BacksyncLimit::Limit(0),
        Arc::new(AtomicBool::new(false)),
        CommitSyncContext::Tests,
        false,
    )
    .await?;
    assert_eq!(dbs.counters.get_counter(&ctx, &counter).await?, Some(0));
    assert_eq!(
        dbs.bookmarks
            .get(ctx.clone(), &bookmark, Freshness::MostRecent)
            .await?,
        None
    );
    let source_csid = resolve_cs_id(&ctx, data.get_source_repo(), "anotherbookmark").await?;
    assert!(
        data.get_commit_sync_outcome(&ctx, source_csid)
            .await?
            .is_none()
    );

    let entries = data
        .get_source_repo()
        .bookmark_update_log()
        .read_next_bookmark_log_entries_by_bookmark(
            ctx.clone(),
            bookmark.clone(),
            BookmarkUpdateLogId(0),
            100,
            Freshness::MostRecent,
        )
        .try_collect::<Vec<_>>()
        .await?;
    let last = entries
        .last()
        .expect("fixture has pending bookmark moves")
        .id;
    assert!(last.0 < 1000);
    backsync_latest_for_bookmark(
        ctx.clone(),
        data.clone(),
        dbs.clone(),
        bookmark.clone(),
        BacksyncLimit::NoLimit,
        Arc::new(AtomicBool::new(false)),
        CommitSyncContext::Tests,
        false,
    )
    .await?;
    assert_eq!(
        dbs.counters.get_counter(&ctx, &counter).await?,
        Some(last.try_into()?)
    );
    assert!(
        data.get_commit_sync_outcome(&ctx, source_csid)
            .await?
            .is_some()
    );
    assert!(
        dbs.bookmarks
            .get(ctx.clone(), &bookmark, Freshness::MostRecent)
            .await?
            .is_some()
    );
    let history = dbs
        .bookmark_update_log
        .list_bookmark_log_entries(
            ctx.clone(),
            bookmark.clone(),
            100,
            None,
            Freshness::MostRecent,
        )
        .try_collect::<Vec<_>>()
        .await?;
    assert_eq!(history.len(), entries.len());
    assert!(
        history
            .iter()
            .all(|(_, _, reason, _)| *reason == BookmarkUpdateReason::Backsyncer)
    );

    assert!(
        dbs.counters
            .set_counter(&ctx, &global, 9000, Some(1000))
            .await?
    );
    backsync_latest_for_bookmark(
        ctx.clone(),
        data,
        dbs.clone(),
        bookmark.clone(),
        BacksyncLimit::NoLimit,
        Arc::new(AtomicBool::new(false)),
        CommitSyncContext::Tests,
        false,
    )
    .await?;
    assert_eq!(
        dbs.counters.get_counter(&ctx, &counter).await?,
        Some(last.try_into()?)
    );
    assert_eq!(dbs.counters.get_counter(&ctx, &global).await?, Some(9000));
    assert_eq!(
        dbs.bookmark_update_log
            .list_bookmark_log_entries(ctx, bookmark, 100, None, Freshness::MostRecent)
            .try_collect::<Vec<_>>()
            .await?,
        history
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn missing_cursor_rejects_mapped_target_state_and_deletion_history(
    fb: FacebookInit,
) -> Result<(), Error> {
    for deleted in [false, true] {
        let source_bookmark = BookmarkKey::new("small/already-synced")?;
        let target_bookmark = BookmarkKey::new("already-synced")?;
        let (data, dbs) = init_repos(
            fb,
            MoverType::Noop,
            BookmarkRenamerType::CommonAndPrefix(BookmarkKey::new("master")?, "small/".to_owned()),
        )
        .await?;
        let ctx = CoreContext::test_mock(fb);
        let source_value = resolve_cs_id(&ctx, data.get_source_repo(), "master").await?;
        move_bookmark(
            ctx.clone(),
            data.get_source_repo().clone(),
            &source_bookmark,
            source_value,
        )
        .await?;
        let target_value = CreateCommitContext::new_root(&ctx, data.get_target_repo())
            .commit()
            .await?;
        move_bookmark(
            ctx.clone(),
            data.get_target_repo().clone(),
            &target_bookmark,
            target_value,
        )
        .await?;
        if deleted {
            delete_bookmark(
                ctx.clone(),
                data.get_target_repo().clone(),
                &target_bookmark,
            )
            .await?;
        }
        let counter = format_bookmark_counter(&data.get_source_repo_id(), &source_bookmark)?;
        let dbs = Arc::new(dbs);
        let before = dbs
            .bookmark_update_log
            .list_bookmark_log_entries(
                ctx.clone(),
                target_bookmark.clone(),
                100,
                None,
                Freshness::MostRecent,
            )
            .try_collect::<Vec<_>>()
            .await?;
        let error = backsync_latest_for_bookmark(
            ctx.clone(),
            data,
            dbs.clone(),
            source_bookmark,
            BacksyncLimit::NoLimit,
            Arc::new(AtomicBool::new(false)),
            CommitSyncContext::Tests,
            false,
        )
        .await
        .expect_err("an absent cursor cannot certify historical completion");
        assert!(
            error
                .to_string()
                .contains("explicit operator initialization/reconciliation required")
        );
        assert_eq!(dbs.counters.get_counter(&ctx, &counter).await?, None);
        assert_eq!(
            dbs.bookmarks
                .get(ctx.clone(), &target_bookmark, Freshness::MostRecent)
                .await?,
            (!deleted).then_some(target_value)
        );
        assert_eq!(
            dbs.bookmark_update_log
                .list_bookmark_log_entries(ctx, target_bookmark, 100, None, Freshness::MostRecent)
                .try_collect::<Vec<_>>()
                .await?,
            before
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn bookmark_cursor_replays_deletion_and_recreation(fb: FacebookInit) -> Result<(), Error> {
    let (data, dbs) = init_repos(fb, MoverType::Noop, BookmarkRenamerType::Noop).await?;
    let ctx = CoreContext::test_mock(fb);
    let dbs = Arc::new(dbs);
    let bookmark = BookmarkKey::new("anotherbookmark")?;
    backsync_latest_for_bookmark(
        ctx.clone(),
        data.clone(),
        dbs.clone(),
        bookmark.clone(),
        BacksyncLimit::NoLimit,
        Arc::new(AtomicBool::new(false)),
        CommitSyncContext::Tests,
        false,
    )
    .await?;
    let original = dbs
        .bookmarks
        .get(ctx.clone(), &bookmark, Freshness::MostRecent)
        .await?;
    let source_value = resolve_cs_id(&ctx, data.get_source_repo(), "anotherbookmark").await?;
    delete_bookmark(ctx.clone(), data.get_source_repo().clone(), &bookmark).await?;
    move_bookmark(
        ctx.clone(),
        data.get_source_repo().clone(),
        &bookmark,
        source_value,
    )
    .await?;
    backsync_latest_for_bookmark(
        ctx.clone(),
        data,
        dbs.clone(),
        bookmark.clone(),
        BacksyncLimit::NoLimit,
        Arc::new(AtomicBool::new(false)),
        CommitSyncContext::Tests,
        false,
    )
    .await?;
    assert_eq!(
        dbs.bookmarks
            .get(ctx.clone(), &bookmark, Freshness::MostRecent)
            .await?,
        original
    );
    let history = dbs
        .bookmark_update_log
        .list_bookmark_log_entries(ctx, bookmark, 2, None, Freshness::MostRecent)
        .try_collect::<Vec<_>>()
        .await?;
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].1, original);
    assert_eq!(history[1].1, None);
    Ok(())
}

#[mononoke::fbinit_test]
async fn target_history_and_cursor_are_category_specific(fb: FacebookInit) -> Result<(), Error> {
    let (data, dbs) = init_repos(fb, MoverType::Noop, BookmarkRenamerType::Noop).await?;
    let ctx = CoreContext::test_mock(fb);
    let branch = BookmarkKey::new("anotherbookmark")?;
    let tag = BookmarkKey::with_name_and_category(branch.name().clone(), BookmarkCategory::Tag);
    let target_value = CreateCommitContext::new_root(&ctx, data.get_target_repo())
        .commit()
        .await?;
    move_bookmark(
        ctx.clone(),
        data.get_target_repo().clone(),
        &tag,
        target_value,
    )
    .await?;
    delete_bookmark(ctx.clone(), data.get_target_repo().clone(), &tag).await?;
    let dbs = Arc::new(dbs);
    backsync_latest_for_bookmark(
        ctx.clone(),
        data.clone(),
        dbs.clone(),
        branch.clone(),
        BacksyncLimit::NoLimit,
        Arc::new(AtomicBool::new(false)),
        CommitSyncContext::Tests,
        false,
    )
    .await?;
    assert!(
        dbs.bookmarks
            .get(ctx.clone(), &branch, Freshness::MostRecent)
            .await?
            .is_some()
    );
    let error = backsync_latest_for_bookmark(
        ctx.clone(),
        data.clone(),
        dbs.clone(),
        tag.clone(),
        BacksyncLimit::NoLimit,
        Arc::new(AtomicBool::new(false)),
        CommitSyncContext::Tests,
        false,
    )
    .await
    .expect_err("tag history must not initialize as a new stream");
    assert!(
        error
            .to_string()
            .contains("explicit operator initialization/reconciliation required")
    );
    assert_eq!(
        dbs.counters
            .get_counter(
                &ctx,
                &format_bookmark_counter(&data.get_source_repo_id(), &tag)?
            )
            .await?,
        None
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn bookmark_transaction_failure_is_not_global_completion(
    fb: FacebookInit,
) -> Result<(), Error> {
    let (data, dbs) = init_repos(fb, MoverType::Noop, BookmarkRenamerType::Noop).await?;
    let ctx = CoreContext::test_mock(fb);
    let bookmark = BookmarkKey::new("anotherbookmark")?;
    let counter = format_bookmark_counter(&data.get_source_repo_id(), &bookmark)?;
    assert!(
        dbs.counters
            .set_counter_if_absent(&ctx, &counter, 0)
            .await?
    );
    assert!(
        dbs.counters
            .set_counter(
                &ctx,
                &format_counter(&data.get_source_repo_id()),
                1000,
                Some(0)
            )
            .await?
    );
    let target_value = CreateCommitContext::new_root(&ctx, data.get_target_repo())
        .commit()
        .await?;
    move_bookmark(
        ctx.clone(),
        data.get_target_repo().clone(),
        &bookmark,
        target_value,
    )
    .await?;
    let dbs = Arc::new(dbs);
    let error = backsync_latest_for_bookmark(
        ctx.clone(),
        data,
        dbs.clone(),
        bookmark.clone(),
        BacksyncLimit::NoLimit,
        Arc::new(AtomicBool::new(false)),
        CommitSyncContext::Tests,
        false,
    )
    .await
    .expect_err("the conflicting target create must fail");
    assert!(
        error
            .to_string()
            .contains("backsync transaction failed, but the counter didn't move forward")
    );
    assert_eq!(dbs.counters.get_counter(&ctx, &counter).await?, Some(0));
    assert_eq!(
        dbs.bookmarks
            .get(ctx.clone(), &bookmark, Freshness::MostRecent)
            .await?,
        Some(target_value)
    );
    assert_eq!(
        dbs.bookmark_update_log
            .list_bookmark_log_entries(ctx, bookmark, 100, None, Freshness::MostRecent)
            .try_collect::<Vec<_>>()
            .await?
            .len(),
        1
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn shared_initializer_retains_insert_if_absent_race_contract(
    fb: FacebookInit,
) -> Result<(), Error> {
    let (data, dbs) = init_repos(fb, MoverType::Noop, BookmarkRenamerType::Noop).await?;
    let ctx = CoreContext::test_mock(fb);
    let counter = format_bookmark_counter(&data.get_source_repo_id(), &BookmarkKey::new("race")?)?;
    let (first, second) = future::try_join(
        get_bookmark_counter_or_initialize(&ctx, &dbs.counters, &counter, BookmarkUpdateLogId(0)),
        get_bookmark_counter_or_initialize(&ctx, &dbs.counters, &counter, BookmarkUpdateLogId(17)),
    )
    .await?;
    let stored: BookmarkUpdateLogId = dbs
        .counters
        .get_counter(&ctx, &counter)
        .await?
        .expect("one initializer wins")
        .try_into()?;
    assert!(stored == BookmarkUpdateLogId(0) || stored == BookmarkUpdateLogId(17));
    assert!(first.is_some() || second.is_some());
    assert!(first.is_none_or(|value| value == stored));
    assert!(second.is_none_or(|value| value == stored));
    assert_eq!(
        get_bookmark_counter_or_initialize(&ctx, &dbs.counters, &counter, BookmarkUpdateLogId(99))
            .await?,
        Some(stored)
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn existing_cursor_above_retained_log_is_not_normalized(
    fb: FacebookInit,
) -> Result<(), Error> {
    let (data, dbs) = init_repos(fb, MoverType::Noop, BookmarkRenamerType::Noop).await?;
    let ctx = CoreContext::test_mock(fb);
    let bookmark = BookmarkKey::new("anotherbookmark")?;
    let counter = format_bookmark_counter(&data.get_source_repo_id(), &bookmark)?;
    assert!(
        dbs.counters
            .set_counter_if_absent(&ctx, &counter, 9000)
            .await?
    );
    let dbs = Arc::new(dbs);
    let delay = backsync_latest_for_bookmark(
        ctx.clone(),
        data,
        dbs.clone(),
        bookmark.clone(),
        BacksyncLimit::NoLimit,
        Arc::new(AtomicBool::new(false)),
        CommitSyncContext::Tests,
        false,
    )
    .await?;
    assert_eq!(delay.remaining_entries, 0);
    assert_eq!(dbs.counters.get_counter(&ctx, &counter).await?, Some(9000));
    assert_eq!(
        dbs.bookmarks
            .get(ctx, &bookmark, Freshness::MostRecent)
            .await?,
        None
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn missing_bookmark_cursor_initialization_race_retries(
    fb: FacebookInit,
) -> Result<(), Error> {
    let (data, dbs) = init_repos(fb, MoverType::Noop, BookmarkRenamerType::Noop).await?;
    let ctx = CoreContext::test_mock(fb);
    let dbs = Arc::new(dbs);
    let bookmark = BookmarkKey::new("anotherbookmark")?;
    let counter = format_bookmark_counter(&data.get_source_repo_id(), &bookmark)?;
    let (first, second) = future::try_join(
        backsync_latest_for_bookmark(
            ctx.clone(),
            data.clone(),
            dbs.clone(),
            bookmark.clone(),
            BacksyncLimit::Limit(0),
            Arc::new(AtomicBool::new(false)),
            CommitSyncContext::Tests,
            false,
        ),
        backsync_latest_for_bookmark(
            ctx.clone(),
            data.clone(),
            dbs.clone(),
            bookmark.clone(),
            BacksyncLimit::Limit(0),
            Arc::new(AtomicBool::new(false)),
            CommitSyncContext::Tests,
            false,
        ),
    )
    .await?;
    assert_eq!(first.failed_bookmarks + second.failed_bookmarks, 0);
    assert!(first.remaining_entries + second.remaining_entries <= 1);
    assert_eq!(dbs.counters.get_counter(&ctx, &counter).await?, Some(0));
    assert_eq!(
        dbs.bookmarks
            .get(ctx.clone(), &bookmark, Freshness::MostRecent)
            .await?,
        None
    );
    backsync_latest_for_bookmark(
        ctx.clone(),
        data,
        dbs.clone(),
        bookmark.clone(),
        BacksyncLimit::NoLimit,
        Arc::new(AtomicBool::new(false)),
        CommitSyncContext::Tests,
        false,
    )
    .await?;
    assert!(
        dbs.counters
            .get_counter(&ctx, &counter)
            .await?
            .is_some_and(|value| value > 0)
    );
    assert!(
        dbs.bookmarks
            .get(ctx, &bookmark, Freshness::MostRecent)
            .await?
            .is_some()
    );
    Ok(())
}
