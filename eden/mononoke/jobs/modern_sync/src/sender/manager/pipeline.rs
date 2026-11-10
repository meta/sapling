/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Pipelined EdenAPI flushes with bounded concurrency.
//!
//! Each upload manager (contents, trees, filenodes) accumulates items into
//! batches and flushes them to the shadow replica. `FlushPipeline` lets a
//! manager spawn each flush as a background task while bounding worst-case
//! resource use with two semaphores: max concurrent flushes (requests in
//! flight) and max bytes held by in-flight flushes.
//!
//! Done signals complete in order: every flush gets a sequence number,
//! workers report completion over a channel, and the loop advances a
//! watermark, completing each flush's senders only once every earlier flush
//! has completed. A sender therefore fires only after all data received
//! before it is uploaded. The loop awaits completions in its `select!` and
//! drains them after every other event, so an idle loop observes each
//! completion immediately while a busy loop advances in bulk.
//!
//! Deadlock safety rests on three rules, all enforced in this module:
//! - Permits are acquired only here, request-then-bytes, and held only by
//!   workers, which acquire nothing else and await only timeout-bounded IO.
//! - A batch larger than the byte pool runs exclusively instead of waiting
//!   forever: the ledger grants the whole pool to one waiter at a time.
//! - Every worker reports completion on all paths. Panics are caught
//!   explicitly in the worker body (a panicked spawned task's future is
//!   dropped only once its JoinHandle releases, too late for the
//!   watermark), with a Drop guard as the backstop for aborts.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::Result;
use anyhow::anyhow;
use futures::FutureExt;
use futures::channel::oneshot;
use mononoke_macros::mononoke;
use stats::define_stats;
use stats::prelude::*;
use tokio::sync::Notify;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

define_stats! {
    prefix = "mononoke.modern_sync.flush_pipeline";

    flush_in_flight: dynamic_timeseries("{}.{}.flush_in_flight", (repo: String, channel: String); Average),
    bytes_in_flight: dynamic_timeseries("{}.{}.bytes_in_flight", (repo: String, channel: String); Average),
    semaphore_wait_time_ms: dynamic_timeseries("{}.{}.semaphore_wait_time_ms", (repo: String, channel: String); Average),
}

pub(crate) type DoneSenders = Vec<oneshot::Sender<Result<()>>>;

/// Exact byte accounting with no granularity loss and no integer ceiling.
/// Waiters hold nothing while waiting, and holders wait for nothing, so the
/// wait graph is acyclic. A waiter larger than the whole pool is granted the
/// pool exclusively once it drains, instead of waiting forever.
///
/// Each pipeline is driven by a single manager loop, so at most one waiter
/// ever queues here; the wakeup logic is written to stay correct (no lost
/// wakeups, no waiter picked over a fitter one) if that ever changes.
struct ByteLedger {
    pool: u64,
    used: Mutex<u64>,
    changed: Notify,
}

struct ByteGrant {
    ledger: Arc<ByteLedger>,
    amount: u64,
}

impl ByteLedger {
    async fn acquire(self: &Arc<Self>, want: u64) -> ByteGrant {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            // Register as a waiter BEFORE checking: `notified()` only
            // registers on first poll, so without `enable()` a grant
            // dropped between the check and the await would wake nobody
            // (`notify_waiters` leaves no permit) and this waiter would
            // hang until an unrelated later drop.
            notified.as_mut().enable();
            {
                let mut used = self.used.lock().expect("byte ledger poisoned");
                // Either the request fits in the free space, or it exceeds
                // the whole pool and the pool is fully drained for it.
                // Saturating: a wrapped sum must never read as fitting.
                if (*used).saturating_add(want) <= self.pool || (*used == 0 && want > self.pool) {
                    *used += want;
                    return ByteGrant {
                        ledger: self.clone(),
                        amount: want,
                    };
                }
            }
            notified.await;
        }
    }
}

impl Drop for ByteGrant {
    fn drop(&mut self) {
        let mut used = self.ledger.used.lock().expect("byte ledger poisoned");
        debug_assert!(
            *used >= self.amount,
            "byte grant exceeds ledger usage: {} > {used}",
            self.amount
        );
        *used -= self.amount;
        drop(used);
        // Wake every waiter: only some may fit in the freed space, and a
        // single wakeup could pick one that doesn't while a fitter sleeps.
        self.ledger.changed.notify_waiters();
    }
}

