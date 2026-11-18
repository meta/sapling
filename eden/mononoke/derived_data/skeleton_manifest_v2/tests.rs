/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::task::Poll;

use anyhow::Result;
use blobstore::Loadable;
use blobstore::Storable;
use blobstore_test_utils::Tickable;
use bonsai_hg_mapping::BonsaiHgMapping;
use bookmarks::Bookmarks;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphWriter;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use derived_data_manager::DerivationStagePayload;
use derived_data_manager::ManifestDerivationInput;
use derived_data_manager::ManifestStagePayload;
use fbinit::FacebookInit;
use filestore::FilestoreConfig;
use futures::FutureExt;
use futures::TryStreamExt;
use futures::poll;
use itertools::Itertools;
use manifest::Entry;
use manifest::KnownEntry;
use manifest::ManifestOps;
use mononoke_macros::mononoke;
use mononoke_types::BlobstoreKey;
use mononoke_types::BlobstoreValue;
use mononoke_types::ChangesetId;
use mononoke_types::path::MPath;
use pretty_assertions::assert_eq;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreArc;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedData;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentity;
use repo_identity::RepoIdentityRef;
use tests_utils::CreateCommitContext;
use tests_utils::drawdag::changes;
use tests_utils::drawdag::create_from_dag_with_changes;

use super::*;
use crate::derive::derive_skeleton_manifest_v2_entry;
use crate::derive::get_file_changes;

#[facet::container]
struct TestRepo(
    dyn BonsaiHgMapping,
    dyn Bookmarks,
    CommitGraph,
    dyn CommitGraphWriter,
    RepoDerivedData,
    RepoBlobstore,
    FilestoreConfig,
    RepoIdentity,
);

#[mononoke::fbinit_test]
async fn pipeline_flushes_roots_before_mappings(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let csid = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file", "content")
        .commit()
        .await?;
    let bonsai = csid.load(&ctx, repo.repo_blobstore()).await?.into_blob();
    let manager = repo.repo_derived_data().manager();
    let expected = manager
        .derive::<RootSkeletonManifestV2Id>(&ctx, csid, None, DerivationPriority::LOW)
        .await?;
    let prefix = repo.repo_identity().id().prefix();
    let root_key = format!("{prefix}{}", expected.inner_id().blobstore_key());
    let mapping_key = format!(
        "{prefix}{}",
        format_key(&manager.derivation_context(None), csid)
    );
    let payload = DerivationStagePayload::Manifest(ManifestStagePayload {
        path: MPath::ROOT,
        deps: vec![].into(),
    });

    for error in [None, Some("root write failed")] {
        let store = Arc::new(Tickable::new());
        store.add_bytes(
            format!("{prefix}{}", csid.blobstore_key()),
            bonsai.clone().into(),
        );
        let manager = manager.with_replaced_blobstore(
            RepoBlobstore::new_with_wrapped_inner_blobstore(repo.repo_blobstore().clone(), |_| {
                store.clone()
            }),
        );
        let mut derive = tokio::task::unconstrained(
            manager.derive_stage_batch::<RootSkeletonManifestV2Id>(&ctx, vec![csid], &payload),
        )
        .boxed();

        assert!(poll!(&mut derive).is_pending());
        store.tick(None); // Read the Bonsai changeset.
        loop {
            let result = poll!(&mut derive);
            assert!(store.get_bytes(&mapping_key).is_none());
            if let Poll::Ready(result) = result {
                let error = error.expect("mapping must have a separate flush");
                assert!(format!("{:#}", result.unwrap_err()).contains(error));
                assert!(store.get_bytes(&root_key).is_none());
                break;
            }
            if store.get_bytes(&root_key).is_some() {
                assert!(error.is_none());
                store.tick(None);
                derive.await?;
                assert_eq!(
                    store
                        .get_bytes(&mapping_key)
                        .map(RootSkeletonManifestV2Id::try_from)
                        .transpose()?,
                    Some(expected.clone()),
                );
                break;
            }
            store.tick(error);
            tokio::task::yield_now().await;
        }
    }
    Ok(())
}

