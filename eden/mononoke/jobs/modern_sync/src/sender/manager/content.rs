/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Error;
use anyhow::anyhow;
use bytes::BytesMut;
use bytesize::ByteSize;
use cloned::cloned;
use context::CoreContext;
use edenapi_types::AnyFileContentId;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use metaconfig_types::ModernSyncChannelConfig;
use mononoke_types::ContentId;
use repo_blobstore::RepoBlobstore;
use stats::define_stats;
use stats::prelude::*;
use tokio::sync::mpsc;
use tokio::time::interval;

use super::pipeline::DoneSenders;
use super::pipeline::FlushPipeline;
use crate::sender::edenapi::EdenapiSender;
use crate::sender::manager::ContentLookup;
use crate::sender::manager::ContentMessage;
use crate::sender::manager::LoopAction;
use crate::sender::manager::Manager;

define_stats! {
    prefix = "mononoke.modern_sync.manager.content";

    synced_contents:  dynamic_timeseries("{}.synced_contents", (repo: String); Sum),
    already_present_contents:  dynamic_timeseries("{}.already_present_contents", (repo: String); Sum),
    content_upload_time_s:  dynamic_timeseries("{}.content_upload_time_ms", (repo: String); Average),

    contents_queue_capacity: dynamic_singleton_counter("{}.contents.queue_capacity", (repo: String)),
    contents_queue_len: dynamic_quantile_stat("{}.contents.queue_len", (repo: String); Average; P 50, P 75, P 95, P 99; Duration::from_secs(60), Duration::from_secs(600), Duration::from_secs(3600)),
    contents_queue_max_capacity: dynamic_singleton_counter("{}.contents.queue_max_capacity", (repo: String)),
}

pub(crate) struct ContentManager {
    max_blob_bytes: u64,
    config: ModernSyncChannelConfig,
    content_recv: mpsc::Receiver<ContentMessage>,
    repo_blobstore: RepoBlobstore,
}

impl ContentManager {
    pub(crate) fn new(
        max_blob_bytes: u64,
        config: ModernSyncChannelConfig,
        content_recv: mpsc::Receiver<ContentMessage>,
        repo_blobstore: RepoBlobstore,
    ) -> Self {
        Self {
            max_blob_bytes,
            config,
            content_recv,
            repo_blobstore,
        }
    }
}

struct ContentRunner {
    ctx: CoreContext,
    reponame: String,
    content_es: Arc<dyn EdenapiSender + Send + Sync>,
    cancellation_requested: Arc<AtomicBool>,
    max_blob_bytes: u64,
    config: ModernSyncChannelConfig,
    repo_blobstore: RepoBlobstore,
    pipeline: FlushPipeline,
    pending_senders: DoneSenders,
    current_batch: Vec<ContentId>,
    current_batch_size: u64,
    current_batch_lookup: ContentLookup,
}

impl ContentRunner {
    fn new(
        ctx: CoreContext,
        reponame: String,
        content_es: Arc<dyn EdenapiSender + Send + Sync>,
        cancellation_requested: Arc<AtomicBool>,
        max_blob_bytes: u64,
        config: ModernSyncChannelConfig,
        repo_blobstore: RepoBlobstore,
        pipeline: FlushPipeline,
    ) -> Self {
        Self {
            ctx,
            reponame,
            content_es,
            cancellation_requested,
            max_blob_bytes,
            config,
            repo_blobstore,
            pipeline,
            pending_senders: Vec::new(),
            current_batch: Vec::new(),
            current_batch_size: 0,
            current_batch_lookup: ContentLookup::Skip,
        }
    }

    async fn run(
        &mut self,
        content_recv: &mut mpsc::Receiver<ContentMessage>,
    ) -> anyhow::Result<()> {
        let mut flush_timer = interval(Duration::from_millis(self.config.flush_interval_ms as u64));
        while !self.cancelled() {
            let action = tokio::select! {
                msg = content_recv.recv() => {
                    self.record_queue_stats(content_recv);
                    self.handle_message(msg).await
                }
                _ = flush_timer.tick() => self.handle_tick().await,
                completion = self.pipeline.wait_for_completion() => {
                    match completion {
                        Some(err) => self.fail(err),
                        None => LoopAction::Continue,
                    }
                }
            };
            match action {
                LoopAction::Continue => {}
                LoopAction::Exit => return Ok(()),
                LoopAction::Fail(err) => return Err(err),
            }
        }
        Ok(())
    }