struct PendingFlush {
    senders: DoneSenders,
    bytes: u64,
    result: Option<Result<()>>,
}

/// Sends the flush outcome when dropped: the backstop for paths where the
/// worker future is dropped without completing (e.g. task abort). Panics
/// are caught explicitly in the worker body instead: a panicked spawned
/// task's future is only dropped once its JoinHandle releases, which is
/// too late for the watermark to observe.
struct CompletionGuard {
    seq: u64,
    result: Option<Result<()>>,
    completion_tx: mpsc::UnboundedSender<(u64, Result<()>)>,
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        let result = self
            .result
            .take()
            .unwrap_or_else(|| Err(anyhow!("flush worker panicked before reporting completion")));
        let _ = self.completion_tx.send((self.seq, result));
    }
}

pub(crate) struct FlushPipeline {
    labels: (String, String),
    request_sem: Arc<Semaphore>,
    byte_ledger: Option<Arc<ByteLedger>>,
    completion_tx: mpsc::UnboundedSender<(u64, Result<()>)>,
    completion_rx: mpsc::UnboundedReceiver<(u64, Result<()>)>,
    next_seq: u64,
    committed: u64,
    pending: BTreeMap<u64, PendingFlush>,
    handles: Vec<JoinHandle<()>>,
    in_flight_bytes: u64,
    /// Set once the pipeline fails; later spawns and barriers report it
    /// instead of silently running on a spent pipeline.
    failed: Option<String>,
}

impl FlushPipeline {
    pub(crate) fn new(
        repo: String,
        channel: &'static str,
        max_concurrent_flushes: i64,
        max_bytes_in_flight: Option<i64>,
    ) -> Self {
        // A zero-permit request semaphore would hang every acquire, so clamp
        // defensively; the config converter already clamps such values.
        let max_concurrent_flushes = max_concurrent_flushes.max(1) as usize;
        let byte_ledger = max_bytes_in_flight.map(|pool| {
            Arc::new(ByteLedger {
                pool: pool.max(1) as u64,
                used: Mutex::new(0),
                changed: Notify::new(),
            })
        });
        let (completion_tx, completion_rx) = mpsc::unbounded_channel();
        Self {
            labels: (repo, channel.to_string()),
            request_sem: Arc::new(Semaphore::new(max_concurrent_flushes)),
            byte_ledger,
            completion_tx,
            completion_rx,
            next_seq: 0,
            committed: 0,
            pending: BTreeMap::new(),
            handles: Vec::new(),
            in_flight_bytes: 0,
            failed: None,
        }
    }

    fn report_gauges(&self) {
        STATS::flush_in_flight
            .add_value((self.next_seq - self.committed) as i64, self.labels.clone());
        STATS::bytes_in_flight.add_value(self.in_flight_bytes as i64, self.labels.clone());
    }

