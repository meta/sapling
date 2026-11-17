/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fmt;
use std::pin::Pin;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Error;
use anyhow::anyhow;
use edenapi_types::ToWire;
use futures::FutureExt;
use futures::Stream;
use futures::channel::oneshot;
use futures::stream::TryStreamExt;
use futures_stats::futures03::TimedFutureExt;
use gotham::handler::HandlerError as GothamHandlerError;
use gotham::handler::HandlerFuture;
use gotham::helpers::http::Body;
use gotham::middleware::state::StateMiddleware;
use gotham::pipeline::new_pipeline;
use gotham::pipeline::single_pipeline;
use gotham::router::Router;
use gotham::router::builder::DefineSingleRoute;
use gotham::router::builder::DrawRoutes;
use gotham::router::builder::RouterBuilder;
use gotham::router::builder::build_router as gotham_build_router;
use gotham::state::FromState;
use gotham::state::State;
use gotham_derive::StateData;
use gotham_ext::content_encoding::ContentEncoding;
use gotham_ext::error::ErrorFormatter;
use gotham_ext::error::HttpError;
use gotham_ext::handler::SlapiCommitIdentityScheme;
use gotham_ext::middleware::MetadataState;
use gotham_ext::middleware::load::RequestLoad;
use gotham_ext::middleware::request_context::RequestContext;
use gotham_ext::middleware::scuba::HttpScubaKey;
use gotham_ext::middleware::scuba::ScubaMiddlewareState;
use gotham_ext::response::PendingInBandErrors;
use gotham_ext::response::ResponseTryStreamExt;
use gotham_ext::response::StreamBody;
use gotham_ext::response::TryIntoResponse;
use gotham_ext::response::build_response;
use gotham_ext::state_ext::StateExt;
use http::Method;
use http::Response;
use http::Uri;
use mime::Mime;
use mononoke_api::Repo;
use mononoke_macros::mononoke;
use serde::Deserialize;
use serde::Serialize;
use time_ext::DurationExt;
use tracing::Instrument;

use crate::context::ServerContext;
use crate::middleware::request_dumper::RequestDumper;
use crate::scuba::SaplingRemoteApiScubaKey;
use crate::utils::cbor_mime;
use crate::utils::get_repo;
use crate::utils::monitor::Monitor;
use crate::utils::parse_wire_request;
use crate::utils::to_cbor_bytes;

mod blame;
mod bookmarks;
mod capabilities;
mod commit;
pub mod commit_cloud;
mod files;
mod git_objects;
pub mod handler;
mod history;
mod land;
mod legacy;
pub mod lookup;
mod path_history;
mod repos;
mod suffix_query;
mod trees;
pub(crate) use handler::HandlerResult;
pub(crate) use handler::PathExtractorWithRepo;
pub(crate) use handler::SaplingRemoteApiHandler;

use self::handler::SaplingRemoteApiContext;

const REPORTING_LOOP_WAIT: u64 = 5;

/// Endpoints where a land enters this server. Values are the labels bound to
/// the `mononoke_endpoint` Luna key, kept in step with the matching
/// `SaplingRemoteApiMethod`. `replay_identical_moves` also moves a bookmark but
/// is excluded: it replays already-landed moves onto shadow replicas.
const LAND_ENDPOINTS: &[(&str, &str)] = &[
    (land::LandStackHandler::ENDPOINT, "land_stack"),
    (bookmarks::SetBookmarkHandler::ENDPOINT, "set_bookmark"),
];

/// Label for everything that is not a land.
///
/// Deliberately one bucket rather than a per-endpoint label. Resolving the
/// actual `SaplingRemoteApiMethod` here would mean a second table of all 44
/// endpoints, maintained by hand alongside `build_router` and free to drift
/// from it. The policy only needs to tell lands apart from the rest, so that
/// is all this reports. Splitting this bucket later means giving `build_router`
/// and the table a shared source of handler types, not extending this list.
const OTHER_ENDPOINT: &str = "other";

