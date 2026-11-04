/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use mononoke_api::ChangedPathRestrictionSides;
use mononoke_api::ChangesetId;
use mononoke_api::PathPolicyInfo;
use mononoke_api::PathRestrictionCopySourceInfo;
use mononoke_api::SubtreeSourceMapping;
use mononoke_api::SubtreeSourcePolicyInfo;
use mononoke_types::path::MPath;

use super::*;

fn policy(acl: &str, access: bool) -> PathPolicyInfo {
    PathPolicyInfo {
        restrictions: vec![PathAccessInfo {
            restriction: restricted_paths::PathRestrictionInfo {
                restriction_root: NonRootMPath::new("tent").expect("root"),
                repo_region_acl: acl.into(),
                permission_request_group: MononokeIdentity::from_str(acl).expect("identity"),
                rollout_allowlist_group: None,
            },
            has_access: Some(access),
        }],
        has_access: Some(access),
    }
}

#[mononoke::test]
fn every_changed_path_variant_has_content_and_requires_checked_access() {
    for access in [Some(true), Some(false), None] {
        let mut side = policy("REPO_REGION:acl", access == Some(true));
        side.has_access = access;
        let source = SubtreeSourcePolicyInfo {
            mapping: SubtreeSourceMapping {
                commit_id: ChangesetId::from_str(&"a".repeat(64)).expect("id"),
                source_prefix: MPath::new("source").expect("source prefix"),
                destination_prefix: MPath::new("tent").expect("destination prefix"),
            },
            info: side.clone(),
        };
        for sides in [
            ChangedPathRestrictionSides::Added(side.clone()),
            ChangedPathRestrictionSides::Removed(side.clone()),
            ChangedPathRestrictionSides::Changed {
                base: side.clone(),
                target: side.clone(),
            },
            ChangedPathRestrictionSides::Unchanged {
                base: side.clone(),
                target: side.clone(),
            },
            ChangedPathRestrictionSides::SourceOnly(source),
        ] {
            let info = ChangedPathRestrictionInfo {
                sides,
                copy_sources: vec![],
                subtree_source: None,
                permission_change: None,
            };
            assert!(
                info.content_sides().next().is_some(),
                "every typed variant carries actual content"
            );
            let response = changes_restriction_results_into_response(vec![(
                NonRootMPath::new("tent/file").expect("path"),
                info,
            )]);
            assert_eq!(response.are_restricted, thrift::PathCoverage::ALL);
            assert_eq!(
                response.has_access,
                if access == Some(true) {
                    thrift::PathCoverage::ALL
                } else {
                    thrift::PathCoverage::NONE
                }
            );
            assert_eq!(response.authorized_paths.is_empty(), access != Some(true));
        }
    }
}

#[mononoke::fbinit_test]
async fn checked_unrestricted_content_is_authorized_without_restrictions(
    fb: FacebookInit,
) -> Result<()> {
    let base = create_test_changeset_with_acl_provider(
        fb,
        vec![("secret", "REPO_REGION:acl")],
        TestAclProvider::new(false),
    )
    .await?;
    let added_id = CreateCommitContext::new(base.ctx(), base.repo_ctx().repo(), vec![base.id()])
        .add_file("public/file", "unrestricted content")
        .commit()
        .await?;
    let added = base
        .repo_ctx()
        .changeset(added_id)
        .await?
        .context("added commit exists")?;
    let results: Vec<_> = added
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
    assert_eq!(results.len(), 1);
    let info = &results[0].1;
    assert!(
        info.content_sides().next().is_some(),
        "the file exists even though it has no restrictions"
    );
    for side in info.content_sides() {
        assert!(side.restrictions.is_empty());
        assert_eq!(
            side.has_access,
            Some(true),
            "no required ACL denies checked unrestricted content"
        );
    }
    let response = changes_restriction_results_into_response(results);
    assert_eq!(response.are_restricted, thrift::PathCoverage::NONE);
    assert_eq!(response.has_access, thrift::PathCoverage::ALL);
    assert_eq!(response.changed_paths, vec!["public/file"]);
    assert_eq!(response.authorized_paths, vec!["public/file"]);
    assert!(response.restriction_roots.is_empty());
    Ok(())
}

#[mononoke::test]
fn unchecked_empty_restrictions_do_not_prove_authorization() {
    let info = ChangedPathRestrictionInfo {
        sides: ChangedPathRestrictionSides::Added(PathPolicyInfo {
            restrictions: vec![],
            has_access: None,
        }),
        copy_sources: vec![],
        subtree_source: None,
        permission_change: None,
    };
    let response = changes_restriction_results_into_response(vec![(
        NonRootMPath::new("public/file").expect("path"),
        info,
    )]);
    assert_eq!(response.are_restricted, thrift::PathCoverage::NONE);
    assert_eq!(response.has_access, thrift::PathCoverage::NONE);
    assert_eq!(response.changed_paths, vec!["public/file"]);
    assert!(response.authorized_paths.is_empty());
}

#[mononoke::test]
fn empty_comparison_has_no_denials_and_all_access() {
    let response = changes_restriction_results_into_response(vec![]);
    assert_eq!(response.are_restricted, thrift::PathCoverage::NONE);
    assert_eq!(response.has_access, thrift::PathCoverage::ALL);
    assert_eq!(response.checked_both_sides, Some(true));
    assert!(response.changed_paths.is_empty());
    assert!(response.authorized_paths.is_empty());
    assert!(response.restriction_roots.is_empty());
}

#[mononoke::test]
fn unchecked_copy_source_cannot_grant_complete_comparison_access() {
    let target = policy("REPO_REGION:target", true);
    let mut source = policy("REPO_REGION:source", true);
    source.has_access = None;
    let info = ChangedPathRestrictionInfo {
        sides: ChangedPathRestrictionSides::Added(target),
        copy_sources: vec![PathRestrictionCopySourceInfo {
            commit_id: ChangesetId::from_str(&"a".repeat(64)).expect("id"),
            path: NonRootMPath::new("source/file").expect("path"),
            info: source,
        }],
        subtree_source: None,
        permission_change: None,
    };
    let response = changes_restriction_results_into_response(vec![(
        NonRootMPath::new("tent/file").expect("path"),
        info,
    )]);
    assert_eq!(response.has_access, thrift::PathCoverage::NONE);
    assert!(response.authorized_paths.is_empty());
}
