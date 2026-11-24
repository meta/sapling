/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

mod acl_metrics;
mod bfs;
pub mod config;
mod diff;
mod factory_impls;
mod iter;
mod link;
mod namecmp;
mod store;
pub mod testutil;
mod trait_impls;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::collections::btree_map::Entry;
use std::fmt;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use manifest::DiffEntry;
use manifest::DirDiffEntry;
use manifest::Directory;
use manifest::File;
pub use manifest::FileMetadata;
pub use manifest::FileType;
pub use manifest::FsNodeMetadata;
use manifest::List;
pub use manifest::Manifest;
use manifest::PersistOpts;
use minibytes::Bytes;
use pathmatcher::AlwaysMatcher;
use pathmatcher::DynMatcher;
use pathmatcher::Matcher;
use slex::Items;
pub use store::Flag;
use storemodel::InsertOpts;
use storemodel::Kind;
use storemodel::SerializationFormat;
use thiserror::Error;
use try_once_lock::OnceLock;
use types::HgId;
use types::Key;
pub use types::PathComponent;
pub use types::PathComponentBuf;
use types::RepoPath;
use types::RepoPathBuf;

pub(crate) use self::link::Link;
pub use self::store::Element as TreeElement;
pub use self::store::Entry as TreeEntry;
pub use self::store::TreeStore;
use crate::iter::DfsCursor;
use crate::iter::Step;
use crate::link::DirLink;
use crate::link::Durable;
use crate::link::DurableEntry;
use crate::link::Ephemeral;
use crate::link::Leaf;
use crate::link::MaybeLinks;
use crate::store::InnerStore;

/// The Tree implementation of a Manifest dedicates an inner node for each directory in the
/// repository and a leaf for each file.
#[derive(Clone)]
pub struct TreeManifest {
    store: InnerStore,
    // TODO: root can't be a Leaf
    root: Link,

    // List of from->to grafts to perform before diff operation.
    diff_grafts: Vec<(RepoPathBuf, RepoPathBuf)>,

    // Should ONLY be set for grepo as a workaround for overlapping project paths.
    // See examples in test-grepo.t.
    //
    // When set, Manifest methods transparently encode input paths and decode output paths.
    path_translator: Option<Arc<dyn PathTranslator>>,
}

/// A logical node encountered while reading a [`TreeManifest`].
///
/// The node retains its current directory position, so looking up children continues from this
/// node instead of traversing again from the manifest root. A logical path may have both file
/// content and a directory position when a path translator stores the file under an encoded name.
pub struct TreeManifestReadNode {
    store: InnerStore,
    path_translator: Option<Arc<dyn PathTranslator>>,
    path: RepoPathBuf,
    metadata: FsNodeMetadata,
    directory: Option<DirLink>,
}

impl Clone for TreeManifestReadNode {
    fn clone(&self) -> Self {
        let directory = self.directory.as_ref().map(|directory| DirLink {
            path: directory.path.clone(),
            link: directory.link.thread_copy(),
        });
        Self {
            store: self.store.clone(),
            path_translator: self.path_translator.clone(),
            path: self.path.clone(),
            metadata: self.metadata,
            directory,
        }
    }
}

impl fmt::Debug for TreeManifestReadNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TreeManifestReadNode")
            .field("path", &self.path)
            .field("metadata", &self.metadata)
            .field("directory_hgid", &self.directory_hgid())
            .finish()
    }
}

impl TreeManifestReadNode {
    /// Return the logical path represented by this node.
    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    /// Return the content visible when this exact logical path is queried.
    pub fn metadata(&self) -> FsNodeMetadata {
        self.metadata
    }

    /// Return whether this node can be traversed as a directory.
    pub fn is_directory(&self) -> bool {
        self.directory.is_some()
    }

    /// Return the persisted directory ID, or `None` if the node has no directory or the directory
    /// is ephemeral.
    pub fn directory_hgid(&self) -> Option<HgId> {
        self.directory.as_ref().and_then(DirLink::hgid)
    }

    /// Prefetch the unloaded durable directories of `nodes` in one batch.
    ///
    /// All nodes must use the same store. Nodes from different manifests can share directories,
    /// so the keys are deduplicated.
    pub fn prefetch(nodes: &[TreeManifestReadNode]) -> Result<()> {
        let Some(first) = nodes.first() else {
            return Ok(());
        };

        let mut keys = nodes
            .iter()
            .filter_map(|node| node.directory.as_ref())
            .filter_map(DirLink::prefetch_key)
            .collect::<Vec<Key>>();
        keys.sort_unstable();
        keys.dedup();
        if keys.is_empty() {
            Ok(())
        } else {
            first.store.prefetch(keys)
        }
    }

    /// Look up logical children directly below this node.
    ///
    /// Results retain the input order. A node without a directory returns `None` for every name.
    pub fn lookup_children(
        &self,
        names: &[PathComponentBuf],
    ) -> Result<Vec<Option<TreeManifestReadNode>>> {
        let Some(directory) = &self.directory else {
            return Ok(names.iter().map(|_| None).collect());
        };

        names
            .iter()
            .map(|name| self.lookup_child(directory, name.as_path_component()))
            .collect()
    }

    /// List logical children directly below this node in sorted order.
    pub fn children(&self) -> Result<Vec<(PathComponentBuf, TreeManifestReadNode)>> {
        let Some(directory) = &self.directory else {
            return Ok(Vec::new());
        };

        let names = directory
            .links(&self.store)?
            .map(|(name, link)| {
                if link.is_leaf() {
                    match &self.path_translator {
                        Some(translator) => translator.decode_file_name(name.as_path_component()),
                        None => Ok(name.clone()),
                    }
                } else {
                    Ok(name.clone())
                }
            })
            .collect::<Result<BTreeSet<_>>>()?;
        let names = names.into_iter().collect::<Vec<_>>();
        let children = names
            .iter()
            .map(|name| self.lookup_child(directory, name.as_path_component()))
            .collect::<Result<Vec<_>>>()?;

        names
            .into_iter()
            .zip(children)
            .map(|(name, node)| {
                // Each name came from this directory. A miss means inconsistent path
                // translation, not an absent child.
                let node = node.ok_or_else(|| {
                    anyhow!(
                        "listed child '{name}' was not found during lookup from directory '{}'",
                        self.path
                    )
                })?;
                Ok((name, node))
            })
            .collect()
    }

    /// Look up one logical child in a known directory.
    ///
    /// The metadata comes from the encoded file entry when one exists, and from the directory entry
    /// otherwise.
    fn lookup_child(
        &self,
        directory: &DirLink,
        name: &PathComponent,
    ) -> Result<Option<TreeManifestReadNode>> {
        let raw_link = directory.lookup(&self.store, name)?;
        let mut logical_path = self.path.clone();
        logical_path.push(name);
        let mut storage_path = directory.path.clone();
        storage_path.push(name);
        let child_directory = raw_link
            .as_ref()
            .and_then(|link| DirLink::from_link(link, storage_path));

        let storage_name = match &self.path_translator {
            Some(translator) => translator.encode_file_name(name)?,
            None => name.to_owned(),
        };
        let content_link = if storage_name.as_path_component() == name {
            raw_link
        } else {
            directory.lookup(&self.store, storage_name.as_path_component())?
        };

        let metadata = content_link.as_ref().map(Link::to_fs_node).or_else(|| {
            child_directory
                .as_ref()
                .map(|directory| FsNodeMetadata::Directory(directory.hgid()))
        });

        Ok(metadata.map(|metadata| TreeManifestReadNode {
            store: self.store.clone(),
            path_translator: self.path_translator.clone(),
            path: logical_path,
            metadata,
            directory: child_directory,
        }))
    }
}

/// Diff several manifest pairs through one parallel breadth-first traversal.
///
/// Each result is tagged with its pair's index. All manifests must use the same backing store.
pub fn diff_manifests<'a, M>(
    pairs: Vec<(&'a TreeManifest, &'a TreeManifest, M)>,
) -> Items<(usize, DiffEntry), anyhow::Error>
where
    M: 'static + Matcher + Send + Sync,
{
    let (pairs, translators): (Vec<_>, Vec<_>) = pairs
        .into_iter()
        .map(|(left, right, matcher)| {
            (
                (left, right, left.maybe_wrap_matcher(matcher)),
                left.path_translator.clone(),
            )
        })
        .unzip();
    diff::diff(pairs).try_map_item(move |(index, entry): (usize, DiffEntry)| {
        let translator = translators
            .get(index)
            .ok_or_else(|| anyhow!("missing path translator for manifest pair {index}"))?;
        let path = match translator {
            Some(translator) => translator.decode_file(&entry.path)?,
            None => entry.path,
        };
        Ok((index, DiffEntry::new(path, entry.diff_type)))
    })
}

#[derive(Error, Debug)]
#[error("failure inserting '{path}' in manifest")]
pub struct InsertError {
    pub path: RepoPathBuf,
    pub file_metadata: FileMetadata,
    pub source: InsertErrorCause,
}

impl InsertError {
    pub fn new(path: RepoPathBuf, file_metadata: FileMetadata, source: InsertErrorCause) -> Self {
        Self {
            path,
            file_metadata,
            source,
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum InsertErrorCause {
    #[error("'{0}' is already a file")]
    ParentFileExists(RepoPathBuf),
    #[error("file path is already a directory")]
    DirectoryExistsForPath,
}

impl TreeManifest {
    /// Instantiates a tree manifest that was stored with the specified `HgId`
    pub fn durable(store: Arc<dyn TreeStore>, hgid: HgId) -> Self {
        TreeManifest {
            store: InnerStore::new(store),
            root: Link::durable(hgid),
            diff_grafts: Vec::new(),
            path_translator: None,
        }
    }

    /// Instantiates a new tree manifest with no history
    pub fn ephemeral(store: Arc<dyn TreeStore>) -> Self {
        TreeManifest {
            store: InnerStore::new(store),
            root: Link::ephemeral(),
            diff_grafts: Vec::new(),
            path_translator: None,
        }
    }

    /// Creates an empty manifest using the same backing store and path translation.
    pub fn empty(&self) -> Self {
        TreeManifest {
            store: self.store.clone(),
            root: Link::ephemeral(),
            diff_grafts: Vec::new(),
            path_translator: self.path_translator.clone(),
        }
    }

    /// Return a read node positioned at the manifest root.
    pub fn read_root(&self) -> Result<TreeManifestReadNode> {
        let directory = DirLink::from_root(&self.root)
            .ok_or_else(|| anyhow!("tree manifest root is a file"))?;
        let metadata = FsNodeMetadata::Directory(directory.hgid());
        Ok(TreeManifestReadNode {
            store: self.store.clone(),
            path_translator: self.path_translator.clone(),
            path: RepoPathBuf::new(),
            metadata,
            directory: Some(directory),
        })
    }

    pub fn set_path_translator(&mut self, translator: Arc<dyn PathTranslator>) {
        self.path_translator = Some(translator);
    }

    fn root_cursor<'a>(&'a self) -> DfsCursor<'a> {
        DfsCursor::new(&self.store, RepoPathBuf::new(), &self.root)
    }

    /// Files matching `matcher` in depth-first order: the entries of a directory in name order,
    /// with a subdirectory's contents visited before the entries after it. Trees are fetched in
    /// batches ahead of the traversal. With `start_after`, iteration continues as a walk that
    /// stopped at that path would: after a file, or inside a directory it had entered.
    pub fn files_dfs<'a, M: 'static + Matcher + Sync + Send>(
        &'a self,
        matcher: M,
        start_after: Option<&RepoPath>,
    ) -> Box<dyn Iterator<Item = Result<File>> + 'a> {
        let matcher = self.maybe_wrap_matcher(matcher);
        let start_after = match (&self.path_translator, start_after) {
            (Some(translator), Some(after)) => match translator.encode_file(after) {
                Ok(after) => Some(after),
                Err(err) => return Box::new(std::iter::once(Err(err))),
            },
            (None, Some(after)) => Some(after.to_owned()),
            (_, None) => None,
        };
        let files =
            iter::dfs_iter(self.store.clone(), &self.root, matcher, start_after).into_iter();
        Box::new(files.map(move |result| {
            let file = result?;
            Ok(File::new(self.maybe_decode_path(file.path)?, file.meta))
        }))
    }

    /// Returns manifest entries using inline results for small traversals and streamed batches
    /// after the traversal fans out.
    pub fn iter<M: 'static + Matcher + Sync + Send>(&self, matcher: M) -> iter::BfsItems {
        iter::bfs_iter(self.store.clone(), &[&self.root], matcher)
    }
}

impl TreeManifest {
    fn maybe_encode_path<'a>(&self, path: &'a RepoPath) -> Result<Cow<'a, RepoPath>> {
        match &self.path_translator {
            Some(t) => Ok(Cow::Owned(t.encode_file(path)?)),
            None => Ok(Cow::Borrowed(path)),
        }
    }

    fn maybe_wrap_matcher<M: 'static + Matcher + Sync + Send>(
        &self,
        matcher: M,
    ) -> Arc<dyn Matcher + Sync + Send> {
        match &self.path_translator {
            Some(t) => Arc::new(TranslatingMatcher {
                inner: matcher,
                translator: t.clone(),
            }),
            None => Arc::new(matcher),
        }
    }

    fn maybe_decode_path(&self, path: RepoPathBuf) -> Result<RepoPathBuf> {
        match &self.path_translator {
            Some(t) => t.decode_file(&path),
            None => Ok(path),
        }
    }
}

