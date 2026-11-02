/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use acl_regions::AclRegionsRef;
use anyhow::Result;
use anyhow::anyhow;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkKind;
use commit_cloud::CommitCloudRef;
use commit_cloud::ctx::CommitCloudContext;
use commit_cloud_helpers::make_workspace_acl_name;
#[cfg(fbcode_build)]
use commit_cloud_intern_utils::acl_check::infer_workspace_identity;
#[cfg(fbcode_build)]
use commit_cloud_intern_utils::acl_check::requester_is_manager_of_departed_owner;
use context::CoreContext;
use metaconfig_types::RepoConfigRef;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::path::MPath;
use permission_checker::AclProvider;
use permission_checker::PermissionDenial;
use repo_bookmark_attrs::RepoBookmarkAttrsRef;
use repo_identity::RepoIdentityRef;
use repo_permission_checker::RepoPermissionCheckerRef;

use crate::error::AuthorizationError;
use crate::error::DeniedAction;
use crate::error::PermissionDenied;

const GIT_IMPORT_SVC_WRITE_METHOD: &str = "git_import_operations";

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum AuthorizationContext {
    /// Access is always granted.  Should only be used by internal tools and
    /// tests.
    FullAccess,

    /// Access is granted based on the caller's identity.
    Identity,

    /// Access is granted only for reads. All write and draft operations are forbidden.
    ReadOnlyIdentity,

    /// Access is granted for reads and draft operations. Public writes are forbidden.
    /// Represents off-VPN access.
    DraftOnlyIdentity,

    /// Access is granted based on the caller acting as a named service.
    Service(String),
}

impl AuthorizationContext {
    /// Create a new authorization context.
    ///
    /// This context will use the user's identity to check whether they are
    /// authorized to perform each action.
    pub fn new(ctx: &CoreContext) -> AuthorizationContext {
        // The order matters here since read-only is more restrictive than draft-only.
        if ctx.session().is_readonly() {
            AuthorizationContext::ReadOnlyIdentity
        } else if ctx.session().metadata().client_untrusted() {
            AuthorizationContext::DraftOnlyIdentity
        } else {
            AuthorizationContext::Identity
        }
    }

    /// Create a new authorization context.
    ///
    /// This context will use the user's identity to check whether they are
    /// authorized to perform each action.
    /// Doesn't consider whether use is trusted or not, returning DraftOnlyIdentity.
    pub fn new_non_draft(ctx: &CoreContext) -> AuthorizationContext {
        if ctx.session().is_readonly() {
            AuthorizationContext::ReadOnlyIdentity
        } else {
            AuthorizationContext::Identity
        }
    }

    /// Create a new authorization context.
    ///
    /// This context will use the user's identity to check whether they are
    /// permitted to act as the named service, and then check the service
    /// is permitted to perform each action.
    pub fn new_for_service_writes(service_name: impl Into<String>) -> AuthorizationContext {
        AuthorizationContext::Service(service_name.into())
    }

    /// Create a new authorization context.
    ///
    /// This context will permit all actions, and so must only be used in
    /// internal tools and tests.
    pub fn new_bypass_access_control() -> AuthorizationContext {
        AuthorizationContext::FullAccess
    }

    /// Authorization context for requests to a server built for OSS. Such
    /// servers only accept loopback connections and cannot authenticate
    /// callers, so every request is allowed, as with
    /// [`Self::new_bypass_access_control`].
    #[cfg(not(fbcode_build))]
    pub fn new_oss_local() -> AuthorizationContext {
        AuthorizationContext::FullAccess
    }

    /// Returns true if this context is for a service write.
    pub fn is_service(&self) -> bool {
        matches!(self, AuthorizationContext::Service(_))
    }

    /// Returns service identiry for a service write.
    pub fn service_identity(&self) -> Option<String> {
        match self {
            AuthorizationContext::Service(service_name) => Some(service_name.clone()),
            _ => None,
        }
    }

    /// Create a permission denied error for a particular action for a named
    /// repo.
    fn permission_denied_name(
        &self,
        ctx: &CoreContext,
        repo_name: &str,
        denied_action: DeniedAction,
    ) -> AuthorizationError {
        AuthorizationError::from(Box::new(PermissionDenied {
            denied_action,
            denied_repo_name: repo_name.to_string(),
            context: self.clone(),
            identities: ctx.metadata().identities().clone(),
            denial: None,
        }))
    }

    /// Create a permission denied error for a particular action.
    fn permission_denied(
        &self,
        ctx: &CoreContext,
        repo: &impl RepoIdentityRef,
        denied_action: DeniedAction,
    ) -> AuthorizationError {
        self.permission_denied_name(ctx, repo.repo_identity().name(), denied_action)
    }

