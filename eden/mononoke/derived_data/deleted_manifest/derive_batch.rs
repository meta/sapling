/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use borrowed::borrowed;
use bounded_traversal::bounded_traversal;
use context::CoreContext;
use futures::future::FutureExt;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use itertools::Itertools;
use manifest::PathTree;
use mononoke_types::BlobstoreKey;
use mononoke_types::BlobstoreValue;
use mononoke_types::ChangesetId;
use mononoke_types::DeletedManifestV2Id;
use mononoke_types::MPathElement;
use mononoke_types::NonRootMPath;
use mononoke_types::deleted_manifest_common::DeletedManifestCommon;
use mononoke_types::deleted_manifest_v2::DeletedManifestV2;

use crate::derive::DeletedManifestChangeType;
use crate::derive::DeletedManifestDeriver;
use crate::derive::PathChange;

struct UnfoldNode {
    path_element: Option<MPathElement>,
    all_changes: PathTree<Vec<(ChangesetId, PathChange)>>,
    parent: Option<DeletedManifestV2Id>,
}

struct FoldNode {
    change_by_cs: ByChangeset<DeletedManifestChangeType>,
    parent: Option<DeletedManifestV2>,
    base_name: Option<MPathElement>,
}

struct FoldOutput(
    Option<MPathElement>,
    ByChangeset<Option<DeletedManifestV2Id>>,
);

struct ByChangeset<V>(HashMap<ChangesetId, V>);

impl DeletedManifestDeriver {
    /// Derives DM for a simple stack of commits
    /// The idea is similar to manifest::derive_manifests_for_simple_stack_of_commits (https://fburl.com/code/5fy838h2)
    /// A simple stack of commits is a stack of consecutive commits that does not include a merge commit.
    // Explanation: We can't really parallelise DM derivation over commits, as each change creates a new
    // node, and it depends on the previous node. However, we can parallelise the derivation of the nodes
    // themselves. That is, if a node (for a particular file) was only edited on commit A on the stack,
    // and another node was modified only on commit B, we can derive those two new nodes in parallel.
    // The root will of course be edited always, and the parallelisation won't help there, and in general
    // this optimisation does not improve the worst case (all commits modify the same files), but it
    // optimises the usual case (commits touch mostly different files) quite well.
    pub(crate) async fn derive_simple_stack(
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        parent: Option<DeletedManifestV2Id>,
        all_changes: Vec<(ChangesetId, Vec<(NonRootMPath, PathChange)>)>,
    ) -> Result<Vec<DeletedManifestV2Id>> {
        let (path_tree, commit_stack) = {
            let mut path_tree: PathTree<Vec<(ChangesetId, PathChange)>> = PathTree::default();
            let mut commit_stack: Vec<ChangesetId> = vec![];
            for (csid, cs_changes) in all_changes {
                for (path, change) in cs_changes {
                    path_tree.insert_and_merge(path.into(), (csid, change));
                }
                commit_stack.push(csid);
            }
            (path_tree, commit_stack)
        };

        let FoldOutput(name, mfid_by_cs) = bounded_traversal(
            256,
            UnfoldNode {
                path_element: None,
                all_changes: path_tree,
                parent,
            },
            |unfold_node| Self::unfold_batch(ctx, blobstore, unfold_node).boxed(),
            |fold_node, subentries| {
                Self::fold_batch(ctx, blobstore, &commit_stack, fold_node, subentries).boxed()
            },
        )
        .await?;
        assert!(name.is_none());
        Self::finalize_stack(ctx, blobstore, commit_stack, parent, mfid_by_cs).await
    }

    /// Given a stack of commits and a manifest ids for some of those, create
    /// manifests ids for ALL of them, by reusing from previous nodes in the stack.
    /// If a commit doesn't have an id, the manifest is empty and an empty root
    /// should be created for it.
    async fn finalize_stack(
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        commit_stack: Vec<ChangesetId>,
        parent_id: Option<DeletedManifestV2Id>,
        mfid_by_cs: ByChangeset<Option<DeletedManifestV2Id>>,
    ) -> Result<Vec<DeletedManifestV2Id>> {
        let mut mfids: Vec<DeletedManifestV2Id> = Vec::with_capacity(commit_stack.len());
        let mut cur_mfid = parent_id;
        // This is a "cache", so we only create the empty manifest once, if necessary
        let mut cached_empty_mf_id = None;
        for csid in commit_stack {
            if let Some(maybe_mfid) = mfid_by_cs.0.get(&csid) {
                cur_mfid = *maybe_mfid;
            }
            let mfid = if let Some(mfid) = cur_mfid.clone() {
                mfid
            } else {
                // Empty dm, we need to create an empty root
                // if it's not cached already
                if let Some(mfid) = cached_empty_mf_id {
                    mfid
                } else {
                    let mf = DeletedManifestV2::copy_and_update_subentries(
                        ctx,
                        blobstore,
                        None,
                        None,
                        BTreeMap::new(),
                    )
                    .await?;
                    let mfid = Self::save_mf(mf, ctx, blobstore).await?;
                    cached_empty_mf_id = Some(mfid);
                    mfid
                }
            };
            mfids.push(mfid);
        }
        Ok(mfids)
    }

