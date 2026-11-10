/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! `unified_config_watcher` + its supporting helpers.
//!
//! The watcher is a single tokio task that owns three event sources:
//!
//! 1. **Config source** [`ConfigSource`] — fires on `TierManifest` changes
//!    (repo add/remove, sharding mode flips)
//! 2. **Per-repo control channel** `mpsc::UnboundedReceiver<RepoHandleEvent>` —
//!    notifies the loop when a new per-repo `ConfigHandle<RepoSpec>` is installed
//!    by `load_repo_config_handle` / `ensure_repo_config_handle` (ShardManager
//!    on_add_shard, startup batch loading)
//! 3. **Per-repo wait fan-in** `FuturesUnordered<wait_one>` — one in-flight
//!    future per per-repo watcher; fires when a repo's RepoSpec content changes
//!
//! All three arms feed a single `tokio::select!` so config-application work
//! serializes within one task.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use cached_config::ConfigHandle;
use cached_config::ConfigStore;
use cached_config::ConfigUpdateWatcher;
use futures::future::join_all;
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use metaconfig_parser::RepoConfigs;
use metaconfig_parser::StorageConfigs;
use metaconfig_parser::configerator_repo_spec_handle;
use metaconfig_parser::parse_manifest_common_and_storage;
use metaconfig_parser::parse_repo_spec;
use metaconfig_types::CommonConfig;
use metaconfig_types::RepoConfig;
use repos::RepoSpec;
use repos::TierManifest;
use stats::prelude::*;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::STATS;
use crate::Swappable;
use crate::receiver::ConfigUpdateReceiver;

const LIVENESS_INTERVAL: Duration = Duration::from_secs(300);

/// Kill switch for reusing unchanged `RepoSpec` parses in the bulk manifest pass.
const INCREMENTAL_RELOAD_KNOB: &str = "scm/mononoke:incremental_manifest_reload";

/// Result of awaiting one per-repo watcher fire. Owns the watcher so the
/// caller can re-push a fresh wait future for it without re-subscribing.
type PerRepoWaitResult = (String, Result<Arc<RepoSpec>>, ConfigUpdateWatcher<RepoSpec>);

/// Boxed `wait_one` future. Trait-object form because `FuturesUnordered`
/// can't be generic over a concrete async-fn type.
type PerRepoFuture = Pin<Box<dyn std::future::Future<Output = PerRepoWaitResult> + Send>>;

/// Notification sent to `unified_config_watcher` when a new per-repo handle
/// is registered. Removal is implicit (the watcher's `wait_for_next` returns
/// `Err` when the handle is dropped — see comment on `handle_per_repo_fire`).
pub(crate) enum RepoHandleEvent {
    Added(String, ConfigUpdateWatcher<RepoSpec>),
}

/// Where the watcher sources bulk config from: the tier manifest is
/// authoritative; `tier` resolves `tier_overrides`.
pub(crate) struct ConfigSource {
    pub(crate) handle: ConfigHandle<TierManifest>,
    pub(crate) tier: String,
}

/// Background task that periodically bumps the `liveness_count` stat so
/// monitoring can detect a hung config-update task. Spawned alongside
/// `unified_config_watcher` in `MononokeConfigs::new`.
pub(crate) async fn liveness_updater() {
    loop {
        STATS::liveness_count.add_value(1);
        tokio::time::sleep(LIVENESS_INTERVAL).await;
    }
}

/// Spurious-reload dedup; `None` (nothing applied yet) counts as changed.
fn content_changed<T: PartialEq>(prev: &Option<Arc<T>>, current: &Arc<T>) -> bool {
    match prev {
        Some(p) => **p != **current,
        None => true,
    }
}

/// Per-repo analogue of [`content_changed`]: did the `RepoSpec` change from the
/// last-applied content? `None` (nothing recorded yet) counts as changed.
fn spec_content_changed(prev: Option<&Arc<RepoSpec>>, current: &RepoSpec) -> bool {
    match prev {
        Some(p) => **p != *current,
        None => true,
    }
}

