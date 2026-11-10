/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! The compiled policy of retired blob families.
//!
//! Only keys matching a recipe registered here can be selected for deletion.
//! A recipe is an exact grammar: the canonical repository prefix, a fixed
//! stem, and a 64-digit lowercase hexadecimal hash ending the logical key.
//! Adding a family is a reviewed code change, never a runtime option.

use std::collections::BTreeSet;

use anyhow::Result;
use anyhow::bail;
use clap::ValueEnum;
use mononoke_types::DerivableType;
use mononoke_types::RepositoryId;
use serde::Deserialize;
use serde::Serialize;
use strum::Display;

/// Identifies this policy in plans, so a change to the registered recipes
/// invalidates plans made before it.
pub const POLICY_VERSION: &str = "retired-blob-policy-v1";

/// Length of the hexadecimal hash that ends every registered key.
const HASH_HEX_LEN: usize = 64;

/// A derived data type whose stored blobs and mappings are retired.  It is
/// displayed by its command-line name.
#[derive(
    Clone,
    Copy,
    Debug,
    Display,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    ValueEnum
)]
#[strum(serialize_all = "kebab-case")]
pub enum RetiredBlobType {
    HistoryManifests,
    FastlogV2,
    BlameV3,
}

impl RetiredBlobType {
    pub fn all() -> impl Iterator<Item = RetiredBlobType> {
        Self::value_variants().iter().copied()
    }

    pub fn derivable_type(self) -> DerivableType {
        match self {
            RetiredBlobType::HistoryManifests => DerivableType::HistoryManifests,
            RetiredBlobType::FastlogV2 => DerivableType::FastlogV2,
            RetiredBlobType::BlameV3 => DerivableType::BlameV3,
        }
    }

    /// Every `derived_data_version` any released writer stored in the
    /// `commit_derived_data` mapping for this type.
    pub fn mapping_versions(self) -> &'static [i32] {
        match self {
            RetiredBlobType::HistoryManifests => &[1],
            RetiredBlobType::FastlogV2 => &[1],
            // Version 2 also moved the version into the blob key.
            RetiredBlobType::BlameV3 => &[1, 2],
        }
    }

    /// Key families, after the repository prefix, that hold this type's
    /// blobs.  Every key under one of these must match a registered recipe:
    /// anything else is an unknown format that blocks completion.
    pub fn audit_families(self) -> &'static [&'static str] {
        match self {
            RetiredBlobType::HistoryManifests => &["historymf.", "derived_root_history_manifest."],
            RetiredBlobType::FastlogV2 => &["fastlogbatchv2."],
            RetiredBlobType::BlameV3 => &["blame_v3."],
        }
    }

    /// Types whose stored data refers to this type's blobs, and so must be
    /// retired with it.
    pub fn dependents(self) -> &'static [RetiredBlobType] {
        match self {
            RetiredBlobType::HistoryManifests => {
                &[RetiredBlobType::FastlogV2, RetiredBlobType::BlameV3]
            }
            RetiredBlobType::FastlogV2 | RetiredBlobType::BlameV3 => &[],
        }
    }

    pub fn recipes(self) -> impl Iterator<Item = &'static KeyRecipe> {
        RECIPES
            .iter()
            .filter(move |recipe| recipe.blob_type == self)
    }
}

/// Whether a recipe is still produced by current code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecipeStatus {
    Current,
    /// Written by earlier code only.
    Legacy,
}

/// One exact key grammar: `repo{id:04}.` + `stem` + 64 lowercase hex digits.
#[derive(Debug, PartialEq, Eq)]
pub struct KeyRecipe {
    /// Stable name, used in plans and state directory paths.
    pub id: &'static str,
    pub blob_type: RetiredBlobType,
    pub stem: &'static str,
    pub status: RecipeStatus,
}

