/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use bytes::Bytes;
use caching_ext::CacheDisposition;
use caching_ext::CacheHandlerFactory;
use caching_ext::CacheTtl;
use caching_ext::CachelibHandler;
use caching_ext::EntityStore;
use caching_ext::KeyedEntityStore;
use caching_ext::McErrorKind;
use caching_ext::McResult;
use caching_ext::MemcacheEntity;
use caching_ext::MemcacheHandler;
use caching_ext::get_or_fill_chunked;
use context::CoreContext;
use fbthrift::compact_protocol;
use hg_mutation_entry_thrift as thrift;
use memcache::KeyGen;
use mercurial_types::HgChangesetId;
use mercurial_types::HgNodeHash;
use mononoke_types::RepositoryId;
#[cfg(test)]
use quickcheck_arbitrary_derive::Arbitrary;

use crate::HgMutationEntry;
use crate::HgMutationStore;

/// Struct representing the cache entry for
/// (repo_id, cs_id) -> Vec<HgMutationEntry> mapping
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[derive(bincode::Encode, bincode::Decode)]
#[cfg_attr(test, derive(Arbitrary))]
pub struct HgMutationCacheEntry {
    /// The mutation entries that are part of the cache record
    pub mutation_entries: Vec<HgMutationEntry>,
    /// The ID of the repo corresponding to the mutation entries
    pub repo_id: RepositoryId,
    /// The ID of the changeset corresponding to the mutation
    /// entries
    pub changeset_id: HgChangesetId,
}

impl HgMutationCacheEntry {
    fn from_thrift(entry: thrift::HgMutationCacheEntry) -> Result<Self> {
        Ok(Self {
            repo_id: RepositoryId::new(entry.repo_id.0),
            changeset_id: HgChangesetId::new(HgNodeHash::from_thrift(entry.changeset_id)?),
            mutation_entries: entry
                .mutation_entries
                .into_iter()
                .map(HgMutationEntry::from_thrift)
                .collect::<Result<_>>()?,
        })
    }

    fn into_thrift(self) -> thrift::HgMutationCacheEntry {
        thrift::HgMutationCacheEntry {
            repo_id: thrift::RepoId(self.repo_id.id()),
            changeset_id: self.changeset_id.into_nodehash().into_thrift(),
            mutation_entries: self
                .mutation_entries
                .into_iter()
                .map(HgMutationEntry::into_thrift)
                .collect(),
        }
    }

    pub fn into_entries(
        self,
        repo_id: RepositoryId,
        changeset_id: HgChangesetId,
    ) -> Result<(HgChangesetId, Vec<HgMutationEntry>)> {
        if self.repo_id == repo_id && self.changeset_id == changeset_id {
            Ok((self.changeset_id, self.mutation_entries))
        } else {
            Err(anyhow!(
                "Cache returned invalid entry: repo {} & changeset {} returned for query to repo {} and changeset {}",
                self.repo_id,
                self.changeset_id,
                repo_id,
                changeset_id,
            ))
        }
    }

    pub fn from_entries(
        mutation_entries: Vec<HgMutationEntry>,
        repo_id: RepositoryId,
        changeset_id: HgChangesetId,
    ) -> HgMutationCacheEntry {
        HgMutationCacheEntry {
            repo_id,
            changeset_id,
            mutation_entries,
        }
    }
}

#[derive(Clone)]
pub struct CachedHgMutationStore {
    inner_store: Arc<dyn HgMutationStore>,
    cachelib: CachelibHandler<HgMutationCacheEntry>,
    memcache: MemcacheHandler,
    keygen: KeyGen,
}

impl CachedHgMutationStore {
    pub fn new(
        inner_store: Arc<dyn HgMutationStore>,
        cache_handler_factory: CacheHandlerFactory,
    ) -> Result<Self> {
        Ok(Self {
            inner_store,
            cachelib: cache_handler_factory.cachelib(),
            memcache: cache_handler_factory.memcache(),
            keygen: CachedHgMutationStore::create_key_gen()?,
        })
    }

    pub fn new_test(inner_store: Arc<dyn HgMutationStore>) -> Self {
        Self::new(inner_store, CacheHandlerFactory::Mocked).unwrap()
    }

