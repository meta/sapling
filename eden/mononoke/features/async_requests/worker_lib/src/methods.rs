/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! The concrete method implementations.
//!
//! This module provides the concrete implementations for methods - functions
//! taking the Params and returning the Results - to be used by the request worker.
//! This module is not aware of the async nature of those methods. All the token
//! handling, enqueuing and polling should be done by the callers.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use anyhow::bail;
use async_requests::AsyncMethodRequestQueue;
use async_requests::AsyncRequestsError;
use async_requests::types::AsynchronousRequestParams;
use async_requests::types::AsynchronousRequestResult;
use async_requests::types::IntoConfigFormat;
use async_requests::types::RowId;
use async_requests::types::ThriftAsynchronousRequestResult;
use context::CoreContext;
#[cfg(fbcode_build)]
use ephemeral_blobstore::BubbleId;
#[cfg(fbcode_build)]
use ephemeral_blobstore::RepoEphemeralStore;
#[cfg(fbcode_build)]
use futures::Future;
#[cfg(fbcode_build)]
use futures::FutureExt;
#[cfg(fbcode_build)]
use futures::future::BoxFuture;
#[cfg(fbcode_build)]
use futures::try_join;
use futures_watchdog::WatchdogExt;
use megarepo_api::MegarepoApi;
use megarepo_error::MegarepoError;
use mononoke_api::ChangesetContext;
#[cfg(fbcode_build)]
use mononoke_api::ChangesetSpecifier;
use mononoke_api::Mononoke;
use mononoke_api::MononokeRepo;
use mononoke_api::Repo;
use mononoke_api::RepoContext;
use mononoke_types::ChangesetId;
#[cfg(fbcode_build)]
use repo_authorization::AuthorizationContext;
#[cfg(fbcode_build)]
use scs_errors::ServiceErrorResultExt;
#[cfg(fbcode_build)]
use scs_methods::commit_sparse_profile_info::commit_sparse_profile_delta_impl;
#[cfg(fbcode_build)]
use scs_methods::commit_sparse_profile_info::commit_sparse_profile_size_impl;
#[cfg(fbcode_build)]
use scs_methods::from_request::FromRequest;
#[cfg(fbcode_build)]
use scs_methods::specifiers::SpecifierExt;
use source_control as thrift;
#[cfg(fbcode_build)]
use source_control::CommitSpecifier;

const METHOD_MAX_POLL_TIME_MS: u64 = 100;

#[cfg(not(fbcode_build))]
pub async fn commit_sparse_profile_delta_impl(
    _ctx: &CoreContext,
    _repo: RepoContext<Repo>,
    _changeset: ChangesetContext<Repo>,
    _other: ChangesetContext<Repo>,
    _profiles: thrift::SparseProfiles,
) -> Result<thrift::CommitSparseProfileDeltaResponse, AsyncRequestsError> {
    Err(AsyncRequestsError::internal(anyhow::anyhow!(
        "not implemented in non-fbcode build"
    )))
}

#[cfg(not(fbcode_build))]
pub async fn commit_sparse_profile_size_impl(
    _ctx: &CoreContext,
    _repo: RepoContext<Repo>,
    _changeset: ChangesetContext<Repo>,
    _profiles: thrift::SparseProfiles,
) -> Result<thrift::CommitSparseProfileSizeResponse, AsyncRequestsError> {
    Err(AsyncRequestsError::internal(anyhow::anyhow!(
        "not implemented in non-fbcode build"
    )))
}

async fn megarepo_sync_changeset<R: MononokeRepo>(
    ctx: &CoreContext,
    megarepo_api: &MegarepoApi<R>,
    params: thrift::MegarepoSyncChangesetParams,
) -> Result<thrift::MegarepoSyncChangesetResponse, MegarepoError> {
    let source_cs_id = ChangesetId::from_bytes(params.cs_id).map_err(MegarepoError::request)?;
    let target_location =
        ChangesetId::from_bytes(params.target_location).map_err(MegarepoError::request)?;
    let cs_id = megarepo_api
        .sync_changeset(
            ctx,
            source_cs_id,
            params.source_name,
            params.target.into_config_format(&megarepo_api.mononoke())?,
            target_location,
        )
        .watched()
        .await?
        .as_ref()
        .into();
    Ok(thrift::MegarepoSyncChangesetResponse {
        cs_id,
        ..Default::default()
    })
}

