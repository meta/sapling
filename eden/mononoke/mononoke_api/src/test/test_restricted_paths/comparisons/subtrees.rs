/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

mod mappings;

async fn with_subtrees(
    seed: &ChangesetContext<Repo>,
    changes: Vec<(&str, SubtreeChange)>,
) -> Result<ChangesetContext<Repo>> {
    let mut bonsai = seed.bonsai_changeset().await?.into_mut();
    for (destination, change) in changes {
        bonsai
            .subtree_changes
            .insert(MPath::new(destination)?, change);
    }
    let bonsai = bonsai.freeze()?;
    let id = bonsai.get_changeset_id();
    with_just_knobs_async(
        JustKnobsInMemory::new(HashMap::from([(
            "scm/mononoke:enable_subtree_changes".to_owned(),
            KnobVal::Bool(true),
        )])),
        save_changesets(seed.ctx(), seed.repo_ctx().repo(), vec![bonsai]).boxed(),
    )
    .await?;
    Ok(ChangesetContext::new(seed.repo_ctx().clone(), id))
}

async fn both_views(
    target: &ChangesetContext<Repo>,
    base: Option<&ChangesetContext<Repo>>,
    after: Option<MPath>,
) -> Result<Vec<(NonRootMPath, ChangedPathRestrictionInfo)>> {
    Ok(target
        .changed_paths_restriction_info(
            base,
            after,
            ChangedPathsRestrictionOptions {
                check_permissions: true,
                include_permission_changes: true,
                include_subtree_source_changes: true,
            },
        )
        .await?
        .try_collect()
        .await?)
}

