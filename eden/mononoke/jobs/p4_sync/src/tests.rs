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
use p4_source::P4Auth;
use p4_source::P4Trust;

use super::*;
use crate::sharded::BatchSize;
use crate::sharded::ConfiguredDepot;
use crate::sharded::configured_depot;
use crate::sharded::import_enabled;
use crate::sharded::retry_delay;

#[mononoke::test]
fn test_a_repo_gets_the_entry_of_the_server_its_config_names() {
    let config = |alias: &str| RepoConfig {
        p4_source: Some(P4SourceConfig {
            server_alias: alias.to_owned(),
            depot: "//project_test_stream/".to_owned(),
        }),
        ..Default::default()
    };
    let servers = hashmap! {
        "test1".to_owned() => "test1-port",
        "p4htl".to_owned() => "p4htl-port",
    };
    assert_eq!(
        configured_depot(&config("test1"), &servers),
        ConfiguredDepot::Depot {
            alias: "test1",
            depot: "//project_test_stream/".to_owned(),
            p4: &"test1-port",
        }
    );
    assert_eq!(
        configured_depot(&config("p4htl"), &servers),
        ConfiguredDepot::Depot {
            alias: "p4htl",
            depot: "//project_test_stream/".to_owned(),
            p4: &"p4htl-port",
        }
    );

    let unknown = ConfiguredDepot::UnknownServer {
        configured: "test1".to_owned(),
    };
    let other_server = hashmap! { "p4htl".to_owned() => "p4htl-port" };
    assert_eq!(configured_depot(&config("test1"), &other_server), unknown);
    assert_eq!(
        configured_depot(&config("test1"), &HashMap::<String, &str>::new()),
        unknown
    );
    assert_eq!(
        configured_depot(&RepoConfig::default(), &servers),
        ConfiguredDepot::NotAMirror
    );
}

fn server(alias: &str) -> P4Server {
    P4Server {
        alias: alias.to_owned(),
        port: "ssl6:[2001:db8::1]:1818".to_owned(),
        fingerprint: "AB:CD".to_owned(),
        ticket_file: PathBuf::from(format!("/run/secrets/p4ticket-{alias}")),
    }
}

#[mononoke::test]
fn test_p4_server_flag_is_alias_port_fingerprint_and_ticket_file() {
    assert_eq!(
        parse_server("test1=ssl6:[2001:db8::1]:1818,AB:CD,/run/secrets/p4ticket-test1").unwrap(),
        server("test1")
    );
    for malformed in [
        "test1",
        "test1=ssl6:host:1818",
        "test1=ssl6:host:1818,AB:CD",
        "=ssl6:host:1818,AB:CD,/ticket",
        "test1=,AB:CD,/ticket",
        "test1=ssl6:host:1818,,/ticket",
        "test1=ssl6:host:1818,AB:CD,",
    ] {
        assert!(
            parse_server(malformed).is_err(),
            "{malformed:?} should be rejected"
        );
    }
}

#[mononoke::test]
fn test_each_server_is_given_once() {
    let servers = by_alias(vec![server("test1"), server("p4htl")]).unwrap();
    assert_eq!(servers.len(), 2);
    assert_eq!(servers["p4htl"], server("p4htl"));

    let error = by_alias(vec![server("test1"), server("p4htl"), server("test1")]).unwrap_err();
    assert!(error.to_string().contains("test1"), "{error}");
}

#[mononoke::test]
fn test_every_ticket_file_must_exist_at_startup() {
    let secrets = tempfile::tempdir().unwrap();
    let with_ticket = |alias: &str| P4Server {
        ticket_file: secrets.path().join(format!("p4ticket-{alias}")),
        ..server(alias)
    };
    std::fs::write(with_ticket("test1").ticket_file, "ticket").unwrap();
    let scuba = MononokeScubaSampleBuilder::with_discard();
    let binary = Path::new("/mononoke/p4");

    let clients = p4_clients(vec![with_ticket("test1")], binary, "user", &scuba).unwrap();
    assert_eq!(clients.len(), 1);
    assert!(clients.contains_key("test1"));

    let error = p4_clients(
        vec![with_ticket("test1"), with_ticket("p4htl")],
        binary,
        "user",
        &scuba,
    )
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("--p4-server p4htl"), "{message}");
    assert!(message.contains("p4ticket-p4htl"), "{message}");
    assert!(message.contains("does not exist"), "{message}");
}

#[mononoke::test]
fn test_a_server_gets_p4imports_flags_with_its_own_port_fingerprint_and_ticket() {
    let config = server("p4htl")
        .p4_args(Path::new("/mononoke/p4"), "svc-scm-p4mirror")
        .config()
        .unwrap();
    assert_eq!(config.binary, PathBuf::from("/mononoke/p4"));
    assert_eq!(config.port, "ssl6:[2001:db8::1]:1818");
    assert_eq!(config.user, "svc-scm-p4mirror");
    assert_eq!(config.trust, P4Trust::Fingerprint("AB:CD".to_owned()));
    assert!(
        matches!(config.auth, P4Auth::Ticket(_)),
        "expected a ticket, got {:?}",
        config.auth
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
