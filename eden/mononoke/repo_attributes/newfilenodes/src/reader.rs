/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::borrow::Cow;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Error;
use anyhow::Result;
use context::CoreContext;
use context::PerfCounterType;
use faster_hex::hex_encode;
use filenodes::FilenodeInfo;
use filenodes::FilenodeRange;
use filenodes::FilenodeResult;
use futures::future;
use futures::future::Future;
use itertools::Itertools;
use mercurial_types::HgChangesetId;
use mercurial_types::HgFileNodeId;
use mononoke_types::RepoPath;
use mononoke_types::RepositoryId;
use mononoke_types::Timestamp;
use path_hash::PathBytes;
use path_hash::PathHashBytes;
use path_hash::PathWithHash;
use sql_ext::Connection;
use sql_ext::ConsistentReadOptions;
use sql_ext::SqlConnections;
use sql_ext::consistent_read_options;
use sql_ext::mononoke_queries;
use stats::prelude::*;
use thiserror::Error;
use tokio::time::error::Elapsed;
use tokio::time::timeout;
use vec1::Vec1;

use crate::connections::AcquireReason;
use crate::connections::Connections;
use crate::local_cache::CacheKey;
use crate::local_cache::LocalCache;
use crate::remote_cache::RemoteCache;
use crate::shards::Shards;
use crate::sql_timeout_knobs;

define_stats! {
    prefix = "mononoke.filenodes";
    gets: timeseries(Sum),
    gets_master: timeseries(Sum),
    gets_disabled: timeseries(Sum),
    range_gets: timeseries(Sum),
    range_gets_disabled: timeseries(Sum),
    path_gets: timeseries(Sum),
    get_local_cache_misses: timeseries(Sum),
    range_local_cache_misses: timeseries(Sum),
    remote_cache_timeouts: timeseries(Sum),
    sql_timeouts: timeseries(Sum),
    too_big_history: timeseries(Sum),
}

// Both of these are pretty conservative, and collected experimentally. They're here to ensure one
// bad query doesn't lock down an entire shard for an extended period of time.
const REMOTE_CACHE_TIMEOUT_MILLIS: u64 = 100;

#[derive(Debug, Error)]
pub enum FilenodeReaderError {
    #[error("Filenodes internal error: path is not found: {0:?}")]
    PathNotFound(PathHashBytes),

    #[error("Filenodes internal error: invalid path: {0:?}")]
    InvalidPath(PathBytes, #[source] Error),

    #[error("Filenodes internal error: fixedcopyinfo is missing for filenode: {0:?}")]
    FixedCopyInfoMissing(HgFileNodeId),

    #[error("Filenodes internal error: SQL error")]
    SqlError(#[source] Error),

    #[error("Filenodes internal error: SQL timeout after {0}")]
    SqlTimeout(Elapsed),
}

struct PerfCounterRecorder<'a> {
    ctx: &'a CoreContext,
    counter: PerfCounterType,
}

impl<'a> PerfCounterRecorder<'a> {
    fn increment(&self) {
        self.ctx.perf_counters().increment_counter(self.counter);
    }
}

type FilenodeRow = (
    HgFileNodeId,
    HgChangesetId,
    Option<HgFileNodeId>,
    Option<HgFileNodeId>,
    i8,
    Option<PathHashBytes>,
    Option<HgFileNodeId>,
);

pub fn filenode_cache_key(
    repo_id: RepositoryId,
    pwh: &PathWithHash<'_>,
    filenode: &HgFileNodeId,
) -> CacheKey<FilenodeInfo> {
    let mut v = vec![0; pwh.hash.0.len() * 2];
    let is_tree = pwh.is_tree as u8;
    hex_encode(pwh.hash.0.as_ref(), &mut v).expect("failed to hex encode");
    let key = format!(
        "filenode.{}.{}.{}.{}",
        repo_id.id(),
        filenode,
        unsafe { String::from_utf8_unchecked(v) },
        is_tree
    );

    CacheKey {
        key,
        value: PhantomData,
    }
}

pub fn history_cache_key(
    repo_id: RepositoryId,
    pwh: &PathWithHash<'_>,
    limit: Option<u64>,
) -> CacheKey<FilenodeRange> {
    let mut v = vec![0; pwh.hash.0.len() * 2];
    let is_tree = pwh.is_tree as u8;
    hex_encode(pwh.hash.0.as_ref(), &mut v).expect("failed to hex encode");
    let key = match limit {
        Some(limit) => format!(
            "history.{}.limit.{}.{}.{}",
            repo_id.id(),
            limit,
            unsafe { String::from_utf8_unchecked(v) },
            is_tree
        ),
        None => format!(
            "history.{}.{}.{}",
            repo_id.id(),
            unsafe { String::from_utf8_unchecked(v) },
            is_tree
        ),
    };

    CacheKey {
        key,
        value: PhantomData,
    }
}

struct ReadConnections {
    read_connections: Connections,
    read_master_connections: Connections,
}

pub struct FilenodesReader {
    connections: ReadConnections,
    shards: Arc<Shards>,
    pub local_cache: LocalCache,
    pub remote_cache: RemoteCache,
}

impl FilenodesReader {
    pub fn new(
        read_connections: Vec1<Connection>,
        read_master_connections: Vec1<Connection>,
    ) -> Result<Self> {
        Ok(Self {
            shards: Arc::new(Shards::new(1000, 1000)),
            connections: ReadConnections {
                read_connections: Connections::new(read_connections),
                read_master_connections: Connections::new(read_master_connections),
            },
            local_cache: LocalCache::new_noop(),
            remote_cache: RemoteCache::new_noop()?,
        })
    }

