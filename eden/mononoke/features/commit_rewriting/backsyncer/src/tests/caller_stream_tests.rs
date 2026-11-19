/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Error;
use async_trait::async_trait;
use bookmarks::BookmarkCategory;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkPrefix;
use bookmarks::BookmarkUpdateLogId;
use bookmarks::BookmarkUpdateLogRef;
use bookmarks::Freshness;
use context::CoreContext;
use cross_repo_sync::CommitSyncContext;
use fbinit::FacebookInit;
use futures::FutureExt;
use futures::TryStreamExt;
use futures::future;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use maplit::hashmap;
use mononoke_macros::mononoke;
use mutable_counters::ArcMutableCounters;
use mutable_counters::MutableCounters;
use tests_utils::resolve_cs_id;

use super::BookmarkRenamerType;
use super::MoverType;
use super::delete_bookmark;
use super::init_repos;
use super::move_bookmark;
use crate::BOOKMARK_DISCOVERY_PAGE_SIZE_JUST_KNOB;
use crate::BacksyncLimit;
use crate::MAX_DISCOVERED_BOOKMARKS_JUST_KNOB;
use crate::backsync_bookmark_once;
use crate::backsync_latest_for_bookmark;
use crate::backsync_once_by_prefix;
use crate::format_bookmark_counter;
use crate::format_counter;

