/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::hash::Hash;
use std::iter::Iterator;
use std::sync::Arc;

use anyhow::Result;
use anyhow::ensure;
use anyhow::format_err;
use borrowed::borrowed;
use cloned::cloned;
use context::CoreContext;
use futures::future;
use futures::future::FutureExt;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use itertools::Either;
use mononoke_macros::mononoke;
use mononoke_types::MPathElement;
use mononoke_types::NonRootMPath;
use mononoke_types::path::MPath;
use mononoke_types::prefix_tree::PrefixTree;
use smallvec::SmallVec;
use smallvec::smallvec;

use crate::Deferred;
use crate::Entry;
use crate::Manifest;
use crate::PathTree;
use crate::StoreLoadable;
use crate::TrieMapOps;

/// Information passed to `create_tree` function when tree node is constructed
///
/// `Ctx` is any additional data which is useful for particular implementation of
/// manifest. It is `Some` for subentries for which `create_{tree|leaf}` was called to
/// generate them, and `None` if subentry was reused from one of its parents.
pub struct TreeInfo<TreeId, Leaf, Ctx, TrieMapType> {
    pub path: MPath,
    pub parents: Vec<TreeId>,
    pub subentries: TreeInfoSubentries<TreeId, Leaf, Ctx, TrieMapType>,
}

/// Represents the subentries of a tree node as a combination of singular subentries and
/// reused parent maps. The key for singular subentries is their name, while the key for
/// reused maps is a prefix that's prepended to all of the keys of the map.
pub type TreeInfoSubentries<TreeId, Leaf, Ctx, TrieMapType> =
    BTreeMap<SmallVec<[u8; 24]>, Either<(Option<Ctx>, Entry<TreeId, Leaf>), TrieMapType>>;

pub async fn flatten_subentries<Store, TreeId, Leaf, Ctx, TrieMapType>(
    ctx: &CoreContext,
    blobstore: &Store,
    subentries: TreeInfoSubentries<TreeId, Leaf, Ctx, TrieMapType>,
) -> Result<
    impl Iterator<Item = (MPathElement, (Option<Ctx>, Entry<TreeId, Leaf>))>
    + use<Store, TreeId, Leaf, Ctx, TrieMapType>,
>
where
    TrieMapType: TrieMapOps<Store, Entry<TreeId, Leaf>>,
{
    Ok(stream::iter(subentries)
        .map(|(prefix, entry_or_map)| async move {
            match entry_or_map {
                Either::Left((ctx, entry)) => {
                    Ok(vec![(MPathElement::from_smallvec(prefix)?, (ctx, entry))])
                }
                Either::Right(map) => map
                    .into_stream(ctx, blobstore)
                    .await?
                    .map_ok(|(mut path, entry)| {
                        path.insert_from_slice(0, prefix.as_ref());
                        Ok((MPathElement::from_smallvec(path)?, (None, entry)))
                    })
                    .try_collect::<Vec<_>>()
                    .await?
                    .into_iter()
                    .collect::<Result<_>>(),
            }
        })
        .buffer_unordered(100)
        .try_collect::<Vec<_>>()
        .await?
        .into_iter()
        .flatten())
}

/// Information passed to `create_leaf` function when leaf node is constructed
pub struct LeafInfo<Leaf, LeafChange> {
    pub path: NonRootMPath,
    pub parents: Vec<Leaf>,
    /// Leaf value, if it is not provided it means we have leaves only conflict
    /// which can potentially be resolved by `create_leaf`, in case of mercurial
    /// multiple leaves with the same content can be successfully resolved.
    pub change: Option<LeafChange>,
}

