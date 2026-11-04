/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

#[mononoke::fbinit_test]
async fn test_restriction_comparison_checks_both_acl_versions(fb: FacebookInit) -> Result<()> {
    let (repo_ctx, parent) = create_both_mode_test_changeset(
        fb,
        vec![],
        vec![("secret", "REPO_REGION:old_acl")],
        vec!["secret/file"],
    )
    .await?;
    let tip_id = CreateCommitContext::new(parent.ctx(), repo_ctx.repo(), vec![parent.id()])
        .add_file(
            "secret/.slacl",
            "repo_region_acl = \"REPO_REGION:new_acl\"\n",
        )
        .add_file("secret/file", "changed")
        .commit()
        .await?;
    let tip = ChangesetContext::new(repo_ctx, tip_id);
    let details: BTreeMap<_, _> = tip
        .changed_paths_restriction_info(None, None, rich_restriction_options(true))
        .await?
        .try_collect()
        .await?;
    let file = &details[&NonRootMPath::new("secret/file")?];
    let base = file.sides.base().expect("modified file exists on base");
    let target = file.sides.target().expect("modified file exists on target");
    assert_eq!(
        base.restrictions[0].repo_region_acl(),
        "REPO_REGION:old_acl"
    );
    assert_eq!(
        target.restrictions[0].repo_region_acl(),
        "REPO_REGION:new_acl"
    );
    assert!(base.has_access.is_some());
    assert!(target.has_access.is_some());
    assert!(file.copy_sources.is_empty());
    let results = comparison_restrictions(&tip, None).await?;
    let mut acls: Vec<_> = results[&NonRootMPath::new("secret/file")?]
        .iter()
        .map(PathAccessInfo::repo_region_acl)
        .collect();
    acls.sort();
    assert_eq!(acls, vec!["REPO_REGION:new_acl", "REPO_REGION:old_acl"]);
    assert_eq!(results, comparison_restrictions(&tip, Some(&parent)).await?);
    let groups = tip.restricted_paths_changes(false).await?;
    let acls: Vec<_> = groups
        .restricted_changes
        .iter()
        .filter(|group| {
            group
                .changed_paths
                .contains(&NonRootMPath::new("secret/file").expect("valid path"))
        })
        .map(|group| group.restriction_info.repo_region_acl())
        .collect();
    assert_eq!(acls, vec!["REPO_REGION:new_acl", "REPO_REGION:old_acl"]);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_comparison_does_not_apply_rhs_acl_to_deleted_content(
    fb: FacebookInit,
) -> Result<()> {
    let (repo_ctx, parent) =
        create_both_mode_test_changeset(fb, vec![], vec![], vec!["secret/deleted"]).await?;
    let tip_id = CreateCommitContext::new(parent.ctx(), repo_ctx.repo(), vec![parent.id()])
        .delete_file("secret/deleted")
        .add_file(
            "secret/.slacl",
            "repo_region_acl = \"REPO_REGION:new_acl\"\n",
        )
        .commit()
        .await?;
    let tip = ChangesetContext::new(repo_ctx, tip_id);
    let results = comparison_restrictions(&tip, None).await?;
    assert!(results[&NonRootMPath::new("secret/deleted")?].is_empty());
    assert!(!results[&NonRootMPath::new("secret/.slacl")?].is_empty());
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_comparison_includes_implicit_deletions(fb: FacebookInit) -> Result<()> {
    let (repo_ctx, parent) = create_both_mode_test_changeset(
        fb,
        vec![],
        vec![("secret", "REPO_REGION:old_acl")],
        vec!["secret/file"],
    )
    .await?;
    let tip_id = CreateCommitContext::new(parent.ctx(), repo_ctx.repo(), vec![parent.id()])
        .add_file("secret", "directory replaced by file")
        .commit()
        .await?;
    let tip = ChangesetContext::new(repo_ctx, tip_id);
    let results = comparison_restrictions(&tip, None).await?;
    assert_eq!(
        results.keys().map(ToString::to_string).collect::<Vec<_>>(),
        vec!["secret", "secret/.slacl", "secret/file"],
    );
    assert!(results[&NonRootMPath::new("secret")?].is_empty());
    assert_eq!(
        results[&NonRootMPath::new("secret/file")?][0].repo_region_acl(),
        "REPO_REGION:old_acl",
    );
    assert_eq!(results, comparison_restrictions(&tip, Some(&parent)).await?);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_comparison_file_replaced_by_directory(fb: FacebookInit) -> Result<()> {
    let (repo_ctx, parent) =
        create_both_mode_test_changeset(fb, vec![], vec![], vec!["secret"]).await?;
    let tip_id = CreateCommitContext::new(parent.ctx(), repo_ctx.repo(), vec![parent.id()])
        .delete_file("secret")
        .add_file(
            "secret/.slacl",
            "repo_region_acl = \"REPO_REGION:new_acl\"\n",
        )
        .add_file("secret/file", "new")
        .commit()
        .await?;
    let tip = ChangesetContext::new(repo_ctx, tip_id);
    let results = comparison_restrictions(&tip, None).await?;
    assert_eq!(
        results.keys().map(ToString::to_string).collect::<Vec<_>>(),
        vec!["secret", "secret/.slacl", "secret/file"],
    );
    assert!(results[&NonRootMPath::new("secret")?].is_empty());
    assert_eq!(
        results[&NonRootMPath::new("secret/file")?][0].repo_region_acl(),
        "REPO_REGION:new_acl"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_comparison_range_is_a_net_difference(fb: FacebookInit) -> Result<()> {
    let (repo_ctx, base) = create_both_mode_test_changeset(
        fb,
        vec![],
        vec![("secret", "REPO_REGION:acl")],
        vec!["secret/deleted", "public/reverted"],
    )
    .await?;
    let middle_id = CreateCommitContext::new(base.ctx(), repo_ctx.repo(), vec![base.id()])
        .add_file("public/added", "new")
        .add_file("public/reverted", "temporary change")
        .commit()
        .await?;
    let tip_id = CreateCommitContext::new(base.ctx(), repo_ctx.repo(), vec![middle_id])
        .delete_file("secret/deleted")
        .delete_file("secret/.slacl")
        .add_file("public/reverted", "content")
        .commit()
        .await?;
    let tip = ChangesetContext::new(repo_ctx, tip_id);
    let results = comparison_restrictions(&tip, Some(&base)).await?;
    assert_eq!(
        results.keys().map(ToString::to_string).collect::<Vec<_>>(),
        vec!["public/added", "secret/.slacl", "secret/deleted"],
    );
    assert_eq!(
        results[&NonRootMPath::new("secret/deleted")?][0].repo_region_acl(),
        "REPO_REGION:acl",
    );
    assert!(comparison_restrictions(&tip, Some(&tip)).await?.is_empty());
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_comparison_root_and_empty_commit(fb: FacebookInit) -> Result<()> {
    let (repo_ctx, root) = create_both_mode_test_changeset(
        fb,
        vec![],
        vec![("secret", "REPO_REGION:acl")],
        vec!["secret/file", "public/file"],
    )
    .await?;
    let results = comparison_restrictions(&root, None).await?;
    assert_eq!(results.len(), 3);
    assert!(results[&NonRootMPath::new("public/file")?].is_empty());
    assert_eq!(results[&NonRootMPath::new("secret/file")?].len(), 1);
    let tip_id = CreateCommitContext::new(root.ctx(), repo_ctx.repo(), vec![root.id()])
        .commit()
        .await?;
    let tip = ChangesetContext::new(repo_ctx, tip_id);
    assert!(comparison_restrictions(&tip, None).await?.is_empty());
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_comparison_merge_uses_first_parent(fb: FacebookInit) -> Result<()> {
    let (repo_ctx, root) =
        create_both_mode_test_changeset(fb, vec![], vec![], vec!["public/base"]).await?;
    let left_id = CreateCommitContext::new(root.ctx(), repo_ctx.repo(), vec![root.id()])
        .add_file("public/left", "left")
        .commit()
        .await?;
    let right_id = CreateCommitContext::new(root.ctx(), repo_ctx.repo(), vec![root.id()])
        .add_file("secret/.slacl", "repo_region_acl = \"REPO_REGION:acl\"\n")
        .add_file("secret/right", "right")
        .commit()
        .await?;
    let merge_id = CreateCommitContext::new(root.ctx(), repo_ctx.repo(), vec![left_id, right_id])
        .commit()
        .await?;
    let merge = ChangesetContext::new(repo_ctx.clone(), merge_id);
    let left = ChangesetContext::new(repo_ctx, left_id);
    let results = comparison_restrictions(&merge, None).await?;
    assert_eq!(
        results.keys().map(ToString::to_string).collect::<Vec<_>>(),
        vec!["secret/.slacl", "secret/right"],
    );
    assert_eq!(results[&NonRootMPath::new("secret/right")?].len(), 1);
    assert_eq!(results, comparison_restrictions(&merge, Some(&left)).await?);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_changes_keep_deleted_base_and_unrestricted_target(
    fb: FacebookInit,
) -> Result<()> {
    let (repo_ctx, base) = create_both_mode_test_changeset(
        fb,
        vec![],
        vec![("a", "REPO_REGION:old_acl")],
        vec!["a/deleted", "public/z"],
    )
    .await?;
    let tip_id = CreateCommitContext::new(base.ctx(), repo_ctx.repo(), vec![base.id()])
        .delete_file("a/.slacl")
        .delete_file("a/deleted")
        .add_file("a/new", "new")
        .add_file("a.txt", "sibling")
        .add_file("public/z", "changed")
        .commit()
        .await?;
    let tip = ChangesetContext::new(repo_ctx, tip_id);
    let full = tip
        .changed_paths_restriction_info(None, None, rich_restriction_options(false))
        .await?
        .try_collect::<Vec<_>>()
        .await?;
    assert_eq!(
        full.iter()
            .map(|(path, _)| path.to_string())
            .collect::<Vec<_>>(),
        vec!["a/.slacl", "a/deleted", "a/new", "a.txt", "public/z"]
    );
    let deleted = &full[1].1;
    assert!(deleted.sides.target().is_none());
    let base = deleted.sides.base().expect("deleted file has a base side");
    assert_eq!(
        base.restrictions[0].repo_region_acl(),
        "REPO_REGION:old_acl"
    );
    assert_eq!(base.has_access, None);
    let added = &full[2].1;
    assert!(added.sides.base().is_none());
    assert!(
        added
            .sides
            .target()
            .expect("new file has a target side")
            .restrictions
            .is_empty()
    );
    assert!(full.iter().all(|(_, info)| info.copy_sources.is_empty()));
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_changes_preserve_literal_file_directory_sides(
    fb: FacebookInit,
) -> Result<()> {
    let (repo_ctx, base) = create_both_mode_test_changeset(
        fb,
        vec![],
        vec![("old_dir", "REPO_REGION:old_acl")],
        vec!["old_dir/file", "old_file"],
    )
    .await?;
    let tip_id = CreateCommitContext::new(base.ctx(), repo_ctx.repo(), vec![base.id()])
        .add_file("old_dir", "replacement")
        .delete_file("old_file")
        .add_file(
            "old_file/.slacl",
            "repo_region_acl = \"REPO_REGION:new_acl\"\n",
        )
        .add_file("old_file/file", "replacement")
        .commit()
        .await?;
    let tip = ChangesetContext::new(repo_ctx, tip_id);
    let full = tip
        .changed_paths_restriction_info(None, None, rich_restriction_options(false))
        .await?
        .try_collect::<Vec<_>>()
        .await?;
    assert_eq!(
        full.iter()
            .map(|(path, _)| path.to_string())
            .collect::<Vec<_>>(),
        vec![
            "old_dir",
            "old_dir/.slacl",
            "old_dir/file",
            "old_file",
            "old_file/.slacl",
            "old_file/file"
        ]
    );
    assert!(
        full[0].1.sides.base().is_none(),
        "a base directory is not a file side"
    );
    assert!(full[0].1.sides.target().is_some());
    assert!(full[2].1.sides.target().is_none());
    assert_eq!(
        full[2]
            .1
            .sides
            .base()
            .expect("removed descendant exists on base")
            .restrictions[0]
            .repo_region_acl(),
        "REPO_REGION:old_acl"
    );
    assert!(full[3].1.sides.base().is_some());
    assert!(
        full[3].1.sides.target().is_none(),
        "a target directory is not a file side"
    );
    assert!(full[5].1.sides.base().is_none());
    assert_eq!(
        full[5]
            .1
            .sides
            .target()
            .expect("new descendant exists on target")
            .restrictions[0]
            .repo_region_acl(),
        "REPO_REGION:new_acl"
    );
    Ok(())
}
