/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::fmt;

use acl_regions::AclRegionsRef;
use anyhow::Error;
use anyhow::anyhow;
use async_trait::async_trait;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use bytes::Bytes;
use changeset_info::ChangesetInfo;
use cloned::cloned;
use commit_graph::CommitGraphArc;
use commit_graph::CommitGraphRef;
use context::CoreContext;
use deleted_manifest::DeletedManifestOps;
use deleted_manifest::RootDeletedManifestIdCommon;
use derivation_queue_thrift::DerivationPriority;
use filestore::FetchKey;
use futures::future::TryFutureExt;
use futures::stream;
use futures::stream::BoxStream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use futures_ext::stream::FbStreamExt;
use futures_lazy_shared::LazyShared;
use futures_watchdog::WatchdogExt;
use history_traversal::CsAndPath;
use history_traversal::FastlogError;
use history_traversal::FollowMutableRenames;
use history_traversal::HistoryAcrossDeletions;
use history_traversal::TraversalOrder;
use history_traversal::Visitor;
use history_traversal::list_file_history;
use manifest::Entry;
use manifest::ManifestOps;
use mononoke_types::ChangesetId;
use mononoke_types::ContentManifestId;
use mononoke_types::ContentMetadataV2;
/// Metadata about a file.
pub use mononoke_types::ContentMetadataV2 as FileMetadata;
use mononoke_types::FileChange;
use mononoke_types::FileType;
use mononoke_types::FileUnodeId;
use mononoke_types::ManifestUnodeId;
use mononoke_types::NonRootMPath;
use mononoke_types::blame_v2::BlameV2;
use mononoke_types::content_manifest::ContentManifestFile;
use mononoke_types::deleted_manifest_common::DeletedManifestCommon;
use mononoke_types::path::MPath;
use repo_blobstore::RepoBlobstoreArc;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataArc;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentityRef;
use repo_permission_checker::RepoPermissionCheckerRef;
use restricted_paths::RestrictedPathsArc;

use crate::MononokeRepo;
use crate::changeset::ChangesetContext;
use crate::errors::MononokeError;
use crate::file::FileContext;
use crate::repo::RepoContext;
use crate::restricted_paths::PathAccessInfo;
use crate::tree::TreeContext;

mod p4;

pub use p4::P4ChangesetRevisions;
pub use p4::P4LastChange;
pub use p4::P4PathRevision;
pub use p4::P4RevisionSearch;

pub struct HistoryEntry {
    pub name: String,
    pub changeset_id: ChangesetId,
}

#[derive(Default, Clone, Copy)]
pub struct ChangesetPathHistoryOptions {
    pub until_timestamp: Option<i64>,
    pub until_committer_timestamp: Option<i64>,
    pub descendants_of: Option<ChangesetId>,
    pub exclude_changeset_and_ancestors: Option<ChangesetId>,
    pub follow_history_across_deletions: bool,
    pub follow_mutable_file_history: bool,
}

pub enum PathEntry<R> {
    NotPresent,
    Tree(TreeContext<R>),
    File(FileContext<R>, FileType),
}

type UnodeResult = Result<Option<Entry<ManifestUnodeId, FileUnodeId>>, MononokeError>;
type ContentManifestResult =
    Result<Option<Entry<ContentManifestId, ContentManifestFile>>, MononokeError>;
type LinknodeResult = Result<Option<ChangesetId>, MononokeError>;

/// Context that makes it cheap to fetch content info about a path within a changeset.
///
/// A ChangesetPathContentContext may represent a file, a directory, a path where a
/// file or directory has been deleted, or a path where nothing ever existed.
#[derive(Clone)]
pub struct ChangesetPathContentContext<R> {
    changeset: ChangesetContext<R>,
    path: MPath,
    manifest_entry: LazyShared<ContentManifestResult>,
}

impl<R: RepoIdentityRef> fmt::Debug for ChangesetPathContentContext<R> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "ChangesetPathContentContext(repo={:?} id={:?} path={:?})",
            self.repo_ctx().name(),
            self.changeset().id(),
            self.path()
        )
    }
}

/// Context that makes it cheap to fetch history info about a path within a changeset.
pub struct ChangesetPathHistoryContext<R> {
    changeset: ChangesetContext<R>,
    path: MPath,
    unode_id: LazyShared<UnodeResult>,
    linknode: LazyShared<LinknodeResult>,
}

impl<R: RepoIdentityRef> fmt::Debug for ChangesetPathHistoryContext<R> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "ChangesetPathHistoryContext(repo={:?} id={:?} path={:?})",
            self.repo_ctx().name(),
            self.changeset().id(),
            self.path()
        )
    }
}

/// Context to check if a file or a directory exists in a changeset
pub struct ChangesetPathContext<R> {
    changeset: ChangesetContext<R>,
    path: MPath,
    entry_kind: LazyShared<Result<Option<Entry<(), ()>>, MononokeError>>,
}

