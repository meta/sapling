/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Derivation logic for the Directory Branch Cluster Manifest (DBCM).
//!
//! The DBCM tracks "clusters" of directories that share content due to subtree
//! copy or merge operations. Each cluster has one primary (the original source)
//! and zero or more secondaries (the copies/merge targets).
//!
//! # Manifest Structure
//!
//! The manifest is a tree where each directory entry stores:
//! - `primary`: If this directory was copied/merged from another, points to the source
//! - `secondaries`: List of directories that were copied/merged from this one
//!
//! Unlike other manifests, DBCM only stores directories (no files). It tracks
//! cluster relationships between directories, which files don't have.
//!
//! # Example
//!
//! Consider a repository with these directories after several operations:
//!
//! ```text
//! Initial state: directories A, B, C, D exist independently
//!
//! Commit 1: Subtree Copy A → B
//! Commit 2: Subtree Copy A → C
//! Commit 3: Subtree Merge D → A
//! ```
//!
//! After these commits, the manifest looks like:
//!
//! ```text
//! Root
//! ├── A: { primary: None, secondaries: [B, C, D] }
//! ├── B: { primary: A, secondaries: None }
//! ├── C: { primary: A, secondaries: None }
//! └── D: { primary: A, secondaries: None }
//! ```
//!
//! All four directories form a single cluster with A as the primary.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use blobstore::Storable;
use context::CoreContext;
use derived_data_manager::DerivationContext;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future::BoxFuture;
use futures::stream;
use mononoke_types::BlobstoreValue;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::MPath;
use mononoke_types::MPathElement;
use mononoke_types::TrieMap;
use mononoke_types::directory_branch_cluster_manifest::ClusterMember;
use mononoke_types::directory_branch_cluster_manifest::DirectoryBranchClusterManifest;
use mononoke_types::directory_branch_cluster_manifest::DirectoryBranchClusterManifestEntry;
use mononoke_types::directory_branch_cluster_manifest::DirectoryBranchClusterManifestFile;
use mononoke_types::sharded_map_v2::ShardedMapV2Node;
use mononoke_types::typed_hash::DirectoryBranchClusterManifestId;

use crate::RootDirectoryBranchClusterManifestId;
use crate::cluster_changes::ClusterUpdate;
use crate::cluster_changes::compute_cluster_changes;

/// Create an empty DBCM manifest.
async fn empty_manifest_id(
    ctx: &CoreContext,
    blobstore: &impl KeyedBlobstore,
) -> Result<DirectoryBranchClusterManifestId> {
    let empty_manifest = DirectoryBranchClusterManifest::empty();
    empty_manifest.into_blob().store(ctx, blobstore).await
}

/// Merge multiple parent manifests into a single base manifest.
/// No cluster changes are applied - just combines parent state.
pub async fn merge_parent_manifests(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    parents: Vec<DirectoryBranchClusterManifest>,
) -> Result<DirectoryBranchClusterManifest> {
    if parents.is_empty() {
        return Ok(DirectoryBranchClusterManifest::empty());
    }
    if parents.len() == 1 {
        return Ok(parents.into_iter().next().unwrap());
    }
    merge_manifests_recursive(ctx, blobstore, parents, MPath::ROOT).await
}

/// Apply cluster changes to a base manifest.
pub async fn apply_cluster_changes_to_manifest(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    base: DirectoryBranchClusterManifest,
    changes: HashMap<MPath, ClusterUpdate>,
) -> Result<DirectoryBranchClusterManifest> {
    if changes.is_empty() {
        return Ok(base);
    }
    apply_changes_recursive(ctx, blobstore, base, &changes, MPath::ROOT).await
}

