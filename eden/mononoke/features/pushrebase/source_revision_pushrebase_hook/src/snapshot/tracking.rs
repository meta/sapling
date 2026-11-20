/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Per-string tracking metadata: the `introduced_revision` and
//! `last_changed_revision` of each `modules[*].strings[*]` entry.
//!
//! The metadata is derived from the root `source_revision`, so it is not
//! source content: the root comparison leaves it out, at those positions
//! only. Instead each entry's metadata is checked against the commit's actual
//! parent, matching entries by their `key` across every module and file:
//!
//! - an entry whose source (the entry without its metadata) is unchanged
//!   keeps both values, wherever it moved;
//! - an edited entry keeps `introduced_revision` and takes the new root
//!   revision R as `last_changed_revision`;
//! - a new key takes R for both. A key removed and later added again starts
//!   a new lifetime: nothing remembers that it was there before.
//!
//! Metadata is all or nothing across a snapshot, mirrors included. Tracking
//! starts at a known baseline, where every entry is `1`/`1`: either a
//! metadata-only migration of a root-1 snapshot, with the same source, or the
//! first versioned snapshot. The values mean "tracked since", not when a
//! string was first written. A snapshot past root 1 without metadata cannot
//! start tracking, and a tracked snapshot cannot stop.

use std::collections::BTreeMap;

use derive_more::Display;
use mononoke_types::NonRootMPath;
use serde_json::Map;
use serde_json::Value;

use super::SnapshotError;
use super::SnapshotRevision;
use super::SourceRevision;
use super::TransitionError;

/// When an entry was first tracked.
pub const INTRODUCED_REVISION_FIELD: &str = "introduced_revision";
/// When an entry's source last changed.
pub const LAST_CHANGED_REVISION_FIELD: &str = "last_changed_revision";

/// The tracking metadata of one entry.
#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
#[display(
    "{INTRODUCED_REVISION_FIELD} {introduced} and {LAST_CHANGED_REVISION_FIELD} {last_changed}"
)]
pub struct EntryRevisions {
    pub introduced: SourceRevision,
    pub last_changed: SourceRevision,
}

impl EntryRevisions {
    /// Every entry's metadata when tracking starts.
    pub const BASELINE: EntryRevisions = EntryRevisions {
        introduced: SourceRevision::FIRST,
        last_changed: SourceRevision::FIRST,
    };

    fn both(revision: SourceRevision) -> Self {
        Self {
            introduced: revision,
            last_changed: revision,
        }
    }
}

/// How an entry relates to the entry with its key in the parent.
#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
pub enum EntryChange {
    #[display("unchanged")]
    Unchanged,
    #[display("edited")]
    Edited,
    #[display("new")]
    Added,
}

/// One `modules[*].strings[*]` entry of a file.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Entry {
    /// The entry's position and key, for messages.
    name: String,
    key: Option<String>,
    /// The entry without its metadata.
    source: Map<String, Value>,
    revisions: Option<EntryRevisions>,
    /// For a placeholder standing for a part of the catalog that is present
    /// but not the shape that holds entries: the shape it should have. It
    /// cannot carry metadata, so its entries cannot escape the checks.
    malformed: Option<&'static str>,
}

impl Entry {
    fn malformed(location: String, expected: &'static str) -> Self {
        Self {
            name: location,
            key: None,
            source: Map::new(),
            revisions: None,
            malformed: Some(expected),
        }
    }
}

const AN_ARRAY: &str = "an array";
const AN_OBJECT: &str = "an object";

/// `content` without the entries' metadata, and its entries. An absent
/// `modules` or `strings` holds no entries; one that is present but the
/// wrong shape, or a module or string that is not an object, becomes a
/// placeholder entry (see `Entry::malformed`).
pub(crate) fn split_entries(
    path: &NonRootMPath,
    content: &Map<String, Value>,
) -> Result<(Map<String, Value>, Vec<Entry>), SnapshotError> {
    let mut source = content.clone();
    let mut entries = Vec::new();
    let modules = match source.get_mut("modules") {
        None => return Ok((source, entries)),
        Some(Value::Array(modules)) => modules,
        Some(_) => {
            entries.push(Entry::malformed("modules".to_owned(), AN_ARRAY));
            return Ok((source, entries));
        }
    };
    for (module_index, module) in modules.iter_mut().enumerate() {
        let Value::Object(module) = module else {
            entries.push(Entry::malformed(
                format!("modules[{module_index}]"),
                AN_OBJECT,
            ));
            continue;
        };
        let strings = match module.get_mut("strings") {
            None => continue,
            Some(Value::Array(strings)) => strings,
            Some(_) => {
                entries.push(Entry::malformed(
                    format!("modules[{module_index}].strings"),
                    AN_ARRAY,
                ));
                continue;
            }
        };
        for (index, entry) in strings.iter_mut().enumerate() {
            let position = format!("modules[{module_index}].strings[{index}]");
            let Value::Object(entry) = entry else {
                entries.push(Entry::malformed(position, AN_OBJECT));
                continue;
            };
            let key = entry.get("key").and_then(Value::as_str).map(str::to_owned);
            let name = match &key {
                Some(key) => format!("{position} ({key})"),
                None => position,
            };
            let introduced = entry.remove(INTRODUCED_REVISION_FIELD);
            let last_changed = entry.remove(LAST_CHANGED_REVISION_FIELD);
            let revisions = match (introduced, last_changed) {
                (None, None) => None,
                (Some(introduced), Some(last_changed)) => Some(EntryRevisions {
                    introduced: parse_revision(path, &name, INTRODUCED_REVISION_FIELD, introduced)?,
                    last_changed: parse_revision(
                        path,
                        &name,
                        LAST_CHANGED_REVISION_FIELD,
                        last_changed,
                    )?,
                }),
                _ => {
                    return Err(SnapshotError::PartialEntryMetadata {
                        path: path.clone(),
                        entry: name,
                    });
                }
            };
            entries.push(Entry {
                name,
                key,
                source: entry.clone(),
                revisions,
                malformed: None,
            });
        }
    }
    Ok((source, entries))
}

