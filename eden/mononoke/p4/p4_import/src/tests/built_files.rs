/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use mononoke_types_mocks::contentid::TWOS_CTID;

use super::*;
use crate::built_files::BuiltFiles;

/// Every path of up to three components named `a`, `b` or `c`: few enough that changes
/// land above, below and on each other all the time.
fn paths() -> Vec<NonRootMPath> {
    let names = ["a", "b", "c"];
    let below = |parents: &[String]| -> Vec<String> {
        parents
            .iter()
            .flat_map(|parent| names.iter().map(move |name| format!("{parent}/{name}")))
            .collect()
    };
    let one: Vec<String> = names.iter().map(|name| name.to_string()).collect();
    let two = below(&one);
    let three = below(&two);
    [one, two, three].concat().iter().map(|p| path(p)).collect()
}

/// The same numbers on every run.
struct Numbers(u64);

impl Numbers {
    fn below(&mut self, limit: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % limit
    }
}

/// A changeset that stores or deletes a few of `paths`, or `None` for a choice that isn't
/// a changeset, such as files stored above each other.
fn some_changeset(numbers: &mut Numbers, paths: &[NonRootMPath]) -> Option<BonsaiChangeset> {
    let file_changes = (0..1 + numbers.below(4))
        .map(|_| {
            let path = paths[numbers.below(paths.len())].clone();
            let change = match numbers.below(4) {
                0 => FileChange::Deletion,
                1 => FileChange::tracked(
                    TWOS_CTID,
                    FileType::Executable,
                    1,
                    None,
                    GitLfs::FullContent,
                ),
                _ => {
                    FileChange::tracked(ONES_CTID, FileType::Regular, 1, None, GitLfs::FullContent)
                }
            };
            (path, change)
        })
        .collect();
    BonsaiChangesetMut {
        file_changes,
        ..Default::default()
    }
    .freeze()
    .ok()
}

#[mononoke::test]
fn test_files_by_path_are_the_files_after_the_changesets() {
    let paths = paths();
    let mut numbers = Numbers(7);
    let (mut untouched, mut files, mut not_files) = (0, 0, 0);
    for _history in 0..300 {
        let history: Vec<_> = (0..12)
            .filter_map(|_| some_changeset(&mut numbers, &paths))
            .collect();
        let mut by_path = BuiltFiles::default();
        for (built, changeset) in history.iter().enumerate() {
            by_path.add(changeset);
            for path in &paths {
                let expected = file_after(history[..=built].iter(), path);
                assert_eq!(
                    expected,
                    by_path.file_after(path),
                    "{path} after {:?}",
                    history[..=built]
                        .iter()
                        .map(|changeset| changeset.file_changes().collect::<Vec<_>>())
                        .collect::<Vec<_>>()
                );
                match expected {
                    None => untouched += 1,
                    Some(Some(_)) => files += 1,
                    Some(None) => not_files += 1,
                }
            }
        }
    }
    // The histories gave each answer many times.
    assert!(untouched > 1000 && files > 1000 && not_files > 1000);
}

#[mononoke::test]
fn test_file_deleted_below_a_path_is_not_a_file_stored_below_it() {
    let history = [
        changeset_of(&[("a", true)]),
        changeset_of(&[("a", false), ("a/b", true)]),
        changeset_of(&[("a/b", false)]),
    ];
    let mut by_path = BuiltFiles::default();
    for changeset in &history {
        by_path.add(changeset);
    }

    // The third changeset touches neither `a` nor `a/b/c`: the second one answers for both.
    assert_eq!(Some(None), by_path.file_after(&path("a")));
    assert_eq!(Some(None), by_path.file_after(&path("a/b")));
    assert_eq!(Some(None), by_path.file_after(&path("a/b/c")));
    assert_eq!(None, by_path.file_after(&path("b")));
}

fn by_path() -> ImportOptions {
    ImportOptions {
        index_built_files: true,
        ..Default::default()
    }
}

/// The copy source of `c.txt` is checked against the two changesets built before it, with
/// the files by path and without.
#[mononoke::fbinit_test]
async fn test_copy_sources_are_kept_and_dropped_as_without_files_by_path(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

    for copy_source in ["//test1/b.txt", "//test1/gone.txt", "//test1/never.txt"] {
        let source = branch_after_two_changelists(copy_source);
        let mut built = Vec::new();
        for options in [ImportOptions::default(), by_path()] {
            let changesets =
                build_changesets(&ctx, &repo, &source, DEPOT, None, &[7, 9, 11], &options).await?;
            built.push(
                changesets
                    .iter()
                    .map(|built| {
                        (
                            built.changeset.get_changeset_id(),
                            built.dropped_copy_sources,
                        )
                    })
                    .collect::<Vec<_>>(),
            );
        }
        assert_eq!(built[0], built[1], "{copy_source}");
    }
    Ok(())
}