impl Manifest for TreeManifest {
    fn get(&self, path: &RepoPath) -> Result<Option<FsNodeMetadata>> {
        let result = self.get_link(path)?.map(|link| link.to_fs_node());
        Ok(result)
    }

    fn get_file(&self, file_path: &RepoPath) -> Result<Option<FileMetadata>> {
        let path = self.maybe_encode_path(file_path)?;
        let result = self.get(&path)?.and_then(|fs_hgid| match fs_hgid {
            FsNodeMetadata::File(file_metadata) => Some(file_metadata),
            FsNodeMetadata::Directory(_) => None,
        });
        Ok(result)
    }

    fn list(&self, path: &RepoPath) -> Result<List> {
        let directory = match self.get_link(path)? {
            None => return Ok(List::NotFound),
            Some(l) => match l.as_ref() {
                Leaf(_) => return Ok(List::File),
                Ephemeral(content) => content,
                Durable(entry) => entry.materialize_links(&self.store, path)?,
            },
        };

        let directory = directory
            .iter()
            .map(|(key, value)| (key.to_owned(), value.to_fs_node()))
            .collect();

        Ok(List::Directory(directory))
    }

    fn insert(&mut self, path: RepoPathBuf, file_metadata: FileMetadata) -> Result<()> {
        let path = self.maybe_encode_path(&path)?;
        let mut cursor = &self.root;
        let mut must_insert = false;
        for (parent, component) in path.parents().zip(path.components()) {
            let child = match cursor.as_ref() {
                Leaf(_) => Err(InsertError::new(
                    (*path).to_owned(), // TODO: get rid of clone (it is borrowed)
                    file_metadata,
                    InsertErrorCause::ParentFileExists(parent.to_owned()),
                ))?,
                Ephemeral(links) => links.get(component),
                Durable(entry) => {
                    let links = entry.materialize_links(&self.store, parent)?;
                    links.get(component)
                }
            };
            match child {
                None => {
                    must_insert = true;
                    break;
                }
                Some(link) => cursor = link,
            }
        }
        if !must_insert {
            match cursor.as_ref() {
                Leaf(existing_metadata) => {
                    if *existing_metadata == file_metadata {
                        return Ok(()); // nothing to do
                    }
                }
                Ephemeral(_) | Durable(_) => Err(InsertError::new(
                    (*path).to_owned(), // TODO: get rid of clone (it is borrowed later)
                    file_metadata,
                    InsertErrorCause::DirectoryExistsForPath,
                ))?,
            }
        }
        let (path_parent, last_component) = path.split_last_component().unwrap();
        let mut cursor = &mut self.root;
        // unwrap is fine because root would have been a directory
        for (parent, component) in path_parent.parents().zip(path_parent.components()) {
            cursor = cursor
                .mut_ephemeral_links(&self.store, parent)?
                .entry(component.to_owned())
                .or_insert_with(Link::ephemeral);
        }
        match cursor
            .mut_ephemeral_links(&self.store, path_parent)?
            .entry(last_component.to_owned())
        {
            Entry::Vacant(entry) => {
                entry.insert(Link::leaf(file_metadata));
            }
            Entry::Occupied(mut entry) => {
                if let Leaf(store_ref) = entry.get_mut().as_mut_ref()? {
                    *store_ref = file_metadata;
                } else {
                    unreachable!("Unexpected directory found while insert.");
                }
            }
        }
        Ok(())
    }

    fn remove(&mut self, path: &RepoPath) -> Result<Option<FileMetadata>> {
        // The return value lets us know if there are no more files in the subtree and we should be
        // removing it.
        fn do_remove<'a, I>(store: &InnerStore, cursor: &mut Link, iter: &mut I) -> Result<bool>
        where
            I: Iterator<Item = (&'a RepoPath, &'a PathComponent)>,
        {
            match iter.next() {
                None => {
                    if let Leaf(_) = cursor.as_ref() {
                        // We reached the file that we want to remove.
                        Ok(true)
                    } else {
                        unreachable!("Unexpected directory found while remove.");
                    }
                }
                Some((parent, component)) => {
                    // TODO: only convert to ephemeral if a removal took place
                    // We are navigating the tree down following parent directories
                    let ephemeral_links = cursor.mut_ephemeral_links(store, parent)?;
                    // When there is no `component` subtree we behave like the file was removed.
                    if let Some(link) = ephemeral_links.get_mut(component) {
                        if do_remove(store, link, iter)? {
                            // There are no files in the component subtree so we remove it.
                            ephemeral_links.remove(component);
                        }
                    }
                    Ok(ephemeral_links.is_empty())
                }
            }
        }
        let encoded = self.maybe_encode_path(path)?;
        if let Some(file_metadata) = self.get_file(path)? {
            do_remove(
                &self.store,
                &mut self.root,
                &mut encoded.parents().zip(encoded.components()),
            )?;
            Ok(Some(file_metadata))
        } else {
            Ok(None)
        }
    }

    /// Write dirty trees using specified format to disk. Return the root tree id.
    fn persist(&mut self, opts: PersistOpts<'_, Self>) -> Result<HgId> {
        let _ = TreeManifest::persist(self, opts.parents)?;

        match self.root.as_ref() {
            Leaf(_) | Ephemeral(_) => bail!("invalid root tree after flushing"),
            Durable(entry) => Ok(entry.hgid),
        }
    }

    #[tracing::instrument(skip_all)]
    fn files<'a, M: 'static + Matcher + Sync + Send>(
        &'a self,
        matcher: M,
    ) -> Box<dyn Iterator<Item = Result<File>> + 'a> {
        let matcher = self.maybe_wrap_matcher(matcher);
        let iter = iter::bfs_iter(self.store.clone(), &[&self.root], matcher);
        let files = iter.into_iter().filter_map(move |result| match result {
            Ok((path, FsNodeMetadata::File(metadata))) => {
                Some(self.maybe_decode_path(path).map(|p| File::new(p, metadata)))
            }
            Ok(_) => None,
            Err(err) => Some(Err(err)),
        });
        Box::new(files)
    }

    #[tracing::instrument(skip_all)]
    fn count_files<'a, M: 'static + Matcher + Sync + Send>(&'a self, matcher: M) -> Result<u64> {
        // PERF: the `bfs_iter()` can be optimized to avoid file path construction.
        iter::bfs_iter(self.store.clone(), &[&self.root], matcher)
            .into_iter()
            .try_fold(0, |acc, result| {
                let (_, metadata) = result?;
                if let FsNodeMetadata::File(_) = metadata {
                    Ok(acc + 1)
                } else {
                    Ok(acc)
                }
            })
    }

    /// Returns an iterator over all the directories that are present in the
    /// tree.
    ///
    /// Note: the matcher should be a prefix matcher, other kinds of matchers
    /// could be less effective than expected.
    #[tracing::instrument(skip_all)]
    fn dirs<'a, M: 'static + Matcher + Sync + Send>(
        &'a self,
        matcher: M,
    ) -> Box<dyn Iterator<Item = Result<Directory>> + 'a> {
        let dirs = iter::bfs_iter(self.store.clone(), &[&self.root], matcher)
            .into_iter()
            .filter_map(|result| match result {
                Ok((path, FsNodeMetadata::Directory(metadata))) => {
                    Some(Ok(Directory::new(path, metadata)))
                }
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            });
        Box::new(dirs)
    }

    fn diff<M: 'static + Matcher + Sync + Send>(
        &self,
        other: &Self,
        matcher: M,
    ) -> Result<Items<DiffEntry, anyhow::Error>> {
        Ok(diff_manifests(vec![(self, other, matcher)])
            .try_map_item(|(_, entry): (usize, DiffEntry)| Ok(entry)))
    }

    fn modified_dirs<'a, M: 'static + Matcher + Sync + Send>(
        &'a self,
        other: &'a Self,
        matcher: M,
    ) -> Result<Box<dyn Iterator<Item = Result<DirDiffEntry>> + 'a>> {
        let matcher: DynMatcher = Arc::new(matcher);
        Ok(Box::new(
            diff::diff(vec![(self, other, matcher)])
                .try_map_item(|(_, entry): (usize, DirDiffEntry)| Ok(entry))
                .into_iter(),
        ))
    }
}

pub fn prefetch_diff<'a>(
    pairs: impl IntoIterator<Item = (&'a TreeManifest, &'a TreeManifest)>,
) -> Result<()> {
    let matcher: DynMatcher = Arc::new(AlwaysMatcher::new());
    let pairs = pairs
        .into_iter()
        .map(|(left, right)| (left, right, Arc::clone(&matcher)))
        .collect();
    for entry in diff::diff::<DiffEntry>(pairs) {
        let _ = entry?;
    }
    Ok(())
}

impl fmt::Debug for TreeManifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn write_indent(f: &mut fmt::Formatter<'_>, indent: usize) -> fmt::Result {
            write!(f, "{}", str::repeat("| ", indent))?;
            Ok(())
        }
        fn write_children(
            f: &mut fmt::Formatter<'_>,
            children: &BTreeMap<PathComponentBuf, Link>,
            indent: usize,
        ) -> fmt::Result {
            for (component, link) in children {
                write_indent(f, indent)?;
                write!(f, "{component} ")?;
                write_links(f, link, indent + 1)?;
            }
            Ok(())
        }
        fn write_links(f: &mut fmt::Formatter<'_>, link: &Link, indent: usize) -> fmt::Result {
            match link.as_ref() {
                Leaf(metadata) => {
                    write!(f, "(File, {}, {:?})\n", metadata.hgid, metadata.file_type)
                }
                Ephemeral(children) => {
                    write!(f, "(Ephemeral)\n")?;
                    write_children(f, children, indent)
                }
                Durable(entry) => {
                    write!(f, "(Durable, {})\n", entry.hgid)?;
                    match entry.links.get() {
                        Some(MaybeLinks::Links(children)) => write_children(f, children, indent),
                        _ => Ok(()),
                    }
                }
            }
        }
        write!(f, "Root ")?;
        write_links(f, &self.root, 1)
    }
}

/// Trait for tracking parent trees during finalization.
/// This abstracts the parent-matching logic so `finalize_trees` can work
/// with Hg parent history or without any parents.
trait ParentTreeTracker {
    type ActiveParents;

    /// Returns the initial set of active parents for the root.
    fn initial_active(&self) -> Self::ActiveParents;

    /// Check if a durable node with `hgid` is unchanged (present in a parent).
    fn is_unchanged(&self, hgid: &HgId, parent_tree_nodes: &[HgId]) -> bool;

    /// Return the distinct parent tree node HgIds for the current active set.
    fn parent_tree_nodes(&self, active: &Self::ActiveParents) -> Result<Vec<HgId>>;

    /// Advance parent cursors past the current directory entry.
    fn advance(&mut self, active: &Self::ActiveParents) -> Result<()>;

    /// Narrow the active parents down to those that have a subdirectory at `path`.
    fn for_subdirectory(
        &mut self,
        active: &Self::ActiveParents,
        path: &RepoPath,
    ) -> Result<Self::ActiveParents>;

    /// Get the p1 parent's directory at the current position.
    fn p1_entry(&self, active: &Self::ActiveParents) -> Option<Arc<DurableEntry>>;
}

/// No parent tracking — used for content-addressed stores (Git) where a
/// durable node's identity is fully determined by its content.
struct NoParents;

impl ParentTreeTracker for NoParents {
    type ActiveParents = ();

    fn initial_active(&self) {}
    fn is_unchanged(&self, _hgid: &HgId, _parent_tree_nodes: &[HgId]) -> bool {
        // Durable nodes in content-addressed stores are always valid.
        true
    }
    fn parent_tree_nodes(&self, _active: &()) -> Result<Vec<HgId>> {
        Ok(Vec::new())
    }
    fn advance(&mut self, _active: &()) -> Result<()> {
        Ok(())
    }
    fn for_subdirectory(&mut self, _active: &(), _path: &RepoPath) -> Result<()> {
        Ok(())
    }
    fn p1_entry(&self, _active: &()) -> Option<Arc<DurableEntry>> {
        None
    }
}

/// Hg parent tracking — uses `DfsCursor`s to walk parent trees in lockstep
/// with the working tree, identifying which directories are unchanged.
struct HgParents<'a> {
    cursors: Vec<DfsCursor<'a>>,
}

impl<'a> HgParents<'a> {
    fn new(parent_trees: &[&'a TreeManifest]) -> Result<Self> {
        let mut cursors: Vec<DfsCursor<'a>> =
            parent_trees.iter().map(|v| v.root_cursor()).collect();
        // The first node after step is the root directory. The walk logic
        // expects cursors to be pointing to the underlying link.
        for cursor in cursors.iter_mut() {
            match cursor.step() {
                Step::Success | Step::End => {}
                Step::Err(err) => return Err(err),
            }
        }
        Ok(Self { cursors })
    }
}

impl<'a> ParentTreeTracker for HgParents<'a> {
    /// Indices into `self.cursors` that are active for the current subtree.
    type ActiveParents = Vec<usize>;

    fn initial_active(&self) -> Vec<usize> {
        (0..self.cursors.len()).collect()
    }

    fn is_unchanged(&self, hgid: &HgId, parent_tree_nodes: &[HgId]) -> bool {
        parent_tree_nodes.contains(hgid)
    }