    /// Create a permission denied error that reports what the access checker
    /// said, so the user can tell a missing grant from a policy rejection.
    fn permission_denied_with_reason(
        &self,
        ctx: &CoreContext,
        repo: &impl RepoIdentityRef,
        denied_action: DeniedAction,
        denial: Option<PermissionDenial>,
    ) -> AuthorizationError {
        AuthorizationError::from(Box::new(PermissionDenied {
            denied_action,
            denied_repo_name: repo.repo_identity().name().to_string(),
            context: self.clone(),
            identities: ctx.metadata().identities().clone(),
            denial,
        }))
    }

    /// Check if user has read access to the full repo.
    pub async fn check_full_repo_read(
        &self,
        ctx: &CoreContext,
        repo: &impl RepoPermissionCheckerRef,
    ) -> AuthorizationCheckOutcome {
        let permitted = match self {
            AuthorizationContext::FullAccess => true,
            AuthorizationContext::Identity
            | AuthorizationContext::ReadOnlyIdentity
            | AuthorizationContext::DraftOnlyIdentity
            | AuthorizationContext::Service(_) => {
                // Check the caller's identity permits read access.  Acting as
                // a service does not change read access, so we check the
                // identity in this case also.
                repo.repo_permission_checker()
                    .check_if_read_access_allowed(ctx.metadata().identities())
                    .await
            }
        };
        AuthorizationCheckOutcome::from_permitted(permitted)
    }

    /// Require that the user has read access to the full repo.
    pub async fn require_full_repo_read(
        &self,
        ctx: &CoreContext,
        repo: &(impl RepoPermissionCheckerRef + RepoIdentityRef),
    ) -> Result<(), AuthorizationError> {
        self.check_full_repo_read(ctx, repo)
            .await
            .permitted_or_else(|| self.permission_denied(ctx, repo, DeniedAction::FullRepoRead))
    }

    /// Check if user has read access to the repo metadata.
    ///
    /// The repo metadata is the bookmarks and changesets, but not the
    /// manifests or file contents.
    pub async fn check_repo_metadata_read(
        &self,
        ctx: &CoreContext,
        repo: &impl RepoPermissionCheckerRef,
    ) -> AuthorizationCheckOutcome {
        let permitted = match self {
            AuthorizationContext::FullAccess => true,
            AuthorizationContext::Identity
            | AuthorizationContext::ReadOnlyIdentity
            | AuthorizationContext::DraftOnlyIdentity
            | AuthorizationContext::Service(_) => {
                // Check the caller's identity permits read access.  Acting as
                // a service does not change read access, so we check the
                // identity in this case also.
                repo.repo_permission_checker()
                    .check_if_read_access_allowed(ctx.metadata().identities())
                    .await ||
                // Check if the caller can access via path ACLs.
                repo.repo_permission_checker()
                    .check_if_any_region_read_access_allowed(ctx.metadata().identities())
                    .await
            }
        };
        AuthorizationCheckOutcome::from_permitted(permitted)
    }

    /// Check if user has read access to the repo metadata, also returning
    /// the identity type string that granted access (if any).
    pub async fn check_repo_metadata_read_with_result(
        &self,
        ctx: &CoreContext,
        repo: &impl RepoPermissionCheckerRef,
    ) -> (AuthorizationCheckOutcome, Option<String>) {
        match self {
            AuthorizationContext::FullAccess => (AuthorizationCheckOutcome::Permitted, None),
            AuthorizationContext::Identity
            | AuthorizationContext::ReadOnlyIdentity
            | AuthorizationContext::DraftOnlyIdentity
            | AuthorizationContext::Service(_) => {
                // Check the caller's identity permits read access.  Acting as
                // a service does not change read access, so we check the
                // identity in this case also.
                let result = repo
                    .repo_permission_checker()
                    .check_if_read_access_allowed_with_result(ctx.metadata().identities())
                    .await;
                if result.is_allowed() {
                    return (
                        AuthorizationCheckOutcome::Permitted,
                        result.deciding_identity_type().map(str::to_owned),
                    );
                }
                // Check if the caller can access via path ACLs.
                if repo
                    .repo_permission_checker()
                    .check_if_any_region_read_access_allowed(ctx.metadata().identities())
                    .await
                {
                    return (AuthorizationCheckOutcome::Permitted, None);
                }
                (AuthorizationCheckOutcome::Denied, None)
            }
        }
    }

    /// Require that the user has read access to the repo metadata.
    pub async fn require_repo_metadata_read(
        &self,
        ctx: &CoreContext,
        repo: &(impl RepoPermissionCheckerRef + RepoIdentityRef),
    ) -> Result<(), AuthorizationError> {
        self.check_repo_metadata_read(ctx, repo)
            .await
            .permitted_or_else(|| self.permission_denied(ctx, repo, DeniedAction::RepoMetadataRead))
    }

