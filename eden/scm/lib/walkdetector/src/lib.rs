/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests;
mod walk_node;

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[cfg(test)]
use coarsetime as _; // silence "unused dependency" warning
#[cfg(not(test))]
use coarsetime::Instant;
#[cfg(test)]
use mock_instant::Instant;
use parking_lot::RwLock;
use tracing::Level;
use types::RepoPath;
use types::RepoPathBuf;
use walk_node::GcResult;
use walk_node::WalkNode;

// Goals:
//  - Aggressively detect walk and aggressively cancel walk.
//  - Passive - don't fetch or query any stores.
//  - Minimize memory usage.

#[derive(Clone, Default)]
pub struct Detector {
    config: Config,
    root: Option<PathBuf>,
    inner: Arc<RwLock<Inner>>,
}

struct Inner {
    // Last time we ran GC.
    last_gc_time: Instant,
    // Root node used to track walks.
    node: WalkNode,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            last_gc_time: Instant::now(),
            node: WalkNode::new(DEFAULT_GC_TIMEOUT),
        }
    }
}

/// How long GC can take before we warn that it was slow.
const SLOW_OPERATION_THRESHOLD: Duration = Duration::from_millis(100);

impl Detector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset_config(&mut self) {
        self.config = Config::default();
    }

    pub fn set_walk_threshold(&mut self, threshold: usize) {
        self.config.walk_threshold = threshold;
    }

    pub fn set_walk_ratio(&mut self, threshold: f64) {
        if threshold <= 0.0 {
            return;
        }
        self.config.walk_ratio = threshold;
    }

    /// Set the fraction of the walk ratio that applies to in-order accesses. `1.0` turns the
    /// sorted detection off; values outside (0, 1] are ignored.
    pub fn set_walk_ratio_sorted_factor(&mut self, factor: f64) {
        if factor <= 0.0 || factor > 1.0 {
            return;
        }
        self.config.walk_ratio_sorted_factor = factor;
    }

    pub fn set_gc_interval(&mut self, interval: Duration) {
        self.config.gc_interval = interval;
    }

    pub fn set_gc_timeout(&mut self, timeout: Duration) {
        self.config.gc_timeout = timeout;
        // Update root node's timeout as a special case. The root node is never deleted, so has no
        // chance to get current gc_timeout on creation.
        self.inner.write().node.gc_timeout = timeout;
    }

    /// Set root path. Used in some metric collection.
    pub fn set_root(&mut self, root: Option<PathBuf>) {
        self.root = root;
    }

    /// Run GC if needed. Returns whether a walk was removed.
    fn maybe_gc(&self) -> bool {
        if !self.inner.read().needs_gc(&self.config) {
            return false;
        }
        self.inner
            .write()
            .maybe_gc(&self.config)
            .is_some_and(|result| result.deleted_walks > 0)
    }

    /// Return list of (walk root dir, walk depth) representing active file content walks.
    pub fn file_walks(&self) -> Vec<(RepoPathBuf, usize)> {
        self.walks(Some(WalkType::File))
            .into_iter()
            .map(|(root, depth, _)| (root, depth))
            .collect()
    }

    /// Return list of (walk root dir, walk depth) representing active directory walks.
    pub fn dir_walks(&self) -> Vec<(RepoPathBuf, usize)> {
        self.walks(Some(WalkType::Directory))
            .into_iter()
            .map(|(root, depth, _)| (root, depth))
            .collect()
    }

    /// Return list of (walk root dir, walk depth, walk type) representing all active walks.
    pub fn all_walks(&self) -> Vec<(RepoPathBuf, usize, WalkType)> {
        self.walks(None)
    }

    fn walks(&self, walk_type: Option<WalkType>) -> Vec<(RepoPathBuf, usize, WalkType)> {
        let inner = self.inner.read();

        let mut walks = if inner.needs_gc(&self.config) {
            // Only grab write lock if we need to GC.
            drop(inner);
            self.maybe_gc();
            self.inner.read().node.list_walks(walk_type)
        } else {
            inner.node.list_walks(walk_type)
        };

        walks.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));

        walks
    }

    /// Observe a "heavy" or remote file (content) read of `path`.
    /// Returns whether an active walk changed (either created or removed).
    pub fn file_loaded(&self, path: impl AsRef<RepoPath>, pid: u32) -> bool {
        self.file_loaded_with_dir_size(path, None, pid)
    }

    /// Like `file_loaded`, but `dir_size` optionally reports the `(num_files, num_dirs)` of the
    /// directory containing `path`, which fills in that directory's metadata if the detector
    /// hasn't seen the directory itself load.
    pub fn file_loaded_with_dir_size(
        &self,
        path: impl AsRef<RepoPath>,
        dir_size: Option<(usize, usize)>,
        pid: u32,
    ) -> bool {
        let path = path.as_ref();

        tracing::trace!(%path, ?dir_size, "file_loaded");

        // Try lightweight read-only path.
        if self.mark_read(path, WalkType::File, true, pid).is_some() {
            return false;
        }

        let (dir_path, base_name) = match path.split_last_component() {
            // Shouldn't happen - implies a path of "" which is not valid for a file.
            None => return false,
            Some((dir, base)) => (dir, base),
        };

        let mut walk_changed = self.maybe_gc();
        let mut inner = self.inner.write();

        let walk_threshold = self.config.walk_threshold;
        let walk_ratio = self.config.walk_ratio;

        // A hint of zero files for the directory of a file being loaded is stale.
        if let Some((num_files, num_dirs)) = dir_size
            && num_files > 0
            && interesting_metadata(walk_threshold, walk_ratio, Some(num_files), Some(num_dirs))
            && inner
                .node
                .get_node(dir_path)
                .is_none_or(|node| node.total_files().is_none())
        {
            inner.set_metadata(&self.config, dir_path, num_files, num_dirs);
        }

        let (owner, suffix) =
            inner
                .node
                .get_or_create_owning_node(&self.config, WalkType::File, dir_path);

        owner.last_access.bump();

        if let Some(walk) = owner.get_dominating_walk(WalkType::File) {
            tracing::trace!(walk_root=%path.strip_suffix(suffix, true).unwrap_or_default(), file=%path, "file already in walk");
            walk.inc_file_load(
                dir_path.strip_suffix(suffix, true).unwrap_or_default(),
                self.root.as_deref(),
                path,
            );
        } else {
            let my_dir = owner;

            my_dir.seen_files.insert(base_name.to_owned());

            let seen_count = my_dir.seen_files.len();
            if my_dir.is_walked(
                WalkType::File,
                seen_count,
                0,
                walk_threshold,
                self.config.walk_ratio_for(my_dir.seen_files.is_sorted()),
                0,
            ) {
                my_dir.seen_files.clear();
                inner.insert_walk(
                    &self.config,
                    WalkType::File,
                    Walk::for_type(WalkType::File, 0, seen_count as u64, pid),
                    dir_path,
                );
                walk_changed = true;
            }
        }

        walk_changed
    }

    /// Account a "heavy" or remote file read of `path` to the walk already covering it, if any,
    /// taking only the read lock. Returns whether a walk covered it. Otherwise the caller should
    /// report the read with `file_loaded`, which may create or extend walks under the write lock.
    pub fn file_loaded_fast(&self, path: impl AsRef<RepoPath>, pid: u32) -> bool {
        self.mark_read(path.as_ref(), WalkType::File, true, pid)
            .is_some()
    }

    /// Observe a "soft" or cached file (content) access of `path`.
    /// This will not be tracked as a new walk, but will reset TTL of an existing walk.
    /// Returns whether path was covered by an active walk.
    pub fn file_read(&self, path: impl AsRef<RepoPath>, pid: u32) -> bool {
        let path = path.as_ref();
        tracing::trace!(%path, "file_read");
        self.mark_read(path, WalkType::File, false, pid).is_some()
    }

    /// Observe a directory being loaded (i.e. "heavy" or remote read). `num_files` and `num_dirs`
    /// report the number of file and directory children of `path`, respectively. Pass zero if you
    /// don't know. Returns whether an active walk changed (either created or removed).
    pub fn dir_loaded(
        &self,
        path: impl AsRef<RepoPath>,
        num_files: usize,
        num_dirs: usize,
        pid: u32,
    ) -> bool {
        let path = path.as_ref();

        tracing::trace!(%path, num_files, num_dirs, "dir_loaded");

        let is_interesting_metadata = interesting_metadata(
            self.config.walk_threshold,
            self.config.walk_ratio,
            Some(num_files),
            Some(num_dirs),
        );

        // Try lightweight read-only path.
        if self
            .mark_read(path, WalkType::Directory, true, pid)
            .is_some()
        {
            if is_interesting_metadata {
                // Fill in interesting metadata that informs detection of file content walks.
                let mut inner = self.inner.write();
                inner.set_metadata(&self.config, path, num_files, num_dirs);
            }
            return false;
        }

        if is_interesting_metadata {
            // Fill in interesting metadata that informs detection of file content walks.
            self.inner
                .write()
                .set_metadata(&self.config, path, num_files, num_dirs);
        }

        let mut walk_changed = self.maybe_gc();
        let (dir_path, base_name) = match path.split_last_component() {
            None => return walk_changed,
            Some((dir, base)) => (dir, base),
        };

        let mut inner = self.inner.write();

        let walk_threshold = self.config.walk_threshold;

        let (owner, suffix) =
            inner
                .node
                .get_or_create_owning_node(&self.config, WalkType::Directory, dir_path);

        owner.last_access.bump();

        if let Some(walk) = owner.get_dominating_walk(WalkType::Directory) {
            tracing::trace!(walk_root=%dir_path.strip_suffix(suffix, true).unwrap_or_default(), dir=%path, "dir already in walk");
            walk.inc_dir_load(
                dir_path.strip_suffix(suffix, true).unwrap_or_default(),
                self.root.as_deref(),
                path,
            );
        } else {
            let my_dir = owner;

            my_dir.seen_dirs.insert(base_name.to_owned());

            let seen_count = my_dir.seen_dirs.len();
            if my_dir.is_walked(
                WalkType::Directory,
                seen_count,
                0,
                walk_threshold,
                self.config.walk_ratio_for(my_dir.seen_dirs.is_sorted()),
                0,
            ) {
                let walk = Walk::for_type(WalkType::Directory, 0, seen_count as u64, pid);
                walk.credit_breadth(my_dir.total_dirs().unwrap_or_default());
                my_dir.seen_dirs.clear();
                inner.insert_walk(&self.config, WalkType::Directory, walk, dir_path);

                walk_changed = true;
            }
        }

        walk_changed
    }

    /// Account a "heavy" or remote directory load of `path` to the walk already covering it, if
    /// any, taking only the read lock. Returns whether a walk covered it. Directory sizes worth
    /// remembering are left to `dir_loaded`, which records them under the write lock.
    pub fn dir_loaded_fast(
        &self,
        path: impl AsRef<RepoPath>,
        num_files: usize,
        num_dirs: usize,
        pid: u32,
    ) -> bool {
        !interesting_metadata(
            self.config.walk_threshold,
            self.config.walk_ratio,
            Some(num_files),
            Some(num_dirs),
        ) && self
            .mark_read(path.as_ref(), WalkType::Directory, true, pid)
            .is_some()
    }

    /// Observe a "soft" or cached directory access of `path`. This will not be tracked as a new
    /// walk, but will reset TTL of an existing walk. Returns whether path was covered by an
    /// active walk. Takes only the read lock; see `dir_metadata` for remembering its size.
    pub fn dir_read(&self, path: impl AsRef<RepoPath>, pid: u32) -> bool {
        let path = path.as_ref();
        tracing::trace!(%path, "dir_read");
        self.mark_read(path, WalkType::Directory, false, pid)
            .is_some()
    }

    /// Whether a directory with `num_files` files and `num_dirs` directories is large enough that
    /// its size should be remembered with `dir_metadata` even when it was read from cache: it could
    /// still be involved in ongoing or future walk activity that involves remote fetches.
    pub fn is_dir_metadata_important(&self, num_files: usize, num_dirs: usize) -> bool {
        important_metadata(
            self.config.walk_threshold,
            self.config.walk_ratio,
            Some(num_files),
            Some(num_dirs),
        )
    }

    /// Remember the size of directory `path`. Takes the write lock.
    pub fn dir_metadata(&self, path: impl AsRef<RepoPath>, num_files: usize, num_dirs: usize) {
        self.inner
            .write()
            .set_metadata(&self.config, path.as_ref(), num_files, num_dirs);
    }

    /// Record that `preload_count` files were prefetched for the walk rooted at `walk_root`.
    /// Returns the total files preloaded and the total files the walker has read, cached or
    /// remote, so prefetching can be paced against the walker's progress. A walker can read files
    /// in a different order from the prefetch, so its remote reads count as progress too.
    pub fn files_preloaded(
        &self,
        walk_root: impl AsRef<RepoPath>,
        preload_count: u64,
    ) -> (u64, u64) {
        let walk_root = walk_root.as_ref();
        tracing::trace!(%walk_root, preload_count, "files_preloaded");

        let inner = self.inner.read_recursive();

        if let Some(node) = inner.node.get_node(walk_root) {
            if let Some(walk) = node.get_walk_for_type(WalkType::File) {
                let prev_preload = walk
                    .file_preloads
                    .fetch_add(preload_count, Ordering::Relaxed);
                return (
                    prev_preload + preload_count,
                    walk.file_reads.load(Ordering::Relaxed)
                        + walk.file_loads.load(Ordering::Relaxed),
                );
            }
        }

        (0, 0)
    }

    /// "touch" any walk of type wt that covers `path` to update metrics and keep the walk alive.
    /// Returns the root of walk, if any.
    fn mark_read<'a>(
        &'a self,
        path: &'a RepoPath,
        wt: WalkType,
        heavy_read: bool,
        pid: u32,
    ) -> Option<&'a RepoPath> {
        let dir = path.parent()?;

        // Readers must not queue behind a waiting writer: this runs on filesystem threads for
        // every read, while walk bookkeeping on the write lock is best effort.
        let inner = self.inner.read_recursive();

        // Bump last_access, but don't insert any new nodes/walks.
        if let Some((walk_node, suffix)) = inner.node.get_owning_node(wt, dir) {
            walk_node.last_access.bump();

            if let Some(walk) = walk_node.get_dominating_walk(wt) {
                let walk_root = dir.strip_suffix(suffix, true).unwrap_or_default();
                match (wt, heavy_read) {
                    (WalkType::File, false) => {
                        walk.inc_file_read(walk_root, self.root.as_deref(), path)
                    }
                    (WalkType::File, true) => {
                        walk.inc_file_load(walk_root, self.root.as_deref(), path)
                    }
                    (WalkType::Directory, false) => {
                        walk.inc_dir_read(walk_root, self.root.as_deref(), path)
                    }
                    (WalkType::Directory, true) => {
                        walk.inc_dir_load(walk_root, self.root.as_deref(), path)
                    }
                };

                walk.maybe_swap_pid(pid, 1);
            }

            let walk_root = dir.strip_suffix(suffix, true).unwrap_or_default();

            tracing::trace!(%walk_root, ?wt, %path, "already in walk (fastpath)");

            return Some(walk_root);
        }

        None
    }
}