/// Derive a new manifest from parents and a set of changes. The types have to match in the
/// following way here:
///
/// - We'll walk and merge the manifests from the parents. Those must be Manifests where trees are
///   `TreeId` and leaves are `Leaf`.
/// - We'll create new leaves (for the diff) through create_leaf (which should merge changes), and
///   new trees through create_tree, which will receive entries consisting of existing trees and
///   trees merged with new leaves and trees (and should produce a new tree).
/// - To make this work, `create_tree` must return the same kind of `TreeId` as the ones that exist in
///   the tree currently, and `create_leaf` must return the same kind of `Leaf`.
pub async fn derive_manifest<LeafChange, TreeId, Leaf, T, TFut, L, LFut, Ctx, Store>(
    ctx: CoreContext,
    store: Store,
    parents: impl IntoIterator<Item = TreeId>,
    changes: impl IntoIterator<Item = (NonRootMPath, Option<LeafChange>)>,
    create_tree: T,
    create_leaf: L,
) -> Result<Option<TreeId>>
where
    Store: Sync + Send + Clone + 'static,
    LeafChange: Send + Clone + Eq + Hash + fmt::Debug + 'static,
    Leaf: Send + Clone + Eq + Hash + fmt::Debug + 'static,
    TreeId: StoreLoadable<Store> + Clone + Eq + Hash + fmt::Debug + Send + Sync + 'static,
    TreeId::Value: Manifest<Store, TreeId = TreeId, Leaf = Leaf> + Send + Sync,
    T: Fn(TreeInfo<TreeId, Leaf, Ctx, <TreeId::Value as Manifest<Store>>::TrieMapType>) -> TFut
        + Send
        + Sync
        + 'static,
    TFut: Future<Output = Result<(Ctx, TreeId)>> + Send + 'static,
    L: Fn(LeafInfo<Leaf, LeafChange>) -> LFut + Send + Sync + 'static,
    LFut: Future<Output = Result<(Ctx, Leaf)>> + Send + 'static,
    <TreeId::Value as Manifest<Store>>::TrieMapType:
        TrieMapOps<Store, Entry<TreeId, Leaf>> + Send + Sync + 'static,
    Ctx: Send + 'static,
{
    derive_manifest_with_known_entries(
        ctx,
        store,
        parents.into_iter().map(Entry::Tree),
        changes,
        HashMap::new(),
        MPath::ROOT,
        create_tree,
        create_leaf,
    )
    .await
    .map(|opt| opt.and_then(|entry| entry.into_tree()))
}

/// A completed entry, or the complete children to assemble at its path.
#[derive(derive_more::From)]
pub enum KnownEntry<TreeId, Leaf, TrieMapType> {
    Entry(Option<Entry<TreeId, Leaf>>),
    /// The complete derived children, in maps with disjoint first-byte ranges.
    #[from(ignore)]
    Children(Deferred<Vec<TrieMapType>>),
}

/// Like `derive_manifest`, reusing completed results at repository-relative paths.
/// These must be derived from the same parents and changes.
pub fn derive_manifest_with_known_entries<LeafChange, TreeId, Leaf, T, TFut, L, LFut, Ctx, Store>(
    ctx: CoreContext,
    store: Store,
    parents: impl IntoIterator<Item = Entry<TreeId, Leaf>>,
    changes: impl IntoIterator<Item = (NonRootMPath, Option<LeafChange>)>,
    known_entries: HashMap<
        MPath,
        KnownEntry<TreeId, Leaf, <TreeId::Value as Manifest<Store>>::TrieMapType>,
    >,
    prefix: MPath,
    create_tree: T,
    create_leaf: L,
) -> impl Future<Output = Result<Option<Entry<TreeId, Leaf>>>>
where
    Store: Sync + Send + Clone + 'static,
    LeafChange: Send + Clone + Eq + Hash + fmt::Debug + 'static,
    Leaf: Send + Clone + Eq + Hash + fmt::Debug + 'static,
    TreeId: StoreLoadable<Store> + Clone + Eq + Hash + fmt::Debug + Send + Sync + 'static,
    TreeId::Value: Manifest<Store, TreeId = TreeId, Leaf = Leaf> + Send + Sync,
    T: Fn(TreeInfo<TreeId, Leaf, Ctx, <TreeId::Value as Manifest<Store>>::TrieMapType>) -> TFut
        + Send
        + Sync
        + 'static,
    TFut: Future<Output = Result<(Ctx, TreeId)>> + Send + 'static,
    L: Fn(LeafInfo<Leaf, LeafChange>) -> LFut + Send + Sync + 'static,
    LFut: Future<Output = Result<(Ctx, Leaf)>> + Send + 'static,
    <TreeId::Value as Manifest<Store>>::TrieMapType:
        TrieMapOps<Store, Entry<TreeId, Leaf>> + Send + Sync + 'static,
    Ctx: Send + 'static,
{
    derive_manifest_inner(
        ctx,
        store,
        parents,
        changes,
        known_entries,
        prefix,
        create_tree,
        create_leaf,
    )
}

