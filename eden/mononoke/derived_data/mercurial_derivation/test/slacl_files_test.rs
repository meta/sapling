/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use acl_manifest::RootAclManifestId;
use anyhow::Context;
use anyhow::Result;
use blobstore::Loadable;
use context::CoreContext;
use fbinit::FacebookInit;
use mercurial_derivation::RootHgAugmentedManifestId;
use mercurial_types::HgAugmentedManifestEntry;
use mercurial_types::HgAugmentedManifestEnvelope;
use mercurial_types::HgAugmentedManifestId;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::MPath;
use mononoke_types::acl_manifest::AclManifest;
use mononoke_types::blob::BlobstoreValue;
use mononoke_types::typed_hash::AclManifestId;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataRef;
use tests_utils::CreateCommitContext;

use crate::Repo;

/// Test that `acl_manifest_directory_id` is `None` for all directories when
/// the repo has no `.slacl` files (i.e., the derived AclManifest is empty).
#[mononoke::fbinit_test]
async fn test_root_acl_manifest_pointer_is_none_when_acl_manifest_empty(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir_a/file", "content_a")
        .add_file("dir_b/nested/file", "content_b")
        .commit()
        .await?;

    let envelope = derive_and_load_augmented_manifest(&ctx, &repo, vec![root], root).await?;

    assert_eq!(
        envelope.augmented_manifest.acl_manifest_directory_id, None,
        "Root acl_manifest_directory_id should be None when AclManifest is empty"
    );

    Ok(())
}

/// Test that `acl_manifest_directory_id` is correctly populated for
/// waypoint and restriction-root directories, and `None` for unrelated ones.
#[mononoke::fbinit_test]
async fn test_root_and_waypoint_acl_manifest_pointers(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    // ACL tree: root (waypoint) -> foo (waypoint) -> bar (restriction root)
    // foo/other/ and unrelated/ are NOT in the ACL tree.
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "foo/bar/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project1\"\n",
        )
        .add_file("foo/bar/file", "restricted content")
        .add_file("foo/other/file", "unrestricted")
        .add_file("unrelated/file", "unrelated")
        .commit()
        .await?;

    // Verify AclManifest is NOT the canonical empty manifest (pre-derive batch dependencies first)
    let manager = repo.repo_derived_data().manager();
    // Pre-derive batch dependencies
    fixtures::store_hg_changesets_as_uploaded(&ctx, &repo, vec![root]).await?;
    manager
        .derive_exactly_batch::<RootAclManifestId>(&ctx, vec![root], None)
        .await?;
    let acl_root = manager
        .fetch_derived::<RootAclManifestId>(&ctx, root, None)
        .await?
        .context("Missing RootAclManifestId")?;
    let canonical_empty_id = *AclManifest::empty().into_blob().id();
    assert_ne!(
        acl_root.into_inner_id(),
        canonical_empty_id,
        "AclManifest should not be empty when repo has .slacl files"
    );

    // Derive augmented manifest (dependencies already derived above)
    manager
        .derive_exactly_batch::<RootHgAugmentedManifestId>(&ctx, vec![root], None)
        .await?;
    let aug = manager
        .fetch_derived::<RootHgAugmentedManifestId>(&ctx, root, None)
        .await?
        .context("Missing RootHgAugmentedManifestId")?;
    let root_envelope = aug
        .hg_augmented_manifest_id()
        .load(&ctx, repo.repo_blobstore())
        .await?;

    // Root: Some (waypoint), and not the canonical empty ID
    assert!(
        root_envelope
            .augmented_manifest
            .acl_manifest_directory_id
            .is_some(),
        "Root acl_manifest_directory_id should be Some when repo has .slacl files"
    );
    assert_ne!(
        root_envelope.augmented_manifest.acl_manifest_directory_id,
        Some(canonical_empty_id),
        "Root pointer should never be Some(canonical_empty_acl_manifest_id)"
    );

    // foo/: Some (waypoint)
    assert!(
        get_dir_acl_pointer(&ctx, &repo, &root_envelope, b"foo")
            .await?
            .is_some(),
        "foo/ should have an ACL pointer (waypoint for foo/bar/.slacl)"
    );
    // foo/bar/: Some (restriction root)
    assert!(
        get_nested_dir_acl_pointer(&ctx, &repo, &root_envelope, &MPath::new("foo/bar")?)
            .await?
            .is_some(),
        "foo/bar/ should have an ACL pointer (restriction root)"
    );
    // foo/other/: None (not in ACL tree)
    assert_eq!(
        get_nested_dir_acl_pointer(&ctx, &repo, &root_envelope, &MPath::new("foo/other")?).await?,
        None,
        "foo/other/ should have no ACL pointer (not in sparse ACL tree)"
    );
    // unrelated/: None (not in ACL tree)
    assert_eq!(
        get_dir_acl_pointer(&ctx, &repo, &root_envelope, b"unrelated").await?,
        None,
        "unrelated/ should have no ACL pointer (not in sparse ACL tree)"
    );

    Ok(())
}

