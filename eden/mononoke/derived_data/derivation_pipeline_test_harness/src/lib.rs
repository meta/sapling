/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Shared utilities for derived-data tests: a reusable `TestRepo` facet
//! container and a harness that checks derivation-pipeline output matches
//! canonical derivation.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::num::NonZeroU64;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use blobstore::Loadable;
use bonsai_hg_mapping::BonsaiHgMapping;
use bookmarks::BookmarkKey;
use bookmarks::Bookmarks;
use bookmarks::BookmarksRef;
use bulk_derivation::BulkDerivation;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphRef;
use commit_graph::CommitGraphWriter;
use context::CoreContext;
use derived_data_manager::DerivedDataManager;
use fbinit::FacebookInit;
use filestore::FilestoreConfig;
use fixtures::TestRepoFixture;
use futures::FutureExt;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use metaconfig_types::DerivationPipelineConfig;
use metaconfig_types::DerivationPipelineStageConfig;
use metaconfig_types::RepoConfig;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use mononoke_types::MPath;
use mononoke_types::MPathElement;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedData;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentity;

/// Facet container with the attributes derivation tests need. This is the
/// shared replacement for per-crate `TestRepo` declarations.
#[facet::container]
pub struct TestRepo(
    RepoIdentity,
    dyn BonsaiHgMapping,
    dyn Bookmarks,
    CommitGraph,
    dyn CommitGraphWriter,
    RepoDerivedData,
    RepoBlobstore,
    FilestoreConfig,
    RepoConfig,
);

/// The pipeline-derivable types verified by the harness.
const PIPELINE_TYPES: [DerivableType; 11] = [
    DerivableType::Fsnodes,
    DerivableType::Unodes,
    DerivableType::Fastlog,
    DerivableType::BlameV2,
    DerivableType::SkeletonManifests,
    DerivableType::SkeletonManifestsV2,
    DerivableType::AclManifests,
    DerivableType::HgChangesets,
    DerivableType::HgAugmentedManifests,
    DerivableType::ContentManifests,
    DerivableType::DeletedManifests,
];

const PIPELINE_BATCH_SIZE: u64 = 3;

/// A fixture that additionally describes a pipeline-stage layout.
pub trait PipelineTestFixture: TestRepoFixture {
    /// Stages as `(stage_absolute_path, [dependency_child_component, ...])`.
    /// ROOT is the empty string. Each dependency component names a child stage
    /// exactly one path element deeper than this stage.
    fn pipeline_stages() -> Vec<(&'static str, Vec<&'static str>)>;
}

/// Build and validate a `DerivationPipelineConfig` from the fixture's stages.
pub fn pipeline_config_from_stages(
    stages: Vec<(&'static str, Vec<&'static str>)>,
) -> Result<DerivationPipelineConfig> {
    let stage_paths: Vec<MPath> = stages
        .iter()
        .map(|(path, _)| parse_stage_path(path))
        .collect::<Result<_>>()?;

    let stages_map = stages
        .iter()
        .zip(stage_paths.iter())
        .map(|((_, deps), stage_path)| {
            let dep_paths = deps
                .iter()
                .map(|dep| {
                    let element = MPathElement::new(dep.as_bytes().to_vec())?;
                    Ok(stage_path.join(std::iter::once(&element)))
                })
                .collect::<Result<Vec<MPath>>>()?;
            Ok((
                stage_path.clone(),
                DerivationPipelineStageConfig {
                    dependencies: dep_paths,
                },
            ))
        })
        .collect::<Result<HashMap<_, _>>>()?;

    let config = DerivationPipelineConfig {
        types: BTreeSet::from(PIPELINE_TYPES),
        bookmarks: vec![BookmarkKey::new("master")?],
        stages: stages_map,
        batch_size: NonZeroU64::new(PIPELINE_BATCH_SIZE)
            .ok_or_else(|| anyhow!("batch size must be non-zero"))?,
    };
    config.validate()?;
    Ok(config)
}

fn parse_stage_path(path: &str) -> Result<MPath> {
    if path.is_empty() {
        Ok(MPath::ROOT)
    } else {
        MPath::new(path)
    }
}

/// Verify that derivation-pipeline output matches canonical derivation for
/// every commit, type, and stage of the fixture.
pub async fn verify_pipeline_matches_canonical<F: PipelineTestFixture + Send>(
    fb: FacebookInit,
) -> Result<()> {
    // No pipeline boundary: the pipeline derives (and we verify) every commit.
    verify_pipeline_matches_canonical_impl::<F>(fb, vec![], &PIPELINE_TYPES, &PIPELINE_TYPES)
        .await
        .map(|_| ())
}

