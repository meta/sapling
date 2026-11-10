/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;
use std::sync::RwLock;
use std::time::Instant;

use anyhow::Result;
use context::CoreContext;
use gotham::state::FromState;
use gotham::state::State;
use gotham_derive::StateData;
use gotham_ext::middleware::request_context::RequestContext;
use gotham_ext::middleware::scuba::ScubaMiddlewareState;
use metaconfig_parser::RepoConfigs;
use metaconfig_types::RepoConfigRef;
use metaconfig_types::RepoConfigSourceRef;
use mononoke_api::Mononoke;
use mononoke_app::args::TLSArgs;
use mononoke_repos::MononokeRepos;
use permission_checker::AclProvider;
use repo_authorization::AuthorizationContext;
use repo_identity::RepoIdentityRef;
use repo_permission_checker::RepoPermissionCheckerRef;
use stats::prelude::*;

use super::GitMethodInfo;
use super::Pushvars;
use super::method::GitMethod;
use crate::GitRepos;
use crate::Repo;
use crate::UpstreamLfsUrlFormat;
use crate::errors::GitServerContextErrorKind;
use crate::scuba::MononokeGitScubaKey;

define_stats! {
    prefix = "mononoke.git.server";
    repo_added_on_demand: timeseries(Rate, Sum),
    repo_added_on_demand_failed: timeseries(Rate, Sum),
}

#[derive(Clone)]
pub struct RepositoryRequestContext {
    pub ctx: CoreContext,
    pub repo: Arc<Repo>,
    pub mononoke_repos: Arc<MononokeRepos<Repo>>,
    pub repo_configs: Arc<RepoConfigs>,
    pub pushvars: Pushvars,
}

impl RepositoryRequestContext {
    pub async fn instantiate(
        state: &mut State,
        method_info: GitMethodInfo,
    ) -> Result<Self, GitServerContextErrorKind> {
        state.put(method_info.clone());
        let pushvars = state.take::<Pushvars>();
        let req_ctx = state.borrow_mut::<RequestContext>();
        let ctx = req_ctx.ctx.clone();
        let git_ctx = GitServerContext::borrow_from(state);
        let request_context = git_ctx.request_context(ctx, method_info, pushvars).await?;

        // Per-repo config provenance; mutation id is always None until plumbed.
        let source = request_context.repo.repo_config_source();
        ScubaMiddlewareState::try_borrow_add_repo_config_provenance(
            state,
            source.version.as_deref(),
            source.mutation_id,
        );

        Ok(request_context)
    }

    pub fn bundle_uri_trusted_only(&self) -> bool {
        self.repo
            .repo_config
            .as_ref()
            .git_configs
            .git_bundle_uri
            .as_ref()
            .is_none_or(|buri| buri.trusted_only)
    }
}

#[derive(Clone)]
pub struct GitServerContextInner {
    repos: GitRepos,
    enforce_auth: bool,
    // Upstream LFS server to fetch missing LFS objects from. Used only when
    // `internal_lfs` is false.
    upstream_lfs_server: Option<String>,
    // URL pattern used to construct per-object fetch URLs against the upstream LFS server
    upstream_lfs_url_format: UpstreamLfsUrlFormat,
    // Whether to resolve LFS pointers from the local Mononoke filestore (by
    // SHA256 alias) instead of fetching them over HTTP from the upstream LFS
    // server. The caller is responsible for the precedence: when set true,
    // the upstream URL is ignored. The CLI defaults this to `true` whenever
    // `--upstream-lfs-server` is not provided, so a git_server with no LFS
    // flags at all behaves as if `--internal-lfs` was passed.
    internal_lfs: bool,
    // Used for communicating with upstream LFS server
    tls_args: Option<TLSArgs>,
    // ACL provider for checking group membership
    acl_provider: Arc<dyn AclProvider>,
    // Optional address (host:port) for the Multi-Repo Land Service
    multi_repo_land_service_address: Option<String>,
    // See `GitimportPreferences::persist_partial_mappings`.
    persist_partial_mappings: bool,
}

impl GitServerContextInner {
    pub fn new(
        repos: GitRepos,
        enforce_auth: bool,
        upstream_lfs_server: Option<String>,
        upstream_lfs_url_format: UpstreamLfsUrlFormat,
        internal_lfs: bool,
        tls_args: Option<TLSArgs>,
        acl_provider: Arc<dyn AclProvider>,
        multi_repo_land_service_address: Option<String>,
        persist_partial_mappings: bool,
    ) -> Self {
        Self {
            repos,
            enforce_auth,
            upstream_lfs_server,
            upstream_lfs_url_format,
            internal_lfs,
            tls_args,
            acl_provider,
            multi_repo_land_service_address,
            persist_partial_mappings,
        }
    }
}