/// Awaits the next event from the per-repo control channel. If split-loading
/// is disabled (rx is None) this parks forever, mirroring `wait_for_handle`.
async fn wait_for_event(
    rx: &mut Option<mpsc::UnboundedReceiver<RepoHandleEvent>>,
) -> Option<RepoHandleEvent> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Awaits the next item from a stream, parking forever when the stream is
/// empty or terminated. Keeps the per-repo `FuturesUnordered` arm of the
/// `select!` valid before any per-repo watchers have been registered.
async fn next_or_pending<S>(stream: &mut S) -> S::Item
where
    S: futures::Stream + Unpin,
{
    match stream.next().await {
        Some(item) => item,
        None => std::future::pending().await,
    }
}

/// One per-repo wait. Takes ownership of the watcher and returns it alongside
/// the wait result so the caller can re-push the next wait into a
/// `FuturesUnordered` without re-creating the underlying subscription.
async fn wait_one(
    repo_name: String,
    mut watcher: ConfigUpdateWatcher<RepoSpec>,
) -> PerRepoWaitResult {
    let result = watcher.wait_for_next().await;
    (repo_name, result, watcher)
}

/// Register a per-repo watcher, seeding `prev_specs` with the repo's current
/// content so even a one-shot spurious version bump is deduped on its first fire.
fn push_per_repo_watcher(
    name: String,
    watcher: ConfigUpdateWatcher<RepoSpec>,
    repo_handles: &RwLock<HashMap<String, ConfigHandle<RepoSpec>>>,
    prev_specs: &mut HashMap<String, Arc<RepoSpec>>,
    per_repo_wait_futures: &mut FuturesUnordered<PerRepoFuture>,
) {
    match repo_handles.read() {
        Ok(handles) => {
            if let Some(handle) = handles.get(&name) {
                prev_specs.insert(name.clone(), handle.get());
            }
        }
        Err(e) => error!("repo_handles lock poisoned seeding prev_spec for {name}: {e}"),
    }
    per_repo_wait_futures.push(Box::pin(wait_one(name, watcher)));
}

/// Free function (not an inline async block) so the compiler infers a concrete
/// future type. Required to avoid an "implementation of FnOnce is not general
/// enough" HRTB error when used inside a `FuturesUnordered` over a
/// `Vec<Arc<dyn ConfigUpdateReceiver>>` whose `dyn Trait` lifetime variance
/// trips the closure-bound inference inside a spawned `'static` task.
async fn dispatch_apply_repo_update(
    receiver: Arc<dyn ConfigUpdateReceiver>,
    repo_name: String,
    repo_config: RepoConfig,
) -> Result<()> {
    receiver.apply_repo_update(&repo_name, &repo_config).await
}

/// Applies a per-repo config update atomically against the bulk `RepoConfigs`
/// Arc and the receiver-side state.
///
/// **Ordering matters**: the bulk Arc is patched FIRST so receivers that read
/// `MononokeConfigs::repo_configs()` during `apply_repo_update` see the new
/// state for `repo_name`. The trait comment on
/// `ConfigUpdateReceiver::apply_repo_update` documents this ordering invariant
/// ("the caller must have already swapped in the new config").
///
/// Returns `true` iff every receiver's `apply_repo_update` succeeded. The
/// caller increments the per-refresh success/failure stats so per-refresh
/// counts stay mutually exclusive even when N receivers fail on one refresh.
async fn apply_per_repo_update(
    repo_name: &str,
    new_config: RepoConfig,
    repo_configs: &Swappable<RepoConfigs>,
    update_receivers: &Swappable<Vec<Arc<dyn ConfigUpdateReceiver>>>,
) -> bool {
    // (a) Patch the bulk RepoConfigs Arc via rcu. The closure runs at least
    // once and re-runs on any concurrent writer's CAS failure, so this is
    // safe against the other rcu writers (`get_or_load_repo_config`,
    // `batch_load_repo_configs`) without any lock.
    repo_configs.rcu(|current| {
        let mut next = (**current).clone();
        // Via insert_repo to keep repos_by_id consistent (and Arc-wrap).
        next.insert_repo(repo_name.to_owned(), new_config.clone());
        next
    });

    // (b) Call apply_repo_update on each receiver concurrently via
    // FuturesUnordered. In practice there are typically 1-2 receivers; the
    // unbounded fan-out is safe because the receiver count is governed by
    // `register_for_update` call sites in each service binary (not by repo
    // count or request volume).
    // Snapshot receivers to an owned Vec so each future owns its Arc rather
    // than borrowing into the unified_config_watcher task's frame.
    let mut futs: FuturesUnordered<_> = update_receivers
        .load()
        .iter()
        .cloned()
        .map(|r| dispatch_apply_repo_update(r, repo_name.to_owned(), new_config.clone()))
        .collect();
    let mut had_failure = false;
    while let Some(result) = futs.next().await {
        if let Err(e) = result {
            error!("apply_repo_update for repo {repo_name} failed on a receiver: {e:?}");
            had_failure = true;
        }
    }
    !had_failure
}

