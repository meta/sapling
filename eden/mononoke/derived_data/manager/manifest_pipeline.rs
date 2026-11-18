/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Pipeline execution for types derived through the manifest crate.

use std::collections::HashMap;
use std::fmt::Debug;
use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use blobstore::KeyedBlobstore;
use context::CoreContext;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use manifest::Entry;
use manifest::KnownEntry;
use manifest::LeafInfo;
use manifest::Manifest;
use manifest::ManifestOps;
use manifest::StoreLoadable;
use manifest::TreeInfo;
use manifest::TrieMapOps;
use manifest::derive_manifest_with_known_entries;
use mononoke_types::BlobstoreBytes;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::MPath;
use mononoke_types::NonRootMPath;
use mononoke_types::PipelineDerivableVariant;

use crate::BonsaiDerivable;
use crate::DerivationContext;
use crate::DerivationStagePayload;
use crate::PipelineDerivable;
use crate::StageId;

type Store = Arc<dyn KeyedBlobstore>;

/// The manifest inputs for one changeset at a stage boundary.
pub struct ManifestDerivation<TreeId, Leaf> {
    pub parents: Vec<Entry<TreeId, Leaf>>,
    pub known_entries: HashMap<MPath, Option<Entry<TreeId, Leaf>>>,
    pub path: MPath,
}

impl<TreeId, Leaf> ManifestDerivation<TreeId, Leaf> {
    /// Whether a change needs preparation by this stage's leaf callback.
    pub fn contains_change(&self, path: &NonRootMPath) -> bool {
        self.path.is_prefix_of(path)
            && !self
                .known_entries
                .keys()
                .any(|known| known.is_prefix_of(path))
    }

    /// Derive the supplied inputs using the manifest's existing callbacks.
    pub async fn derive<S, Change, Ctx, T, TFut, L, LFut>(
        self,
        ctx: CoreContext,
        store: S,
        changes: Vec<(NonRootMPath, Option<Change>)>,
        create_tree: T,
        create_leaf: L,
    ) -> Result<Option<Entry<TreeId, Leaf>>>
    where
        S: KeyedBlobstore + Clone + Send + Sync + 'static,
        Change: Send + Clone + Eq + Hash + Debug + 'static,
        Leaf: Send + Clone + Eq + Hash + Debug + 'static,
        TreeId: StoreLoadable<S> + Clone + Eq + Hash + Debug + Send + Sync + 'static,
        TreeId::Value: Manifest<S, TreeId = TreeId, Leaf = Leaf> + Send + Sync,
        <TreeId::Value as Manifest<S>>::TrieMapType:
            TrieMapOps<S, Entry<TreeId, Leaf>> + Send + Sync + 'static,
        T: Fn(TreeInfo<TreeId, Leaf, Ctx, <TreeId::Value as Manifest<S>>::TrieMapType>) -> TFut
            + Send
            + Sync
            + 'static,
        TFut: Future<Output = Result<(Ctx, TreeId)>> + Send + 'static,
        L: Fn(LeafInfo<Leaf, Change>) -> LFut + Send + Sync + 'static,
        LFut: Future<Output = Result<(Ctx, Leaf)>> + Send + 'static,
        Ctx: Send + 'static,
    {
        derive_manifest_with_known_entries(
            ctx,
            store,
            self.parents,
            changes,
            self.known_entries
                .into_iter()
                .map(|(path, entry)| (path, KnownEntry::Entry(entry)))
                .collect(),
            self.path,
            create_tree,
            create_leaf,
        )
        .await
    }
}

/// Type-specific preparation and encoding for a manifest pipeline.
#[async_trait]
pub trait ManifestDerivable: BonsaiDerivable {
    type TreeId: Clone + Debug + Eq + Send + Sync + 'static;
    type Leaf: Clone + Debug + Eq + Send + Sync + 'static;

    const PIPELINE_VARIANT: PipelineDerivableVariant;
    const STAGE_KEY_PREFIX: &'static str;
    const MAPPING_KNOB_SWITCH: &'static str = Self::NAME;

