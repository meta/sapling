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

use context::CoreContext;
use futures::channel::oneshot;
use mercurial_types::HgFileNodeId;
use metaconfig_types::ModernSyncChannelConfig;
use stats::define_stats;
use stats::prelude::*;
use tokio::sync::mpsc;
use tokio::time::interval;

use super::pipeline::DoneSenders;
use super::pipeline::FlushPipeline;
use crate::sender::edenapi::EdenapiSender;
use crate::sender::manager::FileMessage;
use crate::sender::manager::LoopAction;
use crate::sender::manager::Manager;

define_stats! {
    prefix = "mononoke.modern_sync.manager.filenode";

    synced_filenodes:  dynamic_timeseries("{}.synced_filenodes", (repo: String); Sum),
    content_wait_time_s:  dynamic_timeseries("{}.content_wait_time_s", (repo: String); Average),

    files_queue_capacity: dynamic_singleton_counter("{}.files.queue_capacity", (repo: String)),
    files_queue_len: dynamic_quantile_stat("{}.files.queue_len", (repo: String); Average; P 50, P 75, P 95, P 99; Duration::from_secs(60), Duration::from_secs(600), Duration::from_secs(3600)),
    files_queue_max_capacity: dynamic_singleton_counter("{}.files.queue_max_capacity", (repo: String)),
}

pub(crate) struct FilenodeManager {
    config: ModernSyncChannelConfig,
    filenodes_recv: mpsc::Receiver<FileMessage>,
}

impl FilenodeManager {
    pub(crate) fn new(
        config: ModernSyncChannelConfig,
        filenodes_recv: mpsc::Receiver<FileMessage>,
    ) -> Self {
        Self {
            config,
            filenodes_recv,
        }
    }
}

struct FilenodeRunner {
    ctx: CoreContext,
    reponame: String,
    filenodes_es: Arc<dyn EdenapiSender + Send + Sync>,
    cancellation_requested: Arc<AtomicBool>,
    config: ModernSyncChannelConfig,
    pipeline: FlushPipeline,
    encountered_error: Option<anyhow::Error>,
    batch_filenodes: Vec<HgFileNodeId>,
    pending_senders: DoneSenders,
}

