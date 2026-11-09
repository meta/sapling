/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use bookmarks::BookmarkKey;
use commit_graph::CommitGraphRef;
use futures::try_join;
use futures_watchdog::WatchdogExt;
use metaconfig_types::MergeResolutionOverride;
use metaconfig_types::PushrebaseFlags;
use metaconfig_types::RepoConfigRef;
use mononoke_types::ChangesetId;
use phases::PhasesRef;
use pushrebase::MergeResolutionSummary;
use pushrebase::PushrebaseConflict;
use pushrebase::PushrebaseError;
use pushrebase::rebase_stack_onto_manifest;
use repo_identity::RepoIdentityRef;

use crate::BookmarkFreshness;
use crate::MononokeRepo;
use crate::errors::MononokeError;
use crate::repo::RepoContext;
use crate::repo::rebase_stack::MAX_COMMITS_KNOB;
use crate::repo::rebase_stack::MAX_PATHS_KNOB;
use crate::repo::rebase_stack::RebaseStackMergeResolution;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckRebaseConflictsStatus {
    Clean,
    Conflicts,
    AlreadyLanded,
}

#[derive(Debug)]
pub struct CheckRebaseConflictsOutcome {
    pub status: CheckRebaseConflictsStatus,
    pub conflicts: Vec<PushrebaseConflict>,
    pub onto: ChangesetId,
    pub base: ChangesetId,
    pub overlapping_path_count: u64,
    pub merged_path_count: u64,
}

impl<R: MononokeRepo> RepoContext<R> {
    /// Check whether the draft stack `base..head` would merge onto the tip
    /// of `target_bookmark`, creating no commits and moving no bookmark.
    /// `base` is the merge base of `head` and the target. Read-only: unlike
    /// `rebase_stack`, this needs no write authorization.
    pub async fn check_rebase_conflicts(
        &self,
        head: ChangesetId,
        target_bookmark: &str,
        merge_resolution: RebaseStackMergeResolution,
    ) -> Result<CheckRebaseConflictsOutcome, MononokeError> {
        let ctx = self.ctx();
        let repo_name = self.repo().repo_identity().name();
        let bookmark = BookmarkKey::new(target_bookmark)
            .map_err(|e| MononokeError::InvalidRequest(e.to_string()))?;
        let onto = self
            .resolve_bookmark(&bookmark, BookmarkFreshness::MaybeStale)
            .watched()
            .await?
            .ok_or_else(|| {
                MononokeError::InvalidRequest(format!("unknown bookmark {target_bookmark}"))
            })?
            .id();

        if !self
            .repo()
            .phases()
            .get_public(ctx, vec![head], false)
            .await?
            .is_empty()
        {
            return Err(MononokeError::InvalidRequest(format!(
                "head {head} is public"
            )));
        }
        let commit_graph = self.repo().commit_graph();
        let base = commit_graph
            .common_base(ctx, head, onto)
            .watched()
            .await?
            .first()
            .copied()
            .ok_or_else(|| {
                MononokeError::InvalidRequest(format!(
                    "head {head} and target {onto} have no common base"
                ))
            })?;
        if base == head {
            return Ok(CheckRebaseConflictsOutcome {
                status: CheckRebaseConflictsStatus::AlreadyLanded,
                conflicts: Vec::new(),
                onto,
                base,
                overlapping_path_count: 0,
                merged_path_count: 0,
            });
        }
        if !commit_graph.is_linear_stack(ctx, base, head).await? {
            return Err(MononokeError::InvalidRequest(format!(
                "{base}..{head} is not a linear stack"
            )));
        }
        // Linear, so the generation gap is the stack size; check it before
        // walking anything.
        let max_commits: usize = justknobs::get_as::<usize>(MAX_COMMITS_KNOB, Some(repo_name));
        let (base_gen, head_gen) = try_join!(
            commit_graph.changeset_generation(ctx, base),
            commit_graph.changeset_generation(ctx, head),
        )?;
        if head_gen.value().saturating_sub(base_gen.value()) > max_commits as u64 {
            return Err(MononokeError::InvalidRequest(format!(
                "stack has more than {max_commits} commits"
            )));
        }
        if base == onto {
            return Ok(CheckRebaseConflictsOutcome {
                status: CheckRebaseConflictsStatus::Clean,
                conflicts: Vec::new(),
                onto,
                base,
                overlapping_path_count: 0,
                merged_path_count: 0,
            });
        }
        let max_paths: usize = justknobs::get_as::<usize>(MAX_PATHS_KNOB, Some(repo_name));

        let merge_resolution_override = match merge_resolution {
            RebaseStackMergeResolution::Disabled => MergeResolutionOverride::ForceOff,
            RebaseStackMergeResolution::Default => MergeResolutionOverride::UseJk,
        };
        let flags = PushrebaseFlags {
            rewritedates: false,
            merge_resolution_override,
            ..self.repo().repo_config().pushrebase.flags.clone()
        };

        let rebased =
            match rebase_stack_onto_manifest(ctx, self.repo(), &flags, base, head, onto, max_paths)
                .watched()
                .await
            {
                Ok(rebased) => rebased,
                Err(PushrebaseError::Conflicts(conflicts)) => {
                    return Ok(CheckRebaseConflictsOutcome {
                        status: CheckRebaseConflictsStatus::Conflicts,
                        overlapping_path_count: conflicts.len() as u64,
                        conflicts,
                        onto,
                        base,
                        merged_path_count: 0,
                    });
                }
                Err(PushrebaseError::ManifestNotDerived(cs_id)) => {
                    return Err(MononokeError::ManifestNotDerived(cs_id));
                }
                Err(PushrebaseError::Error(e)) => return Err(MononokeError::from(e)),
                Err(other) => return Err(MononokeError::InvalidRequest(other.to_string())),
            };

        let (overlapping_path_count, merged_path_count) = match rebased.merge_summary {
            MergeResolutionSummary::Succeeded {
                conflict_files_count,
                resolved_files_count,
                ..
            } => (conflict_files_count, resolved_files_count),
            _ => (0, 0),
        };
        // The merge drops no-op commits, so a head equal to the target means
        // every commit was already contained in it.
        let status = if rebased.new_head == onto {
            CheckRebaseConflictsStatus::AlreadyLanded
        } else {
            CheckRebaseConflictsStatus::Clean
        };
        Ok(CheckRebaseConflictsOutcome {
            status,
            conflicts: Vec::new(),
            onto,
            base,
            overlapping_path_count,
            merged_path_count,
        })
    }
}
