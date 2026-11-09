/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use context::CoreContext;
use context::SessionContainer;
use fbinit::FacebookInit;
use metaconfig_types::AclManifestMode;
use metadata::ClientPathAclCompatibility;
use metadata::Metadata;
use mononoke_macros::mononoke;
use mononoke_types::NonRootMPath;
use mononoke_types::RepositoryId;
use permission_checker::MononokeIdentity;
use scuba_ext::MononokeScubaSampleBuilder;
use serde_json::Value;
use serde_json::json;

use super::RestrictedPathAccessData;
use super::log_source_results_to_scuba;
use super::log_source_results_to_scuba_with_enforcement;
use super::restriction_check_result_for_source_results;
use crate::ManifestType;
use crate::RestrictedManifestId;
use crate::restriction_check::AuthorizationCheckResult;
use crate::restriction_check::EnforcementDecision;
use crate::restriction_check::ManifestRestrictionCheckResult;
use crate::restriction_check::PathRestrictionCheckResult;
use crate::restriction_check::SourceRestrictionCheck;
use crate::restriction_check::SourceRestrictionChecks;
use crate::restriction_check::SourceRestrictionError;
use crate::restriction_check::SourceRestrictionResult;
use crate::restriction_info::ManifestRestrictionInfo;
use crate::restriction_info::PathRestrictionInfo;

struct ShadowComparisonFieldFixture<T: SourceRestrictionCheck> {
    fb: FacebookInit,
    ctx: CoreContext,
    repo_id: RepositoryId,
    acl_manifest_mode: AclManifestMode,
    config_result: SourceRestrictionResult<T>,
    acl_manifest_result: Option<SourceRestrictionResult<T>>,
    access_data: RestrictedPathAccessData,
    scuba: MononokeScubaSampleBuilder,
    log_path: PathBuf,
}

impl<T: SourceRestrictionCheck> ShadowComparisonFieldFixture<T> {
    fn new(
        fb: FacebookInit,
        config_result: SourceRestrictionResult<T>,
        acl_manifest_result: Option<SourceRestrictionResult<T>>,
        access_data: RestrictedPathAccessData,
    ) -> Result<Self> {
        let temp_log_file = tempfile::NamedTempFile::new()?;
        let log_path = temp_log_file.into_temp_path().keep()?;
        let scuba = MononokeScubaSampleBuilder::with_discard().with_log_file(&log_path)?;

        Ok(Self {
            fb,
            ctx: CoreContext::test_mock(fb),
            repo_id: RepositoryId::new(1),
            acl_manifest_mode: AclManifestMode::Shadow,
            config_result,
            acl_manifest_result,
            access_data,
            scuba,
            log_path,
        })
    }

    fn with_acl_manifest_mode(self, acl_manifest_mode: AclManifestMode) -> Self {
        Self {
            acl_manifest_mode,
            ..self
        }
    }

    /// Rebuilds the fixture context so its requests carry the given client
    /// User-Agent, which `log_access_to_scuba` records as `http_user_agent`.
    fn with_user_agent(self, user_agent: &str) -> Self {
        let ctx = ctx_with_user_agent(self.fb, user_agent);
        Self { ctx, ..self }
    }

    fn with_client_path_acl_compatibility(
        self,
        client_path_acl_compatibility: ClientPathAclCompatibility,
    ) -> Self {
        let metadata = Metadata::default();
        let mut metadata = metadata;
        metadata.add_client_path_acl_compatibility(client_path_acl_compatibility);
        let session = SessionContainer::builder(self.fb)
            .metadata(Arc::new(metadata))
            .build();
        let ctx = session.new_context(MononokeScubaSampleBuilder::with_discard());
        Self { ctx, ..self }
    }

    fn with_upstream_client_id(self, client_id: &str) -> Self {
        let mut metadata = Metadata::default();
        metadata.add_upstream_client_id(client_id.to_string());
        let session = SessionContainer::builder(self.fb)
            .metadata(Arc::new(metadata))
            .build();
        let ctx = session.new_context(MononokeScubaSampleBuilder::with_discard());
        Self { ctx, ..self }
    }

    fn log_with(
        self,
        log_results: impl FnOnce(
            &CoreContext,
            RepositoryId,
            &SourceRestrictionResult<T>,
            Option<&SourceRestrictionResult<T>>,
            AclManifestMode,
            bool,
            RestrictedPathAccessData,
            MononokeScubaSampleBuilder,
        ) -> Result<()>,
    ) -> Result<Vec<serde_json::Map<String, Value>>> {
        let (log_result, samples) = self.log_with_result(log_results)?;
        log_result?;
        Ok(samples)
    }

