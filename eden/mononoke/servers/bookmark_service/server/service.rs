/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Error;
use bookmark_service_if::*;
use bookmark_service_if_services::errors::bookmark_service::*;
use bookmark_service_if_services::*;
use bookmarks_cache::ScopedBookmarksCache;
use bookmarks_cache::ScopedBookmarksCacheRef;
use bookmarks_cache::WarmerRequirement;
use bookmarks_types::BookmarkKind as BookmarkKindRust;
use bookmarks_types::BookmarkPagination;
use bookmarks_types::BookmarkPrefix;
use context::CoreContext;
use context::SessionContainer;
use context::SessionContainerBuilder;
use fbinit::FacebookInit;
use futures::future::BoxFuture;
use futures_stats::TimedFutureExt;
use mononoke_app::MononokeApp;
use mononoke_app::MononokeReposManager;
use mononoke_thrift_server::RequestContext;
use repo_identity::RepoIdentity;
use scuba_ext::MononokeScubaSampleBuilder;
use stats::prelude::*;
use time_ext::DurationExt;

use crate::errors::ThriftError;
use crate::platform;

define_stats! {
    prefix = "mononoke.bookmark_service";
    total_request_start: timeseries(Rate, Sum),
    total_request_success: timeseries(Rate, Sum),
    total_request_internal_failure: timeseries(Rate, Sum),
    total_request_invalid: timeseries(Rate, Sum),
    method_completion_time_ms: dynamic_histogram(
        "method.{}.completion_time_ms", (method: String);
        10, 0, 1_000, Average, Sum, Count; P 5; P 50; P 90),
}

#[derive(Clone)]
#[facet::container]
pub(crate) struct BookmarkServiceRepo {
    #[facet]
    repo_identity: RepoIdentity,
    #[facet]
    scoped_bookmarks_cache: dyn ScopedBookmarksCache,
}

pub fn internal_error<E: Into<Error>>(e: E) -> InternalError {
    InternalError {
        reason: format!("{:?}", e.into()),
    }
}

fn derived_data_set_to_warmer_requirement(
    derived_data_set: DerivedDataSet,
) -> Result<WarmerRequirement, ThriftError> {
    match derived_data_set {
        DerivedDataSet::HG_ONLY => Ok(WarmerRequirement::HgOnly),
        DerivedDataSet::GIT_ONLY => Ok(WarmerRequirement::GitOnly),
        DerivedDataSet::ALL_KINDS => Ok(WarmerRequirement::AllKinds),
        _ => Err(ThriftError::InternalError(anyhow::anyhow!(
            "Unknown DerivedDataSet value: {derived_data_set:?}"
        ))),
    }
}