    fn parent_tree_nodes(&self, active: &Vec<usize>) -> Result<Vec<HgId>> {
        let mut parent_nodes = Vec::with_capacity(active.len());
        for id in active {
            let cursor = &self.cursors[*id];
            let hgid = match cursor.link().as_ref() {
                Leaf(_) | Ephemeral(_) => unreachable!(),
                Durable(entry) => entry.hgid,
            };
            if !parent_nodes.contains(&hgid) {
                parent_nodes.push(hgid);
            }
        }
        Ok(parent_nodes)
    }

    fn advance(&mut self, active: &Vec<usize>) -> Result<()> {
        for id in active {
            let cursor = &mut self.cursors[*id];
            match cursor.step() {
                Step::Success | Step::End => {}
                Step::Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    fn for_subdirectory(&mut self, active: &Vec<usize>, path: &RepoPath) -> Result<Vec<usize>> {
        let mut result = Vec::new();
        for id in active.iter() {
            let cursor = &mut self.cursors[*id];
            while !cursor.finished() && cursor.path() < path {
                cursor.skip_subtree();
                match cursor.step() {
                    Step::Success | Step::End => {}
                    Step::Err(err) => return Err(err),
                }
            }
            if !cursor.finished() && cursor.path() == path {
                match cursor.link().as_ref() {
                    Leaf(_) => {} // files and directories don't share history
                    Durable(_) => result.push(*id),
                    Ephemeral(_) => {
                        panic!("Found ephemeral parent when finalizing manifest.")
                    }
                }
            }
        }
        Ok(result)
    }

    fn p1_entry(&self, active: &Vec<usize>) -> Option<Arc<DurableEntry>> {
        let &first_idx = active.first()?;
        if first_idx != 0 {
            return None;
        }
        match self.cursors[0].link().as_ref() {
            Durable(entry) => Some(entry.clone()),
            _ => None,
        }
    }
}

/// Recursive tree-walking logic shared by `finalize()`.
/// Walks the tree, computing hgids for each directory node.
fn finalize_trees<P: ParentTreeTracker>(
    store: &InnerStore,
    path: &mut RepoPathBuf,
    link: &mut Link,
    format: SerializationFormat,
    load_parent_tree_entry: bool,
    tracker: &mut P,
    active: P::ActiveParents,
) -> Result<(HgId, store::Flag)> {
    let parent_tree_nodes = tracker.parent_tree_nodes(&active)?;
    if let Durable(entry) = link.as_ref() {
        if tracker.is_unchanged(&entry.hgid, &parent_tree_nodes) {
            return Ok((entry.hgid, store::Flag::Directory));
        }
    }
    let p1_entry = tracker.p1_entry(&active);
    tracker.advance(&active)?;
    if let Leaf(file_metadata) = link.as_ref() {
        return Ok((
            file_metadata.hgid,
            store::Flag::File(file_metadata.file_type.clone()),
        ));
    }
    // TODO: This code is also used on durable nodes for the purpose of generating
    // a list of entries to insert in the local store. For those cases we don't
    // need to convert to Ephemeral instead only verify the hash.
    let links = link.mut_ephemeral_links(store, path)?;
    let mut elements = Vec::with_capacity(links.len());
    for (component, child_link) in links.iter_mut() {
        path.push(component.as_path_component());
        let child_active = tracker.for_subdirectory(&active, path)?;
        let (hgid, flag) = finalize_trees(
            store,
            path,
            child_link,
            format,
            load_parent_tree_entry,
            tracker,
            child_active,
        )?;
        path.pop();
        let element = store::Element::new(component.clone(), hgid, flag);
        elements.push(element);
    }
    let entry = store::Entry::from_elements(elements, format);

    // Read after `advance()`, which loads an unloaded p1 directory together with its tree entry.
    let p1_tree_entry = match &p1_entry {
        Some(p1) if load_parent_tree_entry => {
            Some(p1.get_or_load_tree_entry(store, path).with_context(|| {
                format!("failed to load parent tree '{path}' to migrate ACL children")
            })?)
        }
        Some(p1) => p1.get_tree_entry(),
        None => None,
    };
    let acl_children_indices = migrate_acl_children(p1_tree_entry, &entry);

    let cell = OnceLock::new();
    // TODO: remove clone
    cell.set(MaybeLinks::Links(links.clone())).unwrap();

    let hgid = store.insert_entry(path, entry, parent_tree_nodes, acl_children_indices)?;

    let durable_entry = DurableEntry::with_links(hgid, cell);
    let inner = Arc::new(durable_entry);
    *link = Link::new(Durable(inner));

    Ok((hgid, store::Flag::Directory))
}

/// Migrate `acl_children_indices` from the old (p1) parent tree to the new tree.
/// Returns indices of directory children in the new tree whose HgIds match
/// ACL-flagged children in the old parent. Returns `None` if there is nothing
/// to migrate (no parent, or parent has no ACL children).
fn migrate_acl_children(
    p1_tree_entry: Option<&Arc<dyn storemodel::TreeEntry>>,
    new_entry: &store::Entry,
) -> Option<Vec<u32>> {
    let old_tree = p1_tree_entry?;
    let old_acl_children = match old_tree.children_with_acls() {
        Ok(children) => children,
        Err(err) => {
            tracing::warn!(?err, "failed to read ACL children from parent tree");
            return None;
        }
    };
    if old_acl_children.is_empty() {
        return None;
    }

    let acl_hgids: HashSet<HgId> = old_acl_children.into_iter().map(|(_, hgid)| hgid).collect();

    let mut indices = Vec::new();
    for (idx, elem) in new_entry.elements().enumerate() {
        match elem {
            Ok(elem) => {
                if matches!(elem.flag, store::Flag::Directory) && acl_hgids.contains(&elem.hgid) {
                    indices.push(idx as u32);
                }
            }
            Err(err) => {
                tracing::warn!(
                    ?err,
                    idx,
                    "failed to parse tree element during ACL migration"
                );
            }
        }
    }

    if indices.is_empty() {
        None
    } else {
        Some(indices)
    }
}

impl TreeManifest {
    /// Persist dirty trees to the store. Returns the root tree id.
    /// For Hg format, parent trees are used to compute tree hgids and
    /// parent info is written to the history store.
    pub fn persist(&mut self, parent_trees: &[&TreeManifest]) -> Result<HgId> {
        self.persist_with(parent_trees, config::load_parent_tree_entry())
    }

    fn persist_with(
        &mut self,
        parent_trees: &[&TreeManifest],
        load_parent_tree_entry: bool,
    ) -> Result<HgId> {
        let mut path = RepoPathBuf::new();
        let format = self.store.format();
        let (hgid, _) = match format {
            SerializationFormat::Hg => {
                let mut tracker = HgParents::new(parent_trees)?;
                let active = tracker.initial_active();
                finalize_trees(
                    &self.store,
                    &mut path,
                    &mut self.root,
                    format,
                    load_parent_tree_entry,
                    &mut tracker,
                    active,
                )?
            }
            SerializationFormat::Git => {
                let mut tracker = NoParents;
                finalize_trees(
                    &self.store,
                    &mut path,
                    &mut self.root,
                    format,
                    load_parent_tree_entry,
                    &mut tracker,
                    (),
                )?
            }
        };
        Ok(hgid)
    }

    /// Insert `other[other_path]` into `self[path]`. If `path` is already in `self`,
    /// it is replaced, not merged or overlaid. Any conflicting entries in `self` are
    /// overwritten in order to insert into `path`. `other_path` can be a Leaf, but if so cannot be
    /// inserted as the root of `self`. `other[other_path]` is deep copied so it remains mutable. If
    /// `other[other_path]` does not exist, an empty directory is inserted at `path`.
    pub fn graft(&mut self, path: &RepoPath, other: &Self, other_path: &RepoPath) -> Result<()> {
        let other_link = other.get_link(other_path)?;

        if path.is_empty() && other_link.is_some_and(Link::is_leaf) {
            bail!("can't graft leaf node to root of tree");
        }

        let mut cursor = &mut self.root;
        for (parent, component) in path.parents().zip(path.components()) {
            let links = cursor.mut_ephemeral_links(&self.store, parent)?;
            cursor = match links.entry(component.to_owned()) {
                Entry::Vacant(e) => e.insert(Link::ephemeral()),
                Entry::Occupied(o) => {
                    let link = o.into_mut();
                    if link.is_leaf() {
                        // Path conflict - replace file with a directory.
                        *link = Link::ephemeral();
                    }
                    link
                }
            };
        }

        // Deep copy `other_link` so it, and our grafted copy, remain mutable.
        *cursor = other_link.map_or_else(Link::ephemeral, |other| other.clone());

        Ok(())
    }

    /// Return a new tree with registered grafts applied. If there are no grafts, a
    /// shallow copy of `self` is returned. If we have no grafts but `other` does (`other`
    /// is the other side of the diff operation), return a new tree with only the "to"
    /// side of `other`'s grafts present.
    ///
    /// For example in "sl graft -r A --from-path foo --to-path bar", we apply foo->bar
    /// graft to A yielding a tree with just "bar", and we apply bar->bar graft to the
    /// wdir manifest, yield a tree with just "bar". This way we strip out parts of the
    /// manifest that aren't part of the graft.
    pub fn apply_diff_grafts(&self, other: &Self) -> Result<Self> {
        if self.diff_grafts.is_empty() && other.diff_grafts.is_empty() {
            // No grafts to apply - return a shallow copy of ourself.
            return Ok(Self {
                store: self.store.clone(),
                root: self.root.thread_copy(),
                diff_grafts: Vec::new(),
                path_translator: self.path_translator.clone(),
            });
        }

        let mut grafted = Self {
            store: self.store.clone(),
            root: Link::ephemeral(),
            diff_grafts: Vec::new(),
            path_translator: self.path_translator.clone(),
        };

        if self.diff_grafts.is_empty() {
            for (_, to) in other.diff_grafts.iter() {
                tracing::info!(%to, "applying self diff graft");
                grafted.graft(to, self, to)?;
            }
        } else {
            for (from, to) in self.diff_grafts.iter() {
                tracing::info!(%from, %to, "applying diff graft");
                grafted.graft(to, self, from)?;
            }
        }
        Ok(grafted)
    }

    /// Register a graft to take effect during `diff` operations.
    /// This allows temporarily moving tree nodes around just for the diff.
    /// See `ungrafted_path` for mapping the diff result back to the original path.
    /// Returns an error if `to` overlaps with existing graft destination.
    pub fn register_diff_graft(&mut self, from: &RepoPath, to: &RepoPath) -> Result<()> {
        for (_, existing) in self.diff_grafts.iter() {
            if to.starts_with(existing, true) || existing.starts_with(to, true) {
                bail!("overlapping graft destinations {existing} and {to}");
            }
        }
        self.diff_grafts.push((from.to_owned(), to.to_owned()));
        Ok(())
    }

    /// Get registered grafts.
    pub fn diff_grafts(&self) -> &[(RepoPathBuf, RepoPathBuf)] {
        self.diff_grafts.as_slice()
    }

    /// Map a grafted path back to this manifest's original path.
    /// This is used in conjunction with `graft_for_diff` to translate a grafted path in the
    /// diff result back to the original path, if any.
    pub fn ungrafted_path(&self, path: &RepoPath) -> Option<RepoPathBuf> {
        for (from, to) in self.diff_grafts.iter().rev() {
            if let Some(suffix) = path.strip_prefix(to, true) {
                if from == to {
                    return None;
                } else if suffix.is_empty() {
                    return Some(from.clone());
                } else {
                    return Some(from.join(suffix));
                }
            }
        }
        None
    }

    /// Turn a regular path into the equivalent paths after applying registered grafts.
    /// This is the inverse of ungrafted_path, but is one-to-many in this direction.
    ///
    /// With grafts of `foo->bar, foo->baz`, this turns `foo/file` into `[bar/file, baz/file]`.
    pub fn grafted_paths(&self, path: &RepoPath) -> Vec<RepoPathBuf> {
        // NB: we can assume we don't have overlappying "to"s since we validate in
        // register_diff_graft.
        self.diff_grafts
            .iter()
            .filter_map(|(from, to)| {
                if let Some(suffix) = path.strip_prefix(from, true) {
                    if from == to {
                        None
                    } else if suffix.is_empty() {
                        Some(to.clone())
                    } else {
                        Some(to.join(suffix))
                    }
                } else {
                    None
                }
            })
            .collect()
    }

    /// Turn the regular local_path into the equivalent grafted path, inferring which
    /// graft to use based on dest_path (making it a one-to-one mapping).
    ///
    /// With grafts of `foo->bar, foo->baz`, this turns `(foo/file, bar/something)` into `bar/file`.
    pub fn grafted_path(&self, local_path: &RepoPath, dest_path: &RepoPath) -> Option<RepoPathBuf> {
        for (from, to) in self.diff_grafts.iter().rev() {
            if !dest_path.starts_with(to, true) {
                continue;
            }

            // From here on, return unconditionally since `dest_path` can only match one
            // `to` (since `to`s cannot overlap).

            if from == to {
                return None;
            }

            return match local_path.strip_prefix(from, true) {
                None => None,
                Some(suffix) => {
                    if suffix.is_empty() {
                        Some(to.clone())
                    } else {
                        Some(to.join(suffix))
                    }
                }
            };
        }
        None
    }

    /// Turn a regular path into the containing grafts after applying registered grafts.
    ///
    /// With grafts `foo->bar, foo->baz`, this turns path `foo/file` into `[bar, baz]`.
    pub fn grafted_dests(&self, path: &RepoPath) -> Vec<RepoPathBuf> {
        // NB: we can assume we don't have overlappying "to"s since we validate in
        // register_diff_graft.
        self.diff_grafts
            .iter()
            .filter_map(|(from, to)| {
                if path.starts_with(from, true) && from != to {
                    Some(to.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Report whether this manifest has any registered diff grafts.
    pub fn has_grafts(&self) -> bool {
        !self.diff_grafts.is_empty()
    }

    /// Reports whether this manifest has been modified (in-memory).
    pub fn is_dirty(&self) -> bool {
        self.root.is_ephemeral() || !self.diff_grafts.is_empty()
    }

    fn get_link(&self, path: &RepoPath) -> Result<Option<&Link>> {
        let mut cursor = &self.root;
        for (parent, component) in path.parents().zip(path.components()) {
            let child = match cursor.as_ref() {
                Leaf(_) => return Ok(None),
                Ephemeral(links) => links.get(component),
                Durable(entry) => {
                    let links = entry.materialize_links(&self.store, parent)?;
                    links.get(component)
                }
            };
            match child {
                None => return Ok(None),
                Some(link) => cursor = link,
            }
        }
        Ok(Some(cursor))
    }
}

/// Translates between user-facing paths and manifest storage paths.
/// The trait should ONLY be used by grepo and tests.
pub trait PathTranslator: Send + Sync + fmt::Debug {
    /// Encode a user-facing file path to the storage path.
    fn encode_file(&self, path: &RepoPath) -> Result<RepoPathBuf>;
    /// Decode a storage file path to the user-facing path.
    fn decode_file(&self, path: &RepoPath) -> Result<RepoPathBuf>;
    /// Encode a user-facing file name to the storage name.
    fn encode_file_name(&self, name: &PathComponent) -> Result<PathComponentBuf>;
    /// Decode a storage file name to the user-facing name.
    fn decode_file_name(&self, name: &PathComponent) -> Result<PathComponentBuf>;
}

/// Matcher adapter that decodes encoded file paths before matching
/// against the user's original pattern. Directory paths pass through
/// unchanged.
struct TranslatingMatcher<M> {
    inner: M,
    translator: Arc<dyn PathTranslator>,
}

impl<M: Matcher> Matcher for TranslatingMatcher<M> {
    fn matches_file(&self, path: &RepoPath) -> Result<bool> {
        let maybe_decoded = self.translator.decode_file(path)?;
        self.inner.matches_file(&maybe_decoded)
    }

    fn matches_directory(&self, path: &RepoPath) -> Result<pathmatcher::DirectoryMatch> {
        self.inner.matches_directory(path)
    }
}

pub trait TreeResolver: Send + Sync + 'static {
    fn get(&self, commit_id: &HgId) -> Result<TreeManifest>;
    fn get_root_id(&self, commit_id: &HgId) -> Result<HgId>;
    fn get_by_root_id(&self, root_id: &HgId) -> Result<TreeManifest>;

    /// Resolve a batch of root tree IDs while preserving input order.
    fn get_by_root_ids(&self, root_ids: &[HgId]) -> Result<Vec<TreeManifest>> {
        root_ids
            .iter()
            .map(|root_id| self.get_by_root_id(root_id))
            .collect()
    }
}

/// The purpose of this function is to provide compatible behavior with the C++ implementation
/// of the treemanifest. This function is problematic because it goes through abstraction
/// boundaries and is built with the assumption that the storage format is the same as the
/// in memory format that is the same as the wire format.
///
/// This function returns the nodes that need to be sent over the wire for a subtree of the
/// manifest to be fully hydrated. The subtree is represented by `path` and `hgid`. The data
/// that is present locally by the client is represented by `other_nodes`.
///
/// It is undefined what this function will do when called with a path that points to a file
/// or with nodes that don't make sense.
// NOTE: The implementation is currently custom. Consider converting the code to use Cursor.
// The suggestion received in code review was also to consider making the return type more
// simple (RepoPath, HgId) and letting the call sites deal with the Bytes.
pub fn compat_subtree_diff(
    store: Arc<dyn TreeStore>,
    path: &RepoPath,
    hgid: HgId,
    mut other_nodes: Vec<HgId>,
    depth: i32,
) -> Result<Vec<(RepoPathBuf, HgId, Vec<HgId>, Bytes)>> {
    struct State {
        store: InnerStore,
        path: RepoPathBuf,
        result: Vec<(RepoPathBuf, HgId, Vec<HgId>, Bytes)>,
        depth_remaining: i32,
    }
    impl State {
        fn work(&mut self, hgid: HgId, other_nodes: Vec<HgId>) -> Result<()> {
            let entry = self.store.get_entry(&self.path, hgid)?;

            if self.depth_remaining > 0 {
                // TODO: optimize "other_nodes" construction
                // We use BTreeMap for convenience only, it is more efficient to use an array since
                // the entries are already sorted.
                let mut others_map = BTreeMap::new();
                for other_hgid in other_nodes.clone() {
                    let other_entry = self.store.get_entry(&self.path, other_hgid)?;
                    for other_element_result in other_entry.elements() {
                        let other_element = other_element_result?;
                        if other_element.flag == store::Flag::Directory {
                            others_map
                                .entry(other_element.component)
                                .or_insert(vec![])
                                .push(other_element.hgid);
                        }
                    }
                }
                for element_result in entry.elements() {
                    let element = element_result?;
                    if element.flag != store::Flag::Directory {
                        continue;
                    }
                    let mut others = others_map
                        .remove(&element.component)
                        .unwrap_or_else(std::vec::Vec::new);
                    if others.contains(&element.hgid) {
                        continue;
                    }
                    others.dedup();
                    self.path.push(element.component.as_path_component());
                    self.depth_remaining -= 1;
                    self.work(element.hgid, others)?;
                    self.depth_remaining += 1;
                    self.path.pop();
                }
            }
            // NOTE: order in the result set matters for a lot of the integration tests
            self.result
                .push((self.path.clone(), hgid, other_nodes, entry.to_bytes()));
            Ok(())
        }
    }

    if other_nodes.contains(&hgid) {
        return Ok(vec![]);
    }
    other_nodes.dedup();

    let mut state = State {
        store: InnerStore::new(store),
        path: path.to_owned(),
        result: vec![],
        depth_remaining: depth - 1,
    };
    state.work(hgid, other_nodes)?;
    Ok(state.result)
}

pub fn apply_diff_grafts(
    m1: &TreeManifest,
    m2: &TreeManifest,
) -> Result<(TreeManifest, TreeManifest)> {
    Ok((m1.apply_diff_grafts(m2)?, m2.apply_diff_grafts(m1)?))
}

/// Prefetch everything under given tree nodes, filtered by the given matcher.
///
/// Server requests are only made for trees not already available locally.
/// Assuming nothing is available locally, prefetch must make O(depth) serial
/// round trips to the server.
pub fn prefetch(
    store: Arc<dyn TreeStore>,
    mf_nodes: &[HgId],
    matcher: impl 'static + Matcher + Sync + Send,
) -> Result<()> {
    let links: Vec<Link> = mf_nodes.iter().map(|id| Link::durable(*id)).collect();
    for node in iter::bfs_iter(InnerStore::new(store), &links, matcher) {
        let _ = node?;
    }
    Ok(())
}

pub fn init() {
    crate::factory_impls::setup_basic_tree_parser_constructor();
    crate::factory_impls::setup_basic_tree_serializer_constructor();
}

#[cfg(test)]
dev_logger::init!();

#[cfg(test)]
mod tests {
    use manifest::DiffType;
    use manifest::FileType;
    use manifest::testutil::*;
    use pathmatcher::AlwaysMatcher;
    use pathmatcher::TreeMatcher;
    use store::Element;
    use storemodel::InsertOpts;
    use storemodel::Kind;
    use types::testutil::*;

    use self::testutil::*;
    use super::*;

    trait TestInsert {
        fn insert(&self, path: &RepoPath, hgid: HgId, data: Bytes) -> Result<()>;
    }

    impl<T: TreeStore> TestInsert for T {
        fn insert(&self, path: &RepoPath, hgid: HgId, data: Bytes) -> Result<()> {
            let opts = InsertOpts {
                kind: Kind::Tree,
                forced_id: Some(Box::new(hgid)),
                ..Default::default()
            };
            self.insert_data(opts, path, data.into())?;
            Ok(())
        }
    }

    impl store::Entry {
        fn from_elements_hg(elements: Vec<Element>) -> Self {
            Self::from_elements(elements, SerializationFormat::Hg)
        }
    }
    fn store_element(path: &str, hex: &str, flag: store::Flag) -> store::Element {
        store::Element::new(path_component_buf(path), hgid(hex), flag)
    }

    #[derive(Debug)]
    struct SuffixPathTranslator;

    impl PathTranslator for SuffixPathTranslator {
        fn encode_file(&self, path: &RepoPath) -> Result<RepoPathBuf> {
            Ok(RepoPathBuf::from_string(format!("{}\x7f", path.as_str()))?)
        }

        fn decode_file(&self, path: &RepoPath) -> Result<RepoPathBuf> {
            let decoded = path.as_str().strip_suffix('\x7f').unwrap_or(path.as_str());
            Ok(RepoPathBuf::from_string(decoded.to_owned())?)
        }

        fn encode_file_name(&self, name: &PathComponent) -> Result<PathComponentBuf> {
            Ok(PathComponentBuf::from_string(format!(
                "{}\x7f",
                name.as_str()
            ))?)
        }

        fn decode_file_name(&self, name: &PathComponent) -> Result<PathComponentBuf> {
            let decoded = name.as_str().strip_suffix('\x7f').unwrap_or(name.as_str());
            Ok(PathComponentBuf::from_string(decoded.to_owned())?)
        }
    }

    #[derive(Debug)]
    struct InconsistentPathTranslator;

    impl PathTranslator for InconsistentPathTranslator {
        fn encode_file(&self, path: &RepoPath) -> Result<RepoPathBuf> {
            Ok(path.to_owned())
        }

        fn decode_file(&self, path: &RepoPath) -> Result<RepoPathBuf> {
            Ok(path.to_owned())
        }

        fn encode_file_name(&self, _name: &PathComponent) -> Result<PathComponentBuf> {
            Ok(path_component_buf("missing"))
        }

        fn decode_file_name(&self, _name: &PathComponent) -> Result<PathComponentBuf> {
            Ok(path_component_buf("logical"))
        }
    }

    #[test]
    fn test_read_node_retains_directory_position_for_overlapping_path() -> Result<()> {
        let store = Arc::new(TestStore::new());
        let mut source = TreeManifest::ephemeral(store.clone());
        source.set_path_translator(Arc::new(SuffixPathTranslator));
        source.insert(repo_path_buf("a"), make_meta("10"))?;
        source.insert(repo_path_buf("a/b"), make_meta("20"))?;
        let root_hgid = source.persist(&[])?;

        let mut tree = TreeManifest::durable(store, root_hgid);
        tree.set_path_translator(Arc::new(SuffixPathTranslator));
        let root = tree.read_root()?;
        let children = root.children()?;

        assert_eq!(children.len(), 1);
        assert_eq!(children[0].0, path_component_buf("a"));
        let a = &children[0].1;
        assert_eq!(a.path(), repo_path("a"));
        assert_eq!(a.metadata(), FsNodeMetadata::File(make_meta("10")));
        assert!(a.is_directory());
        assert!(a.directory_hgid().is_some());

        let children =
            a.lookup_children(&[path_component_buf("b"), path_component_buf("missing")])?;
        let b = children[0].as_ref().expect("a/b should exist");
        assert_eq!(b.metadata(), FsNodeMetadata::File(make_meta("20")));
        assert!(!b.is_directory());
        assert!(children[1].is_none());

        let below_file = b.lookup_children(&[path_component_buf("child")])?;
        assert!(below_file[0].is_none());
        Ok(())
    }

    #[test]
    fn test_read_node_prefetch_deduplicates_shared_directories() -> Result<()> {
        let store = Arc::new(TestStore::new());
        let mut source = TreeManifest::ephemeral(store.clone());
        source.insert(repo_path_buf("a/b"), make_meta("10"))?;
        let root_hgid = source.persist(&[])?;

        let left = TreeManifest::durable(store.clone(), root_hgid).read_root()?;
        let right = TreeManifest::durable(store.clone(), root_hgid).read_root()?;
        TreeManifestReadNode::prefetch(&[left.clone(), right])?;
        assert_eq!(
            store.prefetches(),
            vec![vec![Key::new(RepoPathBuf::new(), root_hgid)]],
            "two manifests with one root should prefetch the root once"
        );

        let a = left
            .lookup_children(&[path_component_buf("a")])?
            .pop()
            .flatten()
            .expect("a should exist");
        let a_hgid = a.directory_hgid().expect("a should be a durable directory");
        TreeManifestReadNode::prefetch(&[left, a.clone()])?;
        assert_eq!(
            store.prefetches().last(),
            Some(&vec![Key::new(repo_path_buf("a"), a_hgid)]),
            "the loaded root should be skipped"
        );
        Ok(())
    }

    #[test]
    fn test_read_node_children_rejects_inconsistent_translation() -> Result<()> {
        let store = Arc::new(TestStore::new());
        let mut source = TreeManifest::ephemeral(store.clone());
        source.insert(repo_path_buf("stored"), make_meta("10"))?;
        let root_hgid = source.persist(&[])?;

        let mut tree = TreeManifest::durable(store, root_hgid);
        tree.set_path_translator(Arc::new(InconsistentPathTranslator));
        let error = tree
            .read_root()?
            .children()
            .expect_err("inconsistent path translation should fail");

        assert_eq!(
            error.to_string(),
            "listed child 'logical' was not found during lookup from directory ''"
        );
        Ok(())
    }

    fn get_hgid(tree: &TreeManifest, path: &RepoPath) -> HgId {
        match tree.get_link(path).unwrap().unwrap().as_ref() {
            Leaf(file_metadata) => file_metadata.hgid,
            Durable(entry) => entry.hgid,
            Ephemeral(_) => {
                panic!("Asked for hgid on path {path} but found ephemeral hgid.")
            }
        }
    }

    #[test]
    fn test_insert() {
        let mut tree = TreeManifest::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("foo/bar"), make_meta("10"))
            .unwrap();
        assert_eq!(
            tree.get_file(repo_path("foo/bar")).unwrap(),
            Some(make_meta("10"))
        );
        assert_eq!(tree.get_file(repo_path("baz")).unwrap(), None);

        tree.insert(repo_path_buf("baz"), make_meta("20")).unwrap();
        assert_eq!(
            tree.get_file(repo_path("foo/bar")).unwrap(),
            Some(make_meta("10"))
        );
        assert_eq!(
            tree.get_file(repo_path("baz")).unwrap(),
            Some(make_meta("20"))
        );

        tree.insert(repo_path_buf("foo/bat"), make_meta("30"))
            .unwrap();
        assert_eq!(
            tree.get_file(repo_path("foo/bat")).unwrap(),
            Some(make_meta("30"))
        );
        assert_eq!(
            tree.get_file(repo_path("foo/bar")).unwrap(),
            Some(make_meta("10"))
        );
        assert_eq!(
            tree.get_file(repo_path("baz")).unwrap(),
            Some(make_meta("20"))
        );

        assert_eq!(
            tree.insert(repo_path_buf("foo/bar/error"), make_meta("40"))
                .unwrap_err()
                .chain()
                .map(|e| format!("{e}"))
                .collect::<Vec<_>>(),
            vec![
                "failure inserting 'foo/bar/error' in manifest",
                "\'foo/bar\' is already a file",
            ],
        );
        assert_eq!(
            tree.insert(repo_path_buf("foo"), make_meta("50"))
                .unwrap_err()
                .chain()
                .map(|e| format!("{e}"))
                .collect::<Vec<_>>(),
            vec![
                "failure inserting 'foo' in manifest",
                "file path is already a directory",
            ],
        );
    }

    #[test]
    fn test_contains_file() {
        let mut tree = TreeManifest::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("foo/bar"), make_meta("10"))
            .unwrap();
        assert!(tree.contains_file(repo_path("foo/bar")).unwrap());
        assert!(!tree.contains_file(repo_path("foo")).unwrap());
        assert!(!tree.contains_file(repo_path("baz")).unwrap());
    }

    #[test]
    fn test_durable_link() {
        let store = TestStore::new();
        let root_entry = store::Entry::from_elements_hg(vec![
            store_element("foo", "10", store::Flag::Directory),
            store_element("baz", "20", store::Flag::File(FileType::Regular)),
        ]);
        store
            .insert(RepoPath::empty(), hgid("1"), root_entry.to_bytes())
            .unwrap();
        let foo_entry = store::Entry::from_elements_hg(vec![store_element(
            "bar",
            "11",
            store::Flag::File(FileType::Regular),
        )]);
        store
            .insert(repo_path("foo"), hgid("10"), foo_entry.to_bytes())
            .unwrap();
        let mut tree = TreeManifest::durable(Arc::new(store), hgid("1"));

        assert_eq!(
            tree.get_file(repo_path("foo/bar")).unwrap(),
            Some(make_meta("11"))
        );
        assert_eq!(
            tree.get_file(repo_path("baz")).unwrap(),
            Some(make_meta("20"))
        );

        tree.insert(repo_path_buf("foo/bat"), make_meta("12"))
            .unwrap();
        assert_eq!(
            tree.get_file(repo_path("foo/bat")).unwrap(),
            Some(make_meta("12"))
        );
        assert_eq!(
            tree.get_file(repo_path("foo/bar")).unwrap(),
            Some(make_meta("11"))
        );
        assert_eq!(
            tree.get_file(repo_path("baz")).unwrap(),
            Some(make_meta("20"))
        );
    }

    #[test]
    fn test_insert_into_directory() {
        let mut tree = TreeManifest::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("foo/bar/baz"), make_meta("10"))
            .unwrap();
        assert!(
            tree.insert(repo_path_buf("foo/bar"), make_meta("20"))
                .is_err()
        );
        assert!(tree.insert(repo_path_buf("foo"), make_meta("30")).is_err());
    }

    #[test]
    fn test_insert_with_file_parent() {
        let mut tree = TreeManifest::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("foo"), make_meta("10")).unwrap();
        assert!(
            tree.insert(repo_path_buf("foo/bar"), make_meta("20"))
                .is_err()
        );
        assert!(
            tree.insert(repo_path_buf("foo/bar/baz"), make_meta("30"))
                .is_err()
        );
    }

    #[test]
    fn test_get_from_directory() {
        let mut tree = TreeManifest::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("foo/bar/baz"), make_meta("10"))
            .unwrap();
        assert_eq!(
            tree.get(repo_path("foo/bar")).unwrap(),
            Some(FsNodeMetadata::Directory(None))
        );
        assert_eq!(
            tree.get(repo_path("foo")).unwrap(),
            Some(FsNodeMetadata::Directory(None))
        );
    }

    #[test]
    fn test_get_with_file_parent() {
        let mut tree = TreeManifest::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("foo"), make_meta("10")).unwrap();
        assert_eq!(tree.get(repo_path("foo/bar")).unwrap(), None);
        assert_eq!(tree.get(repo_path("foo/bar/baz")).unwrap(), None);
    }

    #[test]
    fn test_remove_from_ephemeral() {
        let mut tree = TreeManifest::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("a1/b1/c1/d1"), make_meta("10"))
            .unwrap();
        tree.insert(repo_path_buf("a1/b2"), make_meta("20"))
            .unwrap();
        tree.insert(repo_path_buf("a2/b2/c2"), make_meta("30"))
            .unwrap();

        assert_eq!(tree.remove(repo_path("a1")).unwrap(), None);
        assert_eq!(tree.remove(repo_path("a1/b1")).unwrap(), None);
        assert_eq!(tree.remove(repo_path("a1/b1/c1/d1/e1")).unwrap(), None);
        assert_eq!(
            tree.remove(repo_path("a1/b1/c1/d1")).unwrap(),
            Some(make_meta("10"))
        );
        assert_eq!(tree.remove(repo_path("a3")).unwrap(), None);
        assert_eq!(tree.remove(repo_path("a1/b3")).unwrap(), None);
        assert_eq!(tree.remove(repo_path("a1/b1/c1/d2")).unwrap(), None);
        assert_eq!(tree.remove(repo_path("a1/b1/c1/d1/e1")).unwrap(), None);
        assert_eq!(tree.remove(RepoPath::empty()).unwrap(), None);
        assert_eq!(tree.get(repo_path("a1/b1/c1/d1")).unwrap(), None);
        assert_eq!(tree.get(repo_path("a1/b1/c1")).unwrap(), None);
        assert_eq!(
            tree.get(repo_path("a1/b2")).unwrap(),
            Some(FsNodeMetadata::File(make_meta("20")))
        );
        assert_eq!(
            tree.remove(repo_path("a1/b2")).unwrap(),
            Some(make_meta("20"))
        );
        assert_eq!(tree.get(repo_path("a1")).unwrap(), None);

        assert_eq!(
            tree.get(repo_path("a2/b2/c2")).unwrap(),
            Some(FsNodeMetadata::File(make_meta("30")))
        );
        assert_eq!(
            tree.remove(repo_path("a2/b2/c2")).unwrap(),
            Some(make_meta("30"))
        );
        assert_eq!(tree.get(repo_path("a2")).unwrap(), None);

        assert_eq!(
            tree.get(RepoPath::empty()).unwrap(),
            Some(FsNodeMetadata::Directory(None))
        );
    }

