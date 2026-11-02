/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use bookmark_service_if::BookmarkKind;
use bookmark_service_if::DerivedDataSet;
use bookmark_service_if::GetRequest;
use bookmark_service_if::GetResponse;
use bookmark_service_if::ListBookmarksRequest;
use bookmark_service_if::ListBookmarksResponse;
use bookmark_service_if_clients::BookmarkService;
use bookmark_service_if_clients::errors::GetError;
use bookmark_service_if_clients::errors::ListBookmarksError;
use bookmarks_cache::BookmarksCache;
use bookmarks_cache::ScopedBookmarksCache;
use bookmarks_cache::WarmerRequirement;
use bookmarks_types::BookmarkKey;
use bookmarks_types::BookmarkKind as BookmarkKindRust;
use bookmarks_types::BookmarkPagination;
use bookmarks_types::BookmarkPrefix;
use context::CoreContext;
use fb303_core::fb303_status;
use fbinit::FacebookInit;
use mononoke_types::ChangesetId;

#[cfg(fbcode_build)]
mod facebook;
#[cfg(not(fbcode_build))]
mod oss;

#[cfg(fbcode_build)]
use facebook as platform;
#[cfg(not(fbcode_build))]
use oss as platform;

/// Converts WarmerRequirement to the thrift DerivedDataSet type.
/// Note: We can't use From trait here due to Rust orphan rules - both types
/// are defined in external crates.
fn warmer_requirement_to_derived_data_set(requirement: WarmerRequirement) -> DerivedDataSet {
    match requirement {
        WarmerRequirement::HgOnly => DerivedDataSet::HG_ONLY,
        WarmerRequirement::GitOnly => DerivedDataSet::GIT_ONLY,
        WarmerRequirement::AllKinds => DerivedDataSet::ALL_KINDS,
    }
}

#[derive(Clone)]
pub struct BookmarkServiceClient {
    thrift_client: platform::Client,
    status_client: platform::StatusClient,
}

impl BookmarkServiceClient {
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
    ) -> Result<Arc<dyn BookmarkService + Send + Sync>> {
        platform::get_service_client(&self.thrift_client, ctx)
    }

    pub async fn get(
        &self,
        ctx: &CoreContext,
        request: &GetRequest,
    ) -> Result<GetResponse, GetError> {
        self.get_service_client(Some(ctx))?.get(request).await
    }

    pub async fn list_bookmarks(
        &self,
        ctx: &CoreContext,
        request: &ListBookmarksRequest,
    ) -> Result<ListBookmarksResponse, ListBookmarksError> {
        self.get_service_client(Some(ctx))?
            .list_bookmarks(request)
            .await
    }

    pub async fn status(&self) -> Result<fb303_status> {
        platform::status(&self.status_client).await
    }
}

#[derive(Clone)]
#[facet::facet]
pub struct RepoBookmarkServiceClient {
    repo_name: String,
    client: BookmarkServiceClient,
    default_warmer_requirement: WarmerRequirement,
}

impl RepoBookmarkServiceClient {
    pub fn new(
        repo_name: String,
        client: BookmarkServiceClient,
        default_warmer_requirement: WarmerRequirement,
    ) -> Self {
        Self {
            repo_name,
            client,
            default_warmer_requirement,
        }
    }

    pub async fn status(&self) -> Result<fb303_status> {
        self.client.status().await
    }
}

#[async_trait]
impl ScopedBookmarksCache for RepoBookmarkServiceClient {
    async fn get(
        &self,
        ctx: &CoreContext,
        bookmark: &BookmarkKey,
        scope: WarmerRequirement,
    ) -> Result<Option<ChangesetId>, Error> {
        let response = self
            .client
            .get(
                ctx,
                &GetRequest {
                    repo_name: self.repo_name.to_string(),
                    bookmark_name: bookmark.to_string(),
                    derived_data_types: Some(warmer_requirement_to_derived_data_set(scope)),
                },
            )
            .await?;

        let maybe_cs_id = match response.changeset_id {
            Some(v) => Some(ChangesetId::from_thrift(v)?),
            None => None,
        };
        Ok(maybe_cs_id)
    }

    async fn list(
        &self,
        ctx: &CoreContext,
        prefix: &BookmarkPrefix,
        pagination: &BookmarkPagination,
        limit: Option<u64>,
        scope: WarmerRequirement,
    ) -> Result<Vec<(BookmarkKey, (ChangesetId, BookmarkKindRust))>, Error> {
        let prefix = prefix.to_string();
        let after = match pagination {
            BookmarkPagination::After(after) => Some(after.to_string()),
            BookmarkPagination::FromStart => None,
        };
        let limit = limit.map(|l| l as i64);
        let response = self
            .client
            .list_bookmarks(
                ctx,
                &ListBookmarksRequest {
                    repo_name: self.repo_name.to_string(),
                    prefix,
                    after,
                    limit,
                    derived_data_types: Some(warmer_requirement_to_derived_data_set(scope)),
                },
            )
            .await?;
        response
            .bookmarks
            .into_iter()
            .map(|spec| {
                let name = BookmarkKey::new(spec.name)?;
                let cs_id = ChangesetId::from_thrift(spec.cs_id)?;
                let kind = match spec.kind {
                    BookmarkKind::SCRATCH => BookmarkKindRust::Scratch,
                    BookmarkKind::PUBLISHING => BookmarkKindRust::Publishing,
                    BookmarkKind::PULL_DEFAULT_PUBLISHING => {
                        BookmarkKindRust::PullDefaultPublishing
                    }
                    _ => return Err(anyhow!("Unknown bookmark kind")),
                };
                Ok((name, (cs_id, kind)))
            })
            .collect()
    }

    async fn sync(&self, _ctx: &CoreContext) {}
}

/// Implements BookmarksCache by delegating to ScopedBookmarksCache with the configured default scope.
#[async_trait]
impl BookmarksCache for RepoBookmarkServiceClient {
    async fn get(
        &self,
        ctx: &CoreContext,
        bookmark: &BookmarkKey,
    ) -> Result<Option<ChangesetId>, Error> {
        ScopedBookmarksCache::get(self, ctx, bookmark, self.default_warmer_requirement).await
    }

    async fn list(
        &self,
        ctx: &CoreContext,
        prefix: &BookmarkPrefix,
        pagination: &BookmarkPagination,
        limit: Option<u64>,
    ) -> Result<Vec<(BookmarkKey, (ChangesetId, BookmarkKindRust))>, Error> {
        ScopedBookmarksCache::list(
            self,
            ctx,
            prefix,
            pagination,
            limit,
            self.default_warmer_requirement,
        )
        .await
    }

    async fn sync(&self, ctx: &CoreContext) {
        ScopedBookmarksCache::sync(self, ctx).await;
    }
}
