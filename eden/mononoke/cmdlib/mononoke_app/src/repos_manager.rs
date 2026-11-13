/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

#[cfg(fbcode_build)]
use MononokeAppStats_ods3::Instrument_MononokeAppStats;
#[cfg(fbcode_build)]
use MononokeAppStats_ods3_types::MononokeAppStats;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use config_reconcile::ConfigSource;
use config_reconcile::DesiredRepo;
use config_reconcile::ManifestEntry;
use config_reconcile::RepoGeneration;
use config_reconcile::RepoManager;
use config_reconcile::RepoState;
use config_reconcile::SharedGensSeen;
use config_reconcile::Splay;
use facet::AsyncBuildable;
use futures::FutureExt;
use futures::future::BoxFuture;
use futures::stream;
use futures::stream::AbortHandle;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use itertools::Itertools;
use metaconfig_parser::RepoConfigs;
use metaconfig_parser::SharedGenerations;
use metaconfig_parser::StorageConfigs;
use metaconfig_parser::parse_repo_spec;
use metaconfig_parser::spec_hash;
use metaconfig_types::CommitIdentityScheme;
use metaconfig_types::Redaction;
use metaconfig_types::RepoConfig;
use metaconfig_types::RepoConfigRef;
use metaconfig_types::RepoConfigSource;
use metaconfig_types::ShardedService;
use mononoke_api::Mononoke;
use mononoke_api::MononokeRepo;
use mononoke_configs::ConfigUpdateReceiver;
use mononoke_configs::MononokeConfigs;
use mononoke_macros::mononoke;
use mononoke_repos::MononokeRepos;
use mononoke_repos::RepoLoader;
use repo_factory::RepoFactory;
use repo_factory::RepoFactoryBuilder;
use repos::RepoSpec;
use repos::TierManifest;
use stats::prelude::*;
use tokio::sync::Notify;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tracing::debug;
use tracing::info;
use tracing::warn;

/// How many repos are built at once, at startup and per reconcile pass. Repo
/// construction is CPU-heavy, so this trades startup time against CPU spikes.
const REPOS_MANAGER_CONCURRENCY: usize = 30;

define_stats! {
    prefix = "mononoke.app";
    completion_duration_secs: timeseries(Average, Sum, Count),
    // Repo load failures (config load or facet build) at assignment, via
    // add_repo or add_repo_for_service.
    add_repo_failed: timeseries(Sum, Count),
    // The same failures for a repo registered unbuilt, when ConfigDefinedRepoLoader
    // builds it on its first request. Counted at most once per build attempt,
    // so a repo that never builds adds one per failed-build retry window.
    lazy_repo_build_failed: timeseries(Sum, Count),
    // Successful repo builds, aggregated over repos so idle repos add no series:
    // on first request via ConfigDefinedRepoLoader, and at assignment via add_repo.
    lazy_repo_build_duration_ms: histogram(250, 0, 120_000, Average, Count; P 50; P 90; P 99),
    add_repo_build_duration_ms: histogram(250, 0, 120_000, Average, Count; P 50; P 90; P 99),
    // A tier member a request asked for after startup, built on that request
    // (see `get_or_load_repo`), or that failed to load for it.
    repo_loaded_on_demand: timeseries(Rate, Sum),
    repo_load_on_demand_failed: timeseries(Rate, Sum),
    reconcile_applied: timeseries(Average, Sum, Count),
    // Repos reconcile would have rebuilt whose new config equals the one serving.
    reconcile_unchanged: timeseries(Average, Sum, Count),
    reconcile_dropped: timeseries(Average, Sum, Count),
    reconcile_failed_repos: timeseries(Average, Sum, Count),
    reconcile_tick_duration_ms: timeseries(Average, Sum, Count),
    // Loaded repos waiting for their splay offset after a shared-config change.
    // Sampled every pass, repeating the last count when a pass fails, so an
    // alert on it staying above zero catches repos that never rebuild.
    reconcile_deferred: timeseries(Average),
    tier_names_len: timeseries(Average),
    tier_names_manifest_len: timeseries(Average),
    // Per-task gauges, sampled on each reconcile tick: repos assigned to this
    // task (built or not) and those of them that are built.
    repos_assigned: timeseries(Average),
    repos_built: timeseries(Average),
}

/// A manager of a MononokeRepos collection.
///
/// This allows repos to be added or removed from the MononokeRepos
/// collection.
///
/// Dropping the manager aborts the background config reconcile loop, and that
/// loop is the only path that rebuilds a served repo when its config changes.
/// A server must therefore keep its manager alive for as long as it serves
/// repos — in particular when it runs without ShardManager (e.g. locally),
/// where no `RepoShardedProcess` is constructed to hold it.
pub struct MononokeReposManager<Repo> {
    repos: Arc<MononokeRepos<Repo>>,
    configs: Arc<MononokeConfigs>,
    repo_factory: Arc<RepoFactory>,
    redaction_disabled: bool,
    // Tier-wide list of enabled repos (name -> default identity scheme).
    // Shared with Mononoke<R> (read by list_repos) and with
    // MononokeConfigUpdateReceiver (which refreshes it on each config update).
    repo_names_in_tier: Arc<ArcSwap<HashMap<String, CommitIdentityScheme>>>,
    // True when `repo_names_in_tier` is a pure function of the tier manifest
    // (manifest mode with the knob on). Then the receiver's manifest refresh is
    // its only writer: `make_mononoke_api` does not seed it from the per-task
    // config cache and per-repo updates do not patch it.
    names_from_manifest: bool,
    // Holds all state a reconcile pass needs (per-repo state, spec-hash cache,
    // single-flight lock). Shared with the background loop.
    reconcile_driver: Arc<ReconcileDriver<Repo>>,
    // Background reconcile loop; aborted on Drop. None without split-loading.
    reconcile_loop_handle: Option<JoinHandle<()>>,
    // The startup `--filter-repos` predicate, so a request-time load never
    // widens the repo set the task was started with.
    repo_filter: Option<RepoFilter>,
}

