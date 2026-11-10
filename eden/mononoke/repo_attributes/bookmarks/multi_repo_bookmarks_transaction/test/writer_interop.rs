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

async fn guard_max(f: &TwoRepoFixture, repo_id: RepositoryId) -> Result<Option<u64>> {
    let rows =
        FindMaxBookmarkLogLockId::query(&f.conn, f.ctx.sql_query_telemetry(), &repo_id).await?;
    assert_eq!(rows.len(), 1);
    Ok(rows[0].0)
}

#[mononoke::fbinit_test]
async fn single_and_multi_repo_writers_share_tickets(fb: FacebookInit) -> Result<()> {
    let knobs = JustKnobsInMemory::new(HashMap::from([
        (
            "scm/mononoke_bookmark_log_migration:bookmark_log_guard_enabled".to_owned(),
            KnobVal::Bool(true),
        ),
        (
            "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_guard_enabled".to_owned(),
            KnobVal::Bool(true),
        ),
    ]));
    with_just_knobs_async(
        knobs,
        async move {
            let f = TwoRepoFixture::new(fb)?;
            let first = BookmarkKey::new("heads/first")?;
            let second = BookmarkKey::new("heads/second")?;
            f.set_bookmark(&f.bookmarks_1, &first, ONES_CSID).await?;
            f.set_bookmark(&f.bookmarks_1, &second, ONES_CSID).await?;
            f.set_bookmark(&f.bookmarks_2, &first, ONES_CSID).await?;
            assert_eq!(guard_max(&f, f.repo_id_1).await?, Some(2));
            assert_eq!(guard_max(&f, f.repo_id_2).await?, Some(1));
            let mut txn = f.multi_txn();
            for key in [&first, &second] {
                txn.update(
                    f.repo_id_1,
                    key,
                    TWOS_CSID,
                    ONES_CSID,
                    BookmarkUpdateReason::TestMove,
                )?;
            }
            txn.update(
                f.repo_id_2,
                &first,
                TWOS_CSID,
                ONES_CSID,
                BookmarkUpdateReason::TestMove,
            )?;
            assert!(txn.commit().await?.is_success());
            assert_eq!(guard_max(&f, f.repo_id_1).await?, Some(4));
            assert_eq!(guard_max(&f, f.repo_id_2).await?, Some(2));
            f.set_bookmark(&f.bookmarks_1, &first, THREES_CSID).await?;
            assert_eq!(guard_max(&f, f.repo_id_1).await?, Some(5));
            Ok(())
        }
        .boxed(),
    )
    .await
}