/// Construct a new manifest from parent manifests and a list of changes from a bonsai commit.
/// The manifest is constructed in accordance with bonsai semantic (see below for explanation).
///
/// Parent manifests should have been constructed for each parent of this bonsai commit
/// (note that there can be more than 2 parents).
///
/// In the context of this function "manifest" means a set of leaves and trees. Manifest is a
/// recursive structure that starts with a root tree which can point to other trees or leaves.
/// Each leaf represents a file in a repository, and each tree represents a directory.
///
/// Note that while `derive_manifest` can be used to construct Mercurial manifests
/// (e.g. Manifests where leafs are filenodes), it's not limited to them. Leafs can be arbitrary
/// ids e.g. SHA-1 content of a file, unode ids. Tree ids can also be arbitrary.
///
/// ## Bonsai Semantic
/// Bonsai semantic is a set of rules about how to apply changes to manifests. Each change is
/// either `Some(Leaf)` meaning that a new file exists in the new commit (either new file is
/// created or reused from one of the parents), `None` meaning that the file was deleted from
/// one of the parents and no longer exist in the new bonsai commit.
/// Also see: [Bonsai changeset actions](https://fb.quip.com/A2kqArd9Nb90)
///
/// Changes are applied recursively starting from the root of parent manifests.
/// Here is how changes affect conflict resolution
/// 1. If no change ends on the current path or any subpaths e.g. we are just merging parent
///    manifests.
///    - If all entries in parent manifests are identical (regardless of whether they are trees
///      or leaves) then we are just reusing the current entry.
///    - If all parents entries are trees then recurse into all of them and continue merging.
///    - If all parent entries are leaves then `create_leaf` is called with `None` leaf, this
///      can result in successful merge (for example, mercurial can merge different entries with
///      same content).
///    - If we have a mix of leaves/trees in parent entries - we have a broken commit and this is
///      an unresolved conflict error.
/// 2. If no change ends on the current path BUT there are changes on the subpaths (e.g. we are
///    on "A/", but there's "A/file.txt").
///    - If all parents entries are trees then recurse into all of them and continue merging
///    - If we have a single leaf in parent entries - we have a broken commit and this is an
///      unresolved conflict error.
/// 3. Current path have `None` change associated with it.
///   - Only trees: invalid changes.
///   - Only leaves: all leaves are removed.
///   - Mix of leaves/trees: all leaves are removed, recurse into the trees.
/// 4. Current path have `Some(leaf)` change associated with it.
///   - _: all the trees are removed in favour of this leaf.
pub async fn derive_manifest_inner<LeafChange, TreeId, Leaf, T, TFut, L, LFut, Ctx, Store>(
    ctx: CoreContext,
    store: Store,
    parents: impl IntoIterator<Item = Entry<TreeId, Leaf>>,
    changes: impl IntoIterator<Item = (NonRootMPath, Option<LeafChange>)>,
    known_entries: HashMap<
        MPath,
        KnownEntry<TreeId, Leaf, <TreeId::Value as Manifest<Store>>::TrieMapType>,
    >,
    prefix: MPath,
    create_tree: T,
    create_leaf: L,
) -> Result<Option<Entry<TreeId, Leaf>>>
where
    Store: Sync + Send + Clone + 'static,
    LeafChange: Send + Clone + Eq + Hash + fmt::Debug + 'static,
    Leaf: Send + Clone + Eq + Hash + fmt::Debug + 'static,
    TreeId: StoreLoadable<Store> + Clone + Eq + Hash + fmt::Debug + Send + Sync + 'static,
    TreeId::Value: Manifest<Store, TreeId = TreeId, Leaf = Leaf> + Send + Sync,
    T: Fn(TreeInfo<TreeId, Leaf, Ctx, <TreeId::Value as Manifest<Store>>::TrieMapType>) -> TFut
        + Send
        + Sync
        + 'static,
    TFut: Future<Output = Result<(Ctx, TreeId)>> + Send + 'static,
    L: Fn(LeafInfo<Leaf, LeafChange>) -> LFut + Send + Sync + 'static,
    LFut: Future<Output = Result<(Ctx, Leaf)>> + Send + 'static,
    <TreeId::Value as Manifest<Store>>::TrieMapType:
        TrieMapOps<Store, Entry<TreeId, Leaf>> + Send + 'static,
    Ctx: Send + 'static,
{
    // Implicit delete: a file added at a strict prefix of `prefix` replaces the subtree.
    let changes: Vec<_> = changes.into_iter().collect();
    let implicit_delete = changes.iter().any(|(mpath, change)| {
        change.is_some() && mpath.is_prefix_of(&prefix) && !prefix.is_prefix_of(mpath)
    });
    if implicit_delete {
        return Ok(None);
    }

    let mut result = derive_manifest_nodes(
        ctx,
        store,
        [MergeNode {
            name: None,
            changes: PathTree::from_iter(
                // Keep changes under `prefix` (re-rooted to a stage-relative path)
                // and changes landing exactly at `prefix` (the stage root itself,
                // keyed at `MPath::ROOT` so the merge handles it as a leaf/None
                // change on the current path); drop disjoint changes. Using
                // `MPath::is_prefix_of` covers both `P` under `prefix` and
                // `P == prefix`, matching the `known_entries` re-rooting above.
                changes.into_iter().filter_map(|(mpath, change)| {
                    let mpath = MPath::from(mpath);
                    prefix.is_prefix_of(&mpath).then(|| {
                        (
                            mpath.remove_prefix_component(&prefix),
                            Some(Change::from(change)),
                        )
                    })
                }),
            ),
            parents: parents.into_iter().collect(),
            known_entries: PathTree::from_iter(
                known_entries
                    .into_iter()
                    .map(|(path, entry)| (path.remove_prefix_component(&prefix), Some(entry))),
            ),
            path: prefix,
        }],
        create_tree,
        create_leaf,
    )
    .await?;
    Ok(result.pop().flatten().map(|derived| derived.entry))
}

