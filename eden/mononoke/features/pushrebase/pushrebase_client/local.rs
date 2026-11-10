/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;

use bookmarks::BookmarkKey;
use bookmarks_movement::BookmarkKindRestrictions;
use bookmarks_movement::BookmarkMovementError;
use bookmarks_movement::Repo;
use bookmarks_movement::postprocess_pushrebase_outcome;
use bookmarks_movement::prepare_pushrebase;
use bytes::Bytes;
use context::CoreContext;
use futures_stats::TimedFutureExt;
use hooks::CrossRepoPushSource;
use hooks::HookManager;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use pushrebase::PushrebaseOutcome;
use pushrebase::RepoLockPolicy;
use pushrebase_hooks::RepoLockPushrebaseHook;
use pushrebase_hooks::get_pushrebase_hooks;
use repo_authorization::AuthorizationContext;

use crate::PushrebaseClient;

pub struct LocalPushrebaseClient<'a, R: Repo> {
    pub repo: &'a R,
    pub hook_manager: &'a HookManager,
}

#[async_trait::async_trait]
impl<'a, R: Repo> PushrebaseClient for LocalPushrebaseClient<'a, R> {
    async fn pushrebase(
        &self,
        ctx: &CoreContext,
        authz: &AuthorizationContext,
        bookmark: &BookmarkKey,
        changesets: &[BonsaiChangeset],
        pushvars: Option<&HashMap<String, Bytes>>,
        cross_repo_push_source: CrossRepoPushSource,
        bookmark_restrictions: BookmarkKindRestrictions,
        log_new_public_commits_to_scribe: bool,
        _required_root: Option<ChangesetId>,
    ) -> Result<PushrebaseOutcome, BookmarkMovementError> {
        let prepared = prepare_pushrebase(
            ctx,
            authz,
            self.repo,
            self.hook_manager,
            bookmark,
            changesets,
            pushvars,
            cross_repo_push_source,
            bookmark_restrictions,
        )
        .await?;
        let mut hooks = get_pushrebase_hooks(
            ctx,
            self.repo,
            bookmark,
            &self.repo.repo_config().pushrebase,
            None,
        )
        .await?;
        if prepared.repo_lock == RepoLockPolicy::Enforce {
            hooks.push(RepoLockPushrebaseHook::new(self.repo.repo_identity().id()));
        }
        let source_changesets = changesets.iter().cloned().collect();

        ctx.scuba()
            .clone()
            .add("bookmark", bookmark.to_string())
            .log_with_msg("Pushrebase started", None);
        let (stats, result) = pushrebase::do_pushrebase_bonsai(
            ctx,
            self.repo,
            &prepared.flags,
            bookmark,
            &source_changesets,
            &hooks,
            None,
        )
        .timed()
        .await;

        let mut scuba = ctx.scuba().clone();
        scuba.add_future_stats(&stats);
        match &result {
            Ok(outcome) => {
                scuba
                    .add("pushrebase_retry_num", outcome.retry_num.0)
                    .add("pushrebase_distance", outcome.pushrebase_distance.0)
                    .add("bookmark", bookmark.to_string())
                    .add("changeset_id", outcome.head.to_string());
                if let Some(paths) = &outcome.merge_resolved_paths {
                    scuba.add("merge_resolved_count", paths.len()).add(
                        "merge_resolved_paths",
                        paths
                            .iter()
                            .take(10)
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(","),
                    );
                }
                scuba.log_with_msg("Pushrebase finished", None);

                postprocess_pushrebase_outcome(
                    ctx,
                    self.repo,
                    bookmark,
                    prepared.kind,
                    outcome,
                    changesets,
                    log_new_public_commits_to_scribe,
                )
                .await?;
            }
            Err(err) => {
                if let pushrebase::PushrebaseError::Conflicts(conflicts) = err {
                    scuba.add("conflict_count", conflicts.len()).add(
                        "conflict_paths",
                        conflicts
                            .iter()
                            .take(10)
                            .map(|conflict| format!("{}={}", conflict.left, conflict.right))
                            .collect::<Vec<_>>()
                            .join(","),
                    );
                }
                scuba.log_with_msg("Pushrebase failed", Some(format!("{err:#?}")));
            }
        }

        result.map_err(BookmarkMovementError::PushrebaseError)
    }
}
