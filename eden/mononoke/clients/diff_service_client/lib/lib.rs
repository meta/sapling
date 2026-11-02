/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::pin::Pin;
use std::sync::Arc;

use anyhow::Error;
use anyhow::Result;
use context::CoreContext;
use diff_service_if as thrift;
use diff_service_if::MetadataDiffRequest;
use diff_service_if::MetadataDiffResponse;
use diff_service_if_clients::DiffService;
use diff_service_if_clients::errors::CommitCompareError;
use diff_service_if_clients::errors::DiffHunksError;
use diff_service_if_clients::errors::DiffUnifiedError;
use diff_service_if_clients::errors::DiffUnifiedHeaderlessError;
use diff_service_if_clients::errors::DiffUnifiedHeaderlessUnaryError;
use diff_service_if_clients::errors::DiffUnifiedUnaryError;
use diff_service_if_clients::errors::MetadataDiffError;
use fb303_core::fb303_status;
use fbinit::FacebookInit;
use futures::Stream;
use futures::StreamExt;
use mononoke_types::ChangesetId;
use mononoke_types::ContentId;
use source_control as source_control_thrift;

#[cfg(fbcode_build)]
mod facebook;
#[cfg(not(fbcode_build))]
mod oss;

#[cfg(fbcode_build)]
use facebook as platform;
#[cfg(not(fbcode_build))]
use oss as platform;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsPointer {
    pub sha256: String,
    pub size: i64,
}

impl LfsPointer {
    fn into_thrift(self) -> thrift::LfsPointer {
        thrift::LfsPointer {
            sha256: self.sha256,
            size: self.size,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone)]
pub enum DiffInput {
    Content {
        content_id: ContentId,
        path: Option<String>,
        lfs_pointer: Option<LfsPointer>,
    },
    CommitPath {
        commit_id: thrift::CommitId,
        path: String,
        replacement_path: Option<String>,
    },
    String {
        content: String,
    },
}

impl DiffInput {
    pub fn content(content_id: ContentId) -> Self {
        Self::Content {
            content_id,
            path: None,
            lfs_pointer: None,
        }
    }

    pub fn content_with_path(content_id: ContentId, path: String) -> Self {
        Self::Content {
            content_id,
            path: Some(path),
            lfs_pointer: None,
        }
    }

    pub fn changeset_path(changeset_id: ChangesetId, path: String) -> Self {
        Self::CommitPath {
            commit_id: thrift::CommitId::bonsai(changeset_id.as_ref().to_vec()),
            path,
            replacement_path: None,
        }
    }

    pub fn changeset_path_with_replacement(
        changeset_id: ChangesetId,
        path: String,
        replacement_path: String,
    ) -> Self {
        Self::CommitPath {
            commit_id: thrift::CommitId::bonsai(changeset_id.as_ref().to_vec()),
            path,
            replacement_path: Some(replacement_path),
        }
    }

    pub fn commit_path(commit_id: thrift::CommitId, path: String) -> Self {
        Self::CommitPath {
            commit_id,
            path,
            replacement_path: None,
        }
    }

    pub fn string(content: String) -> Self {
        Self::String { content }
    }

    fn into_thrift(self) -> thrift::DiffSingleInput {
        match self {
            DiffInput::Content {
                content_id,
                path,
                lfs_pointer,
            } => thrift::DiffSingleInput::content_id(thrift::DiffInputContentId {
                content_id: content_id.as_ref().to_vec(),
                path,
                lfs_pointer: lfs_pointer.map(|lfs| lfs.into_thrift()),
                ..Default::default()
            }),
            DiffInput::CommitPath {
                commit_id,
                path,
                replacement_path,
            } => thrift::DiffSingleInput::commit_path(thrift::DiffInputCommitPath {
                commit_id,
                path,
                replacement_path,
                ..Default::default()
            }),
            DiffInput::String { content } => {
                thrift::DiffSingleInput::str(thrift::DiffInputString {
                    content,
                    ..Default::default()
                })
            }
        }
    }
}

#[cfg(fbcode_build)]
pub use facebook::DEFAULT_DIFF_SERVICE_TIER;

#[derive(Clone)]
pub struct DiffServiceClient {
    thrift_client: platform::Client,
    status_client: platform::StatusClient,
}

impl DiffServiceClient {
    pub fn from_host_port(fb: FacebookInit, host_port: String) -> Result<Self> {
        let (thrift_client, status_client) = platform::from_host_port(fb, host_port)?;
        Ok(Self {
            thrift_client,
            status_client,
        })
    }

    fn get_service_client(
        &self,
        ctx: Option<&CoreContext>,
    ) -> Result<Arc<dyn DiffService + Send + Sync>> {
        platform::get_service_client(&self.thrift_client, ctx)
    }

