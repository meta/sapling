/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

fn numbered(depot_path: &str, action: Action, rev: u32) -> FileRevision {
    FileRevision {
        revision: rev,
        ..revision(depot_path, action)
    }
}

fn unreadable(depot_paths: &[&str]) -> HashSet<String> {
    depot_paths.iter().map(|path| path.to_string()).collect()
}

#[mononoke::test]
fn test_unreadable_revisions_are_deletions_listed_in_their_extra() {
    let changelist = changelist(vec![
        numbered("//test1/z.txt", Action::Edit, 3),
        numbered("//test1/b.txt", Action::Branch, 1),
        numbered("//test1/c.txt", Action::Edit, 2),
        numbered("//other/d.txt", Action::Edit, 4),
    ]);
    let contents = contents(&["//test1/z.txt", "//test1/b.txt", "//test1/c.txt"]);
    let copy_sources = HashMap::from([("//test1/b.txt".to_owned(), path("c.txt"))]);

    let bcs = changeset_with_unreadable(
        DEPOT,
        SERVER_ID,
        &changelist,
        Some(ONES_CSID),
        &contents,
        &copy_sources,
        &unreadable(&["//test1/z.txt", "//test1/b.txt", "//other/d.txt"]),
    )
    .unwrap();

    assert_eq!(
        vec![
            ("b.txt".to_owned(), FileChange::Deletion),
            ("c.txt".to_owned(), tracked(FileType::Regular)),
            ("z.txt".to_owned(), FileChange::Deletion),
        ],
        changes_of(&bcs),
        "unreadable revisions lose their content and copy source, even when given them"
    );
    assert_eq!(
        Some(&b"1\tb.txt\n3\tz.txt\n"[..]),
        extra(&bcs, UNREADABLE_REVS_EXTRA),
        "sorted by repo path, without the other depot's file"
    );
    let revs = P4FileRevs::from_bcs(&bcs).unwrap();
    assert_eq!(Some(3), revs.revision(&path("z.txt")).unwrap());
    assert_eq!(Some(1), revs.revision(&path("b.txt")).unwrap());
}

#[mononoke::test]
fn test_without_unreadable_revisions_there_is_no_extra() {
    let changelist = changelist(vec![numbered("//test1/a.txt", Action::Edit, 2)]);
    let contents = contents(&["//test1/a.txt"]);
    let build = |unreadable_paths: &[&str]| {
        changeset_with_unreadable(
            DEPOT,
            SERVER_ID,
            &changelist,
            Some(ONES_CSID),
            &contents,
            &HashMap::new(),
            &unreadable(unreadable_paths),
        )
        .unwrap()
    };

    let none = build(&[]);
    let elsewhere = build(&["//other/a.txt", "//test1/b.txt"]);

    assert_eq!(None, extra(&none, UNREADABLE_REVS_EXTRA));
    assert_eq!(
        none.get_changeset_id(),
        elsewhere.get_changeset_id(),
        "paths that are not the changelist's files in the depot change nothing"
    );
    let plain = changeset(
        DEPOT,
        SERVER_ID,
        &changelist,
        Some(ONES_CSID),
        &contents,
        &HashMap::new(),
    )
    .unwrap();
    assert_eq!(plain.get_changeset_id(), none.get_changeset_id());
}

/// An unreadable file over a directory whose files Perforce lists as deleted: with no file
/// added, nothing shadows the deletions, and every path is deleted.
#[mononoke::test]
fn test_unreadable_file_over_a_deleted_directory_keeps_its_deletions() {
    let changelist = changelist(vec![
        numbered("//test1/dir", Action::Add, 1),
        numbered("//test1/dir/f.txt", Action::Delete, 2),
    ]);

    let bcs = changeset_with_unreadable(
        DEPOT,
        SERVER_ID,
        &changelist,
        Some(ONES_CSID),
        &HashMap::new(),
        &HashMap::new(),
        &unreadable(&["//test1/dir"]),
    )
    .unwrap();

    assert_eq!(
        vec![
            ("dir".to_owned(), FileChange::Deletion),
            ("dir/f.txt".to_owned(), FileChange::Deletion),
        ],
        changes_of(&bcs)
    );
    assert_eq!(Some(&b"1\tdir\n"[..]), extra(&bcs, UNREADABLE_REVS_EXTRA));
}

fn path(path: &str) -> NonRootMPath {
    NonRootMPath::new(path).unwrap()
}
