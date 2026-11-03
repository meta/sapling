/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::convert::Into;

use anyhow::Error;
use scs_client_raw::thrift;
use source_control_clients::errors::CommitCommonBaseWithError;
use source_control_clients::errors::CommitCompareError;
use source_control_clients::errors::CommitDirectoryBranchClustersError;
use source_control_clients::errors::CommitFileDiffsError;
use source_control_clients::errors::CommitFilterAncestorsError;
use source_control_clients::errors::CommitFindFilesError;
use source_control_clients::errors::CommitFindFilesStreamError;
use source_control_clients::errors::CommitFindRestrictedPathsError;
use source_control_clients::errors::CommitFingerprintError;
use source_control_clients::errors::CommitGitMutationHistoryError;
use source_control_clients::errors::CommitHgMutationHistoryError;
use source_control_clients::errors::CommitHistoryError;
use source_control_clients::errors::CommitInfoError;
use source_control_clients::errors::CommitIsAncestorOfError;
use source_control_clients::errors::CommitLinearHistoryError;
use source_control_clients::errors::CommitListDescendantBookmarksError;
use source_control_clients::errors::CommitLookupError;
use source_control_clients::errors::CommitLookupPushrebaseHistoryError;
use source_control_clients::errors::CommitLookupXrepoError;
use source_control_clients::errors::CommitMultiplePathInfoError;
use source_control_clients::errors::CommitMultiplePathLastChangedError;
use source_control_clients::errors::CommitPathBlameError;
use source_control_clients::errors::CommitPathExistsError;
use source_control_clients::errors::CommitPathFirstChangedError;
use source_control_clients::errors::CommitPathHistoryError;
use source_control_clients::errors::CommitPathInfoError;
use source_control_clients::errors::CommitPathLastChangedError;
use source_control_clients::errors::CommitRateLimitCheckError;
use source_control_clients::errors::CommitRestrictedPathsAccessError;
use source_control_clients::errors::CommitRestrictedPathsChangesError;
use source_control_clients::errors::CommitRunHooksError;
use source_control_clients::errors::CommitSubtreeChangesError;
use source_control_clients::errors::FileContentChunkError;
use source_control_clients::errors::FileDiffError;
use source_control_clients::errors::FileExistsError;
use source_control_clients::errors::FileInfoError;
use source_control_clients::errors::ListReposError;
use source_control_clients::errors::RepoBookmarkInfoError;
use source_control_clients::errors::RepoCreateBookmarkError;
use source_control_clients::errors::RepoDeleteBookmarkError;
use source_control_clients::errors::RepoInfoError;
use source_control_clients::errors::RepoLandStackError;
use source_control_clients::errors::RepoListBookmarksError;
use source_control_clients::errors::RepoMoveBookmarkError;
use source_control_clients::errors::RepoMultipleCommitLookupError;
use source_control_clients::errors::RepoPrepareCommitsError;
use source_control_clients::errors::RepoRebaseStackError;
use source_control_clients::errors::RepoResolveBookmarkError;
use source_control_clients::errors::RepoResolveCommitPrefixError;
use source_control_clients::errors::RepoStackGitBundleStoreError;
use source_control_clients::errors::RepoUpdateSubmoduleExpansionError;
use source_control_clients::errors::TreeExistsError;
use source_control_clients::errors::TreeListError;

pub(crate) trait SelectionErrorExt {
    fn handle_selection_error(self, repo: &thrift::RepoSpecifier) -> Error;
}

/// Render a typed `RestrictedPathsAuthorizationError` into a user-facing
/// access-denied message.
fn format_restricted_paths_authz(e: &thrift::RestrictedPathsAuthorizationError) -> Error {
    let target = match &e.access {
        thrift::RestrictedPathAccess::path(p) => format!("restricted path: {p}"),
        thrift::RestrictedPathAccess::manifest_id(m) => format!("restricted manifest: {m}"),
        // Unknown union arm (forward-compat with a newer server): use the reason
        // as-is rather than the "Access denied to ..." template.
        _ => {
            return anyhow::anyhow!(
                "{}\nTo gain access, request access to: {}",
                e.reason,
                e.permission_request_group,
            );
        }
    };
    let denial_message = e
        .denial_message
        .as_deref()
        .filter(|message| !message.is_empty())
        .map_or(String::new(), |message| format!("\n{message}"));
    anyhow::anyhow!(
        "Access denied to {target}\nTo gain access, request access to: {group}{denial_message}",
        group = e.permission_request_group,
    )
}

// Return a friendly "repo does not exist" error if `self` is a ServiceRouter
// selection error for a nonexistent domain. Shared by both impl macros below.
// Only used by internal builds on Linux (other builds don't check selection
// errors).
#[cfg(all(fbcode_build, not(any(target_os = "macos", target_os = "windows"))))]
macro_rules! return_if_selection_error {
    ($self:ident, $type:ident, $repo:ident) => {
        if let $type::ThriftError(ref err) = $self {
            if let Some(err) = err.downcast_ref::<srclient::TServiceRouterException>() {
                if err.is_selection_error()
                    && err.error_reason() == srclient::ErrorReason::SELECTION_NONEXISTENT_DOMAIN
                {
                    return match $repo.name.strip_suffix(".git") {
                        Some(stripped) => anyhow::anyhow!(
                            "repo does not exist: {}. Try removing the .git suffix (i.e. -R {})",
                            $repo.name,
                            stripped
                        ),
                        None => anyhow::anyhow!("repo does not exist: {}", $repo.name),
                    };
                }
            }
        }
    };
}

