/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![allow(dead_code)]
#![allow(clippy::mutable_key_type)] // false positive: Bytes is not inner mutable

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::OnceLock;

use anyhow::Context;
use anyhow::Ok;
use anyhow::Result;
use anyhow::bail;
use async_recursion::async_recursion;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use blobstore::Storable;
use bounded_traversal::OrderedTraversal;
use bounded_traversal::bounded_traversal_ordered_stream;
use bytes::Bytes;
use context::CoreContext;
use derivative::Derivative;
use futures::FutureExt;
use futures::Stream;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use futures::stream::BoxStream;
use itertools::Either;
use itertools::Itertools;
use nonzero_ext::nonzero;
use smallvec::SmallVec;
use sorted_vector_map::SortedVectorMap;
use sorted_vector_map::sorted_vector_map;

use crate::ThriftConvert;
use crate::TrieMap;
use crate::blob::Blob;
use crate::blob::BlobstoreValue;
use crate::thrift;
use crate::typed_hash::IdContext;
use crate::typed_hash::MononokeId;

pub trait MapValue: ThriftConvert + Debug + Clone + Send + Sync + 'static {
    type Id: MononokeId<Thrift = thrift::id::ShardedMapNodeId, Value = ShardedMapNode<Self>>;
    type Context: IdContext<Id = Self::Id>;
}

type SmallBinary = SmallVec<[u8; 24]>;

#[derive(Derivative)]
#[derivative(PartialEq, Debug)]
#[derive(Clone, Eq)]
pub enum ShardedMapNode<Value: MapValue> {
    Intermediate {
        prefix: SmallBinary,
        value: Option<Value>,
        edges: SortedVectorMap<u8, ShardedMapEdge<Value>>,
        #[derivative(PartialEq = "ignore", Debug = "ignore")]
        size: OnceLock<usize>,
    },
    Terminal {
        // The key is the original map key minus the prefixes and edges from all
        // intermediate nodes in the path to this node.
        values: SortedVectorMap<SmallBinary, Value>,
    },
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ShardedMapEdge<Value: MapValue> {
    size: usize,
    child: ShardedMapChild<Value>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ShardedMapChild<Value: MapValue> {
    Id(Value::Id),
    Inlined(ShardedMapNode<Value>),
}

impl<Value: MapValue> ShardedMapEdge<Value> {
    async fn load_child(
        self,
        ctx: &CoreContext,
        blobstore: &impl KeyedBlobstore,
    ) -> Result<ShardedMapNode<Value>> {
        self.child.load(ctx, blobstore).await
    }

    fn from_thrift(t: thrift::sharded_map::ShardedMapEdge) -> Result<Self> {
        Ok(Self {
            size: t.size.try_into().context("Failed to parse size to usize")?,
            child: ShardedMapChild::from_thrift(t.child)?,
        })
    }

    fn into_thrift(self) -> thrift::sharded_map::ShardedMapEdge {
        thrift::sharded_map::ShardedMapEdge {
            size: self.size as i64,
            child: self.child.into_thrift(),
        }
    }

    /// Return this MapNode's id if it's not inlined
    pub fn id(&self) -> Option<Value::Id> {
        self.child.id()
    }
}

impl<Value: MapValue> ShardedMapChild<Value> {
    pub async fn load(
        self,
        ctx: &CoreContext,
        blobstore: &impl KeyedBlobstore,
    ) -> Result<ShardedMapNode<Value>> {
        match self {
            Self::Inlined(inlined) => Ok(inlined),
            Self::Id(id) => id.load(ctx, blobstore).await.map_err(Into::into),
        }
    }

    fn from_thrift(t: thrift::sharded_map::ShardedMapChild) -> Result<Self> {
        Ok(match t {
            thrift::sharded_map::ShardedMapChild::inlined(inlined) => {
                Self::Inlined(ShardedMapNode::from_thrift(inlined)?)
            }
            thrift::sharded_map::ShardedMapChild::id(id) => Self::Id(Value::Id::from_thrift(id)?),
            thrift::sharded_map::ShardedMapChild::UnknownField(_) => bail!("Unknown variant"),
        })
    }

    fn into_thrift(self) -> thrift::sharded_map::ShardedMapChild {
        match self {
            Self::Inlined(inlined) => {
                thrift::sharded_map::ShardedMapChild::inlined(inlined.into_thrift())
            }
            Self::Id(id) => thrift::sharded_map::ShardedMapChild::id(id.into_thrift()),
        }
    }

    /// Return this MapNode's id if it's not inlined
    pub fn id(&self) -> Option<Value::Id> {
        match self {
            Self::Id(v) => Some(*v),
            Self::Inlined(_) => None,
        }
    }
}

impl<Value: MapValue> Default for ShardedMapChild<Value> {
    fn default() -> Self {
        Self::Inlined(Default::default())
    }
}

impl<Value: MapValue> Default for ShardedMapNode<Value> {
    fn default() -> Self {
        Self::Terminal {
            values: SortedVectorMap::new(),
        }
    }
}

impl<Value: MapValue> Default for ShardedMapEdge<Value> {
    fn default() -> Self {
        Self {
            size: 0,
            child: Default::default(),
        }
    }
}

/// Returns longest common prefix of a and b.
fn common_prefix<'a>(a: &'a [u8], b: &'a [u8]) -> &'a [u8] {
    let lcp = a.iter().zip(b.iter()).take_while(|(a, b)| a == b).count();
    // Panic safety: lcp is at most a.len()
    &a[..lcp]
}

#[derive(PartialEq, Clone)]
pub enum ShardedTrieMap<Value: MapValue> {
    Edge(ShardedMapEdge<Value>),
    Trie(TrieMap<Value>),
}

impl<Value: MapValue> ShardedTrieMap<Value> {
    pub fn new(node: ShardedMapNode<Value>) -> Self {
        ShardedTrieMap::Edge(ShardedMapEdge {
            size: node.size(),
            child: ShardedMapChild::Inlined(node),
        })
    }

    pub async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &impl KeyedBlobstore,
    ) -> Result<(Option<Value>, Vec<(u8, Self)>)> {
        match self {
            Self::Edge(edge) => match edge.load_child(ctx, blobstore).await? {
                ShardedMapNode::Intermediate {
                    prefix,
                    value,
                    edges,
                    ..
                } => match prefix.split_first() {
                    Some((first_byte, rest)) => {
                        let node = ShardedMapNode::from_entries(
                            ctx,
                            blobstore,
                            edges
                                .into_iter()
                                .map(|(next_byte, edge)| {
                                    (
                                        Bytes::copy_from_slice(&[rest, &[next_byte]].concat()),
                                        Either::Right(edge),
                                    )
                                })
                                .collect(),
                        )
                        .await?;
                        Ok((
                            None,
                            vec![(
                                *first_byte,
                                ShardedTrieMap::Edge(ShardedMapEdge {
                                    size: node.size(),
                                    child: ShardedMapChild::Inlined(node),
                                }),
                            )],
                        ))
                    }
                    None => Ok((
                        value,
                        edges
                            .into_iter()
                            .map(|(next_byte, edge)| (next_byte, ShardedTrieMap::Edge(edge)))
                            .collect(),
                    )),
                },
                ShardedMapNode::Terminal { values } => {
                    let trie = values.into_iter().collect::<TrieMap<_>>();
                    let (value, children) = trie.expand();
                    Ok((
                        value,
                        children
                            .into_iter()
                            .map(|(next_byte, child)| (next_byte, ShardedTrieMap::Trie(child)))
                            .collect(),
                    ))
                }
            },
            Self::Trie(trie) => {
                let (value, children) = trie.expand();
                Ok((
                    value,
                    children
                        .into_iter()
                        .map(|(next_byte, child)| (next_byte, ShardedTrieMap::Trie(child)))
                        .collect(),
                ))
            }
        }
    }

    pub async fn into_stream<'a>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a impl KeyedBlobstore,
    ) -> Result<BoxStream<'a, Result<(SmallVec<[u8; 24]>, Value)>>> {
        match self {
            Self::Edge(edge) => Ok(edge
                .load_child(ctx, blobstore)
                .await?
                .into_entries(ctx, blobstore)
                .boxed()),
            Self::Trie(trie) => Ok(stream::iter(trie).map(Ok).boxed()),
        }
    }
}

impl<Value: MapValue> ShardedMapNode<Value> {
    fn intermediate(
        prefix: SmallBinary,
        value: Option<Value>,
        edges: SortedVectorMap<u8, ShardedMapEdge<Value>>,
    ) -> Self {
        Self::Intermediate {
            prefix,
            value,
            edges,
            size: Default::default(),
        }
    }

