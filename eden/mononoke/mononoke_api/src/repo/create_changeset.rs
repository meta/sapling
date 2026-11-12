/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;

use anyhow::anyhow;
use blobstore::Loadable;
use bonsai_globalrev_mapping::BonsaiGlobalrevMappingRef;
use bytes::Bytes;
use chrono::DateTime;
use chrono::FixedOffset;
use commit_graph::CommitGraphRef;
use commit_graph::CommitGraphWriterRef;
use context::CoreContext;
use ephemeral_blobstore::Bubble;
use filestore::FetchKey;
use filestore::FilestoreConfig;
use filestore::FilestoreConfigRef;
use filestore::StoreRequest;
use futures::StreamExt;
use futures::pin_mut;
use futures::stream;
use futures::stream::Stream;
use futures::stream::TryStreamExt;
use futures::try_join;
use futures_stats::TimedFutureExt;
use futures_stats::TimedTryFutureExt;
use itertools::Itertools;
use manifest::Manifest;
use manifest::PathTree;
use metaconfig_types::RepoConfigRef;
use mononoke_types::BonsaiChangeset;
use mononoke_types::BonsaiChangesetMut;
use mononoke_types::ChangesetId;
use mononoke_types::DateTime as MononokeDateTime;
use mononoke_types::FileChange;
use mononoke_types::GitLfs;
use mononoke_types::MPathElement;
use mononoke_types::NonRootMPath;
use mononoke_types::path::MPath;
use repo_authorization::RepoWriteOperation;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use repo_identity::RepoIdentityRef;
use repo_update_logger::CommitInfo;
use repo_update_logger::log_new_commits;
use restricted_paths::RestrictedPathsArc;
use scuba_ext::FutureStatsScubaExt;
use smallvec::SmallVec;
use sorted_vector_map::SortedVectorMap;

use crate::MononokeRepo;
use crate::changeset::ChangesetContext;
use crate::errors::MononokeError;
use crate::file::FileId;
use crate::file::FileType;
use crate::path::MononokePathPrefixes;
use crate::repo::RepoContext;
use crate::restricted_paths::RestrictedPathsPolicy;
use crate::specifiers::ChangesetSpecifier;

#[derive(Clone, Debug)]
pub struct CreateCopyInfo {
    path: MPath,
    parent_index: usize,
}

impl CreateCopyInfo {
    pub fn new(path: MPath, parent_index: usize) -> Self {
        CreateCopyInfo { path, parent_index }
    }

    /// Get the source path for the copy.
    pub fn path(&self) -> &MPath {
        &self.path
    }

    async fn check_valid<R: MononokeRepo>(
        &self,
        stack_changes: Option<&PathTree<CreateChangeType>>,
        stack_parents: &[ChangesetContext<R>],
        copy_from_path_mode: CreateChangesetCheckMode,
    ) -> Result<(), MononokeError> {
        if let Some(stack_changes) = stack_changes {
            // Since this is a stacked commit, there is only one parent.
            if self.parent_index > 0 {
                return Err(MononokeError::InvalidRequest(format!(
                    "Parent index '{}' out of range for stacked commit",
                    self.parent_index,
                )));
            }
            // Check if the copy-from path was added or removed in the stack.
            match stack_changes.get(&self.path) {
                None | Some(CreateChangeType::None) => {}
                Some(CreateChangeType::Change) => {
                    return Ok(());
                }
                Some(CreateChangeType::Deletion) => {
                    return Err(MononokeError::InvalidRequest(String::from(
                        "Copy-from path references a file deleted earlier in the stack",
                    )));
                }
            }
            // Check if the copy-from path was deleted by a prefix change.
            for prefix in MononokePathPrefixes::new(&self.path) {
                if stack_changes.get(&prefix) == Some(&CreateChangeType::Change) {
                    return Err(MononokeError::InvalidRequest(String::from(
                        "Copy-from path references a file in a directory deleted earlier in the stack",
                    )));
                }
            }
            // The remaining branch checks the parent's content manifest, so
            // skip it when the bypass is requested.
            if matches!(copy_from_path_mode, CreateChangesetCheckMode::Skip) {
                return Ok(());
            }
            // The copy-from path wasn't touched in the stack, check it was in
            // at least one of the stack's parents.
            for parent_ctx in stack_parents {
                if parent_ctx
                    .path_with_content(self.path.clone())
                    .await?
                    .is_file()
                    .await?
                {
                    return Ok(());
                }
            }
        } else {
            // This is the root of the stack.  Check the specific parent.
            let parent_ctx = stack_parents.get(self.parent_index).ok_or_else(|| {
                MononokeError::InvalidRequest(format!(
                    "Parent index '{}' out of range for commit with {} parent(s)",
                    self.parent_index,
                    stack_parents.len()
                ))
            })?;
            if matches!(copy_from_path_mode, CreateChangesetCheckMode::Skip) {
                return Ok(());
            }
            // Check the file exists in that parent.
            if parent_ctx
                .path_with_content(self.path.clone())
                .await?
                .is_file()
                .await?
            {
                return Ok(());
            }
        };

        Err(MononokeError::InvalidRequest(String::from(
            "Copy-from path must reference a file",
        )))
    }

    fn into_file_change(
        self,
        parent_ids: &[ChangesetId],
    ) -> Result<(NonRootMPath, ChangesetId), MononokeError> {
        let mpath = self.path.into_optional_non_root_path().ok_or_else(|| {
            MononokeError::InvalidRequest(String::from("Copy-from path cannot be the root"))
        })?;
        let parent_id = parent_ids.get(self.parent_index).ok_or_else(|| {
            MononokeError::InvalidRequest(format!(
                "Parent index '{}' out of range for commit with {} parent(s)",
                self.parent_index,
                parent_ids.len()
            ))
        })?;
        Ok((mpath, *parent_id))
    }
}

/// Description of a change to make to a file.
#[derive(Clone, Debug)]
pub enum CreateChange {
    /// The file is created or modified to contain new data.
    Tracked(CreateChangeFile, Option<CreateCopyInfo>),

    /// A new file is modified in the working copy
    Untracked(CreateChangeFile),

    /// The file is not present in the working copy
    UntrackedDeletion,

    /// The file is marked as deleted
    Deletion,
}

#[derive(Clone, Debug)]
pub enum CreateChangeGitLfs {
    FullContent,
    GitLfsPointer {
        non_canonical_pointer: Option<CreateChangeFileContents>,
    },
}

fn try_into_git_lfs(
    create_change_git_lfs: Option<CreateChangeGitLfs>,
) -> Result<GitLfs, MononokeError> {
    let git_lfs = match create_change_git_lfs {
        None => GitLfs::full_content(),
        Some(CreateChangeGitLfs::FullContent) => GitLfs::full_content(),
        Some(CreateChangeGitLfs::GitLfsPointer {
            non_canonical_pointer:
                Some(CreateChangeFileContents::Existing {
                    file_id,
                    maybe_size: _size,
                }),
        }) => GitLfs::non_canonical_pointer(file_id),
        Some(CreateChangeGitLfs::GitLfsPointer {
            non_canonical_pointer: None,
        }) => GitLfs::canonical_pointer(),
        _ => return Err(anyhow!("Programming error: create change must be resolved first").into()),
    };
    Ok(git_lfs)
}

