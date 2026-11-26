/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Result;
use anyhow::bail;
use async_trait::async_trait;
use blobstore::Blobstore;
use blobstore::BlobstoreGetData;
use blobstore::BlobstoreIsPresent;
use blobstore::OverwriteStatus;
use blobstore::PutBehaviour;
use context::CoreContext;
use mononoke_types::BlobstoreBytes;

#[derive(Debug)]
pub struct TracingBlobstore<T> {
    inner: T,
    gets: Arc<Mutex<Vec<String>>>,
    failing: Arc<Mutex<HashSet<String>>>,
}

impl<T: std::fmt::Display> std::fmt::Display for TracingBlobstore<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "TracingBlobstore<{}>", self.inner)
    }
}

impl<T> TracingBlobstore<T> {
    pub fn new(inner: T) -> Self {
        let gets = Arc::new(Mutex::new(vec![]));
        Self {
            inner,
            gets,
            failing: Default::default(),
        }
    }
}

impl<T> TracingBlobstore<T> {
    pub fn tracing_gets(&self) -> Vec<String> {
        let mut gets = self.gets.lock().expect("poisoned lock");
        std::mem::take(&mut *gets)
    }

    /// Makes the next read of a key ending in `key` (the repo blobstore adds
    /// a prefix) fail with a storage error, not a missing blob.
    pub fn fail_next_get(&self, key: String) {
        self.failing.lock().expect("poisoned lock").insert(key);
    }
}

#[async_trait]
impl<T: Blobstore> Blobstore for TracingBlobstore<T> {
    async fn get<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>> {
        self.gets
            .lock()
            .expect("poisoned lock")
            .push(key.to_owned());
        {
            let mut failing = self.failing.lock().expect("poisoned lock");
            if let Some(suffix) = failing
                .iter()
                .find(|suffix| key.ends_with(*suffix))
                .cloned()
            {
                failing.remove(&suffix);
                bail!("reading {suffix} failed");
            }
        }
        self.inner.get(ctx, key).await
    }

    async fn put_explicit<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
        put_behaviour: PutBehaviour,
    ) -> Result<OverwriteStatus> {
        self.inner
            .put_explicit(ctx, key, value, put_behaviour)
            .await
    }

    async fn put_with_status<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<OverwriteStatus> {
        self.inner.put_with_status(ctx, key, value).await
    }

    async fn is_present<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<BlobstoreIsPresent> {
        self.inner.is_present(ctx, key).await
    }

    async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()> {
        self.inner.unlink(ctx, key).await
    }
}
