/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! # storemodel
//!
//! Provides trait definitions for storage needs.
//! Useful to de-couple from heavyweight storage implementations.
//!
//! Traits defined by this crate are ideally tech-debt free and
//! consider both hg and git use-cases. This mainly means:
//! - APIs do expose hg details like filelog "copy from" or LFS pointer.
//! - History-related APIs should avoid linkrev or linknode, which do not exist
//!   in git.
//!
//! For flexibility, different features might be defined in different traits.
//! Traits can be combined later. For example, reading file content, metadata,
//! and history should probably be 3 different traits.

use std::any::Any;
use std::any::type_name;
use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use blob::Blob;
use edenapi_trait::SaplingRemoteApi;
pub use edenapi_types::FileAuxData;
pub use edenapi_types::TreeAuxData;
pub use futures;
use metalog::MetaLog;
pub use minibytes;
pub use minibytes::Bytes;
use parking_lot::RwLock;
use repo_minimal_info::RepoMinimalInfo;
use serde::Deserialize;
use serde::Serialize;
use slex_items::Items;
use try_once_lock::OnceLock;
pub use types;
use types::FetchContext;
use types::HgId;
use types::Id20;
use types::Key;
use types::PathComponent;
use types::PathComponentBuf;
use types::RepoPath;
use types::RepoPathBuf;
pub use types::SerializationFormat;
pub use types::tree::FileType;
pub use types::tree::TreeItemFlag;

/// Boxed dynamic iterator. Similar to `BoxStream`.
pub type BoxIterator<T> = Box<dyn Iterator<Item = T> + Send + 'static>;

/// Disk-usage state for an on-disk cache, returned by `cache_disk_usage()`.
///
/// Genuine read failures (e.g. a corrupted or unreadable log) are caught by
/// the caller and mapped to `Unavailable` rather than aborting the whole
/// `HgCacheStats` response over one broken field - `cache_disk_usage()`
/// implementations may still return `Err` themselves; it is the caller's
/// job to downgrade that into `Unavailable` per-field. `Err`/`Unavailable`
/// must never be used for "no cache configured" or "cache is empty", which
/// are represented by the `NotConfigured`/`Available{used: 0, ..}` variants
/// below instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CacheUsage {
    /// No cache is configured for this store (e.g. no `remotefilelog.cachepath`).
    NotConfigured,
    /// This store implementation does not support usage reporting.
    #[default]
    Unsupported,
    /// A cache is configured but its usage could not be measured due to an
    /// error (e.g. corrupted on-disk data). The failure reason is logged by
    /// the caller, not carried on this variant, so it stays `Copy`.
    Unavailable,
    /// A cache is configured; `used` bytes are on disk. `limit` is the
    /// configured cap, or `None` if the cache is uncapped.
    Available { used: u64, limit: Option<u64> },
}

/// Boxed dynamic iterator with lifetime.
pub type BoxRefIterator<'a, T> = Box<dyn Iterator<Item = T> + Send + 'a>;

pub type ContentFetch = (Key, Blob);
pub type ContentFetchItems = Items<ContentFetch, anyhow::Error>;
pub type TreeFetch = (Key, Arc<dyn TreeEntry>);
pub type TreeFetchItems = Items<TreeFetch, anyhow::Error>;

/// A store where content is indexed by "(path, hash)", aka "Key".
pub trait KeyStore: Send + Sync {
    /// Read the content of specified files.
    ///
    /// The returned content should be just the file contents. This means:
    /// - The returned content does not contain the "copy from" header.
    /// - The returned content does not contain raw LFS content. LFS pointer
    ///   is resolved transparently.
    ///
    /// Fetch mode is ignored in this prototype method.
    /// The iterator might block waiting for network.
    fn get_content_iter(
        &self,
        fctx: FetchContext,
        keys: Vec<Key>,
    ) -> anyhow::Result<ContentFetchItems> {
        let store = self.clone_key_store();
        let iter = keys
            .into_iter()
            .map(move |k| match store.get_local_content(&k.path, k.hgid) {
                Err(e) => Err(e),
                Ok(None) => Err(anyhow::format_err!(
                    "{}@{}: not found locally",
                    k.path,
                    k.hgid
                )),
                Ok(Some(data)) => {
                    fctx.inc_local(1);
                    Ok((k, data))
                }
            });
        Ok(ContentFetchItems::item_stream(iter))
    }

