/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;

use anyhow::Result;
use blobstore::Loadable;
use bonsai_hg_mapping::BonsaiHgMapping;
use bookmarks::Bookmarks;
use borrowed::borrowed;
use changesets_creation::save_changesets;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphWriter;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use derived_data_manager::DerivationContext;
use derived_data_manager::DerivationStagePayload;
use derived_data_manager::ManifestStagePayload;
use derived_data_manager::PipelineDerivable;
use derived_data_manager::StageId;
use derived_data_manager::StageInputs;
use fbinit::FacebookInit;
use filestore::FilestoreConfig;
use futures::FutureExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use manifest::Entry;
use manifest::ManifestOps;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::FileUnodeId;
use mononoke_types::MPathElement;
use mononoke_types::ManifestUnodeId;
use mononoke_types::NonRootMPath;
use mononoke_types::blame_v2::load_blame_with_prefix;
use mononoke_types::path::MPath;
use mononoke_types::subtree_change::SubtreeChange;
use repo_blobstore::RepoBlobstore;
use repo_derived_data::RepoDerivedData;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentity;
use tests_utils::CreateCommitContext;
use unodes::RootUnodeManifestId;
use unodes::UnodeRenameSource;
use unodes::find_stage_unode_rename_sources;
use unodes::find_unode_rename_sources;

use crate::RootBlameV2;
use crate::format_key;

#[derive(Clone)]
#[facet::container]
struct TestRepo {
    #[facet]
    bonsai_hg_mapping: dyn BonsaiHgMapping,
    #[facet]
    bookmarks: dyn Bookmarks,
    #[facet]
    repo_blobstore: RepoBlobstore,
    #[facet]
    repo_derived_data: RepoDerivedData,
    #[facet]
    filestore_config: FilestoreConfig,
    #[facet]
    commit_graph: CommitGraph,
    #[facet]
    commit_graph_writer: dyn CommitGraphWriter,
    #[facet]
    repo_identity: RepoIdentity,
}

// The stage-S subtree (the `dir` manifest unode) of a changeset.
async fn stage_subtree_of(
    ctx: &CoreContext,
    repo: &TestRepo,
    csid: ChangesetId,
    stage_path: &MPath,
) -> Result<Option<ManifestUnodeId>> {
    let root = repo
        .repo_derived_data()
        .manager()
        .derive::<RootUnodeManifestId>(ctx, csid, None, DerivationPriority::LOW)
        .await?;
    let entry = root
        .manifest_unode_id()
        .find_entry(ctx.clone(), repo.repo_blobstore.clone(), stage_path.clone())
        .await?;
    Ok(entry.and_then(|entry| match entry {
        manifest::Entry::Tree(mf_id) => Some(mf_id),
        manifest::Entry::Leaf(_) => None,
    }))
}

// The canonical FileUnodeId for `path` in `csid` (content-addressed, so the
// namespace is irrelevant).
async fn file_unode_of(
    ctx: &CoreContext,
    repo: &TestRepo,
    csid: ChangesetId,
    path: &str,
) -> Result<FileUnodeId> {
    let root = repo
        .repo_derived_data()
        .manager()
        .derive::<RootUnodeManifestId>(ctx, csid, None, DerivationPriority::LOW)
        .await?;
    root.manifest_unode_id()
        .find_entry(ctx.clone(), repo.repo_blobstore.clone(), MPath::new(path)?)
        .await?
        .and_then(|entry| entry.into_leaf())
        .ok_or_else(|| anyhow::anyhow!("{path} should be a file"))
}

