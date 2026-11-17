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
use bookmarks::BookmarkUpdateReason;
use bookmarks::Bookmarks;
use bookmarks::Freshness;
use context::CoreContext;
use dbbookmarks::SqlBookmarksBuilder;
use dbbookmarks::transaction::FindMaxBookmarkLogLockId;
use fbinit::FacebookInit;
use futures::FutureExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use mononoke_macros::mononoke;
use mononoke_types_mocks::changesetid::ONES_CSID;
use mononoke_types_mocks::repo::REPO_ZERO;
use sql_construct::SqlConstruct;

#[mononoke::fbinit_test]
async fn single_repo_keeps_tickets_until_mrls_name_mode_is_activated(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    for (multi_enabled, multi_by_name) in [(false, false), (true, false), (false, true)] {
        let store = SqlBookmarksBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
        let knobs = JustKnobsInMemory::new(HashMap::from([
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
                KnobVal::Bool(multi_enabled),
            ),
            (
                "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_ids_by_name".to_owned(),
                KnobVal::Bool(multi_by_name),
            ),
            (
                "scm/mononoke_bookmark_log_migration:bookmark_log_guard_retired".to_owned(),
                KnobVal::Bool(true),
            ),
        ]));
        with_just_knobs_async(
            knobs,
            async {
                for name in ["heads/a", "heads/b"] {
                    let mut txn = store.create_transaction(ctx.clone());
                    txn.create(
                        &BookmarkKey::new(name)?,
                        ONES_CSID,
                        BookmarkUpdateReason::TestMove,
                    )?;
                    assert_eq!(txn.commit().await?, Some(1));
                }
                let guards = FindMaxBookmarkLogLockId::query(
                    store.write_connection(),
                    ctx.sql_query_telemetry(),
                    &REPO_ZERO,
                )
                .await?;
                assert_eq!(guards[0].0, Some(2));
                assert_eq!(
                    store
                        .get_raw(
                            ctx.clone(),
                            &BookmarkKey::new("heads/b")?,
                            Freshness::MostRecent
                        )
                        .await?,
                    Some((ONES_CSID, Some(1)))
                );
                Ok::<_, Error>(())
            }
            .boxed(),
        )
        .await?;
    }
    Ok(())
}