    async fn load_root_manifest(
        &self,
        ctx: &CoreContext,
        derivation: &DerivationContext,
    ) -> Result<Self::TreeId>;
    async fn from_root_manifest(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        root: Self::TreeId,
    ) -> Result<Self>;

    async fn derive_manifest(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsai: BonsaiChangeset,
        input: ManifestDerivation<Self::TreeId, Self::Leaf>,
    ) -> Result<Option<Entry<Self::TreeId, Self::Leaf>>>;

    fn encode_entry(entry: Option<Entry<Self::TreeId, Self::Leaf>>) -> BlobstoreBytes;
    fn decode_entry(bytes: BlobstoreBytes) -> Result<Option<Entry<Self::TreeId, Self::Leaf>>>;
}

#[async_trait]
impl<T> PipelineDerivable for T
where
    T: ManifestDerivable,
    T::TreeId: StoreLoadable<Store> + Unpin,
    <T::TreeId as StoreLoadable<Store>>::Value:
        Manifest<Store, TreeId = T::TreeId, Leaf = T::Leaf> + Send + Sync,
    T::Leaf: Unpin,
{
    const PIPELINE_DERIVABLE_VARIANT: PipelineDerivableVariant = T::PIPELINE_VARIANT;
    const HAS_FINALIZE: bool = false;

    type StageOutput = Option<Entry<T::TreeId, T::Leaf>>;

    async fn derive_stage_batch(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsais: Vec<BonsaiChangeset>,
        payload: &DerivationStagePayload,
        parents: HashMap<ChangesetId, Self::StageOutput>,
        dependency_outputs: HashMap<ChangesetId, HashMap<MPath, Self::StageOutput>>,
    ) -> Result<HashMap<ChangesetId, Self::StageOutput>> {
        let DerivationStagePayload::Manifest(payload) = payload else {
            anyhow::bail!("{} has no finalize stage", T::NAME);
        };
        derive_manifest_batch(
            bonsais,
            parents,
            dependency_outputs,
            |bonsai, parents, known_entries| {
                let input = ManifestDerivation {
                    parents: parents.into_iter().flatten().collect(),
                    known_entries,
                    path: payload.path.clone(),
                };
                T::derive_manifest(ctx, derivation, bonsai, input)
            },
        )
        .await
    }

    async fn extract_stage_output_from_derived(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        derived: &Self,
        stage: &StageId,
    ) -> Result<Self::StageOutput> {
        let path = stage_path::<T>(stage)?;
        derived
            .load_root_manifest(ctx, derivation)
            .await?
            .find_entry(ctx.clone(), derivation.blobstore().clone(), path.clone())
            .await
    }

    async fn store_stage_outputs(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        stage: &StageId,
        outputs: HashMap<ChangesetId, Self::StageOutput>,
    ) -> Result<()> {
        let path = stage_path::<T>(stage)?;
        let canonical = use_normal_mapping::<T>(path);
        if !canonical {
            return store_stage_blobs(
                ctx,
                derivation,
                outputs,
                |csid| stage_key::<T>(derivation, path, csid),
                |entry| Ok(T::encode_entry(entry)),
            )
            .await;
        }
        stream::iter(outputs)
            .map(|(csid, entry)| async move {
                let Some(Entry::Tree(root)) = entry else {
                    anyhow::bail!(
                        "terminal stage output for {csid} should be a tree, got {entry:?}"
                    );
                };
                T::from_root_manifest(ctx, derivation, root)
                    .await?
                    .store_mapping(ctx, derivation, csid)
                    .await
            })
            .buffered(100)
            .try_collect::<Vec<_>>()
            .await?;
        Ok(())
    }

    async fn fetch_stage_outputs(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        stage: &StageId,
        csids: Vec<ChangesetId>,
    ) -> Result<HashMap<ChangesetId, Self::StageOutput>> {
        let path = stage_path::<T>(stage)?;
        let canonical = use_normal_mapping::<T>(path);
        if !canonical {
            return fetch_stage_blobs(
                ctx,
                derivation,
                csids,
                |csid| stage_key::<T>(derivation, path, csid),
                |_, bytes| T::decode_entry(bytes),
            )
            .await;
        }
        stream::iter(csids)
            .map(|csid| async move {
                let entry = match T::fetch(ctx, derivation, csid).await? {
                    Some(root) => Some(Some(Entry::Tree(
                        root.load_root_manifest(ctx, derivation).await?,
                    ))),
                    None => None,
                };
                Ok::<_, anyhow::Error>(entry.map(|entry| (csid, entry)))
            })
            .buffered(100)
            .try_filter_map(|entry| async move { Ok(entry) })
            .try_collect()
            .await
    }
}

