/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fmt::Debug;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::sync::OnceLock;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use async_recursion::async_recursion;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use blobstore::Storable;
use bounded_traversal::OrderedTraversal;
use context::CoreContext;
use derivative::Derivative;
use futures::FutureExt;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use futures::stream::FuturesUnordered;
use futures::stream::Stream;
use futures::try_join;
use futures_watchdog::WatchdogExt;
use itertools::Either;
use itertools::Itertools;
use nonzero_ext::nonzero;
use quickcheck::Arbitrary;
use quickcheck::Gen;
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

// More detailed documentation about ShardedMapV2 can be found in serialization/sharded_map.thrift

pub trait ShardedMapV2Value: ThriftConvert + Debug + Hash + Clone + Send + Sync + 'static {
    type NodeId: MononokeId<Thrift = thrift::id::ShardedMapV2NodeId, Value = ShardedMapV2Node<Self>>;
    type Context: IdContext<Id = Self::NodeId>;
    type RollupData: Rollup<Self>;

    // The maximum allowed weight for a sharded map node. The weight of a node is defined as
    // the sum of weights of all its inlined children, plus the count of its non-inlined children,
    // plus the weight of its value if it contains one itself.
    const WEIGHT_LIMIT: usize;

    /// The weight of a ShardedMapV2 value. In most cases this should always 1 so this is
    /// the default implementation. Only cases that have high variance between the sizes
    /// of values should override this.
    fn weight(&self) -> usize {
        1
    }
}

pub trait Rollup<Value: ShardedMapV2Value>:
    ThriftConvert + Debug + Hash + Clone + PartialEq + Eq + Send + Sync + 'static
{
    fn rollup(value: Option<&Value>, child_rollup_data: Vec<Self>) -> Self;
}

impl<Value: ShardedMapV2Value> Rollup<Value> for () {
    fn rollup(_value: Option<&Value>, _child_rollup_data: Vec<Self>) -> Self {}
}

type SmallBinary = SmallVec<[u8; 24]>;

#[derive(Derivative)]
#[derivative(PartialEq, Debug, Hash, Default(bound = ""))]
#[derive(Clone, Eq)]
pub struct ShardedMapV2Node<Value: ShardedMapV2Value> {
    #[derivative(Debug(format_with = "crate::debug::format_byte_string"))]
    prefix: SmallBinary,
    value: Option<Box<Value>>,
    #[derivative(Debug(format_with = "debug_format_children"))]
    children: SortedVectorMap<u8, LoadableShardedMapV2Node<Value>>,
    #[derivative(PartialEq = "ignore", Debug = "ignore", Hash = "ignore")]
    weight: OnceLock<usize>,
    #[derivative(PartialEq = "ignore", Debug = "ignore", Hash = "ignore")]
    size: OnceLock<usize>,
}

/// Debug format node children, showing the key as a byte literal.
fn debug_format_children<Value: ShardedMapV2Value>(
    children: &SortedVectorMap<u8, LoadableShardedMapV2Node<Value>>,
    f: &mut std::fmt::Formatter<'_>,
) -> std::fmt::Result {
    let mut d = f.debug_map();
    for (k, v) in children {
        d.key_with(|f| crate::debug::format_byte(k, f));
        d.value(v);
    }
    d.finish()?;
    Ok(())
}

#[derive(Derivative)]
#[derivative(Default(bound = ""))]
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum LoadableShardedMapV2Node<Value: ShardedMapV2Value> {
    #[derivative(Default)]
    Inlined(ShardedMapV2Node<Value>),
    Stored(ShardedMapV2StoredNode<Value>),
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct ShardedMapV2StoredNode<Value: ShardedMapV2Value> {
    id: Value::NodeId,
    weight: usize,
    size: usize,
    rollup_data: Value::RollupData,
}

impl<Value: ShardedMapV2Value> ShardedMapV2StoredNode<Value> {
    fn from_thrift(t: thrift::sharded_map::ShardedMapV2StoredNode) -> Result<Self> {
        Ok(Self {
            id: Value::NodeId::from_thrift(t.id)?,
            weight: t.weight as usize,
            size: t.size as usize,
            rollup_data: Value::RollupData::from_bytes(&t.rollup_data)?,
        })
    }

    fn into_thrift(self) -> thrift::sharded_map::ShardedMapV2StoredNode {
        thrift::sharded_map::ShardedMapV2StoredNode {
            id: self.id.into_thrift(),
            weight: self.weight as i64,
            size: self.size as i64,
            rollup_data: self.rollup_data.into_bytes(),
        }
    }
}

/// Returns longest common prefix of a and b.
fn common_prefix<'a>(a: &'a [u8], b: &'a [u8]) -> &'a [u8] {
    let lcp = a.iter().zip(b.iter()).take_while(|(a, b)| a == b).count();
    // Panic safety: lcp is at most a.len()
    &a[..lcp]
}

impl<Value: ShardedMapV2Value> LoadableShardedMapV2Node<Value> {
    /// Returns the underlying node, fetching from the blobstore
    /// if it's not inlined.
    pub async fn load(
        self,
        ctx: &CoreContext,
        blobstore: &impl KeyedBlobstore,
    ) -> Result<ShardedMapV2Node<Value>> {
        match self {
            Self::Inlined(inlined) => Ok(inlined),
            Self::Stored(stored) => stored
                .id
                .load(ctx, blobstore)
                .watched()
                .with_max_poll(blobstore::BLOBSTORE_MAX_POLL_TIME_MS)
                .await
                .with_context(|| "Failed to load stored sharded map node"),
        }
    }

    /// Returns an inlined variant of self.
    pub async fn inlined(
        self,
        ctx: &CoreContext,
        blobstore: &impl KeyedBlobstore,
    ) -> Result<LoadableShardedMapV2Node<Value>> {
        match self {
            inlined @ Self::Inlined(_) => Ok(inlined),
            Self::Stored(stored) => Ok(Self::Inlined(stored.id.load(ctx, blobstore).await?)),
        }
    }

    /// Returns a stored variant of self.
    pub async fn stored(
        self,
        ctx: &CoreContext,
        blobstore: &impl KeyedBlobstore,
    ) -> Result<LoadableShardedMapV2Node<Value>> {
        match self {
            Self::Inlined(inlined) => Ok(Self::Stored(ShardedMapV2StoredNode {
                weight: inlined.weight(),
                size: inlined.size(),
                rollup_data: inlined.rollup_data(),
                id: inlined.into_blob().store(ctx, blobstore).await?,
            })),
            stored @ Self::Stored(_) => Ok(stored),
        }
    }

    pub async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &impl KeyedBlobstore,
    ) -> Result<(Option<Value>, Vec<(u8, Self)>)> {
        let ShardedMapV2Node {
            prefix,
            value,
            children,
            ..
        } = self.load(ctx, blobstore).await?;

        match prefix.split_first() {
            Some((first_byte, rest)) => Ok((
                None,
                vec![(
                    *first_byte,
                    LoadableShardedMapV2Node::Inlined(ShardedMapV2Node {
                        prefix: SmallBinary::from(rest),
                        value,
                        children,
                        ..Default::default()
                    }),
                )],
            )),
            None => Ok((value.map(|v| *v), children.into_iter().collect())),
        }
    }

    /// Returns the weight of the underlying node.
    fn weight(&self) -> usize {
        match self {
            Self::Inlined(inlined) => inlined.weight(),
            Self::Stored(stored) => stored.weight,
        }
    }

    pub fn size(&self) -> usize {
        match self {
            LoadableShardedMapV2Node::Inlined(inlined) => inlined.size(),
            LoadableShardedMapV2Node::Stored(stored) => stored.size,
        }
    }

    pub fn rollup_data(&self) -> Value::RollupData {
        match self {
            Self::Inlined(inlined) => inlined.rollup_data(),
            Self::Stored(stored) => stored.rollup_data.clone(),
        }
    }

    fn from_thrift(t: thrift::sharded_map::LoadableShardedMapV2Node) -> Result<Self> {
        Ok(match t {
            thrift::sharded_map::LoadableShardedMapV2Node::inlined(inlined) => {
                Self::Inlined(ShardedMapV2Node::from_thrift(inlined)?)
            }
            thrift::sharded_map::LoadableShardedMapV2Node::stored(stored) => {
                Self::Stored(ShardedMapV2StoredNode::from_thrift(stored)?)
            }
            thrift::sharded_map::LoadableShardedMapV2Node::UnknownField(_) => {
                bail!("Unknown variant")
            }
        })
    }

    fn into_thrift(self) -> thrift::sharded_map::LoadableShardedMapV2Node {
        match self {
            Self::Inlined(inlined) => {
                thrift::sharded_map::LoadableShardedMapV2Node::inlined(inlined.into_thrift())
            }
            Self::Stored(stored) => {
                thrift::sharded_map::LoadableShardedMapV2Node::stored(stored.into_thrift())
            }
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Hash, Debug)]
/// The kind of lookup to perform on when using get_entries_and_partial_maps method.
pub enum LookupKind {
    Entry,
    PartialMap,
}

impl Arbitrary for LookupKind {
    fn arbitrary(g: &mut Gen) -> Self {
        if bool::arbitrary(g) {
            LookupKind::Entry
        } else {
            LookupKind::PartialMap
        }
    }
    fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
        Box::new(std::iter::empty())
    }
}

