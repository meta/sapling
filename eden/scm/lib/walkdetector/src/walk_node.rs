/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::time::Duration;

#[cfg(test)]
use coarsetime as _; // silence "unused dependency" warning
#[cfg(not(test))]
use coarsetime::Instant;
#[cfg(test)]
use mock_instant::Instant;
use types::PathComponentBuf;
use types::RepoPath;
use types::RepoPathBuf;

use crate::AtomicInstant;
use crate::Config;
use crate::Walk;
use crate::WalkType;
use crate::important_metadata;
use crate::interesting_metadata;

pub(crate) struct GcResult {
    pub(crate) deleted_nodes: usize,
    pub(crate) remaining_nodes: usize,
    pub(crate) deleted_walks: usize,
}

/// Child names recorded for one kind of activity, and whether they have arrived in ascending
/// order so far. Tools that list a directory (`ls`, `find`, serial `grep`) visit its entries in
/// order while random access does not, so an in-order run is stronger evidence of a walk than
/// the count alone and the detector holds it to a fraction of the walk ratio. The set stays
/// small: it is cleared once the walk forms.
#[derive(Default)]
pub(crate) struct SeenNames {
    names: BTreeSet<PathComponentBuf>,
    // Set once a new name sorted before one already recorded; only `clear` resets it.
    out_of_order: bool,
}

impl SeenNames {
    /// Record `name`. A name recorded before carries no new order information.
    pub(crate) fn insert(&mut self, name: PathComponentBuf) {
        if self.names.contains(&name) {
            return;
        }
        if self.names.last().is_some_and(|last| name < *last) {
            self.out_of_order = true;
        }
        self.names.insert(name);
    }