/// Every registered recipe.  No stem is a prefix of another.
pub static RECIPES: &[KeyRecipe] = &[
    KeyRecipe {
        id: "historymf.file",
        blob_type: RetiredBlobType::HistoryManifests,
        stem: "historymf.file.blake2.",
        status: RecipeStatus::Current,
    },
    KeyRecipe {
        id: "historymf.dir",
        blob_type: RetiredBlobType::HistoryManifests,
        stem: "historymf.dir.blake2.",
        status: RecipeStatus::Current,
    },
    KeyRecipe {
        id: "historymf.delnode",
        blob_type: RetiredBlobType::HistoryManifests,
        stem: "historymf.delnode.blake2.",
        status: RecipeStatus::Current,
    },
    KeyRecipe {
        id: "historymf.map2node",
        blob_type: RetiredBlobType::HistoryManifests,
        stem: "historymf.map2node.blake2.",
        status: RecipeStatus::Current,
    },
    // Root mappings were stored as blobs keyed by changeset id, without a
    // mapping key prefix, until they moved to `commit_derived_data`.
    KeyRecipe {
        id: "derived_root_history_manifest",
        blob_type: RetiredBlobType::HistoryManifests,
        stem: "derived_root_history_manifest.",
        status: RecipeStatus::Legacy,
    },
    KeyRecipe {
        id: "fastlogbatchv2.hmfile",
        blob_type: RetiredBlobType::FastlogV2,
        stem: "fastlogbatchv2.hmfile.",
        status: RecipeStatus::Current,
    },
    KeyRecipe {
        id: "fastlogbatchv2.hmdir",
        blob_type: RetiredBlobType::FastlogV2,
        stem: "fastlogbatchv2.hmdir.",
        status: RecipeStatus::Current,
    },
    KeyRecipe {
        id: "blame_v3.v2",
        blob_type: RetiredBlobType::BlameV3,
        stem: "blame_v3.v2.historymf.file.blake2.",
        status: RecipeStatus::Current,
    },
    // Before the version was part of the key.
    KeyRecipe {
        id: "blame_v3.unversioned",
        blob_type: RetiredBlobType::BlameV3,
        stem: "blame_v3.historymf.file.blake2.",
        status: RecipeStatus::Legacy,
    },
];

/// A logical blobstore key matching a registered recipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetiredKey<'a> {
    pub repo_id: RepositoryId,
    pub recipe: &'static KeyRecipe,
    pub hash: &'a str,
}

impl RetiredKey<'_> {
    /// The partition of the key's recipe holding it: the first `digits` hex
    /// digits of its hash.
    pub fn partition(&self, digits: u8) -> &str {
        &self.hash[..usize::from(digits)]
    }
}

/// How a logical key is stored in a physical store.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize
)]
pub enum PhysicalEncoding {
    /// Stored under the logical key itself.
    Plain,
    /// Stored under the logical key plus the packblob envelope suffix.
    PackEnvelope,
}

impl PhysicalEncoding {
    pub fn physical_key(self, logical: &str) -> String {
        match self {
            PhysicalEncoding::Plain => logical.to_string(),
            PhysicalEncoding::PackEnvelope => format!("{logical}{}", packblob::ENVELOPE_SUFFIX),
        }
    }
}

/// Splits a key into its repository and the rest, accepting only the
/// canonical prefix `repo{id:04}.` that `RepositoryId::prefix` produces.
fn parse_repo_prefix(key: &str) -> Option<(RepositoryId, &str)> {
    let rest = key.strip_prefix("repo")?;
    let (digits, rest) = rest.split_once('.')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let repo_id = RepositoryId::new(digits.parse().ok()?);
    let prefix_len = "repo".len() + digits.len() + 1;
    if repo_id.prefix() != key[..prefix_len] {
        return None;
    }
    Some((repo_id, rest))
}

fn is_hash(s: &str) -> bool {
    s.len() == HASH_HEX_LEN && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Matches a logical key against the registered recipes.
pub fn parse_logical_key(key: &str) -> Option<RetiredKey<'_>> {
    let (repo_id, rest) = parse_repo_prefix(key)?;
    let recipe = RECIPES
        .iter()
        .find(|recipe| rest.starts_with(recipe.stem))?;
    let hash = &rest[recipe.stem.len()..];
    is_hash(hash).then_some(RetiredKey {
        repo_id,
        recipe,
        hash,
    })
}

/// Matches a physical key, which may carry the packblob envelope suffix.
pub fn parse_physical_key(key: &str) -> Option<(RetiredKey<'_>, PhysicalEncoding)> {
    match key.strip_suffix(packblob::ENVELOPE_SUFFIX) {
        Some(logical) => {
            parse_logical_key(logical).map(|parsed| (parsed, PhysicalEncoding::PackEnvelope))
        }
        None => parse_logical_key(key).map(|parsed| (parsed, PhysicalEncoding::Plain)),
    }
}

/// The set of retired types a run deletes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    types: BTreeSet<RetiredBlobType>,
}

impl Selection {
    /// Builds a selection, rejecting one that would leave a selected type's
    /// dependents referring to deleted blobs.
    pub fn new(types: impl IntoIterator<Item = RetiredBlobType>) -> Result<Self> {
        let types: BTreeSet<_> = types.into_iter().collect();
        if types.is_empty() {
            bail!("select at least one retired type");
        }
        for blob_type in &types {
            for dependent in blob_type.dependents() {
                if !types.contains(dependent) {
                    bail!(
                        "{blob_type} cannot be retired without {dependent}, which refers to its blobs"
                    );
                }
            }
        }
        Ok(Self { types })
    }

