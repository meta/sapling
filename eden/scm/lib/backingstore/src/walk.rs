/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Walk detection and prefetching for one checkout. A `WalkDetector` is created per mount from a
//! `BackingStore` and owns that mount's detector, event worker, prefetch manager and working-copy
//! parent hint. The store stays shared between mounts: the prefetch fetch limits come from it and
//! bound every mount's prefetches together. Neither side holds a reference to the other: every
//! call that reports a read passes the store, and the detector rebuilds its state from it when
//! the store has reloaded its configuration since the state was built.

use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use arc_swap::ArcSwap;
use configloader::Config;
use edenapi::configmodel::ConfigExt;
use parking_lot::Mutex;
use parking_lot::RwLock;
use repo::repo::Repo;
use types::RepoPath;
use types::RepoPathBuf;

use crate::backingstore::BackingStore;
use crate::backingstore::Inner;
use crate::prefetch;
use crate::prefetch::prefetch_manager;

#[derive(Debug, Copy, Clone, PartialEq)]
pub(crate) enum WalkMode {
    // Don't observe walks.
    Off,
    // Watch for walks, but don't take any action.
    Monitor,
    // Prefetch files/trees based on observed walks.
    Prefetch,
}

impl FromStr for WalkMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "monitor" => Ok(Self::Monitor),
            "prefetch" => Ok(Self::Prefetch),
            _ => Ok(Self::Off),
        }
    }
}

/// The walk detection settings of a store's configuration. Read when the store loads and applied
/// to every mount's detector.
#[derive(Clone)]
pub(crate) struct WalkConfig {
    pub(crate) mode: WalkMode,
    threshold: Option<usize>,
    ratio: Option<f64>,
    ratio_sorted_factor: Option<f64>,
    gc_interval: Option<Duration>,
    gc_timeout: Option<Duration>,
    /// Prefetch settings and the store-wide fetch limits, in prefetch mode.
    prefetch: Option<(prefetch::Config, prefetch::Limits)>,
}

impl WalkConfig {
    pub(crate) fn from_config(config: &dyn Config) -> Result<Self> {
        let mode = WalkMode::from_str(
            config
                .get("backingstore", "walk-mode")
                .unwrap_or_default()
                .as_ref(),
        )?;

        let prefetch = if mode == WalkMode::Prefetch {
            let prefetch_config = prefetch::Config {
                // Pause prefetch if ratio of walker reads to prefetches is below min_ratio AND
                // prefetches - reads is greater than max_initial_lag.
                min_ratio: config.get_or("backingstore", "walk-prefetch-min-ratio", || 0.25)?,
                max_initial_lag: config.get_or(
                    "backingstore",
                    "walk-prefetch-max-initial-lag",
                    || 50_000,
                )?,
                min_interval: config.get_or(
                    "backingstore",
                    "walk-prefetch-min-interval",
                    || Duration::from_millis(10),
                )?,
                file_batch_size: config.get_or(
                    "backingstore",
                    "walk-prefetch-file-batch-size",
                    || 2_048,
                )?,
                max_concurrent_file_fetches: config
                    .get_or(
                        "backingstore",
                        "walk-prefetch-max-concurrent-file-fetches",
                        || 4,
                    )?
                    .max(1),
                max_concurrent_manifest_walks: config
                    .get_or(
                        "backingstore",
                        "walk-prefetch-max-concurrent-manifest-walks",
                        || 4,
                    )?
                    .max(1),
                max_concurrent_prefetches: config
                    .get_or(
                        "backingstore",
                        "walk-prefetch-max-concurrent-prefetches",
                        || 4,
                    )?
                    .max(1),
                skip_lfs: config.get_or("backingstore", "walk-prefetch-skip-lfs", || true)?,
            };
            Some((prefetch_config, prefetch::Limits::new(&prefetch_config)))
        } else {
            None
        };

        Ok(Self {
            mode,
            threshold: config.get_opt("backingstore", "walk-threshold")?,
            ratio: config.get_opt("backingstore", "walk-ratio")?,
            ratio_sorted_factor: config.get_opt("backingstore", "walk-ratio-sorted-factor")?,
            gc_interval: config.get_opt("backingstore", "walk-gc-interval")?,
            gc_timeout: config.get_opt("backingstore", "walk-gc-timeout")?,
            prefetch,
        })
    }