    /// Read the content of the specified file without connecting to a remote server.
    /// Return `None` if the file is not available locally.
    fn get_local_content(&self, _path: &RepoPath, _hgid: HgId) -> anyhow::Result<Option<Blob>> {
        Ok(None)
    }

    /// Read the content of the specified file. Ask a remote server on demand.
    /// When fetching many files, use `get_content_iter` instead of calling
    /// this in a loop.
    fn get_content(&self, fctx: FetchContext, path: &RepoPath, hgid: HgId) -> anyhow::Result<Blob> {
        // Handle "broken" implementation that returns Err(_) not Ok(None) on not found.
        if !fctx.mode().is_remote() {
            if let Ok(Some(data)) = self.get_local_content(path, hgid) {
                return Ok(data);
            }
        }

        let key = Key::new(path.to_owned(), hgid);
        match self.get_content_iter(fctx, vec![key])?.into_iter().next() {
            None => Err(anyhow::format_err!("{path}@{hgid}: not found remotely")),
            Some(Err(e)) => Err(e),
            Some(Ok((_k, data))) => Ok(data),
        }
    }

    /// Indicate to the store that we will be attempting to access the given
    /// items soon. Some stores (especially ones that may perform network
    /// I/O) may use this information to prepare for these accesses (e.g., by
    /// by prefetching the nodes in bulk). For some stores this operation does
    /// not make sense, so the default implementation is a no-op.
    ///
    /// This is an old API. Consider `get_content_iter` instead.
    fn prefetch(&self, _keys: Vec<Key>) -> anyhow::Result<()> {
        Ok(())
    }

    /// Insert a serialized entry. Return the hash to the entry.
    ///
    /// After calling this function, the data can be fetched via
    /// `get_local_content` on the same store. The store can buffer pending data
    /// in memory until `flush()`, or `flush()` automatically to keep memory
    /// usage bounded.
    ///
    /// For stores using hg format:
    /// - `parents` is required, and will affect the hash.
    /// - `data` should contain the filelog metadata header.
    ///
    /// For stores using git format:
    /// - `parents` is a hint to choose delta base.
    /// - `data` is the pure content without headers.
    fn insert_data(
        &self,
        _opts: InsertOpts,
        _path: &RepoPath,
        _data: Blob,
    ) -> anyhow::Result<HgId> {
        anyhow::bail!("store {} is read-only", self.type_name())
    }

    /// Write pending in-memory data to disk. Unlike sync(), flush() should be a no-op if
    /// there are no pending writes.
    fn flush(&self) -> anyhow::Result<()> {
        anyhow::bail!("store {} is read-only", self.type_name())
    }

    /// Write pending in-memory data to disk and reload stores from disk.
    fn sync(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Decides whether the store uses git or hg format.
    fn format(&self) -> SerializationFormat {
        // TODO(cuev): Allow Git serialization format
        SerializationFormat::Hg
    }

    /// Free-form statistics.
    fn statistics(&self) -> Vec<(String, usize)> {
        Vec::new()
    }

    /// Optional downcasting. If a store wants downcasting support, implement this
    /// as `Some(self)` explicitly.
    fn maybe_as_any(&self) -> Option<&dyn Any> {
        None
    }

    /// Obtain the type name of the store.
    fn type_name(&self) -> Cow<'static, str> {
        type_name::<Self>().into()
    }

    /// Obtains a snapshot of the store state.
    /// Usually it is just `Arc::clone` under the hood.
    /// Used to relax lifetime requirements for various `BoxIterator` outputs.
    fn clone_key_store(&self) -> Box<dyn KeyStore>;
}