macro_rules! impl_thrift_methods {
    ( $( async fn $method_name:ident($( $param_name:ident : $param_type:ty, )*) -> Result<$ok:ty, $err:ty>; )* ) => {
        $(
            fn $method_name<'implementation, 'req_ctxt, 'async_trait>(
                &'implementation self,
                req_ctxt: &'req_ctxt RequestContext,
                $( $param_name: $param_type ),*
            ) -> BoxFuture<'async_trait, Result<$ok, $err>>
            where
                'implementation: 'async_trait,
                'req_ctxt: 'async_trait,
                Self: Sync + 'async_trait,
            {
                let handler = async move {
                    let ctx = self.create_ctx(stringify!($method_name), req_ctxt, &$( $param_name ),* ).map_err(|e| <$err>::from(internal_error(e)))?;
                    ctx.scuba().clone().log_with_msg("Request start", None);
                    STATS::total_request_start.add_value(1);
                    let mut scuba = ctx.scuba().clone();
                    let perf_counters = ctx.perf_counters().clone();
                    let (stats, res) = BookmarkServiceImpl::$method_name(self, ctx, $( $param_name, )*).timed().await;

                    match res.as_ref() {
                        Err(err) => {
                            let (status, error) = match err {
                                ThriftError::RequestError(e) => {
                                    STATS::total_request_invalid.add_value(1);
                                    ("REQUEST_ERROR", format!("{:?}", e))
                                }
                                ThriftError::InternalError(e) => {
                                    STATS::total_request_internal_failure.add_value(1);
                                    ("INTERNAL_ERROR", format!("{:?}", e))
                                }
                            };
                            scuba.unsampled();
                            scuba.add("status", status);
                            scuba.add("error", error);
                        }
                        Ok(_) => {
                            STATS::total_request_success.add_value(1);
                            scuba.add("error", "");
                            scuba.add("status", "SUCCESS");
                        }
                    };
                    perf_counters.insert_perf_counters(&mut scuba);
                    scuba.add_future_stats(&stats);
                    STATS::method_completion_time_ms.add_value(
                        stats.completion_time.as_millis_unchecked() as i64,
                        (stringify!($method_name).to_string(),),
                    );

                    scuba.log_with_msg("Request complete", None);

                    let res  = res.map_err(|e| match e {
                        ThriftError::RequestError(e) => {
                            <$err>::from(RequestError { reason: e })
                        },
                        ThriftError::InternalError(e) => {
                            <$err>::from(internal_error(e))
                        },
                    });

                    res
                };
                Box::pin(handler)
            }
        )*
    }
}

#[derive(Clone)]
pub struct BookmarkServiceImpl {
    fb: FacebookInit,
    scuba: MononokeScubaSampleBuilder,
    repos_mgr: Arc<MononokeReposManager<BookmarkServiceRepo>>,
}

impl BookmarkServiceImpl {
    pub async fn new(
        app: &MononokeApp,
        repos_mgr: Arc<MononokeReposManager<BookmarkServiceRepo>>,
    ) -> Result<Self, Error> {
        let scuba = app.environment().scuba_sample_builder.clone();
        let fb = app.fb;
        Ok(Self {
            fb,
            scuba,
            repos_mgr,
        })
    }

    #[cfg(test)]
    pub async fn get_with_repos(
        repos: &mononoke_repos::MononokeRepos<BookmarkServiceRepo>,
        ctx: CoreContext,
        request: GetRequest,
    ) -> Result<GetResponse, ThriftError> {
        let maybe_repo = repos.get_by_name(&request.repo_name);

        let repo = maybe_repo.ok_or_else(|| RepoNotFound {
            reason: format!("Requested repo: {} not found", request.repo_name),
        })?;

        let scope = derived_data_set_to_warmer_requirement(
            request
                .derived_data_types
                .unwrap_or(DerivedDataSet::HG_ONLY),
        )?;

        let changeset_id = repo
            .scoped_bookmarks_cache()
            .get(&ctx, &request.bookmark_name.parse()?, scope)
            .await?
            .map(|cs_id| cs_id.into_thrift());
        Ok(GetResponse { changeset_id })
    }

    pub async fn get(
        &self,
        ctx: CoreContext,
        request: GetRequest,
    ) -> Result<GetResponse, ThriftError> {
        let maybe_repo = self.repos_mgr.repos().get_by_name(&request.repo_name);

        let repo = maybe_repo.ok_or_else(|| RepoNotFound {
            reason: format!("Requested repo: {} not found", request.repo_name),
        })?;

        let scope = derived_data_set_to_warmer_requirement(
            request
                .derived_data_types
                .unwrap_or(DerivedDataSet::HG_ONLY),
        )?;

        let changeset_id = repo
            .scoped_bookmarks_cache()
            .get(&ctx, &request.bookmark_name.parse()?, scope)
            .await?
            .map(|cs_id| cs_id.into_thrift());
        Ok(GetResponse { changeset_id })
    }

