/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Rich, non-lossy, sharded-backed manifest adapter for the no-Hg Bonsai-direct
//! augmented derivation path (aug-manifest-v2 Option 1).
//!
//! This mirrors `manifest::Traced` (it carries an originating `ParentIndex`) and
//! the existing `HgAugmentedManifestTrieMapNode` (a sharded-backed trie map), but
//! keeps the whole `HgAugmentedDirectoryNode` in the tree id instead of reducing
//! it to a bare `HgAugmentedManifestId`. Keeping the full node lets a rebuilt
//! parent embed a reused child directory without re-loading its envelope, and
//! keeping the sharded trie map lets `expand` stay lazy instead of enumerating a
//! directory the way `Traced`'s `SortedVectorTrieMap` does.

use std::hash::Hash;
use std::hash::Hasher;

use anyhow::Result;
use async_trait::async_trait;
use blobstore::KeyedBlobstore;
use blobstore::LoadableError;
use blobstore::StoreLoadable;
use context::CoreContext;
use futures::stream::BoxStream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use manifest::Entry;
use manifest::Manifest;
use manifest::TrieMapOps;
use mercurial_types::HgAugmentedManifestEntry;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::sharded_augmented_manifest::HgAugmentedDirectoryNode;
use mercurial_types::sharded_augmented_manifest::HgAugmentedFileLeafNode;
use mercurial_types::sharded_augmented_manifest::HgAugmentedManifestEnvelope;
use mononoke_types::MPathElement;
use mononoke_types::sharded_map_v2::LoadableShardedMapV2Node;
use smallvec::SmallVec;

use crate::derive_hg_manifest::ParentIndex;

// `Eq`/`Hash` deliberately key only on `node` and ignore `index` (the
// payload-vs-identity split from `manifest::Traced`). `derive_manifest` dedups
// tree ids/leaves through these impls, so identical content reached via
// different bonsai-parent slots must collapse to one entry; `index` still
// travels through `expand` to let the caller reconstruct p1/p2.

/// Rich tree id: the full augmented directory node plus its originating parent
/// index (`None` for a freshly built directory).
#[derive(Clone, Debug)]
pub struct IndexedAugmentedDirectory {
    pub node: HgAugmentedDirectoryNode,
    pub index: Option<ParentIndex>,
}

impl PartialEq for IndexedAugmentedDirectory {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node
    }
}

impl Eq for IndexedAugmentedDirectory {}

impl Hash for IndexedAugmentedDirectory {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.node.hash(state);
    }
}

/// Rich leaf counterpart to `IndexedAugmentedDirectory`.
#[derive(Clone, Debug)]
pub struct IndexedAugmentedFile {
    pub node: HgAugmentedFileLeafNode,
    pub index: Option<ParentIndex>,
}

impl PartialEq for IndexedAugmentedFile {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node
    }
}

impl Eq for IndexedAugmentedFile {}

impl Hash for IndexedAugmentedFile {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.node.hash(state);
    }
}

pub type IndexedAugmentedEntry = Entry<IndexedAugmentedDirectory, IndexedAugmentedFile>;

/// Loaded rich manifest view for `IndexedAugmentedDirectory`.
pub struct IndexedAugmentedManifest {
    envelope: HgAugmentedManifestEnvelope,
    index: Option<ParentIndex>,
}

#[async_trait]
impl<Store: KeyedBlobstore> StoreLoadable<Store> for IndexedAugmentedDirectory {
    type Value = IndexedAugmentedManifest;

    async fn load<'a>(
        &'a self,
        ctx: &'a CoreContext,
        blobstore: &'a Store,
    ) -> Result<Self::Value, LoadableError> {
        let id = HgAugmentedManifestId::new(self.node.treenode);
        let envelope = HgAugmentedManifestEnvelope::load(ctx, blobstore, id)
            .await?
            .ok_or_else(|| LoadableError::Missing(id.blobstore_key()))?;
        Ok(IndexedAugmentedManifest {
            envelope,
            index: self.index,
        })
    }
}

#[async_trait]
impl<Store: KeyedBlobstore> Manifest<Store> for IndexedAugmentedManifest {
    type TreeId = IndexedAugmentedDirectory;
    type Leaf = IndexedAugmentedFile;
    type TrieMapType = IndexedAugmentedTrieMap;

    async fn list(
        &self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<BoxStream<'async_trait, Result<(MPathElement, IndexedAugmentedEntry)>>> {
        let index = self.index;
        Ok(self
            .envelope
            .augmented_manifest
            .clone()
            .into_subentries(ctx, blobstore)
            .map_ok(move |(path, entry)| (path, indexed_augmented_entry(entry, index)))
            .boxed())
    }