    /// Start over from `names`, whose arrival order is unknown: later names count as in order
    /// only if they sort after all of them.
    pub(crate) fn reset_to(&mut self, names: impl IntoIterator<Item = PathComponentBuf>) {
        self.clear();
        self.names.extend(names);
    }

    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether every name recorded so far arrived in ascending order.
    pub(crate) fn is_sorted(&self) -> bool {
        !self.out_of_order
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Tree structure to track active walks. This makes it efficient to find a file's
/// "containing" walk, and to efficiently discover a walk's siblings, cousins, etc. in
/// order to merge walks.
pub(crate) struct WalkNode {
    // File content walk, if any, rooted at this node.
    // The Duration is the GC timeout.
    pub(crate) file_walk: Option<Walk>,
    // Directory content walk, if any, rooted at this node.
    // The Duration is the GC timeout.
    pub(crate) dir_walk: Option<Walk>,

    pub(crate) last_access: AtomicInstant,
    pub(crate) gc_timeout: Duration,
    pub(crate) children: HashMap<PathComponentBuf, WalkNode>,

    // Child directories that have a walked descendant "advanced" past our current
    // walk.depth.
    pub(crate) advanced_file_children: HashMap<PathComponentBuf, usize>,
    pub(crate) advanced_dir_children: HashMap<PathComponentBuf, usize>,

    // Child directories that have had a walk rooted at them while we had no walk
    // covering them (only used before transitioning to walk).
    pub(crate) walked_file_children: SeenNames,
    pub(crate) walked_dir_children: SeenNames,

    // File names seen so far (only used before transitioning to walk).
    pub(crate) seen_files: SeenNames,
    // Dir names seen so far (only used before transitioning to walk).
    pub(crate) seen_dirs: SeenNames,

    // Count of children directories.
    total_dirs: Option<usize>,

    // Count of children files.
    total_files: Option<usize>,

    // Total dir count under us, by depth (index 0 is total of grandchildren).
    // This only tracks "important" (i.e. large) values.
    total_dirs_at_depth: Vec<Option<usize>>,

    // Total file count seen under us, by depth (index 0 is total of grandchildren).
    // This only tracks "important" (i.e. large) values.
    total_files_at_depth: Vec<Option<usize>>,

    // Whether a descendant (not include ourself) might have a walk.
    // If false, we can be certain no descendant has a walk.
    // This is used to optimize listing all the walks.
    pub(crate) descendant_might_have_walk: bool,
}

impl WalkNode {
    pub(crate) fn new(gc_timeout: Duration) -> Self {
        let node = Self {
            gc_timeout,
            file_walk: Default::default(),
            dir_walk: Default::default(),
            last_access: Default::default(),
            children: Default::default(),
            advanced_file_children: Default::default(),
            advanced_dir_children: Default::default(),
            walked_file_children: Default::default(),
            walked_dir_children: Default::default(),
            seen_files: Default::default(),
            seen_dirs: Default::default(),
            total_dirs: None,
            total_files: None,
            total_dirs_at_depth: Default::default(),
            total_files_at_depth: Default::default(),
            descendant_might_have_walk: false,
        };
        node.last_access.bump();
        node
    }

    /// Get existing WalkNode entry for specified dir, if any.
    pub(crate) fn get_node(&self, dir: &RepoPath) -> Option<&Self> {
        match dir.split_first_component() {
            Some((head, tail)) => self
                .children
                .get(head)
                .and_then(|child| child.get_node(tail)),
            None => Some(self),
        }
    }

    pub(crate) fn get_node_mut(&mut self, dir: &RepoPath) -> Option<&mut Self> {
        match dir.split_first_component() {
            Some((head, tail)) => self
                .children
                .get_mut(head)
                .and_then(|child| child.get_node_mut(tail)),
            None => Some(self),
        }
    }

    /// Find node with active walk covering directory `dir`, if any.
    pub(crate) fn get_owning_node<'a, 'b>(
        &'a self,
        walk_type: WalkType,
        dir: &'b RepoPath,
    ) -> Option<(&'a Self, &'b RepoPath)> {
        match dir.split_first_component() {
            Some((head, tail)) => {
                if self.contains(walk_type, dir, 0) {
                    Some((self, dir))
                } else {
                    self.children
                        .get(head)
                        .and_then(|child| child.get_owning_node(walk_type, tail))
                }
            }
            None => {
                if self.get_dominating_walk(walk_type).is_some() {
                    Some((self, dir))
                } else {
                    None
                }
            }
        }
    }

    /// Find node with active walk covering directory `dir`, if any.
    pub(crate) fn get_owning_node_mut<'a, 'b>(
        &'a mut self,
        walk_type: WalkType,
        dir: &'b RepoPath,
    ) -> Option<(&'a mut Self, &'b RepoPath)> {
        match dir.split_first_component() {
            Some((head, tail)) => {
                if self.contains(walk_type, dir, 0) {
                    Some((self, dir))
                } else {
                    self.children
                        .get_mut(head)
                        .and_then(|child| child.get_owning_node_mut(walk_type, tail))
                }
            }
            None => {
                if self.get_dominating_walk(walk_type).is_some() {
                    Some((self, dir))
                } else {
                    None
                }
            }
        }
    }

    /// Find node with active walk covering `dir`, or create new node for `dir`. This is a
    /// single step to perform the common get-or-create operation in a single tree
    /// traversal.
    pub(crate) fn get_or_create_owning_node<'a>(
        &'a mut self,
        config: &Config,
        walk_type: WalkType,
        dir: &'a RepoPath,
    ) -> (&'a mut Self, &'a RepoPath) {
        fn inner<'a>(
            node: &'a mut WalkNode,
            config: &Config,
            walk_type: WalkType,
            full_dir: &'a RepoPath,
            relative_dir: &'a RepoPath,
        ) -> (&'a mut WalkNode, &'a RepoPath) {
            match relative_dir.split_first_component() {
                Some((head, tail)) => {
                    if node.contains(walk_type, relative_dir, 0) {
                        (node, relative_dir)
                    } else if node.children.contains_key(head) {
                        inner(
                            node.children.get_mut(head).unwrap(),
                            config,
                            walk_type,
                            full_dir,
                            tail,
                        )
                    } else {
                        inner(
                            node.children
                                .entry(head.to_owned())
                                .or_insert_with(|| WalkNode::new(config.gc_timeout)),
                            config,
                            walk_type,
                            full_dir,
                            tail,
                        )
                    }
                }
                None => {
                    // Perform a JIT "light" GC.
                    if node.expired() {
                        node.clear_except_children(full_dir);
                    }
                    (node, relative_dir)
                }
            }
        }

        inner(self, config, walk_type, dir, dir)
    }