/// An item in the stream returned by `ShardedMapV2Node::difference_stream` method.
pub struct DifferenceStreamItem<V> {
    /// Value of the key in the map.
    pub current_value: V,
    /// Values of the key in `other_maps`.
    pub previous_values: Vec<V>,
}

impl<Value: ShardedMapV2Value> ShardedMapV2Node<Value> {
    pub fn weight(&self) -> usize {
        *self.weight.get_or_init(|| {
            self.value.as_ref().map_or(0, |v| v.weight())
                + self
                    .children
                    .iter()
                    .fold(0, |acc, (_byte, child)| match child {
                        LoadableShardedMapV2Node::Inlined(inlined) => acc + inlined.weight(),
                        LoadableShardedMapV2Node::Stored(_) => acc + 1,
                    })
        })
    }

    pub fn size(&self) -> usize {
        *self.size.get_or_init(|| {
            self.value.iter().len()
                + self
                    .children
                    .values()
                    .map(|child| child.size())
                    .sum::<usize>()
        })
    }

    pub fn rollup_data(&self) -> Value::RollupData {
        Value::RollupData::rollup(
            self.value.as_ref().map(|v| v.as_ref()),
            self.children
                .iter()
                .map(|(_byte, child)| child.rollup_data())
                .collect(),
        )
    }

    /// Create a ShardedMapV2Node out of an iterator of key-value pairs.
    pub async fn from_entries(
        ctx: &CoreContext,
        blobstore: &impl KeyedBlobstore,
        entries: impl IntoIterator<Item = (impl AsRef<[u8]>, Value)>,
    ) -> Result<Self> {
        Self::from_entries_and_partial_maps(
            ctx,
            blobstore,
            entries
                .into_iter()
                .map(|(key, value)| (key, Either::Left(value)))
                .collect(),
        )
        .await
    }

    /// Create a ShardedMapV2Node from a TrieMap of values and partial maps (LoadableShardedMapV2Nodes). The key
    /// for every input partial map is a prefix that's prepended to it, which represents that keys that have this
    /// prefix are all contained in that partial map.
    /// Returns an error if the key for a partial map is a prefix of any other key in the input TrieMap.
    pub async fn from_entries_and_partial_maps(
        ctx: &CoreContext,
        blobstore: &impl KeyedBlobstore,
        entries: TrieMap<Either<Value, LoadableShardedMapV2Node<Value>>>,
    ) -> Result<Self> {
        Self::from_entries_inner(ctx, blobstore, entries)
            .await?
            .load(ctx, blobstore)
            .await
    }

    #[async_recursion]
    async fn from_entries_inner(
        ctx: &CoreContext,
        blobstore: &impl KeyedBlobstore,
        entries: TrieMap<Either<Value, LoadableShardedMapV2Node<Value>>>,
    ) -> Result<LoadableShardedMapV2Node<Value>> {
        let (lcp, entries) = entries.split_longest_common_prefix();
        let (current_entry, children) = entries.expand();

        let current_value = match current_entry {
            Some(Either::Right(partial_map)) => {
                // If there's a partial map in the input TrieMap corresponding to the longest common prefix,
                // then it should be the only entry in the TrieMap, otherwise there's a conflict.
                if !children.is_empty() {
                    bail!("Cannot create sharded map node with conflicting entries");
                }

                // If the longest common prefix is empty we can reuse the partial map directly, otherwise
                // we have to load it in order to append the longest common prefix to it.
                match lcp.is_empty() {
                    true => return Ok(partial_map),
                    false => {
                        let mut node = partial_map.load(ctx, blobstore).await?;
                        node.prefix.insert_from_slice(0, &lcp);
                        return Ok(LoadableShardedMapV2Node::Inlined(node));
                    }
                }
            }
            Some(Either::Left(value)) => Some(value),
            None => None,
        };

        // Assume that all children are not going to be inlined, then the weight of the
        // node will be the number of children plus one if the current node has a value.
        let weight = &mut (current_value.as_ref().map_or(0, |v| v.weight()) + children.len());

        let children_pre_inlining = stream::iter(children)
            .map(|(next_byte, entries)| async move {
                let child = Self::from_entries_inner(ctx, blobstore, entries).await?;
                anyhow::Ok((next_byte, child))
            })
            .buffer_unordered(100)
            .try_collect::<SortedVectorMap<_, _>>()
            .await?;

        // Go through each child in order and check if inlining will not cause the weight
        // of the current node to go beyond the weight limit.
        let children = stream::iter(children_pre_inlining)
            .map(|(next_byte, child)| {
                if *weight + child.weight() - 1 <= Value::WEIGHT_LIMIT {
                    // Below limit: inline it.
                    *weight += child.weight() - 1;
                    Either::Left(async move {
                        anyhow::Ok((next_byte, child.inlined(ctx, blobstore).await?))
                    })
                } else {
                    // Breaches limit: store as separate blob.
                    Either::Right(
                        async move { Ok((next_byte, child.stored(ctx, blobstore).await?)) },
                    )
                }
            })
            .buffer_unordered(100)
            .try_collect::<SortedVectorMap<_, _>>()
            .await?;

        Ok(LoadableShardedMapV2Node::Inlined(Self {
            prefix: lcp,
            value: current_value.map(|v| Box::new(v)),
            children,
            weight: OnceLock::from(*weight),
            size: OnceLock::new(),
        }))
    }

    /// Returns the value corresponding to the given key, or None if there's no value
    /// corresponding to it.
    #[async_recursion]
    pub async fn lookup(
        &self,
        ctx: &CoreContext,
        blobstore: &impl KeyedBlobstore,
        key: &[u8],
    ) -> Result<Option<Value>> {
        // If the key starts with the prefix of this node then strip it, otherwise
        // there's no value corresponding to this key.
        let key = match key.strip_prefix(self.prefix.as_ref()) {
            None => {
                return Ok(None);
            }
            Some(key) => key,
        };

        // If the key becomes empty after stripping the prefix, then the value of
        // this node corresponds to the key. Otherwise split the first character
        // of the key to find out which child node to recurse onto.
        let (first, rest) = match key.split_first() {
            None => {
                return Ok(self.value.clone().map(|v| *v));
            }
            Some((first, rest)) => (first, rest),
        };

        let child = match self.children.get(first) {
            None => {
                return Ok(None);
            }
            Some(child) => child,
        };

        match child {
            LoadableShardedMapV2Node::Inlined(inlined) => {
                inlined.lookup(ctx, blobstore, rest).await
            }
            LoadableShardedMapV2Node::Stored(ShardedMapV2StoredNode { id, .. }) => {
                id.load(ctx, blobstore)
                    .await?
                    .lookup(ctx, blobstore, rest)
                    .await
            }
        }
    }

    /// Returns a map containing all key-value pairs in this map for which the key
    /// starts with the given key_prefix, with key_prefix stripped from the keys.
    ///
    /// Returns None if no key in the map starts with the given key_prefix.
    #[async_recursion]
    pub async fn get_partial_map(
        &self,
        ctx: &CoreContext,
        blobstore: &impl KeyedBlobstore,
        key_prefix: &[u8],
    ) -> Result<Option<LoadableShardedMapV2Node<Value>>> {
        if let Some(remaining_prefix) = self.prefix.strip_prefix(key_prefix) {
            // If key_prefix is a prefix of this node, then the partial map corresponding
            // to the key_prefix is this node itself (possibly with the prefix adjusted).
            let mut node = self.clone();
            node.prefix = remaining_prefix.into();
            return Ok(Some(LoadableShardedMapV2Node::Inlined(node)));
        }

        // If key_prefix starts with the prefix of this node then strip it, otherwise
        // there's no value corresponding to this key_prefix.
        let key_prefix = match key_prefix.strip_prefix(self.prefix.as_ref()) {
            None => {
                return Ok(None);
            }
            Some(stripped_key_prefix) => stripped_key_prefix,
        };

        // The key_prefix should not be empty, as we would have returned an exact match above.
        debug_assert!(!key_prefix.is_empty());
        let (first, rest) = key_prefix.split_first().expect("No exact match possible");

        let child = match self.children.get(first) {
            None => {
                return Ok(None);
            }
            Some(child) => child,
        };

        match child {
            LoadableShardedMapV2Node::Inlined(inlined) => {
                inlined.get_partial_map(ctx, blobstore, rest).await
            }
            LoadableShardedMapV2Node::Stored(ShardedMapV2StoredNode { id, .. }) => {
                id.load(ctx, blobstore)
                    .await?
                    .get_partial_map(ctx, blobstore, rest)
                    .await
            }
        }
    }