/// Unified config watcher: monitors the tier manifest and a dynamic set of
/// per-repo `ConfigHandle<RepoSpec>` watchers via `tokio::select!`, applying
/// changes exactly once.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn unified_config_watcher(
    config_source: ConfigSource,
    repo_handles: Arc<RwLock<HashMap<String, ConfigHandle<RepoSpec>>>>,
    config_store: ConfigStore,
    repo_configs: Swappable<RepoConfigs>,
    storage_configs: Swappable<StorageConfigs>,
    update_receivers: Swappable<Vec<Arc<dyn ConfigUpdateReceiver>>>,
    mut repo_handle_event_rx: Option<mpsc::UnboundedReceiver<RepoHandleEvent>>,
) {
    let mut manifest_watcher = match config_source.handle.watcher() {
        Ok(watcher) => watcher,
        Err(e) => {
            error!("Failed to create manifest watcher, unified_config_watcher exiting: {e:?}");
            return;
        }
    };

    let mut state = ReloadState::default();

    // Last-applied RepoSpec per repo; lets the per-repo arm skip spurious
    // identical-content reloads. Seeded at registration.
    let mut prev_specs: HashMap<String, Arc<RepoSpec>> = HashMap::new();

    // Per-repo watcher set, fed by the control-channel arm and sync_repo_handles.
    let mut per_repo_wait_futures: FuturesUnordered<PerRepoFuture> = FuturesUnordered::new();

    // Bootstrap: seeds `prev_manifest` (per-repo fires defer until set) + subscribes manifest repos.
    run_reload_pass(
        &config_source,
        &repo_handles,
        &config_store,
        &repo_configs,
        &storage_configs,
        &update_receivers,
        &mut state,
        &mut prev_specs,
        &mut per_repo_wait_futures,
    )
    .await;

    loop {
        tokio::select! {
            result = manifest_watcher.wait_for_next() => {
                if let Err(e) = result {
                    error!("Error waiting for manifest config update: {e:?}");
                    continue;
                }
            }
            event = wait_for_event(&mut repo_handle_event_rx) => {
                match event {
                    Some(RepoHandleEvent::Added(name, watcher)) => {
                        debug!("Registering per-repo watcher for {name}");
                        push_per_repo_watcher(
                            name,
                            watcher,
                            &repo_handles,
                            &mut prev_specs,
                            &mut per_repo_wait_futures,
                        );
                    }
                    None => {
                        // Sender side dropped — disable this arm so wait_for_event parks.
                        repo_handle_event_rx = None;
                    }
                }
                continue;
            }
            (name, result, watcher) = next_or_pending(&mut per_repo_wait_futures) => {
                handle_per_repo_fire(
                    name,
                    result,
                    watcher,
                    &repo_handles,
                    Some(&config_source.tier),
                    state.prev_manifest.as_deref(),
                    &repo_configs,
                    &update_receivers,
                    &mut per_repo_wait_futures,
                    &mut prev_specs,
                ).await;
                continue;
            }
        }

        run_reload_pass(
            &config_source,
            &repo_handles,
            &config_store,
            &repo_configs,
            &storage_configs,
            &update_receivers,
            &mut state,
            &mut prev_specs,
            &mut per_repo_wait_futures,
        )
        .await;
    }
}

