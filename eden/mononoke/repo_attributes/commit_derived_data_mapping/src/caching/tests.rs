/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use context::PerfCounterType;
use fbinit::FacebookInit;
use mononoke_macros::mononoke;
use mononoke_types_mocks::changesetid::FOURS_CSID;
use mononoke_types_mocks::changesetid::ONES_CSID;
use mononoke_types_mocks::changesetid::THREES_CSID;
use mononoke_types_mocks::changesetid::TWOS_CSID;
use sql_construct::SqlConstruct;
use sql_construct::SqlShardedConstruct;
use sql_ext::SqlConnections;
use sql_ext::SqlShardedConnections;
use sql_ext::mononoke_queries;
use vec1::vec1;

use super::*;
use crate::CacheEviction;
use crate::CommitDerivedDataMapping;

const REPO: RepositoryId = RepositoryId::new(1);
const TYPE: DerivableType = DerivableType::HistoryManifests;
const VERSION: i32 = 1;
const SHARD: usize = 0;

mononoke_queries! {
    write DropMappingsTable() {
        none,
        "DROP TABLE commit_derived_data"
    }
}

fn mapping_with_mock_caches() -> Result<CommitDerivedDataMapping> {
    Ok(
        CommitDerivedDataMapping::new(SqlCommitDerivedDataMapping::with_sqlite_in_memory()?)
            .with_caching(CacheHandlerFactory::Mocked),
    )
}

fn cache(mapping: &CommitDerivedDataMapping) -> &MappingCache {
    mapping.cache.as_ref().expect("test mapping has caches")
}

