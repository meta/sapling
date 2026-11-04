/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use pretty_assertions::assert_eq;

use super::*;

#[mononoke::fbinit_test]
async fn nested_copy_selects_nearest_mapping_without_outer_source_leaves(
    fb: FacebookInit,
) -> Result<()> {
    let (repo, source) = create_permission_aware_both_mode_test_changeset(
        fb,
        vec![],
        vec![
            ("secret", "REPO_REGION:outer"),
            ("other", "REPO_REGION:inner"),
        ],
        vec!["secret/a", "secret/nested/leak", "other/b"],
    )
    .await?;
    let parent = CreateCommitContext::new(source.ctx(), repo.repo(), vec![source.id()])
        .add_file("public/a", "parent")
        .add_file("public/nested/b", "parent")
        .commit()
        .await?;
    let seed = CreateCommitContext::new(source.ctx(), repo.repo(), vec![parent])
        .add_file("public/a", "parent")
        .commit()
        .await?;
    let target = with_subtrees(
        &ChangesetContext::new(repo, seed),
        vec![
            (
                "public",
                SubtreeChange::deep_copy(MPath::new("secret")?, source.id()),
            ),
            (
                "public/nested",
                SubtreeChange::deep_copy(MPath::new("other")?, source.id()),
            ),
        ],
    )
    .await?;
    let results = both_views(&target, None, None).await?;
    assert!(
        !results
            .iter()
            .any(|(path, _)| path.to_string() == "public/nested/leak")
    );
    for (path, expected_source, expected_destination) in [
        ("public/a", "secret", "public"),
        ("public/nested/b", "other", "public/nested"),
    ] {
        let info = &results
            .iter()
            .find(|(p, _)| p.to_string() == path)
            .expect("source-relative change")
            .1;
        let mapping = &info
            .subtree_source()
            .expect("actual source content")
            .mapping;
        assert_eq!(mapping.commit_id, source.id());
        assert_eq!(mapping.source_prefix, MPath::new(expected_source)?);
        assert_eq!(
            mapping.destination_prefix,
            MPath::new(expected_destination)?
        );
        assert_eq!(
            info.permission_change
                .as_ref()
                .expect("source policy differs")
                .subtree_source
                .as_ref()
                .expect("sparse source mapping")
                .mapping,
            *mapping
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn root_source_prefix_reparents_files_and_deleted_root_acl(fb: FacebookInit) -> Result<()> {
    let (repo, initial) =
        create_permission_aware_both_mode_test_changeset(fb, vec![], vec![], vec!["file"]).await?;
    let source_id = CreateCommitContext::new(initial.ctx(), repo.repo(), vec![initial.id()])
        .add_file(".slacl", "repo_region_acl = \"root_source\"\n")
        .commit()
        .await?;
    let source = ChangesetContext::new(repo.clone(), source_id);
    let parent = CreateCommitContext::new(source.ctx(), repo.repo(), vec![source.id()])
        .delete_file(".slacl")
        .delete_file("file")
        .add_file("public/file", "parent")
        .commit()
        .await?;
    let seed = CreateCommitContext::new(source.ctx(), repo.repo(), vec![parent])
        .add_file("public/file", "parent")
        .commit()
        .await?;
    let target = with_subtrees(
        &ChangesetContext::new(repo, seed),
        vec![("public", SubtreeChange::deep_copy(MPath::ROOT, source.id()))],
    )
    .await?;
    let results = both_views(&target, None, None).await?;
    assert_eq!(
        results
            .iter()
            .map(|(path, _)| path.to_string())
            .collect::<Vec<_>>(),
        vec!["public/.slacl", "public/file"]
    );
    for (_, info) in results {
        let actual = info.subtree_source().expect("source root file");
        assert_eq!(actual.mapping.source_prefix, MPath::ROOT);
        assert_eq!(actual.mapping.destination_prefix, MPath::new("public")?);
        assert_eq!(actual.mapping.commit_id, source.id());
    }
    let replacement =
        CreateCommitContext::new(source.ctx(), source.repo_ctx().repo(), vec![parent])
            .add_file("public", "target")
            .commit()
            .await?;
    let replacement = with_subtrees(
        &ChangesetContext::new(source.repo_ctx().clone(), replacement),
        vec![("public", SubtreeChange::deep_copy(MPath::ROOT, source.id()))],
    )
    .await?;
    assert_eq!(
        both_views(&replacement, None, None)
            .await?
            .iter()
            .map(|(path, _)| path.to_string())
            .collect::<Vec<_>>(),
        vec!["public", "public/.slacl", "public/file"],
        "replacing a copied directory checks a hypothetical source-root policy without treating the directory as content",
    );
    Ok(())
}
