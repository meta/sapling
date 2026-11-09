/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod comparisons;
mod test_restricted_paths_bypassing;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use changesets_creation::save_changesets;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::FutureExt;
use futures::StreamExt;
use futures::TryStreamExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use metaconfig_types::AclManifestMode;
use metaconfig_types::PathRestrictionMetadata;
use metaconfig_types::RestrictedPathsConfig;
use metaconfig_types::RestrictedPathsManifestIdStoreConfig;
use mononoke_macros::mononoke;
use mononoke_types::NonRootMPath;
use mononoke_types::RepositoryId;
use mononoke_types::SubtreeChange;
use mononoke_types::path::MPath;
use permission_checker::AclProvider;
use permission_checker::AlwaysMember;
use permission_checker::BoxMembershipChecker;
use permission_checker::BoxPermissionChecker;
use permission_checker::MononokeIdentity;
use permission_checker::MononokeIdentitySet;
use permission_checker::NeverMember;
use permission_checker::PermissionCheckerBuilder;
use permission_checker::dummy::DummyAclProvider;
use repo_derived_data::RepoDerivedDataArc;
use restricted_paths::PathRestrictionInfo;
use restricted_paths::RestrictedPaths;
use restricted_paths::RestrictedPathsConfigBased;
use restricted_paths::SqlRestrictedPathsManifestIdStoreBuilder;
use scuba_ext::MononokeScubaSampleBuilder;
use sql_construct::SqlConstruct;
use test_repo_factory::TestRepoFactory;
use tests_utils::CreateCommitContext;

use crate::changeset::ChangesetContext;
use crate::repo::Repo;
use crate::repo::RepoContext;
use crate::restricted_paths::ChangedPathRestrictionInfo;
use crate::restricted_paths::ChangedPathsRestrictionOptions;
use crate::restricted_paths::PathAccessInfo;
use crate::restricted_paths::RestrictedChangeGroup;
use crate::restricted_paths::RestrictedPathsChangesInfo;

// ---- restriction_info tests ----

#[mononoke::fbinit_test]
async fn test_restriction_info_exact_match(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) =
        create_test_changeset(fb, vec![("restricted/dir", "GROUP:my-acl")]).await?;

    let info = cs_ctx
        .path_restriction(MPath::try_from("restricted/dir")?)
        .await?
        .restriction_info(true)
        .await?;

    let expected = vec![PathAccessInfo {
        restriction: PathRestrictionInfo {
            restriction_root: NonRootMPath::new("restricted/dir")?,
            repo_region_acl: "GROUP:my-acl".to_string(),
            permission_request_group: MononokeIdentity::from_str("GROUP:my-acl")?,
            rollout_allowlist_group: None,
        },
        has_access: Some(true),
    }];
    let mut actual = info;
    actual.sort_by(|a, b| a.restriction_root().cmp(b.restriction_root()));
    pretty_assertions::assert_eq!(actual, expected);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_info_nested_path(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) =
        create_test_changeset(fb, vec![("restricted", "GROUP:my-acl")]).await?;

    let info = cs_ctx
        .path_restriction(MPath::try_from("restricted/subdir/file.txt")?)
        .await?
        .restriction_info(true)
        .await?;

    let expected = vec![PathAccessInfo {
        restriction: PathRestrictionInfo {
            restriction_root: NonRootMPath::new("restricted")?,
            repo_region_acl: "GROUP:my-acl".to_string(),
            permission_request_group: MononokeIdentity::from_str("GROUP:my-acl")?,
            rollout_allowlist_group: None,
        },
        has_access: Some(true),
    }];
    let mut actual = info;
    actual.sort_by(|a, b| a.restriction_root().cmp(b.restriction_root()));
    pretty_assertions::assert_eq!(actual, expected);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_info_no_match(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) =
        create_test_changeset(fb, vec![("restricted/dir", "GROUP:my-acl")]).await?;

    let info = cs_ctx
        .path_restriction(MPath::try_from("other/path/file.txt")?)
        .await?
        .restriction_info(true)
        .await?;

    assert!(info.is_empty(), "path should not be restricted");

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_info_sibling_path(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) = create_test_changeset(fb, vec![("foo/bar", "GROUP:my-acl")]).await?;

    // foo/baz is a sibling of foo/bar, not under it
    let info = cs_ctx
        .path_restriction(MPath::try_from("foo/baz/file.txt")?)
        .await?
        .restriction_info(true)
        .await?;

    assert!(info.is_empty(), "sibling path should not be restricted");

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_info_multiple_restrictions(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) = create_test_changeset(
        fb,
        vec![("first", "GROUP:first-acl"), ("second", "GROUP:second-acl")],
    )
    .await?;

    let first_info = cs_ctx
        .path_restriction(MPath::try_from("first/nested/file.txt")?)
        .await?
        .restriction_info(true)
        .await?;

    let expected_first = vec![PathAccessInfo {
        restriction: PathRestrictionInfo {
            restriction_root: NonRootMPath::new("first")?,
            repo_region_acl: "GROUP:first-acl".to_string(),
            permission_request_group: MononokeIdentity::from_str("GROUP:first-acl")?,
            rollout_allowlist_group: None,
        },
        has_access: Some(true),
    }];
    let mut actual_first = first_info;
    actual_first.sort_by(|a, b| a.restriction_root().cmp(b.restriction_root()));
    pretty_assertions::assert_eq!(actual_first, expected_first);

    let second_info = cs_ctx
        .path_restriction(MPath::try_from("second/nested/file.txt")?)
        .await?
        .restriction_info(true)
        .await?;

    let expected_second = vec![PathAccessInfo {
        restriction: PathRestrictionInfo {
            restriction_root: NonRootMPath::new("second")?,
            repo_region_acl: "GROUP:second-acl".to_string(),
            permission_request_group: MononokeIdentity::from_str("GROUP:second-acl")?,
            rollout_allowlist_group: None,
        },
        has_access: Some(true),
    }];
    let mut actual_second = second_info;
    actual_second.sort_by(|a, b| a.restriction_root().cmp(b.restriction_root()));
    pretty_assertions::assert_eq!(actual_second, expected_second);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_info_root_path(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) =
        create_test_changeset(fb, vec![("restricted", "GROUP:my-acl")]).await?;

    // Root path cannot be restricted
    let info = cs_ctx
        .path_restriction(MPath::ROOT)
        .await?
        .restriction_info(true)
        .await?;

    assert!(info.is_empty(), "root path cannot be restricted");

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_info_no_restrictions_configured(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) = create_test_changeset(fb, vec![]).await?;

    let info = cs_ctx
        .path_restriction(MPath::try_from("any/path/file.txt")?)
        .await?
        .restriction_info(true)
        .await?;

    assert!(info.is_empty(), "no restrictions configured");

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restriction_info_nested_roots(fb: FacebookInit) -> Result<()> {
    // A path can fall under multiple nested tent roots.
    // e.g. foo/ has acl_a and foo/bar/ has acl_b, querying foo/bar/file.txt
    // should return both roots. See comment in D93601751.
    let (_repo_ctx, cs_ctx) = create_test_changeset(
        fb,
        vec![("foo", "GROUP:outer-acl"), ("foo/bar", "GROUP:inner-acl")],
    )
    .await?;

    let infos = cs_ctx
        .path_restriction(MPath::try_from("foo/bar/file.txt")?)
        .await?
        .restriction_info(true)
        .await?;

    let expected = vec![
        PathAccessInfo {
            restriction: PathRestrictionInfo {
                restriction_root: NonRootMPath::new("foo")?,
                repo_region_acl: "GROUP:outer-acl".to_string(),
                permission_request_group: MononokeIdentity::from_str("GROUP:outer-acl")?,
                rollout_allowlist_group: None,
            },
            has_access: Some(true),
        },
        PathAccessInfo {
            restriction: PathRestrictionInfo {
                restriction_root: NonRootMPath::new("foo/bar")?,
                repo_region_acl: "GROUP:inner-acl".to_string(),
                permission_request_group: MononokeIdentity::from_str("GROUP:inner-acl")?,
                rollout_allowlist_group: None,
            },
            has_access: Some(true),
        },
    ];
    let mut actual = infos;
    actual.sort_by(|a, b| a.restriction_root().cmp(b.restriction_root()));
    pretty_assertions::assert_eq!(actual, expected);

    Ok(())
}

