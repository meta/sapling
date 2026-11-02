/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::backtrace::BacktraceStatus;
use std::error::Error as StdError;

use async_requests::AsyncRequestsError;
use derived_data_manager::DerivationError;
use git_types::GitError;
use megarepo_error::MegarepoError;
use mononoke_api::MononokeError;
use restricted_paths::RestrictedPathAccess;
use source_control as thrift;
use source_control_services::errors::source_control_service as service;

#[derive(Debug)]
pub enum ServiceError {
    Request(thrift::RequestError),
    Internal(thrift::InternalError),
    Overload(thrift::OverloadError),
    Poll(thrift::PollError),
    RestrictedPathsAuthorization(thrift::RestrictedPathsAuthorizationError),
}

impl From<thrift::RequestError> for ServiceError {
    fn from(e: thrift::RequestError) -> Self {
        Self::Request(e)
    }
}

impl From<thrift::InternalError> for ServiceError {
    fn from(e: thrift::InternalError) -> Self {
        Self::Internal(e)
    }
}

impl From<thrift::OverloadError> for ServiceError {
    fn from(e: thrift::OverloadError) -> Self {
        Self::Overload(e)
    }
}

impl From<thrift::PollError> for ServiceError {
    fn from(e: thrift::PollError) -> Self {
        Self::Poll(e)
    }
}

#[derive(Clone, Copy)]
pub enum Status {
    RequestError,
    InternalError,
    OverloadError,
    PollError,
}

/// Error can be logged to SCS scuba table
pub trait LoggableError {
    fn status_and_description(&self) -> (Status, String);
}

impl LoggableError for ServiceError {
    fn status_and_description(&self) -> (Status, String) {
        match self {
            Self::Request(err) => (Status::RequestError, format!("{err:?}")),
            Self::Internal(err) => (Status::InternalError, format!("{err:?}")),
            Self::Overload(err) => (Status::OverloadError, format!("{err:?}")),
            Self::Poll(err) => (Status::PollError, format!("{err:?}")),
            // A denied request is a client-fault, logged as a request-class error.
            Self::RestrictedPathsAuthorization(err) => (Status::RequestError, format!("{err:?}")),
        }
    }
}

impl ServiceError {
    pub fn context(self, context: &str) -> Self {
        match self {
            Self::Request(thrift::RequestError { kind, reason, .. }) => {
                let reason = format!("{context}: {reason}");
                Self::Request(thrift::RequestError {
                    kind,
                    reason,
                    ..Default::default()
                })
            }
            Self::Internal(thrift::InternalError {
                reason,
                backtrace,
                source_chain,
                ..
            }) => {
                let reason = format!("{context}: {reason}");
                Self::Internal(thrift::InternalError {
                    reason,
                    backtrace,
                    source_chain,
                    ..Default::default()
                })
            }
            Self::Overload(thrift::OverloadError { reason, .. }) => {
                let reason = format!("{context}: {reason}");
                Self::Overload(thrift::OverloadError {
                    reason,
                    ..Default::default()
                })
            }
            Self::Poll(thrift::PollError { reason, .. }) => {
                let reason = format!("{context}: {reason}");
                Self::Poll(thrift::PollError {
                    reason,
                    ..Default::default()
                })
            }
            Self::RestrictedPathsAuthorization(thrift::RestrictedPathsAuthorizationError {
                reason,
                access,
                permission_request_group,
                denial_message,
                ..
            }) => {
                let reason = format!("{context}: {reason}");
                Self::RestrictedPathsAuthorization(thrift::RestrictedPathsAuthorizationError {
                    reason,
                    access,
                    permission_request_group,
                    denial_message,
                    ..Default::default()
                })
            }
        }
    }

    pub fn repo_not_found(&self) -> bool {
        match self {
            Self::Request(thrift::RequestError {
                kind: thrift::RequestErrorKind::REPO_NOT_FOUND,
                ..
            }) => true,
            Self::Request(thrift::RequestError {
                kind: thrift::RequestErrorKind::LARGE_REPO_NOT_FOUND,
                ..
            }) => true,
            _ => false,
        }
    }
}

pub trait ServiceErrorResultExt<T> {
    fn context(self, context: &str) -> Result<T, ServiceError>;
    fn with_context(self, context_fn: impl FnOnce() -> String) -> Result<T, ServiceError>;
}

