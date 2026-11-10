/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use blobstore::Loadable;
use bookmarks::BookmarkKey;
use bookmarks_movement::BookmarkKindRestrictions;
use bytes::Bytes;
use context::CoreContext;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use futures_ext::future::FbFutureExt;
use futures_stats::FutureStats;
use futures_stats::TimedFutureExt;
use hook_manager::manager::HookManagerRef;
use hooks::CrossRepoPushSource;
use land_service_if::*;
use mononoke_types::BonsaiChangeset;
use pushrebase_client::BatchedPushrebaseClient;
use pushrebase_client::LocalPushrebaseClient;
use pushrebase_client::PushrebaseClient;
use repo_blobstore::RepoBlobstoreRef;
use stats::prelude::*;
use time_ext::DurationExt;

use crate::errors;
use crate::errors::LandChangesetsError;
use crate::platform::authorization_context;
use crate::repo::Repo;

define_stats! {
    prefix = "mononoke.land_service";
    total_request_start: timeseries(Rate, Sum),
    total_request_success: timeseries(Rate, Sum),
    total_request_internal_failure: timeseries(Rate, Sum),
    total_request_canceled: timeseries(Rate, Sum),
    // Duration per changesets landed with and without batches
    method_completion_time_ms: dynamic_quantile_stat("method.{}.completion_time_ms", (method: String); Average, Sum, Count; P 5, P 50, P 90; Duration::from_secs(60), Duration::from_secs(600), Duration::from_secs(3600)),
}

pub async fn impl_land_changesets(
    ctx: CoreContext,
    repo: Arc<Repo>,
    request: LandChangesetRequest,
) -> Result<LandChangesetsResponse, LandChangesetsError> {
    ctx.scuba().clone().log_with_msg("Request start", None);
    STATS::total_request_start.add_value(1);

    let (stats, res) = process_land_changesets_request(&ctx, &repo, request.clone())
        .timed()
        .on_cancel_with_data(|stats| log_canceled(&ctx, request.repo_name.as_str(), &stats))
        .await;
    log_result(ctx, &request, &stats, &res);
    STATS::method_completion_time_ms.add_value(
        stats.completion_time.as_millis_unchecked() as i64,
        ("impl_land_changesets".to_string(),),
    );
    res
}

async fn process_land_changesets_request(
    ctx: &CoreContext,
    repo: &Repo,
    request: LandChangesetRequest,
) -> Result<LandChangesetsResponse, LandChangesetsError> {
    let authz = authorization_context(ctx, request.service_identity)?;

    let bookmark = BookmarkKey::new(request.bookmark)?;
    let changesets: Vec<BonsaiChangeset> = stream::iter(request.changesets)
        .map(async |cs_id| {
            mononoke_types::ChangesetId::from_bytes(cs_id)?
                .load(ctx, repo.repo_blobstore())
                .await
        })
        .buffer_unordered(100)
        .try_collect()
        .await?;

    let pushvars = request
        .pushvars
        .unwrap_or_default()
        .into_iter()
        .map(|(name, value)| (name, Bytes::from(value)))
        .collect();

    let cross_repo_push_source = match request.cross_repo_push_source {
        land_service_if::CrossRepoPushSource::NATIVE_TO_THIS_REPO => {
            CrossRepoPushSource::NativeToThisRepo
        }
        land_service_if::CrossRepoPushSource::PUSH_REDIRECTED => {
            CrossRepoPushSource::PushRedirected
        }
        other => {
            return Err(LandChangesetsError::InternalError(errors::internal_error(
                anyhow!("Unknown CrossRepoPushSource: {other}").as_ref(),
            )));
        }
    };

    let bookmark_restrictions = match request.bookmark_restrictions {
        land_service_if::BookmarkKindRestrictions::ANY_KIND => BookmarkKindRestrictions::AnyKind,
        land_service_if::BookmarkKindRestrictions::ONLY_SCRATCH => {
            BookmarkKindRestrictions::OnlyScratch
        }
        land_service_if::BookmarkKindRestrictions::ONLY_PUBLISHING => {
            BookmarkKindRestrictions::OnlyPublishing
        }
        other => {
            return Err(LandChangesetsError::InternalError(errors::internal_error(
                anyhow!("Unknown BookmarkKindRestrictions: {other}").as_ref(),
            )));
        }
    };

    let outcome = LocalPushrebaseClient {
        repo,
        hook_manager: repo.hook_manager(),
    }
    .pushrebase(
        ctx,
        &authz,
        &bookmark,
        &changesets,
        Some(&pushvars),
        cross_repo_push_source,
        bookmark_restrictions,
        request.log_new_public_commits_to_scribe,
        None,
    )
    .await?;

    Ok(land_changesets_response(outcome))
}

fn land_changesets_response(outcome: pushrebase::PushrebaseOutcome) -> LandChangesetsResponse {
    LandChangesetsResponse {
        pushrebase_outcome: PushrebaseOutcome {
            head: outcome.head.as_ref().to_vec(),
            rebased_changesets: outcome
                .rebased_changesets
                .into_iter()
                .map(|rebased_changeset| BonsaiHashPairs {
                    old_id: rebased_changeset.id_old.as_ref().to_vec(),
                    new_id: rebased_changeset.id_new.as_ref().to_vec(),
                })
                .collect(),
            pushrebase_distance: outcome.pushrebase_distance.0 as i64,
            retry_num: outcome.retry_num.0 as i64,
            old_bookmark_value: outcome
                .old_bookmark_value
                .map(|cs_id| cs_id.as_ref().to_vec()),
            log_id: Some(
                outcome
                    .log_id
                    .try_into()
                    .expect("log id should be convertible to i64"),
            ),
        },
    }
}

