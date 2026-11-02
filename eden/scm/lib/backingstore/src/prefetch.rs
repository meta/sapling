/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use manifest::Manifest;
use manifest_tree::TreeManifest;
use parking_lot::RwLock;
use pathmatcher::DepthMatcher;
use pathmatcher::DirectoryMatch;
use pathmatcher::DynMatcher;
use pathmatcher::IntersectMatcher;
use pathmatcher::Matcher;
use pathmatcher::UnionMatcher;
use repo::TreeResolver;
use storemodel::FileStore;
use types::FetchContext;
use types::HgId;
use types::Key;
use types::RepoPath;
use types::RepoPathBuf;
use types::fetch_cause::FetchCause;
use types::fetch_mode::FetchMode;
use walkdetector::WalkType;

macro_rules! info_if {
    ($cond:expr, $kind:ident, $($rest:tt)*) => {
        if $cond {
            tracing::$kind!(tracing::Level::INFO, $($rest)*)
        } else {
            tracing::$kind!(tracing::Level::DEBUG, $($rest)*)
        }
    };
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Config {
    pub(crate) file_batch_size: usize,
    /// Maximum outstanding file-fetch batches across all prefetches.
    pub(crate) max_concurrent_file_fetches: usize,
    /// Maximum concurrent manifest walks across all prefetches.
    pub(crate) max_concurrent_manifest_walks: usize,
    /// Maximum active logical prefetches across roots and shallow batches.
    pub(crate) max_concurrent_prefetches: usize,
    pub(crate) max_initial_lag: u64,
    pub(crate) min_ratio: f64,
    pub(crate) min_interval: Duration,
    pub(crate) skip_lfs: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            file_batch_size: 4_096,
            max_concurrent_file_fetches: 4,
            max_concurrent_manifest_walks: 4,
            max_concurrent_prefetches: 4,
            max_initial_lag: 1000,
            min_ratio: 0.1,
            min_interval: Duration::from_millis(10),
            skip_lfs: true,
        }
    }
}

/// How often the manager checks whether a paused prefetch can resume. Kicks only arrive when
/// walks change, and a walker catching up changes nothing the detector reports.
const RESUME_CHECK_INTERVAL: Duration = Duration::from_millis(100);
/// How often a prefetch waiting for a fetch permit checks whether it was canceled.
const PERMIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Directory walks at most this deep are prefetched together, this many per thread, since each
/// one is small.
const SHALLOW_DIR_DEPTH: usize = 2;
const SHALLOW_DIR_BATCH: usize = 100;

/// Walks at least this deep are rare and expensive enough to log at INFO. Shallow ones, including
/// the batches of shallow directory walks, are routine and logged at DEBUG, where the span gives
/// the same context.
const INTERESTING_DEPTH: usize = 10;

type SparseMatcherResult = anyhow::Result<Option<DynMatcher>>;

/// Launch the prefetch manager thread. It starts file and directory prefetches for the walks the
/// detector reports, when kicked through the returned channel or once a second otherwise, so
/// prefetching makes serial walks faster. The manager and every prefetch it started are canceled
/// when all copies of the channel are dropped.
pub(crate) fn prefetch_manager(
    config: Config,
    tree_resolver: Arc<dyn TreeResolver>,
    file_store: Arc<dyn FileStore>,
    current_commit_id: Arc<RwLock<Option<String>>>,
    detector: walkdetector::Detector,
    repo_root: PathBuf,
    make_matcher: impl Fn(PathBuf) -> SparseMatcherResult + Send + 'static,
    limits: Limits,
) -> flume::Sender<()> {
    // We don't need to queue up lots of kicks. A single one will suffice.
    let (send, recv) = flume::bounded(1);
    let manager = Manager {
        config,
        tree_resolver,
        file_store,
        current_commit_id,
        detector,
        repo_root,
        make_matcher: Box::new(make_matcher),
        limits,
        commit: None,
        prefetches: Prefetches::default(),
        last_tick: None,
    };
    std::thread::spawn(move || manager.run(recv));
    send
}

struct Manager {
    config: Config,
    tree_resolver: Arc<dyn TreeResolver>,
    file_store: Arc<dyn FileStore>,
    current_commit_id: Arc<RwLock<Option<String>>>,
    detector: walkdetector::Detector,
    repo_root: PathBuf,
    make_matcher: Box<dyn Fn(PathBuf) -> SparseMatcherResult + Send>,
    limits: Limits,
    commit: Option<CommitState>,
    prefetches: Prefetches,
    last_tick: Option<Instant>,
}

/// What the manager knows about the commit it is prefetching for.
struct CommitState {
    id: HgId,
    /// Shared by every prefetch so trees are fetched and parsed once. Dropped while no walk is
    /// active, since it can hold a lot of trees.
    manifest: Option<TreeManifest>,
    sparse_matcher: Option<DynMatcher>,
}

/// Shared limits on the work prefetches do concurrently.
/// The concurrency limits of a store's prefetches. Clones share the permits, so every prefetch
/// manager fetching through one store is bounded together.
#[derive(Clone)]
pub(crate) struct Limits {
    manifest_walks: ConcurrencyLimiter,
    file_fetches: ConcurrencyLimiter,
}

impl Limits {
    pub(crate) fn new(config: &Config) -> Self {
        Self {
            manifest_walks: ConcurrencyLimiter::new(config.max_concurrent_manifest_walks),
            file_fetches: ConcurrencyLimiter::new(config.max_concurrent_file_fetches),
        }
    }
}

/// The walks the detector reports, by root. A root reports at most one walk of each kind, at the
/// deepest depth seen so far.
#[derive(Default)]
struct ActiveWalks {
    files: HashMap<RepoPathBuf, usize>,
    dirs: HashMap<RepoPathBuf, usize>,
}

impl ActiveWalks {
    fn from_detector(detector: &walkdetector::Detector) -> Self {
        let mut walks = Self::default();
        for (root, depth, kind) in detector.all_walks() {
            match kind {
                WalkType::File => walks.files.insert(root, depth),
                WalkType::Directory => walks.dirs.insert(root, depth),
            };
        }
        walks
    }

    fn is_empty(&self) -> bool {
        self.files.is_empty() && self.dirs.is_empty()
    }
}

/// Prefetches in flight and the walks already handled.
#[derive(Default)]
struct Prefetches {
    /// One file prefetch per walk root, with the depth it covers. A deeper walk replaces it.
    files: HashMap<RepoPathBuf, (usize, PrefetchHandle)>,
    /// Directory prefetches per walk root, by depth. Prefetches at different depths overlap.
    dirs: HashMap<RepoPathBuf, Vec<(usize, PrefetchHandle)>>,
    /// Batches of shallow directory walks. A batch cannot be canceled for one walk, so these are
    /// not tied to walks.
    dir_batches: Vec<PrefetchHandle>,
    /// Walks whose prefetch was started, so it is not started again while the walk is unchanged.
    handled_files: HashSet<(RepoPathBuf, usize)>,
    handled_dirs: HashSet<(RepoPathBuf, usize)>,
}

impl Prefetches {
    /// Logical prefetches in flight, which `max_concurrent_prefetches` bounds. A paused file
    /// prefetch has no thread and holds no slot.
    fn active(&self) -> usize {
        self.files
            .values()
            .filter(|(_, handle)| !handle.is_paused())
            .count()
            + self.dirs.len()
            + self.dir_batches.len()
    }

    fn any_paused(&self) -> bool {
        self.files.values().any(|(_, handle)| handle.is_paused())
    }

    /// Forget prefetches that finished and cancel those whose walk ended, so a walk that comes
    /// back is prefetched again rather than assumed handled.
    fn reconcile(&mut self, walks: &ActiveWalks) {
        self.handled_files
            .retain(|(root, depth)| walks.files.get(root) == Some(depth));
        self.handled_dirs
            .retain(|(root, depth)| walks.dirs.get(root) == Some(depth));
        self.files.retain(|root, (_, handle)| {
            if !walks.files.contains_key(root) {
                tracing::debug!(%root, "canceling file prefetch with no corresponding walk");
                return false;
            }
            !handle.is_done()
        });
        self.dirs.retain(|root, handles| {
            if !walks.dirs.contains_key(root) {
                tracing::debug!(%root, "canceling dir prefetch with no corresponding walk");
                return false;
            }
            handles.retain(|(_, handle)| !handle.is_done());
            !handles.is_empty()
        });
        self.dir_batches.retain(|handle| !handle.is_done());
    }
}

impl Manager {
    /// Run until every sender of `kicks` is dropped. Kicks arrive when walks change; the timeout
    /// notices walks that stopped without one, since kicks only come from file and directory
    /// accesses.
    fn run(mut self, kicks: flume::Receiver<()>) {
        loop {
            let wait = if self.prefetches.any_paused() {
                RESUME_CHECK_INTERVAL
            } else {
                Duration::from_secs(1)
            };
            match kicks.recv_timeout(wait) {
                Ok(()) | Err(flume::RecvTimeoutError::Timeout) => {}
                Err(flume::RecvTimeoutError::Disconnected) => return,
            }
            // Don't busy loop under non-stop walk activity.
            let now = Instant::now();
            if self
                .last_tick
                .replace(now)
                .is_some_and(|last| now.duration_since(last) < self.config.min_interval)
            {
                std::thread::sleep(self.config.min_interval);
            }
            self.tick();
        }
    }

    fn tick(&mut self) {
        if !self.refresh_commit() {
            return;
        }
        let walks = ActiveWalks::from_detector(&self.detector);
        if walks.is_empty()
            && let Some(commit) = &mut self.commit
        {
            commit.manifest = None;
        }
        self.prefetches.reconcile(&walks);
        self.start_prefetches(walks);
    }

