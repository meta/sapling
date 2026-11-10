/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! `MononokeConfigs` — the per-service entry point for fetching repo configs
//! and registering for config-update notifications.
//!
//! Internal organization:
//! - [`ConfigUpdateReceiver`] trait is in `receiver`
//! - The unified watcher task (single tokio task that owns the config source —
//!   the tier manifest — plus the per-repo control channel and
//!   per-repo wait fan-in) is in `watcher`
//!
//! `MononokeConfigs` itself owns the `ArcSwap` state and the task handles.

mod receiver;
mod watcher;

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::RwLock;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use arc_swap::ArcSwap;
use cached_config::ConfigHandle;
use cached_config::ConfigStore;
use cloned::cloned;
use metaconfig_parser::RepoConfigs;
use metaconfig_parser::StorageConfigs;
use metaconfig_parser::configerator_manifest_handle;
use metaconfig_parser::configerator_repo_spec_handle;
use metaconfig_parser::parse_manifest_common_and_storage;
use metaconfig_parser::parse_repo_spec;
use metaconfig_types::RepoConfig;
use repos::RepoSpec;
use repos::TierManifest;
use stats::prelude::*;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::error;
use tracing::info;
use tracing::warn;

pub use crate::receiver::ConfigUpdateReceiver;
use crate::watcher::ConfigSource;
use crate::watcher::RepoHandleEvent;
use crate::watcher::liveness_updater;
use crate::watcher::unified_config_watcher;

/// The configerator path of a tier's manifest.
pub fn tier_manifest_path(tier: &str) -> String {
    format!("scm/mononoke/repos/tiers/{tier}_manifest")
}

/// Shorthand for a value swapped atomically via `arc_swap`. Used for the
/// stateful slots inside `MononokeConfigs` that receivers read.
pub(crate) type Swappable<T> = Arc<ArcSwap<T>>;

define_stats! {
    prefix = "mononoke.config_refresh";
    refresh_failure_count: timeseries(Average, Sum, Count),
    refresh_success_count: timeseries(Average, Sum, Count),
    liveness_count: timeseries(Average, Sum, Count),
    spurious_reload_suppressed: timeseries(Average, Sum, Count),
    merge_skipped_no_handle: timeseries(Average, Sum, Count),
    bulk_reload_spec_reused: timeseries(Sum),
    bulk_reload_spec_parsed: timeseries(Sum),
    per_repo_refresh_count: timeseries(Average, Sum, Count),
    per_repo_refresh_failure_count: timeseries(Average, Sum, Count),
    ensure_repo_handle_success_count: timeseries(Average, Sum, Count),
    ensure_repo_handle_failure_count: timeseries(Average, Sum, Count),
    manifest_common_parse_failure: timeseries(Average, Sum, Count),
    // Presence-keyed: emitted once per task on the manifest-backed path.
    tier_blob_skipped: timeseries(Average, Sum, Count),
}

/// Outcome of a batch config load. Separates the configs that loaded/parsed
/// successfully (`loaded`) from the per-repo parse failures (`failed`), so
/// callers can fail closed instead of silently serving fewer repos.
pub struct BatchLoadOutcome {
    /// Repos that loaded successfully (cached hits + freshly parsed).
    pub loaded: Vec<(String, RepoConfig)>,
    /// Repos whose `RepoSpec` failed to parse, with the parse error.
    pub failed: Vec<(String, anyhow::Error)>,
}

