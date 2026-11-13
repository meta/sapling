/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use arc_swap::ArcSwap;
use cached_config::ConfigStore;
use cached_config::ModificationTime;
use cached_config::TestSource;
use config_reconcile::RepoState;
use environment::MononokeEnvironment;
use fbinit::FacebookInit;
use futures::FutureExt;
use futures::future::BoxFuture;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use metaconfig_parser::RepoConfigs;
use metaconfig_parser::StorageConfigs;
use metaconfig_types::CommitIdentityScheme;
use metaconfig_types::CommitIdentityScheme::GIT;
use metaconfig_types::CommitIdentityScheme::HG;
use metaconfig_types::CommitIdentityScheme::UNKNOWN;
use metaconfig_types::CommonConfig;
use metaconfig_types::LazyLoadingConfig;
use metaconfig_types::RepoConfig;
use metaconfig_types::RepoConfigRef;
use metaconfig_types::ShardedService;
use mononoke_configs::ConfigUpdateReceiver;
use mononoke_configs::MononokeConfigs;
use mononoke_macros::mononoke;
use mononoke_repos::MononokeRepos;
use mononoke_repos::RepoLoader;
use mononoke_types::RepositoryId;
use repo_factory::RepoFactory;
use repos::RawAllowlistIdentity;
use repos::RawBlobstoreConfig;
use repos::RawBlobstoreDisabled;
use repos::RawCommonConfig;
use repos::RawDbLocal;
use repos::RawMetadataConfig;
use repos::RawRedactionConfig;
use repos::RawStorageConfig;
use repos::TierManifest;
use repos::TierRepoEntry;
use tokio::sync::Notify;
use tokio::sync::Semaphore;

use super::ConfigDefinedRepoLoader;
use super::MononokeConfigUpdateReceiver;
use super::ReconcileDriver;
use super::ReconcileTrigger;
use super::RepoFilter;
use super::SplayState;
use super::get_or_load_repo_in;
use super::lazy_for_service;
use super::memoized_spec_hash;
use super::next_wake;
use super::reconcile_loop;
use super::repo_names_from_manifest;
use super::retain_live_cache_entries;
use super::run_exclusive;
use super::scheme_from_config_path;
use super::tick_interval_secs;

#[mononoke::test]
fn test_tick_interval_off_uses_fixed_backstop() {
    // Off: fixed 60s, tunable value ignored (knob may be unregistered).
    assert_eq!(tick_interval_secs(false, 5), 60);
    assert_eq!(tick_interval_secs(false, 0), 60);
}

#[mononoke::test]
fn test_tick_interval_on_honors_knob_and_floors_zero() {
    assert_eq!(tick_interval_secs(true, 30), 30);
    assert_eq!(tick_interval_secs(true, 1), 1);
    // 0 must floor to 1s, never a busy loop.
    assert_eq!(tick_interval_secs(true, 0), 1);
}

#[mononoke::test]
fn test_next_wake_is_the_backstop_or_the_next_due_rebuild() {
    let backstop = Duration::from_secs(60);
    assert_eq!(next_wake(backstop, None), backstop);
    assert_eq!(
        next_wake(backstop, Some(Duration::from_secs(5))),
        Duration::from_secs(5)
    );
    assert_eq!(next_wake(backstop, Some(Duration::from_secs(90))), backstop);
    // A due time already past, as a failed or skipped pass leaves it, waits 1s
    // rather than waking at once in a loop.
    assert_eq!(
        next_wake(backstop, Some(Duration::ZERO)),
        Duration::from_secs(1)
    );
}

#[mononoke::test]
async fn test_reconcile_trigger_wakes_on_bulk_update() {
    let notify = Arc::new(Notify::new());
    let trigger = ReconcileTrigger {
        notify: notify.clone(),
    };
    trigger
        .apply_update(empty_cache(), storage())
        .await
        .expect("apply_update is infallible");
    // notify_one leaves a permit, so notified() resolves at once; the timeout
    // only guards against a hang if the trigger failed to fire.
    tokio::time::timeout(Duration::from_secs(30), notify.notified())
        .await
        .expect("a bulk config update must wake the reconcile loop");
}

#[mononoke::test]
async fn test_reconcile_trigger_wakes_on_per_repo_update() {
    let notify = Arc::new(Notify::new());
    let trigger = ReconcileTrigger {
        notify: notify.clone(),
    };
    trigger
        .apply_repo_update("some_repo", &RepoConfig::default())
        .await
        .expect("apply_repo_update is infallible");
    tokio::time::timeout(Duration::from_secs(30), notify.notified())
        .await
        .expect("a per-repo config update must wake the reconcile loop");
}

// --- memoized_spec_hash ----------------------------------------------------
//
// A tiny `i32` stands in for `RepoSpec`; the injected `compute` treats the value
// itself as the "hash", so two Arcs with the same value hash equal even though
// they are distinct allocations (distinct pointers).

