/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Error;
use bookmark_service_if::*;

#[derive(Debug, thiserror::Error)]
pub enum ThriftError {
    #[error("Request Error: {0:?}")]
    RequestError(RequestErrorReason),

    #[error("Internal Error: {0:?}")]
    InternalError(Error),
}

impl From<Error> for ThriftError {
    fn from(e: Error) -> Self {
        Self::InternalError(e)
    }
}

impl From<derived_data::DerivationError> for ThriftError {
    fn from(e: derived_data::DerivationError) -> Self {
        Self::InternalError(e.into())
    }
}

impl From<shared_error::anyhow::SharedError> for ThriftError {
    fn from(e: shared_error::anyhow::SharedError) -> Self {
        Self::InternalError(e.into())
    }
}

macro_rules! impl_into_thrift_error {
    ($field: tt, $reason:tt) => {
        impl From<$reason> for ThriftError {
            fn from(r: $reason) -> Self {
                ThriftError::RequestError(RequestErrorReason::$field(r))
            }
        }
    };
}

impl_into_thrift_error!(repo_not_found, RepoNotFound);
