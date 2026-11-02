/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::ops::Sub;

use anyhow::Context;
use edenfs_error::EdenFsError;
use edenfs_error::Result;
use serde::Deserialize;
use serde::Serialize;

use crate::client::Client;
use crate::client::EdenFsClient;
use crate::counter_names::*;
use crate::methods::EdenThriftMethod;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// EdenFS filesystem counters
/// The exact VFS implementation depends on the platform
pub struct FilesystemTelemetryCounters {
    // The number of successful open filesystem operations.
    pub syscall_open: u64,
    // The number of successful read operations.
    pub syscall_read: u64,
    // The number of successful readdir operations.
    pub syscall_readdir: u64,
    // The number of successful readdirplus operations.
    pub syscall_readdirplus: u64,
    // The number of successful write operations.
    pub syscall_write: u64,
    // The number of successful stat operations.
    pub syscall_statfs: u64,
    // The number of successful getattr operations.
    pub syscall_getattr: u64,
    // The number of successful setattr operations.
    pub syscall_setattr: u64,
    // The number of successful lookup operations.
    pub syscall_lookup: u64,
    // The number of successful access operations.
    pub syscall_access: u64,
    // The number of successful mkdir operations.
    pub syscall_mkdir: u64,
}

impl Sub for FilesystemTelemetryCounters {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            syscall_open: self.syscall_open.saturating_sub(rhs.syscall_open),
            syscall_read: self.syscall_read.saturating_sub(rhs.syscall_read),
            syscall_readdir: self.syscall_readdir.saturating_sub(rhs.syscall_readdir),
            syscall_readdirplus: self
                .syscall_readdirplus
                .saturating_sub(rhs.syscall_readdirplus),
            syscall_write: self.syscall_write.saturating_sub(rhs.syscall_write),
            syscall_statfs: self.syscall_statfs.saturating_sub(rhs.syscall_statfs),
            syscall_getattr: self.syscall_getattr.saturating_sub(rhs.syscall_getattr),
            syscall_setattr: self.syscall_setattr.saturating_sub(rhs.syscall_setattr),
            syscall_lookup: self.syscall_lookup.saturating_sub(rhs.syscall_lookup),
            syscall_access: self.syscall_access.saturating_sub(rhs.syscall_access),
            syscall_mkdir: self.syscall_mkdir.saturating_sub(rhs.syscall_mkdir),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThriftTelemetryCounters {}

impl Sub for ThriftTelemetryCounters {
    type Output = Self;

    fn sub(self, _rhs: Self) -> Self::Output {
        Self {}
    }
}

/// Remote backends
/// EdenAPI backend counters
/// There are no misses as Mononoke is the source of truth for the data
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdenApiBackendTelemetryCounters {
    /// The number of file content fetches from the EdenAPI backend
    pub edenapi_fetches_blobs: u64,
    /// The number of tree fetches from the EdenAPI backend
    pub edenapi_fetches_trees: u64,
    /// Total number of http requests performed to the EdenAPI backend combined for files and trees
    pub edenapi_requests: u64,
}

impl Sub for EdenApiBackendTelemetryCounters {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            edenapi_fetches_blobs: self
                .edenapi_fetches_blobs
                .saturating_sub(rhs.edenapi_fetches_blobs),
            edenapi_fetches_trees: self
                .edenapi_fetches_trees
                .saturating_sub(rhs.edenapi_fetches_trees),
            edenapi_requests: self.edenapi_requests.saturating_sub(rhs.edenapi_requests),
        }
    }
}

/// LFS backend counters
/// There are no misses as Mononoke is the source of truth for the data
/// LFS is not used for fetching trees
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LfsBackendTelemetryCounters {
    /// The number of file content fetches from the LFS backend
    pub lfs_fetches_blobs: u64,
    /// Total number of http requests performed to the LFS backend combined for files and trees
    pub lfs_requests: u64,
}

impl Sub for LfsBackendTelemetryCounters {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            lfs_fetches_blobs: self.lfs_fetches_blobs.saturating_sub(rhs.lfs_fetches_blobs),
            lfs_requests: self.lfs_requests.saturating_sub(rhs.lfs_requests),
        }
    }
}

/// Remote backend fetch counters to track the number of fetches from the remote backends
/// typically with much higher latency than the local caches
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteBackendFetchTelemetryCounters {
    pub edenapi_backend: Option<EdenApiBackendTelemetryCounters>,
    pub lfs_backend: Option<LfsBackendTelemetryCounters>,
}

impl Sub for RemoteBackendFetchTelemetryCounters {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            edenapi_backend: match (self.edenapi_backend, rhs.edenapi_backend) {
                (Some(lhs), Some(rhs)) => Some(lhs - rhs),
                (lhs, None) => lhs,
                (None, _) => None,
            },
            lfs_backend: match (self.lfs_backend, rhs.lfs_backend) {
                (Some(lhs), Some(rhs)) => Some(lhs - rhs),
                (lhs, None) => lhs,
                (None, _) => None,
            },
        }
    }
}

/// Remote backend prefetch counters to track the number of prefetches from the remote backends
/// typically with much higher latency than the local caches
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteBackendPrefetchTelemetryCounters {
    pub edenapi_backend: Option<EdenApiBackendTelemetryCounters>,
    pub lfs_backend: Option<LfsBackendTelemetryCounters>,
}

impl Sub for RemoteBackendPrefetchTelemetryCounters {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            edenapi_backend: match (self.edenapi_backend, rhs.edenapi_backend) {
                (Some(lhs), Some(rhs)) => Some(lhs - rhs),
                (lhs, None) => lhs,
                (None, _) => None,
            },
            lfs_backend: match (self.lfs_backend, rhs.lfs_backend) {
                (Some(lhs), Some(rhs)) => Some(lhs - rhs),
                (lhs, None) => lhs,
                (None, _) => None,
            },
        }
    }
}