#[mononoke::fbinit_test]
async fn source_relative_paths_include_unchanged_destinations_and_source_only_deletions(
    fb: FacebookInit,
) -> Result<()> {
    let (repo, source) = create_permission_aware_both_mode_test_changeset(
        fb,
        vec![],
        vec![("secret", "REPO_REGION:source_acl")],
        vec!["secret/file", "secret/deleted"],
    )
    .await?;
    let parent_id = CreateCommitContext::new(source.ctx(), repo.repo(), vec![source.id()])
        .add_file("public/copy/file", "parent")
        .commit()
        .await?;
    let seed_id = CreateCommitContext::new(source.ctx(), repo.repo(), vec![parent_id])
        .add_file("public/copy/file", "parent")
        .delete_file("public/copy/.slacl")
        .delete_file("public/copy/deleted")
        .commit()
        .await?;
    let seed = ChangesetContext::new(repo.clone(), seed_id);
    let target = with_subtrees(
        &seed,
        vec![(
            "public/copy",
            SubtreeChange::deep_copy(MPath::new("secret")?, source.id()),
        )],
    )
    .await?;
    let legacy: Vec<_> = target
        .changed_paths_restriction_info(
            None,
            None,
            ChangedPathsRestrictionOptions {
                check_permissions: true,
                include_permission_changes: false,
                include_subtree_source_changes: false,
            },
        )
        .await?
        .try_collect()
        .await?;
    assert!(
        legacy.is_empty(),
        "literal parent and target have identical endpoint manifests"
    );
    let details = both_views(&target, None, None).await?;
    assert_eq!(
        details
            .iter()
            .map(|(path, _)| path.to_string())
            .collect::<Vec<_>>(),
        vec![
            "public/copy/.slacl",
            "public/copy/deleted",
            "public/copy/file"
        ]
    );
    for (path, info) in &details {
        let actual = info.subtree_source().expect("actual source file");
        assert_eq!(actual.mapping.commit_id, source.id());
        assert_eq!(actual.mapping.source_prefix, MPath::new("secret")?);
        assert_eq!(
            actual.mapping.destination_prefix,
            MPath::new("public/copy")?
        );
        assert_eq!(actual.info.has_access, Some(false));
        assert_eq!(
            info.restriction_info()
                .next()
                .expect("source restriction")
                .repo_region_acl(),
            "REPO_REGION:source_acl"
        );
        assert!(
            info.copy_sources.is_empty(),
            "subtrees do not become per-file copy declarations"
        );
        assert!(
            info.permission_change
                .as_ref()
                .expect("source edge policy difference")
                .subtree_source
                .is_some()
        );
        if path.to_string() == "public/copy/deleted" {
            assert!(info.sides.base().is_none() && info.sides.target().is_none());
        }
    }
    for (index, (cursor, _)) in details.iter().enumerate() {
        assert_eq!(
            both_views(&target, None, Some(MPath::from(cursor.clone()))).await?,
            details[index + 1..]
        );
    }
    let distant = both_views(&target, Some(&source), None).await?;
    assert!(
        distant
            .iter()
            .all(|(_, info)| info.subtree_source().is_none()),
        "distant comparisons do not interpret copy declarations"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn same_tent_source_policy_is_omitted_from_sparse_changes(fb: FacebookInit) -> Result<()> {
    let (repo, source) = create_permission_aware_both_mode_test_changeset(
        fb,
        vec![],
        vec![("tent", "REPO_REGION:acl")],
        vec!["tent/source/file"],
    )
    .await?;
    let parent = CreateCommitContext::new(source.ctx(), repo.repo(), vec![source.id()])
        .add_file("tent/dest/file", "parent")
        .commit()
        .await?;
    let seed = CreateCommitContext::new(source.ctx(), repo.repo(), vec![parent])
        .add_file("tent/dest/file", "parent")
        .commit()
        .await?;
    let target = with_subtrees(
        &ChangesetContext::new(repo, seed),
        vec![(
            "tent/dest",
            SubtreeChange::deep_copy(MPath::new("tent/source")?, source.id()),
        )],
    )
    .await?;
    let details = both_views(&target, None, None).await?;
    assert_eq!(details.len(), 1);
    let info = &details[0].1;
    assert!(
        info.subtree_source().is_some(),
        "actual source content still contributes restrictions"
    );
    assert!(
        info.permission_change.is_none(),
        "same effective policies need no sparse source association"
    );
    assert_eq!(info.content_sides().count(), 3);
    Ok(())
}

#[mononoke::fbinit_test]
async fn subtree_merges_keep_the_literal_parent_comparison(fb: FacebookInit) -> Result<()> {
    let (repo, source) =
        create_both_mode_test_changeset(fb, vec![], vec![], vec!["source/file"]).await?;
    let parent = CreateCommitContext::new(source.ctx(), repo.repo(), vec![source.id()])
        .add_file("dest/file", "parent")
        .commit()
        .await?;
    let seed = CreateCommitContext::new(source.ctx(), repo.repo(), vec![parent])
        .add_file("dest/file", "parent")
        .commit()
        .await?;
    let target = with_subtrees(
        &ChangesetContext::new(repo, seed),
        vec![(
            "dest",
            SubtreeChange::merge(MPath::new("source")?, source.id()),
        )],
    )
    .await?;
    assert!(both_views(&target, None, None).await?.is_empty());
    Ok(())
}

#[mononoke::fbinit_test]
async fn subtree_sources_apply_to_second_parent_comparisons(fb: FacebookInit) -> Result<()> {
    let (repo, source) = create_permission_aware_both_mode_test_changeset(
        fb,
        vec![],
        vec![("secret", "REPO_REGION:acl")],
        vec!["secret/file"],
    )
    .await?;
    let left = CreateCommitContext::new(source.ctx(), repo.repo(), vec![source.id()])
        .add_file("public/dest/file", "left")
        .commit()
        .await?;
    let right_id = CreateCommitContext::new(source.ctx(), repo.repo(), vec![source.id()])
        .add_file("public/dest/file", "right")
        .commit()
        .await?;
    let seed = CreateCommitContext::new(source.ctx(), repo.repo(), vec![left, right_id])
        .add_file("public/dest/file", "right")
        .delete_file("public/dest/.slacl")
        .commit()
        .await?;
    let target = with_subtrees(
        &ChangesetContext::new(repo.clone(), seed),
        vec![(
            "public/dest",
            SubtreeChange::deep_copy(MPath::new("secret")?, source.id()),
        )],
    )
    .await?;
    let right = ChangesetContext::new(repo, right_id);
    let details = both_views(&target, Some(&right), None).await?;
    assert!(
        details
            .iter()
            .any(|(path, info)| path.to_string() == "public/dest/file"
                && info.subtree_source().is_some())
    );
    Ok(())
}
