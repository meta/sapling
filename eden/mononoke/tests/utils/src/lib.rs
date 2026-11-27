/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![feature(trait_alias)]

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::str::FromStr;

use anyhow::Error;
use anyhow::format_err;
use bonsai_git_mapping::BonsaiGitMapping;
use bonsai_hg_mapping::BonsaiHgMapping;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bonsai_tag_mapping::BonsaiTagMapping;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkUpdateReason;
use bookmarks::Bookmarks;
use bookmarks::BookmarksRef;
use bytes::Bytes;
use bytes::BytesMut;
use changesets_creation::save_changesets;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphRef;
use commit_graph::CommitGraphWriter;
use commit_graph::CommitGraphWriterRef;
use content_manifest_derivation::RootContentManifestId;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use filestore::FetchKey;
use filestore::FilestoreConfig;
use filestore::FilestoreConfigRef;
use filestore::StoreRequest;
use futures::future;
use futures::stream;
use futures::stream::TryStreamExt;
use manifest::ManifestOps;
use maplit::btreemap;
use mercurial_types::HgChangesetId;
use metaconfig_types::RepoConfig;
use mononoke_types::BonsaiChangesetMut;
use mononoke_types::ChangesetId;
use mononoke_types::DateTime;
use mononoke_types::FileChange;
use mononoke_types::FileType;
use mononoke_types::GitLfs;
use mononoke_types::NonRootMPath;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreArc;
use repo_derived_data::RepoDerivedData;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentity;
use repo_identity::RepoIdentityRef;

pub mod audit;
pub mod drawdag;
pub mod hg_manifest_reads;
pub mod random;

pub trait Repo = BonsaiHgMappingRef
    + BookmarksRef
    + CommitGraphRef
    + CommitGraphWriterRef
    + FilestoreConfigRef
    + RepoBlobstoreArc
    + RepoDerivedDataRef
    + RepoIdentityRef
    + Send
    + Sync;

#[facet::container]
#[derive(Clone)]
/// This BasicTestRepo provides enough functionality for the methods in tests_utils to
/// be used. Please don't add new facets to this type, instead create your own test repo type
/// for your tests. You can list out all of the facets individually, or embed BasicTestRepo in your
/// new struct and use delegate to make its facets accessible.
pub struct BasicTestRepo {
    #[facet]
    pub repo_identity: RepoIdentity,

    #[facet]
    pub repo_config: RepoConfig,

    #[facet]
    pub repo_blobstore: RepoBlobstore,

    #[facet]
    pub commit_graph: CommitGraph,

    #[facet]
    pub commit_graph_writer: dyn CommitGraphWriter,

    #[facet]
    pub bonsai_hg_mapping: dyn BonsaiHgMapping,

    #[facet]
    pub bookmarks: dyn Bookmarks,

    #[facet]
    pub filestore_config: FilestoreConfig,

    #[facet]
    pub repo_derived_data: RepoDerivedData,

    #[facet]
    pub bonsai_tag_mapping: dyn BonsaiTagMapping,

    #[facet]
    pub bonsai_git_mapping: dyn BonsaiGitMapping,
}

pub async fn list_working_copy_utf8(
    ctx: &CoreContext,
    repo: &impl Repo,
    cs_id: ChangesetId,
) -> Result<HashMap<NonRootMPath, String>, Error> {
    let wc = list_working_copy(ctx, repo, cs_id).await?;

    wc.into_iter()
        .map(|(path, content)| Ok((path, String::from_utf8(content.to_vec())?)))
        .collect()
}

pub async fn list_working_copy_utf8_with_types(
    ctx: &CoreContext,
    repo: &impl Repo,
    cs_id: ChangesetId,
) -> Result<HashMap<NonRootMPath, (String, FileType)>, Error> {
    let wc = list_working_copy_with_types(ctx, repo, cs_id).await?;

    wc.into_iter()
        .map(|(path, (content, ty))| Ok((path, (String::from_utf8(content.to_vec())?, ty))))
        .collect()
}

pub async fn list_working_copy(
    ctx: &CoreContext,
    repo: &impl Repo,
    cs_id: ChangesetId,
) -> Result<HashMap<NonRootMPath, Bytes>, Error> {
    let wc = list_working_copy_with_types(ctx, repo, cs_id).await?;

    Ok(wc
        .into_iter()
        .map(|(path, (bytes, _ty))| (path, bytes))
        .collect())
}

