/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Restriction check helpers that turn restriction lookup results into
//! authorization results.

use std::cell::OnceCell;
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
#[cfg(all(fbcode_build, target_os = "linux"))]
use build_info::BuildInfo;
use context::CoreContext;
use futures::FutureExt;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future::BoxFuture;
use futures::future::Shared;
use futures::stream;
use itertools::Itertools;
use metaconfig_types::EnforcementConditionSet;
use metaconfig_types::RequestMatchers;
use metaconfig_types::RestrictedPathsConfig;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::NonRootMPath;
use permission_checker::AclProvider;
use permission_checker::MononokeIdentity;
use permission_checker::MononokeIdentitySet;
use permission_checker::PermissionCheckerBuilder;
use tokio::task::JoinHandle;

use crate::DenyingRestriction;
use crate::ManifestType;
use crate::RestrictedManifestId;
use crate::RestrictedPaths;
use crate::restriction_info;
use crate::restriction_info::ManifestRestrictionInfo;
use crate::restriction_info::PathRestrictionInfo;

#[cfg(test)]
mod tests;

/// Identity users should request for access to a restricted path.
pub type PermissionRequestGroup = MononokeIdentity;

/// Whether an access is enforced, and if so whether it is denied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AccessEnforcementOutcome {
    /// No enforcement condition set matched the access, so it is allowed
    /// whatever the caller's authorization.
    NotEnforced,
    /// An enforcement condition set matched the access. The denying restriction
    /// is present when the caller is unauthorized and the access must be
    /// denied, and absent when the caller is authorized.
    Enforced { denial: Option<DenyingRestriction> },
    /// An enforcement condition set matched the access, but so did an
    /// enforcement exemption set, so the access is allowed whatever the
    /// caller's authorization.
    Exempted,
}

impl AccessEnforcementOutcome {
    /// Apply a matching enforcement exemption to an outcome computed from
    /// successfully read sources: an enforced access becomes exempted, and
    /// every other outcome is unchanged.
    pub(crate) fn exempt_if(self, exemption_matched: bool) -> Self {
        match self {
            Self::Enforced { .. } if exemption_matched => Self::Exempted,
            Self::NotEnforced | Self::Enforced { .. } | Self::Exempted => self,
        }
    }

    /// The denying restriction, if the access is denied.
    pub(crate) fn denial(&self) -> Option<&DenyingRestriction> {
        match self {
            Self::NotEnforced | Self::Exempted => None,
            Self::Enforced { denial } => denial.as_ref(),
        }
    }
}

/// How enforcement was decided for an access, as recorded in the access log.
///
/// Unlike [`AccessEnforcementOutcome`], this also covers accesses where
/// enforcement was never evaluated or failed to evaluate, and carries no
/// denial details. Its `snake_case` variant name is the value of the
/// `enforcement_decision` access-log column.
///
/// Every evaluated decision records whether an enforcement exemption set
/// matched the request, including when that did not change the outcome (no
/// condition matched, or a source failed).
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum EnforcementDecision {
    /// Enforcement is off for the repo: the kill switch is off or no
    /// enforcement condition sets are configured. Exemptions are not evaluated.
    Disabled,
    /// No enforcement condition set matched the access.
    NoConditionMatched { exemption_matched: bool },
    /// An enforcement condition set matched the access, no exemption set did,
    /// and it was enforced.
    Enforced,
    /// An enforcement condition set and an enforcement exemption set both
    /// matched the access, so it was not enforced.
    Exempted,
    /// Evaluating enforcement failed, so the access failed closed. Only
    /// logged in the `Shadow` and `Both` modes: in the other modes a failed
    /// source read writes no access-log row.
    Error { exemption_matched: bool },
}

impl EnforcementDecision {
    /// The logged decision for an evaluated access. `exemption_matched` is
    /// whether an exemption set matched the request: it is recorded for
    /// `NoConditionMatched` and `Error`, and implied by the outcome otherwise
    /// (an `Enforced` outcome means no exemption matched).
    pub(crate) fn from_outcome(
        outcome: &Result<AccessEnforcementOutcome>,
        exemption_matched: bool,
    ) -> Self {
        match outcome {
            Ok(AccessEnforcementOutcome::NotEnforced) => {
                Self::NoConditionMatched { exemption_matched }
            }
            Ok(AccessEnforcementOutcome::Enforced { .. }) => Self::Enforced,
            Ok(AccessEnforcementOutcome::Exempted) => Self::Exempted,
            Err(_) => Self::Error { exemption_matched },
        }
    }
}

/// Source to use for path-side restriction checks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PathRestrictionSource {
    /// Config-backed path ACLs.
    Config,
    /// AclManifest at a specific changeset.
    AclManifest(ChangesetId),
}

/// Where a restriction is declared.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RestrictionSource {
    /// The repo config: `path_restriction_metadata`, plus the manifest-id
    /// store for manifest accesses.
    Config,
    /// `.slacl` files, read through the AclManifest derived data.
    AclManifest,
}

/// Result from restricted path access check — carries both authorization
/// and restriction root info for enforcement condition evaluation.
#[derive(Debug, Clone)]
pub struct RestrictionCheckResult {
    /// Whether the caller has read authorization for the restriction.
    pub has_authorization: bool,
    /// The restriction root paths matched by this access check.
    /// Empty if the path is not restricted.
    pub restriction_roots: Vec<NonRootMPath>,
}

/// Authorization flags produced by evaluating the caller against one source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AuthorizationCheckResult {
    /// Whether the caller has direct read access to every matching path ACL.
    has_acl_access: FactorResult,
    /// Whether the caller is in the tooling allowlist group.
    is_allowlisted_tooling: FactorResult,
    /// Whether the caller is in the rollout allowlist group.
    is_rollout_allowlisted: FactorResult,
    /// Whether the caller is in the admin bypass group. Tracked separately from
    /// `has_acl_access` so a bypass grant is distinguishable from genuine ACL
    /// read access in the access logs.
    is_admin_bypass: FactorResult,
}

/// Result of evaluating one authorization factor.
///
/// `NotSet` represents absent policy, while `Unauthorized` represents a
/// configured policy that the caller did not satisfy. Fetch and evaluation
/// failures remain errors and never become factor observations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FactorResult {
    NotSet,
    Authorized,
    Unauthorized,
}

impl FactorResult {
    fn from_authorized(authorized: bool) -> Self {
        if authorized {
            Self::Authorized
        } else {
            Self::Unauthorized
        }
    }

    fn is_authorized(self) -> bool {
        self == Self::Authorized
    }

    fn from_optional_authorized(configured: bool, authorized: bool) -> Self {
        if configured {
            Self::from_authorized(authorized)
        } else {
            Self::NotSet
        }
    }
}

