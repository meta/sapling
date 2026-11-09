/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::time::Duration;

use anyhow::Context;
use anyhow::Error;
use anyhow::anyhow;
use anyhow::format_err;
use async_stream::try_stream;
use async_trait::async_trait;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use bonsai_hg_mapping::BonsaiHgMappingEntry;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use commit_graph::CommitGraphWriterArc;
use dag_types::Location;
use edenapi_types::AlterSnapshotRequest;
use edenapi_types::AlterSnapshotResponse;
use edenapi_types::AnyFileContentId;
use edenapi_types::AnyId;
use edenapi_types::Batch;
use edenapi_types::BonsaiChangesetContent;
use edenapi_types::BonsaiFileChange;
use edenapi_types::CommitGraphEntry;
use edenapi_types::CommitGraphRequest;
use edenapi_types::CommitGraphSegmentParent;
use edenapi_types::CommitGraphSegmentsEntry;
use edenapi_types::CommitGraphSegmentsRequest;
use edenapi_types::CommitHashLookupRequest;
use edenapi_types::CommitHashLookupResponse;
use edenapi_types::CommitHashToLocationRequestBatch;
use edenapi_types::CommitHashToLocationResponse;
use edenapi_types::CommitId;
use edenapi_types::CommitIdScheme;
use edenapi_types::CommitLocationToHashRequest;
use edenapi_types::CommitLocationToHashRequestBatch;
use edenapi_types::CommitLocationToHashResponse;
use edenapi_types::CommitMutationsRequest;
use edenapi_types::CommitMutationsResponse;
use edenapi_types::CommitRevlogData;
use edenapi_types::CommitRevlogDataRequest;
use edenapi_types::CommitTranslateIdRequest;
use edenapi_types::CommitTranslateIdResponse;
use edenapi_types::EphemeralExtendRequest;
use edenapi_types::EphemeralExtendResponse;
use edenapi_types::EphemeralPrepareRequest;
use edenapi_types::EphemeralPrepareResponse;
use edenapi_types::ExtendBubbleTtlOutcome;
use edenapi_types::FetchSnapshotRequest;
use edenapi_types::FetchSnapshotResponse;
use edenapi_types::HgChangesetContent;
use edenapi_types::IdenticalChangesetContent;
use edenapi_types::UploadBonsaiChangesetRequest;
use edenapi_types::UploadHgChangesetsRequest;
use edenapi_types::UploadIdenticalChangesetsRequest;
use edenapi_types::UploadToken;
use edenapi_types::UploadTokensResponse;
use edenapi_types::commit::SubtreeChange as EdenapiSubtreeChange;
use ephemeral_blobstore::BubbleId;
use ephemeral_blobstore::EphemeralBlobstoreError;
use futures::FutureExt;
use futures::Stream;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use futures::try_join;
use gotham::state::FromState;
use gotham::state::State;
use gotham_derive::StateData;
use gotham_derive::StaticResponseExtender;
use gotham_ext::error::HttpError;
use gotham_ext::handler::SlapiCommitIdentityScheme;
use gotham_ext::middleware::request_context::RequestContext;
use gotham_ext::response::TryIntoResponse;
use maplit::hashmap;
use mercurial_types::HgChangesetId;
use mercurial_types::HgNodeHash;
use mononoke_api::CoreContext;
use mononoke_api::CreateChangesetCheckMode;
use mononoke_api::CreateChangesetChecks;
use mononoke_api::CreateInfo;
use mononoke_api::MononokeError;
use mononoke_api::MononokeRepo;
use mononoke_api::Repo;
use mononoke_api::RepoContext;
use mononoke_api::XRepoLookupExactBehaviour;
use mononoke_api::XRepoLookupSyncBehaviour;
use mononoke_api_hg::HgRepoContext;
use mononoke_types::BlobstoreKey;
use mononoke_types::BlobstoreValue;
use mononoke_types::BonsaiChangeset;
use mononoke_types::BonsaiChangesetMut;
use mononoke_types::ChangesetId;
use mononoke_types::DateTime;
use mononoke_types::FileChange;
use mononoke_types::Globalrev;
use mononoke_types::SubtreeChange;
use mononoke_types::hash::GitSha1;
use mononoke_types::sha1_hash::Sha1;
use rate_limiting::Metric;
use rate_limiting::RateLimitStatus;
use rayon::iter::IntoParallelIterator;
use rayon::iter::ParallelIterator;
use repo_blobstore::RepoBlobstoreRef;
use serde::Deserialize;
use smallvec::SmallVec;
use sorted_vector_map::SortedVectorMap;
use tracing::debug;
use types::HgId;
use types::Parents;
use vec1::Vec1;

use super::HandlerInfo;
use super::HandlerResult;
use super::SaplingRemoteApiHandler;
use super::SaplingRemoteApiMethod;
use super::handler::SaplingRemoteApiContext;
use crate::context::ServerContext;
use crate::errors::MononokeErrorExt;
use crate::errors::SaplingRemoteApiServiceError;
use crate::handlers::git_objects::fetch_git_object;
use crate::utils::build_counter;
use crate::utils::cbor_stream_filtered_errors;
use crate::utils::counter_check_and_bump;
use crate::utils::get_repo;
use crate::utils::parse_cbor_request;
use crate::utils::to_create_change;
use crate::utils::to_hg_path_nonroot;
use crate::utils::to_mpath;
use crate::utils::to_revlog_changeset;
/// XXX: This number was chosen arbitrarily.
const MAX_CONCURRENT_FETCHES_PER_REQUEST: usize = 100;
const HASH_TO_LOCATION_BATCH_SIZE: usize = 100;

const PHASES_CHECK_LIMIT: usize = 10;

const COMMITS_PER_USER_RATE_LIMIT: &str = "commits_per_user";
const LOCATION_TO_HASH_COUNT_RATE_LIMIT: &str = "location_to_hash_count";

#[derive(Debug, Deserialize, StateData, StaticResponseExtender)]
pub struct RevlogDataParams {
    repo: String,
}

#[derive(Debug, Deserialize, StateData, StaticResponseExtender)]
pub struct UploadBonsaiChangesetQueryString {
    bubble_id: Option<NonZeroU64>,
}

pub struct LocationToHashHandler;

