/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fmt;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use blobstore::BlobstoreGetData;
use blobstore::BlobstoreIsPresent;
use blobstore::KeyedBlobstore;
use blobstore::Storable;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::TryStreamExt;
use memblob::KeyedMemblob;
use mononoke_macros::mononoke;
use mononoke_types::BlobstoreBytes;
use mononoke_types::FileType;
use mononoke_types::MPath;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::test_manifest::TestLeaf;
use super::test_manifest::TestManifest;
use crate::Entry;
use crate::Manifest;
use crate::ManifestOps;
use crate::PathOrPrefix;
use crate::find_entries_from_subentries;

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const CANCELLATION_TIMEOUT: Duration = Duration::from_secs(1);
const RETAINED_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
struct GatedBlobstore {
    inner: KeyedMemblob,
    release: Semaphore,
    started: mpsc::UnboundedSender<Weak<Vec<u8>>>,
    dropped: mpsc::UnboundedSender<()>,
}

impl fmt::Display for GatedBlobstore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GatedBlobstore")
    }
}

struct SignalOnDrop(mpsc::UnboundedSender<()>);

impl Drop for SignalOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

struct ReleaseOnDrop<'a>(&'a Semaphore);

impl Drop for ReleaseOnDrop<'_> {
    fn drop(&mut self) {
        self.0.add_permits(1);
    }
}

#[async_trait]
impl KeyedBlobstore for GatedBlobstore {
    async fn get<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>> {
        let _drop_signal = SignalOnDrop(self.dropped.clone());
        let retained = Arc::new(vec![1; RETAINED_BYTES]);
        self.started.send(Arc::downgrade(&retained))?;
        let _permit = self.release.acquire().await?;
        let result = self.inner.get(ctx, key).await;
        drop(retained);
        result
    }

    async fn put<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<()> {
        self.inner.put(ctx, key, value).await
    }

    async fn is_present<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<BlobstoreIsPresent> {
        self.inner.is_present(ctx, key).await
    }

    async fn copy<'a>(
        &'a self,
        ctx: &'a CoreContext,
        old_key: &'a str,
        new_key: String,
    ) -> Result<()> {
        self.inner.copy(ctx, old_key, new_key).await
    }

    async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()> {
        self.inner.unlink(ctx, key).await
    }
}

#[mononoke::fbinit_test]
async fn cancelling_manifest_traversal_drops_inflight_read(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let inner = KeyedMemblob::default();
    let root = TestManifest::new().store(&ctx, &inner).await?;
    let (started, mut started_rx) = mpsc::unbounded_channel();
    let (dropped, mut dropped_rx) = mpsc::unbounded_channel();
    let store = Arc::new(GatedBlobstore {
        inner,
        release: Semaphore::new(0),
        started,
        dropped,
    });
    let _cleanup = ReleaseOnDrop(&store.release);
    let entries = root.list_all_entries(ctx, store.clone());
    let caller = mononoke::spawn_task(async move { entries.try_collect::<Vec<_>>().await });
    let retained = timeout(EVENT_TIMEOUT, started_rx.recv())
        .await?
        .expect("the real manifest load must start before cancellation");
    assert!(retained.upgrade().is_some());

    caller.abort();
    let cancellation = timeout(EVENT_TIMEOUT, caller)
        .await?
        .expect_err("caller was aborted");
    assert!(cancellation.is_cancelled());
    let dropped_on_cancel = timeout(CANCELLATION_TIMEOUT, dropped_rx.recv())
        .await
        .is_ok_and(|event| event.is_some());
    let retained_on_cancel = retained.upgrade().is_some();

    // Release a detached old-code read before asserting, so a red run does not leak a task.
    store.release.add_permits(1);
    if !dropped_on_cancel {
        timeout(EVENT_TIMEOUT, dropped_rx.recv())
            .await?
            .expect("released read must finish and drop its allocation");
    }
    assert!(
        retained.upgrade().is_none(),
        "cleanup must release the 1 MiB sentinel"
    );
    eprintln!(
        "manifest cancellation evidence: dropped_before_release={dropped_on_cancel} retained_before_release={retained_on_cancel} sentinel_bytes={RETAINED_BYTES}"
    );
    assert!(
        dropped_on_cancel && !retained_on_cancel,
        "dropping the traversal must drop its in-flight manifest read before its gate is released"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn partition_lookup_starts_sibling_reads_before_either_finishes(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let inner = KeyedMemblob::default();
    let leaf = TestLeaf::new("content").store(&ctx, &inner).await?;
    let left = TestManifest::new()
        .insert("left", Entry::Leaf((FileType::Regular, leaf)))
        .store(&ctx, &inner)
        .await?;
    let right = TestManifest::new()
        .insert("right", Entry::Leaf((FileType::Regular, leaf)))
        .store(&ctx, &inner)
        .await?;
    let map = TestManifest::new()
        .insert("a", Entry::Tree(left))
        .insert("b", Entry::Tree(right))
        .into_trie_map(&ctx, &inner)
        .await?;
    let (started, mut started_rx) = mpsc::unbounded_channel();
    let (dropped, _dropped_rx) = mpsc::unbounded_channel();
    let store = Arc::new(GatedBlobstore {
        inner,
        release: Semaphore::new(0),
        started,
        dropped,
    });
    let _cleanup = ReleaseOnDrop(&store.release);
    let entries =
        find_entries_from_subentries(ctx, store.clone(), map, [PathOrPrefix::Prefix(MPath::ROOT)]);
    let caller = tokio_util::task::AbortOnDropHandle::new(mononoke::spawn_task(async move {
        entries.try_collect::<Vec<_>>().await
    }));
    for _ in 0..2 {
        timeout(EVENT_TIMEOUT, started_rx.recv())
            .await?
            .expect("both sibling directory reads must start while blocked");
    }
    store.release.add_permits(2);
    let entries = timeout(EVENT_TIMEOUT, caller).await???;
    assert_eq!(entries.len(), 4);
    Ok(())
}