macro_rules! impl_handle_selection_error {
    ($type: ident) => {
        impl SelectionErrorExt for $type {
            #[cfg(all(fbcode_build, not(any(target_os = "macos", target_os = "windows"))))]
            fn handle_selection_error(self, repo: &thrift::RepoSpecifier) -> Error {
                return_if_selection_error!(self, $type, repo);
                self.into()
            }

            #[cfg(any(not(fbcode_build), target_os = "macos", target_os = "windows"))]
            fn handle_selection_error(self, _repo: &thrift::RepoSpecifier) -> Error {
                self.into()
            }
        }
    };
}

// Like `impl_handle_selection_error!`, but renders the typed
// `restricted_paths_authorization_error` variant (subset methods only).
macro_rules! impl_handle_selection_error_with_restricted_paths {
    ($type: ident) => {
        impl SelectionErrorExt for $type {
            #[cfg(all(fbcode_build, not(any(target_os = "macos", target_os = "windows"))))]
            fn handle_selection_error(self, repo: &thrift::RepoSpecifier) -> Error {
                return_if_selection_error!(self, $type, repo);
                if let $type::restricted_paths_authorization_error(ref e) = self {
                    return format_restricted_paths_authz(e);
                }
                self.into()
            }

            #[cfg(any(not(fbcode_build), target_os = "macos", target_os = "windows"))]
            fn handle_selection_error(self, _repo: &thrift::RepoSpecifier) -> Error {
                if let $type::restricted_paths_authorization_error(ref e) = self {
                    return format_restricted_paths_authz(e);
                }
                self.into()
            }
        }
    };
}

impl_handle_selection_error!(CommitCommonBaseWithError);
impl_handle_selection_error_with_restricted_paths!(CommitCompareError);
impl_handle_selection_error!(CommitDirectoryBranchClustersError);
impl_handle_selection_error_with_restricted_paths!(CommitFileDiffsError);
impl_handle_selection_error!(CommitFilterAncestorsError);
impl_handle_selection_error!(CommitFindFilesError);
impl_handle_selection_error!(CommitFindFilesStreamError);
impl_handle_selection_error!(CommitFingerprintError);
impl_handle_selection_error!(CommitHistoryError);
impl_handle_selection_error!(CommitInfoError);
impl_handle_selection_error!(CommitIsAncestorOfError);
impl_handle_selection_error!(CommitLinearHistoryError);
impl_handle_selection_error!(CommitListDescendantBookmarksError);
impl_handle_selection_error!(CommitLookupError);
impl_handle_selection_error!(CommitLookupPushrebaseHistoryError);
impl_handle_selection_error!(CommitGitMutationHistoryError);
impl_handle_selection_error!(CommitHgMutationHistoryError);
impl_handle_selection_error!(CommitLookupXrepoError);
impl_handle_selection_error_with_restricted_paths!(CommitMultiplePathInfoError);
impl_handle_selection_error_with_restricted_paths!(CommitMultiplePathLastChangedError);
impl_handle_selection_error_with_restricted_paths!(CommitPathBlameError);
impl_handle_selection_error_with_restricted_paths!(CommitPathExistsError);
impl_handle_selection_error_with_restricted_paths!(CommitPathFirstChangedError);
impl_handle_selection_error_with_restricted_paths!(CommitPathHistoryError);
impl_handle_selection_error_with_restricted_paths!(CommitPathInfoError);
impl_handle_selection_error_with_restricted_paths!(CommitPathLastChangedError);
impl_handle_selection_error!(CommitRestrictedPathsAccessError);
impl_handle_selection_error!(CommitRestrictedPathsChangesError);
impl_handle_selection_error!(CommitFindRestrictedPathsError);
impl_handle_selection_error!(CommitRateLimitCheckError);
impl_handle_selection_error!(CommitRunHooksError);
impl_handle_selection_error!(CommitSubtreeChangesError);
impl_handle_selection_error_with_restricted_paths!(FileContentChunkError);
impl_handle_selection_error_with_restricted_paths!(FileDiffError);
impl_handle_selection_error_with_restricted_paths!(FileExistsError);
impl_handle_selection_error_with_restricted_paths!(FileInfoError);
impl_handle_selection_error!(ListReposError);
impl_handle_selection_error!(RepoBookmarkInfoError);
impl_handle_selection_error!(RepoCreateBookmarkError);
impl_handle_selection_error!(RepoDeleteBookmarkError);
impl_handle_selection_error!(RepoInfoError);
impl_handle_selection_error!(RepoLandStackError);
impl_handle_selection_error!(RepoListBookmarksError);
impl_handle_selection_error!(RepoMoveBookmarkError);
impl_handle_selection_error!(RepoMultipleCommitLookupError);
impl_handle_selection_error!(RepoPrepareCommitsError);
impl_handle_selection_error!(RepoRebaseStackError);
impl_handle_selection_error!(RepoResolveBookmarkError);
impl_handle_selection_error!(RepoResolveCommitPrefixError);
impl_handle_selection_error!(RepoStackGitBundleStoreError);
impl_handle_selection_error!(RepoUpdateSubmoduleExpansionError);
impl_handle_selection_error_with_restricted_paths!(TreeExistsError);
impl_handle_selection_error_with_restricted_paths!(TreeListError);
