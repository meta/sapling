/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Turns what Perforce reports about a changelist into Bonsai. The caller does all the
//! I/O, so the same inputs always give the same result.

use std::collections::HashMap;

use mononoke_types::BonsaiChangeset;
use mononoke_types::BonsaiChangesetMut;
use mononoke_types::ChangesetId;
use mononoke_types::ContentMetadataV2;
use mononoke_types::DateTime;
use mononoke_types::FileChange;
use mononoke_types::FileType;
use mononoke_types::GitLfs;
use mononoke_types::NonRootMPath;
use mononoke_types::P4ChangelistId;
use mononoke_types::PrefixTrie;
use p4_types::Action;
use p4_types::Changelist;
use p4_types::FileRevision;
use p4_types::Integration;
use p4_types::IntegrationHow;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransformError {
    #[error("depot root {0:?} is not like //depot/")]
    InvalidDepotRoot(String),
    #[error("{depot_path} is not a valid path in the repo")]
    InvalidPath {
        depot_path: String,
        #[source]
        source: anyhow::Error,
    },
    #[error("no stored content for {0}")]
    MissingContent(String),
    #[error("changelist {changelist}'s user {user:?} contains a newline")]
    NewlineInAuthor { changelist: u64, user: String },
    #[error("{0:?} contains a newline")]
    NewlineInPath(String),
    #[error("changelist {changelist}'s description doesn't decode from its original bytes")]
    DescriptionMismatch { changelist: u64 },
    #[error("changelist {changelist} is not a valid changeset")]
    InvalidChangeset {
        changelist: u64,
        #[source]
        source: anyhow::Error,
    },
}

/// The revision number of each of the changelist's files in the depot, one
/// `<rev>\t<repo path>\n` line each, in `file_changes`' path order. Deletions are revisions
/// too, including those Bonsai drops because a file replaced their directory. `p4-` keeps
/// the transform's extras apart from the `convert_*` changelist id ones.
/// Design: designs/file_revisions_and_copy_from.md.
pub const FILE_REVS_EXTRA: &str = "p4-file-revs";

/// For a description that isn't UTF-8, its original bytes; the message is their
/// `String::from_utf8_lossy` decoding.
pub const DESCRIPTION_BYTES_EXTRA: &str = "p4-description-bytes";

/// Builds the commit for one changelist: author, date and message from the changelist,
/// parent `parent`, and a file change for each of its files under `depot_root`
/// (files in other depots are skipped). `contents` and `copy_sources` are looked up
/// by full Perforce path, e.g. `//non-stream-depot/docs/readme.txt`.
/// Every revision keeps its file change, even an unchanged one; a changelist without
/// files is an empty changeset. Design: designs/data_model.md.
pub fn changeset(
    depot_root: &str,
    changelist: &Changelist,
    parent: Option<ChangesetId>,
    contents: &HashMap<String, ContentMetadataV2>,
    copy_sources: &HashMap<String, NonRootMPath>,
) -> Result<BonsaiChangeset, TransformError> {
    if changelist.user.contains('\n') {
        return Err(TransformError::NewlineInAuthor {
            changelist: changelist.number,
            user: changelist.user.clone(),
        });
    }
    let invalid = |source| TransformError::InvalidChangeset {
        changelist: changelist.number,
        source,
    };
    let author_date = i64::try_from(changelist.time)
        .map_err(anyhow::Error::from)
        .and_then(|secs| DateTime::from_timestamp(secs, 0))
        .map_err(invalid)?;
    let mut file_changes = Vec::new();
    let mut revisions = Vec::new();
    for revision in &changelist.files {
        // Without a parent the source can't be in it, so the copy is dropped.
        let copy_from = parent
            .and_then(|parent| Some((copy_sources.get(&revision.depot_path)?.clone(), parent)));
        let content = contents.get(&revision.depot_path);
        if let Some((path, change)) = file_change(depot_root, revision, content, copy_from)? {
            revisions.push((path.clone(), revision.revision));
            file_changes.push((path, change));
        }
    }
    let mut bonsai = BonsaiChangesetMut {
        parents: parent.into_iter().collect(),
        author: changelist.user.clone(),
        author_date,
        message: changelist.description.clone(),
        hg_extra: P4ChangelistId::new(changelist.number)
            .to_hg_extras()
            .into_iter()
            .collect(),
        file_changes: without_shadowed_deletions(file_changes).collect(),
        ..Default::default()
    };
    bonsai
        .hg_extra
        .insert(FILE_REVS_EXTRA.to_owned(), file_revs(revisions)?);
    if let Some(original) = &changelist.description_bytes {
        if String::from_utf8_lossy(original) != changelist.description {
            return Err(TransformError::DescriptionMismatch {
                changelist: changelist.number,
            });
        }
        if std::str::from_utf8(original).is_err() {
            bonsai
                .hg_extra
                .insert(DESCRIPTION_BYTES_EXTRA.to_owned(), original.clone());
        }
    }
    bonsai.freeze().map_err(invalid)
}

