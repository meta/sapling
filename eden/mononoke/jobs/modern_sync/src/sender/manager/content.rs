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
use crate::sender::manager::ContentMessage;
use crate::sender::manager::LoopAction;
use crate::sender::manager::Manager;

define_stats! {
    prefix = "mononoke.modern_sync.manager.content";

    synced_contents:  dynamic_timeseries("{}.synced_contents", (repo: String); Sum),
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
            Some(ContentMessage::Content(ct_id, size)) => {
                self.current_batch_size += size;
                self.current_batch.push(ct_id);
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
        let senders = std::mem::take(&mut self.pending_senders);
        self.spawn_flush(batch, bytes, senders).await
    }

    async fn spawn_flush(
        &mut self,
        batch: Vec<ContentId>,
        batch_bytes: u64,
        senders: DoneSenders,
    ) -> anyhow::Result<()> {
        let batch_len = batch.len();
        let ctx = self.ctx.clone();
        let repo_blobstore = self.repo_blobstore.clone();
        let content_es = self.content_es.clone();
        let reponame = self.reponame.clone();
        self.pipeline
            .spawn_flush(batch_bytes, senders, async move {
                let start = std::time::Instant::now();
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

                // Log upload failures at the point of failure; the loop logs
                // the same line again when the watermark surfaces it.
                if let Err(e) = content_es.upload_contents(full_items).await {
                    tracing::error!("Error processing content: {:?}", e);
                    return Err(e);
                }
                tracing::info!(
                    "Uploaded {} contents with size {} in {}ms",
                    batch_len,
                    ByteSize::b(batch_bytes).to_string(),
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