/// The startup repo filter, as the app environment carries it.
pub type RepoFilter = Arc<dyn Fn(&str) -> bool + Send + Sync + 'static>;

/// Whether `service` should defer building this repo until its first request.
///
/// Anything short of an explicit `true` is eager. Lazy is opt-in per repo
/// rather than a default with exclusions, because a repo slow enough to load
/// is one worth having resident before a request arrives, and that is exactly
/// the population an "everything except" default would catch.
fn lazy_for_service(repo_config: &RepoConfig, service: Option<ShardedService>) -> bool {
    let Some(lazy_loading_config) = repo_config.lazy_loading_config.as_ref() else {
        return false;
    };

    match service {
        Some(service) => lazy_loading_config.sharded.get(&service) == Some(&true),
        None => lazy_loading_config.unsharded,
    }
}

/// Captures no manager on purpose: the collection holding this is itself held
/// by the manager, so a reference back would be a cycle.
///
/// Duplicates `add_repo` rather than sharing one body with it, because
/// `add_repo` also needs the repo id from the resolved config and would have
/// to resolve it a second time on every eager deep-shard load.
///
/// `build_permits` caps concurrent builds from every caller (first requests,
/// Multi-Repo Land): a ceiling against bursts, not a throttle. Only a build
/// holds a permit and no build loads another repo, so it cannot deadlock.
struct ConfigDefinedRepoLoader {
    configs: Arc<MononokeConfigs>,
    repo_factory: Arc<RepoFactory>,
    redaction_disabled: bool,
    build_permits: Arc<Semaphore>,
}

impl<Repo> RepoLoader<Repo> for ConfigDefinedRepoLoader
where
    Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>
        + Send
        + Sync
        + 'static,
{
    fn load(&self, repo_name: String) -> BoxFuture<'static, Result<Repo>> {
        let configs = self.configs.clone();
        let repo_factory = self.repo_factory.clone();
        let redaction_disabled = self.redaction_disabled;
        let build_permits = self.build_permits.clone();
        async move {
            let _permit = build_permits.acquire_owned().await?;
            let start = Instant::now();
            let result: Result<Repo> = async {
                // get_or_load_repo_config_and_source subscribes the per-repo
                // ConfigHandle internally.
                let (mut repo_config, source) =
                    configs.get_or_load_repo_config_and_source(&repo_name)?;
                if redaction_disabled {
                    repo_config.redaction = Redaction::Disabled;
                }
                let common_config = configs.repo_configs().common.clone();
                Ok(repo_factory
                    .build(repo_name.clone(), repo_config, common_config, source)
                    .await?)
            }
            .await;
            let elapsed = start.elapsed();

            let mut scuba = repo_factory.env.scuba_sample_builder.clone();
            scuba
                .add("repo", repo_name.as_str())
                .add("duration_ms", elapsed.as_millis() as i64);
            match &result {
                Ok(_) => {
                    STATS::lazy_repo_build_duration_ms.add_value(elapsed.as_millis() as i64);
                    info!(
                        "Built repo on first request: {} in {:?}",
                        repo_name, elapsed
                    );
                    scuba.log_with_msg("Lazy repo built", None);
                }
                Err(e) => {
                    STATS::lazy_repo_build_failed.add_value(1);
                    scuba.log_with_msg("Lazy repo build failed", format!("{e:#}"));
                }
            }
            result
        }
        .boxed()
    }
}

const LAZY_REPO_BUILD_CONCURRENCY_KNOB: &str = "scm/mononoke:lazy_repo_build_concurrency";

impl<Repo> MononokeReposManager<Repo> {
    // Create a new `MononokeReposManager`.
    // Unlike `new_with_redaction_disabled`, we don't expose the mechanism to access redacted blobs
    // through this API.
    // This should be your goto constructor for this struct except if you have a specific reason
    // for needing to disable redaction.
    #[allow(unused)]
    pub(crate) async fn new<Names>(
        configs: Arc<MononokeConfigs>,
        repo_factory: Arc<RepoFactory>,
        service_name: Option<ShardedService>,
        repo_names: Names,
    ) -> Result<Self>
    where
        Names: IntoIterator<Item = String>,
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>
            + RepoConfigRef
            + Send
            + Sync
            + 'static,
    {
        Self::new_with_redaction_disabled(configs, repo_factory, service_name, repo_names, false)
            .await
    }

    pub(crate) async fn new_with_redaction_disabled<Names>(
        configs: Arc<MononokeConfigs>,
        repo_factory: Arc<RepoFactory>,
        // Retained for the app.rs open_named_managed_repos API; reconcile derives
        // deep-sharding from the manifest, so the receiver no longer needs it.
        _service_name: Option<ShardedService>,
        repo_names: Names,
        redaction_disabled: bool,
    ) -> Result<Self>
    where
        Names: IntoIterator<Item = String>,
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>
            + RepoConfigRef
            + Send
            + Sync
            + 'static,
    {
        // Read once, so a change needs a restart; floored so 0 cannot block all builds.
        let build_concurrency =
            justknobs::get_as::<usize>(LAZY_REPO_BUILD_CONCURRENCY_KNOB, None).max(1);
        let repos = Arc::new(MononokeRepos::new_lazy(Arc::new(ConfigDefinedRepoLoader {
            configs: configs.clone(),
            repo_factory: repo_factory.clone(),
            redaction_disabled,
            build_permits: Arc::new(Semaphore::new(build_concurrency)),
        })));
        let repo_names_in_tier = Arc::new(ArcSwap::from_pointee(HashMap::new()));
        let reconcile_driver = Arc::new(ReconcileDriver {
            configs: configs.clone(),
            repo_factory: repo_factory.clone(),
            repos: repos.clone(),
            redaction_disabled,
            reconcile_state: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            splay_state: Mutex::new(SplayState::default()),
            spec_hash_cache: Arc::new(Mutex::new(HashMap::new())),
            reconcile_lock: Arc::new(tokio::sync::Mutex::new(())),
        });
        // Decided once per task. In manifest mode with the knob on, the tier-wide
        // names map is a pure function of the manifest: seeded here, rebuilt on
        // every manifest change, written by nothing else.
        let manifest_source = match (configs.tier_name(), configs.manifest()) {
            (Some(tier), Some(manifest)) => {
                let on = justknobs::eval(TIER_NAMES_FROM_MANIFEST_KNOB, None, Some(tier));
                debug!("tier_repo_names_from_manifest={on} for tier {tier}");
                if on {
                    repo_names_in_tier.store(Arc::new(repo_names_from_manifest(&manifest)));
                }
                on.then(|| Arc::downgrade(&configs))
            }
            _ => None,
        };
        let names_from_manifest = manifest_source.is_some();
        let mut mgr = MononokeReposManager {
            repos,
            configs,
            repo_factory,
            redaction_disabled,
            repo_names_in_tier: repo_names_in_tier.clone(),
            names_from_manifest,
            reconcile_driver,
            reconcile_loop_handle: None,
            repo_filter: None,
        };
        mgr.populate_repos(repo_names).await?;
        let update_receiver =
            MononokeConfigUpdateReceiver::new(repo_names_in_tier, manifest_source);
        mgr.configs
            .register_for_update(Arc::new(update_receiver) as Arc<dyn ConfigUpdateReceiver>);

        // Split-loaded services drive reconcile from a background loop, woken by
        // a receiver on every config change plus a periodic backstop. The loop
        // is unconditional; the killswitch is checked per-pass.
        if mgr.configs.manifest().is_some() {
            let trigger = Arc::new(Notify::new());
            mgr.configs.register_for_update(Arc::new(ReconcileTrigger {
                notify: trigger.clone(),
            }) as Arc<dyn ConfigUpdateReceiver>);
            mgr.reconcile_loop_handle =
                Some(spawn_reconcile_loop(mgr.reconcile_driver.clone(), trigger));
        }

        Ok(mgr)
    }

