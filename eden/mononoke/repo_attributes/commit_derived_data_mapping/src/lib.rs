/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use caching_ext::CacheHandlerFactory;
use context::CoreContext;
use context::PerfCounterType;
use metaconfig_types::OssRemoteDatabaseConfig;
use metaconfig_types::OssRemoteMetadataDatabaseConfig;
use metaconfig_types::RemoteMetadataDatabaseConfig;
use metaconfig_types::ShardableRemoteDatabaseConfig;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use mononoke_types::RepositoryId;
use sql_construct::SqlShardableConstructFromMetadataDatabaseConfig;
use sql_construct::SqlShardedConstruct;
use sql_ext::Connection;
use sql_ext::SqlShardedConnections;
use sql_ext::mononoke_queries;
use vec1::Vec1;

mod caching;

pub const MYSQL_INSERT_CHUNK_SIZE: usize = 1000;

mononoke_queries! {
    write InsertMappings(values: (
        repo_id: RepositoryId,
        cs_id: ChangesetId,
        derived_data_type: i32,
        derived_data_version: i32,
        derived_value: Vec<u8>
    )) {
        insert_or_ignore,
        "{insert_or_ignore} INTO commit_derived_data (repo_id, cs_id, derived_data_type, derived_data_version, derived_value) VALUES {values}"
    }

    read SelectMapping(
        repo_id: RepositoryId,
        cs_id: ChangesetId,
        derived_data_type: i32,
        derived_data_version: i32
    ) -> (Vec<u8>,) {
        "SELECT derived_value FROM commit_derived_data
         WHERE repo_id = {repo_id}
           AND cs_id = {cs_id}
           AND derived_data_type = {derived_data_type}
           AND derived_data_version = {derived_data_version}"
    }

    read SelectTypeMappings(
        repo_id: RepositoryId,
        derived_data_type: i32,
        limit: u64
    ) -> (ChangesetId, i32) {
        "SELECT cs_id, derived_data_version FROM commit_derived_data
         WHERE repo_id = {repo_id}
           AND derived_data_type = {derived_data_type}
         ORDER BY cs_id, derived_data_version
         LIMIT {limit}"
    }

    read SelectTypeMappingsAfter(
        repo_id: RepositoryId,
        derived_data_type: i32,
        after_cs_id: ChangesetId,
        after_version: i32,
        limit: u64
    ) -> (ChangesetId, i32) {
        "SELECT cs_id, derived_data_version FROM commit_derived_data
         WHERE repo_id = {repo_id}
           AND derived_data_type = {derived_data_type}
           AND (cs_id > {after_cs_id}
                OR (cs_id = {after_cs_id} AND derived_data_version > {after_version}))
         ORDER BY cs_id, derived_data_version
         LIMIT {limit}"
    }

    write DeleteMappings(
        repo_id: RepositoryId,
        derived_data_type: i32,
        derived_data_version: i32,
        >list cs_ids: ChangesetId
    ) {
        none,
        "DELETE FROM commit_derived_data
         WHERE repo_id = {repo_id}
           AND derived_data_type = {derived_data_type}
           AND derived_data_version = {derived_data_version}
           AND cs_id IN {cs_ids}"
    }

    read SelectMappingBatch(
        repo_id: RepositoryId,
        derived_data_type: i32,
        derived_data_version: i32,
        >list cs_ids: ChangesetId
    ) -> (ChangesetId, Vec<u8>) {
        "SELECT cs_id, derived_value FROM commit_derived_data
         WHERE repo_id = {repo_id}
           AND cs_id IN {cs_ids}
           AND derived_data_type = {derived_data_type}
           AND derived_data_version = {derived_data_version}"
    }
}

/// Outcome of evicting cached mappings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheEviction {
    /// Every given mapping's shared-cache entry is gone, whether or not it
    /// was present.
    Evicted,
    /// The mapping has no shared cache to evict from.
    NoSharedCache,
}

