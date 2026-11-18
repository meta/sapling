/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;

use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use borrowed::borrowed;
use cloned::cloned;
use context::CoreContext;
use futures::FutureExt;
use futures::StreamExt;
use futures::TryFutureExt;
use futures::TryStreamExt;
use futures::future;
use futures::future::BoxFuture;
use futures::pin_mut;
use futures::stream;
use futures::stream::BoxStream;
use futures::stream::Stream;
use mononoke_macros::mononoke;
use mononoke_types::MPathElement;
use mononoke_types::NonRootMPath;
use mononoke_types::path::MPath;
use tokio_util::task::AbortOnDropHandle;

use crate::Entry;
use crate::Manifest;
use crate::PathOrPrefix;
use crate::PathTree;
use crate::StoreLoadable;
use crate::TrieMapOps;
use crate::comparison::classify_child;
use crate::comparison::diff_manifest_node;
use crate::comparison::diff_trie_children;
use crate::select::Select;
use crate::select::select_path_tree;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Diff<Entry> {
    Added(MPath, Entry),
    Removed(MPath, Entry),
    Changed(MPath, Entry, Entry),
}

impl<Entry> Diff<Entry> {
    pub fn path(&self) -> &MPath {
        match self {
            Diff::Added(path, _) => path,
            Diff::Removed(path, _) => path,
            Diff::Changed(path, _, _) => path,
        }
    }
}