async fn translate_location<R: MononokeRepo>(
    hg_repo_ctx: HgRepoContext<R>,
    slapi_flavour: SlapiCommitIdentityScheme,
    request: CommitLocationToHashRequest,
) -> Result<CommitLocationToHashResponse, Error> {
    // TODO(mbthomas): refactor HgId to Id20 (and related)
    let location = request.location.map_descendant(|x| x.into());
    let ancestors: Vec<HgChangesetId> = match slapi_flavour {
        SlapiCommitIdentityScheme::Hg => hg_repo_ctx
            .location_to_hg_changeset_id(location, request.count)
            .await
            .context(SaplingRemoteApiServiceError::CommitLocationToHashRequestFailed)?,
        SlapiCommitIdentityScheme::Git => {
            let repo_ctx = hg_repo_ctx.repo_ctx();
            // TODO(mbthomas): This is a working around HgId/HgChangesetId not being "generic".
            // This should be cleaned up when we have a generic Id20 type
            repo_ctx
                .location_to_git_changeset_id(
                    Location::new(
                        GitSha1::from(location.descendant.into_nodehash().sha1().into_byte_array()),
                        location.distance,
                    ),
                    request.count,
                )
                .await
                .context(SaplingRemoteApiServiceError::CommitLocationToHashRequestFailed)?
                .into_iter()
                .map(|id| {
                    HgChangesetId::new(HgNodeHash::new(Sha1::from_byte_array(id.into_inner())))
                })
                .collect()
        }
    };
    let hgids = ancestors.into_iter().map(|x| x.into()).collect();
    let answer = CommitLocationToHashResponse {
        location: request.location,
        count: request.count,
        hgids,
    };
    Ok(answer)
}

async fn bump_counter_check_ratelimit(
    ctx: CoreContext,
    rate_limit_name: &str,
    bump_value: f64,
) -> Result<(), Error> {
    let rate_limiter = match ctx.session().rate_limiter() {
        Some(rate_limiter) => rate_limiter,
        None => {
            debug!("No rate_limiter info found");
            return Ok(());
        }
    };
    let category = rate_limiter.category();

    let client_request_info = match ctx.client_request_info() {
        Some(client_request_info) => client_request_info,
        None => {
            debug!("No client request info found");
            return Ok(());
        }
    };

    let atlas = ctx.metadata().clientinfo_atlas();

    let limit = match rate_limiter.find_rate_limit(
        Metric::CommitsPerUser,
        None,
        client_request_info.main_id.as_deref(),
        atlas,
    ) {
        Some(limit) => limit,
        None => {
            debug!("No {} rate limit found", rate_limit_name);
            return Ok(());
        }
    };

    let enforced = match limit.body.raw_config.status {
        RateLimitStatus::Disabled => return Ok(()),
        RateLimitStatus::Tracked => false,
        RateLimitStatus::Enforced => true,
        _ => panic!("Invalid limit status: {:?}", limit.body.raw_config.status),
    };

    let client_main_id = match &client_request_info.main_id {
        Some(client_main_id) => client_main_id,
        None => {
            debug!("No main client id found");
            return Ok(());
        }
    };

    let counter = build_counter(&ctx, category, rate_limit_name, client_main_id);
    counter_check_and_bump(
        &ctx,
        counter,
        bump_value,
        limit,
        enforced,
        hashmap! {"client_main_id" => client_main_id.as_str() },
    )
    .await
}

#[async_trait]
impl SaplingRemoteApiHandler for LocationToHashHandler {
    type Request = CommitLocationToHashRequestBatch;
    type Response = CommitLocationToHashResponse;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::CommitLocationToHash;
    const ENDPOINT: &'static str = "/commit/location_to_hash";

    fn sampling_rate(_request: &Self::Request) -> NonZeroU64 {
        nonzero_ext::nonzero!(100u64)
    }

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let repo = ectx.repo();
        let slapi_flavour = ectx.slapi_flavour();

        let ctx = repo.ctx().clone();
        let bump: f64 = request.requests.iter().map(|r| r.count as f64).sum();
        bump_counter_check_ratelimit(ctx, LOCATION_TO_HASH_COUNT_RATE_LIMIT, bump).await?;

        let hgid_list = request
            .requests
            .into_iter()
            .map(move |location| translate_location(repo.clone(), slapi_flavour, location));
        let response = stream::iter(hgid_list).buffer_unordered(MAX_CONCURRENT_FETCHES_PER_REQUEST);
        Ok(response.boxed())
    }
}

pub struct HashToLocationHandler;

async fn hg_hash_to_location_chunk<R: MononokeRepo>(
    hg_repo_ctx: HgRepoContext<R>,
    master_heads: Vec<HgChangesetId>,
    hg_cs_ids: Vec<HgChangesetId>,
) -> impl Stream<Item = CommitHashToLocationResponse> {
    let hgcsid_to_location = hg_repo_ctx
        .many_hg_changeset_ids_to_locations(master_heads, hg_cs_ids.clone())
        .await;
    let responses = hg_cs_ids.into_iter().map(move |hgcsid| {
        let result = match hgcsid_to_location.as_ref() {
            Ok(hsh) => match hsh.get(&hgcsid) {
                Some(Ok(l)) => Ok(Some(l.map_descendant(|x| x.into()))),
                Some(Err(e)) => Err(e.into()),
                None => Ok(None),
            },
            Err(e) => Err(e.into()),
        };
        CommitHashToLocationResponse {
            hgid: hgcsid.into(),
            result,
        }
    });
    stream::iter(responses)
}

async fn git_hash_to_location_chunk<R: MononokeRepo>(
    repo_ctx: RepoContext<R>,
    master_heads: Vec<GitSha1>,
    git_commit_ids: Vec<GitSha1>,
) -> impl Stream<Item = CommitHashToLocationResponse> {
    let git_commit_id_to_location = repo_ctx
        .many_git_commit_ids_to_locations(master_heads, git_commit_ids.clone())
        .await;
    let responses = git_commit_ids.into_iter().map(move |git_commit_id| {
        let result = match git_commit_id_to_location.as_ref() {
            Ok(hsh) => match hsh.get(&git_commit_id) {
                Some(Ok(l)) => Ok(Some(l.map_descendant(|x| HgId::from(x.into_inner())))),
                Some(Err(e)) => Err(e.into()),
                None => Ok(None),
            },
            Err(e) => Err(e.into()),
        };
        CommitHashToLocationResponse {
            hgid: HgId::from(git_commit_id.into_inner()),
            result,
        }
    });
    stream::iter(responses)
}

#[async_trait]
impl SaplingRemoteApiHandler for HashToLocationHandler {
    type Request = CommitHashToLocationRequestBatch;
    type Response = CommitHashToLocationResponse;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::CommitHashToLocation;
    const ENDPOINT: &'static str = "/commit/hash_to_location";

    fn sampling_rate(_request: &Self::Request) -> NonZeroU64 {
        nonzero_ext::nonzero!(256_u64)
    }

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let repo = ectx.repo();
        let slapi_flavour = ectx.slapi_flavour();

