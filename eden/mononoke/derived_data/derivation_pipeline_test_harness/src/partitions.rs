/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;

use deleted_manifest::RootDeletedManifestIdCommon;
use deleted_manifest::RootDeletedManifestV2Id;
use derived_data_manager::DerivationStagePayload;
use derived_data_manager::ManifestStagePayload;
use derived_data_manager::PipelineDerivable;
use derived_data_manager::StageId;
use derived_data_manager::StageInputs;
use fixtures::AncestorDirectoryReplacement;
use fixtures::AugmentedManifestV2AbsentParentStageNoHgMapping;
use fixtures::AugmentedManifestV2AclNoHgMapping;
use fixtures::AugmentedManifestV2DuplicateParentEntriesNoHgMapping;
use fixtures::AugmentedManifestV2NoHgMapping;
use fixtures::AugmentedManifestV2P3PlusParentsNoHgMapping;
use fixtures::CrossStageDirectoryCopy;
use fixtures::CrossStageFileCopy;
use fixtures::NestedDirectories;
use fixtures::NestedSubtreeCopy;
use fixtures::PartitionFileCopies;
use fsnodes::RootFsnodeId;
use mononoke_macros::mononoke;
use mononoke_types::FileType;
use mononoke_types::NonRootMPath;
use mononoke_types::deleted_manifest_common::DeletedManifestCommon;
use tests_utils::CreateCommitContext;

use super::*;

async fn derive_with_references(
    manager: &DerivedDataManager,
    ctx: &CoreContext,
    references: &mut Vec<derived_data_manager::StageOutputs>,
    commits: Vec<ChangesetId>,
    payload: &DerivationStagePayload,
    variant: mononoke_types::PipelineDerivableVariant,
) -> Result<()> {
    let inputs = StageInputs::new(references)?;
    let result = bulk_derivation::derive_stage_batch(
        &manager.with_stage_inputs(inputs),
        ctx,
        commits,
        payload,
        variant,
    )
    .await?;
    references.extend(result.outputs);
    Ok(())
}

fn partitioned_config<F: PipelineTestFixture>() -> Result<DerivationPipelineConfig> {
    let mut config = pipeline_config_from_stages(F::pipeline_stages())?;
    for stage in config
        .stages
        .values_mut()
        .filter(|stage| stage.dependencies.is_empty())
    {
        stage.partition_cut_points = b"dgjnqtw".to_vec();
    }
    config.validate()?;
    Ok(config)
}