    /// Given a key, what's the value for that key, if any?
    // See the detailed description of the logic in docs/sharded_map.md
    #[async_recursion]
    pub async fn lookup(
        &self,
        ctx: &CoreContext,
        blobstore: &impl KeyedBlobstore,
        key: &[u8],
    ) -> Result<Option<Value>> {
        Ok(match self {
            // Case 1: Do lookup directly on the inlined map
            Self::Terminal { values } => values.get(key).cloned(),
            Self::Intermediate {
                prefix,
                value,
                edges,
                ..
            } => {
                if let Some(key) = key.strip_prefix(prefix.as_slice()) {
                    if let Some((first, rest)) = key.split_first() {
                        if let Some(edge) = edges.get(first) {
                            // Case 2: Recurse, either inlined or first fetching from the blobstore
                            match &edge.child {
                                ShardedMapChild::Inlined(node) => {
                                    node.lookup(ctx, blobstore, rest).await?
                                }
                                ShardedMapChild::Id(id) => {
                                    id.load(ctx, blobstore)
                                        .await?
                                        .lookup(ctx, blobstore, rest)
                                        .await?
                                }
                            }
                        } else {
                            // Case 3: No edge from this node to the next byte of the key
                            None
                        }
                    } else {
                        // Case 4: The node for this key is this intermediate node, not a terminal node
                        value.clone()
                    }
                } else {
                    // Case 5: Key doesn't match prefix
                    None
                }
            }
        })
    }

    /// The value of each of `keys`, as [`Self::lookup`] returns it. Adjacent
    /// keys that take the same edge share one walk down it, so when `keys` are
    /// sorted each stored node is loaded once, by `load`. Stored nodes are
    /// loaded one at a time.
    #[async_recursion]
    pub async fn lookup_sorted<Load, Loading>(
        &self,
        keys: &[&[u8]],
        load: &Load,
    ) -> Result<Vec<Option<Value>>>
    where
        Load: Fn(Value::Id) -> Loading + Sync,
        Loading: Future<Output = Result<Self>> + Send,
    {
        let (prefix, value, edges) = match self {
            Self::Terminal { values } => {
                return Ok(keys.iter().map(|key| values.get(*key).cloned()).collect());
            }
            Self::Intermediate {
                prefix,
                value,
                edges,
                ..
            } => (prefix, value, edges),
        };
        // `None` for a key outside `prefix`, `Some(None)` for `prefix` itself,
        // else the byte of the edge the key takes.
        let next = |key: &[u8]| {
            key.strip_prefix(prefix.as_slice())
                .map(|rest| rest.first().copied())
        };
        let mut found = Vec::with_capacity(keys.len());
        for run in keys.chunk_by(|a, b| next(a) == next(b)) {
            let edge = match next(run[0]) {
                None => None,
                Some(None) => {
                    found.extend(run.iter().map(|_| value.clone()));
                    continue;
                }
                Some(Some(byte)) => edges.get(&byte),
            };
            let Some(edge) = edge else {
                found.extend(run.iter().map(|_| None));
                continue;
            };
            let rests = run
                .iter()
                .map(|key| &key[prefix.len() + 1..])
                .collect::<Vec<_>>();
            found.extend(match &edge.child {
                ShardedMapChild::Inlined(node) => node.lookup_sorted(&rests, load).await?,
                ShardedMapChild::Id(id) => load(*id).await?.lookup_sorted(&rests, load).await?,
            });
        }
        Ok(found)
    }

    /// Given a map and replacements, return the map with the replacements.
    fn update_map(
        map: SortedVectorMap<SmallBinary, Value>,
        replacements: BTreeMap<Bytes, Option<Value>>,
        deleter: impl Fn(Value),
    ) -> SortedVectorMap<SmallBinary, Value> {
        // Worst case scenario, every update deletes from the map.
        let mut result =
            SortedVectorMap::with_capacity(map.len().saturating_sub(replacements.len()));
        let mut map_iter = map.into_iter().peekable();
        let mut replacements_iter = replacements.into_iter().peekable();
        // Iterate the two maps in lockstep
        while let (Some((map_key, _)), Some((replacements_key, _))) =
            (map_iter.peek(), replacements_iter.peek())
        {
            match map_key.as_slice().cmp(replacements_key.as_ref()) {
                Ordering::Less => {
                    // Next existing map entry is first, copy it.
                    if let Some((key, value)) = map_iter.next() {
                        result.insert(key, value);
                    }
                }
                Ordering::Greater => {
                    // New replacement is first, insert it.
                    if let Some((key, Some(new_value))) = replacements_iter.next() {
                        result.insert(SmallBinary::from_slice(key.as_ref()), new_value);
                    }
                }
                Ordering::Equal => {
                    // Replacement value matches map value.  Replace or
                    // delete.
                    if let Some((key, old_value)) = map_iter.next() {
                        if let Some((_, Some(new_value))) = replacements_iter.next() {
                            result.insert(key, new_value);
                        }
                        deleter(old_value);
                    }
                }
            }
        }
        // If the map iterator is not exhausted, copy the remainder.
        result.extend(map_iter);
        // If the replacements iterator is not exhausted, copy all
        // the insertions.
        result.extend(
            replacements_iter
                .filter_map(|(k, v)| v.map(|v| (SmallBinary::from_slice(k.as_ref()), v))),
        );
        result
    }

    /// Prepend all keys in this node with the given prefix.
    fn prepend(&mut self, prefix: SmallBinary) {
        match self {
            Self::Terminal { values } => {
                *values = std::mem::take(values)
                    .into_iter()
                    .update(|(k, _)| {
                        k.insert_from_slice(0, &prefix);
                    })
                    .collect()
            }
            Self::Intermediate {
                prefix: cur_prefix, ..
            } => {
                cur_prefix.insert_from_slice(0, &prefix);
            }
        }
    }

    fn shard_size() -> Result<usize> {
        if cfg!(test) {
            Ok(5)
        } else {
            thrift::sharded_map::MAP_SHARD_SIZE
                .try_into()
                .context("Failed to parse shard size")
        }
    }

    /// Create a new sharded map node from a map of values and other sharded map nodes. The key for every input sharded map node is
    /// a prefix that's prepended to it, which represents that keys that have this prefix are all contained in this sharded map node.
    /// Returns an error if the key for a sharded map node is a prefix of any other key in the input map.
    #[async_recursion]
    pub async fn from_entries(
        ctx: &CoreContext,
        blobstore: &impl KeyedBlobstore,
        entries: BTreeMap<Bytes, Either<Value, ShardedMapEdge<Value>>>,
    ) -> Result<Self> {
        let shard_size = Self::shard_size()?;

        let node_size: usize = entries
            .values()
            .map(|entry| match entry {
                Either::Left(_value) => 1,
                Either::Right(map) => map.size,
            })
            .sum();

        if node_size <= shard_size {
            if ShardedMapNode::has_conflicts(&entries) {
                bail!("Cannot create sharded map node with conflicting entries");
            }

            let entries_futures = entries
                .into_iter()
                .map(|(key, entry)| async move {
                    match entry {
                        Either::Left(value) => {
                            Ok(vec![(SmallVec::from_slice(key.as_ref()), value)])
                        }
                        Either::Right(map) => {
                            map.load_child(ctx, blobstore)
                                .await?
                                .into_entries(ctx, blobstore)
                                .map_ok(|(mut map_key, map_value)| {
                                    map_key.insert_from_slice(0, key.as_ref());
                                    (map_key, map_value)
                                })
                                .try_collect::<Vec<_>>()
                                .await
                        }
                    }
                })
                .collect::<Vec<_>>();

            Ok(Self::Terminal {
                values: stream::iter(entries_futures)
                    .buffer_unordered(100)
                    .map_ok(|v| stream::iter(v).map(Ok))
                    .try_flatten()
                    .try_collect()
                    .await?,
            })
        } else {
            let mut keys_iter = entries.keys();
            let longest_common_prefix = match keys_iter.next() {
                None => return Ok(Default::default()),
                Some(first_key) => SmallBinary::from_slice(
                    keys_iter.fold(first_key.as_ref(), |longest_common_prefix, key| {
                        common_prefix(longest_common_prefix, key)
                    }),
                ),
            };

            let mut partitioned: BTreeMap<
                u8,
                BTreeMap<Bytes, Either<Value, ShardedMapEdge<Value>>>,
            > = Default::default();
            let mut current_value = None;
            let is_single_entry = entries.len() == 1;

            for (key, entry) in entries {
                let stripped_key = key.strip_prefix(longest_common_prefix.as_ref()).unwrap();
                match stripped_key.split_first() {
                    None => match entry {
                        Either::Left(value) => {
                            current_value = Some(value);
                        }
                        Either::Right(edge) => {
                            if is_single_entry {
                                let mut map = edge.load_child(ctx, blobstore).await?;
                                map.prepend(SmallVec::from_slice(key.as_ref()));
                                return Ok(map);
                            } else {
                                bail!("Cannot create sharded map node with conflicting entries");
                            }
                        }
                    },
                    Some((first, rest)) => {
                        partitioned
                            .entry(*first)
                            .or_default()
                            .insert(key.slice_ref(rest), entry);
                    }
                }
            }

            let edges_futures = partitioned
                .into_iter()
                .map(|(next_byte, entries)| async move {
                    let node = Self::from_entries(ctx, blobstore, entries).await?;
                    Ok((
                        next_byte,
                        ShardedMapEdge {
                            size: node.size(),
                            child: match node {
                                Self::Intermediate { .. } => ShardedMapChild::Inlined(node),
                                Self::Terminal { .. } => ShardedMapChild::Id(
                                    node.into_blob().store(ctx, blobstore).await?,
                                ),
                            },
                        },
                    ))
                })
                .collect::<Vec<_>>();
            let edges = stream::iter(edges_futures)
                .buffer_unordered(100)
                .try_collect::<SortedVectorMap<_, _>>()
                .await?;

            Ok(Self::intermediate(
                longest_common_prefix,
                current_value,
                edges,
            ))
        }
    }

