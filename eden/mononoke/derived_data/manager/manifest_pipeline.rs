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
use manifest::Manifest;
use manifest::ManifestOps;
use manifest::StoreLoadable;
use manifest::TreeInfoSubentries;
use manifest::TrieMapOps;
use mononoke_types::BlobstoreBytes;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::MPath;
use mononoke_types::PipelineDerivableVariant;

use crate::BonsaiDerivable;
use crate::DerivationContext;
use crate::DerivationStagePayload;
use crate::ManifestDerivationInput;
use crate::ManifestStageOutput;
use crate::PipelineDerivable;
use crate::StageId;
use crate::StageOutputs;
use crate::manifest_partitions::PartitionMap;
use crate::manifest_partitions::select_subentries;
use crate::stage_outputs::fetch_stage_outputs;
use crate::stage_outputs::store_stage_outputs;

type Store = Arc<dyn KeyedBlobstore>;

/// Type-specific preparation and encoding for a manifest pipeline.
#[async_trait]
pub trait ManifestDerivable: BonsaiDerivable {
    type TreeId: Clone + Debug + Eq + Send + Sync + 'static;
    type Leaf: Clone + Debug + Eq + Send + Sync + 'static;
    type TrieMapType: TrieMapOps<Store, Entry<Self::TreeId, Self::Leaf>>
        + PartitionMap<Self>
        + Clone
        + Debug
        + Eq
        + Send
        + Sync
        + 'static;

    const PIPELINE_VARIANT: PipelineDerivableVariant;

    /// Preparation depends on complete roots of this type's dependencies.
    const HAS_PREPARE: bool = false;

    async fn prepare_batch(
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
        _bonsais: Vec<BonsaiChangeset>,
        _stages: &[crate::DerivationStagePayload],
    ) -> Result<Vec<crate::PreparedStageInputs>> {
        anyhow::bail!("{} does not have a preparation step", Self::NAME)
    }

    /// An injective, prefix-preserving translation that keeps the root unchanged.
    /// None excludes a path and its descendants.
    /// Partition ranges select the first byte of names in the translated manifest.
    fn manifest_path(path: &MPath) -> Option<MPath> {
        Some(path.clone())
    }

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
        input: ManifestDerivationInput<Self::TreeId, Self::Leaf, Self::TrieMapType>,
    ) -> Result<ManifestStageOutput<Self::TreeId, Self::Leaf, Self::TrieMapType>>;

    /// Derive in parent order, optionally preparing independent inputs for the whole batch.
    async fn derive_manifest_batch(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsais: Vec<BonsaiChangeset>,
        stage: &StageId,
        parents: HashMap<
            ChangesetId,
            ManifestStageOutput<Self::TreeId, Self::Leaf, Self::TrieMapType>,
        >,
        dependency_outputs: HashMap<
            ChangesetId,
            HashMap<StageId, ManifestStageOutput<Self::TreeId, Self::Leaf, Self::TrieMapType>>,
        >,
    ) -> Result<
        HashMap<ChangesetId, ManifestStageOutput<Self::TreeId, Self::Leaf, Self::TrieMapType>>,
    > {
        derive_manifest_batch(
            bonsais,
            parents,
            dependency_outputs,
            |bonsai, parents, dependencies| {
                let input =
                    ManifestDerivationInput::from_stage_outputs(stage, parents, dependencies);
                async move {
                    match input?.map_paths(Self::manifest_path) {
                        Some(input) => Self::derive_manifest(ctx, derivation, bonsai, input).await,
                        None => {
                            if matches!(stage, StageId::Subentries(..)) {
                                Ok(ManifestStageOutput::Subentries(
                                    Self::create_subentries(ctx, derivation, Default::default())
                                        .await?
                                        .into(),
                                ))
                            } else {
                                Ok(ManifestStageOutput::Entry(None))
                            }
                        }
                    }
                }
            },
        )
        .await
    }

    async fn create_subentries(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        subentries: TreeInfoSubentries<Self::TreeId, Self::Leaf, (), Self::TrieMapType>,
    ) -> Result<Self::TrieMapType>;

    fn encode_entry(entry: Option<Entry<Self::TreeId, Self::Leaf>>) -> BlobstoreBytes;
    fn decode_entry(bytes: BlobstoreBytes) -> Result<Option<Entry<Self::TreeId, Self::Leaf>>>;
}

