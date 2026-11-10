/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Mononoke pushrebase implementation. The main goal of pushrebase is to decrease push contention.
//! Commits that client pushed are rebased on top of `onto_bookmark` on the server
//!
//!  Client
//!  ```text
//!     O <- `onto` on client, potentially outdated
//!     |
//!     O  O <- pushed set (in this case just one commit)
//!     | /
//!     O <- root
//!  ```
//!
//!  Server
//!  ```text
//!     O  <- update `onto` bookmark, pointing at the pushed commit
//!     |
//!     O  <- `onto` bookmark on the server before the push
//!     |
//!     O
//!     |
//!     O
//!     |
//!     O <- root
//!  ```
//!
//!  Terminology:
//!  *onto bookmark* - bookmark that is the destination of the rebase, for example "master"
//!
//!  *pushed set* - a set of commits that client has sent us.
//!  Note: all pushed set MUST be committed before doing pushrebase
//!  Note: pushed set MUST contain only one head
//!  Note: not all commits from pushed set maybe rebased on top of onto bookmark. See *rebased set*
//!
//!  *root* - parents of pushed set that are not in the pushed set (see graphs above)
//!
//!  *rebased set* - subset of pushed set that will be rebased on top of onto bookmark
//!  Note: Usually rebased set == pushed set. However in case of merges it may differ
//!
//! Pushrebase supports hooks, which can be used to modify rebased Bonsai commits as well as
//! sideload database updates in the transaction that moves forward the bookmark. See hooks.rs for
//! more information on those;

#![feature(trait_alias)]

use std::cmp::max;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::format_err;
use async_trait::async_trait;
use blobrepo_utils::convert_diff_result_into_file_change_for_diamond_merge;
use blobstore::Loadable;
use bonsai_git_mapping::BonsaiGitMappingArc;
use bonsai_globalrev_mapping::BonsaiGlobalrevMappingArc;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkTransactionHook;
use bookmarks::BookmarkUpdateLogId;
use bookmarks::BookmarkUpdateReason;
use bookmarks::BookmarksRef;
use bytes::Bytes;
use commit_graph::CommitGraphRef;
use commit_graph::CommitGraphWriterRef;
use content_manifest_derivation::RootContentManifestId;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use filenodes_derivation::FilenodesOnlyPublic;
use filestore::FilestoreConfigRef;
use futures::FutureExt;
use futures::StreamExt;
use futures::TryFutureExt;
use futures::TryStream;
use futures::TryStreamExt;
use futures::future;
use futures::future::try_join;
use futures::future::try_join_all;
use futures::stream;
use manifest::BonsaiDiffFileChange;
use manifest::ManifestOps;
use manifest::bonsai_diff;
use maplit::hashmap;
use mercurial_derivation::DeriveHgChangeset;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgChangesetId;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestId;
use mercurial_types::NonRootMPath;
use mercurial_types::blobs::hg_manifests_read_from_augmented;
use mercurial_types::sharded_augmented_manifest::hg_manifest_bonsai_diff;
use metaconfig_types::MergeResolutionOverride;
use metaconfig_types::PushrebaseFlags;
use metaconfig_types::RepoConfigRef;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::ContentId;
use mononoke_types::ContentManifestId;
use mononoke_types::DateTime;
use mononoke_types::DerivableType;
use mononoke_types::FileChange;
use mononoke_types::FileType;
use mononoke_types::GitLfs;
use mononoke_types::MPath;
use mononoke_types::PrefixTrie;
use mononoke_types::Timestamp;
use mononoke_types::check_case_conflicts;
use mononoke_types::find_path_conflicts;
use pushrebase_hook::PushrebaseCommitHook;
use pushrebase_hook::PushrebaseHook;
use pushrebase_hook::PushrebaseTransactionHook;
use pushrebase_hook::RebasedChangesets;
use pushrebase_mutation_mapping::PushrebaseMutationMappingRef;
use repo_blobstore::RepoBlobstoreArc;
use repo_bookmark_attrs::RepoBookmarkAttrsRef;
use repo_cross_repo::RepoCrossRepoRef;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentityRef;
use scuba_ext::MononokeScubaSampleBuilder;
use shared_error::std::SharedError;
use stats::prelude::*;
use thiserror::Error;

mod merge_resolution_summary;
mod queue;
mod take_incoming;
pub use merge_resolution_summary::MR_PATH_SAMPLE_CAP;
pub use merge_resolution_summary::MergeResolutionSummary;
pub use queue::PushrebaseQueue;
pub use queue::PushrebaseRequest;
use queue::QueuedPushrebaseRequest;
use take_incoming::Resolution;
use take_incoming::TakeIncoming;
use take_incoming::TakeIncomingScope;
use take_incoming::log_take_incoming;
use take_incoming::take_incoming_scope;
use three_way_merge::MergeResult;
use three_way_merge::merge_text;
use three_way_merge::merge_text_prefer_local;
use tracing::info;
use tracing::warn;

define_stats! {
    prefix = "mononoke.pushrebase";
    // Clowntown: This is actually nanoseconds (ns), not microseconds (us)
    critical_section_success_duration_us: dynamic_timeseries("{}.critical_section_success_duration_us", (reponame: String); Average, Sum, Count),
    critical_section_failure_duration_us: dynamic_timeseries("{}.critical_section_failure_duration_us", (reponame: String); Average, Sum, Count),
    critical_section_retries_failed: dynamic_timeseries("{}.critical_section_retries_failed", (reponame: String); Average, Sum),
    commits_rebased: dynamic_timeseries("{}.commits_rebased", (reponame: String); Average, Sum, Count),
    conflict_rejections: dynamic_timeseries("{}.conflict_rejections", (reponame: String); Count),
    conflict_files_count: dynamic_timeseries("{}.conflict_files_count", (reponame: String); Average, Sum, Count),
    merge_resolution_lost_on_retry: dynamic_timeseries("{}.merge_resolution_lost_on_retry", (reponame: String); Count),
    noop_merge_commits_detected: dynamic_timeseries("{}.noop_merge_commits_detected", (reponame: String); Count),
    noop_merge_commits_rejected: dynamic_timeseries("{}.noop_merge_commits_rejected", (reponame: String); Count),
    baseline_replay: dynamic_timeseries("{}.baseline_replay.{}.{}", (reponame: String, mode: &'static str, outcome: &'static str); Count),
    baseline_resolution_source: dynamic_timeseries("{}.baseline_resolution_source.{}", (reponame: String, source: &'static str); Count),
}

const MAX_REBASE_ATTEMPTS: usize = 100;

/// 0 rejects as before, 1 replays but still rejects, 2 lands the replay.
const REPLAY_FROM_FORK_JK: &str = "scm/mononoke:pushrebase_replay_from_fork_point";
/// Longest `fork..head` replayed; longer is rejected as before.
const REPLAY_MAX_COMMITS: usize = 50;
/// Changed-path bound for the replay's manifest lookups.
const REPLAY_MAX_PATHS: usize = 5_000;
/// Range length reported for an over-cap replay; longer logs as this.
const REPLAY_LEN_PROBE_CAP: usize = 1_000;

/// Ancestry probes over a baseline's recorded successors. Normally one.
const BASELINE_SUCCESSOR_CONCURRENCY: usize = 5;
/// Concurrent changeset loads when attributing a replay conflict.
const CONFLICT_ATTRIBUTION_CONCURRENCY: usize = 10;

pub const MUTATION_KEYS: &[&str] = &["mutpred", "mutuser", "mutdate", "mutop", "mutsplit"];

pub const FAIL_PUSHREBASE_EXTRA: &str = "failpushrebase";

/// Hg extra, with value `1`, on a merge that pushrebase rebased with its
/// parents reordered (see `PushrebaseFlags::mainline_parent_first`).
/// The Mercurial changeset hash does not depend on the order of the parents,
/// so without it a reordered merge that is otherwise unchanged would have the
/// same hash as the pushed one.
pub const PARENTS_SWAPPED_EXTRA: &str = "parents_swapped";

#[derive(Debug, Error)]
pub enum PushrebaseInternalError {
    #[error("Bonsai not found for hg changeset: {0}")]
    BonsaiNotFoundForHgChangeset(HgChangesetId),
    #[error("Pushrebase onto bookmark not found: {0}")]
    PushrebaseBookmarkNotFound(BookmarkKey),
    #[error("Only one head is allowed in pushed set")]
    PushrebaseTooManyHeads,
    #[error("No common pushrebase root for {0}, all possible roots: {1:?}")]
    PushrebaseNoCommonRoot(BookmarkKey, HashSet<ChangesetId>),
    #[error("No pushrebase roots found")]
    NoRoots,
    #[error("Pushrebase failed after too many unsuccessful rebases")]
    TooManyRebaseAttempts,
    #[error("Forbid pushrebase because root ({0}) is not a p1 of {1} bookmark")]
    P2RootRebaseForbidden(HgChangesetId, BookmarkKey),
    #[error("Unexpected file conflicts when adding new file changes to {0}")]
    NewFileChangesConflict(ChangesetId),
    #[error("Cannot reorder the parents of {0}: it already has the parents_swapped extra")]
    ParentsAlreadySwapped(ChangesetId),
    #[error("Merge resolution was performed in a previous attempt but lost on retry")]
    MergeResolutionLostOnRetry,
}

#[derive(Debug, Error)]
pub enum PushrebaseError {
    #[error("Conflicts while pushrebasing: {0:?}")]
    Conflicts(Vec<PushrebaseConflict>),
    #[error(
        "PotentialCaseConflict: the change this commit introduces at {0} may conflict with other commits. Rebase and retry."
    )]
    PotentialCaseConflict(NonRootMPath),
    #[error("Pushrebase over merge")]
    RebaseOverMerge,
    #[error("Root is too far behind")]
    RootTooFarBehind,
    /// The stack's required root is not where pushrebase would rebase it
    /// from, or is no longer an ancestor of the bookmark.
    #[error("bookmark '{bookmark}' moved while the push was being checked. Pull and retry")]
    BookmarkMoved {
        bookmark: BookmarkKey,
        required_root: ChangesetId,
    },
    #[error("baseline {baseline} {rejection} (onto {onto})")]
    UnplaceableBaseline {
        baseline: ChangesetId,
        onto: ChangesetId,
        rejection: BaselineRejection,
    },
    #[error(
        "Force failed pushrebase, please do a manual rebase. (Bonsai changeset id that triggered it is {0})"
    )]
    ForceFailPushrebase(ChangesetId),
    #[error("manifest not derived for {0}")]
    ManifestNotDerived(ChangesetId),
    #[error("stack changes {paths} paths, over the limit of {max_paths}")]
    TooManyPaths { paths: usize, max_paths: usize },
    #[error(transparent)]
    Error(#[from] Error),
}

/// Why `rebase_stack_onto_with_baseline` could not place a stack on the
/// branch. Displays as the clause after `"baseline <id>"`; the wording is
/// client-visible through Multi-Repo Land's conflict descriptions, where the
/// stack head is called "the target".
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BaselineRejection {
    /// The stack head does not descend from the baseline.
    NotAncestorOfHead,
    /// Off the branch, and no rewritten form of it is on the branch either.
    NotOnBranch,
    /// Rewritten onto the branch, but sharing no single fork point with the
    /// head to bound the server range.
    NoSingleForkPoint,
    /// A commit at or below the baseline did not drop on replay, and the
    /// branch changed a path it touches.
    ParentLandedWithChanges(ChangesetId),
    /// A commit at or below the baseline did not drop on replay, and the
    /// branch left every path it touches alone.
    ParentNotLanded(ChangesetId),
    /// A commit above the baseline is already on the branch.
    AlreadyLanded(ChangesetId),
    /// The stack forked from the branch more than `max_commits` ago.
    HistoryTooLong { max_commits: usize },
}

impl BaselineRejection {
    /// The commit the rejection names, for callers that render it in their
    /// own identifiers.
    pub fn named_commit(&self) -> Option<ChangesetId> {
        match self {
            Self::ParentLandedWithChanges(cs_id)
            | Self::ParentNotLanded(cs_id)
            | Self::AlreadyLanded(cs_id) => Some(*cs_id),
            Self::NotAncestorOfHead
            | Self::NotOnBranch
            | Self::NoSingleForkPoint
            | Self::HistoryTooLong { .. } => None,
        }
    }
}

/// Branch commits that may be the landed form of an off-branch baseline,
/// found from SCM-specific rewrite metadata the caller owns. Pushrebase
/// requires exactly one candidate and still proves it by content.
#[async_trait]
pub trait BaselineCandidateSource: Send + Sync {
    /// Logged as the resolution source.
    fn name(&self) -> &'static str;

    async fn candidates_for(
        &self,
        ctx: &CoreContext,
        baseline: ChangesetId,
        onto: ChangesetId,
        fork_point: ChangesetId,
    ) -> Result<Vec<ChangesetId>>;
}

impl std::fmt::Display for BaselineRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAncestorOfHead => f.write_str("is not an ancestor of the target"),
            Self::NotOnBranch => f.write_str(
                "is not an ancestor of the current head and no rewritten form of it is on \
                 the branch; refusing to rebase across a force-move or unrelated history",
            ),
            Self::NoSingleForkPoint => f.write_str("has no single fork point on the current head"),
            Self::ParentLandedWithChanges(parent) => write!(
                f,
                "is not on the current head, and the stack's parent commit {parent} landed \
                 with different content than this stack was built on; rebase the stack onto \
                 the landed form and resubmit"
            ),
            Self::ParentNotLanded(parent) => write!(
                f,
                "is not on the current head, and the stack's parent commit {parent} has not \
                 landed on this branch; land it first or land the stack together"
            ),
            Self::AlreadyLanded(commit) => write!(
                f,
                "is not on the current head, and commit {commit} of this stack is already on \
                 the branch; nothing new to land"
            ),
            Self::HistoryTooLong { max_commits } => write!(
                f,
                "is not on the current head, and the stack's history forked from the branch \
                 more than {max_commits} commits ago; rebase the stack onto the current head \
                 and resubmit"
            ),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PushrebaseConflict {
    pub left: MPath,
    pub right: MPath,
}

impl PushrebaseConflict {
    fn new(left: MPath, right: MPath) -> Self {
        PushrebaseConflict { left, right }
    }
}

impl From<PushrebaseInternalError> for PushrebaseError {
    fn from(error: PushrebaseInternalError) -> Self {
        PushrebaseError::Error(error.into())
    }
}

#[derive(Debug, Clone)]
pub struct PushrebaseChangesetPair {
    pub id_old: ChangesetId,
    pub id_new: ChangesetId,
}

fn rebased_changesets_into_pairs(
    rebased_changesets: RebasedChangesets,
) -> Vec<PushrebaseChangesetPair> {
    rebased_changesets
        .into_iter()
        .map(|(id_old, (id_new, _))| PushrebaseChangesetPair { id_old, id_new })
        .collect()
}

/// Adapter that bridges the carry-forward state (`Vec<MergedFileInfo>`)
/// to `MergeResolutionSummary::from_carried_paths`. Returns `None` when
/// no MR ran in any prior attempt; otherwise a `Succeeded` summary
/// reconstructed from the path list.
fn synthesize_carried_summary(carried: &[MergedFileInfo]) -> Option<MergeResolutionSummary> {
    if carried.is_empty() {
        return None;
    }
    let paths: Vec<NonRootMPath> = carried.iter().map(|info| info.path.clone()).collect();
    Some(MergeResolutionSummary::from_carried_paths(paths))
}

#[derive(Debug, Clone, Copy)]
pub struct PushrebaseRetryNum(pub usize);

#[derive(Debug, Clone, Copy)]
pub struct PushrebaseDistance(pub usize);

/// Whether a pushrebase transaction must verify that the repository is writable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoLockPolicy {
    Enforce,
    Bypass,
}

#[derive(Debug, Clone)]
pub struct PushrebaseOutcome {
    pub old_bookmark_value: Option<ChangesetId>,
    pub head: ChangesetId,
    pub retry_num: PushrebaseRetryNum,
    pub rebased_changesets: Vec<PushrebaseChangesetPair>,
    pub pushrebase_distance: PushrebaseDistance,
    pub log_id: BookmarkUpdateLogId,
    /// Paths that were resolved via server-side 3-way merge.
    /// `None` means no merge resolution was performed (no conflicts, or feature disabled).
    /// `Some(paths)` means these paths had conflicting edits that were auto-merged.
    pub merge_resolved_paths: Option<Vec<NonRootMPath>>,
    /// Per-push merge-resolution summary. `None` during the staged rollout
    /// when not yet populated by all pushrebase paths; once every path
    /// populates this, the field will be tightened to required.
    /// See `MergeResolutionSummary::add_to_scuba` for the Scuba schema.
    pub merge_summary: Option<MergeResolutionSummary>,
}

/// A pushed stack prepared for conflict checking and rebasing.
pub struct PushrebaseStack {
    /// Changed files in the pushed stack.
    pub changed_files: Vec<MPath>,
    /// Bonsai changesets to rebase, topological order (ancestor first).
    pub changesets: Vec<BonsaiChangeset>,
    /// Head of the pushed stack.
    pub head: ChangesetId,
    /// Root of the pushed stack.
    pub root: ChangesetId,
    /// Set by `require_root`: `root` must be an ancestor of the bookmark
    /// value that each landing attempt reads.
    required_root: Option<ChangesetId>,
    conflict_check_base: ChangesetId,
    carried_merge_file_info: Vec<MergedFileInfo>,
}

impl PushrebaseStack {
    /// Requires the stack to be rebased from `root`. Fails unless `root` is
    /// the root pushrebase chose for it; every landing attempt then also
    /// fails unless `root` is an ancestor of the bookmark value it reads, so
    /// the bookmark's compare-and-swap only lands the stack while that holds.
    pub fn require_root(
        &mut self,
        bookmark: &BookmarkKey,
        root: ChangesetId,
    ) -> Result<(), PushrebaseError> {
        if self.root != root {
            return Err(PushrebaseError::BookmarkMoved {
                bookmark: bookmark.clone(),
                required_root: root,
            });
        }
        self.required_root = Some(root);
        Ok(())
    }

    /// Fails unless the required root, if any, is an ancestor of
    /// `bookmark_value`.
    async fn check_required_root(
        &self,
        ctx: &CoreContext,
        repo: &impl Repo,
        bookmark: &BookmarkKey,
        bookmark_value: Option<ChangesetId>,
    ) -> Result<(), PushrebaseError> {
        let Some(required_root) = self.required_root else {
            return Ok(());
        };
        let on_bookmark = match bookmark_value {
            Some(value) => {
                repo.commit_graph()
                    .is_ancestor(ctx, required_root, value)
                    .await?
            }
            None => false,
        };
        if on_bookmark {
            Ok(())
        } else {
            Err(PushrebaseError::BookmarkMoved {
                bookmark: bookmark.clone(),
                required_root,
            })
        }
    }