fn parse_revision(
    path: &NonRootMPath,
    entry: &str,
    field: &'static str,
    value: Value,
) -> Result<SourceRevision, SnapshotError> {
    value.as_i64().and_then(SourceRevision::new).ok_or_else(|| {
        SnapshotError::InvalidEntryRevision {
            path: path.clone(),
            entry: entry.to_owned(),
            field,
            value: value.to_string(),
        }
    })
}

/// A tracked entry, by key.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TrackedEntry {
    source: Map<String, Value>,
    revisions: EntryRevisions,
}

/// The tracked entries of a snapshot, by key across every module and file of
/// the authoritative directory; mirrors are checked to be identical.
pub(crate) type TrackedEntries = BTreeMap<String, TrackedEntry>;

/// The tracked entries of a snapshot whose files hold `entries`, or `None` if
/// no entry has metadata.
pub(crate) fn tracked_entries<'a>(
    authoritative: Option<&NonRootMPath>,
    entries: impl Iterator<Item = (&'a NonRootMPath, &'a [Entry])> + Clone,
    revision: SnapshotRevision,
) -> Result<Option<TrackedEntries>, SnapshotError> {
    let each = || {
        entries
            .clone()
            .flat_map(|(path, entries)| entries.iter().map(move |entry| (path, entry)))
    };
    let tracked = each().find(|(_, entry)| entry.revisions.is_some());
    let Some((path, entry)) = tracked else {
        // An untracked snapshot keeps the root-only rules, placeholders
        // included.
        return Ok(None);
    };
    // Before anything else, so that no entry hides from the checks below.
    if let Some((malformed_path, malformed)) = each().find(|(_, entry)| entry.malformed.is_some()) {
        return Err(SnapshotError::MalformedCatalog {
            path: malformed_path.clone(),
            location: malformed.name.clone(),
            expected: malformed.malformed.unwrap_or(AN_OBJECT),
        });
    }
    if let Some((untracked_path, untracked)) = each().find(|(_, entry)| entry.revisions.is_none()) {
        return Err(SnapshotError::PartialMetadata {
            tracked: format!("{path} {}", entry.name),
            untracked: format!("{untracked_path} {}", untracked.name),
        });
    }
    let SnapshotRevision::Versioned(root) = revision else {
        return Err(SnapshotError::MetadataWithoutRevision {
            path: path.clone(),
            entry: entry.name.clone(),
        });
    };

    let mut by_key: BTreeMap<String, (&NonRootMPath, &str, TrackedEntry)> = BTreeMap::new();
    let authoritative_entries =
        each().filter(|(path, _)| path.split_dirname().0.as_ref() == authoritative);
    for (path, entry) in authoritative_entries {
        let (Some(key), Some(revisions)) = (&entry.key, entry.revisions) else {
            return Err(SnapshotError::EntryWithoutKey {
                path: path.clone(),
                entry: entry.name.clone(),
            });
        };
        if !(revisions.introduced <= revisions.last_changed && revisions.last_changed <= root) {
            return Err(SnapshotError::EntryRevisionsOutOfRange {
                path: path.clone(),
                entry: entry.name.clone(),
                revisions,
                revision: root,
            });
        }
        if let Some((other_path, other_entry, _)) = by_key.get(key) {
            return Err(SnapshotError::DuplicateKey {
                key: key.clone(),
                path: path.clone(),
                entry: entry.name.clone(),
                other_path: (*other_path).clone(),
                other_entry: (*other_entry).to_owned(),
            });
        }
        by_key.insert(
            key.clone(),
            (
                path,
                &entry.name,
                TrackedEntry {
                    source: entry.source.clone(),
                    revisions,
                },
            ),
        );
    }
    Ok(Some(
        by_key
            .into_iter()
            .map(|(key, (_, _, entry))| (key, entry))
            .collect(),
    ))
}

