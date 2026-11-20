/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! The `source_revision` rules, over file contents already read.

mod strict_json;
mod tracking;

use std::cmp::Ordering;
use std::collections::BTreeMap;

use derive_more::Display;
use mononoke_types::MPath;
use mononoke_types::NonRootMPath;
use serde_json::Map;
use serde_json::Value;
use thiserror::Error;
use tracking::Entry;
pub use tracking::EntryChange;
pub use tracking::EntryRevisions;
pub use tracking::INTRODUCED_REVISION_FIELD;
pub use tracking::LAST_CHANGED_REVISION_FIELD;
use tracking::TrackedEntries;

/// The root field every guarded file carries once versioned.
pub const SOURCE_REVISION_FIELD: &str = "source_revision";

/// A `source_revision`: a positive signed 32-bit integer, so that it fits the
/// publisher's `FlatLanguagePack.version`. Zero is the implicit revision of a
/// snapshot that has none, and is never written.
#[derive(Clone, Copy, Debug, Display, Eq, PartialEq, Ord, PartialOrd)]
pub struct SourceRevision(i32);

impl SourceRevision {
    /// The revision a snapshot without revisions moves to.
    pub const FIRST: SourceRevision = SourceRevision(1);

    /// The largest revision.
    pub const MAX: SourceRevision = SourceRevision(i32::MAX);

    /// `value` as a revision, if it is in `1..=i32::MAX`. The bound is
    /// checked on the wide value, so nothing wraps or truncates.
    pub fn new(value: i64) -> Option<Self> {
        i32::try_from(value)
            .ok()
            .filter(|value| *value > 0)
            .map(Self)
    }

    pub fn get(self) -> i32 {
        self.0
    }

    fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

/// Why the guarded files of one commit are not a valid snapshot.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum SnapshotError {
    #[error("{path} is {size} bytes, over the {limit} byte limit")]
    FileTooLarge {
        path: NonRootMPath,
        size: u64,
        limit: u64,
    },
    #[error("{path} is not a regular file")]
    NotRegularFile { path: NonRootMPath },
    #[error("{path} is changed by an untracked file change")]
    UntrackedChange { path: NonRootMPath },
    #[error("{path} is not valid JSON: {error}")]
    NotJson { path: NonRootMPath, error: String },
    #[error("{path} is not a JSON object")]
    NotAnObject { path: NonRootMPath },
    #[error(
        "{path} has {SOURCE_REVISION_FIELD} {value}, which is not a positive signed 32-bit integer (1 to 2147483647)"
    )]
    InvalidRevision { path: NonRootMPath, value: String },
    #[error("{versioned} has a {SOURCE_REVISION_FIELD} but {legacy} has none")]
    MixedVersioning {
        versioned: NonRootMPath,
        legacy: NonRootMPath,
    },
    #[error("{path} has {SOURCE_REVISION_FIELD} {revision} but {other_path} has {other_revision}")]
    RevisionMismatch {
        path: NonRootMPath,
        revision: SourceRevision,
        other_path: NonRootMPath,
        other_revision: SourceRevision,
    },
    #[error("{path} has no mirror at {mirror}")]
    MirrorMissingFile {
        path: NonRootMPath,
        mirror: NonRootMPath,
    },
    #[error("{mirror} mirrors {path}, which does not exist")]
    MirrorExtraFile {
        mirror: NonRootMPath,
        path: NonRootMPath,
    },
    #[error("{mirror} does not hold the same content as {path}")]
    MirrorMismatch {
        path: NonRootMPath,
        mirror: NonRootMPath,
    },
    #[error(
        "{path} is not an immediate `*.json` file of a generated source directory, so it would hide or replace generated source files"
    )]
    UnexpectedPath { path: NonRootMPath },
    #[error("{directory} has more than {limit} entries")]
    TooManyFiles {
        directory: NonRootMPath,
        limit: usize,
    },
    #[error("the commit changes more than {limit} paths under {directory}")]
    TooManyChanges {
        directory: NonRootMPath,
        limit: usize,
    },
    #[error(
        "the subtree operation at {path} would change generated source files without listing them"
    )]
    SubtreeChange { path: MPath },
    #[error("the generated source files are {size} bytes in total, over the {limit} byte limit")]
    SnapshotTooLarge { size: u64, limit: u64 },
    #[error(
        "{path} entry {entry} has only one of {INTRODUCED_REVISION_FIELD} and {LAST_CHANGED_REVISION_FIELD}"
    )]
    PartialEntryMetadata { path: NonRootMPath, entry: String },
    #[error(
        "{path} entry {entry} has {field} {value}, which is not a positive signed 32-bit integer (1 to 2147483647)"
    )]
    InvalidEntryRevision {
        path: NonRootMPath,
        entry: String,
        field: &'static str,
        value: String,
    },
    #[error(
        "{tracked} has {INTRODUCED_REVISION_FIELD} and {LAST_CHANGED_REVISION_FIELD} but {untracked} does not; every entry of every file must have them, or none"
    )]
    PartialMetadata { tracked: String, untracked: String },
    #[error(
        "{path} entry {entry} has {INTRODUCED_REVISION_FIELD} and {LAST_CHANGED_REVISION_FIELD}, but the files have no {SOURCE_REVISION_FIELD}"
    )]
    MetadataWithoutRevision { path: NonRootMPath, entry: String },
    #[error(
        "{path} {location} is not {expected}, so the strings it holds could not be checked; a tracked snapshot must have the generated shape"
    )]
    MalformedCatalog {
        path: NonRootMPath,
        location: String,
        expected: &'static str,
    },
    #[error("{path} entry {entry} has no string `key`")]
    EntryWithoutKey { path: NonRootMPath, entry: String },
    #[error(
        "key {key:?} is at {path} {entry} and at {other_path} {other_entry}; keys must be unique across every module"
    )]
    DuplicateKey {
        key: String,
        path: NonRootMPath,
        entry: String,
        other_path: NonRootMPath,
        other_entry: String,
    },
    #[error(
        "{path} entry {entry} has {revisions}, which is not within 1 <= {INTRODUCED_REVISION_FIELD} <= {LAST_CHANGED_REVISION_FIELD} <= {SOURCE_REVISION_FIELD} {revision}"
    )]
    EntryRevisionsOutOfRange {
        path: NonRootMPath,
        entry: String,
        revisions: EntryRevisions,
        revision: SourceRevision,
    },
}

/// Why a commit's snapshot cannot follow its parent's.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum TransitionError {
    #[error(
        "it removes {SOURCE_REVISION_FIELD} {parent_revision}; the generated files must keep a {SOURCE_REVISION_FIELD}"
    )]
    RevisionRemoved { parent_revision: SourceRevision },
    #[error(
        "it introduces {SOURCE_REVISION_FIELD} {revision}; files without one must start at {first}",
        first = SourceRevision::FIRST
    )]
    InvalidInitialRevision { revision: SourceRevision },
    #[error(
        "it changes the generated content but keeps {SOURCE_REVISION_FIELD} {parent_revision}, which another landing already used; regenerate on the new parent to get {expected}"
    )]
    DuplicateRevision {
        parent_revision: SourceRevision,
        expected: SourceRevision,
    },
    #[error(
        "it sets {SOURCE_REVISION_FIELD} {revision} below the parent's {parent_revision}; regenerate on the new parent to get {expected}"
    )]
    StaleRevision {
        parent_revision: SourceRevision,
        revision: SourceRevision,
        expected: SourceRevision,
    },
    #[error(
        "it sets {SOURCE_REVISION_FIELD} {revision} over the parent's {parent_revision}, skipping {expected}"
    )]
    RevisionGap {
        parent_revision: SourceRevision,
        revision: SourceRevision,
        expected: SourceRevision,
    },
    #[error(
        "it changes {SOURCE_REVISION_FIELD} from {parent_revision} to {revision} without changing the generated content"
    )]
    RevisionChangedWithoutContentChange {
        parent_revision: SourceRevision,
        revision: SourceRevision,
    },
    #[error(
        "it changes the generated content, but {SOURCE_REVISION_FIELD} {parent_revision} has no successor"
    )]
    RevisionOverflow { parent_revision: SourceRevision },
    #[error(
        "it removes {INTRODUCED_REVISION_FIELD} and {LAST_CHANGED_REVISION_FIELD} from some entries; once tracked, every entry must keep them"
    )]
    MetadataRemoved,
    #[error(
        "it adds {INTRODUCED_REVISION_FIELD} and {LAST_CHANGED_REVISION_FIELD} at {SOURCE_REVISION_FIELD} {parent_revision}; tracking can only start at {SOURCE_REVISION_FIELD} 1"
    )]
    MetadataBootstrapUnsupported { parent_revision: SourceRevision },
    #[error(
        "it adds {INTRODUCED_REVISION_FIELD} and {LAST_CHANGED_REVISION_FIELD} and changes the generated content; the migration must change nothing else"
    )]
    MetadataMigrationChangesSource,
    #[error("it starts tracking key {key:?} with {found}; every entry must start at 1 and 1")]
    MetadataNotBaseline { key: String, found: EntryRevisions },
    #[error("key {key:?} is {change}, so it must have {expected}, but it has {found}")]
    MetadataMismatch {
        key: String,
        change: EntryChange,
        expected: EntryRevisions,
        found: EntryRevisions,
    },
}

