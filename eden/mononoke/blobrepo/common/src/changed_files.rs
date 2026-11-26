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
use manifest::Manifest;
use manifest::ManifestOps;
use manifest::StoreLoadable;
use manifest::TrieMapOps;
use manifest::find_intersection_of_diffs_and_parents;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgManifestId;
use mercurial_types::NULL_HASH;
use mercurial_types::sharded_augmented_manifest::HgAugmentedFileLeafNode;
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
    changed_files(ctx, blobstore, root, p1, p2, |a, b| a == b).await
}

/// Like `compute_changed_files`, but reads the augmented manifests stored under
/// the same ids, and compares files by their Hg file type and filenode only. A
/// null parent is an empty tree, as it is for Hg manifests.
pub async fn compute_changed_files_from_augmented(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    root: HgAugmentedManifestId,
    p1: Option<HgAugmentedManifestId>,
    p2: Option<HgAugmentedManifestId>,
) -> Result<Vec<NonRootMPath>, Error> {
    let non_null =
        |id: Option<HgAugmentedManifestId>| id.filter(|id| id.into_nodehash() != NULL_HASH);
    changed_files(
        ctx,
        blobstore,
        root,
        non_null(p1),
        non_null(p2),
        |a: &HgAugmentedFileLeafNode, b| a.hg_leaf() == b.hg_leaf(),
    )
    .await
}

async fn changed_files<TreeId, Leaf>(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    root: TreeId,
    p1: Option<TreeId>,
    p2: Option<TreeId>,
    same_file: fn(&Leaf, &Leaf) -> bool,
) -> Result<Vec<NonRootMPath>, Error>
where
    TreeId: StoreLoadable<Arc<dyn KeyedBlobstore>> + Clone + Send + Sync + Eq + Unpin + 'static,
    <TreeId as StoreLoadable<Arc<dyn KeyedBlobstore>>>::Value:
        Manifest<Arc<dyn KeyedBlobstore>, TreeId = TreeId, Leaf = Leaf> + Send + Sync,
    Leaf: Clone + Send + Sync + Eq + Unpin + 'static,
    <<TreeId as StoreLoadable<Arc<dyn KeyedBlobstore>>>::Value as Manifest<
        Arc<dyn KeyedBlobstore>,
    >>::TrieMapType: TrieMapOps<Arc<dyn KeyedBlobstore>, Entry<TreeId, Leaf>> + Eq,
{
    let files = match (p1, p2) {
        (None, None) => {
            root.list_leaf_entries(ctx, blobstore)
                .map_ok(|(path, _)| path)
                .try_collect()
                .await?
        }
        (Some(manifest), None) | (None, Some(manifest)) => {
            compute_changed_files_pair(ctx, blobstore.clone(), root, manifest, same_file).await?
        }
        (Some(p1), Some(p2)) => {
            let (changed, removed_p1, removed_p2) = future::try_join3(
                find_intersection_of_diffs_and_parents(
                    ctx.clone(),
                    blobstore.clone(),
                    root.clone(),
                    vec![p1.clone(), p2.clone()],
                )
                .try_filter_map(|(path, entry, parent_entries)| async move {
                    let Entry::Leaf(leaf) = entry else {
                        return Ok(None);
                    };
                    let same_as_a_parent = parent_entries.iter().any(|parent_entry| {
                        matches!(parent_entry, Entry::Leaf(parent_leaf) if same_file(parent_leaf, &leaf))
                    });
                    Ok((!same_as_a_parent)
                        .then(|| Option::<NonRootMPath>::from(path))
                        .flatten())
                })
                .try_collect::<Vec<_>>(),
                compute_removed_files(&ctx, blobstore.clone(), root.clone(), Some(p1)),
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

async fn compute_changed_files_pair<TreeId, Leaf>(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    to: TreeId,
    from: TreeId,
    same_file: fn(&Leaf, &Leaf) -> bool,
) -> Result<HashSet<NonRootMPath>, Error>
where
    TreeId: StoreLoadable<Arc<dyn KeyedBlobstore>> + Clone + Send + Sync + Eq + Unpin + 'static,
    <TreeId as StoreLoadable<Arc<dyn KeyedBlobstore>>>::Value:
        Manifest<Arc<dyn KeyedBlobstore>, TreeId = TreeId, Leaf = Leaf> + Send + Sync,
    Leaf: Clone + Send + Sync + Eq + Unpin + 'static,
    <<TreeId as StoreLoadable<Arc<dyn KeyedBlobstore>>>::Value as Manifest<
        Arc<dyn KeyedBlobstore>,
    >>::TrieMapType: TrieMapOps<Arc<dyn KeyedBlobstore>, Entry<TreeId, Leaf>> + Eq,
{
    from.diff(ctx, blobstore, to)
        .try_filter_map(|diff| async move {
            let (path, entry) = match diff {
                Diff::Added(path, entry) | Diff::Removed(path, entry) => (path, entry),
                Diff::Changed(_, Entry::Leaf(old), Entry::Leaf(new)) if same_file(&old, &new) => {
                    return Ok(None);
                }
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

async fn compute_removed_files<TreeId, Leaf>(
    ctx: &CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    child: TreeId,
    parent: Option<TreeId>,
) -> Result<Vec<NonRootMPath>, Error>
where
    TreeId: StoreLoadable<Arc<dyn KeyedBlobstore>> + Clone + Send + Sync + Eq + Unpin + 'static,
    <TreeId as StoreLoadable<Arc<dyn KeyedBlobstore>>>::Value:
        Manifest<Arc<dyn KeyedBlobstore>, TreeId = TreeId, Leaf = Leaf> + Send + Sync,
    Leaf: Clone + Send + Sync + Eq + Unpin + 'static,
    <<TreeId as StoreLoadable<Arc<dyn KeyedBlobstore>>>::Value as Manifest<
        Arc<dyn KeyedBlobstore>,
    >>::TrieMapType: TrieMapOps<Arc<dyn KeyedBlobstore>, Entry<TreeId, Leaf>> + Eq,
{
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
