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

#[mononoke::fbinit_test]
async fn mrls_keeps_tickets_until_single_repo_name_mode_is_activated(
    fb: FacebookInit,
) -> Result<()> {
    for (single_enabled, single_by_name) in [(false, false), (true, false), (false, true)] {
        let f = TwoRepoFixture::new(fb)?;
        let knobs = JustKnobsInMemory::new(HashMap::from([
            (
                "scm/mononoke_bookmark_log_migration:bookmark_log_guard_enabled".to_owned(),
                KnobVal::Bool(single_enabled),
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
                KnobVal::Bool(true),
            ),
            (
                "scm/mononoke_bookmark_log_migration:bookmark_log_guard_retired".to_owned(),
                KnobVal::Bool(true),
            ),
        ]));
        with_just_knobs_async(
            knobs,
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
                for (repo_id, store) in
                    [(f.repo_id_1, &f.bookmarks_1), (f.repo_id_2, &f.bookmarks_2)]
                {
                    let guards = FindMaxBookmarkLogLockId::query(
                        &f.conn,
                        f.ctx.sql_query_telemetry(),
                        &repo_id,
                    )
                    .await?;
                    assert_eq!(guards[0].0, Some(2));
                    assert_eq!(
                        store
                            .get_raw(
                                f.ctx.clone(),
                                &BookmarkKey::new("heads/b")?,
                                Freshness::MostRecent
                            )
                            .await?,
                        Some((ONES_CSID, Some(1)))
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