/// The tracking rules for `candidate` after `parent`, before the root
/// revision rules: whether tracking may start, continue or stop.
pub(crate) fn check_tracking_mode(
    parent_revision: SnapshotRevision,
    parent: Option<&TrackedEntries>,
    candidate: Option<&TrackedEntries>,
    source_changed: bool,
) -> Result<(), TransitionError> {
    let candidate = match (parent, candidate) {
        (None, None) | (Some(_), Some(_)) => return Ok(()),
        (Some(_), None) => return Err(TransitionError::MetadataRemoved),
        (None, Some(candidate)) => candidate,
    };
    match parent_revision {
        // The first versioned snapshot: the root rules allow only revision 1.
        SnapshotRevision::Unversioned => {}
        SnapshotRevision::Versioned(revision) if revision == SourceRevision::FIRST => {
            if source_changed {
                return Err(TransitionError::MetadataMigrationChangesSource);
            }
        }
        SnapshotRevision::Versioned(parent_revision) => {
            return Err(TransitionError::MetadataBootstrapUnsupported { parent_revision });
        }
    }
    candidate
        .iter()
        .find(|(_, entry)| entry.revisions != EntryRevisions::BASELINE)
        .map_or(Ok(()), |(key, entry)| {
            Err(TransitionError::MetadataNotBaseline {
                key: key.clone(),
                found: entry.revisions,
            })
        })
}