        let master_heads = request
            .master_heads
            .into_iter()
            .map(|x| x.into())
            .collect::<Vec<_>>();
        let response = match slapi_flavour {
            SlapiCommitIdentityScheme::Hg => stream::iter(request.hgids)
                .chunks(HASH_TO_LOCATION_BATCH_SIZE)
                .map(|chunk| chunk.into_iter().map(|x| x.into()).collect::<Vec<_>>())
                .map({
                    move |chunk| {
                        hg_hash_to_location_chunk(repo.clone(), master_heads.clone(), chunk)
                    }
                })
                .buffer_unordered(3)
                .flatten()
                .map(Ok)
                .boxed(),
            SlapiCommitIdentityScheme::Git => {
                // TODO(mbthomas): This is a working around HgId/HgChangesetId not being "generic".
                // This should be cleaned up when we have a generic Id20 type
                stream::iter(request.hgids)
                    .chunks(HASH_TO_LOCATION_BATCH_SIZE)
                    .map(|chunk| {
                        chunk
                            .into_iter()
                            .map(|x| GitSha1::from(x.into_byte_array()))
                            .collect::<Vec<_>>()
                    })
                    .map({
                        let ctx = repo.repo_ctx().clone();
                        let master_heads = master_heads
                            .into_iter()
                            .map(|x| GitSha1::from(x.into_nodehash().sha1().into_byte_array()))
                            .collect::<Vec<_>>();
                        move |chunk| {
                            git_hash_to_location_chunk(ctx.clone(), master_heads.clone(), chunk)
                        }
                    })
                    .buffer_unordered(3)
                    .flatten()
                    .map(Ok)
                    .boxed()
            }
        };
        Ok(response)
    }

    fn extract_in_band_error(response: &Self::Response) -> Option<anyhow::Error> {
        response
            .result
            .as_ref()
            .err()
            .map(|err| format_err!("{err:?}"))
    }
}

pub async fn revlog_data(state: &mut State) -> Result<impl TryIntoResponse + use<>, HttpError> {
    let params = RevlogDataParams::take_from(state);

    state.put(HandlerInfo::new(
        &params.repo,
        SaplingRemoteApiMethod::CommitRevlogData,
    ));

    let sctx = ServerContext::borrow_from(state);
    let rctx = RequestContext::borrow_from(state).clone();
    let slapi_flavour = SlapiCommitIdentityScheme::borrow_from(state).clone();

    let hg_repo_ctx: HgRepoContext<Repo> = get_repo(sctx, &rctx, &params.repo, None).await?;

    let request: CommitRevlogDataRequest = parse_cbor_request(state).await?;
    let revlog_commits = request
        .hgids
        .into_iter()
        .map(move |hg_id| match slapi_flavour {
            SlapiCommitIdentityScheme::Git => {
                fetch_git_object_as_revlog_data(hg_id, hg_repo_ctx.clone()).left_future()
            }
            SlapiCommitIdentityScheme::Hg => {
                commit_revlog_data(hg_repo_ctx.clone(), hg_id).right_future()
            }
        });
    let response =
        stream::iter(revlog_commits).buffer_unordered(MAX_CONCURRENT_FETCHES_PER_REQUEST);
    Ok(cbor_stream_filtered_errors(super::monitor_request(
        state, response,
    )))
}

async fn commit_revlog_data<R: MononokeRepo>(
    hg_repo_ctx: HgRepoContext<R>,
    hg_id: HgId,
) -> Result<CommitRevlogData, Error> {
    let bytes = hg_repo_ctx
        .revlog_commit_data(hg_id.into())
        .await
        .context(SaplingRemoteApiServiceError::CommitRevlogDataRequestFailed)?
        .ok_or(SaplingRemoteApiServiceError::HgIdNotFound(hg_id))?;
    let answer = CommitRevlogData::new(hg_id, bytes.into());
    Ok(answer)
}

// Sapling wants to use revlog_data the same way for Hg and Git, so shaping somehow
// the git object to fit within the defined CommitRevlogData
async fn fetch_git_object_as_revlog_data<R: MononokeRepo>(
    id: HgId,
    repo: HgRepoContext<R>,
) -> Result<CommitRevlogData, Error> {
    Ok(CommitRevlogData {
        hgid: id,
        revlog_data: fetch_git_object(id, &repo)
            .await
            .map(|bytes| bytes.bytes.into())?,
    })
}

pub struct HashLookupHandler;

#[async_trait]
impl SaplingRemoteApiHandler for HashLookupHandler {
    type Request = Batch<CommitHashLookupRequest>;
    type Response = CommitHashLookupResponse;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::CommitHashLookup;
    const ENDPOINT: &'static str = "/commit/hash_lookup";

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let repo = ectx.repo();
        use CommitHashLookupRequest::*;
        Ok(stream::iter(request.batch)
            .map(move |request| {
                let hg_repo_ctx = repo.clone();
                async move {
                    let changesets = match request {
                        InclusiveRange(low, high) => {
                            hg_repo_ctx.get_hg_in_range(low.into(), high.into()).await?
                        }
                    };
                    let hgids = changesets.into_iter().map(|x| x.into()).collect();
                    let response = CommitHashLookupResponse { request, hgids };
                    Ok(response)
                }
            })
            .buffered(MAX_CONCURRENT_FETCHES_PER_REQUEST)
            .boxed())
    }
}

/// Upload list of HgChangesets requested by the client
pub struct UploadHgChangesetsHandler;

#[async_trait]
impl SaplingRemoteApiHandler for UploadHgChangesetsHandler {
    type Request = UploadHgChangesetsRequest;
    type Response = UploadTokensResponse;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::UploadHgChangesets;
    const ENDPOINT: &'static str = "/upload/changesets";

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let repo = ectx.repo();
        // Commit-cloud sync uploads drafts here before touching workspace refs.
        if let Err(err) = repo.repo_ctx().ensure_commit_cloud_writable() {
            return Err(HttpError::e400(err).into());
        }
        let changesets = request.changesets;

        let ctx = repo.ctx().clone();
        // Per-user commit rate limit: always 429, this client is the offender.
        bump_counter_check_ratelimit(ctx, COMMITS_PER_USER_RATE_LIMIT, 1.0)
            .await
            .map_err(HttpError::e429)?;

        let mutations = request.mutations;
        let changesets_data = changesets
            .into_iter()
            .map(|changeset| {
                Ok((
                    HgChangesetId::new(HgNodeHash::from(changeset.node_id)),
                    to_revlog_changeset(changeset.changeset_content)?,
                    None,
                ))
            })
            .collect::<Result<Vec<_>, Error>>()?;

        let mutation_data = mutations
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<_, _>>()?;

        let stored = repo
            .store_hg_changesets(changesets_data, mutation_data)
            .await
            .map_err(|e| match e {
                MononokeError::InvalidRequest(_) => e.into_http_error("invalid changeset upload"),
                e => HttpError::e500(e),
            })?;