    fn log_with_result(
        self,
        log_results: impl FnOnce(
            &CoreContext,
            RepositoryId,
            &SourceRestrictionResult<T>,
            Option<&SourceRestrictionResult<T>>,
            AclManifestMode,
            bool,
            RestrictedPathAccessData,
            MononokeScubaSampleBuilder,
        ) -> Result<()>,
    ) -> Result<(Result<()>, Vec<serde_json::Map<String, Value>>)> {
        let Self {
            fb: _,
            ctx,
            repo_id,
            acl_manifest_mode,
            config_result,
            acl_manifest_result,
            access_data,
            scuba,
            log_path,
        } = self;

        let log_result = log_results(
            &ctx,
            repo_id,
            &config_result,
            acl_manifest_result.as_ref(),
            acl_manifest_mode,
            true,
            access_data,
            scuba,
        );
        let samples = read_logged_samples(&log_path)?;
        Ok((log_result, samples))
    }
}

// What it tests: Shadow logging can surface compact comparison fields for
// config and AclManifest results.
// Expected: source attribution and mismatch summary fields are emitted.
#[mononoke::fbinit_test]
async fn test_shadow_mismatch_summary_fields_are_logged(fb: FacebookInit) -> Result<()> {
    let samples = ShadowComparisonFieldFixture::new(
        fb,
        restricted_path_result(false, false, "config_acl", "config/restricted")?,
        Some(restricted_path_result(
            true,
            true,
            "acl_manifest_acl",
            "acl_manifest/restricted",
        )?),
        full_path_access_data()?,
    )?
    .log_with(log_source_results_to_scuba)?;

    assert_eq!(samples.len(), 1);
    let sample = &samples[0];
    assert_eq!(
        sample_field(sample, "acl_manifest_mode"),
        Some("shadow".to_string())
    );
    assert_eq!(
        sample_field(sample, "shadow_mismatch"),
        Some("true".to_string())
    );
    assert_eq!(
        sample_array(sample, "considered_restricted_by"),
        vec!["manifest_db".to_string(), "acl_manifest".to_string()]
    );

    let detail = sample_json_field(sample, "shadow_mismatch_detail")?
        .ok_or_else(|| anyhow!("missing shadow_mismatch_detail"))?;
    assert_eq!(
        detail["differences"],
        json!(["has_authorization", "restriction_acls", "restriction_paths"])
    );
    assert_eq!(detail["config"]["restricted"], json!(true));
    assert_eq!(detail["config"]["has_authorization"], json!(false));
    assert_eq!(
        detail["config"]["restriction_acls"],
        json!(["REPO_REGION:config_acl"])
    );
    assert_eq!(
        detail["config"]["restriction_paths"],
        json!(["config/restricted"])
    );
    assert_eq!(detail["acl_manifest"]["restricted"], json!(true));
    assert_eq!(detail["acl_manifest"]["has_authorization"], json!(true));
    assert_eq!(
        detail["acl_manifest"]["restriction_acls"],
        json!(["REPO_REGION:acl_manifest_acl"])
    );
    assert_eq!(
        detail["acl_manifest"]["restriction_paths"],
        json!(["acl_manifest/restricted"])
    );
    Ok(())
}

// What it tests: Shadow logging records the parity case where both sources
// report the same restricted result.
// Expected: a row is emitted without mismatch detail when both sources agree.
#[mononoke::fbinit_test]
async fn test_shadow_matching_restricted_sources_log_row_without_mismatch(
    fb: FacebookInit,
) -> Result<()> {
    let samples = ShadowComparisonFieldFixture::new(
        fb,
        restricted_path_result(false, false, "shared_acl", "shared/restricted")?,
        Some(restricted_path_result(
            false,
            false,
            "shared_acl",
            "shared/restricted",
        )?),
        full_path_access_data()?,
    )?
    .log_with(log_source_results_to_scuba)?;

    assert_eq!(samples.len(), 1);
    let sample = &samples[0];
    assert_eq!(
        sample_array(sample, "considered_restricted_by"),
        vec!["manifest_db".to_string(), "acl_manifest".to_string()]
    );
    assert_eq!(
        sample_array(sample, "restricted_paths"),
        vec!["shared/restricted".to_string()]
    );
    assert_eq!(
        sample_array(sample, "acls"),
        vec!["REPO_REGION:shared_acl".to_string()]
    );
    assert_eq!(
        sample_field(sample, "shadow_mismatch"),
        Some("false".to_string())
    );
    assert_eq!(sample_field(sample, "shadow_mismatch_detail"), None);
    Ok(())
}

