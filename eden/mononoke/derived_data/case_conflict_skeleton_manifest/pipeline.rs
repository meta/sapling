/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use async_trait::async_trait;
use blobstore::Loadable;
use blobstore::Storable;
use cloned::cloned;
use context::CoreContext;
use derived_data_manager::DerivationContext;
use derived_data_manager::ManifestDerivable;
use derived_data_manager::ManifestDerivationInput;
use derived_data_manager::ManifestStageOutput;
use fbthrift::compact_protocol;
use manifest::Entry;
use manifest::TreeInfo;
use manifest::TreeInfoSubentries;
use mononoke_types::BlobstoreBytes;
use mononoke_types::BlobstoreValue;
use mononoke_types::BonsaiChangeset;
use mononoke_types::MPath;
use mononoke_types::PipelineDerivableVariant;
use mononoke_types::ThriftConvert;
use mononoke_types::case_conflict_skeleton_manifest::CaseConflictSkeletonManifest;
use mononoke_types::case_conflict_skeleton_manifest::CcsmEntry;
use mononoke_types::sharded_map_v2::LoadableShardedMapV2Node;
use mononoke_types::thrift::ccsm::CcsmStageOutput;

use crate::CcsmPath;
use crate::RootCaseConflictSkeletonManifestId;
use crate::derive::create_case_conflict_skeleton_manifest;
use crate::derive::get_ccsm_path_changes;

#[async_trait]
impl ManifestDerivable for RootCaseConflictSkeletonManifestId {
    type TreeId = CaseConflictSkeletonManifest;
    type Leaf = ();
    type TrieMapType = LoadableShardedMapV2Node<CcsmEntry>;

    const PIPELINE_VARIANT: PipelineDerivableVariant = PipelineDerivableVariant::Ccsm;

    fn manifest_path(path: &MPath) -> Option<MPath> {
        if path.is_root() {
            Some(MPath::ROOT)
        } else {
            CcsmPath::transform_mpath(path.clone()).map(|path| path.into_raw().into())
        }
    }

    async fn load_root_manifest(
        &self,
        ctx: &CoreContext,
        derivation: &DerivationContext,
    ) -> Result<Self::TreeId> {
        Ok(self.inner_id().load(ctx, derivation.blobstore()).await?)
    }

    async fn from_root_manifest(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        root: Self::TreeId,
    ) -> Result<Self> {
        Ok(Self(
            root.into_blob().store(ctx, derivation.blobstore()).await?,
        ))
    }

    async fn derive_manifest(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsai: BonsaiChangeset,
        input: ManifestDerivationInput<Self::TreeId, Self::Leaf, Self::TrieMapType>,
    ) -> Result<ManifestStageOutput<Self::TreeId, Self::Leaf, Self::TrieMapType>> {
        let blobstore = derivation.blobstore();
        input
            .derive(
                ctx.clone(),
                blobstore.clone(),
                get_ccsm_path_changes(&bonsai).await,
                {
                    cloned!(ctx, blobstore);
                    move |info: TreeInfo<Self::TreeId, (), (), Self::TrieMapType>| {
                        cloned!(ctx, blobstore);
                        async move {
                            Ok((
                                (),
                                create_case_conflict_skeleton_manifest(
                                    ctx,
                                    blobstore,
                                    info.subentries,
                                )
                                .await?,
                            ))
                        }
                    }
                },
                |_| async { Ok(((), ())) },
                |subentries| Self::create_subentries(ctx, derivation, subentries),
            )
            .await
    }

    async fn create_subentries(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        subentries: TreeInfoSubentries<Self::TreeId, Self::Leaf, (), Self::TrieMapType>,
    ) -> Result<Self::TrieMapType> {
        Ok(LoadableShardedMapV2Node::Inlined(
            create_case_conflict_skeleton_manifest(
                ctx.clone(),
                derivation.blobstore().clone(),
                subentries,
            )
            .await?
            .subentries,
        ))
    }

    fn encode_entry(entry: Option<Entry<Self::TreeId, Self::Leaf>>) -> BlobstoreBytes {
        let entry = entry
            .map(|entry| match entry {
                Entry::Tree(tree) => CcsmEntry::Directory(tree),
                Entry::Leaf(()) => CcsmEntry::File,
            })
            .map(ThriftConvert::into_thrift);
        BlobstoreBytes::from_bytes(compact_protocol::serialize(&CcsmStageOutput { entry }))
    }

    fn decode_entry(bytes: BlobstoreBytes) -> Result<Option<Entry<Self::TreeId, Self::Leaf>>> {
        let output: CcsmStageOutput = compact_protocol::deserialize(bytes.into_bytes())?;
        Ok(output
            .entry
            .map(CcsmEntry::from_thrift)
            .transpose()?
            .map(|entry| match entry {
                CcsmEntry::Directory(tree) => Entry::Tree(tree),
                CcsmEntry::File => Entry::Leaf(()),
            }))
    }
}
