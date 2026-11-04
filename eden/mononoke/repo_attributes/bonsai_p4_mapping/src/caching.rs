/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use ::sql_ext::Transaction;
use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use bonsai_p4_mapping_thrift as thrift;
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
use caching_ext::get_or_fill;
use context::CoreContext;
use fbthrift::compact_protocol;
use memcache::KeyGen;
use mononoke_types::ChangesetId;
use mononoke_types::P4ChangelistId;
use mononoke_types::RepositoryId;

use super::AddP4MappingErrorKind;
use super::BonsaiP4Mapping;
use super::BonsaiP4MappingEntry;
use super::BonsaisOrP4ChangelistIds;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[derive(bincode::Encode, bincode::Decode)]
pub struct BonsaiP4MappingCacheEntry {
    pub repo_id: RepositoryId,
    pub bcs_id: ChangesetId,
    pub p4_changelist_id: P4ChangelistId,
}

impl BonsaiP4MappingCacheEntry {
    pub fn new(
        repo_id: RepositoryId,
        bcs_id: ChangesetId,
        p4_changelist_id: P4ChangelistId,
    ) -> Self {
        BonsaiP4MappingCacheEntry {
            repo_id,
            bcs_id,
            p4_changelist_id,
        }
    }

    fn into_entry(self, repo_id: RepositoryId) -> Result<BonsaiP4MappingEntry> {
        if self.repo_id == repo_id {
            Ok(BonsaiP4MappingEntry {
                bcs_id: self.bcs_id,
                p4_changelist_id: self.p4_changelist_id,
            })
        } else {
            Err(anyhow!(
                "Cache returned invalid entry: repo {} returned for query to repo {}",
                self.repo_id,
                repo_id
            ))
        }
    }

    fn from_entry(entry: BonsaiP4MappingEntry, repo_id: RepositoryId) -> Self {
        BonsaiP4MappingCacheEntry {
            repo_id,
            bcs_id: entry.bcs_id,
            p4_changelist_id: entry.p4_changelist_id,
        }
    }
}

pub struct CachingBonsaiP4Mapping {
    cachelib: CachelibHandler<BonsaiP4MappingCacheEntry>,
    memcache: MemcacheHandler,
    keygen: KeyGen,
    inner: Arc<dyn BonsaiP4Mapping>,
}

impl CachingBonsaiP4Mapping {
    pub fn new(
        inner: Arc<dyn BonsaiP4Mapping>,
        cache_handler_factory: CacheHandlerFactory,
    ) -> Self {
        Self {
            inner,
            cachelib: cache_handler_factory.cachelib(),
            memcache: cache_handler_factory.memcache(),
            keygen: Self::create_key_gen(),
        }
    }

    pub fn new_test(inner: Arc<dyn BonsaiP4Mapping>) -> Self {
        Self::new(inner, CacheHandlerFactory::Mocked)
    }

    fn create_key_gen() -> KeyGen {
        let key_prefix = "scm.mononoke.bonsai_p4_mapping";

        KeyGen::new(
            key_prefix,
            thrift::MC_CODEVER as u32,
            thrift::MC_SITEVER as u32,
        )
    }

    pub fn cachelib(&self) -> &CachelibHandler<BonsaiP4MappingCacheEntry> {
        &self.cachelib
    }
}

#[async_trait]
impl BonsaiP4Mapping for CachingBonsaiP4Mapping {
    fn repo_id(&self) -> RepositoryId {
        self.inner.as_ref().repo_id()
    }

    async fn bulk_import(
        &self,
        ctx: &CoreContext,
        entries: &[BonsaiP4MappingEntry],
    ) -> Result<(), Error> {
        self.inner.as_ref().bulk_import(ctx, entries).await
    }

    async fn bulk_import_in_transaction(
        &self,
        ctx: &CoreContext,
        entries: &[BonsaiP4MappingEntry],
        transaction: Transaction,
    ) -> Result<Transaction, AddP4MappingErrorKind> {
        self.inner
            .as_ref()
            .bulk_import_in_transaction(ctx, entries, transaction)
            .await
    }

    async fn get_max(&self, ctx: &CoreContext) -> Result<Option<P4ChangelistId>, Error> {
        self.inner.as_ref().get_max(ctx).await
    }

    async fn get(
        &self,
        ctx: &CoreContext,
        objects: BonsaisOrP4ChangelistIds,
    ) -> Result<Vec<BonsaiP4MappingEntry>, Error> {
        let cache_request = (ctx, self);
        let repo_id = self.repo_id();

        let res = match objects {
            BonsaisOrP4ChangelistIds::Bonsai(cs_ids) => {
                get_or_fill(&cache_request, cs_ids.into_iter().collect())
                    .await
                    .with_context(|| "Error fetching p4 changelist ids via cache")?
                    .into_values()
                    .map(|val| val.into_entry(repo_id))
                    .collect::<Result<_>>()?
            }
            BonsaisOrP4ChangelistIds::P4ChangelistId(p4_changelist_ids) => {
                get_or_fill(&cache_request, p4_changelist_ids.into_iter().collect())
                    .await
                    .with_context(|| "Error fetching bonsais via cache")?
                    .into_values()
                    .map(|val| val.into_entry(repo_id))
                    .collect::<Result<_>>()?
            }
        };

        Ok(res)
    }
}