// What it tests: restriction-root-only differences stay diagnostic-only.
// Expected: the broad mismatch detail is populated, but the queryable mismatch
// boolean remains false.
#[mononoke::fbinit_test]
async fn test_shadow_root_only_differences_do_not_set_shadow_mismatch(
    fb: FacebookInit,
) -> Result<()> {
    let samples = ShadowComparisonFieldFixture::new(
        fb,
        restricted_manifest_result(false, false, "shared_acl", Some("config/restricted"))?,
        Some(restricted_manifest_result(
            false,
            false,
            "shared_acl",
            None,
        )?),
        manifest_access_data(ManifestType::HgAugmented),
    )?
    .log_with(log_source_results_to_scuba)?;

    assert_eq!(samples.len(), 1);
    let sample = &samples[0];
    assert_eq!(
        sample_field(sample, "shadow_mismatch"),
        Some("false".to_string())
    );
    let detail = sample_json_field(sample, "shadow_mismatch_detail")?
        .ok_or_else(|| anyhow!("missing shadow_mismatch_detail"))?;
    assert_eq!(detail["differences"], json!(["restriction_paths"]));
    Ok(())
}

// What it tests: Shadow aggregate fields stay config-authoritative while
// AclManifest contributes comparison-only telemetry.
// Expected: top-level aggregate fields are derived from config, while
// AclManifest disagreement is recorded in the mismatch summary.
#[mononoke::fbinit_test]
async fn test_shadow_aggregate_fields_stay_config_authoritative(fb: FacebookInit) -> Result<()> {
    let samples = ShadowComparisonFieldFixture::new(
        fb,
        restricted_manifest_result(false, false, "config_acl", Some("config/restricted"))?,
        Some(unrestricted_result()),
        manifest_access_data(ManifestType::HgAugmented),
    )?
    .log_with(log_source_results_to_scuba)?;

    assert_eq!(samples.len(), 1);
    let sample = &samples[0];
    assert_eq!(
        sample_field(sample, "has_authorization"),
        Some("false".to_string())
    );
    assert_eq!(
        sample_field(sample, "has_acl_access"),
        Some("false".to_string())
    );
    assert_eq!(
        sample_array(sample, "restricted_paths"),
        vec!["config/restricted".to_string()]
    );
    assert_eq!(
        sample_array(sample, "acls"),
        vec!["REPO_REGION:config_acl".to_string()]
    );
    assert_eq!(
        sample_array(sample, "considered_restricted_by"),
        vec!["manifest_db".to_string()]
    );
    assert_eq!(
        sample_field(sample, "shadow_mismatch"),
        Some("true".to_string())
    );
    let detail = sample_json_field(sample, "shadow_mismatch_detail")?
        .ok_or_else(|| anyhow!("missing shadow_mismatch_detail"))?;
    assert_eq!(
        detail["differences"],
        json!([
            "restricted",
            "has_authorization",
            "restriction_acls",
            "restriction_paths"
        ])
    );
    assert_eq!(detail["config"]["restricted"], json!(true));
    assert_eq!(detail["acl_manifest"]["restricted"], json!(false));
    Ok(())
}

// What it tests: Shadow comparison errors are logged without changing the
// config-authoritative aggregate result.
// Expected: the AclManifest error and mismatch summary fields are populated
// while top-level authorization still comes from the config source.
#[mononoke::fbinit_test]
async fn test_shadow_comparison_errors_are_logged(fb: FacebookInit) -> Result<()> {
    let samples = ShadowComparisonFieldFixture::<PathRestrictionCheckResult>::new(
        fb,
        unrestricted_result(),
        Some(error_result("acl manifest lookup failed")),
        full_path_access_data()?,
    )?
    .log_with(log_source_results_to_scuba)?;

    assert_eq!(samples.len(), 1);
    let sample = &samples[0];
    assert_eq!(
        sample_field(sample, "has_authorization"),
        Some("true".to_string())
    );
    assert_eq!(sample_field(sample, "config_error"), None);
    assert!(
        sample_field(sample, "acl_manifest_error")
            .is_some_and(|value| value.contains("acl manifest lookup failed"))
    );
    assert_eq!(
        sample_array(sample, "considered_restricted_by"),
        Vec::<String>::new()
    );
    assert_eq!(
        sample_field(sample, "shadow_mismatch"),
        Some("true".to_string())
    );
    let detail = sample_json_field(sample, "shadow_mismatch_detail")?
        .ok_or_else(|| anyhow!("missing shadow_mismatch_detail"))?;
    assert_eq!(detail["differences"], json!(["acl_manifest_error"]));
    assert_eq!(
        detail["acl_manifest"]["error"],
        json!("acl manifest lookup failed")
    );
    Ok(())
}