    pub async fn diff_hunks(
        &self,
        ctx: &CoreContext,
        request: &thrift::DiffHunksRequest,
    ) -> Result<
        (
            thrift::DiffHunksResponse,
            Pin<Box<dyn Stream<Item = Result<thrift::DiffHunksData, Error>> + Send>>,
        ),
        DiffHunksError,
    > {
        let (response, stream) = self
            .get_service_client(Some(ctx))?
            .diff_hunks(request)
            .await?;

        let converted_stream = stream.map(|result| result.map_err(Error::from));

        Ok((response, Box::pin(converted_stream)))
    }

    pub async fn diff_unified(
        &self,
        ctx: &CoreContext,
        request: &thrift::DiffUnifiedRequest,
    ) -> Result<
        (
            thrift::DiffUnifiedResponse,
            Pin<Box<dyn Stream<Item = Result<thrift::DiffUnifiedChunk, Error>> + Send>>,
        ),
        DiffUnifiedError,
    > {
        let (response, stream) = self
            .get_service_client(Some(ctx))?
            .diff_unified(request)
            .await?;

        let converted_stream = stream.map(|result| result.map_err(Error::from));

        Ok((response, Box::pin(converted_stream)))
    }

    pub async fn diff_unified_headerless(
        &self,
        ctx: &CoreContext,
        request: &thrift::DiffUnifiedHeaderlessRequest,
    ) -> Result<
        (
            thrift::DiffUnifiedHeaderlessResponse,
            Pin<Box<dyn Stream<Item = Result<thrift::DiffUnifiedHeaderlessChunk, Error>> + Send>>,
        ),
        DiffUnifiedHeaderlessError,
    > {
        let (response, stream) = self
            .get_service_client(Some(ctx))?
            .diff_unified_headerless(request)
            .await?;

        let converted_stream = stream.map(|result| result.map_err(Error::from));

        Ok((response, Box::pin(converted_stream)))
    }

    pub async fn diff_unified_unary(
        &self,
        ctx: &CoreContext,
        request: &thrift::DiffUnifiedRequest,
    ) -> Result<thrift::DiffUnifiedUnaryResponse, DiffUnifiedUnaryError> {
        let response = self
            .get_service_client(Some(ctx))?
            .diff_unified_unary(request)
            .await?;

        Ok(response)
    }

    pub async fn diff_unified_headerless_unary(
        &self,
        ctx: &CoreContext,
        request: &thrift::DiffUnifiedHeaderlessRequest,
    ) -> Result<thrift::DiffUnifiedHeaderlessUnaryResponse, DiffUnifiedHeaderlessUnaryError> {
        let response = self
            .get_service_client(Some(ctx))?
            .diff_unified_headerless_unary(request)
            .await?;

        Ok(response)
    }

    pub async fn metadata_diff(
        &self,
        ctx: &CoreContext,
        request: &MetadataDiffRequest,
    ) -> Result<MetadataDiffResponse, MetadataDiffError> {
        let response = self
            .get_service_client(Some(ctx))?
            .metadata_diff(request)
            .await?;

        Ok(response)
    }

    pub async fn commit_compare(
        &self,
        ctx: &CoreContext,
        request: &thrift::CommitCompareRequest,
    ) -> Result<source_control_thrift::CommitCompareResponse, CommitCompareError> {
        let response = self
            .get_service_client(Some(ctx))?
            .commit_compare(request)
            .await?;

        Ok(response)
    }

    /// Returns the server's fb303 status. The OSS transport has no status
    /// interface, so in OSS builds this is `ALIVE` whenever the server accepts
    /// connections.
    pub async fn status(&self) -> Result<fb303_status> {
        platform::status(&self.status_client).await
    }
}

#[derive(Clone)]
pub struct RepoDiffServiceClient {
    repo_name: String,
    client: DiffServiceClient,
}

impl RepoDiffServiceClient {
    pub fn new(repo_name: String, client: DiffServiceClient) -> Self {
        Self { repo_name, client }
    }

    pub async fn status(&self) -> Result<fb303_status> {
        self.client.status().await
    }

    pub async fn diff_hunks(
        &self,
        ctx: &CoreContext,
        base_input: Option<DiffInput>,
        other_input: Option<DiffInput>,
        ignore_whitespace: bool,
    ) -> Result<
        (
            thrift::DiffHunksResponse,
            Pin<Box<dyn Stream<Item = Result<thrift::DiffHunksData, Error>> + Send>>,
        ),
        DiffHunksError,
    > {
        let input = thrift::DiffInput {
            base: base_input.map(|input| input.into_thrift()),
            other: other_input.map(|input| input.into_thrift()),
            ..Default::default()
        };

        let request = thrift::DiffHunksRequest {
            repo_name: self.repo_name.clone(),
            input,
            ignore_whitespace,
            ..Default::default()
        };
        self.client.diff_hunks(ctx, &request).await
    }