/// Test that ACL pointers appear correctly when `.slacl` is added in a
/// child commit that did not exist in the parent.
#[mononoke::fbinit_test]
async fn test_acl_pointers_when_slacl_added_in_child(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("foo/bar/file", "content")
        .add_file("other/file", "other")
        .commit()
        .await?;

    let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .add_file(
            "foo/bar/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project1\"\n",
        )
        .commit()
        .await?;

    let batch = vec![parent, child];
    let parent_env = derive_and_load_augmented_manifest(&ctx, &repo, batch.clone(), parent).await?;
    let child_env = derive_and_load_augmented_manifest(&ctx, &repo, batch, child).await?;

    // Parent: all pointers None (no .slacl)
    assert_eq!(
        parent_env.augmented_manifest.acl_manifest_directory_id, None,
        "Parent root should have no ACL pointer"
    );

    // Child: root and foo should be waypoints; other should be None
    assert!(
        child_env
            .augmented_manifest
            .acl_manifest_directory_id
            .is_some(),
        "Child root should be a waypoint"
    );
    assert!(
        get_dir_acl_pointer(&ctx, &repo, &child_env, b"foo")
            .await?
            .is_some(),
        "foo/ should be a waypoint"
    );
    assert_eq!(
        get_dir_acl_pointer(&ctx, &repo, &child_env, b"other").await?,
        None,
        "other/ should have no pointer"
    );

    Ok(())
}

/// Test that ACL pointers revert to `None` when the `.slacl` file is
/// removed in a child commit.
#[mononoke::fbinit_test]
async fn test_acl_pointers_when_slacl_removed(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "foo/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project1\"\n",
        )
        .add_file("foo/file", "content")
        .commit()
        .await?;

    let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .delete_file("foo/.slacl")
        .commit()
        .await?;

    let batch = vec![parent, child];
    let parent_env = derive_and_load_augmented_manifest(&ctx, &repo, batch.clone(), parent).await?;
    let child_env = derive_and_load_augmented_manifest(&ctx, &repo, batch, child).await?;

    // Parent: root should have pointer
    assert!(
        parent_env
            .augmented_manifest
            .acl_manifest_directory_id
            .is_some(),
        "Parent root should have pointer"
    );

    // Child: all pointers None (no more .slacl)
    assert_eq!(
        child_env.augmented_manifest.acl_manifest_directory_id, None,
        "Child root should have no pointer after .slacl removed"
    );

    Ok(())
}