/// Like `verify_pipeline_matches_canonical`, but a prefix of the graph (the
/// ancestors of `boundary_label`, inclusive) is derived ONLY canonically and is
/// excluded from the pipeline run. The pipeline derives just the descendants of
/// the boundary, so the first pipeline batch's parents are canonical-only and
/// the manager must bridge them via `extract_stage_output_from_derived`. This
/// exercises the transitionary path that `verify_pipeline_matches_canonical`
/// never hits (there every ancestor has a stored pipeline stage output).
pub async fn verify_pipeline_matches_canonical_with_canonical_ancestors<
    F: PipelineTestFixture + Send,
>(
    fb: FacebookInit,
    boundary_label: &str,
    types: &[DerivableType],
) -> Result<()> {
    verify_pipeline_matches_canonical_with_canonical_ancestors_and_repo::<F>(
        fb,
        boundary_label,
        types,
        types,
    )
    .await
    .map(|_| ())
}

async fn verify_pipeline_matches_canonical_with_canonical_ancestors_and_repo<
    F: PipelineTestFixture + Send,
>(
    fb: FacebookInit,
    boundary_label: &str,
    pipeline_types: &[DerivableType],
    canonical_types: &[DerivableType],
) -> Result<(TestRepo, Vec<ChangesetId>)> {
    let (_repo, commits, _dag) = F::get_repo_and_dag::<TestRepo>(fb).await;
    let boundary = *commits.get(boundary_label).ok_or_else(|| {
        anyhow!(
            "fixture {} has no commit labelled {boundary_label}",
            F::REPO_NAME,
        )
    })?;
    verify_pipeline_matches_canonical_impl::<F>(fb, vec![boundary], pipeline_types, canonical_types)
        .await
}

async fn verify_pipeline_matches_canonical_impl<F: PipelineTestFixture + Send>(
    fb: FacebookInit,
    pipeline_boundary: Vec<ChangesetId>,
    pipeline_types: &[DerivableType],
    canonical_types: &[DerivableType],
) -> Result<(TestRepo, Vec<ChangesetId>)> {
    let ctx = &CoreContext::test_mock(fb);
    let (repo, _commits, _dag) = F::get_repo_and_dag::<TestRepo>(fb).await;
    let manager = repo.repo_derived_data().manager();

    let head = repo
        .bookmarks()
        .get(
            ctx.clone(),
            &BookmarkKey::new("master")?,
            bookmarks::Freshness::MostRecent,
        )
        .await?
        .ok_or_else(|| anyhow!("fixture {} has no master bookmark", F::REPO_NAME))?;

    let mut config = pipeline_config_from_stages(F::pipeline_stages())?;
    config.types = pipeline_types.iter().copied().collect();
    config.validate()?;

    // All commits as ancestors of head, oldest first. Every commit is derived
    // canonically (so boundary commits have a canonical value for the manager
    // to extract a stage output from); only the pipeline run and verification
    // are scoped to the descendants of `pipeline_boundary`.
    let mut all_commits = repo
        .commit_graph()
        .ancestors_difference(ctx, vec![head], vec![])
        .await?;
    all_commits.reverse();

    // Subtree-change derivation reads the knobs that gate manifest-altering
    // subtree changes; enable them for both canonical derivation and the
    // pipeline run. Harmless for fixtures without subtree changes.
    let run = run_derivation_and_verification::<F>(
        ctx,
        &repo,
        manager,
        &config,
        pipeline_types,
        canonical_types,
        &all_commits,
        head,
        pipeline_boundary,
    );
    with_just_knobs_async(
        JustKnobsInMemory::new(HashMap::from([(
            "scm/mononoke:enable_subtree_changes".to_string(),
            KnobVal::Bool(true),
        )])),
        run.boxed(),
    )
    .await?;
    Ok((repo, all_commits))
}

/// Like `verify_pipeline_matches_canonical`, but runs the full pipeline
/// derivation BEFORE any canonical derivation, then derives canonical and
/// verifies byte-equality. The canonical-first entry points populate every
/// canonical artifact (e.g. `bonsai_hg_mapping`) before the pipeline runs, so a
/// pipeline code path that reads a canonical artifact for an ancestor silently
/// succeeds. Running pipeline-first reproduces the production "pipeline ahead of
/// canonical" state where such a read would instead fail.
///
/// Scoped to the full graph (no canonical-only boundary): every parent is
/// pipeline-derived in an earlier batch, so the pipeline is self-sufficient.
///
/// `types` selects which types to derive and verify pipeline-first. Restrict it
/// to types whose pipeline derivation is self-sufficient (e.g. `HgChangesets`);
/// types with cross-type dependencies that read canonical data (e.g. `BlameV2`
/// reading `Unodes`) are not yet safe to run pipeline-first.
pub async fn verify_pipeline_first_then_canonical<F: PipelineTestFixture + Send>(
    fb: FacebookInit,
    types: &[DerivableType],
) -> Result<()> {
    verify_pipeline_first_then_canonical_with_repo::<F>(fb, types, false)
        .await
        .map(|_| ())
}

