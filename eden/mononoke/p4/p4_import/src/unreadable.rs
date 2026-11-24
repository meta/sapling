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
use std::time::Duration;

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

/// Imports any revision the server says it has no archive of, without content, once its
/// print has been tried again and failed the same way every time. Only that failure: any
/// other, on the first print or on a retry, fails the changelist. A storage outage on the
/// server can look the same as a lost archive, so the retries are what tells a short one
/// apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SkipUnreadable {
    /// Prints tried after the first before the revision is skipped.
    pub retries: u32,
    /// The wait before the first retry; each later wait is twice the one before.
    pub first_retry_delay: Duration,
}

/// Why a revision was imported without content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// It is one of `ImportOptions::unreadable_revisions`.
    Approved,
    /// Every retry of `ImportOptions::skip_unreadable` failed.
    RetriesFailed,
}

impl SkipReason {
    /// As Scuba has it.
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::Approved => "approved",
            SkipReason::RetriesFailed => "retries_failed",
        }
    }
}

/// A revision imported without content, as a deletion of its path: the server could not read
/// its archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedRevision {
    pub depot_path: String,
    pub revision: u32,
    pub reason: SkipReason,
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
    archive_unreadable(error, revision).is_some()
}

/// Whether `error`, from printing and storing `revision`, says the server has no archive of
/// that revision: its file, or the revision in it, is not there.
pub(crate) fn is_archive_missing(error: &anyhow::Error, revision: &FileRevision) -> bool {
    archive_unreadable(error, revision).is_some_and(|unreadable| unreadable.missing)
}

fn archive_unreadable<'a>(
    error: &'a anyhow::Error,
    revision: &FileRevision,
) -> Option<&'a ArchiveUnreadable> {
    error
        .downcast_ref::<ArchiveUnreadable>()
        .or_else(|| {
            error
                .chain()
                .find_map(|cause| cause.downcast_ref::<ArchiveUnreadable>())
        })
        .filter(|unreadable| {
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
