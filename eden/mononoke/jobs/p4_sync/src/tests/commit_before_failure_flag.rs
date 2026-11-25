/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

#[mononoke::test]
fn test_commit_before_failure_is_off_unless_passed() {
    assert!(!import_options(&[]).commit_before_failure);
    assert!(import_options(&["--commit-before-failure"]).commit_before_failure);
}