async fn verify_pipeline_first_then_canonical_with_repo<F: PipelineTestFixture + Send>(
    fb: FacebookInit,
    types: &[DerivableType],
    use_terminal_mapping: bool,
) -> Result<(TestRepo, Vec<ChangesetId>)> {
    let ctx = &CoreContext::test_mock(fb);
    let (repo, _commits, _dag) = F::get_repo_and_dag::<TestRepo>(fb).await;
    let manager = repo.repo_derived_data().manager();

    let head = repo
        .bookmarks()
        .get(
            ctx.clone(),
            &BookmarkKey::new("master")?,
            bookmarks::Freshness::MostRecent,
        )
        .await?
        .ok_or_else(|| anyhow!("fixture {} has no master bookmark", F::REPO_NAME))?;

    let mut config = pipeline_config_from_stages(F::pipeline_stages())?;
    config.types = types.iter().copied().collect();
    config.validate()?;

    let mut all_commits = repo
        .commit_graph()
        .ancestors_difference(ctx, vec![head], vec![])
        .await?;
    all_commits.reverse();

    let run = async {
        if !use_terminal_mapping {
            return run_pipeline_first_then_canonical::<F>(
                ctx,
                &repo,
                manager,
                &config,
                types,
                &all_commits,
                head,
            )
            .await;
        }

        let plan = plan_pipeline(ctx, &repo, manager, &config, types, head, vec![]).await?;
        run_pipeline(manager, ctx, &config, &plan).await?;

        // A separate blobstore and mapping database keep canonical derivation
        // from reusing or filling in the pipeline's terminal outputs.
        let (canonical_repo, _, _) = F::get_repo_and_dag::<TestRepo>(fb).await;
        let canonical = canonical_repo.repo_derived_data().manager();
        canonical
            .derive_bulk_locally(ctx, &all_commits, None, types, None, None)
            .await?;
        for &derived_type in types {
            let expected = BulkDerivation::fetch_derived_batch(
                canonical,
                ctx,
                &all_commits,
                None,
                derived_type,
            )
            .await?;
            let actual =
                BulkDerivation::fetch_derived_batch(manager, ctx, &all_commits, None, derived_type)
                    .await?;
            assert_eq!(expected.len(), all_commits.len());
            assert_eq!(
                actual,
                expected,
                "terminal mappings for {derived_type:?} in {}",
                F::REPO_NAME
            );
        }
        Ok(())
    };
    with_just_knobs_async(
        JustKnobsInMemory::new(HashMap::from([
            (
                "scm/mononoke:enable_subtree_changes".to_string(),
                KnobVal::Bool(true),
            ),
            (
                "scm/mononoke:derived_data_pipeline_terminal_stage_prod_mapping".to_string(),
                KnobVal::Bool(use_terminal_mapping),
            ),
        ])),
        run.boxed(),
    )
    .await?;
    Ok((repo, all_commits))
}

/// The planned, payload-agnostic pipeline work: batches (split at chokepoints),
/// the stages in deepest-first (dependency) order, the types in dependency
/// order, and the set of commits the pipeline derives (and we verify).
struct PipelinePlan {
    batches: Vec<derivation_pipeline_utils::Batch>,
    sorted_stages: Vec<MPath>,
    sorted_types: Vec<DerivableType>,
    pipeline_commits: BTreeSet<ChangesetId>,
}

/// Plan batches: topological slices split at chokepoints. With a non-empty
/// `pipeline_boundary` (passed as `common`), only descendants of the boundary
/// are planned, so the boundary commits remain canonical-only parents.
async fn plan_pipeline(
    ctx: &CoreContext,
    repo: &TestRepo,
    manager: &DerivedDataManager,
    config: &DerivationPipelineConfig,
    types: &[DerivableType],
    head: ChangesetId,
    pipeline_boundary: Vec<ChangesetId>,
) -> Result<PipelinePlan> {
    let pipeline_commits: BTreeSet<ChangesetId> = repo
        .commit_graph()
        .ancestors_difference(ctx, vec![head], pipeline_boundary.clone())
        .await?
        .into_iter()
        .collect();
    let batches: Vec<Vec<_>> = repo
        .commit_graph()
        .ancestors_difference_segment_slices(
            ctx,
            vec![head],
            pipeline_boundary,
            PIPELINE_BATCH_SIZE,
        )
        .await?
        .try_collect()
        .await?;
    let blobstore = repo.repo_blobstore();
    let bonsais = stream::iter(batches.iter().flatten().copied())
        .map(|cs_id| async move { anyhow::Ok((cs_id, cs_id.load(ctx, blobstore).await?)) })
        .buffer_unordered(100)
        .try_collect::<HashMap<_, _>>()
        .await?;
    let batches =
        derivation_pipeline_utils::split_batches_at_chokepoints(batches, &bonsais, config);
    let derivation_pipeline_driver::PipelineOrder {
        sorted_stages,
        sorted_types,
    } = derivation_pipeline_driver::plan_stages_and_types(manager, config, types);
    Ok(PipelinePlan {
        batches,
        sorted_stages,
        sorted_types,
        pipeline_commits,
    })
}

