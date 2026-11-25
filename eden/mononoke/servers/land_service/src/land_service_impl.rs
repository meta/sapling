/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;

use anyhow::Result;
use anyhow::anyhow;
use artillery_http_ext::ArtilleryTraceGuard;
use artillery_http_ext::current_trace_id;
use artillery_http_ext::start_trace;
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

/// Operation name for land traces rooted here rather than continued, so that a
/// root is distinguishable from a continuation in Tracery.
const ROOT_OPERATION_NAME: &str = "land_service_root";

/// Endpoint label bound to the `mononoke_endpoint` Luna sample key of the
/// `mononoke_scm` policy. Kept equal to the Thrift method name, as SLAPI and SCS
/// keep theirs, and it matches the policy's `land` prefix so a land rates the
/// same whichever transport carried it.
const LAND_ENDPOINT_LABEL: &str = "land_changesets";

fn tw_job_name() -> Option<&'static str> {
    static TW_JOB_NAME: OnceLock<Option<String>> = OnceLock::new();
    TW_JOB_NAME
        .get_or_init(|| std::env::var("TW_JOB_NAME").ok())
        .as_deref()
}

/// Root an Artillery trace for a land that arrived without one.
///
/// Inbound traces are continued further up by `ContextPropModule`, so a request
/// that already has one is left alone: re-rooting would sever it from its
/// caller. Whether a root is actually sampled is the `mononoke_scm` policy's
/// decision, so `None` here is the normal case rather than a failure.
fn maybe_root_trace(fb: FacebookInit) -> Option<ArtilleryTraceGuard> {
    if !justknobs::eval(
        "scm/artillery:root_land_service_traces",
        None,
        tw_job_name(),
    ) {
        return None;
    }
    if current_trace_id().is_some() {
        return None;
    }
    start_trace(fb, ROOT_OPERATION_NAME, LAND_ENDPOINT_LABEL)
}

/// Name the land on the block serving it, so the last hop of a diff land says
/// what it landed instead of only which Thrift method ran. `ContextPropModule`
/// continues an inbound trace before this handler, so for a propagated land
/// this annotates the caller's block -- the same block `maybe_root_trace`
/// declined to root over.
///
/// `admitted_as` and the batching decision are recorded because they pick the
/// code path: two lands of the same stack onto the same bookmark can still
/// take different routes through this server.
fn annotate_land_block(
    request: &LandChangesetRequest,
    admitted_as: Option<&'static str>,
    batched: bool,
) {
    tracing::event!(
        target: artillery_http_ext::TRACING_TARGET,
        tracing::Level::INFO,
        artillery.action = artillery_http_ext::LOG_TO_REQUEST_BLOCK,
        land.repo = request.repo_name.as_str(),
        land.bookmark = request.bookmark.as_str(),
        land.changesets_count = request.changesets.len(),
        land.admitted_as = admitted_as.unwrap_or_default(),
        land.batched = batched,
    );
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
        let _artillery_trace = maybe_root_trace(ctx.fb);
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

        let batched = justknobs::eval(
            "scm/mononoke:land_service_batching_enabled",
            None,
            Some(&land_changesets.repo_name),
        );

        annotate_land_block(&land_changesets, admitted_as, batched);

        if batched {
            let client = self
                .get_or_create_batched_pushrebase_client(&land_changesets.repo_name)
                .await;
            Ok(worker::impl_land_changesets_batched(ctx, repo, client, land_changesets).await?)
        } else {
            Ok(worker::impl_land_changesets(ctx, repo, land_changesets).await?)
        }
    }
}

#[cfg(test)]
mod tests {
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::with_just_knobs;
    use mononoke_macros::mononoke;

    use super::*;

    fn knobs(enabled: bool) -> JustKnobsInMemory {
        JustKnobsInMemory::new(HashMap::from([(
            "scm/artillery:root_land_service_traces".to_string(),
            KnobVal::Bool(enabled),
        )]))
    }

    /// The `mononoke_scm` Luna expression selects its 1:1 land branch by
    /// matching this label against the prefix `land`. Drifting off it demotes
    /// every land arriving here to the thin catch-all rate, which would leave
    /// the server rooting traces for lands it was added to cover.
    #[mononoke::test]
    fn the_land_label_matches_the_policy_prefix() {
        assert!(LAND_ENDPOINT_LABEL.starts_with("land"));
    }

    /// Guards the label drifting away from the method. SLAPI and SCS both keep
    /// theirs equal to the name the land arrived under, so that the policy, the
    /// ODS stats and the traces all name endpoints the same way.
    #[mononoke::test]
    fn the_label_is_the_thrift_method_name() {
        assert_eq!(LAND_ENDPOINT_LABEL, "land_changesets");
    }

    /// A root has to be distinguishable from a continuation in Tracery, and
    /// from the roots the other servers start.
    #[mononoke::test]
    fn the_root_operation_name_names_this_server() {
        assert_eq!(ROOT_OPERATION_NAME, "land_service_root");
        assert_ne!(ROOT_OPERATION_NAME, "scs_method_root");
        assert_ne!(ROOT_OPERATION_NAME, "mononoke_http_request_root");
    }

    #[mononoke::fbinit_test]
    fn rooting_does_nothing_while_the_killswitch_is_off(fb: FacebookInit) {
        with_just_knobs(knobs(false), || {
            assert!(
                maybe_root_trace(fb).is_none(),
                "no trace should be rooted while the knob is off"
            );
        });
    }

    /// With the knob on, the start path runs for real through the cxx bridge.
    /// Asserts nothing about the outcome: sampling is the `mononoke_scm`
    /// policy's decision and the policy lives in configerator, out of reach of
    /// a unit test. What is checked is that the path cannot be made to panic.
    #[mononoke::fbinit_test]
    fn rooting_a_land_is_infallible_when_enabled(fb: FacebookInit) {
        with_just_knobs(knobs(true), || {
            let _ = maybe_root_trace(fb);
        });
    }

    /// End-to-end over the real Artillery cxx bridge: synthesise the inbound
    /// context a traced caller would send, continue it, and check that a trace
    /// genuinely becomes current and that this server then declines to root
    /// over it. Re-rooting would sever the land from the WWW request that asked
    /// for it — the one thing this server must never do to a traced land.
    ///
    /// Continuation is what makes this assertable without the `mononoke_scm`
    /// policy existing: `continue_managed_trace` bypasses sampling and rate
    /// limiting, where `start_managed_trace` does not.
    #[cfg(fbcode_build)]
    #[mononoke::fbinit_test]
    fn an_inbound_trace_is_continued_and_never_re_rooted(fb: FacebookInit) {
        // W3C traceparent: version-traceid-spanid-sampled.
        const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

        with_just_knobs(knobs(true), || {
            let mut inbound = tracer::parse_traceparent_to_context(TRACEPARENT)
                .expect("a well-formed traceparent should parse");
            tracer::add_policy_to_context(inbound.pin_mut(), "mononoke_scm");

            let _continued = tracer::Tracer::new(fb, "mononoke_scm")
                .continue_managed_trace(&inbound)
                .expect("continuation bypasses sampling, so it must succeed");

            assert!(
                current_trace_id().is_some(),
                "continuing an inbound context should make a trace current"
            );
            assert!(
                maybe_root_trace(fb).is_none(),
                "a land that already carries a trace must not be re-rooted"
            );
        });
    }
}