fn file_revs(mut revisions: Vec<(NonRootMPath, u32)>) -> Result<Vec<u8>, TransformError> {
    revisions.sort_unstable();
    let mut lines = String::new();
    for (path, rev) in revisions {
        let path = path.to_string();
        // A newline would end the line early; a tab is fine after the first.
        if path.contains('\n') {
            return Err(TransformError::NewlineInPath(path));
        }
        lines.push_str(&format!("{rev}\t{path}\n"));
    }
    Ok(lines.into_bytes())
}

/// When a file replaces a directory, Perforce also lists the directory's files as
/// deleted. Adding the file already removes them, and `freeze` rejects the pair.
/// Design: designs/data_model.md.
fn without_shadowed_deletions(
    changes: Vec<(NonRootMPath, FileChange)>,
) -> impl Iterator<Item = (NonRootMPath, FileChange)> {
    let mut changed = PrefixTrie::new();
    for (path, change) in &changes {
        if change.is_changed() {
            changed.add(path);
        }
    }
    changes
        .into_iter()
        .filter(move |(path, change)| change.is_changed() || !changed.contains_prefix(path))
}

/// The change a file revision makes in the depot at `depot_root` (like `//test1/`), or
/// `None` if the file is in another depot. `content` is the revision's content, already in
/// the filestore, and is required for actions that leave content behind. `copy_from` is
/// the `copy_source` path in the parent changeset.
pub fn file_change(
    depot_root: &str,
    revision: &FileRevision,
    content: Option<&ContentMetadataV2>,
    copy_from: Option<(NonRootMPath, ChangesetId)>,
) -> Result<Option<(NonRootMPath, FileChange)>, TransformError> {
    let Some(path) = repo_path(depot_root, &revision.depot_path)? else {
        return Ok(None);
    };
    let change = match revision.action {
        Action::Add
        | Action::Edit
        | Action::Import
        | Action::Integrate
        | Action::Branch
        | Action::MoveAdd => {
            let content = content
                .ok_or_else(|| TransformError::MissingContent(revision.depot_path.clone()))?;
            FileChange::tracked(
                content.content_id,
                file_type(&revision.file_type),
                content.total_size,
                copy_from,
                GitLfs::FullContent,
            )
        }
        // Purged and archived revisions have no content left to print.
        Action::Delete | Action::MoveDelete | Action::Purge | Action::Archive => {
            FileChange::Deletion
        }
    };
    Ok(Some((path, change)))
}

/// The repo path a revision was copied from, or why it has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopySource {
    From(NonRootMPath),
    /// Not a file starting life at a new path, or filelog records no source.
    NotACopy,
    /// Several sources: picking one would be a guess.
    Ambiguous,
    OtherDepot,
}

/// The source of a revision of a file that starts life at a new path, given that
/// revision's filelog `integrations`. Whether the source is a file in the parent needs the
/// repo, so the importer checks that. Design: designs/file_revisions_and_copy_from.md.
pub fn copy_source(
    depot_root: &str,
    revision: &FileRevision,
    integrations: &[Integration],
) -> Result<CopySource, TransformError> {
    // Onto an existing path, copy_from would read as that file being replaced, hiding
    // its own history.
    if !can_have_copy_source(revision.action) {
        return Ok(CopySource::NotACopy);
    }
    let mut sources = integrations.iter().filter(|integration| {
        matches!(
            integration.how,
            IntegrationHow::BranchFrom | IntegrationHow::CopyFrom | IntegrationHow::MovedFrom
        )
    });
    let source = match (sources.next(), sources.next()) {
        (None, _) => return Ok(CopySource::NotACopy),
        (Some(source), None) => source,
        (Some(_), Some(_)) => return Ok(CopySource::Ambiguous),
    };
    Ok(match repo_path(depot_root, &source.file)? {
        Some(path) => CopySource::From(path),
        None => CopySource::OtherDepot,
    })
}

