/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![cfg_attr(fbcode_build, feature(error_generic_member_access))]

use std::backtrace::BacktraceStatus;
use std::convert::Infallible;
use std::error::Error as StdError;
use std::sync::Arc;

use anyhow::Error;
use blobstore::LoadableError;
use derived_data_manager::SharedDerivationError;
use source_control as scs_thrift;
use thiserror::Error;

pub mod macro_reexport {
    pub use anyhow::anyhow;
}
// The cargo build of anyhow disables its backtrace features when using RUSTC_BOOTSTRAP=1
#[cfg(not(fbcode_build))]
pub static DISABLED: std::backtrace::Backtrace = std::backtrace::Backtrace::disabled();

#[macro_export]
macro_rules! cloneable_error {
    ($name: ident) => {
        #[derive(Clone, Debug)]
        pub struct $name(pub ::std::sync::Arc<anyhow::Error>);

        impl $name {
            #[cfg(fbcode_build)]
            pub fn backtrace(&self) -> &::std::backtrace::Backtrace {
                self.0.backtrace()
            }

            #[cfg(not(fbcode_build))]
            pub fn backtrace(&self) -> &::std::backtrace::Backtrace {
                &$crate::DISABLED
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl From<anyhow::Error> for $name {
            fn from(error: anyhow::Error) -> Self {
                Self(::std::sync::Arc::new(error))
            }
        }

        impl ::std::error::Error for $name {
            fn source(&self) -> Option<&(dyn ::std::error::Error + 'static)> {
                Some(&**self.0)
            }

            #[cfg(fbcode_build)]
            fn provide<'a>(&'a self, request: &mut ::std::error::Request<'a>) {
                request.provide_ref::<::std::backtrace::Backtrace>(self.backtrace());
            }
        }
    };
}

cloneable_error!(InternalError);
cloneable_error!(RequestError);

#[derive(Clone, Debug, Error)]
pub enum MegarepoError {
    #[error("{0}")]
    RequestError(#[source] RequestError),
    #[error("{0}")]
    InternalError(#[source] InternalError),
}

impl MegarepoError {
    pub fn internal(e: impl Into<Error>) -> Self {
        Self::InternalError(InternalError::from(e.into()))
    }

    pub fn request(e: impl Into<Error>) -> Self {
        Self::RequestError(RequestError::from(e.into()))
    }
}

/// By default, let's treat errors as internal
impl From<Error> for MegarepoError {
    fn from(e: Error) -> Self {
        match e.downcast::<MegarepoError>() {
            Ok(megarepo_error) => match megarepo_error {
                Self::RequestError(e) => Self::RequestError(e),
                Self::InternalError(e) => Self::InternalError(e),
            },
            Err(orig) => Self::internal(orig),
        }
    }
}

impl From<Infallible> for MegarepoError {
    fn from(_i: Infallible) -> Self {
        unreachable!()
    }
}

impl From<LoadableError> for MegarepoError {
    fn from(e: LoadableError) -> Self {
        MegarepoError::InternalError(InternalError(Arc::new(e.into())))
    }
}

impl From<SharedDerivationError> for MegarepoError {
    fn from(e: SharedDerivationError) -> Self {
        MegarepoError::InternalError(InternalError(Arc::new(e.into())))
    }
}

#[macro_export]
macro_rules! bail_request {
    ($msg:literal $(,)?) => {
        return Err($crate::MegarepoError::RequestError($crate::RequestError(::std::sync::Arc::new($crate::macro_reexport::anyhow!($msg)))))
    };
    ($fmt:expr, $($arg:tt)*) => {
        return Err($crate::MegarepoError::RequestError($crate::RequestError(::std::sync::Arc::new($crate::macro_reexport::anyhow!($fmt, $($arg)*)))))
    };
}

#[macro_export]
macro_rules! bail_internal {
    ($msg:literal $(,)?) => {
        return Err($crate::MegarepoError::InternalError($crate::InternalError(::std::sync::Arc::new($crate::macro_reexport::anyhow!($msg)))))
    };
    ($fmt:expr, $($arg:tt)*) => {
        return Err($crate::MegarepoError::InternalError($crate::InternalError(::std::sync::Arc::new($crate::macro_reexport::anyhow!($fmt, $($arg)*)))))
    };
}

impl From<MegarepoError> for scs_thrift::MegarepoAsynchronousRequestError {
    fn from(e: MegarepoError) -> Self {
        match e {
            MegarepoError::RequestError(e) => Self::request_error(scs_thrift::RequestErrorStruct {
                kind: scs_thrift::RequestErrorKind::INVALID_REQUEST,
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

                Self::internal_error(scs_thrift::InternalErrorStruct {
                    reason,
                    backtrace,
                    source_chain,
                    ..Default::default()
                })
            }
        }
    }
}
