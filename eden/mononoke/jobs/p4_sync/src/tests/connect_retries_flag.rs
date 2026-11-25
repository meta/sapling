/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

fn connect_retries(flags: &[&'static str]) -> Result<u32, clap::Error> {
    let argv = [
        "p4_sync",
        "--exit-file",
        "/exit",
        "--p4-binary",
        "/mononoke/p4",
    ];
    P4SyncArgs::try_parse_from(argv.iter().chain(flags)).map(|args| args.p4_connect_retries)
}

#[mononoke::test]
fn test_connect_retries_are_on_unless_turned_off() {
    assert_eq!(connect_retries(&[]).unwrap(), CONNECT_RETRIES);
    assert_eq!(connect_retries(&["--p4-connect-retries", "0"]).unwrap(), 0);
    assert!(connect_retries(&["--p4-connect-retries", "6"]).is_err());
}
