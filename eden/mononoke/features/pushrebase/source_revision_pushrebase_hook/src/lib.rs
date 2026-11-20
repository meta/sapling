/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! A pushrebase hook that rejects landings whose generated JSON files do not
//! advance their shared root `source_revision` by exactly one over the
//! landing commit's actual parent.
//!
//! The guarded files are the immediate `*.json` children of a few configured
//! directories: an authoritative one and its mirrors. A commit may not change
//! anything else under them, or put a file at or above them, except to delete
//! it.
//!
//! The check runs in `into_transaction_hook`: after pushrebase has rebased,
//! merge-resolved and saved every commit, and before it compares-and-swaps
//! the bookmark against the value the commits were rebased onto. Two
//! landings that claim the same next revision cannot both pass: the second
//! is rebased onto the first, where its revision is no longer the next one.
//! A swap that fails because the bookmark moved is retried with fresh hooks,
//! so the check is repeated against the new parent.
//!
//! Files without a `source_revision` anywhere are the unversioned baseline,
//! revision zero: they may change freely until one landing versions every
//! file at revision one. A versioned snapshot never goes back.

mod snapshot;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::iter;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering as AtomicOrdering;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::ensure;
use async_trait::async_trait;
use blobstore::Loadable;
use bookmarks::BookmarkTransactionError;
use bytes::Bytes;
use content_manifest_derivation::RootContentManifestId;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use manifest::Entry;
use manifest::Manifest;
use manifest::ManifestOps;
use metaconfig_types::SourceRevisionGuardConfig;
use mononoke_types::BonsaiChangesetMut;
use mononoke_types::ChangesetId;
use mononoke_types::ContentId;
use mononoke_types::FileChange;
use mononoke_types::FileType;
use mononoke_types::MPath;
use mononoke_types::NonRootMPath;
use pushrebase_hook::PreparationRequired;
use pushrebase_hook::PushrebaseCommitHook;
use pushrebase_hook::PushrebaseHook;
use pushrebase_hook::PushrebaseTransactionHook;
use pushrebase_hook::RebasedChangesets;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreArc;
use repo_derived_data::RepoDerivedData;
use repo_derived_data::RepoDerivedDataArc;
pub use snapshot::ParsedFile;
pub use snapshot::ParsedSnapshot;
pub use snapshot::SOURCE_REVISION_FIELD;
pub use snapshot::SnapshotError;
pub use snapshot::SnapshotRevision;
pub use snapshot::SourceRevision;
pub use snapshot::TransitionError;
pub use snapshot::check_transition;
pub use snapshot::parse_file;
pub use snapshot::parse_snapshot;
use sql_ext::Transaction;
use stats::prelude::*;
use thiserror::Error;

define_stats! {
    prefix = "mononoke.pushrebase.source_revision_guard";
    checked: timeseries(Rate, Sum),
    prepared: timeseries(Rate, Sum),
    rejected: timeseries(Rate, Sum),
    rejected_layout: timeseries(Rate, Sum),
    preparation_required: timeseries(Rate, Sum),
    read_failed: timeseries(Rate, Sum),
}

/// What one hook has recorded, as `GuardCounters::counts` reads it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GuardCounts {
    /// Attempts in which the guard examined guarded changes: one that wrote
    /// to the guarded directories in a way it rejects while rebasing, or one
    /// whose rebased commits it checked before the bookmark swap.
    pub checked: u64,
    /// Attempts the guard turned down, in either phase.
    pub rejected: u64,
    /// The part of `rejected` turned down while rebasing, for the layout of
    /// the change: a subtree operation, a path that is not a guarded file,
    /// too many changes.
    pub rejected_layout: u64,
    /// Attempts that stopped for preparation; not rejections.
    pub preparation_required: u64,
    /// Attempts that stopped because a read failed; not rejections.
    pub read_failed: u64,
    /// Calls to `prepare`.
    pub prepared: u64,
}