/// Repo-wide allowlist authorization, shared by every restriction in one source
/// batch. The rollout allowlist is deliberately absent: it is configured per
/// tent, so it has to be evaluated per restriction rather than once per batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AllowlistAuthorization {
    is_allowlisted_tooling: FactorResult,
    is_admin_bypass: FactorResult,
}

impl AllowlistAuthorization {
    fn into_authorization_check_result(
        self,
        has_acl_access: FactorResult,
        is_rollout_allowlisted: FactorResult,
    ) -> AuthorizationCheckResult {
        AuthorizationCheckResult {
            has_acl_access,
            is_allowlisted_tooling: self.is_allowlisted_tooling,
            is_rollout_allowlisted,
            is_admin_bypass: self.is_admin_bypass,
        }
    }
}

impl AuthorizationCheckResult {
    pub(crate) fn new(
        has_acl_access: bool,
        is_allowlisted_tooling: bool,
        is_rollout_allowlisted: bool,
        is_admin_bypass: bool,
    ) -> Self {
        Self {
            has_acl_access: FactorResult::from_authorized(has_acl_access),
            is_allowlisted_tooling: FactorResult::from_authorized(is_allowlisted_tooling),
            is_rollout_allowlisted: FactorResult::from_authorized(is_rollout_allowlisted),
            is_admin_bypass: FactorResult::from_authorized(is_admin_bypass),
        }
    }

    /// Whether the caller has read authorization through ACLs, allowlists, or
    /// the admin bypass group.
    pub(crate) fn has_authorization(&self) -> bool {
        self.has_acl_access.is_authorized()
            || self.is_allowlisted_tooling.is_authorized()
            || self.is_rollout_allowlisted.is_authorized()
            || self.is_admin_bypass.is_authorized()
    }

    pub(crate) fn has_acl_access(&self) -> bool {
        self.has_acl_access.is_authorized()
    }

    pub(crate) fn is_allowlisted_tooling(&self) -> bool {
        self.is_allowlisted_tooling.is_authorized()
    }

    pub(crate) fn is_rollout_allowlisted(&self) -> bool {
        self.is_rollout_allowlisted.is_authorized()
    }

    pub(crate) fn is_admin_bypass(&self) -> bool {
        self.is_admin_bypass.is_authorized()
    }

    fn acl_result(&self) -> FactorResult {
        self.has_acl_access
    }

    fn rollout_result(&self) -> FactorResult {
        self.is_rollout_allowlisted
    }

    fn tooling_result(&self) -> FactorResult {
        self.is_allowlisted_tooling
    }

    fn admin_result(&self) -> FactorResult {
        self.is_admin_bypass
    }
}

/// Path restriction information paired with the caller's authorization result.
#[derive(Clone, Debug, PartialEq)]
pub struct PathRestrictionCheckResult {
    restriction_info: PathRestrictionInfo,
    authorization: AuthorizationCheckResult,
    repo_region_acl: MononokeIdentity,
}

impl PathRestrictionCheckResult {
    pub(crate) fn new(
        restriction_info: PathRestrictionInfo,
        authorization: AuthorizationCheckResult,
        repo_region_acl: MononokeIdentity,
    ) -> Self {
        Self {
            restriction_info,
            authorization,
            repo_region_acl,
        }
    }

    /// Restriction information for the checked path.
    pub fn restriction_info(&self) -> &PathRestrictionInfo {
        &self.restriction_info
    }

    /// Whether the caller has read authorization through ACLs or allowlists.
    pub fn has_authorization(&self) -> bool {
        self.authorization.has_authorization()
    }

    /// Consume this result and return the restriction information.
    pub fn into_restriction_info(self) -> PathRestrictionInfo {
        self.restriction_info
    }
}

/// Manifest restriction information paired with the caller's authorization result.
#[derive(Clone, Debug, PartialEq)]
pub struct ManifestRestrictionCheckResult {
    restriction_info: ManifestRestrictionInfo,
    authorization: AuthorizationCheckResult,
    repo_region_acl: MononokeIdentity,
}

impl ManifestRestrictionCheckResult {
    pub(crate) fn new(
        restriction_info: ManifestRestrictionInfo,
        authorization: AuthorizationCheckResult,
        repo_region_acl: MononokeIdentity,
    ) -> Self {
        Self {
            restriction_info,
            authorization,
            repo_region_acl,
        }
    }

    /// Restriction information for the checked manifest.
    pub fn restriction_info(&self) -> &ManifestRestrictionInfo {
        &self.restriction_info
    }

    /// Whether the caller has read authorization through ACLs or allowlists.
    pub fn has_authorization(&self) -> bool {
        self.authorization.has_authorization()
    }

    /// Whether the caller has direct read access to the matching path ACL.
    pub fn has_acl_access(&self) -> bool {
        self.authorization.has_acl_access()
    }

    /// Whether the caller is in the tooling allowlist group.
    pub fn is_allowlisted_tooling(&self) -> bool {
        self.authorization.is_allowlisted_tooling()
    }

    /// Whether the caller is in the rollout allowlist group.
    pub fn is_rollout_allowlisted(&self) -> bool {
        self.authorization.is_rollout_allowlisted()
    }

    pub fn is_admin_bypass(&self) -> bool {
        self.authorization.is_admin_bypass()
    }
}

/// Result returned by one restriction source fetch.
///
/// `SourceRestrictionChecks<T>` carries the `SourceRestrictionCheck` bound, so
/// this result is still limited to typed source check results without relying on
/// Rust's non-enforcing type-alias bounds.
pub(crate) type SourceRestrictionResult<T> =
    std::result::Result<SourceRestrictionChecks<T>, SourceRestrictionError>;

/// Successful restriction check results returned by one source.
///
/// The results are shared behind an `Arc` so multiple consumers can observe the
/// same typed checks without cloning every item.
#[derive(Clone)]
pub(crate) struct SourceRestrictionChecks<T: SourceRestrictionCheck> {
    inner: Arc<Vec<T>>,
}

impl<T: SourceRestrictionCheck> SourceRestrictionChecks<T> {
    pub(crate) fn new(checks: Vec<T>) -> Self {
        Self {
            inner: Arc::new(checks),
        }
    }

    pub(crate) fn is_restricted(&self) -> bool {
        !self.inner.is_empty()
    }
}

impl<T: SourceRestrictionCheck> AsRef<[T]> for SourceRestrictionChecks<T> {
    fn as_ref(&self) -> &[T] {
        self.inner.as_slice()
    }
}

/// Shared restriction-source error.
///
/// Source errors may be logged and surfaced after the same fetch completes, so
/// the error is shared while preserving the original error as its source.
#[derive(Clone, Debug)]
pub(crate) struct SourceRestrictionError {
    inner: Arc<anyhow::Error>,
}

impl From<anyhow::Error> for SourceRestrictionError {
    fn from(error: anyhow::Error) -> Self {
        Self {
            inner: Arc::new(error),
        }
    }
}

