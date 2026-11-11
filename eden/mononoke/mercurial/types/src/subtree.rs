/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use mononoke_types::ChangesetId;
use mononoke_types::MPath;
use mononoke_types::subtree_change::SubtreeChange;
use mononoke_types::subtree_change::SubtreeCrossRepoMerge;
use mononoke_types::subtree_change::SubtreeDeepCopy;
use mononoke_types::subtree_change::SubtreeImport;
use mononoke_types::subtree_change::SubtreeMerge;
use serde_derive::Deserialize;
use serde_derive::Serialize;
use sorted_vector_map::SortedVectorMap;
use thiserror::Error;

use crate::HgChangesetId;

/// Mercurial-encoded subtree changes.  This contains subtree changes as defined
/// in the `subtree` extra field of a commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HgSubtreeChanges {
    pub deep_copies: Vec<HgSubtreeDeepCopy>,
    pub merges: Vec<HgSubtreeMerge>,
    pub imports: Vec<HgSubtreeImport>,
    pub cross_repo_merges: Vec<HgSubtreeCrossRepoMerge>,
}

/// The `subtree` extra contains shallow subtree copies (the `copies` key),
/// which are not supported.
#[derive(Debug, Error)]
#[error("Shallow subtree copies are not supported")]
pub struct ShallowSubtreeCopiesUnsupported;