impl<R: RepoIdentityRef> fmt::Debug for ChangesetPathContext<R> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "ChangesetPathContext(repo={:?} id={:?} path={:?})",
            self.repo_ctx().name(),
            self.changeset().id(),
            self.path()
        )
    }
}

impl<R> ChangesetPathContentContext<R> {
    /// The `RepoContext` for this query.
    pub fn repo_ctx(&self) -> &RepoContext<R> {
        self.changeset.repo_ctx()
    }

    /// The `ChangesetContext` for this query.
    pub fn changeset(&self) -> &ChangesetContext<R> {
        &self.changeset
    }

    /// The path for this query.
    pub fn path(&self) -> &MPath {
        &self.path
    }
}

impl<R> ChangesetPathContentContext<R>
where
    R: RepoPermissionCheckerRef
        + AclRegionsRef
        + RepoIdentityRef
        + RestrictedPathsArc
        + RepoBlobstoreArc
        + RepoBlobstoreRef
        + RepoDerivedDataArc
        + RepoDerivedDataRef
        + Clone
        + Send
        + Sync
        + 'static,
{
    pub(crate) async fn new(
        changeset: ChangesetContext<R>,
        path: impl Into<MPath>,
    ) -> Result<Self, MononokeError> {
        let path = path.into();
        let id_type = changeset
            .repo_ctx()
            .authorization_context()
            .require_path_read_with_result(
                changeset.ctx(),
                changeset.repo_ctx().repo(),
                changeset.id(),
                &path,
            )
            .await?;
        if let Some(id_type) = id_type {
            changeset
                .repo_ctx()
                .record_path_acl_deciding_identity_type(id_type);
        }

        restricted_paths::spawn_enforce_restricted_path_access(
            changeset.ctx(),
            changeset.repo_ctx().repo().restricted_paths_arc().clone(),
            &path,
            "changeset_path_content_context_new",
            Some(changeset.id()),
        )
        .await?;

        Ok(Self {
            changeset,
            path,
            manifest_entry: LazyShared::new_empty(),
        })
    }

    pub(crate) async fn new_with_manifest_entry(
        changeset: ChangesetContext<R>,
        path: impl Into<MPath>,
        manifest_entry: Entry<ContentManifestId, ContentManifestFile>,
    ) -> Result<Self, MononokeError> {
        let path = path.into();
        let id_type = changeset
            .repo_ctx()
            .authorization_context()
            .require_path_read_with_result(
                changeset.ctx(),
                changeset.repo_ctx().repo(),
                changeset.id(),
                &path,
            )
            .await?;
        if let Some(id_type) = id_type {
            changeset
                .repo_ctx()
                .record_path_acl_deciding_identity_type(id_type);
        }

        restricted_paths::spawn_enforce_restricted_path_access(
            changeset.ctx(),
            changeset.repo_ctx().repo().restricted_paths_arc().clone(),
            &path,
            "changeset_path_context_fsnode_new",
            Some(changeset.id()),
        )
        .await?;

        Ok(Self {
            changeset,
            path,
            manifest_entry: LazyShared::new_ready(Ok(Some(manifest_entry))),
        })
    }

    async fn manifest_entry(
        &self,
    ) -> Result<Option<Entry<ContentManifestId, ContentManifestFile>>, MononokeError> {
        self.manifest_entry
            .get_or_init(|| {
                cloned!(self.changeset, self.path);
                async move {
                    let ctx = changeset.ctx().clone();
                    let blobstore = changeset.repo_ctx().repo().repo_blobstore().clone();
                    let root_id = changeset.root_content_manifest_id().await?;

                    if let Some(mpath) = path.into_optional_non_root_path() {
                        root_id
                            .find_entry(ctx, blobstore, MPath::from(mpath))
                            .await
                            .map_err(MononokeError::from)
                    } else {
                        Ok(Some(Entry::Tree(root_id)))
                    }
                }
            })
            .await
    }

    /// Returns `true` if the path exists (as a file or directory) in this commit.
    pub async fn exists(&self) -> Result<bool, MononokeError> {
        Ok(self.manifest_entry().await?.is_some())
    }

    pub async fn is_file(&self) -> Result<bool, MononokeError> {
        let is_file = match self.manifest_entry().await? {
            Some(Entry::Leaf(_)) => true,
            _ => false,
        };
        Ok(is_file)
    }

    pub async fn is_tree(&self) -> Result<bool, MononokeError> {
        let is_tree = match self.manifest_entry().await? {
            Some(Entry::Tree(_)) => true,
            _ => false,
        };
        Ok(is_tree)
    }

    pub async fn file_type(&self) -> Result<Option<FileType>, MononokeError> {
        let file_type = match self.manifest_entry().await? {
            Some(Entry::Leaf(file)) => Some(file.file_type),
            _ => None,
        };
        Ok(file_type)
    }

    /// Returns a `TreeContext` for the tree at this path.  Returns `None` if the path
    /// is not a directory in this commit.
    pub async fn tree(&self) -> Result<Option<TreeContext<R>>, MononokeError> {
        let tree = match self.manifest_entry().await? {
            Some(Entry::Tree(manifest_id)) => Some(TreeContext::new_authorized(
                self.repo_ctx().clone(),
                manifest_id,
            )),
            _ => None,
        };
        Ok(tree)
    }

    /// Returns a `FileContext` for the file at this path.  Returns `None` if the path
    /// is not a file in this commit.
    pub async fn file(&self) -> Result<Option<FileContext<R>>, MononokeError> {
        let file = match self.manifest_entry().await? {
            Some(Entry::Leaf(file)) => Some(FileContext::new_authorized(
                self.repo_ctx().clone(),
                FetchKey::Canonical(file.content_id),
            )),
            _ => None,
        };
        Ok(file)
    }

    pub async fn file_content(&self) -> Result<Option<Bytes>, MononokeError> {
        Ok(match self.file().await? {
            Some(context) => Some(context.content_concat().await?),
            None => None,
        })
    }

    pub async fn file_metadata(&self) -> Result<Option<ContentMetadataV2>, MononokeError> {
        let metadata = match self.file().await? {
            Some(context) => Some(context.metadata().await?),
            None => None,
        };
        Ok(metadata)
    }

    pub async fn file_change(&self) -> Result<Option<FileChange>, MononokeError> {
        let non_root_mpath = NonRootMPath::try_from(self.path.clone())?;
        let file_changes = self.changeset.file_changes().await?;
        let file_change = file_changes.get(&non_root_mpath);
        match file_change {
            Some(file_change) => Ok(Some(file_change.clone())),
            None => Ok(None),
        }
    }

    /// Returns a `TreeContext` or `FileContext` and `FileType` for the tree
    /// or file at this path. Returns `NotPresent` if the path is not a file
    /// or directory in this commit.
    pub async fn entry(&self) -> Result<PathEntry<R>, MononokeError> {
        let entry = match self.manifest_entry().await? {
            Some(Entry::Tree(manifest_id)) => PathEntry::Tree(TreeContext::new_authorized(
                self.repo_ctx().clone(),
                manifest_id,
            )),
            Some(Entry::Leaf(file)) => PathEntry::File(
                FileContext::new_authorized(
                    self.repo_ctx().clone(),
                    FetchKey::Canonical(file.content_id),
                ),
                file.file_type,
            ),
            _ => PathEntry::NotPresent,
        };
        Ok(entry)
    }
}

