/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ops::Deref;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use manifest::File;
use manifest::FileMetadata;
use manifest::FsNodeMetadata;
use try_once_lock::OnceLock;
use types::HgId;
use types::Key;
use types::PathComponent;
use types::PathComponentBuf;
use types::RepoPath;
use types::RepoPathBuf;
use types::tree::TreeItemFlag;

use crate::store::InnerStore;

// Allows sending link between threads, but disallows general copying.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct Link {
    inner: Arc<LinkData>,
}

impl Deref for Link {
    type Target = LinkData;
    fn deref(&self) -> &LinkData {
        self.inner.as_ref()
    }
}

impl AsRef<LinkData> for Link {
    fn as_ref(&self) -> &LinkData {
        self.inner.as_ref()
    }
}

impl Clone for Link {
    fn clone(&self) -> Self {
        // Most code should not be aware of the fact that Link can be cloned as an Arc, so for the
        // default clone implementation do a deep clone. thread_copy() should be used for explicit
        // cases that need a shallow copy.
        Link::new(self.inner.as_ref().clone())
    }
}

/// `Link` describes the type of nodes that tree manifest operates on.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub enum LinkData {
    /// `Leaf` nodes store FileMetadata. They are terminal nodes and don't have any other
    /// information.
    Leaf(FileMetadata),
    /// `Ephemeral` nodes are inner nodes that have not been committed to storage. They are only
    /// available in memory. They need to be persisted to be available in future. They are the
    /// mutable type of an inner node. They store the contents of a directory that has been
    /// modified.
    Ephemeral(BTreeMap<PathComponentBuf, Link>),
    /// `Durable` nodes are inner nodes that come from storage. Their contents can be
    /// shared between multiple instances of Tree. They are lazily evaluated. Their children
    /// list will be read from storage only when it is accessed.
    Durable(Arc<DurableEntry>),
}
pub use self::LinkData::*;

/// Result of materializing a durable tree entry's children.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub enum MaybeLinks {
    /// The tree was fetched successfully.
    Links(BTreeMap<PathComponentBuf, Link>),
    /// Access to this tree was denied by a path ACL.
    PermissionDenied(types::errors::PermissionDenied),
}

// TODO: Use Vec instead of BTreeMap
/// The inner structure of a durable link.
pub struct DurableEntry {
    pub hgid: HgId,
    pub links: OnceLock<MaybeLinks>,
    tree_entry: OnceLock<Arc<dyn storemodel::TreeEntry>>,
}

impl std::fmt::Debug for DurableEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableEntry")
            .field("hgid", &self.hgid)
            .field("links", &self.links)
            .finish_non_exhaustive()
    }
}

impl Link {
    pub fn new(link: LinkData) -> Self {
        Link {
            inner: Arc::new(link),
        }
    }

    pub fn durable(hgid: HgId) -> Link {
        Link::new(LinkData::Durable(Arc::new(DurableEntry::new(hgid))))
    }

    pub fn durable_permission_denied(err: types::errors::PermissionDenied) -> Link {
        let hgid = err.hgid;
        let links = OnceLock::new();
        links.set(MaybeLinks::PermissionDenied(err)).unwrap();
        Link::new(LinkData::Durable(Arc::new(DurableEntry {
            hgid,
            links,
            tree_entry: OnceLock::new(),
        })))
    }

    pub fn ephemeral() -> Link {
        Link::new(LinkData::Ephemeral(BTreeMap::new()))
    }

    pub fn leaf(metadata: FileMetadata) -> Link {
        Link::new(LinkData::Leaf(metadata))
    }

    pub fn mut_ephemeral_links(
        &mut self,
        store: &InnerStore,
        parent: &RepoPath,
    ) -> Result<&mut BTreeMap<PathComponentBuf, Link>> {
        self.as_mut_ref()?.mut_ephemeral_links(store, parent)
    }

    pub fn to_fs_node(&self) -> FsNodeMetadata {
        match self.as_ref() {
            Leaf(metadata) => FsNodeMetadata::File(*metadata),
            Ephemeral(_) => FsNodeMetadata::Directory(None),
            Durable(durable) => FsNodeMetadata::Directory(Some(durable.hgid)),
        }
    }