    pub async fn diff_unified(
        &self,
        ctx: &CoreContext,
        base_input: Option<DiffInput>,
        other_input: Option<DiffInput>,
        options: thrift::DiffUnifiedOptions,
    ) -> Result<
        (
            thrift::DiffUnifiedResponse,
            Pin<Box<dyn Stream<Item = Result<thrift::DiffUnifiedChunk, Error>> + Send>>,
        ),
        DiffUnifiedError,
    > {
        let input = thrift::DiffInput {
            base: base_input.map(|input| input.into_thrift()),
            other: other_input.map(|input| input.into_thrift()),
            ..Default::default()
        };

        let request = thrift::DiffUnifiedRequest {
            repo_name: self.repo_name.clone(),
            input,
            options,
            ..Default::default()
        };
        self.client.diff_unified(ctx, &request).await
    }

    pub async fn diff_unified_headerless(
        &self,
        ctx: &CoreContext,
        base_input: Option<DiffInput>,
        other_input: Option<DiffInput>,
        options: Option<thrift::DiffUnifiedHeaderlessOptions>,
    ) -> Result<
        (
            thrift::DiffUnifiedHeaderlessResponse,
            Pin<Box<dyn Stream<Item = Result<thrift::DiffUnifiedHeaderlessChunk, Error>> + Send>>,
        ),
        DiffUnifiedHeaderlessError,
    > {
        let input = thrift::DiffInput {
            base: base_input.map(|input| input.into_thrift()),
            other: other_input.map(|input| input.into_thrift()),
            ..Default::default()
        };

        let request = thrift::DiffUnifiedHeaderlessRequest {
            repo_name: self.repo_name.clone(),
            input,
            options,
            ..Default::default()
        };
        self.client.diff_unified_headerless(ctx, &request).await
    }

    pub async fn diff_unified_unary(
        &self,
        ctx: &CoreContext,
        base_input: Option<DiffInput>,
        other_input: Option<DiffInput>,
        options: thrift::DiffUnifiedOptions,
    ) -> Result<thrift::DiffUnifiedUnaryResponse, DiffUnifiedUnaryError> {
        let input = thrift::DiffInput {
            base: base_input.map(|input| input.into_thrift()),
            other: other_input.map(|input| input.into_thrift()),
            ..Default::default()
        };

        let request = thrift::DiffUnifiedRequest {
            repo_name: self.repo_name.clone(),
            input,
            options,
            ..Default::default()
        };
        self.client.diff_unified_unary(ctx, &request).await
    }

    pub async fn diff_unified_headerless_unary(
        &self,
        ctx: &CoreContext,
        base_input: Option<DiffInput>,
        other_input: Option<DiffInput>,
        options: Option<thrift::DiffUnifiedHeaderlessOptions>,
    ) -> Result<thrift::DiffUnifiedHeaderlessUnaryResponse, DiffUnifiedHeaderlessUnaryError> {
        let input = thrift::DiffInput {
            base: base_input.map(|input| input.into_thrift()),
            other: other_input.map(|input| input.into_thrift()),
            ..Default::default()
        };

        let request = thrift::DiffUnifiedHeaderlessRequest {
            repo_name: self.repo_name.clone(),
            input,
            options,
            ..Default::default()
        };
        self.client
            .diff_unified_headerless_unary(ctx, &request)
            .await
    }

    pub async fn metadata_diff(
        &self,
        ctx: &CoreContext,
        base_input: Option<DiffInput>,
        other_input: Option<DiffInput>,
        ignore_whitespace: bool,
    ) -> Result<MetadataDiffResponse, MetadataDiffError> {
        let input = thrift::DiffInput {
            base: base_input.map(|input| input.into_thrift()),
            other: other_input.map(|input| input.into_thrift()),
            ..Default::default()
        };

        let request = MetadataDiffRequest {
            repo_name: self.repo_name.clone(),
            input,
            ignore_whitespace,
            ..Default::default()
        };
        self.client.metadata_diff(ctx, &request).await
    }

    pub async fn commit_compare(
        &self,
        ctx: &CoreContext,
        commit_id: thrift::CommitId,
        params: source_control_thrift::CommitCompareParams,
    ) -> Result<source_control_thrift::CommitCompareResponse, CommitCompareError> {
        let request = thrift::CommitCompareRequest {
            repo_name: self.repo_name.clone(),
            commit_id,
            params,
            ..Default::default()
        };
        self.client.commit_compare(ctx, &request).await
    }
}
