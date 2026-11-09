/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::str::FromStr;

use anyhow::Result;
use fbinit::FacebookInit;
use futures::FutureExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use metaconfig_types::AclManifestMode;
use metaconfig_types::ComparableRegex;
use metaconfig_types::EnforcementExemptionSet;
use metaconfig_types::RequestMatchers;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::NonRootMPath;
use mononoke_types::RepoPath;
use mononoke_types::RepositoryId;
use permission_checker::MononokeIdentity;
use restricted_paths::*;

mod utils;
use utils::*;

#[mononoke::fbinit_test]
async fn test_no_restricted_change(fb: FacebookInit) -> Result<()> {
    let restricted_paths = vec![(
        NonRootMPath::new("restricted/dir")?,
        MononokeIdentity::from_str("REPO_REGION:restricted_acl")?,
    )];
    let restricted_acl = restricted_paths[0].1.clone();
    let expected_hg_manifest_id =
        RestrictedManifestId::from("d2bc28c1e22aa87a4df6fda1a5a8b76cbb8a6ebe");
    let expected_fsnode_id = RestrictedManifestId::from(
        "c3de088b372fa1eb92d2cf815aa14c6f066075146bcdd6fb1213273e4b0d28f1",
    );
    let expected_content_manifest_id = RestrictedManifestId::from(
        "df75ca27defed1062b2b24d88d5672ec9994ee14590c2b1ab546132c937bd903",
    );

    // Base sample with fields common to ALL expected samples
    let base_sample = ScubaAccessLogSampleBuilder::new()
        .with_repo_id(RepositoryId::new(0))
        .with_client_identities(
            vec!["USER:myusername0"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        )
        .with_client_main_id(TEST_CLIENT_MAIN_ID.to_string());

    RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(restricted_paths)
        .with_file_path_changes(vec![("unrestricted/dir/a", None)])
        .expecting_manifest_id_store_entries(vec![
            RestrictedPathManifestIdEntry::new(
                ManifestType::Hg,
                expected_hg_manifest_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::HgAugmented,
                expected_hg_manifest_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::Fsnode,
                expected_fsnode_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::ContentManifest,
                expected_content_manifest_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
        ])
        .expecting_scuba_access_logs(vec![
            // HgManifest tree traversal
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_manifest_id(expected_hg_manifest_id.clone())
                .with_manifest_type(ManifestType::Hg)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // HgAugmentedManifest tree traversal
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_manifest_id(expected_hg_manifest_id.clone())
                .with_manifest_type(ManifestType::HgAugmented)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // Path tree traversal
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // ContentManifest tree traversal
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_manifest_id(expected_content_manifest_id.clone())
                .with_manifest_type(ManifestType::ContentManifest)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // .slacl path access (paths_with_content)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/.slacl")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // .slacl path access (paths_with_history)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/.slacl")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
        ])
        .build(fb)
        .await?
        .run_restricted_paths_test()
        .await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_change_to_restricted_with_access_is_logged(fb: FacebookInit) -> Result<()> {
    let project_acl = MononokeIdentity::from_str("REPO_REGION:myusername_project")?;
    let restricted_paths = vec![(NonRootMPath::new("user_project/foo")?, project_acl.clone())];

    let expected_manifest_id =
        RestrictedManifestId::from("244ccdc0f5356411811d5fb7ddc684768d11530b");

    // Base sample with fields common to ALL expected samples
    let base_sample = ScubaAccessLogSampleBuilder::new()
        .with_repo_id(RepositoryId::new(0))
        .with_client_identities(
            vec!["USER:myusername0"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        )
        .with_client_main_id(TEST_CLIENT_MAIN_ID.to_string())
        .with_access_enforcement_enabled(true);

    let expected_fsnode_id = RestrictedManifestId::from(
        "d2f5938d41237c86b1b81ffad71cd54c0aba164651bf10af6662ca92d6945676",
    );

    let expected_content_manifest_id = RestrictedManifestId::from(
        "46fb0b98545a63210d4d2007127ccfa186dab4179b328b3d2ad6c94d96c3f3f7",
    );

    RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(restricted_paths)
        .with_file_path_changes(vec![("user_project/foo/bar/a", None)])
        .expecting_manifest_id_store_entries(vec![
            RestrictedPathManifestIdEntry::new(
                ManifestType::Hg,
                expected_manifest_id.clone(),
                RepoPath::dir("user_project/foo")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::HgAugmented,
                expected_manifest_id.clone(),
                RepoPath::dir("user_project/foo")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::Fsnode,
                expected_fsnode_id.clone(),
                RepoPath::dir("user_project/foo")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::ContentManifest,
                expected_content_manifest_id.clone(),
                RepoPath::dir("user_project/foo")?,
            )?,
        ])
        .expecting_scuba_access_logs(vec![
            // HgManifest access log
            base_sample
                .clone()
                // The restricted path root is logged, not the full path
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["user_project/foo"])?)
                .with_manifest_id(expected_manifest_id.clone())
                .with_manifest_type(ManifestType::Hg)
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![project_acl.clone()])
                .build()?,
            // HgAugmentedManifest access log
            base_sample
                .clone()
                // The restricted path root is logged, not the full path
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["user_project/foo"])?)
                .with_manifest_id(expected_manifest_id.clone())
                .with_manifest_type(ManifestType::HgAugmented)
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![project_acl.clone()])
                .build()?,
            // Path access logs for directories traversed
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["user_project/foo"])?)
                .with_full_path(NonRootMPath::new("user_project/foo")?)
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![project_acl.clone()])
                .build()?,
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["user_project/foo"])?)
                .with_full_path(NonRootMPath::new("user_project/foo/bar")?)
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![project_acl.clone()])
                .build()?,
            // ContentManifest access log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["user_project/foo"])?)
                .with_manifest_id(expected_content_manifest_id.clone())
                .with_manifest_type(ManifestType::ContentManifest)
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![project_acl.clone()])
                .build()?,
            // .slacl path access (paths_with_content)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["user_project/foo"])?)
                .with_full_path(NonRootMPath::new("user_project/foo/.slacl")?)
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![project_acl.clone()])
                .build()?,
            // .slacl path access (paths_with_history)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["user_project/foo"])?)
                .with_full_path(NonRootMPath::new("user_project/foo/.slacl")?)
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![project_acl.clone()])
                .build()?,
            // Path access logs for directories traversed
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["user_project/foo"])?)
                .with_full_path(NonRootMPath::new("user_project/foo/bar/a")?)
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![project_acl.clone()])
                .build()?,
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["user_project/foo"])?)
                .with_full_path(NonRootMPath::new("user_project/foo/bar/a")?)
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![project_acl.clone()])
                .build()?,
        ])
        .build(fb)
        .await?
        .run_restricted_paths_test_scenario(
            &[EnforcementConditionSetBuilder::new()
                .with_restriction_acls([project_acl.clone()])
                .build()],
            // Enforcement is enabled, but user has access to the restricted
            // path, so no AuthorizationError is thrown.
            false,
        )
        .await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_single_dir_single_restricted_change(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let restricted_paths = vec![(NonRootMPath::new("restricted/dir")?, restricted_acl.clone())];

    let expected_manifest_id =
        RestrictedManifestId::from("341074482e5d30e3afb06cb4c89e758821073296");

    // Base sample with fields common to ALL expected samples
    let base_sample = ScubaAccessLogSampleBuilder::new()
        .with_repo_id(RepositoryId::new(0))
        .with_client_identities(
            vec!["USER:myusername0"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        )
        .with_client_main_id(TEST_CLIENT_MAIN_ID.to_string());

    let expected_fsnode_id = RestrictedManifestId::from(
        "2d3027385add91b3b1f68187c99b03fe464d8539dfc80507005ff690bd4740bb",
    );

    let expected_content_manifest_id = RestrictedManifestId::from(
        "9f452248a768ac52e6e32863c5fd983fa76e9d2952ba9fb449dac7044ec46c7c",
    );

    let expected_manifest_entries = || -> Result<Vec<RestrictedPathManifestIdEntry>> {
        Ok(vec![
            RestrictedPathManifestIdEntry::new(
                ManifestType::Hg,
                expected_manifest_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::HgAugmented,
                expected_manifest_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::Fsnode,
                expected_fsnode_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::ContentManifest,
                expected_content_manifest_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
        ])
    };

    let expected_scuba_logs =
        |access_enforcement_enabled: Option<bool>| -> Result<Vec<ScubaAccessLogSample>> {
            let base_sample = match access_enforcement_enabled {
                Some(access_enforcement_enabled) => base_sample
                    .clone()
                    .with_access_enforcement_enabled(access_enforcement_enabled),
                None => base_sample.clone(),
            };

            Ok(vec![
                // HgManifest access log
                base_sample
                    .clone()
                    .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                    .with_manifest_id(expected_manifest_id.clone())
                    .with_manifest_type(ManifestType::Hg)
                    .with_has_authorization(false)
                    .with_acls(vec![restricted_acl.clone()])
                    .build()?,
                // HgAugmentedManifest access log
                base_sample
                    .clone()
                    .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                    .with_manifest_id(expected_manifest_id.clone())
                    .with_manifest_type(ManifestType::HgAugmented)
                    .with_has_authorization(false)
                    .with_acls(vec![restricted_acl.clone()])
                    .build()?,
                // Path access log
                base_sample
                    .clone()
                    .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                    .with_full_path(NonRootMPath::new("restricted/dir")?)
                    .with_has_authorization(false)
                    .with_acls(vec![restricted_acl.clone()])
                    .build()?,
                // ContentManifest access log
                base_sample
                    .clone()
                    .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                    .with_manifest_id(expected_content_manifest_id.clone())
                    .with_manifest_type(ManifestType::ContentManifest)
                    .with_has_authorization(false)
                    .with_acls(vec![restricted_acl.clone()])
                    .build()?,
                // .slacl path access (paths_with_content)
                base_sample
                    .clone()
                    .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                    .with_full_path(NonRootMPath::new("restricted/dir/.slacl")?)
                    .with_has_authorization(false)
                    .with_acls(vec![restricted_acl.clone()])
                    .build()?,
                // .slacl path access (paths_with_history)
                base_sample
                    .clone()
                    .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                    .with_full_path(NonRootMPath::new("restricted/dir/.slacl")?)
                    .with_has_authorization(false)
                    .with_acls(vec![restricted_acl.clone()])
                    .build()?,
                // Path content access log
                base_sample
                    .clone()
                    .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                    .with_full_path(NonRootMPath::new("restricted/dir/a")?)
                    .with_has_authorization(false)
                    .with_acls(vec![restricted_acl.clone()])
                    .build()?,
                base_sample
                    .clone()
                    .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                    .with_full_path(NonRootMPath::new("restricted/dir/a")?)
                    .with_has_authorization(false)
                    .with_acls(vec![restricted_acl.clone()])
                    .build()?,
            ])
        };

    let enforcement_scenarios = vec![
        // No condition sets = no enforcement (logging only)
        (vec![], false, None),
        // Non-matching restriction ACL = no enforcement
        (
            vec![
                EnforcementConditionSetBuilder::new()
                    .with_restriction_acls([MononokeIdentity::from_legacy_type_data(
                        "REPO_REGION",
                        "nonexistent_acl",
                    )])
                    .build(),
            ],
            false,
            Some(false),
        ),
        // Matching restriction ACL = enforcement triggered
        (
            vec![
                EnforcementConditionSetBuilder::new()
                    .with_restriction_acls([restricted_acl.clone()])
                    .build(),
            ],
            true,
            Some(true),
        ),
        // Multiple condition sets are OR'd together: if any set matches,
        // enforcement is triggered.
        (
            vec![
                EnforcementConditionSetBuilder::new()
                    .with_restriction_acls([MononokeIdentity::from_legacy_type_data(
                        "REPO_REGION",
                        "nonexistent_acl",
                    )])
                    .build(),
                EnforcementConditionSetBuilder::new()
                    .with_restriction_acls([restricted_acl.clone()])
                    .build(),
            ],
            true,
            Some(true),
        ),
    ];

    for (enforcement_condition_sets, expect_enforcement, access_enforcement_enabled) in
        enforcement_scenarios
    {
        RestrictedPathsTestDataBuilder::new()
            .with_restricted_paths(restricted_paths.clone())
            .with_file_path_changes(vec![("restricted/dir/a", None)])
            .expecting_manifest_id_store_entries(expected_manifest_entries()?)
            .expecting_scuba_access_logs(expected_scuba_logs(access_enforcement_enabled)?)
            .build(fb)
            .await?
            .run_restricted_paths_test_scenario(&enforcement_condition_sets, expect_enforcement)
            .await?;
    }

    Ok(())
}

// What it tests: a condition set scoped to the matching restricted path ACL.
// Expected: unauthorized access to a path with that restriction ACL is denied.
#[mononoke::fbinit_test]
async fn test_enforcement_condition_set_matching_restriction_acl(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let was_denied = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(
            NonRootMPath::new("restricted/dir")?,
            restricted_acl.clone(),
        )])
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_restriction_acls([restricted_acl])
                .build()],
        )
        .await?;

    assert!(was_denied);
    Ok(())
}

