/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

// Log access to restricted paths

use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use context::CoreContext;
use metaconfig_types::AclManifestMode;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::NonRootMPath;
use mononoke_types::RepositoryId;
use permission_checker::AclProvider;
use permission_checker::MononokeIdentity;
use scuba_ext::MononokeScubaSampleBuilder;
use scuba_ext::ScubaValue;
use serde_json::Value;
use serde_json::json;

use crate::ManifestType;
use crate::RestrictedManifestId;
use crate::RestrictedPaths;
use crate::restriction_check;
use crate::restriction_check::EnforcementDecision;
use crate::restriction_check::PathRestrictionSource;
use crate::restriction_check::RestrictionCheckResult;
use crate::restriction_check::RestrictionSource;
use crate::restriction_check::SharedFetchHandle;
use crate::restriction_check::SourceRestrictionCheck;
use crate::restriction_check::SourceRestrictionChecks;
use crate::restriction_check::SourceRestrictionError;
use crate::restriction_check::SourceRestrictionResult;
use crate::restriction_check::SourceRestrictionSummary;

#[cfg(test)]
mod tests;

pub const ACCESS_LOG_SCUBA_TABLE: &str = "mononoke_restricted_paths_access_test";

pub(crate) enum RestrictedPathAccessData {
    /// When the tree is accessed by manifest id
    Manifest(RestrictedManifestId, ManifestType),
    /// When the tree is accessed by path
    FullPath { full_path: NonRootMPath },
}

/// Authorization fields that are logged at the top level of a restricted-path
/// access row.
#[derive(Clone, Copy)]
struct LoggedAuthorization {
    has_authorization: bool,
    is_allowlisted_tooling: bool,
    is_rollout_allowlisted: bool,
    is_admin_bypass: bool,
    has_acl_access: bool,
}

/// Top-level restriction and authorization fields for the source that controls
/// the aggregate log row.
struct RestrictedPathAggregateLogData<'a> {
    restricted_paths: Option<&'a [NonRootMPath]>,
    authorization: LoggedAuthorization,
    acls: Vec<&'a MononokeIdentity>,
}

/// Complete Scuba row payload for restricted-path access logging.
struct RestrictedPathLogData<'a> {
    repo_id: RepositoryId,
    access_data: RestrictedPathAccessData,
    aggregate: Option<RestrictedPathAggregateLogData<'a>>,
    considered_restricted_by: Vec<String>,
    enforcement_decision: Option<EnforcementDecision>,
    acl_manifest_mode: Option<AclManifestMode>,
    source_comparison: Option<SourceComparisonLogContext>,
    authorization_detail: Option<String>,
}

/// The enforcement columns of an access-log row.
///
/// Raw Scuba and the schematized logger both write these, so both derive them
/// from the enforcement decision here.
#[derive(Debug)]
struct EnforcementLogFields {
    /// The `enforcement_decision` column, set on every row logged from request
    /// enforcement.
    enforcement_decision: Option<EnforcementDecision>,
    /// The legacy `access_enforcement_enabled` column: `true` when enforced,
    /// `false` when no condition matched or the access was exempted, and
    /// omitted when enforcement was disabled, failed, or was not evaluated for
    /// the row.
    access_enforcement_enabled: Option<bool>,
    /// The `has_enforcement_exemption` column: whether an exemption set
    /// matched the request, omitted when enforcement was disabled (exemptions
    /// are not evaluated then) or not evaluated for the row.
    has_enforcement_exemption: Option<bool>,
}

impl EnforcementLogFields {
    fn new(decision: Option<EnforcementDecision>) -> Self {
        let access_enforcement_enabled = match decision {
            Some(EnforcementDecision::Enforced) => Some(true),
            Some(
                EnforcementDecision::NoConditionMatched { .. } | EnforcementDecision::Exempted,
            ) => Some(false),
            Some(EnforcementDecision::Disabled | EnforcementDecision::Error { .. }) | None => None,
        };
        let has_enforcement_exemption = match decision {
            Some(EnforcementDecision::Enforced) => Some(false),
            Some(EnforcementDecision::Exempted) => Some(true),
            Some(
                EnforcementDecision::NoConditionMatched { exemption_matched }
                | EnforcementDecision::Error { exemption_matched },
            ) => Some(exemption_matched),
            Some(EnforcementDecision::Disabled) | None => None,
        };
        Self {
            enforcement_decision: decision,
            access_enforcement_enabled,
            has_enforcement_exemption,
        }
    }
}

impl From<EnforcementDecision> for ScubaValue {
    fn from(decision: EnforcementDecision) -> Self {
        ScubaValue::from(decision.as_ref())
    }
}

/// Extra fields emitted only when a row compares config and AclManifest source
/// results.
struct SourceComparisonLogContext {
    acl_manifest_mode: AclManifestMode,
    config_error: Option<String>,
    acl_manifest_error: Option<String>,
    shadow_mismatch: bool,
    shadow_mismatch_detail: Option<String>,
}

impl SourceComparisonLogContext {
    fn add_to_scuba(self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add(
            "acl_manifest_mode",
            acl_manifest_mode_as_scuba_value(self.acl_manifest_mode),
        );
        if let Some(config_error) = self.config_error {
            scuba.add("config_error", config_error);
        }
        if let Some(acl_manifest_error) = self.acl_manifest_error {
            scuba.add("acl_manifest_error", acl_manifest_error);
        }
        scuba.add("shadow_mismatch", self.shadow_mismatch);
        if let Some(shadow_mismatch_detail) = self.shadow_mismatch_detail {
            scuba.add("shadow_mismatch_detail", shadow_mismatch_detail);
        }
    }
}