impl std::fmt::Display for SourceRestrictionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Propagate the alternate flag so `format!("{:#}", err)` emits the full
        // anyhow chain joined by ": "; otherwise only the top-level context is
        // shown and inner causes are dropped from Scuba logs.
        if f.alternate() {
            write!(f, "{:#}", self.inner.as_ref())
        } else {
            write!(f, "{}", self.inner.as_ref())
        }
    }
}

impl std::error::Error for SourceRestrictionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.inner.chain().nth(1)
    }
}

/// Common read-only view over typed source check results.
///
/// Path and manifest checks keep their concrete restriction-info types, but
/// logging and enforcement need a small common surface to derive aggregate
/// authorization, ACL, and restriction-root fields without copying data into a
/// flattened adapter struct.
pub(crate) trait SourceRestrictionCheck: Clone {
    /// Caller authorization result for this source result.
    fn authorization(&self) -> &AuthorizationCheckResult;
    /// Permission request group to show users for this restriction.
    fn permission_request_group(&self) -> &PermissionRequestGroup;
    /// Parsed repo-region ACL associated with this restriction.
    fn repo_region_identity(&self) -> &MononokeIdentity;
    /// Restriction root when the source can report one.
    fn restriction_root(&self) -> Option<&NonRootMPath>;
    /// Whether this check type carries restriction roots independent of source.
    fn reports_restriction_roots() -> bool;
}

impl SourceRestrictionCheck for PathRestrictionCheckResult {
    fn authorization(&self) -> &AuthorizationCheckResult {
        &self.authorization
    }

    fn permission_request_group(&self) -> &PermissionRequestGroup {
        &self.restriction_info().permission_request_group
    }

    fn repo_region_identity(&self) -> &MononokeIdentity {
        &self.repo_region_acl
    }

    fn restriction_root(&self) -> Option<&NonRootMPath> {
        Some(&self.restriction_info().restriction_root)
    }

    fn reports_restriction_roots() -> bool {
        true
    }
}

impl SourceRestrictionCheck for ManifestRestrictionCheckResult {
    fn authorization(&self) -> &AuthorizationCheckResult {
        &self.authorization
    }

    fn permission_request_group(&self) -> &PermissionRequestGroup {
        &self.restriction_info().permission_request_group
    }

    fn repo_region_identity(&self) -> &MononokeIdentity {
        &self.repo_region_acl
    }

    fn restriction_root(&self) -> Option<&NonRootMPath> {
        self.restriction_info().restriction_root.as_ref()
    }

    fn reports_restriction_roots() -> bool {
        false
    }
}

/// Aggregate view over one source's typed restriction checks.
///
/// Logging derives authorization, ACL, and restriction-root summaries from
/// this type. `has_authorization` is the decision for the checks that loaded,
/// made as enforcement makes it: every restriction is authorized by at least
/// one of its own factors. The factor fields aggregate each factor on its own,
/// so a request can be authorized even when no single factor covers every
/// restriction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceRestrictionSummary {
    has_authorization: bool,
    authorization: AuthorizationCheckResult,
    repo_region_acls: Vec<String>,
    restriction_roots: Vec<NonRootMPath>,
}

impl SourceRestrictionSummary {
    pub(crate) fn from_checks(checks: &[impl SourceRestrictionCheck]) -> Self {
        let has_authorization = all_checks_authorized(checks);
        let authorization = aggregate_single_source_authorization(checks);
        let repo_region_acls = checks
            .iter()
            .map(|check| check.repo_region_identity().to_string())
            .collect();
        let restriction_roots = checks
            .iter()
            .filter_map(SourceRestrictionCheck::restriction_root)
            .cloned()
            .collect();

        Self {
            has_authorization,
            authorization,
            repo_region_acls,
            restriction_roots,
        }
    }

