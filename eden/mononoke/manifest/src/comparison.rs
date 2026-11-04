/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::iter::Peekable;

use anyhow::Result;
use blobstore::StoreLoadable;
use borrowed::borrowed;
use cloned::cloned;
use context::CoreContext;
use futures::future;
use futures::future::FutureExt;
use futures::stream;
use futures::stream::Stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use futures_watchdog::WatchdogExt;
use mononoke_types::MPath;
use mononoke_types::MPathElement;
use mononoke_types::MPathElementPrefix;
use mononoke_types::prefix_tree::PrefixTree;

use crate::Diff;
use crate::Entry;
use crate::Manifest;
use crate::OrderedManifest;
use crate::PathTree;
use crate::TrieMapOps;
use crate::types::Weight;

/// How much of the trie keyspace a comparison result covers: a single complete
/// entry, or a whole unexpanded sub-trie under a byte-prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Span<TrieMapType, V> {
    /// A single resolved entry, identified by its complete name.
    Element(MPathElement, V),
    /// A whole unexpanded sub-trie of entries sharing a byte-prefix.
    Prefix(MPathElementPrefix, TrieMapType),
}

/// Result of a multi-way comparison between a single manifest and the merge
/// of a number of base manifests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestComparison<TrieMapType, V> {
    /// The span at this path is new.
    New(Span<TrieMapType, V>),
    /// The entry at this path has changed compared to all of the bases.
    Changed(MPathElement, V, Vec<Option<V>>),
    /// The span at this path is the same as at least one of the bases (at the
    /// given index).
    Same(
        Span<TrieMapType, V>,
        /// The index of the first base manifest that this span is the same as.
        usize,
    ),
    /// The span at this path has been removed.
    Removed(Span<Vec<Option<TrieMapType>>, Vec<Option<V>>>),
}

pub async fn compare_manifest<'a, M, Store>(
    ctx: &'a CoreContext,
    blobstore: &'a Store,
    mf: M,
    base_mfs: Vec<Option<M>>,
) -> Result<
    impl Stream<Item = Result<ManifestComparison<M::TrieMapType, Entry<M::TreeId, M::Leaf>>>> + 'a,
>
where
    M: Manifest<Store>,
    M::TreeId: Send + Sync + Eq + 'static,
    M::Leaf: Send + Sync + Eq + 'static,
    M::TrieMapType: TrieMapOps<Store, Entry<M::TreeId, M::Leaf>> + Eq,
    Store: Send + Sync + 'static,
{
    let (mf_trie_map, base_mf_trie_maps) = future::try_join(
        mf.into_trie_map(ctx, blobstore),
        future::try_join_all(base_mfs.into_iter().map(|p| async move {
            match p {
                Some(p) => Ok(Some(p.into_trie_map(ctx, blobstore).await?)),
                None => Ok(None),
            }
        })),
    )
    .await?;
    compare_tries(
        ctx,
        blobstore,
        blobstore,
        Some(mf_trie_map),
        base_mf_trie_maps,
        PrefixTree::default(),
    )
}