    async fn handle_message(&mut self, msg: Option<ContentMessage>) -> LoopAction {
        match msg {
            Some(ContentMessage::Content(ct_id, size, lookup)) => {
                self.current_batch_size += size;
                self.current_batch.push(ct_id);
                if lookup == ContentLookup::Perform {
                    self.current_batch_lookup = ContentLookup::Perform;
                }
            }
            Some(ContentMessage::ContentDone(files_sender, tree_sender)) => {
                self.pending_senders.push(files_sender);
                self.pending_senders.push(tree_sender);
            }
            None => return self.drain().await,
        }

        // Only non-empty batches may spawn an upload; degenerate
        // configs (zero blob or batch cap) must not trigger
        // empty ones.
        if !self.current_batch.is_empty()
            && (self.current_batch_size >= self.max_blob_bytes
                || self.current_batch.len() >= self.config.batch_size as usize)
        {
            if let Err(err) = self.flush_current_batch().await {
                return self.fail(err);
            }
        }
        self.poll_completions()
    }

    async fn handle_tick(&mut self) -> LoopAction {
        if !self.current_batch.is_empty() {
            if let Err(err) = self.flush_current_batch().await {
                return self.fail(err);
            }
        } else if !self.pending_senders.is_empty() {
            // Signal-only tick: no data, so wait for
            // in-flight flushes, then release the signals.
            let senders = std::mem::take(&mut self.pending_senders);
            if let Err(e) = self.pipeline.barrier(senders).await {
                tracing::error!("Error processing content: {:?}", e);
                return LoopAction::Fail(e);
            }
        }
        self.poll_completions()
    }

    async fn drain(&mut self) -> LoopAction {
        // On cancellation, stop without draining: the
        // pipeline Drop aborts in-flight uploads and
        // drops held signals.
        if self.cancelled() {
            return LoopAction::Exit;
        }
        if !self.current_batch.is_empty() {
            if let Err(err) = self.flush_current_batch().await {
                return self.fail(err);
            }
        }
        let senders = std::mem::take(&mut self.pending_senders);
        if let Err(e) = self.pipeline.barrier(senders).await {
            tracing::error!("Error processing content: {:?}", e);
            return LoopAction::Fail(e);
        }
        LoopAction::Exit
    }

    async fn flush_current_batch(&mut self) -> anyhow::Result<()> {
        let batch = std::mem::take(&mut self.current_batch);
        let bytes = std::mem::replace(&mut self.current_batch_size, 0);
        let lookup = std::mem::replace(&mut self.current_batch_lookup, ContentLookup::Skip);
        let senders = std::mem::take(&mut self.pending_senders);
        self.spawn_flush(batch, bytes, lookup, senders).await
    }

    async fn spawn_flush(
        &mut self,
        batch: Vec<ContentId>,
        batch_bytes: u64,
        lookup: ContentLookup,
        senders: DoneSenders,
    ) -> anyhow::Result<()> {
        let ctx = self.ctx.clone();
        let repo_blobstore = self.repo_blobstore.clone();
        let content_es = self.content_es.clone();
        let reponame = self.reponame.clone();
        self.pipeline
            .spawn_flush(batch_bytes, senders, async move {
                let start = std::time::Instant::now();
                let batch = match lookup {
                    ContentLookup::Skip => batch,
                    ContentLookup::Perform => {
                        let requested = batch.len();
                        let missing = match content_es.filter_existing_contents(batch).await {
                            Ok(missing) => missing,
                            Err(e) => {
                                tracing::error!("Error processing content: {:?}", e);
                                return Err(e);
                            }
                        };
                        let present = requested - missing.len();
                        if present > 0 {
                            tracing::info!(
                                "Skipping {} of {} contents the replica already has",
                                present,
                                requested,
                            );
                        }
                        STATS::already_present_contents
                            .add_value(present as i64, (reponame.clone(),));
                        missing
                    }
                };
                if batch.is_empty() {
                    return Ok(());
                }
                let batch_len = batch.len();
                let full_items = stream::iter(batch)
                    .map(|id| {
                        cloned!(repo_blobstore);
                        let ctx = ctx.clone();
                        async move {
                            let bytes = filestore::fetch(repo_blobstore, &ctx, &id.into())
                                .await?
                                .ok_or(anyhow!("Content is not found (which should never happen)"))?
                                .try_collect::<BytesMut>()
                                .await?;
                            Ok::<_, Error>((
                                AnyFileContentId::ContentId(id.into()),
                                bytes.freeze().into(),
                            ))
                        }
                    })
                    // Fetch concurrency is batch_len per flush, so at most
                    // batch_size x max_concurrent_flushes fetches run at once;
                    // total buffered bytes stay within the byte pool.
                    .buffer_unordered(batch_len)
                    .try_collect::<Vec<(AnyFileContentId, minibytes::Bytes)>>()
                    .await?;
                let uploaded_bytes: u64 =
                    full_items.iter().map(|(_, bytes)| bytes.len() as u64).sum();

                // Log upload failures at the point of failure; the loop logs
                // the same line again when the watermark surfaces it.
                if let Err(e) = content_es.upload_contents(full_items).await {
                    tracing::error!("Error processing content: {:?}", e);
                    return Err(e);
                }
                tracing::info!(
                    "Uploaded {} contents with size {} in {}ms",
                    batch_len,
                    ByteSize::b(uploaded_bytes).to_string(),
                    start.elapsed().as_millis(),
                );

                let elapsed = start.elapsed().as_millis() / batch_len as u128;
                STATS::content_upload_time_s.add_value(elapsed as i64, (reponame.clone(),));
                STATS::synced_contents.add_value(batch_len as i64, (reponame.clone(),));
                Ok(())
            })
            .await
    }

