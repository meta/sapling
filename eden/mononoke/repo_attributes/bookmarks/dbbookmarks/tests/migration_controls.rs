/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;

use anyhow::Error;
use anyhow::Result;
use dbbookmarks::transaction::BookmarkLogWriter;
use dbbookmarks::transaction::bookmark_log_guard_enabled;
use fbinit::FacebookInit;
use futures::FutureExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use mononoke_macros::mononoke;
use mononoke_types_mocks::repo::REPO_ZERO;

#[mononoke::fbinit_test]
async fn migration_controls_default_off(_fb: FacebookInit) -> Result<()> {
    for writer in [BookmarkLogWriter::SingleRepo, BookmarkLogWriter::MultiRepo] {
        assert!(!bookmark_log_guard_enabled(REPO_ZERO, writer));
    }
    for single_enabled in [false, true] {
        for multi_enabled in [false, true] {
            let knobs = JustKnobsInMemory::new(HashMap::from([
                (
                    "scm/mononoke_bookmark_log_migration:bookmark_log_guard_enabled".to_owned(),
                    KnobVal::Bool(single_enabled),
                ),
                (
                    "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_guard_enabled"
                        .to_owned(),
                    KnobVal::Bool(multi_enabled),
                ),
            ]));
            with_just_knobs_async(
                knobs,
                async move {
                    assert_eq!(
                        bookmark_log_guard_enabled(REPO_ZERO, BookmarkLogWriter::SingleRepo),
                        single_enabled
                    );
                    assert_eq!(
                        bookmark_log_guard_enabled(REPO_ZERO, BookmarkLogWriter::MultiRepo),
                        multi_enabled
                    );
                    Ok::<_, Error>(())
                }
                .boxed(),
            )
            .await?;
        }
    }
    Ok(())
}