// Whether directory metadata is interesting and we should retain it until normal expiration.
fn interesting_metadata(
    threshold: usize,
    walk_ratio: f64,
    num_files: Option<usize>,
    num_dirs: Option<usize>,
) -> bool {
    // "interesting" means the directory size metadata might influence our walk detection decisions.
    // Basically, we care about very small or very large directories.

    if important_metadata(threshold, walk_ratio, num_files, num_dirs) {
        return true;
    }

    // Work backwards from walk threshold and walk ratio to calculate what size of directory would
    // start to increase our walk threshold (due to the walk ratio).
    let big_dir_threshold: usize = ((threshold + 1) as f64 / walk_ratio) as usize;

    // We don't care about empty directories because we will never see "activity" for an empty
    // directory, so probably won't ever make use of the size hint. Marking empty directories as
    // "not interesting" significantly reduces the number of nodes we create during big walks.
    num_dirs.is_some_and(|dirs| dirs > 0 && dirs < threshold || dirs >= big_dir_threshold)
        || num_files
            .is_some_and(|files| files > 0 && files < threshold || files >= big_dir_threshold)
}

// Whether directory metadata is important and we should retain indefinitely.
fn important_metadata(
    threshold: usize,
    walk_ratio: f64,
    num_files: Option<usize>,
    num_dirs: Option<usize>,
) -> bool {
    // "important" means the directory size metadata should be retained indefinitely.
    // Basically, we really care about very large directories.

    // Work backwards from walk threshold and walk ratio to calculate what size of directory would
    // start to increase our walk threshold (due to the walk ratio).
    let big_dir_threshold: usize = ((threshold + 1) as f64 / walk_ratio) as usize;

    // We mainly care about directories with lots of children directories since they can very
    // quickly cause exponential growth. We also care about directories with a lot of files, but
    // less so since it is a one-time cost vs. an indication of potential extreme growth in the
    // future. For example, a directory with 100 sub-directories is something we should slow down
    // for, but a directory with 100 files is not that big of a deal. So, increase threshold by 10x
    // for the file check. This greatly reduces the number of WalkNodes we keep indefinitely after
    // large walks.
    num_dirs.is_some_and(|dirs| dirs >= big_dir_threshold)
        || num_files.is_some_and(|files| files >= 10 * big_dir_threshold)
}