/// Counts every outcome once, where it is decided, both into the process's
/// stats and into these counters, which only the owner of the hook reads.
#[derive(Debug, Default)]
pub struct GuardCounters {
    checked: AtomicU64,
    rejected: AtomicU64,
    rejected_layout: AtomicU64,
    preparation_required: AtomicU64,
    read_failed: AtomicU64,
    prepared: AtomicU64,
}

impl GuardCounters {
    pub fn counts(&self) -> GuardCounts {
        let read = |counter: &AtomicU64| counter.load(AtomicOrdering::Relaxed);
        GuardCounts {
            checked: read(&self.checked),
            rejected: read(&self.rejected),
            rejected_layout: read(&self.rejected_layout),
            preparation_required: read(&self.preparation_required),
            read_failed: read(&self.read_failed),
            prepared: read(&self.prepared),
        }
    }

    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, AtomicOrdering::Relaxed);
    }

    fn checked(&self) {
        STATS::checked.add_value(1);
        Self::bump(&self.checked);
    }

    fn prepared(&self) {
        STATS::prepared.add_value(1);
        Self::bump(&self.prepared);
    }

    /// A change rejected while rebasing. The attempt stops there, so its
    /// transaction phase never runs.
    fn rejected_layout(&self) {
        self.checked();
        STATS::rejected.add_value(1);
        STATS::rejected_layout.add_value(1);
        Self::bump(&self.rejected);
        Self::bump(&self.rejected_layout);
    }

    /// The outcome of the transaction phase.
    fn outcome(&self, outcome: GuardOutcome) {
        match outcome {
            GuardOutcome::Passed => {}
            GuardOutcome::Rejected => {
                STATS::rejected.add_value(1);
                Self::bump(&self.rejected);
            }
            GuardOutcome::PreparationRequired => {
                STATS::preparation_required.add_value(1);
                Self::bump(&self.preparation_required);
            }
            GuardOutcome::ReadFailed => {
                STATS::read_failed.add_value(1);
                Self::bump(&self.read_failed);
            }
        }
    }
}

/// Why a landing was rejected.
#[derive(Debug, Error)]
pub enum SourceRevisionGuardError {
    #[error(
        "{changeset} changes generated source directories in a way pushrebase cannot check: {error}"
    )]
    InvalidChange {
        changeset: ChangesetId,
        error: SnapshotError,
    },
    #[error("generated source files in {candidate} are invalid: {error}")]
    InvalidCandidate {
        candidate: ChangesetId,
        error: SnapshotError,
    },
    #[error(
        "generated source files in {parent}, the parent {candidate} lands on, are invalid: {error}"
    )]
    InvalidParent {
        candidate: ChangesetId,
        parent: ChangesetId,
        error: SnapshotError,
    },
    #[error("{candidate} cannot land on {parent}: {error}")]
    InvalidTransition {
        candidate: ChangesetId,
        parent: ChangesetId,
        error: TransitionError,
    },
    #[error(
        "{candidate} is a merge that changes generated source files; land the generated change as an ordinary commit"
    )]
    MergeChangesGeneratedFiles { candidate: ChangesetId },
}

impl SourceRevisionGuardError {
    /// The commit that was rejected: as pushed for `InvalidChange`, which
    /// is raised while rebasing, and as rebased otherwise.
    pub fn candidate(&self) -> ChangesetId {
        match self {
            Self::InvalidChange {
                changeset: candidate,
                ..
            }
            | Self::InvalidCandidate { candidate, .. }
            | Self::InvalidParent { candidate, .. }
            | Self::InvalidTransition { candidate, .. }
            | Self::MergeChangesGeneratedFiles { candidate } => *candidate,
        }
    }
}

/// A file at a guarded path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileRef {
    content_id: ContentId,
    file_type: FileType,
    size: u64,
}

/// The guarded files of a commit. A guarded directory holds nothing else:
/// commits being landed cannot add other entries, and a parent with any is
/// rejected.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Snapshot {
    files: BTreeMap<NonRootMPath, FileRef>,
}

