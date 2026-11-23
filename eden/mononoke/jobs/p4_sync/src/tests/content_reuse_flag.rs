/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

#[mononoke::test]
fn test_content_reuse_flag_sets_the_mode_and_is_off_unless_given() {
    assert_eq!(import_options(&[]).content_reuse, ContentReuse::Off);
    for (flag, mode) in [
        ("off", ContentReuse::Off),
        ("verify", ContentReuse::Verify),
        ("on", ContentReuse::On),
    ] {
        assert_eq!(
            import_options(&["--content-reuse", flag]).content_reuse,
            mode,
            "{flag}"
        );
    }
}

#[mononoke::test]
fn test_content_reuse_flag_rejects_other_values() {
    let argv = [
        "p4_sync",
        "--exit-file",
        "/exit",
        "--p4-binary",
        "/mononoke/p4",
        "--p4-user",
        "svc-scm-p4mirror",
        "--p4-server",
        "test1=ssl6:[2001:db8::1]:1818,AB:CD,/run/secrets/p4ticket-test1",
        "--content-reuse",
        "yes",
    ];
    assert!(P4SyncArgs::try_parse_from(argv).is_err());
}