pub trait ManifestOps<Store>
where
    Store: Sync + Send + Clone + 'static,
    Self: StoreLoadable<Store> + Clone + Send + Sync + Eq + Unpin + 'static,
    <Self as StoreLoadable<Store>>::Value: Manifest<Store, TreeId = Self> + Send + Sync,
    <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf: Clone + Send + Eq + Unpin,
{
    fn find_entries_filtered<I, P, F>(
        &self,
        ctx: CoreContext,
        store: Store,
        paths_or_prefixes: I,
        filter: F,
    ) -> BoxStream<
        'static,
        Result<
            (
                MPath,
                Entry<Self, <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf>,
            ),
            Error,
        >,
    >
    where
        I: IntoIterator<Item = P>,
        PathOrPrefix: From<P>,
        F: Fn(&MPath, Self) -> bool + Clone + Send + Sync + 'static,
    {
        find_entries_from_roots(
            ctx,
            store,
            vec![(
                self.clone(),
                select_path_tree(paths_or_prefixes),
                MPath::ROOT,
                false,
            )],
            filter,
        )
    }

    fn find_entries<I, P>(
        &self,
        ctx: CoreContext,
        store: Store,
        paths_or_prefixes: I,
    ) -> BoxStream<
        'static,
        Result<
            (
                MPath,
                Entry<Self, <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf>,
            ),
            Error,
        >,
    >
    where
        I: IntoIterator<Item = P>,
        PathOrPrefix: From<P>,
    {
        self.find_entries_filtered(ctx, store, paths_or_prefixes, |_, _| true)
    }

    fn find_entry(
        &self,
        ctx: CoreContext,
        store: Store,
        path: MPath,
    ) -> BoxFuture<
        'static,
        Result<
            Option<Entry<Self, <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf>>,
            Error,
        >,
    > {
        self.find_entries(ctx, store, Some(PathOrPrefix::Path(path)))
            .into_future()
            .map(|(first, _rest)| Ok(first.transpose()?.map(|(_path, entry)| entry)))
            .boxed()
    }

    fn list_all_entries(
        &self,
        ctx: CoreContext,
        store: Store,
    ) -> BoxStream<
        'static,
        Result<
            (
                MPath,
                Entry<Self, <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf>,
            ),
            Error,
        >,
    > {
        self.find_entries(ctx, store, vec![PathOrPrefix::Prefix(MPath::ROOT)])
    }

    fn list_leaf_entries(
        &self,
        ctx: CoreContext,
        store: Store,
    ) -> BoxStream<
        'static,
        Result<
            (
                NonRootMPath,
                <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf,
            ),
            Error,
        >,
    > {
        self.list_all_entries(ctx, store)
            .filter_map(|result| {
                let maybe_leaf = match result {
                    Ok((path, Entry::Leaf(filenode_id))) => match NonRootMPath::try_from(path) {
                        Ok(path) => Some(Ok((path, filenode_id))),
                        Err(e) => Some(Err(e)),
                    },
                    Err(err) => Some(Err(err)),
                    _ => None,
                };
                future::ready(maybe_leaf)
            })
            .boxed()
    }

    fn list_leaf_entries_under(
        &self,
        ctx: CoreContext,
        store: Store,
        prefixes: impl IntoIterator<Item = NonRootMPath>,
    ) -> BoxStream<
        'static,
        Result<
            (
                NonRootMPath,
                <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf,
            ),
            Error,
        >,
    > {
        self.find_entries(
            ctx,
            store,
            prefixes
                .into_iter()
                .map(|pref| PathOrPrefix::Prefix(pref.into())),
        )
        .filter_map(|result| {
            let maybe_leaf = match result {
                Ok((path, Entry::Leaf(filenode_id))) => match NonRootMPath::try_from(path) {
                    Ok(path) => Some(Ok((path, filenode_id))),
                    Err(e) => Some(Err(e)),
                },
                Err(err) => Some(Err(err)),
                _ => None,
            };
            future::ready(maybe_leaf)
        })
        .boxed()
    }

    fn list_tree_entries(
        &self,
        ctx: CoreContext,
        store: Store,
    ) -> BoxStream<
        'static,
        Result<
            (
                MPath,
                <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::TreeId,
            ),
            Error,
        >,
    > {
        self.list_all_entries(ctx, store)
            .filter_map(|result| {
                let maybe_tree = match result {
                    Ok((path, Entry::Tree(tree_id))) => Some(Ok((path, tree_id))),
                    Err(err) => Some(Err(err)),
                    _ => None,
                };
                future::ready(maybe_tree)
            })
            .boxed()
    }

    /// Returns differences between two manifests.
    ///
    /// `self` is considered the "old" manifest (so entries missing there are "Added")
    /// `other` is considered the "new" manifest (so entries missing there are "Removed")
    fn diff(
        &self,
        ctx: CoreContext,
        store: Store,
        other: Self,
    ) -> BoxStream<
        'static,
        Result<
            Diff<Entry<Self, <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf>>,
            Error,
        >,
    >
    where
        <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf: Sync,
        <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::TrieMapType: TrieMapOps<
                Store,
                Entry<Self, <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf>,
            > + Eq,
    {
        self.filtered_diff(
            ctx,
            store.clone(),
            other,
            store,
            Some,
            |_| true,
            Default::default(),
        )
    }

    /// Do a diff, but with knobs to filter_map output and prune some subtrees.
    /// `output_filter` lets us configure what will be returned from filtered_diff. it accepts
    /// every diff entry and returns `Option<Out>`, so it acts similar to `filter_map()` function
    /// recurse_pruner is a function that allows us to skip iterating over some subtrees
    ///
    /// Identical sub-shards are pruned by their content-addressed IDs without
    /// loading them. Manifest replacements only disable pruning along the trie
    /// paths leading to those replacements. Results are unordered.
    fn filtered_diff<FilterMap, Out, RecursePruner>(
        &self,
        ctx: CoreContext,
        store: Store,
        other: Self,
        other_store: Store,
        output_filter: FilterMap,
        recurse_pruner: RecursePruner,
        manifest_replacements: HashMap<
            MPath,
            Entry<Self, <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf>,
        >,
    ) -> BoxStream<'static, Result<Out, Error>>
    where
        FilterMap: Fn(
                Diff<Entry<Self, <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf>>,
            ) -> Option<Out>
            + Clone
            + Send
            + 'static,
        RecursePruner: Fn(&Diff<Self>) -> bool + Clone + Send + 'static,
        Out: Send + 'static,
        <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf: Sync,
        <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::TrieMapType: TrieMapOps<
                Store,
                Entry<Self, <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf>,
            > + Eq,
    {
        let PathTree {
            value: replacement,
            subentries: child_replacements,
        } = PathTree::from_iter(
            manifest_replacements
                .into_iter()
                .map(|(path, entry)| (path, Some(entry))),
        );
        let this = match replacement {
            None => self.clone(),
            Some(Entry::Tree(replacement)) => replacement,
            Some(Entry::Leaf(_)) => {
                return stream::once(async move {
                    Err(anyhow!(
                        "Manifest replacement at root which resolves to a leaf"
                    ))
                })
                .boxed();
            }
        };

        if this == other && child_replacements.is_empty() {
            return stream::empty().boxed();
        }

        let init = Some((Diff::Changed(MPath::ROOT, this, other), child_replacements));

        bounded_traversal::bounded_traversal_stream(256, init, move |(work, replacements)| {
            cloned!(ctx, output_filter, recurse_pruner, store, other_store);
            async move {
                let (outs, recurse) = diff_manifest_node(
                    &ctx,
                    &store,
                    &other_store,
                    work,
                    replacements,
                    recurse_pruner,
                )
                .await?;
                let outs: Vec<Out> = outs.into_iter().filter_map(&output_filter).collect();
                anyhow::Ok((outs, recurse))
            }
            .boxed()
        })
        .map_ok(|entries| stream::iter(entries.into_iter().map(Ok)))
        .try_flatten()
        .boxed()
    }
}