impl<R> ChangesetPathHistoryContext<R> {
    /// The `RepoContext` for this query.
    pub fn repo_ctx(&self) -> &RepoContext<R> {
        self.changeset.repo_ctx()
    }

    /// The `ChangesetContext` for this query.
    pub fn changeset(&self) -> &ChangesetContext<R> {
        &self.changeset
    }

    /// The path for this query.
    pub fn path(&self) -> &MPath {
        &self.path
    }
}

impl<R: MononokeRepo> ChangesetPathHistoryContext<R> {
    pub(crate) async fn new(
        changeset: ChangesetContext<R>,
        path: impl Into<MPath>,
    ) -> Result<Self, MononokeError> {
        let path = path.into();
        let id_type = changeset
            .repo_ctx()
            .authorization_context()
            .require_path_read_with_result(
                changeset.ctx(),
                changeset.repo_ctx().repo(),
                changeset.id(),
                &path,
            )
            .await?;
        if let Some(id_type) = id_type {
            changeset
                .repo_ctx()
                .record_path_acl_deciding_identity_type(id_type);
        }

        restricted_paths::spawn_enforce_restricted_path_access(
            changeset.ctx(),
            changeset.repo_ctx().repo().restricted_paths_arc().clone(),
            &path,
            "changeset_path_history_context_new",
            Some(changeset.id()),
        )
        .await?;

        Ok(Self {
            changeset,
            path,
            unode_id: LazyShared::new_empty(),
            linknode: LazyShared::new_empty(),
        })
    }

    pub(crate) async fn new_with_unode_entry(
        changeset: ChangesetContext<R>,
        path: impl Into<MPath>,
        unode_entry: Entry<ManifestUnodeId, FileUnodeId>,
    ) -> Result<Self, MononokeError> {
        let path = path.into();
        let id_type = changeset
            .repo_ctx()
            .authorization_context()
            .require_path_read_with_result(
                changeset.ctx(),
                changeset.repo_ctx().repo(),
                changeset.id(),
                &path,
            )
            .await?;
        if let Some(id_type) = id_type {
            changeset
                .repo_ctx()
                .record_path_acl_deciding_identity_type(id_type);
        }

        restricted_paths::spawn_enforce_restricted_path_access(
            changeset.ctx(),
            changeset.repo_ctx().repo().restricted_paths_arc().clone(),
            &path,
            "changeset_path_history_context_new",
            Some(changeset.id()),
        )
        .await?;

        Ok(Self {
            changeset,
            path,
            unode_id: LazyShared::new_ready(Ok(Some(unode_entry))),
            linknode: LazyShared::new_empty(),
        })
    }

