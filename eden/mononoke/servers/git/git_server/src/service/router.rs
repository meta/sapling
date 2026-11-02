/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::pin::Pin;

use bytes::Bytes;
use fbinit::FacebookInit;
use futures::FutureExt;
use futures_stats::futures03::TimedFutureExt;
use gotham::handler::HandlerFuture;
use gotham::middleware::state::StateMiddleware;
use gotham::pipeline::new_pipeline;
use gotham::pipeline::single_pipeline;
use gotham::router::Router;
use gotham::router::builder::DefineSingleRoute;
use gotham::router::builder::DrawRoutes;
use gotham::router::builder::build_router as gotham_build_router;
use gotham::state::FromState;
use gotham::state::State;
use gotham_ext::error::HttpError;
use gotham_ext::middleware::ScubaMiddlewareState;
use gotham_ext::response::BytesBody;
use gotham_ext::response::build_error_response;
use gotham_ext::response::build_response;
use http::HeaderMap;
use slapi_service::handlers::JsonErrorFormatter;
use slapi_service::handlers::handler::BasicPathExtractor;
use slapi_service::handlers::handler::PathExtractorWithRepo;
use stats::prelude::*;

use super::error_formatter::GitErrorFormatter;
use super::lfs_redirect::lfs_redirect_handler;
use crate::model::GitServerContext;
use crate::model::RepositoryParams;
use crate::model::ServiceType;
use crate::profiling;
use crate::read;
use crate::service::slapi_compat::GitHandlers;
use crate::write;

define_stats! {
    container_memory: dynamic_singleton_counter("{}", (counter_name: String)),
}

fn capability_advertisement_handler(mut state: State) -> Pin<Box<HandlerFuture>> {
    async move {
        let (future_stats, res) = read::capability_advertisement(&mut state).timed().await;
        ScubaMiddlewareState::try_set_future_stats(&mut state, &future_stats);
        match res {
            Ok(res) => Ok((state, res)),
            Err(err) => {
                println!("Encountered error {err:?}");
                build_error_response(err, state, &GitErrorFormatter)
            }
        }
    }
    .boxed()
}

fn upload_pack_handler(mut state: State) -> Pin<Box<HandlerFuture>> {
    async move {
        let (future_stats, res) = read::upload_pack(&mut state).timed().await;
        ScubaMiddlewareState::try_set_future_stats(&mut state, &future_stats);
        match res {
            Ok(res) => Ok((state, res)),
            Err(err) => {
                println!("Encountered error {err:?}");
                build_error_response(err, state, &GitErrorFormatter)
            }
        }
    }
    .boxed()
}

fn receive_pack_handler(mut state: State) -> Pin<Box<HandlerFuture>> {
    async move {
        let (future_stats, res) = write::receive_pack(&mut state).timed().await;
        ScubaMiddlewareState::try_set_future_stats(&mut state, &future_stats);
        match res {
            Ok(res) => Ok((state, res)),
            Err(err) => {
                println!("Encountered error {err:?}");
                build_error_response(err, state, &GitErrorFormatter)
            }
        }
    }
    .boxed()
}

fn clone_bundle_handler(mut state: State) -> Pin<Box<HandlerFuture>> {
    async move {
        let (future_stats, res) = read::clone_bundle(&mut state).timed().await;
        ScubaMiddlewareState::try_set_future_stats(&mut state, &future_stats);
        match res {
            Ok(res) => Ok((state, res)),
            Err(err) => {
                println!("Encountered error {err:?}");
                build_error_response(err, state, &GitErrorFormatter)
            }
        }
    }
    .boxed()
}

fn add_healthcheck_load_header(fb: FacebookInit, headers: &mut HeaderMap, avg_window: i64) {
    if avg_window <= 0 {
        return;
    }
    let counter = format!("container_memory_usage_percent.avg.{avg_window}");
    if let Some(load) = STATS::container_memory.get_value(fb, (counter,)) {
        let load = if load == 0 { 1 } else { load };
        headers.insert("X-FB-Load", load.into());
    }
}

fn health_handler(state: State) -> Pin<Box<HandlerFuture>> {
    async move {
        let headers = HeaderMap::borrow_from(&state);
        if let Some(wait_time) = headers
            .get("x-fb-healthcheck-wait-time-seconds")
            .and_then(|x| x.to_str().ok())
            .and_then(|t| t.parse().ok())
        {
            tokio::time::sleep(std::time::Duration::from_secs(wait_time)).await;
        }

        let mut res = gotham::helpers::http::response::create_response(
            &state,
            http::status::StatusCode::OK,
            mime::TEXT_PLAIN,
            "I_AM_ALIVE\n",
        );

        let fb = GitServerContext::borrow_from(&state).fb();
        let avg_window =
            justknobs::get_as::<i64>("scm/mononoke:git_server_healthcheck_load_avg_secs", None);
        add_healthcheck_load_header(fb, res.headers_mut(), avg_window);

        Ok((state, res))
    }
    .boxed()
}

