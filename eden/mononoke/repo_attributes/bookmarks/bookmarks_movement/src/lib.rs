/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![feature(trait_alias)]

use std::collections::HashMap;
use std::time::Instant;

use ::repo_lock::RepoLockRef;
use bonsai_git_mapping::BonsaiGitMappingArc;
use bonsai_git_mapping::BonsaiGitMappingRef;
use bonsai_globalrev_mapping::BonsaiGlobalrevMappingArc;
use bonsai_globalrev_mapping::BonsaiGlobalrevMappingRef;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bookmarks::BookmarkMoveAlreadyProcessed;
use bookmarks::BookmarkTransaction;
use bookmarks::BookmarkTransactionHook;
use bookmarks::BookmarkUpdateLogId;
use bookmarks::BookmarksRef;
use bookmarks_types::BookmarkKey;
use bytes::Bytes;
use commit_graph::CommitGraphRef;
use commit_graph::CommitGraphWriterRef;
use context::CoreContext;
use filestore::FilestoreConfigRef;
use itertools::Itertools;
use metaconfig_types::RepoConfigRef;
use mononoke_types::ChangesetId;
use mononoke_types::NonRootMPath;
use phases::PhasesRef;
use pushrebase::PushrebaseError;
use pushrebase_mutation_mapping::PushrebaseMutationMappingRef;
use repo_authorization::AuthorizationError;
use repo_blobstore::RepoBlobstoreArc;
use repo_blobstore::RepoBlobstoreRef;
use repo_bookmark_attrs::RepoBookmarkAttrsRef;
use repo_cross_repo::RepoCrossRepoRef;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentityRef;
use repo_permission_checker::RepoPermissionCheckerRef;
use repo_update_logger::BookmarkInfo;
use repo_update_logger::CommitInfo;
use repo_update_logger::log_bookmark_operation;
use repo_update_logger::log_new_commits;
use thiserror::Error;

pub mod affected_changesets;
mod create;
mod delete;
mod git_mapping;
mod hook_running;
mod pushrebase_onto;
mod repo_lock;
mod restrictions;
mod update;

pub use bookmarks_types::BookmarkKind;
pub use hooks::CrossRepoPushSource;
pub use hooks::HookRejection;
pub use pushrebase::PushrebaseOutcome;
pub use pushrebase_hooks::PushrebaseHooksError;
pub use pushrebase_hooks::get_pushrebase_hooks;

pub use crate::affected_changesets::N_CHANGESETS_TO_LOAD_AT_ONCE;
pub use crate::affected_changesets::check_case_conflicts;
pub use crate::affected_changesets::newly_public_changeset_ids;
pub use crate::affected_changesets::reject_disallowed_publishing_extras;
pub use crate::create::CreateBookmarkOp;
pub use crate::delete::DeleteBookmarkOp;
pub use crate::hook_running::AdminBypassError;
pub use crate::hook_running::run_bookmark_hooks;
pub use crate::hook_running::run_changeset_hooks;
pub use crate::pushrebase_onto::PushrebasePreparation;
pub use crate::pushrebase_onto::postprocess_pushrebase_outcome;
pub use crate::pushrebase_onto::prepare_pushrebase;
pub use crate::pushrebase_onto::pushrebase_flags;
pub use crate::restrictions::BookmarkKindRestrictions;
pub use crate::restrictions::check_bookmark_sync_config;
pub use crate::update::BookmarkUpdatePolicy;
pub use crate::update::BookmarkUpdateTargets;
pub use crate::update::UpdateBookmarkOp;

const ALLOW_NON_FFWD_PUSHVAR: &str = "x-git-allow-non-ffwd-push";
const ALLOW_BRANCH_DELETION: &str = "x-git-allow-branch-deletion";
const ALLOW_TAG_DELETION: &str = "x-git-allow-tag-deletion";
const MIRROR_UPLOAD_PUSHVAR: &str = "MIRROR_UPLOAD";