#[mononoke::test]
fn test_memoized_spec_hash_steady_state_hit() {
    // Same Arc across calls: the cached hash is reused, so compute runs once.
    let cache: Mutex<HashMap<String, (Arc<i32>, u64)>> = Mutex::new(HashMap::new());
    let calls = AtomicUsize::new(0);
    let spec = Arc::new(7i32);

    let h1 = memoized_spec_hash(&cache, "repo", &spec, |s| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(*s as u64)
    });
    assert_eq!(h1, Some(7), "first call computes and returns the hash");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let h2 = memoized_spec_hash(&cache, "repo", &spec, |s| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(*s as u64)
    });
    assert_eq!(h2, Some(7));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "pointer-identical Arc must reuse the cached hash without recomputing",
    );
}

#[mononoke::test]
fn test_memoized_spec_hash_noop_bump_recomputes_same_hash() {
    // A new Arc (different allocation) with the same value: the pointer miss
    // forces a recompute, but the resulting hash is unchanged (no-op content bump).
    let cache: Mutex<HashMap<String, (Arc<i32>, u64)>> = Mutex::new(HashMap::new());
    let calls = AtomicUsize::new(0);

    let spec1 = Arc::new(7i32);
    let h1 = memoized_spec_hash(&cache, "repo", &spec1, |s| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(*s as u64)
    });
    assert_eq!(h1, Some(7));

    let spec2 = Arc::new(7i32);
    assert!(
        !Arc::ptr_eq(&spec1, &spec2),
        "spec2 must be a distinct allocation",
    );
    let h2 = memoized_spec_hash(&cache, "repo", &spec2, |s| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(*s as u64)
    });
    assert_eq!(h2, Some(7), "same content must yield the same hash");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "a new Arc must recompute even when the hash is unchanged",
    );
}

#[mononoke::test]
fn test_memoized_spec_hash_real_change() {
    // A new Arc with a different value: recompute yields the new, different hash.
    let cache: Mutex<HashMap<String, (Arc<i32>, u64)>> = Mutex::new(HashMap::new());

    let spec1 = Arc::new(7i32);
    let h1 = memoized_spec_hash(&cache, "repo", &spec1, |s| Ok(*s as u64));
    assert_eq!(h1, Some(7));

    let spec2 = Arc::new(8i32);
    let h2 = memoized_spec_hash(&cache, "repo", &spec2, |s| Ok(*s as u64));
    assert_eq!(h2, Some(8), "changed content must produce the new hash");
}

#[mononoke::test]
fn test_memoized_spec_hash_caches_arc_strong_count() {
    // ABA proxy: after insert, the cache holds its own clone of the Arc, so the
    // strong count rises to >= 2 (our local + the cached one). Storing the Arc
    // (not a raw pointer) is what stops a reused address from false-matching.
    let cache: Mutex<HashMap<String, (Arc<i32>, u64)>> = Mutex::new(HashMap::new());
    let spec = Arc::new(7i32);
    assert_eq!(Arc::strong_count(&spec), 1);

    let _ = memoized_spec_hash(&cache, "repo", &spec, |s| Ok(*s as u64));
    assert!(
        Arc::strong_count(&spec) >= 2,
        "cache must retain its own clone of the Arc, got {}",
        Arc::strong_count(&spec),
    );
}

#[mononoke::test]
fn test_memoized_spec_hash_compute_err_returns_none() {
    // compute Err => None (matching the caller's `.ok()?`) and nothing is cached.
    let cache: Mutex<HashMap<String, (Arc<i32>, u64)>> = Mutex::new(HashMap::new());
    let spec = Arc::new(7i32);

    let h = memoized_spec_hash(&cache, "repo", &spec, |_| Err(anyhow::anyhow!("boom")));
    assert_eq!(h, None, "a failed compute must return None");
    assert!(
        cache.lock().expect("cache poisoned").is_empty(),
        "a failed compute must not populate the cache",
    );
}

// --- retain_live_cache_entries ---------------------------------------------

#[mononoke::test]
fn test_retain_live_cache_entries_evicts_absent() {
    // Seed {a,b,c}; keep {a,c}; b is evicted and its cached Arc released.
    let cache: Mutex<HashMap<String, (Arc<i32>, u64)>> = Mutex::new(HashMap::new());
    let spec_b = Arc::new(2i32);
    {
        let mut c = cache.lock().expect("cache poisoned");
        c.insert("a".to_string(), (Arc::new(1i32), 1));
        c.insert("b".to_string(), (spec_b.clone(), 2));
        c.insert("c".to_string(), (Arc::new(3i32), 3));
    }
    assert_eq!(
        Arc::strong_count(&spec_b),
        2,
        "local + cached clone before eviction",
    );

    let live: HashSet<&str> = ["a", "c"].into_iter().collect();
    retain_live_cache_entries(&cache, &live);

    let c = cache.lock().expect("cache poisoned");
    let mut names: Vec<&str> = c.keys().map(String::as_str).collect();
    names.sort();
    assert_eq!(names, vec!["a", "c"], "only live entries remain");
    drop(c);
    assert_eq!(
        Arc::strong_count(&spec_b),
        1,
        "evicting b must drop the cache's clone of its Arc",
    );
}