const A_FILES: &[&str] = &[
    "dir1/subdir1/subsubdir1/file1",
    "dir1/subdir1/subsubdir2/file2",
];

const B_FILES: &[&str] = &[
    "dir1/subdir1/subsubdir1/file1",
    "dir1/subdir1/subsubdir2/file2",
    "dir1/subdir1/subsubdir3/file3",
    "dir1/subdir2/subsubdir1/file1",
    "dir1/subdir2/subsubdir2/file1",
    "dir2/subdir1/subsubdir1/file1",
];

const C_FILES: &[&str] = &[
    "dir1/subdir1/subsubdir1/FILE1",
    "dir1/subdir1/SUBSUBDIR2/file2",
    "dir1/subdir1/SUBSUBDIR3/FILE3",
    "dir1/subdir2/SUBSUBDIR1/file1",
];

const D_FILES: &[&str] = &[
    "dir1/subdir1/subsubdir2/file2",
    "dir1/subdir1/SUBSUBDIR3/FILE3",
];

const E_FILES: &[&str] = &[
    "dir1/subdir1/subsubdir1/FILE1",
    "dir1/subdir2/subsubdir1/file1",
];

const F_FILES: &[&str] = &["dir1/subdir1/SUBSUBDIR3/File3"];

const G_FILES: &[&str] = &["dir3/FILE1", "dir3/file1"];

const H_FILES: &[&str] = &["dir3/file2"];

const I_FILES: &[&str] = &["dir2/subdir1/subsubdir1/FILE1"];

const J_FILES: &[&str] = &["dir2/subdir2/subsubdir1/file1"];

fn add_files<'a>(
    mut c: CreateCommitContext<'a, TestRepo>,
    files: &[&str],
) -> CreateCommitContext<'a, TestRepo> {
    for &file in files {
        c = c.add_file(file, file);
    }
    c
}

fn delete_files<'a>(
    mut c: CreateCommitContext<'a, TestRepo>,
    files: &[&str],
) -> CreateCommitContext<'a, TestRepo> {
    for &file in files {
        c = c.delete_file(file);
    }
    c
}

async fn init_repo(ctx: &CoreContext) -> Result<(TestRepo, BTreeMap<String, ChangesetId>)> {
    let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await.unwrap();
    let changesets = create_from_dag_with_changes(
        ctx,
        &repo,
        r"
                F-G
                 /
            A-B-C-D-E
                 \
                  H
                   \
                  I-J
        ",
        changes! {
            "A" => |c| add_files(c, A_FILES),
            "B" => |c| add_files(c, B_FILES),
            "C" => |c| add_files(c, C_FILES),
            "D" => |c| delete_files(c, D_FILES),
            "E" => |c| delete_files(c, E_FILES),
            "F" => |c| add_files(c, F_FILES),
            "G" => |c| add_files(c, G_FILES),
            "H" => |c| add_files(c, H_FILES),
            "I" => |c| add_files(c, I_FILES),
            "J" => |c| add_files(c, J_FILES),
        },
    )
    .await?;
    Ok((repo, changesets))
}