    /// Track the commit being prefetched for. When it changes, prefetches for the old commit are
    /// pointless, so everything about it is dropped and the sparse matcher is re-evaluated.
    fn refresh_commit(&mut self) -> bool {
        let id = match self.current_commit_id.read().as_ref() {
            Some(hex) => match HgId::from_hex(hex.as_bytes()) {
                Ok(id) => id,
                Err(err) => {
                    tracing::warn!(?err, commit_hex = hex, "invalid commit hash");
                    return false;
                }
            },
            None => {
                tracing::warn!("no commit when managing prefetches");
                return false;
            }
        };
        if self.commit.as_ref().is_some_and(|commit| commit.id == id) {
            return true;
        }
        tracing::info!(
            prev_commit_id = ?self.commit.as_ref().map(|commit| commit.id),
            current_commit_id = %id,
            "clearing out state for new commit id"
        );
        self.commit = None;
        self.prefetches = Prefetches::default();
        let sparse_matcher = match (self.make_matcher)(self.repo_root.clone()) {
            Ok(matcher) => matcher,
            Err(err) => {
                tracing::error!("fetching sparse matcher for workingcopy: {:?}", err);
                return false;
            }
        };
        self.commit = Some(CommitState {
            id,
            manifest: None,
            sparse_matcher,
        });
        true
    }

    /// The manifest of the current commit, resolved on first use.
    fn manifest(&mut self) -> Option<TreeManifest> {
        let commit = self.commit.as_mut()?;
        if commit.manifest.is_none() {
            match self.tree_resolver.get(&commit.id) {
                Ok(manifest) => commit.manifest = Some(manifest),
                Err(err) => {
                    tracing::error!(?err, commit_id = %commit.id, "error fetching root manifest");
                    return None;
                }
            }
        }
        commit.manifest.clone()
    }

    /// Start prefetches for walks not handled yet, within the prefetch limit.
    fn start_prefetches(&mut self, walks: ActiveWalks) {
        let Some(manifest) = self.manifest() else {
            return;
        };
        let mut slots = self
            .config
            .max_concurrent_prefetches
            .saturating_sub(self.prefetches.active());

        let mut shallow = Vec::new();
        for (root, depth) in walks.dirs {
            if self
                .prefetches
                .handled_dirs
                .contains(&(root.clone(), depth))
            {
                continue;
            }
            if depth <= SHALLOW_DIR_DEPTH {
                shallow.push((root, depth));
                if shallow.len() >= SHALLOW_DIR_BATCH {
                    self.start_dir_batch(&manifest, mem::take(&mut shallow), &mut slots);
                }
                continue;
            }
            let is_new = !self.prefetches.dirs.contains_key(&root);
            if is_new && slots == 0 {
                tracing::debug!(%root, depth, "not kicking off dir walk - prefetches full");
                continue;
            }
            if should_pause_prefetch(&self.detector, &self.config, &root, false) {
                tracing::debug!(%root, depth, "not kicking off dir walk - prefetching paused");
                continue;
            }
            let work = PrefetchWork::Dirs(vec![(root.clone(), depth)]);
            let Some(handle) = self.spawn(&manifest, work) else {
                tracing::debug!(%root, depth, "not kicking off dir walk - manifest walks full");
                continue;
            };
            tracing::debug!(%root, depth, "kicking off dir prefetch");
            self.prefetches.handled_dirs.insert((root.clone(), depth));
            let handles = self.prefetches.dirs.entry(root).or_default();
            handles.push((depth, handle));
            handles.sort_by_key(|(depth, _)| *depth);
            if is_new {
                slots -= 1;
            }
        }
        self.start_dir_batch(&manifest, shallow, &mut slots);

        for (root, depth) in walks.files {
            if let Some((prefetched, handle)) = self.prefetches.files.get(&root)
                && handle.is_paused()
                && *prefetched >= depth
            {
                self.resume(&manifest, root, &mut slots);
                continue;
            }
            if self
                .prefetches
                .handled_files
                .contains(&(root.clone(), depth))
            {
                continue;
            }
            // A running prefetch that already covers the reported depth is left alone; the walk
            // may have been replaced by a shallower one.
            if let Some((prefetched, _)) = self.prefetches.files.get(&root)
                && *prefetched >= depth
            {
                self.prefetches.handled_files.insert((root, depth));
                continue;
            }
            // A file prefetch is a single depth-first enumeration: separate per-depth prefetches
            // would share the root's pacing budget, with the shallow one spending it far ahead of
            // the walker while the deep ones starve. A deeper walk replaces the prefetch from the
            // top, since the deeper files under directories already passed are wanted too; the
            // new one passes already-fetched directories quickly from local trees. The shallower
            // prefetch runs until the deeper one has started, so a full manifest-walk limit
            // leaves the root with that prefetch rather than none until the next tick.
            let needs_slot = self
                .prefetches
                .files
                .get(&root)
                .is_none_or(|(_, handle)| handle.is_paused());
            if needs_slot && slots == 0 {
                tracing::debug!(%root, depth, "not kicking off file walk - prefetches full");
                continue;
            }
            if should_pause_prefetch(&self.detector, &self.config, &root, false) {
                tracing::debug!(%root, depth, "not kicking off file walk - prefetching paused");
                continue;
            }
            let work = PrefetchWork::Files {
                root: root.clone(),
                depth,
                start_after: None,
            };
            let Some(handle) = self.spawn(&manifest, work) else {
                tracing::debug!(%root, depth, "not kicking off file walk - manifest walks full");
                continue;
            };
            if self.prefetches.files.contains_key(&root) {
                tracing::debug!(%root, depth, "replacing shallower prefetch");
            }
            tracing::debug!(%root, depth, "kicking off file prefetch");
            self.prefetches.handled_files.insert((root.clone(), depth));
            self.prefetches.files.insert(root, (depth, handle));
            if needs_slot {
                slots -= 1;
            }
        }
    }

    /// Start a paused prefetch again, after the last file it fetched, once the walker has caught
    /// up enough and a slot is free.
    fn resume(&mut self, manifest: &TreeManifest, root: RepoPathBuf, slots: &mut usize) {
        let Some((depth, handle)) = self.prefetches.files.get(&root) else {
            return;
        };
        let (depth, start_after) = (*depth, handle.resume_after());
        if *slots == 0 || should_pause_prefetch(&self.detector, &self.config, &root, true) {
            return;
        }
        let work = PrefetchWork::Files {
            root: root.clone(),
            depth,
            start_after,
        };
        let Some(handle) = self.spawn(manifest, work) else {
            return;
        };
        tracing::debug!(%root, depth, "resuming prefetch");
        self.prefetches.files.insert(root, (depth, handle));
        *slots -= 1;
    }

    fn start_dir_batch(
        &mut self,
        manifest: &TreeManifest,
        batch: Vec<(RepoPathBuf, usize)>,
        slots: &mut usize,
    ) {
        if batch.is_empty() {
            return;
        }
        if *slots == 0 {
            tracing::debug!(
                batch_size = batch.len(),
                "not kicking off new batch of shallow dir walks - prefetches full"
            );
            return;
        }
        let handled = batch.clone();
        let Some(handle) = self.spawn(manifest, PrefetchWork::Dirs(batch)) else {
            tracing::debug!(
                batch_size = handled.len(),
                "not kicking off new batch of shallow dir walks - manifest walks full"
            );
            return;
        };
        self.prefetches.handled_dirs.extend(handled);
        self.prefetches.dir_batches.push(handle);
        *slots -= 1;
    }

    fn spawn(&self, manifest: &TreeManifest, work: PrefetchWork) -> Option<PrefetchHandle> {
        PrefetchJob {
            config: self.config,
            manifest: manifest.clone(),
            file_store: self.file_store.clone(),
            limits: self.limits.clone(),
            detector: self.detector.clone(),
            sparse_matcher: self
                .commit
                .as_ref()
                .and_then(|commit| commit.sparse_matcher.clone()),
            work,
        }
        .spawn()
    }
}

enum PrefetchWork {
    /// Files under `root` down to `depth`, in the walker's order, paced against its progress,
    /// starting after `start_after` when resuming.
    Files {
        root: RepoPathBuf,
        depth: usize,
        start_after: Option<RepoPathBuf>,
    },
    /// The trees under each `(root, depth)`.
    Dirs(Vec<(RepoPathBuf, usize)>),
}

/// One prefetch. Sapling APIs are synchronous, so each prefetch runs on its own thread.
struct PrefetchJob {
    config: Config,
    manifest: TreeManifest,
    file_store: Arc<dyn FileStore>,
    limits: Limits,
    detector: walkdetector::Detector,
    sparse_matcher: Option<DynMatcher>,
    work: PrefetchWork,
}

impl PrefetchJob {
    /// Start the prefetch if a manifest-walk slot is free. The slot is taken before spawning so
    /// queued walks cannot create an unbounded number of threads; the manager retries walks it
    /// could not start. Dropping every copy of the returned handle cancels the prefetch: the
    /// thread returns at its next check and drops its iterators, which stops their work.
    fn spawn(self) -> Option<PrefetchHandle> {
        let permit = self.limits.manifest_walks.try_acquire()?;
        let handle = PrefetchHandle::default();
        let thread_handle = handle.clone();
        std::thread::spawn(move || self.run(permit, thread_handle));
        Some(handle)
    }