/// A store for files.
pub trait FileStore: KeyStore + 'static {
    /// Get an upper bound on file content size without loading or hashing content.
    /// The bound may include a serialized copy header. None means unsupported.
    fn get_content_size_bound(
        &self,
        _fctx: FetchContext,
        _path: &RepoPath,
        _id: HgId,
    ) -> anyhow::Result<Option<u64>> {
        Ok(None)
    }

    /// Read rename metadata of specified files.
    ///
    /// The result is a vector of (key, rename_from_key) pairs for files with
    /// rename information.
    ///
    /// The iterator might block waiting for network.
    fn get_rename_iter(
        &self,
        _keys: Vec<Key>,
    ) -> anyhow::Result<BoxIterator<anyhow::Result<(Key, Key)>>> {
        Ok(Box::new(std::iter::empty()))
    }

    /// Get auxiliary metadata for a single file.
    /// Returns `None` if the information is unavailable locally.
    /// The default implementation falls back to calculating them from content.
    fn get_local_aux(&self, path: &RepoPath, id: HgId) -> anyhow::Result<Option<FileAuxData>> {
        Ok(self
            .get_local_content(path, id)?
            .map(|data| FileAuxData::from_content(&data)))
    }

    /// Get auxiliary metadata for the given files.
    /// Contact remote server on demand. Might block.
    /// The default implementation falls back to calculating them from content.
    fn get_aux_iter(
        &self,
        fctx: FetchContext,
        keys: Vec<Key>,
    ) -> anyhow::Result<BoxIterator<anyhow::Result<(Key, FileAuxData)>>> {
        let iter = self
            .get_content_iter(fctx, keys)?
            .into_iter()
            .map(|entry| match entry {
                Err(e) => Err(e),
                Ok((key, data)) => Ok((key, FileAuxData::from_content(&data))),
            });
        Ok(Box::new(iter))
    }

    /// Get auxiliary metadata for the given file.
    /// Contact remote server on demand. Might block.
    /// When fetching many files, use `get_aux_iter` instead of calling this in a loop.
    fn get_aux(
        &self,
        fctx: FetchContext,
        path: &RepoPath,
        id: HgId,
    ) -> anyhow::Result<FileAuxData> {
        let key = Key::new(path.to_owned(), id);
        match self.get_aux_iter(fctx, vec![key])?.next() {
            None => Err(anyhow::format_err!("{path}@{id}: not found remotely")),
            Some(Err(e)) => Err(e),
            Some(Ok((_k, aux))) => Ok(aux),
        }
    }

    /// Get parents at the file store layer.
    /// This is only used by legacy Hg logic and is incompatible with Git.
    /// New logic should use `pathhistory` or server-provided history instead.
    fn get_hg_parents(&self, _path: &RepoPath, _id: HgId) -> anyhow::Result<Vec<HgId>> {
        Ok(Vec::new())
    }

    /// Get the "raw" content. For LFS this returns its raw pointer.
    /// This is only used by legacy Hg logic and is incompatible with Git.
    fn get_hg_raw_content(&self, path: &RepoPath, id: HgId) -> anyhow::Result<minibytes::Bytes> {
        // The default fetch mode is AllowRemote, which accesses both local and remote stores.
        self.get_content(FetchContext::default(), path, id)
            .map(|blob| blob.into_bytes())
    }

    /// Get the "raw" flags. For LFS this is non-zero.
    /// This is only used by legacy Hg logic and is incompatible with Git.
    fn get_hg_flags(&self, _path: &RepoPath, _id: HgId) -> anyhow::Result<u32> {
        Ok(0)
    }

    /// Upload LFS files specified by the keys.
    /// This is called before push.
    fn upload_lfs(&self, _keys: Vec<Key>) -> anyhow::Result<()> {
        Ok(())
    }

    /// Similar to `KeyStore::insert_data` with `opts.kind` set to `Kind::File`.
    fn insert_file(
        &self,
        mut opts: InsertOpts,
        path: &RepoPath,
        data: Blob,
    ) -> anyhow::Result<HgId> {
        opts.kind = Kind::File;
        KeyStore::insert_data(self, opts, path, data)
    }

    fn as_key_store(&self) -> &dyn KeyStore
    where
        Self: Sized,
    {
        self
    }

    /// Obtains a snapshot of the store state.
    /// Usually it is just `Arc::clone` under the hood.
    /// Used to relax lifetime requirements for various `BoxIterator` outputs.
    fn clone_file_store(&self) -> Box<dyn FileStore>;

    /// Disk-usage state of the file/blob cache. See `CacheUsage` for the
    /// meaning of each state.
    fn cache_disk_usage(&self) -> anyhow::Result<CacheUsage> {
        Ok(CacheUsage::Unsupported)
    }

    /// Disk-usage state of the LFS blob cache, if this store has one.
    /// Measures only the indexedlog-backed portion of the LFS cache
    /// (`lfs/blobs`, `lfs/pointers`); the loose-file `lfs/objects`
    /// directory is intentionally excluded since there is no cheap way to
    /// size it without a full directory walk.
    fn lfs_cache_disk_usage(&self) -> anyhow::Result<CacheUsage> {
        Ok(CacheUsage::Unsupported)
    }
}