    /// Create a file record for a `Link`, failing if the link
    /// refers to a directory rather than a file.
    pub fn to_file(&self, path: RepoPathBuf) -> Option<File> {
        match self.as_ref() {
            Leaf(metadata) => Some(File::new(path, *metadata)),
            _ => None,
        }
    }

    pub fn thread_copy(&self) -> Self {
        Link {
            inner: self.inner.clone(),
        }
    }

    pub fn as_mut_ref(&mut self) -> Result<&mut LinkData> {
        // This introduces an unusual mutability pattern where we allow mutations as long as there
        // is only one copy of the Link's Arc. That one copy will always be the parent directory.
        // In normal treemanifest operations, these Links are never shared between trees (and in
        // fact the only way to copy the Arc is through the thread_copy function) so it will always
        // be the case that there is only one copy.
        //
        // The one exception, and the reason we use Arc at all, is during tree traversals. In that
        // case we pass copies of the Arc to other threads for parallel traversals. Therefore we
        // cannot use as_mut_ref() while traversing the tree.
        //
        // Normally we'd use RwLock so the compiler could enforce this, but having a RwLock on
        // every Link would be expensive and tree reads are a critical hotpath. Using Arc like this
        // gives us zero-cost reads and standard rust mutable-reference safety, as long as we don't
        // copy the Arc.

        Arc::get_mut(&mut self.inner).ok_or_else(|| {
            anyhow!("cannot mutate tree manifest link if there are multiple readers")
        })
    }

    pub fn is_leaf(&self) -> bool {
        matches!(self.as_ref(), Leaf(_))
    }

    pub fn is_ephemeral(&self) -> bool {
        matches!(self.as_ref(), Ephemeral(_))
    }
}

impl LinkData {
    pub fn mut_ephemeral_links(
        &mut self,
        store: &InnerStore,
        parent: &RepoPath,
    ) -> Result<&mut BTreeMap<PathComponentBuf, Link>> {
        loop {
            match self {
                Leaf(_) => bail!("Path {parent} is a file but a directory was expected."),
                Ephemeral(links) => return Ok(links),
                &mut Durable(ref entry) => {
                    let durable_links = entry.materialize_links(store, parent)?;
                    *self = Ephemeral(durable_links.clone());
                }
            };
        }
    }
}

impl DurableEntry {
    pub fn new(hgid: HgId) -> Self {
        DurableEntry {
            hgid,
            links: OnceLock::new(),
            tree_entry: OnceLock::new(),
        }
    }

    pub fn with_links(hgid: HgId, links: OnceLock<MaybeLinks>) -> Self {
        DurableEntry {
            hgid,
            links,
            tree_entry: OnceLock::new(),
        }
    }

    pub fn is_permission_denied(&self) -> bool {
        matches!(self.links.get(), Some(MaybeLinks::PermissionDenied(_)))
    }

    pub fn permission_denied_error(&self) -> Option<&types::errors::PermissionDenied> {
        match self.links.get() {
            Some(MaybeLinks::PermissionDenied(err)) => Some(err),
            _ => None,
        }
    }

    /// Returns true if links have already been materialized.
    pub fn links_initialized(&self) -> bool {
        matches!(self.links.get(), Some(MaybeLinks::Links(_)))
    }

    pub fn get_tree_entry(&self) -> Option<&Arc<dyn storemodel::TreeEntry>> {
        self.tree_entry.get()
    }

    /// Returns the tree entry, loading it from `store` if the links were set without one: by the
    /// batch prefetch, or for a tree written by this process.
    pub fn get_or_load_tree_entry(
        &self,
        store: &InnerStore,
        path: &RepoPath,
    ) -> Result<&Arc<dyn storemodel::TreeEntry>> {
        self.tree_entry
            .get_or_try_init(|| store.get_tree_entry(path, self.hgid))
    }