fn find_entries_from_roots<TreeId, Store, F>(
    ctx: CoreContext,
    store: Store,
    roots: Vec<(TreeId, PathTree<Select>, MPath, bool)>,
    filter: F,
) -> BoxStream<
    'static,
    Result<(
        MPath,
        Entry<TreeId, <TreeId::Value as Manifest<Store>>::Leaf>,
    )>,
>
where
    Store: Clone + Send + Sync + 'static,
    TreeId: StoreLoadable<Store> + Clone + Send + Sync + Eq + Unpin + 'static,
    TreeId::Value: Manifest<Store, TreeId = TreeId> + Send + Sync,
    <TreeId::Value as Manifest<Store>>::Leaf: Clone + Send + Eq + Unpin,
    F: Fn(&MPath, TreeId) -> bool + Clone + Send + Sync + 'static,
{
    (async_stream::stream! {
        let store = &store;
        borrowed!(ctx, store, filter);
        let s = bounded_traversal::bounded_traversal_stream(
            256,
            roots,
            move |(manifest_id, selector, path, recursive)| {
                let (select, subentries) = selector.deconstruct();
                cloned!(ctx, store, filter);
                async move {
                    if !filter(&path, manifest_id.clone()) {
                        return Ok((Vec::new(), Vec::new()));
                    }
                    // DO NOT REMOVE THIS SPAWN: Git boundary filtering has a downstream
                    // async buffer that can stop polling this traversal while full.
                    // Its metadata lookups may depend on in-flight directory reads;
                    // those reads must keep progressing to avoid a subexecutor deadlock.
                    AbortOnDropHandle::new(mononoke::spawn_task(async move {
                        let manifest = manifest_id.load(&ctx, &store).await?;
                        let mut output = Vec::new();
                        let mut recurse = Vec::new();
                        if recursive || select.is_recursive() {
                            output.push((path.clone(), Entry::Tree(manifest_id)));
                            let mut stream = manifest.list(&ctx, &store).await?;
                            while let Some((name, entry)) = stream.try_next().await? {
                                let path = path.join(&name);
                                match entry {
                                    Entry::Leaf(_) => {
                                        output.push((path.clone(), entry));
                                    }
                                    Entry::Tree(manifest_id) => {
                                        recurse.push((manifest_id, Default::default(), path, true));
                                    }
                                }
                            }
                        } else {
                            if select.is_selected() {
                                output.push((path.clone(), Entry::Tree(manifest_id)));
                            }
                            for (name, selector) in subentries {
                                if let Some(entry) = manifest.lookup(&ctx, &store, &name).await? {
                                    let path = path.join(&name);
                                    match entry {
                                        Entry::Leaf(_) => {
                                            if selector.value.is_selected() {
                                                output.push((path.clone(), entry));
                                            }
                                        }
                                        Entry::Tree(manifest_id) => {
                                            recurse.push((manifest_id, selector, path, false));
                                        }
                                    }
                                }
                            }
                        }

                        Ok::<_, Error>((output, recurse))
                    })).await?
                }.boxed()
            },
        )
        .map_ok(|entries| stream::iter(entries.into_iter().map(Ok)))
        .try_flatten();

        pin_mut!(s);
        while let Some(value) = s.next().await {
            yield value;
        }
    })
    .boxed()
}