/// State one reload pass hands to the next; extracted so `run_reload_pass` is unit-testable.
#[derive(Default)]
struct ReloadState {
    /// Last applied manifest; advanced only by a fully successful pass.
    prev_manifest: Option<Arc<TierManifest>>,
    /// Reused on equal re-parse to keep pointer identity stable.
    cached_manifest_storage: Option<Arc<StorageConfigs>>,
    /// Parse results from the last committed manifest pass, valid against `prev_manifest.storage`.
    parsed_specs: HashMap<String, ParsedSpec>,
}

/// A `RepoSpec` and the `RepoConfig` it parsed into, so an unchanged spec skips the re-parse.
#[derive(Clone)]
struct ParsedSpec {
    spec: Arc<RepoSpec>,
    config: Arc<RepoConfig>,
}

/// One reload pass; everything fallible returns before the commit.
#[expect(
    clippy::too_many_arguments,
    reason = "pass body extracted from the watcher loop for testability; \
              the arguments are the loop's captured environment"
)]
async fn run_reload_pass(
    config_source: &ConfigSource,
    repo_handles: &RwLock<HashMap<String, ConfigHandle<RepoSpec>>>,
    config_store: &ConfigStore,
    repo_configs: &Swappable<RepoConfigs>,
    storage_configs: &Swappable<StorageConfigs>,
    update_receivers: &Swappable<Vec<Arc<dyn ConfigUpdateReceiver>>>,
    state: &mut ReloadState,
    prev_specs: &mut HashMap<String, Arc<RepoSpec>>,
    per_repo_wait_futures: &mut FuturesUnordered<PerRepoFuture>,
) {
    run_manifest_reload_pass(
        &config_source.handle,
        &config_source.tier,
        repo_handles,
        config_store,
        repo_configs,
        storage_configs,
        update_receivers,
        state,
        prev_specs,
        per_repo_wait_futures,
    )
    .await
}