#[async_trait]
pub trait ReadRootTreeIds {
    /// Read root tree nodes of given commits.
    /// Return `(commit_id, tree_id)` list.
    async fn read_root_tree_ids(&self, commits: Vec<HgId>) -> anyhow::Result<Vec<(HgId, HgId)>>;
}

/// Abstracted tree entry.
pub trait TreeEntry: Send + Sync + 'static {
    /// Iterate through the tree items.
    /// Note the iteration order is serialization format defined.
    /// Practically, Git appends `/` to directories when sorting them.
    fn iter<'a>(
        &'a self,
    ) -> anyhow::Result<BoxRefIterator<'a, anyhow::Result<(&'a PathComponent, HgId, TreeItemFlag)>>>;

    /// Like `iter()`, but yields owned `PathComponentBuf`.
    fn iter_owned(
        &self,
    ) -> anyhow::Result<BoxIterator<anyhow::Result<(PathComponentBuf, HgId, TreeItemFlag)>>>;

    /// Lookup a single item.
    /// The actual implementation might use bisect under the hood.
    /// Practically, only hg tree supports bisecting.
    fn lookup(&self, name: &PathComponent) -> anyhow::Result<Option<(HgId, TreeItemFlag)>>;

    /// Iterate through the file aux data if they are available.
    /// For performance reasons, the iteration is on `HgId`.
    fn file_aux_iter(&self) -> anyhow::Result<BoxIterator<anyhow::Result<(HgId, FileAuxData)>>> {
        Ok(Box::new(std::iter::empty()))
    }

    /// Iterate through the child tree aux data if they are available.
    /// For performance reasons, the iteration is on `HgId`.
    fn tree_aux_data_iter(
        &self,
    ) -> anyhow::Result<BoxIterator<anyhow::Result<(HgId, TreeAuxData)>>> {
        Ok(Box::new(std::iter::empty()))
    }

    /// Get the tree aux data data of the tree.
    fn aux_data(&self) -> anyhow::Result<Option<TreeAuxData>> {
        Ok(None)
    }

    /// Get children entries that will be denied permission when fetched.
    fn permission_denied_children(
        &self,
    ) -> anyhow::Result<BoxIterator<anyhow::Result<(PathComponentBuf, HgId, PermissionDenial)>>>
    {
        self.filter_permission_denied(self.children_with_acls()?)
    }

    /// Filter caller-selected `has_acl` directory children to those denied permission when fetched.
    fn filter_permission_denied(
        &self,
        _children_with_acl: Vec<(PathComponentBuf, HgId)>,
    ) -> anyhow::Result<BoxIterator<anyhow::Result<(PathComponentBuf, HgId, PermissionDenial)>>>
    {
        Ok(Box::new(std::iter::empty()))
    }

    /// Get directory children that have `has_acl` set, without triggering
    /// permission checks. Returns `(path_component, manifest_id)` pairs.
    fn children_with_acls(&self) -> anyhow::Result<Vec<(PathComponentBuf, HgId)>> {
        Ok(Vec::new())
    }

    /// Get number of entries, if available. Useful to pre-allocate vector capacity, etc.
    fn size_hint(&self) -> Option<usize>;
}

/// Why a `has_acl` directory child was denied, as reported by the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionDenial {
    /// ACL to request access through.
    pub request_acl: String,
    /// Repo-configured text to show with the denial, if any.
    pub denial_message: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PathAclEntry {
    pub restriction_root: RepoPathBuf,
    pub repo_region_acl: String,
    pub permission_request_group: String,
}

#[derive(Clone, Debug)]
pub struct PathAclInfo {
    pub path: RepoPathBuf,
    pub entries: Vec<PathAclEntry>,
    pub error: Option<String>,
}