#[async_trait]
impl<T, M> PipelineDerivable for T
where
    T: ManifestDerivable,
    T::TreeId: StoreLoadable<Store, Value = M> + Unpin,
    M: Manifest<Store, TreeId = T::TreeId, Leaf = T::Leaf, TrieMapType = T::TrieMapType>
        + Send
        + Sync,
    T::Leaf: Unpin,
{
    const PIPELINE_DERIVABLE_VARIANT: PipelineDerivableVariant = T::PIPELINE_VARIANT;
    const HAS_FINALIZE: bool = false;
    const HAS_PREPARE: bool = T::HAS_PREPARE;

    async fn prepare_batch(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsais: Vec<BonsaiChangeset>,
        stages: &[DerivationStagePayload],
    ) -> Result<Vec<crate::PreparedStageInputs>> {
        T::prepare_batch(ctx, derivation, bonsais, stages).await
    }

    type StageOutput = ManifestStageOutput<T::TreeId, T::Leaf, T::TrieMapType>;

    async fn derive_stage_batch(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsais: Vec<BonsaiChangeset>,
        payload: &DerivationStagePayload,
        parents: HashMap<ChangesetId, Self::StageOutput>,
        dependency_outputs: HashMap<ChangesetId, HashMap<StageId, Self::StageOutput>>,
    ) -> Result<HashMap<ChangesetId, Self::StageOutput>> {
        T::derive_manifest_batch(
            ctx,
            derivation,
            bonsais,
            &payload.stage_id(),
            parents,
            dependency_outputs,
        )
        .await
    }

    async fn extract_stage_output_from_derived(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        derived: &Self,
        stage: &StageId,
    ) -> Result<Self::StageOutput> {
        let root = derived.load_root_manifest(ctx, derivation).await?;
        Self::extract_stage_output_from_root(
            ctx,
            derivation,
            ManifestStageOutput::Entry(Some(Entry::Tree(root))),
            stage,
        )
        .await
    }

    async fn extract_stage_output_from_root(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        root: Self::StageOutput,
        stage: &StageId,
    ) -> Result<Self::StageOutput> {
        let path = stage_path::<T>(stage)?;
        let blobstore: Store = derivation.blobstore().clone();
        let Some(Entry::Tree(root)) = root.into_entry()? else {
            anyhow::bail!("completed manifest root must be a tree");
        };
        let entry = match T::manifest_path(path) {
            Some(path) => root.find_entry(ctx.clone(), blobstore, path).await?,
            None => None,
        };
        if let StageId::Subentries(_, range) = stage {
            let subentries = select_subentries(ctx, derivation.blobstore(), entry, *range).await?;
            let entries = T::create_subentries(ctx, derivation, subentries).await?;
            Ok(ManifestStageOutput::Subentries(entries.into()))
        } else {
            Ok(ManifestStageOutput::Entry(entry))
        }
    }

    async fn store_stage_outputs(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        stage: &StageId,
        outputs: HashMap<ChangesetId, Self::StageOutput>,
    ) -> Result<Option<StageOutputs>> {
        let path = stage_path::<T>(stage)?;
        let canonical =
            matches!(stage, StageId::Manifest(_)) && use_normal_mapping(derivation, path);
        if !canonical {
            return store_stage_outputs::<Self, _, _>(
                ctx,
                derivation,
                stage,
                outputs,
                |_| None,
                |output| encode_output::<T>(ctx, derivation, output),
            )
            .await;
        }
        let derived = stream::iter(outputs)
            .map(|(csid, entry)| async move {
                let Some(Entry::Tree(root)) = entry.into_entry()? else {
                    anyhow::bail!("terminal stage output for {csid} should be a tree");
                };
                Ok((csid, T::from_root_manifest(ctx, derivation, root).await?))
            })
            .buffered(100)
            .try_collect::<Vec<_>>()
            .await?;
        derivation.flush(ctx).await?;
        stream::iter(derived)
            .map(
                |(csid, derived)| async move { derived.store_mapping(ctx, derivation, csid).await },
            )
            .buffered(100)
            .try_collect::<()>()
            .await?;
        Ok(None)
    }

    async fn fetch_stage_outputs(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        stage: &StageId,
        csids: Vec<ChangesetId>,
    ) -> Result<HashMap<ChangesetId, Self::StageOutput>> {
        let path = stage_path::<T>(stage)?;
        let canonical =
            matches!(stage, StageId::Manifest(_)) && use_normal_mapping(derivation, path);
        if !canonical {
            return fetch_stage_outputs::<Self, _, _>(
                ctx,
                derivation,
                stage,
                csids,
                |_| None,
                |_, bytes| decode_output::<T>(ctx, derivation, stage, bytes),
            )
            .await;
        }
        stream::iter(csids)
            .map(|csid| async move {
                let entry = match T::fetch(ctx, derivation, csid).await? {
                    Some(root) => Some(ManifestStageOutput::Entry(Some(Entry::Tree(
                        root.load_root_manifest(ctx, derivation).await?,
                    )))),
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
    mut dependency_outputs: HashMap<ChangesetId, HashMap<StageId, Output>>,
    mut derive: F,
) -> Result<HashMap<ChangesetId, Output>>
where
    Output: Clone,
    F: FnMut(BonsaiChangeset, Vec<Output>, HashMap<StageId, Output>) -> Fut,
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

fn stage_path<T: ManifestDerivable>(stage: &StageId) -> Result<&MPath> {
    match stage {
        StageId::Manifest(path) | StageId::Subentries(path, _) => Ok(path),
        StageId::Finalize => anyhow::bail!("{} has no finalize stage", T::NAME),
    }
}

fn use_normal_mapping(derivation: &DerivationContext, path: &MPath) -> bool {
    path.is_root() && derivation.publish_pipeline_outputs()
}

async fn encode_output<T: ManifestDerivable>(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    output: ManifestStageOutput<T::TreeId, T::Leaf, T::TrieMapType>,
) -> Result<BlobstoreBytes> {
    match output {
        ManifestStageOutput::Entry(entry) => Ok(T::encode_entry(entry)),
        ManifestStageOutput::Subentries(map) => {
            <T::TrieMapType as PartitionMap<T>>::encode(ctx, derivation, map).await
        }
    }
}

async fn decode_output<T: ManifestDerivable>(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    stage: &StageId,
    bytes: BlobstoreBytes,
) -> Result<ManifestStageOutput<T::TreeId, T::Leaf, T::TrieMapType>> {
    if matches!(stage, StageId::Subentries(..)) {
        Ok(ManifestStageOutput::Subentries(
            <T::TrieMapType as PartitionMap<T>>::decode(ctx, derivation, bytes)?,
        ))
    } else {
        Ok(ManifestStageOutput::Entry(T::decode_entry(bytes)?))
    }
}