/// Compare a trie against a number of base tries, applying `replacements` to the
/// base side as the walk descends.
///
/// Generic over the trie's value type so the ordered path can drive it with a
/// weighted view (`V = Entry<(Weight, TreeId), Leaf>`) and get the weights out of
/// the walk itself rather than looking each one up afterwards.
pub fn compare_tries<'a, T, V, Store>(
    ctx: &'a CoreContext,
    mf_store: &'a Store,
    base_store: &'a Store,
    mf_trie_map: Option<T>,
    base_mf_trie_maps: Vec<Option<T>>,
    replacements: PrefixTree<PathTree<Option<V>>>,
) -> Result<impl Stream<Item = Result<ManifestComparison<T, V>>> + 'a>
where
    T: TrieMapOps<Store, V> + Eq + Send + Sync + 'static,
    V: Eq + Send + Sync + 'static,
    Store: Send + Sync + 'static,
{
    Ok(bounded_traversal::bounded_traversal_stream(
        256,
        Some((
            MPathElementPrefix::new(),
            mf_trie_map,
            base_mf_trie_maps,
            replacements,
        )),
        {
            cloned!(ctx, mf_store, base_store);
            move |(prefix, mf_trie_map, base_mf_trie_maps, replacements): (
                MPathElementPrefix,
                Option<T>,
                Vec<Option<T>>,
                PrefixTree<PathTree<Option<V>>>,
            )| {
                cloned!(ctx, mf_store, base_store);
                async move {
                    let mf_trie_map =
                        if let Some(mf_trie_map) = mf_trie_map {
                            if replacements.is_empty() {
                                if let Some(index) = base_mf_trie_maps
                                    .iter()
                                    .position(|parent| parent.as_ref() == Some(&mf_trie_map))
                                {
                                    return anyhow::Ok((
                                        stream::iter(vec![Ok(ManifestComparison::Same(
                                            Span::Prefix(prefix, mf_trie_map),
                                            index,
                                        ))]),
                                        vec![],
                                    ));
                                }

                                if base_mf_trie_maps
                                    .iter()
                                    .all(|parent| parent.as_ref().is_none_or(TrieMapOps::is_empty))
                                {
                                    return Ok((
                                        stream::iter(vec![Ok(ManifestComparison::New(
                                            Span::Prefix(prefix, mf_trie_map),
                                        ))]),
                                        vec![],
                                    ));
                                }
                            }
                            Some(mf_trie_map)
                        } else {
                            if replacements.is_empty()
                                && !base_mf_trie_maps
                                    .iter()
                                    .all(|parent| parent.as_ref().is_none_or(TrieMapOps::is_empty))
                            {
                                return Ok((
                                    stream::iter(vec![Ok(ManifestComparison::Removed(
                                        Span::Prefix(prefix, base_mf_trie_maps),
                                    ))]),
                                    vec![],
                                ));
                            }
                            None
                        };

                    borrowed!(ctx, mf_store, base_store);
                    let ((mf_value, mf_children), expanded_base_mfs) = future::try_join(
                        async {
                            match mf_trie_map {
                                Some(mf_trie_map) => mf_trie_map.expand(ctx, mf_store).await,
                                None => anyhow::Ok((None, Vec::new())),
                            }
                        },
                        future::try_join_all(base_mf_trie_maps.into_iter().map({
                            |parent| async move {
                                match parent {
                                    Some(parent) => parent.expand(ctx, base_store).await,
                                    None => Ok((None, Vec::new())),
                                }
                            }
                        })),
                    )
                    .await?;
                    let (mut parent_values, parent_children): (Vec<_>, Vec<_>) =
                        expanded_base_mfs.into_iter().unzip();

                    let (replacement_here, replacement_children) = replacements.expand();
                    let has_descendant_replacements = replacement_here
                        .as_ref()
                        .is_some_and(|tree| !tree.subentries.is_empty());
                    if let Some(replacement) = replacement_here.and_then(|tree| tree.value) {
                        parent_values = vec![Some(replacement)];
                    }

                    let mut out = Vec::new();
                    let mut recurse = Vec::new();

                    if let Some(value) = mf_value {
                        if let Some(index) = parent_values
                            .iter()
                            .position(|parent_value| parent_value.as_ref() == Some(&value))
                        {
                            if has_descendant_replacements {
                                out.push(Ok(ManifestComparison::Changed(
                                    prefix.to_element()?,
                                    value,
                                    parent_values,
                                )));
                            } else {
                                out.push(Ok(ManifestComparison::Same(
                                    Span::Element(prefix.to_element()?, value),
                                    index,
                                )));
                            }
                        } else if parent_values.is_empty()
                            || parent_values.iter().all(Option::is_none)
                        {
                            out.push(Ok(ManifestComparison::New(Span::Element(
                                prefix.to_element()?,
                                value,
                            ))));
                        } else {
                            out.push(Ok(ManifestComparison::Changed(
                                prefix.to_element()?,
                                value,
                                parent_values,
                            )));
                        }
                    } else if !parent_values.is_empty()
                        && !parent_values.iter().all(Option::is_none)
                    {
                        out.push(Ok(ManifestComparison::Removed(Span::Element(
                            prefix.to_element()?,
                            parent_values,
                        ))));
                    }

                    let mut diff_iter =
                        DiffIter::new(mf_children, parent_children, replacement_children);

                    while let Some((ch, child_value, child_base_mfs, child_replacements)) =
                        diff_iter.next()
                    {
                        let mut prefix = prefix.clone();
                        prefix.push(ch)?;
                        recurse.push((
                            prefix,
                            child_value,
                            child_base_mfs,
                            child_replacements.unwrap_or_default(),
                        ));
                    }

                    Ok((stream::iter(out), recurse))
                }
                .boxed()
            }
        },
    )
    .try_flatten())
}

struct DiffIter<TrieMapType, R> {
    mf: Peekable<<Vec<(u8, TrieMapType)> as std::iter::IntoIterator>::IntoIter>,
    base_mfs: Vec<Peekable<<Vec<(u8, TrieMapType)> as std::iter::IntoIterator>::IntoIter>>,
    replacements: Peekable<<Vec<(u8, R)> as std::iter::IntoIterator>::IntoIter>,
}

impl<TrieMapType, R> DiffIter<TrieMapType, R> {
    fn new(
        mf: Vec<(u8, TrieMapType)>,
        base_mfs: Vec<Vec<(u8, TrieMapType)>>,
        replacements: Vec<(u8, R)>,
    ) -> Self {
        Self {
            mf: mf.into_iter().peekable(),
            base_mfs: base_mfs
                .into_iter()
                .map(|p| p.into_iter().peekable())
                .collect(),
            replacements: replacements.into_iter().peekable(),
        }
    }