/// `find_stage_unode_rename_sources` resolves copy sources against the parents'
/// stage-S outputs: a source strictly under S resolves against a Tree parent, a
/// source equal to the stage root resolves against a Leaf parent, and a source
/// absent from the parent's stage subtree yields no rename.
#[mononoke::fbinit_test]
async fn test_find_stage_unode_rename_sources(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
    borrowed!(ctx, repo);

    // Stage S owns the `dir` subtree.
    let stage_path = MPath::new("dir")?;

    // Parent A: a tree at the stage root holding `dir/file_in`.
    let tree_parent = CreateCommitContext::new_root(ctx, repo)
        .add_file("dir/file_in", "content in")
        .commit()
        .await?;
    // Parent B: the stage root `dir` is itself a file (a leaf).
    let leaf_parent = CreateCommitContext::new_root(ctx, repo)
        .add_file("dir", "stage root is a file")
        .commit()
        .await?;

    // A merge child carrying three copies: a source strictly under S from the
    // tree parent, a source equal to the stage root from the leaf parent, and a
    // source under S that does not exist in the tree parent.
    let child = CreateCommitContext::new(ctx, repo, vec![tree_parent, leaf_parent])
        .add_file_with_copy_info("dir/copy_under", "content in", (tree_parent, "dir/file_in"))
        .add_file_with_copy_info(
            "dir/from_root",
            "stage root is a file",
            (leaf_parent, "dir"),
        )
        .add_file_with_copy_info("dir/copy_missing", "absent", (tree_parent, "dir/absent"))
        .commit()
        .await?;

    let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
    let bonsai = child.load(ctx, &repo.repo_blobstore).await?;

    let tree_subtree = stage_subtree_of(ctx, repo, tree_parent, &stage_path)
        .await?
        .expect("tree parent should have a dir subtree");
    let leaf_unode = file_unode_of(ctx, repo, leaf_parent, "dir").await?;
    let parent_stage_outputs: HashMap<ChangesetId, Option<Entry<ManifestUnodeId, FileUnodeId>>> =
        HashMap::from([
            (tree_parent, Some(Entry::Tree(tree_subtree))),
            (leaf_parent, Some(Entry::Leaf(leaf_unode))),
        ]);

    let parent_stage_outputs: HashMap<_, _> = parent_stage_outputs
        .into_iter()
        .map(|(csid, entry)| {
            (
                csid,
                derived_data_manager::ManifestStageOutput::Entry(entry),
            )
        })
        .collect();
    let payload = DerivationStagePayload::Manifest(ManifestStagePayload {
        path: stage_path.clone(),
        deps: vec![].into(),
    });
    for missing in [tree_parent, leaf_parent] {
        let mut incomplete = parent_stage_outputs.clone();
        incomplete.remove(&missing);
        let error =
            find_stage_unode_rename_sources(ctx, &derivation_ctx, &payload, &bonsai, &incomplete)
                .await
                .err()
                .expect("missing parent must fail copy resolution");
        assert!(format!("{error:#}").contains("missing unode stage output"));
    }
    let renames = find_stage_unode_rename_sources(
        ctx,
        &derivation_ctx,
        &payload,
        &bonsai,
        &parent_stage_outputs,
    )
    .await?;

    // Source strictly under S resolves against the tree parent.
    let file_in_unode = file_unode_of(ctx, repo, tree_parent, "dir/file_in").await?;
    match renames
        .get(&NonRootMPath::new("dir/copy_under")?)
        .expect("source under S should resolve against the tree parent")
    {
        UnodeRenameSource::CopyInfo(source) => {
            assert_eq!(source.parent_index, 0);
            assert_eq!(source.from_path, NonRootMPath::new("dir/file_in")?);
            assert_eq!(source.unode_id, file_in_unode);
        }
        _ => panic!("expected a copy-info rename source"),
    }

    // Source equal to the stage root resolves against the leaf parent.
    match renames
        .get(&NonRootMPath::new("dir/from_root")?)
        .expect("source at the stage root should resolve against the leaf parent")
    {
        UnodeRenameSource::CopyInfo(source) => {
            assert_eq!(source.parent_index, 1);
            assert_eq!(source.from_path, NonRootMPath::new("dir")?);
            assert_eq!(source.unode_id, leaf_unode);
        }
        _ => panic!("expected a copy-info rename source"),
    }

    // Source absent from the tree parent yields no rename.
    assert!(
        renames
            .get(&NonRootMPath::new("dir/copy_missing")?)
            .is_none(),
        "a copy from a path absent in the parent's stage subtree should not resolve",
    );

    Ok(())
}

/// JustKnobs with the prod-mapping OFF, so the pipeline writes blame to the
/// namespaced key and verify_stage compares namespaced vs canonical blobs.
fn namespaced_test_knobs() -> JustKnobsInMemory {
    JustKnobsInMemory::new(HashMap::from([(
        "scm/mononoke:derived_data_pipeline_terminal_stage_prod_mapping".to_string(),
        KnobVal::Bool(false),
    )]))
}

/// JustKnobs with the prod-mapping ON: the terminal (root) stage uses the
/// canonical mapping key (`format_key`) for its stage outputs, and the pipeline
/// writes blame to the canonical empty-prefix key.
fn prod_mapping_test_knobs() -> JustKnobsInMemory {
    JustKnobsInMemory::new(HashMap::from([(
        "scm/mononoke:derived_data_pipeline_terminal_stage_prod_mapping".to_string(),
        KnobVal::Bool(true),
    )]))
}

