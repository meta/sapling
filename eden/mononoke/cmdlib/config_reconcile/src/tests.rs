/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Unit tests for the config-reconcile controller: the `compute_actions`
//! planner (with the config SEVs as regression cases) and the `reconcile`
//! driver flows, exercised through in-memory `ConfigSource`/`RepoManager` fakes.

use std::sync::Mutex;

use mononoke_macros::mononoke;

use super::*;

fn repo_gen(spec_hash: u64, shared_gen: u64) -> RepoGeneration {
    RepoGeneration {
        spec_hash,
        shared_gen,
    }
}

fn entry(name: &str, is_deep_sharded: bool) -> ManifestEntry {
    ManifestEntry {
        name: name.to_string(),
        is_deep_sharded,
    }
}

fn desired(enabled: bool, spec_hash: u64) -> DesiredRepo {
    DesiredRepo { enabled, spec_hash }
}

fn names(vs: &[&str]) -> HashSet<String> {
    vs.iter().map(|s| s.to_string()).collect()
}

// ---- compute_actions: SEV regressions + core cases ----

#[mononoke::test]
fn identical_content_is_a_noop() {
    // S684063: identical content + storage gen -> no rebuild.
    let state = HashMap::from([(
        "r".to_string(),
        RepoState::Loaded {
            generation: repo_gen(1, 9),
        },
    )]);
    let manifest = vec![entry("r", false)];
    let desired_map = HashMap::from([("r".to_string(), desired(true, 1))]);
    let actions = compute_actions(&state, &names(&["r"]), &manifest, &desired_map, 9);
    assert!(
        actions.is_empty(),
        "identical content must be a no-op, got {actions:?}"
    );
}

#[mononoke::test]
fn drift_rebuilds_shallow_and_deep() {
    // D112108580 (shallow content edit) + S685134 (deep reassign) shapes.
    let state = HashMap::from([
        (
            "s".to_string(),
            RepoState::Loaded {
                generation: repo_gen(1, 0),
            },
        ),
        (
            "d".to_string(),
            RepoState::Loaded {
                generation: repo_gen(2, 0),
            },
        ),
    ]);
    let manifest = vec![entry("s", false), entry("d", true)];
    let desired_map = HashMap::from([
        ("s".to_string(), desired(true, 11)),
        ("d".to_string(), desired(true, 22)),
    ]);
    let actions = compute_actions(&state, &names(&["s", "d"]), &manifest, &desired_map, 0);
    assert_eq!(actions.get("s"), Some(&RepoAction::Rebuild));
    assert_eq!(actions.get("d"), Some(&RepoAction::Rebuild));
}

#[mononoke::test]
fn shared_generation_change_rebuilds() {
    let state = HashMap::from([(
        "r".to_string(),
        RepoState::Loaded {
            generation: repo_gen(1, 7),
        },
    )]);
    let manifest = vec![entry("r", false)];
    let desired_map = HashMap::from([("r".to_string(), desired(true, 1))]);
    let actions = compute_actions(&state, &names(&["r"]), &manifest, &desired_map, 8);
    assert_eq!(
        actions.get("r"),
        Some(&RepoAction::Rebuild),
        "a shared-gen bump must rebuild even when the spec is unchanged",
    );
}

#[mononoke::test]
fn builds_new_shallow_enabled_only() {
    let manifest = vec![entry("on", false), entry("off", false), entry("deep", true)];
    let desired_map = HashMap::from([
        ("on".to_string(), desired(true, 1)),
        ("off".to_string(), desired(false, 1)),
        ("deep".to_string(), desired(true, 1)),
    ]);
    let actions = compute_actions(&HashMap::new(), &HashSet::new(), &manifest, &desired_map, 0);
    assert_eq!(actions.get("on"), Some(&RepoAction::Build));
    assert_eq!(actions.get("off"), None, "disabled member is not built");
    assert_eq!(
        actions.get("deep"),
        None,
        "deep adds are ShardManager's job"
    );
}

#[mononoke::test]
fn drops_only_on_positive_evidence_never_deep_never_unreadable() {
    let state = HashMap::from([
        (
            "gone".to_string(),
            RepoState::Loaded {
                generation: repo_gen(1, 0),
            },
        ),
        (
            "disabled".to_string(),
            RepoState::Loaded {
                generation: repo_gen(1, 0),
            },
        ),
        (
            "unreadable".to_string(),
            RepoState::Loaded {
                generation: repo_gen(1, 0),
            },
        ),
        (
            "deepgone".to_string(),
            RepoState::Loaded {
                generation: repo_gen(1, 0),
            },
        ),
    ]);
    // Manifest still lists "disabled" and "unreadable"; "gone" is absent entirely.
    let manifest = vec![entry("disabled", false), entry("unreadable", false)];
    let desired_map = HashMap::from([("disabled".to_string(), desired(false, 1))]);
    let actions = compute_actions(
        &state,
        &names(&["gone", "disabled", "unreadable", "deepgone"]),
        &manifest,
        &desired_map,
        0,
    );
    assert_eq!(
        actions.get("gone"),
        Some(&RepoAction::Drop),
        "gone-from-manifest drops"
    );
    assert_eq!(
        actions.get("disabled"),
        Some(&RepoAction::Drop),
        "disabled drops"
    );
    assert_eq!(
        actions.get("unreadable"),
        None,
        "S685413/R3-2: an unreadable manifest member is kept, never dropped",
    );
    assert_eq!(actions.get("deepgone"), Some(&RepoAction::Drop));
}

