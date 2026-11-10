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
use bytes::Bytes;
use context::CoreContext;
use hooks::CrossRepoPushSource;
use hooks::HookManager;
use metaconfig_types::Address;
use metaconfig_types::PushrebaseRemoteMode;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use pushrebase::PushrebaseOutcome;
use repo_authorization::AuthorizationContext;
use scuba_ext::MononokeScubaSampleBuilder;

use crate::LandServicePushrebaseClient;
use crate::PushrebaseClient;
use crate::local::LocalPushrebaseClient;

pub async fn normal_pushrebase<'a>(
    ctx: &'a CoreContext,
    repo: &'a impl Repo,
    changesets: &[BonsaiChangeset],
    bookmark: &'a BookmarkKey,
    maybe_pushvars: Option<&'a HashMap<String, Bytes>>,
    hook_manager: &'a HookManager,
    cross_repo_push_source: CrossRepoPushSource,
    bookmark_restrictions: BookmarkKindRestrictions,
    authz: &'a AuthorizationContext,
    log_new_public_commits_to_scribe: bool,
    force_local_pushrebase: bool,
    required_root: Option<ChangesetId>,
) -> Result<PushrebaseOutcome, BookmarkMovementError> {
    let remote_mode = if force_local_pushrebase {
        PushrebaseRemoteMode::Local
    } else {
        repo.repo_config().pushrebase.remote_mode.clone()
    };
    let maybe_fallback_scuba: Option<(MononokeScubaSampleBuilder, BookmarkMovementError)> = {
        let maybe_client: Option<Box<dyn PushrebaseClient>> =
            maybe_client_from_address(&remote_mode, ctx, repo).await?;

        if let Some(client) = maybe_client {
            let result = client
                .pushrebase(
                    ctx,
                    authz,
                    bookmark,
                    changesets,
                    maybe_pushvars,
                    cross_repo_push_source,
                    bookmark_restrictions,
                    log_new_public_commits_to_scribe,
                    required_root,
                )
                .await;
            match (result, &remote_mode) {
                (Ok(outcome), _) => {
                    return Ok(outcome);
                }
                // No fallback, propagate error
                (Err(err), metaconfig_types::PushrebaseRemoteMode::RemoteLandService(..)) => {
                    return Err(err);
                }
                (Err(err), _) => {
                    // Warning: Failed to pushrebase remotely, falling back to local
                    eprintln!("Failed to pushrebase remotely, falling back to local. Error: {err}",);
                    let mut scuba = ctx.scuba().clone();
                    scuba.add("bookmark_name", bookmark.as_str());
                    scuba.add(
                        "changeset_id",
                        changesets
                            .iter()
                            .next()
                            .map(|b| b.get_changeset_id().to_string()),
                    );
                    Some((scuba, err))
                }
            }
        } else {
            None
        }
    };
    let result = LocalPushrebaseClient { repo, hook_manager }
        .pushrebase(
            ctx,
            authz,
            bookmark,
            changesets,
            maybe_pushvars,
            cross_repo_push_source,
            bookmark_restrictions,
            log_new_public_commits_to_scribe,
            required_root,
        )
        .await;
    if let Some((mut scuba, err)) = maybe_fallback_scuba {
        if result.is_ok() {
            scuba.log_with_msg("failed_remote_pushrebase", err.to_string());
        }
    }

    result
}

async fn maybe_client_from_address<'a>(
    remote_mode: &'a PushrebaseRemoteMode,
    ctx: &'a CoreContext,
    repo: &'a impl Repo,
) -> anyhow::Result<Option<Box<dyn PushrebaseClient + 'a>>> {
    match remote_mode {
        PushrebaseRemoteMode::RemoteLandService(address)
        | PushrebaseRemoteMode::RemoteLandServiceWithLocalFallback(address) => {
            Ok(address_from_land_service(address, ctx, repo).await?)
        }
        PushrebaseRemoteMode::Local => Ok(None),
    }
}

async fn address_from_land_service<'a>(
    address: &'a Address,
    ctx: &'a CoreContext,
    repo: &'a impl Repo,
) -> anyhow::Result<Option<Box<dyn PushrebaseClient + 'a>>> {
    match address {
        metaconfig_types::Address::Tier(tier) => Ok(Some(Box::new(
            LandServicePushrebaseClient::from_tier(ctx, tier.clone(), repo).await?,
        ))),
        metaconfig_types::Address::HostPort(host_port) => Ok(Some(Box::new(
            LandServicePushrebaseClient::from_host_port(ctx, host_port.clone(), repo).await?,
        ))),
    }
}