#[mononoke::fbinit_test]
async fn test_unchanged_stage_leaf_preserves_canonical_attribution(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
    borrowed!(ctx, repo);
    let parent = CreateCommitContext::new_root(ctx, repo)
        .add_file("s", "original line\n")
        .commit()
        .await?;
    let child = CreateCommitContext::new(ctx, repo, vec![parent])
        .add_file("outside", "unrelated change")
        .commit()
        .await?;
    let manager = repo.repo_derived_data().manager();
    let stage_path = MPath::new("s")?;
    let payload = DerivationStagePayload::Manifest(ManifestStagePayload {
        path: stage_path.clone(),
        deps: vec![].into(),
    });

    with_just_knobs_async(
        namespaced_test_knobs(),
        async {
            manager
                .derive::<RootBlameV2>(ctx, parent, None, DerivationPriority::LOW)
                .await?;
            let file_unode = file_unode_of(ctx, repo, parent, "s").await?;
            let before = load_blame_with_prefix(ctx, &repo.repo_blobstore, file_unode, "")
                .await?
                .expect("canonical parent blame should exist");

            // Only the child gets a stage mapping. The manager resolves the
            // parent's canonical unode and correctly reuses its unchanged leaf.
            let result = manager
                .derive_stage_batch::<RootUnodeManifestId>(ctx, vec![child], &payload)
                .await?;
            let inputs = StageInputs::new(&result.outputs.into_iter().collect::<Vec<_>>())?;
            let manager = manager.with_stage_inputs(inputs);
            let derivation_ctx = manager.derivation_context(None);
            let outputs = RootUnodeManifestId::fetch_stage_outputs(
                ctx,
                &derivation_ctx,
                &StageId::Manifest(stage_path),
                vec![parent, child],
            )
            .await?.into_iter().map(|(csid, output)| Ok((csid, output.into_entry()?))).collect::<Result<HashMap<_, _>>>()?;
            assert_eq!(outputs, HashMap::from([(child, Some(Entry::Leaf(file_unode)))]));

            manager
                .derive_stage_batch::<RootBlameV2>(ctx, vec![child], &payload)
                .await?;
            let after = load_blame_with_prefix(ctx, &repo.repo_blobstore, file_unode, "pipeline.")
                .await?
                .unwrap_or_else(|| before.clone());
            assert_eq!(
                before, after,
                "commit {child} changed only outside; blame must still attribute the line to {parent}",
            );
            anyhow::Ok(())
        }
        .boxed(),
    )
    .await
}

/// Derive and store the namespaced unode + blame outputs for a single stage
/// (any `stage_path`, root or non-root) across a linear chain, threading each
/// commit's parent stage outputs through unode derivation.
async fn derive_namespaced_stage(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    repo: &TestRepo,
    payload: &DerivationStagePayload,
    chain: &[ChangesetId],
) -> Result<()> {
    let supplied = derive_unode_stage_outputs(ctx, derivation_ctx, repo, payload, chain).await?;
    let derivation_ctx = &supplied;
    for csid in chain {
        let bonsai = csid.load(ctx, &repo.repo_blobstore).await?;
        RootBlameV2::derive_stage_batch(
            ctx,
            derivation_ctx,
            vec![bonsai],
            payload,
            HashMap::new(),
            HashMap::new(),
        )
        .await?;
    }
    Ok(())
}

/// Derive and store the namespaced unode stage outputs for `chain`
/// (ancestors before descendants) at `payload`'s stage, threading each
/// commit's parent stage outputs. Lets blame's `fetch_stage_outputs` find
/// the namespaced unode subtree.
async fn derive_unode_stage_outputs(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    repo: &TestRepo,
    payload: &DerivationStagePayload,
    chain: &[ChangesetId],
) -> Result<DerivationContext> {
    let DerivationStagePayload::Manifest(manifest_payload) = payload else {
        panic!("derive_unode_stage_outputs only supports manifest stages");
    };
    let stage_path = manifest_payload.path.clone();
    let mut references = Vec::new();
    let mut unode_outputs: HashMap<
        ChangesetId,
        <RootUnodeManifestId as PipelineDerivable>::StageOutput,
    > = HashMap::new();
    for csid in chain {
        let bonsai = csid.load(ctx, &repo.repo_blobstore).await?;
        let parents: HashMap<_, _> = bonsai
            .parents()
            .filter_map(|p| unode_outputs.get(&p).map(|o| (p, o.clone())))
            .collect();
        let out = RootUnodeManifestId::derive_stage_batch(
            ctx,
            derivation_ctx,
            vec![bonsai],
            payload,
            parents,
            HashMap::new(),
        )
        .await?;
        let reference = RootUnodeManifestId::store_stage_outputs(
            ctx,
            derivation_ctx,
            &StageId::Manifest(stage_path.clone()),
            out.clone(),
        )
        .await?;
        references.extend(reference);
        unode_outputs.extend(out);
    }
    let inputs = StageInputs::new(&references)?;
    Ok(repo
        .repo_derived_data()
        .manager()
        .with_stage_inputs(inputs)
        .derivation_context(None))
}

/// Derive and store the namespaced unode + blame root-stage outputs for a
/// linear chain (each commit's single parent precedes it).
async fn derive_namespaced_root_stage(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    repo: &TestRepo,
    chain: &[ChangesetId],
) -> Result<()> {
    let stage_path = MPath::ROOT;
    let payload = DerivationStagePayload::Manifest(ManifestStagePayload {
        path: stage_path.clone(),
        deps: vec![].into(),
    });
    derive_namespaced_stage(ctx, derivation_ctx, repo, &payload, chain).await
}