#[derive(Clone, Debug)]
pub struct CreateChangeFile {
    pub contents: CreateChangeFileContents,
    /// File type. If `None`, will be inherited from the copy source or parent.
    pub file_type: Option<FileType>,
    // If missing then server decides whether to use git lfs or not
    pub git_lfs: Option<CreateChangeGitLfs>,
}

#[derive(Clone, Debug)]
pub enum CreateChangeFileContents {
    // Upload content from bytes
    New {
        bytes: Bytes,
    },
    // Use already uploaded content
    Existing {
        file_id: FileId,
        // If not present, will be fetched from the blobstore
        maybe_size: Option<u64>,
    },
}

impl CreateChangeFileContents {
    pub(crate) async fn resolve(
        &mut self,
        ctx: &CoreContext,
        filestore_config: FilestoreConfig,
        repo_blobstore: RepoBlobstore,
    ) -> Result<(), MononokeError> {
        match self {
            CreateChangeFileContents::New { bytes } => {
                let meta = filestore::store(
                    &repo_blobstore,
                    filestore_config,
                    ctx,
                    &StoreRequest::new(bytes.len() as u64),
                    stream::once(async move { Ok(bytes.clone()) }),
                )
                .await?;
                *self = CreateChangeFileContents::Existing {
                    file_id: meta.content_id,
                    maybe_size: Some(meta.total_size),
                };
            }
            CreateChangeFileContents::Existing {
                file_id,
                maybe_size,
                ..
            } => {
                if maybe_size.is_none() {
                    let size = filestore::get_metadata(
                        &repo_blobstore,
                        ctx,
                        &FetchKey::Canonical(*file_id),
                    )
                    .await?
                    .ok_or_else(|| {
                        MononokeError::InvalidRequest(format!(
                            "File id '{file_id}' is not available in this repo"
                        ))
                    })?
                    .total_size;
                    *maybe_size = Some(size);
                }
            }
        }
        Ok(())
    }
}

impl CreateChangeFile {
    /// Get the content ID if contents are resolved (Existing variant)
    pub fn content_id(&self) -> Option<FileId> {
        match &self.contents {
            CreateChangeFileContents::Existing { file_id, .. } => Some(*file_id),
            CreateChangeFileContents::New { .. } => None,
        }
    }

    /// Get the size if contents are resolved
    pub fn size(&self) -> Option<u64> {
        match &self.contents {
            CreateChangeFileContents::Existing { maybe_size, .. } => *maybe_size,
            CreateChangeFileContents::New { bytes } => Some(bytes.len() as u64),
        }
    }
}

#[cfg(test)]
impl CreateChangeFile {
    // constructor that makes tests more ergonomic
    pub fn new_regular(contents: &'static str) -> Self {
        CreateChangeFile {
            contents: CreateChangeFileContents::New {
                bytes: Bytes::from(contents),
            },
            file_type: Some(FileType::Regular),
            git_lfs: None,
        }
    }
}

// Enum for recording whether a path is not changed, changed or deleted.
#[derive(Copy, Clone, Default, Eq, PartialEq, Debug)]
pub(crate) enum CreateChangeType {
    #[default]
    None,
    Change,
    Deletion,
}

impl CreateChangeType {
    fn is_modification(&self) -> bool {
        match self {
            Self::None => false,
            Self::Change => true,
            Self::Deletion => true,
        }
    }
}

#[derive(Copy, Clone, Default, Eq, PartialEq, Debug)]
pub(crate) enum StackContentChange {
    #[default]
    None,
    Change(FileId),
    Deletion,
}

impl CreateChange {
    pub(crate) async fn resolve<R: MononokeRepo>(
        &mut self,
        ctx: &CoreContext,
        filestore_config: FilestoreConfig,
        repo_blobstore: RepoBlobstore,
        stack_changes: Option<&PathTree<CreateChangeType>>,
        stack_parents: &[ChangesetContext<R>],
        copy_from_path_mode: CreateChangesetCheckMode,
    ) -> Result<(), MononokeError> {
        let file = match self {
            CreateChange::Tracked(file, copy_info) => {
                if let Some(copy_info) = copy_info {
                    copy_info
                        .check_valid(stack_changes, stack_parents, copy_from_path_mode)
                        .await?;
                }
                file
            }
            CreateChange::Untracked(file) => file,
            CreateChange::UntrackedDeletion | CreateChange::Deletion => return Ok(()),
        };
        if let Some(CreateChangeGitLfs::GitLfsPointer {
            non_canonical_pointer: Some(non_canonical_pointer),
        }) = &mut file.git_lfs
        {
            non_canonical_pointer
                .resolve(ctx, filestore_config, repo_blobstore.clone())
                .await?;
        }
        file.contents
            .resolve(ctx, filestore_config, repo_blobstore)
            .await?;
        Ok(())
    }

    /// Try to resolve file_type from stack_file_types.
    ///
    /// Resolution order:
    /// 1. Copy source path in stack_file_types (files from earlier in the stack)
    /// 2. Self path in stack_file_types
    ///
    /// Returns true if resolved, false if parent lookup is needed.
    pub(crate) fn resolve_file_type_from_stack(
        &mut self,
        path: &NonRootMPath,
        stack_file_types: &BTreeMap<NonRootMPath, FileType>,
    ) -> bool {
        // Early return for deletions - they don't need file_type
        if matches!(
            self,
            CreateChange::Deletion | CreateChange::UntrackedDeletion
        ) {
            return true;
        }

        let lookup_path = self.copy_source_or_path(path);

        let file = match self {
            CreateChange::Tracked(file, _) | CreateChange::Untracked(file) => file,
            _ => return true,
        };

        // Already resolved
        if file.file_type.is_some() {
            return true;
        }

        // Try resolving from earlier stack commit
        if let Some(ft) = stack_file_types.get(&lookup_path) {
            file.file_type = Some(*ft);
            return true;
        }

        false
    }

    /// Get the path to look up for file_type resolution.
    /// Returns copy source path if present, otherwise self path.
    pub(crate) fn copy_source_or_path(&self, path: &NonRootMPath) -> NonRootMPath {
        match self {
            CreateChange::Tracked(_, Some(copy_info)) => copy_info
                .path()
                .clone()
                .into_optional_non_root_path()
                .unwrap_or_else(|| path.clone()),
            _ => path.clone(),
        }
    }

    /// Get an immutable reference to the file if this is a file change (Tracked or Untracked).
    pub(crate) fn file(&self) -> Option<&CreateChangeFile> {
        match self {
            CreateChange::Tracked(file, _) | CreateChange::Untracked(file) => Some(file),
            _ => None,
        }
    }

    /// Get a mutable reference to the file if this is a file change (Tracked or Untracked).
    pub(crate) fn file_mut(&mut self) -> Option<&mut CreateChangeFile> {
        match self {
            CreateChange::Tracked(file, _) | CreateChange::Untracked(file) => Some(file),
            _ => None,
        }
    }

