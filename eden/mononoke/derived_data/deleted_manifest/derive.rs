/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Error;
use anyhow::anyhow;
use atomic_counter::AtomicCounter;
use atomic_counter::RelaxedCounter;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use borrowed::borrowed;
use bounded_traversal::bounded_traversal_forest;
use bytes::Bytes;
use cloned::cloned;
use context::CoreContext;
use derived_data::batch::FileConflicts;
use derived_data::batch::split_bonsais_in_linear_stacks;
use derived_data_manager::DerivationContext;
use futures::channel::mpsc;
use futures::future;
use futures::future::BoxFuture;
use futures::future::FutureExt;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use manifest::Diff;
use manifest::ManifestOps;
use manifest::PathTree;
use mononoke_macros::mononoke;
use mononoke_types::BlobstoreKey;
use mononoke_types::BlobstoreValue;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::DeletedManifestV2Id;
use mononoke_types::MPathElement;
use mononoke_types::ManifestUnodeId;
use mononoke_types::NonRootMPath;
use mononoke_types::deleted_manifest_common::DeletedManifestCommon;
use mononoke_types::deleted_manifest_v2::DeletedManifestV2;
use mononoke_types::path::MPath;
use mononoke_types::sharded_map::ShardedMapNode;
use mononoke_types::unode::UnodeEntry;
use multimap::MultiMap;
use sorted_vector_map::SortedVectorMap;
use tokio::sync::Mutex;
use tracing::debug;
use unodes::RootUnodeManifestId;

use crate::mapping::RootDeletedManifestIdCommon;

/// Derives deleted manifest for bonsai changeset `cs_id` given parent deleted files
/// manifests, unodes and the current changeset. Parent deleted manifests should be
/// constructed for each parent of the given changeset.
///
/// Deleted manifest is a recursive data structure that starts with a root manifest and
/// points to the other manifests. Each node may represent either deleted directory or deleted file.
/// Both directory's and file's manifest can have subentries, if a file has subentries it means
/// that this path was a directory earlier, then was deleted and reincarnated as a file.
///
/// Each manifest has an optional linknode. The initialized linknode points to the changeset where
/// the path was deleted. If linknode is not set, then manifest represents an existing
/// directory where some of the subentries (directories or files) have been deleted. There cannot
/// be a manifest without linknode and with no subentries.
///
/// The derivation is done as a bounded traversal that traverses (in lockstep)
/// any changes included in bonsai's file_changes. It also keeps track of traversed
/// paths parents deleted manifest and unodes and based on their contents it may
/// recurse into more entries.
///
/// We attempt to avoid traversing entire manifest so we don't descend into
/// subtree if the manifest can be reused.
///
/// 1. If the path exists in current unode - we may need to delete the node from deleted manifest.
/// 2. If the path doesn't exist in the current commit, and we have a single
///    deleted parent manifest for it we can safely reuse it.
/// 3. If the path doesn't exist but existed in any of the parents we need to
///    create a new deleted manifest node.
/// 4. If there are many different parent manifests and path doesn't exist in
///    current commit we need to:
///    - create a new deleted manifest node
///    - recurse into the deleted manifests trees to merge them
///
/// We also recurse into additional paths in the following scenarios:
///
/// 1. When one of the parents unodes has entry for path that other parent has
///    deleted manifest for. In this case the other parent is resurrecting that
///    entry and we have to accommodate for that.
///
/// 2. When the path is a directory in one of the parents but is absent or a file
///    in the current commit. In this case we descend into the removed directory.
pub(crate) struct DeletedManifestDeriver;

#[derive(Default, Debug)]
struct DebugCounters {
    bonsai: RelaxedCounter,
    file_dir: RelaxedCounter,
    merge_intersect: RelaxedCounter,
    dfm_merge: RelaxedCounter,
}

#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub(crate) enum PathChange {
    Add,
    Remove,
    FileDirConflict,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum DeletedManifestChangeType {
    /// Path was deleted, we create a node if not present.
    CreateDeleted,
    /// Path now exists, delete if it doesn't have any subentries that were
    /// previous deleted.
    RemoveIfNowEmpty,
}

/// Per-node outcome produced by `do_unfold` and consumed by `do_create`.
/// The child maps carry the reusable parent entries before applying updates.
#[derive(Debug, Clone)]
enum DeletedManifestChange {
    /// Child-stage boundary: emit this precomputed id (or `None`) directly,
    /// short-circuiting the traversal. Only produced by staged derivation.
    Known(Option<DeletedManifestV2Id>),
    /// No change on a single-parent path: reuse the parent node wholesale.
    Reuse(DeletedManifestV2),
    /// Path was deleted: create a node with its linknode set.
    CreateDeleted(Option<DeletedManifestSubentries>),
    /// Path now exists: create a node without a linknode, dropped if it ends up
    /// with no previously-deleted subentries.
    RemoveIfNowEmpty(Option<DeletedManifestSubentries>),
}

#[derive(Debug, Clone)]
struct DeletedManifestUnfoldNode {
    path_element: Option<MPathElement>,
    changes: PathTree<()>,
    parent_deleted_manifests: MultiMap<DeletedManifestV2Id, ChangesetId>,
    parent_unodes: MultiMap<UnodeEntry, ChangesetId>,
    current_unode: Option<UnodeEntry>,
    known_entries: PathTree<Option<KnownDeletedManifest>>,
}

#[derive(Debug, Clone)]
pub(crate) enum KnownDeletedManifest {
    Entry(Option<DeletedManifestV2Id>),
    /// Complete children; only the containing entry still needs derivation.
    Children(DeletedManifestSubentries),
}