/// Test that multiple `.slacl` files at sibling paths produce independent
/// ACL pointers. Each sibling restriction root should get its own
/// `AclManifestId`, and directories without `.slacl` should have `None`.
#[mononoke::fbinit_test]
async fn test_acl_pointers_multiple_sibling_slacl(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    // ACL tree: root (waypoint) -> foo (restriction root)
    //                            -> bar (restriction root)
    // other/ is NOT in the ACL tree.
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "foo/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project_foo\"\n",
        )
        .add_file("foo/file", "foo content")
        .add_file(
            "bar/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project_bar\"\n",
        )
        .add_file("bar/file", "bar content")
        .add_file("other/file", "other content")
        .commit()
        .await?;

    let envelope = derive_and_load_augmented_manifest(&ctx, &repo, vec![root], root).await?;

    // Root: Some (waypoint)
    assert!(
        envelope
            .augmented_manifest
            .acl_manifest_directory_id
            .is_some(),
        "Root acl_manifest_directory_id should be Some (waypoint for foo/ and bar/)"
    );

    let foo_ptr = get_dir_acl_pointer(&ctx, &repo, &envelope, b"foo").await?;
    let bar_ptr = get_dir_acl_pointer(&ctx, &repo, &envelope, b"bar").await?;

    assert!(
        foo_ptr.is_some(),
        "foo/ should have an ACL pointer (restriction root)"
    );
    assert!(
        bar_ptr.is_some(),
        "bar/ should have an ACL pointer (restriction root)"
    );
    assert_ne!(
        foo_ptr, bar_ptr,
        "foo/ and bar/ should have different AclManifestIds (independent restriction roots)"
    );
    assert_eq!(
        get_dir_acl_pointer(&ctx, &repo, &envelope, b"other").await?,
        None,
        "other/ should have no ACL pointer (not in sparse ACL tree)"
    );

    Ok(())
}

/// Test that nested `.slacl` files work correctly -- a directory can be both
/// a restriction root (has its own `.slacl`) AND a waypoint (ancestor of
/// a deeper `.slacl`). Both should get `Some` pointers with different IDs.
#[mononoke::fbinit_test]
async fn test_acl_pointers_nested_slacl(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    // ACL tree: root (waypoint) -> a (restriction root AND waypoint) -> b (restriction root)
    // a/c/ is NOT in the ACL tree (sibling of b, no .slacl).
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "a/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project_a\"\n",
        )
        .add_file("a/file", "a content")
        .add_file(
            "a/b/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project_ab\"\n",
        )
        .add_file("a/b/file", "ab content")
        .add_file("a/c/file", "ac content")
        .commit()
        .await?;

    let envelope = derive_and_load_augmented_manifest(&ctx, &repo, vec![root], root).await?;

    // Root: Some (waypoint)
    assert!(
        envelope
            .augmented_manifest
            .acl_manifest_directory_id
            .is_some(),
        "Root acl_manifest_directory_id should be Some (waypoint for a/)"
    );

    let a_ptr = get_dir_acl_pointer(&ctx, &repo, &envelope, b"a").await?;
    let ab_ptr = get_nested_dir_acl_pointer(&ctx, &repo, &envelope, &MPath::new("a/b")?).await?;

    assert!(
        a_ptr.is_some(),
        "a/ should have an ACL pointer (restriction root and waypoint)"
    );
    assert!(
        ab_ptr.is_some(),
        "a/b/ should have an ACL pointer (restriction root)"
    );
    assert_ne!(
        a_ptr, ab_ptr,
        "a/ and a/b/ should have different AclManifestIds"
    );
    assert_eq!(
        get_nested_dir_acl_pointer(&ctx, &repo, &envelope, &MPath::new("a/c")?).await?,
        None,
        "a/c/ should have no ACL pointer (not in sparse ACL tree)"
    );

    Ok(())
}

