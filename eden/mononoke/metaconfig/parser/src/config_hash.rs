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
use std::io;
use std::io::BufWriter;

use anyhow::Context;
use anyhow::Result;
use repos::RawRepoConfig;
use repos::RawStorageConfig;
use repos::RepoSpec;
use serde::Serialize;
use serde::Serializer;

/// A stable, content-addressed hash of a `RepoSpec`. Two specs with equal
/// content hash equal regardless of map ordering; a version bump with unchanged
/// content yields the same hash. Used by the reconcile controller as the
/// per-repo drift trigger (R3-1/N4).
pub fn spec_hash(spec: &RepoSpec) -> Result<u64> {
    let json = serde_json::to_value(spec).context("serializing RepoSpec for hashing")?;
    Ok(hash_canonical_json(&json))
}

/// Content hashes of a manifest's shared configs, by name. A repo's shared
/// generation ([`SharedGenerations::of_repo`]) covers only the shared configs it
/// names. A hash changes only on a real content change, not a configerator
/// version bump (the S684063 shape).
pub struct SharedGenerations {
    storage: HashMap<String, u64>,
}

impl SharedGenerations {
    /// From the manifest's named storage configs.
    pub fn new(storage: &HashMap<String, RawStorageConfig>) -> Result<Self> {
        let storage = storage
            .iter()
            .map(|(name, config)| {
                let json = serde_json::to_value(config)
                    .with_context(|| format!("serializing storage config {name} for hashing"))?;
                Ok((name.clone(), hash_canonical_json(&json)))
            })
            .collect::<Result<_>>()?;
        Ok(Self { storage })
    }

    /// The shared generation of `spec` on `tier`: a hash of every storage config
    /// that the spec's own config or its override for `tier` names, even one
    /// the override replaces. A storage config the manifest lacks hashes as
    /// missing, so the repo drifts when it is added.
    pub fn of_repo(&self, spec: &RepoSpec, tier: &str) -> u64 {
        hash_of(repo_layers(spec, tier).map(|layer| {
            layer.map(|config| named(&self.storage, config.storage_config.as_deref()))
        }))
    }
}

/// `None` if no shared config is named, `Some(None)` if the named one is missing.
fn named(hashes: &HashMap<String, u64>, name: Option<&str>) -> Option<Option<u64>> {
    name.map(|name| hashes.get(name).copied())
}

/// The spec's own config and its override for `tier`.
fn repo_layers<'a>(spec: &'a RepoSpec, tier: &str) -> [Option<&'a RawRepoConfig>; 2] {
    [
        spec.repo_config.as_ref(),
        spec.tier_overrides
            .as_ref()
            .and_then(|overrides| overrides.get(tier)),
    ]
}

fn hash_of(value: impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Hash a JSON value by its canonical text (compact, object keys recursively
/// sorted) so map/object iteration order never affects the result. The text is
/// streamed into the hasher rather than built, and hashes as the `str` it
/// spells.
fn hash_canonical_json(json: &serde_json::Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    // serde_json writes many small pieces; the buffer hands the hasher larger ones.
    let mut writer = BufWriter::with_capacity(1024, HashWriter(&mut hasher));
    serde_json::to_writer(&mut writer, &Canonical(json))
        .expect("writing a JSON value into a hasher should never fail");
    let HashWriter(hasher) = writer
        .into_inner()
        .expect("flushing into a hasher should never fail");
    // `str::hash` ends a string with this byte.
    hasher.write_u8(0xff);
    hasher.finish()
}

/// Writes into a hasher.
#[derive(Debug)]
struct HashWriter<'a, H>(&'a mut H);

impl<H: Hasher> io::Write for HashWriter<'_, H> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A JSON value that serializes with every object's keys in sorted order,
/// whatever order its map keeps them in.
struct Canonical<'a>(&'a serde_json::Value);

impl Serialize for Canonical<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            // serde_json's map keeps its keys sorted unless built with `preserve_order`.
            serde_json::Value::Object(map) if map.keys().is_sorted() => {
                serializer.collect_map(map.iter().map(|(key, value)| (key, Canonical(value))))
            }
            serde_json::Value::Object(map) => {
                let mut entries: Vec<_> = map.iter().collect();
                entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
                serializer.collect_map(
                    entries
                        .into_iter()
                        .map(|(key, value)| (key, Canonical(value))),
                )
            }
            serde_json::Value::Array(items) => serializer.collect_seq(items.iter().map(Canonical)),
            value => value.serialize(serializer),
        }
    }
}

#[cfg(test)]
mod tests;