    /// Require that the user has read access to the repo metadata,
    /// also returning the identity type string that granted access (if any).
    pub async fn require_repo_metadata_read_with_result(
        &self,
        ctx: &CoreContext,
        repo: &(impl RepoPermissionCheckerRef + RepoIdentityRef),
    ) -> Result<Option<String>, AuthorizationError> {
        let (outcome, id_type) = self.check_repo_metadata_read_with_result(ctx, repo).await;
        outcome.permitted_or_else(|| {
            self.permission_denied(ctx, repo, DeniedAction::RepoMetadataRead)
        })?;
        Ok(id_type)
    }

    pub async fn check_path_read(
        &self,
        ctx: &CoreContext,
        repo: &(impl RepoPermissionCheckerRef + AclRegionsRef),
        csid: ChangesetId,
        path: &MPath,
    ) -> Result<AuthorizationCheckOutcome> {
        let permitted = match self {
            AuthorizationContext::FullAccess => true,
            AuthorizationContext::Identity
            | AuthorizationContext::ReadOnlyIdentity
            | AuthorizationContext::DraftOnlyIdentity
            | AuthorizationContext::Service(_) => {
                // Check the caller's identity permits read access.  Acting as
                // a service does not change read access, so we check the
                // identity in this case also.
                repo.repo_permission_checker()
                    .check_if_read_access_allowed(ctx.metadata().identities())
                    .await
                    || {
                        let rules = repo.acl_regions().associated_rules(ctx, csid, path).await?;
                        let acls = rules.hipster_acls();
                        repo.repo_permission_checker()
                            .check_if_region_read_access_allowed(&acls, ctx.metadata().identities())
                            .await
                    }
            }
        };
        Ok(AuthorizationCheckOutcome::from_permitted(permitted))
    }

    /// Check if user has read access to the given path, also returning
    /// the identity type string that granted access (if any).
    pub async fn check_path_read_with_result(
        &self,
        ctx: &CoreContext,
        repo: &(impl RepoPermissionCheckerRef + AclRegionsRef),
        csid: ChangesetId,
        path: &MPath,
    ) -> Result<(AuthorizationCheckOutcome, Option<String>)> {
        match self {
            AuthorizationContext::FullAccess => Ok((AuthorizationCheckOutcome::Permitted, None)),
            AuthorizationContext::Identity
            | AuthorizationContext::ReadOnlyIdentity
            | AuthorizationContext::DraftOnlyIdentity
            | AuthorizationContext::Service(_) => {
                // Check the caller's identity permits read access.  Acting as
                // a service does not change read access, so we check the
                // identity in this case also.
                let result = repo
                    .repo_permission_checker()
                    .check_if_read_access_allowed_with_result(ctx.metadata().identities())
                    .await;
                if result.is_allowed() {
                    return Ok((
                        AuthorizationCheckOutcome::Permitted,
                        result.deciding_identity_type().map(str::to_owned),
                    ));
                }
                let rules = repo.acl_regions().associated_rules(ctx, csid, path).await?;
                let acls = rules.hipster_acls();
                let region_result = repo
                    .repo_permission_checker()
                    .check_if_region_read_access_allowed_with_result(
                        &acls,
                        ctx.metadata().identities(),
                    )
                    .await;
                if region_result.is_allowed() {
                    return Ok((
                        AuthorizationCheckOutcome::Permitted,
                        region_result.deciding_identity_type().map(str::to_owned),
                    ));
                }
                Ok((AuthorizationCheckOutcome::Denied, None))
            }
        }
    }

    pub async fn require_path_read(
        &self,
        ctx: &CoreContext,
        repo: &(impl RepoPermissionCheckerRef + AclRegionsRef + RepoIdentityRef),
        csid: ChangesetId,
        path: &MPath,
    ) -> Result<(), AuthorizationError> {
        self.check_path_read(ctx, repo, csid, path)
            .await?
            .permitted_or_else(|| {
                self.permission_denied(ctx, repo, DeniedAction::PathRead(csid, path.clone()))
            })
    }

    /// Require that the user has read access to the given path,
    /// also returning the identity type string that granted access (if any).
    pub async fn require_path_read_with_result(
        &self,
        ctx: &CoreContext,
        repo: &(impl RepoPermissionCheckerRef + AclRegionsRef + RepoIdentityRef),
        csid: ChangesetId,
        path: &MPath,
    ) -> Result<Option<String>, AuthorizationError> {
        let (outcome, id_type) = self
            .check_path_read_with_result(ctx, repo, csid, path)
            .await?;
        outcome.permitted_or_else(|| {
            self.permission_denied(ctx, repo, DeniedAction::PathRead(csid, path.clone()))
        })?;
        Ok(id_type)
    }

