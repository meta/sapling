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
use mononoke_types::FsnodeId;
use mononoke_types::PipelineDerivableVariant;
use mononoke_types::SortedVectorTrieMap;
use mononoke_types::fsnode::FsnodeFile;
use mononoke_types::thrift::fsnodes as fsnodes_thrift;

use crate::derive::derive_fsnode_entry;
use crate::mapping::RootFsnodeId;
use crate::mapping::get_file_changes;

#[async_trait]
impl ManifestDerivable for RootFsnodeId {
    type TreeId = FsnodeId;
    type Leaf = FsnodeFile;
    type TrieMapType = SortedVectorTrieMap<Entry<Self::TreeId, Self::Leaf>>;

    const PIPELINE_VARIANT: PipelineDerivableVariant = PipelineDerivableVariant::Fsnodes;

    async fn load_root_manifest(
        &self,
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
    ) -> Result<FsnodeId> {
        Ok(self.0)
    }

    async fn from_root_manifest(
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
        root: FsnodeId,
    ) -> Result<Self> {
        Ok(Self(root))
    }

    async fn derive_manifest(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsai: BonsaiChangeset,
        input: ManifestDerivationInput<FsnodeId, FsnodeFile, Self::TrieMapType>,
    ) -> Result<ManifestStageOutput<FsnodeId, FsnodeFile, Self::TrieMapType>> {
        derive_fsnode_entry(ctx, derivation, input, get_file_changes(&bonsai)).await
    }

    async fn create_subentries(
        ctx: &CoreContext,
        _derivation: &DerivationContext,
        subentries: TreeInfoSubentries<FsnodeId, FsnodeFile, (), Self::TrieMapType>,
    ) -> Result<Self::TrieMapType> {
        create_sorted_subentries(ctx, subentries).await
    }

    fn encode_entry(entry: Option<Entry<FsnodeId, FsnodeFile>>) -> BlobstoreBytes {
        let entry = match entry {
            Some(Entry::Tree(id)) => fsnodes_thrift::FsnodeStageOutput::fsnode_id(id.into_thrift()),
            Some(Entry::Leaf(file)) => {
                fsnodes_thrift::FsnodeStageOutput::fsnode_file(file.into_thrift())
            }
            None => {
                fsnodes_thrift::FsnodeStageOutput::empty(fsnodes_thrift::FsnodeStageOutputEmpty {
                    ..Default::default()
                })
            }
        };
        BlobstoreBytes::from_bytes(compact_protocol::serialize(&entry))
    }

    fn decode_entry(bytes: BlobstoreBytes) -> Result<Option<Entry<FsnodeId, FsnodeFile>>> {
        let entry: fsnodes_thrift::FsnodeStageOutput =
            compact_protocol::deserialize(bytes.into_bytes())?;
        Ok(match entry {
            fsnodes_thrift::FsnodeStageOutput::fsnode_id(id) => {
                Some(Entry::Tree(FsnodeId::from_thrift(id)?))
            }
            fsnodes_thrift::FsnodeStageOutput::fsnode_file(file) => {
                Some(Entry::Leaf(FsnodeFile::from_thrift(file)?))
            }
            fsnodes_thrift::FsnodeStageOutput::empty(_) => None,
            fsnodes_thrift::FsnodeStageOutput::UnknownField(field) => {
                return Err(anyhow!("unknown FsnodeStageOutput variant {field}"));
            }
        })
    }
}