/// What a rebased commit says about the guarded files, as pushrebase built
/// it.
struct RebasedCommit {
    parents: Vec<ChangesetId>,
    /// The guarded files the commit changes: `None` deletes the file.
    changes: Vec<(NonRootMPath, Option<FileRef>)>,
}

struct Guard {
    directories: Vec<NonRootMPath>,
    max_files: usize,
    max_file_bytes: u64,
    max_total_bytes: u64,
    repo_blobstore: Arc<RepoBlobstore>,
    repo_derived_data: Arc<RepoDerivedData>,
    counters: Arc<GuardCounters>,
}

fn is_guarded_file(directory: &NonRootMPath, path: &NonRootMPath) -> bool {
    path.num_components() == directory.num_components() + 1
        && path.basename().as_ref().ends_with(b".json")
}

impl Guard {
    /// The most paths a commit may change under one guarded directory: every
    /// entry removed and as many added.
    fn max_changes(&self) -> usize {
        self.max_files.saturating_mul(2)
    }

    /// The changes `bcs` makes to guarded files. Looks the directories and
    /// their ancestors up in the file and subtree changes, and reads at most
    /// `max_changes` file changes and one subtree change under each
    /// directory.
    fn guarded_changes(
        &self,
        bcs: &BonsaiChangesetMut,
    ) -> Result<Vec<(NonRootMPath, Option<FileRef>)>, SnapshotError> {
        let mut changes = Vec::new();
        for directory in &self.directories {
            // A subtree operation at, above or below a guarded directory
            // changes its files without listing them.
            let directory_path = MPath::from(directory.clone());
            let at_or_above = iter::once(MPath::ROOT)
                .chain(directory.clone().into_non_root_ancestors().map(MPath::from))
                .find(|path| bcs.subtree_changes.contains_key(path));
            let below = bcs
                .subtree_changes
                .range::<MPath, _>(&directory_path..)
                .next()
                .map(|(path, _)| path)
                .filter(|path| directory_path.is_prefix_of(*path))
                .cloned();
            if let Some(path) = at_or_above.or(below) {
                return Err(SnapshotError::SubtreeChange { path });
            }

            for ancestor in directory.clone().into_non_root_ancestors() {
                if let Some(change) = bcs.file_changes.get(&ancestor)
                    && !matches!(change, FileChange::Deletion | FileChange::UntrackedDeletion)
                {
                    return Err(SnapshotError::UnexpectedPath { path: ancestor });
                }
            }
            let under = bcs
                .file_changes
                .range::<NonRootMPath, _>(directory..)
                .take_while(|(path, _)| directory.is_prefix_of(*path))
                .filter(|(path, _)| *path != directory);
            for (index, (path, change)) in under.enumerate() {
                if index >= self.max_changes() {
                    return Err(SnapshotError::TooManyChanges {
                        directory: directory.clone(),
                        limit: self.max_changes(),
                    });
                }
                let file = match change {
                    FileChange::Change(change) => Some(FileRef {
                        content_id: change.content_id(),
                        file_type: change.file_type(),
                        size: change.size(),
                    }),
                    FileChange::Deletion => None,
                    FileChange::UntrackedChange(_) | FileChange::UntrackedDeletion => {
                        return Err(SnapshotError::UntrackedChange { path: path.clone() });
                    }
                };
                match (is_guarded_file(directory, path), file) {
                    (true, file) => changes.push((path.clone(), file)),
                    // Deleting anything else cannot hide a guarded file.
                    (false, None) => {}
                    (false, Some(_)) => {
                        return Err(SnapshotError::UnexpectedPath { path: path.clone() });
                    }
                }
            }
        }
        Ok(changes)
    }
}

/// Checks the `source_revision` of the generated files `config` names in
/// every commit pushrebase lands. Add it after every hook that changes the
/// rebased commits: it reads them as the hooks before it leave them.
pub struct SourceRevisionPushrebaseHook {
    guard: Arc<Guard>,
}

