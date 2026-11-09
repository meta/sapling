/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Restricted Paths.
//!
//! Abstractions to track a repo's restricted paths, along with their ACLs,
//! and to store the manifest ids of these paths from every revision.

mod access_log;
pub(crate) mod restriction_check;
pub(crate) mod restriction_info;

#[cfg(test)]
mod test_utils;

use std::collections::BTreeSet;
use std::mem;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use context::CoreContext;
use ephemeral_blobstore::Bubble;
use metaconfig_types::AclManifestMode;
use metaconfig_types::RestrictedPathsConfig;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use mononoke_types::MPath;
use mononoke_types::NonRootMPath;
use permission_checker::AclProvider;
use permission_checker::MononokeIdentity;
use repo_derived_data::ArcRepoDerivedData;
pub use restricted_paths_common::*;
use scuba_ext::MononokeScubaSampleBuilder;
use thiserror::Error;

pub use crate::access_log::ACCESS_LOG_SCUBA_TABLE;
use crate::access_log::log_access_to_restricted_path;
use crate::restriction_check::AccessEnforcementOutcome;
use crate::restriction_check::EnforcementDecision;
pub use crate::restriction_check::ManifestRestrictionCheckResult;
pub use crate::restriction_check::PathRestrictionCheckResult;
pub use crate::restriction_check::PermissionRequestGroup;
use crate::restriction_check::PreFilterResult;
use crate::restriction_check::PreFilteredRequest;
pub use crate::restriction_check::RestrictionCheckResult;
use crate::restriction_check::SharedFetchHandle;
use crate::restriction_check::SourceRestrictionCheck;
pub use crate::restriction_check::check_path_restriction_infos;
pub use crate::restriction_check::has_maintainer_access_to_repo_region;
use crate::restriction_check::is_in_optional_group;
pub use crate::restriction_info::ManifestRestrictionInfo;
pub use crate::restriction_info::PathRestrictionInfo;

#[derive(Clone, Debug)]
pub enum RestrictedPathAccess {
    Manifest(RestrictedManifestId),
    Path(MPath),
}

/// The restriction that denied an access: the restriction root
/// when the denying source knows it, the repo-region ACL, and the group the
/// caller should request. Selected deterministically from the denied checks,
/// so a denial always names one stable restriction.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DenyingRestriction {
    /// Root path of the denying restriction (directory containing `.slacl`).
    /// `None` when the source cannot report one (manifest denials).
    pub restriction_root: Option<NonRootMPath>,
    /// Repo-region ACL of the denying restriction.
    pub repo_region_acl: MononokeIdentity,
    /// Group the caller should request for access.
    pub permission_request_group: PermissionRequestGroup,
}

#[derive(Clone, Debug, Error)]
pub struct RestrictedPathsAuthorizationError {
    access: RestrictedPathAccess,
    // Boxed because the denying restriction holds `MononokeIdentity`s, i.e. whole
    // `AuthenticatedIdentity` thrift structs, which would otherwise make every
    // `Result<_, MononokeError>` in the codebase much wider.
    details: Box<RestrictedPathsDenialDetails>,
}

#[derive(Clone, Debug)]
struct RestrictedPathsDenialDetails {
    denial: DenyingRestriction,
    denial_message: Option<String>,
}

impl RestrictedPathsAuthorizationError {
    pub fn new(
        access: RestrictedPathAccess,
        denial: DenyingRestriction,
        denial_message: Option<String>,
    ) -> Self {
        Self {
            access,
            details: Box::new(RestrictedPathsDenialDetails {
                denial,
                denial_message,
            }),
        }
    }

    pub fn permission_request_group(&self) -> &PermissionRequestGroup {
        &self.details.denial.permission_request_group
    }

    /// The denying restriction.
    pub fn denying_restriction(&self) -> &DenyingRestriction {
        &self.details.denial
    }

    /// Root of the denying restriction, when the source reported one.
    pub fn restriction_root(&self) -> Option<&NonRootMPath> {
        self.details.denial.restriction_root.as_ref()
    }

    /// Repo-region ACL of the denying restriction.
    pub fn repo_region_acl(&self) -> &MononokeIdentity {
        &self.details.denial.repo_region_acl
    }

    /// Repo-configured text appended to the error message, if any.
    pub fn denial_message(&self) -> Option<&str> {
        self.details.denial_message.as_deref()
    }

    pub fn access(&self) -> &RestrictedPathAccess {
        &self.access
    }

    pub fn is_manifest_access(&self) -> bool {
        matches!(&self.access, RestrictedPathAccess::Manifest(_))
    }
}

/// Why creation-time validation rejected a change to an ACL (`.slacl`) file.
#[derive(Clone, Debug, Error)]
pub enum AclFileChangeRejection {
    /// The caller lacks maintainer access to the restriction covering `dir`.
    #[error(
        "Cannot modify ACL file at {dir}: directory is restricted by {repo_region_acl} \
         (restriction root: {restriction_root}). Caller does not have maintainer access."
    )]
    Unauthorized {
        dir: MPath,
        repo_region_acl: String,
        restriction_root: NonRootMPath,
    },
    /// An earlier changeset in the same stack already changed the ACL file in `dir`.
    #[error(
        "Cannot modify ACL file at {dir}: .slacl at this path was already modified by an \
         earlier changeset in this stack. Split into separate submissions."
    )]
    ModifiedEarlierInStack { dir: MPath },
}

/// Error type for restricted paths enforcement.
#[derive(Debug, Error)]
pub enum RestrictedPathsError {
    #[error(transparent)]
    AuthorizationError(RestrictedPathsAuthorizationError),
    #[error(transparent)]
    AclFileChangeRejected(#[from] AclFileChangeRejection),
    #[error("Internal error: {0}")]
    InternalError(#[from] anyhow::Error),
}

impl std::fmt::Display for RestrictedPathsAuthorizationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Access denied: unauthorized access to restricted path: {}",
            self.access
        )?;
        if let Some(denial_message) = self.denial_message() {
            write!(f, "\n{denial_message}")?;
        }
        Ok(())
    }
}

impl std::fmt::Display for RestrictedPathAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manifest(manifest_id) => {
                write!(f, "RestrictedManifestId({manifest_id})")
            }
            Self::Path(path) => write!(f, "{path}"),
        }
    }
}

/// Repository restricted paths configuration.
#[facet::facet]
pub struct RestrictedPaths {
    /// Config-based restricted paths (shared with derived-data crates).
    config_based: Arc<RestrictedPathsConfigBased>,
    /// ACL provider for authorization checks
    acl_provider: Arc<dyn AclProvider>,
    /// Scuba sample builder for logging access to restricted paths
    scuba: MononokeScubaSampleBuilder,
    /// Repo derived data for deriving ACL manifests.
    repo_derived_data: ArcRepoDerivedData,
    /// Repo name, the switch value of this repo's JustKnobs.
    repo_name: String,
}

impl RestrictedPaths {
    pub fn new(
        repo_name: String,
        config_based: Arc<RestrictedPathsConfigBased>,
        acl_provider: Arc<dyn AclProvider>,
        scuba: MononokeScubaSampleBuilder,
        repo_derived_data: ArcRepoDerivedData,
    ) -> Result<Self> {
        if !config_based.config().acl_manifest_mode.is_disabled() {
            anyhow::ensure!(
                repo_derived_data
                    .config()
                    .is_enabled(DerivableType::AclManifests),
                "acl_manifest_mode is enabled but AclManifest derivation is not enabled for this repo. \
                 Enable AclManifests in the repo's derived data config."
            );
        }
        Ok(Self {
            config_based,
            acl_provider,
            scuba,
            repo_derived_data,
            repo_name,
        })
    }