/// Label every request for Artillery trace rooting. See
/// `gotham_ext::handler::RootTraceSelector`.
///
/// Every request is eligible: the rate each label actually gets is the
/// `mononoke_scm` policy's decision, not this function's. Returning `None`
/// would put that decision in code, where changing it needs a deploy.
///
/// Matched on the raw path, whose leading segment is the repo name: this runs
/// before routing, so `HandlerInfo` is not in `State` yet.
pub fn select_root_trace_endpoint(state: &State) -> Option<&'static str> {
    let method = Method::try_borrow_from(state)?;
    let path = Uri::try_borrow_from(state)?.path();
    Some(root_trace_endpoint_for(method, path))
}

/// Split out from [`select_root_trace_endpoint`] so the matching itself is
/// testable without standing up a gotham `State`.
fn root_trace_endpoint_for(method: &Method, path: &str) -> &'static str {
    if method != Method::POST {
        return OTHER_ENDPOINT;
    }
    LAND_ENDPOINTS
        .iter()
        .find(|(endpoint, _)| path.ends_with(endpoint))
        .map_or(OTHER_ENDPOINT, |(_, label)| *label)
}

/// Enum identifying the SaplingRemoteAPI method that each handler corresponds to.
/// Used to identify the handler for logging and stats collection.
#[derive(Copy, Clone)]
pub enum SaplingRemoteApiMethod {
    AlterSnapshot,
    Blame,
    Bookmarks2,
    Capabilities,
    CheckManifestPermission,
    CheckPathPermission,
    CloudHistoricalVersions,
    CloudOtherRepoWorkspaces,
    CloudReferences,
    CloudRenameWorkspace,
    CloudRollbackWorkspace,
    CloudShareWorkspace,
    CloudSmartlog,
    CloudSmartlogByVersion,
    CloudUpdateArchive,
    CloudUpdateReferences,
    CloudWorkspace,
    CloudWorkspaces,
    CommitGraphSegments,
    CommitGraphV2,
    CommitHashLookup,
    CommitHashToLocation,
    CommitLocationToHash,
    CommitMutations,
    CommitRevlogData,
    CommitTranslateId,
    DownloadFile,
    EphemeralExtend,
    EphemeralPrepare,
    FetchSnapshot,
    Files2,
    GitObjects,
    History,
    LandStack,
    ListBookmarkPatterns,
    Lookup,
    PathHistory,
    ReplayIdenticalMoves,
    SetBookmark,
    StreamingClone,
    SuffixQuery,
    Trees,
    UploadBonsaiChangeset,
    UploadFile,
    UploadHgChangesets,
    UploadHgFilenodes,
    UploadIdenticalChangesets,
    UploadTrees,
}

impl fmt::Display for SaplingRemoteApiMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::AlterSnapshot => "alter_snapshot",
            Self::Blame => "blame",
            Self::Bookmarks2 => "bookmarks2",
            Self::Capabilities => "capabilities",
            Self::CheckManifestPermission => "check_manifest_permission",
            Self::CheckPathPermission => "check_path_permission",
            Self::CloudHistoricalVersions => "cloud_historical_versions",
            Self::CloudOtherRepoWorkspaces => "cloud_other_repo_workspaces",
            Self::CloudReferences => "cloud_references",
            Self::CloudRenameWorkspace => "cloud_rename_workspace",
            Self::CloudRollbackWorkspace => "cloud_rollback_workspace",
            Self::CloudShareWorkspace => "cloud_share_workspace",
            Self::CloudSmartlog => "cloud_smartlog",
            Self::CloudSmartlogByVersion => "cloud_smartlog_by_version",
            Self::CloudUpdateArchive => "cloud_update_archive",
            Self::CloudUpdateReferences => "cloud_update_references",
            Self::CloudWorkspace => "cloud_workspace",
            Self::CloudWorkspaces => "cloud_workspaces",
            Self::CommitGraphSegments => "commit_graph_segments",
            Self::CommitGraphV2 => "commit_graph_v2",
            Self::CommitHashLookup => "commit_hash_lookup",
            Self::CommitHashToLocation => "commit_hash_to_location",
            Self::CommitLocationToHash => "commit_location_to_hash",
            Self::CommitMutations => "commit_mutations",
            Self::CommitRevlogData => "commit_revlog_data",
            Self::CommitTranslateId => "commit_translate_id",
            Self::DownloadFile => "download_file",
            Self::EphemeralExtend => "ephemeral_extend",
            Self::EphemeralPrepare => "ephemeral_prepare",
            Self::FetchSnapshot => "fetch_snapshot",
            Self::Files2 => "files2",
            Self::GitObjects => "git_objects",
            Self::History => "history",
            Self::LandStack => "land_stack",
            Self::ListBookmarkPatterns => "list_bookmark_patterns",
            Self::Lookup => "lookup",
            Self::PathHistory => "path_history",
            Self::ReplayIdenticalMoves => "replay_identical_moves",
            Self::SetBookmark => "set_bookmark",
            Self::StreamingClone => "streaming_clone",
            Self::SuffixQuery => "suffix_query",
            Self::Trees => "trees",
            Self::UploadBonsaiChangeset => "upload_bonsai_changeset",
            Self::UploadFile => "upload_file",
            Self::UploadHgChangesets => "upload_hg_changesets",
            Self::UploadHgFilenodes => "upload_filenodes",
            Self::UploadIdenticalChangesets => "upload_identical_changesets",
            Self::UploadTrees => "upload_trees",
        };
        write!(f, "{name}")
    }
}

