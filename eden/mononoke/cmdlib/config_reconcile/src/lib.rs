/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Level-triggered config-reload reconciliation controller for Mononoke — the
//! control plane for *"which repos/config should this task serve, right now?"*.
//!
//! [`reconcile`] diffs the controller's per-repo state against the live desired
//! config and converges: build new shallow members, rebuild on real content
//! drift (deep in place), drop only on positive evidence, and retry failures —
//! idempotently, so correctness depends only on *"a reconcile eventually runs"*,
//! never on which event fired in what order.
//!
//! A shared-config change drifts every repo at once. [`Splay`] spreads the
//! resulting rebuilds over a window, each repo at a fixed [`splay_offset`].
//!
//! The crate is deliberately decoupled from Mononoke's config/repo internals:
//! callers implement [`ConfigSource`] and [`RepoManager`]. That keeps this crate
//! free of config-store, repo-factory, JustKnobs, and stats dependencies, and
//! makes the whole control loop unit-testable with in-memory fakes.

use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use futures::stream;
use futures::stream::StreamExt;
use sha2::Digest;
use sha2::Sha256;

/// Content generation of a built repo: hashes captured at build time. Changes
/// only on real content change, not a no-op configerator version bump.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepoGeneration {
    pub spec_hash: u64,
    /// The tier's shared config (see [`ConfigSource::shared_generation`]).
    pub shared_gen: u64,
}

/// The controller's view of a repo it manages. A repo is EITHER healthy-loaded
/// at a known generation OR failed pending retry — never both (the enum makes
/// the contradictory state unrepresentable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepoState {
    /// Built and serving this generation.
    Loaded { generation: RepoGeneration },
    /// Last build/reload failed; retry next pass. `serving` is the generation
    /// still live — reconcile is non-destructive, so a failed rebuild keeps the
    /// old repo serving; `None` if the repo was never successfully built.
    Failed {
        serving: Option<RepoGeneration>,
        attempts: u32,
    },
}

impl RepoState {
    /// The generation currently serving for this repo, if any.
    fn generation(&self) -> Option<RepoGeneration> {
        match self {
            RepoState::Loaded { generation } => Some(*generation),
            RepoState::Failed { serving, .. } => *serving,
        }
    }
}

/// One tier-manifest entry: parse-independent membership (name + sharding mode).
#[derive(Clone, Debug)]
pub struct ManifestEntry {
    pub name: String,
    pub is_deep_sharded: bool,
}

/// Live desired state for a repo whose config the controller could read.
#[derive(Clone, Copy, Debug)]
pub struct DesiredRepo {
    pub enabled: bool,
    pub spec_hash: u64,
}

/// The single action for one repo this pass. One action per repo (the map key)
/// makes "build and drop the same repo" unrepresentable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepoAction {
    /// Shallow, enabled member not loaded here — build and add.
    Build,
    /// Loaded repo whose live config drifted (or that has no known-good
    /// generation, e.g. after a failure) — rebuild in place.
    Rebuild,
    /// Shallow loaded repo with positive evidence it should go.
    Drop,
}

/// Read-only view of the desired config for this task/tier, implemented by the
/// caller over its config store.
pub trait ConfigSource {
    /// Every repo in the tier manifest (parse-independent presence + sharding).
    fn manifest(&self) -> Vec<ManifestEntry>;
    /// Live desired state for `name`, or `None` if its spec is unreadable /
    /// unsubscribed (treated as "unknown", never "drop").
    fn desired(&self, name: &str) -> Option<DesiredRepo>;
    /// Content hash of the config the tier's repos share, now.
    fn shared_generation(&self) -> Result<u64>;
}

/// Mutating operations on the repos loaded for this task. Encapsulates the
/// (async) build and the (sync, lock-guarded) apply so this crate stays free of
/// repo-factory / config-store internals.
#[async_trait]
pub trait RepoManager: Sync {
    /// Names of repos currently loaded here.
    fn loaded_names(&self) -> HashSet<String>;
    /// Build `name` from its live config and insert it. `deep` selects
    /// reload-if-present (deep: never resurrects a repo ShardManager dropped
    /// mid-build) vs reload (shallow). Returns the applied generation, `None` if
    /// a deep repo was absent at apply time (skipped), or `Err` if the build
    /// failed (non-destructive: the previously-serving repo, if any, stays live).
    async fn build_and_apply(
        &self,
        name: &str,
        deep: bool,
        shared_gen: u64,
    ) -> Result<Option<RepoGeneration>>;
    /// Remove `name` from the loaded set and evict its cached config.
    fn drop_repo(&self, name: &str);
}

