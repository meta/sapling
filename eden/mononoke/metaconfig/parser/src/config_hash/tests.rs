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
use repos::RawRepoConfig;

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

fn spec_on(storage: &str, tier_storage: Option<&str>) -> RepoSpec {
    RepoSpec {
        repo_config: Some(RawRepoConfig {
            storage_config: Some(storage.to_string()),
            ..Default::default()
        }),
        tier_overrides: tier_storage.map(|tier_storage| {
            [(
                "tier".to_string(),
                RawRepoConfig {
                    storage_config: Some(tier_storage.to_string()),
                    ..Default::default()
                },
            )]
            .into()
        }),
        ..Default::default()
    }
}

#[mononoke::test]
fn shared_generation_covers_only_the_named_storage() {
    let plain = RawStorageConfig::default();
    let changed = RawStorageConfig {
        ephemeral_blobstore: Some(Default::default()),
        ..Default::default()
    };
    let generations = |storage: HashMap<String, RawStorageConfig>| {
        SharedGenerations::new(&storage).expect("storage configs hash")
    };
    let before = generations(
        hashmap! { "a".to_string() => plain.clone(), "b".to_string() => plain.clone() },
    );
    let b_changed = generations(
        hashmap! { "a".to_string() => plain.clone(), "b".to_string() => changed.clone() },
    );
    let a_changed =
        generations(hashmap! { "a".to_string() => changed, "b".to_string() => plain.clone() });
    let c_added = generations(hashmap! {
        "a".to_string() => plain.clone(),
        "b".to_string() => plain.clone(),
        "c".to_string() => plain,
    });
    let on_a = spec_on("a", None);

    assert_eq!(
        before.of_repo(&on_a, "tier"),
        b_changed.of_repo(&on_a, "tier"),
        "a change to another storage config must not change the generation",
    );
    assert_eq!(
        before.of_repo(&on_a, "tier"),
        c_added.of_repo(&on_a, "tier"),
        "adding a storage config must not change the generation",
    );
    assert_ne!(
        before.of_repo(&on_a, "tier"),
        a_changed.of_repo(&on_a, "tier"),
        "a change to the named storage config must change the generation",
    );

    let b_on_tier = spec_on("a", Some("b"));
    assert_ne!(
        before.of_repo(&b_on_tier, "tier"),
        b_changed.of_repo(&b_on_tier, "tier"),
        "the tier override's storage config counts on its tier",
    );
    assert_ne!(
        before.of_repo(&b_on_tier, "tier"),
        a_changed.of_repo(&b_on_tier, "tier"),
        "the spec's own storage config counts even where the override names another",
    );
    assert_eq!(
        before.of_repo(&b_on_tier, "other"),
        b_changed.of_repo(&b_on_tier, "other"),
        "another tier's override does not apply",
    );

    let on_missing = spec_on("missing", None);
    assert_ne!(
        before.of_repo(&on_missing, "tier"),
        generations(hashmap! { "missing".to_string() => RawStorageConfig::default() })
            .of_repo(&on_missing, "tier"),
        "a storage config the manifest lacks changes the generation when it is added",
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