/// Combined remote backend stats containing both fetch and prefetch operations
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteBackendTelemetryCounters {
    pub fetch: RemoteBackendFetchTelemetryCounters,
    pub prefetch: RemoteBackendPrefetchTelemetryCounters,
}

impl Sub for RemoteBackendTelemetryCounters {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            fetch: self.fetch - rhs.fetch,
            prefetch: self.prefetch - rhs.prefetch,
        }
    }
}

/// Local caches (sapling "local" cache is skipped as it serves only a few fetches only for commits made locally)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaplingCacheTelemetryCounters {
    // Blobs
    pub blobs_hits: u64,
    pub blobs_misses: u64,
    // Trees
    pub trees_hits: u64,
    pub trees_misses: u64,
}

impl Sub for SaplingCacheTelemetryCounters {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            blobs_hits: self.blobs_hits.saturating_sub(rhs.blobs_hits),
            blobs_misses: self.blobs_misses.saturating_sub(rhs.blobs_misses),
            trees_hits: self.trees_hits.saturating_sub(rhs.trees_hits),
            trees_misses: self.trees_misses.saturating_sub(rhs.trees_misses),
        }
    }
}

/// Sapling LFS Cache counters
/// The cache is only used for storing file content
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaplingLFSCacheTelemetryCounters {
    // Blobs
    pub blobs_hits: u64,
    pub blobs_misses: u64,
}
impl Sub for SaplingLFSCacheTelemetryCounters {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            blobs_hits: self.blobs_hits.saturating_sub(rhs.blobs_hits),
            blobs_misses: self.blobs_misses.saturating_sub(rhs.blobs_misses),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InMemoryCacheTelemetryCounters {
    // Blobs
    pub blobs_hits: u64,
    pub blobs_misses: u64,
    // Trees
    pub trees_hits: u64,
    pub trees_misses: u64,
}

impl Sub for InMemoryCacheTelemetryCounters {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            blobs_hits: self.blobs_hits.saturating_sub(rhs.blobs_hits),
            blobs_misses: self.blobs_misses.saturating_sub(rhs.blobs_misses),
            trees_hits: self.trees_hits.saturating_sub(rhs.trees_hits),
            trees_misses: self.trees_misses.saturating_sub(rhs.trees_misses),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalCacheFetchTelemetryCounters {
    /// Shared Sapling Cache counters (known also as hgcache)
    pub sapling_cache: Option<SaplingCacheTelemetryCounters>,
    /// Shared Sapling LFS Cache counters (known also as hgcache)
    pub sapling_lfs_cache: Option<SaplingLFSCacheTelemetryCounters>,
    /// In memory (eden) cache counters
    pub in_memory_local_cache: Option<InMemoryCacheTelemetryCounters>,
}

impl Sub for LocalCacheFetchTelemetryCounters {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            sapling_cache: match (self.sapling_cache, rhs.sapling_cache) {
                (Some(lhs), Some(rhs)) => Some(lhs - rhs),
                (lhs, None) => lhs,
                (None, _) => None,
            },
            sapling_lfs_cache: match (self.sapling_lfs_cache, rhs.sapling_lfs_cache) {
                (Some(lhs), Some(rhs)) => Some(lhs - rhs),
                (lhs, None) => lhs,
                (None, _) => None,
            },
            in_memory_local_cache: match (self.in_memory_local_cache, rhs.in_memory_local_cache) {
                (Some(lhs), Some(rhs)) => Some(lhs - rhs),
                (lhs, None) => lhs,
                (None, _) => None,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalCachePrefetchTelemetryCounters {
    /// Shared Sapling Cache counters (known also as hgcache)
    pub sapling_cache: Option<SaplingCacheTelemetryCounters>,
    /// Shared Sapling LFS Cache counters (known also as hgcache)
    pub sapling_lfs_cache: Option<SaplingLFSCacheTelemetryCounters>,
}

impl Sub for LocalCachePrefetchTelemetryCounters {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            sapling_cache: match (self.sapling_cache, rhs.sapling_cache) {
                (Some(lhs), Some(rhs)) => Some(lhs - rhs),
                (lhs, None) => lhs,
                (None, _) => None,
            },
            sapling_lfs_cache: match (self.sapling_lfs_cache, rhs.sapling_lfs_cache) {
                (Some(lhs), Some(rhs)) => Some(lhs - rhs),
                (lhs, None) => lhs,
                (None, _) => None,
            },
        }
    }
}

/// Combined local cache stats containing both fetch and prefetch operations
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalCacheTelemetryCounters {
    pub fetch: LocalCacheFetchTelemetryCounters,
    pub prefetch: LocalCachePrefetchTelemetryCounters,
}

impl Sub for LocalCacheTelemetryCounters {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            fetch: self.fetch - rhs.fetch,
            prefetch: self.prefetch - rhs.prefetch,
        }
    }
}

/// EdenFS file metadata fetch telemetry
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMetadataFetchTelemetry {
    // The number of times the file metadata was successfully fetched from the in-memory cache
    pub fetched_from_inmemory_cache: u64,
    // The number of times the file metadata was successfully fetched from the backing store
    // This can come from both local backing store aux cache or remote if not found.
    pub fetched_from_backing_store: u64,
    // The number of times the file metadata was successfully fetched from the backing store but was cached.
    pub fetched_from_backing_store_cached: u64,
    // The number of times the file metadata was computed from the file content present in the backing store cache
    pub fetched_from_backing_store_computed: u64,
    // The number of times the file metadata was not found in any cache and had to be fetched from the remote
    pub fetched_from_remote: u64,
}

impl Sub for FileMetadataFetchTelemetry {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            fetched_from_inmemory_cache: self
                .fetched_from_inmemory_cache
                .saturating_sub(rhs.fetched_from_inmemory_cache),
            fetched_from_backing_store: self
                .fetched_from_backing_store
                .saturating_sub(rhs.fetched_from_backing_store),
            fetched_from_backing_store_cached: self
                .fetched_from_backing_store_cached
                .saturating_sub(rhs.fetched_from_backing_store_cached),
            fetched_from_backing_store_computed: self
                .fetched_from_backing_store_computed
                .saturating_sub(rhs.fetched_from_backing_store_computed),
            fetched_from_remote: self
                .fetched_from_remote
                .saturating_sub(rhs.fetched_from_remote),
        }
    }
}