async fn assert_all_leaves(
    ctx: &CoreContext,
    repo: &TestRepo,
    cs_id: ChangesetId,
    expected: &[&str],
) -> Result<()> {
    let root_skeleton_manifest_id = repo
        .repo_derived_data()
        .derive::<RootSkeletonManifestV2Id>(ctx, cs_id, DerivationPriority::LOW)
        .await?;

    let skeleton_manifest = root_skeleton_manifest_id
        .into_inner_id()
        .load(ctx, repo.repo_blobstore())
        .await?;

    let skeleton_manifest_leaf_entries = skeleton_manifest
        .list_leaf_entries(ctx.clone(), repo.repo_blobstore_arc())
        .try_collect::<Vec<_>>()
        .await?;

    assert_eq!(
        skeleton_manifest_leaf_entries
            .into_iter()
            .map(|(path, ())| path.to_string())
            .sorted()
            .collect::<Vec<_>>(),
        expected
            .iter()
            .sorted()
            .map(|path| path.to_string())
            .collect::<Vec<_>>()
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_skeleton_manifests_v2(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, changesets) = init_repo(&ctx).await?;

    assert_all_leaves(
        &ctx,
        &repo,
        changesets["A"],
        &[
            "A",
            "dir1/subdir1/subsubdir1/file1",
            "dir1/subdir1/subsubdir2/file2",
        ],
    )
    .await?;
    assert_all_leaves(
        &ctx,
        &repo,
        changesets["B"],
        &[
            "A",
            "B",
            "dir1/subdir1/subsubdir1/file1",
            "dir1/subdir1/subsubdir2/file2",
            "dir1/subdir1/subsubdir3/file3",
            "dir1/subdir2/subsubdir1/file1",
            "dir1/subdir2/subsubdir2/file1",
            "dir2/subdir1/subsubdir1/file1",
        ],
    )
    .await?;
    assert_all_leaves(
        &ctx,
        &repo,
        changesets["C"],
        &[
            "A",
            "B",
            "C",
            "dir1/subdir1/SUBSUBDIR2/file2",
            "dir1/subdir1/SUBSUBDIR3/FILE3",
            "dir1/subdir1/subsubdir1/FILE1",
            "dir1/subdir1/subsubdir1/file1",
            "dir1/subdir1/subsubdir2/file2",
            "dir1/subdir1/subsubdir3/file3",
            "dir1/subdir2/SUBSUBDIR1/file1",
            "dir1/subdir2/subsubdir1/file1",
            "dir1/subdir2/subsubdir2/file1",
            "dir2/subdir1/subsubdir1/file1",
        ],
    )
    .await?;
    assert_all_leaves(
        &ctx,
        &repo,
        changesets["D"],
        &[
            "A",
            "B",
            "C",
            "D",
            "dir1/subdir1/SUBSUBDIR2/file2",
            "dir1/subdir1/subsubdir1/FILE1",
            "dir1/subdir1/subsubdir1/file1",
            "dir1/subdir1/subsubdir3/file3",
            "dir1/subdir2/SUBSUBDIR1/file1",
            "dir1/subdir2/subsubdir1/file1",
            "dir1/subdir2/subsubdir2/file1",
            "dir2/subdir1/subsubdir1/file1",
        ],
    )
    .await?;
    assert_all_leaves(
        &ctx,
        &repo,
        changesets["E"],
        &[
            "A",
            "B",
            "C",
            "D",
            "E",
            "dir1/subdir1/SUBSUBDIR2/file2",
            "dir1/subdir1/subsubdir1/file1",
            "dir1/subdir1/subsubdir3/file3",
            "dir1/subdir2/SUBSUBDIR1/file1",
            "dir1/subdir2/subsubdir2/file1",
            "dir2/subdir1/subsubdir1/file1",
        ],
    )
    .await?;
    assert_all_leaves(
        &ctx,
        &repo,
        changesets["F"],
        &["F", "dir1/subdir1/SUBSUBDIR3/File3"],
    )
    .await?;
    assert_all_leaves(
        &ctx,
        &repo,
        changesets["G"],
        &[
            "A",
            "B",
            "C",
            "F",
            "G",
            "dir1/subdir1/SUBSUBDIR2/file2",
            "dir1/subdir1/SUBSUBDIR3/FILE3",
            "dir1/subdir1/SUBSUBDIR3/File3",
            "dir1/subdir1/subsubdir1/FILE1",
            "dir1/subdir1/subsubdir1/file1",
            "dir1/subdir1/subsubdir2/file2",
            "dir1/subdir1/subsubdir3/file3",
            "dir1/subdir2/SUBSUBDIR1/file1",
            "dir1/subdir2/subsubdir1/file1",
            "dir1/subdir2/subsubdir2/file1",
            "dir2/subdir1/subsubdir1/file1",
            "dir3/FILE1",
            "dir3/file1",
        ],
    )
    .await?;
    assert_all_leaves(
        &ctx,
        &repo,
        changesets["H"],
        &[
            "A",
            "B",
            "C",
            "H",
            "dir1/subdir1/SUBSUBDIR2/file2",
            "dir1/subdir1/SUBSUBDIR3/FILE3",
            "dir1/subdir1/subsubdir1/FILE1",
            "dir1/subdir1/subsubdir1/file1",
            "dir1/subdir1/subsubdir2/file2",
            "dir1/subdir1/subsubdir3/file3",
            "dir1/subdir2/SUBSUBDIR1/file1",
            "dir1/subdir2/subsubdir1/file1",
            "dir1/subdir2/subsubdir2/file1",
            "dir2/subdir1/subsubdir1/file1",
            "dir3/file2",
        ],
    )
    .await?;
    assert_all_leaves(
        &ctx,
        &repo,
        changesets["I"],
        &["I", "dir2/subdir1/subsubdir1/FILE1"],
    )
    .await?;
    assert_all_leaves(
        &ctx,
        &repo,
        changesets["J"],
        &[
            "A",
            "B",
            "C",
            "H",
            "I",
            "J",
            "dir1/subdir1/SUBSUBDIR2/file2",
            "dir1/subdir1/SUBSUBDIR3/FILE3",
            "dir1/subdir1/subsubdir1/FILE1",
            "dir1/subdir1/subsubdir1/file1",
            "dir1/subdir1/subsubdir2/file2",
            "dir1/subdir1/subsubdir3/file3",
            "dir1/subdir2/SUBSUBDIR1/file1",
            "dir1/subdir2/subsubdir1/file1",
            "dir1/subdir2/subsubdir2/file1",
            "dir2/subdir1/subsubdir1/FILE1",
            "dir2/subdir1/subsubdir1/file1",
            "dir2/subdir2/subsubdir1/file1",
            "dir3/file2",
        ],
    )
    .await?;

    Ok(())
}

/// Pipeline: derive top-level dirs independently, assemble the root; must equal full derivation.
#[mononoke::fbinit_test]
async fn test_derivation_pipeline_independent_dirs(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, changesets) = init_repo(&ctx).await?;
    let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
    let blobstore = repo.repo_blobstore().clone();

    let parent_csid = changesets["A"];
    let csid = changesets["B"];

    let parent_manifest = repo
        .repo_derived_data()
        .derive::<RootSkeletonManifestV2Id>(&ctx, parent_csid, DerivationPriority::LOW)
        .await?
        .into_inner_id()
        .load(&ctx, &blobstore)
        .await?;

    let expected_id = repo
        .repo_derived_data()
        .derive::<RootSkeletonManifestV2Id>(&ctx, csid, DerivationPriority::LOW)
        .await?
        .into_inner_id();

    let bcs = csid.load(&ctx, &blobstore).await?;
    let changes = get_file_changes(&bcs);

    // Derive each top-level directory subtree independently.
    let mut known_entries = HashMap::new();
    for dir in ["dir1", "dir2"] {
        let prefix = MPath::new(dir)?;
        let parents = match parent_manifest
            .find_entry(ctx.clone(), blobstore.clone(), prefix.clone())
            .await?
        {
            Some(entry) => vec![entry],
            None => vec![],
        };
        let entry = derive_skeleton_manifest_v2_entry(
            &ctx,
            &derivation_ctx,
            ManifestDerivationInput::Entry {
                parents: parents.into_iter().map(Some).collect(),
                known_entries: HashMap::new(),
                path: prefix.clone(),
            },
            changes.clone(),
        )
        .await?
        .into_entry()?;
        known_entries.insert(prefix, KnownEntry::Entry(entry));
    }

    // Assemble the root from the staged subtrees.
    let root_entry = derive_skeleton_manifest_v2_entry(
        &ctx,
        &derivation_ctx,
        ManifestDerivationInput::Entry {
            parents: vec![Some(Entry::Tree(parent_manifest))],
            known_entries,
            path: MPath::ROOT,
        },
        changes,
    )
    .await?
    .into_entry()?
    .expect("root stage should produce an entry");

    let pipeline_id = root_entry
        .into_tree()
        .expect("root entry should be a tree")
        .into_blob()
        .store(&ctx, &blobstore)
        .await?;

    // Content-addressed: equal ids imply identical trees.
    assert_eq!(pipeline_id, expected_id);

    Ok(())
}