pub(crate) async fn log_source_comparison_access_by_manifest_if_restricted(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    manifest_id: RestrictedManifestId,
    manifest_type: ManifestType,
    acl_manifest_mode: AclManifestMode,
) -> Result<RestrictionCheckResult> {
    let config_result = restriction_check::check_manifest_restriction_from_source(
        ctx,
        restricted_paths,
        manifest_id.clone(),
        manifest_type.clone(),
        RestrictionSource::Config,
    )
    .await
    .map(SourceRestrictionChecks::new)
    .map_err(SourceRestrictionError::from);
    let fetch_acl_manifest = manifest_type == ManifestType::HgAugmented;
    let acl_manifest_result = if fetch_acl_manifest {
        Some(
            restriction_check::check_manifest_restriction_from_source(
                ctx,
                restricted_paths,
                manifest_id.clone(),
                manifest_type.clone(),
                RestrictionSource::AclManifest,
            )
            .await
            .map(SourceRestrictionChecks::new)
            .map_err(SourceRestrictionError::from),
        )
    } else {
        None
    };

    let reconcile_sources = restricted_paths.reconciles_both_mode_sources();
    let log_result = log_source_results_to_scuba(
        ctx,
        restricted_paths.config_based.manifest_id_store().repo_id(),
        &config_result,
        acl_manifest_result.as_ref(),
        acl_manifest_mode,
        reconcile_sources,
        RestrictedPathAccessData::Manifest(manifest_id, manifest_type),
        restricted_paths.scuba.clone(),
    );

    log_result?;
    restriction_check_result_for_source_results(
        acl_manifest_mode,
        reconcile_sources,
        &config_result,
        acl_manifest_result.as_ref(),
    )
}

pub(crate) async fn log_source_comparison_access_by_path_if_restricted(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    path: NonRootMPath,
    cs_id: Option<ChangesetId>,
    acl_manifest_mode: AclManifestMode,
) -> Result<RestrictionCheckResult> {
    let config_result = restriction_check::check_path_restriction_from_source(
        ctx,
        restricted_paths,
        path.clone(),
        PathRestrictionSource::Config,
    )
    .await
    .map(SourceRestrictionChecks::new)
    .map_err(SourceRestrictionError::from);
    let fetch_acl_manifest = cs_id.is_some();
    let acl_manifest_result = match (fetch_acl_manifest, cs_id) {
        (true, Some(cs_id)) => Some(
            restriction_check::check_path_restriction_from_source(
                ctx,
                restricted_paths,
                path.clone(),
                PathRestrictionSource::AclManifest(cs_id),
            )
            .await
            .map(SourceRestrictionChecks::new)
            .map_err(SourceRestrictionError::from),
        ),
        _ => None,
    };

    let reconcile_sources = restricted_paths.reconciles_both_mode_sources();
    let log_result = log_source_results_to_scuba(
        ctx,
        restricted_paths.config_based.manifest_id_store().repo_id(),
        &config_result,
        acl_manifest_result.as_ref(),
        acl_manifest_mode,
        reconcile_sources,
        RestrictedPathAccessData::FullPath { full_path: path },
        restricted_paths.scuba.clone(),
    );

    log_result?;
    restriction_check_result_for_source_results(
        acl_manifest_mode,
        reconcile_sources,
        &config_result,
        acl_manifest_result.as_ref(),
    )
}

fn restriction_check_result_for_source_results<T: SourceRestrictionCheck>(
    acl_manifest_mode: AclManifestMode,
    reconcile_sources: bool,
    config_result: &SourceRestrictionResult<T>,
    acl_manifest_result: Option<&SourceRestrictionResult<T>>,
) -> Result<RestrictionCheckResult> {
    let summary = if acl_manifest_mode == AclManifestMode::Both {
        both_mode_source_summary(reconcile_sources, config_result, acl_manifest_result)
    } else {
        let config = config_result
            .as_ref()
            .map_err(|err| anyhow::anyhow!("{err:#}"))?;
        SourceRestrictionSummary::from_checks(config.as_ref())
    };
    Ok(summary.into_restriction_check_result())
}

pub(crate) fn spawn_log_source_results_with_enforcement<T>(
    ctx: &CoreContext,
    restricted_paths: Arc<RestrictedPaths>,
    access_data: RestrictedPathAccessData,
    acl_manifest_mode: AclManifestMode,
    reconcile_sources: bool,
    enforcement_decision: EnforcementDecision,
    config_handle: Option<SharedFetchHandle<T>>,
    acl_manifest_handle: Option<SharedFetchHandle<T>>,
) where
    T: SourceRestrictionCheck + Send + Sync + 'static,
{
    if config_handle.is_none() && acl_manifest_handle.is_none() {
        return;
    }

    let ctx = ctx.clone();
    mononoke::spawn_task(async move {
        if let Err(err) = log_source_results(
            &ctx,
            &restricted_paths,
            access_data,
            acl_manifest_mode,
            reconcile_sources,
            enforcement_decision,
            config_handle,
            acl_manifest_handle,
        )
        .await
        {
            tracing::error!("Failed to log restricted source results: {:#}", err);
        }
    });
}

/// Log compact source-comparison results to Scuba.
///
/// This emits one row when a source-comparison lookup either finds a
/// restriction or fails. Shadow aggregate fields use the config result; Both
/// aggregate fields use the union of successful source results. Source
/// disagreement is summarized with compact mismatch fields. If every available
/// source completed unrestricted, no row is written.
pub(crate) fn log_source_results_to_scuba<T: SourceRestrictionCheck>(
    ctx: &CoreContext,
    repo_id: RepositoryId,
    config_result: &SourceRestrictionResult<T>,
    acl_manifest_result: Option<&SourceRestrictionResult<T>>,
    acl_manifest_mode: AclManifestMode,
    reconcile_sources: bool,
    access_data: RestrictedPathAccessData,
    scuba: MononokeScubaSampleBuilder,
) -> Result<()> {
    log_source_results_to_scuba_with_enforcement(
        ctx,
        repo_id,
        config_result,
        acl_manifest_result,
        acl_manifest_mode,
        reconcile_sources,
        None,
        access_data,
        scuba,
    )
}