    pub async fn get_filenode(
        self: Arc<Self>,
        ctx: &CoreContext,
        repo_id: RepositoryId,
        path: &RepoPath,
        filenode: HgFileNodeId,
    ) -> Result<FilenodeResult<Option<FilenodeInfo>>> {
        STATS::gets.add_value(1);

        let pwh = PathWithHash::from_repo_path_cow(Cow::Owned(path.clone()));
        let key = filenode_cache_key(repo_id, &pwh, &filenode);

        if let Some(cached) = self.local_cache.get_filenode(&key) {
            return Ok(FilenodeResult::Present(Some(cached)));
        }

        let ctx = ctx.clone();
        self.shards
            .clone()
            .with_filenodes(path, filenode, move || {
                async move {
                    // Now that we acquired the permit, check our cache again, in case the previous permit
                    // owner just filed the cache with the filenode we're looking for.
                    if let Some(cached) = self.local_cache.get_filenode(&key) {
                        return Ok(FilenodeResult::Present(Some(cached)));
                    }

                    STATS::get_local_cache_misses.add_value(1);

                    if let Some(info) =
                        enforce_remote_cache_timeout(self.remote_cache.get_filenode(&key)).await
                    {
                        self.local_cache.fill_filenode(&key, &info);
                        return Ok(FilenodeResult::Present(Some(info)));
                    }

                    let cache_filler = FilenodeCacheFiller {
                        local_cache: &self.local_cache,
                        remote_cache: &self.remote_cache,
                        key: &key,
                    };

                    let cons_read_opts = consistent_read_options(
                        ctx.client_correlator(),
                        Some("newfilenodes::reader"),
                    )
                    .map_err(FilenodeReaderError::SqlError)?;

                    let used_consistent_reads = cons_read_opts.is_some();

                    let read_result = select_filenode_from_sql(
                        &ctx,
                        cache_filler,
                        &self.connections.read_connections,
                        &self.connections.read_master_connections,
                        repo_id,
                        &pwh,
                        filenode,
                        &PerfCounterRecorder {
                            ctx: &ctx,
                            counter: PerfCounterType::SqlReadsReplica,
                        },
                        cons_read_opts,
                    )
                    .await;

                    if used_consistent_reads {
                        // If we used consistent reads, then we should always
                        // get the correct result back and the fallback to master
                        // is not needed.
                        Ok(read_result?)
                    } else {
                        // If consistent read query was not used, then check
                        // the result and possibly fallback to reading from master
                        match read_result {
                            Ok(FilenodeResult::Present(None))
                            | Err(FilenodeReaderError::FixedCopyInfoMissing(_))
                            | Err(FilenodeReaderError::PathNotFound(_)) => {
                                // If the filenode wasn't found, or its copy info was missing, it might be present
                                // on the master.
                                STATS::gets_master.add_value(1);

                                Ok(select_filenode_from_sql(
                                    &ctx,
                                    cache_filler,
                                    &self.connections.read_master_connections,
                                    &self.connections.read_master_connections,
                                    repo_id,
                                    &pwh,
                                    filenode,
                                    &PerfCounterRecorder {
                                        ctx: &ctx,
                                        counter: PerfCounterType::SqlReadsMaster,
                                    },
                                    None,
                                )
                                .await?)
                            }
                            res => Ok(res?),
                        }
                    }
                }
            })
            .await?
    }