    /// The repo collection that is being managed.
    pub fn repos(&self) -> &Arc<MononokeRepos<Repo>> {
        &self.repos
    }

    pub fn configs(&self) -> Arc<MononokeConfigs> {
        self.configs.clone()
    }

    /// Return a repo config for a named repo.  This reads from the main
    /// configuration, so doesn't need to be a currently managed repo.
    pub fn repo_config(&self, repo_name: &str) -> Result<RepoConfig> {
        self.repo_config_and_source(repo_name)
            .map(|(config, _)| config)
    }

    /// [`Self::repo_config`] with where the config came from.
    fn repo_config_and_source(&self, repo_name: &str) -> Result<(RepoConfig, RepoConfigSource)> {
        let (mut repo_config, source) =
            self.configs.get_or_load_repo_config_and_source(repo_name)?;
        if self.redaction_disabled {
            repo_config.redaction = Redaction::Disabled;
        }
        Ok((repo_config, source))
    }

    /// Construct and add a new repo to the managed repo collection.
    pub async fn add_repo(&self, repo_name: &str) -> Result<Arc<Repo>>
    where
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>,
    {
        // get_or_load_repo_config_and_source (called via
        // repo_config_and_source) subscribes the per-repo ConfigHandle
        // internally.
        let (repo_config, source) = self
            .repo_config_and_source(repo_name)
            .inspect_err(|_| STATS::add_repo_failed.add_value(1))?;
        let repo_id = repo_config.repoid.id();
        let common_config = self.configs.repo_configs().common.clone();
        let start = Instant::now();
        let repo = self
            .repo_factory
            .build(repo_name.to_string(), repo_config, common_config, source)
            .await
            .inspect_err(|_| STATS::add_repo_failed.add_value(1))?;
        STATS::add_repo_build_duration_ms.add_value(start.elapsed().as_millis() as i64);
        self.repos.add(repo_name, repo_id, repo);
        self.repos
            .get_by_name(repo_name)
            .ok_or_else(|| anyhow!("Couldn't retrieve added repo {repo_name}"))
    }

    /// Register a repo as assigned to this service without building it.
    ///
    /// The repo id comes from the tier manifest rather than from the resolved
    /// config, so this needs no `RepoFactory` and no second parse.
    ///
    /// Prefer [`Self::add_repo_for_service`], which decides between this and
    /// [`Self::add_repo`] from the repo's config. Reaching for this directly
    /// opts a repo into lazy loading whatever its config says.
    pub fn add_lazy_repo(&self, repo_name: &str) -> Result<()> {
        let manifest = self
            .configs
            .manifest()
            .context("add_lazy_repo: no tier manifest, so split-loading is off")?;
        let entry = manifest
            .repos
            .iter()
            .find(|entry| entry.repo_name == repo_name)
            .with_context(|| {
                format!("add_lazy_repo: repo {repo_name} is not in the tier manifest")
            })?;
        self.repos.add_placeholder(repo_name, entry.repo_id);
        Ok(())
    }

    /// Register a repo that has just been assigned to `service`, building it
    /// now unless its config defers the build to the repo's first request.
    ///
    /// Resolving the config here means the eager path resolves it twice, but
    /// `get_or_load_repo_config_and_source` serves the second from its cache.
    pub async fn add_repo_for_service(
        &self,
        repo_name: &str,
        service: Option<ShardedService>,
    ) -> Result<()>
    where
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>,
    {
        let repo_config = self
            .repo_config(repo_name)
            .inspect_err(|_| STATS::add_repo_failed.add_value(1))?;
        if lazy_for_service(&repo_config, service) {
            return self
                .add_lazy_repo(repo_name)
                .inspect_err(|_| STATS::add_repo_failed.add_value(1));
        }

        self.add_repo(repo_name).await?;
        Ok(())
    }

    /// Remove a repo from the managed repo collection.
    pub fn remove_repo(&self, repo_name: &str) {
        self.repos.remove(repo_name);
        self.configs.remove_repo_config_handle(repo_name);
    }

    pub(crate) fn with_repo_filter(mut self, repo_filter: Option<RepoFilter>) -> Self {
        self.repo_filter = repo_filter;
        self
    }

