/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;

use acl_manifest::RootAclManifestId;
use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use context::CoreContext;
use derived_data::prefetch_content_metadata;
use derived_data_manager::DerivationContext;
use derived_data_manager::ManifestDerivable;
use derived_data_manager::ManifestDerivationInput;
use derived_data_manager::ManifestStageOutput;
use derived_data_manager::PipelineDerivable;
use derived_data_manager::StageId;
use derived_data_manager::manifest_partitions::PartitionMap;
use derived_data_manager::manifest_pipeline::derive_manifest_batch;
use derived_data_manager::stage_outputs::fetch_stage_outputs_with_fallback;
use fbthrift::compact_protocol;
use manifest::Entry;
use manifest::TreeInfoSubentries;
use mercurial_types::HgAugmentedManifestEntry;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::sharded_augmented_manifest::HgAugmentedDirectoryNode;
use mercurial_types::sharded_augmented_manifest::HgAugmentedFileLeafNode;
use mononoke_types::BlobstoreBytes;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::FileChange;
use mononoke_types::MPath;
use mononoke_types::PipelineDerivableVariant;
use mononoke_types::ThriftConvert;
use mononoke_types::acl_manifest::AclManifest;
use mononoke_types::sharded_map_v2::LoadableShardedMapV2Node;

use crate::derive_hg_augmented_manifest::AclStageOutput;
use crate::derive_hg_augmented_manifest::AugmentedInput;
use crate::derive_hg_augmented_manifest::AugmentedOutput;
use crate::derive_hg_augmented_manifest::augmented_entry_from_indexed;
use crate::derive_hg_augmented_manifest::create_augmented_subentries;
use crate::derive_hg_augmented_manifest::derive_augmented_stage;
use crate::derive_hg_augmented_manifest::finalize_augmented_manifest_root;
use crate::derive_hg_augmented_manifest::indexed_augmented_directory_from_id;
use crate::indexed_augmented_manifest::IndexedAugmentedDirectory;
use crate::indexed_augmented_manifest::IndexedAugmentedFile;
use crate::indexed_augmented_manifest::IndexedAugmentedTrieMap;
use crate::indexed_augmented_manifest::indexed_augmented_entry;
use crate::mapping::RootHgAugmentedManifestId;

#[async_trait]
impl ManifestDerivable for RootHgAugmentedManifestId {
    type TreeId = IndexedAugmentedDirectory;
    type Leaf = IndexedAugmentedFile;
    type TrieMapType = IndexedAugmentedTrieMap;

    const PIPELINE_VARIANT: PipelineDerivableVariant =
        PipelineDerivableVariant::HgAugmentedManifests;

    async fn load_root_manifest(
        &self,
        ctx: &CoreContext,
        derivation: &DerivationContext,
    ) -> Result<Self::TreeId> {
        indexed_augmented_directory_from_id(
            ctx,
            derivation.blobstore(),
            self.hg_augmented_manifest_id(),
            None,
        )
        .await
    }

    async fn from_root_manifest(
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
        root: Self::TreeId,
    ) -> Result<Self> {
        Ok(Self::new(HgAugmentedManifestId::new(root.node.treenode)))
    }

    async fn derive_manifest(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsai: BonsaiChangeset,
        input: AugmentedInput,
    ) -> Result<AugmentedOutput> {
        let stage = input.stage_id();
        let csid = bonsai.get_changeset_id();
        let acl_output =
            RootAclManifestId::fetch_stage_outputs(ctx, derivation, &stage, vec![csid])
                .await?
                .remove(&csid)
                .ok_or_else(|| anyhow!("missing ACL stage output for {csid} at {stage:?}"))?;
        Self::derive_with_acl(ctx, derivation, bonsai, input, acl_output).await
    }

