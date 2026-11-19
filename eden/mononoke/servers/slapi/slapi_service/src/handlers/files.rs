/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::num::NonZeroU64;
use std::str::FromStr;

use anyhow::Context;
use anyhow::Error;
use anyhow::anyhow;
use anyhow::ensure;
use anyhow::format_err;
use async_compression::tokio::bufread::ZstdDecoder;
use async_trait::async_trait;
use bytes::Bytes;
use context::PerfCounterType;
use edenapi_types::AnyFileContentId;
use edenapi_types::AnyId;
use edenapi_types::Batch;
use edenapi_types::FileAttributes;
use edenapi_types::FileAuxData;
use edenapi_types::FileContent;
use edenapi_types::FileContentTokenMetadata;
use edenapi_types::FileEntry;
use edenapi_types::FileRequest;
use edenapi_types::FileResponse;
use edenapi_types::FileSpec;
use edenapi_types::ServerError;
use edenapi_types::UploadHgFilenodeRequest;
use edenapi_types::UploadToken;
use edenapi_types::UploadTokenMetadata;
use edenapi_types::UploadTokensResponse;
use edenapi_types::wire::ToWire;
use ephemeral_blobstore::BubbleId;
use ephemeral_blobstore::bubble_hg_mapping_enabled;
use futures::FutureExt;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use gotham::helpers::http::Body;
use gotham::state::FromState;
use gotham::state::State;
use gotham_derive::StateData;
use gotham_derive::StaticResponseExtender;
use gotham_ext::error::HttpError;
use gotham_ext::handler::SlapiCommitIdentityScheme;
use gotham_ext::middleware::request_context::RequestContext;
use gotham_ext::response::TryIntoResponse;
use http_body_util::BodyExt as _;
use mercurial_types::HgChangesetId;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgNodeHash;
use mercurial_types::blobs::File;
use mononoke_api::MononokeRepo;
use mononoke_api::Repo;
use mononoke_api_hg::HgDataContext;
use mononoke_api_hg::HgDataId;
use mononoke_api_hg::HgRepoContext;
use rate_limiting::Metric;
use rate_limiting::Scope;
use revisionstore_types::Metadata;
use serde::Deserialize;
use stats::define_stats;
use stats::prelude::TimeseriesStatic;
use types::key::Key;
use types::parents::Parents;

use super::HandlerInfo;
use super::HandlerResult;
use super::SaplingRemoteApiHandler;
use super::SaplingRemoteApiMethod;
use super::handler::SaplingRemoteApiContext;
use crate::context::ServerContext;
use crate::errors::SaplingRemoteApiServiceError;
use crate::handlers::git_objects::fetch_git_object;
use crate::utils::cbor_stream_filtered_errors;
use crate::utils::get_repo;

// The size is optimized for the batching settings in EdenFs.
const MAX_CONCURRENT_FILE_FETCHES_PER_REQUEST: usize = 32;

const MAX_CONCURRENT_UPLOAD_FILENODES_PER_REQUEST: usize = 1000;

define_stats! {
    prefix = "mononoke.files";
    files_served: timeseries(Rate, Sum),
    files_batch_keys_requested: timeseries(Rate, Sum),
}

#[derive(Debug, Deserialize, StateData, StaticResponseExtender)]
pub struct UploadFileParams {
    repo: String,
    idtype: String,
    id: String,
}

#[derive(Debug, Deserialize, StateData, StaticResponseExtender)]
pub struct UploadFileQueryString {
    bubble_id: Option<NonZeroU64>,
    /// Stores the file in the bubble of this snapshot, named by its hg
    /// changeset id in hex.
    snapshot: Option<String>,
    content_size: u64,
    compression: Option<String>,
}

/// Fetch the content of the files requested by the client.
pub struct Files2Handler;

#[async_trait]
impl SaplingRemoteApiHandler for Files2Handler {
    type Request = FileRequest;
    type Response = FileResponse;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::Files2;
    const ENDPOINT: &'static str = "/files2";

    fn sampling_rate(_request: &Self::Request) -> NonZeroU64 {
        nonzero_ext::nonzero!(1024u64)
    }

    fn batch_size(request: &Self::Request) -> Option<usize> {
        Some(request.reqs.len())
    }

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let repo = ectx.repo();
        let ctx = repo.ctx().clone();

