/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use anyhow::Error;
use context::CoreContext;
use context::SessionContainer;
use diff as diff_feature;
use diff_service_if as thrift;
use diff_service_if::MetadataDiffRequest;
use diff_service_if::MetadataDiffResponse;
use diff_service_if_services::DiffService;
use diff_service_if_services::errors::diff_service::CommitCompareExn;
use diff_service_if_services::errors::diff_service::DiffHunksExn;
use diff_service_if_services::errors::diff_service::DiffHunksStreamExn;
use diff_service_if_services::errors::diff_service::DiffUnifiedExn;
use diff_service_if_services::errors::diff_service::DiffUnifiedHeaderlessExn;
use diff_service_if_services::errors::diff_service::DiffUnifiedHeaderlessStreamExn;
use diff_service_if_services::errors::diff_service::DiffUnifiedHeaderlessUnaryExn;
use diff_service_if_services::errors::diff_service::DiffUnifiedStreamExn;
use diff_service_if_services::errors::diff_service::DiffUnifiedUnaryExn;
use diff_service_if_services::errors::diff_service::MetadataDiffExn;
use ephemeral_blobstore::StorageLocation;
use fbinit::FacebookInit;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures_stats::TimedFutureExt;
use mononoke_api::ChangesetSpecifier;
use mononoke_api::Mononoke;
use mononoke_api::RepoContext;
use mononoke_api::RestrictedPathsPolicy;
use mononoke_app::MononokeApp;
use mononoke_thrift_server::RequestContext;
use scuba_ext::MononokeScubaSampleBuilder;
use source_control as source_control_thrift;

use crate::errors::ServerError;
use crate::platform;
use crate::repo::Repo;
use crate::utils::diff_conversion::convert_diff_single_input;
use crate::utils::diff_conversion::input_changeset_specifier;

pub fn internal_error<E: Into<Error>>(e: E) -> thrift::InternalError {
    thrift::InternalError {
        reason: format!("{:?}", e.into()),
    }
}

/// Partial-response verdict for a diff-service method, from the request's
/// omission policy. Same contract as SCS `methods::partial_response_info`:
/// `Some` carrying the exact signal when partial responses are allowed for
/// this request, `None` (field unset) when not. Group counts reconcile with
/// the total by construction. Agrees with the context flag (and hence
/// `nocache`) by construction.
pub(crate) fn partial_response_info(
    policy: &RestrictedPathsPolicy,
) -> Option<source_control_thrift::PartialResponseInfo> {
    let omitted_restricted_paths_count = policy.omitted_count()?;
    let omitted_by_restriction = policy
        .omitted_groups()
        .map(|collected| {
            collected
                .into_iter()
                .map(|(restriction, omitted_count)| {
                    source_control_thrift::OmittedRestrictionGroup {
                        restriction_root: restriction.restriction_root.map(|root| root.to_string()),
                        repo_region_acl: restriction.repo_region_acl.to_string(),
                        permission_request_group: restriction
                            .permission_request_group
                            .id_data()
                            .to_string(),
                        omitted_count: omitted_count as i64,
                        ..Default::default()
                    }
                })
                .collect::<Vec<_>>()
        })
        .filter(|groups| !groups.is_empty());
    Some(source_control_thrift::PartialResponseInfo {
        partial: omitted_restricted_paths_count > 0,
        omitted_restricted_paths_count: Some(omitted_restricted_paths_count as i64),
        omitted_by_restriction,
        ..Default::default()
    })
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
                    let ctx = self.create_ctx(stringify!($method_name), req_ctxt, &$( $param_name ),* ).await.map_err(|e| <$err>::from(internal_error(e)))?;
                    ctx.scuba().clone().log_with_msg("Request start", None);
                    let mut scuba = ctx.scuba().clone();
                    let perf_counters = ctx.perf_counters().clone();
                    let (stats, res) = DiffServiceImpl::$method_name(self, ctx.clone(), $( $param_name, )*).timed().await;

                    match res.as_ref() {
                        Err(err) => {
                            let (status, error) = match err {
                                ServerError::ThriftError(e) => {
                                    ("THRIFT_ERROR", format!("{:?}", e))
                                }
                                ServerError::Overloaded(message) => {
                                    ("OVERLOADED", message.clone())
                                }
                                ServerError::RepoNotLoaded(message) => {
                                    ("REPO_NOT_LOADED", message.clone())
                                }
                                ServerError::RequestError(e) => {
                                    ("REQUEST_ERROR", format!("{:?}", e))
                                }
                                ServerError::InternalError(e) => {
                                    ("INTERNAL_ERROR", format!("{:?}", e))
                                }
                            };
                            scuba.unsampled();
                            scuba.add("status", status);
                            scuba.add("error", error);
                        }
                        Ok(_) => {
                            scuba.add("error", "");
                            scuba.add("status", "SUCCESS");
                        }
                    };
                    perf_counters.insert_perf_counters(&mut scuba);
                    scuba.add_future_stats(&stats);
                    scuba.add("partial_response", ctx.partial_response());

                    scuba.log_with_msg("Request complete", None);

                    let res  = res.map_err(|e| match e {
                        ServerError::ThriftError(e) => {
                            <$err>::from(thrift::RequestError { reason: e })
                        },
                        ServerError::Overloaded(message) => {
                            <$err>::from(thrift::OverloadedError { message })
                        },
                        ServerError::RepoNotLoaded(message) => {
                            <$err>::from(thrift::RepoNotLoadedError { message })
                        },
                        ServerError::RequestError(e) => {
                            <$err>::from(thrift::RequestError { reason: thrift::RequestErrorReason::diff_error(thrift::DiffError { reason: e.to_string() }) })
                        },
                        ServerError::InternalError(e) => {
                            <$err>::from(internal_error(e))
                        },
                    });

                    // A partial response must not be served from cache to an entitled
                    // caller (who would silently receive incomplete data); the exact
                    // verdict travels in the typed response instead.
                    if res.is_ok() && ctx.partial_response() {
                        platform::set_nocache_header(req_ctxt);
                    }

                    res
                };
                Box::pin(handler)
            }
        )*
    }
}