/// Test that removing an inner nested `.slacl` correctly clears the ACL
/// pointer on that directory while the outer `.slacl` directory keeps its
/// pointer. The parent commit has both `a/.slacl` and `a/b/.slacl`; the
/// child commit deletes `a/b/.slacl`.
#[mononoke::fbinit_test]
async fn test_acl_pointers_remove_inner_nested_slacl(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "a/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project_a\"\n",
        )
        .add_file("a/file", "a content")
        .add_file(
            "a/b/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project_ab\"\n",
        )
        .add_file("a/b/file", "ab content")
        .commit()
        .await?;

    // Child: remove a/b/.slacl only (a/.slacl remains)
    let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .delete_file("a/b/.slacl")
        .commit()
        .await?;

    let batch = vec![parent, child];
    let parent_env = derive_and_load_augmented_manifest(&ctx, &repo, batch.clone(), parent).await?;
    let child_env = derive_and_load_augmented_manifest(&ctx, &repo, batch, child).await?;

    // Parent: a/ and a/b/ should both have pointers
    assert!(
        get_dir_acl_pointer(&ctx, &repo, &parent_env, b"a")
            .await?
            .is_some(),
        "Parent: a/ should have an ACL pointer"
    );
    assert!(
        get_nested_dir_acl_pointer(&ctx, &repo, &parent_env, &MPath::new("a/b")?)
            .await?
            .is_some(),
        "Parent: a/b/ should have an ACL pointer"
    );

    // Child: a/ still has pointer, a/b/ lost its pointer
    assert!(
        get_dir_acl_pointer(&ctx, &repo, &child_env, b"a")
            .await?
            .is_some(),
        "Child: a/ should still have an ACL pointer (a/.slacl remains)"
    );
    assert_eq!(
        get_nested_dir_acl_pointer(&ctx, &repo, &child_env, &MPath::new("a/b")?).await?,
        None,
        "Child: a/b/ should have no ACL pointer (a/b/.slacl was deleted)"
    );

    Ok(())
}

/// Test that deleting a directory containing `.slacl` (by deleting all files
/// in it) removes the ACL pointers from the augmented manifest tree.
///
/// Parent has `restricted/code/.slacl`, `restricted/code/file.rs`, and
/// `public/readme.md`. Child deletes both files under `restricted/code/`,
/// implicitly removing the directory (and `restricted/` if it has no other
/// children). All ACL pointers should become `None`.
#[mononoke::fbinit_test]
async fn test_acl_pointers_implicit_delete_directory_with_slacl(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "restricted/code/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project1\"\n",
        )
        .add_file("restricted/code/file.rs", "fn secret() {}")
        .add_file("public/readme.md", "hello")
        .commit()
        .await?;

    let parent_env = derive_and_load_augmented_manifest(&ctx, &repo, vec![parent], parent).await?;

    // Parent: root, restricted/, restricted/code/ all have pointers
    assert!(
        parent_env
            .augmented_manifest
            .acl_manifest_directory_id
            .is_some(),
        "Parent root should have ACL pointer"
    );
    assert!(
        get_dir_acl_pointer(&ctx, &repo, &parent_env, b"restricted")
            .await?
            .is_some(),
        "Parent restricted/ should have ACL pointer (waypoint)"
    );
    assert!(
        get_nested_dir_acl_pointer(&ctx, &repo, &parent_env, &MPath::new("restricted/code")?)
            .await?
            .is_some(),
        "Parent restricted/code/ should have ACL pointer (restriction root)"
    );

    // Child: implicitly delete restricted/code/ directory by creating a file
    // at the same path
    let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .add_file(
            "restricted/code",
            "file to implicitly delete restricted/code/",
        )
        .commit()
        .await?;

    let child_env = derive_and_load_augmented_manifest(&ctx, &repo, vec![child], child).await?;

    // Root: None (no .slacl in repo)
    assert_eq!(
        child_env.augmented_manifest.acl_manifest_directory_id, None,
        "Child root should have no ACL pointer after deleting all .slacl files"
    );

    // restricted/ still exists (contains the file "code") but is no longer
    // a waypoint since there are no .slacl files left.
    assert!(
        dir_exists(&ctx, &repo, &child_env, b"restricted").await?,
        "restricted/ should still exist (it now contains the file 'code')"
    );
    assert_eq!(
        get_dir_acl_pointer(&ctx, &repo, &child_env, b"restricted").await?,
        None,
        "restricted/ should have no ACL pointer (no longer a waypoint)"
    );
    // public/ should still exist with None pointer
    assert_eq!(
        get_dir_acl_pointer(&ctx, &repo, &child_env, b"public").await?,
        None,
        "public/ should have no ACL pointer"
    );

    Ok(())
}