/// Find entries below a directory from its supplied child map. Paths are relative
/// to that directory; the directory itself has no entry in this view.
pub fn find_entries_from_subentries<TreeId, Leaf, Store, Map>(
    ctx: CoreContext,
    store: Store,
    map: Map,
    paths: impl IntoIterator<Item = PathOrPrefix>,
) -> BoxStream<'static, Result<(MPath, Entry<TreeId, Leaf>)>>
where
    Store: Clone + Send + Sync + 'static,
    TreeId: StoreLoadable<Store> + Clone + Send + Sync + Eq + Unpin + 'static,
    TreeId::Value: Manifest<Store, TreeId = TreeId, Leaf = Leaf> + Send + Sync,
    Leaf: Clone + Send + Sync + Eq + Unpin + 'static,
    Map: TrieMapOps<Store, Entry<TreeId, Leaf>> + Clone + Send + Sync + 'static,
{
    let (selection, children) = select_path_tree(paths).deconstruct();
    async_stream::try_stream! {
        let mut children: BoxStream<'_, Result<_>> = if selection.is_recursive() {
            map.into_stream(&ctx, &store).await?
                .map_ok(|(name, entry)| {
                    Ok((MPathElement::from_smallvec(name)?, entry,
                        PathTree { value: Select::Recursive, ..Default::default() }))
                })
                .and_then(future::ready)
                .boxed()
        } else {
            let names = children.iter().map(|(name, _)| name.clone().to_smallvec()).collect();
            let mut entries: HashMap<_, _> = map.lookup_many(&ctx, &store, names).await?.into_iter().collect();
            stream::iter(children.into_iter().filter_map(move |(name, selector)| {
                entries.remove(name.as_ref()).map(|entry| Ok((name, entry, selector)))
            })).boxed()
        };
        let mut roots = Vec::new();
        while let Some((name, entry, selector)) = children.try_next().await? {
            let path = MPath::ROOT.join(&name);
            match entry {
                Entry::Tree(tree) => roots.push((tree, selector, path, false)),
                Entry::Leaf(_) if selector.value.is_selected() => yield (path, entry),
                Entry::Leaf(_) => {},
            }
        }
        drop(children);
        let mut entries = find_entries_from_roots(ctx, store, roots, |_, _| true);
        while let Some(entry) = entries.try_next().await? {
            yield entry;
        }
    }
    .boxed()
}

/// Finds subentries in mf_id manifest that are different from entries with the same name in
/// every manifest in `diff_against`. Note that removed entries ARE NOT INCLUDED!
/// F. e. if file 'A' hash HASH_1 in mf_if, HASH_2 and HASH_3 in diff_against, then it will
/// be returned. But if file 'A' has HASH_2 then it wont' be returned because it matches
/// HASH_2 in diff_against.
/// This implementation is more efficient for merges.
pub fn find_intersection_of_diffs<TreeId, Leaf, Store>(
    ctx: CoreContext,
    store: Store,
    mf_id: TreeId,
    diff_against: Vec<TreeId>,
) -> impl Stream<Item = Result<(MPath, Entry<TreeId, Leaf>), Error>> + 'static
where
    Store: Sync + Send + Clone + 'static,
    TreeId: StoreLoadable<Store> + Clone + Send + Sync + Eq + Unpin + 'static,
    <TreeId as StoreLoadable<Store>>::Value:
        Manifest<Store, TreeId = TreeId, Leaf = Leaf> + Send + Sync,
    Leaf: Clone + Send + Sync + Eq + Unpin + 'static,
    <<TreeId as StoreLoadable<Store>>::Value as Manifest<Store>>::TrieMapType:
        TrieMapOps<Store, Entry<TreeId, Leaf>> + Eq,
{
    find_intersection_of_diffs_and_parents(ctx, store, mf_id, diff_against)
        .map_ok(|(path, entry, _)| (path, entry))
}

/// Like `find_intersection_of_diffs`, but `recurse_pruner` gates recursion into
/// subtrees in both the parented and parentless branches; rejected subtrees are
/// never loaded. The pruner only sees trees, so it cannot filter emitted leaf
/// entries -- callers that must exclude leaf-valued paths have to post-filter.
pub fn find_intersection_of_diffs_pruned<TreeId, Leaf, Store, RecursePruner>(
    ctx: CoreContext,
    store: Store,
    mf_id: TreeId,
    diff_against: Vec<TreeId>,
    recurse_pruner: RecursePruner,
) -> impl Stream<Item = Result<(MPath, Entry<TreeId, Leaf>), Error>> + 'static
where
    Store: Sync + Send + Clone + 'static,
    TreeId: StoreLoadable<Store> + Clone + Send + Sync + Eq + Unpin + 'static,
    <TreeId as StoreLoadable<Store>>::Value:
        Manifest<Store, TreeId = TreeId, Leaf = Leaf> + Send + Sync,
    Leaf: Clone + Send + Sync + Eq + Unpin + 'static,
    <<TreeId as StoreLoadable<Store>>::Value as Manifest<Store>>::TrieMapType:
        TrieMapOps<Store, Entry<TreeId, Leaf>> + Eq,
    RecursePruner: Fn(&Diff<TreeId>) -> bool + Clone + Send + Sync + 'static,
{
    find_intersection_of_diffs_and_parents_pruned(ctx, store, mf_id, diff_against, recurse_pruner)
        .map_ok(|(path, entry, _)| (path, entry))
}