/// Execute the pipeline synchronously in dependency-and-ancestor order so every
/// required input is already stored before it is read. Delegates to the shared
/// `derivation_pipeline_driver` so the harness and the production backfill
/// worker exercise the same orchestration.
async fn run_pipeline(
    manager: &DerivedDataManager,
    ctx: &CoreContext,
    config: &DerivationPipelineConfig,
    plan: &PipelinePlan,
) -> Result<()> {
    derivation_pipeline_driver::run_pipeline_batches(
        manager,
        ctx,
        config,
        &plan.sorted_stages,
        &plan.sorted_types,
        &plan.batches,
    )
    .await
    .map_err(anyhow::Error::from)
}

/// Assert pipeline output matches canonical for every pipeline-derived
/// commit/type/stage. Commits outside `plan.pipeline_commits` (a canonical-only
/// boundary and its ancestors) have no pipeline stage outputs and are skipped.
async fn verify_pipeline_output<F: PipelineTestFixture>(
    manager: &DerivedDataManager,
    ctx: &CoreContext,
    all_commits: &[ChangesetId],
    plan: &PipelinePlan,
) -> Result<()> {
    let mut mismatches = Vec::new();
    for &cs_id in all_commits {
        if !plan.pipeline_commits.contains(&cs_id) {
            continue;
        }
        for stage_path in &plan.sorted_stages {
            for &derivable_type in &plan.sorted_types {
                let matches = BulkDerivation::verify_stage_output(
                    manager,
                    ctx,
                    cs_id,
                    derivable_type,
                    &derived_data_manager::StageId::Manifest(stage_path.clone()),
                )
                .await?;
                if !matches {
                    mismatches.push(format!(
                        "cs_id={cs_id} type={derivable_type:?} stage={stage_path:?}"
                    ));
                }
            }
        }

        // Verify the finalize stage for types that have one.
        for &derivable_type in &plan.sorted_types {
            let variant = derivable_type.into_pipeline_derivable_variant()?;
            if !bulk_derivation::pipeline_has_finalize(variant) {
                continue;
            }
            let matches = BulkDerivation::verify_stage_output(
                manager,
                ctx,
                cs_id,
                derivable_type,
                &derived_data_manager::StageId::Finalize,
            )
            .await?;
            if !matches {
                mismatches.push(format!(
                    "cs_id={cs_id} type={derivable_type:?} stage=Finalize"
                ));
            }
        }
    }
    if !mismatches.is_empty() {
        bail!(
            "pipeline output diverged from canonical: fixture={}\n{}",
            F::REPO_NAME,
            mismatches.join("\n"),
        );
    }
    Ok(())
}

async fn run_derivation_and_verification<F: PipelineTestFixture + Send>(
    ctx: &CoreContext,
    repo: &TestRepo,
    manager: &DerivedDataManager,
    config: &DerivationPipelineConfig,
    pipeline_types: &[DerivableType],
    canonical_types: &[DerivableType],
    all_commits: &[ChangesetId],
    head: ChangesetId,
    pipeline_boundary: Vec<ChangesetId>,
) -> Result<()> {
    // Derive canonically for every type and commit.
    manager
        .derive_bulk_locally(ctx, all_commits, None, canonical_types, None, None)
        .await
        .map_err(anyhow::Error::from)?;

    let plan = plan_pipeline(
        ctx,
        repo,
        manager,
        config,
        pipeline_types,
        head,
        pipeline_boundary,
    )
    .await?;
    run_pipeline(manager, ctx, config, &plan).await?;
    verify_pipeline_output::<F>(manager, ctx, all_commits, &plan).await
}

/// Run the full pipeline before any canonical derivation, then derive canonical
/// and verify. See `verify_pipeline_first_then_canonical`.
async fn run_pipeline_first_then_canonical<F: PipelineTestFixture + Send>(
    ctx: &CoreContext,
    repo: &TestRepo,
    manager: &DerivedDataManager,
    config: &DerivationPipelineConfig,
    types: &[DerivableType],
    all_commits: &[ChangesetId],
    head: ChangesetId,
) -> Result<()> {
    // Plan and run the pipeline BEFORE deriving canonical. Over the full graph
    // every parent is pipeline-derived in an earlier batch, so no canonical data
    // is required for the pipeline to complete.
    let plan = plan_pipeline(ctx, repo, manager, config, types, head, vec![]).await?;
    run_pipeline(manager, ctx, config, &plan).await?;

    // Now derive canonical for the selected types, then verify byte-equality.
    manager
        .derive_bulk_locally(ctx, all_commits, None, types, None, None)
        .await
        .map_err(anyhow::Error::from)?;
    verify_pipeline_output::<F>(manager, ctx, all_commits, &plan).await
}