    /// Insert a new walk. Any redundant/contained walks will be removed. `walk` will not
    /// be inserted if it is contained by an ancestor walk.
    pub(crate) fn insert_walk(
        &mut self,
        config: &Config,
        walk_type: WalkType,
        walk_root: &RepoPath,
        mut walk: Walk,
    ) -> &mut Self {
        if self.get_walk_for_type(walk_type).is_some() {
            // Refresh our last_access_time as we see a descendant walk getting inserted. In general
            // a descendant walk is "making progress" towards advancing ancestors walks, and the
            // progress can be very slow (and we don't want the ancestor walk to get GC'd in the
            // mean time).
            self.last_access.bump();
        }

        // If we completely overlap with the walk to be inserted, skip it. This shouldn't
        // happen, but we want to guarantee there are no completely overlapping walks.
        if self.contains(walk_type, walk_root, walk.depth) {
            if let Some(existing) = self.get_walk_for_type(walk_type) {
                existing.absorb_counters(&walk);
            }
            return self;
        }

        match walk_root.split_first_component() {
            Some((head, tail)) => {
                self.descendant_might_have_walk = true;

                if self.children.contains_key(head) {
                    self.children
                        .get_mut(head)
                        .unwrap()
                        .insert_walk(config, walk_type, tail, walk)
                } else {
                    self.children
                        .entry(head.to_owned())
                        .or_insert_with(|| Self::new(config.gc_timeout))
                        .insert_walk(config, walk_type, tail, walk)
                }
            }
            None => {
                let threshold = config.walk_threshold;
                let walk_ratio = config.walk_ratio;

                self.clear_advanced_children(walk_type);
                self.walked_children_mut(walk_type).clear();

                // This can have a side effect of adding to self.advanced_children.
                self.remove_contained(walk_type, &walk, threshold, walk_ratio);

                let seen_count = self.advanced_children_len(walk_type);
                if self.is_walked(
                    WalkType::Directory,
                    seen_count,
                    walk.depth + 1,
                    threshold,
                    walk_ratio,
                    walk.credited_breadth(),
                ) {
                    walk.credit_breadth(
                        self.total_count(WalkType::Directory, walk.depth + 1)
                            .unwrap_or_default(),
                    );
                    walk.depth += 1;
                    tracing::debug!(
                        new_depth = walk.depth,
                        "advancing walk after removing descendants"
                    );
                    self.insert_walk(config, walk_type, walk_root, walk)
                } else {
                    self.set_walk_for_type(walk_type, Some(walk));
                    self
                }
            }
        }
    }

    /// List all active walks. Filter by walk_type, if specified.
    pub(crate) fn list_walks(
        &self,
        walk_type: Option<WalkType>,
    ) -> Vec<(RepoPathBuf, usize, WalkType)> {
        fn inner(
            node: &WalkNode,
            walk_type: Option<WalkType>,
            path: &mut RepoPathBuf,
            list: &mut Vec<(RepoPathBuf, usize, WalkType)>,
        ) {
            if walk_type.is_none_or(|wt| wt == WalkType::File) {
                if let Some(walk) = node.get_walk_for_type(WalkType::File) {
                    list.push((path.clone(), walk.depth, WalkType::File));
                }
            }
            if walk_type.is_none_or(|wt| wt == WalkType::Directory) {
                if let Some(walk) = node.get_walk_for_type(WalkType::Directory) {
                    list.push((path.clone(), walk.depth, WalkType::Directory));
                }
            }

            if node.descendant_might_have_walk {
                for (name, child) in node.children.iter() {
                    path.push(name.as_path_component());
                    inner(child, walk_type, path, list);
                    path.pop();
                }
            }
        }

        let mut list = Vec::new();
        inner(self, walk_type, &mut RepoPathBuf::new(), &mut list);
        list
    }

