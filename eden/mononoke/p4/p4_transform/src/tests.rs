/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeSet;

use mononoke_macros::mononoke;
use mononoke_types::P4FileRevs;
use mononoke_types::hash::Blake3;
use mononoke_types::hash::RichGitSha1;
use mononoke_types::hash::Sha1;
use mononoke_types::hash::Sha256;
use mononoke_types_mocks::changesetid::ONES_CSID;
use mononoke_types_mocks::changesetid::TWOS_CSID;
use mononoke_types_mocks::contentid::ONES_CTID;

use super::*;

const DEPOT: &str = "//test1/";
const SERVER_ID: &str = "commit-standby";

/// Only `content_id` and `total_size` make it into a file change.
fn content() -> ContentMetadataV2 {
    ContentMetadataV2 {
        content_id: ONES_CTID,
        total_size: 3,
        sha1: Sha1::from_byte_array([0; 20]),
        sha256: Sha256::from_byte_array([0; 32]),
        git_sha1: RichGitSha1::from_byte_array([0; 20], "blob", 3),
        is_binary: false,
        is_ascii: true,
        is_utf8: true,
        ends_in_newline: false,
        newline_count: 0,
        first_line: None,
        is_generated: false,
        is_partially_generated: false,
        seeded_blake3: Blake3::from_byte_array([0; 32]),
    }
}

fn revision(depot_path: &str, action: Action) -> FileRevision {
    typed_revision(depot_path, action, "text")
}

fn typed_revision(depot_path: &str, action: Action, file_type: &str) -> FileRevision {
    FileRevision {
        depot_path: depot_path.to_owned(),
        action,
        file_type: file_type.to_owned(),
        revision: 1,
        size: None,
        digest: None,
    }
}

fn change_of(
    depot_path: &str,
    action: Action,
    content: Option<&ContentMetadataV2>,
) -> Result<Option<(NonRootMPath, FileChange)>, TransformError> {
    file_change(DEPOT, &revision(depot_path, action), content, None)
}

fn path_of(depot_path: &str) -> String {
    let (path, _) = change_of(depot_path, Action::Delete, None)
        .unwrap()
        .unwrap();
    path.to_string()
}

fn tracked(file_type: FileType) -> FileChange {
    FileChange::tracked(ONES_CTID, file_type, 3, None, GitLfs::FullContent)
}

#[mononoke::test]
fn test_content_actions_are_changes_with_the_stored_content() {
    for action in [
        Action::Add,
        Action::Edit,
        Action::Import,
        Action::Integrate,
        Action::Branch,
        Action::MoveAdd,
    ] {
        let (path, change) = change_of("//test1/a.txt", action, Some(&content()))
            .unwrap()
            .unwrap();
        assert_eq!("a.txt", path.to_string(), "{action:?}");
        assert_eq!(tracked(FileType::Regular), change, "{action:?}");
    }
}

#[mononoke::test]
fn test_actions_without_content_are_deletions() {
    for action in [
        Action::Delete,
        Action::MoveDelete,
        Action::Purge,
        Action::Archive,
    ] {
        let (_, change) = change_of("//test1/a.txt", action, None).unwrap().unwrap();
        assert_eq!(FileChange::Deletion, change, "{action:?}");
    }
}

#[mononoke::test]
fn test_content_action_without_content_is_an_error() {
    let result = change_of("//test1/a.txt", Action::Edit, None);
    assert!(
        matches!(&result, Err(TransformError::MissingContent(p)) if p == "//test1/a.txt"),
        "{result:?}"
    );
}

#[mononoke::test]
fn test_rule_paths_are_case_preserving() {
    assert_eq!("Dir/README.md", path_of("//test1/Dir/README.md"));
    assert_eq!("dir/readme.md", path_of("//test1/dir/readme.md"));
}

#[mononoke::test]
fn test_files_in_other_depots_have_no_change() {
    for depot_path in ["//test10/a.txt", "//other/a.txt", "//TEST1/a.txt"] {
        let result = change_of(depot_path, Action::Delete, None);
        assert!(matches!(result, Ok(None)), "{depot_path}: {result:?}");
    }
}