    /// Return a copy of `self` whose `repo_derived_data` is wrapped for the
    /// given bubble, so AclManifest derivation runs against the bubble's
    /// derived-data manager rather than the persistent one.
    pub fn for_bubble(&self, bubble: Bubble) -> Self {
        Self {
            config_based: self.config_based.clone(),
            acl_provider: self.acl_provider.clone(),
            scuba: self.scuba.clone(),
            repo_derived_data: Arc::new(self.repo_derived_data.for_bubble(bubble)),
            repo_name: self.repo_name.clone(),
        }
    }

    // TODO(T248660053): make pub(crate) once hooks use dedicated primitives
    // instead of accessing path_acls directly. Blocked on adding a primitive
    // for "is path related to any restriction root" (used by block_restricted_copy
    // and block_restricted_subtree_copy hooks).
    pub fn config(&self) -> &RestrictedPathsConfig {
        self.config_based.config()
    }

    /// Returns whether this repository may have restricted paths.
    /// When AclManifest mode is enabled, restrictions can be discovered
    /// dynamically from `.slacl` files in the repo, so callers cannot treat a
    /// false config-only lookup as proof that no restricted paths exist.
    pub fn may_have_restricted_paths(&self) -> bool {
        // TODO(T248658346): account for manifest-id store entries that were
        // recorded for paths that have since been removed from the current
        // config. Historical manifests for those paths should still be treated
        // as restricted.
        !self.config().acl_manifest_mode.is_disabled() || self.config_based.has_restricted_paths()
    }

    /// Returns the soft path ACLs configuration.
    pub fn soft_path_acls(&self) -> &[metaconfig_types::SoftRestrictedPathConfig] {
        &self.config_based.config().soft_path_acls
    }

    /// Returns the underlying config-based restricted paths.
    pub fn config_based(&self) -> &Arc<RestrictedPathsConfigBased> {
        &self.config_based
    }

    pub fn acl_provider(&self) -> &Arc<dyn AclProvider> {
        &self.acl_provider
    }

    /// Check whether the caller has maintainer access to every repo region ACL
    /// in `acls`, or is a member of the configured admin bypass group.
    ///
    /// Convenience over [`has_maintainer_access_to_repo_region`] that sources
    /// the ACL provider and admin bypass group from this `RestrictedPaths`, so
    /// callers cannot pass a mismatched provider or forget the bypass group.
    pub async fn has_maintainer_access(
        &self,
        ctx: &CoreContext,
        acls: &[&MononokeIdentity],
    ) -> Result<bool> {
        has_maintainer_access_to_repo_region(
            ctx,
            self.acl_provider(),
            acls,
            self.config().admin_bypass_group.as_ref(),
        )
        .await
    }

    // -----------------------------------------------------------------------
    // Public restriction lookup methods
    // -----------------------------------------------------------------------

    /// Get restriction info for paths that are themselves restriction roots.
    /// Does NOT consider parent directories.
    pub async fn get_path_restriction_root_info(
        &self,
        ctx: &CoreContext,
        cs_id: Option<ChangesetId>,
        paths: &[NonRootMPath],
    ) -> Result<Vec<PathRestrictionInfo>> {
        restriction_info::get_path_restriction_root_info(self, ctx, cs_id, paths).await
    }

    /// Get restriction info for one or more paths, considering ancestor restrictions.
    /// For each path, collects restrictions at every ancestor directory.
    pub async fn get_path_restriction_info(
        &self,
        ctx: &CoreContext,
        cs_id: Option<ChangesetId>,
        paths: &[NonRootMPath],
    ) -> Result<Vec<PathRestrictionInfo>> {
        restriction_info::get_path_restriction_info(self, ctx, cs_id, paths).await
    }

    /// Get restriction and authorization checks for paths that are themselves restriction roots.
    /// Does NOT consider parent directories.
    pub async fn get_path_restriction_root_check(
        &self,
        ctx: &CoreContext,
        cs_id: Option<ChangesetId>,
        paths: &[NonRootMPath],
    ) -> Result<Vec<PathRestrictionCheckResult>> {
        restriction_check::get_path_restriction_root_check(self, ctx, cs_id, paths).await
    }

    /// Get restriction and authorization checks for one or more paths,
    /// considering ancestor restrictions.
    pub async fn get_path_restriction_check(
        &self,
        ctx: &CoreContext,
        cs_id: Option<ChangesetId>,
        paths: &[NonRootMPath],
    ) -> Result<Vec<PathRestrictionCheckResult>> {
        restriction_check::get_path_restriction_check(self, ctx, cs_id, paths).await
    }

    /// Get manifest restriction and authorization checks from the config-backed source.
    pub async fn get_manifest_restriction_check(
        &self,
        ctx: &CoreContext,
        manifest_id: &RestrictedManifestId,
        manifest_type: &ManifestType,
    ) -> Result<Vec<ManifestRestrictionCheckResult>> {
        restriction_check::get_manifest_restriction_check_for_current_behavior(
            self,
            ctx,
            manifest_id,
            manifest_type,
        )
        .await
    }

    /// Get manifest restriction info without performing authorization checks.
    pub async fn get_manifest_restriction_info(
        &self,
        ctx: &CoreContext,
        manifest_id: &RestrictedManifestId,
        manifest_type: &ManifestType,
    ) -> Result<Vec<ManifestRestrictionInfo>> {
        restriction_info::get_manifest_restriction_info(self, ctx, manifest_id, manifest_type).await
    }

    /// Check if a path is itself a restriction root (exact match).
    /// Returns false for paths that are merely under a restriction root.
    pub async fn is_restriction_root(
        &self,
        ctx: &CoreContext,
        cs_id: Option<ChangesetId>,
        path: &NonRootMPath,
    ) -> Result<bool> {
        restriction_info::is_restriction_root(self, ctx, cs_id, path).await
    }

    /// Check if a path is restricted, considering ancestor directories.
    /// Returns true if the path itself or any of its ancestors is a restriction root.
    pub async fn is_restricted_path(
        &self,
        ctx: &CoreContext,
        cs_id: Option<ChangesetId>,
        path: &NonRootMPath,
    ) -> Result<bool> {
        restriction_info::is_restricted_path(self, ctx, cs_id, path).await
    }

    /// Returns whether this HgAugmented manifest is a restriction root
    /// according to the sources enabled by `AclManifestMode`.
    ///
    /// `preloaded_is_restricted` is the AclManifest-backed restriction bit that
    /// the caller already loaded with the HgAugmented manifest. This primitive
    /// combines that value with the config-backed manifest-id store when the
    /// selected mode requires it, without reloading AclManifest data.
    pub async fn is_restricted_manifest(
        &self,
        ctx: &CoreContext,
        manifest_id: &RestrictedManifestId,
        manifest_type: &ManifestType,
        preloaded_is_restricted: bool,
    ) -> Result<bool> {
        restriction_info::is_restricted_manifest(
            self,
            ctx,
            manifest_id,
            manifest_type,
            preloaded_is_restricted,
        )
        .await
    }

    /// Find all restriction roots that are descendants of any of the given root paths.
    /// Results are deduplicated by restriction_root.
    pub async fn find_restricted_descendants(
        &self,
        ctx: &CoreContext,
        cs_id: Option<ChangesetId>,
        roots: Vec<MPath>,
    ) -> Result<Vec<PathRestrictionInfo>> {
        restriction_info::find_restricted_descendants(self, ctx, cs_id, roots).await
    }