/// Test that deleting the middle of a nested ACL chain removes only the
/// deleted portion while the surviving `.slacl` keeps its pointers.
///
/// Parent has three `.slacl` levels: `a/.slacl`, `a/b/.slacl`, `a/b/c/.slacl`
/// plus regular files at each level. Child deletes `a/b/.slacl`,
/// `a/b/c/.slacl`, and ALL files under `a/b/`, removing `a/b/` entirely.
/// The `a/.slacl` still exists, so root and `a/` keep their pointers.
#[mononoke::fbinit_test]
async fn test_acl_pointers_implicit_delete_nested_acl_middle(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "a/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=top\"\n",
        )
        .add_file("a/file_a.rs", "fn a() {}")
        .add_file(
            "a/b/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=mid\"\n",
        )
        .add_file("a/b/file_b.rs", "fn b() {}")
        .add_file(
            "a/b/c/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=deep\"\n",
        )
        .add_file("a/b/c/file_c.rs", "fn c() {}")
        .commit()
        .await?;

    let parent_env = derive_and_load_augmented_manifest(&ctx, &repo, vec![parent], parent).await?;

    // Parent: all four levels have pointers
    assert!(
        parent_env
            .augmented_manifest
            .acl_manifest_directory_id
            .is_some(),
        "Parent root should have ACL pointer"
    );
    assert!(
        get_dir_acl_pointer(&ctx, &repo, &parent_env, b"a")
            .await?
            .is_some(),
        "Parent a/ should have ACL pointer"
    );
    assert!(
        get_nested_dir_acl_pointer(&ctx, &repo, &parent_env, &MPath::new("a/b")?)
            .await?
            .is_some(),
        "Parent a/b/ should have ACL pointer"
    );
    assert!(
        get_nested_dir_acl_pointer(&ctx, &repo, &parent_env, &MPath::new("a/b/c")?)
            .await?
            .is_some(),
        "Parent a/b/c/ should have ACL pointer"
    );

    // Child: implicitly delete a/b/ directory by creating a file at the same path
    let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .add_file("a/b", "file to implicitly delete a/b")
        .commit()
        .await?;

    let child_env = derive_and_load_augmented_manifest(&ctx, &repo, vec![child], child).await?;

    // Child: root and a/ keep pointers (a/.slacl survives)
    assert!(
        child_env
            .augmented_manifest
            .acl_manifest_directory_id
            .is_some(),
        "Child root should still have ACL pointer (a/.slacl survives)"
    );
    assert!(
        get_dir_acl_pointer(&ctx, &repo, &child_env, b"a")
            .await?
            .is_some(),
        "Child a/ should still have ACL pointer (a/.slacl survives)"
    );
    // a/b/ directory was implicitly deleted — "b" is now a file inside a/,
    // not a directory.
    let a_child_envelope = {
        let a_entry = child_env
            .augmented_manifest
            .subentries
            .lookup(&ctx, repo.repo_blobstore(), b"a")
            .await?
            .context("a should still exist in child root")?;
        match a_entry {
            HgAugmentedManifestEntry::DirectoryNode(dir) => {
                HgAugmentedManifestId::new(dir.treenode)
                    .load(&ctx, repo.repo_blobstore())
                    .await?
            }
            _ => anyhow::bail!("a should be a directory node"),
        }
    };
    let b_entry = a_child_envelope
        .augmented_manifest
        .subentries
        .lookup(&ctx, repo.repo_blobstore(), b"b")
        .await?
        .context("b should exist as a file in a/")?;
    assert!(
        matches!(b_entry, HgAugmentedManifestEntry::FileNode(_)),
        "a/b should be a file (not a directory) after implicit delete"
    );

    Ok(())
}