    pub async fn get_all_filenodes_for_path(
        self: Arc<Self>,
        ctx: &CoreContext,
        repo_id: RepositoryId,
        path: &RepoPath,
        limit: Option<u64>,
    ) -> Result<FilenodeResult<FilenodeRange>> {
        STATS::range_gets.add_value(1);

        let pwh = PathWithHash::from_repo_path_cow(Cow::Owned(path.clone()));
        let key = history_cache_key(repo_id, &pwh, limit);

        if let Some(cached) = self.local_cache.get_history(&key) {
            return Ok(FilenodeResult::Present(cached));
        }
        let ctx = ctx.clone();
        self.shards
            .clone()
            .with_history(path, move || {
                async move {
                    // See above for rationale here.
                    if let Some(cached) = self.local_cache.get_history(&key) {
                        return Ok(FilenodeResult::Present(cached));
                    }

                    STATS::range_local_cache_misses.add_value(1);

                    if let Some(info) =
                        enforce_remote_cache_timeout(self.remote_cache.get_history(&key)).await
                    {
                        // TODO: We should compress if this is too big.
                        self.local_cache.fill_history(&key, &info);
                        return Ok(FilenodeResult::Present(info));
                    }

                    let cache_filler = HistoryCacheFiller {
                        local_cache: &self.local_cache,
                        remote_cache: &self.remote_cache,
                        key: &key,
                    };

                    select_history_from_sql(
                        &ctx,
                        &cache_filler,
                        &self.connections.read_connections,
                        repo_id,
                        &pwh,
                        &PerfCounterRecorder {
                            ctx: &ctx,
                            counter: PerfCounterType::SqlReadsReplica,
                        },
                        limit,
                    )
                    .await
                }
            })
            .await?
    }
}

#[derive(Copy, Clone)]
struct FilenodeCacheFiller<'a> {
    local_cache: &'a LocalCache,
    remote_cache: &'a RemoteCache,
    key: &'a CacheKey<FilenodeInfo>,
}

impl<'a> FilenodeCacheFiller<'a> {
    fn fill(&self, filenode: FilenodeInfo) {
        self.local_cache.fill_filenode(self.key, &filenode);
        self.remote_cache.fill_filenode(self.key, filenode);
    }
}

struct PartialFilenode {
    filenode: HgFileNodeId,
    p1: Option<HgFileNodeId>,
    p2: Option<HgFileNodeId>,
    copyfrom: Option<(PathHashBytes, HgFileNodeId)>,
    linknode: HgChangesetId,
}

async fn select_filenode_from_sql(
    ctx: &CoreContext,
    filler: FilenodeCacheFiller<'_>,
    read_connections: &Connections,
    read_master_connections: &Connections,
    repo_id: RepositoryId,
    pwh: &PathWithHash<'_>,
    filenode: HgFileNodeId,
    recorder: &PerfCounterRecorder<'_>,
    cons_read_opts: Option<ConsistentReadOptions>,
) -> Result<FilenodeResult<Option<FilenodeInfo>>, FilenodeReaderError> {
    let partial = select_partial_filenode(
        ctx,
        read_connections,
        read_master_connections,
        repo_id,
        pwh,
        filenode,
        recorder,
        cons_read_opts,
    )
    .await?;

    let partial = match partial {
        Some(partial) => partial,
        None => {
            return Ok(FilenodeResult::Present(None));
        }
    };

    let ret = match fill_paths(ctx, read_connections, pwh, repo_id, vec![partial], recorder)
        .await?
        .into_iter()
        .next()
    {
        Some(ret) => ret,
        None => {
            return Ok(FilenodeResult::Present(None));
        }
    };

    filler.fill(ret.clone());

    Ok(FilenodeResult::Present(Some(ret)))
}