    /// Check whether the user has general draft access to the repo.
    ///
    /// This does not check specific paths or bookmarks, which must be checked
    /// separately.
    pub async fn check_full_repo_draft(
        &self,
        ctx: &CoreContext,
        repo: &impl RepoPermissionCheckerRef,
    ) -> AuthorizationCheckOutcome {
        let permitted = match self {
            AuthorizationContext::FullAccess => true,
            AuthorizationContext::Identity | AuthorizationContext::DraftOnlyIdentity => {
                repo.repo_permission_checker()
                    .check_if_draft_access_allowed_with_tunable_enforcement(
                        ctx,
                        ctx.metadata().identities(),
                    )
                    .await
            }
            // The services have narrowly defined permissions. Never full-repo.
            AuthorizationContext::Service(..) | AuthorizationContext::ReadOnlyIdentity => false,
        };
        AuthorizationCheckOutcome::from_permitted(permitted)
    }

    /// Require that the user has general draft access to the repo, and return
    /// and error if they do not.
    ///
    /// This does not check specific paths or bookmarks, which must be checked
    /// separately.
    pub async fn require_full_repo_draft(
        &self,
        ctx: &CoreContext,
        repo: &(impl RepoPermissionCheckerRef + RepoIdentityRef),
    ) -> Result<(), AuthorizationError> {
        self.check_full_repo_draft(ctx, repo)
            .await
            .permitted_or_else(|| self.permission_denied(ctx, repo, DeniedAction::FullRepoDraft))
    }

    /// Check whether the user has general write access to the repo.
    ///
    /// This does not check specific paths or bookmarks, which must be checked
    /// separately.
    ///
    /// In cases where write operation covers draft data the draft access will
    /// be used.
    pub async fn check_repo_write(
        &self,
        ctx: &CoreContext,
        repo: &(impl RepoPermissionCheckerRef + RepoConfigRef),
        op: RepoWriteOperation,
    ) -> AuthorizationCheckOutcome {
        let permitted = match self {
            AuthorizationContext::FullAccess => true,
            AuthorizationContext::DraftOnlyIdentity => {
                if op.is_draft() {
                    repo.repo_permission_checker()
                        .check_if_draft_access_allowed_with_tunable_enforcement(
                            ctx,
                            ctx.metadata().identities(),
                        )
                        .await
                } else {
                    false
                }
            }
            AuthorizationContext::Identity => {
                if op.is_draft() {
                    repo.repo_permission_checker()
                        .check_if_draft_access_allowed_with_tunable_enforcement(
                            ctx,
                            ctx.metadata().identities(),
                        )
                        .await
                } else {
                    repo.repo_permission_checker()
                        .check_if_write_access_allowed(ctx.metadata().identities())
                        .await
                }
            }
            AuthorizationContext::Service(service_name) => {
                // Check the caller is permitted to act as this service.
                repo
                    .repo_permission_checker()
                    .check_if_service_writes_allowed(ctx.metadata().identities(), service_name)
                    .await &&
                // Check the service is allowed to perform this operation
                repo
                    .repo_config()
                    .source_control_service
                    .service_write_method_permitted(service_name, op.method_name())
            }
            AuthorizationContext::ReadOnlyIdentity => false,
        };
        AuthorizationCheckOutcome::from_permitted(permitted)
    }

    /// As check_repo_write, but also returns what the access checker said when
    /// it denied a public write. That is the check a rejected `git push` or
    /// `hg push` to a public bookmark fails, and the one whose reason is worth
    /// reporting back to the user.
    async fn check_repo_write_with_reason(
        &self,
        ctx: &CoreContext,
        repo: &(impl RepoPermissionCheckerRef + RepoConfigRef),
        op: RepoWriteOperation,
    ) -> (AuthorizationCheckOutcome, Option<PermissionDenial>) {
        match self {
            AuthorizationContext::Identity if !op.is_draft() => {
                let result = repo
                    .repo_permission_checker()
                    .check_if_write_access_allowed_with_result(ctx.metadata().identities())
                    .await;
                (
                    AuthorizationCheckOutcome::from_permitted(result.is_allowed()),
                    result.into_denial(),
                )
            }
            _ => (self.check_repo_write(ctx, repo, op).await, None),
        }
    }

    /// Require that the user has general write access to the repo, and return
    /// and error if they do not.
    ///
    /// This does not check specific paths or bookmarks, which must be checked
    /// separately.
    pub async fn require_repo_write(
        &self,
        ctx: &CoreContext,
        repo: &(impl RepoPermissionCheckerRef + RepoConfigRef + RepoIdentityRef),
        op: RepoWriteOperation,
    ) -> Result<(), AuthorizationError> {
        let (outcome, denial) = self.check_repo_write_with_reason(ctx, repo, op).await;
        outcome.permitted_or_else(|| {
            self.permission_denied_with_reason(
                ctx,
                repo,
                DeniedAction::RepoWrite(op),
                denial.clone(),
            )
        })
    }

