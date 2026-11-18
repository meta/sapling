/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt::Debug;
use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::ensure;
use blobstore::KeyedBlobstore;
use context::CoreContext;
use fbthrift::compact_protocol;
use futures::TryStreamExt;
use futures::future::try_join_all;
use futures::stream;
use futures::stream::BoxStream;
use futures::stream::StreamExt;
use itertools::Either;
use manifest::Deferred;
use manifest::Entry;
use manifest::KnownEntry;
use manifest::LeafInfo;
use manifest::Manifest;
use manifest::ManifestOps;
use manifest::PathOrPrefix;
use manifest::StoreLoadable;
use manifest::TreeInfo;
use manifest::TreeInfoSubentries;
use manifest::TrieMapOps;
use manifest::derive_manifest_subentries;
use manifest::derive_manifest_with_known_entries;
use manifest::find_entries_from_subentries;
use manifest::flatten_subentries;
use mononoke_types::BlobstoreBytes;
use mononoke_types::MPath;
use mononoke_types::MPathElement;
use mononoke_types::NonRootMPath;
use mononoke_types::SortedVectorTrieMap;
use mononoke_types::ThriftConvert;
use mononoke_types::hash::Blake2;
use mononoke_types::hash::Context as HashContext;
use mononoke_types::sharded_map_v2::LoadableShardedMapV2Node;
use mononoke_types::sharded_map_v2::ShardedMapV2Value;
use tokio::sync::OnceCell;

use crate::ChildNameRange;
use crate::DerivationContext;
use crate::ManifestDerivable;
use crate::StageId;
use crate::stage_outputs::StageOutput;

/// Native children and their cached RPC encoding.
#[derive(Clone, Debug)]
pub struct PartitionOutput<Map> {
    map: Deferred<Map>,
    encoded: Arc<OnceCell<BlobstoreBytes>>,
}

impl<Map: Clone + Send + Sync + 'static> PartitionOutput<Map> {
    pub async fn load(self) -> Result<Map> {
        self.map.load().await
    }

    /// Change in-memory metadata without changing the map's encoded contents.
    pub fn map_preserving_encoding(
        self,
        map: impl Fn(Map) -> Map + Clone + Send + Sync + 'static,
    ) -> Self {
        Self {
            map: self.map.map(map),
            encoded: self.encoded,
        }
    }
}

impl<Map> From<Map> for PartitionOutput<Map> {
    fn from(map: Map) -> Self {
        Self {
            map: map.into(),
            encoded: Arc::new(OnceCell::new()),
        }
    }
}

#[derive(Clone, Debug)]
pub enum ManifestStageOutput<TreeId, Leaf, TrieMapType> {
    Entry(Option<Entry<TreeId, Leaf>>),
    Subentries(PartitionOutput<TrieMapType>),
}

impl<TreeId, Leaf, TrieMapType> ManifestStageOutput<TreeId, Leaf, TrieMapType> {
    pub fn into_entry(self) -> Result<Option<Entry<TreeId, Leaf>>> {
        match self {
            Self::Entry(entry) => Ok(entry),
            Self::Subentries(_) => Err(anyhow!("expected a manifest entry, got subentries")),
        }
    }

    pub async fn into_subentries(self) -> Result<TrieMapType>
    where
        TrieMapType: Clone + Send + Sync + 'static,
    {
        self.into_partition()?.load().await
    }

    fn into_partition(self) -> Result<PartitionOutput<TrieMapType>> {
        match self {
            Self::Subentries(entries) => Ok(entries),
            Self::Entry(_) => Err(anyhow!("expected subentries, got a manifest entry")),
        }
    }
}

#[async_trait::async_trait]
impl<TreeId, Leaf, Map> StageOutput for ManifestStageOutput<TreeId, Leaf, Map>
where
    TreeId: Clone + Debug + PartialEq + Send + Sync + 'static,
    Leaf: Clone + Debug + PartialEq + Send + Sync + 'static,
    Map: Clone + Debug + PartialEq + Send + Sync + 'static,
{
    async fn equivalent(&self, other: &Self) -> Result<bool> {
        match (self, other) {
            (Self::Entry(a), Self::Entry(b)) => Ok(a == b),
            (Self::Subentries(a), Self::Subentries(b)) => {
                let (a, b) =
                    futures::future::try_join(a.map.clone().load(), b.map.clone().load()).await?;
                Ok(a == b)
            }
            _ => Ok(false),
        }
    }
}

impl<TreeId, Leaf, Map> ManifestStageOutput<TreeId, Leaf, Map> {
    /// Look up repository-relative parent paths, loading the completed root only
    /// for paths outside this stage's coverage.
    pub async fn find_parent_entries<S, F, Fut>(
        self,
        ctx: CoreContext,
        store: S,
        stage: &StageId,
        paths: Vec<NonRootMPath>,
        load_root: F,
    ) -> Result<HashMap<NonRootMPath, Entry<TreeId, Leaf>>>
    where
        S: Clone + Send + Sync + 'static,
        TreeId: StoreLoadable<S> + Clone + Eq + Send + Sync + Unpin + 'static,
        TreeId::Value: Manifest<S, TreeId = TreeId, Leaf = Leaf> + Send + Sync,
        Leaf: Clone + Eq + Send + Sync + Unpin + 'static,
        Map: TrieMapOps<S, Entry<TreeId, Leaf>> + Clone + Send + Sync + 'static,
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<Option<TreeId>>> + Send,
    {
        let prefix = match stage {
            StageId::Manifest(path) | StageId::Subentries(path, _) => path,
            StageId::Finalize => anyhow::bail!("finalize has no manifest entries"),
        };
        let (local, external): (Vec<_>, Vec<_>) =
            paths.into_iter().partition(|path| stage.covers(path));
        let local = self
            .find_entries(
                ctx.clone(),
                store.clone(),
                local
                    .into_iter()
                    .map(|path| {
                        PathOrPrefix::Path(MPath::from(path).remove_prefix_component(prefix))
                    })
                    .collect(),
            )
            .map_ok(|(path, entry)| Ok((prefix.join(path.iter()).try_into()?, entry)))
            .and_then(futures::future::ready)
            .try_collect::<HashMap<_, _>>();
        let external = async {
            if external.is_empty() {
                return Ok(HashMap::new());
            }
            let Some(root) = load_root().await? else {
                return Ok(HashMap::new());
            };
            root.find_entries(ctx, store, external)
                .map_ok(|(path, entry)| Ok((path.try_into()?, entry)))
                .and_then(futures::future::ready)
                .try_collect::<HashMap<_, _>>()
                .await
        };
        let (mut entries, external) = futures::future::try_join(local, external).await?;
        entries.extend(external);
        Ok(entries)
    }