async fn megarepo_add_sync_target<R: MononokeRepo>(
    ctx: &CoreContext,
    megarepo_api: &MegarepoApi<R>,
    params: thrift::MegarepoAddTargetParams,
) -> Result<thrift::MegarepoAddTargetResponse, MegarepoError> {
    let config = params.config_with_new_target;
    let mut changesets_to_merge = HashMap::new();
    for (s, cs_id) in params.changesets_to_merge {
        let cs_id = ChangesetId::from_bytes(cs_id).map_err(MegarepoError::request)?;
        changesets_to_merge.insert(s, cs_id);
    }
    let cs_id = megarepo_api
        .add_sync_target(
            ctx,
            config.into_config_format(&megarepo_api.mononoke())?,
            changesets_to_merge,
            params.message,
        )
        .await?
        .as_ref()
        .into();
    Ok(thrift::MegarepoAddTargetResponse {
        cs_id,
        ..Default::default()
    })
}

async fn megarepo_add_branching_sync_target<R: MononokeRepo>(
    ctx: &CoreContext,
    megarepo_api: &MegarepoApi<R>,
    params: thrift::MegarepoAddBranchingTargetParams,
) -> Result<thrift::MegarepoAddBranchingTargetResponse, MegarepoError> {
    let cs_id = megarepo_api
        .add_branching_sync_target(
            ctx,
            params.target.into_config_format(&megarepo_api.mononoke())?,
            ChangesetId::from_bytes(params.branching_point).map_err(MegarepoError::request)?,
            params
                .source_target
                .into_config_format(&megarepo_api.mononoke())?,
        )
        .await?
        .as_ref()
        .into();
    Ok(thrift::MegarepoAddBranchingTargetResponse {
        cs_id,
        ..Default::default()
    })
}

async fn megarepo_change_target_config<R: MononokeRepo>(
    ctx: &CoreContext,
    megarepo_api: &MegarepoApi<R>,
    params: thrift::MegarepoChangeTargetConfigParams,
) -> Result<thrift::MegarepoChangeTargetConfigResponse, MegarepoError> {
    let mut changesets_to_merge = HashMap::new();
    for (s, cs_id) in params.changesets_to_merge {
        let cs_id = ChangesetId::from_bytes(cs_id).map_err(MegarepoError::request)?;
        changesets_to_merge.insert(s, cs_id);
    }
    let target_location =
        ChangesetId::from_bytes(params.target_location).map_err(MegarepoError::request)?;
    let cs_id = megarepo_api
        .change_target_config(
            ctx,
            params.target.into_config_format(&megarepo_api.mononoke())?,
            params.new_version,
            target_location,
            changesets_to_merge,
            params.message,
        )
        .await?
        .as_ref()
        .into();
    Ok(thrift::MegarepoChangeTargetConfigResponse {
        cs_id,
        ..Default::default()
    })
}

async fn megarepo_remerge_source<R: MononokeRepo>(
    ctx: &CoreContext,
    megarepo_api: &MegarepoApi<R>,
    params: thrift::MegarepoRemergeSourceParams,
) -> Result<thrift::MegarepoRemergeSourceResponse, MegarepoError> {
    let remerge_cs_id = ChangesetId::from_bytes(params.cs_id).map_err(MegarepoError::request)?;
    let target_location =
        ChangesetId::from_bytes(params.target_location).map_err(MegarepoError::request)?;
    let cs_id = megarepo_api
        .remerge_source(
            ctx,
            params.source_name,
            remerge_cs_id,
            params.message,
            &params.target.into_config_format(&megarepo_api.mononoke())?,
            target_location,
        )
        .await?
        .as_ref()
        .into();

    Ok(thrift::MegarepoRemergeSourceResponse {
        cs_id,
        ..Default::default()
    })
}