    pub async fn list_bookmarks(
        &self,
        ctx: CoreContext,
        request: ListBookmarksRequest,
    ) -> Result<ListBookmarksResponse, ThriftError> {
        let maybe_repo = self.repos_mgr.repos().get_by_name(&request.repo_name);

        let repo = maybe_repo.ok_or_else(|| RepoNotFound {
            reason: format!("Requested repo: {} not found", request.repo_name),
        })?;

        let prefix: BookmarkPrefix = request.prefix.parse()?;

        let pagination: BookmarkPagination = match request.after {
            Some(after) => BookmarkPagination::After(after.parse()?),
            None => BookmarkPagination::FromStart,
        };

        let limit = request.limit.map(|l| l as u64);

        let scope = derived_data_set_to_warmer_requirement(
            request
                .derived_data_types
                .unwrap_or(DerivedDataSet::HG_ONLY),
        )?;

        let bookmarks = repo
            .scoped_bookmarks_cache()
            .list(&ctx, &prefix, &pagination, limit, scope)
            .await?
            .into_iter()
            .map(|(key, (cs_id, kind))| {
                let name = key.to_string();
                let cs_id = cs_id.into_thrift();
                let kind = match kind {
                    BookmarkKindRust::Scratch => BookmarkKind::SCRATCH,
                    BookmarkKindRust::Publishing => BookmarkKind::PUBLISHING,
                    BookmarkKindRust::PullDefaultPublishing => {
                        BookmarkKind::PULL_DEFAULT_PUBLISHING
                    }
                };
                BookmarkSpec { name, cs_id, kind }
            })
            .collect();

        Ok(ListBookmarksResponse { bookmarks })
    }

    fn create_ctx(
        &self,
        method: &str,
        req_ctxt: &RequestContext,
        params: &dyn AddScubaParams,
    ) -> Result<CoreContext, ThriftError> {
        let session = self.create_session(req_ctxt)?;
        let mut scuba = self.create_scuba(method, req_ctxt, params)?;
        scuba.add_metadata(session.metadata());

        let ctx = session.new_context(scuba);
        Ok(ctx)
    }

    fn create_scuba(
        &self,
        method: &str,
        req_ctxt: &RequestContext,
        params: &dyn AddScubaParams,
    ) -> Result<MononokeScubaSampleBuilder, ThriftError> {
        let mut scuba = self.scuba.clone();
        scuba.add_common_server_data();
        platform::add_scuba_params(req_ctxt, &mut scuba)?;
        scuba.add("method", method);
        params.add_scuba_params(&mut scuba);
        Ok(scuba)
    }

    fn create_session(&self, req_ctxt: &RequestContext) -> Result<SessionContainer, ThriftError> {
        let metadata = platform::create_metadata(req_ctxt)?;
        let session = SessionContainerBuilder::new(self.fb)
            .metadata(Arc::new(metadata))
            .build();

        Ok(session)
    }
}

trait AddScubaParams: Send + Sync {
    fn add_scuba_params(&self, _scuba: &mut MononokeScubaSampleBuilder) {}
}

impl AddScubaParams for GetRequest {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("repo", self.repo_name.clone());
        scuba.add("bookmark", self.bookmark_name.clone());
        scuba.add("derived_data_set", format!("{:?}", self.derived_data_types));
    }
}

impl AddScubaParams for ListBookmarksRequest {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("repo", self.repo_name.clone());
        scuba.add("prefix", self.prefix.clone());
        scuba.add(
            "after",
            self.after.clone().unwrap_or_else(|| "".to_string()),
        );
        scuba.add("limit", self.limit.clone());
        scuba.add("derived_data_set", format!("{:?}", self.derived_data_types));
    }
}

impl BookmarkService for BookmarkServiceImpl {
    type RequestContext = RequestContext;

    impl_thrift_methods! {
        async fn get(request:GetRequest,) -> Result<GetResponse, GetExn>;
        async fn list_bookmarks(request:ListBookmarksRequest,) -> Result<ListBookmarksResponse, ListBookmarksExn>;
    }
}

#[cfg(test)]
#[path = "test_service_scope.rs"]
mod test_service_scope;
