/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use anyhow::Result;
use futures::future::BoxFuture;

/// A value whose loading is deferred until its contents are needed.
#[derive(Clone, derive_more::From)]
pub enum Deferred<T> {
    Ready(T),
    #[from(ignore)]
    Pending(Arc<dyn Fn() -> BoxFuture<'static, Result<T>> + Send + Sync>),
}

impl<T: fmt::Debug> fmt::Debug for Deferred<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready(value) => value.fmt(f),
            Self::Pending(_) => f.write_str("Deferred"),
        }
    }
}

impl<T: Send + 'static> Deferred<T> {
    pub fn new<F, Fut>(load: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        Self::Pending(Arc::new(move || Box::pin(load())))
    }

    pub async fn load(self) -> Result<T> {
        match self {
            Self::Ready(value) => Ok(value),
            Self::Pending(load) => load().await,
        }
    }

    pub fn map(self, map: impl Fn(T) -> T + Clone + Send + Sync + 'static) -> Self {
        match self {
            Self::Ready(value) => map(value).into(),
            Self::Pending(load) => Self::new(move || {
                let value = load();
                let map = map.clone();
                async move { Ok(map(value.await?)) }
            }),
        }
    }
}