        // Safe to derive now: with bonsai=None, store_hg_changesets already wrote
        // the mapping (via CreateChangeset), so no conflicting hg id is re-derived.
        repo.ensure_uploaded_augmented_manifests_derived(&stored)
            .await;

        let results = stored.into_iter().map(move |r| {
            r.map(|(hg_cs, _bonsai)| {
                let hgid = HgId::from(hg_cs.get_changeset_id().into_nodehash());
                UploadTokensResponse {
                    token: UploadToken::new_fake_token(AnyId::HgChangesetId(hgid), None),
                }
            })
            .map_err(Error::from)
        });

        Ok(stream::iter(results).boxed())
    }
}

/// Upload list of bonsai changesets requested by the client
pub struct UploadBonsaiChangesetHandler;

#[async_trait]
impl SaplingRemoteApiHandler for UploadBonsaiChangesetHandler {
    type QueryStringExtractor = UploadBonsaiChangesetQueryString;
    type Request = UploadBonsaiChangesetRequest;
    type Response = UploadTokensResponse;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::UploadBonsaiChangeset;
    const ENDPOINT: &'static str = "/upload/changeset/bonsai";

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let repo = ectx.repo();
        // Commit-cloud sync uploads drafts here before touching workspace refs.
        if let Err(err) = repo.repo_ctx().ensure_commit_cloud_writable() {
            return Err(HttpError::e400(err).into());
        }
        let query = ectx.query();
        let bubble_id = query.bubble_id.map(BubbleId::new);
        let cs = request.changeset;
        let repo = &repo;

        let ctx = repo.ctx().clone();
        // Per-user commit rate limit: always 429, this client is the offender.
        bump_counter_check_ratelimit(ctx, COMMITS_PER_USER_RATE_LIMIT, 1.0)
            .await
            .map_err(HttpError::e429)?;

        let parents = stream::iter(cs.hg_parents)
            .then(|hgid| async move {
                repo.get_bonsai_from_hg(hgid.into())
                    .await?
                    .ok_or_else(|| anyhow!("Parent HgId {hgid} is invalid"))
            })
            .try_collect()
            .await?;

        let cs_id = upload_bonsai_changeset(cs.clone(), repo, bubble_id, parents).await?;

        Ok(stream::once(async move {
            Ok(UploadTokensResponse {
                token: UploadToken::new_fake_token(
                    AnyId::BonsaiChangesetId(cs_id.into()),
                    bubble_id.map(Into::into),
                ),
            })
        })
        .boxed())
    }
}

async fn upload_bonsai_changeset(
    cs: BonsaiChangesetContent,
    repo: &HgRepoContext<Repo>,
    bubble_id: Option<BubbleId>,
    parents: Vec<ChangesetId>,
) -> anyhow::Result<ChangesetId> {
    let cs_ctx = repo
        .repo_ctx()
        .create_changeset(
            parents,
            CreateInfo {
                author: cs.author,
                author_date: DateTime::from_timestamp(cs.time, cs.tz)?.into(),
                committer: None,
                committer_date: None,
                message: cs.message,
                extra: cs.extra.into_iter().map(|e| (e.key, e.value)).collect(),
                // TODO(rajshar): Need to allow passing git_extra_headers through Eden API as well.
                git_extra_headers: None,
            },
            cs.file_changes
                .into_iter()
                .map(|(path, fc)| {
                    let create_change = to_create_change(fc, bubble_id)
                        .with_context(|| anyhow!("Parsing file changes for {path}"))?;
                    Ok((to_mpath(path)?, create_change))
                })
                .collect::<anyhow::Result<_>>()?,
            match bubble_id {
                Some(id) => Some(repo.open_bubble(id).await?),
                None => None,
            }
            .as_ref(),
            CreateChangesetChecks {
                noop_file_changes: CreateChangesetCheckMode::Skip,
                deleted_files_existed_in_a_parent: CreateChangesetCheckMode::Skip,
                empty_changeset: CreateChangesetCheckMode::Skip,
                copy_from_path: CreateChangesetCheckMode::Skip,
                prefix_files_deleted: CreateChangesetCheckMode::Skip,
            },
        )
        .await
        .with_context(|| anyhow!("When creating bonsai changeset"))?
        .changeset_ctx;

    Ok(cs_ctx.id())
}

/// Get information about a snapshot changeset
pub struct FetchSnapshotHandler;