/// Whether `copy_source` can give a source for a revision with `action`, so callers fetch
/// integrations only when they can matter.
pub fn can_have_copy_source(action: Action) -> bool {
    matches!(action, Action::Branch | Action::MoveAdd)
}

/// `depot_path` in the repo for the depot at `depot_root`, or `None` if it is in another
/// depot. Unescaped, in the case it was submitted in. Design: designs/paths_and_text.md.
pub fn repo_path(
    depot_root: &str,
    depot_path: &str,
) -> Result<Option<NonRootMPath>, TransformError> {
    // Exactly one depot: //test1 would also take //test10's files, and // every depot's.
    let depot_name = depot_root
        .strip_prefix("//")
        .and_then(|rest| rest.strip_suffix('/'));
    if !depot_name.is_some_and(|name| !name.is_empty() && !name.contains('/')) {
        return Err(TransformError::InvalidDepotRoot(depot_root.to_owned()));
    }
    let Some(path) = depot_path.strip_prefix(depot_root) else {
        return Ok(None);
    };
    NonRootMPath::new(unescape_path(path))
        .map(Some)
        .map_err(|source| TransformError::InvalidPath {
            depot_path: depot_path.to_owned(),
            source,
        })
}

/// The executable ones among the old type names `p4 help filetypes` lists, like `xtext`
/// for `text+x`.
const LEGACY_EXECUTABLE_TYPES: [&str; 10] = [
    "cxtext", "kxtext", "uxbinary", "xbinary", "xltext", "xtempobj", "xtext", "xunicode", "xutf16",
    "xutf8",
];

/// A Perforce type is a base type and modifiers, like `text+kx`. Only `x` makes it
/// executable (`X` is the archive modifier).
pub fn file_type(p4_type: &str) -> FileType {
    let (base, modifiers) = p4_type.split_once('+').unwrap_or((p4_type, ""));
    if base == "symlink" {
        FileType::Symlink
    } else if modifiers.contains('x') || LEGACY_EXECUTABLE_TYPES.contains(&base) {
        FileType::Executable
    } else {
        FileType::Regular
    }
}

/// The characters Perforce stores escaped in depot paths, as `%` and two hex digits,
/// because they are revision and wildcard syntax. `%` is among them, so unescaping is
/// unambiguous.
const ESCAPES: [(char, &str); 4] = [('%', "25"), ('@', "40"), ('#', "23"), ('*', "2A")];

/// The real filename for a depot path as Perforce reports it, as `p4 sync` writes it to
/// disk: `foo%402x.png` is `foo@2x.png`. Repo paths are unescaped.
/// Design: designs/paths_and_text.md.
pub fn unescape_path(depot_path: &str) -> String {
    let mut segments = depot_path.split('%');
    let first = segments.next().unwrap_or_default().to_owned();
    segments.fold(first, |mut unescaped, segment| {
        match ESCAPES.iter().find(|(_, hex)| segment.starts_with(hex)) {
            Some((character, hex)) => {
                unescaped.push(*character);
                unescaped.push_str(&segment[hex.len()..]);
            }
            None => {
                unescaped.push('%');
                unescaped.push_str(segment);
            }
        }
        unescaped
    })
}

/// The depot path Perforce expects for a real filename, the inverse of `unescape_path`.
pub fn escape_path(path: &str) -> String {
    path.chars()
        .fold(String::with_capacity(path.len()), |mut escaped, c| {
            match ESCAPES.iter().find(|(character, _)| *character == c) {
                Some((_, hex)) => {
                    escaped.push('%');
                    escaped.push_str(hex);
                }
                None => escaped.push(c),
            }
            escaped
        })
}

#[cfg(test)]
mod tests;
