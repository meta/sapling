/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Result;
use context::CoreContext;
use context::SessionContainer;
use fbinit::FacebookInit;
use metaconfig_types::EnforcementConditionSet;
use metaconfig_types::RequestMatchers;
use metadata::Metadata;
use mononoke_macros::mononoke;
use mononoke_types::NonRootMPath;
use permission_checker::AclProvider;
use permission_checker::Acls;
use permission_checker::InternalAclProvider;
use permission_checker::MononokeIdentity;
use permission_checker::MononokeIdentitySet;

use super::AccessEnforcementOutcome;
use super::AuthorizationCheckResult;
use super::EnforcementDecision;
use super::FactorResult;
use super::ManifestRestrictionCheckResult;
use super::PathRestrictionCheckResult;
use super::PreFilterVariant;
use super::SharedFetchHandle;
use super::SourceRestrictionCheck;
use super::SourceRestrictionSummary;
use crate::DenyingRestriction;
use crate::restriction_info::ManifestRestrictionInfo;
use crate::restriction_info::PathRestrictionInfo;

/// What it tests: tri-state factor observations preserve the distinction
/// between an absent policy and a configured policy that denies the caller.
/// Expected: only `Authorized` converts to the legacy `true` value, while the
/// three states remain pairwise distinct.
#[mononoke::test]
fn test_factor_result_preserves_not_set() -> Result<()> {
    let cases = [
        (FactorResult::NotSet, false),
        (FactorResult::Authorized, true),
        (FactorResult::Unauthorized, false),
    ];

    for (result, expected_authorized) in cases {
        assert_eq!(
            result.is_authorized(),
            expected_authorized,
            "factor result {result:#?}",
        );
    }
    assert_ne!(
        FactorResult::NotSet,
        FactorResult::Unauthorized,
        "absent policy and configured denial must remain distinct",
    );

    let optional_cases = [
        (false, false, FactorResult::NotSet),
        (false, true, FactorResult::NotSet),
        (true, false, FactorResult::Unauthorized),
        (true, true, FactorResult::Authorized),
    ];
    for (configured, authorized, expected) in optional_cases {
        assert_eq!(
            FactorResult::from_optional_authorized(configured, authorized),
            expected,
            "configured={configured}, authorized={authorized}",
        );
    }
    Ok(())
}

/// What it tests: the existing authorization formula derives its public
/// booleans from tri-state factor observations.
/// Expected: each authorized factor grants independently, while `NotSet` and
/// `Unauthorized` do not grant.
#[mononoke::test]
fn test_authorization_check_result_tri_state_formula() -> Result<()> {
    let cases = [
        (FactorResult::NotSet, false),
        (FactorResult::Authorized, true),
        (FactorResult::Unauthorized, false),
    ];
    for (factor, expected) in cases {
        let acl = AuthorizationCheckResult {
            has_acl_access: factor,
            is_allowlisted_tooling: FactorResult::NotSet,
            is_rollout_allowlisted: FactorResult::NotSet,
            is_admin_bypass: FactorResult::NotSet,
        };
        let tooling = AuthorizationCheckResult {
            has_acl_access: FactorResult::NotSet,
            is_allowlisted_tooling: factor,
            is_rollout_allowlisted: FactorResult::NotSet,
            is_admin_bypass: FactorResult::NotSet,
        };
        let rollout = AuthorizationCheckResult {
            has_acl_access: FactorResult::NotSet,
            is_allowlisted_tooling: FactorResult::NotSet,
            is_rollout_allowlisted: factor,
            is_admin_bypass: FactorResult::NotSet,
        };
        let admin = AuthorizationCheckResult {
            has_acl_access: FactorResult::NotSet,
            is_allowlisted_tooling: FactorResult::NotSet,
            is_rollout_allowlisted: FactorResult::NotSet,
            is_admin_bypass: factor,
        };

        assert_eq!(acl.has_acl_access(), expected, "ACL factor {factor:#?}");
        assert_eq!(
            tooling.is_allowlisted_tooling(),
            expected,
            "tooling factor {factor:#?}",
        );
        assert_eq!(
            rollout.is_rollout_allowlisted(),
            expected,
            "rollout factor {factor:#?}",
        );
        assert_eq!(
            admin.is_admin_bypass(),
            expected,
            "admin factor {factor:#?}",
        );
        assert_eq!(acl.has_authorization(), expected, "ACL factor {factor:#?}");
        assert_eq!(
            tooling.has_authorization(),
            expected,
            "tooling factor {factor:#?}",
        );
        assert_eq!(
            rollout.has_authorization(),
            expected,
            "rollout factor {factor:#?}",
        );
        assert_eq!(
            admin.has_authorization(),
            expected,
            "admin factor {factor:#?}",
        );
    }
    Ok(())
}