    /// Paths and results are relative to this stage's directory.
    pub fn find_entries<S>(
        self,
        ctx: CoreContext,
        store: S,
        paths: Vec<PathOrPrefix>,
    ) -> BoxStream<'static, Result<(MPath, Entry<TreeId, Leaf>)>>
    where
        S: Clone + Send + Sync + 'static,
        TreeId: StoreLoadable<S> + Clone + Eq + Send + Sync + Unpin + 'static,
        TreeId::Value: Manifest<S, TreeId = TreeId, Leaf = Leaf> + Send + Sync,
        Leaf: Clone + Eq + Send + Sync + Unpin + 'static,
        Map: TrieMapOps<S, Entry<TreeId, Leaf>> + Clone + Send + Sync + 'static,
    {
        if paths.is_empty() {
            return stream::empty().boxed();
        }
        match self {
            Self::Entry(Some(Entry::Tree(tree))) => tree.find_entries(ctx, store, paths),
            Self::Entry(Some(Entry::Leaf(leaf))) => {
                let selected = paths.iter().any(|path| match path {
                    PathOrPrefix::Path(path) | PathOrPrefix::Prefix(path) => path.is_root(),
                });
                stream::iter(selected.then_some(Ok((MPath::ROOT, Entry::Leaf(leaf))))).boxed()
            }
            Self::Entry(None) => stream::empty().boxed(),
            Self::Subentries(map) => stream::once(async move {
                Ok::<_, anyhow::Error>(find_entries_from_subentries(
                    ctx,
                    store,
                    map.load().await?,
                    paths,
                ))
            })
            .try_flatten()
            .boxed(),
        }
    }
}

/// Inputs for an entry, or for children in a first-byte range.
pub enum ManifestDerivationInput<TreeId, Leaf, TrieMapType> {
    Entry {
        path: MPath,
        /// Bonsai parent order, including parents where this entry is absent.
        parents: Vec<Option<Entry<TreeId, Leaf>>>,
        known_entries: HashMap<MPath, KnownEntry<TreeId, Leaf, TrieMapType>>,
    },
    Partition {
        path: MPath,
        range: ChildNameRange,
        parents: Vec<PartitionOutput<TrieMapType>>,
    },
}