#[async_trait]
impl SaplingRemoteApiHandler for FetchSnapshotHandler {
    type Request = FetchSnapshotRequest;
    type Response = FetchSnapshotResponse;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::FetchSnapshot;
    const ENDPOINT: &'static str = "/snapshot";

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let repo = ectx.repo();
        let cs_id = ChangesetId::from(request.cs_id);
        let bubble_id = repo
            .ephemeral_store()
            .bubble_from_changeset(repo.ctx(), &cs_id)
            .await
            .context("Failure in fetching bubble from changeset")?
            .ok_or_else(|| {
                HttpError::e404(MononokeError::NotAvailable(format!(
                    "Snapshot for changeset {cs_id} not found in bubble"
                )))
            })?;
        let labels = repo
            .ephemeral_store()
            .labels_from_bubble(repo.ctx(), &bubble_id)
            .await
            .context("Failed to fetch labels associated with the snapshot")?;
        let bubble = match repo
            .ephemeral_store()
            .open_bubble(repo.ctx(), bubble_id)
            .await
        {
            Ok(bubble) => bubble,
            Err(e) => {
                let bubble_expired = e
                    .downcast_ref::<EphemeralBlobstoreError>()
                    .is_some_and(EphemeralBlobstoreError::is_bubble_expiry);
                let err = if bubble_expired {
                    HttpError::e400(MononokeError::NotAvailable(format!(
                        "Snapshot for changeset {cs_id} with bubble ID {bubble_id} expired"
                    )))
                } else {
                    HttpError::e500(MononokeError::from(e))
                };
                return Err(err.into());
            }
        };
        let blobstore = bubble.wrap_repo_blobstore(repo.repo().repo_blobstore().clone());
        let fallible_cs = cs_id
            .load(repo.ctx(), &blobstore)
            .await
            .context("Failed to load bonsai changeset through bubble blobstore");
        let cs = match fallible_cs {
            Ok(cs) => cs.into_mut(),
            Err(e) => {
                // Check if this is a bubble expiration error by downcasting.
                // Matches both NoSuchBubble (open-time) and BubbleExpired (mid-operation).
                let bubble_expired = e
                    .downcast_ref::<EphemeralBlobstoreError>()
                    .is_some_and(EphemeralBlobstoreError::is_bubble_expiry);
                let err = if bubble_expired {
                    HttpError::e400(MononokeError::NotAvailable(format!(
                        "Snapshot for changeset {cs_id} with bubble ID {bubble_id} expired"
                    )))
                } else {
                    HttpError::e500(MononokeError::from(e))
                };
                return Err(err.into());
            }
        };
        let time = cs.author_date.timestamp_secs();
        let tz = cs.author_date.tz_offset_secs();
        let parents = cs.parents.clone();
        let response = FetchSnapshotResponse {
            author: cs.author,
            time,
            tz,
            hg_parents: Parents::from_iter(
                stream::iter(
                    cs.parents
                        .into_iter()
                        .map(|cs_id| repo.get_hg_from_bonsai(cs_id)),
                )
                .buffered(2)
                .try_collect::<Vec<_>>()
                .await?
                .into_iter()
                .map(|id| id.into()),
            ),
            file_changes: cs
                .file_changes
                .into_iter()
                .map(|(path, fc)| {
                    Ok((
                        to_hg_path_nonroot(&path)?,
                        match fc {
                            FileChange::Deletion => BonsaiFileChange::Deletion,
                            FileChange::UntrackedDeletion => BonsaiFileChange::UntrackedDeletion,
                            FileChange::Change(tc) => BonsaiFileChange::Change {
                                upload_token: UploadToken::new_fake_token(
                                    AnyId::AnyFileContentId(AnyFileContentId::ContentId(
                                        tc.content_id().into(),
                                    )),
                                    Some(bubble_id.into()),
                                ),
                                file_type: tc.file_type().try_into()?,
                                copy_info: tc.copy_from().and_then(|(copy_path, copy_cs_id)| {
                                    // Find the parent index for the copy source changeset
                                    to_hg_path_nonroot(copy_path).ok().zip(
                                        parents
                                            .iter()
                                            .position(|&parent_cs_id| parent_cs_id == *copy_cs_id),
                                    )
                                }),
                            },
                            FileChange::UntrackedChange(uc) => BonsaiFileChange::UntrackedChange {
                                upload_token: UploadToken::new_fake_token(
                                    AnyId::AnyFileContentId(AnyFileContentId::ContentId(
                                        uc.content_id().into(),
                                    )),
                                    Some(bubble_id.into()),
                                ),
                                file_type: uc.file_type().try_into()?,
                            },
                        },
                    ))
                })
                .collect::<Result<_, Error>>()?,
            bubble_id: Some(bubble_id.into()),
            labels,
        };
        Ok(stream::once(async move { Ok(response) }).boxed())
    }
}

// Alter the properties of an existing snapshot
pub struct AlterSnapshotHandler;

#[async_trait]
impl SaplingRemoteApiHandler for AlterSnapshotHandler {
    type Request = AlterSnapshotRequest;
    type Response = AlterSnapshotResponse;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::AlterSnapshot;
    const ENDPOINT: &'static str = "/snapshot/alter";

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let repo = ectx.repo();
        let cs_id = ChangesetId::from(request.cs_id);
        let id = repo
            .ephemeral_store()
            .bubble_from_changeset(repo.ctx(), &cs_id)
            .await?
            .ok_or_else(|| {
                HttpError::e404(MononokeError::NotAvailable(format!(
                    "Snapshot for changeset {cs_id} not found in bubble"
                )))
            })?;
        let (label_addition, label_removal) = (
            !request.labels_to_add.is_empty(),
            !request.labels_to_remove.is_empty(),
        );
        if label_addition && label_removal {
            // Input has both labels to add and labels to remove, which is not allowed.
            Err(anyhow!(
                "Alter snapshot request cannot have labels_to_add and labels_to_remove both as non-empty"
            ))?
        } else if label_addition {
            // Input has labels to add, so let's add the input labels.
            repo.ephemeral_store()
                .add_bubble_labels(repo.ctx(), id, request.labels_to_add.clone())
                .await?;
        } else {
            // Input has labels to remove, or no labels as input at all. In either case,
            // we need to remove specific or all labels corresponding to the bubble.
            repo.ephemeral_store()
                .remove_bubble_labels(repo.ctx(), id, request.labels_to_remove.clone())
                .await?;
        }
        let current_labels = repo
            .ephemeral_store()
            .labels_from_bubble(repo.ctx(), &id)
            .await?;
        let response = AlterSnapshotResponse { current_labels };
        Ok(stream::once(async move { Ok(response) }).boxed())
    }
}

/// Creates an ephemeral bubble and return its id
pub struct EphemeralPrepareHandler;

#[async_trait]
impl SaplingRemoteApiHandler for EphemeralPrepareHandler {
    type Request = EphemeralPrepareRequest;
    type Response = EphemeralPrepareResponse;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::EphemeralPrepare;
    const ENDPOINT: &'static str = "/ephemeral/prepare";

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let repo = ectx.repo();
        Ok(stream::once(async move {
            let bubble = repo
                .create_bubble(
                    request.custom_duration_secs.map(Duration::from_secs),
                    request.labels.unwrap_or_else(Vec::new),
                )
                .await?;
            Ok(EphemeralPrepareResponse {
                bubble_id: bubble.bubble_id().into(),
                expiration_timestamp: Some(bubble.expires_at().timestamp_secs()),
            })
        })
        .boxed())
    }
}

/// Extends the TTL of an ephemeral bubble
pub struct EphemeralExtendHandler;

#[async_trait]
impl SaplingRemoteApiHandler for EphemeralExtendHandler {
    type Request = EphemeralExtendRequest;
    type Response = EphemeralExtendResponse;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::EphemeralExtend;
    const ENDPOINT: &'static str = "/ephemeral/extend";

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let repo = ectx.repo();
        let cs_id = ChangesetId::from(request.snapshot_id);
        let bubble_id = repo
            .ephemeral_store()
            .bubble_from_changeset(repo.ctx(), &cs_id)
            .await
            .context("Failure in fetching bubble from changeset")?
            .ok_or_else(|| {
                HttpError::e404(MononokeError::NotAvailable(format!(
                    "Snapshot for changeset {cs_id} not found in bubble"
                )))
            })?;
        let custom_duration = request.custom_duration_secs.map(Duration::from_secs);

        // Extend the bubble's TTL using the new method
        let store_outcome = match repo
            .ephemeral_store()
            .extend_bubble_ttl(repo.ctx(), bubble_id, custom_duration)
            .await
        {
            Ok(outcome) => outcome,
            Err(e) => {
                let bubble_expired = e
                    .downcast_ref::<EphemeralBlobstoreError>()
                    .is_some_and(EphemeralBlobstoreError::is_bubble_expiry);
                let err = if bubble_expired {
                    HttpError::e400(MononokeError::NotAvailable(format!(
                        "Bubble with ID {bubble_id} expired"
                    )))
                } else {
                    HttpError::e500(MononokeError::from(e))
                };
                return Err(err.into());
            }
        };

