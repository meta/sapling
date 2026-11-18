/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use mononoke_macros::mononoke;

use super::*;

async fn verify<F: PipelineTestFixture + Send>(fb: FacebookInit) -> Result<()> {
    for (path, cuts) in [
        ("", vec![]),
        ("", b"beiostu".to_vec()),
        ("a", b"beiostu".to_vec()),
    ] {
        let path = MPath::new(path)?;
        for batch_size in [1, 3] {
            let mut config = pipeline_config_from_stages(F::pipeline_stages())?;
            config.types =
                BTreeSet::from([DerivableType::SkeletonManifests, DerivableType::BssmV3]);
            config.batch_size = NonZeroU64::new(batch_size).unwrap();
            let mut stages = HashMap::from([(
                path.clone(),
                DerivationPipelineStageConfig {
                    dependencies: vec![],
                    partition_cut_points: cuts.clone(),
                },
            )]);
            if !path.is_root() {
                stages.insert(
                    MPath::ROOT,
                    DerivationPipelineStageConfig::new(vec![path.clone()]),
                );
            }
            config.stages_by_type.insert(DerivableType::BssmV3, stages);
            verify_pipeline_first_then_canonical_with_config::<F>(fb, config, false).await?;
        }
    }
    Ok(())
}

macro_rules! fixtures {
    ($($name:ident => $fixture:ty),* $(,)?) => { $(
        #[mononoke::fbinit_test]
        async fn $name(fb: FacebookInit) -> Result<()> {
            verify::<$fixture>(fb).await
        }
    )*};
}

fixtures! {
    nested_directories => NestedDirectories,
    ancestor_directory_replacement => AncestorDirectoryReplacement,
    merged_deletion_history => MergedDeletionHistory,
    cross_stage_file_copy => CrossStageFileCopy,
    cross_stage_directory_copy => CrossStageDirectoryCopy,
    nested_subtree_copy => NestedSubtreeCopy,
    partition_file_copies => PartitionFileCopies,
    acl_nested_directories => AclNestedDirectories,
    augmented_no_hg_mapping => AugmentedManifestV2NoHgMapping,
    augmented_duplicate_parents => AugmentedManifestV2DuplicateParentEntriesNoHgMapping,
    augmented_absent_parent_stage => AugmentedManifestV2AbsentParentStageNoHgMapping,
    augmented_many_parents => AugmentedManifestV2P3PlusParentsNoHgMapping,
    augmented_acl => AugmentedManifestV2AclNoHgMapping,
    supplied_root => AugmentedManifestSuppliedRoot,
    supplied_root_acl => AugmentedManifestSuppliedRootAcl,
}

#[mononoke::fbinit_test]
async fn missing_stage_inputs(fb: FacebookInit) -> Result<()> {
    super::partitions::verify_missing_stage_inputs(
        fb,
        &[DerivableType::SkeletonManifests, DerivableType::BssmV3],
    )
    .await
}