// What it tests: Shadow logs config errors before bubbling them up to the
// caller.
// Expected: the config error is returned, and the error-only comparison row is
// emitted without aggregate authorization fields.
#[mononoke::fbinit_test]
async fn test_shadow_config_errors_are_returned(fb: FacebookInit) -> Result<()> {
    let (log_result, samples) = ShadowComparisonFieldFixture::<PathRestrictionCheckResult>::new(
        fb,
        error_result("config lookup failed"),
        Some(error_result("acl manifest lookup failed")),
        full_path_access_data()?,
    )?
    .log_with_result(log_source_results_to_scuba)?;

    let err = match log_result {
        Ok(()) => {
            return Err(anyhow!(
                "expected config error, logged samples: {samples:?}"
            ));
        }
        Err(err) => err,
    };

    assert!(format!("{err:#}").contains("config lookup failed"));
    assert_eq!(samples.len(), 1);
    let sample = &samples[0];
    assert!(
        sample_field(sample, "config_error")
            .is_some_and(|value| value.contains("config lookup failed"))
    );
    assert!(
        sample_field(sample, "acl_manifest_error")
            .is_some_and(|value| value.contains("acl manifest lookup failed"))
    );
    assert_eq!(sample_field(sample, "has_authorization"), None);
    assert_eq!(sample_field(sample, "has_acl_access"), None);
    assert_eq!(sample_field(sample, "acls"), None);
    assert_eq!(
        sample_array(sample, "considered_restricted_by"),
        Vec::<String>::new()
    );
    assert_eq!(
        sample_field(sample, "shadow_mismatch"),
        Some("false".to_string())
    );
    let detail = sample_json_field(sample, "shadow_mismatch_detail")?
        .ok_or_else(|| anyhow!("missing shadow_mismatch_detail"))?;
    assert_eq!(
        detail["differences"],
        json!(["config_error", "acl_manifest_error"])
    );
    assert_eq!(detail["config"]["error"], json!("config lookup failed"));
    assert_eq!(
        detail["acl_manifest"]["error"],
        json!("acl manifest lookup failed")
    );
    Ok(())
}

// What it tests: Both-mode source logging uses the union of successful sources
// for top-level aggregate fields.
// Expected: an AclManifest denial is visible in authorization, ACL, and
// restricted-path fields even when config is unrestricted.
#[mononoke::fbinit_test]
async fn test_both_aggregate_fields_union_source_results(fb: FacebookInit) -> Result<()> {
    let samples = ShadowComparisonFieldFixture::new(
        fb,
        unrestricted_result(),
        Some(restricted_path_result(
            false,
            false,
            "acl_manifest_acl",
            "acl_manifest/restricted",
        )?),
        full_path_access_data()?,
    )?
    .with_acl_manifest_mode(AclManifestMode::Both)
    .log_with(log_source_results_to_scuba)?;

    assert_eq!(samples.len(), 1);
    let sample = &samples[0];
    assert_eq!(
        sample_field(sample, "acl_manifest_mode"),
        Some("both".to_string())
    );
    assert_eq!(
        sample_field(sample, "has_authorization"),
        Some("false".to_string())
    );
    assert_eq!(
        sample_field(sample, "has_acl_access"),
        Some("false".to_string())
    );
    assert_eq!(
        sample_array(sample, "restricted_paths"),
        vec!["acl_manifest/restricted".to_string()]
    );
    assert_eq!(
        sample_array(sample, "acls"),
        vec!["REPO_REGION:acl_manifest_acl".to_string()]
    );
    assert_eq!(
        sample_array(sample, "considered_restricted_by"),
        vec!["acl_manifest".to_string()]
    );
    assert_eq!(
        sample_field(sample, "shadow_mismatch"),
        Some("true".to_string())
    );
    Ok(())
}

/// What it tests: Both-mode logging does not synthesize reconciliation detail
/// when the config source fails.
/// Expected: the config error is returned, the successful AclManifest result is
/// logged as the aggregate, and authorization detail is omitted.
#[mononoke::fbinit_test]
async fn test_both_config_error_omits_authorization_detail(fb: FacebookInit) -> Result<()> {
    let (log_result, samples) = ShadowComparisonFieldFixture::new(
        fb,
        error_result("config lookup failed"),
        Some(restricted_path_result(
            false,
            false,
            "acl_manifest_acl",
            "acl_manifest/restricted",
        )?),
        full_path_access_data()?,
    )?
    .with_acl_manifest_mode(AclManifestMode::Both)
    .log_with_result(log_source_results_to_scuba)?;

    let err = match log_result {
        Ok(()) => {
            return Err(anyhow!(
                "expected config error, logged samples: {samples:?}"
            ));
        }
        Err(err) => err,
    };
    assert!(
        format!("{err:#}").contains("config lookup failed"),
        "the config error should be returned: {err:#}",
    );
    assert_eq!(samples.len(), 1);
    let sample = &samples[0];
    assert!(
        sample_field(sample, "config_error")
            .is_some_and(|value| value.contains("config lookup failed"))
    );
    assert_eq!(sample_field(sample, "acl_manifest_error"), None);
    assert_eq!(sample_field(sample, "authorization_detail"), None);
    assert_eq!(
        sample_field(sample, "has_authorization"),
        Some("false".to_string())
    );
    assert_eq!(
        sample_array(sample, "acls"),
        vec!["REPO_REGION:acl_manifest_acl".to_string()]
    );
    Ok(())
}

