/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Error;
use diff::error as diff_feature;
use diff_service_if as thrift;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("Thrift Error: {0:#?}")]
    ThriftError(thrift::RequestErrorReason),

    #[error("Overloaded: {0}")]
    Overloaded(String),

    #[error("Repo not loaded: {0}")]
    RepoNotLoaded(String),

    #[error("Request Error: {0:#?}")]
    RequestError(Error),

    #[error("Internal Error: {0:#?}")]
    InternalError(#[from] Error),
}

macro_rules! impl_into_thrift_error {
    ($field: tt, $reason:tt) => {
        impl From<thrift::$reason> for ServerError {
            fn from(r: thrift::$reason) -> Self {
                ServerError::ThriftError(thrift::RequestErrorReason::$field(r))
            }
        }
    };
}

impl_into_thrift_error!(diff_error, DiffError);

impl From<diff_feature::DiffError> for ServerError {
    fn from(e: diff_feature::DiffError) -> Self {
        match e {
            diff_feature::DiffError::InvalidInput(msg) => {
                ServerError::RequestError(anyhow::anyhow!(msg))
            }
            diff_feature::DiffError::Internal(error) => ServerError::InternalError(error),
        }
    }
}

impl From<mononoke_api::MononokeError> for ServerError {
    fn from(e: mononoke_api::MononokeError) -> Self {
        use mononoke_api::MononokeError;
        match e {
            // These are user input errors and should be treated as request errors
            MononokeError::InvalidRequest(msg) => ServerError::RequestError(anyhow::anyhow!(msg)),
            MononokeError::AuthorizationError(msg) => {
                ServerError::RequestError(anyhow::anyhow!(msg))
            }
            MononokeError::RestrictedPathsAuthorizationError(err) => {
                ServerError::RequestError(anyhow::anyhow!(err.to_string()))
            }
            MononokeError::NotAvailable(msg) => ServerError::RequestError(anyhow::anyhow!(msg)),
            MononokeError::ManifestNotDerived(_) => ServerError::RequestError(anyhow::anyhow!(e)),
            MononokeError::LargeRepoNotFound(msg) => {
                ServerError::RequestError(anyhow::anyhow!(msg))
            }
            MononokeError::HookFailure(_) => ServerError::RequestError(anyhow::anyhow!(e)),
            MononokeError::PushrebaseConflicts(_) => ServerError::RequestError(anyhow::anyhow!(e)),
            MononokeError::NonFastForwardMove { .. } => {
                ServerError::RequestError(anyhow::anyhow!(e))
            }
            MononokeError::MergeConflicts { .. } => ServerError::RequestError(anyhow::anyhow!(e)),
            MononokeError::ServicePermissionDenied { .. } => {
                ServerError::RequestError(anyhow::anyhow!(e))
            }
            MononokeError::RedactionError { .. } => ServerError::RequestError(anyhow::anyhow!(e)),

            // Only modern_sync mirror moves produce this, and the diff service
            // never sends them. Treat an unexpected leak as an internal error.
            MononokeError::BookmarkMoveAlreadyProcessed => {
                ServerError::InternalError(Error::from(e))
            }

            // This is an internal system error
            MononokeError::InternalError(_) => ServerError::InternalError(Error::from(e)),
        }
    }
}