impl Inner {
    /// Insert a new Walk rooted at `dir`.
    #[tracing::instrument(level = "debug", skip(self, config))]
    fn insert_walk(&mut self, config: &Config, walk_type: WalkType, walk: Walk, dir: &RepoPath) {
        // TODO: consider moving "should merge" logic into `WalkNode::insert_walk` to do
        // more work in a single traversal.

        // A walk rooted inside an existing walk's coverage (merged or split off from it) was
        // justified by that walk's breadth proofs, so it keeps the credit.
        if let Some((owner, _)) = self.node.get_owning_node(walk_type, dir) {
            if let Some(owner_walk) = owner.get_dominating_walk(walk_type) {
                walk.credit_breadth(owner_walk.credited_breadth());
            }
        }

        let walk_node = self.node.insert_walk(config, walk_type, dir, walk);
        walk_node.last_access.bump();

        // Check if we should immediately promote this walk to parent directory. This is
        // similar to the ancestor advancement below, except that it can insert a new
        // walk.
        if self
            .maybe_merge_into_parent(config, walk_type, dir)
            .is_some()
        {
            return;
        }

        // Check if we should merge with cousins (into grandparent). This is similar to
        // maybe_merge_into_parent in that it can insert a new walk.
        if self
            .maybe_merge_into_grandparent(config, walk_type, dir)
            .is_some()
        {
            return;
        }

        // Check if we have a containing walk whose depth boundary should be increased.
        if let Some((ancestor_dir, new_depth, crossed)) =
            self.should_advance_ancestor_walk(config, walk_type, dir)
        {
            let walk = Walk::new(new_depth);
            walk.credit_breadth(crossed);
            self.insert_walk(config, walk_type, walk, ancestor_dir);
        }
    }