    pub fn materialize_links(
        &self,
        store: &InnerStore,
        path: &RepoPath,
    ) -> Result<&BTreeMap<PathComponentBuf, Link>> {
        let maybe = self.links.get_or_try_init(|| -> Result<MaybeLinks> {
            let tree_entry = match store.get_tree_entry(path, self.hgid) {
                Ok(entry) => entry,
                Err(err) => match err.downcast::<types::errors::PermissionDenied>() {
                    Ok(perm_denied) => return Ok(MaybeLinks::PermissionDenied(perm_denied)),
                    Err(err) => {
                        return Err(err.context(format!(
                            "failed fetching from store ({}, {})",
                            path, self.hgid
                        )))
                    }
                },
            };

            let mut links = BTreeMap::new();
            for item_result in tree_entry.iter()? {
                let (component, hgid, flag) = item_result.with_context(|| {
                    format!(
                        "failed to deserialize manifest entry for ({}, {}) (store: {:?}, format: {:?})",
                        path,
                        self.hgid,
                        store.type_name(),
                        store.format(),
                    )
                })?;
                let link = match flag {
                    TreeItemFlag::File(file_type) => {
                        Link::leaf(FileMetadata::new(hgid, file_type))
                    }
                    TreeItemFlag::Directory => Link::durable(hgid),
                };
                links.insert(component.to_owned(), link);
            }

            let _ = self.tree_entry.set(tree_entry);

            Ok(MaybeLinks::Links(links))
        })?;
        match maybe {
            MaybeLinks::Links(links) => Ok(links),
            MaybeLinks::PermissionDenied(err) => {
                let mut err = err.clone();
                err.path = path.to_owned();
                Err(err.into())
            }
        }
    }
}

// `PartialEq` can't be derived because `fallible::Error` does not implement `PartialEq`.
// It should also be noted that `self.links.get() != self.links.get()` can evaluate to true when
// `self.links` are being instantiated.
#[cfg(test)]
impl PartialEq for DurableEntry {
    fn eq(&self, other: &DurableEntry) -> bool {
        if self.hgid != other.hgid {
            return false;
        }
        match (self.links.get(), other.links.get()) {
            (None, None) => true,
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }
}

/// A directory (inner node) encountered during a tree traversal.
///
/// The directory may have a manifest hgid if it is unmodified from its
/// state on disk. If the directory has in-memory modifications that have not
/// been persisted to disk, it will not have an hgid.
#[derive(Clone, Debug)]
pub struct DirLink {
    pub path: RepoPathBuf,
    pub link: Link,
}

impl DirLink {
    /// Create a directory record for a `Link`, failing if the link
    /// refers to a file rather than a directory.
    pub fn from_link(link: &Link, path: RepoPathBuf) -> Option<Self> {
        if let Leaf(_) = link.as_ref() {
            return None;
        }
        Some(DirLink {
            path,
            link: link.thread_copy(),
        })
    }

    /// Same as `from_link`, but set the directory's path to the empty
    /// path, making this method only useful for the root of the tree.
    pub fn from_root(link: &Link) -> Option<Self> {
        Self::from_link(link, RepoPathBuf::new())
    }

    pub fn hgid(&self) -> Option<HgId> {
        match self.link.as_ref() {
            Leaf(_) | Ephemeral(_) => None,
            Durable(entry) => Some(entry.hgid),
        }
    }

    /// Return a fetch key if this directory is durable and its links are not loaded.
    pub(crate) fn prefetch_key(&self) -> Option<Key> {
        match self.link.as_ref() {
            Durable(entry) if !entry.links_initialized() => {
                Some(Key::new(self.path.clone(), entry.hgid))
            }
            Leaf(_) | Ephemeral(_) | Durable(_) => None,
        }
    }

    /// Look up a child without copying its subtree.
    pub(crate) fn lookup(&self, store: &InnerStore, name: &PathComponent) -> Result<Option<Link>> {
        Ok(self
            .link_map(store)?
            .get(name)
            .map(|link| link.thread_copy()))
    }

    /// List the contents of this directory.
    ///
    /// Returns two sorted vectors of files and directories contained
    /// in this directory.
    ///
    /// This operation may perform I/O to load the tree entry from the store
    /// if it is not already in memory. Depending on the store implementation,
    /// this may involve an expensive network request if the required data is
    /// not available locally. As such, algorithms that require fast access to
    /// this data should take care to ensure that this content is present
    /// locally before calling this method.
    pub fn list(&self, store: &InnerStore) -> Result<(Vec<File>, Vec<DirLink>)> {
        let mut files = Vec::new();
        let mut dirs = Vec::new();

        for (name, link) in self.links(store)? {
            let mut path = self.path.clone();
            path.push(name.as_path_component());
            match link.as_ref() {
                Leaf(_) => files.push(link.to_file(path).expect("leaf node must be a valid file")),
                Ephemeral(_) | Durable(_) => dirs.push(
                    DirLink::from_link(link, path).expect("inner node must be a valid directory"),
                ),
            }
        }

        Ok((files, dirs))
    }