    /// Returns the entries and partial maps corresponding to the given TrieMap.
    /// See documentation of `lookup` and `get_partial_map` for the semantics of looking up entries and
    /// partial maps.
    ///
    /// This is more efficient than calling `lookup` and `get_partial_map` separately for each
    /// key and prefix.
    pub async fn get_entries_and_partial_maps(
        self,
        ctx: &CoreContext,
        blobstore: &impl KeyedBlobstore,
        lookup_keys: TrieMap<LookupKind>,
        concurrency: usize,
    ) -> Result<TrieMap<Either<Value, LoadableShardedMapV2Node<Value>>>> {
        bounded_traversal::bounded_traversal_stream(
            concurrency,
            vec![(LoadableShardedMapV2Node::Inlined(self), lookup_keys, SmallBinary::new())],
            |(loadable_node, lookup_keys, mut accumulated_prefix)| {
                async move {
                    let node = loadable_node.load(ctx, blobstore).await?;

                    // Find the longest common prefix between all lookup keys and the current node,
                    // and strip it from the lookup keys and node prefix.
                    let keys_prefix = lookup_keys.longest_common_prefix();
                    let lcp = common_prefix(keys_prefix.as_ref(), node.prefix.as_ref());

                    let lookup_keys = lookup_keys.extract_prefix(lcp).expect("lcp should be a prefix of lookup_keys");
                    let node_prefix: SmallBinary = node.prefix.strip_prefix(lcp).expect("lcp should be a prefix of node.prefix").into();

                    accumulated_prefix.extend_from_slice(lcp);

                    let item = match lookup_keys.value.as_ref().map(|v| **v) {
                        // If the lookup kind is Entry, then we're looking for a value that
                        // corresponds to exactly accumulated_prefix.
                        Some(LookupKind::Entry) => {
                            if node_prefix.is_empty() {
                                node.value.clone().map(|v| Either::Left(*v))
                            } else {
                                None
                            }
                        }
                        // If the lookup kind is PartialMap, then we return the current node
                        // after adjusting its prefix.
                        Some(LookupKind::PartialMap) => {
                            let mut partial_map = node.clone();
                            partial_map.prefix = node_prefix.clone();
                            Some(Either::Right(LoadableShardedMapV2Node::Inlined(
                                partial_map,
                            )))
                        }
                        _ => None,
                    };

                    // Expand the sharded map on the first byte.
                    let mut child_nodes: SortedVectorMap<u8, LoadableShardedMapV2Node<Value>> =
                        match node_prefix.split_first() {
                            None => node.children,
                            Some((first, rest)) => {
                                let mut node = node;
                                node.prefix = rest.into();
                                sorted_vector_map! { *first => LoadableShardedMapV2Node::Inlined(node) }
                            }
                        };

                    // Expand the lookup keys on the first byte.
                    let (_, child_lookup_keys) = lookup_keys.expand();

                    // Group the sharded map children and lookup keys by the first byte.
                    let children = child_lookup_keys
                        .into_iter()
                        .flat_map({
                            |(byte, lookup_keys)| {
                                let mut accumulated_prefix = accumulated_prefix.clone();
                                accumulated_prefix.push(byte);
                                child_nodes.remove(&byte).map(|node| (node, lookup_keys, accumulated_prefix))
                            }
                        })
                        .collect::<Vec<_>>();

                    // Return the current item and recurse on the grouped node children and lookup keys.
                    anyhow::Ok((
                        item.map(|item| (accumulated_prefix, item)),
                        children,
                    ))
                }
                .boxed()
            },
        )
        .try_filter_map(futures::future::ok)
        .try_collect()
        .await
    }