    /// Check if there exists a key corresponding to a map that is also a prefix of any other key in the input map.
    fn has_conflicts(entries: &BTreeMap<Bytes, Either<Value, ShardedMapEdge<Value>>>) -> bool {
        // It's sufficient to only check adjacent entries, because if some key is a prefix of another,
        // then it's also a prefix of all keys in between them in sorted order.
        entries
            .iter()
            .zip(entries.iter().skip(1))
            .any(
                |(first_entry, second_entry)| match (first_entry, second_entry) {
                    ((first_key, Either::Right(_first_map)), (second_key, _))
                        if second_key.starts_with(first_key) =>
                    {
                        true
                    }
                    ((first_key, _), (second_key, Either::Right(_second_map)))
                        if first_key.starts_with(second_key) =>
                    {
                        true
                    }
                    _ => false,
                },
            )
    }

    /// Create a new map from this map with given replacements. It is a generalization of
    /// adding and removing, and should be faster than doing all operations separately.
    /// It does not rely on the added keys not existing or the removed keys existing.
    // See the detailed description of the logic in docs/sharded_map.md
    #[async_recursion]
    pub async fn update(
        self,
        ctx: &CoreContext,
        blobstore: &impl KeyedBlobstore,
        replacements: BTreeMap<Bytes, Option<Value>>,
        // Called for all deletions
        deleter: impl Fn(Value) + Send + Copy + 'async_recursion,
    ) -> Result<Self> {
        let shard_size = Self::shard_size()?;
        match self {
            Self::Terminal { values } => {
                let values = Self::update_map(values, replacements, deleter);
                if values.len() <= shard_size {
                    // Case 1: values is small enough, return a terminal node
                    Ok(Self::Terminal { values })
                } else {
                    // Case 2: This will become a intermediate node
                    // Let's reuse the logic to add values to a intermediate node by creating
                    // an empty one.
                    let lcp = values
                        .keys()
                        .map(|k| k.as_slice())
                        .reduce(common_prefix)
                        .unwrap_or(b"");
                    // Setting the correct prefix is not necessary for correctness, but it avoids
                    // having Case 3 + Case 4.3.2 + compression unnecessarily.
                    Self::intermediate(SmallBinary::from_slice(lcp), None, Default::default())
                        .update(
                            ctx,
                            blobstore,
                            values
                                .into_iter()
                                .map(|(k, v)| (Bytes::copy_from_slice(k.as_ref()), Some(v)))
                                .collect(),
                            deleter,
                        )
                        .await
                }
            }
            Self::Intermediate {
                mut prefix,
                mut value,
                mut edges,
                ..
            } => {
                // LCP only considered added keys
                let lcp = replacements
                    .iter()
                    .filter_map(|(k, v)| v.as_ref().map(|_| k))
                    .fold(prefix.as_slice(), |lcp, key| common_prefix(lcp, key))
                    .len();
                if lcp < prefix.len() {
                    // Case 3: The prefix of all keys is smaller than `prefix`
                    // Let's create two new nodes and recursively update them.
                    // Right: Bytes lcp + 1 .. size
                    let prefix_right = prefix.drain(lcp + 1..).collect();
                    // Middle: Byte lcp
                    // unwrap safety: lcp + 1 > 0, so split_off leaves at least one element at prefix
                    let mid_byte = prefix.pop().unwrap();
                    // Left: Bytes 0 .. lcp
                    let prefix_left = prefix;
                    let right_node = Self::intermediate(prefix_right, value, edges);
                    let left_node = Self::intermediate(
                        prefix_left,
                        None,
                        // Design decision: all intermediate nodes are inlined
                        sorted_vector_map! {
                            mid_byte =>
                            ShardedMapEdge {
                                size: right_node.size(),
                                child: ShardedMapChild::Inlined(right_node),
                            },
                        },
                    );
                    left_node
                        .update(ctx, blobstore, replacements, deleter)
                        .await
                } else {
                    // Case 4: All added keys traverse the long edge (have the prefix `prefix`)
                    let mut partitioned = BTreeMap::<u8, BTreeMap<Bytes, Option<Value>>>::new();
                    // Step 4.1: Strip prefixes, and partition replacements.
                    replacements.into_iter().for_each(|(k, v)| {
                        match k.strip_prefix(prefix.as_slice()) {
                            None => {
                                // Only deletions might not have the correct prefix
                                debug_assert!(v.is_none());
                            }
                            Some(rest) => {
                                if let Some((first, rest)) = rest.split_first() {
                                    partitioned
                                        .entry(*first)
                                        .or_default()
                                        // Panic safety: rest was produced from k
                                        .insert(k.slice_ref(rest), v);
                                } else {
                                    std::mem::replace(&mut value, v).map(deleter);
                                }
                            }
                        }
                    });

                    // Step 4.2: Recursively update partitioned children
                    let replaced_futures = partitioned
                        .into_iter()
                        .map(|(next_byte, replacements)| {
                            let edge = edges.remove(&next_byte).unwrap_or_default();
                            async move {
                                let node = edge.load_child(ctx, blobstore).await?;
                                let replaced_node =
                                    node.update(ctx, blobstore, replacements, deleter).await?;
                                Ok((next_byte, replaced_node))
                            }
                        })
                        .collect::<Vec<_>>();
                    let replaced = stream::iter(replaced_futures)
                        .buffer_unordered(100)
                        .try_collect::<Vec<_>>()
                        .await?;
                    let mut new_children = BTreeMap::new();
                    for (next_byte, replaced_node) in replaced {
                        if !replaced_node.is_empty() {
                            let previous = new_children.insert(next_byte, replaced_node);
                            debug_assert!(previous.is_none());
                        }
                    }

                    let new_size: usize = edges.values().map(|edge| edge.size).sum::<usize>()
                        + new_children.values().map(|v| v.size()).sum::<usize>()
                        + value.iter().len();

                    if new_size <= shard_size {
                        // Case 4.3.1: Compress node into terminal node.
                        // For simplicity, reuse into_entries.
                        // In practice, all children will be terminal nodes, so nothing extra
                        // will be unnecessarily persisted into the blobstore.
                        for (byte, node) in new_children {
                            debug_assert!(matches!(node, Self::Terminal { .. }));
                            let previous = edges.insert(
                                byte,
                                ShardedMapEdge {
                                    size: node.size(),
                                    child: ShardedMapChild::Inlined(node),
                                },
                            );
                            debug_assert!(previous.is_none());
                        }
                        let values = Self::intermediate(prefix, value, edges)
                            .into_entries(ctx, blobstore)
                            // Extending SortedVectorMap 1 by 1 will be fast because into_entries
                            // returns elements in order
                            .try_collect()
                            .await?;

                        Ok(Self::Terminal { values })
                    } else {
                        // Case 4.3.2: This will continue being a intermediate node, let's
                        // inline what's necessary and store everything
                        let new_edges = stream::iter(new_children)
                            .map(|(byte, node)| async move {
                                let size = node.size();
                                let child = match &node {
                                    // Design decision: Inline all intermediate nodes and store
                                    // terminal nodes separated
                                    Self::Intermediate { .. } => ShardedMapChild::Inlined(node),
                                    Self::Terminal { .. } => ShardedMapChild::Id(
                                        node.into_blob().store(ctx, blobstore).await?,
                                    ),
                                };
                                Ok((byte, ShardedMapEdge { size, child }))
                            })
                            .buffer_unordered(100)
                            .try_collect::<Vec<_>>()
                            .await?;
                        for (byte, edge) in new_edges {
                            let previous = edges.insert(byte, edge);
                            debug_assert!(previous.is_none());
                        }
                        debug_assert!(!edges.is_empty());
                        if edges.len() == 1 && value.is_none() {
                            // Unwrap safety: edges.len() == 1 above
                            let (byte, edge) = edges.into_iter().next().unwrap();
                            let mut child = edge.load_child(ctx, blobstore).await?;
                            prefix.push(byte);
                            child.prepend(prefix);
                            Ok(child)
                        } else {
                            Ok(Self::intermediate(prefix, value, edges))
                        }
                    }
                }
            }
        }
    }