// ---- paths_restriction_info batch tests ----

#[mononoke::fbinit_test]
async fn test_batch_restriction_info_mixed_paths(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) = create_test_changeset(
        fb,
        vec![
            ("first", "GROUP:first-acl"),
            ("second", "GROUP:second-acl"),
            ("third/nested", "GROUP:third-acl"),
        ],
    )
    .await?;

    let paths = vec![
        NonRootMPath::new("first/file1.txt")?,
        NonRootMPath::new("second/subdir/file2.txt")?,
        NonRootMPath::new("third/nested/deep/file3.txt")?,
        NonRootMPath::new("unrestricted/file4.txt")?,
    ];

    let results = cs_ctx.paths_restriction_info(paths, true).await?;

    // Build comparable (path, Option<(restriction_root, repo_region_acl)>) tuples
    // Using first() since these paths each have at most one matching root
    let mut actual: Vec<(String, Option<(String, String)>)> = results
        .into_iter()
        .map(|(path, infos)| {
            (
                path.to_string(),
                infos.first().map(|i| {
                    (
                        i.restriction_root().to_string(),
                        i.repo_region_acl().to_string(),
                    )
                }),
            )
        })
        .collect();
    actual.sort_by(|a, b| a.0.cmp(&b.0));

    let expected: Vec<(String, Option<(String, String)>)> = vec![
        (
            "first/file1.txt".to_string(),
            Some(("first".to_string(), "GROUP:first-acl".to_string())),
        ),
        (
            "second/subdir/file2.txt".to_string(),
            Some(("second".to_string(), "GROUP:second-acl".to_string())),
        ),
        (
            "third/nested/deep/file3.txt".to_string(),
            Some(("third/nested".to_string(), "GROUP:third-acl".to_string())),
        ),
        ("unrestricted/file4.txt".to_string(), None),
    ];

    pretty_assertions::assert_eq!(actual, expected);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_batch_all_restricted(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) =
        create_test_changeset(fb, vec![("restricted", "GROUP:my-acl")]).await?;

    let paths = vec![
        NonRootMPath::new("restricted/file1.txt")?,
        NonRootMPath::new("restricted/file2.txt")?,
        NonRootMPath::new("restricted/subdir/file3.txt")?,
    ];

    let results = cs_ctx.paths_restriction_info(paths, true).await?;

    let mut actual: Vec<(String, Option<(String, String)>)> = results
        .into_iter()
        .map(|(path, infos)| {
            (
                path.to_string(),
                infos.first().map(|i| {
                    (
                        i.restriction_root().to_string(),
                        i.repo_region_acl().to_string(),
                    )
                }),
            )
        })
        .collect();
    actual.sort_by(|a, b| a.0.cmp(&b.0));

    let expected: Vec<(String, Option<(String, String)>)> = vec![
        (
            "restricted/file1.txt".to_string(),
            Some(("restricted".to_string(), "GROUP:my-acl".to_string())),
        ),
        (
            "restricted/file2.txt".to_string(),
            Some(("restricted".to_string(), "GROUP:my-acl".to_string())),
        ),
        (
            "restricted/subdir/file3.txt".to_string(),
            Some(("restricted".to_string(), "GROUP:my-acl".to_string())),
        ),
    ];

    pretty_assertions::assert_eq!(actual, expected);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_batch_all_unrestricted(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) =
        create_test_changeset(fb, vec![("restricted", "GROUP:my-acl")]).await?;

    let paths = vec![
        NonRootMPath::new("unrestricted/file1.txt")?,
        NonRootMPath::new("other/file2.txt")?,
    ];

    let results = cs_ctx.paths_restriction_info(paths, true).await?;

    let mut actual: Vec<(String, Option<(String, String)>)> = results
        .into_iter()
        .map(|(path, infos)| {
            (
                path.to_string(),
                infos.first().map(|i| {
                    (
                        i.restriction_root().to_string(),
                        i.repo_region_acl().to_string(),
                    )
                }),
            )
        })
        .collect();
    actual.sort_by(|a, b| a.0.cmp(&b.0));

    let expected: Vec<(String, Option<(String, String)>)> = vec![
        ("other/file2.txt".to_string(), None),
        ("unrestricted/file1.txt".to_string(), None),
    ];

    pretty_assertions::assert_eq!(actual, expected);

    Ok(())
}