/// Derive a batch in changeset order, preserving each parent's position.
pub async fn derive_manifest_batch<Output, F, Fut>(
    bonsais: Vec<BonsaiChangeset>,
    parents: HashMap<ChangesetId, Output>,
    mut dependency_outputs: HashMap<ChangesetId, HashMap<MPath, Output>>,
    mut derive: F,
) -> Result<HashMap<ChangesetId, Output>>
where
    Output: Clone,
    F: FnMut(BonsaiChangeset, Vec<Output>, HashMap<MPath, Output>) -> Fut,
    Fut: Future<Output = Result<Output>>,
{
    let mut results = HashMap::new();
    for bonsai in bonsais {
        let csid = bonsai.get_changeset_id();
        let parents = bonsai
            .parents()
            .map(|parent| {
                results
                    .get(&parent)
                    .or_else(|| parents.get(&parent))
                    .cloned()
                    .ok_or_else(|| anyhow!("missing stage output for parent {parent} of {csid}"))
            })
            .collect::<Result<Vec<_>>>()?;
        let dependencies = dependency_outputs.remove(&csid).unwrap_or_default();
        results.insert(csid, derive(bonsai, parents, dependencies).await?);
    }
    Ok(results)
}

/// Store stage outputs using the type's existing key and encoding.
pub async fn store_stage_blobs<Output>(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    outputs: HashMap<ChangesetId, Output>,
    key: impl Fn(ChangesetId) -> String,
    encode: impl Fn(Output) -> Result<BlobstoreBytes>,
) -> Result<()> {
    stream::iter(outputs)
        .map(|(csid, output)| {
            let key = key(csid);
            let bytes = encode(output);
            async move { derivation.blobstore().put(ctx, key, bytes?).await }
        })
        .buffer_unordered(100)
        .try_collect::<Vec<_>>()
        .await?;
    Ok(())
}

/// Missing blobs are omitted; encoded empty outputs remain present in the map.
pub async fn fetch_stage_blobs<Output>(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    csids: Vec<ChangesetId>,
    key: impl Fn(ChangesetId) -> String,
    decode: impl Fn(ChangesetId, BlobstoreBytes) -> Result<Output>,
) -> Result<HashMap<ChangesetId, Output>> {
    stream::iter(csids)
        .map(|csid| {
            let key = key(csid);
            let decode = &decode;
            async move {
                derivation
                    .blobstore()
                    .get(ctx, &key)
                    .await?
                    .map(|bytes| decode(csid, bytes.into()).map(|output| (csid, output)))
                    .transpose()
            }
        })
        .buffer_unordered(100)
        .try_filter_map(|entry| async move { Ok(entry) })
        .try_collect()
        .await
}

fn stage_path<T: ManifestDerivable>(stage: &StageId) -> Result<&MPath> {
    match stage {
        StageId::Manifest(path) => Ok(path),
        StageId::Finalize => anyhow::bail!("{} has no finalize stage", T::NAME),
    }
}

fn use_normal_mapping<T: ManifestDerivable>(path: &MPath) -> bool {
    path.is_root()
        && justknobs::eval(
            "scm/mononoke:derived_data_pipeline_terminal_stage_prod_mapping",
            None,
            Some(T::MAPPING_KNOB_SWITCH),
        )
}

fn stage_key<T: ManifestDerivable>(
    derivation: &DerivationContext,
    path: &MPath,
    csid: ChangesetId,
) -> String {
    format!(
        "{}.{}.{}{}",
        T::STAGE_KEY_PREFIX,
        path.get_path_hash().to_hex(),
        derivation.mapping_key_prefix::<T>(),
        csid,
    )
}