    fn next(&mut self) -> Option<(u8, Option<TrieMapType>, Vec<Option<TrieMapType>>, Option<R>)> {
        let mf_next_ch = self.mf.peek().map(|(k, _)| *k);
        let min_base_mfs_next_ch = self
            .base_mfs
            .iter_mut()
            .filter_map(|p| p.peek().map(|(k, _)| *k))
            .min();
        let replacements_next_ch = self.replacements.peek().map(|(k, _)| *k);
        let next_ch = [mf_next_ch, min_base_mfs_next_ch, replacements_next_ch]
            .into_iter()
            .flatten()
            .min()?;
        let next_mf = (Some(next_ch) == mf_next_ch)
            .then(|| self.mf.next().map(|(_, v)| v))
            .flatten();
        let next_base_mfs = self
            .base_mfs
            .iter_mut()
            .map(|p| {
                (p.peek().map(|(k, _)| *k) == Some(next_ch))
                    .then(|| p.next().map(|(_, v)| v))
                    .flatten()
            })
            .collect();
        let next_replacements = (Some(next_ch) == replacements_next_ch)
            .then(|| self.replacements.next().map(|(_, v)| v))
            .flatten();
        Some((next_ch, next_mf, next_base_mfs, next_replacements))
    }
}

/// Classify a single child, given its (optional) old and new entries, into an
/// optional leaf-level `Diff` output and an optional subtree `Diff` to recurse
/// into. Callers must have already established that the entries differ (equal
/// children produce no diff).
///
/// Generic over the tree-id payload `T` so both the unordered (`T = TreeId`) and
/// the ordered (`T = (Weight, TreeId)`) paths share the exact leaf/tree
/// classification; the ordered path strips the weight afterwards. A file<->dir
/// transition yields both a leaf output and a subtree recursion.
pub(crate) fn classify_child<T, Leaf>(
    path: MPath,
    old: Option<Entry<T, Leaf>>,
    new: Option<Entry<T, Leaf>>,
) -> (Option<Diff<Entry<T, Leaf>>>, Option<Diff<T>>) {
    match (old, new) {
        (Some(Entry::Leaf(old)), Some(Entry::Leaf(new))) => (
            Some(Diff::Changed(path, Entry::Leaf(old), Entry::Leaf(new))),
            None,
        ),
        (Some(Entry::Tree(old)), Some(new @ Entry::Leaf(_))) => (
            Some(Diff::Added(path.clone(), new)),
            Some(Diff::Removed(path, old)),
        ),
        (Some(old @ Entry::Leaf(_)), Some(Entry::Tree(new))) => (
            Some(Diff::Removed(path.clone(), old)),
            Some(Diff::Added(path, new)),
        ),
        (Some(Entry::Tree(old)), Some(Entry::Tree(new))) => {
            (None, Some(Diff::Changed(path, old, new)))
        }
        (Some(old @ Entry::Leaf(_)), None) => (Some(Diff::Removed(path, old)), None),
        (Some(Entry::Tree(old)), None) => (None, Some(Diff::Removed(path, old))),
        (None, Some(new @ Entry::Leaf(_))) => (Some(Diff::Added(path, new)), None),
        (None, Some(Entry::Tree(new))) => (None, Some(Diff::Added(path, new))),
        (None, None) => (None, None),
    }
}

/// Compare the children of two directory tries, returning only the children that
/// differ as `(element, old_entry, new_entry)` tuples (`None` on a side means the
/// child is absent there). Identical sub-shards are pruned by their
/// content-addressed id -- never loaded -- via [`compare_tries`], and byte-prefix
/// spans of new/removed children are expanded to their individual elements.
///
/// This is the shared node-level core behind both the unordered
/// [`crate::ManifestOps::filtered_diff`] and the ordered
/// [`diff_weighted_children`], which drive it with the unweighted and weighted
/// views of the same directory. [`crate::bonsai_diff_sharded`] runs it once per
/// parent. `old_trie` and `new_trie` may come from different
/// blobstores, and either may be absent (an added or removed subtree).
/// `replacements` are applied to the old side as the comparison descends.
///
/// The returned children are in unspecified order; callers that need them sorted
/// (e.g. the ordered path) must sort by element.
pub(crate) async fn diff_trie_children<T, V, Store>(
    ctx: &CoreContext,
    new_store: &Store,
    new_trie: Option<T>,
    old_store: &Store,
    old_trie: Option<T>,
    replacements: PrefixTree<PathTree<Option<V>>>,
) -> Result<Vec<(MPathElement, Option<V>, Option<V>)>>
where
    T: TrieMapOps<Store, V> + Eq + Send + Sync + 'static,
    V: Eq + Send + Sync + 'static,
    Store: Send + Sync + 'static,
{
    let mut result = Vec::new();
    let mut pending = Vec::new();
    let mut cmps = compare_tries(
        ctx,
        new_store,
        old_store,
        new_trie,
        vec![old_trie],
        replacements,
    )?;
    while let Some(cmp) = cmps.try_next().await? {
        tokio::task::consume_budget().await;
        match cmp {
            ManifestComparison::Same(..) => {}
            ManifestComparison::New(Span::Element(name, entry)) => {
                result.push((name, None, Some(entry)));
            }
            ManifestComparison::New(Span::Prefix(prefix, trie_map)) => {
                pending.push((prefix, trie_map, true));
            }
            ManifestComparison::Removed(Span::Element(name, base_entries)) => {
                result.push((name, base_entries.into_iter().flatten().next(), None));
            }
            ManifestComparison::Removed(Span::Prefix(prefix, base_trie_maps)) => {
                if let Some(trie_map) = base_trie_maps.into_iter().flatten().next() {
                    pending.push((prefix, trie_map, false));
                }
            }
            ManifestComparison::Changed(name, new_entry, base_entries) => {
                result.push((
                    name,
                    base_entries.into_iter().flatten().next(),
                    Some(new_entry),
                ));
            }
        }
    }

    stream::iter(pending)
        .map(async |(prefix, trie_map, is_new)| {
            let store = if is_new { new_store } else { old_store };
            let mut entries = trie_map.into_stream(ctx, store).await?;
            let mut out = Vec::new();
            while let Some((suffix, entry)) = entries.try_next().await? {
                tokio::task::consume_budget().await;
                let name = prefix.clone().join_into_element(suffix)?;
                out.push(if is_new {
                    (name, None, Some(entry))
                } else {
                    (name, Some(entry), None)
                });
            }
            anyhow::Ok(out)
        })
        .buffered(10)
        .try_for_each(|out| {
            result.extend(out);
            future::ready(Ok(()))
        })
        .await?;

    Ok(result)
}