/// What it tests: the authorization producer preserves absent optional policy
/// separately from policy that is configured but denies the caller.
/// Expected: absent tooling, rollout, and admin policies produce `NotSet`,
/// while configured groups that exclude the caller produce `Unauthorized`.
#[mononoke::fbinit_test]
async fn test_authorization_producer_preserves_absent_and_denied_factors(
    fb: FacebookInit,
) -> Result<()> {
    let acl_provider = rollout_allowlist_acl_provider()?;
    let ctx = ctx_with_identities(fb, &["USER:bob"])?;
    let acl = MononokeIdentity::from_str("REPO_REGION:repos/hg/fbsource/=tent_a")?;
    let group = MononokeIdentity::from_str("GROUP:tent_a_rollout")?;

    let absent =
        super::check_authorization(&ctx, &acl_provider, &[(&acl, None)], None, None).await?;
    assert_eq!(
        absent.has_acl_access,
        FactorResult::Unauthorized,
        "configured ACL policy that excludes the caller",
    );
    assert_eq!(
        absent.is_allowlisted_tooling,
        FactorResult::NotSet,
        "absent tooling policy",
    );
    assert_eq!(
        absent.is_rollout_allowlisted,
        FactorResult::NotSet,
        "absent rollout policy",
    );
    assert_eq!(
        absent.is_admin_bypass,
        FactorResult::NotSet,
        "absent admin policy",
    );

    let denied = super::check_authorization(
        &ctx,
        &acl_provider,
        &[(&acl, Some(&group))],
        Some("tent_a_rollout"),
        Some(&group),
    )
    .await?;
    assert_eq!(
        denied.has_acl_access,
        FactorResult::Unauthorized,
        "configured ACL policy that excludes the caller",
    );
    assert_eq!(
        denied.is_allowlisted_tooling,
        FactorResult::Unauthorized,
        "configured tooling policy that excludes the caller",
    );
    assert_eq!(
        denied.is_rollout_allowlisted,
        FactorResult::Unauthorized,
        "configured rollout policy that excludes the caller",
    );
    assert_eq!(
        denied.is_admin_bypass,
        FactorResult::Unauthorized,
        "configured admin policy that excludes the caller",
    );
    Ok(())
}

/// What it tests: path and manifest checks expose the same factor facts through
/// the common aggregation interface, while manifest roots remain optional.
/// Expected: both checks expose identical ACL and rollout observations; the
/// path has a root and the rootless manifest does not.
#[mononoke::test]
fn test_source_restriction_check_exposes_common_facts() -> Result<()> {
    let acl = MononokeIdentity::from_str("REPO_REGION:test_acl")?;
    let permission_group = MononokeIdentity::from_str("GROUP:request_access")?;
    let rollout_group = MononokeIdentity::from_str("GROUP:rollout")?;
    let authorization = AuthorizationCheckResult {
        has_acl_access: FactorResult::Authorized,
        is_allowlisted_tooling: FactorResult::NotSet,
        is_rollout_allowlisted: FactorResult::Unauthorized,
        is_admin_bypass: FactorResult::Authorized,
    };
    let path = PathRestrictionCheckResult::new(
        PathRestrictionInfo {
            restriction_root: NonRootMPath::new("restricted")?,
            repo_region_acl: acl.to_string(),
            permission_request_group: permission_group.clone(),
            rollout_allowlist_group: Some(rollout_group.clone()),
        },
        authorization,
        acl.clone(),
    );
    let manifest = super::ManifestRestrictionCheckResult::new(
        ManifestRestrictionInfo {
            restriction_root: None,
            repo_region_acl: acl.to_string(),
            permission_request_group: permission_group,
            rollout_allowlist_group: Some(rollout_group),
        },
        authorization,
        acl,
    );

    assert_eq!(path.authorization().acl_result(), FactorResult::Authorized);
    assert_eq!(path.authorization().tooling_result(), FactorResult::NotSet);
    assert_eq!(
        path.authorization().rollout_result(),
        FactorResult::Unauthorized,
    );
    assert_eq!(
        path.authorization().admin_result(),
        FactorResult::Authorized
    );
    assert_eq!(
        manifest.authorization().acl_result(),
        FactorResult::Authorized
    );
    assert_eq!(
        manifest.authorization().tooling_result(),
        FactorResult::NotSet,
    );
    assert_eq!(
        manifest.authorization().rollout_result(),
        FactorResult::Unauthorized,
    );
    assert_eq!(
        manifest.authorization().admin_result(),
        FactorResult::Authorized,
    );
    assert!(path.restriction_root().is_some());
    assert!(manifest.restriction_root().is_none());
    Ok(())
}

/// What it tests: named-source reconciliation keeps both ACL requirements for
/// one known root while allowing config to supply a rollout group omitted by
/// AclManifest.
/// Expected: ACL authorization is denied, config rollout authorization grants
/// the restriction, and both ACLs remain visible for condition matching.
#[mononoke::test]
fn test_reconcile_same_root_requires_both_acls_and_falls_back_to_config_rollout() -> Result<()> {
    let config = check_with_factors(
        "tent",
        "REPO_REGION:config_acl",
        FactorResult::Unauthorized,
        FactorResult::Authorized,
        Some("GROUP:config_rollout"),
    )?;
    let acl_manifest = check_with_factors(
        "tent",
        "REPO_REGION:manifest_acl",
        FactorResult::Authorized,
        FactorResult::NotSet,
        None,
    )?;

    let (config, acl_manifest) = ([config], [acl_manifest]);
    let reconciled = super::ReconciledAuthorization::new(&config, &acl_manifest);

    assert!(
        reconciled.has_authorization,
        "config's rollout group should authorize the root: {reconciled:#?}",
    );
    assert!(
        !reconciled.authorization.has_acl_access(),
        "the caller lacks config's ACL: {reconciled:#?}",
    );
    assert!(
        reconciled.authorization.is_rollout_allowlisted(),
        "config's rollout group should be reported: {reconciled:#?}",
    );
    assert_eq!(
        reconciled.to_source_summary().repo_region_acls(),
        [
            "REPO_REGION:config_acl".to_string(),
            "REPO_REGION:manifest_acl".to_string(),
        ]
    );
    Ok(())
}