impl MemcacheEntity for BonsaiP4MappingCacheEntry {
    fn serialize(&self) -> Bytes {
        let entry = thrift::BonsaiP4MappingCacheEntry {
            repo_id: thrift::RepoId(self.repo_id.id()),
            bcs_id: self.bcs_id.into_thrift(),
            p4_changelist_id: self
                .p4_changelist_id
                .id()
                .try_into()
                .expect("P4 changelist ids must fit within a i64"),
        };
        compact_protocol::serialize(&entry)
    }

    fn deserialize(bytes: Bytes) -> McResult<Self> {
        let thrift::BonsaiP4MappingCacheEntry {
            repo_id: thrift::RepoId(repo_id),
            bcs_id,
            p4_changelist_id,
        } = compact_protocol::deserialize(bytes).map_err(|_| McErrorKind::Deserialization)?;

        let repo_id = RepositoryId::new(repo_id);
        let bcs_id = ChangesetId::from_thrift(bcs_id).map_err(|_| McErrorKind::Deserialization)?;
        let p4_changelist_id = P4ChangelistId::new(
            p4_changelist_id
                .try_into()
                .map_err(|_| McErrorKind::Deserialization)?,
        );

        Ok(BonsaiP4MappingCacheEntry {
            repo_id,
            bcs_id,
            p4_changelist_id,
        })
    }
}

type CacheRequest<'a> = (&'a CoreContext, &'a CachingBonsaiP4Mapping);

impl EntityStore<BonsaiP4MappingCacheEntry> for CacheRequest<'_> {
    fn cachelib(&self) -> &CachelibHandler<BonsaiP4MappingCacheEntry> {
        let (_, mapping) = self;
        &mapping.cachelib
    }

    fn keygen(&self) -> &KeyGen {
        let (_, mapping) = self;
        &mapping.keygen
    }

    fn memcache(&self) -> &MemcacheHandler {
        let (_, mapping) = self;
        &mapping.memcache
    }

    fn cache_determinator(&self, _: &BonsaiP4MappingCacheEntry) -> CacheDisposition {
        CacheDisposition::Cache(CacheTtl::NoTtl)
    }

    caching_ext::impl_singleton_stats!("bonsai_p4_mapping");
}

#[async_trait]
impl KeyedEntityStore<ChangesetId, BonsaiP4MappingCacheEntry> for CacheRequest<'_> {
    fn get_cache_key(&self, key: &ChangesetId) -> String {
        let (_, mapping) = self;
        format!("{}.bonsai.{}", mapping.repo_id(), key)
    }

    async fn get_from_db(
        &self,
        keys: HashSet<ChangesetId>,
    ) -> Result<HashMap<ChangesetId, BonsaiP4MappingCacheEntry>, Error> {
        let (ctx, mapping) = self;
        let repo_id = mapping.repo_id();

        let res = mapping
            .inner
            .as_ref()
            .get(
                ctx,
                BonsaisOrP4ChangelistIds::Bonsai(keys.into_iter().collect()),
            )
            .await
            .with_context(|| "Error fetching p4 changelist ids from bonsais from SQL")?;

        Result::<_, Error>::Ok(
            res.into_iter()
                .map(|e| (e.bcs_id, BonsaiP4MappingCacheEntry::from_entry(e, repo_id)))
                .collect(),
        )
    }
}

#[async_trait]
impl KeyedEntityStore<P4ChangelistId, BonsaiP4MappingCacheEntry> for CacheRequest<'_> {
    fn get_cache_key(&self, key: &P4ChangelistId) -> String {
        let (_, mapping) = self;
        format!("{}.p4.{}", mapping.repo_id(), key.id())
    }

    async fn get_from_db(
        &self,
        keys: HashSet<P4ChangelistId>,
    ) -> Result<HashMap<P4ChangelistId, BonsaiP4MappingCacheEntry>, Error> {
        let (ctx, mapping) = self;
        let repo_id = mapping.repo_id();

        let res = mapping
            .inner
            .as_ref()
            .get(
                ctx,
                BonsaisOrP4ChangelistIds::P4ChangelistId(keys.into_iter().collect()),
            )
            .await
            .with_context(|| "Error fetching bonsais from p4 changelist ids from SQL")?;

        Result::<_, Error>::Ok(
            res.into_iter()
                .map(|e| {
                    (
                        e.p4_changelist_id,
                        BonsaiP4MappingCacheEntry::from_entry(e, repo_id),
                    )
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod test {
    use mononoke_macros::mononoke;
    use mononoke_types_mocks::changesetid::ONES_CSID;
    use mononoke_types_mocks::p4_changelist_id::P4_CHANGELIST_THREE;
    use mononoke_types_mocks::repo::REPO_ONE;
    use mononoke_types_mocks::repo::REPO_ZERO;

    use super::*;

    /// The memcache encoding (the private thrift struct) round-trips every field. A non-zero
    /// repo id, so a dropped or defaulted repo id can't pass.
    #[mononoke::test]
    fn test_memcache_entry_round_trip() {
        let entry = BonsaiP4MappingCacheEntry::new(REPO_ONE, ONES_CSID, P4_CHANGELIST_THREE);
        let decoded = BonsaiP4MappingCacheEntry::deserialize(entry.serialize());
        assert!(decoded.is_ok_and(|decoded| decoded == entry));
    }

    /// An entry cached for one repo is refused when read for another.
    #[mononoke::test]
    fn test_cached_entry_for_another_repo_is_rejected() {
        let entry = BonsaiP4MappingCacheEntry::new(REPO_ONE, ONES_CSID, P4_CHANGELIST_THREE);
        assert!(entry.clone().into_entry(REPO_ONE).is_ok());
        assert!(entry.into_entry(REPO_ZERO).is_err());
    }
}