/// Diff a single node by comparing both sides' tries, applying `replacements` to
/// the old side as the comparison descends, returning the child `Diff`s plus the
/// subtree work to recurse into.
///
/// Identical sub-shards are pruned by ID without being loaded. A replacement
/// only disables that pruning along the byte path leading to it.
pub(crate) async fn diff_manifest_node<TreeId, Leaf, Store, Pruner>(
    ctx: &CoreContext,
    old_store: &Store,
    new_store: &Store,
    work: Diff<TreeId>,
    replacements: PrefixTree<PathTree<Option<Entry<TreeId, Leaf>>>>,
    recurse_pruner: Pruner,
) -> Result<(
    Vec<Diff<Entry<TreeId, Leaf>>>,
    Vec<(
        Diff<TreeId>,
        PrefixTree<PathTree<Option<Entry<TreeId, Leaf>>>>,
    )>,
)>
where
    Store: Clone + Send + Sync + 'static,
    TreeId: StoreLoadable<Store> + Clone + Send + Sync + Eq + Unpin + 'static,
    <TreeId as StoreLoadable<Store>>::Value:
        Manifest<Store, TreeId = TreeId, Leaf = Leaf> + Send + Sync,
    Leaf: Clone + Send + Sync + Eq + Unpin + 'static,
    <<TreeId as StoreLoadable<Store>>::Value as Manifest<Store>>::TrieMapType:
        TrieMapOps<Store, Entry<TreeId, Leaf>> + Eq,
    Pruner: Fn(&Diff<TreeId>) -> bool + Send,
{
    let (path, old_id, new_id) = match &work {
        Diff::Changed(path, old_id, new_id) => (path, Some(old_id), Some(new_id)),
        Diff::Added(path, new_id) => (path, None, Some(new_id)),
        Diff::Removed(path, old_id) => (path, Some(old_id), None),
    };
    let load_trie = async |id: Option<&TreeId>, store: &Store| match id {
        Some(id) => Ok(Some(
            id.load(ctx, store)
                .watched()
                .await?
                .into_trie_map(ctx, store)
                .await?,
        )),
        None => anyhow::Ok(None),
    };
    let (old_trie, new_trie) =
        future::try_join(load_trie(old_id, old_store), load_trie(new_id, new_store)).await?;
    let mut outs = Vec::new();
    let mut recurse = Vec::new();

    for (name, old, new) in diff_trie_children(
        ctx,
        new_store,
        new_trie,
        old_store,
        old_trie,
        replacements.clone(),
    )
    .await?
    {
        let child_path = path.join(&name);
        let child_replacements = replacements
            .get(name.as_ref())
            .cloned()
            .unwrap_or_default()
            .subentries;
        let (output, work) = classify_child(child_path, old, new);
        outs.extend(output);
        if let Some(work) = work
            && recurse_pruner(&work)
        {
            recurse.push((work, child_replacements));
        }
    }
    outs.push(match work {
        Diff::Changed(path, old_id, new_id) => {
            Diff::Changed(path, Entry::Tree(old_id), Entry::Tree(new_id))
        }
        Diff::Added(path, new_id) => Diff::Added(path, Entry::Tree(new_id)),
        Diff::Removed(path, old_id) => Diff::Removed(path, Entry::Tree(old_id)),
    });
    Ok((outs, recurse))
}