    /// Iterates through all values in the map, asynchronously and only loading
    /// blobs as needed.
    // See the detailed description of the logic in docs/sharded_map.md
    pub fn into_entries<'a>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a impl KeyedBlobstore,
    ) -> impl Stream<Item = Result<(SmallBinary, Value)>> + 'a {
        self.into_prefix_entries(ctx, blobstore, &[])
    }

    /// Iterates through all values like `into_entries`, but also leaks information about
    /// the shard ids for each encountered ShardMapNode which is stored in the blobstore
    pub fn into_sharded_entries<'a>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a impl KeyedBlobstore,
    ) -> impl Stream<Item = Result<ShardedTraversalOutput<'a, Value>>> + 'a {
        self.into_sharded_prefix_entries(ctx, blobstore, &[])
    }

    pub fn into_prefix_entries<'a>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a impl KeyedBlobstore,
        prefix: &'a [u8],
    ) -> impl Stream<Item = Result<(SmallBinary, Value)>> + 'a {
        bounded_traversal_ordered_stream(
            nonzero!(256usize),
            nonzero!(256usize),
            vec![(
                self.size(),
                (SmallBinary::new(), prefix, ShardedMapChild::Inlined(self)),
            )],
            move |(mut cur_prefix, remaining_prefix, child): (
                SmallBinary,
                &[u8],
                ShardedMapChild<Value>,
            )| {
                async move {
                    Ok(match child.load(ctx, blobstore).await? {
                        // Case 1. Prepend all keys with cur_prefix and output elements
                        Self::Terminal { values } => values
                            .into_iter()
                            .filter(|(k, _)| k.starts_with(remaining_prefix))
                            .map(|(key, value)| {
                                let mut full_key = cur_prefix.clone();
                                full_key.extend(key);
                                OrderedTraversal::Output((full_key, value))
                            })
                            .collect::<Vec<_>>(),
                        // Case 2. Recurse
                        Self::Intermediate {
                            prefix: new_prefix,
                            value,
                            edges,
                            ..
                        } => {
                            let remaining_prefix = if remaining_prefix.len() >= new_prefix.len() {
                                if let Some(new_remaining) =
                                    remaining_prefix.strip_prefix(new_prefix.as_slice())
                                {
                                    new_remaining
                                } else {
                                    // prefix doesn't match
                                    return Ok(vec![]);
                                }
                            } else if new_prefix.starts_with(remaining_prefix) {
                                &[]
                            } else {
                                // prefix doesn't match
                                return Ok(vec![]);
                            };
                            // Step 2-a. Extend cur_prefix
                            cur_prefix.extend(new_prefix);
                            let cur_prefix = &cur_prefix;
                            remaining_prefix
                                .is_empty()
                                .then_some(value)
                                .flatten()
                                // Step 2-b. If value is present (and prefix empty), output (cur_prefix, value)
                                .map(|value| OrderedTraversal::Output((cur_prefix.clone(), value)))
                                .into_iter()
                                // Step 2-c. Copy prefix, append byte, and recurse.
                                .chain(edges.into_iter().filter_map(|(byte, edge)| {
                                    let (first, rest) =
                                        remaining_prefix.split_first().unwrap_or((&byte, &[]));
                                    if *first == byte {
                                        let mut new_prefix = cur_prefix.clone();
                                        new_prefix.push(byte);
                                        let size_prediction = edge.size;
                                        Some(OrderedTraversal::Recurse(
                                            size_prediction,
                                            (new_prefix, rest, edge.child),
                                        ))
                                    } else {
                                        // Byte didn't match prefix
                                        None
                                    }
                                }))
                                .collect::<Vec<_>>()
                        }
                    })
                }
                .boxed()
            },
        )
    }

    // Traverse the `ShardedMap` down to the next blob, be it a non-inlined `ShardedMapChild` or a `Value`
    // and return the current state of the traversal at this point.
    // This allows us to traverse large sharded entries without loading unnecessary blobs if we
    // don't need to. For instance, it can be used to diff sharded manifests and decide not to
    // load non-inlined shards which are the same between both manifests.
    fn into_sharded_prefix_entries<'a>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a impl KeyedBlobstore,
        prefix: &'a [u8],
    ) -> impl Stream<Item = Result<ShardedTraversalOutput<'a, Value>>> + 'a {
        bounded_traversal_ordered_stream(
            nonzero!(256usize),
            nonzero!(256usize),
            vec![(
                self.size(),
                (SmallBinary::new(), prefix, ShardedMapChild::Inlined(self)),
            )],
            move |(cur_prefix, remaining_prefix, child): (
                SmallBinary,
                &[u8],
                ShardedMapChild<Value>,
            )| {
                async move {
                    Self::sharded_traversal_step(
                        ctx,
                        blobstore,
                        cur_prefix,
                        remaining_prefix,
                        child,
                    )
                    .await
                }
                .boxed()
            },
        )
    }

    async fn sharded_traversal_step<'a>(
        ctx: &'a CoreContext,
        blobstore: &'a impl KeyedBlobstore,
        mut cur_prefix: SmallBinary,
        remaining_prefix: &'a [u8],
        child: ShardedMapChild<Value>,
    ) -> Result<
        Vec<
            OrderedTraversal<
                ShardedTraversalOutput<'a, Value>,
                (SmallBinary, &'a [u8], ShardedMapChild<Value>),
            >,
        >,
    > {
        // When this `ShardedMapChild` is inlined, we're not really loading from the blobstore but we fetch from memory instead
        Ok(match child.load(ctx, blobstore).await? {
            // Case 1. Prepend all keys with cur_prefix and output elements
            Self::Terminal { values } => values
                .into_iter()
                .filter(|(k, _)| k.starts_with(remaining_prefix))
                .map(|(key, value)| {
                    let mut full_key = cur_prefix.clone();
                    full_key.extend(key);
                    OrderedTraversal::Output(ShardedTraversalOutput::Value(full_key, value))
                })
                .collect::<Vec<_>>(),
            // Case 2. Recurse
            Self::Intermediate {
                prefix: new_prefix,
                value,
                edges,
                ..
            } => {
                let remaining_prefix = if remaining_prefix.len() >= new_prefix.len() {
                    if let Some(new_remaining) =
                        remaining_prefix.strip_prefix(new_prefix.as_slice())
                    {
                        new_remaining
                    } else {
                        // prefix doesn't match
                        return Ok(vec![]);
                    }
                } else if new_prefix.starts_with(remaining_prefix) {
                    &[]
                } else {
                    // prefix doesn't match
                    return Ok(vec![]);
                };
                // Step 2-a. Extend cur_prefix
                cur_prefix.extend(new_prefix);
                let cur_prefix = &cur_prefix;
                remaining_prefix
                    .is_empty()
                    .then_some(value)
                    .flatten()
                    // Step 2-b. If value is present (and prefix empty), output (cur_prefix, value)
                    .map(|value| {
                        OrderedTraversal::Output(ShardedTraversalOutput::Value(
                            cur_prefix.clone(),
                            value,
                        ))
                    })
                    .into_iter()
                    // Step 2-c. Copy prefix, append byte, and recurse.
                    .chain(edges.into_iter().filter_map(|(byte, edge)| {
                        let (first, rest) = remaining_prefix.split_first().unwrap_or((&byte, &[]));
                        if *first == byte {
                            let mut new_prefix = cur_prefix.clone();
                            new_prefix.push(byte);
                            let size_prediction = edge.size;
                            // TODO (Pierre): don't recurse here, but this is where we
                            // return the shard and maybe its index, maybe its
                            // prefix, maybe both
                            match edge.child {
                                ShardedMapChild::Inlined(_) => Some(OrderedTraversal::Recurse(
                                    size_prediction,
                                    (new_prefix, rest, edge.child),
                                )),
                                // If this shard lives in the blobstore, return its id
                                // alongside the current prefix and the rest so we
                                // can resume the traversal later
                                ShardedMapChild::Id(id) => Some(OrderedTraversal::Output(
                                    ShardedTraversalOutput::Shard(new_prefix, rest, id),
                                )),
                            }
                        } else {
                            // Byte didn't match prefix
                            None
                        }
                    }))
                    .collect::<Vec<_>>()
            }
        })
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Terminal { values } => values.is_empty(),
            Self::Intermediate { .. } => self.size() == 0,
        }
    }

    fn size(&self) -> usize {
        match self {
            Self::Terminal { values } => values.len(),
            Self::Intermediate {
                value, edges, size, ..
            } => *size.get_or_init(|| {
                value.iter().len() + edges.values().map(|edge| edge.size).sum::<usize>()
            }),
        }
    }
}

pub enum ShardedTraversalOutput<'a, Value: MapValue> {
    /// We found a value. Return the key/value pair
    Value(SmallBinary, Value),
    /// We found a non-inlined shard. Return the current prefix, the remainder and the shard id.
    Shard(SmallBinary, &'a [u8], Value::Id),
}

impl<Value: MapValue> ThriftConvert for ShardedMapNode<Value> {
    const NAME: &'static str = "ShardedMapNode";
    type Thrift = thrift::sharded_map::ShardedMapNode;