    /// The repo, built on this request if it joined the tier after startup.
    ///
    /// Request-time counterpart of the ShardManager assignment path, for a
    /// service that opened its whole tier: a name that is in the live tier
    /// manifest but not in the collection is registered as a placeholder and
    /// built through the collection's loader. The slot provides single-flight,
    /// cancellation safety and the failed-build hold; eager startup is
    /// unchanged, and placeholders exist only for names a request asked for.
    ///
    /// `None` when the repo is not served: outside the tier manifest (no
    /// config is read for it), disabled, excluded by the startup repo filter,
    /// or the collection has no loader. A config that does not load, or a
    /// failed build, is this call's error.
    pub async fn get_or_load_repo(&self, repo_name: &str) -> Result<Option<Arc<Repo>>>
    where
        Repo: Send + Sync + 'static,
    {
        get_or_load_repo_in(
            &self.repos,
            &self.configs,
            self.repo_filter.as_ref(),
            repo_name,
        )
        .await
    }

    /// Run one reconciliation pass now. Delegates to the driver. No-op unless the
    /// `use_config_reconcile` killswitch is on (read every call).
    pub async fn reconcile(&self) -> Result<()>
    where
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>
            + RepoConfigRef
            + Send
            + Sync
            + 'static,
    {
        self.reconcile_driver.pass().await
    }

    async fn populate_repos<Names>(&self, repo_names: Names) -> Result<()>
    where
        Names: IntoIterator<Item = String>,
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>
            + Send
            + Sync
            + 'static,
    {
        let repo_configs = repo_names
            .into_iter()
            .unique()
            .map(|repo_name| {
                self.repo_config_and_source(&repo_name)
                    .map(|(repo_config, source)| (repo_name, repo_config, source))
            })
            .collect::<Result<Vec<_>>>()?;
        let total = repo_configs.len();
        let completed = Arc::new(AtomicUsize::new(0));
        let repos_input = stream::iter(repo_configs)
            .map(|(repo_name, repo_config, source)| {
                let repo_factory = self.repo_factory.clone();
                let name = repo_name.clone();
                let common_config = self.configs.repo_configs().common.clone();
                let repo_id = repo_config.repoid.id();
                let completed = completed.clone();
                mononoke::spawn_task(async move {
                    #[cfg(fbcode_build)]
                    let start = Instant::now();
                    info!("Initializing repo: {}", &repo_name);
                    let repo = repo_factory
                        .build(name, repo_config, common_config, source)
                        .await
                        .with_context(|| format!("Failed to initialize repo '{repo_name}'"))?;
                    let n = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    info!("Initialized repo: {} ({}/{})", &repo_name, n, total);
                    #[cfg(fbcode_build)]
                    let instrument = Instrument_MononokeAppStats::new();
                    #[cfg(fbcode_build)]
                    instrument.observe(MononokeAppStats {
                        repo_name: Some(repo_name.to_string()),
                        initialization_time_millisecs: Some(start.elapsed().as_millis() as f64),
                        ..Default::default()
                    });

                    anyhow::Ok((repo_id, repo_name, repo))
                })
            })
            .buffer_unordered(REPOS_MANAGER_CONCURRENCY)
            .map(|r| anyhow::Ok(r??))
            .try_collect::<Vec<_>>()
            .await?;
        self.repos.populate(repos_input);
        Ok(())
    }

    pub fn add_stats_handle_for_repo(&self, repo_name: &str, handle: AbortHandle) {
        self.repos.add_stats_handle_for_repo(repo_name, handle)
    }

    pub fn remove_stats_handle_for_repo(&self, repo_name: &str) {
        self.repos.remove_stats_handle_for_repo(repo_name)
    }
}

/// Memoize a per-name hash of an `Arc<T>`. Returns the cached hash only when the
/// live `Arc` is pointer-identical to the one hashed last time — a re-serialize
/// optimization; `compute` stays the drift signal, `ptr_eq` never is. On a pointer
/// miss, recompute via `compute`; on `Ok` cache `(spec.clone(), hash)` and return
/// it, on `Err` return `None` (matching the caller's `.ok()?`). Stores the `Arc`
/// itself (not a raw pointer) so a reused address can't false-match via `ptr_eq`
/// (ABA).
fn memoized_spec_hash<T>(
    cache: &Mutex<HashMap<String, (Arc<T>, u64)>>,
    name: &str,
    spec: &Arc<T>,
    compute: impl FnOnce(&T) -> Result<u64>,
) -> Option<u64> {
    let mut cache = cache.lock().expect("spec_hash_cache poisoned");
    if let Some((cached_spec, hash)) = cache.get(name) {
        if Arc::ptr_eq(cached_spec, spec) {
            return Some(*hash);
        }
    }
    let hash = compute(spec).ok()?;
    cache.insert(name.to_string(), (spec.clone(), hash));
    Some(hash)
}

/// Drop cache entries whose name is absent from `live`. Called once per pass (there
/// is no end-of-pass hook on `ConfigSource`) to evict repos removed from the
/// manifest.
fn retain_live_cache_entries<T>(
    cache: &Mutex<HashMap<String, (Arc<T>, u64)>>,
    live: &HashSet<&str>,
) {
    let mut cache = cache.lock().expect("spec_hash_cache poisoned");
    cache.retain(|name, _| live.contains(name.as_str()));
}

