/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

fn parse(extra: &[&'static str]) -> Result<P4SyncArgs, clap::Error> {
    P4SyncArgs::try_parse_from(
        [
            "p4_sync",
            "--exit-file",
            "/exit",
            "--p4-binary",
            "/mononoke/p4",
        ]
        .into_iter()
        .chain(extra.iter().copied()),
    )
}

#[mononoke::test]
fn test_filelog_flags_say_how_integration_records_are_asked_for() {
    let args = parse(&[
        "--filelog-files",
        "500",
        "--filelog-concurrency",
        "4",
        "--filelog-one-revision",
    ])
    .unwrap_or_else(|error| panic!("{error}"));

    assert_eq!(
        FilelogOptions {
            files_per_command: NonZeroUsize::new(500).unwrap(),
            commands_at_once: NonZeroUsize::new(4),
            history: FilelogHistory::OnlyRevision,
        },
        filelog_options(
            args.filelog_files,
            args.filelog_concurrency,
            args.filelog_one_revision
        )
    );
}

#[mononoke::test]
fn test_without_filelog_flags_integration_records_are_asked_for_as_before() {
    let args = parse(&[]).unwrap_or_else(|error| panic!("{error}"));

    assert_eq!(
        FilelogOptions::default(),
        filelog_options(
            args.filelog_files,
            args.filelog_concurrency,
            args.filelog_one_revision
        )
    );
}

#[mononoke::test]
fn test_filelog_flags_have_limits() {
    assert!(parse(&["--filelog-files", "2000"]).is_ok());
    assert!(parse(&["--filelog-files", "0"]).is_err());
    assert!(parse(&["--filelog-files", "2001"]).is_err());
    assert!(parse(&["--filelog-concurrency", "64"]).is_ok());
    assert!(parse(&["--filelog-concurrency", "0"]).is_err());
    assert!(parse(&["--filelog-concurrency", "65"]).is_err());
}