    pub(crate) async fn new_with_deleted_manifest<Manifest: DeletedManifestCommon>(
        changeset: ChangesetContext<R>,
        path: MPath,
        deleted_manifest_id: Manifest::Id,
    ) -> Result<Self, MononokeError> {
        let id_type = changeset
            .repo_ctx()
            .authorization_context()
            .require_path_read_with_result(
                changeset.ctx(),
                changeset.repo_ctx().repo(),
                changeset.id(),
                &path,
            )
            .await?;
        if let Some(id_type) = id_type {
            changeset
                .repo_ctx()
                .record_path_acl_deciding_identity_type(id_type);
        }
        let ctx = changeset.ctx().clone();
        let blobstore = changeset.repo_ctx().repo().repo_blobstore().clone();

        restricted_paths::spawn_enforce_restricted_path_access(
            changeset.ctx(),
            changeset.repo_ctx().repo().restricted_paths_arc().clone(),
            &path,
            "changeset_path_history_context_new",
            Some(changeset.id()),
        )
        .await?;

        Ok(Self {
            changeset,
            path,
            unode_id: LazyShared::new_empty(),
            linknode: LazyShared::new_future(async move {
                let deleted_manifest = deleted_manifest_id.load(&ctx, &blobstore).await?;
                Ok(deleted_manifest.linknode().cloned())
            }),
        })
    }

    // pub(crate) for testing
    pub(crate) async fn unode_id(
        &self,
    ) -> Result<Option<Entry<ManifestUnodeId, FileUnodeId>>, MononokeError> {
        self.unode_id
            .get_or_init(|| {
                cloned!(self.changeset, self.path);
                async move {
                    let ctx = changeset.ctx().clone();
                    let blobstore = changeset.repo_ctx().repo().repo_blobstore().clone();
                    let root_unode_manifest_id = changeset.root_unode_manifest_id().await?;
                    if let Some(mpath) = path.into_optional_non_root_path() {
                        root_unode_manifest_id
                            .manifest_unode_id()
                            .find_entry(ctx, blobstore, MPath::from(mpath))
                            .await
                            .map_err(MononokeError::from)
                    } else {
                        Ok(Some(Entry::Tree(
                            root_unode_manifest_id.manifest_unode_id().clone(),
                        )))
                    }
                }
            })
            .await
    }

    async fn linknode_from_id(
        ctx: &CoreContext,
        blobstore: &impl KeyedBlobstore,
        root: impl RootDeletedManifestIdCommon + 'static,
        path: MPath,
    ) -> Result<Option<ChangesetId>, MononokeError> {
        let maybe_id = if let Some(mpath) = path.into_optional_non_root_path() {
            root.find_entry(ctx, blobstore, MPath::from(mpath))
                .await
                .map_err(MononokeError::from)?
        } else {
            Some(root.id().clone())
        };
        if let Some(id) = maybe_id {
            let deleted_manifest = id.load(ctx, blobstore).await?;
            Ok(deleted_manifest.linknode().cloned())
        } else {
            Ok(None)
        }
    }

    async fn linknode(&self) -> Result<Option<ChangesetId>, MononokeError> {
        self.linknode
            .get_or_init(|| {
                cloned!(self.changeset, self.path);
                async move {
                    let ctx = changeset.ctx();
                    let blobstore = changeset.repo_ctx().repo().repo_blobstore();
                    Self::linknode_from_id(
                        ctx,
                        blobstore,
                        changeset.root_deleted_manifest_v2_id().await?,
                        path,
                    )
                    .await
                }
            })
            .await
    }

    /// Returns the last commit that modified this path.  If there is nothing
    /// at this path, returns `None`.
    pub async fn last_modified(&self) -> Result<Option<ChangesetContext<R>>, MononokeError> {
        match self.unode_id().await? {
            Some(Entry::Tree(manifest_unode_id)) => {
                let ctx = self.changeset.ctx();
                let repo = self.changeset.repo_ctx().repo();
                let manifest_unode = manifest_unode_id.load(ctx, repo.repo_blobstore()).await?;
                let cs_id = manifest_unode.linknode().clone();
                Ok(Some(ChangesetContext::new(self.repo_ctx().clone(), cs_id)))
            }
            Some(Entry::Leaf(file_unode_id)) => {
                let ctx = self.changeset.ctx();
                let repo = self.changeset.repo_ctx().repo();
                let file_unode = file_unode_id.load(ctx, repo.repo_blobstore()).await?;
                let cs_id = file_unode.linknode().clone();
                Ok(Some(ChangesetContext::new(self.repo_ctx().clone(), cs_id)))
            }
            None => Ok(None),
        }
    }

