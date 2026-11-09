/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use context::CoreContext;
use futures_watchdog::WatchdogExt;
use mononoke_api::ChangesetSpecifier;
use mononoke_api::repo::check_rebase_conflicts::CheckRebaseConflictsStatus;
use mononoke_api::repo::rebase_stack::RebaseStackMergeResolution;
use scs_errors::ServiceErrorResultExt;
use source_control as thrift;

use crate::commit_id::map_commit_identities;
use crate::from_request::FromRequest;
use crate::source_control_impl::SourceControlServiceImpl;

impl SourceControlServiceImpl {
    pub(crate) async fn repo_check_rebase_conflicts(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        params: thrift::RepoCheckRebaseConflictsParams,
    ) -> Result<thrift::RepoCheckRebaseConflictsResponse, scs_errors::ServiceError> {
        let repo = self.repo(ctx.clone(), &repo).watched().await?;
        let head = repo
            .changeset(ChangesetSpecifier::from_request(&params.head)?)
            .watched()
            .await
            .context("failed to resolve head commit")?
            .ok_or_else(|| scs_errors::commit_not_found(params.head.to_string()))?;
        let merge_resolution = match params.merge_resolution {
            thrift::RepoRebaseStackMergeResolution::DEFAULT => RebaseStackMergeResolution::Default,
            thrift::RepoRebaseStackMergeResolution::DISABLED => {
                RebaseStackMergeResolution::Disabled
            }
            other => {
                return Err(scs_errors::invalid_request(format!(
                    "unknown merge_resolution {other:?}"
                ))
                .into());
            }
        };

        let outcome = repo
            .check_rebase_conflicts(head.id(), &params.target_bookmark, merge_resolution)
            .watched()
            .await?;

        let target_ids = map_commit_identities(&repo, vec![outcome.onto], &params.identity_schemes)
            .watched()
            .await?
            .remove(&outcome.onto)
            .unwrap_or_default();
        Ok(thrift::RepoCheckRebaseConflictsResponse {
            status: match outcome.status {
                CheckRebaseConflictsStatus::Clean => thrift::RepoCheckRebaseConflictsStatus::CLEAN,
                CheckRebaseConflictsStatus::Conflicts => {
                    thrift::RepoCheckRebaseConflictsStatus::CONFLICT
                }
                CheckRebaseConflictsStatus::AlreadyLanded => {
                    thrift::RepoCheckRebaseConflictsStatus::ALREADY_LANDED
                }
            },
            conflicts: outcome
                .conflicts
                .into_iter()
                .map(|c| thrift::PushrebaseConflict {
                    left: c.left.to_string(),
                    right: c.right.to_string(),
                    ..Default::default()
                })
                .collect(),
            target_ids,
            overlapping_path_count: outcome.overlapping_path_count as i64,
            merged_path_count: outcome.merged_path_count as i64,
            ..Default::default()
        })
    }
}