impl<TreeId, Leaf, TrieMapType: Clone + Send + Sync + 'static>
    ManifestDerivationInput<TreeId, Leaf, TrieMapType>
{
    pub fn from_stage_outputs(
        stage: &StageId,
        parents: Vec<ManifestStageOutput<TreeId, Leaf, TrieMapType>>,
        dependencies: HashMap<StageId, ManifestStageOutput<TreeId, Leaf, TrieMapType>>,
    ) -> Result<Self> {
        match stage {
            StageId::Manifest(path) => {
                let parents = parents
                    .into_iter()
                    .map(ManifestStageOutput::into_entry)
                    .collect::<Result<_>>()?;
                let mut known_entries = HashMap::new();
                let mut children = Vec::new();
                for (stage, output) in dependencies {
                    match stage {
                        StageId::Manifest(path) => {
                            known_entries.insert(path, KnownEntry::Entry(output.into_entry()?));
                        }
                        StageId::Subentries(..) => children.push(output.into_partition()?),
                        StageId::Finalize => anyhow::bail!("unexpected finalize dependency"),
                    }
                }
                if !children.is_empty() {
                    known_entries.insert(
                        path.clone(),
                        KnownEntry::Children(Deferred::new(move || {
                            let children = children.clone();
                            async move {
                                try_join_all(children.into_iter().map(PartitionOutput::load)).await
                            }
                        })),
                    );
                }
                Ok(Self::Entry {
                    path: path.clone(),
                    parents,
                    known_entries,
                })
            }
            StageId::Subentries(path, range) => Ok(Self::Partition {
                path: path.clone(),
                range: *range,
                parents: parents
                    .into_iter()
                    .map(ManifestStageOutput::into_partition)
                    .collect::<Result<_>>()?,
            }),
            StageId::Finalize => anyhow::bail!("finalize has no manifest inputs"),
        }
    }

    /// Translate repository paths into the manifest's coordinates.
    pub(crate) fn map_paths(self, map: impl Fn(&MPath) -> Option<MPath>) -> Option<Self> {
        Some(match self {
            Self::Entry {
                path,
                parents,
                known_entries,
            } => Self::Entry {
                path: map(&path)?,
                parents,
                known_entries: known_entries
                    .into_iter()
                    .filter_map(|(path, entry)| Some((map(&path)?, entry)))
                    .collect(),
            },
            Self::Partition {
                path,
                range,
                parents,
            } => Self::Partition {
                path: map(&path)?,
                range,
                parents,
            },
        })
    }

    pub fn path(&self) -> &MPath {
        match self {
            Self::Entry { path, .. } | Self::Partition { path, .. } => path,
        }
    }

    pub fn stage_id(&self) -> StageId {
        match self {
            Self::Entry { path, .. } => StageId::Manifest(path.clone()),
            Self::Partition { path, range, .. } => StageId::Subentries(path.clone(), *range),
        }
    }

    pub fn parent_outputs(&self) -> Vec<ManifestStageOutput<TreeId, Leaf, TrieMapType>>
    where
        TreeId: Clone,
        Leaf: Clone,
        TrieMapType: Clone,
    {
        match self {
            Self::Entry { parents, .. } => parents
                .iter()
                .cloned()
                .map(ManifestStageOutput::Entry)
                .collect(),
            Self::Partition { parents, .. } => parents
                .iter()
                .cloned()
                .map(ManifestStageOutput::Subentries)
                .collect(),
        }
    }

    pub fn contains_change(&self, path: &NonRootMPath) -> bool {
        match self {
            Self::Entry {
                path: stage_path,
                known_entries,
                ..
            } => {
                stage_path.is_prefix_of(path)
                    && !known_entries.iter().any(|(known, entry)| match entry {
                        KnownEntry::Entry(_) => known.is_prefix_of(path),
                        KnownEntry::Children(_) => {
                            known.is_prefix_of(path) && !path.is_prefix_of(known)
                        }
                    })
            }
            Self::Partition {
                path: stage_path,
                range,
                ..
            } => range.contains_path(stage_path, path),
        }
    }

    pub async fn derive<S, Change, Ctx, T, TFut, L, LFut, P, PFut>(
        self,
        ctx: CoreContext,
        store: S,
        changes: Vec<(NonRootMPath, Option<Change>)>,
        create_tree: T,
        create_leaf: L,
        create_subentries: P,
    ) -> Result<ManifestStageOutput<TreeId, Leaf, TrieMapType>>
    where
        S: KeyedBlobstore + Clone + Send + Sync + 'static,
        Change: Send + Clone + Eq + Hash + Debug + 'static,
        Leaf: Send + Sync + Clone + Eq + Hash + Debug + 'static,
        TreeId: StoreLoadable<S> + Clone + Eq + Hash + Debug + Send + Sync + 'static,
        TreeId::Value:
            Manifest<S, TreeId = TreeId, Leaf = Leaf, TrieMapType = TrieMapType> + Send + Sync,
        T: Fn(TreeInfo<TreeId, Leaf, Ctx, TrieMapType>) -> TFut + Send + Sync + 'static,
        TFut: Future<Output = Result<(Ctx, TreeId)>> + Send + 'static,
        L: Fn(LeafInfo<Leaf, Change>) -> LFut + Send + Sync + 'static,
        LFut: Future<Output = Result<(Ctx, Leaf)>> + Send + 'static,
        Ctx: Send + 'static,
        TrieMapType: TrieMapOps<S, Entry<TreeId, Leaf>> + Clone + Send + Sync + 'static,
        P: Fn(TreeInfoSubentries<TreeId, Leaf, Ctx, TrieMapType>) -> PFut + Send + Sync,
        PFut: Future<Output = Result<TrieMapType>> + Send,
    {
        match self {
            Self::Entry {
                path,
                parents,
                known_entries,
            } => {
                let root_parents = path.is_root().then(|| {
                    parents
                        .iter()
                        .filter_map(|entry| entry.clone()?.into_tree())
                        .collect()
                });
                let create_tree = Arc::new(create_tree);
                let entry = derive_manifest_with_known_entries(
                    ctx,
                    store,
                    parents.into_iter().flatten(),
                    changes,
                    known_entries,
                    path,
                    {
                        let create_tree = create_tree.clone();
                        move |info| create_tree(info)
                    },
                    create_leaf,
                )
                .await?;
                let entry = match (entry, root_parents) {
                    (None, Some(parents)) => Some(Entry::Tree(
                        create_tree(TreeInfo {
                            path: MPath::ROOT,
                            parents,
                            subentries: Default::default(),
                        })
                        .await?
                        .1,
                    )),
                    (entry, _) => entry,
                };
                Ok(ManifestStageOutput::Entry(entry))
            }
            Self::Partition {
                path,
                range,
                parents,
            } => {
                let subentries = if changes.iter().any(|(change_path, change)| {
                    change.is_some() && change_path.is_prefix_of(&path)
                }) {
                    BTreeMap::new()
                } else {
                    let changes = changes
                        .into_iter()
                        .filter(|(change_path, _)| range.contains_path(&path, change_path))
                        .collect::<Vec<_>>();
                    if changes.is_empty() && parents.len() == 1 {
                        return Ok(ManifestStageOutput::Subentries(
                            parents.into_iter().next().unwrap(),
                        ));
                    }
                    derive_manifest_subentries(
                        ctx,
                        store,
                        try_join_all(parents.into_iter().map(PartitionOutput::load)).await?,
                        changes,
                        path,
                        create_tree,
                        create_leaf,
                    )
                    .await?
                };
                let map = create_subentries(subentries).await?;
                Ok(ManifestStageOutput::Subentries(map.into()))
            }
        }
    }
}

/// Select a first-byte range without enumerating the native map's child entries.
pub async fn select_subentries<S, TreeId, Leaf, Map, M>(
    ctx: &CoreContext,
    store: &S,
    entry: Option<Entry<TreeId, Leaf>>,
    range: ChildNameRange,
) -> Result<TreeInfoSubentries<TreeId, Leaf, (), Map>>
where
    S: Send + Sync + 'static,
    TreeId: StoreLoadable<S, Value = M> + Send + Sync,
    M: Manifest<S, TreeId = TreeId, Leaf = Leaf, TrieMapType = Map> + Send + Sync,
    Map: TrieMapOps<S, Entry<TreeId, Leaf>>,
{
    match entry {
        Some(Entry::Tree(tree)) => {
            let map = tree
                .load(ctx, store)
                .await?
                .into_trie_map(ctx, store)
                .await?;
            Ok(expand_subentries(ctx, store, map)
                .await?
                .into_iter()
                .filter(|(prefix, _)| range.contains_byte(prefix[0]))
                .collect())
        }
        _ => Ok(BTreeMap::new()),
    }
}