    fn create_key_gen() -> Result<KeyGen> {
        let key_prefix = "scm.mononoke.hg_mutation_store";
        let sitever =
            justknobs::get_as::<u32>("scm/mononoke_memcache_sitevers:hg_mutation_store", None);
        Ok(KeyGen::new(key_prefix, thrift::MC_CODEVER as u32, sitever))
    }
}

fn memcache_deserialize(bytes: Bytes) -> McResult<HgMutationCacheEntry> {
    let thrift_entry =
        compact_protocol::deserialize(bytes).map_err(|_| McErrorKind::Deserialization);
    thrift_entry.and_then(|entry| {
        HgMutationCacheEntry::from_thrift(entry).map_err(|_| McErrorKind::Deserialization)
    })
}

fn memcache_serialize(entry: &HgMutationCacheEntry) -> Bytes {
    compact_protocol::serialize(entry.clone().into_thrift())
}

const CHUNK_SIZE: usize = 1000;
const PARALLEL_CHUNKS: usize = 1;

#[async_trait]
impl HgMutationStore for CachedHgMutationStore {
    fn repo_id(&self) -> RepositoryId {
        self.inner_store.repo_id()
    }

    async fn add_entries(
        &self,
        ctx: &CoreContext,
        new_changeset_ids: HashSet<HgChangesetId>,
        entries: Vec<HgMutationEntry>,
    ) -> Result<()> {
        self.inner_store
            .add_entries(ctx, new_changeset_ids, entries)
            .await
    }

    async fn all_predecessors_by_changeset(
        &self,
        ctx: &CoreContext,
        changeset_ids: HashSet<HgChangesetId>,
    ) -> Result<HashMap<HgChangesetId, Vec<HgMutationEntry>>> {
        let cache_request = (ctx, self);
        let repo_id = self.repo_id();

        let mutation_entries_by_changeset =
            get_or_fill_chunked(&cache_request, changeset_ids, CHUNK_SIZE, PARALLEL_CHUNKS)
                .await?
                .into_iter()
                .map(|(cs_id, val)| val.into_entries(repo_id, cs_id))
                .collect::<Result<_>>()?;

        Ok(mutation_entries_by_changeset)
    }
}

fn get_cache_key(repo_id: RepositoryId, cs: &HgChangesetId) -> String {
    format!("{}.{}", repo_id.prefix(), cs)
}

impl MemcacheEntity for HgMutationCacheEntry {
    fn serialize(&self) -> Bytes {
        memcache_serialize(self)
    }

    fn deserialize(bytes: Bytes) -> McResult<Self> {
        memcache_deserialize(bytes)
    }
}

type CacheRequest<'a> = (&'a CoreContext, &'a CachedHgMutationStore);

impl EntityStore<HgMutationCacheEntry> for CacheRequest<'_> {
    fn cachelib(&self) -> &CachelibHandler<HgMutationCacheEntry> {
        let (_, inner_store) = self;
        &inner_store.cachelib
    }

    fn keygen(&self) -> &KeyGen {
        let (_, inner_store) = self;
        &inner_store.keygen
    }

    fn memcache(&self) -> &MemcacheHandler {
        let (_, inner_store) = self;
        &inner_store.memcache
    }

    fn cache_determinator(&self, _: &HgMutationCacheEntry) -> CacheDisposition {
        CacheDisposition::Cache(CacheTtl::Ttl(Duration::from_hours(1)))
    }

    caching_ext::impl_singleton_stats!("hg_mutation_store");
}

#[async_trait]
impl KeyedEntityStore<HgChangesetId, HgMutationCacheEntry> for CacheRequest<'_> {
    fn get_cache_key(&self, key: &HgChangesetId) -> String {
        let (_, inner_store) = self;
        get_cache_key(inner_store.repo_id(), key)
    }

    async fn get_from_db(
        &self,
        keys: HashSet<HgChangesetId>,
    ) -> Result<HashMap<HgChangesetId, HgMutationCacheEntry>, Error> {
        let (ctx, store) = self;
        let repo_id = store.repo_id();

        let res = store
            .inner_store
            .all_predecessors_by_changeset(ctx, keys)
            .await?;

        Result::<_, Error>::Ok(
            res.into_iter()
                .map(|(cs_id, entries)| {
                    (
                        cs_id,
                        HgMutationCacheEntry::from_entries(entries, repo_id, cs_id),
                    )
                })
                .collect(),
        )
    }
}