/// Information about the handler that served the request.
///
/// This should be inserted into the request's `State` by each handler. It will
/// typically be used by middleware for request logging and stats reporting.
#[derive(Default, StateData, Clone)]
pub struct HandlerInfo {
    pub repo: Option<String>,
    pub method: Option<SaplingRemoteApiMethod>,
}

impl HandlerInfo {
    pub fn new(repo: impl ToString, method: SaplingRemoteApiMethod) -> Self {
        Self {
            repo: Some(repo.to_string()),
            method: Some(method),
        }
    }
}

/// Number of items in this request's batch (files2 `reqs`, trees `keys`).
/// Recorded in request `State` by the handler layer (`handler_wrapper` via
/// the `batch_size` hook, or directly for the legacy trees handler) for
/// per-item latency logging. `OdsMiddleware` reads it; when absent it falls
/// back to the client's count header.
#[derive(Clone, Debug, StateData)]
pub struct RequestBatchSize(pub f64);

/// JSON representation of an error to send to the client.
#[derive(Clone, Serialize, Debug, Deserialize)]
struct JsonError {
    message: String,
    request_id: String,
}

pub struct JsonErrorFormatter;

impl ErrorFormatter for JsonErrorFormatter {
    type Body = Vec<u8>;

    fn format(&self, error: &Error, state: &State) -> Result<(Self::Body, Mime), Error> {
        let message = format!("{error:#}");

        // Package the error message into a JSON response.
        let res = JsonError {
            message,
            request_id: state.short_request_id().to_string(),
        };

        let body = serde_json::to_vec(&res).context("Failed to serialize error")?;

        Ok((body, mime::APPLICATION_JSON))
    }
}

/// Macro to create a Gotham handler function from an async function.
///
/// The expected signature of the input function is:
/// ```rust,ignore
/// async fn handler(state: &mut State) -> Result<impl TryIntoResponse, HttpError>
/// ```
///
/// The resulting wrapped function will have the signature:
/// ```rust,ignore
/// fn wrapped(mut state: State) -> Pin<Box<HandlerFuture>>
/// ```
macro_rules! define_handler {
    ($name:ident, $func:path, [$($flavour:ident),*]) => {
        fn $name(mut state: State) -> Pin<Box<HandlerFuture>> {
            let request_id = state.short_request_id();
            let span = tracing::info_span!("slapi handler", method = %stringify!($name), %request_id, client_correlator = tracing::field::Empty);
            let rctx = RequestContext::borrow_from(&state);
            if let Some(client_correlator) = rctx.ctx.client_correlator() {
                span.record("client_correlator", client_correlator);
            }
            async move {

                let slapi_flavour = SlapiCommitIdentityScheme::borrow_from(&state).clone();
                let supported_flavours = [$(SlapiCommitIdentityScheme::$flavour),*];
                let res = if !supported_flavours
                    .iter()
                    .any(|x| *x == slapi_flavour)
                {
                    Err(HttpError::e400(anyhow!(
                        "Unsupported SaplingRemoteApi flavour"
                    )))
                } else {
                    let (future_stats, res) = $func(&mut state).timed().await;
                    ScubaMiddlewareState::try_set_future_stats(&mut state, &future_stats);
                    res
                };
                build_response(res, state, &JsonErrorFormatter)

            }
            .instrument(span)
            .boxed()
        }
    };
}

