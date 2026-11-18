/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Arc;

use anyhow::Error;
use anyhow::Result;
use async_trait::async_trait;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use blobstore::LoadableError;
use blobstore::Storable;
use bounded_traversal::bounded_traversal_stream;
use cloned::cloned;
use context::CoreContext;
use futures::future;
use futures::future::FutureExt;
use futures::stream;
use futures::stream::BoxStream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use mononoke_types::BlobstoreBytes;
use mononoke_types::ChangesetId;
use mononoke_types::FileType;
use mononoke_types::MPathElement;
use mononoke_types::NonRootMPath;
use mononoke_types::SortedVectorTrieMap;
use mononoke_types::path::MPath;
use serde_derive::Deserialize;
use serde_derive::Serialize;

pub(crate) use crate::Entry;
pub(crate) use crate::Manifest;
pub(crate) use crate::OrderedManifest;
pub(crate) use crate::TreeInfo;
pub(crate) use crate::derive_batch::ManifestChanges;
pub(crate) use crate::derive_batch::derive_manifests_for_simple_stack_of_commits;
pub(crate) use crate::derive_manifest_with_known_entries;
pub(crate) use crate::flatten_subentries;
pub(crate) use crate::types::Weight;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct TestLeafId(u64);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TestLeaf(String);

impl TestLeaf {
    pub(crate) fn new(s: &str) -> Self {
        Self(s.to_string())
    }
}

#[async_trait]
impl Loadable for TestLeafId {
    type Value = TestLeaf;

    async fn load<'a, B: KeyedBlobstore>(
        &'a self,
        ctx: &'a CoreContext,
        blobstore: &'a B,
    ) -> Result<Self::Value, LoadableError> {
        let key = self.0.to_string();
        let bytes = blobstore
            .get(ctx, &key)
            .await?
            .ok_or(LoadableError::Missing(key))?;
        let bytes = std::str::from_utf8(bytes.as_raw_bytes())
            .map_err(|err| LoadableError::Error(Error::from(err)))?;
        Ok(TestLeaf(bytes.to_owned()))
    }
}

#[async_trait]
impl Storable for TestLeaf {
    type Key = TestLeafId;

    async fn store<'a, B: KeyedBlobstore>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a B,
    ) -> Result<Self::Key> {
        let mut hasher = DefaultHasher::new();
        self.0.hash(&mut hasher);
        let key = TestLeafId(hasher.finish());
        blobstore
            .put(ctx, key.0.to_string(), BlobstoreBytes::from_bytes(self.0))
            .await?;
        Ok(key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct TestManifestId(u64);

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct TestManifest(
    pub(crate) BTreeMap<MPathElement, Entry<TestManifestId, (FileType, TestLeafId)>>,
);

impl TestManifest {
    pub(crate) fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub(crate) fn insert(
        mut self,
        elem: &str,
        entry: Entry<TestManifestId, (FileType, TestLeafId)>,
    ) -> Self {
        self.0.insert(
            MPathElement::new_from_slice(elem.as_bytes()).unwrap(),
            entry,
        );
        self
    }
}

#[async_trait]
impl Loadable for TestManifestId {
    type Value = TestManifest;

    async fn load<'a, B: KeyedBlobstore>(
        &'a self,
        ctx: &'a CoreContext,
        blobstore: &'a B,
    ) -> Result<Self::Value, LoadableError> {
        let key = self.0.to_string();
        let get = blobstore.get(ctx, &key);
        let data = get.await?;
        let bytes = data.ok_or(LoadableError::Missing(key))?;
        let mf = serde_cbor::from_slice(bytes.as_raw_bytes().as_ref())
            .map_err(|err| LoadableError::Error(Error::from(err)))?;
        Ok(mf)
    }
}

#[async_trait]
impl Storable for TestManifest {
    type Key = TestManifestId;

    async fn store<'a, B: KeyedBlobstore>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a B,
    ) -> Result<Self::Key> {
        let mut hasher = DefaultHasher::new();
        self.0.hash(&mut hasher);
        let key = TestManifestId(hasher.finish());

        let bytes = serde_cbor::to_vec(&self)?;
        blobstore
            .put(ctx, key.0.to_string(), BlobstoreBytes::from_bytes(bytes))
            .await?;
        Ok(key)
    }
}

#[async_trait]
impl<Store: KeyedBlobstore> Manifest<Store> for TestManifest {
    type Leaf = (FileType, TestLeafId);
    type TreeId = TestManifestId;
    type TrieMapType = SortedVectorTrieMap<Entry<Self::TreeId, Self::Leaf>>;