/// Whether the caller declares this bookmark move part of a mirror upload.
///
/// A mirror upload replays moves that a source repo already made, so it needs
/// the mirror upload permission and it may reuse the source repo's log ids.
/// This pushvar is the only thing that marks a move as a mirror upload.
fn is_mirror_upload(pushvars: Option<&HashMap<String, Bytes>>) -> bool {
    pushvars
        .and_then(|p| p.get(MIRROR_UPLOAD_PUSHVAR))
        .is_some_and(|v| **v == *b"true")
}

/// Trait alias for bookmarks movement repositories.
///
/// These are the repo attributes that are necessary to call most functions in
/// bookmarks movement.
pub trait Repo = BonsaiHgMappingRef
    + BonsaiGitMappingArc
    + BonsaiGlobalrevMappingArc
    + BookmarksRef
    + FilestoreConfigRef
    + PhasesRef
    + PushrebaseMutationMappingRef
    + RepoBookmarkAttrsRef
    + RepoConfigRef
    + RepoDerivedDataRef
    + RepoBlobstoreArc
    + RepoBlobstoreRef
    + RepoCrossRepoRef
    + RepoIdentityRef
    + RepoPermissionCheckerRef
    + RepoLockRef
    + CommitGraphRef
    + CommitGraphWriterRef
    + Send
    + Sync;

/// An error encountered during an attempt to move a bookmark.
#[derive(Debug, Error)]
pub enum BookmarkMovementError {
    #[error("Non fast-forward bookmark move of '{bookmark}' from {from} to {to}")]
    NonFastForwardMove {
        bookmark: BookmarkKey,
        from: ChangesetId,
        to: ChangesetId,
    },

    #[error("Deletion of '{bookmark}' is prohibited")]
    DeletionProhibited { bookmark: BookmarkKey },

    #[error(transparent)]
    AuthorizationError(#[from] AuthorizationError),

    #[error(transparent)]
    AdminBypassError(#[from] hook_running::AdminBypassError),

    #[error(
        "Invalid scratch bookmark: {bookmark} (scratch bookmarks must match pattern {pattern})"
    )]
    InvalidScratchBookmark {
        bookmark: BookmarkKey,
        pattern: String,
    },

    #[error(
        "Invalid publishing bookmark: {bookmark} (only scratch bookmarks may match pattern {pattern})"
    )]
    InvalidPublishingBookmark {
        bookmark: BookmarkKey,
        pattern: String,
    },

    #[error(
        "Invalid scratch bookmark: {bookmark} (scratch bookmarks are not enabled for this repo)"
    )]
    ScratchBookmarksDisabled { bookmark: BookmarkKey },

    #[error("Bookmark transaction failed")]
    TransactionFailed,

    /// A modern_sync mirror move to a `*_shadow` replica was already applied (a
    /// lost-ack replay). Not a failure: the caller reuses the source log id, so
    /// the move is idempotent and safe to replay.
    #[error("Bookmark move already processed")]
    AlreadyProcessed,

    #[error("Hooks failed:\n{}", describe_hook_rejections(.0.as_slice()))]
    HookFailure(Vec<HookRejection>),

    #[error("Disallowed extra {extra} is set on {changeset_id}.")]
    DisallowedExtra {
        changeset_id: ChangesetId,
        extra: String,
    },

    #[error("Pushrebase failed: {0}")]
    PushrebaseError(#[source] PushrebaseError),

    /// The repo's `block_merges` config forbids pushrebasing merge commits.
    #[error(
        "Pushrebase blocked because it contains a merge commit.\n\
        If you need this for a specific use case please contact\n\
        the Source Control team at https://fburl.com/27qnuyl2"
    )]
    MergesBlocked,

    #[error(transparent)]
    PushrebaseHooksError(#[from] PushrebaseHooksError),

    #[error("Repo is locked: {0}")]
    RepoLocked(String),

    #[error("Case conflict found in {changeset_id}: {path1} conflicts with {path2}")]
    CaseConflict {
        changeset_id: ChangesetId,
        path1: NonRootMPath,
        path2: NonRootMPath,
    },

    #[error("Bookmark '{bookmark}' can only be moved to ancestors of '{descendant_bookmark}'")]
    RequiresAncestorOf {
        bookmark: BookmarkKey,
        descendant_bookmark: BookmarkKey,
    },

    #[error(
        "Bookmark '{bookmark}' cannot be moved because publishing bookmarks are being redirected"
    )]
    PushRedirectorEnabledForPublishing { bookmark: BookmarkKey },

    #[error("Bookmark '{bookmark}' cannot be moved because scratch bookmarks are being redirected")]
    PushRedirectorEnabledForScratch { bookmark: BookmarkKey },

    #[error(transparent)]
    Error(#[from] anyhow::Error),
}