pub async fn list_working_copy_with_types(
    ctx: &CoreContext,
    repo: &impl Repo,
    cs_id: ChangesetId,
) -> Result<HashMap<NonRootMPath, (Bytes, FileType)>, Error> {
    let root = repo
        .repo_derived_data()
        .derive::<RootContentManifestId>(ctx, cs_id, DerivationPriority::LOW)
        .await?;

    root.into_content_manifest_id()
        .list_leaf_entries(ctx.clone(), repo.repo_blobstore_arc())
        .map_ok(|(path, file)| (path, file.content_id, file.file_type))
        .map_ok(|(path, content_id, file_type)| async move {
            let maybe_content = filestore::fetch(
                repo.repo_blobstore().clone(),
                ctx,
                &FetchKey::Canonical(content_id),
            )
            .await?;
            let s = match maybe_content {
                Some(s) => s,
                None => {
                    return Err(format_err!("cannot fetch content for {path} {content_id}"));
                }
            };
            let bytes = s
                .try_fold(BytesMut::new(), |mut bytes, new_bytes| {
                    bytes.extend_from_slice(&new_bytes);
                    future::ready(Ok(bytes))
                })
                .await?;
            Ok((path, (bytes.freeze(), file_type)))
        })
        .try_buffer_unordered(100)
        .try_collect()
        .await
}

/// Helper to create bonsai changesets in a repo
pub struct CreateCommitContext<'a, R: Repo> {
    ctx: &'a CoreContext,
    repo: &'a R,
    parents: Vec<CommitIdentifier>,
    files: BTreeMap<NonRootMPath, CreateFileContext>,
    message: Option<String>,
    author: Option<String>,
    author_date: Option<DateTime>,
    committer: Option<String>,
    committer_date: Option<DateTime>,
    extra: BTreeMap<String, Vec<u8>>,
}

impl<'a, R: Repo> CreateCommitContext<'a, R> {
    pub fn new(
        ctx: &'a CoreContext,
        repo: &'a R,
        parents: Vec<impl Into<CommitIdentifier>>,
    ) -> Self {
        let parents: Vec<_> = parents.into_iter().map(|x| x.into()).collect();
        Self {
            ctx,
            repo,
            parents,
            files: BTreeMap::new(),
            message: None,
            author: None,
            author_date: None,
            committer: None,
            committer_date: None,
            extra: btreemap! {},
        }
    }

    /// Creates commit with no parents (this is created to avoid specifying generic parameters
    /// in CreateCommitContext::new() when `parents` parameter is an empty vector)
    pub fn new_root(ctx: &'a CoreContext, repo: &'a R) -> Self {
        Self {
            ctx,
            repo,
            parents: vec![],
            files: BTreeMap::new(),
            message: None,
            author: None,
            author_date: None,
            committer: None,
            committer_date: None,
            extra: btreemap! {},
        }
    }

    pub fn add_parent(mut self, id: impl Into<CommitIdentifier>) -> Self {
        self.parents.push(id.into());
        self
    }

    pub fn add_extra(mut self, key: impl Into<String>, value: impl Into<Vec<u8>>) -> Self {
        self.extra.insert(key.into(), value.into());
        self
    }

    pub fn add_file(
        mut self,
        path: impl TryInto<NonRootMPath>,
        content: impl Into<Vec<u8>>,
    ) -> Self {
        self.files.insert(
            path.try_into().ok().expect("Invalid path"),
            CreateFileContext::FromHelper(
                content.into(),
                FileType::Regular,
                GitLfs::FullContent,
                None,
            ),
        );
        self
    }

    pub fn add_files<P: TryInto<NonRootMPath>, C: Into<Vec<u8>>, I: IntoIterator<Item = (P, C)>>(
        mut self,
        path_contents: I,
    ) -> Self {
        for (path, content) in path_contents {
            self = self.add_file(path, content);
        }
        self
    }

    pub fn delete_file(mut self, path: impl TryInto<NonRootMPath>) -> Self {
        self.files.insert(
            path.try_into().ok().expect("Invalid path"),
            CreateFileContext::Deleted,
        );
        self
    }