    /// Precomputes conflict information against a bookmark snapshot.
    /// Calling this method is optional; the batch worker performs any remaining work.
    pub async fn precompute(
        &mut self,
        ctx: &CoreContext,
        repo: &impl Repo,
        flags: &PushrebaseFlags,
        bookmark: &BookmarkKey,
    ) -> Result<(), PushrebaseError> {
        let ctx = pushrebase_context(ctx, flags);
        let start = Instant::now();
        let onto = get_bookmark_value(&ctx, repo, bookmark)
            .await?
            .unwrap_or(self.root);
        let checked = check_pushrebase_conflicts(
            &ctx,
            repo,
            flags,
            self.root,
            self.conflict_check_base,
            onto,
            &self.changesets,
            &self.changed_files,
        )
        .await?;
        self.carried_merge_file_info = reconcile_merge_file_info(
            &self.carried_merge_file_info,
            &checked.merged_file_overrides.unwrap_or_default(),
        );
        self.conflict_check_base = onto;
        ctx.scuba()
            .clone()
            .add("repo_name", repo.repo_identity().name())
            .add("bookmark", bookmark.as_str())
            .add(
                "pushrebase_preparation_duration_ms",
                start.elapsed().as_millis() as i64,
            )
            .log_with_msg("pushrebase_prepared", None);
        Ok(())
    }
}

pub trait Repo = BookmarksRef
    + RepoBlobstoreArc
    + RepoDerivedDataRef
    + RepoIdentityRef
    + FilestoreConfigRef
    + CommitGraphRef
    + CommitGraphWriterRef
    + Send
    + Sync;

/// Repository capabilities needed to construct hooks for queued pushrebases.
pub trait PushrebaseQueueRepo = Repo
    + BonsaiGitMappingArc
    + BonsaiGlobalrevMappingArc
    + PushrebaseMutationMappingRef
    + RepoBookmarkAttrsRef
    + RepoConfigRef
    + RepoCrossRepoRef;

fn pushrebase_context(ctx: &CoreContext, config: &PushrebaseFlags) -> CoreContext {
    ctx.with_mutated_scuba(|mut scuba| {
        if let Some(land_instance_id) = config.land_instance_id.as_deref() {
            scuba.add("land_instance_id", land_instance_id);
        }
        if let Some(phab_diff_id) = config.phab_diff_id.as_deref() {
            scuba.add("phab_diff_id", phab_diff_id);
        }
        scuba
    })
}

/// Does a pushrebase of a list of commits `pushed` onto `onto_bookmark`
/// The commits from the pushed set should already be committed to the blobrepo
/// Returns updated bookmark value.
///
/// With `required_root`, the stack must be rebased from it (see
/// `PushrebaseStack::require_root`).
pub async fn do_pushrebase_bonsai(
    ctx: &CoreContext,
    repo: &impl Repo,
    config: &PushrebaseFlags,
    onto_bookmark: &BookmarkKey,
    pushed: &HashSet<BonsaiChangeset>,
    prepushrebase_hooks: &[Box<dyn PushrebaseHook>],
    required_root: Option<ChangesetId>,
) -> Result<PushrebaseOutcome, PushrebaseError> {
    // Tag every Scuba sample emitted during this pushrebase with the
    // land-attribution keys from the request pushvars, so downstream
    // samples can be joined back to the originating land job and diff.
    // Rebinding `ctx` here means every downstream `ctx.scuba()` clone
    // inherits the fields.
    let ctx = pushrebase_context(ctx, config);
    let ctx = &ctx;

    let mut stack = index_pushrebase_request(ctx, repo, config, onto_bookmark, pushed).await?;
    if let Some(root) = required_root {
        stack.require_root(onto_bookmark, root)?;
    }
    rebase_in_loop(
        ctx,
        repo,
        config,
        onto_bookmark,
        &stack,
        prepushrebase_hooks,
    )
    .await
}

/// Computes changed files, head, root, and bonsai changesets for a pushed
/// stack.
pub async fn index_pushrebase_request(
    ctx: &CoreContext,
    repo: &impl Repo,
    config: &PushrebaseFlags,
    onto_bookmark: &BookmarkKey,
    pushed: &HashSet<BonsaiChangeset>,
) -> Result<PushrebaseStack, PushrebaseError> {
    let head = find_only_head_or_fail(pushed)?;
    let roots = find_roots(pushed);
    let root = find_closest_root(ctx, repo, config, onto_bookmark, &roots).await?;

    let client_bcs = fetch_bonsai_range_ancestor_not_included(ctx, repo, root, head).await?;
    let client_cf = find_changed_files_with(
        ctx,
        repo,
        root,
        head,
        &client_bcs,
        RangeDiffManifests::FromKnob,
    )
    .await?;

    check_filenodes_backfilled(ctx, repo, &head, config.not_generated_filenodes_limit).await?;

    Ok(PushrebaseStack {
        changed_files: client_cf,
        changesets: client_bcs,
        head,
        root,
        required_root: None,
        conflict_check_base: root,
        carried_merge_file_info: vec![],
    })
}

#[derive(Debug)]
pub struct RebasedStack {
    pub new_head: ChangesetId,
    pub rebased_changesets: Vec<PushrebaseChangesetPair>,
    /// NOT yet saved; the caller must persist before referencing.
    pub rebased_bonsais: Vec<BonsaiChangeset>,
    pub merge_summary: MergeResolutionSummary,
    /// Commits skipped because their new parent already had every change.
    /// Only `rebase_stack_onto_manifest` drops; the other entries reject.
    pub dropped: Vec<ChangesetId>,
    /// Old id -> paths whose content was 3-way merged in that commit.
    pub merged_paths: HashMap<ChangesetId, Vec<NonRootMPath>>,
    /// Stack paths the branch also changed between `root` and `onto`.
    /// Only `rebase_stack_onto_manifest` fills this.
    pub server_overlaps: Vec<MPath>,
}

/// Rebases the linear stack `root..head` onto `onto` with pushrebase's
/// conflict detection and commit rewrite, moving no bookmark and creating no
/// changesets; merged file content is stored when merge resolution is on.
/// Runs no hooks; honors the caller's merge resolution setting. With it on, a
/// replay of an already-landed stack merges to a no-op rather than conflicting
/// on paths, so callers relying on replay failing closed must keep it off or
/// rely on the no-op merge commit rejection. Overlaps whose server content
/// still equals the base are taken as-is; a replay differs from the base and
/// is unaffected. Requires `rewritedates == false`
/// (checked) — date stamping makes the rewrite non-deterministic, breaking
/// callers' retry termination. Rejects merge commits in the stack; uses
/// content-manifest range diffs so repos without HG derived data work.
pub async fn rebase_stack_onto(
    ctx: &CoreContext,
    repo: &impl Repo,
    config: &PushrebaseFlags,
    root: ChangesetId,
    head: ChangesetId,
    onto: ChangesetId,
) -> Result<RebasedStack, PushrebaseError> {
    rebase_stack_onto_with_conflict_base(ctx, repo, config, root, root, head, onto).await
}

/// `rebase_stack_onto` where the server-side range is measured from
/// `conflict_base` instead of `root`.
///
/// They differ only when `root` was itself rewritten by an earlier land: the
/// author's stack still descends from `root`, so it stays the range base and
/// the re-parent source, but `root` is no longer on the branch and cannot
/// bound the server range. `conflict_base` is where the stack left the
/// branch, so the range covers everything the stack has not seen, including
/// the root's own landed form. Overlaps the server never changed relative to
/// the root are the stack's own earlier commits and are taken as-is; the
/// caller must have proven the root landed under another identity. Pass
/// `root` for both to get the ordinary behaviour.
pub async fn rebase_stack_onto_with_conflict_base(
    ctx: &CoreContext,
    repo: &impl Repo,
    config: &PushrebaseFlags,
    root: ChangesetId,
    conflict_base: ChangesetId,
    head: ChangesetId,
    onto: ChangesetId,
) -> Result<RebasedStack, PushrebaseError> {
    if config.rewritedates {
        return Err(PushrebaseError::Error(anyhow!(
            "rebase_stack_onto requires rewritedates to be off: date stamping \
             makes the rewrite non-deterministic, breaking retry termination"
        )));
    }

    // The server range is bounded by conflict_base, so that is what must
    // precede onto; root only has to bound the client stack.
    let (root_is_ancestor_of_head, base_is_ancestor_of_onto) = try_join(
        repo.commit_graph().is_ancestor(ctx, root, head),
        repo.commit_graph().is_ancestor(ctx, conflict_base, onto),
    )
    .await
    .map_err(PushrebaseError::Error)?;
    if !root_is_ancestor_of_head {
        return Err(PushrebaseError::Error(anyhow!(
            "rebase_stack_onto: root {root} must be an ancestor of head {head}, but is not"
        )));
    }
    if !base_is_ancestor_of_onto {
        return Err(PushrebaseError::Error(anyhow!(
            "rebase_stack_onto: conflict base {conflict_base} must be an ancestor of \
             onto {onto}, but is not (was the destination force-moved off the shared \
             history?)"
        )));
    }

    let client_bcs = fetch_bonsai_range_ancestor_not_included(ctx, repo, root, head).await?;
    if client_bcs.is_empty() {
        return Err(PushrebaseError::Error(anyhow!(
            "rebase_stack_onto: empty stack between root {root} and head {head}"
        )));
    }
    if let Some(merge) = client_bcs.iter().find(|bcs| bcs.is_merge()) {
        return Err(PushrebaseError::Error(anyhow!(
            "rebase_stack_onto does not support merge commits in the stack: {}",
            merge.get_changeset_id()
        )));
    }

    let client_cf = find_changed_files_with(
        ctx,
        repo,
        root,
        head,
        &client_bcs,
        RangeDiffManifests::ContentManifest,
    )
    .await?;

    let conflict_result = check_pushrebase_conflicts_with(
        ctx,
        repo,
        config,
        root,
        conflict_base,
        onto,
        &client_bcs,
        &client_cf,
        RangeDiffManifests::ContentManifest,
        UnchangedOverlap::TakeClient,
    )
    .await?;

    let mut no_hooks: Vec<Box<dyn PushrebaseCommitHook>> = Vec::new();
    let (new_head, rebased_changesets, rebased_bonsais) = create_rebased_changesets(
        ctx,
        repo,
        config,
        &client_bcs,
        root,
        head,
        onto,
        &mut no_hooks,
        conflict_result.merged_file_overrides,
    )
    .await?;

    Ok(RebasedStack {
        new_head,
        rebased_changesets: rebased_changesets_into_pairs(rebased_changesets),
        rebased_bonsais,
        merge_summary: conflict_result.merge_summary,
        dropped: Vec::new(),
        merged_paths: HashMap::new(),
        server_overlaps: Vec::new(),
    })
}

/// `rebase_stack_onto` for a caller that knows where its stack was built
/// (`baseline`) but not whether that point is still on the branch, because
/// an earlier pushrebase may have rewritten it. A baseline on the branch is
/// the ordinary case. One off the branch is resolved through the pushrebase
/// mutation mapping: when exactly one recorded successor is an ancestor of
/// `onto`, or the caller's candidate source names exactly one branch commit
/// the baseline drops against, the stack's base landed under another identity
/// and the server range starts at the fork point of `baseline` and `onto`.
/// Otherwise the stack is replayed from that fork point, and anything that
/// does not drop the baseline's history is `UnplaceableBaseline`, never a
/// parent-swap onto unrelated history.
pub async fn rebase_stack_onto_with_baseline(
    ctx: &CoreContext,
    repo: &(impl Repo + PushrebaseMutationMappingRef),
    config: &PushrebaseFlags,
    baseline: ChangesetId,
    head: ChangesetId,
    onto: ChangesetId,
    candidates: Option<&dyn BaselineCandidateSource>,
) -> Result<RebasedStack, PushrebaseError> {
    let unplaceable = |rejection| PushrebaseError::UnplaceableBaseline {
        baseline,
        onto,
        rejection,
    };
    let (baseline_precedes_onto, baseline_precedes_head) = try_join(
        repo.commit_graph().is_ancestor(ctx, baseline, onto),
        repo.commit_graph().is_ancestor(ctx, baseline, head),
    )
    .await
    .map_err(PushrebaseError::Error)?;
    if !baseline_precedes_head {
        return Err(unplaceable(BaselineRejection::NotAncestorOfHead));
    }
    let conflict_base = if baseline_precedes_onto {
        baseline
    } else {
        // The knob is read here only: an on-branch baseline never reaches it.
        let mode = ReplayMode::from_knob(repo.repo_identity().name());
        let fork_point = single_fork_point(ctx, repo, baseline, onto)
            .await
            .map_err(PushrebaseError::Error)?;
        if !rewritten_baseline_is_on_branch(
            ctx, repo, config, mode, baseline, onto, fork_point, candidates,
        )
        .await
        {
            return replay_from_fork_point(
                ctx, repo, config, mode, baseline, head, onto, fork_point,
            )
            .await;
        }
        fork_point.ok_or_else(|| unplaceable(BaselineRejection::NoSingleForkPoint))?
    };
    rebase_stack_onto_with_conflict_base(ctx, repo, config, baseline, conflict_base, head, onto)
        .await
}

/// Rebases `root..head` onto `onto` with no assumption about the ancestry
/// between them: the server side is the difference between the two
/// manifests over the stack's own paths, so `onto` may be older than or
/// divergent from `root`. Never derives; both manifests must exist.
/// Commits whose every change is already in their new parent are dropped,
/// as `sl rebase` does. Moves no bookmark, runs no hooks, writes no
/// changesets; merged content is stored when merge resolution is on.
/// Requires `rewritedates == false`. `max_paths` bounds the stack's changed
/// paths, which is what the manifest lookups scale with.
pub async fn rebase_stack_onto_manifest(
    ctx: &CoreContext,
    repo: &impl Repo,
    config: &PushrebaseFlags,
    root: ChangesetId,
    head: ChangesetId,
    onto: ChangesetId,
    max_paths: usize,
) -> Result<RebasedStack, PushrebaseError> {
    if config.rewritedates {
        return Err(PushrebaseError::Error(anyhow!(
            "rebase_stack_onto_manifest requires rewritedates to be off"
        )));
    }
    if !repo
        .commit_graph()
        .is_ancestor(ctx, root, head)
        .await
        .map_err(PushrebaseError::Error)?
    {
        return Err(PushrebaseError::Error(anyhow!(
            "rebase_stack_onto_manifest: root {root} must be an ancestor of head {head}, but is not"
        )));
    }

    let (root_mf, onto_mf) = try_join(
        fetch_root_manifest_id(ctx, repo, root),
        fetch_root_manifest_id(ctx, repo, onto),
    )
    .await
    .map_err(PushrebaseError::Error)?;
    let root_mf = root_mf.ok_or(PushrebaseError::ManifestNotDerived(root))?;
    let onto_mf = onto_mf.ok_or(PushrebaseError::ManifestNotDerived(onto))?;

    let client_bcs = fetch_bonsai_range_ancestor_not_included(ctx, repo, root, head).await?;
    if client_bcs.is_empty() {
        return Err(PushrebaseError::Error(anyhow!(
            "rebase_stack_onto_manifest: empty stack between root {root} and head {head}"
        )));
    }
    if let Some(merge) = client_bcs.iter().find(|bcs| bcs.is_merge()) {
        return Err(PushrebaseError::Error(anyhow!(
            "rebase_stack_onto_manifest does not support merge commits in the stack: {}",
            merge.get_changeset_id()
        )));
    }
    let client_cf = find_changed_files_with(
        ctx,
        repo,
        root,
        head,
        &client_bcs,
        RangeDiffManifests::ContentManifest,
    )
    .await?;
    if client_cf.len() > max_paths {
        return Err(PushrebaseError::TooManyPaths {
            paths: client_cf.len(),
            max_paths,
        });
    }

    let (base_states, onto_states) = try_join(
        lookup_manifest_states(ctx, repo, root_mf, &client_cf),
        lookup_manifest_states(ctx, repo, onto_mf, &client_cf),
    )
    .await
    .map_err(PushrebaseError::Error)?;
    let server_cf = manifest_overlaps(&client_cf, &base_states, &onto_states);
    let overlaps = server_cf.clone();

    let reponame = repo.repo_identity().name();
    let (overrides, conflict_files_count) = match intersect_changed_files(overlaps, client_cf) {
        Ok(()) => (None, 0),
        Err(PushrebaseError::Conflicts(conflicts)) => {
            let merge_enabled = match config.merge_resolution_override {
                MergeResolutionOverride::ForceOn => true,
                MergeResolutionOverride::ForceOff => false,
                MergeResolutionOverride::UseJk => justknobs::eval(
                    "scm/mononoke:pushrebase_enable_merge_resolution",
                    None,
                    Some(reponame),
                ),
            };
            if !merge_enabled {
                return Err(PushrebaseError::Conflicts(conflicts));
            }
            let max_conflicts: usize = justknobs::get_as::<usize>(
                "scm/mononoke:pushrebase_max_merge_conflicts",
                Some(reponame),
            );
            let max_file_size: u64 = justknobs::get_as::<u64>(
                "scm/mononoke:pushrebase_max_merge_file_size",
                Some(reponame),
            );
            match collect_overlap_states(
                &conflicts,
                &client_bcs,
                &base_states,
                &onto_states,
                max_conflicts,
                max_file_size,
                &config.merge_resolution_excluded_path_prefixes,
                take_incoming_scope(config, reponame),
            ) {
                Ok(overrides) => (Some(overrides), conflicts.len() as u64),
                Err(err) => {
                    ctx.scuba()
                        .clone()
                        .add("repo_name", reponame)
                        .add("merge_resolution_outcome", format!("{err}"))
                        .log_with_msg("Pushrebase merge resolution failed", None);
                    return Err(PushrebaseError::Conflicts(conflicts));
                }
            }
        }
        Err(e) => return Err(e),
    };

    let mut no_hooks: Vec<Box<dyn PushrebaseCommitHook>> = Vec::new();
    let set = create_rebased_changesets_with(
        ctx,
        repo,
        config,
        &client_bcs,
        root,
        head,
        onto,
        &mut no_hooks,
        overrides,
        NoopPolicy::Drop,
    )
    .await?;

    let merge_summary = if conflict_files_count == 0 {
        MergeResolutionSummary::NotNeeded
    } else {
        let resolved: HashSet<&NonRootMPath> = set.merged_paths.values().flatten().collect();
        MergeResolutionSummary::Succeeded {
            conflict_files_count,
            resolved_files_count: resolved.len() as u64,
            resolved_paths_sample: resolved
                .into_iter()
                .take(MR_PATH_SAMPLE_CAP)
                .cloned()
                .collect(),
        }
    };
    Ok(RebasedStack {
        new_head: set.new_head,
        rebased_changesets: rebased_changesets_into_pairs(set.rebased),
        rebased_bonsais: set.bonsais,
        merge_summary,
        dropped: set.dropped,
        merged_paths: set.merged_paths,
        server_overlaps: server_cf,
    })
}