#[mononoke::fbinit_test]
async fn test_sql_cachelib_and_memcache_reads(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let primary = SqlCommitDerivedDataMapping::with_sqlite_in_memory()?;
    let replica = SqlCommitDerivedDataMapping::with_sqlite_in_memory()?;
    let sql = SqlCommitDerivedDataMapping::from_sql_connections(SqlConnections {
        write_connection: primary.write_connections[SHARD].clone(),
        read_master_connection: primary.write_connections[SHARD].clone(),
        read_connection: replica.read_connections[SHARD].clone(),
    });
    let mapping = CommitDerivedDataMapping::new(sql).with_caching(CacheHandlerFactory::Mocked);
    let cachelib = cache(&mapping).cachelib.mock_store().unwrap();
    let memcache = cache(&mapping).memcache.mock_store().unwrap();
    let value = vec![1; 32];

    mapping
        .store_mapping(&ctx, REPO, ONES_CSID, TYPE, VERSION, &value, SHARD)
        .await?;
    assert_eq!(cachelib.stats().sets, 0);
    assert_eq!(memcache.stats().sets, 0);
    assert_eq!(
        mapping
            .fetch_mapping(&ctx, REPO, ONES_CSID, TYPE, VERSION, SHARD)
            .await?,
        Some(value.clone()),
    );
    assert_eq!(cachelib.stats().sets, 1);
    assert_eq!(memcache.stats().sets, 1);

    DropMappingsTable::query(&primary.write_connections[SHARD], ctx.sql_query_telemetry()).await?;
    DropMappingsTable::query(&replica.write_connections[SHARD], ctx.sql_query_telemetry()).await?;
    assert_eq!(
        mapping
            .fetch_mapping(&ctx, REPO, ONES_CSID, TYPE, VERSION, SHARD)
            .await?,
        Some(value.clone()),
    );
    assert_eq!(cachelib.stats().hits, 1);
    assert_eq!(memcache.stats().gets, 1);

    cachelib.flush();
    assert_eq!(
        mapping
            .fetch_mapping_batch(&ctx, REPO, vec![ONES_CSID, ONES_CSID], TYPE, VERSION, SHARD,)
            .await?,
        vec![(ONES_CSID, value.clone())],
    );
    assert_eq!(memcache.stats().hits, 1);
    assert_eq!(cachelib.stats().sets, 2);

    assert_eq!(
        mapping
            .fetch_mapping(&ctx, REPO, ONES_CSID, TYPE, VERSION, SHARD)
            .await?,
        Some(value),
    );
    assert_eq!(cachelib.stats().hits, 2);
    assert_eq!(memcache.stats().gets, 2);
    assert_eq!(
        ctx.perf_counters().get_counter(PerfCounterType::SqlWrites),
        1,
        "The cache wrapper does not double-count the INSERT",
    );
    assert_eq!(
        ctx.perf_counters()
            .get_counter(PerfCounterType::SqlReadsReplica),
        1,
        "Only the initial cache miss reads the replica",
    );
    assert_eq!(
        ctx.perf_counters()
            .get_counter(PerfCounterType::SqlReadsMaster),
        1,
        "Only the initial cache miss falls back to the primary",
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_missing_mappings_are_not_cached(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let mapping = mapping_with_mock_caches()?;
    let cachelib = cache(&mapping).cachelib.mock_store().unwrap();
    let memcache = cache(&mapping).memcache.mock_store().unwrap();

    assert_eq!(
        mapping
            .fetch_mapping(&ctx, REPO, ONES_CSID, TYPE, VERSION, SHARD)
            .await?,
        None,
    );
    assert!(
        mapping
            .fetch_mapping_batch(&ctx, REPO, vec![ONES_CSID, TWOS_CSID], TYPE, VERSION, SHARD)
            .await?
            .is_empty(),
    );
    assert_eq!(cachelib.stats().sets, 0);
    assert_eq!(memcache.stats().sets, 0);

    let entries = vec![
        (ONES_CSID, VERSION, vec![1; 32]),
        (TWOS_CSID, VERSION, vec![2; 32]),
    ];
    mapping
        .store_mapping_batch(&ctx, REPO, entries, TYPE, VERSION, SHARD)
        .await?;
    let rows = mapping
        .fetch_mapping_batch(&ctx, REPO, vec![ONES_CSID, TWOS_CSID], TYPE, VERSION, SHARD)
        .await?;
    assert_eq!(
        rows.into_iter().collect::<HashMap<_, _>>(),
        HashMap::from([(ONES_CSID, vec![1; 32]), (TWOS_CSID, vec![2; 32])]),
    );
    assert_eq!(cachelib.stats().sets, 2);
    assert_eq!(memcache.stats().sets, 2);

    let local_before = cachelib.stats();
    let shared_before = memcache.stats();
    assert!(
        mapping
            .fetch_mapping_batch(&ctx, REPO, vec![], TYPE, VERSION, SHARD)
            .await?
            .is_empty(),
    );
    assert_eq!(cachelib.stats(), local_before);
    assert_eq!(memcache.stats(), shared_before);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_batch_combines_cachelib_memcache_and_sql(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let mapping = mapping_with_mock_caches()?;
    let cachelib = cache(&mapping).cachelib.mock_store().unwrap();
    let memcache = cache(&mapping).memcache.mock_store().unwrap();
    let entries = vec![
        (ONES_CSID, VERSION, vec![1; 32]),
        (TWOS_CSID, VERSION, vec![2; 32]),
        (THREES_CSID, VERSION, vec![3; 32]),
    ];
    mapping
        .store_mapping_batch(&ctx, REPO, entries, TYPE, VERSION, SHARD)
        .await?;
    mapping
        .fetch_mapping(&ctx, REPO, ONES_CSID, TYPE, VERSION, SHARD)
        .await?;
    cachelib.flush();
    mapping
        .fetch_mapping(&ctx, REPO, TWOS_CSID, TYPE, VERSION, SHARD)
        .await?;

    let rows = mapping
        .fetch_mapping_batch(
            &ctx,
            REPO,
            vec![ONES_CSID, TWOS_CSID, THREES_CSID, FOURS_CSID, ONES_CSID],
            TYPE,
            VERSION,
            SHARD,
        )
        .await?;
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.into_iter().collect::<HashMap<_, _>>(),
        HashMap::from([
            (ONES_CSID, vec![1; 32]),
            (TWOS_CSID, vec![2; 32]),
            (THREES_CSID, vec![3; 32]),
        ]),
    );
    assert_eq!(cachelib.stats().hits, 1);
    assert_eq!(memcache.stats().hits, 1);
    assert_eq!(memcache.stats().sets, 3);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_cache_key_isolation(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let first = SqlCommitDerivedDataMapping::with_sqlite_in_memory()?;
    let second = SqlCommitDerivedDataMapping::with_sqlite_in_memory()?;
    let connections = vec1![
        first.write_connections[SHARD].clone(),
        second.write_connections[SHARD].clone(),
    ];
    let sql = SqlCommitDerivedDataMapping::from_sql_shard_connections(SqlShardedConnections {
        write_connections: connections.clone(),
        read_connections: connections.clone(),
        read_master_connections: connections,
    });
    let mapping = CommitDerivedDataMapping::new(sql).with_caching(CacheHandlerFactory::Mocked);
    let entries = [
        (REPO, ONES_CSID, TYPE, VERSION, SHARD, 1),
        (RepositoryId::new(2), ONES_CSID, TYPE, VERSION, SHARD, 2),
        (REPO, ONES_CSID, DerivableType::Fsnodes, VERSION, SHARD, 3),
        (REPO, ONES_CSID, TYPE, VERSION + 1, SHARD, 4),
        (REPO, ONES_CSID, TYPE, VERSION, SHARD + 1, 5),
        (REPO, TWOS_CSID, TYPE, VERSION, SHARD, 6),
    ];

    for (repo, cs_id, dt, version, shard, value) in entries {
        mapping
            .store_mapping(&ctx, repo, cs_id, dt, version, &[value; 32], shard)
            .await?;
        assert_eq!(
            mapping
                .fetch_mapping(&ctx, repo, cs_id, dt, version, shard)
                .await?,
            Some(vec![value; 32]),
        );
    }
    for connection in mapping.sql.write_connections.iter() {
        DropMappingsTable::query(connection, ctx.sql_query_telemetry()).await?;
    }
    for (repo, cs_id, dt, version, shard, value) in entries {
        assert_eq!(
            mapping
                .fetch_mapping(&ctx, repo, cs_id, dt, version, shard)
                .await?,
            Some(vec![value; 32]),
        );
    }
    cache(&mapping).cachelib.mock_store().unwrap().flush();
    for (repo, cs_id, dt, version, shard, value) in entries {
        assert_eq!(
            mapping
                .fetch_mapping(&ctx, repo, cs_id, dt, version, shard)
                .await?,
            Some(vec![value; 32]),
        );
    }
    assert!(
        mapping
            .fetch_mapping(&ctx, REPO, ONES_CSID, TYPE, VERSION, 2)
            .await
            .is_err(),
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_ignored_writes_do_not_poison_cache(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let mapping = mapping_with_mock_caches()?;
    mapping
        .store_mapping(&ctx, REPO, ONES_CSID, TYPE, VERSION, &[1; 32], SHARD)
        .await?;
    mapping
        .store_mapping(&ctx, REPO, ONES_CSID, TYPE, VERSION, &[9; 32], SHARD)
        .await?;
    assert_eq!(
        cache(&mapping).cachelib.mock_store().unwrap().stats().sets,
        0
    );
    assert_eq!(
        mapping
            .fetch_mapping(&ctx, REPO, ONES_CSID, TYPE, VERSION, SHARD)
            .await?,
        Some(vec![1; 32]),
    );
    let entries = vec![
        (ONES_CSID, VERSION, vec![9; 32]),
        (TWOS_CSID, VERSION, vec![2; 32]),
    ];
    mapping
        .store_mapping_batch(&ctx, REPO, entries, TYPE, VERSION, SHARD)
        .await?;
    let rows = mapping
        .fetch_mapping_batch(&ctx, REPO, vec![ONES_CSID, TWOS_CSID], TYPE, VERSION, SHARD)
        .await?;
    assert_eq!(
        rows.into_iter().collect::<HashMap<_, _>>(),
        HashMap::from([(ONES_CSID, vec![1; 32]), (TWOS_CSID, vec![2; 32])]),
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_malformed_memcache_value_falls_back_to_sql(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let mapping = mapping_with_mock_caches()?;
    mapping
        .store_mapping(&ctx, REPO, ONES_CSID, TYPE, VERSION, &[1; 32], SHARD)
        .await?;
    let request = CacheRequest {
        ctx: &ctx,
        sql: &mapping.sql,
        cache: cache(&mapping),
        repo_id: REPO,
        derived_data_type: TYPE,
        derived_data_version: VERSION,
        shard_id: SHARD,
    };
    let key = request.keygen().key(request.get_cache_key(&ONES_CSID));
    request
        .memcache()
        .mock_store()
        .unwrap()
        .set(&key, Bytes::from_static(&[0xff]));

    assert_eq!(
        mapping
            .fetch_mapping(&ctx, REPO, ONES_CSID, TYPE, VERSION, SHARD)
            .await?,
        Some(vec![1; 32]),
    );
    cache(&mapping).cachelib.mock_store().unwrap().flush();
    DropMappingsTable::query(
        &mapping.sql.write_connections[SHARD],
        ctx.sql_query_telemetry(),
    )
    .await?;
    assert_eq!(
        mapping
            .fetch_mapping(&ctx, REPO, ONES_CSID, TYPE, VERSION, SHARD)
            .await?,
        Some(vec![1; 32]),
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_evict_shared_cache(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let mapping = mapping_with_mock_caches()?;
    for cs_id in [ONES_CSID, TWOS_CSID] {
        mapping
            .store_mapping(&ctx, REPO, cs_id, TYPE, VERSION, &[1; 32], SHARD)
            .await?;
    }
    mapping
        .fetch_mapping_batch(&ctx, REPO, vec![ONES_CSID, TWOS_CSID], TYPE, VERSION, SHARD)
        .await?;
    let request = CacheRequest {
        ctx: &ctx,
        sql: &mapping.sql,
        cache: cache(&mapping),
        repo_id: REPO,
        derived_data_type: TYPE,
        derived_data_version: VERSION,
        shard_id: SHARD,
    };
    let memcache = cache(&mapping).memcache.mock_store().unwrap();
    let key = |cs_id| request.keygen().key(request.get_cache_key(&cs_id));
    assert!(memcache.get(&key(ONES_CSID)).is_some());
    assert!(memcache.get(&key(TWOS_CSID)).is_some());

    assert_eq!(
        mapping
            .evict_shared_cache(REPO, TYPE, VERSION, SHARD, &[ONES_CSID])
            .await?,
        CacheEviction::Evicted
    );
    assert!(memcache.get(&key(ONES_CSID)).is_none(), "evicted exactly");
    assert!(memcache.get(&key(TWOS_CSID)).is_some(), "others are kept");
    assert!(
        mapping
            .evict_shared_cache(REPO, TYPE, VERSION, SHARD + 1, &[ONES_CSID])
            .await
            .is_err(),
        "an unknown shard is rejected"
    );

    let uncached =
        CommitDerivedDataMapping::new(SqlCommitDerivedDataMapping::with_sqlite_in_memory()?);
    assert_eq!(
        uncached
            .evict_shared_cache(REPO, TYPE, VERSION, SHARD, &[ONES_CSID])
            .await?,
        CacheEviction::NoSharedCache
    );
    assert!(
        uncached
            .evict_shared_cache(REPO, TYPE, VERSION, SHARD + 1, &[ONES_CSID])
            .await
            .is_err(),
        "an unknown shard is rejected even without a shared cache"
    );
    Ok(())
}