    /// Spawn `work` as a flush after acquiring request and byte permits.
    /// `bytes` is this flush's byte size; `senders` complete once this flush
    /// and every earlier flush have completed successfully. Returns the prior
    /// failure without spawning when an earlier flush already failed. Both
    /// polls are opportunistic: the first fails fast on known failures
    /// before waiting on permits, the second catches completions that
    /// arrived while waiting. At max 1 the second poll is deterministic:
    /// acquiring the only permit implies the single predecessor ended,
    /// and workers report completion before releasing permits.
    pub(crate) async fn spawn_flush(
        &mut self,
        bytes: u64,
        senders: DoneSenders,
        work: impl futures::Future<Output = Result<(), anyhow::Error>> + Send + 'static,
    ) -> Result<()> {
        if let Some(msg) = &self.failed {
            // Spent pipeline: report the stored failure without spawning.
            // Callers return after aborting, so this guards future misuse.
            drop(senders);
            return Err(anyhow::Error::msg(msg.clone()));
        }
        if let Some(err) = self.poll_completions() {
            // The pipeline already failed and the caller logs the error;
            // drop these senders so downstream winds down silently.
            return Err(err);
        }
        let wait_start = Instant::now();
        let request_permit = self
            .request_sem
            .clone()
            .acquire_owned()
            .await
            .expect("request semaphore is never closed");
        let byte_grant = match &self.byte_ledger {
            Some(ledger) => Some(ledger.acquire(bytes).await),
            None => None,
        };
        STATS::semaphore_wait_time_ms
            .add_value(wait_start.elapsed().as_millis() as i64, self.labels.clone());
        if let Some(err) = self.poll_completions() {
            // The pipeline already failed and the caller logs the error;
            // drop these senders so downstream winds down silently.
            return Err(err);
        }

        let seq = self.next_seq;
        self.next_seq += 1;
        self.pending.insert(
            seq,
            PendingFlush {
                senders,
                bytes,
                result: None,
            },
        );
        self.in_flight_bytes += bytes;
        self.report_gauges();

        let completion_tx = self.completion_tx.clone();
        let handle = mononoke::spawn_task(async move {
            // Declaration order is deliberate: locals drop in reverse, so the
            // guard reports completion before the permits release. At max 1
            // this makes the pre-spawn poll deterministic; `in_flight_bytes`
            // may briefly trail the ledger, which stays the scheduling
            // source of truth.
            let _request_permit = request_permit;
            let _byte_grant = byte_grant;
            let mut guard = CompletionGuard {
                seq,
                result: None,
                completion_tx,
            };
            // Catch panics explicitly: a panicked spawned task's future is
            // dropped only when its JoinHandle releases, so Drop alone would
            // report too late and stall the watermark. This is sound because
            // the worker shares no unguarded mutable state.
            let caught = std::panic::AssertUnwindSafe(work).catch_unwind().await;
            // Surface the panic message for operators; payloads are usually
            // &str or String.
            let outcome: Result<()> = match caught {
                Ok(r) => r,
                Err(payload) => Err(if let Some(s) = payload.downcast_ref::<&str>() {
                    anyhow::Error::msg(format!("flush worker panicked: {s}"))
                } else if let Some(s) = payload.downcast_ref::<String>() {
                    anyhow::Error::msg(format!("flush worker panicked: {s}"))
                } else {
                    anyhow::Error::msg("flush worker panicked")
                }),
            };
            guard.result = Some(outcome);
        });
        self.handles.push(handle);
        Ok(())
    }

    /// The failure stored by an abort, re-reportable to later callers.
    fn stored_failure(&self) -> Option<anyhow::Error> {
        self.failed
            .as_ref()
            .map(|msg| anyhow::Error::msg(msg.clone()))
    }

    /// Record every arrived completion and advance the watermark, completing
    /// each newly committed flush's senders. Returns the stored abort
    /// failure, or the first error from a committed flush or a failed send,
    /// if any.
    pub(crate) fn poll_completions(&mut self) -> Option<anyhow::Error> {
        if let Some(err) = self.stored_failure() {
            return Some(err);
        }
        while let Ok((seq, result)) = self.completion_rx.try_recv() {
            if let Some(pending) = self.pending.get_mut(&seq) {
                pending.result = Some(result);
            }
        }
        // Finished workers stay finished; dropping their handles releases the
        // retained task allocations so a long-running sync cannot grow this.
        self.handles.retain(|h| !h.is_finished());
        self.advance_watermark()
    }

    /// Wait for the next completion, record it, and advance the watermark,
    /// completing each newly committed flush's senders. Returns the stored
    /// abort failure, or the first error from a committed flush or a failed
    /// send, if any. The manager loop awaits this in its `select!` so an
    /// idle loop observes completions immediately; `poll_completions` still
    /// drains arrivals after every other event so a busy loop advances in
    /// bulk.
    pub(crate) async fn wait_for_completion(&mut self) -> Option<anyhow::Error> {
        if let Some(err) = self.stored_failure() {
            return Some(err);
        }
        match self.completion_rx.recv().await {
            Some((seq, result)) => {
                if let Some(pending) = self.pending.get_mut(&seq) {
                    pending.result = Some(result);
                }
                self.poll_completions()
            }
            // Unreachable while `self` is alive: the pipeline holds a
            // completion sender, so the channel cannot close.
            None => None,
        }
    }