/// Whether an earlier pushrebase rewrote `baseline` into a commit that is an
/// ancestor of `onto`: the proof that the stack's base is on the branch under
/// another identity.
///
/// A candidate must be an ancestor of `onto`, so a bad row cannot place a
/// stack on unrelated history. Ambiguity, no row, a read failure, or a chain
/// (one hop only) all mean "no"; a read failure is logged rather than
/// propagated so the caller's rejection stays a rejection.
async fn rewritten_baseline_is_on_branch(
    ctx: &CoreContext,
    repo: &(impl Repo + PushrebaseMutationMappingRef),
    config: &PushrebaseFlags,
    mode: ReplayMode,
    baseline: ChangesetId,
    onto: ChangesetId,
    fork_point: Option<ChangesetId>,
    candidates: Option<&dyn BaselineCandidateSource>,
) -> bool {
    match try_resolve_rewritten_baseline(
        ctx, repo, config, mode, baseline, onto, fork_point, candidates,
    )
    .await
    {
        Ok(resolved) => resolved.is_some(),
        Err(err) => {
            ctx.scuba()
                .clone()
                .add("log_tag", "orphaned_baseline_resolution_failed")
                .add("repo", repo.repo_identity().name())
                .add("old_target", baseline.to_string())
                .add("error", format!("{err:#}"))
                .unsampled()
                .log();
            false
        }
    }
}

async fn try_resolve_rewritten_baseline(
    ctx: &CoreContext,
    repo: &(impl Repo + PushrebaseMutationMappingRef),
    config: &PushrebaseFlags,
    mode: ReplayMode,
    baseline: ChangesetId,
    onto: ChangesetId,
    fork_point: Option<ChangesetId>,
    candidates: Option<&dyn BaselineCandidateSource>,
) -> Result<Option<ChangesetId>> {
    let repo_name = repo.repo_identity().name();
    // Deduped: no unique constraint, so a retried land can record a pair twice.
    let successors: HashSet<ChangesetId> = repo
        .pushrebase_mutation_mapping()
        .get_successor_ids(ctx, baseline)
        .await
        .with_context(|| {
            format!("reading recorded successors of baseline {baseline} in {repo_name}")
        })?
        .into_iter()
        .collect();

    let on_branch: Vec<ChangesetId> =
        stream::iter(successors.iter().copied().map(|successor| async move {
            let reachable = repo
                .commit_graph()
                .is_ancestor(ctx, successor, onto)
                .await
                .with_context(|| {
                    format!("checking whether recorded successor {successor} is on the branch")
                })?;
            anyhow::Ok(reachable.then_some(successor))
        }))
        .buffer_unordered(BASELINE_SUCCESSOR_CONCURRENCY)
        .try_filter_map(|candidate| async move { Ok(candidate) })
        .try_collect()
        .await?;

    let mut scuba = ctx.scuba().clone();
    scuba
        .add("log_tag", "orphaned_baseline_resolution")
        .add("repo", repo_name)
        .add("mode", mode.label())
        .add("old_target", baseline.to_string())
        .add("onto", onto.to_string())
        .add("recorded_successors", successors.len())
        .add("candidates_on_branch", on_branch.len());
    // Exactly one, or we do not guess.
    let (resolved, source) = match (on_branch.as_slice(), candidates) {
        ([only], _) => (Some(*only), "mapping"),
        ([], Some(candidates)) => {
            match resolve_through_candidate(
                ctx, repo, config, mode, baseline, onto, fork_point, candidates, &mut scuba,
            )
            .await?
            {
                Some(landed) => (Some(landed), candidates.name()),
                None => (None, "none"),
            }
        }
        _ => (None, "none"),
    };
    if resolved.is_some() {
        STATS::baseline_resolution_source.add_value(1, (repo_name.to_string(), source));
    }
    // 0/0 is "no row"; n/0 is "recorded but off-branch".
    scuba
        .add("resolution_source", source)
        .add(
            "resolved_to",
            resolved.map_or_else(|| "none".to_string(), |cs| cs.to_string()),
        )
        .unsampled()
        .log();
    Ok(resolved)
}

/// The caller's source picks one branch commit; `fork..baseline` must then
/// drop against it by content before it stands in for the baseline.
async fn resolve_through_candidate(
    ctx: &CoreContext,
    repo: &impl Repo,
    config: &PushrebaseFlags,
    mode: ReplayMode,
    baseline: ChangesetId,
    onto: ChangesetId,
    fork_point: Option<ChangesetId>,
    candidates: &dyn BaselineCandidateSource,
    scuba: &mut MononokeScubaSampleBuilder,
) -> Result<Option<ChangesetId>> {
    if mode == ReplayMode::Off || merging_forced_off(config) {
        scuba.add("candidate_source", "off");
        return Ok(None);
    }
    let Some(fork_point) = fork_point else {
        scuba.add("candidate_source", "no_single_fork_point");
        return Ok(None);
    };
    let found = candidates
        .candidates_for(ctx, baseline, onto, fork_point)
        .await?;
    scuba.add("candidate_count", found.len());
    let [candidate] = found.as_slice() else {
        scuba.add(
            "candidate_source",
            if found.is_empty() {
                "none"
            } else {
                "ambiguous"
            },
        );
        return Ok(None);
    };
    let candidate = *candidate;
    scuba.add("candidate", candidate.to_string());
    if !repo
        .commit_graph()
        .is_ancestor(ctx, candidate, onto)
        .await?
    {
        scuba.add("candidate_source", "candidate_off_branch");
        return Ok(None);
    }

    let guard = replay_stack_from_fork(
        ctx, repo, config, baseline, baseline, candidate, fork_point, scuba,
    )
    .await;
    scuba.add("candidate_guard", guard.label());
    match (guard, mode) {
        (ReplayOutcome::Replayed(_), ReplayMode::Enforce) => {
            scuba.add("candidate_source", "resolved");
            Ok(Some(candidate))
        }
        (ReplayOutcome::Replayed(_), _) => {
            scuba.add("candidate_source", "shadow_would_resolve");
            Ok(None)
        }
        _ => {
            scuba.add("candidate_source", "guard_rejected");
            Ok(None)
        }
    }
}

fn merging_forced_off(config: &PushrebaseFlags) -> bool {
    matches!(
        config.merge_resolution_override,
        MergeResolutionOverride::ForceOff
    )
}