    /// If `dir` (and its siblings) imply a walk at `dir.parent()`, insert a walk at `dir.parent()`.
    /// Siblings count once they have had a walk during the parent's current run of activity, so
    /// they don't all need to be live at the same time.
    fn maybe_merge_into_parent<'a>(
        &mut self,
        config: &Config,
        walk_type: WalkType,
        dir: &'a RepoPath,
    ) -> Option<()> {
        tracing::trace!(%dir, "maybe_merge_into_parent");

        let (parent_dir, name) = dir.split_last_component()?;
        let parent_node = self.node.get_node_mut(parent_dir)?;

        // A parent walk that already reaches one level down contains anything this could insert;
        // activity under it counts towards advancing that walk instead.
        if parent_node
            .get_dominating_walk(walk_type)
            .is_some_and(|w| w.depth >= 1)
        {
            return None;
        }

        let (kin_count, sorted) = parent_node.insert_walked_child(walk_type, name.to_owned());

        let walk = should_merge_into_ancestor(
            config.walk_threshold,
            config.walk_ratio_for(sorted),
            walk_type,
            dir,
            kin_count,
            parent_node,
            1,
        )?;

        self.insert_walk(config, walk_type, walk, parent_dir);

        Some(())
    }

    /// If `dir` (and its cousins) imply a walk at `dir.parent().parent()`, insert a walk
    /// at `dir.parent().parent()`.
    fn maybe_merge_into_grandparent(
        &mut self,
        config: &Config,
        walk_type: WalkType,
        dir: &RepoPath,
    ) -> Option<()> {
        tracing::trace!(%dir, "maybe_merge_into_grandparent");

        let parent_dir = dir.parent()?;
        let grandparent_dir = parent_dir.parent()?;

        // Merging cousins willy nilly is too aggressive. We require that the cousins' parents are
        // already contained by a walk. This means we are only advancing a walk across one level,
        // not two.
        let (ancestor, suffix) = self.node.get_owning_node(walk_type, parent_dir)?;
        // If suffix is empty, the walk is for parent_dir itself. We want a higher walk.
        if suffix.is_empty() {
            return None;
        }

        let grandparent_node = ancestor.get_node(suffix.parent()?)?;

        let kin_count = live_kin_count(grandparent_node, walk_type, 2);

        let walk = should_merge_into_ancestor(
            config.walk_threshold,
            config.walk_ratio,
            walk_type,
            dir,
            kin_count,
            grandparent_node,
            2,
        )?;

        self.insert_walk(config, walk_type, walk, grandparent_dir);

        Some(())
    }

    /// If a walk at `dir` suggests we can advance the depth of a containing walk, return
    /// (containing_dir, new_depth, breadth credited to the new walk).
    fn should_advance_ancestor_walk<'a>(
        &mut self,
        config: &Config,
        walk_type: WalkType,
        dir: &'a RepoPath,
    ) -> Option<(&'a RepoPath, usize, usize)> {
        let parent_dir = dir.parent()?;
        let (ancestor, suffix) = self.node.get_owning_node_mut(walk_type, parent_dir)?;
        let ancestor_dir = parent_dir.strip_suffix(suffix, true)?;

        let head = if suffix.is_empty() {
            // If we have no suffix, then `ancestor_dir == parent_dir`. The name of the advanced
            // child is the last part of `dir`.
            dir.split_last_component()?.1
        } else {
            // If we have a suffix, that means `ancestor_dir != parent_dir`. The name of the
            // advanced child is the first part of the suffix.
            suffix.split_first_component()?.0
        };

        let threshold = config.walk_threshold;

        // Check if the containing walk's node has N children with descendants that
        // have pushed to the next depth. The idea is we want some confidence before
        // expanding a huge walk deeper, so we wait until we've seen depth
        // advancements that bubble up to at least N different children of the walk
        // root.
        let (num_advanced_children, num_advanced_descendants, child_seen_count) =
            ancestor.insert_advanced_child(walk_type, head.to_owned());

        tracing::trace!(
            %ancestor_dir,
            num_advanced_children,
            num_advanced_descendants,
            child_seen_count,
            threshold,
            total_dirs=ancestor.total_dirs(),
            "should_advance_ancestor_walk",
        );

        let ancestor_walk = ancestor.get_dominating_walk(walk_type);
        let ancestor_walk_depth = ancestor_walk.map_or(0, |w| w.depth);
        let credit = ancestor_walk.map_or(0, |w| w.credited_breadth());

        // Check that both:
        //    - We are walked at depth=0 based on num_advanced_children. This ensures the walk is
        //    "wide" enough (i.e. enough of our direct children directories have advanced walks).
        //    - We are walked at depth=N+1 based on num_advanced_descendants. This ensures we don't
        //    prematurely advance across a huge directory at depth N+1.
        let is_walked = ancestor.is_walked(
            WalkType::Directory,
            num_advanced_children,
            0,
            threshold,
            config.walk_ratio,
            credit,
        ) && ancestor.is_walked(
            WalkType::Directory,
            num_advanced_descendants,
            ancestor_walk_depth,
            threshold,
            config.walk_ratio,
            credit,
        );

        if is_walked {
            let depth = ancestor_walk_depth + 1;
            let crossed = credit
                .max(
                    ancestor
                        .total_count(WalkType::Directory, 0)
                        .unwrap_or_default(),
                )
                .max(
                    ancestor
                        .total_count(WalkType::Directory, ancestor_walk_depth)
                        .unwrap_or_default(),
                );
            tracing::debug!(dir=%ancestor_dir, depth, crossed, "expanding walk boundary");
            return Some((ancestor_dir, depth, crossed));
        } else if !suffix.is_empty() {
            // If suffix isn't empty (i.e. dir isn't a direct child of ancestor), check if
            // ancestor's direct child `name` should be split off into a separate walk. This helps
            // in cases such as root directory "foo" has a child directory "foo/bar" that is very
            // wide and deep. Ideally the walk at "foo" could advance to cover everything, but at
            // some point "foo/bar" will be its only child with activity. Splitting off a walk for
            // "foo/bar" allows all the activity under "foo/bar" to be consolidated easily to a
            // single walk.

            let threshold = config.walk_threshold;
            let split_off_child = match ancestor.get_node(head.as_ref()) {
                // If we have directory size hints, use them.
                Some(child) => {
                    let child_depth = ancestor_walk_depth.saturating_sub(1);
                    (child.get_walk_for_type(walk_type).is_none()
                        // Similar to above, check for evidence of advanced walk activity for
                        // direct children and at depth N+1.
                        && child.is_walked(
                            WalkType::Directory,
                            child_seen_count,
                            0,
                            threshold,
                            config.walk_ratio,
                            0,
                        )
                        && child.is_walked(
                            WalkType::Directory,
                            child_seen_count,
                            child_depth,
                            threshold,
                            config.walk_ratio,
                            0,
                        ))
                    .then(|| {
                        child
                            .total_count(WalkType::Directory, 0)
                            .unwrap_or_default()
                            .max(
                                child
                                    .total_count(WalkType::Directory, child_depth)
                                    .unwrap_or_default(),
                            )
                    })
                }
                // Otherwise use default threshold.
                None => (child_seen_count >= threshold).then_some(0),
            };

            if let Some(crossed) = split_off_child {
                // If parent=ancestor/foo/bar/parent and suffix=foo/bar/parent,
                // we want "ancestor/foo", so strip suffix[1:] from parent.
                let child_path =
                    parent_dir.strip_suffix(suffix.split_first_component()?.1, true)?;
                tracing::debug!(%child_path, depth=ancestor_walk_depth, "splitting off walk for child");
                return Some((child_path, ancestor_walk_depth, crossed));
            }
        }

        None
    }

    #[allow(clippy::useless_conversion)]
    fn needs_gc(&self, config: &Config) -> bool {
        self.last_gc_time.elapsed() >= config.gc_interval.into()
    }

    /// Returns GC stats, or None if GC did not run.
    fn maybe_gc(&mut self, config: &Config) -> Option<GcResult> {
        if !self.needs_gc(config) {
            return None;
        }

        let start = std::time::Instant::now();

        let result = self.node.gc(config);

        let elapsed = start.elapsed();

        if elapsed > SLOW_OPERATION_THRESHOLD {
            tracing::warn!(
                ?elapsed,
                deleted_nodes = result.deleted_nodes,
                remaining_nodes = result.remaining_nodes,
                deleted_walks = result.deleted_walks,
                "walk detector GC was slow"
            );
        } else if result.deleted_nodes > 0
            || result.deleted_walks > 0
            || elapsed > Duration::from_millis(5)
        {
            tracing::debug!(
                ?elapsed,
                deleted_nodes = result.deleted_nodes,
                remaining_nodes = result.remaining_nodes,
                deleted_walks = result.deleted_walks,
                "GC complete"
            );
        }

        self.last_gc_time = Instant::now();

        Some(result)
    }

    fn set_metadata(&mut self, config: &Config, dir: &RepoPath, num_files: usize, num_dirs: usize) {
        tracing::trace!(%dir, num_files, num_dirs, "setting directory metadata");
        self.node.set_metadata(config, dir, num_files, num_dirs);
    }
}

