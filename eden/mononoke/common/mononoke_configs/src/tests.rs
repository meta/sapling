/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Unit tests for `MononokeConfigs`. `mod tests;` submodule so `super` is the
//! crate root and private items stay visible (as the inline module was).

use std::time::Duration;

use cached_config::ModificationTime;
use cached_config::TestSource;
use metaconfig_types::CommonConfig;
use mononoke_macros::mononoke;
use repos::RawAllowlistIdentity;
use repos::RawBlobstoreConfig;
use repos::RawBlobstoreDisabled;
use repos::RawCommitIdentityScheme;
use repos::RawCommonConfig;
use repos::RawDbLocal;
use repos::RawMetadataConfig;
use repos::RawRedactionConfig;
use repos::RawRepoConfig;
use repos::RawStorageConfig;
use repos::TierRepoEntry;

use super::*;

const TEST_TIER: &str = "scs";
pub(crate) const TEST_STORAGE: &str = "test_storage";

fn empty_configs() -> MononokeConfigs {
    MononokeConfigs {
        repo_configs: Arc::new(ArcSwap::from_pointee(RepoConfigs::new(
            HashMap::new(),
            CommonConfig::default(),
        ))),
        storage_configs: Arc::new(ArcSwap::from_pointee(StorageConfigs {
            storage: HashMap::new(),
        })),
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

fn static_handle() -> ConfigHandle<RepoSpec> {
    ConfigHandle::from_json("{}").expect("RepoSpec::default serializes as {}")
}

fn make_store(entries: &[(&str, &str)]) -> ConfigStore {
    let source = TestSource::new();
    for (path, content) in entries {
        source.insert_config(path, content, ModificationTime::UnixTimestamp(0));
    }
    ConfigStore::new(Arc::new(source), Duration::from_secs(1), None)
}

fn configs_with_manifest(
    manifest_path: &str,
    entries: Vec<TierRepoEntry>,
    extra_paths: &[(&str, &str)],
) -> MononokeConfigs {
    let manifest = TierManifest {
        repos: entries,
        ..Default::default()
    };
    let manifest_json = serde_json::to_string(&manifest).unwrap();

    let mut all = vec![(manifest_path, manifest_json.as_str())];
    all.extend_from_slice(extra_paths);
    let store = make_store(&all);

    let mut cfg = empty_configs();
    cfg.maybe_manifest_handle = Some(
        store
            .get_config_handle::<TierManifest>(manifest_path.to_string())
            .unwrap(),
    );
    cfg.config_store = Some(store);
    cfg
}

#[mononoke::test]
fn test_ensure_repo_config_handle_no_manifest_returns_ok() {
    let cfg = empty_configs();
    assert!(cfg.ensure_repo_config_handle("any_repo").is_ok());
    assert!(cfg.repo_handles.read().unwrap().is_empty());
}

#[mononoke::test]
fn test_ensure_repo_config_handle_idempotent_when_already_present() {
    let cfg = empty_configs();
    cfg.repo_handles
        .write()
        .unwrap()
        .insert("existing".to_string(), static_handle());
    assert!(cfg.ensure_repo_config_handle("existing").is_ok());
    // Should not have created a duplicate or attempted manifest lookup.
    assert_eq!(cfg.repo_handles.read().unwrap().len(), 1);
}

#[mononoke::test]
fn test_ensure_repo_config_handle_not_in_manifest_returns_ok() {
    let cfg = configs_with_manifest(
        "test/manifest",
        vec![TierRepoEntry {
            repo_name: "other_repo".to_string(),
            ..Default::default()
        }],
        &[],
    );
    assert!(cfg.ensure_repo_config_handle("missing_repo").is_ok());
    // Repo not in manifest -> no handle registered (legacy-only path).
    assert!(
        cfg.repo_handles
            .read()
            .unwrap()
            .get("missing_repo")
            .is_none()
    );
}

#[mononoke::test]
fn test_ensure_repo_config_handle_registers_when_in_manifest() {
    let repo_cfg_path = "test/repos/aosp_manifest";
    let cfg = configs_with_manifest(
        "test/manifest",
        vec![TierRepoEntry {
            repo_name: "aosp/manifest".to_string(),
            repo_id: 42,
            config_path: repo_cfg_path.to_string(),
            is_deep_sharded: true,
            ..Default::default()
        }],
        &[(repo_cfg_path, "{}")],
    );

    assert!(cfg.ensure_repo_config_handle("aosp/manifest").is_ok());
    // Bug repro: deep-sharded repo in manifest gets a handle registered
    // by ensure_repo_config_handle. This is the registration that S678887
    // relied on but never happened because get_or_load_repo_config's
    // fast path skipped it.
    assert!(
        cfg.repo_handles
            .read()
            .unwrap()
            .get("aosp/manifest")
            .is_some()
    );

    // Idempotency: second call is a no-op fast path.
    assert!(cfg.ensure_repo_config_handle("aosp/manifest").is_ok());
    assert_eq!(cfg.repo_handles.read().unwrap().len(), 1);
}

// S685134: for a split-loaded repo (has a handle), remove_repo_config_handle must
// evict the bulk repo_configs entry too, else a reassigned repo serves stale config.
#[mononoke::test]
fn test_remove_repo_config_handle_evicts_bulk_cache() {
    let cfg = empty_configs();
    // A served split-loaded repo has both a per-repo handle and a bulk entry.
    cfg.repo_handles
        .write()
        .unwrap()
        .insert("foo".to_string(), static_handle());
    cfg.repo_configs.rcu(|current| {
        let mut next = (**current).clone();
        next.insert_repo(
            "foo".to_string(),
            RepoConfig {
                repoid: mononoke_types::RepositoryId::new(7),
                ..Default::default()
            },
        );
        next
    });

    cfg.remove_repo_config_handle("foo");

    assert!(
        !cfg.repo_configs.load().repos.contains_key("foo"),
        "must evict the bulk repo_configs entry (S685134)",
    );
    assert!(
        !cfg.repo_configs
            .load()
            .repos_by_id
            .contains_key(&mononoke_types::RepositoryId::new(7)),
        "eviction must also clean the repos_by_id index",
    );
}

// A legacy-blob-only repo (bulk entry, no handle) must NOT be evicted: there is no
// handle to re-parse from, so the entry must survive for re-add.
#[mononoke::test]
fn test_remove_repo_config_handle_preserves_legacy_only_entry() {
    let cfg = empty_configs();
    cfg.repo_configs.rcu(|current| {
        let mut next = (**current).clone();
        next.insert_repo(
            "legacy".to_string(),
            RepoConfig {
                repoid: mononoke_types::RepositoryId::new(9),
                ..Default::default()
            },
        );
        next
    });

    cfg.remove_repo_config_handle("legacy");

    assert!(
        cfg.repo_configs.load().repos.contains_key("legacy"),
        "legacy-only bulk entry (no handle) must be preserved",
    );
}

// --- batch_load_repo_configs_checked / load_all_repo_configs_checked ---

/// A minimal valid RawStorageConfig so a RepoSpec referencing it parses.
pub(crate) fn test_raw_storage_config() -> RawStorageConfig {
    RawStorageConfig {
        metadata: RawMetadataConfig::local(RawDbLocal {
            local_db_path: "/tmp/test_db".to_string(),
        }),
        blobstore: RawBlobstoreConfig::disabled(RawBlobstoreDisabled {}),
        ephemeral_blobstore: None,
        mutable_blobstore: RawBlobstoreConfig::disabled(RawBlobstoreDisabled {}),
    }
}

/// JSON for a RepoSpec that parses successfully (references TEST_STORAGE).
pub(crate) fn valid_repo_spec_json(repo_id: i32, repo_name: &str) -> String {
    let spec = RepoSpec {
        repo_id,
        repo_name: repo_name.to_string(),
        enabled: true,
        default_commit_identity_scheme: RawCommitIdentityScheme::GIT,
        repo_config: Some(RawRepoConfig {
            storage_config: Some(TEST_STORAGE.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    serde_json::to_string(&spec).expect("RepoSpec serializes")
}

/// MononokeConfigs with a manifest listing `entries`, a valid named storage,
/// and a tier set (so parse_repo_spec works). `extra_paths` supplies the
/// per-repo config blobs referenced by the entries' config_paths.
fn batch_configs(entries: Vec<TierRepoEntry>, extra_paths: &[(&str, &str)]) -> MononokeConfigs {
    let manifest = TierManifest {
        repos: entries,
        storage: HashMap::from([(TEST_STORAGE.to_string(), test_raw_storage_config())]),
        ..Default::default()
    };
    let manifest_json = serde_json::to_string(&manifest).unwrap();
    let manifest_path = "test/manifest";

    let mut all = vec![(manifest_path, manifest_json.as_str())];
    all.extend_from_slice(extra_paths);
    let store = make_store(&all);

    let mut cfg = empty_configs();
    cfg.maybe_manifest_handle = Some(
        store
            .get_config_handle::<TierManifest>(manifest_path.to_string())
            .unwrap(),
    );
    cfg.config_store = Some(store);
    cfg.tier_name = Some(TEST_TIER.to_string());
    cfg
}

fn entry(repo_name: &str, repo_id: i32, config_path: &str) -> TierRepoEntry {
    TierRepoEntry {
        repo_name: repo_name.to_string(),
        repo_id,
        config_path: config_path.to_string(),
        // is_deep_sharded is irrelevant here: this fixture never pre-loads
        // handles (it bypasses MononokeConfigs::new), so batch load drives it.
        is_deep_sharded: true,
        ..Default::default()
    }
}

#[mononoke::test]
fn test_batch_load_checked_good_repo_in_loaded() {
    let path = "test/repos/good";
    let cfg = batch_configs(
        vec![entry("good/repo", 1, path)],
        &[(path, valid_repo_spec_json(1, "good/repo").as_str())],
    );

    let outcome = cfg
        .batch_load_repo_configs_checked(&["good/repo".to_string()])
        .expect("infra ok");

    assert!(outcome.failed.is_empty(), "a parseable repo must not fail");
    assert_eq!(outcome.loaded.len(), 1);
    assert_eq!(outcome.loaded[0].0, "good/repo");
    assert_eq!(
        outcome.loaded[0].1.repoid,
        mononoke_types::RepositoryId::new(1)
    );
}

// Negative case: a repo whose RepoSpec does not parse must land in `failed`
// (with its name) and must NOT appear in `loaded`.
#[mononoke::test]
fn test_batch_load_checked_unparsable_repo_in_failed() {
    let path = "test/repos/bad";
    // "{}" is a default RepoSpec with no storage_config -> parse fails.
    let cfg = batch_configs(vec![entry("bad/repo", 2, path)], &[(path, "{}")]);

    let outcome = cfg
        .batch_load_repo_configs_checked(&["bad/repo".to_string()])
        .expect("per-repo parse errors must not fail the whole batch");

    assert!(
        outcome.loaded.is_empty(),
        "unparsable repo must not be in loaded",
    );
    assert_eq!(outcome.failed.len(), 1);
    assert_eq!(outcome.failed[0].0, "bad/repo");
}

// A mix of a parseable and an unparsable repo returns both partitions.
#[mononoke::test]
fn test_batch_load_checked_mixed_partitions() {
    let good_path = "test/repos/good";
    let bad_path = "test/repos/bad";
    let cfg = batch_configs(
        vec![
            entry("good/repo", 1, good_path),
            entry("bad/repo", 2, bad_path),
        ],
        &[
            (good_path, valid_repo_spec_json(1, "good/repo").as_str()),
            (bad_path, "{}"),
        ],
    );

    let outcome = cfg
        .batch_load_repo_configs_checked(&["good/repo".to_string(), "bad/repo".to_string()])
        .expect("infra ok");

    let loaded: Vec<&str> = outcome.loaded.iter().map(|(n, _)| n.as_str()).collect();
    let failed: Vec<&str> = outcome.failed.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(loaded, ["good/repo"]);
    assert_eq!(failed, ["bad/repo"]);
}

// The back-compat wrapper discards failures and returns exactly `loaded`.
#[mononoke::test]
fn test_batch_load_wrapper_returns_only_loaded() {
    let good_path = "test/repos/good";
    let bad_path = "test/repos/bad";
    let cfg = batch_configs(
        vec![
            entry("good/repo", 1, good_path),
            entry("bad/repo", 2, bad_path),
        ],
        &[
            (good_path, valid_repo_spec_json(1, "good/repo").as_str()),
            (bad_path, "{}"),
        ],
    );

    let loaded = cfg
        .batch_load_repo_configs(&["good/repo".to_string(), "bad/repo".to_string()])
        .expect("wrapper ok");

    let names: Vec<&str> = loaded.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["good/repo"], "wrapper drops the failed repo");
}

// All-good input yields an empty `failed` partition.
#[mononoke::test]
fn test_batch_load_checked_all_good_no_failures() {
    let p1 = "test/repos/a";
    let p2 = "test/repos/b";
    let cfg = batch_configs(
        vec![entry("repo/a", 1, p1), entry("repo/b", 2, p2)],
        &[
            (p1, valid_repo_spec_json(1, "repo/a").as_str()),
            (p2, valid_repo_spec_json(2, "repo/b").as_str()),
        ],
    );

    let outcome = cfg
        .batch_load_repo_configs_checked(&["repo/a".to_string(), "repo/b".to_string()])
        .expect("infra ok");

    assert!(outcome.failed.is_empty());
    assert_eq!(outcome.loaded.len(), 2);
}

// load_all_repo_configs_checked unions manifest names and reports failures.
#[mononoke::test]
fn test_load_all_repo_configs_checked_unions_and_partitions() {
    let good_path = "test/repos/good";
    let bad_path = "test/repos/bad";
    let cfg = batch_configs(
        vec![
            entry("good/repo", 1, good_path),
            entry("bad/repo", 2, bad_path),
        ],
        &[
            (good_path, valid_repo_spec_json(1, "good/repo").as_str()),
            (bad_path, "{}"),
        ],
    );

    let outcome = cfg.load_all_repo_configs_checked().expect("infra ok");

    let loaded: Vec<&str> = outcome.loaded.iter().map(|(n, _)| n.as_str()).collect();
    let failed: Vec<&str> = outcome.failed.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(loaded, ["good/repo"]);
    assert_eq!(failed, ["bad/repo"]);
}

// A split-loaded repo must record the source-reported config version;
// mutation id is not plumbed yet and must stay None.
#[mononoke::test]
fn test_get_or_load_repo_config_records_config_version() {
    let spec_path = "test/repos/good";
    let manifest_path = "test/manifest";
    let manifest = TierManifest {
        repos: vec![entry("good/repo", 1, spec_path)],
        storage: HashMap::from([(TEST_STORAGE.to_string(), test_raw_storage_config())]),
        ..Default::default()
    };
    let manifest_json = serde_json::to_string(&manifest).unwrap();

    let source = TestSource::new();
    source.insert_config(
        manifest_path,
        &manifest_json,
        ModificationTime::UnixTimestamp(0),
    );
    source.insert_config_with_version(
        spec_path,
        &valid_repo_spec_json(1, "good/repo"),
        ModificationTime::UnixTimestamp(0),
        "config-version-7",
    );
    let store = ConfigStore::new(Arc::new(source), Duration::from_secs(1), None);

    let mut cfg = empty_configs();
    cfg.maybe_manifest_handle = Some(
        store
            .get_config_handle::<TierManifest>(manifest_path.to_string())
            .unwrap(),
    );
    cfg.config_store = Some(store);
    cfg.tier_name = Some(TEST_TIER.to_string());

    let config = cfg
        .get_or_load_repo_config("good/repo")
        .expect("split-loaded repo must load");

    assert_eq!(
        config.config_version.as_deref(),
        Some("config-version-7"),
        "split-loaded repo must record the source-reported config version"
    );
    assert_eq!(
        config.config_mutation_id, None,
        "mutation id is not plumbed yet and must stay None"
    );

    // The cached fast path must serve the same provenance.
    let cached = cfg
        .get_or_load_repo_config("good/repo")
        .expect("cached repo must load");
    assert_eq!(
        cached.config_version.as_deref(),
        Some("config-version-7"),
        "the cached entry must carry the same recorded config version"
    );
}

// --- Sourcing `common`/`storage` from the manifest ---

/// A default `RawCommonConfig` is not parsable: two required fields fail conversion.
pub(crate) fn test_raw_common_config(trusted_tier: &str) -> RawCommonConfig {
    RawCommonConfig {
        trusted_parties_hipster_tier: Some(trusted_tier.to_string()),
        internal_identity: Some(RawAllowlistIdentity {
            identity_type: "SERVICE_IDENTITY".to_string(),
            identity_data: "internal".to_string(),
        }),
        redaction_config: Some(RawRedactionConfig {
            blobstore: TEST_STORAGE.to_string(),
            redaction_sets_location: "test/redaction_sets".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub(crate) fn test_storage_map() -> HashMap<String, RawStorageConfig> {
    HashMap::from([(TEST_STORAGE.to_string(), test_raw_storage_config())])
}

/// Asserted parsable, else the tests below pass via the fallback branch instead.
fn parseable_manifest(trusted_tier: &str) -> TierManifest {
    let manifest = TierManifest {
        common: test_raw_common_config(trusted_tier),
        storage: test_storage_map(),
        ..Default::default()
    };
    assert!(
        parse_manifest_common_and_storage(&manifest).is_ok(),
        "fixture must parse, else the tests below pass for the wrong reason"
    );
    manifest
}

// --- Manifest-mode startup semantics (manifest_path ⇒ manifest authoritative) ---

const TEST_MANIFEST_PATH: &str = "scm/mononoke/repos/tiers/scs_manifest";

/// Runtime handle for `MononokeConfigs::new` to spawn tasks onto; tests never drive them.
fn test_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        // The watcher runs on this runtime and reads knobs, which are thread-local in tests.
        .on_thread_start(mononoke::override_just_knobs)
        .enable_all()
        .build()
        .expect("test runtime builds")
}

#[mononoke::test]
fn test_tier_mode_constructs_from_manifest() {
    let manifest_json = serde_json::to_string(&parseable_manifest("manifest_tier")).unwrap();
    let store = make_store(&[(TEST_MANIFEST_PATH, manifest_json.as_str())]);
    let rt = test_runtime();

    let cfg = MononokeConfigs::new(TEST_TIER, &store, rt.handle().clone())
        .expect("tier mode must construct from the manifest alone");

    assert_eq!(
        cfg.repo_configs().common.trusted_parties_hipster_tier,
        Some("manifest_tier".to_string()),
        "common must be sourced from the manifest",
    );
    assert!(
        cfg.storage_configs().storage.contains_key(TEST_STORAGE),
        "storage must be sourced from the manifest",
    );
    assert!(
        cfg.repo_configs().repos.is_empty(),
        "manifest mode starts with no repos; they are batch-loaded on demand",
    );
    assert!(
        cfg.auto_update_enabled(),
        "the unified watcher must still be spawned in manifest mode",
    );
}

// Fail-closed startup: an unparsable manifest aborts construction.
#[mononoke::test]
fn test_tier_mode_fails_closed_on_unparsable_manifest() {
    // Default RawCommonConfig is unparsable (required fields fail conversion).
    let bad_manifest = TierManifest {
        storage: test_storage_map(),
        ..Default::default()
    };
    assert!(
        parse_manifest_common_and_storage(&bad_manifest).is_err(),
        "fixture must be unparsable, else this test passes for the wrong reason"
    );
    let manifest_json = serde_json::to_string(&bad_manifest).unwrap();
    let store = make_store(&[(TEST_MANIFEST_PATH, manifest_json.as_str())]);
    let rt = test_runtime();

    let result = MononokeConfigs::new(TEST_TIER, &store, rt.handle().clone());

    assert!(
        result.is_err(),
        "tier mode must fail closed on an unparsable manifest",
    );
}

// The bootstrap pass is the sole seeder of the watcher's manifest snapshot:
// a manifest repo must become served with zero config changes after startup.
#[mononoke::test]
fn test_watcher_bootstrap_serves_manifest_repos_without_a_change() {
    let spec_path = "test/repos/good";
    let manifest = TierManifest {
        repos: vec![TierRepoEntry {
            repo_name: "good/repo".to_string(),
            repo_id: 1,
            config_path: spec_path.to_string(),
            is_deep_sharded: false,
            ..Default::default()
        }],
        storage: test_storage_map(),
        common: test_raw_common_config("manifest_tier"),
        ..Default::default()
    };
    assert!(
        parse_manifest_common_and_storage(&manifest).is_ok(),
        "fixture must parse, else construction fails closed and the test \
         cannot observe the bootstrap"
    );
    let manifest_json = serde_json::to_string(&manifest).unwrap();
    let spec_json = valid_repo_spec_json(1, "good/repo");
    let store = make_store(&[
        (TEST_MANIFEST_PATH, manifest_json.as_str()),
        (spec_path, spec_json.as_str()),
    ]);
    let rt = test_runtime();

    let cfg = MononokeConfigs::new(TEST_TIER, &store, rt.handle().clone())
        .expect("manifest mode must construct");

    // Poll: the bootstrap needs no config fire, only scheduling on the watcher task.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !cfg.repo_configs().repos.contains_key("good/repo") {
        assert!(
            std::time::Instant::now() < deadline,
            "bootstrap pass never subscribed+served the manifest repo; \
             without it every per-repo reload defers forever"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
