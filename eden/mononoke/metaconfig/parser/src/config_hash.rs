/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Content-addressed hashing of per-repo config. The reconcile controller uses
//! these to detect *real* config drift: a hash changes only when the underlying
//! content changes, never on a no-op configerator version bump or a difference
//! in map iteration order. Always hashes canonical JSON, never thrift bytes
//! (thrift field ordering is not stable). In-process only (not persisted), so
//! the hasher need not be stable across builds.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;

use anyhow::Context;
use anyhow::Result;
use repos::RawStorageConfig;
use repos::RepoSpec;

/// A stable, content-addressed hash of a `RepoSpec`. Two specs with equal
/// content hash equal regardless of map ordering; a version bump with unchanged
/// content yields the same hash. Used by the reconcile controller as the
/// per-repo drift trigger (R3-1/N4).
pub fn spec_hash(spec: &RepoSpec) -> Result<u64> {
    let json = serde_json::to_value(spec).context("serializing RepoSpec for hashing")?;
    Ok(hash_canonical_json(&json))
}

/// A stable, content-addressed hash of the config a tier's repos share: the
/// manifest's named storage configs (`TierManifest.storage`). Bumps only on a
/// real content change, not a configerator version bump — so an unrelated
/// revision never triggers a fleet-wide rebuild (the S684063 shape).
pub fn shared_generation(storage: &HashMap<String, RawStorageConfig>) -> Result<u64> {
    let json = serde_json::to_value(storage).context("serializing storage configs for hashing")?;
    Ok(hash_canonical_json(&json))
}

/// Hash a JSON value by its canonical form (object keys recursively sorted) so
/// map/object iteration order never affects the result.
fn hash_canonical_json(json: &serde_json::Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    canonicalize_json(json).to_string().hash(&mut hasher);
    hasher.finish()
}

/// Recursively rewrite a JSON value with object keys sorted. `serde_json`
/// serialization preserves object insertion order (or sorts, depending on build
/// features); sorting here makes the serialized bytes canonical either way.
fn canonicalize_json(json: &serde_json::Value) -> serde_json::Value {
    match json {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k.clone(), canonicalize_json(v)))
                    .collect(),
            )
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonicalize_json).collect())
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests;