/// The `TreeStore` is an abstraction layer for the tree manifest that decouples how or where the
/// data is stored. This allows more easy iteration on serialization format. It also simplifies
/// writing storage migration.
pub trait TreeStore: KeyStore {
    /// List a tree with optional auxiliary metadata.
    /// Returns `None` if the information is unavailable locally.
    ///
    /// The default implementation does not provide the aux data.
    /// Currently mainly used by EdenFS.
    fn get_local_tree(
        &self,
        path: &RepoPath,
        id: HgId,
    ) -> anyhow::Result<Option<Arc<dyn TreeEntry>>> {
        let data = match self.get_local_content(path, id)? {
            None => return Ok(None),
            Some(v) => v,
        };
        Ok(Some(basic_parse_tree(data.into_bytes(), self.format())?))
    }

    fn get_remote_tree_iter(
        &self,
        _keys: Vec<Key>,
    ) -> anyhow::Result<BoxIterator<anyhow::Result<(Key, Arc<dyn TreeEntry>)>>> {
        // Function proto, let subclasses implement
        anyhow::bail!("Remote content iterator is not implemented")
    }

    /// List trees with optional auxiliary metadata.
    ///
    /// Currently mainly used by EdenFS.
    fn get_tree_iter(&self, _fctx: FetchContext, keys: Vec<Key>) -> anyhow::Result<TreeFetchItems> {
        let store = self.clone_tree_store();
        let iter = keys
            .into_iter()
            .map(move |k| match store.get_local_tree(&k.path, k.hgid) {
                Err(e) => Err(e),
                Ok(None) => Err(anyhow::format_err!(
                    "{}@{}: not found locally",
                    k.path,
                    k.hgid
                )),
                Ok(Some(data)) => Ok((k, data)),
            });
        Ok(TreeFetchItems::item_stream(iter))
    }

    /// Fetch a single tree with full metadata.
    /// Tries local first, falls back to remote via `get_tree_iter`.
    /// Unary remote fetches are discouraged - prefer `get_tree_iter`.
    fn get_tree(
        &self,
        fctx: FetchContext,
        path: &RepoPath,
        id: HgId,
    ) -> anyhow::Result<Arc<dyn TreeEntry>> {
        if let Some(tree) = self.get_local_tree(path, id)? {
            return Ok(tree);
        }
        let key = Key::new(path.to_owned(), id);
        match self.get_tree_iter(fctx, vec![key])?.into_iter().next() {
            Some(Ok((_, tree))) => Ok(tree),
            Some(Err(e)) => Err(e),
            None => Err(anyhow::format_err!("{path}@{id}: tree not found")),
        }
    }

    /// List metadata of the given trees.
    /// Contact remote server on demand. Might block.
    ///
    /// Ignores fctx.mode in the default implementation
    /// Currently mainly used by EdenFS.
    fn get_tree_aux_data_iter(
        &self,
        _fctx: FetchContext,
        keys: Vec<Key>,
    ) -> anyhow::Result<BoxIterator<anyhow::Result<(Key, TreeAuxData)>>> {
        let store = self.clone_tree_store();
        let iter =
            keys.into_iter().map(
                move |k| match store.get_local_tree_aux_data(&k.path, k.hgid) {
                    Err(e) => Err(e),
                    Ok(None) => Err(anyhow::format_err!(
                        "{}@{}: not found locally",
                        k.path,
                        k.hgid
                    )),
                    Ok(Some(data)) => Ok((k, data)),
                },
            );
        Ok(Box::new(iter))
    }

    fn as_key_store(&self) -> &dyn KeyStore
    where
        Self: Sized,
    {
        self
    }

    /// Get tree aux data for a single tree.
    /// Returns `None` if the information is unavailable locally.
    fn get_local_tree_aux_data(
        &self,
        path: &RepoPath,
        id: HgId,
    ) -> anyhow::Result<Option<TreeAuxData>> {
        match self.get_local_tree(path, id)? {
            None => Ok(None),
            Some(e) => e.aux_data(),
        }
    }

