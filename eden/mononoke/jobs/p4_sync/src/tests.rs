/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::future::Future;
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
use p4_source::PathCharsets;
use p4_source::PathCharsetsError;
use stats_facebook::service_data::ServiceData;
use stats_facebook::service_data::get_service_data_singleton;
use tokio::time::Instant;

use super::*;
use crate::sharded::BatchSize;
use crate::sharded::ConfiguredDepot;
use crate::sharded::LastSuccess;
use crate::sharded::configured_depot;
use crate::sharded::import_enabled;
use crate::sharded::report_freshness;
use crate::sharded::retry_delay;

#[mononoke::test]
fn test_a_repo_gets_the_entry_of_the_server_its_config_names() {
    let config = |alias: &str| RepoConfig {
        p4_source: Some(P4SourceConfig {
            server_alias: alias.to_owned(),
            depot: "//project_test_stream/".to_owned(),
            path_charsets: None,
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
            path_charsets: PathCharsets::default(),
            p4: &"test1-port",
        }
    );
    assert_eq!(
        configured_depot(&config("p4htl"), &servers),
        ConfiguredDepot::Depot {
            alias: "p4htl",
            depot: "//project_test_stream/".to_owned(),
            path_charsets: PathCharsets::default(),
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

#[mononoke::test]
fn test_a_repo_gets_the_path_charsets_its_config_lists() {
    let config = |labels: &[&str]| RepoConfig {
        p4_source: Some(P4SourceConfig {
            server_alias: "p4htl".to_owned(),
            depot: "//AR-G/".to_owned(),
            path_charsets: Some(labels.iter().map(|label| label.to_string()).collect()),
        }),
        ..Default::default()
    };
    let servers = hashmap! { "p4htl".to_owned() => "p4htl-port" };

    assert_eq!(
        configured_depot(&config(&["utf-8", "gbk", "cp1252"]), &servers),
        ConfiguredDepot::Depot {
            alias: "p4htl",
            depot: "//AR-G/".to_owned(),
            path_charsets: PathCharsets::new(&["utf-8", "gbk", "cp1252"]).unwrap(),
            p4: &"p4htl-port",
        }
    );
    assert_eq!(
        configured_depot(&config(&["utf-8", "klingon"]), &servers),
        ConfiguredDepot::InvalidCharsets(PathCharsetsError::Unknown("klingon".to_owned()))
    );
    assert_eq!(
        configured_depot(&config(&[]), &servers),
        ConfiguredDepot::InvalidCharsets(PathCharsetsError::Empty)
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

fn reported(fb: FacebookInit, repo: &str) -> Option<i64> {
    get_service_data_singleton(fb)
        .get_counter(&format!("mononoke.p4_sync.secs_since_success.{repo}"))
}

/// Runs `work` beside the reporter and reads the counter `after` in, while both still run.
async fn reported_after(
    fb: FacebookInit,
    repo: &str,
    last_success: &LastSuccess,
    work: impl Future<Output = ()>,
    after: Duration,
) -> Option<i64> {
    let read = async {
        tokio::time::sleep(after).await;
        reported(fb, repo)
    };
    tokio::select! {
        _ = report_freshness(fb, repo, last_success, work) => panic!("work ended early"),
        value = read => value,
    }
}

#[mononoke::fbinit_test]
async fn test_a_wedged_batch_shows_as_a_growing_value_until_the_shard_stops(fb: FacebookInit) {
    tokio::time::pause();
    let last_success = LastSuccess::new(Instant::now());
    let wedged = std::future::pending::<()>();
    assert_eq!(
        reported_after(
            fb,
            "wedged",
            &last_success,
            wedged,
            Duration::from_secs(1801)
        )
        .await,
        Some(1800)
    );
    assert_eq!(
        reported(fb, "wedged"),
        None,
        "a stopped shard must not leave a value behind"
    );
}

#[mononoke::fbinit_test]
async fn test_a_held_shard_grows_from_its_start(fb: FacebookInit) {
    tokio::time::pause();
    let last_success = LastSuccess::new(Instant::now());
    // A pass every 30 s, none of them a success.
    let held = async {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    };
    assert_eq!(
        reported_after(fb, "held", &last_success, held, Duration::from_secs(3601)).await,
        Some(3600)
    );
}

#[mononoke::fbinit_test]
async fn test_a_success_or_a_switched_off_pass_resets_it(fb: FacebookInit) {
    tokio::time::pause();
    let last_success = LastSuccess::new(Instant::now());
    // Failing for 30 min, then one success.
    let recovers = async {
        tokio::time::sleep(Duration::from_secs(1800)).await;
        last_success.record(Instant::now());
        std::future::pending::<()>().await
    };
    assert_eq!(
        reported_after(
            fb,
            "recovers",
            &last_success,
            recovers,
            Duration::from_secs(1831)
        )
        .await,
        Some(30)
    );

    let last_success = LastSuccess::new(Instant::now());
    // Switched off by JK: each idle pass counts as a success.
    let switched_off = async {
        loop {
            last_success.record(Instant::now());
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    };
    let value = reported_after(
        fb,
        "switched_off",
        &last_success,
        switched_off,
        Duration::from_secs(3601),
    )
    .await;
    assert!(
        matches!(value, Some(0..=30)),
        "never more than one idle pass old, got {value:?}"
    );
}