impl FilenodeRunner {
    async fn run(
        &mut self,
        filenodes_recv: &mut mpsc::Receiver<FileMessage>,
    ) -> anyhow::Result<()> {
        let mut timer = interval(Duration::from_millis(self.config.flush_interval_ms as u64));
        while !self.cancelled() {
            let action = tokio::select! {
                msg = filenodes_recv.recv() => {
                    self.record_queue_stats(filenodes_recv);
                    self.handle_message(msg).await
                }
                _ = timer.tick() => self.handle_tick().await,
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

    async fn handle_message(&mut self, msg: Option<FileMessage>) -> LoopAction {
        match msg {
            Some(FileMessage::WaitForContents(receiver)) => {
                self.note_contents_outcome(receiver).await;
            }
            Some(FileMessage::FileNode(f)) if self.encountered_error.is_none() => {
                self.batch_filenodes.push(f);
            }
            Some(FileMessage::FilesDone(sender)) => {
                self.pending_senders.push(sender);
            }
            Some(FileMessage::FileNode(_)) => (),
            None => return self.drain().await,
        }
        // Only non-empty batches may spawn an upload; a zero
        // batch cap must not trigger empty ones. A recorded
        // upstream error winds down here instead of flushing,
        // so Done signals arriving after the error still fail
        // explicitly rather than stranding as dropped channels.
        if !self.batch_filenodes.is_empty()
            && self.batch_filenodes.len() >= self.config.batch_size as usize
        {
            if let Some(action) = self.wind_down_if_failed() {
                return action;
            }
            if let Err(err) = self.flush_current_batch().await {
                return self.fail(err);
            }
        }
        self.poll_completions()
    }

    async fn handle_tick(&mut self) -> LoopAction {
        if let Some(action) = self.wind_down_if_failed() {
            return action;
        }
        if !self.batch_filenodes.is_empty() {
            if let Err(err) = self.flush_current_batch().await {
                return self.fail(err);
            }
        } else if !self.pending_senders.is_empty() {
            let senders = std::mem::take(&mut self.pending_senders);
            if let Err(e) = self.pipeline.barrier(senders).await {
                tracing::error!("Filenodes flush failed: {:?}", e);
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
        // The loop is ending, so there are no later Done signals to
        // protect: a recorded contents error fails held signals instead
        // of acking in-flight flushes over it.
        if let Some(err) = self.encountered_error.take() {
            self.pipeline
                .abort_with_pending(err, std::mem::take(&mut self.pending_senders));
            return LoopAction::Exit;
        }
        if !self.batch_filenodes.is_empty() {
            if let Err(err) = self.flush_current_batch().await {
                return self.fail(err);
            }
        }
        let senders = std::mem::take(&mut self.pending_senders);
        if let Err(e) = self.pipeline.barrier(senders).await {
            tracing::error!("Filenodes flush failed: {:?}", e);
            return LoopAction::Fail(e);
        }
        LoopAction::Exit
    }

    async fn note_contents_outcome(&mut self, receiver: oneshot::Receiver<anyhow::Result<()>>) {
        let start = std::time::Instant::now();
        match receiver.await {
            Ok(Err(e)) => {
                self.encountered_error.get_or_insert(
                    e.context("Contents error received. Winding down filenodes sender."),
                );
            }
            // A dropped sender means the content
            // manager died without completing the
            // upload; proceeding would upload files
            // whose contents may never land.
            Err(e) => {
                self.encountered_error
                    .get_or_insert(anyhow::anyhow!("Error waiting for contents: {e:#}"));
            }
            _ => (),
        }
        let elapsed = start.elapsed().as_secs();
        STATS::content_wait_time_s.add_value(elapsed as i64, (self.reponame.clone(),));
    }

    fn wind_down_if_failed(&mut self) -> Option<LoopAction> {
        // Wind down only with queued work to fail: with empty queues keep
        // consuming so later Done signals fail explicitly instead of
        // stranding as dropped channels when the loop exits.
        if self.encountered_error.is_none()
            || (self.pending_senders.is_empty() && self.batch_filenodes.is_empty())
        {
            return None;
        }
        let err = self
            .encountered_error
            .take()
            .expect("presence checked above");
        // Fail locally queued signals explicitly alongside
        // in-flight ones, so downstream gets the error payload.
        self.pipeline
            .abort_with_pending(err, std::mem::take(&mut self.pending_senders));
        Some(LoopAction::Exit)
    }

    async fn flush_current_batch(&mut self) -> anyhow::Result<()> {
        let batch = std::mem::take(&mut self.batch_filenodes);
        let senders = std::mem::take(&mut self.pending_senders);
        self.spawn_flush(batch, senders).await
    }

    async fn spawn_flush(
        &mut self,
        batch: Vec<HgFileNodeId>,
        senders: DoneSenders,
    ) -> anyhow::Result<()> {
        let batch_len = batch.len();
        let filenodes_es = self.filenodes_es.clone();
        let reponame = self.reponame.clone();
        self.pipeline
            .spawn_flush(0, senders, async move {
                let start = std::time::Instant::now();
                // Log upload failures at the point of failure; the loop logs
                // "Filenodes flush failed" when the watermark surfaces it.
                if let Err(e) = filenodes_es.upload_filenodes(batch).await {
                    tracing::error!("Failed to upload filenodes: {:?}", e);
                    return Err(e);
                }
                tracing::info!(
                    "Uploaded {} filenodes in {}ms",
                    batch_len,
                    start.elapsed().as_millis(),
                );
                STATS::synced_filenodes.add_value(batch_len as i64, (reponame.clone(),));
                Ok(())
            })
            .await
    }

    fn fail(&mut self, err: anyhow::Error) -> LoopAction {
        tracing::error!("Filenodes flush failed: {:?}", err);
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

    fn record_queue_stats(&self, filenodes_recv: &mpsc::Receiver<FileMessage>) {
        tracing::debug!(
            "Filenodes channel capacity: {} max capacity: {} in queue: {}",
            filenodes_recv.capacity(),
            self.config.channel_size,
            filenodes_recv.len()
        );
        STATS::files_queue_capacity.set_value(
            self.ctx.fb,
            filenodes_recv.capacity() as i64,
            (self.reponame.clone(),),
        );
        STATS::files_queue_len.add_value(filenodes_recv.len() as i64, (self.reponame.clone(),));
        STATS::files_queue_max_capacity.set_value(
            self.ctx.fb,
            filenodes_recv.max_capacity() as i64,
            (self.reponame.clone(),),
        );
    }

    fn cancelled(&self) -> bool {
        self.cancellation_requested.load(Ordering::Relaxed)
    }
}

impl Manager for FilenodeManager {
    async fn run(
        &mut self,
        ctx: CoreContext,
        reponame: String,
        filenodes_es: Arc<dyn EdenapiSender + Send + Sync>,
        cancellation_requested: Arc<AtomicBool>,
    ) -> anyhow::Result<()> {
        let Self {
            config,
            filenodes_recv,
        } = self;
        // Filenodes carry bare IDs with unknown sizes, so only the
        // request semaphore bounds this channel; see the byte-pool docs.
        let pipeline = FlushPipeline::new(
            reponame.clone(),
            "filenodes",
            config.max_concurrent_flushes,
            None,
        );
        let mut runner = FilenodeRunner {
            ctx,
            reponame,
            filenodes_es,
            cancellation_requested,
            config: config.clone(),
            pipeline,
            encountered_error: None,
            batch_filenodes: Vec::new(),
            pending_senders: Vec::new(),
        };
        runner.run(filenodes_recv).await
    }
}