/// Body of [`MononokeReposManager::get_or_load_repo`], over the pieces it
/// needs, so the decision is testable without a `RepoFactory`.
pub(crate) async fn get_or_load_repo_in<R>(
    repos: &MononokeRepos<R>,
    configs: &MononokeConfigs,
    repo_filter: Option<&RepoFilter>,
    repo_name: &str,
) -> Result<Option<Arc<R>>>
where
    R: Send + Sync + 'static,
{
    if let Some(repo) = repos.get_by_name(repo_name) {
        return Ok(Some(repo));
    }
    if !repos.loads_on_demand() {
        return Ok(None);
    }
    if repo_filter.is_some_and(|filter| !filter(repo_name)) {
        return Ok(None);
    }
    // Tier membership is the authorisation boundary: nothing outside the
    // manifest is read, let alone built. Misses are rare, so no cache.
    let Some(repo_id) = configs.manifest().and_then(|manifest| {
        manifest
            .repos
            .iter()
            .find(|entry| entry.repo_name == repo_name)
            .map(|entry| entry.repo_id)
    }) else {
        return Ok(None);
    };
    let repo_config = match configs.get_or_load_repo_config(repo_name) {
        Ok(repo_config) => repo_config,
        Err(e) => {
            STATS::repo_load_on_demand_failed.add_value(1);
            warn!(
                "Tier repo {} is in the manifest but its config did not load: {:#}",
                repo_name, e
            );
            return Err(e).with_context(|| format!("loading config for tier repo {repo_name}"));
        }
    };
    if !repo_config.enabled {
        return Ok(None);
    }

    // No-op if a concurrent request or the reconcile loop got there first.
    repos.add_placeholder(repo_name, repo_id);
    match repos.get(repo_name).await {
        Ok(Some(repo)) => {
            STATS::repo_loaded_on_demand.add_value(1);
            info!("Loaded tier repo on demand: {}", repo_name);
            Ok(Some(repo))
        }
        Ok(None) => Ok(None),
        Err(e) => {
            STATS::repo_load_on_demand_failed.add_value(1);
            warn!("Tier repo {} did not build on demand: {:#}", repo_name, e);
            Err(e.context(format!("loading tier repo {repo_name} on demand")))
        }
    }
}

/// Adapts `MononokeConfigs` to the `config_reconcile::ConfigSource` trait.
struct ReconcileConfigSource {
    configs: Arc<MononokeConfigs>,
    tier: String,
    shared: Arc<SharedGenerations>,
    // Memoizes spec_hash keyed by the RepoSpec Arc identity. See ReconcileDriver.
    spec_hash_cache: Arc<Mutex<HashMap<String, (Arc<RepoSpec>, u64)>>>,
}

impl ConfigSource for ReconcileConfigSource {
    fn manifest(&self) -> Vec<ManifestEntry> {
        self.configs.manifest().map_or_else(Vec::new, |m| {
            m.repos
                .iter()
                .map(|e| ManifestEntry {
                    name: e.repo_name.clone(),
                    is_deep_sharded: e.is_deep_sharded,
                })
                .collect()
        })
    }

    fn desired(&self, name: &str) -> Option<DesiredRepo> {
        let spec = self.configs.live_repo_spec(name)?;

        // Reuse the cached hash when the live RepoSpec Arc is pointer-identical to
        // the one we hashed last time (ConfigHandle::get() is pointer-stable while
        // unchanged), else recompute. spec_hash stays the drift signal — pointer
        // identity only decides whether to re-serialize.
        let hash = memoized_spec_hash(&self.spec_hash_cache, name, &spec, spec_hash)?;

        Some(DesiredRepo {
            enabled: spec.enabled,
            spec_hash: hash,
            shared_gen: self.shared.of_repo(&spec, &self.tier),
        })
    }
}

/// Adapts `MononokeConfigs` + `RepoFactory` + `MononokeRepos` to the
/// `config_reconcile::RepoManager` trait: builds a repo from its live config
/// (async), then inserts it under `MononokeRepos`' update lock (sync).
struct ReconcileRepoManager<Repo> {
    configs: Arc<MononokeConfigs>,
    repo_factory: Arc<RepoFactory>,
    repos: Arc<MononokeRepos<Repo>>,
    tier: String,
    shared: Arc<SharedGenerations>,
    redaction_disabled: bool,
    /// Rebuilds skipped this pass because the config was unchanged.
    unchanged: AtomicUsize,
}

#[async_trait]
impl<Repo> RepoManager for ReconcileRepoManager<Repo>
where
    Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>
        + RepoConfigRef
        + Send
        + Sync
        + 'static,
{
    fn loaded_names(&self) -> HashSet<String> {
        self.repos.iter_loaded_names().collect()
    }

    async fn build_and_apply(&self, name: &str, deep: bool) -> Result<Option<RepoGeneration>> {
        let (spec, source) = self
            .configs
            .live_repo_spec_with_source(name)
            .context("reconcile: no live RepoSpec (unsubscribed or unreadable)")?;
        let spec_hash = spec_hash(&spec)?;
        let shared_gen = self.shared.of_repo(&spec, &self.tier);
        let manifest = self.configs.manifest().context("reconcile: no manifest")?;
        let mut repo_config =
            parse_repo_spec(Arc::unwrap_or_clone(spec), &self.tier, &manifest.storage)?;
        if self.redaction_disabled {
            repo_config.redaction = Redaction::Disabled;
        }
        let generation = RepoGeneration {
            spec_hash,
            shared_gen,
        };
        let unchanged = self
            .repos
            .get_by_name(name)
            .is_some_and(|serving| *serving.repo_config() == repo_config);
        let present = if unchanged {
            self.unchanged.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            let repo_id = repo_config.repoid.id();
            let common_config = self.configs.repo_configs().common.clone();
            let repo = self
                .repo_factory
                .build(name.to_string(), repo_config, common_config, source)
                .await?;
            // The build (async) is done; the insert below is synchronous and takes
            // the MononokeRepos update lock internally — no lock across an await.
            if deep {
                self.repos
                    .reload_if_present(repo_id, name.to_string(), repo)
            } else {
                self.repos.reload(vec![(repo_id, name.to_string(), repo)]);
                true
            }
        };
        // A deep-sharded repo that left this job during the build records nothing.
        Ok(present.then_some(generation))
    }

    fn drop_repo(&self, name: &str) {
        self.repos.remove(name);
        self.configs.remove_repo_config_handle(name);
    }
}