    /// Get most "powerful" walk that covers `walk_type`. Basically, a file walk covers a
    /// directory walk, so if walk_type=Directory, we return `self.file_walk ||
    /// self.dir_walk`.
    pub(crate) fn get_dominating_walk(&self, walk_type: WalkType) -> Option<&Walk> {
        let walk = match walk_type {
            WalkType::File => self.file_walk.as_ref(),
            WalkType::Directory => self.file_walk.as_ref().or(self.dir_walk.as_ref()),
        };

        walk.filter(|&_walk| !self.expired())
    }

    pub(crate) fn get_walk_for_type(&self, walk_type: WalkType) -> Option<&Walk> {
        let walk = match walk_type {
            WalkType::File => self.file_walk.as_ref(),
            WalkType::Directory => self.dir_walk.as_ref(),
        };

        walk.filter(|&_walk| !self.expired())
    }

    /// Set walk of `walk_type` to new_walk. Returns old walk, if any.
    fn set_walk_for_type(&mut self, walk_type: WalkType, new_walk: Option<Walk>) -> Option<Walk> {
        // File walk implies directory walk, so clear out contained directory walk.
        match (walk_type, &new_walk, &self.dir_walk) {
            (
                WalkType::File,
                Some(new_walk @ Walk { depth, .. }),
                Some(
                    dir_walk @ Walk {
                        depth: dir_depth, ..
                    },
                ),
            ) if depth >= dir_depth => {
                new_walk.absorb_counters(dir_walk);
                new_walk.absorb_credit(dir_walk);
                self.dir_walk = None;
            }
            _ => {}
        }

        let old_walk = match walk_type {
            WalkType::File => &mut self.file_walk,
            WalkType::Directory => &mut self.dir_walk,
        };

        if let (Some(old), Some(new)) = (&old_walk, &new_walk) {
            new.absorb_counters(old);
            new.absorb_credit(old);
        }

        std::mem::replace(old_walk, new_walk)
    }

    /// Mark name as an advanced child of self (i.e. a descendant under name has a walk that has advanced one level deeper than our walk).
    /// Returns (total advanced children, total advanced count, name advanced count).
    pub(crate) fn insert_advanced_child(
        &mut self,
        walk_type: WalkType,
        name: PathComponentBuf,
    ) -> (usize, usize, usize) {
        let map = match walk_type {
            WalkType::File => &mut self.advanced_file_children,
            WalkType::Directory => &mut self.advanced_dir_children,
        };

        let counter = {
            let counter = map.entry(name).or_default();
            *counter += 1;
            *counter
        };

        (map.len(), map.values().sum(), counter)
    }

    fn advanced_children_len(&self, walk_type: WalkType) -> usize {
        match walk_type {
            WalkType::File => self.advanced_file_children.len(),
            WalkType::Directory => self.advanced_dir_children.len(),
        }
    }

    fn clear_advanced_children(&mut self, walk_type: WalkType) {
        match walk_type {
            WalkType::File => self.advanced_file_children.clear(),
            WalkType::Directory => self.advanced_dir_children.clear(),
        }
    }

    fn walked_children_mut(&mut self, walk_type: WalkType) -> &mut SeenNames {
        match walk_type {
            WalkType::File => &mut self.walked_file_children,
            WalkType::Directory => &mut self.walked_dir_children,
        }
    }