    /// Whether a file is at this path.
    pub async fn is_file(&self) -> Result<bool, MononokeError> {
        Ok(matches!(self.unode_id().await?, Some(Entry::Leaf(_))))
    }

    /// Returns the first (oldest) changeset that introduced this path.
    ///
    /// We don't need to walk every changeset - we can just walk along the
    /// fastlog batches to find the first changeset.  By default this finds the
    /// oldest changeset of the path's current contiguous history; with
    /// `follow_history_across_deletions` it crosses deletion boundaries to find
    /// the very first time the path ever existed (rather than the changeset that
    /// most recently re-added it).
    pub async fn first_modified(
        &self,
        follow_history_across_deletions: bool,
    ) -> Result<Option<ChangesetContext<R>>, MononokeError> {
        if self.unode_id().await?.is_none() {
            return Err(MononokeError::InvalidRequest(format!(
                "path '{}' does not exist in changeset {}",
                self.path,
                self.changeset.id(),
            )));
        }

        let history_across_deletions = if follow_history_across_deletions {
            history_traversal::HistoryAcrossDeletions::Track
        } else {
            history_traversal::HistoryAcrossDeletions::DontTrack
        };
        let first = history_traversal::fetch_path_first_changeset(
            self.changeset.ctx(),
            self.repo_ctx().repo(),
            self.changeset.id(),
            self.path.clone(),
            history_across_deletions,
        )
        .await
        .map_err(|error| MononokeError::from(Error::from(error)))?;
        Ok(first.map(|cs_id| ChangesetContext::new(self.repo_ctx().clone(), cs_id)))
    }

    /// Returns the last commit that deleted this path.  If something exists
    /// at this path, or nothing ever existed at this path, returns `None`.
    pub async fn last_deleted(&self) -> Result<Option<ChangesetContext<R>>, MononokeError> {
        Ok(self
            .linknode()
            .await?
            .map(|cs_id| ChangesetContext::new(self.repo_ctx().clone(), cs_id)))
    }

    /// Blame metadata for this path.
    pub async fn blame(&self, follow_mutable_file_history: bool) -> Result<BlameV2, MononokeError> {
        let ctx = self.changeset.ctx();
        let repo = self.changeset.repo_ctx().repo();
        let csid = self.changeset.id();
        let blame =
            history_traversal::blame(ctx, repo, csid, &self.path, follow_mutable_file_history)
                .await?;
        Ok(blame)
    }

    /// Blame metadata for this path, and the content that was blamed.
    pub async fn blame_with_content(
        &self,
        follow_mutable_file_history: bool,
    ) -> Result<(BlameV2, Bytes), MononokeError> {
        let ctx = self.changeset.ctx();
        let repo = self.changeset.repo_ctx().repo();
        let csid = self.changeset.id();
        Ok(history_traversal::blame_with_content(
            ctx,
            repo,
            csid,
            &self.path,
            follow_mutable_file_history,
        )
        .await?)
    }

    /// Returns a list of `ChangesetContext` for the file at this path that represents
    /// a history of the path.
    pub async fn history(
        &self,
        opts: ChangesetPathHistoryOptions,
    ) -> Result<BoxStream<'_, Result<ChangesetContext<R>, MononokeError>>, MononokeError> {
        let repo = self.repo_ctx().repo().clone();

        if let Some(descendants_of) = opts.descendants_of {
            if !repo
                .commit_graph()
                .is_ancestor(
                    self.changeset().ctx(),
                    descendants_of,
                    self.changeset().id(),
                )
                .watched()
                .await?
            {
                return Ok(stream::empty().boxed());
            }
        }