    #[test]
    fn test_remove_from_durable() {
        let store = TestStore::new();
        let root_entry = store::Entry::from_elements_hg(vec![
            store_element("a1", "10", store::Flag::Directory),
            store_element("a2", "20", store::Flag::File(FileType::Regular)),
        ]);
        let tree_hgid = hgid("1");
        store
            .insert(RepoPath::empty(), tree_hgid, root_entry.to_bytes())
            .unwrap();
        let a1_entry = store::Entry::from_elements_hg(vec![
            store_element("b1", "11", store::Flag::File(FileType::Regular)),
            store_element("b2", "12", store::Flag::File(FileType::Regular)),
        ]);
        store
            .insert(repo_path("a1"), hgid("10"), a1_entry.to_bytes())
            .unwrap();
        let mut tree = TreeManifest::durable(Arc::new(store), tree_hgid);

        assert_eq!(
            tree.get(RepoPath::empty()).unwrap(),
            Some(FsNodeMetadata::Directory(Some(tree_hgid)))
        );
        assert_eq!(tree.remove(repo_path("a1")).unwrap(), None);
        assert_eq!(
            tree.remove(repo_path("a1/b1")).unwrap(),
            Some(make_meta("11"))
        );
        assert_eq!(tree.get(repo_path("a1/b1")).unwrap(), None);
        assert_eq!(
            tree.get(repo_path("a1/b2")).unwrap(),
            Some(FsNodeMetadata::File(make_meta("12")))
        );
        assert_eq!(
            tree.remove(repo_path("a1/b2")).unwrap(),
            Some(make_meta("12"))
        );
        assert_eq!(tree.get(repo_path("a1/b2")).unwrap(), None);
        assert_eq!(tree.get(repo_path("a1")).unwrap(), None);
        assert_eq!(tree.get_link(repo_path("a1")).unwrap(), None);

        assert_eq!(
            tree.get(repo_path("a2")).unwrap(),
            Some(FsNodeMetadata::File(make_meta("20")))
        );
        assert_eq!(tree.remove(repo_path("a2")).unwrap(), Some(make_meta("20")));
        assert_eq!(tree.get(repo_path("a2")).unwrap(), None);
        assert_eq!(
            tree.get(RepoPath::empty()).unwrap(),
            Some(FsNodeMetadata::Directory(None))
        );
    }