fn log_result(
    ctx: CoreContext,
    request: &LandChangesetRequest,
    stats: &FutureStats,
    result: &Result<LandChangesetsResponse, LandChangesetsError>,
) {
    let mut scuba = ctx.scuba().clone();

    match result {
        Ok(response) => {
            scuba.add(
                "new_head",
                faster_hex::hex_string(&response.pushrebase_outcome.head),
            );
            scuba.add(
                "pushrebase_distance",
                response.pushrebase_outcome.pushrebase_distance,
            );
            scuba.add("retry_num", response.pushrebase_outcome.retry_num);
            scuba.add(
                "rebased_changesets",
                response.pushrebase_outcome.rebased_changesets.len(),
            );
            if let Some(log_id) = response.pushrebase_outcome.log_id {
                scuba.add("bookmark_log_id", log_id);
            }
            if let Some(old_bookmark_value) = &response.pushrebase_outcome.old_bookmark_value {
                scuba.add(
                    "old_bookmark_value",
                    faster_hex::hex_string(old_bookmark_value),
                );
            }

            STATS::total_request_success.add_value(1);
            scuba.add("status", "SUCCESS");
        }
        Err(err) => {
            STATS::total_request_internal_failure.add_value(1);
            scuba.add("status", "INTERNAL_ERROR");
            scuba.add("error", err.to_string());
        }
    };

    ctx.perf_counters().insert_perf_counters(&mut scuba);
    scuba.add("repo_name", request.repo_name.as_str());
    scuba.add("bookmark", request.bookmark.as_str());
    if let Some(service_identity) = &request.service_identity {
        scuba.add("service_identity", service_identity.as_str());
    }
    scuba.add_future_stats(stats);
    scuba.log_with_msg("Request complete", None);
}

fn log_canceled(ctx: &CoreContext, repo_name: &str, stats: &FutureStats) {
    STATS::total_request_canceled.add_value(1);
    let mut scuba = ctx.scuba().clone();
    ctx.perf_counters().insert_perf_counters(&mut scuba);
    scuba.add("repo_name", repo_name.to_owned());
    scuba.add_future_stats(stats);
    scuba.add("status", "CANCELED");
    scuba.log_with_msg("Request canceled", None);
}

pub async fn impl_land_changesets_batched(
    ctx: CoreContext,
    repo: Arc<Repo>,
    client: BatchedPushrebaseClient<Repo>,
    request: LandChangesetRequest,
) -> Result<LandChangesetsResponse, LandChangesetsError> {
    ctx.scuba()
        .clone()
        .log_with_msg("Batched request start", None);
    STATS::total_request_start.add_value(1);

    let (stats, res) = process_batched_request(&ctx, &repo, &client, request.clone())
        .timed()
        .on_cancel_with_data(|stats| log_canceled(&ctx, request.repo_name.as_str(), &stats))
        .await;
    log_result(ctx, &request, &stats, &res);
    STATS::method_completion_time_ms.add_value(
        stats.completion_time.as_millis_unchecked() as i64,
        ("impl_land_changesets_batched".to_string(),),
    );
    res
}

async fn process_batched_request(
    ctx: &CoreContext,
    repo: &Arc<Repo>,
    client: &BatchedPushrebaseClient<Repo>,
    request: LandChangesetRequest,
) -> Result<LandChangesetsResponse, LandChangesetsError> {
    let authz = authorization_context(ctx, request.service_identity)?;

    let bookmark = BookmarkKey::new(&request.bookmark)?;

    let cross_repo_push_source = match request.cross_repo_push_source {
        land_service_if::CrossRepoPushSource::NATIVE_TO_THIS_REPO => {
            CrossRepoPushSource::NativeToThisRepo
        }
        land_service_if::CrossRepoPushSource::PUSH_REDIRECTED => {
            CrossRepoPushSource::PushRedirected
        }
        other => {
            return Err(LandChangesetsError::InternalError(errors::internal_error(
                anyhow!("Unknown CrossRepoPushSource: {other}").as_ref(),
            )));
        }
    };

    let bookmark_restrictions = match request.bookmark_restrictions {
        land_service_if::BookmarkKindRestrictions::ANY_KIND => BookmarkKindRestrictions::AnyKind,
        land_service_if::BookmarkKindRestrictions::ONLY_SCRATCH => {
            BookmarkKindRestrictions::OnlyScratch
        }
        land_service_if::BookmarkKindRestrictions::ONLY_PUBLISHING => {
            BookmarkKindRestrictions::OnlyPublishing
        }
        other => {
            return Err(LandChangesetsError::InternalError(errors::internal_error(
                anyhow!("Unknown BookmarkKindRestrictions: {other}").as_ref(),
            )));
        }
    };

    let changesets: Vec<BonsaiChangeset> = stream::iter(request.changesets)
        .map(async |cs_id| {
            mononoke_types::ChangesetId::from_bytes(cs_id)?
                .load(ctx, repo.repo_blobstore())
                .await
        })
        .buffer_unordered(100)
        .try_collect()
        .await?;

    let pushvars: std::collections::HashMap<String, Bytes> = request
        .pushvars
        .unwrap_or_default()
        .into_iter()
        .map(|(name, value)| (name, Bytes::from(value)))
        .collect();

    let outcome = client
        .pushrebase(
            ctx,
            &authz,
            &bookmark,
            &changesets,
            Some(&pushvars),
            cross_repo_push_source,
            bookmark_restrictions,
            request.log_new_public_commits_to_scribe,
            None,
        )
        .await?;

    Ok(land_changesets_response(outcome))
}