        // Convert the store outcome to the API outcome
        let api_outcome = match store_outcome {
            ephemeral_blobstore::ExtendBubbleTtlOutcome::Extended(timestamp) => {
                ExtendBubbleTtlOutcome::Extended(timestamp.timestamp_seconds())
            }
            ephemeral_blobstore::ExtendBubbleTtlOutcome::NotChanged(timestamp) => {
                ExtendBubbleTtlOutcome::NotChanged(timestamp.timestamp_seconds())
            }
        };

        let response = EphemeralExtendResponse {
            bubble_id: bubble_id.into(),
            result: api_outcome,
        };

        Ok(stream::once(async move { Ok(response) }).boxed())
    }
}

pub struct GraphHandlerV2;

#[async_trait]
impl SaplingRemoteApiHandler for GraphHandlerV2 {
    type Request = CommitGraphRequest;
    type Response = CommitGraphEntry;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::CommitGraphV2;
    const ENDPOINT: &'static str = "/commit/graph_v2";

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let repo = ectx.repo();
        let heads: Vec<_> = request
            .heads
            .into_iter()
            .map(|hg_id| HgChangesetId::new(HgNodeHash::from(hg_id)))
            .collect();
        let common: Vec<_> = request
            .common
            .into_iter()
            .map(|hg_id| HgChangesetId::new(HgNodeHash::from(hg_id)))
            .collect();

        if common.is_empty()
            && repo
                .repo_ctx()
                .config()
                .commit_graph_config
                .disable_commit_graph_v2_with_empty_common
        {
            Err(anyhow!(
                "Commit graph v2 with empty common is not allowed for repo {}",
                repo.repo_ctx().name(),
            ))?
        }

        // If all the requested heads are public, return stream.
        if heads.len() < PHASES_CHECK_LIMIT && repo.is_all_public(&heads).await? {
            let graph_stream = repo
                .get_graph_mapping_stream(common, heads)
                .await?
                .err_into::<Error>()
                .and_then(|(hgid, parents)| async move {
                    Ok(CommitGraphEntry {
                        hgid: HgId::from(hgid.into_nodehash()),
                        parents: parents
                            .into_iter()
                            .map(|p_hgid| HgId::from(p_hgid.into_nodehash()))
                            .collect(),
                        is_draft: Some(false),
                    })
                })
                .boxed();
            return Ok(graph_stream);
        }

        let graph_entries = repo
            .get_graph_mapping(common, heads)
            .await?
            .into_iter()
            .map(|(hgid, (parents, is_draft))| {
                Ok(CommitGraphEntry {
                    hgid: HgId::from(hgid.into_nodehash()),
                    parents: parents
                        .into_iter()
                        .map(|p_hgid| HgId::from(p_hgid.into_nodehash()))
                        .collect(),
                    is_draft: Some(is_draft),
                })
            });
        Ok(stream::iter(graph_entries).boxed())
    }
}

pub struct GraphSegmentsHandler;

#[async_trait]
impl SaplingRemoteApiHandler for GraphSegmentsHandler {
    type Request = CommitGraphSegmentsRequest;
    type Response = CommitGraphSegmentsEntry;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::CommitGraphSegments;
    const ENDPOINT: &'static str = "/commit/graph_segments";
    const SUPPORTED_FLAVOURS: &'static [SlapiCommitIdentityScheme] = &[
        SlapiCommitIdentityScheme::Hg,
        SlapiCommitIdentityScheme::Git,
    ];

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let slapi_flavour = ectx.slapi_flavour();
        let repo = ectx.repo();

        Ok(try_stream! {
            let graph_segments = match slapi_flavour {
                SlapiCommitIdentityScheme::Hg => {
                    let heads: Vec<_> = request
                        .heads
                        .into_iter()
                        .map(|hg_id| HgChangesetId::new(HgNodeHash::from(hg_id)))
                        .collect();
                    let common: Vec<_> = request
                        .common
                        .into_iter()
                        .map(|hg_id| HgChangesetId::new(HgNodeHash::from(hg_id)))
                        .collect();
                    repo.repo_ctx()
                        .graph_segments_hg(common, heads)
                        .await?
                        .map_ok(|segment| segment.map_ids(|id| HgId::from(id.into_nodehash())))
                        .left_stream()
                }
                SlapiCommitIdentityScheme::Git => {
                    let heads: Vec<_> = request
                        .heads
                        .into_iter()
                        .map(|id| GitSha1::from(id.into_byte_array()))
                        .collect();
                    let common: Vec<_> = request
                        .common
                        .into_iter()
                        .map(|id| GitSha1::from(id.into_byte_array()))
                        .collect();
                    repo.repo_ctx()
                        .graph_segments_git(common, heads)
                        .await?
                        .map_ok(|segment| segment.map_ids(|id| HgId::from(id.into_inner())))
                        .right_stream()
                }
            };

            for await segment in graph_segments {
                let segment = segment?;
                yield CommitGraphSegmentsEntry {
                    head: segment.head,
                    base: segment.base,
                    length: segment.length,
                    parents: segment
                        .parents
                        .into_iter()
                        .map(|parent| CommitGraphSegmentParent {
                            hgid: parent.id,
                            location: parent.location,
                        })
                        .collect(),
                }
            }
        }
        .boxed())
    }
}

pub struct CommitMutationsHandler;

#[async_trait]
impl SaplingRemoteApiHandler for CommitMutationsHandler {
    type Request = CommitMutationsRequest;
    type Response = CommitMutationsResponse;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::CommitMutations;
    const ENDPOINT: &'static str = "/commit/mutations";

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let repo = ectx.repo();

        let commits = request
            .commits
            .into_iter()
            .map(|hg_id| HgChangesetId::new(HgNodeHash::from(hg_id)))
            .collect();

        let mutations = repo
            .fetch_mutations(commits)
            .await?
            .into_iter()
            .map(|mutation| {
                Ok(CommitMutationsResponse {
                    mutation: mutation.into(),
                })
            });

        Ok(stream::iter(mutations).boxed())
    }
}

pub struct CommitTranslateId;

#[async_trait]
impl SaplingRemoteApiHandler for CommitTranslateId {
    type Request = CommitTranslateIdRequest;
    type Response = CommitTranslateIdResponse;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::CommitTranslateId;
    const ENDPOINT: &'static str = "/commit/translate_id";

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let from_repo = match request.from_repo {
            Some(from_repo) => ectx.other_repo(from_repo).await?,
            None => ectx.repo(),
        };
        let from_repo = from_repo.repo_ctx();