/// What it tests: an AclManifest rollout observation takes precedence when
/// config names a different rollout group for the same root.
/// Expected: AclManifest authorization grants even when config's rollout group
/// denies the caller.
#[mononoke::test]
fn test_reconcile_same_root_prefers_acl_manifest_rollout() -> Result<()> {
    let config = check_with_factors(
        "tent",
        "REPO_REGION:acl",
        FactorResult::Unauthorized,
        FactorResult::Unauthorized,
        Some("GROUP:config_rollout"),
    )?;
    let acl_manifest = check_with_factors(
        "tent",
        "REPO_REGION:acl",
        FactorResult::Unauthorized,
        FactorResult::Authorized,
        Some("GROUP:manifest_rollout"),
    )?;

    let (config, acl_manifest) = ([config], [acl_manifest]);
    let reconciled = super::ReconciledAuthorization::new(&config, &acl_manifest);

    assert!(
        reconciled.has_authorization,
        "AclManifest's rollout group should authorize the root: {reconciled:#?}",
    );
    assert!(
        reconciled.authorization.is_rollout_allowlisted(),
        "AclManifest's rollout group should be reported: {reconciled:#?}",
    );
    Ok(())
}

/// What it tests: a root whose caller holds every source ACL but is in neither
/// source's rollout group.
/// Expected: ACL access alone authorizes the root, so the request is authorized
/// with no denial. A reconciled denial therefore always names an ACL the
/// caller lacks.
#[mononoke::test]
fn test_reconcile_acl_access_authorizes_despite_rollout_denial() -> Result<()> {
    let config = check_with_factors(
        "tent",
        "REPO_REGION:acl",
        FactorResult::Authorized,
        FactorResult::Unauthorized,
        Some("GROUP:config_rollout"),
    )?;
    let acl_manifest = check_with_factors(
        "tent",
        "REPO_REGION:acl",
        FactorResult::Authorized,
        FactorResult::Unauthorized,
        Some("GROUP:manifest_rollout"),
    )?;

    let (config, acl_manifest) = ([config], [acl_manifest]);
    let reconciled = super::ReconciledAuthorization::new(&config, &acl_manifest);

    assert!(
        reconciled.has_authorization,
        "ACL access should authorize the root despite the rollout denial: {reconciled:#?}",
    );
    assert_eq!(
        super::reconciled_denial(&config, &acl_manifest),
        None,
        "an authorized request carries no denial: {reconciled:#?}",
    );
    assert!(
        !reconciled.authorization.is_rollout_allowlisted(),
        "neither rollout group authorizes the caller: {reconciled:#?}",
    );
    Ok(())
}

/// What it tests: the denial reported when both successful sources deny the
/// same restriction root with different ACLs.
/// Expected: AclManifest's ACL is reported, as for source errors, even though
/// config's ACL sorts first.
#[mononoke::test]
fn test_reconcile_denial_prefers_acl_manifest() -> Result<()> {
    let config = check_with_factors(
        "tent",
        "REPO_REGION:sorts_first",
        FactorResult::Unauthorized,
        FactorResult::NotSet,
        None,
    )?;
    let acl_manifest = check_with_factors(
        "tent",
        "REPO_REGION:sorts_last",
        FactorResult::Unauthorized,
        FactorResult::NotSet,
        None,
    )?;

    let (config, acl_manifest) = ([config], [acl_manifest]);

    assert_eq!(
        super::reconciled_denial(&config, &acl_manifest),
        Some(DenyingRestriction {
            restriction_root: Some(NonRootMPath::new("tent")?),
            repo_region_acl: MononokeIdentity::from_str("REPO_REGION:sorts_last")?,
            permission_request_group: MononokeIdentity::from_str("REPO_REGION:sorts_last")?,
        }),
        "AclManifest's denial should be reported",
    );
    Ok(())
}

/// What it tests: the denial reported when one source denies nested tents.
/// Expected: the outer tent is reported, as single-source enforcement reports
/// it, even though the inner tent's ACL sorts first.
#[mononoke::test]
fn test_reconcile_denial_reports_outer_tent_first() -> Result<()> {
    let outer = check_with_factors(
        "nested",
        "REPO_REGION:outer_acl",
        FactorResult::Unauthorized,
        FactorResult::NotSet,
        None,
    )?;
    let inner = check_with_factors(
        "nested/inner",
        "REPO_REGION:inner_acl",
        FactorResult::Unauthorized,
        FactorResult::NotSet,
        None,
    )?;

    let denial = super::reconciled_denial(&[outer, inner], &[]);

    assert_eq!(
        denial
            .as_ref()
            .and_then(|denial| denial.restriction_root.clone()),
        Some(NonRootMPath::new("nested")?),
        "the outer tent should be reported: {denial:#?}",
    );
    Ok(())
}

