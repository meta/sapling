/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::Context;
use anyhow::Result;
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
use futures::stream;
use futures::stream::TryStreamExt;
use memcache::KeyGen;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use mononoke_types::RepositoryId;

use crate::SqlCommitDerivedDataMapping;

#[cfg(test)]
mod tests;

// Bump the code version when changing cache keys or value encoding.
const CACHE_CODE_VERSION: u32 = 1;
const FETCH_CHUNK_SIZE: usize = 1000;
const PARALLEL_FETCH_CHUNKS: usize = 2;
const PARALLEL_EVICTIONS: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
#[derive(bincode::Encode, bincode::Decode)]
struct CachedMapping(Vec<u8>);

impl MemcacheEntity for CachedMapping {
    fn serialize(&self) -> Bytes {
        compact_protocol::serialize(&self.0)
    }

    fn deserialize(bytes: Bytes) -> McResult<Self> {
        let value =
            compact_protocol::deserialize(bytes).map_err(|_| McErrorKind::Deserialization)?;
        Ok(Self(value))
    }
}

pub(super) struct MappingCache {
    cachelib: CachelibHandler<CachedMapping>,
    memcache: MemcacheHandler,
    keygen: KeyGen,
}

impl MappingCache {
    pub(super) fn new(factory: CacheHandlerFactory) -> Self {
        Self {
            cachelib: factory.cachelib(),
            memcache: factory.memcache(),
            keygen: KeyGen::new(
                "scm.mononoke.commit_derived_data_mapping",
                CACHE_CODE_VERSION,
                justknobs::get_as::<u32>(
                    "scm/mononoke_memcache_sitevers:commit_derived_data_mapping",
                    None,
                ),
            ),
        }
    }
}

fn cache_key(
    repo_id: RepositoryId,
    derived_data_type: DerivableType,
    derived_data_version: i32,
    shard_id: usize,
    cs_id: &ChangesetId,
) -> String {
    format!(
        "v{}.repo{}.type{}.version{}.shard{}.{}",
        CACHE_CODE_VERSION,
        repo_id,
        SqlCommitDerivedDataMapping::derived_data_type_id(derived_data_type),
        derived_data_version,
        shard_id,
        cs_id,
    )
}

impl MappingCache {
    /// Whether entries are shared with other processes through memcache.
    pub(super) fn has_shared_cache(&self) -> bool {
        !self.memcache.is_noop()
    }

    /// Deletes the memcache entries for these mappings.  Entries in any
    /// process's cachelib are unaffected.
    pub(super) async fn evict_shared(
        &self,
        repo_id: RepositoryId,
        derived_data_type: DerivableType,
        derived_data_version: i32,
        shard_id: usize,
        cs_ids: &[ChangesetId],
    ) -> Result<()> {
        stream::iter(cs_ids.iter().map(anyhow::Ok))
            .try_for_each_concurrent(PARALLEL_EVICTIONS, |cs_id| {
                let key = self.keygen.key(cache_key(
                    repo_id,
                    derived_data_type,
                    derived_data_version,
                    shard_id,
                    cs_id,
                ));
                self.memcache.del(key)
            })
            .await
            .with_context(|| {
                format!("Error evicting {derived_data_type:?} mappings for repo {repo_id}")
            })
    }
}

pub(super) struct CacheRequest<'a> {
    pub ctx: &'a CoreContext,
    pub sql: &'a SqlCommitDerivedDataMapping,
    pub cache: &'a MappingCache,
    pub repo_id: RepositoryId,
    pub derived_data_type: DerivableType,
    pub derived_data_version: i32,
    pub shard_id: usize,
}

impl CacheRequest<'_> {
    pub(super) async fn fetch(
        self,
        cs_ids: Vec<ChangesetId>,
    ) -> Result<Vec<(ChangesetId, Vec<u8>)>> {
        Ok(get_or_fill_chunked(
            &self,
            cs_ids.into_iter().collect(),
            FETCH_CHUNK_SIZE,
            PARALLEL_FETCH_CHUNKS,
        )
        .await
        .with_context(|| {
            format!(
                "Error fetching {:?} mappings for repo {} via cache",
                self.derived_data_type, self.repo_id,
            )
        })?
        .into_iter()
        .map(|(cs_id, value)| (cs_id, value.0))
        .collect())
    }
}

impl EntityStore<CachedMapping> for CacheRequest<'_> {
    fn cachelib(&self) -> &CachelibHandler<CachedMapping> {
        &self.cache.cachelib
    }

    fn keygen(&self) -> &KeyGen {
        &self.cache.keygen
    }

    fn memcache(&self) -> &MemcacheHandler {
        &self.cache.memcache
    }

    fn cache_determinator(&self, _: &CachedMapping) -> CacheDisposition {
        CacheDisposition::Cache(CacheTtl::NoTtl)
    }

    caching_ext::impl_singleton_stats!("commit_derived_data_mapping");
}

#[async_trait]
impl KeyedEntityStore<ChangesetId, CachedMapping> for CacheRequest<'_> {
    fn get_cache_key(&self, cs_id: &ChangesetId) -> String {
        cache_key(
            self.repo_id,
            self.derived_data_type,
            self.derived_data_version,
            self.shard_id,
            cs_id,
        )
    }

    async fn get_from_db(
        &self,
        keys: HashSet<ChangesetId>,
    ) -> Result<HashMap<ChangesetId, CachedMapping>> {
        Ok(self
            .sql
            .fetch_mapping_batch(
                self.ctx,
                self.repo_id,
                keys.into_iter().collect(),
                self.derived_data_type,
                self.derived_data_version,
                self.shard_id,
            )
            .await
            .context("Error fetching derived data mappings from SQL")?
            .into_iter()
            .map(|(cs_id, value)| (cs_id, CachedMapping(value)))
            .collect())
    }
}
