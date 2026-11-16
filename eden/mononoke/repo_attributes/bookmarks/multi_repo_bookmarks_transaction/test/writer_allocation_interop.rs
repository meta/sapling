/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;

use dbbookmarks::transaction::FindMaxBookmarkLogLockId;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;

use super::*;

async fn assert_independent_writer_modes(fb: FacebookInit, single_by_name: bool) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    let knobs = JustKnobsInMemory::new(HashMap::from([
        (
            "scm/mononoke_bookmark_log_migration:bookmark_log_guard_enabled".to_owned(),
            KnobVal::Bool(true),
        ),
        (
            "scm/mononoke_bookmark_log_migration:bookmark_log_ids_by_name".to_owned(),
            KnobVal::Bool(single_by_name),
        ),
        (
            "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_guard_enabled".to_owned(),
            KnobVal::Bool(true),
        ),
        (
            "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_ids_by_name".to_owned(),
            KnobVal::Bool(!single_by_name),
        ),
        (
            "scm/mononoke_bookmark_log_migration:bookmark_log_guard_retired".to_owned(),
            KnobVal::Bool(true),
        ),
    ]));
    with_just_knobs_async(
        knobs,
        async {
            let first = BookmarkKey::new("heads/first")?;
            let second = BookmarkKey::new("heads/second")?;
            let third = BookmarkKey::new("heads/third")?;
            f.set_bookmark(&f.bookmarks_1, &first, ONES_CSID).await?;
            let mut txn = f.multi_txn();
            txn.create(f.repo_id_1, &second, ONES_CSID, BookmarkUpdateReason::TestMove)?;
            assert!(txn.commit().await?.is_success());
            assert_eq!(f.bookmarks_1.get_raw(f.ctx.clone(), &second, Freshness::MostRecent).await?, Some((ONES_CSID, Some(if single_by_name { 2 } else { 1 }))));
            f.set_bookmark(&f.bookmarks_1, &third, ONES_CSID).await?;
            assert_eq!(f.bookmarks_1.get_raw(f.ctx.clone(), &third, Freshness::MostRecent).await?, Some((ONES_CSID, Some(if single_by_name { 1 } else { 2 }))));
            let mut txn = f.multi_txn();
            txn.update(f.repo_id_1, &first, TWOS_CSID, ONES_CSID, BookmarkUpdateReason::TestMove)?;
            assert!(txn.commit().await?.is_success());
            let entries: Vec<_> = f.bookmarks_1.read_next_bookmark_log_entries_by_bookmark(
                f.ctx.clone(), first, BookmarkUpdateLogId(0), 10, Freshness::MostRecent,
            ).try_collect().await?;
            assert_eq!(entries.iter().map(|entry| entry.id.0).collect::<Vec<_>>(), vec![1, if single_by_name { 3 } else { 2 }]);
            let guards = FindMaxBookmarkLogLockId::query(&f.conn, f.ctx.sql_query_telemetry(), &f.repo_id_1).await?;
            assert_eq!(guards[0].0, Some(4), "partial name migration must keep both producers guarded even with retirement enabled");
            Ok(())
        }.boxed(),
    ).await
}

#[mononoke::fbinit_test]
async fn mrls_name_mode_is_isolated_from_single_repo_legacy(fb: FacebookInit) -> Result<()> {
    assert_independent_writer_modes(fb, false).await
}

#[mononoke::fbinit_test]
async fn single_repo_name_mode_is_isolated_from_mrls_legacy(fb: FacebookInit) -> Result<()> {
    assert_independent_writer_modes(fb, true).await
}