define_handler!(
    capabilities_handler,
    capabilities::capabilities_handler,
    [Hg, Git]
);
define_handler!(commit_revlog_data_handler, commit::revlog_data, [Hg]);
define_handler!(repos_handler, repos::repos, [Hg]);
define_handler!(trees_handler, trees::trees, [Hg]);
define_handler!(upload_file_handler, files::upload_file, [Hg]);

static HIGH_LOAD_SIGNAL: &str = "I_AM_OVERLOADED";
static ALIVE: &str = "I_AM_ALIVE";
static EXITING: &str = "EXITING";

// Used for monitoring VIP health
fn proxygen_health_handler(state: State) -> (State, &'static str) {
    if ServerContext::<Repo>::borrow_from(&state).will_exit() {
        (state, EXITING)
    } else {
        if let Some(request_load) = RequestLoad::try_borrow_from(&state) {
            let threshold =
                justknobs::get_as::<i64>("scm/mononoke:edenapi_high_load_threshold", None);
            if threshold > 0 && request_load.0 > threshold {
                return (state, HIGH_LOAD_SIGNAL);
            }
        }
        (state, ALIVE)
    }
}

// Used for monitoring TW tasks
fn health_handler(state: State) -> (State, &'static str) {
    if ServerContext::<Repo>::borrow_from(&state).will_exit() {
        (state, EXITING)
    } else {
        (state, ALIVE)
    }
}

async fn handler_wrapper<Handler: SaplingRemoteApiHandler>(
    mut state: State,
) -> Result<(State, Response<Body>), (State, GothamHandlerError)>
where
    <Handler as SaplingRemoteApiHandler>::Request: std::fmt::Debug,
{
    let request_id = state.short_request_id();
    let span = tracing::info_span!("slapi handler", method = %Handler::API_METHOD, %request_id, client_correlator = tracing::field::Empty);
    let rctx = RequestContext::borrow_from(&state);
    if let Some(client_correlator) = rctx.ctx.client_correlator() {
        span.record("client_correlator", client_correlator);
    }

    let (future_stats, res) = async {
        let path = Handler::PathExtractor::take_from(&mut state);
        let query = Handler::QueryStringExtractor::take_from(&mut state);
        let content_encoding = ContentEncoding::from_state(&state);

        let slapi_flavour = SlapiCommitIdentityScheme::borrow_from(&state).clone();
        if !Handler::SUPPORTED_FLAVOURS.contains(&slapi_flavour) {
            return Err(gotham_ext::error::HttpError::e400(anyhow!(
                "Unsupported SaplingRemoteApi flavour"
            )));
        }
        state.put(HandlerInfo::new(path.repo(), Handler::API_METHOD));

        let rctx = RequestContext::borrow_from(&state).clone();
        let sctx = ServerContext::borrow_from(&state).clone();

        let repo = get_repo(&sctx, &rctx, path.repo(), None).await?;

        // Per-repo config provenance; mutation id is always None until plumbed.
        let source = repo.repo_ctx().config_source();
        ScubaMiddlewareState::try_borrow_add_repo_config_provenance(
            &mut state,
            source.version.as_deref(),
            source.mutation_id,
        );

        let request = parse_wire_request::<<Handler::Request as ToWire>::Wire>(&mut state).await?;

        let sampling_rate = Handler::sampling_rate(&request);
        if sampling_rate.get() > 1 {
            ScubaMiddlewareState::try_set_sampling_rate(&mut state, sampling_rate);
        }

        if let Some(batch_size) = Handler::batch_size(&request) {
            state.put(RequestBatchSize(batch_size as f64));
        }

        if let Some(rd) = RequestDumper::try_borrow_mut_from(&mut state) {
            rd.add_request(&request);
        }

        let ectx = SaplingRemoteApiContext::new(rctx, sctx, repo, path, query, slapi_flavour);

        let (in_band_error_sender, in_band_error_receiver) =
            tokio::sync::mpsc::unbounded_channel::<Error>();

        state.put(PendingInBandErrors {
            receiver: in_band_error_receiver,
        });

        match Handler::handler(ectx, request).await {
            Ok(responses) => {
                let stream = monitor_request(&state, responses);
                let stream = stream.inspect_ok(move |response| {
                    if let Some(in_band_error) = Handler::extract_in_band_error(response) {
                        if let Err(err) = in_band_error_sender.send(in_band_error) {
                            tracing::warn!("Failed to send in-band error: {}", err);
                        }
                    }
                });
                Ok(encode_response_stream(stream, content_encoding))
            }
            Err(err) => Err(err.into()),
        }
    }
    .timed()
    .instrument(span.clone())
    .await;
    ScubaMiddlewareState::try_set_future_stats(&mut state, &future_stats);

    span.in_scope(move || build_response(res, state, &JsonErrorFormatter))
}