    // -----------------------------------------------------------------------
    // Public ACL file change validation
    // -----------------------------------------------------------------------

    /// Validate that the caller may modify any `.slacl` files touched by
    /// the given changesets in this stack.
    ///
    /// # COVERED
    /// - Explicit add / modify / delete of files whose basename equals
    ///   `acl_file_name` (typically `.slacl`).
    /// - Stack-internal add-then-modify of the same `.slacl` path, unless the
    ///   caller is an admin bypass member.
    ///
    /// # NOT COVERED (deliberately, for performance)
    /// - **Implicit `.slacl` deletes** (e.g., a file added at `secret`
    ///   that replaces directory `secret/` and its `.slacl`). Detecting
    ///   this would require `find_restricted_descendants`, which triggers
    ///   `AclManifest` derivation — too expensive for the changeset
    ///   creation hot path. Land-time hooks catch this before it becomes
    ///   public, and the read-time path still blocks unauthorized access.
    /// - **Non-`.slacl` file modifications inside restricted directories.**
    ///   The threat model is read access; deleting/modifying protected
    ///   code does not grant access to it.
    /// - **Root-level `.slacl`** (a `.slacl` file at the repository root).
    ///   Degenerate case: the validator's `NonRootMPath::try_from(MPath::ROOT)`
    ///   path silently returns no restriction info. Acceptable because
    ///   no real repo currently uses a root-level `.slacl`; documented
    ///   here for reviewer awareness.
    ///
    /// Skips entirely if the repo has no restricted paths configured, or if
    /// the JustKnob `scm/mononoke:enforce_acl_file_protection_on_create` is
    /// off.
    pub async fn validate_acl_file_changes_in_changesets(
        &self,
        ctx: &CoreContext,
        repo_name: &str,
        parent_cs_ids: &[ChangesetId],
        touched_paths_per_changeset: &[&[NonRootMPath]],
    ) -> std::result::Result<(), RestrictedPathsError> {
        // 1. Early return if no restricted paths. Checked before the JustKnob
        //    so repos without restrictions never evaluate it.
        if !self.may_have_restricted_paths() {
            return Ok(());
        }

        // 2. JustKnob gate — owned by this method, NOT duplicated at call sites.
        if !justknobs::eval(
            "scm/mononoke:enforce_acl_file_protection_on_create",
            ctx.metadata()
                .client_request_info()
                .map(|info| info.correlator.as_str()),
            Some(repo_name),
        ) {
            return Ok(());
        }

        let acl_file_name = self.config().acl_file_name();

        // 3. Extract `.slacl` parent dirs by basename match (pure, sync).
        //    Per-changeset list. Root-path .slacl uses MPath::ROOT.
        let acl_file_dirs_stack: Vec<Vec<MPath>> = touched_paths_per_changeset
            .iter()
            .map(|touched| {
                touched
                    .iter()
                    .filter(|path| path.basename().as_ref() == acl_file_name.as_bytes())
                    .map(|path| {
                        let (parent_opt, _) = path.split_dirname();
                        parent_opt.map_or(MPath::ROOT, MPath::from)
                    })
                    .collect()
            })
            .collect();

        // 4. Track dirs touched by earlier stack entries — block
        //    add-then-modify of the same .slacl in a single stack. Admin
        //    bypass members are exempt, so source control engineers are
        //    never blocked (e.g. while fixing a SEV); maintainers are not.
        let mut dirs_touched_in_stack: BTreeSet<MPath> = BTreeSet::new();

        for touched_dirs in &acl_file_dirs_stack {
            for dir_path in touched_dirs {
                if dirs_touched_in_stack.contains(dir_path)
                    && !is_in_optional_group(
                        ctx,
                        self.acl_provider(),
                        self.config().admin_bypass_group.as_ref(),
                    )
                    .await?
                {
                    return Err(AclFileChangeRejection::ModifiedEarlierInStack {
                        dir: dir_path.clone(),
                    }
                    .into());
                }

                // Targeted restriction lookup using the existing primitive.
                // Root-path .slacl falls through to the empty-slice branch
                // (NonRootMPath::try_from(MPath::ROOT) errs), which returns
                // no restrictions — see method docstring NOT COVERED list.
                let restriction_infos: Vec<PathRestrictionInfo> =
                    match NonRootMPath::try_from(dir_path.clone()) {
                        Ok(non_root) => {
                            // Merge changesets: the caller needs access in EVERY
                            // parent, so gather restrictions from all of them.
                            let parents: Vec<Option<ChangesetId>> = if parent_cs_ids.is_empty() {
                                vec![None]
                            } else {
                                parent_cs_ids.iter().copied().map(Some).collect()
                            };
                            futures::future::try_join_all(parents.into_iter().map(|parent| {
                                self.get_path_restriction_info(
                                    ctx,
                                    parent,
                                    std::slice::from_ref(&non_root),
                                )
                            }))
                            .await?
                            .into_iter()
                            .flatten()
                            .collect()
                        }
                        Err(_) => {
                            // Root path — no restriction lookup possible via this
                            // primitive. Skip (degenerate case).
                            Vec::new()
                        }
                    };

                for info in &restriction_infos {
                    let acl = MononokeIdentity::from_str(&info.repo_region_acl)
                        .context("Failed to parse repo_region_acl")?;
                    let has_access = self.has_maintainer_access(ctx, &[&acl]).await?;
                    if !has_access {
                        return Err(AclFileChangeRejection::Unauthorized {
                            dir: dir_path.clone(),
                            repo_region_acl: info.repo_region_acl.clone(),
                            restriction_root: info.restriction_root.clone(),
                        }
                        .into());
                    }
                }
            }

            dirs_touched_in_stack.extend(touched_dirs.iter().cloned());
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Public access logging methods
    // -----------------------------------------------------------------------

    /// Check if a manifest id belongs to a restricted path and log access to it.
    ///
    /// Returns a `RestrictionCheckResult` with authorization status and
    /// restriction root info.
    pub async fn log_access_by_manifest_if_restricted(
        &self,
        ctx: &CoreContext,
        manifest_id: RestrictedManifestId,
        manifest_type: ManifestType,
        cs_id: Option<ChangesetId>,
    ) -> Result<RestrictionCheckResult> {
        let acl_manifest_mode = self.config().acl_manifest_mode;
        if matches!(
            acl_manifest_mode,
            AclManifestMode::Shadow | AclManifestMode::Both
        ) {
            return access_log::log_source_comparison_access_by_manifest_if_restricted(
                self,
                ctx,
                manifest_id,
                manifest_type,
                acl_manifest_mode,
            )
            .await;
        }

        // No need to query the DB if the config is empty, i.e. the repo doesn't
        // have any restricted paths.
        let _ = cs_id; // Config-backed manifest logging does not use changeset ids.

        if self.config().is_empty() {
            return Ok(RestrictionCheckResult {
                has_authorization: true,
                restriction_roots: vec![],
            });
        }

        let paths = restriction_info::get_manifest_restricted_paths_from_config(
            self,
            ctx,
            &manifest_id,
            &manifest_type,
        )
        .await?;

        if paths.is_empty() {
            return Ok(RestrictionCheckResult {
                has_authorization: true,
                restriction_roots: vec![],
            });
        }

        // Use config-based lookup directly — this method works with manifest IDs
        // from the restricted paths store, not with changesets, so we always use
        // the config to determine which paths are restricted.
        // TODO(T248660053): support manifest-based access using AclManifests.
        let restrictions = restriction_info::get_config_restrictions_for_paths(self, &paths);

        log_access_to_restricted_path(
            ctx,
            self.config_based.manifest_id_store().repo_id(),
            paths,
            restrictions,
            crate::access_log::RestrictedPathAccessData::Manifest(manifest_id, manifest_type),
            self.config().acl_manifest_mode,
            self.acl_provider.clone(),
            self.config().tooling_allowlist_group.as_deref(),
            self.config().admin_bypass_group.as_ref(),
            self.scuba.clone(),
            vec!["manifest_db".to_string()],
        )
        .await
    }

    /// Log access to a restricted path, when it's accessed by the full path,
    /// instead of a manifest id.
    ///
    /// Returns a `RestrictionCheckResult` with authorization status and
    /// restriction root info.
    pub async fn log_access_by_path_if_restricted(
        &self,
        ctx: &CoreContext,
        path: NonRootMPath,
        cs_id: Option<ChangesetId>,
    ) -> Result<RestrictionCheckResult> {
        let acl_manifest_mode = self.config().acl_manifest_mode;
        if matches!(
            acl_manifest_mode,
            AclManifestMode::Shadow | AclManifestMode::Both
        ) {
            return access_log::log_source_comparison_access_by_path_if_restricted(
                self,
                ctx,
                path,
                cs_id,
                acl_manifest_mode,
            )
            .await;
        }

        // Return early if the repo doesn't have any restricted paths.
        let _ = cs_id; // Config-backed path logging does not use changeset ids.
        if self.config().is_empty() {
            return Ok(RestrictionCheckResult {
                has_authorization: true,
                restriction_roots: vec![],
            });
        }

        // Find which restricted path roots match this path, pairing each with
        // its ACL and its own rollout allowlist group.
        let (restricted_path_roots, matched_restrictions): (Vec<_>, Vec<_>) = self
            .config()
            .path_restriction_metadata
            .iter()
            .filter(|(restricted_path_prefix, _)| restricted_path_prefix.is_prefix_of(&path))
            .map(|(prefix, metadata)| {
                (
                    prefix.clone(),
                    (
                        &metadata.repo_region_acl,
                        metadata.rollout_allowlist_group.as_ref(),
                    ),
                )
            })
            .unzip();

        // If no restricted paths match, no need to log
        if restricted_path_roots.is_empty() {
            return Ok(RestrictionCheckResult {
                has_authorization: true,
                restriction_roots: vec![],
            });
        }

        log_access_to_restricted_path(
            ctx,
            self.config_based.manifest_id_store().repo_id(),
            restricted_path_roots,
            matched_restrictions,
            crate::access_log::RestrictedPathAccessData::FullPath { full_path: path },
            self.config().acl_manifest_mode,
            self.acl_provider.clone(),
            self.config().tooling_allowlist_group.as_deref(),
            self.config().admin_bypass_group.as_ref(),
            self.scuba.clone(),
            vec!["manifest_db".to_string()],
        )
        .await
    }
}

/// Spawn enforcement check for restricted path access.
///
/// This function:
/// 1. Spawns any source fetches needed by logging or enforcement
/// 2. Spawns logging as a fire-and-forget task when logging is enabled
/// 3. Checks whether a condition set matches the access and no exemption set
///    matches the request
/// 4. If so AND the user lacks authorization, returns
///    `RestrictedPathsError::AuthorizationError`; an exempted access that fails
///    to read a restriction source returns that source error
///
/// # Returns
/// * `Ok(())` if access is allowed or enforcement is disabled
/// * `Err(RestrictedPathsError::AuthorizationError)` if access is denied
pub async fn spawn_enforce_restricted_path_access<'a, 'b>(
    ctx: &'b CoreContext,
    restricted_paths: Arc<RestrictedPaths>,
    path: &'a MPath,
    switch_value: &'b str,
    cs_id: Option<ChangesetId>,
) -> Result<(), RestrictedPathsError> {
    let non_root_mpath = match NonRootMPath::try_from(path.clone()) {
        Ok(path) => path,
        Err(_) => return Ok(()),
    };
    let config = restricted_paths.config();
    let effective_mode = effective_acl_manifest_mode(config.acl_manifest_mode, true);
    let config_path_may_restrict = config
        .path_restriction_metadata
        .keys()
        .any(|prefix| prefix.is_prefix_of(&non_root_mpath));
    let access_data = access_log::RestrictedPathAccessData::FullPath {
        full_path: non_root_mpath.clone(),
    };

    spawn_enforce_restricted_access(
        ctx,
        restricted_paths.clone(),
        switch_value,
        effective_mode,
        config_path_may_restrict,
        cs_id.is_some(),
        "path",
        access_data,
        move |fetches| SourceHandles {
            config: fetches.fetch_config().then(|| {
                if !config_path_may_restrict {
                    return SharedFetchHandle::from_result(Ok(
                        Vec::<PathRestrictionCheckResult>::new(),
                    ));
                }
                let ctx = ctx.clone();
                let restricted_paths = restricted_paths.clone();
                let path = non_root_mpath.clone();
                SharedFetchHandle::from_future(async move {
                    restriction_check::check_path_restriction_from_source(
                        &ctx,
                        &restricted_paths,
                        path,
                        restriction_check::PathRestrictionSource::Config,
                    )
                    .await
                })
            }),
            acl_manifest: fetches
                .fetch_acl_manifest()
                .then(|| {
                    cs_id.map(|cs_id| {
                        restriction_check::spawn_path_restriction_check(
                            ctx,
                            restricted_paths.clone(),
                            non_root_mpath.clone(),
                            restriction_check::PathRestrictionSource::AclManifest(cs_id),
                        )
                    })
                })
                .flatten(),
        },
        move |denial, denial_message| {
            RestrictedPathsError::AuthorizationError(RestrictedPathsAuthorizationError::new(
                RestrictedPathAccess::Path((*path).clone()),
                denial,
                denial_message,
            ))
        },
    )
    .await
}

/// Spawn enforcement check for restricted manifest access.
///
/// This function:
/// 1. Spawns any source fetches needed by logging or enforcement
/// 2. Spawns logging as a fire-and-forget task when logging is enabled
/// 3. Checks whether a condition set matches the access and no exemption set
///    matches the request
/// 4. If so AND the user lacks authorization, returns
///    `RestrictedPathsError::AuthorizationError`; an exempted access that fails
///    to read a restriction source returns that source error
///
/// # Returns
/// * `Ok(())` if access is allowed or enforcement is disabled
/// * `Err(RestrictedPathsError::AuthorizationError)` if access is denied
pub async fn spawn_enforce_restricted_manifest_access<'a>(
    ctx: &'a CoreContext,
    restricted_paths: Arc<RestrictedPaths>,
    manifest_id: RestrictedManifestId,
    manifest_type: ManifestType,
    switch_value: &'a str,
    _cs_id: Option<ChangesetId>,
) -> Result<(), RestrictedPathsError> {
    let config = restricted_paths.config();
    let acl_manifest_available = manifest_type == ManifestType::HgAugmented;
    let effective_mode =
        effective_acl_manifest_mode(config.acl_manifest_mode, acl_manifest_available);
    // TODO(T248658346): decouple config from manifest_id store. We should continue enforcing if the config doesn't have the path, but the manifest_id store has entries for the manifest.
    let config_manifest_may_restrict = !config.is_empty();
    let access_data =
        access_log::RestrictedPathAccessData::Manifest(manifest_id.clone(), manifest_type.clone());
    let manifest_id_for_fetches = manifest_id.clone();
    let manifest_type_for_fetches = manifest_type.clone();

    spawn_enforce_restricted_access(
        ctx,
        restricted_paths.clone(),
        switch_value,
        effective_mode,
        config_manifest_may_restrict,
        acl_manifest_available,
        "manifest",
        access_data,
        move |fetches| SourceHandles {
            config: fetches.fetch_config().then(|| {
                if !config_manifest_may_restrict {
                    return SharedFetchHandle::from_result(Ok(
                        Vec::<ManifestRestrictionCheckResult>::new(),
                    ));
                }
                let ctx = ctx.clone();
                let restricted_paths = restricted_paths.clone();
                let manifest_id = manifest_id_for_fetches.clone();
                let manifest_type = manifest_type_for_fetches.clone();
                SharedFetchHandle::from_future(async move {
                    restriction_check::check_manifest_restriction_from_source(
                        &ctx,
                        &restricted_paths,
                        manifest_id,
                        manifest_type,
                        restriction_check::RestrictionSource::Config,
                    )
                    .await
                })
            }),
            acl_manifest: fetches
                .fetch_acl_manifest()
                .then(|| {
                    acl_manifest_available.then(|| {
                        restriction_check::spawn_manifest_restriction_check(
                            ctx,
                            restricted_paths.clone(),
                            manifest_id_for_fetches.clone(),
                            manifest_type_for_fetches.clone(),
                            restriction_check::RestrictionSource::AclManifest,
                        )
                    })
                })
                .flatten(),
        },
        move |denial, denial_message| {
            RestrictedPathsError::AuthorizationError(RestrictedPathsAuthorizationError::new(
                RestrictedPathAccess::Manifest(manifest_id),
                denial,
                denial_message,
            ))
        },
    )
    .await
}

const ACCESS_LOGGING_JK: &str = "scm/mononoke:enabled_restricted_paths_access_logging";

struct SourceHandles<T: SourceRestrictionCheck> {
    config: Option<SharedFetchHandle<T>>,
    acl_manifest: Option<SharedFetchHandle<T>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceFetchOptions {
    logging_enabled: bool,
    enforcement_enabled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceFetches {
    logging_config: bool,
    logging_acl_manifest: bool,
    enforcement_config: bool,
    enforcement_acl_manifest: bool,
}

impl SourceFetches {
    fn fetch_config(self) -> bool {
        self.logging_config || self.enforcement_config
    }

    fn fetch_acl_manifest(self) -> bool {
        self.logging_acl_manifest || self.enforcement_acl_manifest
    }
}

/// Shared path/manifest enforcement orchestration.
///
/// The public entrypoints decide how to build source handles for their access
/// type. This helper owns the common flow: evaluate cheap request-local
/// enforcement filters first, choose the sources needed by logging and
/// enforcement, spawn fire-and-forget logging from cloned shared handles, and
/// finally await only the authoritative handles needed to deny the request.
async fn spawn_enforce_restricted_access<T>(
    ctx: &CoreContext,
    restricted_paths: Arc<RestrictedPaths>,
    switch_value: &str,
    effective_mode: AclManifestMode,
    config_source_may_restrict: bool,
    acl_manifest_available: bool,
    access_type: &'static str,
    access_data: access_log::RestrictedPathAccessData,
    build_handles: impl FnOnce(SourceFetches) -> SourceHandles<T>,
    authorization_error: impl FnOnce(DenyingRestriction, Option<String>) -> RestrictedPathsError,
) -> Result<(), RestrictedPathsError>
where
    T: SourceRestrictionCheck + Send + Sync + 'static,
{
    let config = restricted_paths.config();
    let enforcement_enabled =
        config.enforcement_enabled && !config.enforcement_condition_sets.is_empty();
    let source_options = fetch_options_for_access(switch_value, enforcement_enabled)?;
    if !source_options.logging_enabled && !source_options.enforcement_enabled {
        return Ok(());
    }

    let pre_filter = if enforcement_enabled {
        restriction_check::pre_filter_request(ctx, config)
    } else {
        PreFilteredRequest {
            conditions: PreFilterResult::NoMatch,
            exemption_matched: false,
        }
    };
    let exemption_matched = pre_filter.exemption_matched;
    let fetches = source_fetches_for_access(
        source_options,
        &pre_filter.conditions,
        effective_mode,
        config_source_may_restrict,
        acl_manifest_available,
    );
    let handles = build_handles(fetches);

    if !source_options.enforcement_enabled {
        access_log::spawn_log_source_results_with_enforcement(
            ctx,
            restricted_paths.clone(),
            access_data,
            effective_mode,
            EnforcementDecision::Disabled,
            fetches
                .logging_config
                .then(|| handles.config.clone())
                .flatten(),
            fetches
                .logging_acl_manifest
                .then(|| handles.acl_manifest.clone())
                .flatten(),
        );
        return Ok(());
    }

    let enforcement_outcome = enforce_with_source_handles(
        fetches.enforcement_config,
        fetches.enforcement_acl_manifest,
        &handles,
        pre_filter,
        missing_authoritative_source_error(
            access_type,
            effective_mode,
            fetches.enforcement_config,
            fetches.enforcement_acl_manifest,
        ),
    )
    .await;

    access_log::spawn_log_source_results_with_enforcement(
        ctx,
        restricted_paths.clone(),
        access_data,
        effective_mode,
        EnforcementDecision::from_outcome(&enforcement_outcome, exemption_matched),
        fetches
            .logging_config
            .then(|| handles.config.clone())
            .flatten(),
        fetches
            .logging_acl_manifest
            .then(|| handles.acl_manifest.clone())
            .flatten(),
    );

    match enforcement_outcome? {
        AccessEnforcementOutcome::Enforced {
            denial: Some(denial),
        } => Err(authorization_error(denial, config.denial_message.clone())),
        AccessEnforcementOutcome::NotEnforced
        | AccessEnforcementOutcome::Enforced { denial: None }
        | AccessEnforcementOutcome::Exempted => Ok(()),
    }
}

/// Read per-request feature switches that affect source fetching.
///
/// `enforcement_enabled` is passed in after config-level enforcement checks so
/// source selection can treat disabled enforcement like a request that did not
/// match any enforcement condition set.
fn fetch_options_for_access(
    switch_value: &str,
    enforcement_enabled: bool,
) -> Result<SourceFetchOptions> {
    Ok(SourceFetchOptions {
        logging_enabled: justknobs::eval(ACCESS_LOGGING_JK, None, Some(switch_value)),
        enforcement_enabled,
    })
}

/// Decide which restriction sources must be fetched for this access.
///
/// Logging and enforcement choose sources independently: comparison logging can
/// need both config and AclManifest even when enforcement only needs the
/// authoritative source. Centralizing the source table keeps path and manifest
/// callsites from duplicating those mode rules.
fn source_fetches_for_access(
    source_options: SourceFetchOptions,
    pre_filter_result: &PreFilterResult<'_>,
    effective_mode: AclManifestMode,
    config_source_may_restrict: bool,
    acl_manifest_available: bool,
) -> SourceFetches {
    let enforcement_fetch_enabled = !matches!(pre_filter_result, PreFilterResult::NoMatch)
        && source_options.enforcement_enabled;
    let enforcement_config =
        enforcement_fetch_enabled && fetch_config_for_enforcement(effective_mode);
    let enforcement_acl_manifest =
        enforcement_fetch_enabled && fetch_acl_manifest_for_enforcement(effective_mode);
    let logging_acl_manifest = source_options.logging_enabled
        && fetch_acl_manifest_for_logging(effective_mode)
        && acl_manifest_available;
    let logging_config = source_options.logging_enabled
        && effective_mode != AclManifestMode::Authoritative
        && (config_source_may_restrict
            || (compares_sources_for_logging(effective_mode) && logging_acl_manifest));

    SourceFetches {
        logging_config,
        logging_acl_manifest,
        enforcement_config,
        enforcement_acl_manifest,
    }
}

fn missing_authoritative_source_error(
    access_type: &str,
    acl_manifest_mode: metaconfig_types::AclManifestMode,
    needs_config: bool,
    needs_acl_manifest: bool,
) -> anyhow::Error {
    anyhow::anyhow!(
        "authoritative {access_type} source for acl_manifest_mode={acl_manifest_mode:?} unavailable (needs_config={needs_config}, needs_acl_manifest={needs_acl_manifest})",
    )
}

struct SelectedSourceHandles<'a, T: SourceRestrictionCheck> {
    handles: Vec<&'a SharedFetchHandle<T>>,
    missing_source: bool,
}

impl<T: SourceRestrictionCheck> SourceHandles<T> {
    fn selected_for(
        &self,
        fetch_config: bool,
        fetch_acl_manifest: bool,
    ) -> SelectedSourceHandles<'_, T> {
        let mut handles = Vec::new();
        let mut missing_source = false;