#[mononoke::test]
fn test_depot_root_that_is_not_one_depot_is_an_error() {
    for depot_root in ["//test1", "//", "///", "//a/b/", "test1/"] {
        let result = file_change(
            depot_root,
            &revision("//test1/a.txt", Action::Delete),
            None,
            None,
        );
        assert!(
            matches!(&result, Err(TransformError::InvalidDepotRoot(d)) if d == depot_root),
            "{result:?}"
        );
    }
}

#[mononoke::test]
fn test_path_mononoke_rejects_is_an_error() {
    for depot_path in ["//test1/", "//test1/a/../b"] {
        let result = change_of(depot_path, Action::Delete, None);
        assert!(
            matches!(&result, Err(TransformError::InvalidPath { depot_path: p, .. }) if p == depot_path),
            "{result:?}"
        );
    }
}

#[mononoke::test]
fn test_rule_paths_are_unescaped() {
    for (depot_path, path) in [
        ("//test1/foo%402x.png", "foo@2x.png"),
        ("//test1/issue%2312.txt", "issue#12.txt"),
        ("//test1/100%25.txt", "100%.txt"),
        ("//test1/star%2A.txt", "star*.txt"),
        ("//test1/literal%2540.txt", "literal%40.txt"),
    ] {
        assert_eq!(path, path_of(depot_path), "{depot_path}");
    }
}

#[mononoke::test]
fn test_unescape_leaves_other_percent_sequences() {
    assert_eq!("a%2Fb%2a%", unescape_path("a%2Fb%2a%"));
}

#[mononoke::test]
fn test_escape_and_unescape_round_trip() {
    for (path, depot_path) in [
        ("plain/name.txt", "plain/name.txt"),
        ("foo@2x.png", "foo%402x.png"),
        ("issue#12.txt", "issue%2312.txt"),
        ("100%.txt", "100%25.txt"),
        ("star*.txt", "star%2A.txt"),
        ("literal%40.txt", "literal%2540.txt"),
        ("@#%*", "%40%23%25%2A"),
    ] {
        assert_eq!(depot_path, escape_path(path), "{path}");
        assert_eq!(path, unescape_path(depot_path), "{depot_path}");
    }
}

#[mononoke::test]
fn test_file_type_reaches_the_file_change() {
    for (p4_type, file_type) in [
        ("text+x", FileType::Executable),
        ("symlink", FileType::Symlink),
    ] {
        let revision = typed_revision("//test1/a", Action::Add, p4_type);
        let (_, change) = file_change(DEPOT, &revision, Some(&content()), None)
            .unwrap()
            .unwrap();
        assert_eq!(tracked(file_type), change, "{p4_type}");
    }
}

#[mononoke::test]
fn test_file_type_from_base_type_modifiers_and_legacy_names() {
    for (p4_type, expected) in [
        ("text", FileType::Regular),
        ("binary+lF", FileType::Regular),
        ("text+k", FileType::Regular),
        ("binary+S10", FileType::Regular),
        ("binary+X", FileType::Regular),
        ("unicode", FileType::Regular),
        ("ktext", FileType::Regular),
        ("ubinary", FileType::Regular),
        ("tempobj", FileType::Regular),
        ("apple", FileType::Regular),
        ("text+x", FileType::Executable),
        ("text+kx", FileType::Executable),
        ("binary+lx", FileType::Executable),
        ("utf16+xw", FileType::Executable),
        ("xtext", FileType::Executable),
        ("xbinary", FileType::Executable),
        ("kxtext", FileType::Executable),
        ("cxtext", FileType::Executable),
        ("xltext", FileType::Executable),
        ("uxbinary", FileType::Executable),
        ("xtempobj", FileType::Executable),
        ("xunicode", FileType::Executable),
        ("xutf8", FileType::Executable),
        ("xutf16", FileType::Executable),
        ("symlink", FileType::Symlink),
        ("symlink+x", FileType::Symlink),
    ] {
        assert_eq!(expected, file_type(p4_type), "{p4_type}");
    }
}

fn integration(how: IntegrationHow, file: &str) -> Integration {
    Integration {
        how,
        file: file.to_owned(),
        start_rev: None,
        end_rev: 1,
    }
}

fn source_of(action: Action, integrations: &[Integration]) -> CopySource {
    copy_source(DEPOT, &revision("//test1/new.txt", action), integrations).unwrap()
}

fn copied_from(path: &str) -> CopySource {
    CopySource::From(NonRootMPath::new(path).unwrap())
}