/// EdenFS file metadata prefetch telemetry
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMetadataPrefetchTelemetry {
    // The number of times the file metadata was successfully fetched from the backing store aux cache during prefetch operations
    pub prefetch_backing_store_cached: u64,
    // The number of times the file metadata was computed from the file content present in the backing store cache during prefetch operations
    pub prefetch_backing_store_computed: u64,
    // The number of times the file metadata was not found in any cache and had to be fetched from the remote during prefetch operations
    pub prefetch_remote: u64,
}

impl Sub for FileMetadataPrefetchTelemetry {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            prefetch_backing_store_cached: self
                .prefetch_backing_store_cached
                .saturating_sub(rhs.prefetch_backing_store_cached),
            prefetch_backing_store_computed: self
                .prefetch_backing_store_computed
                .saturating_sub(rhs.prefetch_backing_store_computed),
            prefetch_remote: self.prefetch_remote.saturating_sub(rhs.prefetch_remote),
        }
    }
}

/// Combined file metadata stats containing both fetch and prefetch operations
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMetadataTelemetry {
    pub fetch: FileMetadataFetchTelemetry,
    pub prefetch: FileMetadataPrefetchTelemetry,
}

impl Sub for FileMetadataTelemetry {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            fetch: self.fetch - rhs.fetch,
            prefetch: self.prefetch - rhs.prefetch,
        }
    }
}

/// EdenFS tree metadata telemetry
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeMetadataTelemetry {
    // The number of times the tree metadata was successfully fetched from the in-memory cache
    pub fetched_from_inmemory_cache: u64,
    // The number of times the tree metadata was successfully fetched from the backing store
    pub fetched_from_backing_store: u64,
}

impl Sub for TreeMetadataTelemetry {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            fetched_from_inmemory_cache: self
                .fetched_from_inmemory_cache
                .saturating_sub(rhs.fetched_from_inmemory_cache),
            fetched_from_backing_store: self
                .fetched_from_backing_store
                .saturating_sub(rhs.fetched_from_backing_store),
        }
    }
}

/// MonorepoInodes stats for fbsource repository
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonorepoInodesTelemetry {
    /// The number of loaded inodes in fbsource repository (populated on fetch)
    pub loaded_inodes: Option<u64>,
    /// The number of unloaded inodes in fbsource repository (populated on fetch)
    pub unloaded_inodes: Option<u64>,
    /// The increase in loaded inodes (populated on sub operation, only if positive)
    pub loaded_inodes_increase: Option<u64>,
    /// The increase in unloaded inodes (populated on sub operation, only if positive)
    pub unloaded_inodes_increase: Option<u64>,
}

impl Sub for MonorepoInodesTelemetry {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        let loaded_increase = match (self.loaded_inodes, rhs.loaded_inodes) {
            (Some(current), Some(previous)) => {
                let diff = current.saturating_sub(previous);
                if diff > 0 { Some(diff) } else { None }
            }
            _ => None,
        };

        let unloaded_increase = match (self.unloaded_inodes, rhs.unloaded_inodes) {
            (Some(current), Some(previous)) => {
                let diff = current.saturating_sub(previous);
                if diff > 0 { Some(diff) } else { None }
            }
            _ => None,
        };

        Self {
            loaded_inodes: None,   // Don't populate absolute values on sub operation
            unloaded_inodes: None, // Don't populate absolute values on sub operation
            loaded_inodes_increase: loaded_increase,
            unloaded_inodes_increase: unloaded_increase,
        }
    }
}

/// EdenFS cumulative counters
/// This is a subset of the counters that are available as part of the EdenFS telemetry
/// Only covers cumulative counters that are incremented on operations during the lifetime of the EdenFS daemon
/// It is possible to snapshot the counters and compare them to a previous snapshot
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryCounters {
    pub fs_stats: FilesystemTelemetryCounters,
    pub thrift_stats: ThriftTelemetryCounters,
    pub backend_stats: RemoteBackendTelemetryCounters,
    pub local_cache_stats: LocalCacheTelemetryCounters,
    pub file_metadata_stats: Option<FileMetadataTelemetry>,
    pub tree_metadata_stats: Option<TreeMetadataTelemetry>,
    pub monorepo_inodes_stats: Option<MonorepoInodesTelemetry>,
}

impl TelemetryCounters {
    /// Returns a CrawlingScore that aggregates the total amount of fetches from remote backends and local caches
    pub fn get_crawling_score(&self) -> CrawlingScore {
        CrawlingScore::from_telemetry(self)
    }

    /// Serialize the TelemetryCounters to a JSON string
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self)
            .map_err(|e| EdenFsError::from(anyhow::anyhow!("Failed to serialize to JSON: {e}")))
    }

    /// Serialize the TelemetryCounters to a pretty-printed JSON string
    pub fn to_json_pretty(&self) -> Result<String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| EdenFsError::from(anyhow::anyhow!("Failed to serialize to JSON: {e}")))
    }

    /// Deserialize a TelemetryCounters from a JSON string
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json)
            .map_err(|e| EdenFsError::from(anyhow::anyhow!("Failed to deserialize from JSON: {e}")))
    }
}