async fn single_fork_point(
    ctx: &CoreContext,
    repo: &impl Repo,
    baseline: ChangesetId,
    onto: ChangesetId,
) -> Result<Option<ChangesetId>> {
    Ok(
        match repo
            .commit_graph()
            .common_base(ctx, baseline, onto)
            .await?
            .as_slice()
        {
            [fork_point] => Some(*fork_point),
            _ => None,
        },
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplayMode {
    Off,
    Shadow,
    Enforce,
}

impl ReplayMode {
    fn from_knob(repo_name: &str) -> Self {
        match justknobs::get_as::<i64>(REPLAY_FROM_FORK_JK, Some(repo_name)) {
            1 => Self::Shadow,
            2 => Self::Enforce,
            _ => Self::Off,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::Enforce => "enforce",
        }
    }
}

/// What replaying `fork..head` onto the branch would do.
enum ReplayOutcome {
    /// Every commit at or below the baseline dropped as already landed.
    Replayed(RebasedStack),
    /// A commit at or below the baseline did not drop or conflicted, and the
    /// branch changed a path it touches.
    ParentDiffers(ChangesetId),
    /// A commit at or below the baseline did not drop, and the branch left
    /// its paths alone: it never landed.
    ParentNotLanded(ChangesetId),
    /// A commit above the baseline dropped: it is already on the branch.
    AlreadyLanded(ChangesetId),
    /// The stack's own commits conflict with the branch.
    Conflict(PushrebaseError),
    /// Out of bounds; rejected as today.
    TooLong,
    TooManyPaths(usize),
    /// Could not be evaluated; rejected as today.
    Failed(PushrebaseError),
}

impl ReplayOutcome {
    fn label(&self) -> &'static str {
        match self {
            Self::Replayed(_) => "replayed",
            Self::ParentDiffers(_) => "parent_differs",
            Self::ParentNotLanded(_) => "parent_not_landed",
            Self::AlreadyLanded(_) => "already_landed",
            Self::Conflict(_) => "conflict",
            Self::TooLong => "too_long",
            Self::TooManyPaths(_) => "too_many_paths",
            Self::Failed(_) => "failed",
        }
    }
}

/// Replays `fork..head` onto the branch; lands only if every parent dropped
/// and nothing above the baseline did.
async fn replay_from_fork_point(
    ctx: &CoreContext,
    repo: &impl Repo,
    config: &PushrebaseFlags,
    mode: ReplayMode,
    baseline: ChangesetId,
    head: ChangesetId,
    onto: ChangesetId,
    fork_point: Option<ChangesetId>,
) -> Result<RebasedStack, PushrebaseError> {
    let repo_name = repo.repo_identity().name();
    let unplaceable = |rejection| PushrebaseError::UnplaceableBaseline {
        baseline,
        onto,
        rejection,
    };
    // A caller that forbids content merging cannot have parents drop by content.
    if mode == ReplayMode::Off || merging_forced_off(config) {
        return Err(unplaceable(BaselineRejection::NotOnBranch));
    }
    let Some(fork_point) = fork_point else {
        return Err(unplaceable(BaselineRejection::NoSingleForkPoint));
    };

    let mut scuba = ctx.scuba().clone();
    scuba
        .add("log_tag", "baseline_replay_from_fork_point")
        .add("repo", repo_name)
        .add("mode", mode.label())
        .add("resolution_source", "replay")
        .add("old_target", baseline.to_string())
        .add("onto", onto.to_string())
        .add("head", head.to_string());
    let mut outcome = replay_stack_from_fork(
        ctx, repo, config, baseline, head, onto, fork_point, &mut scuba,
    )
    .await;
    if let ReplayOutcome::Replayed(rebased) = &outcome {
        if rebased.rebased_changesets.is_empty() {
            outcome = ReplayOutcome::AlreadyLanded(head);
        }
    }
    scuba.add("outcome", outcome.label());
    match &outcome {
        ReplayOutcome::ParentDiffers(parent) | ReplayOutcome::ParentNotLanded(parent) => {
            scuba.add("undropped_parent", parent.to_string());
        }
        ReplayOutcome::AlreadyLanded(commit) => {
            scuba.add("already_landed", commit.to_string());
        }
        ReplayOutcome::Conflict(err) | ReplayOutcome::Failed(err) => {
            scuba.add("error", format!("{err:#}"));
        }
        ReplayOutcome::TooManyPaths(paths) => {
            scuba.add("changed_paths", *paths);
        }
        ReplayOutcome::Replayed(_) | ReplayOutcome::TooLong => {}
    }
    scuba.unsampled().log();
    STATS::baseline_replay.add_value(1, (repo_name.to_string(), mode.label(), outcome.label()));
    if matches!(outcome, ReplayOutcome::Replayed(_)) {
        STATS::baseline_resolution_source.add_value(1, (repo_name.to_string(), "replay"));
    }

    if mode == ReplayMode::Shadow {
        return Err(unplaceable(BaselineRejection::NotOnBranch));
    }
    match outcome {
        ReplayOutcome::Replayed(stack) => Ok(stack),
        ReplayOutcome::ParentDiffers(parent) => Err(unplaceable(
            BaselineRejection::ParentLandedWithChanges(parent),
        )),
        ReplayOutcome::ParentNotLanded(parent) => {
            Err(unplaceable(BaselineRejection::ParentNotLanded(parent)))
        }
        ReplayOutcome::AlreadyLanded(commit) => {
            Err(unplaceable(BaselineRejection::AlreadyLanded(commit)))
        }
        ReplayOutcome::Conflict(err) => Err(err),
        ReplayOutcome::TooLong => Err(unplaceable(BaselineRejection::HistoryTooLong {
            max_commits: REPLAY_MAX_COMMITS,
        })),
        ReplayOutcome::TooManyPaths(_) | ReplayOutcome::Failed(_) => {
            Err(unplaceable(BaselineRejection::NotOnBranch))
        }
    }
}

async fn replay_stack_from_fork(
    ctx: &CoreContext,
    repo: &impl Repo,
    config: &PushrebaseFlags,
    baseline: ChangesetId,
    head: ChangesetId,
    onto: ChangesetId,
    fork_point: ChangesetId,
    scuba: &mut MononokeScubaSampleBuilder,
) -> ReplayOutcome {
    scuba.add("fork_point", fork_point.to_string());
    let (parents, stack) = match try_join(
        range_ids_above_capped(ctx, repo, fork_point, baseline, REPLAY_MAX_COMMITS),
        range_ids_above_capped(ctx, repo, fork_point, head, REPLAY_MAX_COMMITS),
    )
    .await
    {
        Ok((Some(parents), Some(stack))) => (parents, stack),
        Ok(_) => {
            // Sized for the cap: how far past it the ranges run.
            if let Ok((parent_len, replay_len)) = try_join(
                range_len_capped(ctx, repo, fork_point, baseline, REPLAY_LEN_PROBE_CAP),
                range_len_capped(ctx, repo, fork_point, head, REPLAY_LEN_PROBE_CAP),
            )
            .await
            {
                scuba
                    .add("replay_len", replay_len)
                    .add("parent_len", parent_len);
            }
            return ReplayOutcome::TooLong;
        }
        Err(err) => return ReplayOutcome::Failed(PushrebaseError::Error(err)),
    };
    let parent_set: HashSet<ChangesetId> = parents.iter().copied().collect();
    let above: Vec<ChangesetId> = stack
        .iter()
        .copied()
        .filter(|cs_id| !parent_set.contains(cs_id))
        .collect();
    scuba
        .add("replay_len", stack.len())
        .add("parent_len", parents.len());

    // Dropping a parent means comparing merged content with the branch.
    let flags = PushrebaseFlags {
        merge_resolution_override: MergeResolutionOverride::ForceOn,
        ..config.clone()
    };
    let rebased = match rebase_stack_onto_manifest(
        ctx,
        repo,
        &flags,
        fork_point,
        head,
        onto,
        REPLAY_MAX_PATHS,
    )
    .await
    {
        Ok(rebased) => rebased,
        Err(PushrebaseError::TooManyPaths { paths, .. }) => {
            return ReplayOutcome::TooManyPaths(paths);
        }
        Err(err @ PushrebaseError::Conflicts(_))
        | Err(err @ PushrebaseError::PotentialCaseConflict(_)) => {
            // Only a conflict no commit above the baseline took part in is the parent's.
            return match parent_only_conflict(ctx, repo, &parents, &above, &err).await {
                Ok(Some(parent)) => ReplayOutcome::ParentDiffers(parent),
                Ok(None) => ReplayOutcome::Conflict(err),
                Err(load_err) => ReplayOutcome::Failed(PushrebaseError::Error(load_err)),
            };
        }
        Err(err) => return ReplayOutcome::Failed(err),
    };

    let dropped: HashSet<ChangesetId> = rebased.dropped.iter().copied().collect();
    let undropped: Vec<ChangesetId> = parents
        .iter()
        .copied()
        .filter(|parent| !dropped.contains(parent))
        .collect();
    let landed_above: Vec<ChangesetId> = above
        .iter()
        .copied()
        .filter(|cs_id| dropped.contains(cs_id))
        .collect();
    scuba
        .add("dropped_count", rebased.dropped.len())
        .add("undropped_below_baseline", undropped.len())
        .add("dropped_above_baseline", landed_above.len());
    rebased.merge_summary.add_to_scuba(scuba);
    if let Some(parent) = undropped.first() {
        return match branch_touched_parent_paths(ctx, repo, *parent, &rebased.server_overlaps).await
        {
            Ok(true) => ReplayOutcome::ParentDiffers(*parent),
            Ok(false) => ReplayOutcome::ParentNotLanded(*parent),
            Err(err) => ReplayOutcome::Failed(PushrebaseError::Error(err)),
        };
    }
    if let Some(commit) = landed_above.first() {
        return ReplayOutcome::AlreadyLanded(*commit);
    }
    ReplayOutcome::Replayed(rebased)
}

/// Whether the branch changed a path `parent` touches, or one under or above
/// it. `server_overlaps` is already bounded by the replay's path cap.
async fn branch_touched_parent_paths(
    ctx: &CoreContext,
    repo: &impl Repo,
    parent: ChangesetId,
    server_overlaps: &[MPath],
) -> Result<bool> {
    if server_overlaps.is_empty() {
        return Ok(false);
    }
    let overlaps: HashSet<&MPath> = server_overlaps.iter().collect();
    let bcs = parent.load(ctx, &repo.repo_blobstore()).await?;
    let parent_paths: HashSet<MPath> = bcs
        .file_changes_map()
        .keys()
        .map(|path| MPath::from(path.clone()))
        .collect();
    let under_overlap = parent_paths
        .iter()
        .any(|path| path.clone().into_ancestors().any(|p| overlaps.contains(&p)));
    let over_overlap = server_overlaps.iter().any(|path| {
        path.clone()
            .into_ancestors()
            .any(|p| parent_paths.contains(&p))
    });
    Ok(under_overlap || over_overlap)
}

/// `ancestor..descendant` length without the ancestor, counted up to `cap`.
async fn range_len_capped(
    ctx: &CoreContext,
    repo: &impl CommitGraphRef,
    ancestor: ChangesetId,
    descendant: ChangesetId,
    cap: usize,
) -> Result<usize> {
    Ok(repo
        .commit_graph()
        .range_stream(ctx, ancestor, descendant)
        .await?
        .filter(|cs_id| future::ready(cs_id != &ancestor))
        .take(cap)
        .count()
        .await)
}

/// `ancestor..descendant`, oldest first, without the ancestor; `None` when
/// longer than `cap`.
pub async fn range_ids_above_capped(
    ctx: &CoreContext,
    repo: &impl CommitGraphRef,
    ancestor: ChangesetId,
    descendant: ChangesetId,
    cap: usize,
) -> Result<Option<Vec<ChangesetId>>> {
    let ids: Vec<ChangesetId> = repo
        .commit_graph()
        .range_stream(ctx, ancestor, descendant)
        .await?
        .filter(|cs_id| future::ready(cs_id != &ancestor))
        .take(cap + 1)
        .collect()
        .await;
    Ok((ids.len() <= cap).then_some(ids))
}

/// The first parent commit that touched a conflicting path no commit above
/// the baseline touched; the conflict is then the parent's landed form.
async fn parent_only_conflict(
    ctx: &CoreContext,
    repo: &impl Repo,
    parents: &[ChangesetId],
    above: &[ChangesetId],
    err: &PushrebaseError,
) -> Result<Option<ChangesetId>> {
    let conflict_paths: Vec<MPath> = match err {
        PushrebaseError::Conflicts(conflicts) => conflicts
            .iter()
            .flat_map(|conflict| [conflict.left.clone(), conflict.right.clone()])
            .collect(),
        PushrebaseError::PotentialCaseConflict(path) => vec![MPath::from(path.clone())],
        _ => return Ok(None),
    };
    let load_all = |ids: &[ChangesetId]| {
        stream::iter(ids.to_vec())
            .map(|cs_id| async move { cs_id.load(ctx, &repo.repo_blobstore()).await })
            .buffered(CONFLICT_ATTRIBUTION_CONCURRENCY)
            .try_collect::<Vec<BonsaiChangeset>>()
    };
    let (parent_bcs, above_bcs) = try_join(load_all(parents), load_all(above)).await?;
    let touched = |bcs: &BonsaiChangeset, path: &MPath| {
        bcs.file_changes_map()
            .keys()
            .any(|changed| path.is_related_to(&MPath::from(changed.clone())))
    };
    Ok(parent_bcs
        .iter()
        .find(|parent| {
            conflict_paths.iter().any(|path| {
                touched(parent, path) && !above_bcs.iter().any(|child| touched(child, path))
            })
        })
        .map(|parent| parent.get_changeset_id()))
}

/// A successfully rebased request pending the CAS bookmark update.
struct PendingRebase {
    request: QueuedPushrebaseRequest,
    new_head: ChangesetId,
    pushrebase_distance: usize,
    old_bookmark_value: Option<ChangesetId>,
    merge_resolved_paths: Option<Vec<NonRootMPath>>,
    merge_summary: MergeResolutionSummary,
}

/// Lands multiple indexed stacks in a single critical section pass.
///
/// Takes ownership of requests. Sends results via each request's oneshot
/// for resolved requests. Returns only CAS-failure requests for re-queuing
/// with updated `conflict_check_base`.
async fn do_batched_pushrebase(
    ctx: &CoreContext,
    repo: &impl Repo,
    flags: &PushrebaseFlags,
    onto_bookmark: &BookmarkKey,
    requests: Vec<QueuedPushrebaseRequest>,
    hooks: &[Box<dyn PushrebaseHook>],
) -> Vec<QueuedPushrebaseRequest> {
    let should_log = flags.monitoring_bookmark.as_deref() == Some(onto_bookmark.as_str());
    // Parallel all-bookmarks saturation counters: fire for EVERY bookmark's
    // land, but only in repos already tracked in ODS (monitoring_bookmark set).
    let emit_all_bookmarks = flags.monitoring_bookmark.is_some();
    let reponame = repo.repo_identity().name();
    let repo_args = (reponame.to_string(),);
    let start_critical_section = Instant::now();

    // CRITICAL SECTION START: Read the current bookmark value.
    let old_bookmark_value = match get_bookmark_value(ctx, repo, onto_bookmark).await {
        Ok(v) => v,
        Err(e) => {
            let shared = SharedError::from(e);
            for req in requests {
                let _ = req.response_tx.send(Err(shared.clone()));
            }
            return vec![];
        }
    };

    let mut commit_hooks = match run_batch_hooks(ctx, hooks, old_bookmark_value).await {
        Ok(h) => h,
        Err(e) => {
            let shared = SharedError::from(e);
            for req in requests {
                let _ = req.response_tx.send(Err(shared.clone()));
            }
            return vec![];
        }
    };

    // Per-stack conflict detection and rebase.
    let mut pending: Vec<PendingRebase> = vec![];
    let mut running_head = old_bookmark_value;
    let mut all_rebased_changesets: RebasedChangesets = Default::default();
    let mut all_rebased_bonsais: Vec<BonsaiChangeset> = Vec::new();

    let mut requests_iter = requests.into_iter();
    while let Some(mut request) = requests_iter.next() {
        let request_ctx = pushrebase_context(&request.ctx, flags);
        if let Err(e) = request
            .stack
            .check_required_root(&request_ctx, repo, onto_bookmark, old_bookmark_value)
            .await
        {
            let _ = request.response_tx.send(Err(SharedError::from(e)));
            continue;
        }
        let bookmark_val = old_bookmark_value.unwrap_or(request.stack.root);
        // Only scan the delta since preparation or the last attempt.
        let conflict_result = match check_pushrebase_conflicts(
            &request_ctx,
            repo,
            flags,
            request.stack.root,
            request.stack.conflict_check_base,
            bookmark_val,
            &request.stack.changesets,
            &request.stack.changed_files,
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                let _ = request.response_tx.send(Err(SharedError::from(e)));
                continue;
            }
        };

        // Reconcile carried merge info with delta info from this attempt
        let reconciled_overrides = match conflict_result.merged_file_overrides {
            Some(delta_info) => Some(reconcile_merge_file_info(
                &request.stack.carried_merge_file_info,
                &delta_info,
            )),
            None if !request.stack.carried_merge_file_info.is_empty() => {
                Some(request.stack.carried_merge_file_info.clone())
            }
            None => None,
        };

        let merge_resolved_paths = reconciled_overrides
            .as_ref()
            .map(|overrides| overrides.iter().map(|info| info.path.clone()).collect());

        // The summary for a push that has been retried must reflect that
        // MR previously succeeded — otherwise a clean delta on retry would
        // hide a successful MR run. `carried_merge_file_info` is the
        // signal: non-empty means an earlier attempt resolved conflicts.
        let merge_summary = synthesize_carried_summary(&request.stack.carried_merge_file_info)
            .map(|carried| {
                MergeResolutionSummary::combine(carried, conflict_result.merge_summary.clone())
            })
            .unwrap_or(conflict_result.merge_summary);

        // Store reconciled overrides on the request for carry-forward on re-queue
        request.stack.carried_merge_file_info = reconciled_overrides.clone().unwrap_or_default();
        request.stack.conflict_check_base = bookmark_val;

        let pushrebase_distance = match try_join(
            repo.commit_graph()
                .changeset_linear_depth(&request_ctx, bookmark_val),
            repo.commit_graph()
                .changeset_linear_depth(&request_ctx, request.stack.root),
        )
        .await
        {
            Ok((bookmark_depth, root_depth)) => bookmark_depth.saturating_sub(root_depth) as usize,
            Err(e) => {
                let _ = request
                    .response_tx
                    .send(Err(SharedError::from(PushrebaseError::from(e))));
                continue;
            }
        };

        // Capture the running head before this request's rebase so each
        // request sees the correct "old bookmark value" for its position
        // in the batch.
        let request_old_bookmark_value = running_head;

        // Rebase this stack onto the running head using the immutable root.
        let onto = running_head.unwrap_or(request.stack.root);
        let rebase_result = create_rebased_changesets(
            &request_ctx,
            repo,
            flags,
            &request.stack.changesets,
            request.stack.root,
            request.stack.head,
            onto,
            &mut commit_hooks,
            reconciled_overrides,
        )
        .await;

        match rebase_result {
            Ok((new_head, rebased, rebased_bonsais)) => {
                all_rebased_changesets.extend(rebased);
                all_rebased_bonsais.extend(rebased_bonsais);
                running_head = Some(new_head);
                pending.push(PendingRebase {
                    request,
                    new_head,
                    pushrebase_distance,
                    old_bookmark_value: request_old_bookmark_value,
                    merge_resolved_paths,
                    merge_summary,
                });
            }
            Err(e) => {
                // Fail only the broken request.
                let _ = request.response_tx.send(Err(SharedError::from(e)));
                // `create_rebased_changesets` may have partially mutated
                // `commit_hooks` (e.g. globalrev assignments) before failing.
                // The hooks are now in an inconsistent state, so we cannot
                // continue processing the batch.  Requeue already-pending
                // requests and remaining unprocessed requests so they get
                // fresh hooks on their next pass through the batcher.
                return pending
                    .into_iter()
                    .map(|p| p.request)
                    .chain(requests_iter)
                    .map(|mut req| {
                        req.retry_num = PushrebaseRetryNum(req.retry_num.0 + 1);
                        req
                    })
                    .collect();
            }
        }
    }

    // Save all rebased changesets from all stacks in one batch.
    if let Err(e) = changesets_creation::save_changesets(ctx, repo, all_rebased_bonsais).await {
        let shared = SharedError::from(PushrebaseError::from(e));
        for p in pending {
            let _ = p.request.response_tx.send(Err(shared.clone()));
        }
        return vec![];
    }

    // If no stacks survived conflict detection + rebase, we're done.
    let final_head = match running_head {
        Some(head) if !pending.is_empty() => head,
        _ => return vec![],
    };

    // Convert commit hooks to transaction hooks.
    let txn_hooks = match try_join_all(
        commit_hooks
            .into_iter()
            .map(|h| h.into_transaction_hook(ctx, &all_rebased_changesets)),
    )
    .await
    {
        Ok(h) => h,
        Err(e) => {
            let shared = SharedError::from(PushrebaseError::from(e));
            for p in pending {
                let _ = p.request.response_tx.send(Err(shared.clone()));
            }
            return vec![];
        }
    };

    // Single bookmark CAS update.
    let move_result = try_move_bookmark(
        ctx.clone(),
        repo,
        onto_bookmark,
        old_bookmark_value,
        final_head,
        all_rebased_changesets,
        txn_hooks,
    )
    .await;

    let critical_section_duration_us: i64 = start_critical_section
        .elapsed()
        .as_nanos()
        .try_into()
        .unwrap_or(i64::MAX);

    match move_result {
        Ok(Some((_head, log_id, all_rebased_pairs))) => {
            // CAS succeeded — build per-stack outcomes and send via oneshot.
            if emit_all_bookmarks {
                bookmarks::saturation::record_pushrebase_success(
                    repo.repo_identity().name(),
                    critical_section_duration_us,
                    None,
                    all_rebased_pairs.len() as i64,
                );
            }
            if should_log {
                STATS::critical_section_success_duration_us
                    .add_value(critical_section_duration_us, repo_args.clone());
                STATS::commits_rebased.add_value(all_rebased_pairs.len() as i64, repo_args);
            }

            let batch_size = pending.len();
            for p in pending {
                // Per-request: the batch success sample above is per-batch, so
                // record each landed request's retries-until-success separately.
                if emit_all_bookmarks {
                    bookmarks::saturation::record_pushrebase_retries(
                        repo.repo_identity().name(),
                        p.request.retry_num.0 as i64,
                    );
                }
                let stack_pairs: Vec<PushrebaseChangesetPair> = all_rebased_pairs
                    .iter()
                    .filter(|pair| {
                        p.request
                            .stack
                            .changesets
                            .iter()
                            .any(|cs| cs.get_changeset_id() == pair.id_old)
                    })
                    .cloned()
                    .collect();

                let request_ctx = pushrebase_context(&p.request.ctx, flags);
                let mut sample = request_ctx.scuba().clone();
                sample
                    .add("repo_name", repo.repo_identity().name())
                    .add("bookmark_log_id", log_id.0)
                    .add("batch_size", batch_size)
                    .add("batch_total_commits", all_rebased_pairs.len())
                    .add("rebased_changesets", stack_pairs.len())
                    .add("retry_num", p.request.retry_num.0 as i64);
                p.merge_summary.clone().add_to_scuba(&mut sample);
                sample.log_with_msg("batched_pushrebase_request_complete", None);

                let _ = p.request.response_tx.send(Ok(PushrebaseOutcome {
                    old_bookmark_value: Some(p.old_bookmark_value.unwrap_or(p.request.stack.root)),
                    head: p.new_head,
                    retry_num: p.request.retry_num,
                    rebased_changesets: stack_pairs,
                    pushrebase_distance: PushrebaseDistance(p.pushrebase_distance),
                    log_id,
                    merge_resolved_paths: p.merge_resolved_paths,
                    merge_summary: Some(p.merge_summary),
                }));
            }
            vec![]
        }
        Ok(None) => {
            // CAS failed — return for re-queue with the saved conflict checks.
            if emit_all_bookmarks {
                bookmarks::saturation::record_pushrebase_failure(
                    repo.repo_identity().name(),
                    critical_section_duration_us,
                );
            }
            if should_log {
                STATS::critical_section_failure_duration_us
                    .add_value(critical_section_duration_us, repo_args);
            }
            pending
                .into_iter()
                .map(|mut p| {
                    p.request.retry_num = PushrebaseRetryNum(p.request.retry_num.0 + 1);
                    // carried_merge_file_info is already updated on the request
                    p.request
                })
                .collect()
        }
        Err(e) => {
            let shared = SharedError::from(e);
            for p in pending {
                let _ = p.request.response_tx.send(Err(shared.clone()));
            }
            vec![]
        }
    }
}

async fn check_filenodes_backfilled(
    ctx: &CoreContext,
    repo: &impl RepoDerivedDataRef,
    head: &ChangesetId,
    limit: u64,
) -> Result<(), Error> {
    let derives_filenodes = repo
        .repo_derived_data()
        .active_config()
        .types
        .contains(&DerivableType::FileNodes);

    if !derives_filenodes {
        // Repo doesn't have filenodes derivation enabled, so no need to check
        // if they're backfilled
        return Ok(());
    }

    let underived = repo
        .repo_derived_data()
        .count_underived::<FilenodesOnlyPublic>(ctx, *head)
        .await?;
    if underived >= limit {
        Err(format_err!(
            "Too many commits do not have filenodes derived. This usually happens when \
            merging a new repo or pushing an extremely long stack.
            Contact source control @ fb if you encounter this issue."
        ))
    } else {
        Ok(())
    }
}

/// Info about a single file that was successfully merged during conflict
/// resolution. Carries the base/server content IDs so the cascading merge
/// in `create_rebased_changesets` can reuse them without re-fetching manifests.
///
/// Stored across optimistic-lock retries so the next attempt can reuse
/// resolved file contents without fetching them again.
#[derive(Clone, Debug, PartialEq)]
struct MergedFileInfo {
    path: NonRootMPath,
    /// `None`: absent in the old parent.
    base: Option<BaseFile>,
    /// `None`: absent in the new parent.
    server: Option<BaseFile>,
    /// How the cascade resolves a both-sides-changed region of this path.
    resolution: Resolution,
}

struct ConflictCheckResult {
    /// Number of server-side changesets (used for pushrebase_distance tracking).
    server_changeset_count: usize,
    /// If merge resolution succeeded, info about each merged file.
    /// `None` means no conflicts or merge resolution was not attempted.
    merged_file_overrides: Option<Vec<MergedFileInfo>>,
    /// Merge-resolution outcome for this conflict-check attempt. Always set
    /// on the Ok path: `NotNeeded` when there were no conflicts, `Succeeded`
    /// when MR resolved them. Failure-path summaries are not propagated here
    /// (the standalone "Pushrebase merge resolution failed" Scuba sample
    /// captures them); they ride with PushrebaseError in a follow-up.
    merge_summary: MergeResolutionSummary,
}

/// An overlapping path whose server content still equals the base: only the
/// stack changed it, so there is nothing to merge.
#[derive(Clone, Copy)]
enum UnchangedOverlap {
    /// Report it like any other overlap.
    Conflict,
    /// Take the stack's content; needed when the range holds the root's own
    /// landed form.
    TakeClient,
}

/// Checks for server-side conflicts against the client's pushed stack.
/// Returns conflict check results including the number of server-side changesets
/// and optionally merged file overrides if live merge resolution succeeds.
async fn check_pushrebase_conflicts(
    ctx: &CoreContext,
    repo: &impl Repo,
    config: &PushrebaseFlags,
    root: ChangesetId,
    ancestor: ChangesetId,
    descendant: ChangesetId,
    client_bcs: &[BonsaiChangeset],
    client_cf: &[MPath],
) -> Result<ConflictCheckResult, PushrebaseError> {
    check_pushrebase_conflicts_with(
        ctx,
        repo,
        config,
        root,
        ancestor,
        descendant,
        client_bcs,
        client_cf,
        RangeDiffManifests::FromKnob,
        UnchangedOverlap::Conflict,
    )
    .await
}

