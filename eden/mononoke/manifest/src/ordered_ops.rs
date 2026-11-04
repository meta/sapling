/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;

use anyhow::Error;
use anyhow::anyhow;
use borrowed::borrowed;
use bounded_traversal::OrderedTraversal;
use context::CoreContext;
use futures::future::FutureExt;
use futures::pin_mut;
use futures::stream;
use futures::stream::BoxStream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use futures_watchdog::WatchdogExt;
use mononoke_types::MPathElement;
use mononoke_types::path::MPath;
use nonzero_ext::nonzero;

use crate::Entry;
use crate::Manifest;
use crate::OrderedManifest;
use crate::PathOrPrefix;
use crate::PathTree;
use crate::StoreLoadable;
use crate::TrieMapOps;
use crate::comparison::classify_child;
use crate::comparison::diff_weighted_children;
use crate::ops::Diff;
use crate::select::select_path_tree;
use crate::types::Weight;

/// Track where we are relative to the `after` parameter.
pub enum After {
    /// Include everything.
    All,

    /// Include all contents, but omit the directory itself.
    AllContents,

    /// Include everything in this directory after the named element and the
    /// subpath within that element.
    After(MPathElement, MPath),
}

impl From<Option<MPath>> for After {
    fn from(path: Option<MPath>) -> Self {
        path.map_or(After::All, |p| After::new(&p))
    }
}

impl After {
    fn new(mpath: &MPath) -> Self {
        match mpath.split_first() {
            None => After::AllContents,
            Some((elem, rest)) => After::After(elem.clone(), rest),
        }
    }

    /// Returns true if this element should be skipped entirely.
    ///
    /// We don't skip entries that match exactly, even though they themselves
    /// will not be included.  If the element name matches then we still want
    /// to descend into subdirectories.
    pub fn skip(&self, name: &MPathElement) -> bool {
        match self {
            After::All | After::AllContents => false,
            After::After(elem, _) => name < elem,
        }
    }

    /// Returns true if this directory itself should be included.
    pub fn include_self(&self) -> bool {
        match self {
            After::All => true,
            After::AllContents | After::After(..) => false,
        }
    }

    /// Returns true if a file with the given name in this directory should be
    /// included.
    pub fn include_file(&self, name: &MPathElement) -> bool {
        match self {
            After::All | After::AllContents => true,
            After::After(elem, _) => name > elem,
        }
    }

    /// Enter a subdirectory.  The directory must be one that should be
    /// entered (i.e. skip is false).  Returns an instance of `After` suitable
    /// for the subdirectory.
    pub fn enter_dir(&self, name: &MPathElement) -> After {
        match self {
            After::All | After::AllContents => After::All,
            After::After(elem, rest) => {
                if name == elem {
                    After::new(rest)
                } else {
                    debug_assert!(name > elem);
                    After::All
                }
            }
        }
    }
}

