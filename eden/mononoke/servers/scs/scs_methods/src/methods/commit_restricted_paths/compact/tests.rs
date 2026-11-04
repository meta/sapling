/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::str::FromStr;

use anyhow::Result;
use futures::StreamExt;
use futures::stream;
use mononoke_api::ChangedPathRestrictionSides;
use mononoke_api::PathPermissionChangeInfo;
use mononoke_api::PathRestrictionCopySourceInfo;
use mononoke_api::SubtreeSourceMapping;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::MPath;
use permission_checker::MononokeIdentity;

use super::*;

fn unrestricted() -> PathPolicyInfo {
    PathPolicyInfo {
        restrictions: vec![],
        has_access: Some(true),
    }
}

fn restricted(acl: &str, allowed: bool) -> Result<PathPolicyInfo> {
    Ok(PathPolicyInfo {
        restrictions: vec![PathAccessInfo {
            restriction: restricted_paths::PathRestrictionInfo {
                restriction_root: NonRootMPath::new("secret")?,
                repo_region_acl: acl.into(),
                permission_request_group: MononokeIdentity::from_str(acl)?,
                rollout_allowlist_group: None,
            },
            has_access: Some(allowed),
        }],
        has_access: Some(allowed),
    })
}

fn modified(base: PathPolicyInfo, target: PathPolicyInfo) -> ChangedPathRestrictionInfo {
    ChangedPathRestrictionInfo {
        permission_change: PathPermissionChangeInfo::between(
            base.clone(),
            target.clone(),
            &[],
            None,
        ),
        sides: ChangedPathRestrictionSides::Changed { base, target },
        copy_sources: vec![],
        subtree_source: None,
    }
}

fn subtree(info: PathPolicyInfo) -> Result<SubtreeSourcePolicyInfo> {
    Ok(SubtreeSourcePolicyInfo {
        mapping: SubtreeSourceMapping {
            commit_id: ChangesetId::from_str(&"a".repeat(64))?,
            source_prefix: MPath::new("secret")?,
            destination_prefix: MPath::new("public")?,
        },
        info,
    })
}

#[mononoke::test]
fn checked_policies_are_shared_across_paths() -> Result<()> {
    let change = modified(
        restricted("REPO_REGION:a", false)?,
        restricted("REPO_REGION:b", true)?,
    );
    let results = ["secret/a", "secret/b", "secret/c"]
        .into_iter()
        .map(|path| Ok((NonRootMPath::new(path)?, change.clone())))
        .collect::<Result<Vec<_>>>()?;
    let compact = encode(&results)?;
    assert_eq!(compact.policies.len(), 2);
    assert_eq!(
        compact.restriction_roots["secret/a"],
        compact.restriction_roots["secret/b"]
    );
    let delta = &compact.permission_changes["secret/a"];
    assert!(!compact.policies[delta.base.restrictions[0] as usize].has_access);
    assert!(compact.policies[delta.target.restrictions[0] as usize].has_access);
    Ok(())
}

#[mononoke::test]
fn distinct_checked_access_is_not_coalesced() -> Result<()> {
    let a = restricted("REPO_REGION:a", false)?;
    let mut b = restricted("REPO_REGION:a", true)?;
    b.restrictions[0].restriction.rollout_allowlist_group =
        Some(MononokeIdentity::from_str("GROUP:rollout")?);
    let result = encode(&[
        (NonRootMPath::new("secret/file")?, modified(a.clone(), b)),
        (
            NonRootMPath::new("secret/other")?,
            modified(a, restricted("REPO_REGION:a", true)?),
        ),
    ])?;
    assert_eq!(result.policies.len(), 2);
    assert_eq!(
        result
            .policies
            .iter()
            .filter(|policy| policy.has_access)
            .count(),
        1
    );
    Ok(())
}

#[mononoke::test]
fn rollout_only_changes_do_not_escape_or_leave_orphan_rows() -> Result<()> {
    let target = restricted("REPO_REGION:acl", true)?;
    let mut source = target.clone();
    source.restrictions[0].restriction.rollout_allowlist_group =
        Some(MononokeIdentity::from_str("GROUP:private_rollout")?);
    let subtree = subtree(source.clone())?;
    let copy = PathRestrictionCopySourceInfo {
        commit_id: subtree.mapping.commit_id,
        path: NonRootMPath::new("secret/source")?,
        info: source.clone(),
    };
    let info = ChangedPathRestrictionInfo {
        sides: ChangedPathRestrictionSides::Added(target.clone()),
        copy_sources: vec![copy.clone()],
        subtree_source: Some(subtree.clone()),
        permission_change: PathPermissionChangeInfo::between(
            source,
            target,
            &[copy],
            Some(subtree),
        ),
    };
    assert!(
        info.permission_change.is_some(),
        "internal identity retains rollout controls"
    );
    let compact = encode(&[(NonRootMPath::new("public/file")?, info)])?;
    assert_eq!(compact.policies.len(), 1);
    assert_eq!(compact.restriction_roots["public/file"], vec![0]);
    assert!(compact.permission_changes.is_empty());
    assert!(compact.subtree_sources.is_empty());
    Ok(())
}

