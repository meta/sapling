/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Hg changeset derivation at each value of
//! `scm/mononoke:hgmanifest_retirement_stage`. At 1 it must give the Hg
//! changeset ids and Hg tree bytes that 0 gives, without reading Hg manifests.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::num::NonZeroU64;

use ::manifest::Entry;
use ::manifest::ManifestOps;
use anyhow::Result;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bookmarks::BookmarkKey;
use bytes::Bytes;
use commit_graph::CommitGraphRef;
use context::CoreContext;
use derivation_pipeline_driver::PipelineOrder;
use derivation_pipeline_driver::plan_batches;
use derivation_pipeline_driver::plan_stages_and_types;
use derivation_pipeline_driver::run_pipeline_batches;
use derivation_queue_thrift::DerivationPriority;
use derived_data_manager::BonsaiDerivable;
use derived_data_manager::ChildNameRange;
use derived_data_manager::DerivationStagePayload;
use derived_data_manager::ManifestStageDependencies;
use derived_data_manager::ManifestStagePayload;
use derived_data_manager::PipelineDerivable;
use derived_data_manager::PipelineMode;
use derived_data_manager::StageId;
use derived_data_manager::stage_outputs::StageOutput;
use fbinit::FacebookInit;
use fixtures::TestRepoFixture;
use futures::TryStreamExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::override_just_knobs;
use mercurial_derivation::DeriveHgChangeset;
use mercurial_derivation::MappedHgChangesetId;
use mercurial_derivation::RootHgAugmentedManifestId;
use mercurial_types::HgChangesetId;
use mercurial_types::HgManifestId;
use metaconfig_types::DerivationPipelineConfig;
use metaconfig_types::DerivationPipelineStageConfig;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use mononoke_types::MPath;
use mononoke_types::MPathElement;
use mononoke_types::NonRootMPath;
use mononoke_types::RepoPath;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataRef;
use restricted_paths::ManifestType;
use tests_utils::CreateCommitContext;
use tests_utils::drawdag::extend_from_dag_with_actions;

use crate::Repo;
use crate::augmented_manifest_tests::build_repo_with_restricted_path_config;
use crate::augmented_manifest_tests::create_restricted_path_test_stack;
use crate::augmented_manifest_tests::restricted_path_entries;
use crate::augmented_manifest_tests::restricted_paths_access_logging_knobs;
use crate::hg_manifest_read_check_test::repo_recording_labels;

const STAGE_KNOB: &str = "scm/mononoke:hgmanifest_retirement_stage";

fn set_stage(stage: i64) {
    override_just_knobs(JustKnobsInMemory::new(HashMap::from([(
        STAGE_KNOB.to_string(),
        KnobVal::Int(stage),
    )])));
}

/// Commits covering each kind of change: a merge with resolved files and a
/// directory equal to the second parent's, a rename, a copy, a file type
/// change, a deleted directory, a file replaced by a directory, an empty
/// commit, a symlink, a commit deleting every file, and a commit adding files
/// to the empty tree.
const CHANGE_KINDS_DAG: &str = r#"
    # default_files: false
    # author: * "author"

    J     # modify: J "fresh.txt" "fresh\n"
    |
    I     # delete: I "left/l.txt"
    |     # delete: I "right/r.txt"
    |     # delete: I "other/o.txt"
    |     # delete: I "dir2/b.txt"
    |     # delete: I "run.sh"
    |     # delete: I "a.txt/inner"
    |     # delete: I "link"
    |
    H     # modify: H "link" "symlink" "a.txt/inner"
    |
    G
    |
    F     # delete: F "a.txt"
    |     # modify: F "a.txt/inner" "inner\n"
    |
    E     # delete: E "dir/sub/c.txt"
    |     # delete: E "dir/sub/a_copy.txt"
    |
    D     # copy: D "dir/sub/a_copy.txt" "a-left\n" C "a.txt"
    |     # modify: D "run.sh" "exec" "echo\n"
    |
    C     # copy: C "dir2/b.txt" "b\n" M "dir/b.txt"
    |     # delete: C "dir/b.txt"
    |
    M     # modify: M "a.txt" "a-left\n"
    |\    # modify: M "other/o.txt" "o-right\n"
    | \   # modify: M "left/l.txt" "l\n"
    |  |  # modify: M "right/r.txt" "r\n"
    |  |
    B  R  # modify: B "a.txt" "a-left\n"
    | /   # modify: B "left/l.txt" "l\n"
    |/    # modify: R "other/o.txt" "o-right\n"
    |     # modify: R "right/r.txt" "r\n"
    |
    A     # modify: A "a.txt" "a\n"
          # modify: A "dir/b.txt" "b\n"
          # modify: A "dir/sub/c.txt" "c\n"
          # modify: A "run.sh" "echo\n"
          # modify: A "other/o.txt" "o\n"