#[cfg(test)]
mod tests {
    use bonsai_hg_mapping::BonsaiHgMappingRef;
    use fixtures::AclNestedDirectories;
    use fixtures::AncestorDirectoryReplacement;
    use fixtures::AugmentedManifestSuppliedRoot;
    use fixtures::AugmentedManifestSuppliedRootAcl;
    use fixtures::AugmentedManifestV2AbsentParentStageNoHgMapping;
    use fixtures::AugmentedManifestV2AclNoHgMapping;
    use fixtures::AugmentedManifestV2DuplicateParentEntriesNoHgMapping;
    use fixtures::AugmentedManifestV2NoHgMapping;
    use fixtures::AugmentedManifestV2P3PlusParentsNoHgMapping;
    use fixtures::CrossStageDirectoryCopy;
    use fixtures::CrossStageFileCopy;
    use fixtures::NestedDirectories;
    use fixtures::NestedSubtreeCopy;
    use mononoke_macros::mononoke;

    use super::*;

    /// Types verified pipeline-first (pipeline derived before any canonical
    /// derivation; see `verify_pipeline_first_then_canonical`). A type belongs
    /// here only if its pipeline derivation never reads canonical data that the
    /// pipeline-ahead-of-canonical state would leave absent.
    ///
    /// `HgChangesets` qualifies: its only canonical read is the cross-stage
    /// copy-source parent's hg, now resolved from the pipeline terminal stage
    /// output (canonical fallback only for genuinely pre-pipeline parents).
    ///
    /// Excluded for now: `BlameV2` and `Fastlog` resolve their `Unodes`
    /// dependency from canonical derived data. That is safe in production because
    /// `Unodes` has been fully canonically derived (prod mapping) for a long
    /// time, so it is never absent there — but it makes them fail the artificial
    /// pipeline-first harness that withholds canonical. Add a type here once its
    /// pipeline derivation is self-sufficient for every dependency still in the
    /// transitionary state.
    const PIPELINE_FIRST_TYPES: [DerivableType; 1] = [DerivableType::HgChangesets];
    const AUGMENTED_MANIFEST_TYPES: [DerivableType; 2] = [
        DerivableType::AclManifests,
        DerivableType::HgAugmentedManifests,
    ];

    impl PipelineTestFixture for NestedDirectories {
        fn pipeline_stages() -> Vec<(&'static str, Vec<&'static str>)> {
            vec![
                ("", vec!["top1", "top2", "top3"]),
                ("top1", vec![]),
                ("top2", vec!["nested1", "nested2"]),
                ("top2/nested1", vec![]),
                ("top2/nested2", vec![]),
                ("top3", vec![]),
            ]
        }
    }

    impl PipelineTestFixture for AugmentedManifestV2NoHgMapping {
        fn pipeline_stages() -> Vec<(&'static str, Vec<&'static str>)> {
            vec![
                ("", vec!["top1", "top2"]),
                ("top1", vec!["nested"]),
                ("top1/nested", vec![]),
                ("top2", vec![]),
            ]
        }
    }

    impl PipelineTestFixture for AugmentedManifestV2DuplicateParentEntriesNoHgMapping {
        fn pipeline_stages() -> Vec<(&'static str, Vec<&'static str>)> {
            vec![
                ("", vec!["top1", "top2"]),
                ("top1", vec![]),
                ("top2", vec![]),
            ]
        }
    }

    impl PipelineTestFixture for AugmentedManifestV2AbsentParentStageNoHgMapping {
        fn pipeline_stages() -> Vec<(&'static str, Vec<&'static str>)> {
            vec![
                ("", vec!["top1", "top2"]),
                ("top1", vec![]),
                ("top2", vec![]),
            ]
        }
    }

    impl PipelineTestFixture for AugmentedManifestV2P3PlusParentsNoHgMapping {
        fn pipeline_stages() -> Vec<(&'static str, Vec<&'static str>)> {
            vec![("", vec!["top1"]), ("top1", vec![])]
        }
    }

    impl PipelineTestFixture for AclNestedDirectories {
        fn pipeline_stages() -> Vec<(&'static str, Vec<&'static str>)> {
            vec![
                ("", vec!["top1", "top2"]),
                ("top1", vec!["sub"]),
                ("top1/sub", vec![]),
                ("top2", vec![]),
            ]
        }
    }