/// CrawlingScore aggregates the total amount of fetches from remote backends and local caches hits, as well as amount of
/// filesystem operations performed during the workload.
/// This provides a summary of how much data was fetched during a workload
/// Please, note that this is approximate as if blob accessed multiple times during the workload
/// it will be counted every time it was accessed. The same applies to trees.
/// For example, read of a large file will be counted many times as the file is read in chunks.
/// Remote fetches and prefetches are now tracked separately to distinguish between critical path operations and background prefetching.
/// Prefetch operations are tracked in separate fields to provide better visibility into background vs on-demand data access patterns.
/// Finally, any data served from filesystem cache will not be accounted for as they do not come to EdenFS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrawlingScore {
    /// Total number of blob fetches from remote backends
    pub remote_blob_fetches: u64,
    /// Total number of tree fetches from remote backends
    pub remote_tree_fetches: u64,
    /// Total number of blob prefetches from remote backends (while eden prefetch, crawling prediction prefetch, etc)
    pub remote_blob_prefetches: u64,
    /// Total number of tree prefetches from remote backends (while eden prefetch, crawling prediction prefetch, etc)
    pub remote_tree_prefetches: u64,
    /// Total number of blob hits from local caches
    pub local_cache_blob_fetch_hits: u64,
    /// Total number of tree hits from local caches
    pub local_cache_tree_fetch_hits: u64,
    /// Total number of blob prefetch hits from local caches (while eden prefetch, crawling prediction prefetch, etc)
    pub local_cache_blob_prefetch_hits: u64,
    /// Total number of tree prefetch hits from local caches (while eden prefetch, crawling prediction prefetch, etc)
    pub local_cache_tree_prefetch_hits: u64,
    /// Total number of filesystem open and read operations
    pub fs_open_plus_read: u64,
    /// Total number of filesystem readdir operations (includes both readdir and readdirplus)
    pub fs_readdir: u64,
}

impl CrawlingScore {
    /// Creates a new CrawlingScore by aggregating data from TelemetryCounters
    pub fn from_telemetry(counters: &TelemetryCounters) -> Self {
        let mut remote_blob_fetches = 0;
        let mut remote_tree_fetches = 0;
        let mut remote_blob_prefetches = 0;
        let mut remote_tree_prefetches = 0;
        let mut local_cache_blob_fetch_hits = 0;
        let mut local_cache_tree_fetch_hits = 0;
        let mut local_cache_blob_prefetch_hits = 0;
        let mut local_cache_tree_prefetch_hits = 0;

        // Aggregate remote backend fetches
        if let Some(edenapi) = &counters.backend_stats.fetch.edenapi_backend {
            remote_blob_fetches += edenapi.edenapi_fetches_blobs;
            remote_tree_fetches += edenapi.edenapi_fetches_trees;
        }
        if let Some(lfs) = &counters.backend_stats.fetch.lfs_backend {
            remote_blob_fetches += lfs.lfs_fetches_blobs;
        }

        // Aggregate remote backend prefetches
        if let Some(edenapi) = &counters.backend_stats.prefetch.edenapi_backend {
            remote_blob_prefetches += edenapi.edenapi_fetches_blobs;
            remote_tree_prefetches += edenapi.edenapi_fetches_trees;
        }
        if let Some(lfs) = &counters.backend_stats.prefetch.lfs_backend {
            remote_blob_prefetches += lfs.lfs_fetches_blobs;
        }

        // Aggregate local cache fetch hits
        if let Some(sapling) = &counters.local_cache_stats.fetch.sapling_cache {
            local_cache_blob_fetch_hits += sapling.blobs_hits;
            local_cache_tree_fetch_hits += sapling.trees_hits;
        }
        if let Some(sapling_lfs) = &counters.local_cache_stats.fetch.sapling_lfs_cache {
            local_cache_blob_fetch_hits += sapling_lfs.blobs_hits;
        }
        if let Some(in_memory) = &counters.local_cache_stats.fetch.in_memory_local_cache {
            local_cache_blob_fetch_hits += in_memory.blobs_hits;
            local_cache_tree_fetch_hits += in_memory.trees_hits;
        }

        // Aggregate local cache prefetch hits
        if let Some(sapling) = &counters.local_cache_stats.prefetch.sapling_cache {
            local_cache_blob_prefetch_hits += sapling.blobs_hits;
            local_cache_tree_prefetch_hits += sapling.trees_hits;
        }
        if let Some(sapling_lfs) = &counters.local_cache_stats.prefetch.sapling_lfs_cache {
            local_cache_blob_prefetch_hits += sapling_lfs.blobs_hits;
        }

        // Get filesystem operations
        let fs_open_plus_read = counters.fs_stats.syscall_open + counters.fs_stats.syscall_read;
        let fs_readdir = counters.fs_stats.syscall_readdir + counters.fs_stats.syscall_readdirplus;

        Self {
            remote_blob_fetches,
            remote_tree_fetches,
            remote_blob_prefetches,
            remote_tree_prefetches,
            local_cache_blob_fetch_hits,
            local_cache_tree_fetch_hits,
            local_cache_blob_prefetch_hits,
            local_cache_tree_prefetch_hits,
            fs_open_plus_read,
            fs_readdir,
        }
    }

    /// Returns the total number of remote fetches (blobs + trees)
    pub fn total_remote_fetches(&self) -> u64 {
        self.remote_blob_fetches + self.remote_tree_fetches
    }

    /// Returns the total number of local cache hits (blobs + trees)
    pub fn total_local_cache_hits(&self) -> u64 {
        self.local_cache_blob_fetch_hits + self.local_cache_tree_fetch_hits
    }

    /// Returns the total number of fetches and hits (remote + local cache)
    pub fn total_access_score(&self) -> u64 {
        self.total_remote_fetches() + self.total_local_cache_hits()
    }

    /// Returns the total number of filesystem operations (open + read + readdir)
    pub fn total_filesystem_ops(&self) -> u64 {
        self.fs_open_plus_read + self.fs_readdir
    }

    /// Serialize the CrawlingScore to a JSON string
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self)
            .map_err(|e| EdenFsError::from(anyhow::anyhow!("Failed to serialize to JSON: {e}")))
    }

    /// Serialize the CrawlingScore to a pretty-printed JSON string
    pub fn to_json_pretty(&self) -> Result<String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| EdenFsError::from(anyhow::anyhow!("Failed to serialize to JSON: {e}")))
    }
}