/// Derive the supplied children of `path` without constructing their containing
/// directory. The caller selects the parent child maps and changes to include.
pub async fn derive_manifest_subentries<LeafChange, TreeId, Leaf, T, TFut, L, LFut, Ctx, Store>(
    ctx: CoreContext,
    store: Store,
    parents: Vec<<TreeId::Value as Manifest<Store>>::TrieMapType>,
    changes: impl IntoIterator<Item = (NonRootMPath, Option<LeafChange>)>,
    path: MPath,
    create_tree: T,
    create_leaf: L,
) -> Result<TreeInfoSubentries<TreeId, Leaf, Ctx, <TreeId::Value as Manifest<Store>>::TrieMapType>>
where
    Store: Sync + Send + Clone + 'static,
    LeafChange: Send + Clone + Eq + Hash + fmt::Debug + 'static,
    Leaf: Send + Clone + Eq + Hash + fmt::Debug + 'static,
    TreeId: StoreLoadable<Store> + Clone + Eq + Hash + fmt::Debug + Send + Sync + 'static,
    TreeId::Value: Manifest<Store, TreeId = TreeId, Leaf = Leaf> + Send + Sync,
    T: Fn(TreeInfo<TreeId, Leaf, Ctx, <TreeId::Value as Manifest<Store>>::TrieMapType>) -> TFut
        + Send
        + Sync
        + 'static,
    TFut: Future<Output = Result<(Ctx, TreeId)>> + Send + 'static,
    L: Fn(LeafInfo<Leaf, LeafChange>) -> LFut + Send + Sync + 'static,
    LFut: Future<Output = Result<(Ctx, Leaf)>> + Send + 'static,
    <TreeId::Value as Manifest<Store>>::TrieMapType:
        TrieMapOps<Store, Entry<TreeId, Leaf>> + Send + 'static,
    Ctx: Send + 'static,
{
    let changes = PathTree::from_iter(changes.into_iter().filter_map(|(mpath, change)| {
        let mpath = MPath::from(mpath);
        (path != mpath && path.is_prefix_of(&mpath)).then(|| {
            (
                mpath.remove_prefix_component(&path),
                Some(Change::from(change)),
            )
        })
    }));
    let merged = merge_subentries(
        &ctx,
        &store,
        &path,
        changes.subentries,
        parents,
        Default::default(),
    )
    .await?;
    let children =
        derive_manifest_nodes(ctx, store, merged.merge_nodes, create_tree, create_leaf).await?;
    Ok(collect_subentries(children, merged.reused_maps))
}

struct DerivedEntry<TreeId, Leaf, Ctx> {
    name: Option<MPathElement>,
    context: Option<Ctx>,
    entry: Entry<TreeId, Leaf>,
}

fn collect_subentries<TreeId, Leaf, Ctx, TrieMapType>(
    children: impl IntoIterator<Item = Option<DerivedEntry<TreeId, Leaf, Ctx>>>,
    reused_maps: Vec<(SmallVec<[u8; 24]>, TrieMapType)>,
) -> TreeInfoSubentries<TreeId, Leaf, Ctx, TrieMapType> {
    let mut subentries = BTreeMap::new();
    for child in children.into_iter().flatten() {
        if let Some(name) = child.name {
            subentries.insert(
                name.to_smallvec(),
                Either::Left((child.context, child.entry)),
            );
        }
    }
    subentries.extend(
        reused_maps
            .into_iter()
            .map(|(prefix, map)| (prefix, Either::Right(map))),
    );
    subentries
}