async fn select_partial_filenode(
    ctx: &CoreContext,
    read_connections: &Connections,
    read_master_connections: &Connections,
    repo_id: RepositoryId,
    pwh: &PathWithHash<'_>,
    filenode: HgFileNodeId,
    recorder: &PerfCounterRecorder<'_>,
    cons_read_opts: Option<ConsistentReadOptions>,
) -> Result<Option<PartialFilenode>, FilenodeReaderError> {
    let connection = read_connections.checkout(pwh, AcquireReason::Filenodes);

    recorder.increment();

    let rows = if let Some(cons_read_opts) = cons_read_opts {
        // Create SqlConnections where write_connection has the read connection
        // to ensure writes would never succeed if somebody tries to use it later
        let read_master_connection =
            read_master_connections.checkout(pwh, AcquireReason::Filenodes);
        let sql_connections = SqlConnections {
            // The write connections won't be used, but pass the read-only
            // connection to ensure that it can't be used by accident in the future.
            write_connection: connection.clone(),
            read_connection: connection.clone(),
            read_master_connection: read_master_connection.clone(),
        };

        // Return early if filenode is present in the replica
        let return_early_if: Arc<Box<dyn for<'a> Fn(&'a Vec<FilenodeRow>) -> bool + Send + Sync>> =
            Arc::new(Box::new(|rows| {
                rows.iter()
                    .next()
                    .and_then(|row| convert_row_to_partial_filenode(row.clone()).ok())
                    .is_some()
            }));

        enforce_sql_timeout(SelectFilenode::query_with_consistency(
            &sql_connections,
            ctx.sql_query_telemetry(),
            // TODO(T237806453): should we specify a `target_lower_bound_hlc`?
            None,
            Some(return_early_if),
            cons_read_opts,
            &repo_id,
            &pwh.hash,
            pwh.sql_is_tree(),
            &filenode,
        ))
        .await?
    } else {
        enforce_sql_timeout(SelectFilenode::query(
            connection,
            ctx.sql_query_telemetry(),
            &repo_id,
            &pwh.hash,
            pwh.sql_is_tree(),
            &filenode,
        ))
        .await?
    };

    match rows.into_iter().next() {
        Some(row) => {
            let partial = convert_row_to_partial_filenode(row)?;
            Ok(Some(partial))
        }
        None => Ok(None),
    }
}

#[derive(Copy, Clone)]
struct HistoryCacheFiller<'a> {
    local_cache: &'a LocalCache,
    remote_cache: &'a RemoteCache,
    key: &'a CacheKey<FilenodeRange>,
}

impl<'a> HistoryCacheFiller<'a> {
    fn fill(&self, history: FilenodeRange) {
        self.local_cache.fill_history(self.key, &history);
        self.remote_cache.fill_history(self.key, history);
    }
}

async fn select_history_from_sql(
    ctx: &CoreContext,
    filler: &HistoryCacheFiller<'_>,
    connections: &Connections,
    repo_id: RepositoryId,
    pwh: &PathWithHash<'_>,
    recorder: &PerfCounterRecorder<'_>,
    limit: Option<u64>,
) -> Result<FilenodeResult<FilenodeRange>> {
    let maybe_partial =
        select_partial_history(ctx, connections, repo_id, pwh, recorder, limit).await?;
    if let Some(partial) = maybe_partial {
        let history = FilenodeRange::Filenodes(
            fill_paths(ctx, connections, pwh, repo_id, partial, recorder).await?,
        );
        filler.fill(history.clone());
        Ok(FilenodeResult::Present(history))
    } else {
        filler.fill(FilenodeRange::TooBig);
        Ok(FilenodeResult::Present(FilenodeRange::TooBig))
    }
}

async fn select_partial_history(
    ctx: &CoreContext,
    connections: &Connections,
    repo_id: RepositoryId,
    pwh: &PathWithHash<'_>,
    recorder: &PerfCounterRecorder<'_>,
    limit: Option<u64>,
) -> Result<Option<Vec<PartialFilenode>>, FilenodeReaderError> {
    let connection = connections.checkout(pwh, AcquireReason::History);

    recorder.increment();

    // Try to fetch one entry more - if we fetched limit + 1, then file
    // history is too big.
    let limit = limit.map(|l| l + 1);
    let rows = match limit {
        Some(limit) => {
            let rows = enforce_sql_timeout(SelectLimitedFilenodes::query(
                connection,
                ctx.sql_query_telemetry(),
                &repo_id,
                &pwh.hash,
                pwh.sql_is_tree(),
                &limit,
            ))
            .await?;
            if rows.len() >= limit as usize {
                STATS::too_big_history.add_value(1);
                return Ok(None);
            }
            rows
        }
        None => {
            enforce_sql_timeout(SelectAllFilenodes::query(
                connection,
                ctx.sql_query_telemetry(),
                &repo_id,
                &pwh.hash,
                pwh.sql_is_tree(),
            ))
            .await?
        }
    };

    let history = rows
        .into_iter()
        .map(convert_row_to_partial_filenode)
        .collect::<Result<Vec<PartialFilenode>, FilenodeReaderError>>()?;

    // TODO: It'd be nice to have some eviction here.
    // TODO: It'd be nice to chain those.

    Ok(Some(history))
}