/// How a pass spreads the rebuilds that a shared-config change causes.
///
/// A loaded repo whose only drift is the shared generation rebuilds at its
/// [`splay_offset`] after the controller first saw that generation. A repo whose
/// own spec changed rebuilds at once, and its build reads the current shared
/// config, so it takes both changes together. Builds, drops and retries are
/// never delayed.
#[derive(Clone, Copy, Debug)]
pub struct Splay {
    pub window: Duration,
    pub now: Instant,
}

/// The shared generation the controller is converging to and when it first saw
/// it. The caller carries it from one pass to the next; offsets count from
/// `since`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharedGenSeen {
    pub shared_gen: u64,
    pub since: Instant,
}

/// The point in `window` at which `repo_name` rebuilds after a shared-config
/// change: the first byte of the SHA-256 of its name, as a fraction of 256. That
/// byte is the bucket of the repo's spec in configerator
/// (`scm/mononoke/repos/<kind>/<bucket>/<name>`), so every host rebuilds repos in
/// the same order.
pub fn splay_offset(repo_name: &str, window: Duration) -> Duration {
    let bucket = Sha256::digest(repo_name.as_bytes())[0];
    window.saturating_mul(u32::from(bucket)) / 256
}

/// Result of one reconcile pass: the next state plus a summary for the caller to
/// log / turn into metrics (keeps this crate free of stats/tracing deps).
#[derive(Debug, Default)]
pub struct ReconcileOutcome {
    pub next_state: HashMap<String, RepoState>,
    pub built: usize,
    pub rebuilt: usize,
    pub dropped: usize,
    pub failed: Vec<String>,
    /// Loaded repos waiting for their splay offset.
    pub deferred: usize,
    /// When the earliest waiting repo is due; `None` when none is waiting.
    pub next_due: Option<Instant>,
    /// To pass to the next pass.
    pub shared_gen_seen: Option<SharedGenSeen>,
}

/// When a repo whose only drift is the shared generation may rebuild.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SplaySchedule {
    pub(crate) since: Instant,
    pub(crate) now: Instant,
    pub(crate) window: Duration,
}

impl SplaySchedule {
    fn due(&self, repo_name: &str) -> Instant {
        self.since + splay_offset(repo_name, self.window)
    }
}

/// The actions for one pass, plus the repos left waiting for their offset.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    pub(crate) actions: HashMap<String, RepoAction>,
    pub(crate) deferred: usize,
    pub(crate) next_due: Option<Instant>,
}

/// Compute the action (if any) for each repo. Pure — the whole decision surface
/// is unit-testable, including the config SEVs as regression cases. With no
/// `schedule`, drift in the shared generation alone rebuilds at once.
pub(crate) fn compute_actions(
    state: &HashMap<String, RepoState>,
    loaded: &HashSet<String>,
    manifest: &[ManifestEntry],
    desired: &HashMap<String, DesiredRepo>,
    shared_gen: u64,
    schedule: Option<&SplaySchedule>,
) -> Plan {
    let is_deep: HashMap<&str, bool> = manifest
        .iter()
        .map(|e| (e.name.as_str(), e.is_deep_sharded))
        .collect();
    let manifest_names: HashSet<&str> = manifest.iter().map(|e| e.name.as_str()).collect();

    // Consider every repo that could need an action: manifest members, loaded
    // repos, and repos we already track.
    let mut candidates: HashSet<&str> = HashSet::new();
    candidates.extend(manifest_names.iter().copied());
    candidates.extend(loaded.iter().map(String::as_str));
    candidates.extend(state.keys().map(String::as_str));

    let mut plan = Plan::default();
    for name in candidates {
        let deep = is_deep.get(name).copied().unwrap_or(false);
        let d = desired.get(name);
        let is_loaded = loaded.contains(name);
        let in_manifest = manifest_names.contains(name);
        let served_gen = state.get(name).and_then(RepoState::generation);

        let action = if is_loaded && !deep && (!in_manifest || d.is_some_and(|d| !d.enabled)) {
            // Positive-evidence drop: gone from the manifest, or disabled.
            // Never deep; never on an unreadable spec (d == None keeps it).
            Some(RepoAction::Drop)
        } else if !is_loaded && !deep && d.is_some_and(|d| d.enabled) {
            // New shallow enabled member.
            Some(RepoAction::Build)
        } else if is_loaded && d.is_some_and(|d| d.enabled) {
            // Loaded + enabled (shallow or deep): rebuild on real drift, or
            // if there is no known-good generation (prior failure / seeded).
            match (d, served_gen) {
                (Some(d), Some(g)) if d.spec_hash != g.spec_hash => Some(RepoAction::Rebuild),
                (Some(_), Some(g)) if g.shared_gen != shared_gen => {
                    match schedule.map(|s| (s.due(name), s.now)) {
                        Some((due, now)) if due > now => {
                            plan.deferred += 1;
                            plan.next_due = Some(plan.next_due.map_or(due, |next| next.min(due)));
                            None
                        }
                        _ => Some(RepoAction::Rebuild),
                    }
                }
                (Some(_), Some(_)) => None,
                _ => Some(RepoAction::Rebuild),
            }
        } else {
            None
        };
        if let Some(action) = action {
            plan.actions.insert(name.to_string(), action);
        }
    }
    plan
}