/// Count nodes `distance` below `ancestor` that have a live walk of `walk_type`.
fn live_kin_count(ancestor: &WalkNode, walk_type: WalkType, distance: usize) -> usize {
    let mut kin_count = 0;
    ancestor.iter(|node, depth| -> bool {
        if depth == distance {
            if node.get_walk_for_type(walk_type).is_some() {
                kin_count += 1;
            }
        }

        depth < distance
    });
    kin_count
}

/// Check if `kin_count` walks at nodes `ancestor_distance` below `ancestor` should be "merged"
/// into a new walk at `ancestor`. Returns the new walk to be inserted, if any.
fn should_merge_into_ancestor(
    walk_threshold: usize,
    walk_ratio: f64,
    walk_type: WalkType,
    dir: &RepoPath,
    kin_count: usize,
    ancestor: &WalkNode,
    ancestor_distance: usize,
) -> Option<Walk> {
    tracing::trace!(
        %dir,
        kin_count,
        ancestor_distance,
        walk_threshold,
        walk_ratio,
        total_dirs=ancestor.total_dirs(),
        "should_merge_into_ancestor",
    );

    if ancestor.is_walked(
        WalkType::Directory, // We are combining directories here, so always use directory count.
        kin_count,
        ancestor_distance - 1,
        walk_threshold,
        walk_ratio,
        ancestor
            .get_dominating_walk(walk_type)
            .map_or(0, |w| w.credited_breadth()),
    ) {
        tracing::debug!(
            %dir,
            kin_count,
            ancestor_distance,
            walk_threshold,
            walk_ratio,
            total_dirs=ancestor.total_dirs(),
            "combining with collateral kin",
        );
        let walk = Walk::new(ancestor_distance);
        walk.credit_breadth(
            ancestor
                .total_count(WalkType::Directory, ancestor_distance - 1)
                .unwrap_or_default(),
        );
        Some(walk)
    } else {
        None
    }
}

// How many children must be accessed in a directory to consider the directory "walked".
const DEFAULT_WALK_THRESHOLD: usize = 3;

// If we know the total dir size, make sure walk threshold is at least 3% of dir size.
// This avoids detecting walks for enormous directories too quickly.
const DEFAULT_WALK_RATIO: f64 = 0.03;