/// What it tests: reconciled enforcement for a request whose condition sets
/// are scoped to restriction ACLs, so they apply only once checks are fetched.
/// Expected: a condition naming neither source's ACL leaves the request
/// unenforced, and a condition naming only AclManifest's ACL enforces
/// config's denial for the shared root.
#[mononoke::test]
fn test_reconciled_enforcement_matches_conditions_against_both_sources_acls() -> Result<()> {
    let config = check_with_factors(
        "tent",
        "REPO_REGION:config_acl",
        FactorResult::Unauthorized,
        FactorResult::NotSet,
        None,
    )?;
    let acl_manifest = check_with_factors(
        "tent",
        "REPO_REGION:manifest_acl",
        FactorResult::Authorized,
        FactorResult::NotSet,
        None,
    )?;
    let (config, acl_manifest) = ([config], [acl_manifest]);
    let unrelated = acl_scoped_condition_set("REPO_REGION:unrelated_acl")?;
    let manifest_scoped = acl_scoped_condition_set("REPO_REGION:manifest_acl")?;

    assert_eq!(
        super::reconciled_enforcement_outcome(
            &config,
            &acl_manifest,
            &[&unrelated],
            &PreFilterVariant::NeedsFetch,
        ),
        AccessEnforcementOutcome::NotEnforced,
        "a condition naming neither ACL should not enforce",
    );
    assert_eq!(
        super::reconciled_enforcement_outcome(
            &config,
            &acl_manifest,
            &[&manifest_scoped],
            &PreFilterVariant::NeedsFetch,
        ),
        AccessEnforcementOutcome::Enforced {
            denial: Some(DenyingRestriction {
                restriction_root: Some(NonRootMPath::new("tent")?),
                repo_region_acl: MononokeIdentity::from_str("REPO_REGION:config_acl")?,
                permission_request_group: MononokeIdentity::from_str("REPO_REGION:config_acl")?,
            }),
        },
        "a condition naming AclManifest's ACL should enforce config's denial",
    );
    Ok(())
}

// What it tests: cloned source fetch handles share one spawned fetch result.
// Expected: the underlying task runs once and all awaiters receive the shared
// cached result.
#[tokio::test]
async fn test_shared_fetch_handle_awaits_one_spawned_fetch() -> Result<()> {
    let run_count = Arc::new(AtomicUsize::new(0));
    let (release_sender, release_receiver) = tokio::sync::oneshot::channel();
    let run_count_for_task = run_count.clone();

    let join_handle = mononoke::spawn_task(async move {
        run_count_for_task.fetch_add(1, Ordering::SeqCst);
        release_receiver
            .await
            .context("release shared fetch test task")?;
        Ok(vec![path_restriction_check()?])
    });

    let handle = SharedFetchHandle::from_join_handle(join_handle);
    let first_waiter = handle.await_result();
    let cloned_handle = handle.clone();
    let second_waiter = cloned_handle.await_result();

    release_sender
        .send(())
        .map_err(|()| anyhow::anyhow!("shared fetch test task dropped release receiver"))?;
    let (first_result, second_result) = futures::try_join!(first_waiter, second_waiter)?;

    assert_eq!(run_count.load(Ordering::SeqCst), 1);
    assert_eq!(first_result.as_ref(), second_result.as_ref());
    assert!(std::ptr::eq(
        first_result.as_ref().as_ptr(),
        second_result.as_ref().as_ptr(),
    ));

    Ok(())
}

// What it tests: denied checks select a stable denying restriction independent
// of the source's original result order.
// Expected: the lexicographically first known restriction root
// is returned, carrying its root, ACL, and permission request group.
#[tokio::test]
async fn test_source_enforcement_outcome_denying_restriction_is_deterministic() -> Result<()> {
    let handle = SharedFetchHandle::from_result(Ok(vec![
        path_restriction_check_with("restricted/z", "REPO_REGION:z_acl", false)?,
        path_restriction_check_with("restricted/a", "REPO_REGION:a_acl", false)?,
    ]));

    let outcome =
        super::source_enforcement_outcome(&handle, &[], &super::PreFilterVariant::Definite).await?;

    assert_eq!(
        outcome.denial(),
        Some(&DenyingRestriction {
            restriction_root: Some(NonRootMPath::new("restricted/a")?),
            repo_region_acl: MononokeIdentity::from_str("REPO_REGION:a_acl")?,
            permission_request_group: MononokeIdentity::from_str("REPO_REGION:a_acl")?,
        }),
    );
    Ok(())
}

// What it tests: a manifest denial whose source reports no restriction root
// still produces a denying restriction carrying the ACL and group.
// Expected: `restriction_root` is None; ACL and group are populated.
#[tokio::test]
async fn test_source_enforcement_outcome_manifest_denial_without_root() -> Result<()> {
    let acl = MononokeIdentity::from_str("REPO_REGION:manifest_acl")?;
    let handle = SharedFetchHandle::from_result(Ok(vec![ManifestRestrictionCheckResult::new(
        ManifestRestrictionInfo {
            restriction_root: None,
            repo_region_acl: acl.to_string(),
            permission_request_group: acl.clone(),
            rollout_allowlist_group: None,
        },
        AuthorizationCheckResult::new(false, false, false, false),
        acl.clone(),
    )]));

    let outcome =
        super::source_enforcement_outcome(&handle, &[], &super::PreFilterVariant::Definite).await?;

    assert_eq!(
        outcome.denial(),
        Some(&DenyingRestriction {
            restriction_root: None,
            repo_region_acl: acl.clone(),
            permission_request_group: acl,
        }),
    );
    Ok(())
}

/// A denying restriction for a `restricted` root denied under `REPO_REGION:deny_acl`.
fn denying_restriction() -> Result<DenyingRestriction> {
    let acl = MononokeIdentity::from_str("REPO_REGION:deny_acl")?;
    Ok(DenyingRestriction {
        restriction_root: Some(NonRootMPath::new("restricted")?),
        repo_region_acl: acl.clone(),
        permission_request_group: acl,
    })
}