#[mononoke::fbinit_test]
async fn known_transient_bookmark_does_not_require_snapshot_discovery(
    fb: FacebookInit,
) -> Result<(), Error> {
    let (data, dbs) = init_repos(fb, MoverType::Noop, BookmarkRenamerType::Noop).await?;
    let ctx = CoreContext::test_mock(fb);
    let bookmark = BookmarkKey::new("transient")?;
    let source_value = resolve_cs_id(&ctx, data.get_source_repo(), "master").await?;
    move_bookmark(
        ctx.clone(),
        data.get_source_repo().clone(),
        &bookmark,
        source_value,
    )
    .await?;
    delete_bookmark(ctx.clone(), data.get_source_repo().clone(), &bookmark).await?;
    for name in ["unrelated/one", "unrelated/two"] {
        move_bookmark(
            ctx.clone(),
            data.get_source_repo().clone(),
            &BookmarkKey::new(name)?,
            source_value,
        )
        .await?;
    }
    let dbs = Arc::new(dbs);
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
    with_just_knobs_async(
        JustKnobsInMemory::new(hashmap! {
            BOOKMARK_DISCOVERY_PAGE_SIZE_JUST_KNOB.to_owned() => KnobVal::Int(1),
            MAX_DISCOVERED_BOOKMARKS_JUST_KNOB.to_owned() => KnobVal::Int(1),
        }),
        backsync_bookmark_once(
            ctx.clone(),
            data.clone(),
            dbs.clone(),
            bookmark.clone(),
            BacksyncLimit::NoLimit,
            Arc::new(AtomicBool::new(false)),
            CommitSyncContext::PushRedirector,
            true,
            Box::new(future::ready(())),
        )
        .boxed(),
    )
    .await?
    .1
    .await;
    assert_eq!(
        dbs.bookmarks
            .get(ctx.clone(), &bookmark, Freshness::MostRecent)
            .await?,
        None
    );
    let source_entries = data
        .get_source_repo()
        .bookmark_update_log()
        .read_next_bookmark_log_entries_by_bookmark(
            ctx.clone(),
            bookmark.clone(),
            BookmarkUpdateLogId(0),
            10,
            Freshness::MostRecent,
        )
        .try_collect::<Vec<_>>()
        .await?;
    assert_eq!(source_entries.len(), 2);
    assert_eq!(
        dbs.counters
            .get_counter(
                &ctx,
                &format_bookmark_counter(&data.get_source_repo_id(), &bookmark)?
            )
            .await?,
        Some(source_entries.last().unwrap().id.try_into()?)
    );
    let history = dbs
        .bookmark_update_log
        .list_bookmark_log_entries(ctx.clone(), bookmark, 10, None, Freshness::MostRecent)
        .try_collect::<Vec<_>>()
        .await?;
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].1, None);
    assert!(history[1].1.is_some());
    assert!(
        data.get_commit_sync_outcome(&ctx, source_value)
            .await?
            .is_some()
    );
    assert_eq!(
        dbs.counters
            .get_counter(
                &ctx,
                &format_bookmark_counter(
                    &data.get_source_repo_id(),
                    &BookmarkKey::new("unrelated/one")?
                )?
            )
            .await?,
        None
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn once_snapshot_is_prefix_scoped_not_a_historical_sweep(
    fb: FacebookInit,
) -> Result<(), Error> {
    let (data, dbs) = init_repos(
        fb,
        MoverType::Noop,
        BookmarkRenamerType::CommonAndPrefix(BookmarkKey::new("master")?, "small/".to_owned()),
    )
    .await?;
    let ctx = CoreContext::test_mock(fb);
    let value = resolve_cs_id(&ctx, data.get_source_repo(), "master").await?;
    let selected = BookmarkKey::new("small/selected")?;
    let transient = BookmarkKey::new("small/transient")?;
    for bookmark in [
        &selected,
        &transient,
        &BookmarkKey::new("unrelated/one")?,
        &BookmarkKey::new("unrelated/two")?,
    ] {
        move_bookmark(ctx.clone(), data.get_source_repo().clone(), bookmark, value).await?;
    }
    delete_bookmark(ctx.clone(), data.get_source_repo().clone(), &transient).await?;
    let dbs = Arc::new(dbs);
    let delay = with_just_knobs_async(
        JustKnobsInMemory::new(hashmap! {
            BOOKMARK_DISCOVERY_PAGE_SIZE_JUST_KNOB.to_owned() => KnobVal::Int(1),
            MAX_DISCOVERED_BOOKMARKS_JUST_KNOB.to_owned() => KnobVal::Int(1),
        }),
        backsync_once_by_prefix(
            ctx.clone(),
            data.clone(),
            dbs.clone(),
            BookmarkPrefix::new("small/")?,
            &[],
            2,
            Arc::new(AtomicBool::new(false)),
            CommitSyncContext::Backsyncer,
            false,
        )
        .boxed(),
    )
    .await?;
    assert_eq!(delay.failed_bookmarks, 0);
    assert!(
        dbs.bookmarks
            .get(
                ctx.clone(),
                &BookmarkKey::new("selected")?,
                Freshness::MostRecent
            )
            .await?
            .is_some()
    );
    assert_eq!(
        dbs.counters
            .get_counter(
                &ctx,
                &format_bookmark_counter(&data.get_source_repo_id(), &transient)?
            )
            .await?,
        None
    );
    assert_eq!(
        dbs.counters
            .get_counter(
                &ctx,
                &format_bookmark_counter(
                    &data.get_source_repo_id(),
                    &BookmarkKey::new("unrelated/one")?
                )?
            )
            .await?,
        None
    );
    backsync_bookmark_once(
        ctx.clone(),
        data.clone(),
        dbs.clone(),
        transient.clone(),
        BacksyncLimit::NoLimit,
        Arc::new(AtomicBool::new(false)),
        CommitSyncContext::RepoImport,
        false,
        Box::new(future::ready(())),
    )
    .await?
    .1
    .await;
    assert_eq!(
        dbs.bookmark_update_log
            .list_bookmark_log_entries(
                ctx,
                BookmarkKey::new("transient")?,
                10,
                None,
                Freshness::MostRecent
            )
            .try_collect::<Vec<_>>()
            .await?
            .len(),
        2
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn tag_stream_replays_creation_deletion_and_recreation(
    fb: FacebookInit,
) -> Result<(), Error> {
    let (data, dbs) = init_repos(
        fb,
        MoverType::Noop,
        BookmarkRenamerType::CommonAndPrefix(BookmarkKey::new("master")?, "small/".to_owned()),
    )
    .await?;
    let ctx = CoreContext::test_mock(fb);
    let source_tag = BookmarkKey::with_name_and_category(
        BookmarkKey::new("small/tag")?.into_name(),
        BookmarkCategory::Tag,
    );
    let target_tag = BookmarkKey::with_name_and_category(
        BookmarkKey::new("tag")?.into_name(),
        BookmarkCategory::Tag,
    );
    let value = resolve_cs_id(&ctx, data.get_source_repo(), "master").await?;
    move_bookmark(
        ctx.clone(),
        data.get_source_repo().clone(),
        &source_tag,
        value,
    )
    .await?;
    delete_bookmark(ctx.clone(), data.get_source_repo().clone(), &source_tag).await?;
    move_bookmark(
        ctx.clone(),
        data.get_source_repo().clone(),
        &source_tag,
        value,
    )
    .await?;
    let dbs = Arc::new(dbs);
    backsync_latest_for_bookmark(
        ctx.clone(),
        data.clone(),
        dbs.clone(),
        source_tag.clone(),
        BacksyncLimit::NoLimit,
        Arc::new(AtomicBool::new(false)),
        CommitSyncContext::Tests,
        false,
    )
    .await?;
    let current = dbs
        .bookmarks
        .get(ctx.clone(), &target_tag, Freshness::MostRecent)
        .await?;
    assert!(current.is_some());
    assert_eq!(
        dbs.bookmarks
            .get(
                ctx.clone(),
                &BookmarkKey::new("tag")?,
                Freshness::MostRecent
            )
            .await?,
        None
    );
    let history = dbs
        .bookmark_update_log
        .list_bookmark_log_entries(ctx.clone(), target_tag, 10, None, Freshness::MostRecent)
        .try_collect::<Vec<_>>()
        .await?;
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].1, current);
    assert_eq!(history[1].1, None);
    assert_eq!(history[2].1, current);
    let last = data
        .get_source_repo()
        .bookmark_update_log()
        .read_next_bookmark_log_entries_by_bookmark(
            ctx.clone(),
            source_tag.clone(),
            BookmarkUpdateLogId(0),
            10,
            Freshness::MostRecent,
        )
        .try_collect::<Vec<_>>()
        .await?
        .last()
        .unwrap()
        .id;
    assert_eq!(
        dbs.counters
            .get_counter(
                &ctx,
                &format_bookmark_counter(&data.get_source_repo_id(), &source_tag)?
            )
            .await?,
        Some(last.try_into()?)
    );
    assert_eq!(
        dbs.counters
            .get_counter(
                &ctx,
                &format_bookmark_counter(
                    &data.get_source_repo_id(),
                    &BookmarkKey::new("small/tag")?
                )?
            )
            .await?,
        None
    );
    Ok(())
}

struct LoseFirstInitialization {
    inner: ArcMutableCounters,
    lost: AtomicBool,
}

#[async_trait]
impl MutableCounters for LoseFirstInitialization {
    async fn get_counter(&self, ctx: &CoreContext, name: &str) -> Result<Option<i64>, Error> {
        self.inner.get_counter(ctx, name).await
    }
    async fn get_maybe_stale_counter(
        &self,
        ctx: &CoreContext,
        name: &str,
    ) -> Result<Option<i64>, Error> {
        self.inner.get_maybe_stale_counter(ctx, name).await
    }
    async fn set_counter(
        &self,
        ctx: &CoreContext,
        name: &str,
        value: i64,
        previous: Option<i64>,
    ) -> Result<bool, Error> {
        self.inner.set_counter(ctx, name, value, previous).await
    }
    async fn set_counters(&self, ctx: &CoreContext, values: &[(&str, i64)]) -> Result<(), Error> {
        self.inner.set_counters(ctx, values).await
    }
    async fn set_counter_if_absent(
        &self,
        ctx: &CoreContext,
        name: &str,
        value: i64,
    ) -> Result<bool, Error> {
        let inserted = self.inner.set_counter_if_absent(ctx, name, value).await?;
        if !self.lost.swap(true, Ordering::SeqCst) {
            // Simulate another initializer inserting the same zero cursor just
            // before this worker's insert. The durable SQL cursor is real.
            assert!(inserted);
            assert_eq!(value, 0);
            Ok(false)
        } else {
            Ok(inserted)
        }
    }
    async fn get_all_counters(&self, ctx: &CoreContext) -> Result<Vec<(String, i64)>, Error> {
        self.inner.get_all_counters(ctx).await
    }
}

#[mononoke::fbinit_test]
async fn one_shot_retries_a_deterministic_initialization_loser(
    fb: FacebookInit,
) -> Result<(), Error> {
    let (data, mut dbs) = init_repos(fb, MoverType::Noop, BookmarkRenamerType::Noop).await?;
    let ctx = CoreContext::test_mock(fb);
    let counters = Arc::new(LoseFirstInitialization {
        inner: dbs.counters.clone(),
        lost: AtomicBool::new(false),
    });
    dbs.counters = counters.clone();
    let dbs = Arc::new(dbs);
    let bookmark = BookmarkKey::new("anotherbookmark")?;
    let (delay, future) = backsync_bookmark_once(
        ctx.clone(),
        data.clone(),
        dbs.clone(),
        bookmark.clone(),
        BacksyncLimit::NoLimit,
        Arc::new(AtomicBool::new(false)),
        CommitSyncContext::PushRedirector,
        false,
        Box::new(future::ready(())),
    )
    .await?;
    future.await;
    assert!(counters.lost.load(Ordering::SeqCst));
    assert!(delay.remaining_entries > 1);
    assert!(
        dbs.bookmarks
            .get(ctx.clone(), &bookmark, Freshness::MostRecent)
            .await?
            .is_some()
    );
    assert!(
        dbs.counters
            .get_counter(
                &ctx,
                &format_bookmark_counter(&data.get_source_repo_id(), &bookmark)?
            )
            .await?
            .is_some_and(|value| value > 0)
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn known_unmapped_stream_keeps_commit_only_future_nonblocking(
    fb: FacebookInit,
) -> Result<(), Error> {
    let (data, dbs) = init_repos(
        fb,
        MoverType::Noop,
        BookmarkRenamerType::Only(BookmarkKey::new("master")?),
    )
    .await?;
    let ctx = CoreContext::test_mock(fb);
    let bookmark = BookmarkKey::new("unmapped")?;
    let value = resolve_cs_id(&ctx, data.get_source_repo(), "master").await?;
    move_bookmark(
        ctx.clone(),
        data.get_source_repo().clone(),
        &bookmark,
        value,
    )
    .await?;
    let dbs = Arc::new(dbs);
    let (_, future) = tokio::time::timeout(
        Duration::from_secs(1),
        backsync_bookmark_once(
            ctx.clone(),
            data.clone(),
            dbs.clone(),
            bookmark.clone(),
            BacksyncLimit::NoLimit,
            Arc::new(AtomicBool::new(false)),
            CommitSyncContext::Tests,
            false,
            Box::new(future::pending()),
        ),
    )
    .await
    .expect("commit-only work must not block the known-stream call")?;
    drop(future);
    assert_eq!(
        dbs.bookmarks
            .get(ctx.clone(), &bookmark, Freshness::MostRecent)
            .await?,
        None
    );
    assert!(
        dbs.counters
            .get_counter(
                &ctx,
                &format_bookmark_counter(&data.get_source_repo_id(), &bookmark)?
            )
            .await?
            .is_some_and(|value| value > 0)
    );
    Ok(())
}