    fn apply(&self, detector: &mut walkdetector::Detector) {
        // First reset to default config values to handle the case when a config item was specified
        // in the sl config, but is no longer present (i.e. need to revert to the in-code default).
        detector.reset_config();

        if let Some(threshold) = self.threshold {
            detector.set_walk_threshold(threshold);
        }
        if let Some(ratio) = self.ratio {
            detector.set_walk_ratio(ratio);
        }
        if let Some(factor) = self.ratio_sorted_factor {
            detector.set_walk_ratio_sorted_factor(factor);
        }
        if let Some(interval) = self.gc_interval {
            detector.set_gc_interval(interval);
        }
        if let Some(timeout) = self.gc_timeout {
            detector.set_gc_timeout(timeout);
        }
    }
}

/// A remote read that no active walk covered, reported to the walk detector off the filesystem
/// thread because creating or extending walks takes the detector's write lock.
enum WalkEvent {
    FileLoaded(RepoPathBuf, Option<(usize, usize)>, u32),
    DirLoaded(RepoPathBuf, usize, usize, u32),
    DirMetadata(RepoPathBuf, usize, usize),
}

/// Bound on queued walk events. Walk detection is best effort, so events beyond it are dropped
/// rather than letting a flood of remote reads grow memory.
const WALK_EVENT_QUEUE_LIMIT: usize = 16_384;

/// Spawn the thread that feeds queued walk events to the detector and kicks prefetching when a
/// walk changes. It exits once every sender is dropped.
fn spawn_walk_event_worker(
    detector: walkdetector::Detector,
    prefetch_send: flume::Sender<()>,
) -> crossfire::MTx<crossfire::mpsc::Array<WalkEvent>> {
    let (send, recv) = crossfire::mpsc::bounded_blocking(WALK_EVENT_QUEUE_LIMIT);
    let spawned = std::thread::Builder::new()
        .name("walkdet-events".to_owned())
        .spawn(move || {
            while let Ok(event) = recv.recv() {
                handle_walk_event(&detector, &prefetch_send, event);
            }
        });
    if let Err(err) = spawned {
        tracing::warn!(?err, "failed to spawn walk event worker thread");
    }
    send
}

fn handle_walk_event(
    detector: &walkdetector::Detector,
    prefetch_send: &flume::Sender<()>,
    event: WalkEvent,
) {
    let walk_changed = match event {
        WalkEvent::FileLoaded(path, dir_size, pid) => {
            detector.file_loaded_with_dir_size(&path, dir_size, pid)
        }
        WalkEvent::DirLoaded(path, num_files, num_dirs, pid) => {
            detector.dir_loaded(&path, num_files, num_dirs, pid)
        }
        WalkEvent::DirMetadata(path, num_files, num_dirs) => {
            detector.dir_metadata(&path, num_files, num_dirs);
            false
        }
    };
    if walk_changed {
        let _ = prefetch_send.try_send(());
    }
}

/// The parts of a `WalkDetector` that are rebuilt when the store reloads, tagged with the store
/// generation they were built from.
#[derive(Clone)]
struct State {
    generation: u64,
    mode: WalkMode,
    detector: walkdetector::Detector,
    event_send: crossfire::MTx<crossfire::mpsc::Array<WalkEvent>>,
    prefetch_send: flume::Sender<()>,
}