pub fn monitor_request<S, T>(
    state: &State,
    stream: S,
) -> impl Stream<Item = T> + Send + 'static + use<S, T>
where
    S: Stream<Item = T> + Send + 'static,
{
    let start = Instant::now();
    let ctx = RequestContext::borrow_from(state).ctx.clone();
    let (sender, receiver) = oneshot::channel::<()>();

    // SaplingRemoteApi doesn't fill these in until the end of the request, so we need
    // to add them now.   A future improvement is to put these on the scuba
    // sample builder earlier on so we can clone it.
    let mut base_scuba = ctx.scuba().clone();
    base_scuba.add(
        HttpScubaKey::RequestId,
        state.short_request_id().to_string(),
    );

    if let Some(info) = state.try_borrow::<HandlerInfo>() {
        base_scuba.add_opt(SaplingRemoteApiScubaKey::Repo, info.repo.clone());
        base_scuba.add_opt(
            SaplingRemoteApiScubaKey::Method,
            info.method.map(|m| m.to_string()),
        );
    }

    if let Some(metadata_state) = MetadataState::try_borrow_from(state) {
        let metadata = metadata_state.metadata();
        if let Some(ref address) = metadata.client_ip() {
            base_scuba.add(HttpScubaKey::ClientIp, address.to_string());
        }

        let identities = metadata.identities();
        let identities_typed: Vec<_> = identities.iter().map(|i| i.to_typed_string()).collect();
        let identities: Vec<_> = identities.iter().map(|i| i.to_string()).collect();
        base_scuba.add(HttpScubaKey::ClientIdentities, identities);
        base_scuba.add(HttpScubaKey::ClientIdentitiesTyped, identities_typed);
    }

    let reporting_loop = async move {
        loop {
            tokio::time::sleep(Duration::from_secs(REPORTING_LOOP_WAIT)).await;
            let mut scuba = base_scuba.clone();
            ctx.perf_counters().insert_perf_counters(&mut scuba);
            scuba.log_with_msg(
                "Long running SaplingRemoteAPI request",
                format!("{} μs", start.elapsed().as_micros_unchecked()),
            );
        }
    };

    mononoke::spawn_task(async move {
        futures::pin_mut!(reporting_loop);
        let _ = futures::future::select(reporting_loop, receiver).await;
    });

    Monitor::new(stream, sender)
}

/// Encode a stream of SaplingRemoteAPI responses into its final on-wire representation.
///
/// This involves converting each item to its wire format, CBOR serializing them, and then
/// optionally compressing the resulting byte stream based on the specified Content-Encoding.
pub fn encode_response_stream<S, T>(stream: S, encoding: ContentEncoding) -> impl TryIntoResponse
where
    S: Stream<Item = Result<T, Error>> + Send + 'static,
    T: ToWire + Send + 'static,
{
    let stream = stream.and_then(|item| async move { to_cbor_bytes(&item.to_wire()) });
    let stream = stream.capture_first_err().encode(encoding);
    StreamBody::new(stream, cbor_mime())
}

