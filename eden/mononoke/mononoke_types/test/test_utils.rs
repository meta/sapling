/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Result;
use async_trait::async_trait;
use blobstore::Blobstore;
use blobstore::BlobstoreBytes;
use blobstore::BlobstoreGetData;
use blobstore::OverwriteStatus;
use blobstore::PutBehaviour;
use context::CoreContext;
use fbinit::FacebookInit;
use memblob::Memblob;
use test_repo_factory::TestRepoFactory;
use tests_utils::BasicTestRepo;

pub(crate) async fn test_repo(fb: FacebookInit) -> Result<(Arc<BasicTestRepo>, Memblob)> {
    let (repo, blobstore, _reads) = test_repo_counting_reads(fb).await?;
    Ok((repo, blobstore))
}

/// A test repo, its blobstore, and the number of reads its blobstore has
/// served.
pub(crate) async fn test_repo_counting_reads(
    fb: FacebookInit,
) -> Result<(Arc<BasicTestRepo>, Memblob, Arc<AtomicUsize>)> {
    let blobstore = Memblob::default();
    let reads = Arc::new(AtomicUsize::new(0));
    let repo: BasicTestRepo = TestRepoFactory::new(fb)?
        .with_blobstore(Arc::new(ReadCountingBlobstore {
            inner: blobstore.clone(),
            reads: reads.clone(),
        }))
        .build()
        .await?;
    Ok((Arc::new(repo), blobstore, reads))
}

/// Counts the reads that reach `inner`.
#[derive(Debug)]
struct ReadCountingBlobstore {
    inner: Memblob,
    reads: Arc<AtomicUsize>,
}

impl fmt::Display for ReadCountingBlobstore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ReadCountingBlobstore<{}>", self.inner)
    }
}

#[async_trait]
impl Blobstore for ReadCountingBlobstore {
    async fn get<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
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

    async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()> {
        Blobstore::unlink(&self.inner, ctx, key).await
    }
}