#[mononoke::test]
fn public_changes_keep_only_sources_that_differ_after_projection() -> Result<()> {
    let target = restricted("REPO_REGION:new", true)?;
    let mut matching_source = target.clone();
    matching_source.restrictions[0]
        .restriction
        .rollout_allowlist_group = Some(MononokeIdentity::from_str("GROUP:private_rollout")?);
    let source = subtree(matching_source.clone())?;
    let copy = PathRestrictionCopySourceInfo {
        commit_id: source.mapping.commit_id,
        path: NonRootMPath::new("secret/source")?,
        info: matching_source,
    };
    let mut info = modified(restricted("REPO_REGION:old", false)?, target.clone());
    info.copy_sources = vec![copy.clone()];
    info.subtree_source = Some(source.clone());
    info.permission_change = PathPermissionChangeInfo::between(
        restricted("REPO_REGION:old", false)?,
        target,
        &[copy],
        Some(source),
    );
    let compact = encode(&[(NonRootMPath::new("secret/file")?, info)])?;
    let change = &compact.permission_changes["secret/file"];
    assert_ne!(change.base, change.target);
    assert!(change.copy_sources.is_empty());
    assert!(change.subtree_source.is_none());
    assert!(compact.subtree_sources.is_empty());
    assert_eq!(compact.policies.len(), 2);
    Ok(())
}

#[mononoke::test]
fn ordinary_and_subtree_sources_have_distinct_encodings() -> Result<()> {
    let source_policy = restricted("REPO_REGION:source", false)?;
    let source = subtree(source_policy.clone())?;
    let file_copy = PathRestrictionCopySourceInfo {
        commit_id: source.mapping.commit_id,
        path: NonRootMPath::new("secret/individual")?,
        info: source_policy,
    };
    let target = unrestricted();
    let change = ChangedPathRestrictionInfo {
        sides: ChangedPathRestrictionSides::Added(target.clone()),
        copy_sources: vec![file_copy.clone()],
        subtree_source: Some(source.clone()),
        permission_change: PathPermissionChangeInfo::between(
            unrestricted(),
            target,
            &[file_copy],
            Some(source.clone()),
        ),
    };
    let compact = encode(&[
        (NonRootMPath::new("public/file")?, change.clone()),
        (NonRootMPath::new("public/other")?, change),
    ])?;
    assert_eq!(compact.subtree_sources.len(), 1);
    let delta = &compact.permission_changes["public/file"];
    assert_eq!(delta.copy_sources.len(), 1);
    assert_eq!(delta.copy_sources[0].path, "secret/individual");
    let mapping = &compact.subtree_sources[delta
        .subtree_source
        .as_ref()
        .expect("source edge")
        .subtree_source_index as usize];
    assert_eq!(mapping.source_prefix, "secret");
    assert_eq!(mapping.destination_prefix, "public");
    assert_eq!(
        mapping.commit_id,
        thrift::CommitId::bonsai(source.mapping.commit_id.as_ref().to_vec())
    );
    Ok(())
}

#[mononoke::test]
async fn hypothetical_source_policy_is_not_an_actual_content_requirement() -> Result<()> {
    let target = unrestricted();
    let delta = PathPermissionChangeInfo::between(
        unrestricted(),
        target.clone(),
        &[],
        Some(subtree(restricted("REPO_REGION:hypothetical", false)?)?),
    );
    let info = ChangedPathRestrictionInfo {
        sides: ChangedPathRestrictionSides::Added(target),
        copy_sources: vec![],
        subtree_source: None,
        permission_change: delta,
    };
    let response = from_stream(
        stream::iter(vec![Ok((NonRootMPath::new("public/file")?, info))]).boxed(),
        1,
    )
    .await
    .expect("valid response");
    assert_eq!(response.has_access, thrift::PathCoverage::ALL);
    assert_eq!(response.authorized_paths, vec!["public/file"]);
    let compact = response.compact.expect("compact");
    assert!(compact.restriction_roots.is_empty());
    assert_eq!(compact.policies.len(), 1);
    assert_eq!(compact.subtree_sources.len(), 1);
    Ok(())
}

#[mononoke::test]
async fn source_only_deletion_keeps_actual_source_restrictions() -> Result<()> {
    let source = subtree(restricted("REPO_REGION:source", false)?)?;
    let info = ChangedPathRestrictionInfo {
        sides: ChangedPathRestrictionSides::SourceOnly(source.clone()),
        copy_sources: vec![],
        subtree_source: None,
        permission_change: PathPermissionChangeInfo::between(
            unrestricted(),
            unrestricted(),
            &[],
            Some(source),
        ),
    };
    let response = from_stream(
        stream::iter(vec![Ok((NonRootMPath::new("public/deleted")?, info))]).boxed(),
        1,
    )
    .await
    .expect("valid response");
    assert_eq!(response.has_access, thrift::PathCoverage::NONE);
    assert!(response.authorized_paths.is_empty());
    assert_eq!(
        response.compact.expect("compact").restriction_roots.len(),
        1
    );
    Ok(())
}