// We use a struct here (rather than just a global function) just for the convenience
// of writing `Handlers::setup::<MyHandler>(route)`
// instead of `setup_handler::<MyHandler, _, _>(route)`, to make things clearer.
struct Handlers<C, P> {
    _phantom: (std::marker::PhantomData<C>, std::marker::PhantomData<P>),
}

impl<C, P> Handlers<C, P>
where
    C: gotham::pipeline::PipelineHandleChain<P> + Copy + Send + Sync + 'static,
    P: std::panic::RefUnwindSafe + Send + Sync + 'static,
{
    fn setup<Handler: SaplingRemoteApiHandler>(route: &mut RouterBuilder<C, P>)
    where
        <Handler as SaplingRemoteApiHandler>::Request: std::fmt::Debug,
    {
        route
            .request(
                vec![Handler::HTTP_METHOD],
                &format!("/:repo{}", Handler::ENDPOINT),
            )
            .with_path_extractor::<Handler::PathExtractor>()
            .with_query_string_extractor::<Handler::QueryStringExtractor>()
            .to_async(handler_wrapper::<Handler>);
    }
}

pub fn build_router<R: Send + Sync + Clone + 'static>(ctx: ServerContext<R>) -> Router {
    let pipeline = new_pipeline().add(StateMiddleware::new(ctx)).build();
    let (chain, pipelines) = single_pipeline(pipeline);

    gotham_build_router(chain, pipelines, |route| {
        route.get("/health_check").to(health_handler);
        route.get("/repos").to(repos_handler);
        route
            .get("/proxygen/health_check")
            .to(proxygen_health_handler);
        Handlers::setup::<blame::BlameHandler>(route);
        Handlers::setup::<bookmarks::Bookmarks2Handler>(route);
        Handlers::setup::<bookmarks::SetBookmarkHandler>(route);
        Handlers::setup::<bookmarks::ReplayIdenticalMovesHandler>(route);
        Handlers::setup::<commit_cloud::CommitCloudHistoricalVersions>(route);
        Handlers::setup::<commit_cloud::CommitCloudOtherRepoWorkspaces>(route);
        Handlers::setup::<commit_cloud::CommitCloudReferences>(route);
        Handlers::setup::<commit_cloud::CommitCloudRenameWorkspace>(route);
        Handlers::setup::<commit_cloud::CommitCloudRollbackWorkspace>(route);
        Handlers::setup::<commit_cloud::CommitCloudShareWorkspace>(route);
        Handlers::setup::<commit_cloud::CommitCloudSmartlog>(route);
        Handlers::setup::<commit_cloud::CommitCloudSmartlogByVersion>(route);
        Handlers::setup::<commit_cloud::CommitCloudUpdateArchive>(route);
        Handlers::setup::<commit_cloud::CommitCloudUpdateReferences>(route);
        Handlers::setup::<commit_cloud::CommitCloudWorkspace>(route);
        Handlers::setup::<commit_cloud::CommitCloudWorkspaces>(route);
        Handlers::setup::<commit::AlterSnapshotHandler>(route);
        Handlers::setup::<commit::CommitMutationsHandler>(route);
        Handlers::setup::<commit::CommitTranslateId>(route);
        Handlers::setup::<commit::EphemeralExtendHandler>(route);
        Handlers::setup::<commit::EphemeralPrepareHandler>(route);
        Handlers::setup::<commit::FetchSnapshotHandler>(route);
        Handlers::setup::<commit::GraphHandlerV2>(route);
        Handlers::setup::<commit::GraphSegmentsHandler>(route);
        Handlers::setup::<commit::HashLookupHandler>(route);
        Handlers::setup::<commit::HashToLocationHandler>(route);
        Handlers::setup::<commit::LocationToHashHandler>(route);
        Handlers::setup::<commit::UploadBonsaiChangesetHandler>(route);
        Handlers::setup::<commit::UploadHgChangesetsHandler>(route);
        Handlers::setup::<commit::UploadIdenticalChangesetsHandler>(route);
        Handlers::setup::<files::DownloadFileHandler>(route);
        Handlers::setup::<files::Files2Handler>(route);
        Handlers::setup::<files::UploadHgFilenodesHandler>(route);
        Handlers::setup::<git_objects::GitObjectsHandler>(route);
        Handlers::setup::<history::HistoryHandler>(route);
        Handlers::setup::<land::LandStackHandler>(route);
        Handlers::setup::<legacy::StreamingCloneHandler>(route);
        Handlers::setup::<legacy::ListBookmarkPatternsHandler>(route);
        Handlers::setup::<lookup::LookupHandler>(route);
        Handlers::setup::<path_history::PathHistoryHandler>(route);
        Handlers::setup::<suffix_query::SuffixQueryHandler>(route);
        Handlers::setup::<trees::CheckManifestPermissionHandler>(route);
        Handlers::setup::<trees::CheckPathPermissionHandler>(route);
        Handlers::setup::<trees::UploadTreesHandler>(route);
        route.get("/:repo/health_check").to(health_handler);
        route
            .get("/:repo/capabilities")
            .with_path_extractor::<capabilities::CapabilitiesParams>()
            .to(capabilities_handler);
        route
            .post("/:repo/trees")
            .with_path_extractor::<trees::TreeParams>()
            .to(trees_handler);
        route
            .post("/:repo/commit/revlog_data")
            .with_path_extractor::<commit::RevlogDataParams>()
            .to(commit_revlog_data_handler);
        route
            .put("/:repo/upload/file/:idtype/:id")
            .with_path_extractor::<files::UploadFileParams>()
            .with_query_string_extractor::<files::UploadFileQueryString>()
            .to(upload_file_handler);
    })
}