/// With the prod-mapping OFF, deriving canonical blame and namespaced
/// pipeline blame for the same commit populates both namespaces with equal
/// blobs, so verify_stage accepts the commit at the root stage.
#[mononoke::fbinit_test]
async fn test_namespaced_equality_verifies(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
    borrowed!(ctx, repo);

    let stage_path = MPath::ROOT;

    let c1 = CreateCommitContext::new_root(ctx, repo)
        .add_file("a/file", "first")
        .commit()
        .await?;
    let c2 = CreateCommitContext::new(ctx, repo, vec![c1])
        .add_file("a/file", "second")
        .add_file("b/other", "other")
        .commit()
        .await?;

    let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);

    with_just_knobs_async(
        namespaced_test_knobs(),
        async {
            // Canonical blame (writes the empty-prefix key) via the normal
            // RootBlameV2 derivation, which also derives canonical unodes.
            for csid in [c1, c2] {
                repo.repo_derived_data()
                    .manager()
                    .derive::<RootBlameV2>(ctx, csid, None, DerivationPriority::LOW)
                    .await?;
            }
            // Namespaced pipeline unodes + blame.
            derive_namespaced_root_stage(ctx, &derivation_ctx, repo, &[c1, c2]).await?;
            anyhow::Ok(())
        }
        .boxed(),
    )
    .await?;

    let verified = with_just_knobs_async(
        namespaced_test_knobs(),
        async {
            RootBlameV2::verify_stage(
                ctx,
                &derivation_ctx,
                c2,
                &StageId::Manifest(stage_path.clone()),
            )
            .await
        }
        .boxed(),
    )
    .await?;
    assert!(
        verified,
        "verify_stage should accept equal namespaced and canonical blame",
    );

    Ok(())
}

/// With the prod-mapping OFF, deriving only canonical blame (no pipeline
/// derivation) leaves the namespaced blob absent for the changed files, so
/// verify_stage rejects the commit at the root stage.
#[mononoke::fbinit_test]
async fn test_namespaced_missing_fails(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
    borrowed!(ctx, repo);

    let stage_path = MPath::ROOT;

    let c1 = CreateCommitContext::new_root(ctx, repo)
        .add_file("a/file", "only canonical")
        .commit()
        .await?;

    let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);

    // Only canonical blame is derived; the pipeline namespace stays empty.
    with_just_knobs_async(
        namespaced_test_knobs(),
        async {
            repo.repo_derived_data()
                .manager()
                .derive::<RootBlameV2>(ctx, c1, None, DerivationPriority::LOW)
                .await?;
            anyhow::Ok(())
        }
        .boxed(),
    )
    .await?;

    let verified = with_just_knobs_async(
        namespaced_test_knobs(),
        async {
            RootBlameV2::verify_stage(
                ctx,
                &derivation_ctx,
                c1,
                &StageId::Manifest(stage_path.clone()),
            )
            .await
        }
        .boxed(),
    )
    .await?;
    assert!(
        !verified,
        "verify_stage should reject a commit with missing pipeline blame",
    );

    Ok(())
}

/// A subtree-copy commit creates files at the destination with no
/// file_changes copy_from. The derive path must resolve those files'
/// renames via the full-root map (to the subtree source, not None), and
/// verify_stage must accept a correctly-derived subtree-change commit.
#[mononoke::fbinit_test]
async fn test_subtree_copy_rename_resolution(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
    borrowed!(ctx, repo);

    // Base commit with the subtree source `dir/file_in`.
    let c1 = CreateCommitContext::new_root(ctx, repo)
        .add_file("dir/file_in", "content in")
        .commit()
        .await?;

    // Subtree-copy commit: copy `dir` -> `copied`. The destination file
    // `copied/file_in` has no file_changes copy_from; the source lives only
    // in subtree_changes.
    let mut bcs = CreateCommitContext::new(ctx, repo, vec![c1])
        .add_file("copied/file_in", "content in")
        .create_commit_object()
        .await?;
    bcs.subtree_changes = [(
        MPath::new("copied")?,
        SubtreeChange::deep_copy(MPath::new("dir")?, c1),
    )]
    .into_iter()
    .collect();
    let bcs = bcs.freeze()?;
    let c2 = bcs.get_changeset_id();
    with_just_knobs_async(
        namespaced_test_knobs(),
        async { save_changesets(ctx, repo, vec![bcs]).await }.boxed(),
    )
    .await?;

    // Single root stage covers the whole tree (the chokepoint scenario).
    let stage_path = MPath::ROOT;

    let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
    let bonsai = c2.load(ctx, &repo.repo_blobstore).await?;

    // Derive canonical blame for both changesets. This writes the
    // empty-prefix (canonical) blame blobs and also derives the canonical
    // unodes that the full-root rename resolution depends on.
    with_just_knobs_async(
        namespaced_test_knobs(),
        async {
            for csid in [c1, c2] {
                repo.repo_derived_data()
                    .manager()
                    .derive::<RootBlameV2>(ctx, csid, None, DerivationPriority::LOW)
                    .await?;
            }
            anyhow::Ok(())
        }
        .boxed(),
    )
    .await?;

    // The derive path resolves the subtree-copied file's rename to the
    // subtree source, not None.
    let copied_path = NonRootMPath::new("copied/file_in")?;
    let renames = find_unode_rename_sources(ctx, &derivation_ctx, &bonsai).await?;
    let rename = renames
        .get(&copied_path)
        .expect("subtree-copied file should resolve to a rename source");
    match rename {
        UnodeRenameSource::SubtreeCopy(source) => {
            assert_eq!(source.parent, c1);
            assert_eq!(source.from_path, MPath::new("dir/file_in")?);
        }
        other => panic!("expected a subtree-copy rename source, got {other:?}"),
    }

    // Derive the namespaced pipeline blame (namespaced unode stage outputs +
    // namespaced blame at the root stage) for the chain, so verify_stage
    // compares two distinct blobs.
    with_just_knobs_async(
        namespaced_test_knobs(),
        async { derive_namespaced_root_stage(ctx, &derivation_ctx, repo, &[c1, c2]).await }.boxed(),
    )
    .await?;

    let verified = with_just_knobs_async(
        namespaced_test_knobs(),
        async {
            RootBlameV2::verify_stage(
                ctx,
                &derivation_ctx,
                c2,
                &StageId::Manifest(stage_path.clone()),
            )
            .await
        }
        .boxed(),
    )
    .await?;
    assert!(
        verified,
        "verify_stage should accept a subtree-copy commit whose namespaced pipeline blame matches canonical blame",
    );

    Ok(())
}