    fn from_thrift(t: thrift::sharded_map::ShardedMapNode) -> Result<Self> {
        Ok(match t {
            thrift::sharded_map::ShardedMapNode::intermediate(intermediate) => Self::Intermediate {
                prefix: intermediate.prefix.0,
                value: intermediate
                    .value
                    .as_ref()
                    .map(ThriftConvert::from_bytes)
                    .transpose()?,
                edges: intermediate
                    .edges
                    .into_iter()
                    .map(|(k, e)| Ok((k as u8, ShardedMapEdge::from_thrift(e)?)))
                    .collect::<Result<_>>()?,
                size: Default::default(),
            },
            thrift::sharded_map::ShardedMapNode::terminal(terminal) => Self::Terminal {
                values: terminal
                    .values
                    .into_iter()
                    .map(|(k, v)| Ok((k.0, Value::from_bytes(&v)?)))
                    .collect::<Result<_>>()?,
            },
            thrift::sharded_map::ShardedMapNode::UnknownField(_) => {
                bail!("Unknown map node variant")
            }
        })
    }

    fn into_thrift(self) -> thrift::sharded_map::ShardedMapNode {
        match self {
            Self::Intermediate {
                prefix,
                value,
                edges,
                ..
            } => thrift::sharded_map::ShardedMapNode::intermediate(
                thrift::sharded_map::ShardedMapIntermediateNode {
                    prefix: thrift::data::SmallBinary(prefix),
                    value: value.map(ThriftConvert::into_bytes),
                    edges: edges
                        .into_iter()
                        .map(|(k, e)| (k as i8, e.into_thrift()))
                        .collect(),
                },
            ),
            Self::Terminal { values } => thrift::sharded_map::ShardedMapNode::terminal(
                thrift::sharded_map::ShardedMapTerminalNode {
                    values: values
                        .into_iter()
                        .map(|(k, v)| (thrift::data::SmallBinary(k), v.into_bytes()))
                        .collect(),
                },
            ),
        }
    }
}

impl<Value: MapValue> BlobstoreValue for ShardedMapNode<Value> {
    type Key = Value::Id;

    fn into_blob(self) -> Blob<Self::Key> {
        let data = self.into_bytes();
        let id = Value::Context::id_from_data(&data);
        Blob::new(id, data)
    }

    fn from_blob(blob: Blob<Self::Key>) -> Result<Self> {
        Self::from_bytes(blob.data())
    }
}

#[cfg(test)]
mod test {
    use std::collections::BTreeSet;
    use std::collections::HashSet;
    use std::str::FromStr;

    use ShardedMapNode::*;
    use async_trait::async_trait;
    use blobstore::BlobstoreKeyParam;
    use blobstore::BlobstoreKeyRange;
    use blobstore::BlobstoreKeySource;
    use blobstore::KeyedBlobstore;
    use blobstore::LoadableError;
    use context::CoreContext;
    use fbinit::FacebookInit;
    use futures::TryStreamExt;
    use memblob::KeyedMemblob;
    use mononoke_macros::mononoke;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::impl_typed_hash;
    use crate::private::Blake2;
    use crate::typed_hash::BlobstoreKey;

    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub struct MyType(i32);

    #[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug, Hash)]
    pub struct ShardedMapNodeMyId(Blake2);

    impl_typed_hash! {
        hash_type => ShardedMapNodeMyId,
        thrift_hash_type => thrift::id::ShardedMapNodeId,
        value_type => ShardedMapNode<MyType>,
        context_type => ShardedMapNodeMyContext,
        context_key => "mytype.mapnode",
    }

    impl MapValue for MyType {
        type Id = ShardedMapNodeMyId;
        type Context = ShardedMapNodeMyContext;
    }

    impl ThriftConvert for MyType {
        const NAME: &'static str = "MyType";
        type Thrift = i32;
        fn into_thrift(self) -> Self::Thrift {
            self.0
        }
        fn from_thrift(t: Self::Thrift) -> Result<Self> {
            Ok(MyType(t))
        }
    }

    type TestShardedMap = ShardedMapNode<MyType>;
    type TestShardedTrieMap = ShardedTrieMap<MyType>;

    fn terminal(values: Vec<(&str, i32)>) -> TestShardedMap {
        ShardedMapNode::Terminal {
            values: values
                .into_iter()
                .map(|(k, v)| (SmallVec::from_slice(k.as_bytes()), MyType(v)))
                .collect(),
        }
    }

    fn intermediate(
        prefix: &str,
        value: Option<i32>,
        children: Vec<(char, TestShardedMap)>,
    ) -> TestShardedMap {
        ShardedMapNode::Intermediate {
            prefix: SmallVec::from_slice(prefix.as_bytes()),
            value: value.map(MyType),
            edges: children
                .into_iter()
                .map(|(c, v)| {
                    (
                        c as u32 as u8,
                        ShardedMapEdge {
                            size: v.size(),
                            child: ShardedMapChild::Inlined(v),
                        },
                    )
                })
                .collect(),
            size: Default::default(),
        }
    }

    /// Returns an example map based on the picture on https://fburl.com/2fqtp2rk
    fn example_map() -> TestShardedMap {
        let abac = terminal(vec![
            ("ab", 7),
            ("aba", 8),
            ("akkk", 9),
            ("ate", 10),
            ("axi", 11),
        ]);
        let abal = terminal(vec![("aba", 5), ("ada", 6)]);
        let a = intermediate("ba", Some(12), vec![('c', abac), ('l', abal)]);
        let o = terminal(vec![("miojo", 1), ("miux", 2), ("mundo", 3), ("mungal", 4)]);
        // root
        intermediate("", None, vec![('a', a), ('o', o)])
    }

    const EXAMPLE_ENTRIES: &[(&str, i32)] = &[
        ("aba", 12),
        ("abacab", 7),
        ("abacaba", 8),
        ("abacakkk", 9),
        ("abacate", 10),
        ("abacaxi", 11),
        ("abalaba", 5),
        ("abalada", 6),
        ("omiojo", 1),
        ("omiux", 2),
        ("omundo", 3),
        ("omungal", 4),
    ];

    fn assert_round_trip(map: TestShardedMap) {
        let map_t = map.clone().into_thrift();
        // This is not deep equality through blobstore
        assert_eq!(ShardedMapNode::from_thrift(map_t).unwrap(), map);
    }

    #[derive(Clone)]
    struct MapHelper(TestShardedMap, CoreContext, KeyedMemblob);
    impl MapHelper {
        async fn from_entries(
            ctx: CoreContext,
            blobstore: KeyedMemblob,
            entries: Vec<(&str, Either<MyType, ShardedMapEdge<MyType>>)>,
        ) -> Result<Self> {
            let map = Self(
                ShardedMapNode::from_entries(
                    &ctx,
                    &blobstore,
                    entries
                        .into_iter()
                        .map(|(key, entry)| (Bytes::copy_from_slice(key.as_bytes()), entry))
                        .collect(),
                )
                .await?,
                ctx,
                blobstore,
            );
            map.validate().await?;
            Ok(map)
        }

        fn into_trie_map(self) -> TestShardedTrieMap {
            ShardedTrieMap::new(self.0)
        }

        async fn into_edge(self) -> Result<ShardedMapEdge<MyType>> {
            let MapHelper(map, _, _) = self;

            Ok(ShardedMapEdge {
                size: map.size(),
                child: ShardedMapChild::Inlined(map),
            })
        }

        fn size(&self) -> usize {
            self.0.size()
        }

        async fn lookup(&self, key: &str) -> Result<Option<i32>> {
            let v = self.0.lookup(&self.1, &self.2, key.as_bytes()).await?;
            Ok(v.map(|my_type| my_type.0))
        }

        fn entries(&self) -> impl Stream<Item = Result<(String, i32)>> + '_ {
            self.0
                .clone()
                .into_entries(&self.1, &self.2)
                .and_then(|(k, v)| async move { Ok((String::from_utf8(k.to_vec())?, v.0)) })
        }