        STATS::files_batch_keys_requested.add_value(request.reqs.len() as i64);

        let fetches = request.reqs.into_iter().map({
            let ctx = ctx.clone();
            move |FileSpec { key, attrs }| {
                if attrs.content {
                    ctx.perf_counters()
                        .increment_counter(PerfCounterType::EdenapiFiles);
                }
                if attrs.aux_data {
                    ctx.perf_counters()
                        .increment_counter(PerfCounterType::EdenapiFilesAuxData);
                }

                match ectx.slapi_flavour() {
                    SlapiCommitIdentityScheme::Hg => {
                        fetch_file_response(repo.clone(), key, attrs).left_future()
                    }
                    SlapiCommitIdentityScheme::Git => {
                        fetch_git_object_as_file(key, repo.clone()).right_future()
                    }
                }
            }
        });

        Ok(stream::iter(fetches)
            .buffer_unordered(MAX_CONCURRENT_FILE_FETCHES_PER_REQUEST)
            .inspect(move |response| {
                if let Ok(result) = &response {
                    if result.result.is_ok() {
                        ctx.session()
                            .bump_load(Metric::GetpackFiles, Scope::Regional, 1.0);

                        STATS::files_served.add_value(1);
                    }
                }
            })
            .boxed())
    }
}

async fn fetch_file_response<R: MononokeRepo>(
    repo: HgRepoContext<R>,
    key: Key,
    attrs: FileAttributes,
) -> Result<FileResponse, Error> {
    let result = fetch_file(repo, key.clone(), attrs)
        .await
        .map_err(|e| ServerError::generic(format!("{e}")));
    Ok(FileResponse { key, result })
}

/// Fetch requested file for a single key.
/// Note that this function consumes the repo context in order
/// to construct a file context for the requested blob.
async fn fetch_file<R: MononokeRepo>(
    repo: HgRepoContext<R>,
    key: Key,
    attrs: FileAttributes,
) -> Result<FileEntry, Error> {
    let id = <HgFileNodeId as HgDataId<R>>::from_node_hash(HgNodeHash::from(key.hgid));

    let ctx = id
        .context(repo)
        .await
        .with_context(|| SaplingRemoteApiServiceError::FileFetchFailed(key.clone()))?
        .with_context(|| SaplingRemoteApiServiceError::KeyDoesNotExist(key.clone()))?;

    let parents = ctx.hg_parents().into();
    let mut file = FileEntry::new(key.clone(), parents);

    let fetch_content = async {
        if attrs.content {
            Ok(Some(ctx.content().await.with_context(|| {
                SaplingRemoteApiServiceError::FileFetchFailed(key.clone())
            })?))
        } else {
            anyhow::Ok(None)
        }
    };

    let fetch_aux_data = async {
        if attrs.aux_data {
            Ok(Some(ctx.content_metadata().await.with_context(|| {
                SaplingRemoteApiServiceError::FileAuxDataFetchFailed(key.clone())
            })?))
        } else {
            anyhow::Ok(None)
        }
    };

    let (content, aux_data) = futures::try_join!(fetch_content, fetch_aux_data)?;

    if let Some((hg_file_blob, metadata)) = content {
        file = file.with_content(FileContent {
            hg_file_blob: hg_file_blob.into(),
            metadata,
        });
    }

    if let Some(content_metadata) = aux_data {
        file = file.with_aux_data(FileAuxData {
            total_size: content_metadata.total_size,
            sha1: content_metadata.sha1.into(),
            blake3: content_metadata.seeded_blake3.into(),
            file_header_metadata: Some(ctx.file_header_metadata().into()),
        });
    }

    Ok(file)
}

// Sapling wants to use files the same way for Hg and Git, so shaping somehow
// the git object to fit within the defined FileResponse
async fn fetch_git_object_as_file<R: MononokeRepo>(
    key: Key,
    repo: HgRepoContext<R>,
) -> Result<FileResponse, Error> {
    let result = fetch_git_object(key.hgid, &repo)
        .await
        .map(|bytes| FileEntry {
            key: key.clone(),
            parents: Parents::None,
            aux_data: None,
            content: Some(FileContent {
                hg_file_blob: bytes.bytes.into(),
                metadata: Metadata {
                    size: None,
                    flags: None,
                },
            }),
        });
    Ok(FileResponse {
        key,
        result: result.map_err(|e| ServerError::generic(format!("{e}"))),
    })
}