"#;

struct ChangeKinds;

#[async_trait::async_trait]
impl TestRepoFixture for ChangeKinds {
    const REPO_NAME: &'static str = "change_kinds";
    const DAG: &'static str = CHANGE_KINDS_DAG;
}

/// A root commit with no files, and a child that adds one.
const EMPTY_ROOT_DAG: &str = r#"
    # default_files: false
    # author: * "author"

    B     # modify: B "file" "b\n"
    |
    A
"#;

struct EmptyRoot;

#[async_trait::async_trait]
impl TestRepoFixture for EmptyRoot {
    const REPO_NAME: &'static str = "empty_root";
    const DAG: &'static str = EMPTY_ROOT_DAG;
}

/// What deriving every commit's Hg changeset produced: each commit's Hg
/// changeset id, and the stored bytes of every Hg tree they reach.
#[derive(Debug, PartialEq)]
struct HgDerivation {
    changesets: BTreeMap<String, HgChangesetId>,
    trees: BTreeMap<HgManifestId, Bytes>,
}

async fn read_hg_trees(
    ctx: &CoreContext,
    repo: &Repo,
    changesets: &BTreeMap<String, HgChangesetId>,
) -> Result<BTreeMap<HgManifestId, Bytes>> {
    let blobstore = repo.repo_blobstore().clone();
    let mut trees = BTreeMap::new();
    for hg_cs_id in changesets.values() {
        let root = hg_cs_id.load(ctx, &blobstore).await?.manifestid();
        let ids: Vec<_> = root
            .list_all_entries(ctx.clone(), blobstore.clone())
            .try_filter_map(|(_path, entry)| async move {
                Ok(match entry {
                    Entry::Tree(id) => Some(id),
                    Entry::Leaf(_) => None,
                })
            })
            .try_collect()
            .await?;
        for id in ids {
            let bytes = blobstore
                .get(ctx, &id.blobstore_key())
                .await?
                .unwrap_or_else(|| panic!("Hg tree {id} should be stored"))
                .into_raw_bytes();
            trees.insert(id, bytes);
        }
    }
    Ok(trees)
}

/// Builds `F` at `stage` in a new repo and derives every commit's Hg
/// changeset. Fixtures that use the default `init_repo` derive them in name
/// order; this derives any left, one at a time, parents first.
async fn derive_fixture<F: TestRepoFixture + Send + Sync>(
    fb: FacebookInit,
    stage: i64,
) -> Result<HgDerivation> {
    set_stage(stage);
    let ctx = CoreContext::test_mock(fb);
    let (repo, reads) = repo_recording_labels(fb).await?;
    let (commits, _dag) = F::init_repo(fb, &repo).await?;
    // A fixture can give one commit several names, such as a bookmark's.
    let mut names: HashMap<ChangesetId, Vec<String>> = HashMap::new();
    for (name, cs_id) in &commits {
        names.entry(*cs_id).or_default().push(name.clone());
    }
    let mut parents_first = repo
        .commit_graph()
        .ancestors_difference(&ctx, commits.values().copied().collect(), vec![])
        .await?;
    parents_first.reverse();
    let mut changesets = BTreeMap::new();
    for cs_id in parents_first {
        let hg_cs_id = repo.derive_hg_changeset(&ctx, cs_id).await?;
        for name in names.get(&cs_id).into_iter().flatten() {
            changesets.insert(name.clone(), hg_cs_id);
        }
    }
    if stage == 1 {
        assert_eq!(
            reads.take(),
            vec![],
            "{}: stage 1 should read no Hg manifest",
            F::REPO_NAME
        );
    }
    let trees = read_hg_trees(&ctx, &repo, &changesets).await?;
    Ok(HgDerivation { changesets, trees })
}