// What it tests: a condition set scoped to an unrelated restricted path ACL should
// not deny access to this restricted path.
// Expected: unauthorized access is allowed because the condition set does not match.
#[mononoke::fbinit_test]
async fn test_enforcement_condition_set_non_matching_restriction_acl(
    fb: FacebookInit,
) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let unrelated_acl = MononokeIdentity::from_str("REPO_REGION:unrelated_acl")?;
    let was_denied = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(NonRootMPath::new("restricted/dir")?, restricted_acl)])
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_restriction_acls([unrelated_acl])
                .build()],
        )
        .await?;

    assert!(!was_denied);
    Ok(())
}

// What it tests: the all-default condition set.
// Expected: unauthorized access is allowed because default sets do not enforce.
#[mononoke::fbinit_test]
async fn test_enforcement_condition_set_default_does_not_enforce(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let was_denied = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(NonRootMPath::new("restricted/dir")?, restricted_acl)])
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new().build()],
        )
        .await?;

    assert!(!was_denied);
    Ok(())
}

// What it tests: an explicit always-enabled condition set.
// Expected: unauthorized access is denied for every restricted path.
#[mononoke::fbinit_test]
async fn test_enforcement_condition_set_always_enabled(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let was_denied = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(NonRootMPath::new("restricted/dir")?, restricted_acl)])
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_always_enabled(true)
                .build()],
        )
        .await?;

    assert!(was_denied);
    Ok(())
}

// What it tests: an exemption set matching the caller, with an
// always-enabled condition set.
// Expected: unauthorized access is allowed, because the exemption switches
// enforcement off even for `always_enabled` conditions.
#[mononoke::fbinit_test]
async fn test_enforcement_exemption_with_always_enabled_condition(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let was_denied = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(NonRootMPath::new("restricted/dir")?, restricted_acl)])
        .with_enforcement_exemption_sets(vec![EnforcementExemptionSet::new(RequestMatchers {
            client_identity_regexes: vec![ComparableRegex::new("^USER:myusername0$")?],
            ..Default::default()
        })?])
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_always_enabled(true)
                .build()],
        )
        .await?;

    assert!(!was_denied, "a matching exemption should allow the access");
    Ok(())
}

// What it tests: an exemption set is evaluated with AND semantics across its
// filters, like a condition set.
// Expected: unauthorized access is still denied when only some of the
// exemption's filters match the caller.
#[mononoke::fbinit_test]
async fn test_enforcement_exemption_partial_match_does_not_exempt(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let was_denied = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(NonRootMPath::new("restricted/dir")?, restricted_acl)])
        .with_enforcement_exemption_sets(vec![EnforcementExemptionSet::new(RequestMatchers {
            machine_tiers: vec!["nonexistent_tier".to_string()],
            client_identity_regexes: vec![ComparableRegex::new("^USER:myusername0$")?],
            ..Default::default()
        })?])
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_always_enabled(true)
                .build()],
        )
        .await?;

    assert!(
        was_denied,
        "an exemption whose machine tier does not match should not exempt"
    );
    Ok(())
}

// What it tests: an exemption set that does not match the caller.
// Expected: unauthorized access is denied as if no exemption were configured.
#[mononoke::fbinit_test]
async fn test_enforcement_exemption_non_matching_caller(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let was_denied = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(NonRootMPath::new("restricted/dir")?, restricted_acl)])
        .with_enforcement_exemption_sets(vec![EnforcementExemptionSet::new(RequestMatchers {
            client_identity_regexes: vec![ComparableRegex::new("^USER:someone_else$")?],
            ..Default::default()
        })?])
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_always_enabled(true)
                .build()],
        )
        .await?;

    assert!(
        was_denied,
        "a non-matching exemption should not allow the access"
    );
    Ok(())
}

// What it tests: request-local condition-set filters.
// Expected: unauthorized access is allowed without the required request flag and
// denied when the request flag is present.
#[mononoke::fbinit_test]
async fn test_enforcement_condition_set_request_local_filters(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let restricted_paths = vec![(NonRootMPath::new("restricted/dir")?, restricted_acl.clone())];
    let was_denied_without_flag = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(restricted_paths.clone())
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_entry_points(["tests"])
                .with_require_client_request_flag(true)
                .with_restriction_acls([restricted_acl.clone()])
                .build()],
        )
        .await?;
    let was_denied_with_flag = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(restricted_paths)
        .with_server_side_tenting(true)
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_entry_points(["tests"])
                .with_require_client_request_flag(true)
                .with_restriction_acls([restricted_acl])
                .build()],
        )
        .await?;

    assert!(!was_denied_without_flag);
    assert!(was_denied_with_flag);
    Ok(())
}

// What it tests: a condition set with both the request flag and restriction ACL filters.
// Expected: both filters must match, so unauthorized access is allowed without
// the request flag and denied when the request flag is present.
#[mononoke::fbinit_test]
async fn test_enforcement_condition_set_request_flag_and_restriction_acl(
    fb: FacebookInit,
) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let restricted_paths = vec![(NonRootMPath::new("restricted/dir")?, restricted_acl.clone())];
    let condition_set = || {
        EnforcementConditionSetBuilder::new()
            .with_require_client_request_flag(true)
            .with_restriction_acls([restricted_acl.clone()])
            .build()
    };

    let was_denied_without_flag = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(restricted_paths.clone())
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[condition_set()],
        )
        .await?;
    let was_denied_with_flag = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(restricted_paths)
        .with_server_side_tenting(true)
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[condition_set()],
        )
        .await?;

    assert!(!was_denied_without_flag);
    assert!(was_denied_with_flag);
    Ok(())
}

// What it tests: `require_client_request_flag` satisfied by either
// partial-responses request header.
// Expected: unauthorized access is enforced when the client sent the opt-in
// or the opt-out header, since either declares the client handles
// enforcement; allowed when neither was sent.
#[mononoke::fbinit_test]
async fn test_enforcement_condition_set_partial_responses_headers(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let restricted_paths = vec![(NonRootMPath::new("restricted/dir")?, restricted_acl.clone())];
    let condition_set = || {
        EnforcementConditionSetBuilder::new()
            .with_require_client_request_flag(true)
            .with_restriction_acls([restricted_acl.clone()])
            .build()
    };

    let was_denied_without_flag = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(restricted_paths.clone())
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[condition_set()],
        )
        .await?;
    let was_denied_with_opt_in = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(restricted_paths.clone())
        .with_partial_responses_opt_in(true)
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[condition_set()],
        )
        .await?;
    let was_denied_with_opt_out = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(restricted_paths)
        .with_partial_responses_opt_out(true)
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[condition_set()],
        )
        .await?;

    assert!(!was_denied_without_flag);
    assert!(was_denied_with_opt_in);
    assert!(was_denied_with_opt_out);
    Ok(())
}

// What it tests: condition set whose `machine_tiers` list contains the caller's
// MACHINE_TIER identity value.
// Expected: filter matches, so unauthorized access is enforced.
#[mononoke::fbinit_test]
async fn machine_tier_matches_caller_tier_triggers_enforcement(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let was_denied = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(NonRootMPath::new("restricted/dir")?, restricted_acl)])
        .with_machine_tier("devvm")
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_machine_tiers(["devvm"])
                .build()],
        )
        .await?;

    assert!(
        was_denied,
        "caller MACHINE_TIER `devvm` is listed in the condition set's machine_tiers, so enforcement should fire"
    );
    Ok(())
}

// What it tests: condition set whose `machine_tiers` list does not contain the
// caller's MACHINE_TIER identity value.
// Expected: filter does not match, so unauthorized access is not enforced.
#[mononoke::fbinit_test]
async fn machine_tier_mismatch_does_not_trigger_enforcement(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let was_denied = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(NonRootMPath::new("restricted/dir")?, restricted_acl)])
        .with_machine_tier("devvm")
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_machine_tiers(["nonexistent_tier"])
                .build()],
        )
        .await?;

    assert!(
        !was_denied,
        "caller MACHINE_TIER `devvm` is not in `[nonexistent_tier]`, so enforcement should not fire"
    );
    Ok(())
}

// What it tests: condition set whose `client_identity_regexes` matches the
// caller's default USER identity.
// Expected: filter matches, so unauthorized access is enforced.
#[mononoke::fbinit_test]
async fn client_identity_regex_matches_caller_identity_triggers_enforcement(
    fb: FacebookInit,
) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let was_denied = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(NonRootMPath::new("restricted/dir")?, restricted_acl)])
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_client_identity_regexes(["^USER:myusername0$"])
                .build()],
        )
        .await?;

    assert!(
        was_denied,
        "caller USER `myusername0` matches `^USER:myusername0$`, so enforcement should fire"
    );
    Ok(())
}

// What it tests: condition set whose `client_identity_regexes` does not match
// any caller identity.
// Expected: filter does not match, so unauthorized access is not enforced.
#[mononoke::fbinit_test]
async fn client_identity_regex_mismatch_does_not_trigger_enforcement(
    fb: FacebookInit,
) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let was_denied = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(NonRootMPath::new("restricted/dir")?, restricted_acl)])
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_client_identity_regexes(["^USER:nonexistent$"])
                .build()],
        )
        .await?;

    assert!(
        !was_denied,
        "caller USER `myusername0` does not match `^USER:nonexistent$`, so enforcement should not fire"
    );
    Ok(())
}

// What it tests: any-of semantics across regex list and any-of semantics across
// caller identities. The caller carries both USER and MACHINE_TIER; the
// condition set has one regex that does not match and one that matches the
// MACHINE_TIER identity.
// Expected: filter matches via the second regex, so enforcement fires.
#[mononoke::fbinit_test]
async fn any_regex_matches_any_identity_triggers_enforcement(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let was_denied = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(NonRootMPath::new("restricted/dir")?, restricted_acl)])
        .with_machine_tier("devvm")
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_client_identity_regexes(["^USER:nonexistent$", "^MACHINE_TIER:devvm$"])
                .build()],
        )
        .await?;

    assert!(
        was_denied,
        "caller MACHINE_TIER `devvm` matches the second regex, so enforcement should fire"
    );
    Ok(())
}

