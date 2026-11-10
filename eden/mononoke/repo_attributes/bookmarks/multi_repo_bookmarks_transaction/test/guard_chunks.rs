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

#[mononoke::fbinit_test]
async fn guard_tickets_continue_across_interleaved_log_chunks(fb: FacebookInit) -> Result<()> {
    let f = TwoRepoFixture::new(fb)?;
    let knobs = JustKnobsInMemory::new(HashMap::from([(
        "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_guard_enabled".to_owned(),
        KnobVal::Bool(true),
    )]));
    with_just_knobs_async(
        knobs,
        async {
            let mut initial = f.multi_txn();
            for index in 0..110 {
                let key = BookmarkKey::new(format!("heads/key-{index:03}"))?;
                for repo_id in [f.repo_id_1, f.repo_id_2] {
                    initial.create(repo_id, &key, ONES_CSID, BookmarkUpdateReason::TestMove)?;
                }
            }
            assert!(initial.commit().await?.is_success());
            let mut updates = f.multi_txn();
            for index in 0..110 {
                let key = BookmarkKey::new(format!("heads/key-{index:03}"))?;
                for repo_id in [f.repo_id_1, f.repo_id_2] {
                    updates.update(
                        repo_id,
                        &key,
                        TWOS_CSID,
                        ONES_CSID,
                        BookmarkUpdateReason::TestMove,
                    )?;
                }
            }
            assert!(updates.commit().await?.is_success());
            for (repo_id, store) in [(f.repo_id_1, &f.bookmarks_1), (f.repo_id_2, &f.bookmarks_2)] {
                let guards =
                    FindMaxBookmarkLogLockId::query(&f.conn, f.ctx.sql_query_telemetry(), &repo_id)
                        .await?;
                assert_eq!(guards[0].0, Some(220));
                let entries: Vec<_> = store
                    .read_next_bookmark_log_entries(
                        f.ctx.clone(),
                        BookmarkUpdateLogId(0),
                        500,
                        Freshness::MostRecent,
                    )
                    .try_collect()
                    .await?;
                assert_eq!(entries.len(), 220);
                assert_eq!(
                    entries.iter().map(|entry| entry.id.0).collect::<Vec<_>>(),
                    (1..=220).collect::<Vec<_>>()
                );
            }
            Ok(())
        }
        .boxed(),
    )
    .await
}