    async fn unfold_batch(
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        unfold_node: UnfoldNode,
    ) -> Result<(FoldNode, impl IntoIterator<Item = UnfoldNode> + use<>)> {
        let UnfoldNode {
            path_element,
            all_changes,
            parent,
        } = unfold_node;
        let (change_by_cs, subentries) = all_changes.deconstruct();

        let parent = match parent {
            None => None,
            Some(p) => Some(p.load(ctx, blobstore).await?),
        };

        let change_by_cs = ByChangeset(
            change_by_cs
                .into_iter()
                .map(|(cs_id, change)| {
                    (
                        cs_id,
                        match change {
                            PathChange::Add | PathChange::FileDirConflict => {
                                DeletedManifestChangeType::RemoveIfNowEmpty
                            }
                            PathChange::Remove => DeletedManifestChangeType::CreateDeleted,
                        },
                    )
                })
                .collect(),
        );

        let children = {
            borrowed!(parent);
            stream::iter(
                subentries
                    .into_iter()
                    .map({
                        |(path, path_tree)| async move {
                            let parent = match parent {
                                None => None,
                                Some(p) => p.lookup(ctx, blobstore, &path).await?,
                            };
                            anyhow::Ok(UnfoldNode {
                                path_element: Some(path),
                                all_changes: path_tree,
                                parent,
                            })
                        }
                    })
                    .collect::<Vec<_>>(),
            )
            .buffer_unordered(100)
            .try_collect::<Vec<_>>()
            .await?
        };

        let fold_node = FoldNode {
            change_by_cs,
            parent,
            base_name: path_element,
        };

        Ok((fold_node, children))
    }

    /// Given a stack of commits and changes applied to some of those commits, create
    /// the manifests for the commits with modifications.
    async fn apply_changes_to_stack_and_create_mfs(
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        commit_stack: &[ChangesetId],
        mut parent: Option<DeletedManifestV2>,
        mut modified_subentries_by_cs: ByChangeset<
            BTreeMap<MPathElement, Option<DeletedManifestV2Id>>,
        >,
        mut change_by_cs: ByChangeset<DeletedManifestChangeType>,
    ) -> Result<ByChangeset<Option<DeletedManifestV2Id>>> {
        let mut mfid_by_cs: ByChangeset<Option<DeletedManifestV2Id>> = ByChangeset(HashMap::new());
        // TODO: We can optimise here even further by only iterating over the commits that are
        // keys in either modified_subentries_by_cs or change_by_cs, but we need to do so in
        // the correct order. This optimisation likely doesn't help much as iterating over the
        // whole array likely doesn't add much overhead.
        for csid in commit_stack {
            let modified_subentries = modified_subentries_by_cs.0.remove(csid);
            let change = change_by_cs.0.remove(csid);
            if change.is_none() && modified_subentries.is_none() {
                // No changes since last commit, continuing here just reuses the
                // same node from the previous commit
                continue;
            }
            let cur_subentries_to_update = modified_subentries.unwrap_or_default();
            let mf = match change.unwrap_or(DeletedManifestChangeType::RemoveIfNowEmpty) {
                DeletedManifestChangeType::CreateDeleted => {
                    let mf = DeletedManifestV2::copy_and_update_subentries(
                        ctx,
                        blobstore,
                        parent,
                        Some(*csid),
                        cur_subentries_to_update.clone(),
                    )
                    .await?;
                    Self::save_mf(mf.clone(), ctx, blobstore).await?;
                    Some(mf)
                }
                DeletedManifestChangeType::RemoveIfNowEmpty => {
                    let mf = DeletedManifestV2::copy_and_update_subentries(
                        ctx,
                        blobstore,
                        parent,
                        None,
                        cur_subentries_to_update.clone(),
                    )
                    .await?;
                    if mf.is_empty() {
                        None
                    } else {
                        Self::save_mf(mf.clone(), ctx, blobstore).await?;
                        Some(mf)
                    }
                }
            };
            let previous = mfid_by_cs.0.insert(*csid, mf.as_ref().map(|m| m.id()));
            assert!(previous.is_none());
            parent = mf;
        }
        assert!(change_by_cs.0.is_empty());
        assert!(modified_subentries_by_cs.0.is_empty());
        Ok(mfid_by_cs)
    }

    async fn fold_batch(
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        commit_stack: &[ChangesetId],
        fold_node: FoldNode,
        subentries: impl Iterator<Item = FoldOutput>,
    ) -> Result<FoldOutput> {
        let FoldNode {
            change_by_cs,
            parent,
            base_name,
        } = fold_node;
        let modified_subentries_by_cs = ByChangeset(
            subentries
                .filter_map(|FoldOutput(subname, mfid_by_cs)| {
                    Some(std::iter::repeat(subname?).zip(mfid_by_cs.0))
                })
                .flatten()
                .map(|(sub_name, (csid, maybe_mfid))| (csid, (sub_name, maybe_mfid)))
                .into_grouping_map()
                .collect(),
        );

        let mfid_by_cs = Self::apply_changes_to_stack_and_create_mfs(
            ctx,
            blobstore,
            commit_stack,
            parent,
            modified_subentries_by_cs,
            change_by_cs,
        )
        .await?;
        Ok(FoldOutput(base_name, mfid_by_cs))
    }

    pub(super) async fn save_mf(
        mf: DeletedManifestV2,
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
    ) -> Result<DeletedManifestV2Id> {
        let mf_id = mf.id();
        let key = mf_id.blobstore_key();
        blobstore.put(ctx, key, mf.into_blob().into()).await?;
        Ok(mf_id)
    }
}