    #[test]
    fn test_flush() {
        let store = Arc::new(TestStore::new());
        let mut tree = TreeManifest::ephemeral(store.clone());
        tree.insert(repo_path_buf("a1/b1/c1/d1"), make_meta("10"))
            .unwrap();
        tree.insert(repo_path_buf("a1/b2"), make_meta("20"))
            .unwrap();
        tree.insert(repo_path_buf("a2/b2/c2"), make_meta("30"))
            .unwrap();

        let hgid = Manifest::persist(&mut tree, PersistOpts { parents: &[] }).unwrap();

        let tree = TreeManifest::durable(store, hgid);
        assert_eq!(
            tree.get_file(repo_path("a1/b1/c1/d1")).unwrap(),
            Some(make_meta("10"))
        );
        assert_eq!(
            tree.get_file(repo_path("a1/b2")).unwrap(),
            Some(make_meta("20"))
        );
        assert_eq!(
            tree.get_file(repo_path("a2/b2/c2")).unwrap(),
            Some(make_meta("30"))
        );
        assert_eq!(tree.get(repo_path("a2/b1")).unwrap(), None);
    }

    #[test]
    fn test_finalize_with_zero_and_one_parents() {
        let store = Arc::new(TestStore::new());
        let mut tree = TreeManifest::ephemeral(store.clone());
        tree.insert(repo_path_buf("a1/b1/c1/d1"), make_meta("10"))
            .unwrap();
        tree.insert(repo_path_buf("a1/b2"), make_meta("20"))
            .unwrap();
        tree.insert(repo_path_buf("a2/b2/c2"), make_meta("30"))
            .unwrap();
        let root_id = tree.persist(&[]).unwrap();
        assert_eq!(root_id, get_hgid(&tree, RepoPath::empty()));

        let mut update = tree.clone();
        update
            .insert(repo_path_buf("a1/b2"), make_meta("40"))
            .unwrap();
        update.remove(repo_path("a2/b2/c2")).unwrap();
        update
            .insert(repo_path_buf("a3/b1"), make_meta("50"))
            .unwrap();
        let update_root = update.persist(&[&tree]).unwrap();
        assert_eq!(update_root, get_hgid(&update, RepoPath::empty()));

        // Verify parent info was recorded in the store.
        let a1_id = get_hgid(&update, repo_path("a1"));
        let a1_parents = store.get_parents(repo_path("a1"), a1_id).unwrap();
        assert_eq!(a1_parents, vec![get_hgid(&tree, repo_path("a1"))]);

        // a3 is new (not in tree), so no parent hgids.
        let a3_id = get_hgid(&update, repo_path("a3"));
        assert_eq!(store.get_parents(repo_path("a3"), a3_id), None);

        let root_parents = store.get_parents(RepoPath::empty(), update_root).unwrap();
        assert_eq!(root_parents, vec![root_id]);
    }

