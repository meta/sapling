/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Perforce revisions of paths in a `repos/p4/` mirror, read from the extras the
//! importer records on each changeset.

use std::collections::HashMap;
use std::fmt;

use anyhow::Context;
use anyhow::anyhow;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use manifest::Entry;
use mononoke_types::NonRootMPath;
use mononoke_types::P4ChangelistId;
use mononoke_types::P4FileRevs;
use mononoke_types::path::MPath;
use repo_identity::RepoIdentityRef;

use super::ChangesetPathHistoryContext;
use super::ChangesetPathHistoryOptions;
use crate::MononokeRepo;
use crate::changeset::ChangesetContext;
use crate::errors::MononokeError;
use crate::repo::RepoContext;

/// The Perforce revision a path has as of some changeset: the changeset, and changelist,
/// whose change to the path gave it that revision.
pub struct P4PathRevision<R> {
    /// The changeset that last changed the path.
    pub changeset: ChangesetContext<R>,
    /// Its changelist.
    pub changelist: P4ChangelistId,
    /// The revision that change gave the path.
    pub revision: u32,
    /// Whether the path exists after the change: `false` when the revision is a deletion.
    pub exists: bool,
    /// The path as Perforce records it.
    pub path: NonRootMPath,
}

// Not derived: `ChangesetContext<R>` is only `Debug` for `R: RepoIdentityRef`.
impl<R: RepoIdentityRef> fmt::Debug for P4PathRevision<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("P4PathRevision")
            .field("changeset", &self.changeset)
            .field("changelist", &self.changelist)
            .field("revision", &self.revision)
            .field("exists", &self.exists)
            .field("path", &self.path)
            .finish()
    }
}

/// How many changesets' extras are fetched at once.
const P4_EXTRAS_CONCURRENCY: usize = 10;

/// The Perforce revisions one changeset's changelist gave the files it touched, for
/// looking several paths up against one fetch of the changeset's extras.
#[derive(Debug, Clone)]
pub struct P4ChangesetRevisions {
    changeset_id: mononoke_types::ChangesetId,
    changelist: P4ChangelistId,
    revs: P4FileRevs,
}

impl P4ChangesetRevisions {
    pub fn changelist(&self) -> P4ChangelistId {
        self.changelist
    }

    /// The revision the changelist gave `path`, if it touched that file. Directories have
    /// none.
    pub fn revision(&self, path: &NonRootMPath) -> Result<Option<u32>, MononokeError> {
        Ok(self
            .revs
            .revision(path)
            .with_context(|| format!("commit {}", self.changeset_id))?)
    }

    /// The revision the changelist gave the file `path`, which it must have touched: the
    /// importer lists every file of a changelist, so a file a changeset changed but did
    /// not record is an internal error.
    pub fn file_revision(&self, path: &NonRootMPath) -> Result<u32, MononokeError> {
        self.revision(path)?.ok_or_else(|| {
            MononokeError::from(anyhow!(
                "commit {} (changelist {}) changed {} but recorded no Perforce revision for it",
                self.changeset_id,
                self.changelist.id(),
                path
            ))
        })
    }
}

impl<R: MononokeRepo> ChangesetContext<R> {
    /// The Perforce revisions this changeset's changelist gave the files it touched, or
    /// `None` if the changeset records none: it is not a Perforce import, or predates
    /// the importer recording them.
    pub async fn p4_file_revisions(&self) -> Result<Option<P4ChangesetRevisions>, MononokeError> {
        let info = self.changeset_info().await?;
        let Some(revs) = P4FileRevs::from_hg_extras(info.hg_extra()) else {
            return Ok(None);
        };
        let changelist = P4ChangelistId::from_hg_extras(info.hg_extra())
            .with_context(|| format!("commit {} records Perforce file revisions", self.id()))?;
        Ok(Some(P4ChangesetRevisions {
            changeset_id: self.id(),
            changelist,
            revs,
        }))
    }
}