    /// Record that direct child `name` has a walk of `walk_type`, and count this as activity on
    /// self. Once self has gone `gc_timeout` without activity, the record starts over from the
    /// children whose walks are still live. Returns the number of recorded children and whether
    /// they were recorded in sorted order.
    pub(crate) fn insert_walked_child(
        &mut self,
        walk_type: WalkType,
        name: PathComponentBuf,
    ) -> (usize, bool) {
        let expired = self.expired();
        let children = &self.children;
        let walked = match walk_type {
            WalkType::File => &mut self.walked_file_children,
            WalkType::Directory => &mut self.walked_dir_children,
        };

        if expired {
            walked.reset_to(
                children
                    .iter()
                    .filter(|(_, child)| child.get_walk_for_type(walk_type).is_some())
                    .map(|(name, _)| name.clone()),
            );
        }

        walked.insert(name);
        let recorded = (walked.len(), walked.is_sorted());

        self.last_access.bump();

        recorded
    }

    /// Recursively remove all walks contained within a walk of depth `depth`.
    fn remove_contained(
        &mut self,
        walk_type: WalkType,
        new_walk: &Walk,
        threshold: usize,
        ratio: f64,
    ) {
        // Returns whether a walk exists at depth+1.
        fn inner(
            new_walk: &Walk,
            path: &mut RepoPathBuf,
            node: &mut WalkNode,
            walk_type: WalkType,
            depth: usize,
            top: bool,
            threshold: usize,
            ratio: f64,
        ) -> bool {
            let mut any_child_advanced = false;
            let mut new_advanced_children = Vec::new();
            let mut descendant_might_have_walk = false;
            node.children.retain(|name, child| {
                let mut child_advanced = false;

                path.push(name);

                if child
                    .get_walk_for_type(walk_type)
                    .is_some_and(|w| w.depth >= depth)
                {
                    child_advanced = true;
                } else if let Some(old_walk) = child.set_walk_for_type(walk_type, None) {
                    new_walk.absorb_counters(&old_walk);
                }

                if depth > 0 {
                    if inner(
                        new_walk,
                        path,
                        child,
                        walk_type,
                        depth - 1,
                        false,
                        threshold,
                        ratio,
                    ) {
                        child_advanced = true;
                    }
                }

                if top && child_advanced {
                    // Record if this top-level child has advanced children, meaning a
                    // descendant walk that has pushed to depth+1.
                    new_advanced_children.push(name.to_owned());
                }

                any_child_advanced = any_child_advanced || child_advanced;

                let child_has_walk = child.has_walk() && !child.expired();

                let retain = child_has_walk
                    || !child.children.is_empty()
                    // Keep node around if it has total file/dir hints that are likely to be useful.
                    || interesting_metadata(threshold, ratio, child.total_files(), child.total_dirs());

                if !retain {
                    tracing::trace!(%path, "dropping node during insert");
                }

                path.pop();

                if child_has_walk || child.descendant_might_have_walk {
                    descendant_might_have_walk = true;
                }

                retain
            });

            for advanced in new_advanced_children {
                tracing::trace!(dir=%path, child=%advanced, "inserting advanced child during removal");
                node.insert_advanced_child(walk_type, advanced);
            }

            node.descendant_might_have_walk = descendant_might_have_walk;

            any_child_advanced
        }

        inner(
            new_walk,
            &mut RepoPathBuf::new(),
            self,
            walk_type,
            new_walk.depth,
            true,
            threshold,
            ratio,
        );
    }

    pub(crate) fn total_dirs(&self) -> Option<usize> {
        self.total_dirs_at_depth(0)
    }

    pub(crate) fn total_dirs_at_depth(&self, depth: usize) -> Option<usize> {
        if depth == 0 {
            self.total_dirs
        } else {
            self.total_dirs_at_depth
                .get(depth - 1)
                .cloned()
                .unwrap_or_default()
        }
    }

    pub(crate) fn total_files(&self) -> Option<usize> {
        self.total_files_at_depth(0)
    }

    pub(crate) fn total_files_at_depth(&self, depth: usize) -> Option<usize> {
        if depth == 0 {
            self.total_files
        } else {
            self.total_files_at_depth
                .get(depth - 1)
                .cloned()
                .unwrap_or_default()
        }
    }

