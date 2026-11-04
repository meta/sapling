/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use mononoke_api::ChangedPathRestrictionSides;
use mononoke_api::PathPolicyInfo;
use mononoke_api::PathRestrictionCopySourceInfo;

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
fn legacy_response_requires_base_target_and_copy_source_access() {
    let base = policy("REPO_REGION:old", false);
    let target = policy("REPO_REGION:new", true);
    let source = PathRestrictionCopySourceInfo {
        commit_id: ChangesetId::from_str(&"a".repeat(64)).expect("id"),
        path: NonRootMPath::new("source/file").expect("path"),
        info: policy("REPO_REGION:source", false),
    };
    let info = ChangedPathRestrictionInfo {
        sides: ChangedPathRestrictionSides::Changed { base, target },
        copy_sources: vec![source],
        subtree_source: None,
        permission_change: None,
    };
    let response = changes_restriction_results_into_response(vec![(
        NonRootMPath::new("tent/file").expect("path"),
        info,
    )]);
    assert_eq!(response.checked_both_sides, Some(true));
    assert_eq!(response.has_access, thrift::PathCoverage::NONE);
    assert!(response.authorized_paths.is_empty());
    assert!(response.compact.is_none());
    assert_eq!(response.restriction_roots["tent/file"].len(), 3);
}

#[mononoke::test]
fn legacy_deleted_content_keeps_its_covering_policy() {
    let info = ChangedPathRestrictionInfo {
        sides: ChangedPathRestrictionSides::Removed(policy("REPO_REGION:deleted", false)),
        copy_sources: vec![],
        subtree_source: None,
        permission_change: None,
    };
    let response = changes_restriction_results_into_response(vec![(
        NonRootMPath::new("tent/deleted").expect("path"),
        info,
    )]);
    assert_eq!(response.are_restricted, thrift::PathCoverage::ALL);
    assert_eq!(
        response.restriction_roots["tent/deleted"][0].acls,
        vec!["REPO_REGION:deleted"]
    );
    let empty = changes_restriction_results_into_response(vec![]);
    assert_eq!(empty.has_access, thrift::PathCoverage::ALL);
    assert_eq!(empty.checked_both_sides, Some(true));
}

#[mononoke::test]
fn new_capabilities_require_compact_and_paging() {
    assert!(matches!(
        restriction_comparison_request(thrift::CommitRestrictedPathsChangesParams::default()),
        Ok(RestrictionComparisonRequest::Legacy)
    ));
    let ordered = thrift::CommitRestrictedPathsChangesOrderedParams {
        limit: 1,
        ..Default::default()
    };
    let base = thrift::CommitId::bonsai(vec![0; 32]);
    for params in [
        thrift::CommitRestrictedPathsChangesParams {
            other_commit_id: Some(base.clone()),
            ..Default::default()
        },
        thrift::CommitRestrictedPathsChangesParams {
            ordered_params: Some(ordered.clone()),
            ..Default::default()
        },
        thrift::CommitRestrictedPathsChangesParams {
            format: Some(thrift::CommitRestrictedPathsChangesFormat::COMPACT),
            ..Default::default()
        },
    ] {
        assert!(restriction_comparison_request(params).is_err());
    }
    let params = thrift::CommitRestrictedPathsChangesParams {
        other_commit_id: Some(base),
        ordered_params: Some(ordered),
        format: Some(thrift::CommitRestrictedPathsChangesFormat::COMPACT),
        ..Default::default()
    };
    assert!(matches!(
        restriction_comparison_request(params),
        Ok(RestrictionComparisonRequest::Compact { .. })
    ));
    for limit in [0, thrift::COMMIT_RESTRICTED_PATHS_CHANGES_MAX_LIMIT + 1] {
        assert!(
            restriction_comparison_page_params(Some(
                thrift::CommitRestrictedPathsChangesOrderedParams {
                    limit,
                    ..Default::default()
                }
            ))
            .is_err()
        );
    }
    assert!(
        restriction_comparison_format(Some(thrift::CommitRestrictedPathsChangesFormat::from(9)))
            .is_err()
    );
}
