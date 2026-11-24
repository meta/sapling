/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

fn flag(alias: &str, change: u64, depot_path: &str, revision: u32) -> UnreadableRevisionFlag {
    UnreadableRevisionFlag {
        alias: alias.to_owned(),
        revision: UnreadableRevision {
            change,
            depot_path: depot_path.to_owned(),
            revision,
        },
    }
}

#[mononoke::test]
fn test_unreadable_revision_flag_is_alias_change_and_depot_path_with_revision() {
    for (arg, expected) in [
        (
            "p4main:7881://depot/Software/OculusSDK/0.3/Main/LibOVR/Src/Kernel/OVR_Log.h#3",
            flag(
                "p4main",
                7881,
                "//depot/Software/OculusSDK/0.3/Main/LibOVR/Src/Kernel/OVR_Log.h",
                3,
            ),
        ),
        (
            "p4main:7881://depot/a:b/c%23d#e.h#12",
            flag("p4main", 7881, "//depot/a:b/c%23d#e.h", 12),
        ),
        (
            "p4htl:5://Depot/With Space/%40x.png#1",
            flag("p4htl", 5, "//Depot/With Space/%40x.png", 1),
        ),
    ] {
        assert_eq!(expected, parse_unreadable_revision(arg).unwrap(), "{arg}");
    }
}

#[mononoke::test]
fn test_unreadable_revision_flag_rejects_other_shapes() {
    for malformed in [
        "",
        "p4main",
        "p4main:7881",
        "p4main:7881://depot/a.h",
        ":7881://depot/a.h#3",
        "p4main::/depot/a.h#3",
        "p4main:x://depot/a.h#3",
        "p4main:-1://depot/a.h#3",
        "p4main:7881:depot/a.h#3",
        "p4main:7881://#3",
        "p4main:7881:#3",
        "p4main:7881://depot/a.h#",
        "p4main:7881://depot/a.h#head",
        "p4main:7881://depot/a.h#-3",
        "p4main:7881://depot/a.h#3 ",
        "p4main:0://depot/a.h#3",
        "p4main:7881://depot/a.h#0",
    ] {
        let error = parse_unreadable_revision(malformed).unwrap_err();
        assert!(
            error.to_string().starts_with("--unreadable-revision"),
            "{malformed:?}: {error}"
        );
    }
}

#[mononoke::test]
fn test_unreadable_revision_flag_repeats_and_is_checked_at_startup() {
    let argv = |extra: &[&'static str]| {
        [
            "p4_sync",
            "--exit-file",
            "/exit",
            "--p4-binary",
            "/mononoke/p4",
            "--p4-user",
            "svc-scm-p4mirror",
            "--p4-server",
            "test1=ssl6:[2001:db8::1]:1818,AB:CD,/run/secrets/p4ticket-test1",
        ]
        .into_iter()
        .chain(extra.iter().copied())
        .collect::<Vec<_>>()
    };

    let args = P4SyncArgs::try_parse_from(argv(&[
        "--unreadable-revision",
        "test1:7881://depot/a.h#3",
        "--unreadable-revision",
        "test1:7882://depot/b.h#1",
    ]))
    .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        vec![
            flag("test1", 7881, "//depot/a.h", 3),
            flag("test1", 7882, "//depot/b.h", 1),
        ],
        args.unreadable_revisions
    );
    assert!(
        P4SyncArgs::try_parse_from(argv(&["--unreadable-revision", "test1:7881://depot/a.h"]))
            .is_err()
    );
}

#[mononoke::test]
fn test_each_server_gets_only_its_own_unreadable_revisions() {
    let main_a = flag("p4main", 7881, "//depot/a.h", 3);
    let main_b = flag("p4main", 7881, "//depot/b.h", 3);
    let htl = flag("p4htl", 7881, "//depot/a.h", 3);

    let by_server = unreadable_by_server(vec![
        main_a.clone(),
        htl.clone(),
        main_b.clone(),
        main_a.clone(),
    ]);

    let revisions = |alias: &str| {
        let mut revisions: Vec<_> = by_server
            .get(alias)
            .map(|revisions| revisions.iter().cloned().collect())
            .unwrap_or_default();
        revisions.sort();
        revisions
    };
    assert_eq!(vec![main_a.revision, main_b.revision], revisions("p4main"));
    assert_eq!(vec![htl.revision], revisions("p4htl"));
    assert_eq!(Vec::<UnreadableRevision>::new(), revisions("test1"));
}

#[mononoke::test]
fn test_a_repo_gets_only_the_unreadable_revisions_of_its_depot() {
    let depot = flag("p4main", 7881, "//depot/a.h", 3).revision;
    let depot2 = flag("p4main", 7881, "//depot2/a.h", 3).revision;
    let of_server = Arc::new(HashSet::from([depot.clone(), depot2]));

    let in_depot = crate::sharded::unreadable_in_depot(Some(&of_server), "//depot/");

    assert_eq!(HashSet::from([depot]), *in_depot);
    assert!(crate::sharded::unreadable_in_depot(None, "//depot/").is_empty());
}