    pub fn forget_file(mut self, path: impl TryInto<NonRootMPath>) -> Self {
        let path = path.try_into().ok().expect("Invalid path");
        self.files.remove(&path);
        self
    }

    pub fn add_file_with_type(
        mut self,
        path: impl TryInto<NonRootMPath>,
        content: impl Into<Vec<u8>>,
        t: FileType,
    ) -> Self {
        self.files.insert(
            path.try_into().ok().expect("Invalid path"),
            CreateFileContext::FromHelper(content.into(), t, GitLfs::FullContent, None),
        );
        self
    }

    pub fn add_file_with_type_and_lfs(
        mut self,
        path: impl TryInto<NonRootMPath>,
        content: impl Into<Vec<u8>>,
        t: FileType,
        git_lfs: GitLfs,
    ) -> Self {
        self.files.insert(
            path.try_into().ok().expect("Invalid path"),
            CreateFileContext::FromHelper(content.into(), t, git_lfs, None),
        );
        self
    }

    pub fn add_file_with_copy_info(
        mut self,
        path: impl TryInto<NonRootMPath>,
        content: impl Into<Vec<u8>>,
        (parent, parent_path): (impl Into<CommitIdentifier>, impl TryInto<NonRootMPath>),
    ) -> Self {
        let copy_info = (
            parent_path.try_into().ok().expect("Invalid path"),
            parent.into(),
        );
        self.files.insert(
            path.try_into().ok().expect("Invalid path"),
            CreateFileContext::FromHelper(
                content.into(),
                FileType::Regular,
                GitLfs::FullContent,
                Some(copy_info),
            ),
        );
        self
    }

    pub fn add_file_with_copy_info_and_type(
        mut self,
        path: impl TryInto<NonRootMPath>,
        content: impl Into<Vec<u8>>,
        (parent, parent_path): (impl Into<CommitIdentifier>, impl TryInto<NonRootMPath>),
        file_type: FileType,
        git_lfs: GitLfs,
    ) -> Self {
        let copy_info = (
            parent_path.try_into().ok().expect("Invalid path"),
            parent.into(),
        );
        self.files.insert(
            path.try_into().ok().expect("Invalid path"),
            CreateFileContext::FromHelper(content.into(), file_type, git_lfs, Some(copy_info)),
        );
        self
    }

    pub fn add_file_change(
        mut self,
        path: impl TryInto<NonRootMPath>,
        file_change: FileChange,
    ) -> Self {
        self.files.insert(
            path.try_into().ok().expect("Invalid path"),
            CreateFileContext::FromFileChange(file_change),
        );
        self
    }

    pub fn set_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    pub fn set_author(mut self, author: impl Into<String>) -> Self {
        self.author = Some(author.into());
        self
    }

    pub fn set_author_date(mut self, author_date: DateTime) -> Self {
        self.author_date = Some(author_date);
        self
    }

    pub fn set_committer(mut self, committer: impl Into<String>) -> Self {
        self.committer = Some(committer.into());
        self
    }

    pub fn set_committer_date(mut self, committer_date: DateTime) -> Self {
        self.committer_date = Some(committer_date);
        self
    }

    pub async fn create_commit_object(self) -> Result<BonsaiChangesetMut, Error> {
        let parents = future::try_join_all(self.parents.into_iter().map({
            let ctx = self.ctx;
            let repo = self.repo;
            move |p| resolve_cs_id(ctx, repo, p)
        }))
        .await?;

        let files = future::try_join_all(self.files.into_iter().map({
            let ctx = self.ctx;
            let repo = self.repo;
            let parents = &parents;
            move |(path, create_file_context)| async move {
                let file_change = create_file_context
                    .into_file_change(ctx, repo, parents)
                    .await?;

                Result::<_, Error>::Ok((path, file_change))
            }
        }))
        .await?;

        let author_date = match self.author_date {
            Some(author_date) => author_date,
            None => DateTime::from_timestamp(0, 0)?,
        };

        let mut bcs = BonsaiChangesetMut {
            parents,
            author: self.author.unwrap_or_else(|| String::from("author")),
            author_date,
            committer: self.committer,
            committer_date: self.committer_date,
            message: self.message.unwrap_or_else(|| String::from("message")),
            hg_extra: self.extra.into(),
            ..Default::default()
        };

        for (path, file_change) in files {
            bcs.file_changes.insert(path, file_change);
        }

        Ok(bcs)
    }