impl<T, E> ServiceErrorResultExt<T> for Result<T, E>
where
    E: Into<ServiceError>,
{
    fn context(self, context: &str) -> Result<T, ServiceError> {
        match self {
            Ok(v) => Ok(v),
            Err(e) => Err(e.into().context(context)),
        }
    }

    fn with_context(self, context_fn: impl FnOnce() -> String) -> Result<T, ServiceError> {
        match self {
            Ok(v) => Ok(v),
            Err(e) => Err(e.into().context(context_fn().as_str())),
        }
    }
}

impl From<MegarepoError> for ServiceError {
    fn from(e: MegarepoError) -> Self {
        match e {
            MegarepoError::RequestError(e) => Self::Request(thrift::RequestError {
                kind: thrift::RequestErrorKind::INVALID_REQUEST,
                reason: format!("{e}"),
                ..Default::default()
            }),
            MegarepoError::InternalError(error) => {
                let reason = error.to_string();
                let backtrace = match error.backtrace().status() {
                    BacktraceStatus::Captured => Some(error.backtrace().to_string()),
                    _ => None,
                };
                let mut source_chain = Vec::new();
                let mut error: &dyn StdError = &error;
                while let Some(source) = error.source() {
                    source_chain.push(source.to_string());
                    error = source;
                }

                Self::Internal(thrift::InternalError {
                    reason,
                    backtrace,
                    source_chain,
                    ..Default::default()
                })
            }
        }
    }
}

impl From<AsyncRequestsError> for ServiceError {
    fn from(e: AsyncRequestsError) -> Self {
        match e {
            AsyncRequestsError::RequestError(e) => Self::Request(thrift::RequestError {
                kind: thrift::RequestErrorKind::INVALID_REQUEST,
                reason: format!("{e}"),
                ..Default::default()
            }),
            AsyncRequestsError::InternalError(error) => {
                let reason = error.to_string();
                let backtrace = match error.backtrace().status() {
                    BacktraceStatus::Captured => Some(error.backtrace().to_string()),
                    _ => None,
                };
                let mut source_chain = Vec::new();
                let mut error: &dyn StdError = &error;
                while let Some(source) = error.source() {
                    source_chain.push(source.to_string());
                    error = source;
                }

                Self::Internal(thrift::InternalError {
                    reason,
                    backtrace,
                    source_chain,
                    ..Default::default()
                })
            }
        }
    }
}

impl From<ServiceError> for AsyncRequestsError {
    fn from(e: ServiceError) -> Self {
        match e {
            ServiceError::Request(e) => Self::request(e),
            ServiceError::Internal(e) => Self::internal(e),
            ServiceError::Overload(e) => Self::internal(e),
            ServiceError::Poll(e) => Self::internal(e), // FIXME
            // The async-requests error union has no typed authz arm, so flatten
            // to RequestError{PERMISSION_DENIED}.
            ServiceError::RestrictedPathsAuthorization(e) => {
                Self::request(restricted_paths_authz_to_request_error(e))
            }
        }
    }
}