async fn verify_partitions<F: PipelineTestFixture + Send>(
    fb: FacebookInit,
    config: DerivationPipelineConfig,
    canonical_boundary: &[&str],
) -> Result<()> {
    config.validate()?;
    let ctx = &CoreContext::test_mock(fb);
    let (repo, labels, _) = F::get_repo_and_dag::<TestRepo>(fb).await;
    let manager = &repo
        .repo_derived_data()
        .manager()
        .with_pipeline_mode(derived_data_manager::PipelineMode::Shadow);
    let head = repo
        .bookmarks()
        .get(
            ctx.clone(),
            &BookmarkKey::new("master")?,
            bookmarks::Freshness::MostRecent,
        )
        .await?
        .unwrap();
    let mut commits = repo
        .commit_graph()
        .ancestors_difference(ctx, vec![head], vec![])
        .await?;
    commits.reverse();
    let boundary = canonical_boundary
        .iter()
        .map(|label| labels[*label])
        .collect::<Vec<_>>();
    manager
        .derive_bulk_locally(ctx, &boundary, None, &PIPELINE_TYPES, None, None)
        .await?;
    let plan = plan_pipeline(
        ctx,
        &repo,
        manager,
        &config,
        &PIPELINE_TYPES,
        head,
        boundary,
    )
    .await?;
    assert!(
        plan.batches
            .iter()
            .all(|batch| batch.commits.len() as u64 <= config.batch_size.get())
    );

    async {
        let mut references = Vec::new();
        for batches in plan.batches.chunk_by(|_, next| next.chokepoint.is_none()) {
            for &derived_type in &plan.sorted_types {
                let variant = derived_type.into_pipeline_derivable_variant()?;
                // Partitions advance to the next chokepoint before assembly catches up.
                for (path, stage) in &config.stages {
                    let payload = ManifestStagePayload::from_config(path.clone(), stage);
                    for range in payload.partitions().into_iter().rev() {
                        let payload = DerivationStagePayload::Subentries {
                            path: path.clone(),
                            range,
                        };
                        let payload = DerivationStagePayload::from_thrift(payload.to_thrift())?;
                        for batch in batches {
                            derive_with_references(
                                manager,
                                ctx,
                                &mut references,
                                batch.commits.clone(),
                                &payload,
                                variant,
                            )
                            .await?;
                        }
                    }
                }
                assert!(
                    !BulkDerivation::is_stage_derived(
                        manager,
                        ctx,
                        head,
                        derived_type,
                        &StageId::Manifest(MPath::ROOT)
                    )
                    .await?
                );
                for batch in batches {
                    for path in &plan.sorted_stages {
                        let payload = DerivationStagePayload::Manifest(
                            ManifestStagePayload::from_config(path.clone(), &config.stages[path]),
                        );
                        let payload = DerivationStagePayload::from_thrift(payload.to_thrift())?;
                        derive_with_references(
                            manager,
                            ctx,
                            &mut references,
                            batch.commits.clone(),
                            &payload,
                            variant,
                        )
                        .await?;
                    }
                    if bulk_derivation::pipeline_has_finalize(variant) {
                        derive_with_references(
                            manager,
                            ctx,
                            &mut references,
                            batch.commits.clone(),
                            &DerivationStagePayload::Finalize,
                            variant,
                        )
                        .await?;
                    }
                }
            }
        }
        manager
            .derive_bulk_locally(ctx, &commits, None, &PIPELINE_TYPES, None, None)
            .await?;
        let inputs = StageInputs::new(&references)?;
        verify_pipeline_output::<F>(
            &manager.with_stage_inputs(inputs),
            ctx,
            &commits,
            &config,
            &plan,
        )
        .await
    }
    .await
}