// What it tests: a matching exemption only changes an enforced outcome.
// Expected: enforced outcomes (denied or allowed) become `Exempted`, while
// `NotEnforced` is unchanged and nothing changes when no exemption matched.
#[mononoke::test]
fn test_access_enforcement_outcome_exempt_if() -> Result<()> {
    let denied = AccessEnforcementOutcome::Enforced {
        denial: Some(denying_restriction()?),
    };
    let allowed = AccessEnforcementOutcome::Enforced { denial: None };

    assert_eq!(
        denied.clone().exempt_if(true),
        AccessEnforcementOutcome::Exempted,
        "an exemption should exempt an enforced denial",
    );
    assert_eq!(
        allowed.clone().exempt_if(true),
        AccessEnforcementOutcome::Exempted,
        "an exemption should exempt an enforced, authorized access",
    );
    assert_eq!(
        AccessEnforcementOutcome::NotEnforced.exempt_if(true),
        AccessEnforcementOutcome::NotEnforced,
        "an exemption should not change an access no condition matched",
    );
    assert_eq!(
        denied.clone().exempt_if(false),
        denied,
        "without an exemption an enforced denial should be unchanged",
    );
    assert_eq!(
        allowed.clone().exempt_if(false),
        allowed,
        "without an exemption an enforced allow should be unchanged",
    );
    assert_eq!(
        AccessEnforcementOutcome::Exempted.denial(),
        None,
        "an exempted access should carry no denial",
    );
    Ok(())
}

// What it tests: authoritative source aggregation keeps deny-over-error
// semantics after carrying the denying restriction through.
// Expected: any denial wins over sibling source errors, while a no-deny error
// is propagated.
#[tokio::test]
async fn test_authoritative_source_enforcement_outcome_preserves_error_semantics() -> Result<()> {
    let denial = denying_restriction()?;
    let denied = super::authoritative_sources_enforcement_outcome(vec![
        Err(anyhow::anyhow!("source failed")),
        Ok(AccessEnforcementOutcome::Enforced {
            denial: Some(denial.clone()),
        }),
    ])?;
    assert_eq!(denied.denial(), Some(&denial));

    let no_denial = super::authoritative_sources_enforcement_outcome(vec![
        Ok(AccessEnforcementOutcome::NotEnforced),
        Err(anyhow::anyhow!("source failed")),
    ]);
    assert!(no_denial.is_err());

    Ok(())
}

// What it tests: a member of the admin bypass group is authorized, and the grant
// is attributed to the bypass — not mislabeled as direct ACL read access.
// Expected: is_admin_bypass is true, has_acl_access is false, and the caller is
// authorized overall.
#[mononoke::fbinit_test]
async fn test_admin_bypass_group_member_is_authorized_and_flagged(fb: FacebookInit) -> Result<()> {
    let acl_provider = admin_bypass_acl_provider()?;
    // carol is only in the bypass group, with no direct read access.
    let ctx = ctx_with_identities(fb, &["USER:carol"])?;
    let acl = MononokeIdentity::from_str("REPO_REGION:repos/hg/fbsource/=project1")?;
    let bypass_group = MononokeIdentity::from_str("GROUP:path_acls_admin_bypass")?;

    let authorization = super::check_authorization(
        &ctx,
        &acl_provider,
        &[(&acl, None)],
        None,
        Some(&bypass_group),
    )
    .await?;

    assert!(
        authorization.is_admin_bypass(),
        "bypass-group member should be flagged as an admin bypass",
    );
    assert!(
        !authorization.has_acl_access(),
        "bypass grant must not be mislabeled as direct ACL read access",
    );
    assert!(
        authorization.has_authorization(),
        "bypass-group member should be authorized overall",
    );
    Ok(())
}

// What it tests: a caller with neither read access nor bypass-group membership
// is denied even when a bypass group is configured.
// Expected: no authorization, and neither the ACL nor bypass flag is set.
#[mononoke::fbinit_test]
async fn test_non_member_without_acl_is_denied(fb: FacebookInit) -> Result<()> {
    let acl_provider = admin_bypass_acl_provider()?;
    // bob has neither read access nor bypass-group membership.
    let ctx = ctx_with_identities(fb, &["USER:bob"])?;
    let acl = MononokeIdentity::from_str("REPO_REGION:repos/hg/fbsource/=project1")?;
    let bypass_group = MononokeIdentity::from_str("GROUP:path_acls_admin_bypass")?;

    let authorization = super::check_authorization(
        &ctx,
        &acl_provider,
        &[(&acl, None)],
        None,
        Some(&bypass_group),
    )
    .await?;

    assert!(
        !authorization.has_authorization(),
        "caller without read access or bypass membership should be denied",
    );
    assert!(
        !authorization.has_acl_access(),
        "caller has no ACL read access"
    );
    assert!(
        !authorization.is_admin_bypass(),
        "caller is not in the bypass group"
    );
    Ok(())
}

// What it tests: a caller with direct ACL read access is authorized via the ACL,
// not the bypass.
// Expected: has_acl_access is true and is_admin_bypass is false.
#[mononoke::fbinit_test]
async fn test_direct_acl_read_is_not_flagged_as_bypass(fb: FacebookInit) -> Result<()> {
    let acl_provider = admin_bypass_acl_provider()?;
    // alice has direct read access on project1 but is not in the bypass group.
    let ctx = ctx_with_identities(fb, &["USER:alice"])?;
    let acl = MononokeIdentity::from_str("REPO_REGION:repos/hg/fbsource/=project1")?;
    let bypass_group = MononokeIdentity::from_str("GROUP:path_acls_admin_bypass")?;

    let authorization = super::check_authorization(
        &ctx,
        &acl_provider,
        &[(&acl, None)],
        None,
        Some(&bypass_group),
    )
    .await?;

    assert!(
        authorization.has_acl_access(),
        "user with direct ACL read access should be granted via the ACL",
    );
    assert!(
        !authorization.is_admin_bypass(),
        "a direct ACL reader must not be flagged as an admin bypass",
    );
    Ok(())
}