/// Run one level-triggered reconciliation pass. Reads live desired state via
/// `config`, diffs it against `current_state` (plus the actually-loaded repos),
/// and converges via `manager`. Returns the next state + a summary. Builds run
/// concurrently up to `concurrency`; a failed build is non-destructive.
/// `shared_gen_seen` is the previous pass's [`ReconcileOutcome::shared_gen_seen`].
pub async fn reconcile<C: ConfigSource, M: RepoManager>(
    config: &C,
    manager: &M,
    current_state: &HashMap<String, RepoState>,
    concurrency: usize,
    splay: Splay,
    shared_gen_seen: Option<SharedGenSeen>,
) -> Result<ReconcileOutcome> {
    let manifest = config.manifest();
    let shared_gen = config.shared_generation()?;
    let loaded = manager.loaded_names();
    let seen = match shared_gen_seen {
        Some(seen) if seen.shared_gen == shared_gen => seen,
        _ => SharedGenSeen {
            shared_gen,
            since: splay.now,
        },
    };
    let schedule = SplaySchedule {
        since: seen.since,
        now: splay.now,
        window: splay.window,
    };

    // Live desired state for every manifest member the controller can read.
    let desired: HashMap<String, DesiredRepo> = manifest
        .iter()
        .filter_map(|e| config.desired(&e.name).map(|d| (e.name.clone(), d)))
        .collect();

    // Seed state for repos that are loaded but not yet tracked (built by startup
    // / ShardManager before reconcile ran): record them at their current
    // generation so the first pass does NOT needlessly rebuild the whole fleet.
    // Genuine drift is still caught on later passes.
    let mut state = current_state.clone();
    for name in &loaded {
        if !state.contains_key(name) {
            let generation = desired.get(name).map_or(
                RepoGeneration {
                    spec_hash: 0,
                    shared_gen: 0,
                },
                |d| RepoGeneration {
                    spec_hash: d.spec_hash,
                    shared_gen,
                },
            );
            state.insert(name.clone(), RepoState::Loaded { generation });
        }
    }

    let Plan {
        actions,
        deferred,
        next_due,
    } = compute_actions(
        &state,
        &loaded,
        &manifest,
        &desired,
        shared_gen,
        Some(&schedule),
    );
    if actions.is_empty() {
        return Ok(ReconcileOutcome {
            next_state: state,
            deferred,
            next_due,
            shared_gen_seen: Some(seen),
            ..Default::default()
        });
    }

    let is_deep: HashMap<&str, bool> = manifest
        .iter()
        .map(|e| (e.name.as_str(), e.is_deep_sharded))
        .collect();

    let mut outcome = ReconcileOutcome {
        deferred,
        next_due,
        shared_gen_seen: Some(seen),
        ..Default::default()
    };

    // Drops first: synchronous and cheap, and free membership before builds.
    for name in actions
        .iter()
        .filter(|(_, a)| **a == RepoAction::Drop)
        .map(|(name, _)| name)
    {
        manager.drop_repo(name);
        state.remove(name);
        outcome.dropped += 1;
    }

    // Builds + rebuilds concurrently (bounded). No lock is held across a build.
    let to_build: Vec<(String, RepoAction)> = actions
        .into_iter()
        .filter(|(_, a)| *a != RepoAction::Drop)
        .collect();

    let results: Vec<(String, RepoAction, Result<Option<RepoGeneration>>)> = stream::iter(to_build)
        .map(|(name, action)| {
            let deep = is_deep.get(name.as_str()).copied().unwrap_or(false);
            async move {
                let result = manager.build_and_apply(&name, deep, shared_gen).await;
                (name, action, result)
            }
        })
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await;

    for (name, action, result) in results {
        match result {
            Ok(Some(generation)) => {
                state.insert(name, RepoState::Loaded { generation });
                match action {
                    RepoAction::Build => outcome.built += 1,
                    RepoAction::Rebuild => outcome.rebuilt += 1,
                    RepoAction::Drop => {}
                }
            }
            // Deep repo absent at apply (ShardManager dropped it mid-build) — skip.
            Ok(None) => {}
            Err(_) => {
                let (serving, attempts) = match state.get(&name) {
                    Some(RepoState::Failed { serving, attempts }) => (*serving, attempts + 1),
                    Some(RepoState::Loaded { generation }) => (Some(*generation), 1),
                    None => (None, 1),
                };
                state.insert(name.clone(), RepoState::Failed { serving, attempts });
                outcome.failed.push(name);
            }
        }
    }

    outcome.next_state = state;
    Ok(outcome)
}

#[cfg(test)]
mod tests;
