/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use edenapi_types::AnyFileContentId;
use futures::FutureExt;
use futures::future::BoxFuture;
use mercurial_types::HgChangesetId;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestId;
use mercurial_types::blobs::HgBlobChangeset;
use minibytes::Bytes;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;

use crate::sender::edenapi::BookmarkMove;
use crate::sender::edenapi::BookmarkMovesRejected;
use crate::sender::edenapi::EdenapiSender;

const MAX_RETRIES: usize = 3;

pub struct RetryEdenapiSender {
    inner: Arc<dyn EdenapiSender + Send + Sync>,
}

impl RetryEdenapiSender {
    pub fn new(inner: Arc<dyn EdenapiSender + Send + Sync>) -> Self {
        Self { inner }
    }

    async fn with_retry<'t, T>(
        &'t self,
        func: impl Fn(&'t Self) -> BoxFuture<'t, Result<T>>,
    ) -> Result<T> {
        let retry_count = MAX_RETRIES;
        with_retry(retry_count, || func(self)).await
    }
}

#[async_trait]
impl EdenapiSender for RetryEdenapiSender {
    async fn upload_contents(&self, contents: Vec<(AnyFileContentId, Bytes)>) -> Result<()> {
        self.with_retry(|this| this.inner.upload_contents(contents.clone()).boxed())
            .await
    }

    async fn upload_trees(&self, trees: Vec<HgManifestId>) -> Result<()> {
        self.with_retry(|this| this.inner.upload_trees(trees.clone()).boxed())
            .await
    }

    async fn upload_filenodes(&self, fn_ids: Vec<HgFileNodeId>) -> Result<()> {
        self.with_retry(|this| this.inner.upload_filenodes(fn_ids.clone()).boxed())
            .await
    }

    async fn set_bookmark(&self, bookmark: String, moves: Vec<BookmarkMove>) -> Result<()> {
        // Retry only when no answer came back (transport error, 5xx, timeout).
        // A replay is one compare-and-swap on the replica, and a chain it
        // already applied comes back as "already processed", which counts as
        // success, so sending it again is safe. A refusal is not retried: the
        // sync must stop on it.
        with_retry_if(
            MAX_RETRIES,
            || {
                self.inner
                    .set_bookmark(bookmark.clone(), moves.clone())
                    .boxed()
            },
            |e| e.downcast_ref::<BookmarkMovesRejected>().is_none(),
        )
        .await
    }

    async fn upload_identical_changeset(
        &self,
        css: Vec<(HgBlobChangeset, BonsaiChangeset)>,
    ) -> Result<()> {
        self.with_retry(|this| this.inner.upload_identical_changeset(css.clone()).boxed())
            .await
    }

    async fn filter_existing_commits(
        &self,
        ids: Vec<(HgChangesetId, ChangesetId)>,
    ) -> Result<Vec<ChangesetId>> {
        self.with_retry(|this| this.inner.filter_existing_commits(ids.clone()).boxed())
            .await
    }

    async fn read_bookmark(&self, bookmark: String) -> Result<Option<HgChangesetId>> {
        self.with_retry(|this| this.inner.read_bookmark(bookmark.clone()).boxed())
            .await
    }
}

async fn with_retry<'t, T>(
    max_retry_count: usize,
    func: impl Fn() -> BoxFuture<'t, Result<T>>,
) -> Result<T> {
    with_retry_if(max_retry_count, func, |_| true).await
}