    pub(crate) fn from_check_union<'a, T>(checks: impl IntoIterator<Item = &'a T>) -> Self
    where
        T: SourceRestrictionCheck + 'a,
    {
        let checks = checks.into_iter().collect::<Vec<_>>();
        let has_authorization = all_checks_authorized(checks.iter().copied());
        let has_acl_access = checks
            .iter()
            .all(|check| check.authorization().has_acl_access());
        let is_allowlisted_tooling = checks
            .iter()
            .any(|check| check.authorization().is_allowlisted_tooling());
        let is_rollout_allowlisted = all_rollout_allowlisted(
            checks
                .iter()
                .map(|check| check.authorization().is_rollout_allowlisted()),
        );
        let is_admin_bypass = checks
            .iter()
            .any(|check| check.authorization().is_admin_bypass());
        let repo_region_acls = checks
            .iter()
            .map(|check| check.repo_region_identity().to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let restriction_roots = checks
            .iter()
            .filter_map(|check| check.restriction_root().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        Self {
            has_authorization,
            authorization: AuthorizationCheckResult::new(
                has_acl_access,
                is_allowlisted_tooling,
                is_rollout_allowlisted,
                is_admin_bypass,
            ),
            repo_region_acls,
            restriction_roots,
        }
    }

    pub(crate) fn has_authorization(&self) -> bool {
        self.has_authorization
    }

    pub(crate) fn has_acl_access(&self) -> bool {
        self.authorization.has_acl_access()
    }

    pub(crate) fn is_allowlisted_tooling(&self) -> bool {
        self.authorization.is_allowlisted_tooling()
    }

    pub(crate) fn is_rollout_allowlisted(&self) -> bool {
        self.authorization.is_rollout_allowlisted()
    }

    pub(crate) fn is_admin_bypass(&self) -> bool {
        self.authorization.is_admin_bypass()
    }

    pub(crate) fn repo_region_acls(&self) -> &[String] {
        &self.repo_region_acls
    }

    pub(crate) fn restriction_roots(&self) -> &[NonRootMPath] {
        &self.restriction_roots
    }

    pub(crate) fn restriction_root_strings(&self) -> Vec<String> {
        self.restriction_roots
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    pub(crate) fn sorted_repo_region_acls(&self) -> Vec<String> {
        let mut acls = self.repo_region_acls.clone();
        acls.sort();
        acls
    }

    pub(crate) fn sorted_restriction_root_strings(&self) -> Vec<String> {
        let mut paths = self.restriction_root_strings();
        paths.sort();
        paths
    }

    pub(crate) fn into_restriction_check_result(self) -> RestrictionCheckResult {
        build_restriction_check_result(self.has_authorization(), self.restriction_roots)
    }
}

/// Cloneable handle for a spawned source fetch.
#[derive(Clone)]
pub(crate) struct SharedFetchHandle<T: SourceRestrictionCheck> {
    inner: Shared<BoxFuture<'static, SourceRestrictionResult<T>>>,
}

impl<T: SourceRestrictionCheck + Send + Sync + 'static> SharedFetchHandle<T> {
    pub(crate) fn from_join_handle(handle: JoinHandle<Result<Vec<T>>>) -> Self {
        Self::from_future(async move { handle.await.map_err(anyhow::Error::from)? })
    }

    pub(crate) fn from_future(
        fetch: impl Future<Output = Result<Vec<T>>> + Send + 'static,
    ) -> Self {
        let inner = async move {
            fetch
                .await
                .map(SourceRestrictionChecks::new)
                .map_err(SourceRestrictionError::from)
        }
        .boxed()
        .shared();
        Self { inner }
    }

    pub(crate) fn from_result(result: Result<Vec<T>>) -> Self {
        Self::from_future(futures::future::ready(result))
    }

    pub(crate) async fn await_result(&self) -> SourceRestrictionResult<T> {
        self.inner.clone().await
    }
}

/// Request-local result of evaluating enforcement condition sets before
/// fetching restriction data.
///
/// Entry point and request-flag filters can be evaluated from `CoreContext`
/// alone, so callers can skip restriction fetches when no set can match this
/// request. `always_enabled` sets are definite matches. Remaining candidates
/// still need fetched restriction ACLs before enforcement can decide whether
/// the accessed restricted root is in scope.
pub(crate) enum PreFilterResult<'a> {
    NoMatch,
    DefiniteMatch {
        candidates: Vec<&'a EnforcementConditionSet>,
    },
    NeedsFetch {
        candidates: Vec<&'a EnforcementConditionSet>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreFilterVariant {
    Definite,
    NeedsFetch,
}

/// Check if the caller has direct read access to every repo region ACL in
/// `acls` (conjunctive evaluation).
///
/// For nested restrictions (e.g. `/secret` plus `/secret/inner`), the caller
/// must satisfy the inner ACL even when they already satisfy the outer one —
/// otherwise being a member of an outer ACL would silently bypass an inner
/// restriction. This checks ACL read access only; admin-bypass-group membership
/// is evaluated separately (see [`check_allowlist_authorization`]) so a bypass
/// grant stays distinguishable from genuine ACL read access in the access logs.
pub(crate) async fn has_read_access_to_repo_region(
    ctx: &CoreContext,
    acl_provider: &Arc<dyn AclProvider>,
    acls: &[&MononokeIdentity],
) -> Result<bool> {
    if acls.is_empty() {
        return Ok(true);
    }
    has_repo_region_acls_for_action(ctx, acl_provider, acls, "read").await
}

/// Check if the caller has maintainer access to every repo region ACL in `acls`
/// (conjunctive evaluation), or is a member of `admin_bypass_group`.
///
/// Mirrors `has_read_access_to_repo_region` but checks the `maintainers`
/// action instead of `read`. Used to gate `.slacl` ACL-file modifications while
/// letting admins in `admin_bypass_group` through even when they do not maintain
/// the affected ACL.
pub async fn has_maintainer_access_to_repo_region(
    ctx: &CoreContext,
    acl_provider: &Arc<dyn AclProvider>,
    acls: &[&MononokeIdentity],
    admin_bypass_group: Option<&MononokeIdentity>,
) -> Result<bool> {
    has_repo_region_access(ctx, acl_provider, acls, "maintainers", admin_bypass_group).await
}

/// Check whether the caller has `action` access to every repo region ACL in
/// `acls`, or is a member of `admin_bypass_group`. An empty `acls` slice is
/// unrestricted. The bypass-group and per-ACL checks run concurrently and both
/// always run to completion; access is granted if either succeeds, and an error
/// in either fails closed.
async fn has_repo_region_access(
    ctx: &CoreContext,
    acl_provider: &Arc<dyn AclProvider>,
    acls: &[&MononokeIdentity],
    action: &str,
    admin_bypass_group: Option<&MononokeIdentity>,
) -> Result<bool> {
    if acls.is_empty() {
        return Ok(true);
    }

    let (in_bypass_group, has_acl_access) = futures::try_join!(
        is_in_optional_group(ctx, acl_provider, admin_bypass_group),
        has_repo_region_acls_for_action(ctx, acl_provider, acls, action),
    )?;
    Ok(in_bypass_group || has_acl_access)
}

/// Check that the caller has direct `action` access to every repo region ACL in
/// `acls`. Runs checks concurrently and short-circuits on the first deny or
/// error.
async fn has_repo_region_acls_for_action(
    ctx: &CoreContext,
    acl_provider: &Arc<dyn AclProvider>,
    acls: &[&MononokeIdentity],
    action: &str,
) -> Result<bool> {
    let identities = ctx.metadata().identities();
    stream::iter(acls.iter().copied())
        .map(|acl| async move {
            let checker = acl_provider
                .repo_region_acl(acl.id_data())
                .await
                .with_context(|| {
                    format!("Failed to create PermissionChecker for {}", acl.id_data())
                })?;
            let permission_checker = PermissionCheckerBuilder::new().allow(checker).build();
            anyhow::Ok(permission_checker.check_set(identities, &[action]).await)
        })
        .boxed()
        .buffer_unordered(acls.len())
        .try_all(futures::future::ready)
        .await
        .inspect_err(|err| {
            ctx.scuba().clone().log_with_msg(
                "Failed repo region ACL check",
                format!("ACLs: {acls:?}. Action {action}. Exception: {err:#?}"),
            );
        })
}

/// Check whether the caller is a member of the given group identity, if one is
/// configured. Returns `false` when no group is set — an absent group never
/// grants access.
pub(crate) async fn is_in_optional_group(
    ctx: &CoreContext,
    acl_provider: &Arc<dyn AclProvider>,
    group: Option<&MononokeIdentity>,
) -> Result<bool> {
    match group {
        Some(group) => is_part_of_group(ctx, acl_provider, group.id_data()).await,
        None => Ok(false),
    }
}

/// Check if the caller is a member of the given group.
pub(crate) async fn is_part_of_group(
    ctx: &CoreContext,
    acl_provider: &Arc<dyn AclProvider>,
    group_name: &str,
) -> Result<bool> {
    let membership_checker = acl_provider
        .group(group_name)
        .await
        .with_context(|| format!("Failed to get membership checker for group {group_name}"))
        .inspect_err(|err| {
            ctx.scuba().clone().log_with_msg(
                "Failed to get membership checker for group",
                format!("Group: {group_name}. Exception: {err:#?}"),
            );
        })?;

    Ok(membership_checker
        .is_member(ctx.metadata().identities())
        .await)
}

/// Evaluate ACL and allowlist authorization for a restricted-path access.
///
/// `restrictions` pairs each matching repo-region ACL with that tent's rollout
/// allowlist group. The caller counts as rollout-allowlisted only when every
/// restriction allowlists them, so being allowlisted for one tent cannot
/// authorize a different tent caught by the same access.
pub(crate) async fn check_authorization(
    ctx: &CoreContext,
    acl_provider: &Arc<dyn AclProvider>,
    restrictions: &[(&MononokeIdentity, Option<&MononokeIdentity>)],
    tooling_allowlist_group: Option<&str>,
    admin_bypass_group: Option<&MononokeIdentity>,
) -> Result<AuthorizationCheckResult> {
    let has_rollout_allowlist = restrictions.iter().any(|(_, group)| group.is_some());
    let acls = restrictions.iter().map(|(acl, _)| *acl).collect::<Vec<_>>();
    let allowlist_authorization = check_allowlist_authorization(
        ctx,
        acl_provider,
        tooling_allowlist_group,
        admin_bypass_group,
    )
    .await?;
    let (has_acl_access, rollout_allowlisted) = tokio::try_join!(
        has_read_access_to_repo_region(ctx, acl_provider, &acls),
        stream::iter(restrictions)
            .map(|(_, group)| is_in_optional_group(ctx, acl_provider, *group))
            .buffered(10)
            .try_collect::<Vec<_>>(),
    )?;
    Ok(allowlist_authorization.into_authorization_check_result(
        FactorResult::from_authorized(has_acl_access),
        FactorResult::from_optional_authorized(
            has_rollout_allowlist,
            all_rollout_allowlisted(rollout_allowlisted),
        ),
    ))
}

async fn check_restricted_paths_allowlist_authorization(
    ctx: &CoreContext,
    restricted_paths: &RestrictedPaths,
) -> Result<AllowlistAuthorization> {
    check_allowlist_authorization(
        ctx,
        restricted_paths.acl_provider(),
        restricted_paths.config().tooling_allowlist_group.as_deref(),
        restricted_paths.config().admin_bypass_group.as_ref(),
    )
    .await
}

async fn check_allowlist_authorization(
    ctx: &CoreContext,
    acl_provider: &Arc<dyn AclProvider>,
    tooling_allowlist_group: Option<&str>,
    admin_bypass_group: Option<&MononokeIdentity>,
) -> Result<AllowlistAuthorization> {
    let (is_allowlisted_tooling, is_admin_bypass) = tokio::try_join!(
        check_optional_allowlist_group(ctx, acl_provider, tooling_allowlist_group),
        is_in_optional_group(ctx, acl_provider, admin_bypass_group),
    )?;
    Ok(AllowlistAuthorization {
        is_allowlisted_tooling,
        is_admin_bypass: FactorResult::from_optional_authorized(
            admin_bypass_group.is_some(),
            is_admin_bypass,
        ),
    })
}

async fn check_optional_allowlist_group(
    ctx: &CoreContext,
    acl_provider: &Arc<dyn AclProvider>,
    group_name: Option<&str>,
) -> Result<FactorResult> {
    match group_name {
        Some(group_name) => is_part_of_group(ctx, acl_provider, group_name)
            .await
            .map(FactorResult::from_authorized),
        None => Ok(FactorResult::NotSet),
    }
}

/// Build the legacy restricted-path check result shape.
pub(crate) fn build_restriction_check_result(
    has_authorization: bool,
    restriction_roots: Vec<NonRootMPath>,
) -> RestrictionCheckResult {
    RestrictionCheckResult {
        has_authorization,
        restriction_roots,
    }
}

/// Check restriction-root paths and authorization for one or more paths.
pub(crate) async fn get_path_restriction_root_check(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    cs_id: Option<ChangesetId>,
    paths: &[NonRootMPath],
) -> Result<Vec<PathRestrictionCheckResult>> {
    // TODO(T289977216): In Both mode, fetch both sources and reconcile their
    // tri-state factors instead of selecting one source's metadata.
    let restriction_info =
        restriction_info::get_path_restriction_root_info(restricted_paths, ctx, cs_id, paths)
            .await?;
    check_path_restriction_infos(ctx, restricted_paths, restriction_info).await
}

/// Check ancestor path restrictions and authorization for one or more paths.
pub(crate) async fn get_path_restriction_check(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    cs_id: Option<ChangesetId>,
    paths: &[NonRootMPath],
) -> Result<Vec<PathRestrictionCheckResult>> {
    // TODO(T289977216): In Both mode, fetch both sources and reconcile their
    // tri-state factors instead of selecting one source's metadata.
    let restriction_info =
        restriction_info::get_path_restriction_info(restricted_paths, ctx, cs_id, paths).await?;
    check_path_restriction_infos(ctx, restricted_paths, restriction_info).await
}

/// Check manifest restrictions and authorization for one manifest source.
pub(crate) async fn get_manifest_restriction_check(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    manifest_id: &RestrictedManifestId,
    manifest_type: &ManifestType,
    source: RestrictionSource,
) -> Result<Vec<ManifestRestrictionCheckResult>> {
    let restriction_info = match source {
        RestrictionSource::Config => restriction_info::get_manifest_restriction_info_from_config(
            restricted_paths,
            ctx,
            manifest_id,
            manifest_type,
        )
        .await
        .context("find config restrictions for manifest-side check")?,
        RestrictionSource::AclManifest => {
            restriction_info::get_manifest_restriction_info_from_acl_manifest(
                restricted_paths,
                ctx,
                manifest_id,
                manifest_type,
            )
            .await
            .context("find AclManifest restrictions for manifest-side check")?
        }
    };
    check_manifest_restriction_infos(ctx, restricted_paths, restriction_info).await
}

/// Check manifest restrictions using the repository's configured lookup behavior.
pub(crate) async fn get_manifest_restriction_check_for_current_behavior(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    manifest_id: &RestrictedManifestId,
    manifest_type: &ManifestType,
) -> Result<Vec<ManifestRestrictionCheckResult>> {
    // TODO(T289977216): In Both mode, fetch both sources and reconcile their
    // tri-state factors instead of selecting one source's metadata.
    let restriction_info = restriction_info::get_manifest_restriction_info(
        restricted_paths,
        ctx,
        manifest_id,
        manifest_type,
    )
    .await?;
    check_manifest_restriction_infos(ctx, restricted_paths, restriction_info).await
}

/// Request-local result of evaluating a repo's enforcement condition and
/// exemption sets, before any restriction data is fetched.
pub(crate) struct PreFilteredRequest<'a> {
    /// Condition sets that can still match this access.
    pub(crate) conditions: PreFilterResult<'a>,
    /// Whether an exemption set matches this request. Exemptions only have
    /// request-local filters, so this is final before any fetch.
    pub(crate) exemption_matched: bool,
}