/// Test that modifying `.slacl` content changes the `acl_manifest_directory_id`
/// pointer value. Because ACL manifest IDs are content-addressed, different
/// `.slacl` content produces a different blob and therefore a different tree ID.
#[mononoke::fbinit_test]
async fn test_acl_pointer_changes_when_slacl_content_changes(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "foo/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project_alpha\"\n",
        )
        .add_file("foo/file.rs", "fn alpha() {}")
        .commit()
        .await?;

    // Child: modify foo/.slacl to have a different ACL rule
    let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .add_file(
            "foo/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project_beta\"\n",
        )
        .commit()
        .await?;

    let batch = vec![parent, child];
    let parent_env = derive_and_load_augmented_manifest(&ctx, &repo, batch.clone(), parent).await?;
    let child_env = derive_and_load_augmented_manifest(&ctx, &repo, batch, child).await?;

    // Both roots should have Some pointers
    assert!(
        parent_env
            .augmented_manifest
            .acl_manifest_directory_id
            .is_some(),
        "Parent root should have an ACL pointer"
    );
    assert!(
        child_env
            .augmented_manifest
            .acl_manifest_directory_id
            .is_some(),
        "Child root should have an ACL pointer"
    );

    // foo/ ACL pointer must DIFFER (content-addressed)
    let parent_foo_ptr = get_dir_acl_pointer(&ctx, &repo, &parent_env, b"foo").await?;
    let child_foo_ptr = get_dir_acl_pointer(&ctx, &repo, &child_env, b"foo").await?;
    assert!(
        parent_foo_ptr.is_some(),
        "Parent foo/ should have an ACL pointer"
    );
    assert!(
        child_foo_ptr.is_some(),
        "Child foo/ should have an ACL pointer"
    );
    assert_ne!(
        parent_foo_ptr, child_foo_ptr,
        "foo/ ACL pointer should change when .slacl content changes (content-addressed)"
    );

    Ok(())
}

