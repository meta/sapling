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
use mononoke_types::PipelineDerivableVariant;
use mononoke_types::ThriftConvert;
use mononoke_types::acl_manifest::AclManifestEntry;
use mononoke_types::acl_manifest::AclManifestRestriction;
use mononoke_types::sharded_map_v2::LoadableShardedMapV2Node;
use mononoke_types::thrift::acl_manifest as acl_manifest_thrift;
use mononoke_types::typed_hash::AclManifestId;

use crate::RootAclManifestId;
use crate::derive::create_acl_manifest_subentries;
use crate::derive::derive_acl_manifest_entry;
use crate::derive::get_acl_file_changes;

#[async_trait]
impl ManifestDerivable for RootAclManifestId {
    type TreeId = AclManifestId;
    type Leaf = AclManifestRestriction;
    type TrieMapType = LoadableShardedMapV2Node<AclManifestEntry>;

    const PIPELINE_VARIANT: PipelineDerivableVariant = PipelineDerivableVariant::AclManifests;

    async fn load_root_manifest(
        &self,
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
    ) -> Result<AclManifestId> {
        Ok(self.0)
    }

    async fn from_root_manifest(
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
        root: AclManifestId,
    ) -> Result<Self> {
        Ok(Self(root))
    }

    async fn derive_manifest(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsai: BonsaiChangeset,
        input: ManifestDerivationInput<Self::TreeId, Self::Leaf, Self::TrieMapType>,
    ) -> Result<ManifestStageOutput<Self::TreeId, Self::Leaf, Self::TrieMapType>> {
        let blobstore = derivation.blobstore();
        let acl_file_name = derivation
            .restricted_paths()
            .config()
            .acl_file_name()
            .to_string();
        let changes = get_acl_file_changes(
            ctx,
            blobstore,
            &bonsai,
            input.parent_outputs(),
            &acl_file_name,
            input.path(),
        )
        .await?;
        derive_acl_manifest_entry(ctx, blobstore, input, changes, &acl_file_name).await
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
            create_acl_manifest_subentries(ctx.clone(), derivation.blobstore().clone(), subentries)
                .await?,
        ))
    }

    fn encode_entry(entry: Option<Entry<AclManifestId, AclManifestRestriction>>) -> BlobstoreBytes {
        let entry = match entry {
            Some(Entry::Tree(acl_manifest_id)) => {
                acl_manifest_thrift::AclManifestStageOutput::acl_manifest_id(
                    acl_manifest_id.into_thrift(),
                )
            }
            Some(Entry::Leaf(acl_file)) => {
                acl_manifest_thrift::AclManifestStageOutput::acl_file(acl_file.into_thrift())
            }
            None => acl_manifest_thrift::AclManifestStageOutput::empty(
                acl_manifest_thrift::AclManifestStageOutputEmpty {
                    ..Default::default()
                },
            ),
        };
        BlobstoreBytes::from_bytes(compact_protocol::serialize(&entry))
    }

    fn decode_entry(
        bytes: BlobstoreBytes,
    ) -> Result<Option<Entry<AclManifestId, AclManifestRestriction>>> {
        let thrift_output: acl_manifest_thrift::AclManifestStageOutput =
            compact_protocol::deserialize(bytes.into_bytes())?;
        Ok(match thrift_output {
            acl_manifest_thrift::AclManifestStageOutput::acl_manifest_id(id) => {
                Some(Entry::Tree(AclManifestId::from_thrift(id)?))
            }
            acl_manifest_thrift::AclManifestStageOutput::acl_file(acl_file) => {
                Some(Entry::Leaf(AclManifestRestriction::from_thrift(acl_file)?))
            }
            acl_manifest_thrift::AclManifestStageOutput::empty(_) => None,
            acl_manifest_thrift::AclManifestStageOutput::UnknownField(x) => {
                return Err(anyhow!("unknown AclManifestStageOutput variant {x}"));
            }
        })
    }
}