        struct FilterVisitor {
            cs_info_enabled: bool,
            until_timestamp: Option<i64>,
            until_committer_timestamp: Option<i64>,
            descendants_of: Option<ChangesetId>,
            exclude_changeset_and_ancestors: Option<ChangesetId>,
            cache: HashMap<(Option<CsAndPath>, Vec<CsAndPath>), Vec<CsAndPath>>,
        }
        impl FilterVisitor {
            async fn _visit(
                &self,
                ctx: &CoreContext,
                repo: &impl history_traversal::Repo,
                _descendant_cs_id: Option<CsAndPath>,
                mut cs_ids: Vec<CsAndPath>,
            ) -> Result<Vec<CsAndPath>, Error> {
                let cs_info_enabled = self.cs_info_enabled;
                if self.until_timestamp.is_some() || self.until_committer_timestamp.is_some() {
                    let until_timestamp = self.until_timestamp;
                    let until_committer_timestamp = self.until_committer_timestamp;
                    cs_ids = stream::iter(cs_ids)
                        .map(|(cs_id, path)| async move {
                            let info = if cs_info_enabled {
                                repo.repo_derived_data()
                                    .derive::<ChangesetInfo>(ctx, cs_id, DerivationPriority::LOW)
                                    .watched()
                                    .await
                            } else {
                                let bonsai =
                                    cs_id.load(ctx, repo.repo_blobstore()).watched().await?;
                                Ok(ChangesetInfo::new(cs_id, bonsai))
                            }?;

                            if let Some(until_ts) = until_timestamp {
                                let timestamp = info.author_date().as_chrono().timestamp();
                                if timestamp < until_ts {
                                    return anyhow::Ok(None);
                                }
                            }
                            if let Some(until_committer_ts) = until_committer_timestamp {
                                // Get committer_date if available, otherwise fall back to author_date
                                let timestamp = match info.committer_date() {
                                    Some(committer_date) => committer_date.as_chrono().timestamp(),
                                    None => info.author_date().as_chrono().timestamp(),
                                };
                                if timestamp < until_committer_ts {
                                    return anyhow::Ok(None);
                                }
                            }
                            anyhow::Ok(Some((cs_id, path)))
                        })
                        .buffer_unordered(100)
                        .try_collect::<Vec<_>>()
                        .watched()
                        .await?
                        .into_iter()
                        .filter_map(std::convert::identity)
                        .collect();
                }

                if let Some(descendants_of) = self.descendants_of {
                    cs_ids = stream::iter(cs_ids)
                        .map(|(cs_id, path)| async move {
                            if repo
                                .commit_graph()
                                .is_ancestor(ctx, descendants_of, cs_id)
                                .watched()
                                .await?
                            {
                                anyhow::Ok(Some((cs_id, path)))
                            } else {
                                anyhow::Ok(None)
                            }
                        })
                        .buffer_unordered(100)
                        .try_collect::<Vec<_>>()
                        .watched()
                        .await?
                        .into_iter()
                        .filter_map(std::convert::identity)
                        .collect();
                }

                if let Some(exclude_changeset_and_ancestors) = self.exclude_changeset_and_ancestors
                {
                    cs_ids = stream::iter(cs_ids)
                        .map(|(cs_id, path)| async move {
                            if repo
                                .commit_graph()
                                .is_ancestor(ctx, cs_id, exclude_changeset_and_ancestors)
                                .watched()
                                .await?
                            {
                                Ok::<_, MononokeError>(None)
                            } else {
                                Ok::<_, MononokeError>(Some((cs_id, path)))
                            }
                        })
                        .buffer_unordered(100)
                        .try_collect::<Vec<_>>()
                        .watched()
                        .await?
                        .into_iter()
                        .filter_map(std::convert::identity)
                        .collect();
                }
                Ok(cs_ids)
            }
        }
        #[async_trait]
        impl Visitor for FilterVisitor {
            async fn visit(
                &mut self,
                ctx: &CoreContext,
                repo: &impl history_traversal::Repo,
                descendant_cs_id: Option<CsAndPath>,
                cs_ids: Vec<CsAndPath>,
            ) -> Result<Vec<CsAndPath>, Error> {
                if let Some(res) = self
                    .cache
                    .remove(&(descendant_cs_id.clone(), cs_ids.clone()))
                {
                    Ok(res)
                } else {
                    Ok(self
                        ._visit(ctx, repo, descendant_cs_id, cs_ids)
                        .watched()
                        .await?)
                }
            }

            async fn preprocess(
                &mut self,
                ctx: &CoreContext,
                repo: &impl history_traversal::Repo,
                descendant_id_cs_ids: Vec<(Option<CsAndPath>, Vec<CsAndPath>)>,
            ) -> Result<(), Error> {
                let items = stream::iter(descendant_id_cs_ids)
                    .map(|(descendant_cs_id, cs_ids)| {
                        self._visit(ctx, repo, descendant_cs_id.clone(), cs_ids.clone())
                            .map_ok(move |res| ((descendant_cs_id, cs_ids), res))
                    })
                    .buffered(10)
                    .yield_periodically()
                    .try_collect::<Vec<_>>()
                    .await?;
                for (k, v) in items {
                    self.cache.insert(k, v);
                }
                Ok(())
            }
        }
        let cs_info_enabled = self.repo_ctx().derive_changeset_info_enabled();

        let history_across_deletions = if opts.follow_history_across_deletions {
            HistoryAcrossDeletions::Track
        } else {
            HistoryAcrossDeletions::DontTrack
        };