pub trait ManifestOrderedOps<Store>
where
    Store: Sync + Send + Clone + 'static,
    Self: StoreLoadable<Store> + Clone + Send + Sync + Eq + Unpin + 'static,
    <Self as StoreLoadable<Store>>::Value:
        Manifest<Store, TreeId = Self> + OrderedManifest<Store> + Send + Sync,
    <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf: Clone + Send + Eq + Unpin,
{
    fn find_entries_ordered<I, P>(
        &self,
        ctx: CoreContext,
        store: Store,
        paths_or_prefixes: I,
        after: impl Into<After>,
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
        let selector = select_path_tree(paths_or_prefixes);

        // Schedule a maximum of 256 concurrently unfolding directories.
        let schedule_max = nonzero!(256usize);

        // Allow queueing of up to 2,560 items, which would be 10 items per
        // directory at the maximum concurrency level.  Experiments show this
        // is a good balance of queueing items while not spending too long
        // determining what can be scheduled.
        let queue_max = nonzero!(2560usize);

        let after = after.into();

        let init = Some((
            queue_max.get(),
            (self.clone(), selector, MPath::ROOT, false, after),
        ));
        (async_stream::stream! {
            borrowed!(ctx, store);
            let s = bounded_traversal::bounded_traversal_ordered_stream(
                schedule_max,
                queue_max,
                init,
                move |(manifest_id, selector, path, recursive, after)| {
                    let (select, subentries) = selector.deconstruct();

                    async move {
                        let manifest = manifest_id.load(ctx, store).await?;

                        let mut output = Vec::new();

                        if recursive || select.is_recursive() {
                            if after.include_self() {
                                output.push(OrderedTraversal::Output((
                                    path.clone(),
                                    Entry::Tree(manifest_id),
                                )));
                            }
                            let mut stream = manifest.list_weighted(ctx, store).await?;
                            while let Some((name, entry)) = stream.try_next().await? {
                                if after.skip(&name) {
                                    continue;
                                }
                                let path = path.join(&name);
                                match entry {
                                    Entry::Leaf(leaf) => {
                                        if after.include_file(&name) {
                                            output.push(OrderedTraversal::Output((
                                                path.clone(),
                                                Entry::Leaf(leaf),
                                            )));
                                        }
                                    }
                                    Entry::Tree((weight, manifest_id)) => {
                                        output.push(OrderedTraversal::Recurse(
                                            weight,
                                            (
                                                manifest_id,
                                                Default::default(),
                                                path,
                                                true,
                                                after.enter_dir(&name),
                                            ),
                                        ));
                                    }
                                }
                            }
                        } else {
                            if after.include_self() && select.is_selected() {
                                output.push(OrderedTraversal::Output((
                                    path.clone(),
                                    Entry::Tree(manifest_id),
                                )));
                            }
                            for (name, selector) in subentries {
                                if after.skip(&name) {
                                    continue;
                                }
                                if let Some(entry) = manifest.lookup_weighted(ctx, store, &name).await? {
                                    let path = path.join(&name);
                                    match entry {
                                        Entry::Leaf(leaf) => {
                                            if after.include_file(&name)
                                                && selector.value.is_selected()
                                            {
                                                output.push(OrderedTraversal::Output((
                                                    path.clone(),
                                                    Entry::Leaf(leaf),
                                                )));
                                            }
                                        }
                                        Entry::Tree((weight, manifest_id)) => {
                                            output.push(OrderedTraversal::Recurse(
                                                weight,
                                                (
                                                    manifest_id,
                                                    selector,
                                                    path,
                                                    false,
                                                    after.enter_dir(&name),
                                                ),
                                            ));
                                        }
                                    }
                                }
                            }
                        }

                        Ok::<_, Error>(output)
                    }
                    .boxed()
                },
            );

            pin_mut!(s);
            while let Some(value) = s.next().await {
                yield value;
            }
        })
        .boxed()
    }

    /// Returns ordered differences between two manifests.
    ///
    /// `self` is considered the "old" manifest (so entries missing there are "Added")
    /// `other` is considered the "new" manifest (so entries missing there are "Removed")
    fn diff_ordered(
        &self,
        ctx: CoreContext,
        store: Store,
        other: Self,
        after: Option<MPath>,
    ) -> BoxStream<
        'static,
        Result<
            Diff<Entry<Self, <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf>>,
            Error,
        >,
    >
    where
        <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf: Sync,
        <<Self as StoreLoadable<Store>>::Value as OrderedManifest<Store>>::WeightedTrieMapType:
            TrieMapOps<
                    Store,
                    Entry<
                        (Weight, Self),
                        <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf,
                    >,
                > + Eq
                + 'static,
    {
        self.filtered_diff_ordered(
            ctx,
            store.clone(),
            other,
            store,
            after,
            Some,
            |_| true,
            Default::default(),
        )
    }

    /// Do a diff, but with knobs to filter_map output and prune some subtrees.
    /// `output_filter` let's us configure what will be returned from filtered_diff. it accepts
    /// every diff entry and returns `Option<Out>`, so it acts similar to `filter_map()` function
    /// recurse_pruner is a function that allows us to skip iterating over some subtrees
    fn filtered_diff_ordered<FilterMap, Out, RecursePruner>(
        &self,
        ctx: CoreContext,
        store: Store,
        other: Self,
        other_store: Store,
        after: Option<MPath>,
        output_filter: FilterMap,
        recurse_pruner: RecursePruner,
        manifest_replacements: HashMap<
            MPath,
            Entry<(usize, Self), <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf>,
        >,
    ) -> BoxStream<'static, Result<Out, Error>>
    where
        FilterMap: Fn(
                Diff<Entry<Self, <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf>>,
            ) -> Option<Out>
            + Send
            + Sync
            + 'static,
        RecursePruner: Fn(&Diff<Self>) -> bool + Send + Sync + 'static,
        Out: Send + Unpin + 'static,
        <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf: Sync,
        <<Self as StoreLoadable<Store>>::Value as OrderedManifest<Store>>::WeightedTrieMapType:
            TrieMapOps<
                    Store,
                    Entry<
                        (Weight, Self),
                        <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf,
                    >,
                > + Eq
                + 'static,
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
            Some(Entry::Tree((_weight, replacement))) => replacement,
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

        // Schedule a maximum of 256 concurrently unfolding directories.
        let schedule_max = nonzero!(256usize);

        // Allow queueing of up to 2,560 items, which would be 10 items per
        // directory at the maximum concurrency level.  Experiments show this
        // is a good balance of queueing items while not spending too long
        // determining what can be scheduled.
        let queue_max = nonzero!(2560usize);

        let after = After::from(after);

        let init = Some((
            queue_max.get(),
            (
                Diff::Changed(MPath::ROOT, this, other),
                after,
                child_replacements,
            ),
        ));

        (async_stream::stream! {
            borrowed!(ctx, store, other_store, output_filter, recurse_pruner);

            let s = bounded_traversal::bounded_traversal_ordered_stream(
                schedule_max,
                queue_max,
                init,
                move |(input, after, replacements)| {
                    async move {
                        let mut output = Vec::new();

                        let push_output = |output: &mut Vec<_>, out| {
                            if let Some(out) = output_filter(out) {
                                output.push(OrderedTraversal::Output(out));
                            }
                        };

                        let push_recurse = |output: &mut Vec<_>, weight, recurse, after, replacements| {
                            if recurse_pruner(&recurse) {
                                output.push(OrderedTraversal::Recurse(weight, (recurse, after, replacements)));
                            }
                        };

                        if after.include_self() {
                            push_output(&mut output, match &input {
                                Diff::Changed(path, left, right) => Diff::Changed(
                                    path.clone(),
                                    Entry::Tree(left.clone()),
                                    Entry::Tree(right.clone()),
                                ),
                                Diff::Added(path, tree) => {
                                    Diff::Added(path.clone(), Entry::Tree(tree.clone()))
                                }
                                Diff::Removed(path, tree) => {
                                    Diff::Removed(path.clone(), Entry::Tree(tree.clone()))
                                }
                            });
                        }

                        let (path, left, right) = match input {
                            Diff::Changed(path, left, right) => (path, Some(left), Some(right)),
                            Diff::Added(path, tree) => (path, None, Some(tree)),
                            Diff::Removed(path, tree) => (path, Some(tree), None),
                        };

                        let entries = diff_weighted_children(
                            ctx,
                            store,
                            left.as_ref(),
                            other_store,
                            right.as_ref(),
                            replacements.clone(),
                        )
                        .watched()
                        .await?;

                        for (name, left, right) in entries {
                            tokio::task::consume_budget().await;
                            if after.skip(&name) {
                                continue;
                            }
                            let (child_output, child_recurse) =
                                classify_child(path.join(&name), left, right);
                            if let Some(out) = child_output {
                                if after.include_file(&name) {
                                    push_output(&mut output, strip_entry_weight(out));
                                }
                            }
                            if let Some(work) = child_recurse {
                                let (weight, work) = strip_weight(work);
                                let child_replacements = replacements
                                    .get(name.as_ref())
                                    .cloned()
                                    .unwrap_or_default()
                                    .subentries;
                                push_recurse(
                                    &mut output,
                                    weight,
                                    work,
                                    after.enter_dir(&name),
                                    child_replacements,
                                );
                            }
                        }

                        Ok(output)
                    }
                    .boxed()
                },
            );

            pin_mut!(s);
            while let Some(value) = s.next().watched().await {
                yield value;
            }
        })
        .boxed()
    }
}