fn convert_row_to_partial_filenode(
    row: FilenodeRow,
) -> Result<PartialFilenode, FilenodeReaderError> {
    let (filenode, linknode, p1, p2, has_copyinfo, from_path_hash, from_node) = row;

    let copyfrom = if has_copyinfo == 0 {
        None
    } else {
        let from_path_hash =
            from_path_hash.ok_or(FilenodeReaderError::FixedCopyInfoMissing(filenode))?;

        let from_node = from_node.ok_or(FilenodeReaderError::FixedCopyInfoMissing(filenode))?;

        Some((from_path_hash, from_node))
    };

    let ret = PartialFilenode {
        filenode,
        p1,
        p2,
        copyfrom,
        linknode,
    };

    Ok(ret)
}

async fn fill_paths(
    ctx: &CoreContext,
    connections: &Connections,
    pwh: &PathWithHash<'_>,
    repo_id: RepositoryId,
    rows: Vec<PartialFilenode>,
    recorder: &PerfCounterRecorder<'_>,
) -> Result<Vec<FilenodeInfo>, FilenodeReaderError> {
    let path_hashes_to_fetch = rows
        .iter()
        .filter_map(|r| r.copyfrom.as_ref().map(|c| c.0.clone()));

    let path_hashes_to_paths =
        select_paths(ctx, connections, repo_id, path_hashes_to_fetch, recorder).await?;

    let ret = rows
        .into_iter()
        .map(|partial| {
            let PartialFilenode {
                filenode,
                p1,
                p2,
                copyfrom,
                linknode,
            } = partial;

            let copyfrom = match copyfrom {
                Some((from_path_hash, from_node)) => {
                    let from_path = path_hashes_to_paths
                        .get(&from_path_hash)
                        .ok_or_else(|| FilenodeReaderError::PathNotFound(from_path_hash.clone()))?;
                    let repo_path = if pwh.is_tree {
                        RepoPath::dir(&from_path.0[..])
                            .map_err(|e| FilenodeReaderError::InvalidPath(from_path.clone(), e))?
                    } else {
                        RepoPath::file(&from_path.0[..])
                            .map_err(|e| FilenodeReaderError::InvalidPath(from_path.clone(), e))?
                    };
                    Some((repo_path, from_node))
                }
                None => None,
            };

            let ret = FilenodeInfo {
                filenode,
                p1,
                p2,
                copyfrom,
                linknode,
            };

            Result::<_, FilenodeReaderError>::Ok(ret)
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ret)
}

async fn select_paths<I: Iterator<Item = PathHashBytes>>(
    ctx: &CoreContext,
    connections: &Connections,
    repo_id: RepositoryId,
    iter: I,
    recorder: &PerfCounterRecorder<'_>,
) -> Result<HashMap<PathHashBytes, PathBytes>, FilenodeReaderError> {
    let futs = iter
        .chunk_by(|path_hash| connections.shard_id(path_hash))
        .into_iter()
        .map(|(shard_id, group)| {
            let group = group.collect::<Vec<_>>();

            STATS::path_gets.add_value(group.len() as i64);
            let ctx = ctx.clone();
            async move {
                recorder.increment();

                let connection = connections.checkout_by_shard_id(shard_id, AcquireReason::Paths);

                let output = enforce_sql_timeout(SelectPaths::query(
                    connection,
                    ctx.sql_query_telemetry(),
                    &repo_id,
                    &group[..],
                ))
                .await?
                .into_iter()
                .collect::<HashMap<_, _>>();

                Result::<_, FilenodeReaderError>::Ok(output)
            }
        })
        .collect::<Vec<_>>();

    let groups = future::try_join_all(futs).await?;

    let mut ret = HashMap::new();
    for group in groups {
        ret.extend(group);
    }

    Ok(ret)
}

async fn enforce_remote_cache_timeout<T, Fut>(fut: Fut) -> Option<T>
where
    Fut: Future<Output = Option<T>>,
{
    match timeout(Duration::from_millis(REMOTE_CACHE_TIMEOUT_MILLIS), fut).await {
        Ok(r) => r,
        Err(_) => {
            STATS::remote_cache_timeouts.add_value(1);
            None
        }
    }
}

async fn enforce_sql_timeout<T, Fut>(fut: Fut) -> Result<T, FilenodeReaderError>
where
    Fut: Future<Output = Result<T>>,
{
    if !sql_timeout_knobs::should_enforce_sql_timeouts() {
        return fut.await.map_err(FilenodeReaderError::SqlError);
    }

    let sql_timeout_ms = justknobs::get_as::<u64>(
        "scm/mononoke_timeouts:remote_derivation_client_timeout_ms",
        None,
    );

    match timeout(Duration::from_millis(sql_timeout_ms), fut).await {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(e)) => Err(FilenodeReaderError::SqlError(e)),
        Err(e) => {
            STATS::sql_timeouts.add_value(1);
            Err(FilenodeReaderError::SqlTimeout(e))
        }
    }
}