    /// Check if this change needs file_type resolution.
    pub(crate) fn needs_file_type_resolution(&self) -> bool {
        self.file().is_some_and(|f| f.file_type.is_none())
    }

    /// Set the file_type for this change.
    pub(crate) fn set_file_type(&mut self, file_type: FileType) {
        if let Some(f) = self.file_mut() {
            f.file_type = Some(file_type);
        }
    }

    /// Get the current file_type (if any).
    pub(crate) fn file_type(&self) -> Option<FileType> {
        self.file().and_then(|f| f.file_type)
    }
}

/// Look up file types from parent contexts for the given paths.
///
/// This function performs a batched lookup of file types from parent contexts.
/// It tries each parent in order until all paths are resolved.
///
/// Returns a map from path to file type for the paths that were found.
pub(crate) async fn lookup_file_types_from_parents<R: MononokeRepo>(
    paths: impl IntoIterator<Item = NonRootMPath>,
    parent_contexts: &[ChangesetContext<R>],
) -> Result<BTreeMap<NonRootMPath, FileType>, MononokeError> {
    let unique_paths: BTreeSet<NonRootMPath> = paths.into_iter().collect();
    let mut resolved: BTreeMap<NonRootMPath, FileType> = BTreeMap::new();

    if unique_paths.is_empty() {
        return Ok(resolved);
    }

    // Try each parent in order, collecting types for unresolved paths
    for parent_ctx in parent_contexts {
        if resolved.len() == unique_paths.len() {
            break;
        }

        // Query paths not yet resolved
        let paths_to_query: Vec<MPath> = unique_paths
            .iter()
            .filter(|p| !resolved.contains_key(*p))
            .map(|p| MPath::from(p.clone()))
            .collect();

        let path_contexts = parent_ctx
            .paths_with_content(paths_to_query.into_iter(), &RestrictedPathsPolicy::Strict)
            .await?;

        pin_mut!(path_contexts);
        while let Some(path_ctx) = path_contexts.try_next().await? {
            if let Some(ft) = path_ctx.file_type().await? {
                if let Some(non_root) = path_ctx.path().clone().into_optional_non_root_path() {
                    resolved.insert(non_root, ft);
                }
            }
        }
    }

    Ok(resolved)
}

impl CreateChange {
    pub fn into_file_change(self, parent_ids: &[ChangesetId]) -> Result<FileChange, MononokeError> {
        match self {
            CreateChange::Tracked(
                CreateChangeFile {
                    contents:
                        CreateChangeFileContents::Existing {
                            file_id,
                            maybe_size: Some(size),
                        },
                    file_type: Some(file_type),
                    git_lfs,
                },
                copy_info,
            ) => Ok(FileChange::tracked(
                file_id,
                file_type,
                size,
                copy_info
                    .map(|copy_info| copy_info.into_file_change(parent_ids))
                    .transpose()?,
                try_into_git_lfs(git_lfs)?,
            )),
            CreateChange::Untracked(CreateChangeFile {
                contents:
                    CreateChangeFileContents::Existing {
                        file_id,
                        maybe_size: Some(size),
                    },
                file_type: Some(file_type),
                git_lfs: None,
            }) => Ok(FileChange::untracked(file_id, file_type, size)),
            CreateChange::UntrackedDeletion => Ok(FileChange::UntrackedDeletion),
            CreateChange::Untracked(CreateChangeFile {
                git_lfs: Some(_git_lfs),
                ..
            }) => Err(anyhow!("Error: git_lfs not supported for untracked changes").into()),
            CreateChange::Deletion => Ok(FileChange::Deletion),
            _ => Err(anyhow!("Programming error: create change must be resolved first").into()),
        }
    }

    pub(crate) fn change_type(&self) -> CreateChangeType {
        match self {
            CreateChange::Deletion | CreateChange::UntrackedDeletion => CreateChangeType::Deletion,
            CreateChange::Tracked(..) | CreateChange::Untracked(..) => CreateChangeType::Change,
        }
    }

    fn content_id(&self) -> Result<Option<FileId>, MononokeError> {
        match self {
            CreateChange::Tracked(
                CreateChangeFile {
                    contents: CreateChangeFileContents::Existing { file_id, .. },
                    ..
                },
                ..,
            ) => Ok(Some(*file_id)),
            CreateChange::Untracked(CreateChangeFile {
                contents: CreateChangeFileContents::Existing { file_id, .. },
                ..
            }) => Ok(Some(*file_id)),
            CreateChange::UntrackedDeletion => Ok(None),
            CreateChange::Untracked(CreateChangeFile {
                git_lfs: Some(_git_lfs),
                ..
            }) => Err(anyhow!("Error: git_lfs not supported for untracked changes").into()),
            CreateChange::Deletion => Ok(None),
            _ => Err(anyhow!("Programming error: create change must be resolved first").into()),
        }
    }
}

/// Commit info for a newly created commit.
pub struct CreateInfo {
    pub author: String,
    pub author_date: DateTime<FixedOffset>,
    pub committer: Option<String>,
    pub committer_date: Option<DateTime<FixedOffset>>,
    pub message: String,
    pub extra: BTreeMap<String, Vec<u8>>,
    pub git_extra_headers: Option<BTreeMap<SmallVec<[u8; 24]>, Bytes>>,
}

/// Extra key that records which bubble a snapshot was created in.
///
/// Snapshot IDs are content hashes. Without this key, two identical snapshots
/// created in different bubbles get the same ID, so one ID maps to several
/// bubbles. Including the bubble ID in the hashed content gives each bubble
/// its own snapshot ID.
pub const SNAPSHOT_BUBBLE_ID_EXTRA: &str = "snapshot_bubble_id";

