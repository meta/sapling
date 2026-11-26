/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Rebuild the Hg trees of commits from their augmented manifests, for commits
//! whose Hg trees were never written or have been deleted.

use std::collections::HashMap;
use std::iter;

use anyhow::Context;
use anyhow::Result;
use blobstore::BlobstoreBytes;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use bonsai_hg_mapping::BonsaiHgMapping;
use cloned::cloned;
use context::CoreContext;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future;
use futures::stream;
use futures::stream::BoxStream;
use manifest::ManifestOps;
use manifest::find_intersection_of_diffs;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgChangesetId;
use mercurial_types::HgManifestId;
use mercurial_types::NULL_HASH;
use mercurial_types::blobs::MononokeHgBlobError;
use mercurial_types::sharded_augmented_manifest::rebuild_hg_manifest_envelope;
use mononoke_types::ChangesetId;
use mononoke_types::MPath;
use mononoke_types::RepoPath;
use restricted_paths_common::ArcRestrictedPathsConfigBased;

use crate::access_labels;
use crate::derive_hg_manifest::restricted_hg_tree_entry;

const MAX_CONCURRENT_TREES: usize = 100;
const MAX_CONCURRENT_CHANGESET_LOADS: usize = 10;

/// The root Hg trees of a commit and of its parents, taken from their Hg
/// changesets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitHgRoots {
    pub root: HgManifestId,
    /// One per Bonsai parent, in order.
    pub parents: Vec<HgManifestId>,
}

/// Which of a commit's trees to rebuild.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeScope {
    /// The trees that differ from the tree at the same path in every parent,
    /// as Modern Sync sends them. The others are the parents' trees.
    NewInCommit,
    /// Every tree of the commit.
    Whole,
}