    async fn lookup(
        &self,
        _ctx: &CoreContext,
        _blobstore: &Store,
        name: &MPathElement,
    ) -> Result<Option<Entry<Self::TreeId, Self::Leaf>>> {
        Ok(self.0.get(name).cloned())
    }

    async fn list(
        &self,
        _ctx: &CoreContext,
        _blobstore: &Store,
    ) -> Result<BoxStream<'async_trait, Result<(MPathElement, Entry<Self::TreeId, Self::Leaf>)>>>
    {
        Ok(stream::iter(self.0.clone()).map(Ok).boxed())
    }

    async fn into_trie_map(
        self,
        _ctx: &CoreContext,
        _blobstore: &Store,
    ) -> Result<Self::TrieMapType> {
        let entries = self
            .0
            .iter()
            .map(|(k, v)| (k.clone().to_smallvec(), v.clone()))
            .collect();
        Ok(SortedVectorTrieMap::new(entries))
    }
}

// Implement OrderedManifests for tests.  Ideally we'd need to know the
// descendant count, but since we don't, just make something up (10).  The
// code still works if this estimate is wrong, but it may schedule the
// wrong number of child manifest expansions when this happens.
#[async_trait]
impl<Store: KeyedBlobstore> OrderedManifest<Store> for TestManifest {
    type WeightedTrieMapType = SortedVectorTrieMap<Entry<(Weight, Self::TreeId), Self::Leaf>>;

    async fn into_weighted_trie_map(
        self,
        _ctx: &CoreContext,
        _blobstore: &Store,
    ) -> Result<Self::WeightedTrieMapType> {
        let entries = self
            .0
            .iter()
            .map(|(k, v)| {
                let entry = match v.clone() {
                    Entry::Leaf(leaf) => Entry::Leaf(leaf),
                    Entry::Tree(tree) => Entry::Tree((10, tree)),
                };
                (k.clone().to_smallvec(), entry)
            })
            .collect();
        Ok(SortedVectorTrieMap::new(entries))
    }

    async fn list_weighted(
        &self,
        _ctx: &CoreContext,
        _blobstore: &Store,
    ) -> Result<
        BoxStream<'async_trait, Result<(MPathElement, Entry<(Weight, Self::TreeId), Self::Leaf>)>>,
    > {
        let iter = self.0.clone().into_iter().map(|entry| match entry {
            (elem, Entry::Leaf(leaf)) => (elem, Entry::Leaf(leaf)),
            (elem, Entry::Tree(tree)) => (elem, Entry::Tree((10, tree))),
        });
        Ok(stream::iter(iter).map(Ok).boxed())
    }

    async fn lookup_weighted(
        &self,
        _ctx: &CoreContext,
        _blobstore: &Store,
        name: &MPathElement,
    ) -> Result<Option<Entry<(Weight, Self::TreeId), Self::Leaf>>> {
        Ok(self.0.get(name).cloned().map(|entry| match entry {
            Entry::Leaf(leaf) => Entry::Leaf(leaf),
            Entry::Tree(tree) => Entry::Tree((10, tree)),
        }))
    }
}

pub(crate) async fn derive_test_manifest(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    parents: Vec<TestManifestId>,
    changes_str: BTreeMap<&str, Option<&str>>,
) -> Result<Option<TestManifestId>> {
    derive_test_manifest_with_known_entries(
        ctx,
        blobstore,
        parents.into_iter().map(Entry::Tree).collect(),
        changes_str,
        HashMap::new(),
        MPath::ROOT,
    )
    .await
    .map(|opt| opt.and_then(|entry| entry.into_tree()))
}