    /// Check whether a user with write permissions may write to any path in the repo.
    pub async fn check_any_path_write(
        &self,
        _ctx: &CoreContext,
        repo: &impl RepoConfigRef,
    ) -> AuthorizationCheckOutcome {
        let permitted = match self {
            AuthorizationContext::FullAccess | AuthorizationContext::Identity => true,
            AuthorizationContext::Service(service_name) => repo
                .repo_config()
                .source_control_service
                .service_write_all_paths_permitted(service_name),
            AuthorizationContext::ReadOnlyIdentity | AuthorizationContext::DraftOnlyIdentity => {
                false
            }
        };
        AuthorizationCheckOutcome::from_permitted(permitted)
    }

    /// Require that a user with write permissions may write to the paths in
    /// a changeset (i.e., is the user permitted to land this changeset, based
    /// on the paths that it touches).
    pub async fn require_changeset_paths_write(
        &self,
        ctx: &CoreContext,
        repo: &(impl RepoConfigRef + RepoIdentityRef),
        changeset: &BonsaiChangeset,
    ) -> Result<(), AuthorizationError> {
        match self {
            AuthorizationContext::FullAccess | AuthorizationContext::Identity => Ok(()),
            AuthorizationContext::Service(service_name) => repo
                .repo_config()
                .source_control_service
                .service_write_paths_permitted(service_name, changeset)
                .map_err(|path| {
                    self.permission_denied(ctx, repo, DeniedAction::PathWrite(path.clone()))
                }),
            AuthorizationContext::ReadOnlyIdentity | AuthorizationContext::DraftOnlyIdentity => {
                Err(self.permission_denied(
                    ctx,
                    repo,
                    DeniedAction::PathWrite(
                        changeset
                            .file_changes_map()
                            .keys()
                            .next()
                            .cloned()
                            .ok_or_else(|| anyhow!("no writes allowed in readonly mode!"))?,
                    ),
                ))
            }
        }
    }

    /// Check whether the user is allowed to modify (create, update or delete)
    /// a particular bookmark.
    pub async fn check_bookmark_modify(
        &self,
        ctx: &CoreContext,
        repo: &(impl RepoConfigRef + RepoBookmarkAttrsRef),
        bookmark: &BookmarkKey,
    ) -> AuthorizationCheckOutcome {
        let permitted = match self {
            AuthorizationContext::FullAccess => true,
            AuthorizationContext::Identity | AuthorizationContext::DraftOnlyIdentity => {
                let user = ctx.metadata().unix_name().unwrap_or("svcscm");
                repo.repo_bookmark_attrs()
                    .is_allowed_user(ctx, user, bookmark)
                    .await

                // TODO: Check using ctx.identities, and deny if neither are provided.
            }
            AuthorizationContext::Service(service_name) => {
                // Check this service is permitted to modify this bookmark.
                repo.repo_config()
                    .source_control_service
                    .service_write_bookmark_permitted(service_name, bookmark)
            }
            AuthorizationContext::ReadOnlyIdentity => false,
        };
        AuthorizationCheckOutcome::from_permitted(permitted)
    }

    /// Require that the user is allowed to modify (create, update or delete)
    /// a particular bookmark.
    pub async fn require_bookmark_modify(
        &self,
        ctx: &CoreContext,
        repo: &(impl RepoConfigRef + RepoBookmarkAttrsRef + RepoIdentityRef),
        bookmark: &BookmarkKey,
    ) -> Result<(), AuthorizationError> {
        self.check_bookmark_modify(ctx, repo, bookmark)
            .await
            .permitted_or_else(|| {
                self.permission_denied(
                    ctx,
                    repo,
                    DeniedAction::BookmarkModification(bookmark.clone()),
                )
            })
    }

    /// Check whether the user is allowed to set the Git mapping for a
    /// changeset to a commit that we cannot prove is round-trippable for
    /// the given Git commit id.
    pub async fn check_override_git_mapping(
        &self,
        _ctx: &CoreContext,
        repo: &impl RepoConfigRef,
    ) -> AuthorizationCheckOutcome {
        let permitted = match self {
            AuthorizationContext::FullAccess => true,
            AuthorizationContext::Identity => {
                // Users are never allowed to do this.
                false
            }
            AuthorizationContext::Service(service_name) => {
                // Services are allowed to do this if they are configured to
                // allow the method.
                repo.repo_config()
                    .source_control_service
                    .service_write_method_permitted(service_name, "set_git_mapping_from_changeset")
            }
            AuthorizationContext::ReadOnlyIdentity | AuthorizationContext::DraftOnlyIdentity => {
                false
            }
        };
        AuthorizationCheckOutcome::from_permitted(permitted)
    }

