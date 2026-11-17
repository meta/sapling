/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Error;
use anyhow::Result;
use dbbookmarks::transaction::BookmarkLogWriter;
use dbbookmarks::transaction::bookmark_log_guard_enabled;
use dbbookmarks::transaction::bookmark_log_guard_enabled_for_allocation;
use dbbookmarks::transaction::bookmark_log_guard_required;
use dbbookmarks::transaction::bookmark_log_ids_by_name;
use fbinit::FacebookInit;
use futures::FutureExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use mononoke_macros::mononoke;
use mononoke_types_mocks::repo::REPO_ZERO;

const CONTROLS: [&str; 5] = [
    "scm/mononoke_bookmark_log_migration:bookmark_log_guard_enabled",
    "scm/mononoke_bookmark_log_migration:bookmark_log_ids_by_name",
    "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_guard_enabled",
    "scm/mononoke_bookmark_log_migration:mrls_bookmark_log_ids_by_name",
    "scm/mononoke_bookmark_log_migration:bookmark_log_guard_retired",
];

fn knobs(values: [bool; 5]) -> JustKnobsInMemory {
    JustKnobsInMemory::new(
        CONTROLS
            .into_iter()
            .zip(values)
            .map(|(name, value)| (name.to_owned(), KnobVal::Bool(value)))
            .collect(),
    )
}

#[mononoke::fbinit_test]
async fn producer_selection_and_coordinated_retirement_cover_all_controls(
    _fb: FacebookInit,
) -> Result<()> {
    for bits in 0..32 {
        let values = std::array::from_fn(|bit| bits & (1 << bit) != 0);
        let [
            single_enabled,
            single_by_name,
            multi_enabled,
            multi_by_name,
            retired,
        ] = values;
        with_just_knobs_async(
            knobs(values),
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
                    let required = bookmark_log_guard_required(REPO_ZERO, writer, by_name, enabled);
                    if by_name && !enabled {
                        assert!(activated.is_err(), "{writer:?}: {values:?}");
                        assert!(required.is_err(), "{writer:?}: {values:?}");
                    } else {
                        assert_eq!(activated?, enabled);
                        let both_name_modes =
                            single_enabled && single_by_name && multi_enabled && multi_by_name;
                        assert_eq!(
                            required?,
                            enabled && !(retired && both_name_modes),
                            "{writer:?}: {values:?}"
                        );
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

#[mononoke::fbinit_test]
async fn migration_controls_default_off(_fb: FacebookInit) -> Result<()> {
    for writer in [BookmarkLogWriter::SingleRepo, BookmarkLogWriter::MultiRepo] {
        assert!(!bookmark_log_guard_enabled(REPO_ZERO, writer));
        assert!(!bookmark_log_ids_by_name(REPO_ZERO, writer));
        assert!(!bookmark_log_guard_required(
            REPO_ZERO, writer, false, false
        )?);
    }
    assert!(!justknobs::eval(CONTROLS[4], Some("0"), Some("0")));
    Ok(())
}

#[mononoke::fbinit_test]
async fn retirement_reuses_callers_latched_controls(_fb: FacebookInit) -> Result<()> {
    for (writer, values) in [
        (
            BookmarkLogWriter::SingleRepo,
            [false, false, true, true, true],
        ),
        (
            BookmarkLogWriter::MultiRepo,
            [true, true, false, false, true],
        ),
    ] {
        with_just_knobs_async(
            knobs(values),
            async move {
                assert!(!bookmark_log_guard_required(REPO_ZERO, writer, true, true)?);
                assert!(bookmark_log_guard_required(REPO_ZERO, writer, false, true)?);
                assert!(bookmark_log_guard_required(REPO_ZERO, writer, true, false).is_err());
                Ok::<_, Error>(())
            }
            .boxed(),
        )
        .await?;
    }
    Ok(())
}