/// Like `find_intersection_of_diffs` but for each returned entry it also returns diff_against
/// entries with the same path.
pub fn find_intersection_of_diffs_and_parents<TreeId, Leaf, Store>(
    ctx: CoreContext,
    store: Store,
    mf_id: TreeId,
    diff_against: Vec<TreeId>,
) -> impl Stream<Item = Result<(MPath, Entry<TreeId, Leaf>, Vec<Entry<TreeId, Leaf>>), Error>> + 'static
where
    Store: Sync + Send + Clone + 'static,
    TreeId: StoreLoadable<Store> + Clone + Send + Sync + Eq + Unpin + 'static,
    <TreeId as StoreLoadable<Store>>::Value:
        Manifest<Store, TreeId = TreeId, Leaf = Leaf> + Send + Sync,
    Leaf: Clone + Send + Sync + Eq + Unpin + 'static,
    <<TreeId as StoreLoadable<Store>>::Value as Manifest<Store>>::TrieMapType:
        TrieMapOps<Store, Entry<TreeId, Leaf>> + Eq,
{
    find_intersection_of_diffs_and_parents_pruned(ctx, store, mf_id, diff_against, |_| true)
}

/// Like `find_intersection_of_diffs_and_parents`, but prunes subtrees for which
/// `recurse_pruner` returns false.
pub fn find_intersection_of_diffs_and_parents_pruned<TreeId, Leaf, Store, RecursePruner>(
    ctx: CoreContext,
    store: Store,
    mf_id: TreeId,
    diff_against: Vec<TreeId>,
    recurse_pruner: RecursePruner,
) -> impl Stream<Item = Result<(MPath, Entry<TreeId, Leaf>, Vec<Entry<TreeId, Leaf>>), Error>> + 'static
where
    Store: Sync + Send + Clone + 'static,
    TreeId: StoreLoadable<Store> + Clone + Send + Sync + Eq + Unpin + 'static,
    <TreeId as StoreLoadable<Store>>::Value:
        Manifest<Store, TreeId = TreeId, Leaf = Leaf> + Send + Sync,
    Leaf: Clone + Send + Sync + Eq + Unpin + 'static,
    <<TreeId as StoreLoadable<Store>>::Value as Manifest<Store>>::TrieMapType:
        TrieMapOps<Store, Entry<TreeId, Leaf>> + Eq,
    RecursePruner: Fn(&Diff<TreeId>) -> bool + Clone + Send + Sync + 'static,
{
    match diff_against.first().cloned() {
        Some(parent) => async move {
            mononoke::spawn_task(async move {
                let new_entries: Vec<_> = parent
                    .filtered_diff(
                        ctx.clone(),
                        store.clone(),
                        mf_id,
                        store.clone(),
                        |diff| match diff {
                            Diff::Added(path, entry) => Some((path, entry, vec![])),
                            Diff::Changed(path, parent_entry, entry) => {
                                Some((path, entry, vec![parent_entry]))
                            }
                            Diff::Removed(..) => None,
                        },
                        move |diff| !matches!(diff, Diff::Removed(..)) && recurse_pruner(diff),
                        Default::default(),
                    )
                    .try_collect()
                    .await?;

                let paths: Vec<_> = new_entries
                    .iter()
                    .map(|(path, _, _)| path.clone())
                    .collect();

                let futs = diff_against.into_iter().skip(1).map(move |p| {
                    p.find_entries(ctx.clone(), store.clone(), paths.clone())
                        .try_collect::<HashMap<_, _>>()
                });
                let entries_in_parents = future::try_join_all(futs).await?;

                let mut res = vec![];
                for (path, unode, mut parent_entries) in new_entries {
                    let mut new_entry = true;
                    for p in &entries_in_parents {
                        if let Some(parent_entry) = p.get(&path) {
                            if parent_entry == &unode {
                                new_entry = false;
                                break;
                            } else {
                                parent_entries.push(parent_entry.clone());
                            }
                        }
                    }

                    if new_entry {
                        res.push((path, unode, parent_entries));
                    }
                }

                Ok(stream::iter(res.into_iter().map(Ok)))
            })
            .await?
        }
        .try_flatten_stream()
        .right_stream(),
        None => {
            let recurse_pruner = recurse_pruner.clone();
            mf_id
                .find_entries_filtered(
                    ctx,
                    store,
                    vec![PathOrPrefix::Prefix(MPath::ROOT)],
                    move |path: &MPath, tree_id| {
                        recurse_pruner(&Diff::Added(path.clone(), tree_id))
                    },
                )
                .map_ok(|(path, entry)| (path, entry, vec![]))
                .left_stream()
        }
    }
}