/// What to write for the rebuilt trees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebuildAction {
    /// Write nothing.
    Compare,
    /// Store each missing tree under its node id, and record the restriction
    /// of each consistent tree at a writable restriction root. While
    /// restricted-path recording is switched off, every such tree except a
    /// `Different` one, which already fails the command, is reported as
    /// `RestrictionNotRecorded`, and a missing one is not stored.
    StoreMissing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TreeOutcome {
    /// The stored Hg tree has the rebuilt bytes.
    Identical,
    /// The stored Hg tree has other bytes. It is not overwritten, but under
    /// `StoreMissing`, while recording is on, its restriction is still
    /// recorded, as the entry names the tree id and the path, not the bytes.
    Different,
    /// No Hg tree is stored, and nothing was written.
    Missing,
    /// No Hg tree was stored, and the rebuilt one now is.
    Stored,
    /// The rebuilt contents do not hash to the computed node id recorded in
    /// the augmented manifest, so the record is wrong. Nothing was written.
    Inconsistent,
    /// The tree is at a writable restriction root, and restricted-path
    /// recording is switched off, so its restriction could not be recorded. A
    /// missing tree was not stored. A tree whose stored bytes differ is
    /// reported as `Different` instead.
    RestrictionNotRecorded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeRebuild {
    pub path: MPath,
    pub tree: HgManifestId,
    pub outcome: TreeOutcome,
}

/// The root Hg trees of `cs_id` and its parents. Nothing is derived: the
/// commit and each parent must already have an Hg changeset.
pub async fn commit_hg_roots(
    ctx: &CoreContext,
    blobstore: &impl KeyedBlobstore,
    bonsai_hg_mapping: &dyn BonsaiHgMapping,
    cs_id: ChangesetId,
) -> Result<CommitHgRoots> {
    let bonsai = cs_id
        .load(ctx, blobstore)
        .await
        .with_context(|| format!("loading commit {cs_id}"))?;
    let commits: Vec<ChangesetId> = iter::once(cs_id).chain(bonsai.parents()).collect();
    let hg_ids: HashMap<ChangesetId, HgChangesetId> = bonsai_hg_mapping
        .get(ctx, commits.clone().into())
        .await?
        .into_iter()
        .map(|entry| (entry.bcs_id, entry.hg_cs_id))
        .collect();
    let mut roots: Vec<HgManifestId> = stream::iter(commits)
        .map(|commit| {
            let hg_id = hg_ids.get(&commit).copied();
            async move {
                let hg_id =
                    hg_id.with_context(|| format!("commit {commit} has no Hg changeset"))?;
                let hg_cs = hg_id
                    .load(ctx, blobstore)
                    .await
                    .with_context(|| format!("loading Hg changeset {hg_id} of commit {commit}"))?;
                anyhow::Ok(hg_cs.manifestid())
            }
        })
        .buffered(MAX_CONCURRENT_CHANGESET_LOADS)
        .try_collect()
        .await?;
    let root = roots.remove(0);
    Ok(CommitHgRoots {
        root,
        parents: roots,
    })
}

/// Rebuilds Hg trees from the augmented manifests stored under the same ids.
#[derive(Clone)]
pub struct HgTreeRebuilder<B> {
    blobstore: B,
    restricted_paths: ArcRestrictedPathsConfigBased,
}

impl<B: KeyedBlobstore + Clone + 'static> HgTreeRebuilder<B> {
    pub fn new(blobstore: B, restricted_paths: ArcRestrictedPathsConfigBased) -> Self {
        Self {
            blobstore,
            restricted_paths,
        }
    }

    /// Rebuild the trees of `roots` within `scope`, and compare each with the
    /// stored Hg tree. A tree with no augmented manifest fails the stream.
    pub fn rebuild(
        &self,
        ctx: &CoreContext,
        roots: CommitHgRoots,
        scope: TreeScope,
        action: RebuildAction,
    ) -> BoxStream<'static, Result<TreeRebuild>> {
        let ctx = ctx.with_access_label(access_labels::HG_MANIFEST_REBUILD);
        let this = self.clone();
        list_trees(&ctx, self.blobstore.clone(), roots, scope)
            .map_ok(move |(path, tree)| {
                cloned!(ctx, this);
                async move {
                    let outcome = this
                        .rebuild_tree(&ctx, &path, tree, action)
                        .await
                        .with_context(|| format!("rebuilding Hg tree {tree} at {path}"))?;
                    Ok(TreeRebuild {
                        path,
                        tree,
                        outcome,
                    })
                }
            })
            .try_buffer_unordered(MAX_CONCURRENT_TREES)
            .boxed()
    }

    fn is_restricted(&self, path: &RepoPath) -> bool {
        match path {
            RepoPath::DirectoryPath(path) => {
                self.restricted_paths.should_record_manifest_id_entry(path)
            }
            _ => false,
        }
    }

    async fn rebuild_tree(
        &self,
        ctx: &CoreContext,
        path: &MPath,
        tree: HgManifestId,
        action: RebuildAction,
    ) -> Result<TreeOutcome> {
        let rebuilt = match rebuild_hg_manifest_envelope(ctx, &self.blobstore, tree).await {
            Ok(rebuilt) => rebuilt,
            Err(err) if is_inconsistent(&err) => return Ok(TreeOutcome::Inconsistent),
            Err(err) => return Err(err),
        };

        let key = tree.blobstore_key();
        let rebuilt = BlobstoreBytes::from(rebuilt.into_blob());
        let outcome = match self.blobstore.get(ctx, &key).await? {
            Some(stored) if stored.as_raw_bytes() == rebuilt.as_bytes() => TreeOutcome::Identical,
            Some(_) => TreeOutcome::Different,
            None => TreeOutcome::Missing,
        };
        if action == RebuildAction::Compare {
            return Ok(outcome);
        }

        // Recorded before storing, so that a restricted path never stores its
        // tree without recording its restriction, and for a stored tree too:
        // a tree stored through another path gets its entry here, on the rerun
        // the command's failure asks for. The entry names the tree id and the
        // path, not the bytes.
        let repo_path = match path.clone().into_optional_non_root_path() {
            None => RepoPath::RootPath,
            Some(path) => RepoPath::DirectoryPath(path),
        };
        match restricted_hg_tree_entry(&self.restricted_paths, tree, &repo_path)? {
            Some(entry) => {
                self.restricted_paths
                    .manifest_id_store()
                    .add_entry(ctx, entry)
                    .await
                    .with_context(|| format!("recording the restriction of {tree} at {path}"))?;
            }
            // Recording is switched off, so a missing restricted tree is left
            // missing, and a stored one, which may have been stored at another
            // path, is reported, both for a rerun once it is back on.
            None if outcome != TreeOutcome::Different && self.is_restricted(&repo_path) => {
                return Ok(TreeOutcome::RestrictionNotRecorded);
            }
            None => {}
        }
        if outcome != TreeOutcome::Missing {
            return Ok(outcome);
        }
        self.blobstore.put(ctx, key, rebuilt).await?;
        Ok(TreeOutcome::Stored)
    }
}

fn is_inconsistent(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<MononokeHgBlobError>(),
            Some(MononokeHgBlobError::InconsistentComputedEntryHash(..))
        )
    })
}

/// The trees of `roots` within `scope`, listed from augmented manifests.
fn list_trees<B: KeyedBlobstore + Clone + 'static>(
    ctx: &CoreContext,
    blobstore: B,
    roots: CommitHgRoots,
    scope: TreeScope,
) -> BoxStream<'static, Result<(MPath, HgManifestId)>> {
    let is_null = |id: &HgManifestId| id.into_nodehash() == NULL_HASH;
    if is_null(&roots.root) {
        return stream::empty().boxed();
    }
    let root = HgAugmentedManifestId::from(roots.root);
    let trees = match scope {
        TreeScope::Whole => root.list_tree_entries(ctx.clone(), blobstore),
        TreeScope::NewInCommit => {
            // A null parent is an empty tree, which every tree differs from.
            let parents = roots
                .parents
                .into_iter()
                .filter(|id| !is_null(id))
                .map(HgAugmentedManifestId::from)
                .collect();
            find_intersection_of_diffs(ctx.clone(), blobstore, root, parents)
                .try_filter_map(|(path, entry)| {
                    future::ok(entry.into_tree().map(|tree| (path, tree)))
                })
                .boxed()
        }
    };
    trees
        .map_ok(|(path, tree)| (path, HgManifestId::new(tree.into_nodehash())))
        .boxed()
}