/// Configuration provider and update notifier for all of Mononoke services
/// and jobs. The configurations provided by this struct are always up-to-date
/// with its source.
pub struct MononokeConfigs {
    repo_configs: Swappable<RepoConfigs>,
    storage_configs: Swappable<StorageConfigs>,
    update_receivers: Swappable<Vec<Arc<dyn ConfigUpdateReceiver>>>,
    maybe_config_updater: Option<JoinHandle<()>>,
    maybe_liveness_updater: Option<JoinHandle<()>>,
    // Per-repo split-loading fields
    maybe_manifest_handle: Option<ConfigHandle<TierManifest>>,
    repo_handles: Arc<RwLock<HashMap<String, ConfigHandle<RepoSpec>>>>,
    config_store: Option<ConfigStore>,
    /// Tier name derived from the configerator config path.
    /// Used for resolving tier_overrides in RepoSpec configs during split-loading.
    tier_name: Option<String>,
    /// Sender side of the control channel that notifies `unified_config_watcher`
    /// when a new per-repo `ConfigUpdateWatcher<RepoSpec>` has been created
    /// (via sync_repo_handles or load_repo_config_handle) so it can be added
    /// to the watcher's `FuturesUnordered` set. `None` when split-loading is
    /// disabled.
    repo_handle_event_tx: Option<mpsc::UnboundedSender<RepoHandleEvent>>,
}

impl MononokeConfigs {
    /// Create a new instance of MononokeConfigs for `tier`, read through
    /// `config_store`: `scm/mononoke/repos/tiers/<tier>_manifest` plus the
    /// per-repo `RepoSpec` each manifest entry names, all watched for changes.
    pub fn new(tier: &str, config_store: &ConfigStore, runtime_handle: Handle) -> Result<Self> {
        let update_receivers = Arc::new(ArcSwap::from_pointee(vec![]));

        let manifest_handle =
            configerator_manifest_handle(&tier_manifest_path(tier), config_store)?;
        // The manifest is the sole source of common/storage; fail closed at startup.
        let manifest = manifest_handle.get();
        let (common, storage) = parse_manifest_common_and_storage(&manifest).context(
            "common/storage could not be parsed from the tier manifest; refusing to start",
        )?;
        STATS::tier_blob_skipped.add_value(1);
        // Repos load on demand.
        let storage_configs = Arc::new(ArcSwap::from_pointee(storage));
        let repo_configs = Arc::new(ArcSwap::from_pointee(RepoConfigs::new(
            HashMap::new(),
            common,
        )));
        let tier_name = Some(tier.to_owned());
        let maybe_manifest_handle = Some(manifest_handle.clone());

        let repo_handles = Arc::new(RwLock::new(HashMap::new()));

        // Control channel for the per-repo arm of unified_config_watcher: a
        // single registration channel avoids the duplicate-registration race a
        // seed loop would have.
        let (repo_handle_event_tx, repo_handle_event_rx) = mpsc::unbounded_channel();
        let repo_handle_event_tx = Some(repo_handle_event_tx);

        let config_source = ConfigSource {
            handle: manifest_handle,
            tier: tier.to_owned(),
        };

        let maybe_liveness_updater = Some(runtime_handle.spawn(liveness_updater()));

        let maybe_config_updater = {
            cloned!(
                repo_handles,
                repo_configs,
                storage_configs,
                update_receivers,
            );
            let config_store_clone = config_store.clone();
            Some(runtime_handle.spawn(unified_config_watcher(
                config_source,
                repo_handles,
                config_store_clone,
                repo_configs,
                storage_configs,
                update_receivers,
                Some(repo_handle_event_rx),
            )))
        };

        Ok(Self {
            repo_configs,
            storage_configs,
            update_receivers,
            maybe_config_updater,
            maybe_liveness_updater,
            maybe_manifest_handle,
            repo_handles,
            config_store: Some(config_store.clone()),
            tier_name,
            repo_handle_event_tx,
        })
    }

    /// Creates an instance backed by fixed in-memory configurations.
    pub fn new_static(repo_configs: RepoConfigs, storage_configs: StorageConfigs) -> Self {
        Self {
            repo_configs: Arc::new(ArcSwap::from_pointee(repo_configs)),
            storage_configs: Arc::new(ArcSwap::from_pointee(storage_configs)),
            update_receivers: Arc::new(ArcSwap::from_pointee(vec![])),
            maybe_config_updater: None,
            maybe_liveness_updater: None,
            maybe_manifest_handle: None,
            repo_handles: Arc::new(RwLock::new(HashMap::new())),
            config_store: None,
            tier_name: None,
            repo_handle_event_tx: None,
        }
    }