/// A merge commit (two parents) exercises the multi-parent branch of
/// `find_intersection_of_diffs_pruned` together with dependency pruning:
/// files under the dep path must be dropped, files under S must be blamed,
/// and verify_stage must accept the result.
#[mononoke::fbinit_test]
async fn test_merge_with_dep_pruning(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
    borrowed!(ctx, repo);

    // Stage S is the root; `dep` is a child stage S depends on.
    let stage_path = MPath::ROOT;
    let payload = DerivationStagePayload::Manifest(ManifestStagePayload {
        path: stage_path.clone(),
        deps: vec![MPathElement::new(b"dep".to_vec())?].into(),
    });

    // Base forks into two branches; each touches a file under S and a file
    // under the dep path, then they merge.
    let base = CreateCommitContext::new_root(ctx, repo)
        .add_file("s/base", "base")
        .add_file("dep/base", "base")
        .commit()
        .await?;
    let left = CreateCommitContext::new(ctx, repo, vec![base])
        .add_file("s/left", "left")
        .add_file("dep/left", "left")
        .commit()
        .await?;
    let right = CreateCommitContext::new(ctx, repo, vec![base])
        .add_file("s/right", "right")
        .add_file("dep/right", "right")
        .commit()
        .await?;
    let merge = CreateCommitContext::new(ctx, repo, vec![left, right])
        .add_file("s/merged", "merged")
        .add_file("dep/merged", "merged")
        .commit()
        .await?;

    let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);

    // Derive canonical blame for every commit in the merge DAG. This writes
    // the empty-prefix (canonical) blame blobs and also derives the
    // canonical unodes the merge changed-file resolution depends on.
    with_just_knobs_async(
        namespaced_test_knobs(),
        async {
            for csid in [base, left, right, merge] {
                repo.repo_derived_data()
                    .manager()
                    .derive::<RootBlameV2>(ctx, csid, None, DerivationPriority::LOW)
                    .await?;
            }
            anyhow::Ok(())
        }
        .boxed(),
    )
    .await?;

    // The `dep` child stage blames the dep subtree; the root stage prunes
    // it. verify checks the whole commit, so both must be blamed.
    let dep_payload = DerivationStagePayload::Manifest(ManifestStagePayload {
        path: MPath::new("dep")?,
        deps: vec![].into(),
    });
    let chain = [base, left, right, merge];

    // Each stage needs its own (namespaced) unode stage outputs; derive and
    // store those first, then blame the dep stage and the root stage.
    with_just_knobs_async(
        namespaced_test_knobs(),
        async {
            for stage_payload in [&dep_payload, &payload] {
                let derivation_ctx =
                    derive_unode_stage_outputs(ctx, &derivation_ctx, repo, stage_payload, &chain)
                        .await?;
                for csid in chain {
                    let bonsai = csid.load(ctx, &repo.repo_blobstore).await?;
                    RootBlameV2::derive_stage_batch(
                        ctx,
                        &derivation_ctx,
                        vec![bonsai],
                        stage_payload,
                        HashMap::new(),
                        HashMap::new(),
                    )
                    .await?;
                }
            }
            anyhow::Ok(())
        }
        .boxed(),
    )
    .await?;

    let verified = with_just_knobs_async(
        namespaced_test_knobs(),
        async {
            RootBlameV2::verify_stage(
                ctx,
                &derivation_ctx,
                merge,
                &StageId::Manifest(stage_path.clone()),
            )
            .await
        }
        .boxed(),
    )
    .await?;
    assert!(
        verified,
        "verify_stage should accept a merge-with-dep-pruning commit whose namespaced pipeline blame matches canonical blame",
    );

    Ok(())
}

