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
use mercurial_types::HgManifestId;
use metaconfig_types::ModernSyncChannelConfig;
use stats::define_stats;
use stats::prelude::*;
use tokio::sync::mpsc;
use tokio::time::interval;

use super::pipeline::DoneSenders;
use super::pipeline::FlushPipeline;
use crate::sender::edenapi::EdenapiSender;
use crate::sender::manager::LoopAction;
use crate::sender::manager::Manager;
use crate::sender::manager::TreeMessage;

define_stats! {
    prefix = "mononoke.modern_sync.manager.tree";

    synced_trees:  dynamic_timeseries("{}.synced_trees", (repo: String); Sum),
    content_wait_time_s:  dynamic_timeseries("{}.content_wait_time_s", (repo: String); Average),

    trees_queue_capacity: dynamic_singleton_counter("{}.trees.queue_capacity", (repo: String)),
    trees_queue_len: dynamic_quantile_stat("{}.trees.queue_len", (repo: String); Average; P 50, P 75, P 95, P 99; Duration::from_secs(60), Duration::from_secs(600), Duration::from_secs(3600)),
    trees_queue_max_capacity: dynamic_singleton_counter("{}.trees.queue_max_capacity", (repo: String)),
}

pub(crate) struct TreeManager {
    config: ModernSyncChannelConfig,
    trees_recv: mpsc::Receiver<TreeMessage>,
}

impl TreeManager {
    pub(crate) fn new(
        config: ModernSyncChannelConfig,
        trees_recv: mpsc::Receiver<TreeMessage>,
    ) -> Self {
        Self { config, trees_recv }
    }
}

struct TreeRunner {
    ctx: CoreContext,
    reponame: String,
    trees_es: Arc<dyn EdenapiSender + Send + Sync>,
    cancellation_requested: Arc<AtomicBool>,
    config: ModernSyncChannelConfig,
    pipeline: FlushPipeline,
    encountered_error: Option<anyhow::Error>,
    batch_trees: Vec<HgManifestId>,
    pending_senders: DoneSenders,
}

impl TreeRunner {
    async fn run(&mut self, trees_recv: &mut mpsc::Receiver<TreeMessage>) -> anyhow::Result<()> {
        let mut timer = interval(Duration::from_millis(self.config.flush_interval_ms as u64));
        while !self.cancelled() {
            let action = tokio::select! {
                msg = trees_recv.recv() => {
                    self.record_queue_stats(trees_recv);
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

    async fn handle_message(&mut self, msg: Option<TreeMessage>) -> LoopAction {
        match msg {
            Some(TreeMessage::WaitForContents(receiver)) => {
                self.note_contents_outcome(receiver).await;
            }
            Some(TreeMessage::Tree(t)) if self.encountered_error.is_none() => {
                self.batch_trees.push(t);
            }
            Some(TreeMessage::TreesDone(sender)) => {
                self.pending_senders.push(sender);
            }
            Some(TreeMessage::Tree(_)) => (),
            None => return self.drain().await,
        }
        // Only non-empty batches may spawn an upload; a zero
        // batch cap must not trigger empty ones. A recorded
        // upstream error winds down here instead of flushing,
        // so Done signals arriving after the error still fail
        // explicitly rather than stranding as dropped channels.
        if !self.batch_trees.is_empty() && self.batch_trees.len() >= self.config.batch_size as usize
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
        if !self.batch_trees.is_empty() {
            if let Err(err) = self.flush_current_batch().await {
                return self.fail(err);
            }
        } else if !self.pending_senders.is_empty() {
            let senders = std::mem::take(&mut self.pending_senders);
            if let Err(e) = self.pipeline.barrier(senders).await {
                tracing::error!("Trees flush failed: {:?}", e);
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
        if !self.batch_trees.is_empty() {
            if let Err(err) = self.flush_current_batch().await {
                return self.fail(err);
            }
        }
        let senders = std::mem::take(&mut self.pending_senders);
        if let Err(e) = self.pipeline.barrier(senders).await {
            tracing::error!("Trees flush failed: {:?}", e);
            return LoopAction::Fail(e);
        }
        LoopAction::Exit
    }

    async fn note_contents_outcome(&mut self, receiver: oneshot::Receiver<anyhow::Result<()>>) {
        // Read outcome from content upload
        let start = std::time::Instant::now();
        match receiver.await {
            Ok(Err(e)) => {
                self.encountered_error.get_or_insert(
                    e.context("Contents error received. Winding down trees sender."),
                );
            }
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
            || (self.pending_senders.is_empty() && self.batch_trees.is_empty())
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
        let batch = std::mem::take(&mut self.batch_trees);
        let senders = std::mem::take(&mut self.pending_senders);
        self.spawn_flush(batch, senders).await
    }

    async fn spawn_flush(
        &mut self,
        batch: Vec<HgManifestId>,
        senders: DoneSenders,
    ) -> anyhow::Result<()> {
        let batch_len = batch.len();
        let trees_es = self.trees_es.clone();
        let reponame = self.reponame.clone();
        self.pipeline
            .spawn_flush(0, senders, async move {
                let start = std::time::Instant::now();
                // Log upload failures at the point of failure; the loop logs
                // "Trees flush failed" when the watermark surfaces it.
                if let Err(e) = trees_es.upload_trees(batch).await {
                    tracing::error!("Failed to upload trees: {:?}", e);
                    return Err(e);
                }
                tracing::info!(
                    "Uploaded {} trees in {}ms",
                    batch_len,
                    start.elapsed().as_millis(),
                );
                STATS::synced_trees.add_value(batch_len as i64, (reponame.clone(),));
                Ok(())
            })
            .await
    }

    fn fail(&mut self, err: anyhow::Error) -> LoopAction {
        tracing::error!("Trees flush failed: {:?}", err);
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

    fn record_queue_stats(&self, trees_recv: &mpsc::Receiver<TreeMessage>) {
        tracing::debug!(
            "Trees channel capacity: {} max capacity: {} in queue: {}",
            trees_recv.capacity(),
            self.config.channel_size,
            trees_recv.len()
        );
        STATS::trees_queue_capacity.set_value(
            self.ctx.fb,
            trees_recv.capacity() as i64,
            (self.reponame.clone(),),
        );
        STATS::trees_queue_len.add_value(trees_recv.len() as i64, (self.reponame.clone(),));
        STATS::trees_queue_max_capacity.set_value(
            self.ctx.fb,
            trees_recv.max_capacity() as i64,
            (self.reponame.clone(),),
        );
    }

    fn cancelled(&self) -> bool {
        self.cancellation_requested.load(Ordering::Relaxed)
    }
}

impl Manager for TreeManager {
    async fn run(
        &mut self,
        ctx: CoreContext,
        reponame: String,
        trees_es: Arc<dyn EdenapiSender + Send + Sync>,
        cancellation_requested: Arc<AtomicBool>,
    ) -> anyhow::Result<()> {
        let Self { config, trees_recv } = self;
        // Trees carry bare IDs with unknown sizes, so only the request
        // semaphore bounds this channel; see the byte-pool docs.
        let pipeline = FlushPipeline::new(
            reponame.clone(),
            "trees",
            config.max_concurrent_flushes,
            None,
        );
        let mut runner = TreeRunner {
            ctx,
            reponame,
            trees_es,
            cancellation_requested,
            config: config.clone(),
            pipeline,
            encountered_error: None,
            batch_trees: Vec::new(),
            pending_senders: Vec::new(),
        };
        runner.run(trees_recv).await
    }
}