/// Mapping of (repo_id, changeset_id, derived_data_type, version) -> derived_value
/// stored in XDB, sharded by derived data type.
///
/// Reads check the primary for replica misses to preserve read-after-write consistency.
///
/// The caller is responsible for providing the correct connection index
/// (from `xdb_mapping_shard_ids` config) for each derived data type.
#[facet::facet]
pub struct CommitDerivedDataMapping {
    pub sql: SqlCommitDerivedDataMapping,
    cache: Option<caching::MappingCache>,
}

impl CommitDerivedDataMapping {
    /// Construct a mapping without caching.
    pub fn new(sql: SqlCommitDerivedDataMapping) -> Self {
        Self { sql, cache: None }
    }

    /// Cache successful mapping reads in the configured local and shared caches.
    pub fn with_caching(self, factory: CacheHandlerFactory) -> Self {
        Self {
            cache: Some(caching::MappingCache::new(factory)),
            ..self
        }
    }

    pub async fn store_mapping(
        &self,
        ctx: &CoreContext,
        repo_id: RepositoryId,
        cs_id: ChangesetId,
        derived_data_type: DerivableType,
        derived_data_version: i32,
        derived_value: &[u8],
        shard_id: usize,
    ) -> Result<()> {
        self.sql
            .store_mapping(
                ctx,
                repo_id,
                cs_id,
                derived_data_type,
                derived_data_version,
                derived_value,
                shard_id,
            )
            .await
    }

    /// Store a batch of mappings, all for the same derived data type
    /// and targeting the same connection.
    pub async fn store_mapping_batch(
        &self,
        ctx: &CoreContext,
        repo_id: RepositoryId,
        entries: Vec<(ChangesetId, i32, Vec<u8>)>,
        derived_data_type: DerivableType,
        derived_data_version: i32,
        shard_id: usize,
    ) -> Result<u64> {
        self.sql
            .store_mapping_batch(
                ctx,
                repo_id,
                entries,
                derived_data_type,
                derived_data_version,
                shard_id,
            )
            .await
    }

    pub async fn fetch_mapping(
        &self,
        ctx: &CoreContext,
        repo_id: RepositoryId,
        cs_id: ChangesetId,
        derived_data_type: DerivableType,
        derived_data_version: i32,
        shard_id: usize,
    ) -> Result<Option<Vec<u8>>> {
        if self.cache.is_some() {
            let rows = self
                .fetch_mapping_batch(
                    ctx,
                    repo_id,
                    vec![cs_id],
                    derived_data_type,
                    derived_data_version,
                    shard_id,
                )
                .await?;
            return Ok(rows.into_iter().next().map(|(_, value)| value));
        }

        self.sql
            .fetch_mapping(
                ctx,
                repo_id,
                cs_id,
                derived_data_type,
                derived_data_version,
                shard_id,
            )
            .await
    }

    pub async fn fetch_mapping_batch(
        &self,
        ctx: &CoreContext,
        repo_id: RepositoryId,
        cs_ids: Vec<ChangesetId>,
        derived_data_type: DerivableType,
        derived_data_version: i32,
        shard_id: usize,
    ) -> Result<Vec<(ChangesetId, Vec<u8>)>> {
        if let Some(cache) = &self.cache {
            self.sql.shard_id(shard_id)?;
            return caching::CacheRequest {
                ctx,
                sql: &self.sql,
                cache,
                repo_id,
                derived_data_type,
                derived_data_version,
                shard_id,
            }
            .fetch(cs_ids)
            .await;
        }

        self.sql
            .fetch_mapping_batch(
                ctx,
                repo_id,
                cs_ids,
                derived_data_type,
                derived_data_version,
                shard_id,
            )
            .await
    }

    /// Deletes the shared-cache (memcache) entries for these mappings, keyed
    /// exactly as reads through `shard_id` cache them.  Mappings are cached
    /// without expiry, so this must follow any deletion of their rows.
    /// Entries already in any process's local cache are unaffected.
    pub async fn evict_shared_cache(
        &self,
        repo_id: RepositoryId,
        derived_data_type: DerivableType,
        derived_data_version: i32,
        shard_id: usize,
        cs_ids: &[ChangesetId],
    ) -> Result<CacheEviction> {
        self.sql.shard_id(shard_id)?;
        match &self.cache {
            Some(cache) if cache.has_shared_cache() => {
                cache
                    .evict_shared(
                        repo_id,
                        derived_data_type,
                        derived_data_version,
                        shard_id,
                        cs_ids,
                    )
                    .await?;
                Ok(CacheEviction::Evicted)
            }
            _ => Ok(CacheEviction::NoSharedCache),
        }
    }
}