#[mononoke::fbinit_test]
async fn partitions_advance_ahead_of_assembly(fb: FacebookInit) -> Result<()> {
    for batch_size in [1, 3, 20] {
        let mut config = partitioned_config::<NestedDirectories>()?;
        config.batch_size = NonZeroU64::new(batch_size).unwrap();
        verify_partitions::<NestedDirectories>(fb, config, &[]).await?;
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn partitions_extract_canonical_parents(fb: FacebookInit) -> Result<()> {
    for boundary in ["Z", "A", "B", "C", "D"] {
        verify_partitions::<AugmentedManifestV2NoHgMapping>(
            fb,
            partitioned_config::<AugmentedManifestV2NoHgMapping>()?,
            &[boundary],
        )
        .await?;
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn partitions_reuse_supplied_root(fb: FacebookInit) -> Result<()> {
    verify_partitions::<AugmentedManifestSuppliedRoot>(
        fb,
        partitioned_config::<AugmentedManifestSuppliedRoot>()?,
        &["S"],
    )
    .await
}

#[mononoke::fbinit_test]
async fn partitions_reuse_supplied_root_acl(fb: FacebookInit) -> Result<()> {
    verify_partitions::<AugmentedManifestSuppliedRootAcl>(
        fb,
        partitioned_config::<AugmentedManifestSuppliedRootAcl>()?,
        &["S"],
    )
    .await
}

#[mononoke::fbinit_test]
async fn partitions_directory_replacement(fb: FacebookInit) -> Result<()> {
    verify_partitions::<AncestorDirectoryReplacement>(
        fb,
        partitioned_config::<AncestorDirectoryReplacement>()?,
        &[],
    )
    .await
}

#[mononoke::fbinit_test]
async fn partitions_empty_root_and_stage_transitions(fb: FacebookInit) -> Result<()> {
    verify_partitions::<AugmentedManifestV2NoHgMapping>(
        fb,
        partitioned_config::<AugmentedManifestV2NoHgMapping>()?,
        &[],
    )
    .await
}

#[mononoke::fbinit_test]
async fn partitions_duplicate_parent_entries(fb: FacebookInit) -> Result<()> {
    verify_partitions::<AugmentedManifestV2DuplicateParentEntriesNoHgMapping>(
        fb,
        partitioned_config::<AugmentedManifestV2DuplicateParentEntriesNoHgMapping>()?,
        &[],
    )
    .await
}

#[mononoke::fbinit_test]
async fn partitions_absent_parent_stage(fb: FacebookInit) -> Result<()> {
    verify_partitions::<AugmentedManifestV2AbsentParentStageNoHgMapping>(
        fb,
        partitioned_config::<AugmentedManifestV2AbsentParentStageNoHgMapping>()?,
        &[],
    )
    .await
}

#[mononoke::fbinit_test]
async fn partitions_octopus_merge(fb: FacebookInit) -> Result<()> {
    verify_partitions::<AugmentedManifestV2P3PlusParentsNoHgMapping>(
        fb,
        partitioned_config::<AugmentedManifestV2P3PlusParentsNoHgMapping>()?,
        &[],
    )
    .await
}

#[mononoke::fbinit_test]
async fn partitions_mixed_canonical_and_pipeline_parents(fb: FacebookInit) -> Result<()> {
    for boundary in ["B", "C"] {
        verify_partitions::<AugmentedManifestV2AbsentParentStageNoHgMapping>(
            fb,
            partitioned_config::<AugmentedManifestV2AbsentParentStageNoHgMapping>()?,
            &[boundary],
        )
        .await?;
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn partitions_cover_byte_boundaries(fb: FacebookInit) -> Result<()> {
    async {
        for cuts in [vec![1, 255], b"dgjnqtw".to_vec(), (1..=255).collect()] {
            verify_byte_boundaries(fb, cuts).await?;
        }
        Ok(())
    }
    .await
}

async fn verify_byte_boundaries(fb: FacebookInit, cuts: Vec<u8>) -> Result<()> {
    let ctx = &CoreContext::test_mock(fb);
    let (repo, _, _) = AugmentedManifestV2NoHgMapping::get_repo_and_dag::<TestRepo>(fb).await;
    let manager = &repo
        .repo_derived_data()
        .manager()
        .with_pipeline_mode(derived_data_manager::PipelineMode::Shadow);
    // Mercurial rejects newline and 0x01 in paths.
    let paths = (1..=255)
        .filter(|byte| !b"/\n\x01".contains(byte))
        .flat_map(|byte| {
            [
                vec![byte, b'a'],
                vec![byte, b'a', b'a'],
                [vec![byte], b"b/leaf".to_vec()].concat(),
            ]
        })
        .map(NonRootMPath::new)
        .collect::<Result<Vec<_>>>()?;
    let first = CreateCommitContext::new_root(ctx, &repo)
        .add_files(paths.iter().cloned().map(|path| (path, "content")))
        .commit()
        .await?;
    let left = CreateCommitContext::new(ctx, &repo, vec![first])
        .add_file_with_type(paths[0].clone(), "content", FileType::Executable)
        .delete_file(paths[1].clone())
        .commit()
        .await?;
    let right = CreateCommitContext::new(ctx, &repo, vec![first])
        .add_file_with_type(paths[0].clone(), "content", FileType::Symlink)
        .add_file(paths[1].clone(), "changed")
        .commit()
        .await?;
    let merged = CreateCommitContext::new(ctx, &repo, vec![left, right])
        .add_file_with_type(paths[0].clone(), "content", FileType::Executable)
        .delete_file(paths[1].clone())
        .commit()
        .await?;
    let unchanged = CreateCommitContext::new(ctx, &repo, vec![merged])
        .commit()
        .await?;
    let mut delete = CreateCommitContext::new(ctx, &repo, vec![unchanged]);
    for (i, path) in paths.iter().enumerate() {
        if i != 1 {
            delete = delete.delete_file(path.clone());
        }
    }
    let empty = delete.commit().await?;
    let commits = [first, left, right, merged, unchanged, empty];
    let mut config = pipeline_config_from_stages(vec![("", vec![])])?;
    config
        .stages
        .get_mut(&MPath::ROOT)
        .unwrap()
        .partition_cut_points = cuts;
    run_pipeline_first_then_canonical::<AugmentedManifestV2NoHgMapping>(
        ctx,
        &repo,
        manager,
        &config,
        &PIPELINE_TYPES,
        &commits,
        empty,
    )
    .await
}

#[mononoke::fbinit_test]
async fn partitions_preserve_case_conflicts(fb: FacebookInit) -> Result<()> {
    for nested in [false, true] {
        let ctx = &CoreContext::test_mock(fb);
        let (repo, _, _) = AugmentedManifestV2NoHgMapping::get_repo_and_dag::<TestRepo>(fb).await;
        let manager = &repo
            .repo_derived_data()
            .manager()
            .with_pipeline_mode(derived_data_manager::PipelineMode::Shadow);
        let first = CreateCommitContext::new_root(ctx, &repo)
            .add_files([
                ("Dir/Apple/leaf", "a"),
                ("Dir/apple", "b"),
                ("Dir/Zebra", "c"),
                ("Dir/zebra", "d"),
                ("Dir/İ/leaf", "e"),
                ("Dir/i̇/leaf", "f"),
                ("dir/other", "g"),
                ("K/leaf", "h"),
                ("k/leaf", "i"),
            ])
            .add_file(NonRootMPath::new(b"\xff/leaf")?, "ignored by CCSM")
            .commit()
            .await?;
        let deleted = CreateCommitContext::new(ctx, &repo, vec![first])
            .delete_file("Dir/apple")
            .delete_file("Dir/İ/leaf")
            .commit()
            .await?;
        let replaced = CreateCommitContext::new(ctx, &repo, vec![deleted])
            .add_file("Dir", "replace directory")
            .commit()
            .await?;
        let restored = CreateCommitContext::new(ctx, &repo, vec![replaced])
            .delete_file("Dir")
            .add_file("Dir/APPLE", "recreate")
            .add_file("Dir/apple", "conflict")
            .commit()
            .await?;
        let mut config = if nested {
            pipeline_config_from_stages(vec![
                ("", vec!["Dir", "dir", "K", "k"]),
                ("Dir", vec![]),
                ("dir", vec![]),
                ("K", vec![]),
                ("k", vec![]),
            ])?
        } else {
            pipeline_config_from_stages(vec![("", vec![])])?
        };
        if nested {
            let invalid = MPath::new(b"\xff")?;
            config
                .stages
                .get_mut(&MPath::ROOT)
                .unwrap()
                .dependencies
                .push(invalid.clone());
            config
                .stages
                .insert(invalid, DerivationPipelineStageConfig::new(vec![]));
        }
        for stage in config
            .stages
            .values_mut()
            .filter(|stage| stage.dependencies.is_empty())
        {
            stage.partition_cut_points = b"AMamz".to_vec();
        }
        run_pipeline_first_then_canonical::<AugmentedManifestV2NoHgMapping>(
            ctx,
            &repo,
            manager,
            &config,
            &PIPELINE_TYPES,
            &[first, deleted, replaced, restored],
            restored,
        )
        .await?;
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn partitions_require_all_dependencies_and_allow_retry(fb: FacebookInit) -> Result<()> {
    let ctx = &CoreContext::test_mock(fb);
    let (repo, _, _) = AugmentedManifestV2NoHgMapping::get_repo_and_dag::<TestRepo>(fb).await;
    let manager = &repo
        .repo_derived_data()
        .manager()
        .with_pipeline_mode(derived_data_manager::PipelineMode::Shadow);
    let csid = CreateCommitContext::new_root(ctx, &repo)
        .add_file("a", "left")
        .add_file("z", "right")
        .commit()
        .await?;
    let stage = StageId::Manifest(MPath::ROOT);
    let config = DerivationPipelineStageConfig {
        dependencies: vec![],
        partition_cut_points: vec![b'm'],
    };
    let payload = ManifestStagePayload::from_config(MPath::ROOT, &config);
    let partitions = payload
        .partitions()
        .into_iter()
        .map(|range| DerivationStagePayload::Subentries {
            path: MPath::ROOT,
            range,
        })
        .collect::<Vec<_>>();
    let assembly = DerivationStagePayload::Manifest(payload);
    let first = manager
        .derive_stage_batch::<RootFsnodeId>(ctx, vec![csid], &partitions[0])
        .await?;
    let mut references: Vec<_> = first.outputs.into_iter().collect();
    let inputs = StageInputs::new(&references)?;
    let manager = manager.with_stage_inputs(inputs);
    let error = manager
        .derive_stage_batch::<RootFsnodeId>(ctx, vec![csid], &assembly)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("missing dependency stage output"));
    assert!(
        !manager
            .is_stage_derived::<RootFsnodeId>(ctx, csid, &stage)
            .await?
    );

    let second = manager
        .derive_stage_batch::<RootFsnodeId>(ctx, vec![csid], &partitions[1])
        .await?;
    references.extend(second.outputs);
    let inputs = StageInputs::new(&references)?;
    let manager = manager.with_stage_inputs(inputs);
    let initial = manager
        .derive_stage_batch::<RootFsnodeId>(ctx, vec![csid], &assembly)
        .await?
        .outputs;
    let mut replayed = None;
    for payload in partitions.iter().chain(std::iter::once(&assembly)) {
        replayed = manager
            .derive_stage_batch::<RootFsnodeId>(ctx, vec![csid], payload)
            .await?
            .outputs;
    }
    assert_eq!(initial, replayed);
    references.extend(replayed);
    let manager = manager.with_stage_inputs(StageInputs::new(&references)?);
    manager
        .derive_bulk_locally(ctx, &[csid], None, &[DerivableType::Fsnodes], None, None)
        .await?;
    assert!(
        RootFsnodeId::verify_stage(ctx, &manager.derivation_context(None), csid, &stage).await?
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn local_driver_executes_partitions(fb: FacebookInit) -> Result<()> {
    for terminal_mapping in [false, true] {
        verify_pipeline_first_then_canonical_with_config::<NestedDirectories>(
            fb,
            partitioned_config::<NestedDirectories>()?,
            terminal_mapping,
        )
        .await?;
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn local_driver_releases_outputs_after_their_last_consumer(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, _, _) = NestedDirectories::get_repo_and_dag::<TestRepo>(fb).await;
    let a = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("data/a", "a")
        .commit()
        .await?;
    let b = CreateCommitContext::new(&ctx, &repo, vec![a])
        .add_file("data/b", "b")
        .commit()
        .await?;
    let c = CreateCommitContext::new(&ctx, &repo, vec![b])
        .add_file("data/c", "c")
        .commit()
        .await?;
    let d = CreateCommitContext::new(&ctx, &repo, vec![c, a])
        .add_file("data/d", "d")
        .commit()
        .await?;
    let manager = &repo
        .repo_derived_data()
        .manager()
        .with_pipeline_mode(derived_data_manager::PipelineMode::Shadow);
    let mut config = pipeline_config_from_stages(vec![("", vec!["data"]), ("data", vec![])])?;
    config.types = BTreeSet::from([DerivableType::Fsnodes]);
    config.batch_size = NonZeroU64::new(1).unwrap();
    config
        .stages
        .get_mut(&MPath::new("data")?)
        .unwrap()
        .partition_cut_points = vec![b'b'];
    let order = derivation_pipeline_driver::plan_stages_and_types(
        manager,
        &config,
        &[DerivableType::Fsnodes],
    )?;
    let batches = derivation_pipeline_driver::plan_batches(
        &ctx,
        repo.repo_blobstore(),
        &config,
        vec![a, b, c, d],
    )
    .await?;
    async {
        let mut observed: Vec<std::sync::Weak<_>> = Vec::new();
        derivation_pipeline_driver::run_pipeline_batches(
            manager,
            &ctx,
            &config,
            &order.sorted_stages,
            &order.sorted_types,
            &batches,
            |outputs| {
                if !observed.is_empty() {
                    assert!(
                        observed[0].upgrade().is_some(),
                        "old merge parent was released too early"
                    );
                }
                if observed.len() == 3 {
                    assert!(
                        observed[1].upgrade().is_none(),
                        "unneeded batch was retained"
                    );
                }
                observed.push(std::sync::Arc::downgrade(&outputs[0].outputs));
            },
        )
        .await?;
        assert_eq!(observed.len(), 4);
        assert!(observed.iter().all(|output| output.upgrade().is_none()));
        anyhow::Ok(())
    }
    .await
}

#[mononoke::fbinit_test]
async fn local_driver_partitions_subtree_copy(fb: FacebookInit) -> Result<()> {
    for terminal_mapping in [false, true] {
        verify_pipeline_first_then_canonical_with_config::<NestedSubtreeCopy>(
            fb,
            partitioned_config::<NestedSubtreeCopy>()?,
            terminal_mapping,
        )
        .await?;
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn local_driver_partitions_cross_stage_directory_copy(fb: FacebookInit) -> Result<()> {
    for terminal_mapping in [false, true] {
        verify_pipeline_first_then_canonical_with_config::<CrossStageDirectoryCopy>(
            fb,
            partitioned_config::<CrossStageDirectoryCopy>()?,
            terminal_mapping,
        )
        .await?;
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn local_driver_partitions_cross_stage_file_copy(fb: FacebookInit) -> Result<()> {
    for terminal_mapping in [false, true] {
        verify_pipeline_first_then_canonical_with_config::<CrossStageFileCopy>(
            fb,
            partitioned_config::<CrossStageFileCopy>()?,
            terminal_mapping,
        )
        .await?;
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn partitions_preserve_acl_restrictions(fb: FacebookInit) -> Result<()> {
    verify_partitions::<AugmentedManifestV2AclNoHgMapping>(
        fb,
        partitioned_config::<AugmentedManifestV2AclNoHgMapping>()?,
        &[],
    )
    .await
}

macro_rules! partitioned_pipeline_tests {
    ($($name:ident: $fixture:ty),* $(,)?) => {$(
        #[mononoke::fbinit_test]
        async fn $name(fb: FacebookInit) -> Result<()> {
            let config = partitioned_config::<$fixture>()?;
            for terminal_mapping in [false, true] {
                verify_pipeline_first_then_canonical_with_config::<$fixture>(
                    fb, config.clone(), terminal_mapping,
                ).await?;
            }
            verify_batch_outputs::<$fixture>(fb).await?;
            Ok(())
        }
    )*};
}

partitioned_pipeline_tests! {
    partitioned_pipeline_nested: NestedDirectories,
    partitioned_pipeline_nested_acl: AclNestedDirectories,
    partitioned_pipeline_empty_and_replaced_stages: AugmentedManifestV2NoHgMapping,
    partitioned_pipeline_ancestor_replacement: AncestorDirectoryReplacement,
    partitioned_pipeline_absent_parent: AugmentedManifestV2AbsentParentStageNoHgMapping,
    partitioned_pipeline_identical_parents: AugmentedManifestV2DuplicateParentEntriesNoHgMapping,
    partitioned_pipeline_octopus_parents: AugmentedManifestV2P3PlusParentsNoHgMapping,
    partitioned_pipeline_cross_stage_copy: CrossStageFileCopy,
    partitioned_pipeline_directory_copy: CrossStageDirectoryCopy,
    partitioned_pipeline_deep_copy: NestedSubtreeCopy,
    partitioned_pipeline_acl: AugmentedManifestV2AclNoHgMapping,
    partitioned_pipeline_cross_range_copy: PartitionFileCopies,
}

async fn verify_merged_deletion_history(
    fb: FacebookInit,
    config: DerivationPipelineConfig,
    terminal_mapping: bool,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, commits) =
        verify_pipeline_first_then_canonical_with_config::<MergedDeletionHistory>(
            fb,
            config,
            terminal_mapping,
        )
        .await?;
    let mut labels = BTreeMap::new();
    for csid in commits {
        let bonsai = csid.load(&ctx, repo.repo_blobstore()).await?;
        labels.insert(bonsai.message().to_owned(), csid);
    }
    for &(label, entries) in MergedDeletionHistory::DELETED_MANIFESTS {
        let root = repo
            .repo_derived_data()
            .manager()
            .fetch_derived::<RootDeletedManifestV2Id>(&ctx, labels[label], None)
            .await?
            .ok_or_else(|| anyhow!("missing deleted manifest for {label}"))?;
        let mut pending = vec![(MPath::ROOT, *root.id())];
        let mut actual = BTreeMap::new();
        while let Some((path, id)) = pending.pop() {
            let node = id.load(&ctx, repo.repo_blobstore()).await?;
            actual.insert(path.clone(), node.linknode().copied());
            let mut children = node.into_subentries(&ctx, repo.repo_blobstore());
            while let Some((name, id)) = children.try_next().await? {
                pending.push((path.join(std::iter::once(&name)), id));
            }
        }
        let expected = entries
            .iter()
            .map(|(path, deleted)| {
                Ok((MPath::try_from(*path)?, deleted.map(|label| labels[label])))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        assert_eq!(actual, expected, "deletion history at {label}");
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn merged_deletion_history_paths(fb: FacebookInit) -> Result<()> {
    verify_merged_deletion_history(
        fb,
        pipeline_config_from_stages(MergedDeletionHistory::pipeline_stages())?,
        false,
    )
    .await
}

#[mononoke::fbinit_test]
async fn merged_deletion_history_paths_terminal_mapping(fb: FacebookInit) -> Result<()> {
    verify_merged_deletion_history(
        fb,
        pipeline_config_from_stages(MergedDeletionHistory::pipeline_stages())?,
        true,
    )
    .await
}

#[mononoke::fbinit_test]
async fn merged_deletion_history_partitions(fb: FacebookInit) -> Result<()> {
    verify_merged_deletion_history(fb, partitioned_config::<MergedDeletionHistory>()?, false).await
}

#[mononoke::fbinit_test]
async fn merged_deletion_history_partitions_terminal_mapping(fb: FacebookInit) -> Result<()> {
    verify_merged_deletion_history(fb, partitioned_config::<MergedDeletionHistory>()?, true).await
}

#[mononoke::fbinit_test]
async fn merged_deletion_history_batch_outputs(fb: FacebookInit) -> Result<()> {
    verify_batch_outputs::<MergedDeletionHistory>(fb).await
}

#[mononoke::fbinit_test]
async fn merged_deletion_history_partition_outputs(fb: FacebookInit) -> Result<()> {
    verify_partitions::<MergedDeletionHistory>(
        fb,
        partitioned_config::<MergedDeletionHistory>()?,
        &[],
    )
    .await
}

#[mononoke::fbinit_test]
async fn copies_within_a_partition_advance_before_assembly(fb: FacebookInit) -> Result<()> {
    async {
        for canonical_parent in [false, true] {
            let ctx = &CoreContext::test_mock(fb);
            let (repo, labels, _) = PartitionFileCopies::get_repo_and_dag::<TestRepo>(fb).await;
            let manager = &repo
                .repo_derived_data()
                .manager()
                .with_pipeline_mode(derived_data_manager::PipelineMode::Shadow);
            let mut config = partitioned_config::<PartitionFileCopies>()?;
            config.batch_size = NonZeroU64::new(1).unwrap();
            let head = labels["B"];
            let boundary = if canonical_parent {
                vec![labels["A"]]
            } else {
                vec![]
            };
            manager
                .derive_bulk_locally(ctx, &boundary, None, &PIPELINE_TYPES, None, None)
                .await?;
            let plan = plan_pipeline(
                ctx,
                &repo,
                manager,
                &config,
                &PIPELINE_TYPES,
                head,
                boundary,
            )
            .await?;
            let commits = plan
                .batches
                .iter()
                .flat_map(|batch| batch.commits.clone())
                .collect::<Vec<_>>();
            let path = MPath::new("data")?;
            let manifest = ManifestStagePayload::from_config(path.clone(), &config.stages[&path]);
            let mut references = Vec::new();
            for derived_type in &plan.sorted_types {
                let variant = derived_type.into_pipeline_derivable_variant()?;
                for range in manifest.partitions() {
                    let payload = DerivationStagePayload::Subentries {
                        path: path.clone(),
                        range,
                    };
                    for &csid in &commits {
                        derive_with_references(
                            manager,
                            ctx,
                            &mut references,
                            vec![csid],
                            &payload,
                            variant,
                        )
                        .await?;
                    }
                }
                assert!(
                    !BulkDerivation::is_stage_derived(
                        manager,
                        ctx,
                        head,
                        *derived_type,
                        &StageId::Manifest(MPath::ROOT),
                    )
                    .await?
                );
            }
            let mut references = Vec::new();
            derivation_pipeline_driver::run_pipeline_batches(
                manager,
                ctx,
                &config,
                &plan.sorted_stages,
                &plan.sorted_types,
                &plan.batches,
                |outputs| references.extend_from_slice(outputs),
            )
            .await?;
            manager
                .derive_bulk_locally(ctx, &commits, None, &PIPELINE_TYPES, None, None)
                .await?;
            let inputs = StageInputs::new(&references)?;
            verify_pipeline_output::<PartitionFileCopies>(
                &manager.with_stage_inputs(inputs),
                ctx,
                &commits,
                &config,
                &plan,
            )
            .await?;
        }
        Ok(())
    }
    .await
}

async fn verify_batch_outputs<F: PipelineTestFixture + Send>(fb: FacebookInit) -> Result<()> {
    for release_parents in [false, true] {
        let ctx = &CoreContext::test_mock(fb);
        let (repo, _, _) = F::get_repo_and_dag::<TestRepo>(fb).await;
        let manager = &repo
            .repo_derived_data()
            .manager()
            .with_pipeline_mode(if release_parents {
                derived_data_manager::PipelineMode::Production
            } else {
                derived_data_manager::PipelineMode::Shadow
            });
        let mut config = partitioned_config::<F>()?;
        config.batch_size = NonZeroU64::new(1).unwrap();
        let head = repo
            .bookmarks()
            .get(
                ctx.clone(),
                &BookmarkKey::new("master")?,
                bookmarks::Freshness::MostRecent,
            )
            .await?
            .unwrap();
        let plan =
            plan_pipeline(ctx, &repo, manager, &config, &PIPELINE_TYPES, head, vec![]).await?;
        let commits: Vec<_> = plan
            .batches
            .iter()
            .flat_map(|batch| batch.commits.clone())
            .collect();
        with_just_knobs_async(
            JustKnobsInMemory::new(HashMap::from([(
                "scm/mononoke:enable_subtree_changes".to_owned(),
                KnobVal::Bool(true),
            )])),
            async {
                let mut references = Vec::new();
                let mut verification_refs = Vec::new();
                for batch in &plan.batches {
                    let mut work = Vec::new();
                    for path in &plan.sorted_stages {
                        let manifest =
                            ManifestStagePayload::from_config(path.clone(), &config.stages[path]);
                        for &ty in &plan.sorted_types {
                            for range in manifest.partitions() {
                                work.push((
                                    ty,
                                    DerivationStagePayload::Subentries {
                                        path: path.clone(),
                                        range,
                                    },
                                ));
                            }
                            work.push((ty, DerivationStagePayload::Manifest(manifest.clone())));
                        }
                    }
                    for &ty in &plan.sorted_types {
                        if bulk_derivation::pipeline_has_finalize(
                            ty.into_pipeline_derivable_variant()?,
                        ) {
                            work.push((ty, DerivationStagePayload::Finalize));
                        }
                    }
                    for (ty, payload) in work {
                        let outputs = StageInputs::new(&references)?;
                        let supplied = manager.with_stage_inputs(outputs);
                        let result = bulk_derivation::derive_stage_batch(
                            &supplied,
                            ctx,
                            batch.commits.clone(),
                            &payload,
                            ty.into_pipeline_derivable_variant()?,
                        )
                        .await?;
                        if let Some(reference) = result.outputs {
                            for csid in &batch.commits {
                                assert!(
                                    !BulkDerivation::is_stage_derived(
                                        manager,
                                        ctx,
                                        *csid,
                                        ty,
                                        &payload.stage_id()
                                    )
                                    .await?,
                                    "intermediate output must be supplied explicitly"
                                );
                            }
                            references.push(reference.clone());
                            verification_refs.push(reference);
                        }
                    }
                    if release_parents {
                        references.clear();
                    }
                }
                if release_parents {
                    return verify_terminal_mappings::<F>(ctx, manager, &PIPELINE_TYPES, &commits)
                        .await;
                }
                manager
                    .derive_bulk_locally(ctx, &commits, None, &PIPELINE_TYPES, None, None)
                    .await?;
                let inputs = StageInputs::new(&verification_refs)?;
                verify_pipeline_output::<F>(
                    &manager.with_stage_inputs(inputs),
                    ctx,
                    &commits,
                    &config,
                    &plan,
                )
                .await
            }
            .boxed(),
        )
        .await?;
    }
    Ok(())
}