#[derive(Clone, StateData)]
pub struct GitServerContext {
    inner: Arc<RwLock<GitServerContextInner>>,
    fb: fbinit::FacebookInit,
}

impl GitServerContext {
    pub fn new(
        fb: fbinit::FacebookInit,
        repos: GitRepos,
        enforce_auth: bool,
        upstream_lfs_server: Option<String>,
        upstream_lfs_url_format: UpstreamLfsUrlFormat,
        internal_lfs: bool,
        tls_args: Option<TLSArgs>,
        acl_provider: Arc<dyn AclProvider>,
        multi_repo_land_service_address: Option<String>,
        persist_partial_mappings: bool,
    ) -> Self {
        let inner = Arc::new(RwLock::new(GitServerContextInner::new(
            repos,
            enforce_auth,
            upstream_lfs_server,
            upstream_lfs_url_format,
            internal_lfs,
            tls_args,
            acl_provider,
            multi_repo_land_service_address,
            persist_partial_mappings,
        )));
        Self { inner, fb }
    }

    pub fn persist_partial_mappings(&self) -> bool {
        self.inner
            .read()
            .expect("poisoned lock in git server context")
            .persist_partial_mappings
    }

    pub fn fb(&self) -> fbinit::FacebookInit {
        self.fb
    }

    pub fn acl_provider(&self) -> Arc<dyn AclProvider> {
        self.inner
            .read()
            .expect("poisoned lock in git server context")
            .acl_provider
            .clone()
    }

    pub fn enforce_authorization(&self) -> bool {
        self.inner
            .read()
            .expect("poisoned lock in git server context")
            .enforce_auth
    }

    fn repos(&self) -> GitRepos {
        self.inner
            .read()
            .expect("poisoned lock in git server context")
            .repos
            .clone()
    }

    pub async fn request_context(
        &self,
        ctx: CoreContext,
        method_info: GitMethodInfo,
        pushvars: Pushvars,
    ) -> Result<RepositoryRequestContext, GitServerContextErrorKind> {
        // First, try the repos assigned to this task, building one that was
        // assigned lazily. A failed build is reported as is rather than
        // falling through to the on-demand path below, which would build it
        // again.
        let assigned_repo = self.repos().get(&method_info.repo).await.map_err(|e| {
            GitServerContextErrorKind::RepoSetupError {
                repo_name: method_info.repo.to_string(),
                error: e.to_string(),
            }
        })?;

        let initial_lookup = {
            let inner = self
                .inner
                .read()
                .expect("poisoned lock in git server context");
            match assigned_repo {
                Some(repo) => Ok((
                    repo,
                    inner.repos.repo_mgr.repos().clone(),
                    inner.enforce_auth,
                    inner.repos.repo_configs(),
                )),
                None => {
                    // Check if the repo exists in the global configuration.
                    // Route through `get_or_load_repo_config` so split-loaded
                    // repos (only present in the per-tier RepoSpec manifest)
                    // are recognized — otherwise we'd return `Err(None)`
                    // ("repo does not exist") instead of falling through to
                    // the on-demand load path.
                    let repo_exists_in_config = inner
                        .repos
                        .repo_mgr
                        .configs()
                        .get_or_load_repo_config(&method_info.repo)
                        .is_ok();
                    if repo_exists_in_config {
                        // Repo exists but is not loaded, we'll need to add it
                        Err(Some(inner.repos.repo_mgr.clone()))
                    } else {
                        // Repo does not exist at all
                        Err(None)
                    }
                }
            }
        };

        let (repo, mononoke_repos, enforce_authorization, repo_configs) = match initial_lookup {
            Ok(result) => result,
            Err(Some(repo_mgr)) => {
                // Repo exists in config but not loaded
                // DO NOT BUILD ON THIS. It is a request-time rescue for eager
                // repos that miss, with no single-flight: ten concurrent first
                // requests build the repo ten times. Lazy repos are built through
                // `RepoSlot` by `MononokeRepos::get` above. Do not extend it.
                // Check if dynamic repo loading is enabled via JustKnobs
                let dynamic_loading_enabled =
                    justknobs::eval("scm/mononoke:git_server_dynamic_repo_loading", None, None);

                if !dynamic_loading_enabled {
                    // Dynamic loading is disabled, return the old behavior
                    return Err(GitServerContextErrorKind::RepositoryNotLoaded(
                        method_info.repo.to_string(),
                    ));
                }

                // Add it dynamically - this is done outside the lock since add_repo is async
                let repo_name = method_info.repo.clone();
                let mut scuba = ctx.scuba().clone();
                scuba
                    .add(MononokeGitScubaKey::Repo, repo_name.as_str())
                    .add(MononokeGitScubaKey::Method, method_info.method.to_string())
                    .unsampled();
                let start = Instant::now();
                let result = repo_mgr.add_repo(&repo_name).await;
                scuba.add("duration_ms", start.elapsed().as_millis() as u64);
                let repo = match result {
                    Ok(repo) => {
                        STATS::repo_added_on_demand.add_value(1);
                        scuba.log_with_msg("Repo loaded on demand", None);
                        repo
                    }
                    Err(e) => {
                        STATS::repo_added_on_demand_failed.add_value(1);
                        scuba.log_with_msg("Repo load on demand failed", format!("{e:#}"));
                        return Err(GitServerContextErrorKind::RepoSetupError {
                            repo_name,
                            error: e.to_string(),
                        });
                    }
                };
                let inner = self
                    .inner
                    .read()
                    .expect("poisoned lock in git server context");
                (
                    repo,
                    inner.repos.repo_mgr.repos().clone(),
                    inner.enforce_auth,
                    inner.repos.repo_configs(),
                )
            }
            Err(None) => {
                // Repo does not exist at all
                return Err(GitServerContextErrorKind::RepositoryDoesNotExist(
                    method_info.repo.to_string(),
                ));
            }
        };

        acl_check(&ctx, &repo, enforce_authorization, method_info.method).await?;
        Ok(RepositoryRequestContext {
            ctx,
            repo,
            mononoke_repos,
            repo_configs,
            pushvars,
        })
    }

