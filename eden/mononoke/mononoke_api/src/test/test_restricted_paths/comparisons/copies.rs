/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

#[mononoke::fbinit_test]
async fn test_restriction_comparison_copy_from_second_parent(fb: FacebookInit) -> Result<()> {
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
        .add_file_with_copy_info("public/copy", "right", (right_id, "secret/right"))
        .commit()
        .await?;
    let merge = ChangesetContext::new(repo_ctx.clone(), merge_id);
    let right = ChangesetContext::new(repo_ctx, right_id);
    let first_parent = comparison_restrictions(&merge, None).await?;
    assert!(first_parent[&NonRootMPath::new("public/copy")?].is_empty());
    let second_parent = comparison_restrictions(&merge, Some(&right)).await?;
    assert_eq!(
        second_parent
            .keys()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec!["public/copy", "public/left"],
    );
    assert_eq!(
        second_parent[&NonRootMPath::new("public/copy")?][0].repo_region_acl(),
        "REPO_REGION:acl"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_comparison_copy_overwrites_existing_destination(
    fb: FacebookInit,
) -> Result<()> {
    let (repo_ctx, root) = create_both_mode_test_changeset(
        fb,
        vec![],
        vec![("secret", "REPO_REGION:source_acl")],
        vec!["secret/source", "public/dest"],
    )
    .await?;
    let parent_id = CreateCommitContext::new(root.ctx(), repo_ctx.repo(), vec![root.id()])
        .add_file("public/dest", "old content")
        .commit()
        .await?;
    let parent = ChangesetContext::new(repo_ctx.clone(), parent_id);
    let tip_id = CreateCommitContext::new(root.ctx(), repo_ctx.repo(), vec![parent_id])
        .add_file_with_copy_info(
            "public/dest",
            "secret derivative",
            (parent_id, "secret/source"),
        )
        .commit()
        .await?;
    let tip = ChangesetContext::new(repo_ctx, tip_id);
    let results = comparison_restrictions(&tip, Some(&parent)).await?;
    assert_eq!(
        results.keys().map(ToString::to_string).collect::<Vec<_>>(),
        vec!["public/dest"]
    );
    assert!(results[&NonRootMPath::new("public/dest")?].is_empty());
    let range = comparison_restrictions(&tip, Some(&root)).await?;
    assert!(range[&NonRootMPath::new("public/dest")?].is_empty());
    let details: BTreeMap<_, _> = tip
        .changed_paths_restriction_info(Some(&parent), None, rich_restriction_options(false))
        .await?
        .try_collect()
        .await?;
    let dest = &details[&NonRootMPath::new("public/dest")?];
    assert!(dest.sides.base().is_some());
    assert!(dest.sides.target().is_some());
    assert!(
        dest.copy_sources.is_empty(),
        "overwriting a file is not rendered as a copy"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_copy_replacing_directory_has_no_source(fb: FacebookInit) -> Result<()> {
    let (repo_ctx, base) = create_both_mode_test_changeset(
        fb,
        vec![],
        vec![("secret", "REPO_REGION:source_acl")],
        vec!["secret/source", "public/dest/file"],
    )
    .await?;
    let tip_id = CreateCommitContext::new(base.ctx(), repo_ctx.repo(), vec![base.id()])
        .add_file_with_copy_info("public/dest", "content", (base.id(), "secret/source"))
        .commit()
        .await?;
    let tip = ChangesetContext::new(repo_ctx, tip_id);
    let details: BTreeMap<_, _> = tip
        .changed_paths_restriction_info(None, None, rich_restriction_options(false))
        .await?
        .try_collect()
        .await?;
    let dest = &details[&NonRootMPath::new("public/dest")?];
    assert!(
        dest.sides.base().is_none(),
        "a directory is not a literal base file side"
    );
    assert!(dest.sides.target().is_some());
    assert!(
        dest.copy_sources.is_empty(),
        "an existing directory excludes copy recognition"
    );
    assert!(dest.restriction_info().next().is_none());
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_copy_only_in_selected_parent_comparison(fb: FacebookInit) -> Result<()> {
    let (repo_ctx, source) = create_both_mode_test_changeset(
        fb,
        vec![],
        vec![("secret", "REPO_REGION:source_acl")],
        vec!["secret/source"],
    )
    .await?;
    let middle_id = CreateCommitContext::new(source.ctx(), repo_ctx.repo(), vec![source.id()])
        .add_file("public/middle", "middle")
        .commit()
        .await?;
    let tip_id = CreateCommitContext::new(source.ctx(), repo_ctx.repo(), vec![middle_id])
        .add_file_with_copy_info("public/new", "content", (middle_id, "secret/source"))
        .commit()
        .await?;
    let tip = ChangesetContext::new(repo_ctx, tip_id);
    for base in [None, Some(&source)] {
        let details: BTreeMap<_, _> = tip
            .changed_paths_restriction_info(base, None, rich_restriction_options(false))
            .await?
            .try_collect()
            .await?;
        let added = &details[&NonRootMPath::new("public/new")?];
        if base.is_none() {
            assert_eq!(added.copy_sources.len(), 1);
            assert_eq!(added.copy_sources[0].commit_id, middle_id);
        } else {
            assert!(
                added.copy_sources.is_empty(),
                "non-parent comparisons do not render copy sources"
            );
        }
        assert!(added.sides.base().is_none());
        assert!(
            added
                .sides
                .target()
                .expect("added destination exists")
                .restrictions
                .is_empty()
        );
    }
    let range = assert_rich_restriction_pages(&tip, Some(&source), false).await?;
    assert!(range.iter().all(|(_, info)| info.copy_sources.is_empty()));
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_copy_keeps_literal_base_absent(fb: FacebookInit) -> Result<()> {
    let (repo_ctx, parent) = create_both_mode_test_changeset(
        fb,
        vec![],
        vec![("secret", "REPO_REGION:source_acl")],
        vec!["secret/source"],
    )
    .await?;
    let tip_id = CreateCommitContext::new(parent.ctx(), repo_ctx.repo(), vec![parent.id()])
        .add_file_with_copy_info("public/new", "content", (parent.id(), "secret/source"))
        .commit()
        .await?;
    let tip = ChangesetContext::new(repo_ctx, tip_id);
    let details: BTreeMap<_, _> = tip
        .changed_paths_restriction_info(None, None, rich_restriction_options(true))
        .await?
        .try_collect()
        .await?;
    let added = &details[&NonRootMPath::new("public/new")?];
    assert!(added.sides.base().is_none());
    let target = added.sides.target().expect("copy destination exists");
    assert!(target.restrictions.is_empty());
    assert_eq!(target.has_access, Some(true));
    assert_eq!(added.copy_sources.len(), 1);
    assert_eq!(added.copy_sources[0].commit_id, parent.id());
    assert_eq!(
        added.copy_sources[0].path,
        NonRootMPath::new("secret/source")?
    );
    assert_eq!(
        added.copy_sources[0].info.restrictions[0].repo_region_acl(),
        "REPO_REGION:source_acl"
    );
    assert!(added.copy_sources[0].info.has_access.is_some());
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_pages_preserve_rich_endpoint_and_copy_metadata(
    fb: FacebookInit,
) -> Result<()> {
    let (repo_ctx, base) = create_both_mode_test_changeset(
        fb,
        vec![],
        vec![("secret", "REPO_REGION:old_acl")],
        vec![
            "secret/source",
            "secret/deleted",
            "old_dir/file",
            "old_file",
            "a/b",
        ],
    )
    .await?;
    let tip_id = CreateCommitContext::new(base.ctx(), repo_ctx.repo(), vec![base.id()])
        .delete_file("secret/.slacl")
        .delete_file("secret/deleted")
        .add_file("old_dir", "replacement")
        .delete_file("old_file")
        .add_file("old_file/new", "new descendant")
        .add_file("a/b", "modified")
        .add_file("a.txt", "dotted sibling")
        .add_file_with_copy_info("public/copy", "content", (base.id(), "secret/source"))
        .commit()
        .await?;
    let tip = ChangesetContext::new(repo_ctx, tip_id);
    let full = assert_rich_restriction_pages(&tip, None, true).await?;
    let copy = &full
        .iter()
        .find(|(path, _)| path.to_string() == "public/copy")
        .expect("copy path is emitted")
        .1;
    assert!(copy.sides.base().is_none());
    assert!(
        copy.sides
            .target()
            .expect("target file exists")
            .restrictions
            .is_empty()
    );
    assert_eq!(copy.copy_sources.len(), 1);
    assert_eq!(copy.copy_sources[0].commit_id, base.id());
    assert_eq!(
        copy.copy_sources[0].path,
        NonRootMPath::new("secret/source")?
    );
    let deleted = &full
        .iter()
        .find(|(path, _)| path.to_string() == "secret/deleted")
        .expect("deleted path is emitted")
        .1;
    assert!(deleted.sides.target().is_none());
    assert_eq!(
        deleted.sides.base().expect("base file exists").restrictions[0].repo_region_acl(),
        "REPO_REGION:old_acl"
    );
    assert_eq!(
        full,
        assert_rich_restriction_pages(&tip, Some(&base), true).await?
    );
    Ok(())
}