/// Manifest-authoritative pass: a parse failure aborts before any state
/// mutation (keep-last-known-good; the retry survives the content dedup).
#[expect(
    clippy::too_many_arguments,
    reason = "pass body extracted from the watcher loop for testability; \
              the arguments are the loop's captured environment"
)]
async fn run_manifest_reload_pass(
    manifest_handle: &ConfigHandle<TierManifest>,
    tier: &str,
    repo_handles: &RwLock<HashMap<String, ConfigHandle<RepoSpec>>>,
    config_store: &ConfigStore,
    repo_configs: &Swappable<RepoConfigs>,
    storage_configs: &Swappable<StorageConfigs>,
    update_receivers: &Swappable<Vec<Arc<dyn ConfigUpdateReceiver>>>,
    state: &mut ReloadState,
    prev_specs: &mut HashMap<String, Arc<RepoSpec>>,
    per_repo_wait_futures: &mut FuturesUnordered<PerRepoFuture>,
) {
    let current_manifest = manifest_handle.get();
    if !content_changed(&state.prev_manifest, &current_manifest) {
        STATS::spurious_reload_suppressed.add_value(1);
        debug!("Config version bumped but content identical, skipping reload");
        return;
    }

    // The first pass only seeds state; a real change is worth an info line.
    let bootstrap = state.prev_manifest.is_none();
    if bootstrap {
        debug!("Seeding manifest state");
    } else {
        info!("Manifest config content changed, applying update");
    }

    // Parse first: fail closed before any state mutation.
    let (common, storage) = match parse_manifest_common_and_storage(&current_manifest) {
        Ok(parsed) => parsed,
        Err(e) => {
            error!(
                "Failed to parse common/storage from tier manifest, \
                 keeping last known good config: {e:?}"
            );
            STATS::manifest_common_parse_failure.add_value(1);
            STATS::refresh_failure_count.add_value(1);
            return;
        }
    };

    match sync_repo_handles(&current_manifest, repo_handles, config_store, bootstrap) {
        Ok(new_watchers) => {
            // Register new watchers so per-repo changes propagate without a bulk reload.
            for (name, watcher) in new_watchers {
                push_per_repo_watcher(
                    name,
                    watcher,
                    repo_handles,
                    prev_specs,
                    per_repo_wait_futures,
                );
            }
        }
        Err(e) => {
            // Don't advance prev_manifest: transient failures retry on the next fire.
            error!("Failed to sync repo handles: {e:?}");
            STATS::refresh_failure_count.add_value(1);
            return;
        }
    }

    // Reuse the previous Arc on equal content to keep pointer identity stable.
    let storage = match state.cached_manifest_storage.take() {
        Some(cached) if *cached == storage => cached,
        _ => Arc::new(storage),
    };
    state.cached_manifest_storage = Some(storage.clone());

    // Rebuilt every pass so repos that left the manifest or failed to parse drop out.
    let mut parsed_specs = HashMap::with_capacity(current_manifest.repos.len());

    // Scoped so the repo_handles read guard is dropped before any await below.
    let merged = {
        let handles = match repo_handles.read() {
            Ok(h) => h,
            Err(e) => {
                error!("Failed to read repo handles lock: {e:?}");
                STATS::refresh_failure_count.add_value(1);
                return;
            }
        };
        // Served snapshot for per-repo keep-last-known-good below.
        let served = repo_configs.load_full();
        // Every parse depends on the manifest's storage, so a storage change invalidates them all.
        let previous_parses = (justknobs::eval(INCREMENTAL_RELOAD_KNOB, None, None)
            && state
                .prev_manifest
                .as_ref()
                .is_some_and(|prev| prev.storage == current_manifest.storage))
        .then_some(&state.parsed_specs);
        let mut repos = RepoConfigs::new(HashMap::new(), CommonConfig::default()).repos;
        for entry in &current_manifest.repos {
            if let Some(handle) = handles.get(&entry.repo_name) {
                let (spec, version_info) = handle.get_with_version();
                let previous = previous_parses.and_then(|parses| parses.get(&entry.repo_name));
                // Reuse only a parse that is still the one served: the per-repo path and on-demand
                // loads replace served configs without going through this cache.
                let reusable = previous.filter(|parsed| {
                    (Arc::ptr_eq(&parsed.spec, &spec) || *parsed.spec == *spec)
                        && served
                            .repos
                            .get(&entry.repo_name)
                            .is_some_and(|config| Arc::ptr_eq(config, &parsed.config))
                });
                if let Some(parsed) = reusable {
                    STATS::bulk_reload_spec_reused.add_value(1);
                    repos.insert(entry.repo_name.clone(), parsed.config.clone());
                    // Hold the live Arc, not the old one, so a no-op version bump does not pin a duplicate.
                    parsed_specs.insert(
                        entry.repo_name.clone(),
                        ParsedSpec {
                            spec,
                            config: parsed.config.clone(),
                        },
                    );
                    continue;
                }
                STATS::bulk_reload_spec_parsed.add_value(1);
                match parse_repo_spec(RepoSpec::clone(&spec), tier, &current_manifest.storage) {
                    Ok(mut config) => {
                        config.config_version = version_info.map(|info| info.version);
                        let config = Arc::new(config);
                        repos.insert(entry.repo_name.clone(), config.clone());
                        parsed_specs.insert(entry.repo_name.clone(), ParsedSpec { spec, config });
                    }
                    Err(e) => {
                        error!(
                            "Failed to parse RepoSpec for repo '{}', keeping last \
                             served config if any: {e:?}",
                            entry.repo_name,
                        );
                        STATS::per_repo_refresh_failure_count.add_value(1);
                        // Keep-last-known-good: never drop a served repo on a transient failure.
                        if let Some(served_config) = served.repos.get(&entry.repo_name) {
                            repos.insert(entry.repo_name.clone(), served_config.clone());
                        }
                        // Carry the last good parse so a spec fixed back to it is not re-parsed.
                        if let Some(parsed) = previous {
                            parsed_specs.insert(entry.repo_name.clone(), parsed.clone());
                        }
                    }
                }
            } else {
                STATS::merge_skipped_no_handle.add_value(1);
            }
        }
        // from_arc_map rebuilds repos_by_id from the merged map.
        RepoConfigs::from_arc_map(repos, common)
    };

    // Commit only now: everything above can `return`.
    // rcu overlay, not a blind store: keep repos rcu-inserted by concurrent
    // loads after the merge snapshot; non-members still drop.
    let manifest_names: HashSet<&str> = current_manifest
        .repos
        .iter()
        .map(|e| e.repo_name.as_str())
        .collect();
    repo_configs.rcu(|current| {
        let mut repos = merged.repos.clone();
        for (name, config) in current.repos.iter() {
            if manifest_names.contains(name.as_str()) && !repos.contains_key(name) {
                repos.insert(name.clone(), config.clone());
            }
        }
        RepoConfigs::from_arc_map(repos, merged.common.clone())
    });
    state.prev_manifest = Some(current_manifest);
    state.parsed_specs = parsed_specs;
    storage_configs.store(storage);
    notify_receivers(
        repo_configs.load_full(),
        storage_configs,
        update_receivers,
        bootstrap,
    )
    .await;
}