    pub async fn commit(self) -> Result<ChangesetId, Error> {
        let ctx = self.ctx;
        let repo = self.repo;
        let bcs = self.create_commit_object().await?;
        let bcs = bcs.freeze()?;
        let bcs_id = bcs.get_changeset_id();
        save_changesets(ctx, repo, vec![bcs]).await?;
        Ok(bcs_id)
    }
}

enum CreateFileContext {
    FromHelper(
        Vec<u8>,
        FileType,
        GitLfs,
        Option<(NonRootMPath, CommitIdentifier)>,
    ),
    FromFileChange(FileChange),
    Deleted,
}

impl CreateFileContext {
    async fn into_file_change(
        self,
        ctx: &CoreContext,
        repo: &impl Repo,
        parents: &[ChangesetId],
    ) -> Result<FileChange, Error> {
        let file_change = match self {
            Self::FromHelper(content, file_type, git_lfs, copy_info) => {
                let content = Bytes::copy_from_slice(content.as_ref());

                let meta = filestore::store(
                    repo.repo_blobstore(),
                    repo.filestore_config().clone(),
                    ctx,
                    &StoreRequest::new(content.len().try_into().unwrap()),
                    stream::once(async move { Ok(content) }),
                )
                .await?;

                let copy_info = match copy_info {
                    Some((path, cs_id)) => {
                        let cs_id = resolve_cs_id(ctx, repo, cs_id).await?;

                        if !parents.contains(&cs_id) {
                            return Err(format_err!(
                                "CopyInfo at {path:?} references invalid parent: {cs_id:?}"
                            ));
                        }

                        Some((path, cs_id))
                    }
                    None => None,
                };

                FileChange::tracked(
                    meta.content_id,
                    file_type,
                    meta.total_size,
                    copy_info,
                    git_lfs,
                )
            }
            Self::FromFileChange(file_change) => file_change,
            Self::Deleted => FileChange::Deletion,
        };

        Ok(file_change)
    }
}

/// Returns helper that can be moved to move/delete/create a bookmark
pub fn bookmark<R: Repo + Clone>(
    ctx: &CoreContext,
    repo: &R,
    book_ident: impl Into<BookmarkIdentifier>,
) -> UpdateBookmarkContext<R> {
    UpdateBookmarkContext {
        ctx: ctx.clone(),
        repo: repo.clone(),
        book_ident: book_ident.into(),
    }
}

pub struct UpdateBookmarkContext<R: Repo> {
    ctx: CoreContext,
    repo: R,
    book_ident: BookmarkIdentifier,
}

impl<R: Repo> UpdateBookmarkContext<R> {
    pub async fn set_to(self, cs_ident: impl Into<CommitIdentifier>) -> Result<BookmarkKey, Error> {
        use BookmarkIdentifier::*;
        let bookmark = match self.book_ident {
            Bookmark(bookmark) => bookmark,
            String(s) => BookmarkKey::new(s)?,
        };

        let cs_id = resolve_cs_id(&self.ctx, &self.repo, cs_ident).await?;
        let mut book_txn = self.repo.bookmarks().create_transaction(self.ctx);
        book_txn.force_set(&bookmark, cs_id, BookmarkUpdateReason::TestMove)?;
        book_txn.commit().await?;
        Ok(bookmark)
    }

    pub async fn create_publishing(
        self,
        cs_ident: impl Into<CommitIdentifier>,
    ) -> Result<BookmarkKey, Error> {
        use BookmarkIdentifier::*;
        let bookmark = match self.book_ident {
            Bookmark(bookmark) => bookmark,
            String(s) => BookmarkKey::new(s)?,
        };

        let cs_id = resolve_cs_id(&self.ctx, &self.repo, cs_ident).await?;
        let mut book_txn = self.repo.bookmarks().create_transaction(self.ctx);
        book_txn.create_publishing(&bookmark, cs_id, BookmarkUpdateReason::TestMove)?;
        book_txn.commit().await?;
        Ok(bookmark)
    }