/// Evaluate a repo's enforcement condition and exemption sets against the
/// request metadata, before any restriction data is fetched.
pub(crate) fn pre_filter_request<'a>(
    ctx: &CoreContext,
    config: &'a RestrictedPathsConfig,
) -> PreFilteredRequest<'a> {
    let request = RequestFacts::new(ctx);
    PreFilteredRequest {
        conditions: pre_filter_condition_sets(&request, &config.enforcement_condition_sets),
        exemption_matched: config
            .enforcement_exemption_sets
            .iter()
            .any(|set| request.matches(set.matchers())),
    }
}

/// Apply the request-local portion of `enforcement_condition_sets`.
///
/// This is intentionally split from restriction ACL matching: request metadata
/// is available before fetching restrictions, but `restriction_acls` can only
/// be compared after the accessed restricted roots are known. Splitting the
/// checks lets enforcement avoid unnecessary fetches for requests that cannot
/// match any condition set while keeping restriction-scoped enforcement precise.
fn pre_filter_condition_sets<'a>(
    request: &RequestFacts<'_>,
    condition_sets: &'a [EnforcementConditionSet],
) -> PreFilterResult<'a> {
    let candidates = condition_sets
        .iter()
        .filter(|set| {
            condition_set_has_active_filter(set)
                && (set.always_enabled || request.matches_condition_set(set))
        })
        .collect::<Vec<_>>();

    if candidates.is_empty() {
        return PreFilterResult::NoMatch;
    }

    if candidates.iter().any(|set| set.always_enabled) {
        PreFilterResult::DefiniteMatch { candidates }
    } else {
        PreFilterResult::NeedsFetch { candidates }
    }
}