    /// Complete `senders` once all in-flight flushes finish. Used for timer
    /// ticks that carry only signals and for final shutdown.
    pub(crate) async fn barrier(&mut self, senders: DoneSenders) -> Result<()> {
        if let Some(msg) = &self.failed {
            drop(senders);
            return Err(anyhow::Error::msg(msg.clone()));
        }
        while self.committed < self.next_seq {
            match self.completion_rx.recv().await {
                Some((seq, result)) => {
                    if let Some(pending) = self.pending.get_mut(&seq) {
                        pending.result = Some(result);
                    }
                    if let Some(err) = self.advance_watermark() {
                        // Preserve the raw error text so the loop's standard
                        // message renders the upload failure verbatim. The
                        // barrier's own senders drop uncompleted: explicit
                        // failures here would log a duplicate downstream
                        // error line per queued signal; downstream treats
                        // the dropped signal as a wind-down.
                        let msg = format!("{err:?}");
                        self.fail_all_pending(&anyhow::Error::msg(msg.clone()), Vec::new());
                        return Err(anyhow::Error::msg(msg));
                    }
                }
                // Unreachable while `self` is alive: the pipeline holds a
                // completion sender, so the channel cannot close. Break
                // rather than hang if it ever does, releasing the barrier
                // senders below.
                None => break,
            }
            self.handles.retain(|h| !h.is_finished());
        }
        Self::complete_senders(senders, None)
    }

    /// Abort all in-flight workers and fail every held sender with `err`.
    /// After this the pipeline is spent; the manager loop must return.
    /// Synchronous: aborting only requests cancellation without waiting.
    pub(crate) fn abort(&mut self, err: &anyhow::Error) {
        self.fail_all_pending(err, Vec::new());
    }

    /// Abort like `abort`, additionally failing `extra` senders that were
    /// queued locally but never attached to a spawned flush. Used on
    /// wind-down so downstream receives the error payload for every
    /// queued signal instead of a bare dropped channel.
    pub(crate) fn abort_with_pending(&mut self, err: anyhow::Error, extra: DoneSenders) {
        self.fail_all_pending(&err, extra);
    }

    fn advance_watermark(&mut self) -> Option<anyhow::Error> {
        // `committed` counts committed flushes, so it is also the next
        // sequence number to commit; flush sequences start at 0.
        let mut first_err = None;
        loop {
            let next = self.committed;
            let complete = matches!(
                self.pending.get(&next),
                Some(PendingFlush {
                    result: Some(_),
                    ..
                })
            );
            if !complete {
                break;
            }
            let pending = self.pending.remove(&next).expect("presence checked above");
            self.committed = next + 1;
            debug_assert!(
                self.in_flight_bytes >= pending.bytes,
                "committed flush exceeds tracked bytes"
            );
            self.in_flight_bytes -= pending.bytes;
            match pending.result.expect("presence checked above") {
                Ok(()) => {
                    // A dropped receiver means downstream is gone and the
                    // caller will abort; still keep committing contiguous
                    // successes so the byte accounting stays exact.
                    if let Err(e) = Self::complete_senders(pending.senders, None) {
                        first_err.get_or_insert(e);
                    }
                }
                Err(err) => {
                    // Drop the failed flush's senders without completing them:
                    // downstream treats a dropped signal as a silent wind-down.
                    // Completing them with an explicit error would add a
                    // downstream error line for a failure the worker and the
                    // loop already report. Propagate the raw upload error
                    // unchanged so the manager's standard message renders the
                    // failure verbatim. Stop here: later flushes' senders must
                    // not fire before this one.
                    drop(pending.senders);
                    first_err.get_or_insert(err);
                    break;
                }
            }
        }
        self.report_gauges();
        first_err
    }

    fn complete_senders(senders: DoneSenders, err: Option<anyhow::Error>) -> Result<()> {
        // A dropped receiver means the downstream manager is gone; fail
        // loudly so the loop aborts instead of losing the signal quietly.
        match err {
            None => {
                // Attempt every send: a dropped receiver must not strand
                // the remaining senders.
                let mut first_err = None;
                for sender in senders {
                    if let Err(e) = sender.send(Ok(())) {
                        first_err.get_or_insert(anyhow!("error sending flush ready: {e:?}"));
                    }
                }
                match first_err {
                    Some(e) => Err(e),
                    None => Ok(()),
                }
            }
            Some(err) => {
                let msg = format!("{err:?}");
                for sender in senders {
                    let _ = sender.send(Err(anyhow!("{msg}")));
                }
                Err(anyhow!("{msg}"))
            }
        }
    }