#[mononoke::test]
fn test_rule_copy_from_for_branch_and_move_add() {
    for (action, how) in [
        (Action::Branch, IntegrationHow::BranchFrom),
        (Action::Branch, IntegrationHow::CopyFrom),
        (Action::MoveAdd, IntegrationHow::MovedFrom),
    ] {
        let source = source_of(action, &[integration(how.clone(), "//test1/dir/old.txt")]);
        assert_eq!(copied_from("dir/old.txt"), source, "{action:?} {how:?}");
    }
}

#[mononoke::test]
fn test_other_actions_are_not_copies() {
    for action in [
        Action::Add,
        Action::Edit,
        Action::Integrate,
        Action::Import,
        Action::Delete,
        Action::MoveDelete,
        Action::Purge,
        Action::Archive,
    ] {
        for how in [IntegrationHow::BranchFrom, IntegrationHow::CopyFrom] {
            let source = source_of(action, &[integration(how, "//test1/old.txt")]);
            assert_eq!(CopySource::NotACopy, source, "{action:?}");
        }
    }
}

#[mononoke::test]
fn test_records_that_do_not_start_a_file_are_not_sources() {
    for how in [
        IntegrationHow::MergeFrom,
        IntegrationHow::EditFrom,
        IntegrationHow::IntegrateFrom,
        IntegrationHow::Ignored,
        IntegrationHow::DeleteFrom,
        IntegrationHow::AddFrom,
        IntegrationHow::Undid,
        IntegrationHow::BranchInto,
        IntegrationHow::Other("future from".to_owned()),
    ] {
        let source = source_of(Action::Branch, &[integration(how, "//test1/old.txt")]);
        assert_eq!(CopySource::NotACopy, source);
    }
}

#[mononoke::test]
fn test_rule_copy_from_needs_a_single_source() {
    let source = source_of(
        Action::Branch,
        &[
            integration(IntegrationHow::BranchFrom, "//test1/a.txt"),
            integration(IntegrationHow::BranchFrom, "//test1/b.txt"),
        ],
    );
    assert_eq!(CopySource::Ambiguous, source);
}

#[mononoke::test]
fn test_rule_add_after_integrate_is_not_a_copy() {
    let source = source_of(
        Action::Add,
        &[integration(IntegrationHow::AddFrom, "//test1/old.txt")],
    );
    assert_eq!(CopySource::NotACopy, source);
}

#[mononoke::test]
fn test_no_source_is_not_a_copy() {
    assert_eq!(CopySource::NotACopy, source_of(Action::Branch, &[]));
}

#[mononoke::test]
fn test_one_source_among_other_records_is_a_copy() {
    let source = source_of(
        Action::Branch,
        &[
            integration(IntegrationHow::BranchFrom, "//test1/old.txt"),
            integration(IntegrationHow::BranchInto, "//test1/later.txt"),
        ],
    );
    assert_eq!(copied_from("old.txt"), source);
}

#[mononoke::test]
fn test_rule_copy_from_same_depot_only() {
    for file in ["//other/old.txt", "//test10/old.txt"] {
        let source = source_of(
            Action::Branch,
            &[integration(IntegrationHow::BranchFrom, file)],
        );
        assert_eq!(CopySource::OtherDepot, source, "{file}");
    }
}

#[mononoke::test]
fn test_source_paths_are_unescaped() {
    let source = source_of(
        Action::MoveAdd,
        &[integration(
            IntegrationHow::MovedFrom,
            "//test1/foo%402x.png",
        )],
    );
    assert_eq!(copied_from("foo@2x.png"), source);
}

#[mononoke::test]
fn test_copy_from_reaches_the_file_change() {
    let from = NonRootMPath::new("old.txt").unwrap();
    let (_, change) = file_change(
        DEPOT,
        &revision("//test1/new.txt", Action::Branch),
        Some(&content()),
        Some((from.clone(), ONES_CSID)),
    )
    .unwrap()
    .unwrap();
    let expected = FileChange::tracked(
        ONES_CTID,
        FileType::Regular,
        3,
        Some((from, ONES_CSID)),
        GitLfs::FullContent,
    );
    assert_eq!(expected, change);
}

fn changelist(files: Vec<FileRevision>) -> Changelist {
    Changelist {
        number: 42,
        user: "jdoe".to_owned(),
        time: 1_700_000_000,
        description: "Fix the build\n".to_owned(),
        description_bytes: None,
        files,
    }
}