/// Request metadata that the request-local filters of enforcement condition
/// and exemption sets are matched against.
///
/// Built once per access so every set is evaluated against the same snapshot
/// of the request.
struct RequestFacts<'a> {
    entry_point: Option<String>,
    server_side_tenting: bool,
    client_partial_responses_flag: bool,
    machine_tier: Option<&'a str>,
    build_rule: Option<&'static str>,
    identities: &'a MononokeIdentitySet,
    /// `identities` rendered for regex matching, built the first time a set
    /// with `client_identity_regexes` is evaluated and then shared by every
    /// set, so a request evaluated against no identity-filtered set never
    /// renders them.
    identity_strings: OnceCell<Vec<String>>,
    is_agent: bool,
}

impl<'a> RequestFacts<'a> {
    fn new(ctx: &'a CoreContext) -> Self {
        let metadata = ctx.metadata();
        Self {
            entry_point: metadata
                .client_request_info()
                .map(|cri| cri.entry_point.to_string()),
            server_side_tenting: ctx.session().server_side_tenting(),
            client_partial_responses_flag: metadata.partial_responses_opt_in()
                || metadata.partial_responses_opt_out(),
            machine_tier: metadata.machine_tier(),
            build_rule: server_build_rule(),
            identities: metadata.identities(),
            identity_strings: OnceCell::new(),
            is_agent: metadata.likely_an_agent(),
        }
    }

    /// Whether every non-empty matcher in `matchers` matches this request.
    fn matches(&self, matchers: &RequestMatchers) -> bool {
        let entry_point_matches = matchers.entry_points.is_empty()
            || self
                .entry_point
                .as_ref()
                .is_some_and(|entry_point| matchers.entry_points.contains(entry_point));
        let machine_tier_matches = matchers.machine_tiers.is_empty()
            || self
                .machine_tier
                .is_some_and(|tier| matchers.machine_tiers.iter().any(|c| c == tier));
        let build_rule_matches = matchers.build_rules.is_empty()
            || self
                .build_rule
                .is_some_and(|rule| matchers.build_rules.iter().any(|c| c == rule));
        let identity_regex_matches = matchers.client_identity_regexes.is_empty()
            || self
                .identity_strings
                .get_or_init(|| self.identities.iter().map(ToString::to_string).collect())
                .iter()
                .any(|identity| {
                    matchers
                        .client_identity_regexes
                        .iter()
                        .any(|re| re.is_match(identity))
                });
        let is_agent_matches = matchers.is_agent.is_none_or(|want| want == self.is_agent);

        entry_point_matches
            && machine_tier_matches
            && build_rule_matches
            && identity_regex_matches
            && is_agent_matches
    }

    /// Whether the request-local filters of `set` match this request: its
    /// request matchers and `require_client_request_flag`.
    ///
    /// `always_enabled` and `restriction_acls` are the caller's to evaluate.
    fn matches_condition_set(&self, set: &EnforcementConditionSet) -> bool {
        self.matches(&set.matchers)
            && (!set.require_client_request_flag
                || self.server_side_tenting
                || self.client_partial_responses_flag)
    }
}

fn condition_set_has_active_filter(set: &EnforcementConditionSet) -> bool {
    set.always_enabled
        || set.require_client_request_flag
        || !set.restriction_acls.is_empty()
        || set.matchers.has_matcher()
}

/// The build rule of the running server binary (the Buck target it was built
/// from), as exposed by `build_info` and logged to scuba by
/// `add_common_server_data`. Returns `None` outside fbcode Linux builds: the
/// Meta-internal `build_info` crate is unavailable in OSS builds, and its build
/// rule symbol is only populated on Linux.
fn server_build_rule() -> Option<&'static str> {
    #[cfg(all(fbcode_build, target_os = "linux"))]
    {
        Some(BuildInfo::get_rule())
    }
    #[cfg(not(all(fbcode_build, target_os = "linux")))]
    {
        None
    }
}

pub(crate) fn condition_sets_match_restriction_acls(
    condition_sets: &[&EnforcementConditionSet],
    restriction_acls: &[&MononokeIdentity],
) -> bool {
    condition_sets.iter().any(|set| {
        condition_set_has_active_filter(set)
            && (set.restriction_acls.is_empty()
                || set
                    .restriction_acls
                    .iter()
                    .any(|condition_acl| restriction_acls.contains(&condition_acl)))
    })
}

/// Evaluate one authoritative source for enforcement outcome.
///
/// Returns `Enforced` when the source matches the active enforcement condition
/// sets, with a denial permission request group only when a matching
/// restriction denies the caller. Returns `NotEnforced` when the fetched source
/// does not match restriction-ACL-scoped condition sets. Returns `Err(_)` when
/// fetching or evaluating the source fails.
pub(crate) async fn source_enforcement_outcome<'a, T>(
    handle: &SharedFetchHandle<T>,
    candidates: &[&'a EnforcementConditionSet],
    pre_filter_variant: &PreFilterVariant,
) -> Result<AccessEnforcementOutcome>
where
    T: SourceRestrictionCheck + Send + Sync + 'static,
{
    let result = handle.await_result().await?;
    Ok(completed_source_enforcement_outcome(
        result.as_ref(),
        candidates,
        pre_filter_variant,
    ))
}