/// Generate an upload token for already uploaded content
async fn generate_upload_token<R>(
    _repo: HgRepoContext<R>,
    id: AnyFileContentId,
    content_size: u64,
    bubble_id: Option<NonZeroU64>,
) -> Result<UploadToken, Error> {
    // At first, returns a fake token
    Ok(UploadToken::new_fake_token_with_metadata(
        AnyId::AnyFileContentId(id),
        bubble_id,
        UploadTokenMetadata::FileContentTokenMetadata(FileContentTokenMetadata { content_size }),
    ))
}

/// Upload content of a file requested by the client.
pub async fn upload_file(state: &mut State) -> Result<impl TryIntoResponse + use<>, HttpError> {
    let params = UploadFileParams::take_from(state);
    let query_string = UploadFileQueryString::take_from(state);

    state.put(HandlerInfo::new(
        &params.repo,
        SaplingRemoteApiMethod::UploadFile,
    ));

    let rctx = RequestContext::borrow_from(state).clone();
    let sctx = ServerContext::borrow_from(state);

    let repo: HgRepoContext<Repo> = get_repo(sctx, &rctx, &params.repo, None).await?;

    let bubble_id = match (query_string.bubble_id, query_string.snapshot) {
        (Some(_), Some(_)) => {
            return Err(HttpError::e400(anyhow!(
                "Name either a bubble or a snapshot, not both"
            )));
        }
        (bubble_id, None) => bubble_id.map(BubbleId::new),
        (None, Some(snapshot)) => {
            let repo_name = repo.repo_ctx().name();
            if !bubble_hg_mapping_enabled(repo_name) {
                return Err(HttpError::e400(anyhow!(
                    "Bubble hg mapping is disabled for repo {repo_name}, so snapshots cannot be named by hg id"
                )));
            }
            let hg_cs_id = HgChangesetId::from_str(&snapshot).map_err(HttpError::e400)?;
            let bubble_id = repo
                .ephemeral_store()
                .bubble_from_hg_changeset(repo.ctx(), hg_cs_id)
                .await
                .map_err(HttpError::e500)?
                .ok_or_else(|| HttpError::e404(anyhow!("Snapshot {hg_cs_id} not found")))?;
            Some(bubble_id)
        }
    };

    let id = AnyFileContentId::from_str(&format!("{}/{}", params.idtype, params.id))
        .map_err(HttpError::e400)?;
    let body = Body::take_from(state)
        .into_data_stream()
        .map_err(Error::from);
    let content_size = query_string.content_size;
    let compression = query_string.compression;

    let (body, content_size) = match compression.as_deref() {
        Some("zstd") => {
            let body = body.map_err(|e| std::io::Error::other(format!("{e:?}")));
            let decoder = ZstdDecoder::new(tokio_util::io::StreamReader::new(body));
            let body = tokio_util::io::ReaderStream::new(decoder).map_err(|e| e.into());
            Ok((body.left_stream(), content_size))
        }
        None => Ok((body.right_stream(), content_size)),
        Some(compression) => Err(HttpError::e400(anyhow!(
            "Unsupported compression type: {compression:?}"
        ))),
    }?;

    let bypass_redaction = if repo
        .repo_ctx()
        .config()
        .mirror_upload_redaction_bypass_enabled
    {
        repo.repo_ctx()
            .authorization_context()
            .check_mirror_upload_operations(repo.ctx(), repo.repo())
            .await
            .is_permitted()
    } else {
        false
    };
    repo.store_file(id, content_size, body, bubble_id, bypass_redaction)
        .await
        .map_err(HttpError::e500)?;

    let token = generate_upload_token(repo, id, content_size, bubble_id.map(NonZeroU64::from))
        .await
        .map(|v| v.to_wire());

    Ok(cbor_stream_filtered_errors(super::monitor_request(
        state,
        stream::iter(vec![token]),
    )))
}