pub struct SqlCommitDerivedDataMapping {
    write_connections: Arc<Vec1<Connection>>,
    read_connections: Arc<Vec1<Connection>>,
    read_master_connections: Arc<Vec1<Connection>>,
}

impl SqlCommitDerivedDataMapping {
    fn derived_data_type_id(dt: DerivableType) -> i32 {
        i32::from(dt.into_thrift())
    }

    fn shard_id(&self, id: usize) -> Result<usize> {
        let count = self.read_connections.len();
        if id >= count {
            return Err(anyhow!(
                "connection index {id} out of range (have {count} connections)",
            ));
        }
        Ok(id)
    }

    pub async fn store_mapping(
        &self,
        ctx: &CoreContext,
        repo_id: RepositoryId,
        cs_id: ChangesetId,
        derived_data_type: DerivableType,
        derived_data_version: i32,
        derived_value: &[u8],
        shard_id: usize,
    ) -> Result<()> {
        let idx = self.shard_id(shard_id)?;
        let type_id = Self::derived_data_type_id(derived_data_type);
        let value_vec = derived_value.to_vec();
        let values: Vec<_> = vec![(
            &repo_id,
            &cs_id,
            &type_id,
            &derived_data_version,
            &value_vec,
        )];
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlWrites);
        InsertMappings::query(
            &self.write_connections[idx],
            ctx.sql_query_telemetry(),
            &values[..],
        )
        .await?;
        Ok(())
    }

    /// Store a batch of mappings, all for the same derived data type
    /// and targeting the same connection.
    pub async fn store_mapping_batch(
        &self,
        ctx: &CoreContext,
        repo_id: RepositoryId,
        entries: Vec<(ChangesetId, i32, Vec<u8>)>,
        derived_data_type: DerivableType,
        derived_data_version: i32,
        shard_id: usize,
    ) -> Result<u64> {
        if entries.is_empty() {
            return Ok(0);
        }

        let idx = self.shard_id(shard_id)?;
        let type_id = Self::derived_data_type_id(derived_data_type);

        let prepared: Vec<_> = entries
            .iter()
            .map(|(cs_id, _version, value)| {
                (
                    repo_id,
                    *cs_id,
                    type_id,
                    derived_data_version,
                    value.clone(),
                )
            })
            .collect();

        let mut affected_rows = 0;
        for chunk in prepared.chunks(MYSQL_INSERT_CHUNK_SIZE) {
            // This pattern is used to convert a ref to tuple into a tuple of refs.
            #[allow(clippy::map_identity)]
            let chunk: Vec<_> = chunk
                .iter()
                .map(|(a, b, c, d, e)| (a, b, c, d, e))
                .collect();
            ctx.perf_counters()
                .increment_counter(PerfCounterType::SqlWrites);
            let result = InsertMappings::query(
                &self.write_connections[idx],
                ctx.sql_query_telemetry(),
                &chunk[..],
            )
            .await?;
            affected_rows += result.affected_rows();
        }
        Ok(affected_rows)
    }

    pub async fn fetch_mapping(
        &self,
        ctx: &CoreContext,
        repo_id: RepositoryId,
        cs_id: ChangesetId,
        derived_data_type: DerivableType,
        derived_data_version: i32,
        shard_id: usize,
    ) -> Result<Option<Vec<u8>>> {
        let idx = self.shard_id(shard_id)?;
        let type_id = Self::derived_data_type_id(derived_data_type);
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlReadsReplica);
        let rows = SelectMapping::query(
            &self.read_connections[idx],
            ctx.sql_query_telemetry(),
            &repo_id,
            &cs_id,
            &type_id,
            &derived_data_version,
        )
        .await?;
        if let Some((value,)) = rows.into_iter().next() {
            return Ok(Some(value));
        }

        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlReadsMaster);
        let rows = SelectMapping::query(
            &self.read_master_connections[idx],
            ctx.sql_query_telemetry(),
            &repo_id,
            &cs_id,
            &type_id,
            &derived_data_version,
        )
        .await?;
        Ok(rows.into_iter().next().map(|(v,)| v))
    }

    pub async fn fetch_mapping_batch(
        &self,
        ctx: &CoreContext,
        repo_id: RepositoryId,
        cs_ids: Vec<ChangesetId>,
        derived_data_type: DerivableType,
        derived_data_version: i32,
        shard_id: usize,
    ) -> Result<Vec<(ChangesetId, Vec<u8>)>> {
        let idx = self.shard_id(shard_id)?;
        if cs_ids.is_empty() {
            return Ok(Vec::new());
        }

        let type_id = Self::derived_data_type_id(derived_data_type);
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlReadsReplica);
        let mut rows = SelectMappingBatch::query(
            &self.read_connections[idx],
            ctx.sql_query_telemetry(),
            &repo_id,
            &type_id,
            &derived_data_version,
            &cs_ids[..],
        )
        .await?;
        let fetched_cs_ids: HashSet<_> = rows.iter().map(|(cs_id, _)| *cs_id).collect();
        let missing_cs_ids: Vec<_> = cs_ids
            .into_iter()
            .filter(|cs_id| !fetched_cs_ids.contains(cs_id))
            .collect();
        if missing_cs_ids.is_empty() {
            return Ok(rows);
        }

        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlReadsMaster);
        let primary_rows = SelectMappingBatch::query(
            &self.read_master_connections[idx],
            ctx.sql_query_telemetry(),
            &repo_id,
            &type_id,
            &derived_data_version,
            &missing_cs_ids[..],
        )
        .await?;
        rows.extend(primary_rows);
        Ok(rows)
    }

    /// Number of connections, each a possible `shard_id`.
    pub fn connection_count(&self) -> usize {
        self.write_connections.len()
    }

    /// Lists up to `limit` mappings of one repository and type on one
    /// connection, as `(cs_id, version)` in ascending order after `after`.
    /// Reads from the primary.
    pub async fn list_type_mappings(
        &self,
        ctx: &CoreContext,
        repo_id: RepositoryId,
        derived_data_type: DerivableType,
        after: Option<(ChangesetId, i32)>,
        limit: NonZeroU32,
        shard_id: usize,
    ) -> Result<Vec<(ChangesetId, i32)>> {
        let idx = self.shard_id(shard_id)?;
        let type_id = Self::derived_data_type_id(derived_data_type);
        let limit = u64::from(limit.get());
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlReadsMaster);
        let rows = match after {
            None => {
                SelectTypeMappings::query(
                    &self.read_master_connections[idx],
                    ctx.sql_query_telemetry(),
                    &repo_id,
                    &type_id,
                    &limit,
                )
                .await?
            }
            Some((after_cs_id, after_version)) => {
                SelectTypeMappingsAfter::query(
                    &self.read_master_connections[idx],
                    ctx.sql_query_telemetry(),
                    &repo_id,
                    &type_id,
                    &after_cs_id,
                    &after_version,
                    &limit,
                )
                .await?
            }
        };
        Ok(rows)
    }

    /// Deletes the mappings of these changesets for one repository, type and
    /// version on one connection, returning the number of rows deleted.
    pub async fn delete_mappings(
        &self,
        ctx: &CoreContext,
        repo_id: RepositoryId,
        derived_data_type: DerivableType,
        derived_data_version: i32,
        cs_ids: &[ChangesetId],
        shard_id: usize,
    ) -> Result<u64> {
        let idx = self.shard_id(shard_id)?;
        if cs_ids.is_empty() {
            return Ok(0);
        }
        let type_id = Self::derived_data_type_id(derived_data_type);
        let mut deleted = 0;
        for chunk in cs_ids.chunks(MYSQL_INSERT_CHUNK_SIZE) {
            ctx.perf_counters()
                .increment_counter(PerfCounterType::SqlWrites);
            deleted += DeleteMappings::query(
                &self.write_connections[idx],
                ctx.sql_query_telemetry(),
                &repo_id,
                &type_id,
                &derived_data_version,
                chunk,
            )
            .await?
            .affected_rows();
        }
        Ok(deleted)
    }
}