/// One guarded file, with its revision taken out of its content.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedFile {
    revision: Option<SourceRevision>,
    /// Everything but the root revision, which mirrors must match.
    content: Map<String, Value>,
    /// `content` without the entries' metadata, which the root revision
    /// covers.
    source: Map<String, Value>,
    entries: Vec<Entry>,
}

/// Parse a guarded file: a JSON object whose root `source_revision`, if
/// present, is a JSON integer in `1..=2147483647`, a positive signed 32-bit
/// integer. Floats and exponent forms are not integers, and an object with a
/// repeated key is not valid. Content compares as JSON:
/// object keys in any order, arrays in order.
pub fn parse_file(path: &NonRootMPath, bytes: &[u8]) -> Result<ParsedFile, SnapshotError> {
    let value: Value = strict_json::from_slice(bytes).map_err(|error| SnapshotError::NotJson {
        path: path.clone(),
        error: error.to_string(),
    })?;
    let Value::Object(mut content) = value else {
        return Err(SnapshotError::NotAnObject { path: path.clone() });
    };
    let revision = match content.remove(SOURCE_REVISION_FIELD) {
        None => None,
        Some(value) => Some(
            value
                .as_i64()
                .and_then(SourceRevision::new)
                .ok_or_else(|| SnapshotError::InvalidRevision {
                    path: path.clone(),
                    value: value.to_string(),
                })?,
        ),
    };
    let (source, entries) = tracking::split_entries(path, &content)?;
    Ok(ParsedFile {
        revision,
        content,
        source,
        entries,
    })
}

/// The revision of a whole snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotRevision {
    /// No guarded file has a `source_revision`: the files predate versioning,
    /// and the snapshot's revision is the implicit zero.
    Unversioned,
    /// Every guarded file is present, has this revision, and agrees with its
    /// mirrors.
    Versioned(SourceRevision),
}

/// The guarded files of one commit, checked as a whole.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedSnapshot {
    revision: SnapshotRevision,
    /// Each file without its root revision and entry metadata.
    source: BTreeMap<NonRootMPath, Map<String, Value>>,
    /// `None` if no entry is tracked.
    tracked: Option<TrackedEntries>,
}

impl ParsedSnapshot {
    pub fn revision(&self) -> SnapshotRevision {
        self.revision
    }

    /// Whether the entries carry tracking metadata.
    pub fn is_tracked(&self) -> bool {
        self.tracked.is_some()
    }
}