    pub async fn create_pull_default(
        self,
        cs_ident: impl Into<CommitIdentifier>,
    ) -> Result<BookmarkKey, Error> {
        use BookmarkIdentifier::*;
        let bookmark = match self.book_ident {
            Bookmark(bookmark) => bookmark,
            String(s) => BookmarkKey::new(s)?,
        };

        let cs_id = resolve_cs_id(&self.ctx, &self.repo, cs_ident).await?;
        let mut book_txn = self.repo.bookmarks().create_transaction(self.ctx);
        book_txn.create(&bookmark, cs_id, BookmarkUpdateReason::TestMove)?;
        book_txn.commit().await?;
        Ok(bookmark)
    }

    pub async fn create_scratch(
        self,
        cs_ident: impl Into<CommitIdentifier>,
    ) -> Result<BookmarkKey, Error> {
        use BookmarkIdentifier::*;
        let bookmark = match self.book_ident {
            Bookmark(bookmark) => bookmark,
            String(s) => BookmarkKey::new(s)?,
        };

        let cs_id = resolve_cs_id(&self.ctx, &self.repo, cs_ident).await?;
        let mut book_txn = self.repo.bookmarks().create_transaction(self.ctx);
        book_txn.create_scratch(&bookmark, cs_id)?;
        book_txn.commit().await?;
        Ok(bookmark)
    }

    pub async fn delete(self) -> Result<(), Error> {
        use BookmarkIdentifier::*;
        let bookmark = match self.book_ident {
            Bookmark(bookmark) => bookmark,
            String(s) => BookmarkKey::new(s)?,
        };

        let mut book_txn = self.repo.bookmarks().create_transaction(self.ctx);
        book_txn.force_delete(&bookmark, BookmarkUpdateReason::TestMove)?;
        book_txn.commit().await?;
        Ok(())
    }
}

pub enum CommitIdentifier {
    Bonsai(ChangesetId),
    Hg(HgChangesetId),
    String(String),
    Bookmark(BookmarkKey),
}

impl From<ChangesetId> for CommitIdentifier {
    fn from(bcs_id: ChangesetId) -> Self {
        Self::Bonsai(bcs_id)
    }
}

impl From<HgChangesetId> for CommitIdentifier {
    fn from(hg_cs_id: HgChangesetId) -> Self {
        Self::Hg(hg_cs_id)
    }
}

impl From<&str> for CommitIdentifier {
    fn from(s: &str) -> Self {
        Self::String(s.to_string())
    }
}