impl From<MononokeError> for ServiceError {
    fn from(e: MononokeError) -> Self {
        match e {
            MononokeError::InvalidRequest(reason) => Self::Request(thrift::RequestError {
                kind: thrift::RequestErrorKind::INVALID_REQUEST,
                reason,
                ..Default::default()
            }),
            error @ MononokeError::MergeConflicts { .. } => Self::Request(thrift::RequestError {
                kind: thrift::RequestErrorKind::MERGE_CONFLICTS,
                reason: error.to_string(),
                ..Default::default()
            }),
            error @ MononokeError::LargeRepoNotFound(_) => Self::Request(thrift::RequestError {
                kind: thrift::RequestErrorKind::LARGE_REPO_NOT_FOUND,
                reason: error.to_string(),
                ..Default::default()
            }),
            error @ MononokeError::ServicePermissionDenied { .. } => {
                Self::Request(thrift::RequestError {
                    kind: thrift::RequestErrorKind::PERMISSION_DENIED,
                    reason: error.to_string(),
                    ..Default::default()
                })
            }
            error @ MononokeError::AuthorizationError(_) => Self::Request(thrift::RequestError {
                kind: thrift::RequestErrorKind::PERMISSION_DENIED,
                reason: error.to_string(),
                ..Default::default()
            }),
            MononokeError::RestrictedPathsAuthorizationError(err) => {
                let reason = err.to_string();
                let access = match err.access() {
                    RestrictedPathAccess::Path(path) => {
                        thrift::RestrictedPathAccess::path(path.to_string())
                    }
                    RestrictedPathAccess::Manifest(manifest_id) => {
                        thrift::RestrictedPathAccess::manifest_id(manifest_id.to_string())
                    }
                };
                let permission_request_group = err.permission_request_group().id_data().to_string();
                Self::RestrictedPathsAuthorization(thrift::RestrictedPathsAuthorizationError {
                    reason,
                    access,
                    permission_request_group,
                    denial_message: err.denial_message().map(str::to_string),
                    ..Default::default()
                })
            }
            error @ MononokeError::NotAvailable(_) => Self::Request(thrift::RequestError {
                kind: thrift::RequestErrorKind::NOT_AVAILABLE,
                reason: error.to_string(),
                ..Default::default()
            }),
            error @ MononokeError::ManifestNotDerived(_) => Self::Request(thrift::RequestError {
                kind: thrift::RequestErrorKind::MANIFEST_NOT_DERIVED,
                reason: error.to_string(),
                ..Default::default()
            }),
            error @ MononokeError::HookFailure(_) => Self::Request(thrift::RequestError {
                kind: thrift::RequestErrorKind::INVALID_REQUEST,
                reason: error.to_string(),
                ..Default::default()
            }),
            error @ MononokeError::NonFastForwardMove { .. } => {
                Self::Request(thrift::RequestError {
                    kind: thrift::RequestErrorKind::INVALID_REQUEST,
                    reason: error.to_string(),
                    ..Default::default()
                })
            }
            error @ MononokeError::PushrebaseConflicts(_) => Self::Request(thrift::RequestError {
                kind: thrift::RequestErrorKind::INVALID_REQUEST,
                reason: error.to_string(),
                ..Default::default()
            }),
            error @ MononokeError::RedactionError { key: _, reason: _ } => {
                Self::Request(thrift::RequestError {
                    kind: thrift::RequestErrorKind::REDACTED,
                    reason: error.to_string(),
                    ..Default::default()
                })
            }
            error @ MononokeError::BookmarkMoveAlreadyProcessed => {
                // Only modern_sync mirror moves produce this, and SCS never
                // sends them. Treat an unexpected leak as an internal error.
                Self::Internal(internal_error(error))
            }
            MononokeError::InternalError(error) => {
                let reason = format!("{error:#}");
                let backtrace = match error.backtrace().status() {
                    BacktraceStatus::Captured => Some(error.backtrace().to_string()),
                    _ => None,
                };
                let mut source_chain = Vec::new();
                let mut error: &dyn StdError = &error;
                while let Some(source) = error.source() {
                    source_chain.push(source.to_string());
                    error = source;
                }
                Self::Internal(thrift::InternalError {
                    reason,
                    backtrace,
                    source_chain,
                    ..Default::default()
                })
            }
        }
    }
}

impl From<DerivationError> for ServiceError {
    fn from(e: DerivationError) -> Self {
        let mononoke_error: MononokeError = e.into();
        mononoke_error.into()
    }
}

macro_rules! impl_into_thrift_error {
    // Shared body. Only the Poll and authz arms differ between the forms below.
    (@impl $t:ty, |$pe:ident| $poll:expr, |$ae:ident| $authz:expr) => {
        impl From<ServiceError> for $t {
            fn from(e: ServiceError) -> Self {
                match e {
                    ServiceError::Request(e) => e.into(),
                    ServiceError::Internal(e) => e.into(),
                    ServiceError::Overload(e) => e.into(),
                    ServiceError::Poll($pe) => $poll,
                    ServiceError::RestrictedPathsAuthorization($ae) => $authz,
                }
            }
        }
    };

    // restricted_paths form: the method declared `RestrictedPathsAuthorizationError`
    // in its thrift `throws`, so its generated `*Exn` exposes a named-variant
    // constructor (generated `*Exn` types have no blanket `From<exception>`). Using
    // it makes registering a non-subset method with the `restricted_paths` form a
    // compile error. Precedent: `RepoLandStackExn::hook_rejections(...)`.
    (restricted_paths $t:ty) => {
        impl_into_thrift_error!(@impl $t,
            |e| internal_error(format!("poll error: {e}")).into(), // shouldn't happen
            |e| <$t>::restricted_paths_authorization_error(e));
    };

    // new-style poll methods can return a Poll error
    (poll $t:ty) => {
        impl_into_thrift_error!(@impl $t,
            |e| e.into(),
            |e| restricted_paths_authz_to_request_error(e).into());
    };

    // Old-style poll methods can't distinguish between a Poll error and an Internal error, so let's do our best.
    // This also works just fine for non-poll methods that won't be returning `ServiceError::Poll` anyway.
    ($t:ty) => {
        impl_into_thrift_error!(@impl $t,
            |e| internal_error(format!("poll error: {e}")).into(), // shouldn't happen
            |e| restricted_paths_authz_to_request_error(e).into());
    };
}