pub(crate) fn log_source_results_to_scuba_with_enforcement<T: SourceRestrictionCheck>(
    ctx: &CoreContext,
    repo_id: RepositoryId,
    config_result: &SourceRestrictionResult<T>,
    acl_manifest_result: Option<&SourceRestrictionResult<T>>,
    acl_manifest_mode: AclManifestMode,
    reconcile_sources: bool,
    enforcement_decision: Option<EnforcementDecision>,
    access_data: RestrictedPathAccessData,
    scuba: MononokeScubaSampleBuilder,
) -> Result<()> {
    let config_source = successful_source_data(SourceKind::Config, Some(config_result));
    let acl_manifest_source = successful_source_data(SourceKind::AclManifest, acl_manifest_result);
    let Some(source_comparison) = source_comparison_log_context(
        config_result,
        acl_manifest_result,
        acl_manifest_mode,
        config_source.as_ref(),
        acl_manifest_source.as_ref(),
    ) else {
        return Ok(());
    };

    let reconciled = match (config_result, acl_manifest_result) {
        (Ok(config), Some(Ok(acl_manifest)))
            if reconcile_sources && acl_manifest_mode == AclManifestMode::Both =>
        {
            Some(restriction_check::ReconciledAuthorization::new_with_detail(
                config.as_ref(),
                acl_manifest.as_ref(),
            ))
        }
        _ => None,
    };
    let aggregate_summary = match reconciled.as_ref() {
        Some((reconciled, _)) => Some(reconciled.to_source_summary()),
        None => aggregate_summary_for_source_results(
            acl_manifest_mode,
            config_result,
            acl_manifest_result,
            config_source.as_ref(),
            acl_manifest_source.as_ref(),
        ),
    };
    let restriction_acls = match aggregate_summary.as_ref() {
        Some(summary) => summary
            .repo_region_acls()
            .iter()
            .map(|acl| {
                MononokeIdentity::from_str(acl)
                    .with_context(|| format!("Failed to parse repo_region_acl {acl}"))
            })
            .collect::<Result<Vec<_>>>()?,
        None => Vec::new(),
    };

    // Shared entry point with the legacy path: applies SCSC override sampling
    // and feeds the schematized logger, not just the legacy Scuba table.
    log_checked_access_to_restricted_path(
        ctx,
        RestrictedPathLogData {
            repo_id,
            access_data,
            aggregate: aggregate_summary
                .as_ref()
                .map(|summary| RestrictedPathAggregateLogData {
                    restricted_paths: Some(summary.restriction_roots()),
                    authorization: LoggedAuthorization {
                        has_authorization: summary.has_authorization(),
                        is_allowlisted_tooling: summary.is_allowlisted_tooling(),
                        is_rollout_allowlisted: summary.is_rollout_allowlisted(),
                        is_admin_bypass: summary.is_admin_bypass(),
                        has_acl_access: summary.has_acl_access(),
                    },
                    acls: restriction_acls.iter().collect(),
                }),
            considered_restricted_by: considered_restricted_by_for_source_results(
                config_source.as_ref(),
                acl_manifest_source.as_ref(),
            ),
            enforcement_decision,
            acl_manifest_mode: None,
            source_comparison: Some(source_comparison),
            authorization_detail: reconciled.as_ref().map(|(_, detail)| detail.to_string()),
        },
        scuba,
    )?;

    config_result
        .as_ref()
        .map(|_| ())
        .map_err(|err| anyhow::anyhow!("{err:#}"))
}

async fn log_source_results<T>(
    ctx: &CoreContext,
    restricted_paths: &RestrictedPaths,
    access_data: RestrictedPathAccessData,
    acl_manifest_mode: AclManifestMode,
    reconcile_sources: bool,
    enforcement_decision: EnforcementDecision,
    config_handle: Option<SharedFetchHandle<T>>,
    acl_manifest_handle: Option<SharedFetchHandle<T>>,
) -> Result<()>
where
    T: SourceRestrictionCheck + Send + Sync + 'static,
{
    let (config_result, acl_manifest_result) = futures::join!(
        async {
            match config_handle.as_ref() {
                Some(handle) => Some(handle.await_result().await),
                None => None,
            }
        },
        async {
            match acl_manifest_handle.as_ref() {
                Some(handle) => Some(handle.await_result().await),
                None => None,
            }
        },
    );

    if matches!(
        acl_manifest_mode,
        AclManifestMode::Shadow | AclManifestMode::Both
    ) {
        let config_result = config_result.ok_or_else(|| {
            anyhow::anyhow!("missing config source for source-comparison logging")
        })?;
        return log_source_results_to_scuba_with_enforcement(
            ctx,
            restricted_paths.config_based.manifest_id_store().repo_id(),
            &config_result,
            acl_manifest_result.as_ref(),
            acl_manifest_mode,
            reconcile_sources,
            Some(enforcement_decision),
            access_data,
            restricted_paths.scuba.clone(),
        );
    }

    let Some((source_name, result)) =
        legacy_logging_result(acl_manifest_mode, config_result, acl_manifest_result)?
    else {
        return Ok(());
    };
    log_source_result_to_legacy_scuba(
        ctx,
        restricted_paths.config_based.manifest_id_store().repo_id(),
        result.as_ref(),
        access_data,
        acl_manifest_mode,
        restricted_paths.scuba.clone(),
        vec![source_name.to_string()],
        Some(enforcement_decision),
    )?;
    Ok(())
}

