/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;

use mononoke_types::BonsaiChangeset;
use mononoke_types::ContentId;
use mononoke_types::FileType;
use mononoke_types::NonRootMPath;

/// What the changesets added so far did at one path.
struct Changed {
    /// The newest changeset to change or delete the path, counted from the first added.
    newest: usize,
    /// The file that changeset left at the path: `None` if it deleted it.
    file: Option<(ContentId, FileType)>,
    /// The newest changeset to store a file at the path.
    stored: Option<usize>,
}

/// A batch's changesets built so far, by path: answers what `file_after` answers for them,
/// without reading through every file change of every changeset for each path asked.
#[derive(Default)]
pub(crate) struct BuiltFiles {
    changesets: usize,
    changed: HashMap<NonRootMPath, Changed>,
    /// By directory, the newest changeset to store a file below it.
    stored_below: HashMap<NonRootMPath, usize>,
}

impl BuiltFiles {
    /// Adds the changeset built on top of those added before.
    pub(crate) fn add(&mut self, changeset: &BonsaiChangeset) {
        let newest = self.changesets;
        self.changesets += 1;
        for (path, change) in changeset.file_changes() {
            let file = change.simplify().map(|c| (c.content_id(), c.file_type()));
            let stored = change.is_changed().then_some(newest);
            match self.changed.get_mut(path) {
                Some(changed) => {
                    changed.newest = newest;
                    changed.file = file;
                    changed.stored = stored.or(changed.stored);
                }
                None => {
                    let changed = Changed {
                        newest,
                        file,
                        stored,
                    };
                    self.changed.insert(path.clone(), changed);
                }
            }
            if stored.is_none() {
                continue;
            }
            for directory in path.clone().into_non_root_ancestors().skip(1) {
                // Its own ancestors were given this changeset when it was.
                if self.stored_below.insert(directory, newest) == Some(newest) {
                    break;
                }
            }
        }
    }

    /// The content and type of the file at `path` after the changesets added (`Some(None)`
    /// if it isn't a file), or `None` if none of them touches it: `file_after` of them.
    pub(crate) fn file_after(&self, path: &NonRootMPath) -> Option<Option<(ContentId, FileType)>> {
        // A file stored above the path replaces its directory; one below makes it one.
        let above = path
            .clone()
            .into_non_root_ancestors()
            .skip(1)
            .filter_map(|ancestor| self.changed.get(&ancestor)?.stored)
            .max();
        let below = self.stored_below.get(path).copied();
        let around = above.max(below);
        match (self.changed.get(path), around) {
            (None, None) => None,
            (None, Some(_)) => Some(None),
            // As in `file_after`: in one changeset, the change to the path itself answers.
            (Some(changed), around) if Some(changed.newest) >= around => Some(changed.file),
            (Some(_), _) => Some(None),
        }
    }
}