// What it tests: Both-mode path metadata should include config-only
// restrictions after metadata results are unioned across sources.
// Expected: config-only restrictions are returned.
#[mononoke::fbinit_test]
async fn test_batch_restriction_info_both_mode_config_only_root(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) = create_both_mode_test_changeset(
        fb,
        vec![("config_only", "GROUP:config-acl")],
        vec![],
        vec!["config_only/file.txt"],
    )
    .await?;

    let results = cs_ctx
        .paths_restriction_info(vec![NonRootMPath::new("config_only/file.txt")?], false)
        .await?;

    let infos = results
        .first()
        .map(|(_, infos)| infos.as_slice())
        .unwrap_or_default();
    let info = infos
        .first()
        .ok_or_else(|| anyhow::anyhow!("expected config-only restriction"))?;
    assert_eq!(info.restriction_root(), &NonRootMPath::new("config_only")?);
    assert_eq!(info.repo_region_acl(), "GROUP:config-acl");

    Ok(())
}

// What it tests: Both-mode path metadata should include AclManifest-only
// restrictions.
// Expected: AclManifest-only restrictions are returned.
#[mononoke::fbinit_test]
async fn test_batch_restriction_info_both_mode_acl_manifest_only_root(
    fb: FacebookInit,
) -> Result<()> {
    let (_repo_ctx, cs_ctx) = create_both_mode_test_changeset(
        fb,
        vec![],
        vec![("acl_manifest_only", "REPO_REGION:acl_manifest_acl")],
        vec!["acl_manifest_only/file.txt"],
    )
    .await?;

    let results = cs_ctx
        .paths_restriction_info(
            vec![NonRootMPath::new("acl_manifest_only/file.txt")?],
            false,
        )
        .await?;

    let infos = results
        .first()
        .map(|(_, infos)| infos.as_slice())
        .unwrap_or_default();
    let info = infos
        .first()
        .ok_or_else(|| anyhow::anyhow!("expected AclManifest-only restriction"))?;
    assert_eq!(
        info.restriction_root(),
        &NonRootMPath::new("acl_manifest_only")?
    );
    assert_eq!(info.repo_region_acl(), "REPO_REGION:acl_manifest_acl");

    Ok(())
}

// What it tests: Both-mode path metadata should prefer AclManifest on
// same-root disagreement.
// Expected: the AclManifest ACL is returned for the shared root.
#[mononoke::fbinit_test]
async fn test_batch_restriction_info_both_mode_same_root_prefers_acl_manifest(
    fb: FacebookInit,
) -> Result<()> {
    let (_repo_ctx, cs_ctx) = create_both_mode_test_changeset(
        fb,
        vec![("shared", "REPO_REGION:config_acl")],
        vec![("shared", "REPO_REGION:acl_manifest_acl")],
        vec!["shared/file.txt"],
    )
    .await?;

    let results = cs_ctx
        .paths_restriction_info(vec![NonRootMPath::new("shared/file.txt")?], false)
        .await?;

    let infos = results
        .first()
        .map(|(_, infos)| infos.as_slice())
        .unwrap_or_default();
    let info = infos
        .first()
        .ok_or_else(|| anyhow::anyhow!("expected same-root restriction"))?;
    assert_eq!(info.restriction_root(), &NonRootMPath::new("shared")?);
    assert_eq!(info.repo_region_acl(), "REPO_REGION:acl_manifest_acl");

    Ok(())
}

/// What it tests: permission-aware path lookup in Both mode when config and
/// AclManifest disagree on the ACL for the same root.
/// Expected: one AclManifest metadata row is returned and its allowed ACL
/// is reported, while authorization requires both source ACLs and is denied.
#[mononoke::fbinit_test]
async fn test_restriction_info_both_mode_same_root_requires_both_acls(
    fb: FacebookInit,
) -> Result<()> {
    let (_repo_ctx, cs_ctx) = create_permission_aware_both_mode_test_changeset(
        fb,
        vec![("shared", "REPO_REGION:restricted_acl")],
        vec![("shared", "REPO_REGION:myusername_project")],
        vec!["shared/file.txt"],
    )
    .await?;

    let infos = cs_ctx
        .path_restriction(MPath::try_from("shared/file.txt")?)
        .await?
        .restriction_info(true)
        .await?;

    let info = infos
        .first()
        .ok_or_else(|| anyhow::anyhow!("expected one same-root restriction"))?;
    assert_eq!(infos.len(), 1, "same-root metadata should produce one row");
    assert_eq!(info.restriction_root(), &NonRootMPath::new("shared")?);
    assert_eq!(info.repo_region_acl(), "REPO_REGION:myusername_project");
    // FIXME(T291525172): The current metadata lookup applies AclManifest
    // precedence, so this temporarily asserts today's access grant. Invert the
    // assertion when Both-mode reconciliation is used.
    assert_eq!(info.has_access, Some(true));

    Ok(())
}

/// What it tests: permission-aware path lookup in Both mode when only config
/// defines a rollout allowlist for a same-root restriction.
/// Expected: config's successful rollout decision fills AclManifest's absent
/// rollout policy and the single metadata row reports access.
#[mononoke::fbinit_test]
async fn test_restriction_info_both_mode_uses_config_rollout_fallback(
    fb: FacebookInit,
) -> Result<()> {
    let (_repo_ctx, cs_ctx) = create_permission_aware_both_mode_rollout_test_changeset(
        fb,
        "shared",
        "REPO_REGION:restricted_acl",
        "GROUP:rollout",
    )
    .await?;

    let infos = cs_ctx
        .path_restriction(MPath::try_from("shared/file.txt")?)
        .await?
        .restriction_info(true)
        .await?;
    let info = infos
        .first()
        .ok_or_else(|| anyhow::anyhow!("expected one same-root restriction"))?;

    assert_eq!(infos.len(), 1, "same-root metadata should produce one row");
    // FIXME(T291525172): The current metadata lookup treats AclManifest's
    // missing rollout group as denial, so this temporarily asserts today's
    // denial. Invert the assertion with reconciliation.
    assert_eq!(info.has_access, Some(false));
    Ok(())
}

