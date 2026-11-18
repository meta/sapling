/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use async_trait::async_trait;
use blobstore::KeyedBlobstore;
use context::CoreContext;
use futures::stream;
use futures::stream::BoxStream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use mononoke_types::ContentManifestId;
use mononoke_types::SortedVectorTrieMap;
use mononoke_types::TrieMap;
use mononoke_types::acl_manifest::AclManifestEntry;
use mononoke_types::acl_manifest::AclManifestRestriction;
use mononoke_types::basename_suffix_skeleton_manifest_v3::BssmV3Directory;
use mononoke_types::basename_suffix_skeleton_manifest_v3::BssmV3Entry;
use mononoke_types::case_conflict_skeleton_manifest::CaseConflictSkeletonManifest;
use mononoke_types::case_conflict_skeleton_manifest::CcsmEntry;
use mononoke_types::content_manifest::ContentManifestEntry;
use mononoke_types::content_manifest::ContentManifestFile;
use mononoke_types::directory_branch_cluster_manifest::DirectoryBranchClusterManifest;
use mononoke_types::directory_branch_cluster_manifest::DirectoryBranchClusterManifestEntry;
use mononoke_types::directory_branch_cluster_manifest::DirectoryBranchClusterManifestFile;
use mononoke_types::history_manifest::HistoryManifestEntry;
use mononoke_types::sharded_map_v2::LoadableShardedMapV2Node;
use mononoke_types::skeleton_manifest_v2::SkeletonManifestV2;
use mononoke_types::skeleton_manifest_v2::SkeletonManifestV2Entry;
use mononoke_types::test_sharded_manifest::TestShardedManifestDirectory;
use mononoke_types::test_sharded_manifest::TestShardedManifestEntry;
use mononoke_types::typed_hash::AclManifestId;
use mononoke_types::typed_hash::HistoryManifestDirectoryId;
use mononoke_types::typed_hash::HistoryManifestFileId;
use smallvec::SmallVec;

use crate::types::Entry;
use crate::types::Weight;
use crate::types::history_manifest_to_mf_entry;

#[async_trait]
pub trait TrieMapOps<Store, Value>: Sized {
    async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<(Option<Value>, Vec<(u8, Self)>)>;

    async fn into_stream(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<BoxStream<'async_trait, Result<(SmallVec<[u8; 24]>, Value)>>>;

    /// Look up names relative to this trie map, omitting missing entries.
    async fn lookup_many(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
        names: Vec<SmallVec<[u8; 24]>>,
    ) -> Result<Vec<(SmallVec<[u8; 24]>, Value)>>
    where
        Store: Sync,
        Value: Clone + Send;

    fn is_empty(&self) -> bool;
}

#[async_trait]
impl<Store, V: Send> TrieMapOps<Store, V> for TrieMap<V> {
    async fn expand(
        self,
        _ctx: &CoreContext,
        _blobstore: &Store,
    ) -> Result<(Option<V>, Vec<(u8, Self)>)> {
        Ok(self.expand())
    }

    async fn into_stream(
        self,
        _ctx: &CoreContext,
        _blobstore: &Store,
    ) -> Result<BoxStream<'async_trait, Result<(SmallVec<[u8; 24]>, V)>>> {
        Ok(stream::iter(self).map(Ok).boxed())
    }

    async fn lookup_many(
        self,
        _ctx: &CoreContext,
        _blobstore: &Store,
        names: Vec<SmallVec<[u8; 24]>>,
    ) -> Result<Vec<(SmallVec<[u8; 24]>, V)>>
    where
        Store: Sync,
        V: Clone + Send,
    {
        Ok(names
            .into_iter()
            .filter_map(|name| self.get(&name).cloned().map(|entry| (name, entry)))
            .collect())
    }

    fn is_empty(&self) -> bool {
        self.is_empty()
    }
}

#[async_trait]
impl<Store, V: Clone + Send + Sync> TrieMapOps<Store, V> for SortedVectorTrieMap<V> {
    async fn expand(
        self,
        _ctx: &CoreContext,
        _blobstore: &Store,
    ) -> Result<(Option<V>, Vec<(u8, Self)>)> {
        SortedVectorTrieMap::expand(self)
    }