impl SourceRevisionPushrebaseHook {
    /// Fails if `config` has no directories, has one inside another, or has
    /// a zero limit, which would reject every landing that touches them.
    pub fn try_new(
        config: &SourceRevisionGuardConfig,
        repo: &(impl RepoBlobstoreArc + RepoDerivedDataArc),
    ) -> Result<Box<dyn PushrebaseHook>> {
        Ok(Self::try_new_with_counters(config, repo)?.0)
    }

    /// `try_new`, also returning the counters the hook records its outcomes
    /// in, alongside the process's stats.
    pub fn try_new_with_counters(
        config: &SourceRevisionGuardConfig,
        repo: &(impl RepoBlobstoreArc + RepoDerivedDataArc),
    ) -> Result<(Box<dyn PushrebaseHook>, Arc<GuardCounters>)> {
        ensure!(
            !config.directories.is_empty(),
            "source_revision guard for {} has no directories",
            config.bookmark
        );
        ensure!(
            config.max_files > 0 && config.max_file_bytes > 0 && config.max_total_bytes > 0,
            "source_revision guard for {} needs positive limits, got max_files {}, max_file_bytes {}, max_total_bytes {}",
            config.bookmark,
            config.max_files,
            config.max_file_bytes,
            config.max_total_bytes
        );
        for (index, directory) in config.directories.iter().enumerate() {
            for other in &config.directories[index + 1..] {
                ensure!(
                    !directory.is_prefix_of(other) && !other.is_prefix_of(directory),
                    "source_revision guard for {} has nested directories {directory} and {other}",
                    config.bookmark
                );
            }
        }
        let counters = Arc::new(GuardCounters::default());
        let hook = Box::new(Self {
            guard: Arc::new(Guard {
                directories: config.directories.clone(),
                max_files: config.max_files,
                max_file_bytes: config.max_file_bytes,
                max_total_bytes: config.max_total_bytes,
                repo_blobstore: repo.repo_blobstore_arc(),
                repo_derived_data: repo.repo_derived_data_arc(),
                counters: counters.clone(),
            }),
        });
        Ok((hook, counters))
    }
}

#[async_trait]
impl PushrebaseHook for SourceRevisionPushrebaseHook {
    /// Derive the content manifests of `changesets`, the commits a previous
    /// attempt needed and found underived. This runs outside the critical
    /// section; inside it the guard only fetches derived data.
    async fn prepare(&self, ctx: &CoreContext, changesets: &[ChangesetId]) -> Result<()> {
        self.guard.counters.prepared();
        let derived_data = &self.guard.repo_derived_data;
        stream::iter(changesets.iter().copied())
            .map(|changeset| async move {
                derived_data
                    .derive::<RootContentManifestId>(ctx, changeset, DerivationPriority::HIGH)
                    .await
                    .with_context(|| format!("while deriving the content manifest of {changeset}"))
            })
            .buffer_unordered(4)
            .try_collect::<Vec<_>>()
            .await?;
        Ok(())
    }

    async fn in_critical_section(
        &self,
        _ctx: &CoreContext,
        old_bookmark_value: Option<ChangesetId>,
    ) -> Result<Box<dyn PushrebaseCommitHook>> {
        Ok(Box::new(SourceRevisionCommitHook {
            guard: self.guard.clone(),
            onto: old_bookmark_value,
            rebased: HashMap::new(),
        }))
    }
}

struct SourceRevisionCommitHook {
    guard: Arc<Guard>,
    onto: Option<ChangesetId>,
    /// Keyed by the commit before it was rebased.
    rebased: HashMap<ChangesetId, RebasedCommit>,
}

