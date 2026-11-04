/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use mononoke_macros::mononoke;

use super::*;

#[mononoke::test]
fn test_repo_depot_splits_at_the_first_equals_sign() {
    assert_eq!(
        parse_repo_depot("p4/test1=//test_bjc_a_stream/").unwrap(),
        ("p4/test1".to_owned(), "//test_bjc_a_stream/".to_owned())
    );
}

#[mononoke::test]
fn test_repo_depot_needs_a_repo_and_a_depot_root() {
    for arg in [
        "p4/test1",
        "=//depot/",
        "p4/test1=depot/",
        "p4/test1=//depot",
        "p4/test1=",
    ] {
        assert!(parse_repo_depot(arg).is_err(), "{arg} should be rejected");
    }
}