// --- run_exclusive ---------------------------------------------------------

#[mononoke::test]
async fn test_run_exclusive_runs_when_free() {
    let lock = tokio::sync::Mutex::new(());
    let ran = AtomicUsize::new(0);
    let out = run_exclusive(&lock, || async {
        ran.fetch_add(1, Ordering::SeqCst);
        42
    })
    .await;
    assert_eq!(out, Some(42), "free lock runs body and returns its output");
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

#[mononoke::test]
async fn test_run_exclusive_skips_when_held() {
    let lock = tokio::sync::Mutex::new(());
    let guard = lock.lock().await; // hold the lock
    let ran = AtomicUsize::new(0);

    // try_lock fails, so this returns promptly without queuing.
    let out = run_exclusive(&lock, || async {
        ran.fetch_add(1, Ordering::SeqCst);
        42
    })
    .await;
    assert_eq!(out, None, "a held lock must skip (not queue)");
    assert_eq!(ran.load(Ordering::SeqCst), 0, "skipped body must not run");
    drop(guard);
}

#[mononoke::test]
async fn test_run_exclusive_reruns_after_release() {
    let lock = tokio::sync::Mutex::new(());
    let ran = AtomicUsize::new(0);
    {
        let guard = lock.lock().await;
        let skipped = run_exclusive(&lock, || async {
            ran.fetch_add(1, Ordering::SeqCst);
            1
        })
        .await;
        assert_eq!(skipped, None, "skipped while held");
        drop(guard);
    }
    let out = run_exclusive(&lock, || async {
        ran.fetch_add(1, Ordering::SeqCst);
        1
    })
    .await;
    assert_eq!(out, Some(1), "must run once the lock is free again");
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "only the post-release run fired"
    );
}

#[mononoke::test]
async fn test_run_exclusive_sequential_calls_both_run() {
    // No contention between sequential calls: each acquires and releases the lock.
    let lock = tokio::sync::Mutex::new(());
    let ran = AtomicUsize::new(0);
    let a = run_exclusive(&lock, || async {
        ran.fetch_add(1, Ordering::SeqCst);
    })
    .await;
    let b = run_exclusive(&lock, || async {
        ran.fetch_add(1, Ordering::SeqCst);
    })
    .await;
    assert_eq!(a, Some(()));
    assert_eq!(b, Some(()));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        2,
        "both sequential (non-contending) calls run",
    );
}

// --- reconcile_loop --------------------------------------------------------
//
// Virtual time (`tokio::time::pause`) drives the backstop; the pass closure
// signals a `passed` Notify after each run so the test can await exactly one pass
// per phase (rather than guess yield counts). The backstop advance goes just past
// the interval so the sleep deadline is definitely reached. The injected
// `next_interval` keeps justknobs out of the loop.

/// Spawn a `reconcile_loop` whose pass bumps `count` then fires `passed`.
fn spawn_counting_loop(
    count: Arc<AtomicUsize>,
    passed: Arc<Notify>,
    trigger: Arc<Notify>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(reconcile_loop(
        move || {
            let count = count.clone();
            let passed = passed.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                passed.notify_one();
            }
        },
        trigger,
        move || interval,
    ))
}

#[mononoke::test]
async fn test_reconcile_loop_reconcile_first_and_wakes() {
    tokio::time::pause();
    let count = Arc::new(AtomicUsize::new(0));
    let passed = Arc::new(Notify::new());
    let trigger = Arc::new(Notify::new());
    let interval = Duration::from_secs(60);

    let handle = spawn_counting_loop(count.clone(), passed.clone(), trigger.clone(), interval);

    // Reconcile-first: one pass runs before the loop ever waits.
    passed.notified().await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "loop must run a pass before waiting",
    );

    // A trigger notification wakes it for another pass.
    trigger.notify_one();
    passed.notified().await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "a trigger notification must run another pass",
    );

    // The backstop sleep wakes it: advance just past the interval to fire it.
    tokio::time::advance(interval + Duration::from_millis(1)).await;
    passed.notified().await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        3,
        "the backstop sleep must run another pass",
    );

    handle.abort();
}

#[mononoke::test]
async fn test_reconcile_loop_abort_stops() {
    tokio::time::pause();
    let count = Arc::new(AtomicUsize::new(0));
    let passed = Arc::new(Notify::new());
    let trigger = Arc::new(Notify::new());
    let interval = Duration::from_secs(60);

    let handle = spawn_counting_loop(count.clone(), passed.clone(), trigger.clone(), interval);

    // Let the reconcile-first pass run, then stop the loop.
    passed.notified().await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    handle.abort();
    tokio::task::yield_now().await;

    // Neither a trigger nor the backstop runs a pass after abort. There is no
    // pass to await, so drive the scheduler and assert the count is unchanged.
    trigger.notify_one();
    tokio::time::advance(interval + Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "an aborted loop must not run further passes",
    );
}

