/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

#[mononoke::fbinit_test]
async fn test_permission_changes_omit_added_deleted_and_modified_in_unchanged_tent(
    fb: FacebookInit,
) -> Result<()> {
    let (repo_ctx, base) = create_both_mode_test_changeset(
        fb,
        vec![],
        vec![("secret", "REPO_REGION:acl")],
        vec!["secret/deleted", "secret/modified"],
    )
    .await?;
    let tip_id = CreateCommitContext::new(base.ctx(), repo_ctx.repo(), vec![base.id()])
        .delete_file("secret/deleted")
        .add_file("secret/modified", "changed")
        .add_file("secret/added", "new")
        .commit()
        .await?;
    let tip = ChangesetContext::new(repo_ctx, tip_id);
    let details: BTreeMap<_, _> = tip
        .changed_paths_restriction_info(None, None, rich_restriction_options(false))
        .await?
        .try_collect()
        .await?;
    assert_eq!(details.len(), 3);
    for info in details.values() {
        assert!(info.permission_change.is_none());
        assert!(
            info.restriction_info()
                .any(|info| info.repo_region_acl() == "REPO_REGION:acl")
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_permission_changes_compare_missing_files_when_acl_created_or_deleted(
    fb: FacebookInit,
) -> Result<()> {
    let (repo_ctx, base) =
        create_both_mode_test_changeset(fb, vec![], vec![], vec!["secret/deleted"]).await?;
    let created_id = CreateCommitContext::new(base.ctx(), repo_ctx.repo(), vec![base.id()])
        .delete_file("secret/deleted")
        .add_file("secret/.slacl", "repo_region_acl = \"REPO_REGION:acl\"\n")
        .add_file("secret/added", "new")
        .commit()
        .await?;
    let created = ChangesetContext::new(repo_ctx.clone(), created_id);
    let details: BTreeMap<_, _> = created
        .changed_paths_restriction_info(None, None, rich_restriction_options(false))
        .await?
        .try_collect()
        .await?;
    for path in ["secret/deleted", "secret/added"] {
        let info = &details[&NonRootMPath::new(path)?];
        let change = info
            .permission_change
            .as_ref()
            .expect("empty-to-ACL policy change");
        assert!(change.base.restrictions.is_empty());
        assert_eq!(
            change.target.restrictions[0].repo_region_acl(),
            "REPO_REGION:acl"
        );
    }
    assert!(
        details[&NonRootMPath::new("secret/deleted")?]
            .restriction_info()
            .next()
            .is_none(),
        "absent target policy is not a content restriction"
    );
    let removed_id = CreateCommitContext::new(base.ctx(), repo_ctx.repo(), vec![created_id])
        .delete_file("secret/.slacl")
        .delete_file("secret/added")
        .commit()
        .await?;
    let removed = ChangesetContext::new(repo_ctx, removed_id);
    let details: BTreeMap<_, _> = removed
        .changed_paths_restriction_info(None, None, rich_restriction_options(false))
        .await?
        .try_collect()
        .await?;
    let info = &details[&NonRootMPath::new("secret/added")?];
    let change = info
        .permission_change
        .as_ref()
        .expect("ACL-to-empty policy change");
    assert_eq!(
        change.base.restrictions[0].repo_region_acl(),
        "REPO_REGION:acl"
    );
    assert!(change.target.restrictions.is_empty());
    assert_eq!(
        info.restriction_info()
            .next()
            .expect("deleted content remains restricted")
            .repo_region_acl(),
        "REPO_REGION:acl"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_permission_changes_filter_matching_copy_policies(fb: FacebookInit) -> Result<()> {
    let (repo_ctx, base) = create_both_mode_test_changeset(
        fb,
        vec![],
        vec![("secret", "REPO_REGION:acl")],
        vec!["secret/source", "public/source"],
    )
    .await?;
    let tip_id = CreateCommitContext::new(base.ctx(), repo_ctx.repo(), vec![base.id()])
        .add_file_with_copy_info("secret/equal", "content", (base.id(), "secret/source"))
        .add_file_with_copy_info("public/different", "content", (base.id(), "secret/source"))
        .add_file_with_copy_info(
            "secret/from_public",
            "content",
            (base.id(), "public/source"),
        )
        .commit()
        .await?;
    let tip = ChangesetContext::new(repo_ctx, tip_id);
    let details: BTreeMap<_, _> = tip
        .changed_paths_restriction_info(None, None, rich_restriction_options(false))
        .await?
        .try_collect()
        .await?;
    let equal = &details[&NonRootMPath::new("secret/equal")?];
    assert_eq!(
        equal.copy_sources.len(),
        1,
        "actual source remains available for aggregate checks"
    );
    assert!(
        equal.permission_change.is_none(),
        "provenance alone is not a permission change"
    );
    assert!(
        equal
            .restriction_info()
            .all(|info| info.repo_region_acl() == "REPO_REGION:acl")
    );
    let different = details[&NonRootMPath::new("public/different")?]
        .permission_change
        .as_ref()
        .expect("source policy differs");
    assert_eq!(different.base, different.target);
    assert_eq!(different.copy_sources.len(), 1);
    assert_eq!(different.copy_sources[0].commit_id, base.id());
    assert_eq!(
        different.copy_sources[0].info.restrictions[0].repo_region_acl(),
        "REPO_REGION:acl"
    );
    let from_public = details[&NonRootMPath::new("secret/from_public")?]
        .permission_change
        .as_ref()
        .expect("unrestricted source also differs from protected target");
    assert_eq!(from_public.base, from_public.target);
    assert!(from_public.copy_sources[0].info.restrictions.is_empty());
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_permission_changes_root_uses_unrestricted_virtual_base(
    fb: FacebookInit,
) -> Result<()> {
    let (_, root) = create_both_mode_test_changeset(
        fb,
        vec![],
        vec![("secret", "REPO_REGION:acl")],
        vec!["secret/file", "public/file"],
    )
    .await?;
    let details: BTreeMap<_, _> = root
        .changed_paths_restriction_info(None, None, rich_restriction_options(true))
        .await?
        .try_collect()
        .await?;
    assert!(
        details[&NonRootMPath::new("public/file")?]
            .permission_change
            .is_none()
    );
    let change = details[&NonRootMPath::new("secret/file")?]
        .permission_change
        .as_ref()
        .expect("root introduces a protected policy");
    assert!(change.base.restrictions.is_empty());
    assert_eq!(change.base.has_access, Some(true));
    assert_eq!(
        change.target.restrictions[0].repo_region_acl(),
        "REPO_REGION:acl"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_permission_changes_implicit_directory_replacement(fb: FacebookInit) -> Result<()> {
    let (repo_ctx, base) = create_both_mode_test_changeset(
        fb,
        vec![],
        vec![("secret", "REPO_REGION:acl")],
        vec!["secret/file"],
    )
    .await?;
    let tip_id = CreateCommitContext::new(base.ctx(), repo_ctx.repo(), vec![base.id()])
        .add_file("secret", "replacement")
        .commit()
        .await?;
    let tip = ChangesetContext::new(repo_ctx, tip_id);
    let details: BTreeMap<_, _> = tip
        .changed_paths_restriction_info(None, None, rich_restriction_options(false))
        .await?
        .try_collect()
        .await?;
    for info in details.values() {
        let change = info
            .permission_change
            .as_ref()
            .expect("directory tent policy is removed");
        assert_eq!(
            change.base.restrictions[0].repo_region_acl(),
            "REPO_REGION:acl"
        );
        assert!(change.target.restrictions.is_empty());
    }
    assert!(
        details[&NonRootMPath::new("secret")?]
            .restriction_info()
            .next()
            .is_none(),
        "directory policy is not a nonexistent base file restriction"
    );
    Ok(())
}