    async fn lookup(
        &self,
        ctx: &CoreContext,
        blobstore: &Store,
        name: &MPathElement,
    ) -> Result<Option<IndexedAugmentedEntry>> {
        Ok(self
            .envelope
            .augmented_manifest
            .lookup(ctx, blobstore, name)
            .await?
            .map(|entry| indexed_augmented_entry(entry, self.index)))
    }

    async fn into_trie_map(
        self,
        _ctx: &CoreContext,
        _blobstore: &Store,
    ) -> Result<Self::TrieMapType> {
        Ok(IndexedAugmentedTrieMap::new(
            LoadableShardedMapV2Node::Inlined(self.envelope.augmented_manifest.subentries),
            self.index,
        ))
    }
}

/// Convert a stored augmented subentry into a rich manifest entry, stamping the
/// originating parent index onto it (mirrors `Traced::inherit_into_entry`).
pub fn indexed_augmented_entry(
    entry: HgAugmentedManifestEntry,
    index: Option<ParentIndex>,
) -> IndexedAugmentedEntry {
    match entry {
        HgAugmentedManifestEntry::FileNode(node) => {
            Entry::Leaf(IndexedAugmentedFile { node, index })
        }
        HgAugmentedManifestEntry::DirectoryNode(node) => {
            Entry::Tree(IndexedAugmentedDirectory { node, index })
        }
    }
}

/// Sharded-backed trie map yielding rich, index-carrying entries. Wraps
/// `LoadableShardedMapV2Node` so `expand` loads only the current node and hands
/// children back as possibly-unloaded nodes, propagating the carried `index` to
/// every entry and child.
#[derive(Clone, Debug)]
pub struct IndexedAugmentedTrieMap {
    inner: LoadableShardedMapV2Node<HgAugmentedManifestEntry>,
    index: Option<ParentIndex>,
}

impl PartialEq for IndexedAugmentedTrieMap {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl Eq for IndexedAugmentedTrieMap {}

impl IndexedAugmentedTrieMap {
    pub(crate) fn new(
        inner: LoadableShardedMapV2Node<HgAugmentedManifestEntry>,
        index: Option<ParentIndex>,
    ) -> Self {
        Self { inner, index }
    }

    pub(crate) fn with_index(self, index: Option<ParentIndex>) -> Self {
        Self { index, ..self }
    }

    pub(crate) fn into_inner(self) -> LoadableShardedMapV2Node<HgAugmentedManifestEntry> {
        self.inner
    }
}

#[async_trait]
impl<Store: KeyedBlobstore> TrieMapOps<Store, IndexedAugmentedEntry> for IndexedAugmentedTrieMap {
    async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<(Option<IndexedAugmentedEntry>, Vec<(u8, Self)>)> {
        let index = self.index;
        let (entry, children) = self.inner.expand(ctx, blobstore).await?;
        Ok((
            entry.map(|entry| indexed_augmented_entry(entry, index)),
            children
                .into_iter()
                .map(|(byte, inner)| (byte, Self { inner, index }))
                .collect(),
        ))
    }

    async fn into_stream(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<BoxStream<'async_trait, Result<(SmallVec<[u8; 24]>, IndexedAugmentedEntry)>>> {
        let index = self.index;
        Ok(self
            .inner
            .load(ctx, blobstore)
            .await?
            .into_entries(ctx, blobstore)
            .map_ok(move |(key, entry)| (key, indexed_augmented_entry(entry, index)))
            .boxed())
    }

    async fn lookup_many(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
        names: Vec<SmallVec<[u8; 24]>>,
    ) -> Result<Vec<(SmallVec<[u8; 24]>, IndexedAugmentedEntry)>> {
        Ok(self
            .inner
            .lookup_many(ctx, blobstore, names)
            .await?
            .into_iter()
            .map(|(name, entry)| (name, indexed_augmented_entry(entry, self.index)))
            .collect())
    }

    fn is_empty(&self) -> bool {
        self.inner.size() == 0
    }
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hash;
    use std::hash::Hasher;