/// What it tests: Both-mode logging does not synthesize reconciliation detail
/// when the AclManifest source fails.
/// Expected: the successful config result remains the aggregate, the source
/// error is logged, and authorization detail is omitted.
#[mononoke::fbinit_test]
async fn test_both_acl_manifest_error_omits_authorization_detail(fb: FacebookInit) -> Result<()> {
    let samples = ShadowComparisonFieldFixture::new(
        fb,
        restricted_path_result(false, false, "config_acl", "config/restricted")?,
        Some(error_result("acl manifest lookup failed")),
        full_path_access_data()?,
    )?
    .with_acl_manifest_mode(AclManifestMode::Both)
    .log_with(log_source_results_to_scuba)?;

    assert_eq!(samples.len(), 1);
    let sample = &samples[0];
    assert_eq!(sample_field(sample, "config_error"), None);
    assert!(
        sample_field(sample, "acl_manifest_error")
            .is_some_and(|value| value.contains("acl manifest lookup failed"))
    );
    assert_eq!(sample_field(sample, "authorization_detail"), None);
    assert_eq!(
        sample_field(sample, "has_authorization"),
        Some("false".to_string())
    );
    assert_eq!(
        sample_array(sample, "acls"),
        vec!["REPO_REGION:config_acl".to_string()]
    );
    Ok(())
}

// What it tests: Both-mode source-comparison callers return the union of
// successful source results, matching the aggregate logging semantics.
// Expected: an AclManifest-only restriction is denied instead of returning the
// unrestricted config result.
#[mononoke::fbinit_test]
async fn test_both_check_result_unions_acl_manifest_only_restriction(
    _fb: FacebookInit,
) -> Result<()> {
    let acl_manifest_result =
        restricted_path_result(false, false, "acl_manifest_acl", "acl_manifest/restricted")?;
    let result = restriction_check_result_for_source_results(
        AclManifestMode::Both,
        true,
        &unrestricted_result(),
        Some(&acl_manifest_result),
    )?;

    assert!(!result.has_authorization);
    assert_eq!(
        result.restriction_roots,
        vec![NonRootMPath::new("acl_manifest/restricted")?]
    );
    Ok(())
}

// What it tests: a skipped comparison source stays distinct from an
// unrestricted source.
// Expected: skipped AclManifest comparison does not produce an error, mismatch
// detail, or AclManifest source attribution.
#[mononoke::fbinit_test]
async fn test_shadow_skipped_comparison_source_is_not_logged(fb: FacebookInit) -> Result<()> {
    let samples = ShadowComparisonFieldFixture::new(
        fb,
        restricted_path_result(false, false, "config_acl", "config/restricted")?,
        None,
        manifest_access_data(ManifestType::Hg),
    )?
    .log_with(log_source_results_to_scuba)?;

    assert_eq!(samples.len(), 1);
    let sample = &samples[0];
    assert_eq!(sample_field(sample, "acl_manifest_error"), None);
    assert_eq!(
        sample_field(sample, "shadow_mismatch"),
        Some("false".to_string())
    );
    assert_eq!(sample_field(sample, "shadow_mismatch_detail"), None);
    assert_eq!(
        sample_array(sample, "considered_restricted_by"),
        vec!["manifest_db".to_string()]
    );
    Ok(())
}

// What it tests: successful unrestricted source results do not emit a row.
// Expected: no Scuba row is written when both sources are unrestricted.
#[mononoke::fbinit_test]
async fn test_shadow_unrestricted_sources_do_not_log_rows(fb: FacebookInit) -> Result<()> {
    let samples = ShadowComparisonFieldFixture::<PathRestrictionCheckResult>::new(
        fb,
        unrestricted_result(),
        Some(unrestricted_result()),
        full_path_access_data()?,
    )?
    .log_with(log_source_results_to_scuba)?;

    assert_eq!(samples, Vec::<serde_json::Map<String, Value>>::new());
    Ok(())
}