impl Sub for CrawlingScore {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            remote_blob_fetches: self
                .remote_blob_fetches
                .saturating_sub(rhs.remote_blob_fetches),
            remote_tree_fetches: self
                .remote_tree_fetches
                .saturating_sub(rhs.remote_tree_fetches),
            remote_blob_prefetches: self
                .remote_blob_prefetches
                .saturating_sub(rhs.remote_blob_prefetches),
            remote_tree_prefetches: self
                .remote_tree_prefetches
                .saturating_sub(rhs.remote_tree_prefetches),
            local_cache_blob_fetch_hits: self
                .local_cache_blob_fetch_hits
                .saturating_sub(rhs.local_cache_blob_fetch_hits),
            local_cache_tree_fetch_hits: self
                .local_cache_tree_fetch_hits
                .saturating_sub(rhs.local_cache_tree_fetch_hits),
            local_cache_blob_prefetch_hits: self
                .local_cache_blob_prefetch_hits
                .saturating_sub(rhs.local_cache_blob_prefetch_hits),
            local_cache_tree_prefetch_hits: self
                .local_cache_tree_prefetch_hits
                .saturating_sub(rhs.local_cache_tree_prefetch_hits),
            fs_open_plus_read: self.fs_open_plus_read.saturating_sub(rhs.fs_open_plus_read),
            fs_readdir: self.fs_readdir.saturating_sub(rhs.fs_readdir),
        }
    }
}

impl Sub for TelemetryCounters {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            fs_stats: self.fs_stats - rhs.fs_stats,
            thrift_stats: self.thrift_stats - rhs.thrift_stats,
            backend_stats: self.backend_stats - rhs.backend_stats,
            local_cache_stats: self.local_cache_stats - rhs.local_cache_stats,
            file_metadata_stats: match (self.file_metadata_stats, rhs.file_metadata_stats) {
                (Some(lhs), Some(rhs)) => Some(lhs - rhs),
                (lhs, None) => lhs,
                (None, _) => None,
            },
            tree_metadata_stats: match (self.tree_metadata_stats, rhs.tree_metadata_stats) {
                (Some(lhs), Some(rhs)) => Some(lhs - rhs),
                (lhs, None) => lhs,
                (None, _) => None,
            },
            monorepo_inodes_stats: match (self.monorepo_inodes_stats, rhs.monorepo_inodes_stats) {
                (Some(lhs), Some(rhs)) => Some(lhs - rhs),
                (lhs, None) => lhs,
                (None, _) => None,
            },
        }
    }
}

impl EdenFsClient {
    pub async fn get_regex_counters(&self, arg_regex: &str) -> Result<BTreeMap<String, i64>> {
        self.with_thrift(|thrift| {
            (
                thrift.getRegexCounters(arg_regex),
                EdenThriftMethod::GetRegexCounters,
            )
        })
        .await
        .with_context(|| "failed to get regex counters")
        .map_err(EdenFsError::from)
    }

    pub async fn get_selected_counters(&self, keys: &[String]) -> Result<BTreeMap<String, i64>> {
        self.with_thrift(|thrift| {
            (
                thrift.getSelectedCounters(keys),
                EdenThriftMethod::GetSelectedCounters,
            )
        })
        .await
        .with_context(|| "failed to get selected counters")
        .map_err(EdenFsError::from)
    }

    pub async fn get_counters(&self) -> Result<BTreeMap<String, i64>> {
        self.with_thrift(|thrift| (thrift.getCounters(), EdenThriftMethod::GetCounters))
            .await
            .with_context(|| "failed to get counters")
            .map_err(EdenFsError::from)
    }

    pub async fn get_counter(&self, key: &str) -> Result<i64> {
        self.with_thrift(|thrift| (thrift.getCounter(key), EdenThriftMethod::GetCounter))
            .await
            .with_context(|| format!("failed to get counter for key {key}"))
            .map_err(EdenFsError::from)
    }