    /// The latest repo configs fetched from the underlying configuration store.
    pub fn repo_configs(&self) -> Arc<RepoConfigs> {
        // Load full since there could be lots of calls to repo_configs.
        self.repo_configs.load_full()
    }

    /// The latest storage configs fetched from the underlying configuration store.
    pub fn storage_configs(&self) -> Arc<StorageConfigs> {
        // Load full since there could be lots of calls to storage_configs.
        self.storage_configs.load_full()
    }

    /// Returns the ConfigStore, if available (configerator-backed configs only).
    pub fn config_store(&self) -> Option<&ConfigStore> {
        self.config_store.as_ref()
    }

    /// Returns the current TierManifest, if split-loading is enabled.
    pub fn manifest(&self) -> Option<Arc<TierManifest>> {
        self.maybe_manifest_handle.as_ref().map(|h| h.get())
    }

    /// Configerator version of the tier manifest this task last loaded, when
    /// configs come from configerator. Lets a caller tell "this task is at
    /// or past my change" from "this task is stale".
    pub fn manifest_version(&self) -> Option<String> {
        self.maybe_manifest_handle
            .as_ref()
            .and_then(|h| h.get_with_version().1)
            .map(|info| info.version)
    }

    /// The tier name (derived from the configerator config path), if split-
    /// loading is enabled. The reconcile controller needs it to parse `RepoSpec`s.
    pub fn tier_name(&self) -> Option<&str> {
        self.tier_name.as_deref()
    }

    /// The live per-repo `RepoSpec` read straight from its subscribed config
    /// handle (bypassing the bulk config cache), so the reconcile controller sees
    /// current desired state for drift detection. Returns `None` when the repo
    /// has no subscribed handle (not split-loaded, or not yet subscribed) —
    /// callers must treat that as "unknown", never "drop".
    pub fn live_repo_spec(&self, repo_name: &str) -> Option<Arc<RepoSpec>> {
        self.repo_handles
            .read()
            .ok()?
            .get(repo_name)
            .map(|handle| handle.get())
    }

    /// [`Self::live_repo_spec`] with the configerator version of that spec, for
    /// a config built from it to record.
    pub fn live_repo_spec_with_version(
        &self,
        repo_name: &str,
    ) -> Option<(Arc<RepoSpec>, Option<String>)> {
        self.repo_handles.read().ok()?.get(repo_name).map(|handle| {
            let (spec, version_info) = handle.get_with_version();
            (spec, version_info.map(|info| info.version))
        })
    }

    /// Is automatic update of the underlying configuration enabled?
    pub fn auto_update_enabled(&self) -> bool {
        // If the config updater handle is none, configs won't be updated.
        self.maybe_config_updater.is_some()
    }

    /// Register an instance of ConfigUpdateReceiver to receive notifications of updates to
    /// the underlying configs which can then be used to perform further actions. Note that
    /// the operation performed by the ConfigUpdateReceiver should not be too long running.
    ///
    /// Thread-safe: uses `rcu` so concurrent registrations from multiple
    /// services don't lose entries to a load-modify-store race.
    pub fn register_for_update(&self, update_receiver: Arc<dyn ConfigUpdateReceiver>) {
        self.update_receivers.rcu(|current| {
            let mut next: Vec<Arc<dyn ConfigUpdateReceiver>> = (**current).clone();
            next.push(update_receiver.clone());
            next
        });
    }