    use anyhow::Result;
    use blobstore::Storable;
    use blobstore::StoreLoadable;
    use context::CoreContext;
    use fbinit::FacebookInit;
    use manifest::Entry;
    use manifest::Manifest;
    use manifest::TrieMapOps;
    use mercurial_types::HgAugmentedManifestEntry;
    use mercurial_types::HgNodeHash;
    use mercurial_types::sharded_augmented_manifest::HgAugmentedDirectoryNode;
    use mercurial_types::sharded_augmented_manifest::HgAugmentedFileLeafNode;
    use mercurial_types::sharded_augmented_manifest::HgAugmentedManifestEnvelope;
    use mercurial_types::sharded_augmented_manifest::ShardedHgAugmentedManifest;
    use mononoke_macros::mononoke;
    use mononoke_types::FileType;
    use mononoke_types::MPathElement;
    use mononoke_types::hash::Blake3;
    use mononoke_types::hash::Sha1 as ContentSha1;
    use mononoke_types::sha1_hash::Sha1 as NodeSha1;
    use mononoke_types::sharded_map_v2::LoadableShardedMapV2Node;
    use mononoke_types::sharded_map_v2::ShardedMapV2Node;
    use repo_blobstore::RepoBlobstore;
    use repo_blobstore::RepoBlobstoreRef;

    use crate::derive_hg_manifest::ParentIndex;
    use crate::indexed_augmented_manifest::IndexedAugmentedDirectory;
    use crate::indexed_augmented_manifest::IndexedAugmentedFile;
    use crate::indexed_augmented_manifest::IndexedAugmentedTrieMap;
    use crate::indexed_augmented_manifest::indexed_augmented_entry;

    #[facet::container]
    #[derive(Clone)]
    struct TestRepo(RepoBlobstore);

    fn dir_node(treenode_byte: u8, aug_byte: u8, size: u64) -> HgAugmentedDirectoryNode {
        HgAugmentedDirectoryNode {
            treenode: HgNodeHash::new(NodeSha1::from_byte_array([treenode_byte; 20])),
            augmented_manifest_id: Blake3::from_byte_array([aug_byte; 32]),
            augmented_manifest_size: size,
            acl_manifest_directory_id: None,
        }
    }

    fn file_node(filenode_byte: u8, content_byte: u8, size: u64) -> HgAugmentedFileLeafNode {
        HgAugmentedFileLeafNode {
            file_type: FileType::Regular,
            filenode: HgNodeHash::new(NodeSha1::from_byte_array([filenode_byte; 20])),
            content_blake3: Blake3::from_byte_array([content_byte; 32]),
            content_sha1: ContentSha1::from_byte_array([content_byte; 20]),
            total_size: size,
            file_header_metadata: None,
        }
    }

    fn hash_value<T: Hash>(value: &T) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    /// Once these types become `derive_manifest` tree ids, dedup keys on the
    /// tree id's `Eq`/`Hash`. Identity must be the directory node alone: the
    /// same content reached through different bonsai-parent slots must compare
    /// and hash equal so it collapses to one entry, while a different node stays
    /// distinct.
    #[mononoke::test]
    fn directory_identity_ignores_parent_index() {
        // Given: one directory node carried with two different parent indices and
        // as a freshly built (index-less) node, plus a different directory node.
        let node = dir_node(0x11, 0x33, 10);
        let from_parent_0 = IndexedAugmentedDirectory {
            node: node.clone(),
            index: Some(ParentIndex(0)),
        };
        let from_parent_1 = IndexedAugmentedDirectory {
            node: node.clone(),
            index: Some(ParentIndex(1)),
        };
        let freshly_built = IndexedAugmentedDirectory {
            node: node.clone(),
            index: None,
        };
        let different_node = IndexedAugmentedDirectory {
            node: dir_node(0x99, 0x33, 10),
            index: Some(ParentIndex(0)),
        };

        // When: hashing each value (the operation dedup relies on).
        let hash_parent_0 = hash_value(&from_parent_0);
        let hash_parent_1 = hash_value(&from_parent_1);
        let hash_freshly_built = hash_value(&freshly_built);

        // Then: identical content compares and hashes equal regardless of index,
        assert_eq!(from_parent_0, from_parent_1);
        assert_eq!(from_parent_0, freshly_built);
        assert_eq!(hash_parent_0, hash_parent_1);
        assert_eq!(hash_parent_0, hash_freshly_built);
        // and differing content stays distinct.
        assert_ne!(from_parent_0, different_node);
    }