    fn run(self, permit: ConcurrencyPermit, handle: PrefetchHandle) {
        let Self {
            config,
            manifest,
            file_store,
            limits,
            detector,
            sparse_matcher,
            work,
        } = self;
        match work {
            PrefetchWork::Dirs(walks) => {
                let Some((first_root, first_depth)) = walks.first().cloned() else {
                    return;
                };
                let _span = tracing::debug_span!(
                    "dir prefetch",
                    num_dir_walks = walks.len(),
                    %first_root,
                    first_depth,
                )
                .entered();
                let log = WalkLog::new(first_depth, || {
                    format!(
                        "root={first_root} depth={first_depth} num_dir_walks={}",
                        walks.len()
                    )
                });
                // The DepthMatcher is relative to file path depth. To fetch directories at
                // depth=N, we need to pretend we are fetching files at depth=N+1.
                let matchers = walks
                    .into_iter()
                    .map(|(root, depth)| Arc::new(WalkMatcher::new(root, depth + 1)) as DynMatcher)
                    .collect();
                let matcher = with_sparse(UnionMatcher::new_or_single(matchers), sparse_matcher);
                prefetch_dirs(&manifest, matcher, &handle, &log);
                drop(permit);
            }
            PrefetchWork::Files {
                root,
                depth,
                start_after,
            } => {
                let _span = tracing::debug_span!("file prefetch", %root, depth).entered();
                let log = WalkLog::new(depth, || format!("root={root} depth={depth}"));
                let matcher = with_sparse(
                    Arc::new(WalkMatcher::new(root.clone(), depth)) as DynMatcher,
                    sparse_matcher,
                );
                let files = manifest.files_dfs(matcher, start_after.as_deref());
                FilePrefetch {
                    config,
                    root,
                    detector,
                    handle,
                    log,
                    fetches: FileFetchQueue::new(file_store, limits.file_fetches),
                    manifest_walk_permit: Some(permit),
                    batch: Vec::new(),
                    batch_end: None,
                    last_fetched: None,
                    file_count: 0,
                    start: Instant::now(),
                }
                .run(files);
            }
        }
    }
}

fn with_sparse(matcher: DynMatcher, sparse_matcher: Option<DynMatcher>) -> DynMatcher {
    match sparse_matcher {
        Some(sparse_matcher) => Arc::new(IntersectMatcher::new(vec![matcher, sparse_matcher])),
        None => matcher,
    }
}

/// Load the trees `matcher` selects. Enumerating the files loads them; the files are not wanted.
fn prefetch_dirs(
    manifest: &TreeManifest,
    matcher: DynMatcher,
    handle: &PrefetchHandle,
    log: &WalkLog,
) {
    let start = Instant::now();
    for file in manifest.files(matcher) {
        if !handle.is_in_progress() {
            log.finished("prefetch canceled", start, 0);
            return;
        }
        if let Err(err) = file {
            tracing::error!(?err, "error fetching files from manifest");
        }
    }
    log.finished("prefetch complete", start, 0);
}

/// A file-content prefetch: files are enumerated in the walker's order, fetched in batches, and
/// paced against the walker's progress. When the walker falls too far behind, the prefetch
/// records the last file it fetched in its handle and stops; the manager starts it again from
/// there once the walker has caught up.
struct FilePrefetch {
    config: Config,
    root: RepoPathBuf,
    detector: walkdetector::Detector,
    handle: PrefetchHandle,
    log: WalkLog,
    fetches: FileFetchQueue,
    /// Released before waiting for the last fetches so an idle prefetch does not keep another
    /// from walking.
    manifest_walk_permit: Option<ConcurrencyPermit>,
    /// Keys carry no path, to keep a batch small; the path of the batch's last file is kept
    /// separately as the point to resume from.
    batch: Vec<Key>,
    batch_end: Option<RepoPathBuf>,
    last_fetched: Option<RepoPathBuf>,
    file_count: usize,
    start: Instant,
}

impl FilePrefetch {
    fn run(mut self, files: impl Iterator<Item = anyhow::Result<manifest::File>>) {
        // Enumerate file content depth-first in sorted order so the prefetch lead stays in the
        // walker's path; a breadth-first enumeration would fetch every shallow file under the
        // root before any deep one. The iterator fetches trees in batches ahead of its position.
        for file in files {
            if !self.handle.is_in_progress() {
                return self.finished("prefetch canceled");
            }
            match file {
                Ok(file) => {
                    self.batch
                        .push(Key::new(RepoPathBuf::new(), file.meta.hgid));
                    self.batch_end = Some(file.path);
                    if self.batch.len() >= self.config.file_batch_size && !self.fetch_batch() {
                        return self.stopped();
                    }
                }
                Err(err) => tracing::error!(?err, "error fetching files from manifest"),
            }
        }
        if !self.fetch_batch() {
            return self.stopped();
        }
        self.manifest_walk_permit.take();
        // Wait for the remaining fetches to finish.
        while self.handle.is_in_progress() && self.consume_one() {}
        if !self.handle.is_in_progress() {
            return self.finished("prefetch canceled");
        }
        self.finished("prefetch complete");
    }

    /// Fetch the batched files, unless the walker is too far behind, in which case the prefetch
    /// pauses. Returns false when the prefetch should stop, paused or canceled.
    fn fetch_batch(&mut self) -> bool {
        if self.batch.is_empty() {
            return true;
        }
        let _span = tracing::debug_span!("prefetching file content", batch_size = self.batch.len())
            .entered();
        if should_pause_prefetch(&self.detector, &self.config, &self.root, false) {
            self.pause();
            return false;
        }
        let Some(permit) = self.acquire_slot() else {
            return false;
        };
        // Consuming a fetch can take a while; the prefetch may have been canceled meanwhile.
        if !self.handle.is_in_progress() {
            return false;
        }
        // Use IGNORE_RESULT optimization since we don't care about the data.
        // An important implementation detail for us: the scmstore FileStore operates
        // asynchronously when you fetch more than 1_000 keys. If that assumption changes, we
        // will need to change what we do here.
        let batch = mem::take(&mut self.batch);
        let batch_end = self.batch_end.take();
        self.file_count += batch.len();
        match self.fetches.start(permit, batch, self.config.skip_lfs) {
            Ok(()) => self.last_fetched = batch_end,
            Err(err) => tracing::error!(?err, "error prefetching file content"),
        }
        true
    }

    /// Stop here and let the manager start again after the last fetched file once the walker has
    /// caught up. The files batched but not fetched are enumerated again then. Outstanding
    /// fetches are drained first so their files are accounted to the walk.
    fn pause(&mut self) {
        self.log.event("pausing prefetch");
        self.manifest_walk_permit.take();
        while self.consume_one() {}
        self.handle.pause(self.last_fetched.take());
    }

    /// A fetch slot, consuming this prefetch's own finished fetches first. None if canceled.
    fn acquire_slot(&mut self) -> Option<ConcurrencyPermit> {
        loop {
            if !self.handle.is_in_progress() {
                return None;
            }
            if let Some(permit) = self.fetches.try_acquire() {
                return Some(permit);
            }
            if !self.consume_one() {
                return self.fetches.acquire(&self.handle);
            }
        }
    }

    /// Wait for the oldest fetch to finish and account it to the walk, so pacing knows how far
    /// ahead the prefetch is. False if none was pending.
    fn consume_one(&mut self) -> bool {
        let Some(fctx) = self.fetches.consume_one() else {
            return false;
        };
        self.detector
            .files_preloaded(&self.root, fctx.remote_fetch_count());
        true
    }

    fn stopped(&self) {
        if self.handle.is_paused() {
            self.finished("prefetch paused");
        } else {
            self.finished("prefetch canceled");
        }
    }

    fn finished(&self, message: &str) {
        self.log.finished(message, self.start, self.file_count);
    }
}

/// Logging for one prefetch, at INFO for deep walks and DEBUG otherwise.
struct WalkLog {
    interesting: bool,
    /// Only interesting walks allocate a description; routine ones get their context from the
    /// span.
    desc: String,
}

impl WalkLog {
    fn new(depth: usize, desc: impl FnOnce() -> String) -> Self {
        let interesting = depth >= INTERESTING_DEPTH;
        Self {
            interesting,
            desc: if interesting { desc() } else { String::new() },
        }
    }

    fn event(&self, message: &str) {
        info_if!(self.interesting, event, walk = %self.desc, "{}", message);
    }

    fn finished(&self, message: &str, start: Instant, file_count: usize) {
        info_if!(
            self.interesting,
            event,
            walk = %self.desc,
            elapsed = ?start.elapsed(),
            file_count,
            "{}",
            message
        );
    }
}

#[cfg(test)]
fn prefetch(
    config: Config,
    manifest: TreeManifest,
    file_store: Arc<dyn FileStore>,
    detector: walkdetector::Detector,
    work: PrefetchWork,
    sparse_matcher: Option<DynMatcher>,
) -> PrefetchHandle {
    PrefetchJob {
        config,
        manifest,
        file_store,
        limits: Limits::new(&config),
        detector,
        sparse_matcher,
        work,
    }
    .spawn()
    .unwrap()
}

/// Whether prefetching for `walk_root` should pause, given how much has been prefetched and how
/// much of it the walker has read. Files are not necessarily read in the order they are
/// prefetched, so a sizeable lag is allowed before pausing; a paused prefetch resumes only once
/// the lag is down to half that, so it does not stop and start on every batch.
fn should_pause_prefetch(
    detector: &walkdetector::Detector,
    config: &Config,
    walk_root: &RepoPath,
    resuming: bool,
) -> bool {
    let (prefetch_count, read_count) = detector.files_preloaded(walk_root, 0);
    tracing::debug!(%walk_root, prefetch_count, read_count, "should_pause_prefetch");

    if prefetch_count == 0 || config.max_initial_lag == 0 {
        return false;
    }

    let prefetch_ratio = read_count as f64 / prefetch_count as f64;
    if prefetch_ratio > config.min_ratio {
        // If we haven't gone below our allowed ratio, keep prefetching.
        return false;
    }

    let max_lag = if resuming {
        config.max_initial_lag / 2
    } else {
        config.max_initial_lag
    };
    prefetch_count.saturating_sub(read_count) > max_lag
}

#[derive(Clone)]
struct ConcurrencyLimiter {
    available: flume::Receiver<()>,
    release: flume::Sender<()>,
}