    fn fail(&mut self, err: anyhow::Error) -> LoopAction {
        tracing::error!("Error processing content: {:?}", err);
        // This channel already logged the failure; drop locally
        // queued signals so downstream winds down silently
        // instead of logging the same failure again per signal.
        self.pipeline.abort(&err);
        LoopAction::Fail(err)
    }

    // Drain completions after every select event so the watermark keeps
    // advancing even when no new flush spawns.
    fn poll_completions(&mut self) -> LoopAction {
        match self.pipeline.poll_completions() {
            Some(err) => self.fail(err),
            None => LoopAction::Continue,
        }
    }

    fn record_queue_stats(&self, content_recv: &mpsc::Receiver<ContentMessage>) {
        tracing::debug!(
            "Content channel capacity: {} max capacity: {} in queue: {}",
            content_recv.capacity(),
            self.config.channel_size,
            content_recv.len()
        );
        STATS::contents_queue_capacity.set_value(
            self.ctx.fb,
            content_recv.capacity() as i64,
            (self.reponame.clone(),),
        );
        STATS::contents_queue_len.add_value(content_recv.len() as i64, (self.reponame.clone(),));
        STATS::contents_queue_max_capacity.set_value(
            self.ctx.fb,
            content_recv.max_capacity() as i64,
            (self.reponame.clone(),),
        );
    }

    fn cancelled(&self) -> bool {
        self.cancellation_requested.load(Ordering::Relaxed)
    }
}