impl_into_thrift_error!(service::ListReposExn);
impl_into_thrift_error!(service::RepoExistsExn);
impl_into_thrift_error!(service::StorageConfigExistsExn);
impl_into_thrift_error!(service::CreateOrgStorageConfigExn);
impl_into_thrift_error!(service::DeleteOrgStorageConfigExn);
impl_into_thrift_error!(service::OrgStorageConfigPollExn);
impl_into_thrift_error!(service::OrgStorageConfigExn);
impl_into_thrift_error!(service::RepoInfoExn);
impl_into_thrift_error!(service::GitRepoStateExn);
impl_into_thrift_error!(service::RepoResolveBookmarkExn);
impl_into_thrift_error!(service::RepoResolveCommitPrefixExn);
impl_into_thrift_error!(service::RepoListBookmarksExn);
impl_into_thrift_error!(service::RepoCreateCommitExn);
impl_into_thrift_error!(service::RepoFoldCommitsExn);
impl_into_thrift_error!(service::RepoCreateStackExn);
impl_into_thrift_error!(service::RepoCreateBookmarkExn);
impl_into_thrift_error!(service::RepoMoveBookmarkExn);
impl_into_thrift_error!(service::RepoMultipleCommitLookupExn);
impl_into_thrift_error!(service::RepoDeleteBookmarkExn);
impl_into_thrift_error!(service::RepoLandStackExn);
impl_into_thrift_error!(service::RepoRebaseStackExn);
impl_into_thrift_error!(service::RepoBookmarkInfoExn);
impl_into_thrift_error!(service::RepoStackInfoExn);
impl_into_thrift_error!(service::RepoStackGitBundleStoreExn);
impl_into_thrift_error!(service::RepoPrepareCommitsExn);
impl_into_thrift_error!(service::RepoUploadFileContentExn);
impl_into_thrift_error!(service::CommitCommonBaseWithExn);
impl_into_thrift_error!(restricted_paths service::CommitFileDiffsExn);
impl_into_thrift_error!(service::CommitLookupExn);
impl_into_thrift_error!(service::CommitLookupPushrebaseHistoryExn);
impl_into_thrift_error!(service::CommitInfoExn);
impl_into_thrift_error!(service::CommitGenerationExn);
impl_into_thrift_error!(service::CommitFingerprintExn);
impl_into_thrift_error!(restricted_paths service::CommitCompareExn);
impl_into_thrift_error!(service::CommitIsAncestorOfExn);
impl_into_thrift_error!(service::CommitFilterAncestorsExn);
impl_into_thrift_error!(service::CommitIsPublicExn);
impl_into_thrift_error!(service::CommitFindFilesExn);
impl_into_thrift_error!(service::CommitFindFilesStreamExn);
impl_into_thrift_error!(service::CommitFindFilesStreamStreamExn);
impl_into_thrift_error!(service::CommitHistoryExn);
impl_into_thrift_error!(service::CommitHgMutationHistoryExn);
impl_into_thrift_error!(service::CommitGitMutationHistoryExn);
impl_into_thrift_error!(service::CommitDirectoryBranchClustersExn);
impl_into_thrift_error!(service::CommitRestrictedPathsAccessExn);
impl_into_thrift_error!(service::CommitFindRestrictedPathsExn);
impl_into_thrift_error!(service::CommitFindRestrictedPathsStreamExn);
impl_into_thrift_error!(service::CommitRestrictedPathsChangesExn);
impl_into_thrift_error!(service::CommitChangedPathsApproxExn);
impl_into_thrift_error!(service::CommitLinearHistoryExn);
impl_into_thrift_error!(service::CommitListDescendantBookmarksExn);
impl_into_thrift_error!(service::CommitRunHooksExn);
impl_into_thrift_error!(service::CommitRateLimitCheckExn);
impl_into_thrift_error!(service::CommitSubtreeChangesExn);
impl_into_thrift_error!(restricted_paths service::CommitPathExistsExn);
impl_into_thrift_error!(restricted_paths service::CommitPathInfoExn);
impl_into_thrift_error!(restricted_paths service::CommitMultiplePathInfoExn);
impl_into_thrift_error!(restricted_paths service::CommitPathBlameExn);
impl_into_thrift_error!(restricted_paths service::CommitPathHistoryExn);
impl_into_thrift_error!(restricted_paths service::CommitPathLastChangedExn);
impl_into_thrift_error!(restricted_paths service::CommitPathFirstChangedExn);
impl_into_thrift_error!(restricted_paths service::CommitMultiplePathLastChangedExn);
impl_into_thrift_error!(service::CommitSparseProfileDeltaAsyncExn);
impl_into_thrift_error!(poll service::CommitSparseProfileDeltaPollExn);
impl_into_thrift_error!(service::CommitSparseProfileSizeAsyncExn);
impl_into_thrift_error!(poll service::CommitSparseProfileSizePollExn);
impl_into_thrift_error!(restricted_paths service::TreeExistsExn);
impl_into_thrift_error!(restricted_paths service::TreeListExn);
impl_into_thrift_error!(restricted_paths service::FileExistsExn);
impl_into_thrift_error!(restricted_paths service::FileInfoExn);
impl_into_thrift_error!(restricted_paths service::FileContentChunkExn);
impl_into_thrift_error!(restricted_paths service::FileDiffExn);
impl_into_thrift_error!(service::CommitLookupXrepoExn);
impl_into_thrift_error!(service::CreateReposExn);
impl_into_thrift_error!(service::CreateReposPollExn);
impl_into_thrift_error!(service::MegarepoAddSyncTargetConfigExn);
impl_into_thrift_error!(service::MegarepoReadTargetConfigExn);
impl_into_thrift_error!(service::MegarepoAddSyncTargetExn);
impl_into_thrift_error!(service::MegarepoAddSyncTargetPollExn);
impl_into_thrift_error!(service::MegarepoAddBranchingSyncTargetExn);
impl_into_thrift_error!(service::MegarepoAddBranchingSyncTargetPollExn);
impl_into_thrift_error!(service::MegarepoChangeTargetConfigExn);
impl_into_thrift_error!(service::MegarepoChangeTargetConfigPollExn);
impl_into_thrift_error!(service::MegarepoSyncChangesetExn);
impl_into_thrift_error!(service::MegarepoSyncChangesetPollExn);
impl_into_thrift_error!(service::MegarepoRemergeSourceExn);
impl_into_thrift_error!(service::MegarepoRemergeSourcePollExn);
impl_into_thrift_error!(service::RepoUpdateSubmoduleExpansionExn);
impl_into_thrift_error!(service::RepoUploadNonBlobGitObjectExn);
impl_into_thrift_error!(service::CreateGitTreeExn);
impl_into_thrift_error!(service::CreateGitTagExn);
impl_into_thrift_error!(service::RepoUploadPackfileBaseItemExn);
impl_into_thrift_error!(service::RepoTagInfoExn);
impl_into_thrift_error!(service::CloudWorkspaceInfoExn);
impl_into_thrift_error!(service::CloudUserWorkspacesExn);
impl_into_thrift_error!(service::CloudWorkspaceSmartlogExn);

