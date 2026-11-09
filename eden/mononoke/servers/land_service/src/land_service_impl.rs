/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use context::CoreContext;
use context::SessionContainer;
use fbinit::FacebookInit;
use land_service_if::*;
use land_service_if_services::LandService;
use land_service_if_services::errors::LandChangesetsExn;
use mononoke_app::MononokeReposManager;
use mononoke_thrift_server::RequestContext;
use pushrebase_client::BatchedPushrebaseClient;
use scribe_ext::Scribe;
use scuba_ext::MononokeScubaSampleBuilder;

use crate::errors;
use crate::errors::LandChangesetsError;
use crate::platform;
use crate::repo::Repo;
use crate::worker;

#[derive(Clone)]
pub(crate) struct LandServiceImpl {
    fb: FacebookInit,
    scuba_builder: MononokeScubaSampleBuilder,
    repos_mgr: Arc<MononokeReposManager<Repo>>,
    scribe: Scribe,
    platform: platform::ServiceState,
    batched_pushrebase_clients:
        Arc<tokio::sync::Mutex<HashMap<String, BatchedPushrebaseClient<Repo>>>>,
}

impl LandServiceImpl {
    pub fn new(
        fb: FacebookInit,
        repos_mgr: Arc<MononokeReposManager<Repo>>,
        mut scuba_builder: MononokeScubaSampleBuilder,
        scribe: Scribe,
        platform: platform::ServiceState,
    ) -> Self {
        scuba_builder.add_common_server_data();
        Self {
            fb,
            repos_mgr,
            scuba_builder,
            scribe,
            platform,
            batched_pushrebase_clients: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    pub(crate) async fn create_ctx(
        &self,
        name: &str,
        req_ctxt: &RequestContext,
    ) -> Result<CoreContext, LandChangesetsError> {
        let session = self.create_session(req_ctxt).await?;
        let mut scuba = self.create_scuba(name, req_ctxt)?;
        scuba.add_metadata(session.metadata());
        let ctx = session.new_context_with_scribe(scuba, self.scribe.clone());
        Ok(ctx)
    }

    async fn get_or_create_batched_pushrebase_client(
        &self,
        repo_name: &str,
    ) -> BatchedPushrebaseClient<Repo> {
        let mut clients = self.batched_pushrebase_clients.lock().await;
        if let Some(client) = clients.get(repo_name) {
            return client.clone();
        }

        let repos = self.repos_mgr.repos().clone();
        let name = repo_name.to_owned();
        let batch_ctx = SessionContainer::new_with_defaults(self.fb)
            .new_context_with_scribe(self.scuba_builder.clone(), self.scribe.clone());
        let client = BatchedPushrebaseClient::new(batch_ctx, move || repos.get_by_name(&name));
        clients.insert(repo_name.to_owned(), client.clone());
        client
    }

    fn create_scuba(
        &self,
        name: &str,
        req_ctxt: &RequestContext,
    ) -> Result<MononokeScubaSampleBuilder, LandChangesetsError> {
        let mut scuba = self.scuba_builder.clone().with_seq("seq");
        scuba.add("type", "thrift");
        scuba.add("method", name);
        self.platform.add_scuba_params(req_ctxt, &mut scuba)?;
        Ok(scuba)
    }

    async fn create_session(
        &self,
        req_ctxt: &RequestContext,
    ) -> Result<SessionContainer, LandChangesetsError> {
        let metadata = self.platform.create_metadata(req_ctxt).await?;
        let session = SessionContainer::builder(self.fb)
            .metadata(Arc::new(metadata))
            .build();
        Ok(session)
    }
}

#[async_trait]
impl LandService for LandServiceImpl {
    type RequestContext = RequestContext;

    async fn land_changesets(
        &self,
        req_ctxt: &RequestContext,
        land_changesets: LandChangesetRequest,
    ) -> Result<LandChangesetsResponse, LandChangesetsExn> {
        let ctx: CoreContext = self.create_ctx("land_changesets", req_ctxt).await?;
        let repo = self
            .repos_mgr
            .repos()
            .get_by_name(&land_changesets.repo_name)
            .ok_or_else(|| {
                errors::internal_error(
                    anyhow!("Not finding the repo: {}", land_changesets.repo_name).as_ref(),
                )
            })?;

        let admitted_as = self
            .platform
            .caller_admission(&ctx, land_changesets.service_identity.as_deref())
            .await?;
        let ctx = ctx.with_mutated_scuba(|mut scuba| {
            scuba
                .add("repo_name", land_changesets.repo_name.as_str())
                .add("bookmark", land_changesets.bookmark.as_str())
                .add("changesets_count", land_changesets.changesets.len());
            if let Some(admitted_as) = admitted_as {
                scuba.add("admitted_as", admitted_as);
            }
            scuba
        });

        if justknobs::eval(
            "scm/mononoke:land_service_batching_enabled",
            None,
            Some(&land_changesets.repo_name),
        ) {
            let client = self
                .get_or_create_batched_pushrebase_client(&land_changesets.repo_name)
                .await;
            Ok(worker::impl_land_changesets_batched(ctx, repo, client, land_changesets).await?)
        } else {
            Ok(worker::impl_land_changesets(ctx, repo, land_changesets).await?)
        }
    }
}
