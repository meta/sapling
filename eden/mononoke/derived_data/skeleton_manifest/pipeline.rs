/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use context::CoreContext;
use derived_data_manager::DerivationContext;
use derived_data_manager::ManifestDerivable;
use derived_data_manager::ManifestDerivationInput;
use derived_data_manager::ManifestStageOutput;
use derived_data_manager::manifest_partitions::create_sorted_subentries;
use fbthrift::compact_protocol;
use manifest::Entry;
use manifest::TreeInfoSubentries;
use mononoke_types::BlobstoreBytes;
use mononoke_types::BonsaiChangeset;
use mononoke_types::PipelineDerivableVariant;
use mononoke_types::SkeletonManifestId;
use mononoke_types::SortedVectorTrieMap;
use mononoke_types::thrift::skeleton_manifest as skmf_thrift;

use crate::derive::derive_skeleton_manifest_entry;
use crate::mapping::RootSkeletonManifestId;
use crate::mapping::get_file_changes;

#[async_trait]
impl ManifestDerivable for RootSkeletonManifestId {
    type TreeId = SkeletonManifestId;
    type Leaf = ();
    type TrieMapType = SortedVectorTrieMap<Entry<Self::TreeId, Self::Leaf>>;

    const PIPELINE_VARIANT: PipelineDerivableVariant = PipelineDerivableVariant::SkeletonManifests;

    async fn load_root_manifest(
        &self,
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
    ) -> Result<SkeletonManifestId> {
        Ok(self.0)
    }

    async fn from_root_manifest(
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
        root: SkeletonManifestId,
    ) -> Result<Self> {
        Ok(Self(root))
    }

    async fn derive_manifest(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsai: BonsaiChangeset,
        input: ManifestDerivationInput<Self::TreeId, Self::Leaf, Self::TrieMapType>,
    ) -> Result<ManifestStageOutput<Self::TreeId, Self::Leaf, Self::TrieMapType>> {
        derive_skeleton_manifest_entry(ctx, derivation, input, get_file_changes(&bonsai)).await
    }

    async fn create_subentries(
        ctx: &CoreContext,
        _derivation: &DerivationContext,
        subentries: TreeInfoSubentries<Self::TreeId, Self::Leaf, (), Self::TrieMapType>,
    ) -> Result<Self::TrieMapType> {
        create_sorted_subentries(ctx, subentries).await
    }

    fn encode_entry(entry: Option<Entry<SkeletonManifestId, ()>>) -> BlobstoreBytes {
        let entry = match entry {
            Some(Entry::Tree(id)) => {
                skmf_thrift::SkeletonManifestStageOutput::skeleton_manifest_id(id.into_thrift())
            }
            Some(Entry::Leaf(_)) => skmf_thrift::SkeletonManifestStageOutput::file(
                skmf_thrift::SkeletonManifestStageOutputFile {
                    ..Default::default()
                },
            ),
            None => skmf_thrift::SkeletonManifestStageOutput::empty(
                skmf_thrift::SkeletonManifestStageOutputEmpty {
                    ..Default::default()
                },
            ),
        };
        BlobstoreBytes::from_bytes(compact_protocol::serialize(&entry))
    }

    fn decode_entry(bytes: BlobstoreBytes) -> Result<Option<Entry<SkeletonManifestId, ()>>> {
        let entry: skmf_thrift::SkeletonManifestStageOutput =
            compact_protocol::deserialize(bytes.into_bytes())?;
        Ok(match entry {
            skmf_thrift::SkeletonManifestStageOutput::skeleton_manifest_id(id) => {
                Some(Entry::Tree(SkeletonManifestId::from_thrift(id)?))
            }
            skmf_thrift::SkeletonManifestStageOutput::file(_) => Some(Entry::Leaf(())),
            skmf_thrift::SkeletonManifestStageOutput::empty(_) => None,
            skmf_thrift::SkeletonManifestStageOutput::UnknownField(field) => {
                return Err(anyhow!(
                    "unknown SkeletonManifestStageOutput variant {field}"
                ));
            }
        })
    }
}