async fn check_pushrebase_conflicts_with(
    ctx: &CoreContext,
    repo: &impl Repo,
    config: &PushrebaseFlags,
    root: ChangesetId,
    ancestor: ChangesetId,
    descendant: ChangesetId,
    client_bcs: &[BonsaiChangeset],
    client_cf: &[MPath],
    manifests: RangeDiffManifests,
    unchanged: UnchangedOverlap,
) -> Result<ConflictCheckResult, PushrebaseError> {
    let server_bcs =
        fetch_bonsai_range_ancestor_not_included(ctx, repo, ancestor, descendant).await?;
    let server_bcs_len = server_bcs.len();

    if let Some(bcs) = server_bcs.iter().find(|bcs| should_fail_pushrebase(bcs)) {
        return Err(PushrebaseError::ForceFailPushrebase(bcs.get_changeset_id()));
    }

    // Safe with narrow ranges: if attempt 1 passed case-folding for
    // root→S1, no case conflict exists in that range. Retry only needs
    // to check S1→S2 for new case conflicts with client changesets.
    if config.casefolding_check {
        let conflict = check_case_conflicts(
            server_bcs.iter().chain(client_bcs.iter()),
            &config.casefolding_check_excluded_paths,
        );
        if let Some(conflict) = conflict {
            return Err(PushrebaseError::PotentialCaseConflict(conflict.1));
        }
    }

    let server_cf =
        find_changed_files_with(ctx, repo, ancestor, descendant, &server_bcs, manifests).await?;

    match intersect_changed_files(server_cf, client_cf.to_vec()) {
        Ok(()) => Ok(ConflictCheckResult {
            server_changeset_count: server_bcs_len,
            merged_file_overrides: None,
            merge_summary: MergeResolutionSummary::NotNeeded,
        }),
        Err(PushrebaseError::Conflicts(conflicts)) => {
            let reponame = repo.repo_identity().name();
            let (conflicts, bases) = match unchanged {
                UnchangedOverlap::Conflict => (conflicts, Bases::ProbeThenRead),
                UnchangedOverlap::TakeClient => {
                    let total = conflicts.len();
                    let (remaining, bases) = without_unchanged_overlaps(
                        ctx,
                        repo,
                        conflicts,
                        root,
                        descendant,
                        &server_bcs,
                    )
                    .await?;
                    if remaining.len() < total {
                        ctx.scuba()
                            .clone()
                            .add("repo_name", reponame)
                            .add("unchanged_overlaps_taken", (total - remaining.len()) as i64)
                            .log_with_msg("Pushrebase unchanged overlaps taken", None);
                    }
                    if remaining.is_empty() {
                        return Ok(ConflictCheckResult {
                            server_changeset_count: server_bcs_len,
                            merged_file_overrides: None,
                            merge_summary: MergeResolutionSummary::NotNeeded,
                        });
                    }
                    (remaining, Bases::Precomputed(bases))
                }
            };
            let conflict_files_count = conflicts.len() as u64;
            STATS::conflict_rejections.add_value(1, (reponame.to_string(),));
            STATS::conflict_files_count.add_value(conflicts.len() as i64, (reponame.to_string(),));

            // Per-request override wins; UseJk defers to the JK.
            let merge_enabled = match config.merge_resolution_override {
                MergeResolutionOverride::ForceOn => true,
                MergeResolutionOverride::ForceOff => false,
                MergeResolutionOverride::UseJk => justknobs::eval(
                    "scm/mononoke:pushrebase_enable_merge_resolution",
                    None,
                    Some(reponame),
                ),
            };
            let max_merge_conflicts: usize = justknobs::get_as::<usize>(
                "scm/mononoke:pushrebase_max_merge_conflicts",
                Some(reponame),
            );
            let max_merge_file_size: u64 = justknobs::get_as::<u64>(
                "scm/mononoke:pushrebase_max_merge_file_size",
                Some(reponame),
            );
            let merge_result = if merge_enabled {
                let derive_manifests: bool = justknobs::eval(
                    "scm/mononoke:pushrebase_merge_resolution_derive_fsnodes",
                    None,
                    Some(reponame),
                );
                Some(
                    collect_merge_file_info(
                        ctx,
                        repo,
                        &conflicts,
                        root,
                        bases,
                        &server_bcs,
                        client_bcs,
                        max_merge_conflicts,
                        max_merge_file_size,
                        derive_manifests,
                        &config.merge_resolution_excluded_path_prefixes,
                        take_incoming_scope(config, reponame),
                    )
                    .await,
                )
            } else {
                None
            };

            match merge_result {
                Some(Ok(merged_changes)) => {
                    let resolved_paths_sample = merged_changes
                        .iter()
                        .take(MR_PATH_SAMPLE_CAP)
                        .map(|info| info.path.clone())
                        .collect();
                    let merge_summary = MergeResolutionSummary::Succeeded {
                        conflict_files_count,
                        resolved_files_count: merged_changes.len() as u64,
                        resolved_paths_sample,
                    };
                    Ok(ConflictCheckResult {
                        server_changeset_count: server_bcs_len,
                        merged_file_overrides: Some(merged_changes),
                        merge_summary,
                    })
                }
                _ => {
                    // Failure-path summary is intentionally NOT propagated via
                    // PushrebaseError::Conflicts in this diff. The existing
                    // "Pushrebase merge resolution failed" Scuba sample captures
                    // it; a follow-up will fold the failure summary into a
                    // richer error variant once parity is verified.
                    if let Some(Err(ref err)) = merge_result {
                        ctx.scuba()
                            .clone()
                            .add("repo_name", reponame)
                            .add("merge_resolution_outcome", format!("{err}"))
                            .log_with_msg("Pushrebase merge resolution failed", None);
                    }
                    Err(PushrebaseError::Conflicts(conflicts))
                }
            }
        }
        Err(e) => Err(e),
    }
}

async fn run_batch_hooks(
    ctx: &CoreContext,
    hooks: &[Box<dyn PushrebaseHook>],
    old_bookmark_value: Option<ChangesetId>,
) -> Result<Vec<Box<dyn PushrebaseCommitHook>>, PushrebaseError> {
    try_join_all(hooks.iter().map(|h| {
        h.in_critical_section(ctx, old_bookmark_value)
            .map_err(PushrebaseError::from)
    }))
    .await
}

async fn rebase_in_loop(
    ctx: &CoreContext,
    repo: &impl Repo,
    config: &PushrebaseFlags,
    onto_bookmark: &BookmarkKey,
    stack: &PushrebaseStack,
    prepushrebase_hooks: &[Box<dyn PushrebaseHook>],
) -> Result<PushrebaseOutcome, PushrebaseError> {
    let should_log = config.monitoring_bookmark.as_deref() == Some(onto_bookmark.as_str());
    // Parallel all-bookmarks saturation counters (see do_batched_pushrebase).
    let emit_all_bookmarks = config.monitoring_bookmark.is_some();
    let mut any_attempt_resolved_conflicts = false;
    let repo_args = (repo.repo_identity().name().to_string(),);
    let mut latest_rebase_attempt = stack.root;
    let mut carried_merge_file_info: Vec<MergedFileInfo> = Vec::new();
    let mut total_pushrebase_distance: usize = 0;
    let mut accumulated_merge_summary = MergeResolutionSummary::NotNeeded;
    for retry_num in 0..MAX_REBASE_ATTEMPTS {
        let retry_num = PushrebaseRetryNum(retry_num);

        let start_critical_section = Instant::now();
        // CRITICAL SECTION START: After getting the value of the bookmark
        let old_bookmark_value = get_bookmark_value(ctx, repo, onto_bookmark).await?;
        stack
            .check_required_root(ctx, repo, onto_bookmark, old_bookmark_value)
            .await?;
        let hooks = try_join_all(prepushrebase_hooks.iter().map(|h| {
            h.in_critical_section(ctx, old_bookmark_value)
                .map_err(PushrebaseError::from)
        }))
        .await?;

        // Narrow-range scan: only check changesets since the last attempt.
        // Carried MergedFileInfo from previous attempts provides the rest.
        // Note: if a carried file is deleted or type-changed on the server
        // in the delta range, collect_merge_file_info rejects the conflict
        // and check_pushrebase_conflicts returns Err(Conflicts) before
        // reconciliation is reached, so no special handling is needed.
        let conflict_result = check_pushrebase_conflicts(
            ctx,
            repo,
            config,
            stack.root,
            latest_rebase_attempt,
            old_bookmark_value.unwrap_or(stack.root),
            &stack.changesets,
            &stack.changed_files,
        )
        .await?;
        // Accumulate total pushrebase distance across retries since each
        // narrow-range scan only covers the delta since the last attempt.
        total_pushrebase_distance += conflict_result.server_changeset_count;
        let pushrebase_distance = PushrebaseDistance(total_pushrebase_distance);
        // Accumulate the per-attempt summary so the final outcome reflects
        // any MR success across the retry chain (Succeeded is sticky).
        accumulated_merge_summary = MergeResolutionSummary::combine(
            accumulated_merge_summary,
            conflict_result.merge_summary,
        );

        // Reconcile carried info with delta info from this attempt
        let reconciled_overrides = match conflict_result.merged_file_overrides {
            Some(delta_info) => Some(reconcile_merge_file_info(
                &carried_merge_file_info,
                &delta_info,
            )),
            None if !carried_merge_file_info.is_empty() => {
                // Delta had no conflicts, but we have carried info from
                // previous attempts — use it as-is.
                Some(carried_merge_file_info.clone())
            }
            None => None,
        };

        if reconciled_overrides.is_some() {
            any_attempt_resolved_conflicts = true;
        }

        let merge_resolved_paths = reconciled_overrides
            .as_ref()
            .map(|overrides| overrides.iter().map(|info| info.path.clone()).collect());

        // INVARIANT (defense-in-depth, expected unreachable with carry-forward):
        // If any previous attempt resolved conflicts, carried_merge_file_info
        // is non-empty, so reconciled_overrides is always Some. This check
        // guards against future logic changes that might break that property.
        if any_attempt_resolved_conflicts && merge_resolved_paths.is_none() {
            STATS::merge_resolution_lost_on_retry.add_value(1, repo_args.clone());

            // Log to Scuba for oncall visibility. The ODS counter alone
            // doesn't carry enough context to investigate.
            ctx.scuba().clone()
                .add("log_tag", "MergeResolutionLostOnRetry")
                .add("repo_name", repo.repo_identity().name())
                .add("retry_num", retry_num.0 as i64)
                .add(
                    "merge_resolution_invariant_violation",
                    "any_attempt_resolved_conflicts=true but final attempt has no merge_resolved_paths",
                )
                .log();

            return Err(PushrebaseInternalError::MergeResolutionLostOnRetry.into());
        }

        let rebase_outcome = do_rebase(
            ctx,
            repo,
            config,
            stack,
            old_bookmark_value,
            onto_bookmark,
            hooks,
            reconciled_overrides.clone(),
        )
        .await?;
        // CRITICAL SECTION END: Right after writing new value of bookmark

        let critical_section_duration_us: i64 = start_critical_section
            .elapsed()
            .as_nanos()
            .try_into()
            .unwrap_or(i64::MAX);
        if let Some((head, log_id, rebased_changesets)) = rebase_outcome {
            if emit_all_bookmarks {
                bookmarks::saturation::record_pushrebase_success(
                    repo.repo_identity().name(),
                    critical_section_duration_us,
                    Some(retry_num.0 as i64),
                    rebased_changesets.len() as i64,
                );
            }
            if should_log {
                STATS::critical_section_success_duration_us
                    .add_value(critical_section_duration_us, repo_args.clone());
                STATS::critical_section_retries_failed
                    .add_value(retry_num.0 as i64, repo_args.clone());
                STATS::commits_rebased
                    .add_value(rebased_changesets.len() as i64, repo_args.clone());
            }
            let merge_summary_for_scuba = accumulated_merge_summary.clone();
            let mut sample = ctx.scuba().clone();
            sample
                .add("repo_name", repo.repo_identity().name())
                .add("retry_num", retry_num.0 as i64)
                .add("rebased_changesets", rebased_changesets.len() as i64);
            merge_summary_for_scuba.add_to_scuba(&mut sample);
            sample.log_with_msg("pushrebase_complete", None);

            let res = PushrebaseOutcome {
                old_bookmark_value: Some(old_bookmark_value.unwrap_or(stack.root)),
                head,
                retry_num,
                rebased_changesets,
                pushrebase_distance,
                log_id,
                merge_resolved_paths,
                merge_summary: Some(accumulated_merge_summary),
            };
            return Ok(res);
        } else {
            // CAS failed — carry forward merge info for next attempt
            carried_merge_file_info = reconciled_overrides.unwrap_or_default();
            latest_rebase_attempt = old_bookmark_value.unwrap_or(stack.root);
            if emit_all_bookmarks {
                bookmarks::saturation::record_pushrebase_failure(
                    repo.repo_identity().name(),
                    critical_section_duration_us,
                );
            }
            if should_log {
                STATS::critical_section_failure_duration_us
                    .add_value(critical_section_duration_us, repo_args.clone());
            }
        }
    }
    if emit_all_bookmarks {
        bookmarks::saturation::record_pushrebase_retries(
            repo.repo_identity().name(),
            MAX_REBASE_ATTEMPTS as i64,
        );
    }
    if should_log {
        STATS::critical_section_retries_failed.add_value(MAX_REBASE_ATTEMPTS as i64, repo_args);
    }

    Err(PushrebaseInternalError::TooManyRebaseAttempts.into())
}

fn should_fail_pushrebase(bcs: &BonsaiChangeset) -> bool {
    bcs.hg_extra().any(|(key, _)| key == FAIL_PUSHREBASE_EXTRA)
}

async fn do_rebase(
    ctx: &CoreContext,
    repo: &impl Repo,
    config: &PushrebaseFlags,
    stack: &PushrebaseStack,
    old_bookmark_value: Option<ChangesetId>,
    onto_bookmark: &BookmarkKey,
    mut hooks: Vec<Box<dyn PushrebaseCommitHook>>,
    merged_file_overrides: Option<Vec<MergedFileInfo>>,
) -> Result<
    Option<(
        ChangesetId,
        BookmarkUpdateLogId,
        Vec<PushrebaseChangesetPair>,
    )>,
    PushrebaseError,
> {
    let (new_head, rebased_changesets, rebased_bonsais) = create_rebased_changesets(
        ctx,
        repo,
        config,
        &stack.changesets,
        stack.root,
        stack.head,
        old_bookmark_value.unwrap_or(stack.root),
        &mut hooks,
        merged_file_overrides,
    )
    .await?;

    changesets_creation::save_changesets(ctx, repo, rebased_bonsais).await?;

    let hooks = try_join_all(
        hooks
            .into_iter()
            .map(|h| h.into_transaction_hook(ctx, &rebased_changesets)),
    )
    .await?;

    try_move_bookmark(
        ctx.clone(),
        repo,
        onto_bookmark,
        old_bookmark_value,
        new_head,
        rebased_changesets,
        hooks,
    )
    .await
}

// There should only be one head in the pushed set
fn find_only_head_or_fail(
    commits: &HashSet<BonsaiChangeset>,
) -> Result<ChangesetId, PushrebaseError> {
    let mut commits_set: HashSet<_> =
        HashSet::from_iter(commits.iter().map(|commit| commit.get_changeset_id()));
    for commit in commits {
        for p in commit.parents() {
            commits_set.remove(&p);
        }
    }
    if commits_set.len() == 1 {
        Ok(commits_set.iter().next().unwrap().clone())
    } else {
        Err(PushrebaseError::Error(
            PushrebaseInternalError::PushrebaseTooManyHeads.into(),
        ))
    }
}

/// Represents index of current child with regards to its parent
#[derive(Clone, Copy, PartialEq, Eq)]
struct ChildIndex(usize);

fn find_roots(commits: &HashSet<BonsaiChangeset>) -> HashMap<ChangesetId, ChildIndex> {
    let commits_set: HashSet<_> =
        HashSet::from_iter(commits.iter().map(|commit| commit.get_changeset_id()));
    let mut roots = HashMap::new();
    for commit in commits {
        for (index, parent) in commit.parents().enumerate() {
            if !commits_set.contains(&parent) {
                let ChildIndex(max_index) = roots.entry(parent.clone()).or_insert(ChildIndex(0));
                *max_index = max(index, *max_index);
            }
        }
    }
    roots
}

async fn find_closest_root(
    ctx: &CoreContext,
    repo: &impl Repo,
    config: &PushrebaseFlags,
    bookmark: &BookmarkKey,
    roots: &HashMap<ChangesetId, ChildIndex>,
) -> Result<ChangesetId, PushrebaseError> {
    let maybe_id = get_bookmark_value(ctx, repo, bookmark).await?;

    if let Some(id) = maybe_id {
        return find_closest_ancestor_root(ctx, repo, config, bookmark, roots, id).await;
    }

    let roots = repo
        .commit_graph()
        .many_changeset_generations(ctx, &roots.keys().copied().collect::<Vec<_>>())
        .await?;

    let (cs_id, _) = roots
        .into_iter()
        .max_by_key(|(_, gen_num)| gen_num.clone())
        .ok_or_else(|| PushrebaseError::from(PushrebaseInternalError::NoRoots))?;

    Ok(cs_id)
}

async fn find_closest_ancestor_root(
    ctx: &CoreContext,
    repo: &impl Repo,
    config: &PushrebaseFlags,
    bookmark: &BookmarkKey,
    roots: &HashMap<ChangesetId, ChildIndex>,
    onto_bookmark_cs_id: ChangesetId,
) -> Result<ChangesetId, PushrebaseError> {
    let id = repo
        .commit_graph()
        .filter_ancestors(ctx, onto_bookmark_cs_id, roots.keys().copied().collect())
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| {
            PushrebaseError::Error(
                PushrebaseInternalError::PushrebaseNoCommonRoot(
                    bookmark.clone(),
                    roots.keys().copied().collect(),
                )
                .into(),
            )
        })?;
    let (bookmark_generation, root_generation) = futures::try_join!(
        repo.commit_graph()
            .changeset_generation(ctx, onto_bookmark_cs_id),
        repo.commit_graph().changeset_generation(ctx, id),
    )?;
    let distance = bookmark_generation
        .value()
        .saturating_sub(root_generation.value());
    if config
        .recursion_limit
        .is_some_and(|limit| distance >= u64::try_from(limit).unwrap_or(u64::MAX))
    {
        return Err(PushrebaseError::RootTooFarBehind);
    }

    let index = roots
        .get(&id)
        .expect("closest root should be one of the candidate roots");
    if config.forbid_p2_root_rebases && !config.mainline_parent_first && *index != ChildIndex(0) {
        ctx.scuba().clone().log_with_msg(
            "pushrebase_p2_root_rejected",
            Some(format!(
                "root={}, bookmark={}, depth={}, child_index={}",
                id, bookmark, distance, index.0,
            )),
        );

        let hgcs = repo.derive_hg_changeset(ctx, id).await?;
        return Err(PushrebaseError::Error(
            PushrebaseInternalError::P2RootRebaseForbidden(hgcs, bookmark.clone()).into(),
        ));
    }

    Ok(id)
}

/// Backing manifests for the range diff of a merge commit whose other
/// parent lies outside the range.
#[derive(Copy, Clone)]
enum RangeDiffManifests {
    ContentManifest,
    /// HG historically; the knob rolls repos onto the content manifests.
    /// Resolved lazily so the knob is read only when a merge is in range.
    FromKnob,
}

async fn find_changed_files_between_manifests(
    ctx: &CoreContext,
    repo: &impl Repo,
    ancestor: ChangesetId,
    descendant: ChangesetId,
) -> Result<Vec<MPath>, PushrebaseError> {
    let paths = find_bonsai_diff(ctx, repo, ancestor, descendant)
        .await?
        .map_ok(|diff| MPath::from(diff.into_path()))
        .try_collect()
        .await?;

    Ok(paths)
}

async fn find_changed_files_between_root_manifests(
    ctx: &CoreContext,
    repo: &impl Repo,
    ancestor: ChangesetId,
    descendant: ChangesetId,
) -> Result<Vec<MPath>, PushrebaseError> {
    let (d_mf, a_mf) = try_join(
        id_to_root_manifest_id(ctx, repo, descendant),
        id_to_root_manifest_id(ctx, repo, ancestor),
    )
    .await?;

    let paths = bonsai_diff(
        ctx.clone(),
        repo.repo_blobstore().clone(),
        d_mf,
        Some(a_mf).into_iter().collect(),
    )
    .map_ok(|diff| MPath::from(diff.into_path()))
    .try_collect()
    .await?;

    Ok(paths)
}

pub async fn find_bonsai_diff<T: Repo>(
    ctx: &CoreContext,
    repo: &T,
    ancestor: ChangesetId,
    descendant: ChangesetId,
) -> Result<
    impl TryStream<Ok = BonsaiDiffFileChange<(FileType, HgFileNodeId)>, Error = Error> + use<T>,