/// Split a weighted subtree diff (produced by [`classify_child`] over
/// weighted entries) into the recursion weight and the plain `Diff<TreeId>` the
/// ordered scheduler recurses on. A changed tree approximates its weight with
/// `max` of the two sides (the theoretical maximum is the sum, but that is
/// overkill in practice).
fn strip_weight<TreeId>(diff: Diff<(Weight, TreeId)>) -> (Weight, Diff<TreeId>) {
    match diff {
        Diff::Added(path, (weight, tree)) => (weight, Diff::Added(path, tree)),
        Diff::Removed(path, (weight, tree)) => (weight, Diff::Removed(path, tree)),
        Diff::Changed(path, (left_weight, left), (right_weight, right)) => (
            left_weight.max(right_weight),
            Diff::Changed(path, left, right),
        ),
    }
}

/// Drop the weights from a leaf-level diff so it can be emitted as an ordinary
/// `Diff<Entry<TreeId, Leaf>>` (the entries are always leaves in practice).
fn strip_entry_weight<TreeId, Leaf>(
    diff: Diff<Entry<(Weight, TreeId), Leaf>>,
) -> Diff<Entry<TreeId, Leaf>> {
    let strip = |entry: Entry<(Weight, TreeId), Leaf>| entry.map_tree(|(_weight, tree)| tree);
    match diff {
        Diff::Added(path, entry) => Diff::Added(path, strip(entry)),
        Diff::Removed(path, entry) => Diff::Removed(path, strip(entry)),
        Diff::Changed(path, left, right) => Diff::Changed(path, strip(left), strip(right)),
    }
}

impl<TreeId, Store> ManifestOrderedOps<Store> for TreeId
where
    Store: Sync + Send + Clone + 'static,
    Self: StoreLoadable<Store> + Clone + Send + Sync + Eq + Unpin + 'static,
    <Self as StoreLoadable<Store>>::Value:
        Manifest<Store, TreeId = Self> + OrderedManifest<Store> + Send + Sync,
    <<Self as StoreLoadable<Store>>::Value as Manifest<Store>>::Leaf: Send + Clone + Eq + Unpin,
{
}