        fn prefix_entries<'a>(
            &'a self,
            prefix: &'a str,
        ) -> impl Stream<Item = Result<(String, i32)>> + 'a {
            self.0
                .clone()
                .into_prefix_entries(&self.1, &self.2, prefix.as_bytes())
                .and_then(|(k, v)| async move { Ok((String::from_utf8(k.to_vec())?, v.0)) })
        }

        async fn assert_entries(&self, entries: &[(&str, i32)]) -> Result<()> {
            assert_eq!(
                self.entries().try_collect::<Vec<_>>().await?,
                entries
                    .iter()
                    .map(|(k, v)| (String::from(*k), *v))
                    .collect::<Vec<_>>()
            );
            Ok(())
        }

        async fn assert_prefix_entries(&self, prefix: &str, entries: &[(&str, i32)]) -> Result<()> {
            assert_eq!(
                self.prefix_entries(prefix).try_collect::<Vec<_>>().await?,
                entries
                    .iter()
                    .map(|(k, v)| (String::from(*k), *v))
                    .collect::<Vec<_>>()
            );
            Ok(())
        }

        async fn assert_example_map(&self) -> Result<()> {
            self.assert_entries(EXAMPLE_ENTRIES).await?;

            self.assert_prefix_entries("", EXAMPLE_ENTRIES).await?;
            self.assert_prefix_entries("aba", &EXAMPLE_ENTRIES[0..8])
                .await?;
            self.assert_prefix_entries("abaca", &EXAMPLE_ENTRIES[1..6])
                .await?;
            self.assert_prefix_entries("omi", &EXAMPLE_ENTRIES[8..10])
                .await?;
            self.assert_prefix_entries("om", &EXAMPLE_ENTRIES[8..])
                .await?;
            self.assert_prefix_entries("o", &EXAMPLE_ENTRIES[8..])
                .await?;
            self.assert_prefix_entries("ban", &[]).await?;

            self.assert_intermediate(2);
            self.child('a').await?.assert_intermediate(2);
            self.child('o').await?.assert_terminal(4);
            self.child('a').await?.child('c').await?.assert_terminal(5);
            self.child('a').await?.child('l').await?.assert_terminal(2);

            Ok(())
        }

        async fn add_remove(
            &mut self,
            to_add: &[(&str, i32)],
            to_remove: &[&str],
        ) -> Result<Vec<i32>> {
            let map = std::mem::take(&mut self.0);
            let (send, recv) = crossbeam::channel::unbounded();
            self.0 = map
                .update(
                    &self.1,
                    &self.2,
                    to_add
                        .iter()
                        .map(|(k, v)| (Bytes::copy_from_slice(k.as_bytes()), Some(MyType(*v))))
                        .chain(
                            to_remove
                                .iter()
                                .map(|k| (Bytes::copy_from_slice(k.as_bytes()), None)),
                        )
                        .collect(),
                    |x| send.send(x.0).unwrap(),
                )
                .await?;
            self.validate().await?;
            Ok(recv.try_iter().collect())
        }

        #[async_recursion]
        async fn validate(&self) -> Result<()> {
            let size = self.size();
            assert_eq!(self.0.is_empty(), size == 0);
            match &self.0 {
                Terminal { values } => assert!(values.len() <= 5),
                Intermediate {
                    prefix: _,
                    value,
                    edges,
                    size: _,
                } => {
                    let children_size: usize = stream::iter(
                        edges
                            .into_iter()
                            .map(|(_, e)| async move {
                                anyhow::Ok((e.size, e.child.clone().load(&self.1, &self.2).await?))
                            })
                            // prevent compiler bug
                            .collect::<Vec<_>>(),
                    )
                    .buffer_unordered(100)
                    .and_then(|(size, child)| async move {
                        let child = Self(child, self.1.clone(), self.2.clone());
                        child.validate().await?;
                        assert_eq!(child.size(), size);
                        Ok(size)
                    })
                    .try_collect::<Vec<_>>()
                    .await?
                    .into_iter()
                    .sum();
                    assert_eq!(children_size + value.iter().len(), size);
                }
            }
            Ok(())
        }

        #[async_recursion]
        async fn inner_inline_all(
            &self,
            mut map: ShardedMapNode<MyType>,
        ) -> Result<ShardedMapNode<MyType>> {
            match &mut map {
                Terminal { .. } => {}
                Intermediate { edges, .. } => {
                    for (_, edge) in edges {
                        let node = std::mem::take(&mut edge.child)
                            .load(&self.1, &self.2)
                            .await?;
                        edge.child = ShardedMapChild::Inlined(self.inner_inline_all(node).await?);
                    }
                }
            }
            Ok(map)
        }

        async fn inline_all(&mut self) -> Result<()> {
            let map = std::mem::take(&mut self.0);
            self.0 = self.inner_inline_all(map).await?;
            Ok(())
        }

        async fn child(&self, key: char) -> Result<Self> {
            let child = match &self.0 {
                Terminal { .. } => bail!("terminal"),
                Intermediate { edges, .. } => {
                    edges
                        .get(&(key as u8))
                        .unwrap()
                        .child
                        .clone()
                        .load(&self.1, &self.2)
                        .await?
                }
            };
            Ok(Self(child, self.1.clone(), self.2.clone()))
        }

        fn assert_terminal(&self, values_len: usize) {
            match &self.0 {
                Intermediate { .. } => panic!("not terminal"),
                Terminal { values } => assert_eq!(values.len(), values_len),
            }
        }
        fn assert_intermediate(&self, child_count: usize) {
            match &self.0 {
                Terminal { .. } => panic!("not intermediate"),
                Intermediate { edges, .. } => assert_eq!(edges.len(), child_count),
            }
        }
        fn assert_prefix(&self, prefix: &str) {
            match &self.0 {
                Terminal { .. } => panic!("not intermediate"),
                Intermediate {
                    prefix: my_prefix, ..
                } => assert_eq!(my_prefix.as_slice(), prefix.as_bytes()),
            }
        }
    }

    #[mononoke::test]
    fn basic_test() {
        let empty = ShardedMapNode::<MyType>::Terminal {
            values: Default::default(),
        };
        assert!(empty.is_empty());
        assert_eq!(empty.size(), 0);
        let empty = ShardedMapNode::<MyType>::Intermediate {
            value: None,
            edges: Default::default(),
            prefix: Default::default(),
            size: Default::default(),
        };
        assert!(empty.is_empty());
        assert_eq!(empty.size(), 0);
        assert!(ShardedMapNode::<MyType>::default().is_empty());

        let map = terminal(vec![("ab", 3), ("cd", 5)]);
        assert!(!map.is_empty());
        assert_round_trip(map);

        let map = example_map();
        assert!(!map.is_empty());
        assert_eq!(map.size(), 12);
        assert_round_trip(map);
    }

    #[mononoke::fbinit_test]
    async fn lookup_test(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();

        let map = MapHelper(example_map(), ctx, blobstore);
        // Case 2 > Case 1
        assert_eq!(map.lookup("omiux").await?, Some(2));
        // Case 3
        assert_eq!(map.lookup("inexistent").await?, None);
        // Case 2 > Case 5
        assert_eq!(map.lookup("abxio").await?, None);
        // Case 2 > Case 4
        assert_eq!(map.lookup("aba").await?, Some(12));
        // Case 2 > Case 2 > Case 1
        assert_eq!(map.lookup("abacakkk").await?, Some(9));
        assert_eq!(map.lookup("abacakk").await?, None);
        // Case 4
        assert_eq!(map.lookup("").await?, None);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn lookup_sorted_matches_lookup_and_loads_each_stored_node_once(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given a map whose entries are spread over stored nodes, and sorted
        // keys that are present, absent, and prefixes of present keys
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();
        let mut map = MapHelper(Default::default(), ctx.clone(), blobstore.clone());
        map.add_remove(EXAMPLE_ENTRIES, &[]).await?;
        let keys = EXAMPLE_ENTRIES
            .iter()
            .map(|(key, _)| *key)
            .chain(["", "ab", "abac", "abacakk", "abz", "b", "omiux2", "z"])
            .map(str::as_bytes)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut expected = Vec::new();
        for key in &keys {
            expected.push(map.0.lookup(&ctx, &blobstore, key).await?);
        }
        let loads = std::sync::Mutex::new(Vec::new());

        // When every key is looked up in one sorted pass
        let found = map
            .0
            .lookup_sorted(&keys, &|id: ShardedMapNodeMyId| {
                loads.lock().unwrap().push(id);
                let (ctx, blobstore) = (&ctx, &blobstore);
                async move { id.load(ctx, blobstore).await.map_err(Into::into) }
            })
            .await?;

        // Then each result equals `lookup`'s, and no stored node is loaded twice
        assert_eq!(found, expected);
        let loads = loads.into_inner().unwrap();
        assert_eq!(loads.len(), loads.iter().collect::<HashSet<_>>().len());
        assert!(!loads.is_empty());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn into_entries_test(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();

        let map = MapHelper(example_map(), ctx, blobstore);
        map.assert_example_map().await
    }

    async fn get_all_keys<T: BlobstoreKeySource>(
        ctx: &CoreContext,
        blobstore: &T,
    ) -> Result<impl Iterator<Item = String> + use<T>> {
        let data = blobstore
            .enumerate(
                ctx,
                &BlobstoreKeyParam::Start(BlobstoreKeyRange {
                    begin_key: String::new(),
                    end_key: String::new(),
                }),
            )
            .await?;
        if data.next_token.is_some() {
            unimplemented!();
        }
        let mut data: Vec<_> = data.keys.into_iter().collect();
        data.sort();
        Ok(data.into_iter())
    }

    async fn assert_all_keys(
        ctx: &CoreContext,
        blobstore: &KeyedMemblob,
        keys: Vec<&str>,
    ) -> Result<()> {
        assert_eq!(
            get_all_keys(ctx, blobstore.as_inner())
                .await?
                .collect::<Vec<_>>(),
            keys.into_iter().map(String::from).collect::<Vec<_>>()
        );
        Ok(())
    }

    async fn assert_key_count(
        ctx: &CoreContext,
        blobstore: &KeyedMemblob,
        count: usize,
    ) -> Result<()> {
        assert_eq!(
            get_all_keys(ctx, blobstore.as_inner()).await?.count(),
            count
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn store_test(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();
        let map = example_map();
        map.into_blob().store(&ctx, &blobstore).await?;
        assert_all_keys(&ctx, &blobstore, vec!["mytype.mapnode.blake2.9239707907ceb346d7146c601f131ab7c598a8e98441c2934817c964f0a2c270"]).await?;
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn update_basic_test(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();
        let mut map = MapHelper(Default::default(), ctx.clone(), blobstore.clone());
        map.assert_entries(&[]).await?;
        map.add_remove(EXAMPLE_ENTRIES, &[]).await?;
        map.assert_entries(EXAMPLE_ENTRIES).await?;
        assert_all_keys(
            &ctx,
            &blobstore,
            vec!["mytype.mapnode.blake2.8c8f56e9612f7cc94187729e4eff067bf56bb239019e25c8421243a60e4d1fb9",
                "mytype.mapnode.blake2.e808435da65aa2e0f61db333fba1904e57b1b46dff2d3a5c263d0016750f1f0d",
                "mytype.mapnode.blake2.f8728f3aabc9b8083db7150e9c2636c2971dac0784fac59cee3b7b6908165476"],
        )
        .await?;
        {
            // Let's compare it to our hand-written map
            let mut map = map.clone();
            map.inline_all().await?;
            map.assert_entries(EXAMPLE_ENTRIES).await?;
            assert_eq!(map.0, example_map());
        }
        let deleted = map.add_remove(&[], &["abalaba", "non_existing"]).await?;
        assert_eq!(deleted, vec![5]);
        assert_eq!(map.0.size(), EXAMPLE_ENTRIES.len() - 1);
        assert_key_count(&ctx, &blobstore, 4).await?;
        let deleted = map.add_remove(&[], &["abalada"]).await?;
        assert_eq!(deleted, vec![6]);
        // Intermeditate node should now have 1 child, but also a value
        assert_key_count(&ctx, &blobstore, 4).await?;
        let child = map.child('a').await?;
        match child.0 {
            Terminal { .. } => bail!("not intermediate"),
            Intermediate { value, edges, .. } => {
                assert!(value.is_some());
                assert_eq!(edges.len(), 1);
            }
        }
        let deleted = map.add_remove(&[], &["aba"]).await?;
        assert_eq!(deleted, vec![12]);
        // Intermediate node without a value should be merged
        assert_key_count(&ctx, &blobstore, 5).await?;
        map.assert_entries(&[
            ("abacab", 7),
            ("abacaba", 8),
            ("abacakkk", 9),
            ("abacate", 10),
            ("abacaxi", 11),
            ("omiojo", 1),
            ("omiux", 2),
            ("omundo", 3),
            ("omungal", 4),
        ])
        .await?;
        map.child('a').await?.assert_terminal(5);
        assert!(
            map.add_remove(&[], &["abalada", "abalaba", "aba"])
                .await?
                .is_empty()
        );
        assert_eq!(
            map.add_remove(&[("potato", 1000), ("abacaxi", 1001)], &[])
                .await?,
            vec![11]
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn update_tricky_test(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();
        let mut map = MapHelper(Default::default(), ctx.clone(), blobstore.clone());
        map.add_remove(
            &[
                ("A11", 1),
                ("A12", 2),
                ("A13", 3),
                ("A21", 1),
                ("A22", 2),
                ("A23", 3),
            ],
            &[],
        )
        .await?;
        map.assert_intermediate(2);
        map.child('1').await?.assert_terminal(3);
        map.child('2').await?.assert_terminal(3);
        // LCP of keys is smaller than prefix only due to removals.
        map.add_remove(&[("A14", 4)], &["cz", "A", "A31"]).await?;
        map.assert_intermediate(2);
        map.child('1').await?.assert_terminal(4);
        map.child('2').await?.assert_terminal(3);
        map.add_remove(
            &[
                ("B11", 1),
                ("B21", 1),
                ("B22", 2),
                ("B23", 3),
                ("B24", 4),
                ("B31", 1),
            ],
            &["A11", "A12", "A13", "A14", "A21", "A22", "A23"],
        )
        .await?;
        map.assert_intermediate(3);
        map.child('1').await?.assert_terminal(1);
        map.child('2').await?.assert_terminal(4);
        map.child('3').await?.assert_terminal(1);
        map.add_remove(&[], &[""]).await?;
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn update_tricky_deletes_test(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();
        let mut map = MapHelper(Default::default(), ctx.clone(), blobstore.clone());
        map.add_remove(EXAMPLE_ENTRIES, &[]).await?;
        // Removing something that is not a prefix of the intermediate node
        // can panic if not done correctly
        map.add_remove(&[], &[""]).await?;
        map.add_remove(&[], &["a"]).await?;
        map.add_remove(&[], &["ab"]).await?;
        // Bug where we might mismatch prefix of deleted keys
        map.add_remove(&[], &["abx"]).await?;
        assert_eq!(map.size(), 12);
        map.add_remove(&[], &["abxlada"]).await?;
        assert_eq!(map.size(), 12);
        // Let's play with the value of an intermediate node and assert all is still good:
        map.add_remove(&[], &["aba"]).await?;
        assert_eq!(map.size(), 11);
        let child = map.child('a').await?;
        assert_eq!(child.size(), 7);
        assert_eq!(map.lookup("aba").await?, None);
        map.add_remove(&[("aba", 0)], &[]).await?;
        assert_eq!(map.size(), 12);
        assert_eq!(map.lookup("aba").await?, Some(0));
        map.add_remove(&[("aba", -1)], &[]).await?;
        assert_eq!(map.size(), 12);
        assert_eq!(map.lookup("aba").await?, Some(-1));
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn update_cases_test(fb: FacebookInit) -> Result<()> {
        // Let's try to do updates that cause different cases and assert it all works out
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();
        let mut map = MapHelper(Default::default(), ctx.clone(), blobstore.clone());
        // Case 1
        map.add_remove(&[("_a", 1), ("_b", 2), ("_c", 3)], &[])
            .await?;
        map.assert_terminal(3);
        // Case 2 > Case 4 > (Recursive Case 1's) > Case 4.3.2
        map.add_remove(&[("_d", 4), ("_e", 5), ("_f", 6)], &[])
            .await?;
        map.assert_intermediate(6);
        map.child('d').await?.assert_terminal(1);
        // Case 4 > (Recursive Case 1) > case 4.3.1
        map.add_remove(&[], &["_b"]).await?;
        map.assert_terminal(5);
        // Case 2 > Case 3 > Case 4 > (Recursive Case 1, Case 2>...) > Case 4.3.2
        map.add_remove(&[("_b", 2), ("z", -1)], &[]).await?;
        map.assert_intermediate(2);
        map.assert_prefix("");
        map.child('_').await?.assert_intermediate(6);
        map.child('z').await?.assert_terminal(1);
        // Case 4 > (Rec Case 1) > Case 4.3.2 + merge
        map.add_remove(&[], &["z"]).await?;
        map.assert_intermediate(6);
        map.assert_prefix("_");
        // Case 3 > Case 4 > Rec Case 1 > Case 4.3.2
        map.add_remove(&[("y", -2)], &[]).await?;
        map.assert_intermediate(2);
        map.assert_prefix("");
        // Case 4 > (Rec Case 1, Case 4>Rec Case1>Case 4.3.1) > Case 4.3.1
        map.add_remove(&[], &["_b", "y"]).await?;
        map.assert_terminal(5);
        map.assert_entries(&[("_a", 1), ("_c", 3), ("_d", 4), ("_e", 5), ("_f", 6)])
            .await?;
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn update_case_3_test(fb: FacebookInit) -> Result<()> {
        // Let's try an update that causes case 3 and do detailed asserting
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();
        let mut map = MapHelper(Default::default(), ctx.clone(), blobstore.clone());
        map.add_remove(
            &[
                ("abc1", 1),
                ("abc2", 2),
                ("abc3", 3),
                ("abc4", 4),
                ("abc5", 5),
                ("abc6", 6),
            ],
            &[],
        )
        .await?;
        map.assert_intermediate(6);
        map.assert_prefix("abc");
        map.add_remove(&[("a1", 1)], &[]).await?;
        map.assert_intermediate(2);
        map.assert_prefix("a");
        let childb = map.child('b').await?;
        childb.assert_prefix("c");
        childb.assert_intermediate(6);
        let child1 = map.child('1').await?;
        child1.assert_terminal(1);
        map.assert_entries(&[
            ("a1", 1),
            ("abc1", 1),
            ("abc2", 2),
            ("abc3", 3),
            ("abc4", 4),
            ("abc5", 5),
            ("abc6", 6),
        ])
        .await?;
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_from_entries_only_values(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();

        let map = MapHelper::from_entries(
            ctx,
            blobstore,
            EXAMPLE_ENTRIES
                .iter()
                .map(|(key, value)| (*key, Either::Left(MyType(*value))))
                .collect(),
        )
        .await?;

        map.assert_example_map().await
    }

    #[mononoke::fbinit_test]
    async fn test_from_entries_only_maps(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();

        let map_ab = MapHelper::from_entries(
            ctx.clone(),
            blobstore.clone(),
            EXAMPLE_ENTRIES[0..8]
                .iter()
                .map(|(key, value)| (&key[2..], Either::Left(MyType(*value))))
                .collect(),
        )
        .await?;

        let map_omi = MapHelper::from_entries(
            ctx.clone(),
            blobstore.clone(),
            EXAMPLE_ENTRIES[8..10]
                .iter()
                .map(|(key, value)| (&key[3..], Either::Left(MyType(*value))))
                .collect(),
        )
        .await?;

        let map_omu = MapHelper::from_entries(
            ctx.clone(),
            blobstore.clone(),
            EXAMPLE_ENTRIES[10..12]
                .iter()
                .map(|(key, value)| (&key[3..], Either::Left(MyType(*value))))
                .collect(),
        )
        .await?;

        let map = MapHelper::from_entries(
            ctx,
            blobstore,
            vec![
                ("ab", Either::Right(map_ab.into_edge().await?)),
                ("omi", Either::Right(map_omi.into_edge().await?)),
                ("omu", Either::Right(map_omu.into_edge().await?)),
            ],
        )
        .await?;

        map.assert_example_map().await?;

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_from_entries_maps_and_values(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();

        let map_ab = MapHelper::from_entries(
            ctx.clone(),
            blobstore.clone(),
            EXAMPLE_ENTRIES[0..8]
                .iter()
                .map(|(key, value)| (&key[2..], Either::Left(MyType(*value))))
                .collect(),
        )
        .await?;

        let map = MapHelper::from_entries(
            ctx,
            blobstore,
            std::iter::once(("ab", Either::Right(map_ab.into_edge().await?)))
                .chain(
                    EXAMPLE_ENTRIES[8..]
                        .iter()
                        .map(|(key, value)| (*key, Either::Left(MyType(*value)))),
                )
                .collect(),
        )
        .await?;

        map.assert_example_map().await?;

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_from_entries_conflict_detection(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();

        let map_first_six = MapHelper::from_entries(
            ctx.clone(),
            blobstore.clone(),
            EXAMPLE_ENTRIES[0..6]
                .iter()
                .map(|(key, value)| (&key[2..], Either::Left(MyType(*value))))
                .collect(),
        )
        .await?;

        let map_last_six = MapHelper::from_entries(
            ctx.clone(),
            blobstore.clone(),
            EXAMPLE_ENTRIES[6..12]
                .iter()
                .map(|(key, value)| (*key, Either::Left(MyType(*value))))
                .collect(),
        )
        .await?;

        assert!(
            MapHelper::from_entries(
                ctx.clone(),
                blobstore.clone(),
                vec![
                    ("ab", Either::Right(map_first_six.into_edge().await?)),
                    ("", Either::Right(map_last_six.into_edge().await?)),
                ],
            )
            .await
            .is_err()
        );

        let map_ab = MapHelper::from_entries(
            ctx.clone(),
            blobstore.clone(),
            EXAMPLE_ENTRIES[0..8]
                .iter()
                .map(|(key, value)| (&key[2..], Either::Left(MyType(*value))))
                .collect(),
        )
        .await?;

        let map_om = MapHelper::from_entries(
            ctx.clone(),
            blobstore.clone(),
            EXAMPLE_ENTRIES[8..12]
                .iter()
                .map(|(key, value)| (*key, Either::Left(MyType(*value))))
                .collect(),
        )
        .await?;

        assert!(
            MapHelper::from_entries(
                ctx.clone(),
                blobstore.clone(),
                vec![
                    ("ab", Either::Right(map_ab.clone().into_edge().await?)),
                    ("om", Either::Right(map_om.clone().into_edge().await?)),
                ],
            )
            .await
            .is_ok()
        );

        assert!(
            MapHelper::from_entries(
                ctx.clone(),
                blobstore.clone(),
                vec![
                    ("ab", Either::Right(map_ab.clone().into_edge().await?)),
                    ("om", Either::Right(map_om.clone().into_edge().await?)),
                    ("abababab", Either::Left(MyType(100))),
                ],
            )
            .await
            .is_err()
        );

        assert!(
            MapHelper::from_entries(
                ctx.clone(),
                blobstore.clone(),
                vec![
                    ("ab", Either::Right(map_ab.clone().into_edge().await?)),
                    ("om", Either::Right(map_om.clone().into_edge().await?)),
                    ("zz", Either::Left(MyType(100))),
                ],
            )
            .await
            .is_ok()
        );

        assert!(
            MapHelper::from_entries(
                ctx.clone(),
                blobstore.clone(),
                vec![
                    ("ab", Either::Right(map_ab.clone().into_edge().await?)),
                    ("om", Either::Right(map_om.clone().into_edge().await?)),
                    ("omo", Either::Left(MyType(100))),
                ],
            )
            .await
            .is_err()
        );

        assert!(
            MapHelper::from_entries(
                ctx.clone(),
                blobstore.clone(),
                vec![
                    ("o", Either::Left(MyType(100))),
                    ("ab", Either::Right(map_ab.clone().into_edge().await?)),
                    ("om", Either::Right(map_om.clone().into_edge().await?)),
                ],
            )
            .await
            .is_ok()
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_sharded_trie_map(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();

        let map = ShardedMapNode::from_entries(
            &ctx,
            &blobstore,
            EXAMPLE_ENTRIES
                .iter()
                .map(|(key, entry)| {
                    (
                        Bytes::copy_from_slice(key.as_bytes()),
                        Either::Left(MyType(*entry)),
                    )
                })
                .chain(std::iter::once((
                    Bytes::from_static("z".as_bytes()),
                    Either::Left(MyType(100)),
                )))
                .collect(),
        )
        .await?;

        let sharded_trie_map = ShardedTrieMap::new(map);

        let (root_value, children) = sharded_trie_map.expand(&ctx, &blobstore).await?;

        assert_eq!(root_value, None);
        assert_eq!(children.len(), 3);
        assert_eq!(children[0].0, b'a');
        assert_eq!(children[1].0, b'o');
        assert_eq!(children[2].0, b'z');

        let (a_value, a_children) = children[0].1.clone().expand(&ctx, &blobstore).await?;
        assert_eq!(a_value, None);
        assert_eq!(a_children.len(), 1);
        assert_eq!(a_children[0].0, b'b');

        let (o_value, o_children) = children[1].1.clone().expand(&ctx, &blobstore).await?;
        assert_eq!(o_value, None);
        assert_eq!(o_children.len(), 1);
        assert_eq!(o_children[0].0, b'm');

        let (z_value, z_children) = children[2].1.clone().expand(&ctx, &blobstore).await?;
        assert_eq!(z_value, Some(MyType(100)));
        assert_eq!(z_children.len(), 0);

        let (om_value, om_children) = o_children[0].1.clone().expand(&ctx, &blobstore).await?;
        assert_eq!(om_value, None);
        assert_eq!(om_children.len(), 2);
        assert_eq!(om_children[0].0, b'i');
        assert_eq!(om_children[1].0, b'u');

        assert_eq!(
            children[0]
                .1
                .clone()
                .into_stream(&ctx, &blobstore)
                .await?
                .try_collect::<Vec<_>>()
                .await?,
            EXAMPLE_ENTRIES[0..8]
                .iter()
                .map(|(key, entry)| {
                    let mut key = SmallBinary::from_slice(key.as_bytes());
                    key.remove(0);
                    (key, MyType(*entry))
                })
                .collect::<Vec<_>>(),
        );

        assert_eq!(
            children[1]
                .1
                .clone()
                .into_stream(&ctx, &blobstore)
                .await?
                .try_collect::<Vec<_>>()
                .await?,
            EXAMPLE_ENTRIES[8..12]
                .iter()
                .map(|(key, entry)| {
                    let mut key = SmallBinary::from_slice(key.as_bytes());
                    key.remove(0);
                    (key, MyType(*entry))
                })
                .collect::<Vec<_>>(),
        );

        Ok(())
    }

    #[mononoke::quickcheck_test]
    async fn round_trip_quickcheck(
        fb: FacebookInit,
        values: BTreeMap<String, i32>,
        extra_queries: Vec<String>,
    ) -> bool {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();
        let mut map = MapHelper(Default::default(), ctx, blobstore);

        // Add all values to the map
        let adds = values
            .iter()
            .map(|(k, v)| (k.as_str(), *v))
            .collect::<Vec<_>>();
        if map.add_remove(&adds, &[]).await.is_err() {
            return false;
        }

        // Check size matches
        if map.size() != values.len() {
            return false;
        }

        // Check all keys from the map exist with correct values
        for (k, v) in &values {
            if let Result::Ok(Some(test_v)) = map.lookup(k).await {
                if test_v != *v {
                    return false;
                }
            } else {
                return false;
            }
        }

        // Check extra queries (may or may not exist in the map)
        for k in &extra_queries {
            let correct_v = values.get(k).cloned();
            if let Result::Ok(test_v) = map.lookup(k).await {
                if test_v != correct_v {
                    return false;
                }
            } else {
                return false;
            }
        }

        // Check round trip - entries() should return the same BTreeMap
        if let Result::Ok(test_roundtrip) = map.entries().try_collect::<BTreeMap<_, _>>().await {
            values == test_roundtrip
        } else {
            false
        }
    }
}