#[cfg(fbcode_build)]
pub async fn commit_sparse_profile_size(
    ctx: &CoreContext,
    mononoke: Arc<Mononoke<Repo>>,
    params: thrift::CommitSparseProfileSizeParamsV2,
) -> Result<thrift::CommitSparseProfileSizeResponse, AsyncRequestsError> {
    let (repo, changeset) = get_repo_and_changeset(ctx, mononoke, &params.commit)
        .await
        .map_err(<scs_errors::ServiceError as Into<AsyncRequestsError>>::into)?;

    commit_sparse_profile_size_impl(ctx, repo, changeset, params.profiles)
        .await
        .map_err(<scs_errors::ServiceError as Into<AsyncRequestsError>>::into)
}

#[cfg(fbcode_build)]
pub async fn commit_sparse_profile_delta(
    ctx: &CoreContext,
    mononoke: Arc<Mononoke<Repo>>,
    params: thrift::CommitSparseProfileDeltaParamsV2,
) -> Result<thrift::CommitSparseProfileDeltaResponse, AsyncRequestsError> {
    let (repo, changeset, other) =
        repo_changeset_pair(ctx.clone(), mononoke, &params.commit, &params.other_id)
            .await
            .map_err(<scs_errors::ServiceError as Into<AsyncRequestsError>>::into)?;

    commit_sparse_profile_delta_impl(ctx, repo, changeset, other, params.profiles)
        .await
        .map_err(<scs_errors::ServiceError as Into<AsyncRequestsError>>::into)
}