pub(crate) type DeletedManifestSubentries = ShardedMapNode<DeletedManifestV2Id>;
pub(crate) type UnodeChildren = SortedVectorMap<MPathElement, UnodeEntry>;

pub(crate) struct DeletedManifestChildren {
    pub parents: Vec<(DeletedManifestSubentries, Vec<ChangesetId>)>,
    pub parent_unodes: Vec<(UnodeChildren, Vec<ChangesetId>)>,
    pub current_unode: Option<UnodeChildren>,
}

impl DeletedManifestDeriver {
    /// Derives a Deleted Manifest for a single commit (canonical, whole-tree).
    /// Thin wrapper over `derive_subtree` seeded at the repo root with no
    /// child-stage cutoffs.
    pub(crate) async fn derive(
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        bonsai: BonsaiChangeset,
        parents: Vec<(ChangesetId, DeletedManifestV2Id, ManifestUnodeId)>,
        current_unode: ManifestUnodeId,
    ) -> Result<DeletedManifestV2Id, Error> {
        let changes: PathTree<()> = PathTree::from_iter(
            bonsai
                .file_changes()
                .map(|(path, _change)| (path.clone(), ())),
        );
        let cs_id = bonsai.get_changeset_id();
        let parent_deleted_manifests = parents
            .iter()
            .map(|(cs_id, parent_mf_id, _)| (*parent_mf_id, *cs_id))
            .collect();
        let parent_unodes = parents
            .iter()
            .map(|(cs_id, _, parent_unode_id)| (UnodeEntry::Directory(*parent_unode_id), *cs_id))
            .collect();
        Self::derive_subtree(
            ctx,
            blobstore,
            cs_id,
            changes,
            parent_deleted_manifests,
            Some(UnodeEntry::Directory(current_unode)),
            parent_unodes,
            HashMap::new(),
            true,
        )
        .await?
        .ok_or_else(|| anyhow!("derive_subtree returned None despite materialize_empty_root"))
    }