pub(crate) async fn expand_subentries<S, TreeId, Leaf, Ctx, TrieMapType>(
    ctx: &CoreContext,
    store: &S,
    map: TrieMapType,
) -> Result<TreeInfoSubentries<TreeId, Leaf, Ctx, TrieMapType>>
where
    TrieMapType: TrieMapOps<S, Entry<TreeId, Leaf>>,
{
    let (entry, children) = map.expand(ctx, store).await?;
    ensure!(entry.is_none(), "manifest child name must not be empty");
    Ok(children
        .into_iter()
        .map(|(byte, map)| (std::iter::once(byte).collect(), Either::Right(map)))
        .collect())
}

/// Materialize fragments for manifests whose child map is an in-memory sorted vector.
pub async fn create_sorted_subentries<TreeId, Leaf, Ctx>(
    ctx: &CoreContext,
    subentries: TreeInfoSubentries<TreeId, Leaf, Ctx, SortedVectorTrieMap<Entry<TreeId, Leaf>>>,
) -> Result<SortedVectorTrieMap<Entry<TreeId, Leaf>>>
where
    TreeId: Clone + Send + Sync,
    Leaf: Clone + Send + Sync,
{
    if subentries.len() == 1
        && let Some((prefix, Either::Right(map))) = subentries.first_key_value()
        && prefix.is_empty()
    {
        return Ok(map.clone());
    }
    Ok(SortedVectorTrieMap::new(
        flatten_subentries(ctx, &(), subentries)
            .await?
            .map(|(name, (_, entry))| (name.to_smallvec(), entry))
            .collect(),
    ))
}

/// Stage encoding follows the native child-map representation.
#[async_trait::async_trait]
pub trait PartitionMap<T: ManifestDerivable>: Sized {
    async fn encode(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        output: PartitionOutput<Self>,
    ) -> Result<BlobstoreBytes>;
    fn decode(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bytes: BlobstoreBytes,
    ) -> Result<PartitionOutput<Self>>;
}

#[async_trait::async_trait]
impl<T: ManifestDerivable> PartitionMap<T> for SortedVectorTrieMap<Entry<T::TreeId, T::Leaf>> {
    async fn encode(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        output: PartitionOutput<Self>,
    ) -> Result<BlobstoreBytes> {
        store_sorted_subentries(ctx, derivation.blobstore(), output, |entry| {
            Ok(T::encode_entry(Some(entry)))
        })
        .await
    }

    fn decode(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bytes: BlobstoreBytes,
    ) -> Result<PartitionOutput<Self>> {
        decode_partition_reference(
            ctx,
            derivation.blobstore().clone(),
            derivation.stage_inputs.clone(),
            bytes,
            |bytes| {
                T::decode_entry(bytes)?.ok_or_else(|| anyhow!("empty entry in manifest partition"))
            },
        )
    }
}

#[async_trait::async_trait]
impl<T: ManifestDerivable, V: ShardedMapV2Value> PartitionMap<T> for LoadableShardedMapV2Node<V> {
    async fn encode(
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
        output: PartitionOutput<Self>,
    ) -> Result<BlobstoreBytes> {
        Ok(output
            .encoded
            .get_or_try_init(|| async {
                Ok::<_, anyhow::Error>(BlobstoreBytes::from_bytes(
                    output.map.load().await?.into_bytes(),
                ))
            })
            .await?
            .clone())
    }

    fn decode(
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
        bytes: BlobstoreBytes,
    ) -> Result<PartitionOutput<Self>> {
        let encoded = Arc::new(OnceCell::new_with(Some(bytes.clone())));
        let decoded = Arc::new(OnceCell::new());
        let map = Deferred::new(move || {
            let (bytes, decoded) = (bytes.clone(), decoded.clone());
            async move {
                decoded
                    .get_or_try_init(async || Self::from_bytes(&bytes.into_bytes()))
                    .await
                    .cloned()
            }
        });
        Ok(PartitionOutput { map, encoded })
    }
}

fn snapshot_key(id: Blake2) -> String {
    format!("derivation_pipeline.children.v1.{id}")
}

pub async fn store_sorted_subentries<V: Clone + Send + Sync + 'static>(
    ctx: &CoreContext,
    store: &impl KeyedBlobstore,
    output: PartitionOutput<SortedVectorTrieMap<V>>,
    encode: impl Fn(V) -> Result<BlobstoreBytes> + Send + Sync,
) -> Result<BlobstoreBytes> {
    let encoded = output
        .encoded
        .get_or_try_init(|| async {
            let bytes = encode_sorted_subentries(output.map.load().await?, encode)?;
            let mut hash = HashContext::new(b"derivation_pipeline.children.v1");
            hash.update(bytes.as_bytes());
            let id = hash.finish();
            store.put(ctx, snapshot_key(id), bytes).await?;
            Ok::<_, anyhow::Error>(BlobstoreBytes::from_bytes(id.as_ref().to_vec()))
        })
        .await?;
    Ok(encoded.clone())
}