impl HgSubtreeChanges {
    /// Convert from JSON (as defined in sapling/utils/subtreeutils.py).
    ///
    /// Fails with `ShallowSubtreeCopiesUnsupported` if the JSON contains
    /// shallow subtree copies.
    pub fn from_json(json: &[u8]) -> Result<Self> {
        let all_changes: Vec<HgSubtreeChangesVersion> =
            serde_json::from_slice(json).context("Failed to parse subtree branch info")?;
        let mut deep_copies = Vec::new();
        let mut merges = Vec::new();
        let mut imports = Vec::new();
        let mut cross_repo_merges = Vec::new();
        for changes in all_changes {
            if !changes.copies.is_empty() {
                return Err(ShallowSubtreeCopiesUnsupported.into());
            }
            deep_copies.extend(changes.deepcopies);
            merges.extend(changes.merges);
            imports.extend(changes.imports);
            cross_repo_merges.extend(changes.xmerges);
        }
        Ok(HgSubtreeChanges {
            deep_copies,
            merges,
            imports,
            cross_repo_merges,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.deep_copies.is_empty()
            && self.merges.is_empty()
            && self.imports.is_empty()
            && self.cross_repo_merges.is_empty()
    }

    /// Get the set of changeset ids that are referenced by this set of changes.
    /// This does not include ids from external repositories that have been imported.
    pub fn source_changeset_ids(&self) -> HashSet<HgChangesetId> {
        let mut ids = HashSet::new();
        ids.extend(self.deep_copies.iter().map(|copy| copy.from_commit));
        ids.extend(self.merges.iter().map(|merge| merge.from_commit));
        ids
    }

    /// Convert to JSON (as defined in sapling/utils/subtreeutils.py).
    pub fn to_json(&self) -> Result<String> {
        if self.is_empty() {
            return Ok("[]".to_string());
        }
        // Currently we only support version 1.
        let all_changes = vec![HgSubtreeChangesVersion {
            copies: Vec::new(),
            deepcopies: self.deep_copies.clone(),
            merges: self.merges.clone(),
            imports: self.imports.clone(),
            xmerges: self.cross_repo_merges.clone(),
            version: 1,
        }];
        let json = serde_json::to_string(&all_changes).context("Failed to serialize changes")?;
        Ok(json)
    }

    pub fn from_bonsai_subtree_changes(
        subtree_changes: &SortedVectorMap<MPath, SubtreeChange>,
        subtree_change_sources: HashMap<ChangesetId, HgChangesetId>,
    ) -> Result<Option<Self>> {
        if subtree_changes.is_empty() {
            return Ok(None);
        }
        let mut deep_copies = Vec::new();
        let mut merges = Vec::new();
        let mut imports = Vec::new();
        let mut cross_repo_merges = Vec::new();
        for (path, change) in subtree_changes.iter() {
            match change {
                SubtreeChange::SubtreeDeepCopy(SubtreeDeepCopy {
                    from_path,
                    from_cs_id,
                }) => {
                    deep_copies.push(HgSubtreeDeepCopy {
                        from_path: from_path.clone(),
                        to_path: path.clone(),
                        from_commit: subtree_change_sources
                            .get(from_cs_id)
                            .ok_or_else(|| anyhow!("Subtree copy source {from_cs_id} not found"))?
                            .clone(),
                    });
                }
                SubtreeChange::SubtreeMerge(SubtreeMerge {
                    from_path,
                    from_cs_id,
                }) => {
                    merges.push(HgSubtreeMerge {
                        from_path: from_path.clone(),
                        to_path: path.clone(),
                        from_commit: subtree_change_sources
                            .get(from_cs_id)
                            .ok_or_else(|| anyhow!("Subtree copy source {from_cs_id} not found"))?
                            .clone(),
                    });
                }
                SubtreeChange::SubtreeImport(SubtreeImport {
                    from_path,
                    from_commit,
                    from_repo_url,
                }) => {
                    imports.push(HgSubtreeImport {
                        from_path: from_path.clone(),
                        to_path: path.clone(),
                        from_commit: from_commit.clone(),
                        url: from_repo_url.clone(),
                    });
                }
                SubtreeChange::SubtreeCrossRepoMerge(SubtreeCrossRepoMerge {
                    from_path,
                    from_commit,
                    from_repo_url,
                }) => {
                    cross_repo_merges.push(HgSubtreeCrossRepoMerge {
                        from_path: from_path.clone(),
                        to_path: path.clone(),
                        from_commit: from_commit.clone(),
                        url: from_repo_url.clone(),
                    });
                }
            }
        }
        Ok(Some(HgSubtreeChanges {
            deep_copies,
            merges,
            imports,
            cross_repo_merges,
        }))
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct HgSubtreeChangesVersion {
    #[serde(default, skip_serializing)]
    copies: Vec<serde::de::IgnoredAny>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    deepcopies: Vec<HgSubtreeDeepCopy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    merges: Vec<HgSubtreeMerge>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    imports: Vec<HgSubtreeImport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    xmerges: Vec<HgSubtreeCrossRepoMerge>,
    #[serde(rename = "v", with = "version")]
    #[allow(unused)]
    version: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct HgSubtreeDeepCopy {
    #[serde(with = "hg_changeset_id")]
    pub from_commit: HgChangesetId,
    #[serde(with = "mpath")]
    pub from_path: MPath,
    #[serde(with = "mpath")]
    pub to_path: MPath,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct HgSubtreeMerge {
    #[serde(with = "hg_changeset_id")]
    pub from_commit: HgChangesetId,
    #[serde(with = "mpath")]
    pub from_path: MPath,
    #[serde(with = "mpath")]
    pub to_path: MPath,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct HgSubtreeImport {
    pub from_commit: String,
    #[serde(with = "mpath")]
    pub from_path: MPath,
    #[serde(with = "mpath")]
    pub to_path: MPath,
    pub url: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct HgSubtreeCrossRepoMerge {
    pub from_commit: String,
    #[serde(with = "mpath")]
    pub from_path: MPath,
    #[serde(with = "mpath")]
    pub to_path: MPath,
    pub url: String,
}

mod version {
    use serde::Deserialize;
    use serde::de::Error;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let version = u64::deserialize(deserializer)?;
        if version != 1 {
            return Err(D::Error::custom(format!(
                "Unsupported version of subtree changes: {version}"
            )));
        }
        Ok(version)
    }

    pub fn serialize<S>(version: &u64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(*version)
    }
}

pub(crate) mod mpath {
    use mononoke_types::MPath;
    use serde::Deserialize;
    use serde::de::Error;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<MPath, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let path = String::deserialize(deserializer)?;
        MPath::new(path.as_bytes())
            .map_err(|e| D::Error::custom(format!("Invalid path: {path}: {e}")))
    }

    pub fn serialize<S>(path: &MPath, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let path = path.to_string();
        serializer.serialize_str(&path)
    }
}

mod hg_changeset_id {
    use std::str::FromStr;

    use serde::Deserialize;
    use serde::de::Error;

    use crate::HgChangesetId;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<HgChangesetId, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let id = String::deserialize(deserializer)?;
        HgChangesetId::from_str(&id)
            .map_err(|e| D::Error::custom(format!("Invalid changeset id: {id}: {e}")))
    }

    pub fn serialize<S>(id: &HgChangesetId, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let id = id.to_string();
        serializer.serialize_str(&id)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mononoke_macros::mononoke;
    use mononoke_types::MPath;

    use super::*;

    fn compare(changes: HgSubtreeChanges, expected: &str) {
        let json = changes.to_json().unwrap();
        assert_eq!(json, expected);
        let parsed = HgSubtreeChanges::from_json(json.as_bytes()).unwrap();
        assert_eq!(changes, parsed);
    }

    #[mononoke::test]
    fn test_roundtrip() {
        compare(HgSubtreeChanges::default(), "[]");
        compare(
            HgSubtreeChanges {
                deep_copies: vec![HgSubtreeDeepCopy {
                    from_path: MPath::new("e").unwrap(),
                    to_path: MPath::new("f").unwrap(),
                    from_commit: HgChangesetId::from_str(
                        "cccccccccccccccccccccccccccccccccccccccc",
                    )
                    .unwrap(),
                }],
                merges: vec![HgSubtreeMerge {
                    from_path: MPath::new("g").unwrap(),
                    to_path: MPath::new("h").unwrap(),
                    from_commit: HgChangesetId::from_str(
                        "dddddddddddddddddddddddddddddddddddddddd",
                    )
                    .unwrap(),
                }],
                imports: vec![HgSubtreeImport {
                    from_path: MPath::new("i").unwrap(),
                    to_path: MPath::new("j").unwrap(),
                    from_commit: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".to_string(),
                    url: "other:repo".to_string(),
                }],
                cross_repo_merges: vec![HgSubtreeCrossRepoMerge {
                    from_path: MPath::new("k").unwrap(),
                    to_path: MPath::new("l").unwrap(),
                    from_commit: "ffffffffffffffffffffffffffffffffffffffff".to_string(),
                    url: "xmerge:repo".to_string(),
                }],
            },
            concat!(
                r##"[{"deepcopies":[{"from_commit":"cccccccccccccccccccccccccccccccccccccccc","from_path":"e","to_path":"f"}],"##,
                r##""merges":[{"from_commit":"dddddddddddddddddddddddddddddddddddddddd","from_path":"g","to_path":"h"}],"##,
                r##""imports":[{"from_commit":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee","from_path":"i","to_path":"j","url":"other:repo"}],"##,
                r##""xmerges":[{"from_commit":"ffffffffffffffffffffffffffffffffffffffff","from_path":"k","to_path":"l","url":"xmerge:repo"}],"##,
                r##""v":1}]"##
            ),
        );
    }

    #[mononoke::test]
    fn test_unsupported_version() {
        let json = r##"[{"deepcopies":[{"from_commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","from_path":"a","to_path":"b"}],"v":2}]"##;
        let changes = HgSubtreeChanges::from_json(json.as_bytes());
        assert!(changes.is_err());
    }

    #[mononoke::test]
    fn test_shallow_copies_unsupported() {
        let json = r##"[{"copies":[{"from_commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","from_path":"a","to_path":"b"}],"deepcopies":[{"from_commit":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","from_path":"c","to_path":"d"}],"v":1}]"##;
        let err = HgSubtreeChanges::from_json(json.as_bytes())
            .expect_err("shallow subtree copies should be rejected");
        assert!(
            err.downcast_ref::<ShallowSubtreeCopiesUnsupported>()
                .is_some(),
            "unexpected error: {err:?}"
        );
    }
}