    /// Get tree aux data for the given tree.
    /// Contact remote server on demand. Might block.
    /// When fetching data for many trees, use `get_tree_aux_data_iter`
    /// instead of calling this in a loop.
    fn get_tree_aux_data(
        &self,
        fctx: FetchContext,
        path: &RepoPath,
        id: HgId,
    ) -> anyhow::Result<TreeAuxData> {
        let key = Key::new(path.to_owned(), id);
        match self.get_tree_aux_data_iter(fctx, vec![key.clone()])?.next() {
            None => Err(anyhow::format_err!("{path}@{id}: not found remotely")),
            Some(Err(e)) => Err(e),
            Some(Ok((_k, aux))) => Ok(aux),
        }
    }

    /// Fetch path-based ACL metadata for repo-relative paths at a commit.
    ///
    /// Results are aligned with input paths. Implementations that can answer a
    /// batch but fail individual paths should return per-path errors instead of
    /// silently falling back to unknown data.
    fn check_path_permissions(
        &self,
        _hg_cs_id: HgId,
        _paths: Vec<RepoPathBuf>,
    ) -> anyhow::Result<Vec<PathAclInfo>> {
        Err(anyhow::format_err!(
            "path ACL lookup unavailable: no permission backend configured"
        ))
    }

    /// Similar to `KeyStore::insert_data` with `opts.kind` set to `Kind::Tree`.
    fn insert_tree(
        &self,
        mut opts: InsertOpts,
        path: &RepoPath,
        items: Vec<(PathComponentBuf, Id20, TreeItemFlag)>,
    ) -> anyhow::Result<Id20> {
        opts.kind = Kind::Tree;
        let data = basic_serialize_tree(items, self.format())?;
        KeyStore::insert_data(self, opts, path, data.into())
    }

    /// Record that a permission-denied tree was encountered at the given path.
    fn record_permission_denied(&self, _err: types::errors::PermissionDenied) {}

    /// Obtains a snapshot of the store state.
    /// Usually it is just `Arc::clone` under the hood.
    /// Used to relax lifetime requirements for various `BoxIterator` outputs.
    fn clone_tree_store(&self) -> Box<dyn TreeStore>;

    /// Disk-usage state of the tree cache. See `CacheUsage` for the meaning
    /// of each state.
    fn cache_disk_usage(&self) -> anyhow::Result<CacheUsage> {
        Ok(CacheUsage::Unsupported)
    }
}

/// Options used by `insert_data`
#[derive(Deserialize)]
pub struct InsertOpts {
    /// Parent hashes.
    /// For Hg it's required and affects SHA1.
    /// For Git it's a hint about the delta bases.
    #[serde(default)]
    pub parents: Vec<HgId>,

    /// Whether this is a file or a tree.
    /// For Hg it's ignored. For Git it affects SHA1.
    #[serde(default)]
    pub kind: Kind,

    /// Forced SHA1 to use. Mainly for testing purpose.
    #[serde(default)]
    pub forced_id: Option<Box<HgId>>,

    /// Hg flags to use. Used for legacy LFS support.
    #[serde(default)]
    pub hg_flags: u32,

    /// Avoid insert if data is already in store.
    #[serde(default)]
    pub read_before_write: bool,

    /// Indices (in manifest blob order) of directory children that have ACL.
    #[serde(default)]
    pub acl_children_indices: Option<Vec<u32>>,

    /// Require durable storage that won't rotate out. When false, the store
    /// may use non-permanent storage (e.g. a shared cache) that can be
    /// reclaimed. Defaults to true.
    #[serde(default = "default_true")]
    pub permanent: bool,
}

fn default_true() -> bool {
    true
}

impl Default for InsertOpts {
    fn default() -> Self {
        Self {
            parents: Vec::new(),
            kind: Kind::default(),
            forced_id: None,
            hg_flags: 0,
            read_before_write: false,
            permanent: true,
            acl_children_indices: None,
        }
    }
}

/// Distinguish between a file and a tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Kind {
    #[default]
    File,
    Tree,
}