impl From<String> for CommitIdentifier {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl From<&BookmarkKey> for CommitIdentifier {
    fn from(bookmark: &BookmarkKey) -> Self {
        Self::Bookmark(bookmark.clone())
    }
}

impl From<BookmarkKey> for CommitIdentifier {
    fn from(bookmark: BookmarkKey) -> Self {
        Self::Bookmark(bookmark)
    }
}

pub enum BookmarkIdentifier {
    String(String),
    Bookmark(BookmarkKey),
}

impl From<&str> for BookmarkIdentifier {
    fn from(s: &str) -> Self {
        Self::String(s.to_string())
    }
}

impl From<String> for BookmarkIdentifier {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl From<&BookmarkKey> for BookmarkIdentifier {
    fn from(bookmark: &BookmarkKey) -> Self {
        Self::Bookmark(bookmark.clone())
    }
}

impl From<BookmarkKey> for BookmarkIdentifier {
    fn from(bookmark: BookmarkKey) -> Self {
        Self::Bookmark(bookmark)
    }
}

pub async fn store_files<T: AsRef<str>>(
    ctx: &CoreContext,
    files: BTreeMap<&str, Option<T>>,
    repo: &impl RepoBlobstoreArc,
) -> BTreeMap<NonRootMPath, FileChange> {
    let mut res = btreemap! {};

    for (path, content) in files {
        let path = NonRootMPath::new(path).unwrap();
        match content {
            Some(content) => {
                let content = content.as_ref();
                let size = content.len() as u64;
                let content_id = filestore::store(
                    &repo.repo_blobstore_arc(),
                    FilestoreConfig::no_chunking_filestore(),
                    ctx,
                    &StoreRequest::new(size),
                    stream::iter(vec![anyhow::Ok(Bytes::copy_from_slice(content.as_bytes()))]),
                )
                .await
                .unwrap()
                .content_id;

                let file_change = FileChange::tracked(
                    content_id,
                    FileType::Regular,
                    size,
                    None,
                    GitLfs::FullContent,
                );
                res.insert(path, file_change);
            }
            None => {
                res.insert(path, FileChange::Deletion);
            }
        }
    }
    res
}

pub async fn store_rename(
    ctx: &CoreContext,
    copy_src: (NonRootMPath, ChangesetId),
    path: &str,
    content: &str,
    repo: &impl RepoBlobstoreArc,
) -> (NonRootMPath, FileChange) {
    let path = NonRootMPath::new(path).unwrap();
    let size = content.len() as u64;
    let content_id = filestore::store(
        &repo.repo_blobstore_arc(),
        FilestoreConfig::no_chunking_filestore(),
        ctx,
        &StoreRequest::new(size),
        stream::iter(vec![anyhow::Ok(Bytes::copy_from_slice(content.as_bytes()))]),
    )
    .await
    .unwrap()
    .content_id;

    let file_change = FileChange::tracked(
        content_id,
        FileType::Regular,
        size,
        Some(copy_src),
        GitLfs::FullContent,
    );
    (path, file_change)
}

pub async fn resolve_cs_id(
    ctx: &CoreContext,
    repo: &(impl BookmarksRef + BonsaiHgMappingRef),
    cs_ident: impl Into<CommitIdentifier>,
) -> Result<ChangesetId, Error> {
    use CommitIdentifier::*;
    match cs_ident.into() {
        Bonsai(cs_id) => Ok(cs_id),
        Hg(hg_cs_id) => {
            let maybe_cs_id = repo
                .bonsai_hg_mapping()
                .get_bonsai_from_hg(ctx, hg_cs_id)
                .await?;
            maybe_cs_id.ok_or_else(|| format_err!("{hg_cs_id} not found"))
        }
        Bookmark(bookmark) => {
            let maybe_cs_id = repo
                .bookmarks()
                .get(ctx.clone(), &bookmark, bookmarks::Freshness::MostRecent)
                .await?;
            maybe_cs_id.ok_or_else(|| format_err!("{bookmark} not found"))
        }
        String(hash_or_bookmark) => {
            if let Ok(name) = BookmarkKey::new(hash_or_bookmark.clone()) {
                if let Ok(Some(csid)) = repo
                    .bookmarks()
                    .get(ctx.clone(), &name, bookmarks::Freshness::MostRecent)
                    .await
                {
                    return Ok(csid);
                }
            }

            if let Ok(hg_cs_id) = HgChangesetId::from_str(&hash_or_bookmark) {
                if let Ok(Some(cs_id)) = repo
                    .bonsai_hg_mapping()
                    .get_bonsai_from_hg(ctx, hg_cs_id)
                    .await
                {
                    return Ok(cs_id);
                }
            }

            if let Ok(cs_id) = ChangesetId::from_str(&hash_or_bookmark) {
                return Ok(cs_id);
            }
            Err(format_err!(
                "invalid (hash|bookmark) or does not exist in this repository: {hash_or_bookmark}"
            ))
        }
    }
}

pub async fn create_commit(
    ctx: CoreContext,
    repo: impl Repo,
    parents: Vec<ChangesetId>,
    file_changes: BTreeMap<NonRootMPath, FileChange>,
) -> ChangesetId {
    let bcs = BonsaiChangesetMut {
        parents,
        author: "author".to_string(),
        author_date: DateTime::from_timestamp(0, 0).unwrap(),
        message: "message".to_string(),
        file_changes: file_changes.into(),
        ..Default::default()
    }
    .freeze()
    .unwrap();

    let bcs_id = bcs.get_changeset_id();
    save_changesets(&ctx, &repo, vec![bcs]).await.unwrap();
    bcs_id
}

pub async fn create_commit_with_date(
    ctx: CoreContext,
    repo: impl Repo,
    parents: Vec<ChangesetId>,
    file_changes: BTreeMap<NonRootMPath, FileChange>,
    author_date: DateTime,
) -> ChangesetId {
    let bcs = BonsaiChangesetMut {
        parents,
        author: "author".to_string(),
        author_date,
        message: "message".to_string(),
        hg_extra: Default::default(),
        file_changes: file_changes.into(),
        ..Default::default()
    }
    .freeze()
    .unwrap();

    let bcs_id = bcs.get_changeset_id();
    save_changesets(&ctx, &repo, vec![bcs]).await.unwrap();
    bcs_id
}
