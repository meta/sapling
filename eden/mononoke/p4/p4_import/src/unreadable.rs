/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Revisions whose archive the server cannot read. One is imported without content only if
//! the caller has approved that exact revision; any other fails its changelist.

use std::collections::HashSet;
use std::fmt;

use anyhow::Result;
use p4_types::ArchiveUnreadable;
use p4_types::FileRevision;
use thiserror::Error;

/// A revision the caller approves importing without content if the server cannot read its
/// archive: changelist `change`'s revision `revision` of `depot_path`, exactly as
/// `FileRevision::depot_path` has it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UnreadableRevision {
    pub change: u64,
    pub depot_path: String,
    pub revision: u32,
}

impl fmt::Display for UnreadableRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.depot_path, self.revision)
    }
}

/// A changelist has revisions whose archive the server cannot read and that are not approved.
#[derive(Debug, Error)]
#[error(
    "the server cannot read its archive of {} unapproved revision(s) of changelist {change}{}: {}",
    revisions.len(),
    if *incomplete { ", the first ones found; there may be more" } else { "" },
    revisions.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
)]
pub struct UnreadableRevisions {
    pub change: u64,
    /// Sorted by depot path and revision.
    pub revisions: Vec<UnreadableRevision>,
    /// The import stopped looking after `MAX_REPORTED`.
    pub incomplete: bool,
}

/// Unapproved unreadable revisions of a changelist reported at most: past this many, the
/// changelist fails without printing the rest.
pub(crate) const MAX_REPORTED: usize = 100;

/// Whether `error`, from printing and storing `revision`, says the server cannot read its
/// archive of that revision. One that names another revision does not.
pub(crate) fn is_archive_unreadable(error: &anyhow::Error, revision: &FileRevision) -> bool {
    error
        .downcast_ref::<ArchiveUnreadable>()
        .or_else(|| {
            error
                .chain()
                .find_map(|cause| cause.downcast_ref::<ArchiveUnreadable>())
        })
        .is_some_and(|unreadable| {
            unreadable.depot_path == revision.depot_path && unreadable.revision == revision.revision
        })
}

/// Whether `approved` holds `revision` of changelist `change`.
pub(crate) fn is_approved(
    approved: &HashSet<UnreadableRevision>,
    change: u64,
    revision: &FileRevision,
) -> bool {
    !approved.is_empty()
        && approved.contains(&UnreadableRevision {
            change,
            depot_path: revision.depot_path.clone(),
            revision: revision.revision,
        })
}

/// Collects a changelist's unapproved unreadable revisions.
pub(crate) struct Unapproved {
    change: u64,
    revisions: Vec<UnreadableRevision>,
}

impl Unapproved {
    pub(crate) fn new(change: u64) -> Self {
        Self {
            change,
            revisions: Vec::new(),
        }
    }

    /// Fails once `MAX_REPORTED` have been collected.
    pub(crate) fn add(&mut self, revision: &FileRevision) -> Result<()> {
        self.revisions.push(UnreadableRevision {
            change: self.change,
            depot_path: revision.depot_path.clone(),
            revision: revision.revision,
        });
        if self.revisions.len() >= MAX_REPORTED {
            return Err(self.error(true).into());
        }
        Ok(())
    }

    /// Fails if any have been collected.
    pub(crate) fn finish(mut self) -> Result<()> {
        if self.revisions.is_empty() {
            return Ok(());
        }
        Err(self.error(false).into())
    }

    fn error(&mut self, incomplete: bool) -> UnreadableRevisions {
        let mut revisions = std::mem::take(&mut self.revisions);
        revisions.sort();
        UnreadableRevisions {
            change: self.change,
            revisions,
            incomplete,
        }
    }
}
