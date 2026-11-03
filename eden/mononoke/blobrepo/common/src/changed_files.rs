/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Error;
use blobstore::KeyedBlobstore;
use context::CoreContext;
use futures::TryStreamExt;
use futures::future;
use manifest::Diff;
use manifest::Entry;
use manifest::ManifestOps;
use manifest::find_intersection_of_diffs;
use mercurial_types::HgManifestId;
use mononoke_types::NonRootMPath;

/// NOTE: To be used only for generating list of files for old, Mercurial format of Changesets.
///
/// This function is used to extract any new files that the given root manifest has provided
/// compared to the provided p1 and p2 parents.
/// A files is considered new when it was not present in neither of parent manifests or it was
/// present, but with a different content.
/// It sorts the returned `Vec<NonRootMPath>` in the order expected by Mercurial.
pub async fn compute_changed_files(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    root: HgManifestId,
    p1: Option<HgManifestId>,
    p2: Option<HgManifestId>,
) -> Result<Vec<NonRootMPath>, Error> {
    let files = match (p1, p2) {
        (None, None) => {
            root.list_leaf_entries(ctx, blobstore)
                .map_ok(|(path, _)| path)
                .try_collect()
                .await?
        }
        (Some(manifest), None) | (None, Some(manifest)) => {
            compute_changed_files_pair(ctx, blobstore.clone(), root, manifest).await?
        }
        (Some(p1), Some(p2)) => {
            let (changed, removed_p1, removed_p2) = future::try_join3(
                find_intersection_of_diffs(ctx.clone(), blobstore.clone(), root, vec![p1, p2])
                    .try_filter_map(|(path, entry)| async move {
                        match entry {
                            Entry::Leaf(_) => Ok(Option::<NonRootMPath>::from(path)),
                            _ => Ok(None),
                        }
                    })
                    .try_collect::<Vec<_>>(),
                compute_removed_files(&ctx, blobstore.clone(), root, Some(p1)),
                compute_removed_files(&ctx, blobstore.clone(), root, Some(p2)),
            )
            .await?;
            changed
                .into_iter()
                .chain(removed_p1)
                .chain(removed_p2)
                .collect::<HashSet<_>>()
        }
    };

    let mut files: Vec<NonRootMPath> = files.into_iter().collect();
    files.sort_unstable_by(mercurial_mpath_comparator);
    Ok(files)
}

async fn compute_changed_files_pair(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    to: HgManifestId,
    from: HgManifestId,
) -> Result<HashSet<NonRootMPath>, Error> {
    from.diff(ctx, blobstore, to)
        .try_filter_map(|diff| async move {
            let (path, entry) = match diff {
                Diff::Added(path, entry) | Diff::Removed(path, entry) => (path, entry),
                Diff::Changed(path, .., entry) => (path, entry),
            };

            match entry {
                Entry::Tree(_) => Ok(None),
                Entry::Leaf(_) => Ok(Option::<NonRootMPath>::from(path)),
            }
        })
        .try_collect()
        .await
}

async fn compute_removed_files(
    ctx: &CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    child: HgManifestId,
    parent: Option<HgManifestId>,
) -> Result<Vec<NonRootMPath>, Error> {
    match parent {
        Some(parent) => {
            parent
                .filtered_diff(
                    ctx.clone(),
                    blobstore.clone(),
                    child,
                    blobstore,
                    |diff| match diff {
                        Diff::Removed(path, Entry::Leaf(_)) => path.into_optional_non_root_path(),
                        _ => None,
                    },
                    |diff| !matches!(diff, Diff::Added(..)),
                    Default::default(),
                )
                .try_collect()
                .await
        }
        None => Ok(Vec::new()),
    }
}

fn mercurial_mpath_comparator(a: &NonRootMPath, b: &NonRootMPath) -> ::std::cmp::Ordering {
    a.compare_bytes(b)
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::test]
    fn test_mercurial_mpath_comparator() {
        let mut paths = vec![
            "foo/bar/baz/a.test",
            "foo/bar/baz-boo/a.test",
            "foo-faz/bar/baz/a.test",
        ];

        let mut mpaths: Vec<_> = paths
            .iter()
            .map(|path| NonRootMPath::new(path).expect("invalid path"))
            .collect();

        {
            mpaths.sort_unstable();
            let result: Vec<_> = mpaths
                .iter()
                .map(|mpath| String::from_utf8(mpath.to_vec()).unwrap())
                .collect();
            assert!(paths == result);
        }

        {
            paths.sort_unstable();
            mpaths.sort_unstable_by(mercurial_mpath_comparator);
            let result: Vec<_> = mpaths
                .iter()
                .map(|mpath| String::from_utf8(mpath.to_vec()).unwrap())
                .collect();
            assert!(paths == result);
        }
    }
}