    /// Drop the per-repo ConfigHandle (called by ShardManager on_drop_shard via
    /// repos_manager::remove_repo). Symmetric counterpart to load_repo_config_handle.
    /// No-op if no handle is held.
    pub fn remove_repo_config_handle(&self, repo_name: &str) {
        let had_handle = match self.repo_handles.write() {
            Ok(mut handles) => handles.remove(repo_name).is_some(),
            Err(e) => {
                error!("repo_handles lock poisoned while removing {repo_name}: {e}");
                false
            }
        };
        // Only evict the bulk repo_configs entry when we actually dropped a handle
        // (i.e. a split-loaded repo). This forces a later get_or_load to re-parse
        // from the fresh handle instead of serving stale config (S685134), while
        // leaving a legacy-blob-only entry intact so it can still be re-added.
        if had_handle {
            info!("Removed config handle for repo: {repo_name}");
            if self.repo_configs.load().repos.contains_key(repo_name) {
                self.repo_configs.rcu(|current| {
                    let mut next = (**current).clone();
                    next.remove_repo(repo_name);
                    next
                });
            }
        }
    }

    /// Create a per-repo ConfigHandle on-demand. Reached from ShardManager
    /// on_add_shard via `repos_manager::add_repo` -> `get_or_load_repo_config`;
    /// nothing in the sharding layer calls it directly.
    pub fn load_repo_config_handle(&self, repo_name: &str) -> Result<()> {
        // Fast path: already loaded
        if self
            .repo_handles
            .read()
            .map_err(|e| anyhow!("repo_handles lock poisoned: {e}"))?
            .contains_key(repo_name)
        {
            return Ok(());
        }

        let manifest = self
            .maybe_manifest_handle
            .as_ref()
            .context("No manifest handle available")?
            .get();

        let entry = manifest
            .repos
            .iter()
            .find(|e| e.repo_name == repo_name)
            .ok_or_else(|| anyhow!("Repo {repo_name} not found in manifest"))?;

        let config_store = self
            .config_store
            .as_ref()
            .context("No config store available")?;

        let handle = configerator_repo_spec_handle(&entry.config_path, config_store)?;
        // Derive the watcher BEFORE moving the handle into repo_handles —
        // the watcher only requires &self on ConfigHandle, but we hand off
        // ownership of the handle to the HashMap below.
        //
        // Subscription is live from `handle.watcher()` onwards: any
        // configerator updates between here and when the unified_config_watcher
        // loop processes the `Added` event get buffered in the
        // `tokio::sync::watch::Receiver` (latest-value semantics) and are
        // delivered on the first `wait_for_next` call. No update can be
        // dropped.
        let watcher = handle.watcher();
        self.repo_handles
            .write()
            .map_err(|e| anyhow!("repo_handles lock poisoned: {e}"))?
            .insert(repo_name.to_owned(), handle);
        // Notify unified_config_watcher to start watching this repo for
        // per-repo content updates. Send AFTER the handle is in the map so
        // the `still_present` check in `handle_per_repo_fire` passes on
        // dispatch.
        match watcher {
            Ok(w) => {
                if let Some(tx) = self.repo_handle_event_tx.as_ref() {
                    if let Err(e) = tx.send(RepoHandleEvent::Added(repo_name.to_owned(), w)) {
                        warn!(
                            "Failed to send Added event for repo {repo_name} \
                             (watcher loop gone?): {e}",
                        );
                    }
                }
            }
            Err(e) => {
                warn!(
                    "load_repo_config_handle: failed to create watcher for {repo_name}, \
                     per-repo hot-reload disabled for this repo until restart: {e:?}",
                );
            }
        }
        Ok(())
    }