// The fraction of the walk ratio that applies while a directory's children have all been
// accessed in sorted order. In-order access is how a lister visits a directory, so a quarter of
// the usual share of a huge directory is enough evidence.
const DEFAULT_WALK_RATIO_SORTED_FACTOR: f64 = 0.25;

// How often we garbage collect stale nodes.
// We do not rely on running a full GC to expire old walks, only to clean up memory.
const DEFAULT_GC_INTERVAL: Duration = Duration::from_secs(10);

// How stale a walk must be before we remove it.
const DEFAULT_GC_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone)]
struct Config {
    // "How many children must be accessed before we consider parent walked?"
    // This is the main threshold to tune detector aggro.
    walk_threshold: usize,
    // A minimum walk threshold as ratio of total directory size. This only applies when we know the
    // size of the directory. This slows down walk detection in humongous directories.
    walk_ratio: f64,
    // Fraction of `walk_ratio` that applies while the accesses counted for a directory have all
    // come in sorted order; 1.0 means in-order access gets no credit.
    walk_ratio_sorted_factor: f64,
    // How often we run a full GC to clean up memory. We do not rely on a full GC to functionally
    // expire inactive walks.
    gc_interval: Duration,
    // How long after a node was last accessed until we GC it.
    gc_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            walk_threshold: DEFAULT_WALK_THRESHOLD,
            walk_ratio: DEFAULT_WALK_RATIO,
            walk_ratio_sorted_factor: DEFAULT_WALK_RATIO_SORTED_FACTOR,
            gc_interval: DEFAULT_GC_INTERVAL,
            gc_timeout: DEFAULT_GC_TIMEOUT,
        }
    }
}

impl Config {
    /// The walk ratio to hold a directory to, given whether its counted accesses have all come in
    /// sorted order.
    fn walk_ratio_for(&self, sorted: bool) -> f64 {
        if sorted {
            self.walk_ratio * self.walk_ratio_sorted_factor
        } else {
            self.walk_ratio
        }
    }
}

#[derive(Default)]
pub struct Walk {
    // Depth of this walk (relative to the root directory of the walk).
    depth: usize,

    // How many heavy/remote file fetches we've observed under this walk.
    file_loads: AtomicU64,
    // How many light/cached file fetches we've observed under this walk.
    file_reads: AtomicU64,
    // How many remote file prefetches we've been informed about for this walk.
    file_preloads: AtomicU64,
    // How many heavy/remote dir fetches we've observed under this walk.
    dir_loads: AtomicU64,
    // How many light/cached dir fetches we've observed under this walk.
    dir_reads: AtomicU64,

    // Largest directory count this walk has satisfied `walk_ratio` against. Advancing across
    // levels no wider than this only needs the base walk threshold.
    credited_breadth: AtomicUsize,

    // Most commonly seen pid for this walk. Wrapped in an Arc so it can be shared with the
    // background thread that resolves pid_detail (see spawn_pid_detail_resolution).
    pid: Arc<AtomicU64>,
    // Human readable detail about the walking process (its ancestry). Resolved lazily on a
    // background thread - the resolution can block, so it must not run on the calling (FS
    // dispatch) thread. See spawn_pid_detail_resolution.
    pid_detail: Arc<OnceLock<String>>,
    // Set once we've kicked off background resolution of pid_detail, so we only ever spawn one
    // resolver thread per walk.
    pid_detail_resolving: AtomicBool,

    // When walk first started - used to estimate walk duration.
    start_time: AtomicInstant,

    // Whether we have already logged the start of this walk.
    logged_start: AtomicBool,

    // Whether we have logged the end of this walk.
    #[cfg(test)]
    logged_end: Arc<AtomicBool>,
}

impl std::fmt::Debug for Walk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Don't include all the counters - they are very noisy in traces.
        f.debug_struct("Walk").field("depth", &self.depth).finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkType {
    File,
    Directory,
}

impl Walk {
    /// Arbitrary threshold for logging a "big" walk.
    const BIG_WALK_THRESHOLD: u64 = 10_000;

    fn new(depth: usize) -> Self {
        let start_time = AtomicInstant::new();
        start_time.bump();
        Self {
            depth,
            start_time,
            ..Default::default()
        }
    }

    fn for_type(t: WalkType, depth: usize, initial_loads: u64, pid: u32) -> Self {
        let w = Self::new(depth);
        w.pid.store(pid as u64, Ordering::Relaxed);
        let counter = match t {
            WalkType::Directory => &w.dir_loads,
            WalkType::File => &w.file_loads,
        };
        counter.fetch_add(initial_loads, Ordering::Relaxed);
        w
    }

    fn credited_breadth(&self) -> usize {
        self.credited_breadth.load(Ordering::Relaxed)
    }

    fn credit_breadth(&self, breadth: usize) {
        self.credited_breadth.fetch_max(breadth, Ordering::Relaxed);
    }

    /// Take over `other`'s credit. Only for a walk replacing `other` at the same root; a
    /// descendant's crossing says nothing about the rest of this walk's subtree.
    fn absorb_credit(&self, other: &Self) {
        self.credit_breadth(other.credited_breadth());
    }

    fn absorb_counters(&self, other: &Self) {
        // If we are absorbing another walk whose start has already been logged, don't log the start
        // again. There might be a lot of start events as walks coalesce; the important event is
        // logged when the fully coalesced walk ends.
        if other.logged_start.load(Ordering::Relaxed) {
            // Race conditions are not a big deal.
            self.logged_start.store(true, Ordering::Relaxed);
        }

        // Will not notice beginning of "big walk", but next file access (or GC) will.
        self.file_loads
            .fetch_add(other.file_loads.load(Ordering::Relaxed), Ordering::AcqRel);
        self.dir_loads
            .fetch_add(other.dir_loads.load(Ordering::Relaxed), Ordering::AcqRel);
        self.file_reads
            .fetch_add(other.file_reads.load(Ordering::Relaxed), Ordering::AcqRel);
        self.dir_reads
            .fetch_add(other.dir_reads.load(Ordering::Relaxed), Ordering::AcqRel);
        self.file_preloads.fetch_add(
            other.file_preloads.load(Ordering::Relaxed),
            Ordering::AcqRel,
        );

        // If we are taking the other walk's pid and detail, first claim the pid-detail guard so a
        // concurrent resolver cannot race in and later overwrite the pid with a detail from a
        // different process. If the guard is already claimed, a resolution is in flight and
        // `maybe_swap_pid` would reject the swap, so leave the existing pid/detail pair alone.
        let other_pid = other.pid.load(Ordering::Relaxed) as u32;
        let other_total_accesses = other.total_accesses();
        if let Some(detail) = other.pid_detail.get() {
            if self
                .pid_detail_resolving
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                if self.maybe_swap_pid_without_detail_guard(other_pid, other_total_accesses)
                    || (other_pid > 0 && other_pid == self.pid())
                {
                    let _ = self.pid_detail.set(detail.to_string());
                } else {
                    self.pid_detail_resolving.store(false, Ordering::Release);
                }
            }
        } else {
            self.maybe_swap_pid(other_pid, other_total_accesses);
        }