/// Verify that all deleted files existed in at least one of the parents.
pub(crate) async fn verify_deleted_files_existed_in_a_parent<R: MononokeRepo>(
    parent_ctxs: &[ChangesetContext<R>],
    stack_changes: Option<&PathTree<CreateChangeType>>,
    mut deleted_files: BTreeSet<MPath>,
    check_mode: CreateChangesetCheckMode,
) -> Result<BTreeSet<MPath>, MononokeError> {
    if check_mode == CreateChangesetCheckMode::Skip {
        return Ok(Default::default());
    }

    async fn get_matching_files<'a, R: MononokeRepo>(
        parent_ctx: &'a ChangesetContext<R>,
        files: &'a BTreeSet<MPath>,
    ) -> Result<impl Stream<Item = Result<MPath, MononokeError>> + 'a, MononokeError> {
        Ok(parent_ctx
            .paths(files.iter().cloned())
            .await?
            .try_filter_map(async |changeset_path| {
                if changeset_path.is_file().await? {
                    Ok(Some(changeset_path.path().clone()))
                } else {
                    Ok(None)
                }
            })
            .boxed())
    }

    let mut deletions_to_remove: BTreeSet<MPath> = Default::default();

    if let Some(stack_changes) = stack_changes {
        // Ignore files that were created or modified earlier in the stack.
        deleted_files.retain(|deleted_file| {
            stack_changes.get(deleted_file) != Some(&CreateChangeType::Change)
        });

        for deleted_file in deleted_files.iter() {
            // It's an error if this file was already deleted, or if any of
            // its path prefixes were created (this implicitly deletes the
            // directory).
            if stack_changes.get(deleted_file) == Some(&CreateChangeType::Deletion) {
                if check_mode == CreateChangesetCheckMode::Fix {
                    deletions_to_remove.insert(deleted_file.clone());
                } else {
                    return Err(MononokeError::InvalidRequest(format!(
                        "Deleted file '{deleted_file}' was deleted earlier in the stack"
                    )));
                }
            }
            for prefix in MononokePathPrefixes::new(deleted_file) {
                if let Some(CreateChangeType::Change) = stack_changes.get(&prefix) {
                    if check_mode == CreateChangesetCheckMode::Fix {
                        deletions_to_remove.insert(deleted_file.clone());
                    } else {
                        return Err(MononokeError::InvalidRequest(format!(
                            "Deleted file '{deleted_file}' was deleted earlier in the stack through replacement of '{prefix}'"
                        )));
                    }
                }
            }
        }
    }

    // Filter the deleted files to those that existed in a parent.
    let deleted_files = &deleted_files;
    let parent_files: BTreeSet<_> = stream::iter(
        parent_ctxs
            .iter()
            .map(async |parent_ctx| get_matching_files(parent_ctx, deleted_files).await),
    )
    .boxed()
    .buffered(10)
    .try_flatten()
    .try_collect()
    .await?;

    // Quickly check if all deleted files existed by comparing set lengths.
    if deleted_files.len() == parent_files.len() {
        Ok(deletions_to_remove)
    } else if check_mode == CreateChangesetCheckMode::Fix {
        deletions_to_remove.extend(deleted_files.difference(&parent_files).cloned());
        Ok(deletions_to_remove)
    } else {
        // At least one deleted file didn't exist. Find out which ones to
        // give a good error message.
        let non_existent_path = deleted_files
            .difference(&parent_files)
            .next()
            .expect("at least one file did not exist");
        let path_count = deleted_files.len().saturating_sub(parent_files.len());
        if path_count == 1 {
            Err(MononokeError::InvalidRequest(format!(
                "Deleted file '{non_existent_path}' does not exist in any parent"
            )))
        } else {
            Err(MononokeError::InvalidRequest(format!(
                "{path_count} deleted files ('{non_existent_path}', ...) do not exist in any parent"
            )))
        }
    }
}