        if fetch_config {
            if let Some(handle) = self.config.as_ref() {
                handles.push(handle);
            } else {
                missing_source = true;
            }
        }
        if fetch_acl_manifest {
            if let Some(handle) = self.acl_manifest.as_ref() {
                handles.push(handle);
            } else {
                missing_source = true;
            }
        }

        SelectedSourceHandles {
            handles,
            missing_source,
        }
    }
}

/// Normalize the repo-configured rollout mode to the mode this access can use.
///
/// `AclManifestMode` is configured at the repo level, but AclManifest is not
/// always an available source for an individual access: path checks need a
/// changeset id, and manifest checks need a manifest type backed by
/// AclManifest data. This helper is the single boundary where callsites combine
/// configured mode with source availability before source selection.
///
/// In this diff only Shadow comparison can use AclManifest, so non-Shadow modes
/// collapse to Disabled. Follow-up diffs extend the match arms for Both and
/// Authoritative without forcing each path/manifest callsite to duplicate the
/// availability fallback logic.
fn effective_acl_manifest_mode(
    acl_manifest_mode: AclManifestMode,
    acl_manifest_supported: bool,
) -> AclManifestMode {
    match acl_manifest_mode {
        AclManifestMode::Shadow => AclManifestMode::Shadow,
        // TODO(T248658346): ensure access through all other manifest types is
        // deprecated
        AclManifestMode::Both if acl_manifest_supported => AclManifestMode::Both,
        AclManifestMode::Authoritative if acl_manifest_supported => AclManifestMode::Authoritative,
        _ => AclManifestMode::Disabled,
    }
}

/// Whether config should be fetched as an authoritative enforcement source.
///
/// These mode predicates intentionally keep source selection as a small table:
/// follow-up diffs can change one predicate when enabling a rollout mode
/// without rewriting the shared fetch orchestration.
fn fetch_config_for_enforcement(effective_mode: AclManifestMode) -> bool {
    effective_mode != AclManifestMode::Authoritative
}

/// Whether AclManifest should be fetched as an authoritative enforcement source.
fn fetch_acl_manifest_for_enforcement(effective_mode: AclManifestMode) -> bool {
    matches!(
        effective_mode,
        AclManifestMode::Both | AclManifestMode::Authoritative
    )
}

/// Whether AclManifest should be fetched for access-log telemetry.
fn fetch_acl_manifest_for_logging(effective_mode: AclManifestMode) -> bool {
    matches!(
        effective_mode,
        AclManifestMode::Shadow | AclManifestMode::Both | AclManifestMode::Authoritative
    )
}

/// Whether logging needs both sources to produce source-comparison fields.
fn compares_sources_for_logging(effective_mode: AclManifestMode) -> bool {
    matches!(
        effective_mode,
        AclManifestMode::Shadow | AclManifestMode::Both
    )
}

async fn enforce_with_source_handles<'a, T>(
    fetch_config: bool,
    fetch_acl_manifest: bool,
    handles: &SourceHandles<T>,
    pre_filter: PreFilteredRequest<'a>,
    missing_source_error: anyhow::Error,
) -> Result<AccessEnforcementOutcome>
where
    T: SourceRestrictionCheck + Send + Sync + 'static,
{
    let exemption_matched = pre_filter.exemption_matched;
    let (candidates, pre_filter_variant) = match pre_filter.conditions {
        PreFilterResult::NoMatch => {
            return Ok(AccessEnforcementOutcome::NotEnforced);
        }
        PreFilterResult::DefiniteMatch { candidates } => {
            (candidates, restriction_check::PreFilterVariant::Definite)
        }
        PreFilterResult::NeedsFetch { candidates } => {
            (candidates, restriction_check::PreFilterVariant::NeedsFetch)
        }
    };

    // TODO(T289977216): Reconcile successful sources' tri-state factors before
    // reducing each source to an independent boolean enforcement outcome.
    let selected_sources = handles.selected_for(fetch_config, fetch_acl_manifest);
    let mut source_outcomes =
        futures::future::join_all(selected_sources.handles.into_iter().map(|handle| {
            restriction_check::source_enforcement_outcome(handle, &candidates, &pre_filter_variant)
        }))
        .await;
    // `selected_for` lists config before AclManifest: when both fail, report
    // the AclManifest error.
    if let [config @ Err(_), acl_manifest @ Err(_)] = source_outcomes.as_mut_slice() {
        mem::swap(config, acl_manifest);
    }
    if selected_sources.missing_source {
        source_outcomes.push(Err(missing_source_error));
    }

    if exemption_matched && let Some(first_error) = source_outcomes.iter().position(Result::is_err)
    {
        // An exempted request that could not read every source fails with the
        // first source error rather than a denial: it would not have been
        // enforced had the read succeeded, so an authorization error would
        // send the caller to request an ACL they don't need.
        return source_outcomes.swap_remove(first_error);
    }

    restriction_check::authoritative_sources_enforcement_outcome(source_outcomes)
        .map(|outcome| outcome.exempt_if(exemption_matched))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use anyhow::Result;
    use fbinit::FacebookInit;
    use metaconfig_types::AclManifestMode;
    use mononoke_macros::mononoke;
    use mononoke_types::NonRootMPath;
    use permission_checker::MononokeIdentity;
    use permission_checker::dummy::DummyAclProvider;

    use super::*;
    use crate::restriction_check::AuthorizationCheckResult;
    use crate::test_utils::RestrictedPathsConfigBuilder;
    use crate::test_utils::build_test_restricted_paths_with_dummy_acl_provider as build_test_restricted_paths;
    use crate::test_utils::build_test_restricted_paths_with_options;

    // What it tests: a matching exemption does not apply when a source fails.
    // Expected: a source error is returned, both when the other source denies
    // and when it also fails (the AclManifest error when both fail), so an
    // exempted request fails closed with the source error rather than an
    // authorization error. Without an exemption the denial still wins over
    // the error.
    #[mononoke::test]
    async fn test_enforcement_exemption_returns_source_error() -> Result<()> {
        let error = enforce_with_results(
            Ok(vec![denied_check()?]),
            Err(anyhow::anyhow!("AclManifest source failed")),
            true,
        )
        .await
        .err()
        .context("an exempted request with a failed source should return an error")?;
        assert!(
            format!("{error:#}").contains("AclManifest source failed"),
            "the source error should be returned instead of the denial: {error:#}",
        );

        let error = enforce_with_results(
            Err(anyhow::anyhow!("config source failed")),
            Err(anyhow::anyhow!("AclManifest source failed")),
            true,
        )
        .await
        .err()
        .context("an exemption must not turn a failed lookup into an allow")?;
        assert!(
            format!("{error:#}").contains("AclManifest source failed"),
            "the AclManifest error should be returned when both sources fail: {error:#}",
        );

        assert_eq!(
            enforce_with_results(
                Ok(vec![denied_check()?]),
                Err(anyhow::anyhow!("AclManifest source failed")),
                false,
            )
            .await?
            .denial(),
            Some(&DenyingRestriction {
                restriction_root: Some(NonRootMPath::new("restricted")?),
                repo_region_acl: MononokeIdentity::from_str("REPO_REGION:denied")?,
                permission_request_group: MononokeIdentity::from_str("REPO_REGION:denied")?,
            }),
            "without an exemption the denial should still win over the source error",
        );
        Ok(())
    }

    /// What it tests: the error reported when both authoritative sources fail
    /// and no exemption matches.
    /// Expected: the AclManifest error is returned, not the config error.
    #[mononoke::test]
    async fn test_enforcement_reports_acl_manifest_error_when_both_sources_fail() -> Result<()> {
        let error = enforce_with_results(
            Err(anyhow::anyhow!("config source failed")),
            Err(anyhow::anyhow!("AclManifest source failed")),
            false,
        )
        .await
        .err()
        .context("a request whose sources both failed should return an error")?;
        let message = format!("{error:#}");
        assert!(
            message.contains("AclManifest source failed"),
            "the AclManifest error should be returned: {message}",
        );
        assert!(
            !message.contains("config source failed"),
            "the config error should not be returned: {message}",
        );
        Ok(())
    }

    /// What it tests: the error reported when the AclManifest handle is missing
    /// and the config read fails.
    /// Expected: the config error is returned; the missing-source error stays
    /// behind every real source error.
    #[mononoke::test]
    async fn test_enforcement_reports_source_error_before_missing_source() -> Result<()> {
        let handles = SourceHandles {
            config: Some(SharedFetchHandle::from_result(Err(anyhow::anyhow!(
                "config source failed"
            )))),
            acl_manifest: None,
        };
        let error = enforce_with_handles(&handles, false)
            .await
            .err()
            .context("a request with a failed and a missing source should return an error")?;
        let message = format!("{error:#}");
        assert!(
            message.contains("config source failed"),
            "the config error should be returned: {message}",
        );
        assert!(
            !message.contains("missing source"),
            "the missing-source error should not be returned: {message}",
        );
        Ok(())
    }

    // What it tests: enforcement with both authoritative sources read
    // successfully, with and without a matching exemption.
    // Expected: the denial is returned without an exemption, and the access is
    // exempted with one.
    #[mononoke::test]
    async fn test_enforcement_exemption_applies_when_sources_succeed() -> Result<()> {
        assert_eq!(
            enforce_with_results(Ok(vec![denied_check()?]), Ok(vec![]), false)
                .await?
                .denial(),
            Some(&DenyingRestriction {
                restriction_root: Some(NonRootMPath::new("restricted")?),
                repo_region_acl: MononokeIdentity::from_str("REPO_REGION:denied")?,
                permission_request_group: MononokeIdentity::from_str("REPO_REGION:denied")?,
            }),
            "without an exemption the unauthorized access should be denied",
        );
        assert_eq!(
            enforce_with_results(Ok(vec![denied_check()?]), Ok(vec![]), true).await?,
            AccessEnforcementOutcome::Exempted,
            "a matching exemption should exempt the enforced access",
        );
        Ok(())
    }

    // What it tests: `denial_message` is appended to the error text on its own line.
    // Expected: no message keeps the base text unchanged.
    #[mononoke::test]
    fn test_authorization_error_display_appends_denial_message() -> Result<()> {
        let access = RestrictedPathAccess::Path(MPath::new("restricted/file")?);
        let acl: PermissionRequestGroup = "REPO_REGION:test_acl".parse()?;
        let denial = DenyingRestriction {
            restriction_root: Some(NonRootMPath::new("restricted")?),
            repo_region_acl: acl.clone(),
            permission_request_group: acl,
        };
        let base = "Access denied: unauthorized access to restricted path: restricted/file";

        let without = RestrictedPathsAuthorizationError::new(access.clone(), denial.clone(), None);
        assert_eq!(without.to_string(), base);

        let with = RestrictedPathsAuthorizationError::new(
            access,
            denial,
            Some("see https://fburl.com/example".to_string()),
        );
        assert_eq!(
            with.to_string(),
            format!("{base}\nsee https://fburl.com/example")
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_empty_config(fb: FacebookInit) -> Result<()> {
        let repo_restricted_paths =
            build_test_restricted_paths(fb, RestrictedPathsConfig::default()).await?;

        assert!(!repo_restricted_paths.may_have_restricted_paths());

        let ctx = CoreContext::test_mock(fb);
        let cs_id = ChangesetId::new(mononoke_types::hash::Blake2::from_byte_array([0u8; 32]));
        let test_path = NonRootMPath::new("test/path")?;
        assert!(
            repo_restricted_paths
                .get_path_restriction_root_info(&ctx, Some(cs_id), &[test_path])
                .await?
                .is_empty()
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_with_config(fb: FacebookInit) -> Result<()> {
        let config = RestrictedPathsConfigBuilder::new()
            .with_path_restriction_metadata("restricted/dir", "SERVICE_IDENTITY:restricted_acl")?
            .with_path_restriction_metadata("other/restricted", "SERVICE_IDENTITY:other_acl")?
            .build();

        let repo_restricted_paths = build_test_restricted_paths(fb, config).await?;

        assert!(repo_restricted_paths.may_have_restricted_paths());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_path_matching(fb: FacebookInit) -> Result<()> {
        let restricted_acl = MononokeIdentity::from_str("SERVICE_IDENTITY:restricted_acl")?;
        let config = RestrictedPathsConfigBuilder::new()
            .with_path_restriction_metadata("restricted/dir", &restricted_acl.to_string())?
            .build();

        let repo_restricted_paths = build_test_restricted_paths(fb, config).await?;

        let ctx = CoreContext::test_mock(fb);
        let cs_id = ChangesetId::new(mononoke_types::hash::Blake2::from_byte_array([0u8; 32]));

        // Test exact match
        let exact_path = NonRootMPath::new("restricted/dir")?;
        let results = repo_restricted_paths
            .get_path_restriction_root_info(&ctx, Some(cs_id), &[exact_path])
            .await?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].repo_region_acl, restricted_acl.to_string());

        // Test subdirectory — should NOT match (exact only)
        let sub_path = NonRootMPath::new("restricted/dir/subdir/file.txt")?;
        assert!(
            repo_restricted_paths
                .get_path_restriction_root_info(&ctx, Some(cs_id), &[sub_path])
                .await?
                .is_empty()
        );

        // Test non-matching path
        let other_path = NonRootMPath::new("other/dir/file.txt")?;
        assert!(
            repo_restricted_paths
                .get_path_restriction_root_info(&ctx, Some(cs_id), &[other_path])
                .await?
                .is_empty()
        );

        // Test path that shares parent directory — should NOT match
        let partial_path = NonRootMPath::new("restricted/different")?;
        assert!(
            repo_restricted_paths
                .get_path_restriction_root_info(&ctx, Some(cs_id), &[partial_path])
                .await?
                .is_empty()
        );

        // Test partial match, differing by one character — should NOT match
        let partial_path = NonRootMPath::new("restricted/di")?;
        assert!(
            repo_restricted_paths
                .get_path_restriction_root_info(&ctx, Some(cs_id), &[partial_path])
                .await?
                .is_empty()
        );
        Ok(())
    }

    // What it tests: default Disabled mode does not require AclManifest derived data.
    // Expected: construction succeeds without AclManifest derived data.
    #[mononoke::fbinit_test]
    async fn test_disabled_mode_does_not_require_acl_manifest_derivation(
        fb: FacebookInit,
    ) -> Result<()> {
        let config = RestrictedPathsConfig {
            acl_manifest_mode: AclManifestMode::Disabled,
            ..Default::default()
        };
        let _restricted_paths =
            build_test_restricted_paths_with_options(fb, config, DummyAclProvider::new(fb)?, false)
                .await?;

        assert!(!_restricted_paths.may_have_restricted_paths());
        Ok(())
    }

    // What it tests: Shadow mode requires AclManifest derived data.
    // Expected: construction fails with a clear configuration error.
    #[mononoke::fbinit_test]
    async fn test_shadow_mode_requires_acl_manifest_derivation(fb: FacebookInit) -> Result<()> {
        let config = RestrictedPathsConfig {
            acl_manifest_mode: AclManifestMode::Shadow,
            ..Default::default()
        };
        let result =
            build_test_restricted_paths_with_options(fb, config, DummyAclProvider::new(fb)?, false)
                .await;

        let err = result.err().ok_or_else(|| {
            anyhow::anyhow!(
                "expected Shadow construction to fail when AclManifest derivation is disabled"
            )
        })?;
        assert_error_chain_contains(&err, "acl_manifest_mode is enabled");
        assert_error_chain_contains(&err, "AclManifest derivation is not enabled");
        Ok(())
    }

    // What it tests: Both mode should require AclManifest derived data because
    // it must fetch restrictions from AclManifest as an authoritative source.
    // Expected: construction fails with a clear configuration error.
    #[mononoke::fbinit_test]
    async fn test_both_mode_requires_acl_manifest_derivation(fb: FacebookInit) -> Result<()> {
        let config = RestrictedPathsConfig {
            acl_manifest_mode: AclManifestMode::Both,
            ..Default::default()
        };
        let result =
            build_test_restricted_paths_with_options(fb, config, DummyAclProvider::new(fb)?, false)
                .await;

        let err = result.err().ok_or_else(|| {
            anyhow::anyhow!(
                "expected Both construction to fail when AclManifest derivation is disabled"
            )
        })?;
        assert_error_chain_contains(&err, "acl_manifest_mode is enabled");
        assert_error_chain_contains(&err, "AclManifest derivation is not enabled");
        Ok(())
    }

    // What it tests: Authoritative mode should require AclManifest derived data
    // because config should no longer be the primary restriction source.
    // Expected: construction fails with a clear configuration error.
    #[mononoke::fbinit_test]
    async fn test_authoritative_mode_requires_acl_manifest_derivation(
        fb: FacebookInit,
    ) -> Result<()> {
        let config = RestrictedPathsConfig {
            acl_manifest_mode: AclManifestMode::Authoritative,
            ..Default::default()
        };
        let result =
            build_test_restricted_paths_with_options(fb, config, DummyAclProvider::new(fb)?, false)
                .await;

        let err = result.err().ok_or_else(|| {
            anyhow::anyhow!(
                "expected Authoritative construction to fail when AclManifest derivation is disabled"
            )
        })?;
        assert_error_chain_contains(&err, "acl_manifest_mode is enabled");
        assert_error_chain_contains(&err, "AclManifest derivation is not enabled");
        Ok(())
    }

    // What it tests: default Disabled mode keeps config-backed lookup behavior.
    // Expected: the config source remains authoritative in Disabled mode.
    #[mononoke::fbinit_test]
    async fn test_disabled_mode_keeps_config_authoritative_lookup(fb: FacebookInit) -> Result<()> {
        let config = RestrictedPathsConfigBuilder::new()
            .with_path_restriction_metadata("restricted/dir", "SERVICE_IDENTITY:restricted_acl")?
            .build();
        let restricted_paths =
            build_test_restricted_paths_with_options(fb, config, DummyAclProvider::new(fb)?, false)
                .await?;
        let ctx = CoreContext::test_mock(fb);
        let cs_id = ChangesetId::new(mononoke_types::hash::Blake2::from_byte_array([0u8; 32]));
        let path = NonRootMPath::new("restricted/dir/file")?;
        let lookup = restricted_paths
            .get_path_restriction_info(&ctx, Some(cs_id), &[path])
            .await?;

        assert_eq!(lookup.len(), 1);
        assert_eq!(lookup[0].repo_region_acl, "SERVICE_IDENTITY:restricted_acl");
        Ok(())
    }

    /// Runs enforcement over both authoritative sources with the given source
    /// results and a definite pre-filter match, so enforcement only depends on
    /// the source results and the exemption.
    async fn enforce_with_results(
        config_result: Result<Vec<PathRestrictionCheckResult>>,
        acl_manifest_result: Result<Vec<PathRestrictionCheckResult>>,
        exemption_matched: bool,
    ) -> Result<AccessEnforcementOutcome> {
        let handles = SourceHandles {
            config: Some(SharedFetchHandle::from_result(config_result)),
            acl_manifest: Some(SharedFetchHandle::from_result(acl_manifest_result)),
        };
        enforce_with_handles(&handles, exemption_matched).await
    }

    /// Runs enforcement with both authoritative sources selected over the
    /// given handles and a definite pre-filter match.
    async fn enforce_with_handles(
        handles: &SourceHandles<PathRestrictionCheckResult>,
        exemption_matched: bool,
    ) -> Result<AccessEnforcementOutcome> {
        enforce_with_source_handles(
            true,
            true,
            handles,
            PreFilteredRequest {
                conditions: PreFilterResult::DefiniteMatch { candidates: vec![] },
                exemption_matched,
            },
            anyhow::anyhow!("missing source"),
        )
        .await
    }

    /// A source check for `restricted` that denies the caller.
    fn denied_check() -> Result<PathRestrictionCheckResult> {
        let denied_acl = MononokeIdentity::from_str("REPO_REGION:denied")?;
        Ok(PathRestrictionCheckResult::new(
            PathRestrictionInfo {
                restriction_root: NonRootMPath::new("restricted")?,
                repo_region_acl: denied_acl.to_string(),
                permission_request_group: denied_acl.clone(),
                rollout_allowlist_group: None,
            },
            AuthorizationCheckResult::new(false, false, false, false),
            denied_acl,
        ))
    }

    fn assert_error_chain_contains(err: &anyhow::Error, needle: &str) {
        assert!(
            err.chain().any(|err| err.to_string().contains(needle)),
            "error chain should contain '{needle}', got: {err:?}"
        );
    }
}
