/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::time::Duration;

use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs;
use maplit::hashmap;
use metaconfig_types::P4SourceConfig;
use metaconfig_types::RepoConfig;
use mononoke_macros::mononoke;

use super::*;
use crate::sharded::BatchSize;
use crate::sharded::ConfiguredDepot;
use crate::sharded::configured_depot;
use crate::sharded::import_enabled;
use crate::sharded::retry_delay;

#[mononoke::test]
fn test_only_a_repo_on_this_server_has_a_depot_to_import() {
    let config = RepoConfig {
        p4_source: Some(P4SourceConfig {
            server_alias: "test1".to_owned(),
            depot: "//project_test_stream/".to_owned(),
        }),
        ..Default::default()
    };
    assert_eq!(
        configured_depot(&config, Some("test1")),
        ConfiguredDepot::Depot("//project_test_stream/".to_owned())
    );
    assert_eq!(
        configured_depot(&config, Some("test2")),
        ConfiguredDepot::OtherServer {
            configured: "test1".to_owned()
        }
    );
    assert_eq!(configured_depot(&config, None), ConfiguredDepot::NoOwnAlias);
    assert_eq!(
        configured_depot(&RepoConfig::default(), Some("test1")),
        ConfiguredDepot::NotAMirror
    );
}

#[mononoke::test]
fn test_retry_delay_doubles_up_to_a_minute() {
    let delays: Vec<_> = (1..=6).map(retry_delay).collect();
    let secs = |s| Duration::from_secs(s);
    assert_eq!(
        delays,
        [secs(5), secs(10), secs(20), secs(40), secs(60), secs(60)]
    );
    assert_eq!(
        retry_delay(u32::MAX),
        secs(60),
        "no overflow on a long outage"
    );
}

#[mononoke::test]
fn test_idle_interval_must_be_at_least_a_second() {
    assert_eq!(parse_secs("30").unwrap(), Duration::from_secs(30));
    assert!(
        parse_secs("0").is_err(),
        "a zero wait would poll in a tight loop"
    );
}

#[mononoke::test]
fn test_import_follows_the_enabled_knob() {
    let knob = |on| {
        JustKnobsInMemory::new(hashmap! {
            "scm/mononoke:p4_sync_enabled".to_owned() => KnobVal::Bool(on),
        })
    };
    assert!(with_just_knobs(knob(true), || import_enabled("p4/test1")));
    assert!(!with_just_knobs(knob(false), || import_enabled("p4/test1")));
}

#[mononoke::test]
fn test_batch_size_halves_on_failure_and_doubles_back() {
    let mut size = BatchSize::new(NonZeroUsize::new(100).unwrap());
    let mut sizes = vec![size.get()];
    for _ in 0..8 {
        size.failed();
        sizes.push(size.get());
    }
    assert_eq!(sizes, [100, 50, 25, 12, 6, 3, 1, 1, 1]);

    let mut sizes = vec![];
    for _ in 0..8 {
        size.succeeded();
        sizes.push(size.get());
    }
    assert_eq!(sizes, [2, 4, 8, 16, 32, 64, 100, 100]);
}

#[mononoke::test]
fn test_stuck_after_failing_alone_three_times_in_a_row() {
    let mut size = BatchSize::new(NonZeroUsize::new(4).unwrap());
    // 4 -> 2 -> 1: halving, not yet alone.
    size.failed();
    size.failed();
    assert_eq!(size.get(), 1);
    assert!(!size.stuck());
    size.failed();
    size.failed();
    assert!(!size.stuck());
    size.failed();
    assert!(size.stuck());

    // A success clears it, and the count starts again.
    size.succeeded();
    assert!(!size.stuck());
    size.failed();
    size.failed();
    size.failed();
    assert!(!size.stuck());
}
