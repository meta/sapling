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
use fbthrift::compact_protocol;
use manifest::Entry;
use manifest::TreeInfoSubentries;
use mononoke_types::BlobstoreBytes;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ContentManifestId;
use mononoke_types::PipelineDerivableVariant;
use mononoke_types::ThriftConvert;
use mononoke_types::content_manifest::ContentManifestEntry;
use mononoke_types::content_manifest::ContentManifestFile;
use mononoke_types::sharded_map_v2::LoadableShardedMapV2Node;
use mononoke_types::thrift::content_manifest as content_manifest_thrift;

use crate::RootContentManifestId;
use crate::derive::create_content_manifest_subentries;
use crate::derive::derive_content_manifest_entry;
use crate::derive::get_changes;

#[async_trait]
impl ManifestDerivable for RootContentManifestId {
    type TreeId = ContentManifestId;
    type Leaf = ContentManifestFile;
    type TrieMapType = LoadableShardedMapV2Node<ContentManifestEntry>;

    const PIPELINE_VARIANT: PipelineDerivableVariant = PipelineDerivableVariant::ContentManifests;

    async fn load_root_manifest(
        &self,
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
    ) -> Result<ContentManifestId> {
        Ok(self.0)
    }

    async fn from_root_manifest(
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
        root: ContentManifestId,
    ) -> Result<Self> {
        Ok(Self(root))
    }

    async fn derive_manifest(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsai: BonsaiChangeset,
        input: ManifestDerivationInput<Self::TreeId, Self::Leaf, Self::TrieMapType>,
    ) -> Result<ManifestStageOutput<Self::TreeId, Self::Leaf, Self::TrieMapType>> {
        derive_content_manifest_entry(ctx, derivation, input, get_changes(&bonsai)).await
    }

    async fn create_subentries(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        subentries: TreeInfoSubentries<Self::TreeId, Self::Leaf, (), Self::TrieMapType>,
    ) -> Result<Self::TrieMapType> {
        let subentries = subentries
            .into_iter()
            .map(|(prefix, entry)| (prefix, entry.map_left(|(_, entry)| (None, entry))))
            .collect();
        Ok(LoadableShardedMapV2Node::Inlined(
            create_content_manifest_subentries(
                ctx.clone(),
                derivation.blobstore().clone(),
                subentries,
            )
            .await?,
        ))
    }

    fn encode_entry(
        entry: Option<Entry<ContentManifestId, ContentManifestFile>>,
    ) -> BlobstoreBytes {
        let entry = match entry {
            Some(Entry::Tree(content_manifest_id)) => {
                content_manifest_thrift::ContentManifestStageOutput::content_manifest_id(
                    content_manifest_id.into_thrift(),
                )
            }
            Some(Entry::Leaf(content_manifest_file)) => {
                content_manifest_thrift::ContentManifestStageOutput::content_manifest_file(
                    content_manifest_file.into_thrift(),
                )
            }
            None => content_manifest_thrift::ContentManifestStageOutput::empty(
                content_manifest_thrift::ContentManifestStageOutputEmpty {
                    ..Default::default()
                },
            ),
        };
        BlobstoreBytes::from_bytes(compact_protocol::serialize(&entry))
    }

    fn decode_entry(
        bytes: BlobstoreBytes,
    ) -> Result<Option<Entry<ContentManifestId, ContentManifestFile>>> {
        let thrift_output: content_manifest_thrift::ContentManifestStageOutput =
            compact_protocol::deserialize(bytes.into_bytes())?;
        Ok(match thrift_output {
            content_manifest_thrift::ContentManifestStageOutput::content_manifest_id(id) => {
                Some(Entry::Tree(ContentManifestId::from_thrift(id)?))
            }
            content_manifest_thrift::ContentManifestStageOutput::content_manifest_file(file) => {
                Some(Entry::Leaf(ContentManifestFile::from_thrift(file)?))
            }
            content_manifest_thrift::ContentManifestStageOutput::empty(_) => None,
            content_manifest_thrift::ContentManifestStageOutput::UnknownField(x) => {
                return Err(anyhow!("unknown ContentManifestStageOutput variant {x}"));
            }
        })
    }
}
