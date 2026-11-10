/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Unit tests for config-drift hashing: content-addressing, order-independence,
//! and canonical-JSON key-order invariance.

use maplit::hashmap;
use mononoke_macros::mononoke;

use super::*;

#[mononoke::test]
fn spec_hash_is_content_addressed() {
    let a = RepoSpec::default();
    assert_eq!(
        spec_hash(&a).unwrap(),
        spec_hash(&RepoSpec::default()).unwrap(),
        "identical specs must hash equal",
    );
    let changed = RepoSpec {
        repo_id: 42,
        ..Default::default()
    };
    assert_ne!(
        spec_hash(&a).unwrap(),
        spec_hash(&changed).unwrap(),
        "a differing field must change the hash",
    );
}

#[mononoke::test]
fn shared_generation_reflects_content_not_order() {
    let cfg = RawStorageConfig::default();
    let m1: HashMap<String, RawStorageConfig> =
        hashmap! { "a".to_string() => cfg.clone(), "b".to_string() => cfg.clone() };
    let m2: HashMap<String, RawStorageConfig> =
        hashmap! { "b".to_string() => cfg.clone(), "a".to_string() => cfg.clone() };
    assert_eq!(
        shared_generation(&m1).unwrap(),
        shared_generation(&m2).unwrap(),
        "shared generation must be independent of insertion order",
    );
    let m3: HashMap<String, RawStorageConfig> = hashmap! { "a".to_string() => cfg };
    assert_ne!(
        shared_generation(&m1).unwrap(),
        shared_generation(&m3).unwrap(),
        "different storage content must change the generation",
    );
}

#[mononoke::test]
fn canonical_json_hash_ignores_key_order() {
    let a = serde_json::json!({"x": 1, "y": {"p": 2, "q": 3}});
    let b = serde_json::json!({"y": {"q": 3, "p": 2}, "x": 1});
    assert_eq!(
        hash_canonical_json(&a),
        hash_canonical_json(&b),
        "object key order must not affect the hash",
    );
    let c = serde_json::json!({"x": 2, "y": {"p": 2, "q": 3}});
    assert_ne!(
        hash_canonical_json(&a),
        hash_canonical_json(&c),
        "a different value must change the hash",
    );
}