impl Manager for ContentManager {
    async fn run(
        &mut self,
        ctx: CoreContext,
        reponame: String,
        content_es: Arc<dyn EdenapiSender + Send + Sync>,
        cancellation_requested: Arc<AtomicBool>,
    ) -> anyhow::Result<()> {
        let Self {
            max_blob_bytes,
            config,
            content_recv,
            repo_blobstore,
        } = self;
        let pipeline = FlushPipeline::new(
            reponame.clone(),
            "content",
            config.max_concurrent_flushes,
            Some(config.max_bytes_in_flight),
        );
        let mut runner = ContentRunner::new(
            ctx,
            reponame,
            content_es,
            cancellation_requested,
            *max_blob_bytes,
            config.clone(),
            repo_blobstore.clone(),
            pipeline,
        );
        runner.run(content_recv).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Mutex;

    use anyhow::Result;
    use async_trait::async_trait;
    use bytes::Bytes;
    use fbinit::FacebookInit;
    use filestore::FilestoreConfig;
    use futures::channel::oneshot;
    use memblob::Memblob;
    use mercurial_types::HgChangesetId;
    use mercurial_types::HgFileNodeId;
    use mercurial_types::HgManifestId;
    use mercurial_types::blobs::HgBlobChangeset;
    use mononoke_macros::mononoke;
    use mononoke_types::BonsaiChangeset;
    use mononoke_types::ChangesetId;
    use mononoke_types::RepositoryId;
    use scuba_ext::MononokeScubaSampleBuilder;

    use super::*;
    use crate::sender::edenapi::BookmarkMove;

    type Upload = (AnyFileContentId, minibytes::Bytes);

    /// Answers content lookups as a replica holding `on_replica` would, and
    /// records the lookups and uploads it receives.
    #[derive(Default)]
    struct RecordingSender {
        on_replica: HashSet<ContentId>,
        fail_lookups: bool,
        lookups: Mutex<Vec<Vec<ContentId>>>,
        uploads: Mutex<Vec<Vec<Upload>>>,
    }

    impl RecordingSender {
        fn with_replica_contents(on_replica: impl IntoIterator<Item = ContentId>) -> Self {
            Self {
                on_replica: on_replica.into_iter().collect(),
                ..Default::default()
            }
        }

        fn lookups(&self) -> Vec<Vec<ContentId>> {
            self.lookups.lock().expect("lookups lock poisoned").clone()
        }

        fn uploads(&self) -> Vec<Vec<Upload>> {
            self.uploads.lock().expect("uploads lock poisoned").clone()
        }
    }

    #[async_trait]
    impl EdenapiSender for RecordingSender {
        async fn upload_contents(&self, contents: Vec<Upload>) -> Result<()> {
            self.uploads
                .lock()
                .expect("uploads lock poisoned")
                .push(contents);
            Ok(())
        }

        async fn upload_trees(&self, _trees: Vec<HgManifestId>) -> Result<()> {
            unimplemented!()
        }

        async fn upload_filenodes(&self, _fn_ids: Vec<HgFileNodeId>) -> Result<()> {
            unimplemented!()
        }

        async fn set_bookmark(&self, _bookmark: String, _moves: Vec<BookmarkMove>) -> Result<()> {
            unimplemented!()
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

        async fn filter_existing_contents(&self, ids: Vec<ContentId>) -> Result<Vec<ContentId>> {
            self.lookups
                .lock()
                .expect("lookups lock poisoned")
                .push(ids.clone());
            if self.fail_lookups {
                return Err(anyhow!("504 Gateway Timeout on lookup"));
            }
            Ok(ids
                .into_iter()
                .filter(|id| !self.on_replica.contains(id))
                .collect())
        }
    }

    struct Source {
        ctx: CoreContext,
        blobstore: RepoBlobstore,
    }

    impl Source {
        fn new(fb: FacebookInit) -> Self {
            Self {
                ctx: CoreContext::test_mock(fb),
                blobstore: RepoBlobstore::new(
                    Arc::new(Memblob::default()),
                    None,
                    RepositoryId::new(0),
                    MononokeScubaSampleBuilder::with_discard(),
                ),
            }
        }

        async fn store(&self, data: &'static str) -> Result<ContentId> {
            let ((id, _size), upload) = filestore::store_bytes(
                &self.blobstore,
                FilestoreConfig::no_chunking_filestore(),
                &self.ctx,
                Bytes::from_static(data.as_bytes()),
            );
            upload.await?;
            Ok(id)
        }

        /// One flush at a time, so uploads are recorded in batch order.
        fn runner(&self, sender: Arc<RecordingSender>, batch_size: i64) -> ContentRunner {
            let config = ModernSyncChannelConfig {
                batch_size,
                channel_size: 100,
                flush_interval_ms: 1000,
                max_concurrent_flushes: 1,
                max_bytes_in_flight: 1_000_000,
            };
            ContentRunner::new(
                self.ctx.clone(),
                "testrepo".to_string(),
                sender,
                Arc::new(AtomicBool::new(false)),
                1_000_000,
                config,
                self.blobstore.clone(),
                FlushPipeline::new("testrepo".to_string(), "content", 1, Some(1_000_000)),
            )
        }
    }

    /// Hands `messages` to the runner, then closes its channel and waits for
    /// every upload, the way the manager loop does.
    async fn feed(runner: &mut ContentRunner, messages: Vec<ContentMessage>) -> Result<()> {
        for msg in messages {
            if let LoopAction::Fail(err) = runner.handle_message(Some(msg)).await {
                return Err(err);
            }
        }
        match runner.handle_message(None).await {
            LoopAction::Fail(err) => Err(err),
            LoopAction::Continue | LoopAction::Exit => Ok(()),
        }
    }

    fn content(id: ContentId, lookup: ContentLookup) -> ContentMessage {
        ContentMessage::Content(id, 1, lookup)
    }

    fn upload(id: ContentId, data: &'static str) -> Upload {
        (
            AnyFileContentId::ContentId(id.into()),
            minibytes::Bytes::from_static(data.as_bytes()),
        )
    }

    #[mononoke::fbinit_test]
    async fn lookup_uploads_only_contents_the_replica_lacks(fb: FacebookInit) -> Result<()> {
        let source = Source::new(fb);
        let first = source.store("first").await?;
        let third = source.store("third").await?;
        // Not in the source blobstore: fetching it would fail the batch, so
        // the batch succeeding shows a present content is never fetched.
        let on_replica = ContentId::from_bytes([7; 32])?;
        let sender = Arc::new(RecordingSender::with_replica_contents([on_replica]));
        let mut runner = source.runner(sender.clone(), 10);

        feed(
            &mut runner,
            [first, on_replica, third]
                .into_iter()
                .map(|id| content(id, ContentLookup::Perform))
                .collect(),
        )
        .await?;

        assert_eq!(sender.lookups(), vec![vec![first, on_replica, third]]);
        assert_eq!(
            sender.uploads(),
            vec![vec![upload(first, "first"), upload(third, "third")]],
            "only the contents missing on the replica are uploaded, in order"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn skip_uploads_every_content_without_a_lookup(fb: FacebookInit) -> Result<()> {
        let source = Source::new(fb);
        let first = source.store("first").await?;
        let second = source.store("second").await?;
        let sender = Arc::new(RecordingSender::with_replica_contents([second]));
        let mut runner = source.runner(sender.clone(), 10);

        feed(
            &mut runner,
            vec![
                content(first, ContentLookup::Skip),
                content(second, ContentLookup::Skip),
            ],
        )
        .await?;

        assert!(sender.lookups().is_empty(), "Skip must not ask the replica");
        assert_eq!(
            sender.uploads(),
            vec![vec![upload(first, "first"), upload(second, "second")]],
            "Skip uploads every content, even one the replica already has"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn one_content_asking_for_a_lookup_covers_only_its_batch(fb: FacebookInit) -> Result<()> {
        let source = Source::new(fb);
        let [first, second, third, fourth, fifth] = [
            source.store("first").await?,
            source.store("second").await?,
            source.store("third").await?,
            source.store("fourth").await?,
            source.store("fifth").await?,
        ];
        let sender = Arc::new(RecordingSender::with_replica_contents([
            first, third, fourth,
        ]));
        // Batches of three: [first, second, third], then [fourth, fifth] on drain.
        let mut runner = source.runner(sender.clone(), 3);

        feed(
            &mut runner,
            vec![
                content(first, ContentLookup::Skip),
                content(second, ContentLookup::Perform),
                content(third, ContentLookup::Skip),
                content(fourth, ContentLookup::Skip),
                content(fifth, ContentLookup::Skip),
            ],
        )
        .await?;

        assert_eq!(
            sender.lookups(),
            vec![vec![first, second, third]],
            "the first batch is looked up as a whole; the second is not"
        );
        assert_eq!(
            sender.uploads(),
            vec![
                vec![upload(second, "second")],
                vec![upload(fourth, "fourth"), upload(fifth, "fifth")],
            ],
            "first and third are skipped after the lookup; fourth is uploaded because its batch skips the lookup"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn batch_already_on_the_replica_still_releases_files_and_trees(
        fb: FacebookInit,
    ) -> Result<()> {
        let source = Source::new(fb);
        let first = source.store("first").await?;
        let sender = Arc::new(RecordingSender::with_replica_contents([first]));
        let mut runner = source.runner(sender.clone(), 10);
        let (files_tx, files_rx) = oneshot::channel();
        let (trees_tx, trees_rx) = oneshot::channel();

        feed(
            &mut runner,
            vec![
                content(first, ContentLookup::Perform),
                ContentMessage::ContentDone(files_tx, trees_tx),
            ],
        )
        .await?;

        assert!(
            sender.uploads().is_empty(),
            "nothing is left to upload, so no upload request is sent"
        );
        files_rx.await??;
        trees_rx.await??;
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn failed_lookup_fails_the_batch_without_uploading(fb: FacebookInit) -> Result<()> {
        let source = Source::new(fb);
        let first = source.store("first").await?;
        let sender = Arc::new(RecordingSender {
            fail_lookups: true,
            ..Default::default()
        });
        let mut runner = source.runner(sender.clone(), 10);
        let (files_tx, files_rx) = oneshot::channel();
        let (trees_tx, _trees_rx) = oneshot::channel();

        let err = feed(
            &mut runner,
            vec![
                content(first, ContentLookup::Perform),
                ContentMessage::ContentDone(files_tx, trees_tx),
            ],
        )
        .await
        .expect_err("a failed lookup must fail the content manager");

        assert!(format!("{err:?}").contains("504 Gateway Timeout on lookup"));
        assert!(
            sender.uploads().is_empty(),
            "a failed lookup must not fall back to uploading"
        );
        assert!(
            !matches!(files_rx.await, Ok(Ok(()))),
            "files must not start after a failed content batch"
        );
        Ok(())
    }
}