/// What it tests: rollout allowlisting is aggregated with `all`, not `any`.
/// Expected: the caller counts as rollout-allowlisted only when every
/// restriction in the batch allowlists them. Being allowlisted for one tent must
/// not authorize a different tent caught by the same request.
#[mononoke::test]
fn test_summary_rollout_allowlist_requires_every_check() -> Result<()> {
    let allowlisted = AuthorizationCheckResult::new(false, false, true, false);
    let not_allowlisted = AuthorizationCheckResult::new(false, false, false, false);

    let all_allowlisted = [
        check_with_authorization("tent_a", allowlisted)?,
        check_with_authorization("tent_b", allowlisted)?,
    ];
    assert!(
        SourceRestrictionSummary::from_checks(&all_allowlisted).is_rollout_allowlisted(),
        "every restriction in the batch allowlists the caller",
    );

    let mixed = [
        check_with_authorization("tent_a", allowlisted)?,
        check_with_authorization("tent_b", not_allowlisted)?,
    ];
    let summary = SourceRestrictionSummary::from_checks(&mixed);
    assert!(
        !summary.is_rollout_allowlisted(),
        "allowlisted on tent_a but not tent_b must not count as rollout-allowlisted",
    );
    assert!(
        !summary.has_authorization(),
        "with no ACL access, a partially allowlisted batch must be denied",
    );
    Ok(())
}

/// What it tests: `from_check_union` applies the same unanimity rule as
/// `from_checks` when merging checks reported by several sources.
/// Expected: one non-allowlisted check makes the union not rollout-allowlisted,
/// and with no ACL access that check denies the whole request.
#[mononoke::test]
fn test_summary_union_rollout_allowlist_requires_every_check() -> Result<()> {
    let allowlisted = AuthorizationCheckResult::new(false, false, true, false);
    let not_allowlisted = AuthorizationCheckResult::new(false, false, false, false);

    let allowlisted_check = check_with_authorization("tent_a", allowlisted)?;
    let other_allowlisted_check = check_with_authorization("tent_b", allowlisted)?;
    assert!(
        SourceRestrictionSummary::from_check_union([&allowlisted_check, &other_allowlisted_check])
            .is_rollout_allowlisted(),
        "every check in the union allowlists the caller",
    );

    let denied_check = check_with_authorization("tent_b", not_allowlisted)?;
    let summary = SourceRestrictionSummary::from_check_union([&allowlisted_check, &denied_check]);
    assert!(
        !summary.is_rollout_allowlisted(),
        "one non-allowlisted check in the union is not rollout-allowlisted",
    );
    assert!(
        !summary.has_authorization(),
        "with no ACL access, tent_b has no authorizing factor, so the request is denied",
    );
    Ok(())
}

/// What it tests: the request decision for restrictions that are each
/// authorized by a different factor: `tent_a` by ACL and `tent_b` by its
/// rollout group.
/// Expected: both `from_checks` and `from_check_union` report the request as
/// authorized, as enforcement allows it, while ACL access and the rollout
/// group are each reported false because neither covers both restrictions.
#[mononoke::test]
fn test_summary_authorizes_restrictions_with_different_factors() -> Result<()> {
    let acl_only = check_with_authorization(
        "tent_a",
        AuthorizationCheckResult::new(true, false, false, false),
    )?;
    let rollout_only = check_with_authorization(
        "tent_b",
        AuthorizationCheckResult::new(false, false, true, false),
    )?;
    let checks = [acl_only, rollout_only];

    let summaries = [
        SourceRestrictionSummary::from_checks(&checks),
        SourceRestrictionSummary::from_check_union(&checks),
    ];

    for summary in summaries {
        assert!(
            summary.has_authorization(),
            "every restriction is authorized by one of its factors: {summary:#?}",
        );
        assert!(
            !summary.has_acl_access(),
            "ACL access covers only tent_a: {summary:#?}",
        );
        assert!(
            !summary.is_rollout_allowlisted(),
            "the rollout group covers only tent_b: {summary:#?}",
        );
    }
    Ok(())
}

/// What it tests: an empty check batch is not reported as rollout-allowlisted.
/// Expected: `is_rollout_allowlisted` is false even though `all` is vacuously
/// true over an empty batch, while `has_authorization` stays true. Nothing was
/// restricted, so the access is allowed — but it was not allowlisted, and
/// logging it as such would pollute the `is_rollout_allowlisted` column.
#[mononoke::test]
fn test_summary_empty_batch_is_not_rollout_allowlisted() -> Result<()> {
    let empty: [PathRestrictionCheckResult; 0] = [];
    let summary = SourceRestrictionSummary::from_checks(&empty);

    assert!(
        !summary.is_rollout_allowlisted(),
        "an unrestricted access must not be reported as rollout-allowlisted",
    );
    assert!(
        summary.has_authorization(),
        "no restriction matched, so the access is authorized",
    );
    Ok(())
}

/// What it tests: the tooling and admin-bypass flags keep `any` aggregation.
/// Expected: both are repo-wide grants, so a single matching check is enough.
/// Only the per-tent rollout allowlist requires unanimity.
#[mononoke::test]
fn test_summary_repo_wide_flags_use_any() -> Result<()> {
    let tooling_only = AuthorizationCheckResult::new(false, true, false, false);
    let admin_only = AuthorizationCheckResult::new(false, false, false, true);
    let neither = AuthorizationCheckResult::new(false, false, false, false);

    let checks = [
        check_with_authorization("tent_a", tooling_only)?,
        check_with_authorization("tent_b", admin_only)?,
        check_with_authorization("tent_c", neither)?,
    ];
    let summary = SourceRestrictionSummary::from_checks(&checks);

    assert!(
        summary.is_allowlisted_tooling(),
        "the tooling allowlist is repo-wide, so one matching check is enough",
    );
    assert!(
        summary.is_admin_bypass(),
        "the admin bypass is repo-wide, so one matching check is enough",
    );
    assert!(
        !summary.is_rollout_allowlisted(),
        "no check in the batch is rollout-allowlisted",
    );
    Ok(())
}

