/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Helpers for unit tests of the per-commit storage audit families.

use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Poll;

use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use blobstore::Blobstore;
use blobstore::BlobstoreBytes;
use blobstore::BlobstoreGetData;
use blobstore::OverwriteStatus;
use blobstore::PutBehaviour;
use commit_graph::CommitGraphRef;
use context::CoreContext;
use futures::TryStreamExt;
use futures::future;
use mononoke_types::ChangesetId;
use mononoke_types::audit::CommitInfo;
use mononoke_types::audit::LoadContext;
use mononoke_types::audit::LoadTags;
use mononoke_types::audit::ObjectId;
use mononoke_types::audit::StoredObject;
use mononoke_types::audit::WalkError;
use mononoke_types::audit::WalkStage;
use mononoke_types::audit::Walkable;
use repo_blobstore::RepoBlobstoreRef;

/// A load context over the repo's blobstore whose tags nothing reads.
pub fn load_ctx(ctx: &CoreContext, repo: &impl RepoBlobstoreRef) -> LoadContext {
    LoadContext::new(
        ctx.clone(),
        Arc::new(repo.repo_blobstore().clone()),
        Arc::new(LoadTags::default()),
    )
}

/// `changeset_id` as the runner audits it, with its parents from the commit
/// graph.
pub async fn commit(
    ctx: &CoreContext,
    repo: &impl CommitGraphRef,
    changeset_id: ChangesetId,
) -> Result<Arc<CommitInfo>> {
    let parents = repo
        .commit_graph()
        .changeset_parents(ctx, changeset_id)
        .await?
        .into_vec();
    Ok(Arc::new(CommitInfo {
        changeset_id,
        parents,
    }))
}

/// What family `W` emits for `changeset_id`, loading at most `max_loads`
/// objects at once (the `.t` tests use four), or the error that ended it.
pub async fn walk<W, R>(
    ctx: &CoreContext,
    repo: &Arc<R>,
    changeset_id: ChangesetId,
    max_loads: usize,
) -> Result<Result<Vec<StoredObject<R>>, WalkError>>
where
    W: Walkable<R>,
    R: CommitGraphRef + RepoBlobstoreRef,
{
    let commit = commit(ctx, &**repo, changeset_id).await?;
    Ok(W::walk_new(
        load_ctx(ctx, &**repo),
        repo.clone(),
        commit,
        NonZeroUsize::new(max_loads).context("max_loads is zero")?,
    )
    .try_collect()
    .await)
}

/// The class, stage and object of a walk that failed.
pub fn failed(error: WalkError) -> (&'static str, WalkStage, Option<ObjectId>) {
    (error.class(), error.stage(), error.object().cloned())
}

/// Records the most reads of keys containing `key_part` in progress at once.
/// Each such read waits for one more poll, so reads started together overlap.
#[derive(Debug)]
pub struct OverlappingReadsBlobstore<B> {
    inner: B,
    key_part: &'static str,
    reading: AtomicUsize,
    peak: Arc<AtomicUsize>,
}

impl<B> OverlappingReadsBlobstore<B> {
    /// The store, and the peak it records, which the test can also reset.
    pub fn new(inner: B, key_part: &'static str) -> (Self, Arc<AtomicUsize>) {
        let peak = Arc::new(AtomicUsize::new(0));
        let store = Self {
            inner,
            key_part,
            reading: AtomicUsize::new(0),
            peak: peak.clone(),
        };
        (store, peak)
    }
}

impl<B: fmt::Display> fmt::Display for OverlappingReadsBlobstore<B> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "OverlappingReadsBlobstore<{}>", self.inner)
    }
}

#[async_trait]
impl<B: Blobstore> Blobstore for OverlappingReadsBlobstore<B> {
    async fn get<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>> {
        if !key.contains(self.key_part) {
            return self.inner.get(ctx, key).await;
        }
        let reading = self.reading.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(reading, Ordering::SeqCst);
        let mut yielded = false;
        future::poll_fn(|cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
        self.reading.fetch_sub(1, Ordering::SeqCst);
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
        self.inner.unlink(ctx, key).await
    }
}