    async fn derive_manifest_batch(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsais: Vec<BonsaiChangeset>,
        stage: &StageId,
        parents: HashMap<ChangesetId, AugmentedOutput>,
        dependency_outputs: HashMap<ChangesetId, HashMap<StageId, AugmentedOutput>>,
    ) -> Result<HashMap<ChangesetId, AugmentedOutput>> {
        let csids = bonsais
            .iter()
            .map(BonsaiChangeset::get_changeset_id)
            .collect();
        let mut acl_outputs =
            RootAclManifestId::fetch_stage_outputs(ctx, derivation, stage, csids).await?;
        derive_manifest_batch(
            bonsais,
            parents,
            dependency_outputs,
            |bonsai, parents, dependencies| {
                let csid = bonsai.get_changeset_id();
                let acl_output = acl_outputs.remove(&csid);
                async move {
                    let acl_output = acl_output.ok_or_else(|| {
                        anyhow!("missing ACL stage output for {csid} at {stage:?}")
                    })?;
                    let input =
                        ManifestDerivationInput::from_stage_outputs(stage, parents, dependencies)?;
                    Self::derive_with_acl(ctx, derivation, bonsai, input, acl_output).await
                }
            },
        )
        .await
    }

    async fn create_subentries(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        entries: TreeInfoSubentries<Self::TreeId, Self::Leaf, (), Self::TrieMapType>,
    ) -> Result<Self::TrieMapType> {
        create_augmented_subentries(ctx, derivation.blobstore(), entries).await
    }

    fn encode_entry(entry: Option<Entry<Self::TreeId, Self::Leaf>>) -> BlobstoreBytes {
        encode_entry(entry.map(augmented_entry_from_indexed))
    }

    fn decode_entry(bytes: BlobstoreBytes) -> Result<Option<Entry<Self::TreeId, Self::Leaf>>> {
        Ok(decode_entry(bytes)?.map(|entry| indexed_augmented_entry(entry, None)))
    }
}