fn legacy_logging_result<T: SourceRestrictionCheck>(
    acl_manifest_mode: AclManifestMode,
    config_result: Option<SourceRestrictionResult<T>>,
    acl_manifest_result: Option<SourceRestrictionResult<T>>,
) -> Result<Option<(&'static str, SourceRestrictionChecks<T>)>> {
    let selected = match acl_manifest_mode {
        AclManifestMode::Authoritative => {
            acl_manifest_result.map(|result| ("acl_manifest", result))
        }
        _ => config_result.map(|result| ("manifest_db", result)),
    };

    selected
        .map(|(source_name, result)| result.map(|checks| (source_name, checks)))
        .transpose()
        .map_err(anyhow::Error::from)
}

/// Logging summary when the sources were not reconciled: config's outside
/// Both mode, and in Both mode the union of the checks that loaded after a
/// failed read.
fn aggregate_summary_for_source_results<T: SourceRestrictionCheck>(
    acl_manifest_mode: AclManifestMode,
    config_result: &SourceRestrictionResult<T>,
    acl_manifest_result: Option<&SourceRestrictionResult<T>>,
    config_source: Option<&SuccessfulSourceData>,
    acl_manifest_source: Option<&SuccessfulSourceData>,
) -> Option<SourceRestrictionSummary> {
    if acl_manifest_mode != AclManifestMode::Both {
        return config_source.map(|source| source.summary.clone());
    }

    let any_source_succeeded = config_source.is_some() || acl_manifest_source.is_some();
    any_source_succeeded.then(|| {
        SourceRestrictionSummary::from_check_union(
            successful_source_checks(Some(config_result))
                .chain(successful_source_checks(acl_manifest_result)),
        )
    })
}

/// Both-mode logging summary: the reconciled authorization when
/// reconciliation is enabled and both reads succeeded, otherwise the union of
/// the checks that loaded.
fn both_mode_source_summary<T: SourceRestrictionCheck>(
    reconcile_sources: bool,
    config_result: &SourceRestrictionResult<T>,
    acl_manifest_result: Option<&SourceRestrictionResult<T>>,
) -> SourceRestrictionSummary {
    match (config_result, acl_manifest_result) {
        (Ok(config), Some(Ok(acl_manifest))) if reconcile_sources => {
            restriction_check::ReconciledAuthorization::new(config.as_ref(), acl_manifest.as_ref())
                .to_source_summary()
        }
        _ => SourceRestrictionSummary::from_check_union(
            successful_source_checks(Some(config_result))
                .chain(successful_source_checks(acl_manifest_result)),
        ),
    }
}

fn successful_source_checks<T: SourceRestrictionCheck>(
    result: Option<&SourceRestrictionResult<T>>,
) -> impl Iterator<Item = &T> {
    result
        .into_iter()
        .filter_map(|result| result.as_ref().ok())
        .flat_map(|checks| checks.as_ref().iter())
}

/// Build the source-comparison fields for rows that need Shadow telemetry.
///
/// Returns `None` when every source that ran completed unrestricted, matching
/// the old behavior of skipping those rows entirely.
fn source_comparison_log_context<T: SourceRestrictionCheck>(
    config_result: &SourceRestrictionResult<T>,
    acl_manifest_result: Option<&SourceRestrictionResult<T>>,
    acl_manifest_mode: AclManifestMode,
    config_source: Option<&SuccessfulSourceData>,
    acl_manifest_source: Option<&SuccessfulSourceData>,
) -> Option<SourceComparisonLogContext> {
    let any_source_restricted = config_source.is_some_and(|source| source.restricted)
        || acl_manifest_source.is_some_and(|source| source.restricted);
    let any_source_failed =
        is_source_error(Some(config_result)) || is_source_error(acl_manifest_result);
    if !any_source_restricted && !any_source_failed {
        return None;
    }

    Some(SourceComparisonLogContext {
        acl_manifest_mode,
        config_error: config_result.as_ref().err().map(|err| format!("{err:#}")),
        acl_manifest_error: acl_manifest_result
            .and_then(|result| result.as_ref().err().map(|err| format!("{err:#}"))),
        shadow_mismatch: shadow_mismatch_for_source_results(
            Some(config_result),
            acl_manifest_result,
            config_source,
            acl_manifest_source,
        ),
        shadow_mismatch_detail: source_mismatch_detail_for_source_results(
            config_result,
            acl_manifest_result,
            config_source,
            acl_manifest_source,
        ),
    })
}

fn considered_restricted_by_for_source_results(
    config_source: Option<&SuccessfulSourceData>,
    acl_manifest_source: Option<&SuccessfulSourceData>,
) -> Vec<String> {
    let mut restricted_sources = Vec::new();
    if config_source.is_some_and(|source| source.restricted) {
        restricted_sources.push(SourceKind::Config.as_scuba_value().to_string());
    }
    if acl_manifest_source.is_some_and(|source| source.restricted) {
        restricted_sources.push(SourceKind::AclManifest.as_scuba_value().to_string());
    }
    restricted_sources
}

fn is_source_error<T: SourceRestrictionCheck>(result: Option<&SourceRestrictionResult<T>>) -> bool {
    matches!(result, Some(Err(_)))
}

fn source_mismatch_detail_for_source_results<T: SourceRestrictionCheck>(
    config_result: &SourceRestrictionResult<T>,
    acl_manifest_result: Option<&SourceRestrictionResult<T>>,
    config_source: Option<&SuccessfulSourceData>,
    acl_manifest_source: Option<&SuccessfulSourceData>,
) -> Option<String> {
    let differences = source_mismatch_differences(
        config_result,
        acl_manifest_result,
        config_source,
        acl_manifest_source,
    );
    if differences.is_empty() {
        return None;
    }

    let mut detail = serde_json::Map::new();
    if let Some(config_detail) = source_result_detail(Some(config_result), config_source) {
        detail.insert("config".to_string(), config_detail);
    }
    if let Some(acl_manifest_detail) =
        source_result_detail(acl_manifest_result, acl_manifest_source)
    {
        detail.insert("acl_manifest".to_string(), acl_manifest_detail);
    }
    detail.insert("differences".to_string(), json!(differences));
    Some(Value::Object(detail).to_string())
}