/// Entries in a child map that differ from every parent, including descendants.
/// `keep` applies to both emitted entries and traversal into directories.
pub fn find_intersection_of_subentry_diffs<TreeId, Leaf, Store, M, Map, Keep>(
    ctx: CoreContext,
    store: Store,
    current: Map,
    parents: Vec<Map>,
    keep: Keep,
) -> BoxStream<'static, Result<(MPath, Entry<TreeId, Leaf>)>>
where
    Store: Clone + Send + Sync + 'static,
    TreeId: StoreLoadable<Store, Value = M> + Clone + Send + Sync + Eq + Unpin + 'static,
    M: Manifest<Store, TreeId = TreeId, Leaf = Leaf, TrieMapType = Map> + Send + Sync,
    Leaf: Clone + Send + Sync + Eq + Unpin + 'static,
    Map: TrieMapOps<Store, Entry<TreeId, Leaf>> + Clone + Eq + Send + Sync + 'static,
    Keep: Fn(&MPath) -> bool + Clone + Send + Sync + 'static,
{
    async move {
        mononoke::spawn_task(async move {
            let children = diff_trie_children(
                &ctx,
                &store,
                Some(current),
                &store,
                parents.first().cloned(),
                Default::default(),
            )
            .await?;
            let mut leaves = Vec::new();
            let mut trees = Vec::new();
            for (name, old, new) in children {
                let path = MPath::ROOT.join_element(Some(&name));
                if !keep(&path) {
                    continue;
                }
                let (leaf, tree) = classify_child(path, old, new);
                leaves.extend(leaf);
                trees.extend(tree.filter(|diff| !matches!(diff, Diff::Removed(..))));
            }
            let traversal = bounded_traversal::bounded_traversal_stream(256, trees, {
                cloned!(ctx, store, keep);
                move |work| {
                    cloned!(ctx, store, keep);
                    async move {
                        let (outputs, children) = diff_manifest_node(
                            &ctx,
                            &store,
                            &store,
                            work,
                            Default::default(),
                            |diff| !matches!(diff, Diff::Removed(..)) && keep(diff.path()),
                        )
                        .await?;
                        Ok::<_, Error>((outputs, children.into_iter().map(|(work, _)| work)))
                    }
                    .boxed()
                }
            })
            .map_ok(|outputs| stream::iter(outputs.into_iter().map(Ok::<_, Error>)))
            .try_flatten();
            let entries: Vec<_> = stream::iter(leaves.into_iter().map(Ok))
                .chain(traversal)
                .try_filter_map(|diff| {
                    future::ready(Ok(match diff {
                        Diff::Added(path, entry) | Diff::Changed(path, _, entry) if keep(&path) => {
                            Some((path, entry))
                        }
                        _ => None,
                    }))
                })
                .try_collect()
                .await?;
            let paths: Vec<_> = entries
                .iter()
                .map(|(path, _)| PathOrPrefix::Path(path.clone()))
                .collect();
            let other_parents = future::try_join_all(parents.into_iter().skip(1).map(|parent| {
                find_entries_from_subentries(ctx.clone(), store.clone(), parent, paths.clone())
                    .try_collect::<HashMap<_, _>>()
            }))
            .await?;
            Ok::<_, Error>(stream::iter(
                entries
                    .into_iter()
                    .filter(move |(path, entry)| {
                        other_parents
                            .iter()
                            .all(|parent| parent.get(path) != Some(entry))
                    })
                    .map(Ok),
            ))
        })
        .await?
    }
    .try_flatten_stream()
    .boxed()
}

impl<TreeId, Store> ManifestOps<Store> for TreeId
where
    Store: Sync + Send + Clone + 'static,
    Self: StoreLoadable<Store> + Clone + Send + Sync + Eq + Unpin + 'static,
    <Self as StoreLoadable<Store>>::Value: Manifest<Store, TreeId = Self> + Send + Sync,
    <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf: Send + Clone + Eq + Unpin,
{
}