pub(crate) async fn is_noop_file_change<R: MononokeRepo>(
    parent_ctxs: &[ChangesetContext<R>],
    stack_changes: Option<&PathTree<StackContentChange>>,
    path: &NonRootMPath,
    change: &CreateChange,
) -> Result<bool, MononokeError> {
    let content_id = if let Some(content_id) = change.content_id()? {
        content_id
    } else {
        // Deletions are irrelevant for this check.
        return Ok(false);
    };

    // Check if the PathTree of stack changes cover this path i.e. the
    // state of this path is determined by the previous changes in the
    // stack and not by the parents.
    if let Some(stack_changes) = stack_changes
        && let Some(stack_change) = stack_changes.get(path.as_mpath())
    {
        if *stack_change == StackContentChange::Change(content_id) {
            return Ok(true);
        }
    } else if let Some(stack_changes) = stack_changes
        && stack_changes
            .get_nearest_parent(path.as_mpath(), |change| {
                *change != StackContentChange::None
            })
            .is_some()
    {
        return Ok(false);
    } else {
        let parents = stream::iter(parent_ctxs)
            .map(Ok::<_, MononokeError>)
            .map_ok(async |parent_ctx| {
                let file_ctx = parent_ctx
                    .path_with_content(path.as_mpath().clone())
                    .await?
                    .file()
                    .await?;

                if let Some(file_ctx) = file_ctx {
                    Ok(Some(file_ctx.id().await?))
                } else {
                    Ok(None)
                }
            })
            .boxed()
            .try_buffered(10)
            // Filter out parents that don't have the file.
            .try_filter_map(futures::future::ok)
            .try_collect::<Vec<_>>()
            .await?;

        // If all parents have the same content id and it's the same as
        // the current content id, then it's a no-op change
        if let Ok(parent_content_id) = parents.into_iter().all_equal_value() {
            if parent_content_id == content_id {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(crate) async fn verify_no_noop_file_changes<R: MononokeRepo>(
    parent_ctxs: &[ChangesetContext<R>],
    stack_changes: Option<PathTree<StackContentChange>>,
    file_changes: &SortedVectorMap<NonRootMPath, CreateChange>,
) -> Result<(), MononokeError> {
    stream::iter(file_changes)
        .map(Ok)
        .try_for_each_concurrent(10, async |(path, change)| {
            if is_noop_file_change(parent_ctxs, stack_changes.as_ref(), path, change).await? {
                return Err(MononokeError::InvalidRequest(format!(
                    "Found no-op file change at path '{path}'. File changes that don't change file content are not allowed"
                )));
            }
            Ok(())
        })
        .await
}

async fn remove_noop_file_changes<R: MononokeRepo>(
    parent_ctxs: &[ChangesetContext<R>],
    stack_changes: Option<PathTree<StackContentChange>>,
    file_changes: SortedVectorMap<NonRootMPath, CreateChange>,
) -> Result<SortedVectorMap<NonRootMPath, CreateChange>, MononokeError> {
    stream::iter(file_changes)
        .map(Ok)
        .map_ok(async |(path, change)| {
            let is_noop =
                is_noop_file_change(parent_ctxs, stack_changes.as_ref(), &path, &change).await?;
            Ok((is_noop, path, change))
        })
        .try_buffered(10)
        .try_filter_map(async |(is_noop, path, change)| {
            if is_noop {
                Ok(None)
            } else {
                Ok(Some((path, change)))
            }
        })
        .try_collect()
        .await
}

/// Returns `true` if any prefix of the path has a change.  Use for
/// detecting when a directory is replaced by a file.
pub(crate) fn is_prefix_changed(path: &MPath, paths: &PathTree<CreateChangeType>) -> bool {
    MononokePathPrefixes::new(path)
        .any(|prefix| paths.get(&prefix) == Some(&CreateChangeType::Change))
}

/// Verify that any files in `prefix_paths` that exist in any of
/// `parent_ctxs`, as modified by the existing stack changes, have been marked
/// as deleted in `path_changes`.
///
/// `prefix_files_deleted_mode` only controls the parent-traversal half of the
/// check (which depends on derived data of the parents). The stack-local half
/// always runs because it is cheap and self-consistent.
pub(crate) async fn verify_prefix_files_deleted<R: MononokeRepo>(
    parent_ctxs: &[ChangesetContext<R>],
    stack_changes: Option<&PathTree<CreateChangeType>>,
    mut prefix_paths: BTreeSet<MPath>,
    path_changes: &PathTree<CreateChangeType>,
    prefix_files_deleted_mode: CreateChangesetCheckMode,
) -> Result<(), MononokeError> {
    if let Some(stack_changes) = stack_changes {
        // Remove any prefix paths that have already been deleted earlier in the stack.
        prefix_paths.retain(|prefix_path| {
            stack_changes.get(prefix_path) != Some(&CreateChangeType::Deletion)
        });
        // Check that any prefix path added earlier in the stack is being deleted.
        for prefix_path in prefix_paths.iter() {
            if stack_changes.get(prefix_path) == Some(&CreateChangeType::Change)
                && path_changes.get(prefix_path) != Some(&CreateChangeType::Deletion)
            {
                return Err(MononokeError::InvalidRequest(format!(
                    concat!(
                        "Creating files inside '{}' requires deleting the file ",
                        "added earlier in the stack at that path"
                    ),
                    prefix_path
                )));
            }
        }
    }
    if matches!(prefix_files_deleted_mode, CreateChangesetCheckMode::Skip) {
        return Ok(());
    }
    // Check that any prefix path that exists in any parent is being deleted.
    stream::iter(parent_ctxs.iter().map(Ok))
        .try_for_each_concurrent(10, async |parent_ctx| {
            parent_ctx
                .paths(prefix_paths.iter().cloned())
                .await?
                .try_for_each(async |prefix_path| {
                    if prefix_path.is_file().await?
                        && path_changes.get(prefix_path.path()) != Some(&CreateChangeType::Deletion)
                    {
                        Err(MononokeError::InvalidRequest(format!(
                            "Creating files inside '{}' requires deleting the file at that path",
                            prefix_path.path()
                        )))
                    } else {
                        Ok(())
                    }
                })
                .await
        })
        .await
}

async fn check_addless_union_conflicts<R: MononokeRepo>(
    ctx: &CoreContext,
    repo_blobstore: RepoBlobstore,
    changesets: &[ChangesetContext<R>],
    fix_paths: &PathTree<CreateChangeType>,
) -> Result<(), MononokeError> {
    if changesets.len() < 2 {
        return Ok(());
    }

    let root_manifest_ids: Vec<_> = stream::iter(
        changesets
            .iter()
            .map(|cs_ctx| async move { cs_ctx.root_content_manifest_id().await }),
    )
    .boxed()
    .buffered(10)
    .try_collect()
    .await?;

    let store = &repo_blobstore;

    let conflict_paths = bounded_traversal::bounded_traversal_stream(
        256,
        Some((root_manifest_ids, MPath::ROOT)),
        move |(manifest_ids_to_check, current_path)| {
            Box::pin(async move {
                let mut leaf_content: BTreeMap<MPathElement, HashSet<_>> = BTreeMap::new();
                let mut trees: BTreeMap<MPathElement, BTreeSet<_>> = BTreeMap::new();

                for manifest_id in manifest_ids_to_check {
                    let manifest = manifest_id.load(ctx, store).await?;
                    let entries: Vec<_> = manifest.list(ctx, store).await?.try_collect().await?;
                    for (path_element, entry) in entries {
                        match entry {
                            manifest::Entry::Tree(id) => {
                                trees.entry(path_element.clone()).or_default().insert(id)
                            }
                            manifest::Entry::Leaf(file) => leaf_content
                                .entry(path_element.clone())
                                .or_default()
                                .insert(file),
                        };
                    }
                }

                // Conflict rules only apply to leaves. A path in `fix_paths` means no conflict
                //
                // Two rules:
                // 1. If there are multiple choices for content, then there's a conflict
                // 2. If there's a tree and a leaf for this path, then there's a conflict
                let conflicts: Vec<_> = leaf_content
                    .into_iter()
                    .filter_map(|(path_element, contents)| {
                        let path = current_path.join_element(Some(&path_element));
                        let fix_exists = fix_paths
                            .get(&path)
                            .is_some_and(CreateChangeType::is_modification);
                        let conflict_exists =
                            contents.len() > 1 || trees.contains_key(&path_element);
                        if !fix_exists && conflict_exists {
                            Some(path)
                        } else {
                            None
                        }
                    })
                    .collect();
                // Recurse into trees that might reveal more conflicts.
                // If we already have new content for a path, then we don't recurse into it
                let recurse: Vec<_> = trees
                    .into_iter()
                    .filter_map(|(path_element, manifest_ids)| {
                        let path = current_path.join_element(Some(&path_element));
                        let fix_exists = fix_paths
                            .get(&path)
                            .is_some_and(CreateChangeType::is_modification);

                        if !fix_exists && manifest_ids.len() > 1 {
                            Some((manifest_ids.into_iter().collect(), path))
                        } else {
                            None
                        }
                    })
                    .collect();
                anyhow::Ok((conflicts, recurse))
            })
        },
    )
    .try_concat()
    .await?;

    if conflict_paths.is_empty() {
        Ok(())
    } else {
        Err(MononokeError::MergeConflicts { conflict_paths })
    }
}

pub struct CreatedChangeset<R> {
    pub hg_extras: SortedVectorMap<String, Vec<u8>>,
    pub changeset_ctx: ChangesetContext<R>,
}

#[derive(PartialEq, Eq, Copy, Clone, Debug)]
pub enum CreateChangesetCheckMode {
    /// Skip the check completely
    Skip,
    /// Make the check and fail the request if it fails
    Check,
    /// Fix the request to make the check pass
    Fix,
}

#[derive(Copy, Clone, Debug)]
pub struct CreateChangesetChecks {
    pub noop_file_changes: CreateChangesetCheckMode,
    pub deleted_files_existed_in_a_parent: CreateChangesetCheckMode,
    pub empty_changeset: CreateChangesetCheckMode,
    pub copy_from_path: CreateChangesetCheckMode,
    pub prefix_files_deleted: CreateChangesetCheckMode,
}

impl CreateChangesetChecks {
    /// Create a checks config that validates all conditions (the common case).
    pub fn check() -> Self {
        Self {
            noop_file_changes: CreateChangesetCheckMode::Check,
            deleted_files_existed_in_a_parent: CreateChangesetCheckMode::Check,
            empty_changeset: CreateChangesetCheckMode::Check,
            copy_from_path: CreateChangesetCheckMode::Check,
            prefix_files_deleted: CreateChangesetCheckMode::Check,
        }
    }

    /// Returns true if any check is set to `Skip`.
    pub fn any_bypass_requested(&self) -> bool {
        use CreateChangesetCheckMode::Skip;
        self.noop_file_changes == Skip
            || self.deleted_files_existed_in_a_parent == Skip
            || self.empty_changeset == Skip
            || self.copy_from_path == Skip
            || self.prefix_files_deleted == Skip
    }
}

impl<R: MononokeRepo> RepoContext<R> {
    pub async fn enforce_create_commit_check_bypass(
        &self,
        checks: &CreateChangesetChecks,
    ) -> Result<(), MononokeError> {
        if !checks.any_bypass_requested() {
            return Ok(());
        }
        if self
            .authorization_context()
            .check_create_commit_check_bypass(self.ctx(), self.repo())
            .await
            .is_permitted()
        {
            return Ok(());
        }
        let enforce = justknobs::eval(
            "scm/mononoke:create_commit_bypass_config_enforce",
            None,
            Some(self.name()),
        );
        let log_tag = if enforce {
            "create_commit bypass not permitted by service_write_restrictions"
        } else {
            "create_commit bypass would not be permitted by service_write_restrictions"
        };
        self.ctx().scuba().clone().log_with_msg(log_tag, None);
        if enforce {
            return Err(MononokeError::AuthorizationError(
                "create_commit bypass not permitted by service_write_restrictions".to_string(),
            ));
        }
        Ok(())
    }

    pub(crate) async fn save_changesets(
        &self,
        changesets: Vec<BonsaiChangeset>,
        repo: &(
             impl BonsaiGlobalrevMappingRef
             + CommitGraphRef
             + CommitGraphWriterRef
             + RepoBlobstoreRef
             + RepoIdentityRef
             + RepoConfigRef
         ),
        bubble: Option<&Bubble>,
    ) -> Result<(), MononokeError> {
        let bubble_id = bubble.map(|x| x.bubble_id());
        let commit_infos = changesets
            .iter()
            .map(|changeset| CommitInfo::new(changeset, bubble_id))
            .collect();
        changesets_creation::save_changesets(self.ctx(), repo, changesets).await?;

        log_new_commits(self.ctx(), repo, None, commit_infos).await;

        Ok(())
    }

    /// Create a new changeset in the repository.
    ///
    /// The new changeset is created with the given metadata by unioning the
    /// contents of all parent changesets and then applying the provided
    /// changes on top.
    ///
    /// Note that:
    ///   - The changes must be internally consistent (there must be no path
    ///     conflicts between changed files).
    ///   - If a file in any parent changeset is being replaced by a directory
    ///     then that file must be deleted in the set of changes.
    ///   - If a directory in any parent changeset is being replaced by a file,
    ///     then the contents of the parent directory do not need to be deleted.
    ///     If deletions for the contents of the directory are included they will
    ///     be checked for correctness (the files must exist), but they will
    ///     otherwise be ignored.
    ///   - Any merge conflicts introduced by merging the parent changesets
    ///     must be resolved by a corresponding change in the set of changes.
    pub async fn create_changeset(
        &self,
        parents: Vec<ChangesetId>,
        info: CreateInfo,
        changes: BTreeMap<MPath, CreateChange>,
        // If some, this changeset is a snapshot. Currently unsupported to upload a
        // normal commit to a bubble, though can be easily added.
        bubble: Option<&Bubble>,
        checks: CreateChangesetChecks,
    ) -> Result<CreatedChangeset<R>, MononokeError> {
        let changesets = self
            .create_changeset_stack(parents, vec![info], vec![changes], bubble, checks)
            .await?;
        changesets
            .into_iter()
            .exactly_one()
            .map_err(|e| anyhow!("Expected 1 changeset, but created {}", e.len()).into())
    }

    /// Create a new stack of changesets in the repository.
    ///
    /// The first new changeset is created with the given metadata by unioning the
    /// contents of all parent changesets and then applying the provided
    /// changes on top.  The subsequent changesets are then stacked on top of
    /// the first changeset.
    ///
    /// The requirements for `create_changeset` must be met for each changeset
    /// in the stack.
    pub async fn create_changeset_stack(
        &self,
        stack_parents: Vec<ChangesetId>,
        info_stack: Vec<CreateInfo>,
        changes_stack: Vec<BTreeMap<MPath, CreateChange>>,
        // If some, this changeset is a snapshot. Currently unsupported to upload a
        // normal commit to a bubble, though can be easily added.
        bubble: Option<&Bubble>,
        checks: CreateChangesetChecks,
    ) -> Result<Vec<CreatedChangeset<R>>, MononokeError> {
        self.start_write()?;
        self.authorization_context()
            .require_repo_write(self.ctx(), self.repo(), RepoWriteOperation::CreateChangeset)
            .await?;

        validate_acl_file_changes(self, &stack_parents, &changes_stack).await?;

        let allowed_no_parents = self
            .config()
            .source_control_service
            .permit_commits_without_parents;
        if !allowed_no_parents && stack_parents.is_empty() {
            return Err(MononokeError::InvalidRequest(String::from(
                "Changesets with no parents cannot be created",
            )));
        }

        // Obtain contexts for each of the parents (which should exist).
        let stack_parent_ctxs: Vec<_> = stream::iter(stack_parents.iter().map(async |parent_id| {
            let parent_ctx = self
                .changeset(ChangesetSpecifier::Bonsai(parent_id.clone()))
                .await?
                .ok_or_else(|| {
                    MononokeError::InvalidRequest(format!("Parent {parent_id} does not exist"))
                })?;
            Ok::<_, MononokeError>(parent_ctx)
        }))
        .boxed()
        .buffered(10)
        .try_collect()
        .await?;

        // Collect together information about each new commit.

        // Extract the set of deleted files.
        let tracked_deletion_files_stack: Vec<BTreeSet<_>> = changes_stack
            .iter()
            .map(|changes| {
                changes
                    .iter()
                    .filter(|(_path, change)| matches!(change, CreateChange::Deletion))
                    .map(|(path, _change)| path.clone())
                    .collect()
            })
            .collect();

        // Build a path tree recording each path that has been created or deleted.
        let path_changes_stack: Vec<_> = changes_stack
            .iter()
            .map(|changes| {
                PathTree::from_iter(
                    changes
                        .iter()
                        .map(|(path, change)| (path.clone(), change.change_type())),
                )
            })
            .collect();

        // Determine the prefixes of all changed files.
        let prefix_paths_stack: Vec<BTreeSet<_>> = changes_stack
            .iter()
            .map(|changes| {
                changes
                    .iter()
                    .filter(|(_path, change)| change.change_type() == CreateChangeType::Change)
                    .flat_map(|(path, _change)| MononokePathPrefixes::new(path))
                    .collect()
            })
            .collect();

        // Convert change paths into the form needed for the bonsai changeset.
        let changes_stack: Vec<Vec<(NonRootMPath, CreateChange)>> = changes_stack
            .into_iter()
            .zip(path_changes_stack.iter())
            .map(|(changes, path_changes)| {
                changes
                    .into_iter()
                    // Filter deletions that have a change at a path prefix. The
                    // deletion is implicit from the change. (2)
                    .filter(|(path, change)| {
                        change.change_type() != CreateChangeType::Deletion
                            || !is_prefix_changed(path, path_changes)
                    })
                    // Then convert the paths to MPaths. Do this before we start
                    // resolving any changes, so that we don't start storing data
                    // until we're happy that the changes are valid.
                    .map(|(path, change)| {
                        path.into_optional_non_root_path()
                            .ok_or_else(|| {
                                MononokeError::InvalidRequest(String::from(
                                    "Cannot create a file with an empty path",
                                ))
                            })
                            .map(move |mpath| (mpath, change))
                    })
                    .collect::<Result<_, _>>()
            })
            .collect::<Result<_, _>>()?;

        // Track the changes already made so far at each step in the stack.
        let stack_changes_stack = {
            let mut stack_changes_stack = vec![None];
            let mut stack_changes = PathTree::default();
            for (index, path_changes) in path_changes_stack.iter().enumerate() {
                if index < path_changes_stack.len() - 1 {
                    for (path, change) in path_changes.clone().into_iter() {
                        match change {
                            CreateChangeType::Change => {
                                stack_changes.insert_and_prune(path, change);
                            }
                            CreateChangeType::Deletion => {
                                stack_changes.insert(path, change);
                            }
                            CreateChangeType::None => {}
                        }
                    }
                    stack_changes_stack.push(Some(stack_changes.clone()))
                }
            }
            stack_changes_stack
        };

        // Check that changes are valid according to bonsai rules:
        // (1) deletions and copy-from info must reference a real path in a
        //     valid parent.
        // (2) deletions for paths where a prefix directory has been replaced
        //     by a file should be dropped, as the deletion is implicit from the
        //     file change for the prefix path.
        // (3) conversely, when a file has been replaced by a directory, there
        //     must be a delete for the file.

        // Check deleted files existed in a parent. (1)
        let verify_deleted_files_existed_fut = async {
            stream::iter(
                tracked_deletion_files_stack
                    .into_iter()
                    .zip(stack_changes_stack.iter())
                    .map(Ok),
            )
            .map_ok(async |(tracked_deletion_files, stack_changes)| {
                // This does NOT consider "missing" (untracked deletion) files as it is NOT
                // necessary for them to exist in a parent. If they don't exist on a parent,
                // this means the file was "hg added" and then manually deleted.
                verify_deleted_files_existed_in_a_parent(
                    &stack_parent_ctxs,
                    stack_changes.as_ref(),
                    tracked_deletion_files,
                    checks.deleted_files_existed_in_a_parent,
                )
                .timed()
                .await
                .log_future_stats(
                    self.ctx().scuba().clone(),
                    "Verify deleted files existed in a parent",
                    None,
                )
            })
            .boxed()
            .try_buffered(10)
            .try_collect::<Vec<_>>()
            .await
        };

        // Check changes that replace a file with a directory also delete
        // this replaced file. (3)
        let verify_prefix_files_deleted_fut = async {
            stream::iter(
                prefix_paths_stack
                    .into_iter()
                    .zip(path_changes_stack.iter())
                    .zip(stack_changes_stack.iter())
                    .map(Ok),
            )
            .try_for_each_concurrent(10, async |((prefix_paths, path_changes), stack_changes)| {
                verify_prefix_files_deleted(
                    &stack_parent_ctxs,
                    stack_changes.as_ref(),
                    prefix_paths,
                    path_changes,
                    checks.prefix_files_deleted,
                )
                .timed()
                .await
                .log_future_stats(
                    self.ctx().scuba().clone(),
                    "Verify prefix files in parents have been deleted",
                    None,
                )
            })
            .await
        };

        // Check for merge conflicts.  This only applies to the first commit
        // in a stack.
        let verify_no_merge_conflicts_fut = async {
            check_addless_union_conflicts(
                self.ctx(),
                match &bubble {
                    Some(bubble) => {
                        bubble.wrap_repo_blobstore(self.repo().repo_blobstore().clone())
                    }
                    None => self.repo().repo_blobstore().clone(),
                },
                &stack_parent_ctxs,
                path_changes_stack
                    .first()
                    .ok_or_else(|| anyhow!("Should be at least one commit"))?,
            )
            .timed()
            .await
            .log_future_stats(
                self.ctx().scuba().clone(),
                "Verify all merge conflicts are resolved",
                None,
            )
        };

        // Resolve the changes so that they are ready to be converted into
        // bonsai changes. This also checks (1) for copy-from info.
        let blobstore = match &bubble {
            Some(bubble) => bubble.wrap_repo_blobstore(self.repo().repo_blobstore().clone()),
            None => self.repo().repo_blobstore().clone(),
        };
        let resolve_file_changes_fut = async {
            stream::iter(
                changes_stack
                    .into_iter()
                    .zip(stack_changes_stack.iter())
                    .map(async |(changes, stack_changes)| {
                        stream::iter(changes.into_iter().map(async |(path, mut change)| {
                            change
                                .resolve(
                                    self.ctx(),
                                    *self.repo().filestore_config(),
                                    blobstore.clone(),
                                    stack_changes.as_ref(),
                                    &stack_parent_ctxs,
                                    checks.copy_from_path,
                                )
                                .await?;
                            Ok::<_, MononokeError>((path, change))
                        }))
                        .buffered(1000)
                        .try_collect::<SortedVectorMap<NonRootMPath, CreateChange>>()
                        .timed()
                        .await
                        .log_future_stats(
                            self.ctx().scuba().clone(),
                            "Convert create changeset parameters to bonsai changes",
                            None,
                        )
                    }),
            )
            .boxed()
            .buffered(10)
            .try_collect::<Vec<_>>()
            .await
        };

        let (deletions_to_remove_stack, (), (), mut file_changes_stack) = try_join!(
            verify_deleted_files_existed_fut,
            verify_prefix_files_deleted_fut,
            verify_no_merge_conflicts_fut,
            resolve_file_changes_fut,
        )?;

        file_changes_stack
            .iter_mut()
            .zip(deletions_to_remove_stack)
            .for_each(|(file_changes, deletions_to_remove)| {
                file_changes.retain(|path, _| !deletions_to_remove.contains(path.as_mpath()));
            });

        if checks.noop_file_changes != CreateChangesetCheckMode::Skip
            && justknobs::eval(
                "scm/mononoke:create_changeset_stack_noop_file_changes_check",
                None,
                Some(self.name()),
            )
        {
            // When to build another stack of PathTrees that contains the content_id for each file change
            let mut stack_changes_stack = vec![None];
            let mut stack_changes = PathTree::default();
            for (index, path_changes) in file_changes_stack.iter().enumerate() {
                if index < file_changes_stack.len() - 1 {
                    for (path, change) in path_changes.iter() {
                        match change.content_id()? {
                            Some(content_id) => {
                                stack_changes.insert_and_prune(
                                    path.clone().into(),
                                    StackContentChange::Change(content_id),
                                );
                            }
                            None => {
                                stack_changes
                                    .insert(path.clone().into(), StackContentChange::Deletion);
                            }
                        }
                    }
                    stack_changes_stack.push(Some(stack_changes.clone()))
                }
            }

            if checks.noop_file_changes == CreateChangesetCheckMode::Fix {
                file_changes_stack =
                    stream::iter(file_changes_stack.into_iter().zip(stack_changes_stack))
                        .map(Ok)
                        .map_ok(async |(file_changes, stack_changes)| {
                            Ok::<_, MononokeError>(
                                remove_noop_file_changes(
                                    &stack_parent_ctxs,
                                    stack_changes,
                                    file_changes,
                                )
                                .try_timed()
                                .await?
                                .log_future_stats(
                                    self.ctx().scuba().clone(),
                                    "Removing no no-op file changes",
                                    None,
                                ),
                            )
                        })
                        .try_buffered(10)
                        .try_collect::<Vec<_>>()
                        .await?;
            } else {
                stream::iter(file_changes_stack.iter().zip(stack_changes_stack))
                    .map(Ok)
                    .try_for_each_concurrent(10, async |(file_changes, stack_changes)| {
                        verify_no_noop_file_changes(&stack_parent_ctxs, stack_changes, file_changes)
                            .timed()
                            .await
                            .log_future_stats(
                                self.ctx().scuba().clone(),
                                "Verify no no-op file changes",
                                None,
                            )
                    })
                    .await?;
            }
        };

        // Resolve file types for changes with file_type: None
        // For each commit in the stack, we need to look at:
        // 1. Previous commits in the stack (for files created earlier in the stack)
        // 2. Parent contexts (for files from the parent commits)
        // This must be sequential per-commit because each commit's file types may depend on
        // previous commits in the stack. However, parent lookups within a commit are parallel.
        let mut stack_file_types: BTreeMap<NonRootMPath, FileType> = BTreeMap::new();
        for file_changes in &mut file_changes_stack {
            // Phase 1: Sync resolution from stack_file_types
            let mut needs_parent_lookup: Vec<(NonRootMPath, NonRootMPath)> = Vec::new();
            for (path, change) in file_changes.iter_mut() {
                if !change.resolve_file_type_from_stack(path, &stack_file_types) {
                    let lookup_path = change.copy_source_or_path(path);
                    needs_parent_lookup.push((path.clone(), lookup_path));
                }
            }

            // Phase 2: Batch parent lookups using shared helper
            let unique_lookup_paths = needs_parent_lookup
                .iter()
                .map(|(_, lookup_path)| lookup_path.clone());
            let parent_file_types =
                lookup_file_types_from_parents(unique_lookup_paths, &stack_parent_ctxs).await?;

            // Phase 3: Apply resolved types from parents, defaulting to Regular
            for (path, change) in file_changes.iter_mut() {
                if change.needs_file_type_resolution() {
                    let lookup_path = change.copy_source_or_path(path);
                    let ft = parent_file_types
                        .get(&lookup_path)
                        .copied()
                        .unwrap_or(FileType::Regular);
                    change.set_file_type(ft);
                }
            }

            // Update stack_file_types for next commit
            for (path, change) in file_changes.iter() {
                if let Some(ft) = change.file_type() {
                    stack_file_types.insert(path.clone(), ft);
                }
            }
        }

        let mut new_changesets = Vec::new();
        let mut new_changeset_ids = Vec::new();
        let mut parents = stack_parents;
        for (info, file_changes) in info_stack.into_iter().zip(file_changes_stack) {
            if checks.empty_changeset != CreateChangesetCheckMode::Skip
                && justknobs::eval(
                    "scm/mononoke:create_changeset_stack_empty_changeset_check",
                    None,
                    Some(self.name()),
                )
                && file_changes.is_empty()
                // exclude merge commits
                && parents.len() <= 1
            {
                return Err(MononokeError::InvalidRequest(String::from(
                    "Commits that don't modify or delete any files are not allowed",
                )));
            }

            let author_date = MononokeDateTime::new(info.author_date);
            let committer_date = info.committer_date.map(MononokeDateTime::new);
            let extra: BTreeMap<_, _> = info
                .extra
                .into_iter()
                .chain(bubble.map(|bubble| {
                    (
                        SNAPSHOT_BUBBLE_ID_EXTRA.to_string(),
                        bubble.bubble_id().to_string().into_bytes(),
                    )
                }))
                .collect();
            let hg_extra = SortedVectorMap::<_, _>::from(extra);
            let git_extra_headers = info.git_extra_headers.map(SortedVectorMap::from);
            let file_changes = file_changes
                .into_iter()
                .map(|(path, change)| Ok((path, change.into_file_change(&parents)?)))
                .collect::<Result<SortedVectorMap<NonRootMPath, FileChange>, MononokeError>>()?;

            // Create the new Bonsai Changeset. The `freeze` method validates
            // that the bonsai changeset is internally consistent.

            let new_changeset = BonsaiChangesetMut {
                parents,
                author: info.author,
                author_date,
                committer: info.committer,
                committer_date,
                message: info.message,
                hg_extra: hg_extra.clone(),
                git_extra_headers,
                git_tree_hash: None,
                file_changes,
                is_snapshot: bubble.is_some(),
                git_annotated_tag: None,
                subtree_changes: SortedVectorMap::new(),
            }
            .freeze()
            .map_err(|e| {
                MononokeError::InvalidRequest(format!(
                    "Changes create invalid bonsai changeset: {e}"
                ))
            })?;

            let new_changeset_id = new_changeset.get_changeset_id();
            parents = vec![new_changeset_id];
            new_changesets.push(new_changeset);
            new_changeset_ids.push((hg_extra, new_changeset_id));
        }

        if let Some(bubble) = &bubble {
            self.save_changesets(new_changesets, &bubble.repo_view(self.repo()), Some(bubble))
                .await?;
        } else {
            self.save_changesets(new_changesets, self.repo(), None)
                .await?;
        }

        Ok(new_changeset_ids
            .into_iter()
            .map(|(hg_extras, id)| CreatedChangeset {
                hg_extras,
                changeset_ctx: ChangesetContext::new(self.clone(), id),
            })
            .collect())
    }
}

/// Validate `.slacl` file changes for a stack of `CreateChange` maps,
/// delegating to `RestrictedPaths::validate_acl_file_changes_in_changesets`.
/// All policy and JustKnob gating live in the chokepoint method.
async fn validate_acl_file_changes<R: MononokeRepo>(
    repo_ctx: &RepoContext<R>,
    stack_parents: &[ChangesetId],
    changes_stack: &[BTreeMap<MPath, CreateChange>],
) -> Result<(), MononokeError> {
    let restricted_paths = repo_ctx.repo().restricted_paths_arc();
    let repo_name = repo_ctx.repo().repo_identity().name();
    let acl_file_name = restricted_paths.config().acl_file_name();

    // Build per-changeset list of touched ACL file paths, keeping only
    // `.slacl` basenames so ordinary paths are never cloned. Root-path
    // entries can't be `.slacl` (the root has no basename), so dropping
    // them via `try_from(...).ok()` is safe.
    let touched_per_changeset: Vec<Vec<NonRootMPath>> = changes_stack
        .iter()
        .map(|changes| {
            changes
                .keys()
                .filter(|path| {
                    path.basename()
                        .is_some_and(|name| name.as_ref() == acl_file_name.as_bytes())
                })
                .filter_map(|path| NonRootMPath::try_from(path.clone()).ok())
                .collect()
        })
        .collect();

    let touched_refs: Vec<&[NonRootMPath]> =
        touched_per_changeset.iter().map(Vec::as_slice).collect();

    restricted_paths
        .validate_acl_file_changes_in_changesets(
            repo_ctx.ctx(),
            repo_name,
            stack_parents,
            &touched_refs,
        )
        .await
        .map_err(MononokeError::from)
}