    /// Require that the user is allowed to set the Git mapping for a
    /// changeset to a commit that we cannot prove is round-trippable for
    /// the given Git commit id.
    pub async fn require_override_git_mapping(
        &self,
        ctx: &CoreContext,
        repo: &(impl RepoConfigRef + RepoIdentityRef),
    ) -> Result<(), AuthorizationError> {
        self.check_override_git_mapping(ctx, repo)
            .await
            .permitted_or_else(|| {
                self.permission_denied(ctx, repo, DeniedAction::OverrideGitMapping)
            })
    }

    /// Check whether the caller is allowed to invoke git-import related
    /// operations for the given repo.
    pub async fn check_git_import_operations(
        &self,
        _ctx: &CoreContext,
        repo: &impl RepoConfigRef,
    ) -> AuthorizationCheckOutcome {
        let permitted = match self {
            AuthorizationContext::FullAccess => true,
            AuthorizationContext::Identity => {
                // Users are never allowed to do this.
                false
            }
            AuthorizationContext::Service(service_name) => {
                // Services are allowed to do this if they are configured to
                // allow the method.
                repo.repo_config()
                    .source_control_service
                    .service_write_method_permitted(service_name, GIT_IMPORT_SVC_WRITE_METHOD)
            }
            AuthorizationContext::ReadOnlyIdentity | AuthorizationContext::DraftOnlyIdentity => {
                false
            }
        };
        AuthorizationCheckOutcome::from_permitted(permitted)
    }

    /// Require that the caller is allowed to invoke git-import related
    /// operations for the given repo.
    pub async fn require_git_import_operations(
        &self,
        ctx: &CoreContext,
        repo: &(impl RepoConfigRef + RepoIdentityRef),
    ) -> Result<(), AuthorizationError> {
        self.check_git_import_operations(ctx, repo)
            .await
            .permitted_or_else(|| {
                self.permission_denied(ctx, repo, DeniedAction::GitImportOperation)
            })
    }

    /// Check whether the caller is allowed to create a repo.
    pub async fn check_repo_create(
        &self,
        ctx: &CoreContext,
        repo_name: &str,
        acl_provider: &dyn AclProvider,
    ) -> AuthorizationCheckOutcome {
        let permitted = match self {
            AuthorizationContext::FullAccess => true,
            AuthorizationContext::Service(_service_name) => {
                // Services should use the normal "identity" access for this
                // (because service-level permissions are configured on existing repos)
                // Services are allowed to do this if they are configured to
                // allow the method.
                false
            }
            AuthorizationContext::Identity => {
                // Here we're replicating current logic used on our Git servers. Once we get rid of them
                // let's make this more generic.
                let acl_name = if repo_name.starts_with("aosp/") {
                    "repos/git/aosp"
                } else {
                    "repos"
                };
                let acl = acl_provider.repo_acl(acl_name).await;
                if let Ok(acl) = acl {
                    acl.check_set(ctx.metadata().identities(), &["create"])
                        .await
                } else {
                    false
                }
            }
            AuthorizationContext::ReadOnlyIdentity | AuthorizationContext::DraftOnlyIdentity => {
                false
            }
        };
        AuthorizationCheckOutcome::from_permitted(permitted)
    }

    /// Require that the caller is allowed to create given repo.
    pub async fn require_repo_create(
        &self,
        ctx: &CoreContext,
        repo_name: &str,
        acl_provider: &dyn AclProvider,
    ) -> Result<(), AuthorizationError> {
        self.check_repo_create(ctx, repo_name, acl_provider)
            .await
            .permitted_or_else(|| {
                self.permission_denied_name(ctx, repo_name, DeniedAction::CreateRepo)
            })
    }