    fn fail_all_pending(&mut self, err: &anyhow::Error, extra: DoneSenders) {
        for handle in self.handles.drain(..) {
            handle.abort();
        }
        let mut senders = Vec::new();
        // BTreeMap iteration is in sequence order, preserving signal order.
        for (_, pending) in std::mem::take(&mut self.pending) {
            senders.extend(pending.senders);
        }
        senders.extend(extra);
        let msg = format!("{err:?}");
        let _ = Self::complete_senders(senders, Some(anyhow::Error::msg(msg.clone())));
        self.failed = Some(msg);
        self.committed = self.next_seq;
        self.in_flight_bytes = 0;
        self.report_gauges();
    }
}

impl Drop for FlushPipeline {
    fn drop(&mut self) {
        // Backstop: if the manager loop exits without aborting (cancellation),
        // cancel in-flight uploads instead of leaking them. Held senders drop
        // uncompleted; downstream treats the dropped signal as an error.
        for handle in self.handles.drain(..) {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use super::*;

    fn test_pipeline(max_flushes: i64, max_bytes: Option<i64>) -> FlushPipeline {
        FlushPipeline::new("testrepo".to_string(), "test", max_flushes, max_bytes)
    }

    fn done_pair() -> (oneshot::Sender<Result<()>>, oneshot::Receiver<Result<()>>) {
        oneshot::channel()
    }

    /// Wait until a flush failure surfaces. Either worker may report
    /// first, so the error appears after at most both reports.
    async fn expect_flush_error(pipeline: &mut FlushPipeline, msg: &str) -> anyhow::Error {
        match pipeline.wait_for_completion().await {
            Some(err) => err,
            None => pipeline.wait_for_completion().await.expect(msg),
        }
    }

    #[mononoke::test]
    async fn watermark_completes_in_order_despite_out_of_order_finish() -> Result<()> {
        let mut pipeline = test_pipeline(4, None);
        // Two flushes whose workers we release in reverse order.
        let (release_first, first_gate) = oneshot::channel::<()>();
        let (first_tx, first_rx) = done_pair();
        let (second_tx, mut second_rx) = done_pair();

        pipeline
            .spawn_flush(0, vec![first_tx], async move {
                first_gate.await.map_err(|e| anyhow!("{e:?}"))?;
                Ok(())
            })
            .await?;
        pipeline
            .spawn_flush(0, vec![second_tx], async move { Ok(()) })
            .await?;

        // Let the second worker finish while the first is still gated.
        assert!(pipeline.wait_for_completion().await.is_none());
        // Watermark holds: the second sender must not fire before the first.
        assert!(second_rx.try_recv()?.is_none());

        release_first
            .send(())
            .map_err(|_| anyhow!("gate dropped"))?;
        assert!(pipeline.wait_for_completion().await.is_none());
        first_rx.await??;
        second_rx.await??;
        Ok(())
    }

    #[mononoke::test]
    async fn failed_flush_drops_its_senders_and_wind_down_fails_the_rest() -> Result<()> {
        let mut pipeline = test_pipeline(4, None);
        let (first_tx, first_rx) = done_pair();
        let (second_tx, second_rx) = done_pair();

        pipeline
            .spawn_flush(0, vec![first_tx], async move { Err(anyhow!("boom")) })
            .await?;
        pipeline
            .spawn_flush(0, vec![second_tx], async move { Ok(()) })
            .await?;

        let err = expect_flush_error(&mut pipeline, "failed flush must surface").await;
        assert!(format!("{err:?}").contains("boom"));
        // The failed flush's senders drop uncompleted; downstream treats
        // the dropped signal as a silent wind-down.
        assert!(first_rx.await.is_err());
        // Winding down fails everything still held, explicitly, including
        // locally queued signals never attached to a flush.
        let (extra_tx, extra_rx) = done_pair();
        pipeline.abort_with_pending(err, vec![extra_tx]);
        assert!(second_rx.await?.is_err());
        assert!(extra_rx.await?.is_err());
        Ok(())
    }

    #[mononoke::test]
    async fn spawn_after_failure_drops_new_senders() -> Result<()> {
        let mut pipeline = test_pipeline(4, None);
        let (first_tx, first_rx) = done_pair();
        let (second_tx, second_rx) = done_pair();

        pipeline
            .spawn_flush(0, vec![first_tx], async move { Err(anyhow!("boom")) })
            .await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        // The pipeline already failed: this spawn returns the prior error
        // without uploading, and drops the new senders for a silent
        // downstream wind-down.
        let err = pipeline
            .spawn_flush(0, vec![second_tx], async move { Ok(()) })
            .await
            .expect_err("spawn after failure must surface the prior error");
        assert!(format!("{err:?}").contains("boom"));
        assert!(first_rx.await.is_err());
        assert!(second_rx.await.is_err());
        Ok(())
    }

    #[mononoke::test]
    async fn request_semaphore_serializes_when_max_is_one() -> Result<()> {
        let mut pipeline = test_pipeline(1, None);
        let running = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let (release, gate) = oneshot::channel::<()>();
        let gate = Arc::new(tokio::sync::Mutex::new(Some(gate)));

        // First flush holds the only permit behind a gate ...
        {
            let running = running.clone();
            let max_seen = max_seen.clone();
            let gate = gate.clone();
            pipeline
                .spawn_flush(0, vec![], async move {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    if let Some(gate) = gate.lock().await.take() {
                        gate.await.map_err(|e| anyhow!("{e:?}"))?;
                    }
                    running.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
                .await?;
        }
        // ... so the second spawn waits for a permit; release the gate from
        // a separate task so the test itself never blocks on it.
        let releaser = mononoke::spawn_task(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = release.send(());
        });
        {
            let running = running.clone();
            let max_seen = max_seen.clone();
            let gate = gate.clone();
            pipeline
                .spawn_flush(0, vec![], async move {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    if let Some(gate) = gate.lock().await.take() {
                        gate.await.map_err(|e| anyhow!("{e:?}"))?;
                    }
                    running.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
                .await?;
        }
        releaser.await.map_err(|e| anyhow!("{e:?}"))?;
        pipeline.barrier(vec![]).await?;
        assert_eq!(max_seen.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[mononoke::test]
    async fn oversized_batch_runs_exclusively_instead_of_deadlocking() -> Result<()> {
        // Pool of 100 bytes; a 10_000-byte batch must still run (alone).
        let mut pipeline = test_pipeline(4, Some(100));
        let (tx, rx) = done_pair();
        pipeline
            .spawn_flush(10_000, vec![tx], async move { Ok(()) })
            .await?;
        pipeline.barrier(vec![]).await?;
        rx.await??;
        Ok(())
    }

    #[mononoke::test]
    async fn barrier_completes_signal_only_senders_after_quiescence() -> Result<()> {
        let mut pipeline = test_pipeline(4, None);
        let (release, gate) = oneshot::channel::<()>();
        let (inflight_tx, inflight_rx) = done_pair();
        let (barrier_tx, barrier_rx) = done_pair();

        pipeline
            .spawn_flush(0, vec![inflight_tx], async move {
                gate.await.map_err(|e| anyhow!("{e:?}"))?;
                Ok(())
            })
            .await?;

        let barrier_done = mononoke::spawn_task(barrier_rx);
        // Barrier must wait: nothing completes while the flush is gated.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!barrier_done.is_finished());

        release.send(()).map_err(|_| anyhow!("gate dropped"))?;
        pipeline.barrier(vec![barrier_tx]).await?;
        inflight_rx.await??;
        barrier_done.await???;
        Ok(())
    }

    #[mononoke::test]
    async fn worker_panic_surfaces_as_flush_error() -> Result<()> {
        let mut pipeline = test_pipeline(4, None);
        let (tx, rx) = done_pair();
        pipeline
            .spawn_flush(0, vec![tx], async move {
                panic!("simulated worker panic");
                #[allow(unreachable_code)]
                Ok(())
            })
            .await?;
        let err = pipeline
            .wait_for_completion()
            .await
            .expect("panicking worker must surface");
        assert!(format!("{err:?}").contains("simulated worker panic"));
        // The panicked flush's sender drops uncompleted, like any failed flush.
        assert!(rx.await.is_err());
        Ok(())
    }

    #[mononoke::test]
    async fn send_failure_still_commits_later_successes() -> Result<()> {
        let mut pipeline = test_pipeline(4, None);
        let (first_tx, first_rx) = done_pair();
        let (first_extra_tx, first_extra_rx) = done_pair();
        let (second_tx, mut second_rx) = done_pair();

        pipeline
            .spawn_flush(0, vec![first_tx, first_extra_tx], async move { Ok(()) })
            .await?;
        pipeline
            .spawn_flush(0, vec![second_tx], async move { Ok(()) })
            .await?;

        // Drop the first receiver: completing it fails, but the surviving
        // sender of the same flush still fires, and the watermark still
        // commits the second flush.
        drop(first_rx);
        let err = expect_flush_error(&mut pipeline, "send failure must surface").await;
        assert!(format!("{err:?}").contains("error sending flush ready"));
        first_extra_rx.await??;
        // The second flush commits after at most one more report,
        // whichever order the two reports arrived in.
        let second_result = loop {
            if let Some(result) = second_rx.try_recv()? {
                break result;
            }
            pipeline.wait_for_completion().await;
        };
        second_result?;
        Ok(())
    }

    #[mononoke::test]
    async fn byte_ledger_waiter_wakes_when_grant_drops() -> Result<()> {
        use std::sync::atomic::AtomicBool;

        let ledger = Arc::new(ByteLedger {
            pool: 100,
            used: Mutex::new(0),
            changed: Notify::new(),
        });
        let grant = ledger.acquire(100).await;
        let woken = Arc::new(AtomicBool::new(false));
        let woken_clone = woken.clone();
        let ledger_clone = ledger.clone();
        let waiter = mononoke::spawn_task(async move {
            let _grant = ledger_clone.acquire(100).await;
            woken_clone.store(true, Ordering::SeqCst);
        });
        // The waiter cannot proceed while the pool is fully held.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!woken.load(Ordering::SeqCst));
        drop(grant);
        waiter.await.map_err(|e| anyhow!("{e:?}"))?;
        assert!(woken.load(Ordering::SeqCst));
        Ok(())
    }

    #[mononoke::test]
    async fn barrier_surfaces_inflight_failure_and_drops_its_senders() -> Result<()> {
        let mut pipeline = test_pipeline(4, None);
        let (first_tx, first_rx) = done_pair();
        let (second_tx, second_rx) = done_pair();
        let (barrier_tx, barrier_rx) = done_pair();

        pipeline
            .spawn_flush(0, vec![first_tx], async move { Err(anyhow!("boom")) })
            .await?;
        pipeline
            .spawn_flush(0, vec![second_tx], async move { Ok(()) })
            .await?;

        let err = pipeline
            .barrier(vec![barrier_tx])
            .await
            .expect_err("barrier must surface the inflight failure");
        assert!(format!("{err:?}").contains("boom"));
        // The failed flush's senders and the barrier's own senders drop
        // for a silent downstream wind-down; held later senders fail
        // explicitly.
        assert!(first_rx.await.is_err());
        assert!(barrier_rx.await.is_err());
        assert!(second_rx.await?.is_err());
        Ok(())
    }

    #[mononoke::test]
    async fn spawn_and_barrier_after_abort_report_stored_failure() -> Result<()> {
        use std::sync::atomic::AtomicBool;

        let mut pipeline = test_pipeline(4, None);
        let ran = Arc::new(AtomicBool::new(false));
        pipeline.abort(&anyhow!("boom"));

        // Spawn on a spent pipeline reports the stored failure without
        // running the work, and drops the new senders.
        let ran_clone = ran.clone();
        let (spawn_tx, spawn_rx) = done_pair();
        let err = pipeline
            .spawn_flush(0, vec![spawn_tx], async move {
                ran_clone.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await
            .expect_err("spawn after abort must fail");
        assert!(format!("{err:?}").contains("boom"));
        assert!(!ran.load(Ordering::SeqCst));
        assert!(spawn_rx.await.is_err());

        // Barrier on a spent pipeline reports the stored failure too
        // instead of signaling success.
        let (barrier_tx, barrier_rx) = done_pair();
        let err = pipeline
            .barrier(vec![barrier_tx])
            .await
            .expect_err("barrier after abort must fail");
        assert!(format!("{err:?}").contains("boom"));
        assert!(barrier_rx.await.is_err());
        Ok(())
    }

    #[mononoke::test]
    async fn poll_and_wait_after_abort_report_stored_failure() -> Result<()> {
        let mut pipeline = test_pipeline(4, None);
        pipeline.abort(&anyhow!("boom"));

        // Polling a spent pipeline reports the stored failure instead of
        // success, and waiting reports it instead of hanging on a channel
        // that can no longer deliver.
        let err = pipeline
            .poll_completions()
            .expect("poll after abort must surface the stored failure");
        assert!(format!("{err:?}").contains("boom"));
        let err = pipeline
            .wait_for_completion()
            .await
            .expect("wait after abort must surface the stored failure");
        assert!(format!("{err:?}").contains("boom"));
        Ok(())
    }

    #[mononoke::test]
    async fn byte_ledger_wakes_multiple_waiters_without_loss() -> Result<()> {
        use std::sync::atomic::AtomicBool;

        let ledger = Arc::new(ByteLedger {
            pool: 100,
            used: Mutex::new(0),
            changed: Notify::new(),
        });
        let grant = ledger.acquire(100).await;
        let first_done = Arc::new(AtomicBool::new(false));
        let second_done = Arc::new(AtomicBool::new(false));

        // Two waiters queue while the pool is fully held; whichever acquires
        // first holds 60, so the other must wait for its release too. A lost
        // wakeup would hang this test.
        let waiter = |ledger: Arc<ByteLedger>, done: Arc<AtomicBool>| {
            mononoke::spawn_task(async move {
                let _grant = ledger.acquire(60).await;
                done.store(true, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
            })
        };
        let first = waiter(ledger.clone(), first_done.clone());
        let second = waiter(ledger.clone(), second_done.clone());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!first_done.load(Ordering::SeqCst));
        assert!(!second_done.load(Ordering::SeqCst));
        drop(grant);
        first.await.map_err(|e| anyhow!("{e:?}"))?;
        second.await.map_err(|e| anyhow!("{e:?}"))?;
        assert!(first_done.load(Ordering::SeqCst));
        assert!(second_done.load(Ordering::SeqCst));
        Ok(())
    }

    #[mononoke::test]
    async fn max_one_spawn_observes_prior_failure_without_polling_delay() -> Result<()> {
        // At max 1, the second spawn blocks on the only permit until the
        // first worker ends; workers report completion before releasing
        // permits, so the post-acquire poll observes the failure
        // deterministically, with no sleep.
        let mut pipeline = test_pipeline(1, None);
        let (first_tx, first_rx) = done_pair();
        let (second_tx, second_rx) = done_pair();

        pipeline
            .spawn_flush(0, vec![first_tx], async move { Err(anyhow!("boom")) })
            .await?;
        let err = pipeline
            .spawn_flush(0, vec![second_tx], async move { Ok(()) })
            .await
            .expect_err("second spawn must surface the prior failure");
        assert!(format!("{err:?}").contains("boom"));
        assert!(first_rx.await.is_err());
        assert!(second_rx.await.is_err());
        Ok(())
    }

    #[mononoke::test]
    async fn wait_for_completion_observes_success_without_polling() -> Result<()> {
        // No `poll_completions` call anywhere: the wait observes the
        // completion directly and commits the flush's senders.
        let mut pipeline = test_pipeline(4, None);
        let (tx, rx) = done_pair();

        pipeline
            .spawn_flush(0, vec![tx], async move { Ok(()) })
            .await?;
        assert!(pipeline.wait_for_completion().await.is_none());
        rx.await??;
        Ok(())
    }

    #[mononoke::test]
    async fn wait_for_completion_drains_every_arrived_completion() -> Result<()> {
        let mut pipeline = test_pipeline(4, None);
        let (first_tx, first_rx) = done_pair();
        let (second_tx, second_rx) = done_pair();

        pipeline
            .spawn_flush(0, vec![first_tx], async move { Ok(()) })
            .await?;
        pipeline
            .spawn_flush(0, vec![second_tx], async move { Ok(()) })
            .await?;

        // Both workers finish near-instantly; a single wait records the
        // first arrival and drains the second before advancing the
        // watermark, so both senders fire.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(pipeline.wait_for_completion().await.is_none());
        first_rx.await??;
        second_rx.await??;
        Ok(())
    }

    #[mononoke::test]
    async fn wait_for_completion_surfaces_failure_and_drops_its_senders() -> Result<()> {
        let mut pipeline = test_pipeline(4, None);
        let (tx, rx) = done_pair();

        pipeline
            .spawn_flush(0, vec![tx], async move { Err(anyhow!("boom")) })
            .await?;
        let err = pipeline
            .wait_for_completion()
            .await
            .expect("failed flush must surface");
        assert!(format!("{err:?}").contains("boom"));
        // The failed flush's senders drop uncompleted for a silent
        // downstream wind-down.
        assert!(rx.await.is_err());
        Ok(())
    }
}