impl ConcurrencyLimiter {
    fn new(limit: usize) -> Self {
        let limit = limit.max(1);
        let (release, available) = flume::bounded(limit);
        for _ in 0..limit {
            release
                .try_send(())
                .expect("new limiter channel should have capacity for its initial permits");
        }

        Self { available, release }
    }

    fn try_acquire(&self) -> Option<ConcurrencyPermit> {
        self.available.try_recv().ok().map(|()| ConcurrencyPermit {
            release: self.release.clone(),
        })
    }

    /// Wait for a permit, or for `handle` to stop. A plain timed receive is used rather than a
    /// `flume::Selector` over the permit and cancellation channels: a selector that is woken for
    /// a permit which a concurrent `try_acquire` takes first parks again without re-registering,
    /// and then sleeps through every later release.
    fn acquire(&self, handle: &PrefetchHandle) -> Option<ConcurrencyPermit> {
        loop {
            if !handle.is_in_progress() {
                return None;
            }
            match self.available.recv_timeout(PERMIT_POLL_INTERVAL) {
                Ok(()) => {
                    let permit = ConcurrencyPermit {
                        release: self.release.clone(),
                    };
                    return handle.is_in_progress().then_some(permit);
                }
                Err(flume::RecvTimeoutError::Timeout) => {}
                Err(flume::RecvTimeoutError::Disconnected) => return None,
            }
        }
    }
}

struct ConcurrencyPermit {
    release: flume::Sender<()>,
}

impl Drop for ConcurrencyPermit {
    fn drop(&mut self) {
        match self.release.try_send(()) {
            Ok(()) | Err(flume::TrySendError::Disconnected(())) => {}
            Err(flume::TrySendError::Full(())) => {
                tracing::error!("concurrency limiter permit channel unexpectedly full");
            }
        }
    }
}

/// Outstanding file fetches of one prefetch, bounded by the shared fetch limit.
struct FileFetchQueue {
    file_store: Arc<dyn FileStore>,
    limiter: ConcurrencyLimiter,
    fetches: VecDeque<(
        FetchContext,
        storemodel::ContentFetchItems,
        ConcurrencyPermit,
    )>,
}

impl FileFetchQueue {
    fn new(file_store: Arc<dyn FileStore>, limiter: ConcurrencyLimiter) -> Self {
        Self {
            file_store,
            limiter,
            fetches: VecDeque::new(),
        }
    }

    /// A fetch slot, if one is free.
    fn try_acquire(&self) -> Option<ConcurrencyPermit> {
        self.limiter.try_acquire()
    }

    /// Wait for a fetch slot, or for `handle` to stop.
    fn acquire(&self, handle: &PrefetchHandle) -> Option<ConcurrencyPermit> {
        self.limiter.acquire(handle)
    }

    fn start(
        &mut self,
        permit: ConcurrencyPermit,
        keys: Vec<Key>,
        skip_lfs: bool,
    ) -> anyhow::Result<()> {
        let fctx = FetchContext::new_with_mode_and_cause(
            FetchMode::AllowRemote | FetchMode::IGNORE_RESULT,
            FetchCause::EdenWalkPrefetch,
        )
        .with_skip_lfs(skip_lfs);
        let iter = self.file_store.get_content_iter(fctx.clone(), keys)?;
        self.fetches.push_back((fctx, iter, permit));
        Ok(())
    }

    /// Wait for the oldest fetch to finish. None if none was pending.
    fn consume_one(&mut self) -> Option<FetchContext> {
        let (fctx, iter, _permit) = self.fetches.pop_front()?;
        iter.into_iter().for_each(drop);
        Some(fctx)
    }
}

// A pathmatcher::Matcher impl that matches a simple directory prefix along with a depth filter.
struct WalkMatcher {
    depth_matcher: DepthMatcher,
    dir: RepoPathBuf,
}

impl WalkMatcher {
    fn new(dir: RepoPathBuf, walk_depth: usize) -> Self {
        let start_depth = dir.depth();
        Self {
            depth_matcher: DepthMatcher::new(Some(start_depth), Some(start_depth + walk_depth)),
            dir,
        }
    }
}

impl Matcher for WalkMatcher {
    fn matches_directory(&self, path: &RepoPath) -> anyhow::Result<DirectoryMatch> {
        // Check that either path is a prefix of self.dir (i.e. later might be under self.dir), or
        // self.dir is a prefix of path (i.e. we are under self.dir).
        if !path.starts_with(&self.dir, true) && !self.dir.starts_with(path, true) {
            return Ok(DirectoryMatch::Nothing);
        }

        self.depth_matcher.matches_directory(path)
    }

    fn matches_file(&self, path: &RepoPath) -> anyhow::Result<bool> {
        if !path.starts_with(&self.dir, true) {
            return Ok(false);
        }
        self.depth_matcher.matches_file(path)
    }
}

#[derive(Debug)]
struct PrefetchState {
    status: AtomicU8,
    /// Where a paused prefetch left off: the last file it fetched, or None if it fetched nothing.
    resume_after: parking_lot::Mutex<Option<RepoPathBuf>>,
}

#[derive(Clone, Debug)]
pub(crate) struct PrefetchHandle {
    state: Arc<PrefetchState>,
}

impl Default for PrefetchHandle {
    fn default() -> Self {
        Self {
            state: Arc::new(PrefetchState {
                status: AtomicU8::new(Self::IN_PROGRESS),
                resume_after: Default::default(),
            }),
        }
    }
}

impl PrefetchHandle {
    const IN_PROGRESS: u8 = 0;
    const DONE: u8 = 1;
    const PAUSED: u8 = 2;