/// Holds all state a reconciliation pass needs and runs one pass via `pass()`.
/// Shared (via `Arc`) between the background loop and the public `reconcile`
/// entry point. Owns the per-repo reconcile state, the spec-hash cache (perf),
/// and the single-flight lock.
struct ReconcileDriver<Repo> {
    configs: Arc<MononokeConfigs>,
    repo_factory: Arc<RepoFactory>,
    repos: Arc<MononokeRepos<Repo>>,
    redaction_disabled: bool,
    // Per-repo reconcile state (loaded generation or last failure), keyed by
    // name. Empty until reconcile runs; driven by the config_reconcile crate.
    reconcile_state: Arc<ArcSwap<HashMap<String, RepoState>>>,
    // Carried between passes so rebuilds after a shared-config change keep
    // their offsets, and so the loop wakes when the next one is due.
    splay_state: Mutex<SplayState>,
    // Memoizes spec_hash keyed by RepoSpec Arc identity so an unchanged repo is
    // not re-serialized every pass. Stores the Arc itself (not a raw pointer) to
    // pin the allocation, so Arc::ptr_eq can't false-match a reused address (ABA).
    spec_hash_cache: Arc<Mutex<HashMap<String, (Arc<RepoSpec>, u64)>>>,
    // Single-flight guard: only one pass runs at a time.
    reconcile_lock: Arc<tokio::sync::Mutex<()>>,
}

/// What one reconcile pass leaves for the next about a splayed shared-config change.
#[derive(Default)]
struct SplayState {
    shared_gens_seen: SharedGensSeen,
    next_due: Option<Instant>,
    deferred: usize,
}

/// Single-flight guard: run `body` under `lock` iff it is free (`try_lock`), else
/// skip (never queue) and return `None`. The tokio Mutex is await-safe, so the
/// guard is intentionally held across `body`'s await.
async fn run_exclusive<F, Fut, T>(lock: &tokio::sync::Mutex<()>, body: F) -> Option<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    let Ok(_guard) = lock.try_lock() else {
        return None;
    };
    Some(body().await)
}

impl<Repo> ReconcileDriver<Repo>
where
    Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>
        + RepoConfigRef
        + Send
        + Sync
        + 'static,
{
    /// One reconciliation pass, shared by the background loop and `reconcile`.
    /// No-op when `use_config_reconcile` is off or there is no tier/manifest.
    async fn pass(&self) -> Result<()> {
        if !justknobs::eval("scm/mononoke:use_config_reconcile", None, None) {
            return Ok(());
        }
        // Needs a tier + manifest to parse specs and know membership.
        let Some(tier) = self.configs.tier_name().map(str::to_owned) else {
            return Ok(());
        };
        if self.configs.manifest().is_none() {
            return Ok(());
        }

        // Single-flight: skip if a pass is already running. The loop is the only
        // caller today and is sequential, so this can't currently contend; it
        // keeps the pub reconcile() entry safe if ever driven concurrently. The
        // tokio Mutex is await-safe, so the guard is held across the whole pass.
        // A held lock means "skip this pass", which is a success (Ok(())).
        run_exclusive(&self.reconcile_lock, move || async move {
            let manifest = self
                .configs
                .manifest()
                .context("reconcile: no manifest for shared generations")?;
            let shared = Arc::new(SharedGenerations::new(&manifest.storage)?);
            let config = ReconcileConfigSource {
                configs: self.configs.clone(),
                tier: tier.clone(),
                shared: shared.clone(),
                spec_hash_cache: self.spec_hash_cache.clone(),
            };
            let manager = ReconcileRepoManager {
                configs: self.configs.clone(),
                repo_factory: self.repo_factory.clone(),
                repos: self.repos.clone(),
                tier,
                shared,
                redaction_disabled: self.redaction_disabled,
                unchanged: AtomicUsize::new(0),
            };
            let current = self.reconcile_state.load_full();
            let splay = Splay {
                window: Duration::from_secs(justknobs::get_as::<u64>(RELOAD_SPLAY_KNOB, None)),
                now: Instant::now(),
            };
            let shared_gens_seen = self
                .splay_state
                .lock()
                .expect("splay_state poisoned")
                .shared_gens_seen
                .clone();
            let outcome = match config_reconcile::reconcile(
                &config,
                &manager,
                &current,
                REPOS_MANAGER_CONCURRENCY,
                splay,
                shared_gens_seen,
            )
            .await
            {
                Ok(outcome) => outcome,
                Err(e) => {
                    // The splay state stays as it was for the next pass.
                    let deferred = self
                        .splay_state
                        .lock()
                        .expect("splay_state poisoned")
                        .deferred;
                    STATS::reconcile_deferred.add_value(deferred as i64);
                    return Err(e);
                }
            };

            // Reconcile counts a skipped rebuild as applied; it built nothing.
            let unchanged = manager.unchanged.load(Ordering::Relaxed);
            let rebuilt = outcome.rebuilt.saturating_sub(unchanged);
            // A pass that applies nothing is routine: the first one after
            // startup usually finds every repo unchanged.
            if outcome.built + rebuilt + outcome.dropped > 0 || !outcome.failed.is_empty() {
                info!(
                    "reconcile: built={} rebuilt={} unchanged={} dropped={} failed={} deferred={}",
                    outcome.built,
                    rebuilt,
                    unchanged,
                    outcome.dropped,
                    outcome.failed.len(),
                    outcome.deferred,
                );
            } else if unchanged > 0 || outcome.deferred > 0 {
                debug!(
                    "reconcile: no changes applied (unchanged={} deferred={})",
                    unchanged, outcome.deferred,
                );
            }
            STATS::reconcile_applied.add_value((outcome.built + rebuilt) as i64);
            STATS::reconcile_unchanged.add_value(unchanged as i64);
            STATS::reconcile_dropped.add_value(outcome.dropped as i64);
            STATS::reconcile_failed_repos.add_value(outcome.failed.len() as i64);
            STATS::reconcile_deferred.add_value(outcome.deferred as i64);
            *self.splay_state.lock().expect("splay_state poisoned") = SplayState {
                shared_gens_seen: outcome.shared_gens_seen,
                next_due: outcome.next_due,
                deferred: outcome.deferred,
            };
            self.reconcile_state.store(Arc::new(outcome.next_state));

            // Evict spec-hash cache entries for repos no longer in the manifest.
            // Done here (once per pass) because ConfigSource has no end-of-pass
            // hook. The std Mutex is acquired after all awaits, not across one.
            if let Some(manifest) = self.configs.manifest() {
                let live: HashSet<&str> = manifest
                    .repos
                    .iter()
                    .map(|e| e.repo_name.as_str())
                    .collect();
                retain_live_cache_entries(&self.spec_hash_cache, &live);
            }

            Ok(())
        })
        .await
        .unwrap_or(Ok(()))
    }
}