    pub fn upstream_lfs_server(&self) -> Result<Option<String>> {
        let inner = self
            .inner
            .read()
            .expect("poisoned lock in git server context");
        Ok(inner.upstream_lfs_server.clone())
    }

    pub fn upstream_lfs_url_format(&self) -> UpstreamLfsUrlFormat {
        self.inner
            .read()
            .expect("poisoned lock in git server context")
            .upstream_lfs_url_format
    }

    pub fn internal_lfs(&self) -> bool {
        self.inner
            .read()
            .expect("poisoned lock in git server context")
            .internal_lfs
    }

    pub fn tls_args(&self) -> Result<Option<TLSArgs>> {
        let inner = self
            .inner
            .read()
            .expect("poisoned lock in git server context");
        Ok(inner.tls_args.clone())
    }

    pub fn multi_repo_land_service_address(&self) -> Option<String> {
        self.inner
            .read()
            .expect("poisoned lock in git server context")
            .multi_repo_land_service_address
            .clone()
    }

    pub fn repo_as_mononoke_api(&self) -> Result<Mononoke<mononoke_api::Repo>> {
        let inner = self
            .inner
            .read()
            .expect("poisoned lock in git server context");
        inner.repos.repo_mgr.make_mononoke_api()
    }
}

async fn acl_check(
    ctx: &CoreContext,
    repo: &(impl RepoPermissionCheckerRef + RepoIdentityRef + RepoConfigRef),
    enforce_authorization: bool,
    method: GitMethod,
) -> Result<(), GitServerContextErrorKind> {
    let authz = AuthorizationContext::new_non_draft(ctx);
    let (action, acl_check) = if method.is_read_only() {
        ("read", authz.check_full_repo_read(ctx, repo).await)
    } else {
        ("draft", authz.check_full_repo_draft(ctx, repo).await)
    };

    if acl_check.is_denied() && enforce_authorization {
        // Tell the caller which identities were rejected on the ACL.
        let identities = ctx
            .metadata()
            .identities()
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let err = match repo.repo_config().hipster_acl.as_deref() {
            Some(acl) => GitServerContextErrorKind::ForbiddenByAcl {
                identities,
                acl: acl.to_string(),
                action,
            },
            None => GitServerContextErrorKind::ForbiddenNoAcl {
                repo_name: repo.repo_identity().name().to_string(),
            },
        };
        Err(err)
    } else {
        Ok(())
    }
}