    /// Check whether the caller is allowed to operate on certain commit cloud workspace.
    pub async fn check_commitcloud_operation(
        &self,
        ctx: &CoreContext,
        repo: &impl CommitCloudRef,
        cc_ctx: &mut CommitCloudContext,
        action: &str,
    ) -> AuthorizationCheckOutcome {
        let permitted = match self {
            AuthorizationContext::FullAccess => true,
            AuthorizationContext::Identity | AuthorizationContext::DraftOnlyIdentity => {
                #[cfg(fbcode_build)]
                {
                    if cc_ctx.owner.is_none() {
                        let inferred_owner = infer_workspace_identity(
                            ctx.fb,
                            &cc_ctx.workspace,
                            repo.commit_cloud().config.mocked_employees.clone(),
                        )
                        .await;

                        match inferred_owner {
                            Ok(owner) => {
                                if owner.is_some() {
                                    cc_ctx.set_owner(owner)
                                } else {
                                    // In CI some workspaces don't allow for an owner to be identified,
                                    // don't block the commit cloud operation in these cases
                                    ctx.scuba().clone().log_with_msg(
                                        "commit cloud ACL check fail",
                                        Some("unidentified owner, granting access".to_owned()),
                                    );
                                    return AuthorizationCheckOutcome::from_permitted(true);
                                }
                            }
                            Err(_) => {}
                        };
                    }
                    match &cc_ctx.owner {
                        Some(owner_identities) => {
                            // Check if any requesting identity matches any owner identity.
                            for identity in ctx.metadata().identities().iter() {
                                for owner in owner_identities.iter() {
                                    if identity.id_type() == owner.id_type()
                                        && identity.id_data() == owner.id_data()
                                    {
                                        return AuthorizationCheckOutcome::from_permitted(true);
                                    }
                                }
                            }
                        }
                        None => (),
                    };
                }

                match repo
                    .commit_cloud()
                    .commit_cloud_acl(&make_workspace_acl_name(
                        &cc_ctx.workspace,
                        &cc_ctx.reponame,
                    ))
                    .await
                {
                    Ok(Some(checker)) => {
                        if checker
                            .check_set(ctx.metadata().identities(), &[action])
                            .await
                        {
                            ctx.scuba().clone().log_with_msg(
                                "commit cloud ACL check success",
                                Some("ACL check".to_owned()),
                            );
                            return AuthorizationCheckOutcome::from_permitted(true);
                        }
                    }
                    Err(_) | Ok(None) => (),
                }

                match repo.commit_cloud().commit_cloud_acl("allow_list").await {
                    Ok(Some(checker)) => {
                        if checker
                            .check_set(ctx.metadata().identities(), &[action])
                            .await
                        {
                            return AuthorizationCheckOutcome::from_permitted(true);
                        }
                    }
                    Err(_) | Ok(None) => (),
                }

                #[cfg(fbcode_build)]
                {
                    // Fallback: allow the direct manager of a departed workspace
                    // owner to recover the report's Commit Cloud workspace. Gated
                    // by a default-off, per-repo JustKnob killswitch. We read it
                    // via the Mononoke justknobs facade, which surfaces a read
                    // failure loudly rather than silently defaulting on; a knob
                    // that evaluates to false falls through to deny (fail closed).
                    if justknobs::eval(
                        "scm/mononoke:commitcloud_manager_access",
                        None,
                        Some(&cc_ctx.reponame),
                    ) {
                        match requester_is_manager_of_departed_owner(
                            ctx.fb,
                            &cc_ctx.workspace,
                            ctx.metadata().identities(),
                            repo.commit_cloud().config.mocked_employees.clone(),
                        )
                        .await
                        {
                            Ok(true) => {
                                ctx.scuba().clone().log_with_msg(
                                    "commit cloud ACL check success",
                                    Some(format!("manager of departed owner (action: {action})")),
                                );
                                return AuthorizationCheckOutcome::from_permitted(true);
                            }
                            // Not a manager, or resolution failed: fall through to deny.
                            Ok(false) | Err(_) => {}
                        }
                    }
                }

                ctx.scuba().clone().log_with_msg(
                    "commit cloud ACL check failed",
                    Some(format!(
                        "No access to workspace {} on repo {} for client with identities {:?}",
                        cc_ctx.workspace,
                        cc_ctx.reponame,
                        ctx.metadata().identities()
                    )),
                );
                false
            }
            AuthorizationContext::Service(_service_name) => false,
            AuthorizationContext::ReadOnlyIdentity => false,
        };
        AuthorizationCheckOutcome::from_permitted(permitted)
    }

    /// Require that the caller is allowed to operate on certain commit cloud workspace.
    pub async fn require_commitcloud_operation(
        &self,
        ctx: &CoreContext,
        repo: &(impl CommitCloudRef + RepoIdentityRef),
        cc_ctx: &mut CommitCloudContext,
        action: &str,
    ) -> Result<(), AuthorizationError> {
        self.check_commitcloud_operation(ctx, repo, cc_ctx, action)
            .await
            .permitted_or_else(|| {
                self.permission_denied(
                    ctx,
                    repo,
                    DeniedAction::CommitCloudOperation(
                        action.to_string(),
                        cc_ctx.workspace.clone(),
                    ),
                )
            })
    }