async fn derive_manifest_nodes<LeafChange, TreeId, Leaf, T, TFut, L, LFut, Ctx, Store>(
    ctx: CoreContext,
    store: Store,
    roots: impl IntoIterator<
        Item = MergeNode<TreeId, Leaf, LeafChange, <TreeId::Value as Manifest<Store>>::TrieMapType>,
    >,
    create_tree: T,
    create_leaf: L,
) -> Result<Vec<Option<DerivedEntry<TreeId, Leaf, Ctx>>>>
where
    Store: Sync + Send + Clone + 'static,
    LeafChange: Send + Clone + Eq + Hash + fmt::Debug + 'static,
    Leaf: Send + Clone + Eq + Hash + fmt::Debug + 'static,
    TreeId: StoreLoadable<Store> + Clone + Eq + Hash + fmt::Debug + Send + Sync + 'static,
    TreeId::Value: Manifest<Store, TreeId = TreeId, Leaf = Leaf> + Send + Sync,
    T: Fn(TreeInfo<TreeId, Leaf, Ctx, <TreeId::Value as Manifest<Store>>::TrieMapType>) -> TFut
        + Send
        + Sync
        + 'static,
    TFut: Future<Output = Result<(Ctx, TreeId)>> + Send + 'static,
    L: Fn(LeafInfo<Leaf, LeafChange>) -> LFut + Send + Sync + 'static,
    LFut: Future<Output = Result<(Ctx, Leaf)>> + Send + 'static,
    <TreeId::Value as Manifest<Store>>::TrieMapType:
        TrieMapOps<Store, Entry<TreeId, Leaf>> + Send + 'static,
    Ctx: Send + 'static,
{
    bounded_traversal::bounded_traversal_forest(
        256,
        roots,
        move |node| merge(ctx.clone(), store.clone(), node).boxed(),
        {
            let create_tree = Arc::new(create_tree);
            let create_leaf = Arc::new(create_leaf);
            move |result, children| {
                let create_tree = create_tree.clone();
                let create_leaf = create_leaf.clone();
                async move {
                    mononoke::spawn_task(async move {
                        match result {
                            MergeResult::Reuse { name, entry } => Ok(Some(DerivedEntry {
                                name,
                                context: None,
                                entry,
                            })),
                            MergeResult::Delete => Ok(None),
                            MergeResult::CreateLeaf {
                                change,
                                name,
                                path,
                                parents,
                            } => {
                                let (context, leaf_id) = create_leaf(LeafInfo {
                                    change,
                                    path,
                                    parents,
                                })
                                .await?;
                                Ok(Some(DerivedEntry {
                                    name,
                                    context: Some(context),
                                    entry: Entry::Leaf(leaf_id),
                                }))
                            }
                            MergeResult::CreateTree {
                                name,
                                path,
                                parents,
                                reused_maps,
                            } => {
                                let subentries = collect_subentries(children, reused_maps);
                                if subentries.is_empty() {
                                    return Ok(None);
                                }
                                let (context, id) = create_tree(TreeInfo {
                                    path,
                                    parents,
                                    subentries,
                                })
                                .await?;
                                Ok(Some(DerivedEntry {
                                    name,
                                    context: Some(context),
                                    entry: Entry::Tree(id),
                                }))
                            }
                        }
                    })
                    .await?
                }
                .boxed()
            }
        },
    )
    .await
}

// Change is isomorphic to Option, but it makes it easier to understand merge logic
enum Change<LeafChange> {
    Add(LeafChange),
    Remove,
}

impl<Leaf> From<Option<Leaf>> for Change<Leaf> {
    fn from(change: Option<Leaf>) -> Self {
        change.map_or(Change::Remove, Change::Add)
    }
}

enum MergeResult<TreeId, Leaf, LeafChange, TrieMapType> {
    Delete,
    Reuse {
        name: Option<MPathElement>,
        entry: Entry<TreeId, Leaf>,
    },
    CreateLeaf {
        change: Option<LeafChange>,
        name: Option<MPathElement>,
        path: NonRootMPath,
        parents: Vec<Leaf>,
    },
    CreateTree {
        name: Option<MPathElement>,
        path: MPath,
        parents: Vec<TreeId>,
        reused_maps: Vec<(SmallVec<[u8; 24]>, TrieMapType)>,
    },
}