/// Combine authoritative source enforcement outcomes.
///
/// Returns a denial when any source denies access, even if a sibling source
/// failed. Returns the first source error only when no source denies.
pub(crate) fn authoritative_sources_enforcement_outcome(
    source_outcomes: Vec<Result<AccessEnforcementOutcome>>,
) -> Result<AccessEnforcementOutcome> {
    let (outcomes, errors): (Vec<_>, Vec<_>) = source_outcomes.into_iter().partition_result();
    // Exemptions apply after aggregation, so no single source is exempted.
    let enforced = outcomes.iter().any(|outcome| match outcome {
        AccessEnforcementOutcome::Enforced { .. } => true,
        AccessEnforcementOutcome::NotEnforced | AccessEnforcementOutcome::Exempted => false,
    });

    if let Some(denial) = outcomes
        .into_iter()
        .find(|outcome| outcome.denial().is_some())
    {
        return Ok(denial);
    }
    match errors.into_iter().next() {
        Some(error) => Err(error),
        None if enforced => Ok(AccessEnforcementOutcome::Enforced { denial: None }),
        None => Ok(AccessEnforcementOutcome::NotEnforced),
    }
}

/// Order denied checks to make single-ACL error reporting deterministic.
///
/// Restriction roots are the most stable user-facing ordering key when they
/// are available. Rootless manifest checks sort after rooted checks, and the
/// permission request group breaks ties.
fn compare_denied_checks<T: SourceRestrictionCheck>(left: &T, right: &T) -> Ordering {
    compare_restriction_roots(left, right).then_with(|| {
        left.permission_request_group()
            .cmp(right.permission_request_group())
    })
}

/// Check a path against one selected restriction source.
///
/// Normal callers should use `get_path_restriction_check`, which follows the
/// repository's configured lookup behavior. This helper exists for
/// source-comparison paths that must fetch config and AclManifest independently
/// before logging or selecting an authoritative result.
pub(crate) async fn check_path_restriction_from_source(
    ctx: &CoreContext,
    restricted_paths: &RestrictedPaths,
    path: NonRootMPath,
    source: PathRestrictionSource,
) -> Result<Vec<PathRestrictionCheckResult>> {
    match source {
        PathRestrictionSource::Config => {
            check_config_path_restriction_infos(ctx, restricted_paths, &path).await
        }
        PathRestrictionSource::AclManifest(cs_id) => {
            let restriction_info = restriction_info::get_path_restriction_info_from_acl_manifest(
                restricted_paths,
                ctx,
                cs_id,
                std::slice::from_ref(&path),
            )
            .await
            .context("find AclManifest restrictions for path-side fetch")?;
            check_path_restriction_infos(ctx, restricted_paths, restriction_info).await
        }
    }
}

pub(crate) fn spawn_path_restriction_check(
    ctx: &CoreContext,
    restricted_paths: Arc<RestrictedPaths>,
    path: NonRootMPath,
    source: PathRestrictionSource,
) -> SharedFetchHandle<PathRestrictionCheckResult> {
    let ctx = ctx.clone();
    let handle = mononoke::spawn_task(async move {
        check_path_restriction_from_source(&ctx, &restricted_paths, path, source).await
    });
    SharedFetchHandle::from_join_handle(handle)
}

/// Check a manifest against one selected restriction source.
///
/// Normal callers should use `get_manifest_restriction_check`, which exposes
/// the configured manifest check primitive. This helper exists for
/// source-comparison paths that need separate config and AclManifest results.
pub(crate) async fn check_manifest_restriction_from_source(
    ctx: &CoreContext,
    restricted_paths: &RestrictedPaths,
    manifest_id: RestrictedManifestId,
    manifest_type: ManifestType,
    source: RestrictionSource,
) -> Result<Vec<ManifestRestrictionCheckResult>> {
    get_manifest_restriction_check(restricted_paths, ctx, &manifest_id, &manifest_type, source)
        .await
}

pub(crate) fn spawn_manifest_restriction_check(
    ctx: &CoreContext,
    restricted_paths: Arc<RestrictedPaths>,
    manifest_id: RestrictedManifestId,
    manifest_type: ManifestType,
    source: RestrictionSource,
) -> SharedFetchHandle<ManifestRestrictionCheckResult> {
    let ctx = ctx.clone();
    let handle = mononoke::spawn_task(async move {
        check_manifest_restriction_from_source(
            &ctx,
            &restricted_paths,
            manifest_id,
            manifest_type,
            source,
        )
        .await
    });
    SharedFetchHandle::from_join_handle(handle)
}

pub async fn check_path_restriction_infos(
    ctx: &CoreContext,
    restricted_paths: &RestrictedPaths,
    restriction_info: Vec<PathRestrictionInfo>,
) -> Result<Vec<PathRestrictionCheckResult>> {
    if restriction_info.is_empty() {
        return Ok(vec![]);
    }

    let allowlist_authorization =
        check_restricted_paths_allowlist_authorization(ctx, restricted_paths).await?;
    stream::iter(restriction_info)
        .map(|restriction_info| async move {
            let (authorization, acl) = check_restriction_authorization(
                ctx,
                restricted_paths,
                &restriction_info.repo_region_acl,
                restriction_info.rollout_allowlist_group.as_ref(),
                allowlist_authorization,
            )
            .await?;
            Ok(PathRestrictionCheckResult::new(
                restriction_info,
                authorization,
                acl,
            ))
        })
        .buffered(100)
        .try_collect()
        .await
}

pub async fn check_config_path_restriction_infos(
    ctx: &CoreContext,
    restricted_paths: &RestrictedPaths,
    path: &NonRootMPath,
) -> Result<Vec<PathRestrictionCheckResult>> {
    let restriction_info = restricted_paths
        .config()
        .path_restriction_metadata
        .iter()
        .filter(|(prefix, _)| prefix.is_prefix_of(path))
        .map(|(restriction_root, metadata)| {
            (
                PathRestrictionInfo {
                    restriction_root: restriction_root.clone(),
                    permission_request_group: metadata.effective_permission_request_group(),
                    repo_region_acl: metadata.repo_region_acl.to_string(),
                    rollout_allowlist_group: metadata.rollout_allowlist_group.clone(),
                },
                metadata.repo_region_acl.clone(),
            )
        })
        .collect::<Vec<_>>();
    if restriction_info.is_empty() {
        return Ok(vec![]);
    }

    let allowlist_authorization =
        check_restricted_paths_allowlist_authorization(ctx, restricted_paths).await?;
    stream::iter(restriction_info)
        .map(|(restriction_info, acl)| async move {
            let authorization = check_restriction_authorization_with_acl(
                ctx,
                restricted_paths,
                &acl,
                restriction_info.rollout_allowlist_group.as_ref(),
                allowlist_authorization,
            )
            .await?;
            Ok(PathRestrictionCheckResult::new(
                restriction_info,
                authorization,
                acl,
            ))
        })
        .buffered(100)
        .try_collect()
        .await
}