/// What it tests: permission-aware batch lookup in Both mode when config and
/// AclManifest disagree on the ACL for the same root.
/// Expected: the path maps to one AclManifest metadata row without access,
/// because authorization requires both source ACLs.
#[mononoke::fbinit_test]
async fn test_batch_restriction_info_both_mode_same_root_requires_both_acls(
    fb: FacebookInit,
) -> Result<()> {
    let (_repo_ctx, cs_ctx) = create_permission_aware_both_mode_test_changeset(
        fb,
        vec![("shared", "REPO_REGION:restricted_acl")],
        vec![("shared", "REPO_REGION:myusername_project")],
        vec!["shared/file.txt"],
    )
    .await?;
    let path = NonRootMPath::new("shared/file.txt")?;

    let results = cs_ctx
        .paths_restriction_info(vec![path.clone()], true)
        .await?;

    let (actual_path, infos) = results
        .first()
        .ok_or_else(|| anyhow::anyhow!("expected one batch path result"))?;
    let info = infos
        .first()
        .ok_or_else(|| anyhow::anyhow!("expected one same-root restriction"))?;
    assert_eq!(results.len(), 1, "batch should contain one path result");
    assert_eq!(actual_path, &path);
    assert_eq!(infos.len(), 1, "same-root metadata should produce one row");
    assert_eq!(info.restriction_root(), &NonRootMPath::new("shared")?);
    assert_eq!(info.repo_region_acl(), "REPO_REGION:myusername_project");
    // FIXME(T291525172): The current metadata lookup applies AclManifest
    // precedence, so this temporarily asserts today's access grant. Invert the
    // assertion when Both-mode reconciliation is used.
    assert_eq!(info.has_access, Some(true));

    Ok(())
}

// ---- find_restricted_descendants tests ----