/// Each tracked entry of `candidate`, at root revision `revision`, against
/// the entry with its key in `parent`.
pub(crate) fn check_entries(
    parent: &TrackedEntries,
    candidate: &TrackedEntries,
    revision: SourceRevision,
) -> Result<(), TransitionError> {
    for (key, entry) in candidate {
        let (change, expected) = match parent.get(key) {
            Some(before) if before.source == entry.source => {
                (EntryChange::Unchanged, before.revisions)
            }
            Some(before) => (
                EntryChange::Edited,
                EntryRevisions {
                    introduced: before.revisions.introduced,
                    last_changed: revision,
                },
            ),
            None => (EntryChange::Added, EntryRevisions::both(revision)),
        };
        if entry.revisions != expected {
            return Err(TransitionError::MetadataMismatch {
                key: key.clone(),
                change,
                expected,
                found: entry.revisions,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;
    use serde_json::json;

    use super::super::ParsedSnapshot;
    use super::super::check_transition;
    use super::super::parse_file;
    use super::super::parse_snapshot;
    use super::*;

    const WWW: &str = "www/generated";
    const COMMON: &str = "common/generated";

    fn path(path: &str) -> NonRootMPath {
        NonRootMPath::new(path).expect("test paths are valid")
    }

    fn revision(value: i64) -> SourceRevision {
        SourceRevision::new(value).expect("test revisions are valid")
    }

    fn revisions(introduced: i64, last_changed: i64) -> EntryRevisions {
        EntryRevisions {
            introduced: revision(introduced),
            last_changed: revision(last_changed),
        }
    }

    /// A string entry, tracked when `revisions` is given.
    fn entry(key: &str, text: &str, tracked: Option<(i64, i64)>) -> Value {
        let mut entry = json!({"key": key, "en_string": text, "comment": "c"});
        if let Some((introduced, last_changed)) = tracked {
            entry[INTRODUCED_REVISION_FIELD] = json!(introduced);
            entry[LAST_CHANGED_REVISION_FIELD] = json!(last_changed);
        }
        entry
    }

    /// A file of one module, as JSON text.
    fn file(root: Option<i64>, module: &str, strings: Vec<Value>) -> String {
        let mut file = json!({
            "modules": [{"key": module, "description": "d", "strings": strings}],
        });
        if let Some(root) = root {
            file[super::super::SOURCE_REVISION_FIELD] = json!(root);
        }
        file.to_string()
    }

    /// A snapshot of `files`, by name, each mirrored.
    fn mirrored(files: &[(&str, String)]) -> Result<ParsedSnapshot, SnapshotError> {
        let files: Vec<(String, String)> = files
            .iter()
            .flat_map(|(name, json)| {
                [WWW, COMMON].map(|dir| (format!("{dir}/{name}.json"), json.clone()))
            })
            .collect();
        raw(&files)
    }

    fn raw(files: &[(String, String)]) -> Result<ParsedSnapshot, SnapshotError> {
        let files = files
            .iter()
            .map(|(name, json)| {
                let name = path(name);
                let file = parse_file(&name, json.as_bytes())?;
                Ok((name, file))
            })
            .collect::<Result<_, SnapshotError>>()?;
        parse_snapshot(&[path(WWW), path(COMMON)], files)
    }

    fn snapshot(files: &[(&str, String)]) -> ParsedSnapshot {
        mirrored(files).expect("test snapshot is valid")
    }

    /// Root revision `root`, with `a` (`A`) and `b` (`B`) in module `m`.
    fn ab(root: i64, a: (i64, i64), b: (i64, i64)) -> ParsedSnapshot {
        snapshot(&[(
            "m",
            file(
                Some(root),
                "m",
                vec![entry("A", "a", Some(a)), entry("B", "b", Some(b))],
            ),
        )])
    }

    fn mismatch(
        key: &str,
        change: EntryChange,
        expected: (i64, i64),
        found: (i64, i64),
    ) -> TransitionError {
        TransitionError::MetadataMismatch {
            key: key.to_owned(),
            change,
            expected: revisions(expected.0, expected.1),
            found: revisions(found.0, found.1),
        }
    }

    #[mononoke::test]
    fn test_metadata_is_left_out_of_the_root_content_only_at_entries() {
        // Different entry metadata, same root content.
        let one = snapshot(&[("m", file(Some(1), "m", vec![entry("A", "a", Some((1, 1)))]))]);
        let other = snapshot(&[("m", file(Some(1), "m", vec![entry("A", "a", Some((1, 1)))]))]);
        assert_eq!(one.source, other.source);
        let edited = ab(3, (1, 3), (1, 2));
        let tampered = ab(3, (2, 3), (1, 1));
        assert_eq!(edited.source, tampered.source);

        // The same names elsewhere are content: at the root, in a module, or
        // nested in an entry.
        let at = |root_field: i64, module_field: i64, nested: i64| {
            let json = json!({
                "source_revision": 1,
                "introduced_revision": root_field,
                "modules": [{
                    "key": "m",
                    "last_changed_revision": module_field,
                    "strings": [{
                        "key": "A",
                        "extra": {"introduced_revision": nested},
                        "introduced_revision": 1,
                        "last_changed_revision": 1,
                    }],
                }],
            })
            .to_string();
            snapshot(&[("m", json)])
        };
        let base = at(1, 1, 1);
        assert_ne!(base.source, at(2, 1, 1).source);
        assert_ne!(base.source, at(1, 2, 1).source);
        assert_ne!(base.source, at(1, 1, 2).source);
        assert!(base.is_tracked());
    }

    #[mononoke::test]
    fn test_tracking_starts_only_at_the_baseline() {
        let legacy = |root: Option<i64>, text: &str| {
            snapshot(&[(
                "m",
                file(
                    root,
                    "m",
                    vec![entry("A", text, None), entry("B", "b", None)],
                ),
            )])
        };
        let baseline = |root: i64, text: &str| {
            snapshot(&[(
                "m",
                file(
                    Some(root),
                    "m",
                    vec![
                        entry("A", text, Some((1, 1))),
                        entry("B", "b", Some((1, 1))),
                    ],
                ),
            )])
        };

        // The metadata-only migration of root 1.
        assert_eq!(
            check_transition(&legacy(Some(1), "a"), &baseline(1, "a")),
            Ok(())
        );
        // Not combined with a source change, at either revision.
        assert_eq!(
            check_transition(&legacy(Some(1), "a"), &baseline(1, "edited")),
            Err(TransitionError::MetadataMigrationChangesSource)
        );
        assert_eq!(
            check_transition(&legacy(Some(1), "a"), &baseline(2, "edited")),
            Err(TransitionError::MetadataMigrationChangesSource)
        );
        // Every entry starts at 1 and 1.
        let not_baseline = snapshot(&[(
            "m",
            file(
                Some(2),
                "m",
                vec![entry("A", "a", Some((2, 2))), entry("B", "b", Some((1, 1)))],
            ),
        )]);
        assert_eq!(
            check_transition(&legacy(Some(1), "a"), &not_baseline),
            Err(TransitionError::MetadataNotBaseline {
                key: "A".to_owned(),
                found: revisions(2, 2),
            })
        );
        // The first versioned snapshot may start tracking, with any content.
        assert_eq!(
            check_transition(&legacy(None, "a"), &baseline(1, "edited")),
            Ok(())
        );
        assert_eq!(
            check_transition(&legacy(None, "a"), &baseline(2, "a")),
            Err(TransitionError::InvalidInitialRevision {
                revision: revision(2)
            })
        );
        // Past root 1 there is no known baseline.
        for (root, text) in [(3, "a"), (4, "edited")] {
            assert_eq!(
                check_transition(&legacy(Some(3), "a"), &baseline(root, text)),
                Err(TransitionError::MetadataBootstrapUnsupported {
                    parent_revision: revision(3)
                })
            );
        }
        // Untracked snapshots keep the root-only rules.
        assert_eq!(
            check_transition(&legacy(Some(3), "a"), &legacy(Some(4), "edited")),
            Ok(())
        );
    }

    #[mononoke::test]
    fn test_tracking_never_stops() {
        let tracked = ab(2, (1, 1), (1, 2));
        let untracked = snapshot(&[(
            "m",
            file(
                Some(3),
                "m",
                vec![entry("A", "a", None), entry("B", "x", None)],
            ),
        )]);
        assert_eq!(
            check_transition(&tracked, &untracked),
            Err(TransitionError::MetadataRemoved)
        );
        // No entries at all is not a way out.
        let empty = snapshot(&[("m", file(Some(3), "m", vec![]))]);
        assert!(!empty.is_tracked());
        assert_eq!(
            check_transition(&tracked, &empty),
            Err(TransitionError::MetadataRemoved)
        );
        // Neither is deleting the files.
        assert_eq!(
            check_transition(&tracked, &mirrored(&[]).expect("no files is a snapshot")),
            Err(TransitionError::MetadataRemoved)
        );
    }

    #[mononoke::test]
    fn test_metadata_is_all_or_nothing() {
        let tracked = file(Some(1), "m", vec![entry("A", "a", Some((1, 1)))]);
        let untracked = file(Some(1), "n", vec![entry("B", "b", None)]);
        let partial = file(
            Some(1),
            "m",
            vec![entry("A", "a", Some((1, 1))), entry("B", "b", None)],
        );
        assert!(matches!(
            mirrored(&[("m", partial)]),
            Err(SnapshotError::PartialMetadata { .. })
        ));
        assert!(matches!(
            mirrored(&[("m", tracked.clone()), ("n", untracked)]),
            Err(SnapshotError::PartialMetadata { .. })
        ));
        // A string that is not an object cannot carry metadata.
        let not_an_object = json!({
            "source_revision": 1,
            "modules": [{"key": "m", "strings": [entry("A", "a", Some((1, 1))), "B"]}],
        })
        .to_string();
        assert!(matches!(
            mirrored(&[("m", not_an_object)]),
            Err(SnapshotError::MalformedCatalog {
                expected: "an object",
                ..
            })
        ));
        // One of the two fields.
        let mut one_field = entry("A", "a", None);
        one_field[INTRODUCED_REVISION_FIELD] = json!(1);
        assert!(matches!(
            mirrored(&[("m", file(Some(1), "m", vec![one_field]))]),
            Err(SnapshotError::PartialEntryMetadata { .. })
        ));
        // Metadata needs a root revision.
        assert!(matches!(
            mirrored(&[("m", file(None, "m", vec![entry("A", "a", Some((1, 1)))]))]),
            Err(SnapshotError::MetadataWithoutRevision { .. })
        ));
        // Mirrors hold the same metadata.
        let other = file(Some(1), "m", vec![entry("A", "a", None)]);
        assert!(matches!(
            raw(&[
                (format!("{WWW}/m.json"), tracked.clone()),
                (format!("{COMMON}/m.json"), other),
            ]),
            Err(SnapshotError::MirrorMismatch { .. })
        ));
        let other = file(Some(1), "m", vec![entry("A", "a", Some((1, 1)))]).replace(
            r#""last_changed_revision":1"#,
            r#""last_changed_revision":2"#,
        );
        assert!(matches!(
            raw(&[
                (format!("{WWW}/m.json"), tracked),
                (format!("{COMMON}/m.json"), other)
            ]),
            Err(SnapshotError::MirrorMismatch { .. })
        ));
    }

    #[mononoke::test]
    fn test_entry_revisions_are_positive_int32_within_the_root() {
        for value in [
            "0",
            "-1",
            "1.0",
            "1e0",
            "\"1\"",
            "true",
            "null",
            "[1]",
            "2147483648",
            "9223372036854775808",
        ] {
            for field in [INTRODUCED_REVISION_FIELD, LAST_CHANGED_REVISION_FIELD] {
                let json = file(Some(1), "m", vec![entry("A", "a", Some((1, 1)))])
                    .replace(&format!(r#""{field}":1"#), &format!(r#""{field}":{value}"#));
                assert!(
                    matches!(
                        mirrored(&[("m", json)]),
                        Err(SnapshotError::InvalidEntryRevision { .. })
                    ),
                    "{field} {value}"
                );
            }
        }
        for (root, introduced, last_changed) in [(3, 3, 2), (3, 1, 4), (3, 4, 4)] {
            assert!(
                matches!(
                    mirrored(&[(
                        "m",
                        file(
                            Some(root),
                            "m",
                            vec![entry("A", "a", Some((introduced, last_changed)))]
                        )
                    )]),
                    Err(SnapshotError::EntryRevisionsOutOfRange { .. })
                ),
                "{introduced} {last_changed} at {root}"
            );
        }
        let max = i64::from(i32::MAX);
        assert!(
            mirrored(&[(
                "m",
                file(Some(max), "m", vec![entry("A", "a", Some((1, max)))])
            )])
            .is_ok()
        );
    }

    #[mononoke::test]
    fn test_keys_are_unique_across_every_module_and_file() {
        let in_two_modules = json!({
            "source_revision": 1,
            "modules": [
                {"key": "m", "strings": [entry("A", "a", Some((1, 1)))]},
                {"key": "n", "strings": [entry("A", "other", Some((1, 1)))]},
            ],
        })
        .to_string();
        assert!(matches!(
            mirrored(&[("m", in_two_modules)]),
            Err(SnapshotError::DuplicateKey { .. })
        ));
        assert!(matches!(
            mirrored(&[
                ("m", file(Some(1), "m", vec![entry("A", "a", Some((1, 1)))])),
                ("n", file(Some(1), "n", vec![entry("A", "a", Some((1, 1)))])),
            ]),
            Err(SnapshotError::DuplicateKey { .. })
        ));
        // A key in a mirror is the authoritative key, not a duplicate.
        assert!(
            mirrored(&[("m", file(Some(1), "m", vec![entry("A", "a", Some((1, 1)))]))]).is_ok()
        );
        let mut keyless = entry("A", "a", Some((1, 1)));
        keyless
            .as_object_mut()
            .expect("an entry is an object")
            .remove("key");
        assert!(matches!(
            mirrored(&[("m", file(Some(1), "m", vec![keyless]))]),
            Err(SnapshotError::EntryWithoutKey { .. })
        ));
        // A repeated JSON key, anywhere, is not valid JSON.
        for json in [
            r#"{"source_revision": 1, "source_revision": 1, "modules": []}"#,
            r#"{"source_revision": 1, "modules": [{"key": "m", "strings": [{"key": "A", "en_string": "a", "en_string": "b", "introduced_revision": 1, "last_changed_revision": 1}]}]}"#,
            r#"{"source_revision": 1, "modules": [{"key": "m", "strings": [{"key": "A", "introduced_revision": 1, "introduced_revision": 1, "last_changed_revision": 1}]}]}"#,
        ] {
            let error = mirrored(&[("m", json.to_owned())]).expect_err(json);
            assert!(
                matches!(&error, SnapshotError::NotJson { error, .. } if error.contains("duplicate key")),
                "{json}: {error}"
            );
        }
    }

    #[mononoke::test]
    fn test_edits_keep_introduced_and_take_the_new_revision() {
        let parent = ab(2, (1, 1), (1, 2));
        let edited = |a: (i64, i64), change: Value| {
            let mut a_entry = entry("A", "a", Some(a));
            for (field, value) in change.as_object().expect("changes are an object") {
                a_entry[field] = value.clone();
            }
            snapshot(&[(
                "m",
                file(Some(3), "m", vec![a_entry, entry("B", "b", Some((1, 2)))]),
            )])
        };
        // Any field of the entry but the metadata, unknown ones included.
        for change in [
            json!({"en_string": "edited"}),
            json!({"comment": "edited"}),
            json!({"future_field": 1}),
        ] {
            assert_eq!(
                check_transition(&parent, &edited((1, 3), change.clone())),
                Ok(())
            );
            assert_eq!(
                check_transition(&parent, &edited((1, 1), change.clone())),
                Err(mismatch("A", EntryChange::Edited, (1, 3), (1, 1)))
            );
            assert_eq!(
                check_transition(&parent, &edited((3, 3), change)),
                Err(mismatch("A", EntryChange::Edited, (1, 3), (3, 3)))
            );
        }
        // No change keeps the metadata, and the metadata alone is no change.
        assert_eq!(check_transition(&parent, &ab(2, (1, 1), (1, 2))), Ok(()));
        assert_eq!(
            check_transition(&parent, &ab(2, (1, 2), (1, 2))),
            Err(mismatch("A", EntryChange::Unchanged, (1, 1), (1, 2)))
        );
        assert_eq!(
            check_transition(&parent, &ab(3, (1, 3), (1, 2))),
            Err(TransitionError::RevisionChangedWithoutContentChange {
                parent_revision: revision(2),
                revision: revision(3),
            })
        );
    }

    #[mononoke::test]
    fn test_new_removed_and_added_again() {
        let parent = ab(2, (1, 1), (1, 2));
        let with_c = |c: (i64, i64)| {
            snapshot(&[(
                "m",
                file(
                    Some(3),
                    "m",
                    vec![
                        entry("A", "a", Some((1, 1))),
                        entry("B", "b", Some((1, 2))),
                        entry("C", "c", Some(c)),
                    ],
                ),
            )])
        };
        assert_eq!(check_transition(&parent, &with_c((3, 3))), Ok(()));
        for c in [(1, 3), (2, 2), (1, 1)] {
            assert_eq!(
                check_transition(&parent, &with_c(c)),
                Err(mismatch("C", EntryChange::Added, (3, 3), c))
            );
        }
        // Removed: no entry to check, but the root revision moves.
        let without_b = snapshot(&[("m", file(Some(3), "m", vec![entry("A", "a", Some((1, 1)))]))]);
        assert_eq!(check_transition(&parent, &without_b), Ok(()));
        // Added again later: a new lifetime, not the old one.
        let readded = |b: (i64, i64)| {
            snapshot(&[(
                "m",
                file(
                    Some(4),
                    "m",
                    vec![entry("A", "a", Some((1, 1))), entry("B", "b", Some(b))],
                ),
            )])
        };
        assert_eq!(check_transition(&without_b, &readded((4, 4))), Ok(()));
        assert_eq!(
            check_transition(&without_b, &readded((1, 2))),
            Err(mismatch("B", EntryChange::Added, (4, 4), (1, 2)))
        );
    }

    #[mononoke::test]
    fn test_moves_reorders_and_descriptions_change_only_the_root() {
        let parent = snapshot(&[
            (
                "m",
                file(
                    Some(2),
                    "m",
                    vec![entry("A", "a", Some((1, 1))), entry("B", "b", Some((1, 2)))],
                ),
            ),
            ("n", file(Some(2), "n", vec![entry("C", "c", Some((2, 2)))])),
        ]);
        let next = |m: Vec<Value>, n: Vec<Value>, description: &str| {
            let m = file(Some(3), "m", m).replace(
                r#""description":"d""#,
                &format!(r#""description":"{description}""#),
            );
            snapshot(&[("m", m), ("n", file(Some(3), "n", n))])
        };
        let a = || entry("A", "a", Some((1, 1)));
        let b = || entry("B", "b", Some((1, 2)));
        let c = || entry("C", "c", Some((2, 2)));
        // A to another file, the entries reordered, a description edited.
        for candidate in [
            next(vec![b()], vec![c(), a()], "d"),
            next(vec![b(), a()], vec![c()], "d"),
            next(vec![a(), b()], vec![c()], "edited"),
        ] {
            assert_eq!(check_transition(&parent, &candidate), Ok(()));
        }
        // The metadata moves with the entry and is not bumped.
        assert_eq!(
            check_transition(
                &parent,
                &next(vec![b()], vec![c(), entry("A", "a", Some((1, 3)))], "d")
            ),
            Err(mismatch("A", EntryChange::Unchanged, (1, 1), (1, 3)))
        );
        // Moved and edited is edited.
        assert_eq!(
            check_transition(
                &parent,
                &next(
                    vec![b()],
                    vec![c(), entry("A", "edited", Some((1, 3)))],
                    "d"
                )
            ),
            Ok(())
        );
        // Still a root change, which needs the bump.
        let same_root = snapshot(&[
            ("m", file(Some(2), "m", vec![b()])),
            ("n", file(Some(2), "n", vec![c(), a()])),
        ]);
        assert!(matches!(
            check_transition(&parent, &same_root),
            Err(TransitionError::DuplicateRevision { .. })
        ));
    }

    #[mononoke::test]
    fn test_the_largest_revision() {
        let max = i64::from(i32::MAX);
        let at_max = |text: &str, a: (i64, i64)| {
            snapshot(&[("m", file(Some(max), "m", vec![entry("A", text, Some(a))]))])
        };
        let before = snapshot(&[(
            "m",
            file(Some(max - 1), "m", vec![entry("A", "a", Some((1, 1)))]),
        )]);
        assert_eq!(
            check_transition(&before, &at_max("edited", (1, max))),
            Ok(())
        );
        let parent = at_max("edited", (1, max));
        assert_eq!(
            check_transition(&parent, &at_max("edited", (1, max))),
            Ok(())
        );
        assert_eq!(
            check_transition(&parent, &at_max("again", (1, max))),
            Err(TransitionError::RevisionOverflow {
                parent_revision: SourceRevision::MAX
            })
        );
    }

    /// The JSON text of a file whose module `n` puts `hidden` where its
    /// strings or the module itself belong, next to a well-formed module `m`
    /// holding `A` with metadata `a`.
    fn with_hidden(root: i64, a: Option<(i64, i64)>, hidden: Value) -> String {
        let mut file = json!({
            "source_revision": root,
            "modules": [
                {"key": "m", "strings": [entry("A", "a", a)]},
                {"key": "n", "strings": []},
            ],
        });
        match hidden {
            Value::Object(module) if module.contains_key("module") => {
                file["modules"][1] = module["module"].clone();
            }
            strings => file["modules"][1]["strings"] = strings,
        }
        file.to_string()
    }

    #[mononoke::test]
    fn test_malformed_catalogs_cannot_hide_entries_from_a_tracked_snapshot() {
        // A duplicate of `A` and an untracked string, hidden in each shape.
        let hidden_entry = entry("A", "other", None);
        let cases: Vec<(&str, Value, &str)> = vec![
            (
                "modules[1].strings",
                json!({"0": hidden_entry.clone()}),
                "an array",
            ),
            ("modules[1].strings", json!(null), "an array"),
            ("modules[1].strings", json!("A"), "an array"),
            ("modules[1].strings", json!(7), "an array"),
            (
                "modules[1]",
                json!({"module": [hidden_entry.clone()]}),
                "an object",
            ),
            ("modules[1]", json!({"module": "n"}), "an object"),
            ("modules[1]", json!({"module": null}), "an object"),
            ("modules[1].strings[0]", json!([["A"]]), "an object"),
        ];
        for (location, hidden, expected) in cases {
            let json = with_hidden(1, Some((1, 1)), hidden);
            for files in [
                // In the only file, mirrored.
                vec![("m", json.clone())],
                // In a second file next to a well-formed one, mirrored.
                vec![
                    ("m", file(Some(1), "m", vec![entry("B", "b", Some((1, 1)))])),
                    ("n", json.clone()),
                ],
            ] {
                let error = mirrored(&files).expect_err(location);
                assert!(
                    matches!(
                        &error,
                        SnapshotError::MalformedCatalog { location: found, expected: shape, .. }
                            if found == location && *shape == expected
                    ),
                    "{location}: {error}"
                );
            }
        }
        // `modules` itself.
        for modules in [json!({"0": {"strings": []}}), json!(null), json!("m")] {
            let tracked = file(Some(1), "m", vec![entry("A", "a", Some((1, 1)))]);
            let malformed = json!({"source_revision": 1, "modules": modules}).to_string();
            assert!(matches!(
                mirrored(&[("m", tracked), ("n", malformed)]),
                Err(SnapshotError::MalformedCatalog {
                    expected: "an array",
                    ..
                })
            ));
        }
        // Only in a mirror: the mirrors disagree.
        let good = file(Some(1), "m", vec![entry("A", "a", Some((1, 1)))]);
        assert!(matches!(
            raw(&[
                (format!("{WWW}/m.json"), good),
                (
                    format!("{COMMON}/m.json"),
                    with_hidden(1, Some((1, 1)), json!({}))
                ),
            ]),
            Err(SnapshotError::MalformedCatalog { .. } | SnapshotError::MirrorMismatch { .. })
        ));
    }

    #[mononoke::test]
    fn test_untracked_snapshots_keep_their_shape_rules() {
        // Malformed but untracked: the root-only rules, as before tracking.
        for hidden in [json!({}), json!(null), json!({"module": "n"})] {
            let snapshot =
                mirrored(&[("m", with_hidden(1, None, hidden))]).expect("untracked is unchecked");
            assert!(!snapshot.is_tracked());
        }
        // Absent `modules` or `strings` hold no entries, tracked or not.
        let no_strings = json!({
            "source_revision": 1,
            "modules": [
                {"key": "m", "strings": [entry("A", "a", Some((1, 1)))]},
                {"key": "n"},
            ],
        })
        .to_string();
        assert!(snapshot(&[("m", no_strings)]).is_tracked());
        let no_modules = json!({"source_revision": 1}).to_string();
        assert!(
            snapshot(&[
                ("m", file(Some(1), "m", vec![entry("A", "a", Some((1, 1)))])),
                ("n", no_modules),
            ])
            .is_tracked()
        );
    }

    #[mononoke::test]
    fn test_hidden_entries_are_not_new_when_exposed() {
        // A tracked parent cannot hold a hidden entry, so the only way to
        // expose one is from an untracked snapshot, which then has to start
        // tracking at the baseline.
        let legacy = mirrored(&[(
            "m",
            with_hidden(1, None, json!({"0": entry("C", "c", None)})),
        )])
        .expect("untracked is unchecked");
        let exposed = snapshot(&[(
            "m",
            json!({
                "source_revision": 1,
                "modules": [
                    {"key": "m", "strings": [entry("A", "a", Some((1, 1)))]},
                    {"key": "n", "strings": [entry("C", "c", Some((1, 1)))]},
                ],
            })
            .to_string(),
        )]);
        assert_eq!(
            check_transition(&legacy, &exposed),
            Err(TransitionError::MetadataMigrationChangesSource)
        );
        // A tracked parent cannot become malformed either.
        let parent = snapshot(&[(
            "m",
            json!({
                "source_revision": 1,
                "modules": [
                    {"key": "m", "strings": [entry("A", "a", Some((1, 1)))]},
                    {"key": "n", "strings": [entry("C", "c", Some((1, 1)))]},
                ],
            })
            .to_string(),
        )]);
        assert!(matches!(
            mirrored(&[(
                "m",
                with_hidden(2, Some((1, 1)), json!({"0": entry("C", "c", Some((1, 1)))}))
            )]),
            Err(SnapshotError::MalformedCatalog { .. })
        ));
        // Hiding every tracked entry is removing the metadata.
        let all_hidden = json!({"source_revision": 2, "modules": {"m": []}}).to_string();
        let all_hidden = mirrored(&[("m", all_hidden)]).expect("untracked is unchecked");
        assert_eq!(
            check_transition(&parent, &all_hidden),
            Err(TransitionError::MetadataRemoved)
        );
    }

    #[mononoke::test]
    fn test_removing_some_entries_keeps_tracking() {
        let parent = ab(2, (1, 1), (1, 2));
        // An empty module next to a tracked one is fine.
        let one_left = json!({
            "source_revision": 3,
            "modules": [
                {"key": "m", "strings": [entry("A", "a", Some((1, 1)))]},
                {"key": "n", "strings": []},
            ],
        })
        .to_string();
        let one_left = snapshot(&[("m", one_left)]);
        assert_eq!(check_transition(&parent, &one_left), Ok(()));
        // The last tracked entry cannot go: there would be nothing to keep
        // tracking, so it reads as a reset.
        let none_left = json!({
            "source_revision": 4,
            "modules": [{"key": "m", "strings": []}, {"key": "n", "strings": []}],
        })
        .to_string();
        assert_eq!(
            check_transition(&one_left, &snapshot(&[("m", none_left)])),
            Err(TransitionError::MetadataRemoved)
        );
    }

    #[mononoke::test]
    fn test_diagnostics_name_both_entries() {
        let twice = json!({
            "source_revision": 3,
            "modules": [{"key": "m", "strings": [
                entry("A", "a", Some((1, 1))),
                entry("A", "b", Some((2, 2))),
            ]}],
        })
        .to_string();
        let error = mirrored(&[("m", twice)]).expect_err("A twice");
        let message = error.to_string();
        assert!(
            message.contains("modules[0].strings[0] (A)")
                && message.contains("modules[0].strings[1] (A)"),
            "{message}"
        );
        let out_of_range = file(Some(2), "m", vec![entry("A", "a", Some((1, 3)))]);
        let message = mirrored(&[("m", out_of_range)])
            .expect_err("out of range")
            .to_string();
        assert!(
            message.contains("www/generated/m.json")
                && message.contains("modules[0].strings[0] (A)"),
            "{message}"
        );
    }
}