    /// Idempotent best-effort subscription of a per-repo `ConfigHandle`.
    /// No-op when already registered, when split-loading is off, or when
    /// the repo is not in the manifest (legacy-only). Subscription
    /// failure is the only `Err` path. Closes the gap that caused
    /// S678887.
    pub fn ensure_repo_config_handle(&self, repo_name: &str) -> Result<()> {
        if self
            .repo_handles
            .read()
            .map_err(|e| anyhow!("repo_handles lock poisoned: {e}"))?
            .contains_key(repo_name)
        {
            return Ok(());
        }

        let Some(manifest_handle) = self.maybe_manifest_handle.as_ref() else {
            return Ok(());
        };
        let manifest = manifest_handle.get();

        let Some(entry) = manifest.repos.iter().find(|e| e.repo_name == repo_name) else {
            return Ok(());
        };

        let config_store = self
            .config_store
            .as_ref()
            .context("No config store available")?;

        let handle = configerator_repo_spec_handle(&entry.config_path, config_store)
            .inspect_err(|_| {
                STATS::ensure_repo_handle_failure_count.add_value(1);
            })
            .with_context(|| {
                format!(
                    "ensure_repo_config_handle: failed to subscribe to repo {repo_name} \
                     (config_path={})",
                    entry.config_path
                )
            })?;
        let watcher = handle.watcher();

        // Check-under-write-lock: if a concurrent caller won the race,
        // drop our handle — its configerator subscription cancels on Drop.
        let inserted = {
            let mut handles = self
                .repo_handles
                .write()
                .map_err(|e| anyhow!("repo_handles lock poisoned: {e}"))?;
            if handles.contains_key(repo_name) {
                false
            } else {
                handles.insert(repo_name.to_owned(), handle);
                true
            }
        };

        if inserted {
            STATS::ensure_repo_handle_success_count.add_value(1);
            // Send Added AFTER insert so handle_per_repo_fire's still_present check passes.
            match watcher {
                Ok(w) => {
                    if let Some(tx) = self.repo_handle_event_tx.as_ref() {
                        if let Err(e) = tx.send(RepoHandleEvent::Added(repo_name.to_owned(), w)) {
                            warn!("Failed to send Added event for {repo_name}: {e}");
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to create watcher for {repo_name}, per-repo hot-reload \
                         disabled until restart: {e:?}"
                    );
                }
            }
        }
        Ok(())
    }

    /// Load a repo config on-demand. Checks the repo_configs cache first,
    /// falls back to loading from the TierManifest via ConfigHandle.
    ///
    /// Thread-safe: uses `ArcSwap::rcu` for write serialization. The closure
    /// inside `rcu` runs at least once (possibly multiple times on CAS retry),
    /// so the expensive work — ConfigHandle subscription, RepoSpec parsing —
    /// happens once OUTSIDE the closure.
    pub fn get_or_load_repo_config(&self, repo_name: &str) -> Result<RepoConfig> {
        // Subscribe per-repo handle before the cache lookup — otherwise the
        // fast path returns a legacy-blob entry without ever subscribing,
        // and a subsequent blob hot-reload silently drops the repo. S678887.
        self.ensure_repo_config_handle(repo_name)?;
        // Fast path: lock-free read from cache (covers both legacy blob
        // and previously loaded split-config repos)
        if let Some(config) = self.repo_configs.load_full().repos.get(repo_name) {
            return Ok((**config).clone());
        }

        // Slow path: try loading from manifest. If split-loading infrastructure
        // is unavailable (no manifest, no config store), this will fail and
        // we return the error — the fast path above already checked the legacy blob.
        let repo_config = self.load_and_parse_repo_config(repo_name)?;

        // Insert into cache via rcu. The closure is idempotent: on CAS retry
        // it re-runs against a fresher snapshot, and if a concurrent writer
        // (per-repo refresh, another get_or_load) already inserted this repo,
        // we keep their entry to avoid a redundant clone+store.
        self.repo_configs.rcu(|current| {
            let mut next = (**current).clone();
            if !next.repos.contains_key(repo_name) {
                next.insert_repo(repo_name.to_owned(), repo_config.clone());
            }
            next
        });

        // All paths leave the cache containing some entry for repo_name with
        // the same value (configs for one repo at one snapshot are
        // deterministic). Return what we parsed.
        Ok(repo_config)
    }

    /// Subscribe to a repo's ConfigHandle and parse its RepoSpec into a RepoConfig.
    /// This is the shared helper for get_or_load_repo_config and batch_load_repo_configs.
    fn load_and_parse_repo_config(&self, repo_name: &str) -> Result<RepoConfig> {
        self.load_repo_config_handle(repo_name)?;
        let handle = self
            .repo_handles
            .read()
            .map_err(|e| anyhow!("repo_handles lock poisoned: {e}"))?
            .get(repo_name)
            .context("handle not found after load")?
            .clone();
        let (repo_spec, version_info) = handle.get_with_version();
        let tier = self
            .tier_name
            .as_deref()
            .context("tier_name required for split-loading")?;
        let manifest = self
            .maybe_manifest_handle
            .as_ref()
            .context("manifest handle required for split-loading")?
            .get();
        let mut repo_config =
            parse_repo_spec(Arc::unwrap_or_clone(repo_spec), tier, &manifest.storage)?;
        // Provenance comes only from the ConfigHandle accessor, never from thrift RepoSpec fields.
        repo_config.config_version = version_info.map(|info| info.version);
        Ok(repo_config)
    }

    /// Batch-load repo configs, surfacing per-repo parse failures instead of
    /// warn+dropping them. Single rcu acquisition, single HashMap clone, single
    /// ArcSwap store regardless of how many repos are loaded.
    ///
    /// `loaded` holds the successes (cached hits + freshly parsed), `failed`
    /// holds the per-repo parse errors. The outer `Result` is reserved for
    /// infra errors (e.g. lock poison, handle subscription failure), not
    /// per-repo parse errors.
    pub fn batch_load_repo_configs_checked(
        &self,
        repo_names: &[String],
    ) -> Result<BatchLoadOutcome> {
        // Subscribe per-repo handles up-front; the cached-fast-path loop
        // below would otherwise skip subscription. S678887.
        for name in repo_names {
            self.ensure_repo_config_handle(name)?;
        }

        // Step 1: Separate cached from missing
        let current = self.repo_configs.load_full();
        let mut results: Vec<(String, RepoConfig)> = Vec::new();
        let mut missing: Vec<String> = Vec::new();

        for name in repo_names {
            if let Some(config) = current.repos.get(name.as_str()) {
                results.push((name.clone(), (**config).clone()));
            } else {
                missing.push(name.clone());
            }
        }

        if missing.is_empty() {
            return Ok(BatchLoadOutcome {
                loaded: results,
                failed: Vec::new(),
            });
        }

        // Step 2: Subscribe to ConfigHandles + parse OUTSIDE rcu. Accumulate
        // per-repo parse failures instead of dropping them, so the caller can
        // decide whether to fail closed.
        let mut newly_loaded: Vec<(String, RepoConfig)> = Vec::new();
        let mut failed: Vec<(String, anyhow::Error)> = Vec::new();
        for name in &missing {
            match self.load_and_parse_repo_config(name) {
                Ok(config) => newly_loaded.push((name.clone(), config)),
                Err(e) => failed.push((name.clone(), e)),
            }
        }

        // Step 3: rcu — bulk insert via a single closure that re-runs on CAS
        // failure. Safe against concurrent per-repo refreshes and other
        // get_or_load callers without needing a separate lock. Already-present
        // entries (set by a concurrent writer between Step 1 and here) win
        // — caller-side idempotency.
        if !newly_loaded.is_empty() {
            self.repo_configs.rcu(|current| {
                let mut next = (**current).clone();
                for (name, config) in &newly_loaded {
                    if !next.repos.contains_key(name.as_str()) {
                        next.insert_repo(name.clone(), config.clone());
                    }
                }
                next
            });
        }

        results.extend(newly_loaded);
        Ok(BatchLoadOutcome {
            loaded: results,
            failed,
        })
    }

    /// Batch-load repo configs. Single rcu acquisition, single HashMap clone,
    /// single ArcSwap store regardless of how many repos are loaded.
    /// This is the default path for startup (`open_managed_repos`).
    ///
    /// Thin wrapper over [`Self::batch_load_repo_configs_checked`] that
    /// warn+drops per-repo parse failures (historical behavior). Callers that
    /// need to observe the failures should use the checked variant.
    pub fn batch_load_repo_configs(
        &self,
        repo_names: &[String],
    ) -> Result<Vec<(String, RepoConfig)>> {
        let outcome = self.batch_load_repo_configs_checked(repo_names)?;
        for (name, e) in &outcome.failed {
            warn!("batch_load: failed to load config for {name}: {e:#}");
        }
        Ok(outcome.loaded)
    }

    /// Load configs for all repos discovered from both the legacy blob and
    /// the manifest. Uses batch loading (single rcu, single clone).
    pub fn load_all_repo_configs(&self) -> Result<Vec<(String, RepoConfig)>> {
        let names = self.all_repo_names();
        self.batch_load_repo_configs(&names)
    }

    /// Like [`Self::load_all_repo_configs`] but surfaces per-repo parse
    /// failures (via [`Self::batch_load_repo_configs_checked`]) instead of
    /// warn+dropping them. Seam for fail-closed startup.
    pub fn load_all_repo_configs_checked(&self) -> Result<BatchLoadOutcome> {
        let names = self.all_repo_names();
        self.batch_load_repo_configs_checked(&names)
    }

    /// Union of repo names from the legacy blob cache and the manifest.
    fn all_repo_names(&self) -> Vec<String> {
        let mut all_names: HashSet<String> = self
            .repo_configs
            .load_full()
            .repos
            .keys()
            .cloned()
            .collect();
        if let Some(manifest) = self.manifest() {
            for entry in &manifest.repos {
                all_names.insert(entry.repo_name.clone());
            }
        }
        all_names.into_iter().collect()
    }

    /// Load a repo config by repository ID. O(1) cache lookup via repos_by_id
    /// index, falls back to searching the manifest by repo_id.
    pub fn get_or_load_repo_config_by_id(&self, repo_id: i32) -> Result<(String, RepoConfig)> {
        // Fast path: O(1) lookup via repos_by_id index
        let current = self.repo_configs.load_full();
        if let Some((name, config)) = current.get_repo_config_by_raw_id(repo_id) {
            let name = name.clone();
            // See get_or_load_repo_config — S678887.
            self.ensure_repo_config_handle(&name)?;
            return Ok((name, config.clone()));
        }

        // Slow path: search manifest for repo_id.
        // If no manifest is available (e.g. integration tests, file-backed configs),
        // fall back to the original "unknown repoid" error.
        let manifest = match self.maybe_manifest_handle.as_ref() {
            Some(handle) => handle.get(),
            None => anyhow::bail!("unknown repoid: RepositoryId({repo_id})"),
        };
        let entry = manifest
            .repos
            .iter()
            .find(|e| e.repo_id == repo_id)
            .ok_or_else(|| anyhow!("unknown repoid: RepositoryId({repo_id})"))?;
        let config = self.get_or_load_repo_config(&entry.repo_name)?;
        Ok((entry.repo_name.clone(), config))
    }
}

impl Drop for MononokeConfigs {
    // If MononokeConfigs is getting dropped, then we need to terminate the updater
    // process as well.
    fn drop(&mut self) {
        // If the updater process exists, abort it.
        if let Some(updater_handle) = self.maybe_config_updater.as_ref() {
            updater_handle.abort();
        }
        // If the liveness updater process exists, abort it.
        if let Some(liveness_updater) = self.maybe_liveness_updater.as_ref() {
            liveness_updater.abort();
        }
    }
}

#[cfg(test)]
mod tests;