async fn assert_fixture_same_at_stage_1<F: TestRepoFixture + Send + Sync>(
    fb: FacebookInit,
) -> Result<()> {
    let from_hg = derive_fixture::<F>(fb, 0).await?;
    let from_augmented = derive_fixture::<F>(fb, 1).await?;
    assert!(
        !from_hg.trees.is_empty(),
        "{} should have Hg trees",
        F::REPO_NAME
    );
    assert_eq!(
        from_augmented,
        from_hg,
        "{}: stage 1 should give the Hg changesets and Hg tree bytes of stage 0",
        F::REPO_NAME,
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_stage_1_matches_stage_0_on_each_kind_of_change(fb: FacebookInit) -> Result<()> {
    assert_fixture_same_at_stage_1::<ChangeKinds>(fb).await?;
    assert_fixture_same_at_stage_1::<EmptyRoot>(fb).await
}

#[mononoke::fbinit_test]
async fn test_stage_1_matches_stage_0_on_fixtures(fb: FacebookInit) -> Result<()> {
    assert_fixture_same_at_stage_1::<fixtures::Linear>(fb).await?;
    assert_fixture_same_at_stage_1::<fixtures::BranchEven>(fb).await?;
    assert_fixture_same_at_stage_1::<fixtures::BranchUneven>(fb).await?;
    assert_fixture_same_at_stage_1::<fixtures::BranchWide>(fb).await?;
    assert_fixture_same_at_stage_1::<fixtures::MergeEven>(fb).await?;
    assert_fixture_same_at_stage_1::<fixtures::MergeUneven>(fb).await?;
    assert_fixture_same_at_stage_1::<fixtures::MergeMultipleFiles>(fb).await?;
    assert_fixture_same_at_stage_1::<fixtures::UnsharedMergeEven>(fb).await?;
    assert_fixture_same_at_stage_1::<fixtures::UnsharedMergeUneven>(fb).await?;
    assert_fixture_same_at_stage_1::<fixtures::ManyDiamonds>(fb).await?;
    assert_fixture_same_at_stage_1::<fixtures::ManyFilesDirs>(fb).await?;
    assert_fixture_same_at_stage_1::<fixtures::NestedDirectories>(fb).await?;
    assert_fixture_same_at_stage_1::<fixtures::AugmentedManifestV2P3PlusParentsNoHgMapping>(fb)
        .await?;
    assert_fixture_same_at_stage_1::<fixtures::CrossStageDirectoryCopy>(fb).await?;
    assert_fixture_same_at_stage_1::<fixtures::CrossStageFileCopy>(fb).await?;
    assert_fixture_same_at_stage_1::<fixtures::NestedSubtreeCopy>(fb).await?;
    assert_fixture_same_at_stage_1::<fixtures::AclNestedDirectories>(fb).await?;
    assert_fixture_same_at_stage_1::<fixtures::AncestorDirectoryReplacement>(fb).await
}

#[mononoke::fbinit_test]
async fn test_stage_1_batch_matches_one_at_a_time_and_stage_0(fb: FacebookInit) -> Result<()> {
    // Given: every commit's Hg changeset derived one at a time at stage 0 and
    // at stage 1.
    let from_hg = derive_fixture::<ChangeKinds>(fb, 0).await?;
    let one_at_a_time = derive_fixture::<ChangeKinds>(fb, 1).await?;

    // When: deriving the same commits at stage 1 as one batch, after their
    // augmented manifests.
    set_stage(1);
    let ctx = CoreContext::test_mock(fb);
    let (repo, reads) = repo_recording_labels(fb).await?;
    let (commits, _dag) = extend_from_dag_with_actions(&ctx, &repo, CHANGE_KINDS_DAG).await?;
    let manager = repo.repo_derived_data().manager();
    let csids: Vec<ChangesetId> = ["A", "B", "R", "M", "C", "D", "E", "F", "G", "H", "I", "J"]
        .into_iter()
        .map(|name| commits[name])
        .collect();
    repo.repo_derived_data()
        .derive::<RootHgAugmentedManifestId>(
            &ctx,
            commits["J"],
            derivation_queue_thrift::DerivationPriority::LOW,
        )
        .await?;
    reads.take();
    let derived = manager
        .derive_exactly_batch::<MappedHgChangesetId>(&ctx, csids, None)
        .await;
    let batch_reads = reads.take().len();
    derived?;

    // Then: the batch read no Hg manifest and gave the same Hg changesets and
    // Hg trees.
    assert_eq!(batch_reads, 0, "stage 1 should read no Hg manifest");
    let mut changesets = BTreeMap::new();
    for (name, cs_id) in commits {
        changesets.insert(name, repo.derive_hg_changeset(&ctx, cs_id).await?);
    }
    let trees = read_hg_trees(&ctx, &repo, &changesets).await?;
    let batch = HgDerivation { changesets, trees };
    assert_eq!(
        batch, one_at_a_time,
        "a batch should derive what one at a time does"
    );
    assert_eq!(batch, from_hg, "stage 1 should derive what stage 0 does");
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_stage_1_linear_stack_reads_no_hg_manifest(fb: FacebookInit) -> Result<()> {
    // Given: every commit's Hg changeset derived at stage 0, and a stage 1
    // repo where the commits up to G have their Hg changesets.
    let from_hg = derive_fixture::<ChangeKinds>(fb, 0).await?;
    set_stage(1);
    let ctx = CoreContext::test_mock(fb);
    let (repo, reads) = repo_recording_labels(fb).await?;
    let (commits, _dag) = extend_from_dag_with_actions(&ctx, &repo, CHANGE_KINDS_DAG).await?;
    repo.derive_hg_changeset(&ctx, commits["G"]).await?;
    repo.repo_derived_data()
        .derive::<RootHgAugmentedManifestId>(
            &ctx,
            commits["J"],
            derivation_queue_thrift::DerivationPriority::LOW,
        )
        .await?;
    reads.take();

    // When: deriving H, I and J as one batch, where I and J form a linear
    // stack on H, which today's derivation builds from H's Hg manifest.
    let csids = ["H", "I", "J"].map(|name| commits[name]).to_vec();
    let derived = repo
        .repo_derived_data()
        .manager()
        .derive_exactly_batch::<MappedHgChangesetId>(&ctx, csids, None)
        .await;
    let batch_reads = reads.take().len();
    derived?;

    // Then: the batch read no Hg manifest and gave the stage 0 Hg changesets.
    assert_eq!(batch_reads, 0, "stage 1 should read no Hg manifest");
    for name in ["H", "I", "J"] {
        assert_eq!(
            repo.derive_hg_changeset(&ctx, commits[name]).await?,
            from_hg.changesets[name],
            "{name}"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_stage_1_reads_no_hg_manifest(fb: FacebookInit) -> Result<()> {
    // Given: a repo at stage 1 where every commit but the tip has its Hg
    // changeset.
    set_stage(1);
    let ctx = CoreContext::test_mock(fb);
    let (repo, reads) = repo_recording_labels(fb).await?;
    let (commits, _dag) = extend_from_dag_with_actions(&ctx, &repo, CHANGE_KINDS_DAG).await?;
    repo.derive_hg_changeset(&ctx, commits["I"]).await?;
    repo.repo_derived_data()
        .derive::<RootHgAugmentedManifestId>(
            &ctx,
            commits["J"],
            derivation_queue_thrift::DerivationPriority::LOW,
        )
        .await?;
    reads.take();

    // When: deriving the tip's Hg changeset.
    repo.derive_hg_changeset(&ctx, commits["J"]).await?;

    // Then: no Hg manifest was read.
    assert_eq!(reads.take().len(), 0, "stage 1 should read no Hg manifest");
    Ok(())
}

/// A repo with a parent whose stored augmented root is another commit's, and
/// whose Hg changeset was derived from Hg manifests, so its root tree is not
/// that augmented root. An identical root commit stores the augmented record
/// under the parent's Hg root tree id, so derivation that reads the parent by
/// that id would not fail on its own. Returns the parent and a child of it.
async fn repo_with_parent_mapped_to_another_root(
    ctx: &CoreContext,
    repo: &Repo,
) -> Result<(ChangesetId, ChangesetId)> {
    set_stage(0);
    let commit = |parents: Vec<ChangesetId>, message: &'static str, content: &'static str| {
        CreateCommitContext::new(ctx, repo, parents)
            .set_message(message)
            .add_file("file", content)
            .commit()
    };
    let other = commit(vec![], "other", "other").await?;
    let identical = commit(vec![], "identical", "one").await?;
    let parent = commit(vec![], "parent", "one").await?;
    let child = commit(vec![parent], "child", "two").await?;
    let derived_data = repo.repo_derived_data();
    derived_data
        .derive::<RootHgAugmentedManifestId>(ctx, identical, DerivationPriority::LOW)
        .await?;
    derived_data
        .derive::<RootHgAugmentedManifestId>(ctx, other, DerivationPriority::LOW)
        .await?
        .store_mapping(
            ctx,
            &derived_data.manager().derivation_context(None),
            parent,
        )
        .await?;
    repo.derive_hg_changeset(ctx, parent).await?;
    Ok((parent, child))
}

#[mononoke::fbinit_test]
async fn test_stage_1_fails_when_a_parent_maps_to_another_augmented_root(
    fb: FacebookInit,
) -> Result<()> {
    // Given: a parent that maps to another augmented root.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let (parent, child) = repo_with_parent_mapped_to_another_root(&ctx, &repo).await?;

    // When: deriving the child's Hg changeset at stage 1.
    set_stage(1);
    let err = repo
        .derive_hg_changeset(&ctx, child)
        .await
        .expect_err("a parent mapped to another augmented root should fail derivation");

    // Then: the error names the parent, and the child has no Hg changeset.
    assert!(
        format!("{err:#}").contains(&format!("parent {parent} maps to augmented root")),
        "error should name the parent: {err:#}",
    );
    assert_eq!(
        repo.bonsai_hg_mapping()
            .get_hg_from_bonsai(&ctx, child)
            .await?,
        None,
        "a failed guard should map nothing"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_invalid_stage_fails_derivation(fb: FacebookInit) -> Result<()> {
    // Given: a repo whose stage is 3, which is not a stage.
    set_stage(3);
    let ctx = CoreContext::test_mock(fb);
    let (repo, _reads) = repo_recording_labels(fb).await?;
    let (commits, _dag) = extend_from_dag_with_actions(&ctx, &repo, CHANGE_KINDS_DAG).await?;

    // When: deriving an Hg changeset.
    let err = repo
        .derive_hg_changeset(&ctx, commits["A"])
        .await
        .expect_err("an invalid stage should fail derivation");

    // Then: the error names the JustKnob.
    assert!(
        format!("{err:#}").contains(STAGE_KNOB),
        "error should name {STAGE_KNOB}: {err:#}",
    );

    // And: a batch that is one linear stack fails the same way.
    set_stage(0);
    repo.derive_hg_changeset(&ctx, commits["G"]).await?;
    set_stage(3);
    let err = repo
        .derive_hg_changeset(&ctx, commits["J"])
        .await
        .expect_err("an invalid stage should fail a linear batch");
    assert!(
        format!("{err:#}").contains(STAGE_KNOB),
        "error should name {STAGE_KNOB}: {err:#}",
    );
    Ok(())
}

/// A pipeline stage layout: each stage's path, its child stages, and the
/// cut points that partition its children.
type StageLayout = Vec<(&'static str, Vec<&'static str>, Vec<u8>)>;

/// Layouts covering each kind of stage for `CHANGE_KINDS_DAG`: stages for
/// child directories, a partitioned root, and a partitioned child stage.
fn change_kinds_layouts() -> Vec<StageLayout> {
    vec![
        vec![
            ("", vec!["dir", "left"], vec![]),
            ("dir", vec![], vec![]),
            ("left", vec![], vec![]),
        ],
        vec![("", vec![], b"dmr".to_vec())],
        vec![("", vec!["dir"], vec![]), ("dir", vec![], b"c".to_vec())],
    ]
}

fn stage_path(path: &str) -> Result<MPath> {
    if path.is_empty() {
        Ok(MPath::ROOT)
    } else {
        MPath::new(path)
    }
}

fn pipeline_config(layout: &StageLayout) -> Result<DerivationPipelineConfig> {
    let stages = layout
        .iter()
        .map(|(path, children, cut_points)| {
            let path = stage_path(path)?;
            let dependencies = children
                .iter()
                .map(|child| {
                    Ok(path.join_element(Some(&MPathElement::new(child.as_bytes().to_vec())?)))
                })
                .collect::<Result<_>>()?;
            Ok((
                path,
                DerivationPipelineStageConfig {
                    partition_cut_points: cut_points.clone(),
                    dependencies,
                },
            ))
        })
        .collect::<Result<_>>()?;
    let config = DerivationPipelineConfig {
        stages_by_type: Default::default(),
        types: BTreeSet::from(PIPELINE_TYPES),
        bookmarks: vec![BookmarkKey::new("master")?],
        stages,
        batch_size: NonZeroU64::new(3).expect("3 is not zero"),
    };
    config.validate()?;
    Ok(config)
}

const PIPELINE_TYPES: [DerivableType; 3] = [
    DerivableType::AclManifests,
    DerivableType::HgAugmentedManifests,
    DerivableType::HgChangesets,
];

/// Derive the Hg changesets of `head` and its ancestors with the derivation
/// pipeline, publishing them as the pipeline does in production.
async fn derive_with_pipeline(
    ctx: &CoreContext,
    repo: &Repo,
    layout: &StageLayout,
    head: ChangesetId,
) -> Result<()> {
    let mut commits = repo
        .commit_graph()
        .ancestors_difference(ctx, vec![head], vec![])
        .await?;
    commits.reverse();
    run_pipeline(ctx, repo, layout, commits).await
}

/// Derive `commits`, parents first, with the derivation pipeline, publishing
/// them as the pipeline does in production.
async fn run_pipeline(
    ctx: &CoreContext,
    repo: &Repo,
    layout: &StageLayout,
    commits: Vec<ChangesetId>,
) -> Result<()> {
    let config = pipeline_config(layout)?;
    let manager = repo
        .repo_derived_data()
        .manager()
        .with_pipeline_mode(PipelineMode::Production);
    let PipelineOrder {
        sorted_stages,
        sorted_types,
    } = plan_stages_and_types(&manager, &config, &PIPELINE_TYPES)?;
    let batches = plan_batches(ctx, repo.repo_blobstore(), &config, commits).await?;
    run_pipeline_batches(
        &manager,
        ctx,
        &config,
        &sorted_stages,
        &sorted_types,
        &batches,
        |_| {},
    )
    .await?;
    Ok(())
}

/// The Hg changesets the pipeline published for `commits`.
async fn published_hg_changesets(
    ctx: &CoreContext,
    repo: &Repo,
    commits: &BTreeMap<String, ChangesetId>,
) -> Result<BTreeMap<String, HgChangesetId>> {
    let mut changesets = BTreeMap::new();
    for (name, cs_id) in commits {
        let hg_cs_id = repo
            .bonsai_hg_mapping()
            .get_hg_from_bonsai(ctx, *cs_id)
            .await?
            .unwrap_or_else(|| panic!("the pipeline should publish the Hg changeset of {name}"));
        changesets.insert(name.clone(), hg_cs_id);
    }
    Ok(changesets)
}

/// Builds `CHANGE_KINDS_DAG` at `stage` in a new repo and derives every
/// commit's Hg changeset with the pipeline in `layout`. Also returns how many
/// Hg manifests the pipeline read.
async fn derive_change_kinds_with_pipeline(
    fb: FacebookInit,
    stage: i64,
    layout: &StageLayout,
) -> Result<(HgDerivation, usize)> {
    set_stage(stage);
    let ctx = CoreContext::test_mock(fb);
    let (repo, reads) = repo_recording_labels(fb).await?;
    let (commits, _dag) = extend_from_dag_with_actions(&ctx, &repo, CHANGE_KINDS_DAG).await?;
    let commits: BTreeMap<_, _> = commits.into_iter().collect();
    reads.take();
    derive_with_pipeline(&ctx, &repo, layout, commits["J"]).await?;
    let pipeline_reads = reads.take().len();
    let changesets = published_hg_changesets(&ctx, &repo, &commits).await?;
    let trees = read_hg_trees(&ctx, &repo, &changesets).await?;
    Ok((HgDerivation { changesets, trees }, pipeline_reads))
}

#[mononoke::fbinit_test]
async fn test_stage_1_pipeline_matches_stage_0(fb: FacebookInit) -> Result<()> {
    for layout in change_kinds_layouts() {
        // Given: every commit's Hg changeset derived by the pipeline at stage 0.
        let (from_hg, _) = derive_change_kinds_with_pipeline(fb, 0, &layout).await?;

        // When: the pipeline derives them at stage 1 in a new repo.
        let (from_augmented, reads) = derive_change_kinds_with_pipeline(fb, 1, &layout).await?;

        // Then: it read no Hg manifest and gave the same Hg changesets and Hg
        // tree bytes.
        assert_eq!(reads, 0, "{layout:?}: stage 1 should read no Hg manifest");
        assert_eq!(
            from_augmented.changesets.len(),
            12,
            "{layout:?}: every commit should have an Hg changeset"
        );
        assert_eq!(
            from_augmented, from_hg,
            "{layout:?}: stage 1 should give the Hg changesets and Hg tree bytes of stage 0"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_stage_1_pipeline_extraction_matches_stage_0(fb: FacebookInit) -> Result<()> {
    // Given: every commit's Hg changeset and augmented manifest derived
    // without the pipeline.
    set_stage(0);
    let ctx = CoreContext::test_mock(fb);
    let (repo, reads) = repo_recording_labels(fb).await?;
    let (commits, _dag) = extend_from_dag_with_actions(&ctx, &repo, CHANGE_KINDS_DAG).await?;
    repo.derive_hg_changeset(&ctx, commits["J"]).await?;
    let manager = repo.repo_derived_data().manager();
    let derivation_ctx = manager.derivation_context(None);
    let stages = [
        StageId::Manifest(MPath::ROOT),
        StageId::Manifest(MPath::new("dir")?),
        StageId::Manifest(MPath::new("dir/sub")?),
        StageId::Manifest(MPath::new("a.txt")?),
        StageId::Manifest(MPath::new("missing")?),
        StageId::Subentries(MPath::ROOT, ChildNameRange::new(b'd', b'l')?),
        StageId::Subentries(MPath::new("dir")?, ChildNameRange::new(0, b'b')?),
        StageId::Subentries(MPath::new("dir")?, ChildNameRange::new(b'c', u8::MAX)?),
        StageId::Finalize,
    ];

    for name in ["A", "M", "E", "F", "I"] {
        let mapped = manager
            .fetch_derived::<MappedHgChangesetId>(&ctx, commits[name], None)
            .await?
            .expect("the Hg changeset was just derived");
        for stage in &stages {
            // When: the pipeline takes the commit's output for the stage from
            // its derived Hg changeset at stage 0 and at stage 1.
            set_stage(0);
            let from_hg = MappedHgChangesetId::extract_stage_output_from_derived(
                &ctx,
                &derivation_ctx,
                &mapped,
                stage,
            )
            .await?;
            set_stage(1);
            reads.take();
            let from_augmented = MappedHgChangesetId::extract_stage_output_from_derived(
                &ctx,
                &derivation_ctx,
                &mapped,
                stage,
            )
            .await?;

            // Then: stage 1 read no Hg manifest and gave the same output.
            assert_eq!(
                reads.take().len(),
                0,
                "{name} {stage:?}: stage 1 should read no Hg manifest"
            );
            assert!(
                from_augmented.equivalent(&from_hg).await?,
                "{name} {stage:?}: stage 1 gave {from_augmented:?}, stage 0 gave {from_hg:?}"
            );
        }
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_stage_1_pipeline_needs_augmented_stage_outputs(fb: FacebookInit) -> Result<()> {
    // Given: a stage 1 repo whose augmented manifests are derived without the
    // pipeline, so no augmented stage output is supplied.
    set_stage(1);
    let ctx = CoreContext::test_mock(fb);
    let (repo, reads) = repo_recording_labels(fb).await?;
    let (commits, _dag) = extend_from_dag_with_actions(&ctx, &repo, CHANGE_KINDS_DAG).await?;
    repo.derive_hg_changeset(&ctx, commits["I"]).await?;
    repo.repo_derived_data()
        .derive::<RootHgAugmentedManifestId>(
            &ctx,
            commits["J"],
            derivation_queue_thrift::DerivationPriority::LOW,
        )
        .await?;
    reads.take();

    // When: the pipeline derives the tip's `dir` stage. (The root stage can
    // use the published augmented manifest.)
    let err = repo
        .repo_derived_data()
        .manager()
        .derive_stage_batch::<MappedHgChangesetId>(
            &ctx,
            vec![commits["J"]],
            &DerivationStagePayload::Manifest(ManifestStagePayload {
                path: MPath::new("dir")?,
                deps: ManifestStageDependencies::Children(vec![]),
            }),
        )
        .await
        .expect_err("a missing augmented stage output should fail the stage");

    // Then: the error says the stage layouts must match, and no Hg manifest
    // was read.
    assert!(
        format!("{err:#}").contains("same pipeline stage layout"),
        "unexpected error: {err:#}"
    );
    assert_eq!(reads.take().len(), 0, "stage 1 should read no Hg manifest");
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_stage_1_pipeline_fails_when_a_parent_maps_to_another_augmented_root(
    fb: FacebookInit,
) -> Result<()> {
    // Given: a parent derived without the pipeline that maps to another
    // augmented root.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let (parent, child) = repo_with_parent_mapped_to_another_root(&ctx, &repo).await?;

    // When: the pipeline derives the child at stage 1.
    set_stage(1);
    let err = run_pipeline(&ctx, &repo, &vec![("", vec![], vec![])], vec![child])
        .await
        .expect_err("a parent mapped to another augmented root should fail the pipeline");

    // Then: the error names the parent.
    assert!(
        format!("{err:#}").contains(&format!("parent {parent} maps to augmented root")),
        "error should name the parent: {err:#}",
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_stage_1_pipeline_records_restricted_paths_like_stage_0(
    fb: FacebookInit,
) -> Result<()> {
    // A partitioned root puts `public` and `restricted` in different
    // partitions; a child stage for `restricted` puts the restriction roots
    // in a manifest stage.
    let layouts: Vec<StageLayout> = vec![
        vec![("", vec![], b"q".to_vec())],
        vec![
            ("", vec!["restricted"], vec![]),
            ("restricted", vec![], vec![]),
        ],
    ];
    for layout in layouts {
        // Given: two equivalent repos with two restriction roots and an
        // unrestricted directory, with restricted-path recording on.
        override_just_knobs(restricted_paths_access_logging_knobs(true));
        let ctx = CoreContext::test_mock(fb);
        let restricted_paths = vec![
            NonRootMPath::new("restricted")?,
            NonRootMPath::new("restricted/nested")?,
        ];
        let stage_0_repo =
            build_repo_with_restricted_path_config(fb, restricted_paths.clone()).await?;
        let stage_1_repo = build_repo_with_restricted_path_config(fb, restricted_paths).await?;
        let (_, stage_0_child) = create_restricted_path_test_stack(&ctx, &stage_0_repo).await?;
        let (_, stage_1_child) = create_restricted_path_test_stack(&ctx, &stage_1_repo).await?;

        // When: the pipeline derives the stack at stage 0 in one repo and at
        // stage 1 in the other.
        set_stage(0);
        derive_with_pipeline(&ctx, &stage_0_repo, &layout, stage_0_child).await?;
        set_stage(1);
        derive_with_pipeline(&ctx, &stage_1_repo, &layout, stage_1_child).await?;

        // Then: both give the same Hg changeset and record the same Hg
        // restricted-path entries.
        let stage_0_cs = stage_0_repo
            .bonsai_hg_mapping()
            .get_hg_from_bonsai(&ctx, stage_0_child)
            .await?;
        let stage_1_cs = stage_1_repo
            .bonsai_hg_mapping()
            .get_hg_from_bonsai(&ctx, stage_1_child)
            .await?;
        assert!(stage_0_cs.is_some(), "{layout:?}: stage 0 should publish");
        assert_eq!(stage_1_cs, stage_0_cs, "{layout:?}");
        let stage_0_entries =
            restricted_path_entries(&ctx, &stage_0_repo, ManifestType::Hg).await?;
        let stage_1_entries =
            restricted_path_entries(&ctx, &stage_1_repo, ManifestType::Hg).await?;
        assert!(
            !stage_0_entries.is_empty(),
            "{layout:?}: stage 0 should record restriction roots"
        );
        assert_eq!(stage_1_entries, stage_0_entries, "{layout:?}");
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_stage_1_records_restricted_paths_like_stage_0(fb: FacebookInit) -> Result<()> {
    // Given: two equivalent repos with a two-commit stack, two configured
    // restriction roots and an unrestricted public directory, with
    // restricted-path recording on.
    override_just_knobs(restricted_paths_access_logging_knobs(true));
    let ctx = CoreContext::test_mock(fb);
    let restricted_paths = vec![
        NonRootMPath::new("restricted")?,
        NonRootMPath::new("restricted/nested")?,
    ];
    let stage_0_repo = build_repo_with_restricted_path_config(fb, restricted_paths.clone()).await?;
    let stage_1_repo = build_repo_with_restricted_path_config(fb, restricted_paths).await?;
    let (stage_0_parent, stage_0_child) =
        create_restricted_path_test_stack(&ctx, &stage_0_repo).await?;
    let (stage_1_parent, stage_1_child) =
        create_restricted_path_test_stack(&ctx, &stage_1_repo).await?;

    // When: deriving the stack's Hg changesets one commit at a time at stage 0
    // in one repo and at stage 1 in the other.
    set_stage(0);
    stage_0_repo
        .derive_hg_changeset(&ctx, stage_0_parent)
        .await?;
    let stage_0_cs = stage_0_repo
        .derive_hg_changeset(&ctx, stage_0_child)
        .await?;
    set_stage(1);
    stage_1_repo
        .derive_hg_changeset(&ctx, stage_1_parent)
        .await?;
    let stage_1_cs = stage_1_repo
        .derive_hg_changeset(&ctx, stage_1_child)
        .await?;

    // Then: both record the same Hg restricted-path entries, only for the
    // configured restriction roots.
    assert_eq!(stage_1_cs, stage_0_cs);
    let stage_0_entries = restricted_path_entries(&ctx, &stage_0_repo, ManifestType::Hg).await?;
    let stage_1_entries = restricted_path_entries(&ctx, &stage_1_repo, ManifestType::Hg).await?;
    assert_eq!(stage_1_entries, stage_0_entries);
    let paths: Vec<_> = stage_1_entries
        .iter()
        .map(|entry| entry.repo_path())
        .collect::<Result<_>>()?;
    assert!(
        paths.contains(&RepoPath::dir("restricted")?)
            && paths.contains(&RepoPath::dir("restricted/nested")?),
        "both restriction roots should be recorded, got {paths:?}",
    );
    assert!(
        !paths.contains(&RepoPath::dir("public")?),
        "the unrestricted directory should not be recorded, got {paths:?}",
    );
    Ok(())
}