// What it tests: empty `client_identity_regexes` does not gate enforcement when
// another filter (here, `restriction_acls`) is active and matches the access.
// Expected: enforcement fires based on the restriction_acl match alone.
#[mononoke::fbinit_test]
async fn empty_client_identity_regexes_does_not_gate_when_other_filter_fires(
    fb: FacebookInit,
) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let was_denied = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(
            NonRootMPath::new("restricted/dir")?,
            restricted_acl.clone(),
        )])
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_restriction_acls([restricted_acl])
                .build()],
        )
        .await?;

    assert!(
        was_denied,
        "empty client_identity_regexes should not gate; matching restriction_acl decides"
    );
    Ok(())
}

// What it tests: a condition set with empty `client_identity_regexes` and no
// other active filter (everything default) should not trigger enforcement.
// Expected: `condition_set_has_active_filter` returns false → set dropped at
// the gate → no enforcement.
#[mononoke::fbinit_test]
async fn empty_client_identity_regexes_with_no_other_filter_does_not_enforce(
    fb: FacebookInit,
) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let was_denied = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(NonRootMPath::new("restricted/dir")?, restricted_acl)])
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new().build()],
        )
        .await?;

    assert!(
        !was_denied,
        "condition set with no active filter (default) should be dropped, so no enforcement"
    );
    Ok(())
}

// What it tests: condition set with only `is_agent = Some(true)` and a caller
// whose identity carries an `agent` attribute.
// Expected: the dimension alone counts as an active filter and matches, so
// unauthorized access is enforced.
#[cfg(fbcode_build)]
#[mononoke::fbinit_test]
async fn is_agent_true_matches_agentic_caller_triggers_enforcement(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let was_denied = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(NonRootMPath::new("restricted/dir")?, restricted_acl)])
        .with_agentic_client_identity()?
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_is_agent(true)
                .build()],
        )
        .await?;

    assert!(
        was_denied,
        "caller carries an `agent` attribute, so an is_agent=Some(true) set should fire"
    );
    Ok(())
}

// What it tests: condition set with only `is_agent = Some(true)` and the default
// caller, whose identity carries no `agent` attribute.
// Expected: filter does not match, so unauthorized access is not enforced.
#[cfg(fbcode_build)]
#[mononoke::fbinit_test]
async fn is_agent_true_non_agent_caller_does_not_trigger_enforcement(
    fb: FacebookInit,
) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let was_denied = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(NonRootMPath::new("restricted/dir")?, restricted_acl)])
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_is_agent(true)
                .build()],
        )
        .await?;

    assert!(
        !was_denied,
        "caller has no `agent` attribute, so an is_agent=Some(true) set should not fire"
    );
    Ok(())
}

// What it tests: AND semantics between `is_agent` and
// `client_identity_regexes`. The agentic caller's id is still
// `USER:myusername0`, so both dimensions match.
// Expected: filter matches, so unauthorized access is enforced.
#[cfg(fbcode_build)]
#[mononoke::fbinit_test]
async fn is_agent_true_and_identity_regex_both_match_triggers_enforcement(
    fb: FacebookInit,
) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let was_denied = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(NonRootMPath::new("restricted/dir")?, restricted_acl)])
        .with_agentic_client_identity()?
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_is_agent(true)
                .with_client_identity_regexes(["^USER:myusername0$"])
                .build()],
        )
        .await?;

    assert!(
        was_denied,
        "agentic caller `USER:myusername0` satisfies both is_agent=Some(true) and the regex, so enforcement should fire"
    );
    Ok(())
}

// What it tests: condition set with only `is_agent = Some(false)` and the
// default caller, whose identity carries no `agent` attribute.
// Expected: the dimension alone counts as an active filter and matches, so
// unauthorized access is enforced.
#[mononoke::fbinit_test]
async fn is_agent_false_matches_non_agent_caller_triggers_enforcement(
    fb: FacebookInit,
) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let was_denied = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(NonRootMPath::new("restricted/dir")?, restricted_acl)])
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_is_agent(false)
                .build()],
        )
        .await?;

    assert!(
        was_denied,
        "caller has no `agent` attribute, so an is_agent=Some(false) set should fire"
    );
    Ok(())
}

// What it tests: condition set with only `is_agent = Some(false)` and a caller
// whose identity carries an `agent` attribute.
// Expected: filter does not match, so unauthorized access is not enforced.
#[cfg(fbcode_build)]
#[mononoke::fbinit_test]
async fn is_agent_false_agentic_caller_does_not_trigger_enforcement(
    fb: FacebookInit,
) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let was_denied = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(NonRootMPath::new("restricted/dir")?, restricted_acl)])
        .with_agentic_client_identity()?
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_is_agent(false)
                .build()],
        )
        .await?;

    assert!(
        !was_denied,
        "caller carries an `agent` attribute, so an is_agent=Some(false) set should not fire"
    );
    Ok(())
}

// What it tests: enforcement should not depend on the access-log JK once
// source fetches are spawned for enforcement independently from logging.
// Expected: disabling `enabled_restricted_paths_access_logging` still denies
// this unauthorized config-backed access.
#[mononoke::fbinit_test]
async fn test_enforcement_does_not_depend_on_logging_jk(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let test_data = RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(vec![(
            NonRootMPath::new("restricted/dir")?,
            restricted_acl.clone(),
        )])
        .build(fb)
        .await?;

    with_just_knobs_async(
        restricted_paths_justknobs(&[(
            "scm/mononoke:enabled_restricted_paths_access_logging",
            false,
        )]),
        async move {
            let result = test_data
                .observe_path_enforcement(
                    NonRootMPath::new("restricted/dir/file")?,
                    &[EnforcementConditionSetBuilder::new()
                        .with_restriction_acls([restricted_acl])
                        .build()],
                )
                .await?;
            assert!(result);
            Ok(())
        }
        .boxed(),
    )
    .await
}

// What it tests: Authoritative mode should require the AclManifest source
// when enforcing a path access.
// Expected: this returns an internal error because no changeset id is
// available for the authoritative path source.
#[mononoke::fbinit_test]
async fn test_authoritative_path_enforcement_requires_acl_manifest_source(
    fb: FacebookInit,
) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let result = RestrictedPathsTestDataBuilder::new()
        .with_acl_manifest_mode(AclManifestMode::Authoritative)
        .with_acl_manifest_restricted_paths(vec![(
            NonRootMPath::new("restricted/dir")?,
            restricted_acl,
        )])
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_always_enabled(true)
                .build()],
        )
        .await;

    assert!(result.is_err());
    Ok(())
}

// What it tests: Authoritative mode should deny from AclManifest-only
// restrictions once AclManifest enforcement source selection is wired.
// Expected: Authoritative mode uses the AclManifest source and denies this
// unauthorized path access.
#[mononoke::fbinit_test]
async fn test_authoritative_mode_acl_manifest_only_restriction_denies(
    fb: FacebookInit,
) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;

    let result = RestrictedPathsTestDataBuilder::new()
        .with_acl_manifest_mode(AclManifestMode::Authoritative)
        .with_acl_manifest_restricted_paths(vec![(
            NonRootMPath::new("restricted/dir")?,
            restricted_acl.clone(),
        )])
        .with_file_path_changes(vec![("restricted/dir/file", None)])
        .build(fb)
        .await?
        .observe_path_enforcement_after_commit(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_always_enabled(true)
                .build()],
        )
        .await?;

    assert!(result);
    Ok(())
}

// What it tests: Both mode should allow when config and AclManifest agree that
// the caller is authorized.
// Expected: access remains allowed after Both mode starts evaluating both
// sources.
#[mononoke::fbinit_test]
async fn test_both_mode_agreement_allows(fb: FacebookInit) -> Result<()> {
    let allowed_acl = MononokeIdentity::from_str("REPO_REGION:myusername_project")?;
    let restricted_root = NonRootMPath::new("user_project/foo")?;

    let result = RestrictedPathsTestDataBuilder::new()
        .with_acl_manifest_mode(AclManifestMode::Both)
        .with_config_restricted_paths(vec![(restricted_root.clone(), allowed_acl.clone())])
        .with_acl_manifest_restricted_paths(vec![(restricted_root, allowed_acl)])
        .with_file_path_changes(vec![("user_project/foo/file", None)])
        .build(fb)
        .await?
        .observe_path_enforcement_after_commit(
            NonRootMPath::new("user_project/foo/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_always_enabled(true)
                .build()],
        )
        .await?;

    assert!(!result);
    Ok(())
}

// What it tests: Both mode should deny when AclManifest denies even if config
// allows the same path.
// Expected: Both mode denies because one authoritative source denies.
#[mononoke::fbinit_test]
async fn test_both_mode_acl_manifest_deny_overrides_config_allow(fb: FacebookInit) -> Result<()> {
    let allowed_acl = MononokeIdentity::from_str("REPO_REGION:myusername_project")?;
    let denied_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let restricted_root = NonRootMPath::new("restricted/dir")?;

    let result = RestrictedPathsTestDataBuilder::new()
        .with_acl_manifest_mode(AclManifestMode::Both)
        .with_config_restricted_paths(vec![(restricted_root.clone(), allowed_acl)])
        .with_acl_manifest_restricted_paths(vec![(restricted_root, denied_acl.clone())])
        .with_file_path_changes(vec![("restricted/dir/file", None)])
        .build(fb)
        .await?
        .observe_path_enforcement_after_commit(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_restriction_acls([denied_acl])
                .build()],
        )
        .await?;

    assert!(result);
    Ok(())
}

/// What it tests: combined metadata checking and Both-mode request enforcement
/// reconcile conflicting ACLs for the same restriction root consistently.
/// Expected: the path, root and descendant checks and enforcement all require
/// the config and AclManifest ACLs, so the checks report no authorization and
/// enforcement denies access.
#[mononoke::fbinit_test]
async fn test_both_mode_combined_check_and_enforcement_agree_for_conflicting_acls(
    fb: FacebookInit,
) -> Result<()> {
    let denied_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let allowed_acl = MononokeIdentity::from_str("REPO_REGION:myusername_project")?;
    let restricted_root = NonRootMPath::new("restricted/dir")?;
    let restricted_path = NonRootMPath::new("restricted/dir/file")?;
    let enforcement_condition_sets = [EnforcementConditionSetBuilder::new()
        .with_always_enabled(true)
        .build()];

    let test_data = RestrictedPathsTestDataBuilder::new()
        .with_acl_manifest_mode(AclManifestMode::Both)
        .with_config_restricted_paths(vec![(restricted_root.clone(), denied_acl.clone())])
        .with_acl_manifest_restricted_paths(vec![(restricted_root.clone(), allowed_acl.clone())])
        .with_file_path_changes(vec![("restricted/dir/file", None)])
        .build(fb)
        .await?;

    let observed = test_data
        .observe_combined_checks_and_enforcement_after_commit(
            restricted_root.clone(),
            restricted_path,
            &enforcement_condition_sets,
        )
        .await?;
    for checks in [
        &observed.path_checks,
        &observed.root_checks,
        &observed.descendant_checks,
    ] {
        assert_eq!(checks.len(), 1, "{checks:#?}");
        assert_eq!(
            checks[0].restriction_info().repo_region_acl,
            allowed_acl.to_string(),
            "the combined check should show the AclManifest ACL"
        );
        assert!(
            !checks[0].has_authorization(),
            "the combined check must also require the config ACL: {checks:#?}"
        );
    }

    assert!(
        observed.was_denied,
        "Both-mode request enforcement should deny on the config ACL"
    );
    Ok(())
}

// What it tests: Both mode should deny if one source denies even when the
// sibling source errors.
// Expected: Both mode gives deny precedence over sibling source errors.
#[mononoke::fbinit_test]
async fn test_both_mode_deny_overrides_sibling_error(fb: FacebookInit) -> Result<()> {
    let denied_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let result = RestrictedPathsTestDataBuilder::new()
        .with_acl_manifest_mode(AclManifestMode::Both)
        .with_config_restricted_paths(vec![(
            NonRootMPath::new("restricted/dir")?,
            denied_acl.clone(),
        )])
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_restriction_acls([denied_acl])
                .build()],
        )
        .await?;

    assert!(result);
    Ok(())
}