    #[test]
    fn test_finalize_merge() {
        let store = Arc::new(TestStore::new());
        let mut p1 = TreeManifest::ephemeral(store.clone());
        p1.insert(repo_path_buf("a1/b1/c1/d1"), make_meta("10"))
            .unwrap();
        p1.insert(repo_path_buf("a1/b2"), make_meta("20")).unwrap();
        p1.insert(repo_path_buf("a2/b2/c2"), make_meta("30"))
            .unwrap();
        p1.persist(&[]).unwrap();

        let mut p2 = TreeManifest::ephemeral(store.clone());
        p2.insert(repo_path_buf("a1/b2"), make_meta("40")).unwrap();
        p2.insert(repo_path_buf("a3/b1"), make_meta("50")).unwrap();
        p2.persist(&[]).unwrap();

        let mut tree = p1.clone();
        tree.insert(repo_path_buf("a1/b2"), make_meta("40"))
            .unwrap();
        tree.insert(repo_path_buf("a2/b2/c2"), make_meta("60"))
            .unwrap();
        tree.insert(repo_path_buf("a3/b1"), make_meta("50"))
            .unwrap();
        let root_id = tree.persist(&[&p1, &p2]).unwrap();

        let a1_id = get_hgid(&tree, repo_path("a1"));
        let a1_parents = store.get_parents(repo_path("a1"), a1_id).unwrap();
        assert_eq!(
            a1_parents,
            vec![
                get_hgid(&p1, repo_path("a1")),
                get_hgid(&p2, repo_path("a1"))
            ]
        );

        let a2_b2_id = get_hgid(&tree, repo_path("a2/b2"));
        let a2_b2_parents = store.get_parents(repo_path("a2/b2"), a2_b2_id).unwrap();
        assert_eq!(a2_b2_parents, vec![get_hgid(&p1, repo_path("a2/b2"))]);

        // a3 only exists in p2.
        let a3_id = get_hgid(&tree, repo_path("a3"));
        let a3_parents = store.get_parents(repo_path("a3"), a3_id).unwrap();
        assert_eq!(a3_parents, vec![get_hgid(&p2, repo_path("a3"))]);

        let root_parents = store.get_parents(RepoPath::empty(), root_id).unwrap();
        assert_eq!(
            root_parents,
            vec![
                get_hgid(&p1, RepoPath::empty()),
                get_hgid(&p2, RepoPath::empty()),
            ]
        );
    }

    #[test]
    fn test_finalize_file_to_directory() {
        let store = Arc::new(TestStore::new());
        let mut tree1 = TreeManifest::ephemeral(store.clone());
        tree1.insert(repo_path_buf("a1"), make_meta("10")).unwrap();
        let tree1_root = tree1.persist(&[]).unwrap();

        // Initial commit has no parent info recorded (empty parents vec).
        assert_eq!(store.get_parents(RepoPath::empty(), tree1_root), None);

        let mut tree2 = TreeManifest::ephemeral(store.clone());
        tree2
            .insert(repo_path_buf("a1/b1"), make_meta("20"))
            .unwrap();
        let tree2_root = tree2.persist(&[&tree1]).unwrap();

        // "a1" was a file in tree1, now a directory — no parent for "a1" dir.
        let a1_id = get_hgid(&tree2, repo_path("a1"));
        assert_eq!(store.get_parents(repo_path("a1"), a1_id), None);
        // Root's parent should be tree1's root.
        let root_parents = store.get_parents(RepoPath::empty(), tree2_root).unwrap();
        assert_eq!(root_parents, vec![tree1_root]);

        let mut tree3 = TreeManifest::ephemeral(store.clone());
        tree3.insert(repo_path_buf("a1"), make_meta("30")).unwrap();
        let tree3_root = tree3.persist(&[&tree2]).unwrap();

        // Root's parent should be tree2's root.
        let root_parents = store.get_parents(RepoPath::empty(), tree3_root).unwrap();
        assert_eq!(root_parents, vec![tree2_root]);
    }

    #[test]
    fn test_finalize_on_durable() {
        let store = Arc::new(TestStore::new());
        let mut tree1 = TreeManifest::ephemeral(store);
        tree1
            .insert(repo_path_buf("a1/b1/c1/d1"), make_meta("10"))
            .unwrap();
        tree1
            .insert(repo_path_buf("a1/b2"), make_meta("20"))
            .unwrap();
        tree1
            .insert(repo_path_buf("a2/b2/c2"), make_meta("30"))
            .unwrap();
        tree1.persist(&[]).unwrap();

        let mut tree2 = tree1.clone();
        tree2
            .insert(repo_path_buf("a1/b2"), make_meta("40"))
            .unwrap();
        tree2
            .insert(repo_path_buf("a2/b2/c2"), make_meta("60"))
            .unwrap();
        tree2
            .insert(repo_path_buf("a3/b1"), make_meta("50"))
            .unwrap();
        let root1 = tree2.persist(&[&tree1]).unwrap();
        // Persisting again should return the same root (deterministic).
        let root2 = tree2.persist(&[&tree1]).unwrap();
        assert_eq!(root1, root2);
    }

    #[test]
    fn test_finalize_materialization() {
        let store = Arc::new(TestStore::new());
        let entry_1 = store::Entry::from_elements_hg(vec![
            store_element("foo", "10", store::Flag::Directory),
            store_element("baz", "20", store::Flag::File(FileType::Regular)),
        ]);
        store
            .insert(RepoPath::empty(), hgid("1"), entry_1.to_bytes())
            .unwrap();
        let parent = TreeManifest::durable(store.clone(), hgid("1"));

        let entry_2 = store::Entry::from_elements_hg(vec![
            store_element("foo", "10", store::Flag::Directory),
            store_element("baz", "21", store::Flag::File(FileType::Regular)),
        ]);
        store
            .insert(RepoPath::empty(), hgid("2"), entry_2.to_bytes())
            .unwrap();

        let mut tree = TreeManifest::durable(store, hgid("2"));

        let _root = tree.persist(&[&parent]).unwrap();
        // expecting the code to not panic
        // the panic would be caused by materializing link (foo, 10) which
        // doesn't have a store entry
    }

    #[test]
    fn test_cursor_skip_on_root() {
        let tree = TreeManifest::ephemeral(Arc::new(TestStore::new()));
        let mut cursor = tree.root_cursor();
        cursor.skip_subtree();
        match cursor.step() {
            Step::Success => panic!("should have reached the end of the tree"),
            Step::End => {} // success
            Step::Err(error) => panic!("{}", error),
        }
    }

    #[test]
    fn test_cursor_skip() {
        fn step<'a>(cursor: &mut DfsCursor<'a>) {
            match cursor.step() {
                Step::Success => {}
                Step::End => panic!("reached the end too soon"),
                Step::Err(error) => panic!("{}", error),
            }
        }
        let mut tree = TreeManifest::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("a1"), make_meta("10")).unwrap();
        tree.insert(repo_path_buf("a2/b2"), make_meta("20"))
            .unwrap();
        tree.insert(repo_path_buf("a3"), make_meta("30")).unwrap();