/// Returns the queryable Shadow mismatch signal.
///
/// This marks rows that should be investigated: asymmetric source errors, or
/// successful source results that differ in restricted/unrestricted state,
/// authorization outcome, or restriction ACLs. Restriction-root differences are
/// intentionally excluded because AclManifest cannot provide roots by design;
/// they remain in `shadow_mismatch_detail` for diagnosis.
fn shadow_mismatch_for_source_results<T: SourceRestrictionCheck>(
    config_result: Option<&SourceRestrictionResult<T>>,
    acl_manifest_result: Option<&SourceRestrictionResult<T>>,
    config_source: Option<&SuccessfulSourceData>,
    acl_manifest_source: Option<&SuccessfulSourceData>,
) -> bool {
    let error_mismatch = source_error_status(config_result)
        .zip(source_error_status(acl_manifest_result))
        .is_some_and(|(config_error, acl_manifest_error)| config_error != acl_manifest_error);

    if error_mismatch {
        return true;
    }

    config_source
        .zip(acl_manifest_source)
        .is_some_and(|(config, acl_manifest)| {
            let config = &config.comparison;
            let acl_manifest = &acl_manifest.comparison;
            (
                config.restricted,
                config.has_authorization,
                config.restriction_acls.as_slice(),
            ) != (
                acl_manifest.restricted,
                acl_manifest.has_authorization,
                acl_manifest.restriction_acls.as_slice(),
            )
        })
}

fn source_error_status<T: SourceRestrictionCheck>(
    result: Option<&SourceRestrictionResult<T>>,
) -> Option<bool> {
    result.map(|result| result.is_err())
}

fn source_mismatch_differences<T: SourceRestrictionCheck>(
    config_result: &SourceRestrictionResult<T>,
    acl_manifest_result: Option<&SourceRestrictionResult<T>>,
    config_source: Option<&SuccessfulSourceData>,
    acl_manifest_source: Option<&SuccessfulSourceData>,
) -> Vec<&'static str> {
    let error_differences = [
        is_source_error(Some(config_result)).then_some(SourceKind::Config.error_field()),
        is_source_error(acl_manifest_result).then_some(SourceKind::AclManifest.error_field()),
    ];

    let successful_source_differences = match (config_source, acl_manifest_source) {
        (Some(config), Some(acl_manifest)) => [
            (
                config.comparison.restricted != acl_manifest.comparison.restricted,
                "restricted",
            ),
            (
                config.comparison.has_authorization != acl_manifest.comparison.has_authorization,
                "has_authorization",
            ),
            (
                config.comparison.restriction_acls != acl_manifest.comparison.restriction_acls,
                "restriction_acls",
            ),
            (
                config.comparison.restriction_paths != acl_manifest.comparison.restriction_paths,
                "restriction_paths",
            ),
        ]
        .into_iter()
        .filter_map(|(is_different, field)| is_different.then_some(field))
        .collect(),
        _ => Vec::new(),
    };

    error_differences
        .into_iter()
        .flatten()
        .chain(successful_source_differences)
        .collect()
}

fn source_result_detail(
    result: Option<&SourceRestrictionResult<impl SourceRestrictionCheck>>,
    source: Option<&SuccessfulSourceData>,
) -> Option<Value> {
    match result? {
        Ok(_) => source.map(successful_source_detail),
        Err(err) => Some(json!({
            "error": format!("{:#}", err),
        })),
    }
}

fn successful_source_detail(source: &SuccessfulSourceData) -> Value {
    json!({
        "restricted": source.comparison.restricted,
        "has_authorization": source.comparison.has_authorization,
        "restriction_acls": &source.comparison.restriction_acls,
        "restriction_paths": &source.comparison.restriction_paths,
    })
}

fn successful_source_data<T: SourceRestrictionCheck>(
    source_kind: SourceKind,
    result: Option<&SourceRestrictionResult<T>>,
) -> Option<SuccessfulSourceData> {
    let result = match result? {
        Ok(result) => result,
        Err(_) => return None,
    };
    let summary = SourceRestrictionSummary::from_checks(result.as_ref());
    let restricted = result.is_restricted();
    let comparison = SourceComparisonData {
        restricted,
        has_authorization: summary.has_authorization(),
        restriction_acls: summary.sorted_repo_region_acls(),
        restriction_paths: restriction_paths_for_source::<T>(source_kind, &summary),
    };
    Some(SuccessfulSourceData {
        restricted,
        summary,
        comparison,
    })
}

fn restriction_paths_for_source<T: SourceRestrictionCheck>(
    source_kind: SourceKind,
    summary: &SourceRestrictionSummary,
) -> Option<Vec<String>> {
    (matches!(source_kind, SourceKind::Config) || T::reports_restriction_roots())
        .then(|| summary.sorted_restriction_root_strings())
}

struct SuccessfulSourceData {
    restricted: bool,
    summary: SourceRestrictionSummary,
    comparison: SourceComparisonData,
}

#[derive(Eq, PartialEq)]
struct SourceComparisonData {
    restricted: bool,
    has_authorization: bool,
    restriction_acls: Vec<String>,
    restriction_paths: Option<Vec<String>>,
}