    /// Shared traversal core for canonical and staged (pipeline) derivation.
    ///
    /// The traversal is seeded directly at a subtree: for canonical derivation
    /// that subtree is the repo root; for pipeline derivation it is the stage
    /// path, with inputs (`changes`, parent DMs, current/parent unodes) already
    /// scoped to that subtree. `known_entries` (keyed relative to the subtree
    /// root) supply a precomputed entry or its complete children, cutting off
    /// recursion. They are empty for canonical derivation. When the subtree
    /// has no deletions the result is `None`, unless `materialize_empty_root` is
    /// set (canonical derivation and the terminal stage), which then creates an
    /// empty root manifest.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn derive_subtree(
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        cs_id: ChangesetId,
        changes: PathTree<()>,
        parent_deleted_manifests: MultiMap<DeletedManifestV2Id, ChangesetId>,
        current_unode: Option<UnodeEntry>,
        parent_unodes: MultiMap<UnodeEntry, ChangesetId>,
        known_entries: HashMap<MPath, KnownDeletedManifest>,
        materialize_empty_root: bool,
    ) -> Result<Option<DeletedManifestV2Id>, Error> {
        let nodes = vec![DeletedManifestUnfoldNode {
            path_element: None,
            changes,
            parent_deleted_manifests,
            current_unode,
            parent_unodes,
            known_entries: PathTree::from_iter(
                known_entries.into_iter().map(|(path, id)| (path, Some(id))),
            ),
        }];
        let (_, output) = Self::derive_nodes(ctx, blobstore, cs_id, nodes)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("missing deleted manifest root"))?;
        match output {
            None if materialize_empty_root => Self::save_mf(
                DeletedManifestV2::new(None, ShardedMapNode::default()),
                ctx,
                blobstore,
            )
            .await
            .map(Some),
            output => Ok(output),
        }
    }

    async fn derive_nodes(
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        cs_id: ChangesetId,
        nodes: Vec<DeletedManifestUnfoldNode>,
    ) -> Result<Vec<(Option<MPathElement>, Option<DeletedManifestV2Id>)>, Error> {
        // Stream is used to batch writes to blobstore
        let (sender, receiver) = mpsc::unbounded();
        let created = Arc::new(Mutex::new(HashSet::new()));
        cloned!(blobstore, ctx);
        let counters: Arc<DebugCounters> = Arc::new(Default::default());

        let f = async move {
            borrowed!(ctx, blobstore, counters);
            let outputs = bounded_traversal_forest(
                256,
                nodes,
                // unfold
                {
                    move |DeletedManifestUnfoldNode {
                              path_element,
                              changes,
                              parent_deleted_manifests,
                              parent_unodes,
                              current_unode,
                              known_entries,
                          }| {
                        // -> ((Option<MPathElement>, DeletedManifestChange), Vec<UnfoldNode>)
                        async move {
                            let (mf_change, next_states) = Self::do_unfold(
                                ctx,
                                blobstore,
                                changes,
                                parent_deleted_manifests,
                                parent_unodes,
                                current_unode,
                                known_entries,
                                counters,
                            )
                            .await?;
                            Ok(((path_element, mf_change), next_states))
                        }
                        .boxed()
                    }
                },
                // fold
                {
                    cloned!(sender, created);
                    move |
                        (path, manifest_change): (
                            Option<MPathElement>,
                            DeletedManifestChange,
                        ),
                        // impl Iterator<Out>
                        subentries_iter,
                        // -> Out = (Option<MPathElement>, Option<DeletedManifestV2Id>)
                        // (_, None) means a leaf node was deleted because the file was recreated.
                        // (None, _) means the path is empty and should only happen on the root.
                    | {
                        cloned!(sender, created, cs_id);
                        async move {
                            let mut subentries_to_update = BTreeMap::new();
                            for entry in subentries_iter {
                                match entry {
                                    (None, _) => {
                                        return Err(anyhow!(concat!(
                                            "Failed to create deleted manifest: ",
                                            "subentry must have a path"
                                        )));
                                    }
                                    (Some(path), maybe_mf_id) => {
                                        subentries_to_update.insert(path, maybe_mf_id);
                                    }
                                }
                            }

                            let maybe_mf_id = Self::do_create(
                                ctx,
                                blobstore,
                                cs_id.clone(),
                                manifest_change,
                                subentries_to_update,
                                sender.clone(),
                                created.clone(),
                            )
                            .await?;

                            Ok((path, maybe_mf_id))
                        }
                        .boxed()
                    }
                },
            )
            .await?;

            debug!("deleted manifest derivation perf counters {:?}", counters);
            Ok(outputs)
        };

        let handle = mononoke::spawn_task(f);

        receiver
            .buffered(1024)
            .try_for_each(|_| async { Ok(()) })
            .await?;
        handle.await?
    }

    #[allow(clippy::too_many_arguments)]
    async fn do_unfold(
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        changes: PathTree<()>,
        parents_dm_ids: MultiMap<DeletedManifestV2Id, ChangesetId>,
        parents_unode_ids: MultiMap<UnodeEntry, ChangesetId>,
        current_unode_id: Option<UnodeEntry>,
        mut known_entries: PathTree<Option<KnownDeletedManifest>>,
        counters: &Arc<DebugCounters>,
    ) -> Result<(DeletedManifestChange, Vec<DeletedManifestUnfoldNode>), Error> {
        let known_value = known_entries.value.take();

        // This node is itself a child-stage boundary: emit its precomputed id and
        // stop. The child stage already derived everything below here.
        if let Some(KnownDeletedManifest::Entry(known_id)) = known_value {
            return Ok((DeletedManifestChange::Known(known_id), vec![]));
        }

        // We're assuming that the commits have hanful of parents in (in most
        // cases <= 2) and iterating over all of them won't be a problem.
        // (which is the case for all our current repos)
        let parent_manifests: Vec<(DeletedManifestV2, Vec<ChangesetId>)> = future::try_join_all(
            parents_dm_ids
                .iter_all()
                .map(|(mf_id, parent_cs_ids)| async {
                    Ok::<(DeletedManifestV2, Vec<ChangesetId>), Error>((
                        mf_id.load(ctx, blobstore).await?,
                        parent_cs_ids.clone(),
                    ))
                }),
        )
        .await?;

        let change = Self::classify_node(
            &parent_manifests,
            current_unode_id.is_some(),
            !parents_unode_ids.is_empty(),
        );
        if matches!(
            change,
            DeletedManifestChange::Known(_) | DeletedManifestChange::Reuse(_)
        ) {
            return Ok((change, vec![]));
        }
        let (parent, children) = if let Some(KnownDeletedManifest::Children(children)) = known_value
        {
            (Some(children), vec![])
        } else {
            let parent_unodes = stream::iter(parents_unode_ids.iter_all())
                .map(Ok::<_, Error>)
                .try_filter_map(|(entry, csids)| async move {
                    match entry {
                        UnodeEntry::Directory(id) => Ok(Some((
                            id.load(ctx, blobstore).await?.into_subentries(),
                            csids.clone(),
                        ))),
                        _ => Ok(None),
                    }
                })
                .try_collect()
                .await?;
            let current_unode = match current_unode_id {
                Some(UnodeEntry::Directory(id)) => {
                    Some(id.load(ctx, blobstore).await?.into_subentries())
                }
                _ => None,
            };
            Self::unfold_children(
                ctx,
                blobstore,
                changes,
                DeletedManifestChildren {
                    parents: parent_manifests
                        .into_iter()
                        .map(|(mf, csids)| (mf.subentries, csids))
                        .collect(),
                    parent_unodes,
                    current_unode,
                },
                known_entries,
                counters,
            )
            .await?
        };
        let change = match change {
            DeletedManifestChange::CreateDeleted(_) => DeletedManifestChange::CreateDeleted(parent),
            DeletedManifestChange::RemoveIfNowEmpty(_) => {
                DeletedManifestChange::RemoveIfNowEmpty(parent)
            }
            _ => unreachable!("reuse returned above"),
        };
        Ok((change, children))
    }

    fn classify_node(
        parents: &[(DeletedManifestV2, Vec<ChangesetId>)],
        exists: bool,
        parent_exists: bool,
    ) -> DeletedManifestChange {
        if exists {
            DeletedManifestChange::RemoveIfNowEmpty(None)
        } else if !parent_exists && parents.is_empty() {
            DeletedManifestChange::Known(None)
        } else if !parent_exists && let [(parent, _)] = parents {
            DeletedManifestChange::Reuse(parent.clone())
        } else {
            DeletedManifestChange::CreateDeleted(None)
        }
    }

    async fn unfold_children(
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        mut changes: PathTree<()>,
        input: DeletedManifestChildren,
        known_entries: PathTree<Option<KnownDeletedManifest>>,
        counters: &Arc<DebugCounters>,
    ) -> Result<
        (
            Option<DeletedManifestSubentries>,
            Vec<DeletedManifestUnfoldNode>,
        ),
        Error,
    > {
        let DeletedManifestChildren {
            parents: parent_manifests,
            parent_unodes: parent_tree_unodes,
            current_unode,
        } = input;
        let (_, known_subentries) = known_entries.deconstruct();
        // Directory replacement can delete descendants absent from Bonsai's changes.
        if current_unode.is_none() {
            for (parent_unode, _) in &parent_tree_unodes {
                for (name, _) in parent_unode {
                    counters.file_dir.inc();
                    changes.insert(MPath::from(vec![name.clone()]), ());
                }
            }
        }
        let (_, subentries) = changes.deconstruct();

        // Base traversal for all entries included in `changes` arg
        let mut recurse_entries = subentries
            .into_iter()
            .map(|(path, change_tree)| {
                counters.bonsai.inc();
                (
                    path.clone(),
                    DeletedManifestUnfoldNode {
                        path_element: Some(path),
                        changes: change_tree,
                        parent_deleted_manifests: MultiMap::new(),
                        parent_unodes: MultiMap::new(),
                        current_unode: None,
                        known_entries: PathTree::default(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();

        // Distribute child-stage boundaries down to the children they belong to.
        // Each child carries its slice of the boundary tree and self-identifies
        // when the recursion reaches it (see the short-circuit at the top). A
        // boundary child with no other reason to exist is created here so the
        // pointer path is still materialized.
        for (elem, subtree) in known_subentries {
            recurse_entries
                .entry(elem.clone())
                .or_insert_with(|| DeletedManifestUnfoldNode {
                    path_element: Some(elem),
                    changes: Default::default(),
                    parent_deleted_manifests: MultiMap::new(),
                    parent_unodes: MultiMap::new(),
                    current_unode: None,
                    known_entries: PathTree::default(),
                })
                .known_entries = subtree;
        }

        // Find intersections between parents manifests and unodes. Each such intersection means
        // that there's path that's deleted in one parent but still present in another parent.
        // In this case the merge commit will be undeleting the path so we have to recurse into
        // it to create a new deleted manifest with that path removed.
        for (parent, dfm_parent_cs_ids) in parent_manifests.iter() {
            for (other_parent_unode, unode_parent_cs_ids) in parent_tree_unodes.iter() {
                if dfm_parent_cs_ids.len() == 1 && dfm_parent_cs_ids == unode_parent_cs_ids {
                    continue;
                }
                let mut deleted_entries = parent
                    .clone()
                    .into_entries(ctx, blobstore)
                    .and_then(
                        |(name, id)| async move { Ok((MPathElement::from_smallvec(name)?, id)) },
                    )
                    .boxed()
                    .fuse();
                let mut unode_entries = other_parent_unode.iter();

                let mut deleted_entry = deleted_entries.next().await.transpose()?;
                let mut unode_entry = unode_entries.next();
                while let (
                    Some((deleted_mpath_elem, _deleted_id)),
                    Some((unode_mpath_elem, _uid)),
                ) = (&deleted_entry, unode_entry)
                {
                    if deleted_mpath_elem == unode_mpath_elem {
                        counters.merge_intersect.inc();
                        recurse_entries
                            .entry(deleted_mpath_elem.clone())
                            .or_insert_with(|| DeletedManifestUnfoldNode {
                                path_element: Some(deleted_mpath_elem.clone()),
                                changes: Default::default(),
                                parent_deleted_manifests: MultiMap::new(),
                                parent_unodes: MultiMap::new(),
                                current_unode: None,
                                known_entries: PathTree::default(),
                            });
                    }
                    if deleted_mpath_elem <= unode_mpath_elem {
                        deleted_entry = deleted_entries.next().await.transpose()?;
                    } else {
                        unode_entry = unode_entries.next();
                    }
                }
            }
        }
        let base = match parent_manifests.as_slice() {
            [] => None,
            [(parent, parent_cs_ids)] => {
                // If there's one parent, we can "copy" its subentries
                // and modify only a few fields. Important if we're doing few
                // changes on a big node and need to optimise.
                stream::iter(recurse_entries.iter_mut().map(anyhow::Ok))
                    .try_for_each_concurrent(100, |(path, node)| async move {
                        if let Some(subentry_id) =
                            parent.lookup(ctx, blobstore, path.as_ref()).await?
                        {
                            node.parent_deleted_manifests
                                .insert_many(subentry_id, parent_cs_ids.iter().cloned());
                        }
                        anyhow::Ok(())
                    })
                    .await?;

                Some(parent.clone())
            }
            _ => {
                // If there are multiple parents and they're different, we need to
                // merge all different subentries. So let's just look at all of them.
                for (parent, parent_cs_ids) in parent_manifests {
                    parent
                        .into_entries(ctx, blobstore)
                        .and_then(|(name, id)| async move {
                            Ok((MPathElement::from_smallvec(name)?, id))
                        })
                        .try_for_each(|(path, mf_id)| {
                            let entry = recurse_entries.entry(path.clone()).or_insert_with(|| {
                                counters.dfm_merge.inc();
                                DeletedManifestUnfoldNode {
                                    path_element: Some(path),
                                    changes: Default::default(),
                                    parent_deleted_manifests: MultiMap::new(),
                                    parent_unodes: MultiMap::new(),
                                    current_unode: None,
                                    known_entries: PathTree::default(),
                                }
                            });
                            entry
                                .parent_deleted_manifests
                                .insert_many(mf_id, parent_cs_ids.iter().cloned());
                            async { Ok(()) }
                        })
                        .await?;
                }
                None
            }
        };

        for (parent_unode, parent_cs_ids) in parent_tree_unodes.iter() {
            for (path, unode_entry) in parent_unode {
                if let Some(node) = recurse_entries.get_mut(path) {
                    node.parent_unodes
                        .insert_many(unode_entry.clone(), parent_cs_ids.iter().cloned());
                }
            }
        }
        if let Some(current_unode) = &current_unode {
            for (path, unode_entry) in current_unode {
                if let Some(node) = recurse_entries.get_mut(path) {
                    node.current_unode = Some(unode_entry.clone());
                }
            }
        }

        Ok((base, recurse_entries.into_values().collect::<Vec<_>>()))
    }

    async fn update_children(
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        parent: Option<DeletedManifestSubentries>,
        changes: BTreeMap<MPathElement, Option<DeletedManifestV2Id>>,
    ) -> Result<DeletedManifestSubentries, Error> {
        parent
            .unwrap_or_default()
            .update(
                ctx,
                blobstore,
                changes
                    .into_iter()
                    .map(|(name, id)| (Bytes::copy_from_slice(name.as_ref()), id))
                    .collect(),
                |_| (),
            )
            .await
    }

    pub(crate) async fn derive_children(
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        cs_id: ChangesetId,
        changes: PathTree<()>,
        input: DeletedManifestChildren,
    ) -> Result<DeletedManifestSubentries, Error> {
        let (parent, nodes) = Self::unfold_children(
            ctx,
            blobstore,
            changes,
            input,
            PathTree::default(),
            &Arc::new(DebugCounters::default()),
        )
        .await?;
        let updates = Self::derive_nodes(ctx, blobstore, cs_id, nodes)
            .await?
            .into_iter()
            .map(|(name, id)| Ok((name.ok_or_else(|| anyhow!("missing child name"))?, id)))
            .collect::<Result<_, Error>>()?;
        Self::update_children(ctx, blobstore, parent, updates).await
    }

    async fn save_manifest(
        manifest: DeletedManifestV2,
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        sender: mpsc::UnboundedSender<BoxFuture<'static, Result<(), Error>>>,
        created: Arc<Mutex<HashSet<String>>>,
    ) -> Result<DeletedManifestV2Id, Error> {
        let mf_id = manifest.id();

        let key = mf_id.blobstore_key();
        let mut created = created.lock().await;
        if created.insert(key.clone()) {
            let blob = manifest.into_blob();
            cloned!(ctx, blobstore);
            let f = async move { blobstore.put(&ctx, key, blob.into()).await }.boxed();

            sender
                .unbounded_send(f)
                .map_err(|err| anyhow!("failed to send manifest future {err}"))?;
        }
        Ok(mf_id)
    }

    async fn do_create(
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        cs_id: ChangesetId,
        change: DeletedManifestChange,
        subentries_to_update: BTreeMap<MPathElement, Option<DeletedManifestV2Id>>,
        sender: mpsc::UnboundedSender<BoxFuture<'static, Result<(), Error>>>,
        created: Arc<Mutex<HashSet<String>>>,
    ) -> Result<Option<DeletedManifestV2Id>, Error> {
        let (parent, linknode) = match change {
            DeletedManifestChange::Known(id) => return Ok(id),
            DeletedManifestChange::Reuse(parent) => return Ok(Some(parent.id())),
            DeletedManifestChange::CreateDeleted(parent) => (parent, Some(cs_id)),
            DeletedManifestChange::RemoveIfNowEmpty(parent) => (parent, None),
        };
        let children = Self::update_children(ctx, blobstore, parent, subentries_to_update).await?;
        if linknode.is_none() && children.is_empty() {
            return Ok(None);
        }
        Self::save_manifest(
            DeletedManifestV2::new(linknode, children),
            ctx,
            blobstore,
            sender,
            created,
        )
        .await
        .map(Some)
    }
}

pub(crate) async fn get_changes_list(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: BonsaiChangeset,
) -> Result<Vec<(NonRootMPath, PathChange)>, Error> {
    // Get file/directory changes between the current changeset and its parents
    //
    // get unode manifests first
    let bcs_id = bonsai.get_changeset_id();

    // get parent unodes
    let parent_cs_ids: Vec<_> = bonsai.parents().collect();
    let parent_unodes = parent_cs_ids.into_iter().map({
        move |cs_id| async move {
            let root_mf_id = derivation_ctx
                .fetch_dependency::<RootUnodeManifestId>(ctx, cs_id)
                .await?;
            Ok(root_mf_id.manifest_unode_id().clone())
        }
    });

    let (root_unode_mf_id, parent_mf_ids) = future::try_join(
        derivation_ctx.fetch_dependency::<RootUnodeManifestId>(ctx, bcs_id),
        future::try_join_all(parent_unodes),
    )
    .await?;

    // compute diff between changeset's and its parents' manifests
    let unode_mf_id = root_unode_mf_id.manifest_unode_id().clone();
    let changes = if parent_mf_ids.is_empty() {
        unode_mf_id
            .list_all_entries(ctx.clone(), derivation_ctx.blobstore().clone())
            .try_filter_map(move |(path, _)| async {
                match Option::<NonRootMPath>::from(path) {
                    Some(path) => Ok(Some((path, PathChange::Add))),
                    None => Ok(None),
                }
            })
            .try_collect::<Vec<_>>()
            .await
    } else {
        diff_against_parents(ctx, derivation_ctx, unode_mf_id, parent_mf_ids).await
    }?;

    Ok(changes)
}

async fn diff_against_parents(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    unode: ManifestUnodeId,
    parents: Vec<ManifestUnodeId>,
) -> Result<Vec<(NonRootMPath, PathChange)>, Error> {
    let blobstore = derivation_ctx.blobstore();
    let parent_diffs_fut = parents.into_iter().map({
        cloned!(ctx, blobstore, unode);
        move |parent| {
            parent
                .diff(ctx.clone(), blobstore.clone(), unode.clone())
                .try_collect::<Vec<_>>()
        }
    });
    let parent_diffs = future::try_join_all(parent_diffs_fut).await?;
    let diffs = parent_diffs
        .into_iter()
        .flatten()
        .filter_map(|diff| match diff {
            Diff::Added(path, _) => {
                Option::<NonRootMPath>::from(path).map(|path| (path, PathChange::Add))
            }
            Diff::Removed(path, _) => {
                Option::<NonRootMPath>::from(path).map(|path| (path, PathChange::Remove))
            }
            _ => None,
        });

    let mut changes = BTreeMap::new();
    for (path, change) in diffs {
        // If the changeset has file/dir conflict the diff between
        // parent manifests and the current will have two entries
        // for the same path: one to remove the file/dir, another
        // to introduce new dir/file node.
        changes
            .entry(path)
            .and_modify(|e| {
                if *e != change {
                    *e = PathChange::FileDirConflict
                }
            })
            .or_insert(change);
    }
    let res: Vec<_> = changes.into_iter().collect();
    Ok(res)
}

pub(crate) struct RootDeletedManifestDeriver<Root: RootDeletedManifestIdCommon>(
    std::marker::PhantomData<Root>,
);

impl<Root: RootDeletedManifestIdCommon<Manifest = DeletedManifestV2, Id = DeletedManifestV2Id>>
    RootDeletedManifestDeriver<Root>
{
    pub(crate) async fn derive_single(
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        bonsai: BonsaiChangeset,
        parent_manifests: Vec<Root>,
    ) -> Result<Root, Error> {
        let (current_unode, parent_unodes) = get_unodes(ctx, derivation_ctx, &bonsai).await?;
        let parents = bonsai
            .parents()
            .zip(parent_manifests)
            .zip(parent_unodes)
            .map(|((bcs_id, parent_dm), parent_unode)| {
                (bcs_id, parent_dm.id().clone(), parent_unode)
            })
            .collect();
        let id = DeletedManifestDeriver::derive(
            ctx,
            derivation_ctx.blobstore(),
            bonsai,
            parents,
            current_unode,
        )
        .await
        .with_context(|| format!("Deriving {}", Root::NAME))?;
        Ok(Root::new(id))
    }

    pub(crate) async fn derive_batch(
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        bonsais: Vec<BonsaiChangeset>,
    ) -> Result<HashMap<ChangesetId, Root>, Error> {
        let simple_stacks =
            split_bonsais_in_linear_stacks(&bonsais, FileConflicts::AnyChange.into())?;
        let id_to_bonsai: HashMap<ChangesetId, BonsaiChangeset> = bonsais
            .into_iter()
            .map(|bonsai| (bonsai.get_changeset_id(), bonsai))
            .collect();
        borrowed!(id_to_bonsai);
        // Map of ids to derived values.
        // We need to be careful to use this for self-references, since the intermediate derived
        // values don't get stored in blobstore until after this function returns.
        let mut derived: HashMap<ChangesetId, Root> = HashMap::with_capacity(id_to_bonsai.len());
        for stack in simple_stacks {
            let bonsais: Vec<BonsaiChangeset> = stack
                .stack_items
                .into_iter()
                // Panic safety: ids were created from the received bonsais
                .map(|item| id_to_bonsai.get(&item.cs_id).unwrap().clone())
                .collect();
            let parents: Vec<Root::Id> = stream::iter(stack.parents)
                .then(|p| match derived.get(&p) {
                    Some(root) => future::ok(root.clone()).left_future(),
                    None => derivation_ctx.fetch_dependency(ctx, p).right_future(),
                })
                .map_ok(|root: Root| root.id().clone())
                .try_collect()
                .await?;
            Self::derive_single_stack(ctx, derivation_ctx, bonsais, parents, &mut derived).await?;
        }
        Ok(derived)
    }

    async fn derive_serially(
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        stack: Vec<BonsaiChangeset>,
        derived: &mut HashMap<ChangesetId, Root>,
    ) -> Result<(), Error> {
        for bonsai in stack {
            let parents = derivation_ctx
                .fetch_unknown_parents(ctx, Some(derived), &bonsai)
                .await?;
            let id = bonsai.get_changeset_id();
            let root = Self::derive_single(ctx, derivation_ctx, bonsai, parents).await?;
            derived.insert(id, root);
        }
        Ok(())
    }

    async fn derive_single_stack(
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        stack: Vec<BonsaiChangeset>,
        parents: Vec<Root::Id>,
        derived: &mut HashMap<ChangesetId, Root>,
    ) -> Result<(), Error> {
        if stack.len() == 1 {
            // We can't derive stacks for merge commits or commits with subtree changes.
            // Let's derive normally.
            Self::derive_serially(ctx, derivation_ctx, stack, derived).await?;
        } else {
            let ids: Vec<_> = stack
                .iter()
                .map(|bonsai| bonsai.get_changeset_id())
                .collect();
            let all_changes = stream::iter(stack)
                .map(|bonsai| async move {
                    anyhow::Ok((
                        bonsai.get_changeset_id(),
                        get_changes_list(ctx, derivation_ctx, bonsai).await?,
                    ))
                })
                .buffered(100)
                .try_collect()
                .await?;
            let mf_ids = DeletedManifestDeriver::derive_simple_stack(
                ctx,
                derivation_ctx.blobstore(),
                parents.into_iter().next(),
                all_changes,
            )
            .await?;
            derived.extend(
                ids.into_iter()
                    .zip(mf_ids.into_iter().map(|id| Root::new(id))),
            );
        }
        Ok(())
    }

    pub(crate) async fn store_mapping(
        root: Root,
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        changeset_id: ChangesetId,
    ) -> Result<(), Error> {
        let key = Root::format_key(derivation_ctx, changeset_id);
        derivation_ctx.blobstore().put(ctx, key, root.into()).await
    }

    pub(crate) async fn fetch(
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        changeset_id: ChangesetId,
    ) -> Result<Option<Root>, Error> {
        let key = Root::format_key(derivation_ctx, changeset_id);
        derivation_ctx
            .blobstore()
            .get(ctx, &key)
            .await?
            .map(TryInto::try_into)
            .transpose()
    }
}

/// Returns root unode manifests for changeset and its parents.
pub(crate) async fn get_unodes(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: &BonsaiChangeset,
) -> Result<(ManifestUnodeId, Vec<ManifestUnodeId>), Error> {
    let parent_cs_ids: Vec<_> = bonsai.parents().collect();
    let parent_unodes = parent_cs_ids.into_iter().map({
        move |cs_id| async move {
            let root_mf_id = derivation_ctx
                .fetch_dependency::<RootUnodeManifestId>(ctx, cs_id)
                .await?;
            Ok(root_mf_id.manifest_unode_id().clone())
        }
    });

    let (root_unode_mf_id, parent_unodes) = future::try_join(
        derivation_ctx.fetch_dependency::<RootUnodeManifestId>(ctx, bonsai.get_changeset_id()),
        // We're assuming that the commits have hanful of parents in (in most
        // cases <= 2) and iterating over all of them won't be a problem.
        // (which is the case for all our current repos)
        future::try_join_all(parent_unodes),
    )
    .await?;

    Ok((root_unode_mf_id.manifest_unode_id().clone(), parent_unodes))
}

#[cfg(test)]
mod test {
    use anyhow::Result;
    use derivation_queue_thrift::DerivationPriority;
    use derived_data_manager::BonsaiDerivable;
    use fbinit::FacebookInit;
    use maplit::btreemap;
    use mononoke_macros::mononoke;
    use mononoke_types::DeletedManifestV2Id;
    use pretty_assertions::assert_eq;
    use repo_blobstore::RepoBlobstoreArc;
    use repo_derived_data::RepoDerivedDataRef;
    use tests_utils::drawdag::changes;
    use tests_utils::drawdag::create_from_dag_with_changes;
    use tests_utils::drawdag::extend_from_dag_with_changes;
    use unodes::RootUnodeManifestId;

    use super::*;
    use crate::RootDeletedManifestV2Id;
    use crate::test_utils::TestRepo;
    use crate::test_utils::build_repo;
    type Id = DeletedManifestV2Id;
    type Root = RootDeletedManifestV2Id;

    async fn assert_derive_stack(
        ctx: &CoreContext,
        derivation_ctx: &DerivationContext,
        stacked_bonsais: Vec<BonsaiChangeset>,
    ) -> Result<()> {
        let mut derived_stack: HashMap<ChangesetId, Root> = Default::default();
        let parent_dm: Option<Id> = if let Some(parent_cs_id) = stacked_bonsais[0].parents().next()
        {
            let root: Root = derivation_ctx.fetch_dependency(ctx, parent_cs_id).await?;
            Some(*root.id())
        } else {
            None
        };

        RootDeletedManifestDeriver::<Root>::derive_single_stack(
            ctx,
            derivation_ctx,
            stacked_bonsais.clone(),
            parent_dm.into_iter().collect(),
            &mut derived_stack,
        )
        .await?;

        let mut derived_serially: HashMap<ChangesetId, Root> = Default::default();
        RootDeletedManifestDeriver::<Root>::derive_serially(
            ctx,
            derivation_ctx,
            stacked_bonsais.clone(),
            &mut derived_serially,
        )
        .await?;

        for bonsai in stacked_bonsais {
            let cs_id = bonsai.get_changeset_id().clone();
            assert_eq!(derived_stack.get(&cs_id), derived_serially.get(&cs_id));

            // we need to persist as subsequent calls will be reading the mapping
            derived_stack
                .get(&cs_id)
                .expect("changeset missing in derived stack")
                .store_mapping(ctx, derivation_ctx, cs_id)
                .await?;
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn known_children_preserve_history_without_loading_unodes(
        fb: FacebookInit,
    ) -> Result<()> {
        let repo = build_repo(fb).await?;
        let ctx = CoreContext::test_mock(fb);
        let blobstore = repo.repo_blobstore_arc() as Arc<dyn KeyedBlobstore>;
        let csid = ChangesetId::from_bytes([1; 32])?;
        let parent = ChangesetId::from_bytes([2; 32])?;
        // These unodes are intentionally absent: complete children make loading them unnecessary.
        let directory = UnodeEntry::Directory(ManifestUnodeId::from_bytes([3; 32])?);
        let file = UnodeEntry::File(mononoke_types::FileUnodeId::from_bytes([4; 32])?);
        let deleted_child = DeletedManifestV2::new(Some(parent), ShardedMapNode::default());
        let child_id = DeletedManifestDeriver::save_mf(deleted_child, &ctx, &blobstore).await?;
        let children = ShardedMapNode::from_entries(
            &ctx,
            &blobstore,
            BTreeMap::from([(
                Bytes::from_static(b"deleted"),
                itertools::Either::Left(child_id),
            )]),
        )
        .await?;
        for children in [children, ShardedMapNode::default()] {
            for current in [None, Some(directory.clone()), Some(file.clone())] {
                for materialize_empty_root in [false, true] {
                    let output = DeletedManifestDeriver::derive_subtree(
                        &ctx,
                        &blobstore,
                        csid,
                        PathTree::from_iter([(MPath::try_from("deleted")?, ())]),
                        MultiMap::new(),
                        current.clone(),
                        MultiMap::from_iter([(directory.clone(), parent)]),
                        HashMap::from([(
                            MPath::ROOT,
                            KnownDeletedManifest::Children(children.clone()),
                        )]),
                        materialize_empty_root,
                    )
                    .await?;
                    let expected =
                        if current.is_some() && children.is_empty() && !materialize_empty_root {
                            None
                        } else {
                            Some(
                                DeletedManifestV2::new(
                                    current.is_none().then_some(csid),
                                    children.clone(),
                                )
                                .id(),
                            )
                        };
                    assert_eq!(output, expected);
                    if let Some(id) = output {
                        assert_eq!(id.load(&ctx, &blobstore).await?.subentries, children);
                    }
                }
            }
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_stack_derive(fb: FacebookInit) -> Result<()> {
        let repo: TestRepo = build_repo(fb).await.unwrap();
        let blobstore = repo.repo_blobstore_arc() as Arc<dyn KeyedBlobstore>;
        let derivation_ctx = &repo.repo_derived_data().manager().derivation_context(None);

        let ctx = CoreContext::test_mock(fb);

        let commits = create_from_dag_with_changes(
            &ctx,
            &repo,
            r##"
                A-B-C-D-E-F-G-H
            "##,
            changes! {
                "A" => |c| c.add_file("dira/a", "a"),
                "B" => |c| c.add_file("dirb/b", "b"),
                "C" => |c| c.delete_file("dira/a"),
                "D" => |c| c.add_file("dira/a", "aa").add_file("dirc/c", "c"),
                "E" => |c| c.delete_file("dirb/b").delete_file("dirc/c"),
                "F" => |c| c.add_file("dirc/d", "d").add_file("dirc/c/inner_c", "c").add_file("new_file", "new"),
                "G" => |c| c.delete_file("dirc/c/inner_c").add_file("dird/d/inner_d", "d"),
                "H" => |c| c.delete_file("dird/d/inner_d").delete_file("new_file").add_file("newer_file", "nwer"),
            },
        )
        .await?;

        repo.repo_derived_data()
            .manager()
            .derive::<RootUnodeManifestId>(
                &ctx,
                *commits.get("H").expect("Drawdag failed us"),
                None,
                DerivationPriority::LOW,
            )
            .await?;

        borrowed!(ctx, commits);
        let blobstore: &Arc<dyn KeyedBlobstore> = &blobstore;

        let derive = |stack: Vec<&'static str>| async move {
            let bonsais = future::try_join_all(stack.iter().map(|c| {
                commits
                    .get(*c)
                    .expect("commit doesn't exist")
                    .load(ctx, blobstore)
            }))
            .await?;
            assert_derive_stack(ctx, derivation_ctx, bonsais).await
        };

        derive(vec!["A", "B", "C", "D"]).await?;
        derive(vec!["E", "F", "G", "H"]).await?;

        // Let's try to get many operations on a single node, basically
        // by deleting and re-adding the same files over and over again
        let files = ["/dir/a", "/dir/b", "/dir/c", "/dir/d", "/other_dir/e"];
        let mut has = vec![false; files.len()];

        let mut last = *commits.get("H").expect("h doesn't exist");
        let mut new_commits = vec![];

        for idx in 1..100usize {
            let idx1 = idx % files.len();
            let idx2 = (idx * 173) % files.len();
            let has1 = has[idx1];
            let has2 = has[idx2];
            cloned!(files);
            has[idx1] = !has1;
            if idx2 != idx1 {
                has[idx2] = !has2;
            }
            let (commits, _dag) = extend_from_dag_with_changes(
                ctx,
                &repo,
                r##"
                    LAST - NEW
                "##,
                None,
                changes! {
                    "NEW" => |c| {
                        let c = if has1 {
                            c.delete_file(files[idx1])
                        } else {
                            c.add_file(files[idx1], "contents")
                        };
                        if idx2 != idx1 {
                            if has2 {
                                c.delete_file(files[idx2])

                            } else {
                                c.add_file(files[idx2], "contents")
                            }
                        } else {
                            c
                        }
                    },
                },
                btreemap! {
                    "LAST".to_string() => last,
                },
                false,
            )
            .await?;
            last = commits
                .into_values()
                .next()
                .expect("commit was not created!");
            new_commits.push(last);
        }
        let bonsais =
            future::try_join_all(new_commits.iter().map(|nc| nc.load(ctx, blobstore))).await?;
        assert_derive_stack(ctx, derivation_ctx, bonsais[..50].to_vec()).await?;
        assert_derive_stack(ctx, derivation_ctx, bonsais[50..].to_vec()).await?;
        let root: Root = derivation_ctx
            .fetch_dependency(
                ctx,
                bonsais
                    .last()
                    .expect("this vec can't be empty")
                    .get_changeset_id(),
            )
            .await?;
        // DM format shouldn't change easily, let's store it in a test.
        assert_eq!(
            root.id().blobstore_key(),
            "deletedmanifest2.blake2.00c7baf3502624a9a0a5f5a76083f86427a3c00181aa28d7763afc485824f4d6"
        );

        Ok(())
    }
}