fn flamegraph_handler(mut state: State) -> Pin<Box<HandlerFuture>> {
    async move {
        let (future_stats, res) = profiling::flamegraph(&mut state).timed().await;
        ScubaMiddlewareState::try_set_future_stats(&mut state, &future_stats);
        match res {
            Ok(res) => Ok((state, res)),
            Err(err) => build_error_response(err, state, &GitErrorFormatter),
        }
    }
    .boxed()
}

pub async fn get_capabilities() -> Result<BytesBody<Bytes>, HttpError> {
    let caps: Vec<&str> = vec!["commit-cloud"];
    let caps_json = serde_json::to_vec(&caps).map_err(|e| {
        HttpError::e500(anyhow::Error::from(e).context("converting capabilities to JSON"))
    })?;
    Ok(BytesBody::new(caps_json.into(), mime::APPLICATION_JSON))
}

fn slapi_capabilities_handler(mut state: State) -> Pin<Box<HandlerFuture>> {
    async move {
        ScubaMiddlewareState::try_borrow_add(&mut state, "method", "capabilities");
        let repo_name = BasicPathExtractor::borrow_from(&state).repo();
        ScubaMiddlewareState::try_borrow_add(&mut state, "repo", repo_name);
        let res = get_capabilities().await;
        build_response(res, state, &JsonErrorFormatter)
    }
    .boxed()
}

pub fn build_router(context: GitServerContext) -> Router {
    let pipeline = new_pipeline().add(StateMiddleware::new(context)).build();

    let (chain, pipelines) = single_pipeline(pipeline);

    gotham_build_router(chain, pipelines, |route| {
        route
            .get("/repos/git/:server_type/*repository/info/refs")
            .with_path_extractor::<RepositoryParams>()
            .with_query_string_extractor::<ServiceType>()
            .to(capability_advertisement_handler);

        route
            .post("/repos/git/:server_type/*repository/git-upload-pack")
            .with_path_extractor::<RepositoryParams>()
            .to(upload_pack_handler);

        route
            .post("/repos/git/:server_type/*repository/git-receive-pack")
            .with_path_extractor::<RepositoryParams>()
            .to(receive_pack_handler);

        route
            .get("/repos/git/:server_type/*repository/clone.bundle")
            .with_path_extractor::<RepositoryParams>()
            .to(clone_bundle_handler);

        route
            .post("/repos/git/:server_type/*repository/info/lfs/objects/batch")
            .with_path_extractor::<RepositoryParams>()
            .to(lfs_redirect_handler);

        route.get("/health_check").to(health_handler);
        route.get("/flamegraph").to(flamegraph_handler);

        // SLAPI endpoints
        route
            .get("/edenapi/*repo/capabilities")
            .with_path_extractor::<BasicPathExtractor>()
            .to(slapi_capabilities_handler);
        GitHandlers::setup::<slapi_service::handlers::commit_cloud::CommitCloudWorkspaces>(route);
        GitHandlers::setup::<slapi_service::handlers::commit_cloud::CommitCloudWorkspace>(route);
        GitHandlers::setup::<slapi_service::handlers::commit_cloud::CommitCloudReferences>(route);
        GitHandlers::setup::<slapi_service::handlers::commit_cloud::CommitCloudSmartlog>(route);
        GitHandlers::setup::<slapi_service::handlers::commit_cloud::CommitCloudUpdateReferences>(
            route,
        );
        GitHandlers::setup::<slapi_service::handlers::commit_cloud::CommitCloudOtherRepoWorkspaces>(
            route,
        );
        GitHandlers::setup::<slapi_service::handlers::lookup::LookupHandler>(route);
    })
}

#[cfg(all(test, fbcode_build))]
mod tests {
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::fbinit_test]
    fn healthcheck_load_reads_unprefixed_counter(fb: FacebookInit) {
        let avg_window = 123_001;
        let counter = stats::create_singleton_counter(format!(
            "container_memory_usage_percent.avg.{avg_window}"
        ));
        counter.set_value(fb, 37);

        let mut headers = HeaderMap::new();
        add_healthcheck_load_header(fb, &mut headers, avg_window);
        assert_eq!(headers.get("X-FB-Load").expect("load header"), "37");

        counter.set_value(fb, 0);
        add_healthcheck_load_header(fb, &mut headers, avg_window);
        assert_eq!(headers.get("X-FB-Load").expect("zero load header"), "1");
    }

    #[mononoke::fbinit_test]
    fn healthcheck_load_disabled_omits_header(fb: FacebookInit) {
        stats::create_singleton_counter("container_memory_usage_percent.avg.0".to_owned())
            .set_value(fb, 73);
        for avg_window in [0, -1] {
            let mut headers = HeaderMap::new();
            add_healthcheck_load_header(fb, &mut headers, avg_window);
            assert!(!headers.contains_key("X-FB-Load"));
        }
    }

    #[mononoke::fbinit_test]
    fn healthcheck_load_missing_counter_omits_header(fb: FacebookInit) {
        let mut headers = HeaderMap::new();
        add_healthcheck_load_header(fb, &mut headers, 123_002);
        assert!(!headers.contains_key("X-FB-Load"));
    }
}