        let history = list_file_history(
            self.changeset.ctx(),
            self.repo_ctx().repo(),
            self.path.clone(),
            self.changeset.id(),
            FilterVisitor {
                cs_info_enabled,
                until_timestamp: opts.until_timestamp,
                until_committer_timestamp: opts.until_committer_timestamp,
                descendants_of: opts.descendants_of,
                exclude_changeset_and_ancestors: opts.exclude_changeset_and_ancestors,
                cache: HashMap::new(),
            },
            history_across_deletions,
            if opts.follow_mutable_file_history {
                FollowMutableRenames::Yes
            } else {
                FollowMutableRenames::No
            },
            self.repo_ctx().mutable_renames().clone(),
            TraversalOrder::new_gen_num_order(
                self.changeset.ctx().clone(),
                repo.commit_graph_arc(),
            ),
        )
        .watched()
        .await
        .map_err(|error| match error {
            FastlogError::InternalError(e) => MononokeError::from(anyhow!(e)),
            FastlogError::DerivationError(e) => MononokeError::from(e),
            FastlogError::LoadableError(e) => MononokeError::from(e),
            FastlogError::Error(e) => MononokeError::from(e),
        })?;

        Ok(history
            .map_err(MononokeError::from)
            .map_ok(move |changeset_id| {
                ChangesetContext::new(self.repo_ctx().clone(), changeset_id)
            })
            .boxed())
    }
}

impl<R> ChangesetPathContext<R> {
    /// The `RepoContext` for this query.
    pub fn repo_ctx(&self) -> &RepoContext<R> {
        self.changeset.repo_ctx()
    }

    /// The `ChangesetContext` for this query.
    pub fn changeset(&self) -> &ChangesetContext<R> {
        &self.changeset
    }

    /// The path for this query.
    pub fn path(&self) -> &MPath {
        &self.path
    }
}

impl<R: MononokeRepo> ChangesetPathContext<R> {
    pub(crate) async fn new(
        changeset: ChangesetContext<R>,
        path: impl Into<MPath>,
    ) -> Result<Self, MononokeError> {
        let path = path.into();
        let id_type = changeset
            .repo_ctx()
            .authorization_context()
            .require_path_read_with_result(
                changeset.ctx(),
                changeset.repo_ctx().repo(),
                changeset.id(),
                &path,
            )
            .await?;
        if let Some(id_type) = id_type {
            changeset
                .repo_ctx()
                .record_path_acl_deciding_identity_type(id_type);
        }

        restricted_paths::spawn_enforce_restricted_path_access(
            changeset.ctx(),
            changeset.repo_ctx().repo().restricted_paths_arc().clone(),
            &path,
            "changeset_path_context_new",
            Some(changeset.id()),
        )
        .await?;

        Ok(Self {
            changeset,
            path,
            entry_kind: LazyShared::new_empty(),
        })
    }

    pub(crate) async fn new_with_entry(
        changeset: ChangesetContext<R>,
        path: impl Into<MPath>,
        entry: Entry<(), ()>,
    ) -> Result<Self, MononokeError> {
        let path = path.into();
        let id_type = changeset
            .repo_ctx()
            .authorization_context()
            .require_path_read_with_result(
                changeset.ctx(),
                changeset.repo_ctx().repo(),
                changeset.id(),
                &path,
            )
            .await?;
        if let Some(id_type) = id_type {
            changeset
                .repo_ctx()
                .record_path_acl_deciding_identity_type(id_type);
        }
        Ok(Self {
            changeset,
            path,
            entry_kind: LazyShared::new_ready(Ok(Some(entry))),
        })
    }

    async fn entry_kind(&self) -> Result<Option<Entry<(), ()>>, MononokeError> {
        self.entry_kind
            .get_or_init(|| {
                cloned!(self.changeset, self.path);
                async move {
                    let ctx = changeset.ctx().clone();
                    let blobstore = changeset.repo_ctx().repo().repo_blobstore().clone();
                    let repo_name = changeset.repo_ctx().name().to_string();

                    if let Some(mpath) = path.into_optional_non_root_path() {
                        if justknobs::eval(
                            "scm/mononoke:changeset_path_context_use_skeleton_manifest_v2",
                            None,
                            Some(&repo_name),
                        ) {
                            let root_skeleton_manifest_v2_id =
                                changeset.root_skeleton_manifest_v2_id().await?;
                            root_skeleton_manifest_v2_id
                                .into_inner_id()
                                .load(&ctx, &blobstore)
                                .await?
                                .find_entry(ctx, blobstore, MPath::from(mpath))
                                .await
                                .map(|maybe_entry| maybe_entry.map(|entry| entry.map_tree(|_| ())))
                                .map_err(MononokeError::from)
                        } else {
                            let root_skeleton_manifest_id =
                                changeset.root_skeleton_manifest_id().await?;
                            root_skeleton_manifest_id
                                .skeleton_manifest_id()
                                .find_entry(ctx, blobstore, MPath::from(mpath))
                                .await
                                .map(|maybe_entry| maybe_entry.map(|entry| entry.map_tree(|_| ())))
                                .map_err(MononokeError::from)
                        }
                    } else {
                        Ok(Some(Entry::Tree(())))
                    }
                }
            })
            .await
    }