    async fn into_stream(
        self,
        _ctx: &CoreContext,
        _blobstore: &Store,
    ) -> Result<BoxStream<'async_trait, Result<(SmallVec<[u8; 24]>, V)>>> {
        Ok(stream::iter(self).map(Ok).boxed())
    }

    async fn lookup_many(
        self,
        _ctx: &CoreContext,
        _blobstore: &Store,
        names: Vec<SmallVec<[u8; 24]>>,
    ) -> Result<Vec<(SmallVec<[u8; 24]>, V)>>
    where
        Store: Sync,
        V: Clone + Send,
    {
        Ok(names
            .into_iter()
            .filter_map(|name| self.get(&name).cloned().map(|entry| (name, entry)))
            .collect())
    }

    fn is_empty(&self) -> bool {
        SortedVectorTrieMap::is_empty(self)
    }
}

#[async_trait]
impl<Store: KeyedBlobstore> TrieMapOps<Store, Entry<TestShardedManifestDirectory, ()>>
    for LoadableShardedMapV2Node<TestShardedManifestEntry>
{
    async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<(
        Option<Entry<TestShardedManifestDirectory, ()>>,
        Vec<(u8, Self)>,
    )> {
        let (entry, children) = self.expand(ctx, blobstore).await?;
        Ok((
            entry.map(crate::types::convert_test_sharded_manifest),
            children,
        ))
    }

    async fn into_stream(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<
        BoxStream<
            'async_trait,
            Result<(SmallVec<[u8; 24]>, Entry<TestShardedManifestDirectory, ()>)>,
        >,
    > {
        Ok(self
            .load(ctx, blobstore)
            .await?
            .into_entries(ctx, blobstore)
            .map_ok(|(k, v)| (k, crate::types::convert_test_sharded_manifest(v)))
            .boxed())
    }

    async fn lookup_many(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
        names: Vec<SmallVec<[u8; 24]>>,
    ) -> Result<Vec<(SmallVec<[u8; 24]>, Entry<TestShardedManifestDirectory, ()>)>> {
        Ok(
            LoadableShardedMapV2Node::lookup_many(self, ctx, blobstore, names)
                .await?
                .into_iter()
                .map(|(name, entry)| (name, crate::types::convert_test_sharded_manifest(entry)))
                .collect(),
        )
    }

    fn is_empty(&self) -> bool {
        self.size() == 0
    }
}

#[async_trait]
impl<Store: KeyedBlobstore> TrieMapOps<Store, Entry<BssmV3Directory, ()>>
    for LoadableShardedMapV2Node<BssmV3Entry>
{
    async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<(Option<Entry<BssmV3Directory, ()>>, Vec<(u8, Self)>)> {
        let (entry, children) = self.expand(ctx, blobstore).await?;
        Ok((entry.map(crate::types::bssm_v3_to_mf_entry), children))
    }

    async fn into_stream(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<BoxStream<'async_trait, Result<(SmallVec<[u8; 24]>, Entry<BssmV3Directory, ()>)>>>
    {
        Ok(self
            .load(ctx, blobstore)
            .await?
            .into_entries(ctx, blobstore)
            .map_ok(|(k, v)| (k, crate::types::bssm_v3_to_mf_entry(v)))
            .boxed())
    }

    async fn lookup_many(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
        names: Vec<SmallVec<[u8; 24]>>,
    ) -> Result<Vec<(SmallVec<[u8; 24]>, Entry<BssmV3Directory, ()>)>> {
        Ok(
            LoadableShardedMapV2Node::lookup_many(self, ctx, blobstore, names)
                .await?
                .into_iter()
                .map(|(name, entry)| (name, crate::types::bssm_v3_to_mf_entry(entry)))
                .collect(),
        )
    }

    fn is_empty(&self) -> bool {
        self.size() == 0
    }
}

#[async_trait]
impl<Store: KeyedBlobstore> TrieMapOps<Store, Entry<SkeletonManifestV2, ()>>
    for LoadableShardedMapV2Node<SkeletonManifestV2Entry>
{
    async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<(Option<Entry<SkeletonManifestV2, ()>>, Vec<(u8, Self)>)> {
        let (entry, children) = self.expand(ctx, blobstore).await?;
        Ok((
            entry.map(crate::types::skeleton_manifest_v2_to_mf_entry),
            children,
        ))
    }

    async fn into_stream(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<BoxStream<'async_trait, Result<(SmallVec<[u8; 24]>, Entry<SkeletonManifestV2, ()>)>>>
    {
        Ok(self
            .load(ctx, blobstore)
            .await?
            .into_entries(ctx, blobstore)
            .map_ok(|(k, v)| (k, crate::types::skeleton_manifest_v2_to_mf_entry(v)))
            .boxed())
    }

    async fn lookup_many(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
        names: Vec<SmallVec<[u8; 24]>>,
    ) -> Result<Vec<(SmallVec<[u8; 24]>, Entry<SkeletonManifestV2, ()>)>> {
        Ok(
            LoadableShardedMapV2Node::lookup_many(self, ctx, blobstore, names)
                .await?
                .into_iter()
                .map(|(name, entry)| (name, crate::types::skeleton_manifest_v2_to_mf_entry(entry)))
                .collect(),
        )
    }

    fn is_empty(&self) -> bool {
        self.size() == 0
    }
}

#[async_trait]
impl<Store: KeyedBlobstore> TrieMapOps<Store, Entry<CaseConflictSkeletonManifest, ()>>
    for LoadableShardedMapV2Node<CcsmEntry>
{
    async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<(
        Option<Entry<CaseConflictSkeletonManifest, ()>>,
        Vec<(u8, Self)>,
    )> {
        let (entry, children) = self.expand(ctx, blobstore).await?;
        Ok((entry.map(crate::types::ccsm_to_mf_entry), children))
    }

    async fn into_stream(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<
        BoxStream<
            'async_trait,
            Result<(SmallVec<[u8; 24]>, Entry<CaseConflictSkeletonManifest, ()>)>,
        >,
    > {
        Ok(self
            .load(ctx, blobstore)
            .await?
            .into_entries(ctx, blobstore)
            .map_ok(|(k, v)| (k, crate::types::ccsm_to_mf_entry(v)))
            .boxed())
    }

    async fn lookup_many(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
        names: Vec<SmallVec<[u8; 24]>>,
    ) -> Result<Vec<(SmallVec<[u8; 24]>, Entry<CaseConflictSkeletonManifest, ()>)>> {
        Ok(
            LoadableShardedMapV2Node::lookup_many(self, ctx, blobstore, names)
                .await?
                .into_iter()
                .map(|(name, entry)| (name, crate::types::ccsm_to_mf_entry(entry)))
                .collect(),
        )
    }

    fn is_empty(&self) -> bool {
        self.size() == 0
    }
}

#[async_trait]
impl<Store: KeyedBlobstore> TrieMapOps<Store, Entry<ContentManifestId, ContentManifestFile>>
    for LoadableShardedMapV2Node<ContentManifestEntry>
{
    async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<(
        Option<Entry<ContentManifestId, ContentManifestFile>>,
        Vec<(u8, Self)>,
    )> {
        let (entry, children) = self.expand(ctx, blobstore).await?;
        Ok((entry.map(crate::types::convert_content_manifest), children))
    }

    async fn into_stream(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<
        BoxStream<
            'async_trait,
            Result<(
                SmallVec<[u8; 24]>,
                Entry<ContentManifestId, ContentManifestFile>,
            )>,
        >,
    > {
        Ok(self
            .load(ctx, blobstore)
            .await?
            .into_entries(ctx, blobstore)
            .map_ok(|(k, v)| (k, crate::types::convert_content_manifest(v)))
            .boxed())
    }

    async fn lookup_many(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
        names: Vec<SmallVec<[u8; 24]>>,
    ) -> Result<
        Vec<(
            SmallVec<[u8; 24]>,
            Entry<ContentManifestId, ContentManifestFile>,
        )>,
    > {
        Ok(
            LoadableShardedMapV2Node::lookup_many(self, ctx, blobstore, names)
                .await?
                .into_iter()
                .map(|(name, entry)| (name, crate::types::convert_content_manifest(entry)))
                .collect(),
        )
    }

    fn is_empty(&self) -> bool {
        self.size() == 0
    }
}

#[async_trait]
impl<Store: KeyedBlobstore>
    TrieMapOps<Store, Entry<DirectoryBranchClusterManifest, DirectoryBranchClusterManifestFile>>
    for LoadableShardedMapV2Node<DirectoryBranchClusterManifestEntry>
{
    async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<(
        Option<Entry<DirectoryBranchClusterManifest, DirectoryBranchClusterManifestFile>>,
        Vec<(u8, Self)>,
    )> {
        let (entry, children) = self.expand(ctx, blobstore).await?;
        Ok((entry.map(crate::types::dbcm_to_mf_entry), children))
    }

    async fn into_stream(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<
        BoxStream<
            'async_trait,
            Result<(
                SmallVec<[u8; 24]>,
                Entry<DirectoryBranchClusterManifest, DirectoryBranchClusterManifestFile>,
            )>,
        >,
    > {
        Ok(self
            .load(ctx, blobstore)
            .await?
            .into_entries(ctx, blobstore)
            .map_ok(|(k, v)| (k, crate::types::dbcm_to_mf_entry(v)))
            .boxed())
    }

    async fn lookup_many(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
        names: Vec<SmallVec<[u8; 24]>>,
    ) -> Result<
        Vec<(
            SmallVec<[u8; 24]>,
            Entry<DirectoryBranchClusterManifest, DirectoryBranchClusterManifestFile>,
        )>,
    > {
        Ok(
            LoadableShardedMapV2Node::lookup_many(self, ctx, blobstore, names)
                .await?
                .into_iter()
                .map(|(name, entry)| (name, crate::types::dbcm_to_mf_entry(entry)))
                .collect(),
        )
    }

    fn is_empty(&self) -> bool {
        self.size() == 0
    }
}

#[async_trait]
impl<Store: KeyedBlobstore>
    TrieMapOps<Store, Entry<HistoryManifestDirectoryId, HistoryManifestFileId>>
    for LoadableShardedMapV2Node<HistoryManifestEntry>
{
    async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<(
        Option<Entry<HistoryManifestDirectoryId, HistoryManifestFileId>>,
        Vec<(u8, Self)>,
    )> {
        let (entry, children) = self.expand(ctx, blobstore).await?;
        Ok((entry.and_then(history_manifest_to_mf_entry), children))
    }

    async fn into_stream(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<
        BoxStream<
            'async_trait,
            Result<(
                SmallVec<[u8; 24]>,
                Entry<HistoryManifestDirectoryId, HistoryManifestFileId>,
            )>,
        >,
    > {
        Ok(self
            .load(ctx, blobstore)
            .await?
            .into_entries(ctx, blobstore)
            .try_filter_map(|(key, entry)| async move {
                Ok(history_manifest_to_mf_entry(entry).map(|entry| (key, entry)))
            })
            .boxed())
    }

    async fn lookup_many(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
        names: Vec<SmallVec<[u8; 24]>>,
    ) -> Result<
        Vec<(
            SmallVec<[u8; 24]>,
            Entry<HistoryManifestDirectoryId, HistoryManifestFileId>,
        )>,
    > {
        Ok(
            LoadableShardedMapV2Node::lookup_many(self, ctx, blobstore, names)
                .await?
                .into_iter()
                .filter_map(|(name, entry)| {
                    history_manifest_to_mf_entry(entry).map(|entry| (name, entry))
                })
                .collect(),
        )
    }

    fn is_empty(&self) -> bool {
        // A nonempty shard may contain only deleted entries. Checking that
        // requires loading it, so only prune shards known to be empty here.
        self.size() == 0
    }
}

#[async_trait]
impl<Store: KeyedBlobstore> TrieMapOps<Store, Entry<AclManifestId, AclManifestRestriction>>
    for LoadableShardedMapV2Node<AclManifestEntry>
{
    async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<(
        Option<Entry<AclManifestId, AclManifestRestriction>>,
        Vec<(u8, Self)>,
    )> {
        let (entry, children) = self.expand(ctx, blobstore).await?;
        Ok((entry.map(crate::types::convert_acl_manifest), children))
    }

    async fn into_stream(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<
        BoxStream<
            'async_trait,
            Result<(
                SmallVec<[u8; 24]>,
                Entry<AclManifestId, AclManifestRestriction>,
            )>,
        >,
    > {
        Ok(self
            .load(ctx, blobstore)
            .await?
            .into_entries(ctx, blobstore)
            .map_ok(|(k, v)| (k, crate::types::convert_acl_manifest(v)))
            .boxed())
    }

    async fn lookup_many(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
        names: Vec<SmallVec<[u8; 24]>>,
    ) -> Result<
        Vec<(
            SmallVec<[u8; 24]>,
            Entry<AclManifestId, AclManifestRestriction>,
        )>,
    > {
        Ok(
            LoadableShardedMapV2Node::lookup_many(self, ctx, blobstore, names)
                .await?
                .into_iter()
                .map(|(name, entry)| (name, crate::types::convert_acl_manifest(entry)))
                .collect(),
        )
    }

    fn is_empty(&self) -> bool {
        self.size() == 0
    }
}

#[async_trait]
impl<Store: KeyedBlobstore> TrieMapOps<Store, Entry<(Weight, BssmV3Directory), ()>>
    for LoadableShardedMapV2Node<BssmV3Entry>
{
    async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<(
        Option<Entry<(Weight, BssmV3Directory), ()>>,
        Vec<(u8, Self)>,
    )> {
        let (entry, children) = self.expand(ctx, blobstore).await?;
        Ok((
            entry.map(|entry| {
                crate::types::convert_bssm_v3_to_weighted(crate::types::bssm_v3_to_mf_entry(entry))
            }),
            children,
        ))
    }

    async fn into_stream(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<
        BoxStream<'async_trait, Result<(SmallVec<[u8; 24]>, Entry<(Weight, BssmV3Directory), ()>)>>,
    > {
        Ok(self
            .load(ctx, blobstore)
            .await?
            .into_entries(ctx, blobstore)
            .map_ok(|(k, v)| {
                (
                    k,
                    crate::types::convert_bssm_v3_to_weighted(crate::types::bssm_v3_to_mf_entry(v)),
                )
            })
            .boxed())
    }

    async fn lookup_many(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
        names: Vec<SmallVec<[u8; 24]>>,
    ) -> Result<Vec<(SmallVec<[u8; 24]>, Entry<(Weight, BssmV3Directory), ()>)>> {
        Ok(
            LoadableShardedMapV2Node::lookup_many(self, ctx, blobstore, names)
                .await?
                .into_iter()
                .map(|(name, entry)| {
                    (
                        name,
                        crate::types::convert_bssm_v3_to_weighted(
                            crate::types::bssm_v3_to_mf_entry(entry),
                        ),
                    )
                })
                .collect(),
        )
    }

    fn is_empty(&self) -> bool {
        self.size() == 0
    }
}

#[async_trait]
impl<Store: KeyedBlobstore> TrieMapOps<Store, Entry<(Weight, CaseConflictSkeletonManifest), ()>>
    for LoadableShardedMapV2Node<CcsmEntry>
{
    async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<(
        Option<Entry<(Weight, CaseConflictSkeletonManifest), ()>>,
        Vec<(u8, Self)>,
    )> {
        let (entry, children) = self.expand(ctx, blobstore).await?;
        Ok((
            entry.map(|entry| {
                crate::types::convert_ccsm_to_weighted(crate::types::ccsm_to_mf_entry(entry))
            }),
            children,
        ))
    }

    async fn into_stream(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<
        BoxStream<
            'async_trait,
            Result<(
                SmallVec<[u8; 24]>,
                Entry<(Weight, CaseConflictSkeletonManifest), ()>,
            )>,
        >,
    > {
        Ok(self
            .load(ctx, blobstore)
            .await?
            .into_entries(ctx, blobstore)
            .map_ok(|(k, v)| {
                (
                    k,
                    crate::types::convert_ccsm_to_weighted(crate::types::ccsm_to_mf_entry(v)),
                )
            })
            .boxed())
    }

    async fn lookup_many(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
        names: Vec<SmallVec<[u8; 24]>>,
    ) -> Result<
        Vec<(
            SmallVec<[u8; 24]>,
            Entry<(Weight, CaseConflictSkeletonManifest), ()>,
        )>,
    > {
        Ok(
            LoadableShardedMapV2Node::lookup_many(self, ctx, blobstore, names)
                .await?
                .into_iter()
                .map(|(name, entry)| {
                    (
                        name,
                        crate::types::convert_ccsm_to_weighted(crate::types::ccsm_to_mf_entry(
                            entry,
                        )),
                    )
                })
                .collect(),
        )
    }

    fn is_empty(&self) -> bool {
        self.size() == 0
    }
}

#[async_trait]
impl<Store: KeyedBlobstore> TrieMapOps<Store, Entry<(Weight, SkeletonManifestV2), ()>>
    for LoadableShardedMapV2Node<SkeletonManifestV2Entry>
{
    async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<(
        Option<Entry<(Weight, SkeletonManifestV2), ()>>,
        Vec<(u8, Self)>,
    )> {
        let (entry, children) = self.expand(ctx, blobstore).await?;
        Ok((
            entry.map(|entry| {
                crate::types::convert_skeleton_manifest_v2_to_weighted(
                    crate::types::skeleton_manifest_v2_to_mf_entry(entry),
                )
            }),
            children,
        ))
    }

    async fn into_stream(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<
        BoxStream<
            'async_trait,
            Result<(SmallVec<[u8; 24]>, Entry<(Weight, SkeletonManifestV2), ()>)>,
        >,
    > {
        Ok(self
            .load(ctx, blobstore)
            .await?
            .into_entries(ctx, blobstore)
            .map_ok(|(k, v)| {
                (
                    k,
                    crate::types::convert_skeleton_manifest_v2_to_weighted(
                        crate::types::skeleton_manifest_v2_to_mf_entry(v),
                    ),
                )
            })
            .boxed())
    }

    async fn lookup_many(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
        names: Vec<SmallVec<[u8; 24]>>,
    ) -> Result<Vec<(SmallVec<[u8; 24]>, Entry<(Weight, SkeletonManifestV2), ()>)>> {
        Ok(
            LoadableShardedMapV2Node::lookup_many(self, ctx, blobstore, names)
                .await?
                .into_iter()
                .map(|(name, entry)| {
                    (
                        name,
                        crate::types::convert_skeleton_manifest_v2_to_weighted(
                            crate::types::skeleton_manifest_v2_to_mf_entry(entry),
                        ),
                    )
                })
                .collect(),
        )
    }

    fn is_empty(&self) -> bool {
        self.size() == 0
    }
}

#[async_trait]
impl<Store: KeyedBlobstore>
    TrieMapOps<Store, Entry<(Weight, ContentManifestId), ContentManifestFile>>
    for LoadableShardedMapV2Node<ContentManifestEntry>
{
    async fn expand(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<(
        Option<Entry<(Weight, ContentManifestId), ContentManifestFile>>,
        Vec<(u8, Self)>,
    )> {
        let (entry, children) = self.expand(ctx, blobstore).await?;
        Ok((
            entry.map(crate::types::convert_content_manifest_weighted),
            children,
        ))
    }

    async fn into_stream(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
    ) -> Result<
        BoxStream<
            'async_trait,
            Result<(
                SmallVec<[u8; 24]>,
                Entry<(Weight, ContentManifestId), ContentManifestFile>,
            )>,
        >,
    > {
        Ok(self
            .load(ctx, blobstore)
            .await?
            .into_entries(ctx, blobstore)
            .map_ok(|(k, v)| (k, crate::types::convert_content_manifest_weighted(v)))
            .boxed())
    }

    async fn lookup_many(
        self,
        ctx: &CoreContext,
        blobstore: &Store,
        names: Vec<SmallVec<[u8; 24]>>,
    ) -> Result<
        Vec<(
            SmallVec<[u8; 24]>,
            Entry<(Weight, ContentManifestId), ContentManifestFile>,
        )>,
    > {
        Ok(
            LoadableShardedMapV2Node::lookup_many(self, ctx, blobstore, names)
                .await?
                .into_iter()
                .map(|(name, entry)| (name, crate::types::convert_content_manifest_weighted(entry)))
                .collect(),
        )
    }

    fn is_empty(&self) -> bool {
        self.size() == 0
    }
}