        let mut cursor = tree.root_cursor();
        step(&mut cursor);
        assert_eq!(cursor.path(), RepoPath::empty());
        step(&mut cursor);
        assert_eq!(cursor.path(), RepoPath::from_str("a1").unwrap());
        // Skip leaf
        cursor.skip_subtree();
        step(&mut cursor);
        assert_eq!(cursor.path(), RepoPath::from_str("a2").unwrap());
        // Skip directory
        cursor.skip_subtree();
        step(&mut cursor);
        assert_eq!(cursor.path(), RepoPath::from_str("a3").unwrap());
        // Skip on the element before State::End
        cursor.skip_subtree();
        match cursor.step() {
            Step::Success => panic!("should have reached the end of the tree"),
            Step::End => {} // success
            Step::Err(error) => panic!("{}", error),
        }
    }

    #[test]
    fn test_debug() {
        use std::fmt::Write;

        let store = Arc::new(TestStore::new());
        let mut tree = TreeManifest::ephemeral(store);
        tree.insert(repo_path_buf("a1/b1/c1/d1"), make_meta("10"))
            .unwrap();
        let _hgid = Manifest::persist(&mut tree, PersistOpts { parents: &[] }).unwrap();

        tree.insert(repo_path_buf("a1/b2"), make_meta("20"))
            .unwrap();
        tree.insert(repo_path_buf("a2/b2/c2"), make_meta("30"))
            .unwrap();

        let mut output = String::new();
        write!(output, "{tree:?}").unwrap();
        assert_eq!(
            output,
            "Root (Ephemeral)\n\
             | a1 (Ephemeral)\n\
             | | b1 (Durable, d6a71387fe6f91389e9f1b253b5d89d73e2c0741)\n\
             | | | c1 (Durable, 73480e69d7ce9b9b3e8a3a02c4d0190f1f460306)\n\
             | | | | d1 (File, 0000000000000000000000000000000000000010, Regular)\n\
             | | b2 (File, 0000000000000000000000000000000000000020, Regular)\n\
             | a2 (Ephemeral)\n\
             | | b2 (Ephemeral)\n\
             | | | c2 (File, 0000000000000000000000000000000000000030, Regular)\n\
             "
        );
    }

    #[test]
    fn test_compat_subtree_diff() {
        let store = Arc::new(TestStore::new());
        // add ("", 1), ("foo", 11), ("baz", 21), ("foo/bar", 111)
        let root_1_entry = store::Entry::from_elements_hg(vec![
            store_element("foo", "11", store::Flag::Directory),
            store_element("baz", "21", store::Flag::File(FileType::Regular)),
        ]);
        store
            .insert(
                RepoPath::empty(),
                hgid("1"),
                root_1_entry.clone().to_bytes(),
            )
            .unwrap();
        let foo_11_entry = store::Entry::from_elements_hg(vec![store_element(
            "bar",
            "111",
            store::Flag::File(FileType::Regular),
        )]);
        store
            .insert(
                repo_path("foo"),
                hgid("11"),
                foo_11_entry.clone().to_bytes(),
            )
            .unwrap();

        // add ("", 2), ("foo", 12), ("baz", 21), ("foo/bar", 112)
        let root_2_entry = store::Entry::from_elements_hg(vec![
            store_element("foo", "12", store::Flag::Directory),
            store_element("baz", "21", store::Flag::File(FileType::Regular)),
        ]);
        store
            .insert(RepoPath::empty(), hgid("2"), root_2_entry.to_bytes())
            .unwrap();
        let foo_12_entry = store::Entry::from_elements_hg(vec![store_element(
            "bar",
            "112",
            store::Flag::File(FileType::Regular),
        )]);
        store
            .insert(repo_path("foo"), hgid("12"), foo_12_entry.to_bytes())
            .unwrap();

        assert_eq!(
            compat_subtree_diff(
                store.clone(),
                RepoPath::empty(),
                hgid("1"),
                vec![hgid("2")],
                3
            )
            .unwrap(),
            vec![
                (
                    repo_path_buf("foo"),
                    hgid("11"),
                    vec![hgid("12")],
                    foo_11_entry.clone().to_bytes()
                ),
                (
                    RepoPathBuf::new(),
                    hgid("1"),
                    vec![hgid("2")],
                    root_1_entry.clone().to_bytes()
                ),
            ]
        );
        assert_eq!(
            compat_subtree_diff(
                store.clone(),
                RepoPath::empty(),
                hgid("1"),
                vec![hgid("2")],
                1
            )
            .unwrap(),
            vec![(
                RepoPathBuf::new(),
                hgid("1"),
                vec![hgid("2")],
                root_1_entry.to_bytes()
            ),]
        );
        assert_eq!(
            compat_subtree_diff(
                store.clone(),
                repo_path("foo"),
                hgid("11"),
                vec![hgid("12")],
                3
            )
            .unwrap(),
            vec![(
                repo_path_buf("foo"),
                hgid("11"),
                vec![hgid("12")],
                foo_11_entry.to_bytes()
            ),]
        );
        assert_eq!(
            compat_subtree_diff(
                store.clone(),
                RepoPath::empty(),
                hgid("1"),
                vec![hgid("1")],
                3
            )
            .unwrap(),
            vec![]
        );
        assert_eq!(
            compat_subtree_diff(store, repo_path("foo"), hgid("11"), vec![hgid("11")], 3).unwrap(),
            vec![]
        );
        // it is illegal to call compat_subtree_diff with "baz" but we can't validate for it
    }

    #[test]
    fn test_compat_subtree_diff_file_to_directory() {
        let store = Arc::new(TestStore::new());
        // add ("", 1), ("foo", 11)
        let root_1_entry = store::Entry::from_elements_hg(vec![store_element(
            "foo",
            "11",
            store::Flag::File(FileType::Regular),
        )]);
        store
            .insert(RepoPath::empty(), hgid("1"), root_1_entry.to_bytes())
            .unwrap();

        // add ("", 2), ("foo", 12), ("foo/bar", 121)
        let root_2_entry = store::Entry::from_elements_hg(vec![store_element(
            "foo",
            "12",
            store::Flag::Directory,
        )]);
        store
            .insert(
                RepoPath::empty(),
                hgid("2"),
                root_2_entry.clone().to_bytes(),
            )
            .unwrap();
        let foo_12_entry = store::Entry::from_elements_hg(vec![store_element(
            "bar",
            "121",
            store::Flag::File(FileType::Regular),
        )]);
        store
            .insert(
                repo_path("foo"),
                hgid("12"),
                foo_12_entry.clone().to_bytes(),
            )
            .unwrap();

        assert_eq!(
            compat_subtree_diff(store, RepoPath::empty(), hgid("2"), vec![hgid("1")], 3).unwrap(),
            vec![
                (
                    repo_path_buf("foo"),
                    hgid("12"),
                    vec![],
                    foo_12_entry.to_bytes()
                ),
                (
                    RepoPathBuf::new(),
                    hgid("2"),
                    vec![hgid("1")],
                    root_2_entry.to_bytes()
                ),
            ]
        );
    }

    #[test]
    fn test_list() {
        test_list_format(SerializationFormat::Git);
        test_list_format(SerializationFormat::Hg);
    }

    fn test_list_format(format: SerializationFormat) {
        let mut tree = TreeManifest::ephemeral(Arc::new(TestStore::new().with_format(format)));
        let c1_meta = make_meta("10");
        tree.insert(repo_path_buf("a1/b1/c1"), c1_meta).unwrap();
        let b2_meta = make_meta("20");
        tree.insert(repo_path_buf("a1/b2"), b2_meta).unwrap();
        let _hgid = Manifest::persist(&mut tree, PersistOpts { parents: &[] }).unwrap();
        let c2_meta = make_meta("30");
        tree.insert(repo_path_buf("a2/b3/c2"), c2_meta).unwrap();
        let b4_meta = make_meta("40");
        tree.insert(repo_path_buf("a2/b4"), b4_meta).unwrap();

        assert_eq!(tree.list(repo_path("not_found")).unwrap(), List::NotFound);
        assert_eq!(tree.list(repo_path("a1/b1/c1")).unwrap(), List::File);
        assert_eq!(
            tree.list(repo_path("a1/b1")).unwrap(),
            List::Directory(vec![(
                path_component_buf("c1"),
                FsNodeMetadata::File(c1_meta)
            )]),
        );
        assert_eq!(
            tree.list(repo_path("a1")).unwrap(),
            List::Directory(vec![
                (
                    path_component_buf("b1"),
                    tree.get(repo_path("a1/b1")).unwrap().unwrap()
                ),
                (path_component_buf("b2"), FsNodeMetadata::File(b2_meta)),
            ]),
        );
        assert_eq!(tree.list(repo_path("a2/b3/c2")).unwrap(), List::File);
        assert_eq!(
            tree.list(repo_path("a2/b3")).unwrap(),
            List::Directory(vec![(
                path_component_buf("c2"),
                FsNodeMetadata::File(c2_meta)
            )]),
        );
        assert_eq!(
            tree.list(repo_path("a2")).unwrap(),
            List::Directory(vec![
                (path_component_buf("b3"), FsNodeMetadata::Directory(None)),
                (path_component_buf("b4"), FsNodeMetadata::File(b4_meta)),
            ]),
        );
        assert_eq!(
            tree.list(RepoPath::empty()).unwrap(),
            List::Directory(vec![
                (
                    path_component_buf("a1"),
                    tree.get(repo_path("a1")).unwrap().unwrap()
                ),
                (path_component_buf("a2"), FsNodeMetadata::Directory(None)),
            ]),
        );
    }

    fn list_files(m: &TreeManifest) -> Vec<String> {
        let mut files = m
            .files(AlwaysMatcher::new())
            .map(|f| Ok(f?.path.into_string()))
            .collect::<Result<Vec<_>>>()
            .unwrap();
        files.sort();
        files
    }

    #[test]
    fn test_graft() {
        let mut tree = TreeManifest::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("a"), make_meta("10")).unwrap();
        tree.insert(repo_path_buf("dir/b"), make_meta("10"))
            .unwrap();
        tree.insert(repo_path_buf("dir/dir/c"), make_meta("10"))
            .unwrap();

        let mut grafted = tree.clone();
        grafted
            .graft(repo_path("dir"), &tree, repo_path("dir/dir"))
            .unwrap();
        // Graft overwrites existing tree - does not "overlay".
        assert_eq!(list_files(&grafted), vec!["a", "dir/c"]);

        // Other tree didn't change
        assert_eq!(list_files(&tree), vec!["a", "dir/b", "dir/dir/c"]);

        // Can graft over a file
        let mut grafted = tree.clone();
        grafted
            .graft(repo_path("dir/b"), &tree, repo_path("dir/dir"))
            .unwrap();
        assert_eq!(list_files(&grafted), vec!["a", "dir/b/c", "dir/dir/c"]);

        // Can insert empty directories
        let mut grafted = tree.clone();
        grafted
            .graft(repo_path("dir"), &tree, repo_path("not_exist"))
            .unwrap();
        assert_eq!(list_files(&grafted), vec!["a"]);
    }

    fn grafted_diff(a: &TreeManifest, b: &TreeManifest) -> Vec<String> {
        let (a, b) = apply_diff_grafts(a, b).unwrap();
        let mut files = a
            .diff(&b, AlwaysMatcher::new())
            .unwrap()
            .into_iter()
            .map(|e| Ok(e?.path.into_string()))
            .collect::<Result<Vec<_>>>()
            .unwrap();
        files.sort();
        files
    }

    #[test]
    fn test_register_diff_graft_validation() {
        let mut tree = TreeManifest::ephemeral(Arc::new(TestStore::new()));
        // Test we don't allow overlapping "to" values in grafts.
        assert!(
            tree.register_diff_graft(repo_path("foo"), repo_path("bar"))
                .is_ok()
        );
        assert!(
            tree.register_diff_graft(repo_path("foo"), repo_path("baz"))
                .is_ok()
        );
        assert!(
            tree.register_diff_graft(repo_path("anything"), repo_path("bar"))
                .is_err()
        );
        assert!(
            tree.register_diff_graft(repo_path("anything"), repo_path("bar/anything"))
                .is_err()
        );
        assert!(
            tree.register_diff_graft(repo_path("anything"), repo_path(""))
                .is_err()
        );

        let mut tree = TreeManifest::ephemeral(Arc::new(TestStore::new()));
        assert!(
            tree.register_diff_graft(repo_path("foo"), repo_path(""))
                .is_ok()
        );
        assert!(
            tree.register_diff_graft(repo_path("anything"), repo_path("anything"))
                .is_err()
        );

        let mut tree = TreeManifest::ephemeral(Arc::new(TestStore::new()));
        assert!(
            tree.register_diff_graft(repo_path("foo"), repo_path("bar/one"))
                .is_ok()
        );
        assert!(
            tree.register_diff_graft(repo_path("foo"), repo_path("bar/two"))
                .is_ok()
        );
        assert!(
            tree.register_diff_graft(repo_path("foo"), repo_path("bar/two"))
                .is_err()
        );
    }

    #[test]
    fn test_graft_for_diff() {
        let mut left = TreeManifest::ephemeral(Arc::new(TestStore::new()));
        left.insert(repo_path_buf("left/a"), make_meta("10"))
            .unwrap();
        left.insert(repo_path_buf("left/b"), make_meta("10"))
            .unwrap();
        left.insert(repo_path_buf("left_only"), make_meta("10"))
            .unwrap();

        let mut right = TreeManifest::ephemeral(Arc::new(TestStore::new()));
        right
            .insert(repo_path_buf("right/b"), make_meta("10"))
            .unwrap();
        right
            .insert(repo_path_buf("right/c"), make_meta("10"))
            .unwrap();
        right
            .insert(repo_path_buf("right_only"), make_meta("10"))
            .unwrap();

        // Sanity with no grafts registered
        assert_eq!(
            grafted_diff(&left, &right),
            vec![
                "left/a",
                "left/b",
                "left_only",
                "right/b",
                "right/c",
                "right_only",
            ]
        );
        assert!(left.ungrafted_path(repo_path("right/b")).is_none());

        // Now register a graft form left->right
        left.register_diff_graft(repo_path("left"), repo_path("right"))
            .unwrap();

        assert_eq!(grafted_diff(&left, &right), vec!["right/a", "right/c"]);
        assert_eq!(
            left.ungrafted_path(repo_path("right/b")),
            Some(repo_path_buf("left/b"))
        );
        assert!(right.ungrafted_path(repo_path("right/b")).is_none());

        // Order doesn't matter
        assert_eq!(grafted_diff(&right, &left), vec!["right/a", "right/c"]);

        // Can graft same path again
        left.register_diff_graft(repo_path("left"), repo_path("right-copy"))
            .unwrap();

        assert_eq!(
            grafted_diff(&left, &right),
            vec!["right-copy/a", "right-copy/b", "right/a", "right/c"]
        );

        // Can graft other side, too:

        // This keeps "right" in place.
        right
            .register_diff_graft(repo_path("right"), repo_path("right"))
            .unwrap();
        // This grafts right into right-copy
        right
            .register_diff_graft(repo_path("right"), repo_path("right-copy"))
            .unwrap();

        assert_eq!(
            grafted_diff(&left, &right),
            vec!["right-copy/a", "right-copy/c", "right/a", "right/c"]
        );
        assert_eq!(
            right.ungrafted_path(repo_path("right-copy/b")),
            Some(repo_path_buf("right/b"))
        );
        assert!(right.ungrafted_path(repo_path("right/b")).is_none());
        assert_eq!(
            left.ungrafted_path(repo_path("right-copy/b")),
            Some(repo_path_buf("left/b"))
        );
    }

    #[test]
    fn test_grafted_conversion() {
        let mut tree = TreeManifest::ephemeral(Arc::new(TestStore::new()));
        tree.insert(repo_path_buf("foo/a"), make_meta("10"))
            .unwrap();

        tree.register_diff_graft(repo_path("foo"), repo_path("bar"))
            .unwrap();
        tree.register_diff_graft(repo_path("foo"), repo_path("baz"))
            .unwrap();
        tree.register_diff_graft(repo_path("something"), repo_path("else"))
            .unwrap();

        assert_eq!(
            tree.grafted_paths(repo_path("foo")),
            &[repo_path_buf("bar"), repo_path_buf("baz")]
        );
        assert_eq!(
            tree.grafted_dests(repo_path("foo")),
            &[repo_path_buf("bar"), repo_path_buf("baz")]
        );

        assert_eq!(
            tree.grafted_paths(repo_path("foo/a/b")),
            &[repo_path_buf("bar/a/b"), repo_path_buf("baz/a/b")]
        );
        assert_eq!(
            tree.grafted_dests(repo_path("foo/a/b")),
            &[repo_path_buf("bar"), repo_path_buf("baz")]
        );

        assert!(
            tree.grafted_path(repo_path("foo/a/b"), repo_path("nothing"))
                .is_none()
        );
        assert_eq!(
            tree.grafted_path(repo_path("foo/a/b"), repo_path("baz/something")),
            Some(repo_path_buf("baz/a/b"))
        );
    }

    #[test]
    fn test_is_dirty() {
        let mut tree = TreeManifest::ephemeral(Arc::new(TestStore::new()));

        tree.insert(repo_path_buf("foo/bar/file"), make_meta("10"))
            .unwrap();
        assert!(tree.is_dirty());

        let _ = tree.persist(&[]).unwrap();
        assert!(!tree.is_dirty());

        tree.insert(repo_path_buf("foo/bar/file"), make_meta("11"))
            .unwrap();
        assert!(tree.is_dirty());

        let _ = tree.persist(&[]).unwrap();
        assert!(!tree.is_dirty());

        tree.register_diff_graft(repo_path("from"), repo_path("to"))
            .unwrap();
        assert!(tree.is_dirty());
    }

    #[test]
    fn test_hg_size_hint() {
        use storemodel::TreeEntry;

        let entry = store::Entry::from_elements_hg(Vec::new());
        assert_eq!(entry.size_hint(), Some(0));

        let entry = store::Entry::from_elements_hg(vec![store_element(
            "foo",
            "123",
            store::Flag::File(FileType::Regular),
        )]);
        assert_eq!(entry.size_hint(), Some(1));

        let entry = store::Entry::from_elements_hg(vec![
            store_element("foo", "123", store::Flag::File(FileType::Regular)),
            store_element("bar", "456", store::Flag::Directory),
        ]);
        assert_eq!(entry.size_hint(), Some(2));
    }

    fn make_test_tree_entry(
        store: &TestStore,
        elements: Vec<store::Element>,
        acl_indices: Option<Vec<u32>>,
    ) -> Arc<dyn storemodel::TreeEntry> {
        let entry = store::Entry::from_elements_hg(elements);
        let hgid = hgid("fff");
        store
            .insert(RepoPath::empty(), hgid, entry.to_bytes())
            .unwrap();
        if let Some(indices) = acl_indices {
            store.set_acl_children_indices(hgid, indices);
        }
        store
            .get_local_tree(RepoPath::empty(), hgid)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn test_migrate_acl_children_preserves_unchanged() {
        let store = TestStore::new();
        let old_tree = make_test_tree_entry(
            &store,
            vec![
                store_element("dir", "aaa", store::Flag::Directory),
                store_element("file", "bbb", store::Flag::File(FileType::Regular)),
            ],
            Some(vec![0]),
        );

        let new_entry = store::Entry::from_elements_hg(vec![
            store_element("dir", "aaa", store::Flag::Directory),
            store_element("file", "bbb", store::Flag::File(FileType::Regular)),
        ]);

        let result = migrate_acl_children(Some(&old_tree), &new_entry);
        assert_eq!(result, Some(vec![0]));
    }

    #[test]
    fn test_migrate_acl_children_remaps_index() {
        let store = TestStore::new();
        // Old tree: [dir @aaa (acl), file @bbb] — "dir" is at index 0.
        let old_tree = make_test_tree_entry(
            &store,
            vec![
                store_element("dir", "aaa", store::Flag::Directory),
                store_element("file", "bbb", store::Flag::File(FileType::Regular)),
            ],
            Some(vec![0]),
        );

        // New tree: [aaa_new_file @ccc, dir @aaa, file @bbb] — "dir" shifted to index 1.
        let new_entry = store::Entry::from_elements_hg(vec![
            store_element("aaa_new_file", "ccc", store::Flag::File(FileType::Regular)),
            store_element("dir", "aaa", store::Flag::Directory),
            store_element("file", "bbb", store::Flag::File(FileType::Regular)),
        ]);

        let result = migrate_acl_children(Some(&old_tree), &new_entry);
        assert_eq!(result, Some(vec![1]));
    }

    #[test]
    fn test_migrate_acl_children_skips_changed() {
        let store = TestStore::new();
        let old_tree = make_test_tree_entry(
            &store,
            vec![store_element("dir", "aaa", store::Flag::Directory)],
            Some(vec![0]),
        );

        let new_entry = store::Entry::from_elements_hg(vec![store_element(
            "dir",
            "ccc",
            store::Flag::Directory,
        )]);

        let result = migrate_acl_children(Some(&old_tree), &new_entry);
        assert_eq!(result, None);
    }

    #[test]
    fn test_migrate_acl_children_no_parent() {
        let new_entry = store::Entry::from_elements_hg(vec![store_element(
            "dir",
            "aaa",
            store::Flag::Directory,
        )]);
        let result = migrate_acl_children(None, &new_entry);
        assert_eq!(result, None);
    }

    /// Persists a tree whose directory "a" lists "f" and "restricted", with "restricted" (index
    /// 1) marked as having an ACL, and returns the root id.
    fn persist_tree_with_acl_child(store: &Arc<TestStore>) -> HgId {
        let mut tree = TreeManifest::ephemeral(store.clone());
        tree.insert(repo_path_buf("a/f"), make_meta("10")).unwrap();
        tree.insert(repo_path_buf("a/restricted/f"), make_meta("20"))
            .unwrap();
        let root = tree.persist(&[]).unwrap();
        store.set_acl_children_indices(get_hgid(&tree, repo_path("a")), vec![1]);
        root
    }

    #[test]
    fn test_persist_acl_children_with_parent_prefetched() {
        // Rebase diffs the destination tree to find merge inputs. The diff's batch prefetch sets
        // the directories' links without keeping their tree entries.
        let store = Arc::new(TestStore::new());
        let root = persist_tree_with_acl_child(&store);
        let parent = TreeManifest::durable(store.clone(), root);
        parent
            .diff(
                &TreeManifest::ephemeral(store.clone()),
                AlwaysMatcher::new(),
            )
            .unwrap()
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let mut tree = parent.clone();
        tree.insert(repo_path_buf("a/g"), make_meta("30")).unwrap();
        tree.persist(&[&parent]).unwrap();

        // "a" now lists "f", "g" and "restricted".
        let a = get_hgid(&tree, repo_path("a"));
        assert_eq!(store.get_acl_children_indices(a), Some(vec![2]));
    }

    #[test]
    fn test_persist_acl_children_with_parent_persisted_in_process() {
        // The second tree's parent is the first tree, just persisted by this process.
        let store = Arc::new(TestStore::new());
        let root = persist_tree_with_acl_child(&store);
        let mut first = TreeManifest::durable(store.clone(), root);
        let first_parent = first.clone();
        first.insert(repo_path_buf("a/g"), make_meta("30")).unwrap();
        first.persist(&[&first_parent]).unwrap();
        let first_a = get_hgid(&first, repo_path("a"));
        assert_eq!(store.get_acl_children_indices(first_a), Some(vec![2]));

        let mut second = first.clone();
        second
            .insert(repo_path_buf("a/h"), make_meta("40"))
            .unwrap();
        second.persist(&[&first]).unwrap();

        // "a" now lists "f", "g", "h" and "restricted".
        let second_a = get_hgid(&second, repo_path("a"));
        assert_eq!(store.get_acl_children_indices(second_a), Some(vec![3]));
    }

    /// Returns a prefetched parent from its own store, and a tree in another store, so the
    /// parent's tree entries can't be loaded from the tree's store.
    fn tree_with_parent_from_another_store() -> (TreeManifest, TreeManifest, Arc<TestStore>) {
        let parent_store = Arc::new(TestStore::new());
        let root = persist_tree_with_acl_child(&parent_store);
        let parent = TreeManifest::durable(parent_store.clone(), root);
        parent
            .diff(
                &TreeManifest::ephemeral(parent_store.clone()),
                AlwaysMatcher::new(),
            )
            .unwrap()
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let store = Arc::new(TestStore::new());
        let mut tree = TreeManifest::ephemeral(store.clone());
        tree.insert(repo_path_buf("a/g"), make_meta("30")).unwrap();
        (tree, parent, store)
    }

    #[test]
    fn test_persist_fails_when_parent_tree_entry_cannot_be_loaded() {
        // Persisting must fail rather than write "a" without its ACL child.
        let (mut tree, parent, _store) = tree_with_parent_from_another_store();

        let err = tree.persist(&[&parent]).unwrap_err();
        assert!(
            format!("{err:#}").contains("failed to load parent tree 'a' to migrate ACL children"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn test_persist_without_loading_parent_tree_entry() {
        // With the load disabled, the parent's tree entry isn't loaded: persisting succeeds and
        // "a" is written without its ACL child.
        let (mut tree, parent, store) = tree_with_parent_from_another_store();

        tree.persist_with(&[&parent], false).unwrap();

        let a = get_hgid(&tree, repo_path("a"));
        assert_eq!(store.get_acl_children_indices(a), None);
    }

    #[derive(Debug)]
    struct TestTranslator;

    impl PathTranslator for TestTranslator {
        fn encode_file(&self, path: &RepoPath) -> anyhow::Result<RepoPathBuf> {
            Ok(RepoPathBuf::from_string(format!("{}?", path.as_str()))?)
        }

        fn decode_file(&self, path: &RepoPath) -> anyhow::Result<RepoPathBuf> {
            let s = path.as_str();
            let decoded = s.strip_suffix('?').unwrap_or(s);
            Ok(RepoPathBuf::from_string(decoded.to_string())?)
        }

        fn encode_file_name(&self, name: &PathComponent) -> anyhow::Result<PathComponentBuf> {
            Ok(PathComponentBuf::from_string(format!(
                "{}?",
                name.as_str()
            ))?)
        }
        fn decode_file_name(&self, name: &PathComponent) -> anyhow::Result<PathComponentBuf> {
            let s = name.as_str();
            let decoded = s.strip_suffix('?').unwrap_or(s);
            Ok(PathComponentBuf::from_string(decoded.to_string())?)
        }
    }

    fn make_grepo_tree(store: Arc<TestStore>) -> TreeManifest {
        let mut tree = TreeManifest::ephemeral(store);
        tree.set_path_translator(Arc::new(TestTranslator));

        tree.insert(
            repo_path_buf("foo/a"),
            FileMetadata::new(hgid("10"), FileType::GitSubmodule),
        )
        .unwrap();
        tree.insert(
            repo_path_buf("foo/a/sub/c"),
            FileMetadata::new(hgid("20"), FileType::GitSubmodule),
        )
        .unwrap();
        tree.insert(repo_path_buf("regular_file"), make_meta("30"))
            .unwrap();
        tree
    }

    #[test]
    fn test_translator_encode_decode_roundtrip() {
        let t = TestTranslator;
        let path = repo_path("foo/a");
        let encoded = t.encode_file(path).unwrap();
        assert_eq!(encoded.as_str(), "foo/a?");
        let decoded = t.decode_file(&encoded).unwrap();
        assert_eq!(decoded.as_str(), "foo/a");
        // Non-encoded path passes through decode unchanged
        assert_eq!(t.decode_file(repo_path("plain")).unwrap().as_str(), "plain");
    }

    #[test]
    fn test_translating_matcher() {
        use pathmatcher::DirectoryMatch;

        let t = Arc::new(TestTranslator) as Arc<dyn PathTranslator>;
        let inner = TreeMatcher::from_rules(["foo/a"].iter(), true).unwrap();
        let m = TranslatingMatcher {
            inner,
            translator: t,
        };
        // Encoded file path decoded before matching
        assert!(m.matches_file(repo_path("foo/a?")).unwrap());
        assert!(!m.matches_file(repo_path("foo/b?")).unwrap());
        // Directory paths pass through unchanged
        assert_ne!(
            m.matches_directory(repo_path("foo")).unwrap(),
            DirectoryMatch::Nothing
        );
    }

    #[test]
    fn test_get_file_with_translator() {
        let tree = make_grepo_tree(Arc::new(TestStore::new()));
        let result = tree.get_file(repo_path("foo/a")).unwrap();
        assert_eq!(
            result,
            Some(FileMetadata::new(hgid("10"), FileType::GitSubmodule))
        );
        assert_eq!(tree.get_file(repo_path("foo/a/sub")).unwrap(), None);
        assert_eq!(tree.get_file(repo_path("nonexistent")).unwrap(), None);
    }

    #[test]
    fn test_get_with_translator() {
        let tree = make_grepo_tree(Arc::new(TestStore::new()));
        // "foo/a" is a directory in storage (contains sub/c?)
        assert!(tree.get(repo_path("foo/a")).unwrap().is_some());
        assert!(matches!(
            tree.get(repo_path("foo/a")).unwrap(),
            Some(FsNodeMetadata::Directory(_))
        ));
        // "foo/a/sub" is also a directory
        assert!(matches!(
            tree.get(repo_path("foo/a/sub")).unwrap(),
            Some(FsNodeMetadata::Directory(_))
        ));
    }

    #[test]
    fn test_files_with_translator() {
        let tree = make_grepo_tree(Arc::new(TestStore::new()));
        let mut files: Vec<String> = tree
            .files(AlwaysMatcher::new())
            .map(|f| Ok(f?.path.into_string()))
            .collect::<anyhow::Result<Vec<_>>>()
            .unwrap();
        files.sort();
        // Output paths should be decoded (no suffix)
        assert_eq!(files, vec!["foo/a", "foo/a/sub/c", "regular_file"]);

        // with matcher
        let matcher = TreeMatcher::from_rules(["foo/a"].iter(), true).unwrap();
        let files: Vec<String> = tree
            .files(matcher)
            .map(|f| Ok(f?.path.into_string()))
            .collect::<anyhow::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files, vec!["foo/a"]);
    }

    #[test]
    fn test_diff_with_translator() {
        let store = Arc::new(TestStore::new());
        let left = make_grepo_tree(store.clone());

        let mut right = TreeManifest::ephemeral(store);
        right.set_path_translator(Arc::new(TestTranslator));
        right
            .insert(
                repo_path_buf("foo/a"),
                FileMetadata::new(hgid("11"), FileType::GitSubmodule),
            )
            .unwrap();
        right
            .insert(
                repo_path_buf("foo/a/sub/c"),
                FileMetadata::new(hgid("22"), FileType::GitSubmodule),
            )
            .unwrap();
        right
            .insert(repo_path_buf("regular_file"), make_meta("33"))
            .unwrap();

        let mut entries: Vec<_> = left
            .diff(&right, AlwaysMatcher::new())
            .unwrap()
            .into_iter()
            .collect::<anyhow::Result<Vec<_>>>()
            .unwrap();
        entries.sort_by(|a, b| a.path.cmp(&b.path));

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path.as_str(), "foo/a");
        assert_eq!(entries[1].path.as_str(), "foo/a/sub/c");
        assert_eq!(entries[2].path.as_str(), "regular_file");
        assert_eq!(
            entries[0].diff_type,
            DiffType::Changed(
                FileMetadata::new(hgid("10"), FileType::GitSubmodule),
                FileMetadata::new(hgid("11"), FileType::GitSubmodule),
            )
        );
        assert_eq!(
            entries[1].diff_type,
            DiffType::Changed(
                FileMetadata::new(hgid("20"), FileType::GitSubmodule),
                FileMetadata::new(hgid("22"), FileType::GitSubmodule),
            )
        );
        assert_eq!(
            entries[2].diff_type,
            DiffType::Changed(
                FileMetadata::new(hgid("30"), FileType::Regular),
                FileMetadata::new(hgid("33"), FileType::Regular),
            )
        );

        // Matcher on decoded path "foo/a/sub/c" should match encoded "foo/a/sub/c?"
        let matcher = TreeMatcher::from_rules(["foo/a/sub/c"].iter(), true).unwrap();
        let entries: Vec<_> = left
            .diff(&right, matcher)
            .unwrap()
            .into_iter()
            .collect::<anyhow::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path.as_str(), "foo/a/sub/c");
    }
}
