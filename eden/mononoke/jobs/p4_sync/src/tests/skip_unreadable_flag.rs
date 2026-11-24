/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

#[mononoke::test]
fn test_skip_unreadable_revisions_names_servers_and_takes_the_retry_settings() {
    let args = P4SyncArgs::try_parse_from([
        "p4_sync",
        "--exit-file",
        "/exit",
        "--p4-binary",
        "/mononoke/p4",
        "--skip-unreadable-revisions",
        "p4main",
        "--skip-unreadable-revisions",
        "test1",
        "--unreadable-retries",
        "3",
        "--unreadable-retry-delay-ms",
        "2000",
    ])
    .unwrap_or_else(|error| panic!("{error}"));

    assert_eq!(vec!["p4main", "test1"], args.skip_unreadable_servers);
    assert_eq!(
        (3, 2000),
        (args.unreadable_retries, args.unreadable_retry_delay_ms)
    );
}

#[mononoke::test]
fn test_a_revision_is_never_skipped_without_enough_retries() {
    let parse = |retries: &'static str, delay: &'static str| {
        P4SyncArgs::try_parse_from([
            "p4_sync",
            "--exit-file",
            "/exit",
            "--p4-binary",
            "/mononoke/p4",
            "--skip-unreadable-revisions",
            "p4main",
            "--unreadable-retries",
            retries,
            "--unreadable-retry-delay-ms",
            delay,
        ])
    };

    assert!(parse("3", "1000").is_ok());
    assert!(parse("0", "4000").is_err(), "no retry at all");
    assert!(parse("2", "4000").is_err());
    assert!(parse("11", "4000").is_err());
    assert!(parse("5", "999").is_err(), "retries with hardly a wait");
    assert!(parse("5", "60001").is_err());
}

#[mononoke::test]
fn test_an_alias_no_server_has_is_reported() {
    let flags = HashMap::from([("test1".to_owned(), ())]);
    let tier = BTreeMap::from([("p4main".to_owned(), ())]);
    let skip = ["p4main", "test1", "p4mian"].map(str::to_owned);

    assert_eq!(
        vec!["p4mian"],
        aliases_without_a_server(&skip, &flags, &tier)
    );
}