    /// End a running prefetch. The thread's own copy of the handle drops when it returns, so a
    /// prefetch that paused stays paused and one that finished is marked done.
    pub(crate) fn cancel(&self) {
        let _ = self.state.status.compare_exchange(
            Self::IN_PROGRESS,
            Self::DONE,
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }

    /// Mark the prefetch paused after `resume_after`, unless it was canceled meanwhile.
    fn pause(&self, resume_after: Option<RepoPathBuf>) {
        *self.state.resume_after.lock() = resume_after;
        let _ = self.state.status.compare_exchange(
            Self::IN_PROGRESS,
            Self::PAUSED,
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }

    pub(crate) fn is_done(&self) -> bool {
        self.state.status.load(Ordering::Acquire) == Self::DONE
    }

    pub(crate) fn is_in_progress(&self) -> bool {
        self.state.status.load(Ordering::Acquire) == Self::IN_PROGRESS
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.state.status.load(Ordering::Acquire) == Self::PAUSED
    }

    /// The last file a paused prefetch fetched, to resume after.
    pub(crate) fn resume_after(&self) -> Option<RepoPathBuf> {
        self.state.resume_after.lock().clone()
    }
}

impl Drop for PrefetchHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use anyhow::anyhow;
    use blob::Blob;
    use manifest::FileMetadata;
    use manifest_tree::TreeManifest;
    use manifest_tree::testutil;
    use manifest_tree::testutil::TestStore;
    use pathmatcher::TreeMatcher;
    use rand_chacha::ChaChaRng;
    use rand_chacha::rand_core::SeedableRng;
    use storemodel::KeyStore;
    use storemodel::SerializationFormat;
    use storemodel::TreeEntry;
    use storemodel::TreeFetchItems;
    use storemodel::TreeStore;

    use super::*;

    #[test]
    fn test_prefetch_files() -> anyhow::Result<()> {
        let store = Arc::new(TestStore::new());

        let file_store = store.clone() as Arc<dyn FileStore>;

        let foo_path: RepoPathBuf = "dir/foo".to_string().try_into()?;
        let bar_path: RepoPathBuf = "dir/dir2/bar".to_string().try_into()?;
        let exclude_path: RepoPathBuf = "excludeme/please".to_string().try_into()?;
        let exclude_dir: RepoPathBuf = "excludeme".to_string().try_into()?;

        // Insert a few files into the file store.
        let foo_hgid =
            file_store.insert_data(Default::default(), &foo_path, b"foo content".into())?;
        let bar_hgid =
            file_store.insert_data(Default::default(), &bar_path, b"bar content".into())?;
        let exclude_hgid = file_store.insert_data(
            Default::default(),
            &exclude_path,
            b"excluded content".into(),
        )?;

        let mut mf = TreeManifest::ephemeral(store.clone());

        // Create corresponding manifest entries.
        mf.insert(
            foo_path.clone(),
            FileMetadata::new(foo_hgid, types::FileType::Regular),
        )?;

        mf.insert(
            bar_path.clone(),
            FileMetadata::new(bar_hgid, types::FileType::Regular),
        )?;

        mf.insert(
            exclude_path.clone(),
            FileMetadata::new(exclude_hgid, types::FileType::Regular),
        )?;

        let detector = walkdetector::Detector::new();
        let rules = ["**", "!excludeme/please"];
        let sparse_matcher = Arc::new(TreeMatcher::from_rules(rules.iter(), true)?);

        // Prefetch for "excludeme/" at depth=0 (i.e. "excludeme/*").
        let handle = prefetch(
            Config::default(),
            mf.clone(),
            file_store.clone(),
            detector.clone(),
            PrefetchWork::Files {
                root: exclude_dir.clone(),
                depth: 0,
                start_after: None,
            },
            Some(sparse_matcher.clone()),
        );

        // Wait for prefetch to complete.
        while !handle.is_done() {
            std::thread::sleep(Duration::from_millis(1));
        }

        // Nothing was fetched since excludeme is in the sparse profile
        assert_eq!(
            store.fetches().into_iter().flatten().collect::<Vec<_>>(),
            Vec::<Key>::new()
        );

        // Prefetch for "dir/" at depth=0 (i.e. "dir/*").
        let handle = prefetch(
            Config::default(),
            mf.clone(),
            file_store.clone(),
            detector.clone(),
            PrefetchWork::Files {
                root: "dir".to_string().try_into()?,
                depth: 0,
                start_after: None,
            },
            Some(sparse_matcher.clone()),
        );

        // Wait for prefetch to complete.
        while !handle.is_done() {
            std::thread::sleep(Duration::from_millis(1));
        }

        // We only fetched "foo" (due to the depth limit).
        assert_eq!(
            store.fetches().into_iter().flatten().collect::<Vec<_>>(),
            vec![Key::new(RepoPathBuf::new(), foo_hgid)]
        );

        // Retry excludeme prefetch without sparse profile
        let handle = prefetch(
            Config::default(),
            mf.clone(),
            file_store.clone(),
            detector.clone(),
            PrefetchWork::Files {
                root: exclude_dir,
                depth: 0,
                start_after: None,
            },
            None,
        );

        // Wait for prefetch to complete.
        while !handle.is_done() {
            std::thread::sleep(Duration::from_millis(1));
        }

        // We fetched "excludeme/please" (i.e. did not redundantly fetch dir/foo).
        assert_eq!(
            store.fetches().into_iter().flatten().collect::<Vec<_>>(),
            vec![
                Key::new(RepoPathBuf::new(), foo_hgid),
                Key::new(RepoPathBuf::new(), exclude_hgid),
            ]
        );

        // Prefetch for "dir/" at depth=1 (i.e. "dir/*" and "dir/*/*").
        let handle = prefetch(
            Config::default(),
            mf.clone(),
            file_store.clone(),
            detector.clone(),
            PrefetchWork::Files {
                root: "dir".to_string().try_into()?,
                depth: 1,
                start_after: None,
            },
            None,
        );

        // Wait for prefetch to complete.
        while !handle.is_done() {
            std::thread::sleep(Duration::from_millis(1));
        }

        // Depth-first in name order: "dir/dir2/bar", then "dir/foo" (already local) is requested
        // again.
        assert_eq!(
            store.fetches().into_iter().flatten().collect::<Vec<_>>(),
            vec![
                Key::new(RepoPathBuf::new(), foo_hgid),
                Key::new(RepoPathBuf::new(), exclude_hgid),
                Key::new(RepoPathBuf::new(), bar_hgid),
                Key::new(RepoPathBuf::new(), foo_hgid),
            ]
        );

        let handle = prefetch(
            Config {
                file_batch_size: 2,
                ..Default::default()
            },
            mf,
            file_store,
            detector,
            PrefetchWork::Files {
                root: RepoPathBuf::new(),
                depth: 2,
                start_after: None,
            },
            None,
        );

        while !handle.is_done() {
            std::thread::sleep(Duration::from_millis(1));
        }

        let fetches = store.fetches();
        let fetch_sizes = fetches.iter().map(Vec::len).collect::<Vec<_>>();
        assert!(
            fetch_sizes.ends_with(&[2, 1]),
            "expected final fetch sizes [2, 1], got {fetch_sizes:?}"
        );

        Ok(())
    }

    #[test]
    fn test_prefetch_dirs() -> anyhow::Result<()> {
        let store = Arc::new(TestStore::new());

        let file_store = store.clone() as Arc<dyn FileStore>;

        let foo_path: RepoPathBuf = "dir/foo".to_string().try_into()?;
        let bar_path: RepoPathBuf = "dir/dir2/bar".to_string().try_into()?;
        let exclude_path: RepoPathBuf = "excludeme/please".to_string().try_into()?;
        let exclude_dir: RepoPathBuf = "excludeme".to_string().try_into()?;

        // Insert a few files into the file store.
        let foo_hgid =
            file_store.insert_data(Default::default(), &foo_path, b"foo content".into())?;
        let bar_hgid =
            file_store.insert_data(Default::default(), &bar_path, b"bar content".into())?;
        let exclude_hgid = file_store.insert_data(
            Default::default(),
            &exclude_path,
            b"excluded content".into(),
        )?;

        let mut mf = TreeManifest::ephemeral(store.clone());

        // Create corresponding manifest entries.
        mf.insert(
            foo_path.clone(),
            FileMetadata::new(foo_hgid, types::FileType::Regular),
        )?;

        mf.insert(
            bar_path.clone(),
            FileMetadata::new(bar_hgid, types::FileType::Regular),
        )?;

        mf.insert(
            exclude_path.clone(),
            FileMetadata::new(exclude_hgid, types::FileType::Regular),
        )?;

        let root_hgid = mf.persist(&[])?;
        let dir_path: RepoPathBuf = "dir".to_string().try_into()?;
        let dir_hgid = testutil::get_hgid(&mf, &dir_path);
        let excludeme_path: RepoPathBuf = "excludeme".to_string().try_into()?;
        let excludeme_hgid = testutil::get_hgid(&mf, &excludeme_path);

        // Create a fresh durable manifest from the root hgid to ensure fetches happen
        let mf = remote_tree_manifest(store.clone(), root_hgid);

        let detector = walkdetector::Detector::new();
        let rules = ["**", "!excludeme/**"];
        let sparse_matcher = Arc::new(TreeMatcher::from_rules(rules.iter(), true)?);

        // matcher must report "Nothing" for excludeme to be properly excluded from the fetch
        assert_eq!(
            sparse_matcher.matches_directory(&exclude_dir).unwrap(),
            DirectoryMatch::Nothing
        );

        // Prefetch directories for "" at depth=0 (i.e. "*"). This should fetch directory "dir".
        let handle = prefetch(
            Config::default(),
            mf.clone(),
            file_store.clone(),
            detector.clone(),
            PrefetchWork::Dirs(vec![("".to_string().try_into()?, 0)]),
            Some(sparse_matcher),
        );

        // Wait for prefetch to complete.
        while !handle.is_done() {
            std::thread::sleep(Duration::from_millis(1));
        }

        // We fetched the root dir and "dir/" (but not "dir/dir2" or "excludeme").
        // Note: keys use empty paths for efficiency, so we check by hgid.
        assert_eq!(
            store
                .fetches()
                .into_iter()
                .map(|batch| {
                    let mut hgids = batch.into_iter().map(|k| k.hgid).collect::<Vec<HgId>>();
                    hgids.sort();
                    hgids
                })
                .collect::<Vec<Vec<HgId>>>(),
            vec![vec![root_hgid], vec![dir_hgid]]
        );

        // Create a fresh durable manifest for the retry (without sparse profile)
        let mf = remote_tree_manifest(store.clone(), root_hgid);

        // Retry prefetch without sparse profile
        let handle = prefetch(
            Config::default(),
            mf,
            file_store.clone(),
            detector.clone(),
            PrefetchWork::Dirs(vec![("".to_string().try_into()?, 0)]),
            None,
        );

        // Wait for prefetch to complete.
        while !handle.is_done() {
            std::thread::sleep(Duration::from_millis(1));
        }

        // We fetched excludeme this time
        assert_eq!(
            store
                .fetches()
                .into_iter()
                .map(|batch| {
                    let mut hgids = batch.into_iter().map(|k| k.hgid).collect::<Vec<HgId>>();
                    hgids.sort();
                    hgids
                })
                .collect::<Vec<Vec<HgId>>>(),
            vec![vec![root_hgid], vec![dir_hgid], vec![root_hgid], {
                let mut v = vec![dir_hgid, excludeme_hgid];
                v.sort();
                v
            }]
        );

        Ok(())
    }

    fn make_test_sparse_matcher(_: PathBuf) -> SparseMatcherResult {
        let rules = ["**", "!excludeme/**"];
        let sparse_matcher = TreeMatcher::from_rules(rules.iter(), true)?;
        Ok(Some(Arc::new(sparse_matcher)))
    }

    #[derive(Clone)]
    struct RemoteTreeTestStore {
        inner: Arc<TestStore>,
    }

    impl RemoteTreeTestStore {
        fn new(inner: Arc<TestStore>) -> Self {
            Self { inner }
        }
    }

    impl KeyStore for RemoteTreeTestStore {
        fn get_content_iter(
            &self,
            fctx: FetchContext,
            keys: Vec<Key>,
        ) -> anyhow::Result<storemodel::ContentFetchItems> {
            self.inner.get_content_iter(fctx, keys)
        }

        fn get_local_content(&self, path: &RepoPath, hgid: HgId) -> anyhow::Result<Option<Blob>> {
            self.inner.get_local_content(path, hgid)
        }

        fn insert_data(
            &self,
            opts: storemodel::InsertOpts,
            path: &RepoPath,
            data: Blob,
        ) -> anyhow::Result<HgId> {
            self.inner.insert_data(opts, path, data)
        }

        fn format(&self) -> SerializationFormat {
            self.inner.format()
        }

        fn clone_key_store(&self) -> Box<dyn KeyStore> {
            Box::new(self.clone())
        }
    }

    impl TreeStore for RemoteTreeTestStore {
        fn get_tree_iter(
            &self,
            fctx: FetchContext,
            keys: Vec<Key>,
        ) -> anyhow::Result<TreeFetchItems> {
            self.inner.get_tree_iter(fctx, keys)
        }

        fn get_local_tree(
            &self,
            _path: &RepoPath,
            _hgid: HgId,
        ) -> anyhow::Result<Option<Arc<dyn TreeEntry>>> {
            Ok(None)
        }

        fn clone_tree_store(&self) -> Box<dyn TreeStore> {
            Box::new(self.clone())
        }
    }

    fn remote_tree_manifest(store: Arc<TestStore>, root_hgid: HgId) -> TreeManifest {
        TreeManifest::durable(Arc::new(RemoteTreeTestStore::new(store)), root_hgid)
    }

    struct TestTreeResolver {
        store: Arc<TestStore>,
        root_hgids: HashMap<HgId, HgId>,
    }

    impl TestTreeResolver {
        fn new(
            store: Arc<TestStore>,
            commit_root_pairs: impl IntoIterator<Item = (HgId, HgId)>,
        ) -> Self {
            Self {
                store,
                root_hgids: commit_root_pairs.into_iter().collect(),
            }
        }
    }

    impl TreeResolver for TestTreeResolver {
        fn get(&self, commit_id: &HgId) -> anyhow::Result<TreeManifest> {
            let root_hgid = self
                .root_hgids
                .get(commit_id)
                .ok_or(anyhow!("no manifest for {commit_id}"))?;
            Ok(remote_tree_manifest(self.store.clone(), *root_hgid))
        }

        fn get_root_id(&self, _commit_id: &HgId) -> anyhow::Result<HgId> {
            unimplemented!()
        }

        fn get_by_root_id(&self, _root_id: &HgId) -> anyhow::Result<TreeManifest> {
            unimplemented!()
        }
    }

    #[test]
    fn test_prefetch_manager() -> anyhow::Result<()> {
        let mut detector = walkdetector::Detector::new();
        detector.set_walk_threshold(2);

        // Use separate stores for trees and files so we can track fetches independently.
        let tree_store = Arc::new(TestStore::new());
        let file_store = Arc::new(TestStore::new());

        let dir1_foo_path: RepoPathBuf = "dir1/foo".to_string().try_into()?;
        let dir1_bar_path: RepoPathBuf = "dir1/bar".to_string().try_into()?;
        let dir2_foo_path: RepoPathBuf = "dir2/foo".to_string().try_into()?;
        let dir2_bar_path: RepoPathBuf = "dir2/bar".to_string().try_into()?;
        let dir3_foo_path: RepoPathBuf = "dir3/foo".to_string().try_into()?;
        let exclude_path: RepoPathBuf = "excludeme/please".to_string().try_into()?;

        // Insert a few files into the file store.
        let dir1_foo_hgid =
            file_store.insert_data(Default::default(), &dir1_foo_path, b"foo content".into())?;
        let dir1_bar_hgid =
            file_store.insert_data(Default::default(), &dir1_bar_path, b"bar content".into())?;
        let dir2_foo_hgid =
            file_store.insert_data(Default::default(), &dir2_foo_path, b"foo content!".into())?;
        let dir2_bar_hgid =
            file_store.insert_data(Default::default(), &dir2_bar_path, b"bar content!".into())?;
        let dir3_foo_hgid =
            file_store.insert_data(Default::default(), &dir3_foo_path, b"foo content!!".into())?;
        let exclude_hgid = file_store.insert_data(
            Default::default(),
            &exclude_path,
            b"excluded content".into(),
        )?;

        let mut mf = TreeManifest::ephemeral(tree_store.clone());

        // Create corresponding manifest entries.
        mf.insert(
            dir1_foo_path.clone(),
            FileMetadata::new(dir1_foo_hgid, types::FileType::Regular),
        )?;

        mf.insert(
            dir1_bar_path.clone(),
            FileMetadata::new(dir1_bar_hgid, types::FileType::Regular),
        )?;

        mf.insert(
            dir2_foo_path.clone(),
            FileMetadata::new(dir2_foo_hgid, types::FileType::Regular),
        )?;

        mf.insert(
            dir2_bar_path.clone(),
            FileMetadata::new(dir2_bar_hgid, types::FileType::Regular),
        )?;

        mf.insert(
            dir3_foo_path.clone(),
            FileMetadata::new(dir3_foo_hgid, types::FileType::Regular),
        )?;

        mf.insert(
            exclude_path.clone(),
            FileMetadata::new(exclude_hgid, types::FileType::Regular),
        )?;

        // Finalize and store tree data
        let root_hgid = mf.persist(&[])?;

        let mut rng = ChaChaRng::from_seed([0u8; 32]);
        let stub_commit_id = HgId::random(&mut rng);

        let tree_resolver =
            TestTreeResolver::new(tree_store.clone(), [(stub_commit_id, root_hgid)]);

        let kick_manager = prefetch_manager(
            Config::default(),
            Arc::new(tree_resolver),
            file_store.clone(),
            Arc::new(RwLock::new(Some(stub_commit_id.to_hex()))),
            detector.clone(),
            "".into(),
            make_test_sparse_matcher,
            Limits::new(&Config::default()),
        );

        // Trigger a walk of dir1.
        detector.file_loaded(&dir1_foo_path, 0);
        detector.file_loaded(&dir1_bar_path, 0);

        kick_manager.send(())?;

        // Wait for prefetch to finish.
        for _ in 0..10 {
            if file_store.key_fetch_count() < 2 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        // Check that expected file hgids are present in file_store fetches.
        let fetches: Vec<Key> = file_store.fetches().into_iter().flatten().collect();
        let expected_file_hgids = vec![dir1_foo_hgid, dir1_bar_hgid];
        for exp_hgid in &expected_file_hgids {
            assert!(
                fetches.iter().any(|f| f.hgid == *exp_hgid),
                "expected hgid {exp_hgid:?} in fetches {fetches:?}"
            );
        }

        // Build a new detector so we can trigger a root prefetch.
        let mut detector = walkdetector::Detector::new();
        let tree_resolver =
            TestTreeResolver::new(tree_store.clone(), [(stub_commit_id, root_hgid)]);
        detector.set_walk_threshold(2);
        let kick_manager = prefetch_manager(
            Config::default(),
            Arc::new(tree_resolver),
            file_store.clone(),
            Arc::new(RwLock::new(Some(stub_commit_id.to_hex()))),
            detector.clone(),
            "".into(),
            make_test_sparse_matcher,
            Limits::new(&Config::default()),
        );

        // Trigger a walk of the root.
        detector.file_loaded(RepoPath::from_static_str("dir1/foo"), 0);
        detector.file_loaded(RepoPath::from_static_str("dir1/bar"), 0);
        detector.file_loaded(RepoPath::from_static_str("dir2/foo"), 0);
        detector.file_loaded(RepoPath::from_static_str("dir2/bar"), 0);

        kick_manager.send(())?;

        // Wait for prefetch to finish.
        for _ in 0..10 {
            if file_store.key_fetch_count() < 7 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        // Check that file content was fetched (the key hgids should be present).
        let fetches: Vec<Key> = file_store.fetches().into_iter().flatten().collect();
        let expected_file_hgids = vec![
            dir1_foo_hgid,
            dir1_bar_hgid,
            dir2_foo_hgid,
            dir2_bar_hgid,
            dir3_foo_hgid,
        ];
        for exp_hgid in &expected_file_hgids {
            assert!(
                fetches.iter().any(|f| f.hgid == *exp_hgid),
                "expected hgid {exp_hgid:?} in fetches {fetches:?}"
            );
        }

        Ok(())
    }

    #[test]
    fn test_batch_prefetch_dirs() -> anyhow::Result<()> {
        let tree_store = Arc::new(TestStore::new());

        let file_store = Arc::new(TestStore::new()) as Arc<dyn FileStore>;

        let foo_path: RepoPathBuf = "dir1/foo".to_string().try_into()?;
        let bar_path: RepoPathBuf = "dir2/bar".to_string().try_into()?;

        let foo_hgid =
            file_store.insert_data(Default::default(), &foo_path, b"foo content".into())?;
        let bar_hgid =
            file_store.insert_data(Default::default(), &bar_path, b"bar content".into())?;

        let mut mf = TreeManifest::ephemeral(tree_store.clone());

        // Create corresponding manifest entries.
        mf.insert(
            foo_path.clone(),
            FileMetadata::new(foo_hgid, types::FileType::Regular),
        )?;

        mf.insert(
            bar_path.clone(),
            FileMetadata::new(bar_hgid, types::FileType::Regular),
        )?;

        let root_hgid = mf.persist(&[])?;
        let dir1_path: RepoPathBuf = "dir1".to_string().try_into()?;
        let dir1_hgid = testutil::get_hgid(&mf, &dir1_path);
        let dir2_path: RepoPathBuf = "dir2".to_string().try_into()?;
        let dir2_hgid = testutil::get_hgid(&mf, &dir2_path);

        let mut detector = walkdetector::Detector::new();
        detector.set_walk_threshold(2);

        let mut rng = ChaChaRng::from_seed([0u8; 32]);
        let stub_commit_id = HgId::random(&mut rng);

        let tree_resolver =
            TestTreeResolver::new(tree_store.clone(), [(stub_commit_id, root_hgid)]);

        let kick_manager = prefetch_manager(
            Config::default(),
            Arc::new(tree_resolver),
            file_store,
            Arc::new(RwLock::new(Some(stub_commit_id.to_hex()))),
            detector.clone(),
            "".into(),
            |_| Ok(None),
            Limits::new(&Config::default()),
        );

        // Trigger a directory walk for each of "dir1" and "dir2".
        detector.dir_loaded(RepoPath::from_static_str("dir1/a"), 0, 0, 0);
        detector.dir_loaded(RepoPath::from_static_str("dir1/b"), 0, 0, 0);
        detector.dir_loaded(RepoPath::from_static_str("dir2/a"), 0, 0, 0);
        detector.dir_loaded(RepoPath::from_static_str("dir2/b"), 0, 0, 0);

        kick_manager.send(())?;

        // Wait for prefetch to finish.
        for _ in 0..10 {
            if tree_store.fetches().len() < 2 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        // Check that we fetched both "dir1" and "dir2" in the same batch.
        // Note: keys use empty paths for efficiency, so we check by hgid.
        assert_eq!(
            tree_store
                .fetches()
                .into_iter()
                .map(|batch| {
                    let mut hgids: Vec<HgId> = batch.into_iter().map(|k| k.hgid).collect();
                    hgids.sort();
                    hgids
                })
                .collect::<Vec<Vec<HgId>>>(),
            vec![vec![root_hgid], {
                let mut v = vec![dir1_hgid, dir2_hgid];
                v.sort();
                v
            },],
        );

        Ok(())
    }

    #[test]
    fn test_prefetch_handle() {
        let handle = PrefetchHandle::default();
        assert!(handle.is_in_progress());
        assert!(!handle.is_done());

        // Dropping any clone cancels the prefetch.
        drop(handle.clone());
        assert!(handle.is_done());
        assert!(!handle.is_in_progress());
    }

    #[test]
    fn test_prefetch_config_defaults() {
        let config = Config::default();
        assert_eq!(config.max_concurrent_prefetches, 4);
        assert_eq!(config.max_concurrent_manifest_walks, 4);
        assert_eq!(config.max_concurrent_file_fetches, 4);
        assert!(config.skip_lfs);
    }

    #[test]
    fn test_active_prefetches_counts_logical_prefetches() {
        let mut prefetches = Prefetches::default();
        // Two depths of one root are one logical prefetch.
        prefetches.dirs.insert(
            RepoPathBuf::new(),
            vec![
                (1, PrefetchHandle::default()),
                (2, PrefetchHandle::default()),
            ],
        );
        prefetches.dir_batches.push(PrefetchHandle::default());
        assert_eq!(prefetches.active(), 2);
        prefetches
            .files
            .insert(RepoPathBuf::new(), (1, PrefetchHandle::default()));
        assert_eq!(prefetches.active(), 3);
    }

    #[test]
    fn test_concurrency_limiter_is_shared() {
        let limiter = ConcurrencyLimiter::new(2);
        let first = limiter.try_acquire().unwrap();
        let second = limiter.clone().try_acquire().unwrap();
        assert!(limiter.try_acquire().is_none());

        drop(first);
        let third = limiter.try_acquire().unwrap();
        assert!(limiter.try_acquire().is_none());

        drop(second);
        drop(third);
        assert!(limiter.try_acquire().is_some());
    }

    #[test]
    fn test_concurrency_limiter_wait_is_cancelable() {
        let limiter = ConcurrencyLimiter::new(1);
        let held_permit = limiter.try_acquire().unwrap();
        let handle = PrefetchHandle::default();
        let waiting_handle = handle.clone();
        let waiting_limiter = limiter.clone();
        let (result_send, result_recv) = std::sync::mpsc::channel();

        let waiter = std::thread::spawn(move || {
            result_send
                .send(waiting_limiter.acquire(&waiting_handle).is_none())
                .unwrap();
        });

        assert_eq!(
            result_recv.recv_timeout(Duration::from_millis(20)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        );
        handle.cancel();
        assert_eq!(result_recv.recv_timeout(Duration::from_secs(1)), Ok(true));
        waiter.join().unwrap();

        drop(held_permit);
        assert!(limiter.try_acquire().is_some());
    }

    #[test]
    fn test_concurrency_limiter_wait_wakes_on_release() {
        let limiter = ConcurrencyLimiter::new(1);
        let held_permit = limiter.try_acquire().unwrap();
        let handle = PrefetchHandle::default();
        let waiting_handle = handle.clone();
        let waiting_limiter = limiter.clone();
        let (result_send, result_recv) = std::sync::mpsc::channel();

        let waiter = std::thread::spawn(move || {
            let got = waiting_limiter.acquire(&waiting_handle);
            result_send.send(got.is_some()).unwrap();
            drop(got);
        });

        std::thread::sleep(Duration::from_millis(50));
        drop(held_permit);
        assert_eq!(result_recv.recv_timeout(Duration::from_secs(2)), Ok(true));
        waiter.join().unwrap();
        assert!(limiter.try_acquire().is_some());
    }

    #[test]
    fn test_concurrency_limiter_wait_survives_stolen_permits() {
        // While a prefetch is blocked waiting for a fetch permit, other prefetches keep taking
        // released permits with try_acquire before the waiter runs. The waiter must still wake
        // up once a permit stays free.
        let limiter = ConcurrencyLimiter::new(1);
        let held = limiter.try_acquire().unwrap();
        let handle = PrefetchHandle::default();
        let waiting_handle = handle.clone();
        let waiting_limiter = limiter.clone();
        let (result_send, result_recv) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let got = waiting_limiter.acquire(&waiting_handle);
            result_send.send(got.is_some()).unwrap();
        });
        std::thread::sleep(Duration::from_millis(20));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thieves: Vec<_> = (0..4)
            .map(|_| {
                let limiter = limiter.clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut stolen = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        if let Some(permit) = limiter.try_acquire() {
                            stolen += 1;
                            drop(permit);
                        }
                    }
                    stolen
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(20));
        drop(held);
        std::thread::sleep(Duration::from_millis(200));
        stop.store(true, Ordering::Relaxed);
        let stolen: u64 = thieves.into_iter().map(|t| t.join().unwrap()).sum();
        let woke = result_recv.recv_timeout(Duration::from_millis(500));
        handle.cancel();
        waiter.join().unwrap();
        assert_eq!(
            woke,
            Ok(true),
            "waiter asleep with a free permit after {stolen} steals"
        );
    }

    #[test]
    fn test_prefetch_passes_skip_lfs() -> anyhow::Result<()> {
        let tree_store = Arc::new(TestStore::new());
        let file_store = Arc::new(TestStore::new());

        let path: RepoPathBuf = "file.txt".to_string().try_into()?;
        let hgid = file_store.insert_data(Default::default(), &path, b"content".into())?;

        let mut mf = TreeManifest::ephemeral(tree_store);
        mf.insert(path, FileMetadata::new(hgid, types::FileType::Regular))?;

        let detector = walkdetector::Detector::new();
        let handle = prefetch(
            Config::default(),
            mf,
            file_store.clone(),
            detector,
            PrefetchWork::Files {
                root: "".to_string().try_into()?,
                depth: 0,
                start_after: None,
            },
            None,
        );

        while !handle.is_done() {
            std::thread::sleep(Duration::from_millis(1));
        }

        let contexts = file_store.fetch_contexts();
        assert_eq!(contexts.len(), 1);
        assert!(contexts[0].skip_lfs());

        Ok(())
    }

    /// A file store whose fetches block until released, so a prefetch can be caught in progress.
    #[derive(Clone)]
    struct GatedFileStore {
        inner: Arc<TestStore>,
        released: Arc<AtomicBool>,
    }

    impl KeyStore for GatedFileStore {
        fn get_content_iter(
            &self,
            fctx: FetchContext,
            keys: Vec<Key>,
        ) -> anyhow::Result<storemodel::ContentFetchItems> {
            // Record the fetch first so the test can see it while it is held.
            let items = self.inner.get_content_iter(fctx, keys)?;
            while !self.released.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(items)
        }

        fn get_local_content(&self, path: &RepoPath, hgid: HgId) -> anyhow::Result<Option<Blob>> {
            self.inner.get_local_content(path, hgid)
        }

        fn insert_data(
            &self,
            opts: storemodel::InsertOpts,
            path: &RepoPath,
            data: Blob,
        ) -> anyhow::Result<HgId> {
            self.inner.insert_data(opts, path, data)
        }

        fn format(&self) -> SerializationFormat {
            self.inner.format()
        }

        fn clone_key_store(&self) -> Box<dyn KeyStore> {
            Box::new(self.clone())
        }
    }

    impl FileStore for GatedFileStore {
        fn clone_file_store(&self) -> Box<dyn FileStore + 'static> {
            Box::new(self.clone())
        }
    }

    #[test]
    fn test_deeper_walk_replaces_prefetch() -> anyhow::Result<()> {
        let tree_store = Arc::new(TestStore::new());
        let inner = Arc::new(TestStore::new());
        let released = Arc::new(AtomicBool::new(false));
        let file_store = Arc::new(GatedFileStore {
            inner: inner.clone(),
            released: released.clone(),
        });
        let mut mf = TreeManifest::ephemeral(tree_store.clone());
        for path in ["r/a", "r/b", "r/d1/x", "r/d1/y", "r/d2/x", "r/d2/y"] {
            let repo_path: RepoPathBuf = path.to_string().try_into()?;
            let hgid = inner.insert_data(Default::default(), &repo_path, path.as_bytes().into())?;
            mf.insert(repo_path, FileMetadata::new(hgid, types::FileType::Regular))?;
        }
        let root_hgid = mf.persist(&[])?;
        let mut rng = ChaChaRng::from_seed([0u8; 32]);
        let commit_id = HgId::random(&mut rng);
        let tree_resolver = TestTreeResolver::new(tree_store, [(commit_id, root_hgid)]);

        let mut detector = walkdetector::Detector::new();
        detector.set_walk_threshold(2);
        let kick_manager = prefetch_manager(
            Config::default(),
            Arc::new(tree_resolver),
            file_store,
            Arc::new(RwLock::new(Some(commit_id.to_hex()))),
            detector.clone(),
            "".into(),
            make_test_sparse_matcher,
            Limits::new(&Config::default()),
        );
        let wait_for_fetches = |count: usize| {
            for _ in 0..2000 {
                if inner.fetches().len() >= count {
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            panic!("timed out waiting for {count} fetches");
        };

        detector.file_loaded(RepoPath::from_static_str("r/a"), 0);
        detector.file_loaded(RepoPath::from_static_str("r/b"), 0);
        kick_manager.send(())?;
        wait_for_fetches(1);

        detector.file_loaded(RepoPath::from_static_str("r/d1/x"), 0);
        detector.file_loaded(RepoPath::from_static_str("r/d1/y"), 0);
        detector.file_loaded(RepoPath::from_static_str("r/d2/x"), 0);
        detector.file_loaded(RepoPath::from_static_str("r/d2/y"), 0);
        assert_eq!(
            detector.file_walks(),
            vec![("r".to_string().try_into()?, 1)]
        );
        kick_manager.send(())?;
        wait_for_fetches(2);
        released.store(true, Ordering::Release);

        // The deeper walk restarts a single enumeration from the root, so the depth-0 files are
        // requested again (served locally) along with the depth-1 files.
        let keys = inner.fetches().swap_remove(1);
        assert_eq!(keys.len(), 6, "{keys:?}");

        Ok(())
    }

    #[test]
    fn test_deeper_walk_keeps_prefetch_until_replacement_starts() -> anyhow::Result<()> {
        let tree_store = Arc::new(TestStore::new());
        let inner = Arc::new(TestStore::new());
        let released = Arc::new(AtomicBool::new(false));
        let file_store = Arc::new(GatedFileStore {
            inner: inner.clone(),
            released: released.clone(),
        });
        let mut mf = TreeManifest::ephemeral(tree_store.clone());
        for path in ["r/a", "r/b", "r/d1/x", "r/d1/y", "r/d2/x", "r/d2/y"] {
            let repo_path: RepoPathBuf = path.to_string().try_into()?;
            let hgid = inner.insert_data(Default::default(), &repo_path, path.as_bytes().into())?;
            mf.insert(repo_path, FileMetadata::new(hgid, types::FileType::Regular))?;
        }
        let root_hgid = mf.persist(&[])?;
        let mut rng = ChaChaRng::from_seed([0u8; 32]);
        let commit_id = HgId::random(&mut rng);
        let mut detector = walkdetector::Detector::new();
        detector.set_walk_threshold(2);
        // A single manifest-walk permit, and the manager ticked by hand so its state can be
        // checked between ticks.
        let config = Config {
            max_concurrent_manifest_walks: 1,
            ..Config::default()
        };
        let mut manager = Manager {
            config,
            tree_resolver: Arc::new(TestTreeResolver::new(tree_store, [(commit_id, root_hgid)])),
            file_store,
            current_commit_id: Arc::new(RwLock::new(Some(commit_id.to_hex()))),
            detector: detector.clone(),
            repo_root: "".into(),
            make_matcher: Box::new(make_test_sparse_matcher),
            limits: Limits::new(&config),
            commit: None,
            prefetches: Prefetches::default(),
            last_tick: None,
        };
        let root: RepoPathBuf = "r".to_string().try_into()?;
        let wait_until = |done: &dyn Fn() -> bool, what: &str| {
            for _ in 0..2000 {
                if done() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            panic!("timed out waiting for {what}");
        };

        detector.file_loaded(RepoPath::from_static_str("r/a"), 0);
        detector.file_loaded(RepoPath::from_static_str("r/b"), 0);
        manager.tick();
        assert_eq!(manager.prefetches.files[&root].0, 0);
        let shallow = manager.prefetches.files[&root].1.clone();
        // The depth-0 prefetch holds the only permit while its fetch is held.
        wait_until(
            &|| inner.fetches().len() == 1,
            "the shallow prefetch's fetch",
        );

        detector.file_loaded(RepoPath::from_static_str("r/d1/x"), 0);
        detector.file_loaded(RepoPath::from_static_str("r/d1/y"), 0);
        detector.file_loaded(RepoPath::from_static_str("r/d2/x"), 0);
        detector.file_loaded(RepoPath::from_static_str("r/d2/y"), 0);
        assert_eq!(detector.file_walks(), vec![(root.clone(), 1)]);
        manager.tick();
        // No permit for the deeper prefetch: the shallower one keeps running and the deeper
        // walk is left unhandled for a later tick.
        assert!(shallow.is_in_progress());
        assert_eq!(manager.prefetches.files[&root].0, 0);
        assert!(
            !manager
                .prefetches
                .handled_files
                .contains(&(root.clone(), 1))
        );

        released.store(true, Ordering::Release);
        wait_until(&|| shallow.is_done(), "the shallow prefetch to finish");
        manager.tick();
        assert_eq!(manager.prefetches.files[&root].0, 1);
        wait_until(
            &|| inner.fetches().len() == 2,
            "the deeper prefetch's fetch",
        );

        Ok(())
    }

    #[test]
    fn test_pause_records_resume_point() -> anyhow::Result<()> {
        let store = Arc::new(TestStore::new());
        let file_store = store.clone() as Arc<dyn FileStore>;
        let mut mf = TreeManifest::ephemeral(store.clone());
        let dir: RepoPathBuf = "dir".to_string().try_into()?;
        let mut paths = Vec::new();
        let mut hgids = Vec::new();
        for i in 0..6 {
            let path: RepoPathBuf = format!("dir/f{i}").try_into()?;
            let hgid = file_store.insert_data(
                Default::default(),
                &path,
                format!("content {i}").into_bytes().into(),
            )?;
            mf.insert(
                path.clone(),
                FileMetadata::new(hgid, types::FileType::Regular),
            )?;
            paths.push(path);
            hgids.push(hgid);
        }

        let mut detector = walkdetector::Detector::new();
        detector.set_walk_threshold(2);
        detector.file_loaded(RepoPath::from_str("dir/f0")?, 0);
        detector.file_loaded(RepoPath::from_str("dir/f1")?, 0);
        // Five files already preloaded against two reads: the lead exceeds max_initial_lag and
        // the read ratio is below min_ratio, so the prefetch pauses before its first batch.
        detector.files_preloaded(&dir, 5);
        let config = Config {
            max_initial_lag: 2,
            min_ratio: 0.5,
            ..Default::default()
        };

        let handle = prefetch(
            config,
            mf.clone(),
            file_store.clone(),
            detector.clone(),
            PrefetchWork::Files {
                root: dir.clone(),
                depth: 0,
                start_after: None,
            },
            None,
        );
        while handle.is_in_progress() {
            std::thread::sleep(Duration::from_millis(1));
        }
        // It stopped without fetching, and has no resume point since it fetched nothing.
        assert!(handle.is_paused());
        assert!(!handle.is_done());
        assert_eq!(handle.resume_after(), None);
        assert!(store.fetches().is_empty());

        // Resuming after a file fetches only the files after it.
        let handle = prefetch(
            Config::default(),
            mf,
            file_store,
            walkdetector::Detector::new(),
            PrefetchWork::Files {
                root: dir,
                depth: 0,
                start_after: Some(paths[2].clone()),
            },
            None,
        );
        while !handle.is_done() {
            std::thread::sleep(Duration::from_millis(1));
        }
        let fetched: Vec<HgId> = store
            .fetches()
            .into_iter()
            .flatten()
            .map(|key| key.hgid)
            .collect();
        assert_eq!(fetched, hgids[3..]);

        Ok(())
    }

    #[test]
    fn test_should_pause_prefetch() -> anyhow::Result<()> {
        let config = Config {
            min_ratio: 0.1,
            max_initial_lag: 20,
            min_interval: Duration::from_millis(1),
            ..Default::default()
        };

        let mut detector = walkdetector::Detector::new();
        detector.set_walk_threshold(2);

        let walk_root: RepoPathBuf = "root".to_string().try_into()?;

        assert!(!should_pause_prefetch(
            &detector, &config, &walk_root, false
        ));

        detector.file_loaded(RepoPath::from_str("root/a")?, 0);
        detector.file_loaded(RepoPath::from_str("root/b")?, 0);
        detector.file_read(RepoPath::from_str("root/c")?, 0);

        assert!(!should_pause_prefetch(
            &detector, &config, &walk_root, false
        ));

        detector.files_preloaded(&walk_root, 15);

        // Still not paused - lag is below 20.
        assert!(!should_pause_prefetch(
            &detector, &config, &walk_root, false
        ));

        // Now we are paused
        detector.files_preloaded(&walk_root, 20);
        assert!(should_pause_prefetch(&detector, &config, &walk_root, false));

        for i in 0..100 {
            detector.file_read(RepoPath::from_str(&format!("root/{i})"))?, 0);
        }
        detector.files_preloaded(&walk_root, 500);
        // Ratio is ~100/500 - above min_ratio of 0.1.
        assert!(!should_pause_prefetch(
            &detector, &config, &walk_root, false
        ));

        // Now we are below the min ratio.
        detector.files_preloaded(&walk_root, 700);
        assert!(should_pause_prefetch(&detector, &config, &walk_root, false));

        // Resuming needs the lag down to half the allowance: 19 files ahead of 2 reads does not
        // pause a running prefetch with an allowance of 20, but keeps a paused one paused.
        let mut detector = walkdetector::Detector::new();
        detector.set_walk_threshold(2);
        detector.file_loaded(RepoPath::from_str("dir/f1")?, 0);
        detector.file_loaded(RepoPath::from_str("dir/f2")?, 0);
        let root = RepoPath::from_str("dir")?;
        let config = Config {
            min_ratio: 0.1,
            max_initial_lag: 20,
            ..Default::default()
        };
        detector.files_preloaded(root, 21);
        assert!(!should_pause_prefetch(&detector, &config, root, false));
        assert!(should_pause_prefetch(&detector, &config, root, true));

        Ok(())
    }

    #[test]
    fn test_remote_reads_resume_prefetch() -> anyhow::Result<()> {
        let config = Config {
            min_ratio: 0.5,
            max_initial_lag: 20,
            ..Default::default()
        };
        let mut detector = walkdetector::Detector::new();
        detector.set_walk_threshold(2);
        let root = RepoPath::from_str("root")?;
        detector.file_loaded(RepoPath::from_str("root/a")?, 0);
        detector.file_loaded(RepoPath::from_str("root/b")?, 0);
        detector.files_preloaded(root, 100);
        assert!(should_pause_prefetch(&detector, &config, root, false));

        for i in 0..49 {
            detector.file_loaded(RepoPath::from_str(&format!("root/{i}"))?, 0);
        }
        assert!(!should_pause_prefetch(&detector, &config, root, false));

        detector.files_preloaded(root, 100);
        assert!(should_pause_prefetch(&detector, &config, root, false));
        Ok(())
    }
}
