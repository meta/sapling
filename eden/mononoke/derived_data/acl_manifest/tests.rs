/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;

use anyhow::Result;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use content_manifest_derivation::RootContentManifestId;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use derived_data_manager::ChildNameRange;
use derived_data_manager::ManifestDerivable;
use derived_data_manager::ManifestDerivationInput;
use derived_data_manager::ManifestStageOutput;
use derived_data_manager::PipelineDerivable;
use derived_data_manager::StageId;
use fbinit::FacebookInit;
use manifest::Entry;
use manifest::ManifestOps;
use mononoke_macros::mononoke;
use mononoke_types::BlobstoreKey;
use mononoke_types::ChangesetId;
use mononoke_types::ContentId;
use mononoke_types::MPath;
use mononoke_types::MPathElement;
use mononoke_types::acl_manifest::AclManifest;
use pretty_assertions::assert_eq;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataRef;
use tests_utils::CreateCommitContext;

use crate::AclChildNode;
use crate::DirectoryAclInputs;
use crate::RootAclManifestId;
use crate::acl_node_for_directory;
use crate::test_utils::*;

#[mononoke::fbinit_test]
async fn test_pipeline_reads_only_owned_acl_changes(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let counted = build_counted_test_repo(fb).await?;
    let repo = &counted.repo;
    let parent = CreateCommitContext::new_root(&ctx, repo)
        .add_file("a/.slacl", SLACL_PROJECT1)
        .add_file("z/.slacl", SLACL_PROJECT2)
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, repo, vec![parent])
        .add_file("a/.slacl", INVALID_SLACL)
        .commit()
        .await?;
    let parent = derive(&ctx, repo, parent).await?;
    let expected = derive(&ctx, repo, child).await?;
    assert_ne!(parent, expected);
    let bonsai = child.load(&ctx, repo.repo_blobstore()).await?;
    let content_id = bonsai
        .file_changes()
        .next()
        .unwrap()
        .1
        .simplify()
        .unwrap()
        .content_id();
    repo.repo_blobstore()
        .unlink(&ctx, &content_id.blobstore_key())
        .await?;
    assert!(
        filestore::fetch_concat(repo.repo_blobstore(), &ctx, content_id)
            .await
            .is_err()
    );
    let derivation = repo.repo_derived_data().manager().derivation_context(None);

    let unchanged = StageId::Subentries(MPath::ROOT, ChildNameRange::new(b'm', u8::MAX)?);
    let parent_output = RootAclManifestId::extract_stage_output_from_derived(
        &ctx,
        &derivation,
        &parent,
        &unchanged,
    )
    .await?;
    let input = ManifestDerivationInput::from_stage_outputs(
        &unchanged,
        vec![parent_output],
        HashMap::new(),
    )?;
    let before = counted.counters.snapshot();
    RootAclManifestId::derive_manifest(&ctx, &derivation, bonsai.clone(), input).await?;
    assert_eq!((counted.counters.snapshot() - before).gets, 0);

    let root = StageId::Manifest(MPath::ROOT);
    for dependencies in [
        vec![
            StageId::Manifest(MPath::new("a")?),
            StageId::Manifest(MPath::new("z")?),
        ],
        vec![
            StageId::Subentries(MPath::ROOT, ChildNameRange::new(0, b'm' - 1)?),
            unchanged,
        ],
    ] {
        let mut outputs = HashMap::new();
        for stage in dependencies {
            outputs.insert(
                stage.clone(),
                RootAclManifestId::extract_stage_output_from_derived(
                    &ctx,
                    &derivation,
                    &expected,
                    &stage,
                )
                .await?,
            );
        }
        let input = ManifestDerivationInput::from_stage_outputs(
            &root,
            vec![ManifestStageOutput::Entry(Some(Entry::Tree(
                *parent.inner_id(),
            )))],
            outputs,
        )?;
        let actual = RootAclManifestId::derive_manifest(&ctx, &derivation, bonsai.clone(), input)
            .await?
            .into_entry()?;
        assert_eq!(actual, Some(Entry::Tree(*expected.inner_id())));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Incremental derivation tests
// ---------------------------------------------------------------------------

/// Test incremental derivation: parent has no .slacl, child adds one.
#[mononoke::fbinit_test]
async fn test_derive_incremental_add(fb: FacebookInit) -> Result<()> {
    let result = setup_and_derive(
        fb,
        vec![
            vec![Change::Add("dir/file.txt", b"content")],
            vec![Change::Add("dir/.slacl", SLACL_PROJECT1)],
        ],
    )
    .await?;

    assert_eq!(Vec::<ExpectedNode>::new(), result.tree(0).await?);

    assert_eq!(
        vec![node(
            "dir",
            Some("REPO_REGION:repos/hg/fbsource/=project1"),
            vec![]
        )],
        result.last_tree().await?,
    );

    Ok(())
}

/// Test incremental derivation: parent has .slacl, child removes it.
#[mononoke::fbinit_test]
async fn test_derive_incremental_remove(fb: FacebookInit) -> Result<()> {
    let result = setup_and_derive(
        fb,
        vec![
            vec![
                Change::Add("dir/file.txt", b"content"),
                Change::Add("dir/.slacl", SLACL_PROJECT1),
            ],
            vec![Change::Delete("dir/.slacl")],
        ],
    )
    .await?;

    assert_eq!(
        vec![node(
            "dir",
            Some("REPO_REGION:repos/hg/fbsource/=project1"),
            vec![]
        )],
        result.tree(0).await?,
    );

    assert_eq!(Vec::<ExpectedNode>::new(), result.last_tree().await?);

    Ok(())
}

/// Test incremental derivation: changing .slacl content changes entry_blob_id.
#[mononoke::fbinit_test]
async fn test_derive_incremental_modify(fb: FacebookInit) -> Result<()> {
    let result = setup_and_derive(
        fb,
        vec![
            vec![
                Change::Add("dir/file.txt", b"content"),
                Change::Add("dir/.slacl", SLACL_PROJECT1),
            ],
            vec![Change::Add("dir/.slacl", SLACL_PROJECT2)],
        ],
    )
    .await?;

    assert_eq!(
        vec![node(
            "dir",
            Some("REPO_REGION:repos/hg/fbsource/=project2"),
            vec![]
        )],
        result.last_tree().await?,
    );

    // Verify entry_blob_ids differ
    let parent_entries =
        load_entries(&result.ctx, &result.repo, result.manifest_ids[0].inner_id()).await?;
    let child_entries =
        load_entries(&result.ctx, &result.repo, result.manifest_ids[1].inner_id()).await?;
    let parent_blob = expect_restricted(&result.ctx, &result.repo, &parent_entries[0].1.id).await?;
    let child_blob = expect_restricted(&result.ctx, &result.repo, &child_entries[0].1.id).await?;
    assert_ne!(
        parent_blob, child_blob,
        "modifying .slacl should produce a different entry_blob_id"
    );

    Ok(())
}

/// What it tests: incrementally adding an invalid .slacl file.
/// Expected: derivation succeeds and the invalid file creates no ACL manifest leaf.
#[mononoke::fbinit_test]
async fn test_incremental_invalid_slacl_add_creates_no_restriction(fb: FacebookInit) -> Result<()> {
    let result = setup_and_derive(
        fb,
        vec![vec![
            Change::Add("dir/file.txt", b"content"),
            Change::Add("dir/.slacl", INVALID_SLACL),
        ]],
    )
    .await?;

    assert_eq!(Vec::<ExpectedNode>::new(), result.last_tree().await?);

    Ok(())
}

/// What it tests: incrementally modifying an existing .slacl file to invalid content.
/// Expected: derivation succeeds and the old manifest entry is deleted.
#[mononoke::fbinit_test]
async fn test_incremental_invalid_slacl_modify_removes_existing_restriction(
    fb: FacebookInit,
) -> Result<()> {
    let result = setup_and_derive(
        fb,
        vec![
            vec![
                Change::Add("dir/file.txt", b"content"),
                Change::Add("dir/.slacl", SLACL_PROJECT1),
            ],
            vec![Change::Add("dir/.slacl", INVALID_SLACL)],
        ],
    )
    .await?;

    assert_eq!(
        vec![node(
            "dir",
            Some("REPO_REGION:repos/hg/fbsource/=project1"),
            vec![]
        )],
        result.tree(0).await?,
    );
    assert_eq!(Vec::<ExpectedNode>::new(), result.last_tree().await?);

    Ok(())
}

/// Test incremental: add nested restrictions to existing restriction roots.
/// Adds a restriction nested UNDER alpha, and a restriction ABOVE beta/deep.
#[mononoke::fbinit_test]
async fn test_incremental_add_nested_restriction(fb: FacebookInit) -> Result<()> {
    let result = setup_and_derive(
        fb,
        vec![
            vec![
                Change::Add("alpha/.slacl", SLACL_PROJECT1),
                Change::Add("alpha/file.txt", b"content"),
                Change::Add("beta/deep/.slacl", SLACL_PROJECT2),
                Change::Add("beta/deep/file.txt", b"content"),
            ],
            vec![
                Change::Add("alpha/inner/.slacl", SLACL_PROJECT2),
                Change::Add("alpha/inner/file.txt", b"content"),
                Change::Add("beta/.slacl", SLACL_PROJECT1),
            ],
        ],
    )
    .await?;

    assert_eq!(
        vec![
            node(
                "alpha",
                Some("REPO_REGION:repos/hg/fbsource/=project1"),
                vec![]
            ),
            node(
                "beta",
                None,
                vec![node(
                    "deep",
                    Some("REPO_REGION:repos/hg/fbsource/=project2"),
                    vec![]
                )]
            ),
        ],
        result.tree(0).await?,
    );

    assert_eq!(
        vec![
            node(
                "alpha",
                Some("REPO_REGION:repos/hg/fbsource/=project1"),
                vec![node(
                    "inner",
                    Some("REPO_REGION:repos/hg/fbsource/=project2"),
                    vec![]
                )]
            ),
            node(
                "beta",
                Some("REPO_REGION:repos/hg/fbsource/=project1"),
                vec![node(
                    "deep",
                    Some("REPO_REGION:repos/hg/fbsource/=project2"),
                    vec![]
                )]
            ),
        ],
        result.last_tree().await?,
    );

    Ok(())
}

/// Test incremental: remove nested restrictions from nested pairs.
/// Removes the nested restriction FROM alpha, and the parent restriction ABOVE beta/deep.
#[mononoke::fbinit_test]
async fn test_incremental_remove_nested_restriction(fb: FacebookInit) -> Result<()> {
    let result = setup_and_derive(
        fb,
        vec![
            vec![
                Change::Add("alpha/.slacl", SLACL_PROJECT1),
                Change::Add("alpha/inner/.slacl", SLACL_PROJECT2),
                Change::Add("alpha/inner/file.txt", b"content"),
                Change::Add("beta/.slacl", SLACL_PROJECT1),
                Change::Add("beta/deep/.slacl", SLACL_PROJECT2),
                Change::Add("beta/deep/file.txt", b"content"),
            ],
            vec![
                Change::Delete("alpha/inner/.slacl"),
                Change::Delete("beta/.slacl"),
            ],
        ],
    )
    .await?;

    assert_eq!(
        vec![
            node(
                "alpha",
                Some("REPO_REGION:repos/hg/fbsource/=project1"),
                vec![node(
                    "inner",
                    Some("REPO_REGION:repos/hg/fbsource/=project2"),
                    vec![]
                )]
            ),
            node(
                "beta",
                Some("REPO_REGION:repos/hg/fbsource/=project1"),
                vec![node(
                    "deep",
                    Some("REPO_REGION:repos/hg/fbsource/=project2"),
                    vec![]
                )]
            ),
        ],
        result.tree(0).await?,
    );

    assert_eq!(
        vec![
            node(
                "alpha",
                Some("REPO_REGION:repos/hg/fbsource/=project1"),
                vec![]
            ),
            node(
                "beta",
                None,
                vec![node(
                    "deep",
                    Some("REPO_REGION:repos/hg/fbsource/=project2"),
                    vec![]
                )]
            ),
        ],
        result.last_tree().await?,
    );

    Ok(())
}

/// Test incremental: modify nested restrictions in nested pairs.
/// Modifies the nested restriction IN alpha, and the parent restriction ABOVE beta/deep.
#[mononoke::fbinit_test]
async fn test_incremental_modify_nested_restriction(fb: FacebookInit) -> Result<()> {
    let result = setup_and_derive(
        fb,
        vec![
            vec![
                Change::Add("alpha/.slacl", SLACL_PROJECT1),
                Change::Add("alpha/inner/.slacl", SLACL_PROJECT2),
                Change::Add("alpha/inner/file.txt", b"content"),
                Change::Add("beta/.slacl", SLACL_PROJECT1),
                Change::Add("beta/deep/.slacl", SLACL_PROJECT2),
                Change::Add("beta/deep/file.txt", b"content"),
            ],
            vec![
                Change::Add("alpha/inner/.slacl", SLACL_PROJECT1),
                Change::Add("beta/.slacl", SLACL_PROJECT2),
            ],
        ],
    )
    .await?;

    // Before: alpha/inner=project2, beta=project1
    assert_eq!(
        vec![
            node(
                "alpha",
                Some("REPO_REGION:repos/hg/fbsource/=project1"),
                vec![node(
                    "inner",
                    Some("REPO_REGION:repos/hg/fbsource/=project2"),
                    vec![]
                )]
            ),
            node(
                "beta",
                Some("REPO_REGION:repos/hg/fbsource/=project1"),
                vec![node(
                    "deep",
                    Some("REPO_REGION:repos/hg/fbsource/=project2"),
                    vec![]
                )]
            ),
        ],
        result.tree(0).await?,
    );

    // After: alpha/inner changed to project1, beta changed to project2
    assert_eq!(
        vec![
            node(
                "alpha",
                Some("REPO_REGION:repos/hg/fbsource/=project1"),
                vec![node(
                    "inner",
                    Some("REPO_REGION:repos/hg/fbsource/=project1"),
                    vec![]
                )]
            ),
            node(
                "beta",
                Some("REPO_REGION:repos/hg/fbsource/=project2"),
                vec![node(
                    "deep",
                    Some("REPO_REGION:repos/hg/fbsource/=project2"),
                    vec![]
                )]
            ),
        ],
        result.last_tree().await?,
    );

    Ok(())
}

/// Test content-addressing: identical .slacl files at different paths
/// should produce the same AclManifestEntryBlobId.
#[mononoke::fbinit_test]
async fn test_entry_blob_content_addressing(fb: FacebookInit) -> Result<()> {
    let result = setup_and_derive(
        fb,
        vec![vec![
            Change::Add("alpha/file.txt", b"content a"),
            Change::Add("alpha/.slacl", SLACL_PROJECT1),
            Change::Add("beta/file.txt", b"content b"),
            Change::Add("beta/.slacl", SLACL_PROJECT1),
        ]],
    )
    .await?;

    let mut entries = load_entries(&result.ctx, &result.repo, result.last_id().inner_id()).await?;
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));

    let alpha_blob = expect_restricted(&result.ctx, &result.repo, &entries[0].1.id).await?;
    let beta_blob = expect_restricted(&result.ctx, &result.repo, &entries[1].1.id).await?;

    assert_eq!(
        alpha_blob, beta_blob,
        "identical .slacl content should produce the same entry_blob_id"
    );

    Ok(())
}

