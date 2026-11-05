/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;

use anyhow::anyhow;
use bookmarks::BookmarkUpdateReason;
use bookmarks_types::BookmarkKey;
use bookmarks_types::BookmarkKind;
use bytes::Bytes;
use context::CoreContext;
use hooks::CrossRepoPushSource;
use hooks::HookManager;
use metaconfig_types::LAND_INSTANCE_ID_PUSHVAR_KEY;
use metaconfig_types::MAINLINE_PARENT_FIRST_PUSHVAR_KEY;
use metaconfig_types::MERGE_TAKE_INCOMING_PUSHVAR_KEY;
use metaconfig_types::MergeResolutionOverride;
use metaconfig_types::PHAB_DIFF_ID_PUSHVAR_KEY;
use metaconfig_types::PushrebaseFlags;
use metaconfig_types::RepoConfigRef;
use metaconfig_types::merge_take_incoming_from_pushvar_value;
use mononoke_types::BonsaiChangeset;
use pushrebase::RepoLockPolicy;
use repo_authorization::AuthorizationContext;
use repo_authorization::RepoWriteOperation;
use repo_bookmark_attrs::RepoBookmarkAttrsRef;
use repo_update_logger::BookmarkInfo;
use repo_update_logger::BookmarkOperation;
use repo_update_logger::CommitInfo;
use repo_update_logger::log_bookmark_operation;
use repo_update_logger::log_new_commits;

use crate::BookmarkMovementError;
use crate::Repo;
use crate::affected_changesets::AdditionalChangesets;
use crate::affected_changesets::AffectedChangesets;
use crate::repo_lock::check_repo_lock;
use crate::restrictions::BookmarkKindRestrictions;
use crate::restrictions::check_bookmark_sync_config;

/// Authorization and request policy needed to execute a pushrebase.
pub struct PushrebasePreparation {
    pub flags: PushrebaseFlags,
    pub kind: BookmarkKind,
    pub repo_lock: RepoLockPolicy,
}

/// Returns the configured pushrebase flags with bookmark and request overrides.
pub fn pushrebase_flags(
    repo: &(impl RepoConfigRef + RepoBookmarkAttrsRef),
    bookmark: &BookmarkKey,
    pushvars: Option<&HashMap<String, Bytes>>,
) -> PushrebaseFlags {
    let mut flags = repo.repo_config().pushrebase.flags.clone();
    if let Some(rewritedates) = repo.repo_bookmark_attrs().should_rewrite_dates(bookmark) {
        flags.rewritedates = rewritedates;
    }

    flags.merge_resolution_override = MergeResolutionOverride::from_pushvar_value(
        pushvars
            .and_then(|p| p.get(MergeResolutionOverride::PUSHVAR_KEY))
            .map(|b| b.as_ref()),
    );
    flags.merge_resolution_take_incoming = merge_take_incoming_from_pushvar_value(
        pushvars
            .and_then(|p| p.get(MERGE_TAKE_INCOMING_PUSHVAR_KEY))
            .map(|b| b.as_ref()),
    );
    flags.land_instance_id = pushvars
        .and_then(|p| p.get(LAND_INSTANCE_ID_PUSHVAR_KEY))
        .and_then(|b| std::str::from_utf8(b.as_ref()).ok())
        .map(str::to_owned);
    flags.phab_diff_id = pushvars
        .and_then(|p| p.get(PHAB_DIFF_ID_PUSHVAR_KEY))
        .and_then(|b| std::str::from_utf8(b.as_ref()).ok())
        .map(str::to_owned);
    flags.mainline_parent_first = pushvars
        .and_then(|p| p.get(MAINLINE_PARENT_FIRST_PUSHVAR_KEY))
        .is_some_and(|b| matches!(b.as_ref(), b"true" | b"1"));
    flags
}