    /// Check whether the caller is allowed to invoke mirror upload related
    /// operations for the given repo.
    pub async fn check_mirror_upload_operations(
        &self,
        ctx: &CoreContext,
        repo: &impl RepoPermissionCheckerRef,
    ) -> AuthorizationCheckOutcome {
        let permitted = match self {
            AuthorizationContext::FullAccess => true,
            AuthorizationContext::Identity | AuthorizationContext::Service(_) => {
                repo.repo_permission_checker()
                    .check_if_mirror_upload_allowed(ctx.metadata().identities())
                    .await
            }
            AuthorizationContext::ReadOnlyIdentity | AuthorizationContext::DraftOnlyIdentity => {
                false
            }
        };
        AuthorizationCheckOutcome::from_permitted(permitted)
    }

    /// Require that the caller is allowed to invoke mirror_upload operations
    pub async fn require_mirror_upload_operations(
        &self,
        ctx: &CoreContext,
        repo: &(impl RepoPermissionCheckerRef + RepoIdentityRef),
    ) -> Result<(), AuthorizationError> {
        self.check_mirror_upload_operations(ctx, repo)
            .await
            .permitted_or_else(|| self.permission_denied(ctx, repo, DeniedAction::MirrorUpload))
    }

    /// Check whether the caller is allowed to bypass the create-commit checks.
    pub async fn check_create_commit_check_bypass(
        &self,
        ctx: &CoreContext,
        repo: &(impl RepoPermissionCheckerRef + RepoConfigRef),
    ) -> AuthorizationCheckOutcome {
        let permitted = match self {
            AuthorizationContext::FullAccess => true,
            AuthorizationContext::Service(service_name) => {
                repo.repo_permission_checker()
                    .check_if_service_writes_allowed(ctx.metadata().identities(), service_name)
                    .await
                    && repo
                        .repo_config()
                        .source_control_service
                        .service_create_commit_check_bypass_permitted(service_name)
            }
            AuthorizationContext::Identity
            | AuthorizationContext::ReadOnlyIdentity
            | AuthorizationContext::DraftOnlyIdentity => false,
        };
        AuthorizationCheckOutcome::from_permitted(permitted)
    }
}

/// Write operations that can be performed on a repo.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RepoWriteOperation {
    /// Create a new (draft) changeset.
    CreateChangeset,

    /// Create a bookmark.
    CreateBookmark(BookmarkKind),

    /// Update a bookmark.
    UpdateBookmark(BookmarkKind),

    /// Delete a bookmark.
    DeleteBookmark(BookmarkKind),

    /// Land a stack to a bookmark (pushrebase and move bookmark)
    LandStack(BookmarkKind),

    /// Perform a megarepo sync
    MegarepoSync,
}

impl RepoWriteOperation {
    /// Returns true if this is an operation that only affects draft commits.
    fn is_draft(&self) -> bool {
        match self {
            RepoWriteOperation::CreateChangeset => true,
            RepoWriteOperation::CreateBookmark(kind)
            | RepoWriteOperation::UpdateBookmark(kind)
            | RepoWriteOperation::DeleteBookmark(kind)
            | RepoWriteOperation::LandStack(kind) => *kind == BookmarkKind::Scratch,
            RepoWriteOperation::MegarepoSync => false,
        }
    }

    /// Returns the name of the operation's method in the source control
    /// service write restrictions config.
    fn method_name(&self) -> &'static str {
        match self {
            RepoWriteOperation::CreateChangeset => "create_changeset",
            RepoWriteOperation::CreateBookmark(_) => "create_bookmark",
            RepoWriteOperation::UpdateBookmark(_) => "move_bookmark",
            RepoWriteOperation::DeleteBookmark(_) => "delete_bookmark",
            RepoWriteOperation::LandStack(_) => "land_stack",
            RepoWriteOperation::MegarepoSync => "megarepo_sync",
        }
    }
}

/// Outcome of an authorization check.
///
/// This enum is returned as the result of an authorization check, and must not
/// be ignored.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[must_use = "outcomes of authorization checks must not be ignored"]
pub enum AuthorizationCheckOutcome {
    Denied,
    Permitted,
}

impl AuthorizationCheckOutcome {
    fn from_permitted(permitted: bool) -> Self {
        if permitted {
            AuthorizationCheckOutcome::Permitted
        } else {
            AuthorizationCheckOutcome::Denied
        }
    }

    pub fn is_permitted(self) -> bool {
        self == AuthorizationCheckOutcome::Permitted
    }

    pub fn is_denied(self) -> bool {
        self == AuthorizationCheckOutcome::Denied
    }

    /// Return an error if the outcome of the check was that it is not permitted.
    pub fn permitted_or_else(
        self,
        err: impl Fn() -> AuthorizationError,
    ) -> Result<(), AuthorizationError> {
        match self {
            AuthorizationCheckOutcome::Permitted => Ok(()),
            AuthorizationCheckOutcome::Denied => Err(err()),
        }
    }
}