/// Spreads rebuilds after a shared-config change over this many seconds.
const RELOAD_SPLAY_KNOB: &str = "scm/mononoke:common_config_reload_splay_secs";

/// Wake policy (pure): the backstop, or sooner if a splayed rebuild comes due.
/// Floored at 1s: a failed or skipped pass leaves `next_due` as it was, so a due
/// time already past must not spin the loop.
fn next_wake(backstop: Duration, until_next_due: Option<Duration>) -> Duration {
    until_next_due.map_or(backstop, |due| {
        backstop.min(due.max(Duration::from_secs(1)))
    })
}

/// Interval policy (pure): fixed 60s backstop when off; the tunable value floored
/// at 1s when on, so a 0 can't spin the loop.
fn tick_interval_secs(reconcile_on: bool, knob_secs: u64) -> u64 {
    if reconcile_on { knob_secs.max(1) } else { 60 }
}

/// Backstop interval. Only reads the tunable knob when reconcile is on (it may be
/// unregistered otherwise, and missing-knob reads are expensive).
fn reconcile_tick_interval() -> Duration {
    let on = justknobs::eval("scm/mononoke:use_config_reconcile", None, None);
    let knob_secs = if on {
        justknobs::get_as::<u64>("scm/mononoke:config_reconcile_tick_interval_secs", None)
    } else {
        0
    };
    Duration::from_secs(tick_interval_secs(on, knob_secs))
}

/// Loop mechanics for the reconcile background task: run `pass` immediately, then
/// forever wake on either a `trigger` notification or the backstop
/// `sleep(next_interval())` and run `pass` again. `next_interval` is injected (not
/// read from justknobs here) so tests can drive the loop without registered knobs.
async fn reconcile_loop<F, Fut>(
    pass: F,
    trigger: Arc<Notify>,
    mut next_interval: impl FnMut() -> Duration,
) where
    F: Fn() -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        pass().await;
        tokio::select! {
            _ = trigger.notified() => {}
            _ = tokio::time::sleep(next_interval()) => {}
        }
    }
}

/// Spawn the background reconcile loop. Owns an Arc to the driver (task is
/// `'static`); aborted on Drop. Runs one pass immediately, then again on each
/// wake — a config-change trigger, the backstop, or a splayed rebuild coming due.
fn spawn_reconcile_loop<Repo>(
    driver: Arc<ReconcileDriver<Repo>>,
    trigger: Arc<Notify>,
) -> JoinHandle<()>
where
    Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>
        + RepoConfigRef
        + Send
        + Sync
        + 'static,
{
    let wake_driver = driver.clone();
    mononoke::spawn_task(reconcile_loop(
        move || {
            let driver = driver.clone();
            async move {
                // Time the whole pass; keep the metric emission adjacent to the pass.
                let start = Instant::now();
                if let Err(e) = driver.pass().await {
                    warn!("reconcile pass failed: {e:#}");
                }
                STATS::reconcile_tick_duration_ms
                    .add_value(start.elapsed().as_millis().try_into().unwrap_or(i64::MAX));
                STATS::repos_assigned.add_value(driver.repos.iter_names().count() as i64);
                STATS::repos_built.add_value(driver.repos.iter_loaded_names().count() as i64);
            }
        },
        trigger,
        move || {
            let next_due = wake_driver
                .splay_state
                .lock()
                .expect("splay_state poisoned")
                .next_due;
            next_wake(
                reconcile_tick_interval(),
                next_due.map(|due| due.saturating_duration_since(Instant::now())),
            )
        },
    ))
}

/// A `ConfigUpdateReceiver` that wakes the reconcile loop on any config change
/// (bulk or per-repo). `notify_one` coalesces changes into one wake.
struct ReconcileTrigger {
    notify: Arc<Notify>,
}

#[async_trait]
impl ConfigUpdateReceiver for ReconcileTrigger {
    async fn apply_update(
        &self,
        _repo_configs: Arc<RepoConfigs>,
        _storage_configs: Arc<StorageConfigs>,
    ) -> Result<()> {
        self.notify.notify_one();
        Ok(())
    }

    async fn apply_repo_update(&self, _repo_name: &str, _repo_config: &RepoConfig) -> Result<()> {
        self.notify.notify_one();
        Ok(())
    }
}

impl<Repo> Drop for MononokeReposManager<Repo> {
    fn drop(&mut self) {
        // Stop the loop; otherwise it would run forever on a torn-down manager.
        if let Some(handle) = self.reconcile_loop_handle.as_ref() {
            handle.abort();
            debug!("Config reconcile loop stopped: repos manager dropped");
        }
    }
}

impl<R> MononokeReposManager<R> {
    pub fn make_mononoke_api(&self) -> Result<Mononoke<R>> {
        // Legacy seed from the per-task config cache. Skipped when the map is
        // manifest-derived: that map was seeded at construction and a cache-
        // derived store here would be a second writer with different contents
        // (`load_all_repo_configs` warn-drops repos whose spec fails to parse).
        //
        // Note: the watcher receiver is already registered by the time we
        // run, so in principle a configerator update fired between
        // registration and this call could land a fresher snapshot that
        // this store() overwrites. In practice make_mononoke_api runs
        // milliseconds after registration during startup, before any
        // notification is plausible; subsequent apply_update calls will
        // correct any drift within one config refresh cycle.
        if !self.names_from_manifest {
            let configs = self.configs.load_all_repo_configs()?;
            self.repo_names_in_tier
                .store(Arc::new(build_repo_names_in_tier(
                    configs.iter().map(|(name, config)| (name, config)),
                )));
        }
        Mononoke::new(self.repos.clone(), self.repo_names_in_tier.clone())
    }
}