#[mononoke::test]
async fn lookahead_does_not_intern_policy_or_source_rows() -> Result<()> {
    let first = modified(unrestricted(), unrestricted());
    let source = subtree(restricted("REPO_REGION:tail", false)?)?;
    let tail = ChangedPathRestrictionInfo {
        sides: ChangedPathRestrictionSides::SourceOnly(source.clone()),
        copy_sources: vec![],
        subtree_source: None,
        permission_change: PathPermissionChangeInfo::between(
            unrestricted(),
            unrestricted(),
            &[],
            Some(source),
        ),
    };
    let response = from_stream(
        stream::iter(vec![
            Ok((NonRootMPath::new("a/file")?, first)),
            Ok((NonRootMPath::new("public/tail")?, tail)),
            Err(MononokeError::InvalidRequest("beyond lookahead".into())),
        ])
        .boxed(),
        1,
    )
    .await
    .expect("valid response");
    assert_eq!(response.last_path.as_deref(), Some("a/file"));
    let compact = response.compact.expect("compact");
    assert!(compact.policies.is_empty());
    assert!(compact.subtree_sources.is_empty());
    Ok(())
}

#[mononoke::test]
async fn streaming_responses_fail_on_retained_or_lookahead_errors() -> Result<()> {
    for limit in [1, 2] {
        let first = modified(unrestricted(), unrestricted());
        let response = from_stream(
            stream::iter(vec![
                Ok((NonRootMPath::new("a/file")?, first)),
                Err(MononokeError::InvalidRequest("comparison failed".into())),
            ])
            .boxed(),
            limit,
        )
        .await;
        assert!(response.is_err());
    }
    let legacy = super::super::legacy_comparison_response(
        stream::iter(vec![
            Ok((
                NonRootMPath::new("a/file")?,
                modified(unrestricted(), unrestricted()),
            )),
            Err(MononokeError::InvalidRequest("comparison failed".into())),
        ])
        .boxed(),
    )
    .await;
    assert!(legacy.is_err());
    Ok(())
}

#[mononoke::test]
async fn streaming_responses_keep_empty_and_mixed_coverage() -> Result<()> {
    assert!(from_stream(stream::empty().boxed(), 0).await.is_err());
    let empty = from_stream(stream::empty().boxed(), 1)
        .await
        .expect("empty compact");
    assert_eq!(empty.are_restricted, thrift::PathCoverage::NONE);
    assert_eq!(empty.has_access, thrift::PathCoverage::ALL);
    assert!(empty.last_path.is_none());
    assert!(empty.compact.expect("empty envelope").policies.is_empty());
    let results = vec![
        (
            NonRootMPath::new("public/file")?,
            modified(unrestricted(), unrestricted()),
        ),
        (
            NonRootMPath::new("secret/file")?,
            modified(restricted("REPO_REGION:old", false)?, unrestricted()),
        ),
    ];
    let compact = from_stream(stream::iter(results.clone().into_iter().map(Ok)).boxed(), 2)
        .await
        .expect("compact");
    let legacy =
        super::super::legacy_comparison_response(stream::iter(results.into_iter().map(Ok)).boxed())
            .await
            .expect("legacy");
    assert_eq!(compact.are_restricted, thrift::PathCoverage::SOME);
    assert_eq!(compact.has_access, thrift::PathCoverage::SOME);
    assert_eq!(compact.authorized_paths, vec!["public/file"]);
    assert!(compact.last_path.is_none());
    assert_eq!(compact.changed_paths, legacy.changed_paths);
    assert_eq!(compact.authorized_paths, legacy.authorized_paths);
    assert_eq!(compact.are_restricted, legacy.are_restricted);
    assert_eq!(compact.has_access, legacy.has_access);
    assert_eq!(
        legacy.restriction_roots["secret/file"][0].acls,
        vec!["REPO_REGION:old"]
    );
    Ok(())
}

#[mononoke::test]
fn unchecked_or_inconsistent_internal_results_are_rejected() -> Result<()> {
    let mut info = restricted("REPO_REGION:a", false)?;
    info.has_access = Some(true);
    assert!(
        encode(&[(
            NonRootMPath::new("secret/file")?,
            modified(info, unrestricted())
        )])
        .is_err()
    );
    let mut info = restricted("REPO_REGION:a", true)?;
    info.restrictions[0].has_access = None;
    assert!(
        encode(&[(
            NonRootMPath::new("secret/file")?,
            modified(info, unrestricted())
        )])
        .is_err()
    );
    Ok(())
}
