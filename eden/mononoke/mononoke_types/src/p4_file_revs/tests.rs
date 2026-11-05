/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use mononoke_macros::mononoke;

use super::P4_FILE_REVS_EXTRA;
use super::P4FileRevs;
use crate::BonsaiChangeset;
use crate::BonsaiChangesetMut;
use crate::DateTime;
use crate::NonRootMPath;

fn create_bonsai(hg_extra: impl IntoIterator<Item = (String, Vec<u8>)>) -> Result<BonsaiChangeset> {
    BonsaiChangesetMut {
        author: "author".into(),
        author_date: DateTime::from_timestamp(0, 0)?,
        message: "message".into(),
        hg_extra: hg_extra.into_iter().collect(),
        ..Default::default()
    }
    .freeze()
}

fn path(path: &str) -> NonRootMPath {
    NonRootMPath::new(path).expect("valid test path")
}

fn revs(value: &[u8]) -> P4FileRevs {
    P4FileRevs::from_hg_extras([(P4_FILE_REVS_EXTRA, value)]).expect("extra is present")
}

/// What `p4_transform::file_revs` writes for a changelist editing `docs/readme.txt`
/// at revision 3, adding `src/a b.txt` and deleting `src/old.c` at revision 7.
const WRITER_OUTPUT: &[u8] = b"3\tdocs/readme.txt\n1\tsrc/a b.txt\n7\tsrc/old.c\n";

#[mononoke::test]
fn test_round_trips_writer_output() -> Result<()> {
    let revs = revs(WRITER_OUTPUT);
    assert_eq!(revs.revision(&path("docs/readme.txt"))?, Some(3));
    assert_eq!(revs.revision(&path("src/a b.txt"))?, Some(1));
    assert_eq!(revs.revision(&path("src/old.c"))?, Some(7));
    assert_eq!(
        revs.revision(&path("docs"))?,
        None,
        "directories have no revision"
    );
    assert_eq!(revs.revision(&path("src/new.c"))?, None);
    assert_eq!(revs.revision(&path("a"))?, None, "before the first line");
    assert_eq!(revs.revision(&path("z"))?, None, "after the last line");
    let expected = [
        (path("docs/readme.txt"), 3),
        (path("src/a b.txt"), 1),
        (path("src/old.c"), 7),
    ];
    assert_eq!(revs.iter().collect::<Result<Vec<_>>>()?, expected);
    let written: P4FileRevs = expected.into_iter().collect();
    assert_eq!(
        written, revs,
        "FromIterator writes what the importer writes"
    );
    Ok(())
}

#[mononoke::test]
fn test_lookup_compares_paths_not_bytes() -> Result<()> {
    // `-` sorts before `/` as a byte, but `a/b` sorts before `a-b/x` as a path, and the
    // writer sorts paths. Every line must be found whichever way the search probes.
    let paths = ["a/b", "a/c", "a-b/x", "a-b/y", "ab"];
    let revs: P4FileRevs = paths
        .iter()
        .enumerate()
        .map(|(index, name)| (path(name), index as u32 + 1))
        .collect();
    let mut sorted = paths.map(path);
    sorted.sort();
    assert_eq!(sorted, paths.map(path), "fixture is in path order");
    for (index, name) in paths.iter().enumerate() {
        assert_eq!(
            revs.revision(&path(name))?,
            Some(index as u32 + 1),
            "{name}"
        );
    }
    assert_eq!(revs.revision(&path("a"))?, None);
    assert_eq!(revs.revision(&path("a-b"))?, None);
    assert_eq!(revs.revision(&path("a/bb"))?, None);
    Ok(())
}

#[mononoke::test]
fn test_from_bcs() -> Result<()> {
    let bcs = create_bonsai([(P4_FILE_REVS_EXTRA.to_string(), WRITER_OUTPUT.to_vec())])?;
    let revs = P4FileRevs::from_bcs(&bcs).expect("extra is present");
    assert_eq!(revs.revision(&path("src/old.c"))?, Some(7));

    let no_files = create_bonsai([(P4_FILE_REVS_EXTRA.to_string(), Vec::new())])?;
    let revs = P4FileRevs::from_bcs(&no_files).expect("extra is present");
    assert!(
        revs.is_empty(),
        "a changelist without files has no revisions"
    );
    assert_eq!(revs.revision(&path("src/old.c"))?, None);
    assert_eq!(revs.iter().count(), 0);

    let missing = create_bonsai([("convert_source".to_string(), b"p4".to_vec())])?;
    assert_eq!(P4FileRevs::from_bcs(&missing), None);
    Ok(())
}

#[mononoke::test]
fn test_path_with_tab() -> Result<()> {
    let revs = revs(b"2\ta\tb.txt\n");
    assert_eq!(revs.revision(&path("a\tb.txt"))?, Some(2));
    Ok(())
}

#[mononoke::test]
fn test_malformed_names_the_line() {
    let cases: [(&[u8], &str, &str); 6] = [
        (
            b"3\tdocs/readme.txt\nnotarev\tsrc/a.c\n",
            "src/a.c",
            "line 2",
        ),
        (b"3\tdocs/readme.txt\n4 src/a.c\n", "src/a.c", "line 2"),
        (b"0\tdocs/readme.txt\n", "docs/readme.txt", "line 1"),
        (b"3\t\n", "x", "line 1"),
        (b"3\tdocs/readme.txt", "docs/readme.txt", "newline"),
        (b"3\tdocs/readme.txt\n4\tsrc/a.c", "src/a.c", "newline"),
    ];
    for (value, lookup, expected) in cases {
        let revs = revs(value);
        let error = revs
            .revision(&path(lookup))
            .expect_err("malformed value is rejected");
        let message = format!("{error:#}");
        assert!(
            message.contains(expected) && message.contains(P4_FILE_REVS_EXTRA),
            "{message:?} should mention {expected:?} for {value:?}"
        );
        let error = revs
            .iter()
            .collect::<Result<Vec<_>>>()
            .expect_err("iteration rejects it too");
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
}