/// Test that adding a file to a restricted directory (without changing any
/// `.slacl` file) does NOT alter ACL manifest pointers. This validates that
/// Hg tree changes that don't affect the ACL tree preserve pointer stability.
#[mononoke::fbinit_test]
async fn test_acl_pointers_stable_when_file_added_no_slacl_change(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "foo/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project_stable\"\n",
        )
        .add_file("foo/existing.rs", "fn existing() {}")
        .add_file("other/file", "other content")
        .commit()
        .await?;

    // Child: add foo/new.rs -- no .slacl change
    let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .add_file("foo/new.rs", "fn new_func() {}")
        .add_file("foo/nested/new.rs", "fn nested_new_func() {}")
        .commit()
        .await?;

    let batch = vec![parent, child];
    let parent_env = derive_and_load_augmented_manifest(&ctx, &repo, batch.clone(), parent).await?;
    let child_env = derive_and_load_augmented_manifest(&ctx, &repo, batch, child).await?;

    // Root ACL pointer should be SAME (ACL tree didn't change)
    assert_eq!(
        parent_env.augmented_manifest.acl_manifest_directory_id,
        child_env.augmented_manifest.acl_manifest_directory_id,
        "Root ACL pointer should be stable when no .slacl files changed"
    );
    // foo/ ACL pointer should be SAME
    assert_eq!(
        get_dir_acl_pointer(&ctx, &repo, &parent_env, b"foo").await?,
        get_dir_acl_pointer(&ctx, &repo, &child_env, b"foo").await?,
        "foo/ ACL pointer should be stable when .slacl content unchanged"
    );
    // other/: None in both
    assert_eq!(
        get_dir_acl_pointer(&ctx, &repo, &parent_env, b"other").await?,
        None,
        "other/ should have no ACL pointer (not in ACL tree)"
    );
    assert_eq!(
        get_dir_acl_pointer(&ctx, &repo, &child_env, b"other").await?,
        None,
        "other/ should have no ACL pointer in child either"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// ACL pointer test helpers
// ---------------------------------------------------------------------------

/// Derive both RootAclManifestId and RootHgAugmentedManifestId for a batch of
/// changesets, then load and return the `HgAugmentedManifestEnvelope` for a
/// single changeset.
async fn derive_and_load_augmented_manifest(
    ctx: &CoreContext,
    repo: &Repo,
    batch: Vec<ChangesetId>,
    cs_id: ChangesetId,
) -> Result<HgAugmentedManifestEnvelope> {
    let manager = repo.repo_derived_data().manager();
    // Pre-derive batch dependencies of RootHgAugmentedManifestId
    fixtures::store_hg_changesets_as_uploaded(ctx, repo, batch.clone()).await?;
    manager
        .derive_exactly_batch::<RootAclManifestId>(ctx, batch.clone(), None)
        .await?;
    manager
        .derive_exactly_batch::<RootHgAugmentedManifestId>(ctx, batch, None)
        .await?;
    let aug = manager
        .fetch_derived::<RootHgAugmentedManifestId>(ctx, cs_id, None)
        .await?
        .context(format!("Missing RootHgAugmentedManifestId for {cs_id}"))?;
    let envelope = aug
        .hg_augmented_manifest_id()
        .load(ctx, repo.repo_blobstore())
        .await?;
    Ok(envelope)
}

/// Look up a single child directory in an augmented manifest envelope and
/// return its `acl_manifest_directory_id`. Returns an error if the entry is
/// missing or is a file (not a directory).
async fn get_dir_acl_pointer(
    ctx: &CoreContext,
    repo: &Repo,
    envelope: &HgAugmentedManifestEnvelope,
    name: &[u8],
) -> Result<Option<AclManifestId>> {
    let entry = envelope
        .augmented_manifest
        .subentries
        .lookup(ctx, repo.repo_blobstore(), name)
        .await?
        .context(format!(
            "{} should exist in subentries",
            String::from_utf8_lossy(name)
        ))?;
    match entry {
        HgAugmentedManifestEntry::DirectoryNode(dir) => Ok(dir.acl_manifest_directory_id),
        _ => anyhow::bail!(
            "{} should be a directory node",
            String::from_utf8_lossy(name)
        ),
    }
}

/// Walk a multi-segment path through the augmented manifest tree and return
/// the `acl_manifest_directory_id` of the final directory. Each intermediate
/// segment is loaded from the blobstore.
async fn get_nested_dir_acl_pointer(
    ctx: &CoreContext,
    repo: &Repo,
    envelope: &HgAugmentedManifestEnvelope,
    path: &MPath,
) -> Result<Option<AclManifestId>> {
    let elements: Vec<_> = path.into_iter().collect();
    anyhow::ensure!(!elements.is_empty(), "path must have at least one segment");

    let mut current_envelope: std::borrow::Cow<'_, HgAugmentedManifestEnvelope> =
        std::borrow::Cow::Borrowed(envelope);

    for (i, element) in elements.iter().enumerate() {
        let entry = current_envelope
            .augmented_manifest
            .subentries
            .lookup(ctx, repo.repo_blobstore(), element.as_ref())
            .await?
            .context(format!("{element} should exist at depth {i}"))?;
        match entry {
            HgAugmentedManifestEntry::DirectoryNode(dir) => {
                if i == elements.len() - 1 {
                    return Ok(dir.acl_manifest_directory_id);
                }
                // Load intermediate directory's manifest for next iteration
                let child = HgAugmentedManifestId::new(dir.treenode)
                    .load(ctx, repo.repo_blobstore())
                    .await?;
                current_envelope = std::borrow::Cow::Owned(child);
            }
            _ => anyhow::bail!("{element} should be a directory node at depth {i}"),
        }
    }
    unreachable!()
}

/// Check whether a directory entry exists in the augmented manifest's
/// subentries. Returns `true` if the entry is present, `false` if missing.
async fn dir_exists(
    ctx: &CoreContext,
    repo: &Repo,
    envelope: &HgAugmentedManifestEnvelope,
    name: &[u8],
) -> Result<bool> {
    let entry = envelope
        .augmented_manifest
        .subentries
        .lookup(ctx, repo.repo_blobstore(), name)
        .await?;
    Ok(entry.is_some())
}