// --- repo_names_from_manifest ----------------------------------------------

fn git_path(name: &str) -> String {
    format!("scm/mononoke/repos/git/ab/{name}")
}

fn hg_path(name: &str) -> String {
    format!("scm/mononoke/repos/hg/cd/{name}")
}

fn entry(name: &str, config_path: &str) -> TierRepoEntry {
    TierRepoEntry {
        repo_name: name.to_owned(),
        config_path: config_path.to_owned(),
        is_deep_sharded: true,
        ..Default::default()
    }
}

fn manifest_of(entries: &[(&str, String)]) -> TierManifest {
    TierManifest {
        repos: entries.iter().map(|(n, p)| entry(n, p)).collect(),
        ..Default::default()
    }
}

fn served_of(entries: &[(&str, bool, CommitIdentityScheme)]) -> RepoConfigs {
    let mut configs = RepoConfigs::new(HashMap::new(), CommonConfig::default());
    for (i, (name, enabled, scheme)) in entries.iter().enumerate() {
        let repoid = RepositoryId::new(i as i32 + 1);
        let default_commit_identity_scheme = scheme.clone();
        configs.insert_repo(
            name.to_string(),
            RepoConfig {
                repoid,
                enabled: *enabled,
                default_commit_identity_scheme,
                ..Default::default()
            },
        );
    }
    configs
}

fn names_of(entries: &[(&str, CommitIdentityScheme)]) -> HashMap<String, CommitIdentityScheme> {
    entries
        .iter()
        .map(|(n, s)| (n.to_string(), s.clone()))
        .collect()
}

// The layout convention is the only source of the scheme: git/, hg/ and p4/
// trees map to their schemes, anything else is UNKNOWN rather than a guess.
#[mononoke::test]
fn test_scheme_from_config_path_follows_the_tree_layout() {
    assert_eq!(scheme_from_config_path(&git_path("org/repo")), GIT);
    assert_eq!(scheme_from_config_path(&hg_path("fbsource")), HG);
    assert_eq!(
        scheme_from_config_path("scm/mononoke/repos/p4/d7/test_p4-repo"),
        CommitIdentityScheme::P4
    );
    assert_eq!(
        scheme_from_config_path("scm/mononoke/repos/common/x"),
        UNKNOWN
    );
    assert_eq!(scheme_from_config_path("test/repos/x"), UNKNOWN);
    assert_eq!(scheme_from_config_path(""), UNKNOWN);
}

// Every manifest entry is listed, enabled or not, served or not; an empty
// manifest yields an empty map.
#[mononoke::test]
fn test_repo_names_from_manifest_lists_every_entry() {
    let names = repo_names_from_manifest(&manifest_of(&[
        ("a", git_path("a")),
        ("fbsource", hg_path("fbsource")),
        ("odd", "elsewhere/odd".to_string()),
    ]));
    assert_eq!(
        names,
        names_of(&[("a", GIT), ("fbsource", HG), ("odd", UNKNOWN)])
    );
    assert!(repo_names_from_manifest(&manifest_of(&[])).is_empty());
}

// --- MononokeConfigUpdateReceiver ------------------------------------------

fn storage() -> Arc<StorageConfigs> {
    Arc::new(StorageConfigs {
        storage: HashMap::new(),
    })
}

fn empty_cache() -> Arc<RepoConfigs> {
    Arc::new(RepoConfigs::new(HashMap::new(), CommonConfig::default()))
}

// Legacy (blob) mode or knob off: no manifest source, rebuild from the cache as before.
#[mononoke::test]
async fn test_receiver_no_manifest_source_rebuilds_from_cache() {
    let map = Arc::new(ArcSwap::from_pointee(names_of(&[("a", GIT), ("b", GIT)])));
    let receiver = MononokeConfigUpdateReceiver::new(map.clone(), None);
    receiver
        .apply_update(Arc::new(served_of(&[("a", true, GIT)])), storage())
        .await
        .unwrap();
    assert_eq!(**map.load(), names_of(&[("a", GIT)]));
}

// Legacy mode: a per-repo update still patches the map by `enabled`.
#[mononoke::test]
async fn test_receiver_no_manifest_source_patches_on_repo_update() {
    let map = Arc::new(ArcSwap::from_pointee(names_of(&[("a", GIT)])));
    let receiver = MononokeConfigUpdateReceiver::new(map.clone(), None);
    let disabled = RepoConfig {
        enabled: false,
        ..Default::default()
    };
    receiver.apply_repo_update("a", &disabled).await.unwrap();
    assert!(map.load().is_empty());
}

// MononokeConfigs gone (teardown): fall back to the cache-derived map, no panic.
#[mononoke::test]
async fn test_receiver_dead_manifest_source_rebuilds_from_cache() {
    let map = Arc::new(ArcSwap::from_pointee(names_of(&[("a", GIT)])));
    let receiver = MononokeConfigUpdateReceiver::new(map.clone(), Some(Weak::new()));
    receiver
        .apply_update(empty_cache(), storage())
        .await
        .unwrap();
    assert!(map.load().is_empty());
}