#[mononoke::test]
fn never_drops_deep_present_in_manifest() {
    let state = HashMap::from([(
        "d".to_string(),
        RepoState::Loaded {
            generation: repo_gen(1, 0),
        },
    )]);
    // Deep repo present in manifest but with no readable desired (unreadable).
    let manifest = vec![entry("d", true)];
    let actions = compute_actions(&state, &names(&["d"]), &manifest, &HashMap::new(), 0);
    assert_eq!(
        actions.get("d"),
        None,
        "a deep repo is never dropped by the controller"
    );
}

// ---- reconcile driver: fakes + flows ----

struct FakeConfig {
    manifest: Vec<ManifestEntry>,
    desired: HashMap<String, DesiredRepo>,
    shared_gen: u64,
}

impl ConfigSource for FakeConfig {
    fn manifest(&self) -> Vec<ManifestEntry> {
        self.manifest.clone()
    }
    fn desired(&self, name: &str) -> Option<DesiredRepo> {
        self.desired.get(name).copied()
    }
    fn shared_generation(&self) -> Result<u64> {
        Ok(self.shared_gen)
    }
}

struct FakeManager {
    loaded: Mutex<HashSet<String>>,
    fail: HashSet<String>,
    built: Mutex<Vec<String>>,
    dropped: Mutex<Vec<String>>,
}

impl FakeManager {
    fn new(loaded: &[&str], fail: &[&str]) -> Self {
        Self {
            loaded: Mutex::new(names(loaded)),
            fail: names(fail),
            built: Mutex::new(Vec::new()),
            dropped: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl RepoManager for FakeManager {
    fn loaded_names(&self) -> HashSet<String> {
        self.loaded.lock().expect("poisoned").clone()
    }
    async fn build_and_apply(
        &self,
        name: &str,
        _deep: bool,
        shared_gen: u64,
    ) -> Result<Option<RepoGeneration>> {
        self.built.lock().expect("poisoned").push(name.to_string());
        if self.fail.contains(name) {
            anyhow::bail!("simulated build failure for {name}");
        }
        self.loaded
            .lock()
            .expect("poisoned")
            .insert(name.to_string());
        Ok(Some(RepoGeneration {
            spec_hash: 100,
            shared_gen,
        }))
    }
    fn drop_repo(&self, name: &str) {
        self.loaded.lock().expect("poisoned").remove(name);
        self.dropped
            .lock()
            .expect("poisoned")
            .push(name.to_string());
    }
}

#[mononoke::test]
async fn first_pass_does_not_rebuild_already_loaded_repos() {
    // Issue #1: with empty state and repos already loaded at the current
    // generation, the first pass must be a no-op — not a fleet rebuild.
    let config = FakeConfig {
        manifest: vec![entry("a", false), entry("b", false)],
        desired: HashMap::from([
            ("a".to_string(), desired(true, 100)),
            ("b".to_string(), desired(true, 100)),
        ]),
        shared_gen: 0,
    };
    let mgr = FakeManager::new(&["a", "b"], &[]);
    let outcome = reconcile(&config, &mgr, &HashMap::new(), 4).await.unwrap();
    assert!(
        mgr.built.lock().unwrap().is_empty(),
        "first pass must not rebuild already-loaded repos, built {:?}",
        mgr.built.lock().unwrap(),
    );
    assert_eq!(outcome.built + outcome.rebuilt + outcome.dropped, 0);
}

#[mononoke::test]
async fn builds_new_and_drops_removed() {
    let config = FakeConfig {
        manifest: vec![entry("new", false)],
        desired: HashMap::from([("new".to_string(), desired(true, 100))]),
        shared_gen: 0,
    };
    // "old" is loaded but absent from the manifest -> dropped; "new" -> built.
    let mgr = FakeManager::new(&["old"], &[]);
    let outcome = reconcile(&config, &mgr, &HashMap::new(), 4).await.unwrap();
    assert_eq!(outcome.built, 1);
    assert_eq!(outcome.dropped, 1);
    assert_eq!(&*mgr.built.lock().unwrap(), &vec!["new".to_string()]);
    assert_eq!(&*mgr.dropped.lock().unwrap(), &vec!["old".to_string()]);
    assert!(outcome.next_state.contains_key("new"));
    assert!(!outcome.next_state.contains_key("old"));
}

#[mononoke::test]
async fn failed_build_is_non_destructive() {
    let config = FakeConfig {
        manifest: vec![entry("r", false)],
        desired: HashMap::from([("r".to_string(), desired(true, 200))]),
        shared_gen: 0,
    };
    // "r" loaded at gen 100; desired 200 -> rebuild, but build fails.
    let mgr = FakeManager::new(&["r"], &["r"]);
    let state = HashMap::from([(
        "r".to_string(),
        RepoState::Loaded {
            generation: repo_gen(100, 0),
        },
    )]);
    let outcome = reconcile(&config, &mgr, &state, 4).await.unwrap();
    assert_eq!(outcome.failed, vec!["r".to_string()]);
    match outcome.next_state.get("r") {
        Some(RepoState::Failed { serving, attempts }) => {
            assert_eq!(
                *serving,
                Some(repo_gen(100, 0)),
                "old generation kept serving"
            );
            assert_eq!(*attempts, 1);
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    // The repo is still loaded (non-destructive).
    assert!(mgr.loaded_names().contains("r"));
}