/// Provide information about how to build a file, tree or commit graph backend.
pub trait StoreInfo: 'static {
    /// Check requirement. Return `true` if the requirement is present.
    fn has_requirement(&self, requirement: &str) -> bool;
    /// Provides the config.
    fn config(&self) -> &dyn configmodel::Config;
    /// Provide the "storage path", which is usually `.sl/store` in the backing repo.
    /// Returns `None` for ephemeral repos with no local disk storage.
    fn store_path(&self) -> Option<&Path>;
    /// Provide the minimal information for a local repository.
    ///
    /// Returns `None` for repositories without a local working copy.
    fn repo_minimal_info(&self) -> Option<&RepoMinimalInfo> {
        None
    }
    /// Provide the remote peer.
    fn remote_peer(&self) -> anyhow::Result<Option<Arc<dyn SaplingRemoteApi>>>;
    // Provide the metalog, useful to sync refs from git.
    fn metalog(&self) -> anyhow::Result<Arc<RwLock<MetaLog>>>;
}

/// Provide ways to obtain file and tree stores.
pub trait StoreOutput: 'static {
    /// Obtain the file store.
    fn file_store(&self) -> Arc<dyn FileStore>;

    /// Obtain the tree store.
    ///
    /// Based on the implementation, this might or might not be the same as the
    /// file store under the hood.
    fn tree_store(&self) -> Arc<dyn TreeStore>;
}

impl<T: FileStore + TreeStore> StoreOutput for Arc<T> {
    fn file_store(&self) -> Arc<dyn FileStore> {
        self.clone() as Arc<dyn FileStore>
    }

    fn tree_store(&self) -> Arc<dyn TreeStore> {
        self.clone() as Arc<dyn TreeStore>
    }
}

#[doc(hidden)]
pub type StaticSerializedTreeParseFunc =
    fn(Bytes, SerializationFormat) -> anyhow::Result<Arc<dyn TreeEntry>>;

#[doc(hidden)]
pub type StaticSerializeTreeFunc =
    fn(Vec<(PathComponentBuf, Id20, TreeItemFlag)>, SerializationFormat) -> anyhow::Result<Bytes>;

/// Parse a serialized git or hg tree into `TreeEntry`.
/// This is basic parsing that does not provide `FileAuxData`.
/// The actual implementation is elsewhere to avoid cyclic dependencies.
pub fn basic_parse_tree(
    data: Bytes,
    format: SerializationFormat,
) -> anyhow::Result<Arc<dyn TreeEntry>> {
    // Only call `call_constructor` once to avoid overhead in `factory`.
    static TREE_PARSER: OnceLock<StaticSerializedTreeParseFunc> = OnceLock::new();
    let parse = TREE_PARSER
        .get_or_try_init(|| factory::call_constructor::<(), StaticSerializedTreeParseFunc>(&()))?;
    parse(data, format)
}

/// Serialize tree items to bytes.
pub fn basic_serialize_tree(
    items: Vec<(PathComponentBuf, Id20, TreeItemFlag)>,
    format: SerializationFormat,
) -> anyhow::Result<Bytes> {
    // Only call `call_constructor` once to avoid overhead in `factory`.
    static TREE_SERIALIZER: OnceLock<StaticSerializeTreeFunc> = OnceLock::new();
    let serialize = TREE_SERIALIZER
        .get_or_try_init(|| factory::call_constructor::<(), StaticSerializeTreeFunc>(&()))?;
    serialize(items, format)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestKeyStore;

    impl KeyStore for TestKeyStore {
        fn get_local_content(&self, _path: &RepoPath, _hgid: HgId) -> anyhow::Result<Option<Blob>> {
            Ok(Some(Blob::from_static(b"content")))
        }

        fn clone_key_store(&self) -> Box<dyn KeyStore> {
            Box::new(TestKeyStore)
        }
    }

    #[test]
    fn test_get_content_iter_default_impl_increments_local_fetch_count() {
        let store = TestKeyStore;
        let fctx = FetchContext::default();
        let key = Key::new(
            RepoPathBuf::from_string("foo".to_string()).expect("valid repo path"),
            HgId::from_byte_array([1; HgId::len()]),
        );

        let results: Vec<_> = store
            .get_content_iter(fctx.clone(), vec![key])
            .expect("get_content_iter should succeed")
            .into_iter()
            .collect();

        assert_eq!(results.len(), 1);
        assert!(results[0].is_ok());
        assert_eq!(fctx.local_fetch_count(), 1);
        assert_eq!(fctx.remote_fetch_count(), 0);
    }
}
