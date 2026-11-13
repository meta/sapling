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

const WINDOW: Duration = Duration::from_secs(256);

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

fn desired(enabled: bool, spec_hash: u64, shared_gen: u64) -> DesiredRepo {
    DesiredRepo {
        enabled,
        spec_hash,
        shared_gen,
    }
}

fn seen(gens: &[(u64, Instant)]) -> SharedGensSeen {
    let mut seen = SharedGensSeen::default();
    for &(shared_gen, since) in gens {
        seen.record(shared_gen, since);
    }
    seen
}

fn no_splay() -> Splay {
    Splay {
        window: Duration::ZERO,
        now: Instant::now(),
    }
}

fn names(vs: &[&str]) -> HashSet<String> {
    vs.iter().map(|s| s.to_string()).collect()
}

// ---- compute_actions: SEV regressions + core cases ----

#[mononoke::test]
fn identical_content_is_a_noop() {
    // S684063: identical content + shared gen -> no rebuild.
    let state = HashMap::from([(
        "r".to_string(),
        RepoState::Loaded {
            generation: repo_gen(1, 9),
        },
    )]);
    let manifest = vec![entry("r", false)];
    let desired_map = HashMap::from([("r".to_string(), desired(true, 1, 9))]);
    let actions = compute_actions(&state, &names(&["r"]), &manifest, &desired_map, None).actions;
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
        ("s".to_string(), desired(true, 11, 0)),
        ("d".to_string(), desired(true, 22, 0)),
    ]);
    let actions =
        compute_actions(&state, &names(&["s", "d"]), &manifest, &desired_map, None).actions;
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
    let desired_map = HashMap::from([("r".to_string(), desired(true, 1, 8))]);
    let actions = compute_actions(&state, &names(&["r"]), &manifest, &desired_map, None).actions;
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
        ("on".to_string(), desired(true, 1, 0)),
        ("off".to_string(), desired(false, 1, 0)),
        ("deep".to_string(), desired(true, 1, 0)),
    ]);
    let actions = compute_actions(
        &HashMap::new(),
        &HashSet::new(),
        &manifest,
        &desired_map,
        None,
    )
    .actions;
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
    let desired_map = HashMap::from([("disabled".to_string(), desired(false, 1, 0))]);
    let actions = compute_actions(
        &state,
        &names(&["gone", "disabled", "unreadable", "deepgone"]),
        &manifest,
        &desired_map,
        None,
    )
    .actions;
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
    let actions = compute_actions(&state, &names(&["d"]), &manifest, &HashMap::new(), None).actions;
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
}

impl ConfigSource for FakeConfig {
    fn manifest(&self) -> Vec<ManifestEntry> {
        self.manifest.clone()
    }
    fn desired(&self, name: &str) -> Option<DesiredRepo> {
        self.desired.get(name).copied()
    }
}

struct FakeManager {
    shared_gen: u64,
    loaded: Mutex<HashSet<String>>,
    // Repos already serving their live config, which a build skips as the
    // repos manager does.
    unchanged: HashSet<String>,
    fail: HashSet<String>,
    built: Mutex<Vec<String>>,
    dropped: Mutex<Vec<String>>,
}

impl FakeManager {
    fn new(loaded: &[&str], fail: &[&str]) -> Self {
        Self {
            shared_gen: 0,
            loaded: Mutex::new(names(loaded)),
            unchanged: HashSet::new(),
            fail: names(fail),
            built: Mutex::new(Vec::new()),
            dropped: Mutex::new(Vec::new()),
        }
    }

    fn unchanged(mut self, unchanged: &[&str]) -> Self {
        self.unchanged = names(unchanged);
        self
    }
}