    /// Iterate over link entries, sorted by name.
    ///
    /// Less convenient than `list`, but allows iterating both files and directories at
    /// the same time.
    pub fn links(
        &self,
        store: &InnerStore,
    ) -> Result<impl Iterator<Item = (&PathComponentBuf, &Link)> + use<'_>> {
        Ok(self.link_map(store)?.iter())
    }

    /// Return this directory's children, loading them if necessary.
    fn link_map(&self, store: &InnerStore) -> Result<&BTreeMap<PathComponentBuf, Link>> {
        let links = match self.link.as_ref() {
            Leaf(_) => panic!("programming error: directory cannot be a leaf node"),
            Ephemeral(links) => links,
            Durable(entry) => entry.materialize_links(store, &self.path)?,
        };
        Ok(links)
    }

    pub fn permission_denied_error(&self) -> Option<&types::errors::PermissionDenied> {
        match self.link.as_ref() {
            Durable(entry) => entry.permission_denied_error(),
            _ => None,
        }
    }
}

impl Eq for DirLink {}

impl PartialEq for DirLink {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.hgid() == other.hgid()
    }
}

impl Ord for DirLink {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.path.cmp(&other.path) {
            Ordering::Equal => self.hgid().cmp(&other.hgid()),
            ord => ord,
        }
    }
}

impl PartialOrd for DirLink {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use manifest::testutil::*;
    use types::testutil::*;

    use super::*;
    use crate::testutil::*;

    #[test]
    fn test_file_from_link() {
        // Leaf link should result in a file.
        let meta = make_meta("a");
        let path = repo_path_buf("test/leaf");

        let leaf = Link::leaf(meta.clone());
        let file = leaf.to_file(path.clone()).unwrap();

        let expected = File {
            path: path.clone(),
            meta,
        };
        assert_eq!(file, expected);

        // Attempting to use a directory link should fail.
        let ephemeral = Link::ephemeral();
        let _file = ephemeral.to_file(path.clone());

        // Durable link should result in a directory.
        let durable = Link::durable(hgid("a"));
        let file = durable.to_file(path);
        assert!(file.is_none());
    }

    #[test]
    fn test_directory_from_link() {
        let meta = make_meta("a");
        let path = repo_path_buf("test/leaf");

        let ephemeral = Link::ephemeral();
        let dir = DirLink::from_link(&ephemeral, path.clone()).unwrap();
        let expected = DirLink {
            path: path.clone(),
            link: ephemeral,
        };
        assert_eq!(dir, expected);

        let hash = hgid("b");
        let durable = Link::durable(hash);
        let dir = DirLink::from_link(&durable, path.clone()).unwrap();
        let expected = DirLink {
            path: path.clone(),
            link: durable,
        };
        assert_eq!(dir, expected);

        // If the Link is actually a file, we should get None.
        let leaf = Link::leaf(meta.clone());
        let dir = DirLink::from_link(&leaf, path);
        assert!(dir.is_none());
    }

    #[test]
    fn test_list_directory() -> Result<()> {
        let store = Arc::new(TestStore::new());
        let tree = make_tree_manifest(store, &[("a", "1"), ("b/f", "2"), ("c", "3"), ("d/f", "4")]);
        let dir = DirLink::from_root(&tree.root).unwrap();
        let (files, dirs) = dir.list(&tree.store)?;

        let file_names = files.into_iter().map(|f| f.path).collect::<Vec<_>>();
        let dir_names = dirs.into_iter().map(|d| d.path).collect::<Vec<_>>();

        assert_eq!(file_names, vec![repo_path_buf("a"), repo_path_buf("c")]);
        assert_eq!(dir_names, vec![repo_path_buf("b"), repo_path_buf("d")]);

        Ok(())
    }
}