fn log_source_result_to_legacy_scuba(
    ctx: &CoreContext,
    repo_id: RepositoryId,
    result: &[impl SourceRestrictionCheck],
    access_data: RestrictedPathAccessData,
    acl_manifest_mode: AclManifestMode,
    scuba: MononokeScubaSampleBuilder,
    considered_restricted_by: Vec<String>,
    enforcement_decision: Option<EnforcementDecision>,
) -> Result<RestrictionCheckResult> {
    let summary = SourceRestrictionSummary::from_checks(result);
    let check_result = summary.clone().into_restriction_check_result();
    if result.is_empty() {
        return Ok(check_result);
    }

    let restriction_acls = summary
        .repo_region_acls()
        .iter()
        .map(|acl| {
            MononokeIdentity::from_str(acl)
                .with_context(|| format!("Failed to parse repo_region_acl {acl}"))
        })
        .collect::<Result<Vec<_>>>()?;
    let restriction_acl_refs = restriction_acls.iter().collect::<Vec<_>>();

    log_checked_access_to_restricted_path(
        ctx,
        RestrictedPathLogData {
            repo_id,
            access_data,
            aggregate: Some(RestrictedPathAggregateLogData {
                restricted_paths: Some(summary.restriction_roots()),
                authorization: LoggedAuthorization {
                    has_authorization: summary.has_authorization(),
                    is_allowlisted_tooling: summary.is_allowlisted_tooling(),
                    is_rollout_allowlisted: summary.is_rollout_allowlisted(),
                    is_admin_bypass: summary.is_admin_bypass(),
                    has_acl_access: summary.has_acl_access(),
                },
                acls: restriction_acl_refs,
            }),
            considered_restricted_by,
            enforcement_decision,
            acl_manifest_mode: Some(acl_manifest_mode),
            source_comparison: None,
            authorization_detail: None,
        },
        scuba,
    )?;

    Ok(check_result)
}

fn acl_manifest_mode_as_scuba_value(acl_manifest_mode: AclManifestMode) -> &'static str {
    match acl_manifest_mode {
        AclManifestMode::Disabled => "disabled",
        AclManifestMode::Shadow => "shadow",
        AclManifestMode::Both => "both",
        AclManifestMode::Authoritative => "authoritative",
    }
}

#[derive(Clone, Copy)]
enum SourceKind {
    Config,
    AclManifest,
}

impl SourceKind {
    fn error_field(self) -> &'static str {
        match self {
            Self::Config => "config_error",
            Self::AclManifest => "acl_manifest_error",
        }
    }

    fn as_scuba_value(self) -> &'static str {
        match self {
            Self::Config => "manifest_db",
            Self::AclManifest => "acl_manifest",
        }
    }
}

// ============================================================================
// Schematized logger implementation (fbcode_build only)
// ============================================================================

#[cfg(fbcode_build)]
mod schematized_logger {
    use anyhow::Result;
    use context::CoreContext;
    use mononoke_restricted_paths_access_rust_logger::MononokeRestrictedPathsAccessLogger;
    use mononoke_types::NonRootMPath;
    use mononoke_types::RepositoryId;
    use permission_checker::MononokeIdentity;
    use scuba_ext::CommonMetadata;
    use scuba_ext::CommonServerData;

    use super::EnforcementLogFields;
    use super::RestrictedPathAccessData;

    /// Log access to the schematized logger for restricted paths.
    ///
    /// This logs to both Scuba and Hive via the MononokeRestrictedPathsAccessLogger.
    pub fn log_access_to_schematized_logger(
        ctx: &CoreContext,
        repo_id: RepositoryId,
        restricted_paths: &[NonRootMPath],
        access_data: &RestrictedPathAccessData,
        has_authorization: bool,
        is_allowlisted_tooling: bool,
        is_rollout_allowlisted: bool,
        is_admin_bypass: bool,
        enforcement: EnforcementLogFields,
        acls: &[&MononokeIdentity],
    ) -> Result<()> {
        let mut logger = MononokeRestrictedPathsAccessLogger::new(ctx.fb);

        // Add common server data using shared struct
        let server_data = CommonServerData::collect();
        apply_server_data(&mut logger, &server_data);

        // Add metadata using shared struct
        let metadata =
            CommonMetadata::from_metadata(ctx.metadata(), ctx.scuba().observability_context());
        apply_metadata(&mut logger, &metadata);
        logger.set_client_path_acl_compatibility(
            ctx.metadata()
                .client_path_acl_compatibility()
                .as_str()
                .to_owned(),
        );

        // Set core access fields
        logger.set_repo_id(repo_id.id() as i64);
        logger.set_restricted_paths(
            restricted_paths
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>(),
        );
        logger.set_has_authorization(has_authorization.to_string());
        logger.set_is_allowlisted_tooling(is_allowlisted_tooling.to_string());
        logger.set_is_rollout_allowlisted(is_rollout_allowlisted.to_string());
        logger.set_is_admin_bypass(is_admin_bypass);
        if let Some(value) = enforcement.enforcement_decision {
            logger.set_enforcement_decision(value.as_ref().to_owned());
        }
        if let Some(value) = enforcement.access_enforcement_enabled {
            logger.set_access_enforcement_enabled(value);
        }
        if let Some(value) = enforcement.has_enforcement_exemption {
            logger.set_has_enforcement_exemption(value);
        }
        logger.set_acls(acls.iter().map(|acl| acl.to_string()).collect::<Vec<_>>());

        // Set access data variant fields
        match access_data {
            RestrictedPathAccessData::Manifest(manifest_id, manifest_type) => {
                logger.set_manifest_id(manifest_id.to_string());
                logger.set_manifest_type(manifest_type.to_string());
            }
            RestrictedPathAccessData::FullPath { full_path } => {
                logger.set_full_path(full_path.to_string());
            }
        }

        logger.log_async()?;
        Ok(())
    }