/// Fan the committed bulk update out to receivers and bump the refresh stats.
async fn notify_receivers(
    new_configs: Arc<RepoConfigs>,
    storage_configs: &Swappable<StorageConfigs>,
    update_receivers: &Swappable<Vec<Arc<dyn ConfigUpdateReceiver>>>,
    bootstrap: bool,
) {
    let current_storage = storage_configs.load_full();
    let receivers = update_receivers.load();
    let results = join_all(
        receivers
            .iter()
            .map(|r| r.apply_update(new_configs.clone(), current_storage.clone())),
    )
    .await;
    let had_failure = results.iter().any(|r| r.is_err());
    for (i, result) in results.iter().enumerate() {
        if let Err(e) = result {
            error!("Config update receiver {i} failed: {e:?}");
        }
    }
    if had_failure {
        STATS::refresh_failure_count.add_value(1);
    } else {
        if bootstrap {
            debug!("Seeded config state");
        } else {
            info!("Successfully applied config update");
        }
        STATS::refresh_success_count.add_value(1);
        // Keep the timeseries alive for OneDetection alerting
        STATS::refresh_failure_count.add_value(0);
    }
}

/// Body of the per-repo `select!` arm — extracted so the main loop reads as
/// straight-line orchestration rather than nested control flow.
///
/// Pure with respect to global state (everything is passed by argument) so
/// the per-repo dispatch logic can be unit-tested directly without spinning
/// up the watcher's full `select!` loop. See `tests::handle_per_repo_fire_*`.
///
/// `Err` = the watch Sender dropped (repo removed, or a duplicate handle lost
/// an insert race): the watcher is dead, don't re-push. Never transient.
#[allow(clippy::too_many_arguments)]
async fn handle_per_repo_fire(
    name: String,
    result: Result<Arc<RepoSpec>>,
    watcher: ConfigUpdateWatcher<RepoSpec>,
    repo_handles: &RwLock<HashMap<String, ConfigHandle<RepoSpec>>>,
    tier_name: Option<&str>,
    prev_manifest: Option<&TierManifest>,
    repo_configs: &Swappable<RepoConfigs>,
    update_receivers: &Swappable<Vec<Arc<dyn ConfigUpdateReceiver>>>,
    per_repo_wait_futures: &mut FuturesUnordered<PerRepoFuture>,
    prev_specs: &mut HashMap<String, Arc<RepoSpec>>,
) {
    // Handle removed concurrently by remove_repo_config_handle (which drops
    // the ConfigHandle, closing the watcher channel). Don't re-push.
    let handle = match repo_handles.read() {
        Ok(h) => h.get(&name).cloned(),
        Err(e) => {
            error!("repo_handles lock poisoned dispatching per-repo update for {name}: {e:?}");
            STATS::per_repo_refresh_failure_count.add_value(1);
            return;
        }
    };
    let Some(handle) = handle else {
        debug!("Per-repo watcher fired for absent repo {name}, dropping");
        prev_specs.remove(&name);
        return;
    };

    if let Err(e) = result {
        // Dead watcher; a live handle may remain (checked above), so leave its dedup seed.
        debug!("Per-repo watcher for {name} closed: {e:?}");
        return;
    }
    // One snapshot gives the spec and its version, at least as new as the fire.
    let (spec, version_info) = handle.get_with_version();

    // Skip spurious version bumps: identical RepoSpec content -> no parse/rebuild
    // (repo_factory::build re-preloads the commit graph). Raw compare.
    if !spec_content_changed(prev_specs.get(&name), &spec) {
        STATS::spurious_reload_suppressed.add_value(1);
        debug!("Per-repo config content unchanged for {name}, skipping reload");
        // Repoint at the new Arc (same content) to release the old allocation
        // and keep sharing storage with the live handle instead of pinning a dup.
        prev_specs.insert(name.clone(), spec);
        per_repo_wait_futures.push(Box::pin(wait_one(name, watcher)));
        return;
    }

    let Some(tier) = tier_name else {
        error!("Per-repo watcher fired without tier_name set (repo {name}); skipping");
        STATS::per_repo_refresh_failure_count.add_value(1);
        per_repo_wait_futures.push(Box::pin(wait_one(name, watcher)));
        return;
    };
    let Some(manifest_for_storage) = prev_manifest else {
        // Manifest watcher hasn't fired yet — we have no storage_config to use
        // when parsing. Skip; the next manifest fire will trigger a bulk reload
        // that picks up the new spec.
        debug!(
            "Per-repo watcher fired for {name} before manifest_watcher; deferring to bulk reload"
        );
        per_repo_wait_futures.push(Box::pin(wait_one(name, watcher)));
        return;
    };

    // Cheap Arc clone; parse_repo_spec consumes the original below.
    let applied_spec = spec.clone();
    let mut new_config = match parse_repo_spec(
        Arc::unwrap_or_clone(spec),
        tier,
        &manifest_for_storage.storage,
    ) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to parse RepoSpec for {name}: {e:?}");
            STATS::per_repo_refresh_failure_count.add_value(1);
            per_repo_wait_futures.push(Box::pin(wait_one(name, watcher)));
            return;
        }
    };
    // Version reflects the last content-changing parse: spurious bumps are deduped above.
    new_config.config_version = version_info.map(|info| info.version);

    info!("Per-repo config refresh: {name}");
    let succeeded = apply_per_repo_update(&name, new_config, repo_configs, update_receivers).await;
    if succeeded {
        STATS::per_repo_refresh_count.add_value(1);
        // Record applied content for dedup, but only once a receiver exists to act
        // on it. The watcher runs before receivers register at startup; advancing
        // then would dedup away the healing reload that fires once they do, leaving
        // the repo on stale config. Failures don't advance either, so they retry.
        if !update_receivers.load().is_empty() {
            prev_specs.insert(name.clone(), applied_spec);
        }
    } else {
        STATS::per_repo_refresh_failure_count.add_value(1);
    }
    // Re-push so we observe the next update for this watcher.
    per_repo_wait_futures.push(Box::pin(wait_one(name, watcher)));
}