/// Sharding-aware ordered child diff for a single directory level: the children
/// that differ between `old_id` and `new_id`, sorted by element name, as weighted
/// entries (matching `OrderedManifest::list_weighted`'s shape). Either directory
/// may be absent when its subtree is added or removed.
///
/// Drives [`diff_trie_children`] with the weighted view of each directory, so the
/// rollup weights come out of the comparison itself. Identical sub-shards are
/// still pruned by id without being loaded.
pub(crate) async fn diff_weighted_children<TreeId, Leaf, Store>(
    ctx: &CoreContext,
    old_store: &Store,
    old_id: Option<&TreeId>,
    new_store: &Store,
    new_id: Option<&TreeId>,
    replacements: PrefixTree<PathTree<Option<Entry<(Weight, TreeId), Leaf>>>>,
) -> Result<
    Vec<(
        MPathElement,
        Option<Entry<(Weight, TreeId), Leaf>>,
        Option<Entry<(Weight, TreeId), Leaf>>,
    )>,
>
where
    Store: Send + Sync + 'static,
    TreeId: StoreLoadable<Store> + Clone + Send + Sync + Eq + Unpin + 'static,
    <TreeId as StoreLoadable<Store>>::Value:
        OrderedManifest<Store> + Manifest<Store, TreeId = TreeId, Leaf = Leaf> + Send + Sync,
    Leaf: Clone + Send + Sync + Eq + Unpin + 'static,
    <<TreeId as StoreLoadable<Store>>::Value as OrderedManifest<Store>>::WeightedTrieMapType:
        TrieMapOps<Store, Entry<(Weight, TreeId), Leaf>> + Eq + 'static,
{
    let load_trie = async |id: Option<&TreeId>, store: &Store| match id {
        Some(id) => Ok(Some(
            id.load(ctx, store)
                .await?
                .into_weighted_trie_map(ctx, store)
                .await?,
        )),
        None => anyhow::Ok(None),
    };
    let (old_trie, new_trie) =
        future::try_join(load_trie(old_id, old_store), load_trie(new_id, new_store)).await?;
    let mut differing =
        diff_trie_children(ctx, new_store, new_trie, old_store, old_trie, replacements).await?;
    // The ordered scheduler consumes children in element order.
    differing.sort_by(|(a, ..), (b, ..)| a.cmp(b));
    Ok(differing)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use anyhow::anyhow;
    use blobstore::KeyedBlobstore;
    use blobstore::PutBehaviour;
    use borrowed::borrowed;
    use fbinit::FacebookInit;
    use futures::stream::TryStreamExt;
    use maplit::btreemap;
    use memblob::KeyedMemblob;
    use memblob::Memblob;
    use mononoke_macros::mononoke;
    use mononoke_types::FileType;
    use mononoke_types::SortedVectorTrieMap;
    use mononoke_types::path::MPath;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::ops::ManifestOps;
    // use crate::tests::test_manifest::TestLeaf;
    use crate::tests::test_manifest::TestLeafId;
    use crate::tests::test_manifest::TestManifestId;
    use crate::tests::test_manifest::derive_test_manifest;

    async fn get_trie_map(
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        mf: TestManifestId,
        path: &str,
        prefix: &str,
    ) -> Result<SortedVectorTrieMap<Entry<TestManifestId, (FileType, TestLeafId)>>> {
        let mf = mf
            .find_entry(ctx.clone(), blobstore.clone(), MPath::new(path)?)
            .await?
            .ok_or_else(|| anyhow!("path {path} not found"))?
            .into_tree()
            .ok_or_else(|| anyhow!("path {path} is not a tree"))?;
        let mut trie_map = mf
            .load(ctx, blobstore)
            .await?
            .into_trie_map(ctx, blobstore)
            .await?;
        for byte in prefix.as_bytes() {
            let (_, subentries) = trie_map.expand()?;
            let mut subentries = subentries.into_iter().collect::<BTreeMap<_, _>>();
            trie_map = subentries
                .remove(byte)
                .ok_or_else(|| anyhow!("prefix {prefix} not found at {path}"))?;
        }
        Ok(trie_map)
    }

    async fn get_entry(
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        mf: TestManifestId,
        path: &str,
    ) -> Result<Entry<TestManifestId, (FileType, TestLeafId)>> {
        mf.find_entry(ctx.clone(), blobstore.clone(), MPath::new(path)?)
            .await?
            .ok_or_else(|| anyhow!("path {path} not found"))
    }

    #[mononoke::fbinit_test]
    async fn test_compare_manifest_single_parent(fb: FacebookInit) -> Result<()> {
        let blobstore: Arc<dyn KeyedBlobstore> =
            Arc::new(KeyedMemblob::new(Memblob::new(PutBehaviour::Overwrite)));
        let ctx = CoreContext::test_mock(fb);
        borrowed!(ctx, blobstore);

        let mf0 = derive_test_manifest(
            ctx,
            blobstore,
            vec![],
            btreemap! {
                "/dir1/file1" => Some("file1"),
                "/dir1/file2" =>  Some("file2"),
                "/dir2/file3" =>  Some("file3"),
                "/dir2/file4" =>  Some("file4"),
                "/dir2/dir3/file5" => Some("file5"),
                "/dir2/dir3/file6" =>  Some("file6"),
                "/dir4a/file7a" => Some("file7a"),
                "/dir4b/file7b" => Some("file7b"),
                "/file7" => Some("file7"),
                "/file8" => Some("file8"),
            },
        )
        .await?
        .unwrap();

        let mf1 = derive_test_manifest(
            ctx,
            blobstore,
            vec![mf0],
            btreemap! {
                "/dir1/file1" => Some("file1a"),
                "/dir2/file3" => None,
                "/dir2/file9" => Some("file9"),
                "/dir2/dir3/file5" => None,
                "/dir2/dir3/file6" => None,
                "/file7" => None,
                "/file7/file7" => Some("file7"),
            },
        )
        .await?
        .unwrap();

        let diff = compare_manifest(
            ctx,
            blobstore,
            mf1.load(ctx, blobstore).await?,
            vec![Some(mf0.load(ctx, blobstore).await?)],
        )
        .await?
        .try_collect::<Vec<_>>()
        .await?;

        assert_eq!(
            diff,
            vec![
                ManifestComparison::Same(
                    Span::Prefix(
                        MPathElementPrefix::from_slice(b"dir4")?,
                        get_trie_map(ctx, blobstore, mf1, "", "dir4").await?,
                    ),
                    0
                ),
                ManifestComparison::Changed(
                    MPathElement::new_from_slice(b"dir2")?,
                    get_entry(ctx, blobstore, mf1, "dir2").await?,
                    vec![Some(get_entry(ctx, blobstore, mf0, "dir2").await?)],
                ),
                ManifestComparison::Changed(
                    MPathElement::new_from_slice(b"dir1")?,
                    get_entry(ctx, blobstore, mf1, "dir1").await?,
                    vec![Some(get_entry(ctx, blobstore, mf0, "dir1").await?)],
                ),
                ManifestComparison::Same(
                    Span::Prefix(
                        MPathElementPrefix::from_slice(b"file8")?,
                        get_trie_map(ctx, blobstore, mf1, "", "file8").await?,
                    ),
                    0
                ),
                ManifestComparison::Changed(
                    MPathElement::new_from_slice(b"file7")?,
                    get_entry(ctx, blobstore, mf1, "file7").await?,
                    vec![Some(get_entry(ctx, blobstore, mf0, "file7").await?)],
                ),
            ]
        );

        Ok(())
    }

    /// Compare directory `path` of `mf` against directory `path` of each base.
    /// A base with no tree there contributes no base manifest.
    async fn compare_dir(
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        mf: TestManifestId,
        bases: Vec<TestManifestId>,
        path: &str,
    ) -> Result<
        Vec<
            ManifestComparison<
                SortedVectorTrieMap<Entry<TestManifestId, (FileType, TestLeafId)>>,
                Entry<TestManifestId, (FileType, TestLeafId)>,
            >,
        >,
    > {
        let mf = get_entry(ctx, blobstore, mf, path)
            .await?
            .into_tree()
            .ok_or_else(|| anyhow!("path {path} is not a tree"))?
            .load(ctx, blobstore)
            .await?;
        let mut base_mfs = Vec::new();
        for base in bases {
            base_mfs.push(
                match base
                    .find_entry(ctx.clone(), blobstore.clone(), MPath::new(path)?)
                    .await?
                {
                    Some(Entry::Tree(tree_id)) => Some(tree_id.load(ctx, blobstore).await?),
                    Some(Entry::Leaf(_)) | None => None,
                },
            );
        }
        compare_manifest(ctx, blobstore, mf, base_mfs)
            .await?
            .try_collect()
            .await
    }

    #[mononoke::fbinit_test]
    async fn test_compare_manifest_nested(fb: FacebookInit) -> Result<()> {
        let blobstore: Arc<dyn KeyedBlobstore> =
            Arc::new(KeyedMemblob::new(Memblob::new(PutBehaviour::Overwrite)));
        let ctx = CoreContext::test_mock(fb);
        let (ctx, blobstore) = (&ctx, &blobstore);

        let mf0 = derive_test_manifest(
            ctx,
            blobstore,
            vec![],
            btreemap! {
                "/dir1/file1" => Some("file1"),
                "/dir1/file2" =>  Some("file2"),
                "/dir2/file3" =>  Some("file3"),
                "/dir2/file4" =>  Some("file4"),
                "/dir2/dir3/file5" => Some("file5"),
                "/dir2/dir3/file6" =>  Some("file6"),
                "/dir4a/file7a" => Some("file7a"),
                "/dir5/file8" => Some("file8"),
                "/file7" => Some("file7"),
            },
        )
        .await?
        .unwrap();

        let mf1 = derive_test_manifest(
            ctx,
            blobstore,
            vec![mf0],
            btreemap! {
                "/dir1/file1" => Some("file1a"),
                "/dir2/file3" => None,
                "/dir2/file9" => Some("file9"),
                "/dir2/dir3/file5" => None,
                "/dir2/dir3/file6" => None,
                "/file7" => None,
                "/file7/file7" => Some("file7"),
            },
        )
        .await?
        .unwrap();

        let mf2 = derive_test_manifest(
            ctx,
            blobstore,
            vec![mf0],
            btreemap! {
                "/dir1/file1" => Some("file1b"),
                "/dir1/file1c" => Some("file1c"),
            },
        )
        .await?
        .unwrap();

        let mf3 = derive_test_manifest(
            ctx,
            blobstore,
            vec![mf1, mf2],
            btreemap! {
                "/dir1/file1" => Some("file1b"),
                "/dir1/file1c" => Some("file1c"),
                "/dir5/file8" => None,
                "/file7" => Some("file7"),
            },
        )
        .await?
        .unwrap();

        let root = compare_dir(ctx, blobstore, mf1, vec![mf0], "").await?;
        assert_eq!(
            root,
            vec![
                ManifestComparison::Same(
                    Span::Prefix(
                        MPathElementPrefix::from_slice(b"dir5")?,
                        get_trie_map(ctx, blobstore, mf1, "", "dir5").await?,
                    ),
                    0,
                ),
                ManifestComparison::Same(
                    Span::Prefix(
                        MPathElementPrefix::from_slice(b"dir4")?,
                        get_trie_map(ctx, blobstore, mf1, "", "dir4").await?,
                    ),
                    0,
                ),
                ManifestComparison::Changed(
                    MPathElement::new_from_slice(b"dir2")?,
                    get_entry(ctx, blobstore, mf1, "dir2").await?,
                    vec![Some(get_entry(ctx, blobstore, mf0, "dir2").await?),],
                ),
                ManifestComparison::Changed(
                    MPathElement::new_from_slice(b"dir1")?,
                    get_entry(ctx, blobstore, mf1, "dir1").await?,
                    vec![Some(get_entry(ctx, blobstore, mf0, "dir1").await?),],
                ),
                ManifestComparison::Changed(
                    MPathElement::new_from_slice(b"file7")?,
                    get_entry(ctx, blobstore, mf1, "file7").await?,
                    vec![Some(get_entry(ctx, blobstore, mf0, "file7").await?),],
                ),
            ]
        );

        let file7_dir = compare_dir(ctx, blobstore, mf1, vec![mf0], "file7").await?;
        assert_eq!(
            file7_dir,
            vec![ManifestComparison::New(Span::Prefix(
                MPathElementPrefix::from_slice(b"")?,
                get_trie_map(ctx, blobstore, mf1, "file7", "").await?,
            )),]
        );

        let dir1 = compare_dir(ctx, blobstore, mf1, vec![mf0], "dir1").await?;
        assert_eq!(
            dir1,
            vec![
                ManifestComparison::Same(
                    Span::Prefix(
                        MPathElementPrefix::from_slice(b"file2")?,
                        get_trie_map(ctx, blobstore, mf1, "dir1", "file2").await?,
                    ),
                    0,
                ),
                ManifestComparison::Changed(
                    MPathElement::new_from_slice(b"file1")?,
                    get_entry(ctx, blobstore, mf1, "dir1/file1").await?,
                    vec![Some(get_entry(ctx, blobstore, mf0, "dir1/file1").await?),],
                ),
            ]
        );

        let dir2 = compare_dir(ctx, blobstore, mf1, vec![mf0], "dir2").await?;
        assert_eq!(
            dir2,
            vec![
                ManifestComparison::Removed(Span::Prefix(
                    MPathElementPrefix::from_slice(b"d")?,
                    vec![Some(get_trie_map(ctx, blobstore, mf0, "dir2", "d").await?),],
                )),
                ManifestComparison::New(Span::Prefix(
                    MPathElementPrefix::from_slice(b"file9")?,
                    get_trie_map(ctx, blobstore, mf1, "dir2", "file9").await?,
                )),
                ManifestComparison::Same(
                    Span::Prefix(
                        MPathElementPrefix::from_slice(b"file4")?,
                        get_trie_map(ctx, blobstore, mf1, "dir2", "file4").await?,
                    ),
                    0,
                ),
                ManifestComparison::Removed(Span::Prefix(
                    MPathElementPrefix::from_slice(b"file3")?,
                    vec![Some(
                        get_trie_map(ctx, blobstore, mf0, "dir2", "file3").await?
                    ),],
                )),
            ]
        );

        let mf2_root = compare_dir(ctx, blobstore, mf2, vec![mf0], "").await?;
        assert_eq!(
            mf2_root,
            vec![
                ManifestComparison::Same(
                    Span::Prefix(
                        MPathElementPrefix::from_slice(b"f")?,
                        get_trie_map(ctx, blobstore, mf2, "", "f").await?,
                    ),
                    0,
                ),
                ManifestComparison::Same(
                    Span::Prefix(
                        MPathElementPrefix::from_slice(b"dir5")?,
                        get_trie_map(ctx, blobstore, mf2, "", "dir5").await?,
                    ),
                    0,
                ),
                ManifestComparison::Same(
                    Span::Prefix(
                        MPathElementPrefix::from_slice(b"dir4")?,
                        get_trie_map(ctx, blobstore, mf2, "", "dir4").await?,
                    ),
                    0,
                ),
                ManifestComparison::Same(
                    Span::Prefix(
                        MPathElementPrefix::from_slice(b"dir2")?,
                        get_trie_map(ctx, blobstore, mf2, "", "dir2").await?,
                    ),
                    0,
                ),
                ManifestComparison::Changed(
                    MPathElement::new_from_slice(b"dir1")?,
                    get_entry(ctx, blobstore, mf2, "dir1").await?,
                    vec![Some(get_entry(ctx, blobstore, mf0, "dir1").await?),],
                ),
            ]
        );

        let mf2_dir1 = compare_dir(ctx, blobstore, mf2, vec![mf0], "dir1").await?;
        assert_eq!(
            mf2_dir1,
            vec![
                ManifestComparison::Same(
                    Span::Prefix(
                        MPathElementPrefix::from_slice(b"file2")?,
                        get_trie_map(ctx, blobstore, mf2, "dir1", "file2").await?,
                    ),
                    0,
                ),
                ManifestComparison::Changed(
                    MPathElement::new_from_slice(b"file1")?,
                    get_entry(ctx, blobstore, mf2, "dir1/file1").await?,
                    vec![Some(get_entry(ctx, blobstore, mf0, "dir1/file1").await?),],
                ),
                ManifestComparison::New(Span::Prefix(
                    MPathElementPrefix::from_slice(b"file1c")?,
                    get_trie_map(ctx, blobstore, mf2, "dir1", "file1c").await?,
                )),
            ]
        );

        let merge_root = compare_dir(ctx, blobstore, mf3, vec![mf1, mf2], "").await?;
        assert_eq!(
            merge_root,
            vec![
                ManifestComparison::Same(
                    Span::Prefix(
                        MPathElementPrefix::from_slice(b"f")?,
                        get_trie_map(ctx, blobstore, mf3, "", "f").await?,
                    ),
                    1,
                ),
                ManifestComparison::Removed(Span::Prefix(
                    MPathElementPrefix::from_slice(b"dir5")?,
                    vec![
                        Some(get_trie_map(ctx, blobstore, mf1, "", "dir5").await?),
                        Some(get_trie_map(ctx, blobstore, mf2, "", "dir5").await?),
                    ],
                )),
                ManifestComparison::Same(
                    Span::Prefix(
                        MPathElementPrefix::from_slice(b"dir4")?,
                        get_trie_map(ctx, blobstore, mf3, "", "dir4").await?,
                    ),
                    0,
                ),
                ManifestComparison::Changed(
                    MPathElement::new_from_slice(b"dir2")?,
                    get_entry(ctx, blobstore, mf3, "dir2").await?,
                    vec![
                        Some(get_entry(ctx, blobstore, mf1, "dir2").await?),
                        Some(get_entry(ctx, blobstore, mf2, "dir2").await?),
                    ],
                ),
                ManifestComparison::Same(
                    Span::Prefix(
                        MPathElementPrefix::from_slice(b"dir1")?,
                        get_trie_map(ctx, blobstore, mf3, "", "dir1").await?,
                    ),
                    1,
                ),
            ]
        );

        let merge_dir2 = compare_dir(ctx, blobstore, mf3, vec![mf1, mf2], "dir2").await?;
        assert_eq!(
            merge_dir2,
            vec![
                ManifestComparison::Same(
                    Span::Prefix(
                        MPathElementPrefix::from_slice(b"d")?,
                        get_trie_map(ctx, blobstore, mf3, "dir2", "d").await?,
                    ),
                    1,
                ),
                ManifestComparison::Same(
                    Span::Prefix(
                        MPathElementPrefix::from_slice(b"file9")?,
                        get_trie_map(ctx, blobstore, mf3, "dir2", "file9").await?,
                    ),
                    0,
                ),
                ManifestComparison::Same(
                    Span::Prefix(
                        MPathElementPrefix::from_slice(b"file4")?,
                        get_trie_map(ctx, blobstore, mf3, "dir2", "file4").await?,
                    ),
                    0,
                ),
                ManifestComparison::Same(
                    Span::Prefix(
                        MPathElementPrefix::from_slice(b"file3")?,
                        get_trie_map(ctx, blobstore, mf3, "dir2", "file3").await?,
                    ),
                    1,
                ),
            ]
        );

        Ok(())
    }
}
