/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Periodic progress lines on stderr for long-running phases.
//!
//! Result lines stay on stdout, so progress never interleaves with output
//! that callers parse.

use std::array;
use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use clap::Args;
use mononoke_macros::mononoke;
use strum::Display;
use strum::EnumCount;
use strum::EnumIter;
use strum::IntoEnumIterator;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const DEFAULT_INTERVAL_SECS: u64 = 10;

#[derive(Args, Clone, Debug)]
pub struct ProgressOptions {
    /// Seconds between progress lines on stderr.  Defaults to 10 when stderr
    /// is a terminal and to none otherwise; 0 disables them
    #[clap(long)]
    progress_interval: Option<u64>,
}

impl ProgressOptions {
    fn interval(&self) -> Option<Duration> {
        let secs = match self.progress_interval {
            Some(secs) => secs,
            None if std::io::stderr().is_terminal() => DEFAULT_INTERVAL_SECS,
            None => 0,
        };
        (secs > 0).then(|| Duration::from_secs(secs))
    }

    /// Starts reporting `phase` until the returned reporter is finished or
    /// dropped.
    pub fn start(&self, phase: Phase) -> Reporter {
        Reporter::start(Arc::new(phase), self.interval())
    }
}

/// A store operation counted by the store adapters, displayed as it is
/// reported.
#[derive(Clone, Copy, Debug, Display, EnumCount, EnumIter, PartialEq, Eq)]
pub enum StoreOp {
    #[strum(to_string = "pages listed")]
    PageListed,
    #[strum(to_string = "keys seen")]
    KeySeen,
    #[strum(to_string = "values read")]
    ValueRead,
    #[strum(to_string = "keys unlinked")]
    KeyUnlinked,
}

type StoreCounts = [u64; StoreOp::COUNT];

/// Store operations made by this process.
pub struct StoreStats {
    counts: [AtomicU64; StoreOp::COUNT],
}

pub static STORE_STATS: StoreStats = StoreStats {
    counts: [const { AtomicU64::new(0) }; StoreOp::COUNT],
};

impl StoreStats {
    pub fn record(&self, op: StoreOp) {
        self.add(op, 1);
    }