/// A path's last change, as `ChangesetPathHistoryContext::last_modified` or
/// `last_deleted` found it, for `RepoContext::p4_path_revisions`.
#[derive(Clone)]
pub struct P4LastChange<R> {
    pub path: MPath,
    /// The changeset that last changed the path.
    pub changeset: ChangesetContext<R>,
    /// Whether the path exists after that change.
    pub exists: bool,
    /// Whether the path is a file there. A deleted path's kind is unknown: `false`.
    pub is_file: bool,
}

impl<R: MononokeRepo> RepoContext<R> {
    /// The Perforce revisions several paths have after their last changes, fetching each
    /// distinct changeset's revisions once: many paths share a last-changed commit.
    ///
    /// A path is absent from the result when it has no revision: the repo root, a
    /// directory, or a last-changed commit that records no revisions (not a Perforce
    /// import). A file whose last-changed commit records revisions but not its own is an
    /// internal error: the importer lists every file of a changelist.
    pub async fn p4_path_revisions(
        &self,
        last_changes: Vec<P4LastChange<R>>,
    ) -> Result<HashMap<MPath, P4PathRevision<R>>, MononokeError> {
        let changesets = last_changes
            .iter()
            .map(|change| (change.changeset.id(), change.changeset.clone()))
            .collect::<HashMap<_, _>>();
        let revisions = stream::iter(changesets.into_values())
            .map(|changeset| async move {
                let revisions = changeset.p4_file_revisions().await?;
                Ok::<_, MononokeError>((changeset.id(), revisions))
            })
            .buffer_unordered(P4_EXTRAS_CONCURRENCY)
            .try_collect::<HashMap<_, _>>()
            .await?;

        let mut found = HashMap::new();
        for change in last_changes {
            let Some(Some(revisions)) = revisions.get(&change.changeset.id()) else {
                continue;
            };
            let Some(path) = change.path.clone().into_optional_non_root_path() else {
                continue;
            };
            let revision = if change.is_file {
                revisions.file_revision(&path)?
            } else {
                match revisions.revision(&path)? {
                    Some(revision) => revision,
                    None => continue,
                }
            };
            found.insert(
                change.path,
                P4PathRevision {
                    changeset: change.changeset,
                    changelist: revisions.changelist(),
                    revision,
                    exists: change.exists,
                    path,
                },
            );
        }
        Ok(found)
    }
}

/// What a search of a path's history for a Perforce revision found.
pub struct P4RevisionSearch<R> {
    /// The change that gave the path the revision, if the search reached it.
    pub found: Option<P4PathRevision<R>>,
    /// The revision the path has at the changeset the search started from, a deletion
    /// included. `None` if nothing ever existed at the path there.
    pub head_revision: Option<u32>,
    /// With `head_revision`: whether the path exists there, or that revision deleted it.
    pub head_exists: Option<bool>,
    /// Set when the search walked `max_commits` changes without reaching the revision:
    /// the oldest revision it saw.
    pub oldest_revision_walked: Option<u32>,
    /// How many changes to the path the search read.
    pub commits_walked: usize,
}

/// How many changes of a path are loaded ahead while walking its history.
const P4_HISTORY_LOOKAHEAD: usize = 10;