/// The root stage carrying `deps: ["dep"]` must prune the dep subtree:
/// running ONLY the root stage (never the dep stage) must blame the
/// S-owned files and leave the dep files unblamed. Covers both pruning
/// paths: `c1` (parentless, list-all-entries + post-collect filter) and
/// `c2` (has-parent, `find_intersection_of_diffs_pruned` recurse_pruner).
#[mononoke::fbinit_test]
async fn test_root_stage_prunes_dep_subtree(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
    borrowed!(ctx, repo);

    // Root stage with a single dep child stage `dep`, so the pruner runs.
    let root_payload = DerivationStagePayload::Manifest(ManifestStagePayload {
        path: MPath::ROOT,
        deps: vec![MPathElement::new(b"dep".to_vec())?].into(),
    });

    let c1 = CreateCommitContext::new_root(ctx, repo)
        .add_file("s/base", "base")
        .add_file("dep/base", "base")
        .commit()
        .await?;
    let c2 = CreateCommitContext::new(ctx, repo, vec![c1])
        .add_file("s/child", "child")
        .add_file("dep/child", "child")
        .commit()
        .await?;

    let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);

    // Look up a file's canonical FileUnodeId from the commit that introduced
    // it (the unode is content-addressed, so the namespace doesn't matter).
    async fn file_unode(
        ctx: &CoreContext,
        repo: &TestRepo,
        csid: ChangesetId,
        path: &str,
    ) -> Result<mononoke_types::FileUnodeId> {
        let root = repo
            .repo_derived_data()
            .manager()
            .derive::<RootUnodeManifestId>(ctx, csid, None, DerivationPriority::LOW)
            .await?;
        let entry = root
            .manifest_unode_id()
            .find_entry(ctx.clone(), repo.repo_blobstore.clone(), MPath::new(path)?)
            .await?;
        entry
            .and_then(|entry| entry.into_leaf())
            .ok_or_else(|| anyhow::anyhow!("{path} should be a file"))
    }

    // Derive canonical unodes (for the unode lookups) and ONLY the root
    // stage's namespaced unodes + blame. The dep stage is never run.
    with_just_knobs_async(
        namespaced_test_knobs(),
        async {
            let derivation_ctx =
                derive_unode_stage_outputs(ctx, &derivation_ctx, repo, &root_payload, &[c1, c2])
                    .await?;
            for csid in [c1, c2] {
                let bonsai = csid.load(ctx, &repo.repo_blobstore).await?;
                RootBlameV2::derive_stage_batch(
                    ctx,
                    &derivation_ctx,
                    vec![bonsai],
                    &root_payload,
                    HashMap::new(),
                    HashMap::new(),
                )
                .await?;
            }
            anyhow::Ok(())
        }
        .boxed(),
    )
    .await?;

    let s_base = file_unode(ctx, repo, c1, "s/base").await?;
    let s_child = file_unode(ctx, repo, c2, "s/child").await?;
    let dep_base = file_unode(ctx, repo, c1, "dep/base").await?;
    let dep_child = file_unode(ctx, repo, c2, "dep/child").await?;

    let blobstore = &repo.repo_blobstore;

    // S-owned files are blamed by the root stage.
    assert!(
        load_blame_with_prefix(ctx, blobstore, s_base, "pipeline.")
            .await?
            .is_some(),
        "root stage should blame s/base",
    );
    assert!(
        load_blame_with_prefix(ctx, blobstore, s_child, "pipeline.")
            .await?
            .is_some(),
        "root stage should blame s/child",
    );

    // Dep files are pruned by the root stage and the dep stage never ran,
    // so they have no pipeline blame blob.
    assert!(
        load_blame_with_prefix(ctx, blobstore, dep_base, "pipeline.")
            .await?
            .is_none(),
        "root stage should prune dep/base (parentless pruning path)",
    );
    assert!(
        load_blame_with_prefix(ctx, blobstore, dep_child, "pipeline.")
            .await?
            .is_none(),
        "root stage should prune dep/child (has-parent pruning path)",
    );

    Ok(())
}