#[async_trait]
impl PushrebaseCommitHook for SourceRevisionCommitHook {
    fn post_rebase_changeset(
        &mut self,
        bcs_old: ChangesetId,
        bcs_new: &mut BonsaiChangesetMut,
    ) -> Result<()> {
        let changes = self.guard.guarded_changes(bcs_new).map_err(|error| {
            self.guard.counters.rejected_layout();
            SourceRevisionGuardError::InvalidChange {
                changeset: bcs_old,
                error,
            }
        })?;
        self.rebased.insert(
            bcs_old,
            RebasedCommit {
                parents: bcs_new.parents.clone(),
                changes,
            },
        );
        Ok(())
    }

    async fn into_transaction_hook(
        mut self: Box<Self>,
        ctx: &CoreContext,
        changesets: &RebasedChangesets,
    ) -> Result<Box<dyn PushrebaseTransactionHook>> {
        let commits = changesets
            .iter()
            .map(|(old, (new, _))| {
                let commit = self.rebased.remove(old).ok_or_else(|| {
                    anyhow!("source_revision guard did not see {old} being rebased to {new}")
                })?;
                Ok((*new, commit))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        let checker = Checker {
            guard: &self.guard,
            onto: self.onto,
            commits: &commits,
        };
        let result = checker.check(ctx).await;
        self.guard.counters.outcome(GuardOutcome::of(&result));
        result?;
        Ok(Box::new(CheckedTransactionHook))
    }
}

/// How a check ended, for the stats: only `Rejected` turns a landing down
/// for its content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardOutcome {
    Passed,
    Rejected,
    /// A parent's content manifest is missing; pushrebase prepares it and
    /// retries.
    PreparationRequired,
    /// Reading the commits or their files failed.
    ReadFailed,
}

impl GuardOutcome {
    pub fn of(result: &Result<()>) -> Self {
        match result {
            Ok(()) => Self::Passed,
            Err(error) if error.is::<SourceRevisionGuardError>() => Self::Rejected,
            Err(error) if error.is::<PreparationRequired>() => Self::PreparationRequired,
            Err(_) => Self::ReadFailed,
        }
    }
}

/// The check is done by the time the bookmark is written.
struct CheckedTransactionHook;

#[async_trait]
impl PushrebaseTransactionHook for CheckedTransactionHook {
    async fn populate_transaction(
        &self,
        _ctx: &CoreContext,
        txn: Transaction,
    ) -> Result<Transaction, BookmarkTransactionError> {
        Ok(txn)
    }
}

struct Checker<'a> {
    guard: &'a Guard,
    onto: Option<ChangesetId>,
    /// Keyed by the rebased commit.
    commits: &'a HashMap<ChangesetId, RebasedCommit>,
}