pub fn invalid_request(reason: impl ToString) -> thrift::RequestError {
    thrift::RequestError {
        kind: thrift::RequestErrorKind::INVALID_REQUEST,
        reason: reason.to_string(),
        ..Default::default()
    }
}

/// Flatten a typed authorization denial into a `RequestError{PERMISSION_DENIED}`.
///
/// Used by methods that do not declare `RestrictedPathsAuthorizationError` in
/// their thrift `throws` (non-subset and poll methods), preserving the
/// pre-existing flattened behavior.
fn restricted_paths_authz_to_request_error(
    e: thrift::RestrictedPathsAuthorizationError,
) -> thrift::RequestError {
    thrift::RequestError {
        kind: thrift::RequestErrorKind::PERMISSION_DENIED,
        reason: e.reason,
        ..Default::default()
    }
}

pub fn internal_error(error: impl ToString) -> thrift::InternalError {
    thrift::InternalError {
        reason: error.to_string(),
        backtrace: None,
        source_chain: Vec::new(),
        ..Default::default()
    }
}

pub fn repo_not_found(repo: String) -> thrift::RequestError {
    thrift::RequestError {
        kind: thrift::RequestErrorKind::REPO_NOT_FOUND,
        reason: format!("repo not found ({repo})"),
        ..Default::default()
    }
}