/// What it tests: a caller in a tent's own rollout allowlist group is granted
/// access to that tent, and the grant is attributed to the rollout allowlist
/// rather than to direct ACL access.
/// Expected: authorized, `is_rollout_allowlisted` set, `has_acl_access` clear.
#[mononoke::fbinit_test]
async fn test_per_tent_rollout_group_member_is_authorized(fb: FacebookInit) -> Result<()> {
    let acl_provider = rollout_allowlist_acl_provider()?;
    // dave is only in tent_a's rollout group, with no direct read access.
    let ctx = ctx_with_identities(fb, &["USER:dave"])?;
    let tent_a = MononokeIdentity::from_str("REPO_REGION:repos/hg/fbsource/=tent_a")?;
    let tent_a_rollout = MononokeIdentity::from_str("GROUP:tent_a_rollout")?;

    let authorization = super::check_authorization(
        &ctx,
        &acl_provider,
        &[(&tent_a, Some(&tent_a_rollout))],
        None,
        None,
    )
    .await?;

    assert!(
        authorization.is_rollout_allowlisted(),
        "rollout-group member should be flagged as rollout-allowlisted",
    );
    assert!(
        !authorization.has_acl_access(),
        "a rollout grant must not be mislabeled as direct ACL read access",
    );
    assert!(
        authorization.has_authorization(),
        "rollout-group member should be authorized overall",
    );
    Ok(())
}

/// What it tests: an access spanning two tents where the caller is allowlisted
/// for only one of them.
/// Expected: denied. Being allowlisted for `tent_a` must not authorize reads of
/// `tent_b` caught by the same access.
#[mononoke::fbinit_test]
async fn test_rollout_allowlist_does_not_leak_across_tents(fb: FacebookInit) -> Result<()> {
    let acl_provider = rollout_allowlist_acl_provider()?;
    // dave is in tent_a's rollout group but not tent_b's.
    let ctx = ctx_with_identities(fb, &["USER:dave"])?;
    let tent_a = MononokeIdentity::from_str("REPO_REGION:repos/hg/fbsource/=tent_a")?;
    let tent_b = MononokeIdentity::from_str("REPO_REGION:repos/hg/fbsource/=tent_b")?;
    let tent_a_rollout = MononokeIdentity::from_str("GROUP:tent_a_rollout")?;
    let tent_b_rollout = MononokeIdentity::from_str("GROUP:tent_b_rollout")?;

    let authorization = super::check_authorization(
        &ctx,
        &acl_provider,
        &[
            (&tent_a, Some(&tent_a_rollout)),
            (&tent_b, Some(&tent_b_rollout)),
        ],
        None,
        None,
    )
    .await?;

    assert!(
        !authorization.is_rollout_allowlisted(),
        "allowlisted on tent_a but not tent_b must not count as rollout-allowlisted",
    );
    assert!(
        !authorization.has_authorization(),
        "the access spans a tent the caller is not allowlisted for, so it must be denied",
    );
    Ok(())
}

/// What it tests: a tent that configures no rollout allowlist group.
/// Expected: the caller is never rollout-allowlisted for it. An absent group
/// grants nothing — it must not be treated as vacuously satisfied, nor inherit
/// another tent's group.
#[mononoke::fbinit_test]
async fn test_absent_rollout_group_never_allowlists(fb: FacebookInit) -> Result<()> {
    let acl_provider = rollout_allowlist_acl_provider()?;
    let ctx = ctx_with_identities(fb, &["USER:dave"])?;
    let tent_b = MononokeIdentity::from_str("REPO_REGION:repos/hg/fbsource/=tent_b")?;

    let authorization =
        super::check_authorization(&ctx, &acl_provider, &[(&tent_b, None)], None, None).await?;

    assert!(
        !authorization.is_rollout_allowlisted(),
        "a tent with no rollout group must never report the caller as allowlisted",
    );
    assert_eq!(
        authorization.is_rollout_allowlisted,
        FactorResult::NotSet,
        "an absent rollout group is not a configured denial",
    );
    assert!(
        !authorization.has_authorization(),
        "caller has neither ACL access nor an applicable allowlist",
    );
    Ok(())
}

// What it tests: the logged enforcement decision for every enforcement result,
// with and without a matching exemption.
// Expected: the decision carries whether an exemption matched for
// `no_condition_matched` and `error`, and the exempted outcome maps to
// `Exempted`.
#[mononoke::test]
fn test_enforcement_decision_from_outcome() {
    let cases = [
        (
            Ok(AccessEnforcementOutcome::NotEnforced),
            false,
            EnforcementDecision::NoConditionMatched {
                exemption_matched: false,
            },
        ),
        (
            Ok(AccessEnforcementOutcome::NotEnforced),
            true,
            EnforcementDecision::NoConditionMatched {
                exemption_matched: true,
            },
        ),
        (
            Ok(AccessEnforcementOutcome::Enforced { denial: None }),
            false,
            EnforcementDecision::Enforced,
        ),
        (
            Ok(AccessEnforcementOutcome::Exempted),
            true,
            EnforcementDecision::Exempted,
        ),
        (
            Err(anyhow::anyhow!("source failed")),
            false,
            EnforcementDecision::Error {
                exemption_matched: false,
            },
        ),
        (
            Err(anyhow::anyhow!("source failed")),
            true,
            EnforcementDecision::Error {
                exemption_matched: true,
            },
        ),
    ];
    for (outcome, exemption_matched, expected) in cases {
        assert_eq!(
            EnforcementDecision::from_outcome(&outcome, exemption_matched),
            expected,
            "outcome {outcome:?} with exemption_matched={exemption_matched}",
        );
    }
}