/// Recursively merge parent manifests at a given path.
fn merge_manifests_recursive<'a>(
    ctx: &'a CoreContext,
    blobstore: &'a Arc<dyn KeyedBlobstore>,
    parents: Vec<DirectoryBranchClusterManifest>,
    current_path: MPath,
) -> BoxFuture<'a, Result<DirectoryBranchClusterManifest>> {
    Box::pin(async move {
        // Collect all child path elements from all parents
        let mut child_elements: std::collections::HashSet<MPathElement> =
            std::collections::HashSet::new();

        // Collect children from parents
        let mut parent_child_entries: HashMap<MPathElement, Vec<DirectoryBranchClusterManifest>> =
            HashMap::new();
        for parent in &parents {
            let parent_entries: Vec<_> = parent
                .clone()
                .into_subentries(ctx, blobstore)
                .try_collect()
                .await?;
            for (elem, entry) in parent_entries {
                if let DirectoryBranchClusterManifestEntry::Directory(dir) = entry {
                    child_elements.insert(elem.clone());
                    parent_child_entries.entry(elem).or_default().push(dir);
                }
                // Files don't have subentries, so we skip them during merge
            }
        }

        // Build entries for each child
        let mut entries: HashMap<MPathElement, DirectoryBranchClusterManifest> = HashMap::new();

        for elem in child_elements {
            let child_path = current_path.join_element(Some(&elem));
            let child_parent_entries = parent_child_entries.remove(&elem).unwrap_or_default();

            if child_parent_entries.len() > 1 {
                // Multiple parents have this entry - need to recurse and merge
                let child_manifest =
                    merge_manifests_recursive(ctx, blobstore, child_parent_entries, child_path)
                        .await?;

                entries.insert(elem, child_manifest);
            } else if let Some(single_parent_entry) = child_parent_entries.into_iter().next() {
                // Single parent has this entry - just copy through
                entries.insert(elem, single_parent_entry);
            }
        }

        // Build the final manifest at this level
        let subentries_trie: TrieMap<_> = entries
            .into_iter()
            .map(|(name, dir)| {
                (
                    name.to_smallvec(),
                    itertools::Either::Left(DirectoryBranchClusterManifestEntry::Directory(dir)),
                )
            })
            .collect();

        let subentries =
            ShardedMapV2Node::from_entries_and_partial_maps(ctx, blobstore, subentries_trie)
                .await?;

        // Merge parent cluster info at this level
        let mut merged_primary: Option<MPath> = None;
        let mut merged_secondaries: Option<Vec<MPath>> = None;

        for parent in &parents {
            if merged_primary.is_none() {
                merged_primary = parent.primary.clone();
            }
            if let Some(ref parent_secs) = parent.secondaries {
                let mut secs = merged_secondaries.take().unwrap_or_default();
                secs.extend(parent_secs.iter().cloned());
                secs.sort();
                secs.dedup();
                merged_secondaries = Some(secs);
            }
        }

        Ok(DirectoryBranchClusterManifest {
            subentries,
            secondaries: merged_secondaries,
            primary: merged_primary,
        })
    })
}