    pub fn add(&self, op: StoreOp, n: usize) {
        self.counts[op as usize].fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    fn snapshot(&self) -> StoreCounts {
        array::from_fn(|op| self.counts[op].load(Ordering::Relaxed))
    }
}

/// A phase of a command: units of work done out of a known total, plus
/// named outcome counts.
pub struct Phase {
    name: &'static str,
    unit: &'static str,
    total: u64,
    done: AtomicU64,
    counts: Vec<(&'static str, AtomicU64)>,
    started: Instant,
    store_baseline: StoreCounts,
}

impl Phase {
    /// A phase of `total` units, with outcome counts named `counts`.
    pub fn new(
        name: &'static str,
        unit: &'static str,
        total: usize,
        counts: &[&'static str],
    ) -> Self {
        Self {
            name,
            unit,
            total: u64::try_from(total).unwrap_or(u64::MAX),
            done: AtomicU64::new(0),
            counts: counts
                .iter()
                .map(|name| (*name, AtomicU64::new(0)))
                .collect(),
            started: Instant::now(),
            store_baseline: STORE_STATS.snapshot(),
        }
    }

    pub fn unit_done(&self) {
        self.done.fetch_add(1, Ordering::Relaxed);
    }

    /// Records one unit done with outcome `count`, which must be one of the
    /// phase's named counts.
    pub fn unit_done_as(&self, count: &str) {
        self.unit_done();
        self.add(count, 1);
    }

    /// Records one unit done, counting a failure as `failed`, which must be
    /// one of the phase's named counts.
    pub fn record<T, E>(&self, result: &Result<T, E>) {
        match result {
            Ok(_) => self.unit_done(),
            Err(_) => self.unit_done_as("failed"),
        }
    }

    /// Adds `n` to the outcome count `count`, which must be one of the
    /// phase's named counts.
    pub fn add(&self, count: &str, n: u64) {
        let slot = self.counts.iter().find(|(name, _)| *name == count);
        debug_assert!(slot.is_some(), "phase {} has no count {count}", self.name);
        if let Some((_, value)) = slot {
            value.fetch_add(n, Ordering::Relaxed);
        }
    }

    fn line(&self) -> String {
        let store = STORE_STATS.snapshot();
        let since_start = array::from_fn(|op| store[op] - self.store_baseline[op]);
        self.render(self.started.elapsed(), since_start)
    }

    fn render(&self, elapsed: Duration, store: StoreCounts) -> String {
        let done = self.done.load(Ordering::Relaxed);
        let percent = done
            .saturating_mul(100)
            .checked_div(self.total)
            .unwrap_or(100);
        let mut line = format!(
            "{}: {done}/{} {} ({percent}%), {} elapsed",
            self.name,
            self.total,
            self.unit,
            format_elapsed(elapsed),
        );
        if !self.counts.is_empty() {
            let counts: Vec<String> = self
                .counts
                .iter()
                .map(|(name, value)| format!("{name} {}", value.load(Ordering::Relaxed)))
                .collect();
            line.push_str("; ");
            line.push_str(&counts.join(", "));
        }
        let operations: Vec<String> = StoreOp::iter()
            .zip(store)
            .filter(|(_, n)| *n > 0)
            .map(|(op, n)| format!("{n} {op}"))
            .collect();
        if !operations.is_empty() {
            line.push_str("; ");
            line.push_str(&operations.join(", "));
        }
        line
    }
}

fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match (secs / 3600, secs / 60 % 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m{s:02}s"),
        (h, m, s) => format!("{h}h{m:02}m{s:02}s"),
    }
}

/// Prints a phase's progress line every interval, and once more when the
/// phase finishes.
pub struct Reporter {
    phase: Arc<Phase>,
    ticker: Option<(oneshot::Sender<()>, JoinHandle<()>)>,
}

impl Reporter {
    fn start(phase: Arc<Phase>, interval: Option<Duration>) -> Self {
        let ticker = interval.map(|interval| {
            let (stop, mut stopped) = oneshot::channel::<()>();
            let ticking = phase.clone();
            let handle = mononoke::spawn_task(async move {
                let mut ticks =
                    tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
                loop {
                    tokio::select! {
                        _ = ticks.tick() => eprintln!("{}", ticking.line()),
                        // Also resolves when the reporter is dropped.
                        _ = &mut stopped => break,
                    }
                }
            });
            (stop, handle)
        });
        Self { phase, ticker }
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    /// Stops the periodic lines and prints the phase's final line.
    pub async fn finish(mut self) {
        if let Some((stop, handle)) = self.ticker.take() {
            let _ = stop.send(());
            let _ = handle.await;
            eprintln!("{} (finished)", self.phase.line());
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    fn store_counts(counts: &[(StoreOp, u64)]) -> StoreCounts {
        counts
            .iter()
            .fold(StoreCounts::default(), |mut all, (op, n)| {
                all[*op as usize] = *n;
                all
            })
    }

    #[mononoke::test]
    fn renders_units_counts_and_store_operations() {
        let phase = Phase::new("scan --exists", "cells", 27, &["present", "absent"]);
        phase.unit_done_as("present");
        phase.record(&Ok::<_, ()>(()));
        phase.unit_done_as("absent");
        let store = store_counts(&[(StoreOp::PageListed, 40), (StoreOp::KeySeen, 1200)]);
        assert_eq!(
            phase.render(Duration::from_secs(185), store),
            "scan --exists: 3/27 cells (11%), 3m05s elapsed; present 1, absent 1; 40 pages listed, 1200 keys seen"
        );
    }

    #[mononoke::test]
    fn failures_are_counted() {
        let phase = Phase::new("apply", "partitions", 2, &["failed"]);
        phase.record(&Err::<(), _>("partition failed"));
        phase.record(&Ok::<_, ()>(()));
        assert_eq!(
            phase.render(Duration::from_secs(1), StoreCounts::default()),
            "apply: 2/2 partitions (100%), 1s elapsed; failed 1"
        );
    }

    #[mononoke::test]
    fn omits_store_operations_that_did_not_happen() {
        let phase = Phase::new("metadata reset", "repos", 2, &[]);
        phase.unit_done();
        assert_eq!(
            phase.render(Duration::from_secs(5), StoreCounts::default()),
            "metadata reset: 1/2 repos (50%), 5s elapsed"
        );
        let unlinking = store_counts(&[(StoreOp::KeyUnlinked, 7), (StoreOp::ValueRead, 3)]);
        assert_eq!(
            phase.render(Duration::from_secs(3725), unlinking),
            "metadata reset: 1/2 repos (50%), 1h02m05s elapsed; 3 values read, 7 keys unlinked"
        );
    }

    #[mononoke::test]
    fn an_empty_phase_is_complete() {
        let phase = Phase::new("apply", "partitions", 0, &[]);
        assert!(
            phase
                .render(Duration::ZERO, StoreCounts::default())
                .starts_with("apply: 0/0 partitions (100%)"),
            "nothing to do is reported as done, not as a division by zero"
        );
    }

    #[mononoke::test]
    fn zero_or_absent_interval_disables_reporting_off_a_terminal() {
        let off = ProgressOptions {
            progress_interval: Some(0),
        };
        assert_eq!(off.interval(), None, "0 disables progress lines");
        let every_three = ProgressOptions {
            progress_interval: Some(3),
        };
        assert_eq!(every_three.interval(), Some(Duration::from_secs(3)));
    }

    #[mononoke::fbinit_test]
    async fn reporter_stops_when_finished(_fb: fbinit::FacebookInit) {
        let reporter = Reporter::start(
            Arc::new(Phase::new("test", "units", 1, &[])),
            Some(Duration::from_secs(3600)),
        );
        reporter.phase().unit_done();
        tokio::time::timeout(Duration::from_secs(10), reporter.finish())
            .await
            .expect("finish should stop the ticker promptly");
    }
}