fn path_restriction_check() -> Result<PathRestrictionCheckResult> {
    path_restriction_check_with("restricted", "REPO_REGION:test_acl", true)
}

/// Build an `InternalAclProvider` for the per-tent rollout allowlist tests.
/// `tent_a` and `tent_b` are separate restrictions with separate rollout
/// groups: `dave` is allowlisted for `tent_a` only, `erin` for `tent_b` only,
/// and neither has direct read access to either tent.
fn rollout_allowlist_acl_provider() -> Result<Arc<dyn AclProvider>> {
    let acls: Acls = serde_json::from_str(
        r#"
        {
            "repo_regions": {
                "repos/hg/fbsource/=tent_a": {
                    "actions": {
                        "read": ["USER:alice"]
                    }
                },
                "repos/hg/fbsource/=tent_b": {
                    "actions": {
                        "read": ["USER:alice"]
                    }
                }
            },
            "groups": {
                "tent_a_rollout": ["USER:dave"],
                "tent_b_rollout": ["USER:erin"]
            }
        }
        "#,
    )?;
    Ok(InternalAclProvider::new(acls))
}

/// Build a check carrying an explicit authorization result, for the summary
/// aggregation tests. `restriction_root` only needs to be unique per check;
/// these tests assert on the aggregated flags, not on paths or ACLs.
fn check_with_authorization(
    restriction_root: &str,
    authorization: AuthorizationCheckResult,
) -> Result<PathRestrictionCheckResult> {
    let acl = MononokeIdentity::from_str("REPO_REGION:test_acl")?;
    Ok(PathRestrictionCheckResult::new(
        PathRestrictionInfo {
            restriction_root: NonRootMPath::new(restriction_root)?,
            repo_region_acl: acl.to_string(),
            permission_request_group: acl.clone(),
            rollout_allowlist_group: None,
        },
        authorization,
        acl,
    ))
}

fn check_with_factors(
    restriction_root: &str,
    acl: &str,
    acl_result: FactorResult,
    rollout_result: FactorResult,
    rollout_group: Option<&str>,
) -> Result<PathRestrictionCheckResult> {
    let acl = MononokeIdentity::from_str(acl)?;
    Ok(PathRestrictionCheckResult::new(
        PathRestrictionInfo {
            restriction_root: NonRootMPath::new(restriction_root)?,
            repo_region_acl: acl.to_string(),
            permission_request_group: acl.clone(),
            rollout_allowlist_group: rollout_group.map(MononokeIdentity::from_str).transpose()?,
        },
        AuthorizationCheckResult {
            has_acl_access: acl_result,
            is_allowlisted_tooling: FactorResult::NotSet,
            is_rollout_allowlisted: rollout_result,
            is_admin_bypass: FactorResult::NotSet,
        },
        acl,
    ))
}

fn path_restriction_check_with(
    restriction_root: &str,
    acl: &str,
    has_acl_access: bool,
) -> Result<PathRestrictionCheckResult> {
    let acl = MononokeIdentity::from_str(acl)?;
    Ok(PathRestrictionCheckResult::new(
        PathRestrictionInfo {
            restriction_root: NonRootMPath::new(restriction_root)?,
            repo_region_acl: acl.to_string(),
            permission_request_group: acl.clone(),
            rollout_allowlist_group: None,
        },
        AuthorizationCheckResult::new(has_acl_access, false, false, false),
        acl,
    ))
}

/// Build an `InternalAclProvider` for the bypass-group access tests:
/// `alice` has direct `read` access on `project1`, while `carol` is only a
/// member of the `path_acls_admin_bypass` group. `bob` has neither.
fn admin_bypass_acl_provider() -> Result<Arc<dyn AclProvider>> {
    let acls: Acls = serde_json::from_str(
        r#"
        {
            "repo_regions": {
                "repos/hg/fbsource/=project1": {
                    "actions": {
                        "read": ["USER:alice"]
                    }
                }
            },
            "groups": {
                "path_acls_admin_bypass": ["USER:carol"]
            }
        }
        "#,
    )?;
    Ok(InternalAclProvider::new(acls))
}

/// Build a test `CoreContext` whose caller presents the given identities.
fn ctx_with_identities(fb: FacebookInit, ids: &[&str]) -> Result<CoreContext> {
    let identities = ids
        .iter()
        .map(|id| id.parse())
        .collect::<Result<MononokeIdentitySet>>()?;
    let metadata = Metadata::default().set_identities(identities);
    let session = SessionContainer::builder(fb)
        .metadata(Arc::new(metadata))
        .build();
    Ok(CoreContext::test_mock_session(session))
}

/// A condition set that applies only to restrictions guarded by `acl`.
fn acl_scoped_condition_set(acl: &str) -> Result<EnforcementConditionSet> {
    Ok(EnforcementConditionSet {
        always_enabled: false,
        require_client_request_flag: false,
        restriction_acls: vec![MononokeIdentity::from_str(acl)?],
        matchers: RequestMatchers::default(),
    })
}