// What it tests: every enforcement decision is logged as `enforcement_decision`
// with whether an exemption matched, and `access_enforcement_enabled` keeps its
// legacy values.
// Expected: `enforced` logs true, `no_condition_matched` and `exempted` log
// false, and `disabled` / `error` omit `access_enforcement_enabled`;
// `has_enforcement_exemption` is omitted only for `disabled`.
#[mononoke::fbinit_test]
async fn test_enforcement_decision_is_logged(fb: FacebookInit) -> Result<()> {
    let cases = [
        (EnforcementDecision::Disabled, "disabled", None, None),
        (
            EnforcementDecision::NoConditionMatched {
                exemption_matched: false,
            },
            "no_condition_matched",
            Some("false"),
            Some("false"),
        ),
        (
            EnforcementDecision::NoConditionMatched {
                exemption_matched: true,
            },
            "no_condition_matched",
            Some("false"),
            Some("true"),
        ),
        (
            EnforcementDecision::Enforced,
            "enforced",
            Some("true"),
            Some("false"),
        ),
        (
            EnforcementDecision::Exempted,
            "exempted",
            Some("false"),
            Some("true"),
        ),
        (
            EnforcementDecision::Error {
                exemption_matched: true,
            },
            "error",
            None,
            Some("true"),
        ),
        (
            EnforcementDecision::Error {
                exemption_matched: false,
            },
            "error",
            None,
            Some("false"),
        ),
    ];

    for (decision, expected_decision, expected_enabled, expected_exemption) in cases {
        let samples = ShadowComparisonFieldFixture::new(
            fb,
            restricted_path_result(false, false, "config_acl", "config/restricted")?,
            Some(restricted_path_result(
                false,
                false,
                "config_acl",
                "config/restricted",
            )?),
            full_path_access_data()?,
        )?
        .log_with(
            |ctx, repo_id, config, acl_manifest, mode, reconcile_sources, access_data, scuba| {
                log_source_results_to_scuba_with_enforcement(
                    ctx,
                    repo_id,
                    config,
                    acl_manifest,
                    mode,
                    reconcile_sources,
                    Some(decision),
                    access_data,
                    scuba,
                )
            },
        )?;

        assert_eq!(samples.len(), 1, "{decision:?} should log one row");
        assert_eq!(
            sample_field(&samples[0], "enforcement_decision").as_deref(),
            Some(expected_decision),
            "{decision:?} should be logged as {expected_decision}",
        );
        assert_eq!(
            sample_field(&samples[0], "access_enforcement_enabled").as_deref(),
            expected_enabled,
            "{decision:?} should keep the legacy access_enforcement_enabled value",
        );
        assert_eq!(
            sample_field(&samples[0], "has_enforcement_exemption").as_deref(),
            expected_exemption,
            "{decision:?} should log whether an exemption matched",
        );
    }
    Ok(())
}

// What it tests: rows logged outside request enforcement carry no decision.
// Expected: none of `enforcement_decision`, `access_enforcement_enabled` or
// `has_enforcement_exemption` is emitted.
#[mononoke::fbinit_test]
async fn test_enforcement_decision_is_omitted_without_enforcement(fb: FacebookInit) -> Result<()> {
    let samples = ShadowComparisonFieldFixture::new(
        fb,
        restricted_path_result(false, false, "config_acl", "config/restricted")?,
        Some(restricted_path_result(
            false,
            false,
            "config_acl",
            "config/restricted",
        )?),
        full_path_access_data()?,
    )?
    .log_with(log_source_results_to_scuba)?;

    assert_eq!(samples.len(), 1, "a restricted access should log one row");
    assert_eq!(
        sample_field(&samples[0], "enforcement_decision"),
        None,
        "rows logged outside request enforcement should not carry a decision",
    );
    assert_eq!(
        sample_field(&samples[0], "access_enforcement_enabled"),
        None,
        "rows logged outside request enforcement should not carry access_enforcement_enabled",
    );
    assert_eq!(
        sample_field(&samples[0], "has_enforcement_exemption"),
        None,
        "rows logged outside request enforcement should not carry has_enforcement_exemption",
    );
    Ok(())
}