    /// Set directory metadata for dir, updating metadata for ancestors of dir as we go.
    pub(crate) fn set_metadata(
        &mut self,
        config: &Config,
        dir: &RepoPath,
        num_files: usize,
        num_dirs: usize,
    ) -> (isize, isize) {
        match dir.split_first_component() {
            Some((head, tail)) => {
                let (file_delta, dir_delta) = if self.children.contains_key(head) {
                    self.children
                        .get_mut(head)
                        .unwrap()
                        .set_metadata(config, tail, num_files, num_dirs)
                } else {
                    self.children
                        .entry(head.to_owned())
                        .or_insert_with(|| Self::new(config.gc_timeout))
                        .set_metadata(config, tail, num_files, num_dirs)
                };

                if !important_metadata(
                    config.walk_threshold,
                    config.walk_ratio,
                    Some(num_files),
                    Some(num_dirs),
                ) {
                    return (0, 0);
                }

                let depth = tail.depth();

                for (delta, totals) in [
                    (file_delta, &mut self.total_files_at_depth),
                    (dir_delta, &mut self.total_dirs_at_depth),
                ] {
                    if totals.len() < depth + 1 {
                        totals.resize(depth + 1, None);
                    }
                    totals[depth] = Some(
                        totals[depth]
                            .unwrap_or_default()
                            .saturating_add_signed(delta),
                    );
                }

                (file_delta, dir_delta)
            }
            None => {
                self.last_access.bump();

                // If we already have total_files and/or total_dirs set, we don't want to add the
                // full value to ancestors' metadata again, so compute the delta relative to current
                // value.
                (
                    num_files as isize
                        - self.total_files.replace(num_files).unwrap_or_default() as isize,
                    num_dirs as isize
                        - self.total_dirs.replace(num_dirs).unwrap_or_default() as isize,
                )
            }
        }
    }

    /// Reports whether self has a walk and the walk fully contains a descendant walk
    /// rooted at `path` of depth `depth`.
    fn contains(&self, walk_type: WalkType, path: &RepoPath, depth: usize) -> bool {
        self.get_dominating_walk(walk_type)
            .is_some_and(|w| w.depth >= (path.depth() + depth))
    }

    /// Return whether this Dir should be considered "walked". `credit` is the largest directory
    /// count the relevant walk has already satisfied `walk_ratio` against; the ratio only raises
    /// the threshold for counts beyond it.
    pub(crate) fn is_walked(
        &self,
        walk_type: WalkType,
        seen_count: usize,
        depth: usize,
        mut walk_threshold: usize,
        walk_ratio: f64,
        credit: usize,
    ) -> bool {
        if seen_count == 0 {
            return false;
        }

        // If we have the total size hint, adjust the threshold for extreme cases.
        if let Some(total) = self.total_count(walk_type, depth) {
            // If dir is too small we know we will never reach the threshold. Adjust threshold down
            // until it is smaller than dir size.
            while walk_threshold > total {
                walk_threshold /= 2;
            }

            // Conversely, if directory is very large we don't want to detect a walk too
            // aggressively. Ensure the threshold is at least `walk_ratio` of the total directory
            // size. For example, a if `walk_ratio` is 0.1 and the directory size is 10_000, we will
            // raise the `walk_threshold` to 1_000. A walk that already crossed a level at least
            // this wide has proven itself, so the ratio does not apply to it again.
            if total > credit && (walk_threshold as f64) / (total as f64) < walk_ratio {
                walk_threshold = ((total as f64) * walk_ratio) as usize;
            }
        }

        seen_count >= walk_threshold
    }

    pub(crate) fn total_count(&self, walk_type: WalkType, depth: usize) -> Option<usize> {
        match walk_type {
            WalkType::File => self.total_files_at_depth(depth),
            WalkType::Directory => self.total_dirs_at_depth(depth),
        }
    }