    /// Fetch telemetry counters from EdenFS and return them as a TelemetryCounters struct.
    /// This method fetches all the counters needed to fill the TelemetryCounters struct.
    ///
    /// The counters returned are cumulative counters for the lifetime of the EdenFS process.
    /// Please use the `get_telemetry_counter_delta` method using
    /// with a snapshot of the counters collected at the beginning of your workflow using `get_telemetry_counters`
    pub async fn get_telemetry_counters(&self) -> Result<TelemetryCounters> {
        // Define the counter keys we need to fetch, organized by category
        let filesystem_counters = [
            COUNTER_FS_OPEN,
            COUNTER_FS_READ,
            COUNTER_FS_READDIR,
            COUNTER_FS_READDIRPLUS,
            COUNTER_FS_WRITE,
            COUNTER_FS_GETATTR,
            COUNTER_FS_SETATTR,
            COUNTER_FS_STATFS,
            COUNTER_FS_LOOKUP,
            COUNTER_FS_ACCESS,
            COUNTER_FS_MKDIR,
        ];

        let edenapi_backend_counters = [
            COUNTER_EDENAPI_BLOBS_KEYS,
            COUNTER_EDENAPI_TREES_KEYS,
            COUNTER_EDENAPI_BLOBS_REQUESTS,
            COUNTER_EDENAPI_TREES_REQUESTS,
        ];

        let edenapi_prefetch_backend_counters = [
            COUNTER_EDENAPI_PREFETCH_BLOBS_KEYS,
            COUNTER_EDENAPI_PREFETCH_TREES_KEYS,
            COUNTER_EDENAPI_PREFETCH_BLOBS_REQUESTS,
            COUNTER_EDENAPI_PREFETCH_TREES_REQUESTS,
        ];

        let lfs_backend_counters = [COUNTER_LFS_BLOBS_KEYS, COUNTER_LFS_BLOBS_REQUESTS];

        let lfs_prefetch_backend_counters = [
            COUNTER_LFS_PREFETCH_BLOBS_KEYS,
            COUNTER_LFS_PREFETCH_BLOBS_REQUESTS,
        ];

        let indexedlog_cache_counters = [
            COUNTER_INDEXEDLOG_BLOBS_HITS,
            COUNTER_INDEXEDLOG_BLOBS_MISSES,
            COUNTER_INDEXEDLOG_BLOBS_REQUESTS,
            COUNTER_INDEXEDLOG_TREES_HITS,
            COUNTER_INDEXEDLOG_TREES_MISSES,
            COUNTER_INDEXEDLOG_TREES_REQUESTS,
        ];

        let indexedlog_prefetch_cache_counters = [
            COUNTER_INDEXEDLOG_PREFETCH_BLOBS_HITS,
            COUNTER_INDEXEDLOG_PREFETCH_BLOBS_MISSES,
            COUNTER_INDEXEDLOG_PREFETCH_BLOBS_REQUESTS,
            COUNTER_INDEXEDLOG_PREFETCH_TREES_HITS,
            COUNTER_INDEXEDLOG_PREFETCH_TREES_MISSES,
            COUNTER_INDEXEDLOG_PREFETCH_TREES_REQUESTS,
        ];

        let lfs_cache_counters = [
            COUNTER_LFS_CACHE_BLOBS_KEYS,
            COUNTER_LFS_CACHE_BLOBS_MISSES,
            COUNTER_LFS_CACHE_BLOBS_REQUESTS,
        ];

        let lfs_prefetch_cache_counters = [
            COUNTER_LFS_CACHE_PREFETCH_BLOBS_KEYS,
            COUNTER_LFS_CACHE_PREFETCH_BLOBS_MISSES,
            COUNTER_LFS_CACHE_PREFETCH_BLOBS_REQUESTS,
        ];

        let in_memory_cache_counters = [
            COUNTER_IN_MEMORY_BLOBS_HITS,
            COUNTER_IN_MEMORY_BLOBS_MISSES,
            COUNTER_IN_MEMORY_TREES_HITS,
            COUNTER_IN_MEMORY_TREES_MISSES,
        ];

        let file_metadata_counters = [
            COUNTER_METADATA_MEMORY,
            COUNTER_METADATA_BACKING_STORE,
            COUNTER_METADATA_AUX_COMPUTED,
            COUNTER_METADATA_AUX_HITS,
            COUNTER_METADATA_AUX_MISSES,
        ];

        let file_metadata_prefetch_counters = [
            COUNTER_METADATA_AUX_PREFETCH_COMPUTED,
            COUNTER_METADATA_AUX_PREFETCH_HITS,
            COUNTER_METADATA_AUX_PREFETCH_MISSES,
        ];

        let tree_metadata_counters = [
            COUNTER_TREE_METADATA_MEMORY,
            COUNTER_TREE_METADATA_BACKING_STORE,
        ];

        let monorepo_inodes_counters = [
            COUNTER_INODEMAP_FBSOURCE_LOADED,
            COUNTER_INODEMAP_FBSOURCE_UNLOADED,
        ];

        // Combine all counter keys into a single vector
        let mut keys: Vec<String> = Vec::new();
        keys.extend(filesystem_counters.iter().map(|&s| s.to_string()));
        keys.extend(edenapi_backend_counters.iter().map(|&s| s.to_string()));
        keys.extend(
            edenapi_prefetch_backend_counters
                .iter()
                .map(|&s| s.to_string()),
        );
        keys.extend(lfs_backend_counters.iter().map(|&s| s.to_string()));
        keys.extend(lfs_prefetch_backend_counters.iter().map(|&s| s.to_string()));
        keys.extend(indexedlog_cache_counters.iter().map(|&s| s.to_string()));
        keys.extend(
            indexedlog_prefetch_cache_counters
                .iter()
                .map(|&s| s.to_string()),
        );
        keys.extend(lfs_cache_counters.iter().map(|&s| s.to_string()));
        keys.extend(lfs_prefetch_cache_counters.iter().map(|&s| s.to_string()));
        keys.extend(in_memory_cache_counters.iter().map(|&s| s.to_string()));
        keys.extend(file_metadata_counters.iter().map(|&s| s.to_string()));
        keys.extend(
            file_metadata_prefetch_counters
                .iter()
                .map(|&s| s.to_string()),
        );
        keys.extend(tree_metadata_counters.iter().map(|&s| s.to_string()));
        keys.extend(monorepo_inodes_counters.iter().map(|&s| s.to_string()));

        // Fetch the counters
        let counters = self.get_selected_counters(&keys).await?;

        // Create the TelemetryCounters struct
        let telemetry_counters = TelemetryCounters {
            fs_stats: FilesystemTelemetryCounters {
                syscall_open: *counters.get(COUNTER_FS_OPEN).unwrap_or(&0) as u64,
                syscall_read: *counters.get(COUNTER_FS_READ).unwrap_or(&0) as u64,
                syscall_readdir: *counters.get(COUNTER_FS_READDIR).unwrap_or(&0) as u64,
                syscall_readdirplus: *counters.get(COUNTER_FS_READDIRPLUS).unwrap_or(&0) as u64,
                syscall_write: *counters.get(COUNTER_FS_WRITE).unwrap_or(&0) as u64,
                syscall_getattr: *counters.get(COUNTER_FS_GETATTR).unwrap_or(&0) as u64,
                syscall_setattr: *counters.get(COUNTER_FS_SETATTR).unwrap_or(&0) as u64,
                syscall_statfs: *counters.get(COUNTER_FS_STATFS).unwrap_or(&0) as u64,
                syscall_lookup: *counters.get(COUNTER_FS_LOOKUP).unwrap_or(&0) as u64,
                syscall_access: *counters.get(COUNTER_FS_ACCESS).unwrap_or(&0) as u64,
                syscall_mkdir: *counters.get(COUNTER_FS_MKDIR).unwrap_or(&0) as u64,
            },
            thrift_stats: ThriftTelemetryCounters {},
            backend_stats: RemoteBackendTelemetryCounters {
                fetch: RemoteBackendFetchTelemetryCounters {
                    edenapi_backend: Some(EdenApiBackendTelemetryCounters {
                        edenapi_fetches_blobs: *counters
                            .get(COUNTER_EDENAPI_BLOBS_KEYS)
                            .unwrap_or(&0) as u64,
                        edenapi_fetches_trees: *counters
                            .get(COUNTER_EDENAPI_TREES_KEYS)
                            .unwrap_or(&0) as u64,
                        edenapi_requests: (*counters
                            .get(COUNTER_EDENAPI_BLOBS_REQUESTS)
                            .unwrap_or(&0)
                            + *counters.get(COUNTER_EDENAPI_TREES_REQUESTS).unwrap_or(&0))
                            as u64,
                    }),
                    lfs_backend: Some(LfsBackendTelemetryCounters {
                        lfs_fetches_blobs: *counters.get(COUNTER_LFS_BLOBS_KEYS).unwrap_or(&0)
                            as u64,
                        lfs_requests: *counters.get(COUNTER_LFS_BLOBS_REQUESTS).unwrap_or(&0)
                            as u64,
                    }),
                },
                prefetch: RemoteBackendPrefetchTelemetryCounters {
                    edenapi_backend: Some(EdenApiBackendTelemetryCounters {
                        edenapi_fetches_blobs: *counters
                            .get(COUNTER_EDENAPI_PREFETCH_BLOBS_KEYS)
                            .unwrap_or(&0) as u64,
                        edenapi_fetches_trees: *counters
                            .get(COUNTER_EDENAPI_PREFETCH_TREES_KEYS)
                            .unwrap_or(&0) as u64,
                        edenapi_requests: (*counters
                            .get(COUNTER_EDENAPI_PREFETCH_BLOBS_REQUESTS)
                            .unwrap_or(&0)
                            + *counters
                                .get(COUNTER_EDENAPI_PREFETCH_TREES_REQUESTS)
                                .unwrap_or(&0)) as u64,
                    }),
                    lfs_backend: Some(LfsBackendTelemetryCounters {
                        lfs_fetches_blobs: *counters
                            .get(COUNTER_LFS_PREFETCH_BLOBS_KEYS)
                            .unwrap_or(&0) as u64,
                        lfs_requests: *counters
                            .get(COUNTER_LFS_PREFETCH_BLOBS_REQUESTS)
                            .unwrap_or(&0) as u64,
                    }),
                },
            },
            local_cache_stats: LocalCacheTelemetryCounters {
                fetch: LocalCacheFetchTelemetryCounters {
                    sapling_cache: Some(SaplingCacheTelemetryCounters {
                        blobs_hits: *counters.get(COUNTER_INDEXEDLOG_BLOBS_HITS).unwrap_or(&0)
                            as u64,
                        blobs_misses: *counters.get(COUNTER_INDEXEDLOG_BLOBS_MISSES).unwrap_or(&0)
                            as u64,
                        trees_hits: *counters.get(COUNTER_INDEXEDLOG_TREES_HITS).unwrap_or(&0)
                            as u64,
                        trees_misses: *counters.get(COUNTER_INDEXEDLOG_TREES_MISSES).unwrap_or(&0)
                            as u64,
                    }),
                    sapling_lfs_cache: Some(SaplingLFSCacheTelemetryCounters {
                        blobs_hits: (*counters.get(COUNTER_LFS_CACHE_BLOBS_KEYS).unwrap_or(&0)
                            - *counters.get(COUNTER_LFS_CACHE_BLOBS_MISSES).unwrap_or(&0))
                            as u64,
                        blobs_misses: *counters.get(COUNTER_LFS_CACHE_BLOBS_MISSES).unwrap_or(&0)
                            as u64,
                    }),
                    in_memory_local_cache: Some(InMemoryCacheTelemetryCounters {
                        blobs_hits: *counters.get(COUNTER_IN_MEMORY_BLOBS_HITS).unwrap_or(&0)
                            as u64,
                        blobs_misses: *counters.get(COUNTER_IN_MEMORY_BLOBS_MISSES).unwrap_or(&0)
                            as u64,
                        trees_hits: *counters.get(COUNTER_IN_MEMORY_TREES_HITS).unwrap_or(&0)
                            as u64,
                        trees_misses: *counters.get(COUNTER_IN_MEMORY_TREES_MISSES).unwrap_or(&0)
                            as u64,
                    }),
                },
                prefetch: LocalCachePrefetchTelemetryCounters {
                    sapling_cache: Some(SaplingCacheTelemetryCounters {
                        blobs_hits: *counters
                            .get(COUNTER_INDEXEDLOG_PREFETCH_BLOBS_HITS)
                            .unwrap_or(&0) as u64,
                        blobs_misses: *counters
                            .get(COUNTER_INDEXEDLOG_PREFETCH_BLOBS_MISSES)
                            .unwrap_or(&0) as u64,
                        trees_hits: *counters
                            .get(COUNTER_INDEXEDLOG_PREFETCH_TREES_HITS)
                            .unwrap_or(&0) as u64,
                        trees_misses: *counters
                            .get(COUNTER_INDEXEDLOG_PREFETCH_TREES_MISSES)
                            .unwrap_or(&0) as u64,
                    }),
                    sapling_lfs_cache: Some(SaplingLFSCacheTelemetryCounters {
                        blobs_hits: (*counters
                            .get(COUNTER_LFS_CACHE_PREFETCH_BLOBS_KEYS)
                            .unwrap_or(&0)
                            - *counters
                                .get(COUNTER_LFS_CACHE_PREFETCH_BLOBS_MISSES)
                                .unwrap_or(&0)) as u64,
                        blobs_misses: *counters
                            .get(COUNTER_LFS_CACHE_PREFETCH_BLOBS_MISSES)
                            .unwrap_or(&0) as u64,
                    }),
                },
            },
            file_metadata_stats: Some(FileMetadataTelemetry {
                fetch: FileMetadataFetchTelemetry {
                    fetched_from_inmemory_cache: *counters
                        .get(COUNTER_METADATA_MEMORY)
                        .unwrap_or(&0) as u64,
                    fetched_from_backing_store: *counters
                        .get(COUNTER_METADATA_BACKING_STORE)
                        .unwrap_or(&0) as u64,
                    fetched_from_backing_store_cached: *counters
                        .get(COUNTER_METADATA_AUX_HITS)
                        .unwrap_or(&0)
                        as u64,
                    fetched_from_backing_store_computed: *counters
                        .get(COUNTER_METADATA_AUX_COMPUTED)
                        .unwrap_or(&0)
                        as u64,
                    fetched_from_remote: *counters.get(COUNTER_METADATA_AUX_MISSES).unwrap_or(&0)
                        as u64,
                },
                prefetch: FileMetadataPrefetchTelemetry {
                    prefetch_backing_store_cached: *counters
                        .get(COUNTER_METADATA_AUX_PREFETCH_HITS)
                        .unwrap_or(&0) as u64,
                    prefetch_backing_store_computed: *counters
                        .get(COUNTER_METADATA_AUX_PREFETCH_COMPUTED)
                        .unwrap_or(&0) as u64,
                    prefetch_remote: *counters
                        .get(COUNTER_METADATA_AUX_PREFETCH_MISSES)
                        .unwrap_or(&0) as u64,
                },
            }),
            tree_metadata_stats: Some(TreeMetadataTelemetry {
                fetched_from_inmemory_cache: *counters
                    .get(COUNTER_TREE_METADATA_MEMORY)
                    .unwrap_or(&0) as u64,
                fetched_from_backing_store: *counters
                    .get(COUNTER_TREE_METADATA_BACKING_STORE)
                    .unwrap_or(&0) as u64,
            }),
            monorepo_inodes_stats: Some(MonorepoInodesTelemetry {
                loaded_inodes: Some(
                    *counters.get(COUNTER_INODEMAP_FBSOURCE_LOADED).unwrap_or(&0) as u64,
                ),
                unloaded_inodes: Some(
                    *counters
                        .get(COUNTER_INODEMAP_FBSOURCE_UNLOADED)
                        .unwrap_or(&0) as u64,
                ),
                loaded_inodes_increase: None,
                unloaded_inodes_increase: None,
            }),
        };

        Ok(telemetry_counters)
    }