/// What it tests: permission-aware descendant lookup in Both mode when config
/// and AclManifest disagree on the ACL for the same root.
/// Expected: one AclManifest metadata row is returned without access, because
/// authorization requires both source ACLs.
#[mononoke::fbinit_test]
async fn test_find_descendants_both_mode_same_root_requires_both_acls(
    fb: FacebookInit,
) -> Result<()> {
    let (_repo_ctx, cs_ctx) = create_permission_aware_both_mode_test_changeset(
        fb,
        vec![("shared", "REPO_REGION:restricted_acl")],
        vec![("shared", "REPO_REGION:myusername_project")],
        vec!["shared/file.txt"],
    )
    .await?;

    let descendants = cs_ctx
        .path_restriction(MPath::ROOT)
        .await?
        .find_restricted_descendants(true)
        .await?;

    let info = descendants
        .first()
        .ok_or_else(|| anyhow::anyhow!("expected one same-root descendant"))?;
    assert_eq!(
        descendants.len(),
        1,
        "same-root metadata should produce one descendant row"
    );
    assert_eq!(info.restriction_root(), &NonRootMPath::new("shared")?);
    assert_eq!(info.repo_region_acl(), "REPO_REGION:myusername_project");
    // FIXME(T291525172): The current metadata lookup applies AclManifest
    // precedence, so this temporarily asserts today's access grant. Invert the
    // assertion when Both-mode reconciliation is used.
    assert_eq!(info.has_access, Some(true));

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_find_descendants_no_restrictions(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) = create_test_changeset(fb, vec![]).await?;

    let descendants = cs_ctx
        .path_restriction(MPath::ROOT)
        .await?
        .find_restricted_descendants(false)
        .await?;

    assert!(descendants.is_empty());

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_find_descendants_root_returns_all(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) = create_test_changeset(
        fb,
        vec![
            ("first/path", "GROUP:first-acl"),
            ("second/path", "GROUP:second-acl"),
        ],
    )
    .await?;

    let descendants = cs_ctx
        .path_restriction(MPath::ROOT)
        .await?
        .find_restricted_descendants(false)
        .await?;

    let expected = vec![
        PathAccessInfo {
            restriction: PathRestrictionInfo {
                restriction_root: NonRootMPath::new("first/path")?,
                repo_region_acl: "GROUP:first-acl".to_string(),
                permission_request_group: MononokeIdentity::from_str("GROUP:first-acl")?,
                rollout_allowlist_group: None,
            },
            has_access: None,
        },
        PathAccessInfo {
            restriction: PathRestrictionInfo {
                restriction_root: NonRootMPath::new("second/path")?,
                repo_region_acl: "GROUP:second-acl".to_string(),
                permission_request_group: MononokeIdentity::from_str("GROUP:second-acl")?,
                rollout_allowlist_group: None,
            },
            has_access: None,
        },
    ];
    let mut actual = descendants;
    actual.sort_by(|a, b| a.restriction_root().cmp(b.restriction_root()));
    pretty_assertions::assert_eq!(actual, expected);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_find_descendants_check_permissions_populates_access(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) =
        create_test_changeset(fb, vec![("restricted/path", "GROUP:restricted-acl")]).await?;

    let descendants = cs_ctx
        .path_restriction(MPath::ROOT)
        .await?
        .find_restricted_descendants(true)
        .await?;

    let expected = vec![PathAccessInfo {
        restriction: PathRestrictionInfo {
            restriction_root: NonRootMPath::new("restricted/path")?,
            repo_region_acl: "GROUP:restricted-acl".to_string(),
            permission_request_group: MononokeIdentity::from_str("GROUP:restricted-acl")?,
            rollout_allowlist_group: None,
        },
        has_access: Some(true),
    }];
    let mut actual = descendants;
    actual.sort_by(|a, b| a.restriction_root().cmp(b.restriction_root()));
    pretty_assertions::assert_eq!(actual, expected);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_find_descendants_filter_exact_match(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) = create_test_changeset(
        fb,
        vec![
            ("first/path", "GROUP:first-acl"),
            ("second/path", "GROUP:second-acl"),
        ],
    )
    .await?;

    let descendants = cs_ctx
        .path_restriction(MPath::try_from("first/path")?)
        .await?
        .find_restricted_descendants(false)
        .await?;

    // "first/path" is itself a restriction root, and is_prefix_of returns
    // true for equal paths, so it should be returned
    let expected = vec![PathAccessInfo {
        restriction: PathRestrictionInfo {
            restriction_root: NonRootMPath::new("first/path")?,
            repo_region_acl: "GROUP:first-acl".to_string(),
            permission_request_group: MononokeIdentity::from_str("GROUP:first-acl")?,
            rollout_allowlist_group: None,
        },
        has_access: None,
    }];
    let mut actual = descendants;
    actual.sort_by(|a, b| a.restriction_root().cmp(b.restriction_root()));
    pretty_assertions::assert_eq!(actual, expected);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_find_descendants_filter_parent_of_root(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) =
        create_test_changeset(fb, vec![("foo/bar/restricted", "GROUP:my-acl")]).await?;

    let descendants = cs_ctx
        .path_restriction(MPath::try_from("foo")?)
        .await?
        .find_restricted_descendants(false)
        .await?;

    let expected = vec![PathAccessInfo {
        restriction: PathRestrictionInfo {
            restriction_root: NonRootMPath::new("foo/bar/restricted")?,
            repo_region_acl: "GROUP:my-acl".to_string(),
            permission_request_group: MononokeIdentity::from_str("GROUP:my-acl")?,
            rollout_allowlist_group: None,
        },
        has_access: None,
    }];
    let mut actual = descendants;
    actual.sort_by(|a, b| a.restriction_root().cmp(b.restriction_root()));
    pretty_assertions::assert_eq!(actual, expected);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_find_descendants_filter_child_of_root_returns_empty(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) = create_test_changeset(fb, vec![("foo/bar", "GROUP:my-acl")]).await?;

    // Filter is a child of the root — should NOT match because we only
    // return roots that are under the filter
    let descendants = cs_ctx
        .path_restriction(MPath::try_from("foo/bar/baz/deep")?)
        .await?
        .find_restricted_descendants(false)
        .await?;

    assert!(descendants.is_empty());

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_find_descendants_filter_no_match(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) = create_test_changeset(
        fb,
        vec![
            ("first/path", "GROUP:first-acl"),
            ("second/path", "GROUP:second-acl"),
        ],
    )
    .await?;

    let descendants = cs_ctx
        .path_restriction(MPath::try_from("third/path")?)
        .await?
        .find_restricted_descendants(false)
        .await?;

    assert!(descendants.is_empty());

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_find_descendants_filter_sibling_no_match(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) = create_test_changeset(fb, vec![("foo/bar", "GROUP:my-acl")]).await?;

    let descendants = cs_ctx
        .path_restriction(MPath::try_from("foo/baz")?)
        .await?
        .find_restricted_descendants(false)
        .await?;

    assert!(descendants.is_empty());

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_find_descendants_nested_roots(fb: FacebookInit) -> Result<()> {
    // Nested restriction roots: foo/ and foo/bar/
    let (_repo_ctx, cs_ctx) = create_test_changeset(
        fb,
        vec![("foo", "GROUP:outer-acl"), ("foo/bar", "GROUP:inner-acl")],
    )
    .await?;

    // Querying from foo/ should return both the outer and inner roots
    let descendants = cs_ctx
        .path_restriction(MPath::try_from("foo")?)
        .await?
        .find_restricted_descendants(false)
        .await?;

    let expected = vec![
        PathAccessInfo {
            restriction: PathRestrictionInfo {
                restriction_root: NonRootMPath::new("foo")?,
                repo_region_acl: "GROUP:outer-acl".to_string(),
                permission_request_group: MononokeIdentity::from_str("GROUP:outer-acl")?,
                rollout_allowlist_group: None,
            },
            has_access: None,
        },
        PathAccessInfo {
            restriction: PathRestrictionInfo {
                restriction_root: NonRootMPath::new("foo/bar")?,
                repo_region_acl: "GROUP:inner-acl".to_string(),
                permission_request_group: MononokeIdentity::from_str("GROUP:inner-acl")?,
                rollout_allowlist_group: None,
            },
            has_access: None,
        },
    ];
    let mut actual = descendants;
    actual.sort_by(|a, b| a.restriction_root().cmp(b.restriction_root()));
    pretty_assertions::assert_eq!(actual, expected);

    Ok(())
}

// ---- batch find_restricted_descendants tests ----

#[mononoke::fbinit_test]
async fn test_batch_find_descendants_multiple_roots(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) = create_test_changeset(
        fb,
        vec![
            ("first/path", "GROUP:first-acl"),
            ("second/path", "GROUP:second-acl"),
            ("third/path", "GROUP:third-acl"),
        ],
    )
    .await?;

    let roots = vec![
        MPath::try_from("first/path")?,
        MPath::try_from("third/path")?,
    ];

    let descendants = cs_ctx.find_restricted_descendants(roots, false).await?;

    let expected = vec![
        PathAccessInfo {
            restriction: PathRestrictionInfo {
                restriction_root: NonRootMPath::new("first/path")?,
                repo_region_acl: "GROUP:first-acl".to_string(),
                permission_request_group: MononokeIdentity::from_str("GROUP:first-acl")?,
                rollout_allowlist_group: None,
            },
            has_access: None,
        },
        PathAccessInfo {
            restriction: PathRestrictionInfo {
                restriction_root: NonRootMPath::new("third/path")?,
                repo_region_acl: "GROUP:third-acl".to_string(),
                permission_request_group: MononokeIdentity::from_str("GROUP:third-acl")?,
                rollout_allowlist_group: None,
            },
            has_access: None,
        },
    ];
    let mut actual = descendants;
    actual.sort_by(|a, b| a.restriction_root().cmp(b.restriction_root()));
    pretty_assertions::assert_eq!(actual, expected);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_batch_find_descendants_deduplicates(fb: FacebookInit) -> Result<()> {
    let (_repo_ctx, cs_ctx) =
        create_test_changeset(fb, vec![("shared/path", "GROUP:my-acl")]).await?;

    // Both roots are parents of the same restriction root
    let roots = vec![MPath::try_from("shared")?, MPath::ROOT];

    let descendants = cs_ctx.find_restricted_descendants(roots, false).await?;

    // Should be deduplicated to just one entry
    let expected = vec![PathAccessInfo {
        restriction: PathRestrictionInfo {
            restriction_root: NonRootMPath::new("shared/path")?,
            repo_region_acl: "GROUP:my-acl".to_string(),
            permission_request_group: MononokeIdentity::from_str("GROUP:my-acl")?,
            rollout_allowlist_group: None,
        },
        has_access: None,
    }];
    let mut actual = descendants;
    actual.sort_by(|a, b| a.restriction_root().cmp(b.restriction_root()));
    pretty_assertions::assert_eq!(actual, expected);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_batch_find_descendants_nested_roots_and_queries(fb: FacebookInit) -> Result<()> {
    // Nested restriction roots
    let (_repo_ctx, cs_ctx) = create_test_changeset(
        fb,
        vec![
            ("foo", "GROUP:outer-acl"),
            ("foo/bar", "GROUP:inner-acl"),
            ("baz", "GROUP:baz-acl"),
        ],
    )
    .await?;

    // Nested query paths: foo/ is a parent of foo/bar/.
    // foo/ should find both foo/ and foo/bar/.
    // foo/bar/ should find just foo/bar/ (already found by foo/).
    // After dedup: foo/ and foo/bar/ (baz/ not matched by either query).
    let roots = vec![MPath::try_from("foo")?, MPath::try_from("foo/bar")?];

    let descendants = cs_ctx.find_restricted_descendants(roots, false).await?;

    let expected = vec![
        PathAccessInfo {
            restriction: PathRestrictionInfo {
                restriction_root: NonRootMPath::new("foo")?,
                repo_region_acl: "GROUP:outer-acl".to_string(),
                permission_request_group: MononokeIdentity::from_str("GROUP:outer-acl")?,
                rollout_allowlist_group: None,
            },
            has_access: None,
        },
        PathAccessInfo {
            restriction: PathRestrictionInfo {
                restriction_root: NonRootMPath::new("foo/bar")?,
                repo_region_acl: "GROUP:inner-acl".to_string(),
                permission_request_group: MononokeIdentity::from_str("GROUP:inner-acl")?,
                rollout_allowlist_group: None,
            },
            has_access: None,
        },
    ];
    let mut actual = descendants;
    actual.sort_by(|a, b| a.restriction_root().cmp(b.restriction_root()));
    pretty_assertions::assert_eq!(actual, expected);

    Ok(())
}

// ---- restricted_paths_changes tests ----

#[mononoke::fbinit_test]
async fn test_restricted_paths_changes_with_restricted_files(fb: FacebookInit) -> Result<()> {
    let restricted_paths =
        create_test_restricted_paths(fb, vec![("restricted", "GROUP:my-acl")]).await?;
    let ctx = CoreContext::test_mock(fb);

    let repo: Repo = TestRepoFactory::new(fb)?
        .with_restricted_paths(restricted_paths)
        .build()
        .await?;

    let root_cs_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("restricted/file.txt", "secret")
        .add_file("public/file.txt", "public")
        .commit()
        .await?;

    let repo_ctx = RepoContext::new_test(ctx.clone(), Arc::new(repo)).await?;
    let cs_ctx = ChangesetContext::new(repo_ctx, root_cs_id);

    let changes = cs_ctx.restricted_paths_changes(false).await?;

    let expected = RestrictedPathsChangesInfo {
        restricted_changes: vec![RestrictedChangeGroup {
            restriction_info: PathAccessInfo {
                restriction: PathRestrictionInfo {
                    restriction_root: NonRootMPath::new("restricted")?,
                    repo_region_acl: "GROUP:my-acl".to_string(),
                    permission_request_group: MononokeIdentity::from_str("GROUP:my-acl")?,
                    rollout_allowlist_group: None,
                },
                has_access: None,
            },
            changed_paths: vec![NonRootMPath::new("restricted/file.txt")?],
        }],
    };
    pretty_assertions::assert_eq!(changes, expected);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restricted_paths_changes_no_restricted_files(fb: FacebookInit) -> Result<()> {
    let restricted_paths =
        create_test_restricted_paths(fb, vec![("restricted", "GROUP:my-acl")]).await?;
    let ctx = CoreContext::test_mock(fb);

    let repo: Repo = TestRepoFactory::new(fb)?
        .with_restricted_paths(restricted_paths)
        .build()
        .await?;

    let root_cs_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("public/file.txt", "public")
        .commit()
        .await?;

    let repo_ctx = RepoContext::new_test(ctx.clone(), Arc::new(repo)).await?;
    let cs_ctx = ChangesetContext::new(repo_ctx, root_cs_id);

    let changes = cs_ctx.restricted_paths_changes(false).await?;

    assert!(changes.restricted_changes.is_empty());

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restricted_paths_changes_nested_roots(fb: FacebookInit) -> Result<()> {
    // Use nested roots: "first" contains "first/second"
    // A file under first/second/ should appear in both groups.
    let restricted_paths = create_test_restricted_paths(
        fb,
        vec![
            ("first", "GROUP:first-acl"),
            ("first/second", "GROUP:second-acl"),
        ],
    )
    .await?;
    let ctx = CoreContext::test_mock(fb);

    let repo: Repo = TestRepoFactory::new(fb)?
        .with_restricted_paths(restricted_paths)
        .build()
        .await?;

    let root_cs_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("first/a.txt", "a")
        .add_file("first/b.txt", "b")
        .add_file("first/second/c.txt", "c")
        .add_file("public/d.txt", "d")
        .commit()
        .await?;

    let repo_ctx = RepoContext::new_test(ctx.clone(), Arc::new(repo)).await?;
    let cs_ctx = ChangesetContext::new(repo_ctx, root_cs_id);

    let changes = cs_ctx.restricted_paths_changes(false).await?;

    // Groups are sorted by restriction_root (BTreeMap ordering)
    // First group has 3 changed paths (a.txt, b.txt, second/c.txt)
    // because first/second/c.txt is also under first/
    // Second group has 1 changed path (first/second/c.txt)
    let expected = RestrictedPathsChangesInfo {
        restricted_changes: vec![
            RestrictedChangeGroup {
                restriction_info: PathAccessInfo {
                    restriction: PathRestrictionInfo {
                        restriction_root: NonRootMPath::new("first")?,
                        repo_region_acl: "GROUP:first-acl".to_string(),
                        permission_request_group: MononokeIdentity::from_str("GROUP:first-acl")?,
                        rollout_allowlist_group: None,
                    },
                    has_access: None,
                },
                changed_paths: vec![
                    NonRootMPath::new("first/a.txt")?,
                    NonRootMPath::new("first/b.txt")?,
                    NonRootMPath::new("first/second/c.txt")?,
                ],
            },
            RestrictedChangeGroup {
                restriction_info: PathAccessInfo {
                    restriction: PathRestrictionInfo {
                        restriction_root: NonRootMPath::new("first/second")?,
                        repo_region_acl: "GROUP:second-acl".to_string(),
                        permission_request_group: MononokeIdentity::from_str("GROUP:second-acl")?,
                        rollout_allowlist_group: None,
                    },
                    has_access: None,
                },
                changed_paths: vec![NonRootMPath::new("first/second/c.txt")?],
            },
        ],
    };
    pretty_assertions::assert_eq!(changes, expected);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restricted_paths_changes_multiple_unrelated_roots(fb: FacebookInit) -> Result<()> {
    // Two independent restriction roots with no nesting relationship
    let restricted_paths = create_test_restricted_paths(
        fb,
        vec![("alpha", "GROUP:alpha-acl"), ("beta", "GROUP:beta-acl")],
    )
    .await?;
    let ctx = CoreContext::test_mock(fb);

    let repo: Repo = TestRepoFactory::new(fb)?
        .with_restricted_paths(restricted_paths)
        .build()
        .await?;

    let root_cs_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("alpha/file1.txt", "a1")
        .add_file("alpha/file2.txt", "a2")
        .add_file("beta/file3.txt", "b1")
        .add_file("public/file4.txt", "p1")
        .commit()
        .await?;

    let repo_ctx = RepoContext::new_test(ctx.clone(), Arc::new(repo)).await?;
    let cs_ctx = ChangesetContext::new(repo_ctx, root_cs_id);

    let changes = cs_ctx.restricted_paths_changes(false).await?;

    // Groups are sorted by restriction_root (BTreeMap ordering):
    // alpha/ has 2 changed paths, beta/ has 1 changed path.
    // public/file4.txt is unrestricted and should not appear.
    let expected = RestrictedPathsChangesInfo {
        restricted_changes: vec![
            RestrictedChangeGroup {
                restriction_info: PathAccessInfo {
                    restriction: PathRestrictionInfo {
                        restriction_root: NonRootMPath::new("alpha")?,
                        repo_region_acl: "GROUP:alpha-acl".to_string(),
                        permission_request_group: MononokeIdentity::from_str("GROUP:alpha-acl")?,
                        rollout_allowlist_group: None,
                    },
                    has_access: None,
                },
                changed_paths: vec![
                    NonRootMPath::new("alpha/file1.txt")?,
                    NonRootMPath::new("alpha/file2.txt")?,
                ],
            },
            RestrictedChangeGroup {
                restriction_info: PathAccessInfo {
                    restriction: PathRestrictionInfo {
                        restriction_root: NonRootMPath::new("beta")?,
                        repo_region_acl: "GROUP:beta-acl".to_string(),
                        permission_request_group: MononokeIdentity::from_str("GROUP:beta-acl")?,
                        rollout_allowlist_group: None,
                    },
                    has_access: None,
                },
                changed_paths: vec![NonRootMPath::new("beta/file3.txt")?],
            },
        ],
    };
    pretty_assertions::assert_eq!(changes, expected);

    Ok(())
}

// ---- helpers ----

async fn create_test_restricted_paths(
    fb: FacebookInit,
    path_acls: Vec<(&str, &str)>,
) -> Result<Arc<RestrictedPaths>> {
    create_test_restricted_paths_with_mode(fb, path_acls, AclManifestMode::Disabled).await
}

async fn create_test_restricted_paths_with_mode(
    fb: FacebookInit,
    path_acls: Vec<(&str, &str)>,
    acl_manifest_mode: AclManifestMode,
) -> Result<Arc<RestrictedPaths>> {
    let repo_id = RepositoryId::new(0);
    let path_restriction_metadata = build_path_restriction_metadata(path_acls)?;

    let config = RestrictedPathsConfig {
        path_restriction_metadata,
        manifest_id_store_config: RestrictedPathsManifestIdStoreConfig {
            use_manifest_id_cache: false,
            cache_update_interval_ms: 100,
            ..Default::default()
        },
        acl_manifest_mode,
        ..Default::default()
    };

    let manifest_id_store = Arc::new(
        SqlRestrictedPathsManifestIdStoreBuilder::with_sqlite_in_memory()?.with_repo_id(repo_id),
    );

    let acl_provider = DummyAclProvider::new(fb)?;
    let scuba = MononokeScubaSampleBuilder::with_discard();

    let config_based = Arc::new(RestrictedPathsConfigBased::new(
        config,
        manifest_id_store,
        None,
    ));

    // Build a minimal repo to get ArcRepoDerivedData
    let test_repo: Repo = TestRepoFactory::new(fb)?.build().await?;
    let repo_derived_data = test_repo.repo_derived_data_arc();

    Ok(Arc::new(RestrictedPaths::new(
        "repo".to_string(),
        config_based,
        acl_provider,
        scuba,
        repo_derived_data,
    )?))
}

async fn create_both_mode_test_changeset(
    fb: FacebookInit,
    config_path_acls: Vec<(&str, &str)>,
    acl_manifest_path_acls: Vec<(&str, &str)>,
    file_paths: Vec<&str>,
) -> Result<(RepoContext<Repo>, ChangesetContext<Repo>)> {
    create_both_mode_test_changeset_with_acl_provider(
        fb,
        config_path_acls,
        acl_manifest_path_acls,
        file_paths,
        DummyAclProvider::new(fb)?,
    )
    .await
}

async fn create_permission_aware_both_mode_test_changeset(
    fb: FacebookInit,
    config_path_acls: Vec<(&str, &str)>,
    acl_manifest_path_acls: Vec<(&str, &str)>,
    file_paths: Vec<&str>,
) -> Result<(RepoContext<Repo>, ChangesetContext<Repo>)> {
    create_both_mode_test_changeset_with_acl_provider(
        fb,
        config_path_acls,
        acl_manifest_path_acls,
        file_paths,
        Arc::new(TestAclProvider),
    )
    .await
}

async fn create_permission_aware_both_mode_rollout_test_changeset(
    fb: FacebookInit,
    root: &str,
    acl: &str,
    rollout_group: &str,
) -> Result<(RepoContext<Repo>, ChangesetContext<Repo>)> {
    let mut config_metadata = build_path_restriction_metadata(vec![(root, acl)])?;
    let metadata = config_metadata
        .get_mut(&NonRootMPath::new(root)?)
        .ok_or_else(|| anyhow::anyhow!("expected config metadata for {root}"))?;
    metadata.rollout_allowlist_group = Some(MononokeIdentity::from_str(rollout_group)?);

    create_both_mode_test_changeset_with_config_metadata(
        fb,
        config_metadata,
        vec![(root, acl)],
        vec!["shared/file.txt"],
        Arc::new(TestAclProvider),
    )
    .await
}

async fn create_both_mode_test_changeset_with_acl_provider(
    fb: FacebookInit,
    config_path_acls: Vec<(&str, &str)>,
    acl_manifest_path_acls: Vec<(&str, &str)>,
    file_paths: Vec<&str>,
    acl_provider: Arc<dyn AclProvider>,
) -> Result<(RepoContext<Repo>, ChangesetContext<Repo>)> {
    create_both_mode_test_changeset_with_config_metadata(
        fb,
        build_path_restriction_metadata(config_path_acls)?,
        acl_manifest_path_acls,
        file_paths,
        acl_provider,
    )
    .await
}

async fn create_both_mode_test_changeset_with_config_metadata(
    fb: FacebookInit,
    path_restriction_metadata: HashMap<NonRootMPath, PathRestrictionMetadata>,
    acl_manifest_path_acls: Vec<(&str, &str)>,
    file_paths: Vec<&str>,
    acl_provider: Arc<dyn AclProvider>,
) -> Result<(RepoContext<Repo>, ChangesetContext<Repo>)> {
    let ctx = CoreContext::test_mock(fb);
    let repo_id = RepositoryId::new(0);
    let config = RestrictedPathsConfig {
        path_restriction_metadata,
        manifest_id_store_config: RestrictedPathsManifestIdStoreConfig {
            use_manifest_id_cache: false,
            cache_update_interval_ms: 100,
            ..Default::default()
        },
        acl_manifest_mode: AclManifestMode::Both,
        ..Default::default()
    };
    let manifest_id_store = Arc::new(
        SqlRestrictedPathsManifestIdStoreBuilder::with_sqlite_in_memory()?.with_repo_id(repo_id),
    );
    let scuba = MononokeScubaSampleBuilder::with_discard();
    let config_based = Arc::new(RestrictedPathsConfigBased::new(
        config,
        manifest_id_store,
        None,
    ));

    let mut factory = TestRepoFactory::new(fb)?;
    let repo: Repo = factory.build().await?;
    let repo_derived_data = repo.repo_derived_data_arc();
    let restricted_paths = Arc::new(RestrictedPaths::new(
        "repo".to_string(),
        config_based,
        acl_provider,
        scuba,
        repo_derived_data,
    )?);
    let repo: Repo = factory
        .with_restricted_paths(restricted_paths)
        .build()
        .await?;

    let mut commit_ctx = CreateCommitContext::new_root(&ctx, &repo);
    for file_path in file_paths {
        commit_ctx = commit_ctx.add_file(file_path, "content");
    }
    for (root, acl) in acl_manifest_path_acls {
        let slacl_path = format!("{root}/.slacl");
        let slacl_content = format!("repo_region_acl = \"{acl}\"\n");
        commit_ctx = commit_ctx.add_file(slacl_path.as_str(), slacl_content);
    }
    let root_cs_id = commit_ctx.commit().await?;

    let repo_ctx = RepoContext::new_test(ctx.clone(), Arc::new(repo)).await?;
    let cs_ctx = ChangesetContext::new(repo_ctx.clone(), root_cs_id);

    Ok((repo_ctx, cs_ctx))
}

struct TestAclProvider;

#[async_trait]
impl AclProvider for TestAclProvider {
    async fn repo_acl(&self, _name: &str) -> Result<BoxPermissionChecker> {
        Ok(PermissionCheckerBuilder::new().allow_all().build())
    }

    async fn repo_region_acl(&self, name: &str) -> Result<BoxPermissionChecker> {
        let checker = PermissionCheckerBuilder::new();
        Ok(if name == "myusername_project" {
            checker.allow_all().build()
        } else {
            checker.build()
        })
    }

    async fn tier_acl(&self, _name: &str) -> Result<BoxPermissionChecker> {
        Ok(PermissionCheckerBuilder::new().allow_all().build())
    }

    async fn commitcloud_workspace_acl(
        &self,
        _name: &str,
        _create_with_owner: &Option<MononokeIdentitySet>,
    ) -> Result<Option<BoxPermissionChecker>> {
        Ok(Some(PermissionCheckerBuilder::new().allow_all().build()))
    }

    async fn group(&self, _name: &str) -> Result<BoxMembershipChecker> {
        Ok(AlwaysMember::new())
    }

    async fn admin_group(&self) -> Result<BoxMembershipChecker> {
        Ok(NeverMember::new())
    }

    async fn reviewers_group(&self) -> Result<BoxMembershipChecker> {
        Ok(NeverMember::new())
    }
}

fn build_path_restriction_metadata(
    path_acls: Vec<(&str, &str)>,
) -> Result<HashMap<NonRootMPath, PathRestrictionMetadata>> {
    path_acls
        .into_iter()
        .map(|(path, acl_str)| -> Result<_> {
            Ok((
                NonRootMPath::new(path)?,
                PathRestrictionMetadata {
                    repo_region_acl: MononokeIdentity::from_str(acl_str)?,
                    permission_request_group: None,
                    rollout_allowlist_group: None,
                    read_only: false,
                },
            ))
        })
        .collect()
}

/// Create a RepoContext and ChangesetContext with restricted paths configured.
async fn create_test_changeset(
    fb: FacebookInit,
    path_acls: Vec<(&str, &str)>,
) -> Result<(RepoContext<Repo>, ChangesetContext<Repo>)> {
    let restricted_paths = create_test_restricted_paths(fb, path_acls).await?;
    let ctx = CoreContext::test_mock(fb);

    let repo: Repo = TestRepoFactory::new(fb)?
        .with_restricted_paths(restricted_paths)
        .build()
        .await?;

    let root_cs_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file.txt", "content")
        .commit()
        .await?;

    let repo_ctx = RepoContext::new_test(ctx.clone(), Arc::new(repo)).await?;
    let cs_ctx = ChangesetContext::new(repo_ctx.clone(), root_cs_id);

    Ok((repo_ctx, cs_ctx))
}
