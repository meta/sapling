/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use blobstore::Blobstore;
use blobstore::BlobstoreGetData;
use blobstore::BlobstoreIsPresent;
use blobstore::OverwriteStatus;
use blobstore::PutBehaviour;
use context::CoreContext;
use mononoke_types::BlobstoreBytes;
use mononoke_types::RepositoryId;
use prefixblob::PrefixBlobstore;

/// A blobstore that stores all keys which are not part of the commit graph of a repo.
/// That is, auxiliary data that is not part of the commit graph, such as
/// segmented changelog data.
/// It does not require redaction as the assumption is that blobs contained can be regenerated
/// excluding redacted content.
///
/// Making PrefixBlobstore part of every blobstore does two things:
/// 1. It ensures that the prefix applies first, which is important for shared caches like
///    memcache.
/// 2. It ensures that all possible blobrepos use a prefix.
type MutableRepoBlobstoreStack<T> = PrefixBlobstore<T>;

// NOTE: We parametize AbstractMutableRepoBlobstore over T instead of explicitly using Arc<dyn Blobstore>
// so that even if we were to add a blobstore to the MutableRepoBlobstoreStack that actually is a Arc<dyn
// Blobstore>, then we cannot accidentally forget to unwrap it below (since we wouldn't get a T
// back).
#[derive(Clone, Debug)]
pub struct AbstractMutableRepoBlobstore<T>(MutableRepoBlobstoreStack<T>);

impl<T: Blobstore + Clone> AbstractMutableRepoBlobstore<T> {
    pub fn as_parts(&self) -> (T, String) {
        let blobstore = self.0.clone();
        let prefix = blobstore.prefix();

        (blobstore.into_inner(), prefix)
    }
}

#[facet::facet]
#[derive(Clone, Debug)]
pub struct MutableRepoBlobstore(AbstractMutableRepoBlobstore<Arc<dyn Blobstore>>);

impl MutableRepoBlobstore {
    pub fn boxed(&self) -> Arc<dyn Blobstore> {
        self.0.as_parts().0
    }

    pub fn new(blobstore: Arc<dyn Blobstore>, repoid: RepositoryId) -> Self {
        Self::build(blobstore, repoid.prefix())
    }

    pub fn new_with_wrapped_inner_blobstore<F>(blobstore: MutableRepoBlobstore, wrapper: F) -> Self
    where
        F: FnOnce(Arc<dyn Blobstore>) -> Arc<dyn Blobstore>,
    {
        let (blobstore, prefix) = blobstore.0.as_parts();

        let new_inner_blobstore = wrapper(blobstore);
        Self::build(new_inner_blobstore, prefix)
    }

    #[allow(clippy::let_and_return)]
    fn build(blobstore: Arc<dyn Blobstore>, prefix: String) -> Self {
        let blobstore = PrefixBlobstore::new(blobstore, prefix);
        let blobstore = MutableRepoBlobstore(AbstractMutableRepoBlobstore(blobstore));
        blobstore
    }
}

impl std::fmt::Display for MutableRepoBlobstore {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "MutableRepoBlobstore<{}>", self.0.0)
    }
}

#[async_trait]
impl Blobstore for MutableRepoBlobstore {
    async fn get<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>> {
        self.0.0.get(ctx, key).await
    }

    async fn put_explicit<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
        put_behaviour: PutBehaviour,
    ) -> Result<OverwriteStatus> {
        self.0.0.put_explicit(ctx, key, value, put_behaviour).await
    }

    async fn put_with_status<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<OverwriteStatus> {
        self.0.0.put_with_status(ctx, key, value).await
    }

    async fn is_present<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<BlobstoreIsPresent> {
        self.0.0.is_present(ctx, key).await
    }

    async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()> {
        self.0.0.unlink(ctx, key).await
    }
}