        // Take the earliest start time.
        if let Some(other_start) = other.start_time.load() {
            if self
                .start_time
                .load()
                .is_none_or(|my_start| other_start < my_start)
            {
                self.start_time.store(other_start);
            }
        }
    }

    fn inc_file_load(&self, walk_root: &RepoPath, repo_root: Option<&Path>, path: &RepoPath) {
        if self.file_loads.fetch_add(1, Ordering::AcqRel) >= Self::BIG_WALK_THRESHOLD - 1 {
            self.maybe_log_big_walk(walk_root, repo_root, path);
        }
    }

    fn inc_file_read(&self, walk_root: &RepoPath, repo_root: Option<&Path>, path: &RepoPath) {
        if self.file_reads.fetch_add(1, Ordering::Relaxed) >= Self::BIG_WALK_THRESHOLD - 1 {
            self.maybe_log_big_walk(walk_root, repo_root, path);
        }
    }

    fn inc_dir_load(&self, walk_root: &RepoPath, repo_root: Option<&Path>, path: &RepoPath) {
        if self.dir_loads.fetch_add(1, Ordering::AcqRel) >= Self::BIG_WALK_THRESHOLD - 1 {
            self.maybe_log_big_walk(walk_root, repo_root, path);
        }
    }

    fn inc_dir_read(&self, walk_root: &RepoPath, repo_root: Option<&Path>, path: &RepoPath) {
        if self.dir_reads.fetch_add(1, Ordering::Relaxed) >= Self::BIG_WALK_THRESHOLD - 1 {
            self.maybe_log_big_walk(walk_root, repo_root, path);
        }
    }

    fn total_accesses(&self) -> u64 {
        self.dir_loads.load(Ordering::Relaxed)
            + self.dir_reads.load(Ordering::Relaxed)
            + self.file_loads.load(Ordering::Relaxed)
            + self.file_reads.load(Ordering::Relaxed)
    }

    /// Probabilistically set self.pid=pid based on size of numerator relative to size of self.
    /// Returns whether we took the new pid.
    fn maybe_swap_pid(&self, pid: u32, numerator: u64) -> bool {
        // If pid detail is filled in or resolving, don't swap to a new pid, lest they mismatch.
        if self.pid_detail.get().is_some() || self.pid_detail_resolving.load(Ordering::Acquire) {
            return false;
        }

        self.maybe_swap_pid_without_detail_guard(pid, numerator)
    }

    fn maybe_swap_pid_without_detail_guard(&self, pid: u32, numerator: u64) -> bool {
        let current_pid = self.pid();

        // Don't swap to a 0 pid to favor having some pid info available.
        if pid > 0
            && pid != current_pid
            && (current_pid == 0 || // Always take the new pid if current pid not set.
                // Give the new pid a probabilistic chance to "take over" based on its weight.
                rand::random_range(0..self.total_accesses().max(1)) < numerator)
        {
            self.pid.store(pid as u64, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Resolve `pid_detail` (and possibly `pid`) for this walk on a dedicated background thread.
    ///
    /// This MUST NOT run on the calling thread. On macOS EdenFS serves the working copy over NFS
    /// and drives the walk detector from its NFS dispatch thread pool. Resolving the pid is
    /// expensive: we scan every process's open files and call realpath() on a path that lives
    /// inside the EdenFS mount. realpath() issues a filesystem request that must be serviced by the
    /// NFS dispatch threads - the very threads that call into the walk detector. Doing this inline
    /// (as it previously did, inside a OnceLock initializer) deadlocks the daemon: the calling
    /// dispatch thread blocks in realpath() while initializing the OnceLock, every other dispatch
    /// thread then blocks waiting on that OnceLock, and no thread is left to service the realpath()
    /// request. Running the resolution on a separate thread keeps it off the dispatch threads (and
    /// out of the walk detector lock) entirely.
    fn spawn_pid_detail_resolution(&self, repo_root: Option<&Path>, path: &RepoPath) {
        // Resolve at most once per walk. If resolution is already in flight or done, do nothing. We
        // never clear this on success because the walk has its pid detail by then.
        if self.pid_detail_resolving.swap(true, Ordering::AcqRel) {
            return;
        }

        let tracing_enabled =
            tracing::enabled!(Level::INFO) || tracing::enabled!(target: "big_walk", Level::DEBUG);
        if !tracing_enabled {
            self.pid_detail_resolving.store(false, Ordering::Release);
            return;
        }

        // Now that the resolving guard is set, future pid swaps are blocked. A caller that already
        // passed the guard may still race with this best-effort telemetry update, so capture the pid
        // as late as possible in the resolver thread.
        let pid_atomic = Arc::clone(&self.pid);
        let pid_detail = Arc::clone(&self.pid_detail);
        let full_path =
            repo_root.and_then(|root| root.join(path.as_str()).to_str().map(str::to_owned));

        let spawned = std::thread::Builder::new()
            .name("walkdet-pid".to_owned())
            .spawn(move || {
                let detail = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let pid = pid_atomic.load(Ordering::Relaxed) as u32;
                    let pid_was_zero_at_spawn = pid == 0;
                    let pid = if pid_was_zero_at_spawn {
                        #[cfg(target_os = "macos")]
                        {
                            if let Some(full_path) = full_path {
                                procinfo::macos::file_path_to_pid(std::path::Path::new(&full_path))
                            } else {
                                0
                            }
                        }

                        #[cfg(not(target_os = "macos"))]
                        {
                            let _ = full_path;
                            0
                        }
                    } else {
                        pid
                    };

                    if pid_was_zero_at_spawn && pid > 0 {
                        pid_atomic.store(pid as u64, Ordering::Relaxed);
                    }

                    if pid > 0 {
                        procinfo::ancestors(pid)
                    } else {
                        String::new()
                    }
                }))
                .unwrap_or_else(|_| String::new());

                let _ = pid_detail.set(detail);
            });

        // If we couldn't spawn the thread, clear the guard so a later access can retry. We don't
        // fall back to resolving inline - that is exactly the path that can deadlock.
        if let Err(err) = spawned {
            tracing::warn!(
                ?err,
                "failed to spawn walkdetector pid detail resolver thread"
            );
            self.pid_detail_resolving.store(false, Ordering::Release);
        }
    }

    fn maybe_log_big_walk(&self, walk_root: &RepoPath, repo_root: Option<&Path>, path: &RepoPath) {
        // Kick off resolution of the pid detail at the start of the walk. If we wait until the walk
        // ends, the walking process may have exited. We start resolution before checking
        // logged_start because it is possible that logged_start is true before pid_detail is set due
        // to absorbing a logged walk.
        self.spawn_pid_detail_resolution(repo_root, path);

        if self.logged_start.swap(true, Ordering::AcqRel) {
            return;
        }

        tracing::info!(
            root=%walk_root,
            file_loads = self.file_loads.load(Ordering::Relaxed),
            file_reads = self.file_reads.load(Ordering::Relaxed),
            file_preloads = self.file_preloads.load(Ordering::Relaxed),
            dir_loads = self.dir_loads.load(Ordering::Relaxed),
            dir_reads = self.dir_reads.load(Ordering::Relaxed),
            walker_detail = self.pid_detail.get(),
            "big walk started",
        );
    }

    fn log_end(&self, root: &RepoPath, end_time: Instant) {
        #[cfg(test)]
        self.logged_end.store(true, Ordering::Relaxed);

        if self.file_loads.load(Ordering::Relaxed) >= Self::BIG_WALK_THRESHOLD
            || self.file_reads.load(Ordering::Relaxed) >= Self::BIG_WALK_THRESHOLD
            || self.file_preloads.load(Ordering::Relaxed) >= Self::BIG_WALK_THRESHOLD
            || self.dir_loads.load(Ordering::Relaxed) >= Self::BIG_WALK_THRESHOLD
            || self.dir_reads.load(Ordering::Relaxed) >= Self::BIG_WALK_THRESHOLD
        {
            let duration = self
                .start_time
                .load()
                .map(|t| end_time.duration_since(t).as_secs())
                .unwrap_or_default();

            tracing::info!(
                %root,
                file_loads = self.file_loads.load(Ordering::Relaxed),
                file_reads = self.file_reads.load(Ordering::Relaxed),
                file_preloads = self.file_preloads.load(Ordering::Relaxed),
                dir_loads = self.dir_loads.load(Ordering::Relaxed),
                dir_reads = self.dir_reads.load(Ordering::Relaxed),
                walk_depth = self.depth,
                walker_detail = self.pid_detail.get(),
                walk_duration = duration,
                "big walk ended",
            );

            tracing::debug!(
                target: "big_walk",
                walk_root = root.as_str(),
                file_loads = self.file_loads.load(Ordering::Relaxed),
                file_reads = self.file_reads.load(Ordering::Relaxed),
                file_preloads = self.file_preloads.load(Ordering::Relaxed),
                dir_loads = self.dir_loads.load(Ordering::Relaxed),
                dir_reads = self.dir_reads.load(Ordering::Relaxed),
                walk_depth = self.depth,
                walker_detail = self.pid_detail.get().map(String::as_str).unwrap_or_default(),
                walk_duration = duration,
            );
        }
    }

    #[cfg(test)]
    fn counters(&self) -> (u64, u64, u64, u64, u64) {
        (
            self.file_loads.load(Ordering::Relaxed),
            self.file_reads.load(Ordering::Relaxed),
            self.file_preloads.load(Ordering::Relaxed),
            self.dir_loads.load(Ordering::Relaxed),
            self.dir_reads.load(Ordering::Relaxed),
        )
    }

    fn pid(&self) -> u32 {
        self.pid.load(Ordering::Relaxed) as u32
    }
}