#[derive(Clone)]
pub struct DiffServiceImpl {
    fb: FacebookInit,
    scuba: MononokeScubaSampleBuilder,
    pub(crate) mononoke: Arc<Mononoke<Repo>>,
    platform: platform::ServiceState,
    pub(crate) inflight_bytes: Arc<AtomicU64>,
}

impl DiffServiceImpl {
    pub async fn new(
        app: &MononokeApp,
        mononoke: Arc<Mononoke<Repo>>,
        platform: platform::ServiceState,
    ) -> Result<Self, Error> {
        let scuba = app.environment().scuba_sample_builder.clone();
        let fb = app.fb;
        Ok(Self {
            fb,
            scuba,
            mononoke,
            platform,
            inflight_bytes: Arc::new(AtomicU64::new(0)),
        })
    }

    pub async fn get_repo_context(
        &self,
        ctx: CoreContext,
        repo_name: &str,
        specifier: Option<ChangesetSpecifier>,
    ) -> Result<RepoContext<Repo>, ServerError> {
        let authz = platform::authorization_context(&ctx);

        let mut builder = self
            .mononoke
            .repo(ctx.clone(), repo_name)
            .await?
            .ok_or_else(|| classify_missing_repo_error(&self.mononoke, repo_name))?
            .with_authorization_context(authz.clone());

        if let Some(ChangesetSpecifier::Bonsai(cs_id)) = specifier {
            // TODO: Remove this check and legacy bubble lookup after all snapshot
            // callers use ephemeral_bonsai, including older SCS deployments.
            let repo = builder.build().await?;
            if repo.config().storage_config.ephemeral_blobstore.is_none()
                || repo
                    .changeset_exists(cs_id, StorageLocation::Persistent)
                    .await?
            {
                return Ok(repo);
            }

            builder = self
                .mononoke
                .repo(ctx.clone(), repo_name)
                .await?
                .ok_or_else(|| classify_missing_repo_error(&self.mononoke, repo_name))?
                .with_authorization_context(authz)
                .with_bubble(|ephemeral_store| {
                    let ctx = ctx.clone();
                    async move { ephemeral_store.bubble_from_changeset(&ctx, &cs_id).await }
                })
                .await?;
        } else if let Some(specifier) = specifier {
            builder = builder
                .with_bubble(|ephemeral_store| {
                    let ctx = ctx.clone();
                    async move { specifier.bubble_id(&ctx, ephemeral_store).await }
                })
                .await?;
        }

        let repo = builder.build().await?;
        Ok(repo)
    }

