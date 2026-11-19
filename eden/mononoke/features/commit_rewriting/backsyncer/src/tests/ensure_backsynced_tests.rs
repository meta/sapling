/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Error;
use bookmarks::BookmarkCategory;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkUpdateLogId;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::FutureExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use maplit::hashmap;
use mononoke_macros::mononoke;
use mononoke_types::RepositoryId;
use repo_identity::RepoIdentityRef;

use super::BookmarkRenamerType;
use super::MoverType;
use super::init_repos;
use crate::ensure_backsynced;
use crate::format_bookmark_counter;
use crate::format_counter;

#[mononoke::fbinit_test]
async fn acknowledgment_requires_exact_bookmark_progress(fb: FacebookInit) -> Result<(), Error> {
    let (commit_sync_data, small_repo_dbs) =
        init_repos(fb, MoverType::Noop, BookmarkRenamerType::Noop).await?;
    let ctx = CoreContext::test_mock(fb);
    let source_repo_id = commit_sync_data.get_source_repo().repo_identity().id();
    let small_repo_dbs = Arc::new(small_repo_dbs);
    assert!(
        small_repo_dbs
            .counters
            .set_counter(&ctx, &format_counter(&source_repo_id), 1000, Some(0),)
            .await?
    );

    let jk = JustKnobsInMemory::new(hashmap! {
        "scm/mononoke:defer_to_backsyncer_for_backsync_timeout_seconds".to_owned()
            => KnobVal::Int(1),
    });
    with_just_knobs_async(
        jk,
        async {
            for (name, initial_counter) in [("behind-bookmark", Some(700)), ("new-bookmark", None)]
            {
                let bookmark = BookmarkKey::new(name)?;
                let counter_name = format_bookmark_counter(&source_repo_id, &bookmark)?;
                let tag = BookmarkKey::with_name_and_category(
                    bookmark.name().clone(),
                    BookmarkCategory::Tag,
                );
                for wrong_key in [
                    format_bookmark_counter(&source_repo_id, &tag)?,
                    format_bookmark_counter(&RepositoryId::new(999), &bookmark)?,
                ] {
                    assert!(
                        small_repo_dbs
                            .counters
                            .set_counter_if_absent(&ctx, &wrong_key, 1000)
                            .await?
                    );
                }
                if let Some(counter) = initial_counter {
                    assert!(
                        small_repo_dbs
                            .counters
                            .set_counter_if_absent(&ctx, &counter_name, counter)
                            .await?
                    );
                }
                let error = ensure_backsynced(
                    ctx.clone(),
                    commit_sync_data.clone(),
                    small_repo_dbs.clone(),
                    &bookmark,
                    BookmarkUpdateLogId(701),
                )
                .await
                .expect_err("global or another stream's progress must not acknowledge this update");
                assert_eq!(
                    error.to_string(),
                    "Timeout expired while waiting for backsyncing"
                );
                assert_eq!(
                    small_repo_dbs
                        .counters
                        .get_counter(&ctx, &counter_name)
                        .await?,
                    initial_counter
                );
                assert!(
                    small_repo_dbs
                        .counters
                        .set_counter(&ctx, &counter_name, 701, None)
                        .await?
                );
                ensure_backsynced(
                    ctx.clone(),
                    commit_sync_data.clone(),
                    small_repo_dbs.clone(),
                    &bookmark,
                    BookmarkUpdateLogId(701),
                )
                .await?;
            }
            Ok::<_, Error>(())
        }
        .boxed(),
    )
    .await
}