    pub fn types(&self) -> impl Iterator<Item = RetiredBlobType> + '_ {
        self.types.iter().copied()
    }

    pub fn contains(&self, blob_type: RetiredBlobType) -> bool {
        self.types.contains(&blob_type)
    }

    pub fn recipes(&self) -> impl Iterator<Item = &'static KeyRecipe> + '_ {
        RECIPES
            .iter()
            .filter(|recipe| self.types.contains(&recipe.blob_type))
    }

    /// Whether `key` is a logical key of `repo_id` selected by this run.
    pub fn selects(&self, repo_id: RepositoryId, key: &str) -> bool {
        parse_logical_key(key).is_some_and(|parsed| {
            parsed.repo_id == repo_id && self.contains(parsed.recipe.blob_type)
        })
    }
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;

    use super::*;

    const H: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn key(prefix: &str, stem: &str) -> String {
        format!("{prefix}{stem}{H}")
    }

    #[mononoke::test]
    fn current_key_builders_match_recipes() {
        let hm_file = mononoke_types::HistoryManifestFileId::from_bytes([0x5a; 32])
            .expect("32 bytes is a valid blake2 hash");
        let hm_dir = mononoke_types::HistoryManifestDirectoryId::from_bytes([0xa5; 32])
            .expect("32 bytes is a valid blake2 hash");
        let repo = RepositoryId::new(2100);
        let cases = [
            (
                mononoke_types::BlobstoreKey::blobstore_key(&hm_file),
                "historymf.file",
            ),
            (
                mononoke_types::BlobstoreKey::blobstore_key(&hm_dir),
                "historymf.dir",
            ),
            (
                mononoke_types::blame_v3::BlameV3Id::from(hm_file).blobstore_key(),
                "blame_v3.v2",
            ),
            (
                format!("fastlogbatchv2.hmfile.{hm_file}"),
                "fastlogbatchv2.hmfile",
            ),
            (
                format!("fastlogbatchv2.hmdir.{hm_dir}"),
                "fastlogbatchv2.hmdir",
            ),
        ];
        for (logical, recipe_id) in cases {
            let full = format!("{}{logical}", repo.prefix());
            let parsed =
                parse_logical_key(&full).unwrap_or_else(|| panic!("{full} should match a recipe"));
            assert_eq!(parsed.recipe.id, recipe_id);
            assert_eq!(parsed.repo_id, repo);
        }
    }

    #[mononoke::test]
    fn every_recipe_matches_and_parses() {
        for recipe in RECIPES {
            let k = key("repo0007.", recipe.stem);
            let parsed = parse_logical_key(&k).expect("recipe key should parse");
            assert_eq!(parsed.recipe, recipe);
            assert_eq!(parsed.repo_id, RepositoryId::new(7));
            assert_eq!(parsed.hash, H);
            let physical = PhysicalEncoding::PackEnvelope.physical_key(&k);
            assert_eq!(physical, format!("{k}.pack"));
            assert_eq!(
                parse_physical_key(&physical),
                Some((parsed, PhysicalEncoding::PackEnvelope))
            );
            assert_eq!(parsed.partition(0), "");
            assert_eq!(parsed.partition(2), "01");
            assert_eq!(
                parse_physical_key(&k),
                Some((parsed, PhysicalEncoding::Plain))
            );
        }
    }

    #[mononoke::test]
    fn stems_are_prefix_free_and_within_audit_families() {
        for a in RECIPES {
            for b in RECIPES {
                if a.id != b.id {
                    assert!(
                        !a.stem.starts_with(b.stem),
                        "{} must not be a prefix of {}",
                        b.stem,
                        a.stem
                    );
                }
            }
            assert!(
                a.blob_type
                    .audit_families()
                    .iter()
                    .any(|family| a.stem.starts_with(family)),
                "{} must be inside one of its type's audit families",
                a.id
            );
        }
        let ids: BTreeSet<_> = RECIPES.iter().map(|recipe| recipe.id).collect();
        assert_eq!(ids.len(), RECIPES.len(), "recipe ids must be unique");
    }

    #[mononoke::test]
    fn repo_prefix_must_be_canonical() {
        assert_eq!(
            parse_repo_prefix("repo0000.x"),
            Some((RepositoryId::new(0), "x"))
        );
        assert_eq!(
            parse_repo_prefix("repo2100.x"),
            Some((RepositoryId::new(2100), "x"))
        );
        assert_eq!(
            parse_repo_prefix("repo12000.x"),
            Some((RepositoryId::new(12000), "x"))
        );
        for bad in [
            "repo1.x",
            "repo001.x",
            "repo00001.x",
            "repo012000.x",
            "repo.x",
            "repo-001.x",
            "repo00a1.x",
            "Repo0001.x",
            "eph1.repo0001.x",
            "flat/repo0001.x",
            "repo99999999999.x",
            "repo0001",
        ] {
            assert_eq!(parse_repo_prefix(bad), None, "{bad} must be rejected");
        }
    }

    #[mononoke::test]
    fn neighbouring_families_and_malformed_keys_do_not_match() {
        let upper = H.to_uppercase();
        let not_selected = [
            // Active neighbouring families.
            key("repo0001.", "fastlogbatch.blake2."),
            key("repo0001.", "fileunode.blake2."),
            key("repo0001.", "manifestunode.blake2."),
            key("repo0001.", "fsnode.blake2."),
            key("repo0001.", "content.blake2."),
            key("repo0001.", "contentmf.map2node.blake2."),
            key("repo0001.", "blame_v2."),
            key("repo0001.", "derived_root_fsnode."),
            key("repo0001.", "derived_root_contentmf."),
            key("repo0001.", "derived_root_blame_v2."),
            // Unregistered variants inside retired families.
            key("repo0001.", "blame_v3.v1.historymf.file.blake2."),
            key("repo0001.", "blame_v3.v20.historymf.file.blake2."),
            key("repo0001.", "historymf.files.blake2."),
            key("repo0001.", "historymf.file.sha1."),
            key("repo0001.", "derived_root_history_manifest.v2."),
            key("repo0001.", "fastlogbatchv2.blake2."),
            // Malformed hashes and suffixes.
            format!("repo0001.historymf.file.blake2.{upper}"),
            format!("repo0001.historymf.file.blake2.{}", &H[1..]),
            format!("repo0001.historymf.file.blake2.{H}0"),
            format!("repo0001.historymf.file.blake2.{H}.extra"),
            format!("repo0001.historymf.file.blake2.{H}.pack.pack"),
            format!("repo0001.historymf.file.blake2.{}g", &H[1..]),
            // Non-canonical or foreign scopes.
            key("repo1.", "historymf.file.blake2."),
            key("repo00001.", "historymf.file.blake2."),
            key("eph12.repo0001.", "historymf.file.blake2."),
            key("flat/repo0001.", "historymf.file.blake2."),
            key("", "historymf.file.blake2."),
            format!("multiblob-{H}.pack"),
        ];
        for k in &not_selected {
            assert_eq!(parse_logical_key(k), None, "{k} must not match");
            assert_eq!(parse_physical_key(k), None, "{k} must not match");
        }
    }

    #[mononoke::test]
    fn only_the_compiled_types_can_be_selected() {
        for name in ["history-manifests", "fastlog-v2", "blame-v3"] {
            let blob_type =
                RetiredBlobType::from_str(name, false).expect("compiled types are selectable");
            assert_eq!(blob_type.to_string(), name, "displayed by its flag name");
        }
        assert_eq!(RetiredBlobType::all().count(), 3);
        for name in [
            "fsnodes", "unodes", "blame", "fastlog", "blame-v2", "content",
        ] {
            assert!(
                RetiredBlobType::from_str(name, true).is_err(),
                "{name} must not be selectable"
            );
        }
    }

    #[mononoke::test]
    fn selection_requires_dependents() {
        assert!(Selection::new([]).is_err());
        assert!(Selection::new([RetiredBlobType::HistoryManifests]).is_err());
        assert!(
            Selection::new([
                RetiredBlobType::HistoryManifests,
                RetiredBlobType::FastlogV2
            ])
            .is_err()
        );
        let all = Selection::new(RetiredBlobType::all()).expect("the full group is valid");
        assert_eq!(all.recipes().count(), RECIPES.len());

        let blame = Selection::new([RetiredBlobType::BlameV3]).expect("a leaf type is valid");
        let blame_key = key("repo0001.", "blame_v3.v2.historymf.file.blake2.");
        let hm_key = key("repo0001.", "historymf.file.blake2.");
        assert!(blame.selects(RepositoryId::new(1), &blame_key));
        assert!(!blame.selects(RepositoryId::new(2), &blame_key));
        assert!(!blame.selects(RepositoryId::new(1), &hm_key));
    }

    #[mononoke::test]
    fn metadata_rules_cover_released_versions() {
        assert_eq!(
            RetiredBlobType::BlameV3.mapping_versions().last(),
            Some(&mononoke_types::blame_v3::BLAME_V3_VERSION)
        );
        for blob_type in RetiredBlobType::all() {
            assert!(!blob_type.mapping_versions().is_empty());
            assert!(blob_type.recipes().count() > 0);
        }
    }
}