async fn check_manifest_restriction_infos(
    ctx: &CoreContext,
    restricted_paths: &RestrictedPaths,
    restriction_info: Vec<ManifestRestrictionInfo>,
) -> Result<Vec<ManifestRestrictionCheckResult>> {
    if restriction_info.is_empty() {
        return Ok(vec![]);
    }

    let allowlist_authorization =
        check_restricted_paths_allowlist_authorization(ctx, restricted_paths).await?;
    stream::iter(restriction_info)
        .map(|restriction_info| async move {
            let (authorization, acl) = check_restriction_authorization(
                ctx,
                restricted_paths,
                &restriction_info.repo_region_acl,
                restriction_info.rollout_allowlist_group.as_ref(),
                allowlist_authorization,
            )
            .await?;
            Ok(ManifestRestrictionCheckResult::new(
                restriction_info,
                authorization,
                acl,
            ))
        })
        .buffered(100)
        .try_collect()
        .await
}

async fn check_restriction_authorization(
    ctx: &CoreContext,
    restricted_paths: &RestrictedPaths,
    repo_region_acl: &str,
    rollout_allowlist_group: Option<&MononokeIdentity>,
    allowlist_authorization: AllowlistAuthorization,
) -> Result<(AuthorizationCheckResult, MononokeIdentity)> {
    let acl = MononokeIdentity::from_str(repo_region_acl)
        .with_context(|| format!("Failed to parse repo_region_acl {repo_region_acl}"))?;
    let authorization = check_restriction_authorization_with_acl(
        ctx,
        restricted_paths,
        &acl,
        rollout_allowlist_group,
        allowlist_authorization,
    )
    .await?;
    Ok((authorization, acl))
}

async fn check_restriction_authorization_with_acl(
    ctx: &CoreContext,
    restricted_paths: &RestrictedPaths,
    acl: &MononokeIdentity,
    rollout_allowlist_group: Option<&MononokeIdentity>,
    allowlist_authorization: AllowlistAuthorization,
) -> Result<AuthorizationCheckResult> {
    let acl_provider = restricted_paths.acl_provider();
    let acls = [acl];
    let (has_acl_access, is_rollout_allowlisted) = tokio::try_join!(
        has_read_access_to_repo_region(ctx, acl_provider, &acls),
        is_in_optional_group(ctx, acl_provider, rollout_allowlist_group),
    )?;
    Ok(allowlist_authorization.into_authorization_check_result(
        FactorResult::from_authorized(has_acl_access),
        FactorResult::from_optional_authorized(
            rollout_allowlist_group.is_some(),
            is_rollout_allowlisted,
        ),
    ))
}

/// Preserve the legacy single-source contract: ACL and rollout require every
/// restriction, while repository-wide tooling and admin grants require any.
fn aggregate_single_source_authorization(
    checks: &[impl SourceRestrictionCheck],
) -> AuthorizationCheckResult {
    AuthorizationCheckResult::new(
        checks
            .iter()
            .all(|check| check.authorization().acl_result().is_authorized()),
        checks
            .iter()
            .any(|check| check.authorization().tooling_result().is_authorized()),
        all_rollout_allowlisted(
            checks
                .iter()
                .map(|check| check.authorization().rollout_result().is_authorized()),
        ),
        checks
            .iter()
            .any(|check| check.authorization().admin_result().is_authorized()),
    )
}

/// Aggregate per-restriction rollout-allowlist flags for one request.
///
/// Every restriction must allowlist the caller: being allowlisted for one tent
/// must not authorize reads of a different tent caught by the same request.
///
/// The emptiness guard is load-bearing. `all()` is vacuously true, and an empty
/// batch means nothing was restricted — already authorized via `has_acl_access`,
/// but not *rollout-allowlisted*. Without the guard every unrestricted access
/// would be logged as rollout-allowlisted.
fn all_rollout_allowlisted(flags: impl IntoIterator<Item = bool>) -> bool {
    let mut flags = flags.into_iter().peekable();
    flags.peek().is_some() && flags.all(|allowlisted| allowlisted)
}

/// Whether every restriction is authorized by at least one of its own factors.
fn all_checks_authorized<'a, T: SourceRestrictionCheck + 'a>(
    checks: impl IntoIterator<Item = &'a T>,
) -> bool {
    checks
        .into_iter()
        .all(|check| check.authorization().has_authorization())
}

/// Calculate enforcement after one source fetch has completed successfully.
///
/// `Definite` always produces `Enforced`; `NeedsFetch` produces `Enforced` only
/// when a restriction ACL matches an active condition, and otherwise produces
/// `NotEnforced`. An enforced result carries a denial group only when at least
/// one fetched restriction is unauthorized. Fetch errors must be handled by
/// the caller before invoking this helper.
fn completed_source_enforcement_outcome<T>(
    result: &[T],
    candidates: &[&EnforcementConditionSet],
    pre_filter_variant: &PreFilterVariant,
) -> AccessEnforcementOutcome
where
    T: SourceRestrictionCheck,
{
    if !enforcement_applies(
        candidates,
        pre_filter_variant,
        result
            .iter()
            .map(SourceRestrictionCheck::repo_region_identity),
    ) {
        return AccessEnforcementOutcome::NotEnforced;
    }

    let denial = result
        .iter()
        .filter(|check| !check.authorization().has_authorization())
        // Multiple denied checks can be returned by one source. Pick a stable
        // denying restriction so error messages do not depend on source
        // result ordering.
        .min_by(|left, right| compare_denied_checks(*left, *right))
        .map(|check| DenyingRestriction {
            restriction_root: check.restriction_root().cloned(),
            repo_region_acl: check.repo_region_identity().clone(),
            permission_request_group: check.permission_request_group().clone(),
        });

    AccessEnforcementOutcome::Enforced { denial }
}

/// Whether enforcement applies to restrictions with these ACLs: always after a
/// definite pre-filter match, otherwise only when an active condition set
/// matches one of them.
fn enforcement_applies<'a>(
    candidates: &[&EnforcementConditionSet],
    pre_filter_variant: &PreFilterVariant,
    restriction_acls: impl IntoIterator<Item = &'a MononokeIdentity>,
) -> bool {
    match pre_filter_variant {
        PreFilterVariant::Definite => true,
        PreFilterVariant::NeedsFetch => condition_sets_match_restriction_acls(
            candidates,
            &restriction_acls.into_iter().collect::<Vec<_>>(),
        ),
    }
}

/// Order checks by restriction root: known roots by path, then rootless
/// checks.
fn compare_restriction_roots<T: SourceRestrictionCheck>(left: &T, right: &T) -> Ordering {
    match (left.restriction_root(), right.restriction_root()) {
        (Some(left_root), Some(right_root)) => left_root.cmp(right_root),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}