impl RootHgAugmentedManifestId {
    async fn derive_with_acl(
        ctx: &CoreContext,
        derivation: &DerivationContext,
        bonsai: BonsaiChangeset,
        input: AugmentedInput,
        mut acl_output: AclStageOutput,
    ) -> Result<AugmentedOutput> {
        if matches!(&acl_output, ManifestStageOutput::Entry(Some(Entry::Tree(id))) if *id == AclManifest::empty_id())
        {
            acl_output = ManifestStageOutput::Entry(None);
        }
        let root_parents = match &input {
            ManifestDerivationInput::Entry { path, parents, .. } if path.is_root() => Some(
                parents
                    .iter()
                    .cloned()
                    .map(|entry| entry.map(augmented_entry_from_indexed))
                    .collect(),
            ),
            _ => None,
        };
        let root_acl_id = match &acl_output {
            ManifestStageOutput::Entry(Some(Entry::Tree(id))) => Some(*id),
            _ => None,
        };
        let file_changes = bonsai
            .file_changes()
            .filter(|(path, change)| {
                input.path().is_prefix_of(*path)
                    || (change.is_changed() && path.is_prefix_of(input.path()))
            })
            .map(|(path, change)| {
                Ok((
                    path.clone(),
                    match change {
                        FileChange::Change(change) => Some(change.clone()),
                        FileChange::Deletion => None,
                        FileChange::UntrackedChange(_) | FileChange::UntrackedDeletion => {
                            anyhow::bail!("Can't derive manifest for snapshot")
                        }
                    },
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let content_ids: HashSet<_> = file_changes
            .iter()
            .filter(|(path, _)| input.contains_change(path))
            .filter_map(|(_, change)| change.as_ref().map(|change| change.content_id()))
            .collect();
        let content_metadata = prefetch_content_metadata(ctx, derivation.blobstore(), content_ids);
        let mut parents = bonsai.parents();
        let parent_csids = (parents.next(), parents.next());
        let output = derive_augmented_stage(
            ctx,
            derivation.blobstore(),
            input,
            file_changes,
            parent_csids,
            |csid| async move {
                let root = fetch_stage_outputs_with_fallback::<Self>(
                    ctx,
                    derivation,
                    &StageId::Manifest(MPath::ROOT),
                    vec![csid],
                )
                .await?
                .remove(&csid)
                .ok_or_else(|| anyhow!("missing augmented root for {csid}"))?
                .into_entry()?
                .and_then(Entry::into_tree)
                .ok_or_else(|| anyhow!("augmented root for {csid} is not a tree"))?;
                Ok(Some(HgAugmentedManifestId::new(root.node.treenode)))
            },
            &content_metadata.await?,
            &derivation.restricted_paths(),
            acl_output,
        )
        .await?;
        match root_parents {
            None => Ok(output),
            Some(parents) => {
                let root = finalize_augmented_manifest_root(
                    ctx,
                    derivation.blobstore(),
                    output.into_entry()?.map(augmented_entry_from_indexed),
                    parents,
                    &derivation.restricted_paths(),
                    root_acl_id,
                )
                .await?;
                Ok(ManifestStageOutput::Entry(Some(Entry::Tree(
                    IndexedAugmentedDirectory {
                        node: root,
                        index: None,
                    },
                ))))
            }
        }
    }
}

#[async_trait]
impl PartitionMap<RootHgAugmentedManifestId> for IndexedAugmentedTrieMap {
    async fn encode(
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
        output: derived_data_manager::manifest_partitions::PartitionOutput<Self>,
    ) -> Result<BlobstoreBytes> {
        Ok(BlobstoreBytes::from_bytes(
            output.load().await?.into_inner().into_bytes(),
        ))
    }

    fn decode(
        _ctx: &CoreContext,
        _derivation: &DerivationContext,
        bytes: BlobstoreBytes,
    ) -> Result<derived_data_manager::manifest_partitions::PartitionOutput<Self>> {
        Ok(Self::new(
            LoadableShardedMapV2Node::from_bytes(&bytes.into_bytes())?,
            None,
        )
        .into())
    }
}

fn encode_entry(output: Option<HgAugmentedManifestEntry>) -> BlobstoreBytes {
    let thrift_output = match output {
        Some(HgAugmentedManifestEntry::DirectoryNode(dir)) => {
            mercurial_thrift::HgAugmentedManifestStageOutput::directory(dir.into_thrift())
        }
        Some(HgAugmentedManifestEntry::FileNode(leaf)) => {
            mercurial_thrift::HgAugmentedManifestStageOutput::file(leaf.into_thrift())
        }
        None => mercurial_thrift::HgAugmentedManifestStageOutput::empty(
            mercurial_thrift::HgAugmentedManifestStageOutputEmpty {},
        ),
    };
    BlobstoreBytes::from_bytes(compact_protocol::serialize(&thrift_output))
}

fn decode_entry(bytes: BlobstoreBytes) -> Result<Option<HgAugmentedManifestEntry>> {
    let thrift_output: mercurial_thrift::HgAugmentedManifestStageOutput =
        compact_protocol::deserialize(bytes.into_bytes())?;
    let output = match thrift_output {
        mercurial_thrift::HgAugmentedManifestStageOutput::directory(dir) => Some(
            HgAugmentedManifestEntry::DirectoryNode(HgAugmentedDirectoryNode::from_thrift(dir)?),
        ),
        mercurial_thrift::HgAugmentedManifestStageOutput::file(leaf) => Some(
            HgAugmentedManifestEntry::FileNode(HgAugmentedFileLeafNode::from_thrift(leaf)?),
        ),
        mercurial_thrift::HgAugmentedManifestStageOutput::empty(_) => None,
        mercurial_thrift::HgAugmentedManifestStageOutput::UnknownField(x) => {
            return Err(anyhow!(
                "unknown HgAugmentedManifestStageOutput variant {x}"
            ));
        }
    };
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use derived_data_manager::BonsaiDerivable;
    use derived_data_manager::StageInputs;
    use derived_data_manager::stage_outputs::fetch_stage_outputs;
    use derived_data_manager::stage_outputs::store_stage_outputs;
    use fbinit::FacebookInit;
    use mercurial_types::HgNodeHash;
    use mononoke_macros::mononoke;
    use mononoke_types::ChangesetId;
    use mononoke_types::FileType;
    use mononoke_types::hash::Blake2;
    use mononoke_types::hash::Blake3;
    use mononoke_types::hash::Sha1 as ContentSha1;
    use mononoke_types::sha1_hash::Sha1 as NodeSha1;
    use repo_blobstore::RepoBlobstore;
    use repo_derived_data::RepoDerivedData;
    use repo_derived_data::RepoDerivedDataRef;

    use super::*;

    fn stage_blobstore_key(path: &MPath, prefix: &str, csid: ChangesetId) -> String {
        format!(
            "derived_hgaugmentedmanifest_stage.{}.{}{}",
            path.get_path_hash().to_hex(),
            prefix,
            csid
        )
    }

    fn output(entry: Option<HgAugmentedManifestEntry>) -> AugmentedOutput {
        ManifestStageOutput::Entry(entry.map(|entry| indexed_augmented_entry(entry, None)))
    }

    #[facet::container]
    #[derive(Clone)]
    struct TestRepo(RepoBlobstore, RepoDerivedData);

    fn directory_output(byte: u8) -> Option<HgAugmentedManifestEntry> {
        Some(HgAugmentedManifestEntry::DirectoryNode(
            HgAugmentedDirectoryNode {
                treenode: HgNodeHash::new(NodeSha1::from_byte_array([byte; 20])),
                augmented_manifest_id: Blake3::from_byte_array([byte; 32]),
                augmented_manifest_size: u64::from(byte),
                acl_manifest_directory_id: None,
            },
        ))
    }

    fn file_output(byte: u8) -> Option<HgAugmentedManifestEntry> {
        Some(HgAugmentedManifestEntry::FileNode(
            HgAugmentedFileLeafNode {
                file_type: FileType::Regular,
                filenode: HgNodeHash::new(NodeSha1::from_byte_array([byte; 20])),
                content_blake3: Blake3::from_byte_array([byte; 32]),
                content_sha1: ContentSha1::from_byte_array([byte; 20]),
                total_size: u64::from(byte),
                file_header_metadata: None,
            },
        ))
    }

    #[mononoke::fbinit_test]
    async fn test_augmented_manifest_stage_storage_round_trips(fb: FacebookInit) -> Result<()> {
        // Given: directory, file, and absent outputs for one stage path.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let stage_path = MPath::new("src")?;
        let key_prefix = "test-prefix.";
        let cases = [
            (
                ChangesetId::new(Blake2::from_byte_array([1; 32])),
                directory_output(1),
            ),
            (
                ChangesetId::new(Blake2::from_byte_array([2; 32])),
                file_output(2),
            ),
            (ChangesetId::new(Blake2::from_byte_array([3; 32])), None),
        ];

        let derivation = repo.repo_derived_data().manager().derivation_context(None);
        let stage = StageId::Manifest(stage_path.clone());
        let reference = store_stage_outputs::<RootHgAugmentedManifestId, _, _>(
            &ctx,
            &derivation,
            &stage,
            HashMap::from(cases.clone()),
            |_| None,
            async |entry| Ok(encode_entry(entry)),
        )
        .await?
        .unwrap();
        // An old individual stage blob must not satisfy a missing batch input.
        derivation
            .blobstore()
            .put(
                &ctx,
                stage_blobstore_key(&stage_path, key_prefix, cases[0].0),
                encode_entry(cases[0].1.clone()),
            )
            .await?;
        let fetch = async |derivation: &DerivationContext| {
            fetch_stage_outputs::<RootHgAugmentedManifestId, _, _>(
                &ctx,
                derivation,
                &stage,
                cases
                    .iter()
                    .map(|(csid, _)| *csid)
                    .chain([ChangesetId::new(Blake2::from_byte_array([4; 32]))])
                    .collect(),
                |_| None,
                async |_, bytes| decode_entry(bytes),
            )
            .await
        };
        assert!(fetch(&derivation).await?.is_empty());
        let inputs = StageInputs::new(&[reference])?;
        let supplied = repo
            .repo_derived_data()
            .manager()
            .with_stage_inputs(inputs)
            .derivation_context(None);
        use derived_data_manager::stage_outputs::StageOutput;
        let actual = fetch(&supplied).await?;
        assert_eq!(actual.len(), cases.len());
        for (csid, expected) in cases {
            assert!(actual[&csid].equivalent(&expected).await?);
        }

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_terminal_publication_uses_only_shared_mapping(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

        // Given: a terminal output in production mode.
        let cs_id = ChangesetId::new(Blake2::from_byte_array([5; 32]));
        let expected =
            HgAugmentedManifestId::new(HgNodeHash::new(NodeSha1::from_byte_array([5; 20])));

        async move {
            let derivation = repo.repo_derived_data().manager().derivation_context(None);
            let key_prefix = derivation.mapping_key_prefix::<RootHgAugmentedManifestId>();

            // When: the pipeline stores the terminal root.
            RootHgAugmentedManifestId::store_stage_outputs(
                &ctx,
                &derivation,
                &StageId::Manifest(MPath::ROOT),
                HashMap::from([(cs_id, output(directory_output(5)))]),
            )
            .await?;

            // Then: only the shared root mapping is recorded.
            assert_eq!(
                derivation
                    .blobstore()
                    .get(&ctx, &stage_blobstore_key(&MPath::ROOT, key_prefix, cs_id))
                    .await?,
                None,
            );
            assert_eq!(
                RootHgAugmentedManifestId::fetch(&ctx, &derivation, cs_id)
                    .await?
                    .map(|root| root.hg_augmented_manifest_id()),
                Some(expected),
            );

            Ok(())
        }
        .await
    }

    #[mononoke::fbinit_test]
    async fn test_shadow_terminal_uses_only_supplied_outputs(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let output_cs_id = ChangesetId::new(Blake2::from_byte_array([6; 32]));
        let shared_cs_id = ChangesetId::new(Blake2::from_byte_array([7; 32]));
        let expected = directory_output(6);

        async move {
            let derivation = repo
                .repo_derived_data()
                .manager()
                .with_pipeline_mode(derived_data_manager::PipelineMode::Shadow)
                .derivation_context(None);

            // One explicit output and one changeset with only a canonical mapping.
            let returned = RootHgAugmentedManifestId::store_stage_outputs(
                &ctx,
                &derivation,
                &StageId::Manifest(MPath::ROOT),
                HashMap::from([(output_cs_id, output(expected.clone()))]),
            )
            .await?;
            RootHgAugmentedManifestId::new(HgAugmentedManifestId::new(HgNodeHash::new(
                NodeSha1::from_byte_array([7; 20]),
            )))
            .store_mapping(&ctx, &derivation, shared_cs_id)
            .await?;

            let inputs = StageInputs::new(&returned.into_iter().collect::<Vec<_>>())?;
            let derivation = repo
                .repo_derived_data()
                .manager()
                .with_pipeline_mode(derived_data_manager::PipelineMode::Shadow)
                .with_stage_inputs(inputs)
                .derivation_context(None);

            // When: the pipeline checks terminal completion in shadow mode.
            let outputs = RootHgAugmentedManifestId::fetch_stage_outputs(
                &ctx,
                &derivation,
                &StageId::Manifest(MPath::ROOT),
                vec![output_cs_id, shared_cs_id],
            )
            .await?;

            // Only the supplied output is completion; no canonical mapping was written.
            use derived_data_manager::stage_outputs::StageOutput;
            assert_eq!(outputs.len(), 1);
            assert!(outputs[&output_cs_id].equivalent(&output(expected)).await?);
            assert_eq!(
                RootHgAugmentedManifestId::fetch(&ctx, &derivation, output_cs_id).await?,
                None,
            );

            Ok(())
        }
        .await
    }
}