    impl PipelineTestFixture for AncestorDirectoryReplacement {
        fn pipeline_stages() -> Vec<(&'static str, Vec<&'static str>)> {
            vec![("", vec!["a"]), ("a", vec!["b"]), ("a/b", vec![])]
        }
    }

    impl PipelineTestFixture for AugmentedManifestV2AclNoHgMapping {
        fn pipeline_stages() -> Vec<(&'static str, Vec<&'static str>)> {
            vec![
                ("", vec!["top1"]),
                ("top1", vec!["docs"]),
                ("top1/docs", vec![]),
            ]
        }
    }

    impl PipelineTestFixture for AugmentedManifestSuppliedRoot {
        fn pipeline_stages() -> Vec<(&'static str, Vec<&'static str>)> {
            vec![("", vec!["dir", "new"]), ("dir", vec![]), ("new", vec![])]
        }
    }

    impl PipelineTestFixture for AugmentedManifestSuppliedRootAcl {
        fn pipeline_stages() -> Vec<(&'static str, Vec<&'static str>)> {
            vec![
                ("", vec!["dir", "new", "restricted"]),
                ("dir", vec![]),
                ("new", vec![]),
                ("restricted", vec![]),
            ]
        }
    }

    impl PipelineTestFixture for NestedSubtreeCopy {
        fn pipeline_stages() -> Vec<(&'static str, Vec<&'static str>)> {
            vec![
                ("", vec!["top1", "top2"]),
                ("top1", vec![]),
                ("top2", vec![]),
            ]
        }
    }

    impl PipelineTestFixture for CrossStageDirectoryCopy {
        fn pipeline_stages() -> Vec<(&'static str, Vec<&'static str>)> {
            vec![
                ("", vec!["top1", "top2"]),
                ("top1", vec![]),
                ("top2", vec![]),
            ]
        }
    }