const TIER: &str = "scs";
const MANIFEST_PATH: &str = "scm/mononoke/repos/tiers/scs_manifest";
const STORAGE: &str = "test_storage";

fn manifest_json(entries: &[(&str, String)]) -> String {
    let manifest = TierManifest {
        repos: entries.iter().map(|(n, p)| entry(n, p)).collect(),
        common: RawCommonConfig {
            trusted_parties_hipster_tier: Some("tier".to_string()),
            internal_identity: Some(RawAllowlistIdentity {
                identity_type: "SERVICE_IDENTITY".to_string(),
                identity_data: "internal".to_string(),
            }),
            redaction_config: Some(RawRedactionConfig {
                blobstore: STORAGE.to_string(),
                redaction_sets_location: "test/redaction_sets".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        },
        storage: HashMap::from([(
            STORAGE.to_string(),
            RawStorageConfig {
                metadata: RawMetadataConfig::local(RawDbLocal {
                    local_db_path: "/tmp/test_db".to_string(),
                }),
                blobstore: RawBlobstoreConfig::disabled(RawBlobstoreDisabled {}),
                ephemeral_blobstore: None,
                mutable_blobstore: RawBlobstoreConfig::disabled(RawBlobstoreDisabled {}),
            },
        )]),
        ..Default::default()
    };
    serde_json::to_string(&manifest).unwrap()
}

/// Manifest-mode MononokeConfigs over a TestSource. Every entry is deep-sharded
/// so the watcher subscribes nothing; no spec is readable because none is read.
/// Leaves the store's poller thread sleeping (as the mononoke_configs tests do).
fn manifest_configs(
    entries: &[(&str, String)],
) -> (Arc<MononokeConfigs>, Arc<TestSource>, ConfigStore) {
    let source = Arc::new(TestSource::new());
    source.insert_config(
        MANIFEST_PATH,
        &manifest_json(entries),
        ModificationTime::UnixTimestamp(0),
    );
    let store = ConfigStore::new(source.clone(), Duration::from_secs(3600), None);
    let configs = Arc::new(
        MononokeConfigs::new(TIER, &store, tokio::runtime::Handle::current())
            .expect("manifest mode constructs"),
    );
    (configs, source, store)
}

// Manifest source live: the map is exactly the manifest, whatever the cache
// holds. A stale entry drops, a never-cached entry appears, and a second pass
// over the same manifest is stable.
#[mononoke::test]
async fn test_receiver_manifest_source_derives_from_manifest() {
    let (configs, _source, _store) = manifest_configs(&[
        ("a", git_path("a")),
        ("b", git_path("b")),
        ("hg", hg_path("hg")),
    ]);
    let map = Arc::new(ArcSwap::from_pointee(names_of(&[
        ("a", GIT),
        ("gone", GIT),
    ])));
    let receiver = MononokeConfigUpdateReceiver::new(map.clone(), Some(Arc::downgrade(&configs)));

    let expected = names_of(&[("a", GIT), ("b", GIT), ("hg", HG)]);
    receiver
        .apply_update(empty_cache(), storage())
        .await
        .unwrap();
    assert_eq!(**map.load(), expected);

    receiver
        .apply_update(empty_cache(), storage())
        .await
        .unwrap();
    assert_eq!(**map.load(), expected, "second pass is stable");
}

// Manifest source live: a per-repo update is not a writer. A served repo
// flipping disabled neither removes it nor adds anything.
#[mononoke::test]
async fn test_receiver_manifest_source_ignores_repo_updates() {
    let (configs, _source, _store) = manifest_configs(&[("a", git_path("a"))]);
    let map = Arc::new(ArcSwap::from_pointee(names_of(&[("a", GIT)])));
    let receiver = MononokeConfigUpdateReceiver::new(map.clone(), Some(Arc::downgrade(&configs)));
    let disabled = RepoConfig {
        enabled: false,
        ..Default::default()
    };
    receiver.apply_repo_update("a", &disabled).await.unwrap();
    receiver
        .apply_repo_update("new", &RepoConfig::default())
        .await
        .unwrap();
    assert_eq!(**map.load(), names_of(&[("a", GIT)]));
}

// A manifest change reaches the map through the real handle: a new entry
// appears and a removed one drops on the next pass.
#[mononoke::test]
async fn test_receiver_manifest_change_is_picked_up() {
    let (configs, source, store) = manifest_configs(&[("a", git_path("a")), ("b", git_path("b"))]);
    let map = Arc::new(ArcSwap::from_pointee(HashMap::new()));
    let receiver = MononokeConfigUpdateReceiver::new(map.clone(), Some(Arc::downgrade(&configs)));
    receiver
        .apply_update(empty_cache(), storage())
        .await
        .unwrap();
    assert_eq!(**map.load(), names_of(&[("a", GIT), ("b", GIT)]));

    source.insert_config(
        MANIFEST_PATH,
        &manifest_json(&[("a", git_path("a")), ("c", hg_path("c"))]),
        ModificationTime::UnixTimestamp(1),
    );
    // TestSource only reports paths it has been told changed; the real
    // configerator source reports every changed path on its own.
    source.insert_to_refresh(MANIFEST_PATH.to_string());
    store.force_update_configs();
    receiver
        .apply_update(empty_cache(), storage())
        .await
        .unwrap();
    assert_eq!(**map.load(), names_of(&[("a", GIT), ("c", HG)]));
}

/// A repo config whose only interesting field is its lazy loading config.
fn repo_config_with(lazy_loading_config: Option<LazyLoadingConfig>) -> RepoConfig {
    RepoConfig {
        lazy_loading_config,
        ..Default::default()
    }
}

#[mononoke::test]
fn test_no_lazy_loading_config_is_eager() {
    let repo_config = repo_config_with(None);

    assert!(!lazy_for_service(
        &repo_config,
        Some(ShardedService::MononokeGitServer)
    ));
    assert!(!lazy_for_service(&repo_config, None));
}

#[mononoke::test]
fn test_lazy_only_for_the_service_it_names() {
    let repo_config = repo_config_with(Some(LazyLoadingConfig {
        sharded: HashMap::from([(ShardedService::MononokeGitServer, true)]),
        unsharded: false,
    }));

    assert!(lazy_for_service(
        &repo_config,
        Some(ShardedService::MononokeGitServer)
    ));
    // A service with no entry has no opinion, which is eager rather than
    // inheriting the answer given to another service.
    assert!(!lazy_for_service(
        &repo_config,
        Some(ShardedService::SourceControlService)
    ));
    assert!(!lazy_for_service(&repo_config, None));
}

#[mononoke::test]
fn test_explicit_false_is_eager() {
    let repo_config = repo_config_with(Some(LazyLoadingConfig {
        sharded: HashMap::from([(ShardedService::MononokeGitServer, false)]),
        unsharded: false,
    }));

    assert!(!lazy_for_service(
        &repo_config,
        Some(ShardedService::MononokeGitServer)
    ));
    assert!(!lazy_for_service(&repo_config, None));
}

#[mononoke::test]
fn test_unsharded_is_independent_of_the_sharded_map() {
    let repo_config = repo_config_with(Some(LazyLoadingConfig {
        sharded: HashMap::from([(ShardedService::MononokeGitServer, false)]),
        unsharded: true,
    }));

    // A task with no service identity reads `unsharded` and nothing else, so
    // an eager sharded entry does not hold it back.
    assert!(lazy_for_service(&repo_config, None));
    assert!(!lazy_for_service(
        &repo_config,
        Some(ShardedService::MononokeGitServer)
    ));
}

// --- get_or_load_repo: a tier member asked for after startup ---

/// Hands out `42` for any name and counts the asks.
#[derive(Default)]
struct CountingLoader {
    loads: AtomicUsize,
}

impl mononoke_repos::RepoLoader<i32> for CountingLoader {
    fn load(&self, _repo_name: String) -> futures::future::BoxFuture<'static, anyhow::Result<i32>> {
        use futures::FutureExt;
        self.loads.fetch_add(1, Ordering::SeqCst);
        async { Ok(42) }.boxed()
    }
}

fn spec_json(repo_id: i32, name: &str, enabled: bool) -> String {
    let spec = repos::RepoSpec {
        repo_id,
        repo_name: name.to_string(),
        enabled,
        default_commit_identity_scheme: repos::RawCommitIdentityScheme::GIT,
        repo_config: Some(repos::RawRepoConfig {
            storage_config: Some(STORAGE.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    serde_json::to_string(&spec).unwrap()
}

/// Manifest-mode configs over `names` (ids 1..), with specs staged only for
/// `with_specs` (name, enabled), plus a lazy collection over a counting loader.
fn on_demand_fixture(
    names: &[&str],
    with_specs: &[(&str, bool)],
) -> (
    Arc<MononokeRepos<i32>>,
    Arc<CountingLoader>,
    Arc<MononokeConfigs>,
    ConfigStore,
) {
    let entries: Vec<(&str, String)> = names.iter().map(|n| (*n, git_path(n))).collect();
    let mut manifest: TierManifest = serde_json::from_str(&manifest_json(&entries)).unwrap();
    for (i, entry) in manifest.repos.iter_mut().enumerate() {
        entry.repo_id = i as i32 + 1;
    }
    let source = Arc::new(TestSource::new());
    source.insert_config(
        MANIFEST_PATH,
        &serde_json::to_string(&manifest).unwrap(),
        ModificationTime::UnixTimestamp(0),
    );
    for (name, enabled) in with_specs {
        let repo_id = names.iter().position(|n| n == name).unwrap() as i32 + 1;
        source.insert_config(
            &git_path(name),
            &spec_json(repo_id, name, *enabled),
            ModificationTime::UnixTimestamp(0),
        );
    }
    let store = ConfigStore::new(source, Duration::from_secs(3600), None);
    let configs = Arc::new(
        MononokeConfigs::new(TIER, &store, tokio::runtime::Handle::current())
            .expect("manifest mode constructs"),
    );
    let loader = Arc::new(CountingLoader::default());
    let repos = Arc::new(MononokeRepos::new_lazy(loader.clone()));
    (repos, loader, configs, store)
}

#[mononoke::test]
async fn test_get_or_load_repo_builds_an_enabled_member_once() {
    let (repos, loader, configs, _store) = on_demand_fixture(&["a"], &[("a", true)]);
    assert!(repos.get_by_name("a").is_none(), "not loaded at startup");
    let repo = get_or_load_repo_in(&repos, &configs, None, "a")
        .await
        .unwrap()
        .expect("built on first use");
    assert_eq!(*repo, 42);
    assert_eq!(
        repos.get_by_id(1).as_deref(),
        Some(&42),
        "placeholder carried the manifest id"
    );
    let again = get_or_load_repo_in(&repos, &configs, None, "a")
        .await
        .unwrap()
        .expect("served");
    assert!(
        Arc::ptr_eq(&repo, &again),
        "the resident repo is returned untouched"
    );
    assert_eq!(loader.loads.load(Ordering::SeqCst), 1, "built once");
}

#[mononoke::test]
async fn test_get_or_load_repo_outside_the_manifest_is_unknown_without_reading_a_spec() {
    let (repos, loader, configs, _store) = on_demand_fixture(&["a"], &[]);
    assert!(
        get_or_load_repo_in(&repos, &configs, None, "ghost")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        repos.iter_names().next().is_none(),
        "no placeholder was registered"
    );
    assert_eq!(loader.loads.load(Ordering::SeqCst), 0);
}

#[mononoke::test]
async fn test_get_or_load_repo_disabled_member_is_unknown() {
    let (repos, loader, configs, _store) = on_demand_fixture(&["a"], &[("a", false)]);
    assert!(
        get_or_load_repo_in(&repos, &configs, None, "a")
            .await
            .unwrap()
            .is_none()
    );
    assert!(repos.iter_names().next().is_none());
    assert_eq!(loader.loads.load(Ordering::SeqCst), 0);
}

#[mononoke::test]
async fn test_get_or_load_repo_member_without_a_readable_spec_is_an_error() {
    let (repos, loader, configs, _store) = on_demand_fixture(&["a"], &[]);
    let err = match get_or_load_repo_in(&repos, &configs, None, "a").await {
        Err(e) => e,
        Ok(_) => panic!("in the manifest but its config does not load"),
    };
    assert!(
        format!("{err:#}").contains("loading config for tier repo a"),
        "{err:#}"
    );
    assert!(
        repos.iter_names().next().is_none(),
        "no placeholder for an unreadable member"
    );
    assert_eq!(loader.loads.load(Ordering::SeqCst), 0);
}

#[mononoke::test]
async fn test_get_or_load_repo_filtered_out_member_is_unknown() {
    let (repos, loader, configs, _store) = on_demand_fixture(&["a"], &[("a", true)]);
    let filter: RepoFilter = Arc::new(|name: &str| name != "a");
    assert!(
        get_or_load_repo_in(&repos, &configs, Some(&filter), "a")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(loader.loads.load(Ordering::SeqCst), 0);
}

#[mononoke::test]
async fn test_get_or_load_repo_is_a_no_op_without_a_loader() {
    let (_, _, configs, _store) = on_demand_fixture(&["a"], &[("a", true)]);
    let eager: MononokeRepos<i32> = MononokeRepos::new();
    assert!(
        get_or_load_repo_in(&eager, &configs, None, "a")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        eager.iter_names().next().is_none(),
        "no placeholder without a loader"
    );
}

// --- reconcile and a first-request build ------------------------------------

#[facet::container]
struct ConfigOnlyRepo {
    #[facet]
    repo_config: RepoConfig,
}

/// The production first-request loader, holding the repo it built until
/// released, so the slot stays Building.
struct PausedLoader {
    inner: ConfigDefinedRepoLoader,
    built: Arc<Notify>,
    release: Arc<Notify>,
}

impl RepoLoader<ConfigOnlyRepo> for PausedLoader {
    fn load(&self, repo_name: String) -> BoxFuture<'static, Result<ConfigOnlyRepo>> {
        let build = RepoLoader::<ConfigOnlyRepo>::load(&self.inner, repo_name);
        let (built, release) = (self.built.clone(), self.release.clone());
        async move {
            let repo = build.await?;
            built.notify_one();
            release.notified().await;
            Ok(repo)
        }
        .boxed()
    }
}

fn spec_with_acl(acl: &str) -> String {
    let mut spec: repos::RepoSpec = serde_json::from_str(&spec_json(1, "a", true)).unwrap();
    spec.hipster_acl = acl.to_string();
    serde_json::to_string(&spec).unwrap()
}

/// Reconcile over an assigned deep-sharded repo whose first request builds it
/// while its spec changes (or doesn't, when `change` is false). Returns the
/// config it serves at the end, and whether that is the repo the first request
/// built.
async fn first_request_build_during_a_spec_change(
    fb: FacebookInit,
    change: bool,
) -> Result<(RepoConfig, bool)> {
    let entries = [("a", git_path("a"))];
    let mut manifest: TierManifest = serde_json::from_str(&manifest_json(&entries))?;
    manifest.repos[0].repo_id = 1;
    let source = Arc::new(TestSource::new());
    source.insert_config(
        MANIFEST_PATH,
        &serde_json::to_string(&manifest)?,
        ModificationTime::UnixTimestamp(0),
    );
    source.insert_config(
        &git_path("a"),
        &spec_with_acl("old_acl"),
        ModificationTime::UnixTimestamp(0),
    );
    let store = ConfigStore::new(source.clone(), Duration::from_secs(3600), None);
    let configs = Arc::new(MononokeConfigs::new(
        TIER,
        &store,
        tokio::runtime::Handle::current(),
    )?);
    let repo_factory = Arc::new(RepoFactory::new(
        Arc::new(MononokeEnvironment::for_test(fb, &store)),
        configs.clone(),
    ));
    let built = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let repos = Arc::new(MononokeRepos::new_lazy(Arc::new(PausedLoader {
        inner: ConfigDefinedRepoLoader {
            configs: configs.clone(),
            repo_factory: repo_factory.clone(),
            redaction_disabled: false,
            build_permits: Arc::new(Semaphore::new(1)),
        },
        built: built.clone(),
        release: release.clone(),
    })));
    let driver = ReconcileDriver {
        configs: configs.clone(),
        repo_factory,
        repos: repos.clone(),
        redaction_disabled: false,
        reconcile_state: Arc::new(ArcSwap::from_pointee(HashMap::new())),
        splay_state: Mutex::new(SplayState::default()),
        spec_hash_cache: Arc::new(Mutex::new(HashMap::new())),
        reconcile_lock: Arc::new(tokio::sync::Mutex::new(())),
    };
    let pass = || {
        with_just_knobs_async(
            JustKnobsInMemory::new(HashMap::from([(
                "scm/mononoke:use_config_reconcile".to_string(),
                KnobVal::Bool(true),
            )])),
            driver.pass().boxed(),
        )
    };

    // ShardManager assigns the repo; its first request builds it from the
    // spec as it is then.
    repos.add_placeholder("a", 1);
    let first_request = {
        let repos = repos.clone();
        tokio::spawn(async move { repos.get("a").await })
    };
    built.notified().await;
    if change {
        source.insert_config(
            &git_path("a"),
            &spec_with_acl("new_acl"),
            ModificationTime::UnixTimestamp(1),
        );
        source.insert_to_refresh(git_path("a"));
        store.force_update_configs();
    }

    pass().await?;
    assert!(
        repos.get_by_name("a").is_none(),
        "the repo is still building"
    );
    assert!(
        !driver.reconcile_state.load().contains_key("a"),
        "reconcile records no generation for a repo that is building"
    );

    release.notify_one();
    let first = first_request
        .await??
        .expect("the first request gets the repo");
    assert_eq!(
        first.repo_config().hipster_acl.as_deref(),
        Some("old_acl"),
        "built from the old spec"
    );

    pass().await?;
    assert!(
        matches!(
            driver.reconcile_state.load().get("a"),
            Some(RepoState::Loaded { .. })
        ),
        "reconcile records the generation it applied"
    );
    let serving = repos.get_by_name("a").expect("still served");
    pass().await?;
    assert!(
        Arc::ptr_eq(&serving, &repos.get_by_name("a").expect("still served")),
        "a later pass with nothing changed builds nothing"
    );
    Ok((serving.repo_config().clone(), Arc::ptr_eq(&serving, &first)))
}

// A first request builds an assigned deep-sharded repo from its spec as the
// build starts. Reconcile passes it by while it builds; the first pass after
// that compares the config it serves with the live one and rebuilds it only if
// the spec changed meanwhile, with no further edit or restart.
#[mononoke::fbinit_test]
async fn test_reconcile_applies_a_spec_changed_during_a_first_request_build(
    fb: FacebookInit,
) -> Result<()> {
    let (serving, kept) = first_request_build_during_a_spec_change(fb, true).await?;
    assert_eq!(serving.hipster_acl.as_deref(), Some("new_acl"));
    assert!(!kept, "rebuilt");
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_reconcile_keeps_a_first_request_build_whose_spec_is_unchanged(
    fb: FacebookInit,
) -> Result<()> {
    let (serving, kept) = first_request_build_during_a_spec_change(fb, false).await?;
    assert_eq!(serving.hipster_acl.as_deref(), Some("old_acl"));
    assert!(kept, "the repo the first request built is kept");
    Ok(())
}