/// T1: A file copied INTO a non-root stage S from OUTSIDE S is a chokepoint;
/// the non-root S stage must resolve the rename via the full-root map
/// (`full_renames`) and produce blame that matches canonical. The root stage
/// (which declares S as a dep and prunes it) lets verify_stage check every
/// changed file -- including the cross-stage-copied file under S.
#[mononoke::fbinit_test]
async fn test_cross_stage_copy_into_non_root_stage(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
    borrowed!(ctx, repo);

    // Stage S owns the `s` subtree; the root stage prunes `s`.
    let s_payload = DerivationStagePayload::Manifest(ManifestStagePayload {
        path: MPath::new("s")?,
        deps: vec![].into(),
    });
    let root_payload = DerivationStagePayload::Manifest(ManifestStagePayload {
        path: MPath::ROOT,
        deps: vec![MPathElement::new(b"s".to_vec())?].into(),
    });
    let stage_path = MPath::ROOT;

    // Base has the copy source `outside/src` (outside S).
    let c1 = CreateCommitContext::new_root(ctx, repo)
        .add_file("outside/src", "copied content")
        .add_file("s/existing", "existing")
        .commit()
        .await?;
    // c2 copies outside/src -> s/copied: a copy INTO S from OUTSIDE S, which
    // makes c2 a chokepoint for stage S.
    let c2 = CreateCommitContext::new(ctx, repo, vec![c1])
        .add_file_with_copy_info("s/copied", "copied content", (c1, "outside/src"))
        .commit()
        .await?;

    let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
    let chain = [c1, c2];

    with_just_knobs_async(
        namespaced_test_knobs(),
        async {
            // Canonical blame + unodes for both commits.
            for csid in chain {
                repo.repo_derived_data()
                    .manager()
                    .derive::<RootBlameV2>(ctx, csid, None, DerivationPriority::LOW)
                    .await?;
            }
            // Namespaced pipeline blame. The root stage blames `outside/*`,
            // including the copy source `outside/src`; the S stage's blame for
            // the cross-stage-copied `s/copied` reads that parent blame, so the
            // root stage must run first.
            derive_namespaced_stage(ctx, &derivation_ctx, repo, &root_payload, &chain).await?;
            derive_namespaced_stage(ctx, &derivation_ctx, repo, &s_payload, &chain).await?;
            anyhow::Ok(())
        }
        .boxed(),
    )
    .await?;

    let verified = with_just_knobs_async(
        namespaced_test_knobs(),
        async {
            RootBlameV2::verify_stage(
                ctx,
                &derivation_ctx,
                c2,
                &StageId::Manifest(stage_path.clone()),
            )
            .await
        }
        .boxed(),
    )
    .await?;
    assert!(
        verified,
        "cross-stage copy into a non-root stage should blame the copied file to match canonical",
    );

    Ok(())
}