/// Authorizes and prepares a pushrebase request for direct or batched execution.
pub async fn prepare_pushrebase(
    ctx: &CoreContext,
    authz: &AuthorizationContext,
    repo: &impl Repo,
    hook_manager: &HookManager,
    bookmark: &BookmarkKey,
    changesets: &[BonsaiChangeset],
    pushvars: Option<&HashMap<String, Bytes>>,
    cross_repo_push_source: CrossRepoPushSource,
    bookmark_restrictions: BookmarkKindRestrictions,
) -> Result<PushrebasePreparation, BookmarkMovementError> {
    let kind = bookmark_restrictions.check_kind(repo, bookmark)?;

    authz
        .require_repo_write(ctx, repo, RepoWriteOperation::LandStack(kind))
        .await?;
    authz.require_bookmark_modify(ctx, repo, bookmark).await?;

    check_bookmark_sync_config(ctx, repo, bookmark, kind).await?;

    let max_commits = justknobs::get_as::<usize>(
        "scm/mononoke:pushrebase_max_commits_per_request",
        Some(repo.repo_identity().name()),
    );
    if changesets.len() > max_commits {
        return Err(anyhow!(
            "Land requests are limited to {max_commits} commits; received {}. Split the stack into smaller land requests.",
            changesets.len(),
        )
        .into());
    }

    if repo.repo_config().pushrebase.block_merges
        && changesets.iter().any(BonsaiChangeset::is_merge)
    {
        return Err(BookmarkMovementError::MergesBlocked);
    }

    let reason = BookmarkUpdateReason::Pushrebase;

    AffectedChangesets::with_source_changesets(changesets)
        .check_restrictions(
            ctx,
            authz,
            repo,
            hook_manager,
            bookmark,
            pushvars,
            reason,
            kind,
            AdditionalChangesets::None,
            cross_repo_push_source,
            None,
        )
        .await?;

    // For pushrebase, we check the repo lock once at the beginning of the
    // pushrebase operation, and then once more as part of the pushrebase
    // bookmark update transaction, to check if the repo got locked while
    // we were performing the pushrebase.
    let repo_lock = check_repo_lock(
        ctx,
        repo,
        kind,
        pushvars,
        ctx.metadata().identities(),
        authz,
    )
    .await?;

    Ok(PushrebasePreparation {
        flags: pushrebase_flags(repo, bookmark, pushvars),
        kind,
        repo_lock,
    })
}

/// Performs all post-pushrebase work: scribe logging, bookmark operation
/// logging, and phase marking.
pub async fn postprocess_pushrebase_outcome(
    ctx: &CoreContext,
    repo: &impl Repo,
    bookmark: &BookmarkKey,
    kind: BookmarkKind,
    outcome: &pushrebase::PushrebaseOutcome,
    source_changesets: &[BonsaiChangeset],
    log_new_public_commits_to_scribe: bool,
) -> Result<(), BookmarkMovementError> {
    if log_new_public_commits_to_scribe {
        let mut changesets_to_log: HashMap<_, _> = source_changesets
            .iter()
            .map(|bcs| (bcs.get_changeset_id(), CommitInfo::new(bcs, None)))
            .collect();

        for pair in outcome.rebased_changesets.iter() {
            let info = changesets_to_log
                .get_mut(&pair.id_old)
                .ok_or_else(|| anyhow!("Missing commit info for {}", pair.id_old))?;
            info.update_changeset_id(pair.id_old, pair.id_new)?;
        }

        log_new_commits(
            ctx,
            repo,
            Some((bookmark, kind)),
            changesets_to_log.into_values().collect(),
        )
        .await;
    }

    let reason = BookmarkUpdateReason::Pushrebase;
    let info = BookmarkInfo {
        bookmark_name: bookmark.clone(),
        bookmark_kind: kind,
        operation: BookmarkOperation::Pushrebase(outcome.old_bookmark_value, outcome.head),
        reason,
    };
    log_bookmark_operation(ctx, repo, &info).await;

    // Marking the pushrebased changeset as public.
    if kind.is_public() {
        repo.phases()
            .add_reachable_as_public(ctx, vec![outcome.head.clone()])
            .await?;
    }

    Ok(())
}