#[async_trait]
impl RepoManager for FakeManager {
    fn loaded_names(&self) -> HashSet<String> {
        self.loaded.lock().expect("poisoned").clone()
    }
    async fn build_and_apply(&self, name: &str, _deep: bool) -> Result<Option<RepoGeneration>> {
        if !self.unchanged.contains(name) {
            self.built.lock().expect("poisoned").push(name.to_string());
            if self.fail.contains(name) {
                anyhow::bail!("simulated build failure for {name}");
            }
            self.loaded
                .lock()
                .expect("poisoned")
                .insert(name.to_string());
        }
        Ok(Some(RepoGeneration {
            spec_hash: 100,
            shared_gen: self.shared_gen,
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
            ("a".to_string(), desired(true, 100, 0)),
            ("b".to_string(), desired(true, 100, 0)),
        ]),
    };
    let mgr = FakeManager::new(&["a", "b"], &[]).unchanged(&["a", "b"]);
    reconcile(
        &config,
        &mgr,
        &HashMap::new(),
        4,
        no_splay(),
        SharedGensSeen::default(),
    )
    .await
    .unwrap();
    assert!(
        mgr.built.lock().unwrap().is_empty(),
        "first pass must not rebuild already-loaded repos, built {:?}",
        mgr.built.lock().unwrap(),
    );
}

#[mononoke::test]
async fn first_pass_rebuilds_a_repo_changed_while_startup_built_it() {
    // "a" and "b" were built at startup, and "a"'s spec changed before the
    // first pass, so only "b" still serves its live config.
    let config = FakeConfig {
        manifest: vec![entry("a", false), entry("b", false)],
        desired: HashMap::from([
            ("a".to_string(), desired(true, 200, 0)),
            ("b".to_string(), desired(true, 100, 0)),
        ]),
    };
    let mgr = FakeManager::new(&["a", "b"], &[]).unchanged(&["b"]);
    reconcile(
        &config,
        &mgr,
        &HashMap::new(),
        4,
        no_splay(),
        SharedGensSeen::default(),
    )
    .await
    .unwrap();
    assert_eq!(*mgr.built.lock().unwrap(), vec!["a".to_string()]);
}

#[mononoke::test]
async fn builds_new_and_drops_removed() {
    let config = FakeConfig {
        manifest: vec![entry("new", false)],
        desired: HashMap::from([("new".to_string(), desired(true, 100, 0))]),
    };
    // "old" is loaded but absent from the manifest -> dropped; "new" -> built.
    let mgr = FakeManager::new(&["old"], &[]);
    let outcome = reconcile(
        &config,
        &mgr,
        &HashMap::new(),
        4,
        no_splay(),
        SharedGensSeen::default(),
    )
    .await
    .unwrap();
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
        desired: HashMap::from([("r".to_string(), desired(true, 200, 0))]),
    };
    // "r" loaded at gen 100; desired 200 -> rebuild, but build fails.
    let mgr = FakeManager::new(&["r"], &["r"]);
    let state = HashMap::from([(
        "r".to_string(),
        RepoState::Loaded {
            generation: repo_gen(100, 0),
        },
    )]);
    let outcome = reconcile(
        &config,
        &mgr,
        &state,
        4,
        no_splay(),
        SharedGensSeen::default(),
    )
    .await
    .unwrap();
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

// ---- splay ----
//
// With a 256s window a repo's offset in seconds is its bucket: sha256("repo")
// starts 0x07 and sha256("fbsource") starts 0xf4.

#[mononoke::test]
fn splay_offset_is_the_spec_bucket() {
    assert_eq!(splay_offset("repo", WINDOW), Duration::from_secs(0x07));
    assert_eq!(splay_offset("fbsource", WINDOW), Duration::from_secs(0xf4));
    assert_eq!(splay_offset("fbsource", Duration::ZERO), Duration::ZERO);
    assert!(
        splay_offset("fbsource", Duration::MAX) > Duration::ZERO,
        "a huge window saturates rather than overflowing"
    );
}

fn loaded_at(names: &[&str], generation: RepoGeneration) -> HashMap<String, RepoState> {
    names
        .iter()
        .map(|name| (name.to_string(), RepoState::Loaded { generation }))
        .collect()
}

/// A schedule `elapsed_secs` after shared generation 8 was first seen.
fn schedule(since: Instant, elapsed_secs: u64) -> SplaySchedule {
    SplaySchedule {
        seen: seen(&[(8, since)]),
        now: since + Duration::from_secs(elapsed_secs),
        window: WINDOW,
    }
}

#[mononoke::test]
fn shared_only_drift_waits_for_its_offset() {
    let state = loaded_at(&["repo", "fbsource"], repo_gen(1, 7));
    let loaded = names(&["repo", "fbsource"]);
    let manifest = vec![entry("repo", false), entry("fbsource", false)];
    let desired_map = HashMap::from([
        ("repo".to_string(), desired(true, 1, 8)),
        ("fbsource".to_string(), desired(true, 1, 8)),
    ]);
    let since = Instant::now();

    let plan = compute_actions(
        &state,
        &loaded,
        &manifest,
        &desired_map,
        Some(&schedule(since, 10)),
    );
    assert_eq!(plan.actions.get("repo"), Some(&RepoAction::Rebuild));
    assert_eq!(plan.actions.get("fbsource"), None);
    assert_eq!(plan.deferred, 1);
    assert_eq!(plan.next_due, Some(since + Duration::from_secs(0xf4)));
    assert_eq!(plan.seen, seen(&[(8, since)]));

    let plan = compute_actions(
        &state,
        &loaded,
        &manifest,
        &desired_map,
        Some(&schedule(since, 0xf4)),
    );
    assert_eq!(plan.actions.get("repo"), Some(&RepoAction::Rebuild));
    assert_eq!(plan.actions.get("fbsource"), Some(&RepoAction::Rebuild));
    assert_eq!(plan.deferred, 0);
    assert_eq!(plan.next_due, None);
}

#[mononoke::test]
fn spec_change_while_waiting_rebuilds_at_once() {
    let state = loaded_at(&["fbsource"], repo_gen(1, 7));
    let manifest = vec![entry("fbsource", false)];
    let desired_map = HashMap::from([("fbsource".to_string(), desired(true, 2, 8))]);
    let plan = compute_actions(
        &state,
        &names(&["fbsource"]),
        &manifest,
        &desired_map,
        Some(&schedule(Instant::now(), 1)),
    );
    assert_eq!(
        plan.actions.get("fbsource"),
        Some(&RepoAction::Rebuild),
        "a repo whose spec changed must not wait for its offset",
    );
    assert_eq!(plan.deferred, 0);
}

#[mononoke::test]
fn builds_and_drops_are_not_splayed() {
    let state = loaded_at(&["gone"], repo_gen(1, 7));
    let manifest = vec![entry("fbsource", false)];
    let desired_map = HashMap::from([("fbsource".to_string(), desired(true, 1, 8))]);
    let plan = compute_actions(
        &state,
        &names(&["gone"]),
        &manifest,
        &desired_map,
        Some(&schedule(Instant::now(), 0)),
    );
    assert_eq!(plan.actions.get("fbsource"), Some(&RepoAction::Build));
    assert_eq!(plan.actions.get("gone"), Some(&RepoAction::Drop));
    assert_eq!(plan.deferred, 0);
}

#[mononoke::test]
async fn shared_change_rebuilds_repos_at_their_offsets() {
    let config = FakeConfig {
        manifest: vec![entry("repo", false), entry("fbsource", false)],
        desired: HashMap::from([
            ("repo".to_string(), desired(true, 100, 1)),
            ("fbsource".to_string(), desired(true, 100, 1)),
        ]),
    };
    let mgr = FakeManager {
        shared_gen: 1,
        ..FakeManager::new(&["repo", "fbsource"], &[])
    };
    let state = loaded_at(&["repo", "fbsource"], repo_gen(100, 0));
    let t0 = Instant::now();
    let at = |secs: u64| Splay {
        window: WINDOW,
        now: t0 + Duration::from_secs(secs),
    };

    let first = reconcile(&config, &mgr, &state, 4, at(0), SharedGensSeen::default())
        .await
        .unwrap();
    assert_eq!(first.rebuilt, 0);
    assert_eq!(first.deferred, 2);
    assert_eq!(first.next_due, Some(t0 + Duration::from_secs(0x07)));
    let waiting = seen(&[(1, t0)]);
    assert_eq!(first.shared_gens_seen, waiting);

    let second = reconcile(
        &config,
        &mgr,
        &first.next_state,
        4,
        at(0x07),
        first.shared_gens_seen.clone(),
    )
    .await
    .unwrap();
    assert_eq!(&*mgr.built.lock().unwrap(), &vec!["repo".to_string()]);
    assert_eq!(second.rebuilt, 1);
    assert_eq!(second.deferred, 1);
    assert_eq!(second.next_due, Some(t0 + Duration::from_secs(0xf4)));
    assert_eq!(second.shared_gens_seen, waiting);

    let third = reconcile(
        &config,
        &mgr,
        &second.next_state,
        4,
        at(0xf4),
        second.shared_gens_seen.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        &*mgr.built.lock().unwrap(),
        &vec!["repo".to_string(), "fbsource".to_string()]
    );
    assert_eq!(third.rebuilt, 1);
    assert_eq!(third.deferred, 0);
    assert_eq!(third.next_due, None);
    assert_eq!(third.shared_gens_seen, SharedGensSeen::default());
}

#[mononoke::test]
async fn new_shared_generation_restarts_the_window() {
    let config = FakeConfig {
        manifest: vec![entry("fbsource", false)],
        desired: HashMap::from([("fbsource".to_string(), desired(true, 100, 2))]),
    };
    let mgr = FakeManager::new(&["fbsource"], &[]);
    let state = loaded_at(&["fbsource"], repo_gen(100, 0));
    let t0 = Instant::now();
    let now = t0 + Duration::from_secs(300);
    let outcome = reconcile(
        &config,
        &mgr,
        &state,
        4,
        Splay {
            window: WINDOW,
            now,
        },
        seen(&[(1, t0)]),
    )
    .await
    .unwrap();
    assert_eq!(
        outcome.rebuilt, 0,
        "the window restarts for the new generation"
    );
    assert_eq!(outcome.shared_gens_seen, seen(&[(2, now)]));
    assert_eq!(outcome.next_due, Some(now + Duration::from_secs(0xf4)));
}

#[mononoke::test]
fn only_repos_using_a_changed_shared_config_rebuild() {
    // a's storage changed (its generation went from 7 to 8); c's storage is
    // unchanged at 5.
    let state = HashMap::from([
        (
            "a".to_string(),
            RepoState::Loaded {
                generation: repo_gen(1, 7),
            },
        ),
        (
            "c".to_string(),
            RepoState::Loaded {
                generation: repo_gen(1, 5),
            },
        ),
    ]);
    let manifest = vec![entry("a", false), entry("c", false)];
    let desired_map = HashMap::from([
        ("a".to_string(), desired(true, 1, 8)),
        ("c".to_string(), desired(true, 1, 5)),
    ]);
    let actions =
        compute_actions(&state, &names(&["a", "c"]), &manifest, &desired_map, None).actions;
    assert_eq!(actions.get("a"), Some(&RepoAction::Rebuild));
    assert_eq!(
        actions.get("c"),
        None,
        "a repo whose shared config did not change is not rebuilt"
    );
}

#[mononoke::test]
fn each_shared_generation_has_its_own_window() {
    // "repo" waits for generation 8, first seen at t0; "fbsource" waits for
    // generation 9, first seen 100s later, so its offset counts from then.
    let t0 = Instant::now();
    let state = loaded_at(&["repo", "fbsource"], repo_gen(1, 7));
    let manifest = vec![entry("repo", false), entry("fbsource", false)];
    let desired_map = HashMap::from([
        ("repo".to_string(), desired(true, 1, 8)),
        ("fbsource".to_string(), desired(true, 1, 9)),
    ]);
    let plan = compute_actions(
        &state,
        &names(&["repo", "fbsource"]),
        &manifest,
        &desired_map,
        Some(&SplaySchedule {
            seen: seen(&[(8, t0), (9, t0 + Duration::from_secs(100))]),
            now: t0 + Duration::from_secs(0xf4),
            window: WINDOW,
        }),
    );
    assert_eq!(plan.actions.get("repo"), Some(&RepoAction::Rebuild));
    assert_eq!(plan.actions.get("fbsource"), None);
    assert_eq!(
        plan.next_due,
        Some(t0 + Duration::from_secs(100 + 0xf4)),
        "fbsource's offset counts from when generation 9 was first seen"
    );
    assert_eq!(
        plan.seen,
        seen(&[(9, t0 + Duration::from_secs(100))]),
        "only generations repos still wait for are carried"
    );
}