impl State {
    fn new(
        inner: &Inner,
        mount: &Path,
        parent_hint: Arc<RwLock<Option<String>>>,
        mut detector: walkdetector::Detector,
    ) -> Result<Self> {
        let config = &inner.walk;
        if config.mode == WalkMode::Off {
            return Ok(Self::off(inner.generation, detector));
        }
        config.apply(&mut detector);
        detector.set_root(Some(mount.to_path_buf()));

        let prefetch_send = match &config.prefetch {
            Some((prefetch_config, limits)) => prefetch_manager(
                *prefetch_config,
                inner.repo.tree_resolver()?,
                inner.filestore.clone(),
                parent_hint,
                detector.clone(),
                inner.repo.path().to_path_buf(),
                |root: PathBuf| {
                    let repo = Repo::load(root, &Vec::new())?;
                    let wc = repo.working_copy()?;
                    wc.read().sparse_matcher()
                },
                limits.clone(),
            ),
            // Stick a dummy channel in so we don't need to fuss with Option.
            None => flume::bounded(0).0,
        };
        let event_send = spawn_walk_event_worker(detector.clone(), prefetch_send.clone());

        Ok(Self {
            generation: inner.generation,
            mode: config.mode,
            detector,
            event_send,
            prefetch_send,
        })
    }

    /// The state of a detector that is not watching: no worker and no prefetch manager.
    fn off(generation: u64, detector: walkdetector::Detector) -> Self {
        Self {
            generation,
            mode: WalkMode::Off,
            detector,
            event_send: crossfire::mpsc::bounded_blocking(1).0,
            prefetch_send: flume::bounded(0).0,
        }
    }

    fn send_walk_event(&self, event: WalkEvent) {
        match self.event_send.try_send(event) {
            Ok(()) => {}
            Err(crossfire::TrySendError::Full(_)) => {
                tracing::trace!("walk event queue full; dropping event");
            }
            Err(crossfire::TrySendError::Disconnected(event)) => {
                // The worker thread never started or has exited. Keep detecting walks on this
                // thread rather than stopping silently.
                static WARNED: AtomicBool = AtomicBool::new(false);
                if !WARNED.swap(true, Ordering::Relaxed) {
                    tracing::warn!("walk event worker is gone; handling walk events inline");
                }
                handle_walk_event(&self.detector, &self.prefetch_send, event);
            }
        }
    }

    fn notify_prefetch(&self) {
        if self.mode != WalkMode::Prefetch {
            return;
        }
        let _ = self.prefetch_send.try_send(());
    }
}

/// Walk detection for one mount of a `BackingStore`.
pub struct WalkDetector {
    mount: PathBuf,
    parent_hint: Arc<RwLock<Option<String>>>,
    /// Built from the store's configuration at generation `state.generation`; generation 0,
    /// which no store has, until the first `sync`.
    state: ArcSwap<State>,
    /// Held by the one thread rebuilding `state`; the others keep using the previous state.
    rebuild: Mutex<()>,
}

impl WalkDetector {
    /// A detector that reports nothing until `sync` builds its state from a store.
    pub(crate) fn new(mount: &Path) -> Self {
        Self {
            mount: mount.to_path_buf(),
            parent_hint: Arc::new(RwLock::default()),
            state: ArcSwap::from_pointee(State::off(0, walkdetector::Detector::new())),
            rebuild: Mutex::new(()),
        }
    }

    /// Bring the state in line with `store`: build it on the first call and rebuild it once the
    /// store has reloaded its configuration.
    pub(crate) fn sync(&self, store: &BackingStore) {
        self.state(store);
    }