/// Check that `files`, every guarded file of a commit, is a valid snapshot.
/// Each file is an immediate child of one of `directories`, whose first
/// entry is authoritative and whose others mirror it file for file.
pub fn parse_snapshot(
    directories: &[NonRootMPath],
    files: BTreeMap<NonRootMPath, ParsedFile>,
) -> Result<ParsedSnapshot, SnapshotError> {
    let mut versioned: Option<(&NonRootMPath, SourceRevision)> = None;
    let mut unversioned: Option<&NonRootMPath> = None;
    for (path, file) in &files {
        match (file.revision, versioned) {
            (None, _) => {
                unversioned.get_or_insert(path);
            }
            (Some(revision), None) => versioned = Some((path, revision)),
            (Some(revision), Some((other_path, other_revision))) if revision != other_revision => {
                return Err(SnapshotError::RevisionMismatch {
                    path: path.clone(),
                    revision,
                    other_path: other_path.clone(),
                    other_revision,
                });
            }
            (Some(_), Some(_)) => {}
        }
    }

    let revision = match (versioned, unversioned) {
        (None, _) => SnapshotRevision::Unversioned,
        (Some((versioned, _)), Some(legacy)) => {
            return Err(SnapshotError::MixedVersioning {
                versioned: versioned.clone(),
                legacy: legacy.clone(),
            });
        }
        (Some((_, revision)), None) => {
            check_mirrored(directories, &files)?;
            SnapshotRevision::Versioned(revision)
        }
    };

    let tracked = tracking::tracked_entries(
        directories.first(),
        files
            .iter()
            .map(|(path, file)| (path, file.entries.as_slice())),
        revision,
    )?;

    Ok(ParsedSnapshot {
        revision,
        source: files
            .into_iter()
            .map(|(path, file)| (path, file.source))
            .collect(),
        tracked,
    })
}

fn check_mirrored(
    directories: &[NonRootMPath],
    files: &BTreeMap<NonRootMPath, ParsedFile>,
) -> Result<(), SnapshotError> {
    let Some((authoritative, mirrors)) = directories.split_first() else {
        return Ok(());
    };
    for (path, file) in files {
        let (dir, name) = path.split_dirname();
        if dir.as_ref() == Some(authoritative) {
            for mirror in mirrors {
                let mirror = mirror.join(name);
                match files.get(&mirror) {
                    None => {
                        return Err(SnapshotError::MirrorMissingFile {
                            path: path.clone(),
                            mirror,
                        });
                    }
                    Some(copy) if copy.content != file.content => {
                        return Err(SnapshotError::MirrorMismatch {
                            path: path.clone(),
                            mirror,
                        });
                    }
                    Some(_) => {}
                }
            }
        } else if !files.contains_key(&authoritative.join(name)) {
            return Err(SnapshotError::MirrorExtraFile {
                mirror: path.clone(),
                path: authoritative.join(name),
            });
        }
    }
    Ok(())
}

/// Check that `candidate` may follow `parent`, its commit's actual parent:
/// a content change takes exactly the parent's revision plus one, no content
/// change keeps the parent's revision, and unversioned files may only become
/// versioned at `SourceRevision::FIRST`. Entry metadata is not content: it
/// is checked entry by entry against the parent (see `tracking`).
pub fn check_transition(
    parent: &ParsedSnapshot,
    candidate: &ParsedSnapshot,
) -> Result<(), TransitionError> {
    let source_changed = parent.source != candidate.source;
    tracking::check_tracking_mode(
        parent.revision,
        parent.tracked.as_ref(),
        candidate.tracked.as_ref(),
        source_changed,
    )?;
    check_root_transition(parent.revision, candidate.revision, source_changed)?;
    match (&parent.tracked, &candidate.tracked, candidate.revision) {
        (Some(parent_entries), Some(candidate_entries), SnapshotRevision::Versioned(revision)) => {
            tracking::check_entries(parent_entries, candidate_entries, revision)
        }
        _ => Ok(()),
    }
}