impl<R: MononokeRepo> ChangesetPathHistoryContext<R> {
    /// Searches this path's history from this changeset, newest first and across
    /// deletions, for the change whose changelist gave the path Perforce revision
    /// `revision`, reading at most `max_commits` changes.
    ///
    /// Assumes a linear import, one stream per repo: revisions of a depot path then only
    /// go up along the history, so the search stops as soon as it passes a revision below
    /// `revision`, which is then not in the history. (Merges would break that order;
    /// `p4 obliterate` of a whole file restarts its numbering, so a revision from before
    /// one is reported not found.) Mutable renames are not followed: revisions belong to a
    /// depot path. The search also stops where the path was a directory: directories
    /// have no revisions.
    ///
    /// Every commit in a Perforce mirror carries the importer's revisions; one in the walk
    /// without them is an internal error. Every change read costs a `ChangesetInfo`
    /// fetch, so a miss deep in a much-changed file costs up to `max_commits` of them
    /// until a `(path, revision)` index replaces the walk behind this signature.
    pub async fn find_p4_revision(
        &self,
        revision: u32,
        max_commits: usize,
    ) -> Result<P4RevisionSearch<R>, MononokeError> {
        let path = self.file_path()?;
        let head_exists = match self.unode_id().await? {
            Some(Entry::Tree(_)) => {
                return Err(MononokeError::InvalidRequest(format!(
                    "{path} is a directory: only files have Perforce revisions"
                )));
            }
            Some(Entry::Leaf(_)) => true,
            None => false,
        };
        let history = self
            .history(ChangesetPathHistoryOptions {
                follow_history_across_deletions: true,
                ..Default::default()
            })
            .await?;
        let stream_path = path.clone();
        let mut changes = history
            .map_ok(move |changeset| {
                let path = stream_path.clone();
                async move {
                    let revisions = changeset.p4_file_revisions().await?.ok_or_else(|| {
                        MononokeError::from(anyhow!(
                            "commit {} changed {} but records no Perforce revisions",
                            changeset.id(),
                            path
                        ))
                    })?;
                    let rev = revisions.revision(&path)?;
                    Ok((changeset, revisions.changelist(), rev))
                }
            })
            .try_buffered(P4_HISTORY_LOOKAHEAD)
            .boxed();

        let mut search = P4RevisionSearch {
            found: None,
            head_revision: None,
            head_exists: None,
            oldest_revision_walked: None,
            commits_walked: 0,
        };
        while let Some((changeset, changelist, rev)) = changes.try_next().await? {
            let Some(rev) = rev else {
                // No revision: the path was a directory in that change. At the head, the
                // request is for a deleted directory; deeper, the file's revisions end.
                if search.commits_walked == 0 {
                    return Err(MononokeError::InvalidRequest(format!(
                        "{path} was a directory: only files have Perforce revisions"
                    )));
                }
                break;
            };
            search.commits_walked += 1;
            if search.head_revision.is_none() {
                search.head_revision = Some(rev);
                search.head_exists = Some(head_exists);
            }
            if rev == revision {
                // The change that gave the revision either left a file or deleted it.
                let exists = changeset
                    .path_with_history(path.clone())
                    .await?
                    .is_file()
                    .await?;
                search.found = Some(P4PathRevision {
                    changeset,
                    changelist,
                    revision,
                    exists,
                    path,
                });
                break;
            }
            if rev < revision {
                break;
            }
            if search.commits_walked >= max_commits {
                search.oldest_revision_walked = Some(rev);
                break;
            }
        }
        Ok(search)
    }

    /// Only files have Perforce revisions.
    fn file_path(&self) -> Result<NonRootMPath, MononokeError> {
        self.path()
            .clone()
            .into_optional_non_root_path()
            .ok_or_else(|| {
                MononokeError::InvalidRequest(
                    "the repo root is not a file: only files have Perforce revisions".to_string(),
                )
            })
    }

    /// The Perforce revision this path has as of this changeset, given its last change
    /// as `last_modified` (`exists`) or `last_deleted` (`!exists`) found it: the revision
    /// that change gave it, a deletion included. `None` if the path is a directory or the
    /// change records no Perforce revisions (the repo is not a Perforce mirror).
    pub async fn p4_revision(
        &self,
        last_change: &ChangesetContext<R>,
        exists: bool,
    ) -> Result<Option<P4PathRevision<R>>, MononokeError> {
        let is_file = exists && self.is_file().await?;
        let mut found = self
            .repo_ctx()
            .p4_path_revisions(vec![P4LastChange {
                path: self.path().clone(),
                changeset: last_change.clone(),
                exists,
                is_file,
            }])
            .await?;
        Ok(found.remove(self.path()))
    }
}