    /// Resolve diff inputs by extracting changeset IDs, creating per-input
    /// repo contexts (for cross-bubble diffs), and converting thrift inputs
    /// to diff feature inputs.
    pub async fn resolve_diff_inputs(
        &self,
        ctx: &CoreContext,
        repo_name: &str,
        input: thrift::DiffInput,
        policy: &RestrictedPathsPolicy,
    ) -> Result<ResolvedDiffInputs, ServerError> {
        let base_specifier = input_changeset_specifier(input.base.as_ref())?;
        let other_specifier = input_changeset_specifier(input.other.as_ref())?;

        let (base_repo, other_repo) = futures::try_join!(
            self.get_repo_context(ctx.clone(), repo_name, base_specifier),
            self.get_repo_context(ctx.clone(), repo_name, other_specifier),
        )?;

        let base = match input.base {
            Some(input) => match convert_diff_single_input(&base_repo, input, policy).await? {
                Some(converted) => ResolvedDiffSide::Resolved(converted),
                None => ResolvedDiffSide::Denied,
            },
            None => ResolvedDiffSide::Absent,
        };
        let other = match input.other {
            Some(input) => match convert_diff_single_input(&other_repo, input, policy).await? {
                Some(converted) => ResolvedDiffSide::Resolved(converted),
                None => ResolvedDiffSide::Denied,
            },
            None => ResolvedDiffSide::Absent,
        };

        Ok(ResolvedDiffInputs {
            base_repo,
            other_repo,
            base,
            other,
        })
    }

    async fn create_ctx(
        &self,
        method: &str,
        req_ctxt: &RequestContext,
        params: &dyn AddScubaParams,
    ) -> Result<CoreContext, ServerError> {
        let session = self.create_session(req_ctxt).await?;
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
    ) -> Result<MononokeScubaSampleBuilder, ServerError> {
        let mut scuba = self.scuba.clone().with_seq("seq");
        scuba.add_common_server_data();
        scuba.add("type", "thrift");
        self.platform.add_scuba_params(req_ctxt, &mut scuba)?;
        scuba.add("method", method);

        params.add_scuba_params(&mut scuba);
        Ok(scuba)
    }

    async fn create_session(
        &self,
        req_ctxt: &RequestContext,
    ) -> Result<SessionContainer, ServerError> {
        let metadata = self.platform.create_metadata(req_ctxt).await?;
        let session = SessionContainer::builder(self.fb)
            .metadata(Arc::new(metadata))
            .build();

        Ok(session)
    }
}

// Define the AddScubaParams trait here so all modules can use it
pub trait AddScubaParams: Send + Sync {
    fn add_scuba_params(&self, _scuba: &mut MononokeScubaSampleBuilder) {}
}

/// One side of a single-pair diff request after input resolution.
pub enum ResolvedDiffSide {
    /// No input was provided for this side.
    Absent,
    /// Input was provided but denied by restricted-path enforcement. The
    /// caller maps the side to absent and sets the partial-response signal.
    Denied,
    /// Resolved input, ready to diff.
    Resolved(diff_feature::types::DiffSingleInput),
}

impl ResolvedDiffSide {
    pub fn input(&self) -> Option<&diff_feature::types::DiffSingleInput> {
        match self {
            Self::Resolved(input) => Some(input),
            _ => None,
        }
    }
}

pub struct ResolvedDiffInputs {
    pub base_repo: RepoContext<Repo>,
    pub other_repo: RepoContext<Repo>,
    pub base: ResolvedDiffSide,
    pub other: ResolvedDiffSide,
}

impl DiffService for DiffServiceImpl {
    type RequestContext = RequestContext;