    /// Returns an ordered stream over all key-value pairs in the map.
    pub fn into_entries<'a>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a impl KeyedBlobstore,
    ) -> impl Stream<Item = Result<(SmallBinary, Value)>> + 'a {
        self.into_prefix_entries_impl(ctx, blobstore, &[], None, 0)
    }

    pub fn into_entries_skip<'a>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a impl KeyedBlobstore,
        skip: usize,
    ) -> impl Stream<Item = Result<(SmallBinary, Value)>> + 'a {
        self.into_prefix_entries_impl(ctx, blobstore, &[], None, skip)
    }

    /// Returns an ordered stream over all key-value pairs in the map for which
    /// the key starts with the given prefix.
    pub fn into_prefix_entries<'a>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a impl KeyedBlobstore,
        prefix: &'a [u8],
    ) -> impl Stream<Item = Result<(SmallBinary, Value)>> + 'a {
        self.into_prefix_entries_impl(ctx, blobstore, prefix, None, 0)
    }

    /// Returns an ordered stream over all key-value pairs in the map for which
    /// the key starts with the given prefix.
    pub fn into_prefix_entries_after<'a>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a impl KeyedBlobstore,
        prefix: &'a [u8],
        after: &'a [u8],
    ) -> impl Stream<Item = Result<(SmallBinary, Value)>> + 'a {
        self.into_prefix_entries_impl(ctx, blobstore, prefix, Some(after), 0)
    }

    /// Traverse all inlined nodes that match the target filter, and add them as
    /// ordered traversal items to `out`.  Non-inlined nodes are added as recursion steps.
    fn into_prefix_entries_traverse<'a, 'b>(
        self,
        mut accumulated_prefix: SmallBinary,
        target_prefix: &'a [u8],
        target_after: Option<&'a [u8]>,
        mut target_skip: usize,
        out: &'b mut Vec<
            OrderedTraversal<
                (SmallBinary, Value),
                (
                    SmallBinary,
                    &'a [u8],
                    Option<&'a [u8]>,
                    usize,
                    LoadableShardedMapV2Node<Value>,
                ),
            >,
        >,
    ) -> Result<()> {
        let Self {
            prefix: current_prefix,
            mut value,
            mut children,
            ..
        } = self;
        if target_prefix.len() <= current_prefix.len() {
            // Exit early if the current prefix doesn't start with the target prefix,
            // as this means all keys in this node and its descendants don't start with
            // the target prefix.
            if !current_prefix.starts_with(target_prefix) {
                return Ok(());
            }

            // Trim the current prefix from the resume point and work out if we need to filter
            // the children of this node.
            if target_after.is_some_and(|after| after.starts_with(current_prefix.as_slice())) {
                // The value at this point is before the resume point, so it should
                // not be output.
                value = None;
            }
            if target_skip > 0 && value.is_some() {
                target_skip -= 1;
                value = None;
            }

            let (byte_after, child_after) = match target_after
                .and_then(|after| after.strip_prefix(current_prefix.as_slice()))
                .and_then(|after| after.split_first())
            {
                None if Some(current_prefix.as_slice()) < target_after => {
                    // The target prefix is after the current prefix, so we should
                    // not recurse into any children.
                    return Ok(());
                }
                None => (None, None),
                Some((byte, rest)) => (Some(*byte), Some(rest)),
            };

            // If target_prefix is a prefix of the current node, then
            // we should output all the values included in this node and
            // its descendants, after any resume point.

            accumulated_prefix.extend_from_slice(&current_prefix);

            if let Some(value) = value {
                out.push(OrderedTraversal::Output((
                    accumulated_prefix.clone(),
                    *value,
                )))
            }

            for (byte, child) in children {
                let mut accumulated_prefix = accumulated_prefix.clone();
                accumulated_prefix.push(byte);
                if Some(byte) >= byte_after {
                    let child_after = if Some(byte) == byte_after {
                        child_after
                    } else {
                        None
                    };
                    let child_skip = target_skip.min(child.size());
                    target_skip = target_skip.saturating_sub(child_skip);
                    if child_skip != child.size() {
                        if let LoadableShardedMapV2Node::Inlined(child) = child {
                            child.into_prefix_entries_traverse(
                                accumulated_prefix,
                                b"".as_slice(),
                                child_after,
                                child_skip,
                                out,
                            )?;
                        } else {
                            out.push(OrderedTraversal::Recurse(
                                child.size(),
                                (
                                    accumulated_prefix,
                                    b"".as_slice(),
                                    child_after,
                                    child_skip,
                                    child,
                                ),
                            ));
                        }
                    }
                }
            }
        } else {
            // target_prefix is longer than the prefix of the current node. This
            // means that there's at most one child we should recurse to while
            // ignoring the value of the current node and all other children.

            let target_prefix = match target_prefix.strip_prefix(current_prefix.as_slice()) {
                Some(remaining_prefix) => remaining_prefix,
                // The target prefix doesn't start with current node's prefix. Exit early
                // as none of the keys in the map can start with the target prefix.
                None => return Ok(()),
            };
            let target_after = match target_after
                .and_then(|after| after.strip_prefix(current_prefix.as_slice()))
            {
                Some(remaining_after) => Some(remaining_after),
                None if Some(current_prefix.as_slice()) < target_after => {
                    return Ok(());
                }
                None => None,
            };

            let (byte_prefix, child_prefix) = target_prefix.split_first().unwrap();
            let child_after = match target_after.and_then(|after| after.split_first()) {
                Some((byte_after, _)) if byte_prefix < byte_after => return Ok(()),
                Some((byte_after, child_after)) if byte_prefix == byte_after => Some(child_after),
                None | Some(_) => None,
            };

            let child = match children.remove(byte_prefix) {
                Some(child) => child,
                // Exit early if we can't find the child corresponding to the first byte of
                // the remainder of target prefix, as that's the only child whose keys can
                // start with the target prefix.
                None => return Ok(()),
            };

            accumulated_prefix.extend_from_slice(&current_prefix);
            accumulated_prefix.push(*byte_prefix);

            if let LoadableShardedMapV2Node::Inlined(child) = child {
                child.into_prefix_entries_traverse(
                    accumulated_prefix,
                    child_prefix,
                    child_after,
                    target_skip,
                    out,
                )?;
            } else {
                out.push(OrderedTraversal::Recurse(
                    child.size(),
                    (
                        accumulated_prefix,
                        child_prefix,
                        child_after,
                        target_skip,
                        child,
                    ),
                ))
            }
        }
        Ok(())
    }

    /// Internal implementation of all `into_*_entries_*` methods.
    ///
    /// Returns an ordered stream over all key-value pairs in the map for which
    /// the key starts with the given prefix, and is either after the given resume
    /// point, or after skipping a number of entries.
    ///
    /// Note that resume points and skipping are incompatible, as it's impossible
    /// to know when recursing how many of a child's entries will end up being
    /// skipped.
    fn into_prefix_entries_impl<'a>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a impl KeyedBlobstore,
        prefix: &'a [u8],
        after: Option<&'a [u8]>,
        skip: usize,
    ) -> impl Stream<Item = Result<(SmallBinary, Value)>> + 'a {
        const BOUNDED_TRAVERSAL_SCHEDULED_MAX: NonZeroUsize = nonzero!(256usize);
        let queued_max =
            NonZeroUsize::new(Value::WEIGHT_LIMIT * 2).unwrap_or(BOUNDED_TRAVERSAL_SCHEDULED_MAX);
        debug_assert!(
            after.is_none() || skip == 0,
            "programming error: only one of after or skip should be set"
        );

        bounded_traversal::bounded_traversal_ordered_stream(
            BOUNDED_TRAVERSAL_SCHEDULED_MAX,
            queued_max,
            vec![(
                self.size(),
                (
                    SmallBinary::new(),
                    prefix,
                    after,
                    skip,
                    LoadableShardedMapV2Node::Inlined(self),
                ),
            )],
            move |(
                accumulated_prefix,
                target_prefix,
                target_after,
                target_skip,
                current_node,
            ): (
                SmallBinary,
                &[u8],
                Option<&[u8]>,
                usize,
                LoadableShardedMapV2Node<Value>,
            )| {
                async move {
                    let mut out = Vec::new();
                    current_node.load(ctx, blobstore).await?.into_prefix_entries_traverse(
                        accumulated_prefix,
                        target_prefix,
                        target_after,
                        target_skip,
                        &mut out,
                    )?;
                    Ok(out)
                }
                .boxed()
            },
        )
    }

    /// Returns an unordered stream over all key-value pairs in the map.
    pub fn into_entries_unordered<'a>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a impl KeyedBlobstore,
        concurrency: usize,
    ) -> impl Stream<Item = Result<(SmallBinary, Value)>> + 'a {
        bounded_traversal::bounded_traversal_stream(
            concurrency,
            vec![(SmallBinary::new(), LoadableShardedMapV2Node::Inlined(self))],
            move |(mut accumulated_prefix, current_node): (
                SmallBinary,
                LoadableShardedMapV2Node<Value>,
            )| {
                async move {
                    let Self {
                        prefix,
                        value,
                        children,
                        ..
                    } = current_node.load(ctx, blobstore).await?;

                    accumulated_prefix.extend_from_slice(&prefix);

                    Ok((
                        value.map(|value| (accumulated_prefix.clone(), *value)),
                        children.into_iter().map(move |(byte, child)| {
                            let mut accumulated_prefix = accumulated_prefix.clone();
                            accumulated_prefix.push(byte);
                            (accumulated_prefix, child)
                        }),
                    ))
                }
                .boxed()
            },
        )
        .try_filter_map(futures::future::ok)
    }

    /// Returns an unordered stream over all key-value pairs in this map for which
    /// the value is different from all corresponding values in all `other_maps`.
    ///
    /// For each key we also return the corresponding values in `other_maps`.
    pub fn difference_stream<'a>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a impl KeyedBlobstore,
        other_maps: Vec<Self>,
        concurrency: usize,
    ) -> impl Stream<Item = Result<(SmallBinary, DifferenceStreamItem<Value>)>> + 'a
    where
        Value: PartialEq,
    {
        bounded_traversal::bounded_traversal_stream(
            concurrency,
            vec![(
                SmallBinary::new(),
                LoadableShardedMapV2Node::Inlined(self),
                other_maps
                    .into_iter()
                    .map(LoadableShardedMapV2Node::Inlined)
                    .collect::<Vec<_>>(),
            )],
            move |(mut accumulated_prefix, current_map, other_maps): (
                SmallBinary,
                LoadableShardedMapV2Node<Value>,
                Vec<LoadableShardedMapV2Node<Value>>,
            )| {
                async move {
                    // If any of the other maps is the same as the current map, then there are no differences.
                    if other_maps.iter().any(|other_map| other_map == &current_map) {
                        return Ok((None, Either::Left(std::iter::empty())));
                    }

                    let (mut current_map, mut other_maps) = try_join!(
                        current_map.load(ctx, blobstore),
                        other_maps
                            .into_iter()
                            .map(|node| async move { node.load(ctx, blobstore).await })
                            .collect::<FuturesUnordered<_>>()
                            .try_collect::<Vec<_>>()
                    )?;

                    // Find the longest common prefix of all maps.
                    let lcp: SmallBinary = other_maps
                        .iter()
                        .fold(current_map.prefix.as_ref(), |lcp, map| {
                            common_prefix(lcp, map.prefix.as_ref())
                        })
                        .into();

                    // Remove the longest common prefix from all maps.
                    current_map.prefix = current_map
                        .prefix
                        .strip_prefix(lcp.as_ref())
                        .expect("lcp is a prefix of current_map")
                        .into();
                    for other_map in &mut other_maps {
                        other_map.prefix = other_map
                            .prefix
                            .strip_prefix(lcp.as_ref())
                            .expect("lcp is a prefix of all maps in other_map")
                            .into();
                    }

                    // Add the longest common prefix to the accumulated prefix.
                    accumulated_prefix.extend_from_slice(lcp.as_ref());

                    // Check if there's a value at the current prefix in the map,
                    // and check that it's different from the values in all `other_maps`.
                    let item = match (current_map.value.take(), current_map.prefix.is_empty()) {
                        (Some(value), true) => {
                            let previous_values = other_maps
                                .iter_mut()
                                .flat_map(|other_map| {
                                    match (other_map.value.take(), other_map.prefix.is_empty()) {
                                        (Some(value), true) => Some(*value),
                                        _ => None,
                                    }
                                })
                                .collect::<Vec<_>>();

                            if !previous_values.iter().any(|other_value| other_value == value.as_ref()) {
                                Some(DifferenceStreamItem { current_value: *value, previous_values })
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };

                    // Find the children of the current map corresponding to all possible
                    // first bytes.
                    let child_maps = match current_map.prefix.clone().split_first() {
                        None => current_map.children,
                        Some((first, rest)) => {
                            current_map.prefix = rest.into();
                            sorted_vector_map! { *first => LoadableShardedMapV2Node::Inlined(current_map) }
                        }
                    };

                    // Find the children of all `other_maps` corresponding to all possible
                    // first bytes.
                    let mut child_other_maps = other_maps
                        .into_iter()
                        .flat_map(|mut other_map| {
                            match other_map.prefix.clone().split_first() {
                                None => Either::Left(other_map.children.into_iter()),
                                Some((first, rest)) => {
                                    other_map.prefix = rest.into();
                                    Either::Right(std::iter::once((*first, LoadableShardedMapV2Node::Inlined(other_map))))
                                }
                            }
                        })
                        .into_group_map();

                    Ok((item.map(|item| (accumulated_prefix.clone(), item)), Either::Right(child_maps.into_iter().map(move |(byte, child)| {
                        // Add the current byte to the accumulated prefix.
                        let mut accumulated_prefix = accumulated_prefix.clone();
                        accumulated_prefix.push(byte);

                        // Find children of `other_maps` that correspond to the current byte.
                        let other_maps = child_other_maps.remove(&byte).unwrap_or_default();

                        (accumulated_prefix, child, other_maps)
                    }))))
                }
                .boxed()
            },
        )
        .try_filter_map(futures::future::ok)
    }
}

impl<Value: ShardedMapV2Value> ThriftConvert for ShardedMapV2Node<Value> {
    const NAME: &'static str = "ShardedMapV2Node";
    type Thrift = thrift::sharded_map::ShardedMapV2Node;

    fn from_thrift(t: thrift::sharded_map::ShardedMapV2Node) -> Result<Self> {
        Ok(Self {
            prefix: t.prefix.0,
            value: t
                .value
                .as_ref()
                .map(|v| anyhow::Ok(Box::new(ThriftConvert::from_bytes(v)?)))
                .transpose()?,
            children: t
                .children
                .into_iter()
                .map(|(key, child)| Ok((key as u8, LoadableShardedMapV2Node::from_thrift(child)?)))
                .collect::<Result<_>>()?,
            weight: OnceLock::new(),
            size: OnceLock::new(),
        })
    }

    fn into_thrift(self) -> thrift::sharded_map::ShardedMapV2Node {
        thrift::sharded_map::ShardedMapV2Node {
            prefix: thrift::data::SmallBinary(self.prefix),
            value: self.value.map(|v| ThriftConvert::into_bytes(*v)),
            children: self
                .children
                .into_iter()
                .map(|(key, child)| (key as i8, child.into_thrift()))
                .collect(),
        }
    }
}

impl<Value: ShardedMapV2Value> BlobstoreValue for ShardedMapV2Node<Value> {
    type Key = Value::NodeId;

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
    use std::collections::BTreeMap;
    use std::str::FromStr;

    use anyhow::anyhow;
    use anyhow::bail;
    use async_recursion::async_recursion;
    use async_trait::async_trait;
    use blobstore::BlobstoreKeyParam;
    use blobstore::BlobstoreKeyRange;
    use blobstore::BlobstoreKeySource;
    use blobstore::KeyedBlobstore;
    use blobstore::LoadableError;
    use blobstore::Storable;
    use context::CoreContext;
    use fbinit::FacebookInit;
    use memblob::KeyedMemblob;
    use mononoke_macros::mononoke;

    use super::*;
    use crate::BlobstoreKey;
    use crate::impl_typed_hash;
    use crate::private::Blake2;

    #[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
    pub struct TestValue(u32);

    #[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
    pub struct MaxTestValue(u32);

    #[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug, Hash)]
    pub struct ShardedMapV2NodeTestId(Blake2);

    impl_typed_hash! {
        hash_type => ShardedMapV2NodeTestId,
        thrift_hash_type => thrift::id::ShardedMapV2NodeId,
        value_type => ShardedMapV2Node<TestValue>,
        context_type => ShardedMapV2NodeTestContext,
        context_key => "test.map2node",
    }

    #[mononoke::test]
    fn sharded_map_v2_blobstore_key() {
        let id = ShardedMapV2NodeTestId::from_byte_array([1; 32]);
        assert_eq!(id.blobstore_key(), format!("test.map2node.blake2.{id}"));
    }

    impl ShardedMapV2Value for TestValue {
        type NodeId = ShardedMapV2NodeTestId;
        type Context = ShardedMapV2NodeTestContext;
        type RollupData = MaxTestValue;

        const WEIGHT_LIMIT: usize = 5;
    }

    impl Rollup<TestValue> for MaxTestValue {
        fn rollup(value: Option<&TestValue>, child_rollup_data: Vec<Self>) -> Self {
            let mut max_test_value = value.map_or(MaxTestValue(0), |value| MaxTestValue(value.0));

            for child_max_test_value in child_rollup_data {
                if child_max_test_value.0 > max_test_value.0 {
                    max_test_value = child_max_test_value;
                }
            }

            max_test_value
        }
    }

    impl ThriftConvert for TestValue {
        const NAME: &'static str = "TestValue";
        type Thrift = i32;
        fn into_thrift(self) -> Self::Thrift {
            self.0 as i32
        }
        fn from_thrift(t: Self::Thrift) -> Result<Self> {
            Ok(TestValue(t as u32))
        }
    }

    impl ThriftConvert for MaxTestValue {
        const NAME: &'static str = "MaxTestValue";
        type Thrift = i32;
        fn into_thrift(self) -> Self::Thrift {
            self.0 as i32
        }
        fn from_thrift(t: Self::Thrift) -> Result<Self> {
            Ok(MaxTestValue(t as u32))
        }
    }

    const EXAMPLE_ENTRIES: &[(&str, u32)] = &[
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

    fn to_test_vec(entries: &[(&str, u32)]) -> Vec<(String, u32)> {
        entries
            .iter()
            .map(|(key, value)| (String::from(*key), *value))
            .collect()
    }

    fn check_round_trip(map: ShardedMapV2Node<TestValue>) -> Result<()> {
        let map_t = map.clone().into_thrift();

        if ShardedMapV2Node::from_thrift(map_t).unwrap() == map {
            Ok(())
        } else {
            Err(anyhow!(
                "converting sharded map node to thrift and back doesn't produce the same map."
            ))
        }
    }

    struct CalculatedValues {
        weight: usize,
        size: usize,
        rollup_data: MaxTestValue,
    }

    #[derive(Clone)]
    struct MapHelper(CoreContext, KeyedMemblob);
    impl MapHelper {
        async fn from_entries_removed_prefix(
            &self,
            entries: &[(&str, u32)],
            prefix_len: usize,
        ) -> Result<ShardedMapV2Node<TestValue>> {
            ShardedMapV2Node::from_entries(
                &self.0,
                &self.1,
                entries
                    .iter()
                    .map(|(key, value)| (&key[prefix_len..], TestValue(*value)))
                    .collect::<TrieMap<_>>(),
            )
            .await
        }

        async fn from_entries(
            &self,
            entries: &[(&str, u32)],
        ) -> Result<ShardedMapV2Node<TestValue>> {
            self.from_entries_removed_prefix(entries, 0).await
        }

        async fn from_entries_and_partial_maps(
            &self,
            entries: &[(&str, Either<u32, ShardedMapV2Node<TestValue>>)],
        ) -> Result<ShardedMapV2Node<TestValue>> {
            ShardedMapV2Node::from_entries_and_partial_maps(
                &self.0,
                &self.1,
                entries
                    .iter()
                    .map(|(key, entry)| {
                        let entry = match entry {
                            Either::Left(value) => Either::Left(TestValue(*value)),
                            Either::Right(map) => {
                                Either::Right(LoadableShardedMapV2Node::Inlined(map.clone()))
                            }
                        };
                        (key.as_bytes(), entry)
                    })
                    .collect(),
            )
            .await
        }

        async fn lookup(
            &self,
            map: &ShardedMapV2Node<TestValue>,
            key: impl AsRef<[u8]>,
        ) -> Result<Option<TestValue>> {
            map.lookup(&self.0, &self.1, key.as_ref()).await
        }

        async fn get_partial_map(
            &self,
            map: &ShardedMapV2Node<TestValue>,
            key: impl AsRef<[u8]>,
        ) -> Result<Option<ShardedMapV2Node<TestValue>>> {
            let partial_map = map.get_partial_map(&self.0, &self.1, key.as_ref()).await?;

            match partial_map {
                Some(partial_map) => Ok(Some(partial_map.load(&self.0, &self.1).await?)),
                None => Ok(None),
            }
        }

        async fn load(
            &self,
            map: LoadableShardedMapV2Node<TestValue>,
        ) -> Result<ShardedMapV2Node<TestValue>> {
            map.load(&self.0, &self.1)
                .watched()
                .with_max_poll(blobstore::BLOBSTORE_MAX_POLL_TIME_MS)
                .await
        }

        async fn get_entries_and_partial_maps(
            &self,
            map: &ShardedMapV2Node<TestValue>,
            lookup_keys: TrieMap<LookupKind>,
        ) -> Result<TrieMap<Either<TestValue, ShardedMapV2Node<TestValue>>>> {
            let trie_map = map
                .clone()
                .get_entries_and_partial_maps(&self.0, &self.1, lookup_keys, 10)
                .await?;

            let mut loaded_trie_map = TrieMap::default();
            for (key, entry) in trie_map {
                let entry = match entry {
                    Either::Left(value) => Either::Left(value),
                    Either::Right(partial_map) => Either::Right(self.load(partial_map).await?),
                };
                loaded_trie_map.insert(key, entry);
            }

            Ok(loaded_trie_map)
        }

        async fn into_entries(
            &self,
            map: ShardedMapV2Node<TestValue>,
        ) -> Result<Vec<(String, u32)>> {
            map.into_entries(&self.0, &self.1)
                .and_then(
                    |(key, value)| async move { Ok((String::from_utf8(key.to_vec())?, value.0)) },
                )
                .try_collect::<Vec<_>>()
                .await
        }

        async fn into_entries_unordered(
            &self,
            map: ShardedMapV2Node<TestValue>,
        ) -> Result<Vec<(String, u32)>> {
            map.into_entries_unordered(&self.0, &self.1, 100)
                .and_then(
                    |(key, value)| async move { Ok((String::from_utf8(key.to_vec())?, value.0)) },
                )
                .try_collect::<Vec<_>>()
                .await
        }

        async fn difference_stream(
            &self,
            map: ShardedMapV2Node<TestValue>,
            other_maps: Vec<ShardedMapV2Node<TestValue>>,
        ) -> Result<Vec<(String, u32, Vec<u32>)>> {
            map.difference_stream(&self.0, &self.1, other_maps, 100)
                .and_then(|(key, item)| async move {
                    Ok((
                        String::from_utf8(key.to_vec())?,
                        item.current_value.0,
                        item.previous_values
                            .into_iter()
                            .map(|value| value.0)
                            .collect(),
                    ))
                })
                .try_collect::<Vec<_>>()
                .await
        }

        async fn into_entries_skip(
            &self,
            map: ShardedMapV2Node<TestValue>,
            skip: usize,
        ) -> Result<Vec<(String, u32)>> {
            map.into_entries_skip(&self.0, &self.1, skip)
                .and_then(
                    |(key, value)| async move { Ok((String::from_utf8(key.to_vec())?, value.0)) },
                )
                .try_collect::<Vec<_>>()
                .await
        }

        async fn into_prefix_entries(
            &self,
            map: ShardedMapV2Node<TestValue>,
            prefix: impl AsRef<[u8]>,
        ) -> Result<Vec<(String, u32)>> {
            map.into_prefix_entries(&self.0, &self.1, prefix.as_ref())
                .and_then(
                    |(key, value)| async move { Ok((String::from_utf8(key.to_vec())?, value.0)) },
                )
                .try_collect::<Vec<_>>()
                .await
        }

        async fn into_prefix_entries_after(
            &self,
            map: ShardedMapV2Node<TestValue>,
            prefix: impl AsRef<[u8]>,
            after: impl AsRef<[u8]>,
        ) -> Result<Vec<(String, u32)>> {
            map.into_prefix_entries_after(&self.0, &self.1, prefix.as_ref(), after.as_ref())
                .and_then(
                    |(key, value)| async move { Ok((String::from_utf8(key.to_vec())?, value.0)) },
                )
                .try_collect::<Vec<_>>()
                .await
        }
        async fn check_example_map(&self, map: ShardedMapV2Node<TestValue>) -> Result<()> {
            self.check_sharded_map(map.clone()).await?;

            for (key, value) in EXAMPLE_ENTRIES {
                assert_eq!(self.lookup(&map, key).await?, Some(TestValue(*value)));
            }
            assert_eq!(self.lookup(&map, "NOT_IN_MAP").await?, None);

            assert_eq!(
                self.into_entries(map.clone()).await?,
                to_test_vec(EXAMPLE_ENTRIES)
            );

            assert_eq!(
                self.into_prefix_entries(map.clone(), "a").await?,
                to_test_vec(&EXAMPLE_ENTRIES[0..8])
            );
            assert_eq!(
                self.into_prefix_entries(map.clone(), "ab").await?,
                to_test_vec(&EXAMPLE_ENTRIES[0..8])
            );
            assert_eq!(
                self.into_prefix_entries(map.clone(), "aba").await?,
                to_test_vec(&EXAMPLE_ENTRIES[0..8])
            );
            assert_eq!(
                self.into_prefix_entries(map.clone(), "o").await?,
                to_test_vec(&EXAMPLE_ENTRIES[8..12])
            );
            assert_eq!(
                self.into_prefix_entries(map.clone(), "om").await?,
                to_test_vec(&EXAMPLE_ENTRIES[8..12])
            );
            assert_eq!(
                self.into_prefix_entries(map.clone(), "omi").await?,
                to_test_vec(&EXAMPLE_ENTRIES[8..10])
            );
            assert_eq!(
                self.into_prefix_entries(map.clone(), "omu").await?,
                to_test_vec(&EXAMPLE_ENTRIES[10..12])
            );

            assert_eq!(
                self.into_prefix_entries_after(map.clone(), "a", "abacab")
                    .await?,
                to_test_vec(&EXAMPLE_ENTRIES[2..8])
            );
            assert_eq!(
                self.into_prefix_entries_after(map.clone(), "abac", "aba")
                    .await?,
                to_test_vec(&EXAMPLE_ENTRIES[1..6])
            );
            assert_eq!(
                self.into_prefix_entries_after(map.clone(), "aba", "abalada")
                    .await?,
                to_test_vec(&EXAMPLE_ENTRIES[8..8])
            );
            assert_eq!(
                self.into_prefix_entries_after(map.clone(), "om", "ab")
                    .await?,
                to_test_vec(&EXAMPLE_ENTRIES[8..12])
            );
            assert_eq!(
                self.into_prefix_entries_after(map.clone(), "omi", "")
                    .await?,
                to_test_vec(&EXAMPLE_ENTRIES[8..10])
            );
            assert_eq!(
                self.into_prefix_entries_after(map.clone(), "", "abalaba")
                    .await?,
                to_test_vec(&EXAMPLE_ENTRIES[7..12])
            );

            for skip in 0..12 {
                assert_eq!(
                    self.into_entries_skip(map.clone(), skip).await?,
                    to_test_vec(&EXAMPLE_ENTRIES[skip..12]),
                    "skip value {skip} failed",
                );
            }

            assert_eq!(map.rollup_data(), MaxTestValue(12),);

            Ok(())
        }

        #[async_recursion]
        async fn check_sharded_map(
            &self,
            map: ShardedMapV2Node<TestValue>,
        ) -> Result<CalculatedValues> {
            check_round_trip(map.clone())?;

            // The minimum weight that this node could have is the number of its children
            // plus one if it has a value.
            let min_possible_weight =
                map.value.as_ref().map_or(0, |v| v.weight()) + map.children.len();

            // Bypass the weight limit check if map's weight is the minimum possible (i.e. all children are stored),
            // this is to avoid failing in quickcheck tests in which sometimes a node will have more than weight
            // limit number of children.
            if map.weight() > TestValue::WEIGHT_LIMIT && map.weight() != min_possible_weight {
                bail!("weight of sharded map node exceeds the limit");
            }

            let mut calculated_weight = map.value.as_ref().map_or(0, |v| v.weight());
            let mut calculated_size = map.value.iter().len();
            let mut calculated_rollup_data = map
                .value
                .clone()
                .map_or(MaxTestValue(0), |value| MaxTestValue(value.0));

            for (_next_byte, child) in map.children.iter() {
                let child_calculated_values = self
                    .check_sharded_map(child.clone().load(&self.0, &self.1).await?)
                    .await?;

                match child {
                    LoadableShardedMapV2Node::Inlined(_) => {
                        calculated_weight += child_calculated_values.weight;
                    }
                    LoadableShardedMapV2Node::Stored(_) => {
                        calculated_weight += 1;
                    }
                }
                calculated_size += child_calculated_values.size;
                if child_calculated_values.rollup_data.0 > calculated_rollup_data.0 {
                    calculated_rollup_data = child_calculated_values.rollup_data;
                }
            }

            if calculated_weight != map.weight() {
                bail!("weight of sharded map node does not match its calculated weight");
            }
            if calculated_size != map.size() {
                bail!("size of sharded map node does not match its calculated size");
            }
            if calculated_rollup_data != map.rollup_data() {
                bail!("sharded map node rollup data does not match its calculated rollup data");
            }

            Ok(CalculatedValues {
                weight: calculated_weight,
                size: calculated_size,
                rollup_data: calculated_rollup_data,
            })
        }

        async fn assert_all_keys(&self, keys: &[&str]) -> Result<()> {
            let data = self
                .1
                .as_inner()
                .enumerate(
                    &self.0,
                    &BlobstoreKeyParam::Start(BlobstoreKeyRange {
                        begin_key: String::new(),
                        end_key: String::new(),
                    }),
                )
                .await?;

            let mut data: Vec<_> = data.keys.into_iter().collect();
            data.sort();

            assert_eq!(
                data,
                keys.iter()
                    .map(|key| String::from(*key))
                    .collect::<Vec<_>>()
            );
            Ok(())
        }

        async fn stored_node(
            &self,
            node: ShardedMapV2Node<TestValue>,
            weight: usize,
            size: usize,
            rollup_data: u32,
            blobstore_key: &str,
        ) -> Result<LoadableShardedMapV2Node<TestValue>> {
            let id = node.into_blob().store(&self.0, &self.1).await?;
            assert_eq!(id.blobstore_key().as_str(), blobstore_key);

            Ok(LoadableShardedMapV2Node::Stored(ShardedMapV2StoredNode {
                id,
                weight,
                size,
                rollup_data: MaxTestValue(rollup_data),
            }))
        }
    }

    fn test_node(
        prefix: &str,
        value: Option<u32>,
        children: Vec<(u8, LoadableShardedMapV2Node<TestValue>)>,
    ) -> ShardedMapV2Node<TestValue> {
        ShardedMapV2Node {
            prefix: SmallBinary::from(prefix.as_bytes()),
            value: value.map(|v| Box::new(TestValue(v))),
            children: children.into_iter().collect(),
            weight: Default::default(),
            size: Default::default(),
        }
    }

    fn inlined_node(
        prefix: &str,
        value: Option<u32>,
        children: Vec<(u8, LoadableShardedMapV2Node<TestValue>)>,
    ) -> LoadableShardedMapV2Node<TestValue> {
        LoadableShardedMapV2Node::Inlined(test_node(prefix, value, children))
    }

    #[mononoke::fbinit_test]
    async fn test_sharded_map_v2_example(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();
        let helper = MapHelper(ctx, blobstore);

        let from_entries_map = helper.from_entries(EXAMPLE_ENTRIES).await?;

        helper.assert_all_keys(&[
            "test.map2node.blake2.6f7dc1a2ad07d16eb4d3e586e2f7361c0990dcf4a29b0bb06fa5d04e69710a64",
            "test.map2node.blake2.d40e11f4f3f08ad21b5eb6bab17e0916d449bffde464048dfb27efa3f9c19cee"
        ]).await?;

        helper.check_example_map(from_entries_map.clone()).await?;

        assert!(
            helper
                .get_partial_map(&from_entries_map, "test")
                .await?
                .is_none()
        );

        // map_abacab:
        //     *=7
        //     |
        //     a=8
        let map_abacab = inlined_node("", Some(7), vec![(b'a', inlined_node("", Some(8), vec![]))]);
        assert_eq!(
            helper.load(map_abacab.clone()).await?,
            helper
                .get_partial_map(&from_entries_map, "abacab")
                .await?
                .unwrap()
        );
        // map_abac:
        //     *
        //     |
        //     a
        //     |____________________
        //     |     \       \      \
        //     b=7   kkk=9  te=10  xi=11
        //     |
        //     a=8
        let map_abac = helper
            .stored_node(
                test_node(
                    "a",
                    None,
                    vec![
                        (b'b', map_abacab),
                        (b'k', inlined_node("kk", Some(9), vec![])),
                        (b't', inlined_node("e", Some(10), vec![])),
                        (b'x', inlined_node("i", Some(11), vec![])),
                    ],
                ),
                5,
                5,
                11,
                "test.map2node.blake2.d40e11f4f3f08ad21b5eb6bab17e0916d449bffde464048dfb27efa3f9c19cee",
            )
            .await?;
        assert_eq!(
            helper.load(map_abac.clone()).await?,
            helper
                .get_partial_map(&from_entries_map, "abac")
                .await?
                .unwrap()
        );
        // map_abal:
        //      *
        //      |
        //      a
        //      |____
        //      |    \
        //     ba=5  da=6
        let map_abal = inlined_node(
            "a",
            None,
            vec![
                (b'b', inlined_node("a", Some(5), vec![])),
                (b'd', inlined_node("a", Some(6), vec![])),
            ],
        );
        assert_eq!(
            helper.load(map_abal.clone()).await?,
            helper
                .get_partial_map(&from_entries_map, "abal")
                .await?
                .unwrap()
        );
        // map_a:
        //     *
        //     |
        //     ba=12
        //     |_______________________________
        //     |                               \
        //     ca                               la
        //     |____________________            |____
        //     |     \       \      \           |    \
        //     b=7   kkk=9  te=10  xi=11      ba=5  da=6
        //     |
        //     a=8
        let map_a = inlined_node(
            "ba",
            Some(12),
            vec![(b'c', map_abac.clone()), (b'l', map_abal)],
        );
        assert_eq!(
            helper.load(map_a.clone()).await?,
            helper
                .get_partial_map(&from_entries_map, "a")
                .await?
                .unwrap()
        );
        // map_omi:
        //     *
        //     |______
        //     |      \
        //     ojo=1  ux=2
        let map_omi = inlined_node(
            "",
            None,
            vec![
                (b'o', inlined_node("jo", Some(1), vec![])),
                (b'u', inlined_node("x", Some(2), vec![])),
            ],
        );
        assert_eq!(
            helper.load(map_omi.clone()).await?,
            helper
                .get_partial_map(&from_entries_map, "omi")
                .await?
                .unwrap()
        );
        // map_omu:
        //     *
        //     |
        //     n
        //     |______
        //     |      \
        //     do=3   gal=4
        let map_omu = inlined_node(
            "n",
            None,
            vec![
                (b'd', inlined_node("o", Some(3), vec![])),
                (b'g', inlined_node("al", Some(4), vec![])),
            ],
        );
        assert_eq!(
            helper.load(map_omu.clone()).await?,
            helper
                .get_partial_map(&from_entries_map, "omu")
                .await?
                .unwrap()
        );
        // map_o:
        //     *
        //     |
        //     m
        //     |______________
        //     |              \
        //     i              un
        //     |______        |______
        //     |      \       |      \
        //     ojo=1  ux=2    do=3   gal=4
        let map_o = helper
            .stored_node(
                test_node("m", None, vec![(b'i', map_omi), (b'u', map_omu)]),
                4,
                4,
                4,
                "test.map2node.blake2.6f7dc1a2ad07d16eb4d3e586e2f7361c0990dcf4a29b0bb06fa5d04e69710a64"
            )
            .await?;
        assert_eq!(
            helper.load(map_o.clone()).await?,
            helper
                .get_partial_map(&from_entries_map, "o")
                .await?
                .unwrap()
        );
        // map:
        //     *
        //     |_______________________________________________
        //     |                                               \
        //     aba=12                                           om
        //     |_______________________________                 |______________
        //     |                               \                |              \
        //     ca                               la              i              un
        //     |____________________            |____           |______        |______
        //     |     \       \      \           |    \          |      \       |      \
        //     b=7   kkk=9  te=10  xi=11      ba=5  da=6       ojo=1   ux=2    do=3   gal=4
        //     |
        //     a=8
        let map = test_node("", None, vec![(b'a', map_a), (b'o', map_o)]);

        assert_eq!(from_entries_map.clone(), map.clone());
        assert_eq!(
            map,
            helper
                .get_partial_map(&from_entries_map, "")
                .await?
                .unwrap()
        );

        assert_eq!(
            helper
                .from_entries_removed_prefix(&EXAMPLE_ENTRIES[0..8], 2)
                .await?,
            helper
                .get_partial_map(&from_entries_map, "ab")
                .await?
                .unwrap()
        );
        assert_eq!(
            helper
                .from_entries_removed_prefix(&EXAMPLE_ENTRIES[0..8], 3)
                .await?,
            helper
                .get_partial_map(&from_entries_map, "aba")
                .await?
                .unwrap()
        );
        assert_eq!(
            helper
                .from_entries_removed_prefix(&EXAMPLE_ENTRIES[8..12], 2)
                .await?,
            helper
                .get_partial_map(&from_entries_map, "om")
                .await?
                .unwrap()
        );
        assert_eq!(
            helper
                .from_entries_removed_prefix(&EXAMPLE_ENTRIES[10..12], 4)
                .await?,
            helper
                .get_partial_map(&from_entries_map, "omun")
                .await?
                .unwrap()
        );

        assert_eq!(
            [
                (
                    "omun",
                    Either::Right(
                        helper
                            .from_entries_removed_prefix(&EXAMPLE_ENTRIES[10..12], 4)
                            .await?
                    )
                ),
                ("abacaxi", Either::Left(TestValue(11))),
                ("abalaba", Either::Left(TestValue(5))),
                ("omiojo", Either::Left(TestValue(1))),
                ("omungal", Either::Left(TestValue(4))),
                (
                    "om",
                    Either::Right(
                        helper
                            .from_entries_removed_prefix(&EXAMPLE_ENTRIES[8..12], 2)
                            .await?
                    )
                ),
                (
                    "aba",
                    Either::Right(
                        helper
                            .from_entries_removed_prefix(&EXAMPLE_ENTRIES[0..8], 3)
                            .await?
                    )
                ),
                (
                    "o",
                    Either::Right(
                        helper
                            .from_entries_removed_prefix(&EXAMPLE_ENTRIES[8..12], 1)
                            .await?
                    )
                ),
                (
                    "ab",
                    Either::Right(
                        helper
                            .from_entries_removed_prefix(&EXAMPLE_ENTRIES[0..8], 2)
                            .await?
                    )
                ),
                (
                    "",
                    Either::Right(helper.from_entries(EXAMPLE_ENTRIES).await?)
                ),
                (
                    "abacab",
                    Either::Right(
                        helper
                            .from_entries_removed_prefix(&EXAMPLE_ENTRIES[1..3], 6)
                            .await?
                    )
                ),
            ]
            .into_iter()
            .collect::<TrieMap<_>>(),
            helper
                .get_entries_and_partial_maps(
                    &from_entries_map,
                    [
                        ("omun", LookupKind::PartialMap),
                        ("om", LookupKind::PartialMap),
                        ("aba", LookupKind::PartialMap),
                        ("o", LookupKind::PartialMap),
                        ("ab", LookupKind::PartialMap),
                        ("", LookupKind::PartialMap),
                        ("abacab", LookupKind::PartialMap),
                        ("abacaxi", LookupKind::Entry),
                        ("abalaba", LookupKind::Entry),
                        ("omiojo", LookupKind::Entry),
                        ("omungal", LookupKind::Entry),
                        ("test", LookupKind::Entry),
                    ]
                    .into_iter()
                    .collect()
                )
                .await?
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_sharded_map_v2_from_entries_only_maps(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();
        let helper = MapHelper(ctx, blobstore);

        let map_ab = helper
            .from_entries_removed_prefix(&EXAMPLE_ENTRIES[0..8], 2)
            .await?;
        let map_omi = helper
            .from_entries_removed_prefix(&EXAMPLE_ENTRIES[8..10], 3)
            .await?;
        let map_omu = helper
            .from_entries_removed_prefix(&EXAMPLE_ENTRIES[10..12], 3)
            .await?;

        let map = helper
            .from_entries_and_partial_maps(&[
                ("ab", Either::Right(map_ab)),
                ("omi", Either::Right(map_omi)),
                ("omu", Either::Right(map_omu)),
            ])
            .await?;

        helper.check_example_map(map).await?;

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_sharded_map_v2_from_entries_maps_and_values(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();
        let helper = MapHelper(ctx, blobstore);

        let map_ab = helper
            .from_entries_removed_prefix(&EXAMPLE_ENTRIES[0..8], 2)
            .await?;

        let map = helper
            .from_entries_and_partial_maps(
                &std::iter::once(("ab", Either::Right(map_ab)))
                    .chain(
                        EXAMPLE_ENTRIES[8..]
                            .iter()
                            .map(|(key, value)| (*key, Either::Left(*value))),
                    )
                    .collect::<Vec<_>>(),
            )
            .await?;

        helper.check_example_map(map).await?;

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_sharded_map_v2_from_entries_conflict_detection(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();
        let helper = MapHelper(ctx, blobstore);

        let map_first_six = helper
            .from_entries_removed_prefix(&EXAMPLE_ENTRIES[0..6], 2)
            .await?;
        let map_last_six = helper.from_entries(&EXAMPLE_ENTRIES[6..12]).await?;

        assert!(
            helper
                .from_entries_and_partial_maps(&[
                    ("ab", Either::Right(map_first_six)),
                    ("", Either::Right(map_last_six)),
                ])
                .await
                .is_err()
        );

        let map_ab = helper
            .from_entries_removed_prefix(&EXAMPLE_ENTRIES[0..8], 2)
            .await?;
        let map_om = helper
            .from_entries_removed_prefix(&EXAMPLE_ENTRIES[8..12], 2)
            .await?;

        assert!(
            helper
                .from_entries_and_partial_maps(&[
                    ("ab", Either::Right(map_ab.clone())),
                    ("om", Either::Right(map_om.clone())),
                ])
                .await
                .is_ok()
        );

        assert!(
            helper
                .from_entries_and_partial_maps(&[
                    ("ab", Either::Right(map_ab.clone())),
                    ("om", Either::Right(map_om.clone())),
                    ("abababab", Either::Left(100)),
                ])
                .await
                .is_err()
        );

        assert!(
            helper
                .from_entries_and_partial_maps(&[
                    ("ab", Either::Right(map_ab.clone())),
                    ("om", Either::Right(map_om.clone())),
                    ("zz", Either::Left(100)),
                ])
                .await
                .is_ok()
        );

        assert!(
            helper
                .from_entries_and_partial_maps(&[
                    ("ab", Either::Right(map_ab.clone())),
                    ("om", Either::Right(map_om.clone())),
                    ("omo", Either::Left(100)),
                ])
                .await
                .is_err()
        );

        assert!(
            helper
                .from_entries_and_partial_maps(&[
                    ("o", Either::Left(100)),
                    ("ab", Either::Right(map_ab)),
                    ("om", Either::Right(map_om)),
                ])
                .await
                .is_ok()
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_sharded_map_v2_difference_stream(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();
        let helper = MapHelper(ctx, blobstore);

        let map = helper
            .from_entries(&[
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
            ])
            .await?;

        let other_map_1 = helper
            .from_entries(&[("aba", 12), ("abacab", 6), ("omiojo", 1), ("omiux", 9)])
            .await?;

        let other_map_2 = helper
            .from_entries(&[
                ("abacab", 13),
                ("abacakkk", 9),
                ("omiojo", 1),
                ("omungal", 4),
            ])
            .await?;

        assert_eq!(
            helper
                .difference_stream(map.clone(), vec![])
                .await?
                .into_iter()
                .sorted()
                .collect::<Vec<_>>(),
            vec![
                ("aba".to_string(), 12, vec![]),
                ("abacab".to_string(), 7, vec![]),
                ("abacaba".to_string(), 8, vec![]),
                ("abacakkk".to_string(), 9, vec![]),
                ("abacate".to_string(), 10, vec![]),
                ("abacaxi".to_string(), 11, vec![]),
                ("abalaba".to_string(), 5, vec![]),
                ("abalada".to_string(), 6, vec![]),
                ("omiojo".to_string(), 1, vec![]),
                ("omiux".to_string(), 2, vec![]),
                ("omundo".to_string(), 3, vec![]),
                ("omungal".to_string(), 4, vec![])
            ],
        );

        assert_eq!(
            helper
                .difference_stream(map.clone(), vec![other_map_1.clone()])
                .await?
                .into_iter()
                .sorted()
                .collect::<Vec<_>>(),
            vec![
                ("abacab".to_string(), 7, vec![6]),
                ("abacaba".to_string(), 8, vec![]),
                ("abacakkk".to_string(), 9, vec![]),
                ("abacate".to_string(), 10, vec![]),
                ("abacaxi".to_string(), 11, vec![]),
                ("abalaba".to_string(), 5, vec![]),
                ("abalada".to_string(), 6, vec![]),
                ("omiux".to_string(), 2, vec![9]),
                ("omundo".to_string(), 3, vec![]),
                ("omungal".to_string(), 4, vec![])
            ]
        );

        assert_eq!(
            helper
                .difference_stream(map.clone(), vec![other_map_2.clone()])
                .await?
                .into_iter()
                .sorted()
                .collect::<Vec<_>>(),
            vec![
                ("aba".to_string(), 12, vec![]),
                ("abacab".to_string(), 7, vec![13]),
                ("abacaba".to_string(), 8, vec![]),
                ("abacate".to_string(), 10, vec![]),
                ("abacaxi".to_string(), 11, vec![]),
                ("abalaba".to_string(), 5, vec![]),
                ("abalada".to_string(), 6, vec![]),
                ("omiux".to_string(), 2, vec![]),
                ("omundo".to_string(), 3, vec![])
            ]
        );

        assert_eq!(
            helper
                .difference_stream(map, vec![other_map_1, other_map_2])
                .await?
                .into_iter()
                .sorted()
                .collect::<Vec<_>>(),
            vec![
                ("abacab".to_string(), 7, vec![6, 13]),
                ("abacaba".to_string(), 8, vec![]),
                ("abacate".to_string(), 10, vec![]),
                ("abacaxi".to_string(), 11, vec![]),
                ("abalaba".to_string(), 5, vec![]),
                ("abalada".to_string(), 6, vec![]),
                ("omiux".to_string(), 2, vec![9]),
                ("omundo".to_string(), 3, vec![])
            ]
        );

        Ok(())
    }

    /// FIXME: BUG! This did drop the entries of a node whose prefix is longer than the longest
    /// common prefix with the other maps (`{"foo": 1}` vs `{"bar": 1}` yields nothing) and
    /// forget the other maps' values behind such a prefix (`{"a": 1, "ab": 2}` vs `{"ab": 2}`
    /// also yields `("ab", 2, [])`), but should have yielded exactly the entries whose value
    /// differs from all other maps, each with the other maps' values at that key. This test
    /// pins the current behaviour; the next diff fixes it.
    #[mononoke::fbinit_test]
    async fn test_sharded_map_v2_difference_stream_longer_prefix(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();
        let helper = MapHelper(ctx, blobstore);

        // The map's only entry is absent from the other map, so it must be reported, but
        // the node prefixes ("foo" and "bar") diverge and the entry is dropped.
        let map = helper.from_entries(&[("foo", 1)]).await?;
        let other = helper.from_entries(&[("bar", 1)]).await?;
        // FIXME: BUG! The entry "foo" is missing; this should be `[("foo", 1, [])]`.
        assert_eq!(
            helper
                .difference_stream(map, vec![other])
                .await?
                .into_iter()
                .sorted()
                .collect::<Vec<_>>(),
            Vec::<(String, u32, Vec<u32>)>::new(),
        );

        // "ab" has the same value in both maps, so only "a" differs, but the other map's
        // value for "ab" is taken while its node still carries the prefix "b", so "ab" is
        // also reported, with no previous values.
        let map = helper.from_entries(&[("a", 1), ("ab", 2)]).await?;
        let other = helper.from_entries(&[("ab", 2)]).await?;
        // FIXME: BUG! "ab" must not be reported; this should be `[("a", 1, [])]`.
        assert_eq!(
            helper
                .difference_stream(map, vec![other])
                .await?
                .into_iter()
                .sorted()
                .collect::<Vec<_>>(),
            vec![("a".to_string(), 1, vec![]), ("ab".to_string(), 2, vec![])],
        );

        Ok(())
    }

    #[mononoke::quickcheck_test]
    async fn test_sharded_map_v2_quickcheck(
        fb: FacebookInit,
        values: BTreeMap<String, u32>,
        queries: Vec<String>,
    ) -> bool {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();

        let helper = MapHelper(ctx, blobstore);

        let map = helper
            .from_entries(
                &values
                    .iter()
                    .map(|(k, v)| (k.as_str(), *v))
                    .collect::<Vec<_>>(),
            )
            .await
            .unwrap();

        helper.check_sharded_map(map.clone()).await.unwrap();

        let queries = queries
            .into_iter()
            .chain(
                values
                    .keys()
                    .take(values.len() / 2)
                    .map(|key| key.to_string()),
            )
            .collect::<Vec<String>>();

        for key in queries {
            let correct_v = values.get(&key).cloned().map(TestValue);
            let test_v = helper.lookup(&map, &key).await.unwrap();
            if correct_v != test_v {
                return false;
            }
        }

        let roundtrip_map = helper
            .into_entries(map.clone())
            .await
            .unwrap()
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        if roundtrip_map != values {
            return false;
        }

        let roundtrip_unordered_map = helper
            .into_entries_unordered(map.clone())
            .await
            .unwrap()
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        if roundtrip_unordered_map != values {
            return false;
        }

        let max_value = values.values().max().copied().unwrap_or_default();
        let rollup_data = map.rollup_data();

        if rollup_data != MaxTestValue(max_value) {
            return false;
        }

        true
    }

    #[mononoke::quickcheck_test]
    async fn test_sharded_map_v2_quickcheck_batch_lookup(
        fb: FacebookInit,
        values: Vec<(String, u32)>,
        lookup_keys: TrieMap<LookupKind>,
    ) -> bool {
        let ctx = CoreContext::test_mock(fb);
        let blobstore = KeyedMemblob::default();

        let helper = MapHelper(ctx, blobstore);

        let map = helper
            .from_entries(
                &values
                    .iter()
                    .map(|(k, v)| (k.as_str(), *v))
                    .collect::<Vec<_>>(),
            )
            .await
            .unwrap();

        let batched_outputs = helper
            .get_entries_and_partial_maps(&map, lookup_keys.clone())
            .await
            .unwrap();

        for (key, kind) in lookup_keys {
            match kind {
                LookupKind::Entry => {
                    let unbatched_lookup = helper.lookup(&map, &key).await.unwrap();
                    let batched_lookup = batched_outputs.get(&key).cloned();

                    if unbatched_lookup.map(Either::Left) != batched_lookup {
                        return false;
                    }
                }
                LookupKind::PartialMap => {
                    let unbatched_lookup = helper.get_partial_map(&map, &key).await.unwrap();
                    let batched_lookup = batched_outputs.get(&key).cloned();

                    if unbatched_lookup.map(Either::Right) != batched_lookup {
                        return false;
                    }
                }
            }
        }

        true
    }
}