    /// The state for `store`'s current configuration, rebuilt when the store has reloaded since
    /// the state was built. The detector's walks carry over; the worker, the prefetch manager
    /// and the settings follow the new configuration. The common case is one load and a compare;
    /// while one thread rebuilds, the others get the previous state rather than waiting.
    fn state(&self, store: &BackingStore) -> arc_swap::Guard<Arc<State>> {
        let inner = store.maybe_reload();
        let state = self.state.load();
        if state.generation == inner.generation {
            return state;
        }

        let Some(_rebuild) = self.rebuild.try_lock() else {
            return state;
        };
        let state = self.state.load();
        if state.generation == inner.generation {
            return state;
        }
        let state = State::new(
            &inner,
            &self.mount,
            self.parent_hint.clone(),
            state.detector.clone(),
        )
        .unwrap_or_else(|err| {
            // Keep the previous state until the store reloads again, as the store itself keeps
            // its previous stores when a reload fails.
            tracing::warn!(?err, "error building walk detector state");
            State {
                generation: inner.generation,
                ..(**state).clone()
            }
        });
        self.state.store(Arc::new(state));
        self.state.load()
    }

    /// `dir_size` is the `(num_files, num_dirs)` of the directory containing `path`, if the
    /// caller knows it.
    pub fn witness_file_read(
        &self,
        store: &BackingStore,
        path: &RepoPath,
        local: bool,
        pid: u32,
        dir_size: Option<(usize, usize)>,
    ) {
        let state = self.state(store);

        if state.mode == WalkMode::Off {
            return;
        }

        if local {
            state.detector.file_read(path, pid);
        } else if !state.detector.file_loaded_fast(path, pid) {
            state.send_walk_event(WalkEvent::FileLoaded(path.to_owned(), dir_size, pid));
        }
    }

    /// Record the size of directory `path` whose inode EdenFS loaded. Only sizes large enough
    /// to change walk detection reach the detector; this is not a read of the directory.
    pub fn witness_dir_size(
        &self,
        store: &BackingStore,
        path: &RepoPath,
        num_files: usize,
        num_dirs: usize,
    ) {
        let state = self.state(store);

        if state.mode == WalkMode::Off {
            return;
        }

        if state
            .detector
            .is_dir_metadata_important(num_files, num_dirs)
        {
            state.send_walk_event(WalkEvent::DirMetadata(path.to_owned(), num_files, num_dirs));
        }
    }

    pub fn witness_dir_read(
        &self,
        store: &BackingStore,
        path: &RepoPath,
        local: bool,
        num_files: usize,
        num_dirs: usize,
        pid: u32,
    ) {
        let state = self.state(store);

        if state.mode == WalkMode::Off {
            return;
        }

        if local {
            state.detector.dir_read(path, pid);
            if state
                .detector
                .is_dir_metadata_important(num_files, num_dirs)
            {
                state.send_walk_event(WalkEvent::DirMetadata(path.to_owned(), num_files, num_dirs));
            }
        } else if !state
            .detector
            .dir_loaded_fast(path, num_files, num_dirs, pid)
        {
            state.send_walk_event(WalkEvent::DirLoaded(
                path.to_owned(),
                num_files,
                num_dirs,
                pid,
            ));
        }
    }

    pub fn set_parent_hint(&self, store: &BackingStore, parent_id: &str) {
        tracing::info!(parent_id, "setting parent hint");

        *self.parent_hint.write() = Some(parent_id.to_string());

        self.state(store).notify_prefetch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_walk_event_worker_detects_and_kicks() -> Result<()> {
        let mut detector = walkdetector::Detector::new();
        detector.set_walk_threshold(2);
        let (prefetch_send, prefetch_recv) = flume::bounded(1);
        let send = spawn_walk_event_worker(detector.clone(), prefetch_send);

        for name in ["dir/a", "dir/b"] {
            send.send(WalkEvent::FileLoaded(
                RepoPathBuf::from_string(name.to_string())?,
                None,
                0,
            ))?;
        }
        send.send(WalkEvent::DirLoaded(
            RepoPathBuf::from_string("dir/sub".to_string())?,
            0,
            0,
            0,
        ))?;

        // The second file load forms the walk, which kicks prefetching.
        prefetch_recv.recv_timeout(Duration::from_secs(10))?;
        assert_eq!(
            detector.file_walks(),
            vec![(RepoPathBuf::from_string("dir".to_string())?, 0)]
        );

        Ok(())
    }
}