pub fn commit_not_found(commit: String) -> thrift::RequestError {
    thrift::RequestError {
        kind: thrift::RequestErrorKind::COMMIT_NOT_FOUND,
        reason: format!("commit not found ({commit})"),
        ..Default::default()
    }
}

pub fn file_not_found(file: String) -> thrift::RequestError {
    thrift::RequestError {
        kind: thrift::RequestErrorKind::FILE_NOT_FOUND,
        reason: format!("file not found ({file})"),
        ..Default::default()
    }
}

pub fn tree_not_found(tree: String) -> thrift::RequestError {
    thrift::RequestError {
        kind: thrift::RequestErrorKind::TREE_NOT_FOUND,
        reason: format!("tree not found ({tree})"),
        ..Default::default()
    }
}

pub fn limit_too_low(limit: usize) -> thrift::RequestError {
    thrift::RequestError {
        kind: thrift::RequestErrorKind::INVALID_REQUEST,
        reason: format!(
            "the limit param value of {limit} is not enough for the method to make any progress",
        ),
        ..Default::default()
    }
}

pub fn diff_input_too_big(total_size: u64) -> thrift::RequestError {
    thrift::RequestError {
        kind: thrift::RequestErrorKind::INVALID_REQUEST_INPUT_TOO_BIG,
        reason: format!(
            "only {} bytes of files (in total) can be diffed in one request, you asked for {} bytes",
            thrift::consts::COMMIT_FILE_DIFFS_SIZE_LIMIT,
            total_size,
        ),
        ..Default::default()
    }
}

pub fn diff_input_too_many_paths(path_count: usize) -> thrift::RequestError {
    thrift::RequestError {
        kind: thrift::RequestErrorKind::INVALID_REQUEST_TOO_MANY_PATHS,
        reason: format!(
            "only at most {} paths can be diffed in one request, you asked for {}",
            thrift::consts::COMMIT_FILE_DIFFS_PATH_COUNT_LIMIT,
            path_count,
        ),
        ..Default::default()
    }
}

pub fn not_available(reason: String) -> thrift::RequestError {
    thrift::RequestError {
        kind: thrift::RequestErrorKind::NOT_AVAILABLE,
        reason,
        ..Default::default()
    }
}

pub fn not_implemented(reason: String) -> thrift::RequestError {
    thrift::RequestError {
        kind: thrift::RequestErrorKind::NOT_IMPLEMENTED,
        reason,
        ..Default::default()
    }
}

pub fn overloaded(reason: String) -> thrift::OverloadError {
    thrift::OverloadError {
        reason,
        ..Default::default()
    }
}

pub fn poll_error(error: impl ToString) -> thrift::PollError {
    thrift::PollError {
        reason: error.to_string(),
        ..Default::default()
    }
}

impl From<GitError> for ServiceError {
    fn from(error: GitError) -> Self {
        match error {
            // Storage failure is a internal error with system generated error message.
            // Convert it into thrift::InternalError before sending across service boundary.
            GitError::StorageFailure(reason, error) => {
                let backtrace = match error.backtrace().status() {
                    BacktraceStatus::Captured => Some(error.backtrace().to_string()),
                    _ => None,
                };
                let mut source_chain = Vec::new();
                let mut error: &dyn StdError = &error;
                while let Some(source) = error.source() {
                    source_chain.push(source.to_string());
                    error = source;
                }
                Self::Internal(thrift::InternalError {
                    reason,
                    backtrace,
                    source_chain,
                    ..Default::default()
                })
            }
            // All other kind of errors associated with git operations are a result of
            // invalid / bad user input. Categorize them under INVALID_REQUEST and include
            // the actual cause in the reason string.
            _ => Self::Request(thrift::RequestError {
                kind: thrift::RequestErrorKind::INVALID_REQUEST,
                reason: error.to_string(),
                ..Default::default()
            }),
        }
    }
}