    /// Leaf counterpart to `directory_identity_ignores_parent_index`: file
    /// identity must key on the leaf node alone, ignoring the parent index.
    #[mononoke::test]
    fn file_identity_ignores_parent_index() {
        // Given: one file node carried with two different parent indices and as a
        // freshly built (index-less) node, plus a different file node.
        let node = file_node(0x22, 0x44, 1000);
        let from_parent_0 = IndexedAugmentedFile {
            node: node.clone(),
            index: Some(ParentIndex(0)),
        };
        let from_parent_1 = IndexedAugmentedFile {
            node: node.clone(),
            index: Some(ParentIndex(1)),
        };
        let freshly_built = IndexedAugmentedFile {
            node: node.clone(),
            index: None,
        };
        let different_node = IndexedAugmentedFile {
            node: file_node(0x88, 0x44, 1000),
            index: Some(ParentIndex(0)),
        };

        // When: hashing each value (the operation dedup relies on).
        let hash_parent_0 = hash_value(&from_parent_0);
        let hash_parent_1 = hash_value(&from_parent_1);
        let hash_freshly_built = hash_value(&freshly_built);

        // Then: identical content compares and hashes equal regardless of index,
        assert_eq!(from_parent_0, from_parent_1);
        assert_eq!(from_parent_0, freshly_built);
        assert_eq!(hash_parent_0, hash_parent_1);
        assert_eq!(hash_parent_0, hash_freshly_built);
        // and differing content stays distinct.
        assert_ne!(from_parent_0, different_node);
    }

    /// The rich adapter must keep the full directory metadata (treenode,
    /// augmented_manifest_id, size, ACL pointer) that the shared lossy adapter
    /// discards, and stamp the originating parent index onto the entry.
    #[mononoke::test]
    fn indexed_entry_preserves_full_directory_node_and_parent_index() -> Result<()> {
        // Given: a stored directory subentry and a stored file subentry, with a
        // known originating parent index.
        let dir = dir_node(0x11, 0x33, 10);
        let file = file_node(0x22, 0x44, 1000);
        let index = Some(ParentIndex(2));

        // When: converting each stored subentry into a rich manifest entry.
        let dir_entry =
            indexed_augmented_entry(HgAugmentedManifestEntry::DirectoryNode(dir.clone()), index);
        let file_entry =
            indexed_augmented_entry(HgAugmentedManifestEntry::FileNode(file.clone()), index);

        // Then: the directory entry keeps the whole node (non-lossy) and the index,
        match dir_entry {
            Entry::Tree(IndexedAugmentedDirectory { node, index: got }) => {
                assert_eq!(node, dir, "directory node must round-trip without loss");
                assert_eq!(got, index, "directory entry must carry the parent index");
            }
            Entry::Leaf(_) => panic!("directory subentry must convert to a tree entry"),
        }
        // and the file entry keeps the whole leaf and the index.
        match file_entry {
            Entry::Leaf(IndexedAugmentedFile { node, index: got }) => {
                assert_eq!(node, file, "file node must round-trip without loss");
                assert_eq!(got, index, "file entry must carry the parent index");
            }
            Entry::Tree(_) => panic!("file subentry must convert to a leaf entry"),
        }
        Ok(())
    }

    async fn collect_entries_via_expand(
        ctx: &CoreContext,
        blobstore: &RepoBlobstore,
        trie: IndexedAugmentedTrieMap,
    ) -> Result<Vec<Entry<IndexedAugmentedDirectory, IndexedAugmentedFile>>> {
        let mut collected = Vec::new();
        let mut stack = vec![trie];
        while let Some(node) = stack.pop() {
            let (value, children) = node.expand(ctx, blobstore).await?;
            if let Some(entry) = value {
                collected.push(entry);
            }
            for (_byte, child) in children {
                stack.push(child);
            }
        }
        Ok(collected)
    }

