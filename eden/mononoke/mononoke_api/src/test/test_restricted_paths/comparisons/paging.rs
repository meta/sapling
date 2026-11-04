/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

#[mononoke::fbinit_test]
async fn test_restriction_root_changes_use_component_order_and_target_sides(
    fb: FacebookInit,
) -> Result<()> {
    let (_, root) = create_both_mode_test_changeset(
        fb,
        vec![],
        vec![("a", "REPO_REGION:acl")],
        vec!["a/b", "a/deep/c", "a.txt", "z/file"],
    )
    .await?;
    let full = root
        .changed_paths_restriction_info(None, None, rich_restriction_options(false))
        .await?
        .try_collect::<Vec<_>>()
        .await?;
    assert_eq!(
        full.iter()
            .map(|(path, _)| path.to_string())
            .collect::<Vec<_>>(),
        vec!["a/.slacl", "a/b", "a/deep/c", "a.txt", "z/file"],
    );
    assert!(full.iter().all(|(_, info)| info.sides.base().is_none()));
    assert!(full.iter().all(|(_, info)| info.sides.target().is_some()));
    assert_eq!(
        full[1]
            .1
            .sides
            .target()
            .expect("root file has a target side")
            .restrictions[0]
            .repo_region_acl(),
        "REPO_REGION:acl"
    );
    assert!(
        full[3]
            .1
            .sides
            .target()
            .expect("public root file exists")
            .restrictions
            .is_empty()
    );
    let checked = root
        .changed_paths_restriction_info(None, None, rich_restriction_options(true))
        .await?
        .try_collect::<Vec<_>>()
        .await?;
    assert!(checked.iter().all(|(_, info)| {
        info.sides
            .target()
            .expect("root target exists")
            .has_access
            .is_some()
    }));
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_comparison_large_unrestricted_root_is_complete(
    fb: FacebookInit,
) -> Result<()> {
    let (repo_ctx, base) = create_both_mode_test_changeset(fb, vec![], vec![], vec![]).await?;
    let commit = (0..10_005).fold(
        CreateCommitContext::new(base.ctx(), repo_ctx.repo(), vec![base.id()]),
        |commit, n| commit.add_file(format!("f{n:05}").as_str(), "content"),
    );
    let tip_id = commit.commit().await?;
    let tip = ChangesetContext::new(repo_ctx, tip_id);
    let full = tip
        .changed_paths_restriction_info(None, None, rich_restriction_options(false))
        .await?
        .try_collect::<Vec<_>>()
        .await?;
    assert_eq!(full.len(), 10_005);
    assert_eq!(full[0].0.to_string(), "f00000");
    assert_eq!(full[10_004].0.to_string(), "f10004");
    assert!(
        full.iter()
            .all(|(_, info)| info.restriction_info().next().is_none())
    );
    let tail = tip
        .changed_paths_restriction_info(
            None,
            Some(MPath::new("f09998")?),
            rich_restriction_options(false),
        )
        .await?
        .take(9)
        .try_collect::<Vec<_>>()
        .await?;
    assert_eq!(tail.len(), 6);
    assert_eq!(tail[0].0.to_string(), "f09999");
    assert_eq!(tail[5].0.to_string(), "f10004");
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_root_pages_preserve_rich_details_and_component_order(
    fb: FacebookInit,
) -> Result<()> {
    let (_, root) = create_both_mode_test_changeset(
        fb,
        vec![],
        vec![("a", "REPO_REGION:acl")],
        vec!["a/b", "a/deep/c", "a.txt", "z/file"],
    )
    .await?;
    let full = assert_rich_restriction_pages(&root, None, true).await?;
    assert_eq!(
        full.iter()
            .map(|(path, _)| path.to_string())
            .collect::<Vec<_>>(),
        vec!["a/.slacl", "a/b", "a/deep/c", "a.txt", "z/file"]
    );
    assert!(full.iter().all(|(_, info)| info.sides.base().is_none()));
    assert!(full.iter().all(|(_, info)| info.copy_sources.is_empty()));
    let after_directory = root
        .changed_paths_restriction_info(
            None,
            Some(MPath::new("a")?),
            rich_restriction_options(true),
        )
        .await?
        .try_collect::<Vec<_>>()
        .await?;
    assert_eq!(after_directory, full);
    Ok(())
}