/// This node represents unmerged state of `parents.len() + 1` way merge
/// between changes and parents.
struct MergeNode<TreeId, Leaf, LeafChange, TrieMapType> {
    name: Option<MPathElement>, // name of this node in parent manifest
    path: MPath,                // path to this node from root of the manifest
    changes: PathTree<Option<Change<LeafChange>>>, // changes associated with current subtree
    parents: Vec<Entry<TreeId, Leaf>>, // unmerged parents of current node
    known_entries: PathTree<Option<KnownEntry<TreeId, Leaf, TrieMapType>>>,
}

async fn merge<TreeId, Leaf, LeafChange, Store>(
    ctx: CoreContext,
    store: Store,
    node: MergeNode<TreeId, Leaf, LeafChange, <TreeId::Value as Manifest<Store>>::TrieMapType>,
) -> Result<(
    MergeResult<TreeId, Leaf, LeafChange, <TreeId::Value as Manifest<Store>>::TrieMapType>,
    Vec<MergeNode<TreeId, Leaf, LeafChange, <TreeId::Value as Manifest<Store>>::TrieMapType>>,
)>
where
    Store: Sync + Send + Clone + 'static,
    LeafChange: Send + Clone + Eq + Hash + fmt::Debug,
    Leaf: Send + Clone + Eq + Hash + fmt::Debug,
    TreeId: StoreLoadable<Store> + Clone + Eq + Hash + fmt::Debug + Send + Sync,
    TreeId::Value: Manifest<Store, TreeId = TreeId, Leaf = Leaf>,
    <TreeId::Value as Manifest<Store>>::TrieMapType: TrieMapOps<Store, Entry<TreeId, Leaf>>,
{
    let MergeNode {
        name,
        path,
        changes: PathTree {
            value: change,
            subentries,
        },
        mut parents,
        known_entries:
            PathTree {
                value: known_entry_value,
                subentries: known_entries_subentries,
            },
    } = node;

    let known_children = match known_entry_value {
        Some(KnownEntry::Entry(entry)) => {
            return Ok(match entry {
                Some(entry) => (MergeResult::Reuse { name, entry }, Vec::new()),
                None => (MergeResult::Delete, Vec::new()),
            });
        }
        Some(KnownEntry::Children(children)) => Some(children),
        None => None,
    };

    // Deduplicate entries in parents list, **preserving order** of entries.
    // Essentially performing a trivial merge between identical entries.
    {
        let mut visited = HashSet::new();
        parents.retain(|parent| visited.insert(parent.clone()));
    }

    // Apply change
    // If we create `parent_subtrees` (none of the return statement have been reached), this
    // indicates that file/tree conflict if any, has been resolved in favour of tree merge.
    let parent_subtrees = match change {
        None => match parents.as_slice() {
            // Changes does not have entry associated with current path
            [parent_entry] => {
                // Only one tree/leaf is left
                if !subentries.is_empty() {
                    match parent_entry {
                        Entry::Leaf(_) => {
                            // Current entry is a leaf but we still have changes that needs
                            // to be applied to its subentries, we cannot resolve this merge.
                            let error = format_err!(
                                "Can not apply changes to a leaf:\npath: {path:?}\nparents: {parents:?}"
                            );
                            return Err(error);
                        }
                        Entry::Tree(tree_id) => {
                            // We have tree entry, and changes that needs to be applied
                            // to its subentries, we cannot reuse this entry and have to recurse
                            vec![tree_id.clone()]
                        }
                    }
                } else {
                    // We have single entry and do not have any changes associated with it subentries,
                    // it is safe to reuse current entry as is.
                    return Ok((
                        MergeResult::Reuse {
                            name,
                            entry: parent_entry.clone(),
                        },
                        Vec::new(),
                    ));
                }
            }
            _ => {
                // Split entries into leaves and trees.
                let mut leaves = Vec::new();
                let mut trees = Vec::new();
                for entry in parents.iter() {
                    match entry {
                        Entry::Leaf(leaf) => leaves.push(leaf.clone()),
                        Entry::Tree(tree) => trees.push(tree.clone()),
                    }
                }

                if leaves.is_empty() {
                    // We do not have any leaves at this point, and should proceed with
                    // merging of trees
                    trees
                } else if trees.is_empty() && subentries.is_empty() {
                    // We have leaves only but their ids are not equal to each other,
                    // this should immediately indicate conflict, as mercurial can successfully
                    // merge these leaves if they have identical content.
                    return Ok((
                        MergeResult::CreateLeaf {
                            change: None,
                            name,
                            path: path
                                .into_optional_non_root_path()
                                .expect("leaf can not have empty path"),
                            parents: leaves,
                        },
                        Vec::new(),
                    ));
                } else {
                    // We can get here in two cases:
                    //   - we have mix of trees and leaves.
                    //   - all of the parents are leaves, but we have changes that need to be
                    //     applied to it current nodes subentries.
                    // both of this situation result in unresolvable conflict.
                    let error = format_err!(
                        "Unresolved conflict at:\npath: {path:?}\nparents: {parents:?}"
                    );
                    return Err(error);
                }
            }
        },
        Some(Change::Remove) => {
            // Remove associated Leaf entr{y|ies}, leaving only trees.
            // This case is used to either remove leaf entry or resolve file/tree conflict
            // in favour of tree merge.
            parents.into_iter().filter_map(Entry::into_tree).collect()
        }
        Some(Change::Add(leaf)) => {
            // Replace current merge node with a leaf, and stop traversal.
            // This case is used to either replace leaf entry or resolve file/tree conflict
            // in favour or file.
            return Ok((
                MergeResult::CreateLeaf {
                    change: Some(leaf),
                    name,
                    path: path
                        .into_optional_non_root_path()
                        .expect("leaf can not have empty path"),
                    parents: parents.into_iter().filter_map(Entry::into_leaf).collect(),
                },
                Vec::new(),
            ));
        }
    };

    let MergeSubentriesResult {
        reused_maps,
        merge_nodes,
    } = if let Some(maps) = known_children {
        let mut reused_maps = Vec::new();
        for map in maps.load().await? {
            let (entry, children) = map.expand(&ctx, &store).await?;
            ensure!(entry.is_none(), "manifest child name must not be empty");
            reused_maps.extend(
                children
                    .into_iter()
                    .map(|(byte, map)| (smallvec![byte], map)),
            );
        }
        MergeSubentriesResult {
            reused_maps,
            merge_nodes: Vec::new(),
        }
    } else {
        if parent_subtrees.is_empty() && subentries.is_empty() {
            // All elements of this merge tree have been deleted.
            // Nothing left to do apart from indicating that this node needs to be removed
            // from its parent.
            return Ok((MergeResult::Delete, Vec::new()));
        }

        // Fetch parent trees and merge them.
        borrowed!(ctx, store);
        let parent_manifests_trie_maps =
            future::try_join_all(parent_subtrees.iter().map(move |tree_id| {
                cloned!(ctx);
                async move {
                    tree_id
                        .load(&ctx, store)
                        .await?
                        .into_trie_map(&ctx, store)
                        .await
                }
            }))
            .await?;

        merge_subentries(
            ctx,
            store,
            &path,
            subentries,
            parent_manifests_trie_maps,
            known_entries_subentries,
        )
        .await?
    };

    Ok((
        MergeResult::CreateTree {
            name,
            path,
            parents: parent_subtrees,
            reused_maps,
        },
        merge_nodes,
    ))
}