    /// Apply CommonServerData fields to the schematized logger.
    fn apply_server_data(
        logger: &mut MononokeRestrictedPathsAccessLogger,
        data: &CommonServerData,
    ) {
        if let Some(ref hostname) = data.server_hostname {
            logger.set_server_hostname(hostname.clone());
        }
        if let Some(ref region) = data.region {
            logger.set_region(region.clone());
        }
        if let Some(ref dc) = data.datacenter {
            logger.set_datacenter(dc.clone());
        }
        if let Some(ref dc_prefix) = data.region_datacenter_prefix {
            logger.set_region_datacenter_prefix(dc_prefix.clone());
        }
        if let Some(ref tier) = data.server_tier {
            logger.set_server_tier(tier.clone());
        }
        if let Some(ref tw_task_id) = data.tw_task_id {
            logger.set_tw_task_id(tw_task_id.clone());
        }
        if let Some(ref tw_canary_id) = data.tw_canary_id {
            logger.set_tw_canary_id(tw_canary_id.clone());
        }
        if let Some(ref tw_handle) = data.tw_handle {
            logger.set_tw_handle(tw_handle.clone());
        }
        if let Some(ref tw_task_handle) = data.tw_task_handle {
            logger.set_tw_task_handle(tw_task_handle.clone());
        }
        if let Some(ref cluster) = data.chronos_cluster {
            logger.set_chronos_cluster(cluster.clone());
        }
        if let Some(ref id) = data.chronos_job_instance_id {
            logger.set_chronos_job_instance_id(id.clone());
        }
        if let Some(ref name) = data.chronos_job_name {
            logger.set_chronos_job_name(name.clone());
        }
        if let Some(ref rev) = data.build_revision {
            logger.set_build_revision(rev.clone());
        }
        if let Some(ref rule) = data.build_rule {
            logger.set_build_rule(rule.clone());
        }
    }

    /// Apply CommonMetadata fields to the schematized logger.
    fn apply_metadata(logger: &mut MononokeRestrictedPathsAccessLogger, data: &CommonMetadata) {
        logger.set_session_uuid(data.session_uuid.clone());
        logger.set_client_identities(data.client_identities.clone());
        if !data.unverified_forwarded_identities.is_empty() {
            logger
                .set_unverified_forwarded_identities(data.unverified_forwarded_identities.clone());
        }
        if let Some(ref verifier) = data.forwarded_cats_verifier {
            logger.set_forwarded_cats_verifier(verifier.clone());
        }
        if !data.forwarded_cats_token_verifiers.is_empty() {
            logger.set_forwarded_cats_token_verifiers(data.forwarded_cats_token_verifiers.clone());
        }
        if let Some(ref client_id) = data.upstream_client_id {
            logger.set_client_id(client_id.clone());
        }

        if let Some(ref hostname) = data.source_hostname {
            logger.set_source_hostname(hostname.clone());
        }
        if let Some(ref ip) = data.client_ip {
            logger.set_client_ip(ip.clone());
        }
        if let Some(ref unix_name) = data.unix_username {
            logger.set_unix_username(unix_name.clone());
        }
        if let Some(ref main_id) = data.client_main_id {
            logger.set_client_main_id(main_id.clone());
        }
        if let Some(ref entry_point) = data.client_entry_point {
            logger.set_client_entry_point(entry_point.clone());
        }
        if let Some(ref correlator) = data.client_correlator {
            logger.set_client_correlator(correlator.clone());
        }
        if !data.enabled_experiments_jk.is_empty() {
            logger.set_enabled_experiments_jk(data.enabled_experiments_jk.clone());
        }
        if let Some(ref alias) = data.sandcastle_alias {
            logger.set_sandcastle_alias(alias.clone());
        }
        if let Some(ref vcs) = data.sandcastle_vcs {
            logger.set_sandcastle_vcs(vcs.clone());
        }
        if let Some(ref region) = data.revproxy_region {
            logger.set_revproxy_region(region.clone());
        }
        if let Some(ref nonce) = data.sandcastle_nonce {
            logger.set_sandcastle_nonce(nonce.clone());
        }
        if let Some(ref tw_job) = data.client_tw_job {
            logger.set_client_tw_job(tw_job.clone());
        }
        if let Some(ref tw_task) = data.client_tw_task {
            logger.set_client_tw_task(tw_task.clone());
        }
        if let Some(ref atlas) = data.client_atlas {
            logger.set_client_atlas(atlas.clone());
        }
        if let Some(ref env_id) = data.client_atlas_env_id {
            logger.set_client_atlas_env_id(env_id.clone());
        }
        if let Some(ref faas_job_name) = data.client_faas_job_name {
            logger.set_client_faas_job_name(faas_job_name.clone());
        }
        if let Some(ref cause) = data.fetch_cause {
            logger.set_fetch_cause(cause.clone());
        }
        logger.set_fetch_from_cas_attempted(data.fetch_from_cas_attempted);
    }
}

/// `restrictions` pairs each matched repo-region ACL with that tent's rollout
/// allowlist group, so the allowlist is evaluated per tent rather than repo-wide.
pub(crate) async fn log_access_to_restricted_path(
    ctx: &CoreContext,
    repo_id: RepositoryId,
    restricted_paths: Vec<NonRootMPath>,
    restrictions: Vec<(&MononokeIdentity, Option<&MononokeIdentity>)>,
    access_data: RestrictedPathAccessData,
    acl_manifest_mode: AclManifestMode,
    acl_provider: Arc<dyn AclProvider>,
    tooling_allowlist_group: Option<&str>,
    admin_bypass_group: Option<&MononokeIdentity>,
    scuba: MononokeScubaSampleBuilder,
    considered_restricted_by: Vec<String>,
) -> Result<RestrictionCheckResult> {
    let authorization = restriction_check::check_authorization(
        ctx,
        &acl_provider,
        &restrictions,
        tooling_allowlist_group,
        admin_bypass_group,
    )
    .await?;
    let acls = restrictions.into_iter().map(|(acl, _)| acl).collect();

    let result = restriction_check::build_restriction_check_result(
        authorization.has_authorization(),
        restricted_paths.clone(),
    );
    log_checked_access_to_restricted_path(
        ctx,
        RestrictedPathLogData {
            repo_id,
            access_data,
            aggregate: Some(RestrictedPathAggregateLogData {
                restricted_paths: Some(&restricted_paths),
                authorization: LoggedAuthorization {
                    has_authorization: authorization.has_authorization(),
                    is_allowlisted_tooling: authorization.is_allowlisted_tooling(),
                    is_rollout_allowlisted: authorization.is_rollout_allowlisted(),
                    is_admin_bypass: authorization.is_admin_bypass(),
                    has_acl_access: authorization.has_acl_access(),
                },
                acls,
            }),
            considered_restricted_by,
            enforcement_decision: None,
            acl_manifest_mode: Some(acl_manifest_mode),
            source_comparison: None,
            authorization_detail: None,
        },
        scuba,
    )?;

    Ok(result)
}