/// Recursively apply cluster changes to a manifest.
///
/// This function traverses the manifest tree and applies cluster updates at each path where
/// changes are needed. It handles both existing entries (from the base manifest) and new
/// entries (from cluster changes).
///
/// # Algorithm Overview
///
/// For each level in the tree, we:
/// 1. Collect `child_elements` - the set of path elements we need to process at this level
/// 2. For each child element, determine if we need to:
///    - Recurse deeper (if there are nested changes below this path)
///    - Apply a direct update (if there's a change at exactly this path)
///    - Copy through unchanged (if the entry exists but has no changes)
///
/// # Key Data Structures
///
/// - `base_child_dirs` / `base_child_files`: Existing entries from the base manifest at this
///   level. These are the entries we're modifying.
/// - `child_elements`: The union of all path elements we need to process - both from existing
///   entries AND from the cluster changes. This ensures we create new entries for paths that
///   don't exist yet.
/// - `result_entries`: The final set of entries to include in the output manifest at this level.
///
/// # Example
///
/// Given a base manifest with structure:
/// ```text
///   root
///   └── a/
///       └── b/  (primary: None, secondaries: None)
/// ```
///
/// And cluster changes:
/// ```text
///   "a/b" -> ClusterUpdate { set_primary: Some("x/y"), ... }
///   "a/c" -> ClusterUpdate { set_primary: Some("x/z"), ... }  // new path!
/// ```
///
/// At the root level:
/// - `child_elements` = {"a"} (from base) ∪ {"a"} (from changes) = {"a"}
/// - We recurse into "a" because it has nested changes
///
/// At the "a" level:
/// - `child_elements` = {"b"} (from base) ∪ {"b", "c"} (from changes) = {"b", "c"}
/// - For "b": apply the update to the existing directory
/// - For "c": create a new directory with the cluster info (since it doesn't exist in base)
///
/// Result:
/// ```text
///   root
///   └── a/
///       ├── b/  (primary: "x/y", secondaries: None)
///       └── c/  (primary: "x/z", secondaries: None)  // newly created
/// ```
fn apply_changes_recursive<'a>(
    ctx: &'a CoreContext,
    blobstore: &'a Arc<dyn KeyedBlobstore>,
    base: DirectoryBranchClusterManifest,
    changes: &'a HashMap<MPath, ClusterUpdate>,
    current_path: MPath,
) -> BoxFuture<'a, Result<DirectoryBranchClusterManifest>> {
    Box::pin(async move {
        // Collect all child path elements we need to process
        let mut child_elements: std::collections::HashSet<MPathElement> =
            std::collections::HashSet::new();

        // Collect existing children from base (both files and directories)
        let base_entries: Vec<_> = base
            .clone()
            .into_subentries(ctx, blobstore)
            .try_collect()
            .await?;
        let mut base_child_dirs: HashMap<MPathElement, DirectoryBranchClusterManifest> =
            HashMap::new();
        let mut base_child_files: HashMap<MPathElement, DirectoryBranchClusterManifestFile> =
            HashMap::new();
        for (elem, entry) in base_entries {
            child_elements.insert(elem.clone());
            match entry {
                DirectoryBranchClusterManifestEntry::Directory(dir) => {
                    base_child_dirs.insert(elem, dir);
                }
                DirectoryBranchClusterManifestEntry::File(file) => {
                    base_child_files.insert(elem, file);
                }
            }
        }

        // Collect children from cluster changes that apply at this level or below
        for path in changes.keys() {
            if current_path.is_prefix_of(path) && path != &current_path {
                let path_components: Vec<_> = path.into_iter().collect();
                let current_components = current_path.num_components();
                if path_components.len() > current_components {
                    child_elements.insert(path_components[current_components].clone());
                }
            }
        }

        // Build entries for each child
        let mut result_entries: HashMap<MPathElement, DirectoryBranchClusterManifestEntry> =
            HashMap::new();

        for elem in child_elements {
            let child_path = current_path.join_element(Some(&elem));
            let base_dir = base_child_dirs.remove(&elem);
            let base_file = base_child_files.remove(&elem);
            let direct_update = changes.get(&child_path);

            // If the path is marked as deleted, skip it entirely
            if direct_update.is_some_and(|u| u.is_deleted) {
                continue;
            }

            // Check if there are any cluster changes for paths under this child
            let has_nested_changes = changes.keys().any(|p| {
                if p == &child_path {
                    false
                } else {
                    child_path.is_prefix_of(p)
                }
            });

            if has_nested_changes {
                // Need to recurse to apply nested changes - this must be a directory
                let child_base = base_dir.unwrap_or_else(DirectoryBranchClusterManifest::empty);

                let mut child_manifest =
                    apply_changes_recursive(ctx, blobstore, child_base, changes, child_path)
                        .await?;

                if let Some(update) = direct_update {
                    apply_cluster_update(&mut child_manifest, update);
                }

                result_entries.insert(
                    elem,
                    DirectoryBranchClusterManifestEntry::Directory(child_manifest),
                );
            } else if let Some(update) = direct_update {
                // Direct update at this path - could be file or directory
                if let Some(mut dir) = base_dir {
                    // Existing directory
                    apply_cluster_update(&mut dir, update);
                    result_entries
                        .insert(elem, DirectoryBranchClusterManifestEntry::Directory(dir));
                } else if let Some(mut file) = base_file {
                    // Existing file
                    apply_cluster_update(&mut file, update);
                    result_entries.insert(elem, DirectoryBranchClusterManifestEntry::File(file));
                } else {
                    // New entry - create as a directory by default for cluster updates
                    // (files would typically come from parent manifests or explicit file changes)
                    let mut dir = DirectoryBranchClusterManifest::empty();
                    apply_cluster_update(&mut dir, update);
                    result_entries
                        .insert(elem, DirectoryBranchClusterManifestEntry::Directory(dir));
                }
            } else if let Some(dir) = base_dir {
                // No changes - just copy through directory
                result_entries.insert(elem, DirectoryBranchClusterManifestEntry::Directory(dir));
            } else if let Some(file) = base_file {
                // No changes - just copy through file
                result_entries.insert(elem, DirectoryBranchClusterManifestEntry::File(file));
            }
        }

        // Build the final manifest at this level
        let subentries_trie: TrieMap<_> = result_entries
            .into_iter()
            .map(|(name, entry)| (name.to_smallvec(), itertools::Either::Left(entry)))
            .collect();

        let subentries =
            ShardedMapV2Node::from_entries_and_partial_maps(ctx, blobstore, subentries_trie)
                .await?;

        Ok(DirectoryBranchClusterManifest {
            subentries,
            secondaries: base.secondaries,
            primary: base.primary,
        })
    })
}