/// Given the request params dispatches the request to the right processing
/// function and returns the computation result. Both successful computation
/// and error are part of the `AsynchronousRequestResult` structure. We only
/// return `Err` for transient errors, to indicate we should retry.
pub(crate) async fn megarepo_async_request_compute<R: MononokeRepo>(
    ctx: &CoreContext,
    mononoke: Arc<Mononoke<Repo>>,
    megarepo_api: &MegarepoApi<R>,
    queue: &AsyncMethodRequestQueue,
    request_row_id: &RowId,
    root_request_id: Option<RowId>,
    params: AsynchronousRequestParams,
    created_by: Option<String>,
) -> Result<AsynchronousRequestResult> {
    match params.into() {
        async_requests_types_thrift::AsynchronousRequestParams::megarepo_add_target_params(params) => {
            Ok(megarepo_add_sync_target(ctx, megarepo_api, params)
                .watched()
                .with_max_poll(METHOD_MAX_POLL_TIME_MS)
                .with_label("megarepo_add_sync_target")
                .await
                .map_err(|e| e.into())
                .into())
        }
        async_requests_types_thrift::AsynchronousRequestParams::megarepo_add_branching_target_params(params) => {
            Ok(megarepo_add_branching_sync_target(ctx, megarepo_api, params)
                .watched()
                .with_max_poll(METHOD_MAX_POLL_TIME_MS)
                .with_label("megarepo_add_branching_sync_target")
                .await
                .map_err(|e| e.into())
                .into())
        }
        async_requests_types_thrift::AsynchronousRequestParams::megarepo_change_target_params(params) => {
            Ok(megarepo_change_target_config(ctx, megarepo_api, params)
                .watched()
                .with_max_poll(METHOD_MAX_POLL_TIME_MS)
                .with_label("megarepo_change_target_config")
                .await
                .map_err(|e| e.into())
                .into())
        }
        async_requests_types_thrift::AsynchronousRequestParams::megarepo_remerge_source_params(params) => {
            Ok(megarepo_remerge_source(ctx, megarepo_api, params)
                .watched()
                .with_max_poll(METHOD_MAX_POLL_TIME_MS)
                .with_label("megarepo_remerge_source")
                .await
                .map_err(|e| e.into())
                .into())
        }
        async_requests_types_thrift::AsynchronousRequestParams::megarepo_sync_changeset_params(params) => {
            Ok(megarepo_sync_changeset(ctx, megarepo_api, params)
                .watched()
                .with_max_poll(METHOD_MAX_POLL_TIME_MS)
                .with_label("megarepo_sync_changeset")
                .await
                .map_err(|e| e.into())
                .into())
        }
        #[cfg(fbcode_build)]
        async_requests_types_thrift::AsynchronousRequestParams::commit_sparse_profile_size_params(params) => {
            Ok(commit_sparse_profile_size(ctx, mononoke, params)
                .watched()
                .with_max_poll(METHOD_MAX_POLL_TIME_MS)
                .with_label("commit_sparse_profile_size")
                .await
                .into())
        }
        #[cfg(not(fbcode_build))]
        async_requests_types_thrift::AsynchronousRequestParams::commit_sparse_profile_size_params(_) => {
            bail!("commit_sparse_profile_size not supported in non-fbcode build")
        }
        #[cfg(fbcode_build)]
        async_requests_types_thrift::AsynchronousRequestParams::commit_sparse_profile_delta_params(params) => {
            Ok(commit_sparse_profile_delta(ctx, mononoke, params)
                .watched()
                .with_max_poll(METHOD_MAX_POLL_TIME_MS)
                .with_label("commit_sparse_profile_delta")
                .await
                .into())
        }
        #[cfg(not(fbcode_build))]
        async_requests_types_thrift::AsynchronousRequestParams::commit_sparse_profile_delta_params(_) => {
            bail!("commit_sparse_profile_delta not supported in non-fbcode build")
        }
        async_requests_types_thrift::AsynchronousRequestParams::derive_boundaries_params(params) => {
            Ok(AsynchronousRequestResult::from_thrift(
                ThriftAsynchronousRequestResult::derive_boundaries_result(
                    crate::backfill::compute_derive_boundaries(ctx, mononoke, params)
                        .watched()
                        .with_max_poll(METHOD_MAX_POLL_TIME_MS)
                        .with_label("derive_boundaries")
                        .await?
                )
            ))
        }
        async_requests_types_thrift::AsynchronousRequestParams::derive_slice_params(params) => {
            Ok(AsynchronousRequestResult::from_thrift(
                ThriftAsynchronousRequestResult::derive_slice_result(
                    crate::backfill::compute_derive_slice(ctx, mononoke, params)
                        .watched()
                        .with_max_poll(METHOD_MAX_POLL_TIME_MS)
                        .with_label("derive_slice")
                        .await?
                )
            ))
        }
        async_requests_types_thrift::AsynchronousRequestParams::derive_backfill_params(params) => {
            Ok(AsynchronousRequestResult::from_thrift(
                ThriftAsynchronousRequestResult::derive_backfill_result(
                    crate::backfill::compute_derive_backfill(ctx, mononoke, queue, params, request_row_id.clone(), created_by.clone())
                        .watched()
                        .with_max_poll(METHOD_MAX_POLL_TIME_MS)
                        .with_label("derive_backfill")
                        .await?
                )
            ))
        }
        async_requests_types_thrift::AsynchronousRequestParams::derive_backfill_repo_params(params) => {
            let effective_root = root_request_id.unwrap_or_else(|| request_row_id.clone());
            Ok(AsynchronousRequestResult::from_thrift(
                ThriftAsynchronousRequestResult::derive_backfill_repo_result(
                    crate::backfill::compute_derive_backfill_repo(ctx, mononoke, queue, params, effective_root, created_by.clone())
                        .watched()
                        .with_max_poll(METHOD_MAX_POLL_TIME_MS)
                        .with_label("derive_backfill_repo")
                        .await?
                )
            ))
        }
        async_requests_types_thrift::AsynchronousRequestParams::mark_type_enabled_params(params) => {
            let effective_root = root_request_id.unwrap_or_else(|| request_row_id.clone());
            Ok(AsynchronousRequestResult::from_thrift(
                ThriftAsynchronousRequestResult::mark_type_enabled_result(
                    crate::backfill::compute_mark_type_enabled(ctx, mononoke, queue, params, effective_root, created_by.clone())
                        .watched()
                        .with_max_poll(METHOD_MAX_POLL_TIME_MS)
                        .with_label("mark_type_enabled")
                        .await?
                )
            ))
        }
        async_requests_types_thrift::AsynchronousRequestParams::UnknownField(union_tag) => {
             bail!(
                "this type of request (AsynchronousRequestParams tag {union_tag}) not supported by this worker!"
             )
        }
    }
}