struct MergeSubentriesNode<'a, TreeId, Leaf, LeafChange, TrieMapType> {
    path: &'a MPath,
    prefix: SmallVec<[u8; 24]>,
    changes: PrefixTree<PathTree<Option<Change<LeafChange>>>>,
    parents: Vec<TrieMapType>,
    known_entries: PrefixTree<PathTree<Option<KnownEntry<TreeId, Leaf, TrieMapType>>>>,
}

struct MergeSubentriesResult<TreeId, Leaf, LeafChange, TrieMapType> {
    reused_maps: Vec<(SmallVec<[u8; 24]>, TrieMapType)>,
    merge_nodes: Vec<MergeNode<TreeId, Leaf, LeafChange, TrieMapType>>,
}

async fn merge_subentries<TreeId, Leaf, LeafChange, TrieMapType, Store>(
    ctx: &CoreContext,
    store: &Store,
    path: &MPath,
    changes: PrefixTree<PathTree<Option<Change<LeafChange>>>>,
    parents: Vec<TrieMapType>,
    known_entries: PrefixTree<PathTree<Option<KnownEntry<TreeId, Leaf, TrieMapType>>>>,
) -> Result<MergeSubentriesResult<TreeId, Leaf, LeafChange, TrieMapType>>
where
    TrieMapType: TrieMapOps<Store, Entry<TreeId, Leaf>> + Send,
    Store: Sync + Send,
    TreeId: Send + Clone,
    Leaf: Send + Clone,
    LeafChange: Send + Clone,
{
    bounded_traversal::bounded_traversal(
        256,
        MergeSubentriesNode {
            path,
            prefix: smallvec![],
            changes,
            parents,
            known_entries,
        },
        move |MergeSubentriesNode::<_, _, _, _> {
                  path,
                  prefix,
                  changes,
                  parents,
                  known_entries,
              }| {
            async move {
                // If there are no changes and only one parent then we can reuse the parent's map.
                // TODO(youssefsalama): In case of multiple identical parent maps, reuse one of their maps. This
                // will only become possible once sharded map nodes are extended with aggregate information.
                if changes.is_empty() && parents.len() <= 1 {
                    return Ok((
                        MergeSubentriesResult {
                            reused_maps: parents
                                .into_iter()
                                .next()
                                .map(|parent| (prefix, parent))
                                .into_iter()
                                .collect(),
                            merge_nodes: vec![],
                        },
                        vec![],
                    ));
                }

                // Expand changes and parent maps by the first byte, group changes and parent subentries that
                // correspond to the current prefix into current_merge_node, then recurse on changes and parent
                // maps that start with each byte, accumulating the resulting merge nodes and reused maps.

                let mut child_merge_subentries_nodes: BTreeMap<
                    u8,
                    MergeSubentriesNode<_, _, _, _>,
                > = Default::default();
                let mut current_merge_node = None;

                let (current_change, child_changes) = changes.expand();

                if let Some(current_change) = current_change {
                    let name = MPathElement::new_from_slice(&prefix)?;
                    current_merge_node = Some(MergeNode {
                        path: path.join_element(Some(&name)),
                        name: Some(name),
                        changes: current_change,
                        parents: Default::default(),
                        known_entries: Default::default(),
                    })
                }

                for (next_byte, changes) in child_changes {
                    child_merge_subentries_nodes
                        .entry(next_byte)
                        .or_insert_with(|| MergeSubentriesNode {
                            path,
                            prefix: prefix
                                .iter()
                                .copied()
                                .chain(std::iter::once(next_byte))
                                .collect(),
                            changes: Default::default(),
                            parents: Default::default(),
                            known_entries: Default::default(),
                        })
                        .changes = changes;
                }

                for parent in parents {
                    let (current_entry, child_trie_maps) = parent.expand(ctx, store).await?;

                    if let Some(current_entry) = current_entry {
                        let name = MPathElement::new_from_slice(&prefix)?;

                        current_merge_node
                            .get_or_insert_with(|| MergeNode {
                                path: path.join(Some(&name)),
                                name: Some(name),
                                changes: Default::default(),
                                parents: Default::default(),
                                known_entries: Default::default(),
                            })
                            .parents
                            .push(current_entry.clone());
                    }

                    for (next_byte, trie_map) in child_trie_maps {
                        child_merge_subentries_nodes
                            .entry(next_byte)
                            .or_insert_with(|| MergeSubentriesNode {
                                path,
                                prefix: prefix
                                    .iter()
                                    .copied()
                                    .chain(std::iter::once(next_byte))
                                    .collect(),
                                changes: Default::default(),
                                parents: Default::default(),
                                known_entries: Default::default(),
                            })
                            .parents
                            .push(trie_map);
                    }
                }

                let (current_known_entries, child_known_entries) = known_entries.expand();

                if let Some(current_known_entries) = current_known_entries {
                    let name = MPathElement::new_from_slice(&prefix)?;

                    current_merge_node
                        .get_or_insert_with(|| MergeNode {
                            path: path.join_element(Some(&name)),
                            name: Some(name),
                            changes: Default::default(),
                            parents: Default::default(),
                            known_entries: Default::default(),
                        })
                        .known_entries = current_known_entries;
                }

                for (next_byte, known_entries) in child_known_entries {
                    child_merge_subentries_nodes
                        .entry(next_byte)
                        .or_insert_with(|| MergeSubentriesNode {
                            path,
                            prefix: prefix
                                .iter()
                                .copied()
                                .chain(std::iter::once(next_byte))
                                .collect(),
                            changes: Default::default(),
                            parents: Default::default(),
                            known_entries: Default::default(),
                        })
                        .known_entries = known_entries;
                }

                Ok((
                    MergeSubentriesResult {
                        reused_maps: vec![],
                        merge_nodes: current_merge_node.into_iter().collect::<Vec<_>>(),
                    },
                    child_merge_subentries_nodes.into_values().collect(),
                ))
            }
            .boxed()
        },
        |mut result,
         child_results: std::iter::Flatten<
            std::vec::IntoIter<Option<MergeSubentriesResult<_, _, _, _>>>,
        >| {
            async {
                for child_result in child_results {
                    result.reused_maps.extend(child_result.reused_maps);
                    result.merge_nodes.extend(child_result.merge_nodes);
                }
                Ok(result)
            }
            .boxed()
        },
    )
    .await
}