    /// Loading an indexed directory must expose the same rich, sharded manifest
    /// view that `derive_manifest` will use: child entries keep their full
    /// augmented nodes and inherit the source parent index.
    #[mononoke::fbinit_test]
    async fn loaded_directory_into_trie_map_preserves_full_nodes_and_parent_index(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given: a stored augmented directory envelope with one child directory
        // and one child file, referenced through an indexed rich directory id.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let blobstore = repo.repo_blobstore();

        let child_dir = dir_node(0x11, 0x33, 10);
        let child_file = file_node(0x22, 0x44, 1000);
        let subentries = vec![
            (
                MPathElement::new_from_slice(b"inner")?,
                HgAugmentedManifestEntry::DirectoryNode(child_dir.clone()),
            ),
            (
                MPathElement::new_from_slice(b"file.txt")?,
                HgAugmentedManifestEntry::FileNode(child_file.clone()),
            ),
        ];
        let subentries = ShardedMapV2Node::from_entries(&ctx, blobstore, subentries).await?;
        let treenode = HgNodeHash::new(NodeSha1::from_byte_array([0x55; 20]));
        let augmented_manifest = ShardedHgAugmentedManifest {
            hg_node_id: treenode,
            p1: None,
            p2: None,
            computed_node_id: treenode,
            subentries,
            acl_manifest_directory_id: None,
        };
        let (augmented_manifest_id, augmented_manifest_size) = augmented_manifest
            .clone()
            .compute_content_addressed_digest(&ctx, blobstore)
            .await?;
        let envelope = HgAugmentedManifestEnvelope {
            augmented_manifest_id,
            augmented_manifest_size,
            augmented_manifest,
        };
        envelope.store(&ctx, blobstore).await?;
        let indexed_dir = IndexedAugmentedDirectory {
            node: HgAugmentedDirectoryNode {
                treenode,
                augmented_manifest_id,
                augmented_manifest_size,
                acl_manifest_directory_id: None,
            },
            index: Some(ParentIndex(1)),
        };

        // When: loading the rich directory and expanding its trie-map view.
        let loaded = indexed_dir.load(&ctx, blobstore).await?;
        let trie = loaded.into_trie_map(&ctx, blobstore).await?;
        let entries = collect_entries_via_expand(&ctx, blobstore, trie).await?;

        // Then: the loaded view yields both child entries, each with the parent
        // index, and without losing the child directory/file nodes.
        assert_eq!(entries.len(), 2, "both subentries must be yielded");
        for entry in &entries {
            let index = match entry {
                Entry::Tree(d) => d.index,
                Entry::Leaf(l) => l.index,
            };
            assert_eq!(
                index,
                Some(ParentIndex(1)),
                "every loaded entry must inherit the directory parent index",
            );
        }
        let dir_entry = entries
            .iter()
            .find_map(|e| match e {
                Entry::Tree(d) => Some(d),
                Entry::Leaf(_) => None,
            })
            .expect("expected a directory entry");
        assert_eq!(
            dir_entry.node, child_dir,
            "loaded directory entry must keep the full node metadata",
        );
        let file_entry = entries
            .iter()
            .find_map(|e| match e {
                Entry::Tree(_) => None,
                Entry::Leaf(f) => Some(f),
            })
            .expect("expected a file entry");
        assert_eq!(
            file_entry.node, child_file,
            "loaded file entry must keep the full leaf metadata",
        );
        Ok(())
    }

    /// Expanding a reused/spliced partial map must propagate the parent index to
    /// every child entry (mirrors `Traced::inherit_into_entry`) and keep the full
    /// directory metadata, so a rebuilt parent can embed reused children and
    /// recover their p1/p2 without re-fetching.
    #[mononoke::fbinit_test]
    async fn expand_propagates_parent_index_and_full_node_to_children(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given: a sharded map holding one directory and one file subentry,
        // wrapped so it carries parent index 2.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let blobstore = repo.repo_blobstore();

        let dir = dir_node(0x11, 0x33, 10);
        let file = file_node(0x22, 0x44, 1000);
        let subentries = vec![
            (
                MPathElement::new_from_slice(b"inner")?,
                HgAugmentedManifestEntry::DirectoryNode(dir.clone()),
            ),
            (
                MPathElement::new_from_slice(b"file.txt")?,
                HgAugmentedManifestEntry::FileNode(file.clone()),
            ),
        ];
        let map = ShardedMapV2Node::from_entries(&ctx, blobstore, subentries).await?;
        let trie = IndexedAugmentedTrieMap::new(
            LoadableShardedMapV2Node::Inlined(map),
            Some(ParentIndex(2)),
        );

        // When: fully expanding the wrapped map through `TrieMapOps::expand`.
        let entries = collect_entries_via_expand(&ctx, blobstore, trie).await?;

        // Then: both subentries surface, each stamped with parent index 2, and
        // the directory entry still carries the full node.
        assert_eq!(entries.len(), 2, "both subentries must be yielded");
        for entry in &entries {
            let index = match entry {
                Entry::Tree(d) => d.index,
                Entry::Leaf(l) => l.index,
            };
            assert_eq!(
                index,
                Some(ParentIndex(2)),
                "every expanded entry must inherit the parent index",
            );
        }
        let dir_entry = entries
            .iter()
            .find_map(|e| match e {
                Entry::Tree(d) => Some(d),
                Entry::Leaf(_) => None,
            })
            .expect("expected a directory entry");
        assert_eq!(
            dir_entry.node, dir,
            "expanded directory entry must keep the full node metadata",
        );
        Ok(())
    }
}