    impl_thrift_methods! {
        async fn diff_hunks(request: thrift::DiffHunksRequest,) -> Result<(thrift::DiffHunksResponse, BoxStream<'static, Result<thrift::DiffHunksData, DiffHunksStreamExn>>), DiffHunksExn>;
        async fn diff_unified(request: thrift::DiffUnifiedRequest,) -> Result<(thrift::DiffUnifiedResponse, BoxStream<'static, Result<thrift::DiffUnifiedChunk, DiffUnifiedStreamExn>>), DiffUnifiedExn>;
        async fn diff_unified_headerless(request: thrift::DiffUnifiedHeaderlessRequest,) -> Result<(thrift::DiffUnifiedHeaderlessResponse, BoxStream<'static, Result<thrift::DiffUnifiedHeaderlessChunk, DiffUnifiedHeaderlessStreamExn>>), DiffUnifiedHeaderlessExn>;
        async fn diff_unified_unary(request: thrift::DiffUnifiedRequest,) -> Result<thrift::DiffUnifiedUnaryResponse, DiffUnifiedUnaryExn>;
        async fn diff_unified_headerless_unary(request: thrift::DiffUnifiedHeaderlessRequest,) -> Result<thrift::DiffUnifiedHeaderlessUnaryResponse, DiffUnifiedHeaderlessUnaryExn>;
        async fn metadata_diff(request: MetadataDiffRequest,) -> Result<MetadataDiffResponse, MetadataDiffExn>;
        async fn commit_compare(request: thrift::CommitCompareRequest,) -> Result<thrift::CommitCompareResponse, CommitCompareExn>;
    }
}

/// Decide which wire-level error to emit when `Mononoke::repo` returns `None`
/// for `repo_name`. The receiving shard distinguishes two cases:
///
/// * **In tier but not loaded on this shard** (a ShardManager routing race):
///   emit `RepoNotLoadedError` so ServiceRouter can classify it separately.
/// * **Not in the tier configuration at all** (a true 404-class client error):
///   emit a permanent `diff_error`. SCS classifies that as a `RequestError`
///   and surfaces the failure immediately.
fn classify_missing_repo_error<R>(mononoke: &Mononoke<R>, repo_name: &str) -> ServerError {
    if mononoke.repo_names_in_tier.load().contains_key(repo_name) {
        ServerError::RepoNotLoaded(format!("repo not loaded on this server: {repo_name}"))
    } else {
        crate::utils::diff_conversion::diff_error(format!("repo does not exist: {repo_name}"))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::str::FromStr;
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use metaconfig_types::CommitIdentityScheme;
    use mononoke_api::OmissionCollector;
    use mononoke_macros::mononoke;
    use mononoke_repos::MononokeRepos;
    use mononoke_types::NonRootMPath;
    use permission_checker::MononokeIdentity;
    use restricted_paths::DenyingRestriction;

    use super::*;

    fn build_mononoke_with_tier(tier: HashMap<String, CommitIdentityScheme>) -> Mononoke<Repo> {
        Mononoke {
            repos: Arc::new(MononokeRepos::new()),
            repo_names_in_tier: Arc::new(ArcSwap::from_pointee(tier)),
        }
    }

    #[mononoke::test]
    fn test_classify_missing_repo_in_tier_not_loaded() {
        // Given: the repo belongs to the tier but is not loaded on this shard.
        let tier = HashMap::from([("test_repo".to_string(), CommitIdentityScheme::BONSAI)]);
        let mononoke = build_mononoke_with_tier(tier);

        // When: the missing repo is classified.
        let err = classify_missing_repo_error(&mononoke, "test_repo");

        // Then: it remains distinct from both overload and a permanently missing repo.
        assert!(matches!(
            err,
            ServerError::RepoNotLoaded(message)
                if message == "repo not loaded on this server: test_repo"
        ));
    }

    #[mononoke::test]
    fn test_classify_missing_repo_not_in_tier() {
        let mononoke = build_mononoke_with_tier(HashMap::new());

        let err = classify_missing_repo_error(&mononoke, "ghost_repo");

        match err {
            ServerError::ThriftError(thrift::RequestErrorReason::diff_error(d)) => {
                assert_eq!(
                    d.reason, "repo does not exist: ghost_repo",
                    "truly-missing must emit a permanent diff_error",
                );
            }
            other => panic!("expected diff_error, got {other:?}"),
        }
    }

    #[mononoke::test]
    fn test_partial_response_info() -> anyhow::Result<()> {
        // Not allowed: no field.
        assert!(partial_response_info(&RestrictedPathsPolicy::Strict).is_none());
        // Allowed: always populated; `partial` iff count is positive,
        // agreeing with the `nocache` decision (set iff count positive) by
        // construction. A complete response carries no groups.
        let collecting =
            || RestrictedPathsPolicy::SkipAndCollect(Arc::new(OmissionCollector::default()));
        let complete = partial_response_info(&collecting()).expect("populated when allowed");
        assert!(!complete.partial);
        assert_eq!(complete.omitted_restricted_paths_count, Some(0));
        assert_eq!(complete.omitted_by_restriction, None);

        // Denials are grouped by restriction; group counts sum to the total.
        let policy = collecting();
        let acl = MononokeIdentity::from_str("REPO_REGION:tent_acl")?;
        policy.record_denial(&DenyingRestriction {
            restriction_root: Some(NonRootMPath::new("tent")?),
            repo_region_acl: acl.clone(),
            permission_request_group: acl,
        });
        policy.record_denial(&DenyingRestriction {
            restriction_root: Some(NonRootMPath::new("tent")?),
            repo_region_acl: MononokeIdentity::from_str("REPO_REGION:tent_acl")?,
            permission_request_group: MononokeIdentity::from_str("REPO_REGION:tent_acl")?,
        });
        let partial = partial_response_info(&policy).expect("populated when allowed");
        assert!(partial.partial);
        assert_eq!(partial.omitted_restricted_paths_count, Some(2));
        let groups = partial
            .omitted_by_restriction
            .expect("partial response carries groups");
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].restriction_root.as_deref(),
            Some("tent"),
            "group names its restriction root"
        );
        assert_eq!(groups[0].omitted_count, 2);
        Ok(())
    }
}