        let to_repo = match request.to_repo {
            Some(to_repo) => ectx.other_repo(to_repo).await?,
            None => ectx.repo(),
        };
        let to_repo = to_repo.repo_ctx();

        let mut hg_ids = Vec::new();
        let mut bonsai_ids = Vec::new();
        let mut git_ids = Vec::new();
        let mut globalrevs = Vec::new();

        let lookup_behavior = match request.lookup_behavior.as_deref() {
            Some("exact") => XRepoLookupExactBehaviour::OnlyExactMapping,
            None | Some("equivalent") => XRepoLookupExactBehaviour::WorkingCopyEquivalence,
            Some(behavior) => {
                return Err(HttpError::e400(MononokeError::InvalidRequest(format!(
                    "invalid lookup behavior '{behavior}'"
                )))
                .into());
            }
        };

        for commit in &request.commits {
            match commit {
                CommitId::Hg(hg_id) => hg_ids.push(HgChangesetId::from(*hg_id)),
                CommitId::Bonsai(bonsai_id) => bonsai_ids.push(ChangesetId::from(*bonsai_id)),
                CommitId::GitSha1(git_id) => git_ids.push(GitSha1::from(*git_id)),
                CommitId::Globalrev(globalrev) => globalrevs.push(Globalrev::new(*globalrev)),
            }
        }

        // Convert request types to intermediate bonsais using "from" repo.
        let (hg_bonsais, git_bonsais, globalrev_bonsais) = try_join!(
            from_repo.many_changeset_ids_from_hg(hg_ids),
            from_repo.many_changeset_ids_from_git_sha1(git_ids),
            from_repo.many_changeset_ids_from_globalrev(globalrevs),
        )?;

        // Mapping of request id to intermediate bonsai id of "from" repo.
        let mut input_to_bonsai: HashMap<CommitId, ChangesetId> = bonsai_ids
            .into_iter()
            .map(|id| (id.clone().into(), id))
            .chain(hg_bonsais.into_iter().map(|(hg, bs)| (hg.into(), bs)))
            .chain(git_bonsais.into_iter().map(|(g, bs)| (g.into(), bs)))
            .chain(globalrev_bonsais.into_iter().map(|(g, bs)| (g.into(), bs)))
            .collect();

        // Convert bonsai ids to that of "to" repo, if necessary.
        if from_repo.repoid() != to_repo.repoid() {
            input_to_bonsai = stream::iter(input_to_bonsai)
                .then(|(id, bs)| {
                    let from_repo = from_repo.clone();
                    let to_repo = to_repo.clone();
                    async move {
                        (
                            id,
                            from_repo
                                .xrepo_commit_lookup(
                                    &to_repo,
                                    bs.clone(),
                                    None,
                                    XRepoLookupSyncBehaviour::SyncIfAbsent,
                                    lookup_behavior,
                                )
                                .await,
                        )
                    }
                })
                .filter_map(|(id, xctx)| async move {
                    match xctx {
                        // If there is no mapping, skip it.
                        // TODO: perhaps we should propagate an error
                        Ok(None) => None,
                        Ok(Some(xctx)) => Some(Ok((id, xctx.id()))),
                        Err(err) => Some(Err(err)),
                    }
                })
                .try_collect()
                .await?;
        }

        let all_bonsai_ids: Vec<_> = input_to_bonsai.values().cloned().collect();
        // Convert all bonsais to the target type
        let bonsai_to_target: HashMap<ChangesetId, CommitId> = match request.scheme {
            CommitIdScheme::Bonsai => all_bonsai_ids
                .into_iter()
                .map(|to| (to.clone(), to.into()))
                .collect(),
            CommitIdScheme::Hg => to_repo
                .many_changeset_hg_ids(all_bonsai_ids)
                .await?
                .into_iter()
                .map(|(id, hg_id)| (id, hg_id.into()))
                .collect(),
            CommitIdScheme::GitSha1 => to_repo
                .many_changeset_git_sha1s(all_bonsai_ids)
                .await?
                .into_iter()
                .map(|(id, git_sha1)| (id, git_sha1.into()))
                .collect(),
            CommitIdScheme::Globalrev => to_repo
                .many_changeset_globalrev_ids(all_bonsai_ids)
                .await?
                .into_iter()
                .map(|(id, globalrev)| (id, globalrev.into()))
                .collect(),
        };

        // Build the response based on the order within the request.
        let translations: Vec<_> = request
            .commits
            .into_iter()
            .filter_map(|commit| {
                let translated = bonsai_to_target.get(input_to_bonsai.get(&commit)?)?.clone();
                Some(CommitTranslateIdResponse { commit, translated })
            })
            .map(anyhow::Ok)
            .collect();

        Ok(stream::iter(translations).boxed())
    }
}

/// For modern sync usage only
pub struct UploadIdenticalChangesetsHandler;

#[async_trait]
impl SaplingRemoteApiHandler for UploadIdenticalChangesetsHandler {
    type Request = UploadIdenticalChangesetsRequest;
    type Response = UploadTokensResponse;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::UploadIdenticalChangesets;
    const ENDPOINT: &'static str = "/upload/changesets/identical";

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let repo = ectx.repo();
        let ctx = repo.ctx().clone();

        repo.repo_ctx()
            .authorization_context()
            .require_mirror_upload_operations(&ctx, repo.repo())
            .await
            .map_err(|err| MononokeError::AuthorizationError(err.to_string()))?;

        let changesets = request.changesets;