> {
    let (d_mf, a_mf) = try_join(
        id_to_manifestid(ctx, repo, descendant),
        id_to_manifestid(ctx, repo, ancestor),
    )
    .await?;

    Ok(hg_manifest_bonsai_diff(
        ctx,
        repo.repo_blobstore().clone(),
        repo.repo_identity().name(),
        d_mf,
        Some(a_mf).into_iter().collect(),
    ))
}

async fn id_to_manifestid(
    ctx: &CoreContext,
    repo: &impl Repo,
    bcs_id: ChangesetId,
) -> Result<HgManifestId, Error> {
    let hg_cs_id = repo.derive_hg_changeset(ctx, bcs_id).await?;
    let hg_cs = hg_cs_id.load(ctx, repo.repo_blobstore()).await?;
    Ok(hg_cs.manifestid())
}

async fn id_to_root_manifest_id(
    ctx: &CoreContext,
    repo: &impl Repo,
    bcs_id: ChangesetId,
) -> Result<ContentManifestId, Error> {
    Ok(repo
        .repo_derived_data()
        .derive::<RootContentManifestId>(ctx, bcs_id, DerivationPriority::HIGH)
        .await?
        .into_content_manifest_id())
}

/// `id_to_root_manifest_id` without derivation: `None` if not derived yet.
async fn fetch_root_manifest_id(
    ctx: &CoreContext,
    repo: &impl Repo,
    bcs_id: ChangesetId,
) -> Result<Option<ContentManifestId>, Error> {
    Ok(repo
        .repo_derived_data()
        .fetch_derived::<RootContentManifestId>(ctx, bcs_id)
        .await?
        .map(RootContentManifestId::into_content_manifest_id))
}

// from smaller generation number to larger
async fn fetch_bonsai_range_ancestor_not_included(
    ctx: &CoreContext,
    repo: &impl Repo,
    ancestor: ChangesetId,
    descendant: ChangesetId,
) -> Result<Vec<BonsaiChangeset>, PushrebaseError> {
    Ok(repo
        .commit_graph()
        .range_stream(ctx, ancestor, descendant)
        .await?
        .filter(|cs_id| future::ready(cs_id != &ancestor))
        .map(async |res| Result::<_, Error>::Ok(res.load(ctx, repo.repo_blobstore()).await?))
        .buffered(100)
        .try_collect::<Vec<_>>()
        .await?)
}

#[cfg(test)]
async fn find_changed_files(
    ctx: &CoreContext,
    repo: &impl Repo,
    ancestor: ChangesetId,
    descendant: ChangesetId,
) -> Result<Vec<MPath>, PushrebaseError> {
    let changesets =
        fetch_bonsai_range_ancestor_not_included(ctx, repo, ancestor, descendant).await?;
    find_changed_files_with(
        ctx,
        repo,
        ancestor,
        descendant,
        &changesets,
        RangeDiffManifests::FromKnob,
    )
    .await
}

async fn find_changed_files_with(
    ctx: &CoreContext,
    repo: &impl Repo,
    ancestor: ChangesetId,
    descendant: ChangesetId,
    changesets: &[BonsaiChangeset],
    manifests: RangeDiffManifests,
) -> Result<Vec<MPath>, PushrebaseError> {
    let ids: HashSet<_> = std::iter::once(ancestor)
        .chain(changesets.iter().map(BonsaiChangeset::get_changeset_id))
        .collect();
    let use_content_manifests = match manifests {
        RangeDiffManifests::ContentManifest => true,
        RangeDiffManifests::FromKnob => justknobs::eval(
            "scm/mononoke:pushrebase_range_diff_use_content_manifests",
            None,
            Some(repo.repo_identity().name()),
        ),
    };

    let file_changes =
        try_join_all(changesets.iter().map(|bcs| async {
            let id = bcs.get_changeset_id();
            let parents: Vec<_> = bcs.parents().collect();
            match *parents {
                [] | [_] => Ok(extract_conflict_files_from_bonsai_changeset(bcs)),
                [p0, p1] => match (ids.get(&p0), ids.get(&p1)) {
                    (Some(_), Some(_)) => Ok(extract_conflict_files_from_bonsai_changeset(bcs)),
                    (Some(parent), None) | (None, Some(parent)) => {
                        if use_content_manifests {
                            find_changed_files_between_root_manifests(ctx, repo, id, *parent).await
                        } else {
                            find_changed_files_between_manifests(ctx, repo, id, *parent).await
                        }
                    }
                    (None, None) => panic!(
                        "`range_stream` produced invalid result for: ({descendant}, {ancestor})",
                    ),
                },
                _ => panic!("pushrebase supports only two parents"),
            }
        }))
        .await?;

    let mut changed_files = file_changes
        .into_iter()
        .flatten()
        .chain(find_subtree_changes(changesets)?)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    changed_files.sort_unstable();

    Ok(changed_files)
}

fn extract_conflict_files_from_bonsai_changeset(bcs: &BonsaiChangeset) -> Vec<MPath> {
    bcs.file_changes()
        .flat_map(|(path, file_change)| {
            let mut v = vec![];
            if let Some((copy_from_path, _)) = file_change.copy_from() {
                v.push(MPath::from(copy_from_path.clone()));
            }
            v.push(MPath::from(path.clone()));
            v.into_iter()
        })
        .collect::<Vec<MPath>>()
}

fn find_subtree_changes(changesets: &[BonsaiChangeset]) -> Result<Vec<MPath>, PushrebaseError> {
    let cs_ids = changesets
        .iter()
        .map(|bcs| bcs.get_changeset_id())
        .collect::<HashSet<_>>();

    let mut paths = Vec::new();
    for bcs in changesets {
        for (path, change) in bcs.subtree_changes() {
            paths.push(path.clone());
            if let Some((from_csid, from_path)) = change.change_source() {
                if cs_ids.contains(&from_csid) {
                    // This change is copying from the rebase set, so its
                    // origin will be updated as part of the pushrebase.
                    // This means we must make the source has not changed
                    // since the root.
                    paths.push(from_path.clone());
                }
            }
        }
    }
    Ok(paths)
}

/// `left` and `right` are considerered to be conflict free, if none of the element from `left`
/// is prefix of element from `right`, and vice versa.
fn intersect_changed_files(left: Vec<MPath>, right: Vec<MPath>) -> Result<(), PushrebaseError> {
    let conflicts: Vec<PushrebaseConflict> = find_path_conflicts(left, right)
        .into_iter()
        .map(|(l, r)| PushrebaseConflict::new(l, r))
        .collect();

    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(PushrebaseError::Conflicts(conflicts))
    }
}

/// Whether the root manifest `base_files` would read for `cs_id` is already
/// derived, so merge resolution can run without paying for derivation inside
/// the pushrebase critical section.
async fn root_manifest_is_derived(
    ctx: &CoreContext,
    repo: &impl Repo,
    cs_id: ChangesetId,
) -> Result<bool> {
    Ok(fetch_root_manifest_id(ctx, repo, cs_id).await?.is_some())
}

/// What the stack saw at a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BaseFile {
    pub(crate) content_id: ContentId,
    pub(crate) file_type: FileType,
}

/// Base content for `paths` from `root`'s manifest, `None` meaning absent,
/// in one batched lookup. Derives the manifest if it is not there yet.
async fn base_files(
    ctx: &CoreContext,
    repo: &impl Repo,
    root: ChangesetId,
    paths: &[NonRootMPath],
) -> Result<HashMap<NonRootMPath, Option<BaseFile>>> {
    let root_id = id_to_root_manifest_id(ctx, repo, root).await?;
    base_files_from_manifest(ctx, repo, root_id, paths).await
}

/// `base_files` for an already-resolved manifest; never derives.
async fn base_files_from_manifest(
    ctx: &CoreContext,
    repo: &impl Repo,
    root_id: ContentManifestId,
    paths: &[NonRootMPath],
) -> Result<HashMap<NonRootMPath, Option<BaseFile>>> {
    use manifest::Entry;

    let found: HashMap<MPath, _> = root_id
        .find_entries(
            ctx.clone(),
            repo.repo_blobstore().clone(),
            paths.iter().cloned(),
        )
        .try_collect()
        .await?;
    Ok(paths
        .iter()
        .map(|path| {
            let base = match found.get(&MPath::from(path.clone())) {
                Some(Entry::Leaf(file)) => Some(BaseFile {
                    content_id: file.content_id,
                    file_type: file.file_type,
                }),
                _ => None,
            };
            (path.clone(), base)
        })
        .collect())
}

/// What a manifest holds at a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManifestState {
    File { file: BaseFile, size: u64 },
    Tree(ContentManifestId),
    Absent,
}

impl ManifestState {
    fn is_file(self) -> bool {
        matches!(self, ManifestState::File { .. })
    }
}

/// State of every path in `paths` and of each of its non-root ancestors,
/// in one manifest walk; never derives.
async fn lookup_manifest_states(
    ctx: &CoreContext,
    repo: &impl Repo,
    root_id: ContentManifestId,
    paths: &[MPath],
) -> Result<HashMap<MPath, ManifestState>> {
    use manifest::Entry;

    let wanted: HashSet<MPath> = paths
        .iter()
        .flat_map(|path| path.clone().into_ancestors())
        .filter(|path| !path.is_root())
        .collect();
    let found: HashMap<MPath, ManifestState> = root_id
        .find_entries(
            ctx.clone(),
            repo.repo_blobstore().clone(),
            wanted.iter().cloned(),
        )
        .map_ok(|(path, entry)| {
            let state = match entry {
                Entry::Leaf(file) => ManifestState::File {
                    file: BaseFile {
                        content_id: file.content_id,
                        file_type: file.file_type,
                    },
                    size: file.size,
                },
                Entry::Tree(id) => ManifestState::Tree(id),
            };
            (path, state)
        })
        .try_collect()
        .await?;
    Ok(wanted
        .into_iter()
        .map(|path| {
            let state = found.get(&path).copied().unwrap_or(ManifestState::Absent);
            (path, state)
        })
        .collect())
}