pub fn decode_partition_reference<V: Clone + Send + Sync + 'static>(
    ctx: &CoreContext,
    store: impl KeyedBlobstore + Clone + 'static,
    inputs: Arc<crate::StageInputs>,
    bytes: BlobstoreBytes,
    decode: impl Fn(BlobstoreBytes) -> Result<V> + Clone + Send + Sync + 'static,
) -> Result<PartitionOutput<SortedVectorTrieMap<V>>> {
    let id = Blake2::from_bytes(bytes.as_bytes())?;
    let ctx = ctx.clone();
    let map = Deferred::new(move || {
        let (ctx, store, inputs, decode) =
            (ctx.clone(), store.clone(), inputs.clone(), decode.clone());
        async move {
            inputs
                .load_snapshot(id, async || {
                    let blob = store
                        .get(&ctx, &snapshot_key(id))
                        .await?
                        .ok_or_else(|| anyhow!("missing partition snapshot {id}"))?;
                    decode_sorted_subentries(blob.into(), decode)
                })
                .await
        }
    });
    Ok(PartitionOutput {
        map,
        encoded: Arc::new(OnceCell::new_with(Some(bytes))),
    })
}

/// Serialize sorted children using the manifest type's entry encoding.
pub fn encode_sorted_subentries<V: Clone>(
    map: SortedVectorTrieMap<V>,
    encode: impl Fn(V) -> Result<BlobstoreBytes>,
) -> Result<BlobstoreBytes> {
    let output = derivation_queue_thrift::ManifestSubentries {
        entries: map
            .into_iter()
            .map(|(name, entry)| {
                Ok(derivation_queue_thrift::ManifestSubentry {
                    name: MPathElement::from_smallvec(name)?.into_thrift(),
                    entry: encode(entry)?.into_bytes().to_vec(),
                    ..Default::default()
                })
            })
            .collect::<Result<_>>()?,
        ..Default::default()
    };
    Ok(BlobstoreBytes::from_bytes(compact_protocol::serialize(
        &output,
    )))
}