fn apply_cluster_update<T: ClusterMember>(member: &mut T, update: &ClusterUpdate) {
    if update.clear_primary {
        *member.primary_mut() = None;
    } else if let Some(ref new_primary) = update.set_primary {
        *member.primary_mut() = Some(new_primary.clone());
    }

    if !update.add_secondaries.is_empty() {
        let mut secondaries = member.secondaries_mut().take().unwrap_or_default();
        secondaries.extend(update.add_secondaries.iter().cloned());
        secondaries.sort();
        secondaries.dedup();
        *member.secondaries_mut() = Some(secondaries);
    }

    if !update.delete_secondaries.is_empty() {
        if let Some(secondaries) = member.secondaries_mut() {
            secondaries.retain(|s| !update.delete_secondaries.contains(s));
            if secondaries.is_empty() {
                *member.secondaries_mut() = None;
            }
        }
    }
}

pub(crate) async fn derive_single(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: BonsaiChangeset,
    parents: Vec<RootDirectoryBranchClusterManifestId>,
    _known: Option<&HashMap<ChangesetId, RootDirectoryBranchClusterManifestId>>,
) -> Result<RootDirectoryBranchClusterManifestId> {
    let blobstore = derivation_ctx.blobstore();

    // Step 1: Load parent manifests
    let parent_manifests = stream::iter(parents)
        .map(|parent| async move { parent.0.load(ctx, blobstore).await })
        .buffered(100)
        .try_collect::<Vec<_>>()
        .await?;

    // Step 2: Merge parent manifests into a single base manifest
    let merged_parent = merge_parent_manifests(ctx, blobstore, parent_manifests).await?;

    // Step 3: Compute cluster changes using the merged parent for lookups
    // Fast path: no subtree operations AND no file deletions means no cluster changes
    let has_deletions = bonsai.file_changes().any(|(_, change)| {
        matches!(
            change,
            mononoke_types::FileChange::Deletion | mononoke_types::FileChange::UntrackedDeletion
        )
    });
    let cluster_changes = if bonsai.subtree_changes().is_empty() && !has_deletions {
        HashMap::new()
    } else {
        compute_cluster_changes(ctx, derivation_ctx, blobstore, &bonsai, &merged_parent).await?
    };

    // Step 4: Apply cluster changes to the merged parent
    let manifest =
        apply_cluster_changes_to_manifest(ctx, blobstore, merged_parent, cluster_changes).await?;

    let is_empty = manifest.secondaries.is_none()
        && manifest.primary.is_none()
        && manifest.subentries.size() == 0;

    // Store the manifest
    Ok(RootDirectoryBranchClusterManifestId(if is_empty {
        empty_manifest_id(ctx, blobstore).await?
    } else {
        manifest
            .into_blob()
            .store(ctx, blobstore)
            .await
            .context("failed to store DirectoryBranchClusterManifest blob")?
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use blobstore::KeyedBlobstore;
    use context::CoreContext;
    use fbinit::FacebookInit;
    use futures::TryStreamExt;
    use memblob::KeyedMemblob;
    use mononoke_macros::mononoke;

    use super::*;

    fn mpath(s: &str) -> MPath {
        MPath::new(s).unwrap()
    }

    fn make_blobstore() -> Arc<dyn KeyedBlobstore> {
        Arc::new(KeyedMemblob::default())
    }

    fn cluster_update(
        set_primary: Option<&str>,
        add_secondaries: Vec<&str>,
        delete_secondaries: Vec<&str>,
    ) -> ClusterUpdate {
        ClusterUpdate {
            set_primary: set_primary.map(mpath),
            clear_primary: false,
            add_secondaries: add_secondaries.into_iter().map(mpath).collect(),
            delete_secondaries: delete_secondaries.into_iter().map(mpath).collect(),
            is_deleted: false,
        }
    }

    /// Helper that merges parents and applies changes - for testing convenience
    async fn build_manifest(
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        parents: Vec<DirectoryBranchClusterManifest>,
        changes: HashMap<MPath, ClusterUpdate>,
    ) -> Result<DirectoryBranchClusterManifest> {
        let merged = merge_parent_manifests(ctx, blobstore, parents).await?;
        apply_cluster_changes_to_manifest(ctx, blobstore, merged, changes).await
    }

    async fn get_cluster_info(
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        manifest: &DirectoryBranchClusterManifest,
        path: &str,
    ) -> Result<(Option<MPath>, Option<Vec<MPath>>)> {
        if path.is_empty() || path == "/" {
            // Root path - return root's cluster info
            return Ok((manifest.primary.clone(), manifest.secondaries.clone()));
        }

        // Parse the path into components and traverse the tree
        let mpath = MPath::new(path)?;
        let mut current_manifest = manifest.clone();
        let elements: Vec<_> = mpath.into_iter().collect();
        let last_idx = elements.len() - 1;

        for (idx, elem) in elements.into_iter().enumerate() {
            // Get subentries at current level
            let entries: BTreeMap<_, _> = current_manifest
                .clone()
                .into_subentries(ctx, blobstore)
                .try_collect()
                .await?;

            if let Some(entry) = entries.get(&elem) {
                match entry {
                    DirectoryBranchClusterManifestEntry::Directory(dir) => {
                        if idx == last_idx {
                            // This is the final path element - return its cluster info
                            return Ok((dir.primary.clone(), dir.secondaries.clone()));
                        }
                        // More path elements to traverse
                        current_manifest = dir.clone();
                    }
                    DirectoryBranchClusterManifestEntry::File(file) => {
                        if idx == last_idx {
                            // This is the final path element - return the file's cluster info
                            return Ok((file.primary.clone(), file.secondaries.clone()));
                        }
                        // Can't traverse into a file - path doesn't exist
                        return Ok((None, None));
                    }
                }
            } else {
                return Ok((None, None));
            }
        }

        Ok((
            current_manifest.primary.clone(),
            current_manifest.secondaries.clone(),
        ))
    }

    #[mononoke::fbinit_test]
    async fn test_build_manifest_empty(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = make_blobstore();

        let manifest = build_manifest(&ctx, &blobstore, vec![], HashMap::new()).await?;

        // Verify root has no cluster info
        let (primary, secondaries) = get_cluster_info(&ctx, &blobstore, &manifest, "").await?;
        assert!(primary.is_none());
        assert!(secondaries.is_none());
        assert_eq!(manifest.subentries.size(), 0);

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_build_manifest_merges_parent_entries(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = make_blobstore();

        // Create parent with "existing" path having cluster info
        let mut parent_changes = HashMap::new();
        parent_changes.insert(
            mpath("existing"),
            cluster_update(Some("src"), vec!["copy"], vec![]),
        );
        let parent = build_manifest(&ctx, &blobstore, vec![], parent_changes).await?;

        // Create child with "new" path
        let mut child_changes = HashMap::new();
        child_changes.insert(mpath("new"), cluster_update(Some("other"), vec![], vec![]));
        let manifest = build_manifest(&ctx, &blobstore, vec![parent], child_changes).await?;

        // Verify both entries exist with correct info
        let (primary, secondaries) =
            get_cluster_info(&ctx, &blobstore, &manifest, "existing").await?;
        assert_eq!(primary, Some(mpath("src")));
        assert_eq!(secondaries, Some(vec![mpath("copy")]));

        let (primary, secondaries) = get_cluster_info(&ctx, &blobstore, &manifest, "new").await?;
        assert_eq!(primary, Some(mpath("other")));
        assert!(secondaries.is_none());

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_build_manifest_subtree_copy(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = make_blobstore();

        // Simulate: Copy A → B
        // A should have B in secondaries
        // B should have primary = A
        let mut changes = HashMap::new();
        changes.insert(mpath("A"), cluster_update(None, vec!["B"], vec![]));
        changes.insert(mpath("B"), cluster_update(Some("A"), vec![], vec![]));

        let manifest = build_manifest(&ctx, &blobstore, vec![], changes).await?;

        // Verify A has B as secondary
        let (primary, secondaries) = get_cluster_info(&ctx, &blobstore, &manifest, "A").await?;
        assert!(primary.is_none());
        assert_eq!(secondaries, Some(vec![mpath("B")]));

        // Verify B has A as primary
        let (primary, secondaries) = get_cluster_info(&ctx, &blobstore, &manifest, "B").await?;
        assert_eq!(primary, Some(mpath("A")));
        assert!(secondaries.is_none());

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_build_manifest_subtree_copy_multiple(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = make_blobstore();

        // Simulate: Copy A → B and Copy A → C in same commit
        let mut changes = HashMap::new();
        changes.insert(mpath("A"), cluster_update(None, vec!["B", "C"], vec![]));
        changes.insert(mpath("B"), cluster_update(Some("A"), vec![], vec![]));
        changes.insert(mpath("C"), cluster_update(Some("A"), vec![], vec![]));

        let manifest = build_manifest(&ctx, &blobstore, vec![], changes).await?;

        // Verify A has B and C as secondaries
        let (_, secondaries) = get_cluster_info(&ctx, &blobstore, &manifest, "A").await?;
        let secs = secondaries.unwrap();
        assert_eq!(secs.len(), 2);
        assert!(secs.contains(&mpath("B")));
        assert!(secs.contains(&mpath("C")));

        // Verify B has A as primary
        let (primary, _) = get_cluster_info(&ctx, &blobstore, &manifest, "B").await?;
        assert_eq!(primary, Some(mpath("A")));

        // Verify C has A as primary
        let (primary, _) = get_cluster_info(&ctx, &blobstore, &manifest, "C").await?;
        assert_eq!(primary, Some(mpath("A")));

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_build_manifest_copy_chained(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = make_blobstore();

        // Simulate: Copy A → B and Copy B → C in same commit
        // Should result in a single cluster with A as primary
        // A.secondaries = [B, C]
        // B.primary = A
        // C.primary = A
        let mut changes = HashMap::new();
        changes.insert(mpath("A"), cluster_update(None, vec!["B", "C"], vec![]));
        changes.insert(mpath("B"), cluster_update(Some("A"), vec![], vec![]));
        changes.insert(mpath("C"), cluster_update(Some("A"), vec![], vec![]));

        let manifest = build_manifest(&ctx, &blobstore, vec![], changes).await?;

        // Verify A has both B and C as secondaries
        let (primary, secondaries) = get_cluster_info(&ctx, &blobstore, &manifest, "A").await?;
        assert!(primary.is_none());
        let secs = secondaries.unwrap();
        assert_eq!(secs.len(), 2);
        assert!(secs.contains(&mpath("B")));
        assert!(secs.contains(&mpath("C")));

        // Verify B has A as primary (not a secondary of anyone)
        let (primary, secondaries) = get_cluster_info(&ctx, &blobstore, &manifest, "B").await?;
        assert_eq!(primary, Some(mpath("A")));
        assert!(secondaries.is_none());

        // Verify C has A as primary (not B)
        let (primary, secondaries) = get_cluster_info(&ctx, &blobstore, &manifest, "C").await?;
        assert_eq!(primary, Some(mpath("A")));
        assert!(secondaries.is_none());

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_build_manifest_full_repo_merge(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = make_blobstore();

        // Parent 1: A has cluster {A, B}
        let mut changes1 = HashMap::new();
        changes1.insert(mpath("A"), cluster_update(None, vec!["B"], vec![]));
        changes1.insert(mpath("B"), cluster_update(Some("A"), vec![], vec![]));
        let parent1 = build_manifest(&ctx, &blobstore, vec![], changes1).await?;

        // Parent 2: C has cluster {C, D}
        let mut changes2 = HashMap::new();
        changes2.insert(mpath("C"), cluster_update(None, vec!["D"], vec![]));
        changes2.insert(mpath("D"), cluster_update(Some("C"), vec![], vec![]));
        let parent2 = build_manifest(&ctx, &blobstore, vec![], changes2).await?;

        // Merge commit: no new changes, just merge the parents
        let manifest =
            build_manifest(&ctx, &blobstore, vec![parent1, parent2], HashMap::new()).await?;

        // Verify A's cluster info from parent1
        let (primary, secondaries) = get_cluster_info(&ctx, &blobstore, &manifest, "A").await?;
        assert!(primary.is_none());
        assert_eq!(secondaries, Some(vec![mpath("B")]));

        // Verify B's cluster info from parent1
        let (primary, _) = get_cluster_info(&ctx, &blobstore, &manifest, "B").await?;
        assert_eq!(primary, Some(mpath("A")));

        // Verify C's cluster info from parent2
        let (primary, secondaries) = get_cluster_info(&ctx, &blobstore, &manifest, "C").await?;
        assert!(primary.is_none());
        assert_eq!(secondaries, Some(vec![mpath("D")]));

        // Verify D's cluster info from parent2
        let (primary, _) = get_cluster_info(&ctx, &blobstore, &manifest, "D").await?;
        assert_eq!(primary, Some(mpath("C")));

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_build_manifest_full_repo_merge_secondaries(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = make_blobstore();

        // Parent 1: A has cluster {A, B}
        let mut changes1 = HashMap::new();
        changes1.insert(mpath("A"), cluster_update(None, vec!["B"], vec![]));
        let parent1 = build_manifest(&ctx, &blobstore, vec![], changes1).await?;

        // Parent 2: A has cluster {A, C}
        let mut changes2 = HashMap::new();
        changes2.insert(mpath("A"), cluster_update(None, vec!["C"], vec![]));
        let parent2 = build_manifest(&ctx, &blobstore, vec![], changes2).await?;

        // Merge commit: A should have union of secondaries {B, C}
        let manifest =
            build_manifest(&ctx, &blobstore, vec![parent1, parent2], HashMap::new()).await?;

        // Verify A has both B and C as secondaries (union)
        let (_, secondaries) = get_cluster_info(&ctx, &blobstore, &manifest, "A").await?;
        let secs = secondaries.unwrap();
        assert_eq!(secs.len(), 2);
        assert!(secs.contains(&mpath("B")));
        assert!(secs.contains(&mpath("C")));

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_build_manifest_full_repo_merge_primaries(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = make_blobstore();

        // Parent 1: A is primary of {A, B}
        let mut changes1 = HashMap::new();
        changes1.insert(mpath("A"), cluster_update(None, vec!["B"], vec![]));
        changes1.insert(mpath("B"), cluster_update(Some("A"), vec![], vec![]));
        let parent1 = build_manifest(&ctx, &blobstore, vec![], changes1).await?;

        // Parent 2: A is also primary of {A, C} (same path as primary on both)
        let mut changes2 = HashMap::new();
        changes2.insert(mpath("A"), cluster_update(None, vec!["C"], vec![]));
        changes2.insert(mpath("C"), cluster_update(Some("A"), vec![], vec![]));
        let parent2 = build_manifest(&ctx, &blobstore, vec![], changes2).await?;

        // Merge: A should have merged clusters {B, C}
        let manifest =
            build_manifest(&ctx, &blobstore, vec![parent1, parent2], HashMap::new()).await?;

        // Verify A has union of secondaries
        let (primary, secondaries) = get_cluster_info(&ctx, &blobstore, &manifest, "A").await?;
        assert!(primary.is_none()); // A is primary, not a secondary
        let secs = secondaries.unwrap();
        assert_eq!(secs.len(), 2);
        assert!(secs.contains(&mpath("B")));
        assert!(secs.contains(&mpath("C")));

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_build_manifest_secondary_deleted(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = make_blobstore();

        // First: A→B creates cluster {A, B}
        let mut changes1 = HashMap::new();
        changes1.insert(mpath("A"), cluster_update(None, vec!["B"], vec![]));
        changes1.insert(mpath("B"), cluster_update(Some("A"), vec![], vec![]));
        let parent = build_manifest(&ctx, &blobstore, vec![], changes1).await?;

        // B is deleted: A should remove B from secondaries
        let mut changes2 = HashMap::new();
        changes2.insert(mpath("A"), cluster_update(None, vec![], vec!["B"]));
        let manifest = build_manifest(&ctx, &blobstore, vec![parent], changes2).await?;

        // A should have empty secondaries
        let (_, secondaries) = get_cluster_info(&ctx, &blobstore, &manifest, "A").await?;
        assert!(secondaries.is_none());

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_build_manifest_nested_path(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = make_blobstore();

        // Create cluster change for nested path "a/b/c"
        let mut changes = HashMap::new();
        changes.insert(
            mpath("a/b/c"),
            cluster_update(Some("x/y/z"), vec![], vec![]),
        );
        changes.insert(mpath("x/y/z"), cluster_update(None, vec!["a/b/c"], vec![]));

        let manifest = build_manifest(&ctx, &blobstore, vec![], changes).await?;

        // Verify the cluster info is at the correct nested location
        let (primary, _) = get_cluster_info(&ctx, &blobstore, &manifest, "a/b/c").await?;
        assert_eq!(primary, Some(mpath("x/y/z")));

        let (_, secondaries) = get_cluster_info(&ctx, &blobstore, &manifest, "x/y/z").await?;
        assert_eq!(secondaries, Some(vec![mpath("a/b/c")]));

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_build_manifest_nested_with_parent(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = make_blobstore();

        // Create parent with nested path having cluster info
        let mut parent_changes = HashMap::new();
        parent_changes.insert(
            mpath("existing/nested"),
            cluster_update(Some("src/path"), vec![], vec![]),
        );
        let parent = build_manifest(&ctx, &blobstore, vec![], parent_changes).await?;

        // Create child with another nested path
        let mut child_changes = HashMap::new();
        child_changes.insert(
            mpath("new/nested/deep"),
            cluster_update(Some("other/path"), vec![], vec![]),
        );
        let manifest = build_manifest(&ctx, &blobstore, vec![parent], child_changes).await?;

        // Verify both nested paths exist with correct info
        let (primary, _) = get_cluster_info(&ctx, &blobstore, &manifest, "existing/nested").await?;
        assert_eq!(primary, Some(mpath("src/path")));

        let (primary, _) = get_cluster_info(&ctx, &blobstore, &manifest, "new/nested/deep").await?;
        assert_eq!(primary, Some(mpath("other/path")));

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_build_manifest_mixed_depths(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = make_blobstore();

        // Mix of root and nested paths in the same manifest
        let mut changes = HashMap::new();
        // Root-level path
        changes.insert(
            mpath("root_dir"),
            cluster_update(Some("src"), vec![], vec![]),
        );
        // First-level nested path
        changes.insert(
            mpath("a/nested"),
            cluster_update(Some("b/nested"), vec![], vec![]),
        );
        // Deep nested path
        changes.insert(
            mpath("x/y/z/deep"),
            cluster_update(None, vec!["p/q/r/deep"], vec![]),
        );

        let manifest = build_manifest(&ctx, &blobstore, vec![], changes).await?;

        // Verify all paths have correct cluster info at their respective depths
        let (primary, _) = get_cluster_info(&ctx, &blobstore, &manifest, "root_dir").await?;
        assert_eq!(primary, Some(mpath("src")));

        let (primary, _) = get_cluster_info(&ctx, &blobstore, &manifest, "a/nested").await?;
        assert_eq!(primary, Some(mpath("b/nested")));

        let (_, secondaries) = get_cluster_info(&ctx, &blobstore, &manifest, "x/y/z/deep").await?;
        assert_eq!(secondaries, Some(vec![mpath("p/q/r/deep")]));

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_build_manifest_nested_parent_merge(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = make_blobstore();

        // Parent 1: has cluster info at a/b with secondaries [c/d]
        let mut changes1 = HashMap::new();
        changes1.insert(mpath("a/b"), cluster_update(None, vec!["c/d"], vec![]));
        let parent1 = build_manifest(&ctx, &blobstore, vec![], changes1).await?;

        // Parent 2: has cluster info at a/b with secondaries [e/f]
        let mut changes2 = HashMap::new();
        changes2.insert(mpath("a/b"), cluster_update(None, vec!["e/f"], vec![]));
        let parent2 = build_manifest(&ctx, &blobstore, vec![], changes2).await?;

        // Merge both parents - should have union of secondaries
        let manifest =
            build_manifest(&ctx, &blobstore, vec![parent1, parent2], HashMap::new()).await?;

        // Verify a/b has both c/d and e/f as secondaries (union)
        let (_, secondaries) = get_cluster_info(&ctx, &blobstore, &manifest, "a/b").await?;
        let secs = secondaries.unwrap();
        assert_eq!(secs.len(), 2);
        assert!(secs.contains(&mpath("c/d")));
        assert!(secs.contains(&mpath("e/f")));

        Ok(())
    }
}