#[cfg(fbcode_build)]
async fn get_repo_and_changeset(
    ctx: &CoreContext,
    mononoke: Arc<Mononoke<Repo>>,
    commit: &CommitSpecifier,
) -> Result<(RepoContext<Repo>, ChangesetContext<Repo>), scs_errors::ServiceError> {
    let changeset_specifier = ChangesetSpecifier::from_request(&commit.id)?;
    let bubble_fetcher = bubble_fetcher_for_changeset(ctx.clone(), changeset_specifier.clone());
    let repo = repo_impl(ctx.clone(), mononoke, &commit.repo, bubble_fetcher).await?;

    let changeset = repo
        .changeset(changeset_specifier)
        .await?
        .ok_or_else(|| scs_errors::commit_not_found(commit.description()))?;

    Ok((repo, changeset))
}

/// Get the repo and pair of changesets specified by a `thrift::CommitSpecifier`
/// and `thrift::CommitId` pair.
#[cfg(fbcode_build)]
async fn repo_changeset_pair(
    ctx: CoreContext,
    mononoke: Arc<Mononoke<Repo>>,
    commit: &thrift::CommitSpecifier,
    other_commit: &thrift::CommitId,
) -> Result<
    (
        RepoContext<Repo>,
        ChangesetContext<Repo>,
        ChangesetContext<Repo>,
    ),
    scs_errors::ServiceError,
> {
    let changeset_specifier =
        ChangesetSpecifier::from_request(&commit.id).context("invalid target commit id")?;
    let other_changeset_specifier = ChangesetSpecifier::from_request(other_commit)
        .context("invalid or missing other commit id")?;
    if other_changeset_specifier.in_bubble() {
        Err(scs_errors::invalid_request(format!(
            "Can't compare against a snapshot: {other_changeset_specifier}"
        )))?
    }
    let bubble_fetcher = bubble_fetcher_for_changeset(ctx.clone(), changeset_specifier.clone());
    let repo = repo_impl(ctx, mononoke, &commit.repo, bubble_fetcher).await?;
    let (changeset, other_changeset) = try_join!(
        async {
            Ok::<_, scs_errors::ServiceError>(
                repo.changeset(changeset_specifier)
                    .await
                    .context("failed to resolve target commit")?
                    .ok_or_else(|| scs_errors::commit_not_found(commit.description()))?,
            )
        },
        async {
            Ok::<_, scs_errors::ServiceError>(
                repo.changeset(other_changeset_specifier)
                    .await
                    .context("failed to resolve other commit")?
                    .ok_or_else(|| {
                        scs_errors::commit_not_found(format!(
                            "repo={} commit={}",
                            commit.repo.name, other_commit
                        ))
                    })?,
            )
        },
    )?;
    Ok((repo, changeset, other_changeset))
}

#[cfg(fbcode_build)]
fn bubble_fetcher_for_changeset(
    ctx: CoreContext,
    specifier: ChangesetSpecifier,
) -> impl FnOnce(RepoEphemeralStore) -> BoxFuture<'static, anyhow::Result<Option<BubbleId>>> {
    move |ephemeral| async move { specifier.bubble_id(&ctx, ephemeral).await }.boxed()
}

#[cfg(fbcode_build)]
async fn repo_impl<F, R>(
    ctx: CoreContext,
    mononoke: Arc<Mononoke<Repo>>,
    repo: &thrift::RepoSpecifier,
    bubble_fetcher: F,
) -> Result<RepoContext<Repo>, scs_errors::ServiceError>
where
    F: FnOnce(RepoEphemeralStore) -> R,
    R: Future<Output = anyhow::Result<Option<BubbleId>>>,
{
    let repo = mononoke
        .repo(ctx, &repo.name)
        .await?
        .ok_or_else(|| scs_errors::repo_not_found(repo.description()))?
        .with_bubble(bubble_fetcher)
        .await?
        .with_authorization_context(AuthorizationContext::new_bypass_access_control())
        .build()
        .await?;
    Ok(repo)
}