/// Read sorted children encoded with `encode_sorted_subentries`.
pub fn decode_sorted_subentries<V>(
    bytes: BlobstoreBytes,
    decode: impl Fn(BlobstoreBytes) -> Result<V>,
) -> Result<SortedVectorTrieMap<V>> {
    let output: derivation_queue_thrift::ManifestSubentries =
        compact_protocol::deserialize(bytes.into_bytes())?;
    let entries = output
        .entries
        .into_iter()
        .map(|entry| {
            Ok((
                MPathElement::from_thrift(entry.name)?.to_smallvec(),
                decode(BlobstoreBytes::from_bytes(entry.entry))?,
            ))
        })
        .collect::<Result<_>>()?;
    Ok(SortedVectorTrieMap::new(entries))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use blobstore::Storable;
    use counting_blob::BlobstoreCounters;
    use counting_blob::CountingBlobstore;
    use fbinit::FacebookInit;
    use futures::TryStreamExt;
    use memblob::Memblob;
    use mononoke_macros::mononoke;
    use mononoke_types::BlobstoreValue;
    use mononoke_types::RepositoryId;
    use mononoke_types::sharded_map_v2::LoadableShardedMapV2Node;
    use mononoke_types::sharded_map_v2::ShardedMapV2Node;
    use mononoke_types::test_sharded_manifest::TestShardedManifest;
    use mononoke_types::test_sharded_manifest::TestShardedManifestDirectory;
    use mononoke_types::test_sharded_manifest::TestShardedManifestEntry;
    use mononoke_types::test_sharded_manifest::TestShardedManifestFile;
    use repo_blobstore::RepoBlobstore;
    use scuba_ext::MononokeScubaSampleBuilder;

    use super::*;

    #[mononoke::fbinit_test]
    async fn parent_lookup_respects_stage_coverage(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let store = RepoBlobstore::new(
            Arc::new(Memblob::default()),
            None,
            RepositoryId::new(0),
            MononokeScubaSampleBuilder::with_discard(),
        );
        let save = |entries: Vec<(&'static str, TestShardedManifestEntry)>| {
            let ctx = &ctx;
            let store = &store;
            async move {
                let subentries = ShardedMapV2Node::from_entries(ctx, store, entries).await?;
                let max_basename_length = subentries.rollup_data();
                let id = TestShardedManifest { subentries }
                    .into_blob()
                    .store(ctx, store)
                    .await?;
                anyhow::Ok(TestShardedManifestDirectory {
                    id,
                    max_basename_length,
                })
            }
        };
        let file = TestShardedManifestEntry::File(TestShardedManifestFile { basename_length: 1 });
        let directory = save(vec![("absent", file.clone()), ("zulu", file.clone())]).await?;
        let root = save(vec![
            ("stage", TestShardedManifestEntry::Directory(directory)),
            ("outside", file),
        ])
        .await?;
        let stage = StageId::Subentries(MPath::new("stage")?, ChildNameRange::new(0, b'm')?);
        let map = SortedVectorTrieMap::new(
            [(
                MPathElement::new_from_slice(b"alpha")?.to_smallvec(),
                Entry::Leaf(()),
            )]
            .into_iter()
            .collect(),
        );
        let output: ManifestStageOutput<TestShardedManifestDirectory, (), _> =
            ManifestStageOutput::Subentries(map.into());
        let paths = ["stage/alpha", "stage/absent", "stage/zulu", "outside"]
            .into_iter()
            .map(NonRootMPath::new)
            .collect::<Result<Vec<_>>>()?;
        let loads = std::sync::atomic::AtomicUsize::new(0);
        let entries = output
            .clone()
            .find_parent_entries(ctx.clone(), store.clone(), &stage, paths, || async {
                loads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(Some(root))
            })
            .await?;
        assert_eq!(loads.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(entries.len(), 3);
        for name in ["stage/alpha", "stage/zulu", "outside"] {
            assert_eq!(
                entries.get(&NonRootMPath::new(name)?),
                Some(&Entry::Leaf(()))
            );
        }
        assert!(!entries.contains_key(&NonRootMPath::new("stage/absent")?));
        let entries = output
            .find_parent_entries(
                ctx.clone(),
                store.clone(),
                &stage,
                vec![NonRootMPath::new("stage/absent")?],
                || async { anyhow::bail!("a missing local entry must not load the parent root") },
            )
            .await?;
        assert!(entries.is_empty());
        let leaf: ManifestStageOutput<
            TestShardedManifestDirectory,
            (),
            SortedVectorTrieMap<Entry<TestShardedManifestDirectory, ()>>,
        > = ManifestStageOutput::Entry(Some(Entry::Leaf(())));
        let path = NonRootMPath::new("stage")?;
        let entries = leaf
            .find_parent_entries(
                ctx,
                store,
                &StageId::Manifest(path.clone().into()),
                vec![path.clone()],
                || async { anyhow::bail!("a file-valued stage root is local") },
            )
            .await?;
        assert_eq!(entries.get(&path), Some(&Entry::Leaf(())));
        Ok(())
    }

    #[tokio::test]
    async fn inputs_preserve_absent_parent_slots() -> Result<()> {
        let parents = vec![
            ManifestStageOutput::Entry(None),
            ManifestStageOutput::Entry(Some(Entry::Leaf(7))),
        ];
        let input = ManifestDerivationInput::<u8, u8, ()>::from_stage_outputs(
            &StageId::Manifest(MPath::new("stage")?),
            parents.clone(),
            HashMap::new(),
        )?;
        let actual = input.parent_outputs();
        assert_eq!(actual.len(), parents.len());
        for (actual, expected) in actual.iter().zip(&parents) {
            assert!(actual.equivalent(expected).await?);
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn partitions_preserve_stored_shards(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let counters = Arc::new(BlobstoreCounters::new());
        let store = RepoBlobstore::new(
            Arc::new(CountingBlobstore::new(Memblob::default(), counters.clone())),
            None,
            RepositoryId::new(0),
            MononokeScubaSampleBuilder::with_discard(),
        );
        let mut partitions = Vec::new();
        let mut expected_subentries = BTreeMap::new();
        for byte in *b"az" {
            let entries = (0..128).map(|n| {
                (
                    format!("{}{n:03}", char::from(byte)),
                    TestShardedManifestEntry::File(TestShardedManifestFile { basename_length: 4 }),
                )
            });
            let parent = LoadableShardedMapV2Node::Inlined(
                ShardedMapV2Node::from_entries(&ctx, &store, entries).await?,
            )
            .stored(&ctx, &store)
            .await?;
            expected_subentries
                .extend(expand_subentries::<_, _, _, (), _>(&ctx, &store, parent.clone()).await?);
            counters.reset();
            let input = ManifestDerivationInput::Partition {
                path: MPath::ROOT,
                range: ChildNameRange::new(byte, byte)?,
                parents: vec![parent.clone().into()],
            };
            let output = input
                .derive(
                    ctx.clone(),
                    store.clone(),
                    Vec::<(NonRootMPath, Option<()>)>::new(),
                    |_| async {
                        Err::<((), TestShardedManifestDirectory), _>(anyhow!(
                            "unchanged partition must not create trees"
                        ))
                    },
                    |_| async { anyhow::bail!("unchanged partition must not create leaves") },
                    |subentries| async move {
                        assert_eq!(subentries.len(), 1);
                        let (prefix, entry) = subentries.into_iter().next().unwrap();
                        assert!(prefix.is_empty());
                        match entry {
                            Either::Right(map) => Ok(map),
                            Either::Left(_) => anyhow::bail!("unchanged map was enumerated"),
                        }
                    },
                )
                .await?
                .into_subentries()
                .await?;
            assert_eq!(output, parent);
            assert_eq!(counters.snapshot().gets, 0);
            assert_eq!(counters.snapshot().puts, 0);
            let changed = manifest::find_intersection_of_subentry_diffs(
                ctx.clone(),
                store.clone(),
                output.clone(),
                vec![parent.clone(), parent],
                |_| true,
            )
            .try_collect::<Vec<_>>()
            .await?;
            assert!(changed.is_empty());
            assert_eq!(counters.snapshot().gets, 0);
            partitions.push(output);
        }
        counters.reset();
        let root = TestShardedManifestDirectory {
            id: *TestShardedManifest::empty().into_blob().id(),
            max_basename_length: Default::default(),
        };
        let expected_root = root.clone();
        let input = ManifestDerivationInput::Entry {
            path: MPath::ROOT,
            parents: vec![],
            known_entries: HashMap::from([(MPath::ROOT, KnownEntry::Children(partitions.into()))]),
        };
        let output = input
            .derive(
                ctx,
                store,
                Vec::<(NonRootMPath, Option<()>)>::new(),
                move |info| {
                    assert_eq!(info.subentries, expected_subentries);
                    let root = root.clone();
                    async move { Ok(((), root)) }
                },
                |_| async { anyhow::bail!("assembly must not create leaves") },
                |_| async { anyhow::bail!("assembly must pass maps to the tree callback") },
            )
            .await?;
        assert_eq!(output.into_entry()?, Some(Entry::Tree(expected_root)));
        // Load each partition root, retaining the stored shards beneath it.
        assert_eq!(counters.snapshot().gets, 2);
        assert_eq!(counters.snapshot().puts, 0);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn assembled_sharded_manifest_has_canonical_hash(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let store = RepoBlobstore::new(
            Arc::new(Memblob::default()),
            None,
            RepositoryId::new(0),
            MononokeScubaSampleBuilder::with_discard(),
        );
        // Cross the test map's inlining threshold, including compressed prefixes
        // and names which are prefixes of other names.
        for (path, size) in [
            ("", 0),
            ("", 1),
            ("", 4),
            ("", 5),
            ("", 6),
            ("", 128),
            ("nested/stage", 0),
            ("nested/stage", 128),
        ] {
            let path = MPath::new(path)?;
            let mut entries = Vec::new();
            for byte in *b"amz" {
                for n in 0..size {
                    let name = if n == 0 {
                        char::from(byte).to_string()
                    } else {
                        format!("{}{n:03}", char::from(byte))
                    };
                    let file = TestShardedManifestFile {
                        basename_length: name.len() as u64,
                    };
                    entries.push((name, TestShardedManifestEntry::File(file)));
                }
            }
            let canonical = TestShardedManifest {
                subentries: ShardedMapV2Node::from_entries(&ctx, &store, entries.clone()).await?,
            };
            let expected = (size != 0 || path.is_root()).then(|| TestShardedManifestDirectory {
                max_basename_length: canonical.subentries.rollup_data(),
                id: *canonical.into_blob().id(),
            });
            let mut partitions = Vec::new();
            for range in crate::partition_ranges(b"dgjnqtw") {
                let entries = entries
                    .iter()
                    .filter(|(name, _)| {
                        range.contains(&MPathElement::new_from_slice(name.as_bytes()).unwrap())
                    })
                    .cloned();
                let map = LoadableShardedMapV2Node::Inlined(
                    ShardedMapV2Node::from_entries(&ctx, &store, entries).await?,
                );
                partitions.push(map.stored(&ctx, &store).await?);
            }
            let result = ManifestDerivationInput::Entry {
                path: path.clone(),
                parents: vec![],
                known_entries: HashMap::from([(
                    path.clone(),
                    KnownEntry::Children(partitions.into()),
                )]),
            }
            .derive(
                ctx.clone(),
                store.clone(),
                entries
                    .iter()
                    .map(|(name, _)| {
                        let name = MPathElement::new_from_slice(name.as_bytes())?;
                        Ok((path.join_element(Some(&name)).try_into()?, Some(())))
                    })
                    .collect::<Result<_>>()?,
                {
                    let ctx = ctx.clone();
                    let store = store.clone();
                    move |info: TreeInfo<TestShardedManifestDirectory, (), (), _>| {
                        let ctx = ctx.clone();
                        let store = store.clone();
                        async move {
                            let subentries = info
                                .subentries
                                .into_iter()
                                .map(|(prefix, entry)| {
                                    let entry = entry.map_left(|(_, entry)| match entry {
                                        Entry::Tree(dir) => {
                                            TestShardedManifestEntry::Directory(dir)
                                        }
                                        Entry::Leaf(()) => TestShardedManifestEntry::File(
                                            TestShardedManifestFile {
                                                basename_length: prefix.len() as u64,
                                            },
                                        ),
                                    });
                                    (prefix, entry)
                                })
                                .collect();
                            let subentries = ShardedMapV2Node::from_entries_and_partial_maps(
                                &ctx, &store, subentries,
                            )
                            .await?;
                            let max_basename_length = subentries.rollup_data();
                            let blob = TestShardedManifest { subentries }.into_blob();
                            Ok((
                                (),
                                TestShardedManifestDirectory {
                                    id: *blob.id(),
                                    max_basename_length,
                                },
                            ))
                        }
                    }
                },
                |_| async { anyhow::bail!("assembly must not create leaves") },
                |_| async { anyhow::bail!("assembly must use the tree callback") },
            )
            .await?
            .into_entry()?;
            assert_eq!(result, expected.map(Entry::Tree), "size {size}");

            if path.is_root() {
                continue;
            }

            // Complete empty children delete the directory without reading its
            // parent or invoking any derivation callbacks.
            let deleted = ManifestDerivationInput::Entry {
                path: path.clone(),
                parents: vec![result],
                known_entries: HashMap::from([(
                    path.clone(),
                    KnownEntry::Children(Vec::new().into()),
                )]),
            }
            .derive(
                ctx.clone(),
                store.clone(),
                entries
                    .iter()
                    .map(|(name, _)| {
                        let name = MPathElement::new_from_slice(name.as_bytes())?;
                        Ok((path.join_element(Some(&name)).try_into()?, None::<()>))
                    })
                    .collect::<Result<_>>()?,
                |_| async {
                    Err::<((), TestShardedManifestDirectory), _>(anyhow!(
                        "empty children must not create a tree"
                    ))
                },
                |_| async { anyhow::bail!("known children must not create leaves") },
                |_| async { anyhow::bail!("known children must not derive partitions") },
            )
            .await?
            .into_entry()?;
            assert_eq!(deleted, None);
        }
        Ok(())
    }
    #[mononoke::fbinit_test]
    async fn flat_snapshots_encode_store_and_load_once(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let counters = Arc::new(BlobstoreCounters::default());
        let store = RepoBlobstore::new(
            Arc::new(CountingBlobstore::new(Memblob::default(), counters.clone())),
            None,
            RepositoryId::new(0),
            MononokeScubaSampleBuilder::with_discard(),
        );
        let map =
            SortedVectorTrieMap::new([(b"alpha".as_slice().into(), 1u8)].into_iter().collect());
        let output: PartitionOutput<_> = map.clone().into();
        let encode = |value| Ok(BlobstoreBytes::from_bytes(vec![value]));
        let reference = store_sorted_subentries(&ctx, &store, output.clone(), encode).await?;
        assert_eq!(reference.len(), 32);
        for _ in 0..100 {
            assert_eq!(
                store_sorted_subentries(&ctx, &store, output.clone(), |_| panic!(
                    "reused snapshot was encoded again"
                ))
                .await?,
                reference
            );
        }
        assert_eq!(counters.snapshot().puts, 1);

        let inputs = Arc::new(crate::StageInputs::default());
        let decode = |bytes: BlobstoreBytes| Ok(bytes.as_bytes()[0]);
        let first = decode_partition_reference(
            &ctx,
            store.clone(),
            inputs.clone(),
            reference.clone(),
            decode,
        )?;
        let second =
            decode_partition_reference(&ctx, store.clone(), inputs, reference.clone(), decode)?;
        assert_eq!(counters.snapshot().gets, 0);
        let (first_map, second_map) = futures::try_join!(first.clone().load(), second.load())?;
        assert_eq!(first_map, map);
        assert!(Arc::ptr_eq(&first_map.entries, &second_map.entries));
        assert_eq!(counters.snapshot().gets, 1);
        assert_eq!(
            store_sorted_subentries(&ctx, &store, first, |_| panic!(
                "loaded snapshot was encoded again"
            ))
            .await?,
            reference
        );
        assert_eq!(counters.snapshot().puts, 1);

        let changed =
            SortedVectorTrieMap::new([(b"alpha".as_slice().into(), 2u8)].into_iter().collect());
        assert_ne!(
            store_sorted_subentries(&ctx, &store, changed.into(), encode).await?,
            reference
        );
        assert_eq!(counters.snapshot().puts, 2);
        Ok(())
    }
    #[mononoke::fbinit_test]
    async fn unchanged_partition_keeps_its_snapshot(fb: FacebookInit) -> Result<()> {
        use mononoke_types::FsnodeId;
        use mononoke_types::fsnode::FsnodeFile;

        let ctx = CoreContext::test_mock(fb);
        let counters = Arc::new(BlobstoreCounters::default());
        let store = RepoBlobstore::new(
            Arc::new(CountingBlobstore::new(Memblob::default(), counters.clone())),
            None,
            RepositoryId::new(0),
            MononokeScubaSampleBuilder::with_discard(),
        );
        let map: SortedVectorTrieMap<Entry<FsnodeId, FsnodeFile>> = SortedVectorTrieMap::new(
            [(
                b"alpha".as_slice().into(),
                Entry::Tree(FsnodeId::new(Blake2::from_byte_array([1; 32]))),
            )]
            .into_iter()
            .collect(),
        );
        let parent: PartitionOutput<_> = map.into();
        let reference =
            store_sorted_subentries(
                &ctx,
                &store,
                parent.clone(),
                |_| Ok(BlobstoreBytes::empty()),
            )
            .await?;
        let parent = decode_partition_reference(
            &ctx,
            store.clone(),
            Arc::new(crate::StageInputs::default()),
            reference.clone(),
            |_| -> Result<Entry<FsnodeId, FsnodeFile>> { panic!("unchanged parent was decoded") },
        )?;
        let input = ManifestDerivationInput::Partition {
            path: MPath::new("dir")?,
            range: ChildNameRange::new(0, b'm')?,
            parents: vec![parent.clone()],
        };
        let output = input
            .derive(
                ctx.clone(),
                store.clone(),
                vec![(NonRootMPath::new("elsewhere")?, Some(1u8))],
                |_| async { Err::<((), FsnodeId), _>(anyhow!("unexpected tree derivation")) },
                |_| async { Err::<((), FsnodeFile), _>(anyhow!("unexpected leaf derivation")) },
                |children| create_sorted_subentries(&ctx, children),
            )
            .await?
            .into_partition()?;
        assert!(Arc::ptr_eq(&parent.encoded, &output.encoded));
        assert_eq!(
            store_sorted_subentries(&ctx, &store, output, |_| panic!(
                "unchanged partition was encoded"
            ))
            .await?,
            reference
        );
        assert_eq!(counters.snapshot().puts, 1);
        assert_eq!(counters.snapshot().gets, 0);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn assembly_loads_children_only_when_needed(fb: FacebookInit) -> Result<()> {
        use mononoke_types::FsnodeId;
        use mononoke_types::fsnode::FsnodeFile;

        let ctx = CoreContext::test_mock(fb);
        let counters = Arc::new(BlobstoreCounters::default());
        let store = RepoBlobstore::new(
            Arc::new(CountingBlobstore::new(Memblob::default(), counters.clone())),
            None,
            RepositoryId::new(0),
            MononokeScubaSampleBuilder::with_discard(),
        );
        let path = MPath::new("dir")?;
        let parent = FsnodeId::new(Blake2::from_byte_array([2; 32]));
        let missing = decode_partition_reference(
            &ctx,
            store.clone(),
            Arc::new(crate::StageInputs::default()),
            BlobstoreBytes::from_bytes(Blake2::from_byte_array([1; 32]).as_ref().to_vec()),
            |_| -> Result<Entry<FsnodeId, FsnodeFile>> { unreachable!() },
        )?;
        for changed in [false, true] {
            let input = ManifestDerivationInput::from_stage_outputs(
                &StageId::Manifest(path.clone()),
                vec![ManifestStageOutput::Entry(Some(Entry::Tree(
                    parent.clone(),
                )))],
                HashMap::from([(
                    StageId::Subentries(path.clone(), ChildNameRange::new(0, 255)?),
                    ManifestStageOutput::Subentries(missing.clone()),
                )]),
            )?;
            let changes = if changed {
                vec![(NonRootMPath::new("dir/alpha")?, Some(()))]
            } else {
                vec![]
            };
            let result = input
                .derive(
                    ctx.clone(),
                    store.clone(),
                    changes,
                    |_| async { Err::<((), FsnodeId), _>(anyhow!("unexpected tree creation")) },
                    |_| async { anyhow::bail!("unexpected leaf creation") },
                    |children| create_sorted_subentries(&ctx, children),
                )
                .await;
            if changed {
                assert!(
                    format!("{:#}", result.unwrap_err()).contains("missing partition snapshot")
                );
                assert_eq!(counters.snapshot().gets, 1);
            } else {
                assert_eq!(result?.into_entry()?, Some(Entry::Tree(parent.clone())));
                assert_eq!(counters.snapshot().gets, 0);
            }
        }
        Ok(())
    }
}