        let bonsai_changesets: Vec<Result<(BonsaiChangeset, IdenticalChangesetContent), _>> =
            changesets
                .into_par_iter()
                .map(|ics| {
                    let parents = ics
                        .bonsai_parents
                        .to_vec()
                        .iter()
                        .map(|p| (*p).into())
                        .collect::<Vec<ChangesetId>>();

                    let mut hg_extra = SortedVectorMap::new();
                    ics.extras.iter().for_each(|bs_extra| {
                        hg_extra.insert(bs_extra.key.clone(), bs_extra.value.clone());
                    });

                    let mut git_extra_headers = None;
                    if let Some(ref git_extra) = ics.git_extra_headers {
                        let mut res = SortedVectorMap::new();
                        git_extra.iter().for_each(|e| {
                            res.insert(SmallVec::from(e.key.clone()), e.value.clone().into());
                        });
                        git_extra_headers = Some(res);
                    }

                    let file_changes = ics
                        .bonsai_file_changes
                        .clone()
                        .into_iter()
                        .map(|(path, bfc)| {
                            let create_change = to_create_change(bfc, None)
                                .with_context(|| anyhow!("Parsing file changes for {path}"))?;

                            let create_change2 = create_change.into_file_change(&parents)?;

                            let path =
                                to_mpath(path)?
                                    .into_optional_non_root_path()
                                    .ok_or_else(|| {
                                        MononokeError::InvalidRequest(String::from(
                                            "Cannot create a file with an empty path",
                                        ))
                                    })?;
                            Ok((path, create_change2))
                        })
                        .collect::<Result<_, MononokeError>>()?;

                    let committer_date = if let Some(committer_time) = ics.committer_time {
                        Some(DateTime::from_timestamp(
                            committer_time,
                            // strongly assume we have a committer_tz if we have a committer_time, but avoid failing
                            ics.committer_tz.unwrap_or(0),
                        )?)
                    } else {
                        None
                    };

                    let subtree_changes = ics
                        .subtree_changes
                        .clone()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(path, stc)| {
                            let mpath = to_mpath(path)?;
                            let subtree_change = match stc {
                                EdenapiSubtreeChange::DeepCopy {
                                    from_path,
                                    from_cs_id,
                                } => SubtreeChange::deep_copy(
                                    to_mpath(from_path)?,
                                    ChangesetId::from(from_cs_id),
                                ),
                                EdenapiSubtreeChange::Merge {
                                    from_path,
                                    from_cs_id,
                                } => SubtreeChange::merge(
                                    to_mpath(from_path)?,
                                    ChangesetId::from(from_cs_id),
                                ),
                                EdenapiSubtreeChange::Import {
                                    from_path,
                                    from_commit,
                                    from_repo_url,
                                } => SubtreeChange::import(
                                    to_mpath(from_path)?,
                                    from_commit,
                                    from_repo_url,
                                ),
                                EdenapiSubtreeChange::CrossRepoMerge {
                                    from_path,
                                    from_commit,
                                    from_repo_url,
                                } => SubtreeChange::cross_repo_merge(
                                    to_mpath(from_path)?,
                                    from_commit,
                                    from_repo_url,
                                ),
                            };
                            Ok((mpath, subtree_change))
                        })
                        .collect::<Result<_, MononokeError>>()?;

                    let bcs = BonsaiChangesetMut {
                        parents,
                        author: ics.author.clone(),
                        author_date: DateTime::from_timestamp(ics.time, ics.tz)?,
                        committer: ics.committer.clone(),
                        committer_date,
                        message: ics.message.clone(),
                        hg_extra,
                        file_changes,
                        git_extra_headers,
                        git_tree_hash: None,
                        is_snapshot: false,
                        git_annotated_tag: None,
                        subtree_changes,
                    }
                    .freeze()?;

                    if bcs.get_changeset_id().to_hex() == ics.bcs_id.to_hex() {
                        Ok::<_, MononokeError>((bcs, ics))
                    } else {
                        Err(MononokeError::InternalError(
                            anyhow!(
                                "the bonsai changeset id {} generated during upload does not match the one in the request {}",
                                bcs.get_changeset_id().to_hex(),
                                ics.bcs_id.to_hex(),
                            )
                            .into(),
                        ))
                    }
                })
                .collect::<Vec<_>>();

        let cloned_repo = repo.repo().clone();
        let blobstore = cloned_repo.repo_blobstore();
        let commit_graph_writer = cloned_repo.commit_graph_writer_arc();
        let bonsai_hg_mapping = cloned_repo.bonsai_hg_mapping();

        let bonsai_changesets_clone = bonsai_changesets.clone();
        let bs_ctx = ctx.clone();
        let bs_fut = async move {
            // Single pass: build blobstore put futures and commit graph entries together.
            let (put_futs, graph_entries_vec): (Vec<_>, Vec<_>) = bonsai_changesets_clone
                .into_iter()
                .map(|res| res.map(|(bcs, _)| bcs))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(|bcs| {
                    let bonsai_blob = bcs.clone().into_blob();
                    let blobstore_key = bcs.get_changeset_id().blobstore_key();
                    let put_fut = blobstore.put(&bs_ctx, blobstore_key, bonsai_blob.into());
                    let graph_entry = (bcs.get_changeset_id(), bcs.parents().collect(), Vec::new());
                    (put_fut, graph_entry)
                })
                .unzip();

            stream::iter(put_futs)
                .buffer_unordered(100)
                .try_collect::<Vec<_>>()
                .await?;

            match Vec1::try_from_vec(graph_entries_vec) {
                Ok(graph_entries) => {
                    commit_graph_writer
                        .add_many(&bs_ctx, graph_entries)
                        .await
                        .context("While inserting into commit graph")?;
                }
                Err(_empty) => {
                    // No changesets -- both put_futs and graph_entries are derived
                    // from the same iterator, so empty here means zero blobstore
                    // puts above as well.
                }
            }
            Ok::<_, MononokeError>(())
        };

        let augmented_repo = repo.clone();
        let hg_fut = async move {
            let mut changeset_data = Vec::new();
            for res in bonsai_changesets {
                let (bcs, ics) = res?;
                let item = (
                    HgChangesetId::new(HgNodeHash::from(ics.hg_info.node_id)),
                    to_revlog_changeset(HgChangesetContent::from(ics))?,
                    Some(bcs),
                );
                changeset_data.push(item);
            }
            let changesets = repo.store_hg_changesets(changeset_data, vec![]).await?;
            Ok::<_, MononokeError>(changesets)
        };

        let (_, hg_changesets) = tokio::try_join!(bs_fut, hg_fut)?;

        let mapping_entries: Vec<BonsaiHgMappingEntry> = hg_changesets
            .clone()
            .into_iter()
            .map(|hg_cs| {
                let (hg_cs, bcs) = hg_cs?;
                Ok(BonsaiHgMappingEntry {
                    hg_cs_id: hg_cs.get_changeset_id(),
                    bcs_id: bcs.get_changeset_id(),
                })
            })
            .collect::<Result<_, Error>>()?;

        bonsai_hg_mapping
            .bulk_add(&ctx, &mapping_entries)
            .await
            .context("While inserting in bonsai-hg mapping")?;

        // Must run AFTER the `bulk_add` above so the augmented derivation reuses
        // the caller-supplied hg id (see `ensure_uploaded_augmented_manifests_derived`).
        augmented_repo
            .ensure_uploaded_augmented_manifests_derived(&hg_changesets)
            .await;

        let tokens = hg_changesets.into_iter().map(move |r| {
            r.map(|(hg_cs, _)| {
                let hgid: types::hash::AbstractHashType<types::hgid::HgIdTypeInfo, 20> =
                    HgId::from(hg_cs.get_changeset_id().into_nodehash());
                UploadTokensResponse {
                    token: UploadToken::new_fake_token(AnyId::HgChangesetId(hgid), None),
                }
            })
            .map_err(Error::from)
        });

        Ok(stream::iter(tokens).boxed())
    }
}