/// Adds preload handles for new non-deep-sharded entries; removes handles
/// whose repo is gone from the manifest. The add/remove filters are
/// asymmetric on purpose: deep-sharded repos are loaded on-demand by
/// ShardManager via `load_repo_config_handle` and must survive manifest
/// refreshes.
///
/// Returns the list of `(name, watcher)` pairs that were just installed, so
/// the caller (`unified_config_watcher`) can register them with its per-repo
/// `FuturesUnordered` set. Watchers for repos that produced an `Err` from
/// `handle.watcher()` (i.e. static configs in test fixtures) log a warning
/// and are skipped — they have no live channel to observe anyway, but the
/// dataloss is observable in production via the warn log.
pub(crate) fn sync_repo_handles(
    manifest: &TierManifest,
    repo_handles: &RwLock<HashMap<String, ConfigHandle<RepoSpec>>>,
    config_store: &ConfigStore,
    bootstrap: bool,
) -> Result<Vec<(String, ConfigUpdateWatcher<RepoSpec>)>> {
    let current_repos: HashSet<String> = repo_handles
        .read()
        .map_err(|e| anyhow!("repo_handles lock poisoned: {e}"))?
        .keys()
        .cloned()
        .collect();

    let to_remove = compute_handles_to_remove(&current_repos, manifest);

    let new_handles: Vec<_> = manifest
        .repos
        .iter()
        .filter(|entry| !entry.is_deep_sharded && !current_repos.contains(&entry.repo_name))
        .filter_map(
            |entry| match configerator_repo_spec_handle(&entry.config_path, config_store) {
                Ok(handle) => {
                    if bootstrap {
                        debug!("Added config handle for repo: {}", entry.repo_name);
                    } else {
                        info!("Added config handle for new repo: {}", entry.repo_name);
                    }
                    Some((entry.repo_name.clone(), handle))
                }
                Err(e) => {
                    error!("Failed to load config for {}: {e:?}", entry.repo_name);
                    STATS::refresh_failure_count.add_value(1);
                    None
                }
            },
        )
        .collect();

    // Derive watchers BEFORE handing handle ownership to the HashMap. Log
    // any watcher() failure since it disables per-repo hot-reload for that
    // repo until the next manifest refresh re-adds it.
    let new_watchers: Vec<_> = new_handles
        .iter()
        .filter_map(|(name, handle)| match handle.watcher() {
            Ok(w) => Some((name.clone(), w)),
            Err(e) => {
                warn!(
                    "sync_repo_handles: failed to create watcher for {name}, \
                     per-repo hot-reload disabled until next manifest refresh: {e:?}",
                );
                None
            }
        })
        .collect();

    // A concurrent ensure/load insert wins; our duplicate handle+watcher drop, never registered.
    let installed_watchers = if !new_handles.is_empty() || !to_remove.is_empty() {
        let mut handles = repo_handles
            .write()
            .map_err(|e| anyhow!("repo_handles lock poisoned: {e}"))?;
        let mut installed: HashSet<String> = HashSet::new();
        for (name, handle) in new_handles {
            if let Entry::Vacant(entry) = handles.entry(name) {
                installed.insert(entry.key().clone());
                entry.insert(handle);
            }
        }
        for repo_name in &to_remove {
            handles.remove(repo_name);
            info!("Removed config handle for repo: {repo_name}");
        }
        new_watchers
            .into_iter()
            .filter(|(name, _)| installed.contains(name))
            .collect()
    } else {
        new_watchers
    };

    Ok(installed_watchers)
}

/// Names in `current_repos` no longer present in the manifest. Pure helper
/// extracted to make the diff testable without a `ConfigStore`.
fn compute_handles_to_remove(
    current_repos: &HashSet<String>,
    manifest: &TierManifest,
) -> Vec<String> {
    let manifest_repo_names: HashSet<&str> = manifest
        .repos
        .iter()
        .map(|e| e.repo_name.as_str())
        .collect();
    current_repos
        .iter()
        .filter(|name| !manifest_repo_names.contains(name.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests;