impl SqlShardedConstruct for SqlCommitDerivedDataMapping {
    const LABEL: &'static str = "commit_derived_data_mapping";

    const CREATION_QUERY: &'static str = include_str!("../schemas/sqlite-commit-derived-data.sql");

    fn from_sql_shard_connections(connections: SqlShardedConnections) -> Self {
        let SqlShardedConnections {
            read_connections,
            read_master_connections,
            write_connections,
        } = connections;

        Self {
            write_connections: Arc::new(write_connections),
            read_connections: Arc::new(read_connections),
            read_master_connections: Arc::new(read_master_connections),
        }
    }
}

impl SqlShardableConstructFromMetadataDatabaseConfig for SqlCommitDerivedDataMapping {
    fn remote_database_config(
        remote: &RemoteMetadataDatabaseConfig,
    ) -> Option<&ShardableRemoteDatabaseConfig> {
        remote.commit_derived_data_mapping.as_ref()
    }
    fn oss_remote_database_config(
        remote: &OssRemoteMetadataDatabaseConfig,
    ) -> Option<&OssRemoteDatabaseConfig> {
        remote.commit_derived_data_mapping.as_ref()
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;

    use fbinit::FacebookInit;
    use mononoke_macros::mononoke;
    use mononoke_types::DerivableType;
    use mononoke_types_mocks::changesetid::ONES_CSID;
    use mononoke_types_mocks::changesetid::THREES_CSID;
    use mononoke_types_mocks::changesetid::TWOS_CSID;
    use sql_construct::SqlConstruct;
    use sql_ext::SqlConnections;

    use super::*;

    // In-memory SQLite has a single connection, so shard_id is always 0.
    const CONN: usize = 0;

    mononoke_queries! {
        write DropMappingsTable() {
            none,
            "DROP TABLE commit_derived_data"
        }
    }

    fn with_lagging_replica() -> Result<(SqlCommitDerivedDataMapping, SqlCommitDerivedDataMapping)>
    {
        let primary = SqlCommitDerivedDataMapping::with_sqlite_in_memory()?;
        let replica = SqlCommitDerivedDataMapping::with_sqlite_in_memory()?;
        let sql = SqlCommitDerivedDataMapping::from_sql_connections(SqlConnections {
            write_connection: primary.write_connections[CONN].clone(),
            read_master_connection: primary.write_connections[CONN].clone(),
            read_connection: replica.read_connections[CONN].clone(),
        });
        Ok((sql, replica))
    }

    #[mononoke::fbinit_test]
    async fn test_single_read_with_replica_lag(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (sql, replica) = with_lagging_replica()?;
        let repo_id = RepositoryId::new(1);
        let dt = DerivableType::HistoryManifests;
        let version = 1;
        let value = vec![1u8; 32];

        sql.store_mapping(&ctx, repo_id, ONES_CSID, dt, version, &value, CONN)
            .await?;
        assert_eq!(
            replica
                .fetch_mapping(&ctx, repo_id, ONES_CSID, dt, version, CONN)
                .await?,
            None,
        );
        assert_eq!(
            sql.fetch_mapping(&ctx, repo_id, ONES_CSID, dt, version, CONN)
                .await?,
            Some(value),
        );
        assert_eq!(
            sql.fetch_mapping(&ctx, repo_id, TWOS_CSID, dt, version, CONN)
                .await?,
            None,
        );

        for (repo_id, dt, version) in [
            (RepositoryId::new(2), dt, version),
            (repo_id, DerivableType::Fsnodes, version),
            (repo_id, dt, version + 1),
        ] {
            assert_eq!(
                sql.fetch_mapping(&ctx, repo_id, ONES_CSID, dt, version, CONN)
                    .await?,
                None,
            );
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_batch_read_with_replica_lag(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (sql, replica) = with_lagging_replica()?;
        let repo_id = RepositoryId::new(1);
        let dt = DerivableType::HistoryManifests;
        let version = 1;
        let value1 = vec![1u8; 32];
        let value2 = vec![2u8; 32];

        sql.store_mapping_batch(
            &ctx,
            repo_id,
            vec![
                (ONES_CSID, version, value1.clone()),
                (TWOS_CSID, version, value2.clone()),
            ],
            dt,
            version,
            CONN,
        )
        .await?;
        replica
            .store_mapping(&ctx, repo_id, ONES_CSID, dt, version, &value1, CONN)
            .await?;

        let results = sql
            .fetch_mapping_batch(
                &ctx,
                repo_id,
                vec![ONES_CSID, ONES_CSID, TWOS_CSID, TWOS_CSID, THREES_CSID],
                dt,
                version,
                CONN,
            )
            .await?;
        assert_eq!(results.len(), 2);
        assert_eq!(
            results.into_iter().collect::<HashMap<_, _>>(),
            HashMap::from([(ONES_CSID, value1), (TWOS_CSID, value2)]),
        );
        assert_eq!(
            ctx.perf_counters()
                .get_counter(PerfCounterType::SqlReadsReplica),
            1,
            "A partial replica miss uses one replica query",
        );
        assert_eq!(
            ctx.perf_counters()
                .get_counter(PerfCounterType::SqlReadsMaster),
            1,
            "Missing mappings are queried together on the primary",
        );

        for cs_ids in [vec![], vec![THREES_CSID]] {
            assert!(
                sql.fetch_mapping_batch(&ctx, repo_id, cs_ids, dt, version, CONN)
                    .await?
                    .is_empty(),
            );
        }
        assert_eq!(
            ctx.perf_counters()
                .get_counter(PerfCounterType::SqlReadsReplica),
            2,
            "The empty batch adds no query; the missing batch adds one",
        );
        assert_eq!(
            ctx.perf_counters()
                .get_counter(PerfCounterType::SqlReadsMaster),
            2,
            "Only the two nonempty batches require primary fallbacks",
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_replica_hits_do_not_query_primary(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (sql, replica) = with_lagging_replica()?;
        let repo_id = RepositoryId::new(1);
        let dt = DerivableType::HistoryManifests;
        let version = 1;
        let value = vec![1u8; 32];

        replica
            .store_mapping(&ctx, repo_id, ONES_CSID, dt, version, &value, CONN)
            .await?;
        DropMappingsTable::query(&sql.write_connections[CONN], ctx.sql_query_telemetry()).await?;

        assert_eq!(
            sql.fetch_mapping(&ctx, repo_id, ONES_CSID, dt, version, CONN)
                .await?,
            Some(value.clone()),
        );
        assert_eq!(
            sql.fetch_mapping_batch(&ctx, repo_id, vec![ONES_CSID, ONES_CSID], dt, version, CONN,)
                .await?,
            vec![(ONES_CSID, value)],
        );
        assert!(
            sql.fetch_mapping_batch(&ctx, repo_id, vec![], dt, version, CONN)
                .await?
                .is_empty(),
        );
        assert_eq!(
            ctx.perf_counters()
                .get_counter(PerfCounterType::SqlReadsReplica),
            2,
            "Each nonempty replica hit issues one query",
        );
        assert_eq!(
            ctx.perf_counters()
                .get_counter(PerfCounterType::SqlReadsMaster),
            0,
            "Replica hits and empty requests do not query the primary",
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_single_write_and_read(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let sql = SqlCommitDerivedDataMapping::with_sqlite_in_memory()?;
        let repo_id = RepositoryId::new(1);
        let dt = DerivableType::Fsnodes;
        let version = 1;
        let value = vec![1u8; 32];

        sql.store_mapping(&ctx, repo_id, ONES_CSID, dt, version, &value, CONN)
            .await?;

        let result = sql
            .fetch_mapping(&ctx, repo_id, ONES_CSID, dt, version, CONN)
            .await?;
        assert_eq!(result, Some(value.clone()));

        // Non-existent mapping returns None
        let result = sql
            .fetch_mapping(&ctx, repo_id, TWOS_CSID, dt, version, CONN)
            .await?;
        assert_eq!(result, None);
        assert_eq!(
            ctx.perf_counters().get_counter(PerfCounterType::SqlWrites),
            1,
            "A single mapping insert issues one write",
        );
        assert_eq!(
            ctx.perf_counters()
                .get_counter(PerfCounterType::SqlReadsReplica),
            2,
            "Each fetch issues one replica query",
        );
        assert_eq!(
            ctx.perf_counters()
                .get_counter(PerfCounterType::SqlReadsMaster),
            1,
            "Only the missing mapping requires a primary query",
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_batch_write_and_read(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let sql = SqlCommitDerivedDataMapping::with_sqlite_in_memory()?;
        let repo_id = RepositoryId::new(1);
        let dt = DerivableType::Fsnodes;
        let version = 1;
        let value1 = vec![1u8; 32];
        let value2 = vec![2u8; 32];
        let value3 = vec![3u8; 32];

        let entries = vec![
            (ONES_CSID, version, value1.clone()),
            (TWOS_CSID, version, value2.clone()),
            (THREES_CSID, version, value3.clone()),
        ];

        let affected = sql
            .store_mapping_batch(&ctx, repo_id, entries, dt, version, CONN)
            .await?;
        assert_eq!(affected, 3);

        let results = sql
            .fetch_mapping_batch(
                &ctx,
                repo_id,
                vec![ONES_CSID, TWOS_CSID, THREES_CSID],
                dt,
                version,
                CONN,
            )
            .await?;
        assert_eq!(results.len(), 3);

        let result_map: HashMap<_, _> = results.into_iter().collect();
        assert_eq!(result_map.get(&ONES_CSID), Some(&value1));
        assert_eq!(result_map.get(&TWOS_CSID), Some(&value2));
        assert_eq!(result_map.get(&THREES_CSID), Some(&value3));

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_batch_write_perf_counters(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let sql = SqlCommitDerivedDataMapping::with_sqlite_in_memory()?;
        let repo_id = RepositoryId::new(1);
        let dt = DerivableType::HistoryManifests;
        let version = 1;

        assert_eq!(
            sql.store_mapping_batch(&ctx, repo_id, vec![], dt, version, CONN)
                .await?,
            0,
        );
        assert_eq!(
            ctx.perf_counters().get_counter(PerfCounterType::SqlWrites),
            0,
            "An empty batch issues no INSERTs",
        );

        let entries = vec![(ONES_CSID, version, vec![1; 32]); MYSQL_INSERT_CHUNK_SIZE + 1];
        assert_eq!(
            sql.store_mapping_batch(&ctx, repo_id, entries.clone(), dt, version, CONN)
                .await?,
            1,
        );
        assert_eq!(
            ctx.perf_counters().get_counter(PerfCounterType::SqlWrites),
            2,
            "One full chunk plus one entry requires two INSERTs despite ignored rows",
        );

        DropMappingsTable::query(&sql.write_connections[CONN], ctx.sql_query_telemetry()).await?;
        assert!(
            sql.store_mapping_batch(&ctx, repo_id, entries, dt, version, CONN)
                .await
                .is_err(),
        );
        assert_eq!(
            ctx.perf_counters().get_counter(PerfCounterType::SqlWrites),
            3,
            "The failed first chunk counts once; the second chunk is never attempted",
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_version_independence(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let sql = SqlCommitDerivedDataMapping::with_sqlite_in_memory()?;
        let repo_id = RepositoryId::new(1);
        let dt = DerivableType::Fsnodes;
        let value_v1 = vec![1u8; 32];
        let value_v2 = vec![2u8; 32];

        sql.store_mapping(&ctx, repo_id, ONES_CSID, dt, 1, &value_v1, CONN)
            .await?;
        sql.store_mapping(&ctx, repo_id, ONES_CSID, dt, 2, &value_v2, CONN)
            .await?;

        let result_v1 = sql
            .fetch_mapping(&ctx, repo_id, ONES_CSID, dt, 1, CONN)
            .await?;
        assert_eq!(result_v1, Some(value_v1));

        let result_v2 = sql
            .fetch_mapping(&ctx, repo_id, ONES_CSID, dt, 2, CONN)
            .await?;
        assert_eq!(result_v2, Some(value_v2));

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_insert_or_ignore(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let sql = SqlCommitDerivedDataMapping::with_sqlite_in_memory()?;
        let repo_id = RepositoryId::new(1);
        let dt = DerivableType::Fsnodes;
        let version = 1;
        let value = vec![1u8; 32];

        sql.store_mapping(&ctx, repo_id, ONES_CSID, dt, version, &value, CONN)
            .await?;

        // Second insert of same key is silently ignored
        let different_value = vec![9u8; 32];
        sql.store_mapping(
            &ctx,
            repo_id,
            ONES_CSID,
            dt,
            version,
            &different_value,
            CONN,
        )
        .await?;

        // Original value is preserved
        let result = sql
            .fetch_mapping(&ctx, repo_id, ONES_CSID, dt, version, CONN)
            .await?;
        assert_eq!(result, Some(value));

        Ok(())
    }

    fn page(rows: u32) -> NonZeroU32 {
        NonZeroU32::new(rows).expect("page sizes in tests are non-zero")
    }

    #[mononoke::fbinit_test]
    async fn test_list_and_delete_type_mappings(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let sql = SqlCommitDerivedDataMapping::with_sqlite_in_memory()?;
        let repo = RepositoryId::new(1);
        let other_repo = RepositoryId::new(2);
        let hm = DerivableType::HistoryManifests;
        let blame = DerivableType::BlameV3;
        let value = [1u8; 32];
        for cs_id in [THREES_CSID, ONES_CSID, TWOS_CSID] {
            sql.store_mapping(&ctx, repo, cs_id, hm, 1, &value, CONN)
                .await?;
        }
        for version in [2, 1] {
            sql.store_mapping(&ctx, repo, ONES_CSID, blame, version, &value, CONN)
                .await?;
        }
        sql.store_mapping(
            &ctx,
            repo,
            ONES_CSID,
            DerivableType::Fsnodes,
            1,
            &value,
            CONN,
        )
        .await?;
        sql.store_mapping(&ctx, other_repo, ONES_CSID, hm, 1, &value, CONN)
            .await?;

        assert_eq!(
            sql.list_type_mappings(&ctx, repo, blame, None, page(10), CONN)
                .await?,
            vec![(ONES_CSID, 1), (ONES_CSID, 2)],
            "every version of the type is listed"
        );
        // Paging continues within one changeset's versions.
        assert_eq!(
            sql.list_type_mappings(&ctx, repo, blame, Some((ONES_CSID, 1)), page(10), CONN)
                .await?,
            vec![(ONES_CSID, 2)]
        );
        let first = sql
            .list_type_mappings(&ctx, repo, hm, None, page(2), CONN)
            .await?;
        assert_eq!(first, vec![(ONES_CSID, 1), (TWOS_CSID, 1)]);
        assert_eq!(
            sql.list_type_mappings(&ctx, repo, hm, first.last().copied(), page(2), CONN)
                .await?,
            vec![(THREES_CSID, 1)]
        );

        assert_eq!(
            sql.delete_mappings(&ctx, repo, hm, 2, &[ONES_CSID], CONN)
                .await?,
            0,
            "only the given version is deleted"
        );
        assert_eq!(
            sql.delete_mappings(&ctx, repo, hm, 1, &[ONES_CSID, TWOS_CSID], CONN)
                .await?,
            2
        );
        assert_eq!(
            sql.list_type_mappings(&ctx, repo, hm, None, page(10), CONN)
                .await?,
            vec![(THREES_CSID, 1)]
        );
        for (repo_id, dt) in [
            (other_repo, hm),
            (repo, DerivableType::Fsnodes),
            (repo, blame),
        ] {
            assert!(
                !sql.list_type_mappings(&ctx, repo_id, dt, None, page(10), CONN)
                    .await?
                    .is_empty(),
                "{dt:?} in {repo_id} must be untouched"
            );
        }
        assert_eq!(sql.connection_count(), 1);
        Ok(())
    }

    #[mononoke::test]
    fn test_invalid_shard_id() -> Result<()> {
        let sql = SqlCommitDerivedDataMapping::with_sqlite_in_memory()?;
        // Single in-memory connection, shard id 0 is valid, 1 is not
        assert!(sql.shard_id(0).is_ok());
        assert!(sql.shard_id(1).is_err());
        Ok(())
    }
}