#[cfg(test)]
mod root_trace_selector_tests {
    use super::*;

    #[mononoke::test]
    fn labels_land_endpoints() {
        // The leading segment is the repo name, so matching is on the suffix.
        assert_eq!(
            root_trace_endpoint_for(&Method::POST, "/fbsource/land"),
            "land_stack",
        );
        assert_eq!(
            root_trace_endpoint_for(&Method::POST, "/fbsource/bookmarks/set"),
            "set_bookmark",
        );
    }

    #[mononoke::test]
    fn labels_reads_as_other() {
        // These are the high-QPS endpoints. They are eligible to root, but the
        // policy rates `other` far below the land labels. They are POST too, so
        // method alone would not tell them apart from a land.
        for path in ["/fbsource/trees", "/fbsource/files2", "/fbsource/history"] {
            assert_eq!(
                root_trace_endpoint_for(&Method::POST, path),
                OTHER_ENDPOINT,
                "read endpoint {path} should fall to the catch-all label",
            );
        }
    }

    #[mononoke::test]
    fn a_land_path_on_the_wrong_method_is_not_a_land() {
        assert_eq!(
            root_trace_endpoint_for(&Method::GET, "/fbsource/land"),
            OTHER_ENDPOINT,
        );
    }

    #[mononoke::test]
    fn does_not_match_on_a_bare_substring() {
        // The endpoint constants carry a leading slash, so a repo or path that
        // merely ends in the same letters must not be labelled a land.
        assert_eq!(
            root_trace_endpoint_for(&Method::POST, "/myland"),
            OTHER_ENDPOINT,
        );
        assert_eq!(
            root_trace_endpoint_for(&Method::POST, "/fbsource/bookmarks2"),
            OTHER_ENDPOINT,
        );
    }

    #[mononoke::test]
    fn labels_match_the_api_method_names() {
        // The labels are bound to the `mononoke_endpoint` Luna sample key and
        // prefix-matched by the mononoke_scm policy. Keeping them equal to
        // SaplingRemoteApiMethod's Display output is what lets the policy, the
        // ODS stats and the traces talk about endpoints the same way.
        assert_eq!(
            root_trace_endpoint_for(&Method::POST, "/fbsource/land"),
            SaplingRemoteApiMethod::LandStack.to_string(),
        );
        assert_eq!(
            root_trace_endpoint_for(&Method::POST, "/fbsource/bookmarks/set"),
            SaplingRemoteApiMethod::SetBookmark.to_string(),
        );
    }
}