pub(crate) async fn derive_stack_of_test_manifests(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    parent: Option<TestManifestId>,
    changes_str_per_cs_id: BTreeMap<ChangesetId, BTreeMap<&str, Option<&str>>>,
) -> Result<BTreeMap<ChangesetId, TestManifestId>> {
    let mut stack_of_commits = vec![];
    let mut all_mf_changes = vec![];
    for (cs_id, changes_str) in changes_str_per_cs_id {
        let mut changes = Vec::new();
        for (path, change) in changes_str {
            let path = NonRootMPath::new(path)?;
            changes.push((path, change.map(|leaf| TestLeaf(leaf.to_string()))));
        }
        let mf_changes = ManifestChanges { cs_id, changes };
        all_mf_changes.push(mf_changes);
        stack_of_commits.push(cs_id);
    }

    let derived = derive_manifests_for_simple_stack_of_commits(
        ctx.clone(),
        blobstore.clone(),
        parent,
        all_mf_changes,
        {
            cloned!(ctx, blobstore);
            move |TreeInfo { subentries, .. }, _| {
                cloned!(ctx, blobstore);
                async move {
                    let id = TestManifest(
                        flatten_subentries(&ctx, &(), subentries)
                            .await?
                            .map(|(path, (_ctx, entry))| (path, entry))
                            .collect(),
                    )
                    .store(&ctx, &blobstore)
                    .await?;
                    Ok(((), id))
                }
            }
        },
        {
            cloned!(ctx, blobstore);
            move |leaf_info, _| {
                cloned!(ctx, blobstore);
                async move {
                    match leaf_info.change {
                        None => Err(Error::msg("leaf only conflict")),
                        Some(change) => {
                            let id = change.store(&ctx, &blobstore).await?;
                            Ok(((), (FileType::Regular, id)))
                        }
                    }
                }
            }
        },
    )
    .await?;

    let mut res = BTreeMap::new();
    for cs_id in stack_of_commits {
        res.insert(cs_id, derived.get(&cs_id).unwrap().clone());
    }
    Ok(res)
}

pub(crate) async fn list_test_manifest<'a, B: KeyedBlobstore>(
    ctx: &'a CoreContext,
    blobstore: &'a B,
    mf_id: TestManifestId,
) -> Result<BTreeMap<NonRootMPath, String>, LoadableError> {
    bounded_traversal_stream(
        256,
        Some((MPath::ROOT, Entry::<_, (FileType, TestLeafId)>::Tree(mf_id))),
        move |(path, entry)| {
            async move {
                Ok(match entry {
                    Entry::Leaf(leaf_id) => {
                        let leaf = leaf_id.load(ctx, blobstore).await?;
                        (Some((path, leaf)), Vec::new())
                    }
                    Entry::Tree(tree_id) => {
                        let tree = tree_id.load(ctx, blobstore).await?;
                        let recurse = tree
                            .list(ctx, blobstore)
                            .await?
                            .map_ok(|(name, entry)| (path.join(&name), entry))
                            .try_collect()
                            .await?;
                        (None, recurse)
                    }
                })
            }
            .boxed()
        },
    )
    .try_filter_map(|item| {
        let item =
            item.and_then(|(path, leaf)| Some((Option::<NonRootMPath>::from(path)?, leaf.1.0)));
        future::ok(item)
    })
    .try_fold(BTreeMap::new(), |mut acc, (path, leaf)| {
        acc.insert(path, leaf);
        future::ok::<_, LoadableError>(acc)
    })
    .await
}

pub(crate) async fn derive_test_manifest_with_known_entries(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    parents: Vec<Entry<TestManifestId, (FileType, TestLeafId)>>,
    changes_str: BTreeMap<&str, Option<&str>>,
    known_entries: HashMap<MPath, Option<Entry<TestManifestId, (FileType, TestLeafId)>>>,
    prefix: MPath,
) -> Result<Option<Entry<TestManifestId, (FileType, TestLeafId)>>> {
    let changes = {
        let mut changes = Vec::new();
        for (path, change) in changes_str {
            let path = NonRootMPath::new(path)?;
            changes.push((path, change.map(|leaf| TestLeaf(leaf.to_string()))));
        }
        changes
    };
    derive_manifest_with_known_entries(
        ctx.clone(),
        blobstore.clone(),
        parents,
        changes,
        known_entries
            .into_iter()
            .map(|(path, entry)| (path, crate::KnownEntry::Entry(entry)))
            .collect(),
        prefix,
        {
            cloned!(ctx, blobstore);
            move |TreeInfo { subentries, .. }| {
                cloned!(ctx, blobstore);
                async move {
                    let id = TestManifest(
                        flatten_subentries(&ctx, &(), subentries)
                            .await?
                            .map(|(path, (_ctx, entry))| (path, entry))
                            .collect(),
                    )
                    .store(&ctx, &blobstore)
                    .await?;
                    Ok(((), id))
                }
            }
        },
        {
            cloned!(ctx, blobstore);
            move |leaf_info| {
                cloned!(ctx, blobstore);
                async move {
                    match leaf_info.change {
                        None => Err(Error::msg("leaf only conflict")),
                        Some(change) => {
                            let id = change.store(&ctx, &blobstore).await?;
                            Ok(((), (FileType::Regular, id)))
                        }
                    }
                }
            }
        },
    )
    .await
}