// What it tests: Both mode should surface an authoritative-source error when
// one source allows and the sibling source errors.
// Expected: Both mode returns an internal error because no authoritative source
// denied and the AclManifest source is unavailable.
#[mononoke::fbinit_test]
async fn test_both_mode_allow_plus_sibling_error_surfaces_error(fb: FacebookInit) -> Result<()> {
    let allowed_acl = MononokeIdentity::from_str("REPO_REGION:myusername_project")?;
    let result = RestrictedPathsTestDataBuilder::new()
        .with_acl_manifest_mode(AclManifestMode::Both)
        .with_config_restricted_paths(vec![(
            NonRootMPath::new("restricted/dir")?,
            allowed_acl.clone(),
        )])
        .build(fb)
        .await?
        .observe_path_enforcement(
            NonRootMPath::new("restricted/dir/file")?,
            &[EnforcementConditionSetBuilder::new()
                .with_restriction_acls([allowed_acl])
                .build()],
        )
        .await;

    assert!(result.is_err());
    Ok(())
}

// Multiple files in a single restricted directory generate a single entry in
// the manifest id store.
#[mononoke::fbinit_test]
async fn test_single_dir_many_restricted_changes(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let restricted_paths = vec![(NonRootMPath::new("restricted/dir")?, restricted_acl.clone())];

    let expected_manifest_id =
        RestrictedManifestId::from("7f5162c269bf44aa3b612600da8b9cdd4285e0bd");

    let expected_fsnode_id = RestrictedManifestId::from(
        "c79005944ac1b56bcabc57c629ee60d247003e08cb0ff37541e2fb1b1362dfd5",
    );

    let expected_content_manifest_id = RestrictedManifestId::from(
        "0f29e96114e59af9baac0936cb2f611e140d2051563108347d6df6736365f8fe",
    );

    // Base sample with fields common to ALL expected samples
    let base_sample = ScubaAccessLogSampleBuilder::new()
        .with_repo_id(RepositoryId::new(0))
        .with_client_identities(
            vec!["USER:myusername0"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        )
        .with_client_main_id(TEST_CLIENT_MAIN_ID.to_string());

    RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(restricted_paths)
        .with_file_path_changes(vec![("restricted/dir/a", None), ("restricted/dir/b", None)])
        .expecting_manifest_id_store_entries(vec![
            RestrictedPathManifestIdEntry::new(
                ManifestType::Hg,
                expected_manifest_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::HgAugmented,
                expected_manifest_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::Fsnode,
                expected_fsnode_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::ContentManifest,
                expected_content_manifest_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
        ])
        .expecting_scuba_access_logs(vec![
            // HgManifest access log - Single log entry for both files, because they're under the same
            // restricted directory
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_manifest_id(expected_manifest_id.clone())
                .with_manifest_type(ManifestType::Hg)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // HgAugmentedManifest access log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_manifest_id(expected_manifest_id.clone())
                .with_manifest_type(ManifestType::HgAugmented)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // Path access log - for the directory containing both files
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // ContentManifest access log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_manifest_id(expected_content_manifest_id.clone())
                .with_manifest_type(ManifestType::ContentManifest)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // .slacl path access (paths_with_content)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/.slacl")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // .slacl path access (paths_with_history)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/.slacl")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // Path access log - for the directory containing both files
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/a")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // Path access log - for the directory containing both files
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/b")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // Blame access log - for the directory containing both files
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/a")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // Blame access log - for the directory containing both files
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/b")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
        ])
        .build(fb)
        .await?
        .run_restricted_paths_test()
        .await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_single_dir_restricted_and_unrestricted(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let restricted_paths = vec![(NonRootMPath::new("restricted/dir")?, restricted_acl.clone())];

    let expected_manifest_id =
        RestrictedManifestId::from("341074482e5d30e3afb06cb4c89e758821073296");

    // Base sample with fields common to ALL expected samples
    let base_sample = ScubaAccessLogSampleBuilder::new()
        .with_repo_id(RepositoryId::new(0))
        .with_client_identities(
            vec!["USER:myusername0"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        )
        .with_client_main_id(TEST_CLIENT_MAIN_ID.to_string());

    let expected_fsnode_id = RestrictedManifestId::from(
        "2d3027385add91b3b1f68187c99b03fe464d8539dfc80507005ff690bd4740bb",
    );

    let expected_content_manifest_id = RestrictedManifestId::from(
        "9f452248a768ac52e6e32863c5fd983fa76e9d2952ba9fb449dac7044ec46c7c",
    );

    RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(restricted_paths)
        .with_file_path_changes(vec![
            ("restricted/dir/a", None),
            ("unrestricted/dir/b", None),
        ])
        .expecting_manifest_id_store_entries(vec![
            RestrictedPathManifestIdEntry::new(
                ManifestType::Hg,
                expected_manifest_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::HgAugmented,
                expected_manifest_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::Fsnode,
                expected_fsnode_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::ContentManifest,
                expected_content_manifest_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
        ])
        .expecting_scuba_access_logs(vec![
            // HgManifest access log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_manifest_id(expected_manifest_id.clone())
                .with_manifest_type(ManifestType::Hg)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // HgAugmentedManifest access log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_manifest_id(expected_manifest_id.clone())
                .with_manifest_type(ManifestType::HgAugmented)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // Path access log - only for restricted directory
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // ContentManifest access log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_manifest_id(expected_content_manifest_id.clone())
                .with_manifest_type(ManifestType::ContentManifest)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // .slacl path access (paths_with_content)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/.slacl")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // .slacl path access (paths_with_history)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/.slacl")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // Path content access log - only for restricted directory
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/a")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // Blame access logs
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/a")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
        ])
        .build(fb)
        .await?
        .run_restricted_paths_test()
        .await?;

    Ok(())
}

// With every config restricted path flipped to read_only, derivation must not
// record new manifest-id-store entries -- while the same writable paths still do.
#[mononoke::fbinit_test]
async fn test_read_only_config_paths_record_no_new_manifest_id_entries(
    fb: FacebookInit,
) -> Result<()> {
    let restricted_paths = vec![(
        NonRootMPath::new("restricted/dir")?,
        MononokeIdentity::from_str("REPO_REGION:restricted_acl")?,
    )];
    // A file directly under the restricted root, so the directory is materialized
    // in the derived manifests and is eligible for recording.
    let file_path_changes = vec![("restricted/dir/a", None)];

    // Baseline: writable config paths record one manifest-id-store entry per
    // manifest type for the restricted root.
    let writable = RestrictedPathsTestDataBuilder::new()
        .with_config_restricted_paths(restricted_paths.clone())
        .with_file_path_changes(file_path_changes.clone())
        .build(fb)
        .await?
        .observe_restricted_paths_scenario(&[])
        .await?;

    let recorded_types: HashSet<ManifestType> = writable
        .manifest_id_store_entries
        .iter()
        .map(|entry| entry.manifest_type.clone())
        .collect();
    assert_eq!(
        recorded_types,
        HashSet::from([
            ManifestType::Hg,
            ManifestType::HgAugmented,
            ManifestType::Fsnode,
            ManifestType::ContentManifest,
        ]),
        "writable restricted paths should record an entry for every manifest type, got {:?}",
        writable.manifest_id_store_entries,
    );

    // Same fixture with every config path marked read_only: no new entries are
    // recorded. The store stays empty because every recording site (the four
    // per-entry derivation gates) consults read_only via
    // should_record_manifest_id_entry.
    let read_only = RestrictedPathsTestDataBuilder::new()
        .with_config_restricted_paths(restricted_paths)
        .with_config_paths_read_only(true)
        .with_file_path_changes(file_path_changes)
        .build(fb)
        .await?
        .observe_restricted_paths_scenario(&[])
        .await?;
    assert!(
        read_only.manifest_id_store_entries.is_empty(),
        "read_only restricted paths must not record new manifest-id entries, got {:?}",
        read_only.manifest_id_store_entries,
    );

    Ok(())
}

// read_only gates derivation recording only; it must NOT change whether a repo
// is considered to have restricted paths. With every config path read_only,
// both may_have_restricted_paths and has_restricted_paths must stay true across
// every AclManifest mode. (read_only is deliberately true here precisely to
// prove these predicates ignore it.)
#[mononoke::fbinit_test]
async fn test_read_only_config_paths_still_count_as_restricted(fb: FacebookInit) -> Result<()> {
    let restricted_paths = vec![
        (
            NonRootMPath::new("restricted/dir")?,
            MononokeIdentity::from_str("REPO_REGION:restricted_acl")?,
        ),
        (
            NonRootMPath::new("other/secret")?,
            MononokeIdentity::from_str("REPO_REGION:other_acl")?,
        ),
    ];

    for mode in [
        AclManifestMode::Disabled,
        AclManifestMode::Shadow,
        AclManifestMode::Both,
        AclManifestMode::Authoritative,
    ] {
        let repo = RestrictedPathsTestDataBuilder::new()
            .with_config_restricted_paths(restricted_paths.clone())
            .with_config_paths_read_only(true)
            .with_acl_manifest_mode(mode)
            .build(fb)
            .await?
            .build_repo_only()
            .await?;

        // `has_restricted_paths` is the load-bearing, read_only-agnostic check in
        // every mode. `may_have_restricted_paths` additionally confirms no mode
        // starts reporting the repo as unrestricted; it short-circuits on the
        // mode for non-Disabled, so it is strongest in Disabled mode.
        assert!(
            repo.restricted_paths().may_have_restricted_paths(),
            "may_have_restricted_paths must be true for read-only paths in mode {mode:?}",
        );
        assert!(
            repo.restricted_paths()
                .config_based()
                .has_restricted_paths(),
            "has_restricted_paths must be true for read-only paths in mode {mode:?}",
        );
    }

    Ok(())
}

// Multiple restricted directories generate multiple entries in the manifest
#[mononoke::fbinit_test]
async fn test_multiple_restricted_dirs(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let another_acl = MononokeIdentity::from_str("REPO_REGION:another_acl")?;
    let restricted_paths = vec![
        (NonRootMPath::new("restricted/one")?, restricted_acl.clone()),
        (NonRootMPath::new("restricted/two")?, another_acl.clone()),
    ];

    let expected_hg_manifest_id_one =
        RestrictedManifestId::from("78faa84a86cb30dfd95b853e87f154851ff0a8c0");
    let expected_hg_manifest_id_two =
        RestrictedManifestId::from("c7d607663aa98d9c03e7e205347e61073075a369");

    let expected_fsnode_id_one = RestrictedManifestId::from(
        "a26c86c7044c434294927b868be621b64a5b47d49c43654f7680d1b018c59721",
    );
    let expected_fsnode_id_two = RestrictedManifestId::from(
        "5a92291e29458c15958f9c7e2eb1fe832e9d441133656df0b09be59278fcb5a7",
    );

    let expected_content_manifest_id_one = RestrictedManifestId::from(
        "59f440164834d1af3deb40582dce788dd101f3cb6badfb497b8fa7a36f633528",
    );
    let expected_content_manifest_id_two = RestrictedManifestId::from(
        "5ae8c3927d5a51891746cb564406f6b70bc4d5b8d45487df66058aed03956d29",
    );

    // Base sample with fields common to ALL expected samples
    let base_sample = ScubaAccessLogSampleBuilder::new()
        .with_repo_id(RepositoryId::new(0))
        .with_client_identities(
            vec!["USER:myusername0"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        )
        .with_client_main_id(TEST_CLIENT_MAIN_ID.to_string());

    RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(restricted_paths)
        .with_file_path_changes(vec![("restricted/one/a", None), ("restricted/two/b", None)])
        .expecting_manifest_id_store_entries(vec![
            RestrictedPathManifestIdEntry::new(
                ManifestType::HgAugmented,
                expected_hg_manifest_id_two.clone(),
                RepoPath::dir("restricted/two")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::HgAugmented,
                expected_hg_manifest_id_one.clone(),
                RepoPath::dir("restricted/one")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::Hg,
                expected_hg_manifest_id_one.clone(),
                RepoPath::dir("restricted/one")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::Hg,
                expected_hg_manifest_id_two.clone(),
                RepoPath::dir("restricted/two")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::Fsnode,
                expected_fsnode_id_one.clone(),
                RepoPath::dir("restricted/one")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::Fsnode,
                expected_fsnode_id_two.clone(),
                RepoPath::dir("restricted/two")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::ContentManifest,
                expected_content_manifest_id_one.clone(),
                RepoPath::dir("restricted/one")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::ContentManifest,
                expected_content_manifest_id_two.clone(),
                RepoPath::dir("restricted/two")?,
            )?,
        ])
        .expecting_scuba_access_logs(vec![
            // restricted/two access - HgManifest log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/two"])?)
                .with_manifest_id(expected_hg_manifest_id_two.clone())
                .with_manifest_type(ManifestType::Hg)
                .with_has_authorization(false)
                .with_acls(vec![another_acl.clone()])
                .build()?,
            // restricted/two access - HgAugmentedManifest log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/two"])?)
                .with_manifest_id(expected_hg_manifest_id_two.clone())
                .with_manifest_type(ManifestType::HgAugmented)
                .with_has_authorization(false)
                .with_acls(vec![another_acl.clone()])
                .build()?,
            // restricted/two access - path log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/two"])?)
                .with_full_path(NonRootMPath::new("restricted/two")?)
                .with_has_authorization(false)
                .with_acls(vec![another_acl.clone()])
                .build()?,
            // restricted/one access - HgManifest log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/one"])?)
                .with_manifest_id(expected_hg_manifest_id_one.clone())
                .with_manifest_type(ManifestType::Hg)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // restricted/one access - HgAugmentedManifest log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/one"])?)
                .with_manifest_id(expected_hg_manifest_id_one.clone())
                .with_manifest_type(ManifestType::HgAugmented)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // restricted/one access - path log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/one"])?)
                .with_full_path(NonRootMPath::new("restricted/one")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // restricted/two access - ContentManifest log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/two"])?)
                .with_manifest_id(expected_content_manifest_id_two.clone())
                .with_manifest_type(ManifestType::ContentManifest)
                .with_has_authorization(false)
                .with_acls(vec![another_acl.clone()])
                .build()?,
            // restricted/one access - ContentManifest log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/one"])?)
                .with_manifest_id(expected_content_manifest_id_one.clone())
                .with_manifest_type(ManifestType::ContentManifest)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // restricted/one/.slacl access - path log (paths_with_content)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/one"])?)
                .with_full_path(NonRootMPath::new("restricted/one/.slacl")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // restricted/one/.slacl access - path log (paths_with_history)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/one"])?)
                .with_full_path(NonRootMPath::new("restricted/one/.slacl")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // restricted/two/.slacl access - path log (paths_with_content)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/two"])?)
                .with_full_path(NonRootMPath::new("restricted/two/.slacl")?)
                .with_has_authorization(false)
                .with_acls(vec![another_acl.clone()])
                .build()?,
            // restricted/two/.slacl access - path log (paths_with_history)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/two"])?)
                .with_full_path(NonRootMPath::new("restricted/two/.slacl")?)
                .with_has_authorization(false)
                .with_acls(vec![another_acl.clone()])
                .build()?,
            // restricted/two access - path log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/two"])?)
                .with_full_path(NonRootMPath::new("restricted/two/b")?)
                .with_has_authorization(false)
                .with_acls(vec![another_acl.clone()])
                .build()?,
            // restricted/one access - path log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/one"])?)
                .with_full_path(NonRootMPath::new("restricted/one/a")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // restricted/two access - path log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/two"])?)
                .with_full_path(NonRootMPath::new("restricted/two/b")?)
                .with_has_authorization(false)
                .with_acls(vec![another_acl.clone()])
                .build()?,
            // restricted/one access - path log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/one"])?)
                .with_full_path(NonRootMPath::new("restricted/one/a")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
        ])
        .build(fb)
        .await?
        .run_restricted_paths_test()
        .await?;

    Ok(())
}

// Test that if the user has access to one of the restricted paths, there will
// be a log entry for each one with the proper authorization result.
#[mononoke::fbinit_test]
async fn test_multiple_restricted_dirs_with_partial_access(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let myusername_project_acl = MononokeIdentity::from_str("REPO_REGION:myusername_project")?;
    let restricted_paths = vec![
        (NonRootMPath::new("restricted/one")?, restricted_acl.clone()),
        (
            // User will have access to this path
            NonRootMPath::new("user_project/foo")?,
            myusername_project_acl.clone(),
        ),
    ];
    let expected_hg_manifest_id_user =
        RestrictedManifestId::from("e79488b9330050337f3f5571ce081d4d083368e5");
    let expected_hg_manifest_id_restricted =
        RestrictedManifestId::from("78faa84a86cb30dfd95b853e87f154851ff0a8c0");

    let expected_fsnode_id_user = RestrictedManifestId::from(
        "02bab3f5ef631f069c14d1c26c21209dd07b97c479be5242f946fce62582dcba",
    );
    let expected_fsnode_id_restricted = RestrictedManifestId::from(
        "a26c86c7044c434294927b868be621b64a5b47d49c43654f7680d1b018c59721",
    );

    let expected_content_manifest_id_restricted = RestrictedManifestId::from(
        "59f440164834d1af3deb40582dce788dd101f3cb6badfb497b8fa7a36f633528",
    );
    let expected_content_manifest_id_user = RestrictedManifestId::from(
        "ccdec6f557d4213cdba387d7d07d497152960b63ee88cb957a605c8ea936a094",
    );

    // Base sample with fields common to ALL expected samples
    let base_sample = ScubaAccessLogSampleBuilder::new()
        .with_repo_id(RepositoryId::new(0))
        .with_client_identities(
            vec!["USER:myusername0"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        )
        .with_client_main_id(TEST_CLIENT_MAIN_ID.to_string());

    RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(restricted_paths)
        .with_file_path_changes(vec![
            ("restricted/one/a", None),
            ("user_project/foo/b", None),
        ])
        .expecting_manifest_id_store_entries(vec![
            RestrictedPathManifestIdEntry::new(
                ManifestType::Hg,
                expected_hg_manifest_id_user.clone(),
                RepoPath::dir("user_project/foo")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::Hg,
                expected_hg_manifest_id_restricted.clone(),
                RepoPath::dir("restricted/one")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::HgAugmented,
                expected_hg_manifest_id_restricted.clone(),
                RepoPath::dir("restricted/one")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::HgAugmented,
                expected_hg_manifest_id_user.clone(),
                RepoPath::dir("user_project/foo")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::Fsnode,
                expected_fsnode_id_restricted.clone(),
                RepoPath::dir("restricted/one")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::Fsnode,
                expected_fsnode_id_user.clone(),
                RepoPath::dir("user_project/foo")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::ContentManifest,
                expected_content_manifest_id_restricted.clone(),
                RepoPath::dir("restricted/one")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::ContentManifest,
                expected_content_manifest_id_user.clone(),
                RepoPath::dir("user_project/foo")?,
            )?,
        ])
        .expecting_scuba_access_logs(vec![
            // user_project/foo access - HgManifest log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["user_project/foo"])?)
                .with_manifest_id(expected_hg_manifest_id_user.clone())
                .with_manifest_type(ManifestType::Hg)
                // User had access to this restricted path
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![myusername_project_acl.clone()])
                .build()?,
            // user_project/foo access - HgAugmentedManifest log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["user_project/foo"])?)
                .with_manifest_id(expected_hg_manifest_id_user.clone())
                .with_manifest_type(ManifestType::HgAugmented)
                // User had access to this restricted path
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![myusername_project_acl.clone()])
                .build()?,
            // user_project/foo access - path log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["user_project/foo"])?)
                .with_full_path(NonRootMPath::new("user_project/foo")?)
                // User had access to this restricted path
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![myusername_project_acl.clone()])
                .build()?,
            // restricted/one access - HgManifest log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/one"])?)
                .with_manifest_id(expected_hg_manifest_id_restricted.clone())
                .with_manifest_type(ManifestType::Hg)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // restricted/one access - HgAugmentedManifest log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/one"])?)
                .with_manifest_id(expected_hg_manifest_id_restricted.clone())
                .with_manifest_type(ManifestType::HgAugmented)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // restricted/one access - path log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/one"])?)
                .with_full_path(NonRootMPath::new("restricted/one")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // user_project/foo access - ContentManifest log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["user_project/foo"])?)
                .with_manifest_id(expected_content_manifest_id_user.clone())
                .with_manifest_type(ManifestType::ContentManifest)
                // User had access to this restricted path
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![myusername_project_acl.clone()])
                .build()?,
            // restricted/one access - ContentManifest log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/one"])?)
                .with_manifest_id(expected_content_manifest_id_restricted.clone())
                .with_manifest_type(ManifestType::ContentManifest)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // restricted/one/.slacl access - path log (paths_with_content)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/one"])?)
                .with_full_path(NonRootMPath::new("restricted/one/.slacl")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // restricted/one/.slacl access - path log (paths_with_history)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/one"])?)
                .with_full_path(NonRootMPath::new("restricted/one/.slacl")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // user_project/foo/.slacl access - path log (paths_with_content)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["user_project/foo"])?)
                .with_full_path(NonRootMPath::new("user_project/foo/.slacl")?)
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![myusername_project_acl.clone()])
                .build()?,
            // user_project/foo/.slacl access - path log (paths_with_history)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["user_project/foo"])?)
                .with_full_path(NonRootMPath::new("user_project/foo/.slacl")?)
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![myusername_project_acl.clone()])
                .build()?,
            // user_project/foo access - path log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["user_project/foo"])?)
                .with_full_path(NonRootMPath::new("user_project/foo/b")?)
                // User had access to this restricted path
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![myusername_project_acl.clone()])
                .build()?,
            // restricted/one access - path log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/one"])?)
                .with_full_path(NonRootMPath::new("restricted/one/a")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // user_project/foo access - path log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["user_project/foo"])?)
                .with_full_path(NonRootMPath::new("user_project/foo/b")?)
                // User had access to this restricted path
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![myusername_project_acl.clone()])
                .build()?,
            // restricted/one access - path log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/one"])?)
                .with_full_path(NonRootMPath::new("restricted/one/a")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
        ])
        .build(fb)
        .await?
        .run_restricted_paths_test()
        .await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_overlapping_restricted_directories(fb: FacebookInit) -> Result<()> {
    // Set up overlapping restricted paths: project/restricted is nested inside project
    let more_restricted_acl = MononokeIdentity::from_str("REPO_REGION:more_restricted_acl")?;
    let project_acl = MononokeIdentity::from_str("REPO_REGION:project_acl")?;
    let restricted_paths = vec![
        (
            NonRootMPath::new("project/restricted")?,
            more_restricted_acl.clone(),
        ),
        (NonRootMPath::new("project")?, project_acl.clone()),
    ];

    let expected_hg_manifest_id_root =
        RestrictedManifestId::from("63675f7e03dfbed9682168b0b240656b297726b6");
    let expected_hg_manifest_id_subdir =
        RestrictedManifestId::from("25ea6eb33a35462d27c75749c8979f94b8e43e18");

    let expected_fsnode_id_root = RestrictedManifestId::from(
        "bf02d14ea3777b2c43028dc4bc2d4cb55c46f83158cc872301f905a8403cb264",
    );
    let expected_fsnode_id_subdir = RestrictedManifestId::from(
        "679b2810512fe1b9c9bd454f0a6975664e2c63d0a212dc6061cf5a965b830172",
    );

    let expected_content_manifest_id_root = RestrictedManifestId::from(
        "8f325f343ded73f773c4f0a65a24a106af5d3a799f63929caf6fd72bbc06dfc5",
    );
    let expected_content_manifest_id_subdir = RestrictedManifestId::from(
        "b97da0af6341ecf430981ad7f699fd6c21f671cc2e784884b4886aa800a8c7ff",
    );

    // Base sample with fields common to ALL expected samples
    let base_sample = ScubaAccessLogSampleBuilder::new()
        .with_repo_id(RepositoryId::new(0))
        .with_client_identities(
            vec!["USER:myusername0"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        )
        .with_client_main_id(TEST_CLIENT_MAIN_ID.to_string());

    // Access a file in the more restricted nested path - this should trigger both ACL checks
    // Custom ACL that gives access to project but NOT to project/restricted
    RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(restricted_paths)
        .with_test_repo_region_acls(vec![
            ("project_acl", vec!["myusername0"]),
            ("more_restricted_acl", vec!["other_user"]),
        ])
        .with_file_path_changes(vec![("project/restricted/sensitive_file.txt", None)])
        .expecting_manifest_id_store_entries(vec![
            RestrictedPathManifestIdEntry::new(
                ManifestType::HgAugmented,
                expected_hg_manifest_id_root.clone(),
                RepoPath::dir("project")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::HgAugmented,
                expected_hg_manifest_id_subdir.clone(),
                RepoPath::dir("project/restricted")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::Hg,
                expected_hg_manifest_id_root.clone(),
                RepoPath::dir("project")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::Hg,
                expected_hg_manifest_id_subdir.clone(),
                RepoPath::dir("project/restricted")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::Fsnode,
                expected_fsnode_id_root.clone(),
                RepoPath::dir("project")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::Fsnode,
                expected_fsnode_id_subdir.clone(),
                RepoPath::dir("project/restricted")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::ContentManifest,
                expected_content_manifest_id_root.clone(),
                RepoPath::dir("project")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::ContentManifest,
                expected_content_manifest_id_subdir.clone(),
                RepoPath::dir("project/restricted")?,
            )?,
        ])
        .expecting_scuba_access_logs(vec![
            // project access - HgManifest log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["project"])?)
                .with_manifest_id(expected_hg_manifest_id_root.clone())
                .with_manifest_type(ManifestType::Hg)
                // User has access to the broader project ACL
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![project_acl.clone()])
                .build()?,
            // project access - HgAugmentedManifest log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["project"])?)
                .with_manifest_id(expected_hg_manifest_id_root.clone())
                .with_manifest_type(ManifestType::HgAugmented)
                // User has access to the broader project ACL
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![project_acl.clone()])
                .build()?,
            // project access - path log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["project"])?)
                .with_full_path(NonRootMPath::new("project")?)
                // User has access to the broader project ACL
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![project_acl.clone()])
                .build()?,
            // project/restricted access - HgManifest log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["project/restricted"])?)
                .with_manifest_id(expected_hg_manifest_id_subdir.clone())
                .with_manifest_type(ManifestType::Hg)
                // User does NOT have access to the more restricted ACL
                .with_has_authorization(false)
                .with_acls(vec![more_restricted_acl.clone()])
                .build()?,
            // project/restricted access - HgAugmentedManifest log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["project/restricted"])?)
                .with_manifest_id(expected_hg_manifest_id_subdir.clone())
                .with_manifest_type(ManifestType::HgAugmented)
                // User does NOT have access to the more restricted ACL
                .with_has_authorization(false)
                .with_acls(vec![more_restricted_acl.clone()])
                .build()?,
            // project/restricted access - path log (includes both ACLs).
            // Conjunctive: user lacks more_restricted_acl, so access is denied.
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec![
                    "project",
                    "project/restricted",
                ])?)
                .with_full_path(NonRootMPath::new("project/restricted")?)
                .with_has_authorization(false)
                .with_acls(vec![more_restricted_acl.clone(), project_acl.clone()])
                .build()?,
            // project access - ContentManifest log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["project"])?)
                .with_manifest_id(expected_content_manifest_id_root.clone())
                .with_manifest_type(ManifestType::ContentManifest)
                // User has access to the broader project ACL
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![project_acl.clone()])
                .build()?,
            // project/restricted access - ContentManifest log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["project/restricted"])?)
                .with_manifest_id(expected_content_manifest_id_subdir.clone())
                .with_manifest_type(ManifestType::ContentManifest)
                // User has access to the broader project ACL
                .with_has_authorization(false)
                .with_acls(vec![more_restricted_acl.clone()])
                .build()?,
            // project/.slacl access - path log (paths_with_content)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["project"])?)
                .with_full_path(NonRootMPath::new("project/.slacl")?)
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![project_acl.clone()])
                .build()?,
            // project/.slacl access - path log (paths_with_history)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["project"])?)
                .with_full_path(NonRootMPath::new("project/.slacl")?)
                .with_has_authorization(true)
                .with_has_acl_access(true)
                .with_acls(vec![project_acl.clone()])
                .build()?,
            // project/restricted/.slacl access - path log (paths_with_content).
            // Conjunctive: user lacks more_restricted_acl, so access is denied.
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec![
                    "project",
                    "project/restricted",
                ])?)
                .with_full_path(NonRootMPath::new("project/restricted/.slacl")?)
                .with_has_authorization(false)
                .with_acls(vec![more_restricted_acl.clone(), project_acl.clone()])
                .build()?,
            // project/restricted/.slacl access - path log (paths_with_history).
            // Conjunctive: user lacks more_restricted_acl, so access is denied.
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec![
                    "project",
                    "project/restricted",
                ])?)
                .with_full_path(NonRootMPath::new("project/restricted/.slacl")?)
                .with_has_authorization(false)
                .with_acls(vec![more_restricted_acl.clone(), project_acl.clone()])
                .build()?,
            // project/restricted access - path log (includes both ACLs).
            // Conjunctive: user lacks more_restricted_acl, so access is denied.
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec![
                    "project",
                    "project/restricted",
                ])?)
                .with_full_path(NonRootMPath::new("project/restricted/sensitive_file.txt")?)
                .with_has_authorization(false)
                .with_acls(vec![more_restricted_acl.clone(), project_acl.clone()])
                .build()?,
            // project/restricted access - path log (includes both ACLs).
            // Conjunctive: user lacks more_restricted_acl, so access is denied.
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec![
                    "project",
                    "project/restricted",
                ])?)
                .with_full_path(NonRootMPath::new("project/restricted/sensitive_file.txt")?)
                .with_has_authorization(false)
                .with_acls(vec![more_restricted_acl.clone(), project_acl.clone()])
                .build()?,
        ])
        .build(fb)
        .await?
        .run_restricted_paths_test()
        .await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_same_manifest_id_restricted_and_unrestricted_paths(fb: FacebookInit) -> Result<()> {
    // Set up a restricted path for the "restricted" directory
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let restricted_paths = vec![(NonRootMPath::new("restricted")?, restricted_acl.clone())];

    // Create two files with the same content in directories under restricted and unrestricted paths:
    // - restricted/foo/bar (under restricted path)
    // - unrestricted/foo/bar (not under restricted path)
    // With .slacl in a parent commit, restricted/foo has a .slacl sibling from the parent
    // while unrestricted/foo does not, so they have DIFFERENT manifest IDs despite identical
    // file content. The test verifies the restricted path still generates access logs while
    // the unrestricted one does not.
    let identical_content = "same file content";

    let expected_manifest_id =
        RestrictedManifestId::from("aeffdc50909f33507a28256c9f14fd98fd60ae63");
    let expected_fsnode_id = RestrictedManifestId::from(
        "ba852d4e20076271ddecbc295aa4f940984397ee8d94ec690e663b7131a38edb",
    );

    let expected_content_manifest_id = RestrictedManifestId::from(
        "bdfd42e3bdebd384ad44cece90309fc5f18051d6d062cda7b76fc303bed8a1df",
    );

    // Base sample with fields common to ALL expected samples
    let base_sample = ScubaAccessLogSampleBuilder::new()
        .with_repo_id(RepositoryId::new(0))
        .with_client_identities(
            vec!["USER:myusername0"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        )
        .with_client_main_id(TEST_CLIENT_MAIN_ID.to_string());

    RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(restricted_paths)
        .with_file_path_changes(vec![
            ("restricted/foo/bar", Some(identical_content)),
            ("unrestricted/foo/bar", Some(identical_content)),
        ])
        .expecting_manifest_id_store_entries(vec![
            RestrictedPathManifestIdEntry::new(
                ManifestType::Hg,
                expected_manifest_id.clone(),
                RepoPath::dir("restricted")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::HgAugmented,
                expected_manifest_id.clone(),
                RepoPath::dir("restricted")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::Fsnode,
                expected_fsnode_id.clone(),
                RepoPath::dir("restricted")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::ContentManifest,
                expected_content_manifest_id.clone(),
                RepoPath::dir("restricted")?,
            )?,
        ])
        .expecting_scuba_access_logs(vec![
            // Path access logs - for directories traversed
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted"])?)
                .with_full_path(NonRootMPath::new("restricted")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted"])?)
                .with_full_path(NonRootMPath::new("restricted/foo")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // .slacl path access (paths_with_content)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted"])?)
                .with_full_path(NonRootMPath::new("restricted/.slacl")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // .slacl path access (paths_with_history)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted"])?)
                .with_full_path(NonRootMPath::new("restricted/.slacl")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // Path access logs for restricted/foo/bar (paths_with_content and paths_with_history)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted"])?)
                .with_full_path(NonRootMPath::new("restricted/foo/bar")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted"])?)
                .with_full_path(NonRootMPath::new("restricted/foo/bar")?)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // HgManifest access log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted"])?)
                .with_manifest_id(expected_manifest_id.clone())
                .with_manifest_type(ManifestType::Hg)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // HgAugmentedManifest access log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted"])?)
                .with_manifest_id(expected_manifest_id.clone())
                .with_manifest_type(ManifestType::HgAugmented)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // ContentManifest access log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted"])?)
                .with_manifest_id(expected_content_manifest_id.clone())
                .with_manifest_type(ManifestType::ContentManifest)
                .with_has_authorization(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
        ])
        .build(fb)
        .await?
        .run_restricted_paths_test()
        .await?;

    Ok(())
}

// Test that is_allowlisted_tooling is set to true when the client is in the
// tooling allowlist group.
#[mononoke::fbinit_test]
async fn test_tooling_allowlist_acl_user_in_acl(fb: FacebookInit) -> Result<()> {
    // Service myservice0 has access to the tooling_group
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let restricted_paths = vec![(NonRootMPath::new("restricted/dir")?, restricted_acl.clone())];

    let expected_manifest_id =
        RestrictedManifestId::from("341074482e5d30e3afb06cb4c89e758821073296");
    let expected_fsnode_id = RestrictedManifestId::from(
        "2d3027385add91b3b1f68187c99b03fe464d8539dfc80507005ff690bd4740bb",
    );

    let expected_content_manifest_id = RestrictedManifestId::from(
        "9f452248a768ac52e6e32863c5fd983fa76e9d2952ba9fb449dac7044ec46c7c",
    );

    // Base sample with fields common to ALL expected samples
    let base_sample = ScubaAccessLogSampleBuilder::new()
        .with_repo_id(RepositoryId::new(0))
        .with_client_identities(
            vec!["SERVICE_IDENTITY:myservice0"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        )
        .with_client_main_id("service_identity:myservice0".to_string());

    RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(restricted_paths)
        .with_client_identity("SERVICE_IDENTITY:myservice0")?
        .with_tooling_allowlist_group("tooling_group")
        .with_test_groups(vec![("tooling_group", vec!["SERVICE_IDENTITY:myservice0"])])
        .with_test_repo_region_acls(vec![("restricted_acl", vec!["other_user"])])
        .with_file_path_changes(vec![("restricted/dir/a", None)])
        .expecting_manifest_id_store_entries(vec![
            RestrictedPathManifestIdEntry::new(
                ManifestType::Hg,
                expected_manifest_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::HgAugmented,
                expected_manifest_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::Fsnode,
                expected_fsnode_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::ContentManifest,
                expected_content_manifest_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
        ])
        .expecting_scuba_access_logs(vec![
            // HgManifest access log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_manifest_id(expected_manifest_id.clone())
                .with_manifest_type(ManifestType::Hg)
                // Client HAS authorization because they are in the tooling allowlist
                .with_has_authorization(true)
                // Client IS in the tooling allowlist
                .with_is_allowlisted_tooling(true)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // HgAugmentedManifest access log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_manifest_id(expected_manifest_id.clone())
                .with_manifest_type(ManifestType::HgAugmented)
                .with_has_authorization(true)
                .with_is_allowlisted_tooling(true)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // Path access log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir")?)
                .with_has_authorization(true)
                .with_is_allowlisted_tooling(true)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // ContentManifest access log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_manifest_id(expected_content_manifest_id.clone())
                .with_manifest_type(ManifestType::ContentManifest)
                .with_has_authorization(true)
                .with_is_allowlisted_tooling(true)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // .slacl path access (paths_with_content)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/.slacl")?)
                .with_has_authorization(true)
                .with_is_allowlisted_tooling(true)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // .slacl path access (paths_with_history)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/.slacl")?)
                .with_has_authorization(true)
                .with_is_allowlisted_tooling(true)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // Path content access log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/a")?)
                .with_has_authorization(true)
                .with_is_allowlisted_tooling(true)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/a")?)
                .with_has_authorization(true)
                .with_is_allowlisted_tooling(true)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
        ])
        .build(fb)
        .await?
        .run_restricted_paths_test()
        .await?;

    Ok(())
}

// Test that is_allowlisted_tooling is set to false when the client is NOT in the
// tooling allowlist group.
#[mononoke::fbinit_test]
async fn test_tooling_allowlist_acl_user_not_in_acl(fb: FacebookInit) -> Result<()> {
    // Service myservice0 does NOT have access to the tooling_group (only other_service does)
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let restricted_paths = vec![(NonRootMPath::new("restricted/dir")?, restricted_acl.clone())];

    let expected_manifest_id =
        RestrictedManifestId::from("341074482e5d30e3afb06cb4c89e758821073296");
    let expected_fsnode_id = RestrictedManifestId::from(
        "2d3027385add91b3b1f68187c99b03fe464d8539dfc80507005ff690bd4740bb",
    );

    let expected_content_manifest_id = RestrictedManifestId::from(
        "9f452248a768ac52e6e32863c5fd983fa76e9d2952ba9fb449dac7044ec46c7c",
    );

    // Base sample with fields common to ALL expected samples
    let base_sample = ScubaAccessLogSampleBuilder::new()
        .with_repo_id(RepositoryId::new(0))
        .with_client_identities(
            vec!["SERVICE_IDENTITY:myservice0"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        )
        .with_client_main_id("service_identity:myservice0".to_string());

    RestrictedPathsTestDataBuilder::new()
        .with_restricted_paths(restricted_paths)
        .with_client_identity("SERVICE_IDENTITY:myservice0")?
        .with_tooling_allowlist_group("tooling_group")
        // myservice0 is NOT in the tooling_group
        .with_test_groups(vec![(
            "tooling_group",
            vec!["SERVICE_IDENTITY:other_service"],
        )])
        .with_test_repo_region_acls(vec![("restricted_acl", vec!["other_user"])])
        .with_file_path_changes(vec![("restricted/dir/a", None)])
        .expecting_manifest_id_store_entries(vec![
            RestrictedPathManifestIdEntry::new(
                ManifestType::Hg,
                expected_manifest_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::HgAugmented,
                expected_manifest_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::Fsnode,
                expected_fsnode_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
            RestrictedPathManifestIdEntry::new(
                ManifestType::ContentManifest,
                expected_content_manifest_id.clone(),
                RepoPath::dir("restricted/dir")?,
            )?,
        ])
        .expecting_scuba_access_logs(vec![
            // HgManifest access log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_manifest_id(expected_manifest_id.clone())
                .with_manifest_type(ManifestType::Hg)
                // Client does NOT have authorization to the restricted path
                .with_has_authorization(false)
                // Client is NOT in the tooling allowlist
                .with_is_allowlisted_tooling(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // HgAugmentedManifest access log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_manifest_id(expected_manifest_id.clone())
                .with_manifest_type(ManifestType::HgAugmented)
                .with_has_authorization(false)
                .with_is_allowlisted_tooling(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // Path access log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir")?)
                .with_has_authorization(false)
                .with_is_allowlisted_tooling(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // ContentManifest access log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_manifest_id(expected_content_manifest_id.clone())
                .with_manifest_type(ManifestType::ContentManifest)
                .with_has_authorization(false)
                .with_is_allowlisted_tooling(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // .slacl path access (paths_with_content)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/.slacl")?)
                .with_has_authorization(false)
                .with_is_allowlisted_tooling(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // .slacl path access (paths_with_history)
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/.slacl")?)
                .with_has_authorization(false)
                .with_is_allowlisted_tooling(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            // Path content access log
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/a")?)
                .with_has_authorization(false)
                .with_is_allowlisted_tooling(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
            base_sample
                .clone()
                .with_restricted_paths(cast_to_non_root_mpaths(vec!["restricted/dir"])?)
                .with_full_path(NonRootMPath::new("restricted/dir/a")?)
                .with_has_authorization(false)
                .with_is_allowlisted_tooling(false)
                .with_acls(vec![restricted_acl.clone()])
                .build()?,
        ])
        .build(fb)
        .await?
        .run_restricted_paths_test()
        .await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_acl_manifest_mode_without_derivation_enabled_fails(fb: FacebookInit) -> Result<()> {
    use context::CoreContext;
    use mononoke_api::Repo as TestRepo;
    use mononoke_types::DerivableType;
    use test_repo_factory::TestRepoFactory;

    let ctx = CoreContext::test_mock(fb);

    let mut factory = TestRepoFactory::new(ctx.fb)?;
    factory.with_config_override(|repo_config| {
        repo_config.restricted_paths_config.acl_manifest_mode = AclManifestMode::Shadow;
        let dd_config = repo_config
            .derived_data_config
            .get_active_config_mut()
            .expect("No enabled derived data types config");
        dd_config.types.remove(&DerivableType::AclManifests);
    });
    // Build should fail because acl_manifest_mode is enabled but AclManifests is not enabled.
    let result = factory.build::<TestRepo>().await;

    let err = result.err().ok_or_else(|| {
        anyhow::anyhow!(
            "Expected error when acl_manifest_mode is enabled but AclManifest derivation is not enabled"
        )
    })?;

    let chain_contains =
        std::iter::successors(Some(&err as &dyn std::error::Error), |e| e.source()).any(|e| {
            e.to_string()
                .contains("AclManifest derivation is not enabled")
        });
    assert!(
        chain_contains,
        "Error chain should mention AclManifest derivation, got: {err:?}"
    );

    Ok(())
}

// What it tests: Shadow path dispatch can run config and AclManifest sources side by side.
// Expected: compact mismatch fields are present while config remains authoritative.
#[mononoke::fbinit_test]
async fn test_shadow_path_dispatch_logs_config_and_acl_manifest_sources(
    fb: FacebookInit,
) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;

    let restricted_root = NonRootMPath::new("restricted/dir")?;
    let result = RestrictedPathsTestDataBuilder::new()
        .with_acl_manifest_mode(AclManifestMode::Shadow)
        .with_config_restricted_paths(vec![(restricted_root.clone(), restricted_acl.clone())])
        .with_acl_manifest_restricted_paths(vec![(restricted_root.clone(), restricted_acl)])
        .with_file_path_changes(vec![("restricted/dir/a", None)])
        .build(fb)
        .await?
        .observe_restricted_paths_scenario(&[])
        .await?;

    assert!(
        result.scuba_logs.iter().any(|log| {
            log.full_path() == Some(&restricted_root)
                && log.acl_manifest_mode() == Some("shadow")
                && !log.has_authorization()
                && log.shadow_mismatch() == Some(false)
                && log.shadow_mismatch_detail().is_none()
                && log.considered_restricted_by()
                    == ["acl_manifest".to_string(), "manifest_db".to_string()]
        }),
        "expected a Shadow path log with matching config and AclManifest sources: {:#?}",
        result.scuba_logs
    );
    Ok(())
}

// What it tests: Shadow path dispatch emits comparison rows for AclManifest-only restrictions.
// Expected: config aggregate fields stay unrestricted while the mismatch summary reports the restriction.
#[mononoke::fbinit_test]
async fn test_shadow_path_dispatch_logs_acl_manifest_only_restriction(
    fb: FacebookInit,
) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;

    let restricted_root = NonRootMPath::new("acl_manifest_only/dir")?;
    let result = RestrictedPathsTestDataBuilder::new()
        .with_acl_manifest_mode(AclManifestMode::Shadow)
        .with_acl_manifest_restricted_paths(vec![(restricted_root.clone(), restricted_acl)])
        .with_file_path_changes(vec![("acl_manifest_only/dir/a", None)])
        .build(fb)
        .await?
        .observe_restricted_paths_scenario(&[])
        .await?;

    assert!(
        result.scuba_logs.iter().any(|log| {
            log.full_path() == Some(&restricted_root)
                && log.has_authorization()
                && log.shadow_mismatch() == Some(true)
                && log.shadow_mismatch_detail().is_some()
                && log.considered_restricted_by() == ["acl_manifest".to_string()]
        }),
        "expected an AclManifest-only Shadow comparison row: {:#?}",
        result.scuba_logs
    );
    Ok(())
}

// What it tests: Shadow path dispatch handles comparison-source lookup errors as logging-only data.
// Expected: AclManifest errors populate acl_manifest_error and mark the row for investigation
// without changing request success.
#[mononoke::fbinit_test]
async fn test_shadow_path_dispatch_logs_comparison_errors(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;

    let restricted_root = NonRootMPath::new("restricted/dir")?;
    let missing_cs_id =
        ChangesetId::from_str("1111111111111111111111111111111111111111111111111111111111111111")?;
    let result = RestrictedPathsTestDataBuilder::new()
        .with_acl_manifest_mode(AclManifestMode::Shadow)
        .with_config_restricted_paths(vec![(restricted_root.clone(), restricted_acl)])
        .build(fb)
        .await?
        .observe_path_access(restricted_root.clone(), Some(missing_cs_id), &[])
        .await?;

    assert!(
        result.scuba_logs.iter().any(|log| {
            log.full_path() == Some(&restricted_root)
                && log.acl_manifest_mode() == Some("shadow")
                && !log.has_authorization()
                && log.acl_manifest_error().is_some()
                && log.shadow_mismatch() == Some(true)
                && log.shadow_mismatch_detail().is_some()
                && log.access_enforcement_enabled().is_none()
                && log.considered_restricted_by() == ["manifest_db".to_string()]
        }),
        "expected Shadow path logging to capture AclManifest comparison errors: {:#?}",
        result.scuba_logs
    );
    Ok(())
}

// What it tests: Shadow path dispatch skips AclManifest comparison when a changeset id is unavailable.
// Expected: skipped comparison leaves acl_manifest_error and mismatch detail empty.
#[mononoke::fbinit_test]
async fn test_shadow_path_dispatch_skips_acl_manifest_without_changeset(
    fb: FacebookInit,
) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;

    let restricted_root = NonRootMPath::new("restricted/dir")?;
    let result = RestrictedPathsTestDataBuilder::new()
        .with_acl_manifest_mode(AclManifestMode::Shadow)
        .with_config_restricted_paths(vec![(restricted_root.clone(), restricted_acl.clone())])
        .with_acl_manifest_restricted_paths(vec![(restricted_root.clone(), restricted_acl)])
        .build(fb)
        .await?
        .observe_path_access(restricted_root.clone(), None, &[])
        .await?;

    assert!(
        result.scuba_logs.iter().any(|log| {
            log.full_path() == Some(&restricted_root)
                && log.acl_manifest_mode() == Some("shadow")
                && !log.has_authorization()
                && log.acl_manifest_error().is_none()
                && log.shadow_mismatch() == Some(false)
                && log.shadow_mismatch_detail().is_none()
                && log.considered_restricted_by() == ["manifest_db".to_string()]
        }),
        "expected Shadow path logging to skip AclManifest without a changeset: {:#?}",
        result.scuba_logs
    );
    Ok(())
}

// What it tests: Shadow path dispatch does not log unrestricted rows.
// Expected: no row is emitted when both sources are unrestricted.
#[mononoke::fbinit_test]
async fn test_shadow_path_dispatch_omits_unrestricted_rows(fb: FacebookInit) -> Result<()> {
    let result = RestrictedPathsTestDataBuilder::new()
        .with_acl_manifest_mode(AclManifestMode::Shadow)
        .with_file_path_changes(vec![("unrestricted/dir/a", None)])
        .build(fb)
        .await?
        .observe_restricted_paths_scenario(&[])
        .await?;

    assert!(
        result.scuba_logs.is_empty(),
        "expected no Shadow rows for unrestricted paths: {:#?}",
        result.scuba_logs
    );
    Ok(())
}

// What it tests: Shadow manifest dispatch can run config and AclManifest sources side by side for HgAugmented manifests.
// Expected: HgAugmented manifest rows expose source details while config remains authoritative.
#[mononoke::fbinit_test]
async fn test_shadow_manifest_dispatch_logs_hg_augmented_sources(fb: FacebookInit) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let restricted_root = NonRootMPath::new("restricted/dir")?;

    let result = RestrictedPathsTestDataBuilder::new()
        .with_acl_manifest_mode(AclManifestMode::Shadow)
        .with_config_restricted_paths(vec![(restricted_root.clone(), restricted_acl.clone())])
        .with_acl_manifest_restricted_paths(vec![(restricted_root.clone(), restricted_acl)])
        .with_file_path_changes(vec![("restricted/dir/a", None)])
        .build(fb)
        .await?
        .observe_restricted_paths_scenario(&[])
        .await?;

    assert!(
        result.scuba_logs.iter().any(|log| {
            log.manifest_type() == Some(&ManifestType::HgAugmented)
                && log.acl_manifest_mode() == Some("shadow")
                && !log.has_authorization()
                && log.shadow_mismatch() == Some(false)
                && log.shadow_mismatch_detail().is_some()
                && log.considered_restricted_by()
                    == ["acl_manifest".to_string(), "manifest_db".to_string()]
        }),
        "expected an HgAugmented Shadow manifest row with compact comparison fields: {:#?}",
        result.scuba_logs
    );
    Ok(())
}

// What it tests: Shadow manifest dispatch emits comparison rows for AclManifest-only restrictions.
// Expected: config aggregate fields stay unrestricted while the mismatch summary reports the restriction.
#[mononoke::fbinit_test]
async fn test_shadow_manifest_dispatch_logs_acl_manifest_only_restriction(
    fb: FacebookInit,
) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let restricted_root = NonRootMPath::new("restricted/dir")?;
    let manifest_id = RestrictedManifestId::from("341074482e5d30e3afb06cb4c89e758821073296");

    let result = RestrictedPathsTestDataBuilder::new()
        .with_acl_manifest_mode(AclManifestMode::Shadow)
        .with_acl_manifest_restricted_paths(vec![(restricted_root.clone(), restricted_acl)])
        .with_file_path_changes(vec![("restricted/dir/a", None)])
        .build(fb)
        .await?
        .observe_manifest_access(manifest_id.clone(), ManifestType::HgAugmented, None, &[])
        .await?;

    assert!(
        result.scuba_logs.iter().any(|log| {
            log.manifest_type() == Some(&ManifestType::HgAugmented)
                && log.manifest_id() == Some(&manifest_id)
                && log.acl_manifest_mode() == Some("shadow")
                && log.has_authorization()
                && log.acl_manifest_error().is_none()
                && log.shadow_mismatch() == Some(true)
                && log.shadow_mismatch_detail().is_some()
                && log.considered_restricted_by() == ["acl_manifest".to_string()]
        }),
        "expected an AclManifest-only Shadow manifest comparison row: {:#?}",
        result.scuba_logs
    );
    Ok(())
}

// What it tests: Shadow manifest dispatch skips AclManifest comparison for unsupported manifest types.
// Expected: unsupported manifest types keep config-only compact logging fields.
#[mononoke::fbinit_test]
async fn test_shadow_manifest_dispatch_skips_unsupported_manifest_types(
    fb: FacebookInit,
) -> Result<()> {
    let restricted_acl = MononokeIdentity::from_str("REPO_REGION:restricted_acl")?;
    let restricted_root = NonRootMPath::new("restricted/dir")?;

    let result = RestrictedPathsTestDataBuilder::new()
        .with_acl_manifest_mode(AclManifestMode::Shadow)
        .with_config_restricted_paths(vec![(restricted_root.clone(), restricted_acl.clone())])
        .with_acl_manifest_restricted_paths(vec![(restricted_root, restricted_acl)])
        .with_file_path_changes(vec![("restricted/dir/a", None)])
        .build(fb)
        .await?
        .observe_restricted_paths_scenario(&[])
        .await?;

    for manifest_type in [ManifestType::Hg, ManifestType::ContentManifest] {
        assert!(
            result.scuba_logs.iter().any(|log| {
                log.manifest_type() == Some(&manifest_type)
                    && log.acl_manifest_mode() == Some("shadow")
                    && !log.has_authorization()
                    && log.acl_manifest_error().is_none()
                    && log.shadow_mismatch() == Some(false)
                    && log.shadow_mismatch_detail().is_none()
                    && log.considered_restricted_by() == ["manifest_db".to_string()]
            }),
            "expected unsupported manifest type {manifest_type:?} to skip AclManifest comparison: {:#?}",
            result.scuba_logs
        );
    }
    Ok(())
}

// What it tests: Shadow manifest dispatch handles comparison-source lookup errors as logging-only data.
// Expected: AclManifest manifest errors populate acl_manifest_error and mark the row for
// investigation without changing request success.
#[mononoke::fbinit_test]
async fn test_shadow_manifest_dispatch_logs_comparison_errors(fb: FacebookInit) -> Result<()> {
    let result = RestrictedPathsTestDataBuilder::new()
        .with_acl_manifest_mode(AclManifestMode::Shadow)
        .build(fb)
        .await?
        .observe_manifest_access(
            RestrictedManifestId::from("not-a-valid-hg-augmented-manifest-id"),
            ManifestType::HgAugmented,
            None,
            &[],
        )
        .await?;

    assert!(
        result.scuba_logs.iter().any(|log| {
            log.manifest_type() == Some(&ManifestType::HgAugmented)
                && log.acl_manifest_mode() == Some("shadow")
                && log.has_authorization()
                && log.acl_manifest_error().is_some()
                && log.shadow_mismatch() == Some(true)
                && log.shadow_mismatch_detail().is_some()
                && log.considered_restricted_by().is_empty()
        }),
        "expected Shadow manifest logging to capture AclManifest comparison errors: {:#?}",
        result.scuba_logs
    );
    Ok(())
}

// What it tests: Shadow manifest dispatch does not log unrestricted rows.
// Expected: no manifest row is emitted when both sources are unrestricted.
#[mononoke::fbinit_test]
async fn test_shadow_manifest_dispatch_omits_unrestricted_rows(fb: FacebookInit) -> Result<()> {
    let result = RestrictedPathsTestDataBuilder::new()
        .with_acl_manifest_mode(AclManifestMode::Shadow)
        .with_file_path_changes(vec![("unrestricted/dir/a", None)])
        .build(fb)
        .await?
        .observe_restricted_paths_scenario(&[])
        .await?;

    assert!(
        result.scuba_logs.is_empty(),
        "expected no Shadow rows for unrestricted manifest accesses: {:#?}",
        result.scuba_logs
    );
    Ok(())
}

/// What it tests: Both-mode combined checks when the AclManifest source cannot
/// be read: no changeset id or a missing changeset for path checks, and an
/// invalid manifest id for the manifest check.
/// Expected: every combined check returns an error instead of answering from
/// the config source alone.
#[mononoke::fbinit_test]
async fn test_both_mode_combined_checks_fail_closed_without_acl_manifest(
    fb: FacebookInit,
) -> Result<()> {
    use context::CoreContext;
    use mononoke_types::MPath;

    let repo = RestrictedPathsTestDataBuilder::new()
        .with_config_restricted_paths(vec![(
            NonRootMPath::new("restricted/dir")?,
            MononokeIdentity::from_str("REPO_REGION:restricted_acl")?,
        )])
        .with_acl_manifest_mode(AclManifestMode::Both)
        .build(fb)
        .await?
        .build_repo_only()
        .await?;
    let ctx = CoreContext::test_mock(fb);
    let restricted_paths = repo.restricted_paths();
    let root = NonRootMPath::new("restricted/dir")?;
    let path = NonRootMPath::new("restricted/dir/file")?;
    let missing_changeset =
        ChangesetId::from_str("1111111111111111111111111111111111111111111111111111111111111111")?;

    for cs_id in [None, Some(missing_changeset)] {
        assert!(
            restricted_paths
                .get_path_restriction_check(&ctx, cs_id, std::slice::from_ref(&path))
                .await
                .is_err(),
            "path check should fail without AclManifest data for {cs_id:?}",
        );
        assert!(
            restricted_paths
                .get_path_restriction_root_check(&ctx, cs_id, std::slice::from_ref(&root))
                .await
                .is_err(),
            "root check should fail without AclManifest data for {cs_id:?}",
        );
        assert!(
            restricted_paths
                .find_restricted_descendant_checks(&ctx, cs_id, vec![MPath::ROOT])
                .await
                .is_err(),
            "descendant check should fail without AclManifest data for {cs_id:?}",
        );
    }
    assert!(
        restricted_paths
            .get_manifest_restriction_check(
                &ctx,
                &RestrictedManifestId::from("not-a-valid-hg-augmented-manifest-id"),
                &ManifestType::HgAugmented,
            )
            .await
            .is_err(),
        "manifest check should fail without AclManifest data",
    );

    Ok(())
}

fn restricted_paths_justknobs(overrides: &[(&str, bool)]) -> JustKnobsInMemory {
    JustKnobsInMemory::new(
        overrides
            .iter()
            .map(|(knob, value)| ((*knob).to_string(), KnobVal::Bool(*value)))
            .collect(),
    )
}