/// T2: A full derive_stage_batch run with a NON-root stage_path and an in-stage
/// copy. Exercises the non-root stage-path arithmetic (is_prefix_of /
/// remove_prefix_component / re-prefixing) through the real derive path and
/// asserts blame parents resolve correctly (the in-stage copy resolves to its
/// in-stage source, so the copied file's blame matches canonical).
#[mononoke::fbinit_test]
async fn test_non_root_stage_in_stage_copy(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
    borrowed!(ctx, repo);

    // Non-root stage S owns the `s` subtree; the root stage prunes it.
    let s_payload = DerivationStagePayload::Manifest(ManifestStagePayload {
        path: MPath::new("s")?,
        deps: vec![].into(),
    });
    let root_payload = DerivationStagePayload::Manifest(ManifestStagePayload {
        path: MPath::ROOT,
        deps: vec![MPathElement::new(b"s".to_vec())?].into(),
    });
    let stage_path = MPath::ROOT;

    let c1 = CreateCommitContext::new_root(ctx, repo)
        .add_file("s/orig", "original content")
        .commit()
        .await?;
    // In-stage copy: s/copy <- s/orig (source and dest both under S, not a
    // chokepoint). Resolved via find_stage_unode_rename_sources against the
    // parent's stage-S output.
    let c2 = CreateCommitContext::new(ctx, repo, vec![c1])
        .add_file_with_copy_info("s/copy", "original content", (c1, "s/orig"))
        .commit()
        .await?;

    let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
    let chain = [c1, c2];

    with_just_knobs_async(
        namespaced_test_knobs(),
        async {
            for csid in chain {
                repo.repo_derived_data()
                    .manager()
                    .derive::<RootBlameV2>(ctx, csid, None, DerivationPriority::LOW)
                    .await?;
            }
            derive_namespaced_stage(ctx, &derivation_ctx, repo, &s_payload, &chain).await?;
            derive_namespaced_stage(ctx, &derivation_ctx, repo, &root_payload, &chain).await?;
            anyhow::Ok(())
        }
        .boxed(),
    )
    .await?;

    // The copied file's blame, derived through the non-root S stage with
    // re-prefixing and in-stage rename resolution, must match canonical.
    let verified = with_just_knobs_async(
        namespaced_test_knobs(),
        async {
            RootBlameV2::verify_stage(
                ctx,
                &derivation_ctx,
                c2,
                &StageId::Manifest(stage_path.clone()),
            )
            .await
        }
        .boxed(),
    )
    .await?;
    assert!(
        verified,
        "non-root stage in-stage copy should resolve blame parents to match canonical",
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_prod_mapping_round_trip(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
    borrowed!(ctx, repo);
    let c1 = CreateCommitContext::new_root(ctx, repo)
        .add_file("a/file", "content")
        .commit()
        .await?;
    let c2 = CreateCommitContext::new(ctx, repo, vec![c1])
        .add_file("a/file", "changed")
        .commit()
        .await?;
    let c3 = CreateCommitContext::new(ctx, repo, vec![c2])
        .add_file("b/file", "not derived")
        .commit()
        .await?;
    let manager = repo.repo_derived_data().manager();
    let derivation = manager.derivation_context(None);
    let stage = StageId::Manifest(MPath::ROOT);
    let outputs = HashMap::from([(c1, ()), (c2, ())]);

    with_just_knobs_async(
        prod_mapping_test_knobs(),
        async {
            let root1 = manager
                .derive::<RootUnodeManifestId>(ctx, c1, None, DerivationPriority::LOW)
                .await?;
            let error = RootBlameV2::store_stage_outputs(ctx, &derivation, &stage, outputs.clone())
                .await
                .unwrap_err();
            assert!(error.to_string().contains(&format!(
                "terminal stage for {c2}: missing unode root stage output"
            )));

            let root2 = manager
                .derive::<RootUnodeManifestId>(ctx, c2, None, DerivationPriority::LOW)
                .await?;
            RootBlameV2::store_stage_outputs(ctx, &derivation, &stage, outputs.clone()).await?;
            assert_eq!(
                RootBlameV2::fetch_stage_outputs(ctx, &derivation, &stage, vec![c1, c2, c3])
                    .await?,
                outputs,
            );
            for (csid, expected) in [(c1, root1), (c2, root2)] {
                let bytes = derivation
                    .blobstore()
                    .get(ctx, &format_key(&derivation, csid))
                    .await?
                    .expect("canonical blame mapping should be present");
                assert_eq!(RootUnodeManifestId::try_from(bytes)?, expected);
            }
            anyhow::Ok(())
        }
        .boxed(),
    )
    .await
}

/// T4: A commit whose stage subtree is UNCHANGED from its parent yields an
/// empty diff: derive_stage_batch must insert (csid, ()) with zero blame writes
/// and not error.
#[mononoke::fbinit_test]
async fn test_unchanged_stage_subtree_is_noop(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
    borrowed!(ctx, repo);

    // Stage S owns the `s` subtree.
    let s_payload = DerivationStagePayload::Manifest(ManifestStagePayload {
        path: MPath::new("s")?,
        deps: vec![].into(),
    });

    let c1 = CreateCommitContext::new_root(ctx, repo)
        .add_file("s/file", "content")
        .add_file("other/file", "other")
        .commit()
        .await?;
    // c2 changes only `other` -- the `s` subtree is byte-identical to c1's.
    let c2 = CreateCommitContext::new(ctx, repo, vec![c1])
        .add_file("other/file", "changed")
        .commit()
        .await?;

    let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
    let chain = [c1, c2];

    // The S subtree's file unode is content-addressed and unchanged across c1
    // and c2; capture it to assert no blame is written for c2.
    let s_file_unode = {
        let root = repo
            .repo_derived_data()
            .manager()
            .derive::<RootUnodeManifestId>(ctx, c1, None, DerivationPriority::LOW)
            .await?;
        root.manifest_unode_id()
            .find_entry(
                ctx.clone(),
                repo.repo_blobstore.clone(),
                MPath::new("s/file")?,
            )
            .await?
            .and_then(|entry| entry.into_leaf())
            .expect("s/file should be a file")
    };

    let results = with_just_knobs_async(
        namespaced_test_knobs(),
        async {
            // Derive namespaced unodes for both commits at stage S.
            let derivation_ctx =
                derive_unode_stage_outputs(ctx, &derivation_ctx, repo, &s_payload, &chain).await?;
            // Run blame for c2 only: its S subtree is unchanged from c1.
            let bonsai_c2 = c2.load(ctx, &repo.repo_blobstore).await?;
            RootBlameV2::derive_stage_batch(
                ctx,
                &derivation_ctx,
                vec![bonsai_c2],
                &s_payload,
                HashMap::new(),
                HashMap::new(),
            )
            .await
        }
        .boxed(),
    )
    .await?;

    assert_eq!(
        results,
        HashMap::from([(c2, ())]),
        "an unchanged stage subtree should still insert (csid, ())",
    );

    // No blame blob should have been written for the unchanged S file by the
    // c2 batch run (the diff against the identical parent subtree is empty).
    assert!(
        load_blame_with_prefix(ctx, &repo.repo_blobstore, s_file_unode, "pipeline.")
            .await?
            .is_none(),
        "an unchanged stage subtree must produce zero blame writes",
    );

    Ok(())
}