fn check_root_transition(
    parent: SnapshotRevision,
    candidate: SnapshotRevision,
    content_changed: bool,
) -> Result<(), TransitionError> {
    use SnapshotRevision::Unversioned;
    use SnapshotRevision::Versioned;

    match (parent, candidate) {
        (Unversioned, Unversioned) => Ok(()),
        (Unversioned, Versioned(revision)) if revision == SourceRevision::FIRST => Ok(()),
        (Unversioned, Versioned(revision)) => {
            Err(TransitionError::InvalidInitialRevision { revision })
        }
        (Versioned(parent_revision), Unversioned) => {
            Err(TransitionError::RevisionRemoved { parent_revision })
        }
        (Versioned(parent_revision), Versioned(revision)) if !content_changed => {
            if revision == parent_revision {
                Ok(())
            } else {
                Err(TransitionError::RevisionChangedWithoutContentChange {
                    parent_revision,
                    revision,
                })
            }
        }
        (Versioned(parent_revision), Versioned(revision)) => {
            let expected = parent_revision
                .next()
                .ok_or(TransitionError::RevisionOverflow { parent_revision })?;
            match revision.cmp(&expected) {
                Ordering::Equal => Ok(()),
                Ordering::Less if revision == parent_revision => {
                    Err(TransitionError::DuplicateRevision {
                        parent_revision,
                        expected,
                    })
                }
                Ordering::Less => Err(TransitionError::StaleRevision {
                    parent_revision,
                    revision,
                    expected,
                }),
                Ordering::Greater => Err(TransitionError::RevisionGap {
                    parent_revision,
                    revision,
                    expected,
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use mononoke_macros::mononoke;

    use super::*;

    const WWW: &str = "www/generated";
    const COMMON: &str = "common/generated";

    fn path(path: &str) -> NonRootMPath {
        NonRootMPath::new(path).expect("test paths are valid")
    }

    fn directories() -> Vec<NonRootMPath> {
        vec![path(WWW), path(COMMON)]
    }

    fn snapshot(files: &[(&str, &str)]) -> Result<ParsedSnapshot, SnapshotError> {
        let files = files
            .iter()
            .map(|(name, json)| {
                let name = path(name);
                let file = parse_file(&name, json.as_bytes())?;
                Ok((name, file))
            })
            .collect::<Result<_, SnapshotError>>()?;
        parse_snapshot(&directories(), files)
    }

    /// Both directories hold the same `modules`, each module's JSON being
    /// `{"strings": <strings>}` plus `revision` when given.
    fn mirrored(revision: Option<i64>, modules: &[(&str, &str)]) -> ParsedSnapshot {
        let files: Vec<(String, String)> = modules
            .iter()
            .flat_map(|(module, strings)| {
                let json = match revision {
                    Some(revision) => {
                        format!(r#"{{"source_revision": {revision}, "strings": {strings}}}"#)
                    }
                    None => format!(r#"{{"strings": {strings}}}"#),
                };
                [WWW, COMMON].map(|dir| (format!("{dir}/{module}.json"), json.clone()))
            })
            .collect();
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, json)| (path.as_str(), json.as_str()))
            .collect();
        snapshot(&files).expect("test snapshot is valid")
    }

    fn versioned(revision: i64, default: &str, onboarding: &str) -> ParsedSnapshot {
        mirrored(
            Some(revision),
            &[("default", default), ("onboarding", onboarding)],
        )
    }

    fn revision(value: i64) -> SourceRevision {
        SourceRevision::new(value).expect("test revisions are positive")
    }

    #[mononoke::test]
    fn test_revision_must_be_positive_int32() -> Result<()> {
        let name = path("www/generated/default.json");
        let parsed = |value: &str| {
            parse_file(
                &name,
                format!(r#"{{"source_revision": {value}}}"#).as_bytes(),
            )
            .map(|file| file.revision)
        };
        assert_eq!(parsed("1")?, Some(revision(1)));
        assert_eq!(parsed("2147483647")?, Some(SourceRevision::MAX));
        assert_eq!(SourceRevision::MAX.get(), i32::MAX);
        for invalid in [
            "0",
            "-1",
            "-2147483648",
            "1.0",
            "1e3",
            "2147483647.0",
            "2.147483647e9",
            "true",
            "\"1\"",
            "null",
            "[1]",
            "2147483648",
            "4294967295",
            "4294967297",
            "9223372036854775807",
            "9223372036854775808",
            "18446744073709551616",
        ] {
            assert!(
                matches!(parsed(invalid), Err(SnapshotError::InvalidRevision { .. })),
                "{invalid} is not a positive signed 32-bit integer"
            );
        }
        // Wide values are rejected rather than wrapped or truncated.
        assert_eq!(SourceRevision::new(i64::from(i32::MAX) + 1), None);
        assert_eq!(SourceRevision::new(1 << 32 | 1), None);
        assert_eq!(SourceRevision::new(i64::MAX), None);
        assert_eq!(
            parse_file(&name, br#"{"modules": []}"#)?.revision,
            None,
            "an absent field is not an error"
        );
        assert!(matches!(
            parse_file(&name, b"[]"),
            Err(SnapshotError::NotAnObject { .. })
        ));
        assert!(matches!(
            parse_file(&name, b"{"),
            Err(SnapshotError::NotJson { .. })
        ));
        Ok(())
    }

    #[mononoke::test]
    fn test_content_ignores_revision_and_key_order_but_not_array_order() -> Result<()> {
        let name = path("www/generated/default.json");
        let a = parse_file(&name, br#"{"source_revision": 1, "a": 1, "b": [1, 2]}"#)?;
        let b = parse_file(&name, br#"{"b": [1, 2], "a": 1, "source_revision": 7}"#)?;
        let c = parse_file(&name, br#"{"a": 1, "b": [2, 1], "source_revision": 1}"#)?;
        let nested = parse_file(
            &name,
            br#"{"a": 1, "b": [1, 2], "x": {"source_revision": 1}}"#,
        )?;
        assert_eq!(a.content, b.content);
        assert_ne!(a.content, c.content, "arrays keep their order");
        assert_ne!(a.content, nested.content, "only the root field is stripped");
        Ok(())
    }

    #[mononoke::test]
    fn test_snapshot_classification() -> Result<()> {
        assert_eq!(
            snapshot(&[])?.revision(),
            SnapshotRevision::Unversioned,
            "no files at all is the unversioned baseline"
        );
        let bare = r#"{"strings": []}"#;
        assert_eq!(
            snapshot(&[("www/generated/default.json", bare)])?.revision(),
            SnapshotRevision::Unversioned,
            "unversioned files need no mirrors"
        );
        assert_eq!(
            versioned(3, "[]", "[]").revision(),
            SnapshotRevision::Versioned(revision(3))
        );

        let v1 = r#"{"source_revision": 1, "strings": []}"#;
        let v2 = r#"{"source_revision": 2, "strings": []}"#;
        let other = r#"{"source_revision": 1, "strings": [1]}"#;
        let www = "www/generated/default.json";
        let common = "common/generated/default.json";
        let www_new = "www/generated/new.json";
        let common_new = "common/generated/new.json";
        assert!(matches!(
            snapshot(&[(www, v1), (common, v1), (www_new, bare), (common_new, bare)]),
            Err(SnapshotError::MixedVersioning { .. })
        ));
        assert!(matches!(
            snapshot(&[(www, v1), (common, v1), (www_new, v2), (common_new, v2)]),
            Err(SnapshotError::RevisionMismatch { .. })
        ));
        assert!(matches!(
            snapshot(&[(www, v1), (common, v1), (www_new, v1)]),
            Err(SnapshotError::MirrorMissingFile { .. })
        ));
        assert!(matches!(
            snapshot(&[(www, v1), (common, v1), (common_new, v1)]),
            Err(SnapshotError::MirrorExtraFile { .. })
        ));
        assert!(matches!(
            snapshot(&[(www, v1), (common, other)]),
            Err(SnapshotError::MirrorMismatch { .. })
        ));
        assert!(
            matches!(
                snapshot(&[(www, v1), (common, v2)]),
                Err(SnapshotError::RevisionMismatch { .. })
            ),
            "a mirror with another revision disagrees on the revision"
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_transitions() {
        let five = versioned(5, "[1]", "[]");
        let ok = |parent: &ParsedSnapshot, candidate: &ParsedSnapshot| {
            check_transition(parent, candidate)
        };

        assert_eq!(
            ok(&five, &versioned(6, "[2]", "[]")),
            Ok(()),
            "next revision"
        );
        assert_eq!(
            ok(&five, &versioned(5, "[1]", "[]")),
            Ok(()),
            "no change keeps the revision"
        );
        assert_eq!(
            ok(
                &five,
                &mirrored(
                    Some(6),
                    &[("default", "[1]"), ("onboarding", "[]"), ("new", "[]")]
                )
            ),
            Ok(()),
            "a new module is a change"
        );
        assert_eq!(
            ok(&five, &mirrored(Some(6), &[("default", "[1]")])),
            Ok(()),
            "a removed module is a change"
        );
        assert_eq!(
            ok(&five, &mirrored(Some(5), &[("default", "[1]")])),
            Err(TransitionError::DuplicateRevision {
                parent_revision: revision(5),
                expected: revision(6),
            }),
            "removing a module needs the next revision"
        );
        assert_eq!(
            ok(&five, &versioned(5, "[2]", "[]")),
            Err(TransitionError::DuplicateRevision {
                parent_revision: revision(5),
                expected: revision(6),
            })
        );
        assert_eq!(
            ok(&five, &versioned(4, "[2]", "[]")),
            Err(TransitionError::StaleRevision {
                parent_revision: revision(5),
                revision: revision(4),
                expected: revision(6),
            })
        );
        assert_eq!(
            ok(&five, &versioned(7, "[2]", "[]")),
            Err(TransitionError::RevisionGap {
                parent_revision: revision(5),
                revision: revision(7),
                expected: revision(6),
            })
        );
        assert_eq!(
            ok(&five, &versioned(6, "[1]", "[]")),
            Err(TransitionError::RevisionChangedWithoutContentChange {
                parent_revision: revision(5),
                revision: revision(6),
            })
        );
        assert_eq!(
            ok(
                &five,
                &mirrored(None, &[("default", "[1]"), ("onboarding", "[]")])
            ),
            Err(TransitionError::RevisionRemoved {
                parent_revision: revision(5),
            })
        );
        assert_eq!(
            ok(&five, &mirrored(None, &[])),
            Err(TransitionError::RevisionRemoved {
                parent_revision: revision(5),
            }),
            "deleting every file removes the revision"
        );

        let legacy = mirrored(None, &[("default", "[1]")]);
        assert_eq!(
            ok(&legacy, &mirrored(None, &[("default", "[2]")])),
            Ok(()),
            "not yet versioned"
        );
        assert_eq!(
            ok(&legacy, &versioned(1, "[1]", "[]")),
            Ok(()),
            "initialised"
        );
        assert_eq!(
            ok(&mirrored(None, &[]), &versioned(1, "[2]", "[]")),
            Ok(()),
            "initialised from no files"
        );
        assert_eq!(
            ok(&legacy, &versioned(2, "[2]", "[]")),
            Err(TransitionError::InvalidInitialRevision {
                revision: revision(2),
            })
        );

        let max = versioned(i32::MAX.into(), "[1]", "[]");
        assert_eq!(ok(&max, &versioned(i32::MAX.into(), "[1]", "[]")), Ok(()));
        assert_eq!(
            ok(&max, &versioned(i32::MAX.into(), "[2]", "[]")),
            Err(TransitionError::RevisionOverflow {
                parent_revision: SourceRevision::MAX,
            })
        );
        assert_eq!(
            ok(
                &versioned(i64::from(i32::MAX) - 1, "[1]", "[]"),
                &versioned(i32::MAX.into(), "[2]", "[]")
            ),
            Ok(()),
            "the largest signed 32-bit revision is valid"
        );
    }
}