    /// Returns `true` if the path exists (as a file or directory) in this commit.
    pub async fn exists(&self) -> Result<bool, MononokeError> {
        // The path exists if there is any kind of skeleton manifest entry.
        Ok(self.entry_kind().await?.is_some())
    }

    pub async fn is_file(&self) -> Result<bool, MononokeError> {
        let is_file = match self.entry_kind().await? {
            Some(Entry::Leaf(_)) => true,
            _ => false,
        };
        Ok(is_file)
    }

    pub async fn is_tree(&self) -> Result<bool, MononokeError> {
        let is_tree = match self.entry_kind().await? {
            Some(Entry::Tree(_)) => true,
            _ => false,
        };
        Ok(is_tree)
    }
}

/// Context for querying restriction metadata about a path in a changeset.
///
/// Unlike `ChangesetPathContext`, `ChangesetPathContentContext`, and
/// `ChangesetPathHistoryContext`, this type does NOT enforce access checks
/// in its constructor. This is intentional: restriction queries are
/// meta-queries about access policy, and callers need to examine paths
/// they may not have read access to (e.g. to determine which paths are
/// restricted and whether the user should request access).
///
/// This type never returns file content, directory listings, or history —
/// only restriction metadata (ACLs, access checks).
pub struct ChangesetPathRestrictionContext<R> {
    changeset: ChangesetContext<R>,
    path: MPath,
}

impl<R: MononokeRepo> ChangesetPathRestrictionContext<R> {
    // Enforces repo read access, but no Path ACLs access checks, since this will
    // be used to query restriction metadata.
    pub(crate) async fn new(
        changeset: ChangesetContext<R>,
        path: MPath,
    ) -> Result<Self, MononokeError> {
        let id_type = changeset
            .repo_ctx()
            .authorization_context()
            .require_path_read_with_result(
                changeset.ctx(),
                changeset.repo_ctx().repo(),
                changeset.id(),
                &path,
            )
            .await?;
        if let Some(id_type) = id_type {
            changeset
                .repo_ctx()
                .record_path_acl_deciding_identity_type(id_type);
        }

        Ok(Self { changeset, path })
    }

    pub fn changeset(&self) -> &ChangesetContext<R> {
        &self.changeset
    }

    pub fn path(&self) -> &MPath {
        &self.path
    }

    /// Check if this path falls under any restricted paths and return restriction info
    /// for all matching roots.
    ///
    /// Returns an empty Vec if the path is not restricted.
    /// When a path is under multiple nested roots (e.g. `foo/` and `foo/bar/`),
    /// returns info for each matching root.
    ///
    /// When `check_permissions` is true, the `has_access` field in each
    /// `PathAccessInfo` will be populated with the result of an ACL check.
    /// When false, `has_access` will be `None`.
    pub async fn restriction_info(
        &self,
        check_permissions: bool,
    ) -> Result<Vec<PathAccessInfo>, MononokeError> {
        let path = match NonRootMPath::try_from(self.path().clone()) {
            Ok(p) => p,
            // Root path cannot be restricted
            Err(_) => return Ok(vec![]),
        };

        let restricted_paths = self.changeset().repo_ctx().repo().restricted_paths_arc();
        let cs_id = self.changeset().id();

        if check_permissions {
            let restriction_checks = restricted_paths
                .get_path_restriction_check(
                    self.changeset().ctx(),
                    Some(cs_id),
                    std::slice::from_ref(&path),
                )
                .await?;
            return Ok(restriction_checks
                .into_iter()
                .map(|restriction_check| PathAccessInfo {
                    has_access: Some(restriction_check.has_authorization()),
                    restriction: restriction_check.into_restriction_info(),
                })
                .collect());
        }

        let restriction_infos = restricted_paths
            .get_path_restriction_info(
                self.changeset().ctx(),
                Some(cs_id),
                std::slice::from_ref(&path),
            )
            .await?;

        Ok(restriction_infos
            .into_iter()
            .map(|restriction| PathAccessInfo {
                restriction,
                has_access: None,
            })
            .collect())
    }

    /// Find all restricted paths that are descendants of this path.
    ///
    /// Returns restriction info for each restriction root under this path.
    /// Since the number of roots can grow, this method will not perform
    /// access checks unless `check_permissions` is true.
    pub async fn find_restricted_descendants(
        &self,
        check_permissions: bool,
    ) -> Result<Vec<PathAccessInfo>, MononokeError> {
        self.changeset()
            .find_restricted_descendants(vec![self.path().clone()], check_permissions)
            .await
    }
}
