/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::anyhow;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use mononoke_macros::mononoke;
use repos::TierRepoEntry;

use super::*;

fn tier_entry(name: &str, is_deep_sharded: bool) -> TierRepoEntry {
    TierRepoEntry {
        repo_name: name.to_owned(),
        is_deep_sharded,
        ..Default::default()
    }
}

fn manifest_with(entries: Vec<TierRepoEntry>) -> TierManifest {
    TierManifest {
        repos: entries,
        ..Default::default()
    }
}

// Regression: deep-sharded handles inserted on-demand by ShardManager
// must survive manifest refresh. See D106658358.
#[mononoke::test]
fn test_compute_handles_to_remove_preserves_deep_sharded() {
    let manifest = manifest_with(vec![
        tier_entry("non_sharded_repo", false),
        tier_entry("deep_sharded_repo", true),
    ]);
    let current: HashSet<String> = ["non_sharded_repo", "deep_sharded_repo"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let to_remove = compute_handles_to_remove(&current, &manifest);
    assert!(
        to_remove.is_empty(),
        "deep-sharded repo present in manifest must not be removed, got {to_remove:?}",
    );
}

#[mononoke::test]
fn test_compute_handles_to_remove_drops_repos_missing_from_manifest() {
    let manifest = manifest_with(vec![tier_entry("still_present", true)]);
    let current: HashSet<String> = ["still_present", "gone_from_manifest"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let to_remove = compute_handles_to_remove(&current, &manifest);
    assert_eq!(
        to_remove,
        vec!["gone_from_manifest".to_string()],
        "only entries absent from manifest should be removed",
    );
}

#[mononoke::test]
fn test_compute_handles_to_remove_empty_manifest() {
    let manifest = manifest_with(vec![]);
    let current: HashSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
    let mut to_remove = compute_handles_to_remove(&current, &manifest);
    to_remove.sort();
    assert_eq!(to_remove, vec!["a".to_string(), "b".to_string()]);
}

#[mononoke::test]
fn test_compute_handles_to_remove_empty_current() {
    let manifest = manifest_with(vec![tier_entry("a", false), tier_entry("b", true)]);
    let current: HashSet<String> = HashSet::new();
    let to_remove = compute_handles_to_remove(&current, &manifest);
    assert!(to_remove.is_empty());
}

/// Records every `apply_repo_update` call for assertion. Tracks the
/// snapshot of the bulk `RepoConfigs` Arc observed at the moment of the
/// call so tests can verify the rcu-patch-THEN-receiver ordering.
struct RecordingReceiver {
    repo_configs: Swappable<RepoConfigs>,
    calls: tokio::sync::Mutex<Vec<RecordedCall>>,
}

#[derive(Clone)]
struct RecordedCall {
    repo_name: String,
    repo_config: RepoConfig,
    bulk_arc_snapshot: Arc<RepoConfigs>,
}

#[async_trait]
impl ConfigUpdateReceiver for RecordingReceiver {
    async fn apply_update(
        &self,
        _repo_configs: Arc<RepoConfigs>,
        _storage_configs: Arc<StorageConfigs>,
    ) -> Result<()> {
        Ok(())
    }

    async fn apply_repo_update(&self, repo_name: &str, repo_config: &RepoConfig) -> Result<()> {
        self.calls.lock().await.push(RecordedCall {
            repo_name: repo_name.to_owned(),
            repo_config: repo_config.clone(),
            bulk_arc_snapshot: self.repo_configs.load_full(),
        });
        Ok(())
    }
}

fn empty_repo_configs() -> Arc<ArcSwap<RepoConfigs>> {
    Arc::new(ArcSwap::from_pointee(RepoConfigs::new(
        HashMap::new(),
        CommonConfig::default(),
    )))
}

fn repo_config_with_id(id: i32) -> RepoConfig {
    RepoConfig {
        repoid: mononoke_types::RepositoryId::new(id),
        ..Default::default()
    }
}

// Verifies (b): apply_repo_update is called on every registered receiver
// with the correct repo name and config.
#[mononoke::test]
async fn test_apply_per_repo_update_calls_receivers() {
    let repo_configs = empty_repo_configs();
    let receiver = Arc::new(RecordingReceiver {
        repo_configs: repo_configs.clone(),
        calls: tokio::sync::Mutex::new(Vec::new()),
    });
    let receivers: Swappable<Vec<Arc<dyn ConfigUpdateReceiver>>> =
        Arc::new(ArcSwap::from_pointee(vec![
            receiver.clone() as Arc<dyn ConfigUpdateReceiver>
        ]));

    let succeeded = apply_per_repo_update(
        "foo",
        repo_config_with_id(42),
        RepoConfigSource::default(),
        &repo_configs,
        &receivers,
    )
    .await;
    assert!(succeeded);

    let calls = receiver.calls.lock().await;
    assert_eq!(
        calls.len(),
        1,
        "exactly one apply_repo_update call expected"
    );
    assert_eq!(calls[0].repo_name, "foo");
    assert_eq!(calls[0].repo_config.repoid.id(), 42);
}

// Verifies (a): the bulk RepoConfigs Arc is patched with the new config.
#[mononoke::test]
async fn test_apply_per_repo_update_patches_bulk_arc() {
    let repo_configs = empty_repo_configs();
    let receivers: Swappable<Vec<Arc<dyn ConfigUpdateReceiver>>> =
        Arc::new(ArcSwap::from_pointee(vec![]));

    let succeeded = apply_per_repo_update(
        "foo",
        repo_config_with_id(7),
        RepoConfigSource::default(),
        &repo_configs,
        &receivers,
    )
    .await;
    assert!(succeeded);

    let after = repo_configs.load();
    let stored = after
        .repos
        .get("foo")
        .expect("foo should be in bulk Arc after per-repo apply");
    assert_eq!(stored.config.repoid.id(), 7);
}

// Regression: a repo added via apply_per_repo_update is findable by id.
#[mononoke::test]
async fn test_apply_per_repo_update_maintains_id_index() {
    let repo_configs = empty_repo_configs();
    let receivers: Swappable<Vec<Arc<dyn ConfigUpdateReceiver>>> =
        Arc::new(ArcSwap::from_pointee(vec![]));

    let succeeded = apply_per_repo_update(
        "foo",
        repo_config_with_id(7),
        RepoConfigSource::default(),
        &repo_configs,
        &receivers,
    )
    .await;
    assert!(succeeded);

    let after = repo_configs.load();
    assert!(after.repos.contains_key("foo"), "foo missing by name");
    let (name, config) = after
        .get_repo_config_by_raw_id(7)
        .expect("foo must be findable by raw id after per-repo apply");
    assert_eq!(name, "foo");
    assert_eq!(config.repoid.id(), 7);
}

// Verifies the ordering invariant: the bulk Arc is patched BEFORE the
// receiver is notified.
#[mononoke::test]
async fn test_apply_per_repo_update_arc_patched_before_receiver_called() {
    let repo_configs = empty_repo_configs();
    let receiver = Arc::new(RecordingReceiver {
        repo_configs: repo_configs.clone(),
        calls: tokio::sync::Mutex::new(Vec::new()),
    });
    let receivers: Swappable<Vec<Arc<dyn ConfigUpdateReceiver>>> =
        Arc::new(ArcSwap::from_pointee(vec![
            receiver.clone() as Arc<dyn ConfigUpdateReceiver>
        ]));

    let succeeded = apply_per_repo_update(
        "foo",
        repo_config_with_id(99),
        RepoConfigSource::default(),
        &repo_configs,
        &receivers,
    )
    .await;
    assert!(succeeded);

    let calls = receiver.calls.lock().await;
    let snapshot = &calls[0].bulk_arc_snapshot;
    let in_snapshot = snapshot
        .repos
        .get("foo")
        .expect("bulk Arc must contain new config BEFORE receiver is called");
    assert_eq!(
        in_snapshot.config.repoid.id(),
        99,
        "receiver must observe the new config in the bulk Arc",
    );
}

// Verifies update_receivers fan-out: every registered receiver sees the call.
#[mononoke::test]
async fn test_apply_per_repo_update_fans_out_to_all_receivers() {
    let repo_configs = empty_repo_configs();
    let r1 = Arc::new(RecordingReceiver {
        repo_configs: repo_configs.clone(),
        calls: tokio::sync::Mutex::new(Vec::new()),
    });
    let r2 = Arc::new(RecordingReceiver {
        repo_configs: repo_configs.clone(),
        calls: tokio::sync::Mutex::new(Vec::new()),
    });
    let receivers: Swappable<Vec<Arc<dyn ConfigUpdateReceiver>>> =
        Arc::new(ArcSwap::from_pointee(vec![
            r1.clone() as Arc<dyn ConfigUpdateReceiver>,
            r2.clone() as Arc<dyn ConfigUpdateReceiver>,
        ]));

    let succeeded = apply_per_repo_update(
        "bar",
        repo_config_with_id(11),
        RepoConfigSource::default(),
        &repo_configs,
        &receivers,
    )
    .await;
    assert!(succeeded);

    assert_eq!(r1.calls.lock().await.len(), 1, "first receiver called");
    assert_eq!(r2.calls.lock().await.len(), 1, "second receiver called");
}

// A receiver that errors out must not block other receivers from being
// notified — and the overall return must reflect failure.
struct FailingReceiver;

#[async_trait]
impl ConfigUpdateReceiver for FailingReceiver {
    async fn apply_update(
        &self,
        _repo_configs: Arc<RepoConfigs>,
        _storage_configs: Arc<StorageConfigs>,
    ) -> Result<()> {
        Ok(())
    }

    async fn apply_repo_update(&self, _repo_name: &str, _repo_config: &RepoConfig) -> Result<()> {
        Err(anyhow!("simulated receiver failure"))
    }
}

#[mononoke::test]
async fn test_apply_per_repo_update_receiver_error_does_not_block_others() {
    let repo_configs = empty_repo_configs();
    let healthy = Arc::new(RecordingReceiver {
        repo_configs: repo_configs.clone(),
        calls: tokio::sync::Mutex::new(Vec::new()),
    });
    let failing = Arc::new(FailingReceiver);
    let receivers: Swappable<Vec<Arc<dyn ConfigUpdateReceiver>>> =
        Arc::new(ArcSwap::from_pointee(vec![
            failing.clone() as Arc<dyn ConfigUpdateReceiver>,
            healthy.clone() as Arc<dyn ConfigUpdateReceiver>,
        ]));

    let succeeded = apply_per_repo_update(
        "baz",
        repo_config_with_id(3),
        RepoConfigSource::default(),
        &repo_configs,
        &receivers,
    )
    .await;
    assert!(!succeeded, "must return false when any receiver fails");

    assert_eq!(
        healthy.calls.lock().await.len(),
        1,
        "healthy receiver must still be called after failing receiver errors",
    );
    // And the bulk Arc must still be patched.
    let stored = repo_configs.load();
    assert!(stored.repos.contains_key("baz"));
}

// -------------------------------------------------------------------------
// handle_per_repo_fire orchestration tests
//
// These cover the per-repo arm orchestration in isolation: missing-handle,
// missing-tier, missing-manifest, and the re-push contract.
//
// We can't construct real ConfigUpdateWatcher<RepoSpec> values in tests (no
// public constructor), so these tests cover paths where `result: Err`
// means we don't even reach the parse step — `wait_for_next` produced an
// Err, the path early-returns without touching the watcher again. That
// catches the still-present, tier-missing, manifest-missing, and parse-
// failure paths without needing a watcher mock.
// -------------------------------------------------------------------------

fn make_repo_handles(names: &[&str]) -> Arc<RwLock<HashMap<String, ConfigHandle<RepoSpec>>>> {
    let map: HashMap<String, ConfigHandle<RepoSpec>> = names
        .iter()
        .map(|n| (n.to_string(), make_static_handle()))
        .collect();
    Arc::new(RwLock::new(map))
}

/// Static handle that can never produce a watcher. Used to populate
/// `repo_handles` so the `still_present` check finds the entry but the
/// test never needs to inject a real watcher fire.
fn make_static_handle() -> ConfigHandle<RepoSpec> {
    ConfigHandle::from_json("{}").expect("RepoSpec::default serializes as {}")
}

// Orchestration tests for handle_per_repo_fire. We can't construct
// ConfigUpdateWatcher<RepoSpec> from a static handle (`watcher()` returns
// Err for `from_json`-built handles), so we spin up a ConfigStore +
// TestSource and register a real handle for a dummy path to obtain a
// live watcher value for the test fixtures.
fn fresh_watcher() -> ConfigUpdateWatcher<RepoSpec> {
    let source = cached_config::TestSource::new();
    source.insert_config(
        "test/path",
        "{}",
        cached_config::ModificationTime::UnixTimestamp(0),
    );
    let store = cached_config::ConfigStore::new(Arc::new(source), Duration::from_secs(1), None);
    store
        .get_config_handle::<RepoSpec>("test/path".to_string())
        .expect("handle for inserted path")
        .watcher()
        .expect("registered handle has a watcher")
}

fn empty_receivers() -> Swappable<Vec<Arc<dyn ConfigUpdateReceiver>>> {
    Arc::new(ArcSwap::from_pointee(vec![]))
}

// still_present=false → handle was removed between fire and dispatch
// → drop the watcher, do NOT re-push.
#[mononoke::test]
async fn test_handle_per_repo_fire_drops_removed_repo() {
    let handles = make_repo_handles(&[]); // empty: "removed_repo" not present
    let configs = empty_repo_configs();
    let receivers = empty_receivers();
    let mut futs: FuturesUnordered<PerRepoFuture> = FuturesUnordered::new();
    let mut prev_specs: HashMap<String, Arc<RepoSpec>> = HashMap::new();

    handle_per_repo_fire(
        "removed_repo".to_string(),
        Ok(Arc::new(RepoSpec::default())),
        fresh_watcher(),
        &handles,
        Some("test_tier"),
        None,
        &configs,
        &receivers,
        &mut futs,
        &mut prev_specs,
    )
    .await;

    assert!(
        futs.is_empty(),
        "must not re-push for an absent repo (would leak the watcher subscription)",
    );
    assert!(
        !configs.load().repos.contains_key("removed_repo"),
        "bulk Arc must not be patched for an absent repo",
    );
}

// result=Err (handle dropped) → drop the watcher, do NOT re-push.
#[mononoke::test]
async fn test_handle_per_repo_fire_drops_on_err_result() {
    let handles = make_repo_handles(&["foo"]); // present
    let configs = empty_repo_configs();
    let receivers = empty_receivers();
    let mut futs: FuturesUnordered<PerRepoFuture> = FuturesUnordered::new();
    let mut prev_specs: HashMap<String, Arc<RepoSpec>> = HashMap::new();

    handle_per_repo_fire(
        "foo".to_string(),
        Err(anyhow!("simulated watch channel closed")),
        fresh_watcher(),
        &handles,
        Some("test_tier"),
        None,
        &configs,
        &receivers,
        &mut futs,
        &mut prev_specs,
    )
    .await;

    assert!(
        futs.is_empty(),
        "Err result must not re-push (sender gone, no future updates possible)",
    );
    assert!(
        !configs.load().repos.contains_key("foo"),
        "bulk Arc must not be patched on Err result",
    );
}

// tier_name=None → log+skip but re-push so watching continues in case
// tier_name appears later.
#[mononoke::test]
async fn test_handle_per_repo_fire_repushes_when_tier_missing() {
    let handles = make_repo_handles(&["foo"]);
    let configs = empty_repo_configs();
    let receivers = empty_receivers();
    let mut futs: FuturesUnordered<PerRepoFuture> = FuturesUnordered::new();
    let mut prev_specs: HashMap<String, Arc<RepoSpec>> = HashMap::new();

    handle_per_repo_fire(
        "foo".to_string(),
        Ok(Arc::new(RepoSpec::default())),
        fresh_watcher(),
        &handles,
        None, // tier_name missing
        None,
        &configs,
        &receivers,
        &mut futs,
        &mut prev_specs,
    )
    .await;

    assert_eq!(
        futs.len(),
        1,
        "must re-push watcher when tier_name is missing so future fires are observed",
    );
}

// prev_manifest=None → log+skip but re-push (next manifest fire bulk-reloads anyway).
#[mononoke::test]
async fn test_handle_per_repo_fire_repushes_when_manifest_missing() {
    let handles = make_repo_handles(&["foo"]);
    let configs = empty_repo_configs();
    let receivers = empty_receivers();
    let mut futs: FuturesUnordered<PerRepoFuture> = FuturesUnordered::new();
    let mut prev_specs: HashMap<String, Arc<RepoSpec>> = HashMap::new();

    handle_per_repo_fire(
        "foo".to_string(),
        Ok(Arc::new(RepoSpec::default())),
        fresh_watcher(),
        &handles,
        Some("test_tier"),
        None, // manifest missing
        &configs,
        &receivers,
        &mut futs,
        &mut prev_specs,
    )
    .await;

    assert_eq!(
        futs.len(),
        1,
        "must re-push watcher when manifest is missing so future fires are observed",
    );
}

// None -> changed (first fire applies); identical -> unchanged; differing -> changed.
#[mononoke::test]
fn test_spec_content_changed() {
    let prev = Arc::new(RepoSpec::default());
    let identical = RepoSpec::default();
    let different = RepoSpec {
        repo_id: 42,
        ..Default::default()
    };

    assert!(
        spec_content_changed(None, &identical),
        "no recorded spec must be treated as changed so the first fire applies",
    );
    assert!(
        !spec_content_changed(Some(&prev), &identical),
        "identical RepoSpec content is a spurious version bump, not a change",
    );
    assert!(
        spec_content_changed(Some(&prev), &different),
        "a differing RepoSpec must be treated as changed",
    );
}

// Identical content -> no apply (no receiver call, bulk untouched), watcher
// still re-pushed. tier+manifest present so only the dedup can short-circuit.
#[mononoke::test]
async fn test_handle_per_repo_fire_skips_unchanged_spec() {
    let handles = make_repo_handles(&["foo"]);
    let configs = empty_repo_configs();
    let receiver = Arc::new(RecordingReceiver {
        repo_configs: configs.clone(),
        calls: tokio::sync::Mutex::new(Vec::new()),
    });
    let receivers: Swappable<Vec<Arc<dyn ConfigUpdateReceiver>>> =
        Arc::new(ArcSwap::from_pointee(vec![
            receiver.clone() as Arc<dyn ConfigUpdateReceiver>
        ]));
    let mut futs: FuturesUnordered<PerRepoFuture> = FuturesUnordered::new();

    let spec = Arc::new(RepoSpec::default());
    let mut prev_specs: HashMap<String, Arc<RepoSpec>> = HashMap::new();
    prev_specs.insert("foo".to_string(), spec.clone());
    let manifest = manifest_with(vec![tier_entry("foo", false)]);

    handle_per_repo_fire(
        "foo".to_string(),
        Ok(spec.clone()), // identical content to the seeded prev_spec
        fresh_watcher(),
        &handles,
        Some("test_tier"),
        Some(&manifest),
        &configs,
        &receivers,
        &mut futs,
        &mut prev_specs,
    )
    .await;

    assert_eq!(
        receiver.calls.lock().await.len(),
        0,
        "identical content must not trigger a per-repo apply/rebuild",
    );
    assert!(
        !configs.load().repos.contains_key("foo"),
        "bulk Arc must not be patched when content is unchanged",
    );
    assert_eq!(
        futs.len(),
        1,
        "watcher must be re-pushed so future real changes are still observed",
    );
}

// run_reload_pass tests: driven directly (no select! loop) so they are deterministic.

use cached_config::ModificationTime;
use cached_config::TestSource;
use futures::FutureExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;

use crate::tests::TEST_STORAGE;
use crate::tests::test_raw_common_config;
use crate::tests::test_storage_map;
use crate::tests::valid_repo_spec_json;

const MANIFEST_PATH: &str = "test/manifest";

/// Manifest entry with a RepoSpec config path; non-deep-sharded so `sync_repo_handles` subscribes it.
fn spec_entry(name: &str, repo_id: i32, config_path: &str) -> TierRepoEntry {
    TierRepoEntry {
        repo_name: name.to_owned(),
        repo_id,
        config_path: config_path.to_owned(),
        is_deep_sharded: false,
        ..Default::default()
    }
}

fn good_manifest(trusted_tier: &str, repos: Vec<TierRepoEntry>) -> TierManifest {
    let manifest = TierManifest {
        common: test_raw_common_config(trusted_tier),
        storage: test_storage_map(),
        repos,
        ..Default::default()
    };
    assert!(
        parse_manifest_common_and_storage(&manifest).is_ok(),
        "fixture must parse, else tests pass via the failure branch"
    );
    manifest
}

/// A common config that fails conversion, so common/storage parsing fails.
fn unparsable_manifest(repos: Vec<TierRepoEntry>) -> TierManifest {
    let manifest = TierManifest {
        common: crate::tests::unparsable_raw_common_config(),
        storage: test_storage_map(),
        repos,
        ..Default::default()
    };
    assert!(
        parse_manifest_common_and_storage(&manifest).is_err(),
        "fixture must NOT parse, else tests pass for the wrong reason"
    );
    manifest
}

/// Records the `common` served with every bulk `apply_update`.
struct BulkRecordingReceiver {
    commons: tokio::sync::Mutex<Vec<CommonConfig>>,
}

#[async_trait]
impl ConfigUpdateReceiver for BulkRecordingReceiver {
    async fn apply_update(
        &self,
        repo_configs: Arc<RepoConfigs>,
        _storage_configs: Arc<StorageConfigs>,
    ) -> Result<()> {
        self.commons.lock().await.push(repo_configs.common.clone());
        Ok(())
    }

    async fn apply_repo_update(&self, _repo_name: &str, _repo_config: &RepoConfig) -> Result<()> {
        Ok(())
    }
}

/// Everything one `run_reload_pass` invocation needs; the TestSource lets tests swap content between passes.
struct ReloadFixture {
    source: Arc<TestSource>,
    store: ConfigStore,
    manifest_handle: ConfigHandle<TierManifest>,
    repo_handles: Arc<RwLock<HashMap<String, ConfigHandle<RepoSpec>>>>,
    repo_configs: Swappable<RepoConfigs>,
    storage_configs: Swappable<StorageConfigs>,
    receiver: Arc<BulkRecordingReceiver>,
    receivers: Swappable<Vec<Arc<dyn ConfigUpdateReceiver>>>,
    state: ReloadState,
    prev_specs: HashMap<String, Arc<RepoSpec>>,
    futs: FuturesUnordered<PerRepoFuture>,
}

impl ReloadFixture {
    fn new(initial_manifest: &TierManifest, seeded_repos: &[&str]) -> Self {
        let source = Arc::new(TestSource::new());
        source.insert_config(
            MANIFEST_PATH,
            &serde_json::to_string(initial_manifest).expect("manifest serializes"),
            ModificationTime::UnixTimestamp(0),
        );
        source.insert_to_refresh(MANIFEST_PATH.to_string());
        // Poll interval is irrelevant: tests refresh via force_update_configs.
        let store = ConfigStore::new(source.clone(), Duration::from_secs(3600), None);
        let manifest_handle = store
            .get_config_handle::<TierManifest>(MANIFEST_PATH.to_string())
            .expect("manifest handle for inserted path");
        let receiver = Arc::new(BulkRecordingReceiver {
            commons: tokio::sync::Mutex::new(Vec::new()),
        });
        let receivers: Swappable<Vec<Arc<dyn ConfigUpdateReceiver>>> =
            Arc::new(ArcSwap::from_pointee(vec![
                receiver.clone() as Arc<dyn ConfigUpdateReceiver>
            ]));
        Self {
            source,
            store,
            manifest_handle,
            repo_handles: make_repo_handles(seeded_repos),
            repo_configs: empty_repo_configs(),
            storage_configs: Arc::new(ArcSwap::from_pointee(StorageConfigs {
                storage: HashMap::new(),
            })),
            receiver,
            receivers,
            state: ReloadState::default(),
            prev_specs: HashMap::new(),
            futs: FuturesUnordered::new(),
        }
    }

    /// Replace the manifest content and propagate it to the handle.
    fn swap_manifest(&self, manifest: &TierManifest, timestamp: u64) {
        self.source.insert_config(
            MANIFEST_PATH,
            &serde_json::to_string(manifest).expect("manifest serializes"),
            ModificationTime::UnixTimestamp(timestamp),
        );
        self.store.force_update_configs();
    }

    /// Insert or replace a per-repo RepoSpec blob and propagate it to subscribed handles.
    fn put_spec(&self, path: &str, json: &str, timestamp: u64) {
        self.source
            .insert_config(path, json, ModificationTime::UnixTimestamp(timestamp));
        self.source.insert_to_refresh(path.to_string());
        self.store.force_update_configs();
    }

    async fn run(&mut self) {
        let source = ConfigSource {
            handle: self.manifest_handle.clone(),
            tier: "test_tier".to_string(),
        };
        run_reload_pass(
            &source,
            &self.repo_handles,
            &self.store,
            &self.repo_configs,
            &self.storage_configs,
            &self.receivers,
            &mut self.state,
            &mut self.prev_specs,
            &mut self.futs,
        )
        .await;
    }

    fn served_trusted_tier(&self) -> Option<String> {
        self.repo_configs
            .load()
            .common
            .trusted_parties_hipster_tier
            .clone()
    }
}

// Manifest keep-last-known-good: a failed pass mutates nothing and must not dedup away the retry.
#[mononoke::test]
async fn test_manifest_reload_keeps_last_known_good_on_parse_failure() {
    // "foo" is deep-sharded (no RepoSpec path needed) and pre-seeded to detect an incorrect sync run.
    let v0 = good_manifest("tier_v0", vec![tier_entry("foo", true)]);
    let mut fx = ReloadFixture::new(&v0, &["foo"]);

    // Pass 1: initial good manifest applies.
    fx.run().await;
    assert_eq!(
        fx.served_trusted_tier(),
        Some("tier_v0".to_string()),
        "initial good manifest must be served"
    );
    assert!(
        fx.storage_configs.load().storage.contains_key(TEST_STORAGE),
        "initial manifest storage must be served"
    );
    assert_eq!(
        fx.state.prev_manifest.as_deref(),
        Some(&v0),
        "pass 1 must record the applied manifest"
    );

    // Pass 2: unparsable manifest that also drops "foo" (detects a sync run before the bail).
    let bad = unparsable_manifest(vec![]);
    fx.swap_manifest(&bad, 1);
    fx.run().await;

    assert_eq!(
        fx.served_trusted_tier(),
        Some("tier_v0".to_string()),
        "a manifest parse failure must keep the last known good common, \
         never a default-constructed one"
    );
    assert!(
        fx.storage_configs.load().storage.contains_key(TEST_STORAGE),
        "storage_configs must be unchanged on parse failure"
    );
    assert!(
        fx.repo_handles.read().unwrap().contains_key("foo"),
        "repo_handles must be unchanged: the failed pass must return \
         BEFORE sync_repo_handles runs"
    );
    assert_eq!(
        fx.state.prev_manifest.as_deref(),
        Some(&v0),
        "prev_manifest must not advance on parse failure, so per-repo \
         fires keep parsing against the last applied snapshot"
    );
    assert_eq!(
        fx.receiver.commons.lock().await.len(),
        1,
        "no receiver update may be pushed for a failed pass"
    );

    // Pass 3: same bad content, new version — the dedup must not swallow the retry.
    fx.swap_manifest(&bad, 2);
    fx.run().await;
    assert_eq!(
        fx.served_trusted_tier(),
        Some("tier_v0".to_string()),
        "retry of the same bad content must still keep last known good"
    );

    // Pass 4: recovery — a good manifest applies normally.
    let v2 = good_manifest("tier_v2", vec![tier_entry("foo", true)]);
    fx.swap_manifest(&v2, 3);
    fx.run().await;
    assert_eq!(
        fx.served_trusted_tier(),
        Some("tier_v2".to_string()),
        "a later good manifest must be applied (retry not deduped)"
    );
    assert_eq!(
        fx.state.prev_manifest.as_deref(),
        Some(&v2),
        "recovery must advance prev_manifest"
    );
    assert_eq!(
        fx.receiver.commons.lock().await.len(),
        2,
        "exactly the two good passes may reach receivers"
    );
}

// Commit overlay: a manifest member rcu-inserted mid-pass survives; a non-member drops.
#[mononoke::test]
async fn test_manifest_reload_preserves_concurrently_loaded_repo() {
    let v0 = good_manifest("tier_v0", vec![tier_entry("concurrent/repo", true)]);
    let mut fx = ReloadFixture::new(&v0, &[]);
    // Simulate a concurrent get_or_load landing between the handle snapshot and the commit.
    fx.repo_configs.rcu(|current| {
        let mut next = (**current).clone();
        next.insert_repo("concurrent/repo".to_string(), repo_config_with_id(1));
        next.insert_repo("gone/repo".to_string(), repo_config_with_id(2));
        next
    });

    fx.run().await;

    let served = fx.repo_configs.load();
    assert!(
        served.repos.contains_key("concurrent/repo"),
        "a concurrently loaded manifest member must survive the bulk commit"
    );
    assert!(
        !served.repos.contains_key("gone/repo"),
        "a repo absent from the manifest must be dropped by the bulk commit"
    );
}

// Bulk merge: a transiently bad RepoSpec keeps the served entry (the merge starts from an empty map).
#[mononoke::test]
async fn test_manifest_reload_retains_served_repo_on_spec_parse_failure() {
    const X_PATH: &str = "test/repos/x";
    const Y_PATH: &str = "test/repos/y";

    let v0 = good_manifest("tier_v0", vec![spec_entry("repo/x", 1, X_PATH)]);
    let mut fx = ReloadFixture::new(&v0, &[]);
    fx.put_spec(X_PATH, &valid_repo_spec_json(1, "repo/x"), 0);

    // Pass 1: X subscribes (sync_repo_handles) and parses into serving.
    fx.run().await;
    assert_eq!(
        fx.repo_configs
            .load()
            .repos
            .get("repo/x")
            .expect("repo/x must be served after the initial good pass")
            .config
            .repoid
            .id(),
        1,
        "repo/x must be served with its parsed config"
    );

    // X's spec goes bad; an unrelated manifest change (new repo Y) triggers the next pass.
    fx.put_spec(X_PATH, "{}", 1);
    fx.put_spec(Y_PATH, &valid_repo_spec_json(2, "repo/y"), 1);
    let v1 = good_manifest(
        "tier_v0",
        vec![
            spec_entry("repo/x", 1, X_PATH),
            spec_entry("repo/y", 2, Y_PATH),
        ],
    );
    fx.swap_manifest(&v1, 2);
    fx.run().await;

    let served = fx.repo_configs.load();
    assert_eq!(
        served
            .repos
            .get("repo/x")
            .expect("repo/x must STAY served: transient spec parse failure must keep the last known good per-repo config")
            .config
            .repoid
            .id(),
        1,
        "repo/x must retain its previously served config"
    );
    assert_eq!(
        served
            .repos
            .get("repo/y")
            .expect("the unrelated new repo/y must appear")
            .config
            .repoid
            .id(),
        2,
        "repo/y must be served from its freshly parsed spec"
    );
}

fn served_entry(fx: &ReloadFixture, name: &str) -> Arc<RepoConfigEntry> {
    fx.repo_configs
        .load()
        .repos
        .get(name)
        .unwrap_or_else(|| panic!("{name} must be served"))
        .clone()
}

// Incremental bulk pass: adding a repo to the manifest parses only the new repo.
#[mononoke::test]
async fn test_manifest_reload_reuses_parse_for_unchanged_spec() {
    const X_PATH: &str = "test/repos/x";
    const Y_PATH: &str = "test/repos/y";

    let v0 = good_manifest("tier_v0", vec![spec_entry("repo/x", 1, X_PATH)]);
    let mut fx = ReloadFixture::new(&v0, &[]);
    fx.put_spec(X_PATH, &valid_repo_spec_json(1, "repo/x"), 0);
    fx.run().await;
    let x_before = served_entry(&fx, "repo/x");

    fx.put_spec(Y_PATH, &valid_repo_spec_json(2, "repo/y"), 1);
    let v1 = good_manifest(
        "tier_v0",
        vec![
            spec_entry("repo/x", 1, X_PATH),
            spec_entry("repo/y", 2, Y_PATH),
        ],
    );
    fx.swap_manifest(&v1, 2);
    fx.run().await;

    assert!(
        Arc::ptr_eq(&x_before, &served_entry(&fx, "repo/x")),
        "repo/x's spec did not change, so the pass must serve the same parsed config, not a re-parse"
    );
    assert_eq!(
        served_entry(&fx, "repo/y").config.repoid.id(),
        2,
        "the newly added repo/y must be parsed and served"
    );
    assert_eq!(
        fx.state.parsed_specs.len(),
        2,
        "both repos must be recorded for the next pass"
    );
}

// A changed spec is re-parsed by the bulk pass even when its per-repo fire has not been handled.
#[mononoke::test]
async fn test_manifest_reload_reparses_changed_spec() {
    const X_PATH: &str = "test/repos/x";
    const Y_PATH: &str = "test/repos/y";

    let v0 = good_manifest("tier_v0", vec![spec_entry("repo/x", 1, X_PATH)]);
    let mut fx = ReloadFixture::new(&v0, &[]);
    fx.put_spec(X_PATH, &valid_repo_spec_json(1, "repo/x"), 0);
    fx.run().await;
    let x_before = served_entry(&fx, "repo/x");

    fx.put_spec(X_PATH, &valid_repo_spec_json(11, "repo/x"), 1);
    fx.put_spec(Y_PATH, &valid_repo_spec_json(2, "repo/y"), 1);
    let v1 = good_manifest(
        "tier_v0",
        vec![
            spec_entry("repo/x", 1, X_PATH),
            spec_entry("repo/y", 2, Y_PATH),
        ],
    );
    fx.swap_manifest(&v1, 2);
    fx.run().await;

    let x_after = served_entry(&fx, "repo/x");
    assert!(
        !Arc::ptr_eq(&x_before, &x_after),
        "a changed spec must not be served from the previous parse"
    );
    assert_eq!(
        x_after.config.repoid.id(),
        11,
        "repo/x must be served from its new spec"
    );
}

// Every parse resolves against the manifest's storage, so a storage change invalidates all reuse.
#[mononoke::test]
async fn test_manifest_reload_reparses_all_on_storage_change() {
    const X_PATH: &str = "test/repos/x";

    let v0 = good_manifest("tier_v0", vec![spec_entry("repo/x", 1, X_PATH)]);
    let mut fx = ReloadFixture::new(&v0, &[]);
    fx.put_spec(X_PATH, &valid_repo_spec_json(1, "repo/x"), 0);
    fx.run().await;
    let x_before = served_entry(&fx, "repo/x");

    let mut v1 = v0.clone();
    let extra = v1.storage[TEST_STORAGE].clone();
    v1.storage.insert("other_storage".to_string(), extra);
    fx.swap_manifest(&v1, 1);
    fx.run().await;

    assert!(
        !Arc::ptr_eq(&x_before, &served_entry(&fx, "repo/x")),
        "a storage change must re-parse repo/x even though its spec is unchanged"
    );
}

// A repo that leaves the manifest must not keep a stale parse that a later re-add could reuse.
#[mononoke::test]
async fn test_manifest_reload_drops_parse_for_removed_repo() {
    const X_PATH: &str = "test/repos/x";
    const Y_PATH: &str = "test/repos/y";

    let v0 = good_manifest(
        "tier_v0",
        vec![
            spec_entry("repo/x", 1, X_PATH),
            spec_entry("repo/y", 2, Y_PATH),
        ],
    );
    let mut fx = ReloadFixture::new(&v0, &[]);
    fx.put_spec(X_PATH, &valid_repo_spec_json(1, "repo/x"), 0);
    fx.put_spec(Y_PATH, &valid_repo_spec_json(2, "repo/y"), 0);
    fx.run().await;
    assert!(fx.state.parsed_specs.contains_key("repo/y"));

    let v1 = good_manifest("tier_v0", vec![spec_entry("repo/x", 1, X_PATH)]);
    fx.swap_manifest(&v1, 1);
    fx.run().await;

    assert!(
        !fx.state.parsed_specs.contains_key("repo/y"),
        "repo/y left the manifest, so its parse must be dropped"
    );
    assert!(
        !fx.repo_configs.load().repos.contains_key("repo/y"),
        "repo/y must no longer be served"
    );
}

// A failed pass must not replace the recorded parses, or the retry would re-parse everything.
#[mononoke::test]
async fn test_manifest_reload_keeps_parses_on_failed_pass() {
    const X_PATH: &str = "test/repos/x";

    let v0 = good_manifest("tier_v0", vec![spec_entry("repo/x", 1, X_PATH)]);
    let mut fx = ReloadFixture::new(&v0, &[]);
    fx.put_spec(X_PATH, &valid_repo_spec_json(1, "repo/x"), 0);
    fx.run().await;
    let x_before = served_entry(&fx, "repo/x");

    fx.swap_manifest(&unparsable_manifest(vec![]), 1);
    fx.run().await;
    assert!(
        fx.state.parsed_specs.contains_key("repo/x"),
        "an aborted pass must leave the recorded parses untouched"
    );

    let v2 = good_manifest("tier_v2", vec![spec_entry("repo/x", 1, X_PATH)]);
    fx.swap_manifest(&v2, 2);
    fx.run().await;
    assert!(
        Arc::ptr_eq(&x_before, &served_entry(&fx, "repo/x")),
        "the recovering pass must reuse repo/x's parse"
    );
}

// A served config replaced outside the bulk pass (per-repo update, on-demand load) is re-parsed, not
// overwritten with the older cached parse.
#[mononoke::test]
async fn test_manifest_reload_reparses_when_served_config_replaced() {
    const X_PATH: &str = "test/repos/x";
    const Y_PATH: &str = "test/repos/y";

    let v0 = good_manifest("tier_v0", vec![spec_entry("repo/x", 1, X_PATH)]);
    let mut fx = ReloadFixture::new(&v0, &[]);
    fx.put_spec(X_PATH, &valid_repo_spec_json(1, "repo/x"), 0);
    fx.run().await;
    let cached = served_entry(&fx, "repo/x");

    fx.repo_configs.rcu(|current| {
        let mut next = (**current).clone();
        next.insert_repo("repo/x".to_string(), repo_config_with_id(1));
        next
    });
    let replaced = served_entry(&fx, "repo/x");

    fx.put_spec(Y_PATH, &valid_repo_spec_json(2, "repo/y"), 1);
    let v1 = good_manifest(
        "tier_v0",
        vec![
            spec_entry("repo/x", 1, X_PATH),
            spec_entry("repo/y", 2, Y_PATH),
        ],
    );
    fx.swap_manifest(&v1, 2);
    fx.run().await;

    let x_after = served_entry(&fx, "repo/x");
    assert!(
        !Arc::ptr_eq(&cached, &x_after),
        "the cached parse is no longer what is served, so it must not be reused"
    );
    assert!(
        !Arc::ptr_eq(&replaced, &x_after),
        "repo/x must be freshly parsed from its spec"
    );
}

// A transient spec failure keeps the last good parse, so a spec fixed back to it reuses that parse.
#[mononoke::test]
async fn test_manifest_reload_carries_parse_across_spec_failure() {
    const X_PATH: &str = "test/repos/x";
    const Y_PATH: &str = "test/repos/y";
    const Z_PATH: &str = "test/repos/z";

    let v0 = good_manifest("tier_v0", vec![spec_entry("repo/x", 1, X_PATH)]);
    let mut fx = ReloadFixture::new(&v0, &[]);
    fx.put_spec(X_PATH, &valid_repo_spec_json(1, "repo/x"), 0);
    fx.run().await;
    let x_good = served_entry(&fx, "repo/x");

    // X's spec goes bad; adding Y triggers a bulk pass.
    fx.put_spec(X_PATH, "{}", 1);
    fx.put_spec(Y_PATH, &valid_repo_spec_json(2, "repo/y"), 1);
    let v1 = good_manifest(
        "tier_v0",
        vec![
            spec_entry("repo/x", 1, X_PATH),
            spec_entry("repo/y", 2, Y_PATH),
        ],
    );
    fx.swap_manifest(&v1, 2);
    fx.run().await;
    assert!(
        Arc::ptr_eq(&x_good, &served_entry(&fx, "repo/x")),
        "a failed spec parse must keep serving the last good config"
    );
    assert!(
        fx.state
            .parsed_specs
            .get("repo/x")
            .is_some_and(|parsed| Arc::ptr_eq(&parsed.entry, &x_good)),
        "the last good parse must be carried into the next pass"
    );

    // X's spec is fixed back to its original content; adding Z triggers another pass.
    fx.put_spec(X_PATH, &valid_repo_spec_json(1, "repo/x"), 3);
    fx.put_spec(Z_PATH, &valid_repo_spec_json(3, "repo/z"), 3);
    let v2 = good_manifest(
        "tier_v0",
        vec![
            spec_entry("repo/x", 1, X_PATH),
            spec_entry("repo/y", 2, Y_PATH),
            spec_entry("repo/z", 3, Z_PATH),
        ],
    );
    fx.swap_manifest(&v2, 4);
    fx.run().await;
    assert!(
        Arc::ptr_eq(&x_good, &served_entry(&fx, "repo/x")),
        "a spec restored to its last good content must reuse the carried parse"
    );
}

// With the kill switch off, every pass re-parses every repo.
#[mononoke::test]
async fn test_manifest_reload_reparses_all_when_knob_off() {
    const X_PATH: &str = "test/repos/x";

    let v0 = good_manifest("tier_v0", vec![spec_entry("repo/x", 1, X_PATH)]);
    let mut fx = ReloadFixture::new(&v0, &[]);
    fx.put_spec(X_PATH, &valid_repo_spec_json(1, "repo/x"), 0);
    fx.run().await;
    let x_before = served_entry(&fx, "repo/x");

    let v1 = good_manifest("tier_v1", vec![spec_entry("repo/x", 1, X_PATH)]);
    fx.swap_manifest(&v1, 1);
    with_just_knobs_async(
        JustKnobsInMemory::new(HashMap::from([(
            INCREMENTAL_RELOAD_KNOB.to_string(),
            KnobVal::Bool(false),
        )])),
        fx.run().boxed(),
    )
    .await;

    assert!(
        !Arc::ptr_eq(&x_before, &served_entry(&fx, "repo/x")),
        "with the kill switch off, an unchanged spec must still be re-parsed"
    );
}
