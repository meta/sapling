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
async fn test_restriction_comparison_deep_copy_uses_source_revision(
    fb: FacebookInit,
) -> Result<()> {
    let (repo_ctx, source) = create_both_mode_test_changeset(
        fb,
        vec![],
        vec![("secret", "REPO_REGION:source_acl")],
        vec!["secret/file"],
    )
    .await?;
    let parent_id = CreateCommitContext::new(source.ctx(), repo_ctx.repo(), vec![source.id()])
        .add_file(
            "secret/.slacl",
            "repo_region_acl = \"REPO_REGION:current_acl\"\n",
        )
        .add_file("public/copy/file", "old destination")
        .add_file("public/copy/absent", "not in source")
        .commit()
        .await?;
    let seed_id = CreateCommitContext::new(source.ctx(), repo_ctx.repo(), vec![parent_id])
        .delete_file("public/copy/.slacl")
        .delete_file("public/copy/absent")
        .add_file("public/copy/file", "content")
        .add_file("public/copy/new", "new destination content")
        .commit()
        .await?;
    let seed = ChangesetContext::new(repo_ctx.clone(), seed_id);
    let mut bonsai = seed.bonsai_changeset().await?.into_mut();
    bonsai.subtree_changes.insert(
        MPath::new("public/copy")?,
        SubtreeChange::deep_copy(MPath::new("secret")?, source.id()),
    );
    let bonsai = bonsai.freeze()?;
    let tip_id = bonsai.get_changeset_id();
    with_just_knobs_async(
        JustKnobsInMemory::new(HashMap::from([(
            "scm/mononoke:enable_subtree_changes".to_string(),
            KnobVal::Bool(true),
        )])),
        save_changesets(source.ctx(), repo_ctx.repo(), vec![bonsai]).boxed(),
    )
    .await?;
    let tip = ChangesetContext::new(repo_ctx, tip_id);
    let results = comparison_restrictions(&tip, None).await?;
    assert_eq!(
        results.keys().map(ToString::to_string).collect::<Vec<_>>(),
        vec!["public/copy/absent", "public/copy/file", "public/copy/new"],
    );
    assert!(results[&NonRootMPath::new("public/copy/absent")?].is_empty());
    assert!(results[&NonRootMPath::new("public/copy/new")?].is_empty());
    assert_eq!(
        results[&NonRootMPath::new("public/copy/file")?][0].repo_region_acl(),
        "REPO_REGION:source_acl",
    );
    let range = comparison_restrictions(&tip, Some(&source)).await?;
    assert!(range[&NonRootMPath::new("public/copy/file")?].is_empty());
    let details: BTreeMap<_, _> = tip
        .changed_paths_restriction_info(None, None, rich_restriction_options(false))
        .await?
        .try_collect()
        .await?;
    let copied = &details[&NonRootMPath::new("public/copy/file")?];
    assert!(
        copied
            .sides
            .base()
            .expect("literal destination existed on parent")
            .restrictions
            .is_empty()
    );
    assert!(
        copied
            .sides
            .target()
            .expect("destination exists on target")
            .restrictions
            .is_empty()
    );
    assert!(copied.copy_sources.is_empty());
    let subtree = copied
        .subtree_source
        .as_ref()
        .expect("declared subtree source");
    assert_eq!(subtree.mapping.commit_id, source.id());
    assert_eq!(subtree.mapping.source_prefix, MPath::new("secret")?);
    assert_eq!(
        subtree.mapping.destination_prefix,
        MPath::new("public/copy")?
    );
    let as_deleted = CreateCommitContext::new(source.ctx(), tip.repo_ctx().repo(), vec![tip.id()])
        .delete_file("public/copy/file")
        .commit()
        .await?;
    let removed = ChangesetContext::new(tip.repo_ctx().clone(), as_deleted);
    let mut removed_bonsai = removed.bonsai_changeset().await?.into_mut();
    removed_bonsai.subtree_changes.insert(
        MPath::new("public/copy")?,
        SubtreeChange::deep_copy(MPath::new("secret")?, source.id()),
    );
    let removed_bonsai = removed_bonsai.freeze()?;
    let removed_id = removed_bonsai.get_changeset_id();
    with_just_knobs_async(
        JustKnobsInMemory::new(HashMap::from([(
            "scm/mononoke:enable_subtree_changes".to_string(),
            KnobVal::Bool(true),
        )])),
        save_changesets(source.ctx(), tip.repo_ctx().repo(), vec![removed_bonsai]).boxed(),
    )
    .await?;
    let removed = ChangesetContext::new(tip.repo_ctx().clone(), removed_id);
    let results = comparison_restrictions(&removed, None).await?;
    assert_eq!(
        results[&NonRootMPath::new("public/copy/file")?][0].repo_region_acl(),
        "REPO_REGION:source_acl"
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