async fn with_retry_if<'t, T>(
    max_retry_count: usize,
    func: impl Fn() -> BoxFuture<'t, Result<T>>,
    should_retry: impl Fn(&anyhow::Error) -> bool,
) -> Result<T> {
    let mut attempt = 0usize;
    loop {
        let result = func().await;
        if attempt >= max_retry_count {
            return result;
        }
        match result {
            Ok(result) => return Ok(result),
            Err(e) if !should_retry(&e) => return Err(e),
            Err(e) => {
                tracing::warn!("Found error: {:?}, retrying attempt #{}", e, attempt);
                tokio::time::sleep(Duration::from_secs(attempt as u64 + 1)).await;
            }
        }
        attempt += 1;
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use anyhow::anyhow;
    use mononoke_macros::mononoke;

    use super::*;

    /// Fails the first `failures` lookup and bookmark calls, then answers.
    struct FlakySender {
        failures: usize,
        calls: AtomicUsize,
    }

    impl FlakySender {
        fn new(failures: usize) -> Self {
            Self {
                failures,
                calls: AtomicUsize::new(0),
            }
        }

        fn fail_or_ok<T>(&self, ok: T) -> Result<T> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.failures {
                Err(anyhow!("504 Gateway Timeout on call {call}"))
            } else {
                Ok(ok)
            }
        }
    }

    #[async_trait]
    impl EdenapiSender for FlakySender {
        async fn upload_contents(&self, _contents: Vec<(AnyFileContentId, Bytes)>) -> Result<()> {
            unimplemented!()
        }

        async fn upload_trees(&self, _trees: Vec<HgManifestId>) -> Result<()> {
            unimplemented!()
        }

        async fn upload_filenodes(&self, _fn_ids: Vec<HgFileNodeId>) -> Result<()> {
            unimplemented!()
        }

        async fn set_bookmark(&self, _bookmark: String, _moves: Vec<BookmarkMove>) -> Result<()> {
            self.fail_or_ok(())
        }

        async fn upload_identical_changeset(
            &self,
            _css: Vec<(HgBlobChangeset, BonsaiChangeset)>,
        ) -> Result<()> {
            unimplemented!()
        }

        async fn filter_existing_commits(
            &self,
            ids: Vec<(HgChangesetId, ChangesetId)>,
        ) -> Result<Vec<ChangesetId>> {
            self.fail_or_ok(ids.into_iter().map(|(_, cs_id)| cs_id).collect())
        }

        async fn read_bookmark(&self, _bookmark: String) -> Result<Option<HgChangesetId>> {
            self.fail_or_ok(Some(HgChangesetId::from_bytes(&[1; 20]).unwrap()))
        }
    }

    fn ids() -> Vec<(HgChangesetId, ChangesetId)> {
        vec![(
            HgChangesetId::from_bytes(&[2; 20]).unwrap(),
            ChangesetId::from_bytes([3; 32]).unwrap(),
        )]
    }

    #[mononoke::test]
    async fn filter_existing_commits_retries_transient_errors() {
        let inner = Arc::new(FlakySender::new(MAX_RETRIES));
        let sender = RetryEdenapiSender::new(inner.clone());

        let missing = sender
            .filter_existing_commits(ids())
            .await
            .expect("lookup should succeed once the transient errors stop");

        assert_eq!(missing, vec![ChangesetId::from_bytes([3; 32]).unwrap()]);
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            MAX_RETRIES + 1,
            "one first attempt plus MAX_RETRIES retries"
        );
    }

    #[mononoke::test]
    async fn filter_existing_commits_gives_up_after_max_retries() {
        let inner = Arc::new(FlakySender::new(MAX_RETRIES + 1));
        let sender = RetryEdenapiSender::new(inner.clone());

        sender
            .filter_existing_commits(ids())
            .await
            .expect_err("lookup should fail when every attempt fails");

        assert_eq!(inner.calls.load(Ordering::SeqCst), MAX_RETRIES + 1);
    }

    #[mononoke::test]
    async fn read_bookmark_retries_transient_errors() {
        let inner = Arc::new(FlakySender::new(1));
        let sender = RetryEdenapiSender::new(inner.clone());

        let hgid = sender
            .read_bookmark("master".to_owned())
            .await
            .expect("bookmark read should succeed on the retry");

        assert_eq!(hgid, Some(HgChangesetId::from_bytes(&[1; 20]).unwrap()));
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
    }
    /// Refuses every bookmark replay in-band, the way the replica answers a
    /// chain it will not apply.
    struct RejectingSender {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl EdenapiSender for RejectingSender {
        async fn upload_contents(&self, _contents: Vec<(AnyFileContentId, Bytes)>) -> Result<()> {
            unimplemented!()
        }

        async fn upload_trees(&self, _trees: Vec<HgManifestId>) -> Result<()> {
            unimplemented!()
        }

        async fn upload_filenodes(&self, _fn_ids: Vec<HgFileNodeId>) -> Result<()> {
            unimplemented!()
        }

        async fn set_bookmark(&self, _bookmark: String, _moves: Vec<BookmarkMove>) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(anyhow!("bookmark transaction failed").context(BookmarkMovesRejected))
        }

        async fn upload_identical_changeset(
            &self,
            _css: Vec<(HgBlobChangeset, BonsaiChangeset)>,
        ) -> Result<()> {
            unimplemented!()
        }

        async fn filter_existing_commits(
            &self,
            _ids: Vec<(HgChangesetId, ChangesetId)>,
        ) -> Result<Vec<ChangesetId>> {
            unimplemented!()
        }

        async fn read_bookmark(&self, _bookmark: String) -> Result<Option<HgChangesetId>> {
            unimplemented!()
        }
    }

    fn moves() -> Vec<BookmarkMove> {
        vec![BookmarkMove {
            log_id: 1,
            from: None,
            to: ChangesetId::from_bytes([3; 32]).unwrap(),
            reason: bookmarks::BookmarkUpdateReason::Push,
        }]
    }

    #[mononoke::test]
    async fn set_bookmark_retries_transport_errors() {
        let inner = Arc::new(FlakySender::new(MAX_RETRIES));
        let sender = RetryEdenapiSender::new(inner.clone());

        sender
            .set_bookmark("master".to_owned(), moves())
            .await
            .expect("bookmark replay should succeed once the transport errors stop");

        assert_eq!(inner.calls.load(Ordering::SeqCst), MAX_RETRIES + 1);
    }

    #[mononoke::test]
    async fn set_bookmark_gives_up_after_max_retries() {
        let inner = Arc::new(FlakySender::new(MAX_RETRIES + 1));
        let sender = RetryEdenapiSender::new(inner.clone());

        sender
            .set_bookmark("master".to_owned(), moves())
            .await
            .expect_err("bookmark replay should fail when every attempt fails");

        assert_eq!(inner.calls.load(Ordering::SeqCst), MAX_RETRIES + 1);
    }

    #[mononoke::test]
    async fn set_bookmark_does_not_retry_a_rejection() {
        let inner = Arc::new(RejectingSender {
            calls: AtomicUsize::new(0),
        });
        let sender = RetryEdenapiSender::new(inner.clone());

        let err = sender
            .set_bookmark("master".to_owned(), moves())
            .await
            .expect_err("a rejected bookmark replay must fail");

        assert!(err.downcast_ref::<BookmarkMovesRejected>().is_some());
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            1,
            "a rejection must stop the sync, not be sent again"
        );
    }
}