/// Base epoch value for AtomicInstant. Instant::now isn't const, so we use LazyLock. We could use
/// SystemTime::UNIX_EPOCH, but then we lose monotonicity.
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

// Atomic instant with millisecond precision relative to EPOCH.
// Negative value means "not set".
struct AtomicInstant(AtomicI64);

impl Default for AtomicInstant {
    fn default() -> Self {
        Self::new()
    }
}

impl AtomicInstant {
    /// Initialize an empty AtomicInstant. `load()` will return `None` until `store()` is called.
    fn new() -> Self {
        // Eagerly initialize EPOCH to reduce the chance an Instant::now() is smaller than EPOCH.
        let _ = *EPOCH;

        Self(AtomicI64::new(i64::MIN))
    }

    /// Set time to "now".
    fn bump(&self) {
        self.store(Instant::now());
    }

    fn store(&self, value: Instant) {
        let epoch = *EPOCH;

        // It is theoretically possible for `value` to be smaller than EPOCH.
        let int_value = if value < epoch {
            0
        } else {
            value.duration_since(epoch).as_millis() as i64
        };

        self.0.store(int_value, Ordering::Relaxed);
    }

    #[allow(clippy::useless_conversion)]
    fn load(&self) -> Option<Instant> {
        match self.0.load(Ordering::Relaxed) {
            v if v < 0 => None,
            v => Some(*EPOCH + Duration::from_millis(v as u64).into()),
        }
    }

    /// Reset to "not set" state. `load()` will return `None` until `store()` is called.
    fn reset(&self) {
        self.0.store(i64::MIN, Ordering::Relaxed);
    }
}