fn contents(depot_paths: &[&str]) -> HashMap<String, ContentMetadataV2> {
    depot_paths
        .iter()
        .map(|depot_path| (depot_path.to_string(), content()))
        .collect()
}

fn changes_of(bcs: &BonsaiChangeset) -> Vec<(String, FileChange)> {
    bcs.file_changes()
        .map(|(path, change)| (path.to_string(), change.clone()))
        .collect()
}

#[mononoke::test]
fn test_changelist_becomes_a_changeset_on_its_parent() {
    let changelist = changelist(vec![
        revision("//test1/a.txt", Action::Edit),
        revision("//test1/b.txt", Action::Delete),
        revision("//other/c.txt", Action::Edit),
    ]);
    let contents = contents(&["//test1/a.txt"]);
    let bcs = changeset(
        DEPOT,
        SERVER_ID,
        &changelist,
        Some(ONES_CSID),
        &contents,
        &HashMap::new(),
    )
    .unwrap();
    assert_eq!(vec![ONES_CSID], bcs.parents().collect::<Vec<_>>());
    assert_eq!("jdoe", bcs.author());
    assert_eq!(
        &DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        bcs.author_date()
    );
    assert_eq!(None, bcs.committer());
    assert_eq!("Fix the build\n", bcs.message());
    assert_eq!(42, P4ChangelistId::from_bcs(&bcs).unwrap().id());
    assert_eq!(
        vec![
            ("a.txt".to_owned(), tracked(FileType::Regular)),
            ("b.txt".to_owned(), FileChange::Deletion),
        ],
        changes_of(&bcs)
    );
}

#[mononoke::test]
fn test_first_changeset_has_no_parent_and_no_copies() {
    let changelist = changelist(vec![revision("//test1/new.txt", Action::Branch)]);
    let contents = contents(&["//test1/new.txt"]);
    let copy_sources = HashMap::from([(
        "//test1/new.txt".to_owned(),
        NonRootMPath::new("old.txt").unwrap(),
    )]);
    let bcs = changeset(
        DEPOT,
        SERVER_ID,
        &changelist,
        None,
        &contents,
        &copy_sources,
    )
    .unwrap();
    assert_eq!(0, bcs.parents().count());
    assert_eq!(
        vec![("new.txt".to_owned(), tracked(FileType::Regular))],
        changes_of(&bcs)
    );
}

#[mononoke::test]
fn test_copies_are_from_the_parent() {
    let changelist = changelist(vec![revision("//test1/new.txt", Action::MoveAdd)]);
    let contents = contents(&["//test1/new.txt"]);
    let from = NonRootMPath::new("old.txt").unwrap();
    let copy_sources = HashMap::from([("//test1/new.txt".to_owned(), from.clone())]);
    let bcs = changeset(
        DEPOT,
        SERVER_ID,
        &changelist,
        Some(TWOS_CSID),
        &contents,
        &copy_sources,
    )
    .unwrap();
    let (_, change) = bcs.file_changes().next().unwrap();
    assert_eq!(Some(&(from, TWOS_CSID)), change.copy_from());
}

#[mononoke::test]
fn test_rule_shadowed_child_deletions_are_dropped() {
    let changelist = changelist(vec![
        revision("//test1/a", Action::Add),
        revision("//test1/a/b", Action::Delete),
        revision("//test1/a/c/d", Action::MoveDelete),
        revision("//test1/ab", Action::Delete),
    ]);
    let contents = contents(&["//test1/a"]);
    let bcs = changeset(
        DEPOT,
        SERVER_ID,
        &changelist,
        Some(ONES_CSID),
        &contents,
        &HashMap::new(),
    )
    .unwrap();
    assert_eq!(
        vec![
            ("a".to_owned(), tracked(FileType::Regular)),
            ("ab".to_owned(), FileChange::Deletion),
        ],
        changes_of(&bcs)
    );
}

#[mononoke::test]
fn test_directory_replacing_a_deleted_file_keeps_the_deletion() {
    let changelist = changelist(vec![
        revision("//test1/a", Action::Delete),
        revision("//test1/a/b", Action::Add),
    ]);
    let contents = contents(&["//test1/a/b"]);
    let bcs = changeset(
        DEPOT,
        SERVER_ID,
        &changelist,
        Some(ONES_CSID),
        &contents,
        &HashMap::new(),
    )
    .unwrap();
    assert_eq!(
        vec![
            ("a".to_owned(), FileChange::Deletion),
            ("a/b".to_owned(), tracked(FileType::Regular)),
        ],
        changes_of(&bcs)
    );
}