/// Server-side list for `intersect_changed_files` when the server side is
/// the difference between two manifests over the stack's own paths: the
/// path when its entry differs, else its shallowest ancestor that is a file
/// on one side only.
fn manifest_overlaps(
    client_cf: &[MPath],
    base: &HashMap<MPath, ManifestState>,
    onto: &HashMap<MPath, ManifestState>,
) -> Vec<MPath> {
    let state = |map: &HashMap<MPath, ManifestState>, path: &MPath| {
        map.get(path).copied().unwrap_or(ManifestState::Absent)
    };
    let mut overlaps: Vec<MPath> = client_cf
        .iter()
        .filter_map(|path| {
            let mut ancestors: Vec<MPath> = path
                .clone()
                .into_ancestors()
                .filter(|p| !p.is_root())
                .collect();
            ancestors.reverse();
            ancestors.into_iter().find(|p| {
                let (b, o) = (state(base, p), state(onto, p));
                if p == path {
                    b != o
                } else {
                    b.is_file() != o.is_file()
                }
            })
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    overlaps.sort_unstable();
    overlaps
}

/// Manifest-mode counterpart of `collect_merge_file_info`. The cascade
/// decides per commit whether an overlap is already applied, mergeable, or
/// a conflict; this rejects only what can never merge. An overlap whose
/// first change in the stack already matches `onto` needs no merge.
fn collect_overlap_states(
    conflicts: &[PushrebaseConflict],
    client_bcs: &[BonsaiChangeset],
    base: &HashMap<MPath, ManifestState>,
    onto: &HashMap<MPath, ManifestState>,
    max_conflicts: usize,
    max_file_size: u64,
    excluded_path_prefixes: &PrefixTrie,
    take_incoming: Option<TakeIncomingScope<'_>>,
) -> Result<Vec<MergedFileInfo>, MergeResolutionError> {
    if conflicts.iter().any(|c| c.left != c.right) {
        return Err(MergeResolutionError::Skipped(
            "prefix conflicts present".to_string(),
        ));
    }
    if conflicts.len() > max_conflicts {
        return Err(MergeResolutionError::TooManyConflicts);
    }

    let state = |map: &HashMap<MPath, ManifestState>, path: &MPath| {
        map.get(path).copied().unwrap_or(ManifestState::Absent)
    };
    conflicts
        .iter()
        .map(|conflict| {
            let path = conflict
                .left
                .clone()
                .into_optional_non_root_path()
                .ok_or_else(|| MergeResolutionError::Skipped("root path conflict".to_string()))?;
            if excluded_path_prefixes.contains_prefix(&path) {
                return Err(MergeResolutionError::Skipped(format!(
                    "file {path} is under an excluded path prefix",
                )));
            }
            let as_file = |state: ManifestState| match state {
                ManifestState::File { file, size } => Ok(Some((file, size))),
                ManifestState::Absent => Ok(None),
                ManifestState::Tree(_) => Err(MergeResolutionError::Skipped(format!(
                    "path {path} is a directory in base or destination",
                ))),
            };
            let base_entry = as_file(state(base, &conflict.left))?;
            let onto_entry = as_file(state(onto, &conflict.left))?;
            let base_state = base_entry.map(|(file, _)| file);
            let onto_state = onto_entry.map(|(file, _)| file);

            let changes: Vec<&FileChange> = client_bcs
                .iter()
                .filter_map(|bcs| bcs.file_changes_map().get(&path))
                .collect();
            if changes.is_empty() {
                return Err(MergeResolutionError::Skipped(format!(
                    "file {path} not a tracked change in pushed changeset",
                )));
            }
            // Taking the pushed side needs the on-branch range to rule out
            // content that already landed and was superseded; this mode has
            // only the two manifests, so a path that would be taken verbatim
            // keeps today's outcome under a reason of its own, and a text
            // path merges as usual.
            if take_incoming.is_some_and(|scope| scope.covers(&path))
                && changes
                    .iter()
                    .all(|fc| matches!(fc, FileChange::Change(tc) if tc.git_lfs().is_lfs_pointer()))
            {
                return Err(MergeResolutionError::Skipped(format!(
                    "file {path} would take the pushed side but the baseline is off-branch",
                )));
            }
            // Safe to skip the merge guards below: the cascade equalises both
            // parent states at the first touch, so no later commit merges here.
            let first_applied = changes.first().is_some_and(|fc| match fc {
                FileChange::Change(tc) => onto_state.is_some_and(|onto| {
                    onto.content_id == tc.content_id() && onto.file_type == tc.file_type()
                }),
                FileChange::Deletion => onto_state.is_none(),
                FileChange::UntrackedChange(_) | FileChange::UntrackedDeletion => false,
            });
            if !first_applied {
                let ((base_file, base_size), (_, onto_size)) = match (base_entry, onto_entry) {
                    (Some(base), Some(onto)) => (base, onto),
                    _ => {
                        return Err(MergeResolutionError::Skipped(format!(
                            "file {path} missing in base or destination",
                        )));
                    }
                };
                if onto_size > max_file_size || base_size > max_file_size {
                    return Err(MergeResolutionError::Skipped(format!(
                        "file {path} is too large in base or destination",
                    )));
                }
                for fc in &changes {
                    let tc = match fc {
                        FileChange::Change(tc) => tc,
                        _ => {
                            return Err(MergeResolutionError::Skipped(format!(
                                "file {path} not a tracked change in pushed changeset",
                            )));
                        }
                    };
                    if tc.copy_from().is_some() {
                        return Err(MergeResolutionError::Skipped(format!(
                            "file {path} has copy-from info",
                        )));
                    }
                    if tc.git_lfs().is_lfs_pointer() {
                        return Err(MergeResolutionError::Skipped(format!(
                            "file {path} is LFS-tracked",
                        )));
                    }
                    if tc.size() > max_file_size {
                        return Err(MergeResolutionError::Skipped(format!(
                            "file {path} is too large ({} bytes)",
                            tc.size(),
                        )));
                    }
                    if tc.file_type() != base_file.file_type
                        || onto_state.is_some_and(|onto| onto.file_type != tc.file_type())
                    {
                        return Err(MergeResolutionError::Skipped(format!(
                            "file {path} has type mismatch",
                        )));
                    }
                }
            }
            Ok(MergedFileInfo {
                path,
                base: base_state,
                server: onto_state,
                resolution: Resolution::Merge,
            })
        })
        .collect()
}

/// Drops overlaps whose server state still equals the base: only the stack
/// changed those, so taking its content is plain rebase, not a merge, and no
/// merge guard applies. Also returns the bases so merge resolution can reuse
/// them. Reads manifests on demand.
async fn without_unchanged_overlaps(
    ctx: &CoreContext,
    repo: &impl Repo,
    conflicts: Vec<PushrebaseConflict>,
    root: ChangesetId,
    onto: ChangesetId,
    server_bcs: &[BonsaiChangeset],
) -> Result<
    (
        Vec<PushrebaseConflict>,
        HashMap<NonRootMPath, Option<BaseFile>>,
    ),
    PushrebaseError,
> {
    let paths: Vec<NonRootMPath> = conflicts
        .iter()
        .filter(|c| c.left == c.right)
        .filter_map(|c| c.left.clone().into_optional_non_root_path())
        .collect();
    let bases = base_files(ctx, repo, root, &paths)
        .await
        .map_err(PushrebaseError::Error)?;
    // A merge's bonsai omits paths it kept from a single parent, so only a
    // linear range can be replayed from file changes; otherwise read `onto`.
    let server_state: HashMap<NonRootMPath, Option<BaseFile>> =
        if server_bcs.iter().any(BonsaiChangeset::is_merge) {
            base_files(ctx, repo, onto, &paths)
                .await
                .map_err(PushrebaseError::Error)?
        } else {
            // Latest wins: server_bcs is oldest-to-newest.
            server_bcs
                .iter()
                .flat_map(|bcs| bcs.file_changes_map().iter())
                .map(|(path, fc)| {
                    let state = match fc {
                        FileChange::Change(tc) => Some(BaseFile {
                            content_id: tc.content_id(),
                            file_type: tc.file_type(),
                        }),
                        // Snapshot-only variants never reach a landed range.
                        FileChange::Deletion
                        | FileChange::UntrackedChange(_)
                        | FileChange::UntrackedDeletion => None,
                    };
                    (path.clone(), state)
                })
                .collect()
        };

    let unchanged: HashSet<MPath> = paths
        .into_iter()
        .filter(|path| match (bases.get(path), server_state.get(path)) {
            (Some(Some(base)), Some(Some(server))) => {
                base.content_id == server.content_id && base.file_type == server.file_type
            }
            (Some(None), Some(None)) => true,
            _ => false,
        })
        .map(MPath::from)
        .collect();

    let remaining = conflicts
        .into_iter()
        .filter(|c| !(c.left == c.right && unchanged.contains(&c.left)))
        .collect();
    Ok((remaining, bases))
}

/// Where merge resolution gets the base content it needs.
enum Bases {
    /// Already read; no derivation probe needed.
    Precomputed(HashMap<NonRootMPath, Option<BaseFile>>),
    /// Not read yet: probe for derived manifests first, then read.
    ProbeThenRead,
}

/// Outcome of attempting a three-way merge on a single file.
enum FileMergeOutcome {
    /// Successfully merged content.
    Clean(Bytes),
    /// True content conflict.
    Conflict(String),
    /// Internal error during fetch.
    Error(anyhow::Error),
}

/// 3-way merge using three ContentIds directly (no manifest lookup).
///
/// Used by the cascading merge in the rebase loop, where the base content
/// comes from a tracked map rather than a manifest.
async fn merge_file_by_content_ids(
    ctx: &CoreContext,
    repo: &impl Repo,
    path: &NonRootMPath,
    base_content_id: ContentId,
    local_content_id: ContentId,
    other_content_id: ContentId,
    prefer_incoming: bool,
) -> FileMergeOutcome {
    let (base_bytes, local_bytes, other_bytes) = futures::join!(
        filestore::fetch_concat(repo.repo_blobstore(), ctx, base_content_id),
        filestore::fetch_concat(repo.repo_blobstore(), ctx, local_content_id),
        filestore::fetch_concat(repo.repo_blobstore(), ctx, other_content_id),
    );

    let (base, local, other) = match (base_bytes, local_bytes, other_bytes) {
        (Ok(base), Ok(local), Ok(other)) => (base, local, other),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return FileMergeOutcome::Error(e),
    };

    // A clean merge is a clean merge whatever the path; the pushed side is
    // preferred only where the ordinary merge conflicts.
    let merged = match merge_text(&base, &local, &other) {
        MergeResult::Conflict(_) if prefer_incoming => {
            merge_text_prefer_local(&base, &local, &other)
        }
        merged => merged,
    };
    match merged {
        MergeResult::Clean(merged) => FileMergeOutcome::Clean(Bytes::from(merged)),
        MergeResult::Conflict(desc) => FileMergeOutcome::Conflict(format!("file {path}: {desc}")),
    }
}

/// Error type for merge resolution failures.
#[derive(Debug, Error)]
enum MergeResolutionError {
    /// A file is missing in one or more versions, or has copy info, or type mismatch.
    #[error("skipped: {0}")]
    Skipped(String),
    /// Too many conflicting files to attempt merge.
    #[error("too many conflicting files")]
    TooManyConflicts,
    /// Internal error fetching file content.
    #[error(transparent)]
    InternalError(Error),
}

/// Collect file metadata needed for cascading merge resolution.
///
/// For each conflicting path, validates that the conflict is an exact path
/// match (not a prefix conflict), checks file types, sizes, and copy info,
/// then fetches the base content ID from ContentManifests. Returns a list of
/// `MergedFileInfo` structs containing the path, base content ID, server
/// content ID, and file type. The actual 3-way merge is deferred to the
/// per-commit rebase loop in `create_rebased_changesets`.
///
/// The server-side content is obtained directly from the bonsai changesets
/// rather than deriving manifests, to avoid expensive derivation in the
/// critical section of pushrebase.
///
/// Fails if any file cannot be merged (missing, type mismatch, copy info,
/// too large, or prefix conflict).
async fn collect_merge_file_info(
    ctx: &CoreContext,
    repo: &impl Repo,
    conflicts: &[PushrebaseConflict],
    root: ChangesetId,
    bases: Bases,
    server_bcs: &[BonsaiChangeset],
    client_bcs: &[BonsaiChangeset],
    max_conflicts: usize,
    max_file_size: u64,
    derive_manifests: bool,
    excluded_path_prefixes: &PrefixTrie,
    take_incoming: Option<TakeIncomingScope<'_>>,
) -> Result<Vec<MergedFileInfo>, MergeResolutionError> {
    // Only handle exact path matches (not prefix conflicts like dir vs dir/file)
    let exact_conflicts: Vec<_> = conflicts.iter().filter(|c| c.left == c.right).collect();

    // If there are prefix conflicts, we can't merge those
    if exact_conflicts.len() != conflicts.len() {
        return Err(MergeResolutionError::Skipped(
            "prefix conflicts present".to_string(),
        ));
    }

    if exact_conflicts.len() > max_conflicts {
        return Err(MergeResolutionError::TooManyConflicts);
    }

    // If derive_manifests is false, check if the root manifest is already
    // derived. If not, skip merge resolution to avoid expensive derivation in
    // the pushrebase critical section.
    let bases = match bases {
        Bases::Precomputed(bases) => bases,
        Bases::ProbeThenRead => {
            if !derive_manifests {
                let is_derived = root_manifest_is_derived(ctx, repo, root)
                    .await
                    .map_err(MergeResolutionError::InternalError)?;
                if !is_derived {
                    return Err(MergeResolutionError::Skipped(
                        "root manifest not derived for base commit".to_string(),
                    ));
                }
            }
            let base_paths: Vec<NonRootMPath> = exact_conflicts
                .iter()
                .filter_map(|c| c.left.clone().into_optional_non_root_path())
                .collect();
            base_files(ctx, repo, root, &base_paths)
                .await
                .map_err(MergeResolutionError::InternalError)?
        }
    };

    // Build a map of path -> FileChange from the client changesets
    let client_changes: HashMap<&NonRootMPath, &FileChange> = client_bcs
        .iter()
        .flat_map(|bcs| bcs.file_changes_map().iter())
        .collect();

    // Build a map of path -> FileChange from the server changesets
    // (latest-wins semantics since server_bcs is oldest-to-newest)
    let server_changes: HashMap<&NonRootMPath, &FileChange> = server_bcs
        .iter()
        .flat_map(|bcs| bcs.file_changes_map().iter())
        .collect();

    let mut merged_file_changes = Vec::new();

    for conflict in &exact_conflicts {
        let path = &conflict.left;
        let non_root_path = match path.clone().into_optional_non_root_path() {
            Some(nrp) => nrp,
            None => {
                return Err(MergeResolutionError::Skipped(
                    "root path conflict".to_string(),
                ));
            }
        };

        if excluded_path_prefixes.contains_prefix(&non_root_path) {
            return Err(MergeResolutionError::Skipped(format!(
                "file {non_root_path} is under an excluded path prefix",
            )));
        }

        // Get the client file change for this path
        let client_fc = match client_changes.get(&non_root_path) {
            Some(FileChange::Change(tc)) => tc,
            _ => {
                return Err(MergeResolutionError::Skipped(format!(
                    "file {path} not a tracked change in pushed changeset",
                )));
            }
        };

        // Skip files with copy info
        if client_fc.copy_from().is_some() {
            return Err(MergeResolutionError::Skipped(format!(
                "file {path} has copy-from info",
            )));
        }

        // Get server (bookmark head) content from the server bonsai changesets
        let server_fc = match server_changes.get(&non_root_path) {
            Some(FileChange::Change(tc)) => tc,
            _ => {
                return Err(MergeResolutionError::Skipped(format!(
                    "file {path} not a tracked change in bookmark head",
                )));
            }
        };

        let resolution = Resolution::for_path(
            take_incoming,
            &non_root_path,
            client_fc.git_lfs().is_lfs_pointer(),
        );

        // Both sides landed the same bytes: nothing to merge, so the LFS and
        // size limits below do not apply. The rebase loop sees it as already
        // applied.
        if client_fc.content_id() == server_fc.content_id()
            && client_fc.file_type() == server_fc.file_type()
        {
            merged_file_changes.push(MergedFileInfo {
                path: non_root_path.clone(),
                base: bases.get(&non_root_path).copied().flatten(),
                server: Some(BaseFile {
                    content_id: server_fc.content_id().clone(),
                    file_type: server_fc.file_type(),
                }),
                resolution,
            });
            continue;
        }

        // The pushed content already reached the destination once and has
        // since been superseded there (a replay of a landed stack, or a push
        // retry after a lost response). Taking it would re-land stale
        // content over newer; reject, as the plain merge does.
        if resolution.takes_incoming()
            && server_bcs.iter().any(|bcs| {
                matches!(
                    bcs.file_changes_map().get(&non_root_path),
                    Some(FileChange::Change(tc)) if tc.content_id() == client_fc.content_id()
                )
            })
        {
            return Err(MergeResolutionError::Skipped(format!(
                "file {path} was already landed and superseded in the destination",
            )));
        }

        // An LFS pointer taken verbatim is never fetched, so neither the LFS
        // restriction nor the merge size limit applies to it.
        if resolution != Resolution::IncomingVerbatim {
            // An LFS-tracked file's bonsai content is the blob behind the pointer;
            // merged bytes would exist on no LFS server.
            if client_fc.git_lfs().is_lfs_pointer() {
                return Err(MergeResolutionError::Skipped(format!(
                    "file {path} is LFS-tracked",
                )));
            }

            // Skip files that are too large
            if client_fc.size() > max_file_size {
                return Err(MergeResolutionError::Skipped(format!(
                    "file {} is too large ({} bytes)",
                    path,
                    client_fc.size(),
                )));
            }

            if server_fc.git_lfs().is_lfs_pointer() {
                return Err(MergeResolutionError::Skipped(format!(
                    "file {path} is LFS-tracked on server",
                )));
            }

            // Also check server file size
            if server_fc.size() > max_file_size {
                return Err(MergeResolutionError::Skipped(format!(
                    "file {} is too large on server ({} bytes)",
                    path,
                    server_fc.size(),
                )));
            }
        }

        let local_file_type = client_fc.file_type();

        // Validate server file type matches client file type
        if server_fc.file_type() != local_file_type {
            return Err(MergeResolutionError::Skipped(format!(
                "file {} has type mismatch: client={:?}, server={:?}",
                path,
                local_file_type,
                server_fc.file_type(),
            )));
        }

        // The cascading merge in create_rebased_changesets reuses this id.
        let base_file = match bases.get(&non_root_path) {
            Some(Some(f)) => f,
            _ => {
                return Err(MergeResolutionError::Skipped(format!(
                    "file {non_root_path} not found in base",
                )));
            }
        };

        if base_file.file_type != local_file_type {
            return Err(MergeResolutionError::Skipped(format!(
                "file {} has type mismatch: base={:?}, local={:?}",
                non_root_path, base_file.file_type, local_file_type,
            )));
        }

        // Record metadata for the cascading merge in
        // create_rebased_changesets. The actual 3-way merge is deferred
        // to the rebase loop where it runs per-commit with the correct
        // base/local/other for each commit in the stack.
        merged_file_changes.push(MergedFileInfo {
            path: non_root_path,
            base: Some(*base_file),
            server: Some(BaseFile {
                content_id: server_fc.content_id().clone(),
                file_type: local_file_type,
            }),
            resolution,
        });
    }

    let take_incoming_count = merged_file_changes
        .iter()
        .filter(|info| info.resolution.takes_incoming())
        .count();

    // Log success
    ctx.scuba()
        .clone()
        .add("repo_name", repo.repo_identity().name())
        .add("merge_resolution_outcome", "success")
        .add("merge_resolution_files", merged_file_changes.len() as i64)
        .add(
            "merge_resolution_take_incoming_files",
            take_incoming_count as i64,
        )
        .log_with_msg("Pushrebase merge resolution succeeded", None);

    info!(
        "Pushrebase merge resolution succeeded for {} files in {}",
        merged_file_changes.len(),
        repo.repo_identity().name(),
    );

    Ok(merged_file_changes)
}

/// Reconciles carried MergedFileInfo from previous CAS retry attempts
/// with new delta info from the latest attempt. Returns the merged set.
///
/// Rules:
/// - Path in both: update server_content_id from delta (file_type is
///   guaranteed identical by collect_merge_file_info validation)
/// - Path only in carried: keep as-is (server unchanged in delta)
/// - Path only in delta: insert fresh entry
///
/// Note: If a file in `carried` is deleted on the server in the delta
/// range, this function is never called for that scenario —
/// `check_pushrebase_conflicts` returns `Err(Conflicts)` before
/// reaching the reconciliation step, since a client modification
/// conflicting with a server deletion is an irreconcilable conflict.
fn reconcile_merge_file_info(
    carried: &[MergedFileInfo],
    delta: &[MergedFileInfo],
) -> Vec<MergedFileInfo> {
    let mut by_path: HashMap<NonRootMPath, MergedFileInfo> = carried
        .iter()
        .map(|info| (info.path.clone(), info.clone()))
        .collect();

    for info in delta {
        match by_path.entry(info.path.clone()) {
            Entry::Occupied(mut e) => {
                let carried = e.get_mut();
                carried.server = info.server;
                // Path-determined, so carried and delta agree unless the
                // config or knob changed between attempts; the latest wins.
                carried.resolution = info.resolution;
            }
            Entry::Vacant(e) => {
                e.insert(info.clone());
            }
        }
    }

    by_path.into_values().collect()
}

async fn get_bookmark_value(
    ctx: &CoreContext,
    repo: &impl BookmarksRef,
    bookmark_name: &BookmarkKey,
) -> Result<Option<ChangesetId>, PushrebaseError> {
    let maybe_cs_id = repo
        .bookmarks()
        .get(ctx.clone(), bookmark_name, bookmarks::Freshness::MostRecent)
        .await?;

    Ok(maybe_cs_id)
}

/// What to do with a commit whose every change is already in its new
/// parent.
#[derive(Clone, Copy)]
enum NoopPolicy {
    /// Reject the stack per `pushrebase_reject_noop_merge_commits`.
    RejectPerKnob,
    /// Skip the commit and alias it to its rebased parent, as `sl rebase`
    /// does.
    Drop,
}

struct RebaseOutput {
    new_head: ChangesetId,
    /// Excludes dropped commits.
    rebased: RebasedChangesets,
    bonsais: Vec<BonsaiChangeset>,
    /// Old ids skipped as already applied, in stack order.
    dropped: Vec<ChangesetId>,
    /// Old id -> paths content-merged in that commit.
    merged_paths: HashMap<ChangesetId, Vec<NonRootMPath>>,
}

async fn create_rebased_changesets(
    ctx: &CoreContext,
    repo: &impl Repo,
    config: &PushrebaseFlags,
    rebased_set: &[BonsaiChangeset],
    root: ChangesetId,
    head: ChangesetId,
    onto: ChangesetId,
    hooks: &mut [Box<dyn PushrebaseCommitHook>],
    merged_file_overrides: Option<Vec<MergedFileInfo>>,
) -> Result<(ChangesetId, RebasedChangesets, Vec<BonsaiChangeset>), PushrebaseError> {
    let set = create_rebased_changesets_with(
        ctx,
        repo,
        config,
        rebased_set,
        root,
        head,
        onto,
        hooks,
        merged_file_overrides,
        NoopPolicy::RejectPerKnob,
    )
    .await?;
    Ok((set.new_head, set.rebased, set.bonsais))
}

fn same_path_conflict(path: &NonRootMPath) -> PushrebaseError {
    PushrebaseError::Conflicts(vec![PushrebaseConflict {
        left: MPath::from(path.clone()),
        right: MPath::from(path.clone()),
    }])
}

async fn create_rebased_changesets_with(
    ctx: &CoreContext,
    repo: &impl Repo,
    config: &PushrebaseFlags,
    // `root..head` in topological order, so holders of the stack need not
    // fetch it twice.
    rebased_set: &[BonsaiChangeset],
    root: ChangesetId,
    head: ChangesetId,
    onto: ChangesetId,
    hooks: &mut [Box<dyn PushrebaseCommitHook>],
    merged_file_overrides: Option<Vec<MergedFileInfo>>,
    noop: NoopPolicy,
) -> Result<RebaseOutput, PushrebaseError> {
    let rebased_set_ids: HashSet<_> = rebased_set.iter().map(|cs| cs.get_changeset_id()).collect();

    // Only consulted for the Scuba row of a path that took the pushed side,
    // which exists only when a scope was present at eligibility.
    let take_incoming = take_incoming_scope(config, repo.repo_identity().name())
        .unwrap_or(TakeIncomingScope::Requested);
    let date = if config.rewritedates {
        Some(Timestamp::now())
    } else {
        None
    };

    // Cascading merge: each commit touching a merge path is merged against
    // its own old and new parent state. `old_parent_content` follows the
    // original chain, `new_parent_content` the rebased one; `None` is
    // absent.
    let mut merge_paths: HashSet<NonRootMPath> = HashSet::new();
    let mut old_parent_content: HashMap<NonRootMPath, Option<BaseFile>> = HashMap::new();
    let mut new_parent_content: HashMap<NonRootMPath, Option<BaseFile>> = HashMap::new();
    let mut resolutions: HashMap<NonRootMPath, Resolution> = HashMap::new();

    if let Some(ref overrides) = merged_file_overrides {
        for info in overrides {
            merge_paths.insert(info.path.clone());
            old_parent_content.insert(info.path.clone(), info.base);
            new_parent_content.insert(info.path.clone(), info.server);
            resolutions.insert(info.path.clone(), info.resolution);
        }
    }

    // Create a fake timestamp, it doesn't matter what timestamp root has
    let mut remapping = hashmap! { root => (onto, Timestamp::now()) };
    let mut rebased = Vec::new();
    let mut noop_commits: Vec<(ChangesetId, Vec<NonRootMPath>)> = Vec::new();
    let mut dropped = Vec::new();
    let mut merged_paths: HashMap<ChangesetId, Vec<NonRootMPath>> = HashMap::new();
    for bcs_old in rebased_set.iter().cloned() {
        let id_old = bcs_old.get_changeset_id();

        let mut overrides_for_this: Vec<(NonRootMPath, FileChange)> = Vec::new();
        let mut duplicate_paths: HashSet<NonRootMPath> = HashSet::new();
        for (path, fc) in bcs_old.file_changes_map() {
            if !merge_paths.contains(path) {
                continue;
            }

            let local = match fc {
                FileChange::Change(tc) => Some(BaseFile {
                    content_id: tc.content_id().clone(),
                    file_type: tc.file_type(),
                }),
                FileChange::Deletion => None,
                FileChange::UntrackedChange(_) | FileChange::UntrackedDeletion => continue,
            };
            let (base, other) = match (old_parent_content.get(path), new_parent_content.get(path)) {
                (Some(base), Some(other)) => (*base, *other),
                _ => continue,
            };

            // Only the stack changed this path: plain rebase.
            if base == other {
                old_parent_content.insert(path.clone(), local);
                new_parent_content.insert(path.clone(), local);
                continue;
            }

            // Already applied in the new parent.
            if local == other {
                duplicate_paths.insert(path.clone());
                old_parent_content.insert(path.clone(), local);
                new_parent_content.insert(path.clone(), local);
                continue;
            }

            let (base, local_file, other_file) = match (base, local, other) {
                (Some(base), Some(local_file), Some(other_file)) => (base, local_file, other_file),
                // Range mode never merged deletions; keep applying them as-is.
                _ if matches!(noop, NoopPolicy::RejectPerKnob) => continue,
                _ => return Err(same_path_conflict(path)),
            };

            let resolution = resolutions.get(path).copied().unwrap_or(Resolution::Merge);

            // Under a take-incoming prefix, the pushed LFS pointer is the
            // result: nothing to fetch or merge, and the rebased commit keeps
            // the pushed change exactly (pointer flag included).
            if resolution == Resolution::IncomingVerbatim {
                // The pushed commit did not change the file (a touch): the
                // destination's change is the only real one and there is
                // nothing of the pushed side to take.
                if base == local_file {
                    return Err(same_path_conflict(path));
                }
                log_take_incoming(
                    ctx,
                    repo,
                    TakeIncoming {
                        bcs: &bcs_old,
                        path,
                        resolution,
                        scope: take_incoming,
                        base: &base,
                        local: &local_file,
                        other: &other_file,
                    },
                );
                merged_paths.entry(id_old).or_default().push(path.clone());
                old_parent_content.insert(path.clone(), local);
                new_parent_content.insert(path.clone(), local);
                continue;
            }

            match merge_file_by_content_ids(
                ctx,
                repo,
                path,
                base.content_id,
                local_file.content_id,
                other_file.content_id,
                resolution == Resolution::PreferIncoming,
            )
            .await
            {
                FileMergeOutcome::Clean(merged_bytes) => {
                    if resolution == Resolution::PreferIncoming {
                        log_take_incoming(
                            ctx,
                            repo,
                            TakeIncoming {
                                bcs: &bcs_old,
                                path,
                                resolution,
                                scope: take_incoming,
                                base: &base,
                                local: &local_file,
                                other: &other_file,
                            },
                        );
                    }
                    let size = merged_bytes.len() as u64;
                    let meta = filestore::store(
                        repo.repo_blobstore(),
                        *repo.filestore_config(),
                        ctx,
                        &filestore::StoreRequest::new(size),
                        stream::once(future::ok(merged_bytes)),
                    )
                    .await?;
                    let merged = Some(BaseFile {
                        content_id: meta.content_id,
                        file_type: local_file.file_type,
                    });

                    overrides_for_this.push((
                        path.clone(),
                        FileChange::tracked(
                            meta.content_id,
                            local_file.file_type,
                            meta.total_size,
                            None,
                            GitLfs::FullContent,
                        ),
                    ));
                    merged_paths.entry(id_old).or_default().push(path.clone());
                    if matches!(noop, NoopPolicy::Drop) && meta.content_id == other_file.content_id
                    {
                        duplicate_paths.insert(path.clone());
                    }

                    old_parent_content.insert(path.clone(), local);
                    new_parent_content.insert(path.clone(), merged);
                }
                FileMergeOutcome::Conflict(description) => {
                    warn!("Cascading merge conflict on {}: {}", path, description,);
                    return Err(same_path_conflict(path));
                }
                FileMergeOutcome::Error(err) => {
                    warn!("Cascading merge error on {}: {:#}", path, err);
                    return Err(same_path_conflict(path));
                }
            }
        }

        let real_change_count = bcs_old
            .file_changes_map()
            .keys()
            .filter(|p| !duplicate_paths.contains(*p))
            .count();
        if real_change_count == 0 && !duplicate_paths.is_empty() {
            match noop {
                NoopPolicy::Drop if bcs_old.subtree_changes().is_empty() => {
                    let parent = bcs_old.parents().next().ok_or_else(|| {
                        PushrebaseError::Error(anyhow!("commit {id_old} has no parent"))
                    })?;
                    let alias = *remapping.get(&parent).ok_or_else(|| {
                        PushrebaseError::Error(anyhow!(
                            "parent {parent} of {id_old} is not in the rebase set"
                        ))
                    })?;
                    remapping.insert(id_old, alias);
                    dropped.push(id_old);
                    merged_paths.remove(&id_old);
                    continue;
                }
                NoopPolicy::Drop => {}
                NoopPolicy::RejectPerKnob => {
                    noop_commits.push((id_old, duplicate_paths.iter().cloned().collect()));
                }
            }
        }

        let overrides_ref = if overrides_for_this.is_empty() {
            None
        } else {
            Some(&overrides_for_this)
        };

        let bcs_new = rebase_changeset(
            ctx,
            bcs_old,
            &remapping,
            date.as_ref(),
            &root,
            &onto,
            repo,
            &rebased_set_ids,
            hooks,
            overrides_ref,
            config.mainline_parent_first,
        )
        .await?;
        let timestamp = Timestamp::from(*bcs_new.author_date());
        remapping.insert(id_old, (bcs_new.get_changeset_id(), timestamp));
        rebased.push(bcs_new);
    }

    // Post-loop: if any commits became no-ops due to merge resolution, log
    // them to Scuba/ODS and (when JK enabled) reject the entire stack with
    // a Conflicts error matching the pre-merge-resolution behavior.
    if !noop_commits.is_empty() {
        let repo_name = repo.repo_identity().name();
        let repo_args = (repo_name.to_string(),);

        let reject_noop = justknobs::eval(
            "scm/mononoke:pushrebase_reject_noop_merge_commits",
            None,
            Some(repo_name),
        );
        let enforcement = if reject_noop { "rejected" } else { "logged" };

        for (cs_id, paths) in &noop_commits {
            STATS::noop_merge_commits_detected.add_value(1, repo_args.clone());

            let path_strs: Vec<String> = paths.iter().take(10).map(|p| p.to_string()).collect();
            ctx.scuba()
                .clone()
                .add("repo_name", repo_name)
                .add("noop_changeset_id", cs_id.to_string())
                .add("noop_duplicate_paths", path_strs.join(", "))
                .add("noop_duplicate_path_count", paths.len() as i64)
                .add("noop_enforcement_action", enforcement)
                .log_with_msg("Pushrebase no-op merge commit detected", None);
        }

        if reject_noop {
            STATS::noop_merge_commits_rejected.add_value(noop_commits.len() as i64, repo_args);
            let conflicts: Vec<PushrebaseConflict> = noop_commits
                .into_iter()
                .flat_map(|(_, paths)| paths)
                .map(|p| PushrebaseConflict {
                    left: MPath::from(p.clone()),
                    right: MPath::from(p),
                })
                .collect();
            return Err(PushrebaseError::Conflicts(conflicts));
        }
    }

    let new_head = remapping
        .get(&head)
        .map(|(cs, _)| cs)
        .cloned()
        .unwrap_or(head);
    let dropped_ids: HashSet<ChangesetId> = dropped.iter().copied().collect();
    Ok(RebaseOutput {
        new_head,
        // `root` wasn't rebased and dropped commits have no successor.
        rebased: remapping
            .into_iter()
            .filter(|(id_old, _)| *id_old != root && !dropped_ids.contains(id_old))
            .collect(),
        bonsais: rebased,
        dropped,
        merged_paths,
    })
}

async fn rebase_changeset(
    ctx: &CoreContext,
    bcs: BonsaiChangeset,
    remapping: &HashMap<ChangesetId, (ChangesetId, Timestamp)>,
    timestamp: Option<&Timestamp>,
    root: &ChangesetId,
    onto: &ChangesetId,
    repo: &impl Repo,
    rebased_set: &HashSet<ChangesetId>,
    hooks: &mut [Box<dyn PushrebaseCommitHook>],
    merged_file_overrides: Option<&Vec<(NonRootMPath, FileChange)>>,
    mainline_parent_first: bool,
) -> Result<BonsaiChangeset> {
    let orig_cs_id = bcs.get_changeset_id();
    let new_file_changes =
        generate_additional_bonsai_file_changes(ctx, &bcs, root, onto, repo, rebased_set).await?;
    let mut bcs = bcs.into_mut();

    // With `mainline_parent_first`, a merge whose first parent is not rebased
    // takes its mainline parent, the first of its parents that is rebased or
    // is the root, as its first parent, keeping the order of the others, and
    // is marked with `PARENTS_SWAPPED_EXTRA`. A bonsai merge's file changes do
    // not depend on the order of its parents.
    let swap_index = bcs
        .parents
        .iter()
        .position(|p| p == root || rebased_set.contains(p))
        .filter(|index| mainline_parent_first && *index > 0);
    bcs.parents = bcs
        .parents
        .into_iter()
        .map(|p| remapping.get(&p).map(|(cs, _)| cs).cloned().unwrap_or(p))
        .collect();
    if let Some(index) = swap_index {
        // The marker must make the hash differ from the pushed commit's.
        if bcs.hg_extra.contains_key(PARENTS_SWAPPED_EXTRA) {
            return Err(PushrebaseInternalError::ParentsAlreadySwapped(orig_cs_id).into());
        }
        bcs.parents[..=index].rotate_right(1);
        bcs.hg_extra
            .insert(PARENTS_SWAPPED_EXTRA.to_string(), b"1".to_vec());
        ctx.scuba()
            .clone()
            .log_with_msg("pushrebase_parents_swapped", Some(orig_cs_id.to_string()));
    }

    match timestamp {
        Some(timestamp) => {
            let author_tz = bcs.author_date.tz_offset_secs();
            bcs.author_date = DateTime::from_timestamp(timestamp.timestamp_seconds(), author_tz)?;
            if let Some(committer_date) = &mut bcs.committer_date {
                let committer_tz = committer_date.tz_offset_secs();
                *committer_date =
                    DateTime::from_timestamp(timestamp.timestamp_seconds(), committer_tz)?;
            }
        }
        None => {}
    }

    // Mutation information from the original commit must be stripped.
    for key in MUTATION_KEYS {
        bcs.hg_extra.remove(*key);
    }

    // Copy information in bonsai changeset contains a commit parent. So parent changes, then
    // copy information for all copied/moved files needs to be updated. Only the copy source
    // changes; the rest of the change, including how Git serves it, is kept as authored.
    let mut file_changes = bcs.file_changes;
    for file_change in file_changes.values_mut() {
        match file_change {
            FileChange::Change(tc) => {
                let copy_from = tc.copy_from().map(|(path, cs)| {
                    (
                        path.clone(),
                        remapping.get(cs).map(|(cs, _)| cs).cloned().unwrap_or(*cs),
                    )
                });
                *file_change = FileChange::Change(tc.with_new_copy_from(copy_from));
            }
            FileChange::Deletion
            | FileChange::UntrackedDeletion
            | FileChange::UntrackedChange(_) => {}
        }
    }

    // Subtree changes might be sourced from the rebase set, in which case they must be updated.
    for (_path, change) in bcs.subtree_changes.iter_mut() {
        if let Some((from_csid, _from_path)) = change.change_source() {
            if rebased_set.contains(&from_csid) {
                if let Some((new_from_csid, _)) = remapping.get(&from_csid) {
                    change.replace_source_changeset_id(*new_from_csid);
                }
            }
        }
    }

    // Apply merged file overrides from merge resolution.
    // These replace the original file changes for conflicting paths with
    // the merged content that incorporates both local and server-side edits.
    if let Some(overrides) = merged_file_overrides {
        for (path, fc) in overrides {
            file_changes.insert(path.clone(), fc.clone());
        }
    }

    let new_file_paths: HashSet<_> =
        HashSet::from_iter(new_file_changes.iter().map(|(path, _)| path));
    for path in file_changes.keys() {
        if new_file_paths.contains(path) {
            return Err(PushrebaseInternalError::NewFileChangesConflict(orig_cs_id).into());
        }
    }

    file_changes.extend(new_file_changes);
    bcs.file_changes = file_changes;

    for hook in hooks.iter_mut() {
        hook.post_rebase_changeset(orig_cs_id, &mut bcs)?;
    }

    bcs.freeze()
}

// Merge bonsai commits are treated specially in Mononoke. If parents of the merge commit
// have the same file but with a different content, then there's a conflict and to resolve it
// this file should be present in merge bonsai commit. So if we are pushrebasing a merge
// commit we need to take special care.
// See example below
//
// o <- onto
// |
// A   C <-  commit to pushrebase
// | / |
// o   D
// | /
// B
//
// If commit 'A' changes any of the files that existed in commit B (say, file.txt), then
// after commit 'C' is pushrebased on top of master then bonsai logic will try to merge
// file.txt from commit D and from "onto". If bonsai commit that corresponds
// to a rebased commit C doesn't have a file.txt entry, then we'll have invalid bonsai
// changeset (i.e. changeset for which no derived data can be derived, including hg changesets).
//
// generate_additional_bonsai_file_changes works around this problem. It returns a Vec containing
// a file change for all files that were changed between root and onto and that are different between onto
// and parent of bcs that's outside of rebase set (in the example above one of the file changes will be the file
// change for "file.txt").
//
// o <- onto
// |
// A  <- modifies file.txt
// |
// |   C <- Commit C is a merge commit we are pushrebasing
// | / |
// o   D <- commit D has file.txt (because it exists in commit B), so we need to add additional change file.txt
// | /
// B <- this commit has file.text
//
// The file change sets the file to the file as it exists in onto, thus resolving the
// conflict. Since these files were changed after bcs lineage forked off of the root, that means
// that bcs has a "stale" version of them, and that's why we use onto's version instead.
//
// Note that there's another correct solution - we could just add union of changed files for
// (root::onto) and changed files for (root::bcs), however that would add a lot of unnecessary
// file change entries to the pushrebased bonsai merge commit. That would be especially wasteful
// for the case we care about the most - merging a new repo - because we'd list all newly added files.
//
// Note that we don't need to do that if both parents of the merge commit are in the rebased
// set (see example below)
//
// o <- onto
// |
// A      C
// |    / |
// o   X  D
// |  / /
// | Z
// |/
// B
async fn generate_additional_bonsai_file_changes(
    ctx: &CoreContext,
    bcs: &BonsaiChangeset,
    root: &ChangesetId,
    onto: &ChangesetId,
    repo: &impl Repo,
    rebased_set: &HashSet<ChangesetId>,
) -> Result<Vec<(NonRootMPath, FileChange)>> {
    let parents: Vec<_> = bcs.parents().collect();

    if parents.len() <= 1 {
        return Ok(vec![]);
    }

    // We use non_root_parent_outside_of_rebase_set below to figure out what
    // stale entries we DO NOT need to add to the bonsai changeset.
    // o <- onto
    // |
    // A
    // |
    // |   C <- this is the commit being rebased (bcs_id)
    // | / |
    // o   D <- this is non_root_parent_outside_of_rebase_set
    // | /
    // B
    let non_root_parents_outside_of_rebase_set = parents
        .iter()
        .filter(|p| !rebased_set.contains(p) && p != &root)
        .collect::<Vec<_>>();

    if non_root_parents_outside_of_rebase_set.is_empty() {
        return Ok(vec![]);
    }

    let bonsai_diff = find_bonsai_diff(ctx, repo, *root, *onto)
        .await?
        .try_collect::<Vec<_>>()
        .await?;

    let mut paths = vec![];
    for res in &bonsai_diff {
        paths.push(res.path().clone())
    }

    // If a file is not present in the parent, then no need to add it to the new_file_changes.
    // This is done in order to not add unnecessary file changes if they are guaranteed to
    // not have conflicts.
    // Consider the following case:
    //
    // o <- onto
    // |
    // A  <- adds file.txt
    // |
    // |   C <- commit C doesn't have file.txt either
    // | / |
    // o   D <- commit D doesn't have file.txt, so no conflicts possible after pushrebase
    // | /
    // B
    let read_from_augmented = hg_manifests_read_from_augmented(repo.repo_identity().name());
    let mut futs = vec![];
    for p in non_root_parents_outside_of_rebase_set {
        let paths = paths.clone();
        futs.push(async move {
            let mfid = id_to_manifestid(ctx, repo, *p).await?;
            let blobstore = repo.repo_blobstore().clone();
            let found = if read_from_augmented {
                HgAugmentedManifestId::new(mfid.into_nodehash())
                    .find_entries(ctx.clone(), blobstore, paths)
                    .map_ok(|(path, _)| path)
                    .left_stream()
            } else {
                mfid.find_entries(ctx.clone(), blobstore, paths)
                    .map_ok(|(path, _)| path)
                    .right_stream()
            };
            let stale = found
                .try_filter_map(async |path| Ok(Option::<NonRootMPath>::from(path)))
                .try_collect::<HashSet<_>>()
                .await?;
            Result::<_, Error>::Ok(stale)
        });
    }

    let stale_entries = future::try_join_all(futs)
        .await?
        .into_iter()
        .flatten()
        .collect::<HashSet<_>>();

    let mut new_file_changes = vec![];
    for res in bonsai_diff {
        if !stale_entries.contains(res.path()) {
            continue;
        }

        new_file_changes.push(convert_diff_result_into_file_change_for_diamond_merge(
            ctx, repo, res,
        ));
    }

    new_file_changes
        .into_iter()
        .collect::<stream::FuturesUnordered<_>>()
        .try_collect()
        .await
}

/// Wrap a list of pushrebase transaction hooks into a single
/// `BookmarkTransactionHook` closure that runs them sequentially.
fn wrap_pushrebase_hooks(
    hooks: Vec<Box<dyn PushrebaseTransactionHook>>,
) -> BookmarkTransactionHook {
    let hooks = Arc::new(hooks);
    Arc::new(move |ctx, mut sql_txn| {
        let hooks = hooks.clone();
        async move {
            for hook in hooks.iter() {
                sql_txn = hook.populate_transaction(&ctx, sql_txn).await?
            }
            Ok(sql_txn)
        }
        .boxed()
    })
}

async fn try_move_bookmark(
    ctx: CoreContext,
    repo: &impl Repo,
    bookmark: &BookmarkKey,
    old_value: Option<ChangesetId>,
    new_value: ChangesetId,
    rebased_changesets: RebasedChangesets,
    hooks: Vec<Box<dyn PushrebaseTransactionHook>>,
) -> Result<
    Option<(
        ChangesetId,
        BookmarkUpdateLogId,
        Vec<PushrebaseChangesetPair>,
    )>,
    PushrebaseError,
> {
    let mut txn = repo.bookmarks().create_transaction(ctx);

    match old_value {
        Some(old_value) => {
            txn.update(
                bookmark,
                new_value,
                old_value,
                BookmarkUpdateReason::Pushrebase,
            )?;
        }
        None => {
            txn.create(bookmark, new_value, BookmarkUpdateReason::Pushrebase)?;
        }
    }

    let maybe_log_id = txn
        .commit_with_hooks(vec![wrap_pushrebase_hooks(hooks)])
        .await?
        .map(BookmarkUpdateLogId::from);

    Ok(maybe_log_id.map(|log_id| {
        (
            new_value,
            log_id,
            rebased_changesets_into_pairs(rebased_changesets),
        )
    }))
}

#[cfg(test)]
mod tests;
