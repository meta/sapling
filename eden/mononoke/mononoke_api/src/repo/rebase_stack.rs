/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;

use commit_graph::CommitGraphRef;
use futures::StreamExt;
use futures::future;
use futures::try_join;
use metaconfig_types::MergeResolutionOverride;
use metaconfig_types::PushrebaseFlags;
use metaconfig_types::RepoConfigRef;
use mononoke_types::ChangesetId;
use mononoke_types::NonRootMPath;
use phases::PhasesRef;
use pushrebase::MergeResolutionSummary;
use pushrebase::PushrebaseError;
use pushrebase::rebase_stack_onto_manifest;
use repo_authorization::RepoWriteOperation;
use repo_identity::RepoIdentityRef;

use crate::MononokeRepo;
use crate::errors::MononokeError;
use crate::repo::RepoContext;

pub(crate) const MAX_COMMITS_KNOB: &str = "scm/mononoke:scs_rebase_stack_max_commits";
pub(crate) const MAX_PATHS_KNOB: &str = "scm/mononoke:scs_rebase_stack_max_paths";

#[derive(Clone, Copy, Debug)]
pub enum RebaseStackMergeResolution {
    /// Follow the repo's `pushrebase_enable_merge_resolution` knob.
    Default,
    /// Any overlapping path is a conflict.
    Disabled,
}

#[derive(Debug)]
pub struct RebasedStackCommit {
    pub old: ChangesetId,
    /// `None` when the commit was dropped as already applied.
    pub new: Option<ChangesetId>,
    pub merged_paths: Vec<NonRootMPath>,
}

#[derive(Debug)]
pub struct RebaseStackOutcome {
    pub head: ChangesetId,
    /// Bottom to top.
    pub commits: Vec<RebasedStackCommit>,
    pub overlapping_path_count: u64,
    pub merged_path_count: u64,
}

impl<R: MononokeRepo> RepoContext<R> {
    /// Rebase the draft stack `base..head` onto `onto`, persisting the new
    /// drafts. No bookmark moves, no hooks run, nothing is derived for the
    /// inputs.
    pub async fn rebase_stack(
        &self,
        base: ChangesetId,
        head: ChangesetId,
        onto: ChangesetId,
        merge_resolution: RebaseStackMergeResolution,
    ) -> Result<RebaseStackOutcome, MononokeError> {
        let ctx = self.ctx();
        let repo_name = self.repo().repo_identity().name();
        self.authorization_context()
            .require_repo_write(ctx, self.repo(), RepoWriteOperation::CreateChangeset)
            .await?;

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
        if base == head {
            return Err(MononokeError::InvalidRequest(
                "base and head are the same commit".to_string(),
            ));
        }
        let commit_graph = self.repo().commit_graph();
        if !commit_graph.is_ancestor(ctx, base, head).await? {
            return Err(MononokeError::InvalidRequest(format!(
                "base {base} is not an ancestor of head {head}"
            )));
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
        let stack: Vec<ChangesetId> = commit_graph
            .range_stream(ctx, base, head)
            .await?
            .filter(|cs_id| future::ready(*cs_id != base))
            .collect()
            .await;
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
            rebase_stack_onto_manifest(ctx, self.repo(), &flags, base, head, onto, max_paths)
                .await
                .map_err(|e| match e {
                    PushrebaseError::Conflicts(conflicts) => {
                        MononokeError::PushrebaseConflicts(conflicts)
                    }
                    PushrebaseError::ManifestNotDerived(cs_id) => {
                        MononokeError::ManifestNotDerived(cs_id)
                    }
                    PushrebaseError::Error(e) => MononokeError::from(e),
                    other => MononokeError::InvalidRequest(other.to_string()),
                })?;

        self.save_changesets(rebased.rebased_bonsais, self.repo(), None)
            .await?;

        let mut new_ids: HashMap<ChangesetId, ChangesetId> = rebased
            .rebased_changesets
            .iter()
            .map(|pair| (pair.id_old, pair.id_new))
            .collect();
        let dropped: HashSet<ChangesetId> = rebased.dropped.iter().copied().collect();
        let mut merged_paths = rebased.merged_paths;
        let commits = stack
            .iter()
            .map(|old| {
                let new = match new_ids.remove(old) {
                    Some(new) => Some(new),
                    None if dropped.contains(old) => None,
                    None => {
                        return Err(MononokeError::from(anyhow::anyhow!(
                            "rebase of {old} produced neither a commit nor a drop"
                        )));
                    }
                };
                Ok(RebasedStackCommit {
                    old: *old,
                    new,
                    merged_paths: merged_paths.remove(old).unwrap_or_default(),
                })
            })
            .collect::<Result<Vec<_>, MononokeError>>()?;
        let (overlapping_path_count, merged_path_count) = match rebased.merge_summary {
            MergeResolutionSummary::Succeeded {
                conflict_files_count,
                resolved_files_count,
                ..
            } => (conflict_files_count, resolved_files_count),
            _ => (0, 0),
        };
        Ok(RebaseStackOutcome {
            head: rebased.new_head,
            commits,
            overlapping_path_count,
            merged_path_count,
        })
    }
}