#[mononoke::test]
fn test_file_and_a_file_under_it_both_changed_is_an_error() {
    let changelist = changelist(vec![
        revision("//test1/a", Action::Edit),
        revision("//test1/a/b", Action::Add),
    ]);
    let contents = contents(&["//test1/a", "//test1/a/b"]);
    let result = changeset(
        DEPOT,
        SERVER_ID,
        &changelist,
        Some(ONES_CSID),
        &contents,
        &HashMap::new(),
    );
    assert!(
        matches!(
            &result,
            Err(TransformError::InvalidChangeset { changelist: 42, .. })
        ),
        "{result:?}"
    );
}

#[mononoke::test]
fn test_user_with_a_newline_is_an_error() {
    let mut changelist = changelist(vec![revision("//test1/a.txt", Action::Delete)]);
    changelist.user = "jdoe\nFake: header".to_owned();
    let result = changeset(
        DEPOT,
        SERVER_ID,
        &changelist,
        None,
        &HashMap::new(),
        &HashMap::new(),
    );
    assert!(
        matches!(
            &result,
            Err(TransformError::NewlineInAuthor { changelist: 42 })
        ),
        "{result:?}"
    );
}

fn extra<'a>(bcs: &'a BonsaiChangeset, key: &str) -> Option<&'a [u8]> {
    bcs.hg_extra()
        .find_map(|(extra_key, value)| (extra_key == key).then_some(value))
}

#[mononoke::test]
fn test_rule_p4_file_revs_format() {
    let numbered = |depot_path, action, rev| FileRevision {
        revision: rev,
        ..revision(depot_path, action)
    };
    let changelist = changelist(vec![
        numbered("//test1/z.txt", Action::Edit, 3),
        numbered("//test1/b.txt", Action::Delete, 2),
        numbered("//test1/img/foo%402x.png", Action::Add, 1),
        numbered("//other/c.txt", Action::Edit, 7),
    ]);
    let contents = contents(&["//test1/z.txt", "//test1/img/foo%402x.png"]);
    let bcs = changeset(
        DEPOT,
        SERVER_ID,
        &changelist,
        None,
        &contents,
        &HashMap::new(),
    )
    .unwrap();
    assert_eq!(
        Some(&b"2\tb.txt\n1\timg/foo@2x.png\n3\tz.txt\n"[..]),
        extra(&bcs, FILE_REVS_EXTRA)
    );
    assert_eq!(42, P4ChangelistId::from_bcs(&bcs).unwrap().id());
    let revs = P4FileRevs::from_bcs(&bcs).unwrap();
    let path = |path| NonRootMPath::new(path).unwrap();
    assert_eq!(Some(2), revs.revision(&path("b.txt")).unwrap());
    assert_eq!(Some(1), revs.revision(&path("img/foo@2x.png")).unwrap());
    assert_eq!(Some(3), revs.revision(&path("z.txt")).unwrap());
    assert_eq!(
        None,
        revs.revision(&path("c.txt")).unwrap(),
        "other depot's file"
    );
    assert_eq!(3, revs.iter().count());
}

#[mononoke::test]
fn test_every_changeset_records_its_server() {
    let changelist = changelist(vec![]);
    let bcs = changeset(
        DEPOT,
        SERVER_ID,
        &changelist,
        None,
        &HashMap::new(),
        &HashMap::new(),
    )
    .unwrap();
    assert_eq!(Some(SERVER_ID.as_bytes()), extra(&bcs, SERVER_ID_EXTRA));
    assert_eq!(Some(SERVER_ID), recorded_server_id(&bcs));
}