/// Store the content of a single HgFilenode
async fn store_hg_filenode<R: MononokeRepo>(
    repo: HgRepoContext<R>,
    item: UploadHgFilenodeRequest,
) -> Result<UploadTokensResponse, Error> {
    // TODO(liubovd): validate signature of the upload token (item.token) and
    // return 'SaplingRemoteApiServiceError::UploadHgFilenodeRequestInvalidToken' if it's invalid.
    // This will be added later, for now assume tokens are always valid.

    let node_id = item.data.node_id;
    let token = item.data.file_content_upload_token;

    let filenode = <HgFileNodeId as HgDataId<R>>::from_node_hash(HgNodeHash::from(node_id));

    let p1: Option<HgFileNodeId> = item
        .data
        .parents
        .p1()
        .cloned()
        .map(HgNodeHash::from)
        .map(<HgFileNodeId as HgDataId<R>>::from_node_hash);

    let p2: Option<HgFileNodeId> = item
        .data
        .parents
        .p2()
        .cloned()
        .map(HgNodeHash::from)
        .map(<HgFileNodeId as HgDataId<R>>::from_node_hash);

    let any_file_content_id = match token.data.id {
        AnyId::AnyFileContentId(id) => Some(id),
        _ => None,
    }
    .ok_or_else(|| {
        SaplingRemoteApiServiceError::UploadHgFilenodeRequestInvalidToken(
            node_id.clone(),
            "the provided token is not for file content".into(),
        )
    })?;

    let content_id = repo
        .convert_file_to_content_id(any_file_content_id, None)
        .await?
        .ok_or_else(|| format_err!("File from upload token should be present"))?;

    let content_size = match token.data.metadata {
        Some(UploadTokenMetadata::FileContentTokenMetadata(meta)) => meta.content_size,
        _ => repo.fetch_file_content_size(content_id, None).await?,
    };

    let metadata = Bytes::from(item.data.metadata);

    // If a file is both merged and copied, we must store the parent in the "p2" field and leave the "p1" field null.
    // Detect that through the presence of copy metadata.
    match File::extract_copied_from(&metadata)? {
        Some(_copy_from) => {
            ensure!(
                p2.is_none(),
                "Copy metadata is not valid for merged filenodes: {filenode}"
            );
            repo.store_hg_filenode(filenode, None, p1, content_id, content_size, metadata)
                .await?;
        }
        _ => {
            repo.store_hg_filenode(filenode, p1, p2, content_id, content_size, metadata)
                .await?;
        }
    }

    Ok(UploadTokensResponse {
        token: UploadToken::new_fake_token(AnyId::HgFilenodeId(node_id), None),
    })
}

/// Upload list of hg filenodes requested by the client (batch request).
pub struct UploadHgFilenodesHandler;

#[async_trait]
impl SaplingRemoteApiHandler for UploadHgFilenodesHandler {
    type Request = Batch<UploadHgFilenodeRequest>;
    type Response = UploadTokensResponse;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::UploadHgFilenodes;
    const ENDPOINT: &'static str = "/upload/filenodes";

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let repo = ectx.repo();
        let tokens = request
            .batch
            .into_iter()
            .map(move |item| store_hg_filenode(repo.clone(), item));
        Ok(stream::iter(tokens)
            .buffer_unordered(MAX_CONCURRENT_UPLOAD_FILENODES_PER_REQUEST)
            .boxed())
    }
}

/// Downloads a file given an upload token
pub struct DownloadFileHandler;

#[async_trait]
impl SaplingRemoteApiHandler for DownloadFileHandler {
    type Request = UploadToken;
    type Response = Bytes;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::DownloadFile;
    const ENDPOINT: &'static str = "/download/file";

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let repo = ectx.repo();
        let content = repo
            .download_file(request)
            .await?
            .context("File not found")?;
        Ok(content.boxed())
    }
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;
    use types::HgId;
    use types::RepoPathBuf;

    use super::*;

    fn file_spec() -> FileSpec {
        FileSpec {
            key: Key::new(RepoPathBuf::new(), HgId::null_id().clone()),
            attrs: FileAttributes {
                content: true,
                aux_data: false,
            },
        }
    }

    #[mononoke::test]
    fn batch_size_counts_reqs() {
        let request = FileRequest {
            reqs: vec![file_spec(), file_spec(), file_spec()],
        };
        assert_eq!(Files2Handler::batch_size(&request), Some(3));
    }

    #[mononoke::test]
    fn batch_size_empty_is_zero() {
        let request = FileRequest { reqs: vec![] };
        assert_eq!(Files2Handler::batch_size(&request), Some(0));
    }
}