/// Build the tier-wide (name -> default identity scheme) map from an iterator
/// of borrowed (repo_name, RepoConfig) pairs, dropping disabled repos. Takes
/// borrows to avoid cloning the heavy RepoConfig struct just to read two
/// fields.
fn build_repo_names_in_tier<'a, I>(configs: I) -> HashMap<String, CommitIdentityScheme>
where
    I: IntoIterator<Item = (&'a String, &'a RepoConfig)>,
{
    configs
        .into_iter()
        .filter(|(_, config)| config.enabled)
        .map(|(name, config)| (name.clone(), config.default_commit_identity_scheme.clone()))
        .collect()
}

const TIER_NAMES_FROM_MANIFEST_KNOB: &str = "scm/mononoke:tier_repo_names_from_manifest";

const GIT_SPEC_PATH_SEGMENT: &str = "/repos/git/";
const HG_SPEC_PATH_SEGMENT: &str = "/repos/hg/";
const P4_SPEC_PATH_SEGMENT: &str = "/repos/p4/";

/// The identity scheme of a manifest entry, from where its spec lives.
/// Configerator keeps Git repo specs under `repos/git/`, Mercurial repo
/// specs under `repos/hg/` and Perforce mirror specs under `repos/p4/`, with
/// no exceptions in the tree (10,051 and 52 entries in the SCS manifest on
/// 2026-10-05). Anything else is `UNKNOWN`: it is still listed, but never
/// matches a scheme filter.
fn scheme_from_config_path(config_path: &str) -> CommitIdentityScheme {
    if config_path.contains(GIT_SPEC_PATH_SEGMENT) {
        CommitIdentityScheme::GIT
    } else if config_path.contains(HG_SPEC_PATH_SEGMENT) {
        CommitIdentityScheme::HG
    } else if config_path.contains(P4_SPEC_PATH_SEGMENT) {
        CommitIdentityScheme::P4
    } else {
        CommitIdentityScheme::UNKNOWN
    }
}

/// Tier-wide (name -> scheme) map as a pure function of the manifest: every
/// entry, no spec reads, no per-task state. The manifest is what the tier
/// serves, so the map is complete by construction and cannot shrink with what
/// this task happens to have cached.
fn repo_names_from_manifest(manifest: &TierManifest) -> HashMap<String, CommitIdentityScheme> {
    manifest
        .repos
        .iter()
        .map(|entry| {
            (
                entry.repo_name.clone(),
                scheme_from_config_path(&entry.config_path),
            )
        })
        .collect()
}

/// A `ConfigUpdateReceiver` that keeps the tier-wide repo names map fresh on
/// every config change. Repo (re)building is owned by the reconcile loop;
/// reconcile does not maintain this map, so it lives here.
pub struct MononokeConfigUpdateReceiver {
    // Shared with MononokeReposManager and Mononoke<R>. Updated on every
    // config change so `list_repos` sees newly-added repos without waiting
    // for a process restart.
    repo_names_in_tier: Arc<ArcSwap<HashMap<String, CommitIdentityScheme>>>,
    // Some only in manifest mode with the knob on, decided once at construction.
    // Weak: MononokeConfigs owns the receiver list.
    manifest_source: Option<Weak<MononokeConfigs>>,
}

impl MononokeConfigUpdateReceiver {
    fn new(
        repo_names_in_tier: Arc<ArcSwap<HashMap<String, CommitIdentityScheme>>>,
        manifest_source: Option<Weak<MononokeConfigs>>,
    ) -> Self {
        Self {
            repo_names_in_tier,
            manifest_source,
        }
    }

    /// Rebuild the tier-wide repo names map from `repo_configs` (the full
    /// tier config, not the per-task subset) and atomically swap it in.
    fn refresh_repo_names_in_tier(&self, repo_configs: &RepoConfigs) {
        let names = build_repo_names_in_tier(repo_configs.iter());
        self.repo_names_in_tier.store(Arc::new(names));
    }

    /// Rebuild the map from the live manifest snapshot. The handle always holds
    /// the last good manifest, so this cannot fail once in manifest mode.
    fn refresh_from_manifest(&self, configs: &MononokeConfigs) {
        match configs.manifest() {
            Some(manifest) => {
                STATS::tier_names_manifest_len.add_value(manifest.repos.len() as i64);
                self.repo_names_in_tier
                    .store(Arc::new(repo_names_from_manifest(&manifest)));
            }
            // Cannot happen: manifest_source is only Some in manifest mode. Keep
            // the map as it was rather than shrink it to the per-task cache.
            None => warn!("tier names: manifest source set but no manifest; map left unchanged"),
        }
    }
}

#[async_trait]
impl ConfigUpdateReceiver for MononokeConfigUpdateReceiver {
    async fn apply_update(
        &self,
        repo_configs: Arc<RepoConfigs>,
        _: Arc<StorageConfigs>,
    ) -> Result<()> {
        // Keep the tier-wide names map fresh so `list_repos` reflects the latest
        // tier config. Repo (re)building is owned by the reconcile loop.
        match self.manifest_source.as_ref().and_then(Weak::upgrade) {
            Some(configs) => self.refresh_from_manifest(&configs),
            None => self.refresh_repo_names_in_tier(&repo_configs),
        }
        STATS::tier_names_len.add_value(self.repo_names_in_tier.load().len() as i64);
        Ok(())
    }

    async fn apply_repo_update(&self, repo_name: &str, repo_config: &RepoConfig) -> Result<()> {
        // Manifest-derived map: membership comes from the manifest alone, so a
        // repo's own spec changing adds or removes nothing. Patching from
        // `enabled` here would make this a second writer with other semantics.
        if self.manifest_source.is_some() {
            return Ok(());
        }
        // Patch the names map from the passed-in arg (authoritative for THIS
        // repo). rcu() makes the load-mutate-store atomic against concurrent
        // writers (e.g. apply_update's bulk refresh); idempotent for this shape.
        self.repo_names_in_tier.rcu(|current| {
            let mut snapshot = (**current).clone();
            if repo_config.enabled {
                snapshot.insert(
                    repo_name.to_string(),
                    repo_config.default_commit_identity_scheme.clone(),
                );
            } else {
                snapshot.remove(repo_name);
            }
            Arc::new(snapshot)
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests;