pub fn describe_hook_rejections(rejections: &[HookRejection]) -> String {
    rejections
        .iter()
        .map(|rejection| {
            format!(
                "  {} for {}: {}",
                rejection.hook_name, rejection.cs_id, rejection.reason.long_description
            )
        })
        .join("\n")
}

pub struct BookmarkInfoData {
    bookmark_info: BookmarkInfo,
    log_new_public_commits_to_scribe: bool,
    commits: Vec<CommitInfo>,
}

impl BookmarkInfoData {
    pub fn new(
        bookmark_info: BookmarkInfo,
        log_new_public_commits_to_scribe: bool,
        commits: Vec<CommitInfo>,
    ) -> Self {
        Self {
            bookmark_info,
            log_new_public_commits_to_scribe,
            commits,
        }
    }

    pub async fn log(
        self,
        ctx: &CoreContext,
        repo: &(
             impl RepoIdentityRef
             + CommitGraphRef
             + RepoConfigRef
             + BonsaiGlobalrevMappingRef
             + BonsaiGitMappingRef
         ),
    ) {
        if self.log_new_public_commits_to_scribe {
            log_new_commits(
                ctx,
                repo,
                Some((
                    &self.bookmark_info.bookmark_name,
                    self.bookmark_info.bookmark_kind,
                )),
                self.commits,
            )
            .await;
        }
        log_bookmark_operation(ctx, repo, &self.bookmark_info).await;
    }
}

pub struct TransactionWithHooks {
    pub transaction: Box<dyn BookmarkTransaction>,
    pub txn_hooks: Vec<BookmarkTransactionHook>,
}

impl TransactionWithHooks {
    pub fn new(
        transaction: Box<dyn BookmarkTransaction>,
        txn_hooks: Vec<BookmarkTransactionHook>,
    ) -> Self {
        Self {
            transaction,
            txn_hooks,
        }
    }

    pub async fn commit(self) -> Result<BookmarkUpdateLogId, BookmarkMovementError> {
        let maybe_log_id = match self.transaction.commit_with_hooks(self.txn_hooks).await {
            Ok(maybe_log_id) => maybe_log_id,
            Err(err) => {
                if err.is::<BookmarkMoveAlreadyProcessed>() {
                    return Err(BookmarkMovementError::AlreadyProcessed);
                }
                return Err(err.into());
            }
        };
        if let Some(log_id) = maybe_log_id {
            Ok(log_id.into())
        } else {
            Err(BookmarkMovementError::TransactionFailed)
        }
    }
}

pub struct BookmarkInfoTransaction {
    pub info_data: BookmarkInfoData,
    pub transaction: TransactionWithHooks,
}

impl BookmarkInfoTransaction {
    pub fn new(
        info_data: BookmarkInfoData,
        transaction: Box<dyn BookmarkTransaction>,
        txn_hooks: Vec<BookmarkTransactionHook>,
    ) -> Self {
        Self {
            info_data,
            transaction: TransactionWithHooks::new(transaction, txn_hooks),
        }
    }

    pub async fn commit_and_log(
        self,
        ctx: &CoreContext,
        repo: &impl Repo,
    ) -> Result<BookmarkUpdateLogId, BookmarkMovementError> {
        let emit_saturation = repo
            .repo_config()
            .pushrebase
            .flags
            .monitoring_bookmark
            .is_some();
        let start = Instant::now();
        let result = self.transaction.commit().await;
        if emit_saturation {
            bookmarks::saturation::record_bookmark_move(
                repo.repo_identity().name(),
                start.elapsed().as_nanos() as i64,
                result.is_ok(),
            );
        }
        let log_id = result?;
        self.info_data.log(ctx, repo).await;
        Ok(log_id)
    }
}
