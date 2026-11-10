/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use bookmarks::BookmarkKey;
use bookmarks_movement::BookmarkKindRestrictions;
use bookmarks_movement::BookmarkMovementError;
use bookmarks_movement::Repo;
use bookmarks_movement::postprocess_pushrebase_outcome;
use bookmarks_movement::prepare_pushrebase;
use bytes::Bytes;
use context::CoreContext;
use hook_manager::manager::HookManagerRef;
use hooks::CrossRepoPushSource;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use pushrebase::PushrebaseError;
use pushrebase::PushrebaseOutcome;
use pushrebase::PushrebaseQueue;
use pushrebase::PushrebaseRequest;
use repo_authorization::AuthorizationContext;
use tokio::sync::Mutex;

use crate::PushrebaseClient;

#[derive(Clone)]
pub struct BatchedPushrebaseClient<R> {
    batch_ctx: CoreContext,
    resolve_repo: Arc<dyn Fn() -> Option<Arc<R>> + Send + Sync>,
    queues: Arc<Mutex<HashMap<BookmarkKey, PushrebaseQueue>>>,
}

impl<R> BatchedPushrebaseClient<R> {
    /// Creates a client that resolves the current repository before preparing
    /// each request and executing each batch.
    pub fn new(
        batch_ctx: CoreContext,
        resolve_repo: impl Fn() -> Option<Arc<R>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            batch_ctx,
            resolve_repo: Arc::new(resolve_repo),
            queues: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait::async_trait]
impl<R: Repo + HookManagerRef + 'static> PushrebaseClient for BatchedPushrebaseClient<R> {
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
        let repo = (self.resolve_repo)().ok_or_else(|| {
            BookmarkMovementError::Error(anyhow!("Pushrebase repository is not loaded"))
        })?;
        let prepared = prepare_pushrebase(
            ctx,
            authz,
            repo.as_ref(),
            repo.hook_manager(),
            bookmark,
            changesets,
            pushvars,
            cross_repo_push_source,
            bookmark_restrictions,
        )
        .await?;
        let source_changesets = changesets.iter().cloned().collect();
        let mut stack = pushrebase::index_pushrebase_request(
            ctx,
            repo.as_ref(),
            &prepared.flags,
            bookmark,
            &source_changesets,
        )
        .await
        .map_err(BookmarkMovementError::PushrebaseError)?;
        stack
            .precompute(ctx, repo.as_ref(), &prepared.flags, bookmark)
            .await
            .map_err(BookmarkMovementError::PushrebaseError)?;
        let queue = {
            let mut queues = self.queues.lock().await;
            let resolve_repo = self.resolve_repo.clone();
            queues
                .entry(bookmark.clone())
                .or_insert_with(|| {
                    PushrebaseQueue::new(
                        self.batch_ctx.clone(),
                        repo.repo_identity().name().to_owned(),
                        bookmark.clone(),
                        move || resolve_repo(),
                    )
                })
                .clone()
        };
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        queue
            .enqueue(PushrebaseRequest {
                ctx: ctx.clone(),
                stack,
                flags: prepared.flags,
                repo_lock: prepared.repo_lock,
                response_tx,
                enqueued_at: tokio::time::Instant::now(),
            })
            .await;
        let outcome = response_rx
            .await
            .map_err(|_| {
                BookmarkMovementError::PushrebaseError(PushrebaseError::Error(anyhow!(
                    "Pushrebase queue dropped without sending a result"
                )))
            })?
            .map_err(|error| {
                BookmarkMovementError::PushrebaseError(match error.inner() {
                    PushrebaseError::Conflicts(conflicts) => {
                        PushrebaseError::Conflicts(conflicts.clone())
                    }
                    _ => PushrebaseError::Error(anyhow!(error)),
                })
            })?;

        postprocess_pushrebase_outcome(
            ctx,
            repo.as_ref(),
            bookmark,
            prepared.kind,
            &outcome,
            changesets,
            log_new_public_commits_to_scribe,
        )
        .await?;

        Ok(outcome)
    }
}