mononoke_queries! {
    read SelectFilenode(
        repo_id: RepositoryId,
        path_hash: PathHashBytes,
        is_tree: i8,
        filenode: HgFileNodeId
    ) -> (
        HgFileNodeId,
        HgChangesetId,
        Option<HgFileNodeId>,
        Option<HgFileNodeId>,
        i8,
        Option<PathHashBytes>,
        Option<HgFileNodeId>,
    ) {
        "
        SELECT
            filenodes.filenode,
            filenodes.linknode,
            filenodes.p1,
            filenodes.p2,
            filenodes.has_copyinfo,
            fixedcopyinfo.frompath_hash,
            fixedcopyinfo.fromnode
        FROM filenodes
        LEFT JOIN fixedcopyinfo
           ON (
                   fixedcopyinfo.repo_id = filenodes.repo_id
               AND fixedcopyinfo.topath_hash = filenodes.path_hash
               AND fixedcopyinfo.tonode = filenodes.filenode
               AND fixedcopyinfo.is_tree = filenodes.is_tree
           )
        WHERE filenodes.repo_id = {repo_id}
          AND filenodes.path_hash = {path_hash}
          AND filenodes.is_tree = {is_tree}
          AND filenodes.filenode = {filenode}
        LIMIT 1
        "
    }


    read SelectAllFilenodes(
        repo_id: RepositoryId,
        path_hash: PathHashBytes,
        is_tree: i8,
    ) -> (
        HgFileNodeId,
        HgChangesetId,
        Option<HgFileNodeId>,
        Option<HgFileNodeId>,
        i8,
        Option<PathHashBytes>,
        Option<HgFileNodeId>,
    ) {
        "
        SELECT
            filenodes.filenode,
            filenodes.linknode,
            filenodes.p1,
            filenodes.p2,
            filenodes.has_copyinfo,
            fixedcopyinfo.frompath_hash,
            fixedcopyinfo.fromnode
        FROM filenodes
        LEFT JOIN fixedcopyinfo
           ON (
                   fixedcopyinfo.repo_id = filenodes.repo_id
               AND fixedcopyinfo.topath_hash = filenodes.path_hash
               AND fixedcopyinfo.tonode = filenodes.filenode
               AND fixedcopyinfo.is_tree = filenodes.is_tree
           )
        WHERE filenodes.repo_id = {repo_id}
          AND filenodes.path_hash = {path_hash}
          AND filenodes.is_tree = {is_tree}
        "
    }

    read SelectLimitedFilenodes(
        repo_id: RepositoryId,
        path_hash: PathHashBytes,
        is_tree: i8,
        limit: u64,
    ) -> (
        HgFileNodeId,
        HgChangesetId,
        Option<HgFileNodeId>,
        Option<HgFileNodeId>,
        i8,
        Option<PathHashBytes>,
        Option<HgFileNodeId>,
    ) {
        "
        SELECT
            filenodes.filenode,
            filenodes.linknode,
            filenodes.p1,
            filenodes.p2,
            filenodes.has_copyinfo,
            fixedcopyinfo.frompath_hash,
            fixedcopyinfo.fromnode
        FROM filenodes
        LEFT JOIN fixedcopyinfo
           ON (
                   fixedcopyinfo.repo_id = filenodes.repo_id
               AND fixedcopyinfo.topath_hash = filenodes.path_hash
               AND fixedcopyinfo.tonode = filenodes.filenode
               AND fixedcopyinfo.is_tree = filenodes.is_tree
           )
        WHERE filenodes.repo_id = {repo_id}
          AND filenodes.path_hash = {path_hash}
          AND filenodes.is_tree = {is_tree}
        LIMIT {limit}
        "
    }

    read SelectPaths(repo_id: RepositoryId, >list path_hashes: PathHashBytes) -> (PathHashBytes, PathBytes) {
        "SELECT path_hash, path
         FROM paths
         WHERE paths.repo_id = {repo_id}
           AND paths.path_hash in {path_hashes}"
    }
}