#[mononoke::test]
fn test_file_revs_list_deletions_a_file_shadows() {
    let numbered = |depot_path, action, rev| FileRevision {
        revision: rev,
        ..revision(depot_path, action)
    };
    let changelist = changelist(vec![
        numbered("//test1/ab", Action::Delete, 2),
        numbered("//test1/a/b", Action::Delete, 3),
        numbered("//test1/a", Action::Add, 1),
    ]);
    let contents = contents(&["//test1/a"]);
    let bcs = changeset(
        DEPOT,
        SERVER_ID,
        &changelist,
        Some(ONES_CSID),
        &contents,
        &HashMap::new(),
    )
    .unwrap();
    assert_eq!(
        vec!["a".to_owned(), "ab".to_owned()],
        changes_of(&bcs)
            .into_iter()
            .map(|(path, _)| path)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        Some(&b"1\ta\n3\ta/b\n2\tab\n"[..]),
        extra(&bcs, FILE_REVS_EXTRA)
    );
}

#[mononoke::test]
fn test_file_revs_key_is_not_a_changelist_id_key() {
    for (key, _) in P4ChangelistId::new(42).to_hg_extras() {
        assert!(!key.starts_with("p4-"), "{key}");
    }
    let keys = BTreeSet::from([FILE_REVS_EXTRA, DESCRIPTION_BYTES_EXTRA, SERVER_ID_EXTRA]);
    assert_eq!(3, keys.len());
    assert!(keys.iter().all(|key| key.starts_with("p4-")), "{keys:?}");
}

#[mononoke::test]
fn test_path_with_a_newline_is_an_error() {
    let changelist = changelist(vec![revision("//test1/a\nb", Action::Delete)]);
    let result = changeset(
        DEPOT,
        SERVER_ID,
        &changelist,
        None,
        &HashMap::new(),
        &HashMap::new(),
    );
    assert!(
        matches!(&result, Err(TransformError::NewlineInPath(path)) if path == "a\nb"),
        "{result:?}"
    );
}

const NOT_UTF8: [&[u8]; 7] = [
    // @lint-ignore SPELL
    b"caf\xe9 na\xefve \xa3 sterling\n",
    b"literal \xef\xbf\xbd then \xff inserted",
    b"\xef\xbf\xbd\xff\xfe\xef\xbf\xbd",
    b"truncated emoji \xf0\x9f\x98",
    b"\xe2\x28\xa1 lone continuation",
    b"\x80",
    b"crlf\r\n\x93quoted\x94\r\n",
];

fn with_description(original: &[u8]) -> Changelist {
    let mut changelist = changelist(vec![revision("//test1/a.txt", Action::Delete)]);
    changelist.description = String::from_utf8_lossy(original).into_owned();
    changelist.description_bytes = Some(original.to_vec());
    changelist
}

#[mononoke::test]
fn test_non_utf8_description_stores_its_original_bytes() {
    for original in NOT_UTF8 {
        let changelist = with_description(original);
        let bcs = changeset(
            DEPOT,
            SERVER_ID,
            &changelist,
            None,
            &HashMap::new(),
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(
            Some(original),
            extra(&bcs, DESCRIPTION_BYTES_EXTRA),
            "{original:?}"
        );
        assert_eq!(changelist.description, bcs.message());
    }
}

#[mononoke::test]
fn test_utf8_description_stores_no_bytes() {
    let utf8: [&[u8]; 3] = [b"", b"plain", b"literal \xef\xbf\xbd only"];
    for original in utf8 {
        let bcs = changeset(
            DEPOT,
            SERVER_ID,
            &with_description(original),
            None,
            &HashMap::new(),
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(None, extra(&bcs, DESCRIPTION_BYTES_EXTRA), "{original:?}");
    }
    let without_bytes = changelist(vec![revision("//test1/a.txt", Action::Delete)]);
    let bcs = changeset(
        DEPOT,
        SERVER_ID,
        &without_bytes,
        None,
        &HashMap::new(),
        &HashMap::new(),
    )
    .unwrap();
    assert_eq!(None, extra(&bcs, DESCRIPTION_BYTES_EXTRA));
}

#[mononoke::test]
fn test_description_that_is_not_from_its_bytes_is_an_error() {
    let mut changelist = changelist(vec![revision("//test1/a.txt", Action::Delete)]);
    changelist.description_bytes = Some(NOT_UTF8[0].to_vec());
    let result = changeset(
        DEPOT,
        SERVER_ID,
        &changelist,
        None,
        &HashMap::new(),
        &HashMap::new(),
    );
    assert!(
        matches!(
            result,
            Err(TransformError::DescriptionMismatch { changelist: 42 })
        ),
        "{result:?}"
    );
}