    pub(crate) fn iter(&self, mut cb: impl FnMut(&WalkNode, usize) -> bool) {
        fn inner(node: &WalkNode, cb: &mut impl FnMut(&WalkNode, usize) -> bool, depth: usize) {
            if !cb(node, depth) {
                return;
            }

            for child in node.children.values() {
                inner(child, cb, depth + 1);
            }
        }

        inner(self, &mut cb, 0);
    }

    /// Delete nodes not accessed within timeout.
    pub(crate) fn gc(&mut self, config: &Config) -> GcResult {
        // Return (nodes_deleted, nodes_remaining, walks_deleted, walks_remaining, keep_me)
        fn inner(
            config: &Config,
            path: &mut RepoPathBuf,
            node: &mut WalkNode,
        ) -> (usize, usize, usize, usize, bool) {
            let mut walks_removed = 0;
            let mut deleted = 0;
            let mut retained = 0;
            let mut walks_remaining = 0;

            node.children.retain(|name, child| {
                path.push(name);

                let (d, r, w, wr, keep) = inner(config, path, child);

                deleted += d;
                retained += r;
                walks_removed += w;
                walks_remaining += wr;

                if !keep {
                    tracing::trace!(%path, has_walk=child.has_walk(), "GC deleting node");
                    // Finalize any walks before dropping the node.
                    child.clear_except_children(path);
                }

                path.pop();

                keep
            });

            node.descendant_might_have_walk = walks_remaining > 0;

            let expired = node.expired();

            let important_metadata = important_metadata(
                config.walk_threshold,
                config.walk_ratio,
                node.total_files(),
                node.total_dirs(),
            );
            let keep_me = !expired || !node.children.is_empty() || important_metadata;
            let has_walk = node.has_walk();

            if has_walk {
                if expired {
                    walks_removed += 1;
                } else {
                    walks_remaining += 1;
                }
            }

            if expired && keep_me {
                tracing::trace!(%path, has_walk, important_metadata, has_children=!node.children.is_empty(), "GC clearing node");
                node.clear_except_children(path);
            }

            if keep_me {
                retained += 1;
            } else {
                deleted += 1;
            }

            (deleted, retained, walks_removed, walks_remaining, keep_me)
        }

        let (mut deleted, remaining, mut walks_deleted, walks_remaining, keep_me) =
            inner(config, &mut RepoPathBuf::new(), self);

        self.descendant_might_have_walk = walks_remaining > 0;

        if !keep_me {
            // We don't actually delete the root node, so take one off.
            deleted -= 1;

            // Log root GC event only if the root node had some activity (and hence had last_access
            // set).
            if self.last_access.load().is_some() {
                tracing::trace!("GCing root node");
            }

            if self.has_walk() {
                walks_deleted += 1;
            }

            // At top level we have no parent to remove us, so just unset our fields.
            self.clear_except_children(RepoPath::empty());
        }

        GcResult {
            deleted_nodes: deleted,
            remaining_nodes: remaining,
            deleted_walks: walks_deleted,
        }
    }

    // Clear all fields except children.
    fn clear_except_children(&mut self, path: &RepoPath) {
        let end_time = self.last_access.load().unwrap_or_else(Instant::now);

        if let Some(walk) = self.file_walk.take() {
            walk.log_end(path, end_time);
        }
        if let Some(walk) = self.dir_walk.take() {
            walk.log_end(path, end_time);
        }
        self.last_access.reset();
        self.advanced_file_children.clear();
        self.advanced_dir_children.clear();
        self.walked_file_children.clear();
        self.walked_dir_children.clear();
        self.seen_files.clear();

        // No harm in retaining self.total_dirs and self.total_files, or the important tallies in
        // total_dirs_at_depth and total_files_at_depth.
    }

    // NB: does not check if self.expired(), so caller must check if appropriate.
    fn has_walk(&self) -> bool {
        self.file_walk.is_some() || self.dir_walk.is_some()
    }

    #[allow(clippy::useless_conversion)]
    pub(crate) fn expired(&self) -> bool {
        self.last_access
            .load()
            .is_none_or(|accessed| accessed.elapsed() >= self.gc_timeout.into())
    }
}