fn log_checked_access_to_restricted_path(
    ctx: &CoreContext,
    log_data: RestrictedPathLogData<'_>,
    scuba: MononokeScubaSampleBuilder,
) -> Result<()> {
    // Override sampling for unauthorized SCSC accesses to restricted paths
    #[cfg(fbcode_build)]
    {
        use clientinfo::ClientEntryPoint;

        let is_scsc = ctx
            .metadata()
            .client_request_info()
            .is_some_and(|cri| cri.entry_point == ClientEntryPoint::ScsClient);

        if let Some(aggregate) = log_data.aggregate.as_ref()
            && is_scsc
            && !aggregate.authorization.has_authorization
        {
            ctx.set_override_sampling();
        }
    }

    // Log to schematized logger (logs to both Scuba and Hive) if enabled via JK
    // Only available in fbcode builds
    #[cfg(fbcode_build)]
    {
        let use_schematized_logger = justknobs::eval(
            "scm/mononoke:restricted_paths_use_schematized_logger",
            None,
            None,
        );

        if let Some(aggregate) = log_data.aggregate.as_ref()
            && use_schematized_logger
        {
            if let Err(e) = schematized_logger::log_access_to_schematized_logger(
                ctx,
                log_data.repo_id,
                aggregate.restricted_paths.unwrap_or(&[]),
                &log_data.access_data,
                aggregate.authorization.has_authorization,
                aggregate.authorization.is_allowlisted_tooling,
                aggregate.authorization.is_rollout_allowlisted,
                aggregate.authorization.is_admin_bypass,
                EnforcementLogFields::new(log_data.enforcement_decision),
                &aggregate.acls,
            ) {
                tracing::error!("Failed to log to schematized logger: {:?}", e);
            }
        }
    }

    log_access_to_scuba(ctx, log_data, scuba)
}

fn log_access_to_scuba(
    ctx: &CoreContext,
    log_data: RestrictedPathLogData<'_>,
    mut scuba: MononokeScubaSampleBuilder,
) -> Result<()> {
    scuba.add_metadata(ctx.metadata());
    scuba.add(
        "client_path_acl_compatibility",
        ctx.metadata().client_path_acl_compatibility().as_str(),
    );
    scuba.add_opt("client_id", ctx.metadata().upstream_client_id());

    if let Some(user_agent) = ctx.metadata().user_agent() {
        scuba.add("http_user_agent", user_agent);
    }

    scuba.add_common_server_data();

    // We want to log all samples
    scuba.unsampled();

    scuba.add("repo_id", log_data.repo_id.id());

    if let Some(aggregate) = log_data.aggregate {
        if let Some(restricted_paths) = aggregate.restricted_paths {
            scuba.add(
                "restricted_paths",
                restricted_paths
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
            );
        }
        scuba.add(
            "has_authorization",
            aggregate.authorization.has_authorization,
        );
        scuba.add(
            "is_allowlisted_tooling",
            aggregate.authorization.is_allowlisted_tooling,
        );
        scuba.add(
            "is_rollout_allowlisted",
            aggregate.authorization.is_rollout_allowlisted,
        );
        scuba.add("is_admin_bypass", aggregate.authorization.is_admin_bypass);
        scuba.add("has_acl_access", aggregate.authorization.has_acl_access);
        scuba.add(
            "acls",
            aggregate
                .acls
                .into_iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
        );
    }

    // Log access data based on the type
    match log_data.access_data {
        RestrictedPathAccessData::Manifest(manifest_id, manifest_type) => {
            scuba.add("manifest_id", manifest_id.to_string());
            scuba.add("manifest_type", manifest_type.to_string());
        }
        RestrictedPathAccessData::FullPath { full_path, .. } => {
            scuba.add("full_path", full_path.to_string());
        }
    }

    scuba.add(
        "considered_restricted_by",
        log_data.considered_restricted_by,
    );
    if let Some(acl_manifest_mode) = log_data.acl_manifest_mode {
        scuba.add(
            "acl_manifest_mode",
            acl_manifest_mode_as_scuba_value(acl_manifest_mode),
        );
    }
    if let Some(source_comparison) = log_data.source_comparison {
        source_comparison.add_to_scuba(&mut scuba);
    }
    let enforcement = EnforcementLogFields::new(log_data.enforcement_decision);
    if let Some(enforcement_decision) = enforcement.enforcement_decision {
        scuba.add("enforcement_decision", enforcement_decision);
    }
    if let Some(access_enforcement_enabled) = enforcement.access_enforcement_enabled {
        scuba.add("access_enforcement_enabled", access_enforcement_enabled);
    }
    if let Some(has_enforcement_exemption) = enforcement.has_enforcement_exemption {
        scuba.add("has_enforcement_exemption", has_enforcement_exemption);
    }
    if let Some(authorization_detail) = log_data.authorization_detail {
        scuba.add("authorization_detail", authorization_detail);
    }

    scuba.log();

    Ok(())
}
