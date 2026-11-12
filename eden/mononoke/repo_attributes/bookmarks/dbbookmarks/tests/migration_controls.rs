/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::array::from_fn;

use anyhow::Error;
use anyhow::Result;
use dbbookmarks::transaction::BookmarkLogWriter;
use dbbookmarks::transaction::bookmark_log_guard_enabled;
use dbbookmarks::transaction::bookmark_log_guard_enabled_for_allocation;
use dbbookmarks::transaction::bookmark_log_ids_by_name;
use fbinit::FacebookInit;
use futures::FutureExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use mononoke_macros::mononoke;
use mononoke_types_mocks::repo::REPO_ZERO;

const CONTROLS: [&str; 4] = [
    "scm/mononoke_bookmark_log_migration:bookmark_log_guard_enabled",
    "scm/mononoke_bookmark_log_migration:bookmark_log_ids_by_name",
    "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_guard_enabled",
    "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_ids_by_name",
];

#[mononoke::fbinit_test]
async fn migration_controls_default_off(_fb: FacebookInit) -> Result<()> {
    for writer in [BookmarkLogWriter::SingleRepo, BookmarkLogWriter::MultiRepo] {
        assert!(!bookmark_log_guard_enabled(REPO_ZERO, writer));
        assert!(!bookmark_log_ids_by_name(REPO_ZERO, writer));
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn producer_selection_and_fail_closed_allocation(_fb: FacebookInit) -> Result<()> {
    for bits in 0..16 {
        let values: [bool; 4] = from_fn(|bit| bits & (1 << bit) != 0);
        let [single_enabled, single_by_name, multi_enabled, multi_by_name] = values;
        let knobs = JustKnobsInMemory::new(
            CONTROLS
                .into_iter()
                .zip(values)
                .map(|(name, value)| (name.to_owned(), KnobVal::Bool(value)))
                .collect(),
        );
        with_just_knobs_async(
            knobs,
            async move {
                for (writer, enabled, by_name) in [
                    (
                        BookmarkLogWriter::SingleRepo,
                        single_enabled,
                        single_by_name,
                    ),
                    (BookmarkLogWriter::MultiRepo, multi_enabled, multi_by_name),
                ] {
                    assert_eq!(bookmark_log_guard_enabled(REPO_ZERO, writer), enabled);
                    assert_eq!(bookmark_log_ids_by_name(REPO_ZERO, writer), by_name);
                    let activated =
                        bookmark_log_guard_enabled_for_allocation(REPO_ZERO, by_name, enabled);
                    if by_name && !enabled {
                        assert!(activated.is_err(), "{writer:?}: {values:?}");
                    } else {
                        assert_eq!(activated?, enabled);
                    }
                }
                Ok::<_, Error>(())
            }
            .boxed(),
        )
        .await?;
    }
    Ok(())
}