    /// Calculates the difference in EdenFS telemetry counters between the current state and a given initial state.
    ///
    /// This function retrieves the current telemetry counters and subtracts the provided initial counters from them.
    pub async fn get_telemetry_counters_delta(
        &self,
        initial_counters: TelemetryCounters,
    ) -> Result<TelemetryCounters> {
        let current_counters = self.get_telemetry_counters().await?;
        Ok(current_counters - initial_counters)
    }

    /// Calculates the EdenFS crawling score delta between the current state and a given initial state.
    pub async fn get_crawling_score_delta(
        &self,
        initial_counters: TelemetryCounters,
    ) -> Result<CrawlingScore> {
        let counters = self.get_telemetry_counters_delta(initial_counters).await?;
        Ok(counters.get_crawling_score())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::client::mock_client::make_boxed_future_result;
    use crate::client::mock_service::MockEdenFsService;
    use crate::use_case::UseCase;
    use crate::use_case::UseCaseId;
    use crate::utils::get_config_dir;

    fn filesystem_counters(value: u64) -> FilesystemTelemetryCounters {
        FilesystemTelemetryCounters {
            syscall_open: value,
            syscall_read: value,
            syscall_readdir: value,
            syscall_readdirplus: value,
            syscall_write: value,
            syscall_statfs: value,
            syscall_getattr: value,
            syscall_setattr: value,
            syscall_lookup: value,
            syscall_access: value,
            syscall_mkdir: value,
        }
    }

    fn crawling_score(value: u64) -> CrawlingScore {
        CrawlingScore {
            remote_blob_fetches: value,
            remote_tree_fetches: value,
            remote_blob_prefetches: value,
            remote_tree_prefetches: value,
            local_cache_blob_fetch_hits: value,
            local_cache_tree_fetch_hits: value,
            local_cache_blob_prefetch_hits: value,
            local_cache_tree_prefetch_hits: value,
            fs_open_plus_read: value,
            fs_readdir: value,
        }
    }

    #[test]
    fn test_delta_while_the_daemon_keeps_running() {
        assert_eq!(
            filesystem_counters(5) - filesystem_counters(2),
            filesystem_counters(3)
        );
        assert_eq!(crawling_score(5) - crawling_score(2), crawling_score(3));
    }

    #[test]
    fn test_filesystem_delta_across_a_daemon_restart() {
        assert_eq!(
            filesystem_counters(2) - filesystem_counters(5),
            filesystem_counters(0)
        );
    }

    #[test]
    fn test_crawling_score_delta_across_a_daemon_restart() {
        assert_eq!(crawling_score(2) - crawling_score(5), crawling_score(0));
    }

    #[fbinit::test]
    async fn test_telemetry_counters_statfs() -> anyhow::Result<()> {
        let config_dir = get_config_dir(&None, &None)?;
        let use_case = Arc::new(UseCase::new(&config_dir, UseCaseId::EdenFsTests));
        let mut client = EdenFsClient::new(use_case, PathBuf::new());
        let mock_client = &mut *client;
        let mut mock_service = MockEdenFsService::new();

        mock_service
            .expect_getSelectedCounters()
            .returning(|keys: &[String]| {
                let counters: BTreeMap<String, i64> = keys
                    .iter()
                    .map(|key| {
                        let value = if key == COUNTER_FS_STATFS { 29131 } else { 1 };
                        (key.clone(), value)
                    })
                    .collect();
                make_boxed_future_result(Ok(counters))
            });
        mock_client.set_thrift_service(Arc::new(mock_service));

        let counters = client.get_telemetry_counters().await?;
        assert_eq!(counters.fs_stats.syscall_open, 1);
        assert_eq!(counters.fs_stats.syscall_statfs, 29131);
        Ok(())
    }
}