/// Test content-addressing at the directory level: an identical nested subtree
/// placed at two different paths should produce the same directory-node
/// `AclManifestId`. This is the property the augmented-manifest copied-tree ACL
/// re-stamp relies on -- a copied directory's stored ACL pointer equals what the
/// destination overlay would compute, because the id is derived from
/// `{restriction, subentries}` with no path baked in.
#[mononoke::fbinit_test]
async fn test_identical_subtree_has_same_directory_id_at_different_paths(
    fb: FacebookInit,
) -> Result<()> {
    // Given: the same nested `.slacl` subtree under two different top-level dirs.
    let result = setup_and_derive(
        fb,
        vec![vec![
            Change::Add("alpha/inner/.slacl", SLACL_PROJECT1),
            Change::Add("beta/inner/.slacl", SLACL_PROJECT1),
        ]],
    )
    .await?;

    // When: load the root's directory entries.
    let mut entries = load_entries(&result.ctx, &result.repo, result.last_id().inner_id()).await?;
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));

    // Then: the two waypoint directories are `alpha` and `beta`, and their
    // directory-node AclManifestIds are equal despite the differing paths.
    assert_eq!(
        vec!["alpha".to_string(), "beta".to_string()],
        entries
            .iter()
            .map(|(name, _)| String::from_utf8(name.as_ref().to_vec()))
            .collect::<std::result::Result<Vec<_>, _>>()?,
        "expected exactly the alpha and beta waypoint directories at the root"
    );
    assert_eq!(
        entries[0].1.id, entries[1].1.id,
        "identical subtree content should produce the same directory-node AclManifestId regardless of path"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Upload-time single-directory construction
//
// The oracle is canonical derivation of the same commit: build one directory's
// node from that directory's own contents and assert it is the node the
// changeset-driven traversal produced for the same directory. `AclManifestId`
// is a content hash, so agreeing on the entry is agreeing on the bytes.
// ---------------------------------------------------------------------------

/// Resolve the `ContentId` of a file in `cs_id`'s tree, the way a caller with a
/// manifest in hand would already know it.
async fn content_id_at_path(
    ctx: &CoreContext,
    repo: &TestRepo,
    cs_id: ChangesetId,
    path: &str,
) -> Result<ContentId> {
    let root_content_manifest_id = repo
        .repo_derived_data()
        .derive::<RootContentManifestId>(ctx, cs_id, DerivationPriority::LOW)
        .await?;
    let entry = root_content_manifest_id
        .content_manifest_id()
        .find_entry(
            ctx.clone(),
            repo.repo_blobstore().clone(),
            MPath::new(path)?,
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("{path} not found in ContentManifests"))?;
    match entry {
        Entry::Leaf(file) => Ok(file.content_id),
        Entry::Tree(_) => anyhow::bail!("{path} is a directory"),
    }
}

/// A directory that is only a waypoint: no ACL file of its own, one restricted
/// child. Passing that child is enough to reproduce the derived node.
#[mononoke::fbinit_test]
async fn test_acl_node_for_directory_matches_derivation_for_a_restricted_child(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let cs_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/child/.slacl", SLACL_PROJECT1)
        .add_file("dir/child/file.txt", "content")
        .commit()
        .await?;
    let root_id = derive(&ctx, &repo, cs_id).await?;

    let child = directory_entry_at_path(&ctx, &repo, root_id.inner_id(), &["dir", "child"])
        .await?
        .ok_or_else(|| anyhow::anyhow!("dir/child missing from the derived manifest"))?;
    let expected = directory_entry_at_path(&ctx, &repo, root_id.inner_id(), &["dir"]).await?;
    let blobstore = repo.repo_blobstore().boxed();

    let actual = acl_node_for_directory(
        &ctx,
        &blobstore,
        DirectoryAclInputs {
            acl_file_name: ".slacl",
            own_acl_file: None,
            children: BTreeMap::from([(
                MPathElement::new(b"child".to_vec())?,
                AclChildNode::Known(child),
            )]),
        },
    )
    .await?;

    assert!(
        expected.is_some(),
        "dir should be a waypoint above the restricted child"
    );
    assert_eq!(expected, actual);

    Ok(())
}

/// The same fixture with only the child's id supplied: the builder must recover
/// the child's flags by loading it, and land on the same node.
#[mononoke::fbinit_test]
async fn test_acl_node_for_directory_recovers_child_flags_from_the_id_alone(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let cs_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/child/.slacl", SLACL_PROJECT1)
        .add_file("dir/child/file.txt", "content")
        .commit()
        .await?;
    let root_id = derive(&ctx, &repo, cs_id).await?;

    let child = directory_entry_at_path(&ctx, &repo, root_id.inner_id(), &["dir", "child"])
        .await?
        .ok_or_else(|| anyhow::anyhow!("dir/child missing from the derived manifest"))?;
    let expected = directory_entry_at_path(&ctx, &repo, root_id.inner_id(), &["dir"]).await?;
    let blobstore = repo.repo_blobstore().boxed();

    let actual = acl_node_for_directory(
        &ctx,
        &blobstore,
        DirectoryAclInputs {
            acl_file_name: ".slacl",
            own_acl_file: None,
            children: BTreeMap::from([(
                MPathElement::new(b"child".to_vec())?,
                AclChildNode::IdOnly(child.id),
            )]),
        },
    )
    .await?;

    assert!(expected.is_some(), "dir should be a waypoint");
    assert_eq!(expected, actual);

    Ok(())
}

/// A restriction root with no ACL-bearing children: the directory's own ACL
/// file is the whole input.
#[mononoke::fbinit_test]
async fn test_acl_node_for_directory_matches_derivation_for_its_own_acl_file(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let cs_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/.slacl", SLACL_PROJECT1)
        .add_file("dir/file.txt", "content")
        .commit()
        .await?;
    let root_id = derive(&ctx, &repo, cs_id).await?;

    let own_acl_file = content_id_at_path(&ctx, &repo, cs_id, "dir/.slacl").await?;
    let expected = directory_entry_at_path(&ctx, &repo, root_id.inner_id(), &["dir"]).await?;
    let blobstore = repo.repo_blobstore().boxed();

    let actual = acl_node_for_directory(
        &ctx,
        &blobstore,
        DirectoryAclInputs {
            acl_file_name: ".slacl",
            own_acl_file: Some(own_acl_file),
            children: BTreeMap::new(),
        },
    )
    .await?;

    assert!(
        expected.is_some(),
        "a directory with its own .slacl should be a restriction root"
    );
    assert_eq!(expected, actual);

    Ok(())
}

/// A restriction root with a restricted child: the helper must agree with
/// canonical derivation when both `is_restricted` and
/// `has_restricted_descendants` are true.
#[mononoke::fbinit_test]
async fn test_acl_node_for_directory_matches_derivation_for_a_restriction_root_with_a_restricted_child(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let cs_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/.slacl", SLACL_PROJECT1)
        .add_file("dir/child/.slacl", SLACL_PROJECT2)
        .add_file("dir/child/file.txt", "content")
        .commit()
        .await?;
    let root_id = derive(&ctx, &repo, cs_id).await?;

    let own_acl_file = content_id_at_path(&ctx, &repo, cs_id, "dir/.slacl").await?;
    let child = directory_entry_at_path(&ctx, &repo, root_id.inner_id(), &["dir", "child"])
        .await?
        .ok_or_else(|| anyhow::anyhow!("dir/child missing from the derived manifest"))?;
    let expected = directory_entry_at_path(&ctx, &repo, root_id.inner_id(), &["dir"]).await?;
    let blobstore = repo.repo_blobstore().boxed();

    let actual = acl_node_for_directory(
        &ctx,
        &blobstore,
        DirectoryAclInputs {
            acl_file_name: ".slacl",
            own_acl_file: Some(own_acl_file),
            children: BTreeMap::from([(
                MPathElement::new(b"child".to_vec())?,
                AclChildNode::Known(child),
            )]),
        },
    )
    .await?;

    assert!(
        expected.is_some(),
        "a directory with its own .slacl and a restricted child should produce a node"
    );
    assert_eq!(expected, actual);

    Ok(())
}

/// Nothing restricted here or below: derivation records no node, and the
/// builder must agree rather than storing an empty one.
#[mononoke::fbinit_test]
async fn test_acl_node_for_directory_is_absent_for_an_unrestricted_directory(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let cs_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/file.txt", "content")
        .add_file("restricted/.slacl", SLACL_PROJECT1)
        .add_file("restricted/file.txt", "content")
        .commit()
        .await?;
    let root_id = derive(&ctx, &repo, cs_id).await?;

    let restricted_sibling =
        directory_entry_at_path(&ctx, &repo, root_id.inner_id(), &["restricted"]).await?;
    let expected = directory_entry_at_path(&ctx, &repo, root_id.inner_id(), &["dir"]).await?;
    let blobstore = repo.repo_blobstore().boxed();

    let actual = acl_node_for_directory(
        &ctx,
        &blobstore,
        DirectoryAclInputs {
            acl_file_name: ".slacl",
            own_acl_file: None,
            children: BTreeMap::new(),
        },
    )
    .await?;

    assert!(
        restricted_sibling.is_some(),
        "the fixture's restricted sibling should be present in the derived manifest"
    );
    assert_eq!(None, expected);
    assert_eq!(expected, actual);

    Ok(())
}

/// The canonical empty ACL id spells absence everywhere else, so a child
/// carrying it must not become a phantom restricted entry.
#[mononoke::fbinit_test]
async fn test_acl_node_for_directory_treats_an_empty_child_id_as_absent(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let blobstore = repo.repo_blobstore().boxed();

    let actual = acl_node_for_directory(
        &ctx,
        &blobstore,
        DirectoryAclInputs {
            acl_file_name: ".slacl",
            own_acl_file: None,
            children: BTreeMap::from([(
                MPathElement::new(b"child".to_vec())?,
                AclChildNode::IdOnly(AclManifest::empty_id()),
            )]),
        },
    )
    .await?;

    assert_eq!(
        None, actual,
        "canonical empty ACL ids should be treated as absent inputs"
    );

    Ok(())
}

/// The ACL file and the children share one key space, so a child named like the
/// ACL file would replace the restriction leaf and the node would come back
/// unrestricted. A manifest name is either a file or a directory, so this can
/// only be a caller error -- and a wrong pointer here is permanent.
#[mononoke::fbinit_test]
async fn test_acl_node_for_directory_rejects_a_child_named_like_the_acl_file(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let cs_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/.slacl", SLACL_PROJECT1)
        .add_file("dir/child/.slacl", SLACL_PROJECT1)
        .commit()
        .await?;
    let root_id = derive(&ctx, &repo, cs_id).await?;

    let own_acl_file = content_id_at_path(&ctx, &repo, cs_id, "dir/.slacl").await?;
    let child = directory_entry_at_path(&ctx, &repo, root_id.inner_id(), &["dir", "child"])
        .await?
        .ok_or_else(|| anyhow::anyhow!("dir/child missing from the derived manifest"))?;
    let blobstore = repo.repo_blobstore().boxed();

    let err = acl_node_for_directory(
        &ctx,
        &blobstore,
        DirectoryAclInputs {
            acl_file_name: ".slacl",
            own_acl_file: Some(own_acl_file),
            children: BTreeMap::from([(
                MPathElement::new(b".slacl".to_vec())?,
                AclChildNode::Known(child),
            )]),
        },
    )
    .await
    .expect_err("a child colliding with the acl file name should be rejected");

    assert!(
        err.to_string().contains("collides with acl file name"),
        "expected a collision error, got: {err}"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Incremental derivation edge-case tests
// ---------------------------------------------------------------------------

/// Test incremental: remove a restriction root that has restricted descendants.
/// The node should become a waypoint, not be pruned.
#[mononoke::fbinit_test]
async fn test_incremental_remove_keeps_waypoint(fb: FacebookInit) -> Result<()> {
    let result = setup_and_derive(
        fb,
        vec![
            vec![
                Change::Add("foo/.slacl", SLACL_PROJECT1),
                Change::Add("foo/bar/.slacl", SLACL_PROJECT2),
                Change::Add("foo/bar/file.txt", b"content"),
            ],
            vec![Change::Delete("foo/.slacl")],
        ],
    )
    .await?;

    // Parent commit: foo was restricted with project1
    assert_eq!(
        vec![node(
            "foo",
            Some("REPO_REGION:repos/hg/fbsource/=project1"),
            vec![node(
                "bar",
                Some("REPO_REGION:repos/hg/fbsource/=project2"),
                vec![]
            )],
        )],
        result.tree(0).await?,
    );

    // Child commit: foo is now an unrestricted waypoint
    assert_eq!(
        vec![node(
            "foo",
            None, // foo is now unrestricted (waypoint)
            vec![node(
                "bar",
                Some("REPO_REGION:repos/hg/fbsource/=project2"),
                vec![]
            )],
        )],
        result.last_tree().await?,
    );

    Ok(())
}

/// Test incremental: unchanged subtree is reused by ID.
#[mononoke::fbinit_test]
async fn test_incremental_reuses_unchanged_subtree(fb: FacebookInit) -> Result<()> {
    let result = setup_and_derive(
        fb,
        vec![
            vec![
                Change::Add("alpha/.slacl", SLACL_PROJECT1),
                Change::Add("alpha/file.txt", b"content"),
                Change::Add("beta/.slacl", SLACL_PROJECT2),
                Change::Add("beta/file.txt", b"content"),
            ],
            vec![Change::Add("alpha/.slacl", SLACL_PROJECT2)],
        ],
    )
    .await?;

    let parent_entries =
        load_entries(&result.ctx, &result.repo, result.manifest_ids[0].inner_id()).await?;
    let child_entries =
        load_entries(&result.ctx, &result.repo, result.manifest_ids[1].inner_id()).await?;

    let parent_beta = parent_entries
        .iter()
        .find(|(name, _)| name.as_ref() == b"beta")
        .map(|(_, e)| e.id)
        .ok_or_else(|| anyhow::anyhow!("beta not found in parent"))?;

    let child_beta = child_entries
        .iter()
        .find(|(name, _)| name.as_ref() == b"beta")
        .map(|(_, e)| e.id)
        .ok_or_else(|| anyhow::anyhow!("beta not found in child"))?;

    assert_eq!(
        parent_beta, child_beta,
        "unchanged beta subtree should be reused by ID"
    );

    Ok(())
}

/// Test incremental: no ACL changes → parent manifest reused entirely.
#[mononoke::fbinit_test]
async fn test_incremental_no_changes_reuses_parent(fb: FacebookInit) -> Result<()> {
    let result = setup_and_derive(
        fb,
        vec![
            vec![
                Change::Add("dir/.slacl", SLACL_PROJECT1),
                Change::Add("dir/file.txt", b"content"),
            ],
            vec![Change::Add("dir/file.txt", b"updated content")],
        ],
    )
    .await?;

    assert_eq!(
        result.manifest_ids[0].inner_id(),
        result.manifest_ids[1].inner_id(),
        "no ACL changes should reuse parent manifest exactly"
    );

    Ok(())
}

/// Test incremental: implicit deletes (adding a file at a directory path).
/// alpha/sub has nested restriction roots (sub + sub/inner); beta has nested
/// restriction roots (beta + beta/deep). The child commit implicitly deletes
/// the nested root in alpha (alpha/sub/inner) and implicitly deletes beta
/// entirely by adding regular files at those paths.
#[mononoke::fbinit_test]
async fn test_incremental_implicit_delete(fb: FacebookInit) -> Result<()> {
    let result = setup_and_derive(
        fb,
        vec![
            vec![
                Change::Add("alpha/sub/.slacl", SLACL_PROJECT1),
                Change::Add("alpha/sub/inner/.slacl", SLACL_PROJECT2),
                Change::Add("alpha/sub/inner/file.txt", b"content"),
                Change::Add("beta/.slacl", SLACL_PROJECT1),
                Change::Add("beta/deep/.slacl", SLACL_PROJECT2),
                Change::Add("beta/deep/file.txt", b"content"),
            ],
            vec![
                // Implicitly delete alpha/sub/inner dir by adding file at that path
                Change::Add("alpha/sub/inner", b"now a file"),
                // Implicitly delete beta dir by adding file at that path
                Change::Add("beta", b"now a file"),
            ],
        ],
    )
    .await?;

    // Before: nested restrictions in both subtrees
    assert_eq!(
        vec![
            node(
                "alpha",
                None,
                vec![node(
                    "sub",
                    Some("REPO_REGION:repos/hg/fbsource/=project1"),
                    vec![node(
                        "inner",
                        Some("REPO_REGION:repos/hg/fbsource/=project2"),
                        vec![]
                    )]
                )]
            ),
            node(
                "beta",
                Some("REPO_REGION:repos/hg/fbsource/=project1"),
                vec![node(
                    "deep",
                    Some("REPO_REGION:repos/hg/fbsource/=project2"),
                    vec![]
                )]
            ),
        ],
        result.tree(0).await?,
    );

    // After: alpha/sub/inner gone (sub remains), beta gone entirely
    assert_eq!(
        vec![node(
            "alpha",
            None,
            vec![node(
                "sub",
                Some("REPO_REGION:repos/hg/fbsource/=project1"),
                vec![]
            )]
        )],
        result.last_tree().await?,
    );

    Ok(())
}

/// Test merge: two branches add restriction roots in different directories.
/// The merge combines both without conflict.
#[mononoke::fbinit_test]
async fn test_merge_no_conflict(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("shared/file.txt", "content")
        .commit()
        .await?;

    // Left branch: adds alpha restriction
    let left = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("alpha/.slacl", SLACL_PROJECT1)
        .add_file("alpha/file.txt", "content")
        .commit()
        .await?;

    // Right branch: adds beta restriction
    let right = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("beta/.slacl", SLACL_PROJECT2)
        .add_file("beta/file.txt", "content")
        .commit()
        .await?;

    // Merge combines both branches
    let merge = CreateCommitContext::new(&ctx, &repo, vec![left, right])
        .commit()
        .await?;

    let merge_id = derive(&ctx, &repo, merge).await?;
    let tree = actual_tree(&ctx, &repo, merge_id.inner_id()).await?;

    assert_eq!(
        vec![
            node(
                "alpha",
                Some("REPO_REGION:repos/hg/fbsource/=project1"),
                vec![]
            ),
            node(
                "beta",
                Some("REPO_REGION:repos/hg/fbsource/=project2"),
                vec![]
            ),
        ],
        tree,
    );

    Ok(())
}

/// Test merge: two branches add conflicting .slacl at the same path.
/// The merge commit resolves the conflict; the derived manifest reflects
/// the merge commit's file contents, not either parent's.
#[mononoke::fbinit_test]
async fn test_merge_conflicting_restriction(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/file.txt", "content")
        .commit()
        .await?;

    // Left: adds dir/.slacl = project1
    let left = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("dir/.slacl", SLACL_PROJECT1)
        .commit()
        .await?;

    // Right: adds dir/.slacl = project2 (conflict with left)
    let right = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("dir/.slacl", SLACL_PROJECT2)
        .commit()
        .await?;

    // Merge resolves conflict: picks project2
    let merge = CreateCommitContext::new(&ctx, &repo, vec![left, right])
        .add_file("dir/.slacl", SLACL_PROJECT2)
        .commit()
        .await?;

    let merge_id = derive(&ctx, &repo, merge).await?;
    let tree = actual_tree(&ctx, &repo, merge_id.inner_id()).await?;

    assert_eq!(
        vec![node(
            "dir",
            Some("REPO_REGION:repos/hg/fbsource/=project2"),
            vec![]
        )],
        tree,
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// From-scratch derivation tests
// ---------------------------------------------------------------------------

/// Test that deriving from scratch with no .slacl files produces an empty manifest.
#[mononoke::fbinit_test]
async fn test_derive_empty_repo(fb: FacebookInit) -> Result<()> {
    let result =
        setup_and_derive_from_scratch(fb, vec![vec![Change::Add("dir/file.txt", b"content")]])
            .await?;

    assert_eq!(Vec::<ExpectedNode>::new(), result.last_tree().await?);

    Ok(())
}

/// Test from-scratch: a single .slacl file at dir/.slacl makes "dir" a restriction root.
#[mononoke::fbinit_test]
async fn test_derive_single_restriction_root(fb: FacebookInit) -> Result<()> {
    let result = setup_and_derive_from_scratch(
        fb,
        vec![vec![
            Change::Add("dir/file.txt", b"content"),
            Change::Add("dir/.slacl", SLACL_PROJECT1),
        ]],
    )
    .await?;

    assert_eq!(
        vec![node(
            "dir",
            Some("REPO_REGION:repos/hg/fbsource/=project1"),
            vec![]
        )],
        result.last_tree().await?,
    );

    // Also verify permission_request_group is None (default)
    let tree = result.last_tree().await?;
    assert_eq!(tree[0].permission_request_group, None);

    Ok(())
}

/// Test from-scratch: nested .slacl files at foo/.slacl and foo/bar/.slacl
#[mononoke::fbinit_test]
async fn test_derive_nested_restrictions(fb: FacebookInit) -> Result<()> {
    let result = setup_and_derive_from_scratch(
        fb,
        vec![vec![
            Change::Add("foo/file.txt", b"content"),
            Change::Add("foo/.slacl", SLACL_PROJECT1),
            Change::Add("foo/bar/file.txt", b"content"),
            Change::Add("foo/bar/.slacl", SLACL_PROJECT2),
        ]],
    )
    .await?;

    assert_eq!(
        vec![node(
            "foo",
            Some("REPO_REGION:repos/hg/fbsource/=project1"),
            vec![node(
                "bar",
                Some("REPO_REGION:repos/hg/fbsource/=project2"),
                vec![]
            )],
        )],
        result.last_tree().await?,
    );

    Ok(())
}

/// Test from-scratch: two independent .slacl files in separate branches of the tree.
#[mononoke::fbinit_test]
async fn test_derive_multiple_independent_roots(fb: FacebookInit) -> Result<()> {
    let result = setup_and_derive_from_scratch(
        fb,
        vec![vec![
            Change::Add("proj/private/file.txt", b"content"),
            Change::Add("proj/private/.slacl", SLACL_PROJECT1),
            Change::Add("team/secret/file.txt", b"content"),
            Change::Add("team/secret/.slacl", SLACL_PROJECT2),
        ]],
    )
    .await?;

    assert_eq!(
        vec![
            node(
                "proj",
                None,
                vec![node(
                    "private",
                    Some("REPO_REGION:repos/hg/fbsource/=project1"),
                    vec![]
                )],
            ),
            node(
                "team",
                None,
                vec![node(
                    "secret",
                    Some("REPO_REGION:repos/hg/fbsource/=project2"),
                    vec![]
                )],
            ),
        ],
        result.last_tree().await?,
    );

    Ok(())
}

/// What it tests: from-scratch derivation with both valid and invalid .slacl files.
/// Expected: derivation succeeds and omits only the invalid ACL file.
#[mononoke::fbinit_test]
async fn test_from_scratch_invalid_slacl_is_omitted(fb: FacebookInit) -> Result<()> {
    let result = setup_and_derive_from_scratch(
        fb,
        vec![vec![
            Change::Add("valid/file.txt", b"content"),
            Change::Add("valid/.slacl", SLACL_PROJECT1),
            Change::Add("invalid/file.txt", b"content"),
            Change::Add("invalid/.slacl", INVALID_SLACL),
        ]],
    )
    .await?;

    assert_eq!(
        vec![node(
            "valid",
            Some("REPO_REGION:repos/hg/fbsource/=project1"),
            vec![]
        )],
        result.last_tree().await?,
    );

    Ok(())
}
// TODO(T248660053): add tests with subtree copies and merges.

// ---------------------------------------------------------------------------
// Perf counter tests — derivation complexity
// ---------------------------------------------------------------------------

/// Test that incremental derivation is cheaper than from-scratch derivation
/// by making the same kind of change and deriving it both ways.
#[mononoke::fbinit_test]
async fn test_derive_incremental_is_cheaper_than_from_scratch(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let counted = build_counted_test_repo(fb).await?;
    let repo = &counted.repo;

    // Setup: base commit with 5 restriction roots.
    let base_changes: Vec<_> = (0..5)
        .flat_map(|i| {
            let dir = format!("dir_{i}");
            let slacl = format!("{dir}/.slacl");
            let file = format!("{dir}/file.txt");
            vec![(slacl, unique_slacl(i)), (file, b"content" as &[u8])]
        })
        .collect();

    let base_cs = base_changes
        .iter()
        .fold(CreateCommitContext::new_root(&ctx, repo), |b, (p, c)| {
            b.add_file(p.as_str(), *c)
        })
        .commit()
        .await?;
    derive(&ctx, repo, base_cs).await?;

    // Commit A (child of base): add 1 restriction root — will be derived FROM SCRATCH.
    let commit_a = CreateCommitContext::new(&ctx, repo, vec![base_cs])
        .add_file("new_a/.slacl", unique_slacl(100))
        .add_file("new_a/file.txt", "content")
        .commit()
        .await?;

    // Commit B (also child of base): add 1 restriction root — will be derived INCREMENTALLY.
    let commit_b = CreateCommitContext::new(&ctx, repo, vec![base_cs])
        .add_file("new_b/.slacl", unique_slacl(101))
        .add_file("new_b/file.txt", "content")
        .commit()
        .await?;

    // Derive A from scratch (untopologically — ignores parent manifest).
    derive_deps(&ctx, repo, commit_a).await?;
    let before_a = counted.counters.snapshot();
    repo.repo_derived_data()
        .manager()
        .unsafe_derive_untopologically::<RootAclManifestId>(&ctx, commit_a, None)
        .await?;
    let cost_a = counted.counters.snapshot() - before_a;

    // Derive B incrementally (normal derivation — uses parent manifest).
    derive_deps(&ctx, repo, commit_b).await?;
    let before_b = counted.counters.snapshot();
    derive(&ctx, repo, commit_b).await?;
    let cost_b = counted.counters.snapshot() - before_b;

    assert!(
        cost_b.puts < cost_a.puts,
        "incremental ({} puts) should be cheaper than from-scratch ({} puts)",
        cost_b.puts,
        cost_a.puts,
    );

    // Blob operations should be deterministic — assert exact values to detect
    // regressions in derivation logic.
    assert_eq!(cost_b.puts, 4, "cost_b.puts doesn't match expectation");
    assert_eq!(cost_a.puts, 14, "cost_a.puts doesn't match expectation");

    assert_eq!(cost_b.gets, 13, "cost_b.gets doesn't match expectation");
    assert_eq!(cost_a.gets, 25, "cost_a.gets doesn't match expectation");

    Ok(())
}

/// Test that derivation cost scales with the number of ACL changes.
/// Derives from-scratch manifests with N, 2N, 10N, and 100N restriction roots
/// in separate repos, then asserts that cost increases monotonically and
/// matches deterministic expected values.
#[mononoke::fbinit_test]
async fn test_derive_cost_scales_with_changes(fb: FacebookInit) -> Result<()> {
    let n = 5_usize;
    let multipliers: Vec<usize> = vec![1, 2, 10, 100];

    // For each multiplier, create a fresh repo with (multiplier * N) restriction
    // roots, derive from scratch, and record blob operation counts.
    // Each .slacl file has unique content (via unique_slacl) to avoid
    // content-address deduplication in the blobstore.
    let costs: Vec<_> = futures::future::try_join_all(multipliers.iter().map(|&mult| {
        let count = mult * n;
        async move {
            let ctx = CoreContext::test_mock(fb);
            let counted = build_counted_test_repo(fb).await?;

            let changes: Vec<_> = (0..count)
                .flat_map(|i| {
                    let dir = format!("dir_{i}");
                    let slacl = format!("{dir}/.slacl");
                    let file = format!("{dir}/file.txt");
                    vec![(slacl, unique_slacl(i)), (file, b"content" as &[u8])]
                })
                .collect();

            let cs = changes
                .iter()
                .fold(
                    CreateCommitContext::new_root(&ctx, &counted.repo),
                    |b, (p, c)| b.add_file(p.as_str(), *c),
                )
                .commit()
                .await?;

            let before = counted.counters.snapshot();
            derive(&ctx, &counted.repo, cs).await?;
            let cost = counted.counters.snapshot() - before;

            Ok::<_, anyhow::Error>((mult, cost))
        }
    }))
    .await?;

    // Print costs for visibility (use `--print-passing-details` to see).
    println!("=== Derivation cost scaling with number of restrictions (N={n}) ===");
    costs.iter().for_each(|(mult, cost)| {
        println!(
            "  {mult:>3}N ({:>3} roots): puts={}, gets={}",
            mult * n,
            cost.puts,
            cost.gets,
        );
    });

    // Assert monotonically increasing: each scenario should cost more than the previous.
    costs.windows(2).try_for_each(|pair| {
        let (mult_a, cost_a) = &pair[0];
        let (mult_b, cost_b) = &pair[1];
        anyhow::ensure!(
            cost_b.puts > cost_a.puts,
            "{mult_b}N puts ({}) should be greater than {mult_a}N puts ({})",
            cost_b.puts,
            cost_a.puts,
        );
        Ok(())
    })?;

    // Blob operations should be deterministic — assert exact values to detect
    // regressions in derivation logic.
    let actual_puts: Vec<u64> = costs.iter().map(|(_, c)| c.puts).collect();
    let actual_gets: Vec<u64> = costs.iter().map(|(_, c)| c.gets).collect();
    assert_eq!(
        actual_puts,
        vec![12, 22, 102, 1002],
        "blob puts by multiplier"
    );
    assert_eq!(actual_gets, vec![9, 14, 54, 504], "blob gets by multiplier");

    Ok(())
}

/// Test that derivation cost scales with depth.
/// N restriction roots at increasing depths should cost more as depth increases,
/// because more waypoint ancestor nodes must be created.
#[mononoke::fbinit_test]
async fn test_derive_cost_scales_with_depth(fb: FacebookInit) -> Result<()> {
    let n = 5_usize;
    // Depth prefixes: each adds more ancestor directories.
    // depth_mult=1: "a/dir_N", depth_mult=2: "a/b/c/d/dir_N", etc.
    let depth_configs: Vec<(usize, &str)> = vec![
        (1, "a"),
        (2, "a/b/c/d"),
        (10, "a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r/s/t"),
    ];

    // For each depth, create a fresh repo with N restriction roots at that
    // depth, derive from scratch, and record blob operation counts.
    let costs: Vec<_> = futures::future::try_join_all(depth_configs.iter().map(
        |&(depth_mult, prefix)| async move {
            let ctx = CoreContext::test_mock(fb);
            let counted = build_counted_test_repo(fb).await?;

            let changes: Vec<_> = (0..n)
                .flat_map(|i| {
                    let dir = format!("{prefix}/dir_{i}");
                    let slacl = format!("{dir}/.slacl");
                    let file = format!("{dir}/file.txt");
                    vec![(slacl, unique_slacl(i)), (file, b"content" as &[u8])]
                })
                .collect();

            let cs = changes
                .iter()
                .fold(
                    CreateCommitContext::new_root(&ctx, &counted.repo),
                    |b, (p, c)| b.add_file(p.as_str(), *c),
                )
                .commit()
                .await?;

            let before = counted.counters.snapshot();
            derive(&ctx, &counted.repo, cs).await?;
            let cost = counted.counters.snapshot() - before;

            Ok::<_, anyhow::Error>((depth_mult, prefix, cost))
        },
    ))
    .await?;

    // Print costs for visibility (use `--print-passing-details` to see).
    println!("=== Derivation cost scaling with depth (N={n}) ===");
    costs.iter().for_each(|(depth_mult, prefix, cost)| {
        println!(
            "  {depth_mult:>2}D ({prefix}/dir_N): puts={}, gets={}",
            cost.puts, cost.gets,
        );
    });

    // Assert monotonically increasing puts: deeper paths cost more.
    costs.windows(2).try_for_each(|pair| {
        let (mult_a, _, cost_a) = &pair[0];
        let (mult_b, _, cost_b) = &pair[1];
        anyhow::ensure!(
            cost_b.puts > cost_a.puts,
            "depth {mult_b}D puts ({}) should be greater than depth {mult_a}D puts ({})",
            cost_b.puts,
            cost_a.puts,
        );
        Ok(())
    })?;

    // Blob operations should be deterministic — assert exact values.
    let actual_puts: Vec<u64> = costs.iter().map(|(_, _, c)| c.puts).collect();
    let actual_gets: Vec<u64> = costs.iter().map(|(_, _, c)| c.gets).collect();
    assert_eq!(actual_puts, vec![13, 16, 32], "blob puts by depth");
    assert_eq!(actual_gets, vec![9, 9, 9], "blob gets by depth");

    Ok(())
}

/// Test that derivation cost is independent of non-ACL file changes.
/// A commit with 3 ACL changes should cost the same whether it also
/// touches 0 or 100 non-ACL files.
#[mononoke::fbinit_test]
async fn test_derive_cost_independent_of_non_acl_changes(fb: FacebookInit) -> Result<()> {
    // Setup: 3 restriction roots (A, B, C) as the base.
    let ctx = CoreContext::test_mock(fb);
    let counted = build_counted_test_repo(fb).await?;
    let repo = &counted.repo;

    let base_cs = CreateCommitContext::new_root(&ctx, repo)
        .add_file("a/.slacl", SLACL_PROJECT1)
        .add_file("a/file.txt", "content")
        .add_file("b/.slacl", SLACL_PROJECT1)
        .add_file("b/file.txt", "content")
        .add_file("c/.slacl", SLACL_PROJECT1)
        .add_file("c/file.txt", "content")
        .commit()
        .await?;
    derive(&ctx, repo, base_cs).await?;

    // Branch A: 3 ACL changes only (add new, modify a, delete b).
    let branch_a = CreateCommitContext::new(&ctx, repo, vec![base_cs])
        .add_file("new/.slacl", SLACL_PROJECT2)
        .add_file("new/file.txt", "content")
        .add_file("a/.slacl", SLACL_PROJECT2)
        .delete_file("b/.slacl")
        .commit()
        .await?;

    let before_a = counted.counters.snapshot();
    derive(&ctx, repo, branch_a).await?;
    let cost_a = counted.counters.snapshot() - before_a;

    // Branch B: same 3 ACL changes PLUS 100 non-ACL file changes.
    let branch_b_builder = (0..100).fold(
        CreateCommitContext::new(&ctx, repo, vec![base_cs])
            .add_file("new/.slacl", SLACL_PROJECT2)
            .add_file("new/file.txt", "content")
            .add_file("a/.slacl", SLACL_PROJECT2)
            .delete_file("b/.slacl"),
        |builder, i| {
            let path = format!("unrelated/file_{i}.txt");
            builder.add_file(path.as_str(), "content")
        },
    );
    let branch_b = branch_b_builder.commit().await?;

    let before_b = counted.counters.snapshot();
    derive(&ctx, repo, branch_b).await?;
    let cost_b = counted.counters.snapshot() - before_b;

    assert_eq!(
        cost_a.puts, cost_b.puts,
        "blob puts should be identical regardless of non-ACL changes: {} vs {}",
        cost_a.puts, cost_b.puts,
    );
    assert_eq!(
        cost_a.gets, cost_b.gets,
        "blob gets should be identical regardless of non-ACL changes: {} vs {}",
        cost_a.gets, cost_b.gets,
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Additional test cases
// ---------------------------------------------------------------------------

/// Test root-level .slacl: a restriction file at the repo root itself.
#[mononoke::fbinit_test]
async fn test_derive_root_level_slacl(fb: FacebookInit) -> Result<()> {
    let result = setup_and_derive(
        fb,
        vec![vec![
            Change::Add("file.txt", b"content"),
            Change::Add(".slacl", SLACL_PROJECT1),
        ]],
    )
    .await?;

    // The root manifest itself should be restricted (no waypoint entries,
    // since the root IS the restriction root).
    let root_id = result.last_id().inner_id();
    let blob_id = expect_restricted(&result.ctx, &result.repo, root_id).await?;

    let blob = blob_id
        .load(&result.ctx, result.repo.repo_blobstore())
        .await?;
    assert_eq!(
        blob.repo_region_acl,
        "REPO_REGION:repos/hg/fbsource/=project1"
    );

    Ok(())
}

/// Test permission_request_group: an ACL file with explicit group.
#[mononoke::fbinit_test]
async fn test_derive_permission_request_group(fb: FacebookInit) -> Result<()> {
    let result = setup_and_derive(
        fb,
        vec![vec![
            Change::Add("dir/file.txt", b"content"),
            Change::Add("dir/.slacl", SLACL_PROJECT1_WITH_GROUP),
        ]],
    )
    .await?;

    let tree = result.last_tree().await?;
    assert_eq!(tree.len(), 1);
    assert_eq!(
        tree[0].permission_request_group.as_deref(),
        Some("GROUP:my_amp_group"),
    );

    Ok(())
}

/// Test large fan-out: many restriction roots at the same directory level.
#[mononoke::fbinit_test]
async fn test_derive_large_fan_out(fb: FacebookInit) -> Result<()> {
    let changes: Vec<Change<'_>> = (0..20)
        .flat_map(|i| {
            let dir = format!("dir_{i:03}");
            // Box::leak needed: Change::Add borrows &str but format! is temporary
            let slacl_path: &str = Box::leak(format!("{dir}/.slacl").into_boxed_str());
            let file_path: &str = Box::leak(format!("{dir}/file.txt").into_boxed_str());
            vec![
                Change::Add(slacl_path, SLACL_PROJECT1),
                Change::Add(file_path, b"content"),
            ]
        })
        .collect();

    let result = setup_and_derive(fb, vec![changes]).await?;
    let tree = result.last_tree().await?;

    assert_eq!(tree.len(), 20, "should have 20 restriction roots");
    assert!(
        tree.iter().all(|n| n.is_restricted),
        "all roots should be restricted"
    );

    Ok(())
}

/// Test derivation idempotency: deriving the same changeset twice produces
/// identical manifest IDs.
#[mononoke::fbinit_test]
async fn test_derive_idempotency(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;

    let cs_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/.slacl", SLACL_PROJECT1)
        .add_file("dir/file.txt", "content")
        .add_file("other/nested/.slacl", SLACL_PROJECT2)
        .add_file("other/nested/file.txt", "content")
        .commit()
        .await?;

    let first = derive(&ctx, &repo, cs_id).await?;
    let second = derive(&ctx, &repo, cs_id).await?;

    assert_eq!(
        first.inner_id(),
        second.inner_id(),
        "deriving the same changeset twice should produce identical manifest IDs"
    );

    Ok(())
}