// What it tests: the client User-Agent is recorded as `http_user_agent` on the
// restricted-path access row when present in the request metadata.
// Expected: the emitted row carries `http_user_agent` equal to the client UA.
#[mononoke::fbinit_test]
async fn test_client_user_agent_is_logged(fb: FacebookInit) -> Result<()> {
    let samples = ShadowComparisonFieldFixture::new(
        fb,
        restricted_path_result(false, false, "config_acl", "config/restricted")?,
        Some(restricted_path_result(
            true,
            true,
            "acl_manifest_acl",
            "acl_manifest/restricted",
        )?),
        full_path_access_data()?,
    )?
    .with_user_agent("Sapling/1.2.3")
    .log_with(log_source_results_to_scuba)?;

    assert_eq!(samples.len(), 1);
    assert_eq!(
        sample_field(&samples[0], "http_user_agent"),
        Some("Sapling/1.2.3".to_string()),
        "restricted-path row should carry the client User-Agent"
    );
    Ok(())
}

// What it tests: no `http_user_agent` column is emitted when the request carries
// no User-Agent in its metadata.
// Expected: the emitted row omits `http_user_agent` entirely.
#[mononoke::fbinit_test]
async fn test_missing_user_agent_is_not_logged(fb: FacebookInit) -> Result<()> {
    let samples = ShadowComparisonFieldFixture::new(
        fb,
        restricted_path_result(false, false, "config_acl", "config/restricted")?,
        Some(restricted_path_result(
            true,
            true,
            "acl_manifest_acl",
            "acl_manifest/restricted",
        )?),
        full_path_access_data()?,
    )?
    .log_with(log_source_results_to_scuba)?;

    assert_eq!(samples.len(), 1);
    assert_eq!(
        sample_field(&samples[0], "http_user_agent"),
        None,
        "row should omit http_user_agent when no User-Agent is present"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_default_client_path_acl_compatibility_is_logged(fb: FacebookInit) -> Result<()> {
    let samples = ShadowComparisonFieldFixture::new(
        fb,
        restricted_path_result(false, false, "config_acl", "config/restricted")?,
        Some(restricted_path_result(
            true,
            true,
            "acl_manifest_acl",
            "acl_manifest/restricted",
        )?),
        full_path_access_data()?,
    )?
    .log_with(log_source_results_to_scuba)?;

    assert_eq!(samples.len(), 1);
    assert_eq!(
        sample_field(&samples[0], "client_path_acl_compatibility"),
        Some("absent".to_string()),
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_non_default_client_path_acl_compatibility_is_logged(fb: FacebookInit) -> Result<()> {
    for (state, expected) in [
        (ClientPathAclCompatibility::ReadyV1, "ready_v1"),
        (ClientPathAclCompatibility::Malformed, "malformed"),
    ] {
        let samples = ShadowComparisonFieldFixture::new(
            fb,
            restricted_path_result(false, false, "config_acl", "config/restricted")?,
            Some(restricted_path_result(
                true,
                true,
                "acl_manifest_acl",
                "acl_manifest/restricted",
            )?),
            full_path_access_data()?,
        )?
        .with_client_path_acl_compatibility(state)
        .log_with(log_source_results_to_scuba)?;

        assert_eq!(samples.len(), 1);
        assert_eq!(
            sample_field(&samples[0], "client_path_acl_compatibility"),
            Some(expected.to_string()),
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_client_id_is_logged_when_present(fb: FacebookInit) -> Result<()> {
    let samples = ShadowComparisonFieldFixture::new(
        fb,
        restricted_path_result(false, false, "config_acl", "config/restricted")?,
        Some(restricted_path_result(
            true,
            true,
            "acl_manifest_acl",
            "acl_manifest/restricted",
        )?),
        full_path_access_data()?,
    )?
    .with_upstream_client_id("SomeSCMQueryCaller")
    .log_with(log_source_results_to_scuba)?;

    assert_eq!(samples.len(), 1);
    assert_eq!(
        sample_field(&samples[0], "client_id"),
        Some("SomeSCMQueryCaller".to_string()),
        "row should carry the caller-declared client_id"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_client_id_is_omitted_when_absent(fb: FacebookInit) -> Result<()> {
    let samples = ShadowComparisonFieldFixture::new(
        fb,
        restricted_path_result(false, false, "config_acl", "config/restricted")?,
        Some(restricted_path_result(
            true,
            true,
            "acl_manifest_acl",
            "acl_manifest/restricted",
        )?),
        full_path_access_data()?,
    )?
    .log_with(log_source_results_to_scuba)?;

    assert_eq!(samples.len(), 1);
    assert_eq!(
        sample_field(&samples[0], "client_id"),
        None,
        "row should omit client_id when the request did not declare one"
    );
    Ok(())
}

/// Builds a test `CoreContext` whose request metadata carries `user_agent`, so
/// tests can exercise the `http_user_agent` logging path.
fn ctx_with_user_agent(fb: FacebookInit, user_agent: &str) -> CoreContext {
    let metadata = Metadata::default().set_user_agent(Some(user_agent.to_string()));
    let session = SessionContainer::builder(fb)
        .metadata(Arc::new(metadata))
        .build();
    session.new_context(MononokeScubaSampleBuilder::with_discard())
}

fn restricted_path_result(
    has_authorization: bool,
    has_acl_access: bool,
    acl_name: &str,
    restriction_path: &str,
) -> Result<SourceRestrictionResult<PathRestrictionCheckResult>> {
    let repo_region_acl = repo_region_acl(acl_name);
    let repo_region_identity = MononokeIdentity::from_str(&repo_region_acl)?;
    Ok(Ok(SourceRestrictionChecks::new(vec![
        PathRestrictionCheckResult::new(
            PathRestrictionInfo {
                restriction_root: NonRootMPath::new(restriction_path)?,
                repo_region_acl: repo_region_acl.clone(),
                permission_request_group: repo_region_identity.clone(),
                rollout_allowlist_group: None,
            },
            authorization_result(has_authorization, has_acl_access),
            repo_region_identity,
        ),
    ])))
}

fn restricted_manifest_result(
    has_authorization: bool,
    has_acl_access: bool,
    acl_name: &str,
    restriction_path: Option<&str>,
) -> Result<SourceRestrictionResult<ManifestRestrictionCheckResult>> {
    let repo_region_acl = repo_region_acl(acl_name);
    let repo_region_identity = MononokeIdentity::from_str(&repo_region_acl)?;
    Ok(Ok(SourceRestrictionChecks::new(vec![
        ManifestRestrictionCheckResult::new(
            ManifestRestrictionInfo {
                restriction_root: restriction_path.map(NonRootMPath::new).transpose()?,
                repo_region_acl: repo_region_acl.clone(),
                permission_request_group: repo_region_identity.clone(),
                rollout_allowlist_group: None,
            },
            authorization_result(has_authorization, has_acl_access),
            repo_region_identity,
        ),
    ])))
}

fn unrestricted_result<T: SourceRestrictionCheck>() -> SourceRestrictionResult<T> {
    Ok(SourceRestrictionChecks::new(Vec::new()))
}

fn error_result<T: SourceRestrictionCheck>(message: &str) -> SourceRestrictionResult<T> {
    Err(SourceRestrictionError::from(anyhow!("{message}")))
}

fn authorization_result(has_authorization: bool, has_acl_access: bool) -> AuthorizationCheckResult {
    AuthorizationCheckResult::new(
        has_acl_access,
        has_authorization && !has_acl_access,
        false,
        false,
    )
}

fn repo_region_acl(acl_name: &str) -> String {
    format!("REPO_REGION:{acl_name}")
}

fn full_path_access_data() -> Result<RestrictedPathAccessData> {
    Ok(RestrictedPathAccessData::FullPath {
        full_path: NonRootMPath::new("requested/path")?,
    })
}

fn manifest_access_data(manifest_type: ManifestType) -> RestrictedPathAccessData {
    RestrictedPathAccessData::Manifest(
        RestrictedManifestId::from("1111111111111111111111111111111111111111"),
        manifest_type,
    )
}

fn read_logged_samples(log_path: &std::path::Path) -> Result<Vec<serde_json::Map<String, Value>>> {
    let contents = std::fs::read_to_string(log_path)
        .with_context(|| format!("failed to read scuba log {}", log_path.display()))?;
    contents
        .lines()
        .map(|line| {
            let json: Value = serde_json::from_str(line)
                .with_context(|| format!("failed to parse scuba row: {line}"))?;
            flatten_scuba_sample(&json)
        })
        .collect()
}

fn flatten_scuba_sample(json: &Value) -> Result<serde_json::Map<String, Value>> {
    let top_level = json
        .as_object()
        .ok_or_else(|| anyhow!("top-level scuba row should be a JSON object"))?;
    Ok(top_level
        .values()
        .filter_map(Value::as_object)
        .flat_map(|category| category.iter())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect())
}

fn sample_field(sample: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    match sample.get(key) {
        Some(Value::String(value)) => Some(value.clone()),
        Some(Value::Bool(value)) => Some(value.to_string()),
        Some(other) => Some(other.to_string()),
        None => None,
    }
}

fn sample_json_field(sample: &serde_json::Map<String, Value>, key: &str) -> Result<Option<Value>> {
    sample_field(sample, key)
        .map(|value| {
            serde_json::from_str(&value)
                .with_context(|| format!("failed to parse {key} as JSON: {value}"))
        })
        .transpose()
}

fn sample_array(sample: &serde_json::Map<String, Value>, key: &str) -> Vec<String> {
    match sample.get(key).and_then(Value::as_array) {
        Some(values) => values
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect(),
        None => Vec::new(),
    }
}