    impl PipelineTestFixture for CrossStageFileCopy {
        fn pipeline_stages() -> Vec<(&'static str, Vec<&'static str>)> {
            vec![
                ("", vec!["top1", "top2"]),
                ("top1", vec![]),
                ("top2", vec![]),
            ]
        }
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_matches_canonical(fb: FacebookInit) -> Result<()> {
        verify_pipeline_matches_canonical::<NestedDirectories>(fb).await
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_matches_canonical_acl(fb: FacebookInit) -> Result<()> {
        verify_pipeline_matches_canonical::<AclNestedDirectories>(fb).await
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_matches_canonical_ancestor_directory_replacement(
        fb: FacebookInit,
    ) -> Result<()> {
        verify_pipeline_matches_canonical::<AncestorDirectoryReplacement>(fb).await
    }

    async fn verify_augmented_manifest_pipeline_first_without_hg_mapping<
        F: PipelineTestFixture + Send,
    >(
        fb: FacebookInit,
    ) -> Result<()> {
        let ctx = &CoreContext::test_mock(fb);
        let (repo, commits) = verify_pipeline_first_then_canonical_with_repo::<F>(
            fb,
            &AUGMENTED_MANIFEST_TYPES,
            false,
        )
        .await?;

        assert!(
            repo.bonsai_hg_mapping()
                .get(ctx, commits.into())
                .await?
                .is_empty(),
            "pipeline and canonical augmented manifest derivation should not create Bonsai-Hg mappings for {}",
            F::REPO_NAME,
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_first_augmented_manifest_without_hg_mapping(
        fb: FacebookInit,
    ) -> Result<()> {
        verify_augmented_manifest_pipeline_first_without_hg_mapping::<
            AugmentedManifestV2NoHgMapping,
        >(fb)
        .await
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_first_augmented_manifest_duplicate_parent_entries(
        fb: FacebookInit,
    ) -> Result<()> {
        verify_augmented_manifest_pipeline_first_without_hg_mapping::<
            AugmentedManifestV2DuplicateParentEntriesNoHgMapping,
        >(fb)
        .await
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_first_augmented_manifest_absent_parent_stage(
        fb: FacebookInit,
    ) -> Result<()> {
        verify_augmented_manifest_pipeline_first_without_hg_mapping::<
            AugmentedManifestV2AbsentParentStageNoHgMapping,
        >(fb)
        .await
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_first_augmented_manifest_p3_plus_parents(
        fb: FacebookInit,
    ) -> Result<()> {
        verify_augmented_manifest_pipeline_first_without_hg_mapping::<
            AugmentedManifestV2P3PlusParentsNoHgMapping,
        >(fb)
        .await
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_first_hg_parent_identity(fb: FacebookInit) -> Result<()> {
        verify_pipeline_first_then_canonical::<AugmentedManifestV2AbsentParentStageNoHgMapping>(
            fb,
            &PIPELINE_FIRST_TYPES,
        )
        .await?;
        verify_pipeline_first_then_canonical::<AugmentedManifestV2DuplicateParentEntriesNoHgMapping>(
            fb, &PIPELINE_FIRST_TYPES,
        ).await?;
        verify_pipeline_first_then_canonical::<AugmentedManifestV2P3PlusParentsNoHgMapping>(
            fb,
            &PIPELINE_FIRST_TYPES,
        )
        .await
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_first_hg_stage_transitions(fb: FacebookInit) -> Result<()> {
        verify_pipeline_first_then_canonical::<NestedDirectories>(fb, &PIPELINE_FIRST_TYPES)
            .await?;
        verify_pipeline_first_then_canonical::<AugmentedManifestV2NoHgMapping>(
            fb,
            &PIPELINE_FIRST_TYPES,
        )
        .await
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_terminal_mappings(fb: FacebookInit) -> Result<()> {
        verify_pipeline_first_then_canonical_with_repo::<NestedDirectories>(
            fb,
            &PIPELINE_TYPES,
            true,
        )
        .await?;
        verify_pipeline_first_then_canonical_with_repo::<AugmentedManifestV2NoHgMapping>(
            fb,
            &PIPELINE_FIRST_TYPES,
            true,
        )
        .await?;
        verify_pipeline_first_then_canonical_with_repo::<CrossStageFileCopy>(
            fb,
            &PIPELINE_FIRST_TYPES,
            true,
        )
        .await?;
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_augmented_manifest_with_canonical_ancestor(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given: commit A and its ancestors are derived only canonically.
        let ctx = &CoreContext::test_mock(fb);

        // When: the pipeline starts at A's children with exactly ACL and augmented manifests.
        let (repo, commits) =
            verify_pipeline_matches_canonical_with_canonical_ancestors_and_repo::<
                AugmentedManifestV2NoHgMapping,
            >(
                fb,
                "A",
                &AUGMENTED_MANIFEST_TYPES,
                &AUGMENTED_MANIFEST_TYPES,
            )
            .await?;

        // Then: the pipeline extracts A's stage entries and remains
        // canonical-equivalent without creating a Bonsai-Hg mapping.
        assert!(
            repo.bonsai_hg_mapping()
                .get(ctx, commits.into())
                .await?
                .is_empty(),
            "canonical and pipeline augmented manifests should not create Bonsai-Hg mappings",
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_first_augmented_manifest_empty_root_terminal_publication(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given: both forms of an empty repository root.
        // When: the pipeline publishes terminal stages to the production mapping.
        // Then: each empty root is a directory that terminal storage accepts.
        verify_pipeline_first_then_canonical_with_repo::<AugmentedManifestV2NoHgMapping>(
            fb,
            &AUGMENTED_MANIFEST_TYPES,
            true,
        )
        .await
        .map(|_| ())
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_first_augmented_manifest_non_root_acl(fb: FacebookInit) -> Result<()> {
        // Given: nested ACL stages, a no-change merge that rebuilds directories,
        // and a final commit that removes the repository's only ACL.
        // When: ACL and augmented manifests run pipeline-first with ACL pointers enabled.
        // Then: every stage and terminal root matches canonical derivation.
        verify_augmented_manifest_pipeline_first_without_hg_mapping::<
            AugmentedManifestV2AclNoHgMapping,
        >(fb)
        .await
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_augmented_manifest_reuses_supplied_root(fb: FacebookInit) -> Result<()> {
        // Given: commit S, whose Hg root is stored under a client-supplied id,
        // derived only canonically, and descendants for which Hg reuses it.
        // When: the pipeline derives S's descendants from Bonsai.
        // Then: every stage, including each root, matches canonical
        // derivation from the Hg manifests.
        verify_pipeline_matches_canonical_with_canonical_ancestors::<AugmentedManifestSuppliedRoot>(
            fb,
            "S",
            &AUGMENTED_MANIFEST_TYPES,
        )
        .await
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_augmented_manifest_reuses_supplied_root_with_acl(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given: the supplied-root history with an ACL restriction root.
        // When: the pipeline derives S's descendants from Bonsai.
        // Then: every stage, including each root and its ACL pointer, matches
        // canonical derivation from the Hg manifests.
        verify_pipeline_matches_canonical_with_canonical_ancestors::<
            AugmentedManifestSuppliedRootAcl,
        >(fb, "S", &AUGMENTED_MANIFEST_TYPES)
        .await
    }

    // Boundary commit `E` is derived canonically only; the pipeline derives just
    // its descendants. The first pipeline batch's parents include `E`, which has
    // no pipeline stage output, so the manager bridges it via
    // `extract_stage_output_from_derived`.
    #[mononoke::fbinit_test]
    async fn test_pipeline_matches_canonical_acl_with_canonical_ancestors(
        fb: FacebookInit,
    ) -> Result<()> {
        verify_pipeline_matches_canonical_with_canonical_ancestors::<AclNestedDirectories>(
            fb,
            "E",
            &PIPELINE_TYPES,
        )
        .await
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_matches_canonical_subtree_copy(fb: FacebookInit) -> Result<()> {
        verify_pipeline_matches_canonical::<NestedSubtreeCopy>(fb).await
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_augmented_manifest_subtree_copy(fb: FacebookInit) -> Result<()> {
        // Given: a subtree copy whose source and destination are separate stages.
        // When: ACL and augmented manifests run through the existing global chokepoint.
        // Then: every stage matches canonical without scheduling HgChangesets.
        verify_pipeline_matches_canonical_impl::<NestedSubtreeCopy>(
            fb,
            vec![],
            &AUGMENTED_MANIFEST_TYPES,
            &AUGMENTED_MANIFEST_TYPES,
        )
        .await
        .map(|_| ())
    }

    // Regression coverage for cross-stage copy-source resolution: the tip commit
    // carries copies whose `copy_from` sources are a directory and a missing path
    // outside the destination's stage. Both canonical and pipeline drop the bogus
    // copies in `resolve_cross_stage_copy_sources`, staying byte-identical.
    #[mononoke::fbinit_test]
    async fn test_pipeline_cross_stage_directory_copy(fb: FacebookInit) -> Result<()> {
        verify_pipeline_matches_canonical::<CrossStageDirectoryCopy>(fb).await
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_augmented_manifest_drops_invalid_copy_sources(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given: cross-stage copies whose source is a directory or is missing.
        // When: ACL and augmented manifests derive the fixture through the pipeline.
        // Then: the pipeline drops the invalid copy metadata and matches canonical output.
        verify_pipeline_matches_canonical_impl::<CrossStageDirectoryCopy>(
            fb,
            vec![],
            &AUGMENTED_MANIFEST_TYPES,
            &AUGMENTED_MANIFEST_TYPES,
        )
        .await
        .map(|_| ())
    }

    // Success path of cross-stage copy resolution: a real file copied across a
    // stage boundary, canonical-first.
    #[mononoke::fbinit_test]
    async fn test_pipeline_cross_stage_file_copy(fb: FacebookInit) -> Result<()> {
        verify_pipeline_matches_canonical::<CrossStageFileCopy>(fb).await
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_first_augmented_manifest_copy_sources(fb: FacebookInit) -> Result<()> {
        // Given: stage-local, file-valued stage-root, and cross-stage copies.
        // When: ACL and augmented manifests run pipeline-first across the stage-shape transitions.
        // Then: every stage matches canonical derivation without a Bonsai-Hg mapping.
        verify_augmented_manifest_pipeline_first_without_hg_mapping::<NestedDirectories>(fb).await
    }

    #[mononoke::fbinit_test]
    async fn test_pipeline_augmented_manifest_copy_from_canonical_parent(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given: the copy source parent exists only in canonical derived data.
        // When: the pipeline derives the cross-stage copy in its child.
        // Then: the pipeline falls back to the canonical parent root and remains equivalent.
        verify_pipeline_matches_canonical_with_canonical_ancestors::<CrossStageFileCopy>(
            fb,
            "P",
            &AUGMENTED_MANIFEST_TYPES,
        )
        .await
    }

    // Pipeline-first: derives the pipeline before any canonical derivation, so a
    // cross-stage copy whose source parent is not yet canonically derived must
    // still resolve. Regression guard for the `bonsai_hg_mapping` dependency in
    // `resolve_cross_stage_copy_sources`. Scoped to `PIPELINE_FIRST_TYPES`.
    #[mononoke::fbinit_test]
    async fn test_pipeline_first_cross_stage_file_copy(fb: FacebookInit) -> Result<()> {
        verify_pipeline_first_then_canonical::<CrossStageFileCopy>(fb, &PIPELINE_FIRST_TYPES).await
    }

    // Boundary commit `D` is derived canonically only; the pipeline derives just
    // its descendants (E, F, G, H, I, J, K, L, M, N, R, O, P, Q). The first pipeline batch's
    // parents include `D`, which has no pipeline stage output, so the manager must
    // bridge it via `extract_stage_output_from_derived`. The bridge is genuinely
    // used: `D` introduced `top2/nested1`, and the descendants leave that subtree
    // unchanged (they work in `top2/nested2`, `top1`, `top3`, and the root), so
    // the `top2`/`top2/nested1` stage outputs of the children are inherited from
    // `D`'s canonical value rather than recomputed from a stored pipeline stage.
    #[mononoke::fbinit_test]
    async fn test_pipeline_matches_canonical_with_canonical_ancestors(
        fb: FacebookInit,
    ) -> Result<()> {
        verify_pipeline_matches_canonical_with_canonical_ancestors::<NestedDirectories>(
            fb,
            "D",
            &PIPELINE_TYPES,
        )
        .await
    }
}