impl Checker<'_> {
    async fn check(&self, ctx: &CoreContext) -> Result<()> {
        // A merge can bring a guarded file in from a parent other than its
        // first without listing it.
        let relevant = self
            .commits
            .values()
            .any(|commit| !commit.changes.is_empty() || commit.parents.len() > 1);
        if !relevant {
            return Ok(());
        }
        self.guard.counters.checked();

        let order = self.parents_first()?;
        let mut snapshots = HashMap::new();
        for &id in &order {
            let snapshot = self.rebased_snapshot(ctx, id, &mut snapshots).await?;
            // Checked before the snapshot is kept or built on, so a long
            // stack cannot grow it past the limit.
            if let Some(error) = self.too_many_files(&snapshot) {
                return Err(SourceRevisionGuardError::InvalidCandidate {
                    candidate: id,
                    error,
                }
                .into());
            }
            snapshots.insert(id, snapshot);
        }

        // Parents first, so that a rejection names the commit that
        // introduced the problem rather than one of its descendants. Only
        // the last candidate's parsed files are kept, for its child.
        let empty = Snapshot::default();
        let mut last_parsed: Option<(ChangesetId, ParsedSnapshot)> = None;
        for candidate in order {
            let commit = &self.commits[&candidate];
            let parent = self.mainline_parent(commit);
            let parent_snapshot = match parent {
                Some(parent) => snapshots
                    .get(&parent)
                    .ok_or_else(|| anyhow!("source_revision guard has no snapshot of {parent}"))?,
                None => &empty,
            };
            let candidate_snapshot = &snapshots[&candidate];
            if parent_snapshot.files == candidate_snapshot.files {
                continue;
            }
            if commit.parents.len() > 1 {
                return Err(
                    SourceRevisionGuardError::MergeChangesGeneratedFiles { candidate }.into(),
                );
            }

            let parsed_candidate = self
                .parse(ctx, candidate_snapshot)
                .await
                .with_context(|| format!("while reading generated source files in {candidate}"))?
                .map_err(|error| SourceRevisionGuardError::InvalidCandidate { candidate, error })?;
            // A root commit's parent is the empty snapshot, which is
            // unversioned; `candidate` stands in for the missing parent id.
            let parent = parent.unwrap_or(candidate);
            let parsed_parent = match last_parsed.take() {
                Some((id, parsed)) if id == parent => parsed,
                _ => self
                    .parse(ctx, parent_snapshot)
                    .await
                    .with_context(|| format!("while reading generated source files in {parent}"))?
                    .map_err(|error| SourceRevisionGuardError::InvalidParent {
                        candidate,
                        parent,
                        error,
                    })?,
            };
            check_transition(&parsed_parent, &parsed_candidate).map_err(|error| {
                SourceRevisionGuardError::InvalidTransition {
                    candidate,
                    parent,
                    error,
                }
            })?;
            last_parsed = Some((candidate, parsed_candidate));
        }
        Ok(())
    }

    /// The parent a rebased commit lands on: its first parent that is
    /// itself being landed or is the bookmark's value. A commit with neither
    /// (one that creates the bookmark) lands on its first parent.
    fn mainline_parent(&self, commit: &RebasedCommit) -> Option<ChangesetId> {
        commit
            .parents
            .iter()
            .copied()
            .find(|parent| self.commits.contains_key(parent) || Some(*parent) == self.onto)
            .or_else(|| commit.parents.first().copied())
    }

    /// The rebased commits, each after its rebased parents.
    fn parents_first(&self) -> Result<Vec<ChangesetId>> {
        let mut waiting_on: HashMap<ChangesetId, usize> = HashMap::new();
        let mut children: HashMap<ChangesetId, Vec<ChangesetId>> = HashMap::new();
        for (&id, commit) in self.commits {
            let rebased_parents: Vec<_> = commit
                .parents
                .iter()
                .filter(|parent| self.commits.contains_key(parent))
                .collect();
            waiting_on.insert(id, rebased_parents.len());
            for parent in rebased_parents {
                children.entry(*parent).or_default().push(id);
            }
        }
        let mut ready: Vec<_> = waiting_on
            .iter()
            .filter(|(_, count)| **count == 0)
            .map(|(id, _)| *id)
            .collect();
        let mut order = Vec::with_capacity(self.commits.len());
        while let Some(id) = ready.pop() {
            order.push(id);
            for child in children.get(&id).into_iter().flatten() {
                let count = waiting_on
                    .get_mut(child)
                    .expect("every child is a rebased commit");
                *count -= 1;
                if *count == 0 {
                    ready.push(*child);
                }
            }
        }
        ensure!(
            order.len() == self.commits.len(),
            "rebased commits do not form a DAG"
        );
        Ok(order)
    }

    /// The snapshot of rebased commit `id`, whose rebased parents are
    /// already in `snapshots`. A file the commit does not list comes from
    /// its first parent that has it, which bonsai guarantees is the content
    /// of every parent that has one.
    async fn rebased_snapshot(
        &self,
        ctx: &CoreContext,
        id: ChangesetId,
        snapshots: &mut HashMap<ChangesetId, Snapshot>,
    ) -> Result<Snapshot> {
        let commit = &self.commits[&id];
        let mut snapshot = match commit.parents.first() {
            Some(parent) => self.parent_snapshot(ctx, id, *parent, snapshots).await?,
            None => Snapshot::default(),
        };
        for parent in commit.parents.iter().skip(1) {
            let other = self.parent_snapshot(ctx, id, *parent, snapshots).await?;
            for (path, file) in other.files {
                snapshot.files.entry(path).or_insert(file);
            }
        }
        for (path, file) in &commit.changes {
            match file {
                Some(file) => snapshot.files.insert(path.clone(), *file),
                None => snapshot.files.remove(path),
            };
        }
        Ok(snapshot)
    }

    async fn parent_snapshot(
        &self,
        ctx: &CoreContext,
        candidate: ChangesetId,
        parent: ChangesetId,
        snapshots: &mut HashMap<ChangesetId, Snapshot>,
    ) -> Result<Snapshot> {
        if let Some(snapshot) = snapshots.get(&parent) {
            return Ok(snapshot.clone());
        }
        let snapshot = self.landed_snapshot(ctx, candidate, parent).await?;
        snapshots.insert(parent, snapshot.clone());
        Ok(snapshot)
    }

    /// The snapshot of `parent`, a commit that is not being landed, from its
    /// derived content manifest. Nothing is derived or replayed here: if the
    /// manifest is missing, the attempt fails with `PreparationRequired`, and
    /// pushrebase derives it in `prepare`, outside the critical section,
    /// before retrying.
    async fn landed_snapshot(
        &self,
        ctx: &CoreContext,
        candidate: ChangesetId,
        parent: ChangesetId,
    ) -> Result<Snapshot> {
        let Some(root) = self
            .guard
            .repo_derived_data
            .fetch_derived::<RootContentManifestId>(ctx, parent)
            .await?
        else {
            return Err(PreparationRequired {
                changesets: vec![parent],
            }
            .into());
        };
        self.manifest_snapshot(ctx, parent, root)
            .await?
            .map_err(|error| {
                SourceRevisionGuardError::InvalidParent {
                    candidate,
                    parent,
                    error,
                }
                .into()
            })
    }

    /// The guarded files in the content manifest `root`. Lists only the
    /// guarded directories, and at most `max_files + 1` entries of each.
    /// The inner error is a directory over the limit.
    async fn manifest_snapshot(
        &self,
        ctx: &CoreContext,
        parent: ChangesetId,
        root: RootContentManifestId,
    ) -> Result<Result<Snapshot, SnapshotError>> {
        let blobstore = self.guard.repo_blobstore.as_ref();
        let trees: Vec<(MPath, Entry<_, _>)> = root
            .into_content_manifest_id()
            .find_entries(
                ctx.clone(),
                blobstore.clone(),
                self.guard.directories.iter().cloned(),
            )
            .try_collect()
            .await
            .with_context(|| {
                format!("while looking up generated source directories in {parent}")
            })?;

        let mut snapshot = Snapshot::default();
        for (directory, entry) in trees {
            let (Some(directory), Entry::Tree(tree_id)) =
                (directory.into_optional_non_root_path(), entry)
            else {
                continue;
            };
            let limit = self.guard.max_files;
            let entries: Vec<_> = tree_id
                .load(ctx, blobstore)
                .await?
                .list(ctx, blobstore)
                .await?
                .take(limit.saturating_add(1))
                .try_collect()
                .await
                .with_context(|| format!("while listing {directory} in {parent}"))?;
            if entries.len() > limit {
                return Ok(Err(SnapshotError::TooManyFiles { directory, limit }));
            }
            for (name, entry) in entries {
                let path = directory.join(&name);
                match entry {
                    Entry::Leaf(file) if is_guarded_file(&directory, &path) => {
                        snapshot.files.insert(
                            path,
                            FileRef {
                                content_id: file.content_id,
                                file_type: file.file_type,
                                size: file.size,
                            },
                        );
                    }
                    // Anything else could hide generated files from the
                    // check, and would leave `max_files` without one meaning.
                    _ => return Ok(Err(SnapshotError::UnexpectedPath { path })),
                }
            }
        }
        Ok(Ok(snapshot))
    }

    /// A guarded directory with more than `max_files` files in `snapshot`.
    fn too_many_files(&self, snapshot: &Snapshot) -> Option<SnapshotError> {
        self.guard.directories.iter().find_map(|directory| {
            snapshot
                .files
                .keys()
                .filter(|path| directory.is_prefix_of(*path))
                .nth(self.guard.max_files)
                .map(|_| SnapshotError::TooManyFiles {
                    directory: directory.clone(),
                    limit: self.guard.max_files,
                })
        })
    }

    /// Read and check `snapshot`, fetching at most 16 files at once. The
    /// outer error is a failure to read; the inner one says why what was
    /// read is not a valid snapshot.
    async fn parse(
        &self,
        ctx: &CoreContext,
        snapshot: &Snapshot,
    ) -> Result<Result<ParsedSnapshot, SnapshotError>> {
        if let Some(error) = self.too_many_files(snapshot) {
            return Ok(Err(error));
        }
        let files = &snapshot.files;
        let size = files
            .values()
            .fold(0u64, |total, file| total.saturating_add(file.size));
        if size > self.guard.max_total_bytes {
            return Ok(Err(SnapshotError::SnapshotTooLarge {
                size,
                limit: self.guard.max_total_bytes,
            }));
        }
        for (path, file) in files {
            if !matches!(file.file_type, FileType::Regular | FileType::Executable) {
                return Ok(Err(SnapshotError::NotRegularFile { path: path.clone() }));
            }
            if file.size > self.guard.max_file_bytes {
                return Ok(Err(SnapshotError::FileTooLarge {
                    path: path.clone(),
                    size: file.size,
                    limit: self.guard.max_file_bytes,
                }));
            }
        }

        let to_fetch: HashMap<ContentId, u64> = files
            .values()
            .map(|file| (file.content_id, file.size))
            .collect();
        let contents: HashMap<ContentId, Bytes> = stream::iter(to_fetch)
            .map(|(content_id, size)| async move {
                let bytes = filestore::fetch_concat_exact(
                    self.guard.repo_blobstore.as_ref(),
                    ctx,
                    content_id,
                    size,
                )
                .await?;
                Ok::<_, Error>((content_id, bytes))
            })
            .buffer_unordered(16)
            .try_collect()
            .await?;

        let parsed = files
            .iter()
            .map(|(path, file)| Ok((path.clone(), parse_file(path, &contents[&file.content_id])?)))
            .collect::<Result<_, SnapshotError>>();
        Ok(parsed.and_then(|parsed| parse_snapshot(&self.guard.directories, parsed)))
    }
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;
    use mononoke_types_mocks::changesetid::ONES_CSID;

    use super::*;

    #[mononoke::test]
    fn test_only_guard_errors_count_as_rejections() {
        assert_eq!(GuardOutcome::of(&Ok(())), GuardOutcome::Passed);
        assert_eq!(
            GuardOutcome::of(&Err(SourceRevisionGuardError::MergeChangesGeneratedFiles {
                candidate: ONES_CSID,
            }
            .into())),
            GuardOutcome::Rejected
        );
        assert_eq!(
            GuardOutcome::of(&Err(Error::from(
                SourceRevisionGuardError::MergeChangesGeneratedFiles {
                    candidate: ONES_CSID,
                }
            )
            .context("while checking"))),
            GuardOutcome::Rejected,
            "context does not hide a rejection"
        );
        assert_eq!(
            GuardOutcome::of(&Err(PreparationRequired {
                changesets: vec![ONES_CSID],
            }
            .into())),
            GuardOutcome::PreparationRequired
        );
        assert_eq!(
            GuardOutcome::of(&Err(
                anyhow!("blobstore unavailable").context("while reading generated source files")
            )),
            GuardOutcome::ReadFailed
        );
    }
}