/// Pipeline: derive a deep subtree, then its ancestor via known_entries, then the root.
#[mononoke::fbinit_test]
async fn test_derivation_pipeline_nested_dirs(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, changesets) = init_repo(&ctx).await?;
    let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
    let blobstore = repo.repo_blobstore().clone();

    // Commit D's changes are all under dir1/subdir1 (plus the top-level "D" file).
    let parent_csid = changesets["C"];
    let csid = changesets["D"];

    let parent_manifest = repo
        .repo_derived_data()
        .derive::<RootSkeletonManifestV2Id>(&ctx, parent_csid, DerivationPriority::LOW)
        .await?
        .into_inner_id()
        .load(&ctx, &blobstore)
        .await?;

    let expected_id = repo
        .repo_derived_data()
        .derive::<RootSkeletonManifestV2Id>(&ctx, csid, DerivationPriority::LOW)
        .await?
        .into_inner_id();

    let bcs = csid.load(&ctx, &blobstore).await?;
    let changes = get_file_changes(&bcs);

    // Stage the deep subtree first.
    let subdir1 = MPath::new("dir1/subdir1")?;
    let subdir1_parents = vec![
        parent_manifest
            .find_entry(ctx.clone(), blobstore.clone(), subdir1.clone())
            .await?,
    ];
    let subdir1_entry = derive_skeleton_manifest_v2_entry(
        &ctx,
        &derivation_ctx,
        ManifestDerivationInput::Entry {
            parents: subdir1_parents,
            known_entries: HashMap::new(),
            path: subdir1.clone(),
        },
        changes.clone(),
    )
    .await?
    .into_entry()?
    .expect("dir1/subdir1 stage should produce an entry");

    // Stage dir1 using the staged subdir1.
    let dir1 = MPath::new("dir1")?;
    let dir1_parents = vec![
        parent_manifest
            .find_entry(ctx.clone(), blobstore.clone(), dir1.clone())
            .await?,
    ];
    let dir1_known = HashMap::from([(subdir1, KnownEntry::Entry(Some(subdir1_entry)))]);
    let dir1_entry = derive_skeleton_manifest_v2_entry(
        &ctx,
        &derivation_ctx,
        ManifestDerivationInput::Entry {
            parents: dir1_parents,
            known_entries: dir1_known,
            path: dir1.clone(),
        },
        changes.clone(),
    )
    .await?
    .into_entry()?
    .expect("dir1 stage should produce an entry");

    // Assemble the root using the staged dir1.
    let root_known = HashMap::from([(dir1, KnownEntry::Entry(Some(dir1_entry)))]);
    let root_entry = derive_skeleton_manifest_v2_entry(
        &ctx,
        &derivation_ctx,
        ManifestDerivationInput::Entry {
            parents: vec![Some(Entry::Tree(parent_manifest))],
            known_entries: root_known,
            path: MPath::ROOT,
        },
        changes,
    )
    .await?
    .into_entry()?
    .expect("root stage should produce an entry");

    let pipeline_id = root_entry
        .into_tree()
        .expect("root entry should be a tree")
        .into_blob()
        .store(&ctx, &blobstore)
        .await?;

    assert_eq!(pipeline_id, expected_id);

    Ok(())
}
