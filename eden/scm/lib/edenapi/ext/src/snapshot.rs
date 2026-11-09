/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::ops::AddAssign;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::format_err;
use edenapi::UploadLookupPolicy;
use edenapi::api::SaplingRemoteApi;
use edenapi_types::AnyFileContentId;
use edenapi_types::AnyId;
use edenapi_types::BonsaiChangesetContent;
use edenapi_types::BonsaiFileChange;
use edenapi_types::BubbleStrategy;
use edenapi_types::BubbleUploadProperties;
use edenapi_types::CacheableSnapshot;
use edenapi_types::ContentId;
use edenapi_types::EphemeralExtendRequest;
use edenapi_types::ExtendBubbleTtlOutcome;
use edenapi_types::FileType;
use edenapi_types::RepoPathBuf;
use edenapi_types::SnapshotRawData;
use edenapi_types::SnapshotRawFiles;
use edenapi_types::UploadSnapshotResponse;
use futures::TryStreamExt;
use minibytes::Bytes;
use tokio::task;

use crate::snapshot_cache::SharedSnapshotFileCache;
use crate::util::calc_contentid;

/// ENXIO errno value (6 on Linux/macOS). Returned when attempting to read
/// non-regular files such as Unix domain sockets or FIFOs.
const ENXIO: i32 = 6;

/// Statistics for blob downloads tracking different sources using atomic counters
/// This provides thread-safe, lock-free counting for concurrent async operations
///
/// Note: Tracks by blobs (content) rather than file paths, since multiple paths
/// can reference the same blob content. This makes it easier to reason about.
#[derive(Debug, Default)]
pub struct DownloadFileStats {
    /// Number of blobs found on disk with correct content
    blobs_from_disk_state: AtomicUsize,
    /// Number of blobs retrieved from local cache
    blobs_from_local_cache: AtomicUsize,
    /// Number of blobs fetched remotely from server
    blobs_fetched_remotely: AtomicUsize,
}

impl DownloadFileStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Total number of blobs processed
    pub fn total_blobs(&self) -> usize {
        self.blobs_from_disk_state.load(Ordering::Relaxed)
            + self.blobs_from_local_cache.load(Ordering::Relaxed)
            + self.blobs_fetched_remotely.load(Ordering::Relaxed)
    }

    /// Number of blobs found on disk
    pub fn blobs_from_disk_state(&self) -> usize {
        self.blobs_from_disk_state.load(Ordering::Relaxed)
    }

    /// Number of blobs from local cache
    pub fn blobs_from_local_cache(&self) -> usize {
        self.blobs_from_local_cache.load(Ordering::Relaxed)
    }

    /// Number of blobs fetched remotely
    pub fn blobs_fetched_remotely(&self) -> usize {
        self.blobs_fetched_remotely.load(Ordering::Relaxed)
    }

    /// Add stats for a blob found on disk
    pub fn add_disk_blob(&self) {
        self.blobs_from_disk_state.fetch_add(1, Ordering::Relaxed);
    }

    /// Add stats for a blob from cache
    pub fn add_cached_blob(&self) {
        self.blobs_from_local_cache.fetch_add(1, Ordering::Relaxed);
    }

    /// Add stats for a blob fetched remotely
    pub fn add_remote_blob(&self) {
        self.blobs_fetched_remotely.fetch_add(1, Ordering::Relaxed);
    }

    /// Get a snapshot of the current stats as regular usize values
    pub fn snapshot(&self) -> DownloadFileStatsSnapshot {
        DownloadFileStatsSnapshot {
            blobs_from_disk_state: self.blobs_from_disk_state(),
            blobs_from_local_cache: self.blobs_from_local_cache(),
            blobs_fetched_remotely: self.blobs_fetched_remotely(),
        }
    }
}

impl fmt::Display for DownloadFileStats {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Downloaded {} blobs (disk: {}, cache: {}, remote: {})",
            self.total_blobs(),
            self.blobs_from_disk_state(),
            self.blobs_from_local_cache(),
            self.blobs_fetched_remotely()
        )
    }
}

/// A snapshot of download blob stats at a point in time
/// This is useful for returning stats from async functions
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DownloadFileStatsSnapshot {
    /// Number of blobs found on disk with correct content
    pub blobs_from_disk_state: usize,
    /// Number of blobs retrieved from local cache
    pub blobs_from_local_cache: usize,
    /// Number of blobs fetched remotely from server
    pub blobs_fetched_remotely: usize,
}

impl DownloadFileStatsSnapshot {
    /// Total number of blobs processed
    pub fn total_blobs(&self) -> usize {
        self.blobs_from_disk_state + self.blobs_from_local_cache + self.blobs_fetched_remotely
    }
}

impl fmt::Display for DownloadFileStatsSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Downloaded {} blobs (disk: {}, cache: {}, remote: {})",
            self.total_blobs(),
            self.blobs_from_disk_state,
            self.blobs_from_local_cache,
            self.blobs_fetched_remotely
        )
    }
}

impl AddAssign for DownloadFileStatsSnapshot {
    fn add_assign(&mut self, rhs: Self) {
        self.blobs_from_disk_state += rhs.blobs_from_disk_state;
        self.blobs_from_local_cache += rhs.blobs_from_local_cache;
        self.blobs_fetched_remotely += rhs.blobs_fetched_remotely;
    }
}

#[derive(PartialEq, Eq)]
enum TrackedType {
    Tracked,
    Untracked,
}
use TrackedType::*;

struct FileMetadata(RepoPathBuf, FileType, ContentId, TrackedType);
#[derive(Clone)]
struct FileData(ContentId, Bytes);

fn load_files(
    root: &RepoPathBuf,
    rel_path: RepoPathBuf,
    file_type: FileType,
    tracked: TrackedType,
) -> Result<(FileMetadata, FileData)> {
    let mut abs_path = root.clone();
    abs_path.push(&rel_path);
    let abs_path = abs_path.as_repo_path().as_str();
    let content = match file_type {
        FileType::Symlink => {
            let link = std::fs::read_link(abs_path)?;
            let to = link
                .to_str()
                .context("symlink is not valid UTF-8")?
                .as_bytes();
            Bytes::copy_from_slice(to)
        }
        FileType::Regular | FileType::Executable => Bytes::from_owner(std::fs::read(abs_path)?),
    };
    let content_id = calc_contentid(&content);
    Ok((
        FileMetadata(rel_path, file_type, content_id, tracked),
        FileData(content_id, content),
    ))
}

/// Context for a prepared ephemeral bubble that's ready for use
struct PreparedEphemeralBubbleContext {
    /// The bubble ID to use for storing data
    bubble_id: std::num::NonZero<u64>,
    /// The timestamp when this bubble will expire, if available
    bubble_expiration_timestamp: Option<i64>,
    /// Optional hint to the backend about which bubble to copy data from for performance optimization
    /// if we can grab a knowledge of the previous bubble ID from the local metadata where we would most likely found the blobs uploaded
    copy_from_bubble_id_hint: Option<std::num::NonZero<u64>>,
}

async fn prepare_ephemeral_bubble(
    api: &(impl SaplingRemoteApi + ?Sized),
    bubble_properties: &BubbleUploadProperties,
) -> Result<PreparedEphemeralBubbleContext> {
    let (bubble_id, bubble_expiration_timestamp, copy_from_bubble_id) =
        match &bubble_properties.strategy {
            BubbleStrategy::Reuse {
                bubble_id: reuse_bubble_id,
                keep_ttl,
                snapshot_id,
            } => {
                // When keep_ttl is true, skip TTL extension for performance
                let expiration = if *keep_ttl {
                    // Skip ephemeral_extend call when keep_ttl is set
                    None
                } else {
                    // Extend the existing bubble's TTL and reuse its storage
                    // Please, see the documentation of ephemeral_extend for more details.
                    // If the requested duration is shorter than the existing bubble's lifetime,
                    // the lifetime will not be changed.
                    // If the requested duration is not provided, the lifetime will be extended to the default lifetime from this moment or
                    // remaining lifetime of the existing bubble, whichever is longer.
                    // Note: Bubbles with labels remain active even past their expiry time and can be extended successfully.
                    // Only bubbles without labels that have expired will cause the request to fail.
                    let extend_response = api
                        .ephemeral_extend(EphemeralExtendRequest {
                            custom_duration_secs: bubble_properties.lifetime_secs,
                            snapshot_id: *snapshot_id,
                        })
                        .await
                        .context("Failed to extend ephemeral bubble lifetime")?;
                    match extend_response.result {
                        ExtendBubbleTtlOutcome::Extended(timestamp) => Some(timestamp),
                        ExtendBubbleTtlOutcome::NotChanged(timestamp) => Some(timestamp),
                    }
                };
                // When reusing, also use as copy_from hint (they're the same bubble)
                (*reuse_bubble_id, expiration, Some(*reuse_bubble_id))
            }
            BubbleStrategy::CreateWithCopyHint {
                bubble_id: copy_from,
            } => {
                // Create a new bubble but hint to backend to copy data from previous bubble for performance
                let prepare_response = api
                    .ephemeral_prepare(
                        bubble_properties.lifetime_secs.map(Duration::from_secs),
                        bubble_properties.labels.clone(),
                    )
                    .await
                    .context("Failed to create ephemeral bubble")?;
                (
                    prepare_response.bubble_id,
                    prepare_response.expiration_timestamp,
                    Some(*copy_from),
                )
            }
            BubbleStrategy::CreateNew => {
                // Create a completely new bubble with no relationship to previous bubbles
                let prepare_response = api
                    .ephemeral_prepare(
                        bubble_properties.lifetime_secs.map(Duration::from_secs),
                        bubble_properties.labels.clone(),
                    )
                    .await
                    .context("Failed to create ephemeral bubble")?;
                (
                    prepare_response.bubble_id,
                    prepare_response.expiration_timestamp,
                    None,
                )
            }
        };

    Ok(PreparedEphemeralBubbleContext {
        bubble_id,
        bubble_expiration_timestamp,
        copy_from_bubble_id_hint: copy_from_bubble_id,
    })
}

pub async fn upload_snapshot(
    api: &(impl SaplingRemoteApi + ?Sized),
    data: SnapshotRawData,
    bubble_properties: BubbleUploadProperties,
) -> Result<UploadSnapshotResponse> {
    upload_snapshot_with_cache(api, data, bubble_properties, None).await
}

/// Upload snapshot with optional local cache support
pub async fn upload_snapshot_with_cache(
    api: &(impl SaplingRemoteApi + ?Sized),
    data: SnapshotRawData,
    bubble_properties: BubbleUploadProperties,
    cache: Option<SharedSnapshotFileCache>,
) -> Result<UploadSnapshotResponse> {
    let SnapshotRawData {
        files,
        author,
        hg_parents,
        time,
        tz,
    } = data;
    let SnapshotRawFiles {
        root,
        modified,
        added,
        removed,
        untracked,
        missing,
    } = files;
    let (need_upload, mut upload_data): (Vec<_>, Vec<_>) = modified
        .into_iter()
        .chain(added)
        .map(|(p, t)| (p, t, Tracked))
        .chain(
            // TODO(yancouto): Don't upload untracked files if they're too big.
            untracked.into_iter().map(|(p, t)| (p, t, Untracked)),
        )
        // rel_path is relative to the repo root
        .map(|(rel_path, file_type, tracked)| -> anyhow::Result<_> {
            load_files(&root, rel_path.clone(), file_type, tracked)
                .with_context(|| anyhow::anyhow!("Failed to load file {rel_path}"))
        })
        // Ignore IO errors for files that can't be read as regular files:
        // - NotFound: transient files that disappeared between status and snapshot
        // - IsADirectory: symlinks resolving to directories, or directory entries
        //   that reached the upload pipeline despite upstream filtering
        // - ENXIO (os error 6): non-regular files like Unix domain sockets (e.g.
        //   .chataccd/socket) or FIFOs that can't be read with std::fs::read
        .filter_map(|res| match res {
            Ok(ok) => Some(Ok(ok)),
            Err(err) => match err.downcast_ref::<std::io::Error>() {
                Some(io_error)
                    if io_error.kind() == std::io::ErrorKind::NotFound
                        || io_error.kind() == std::io::ErrorKind::IsADirectory
                        || io_error.raw_os_error() == Some(ENXIO) =>
                {
                    None
                }
                _ => Some(Err(err)),
            },
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .unzip();

    // Deduplicate upload data
    let mut uniques = BTreeSet::new();
    upload_data.retain(|FileData(content_id, _)| uniques.insert(*content_id));

    // Start caching task concurrently with upload if caching is enabled
    let cache_task = if let Some(cache) = cache.clone() {
        let upload_data_for_cache = upload_data.clone();
        Some(task::spawn(async move {
            for FileData(content_id, data) in upload_data_for_cache {
                if let Err(e) = cache.store_with_content_id(content_id, &data) {
                    tracing::warn!("Failed to cache file content during upload: {}", e);
                }
            }
        }))
    } else {
        None
    };

    let upload_data = upload_data
        .into_iter()
        .map(|FileData(content_id, data)| (AnyFileContentId::ContentId(content_id), data))
        .collect();

    let PreparedEphemeralBubbleContext {
        bubble_id,
        bubble_expiration_timestamp,
        copy_from_bubble_id_hint,
    } = prepare_ephemeral_bubble(api, &bubble_properties).await?;

    let file_content_tokens = {
        let downcast_error = "incorrect upload token, failed to downcast 'token.data.id' to 'AnyId::AnyFileContentId::ContentId' type";
        // upload file contents first, receiving upload tokens
        api.process_files_upload(
            upload_data,
            Some(bubble_id),
            copy_from_bubble_id_hint,
            UploadLookupPolicy::PerformLookup,
        )
        .await?
        .entries
        .try_collect::<Vec<_>>()
        .await?
        .into_iter()
        .map(|token| {
            let content_id = match token.data.id {
                AnyId::AnyFileContentId(AnyFileContentId::ContentId(id)) => id,
                _ => bail!(downcast_error),
            };
            Ok((content_id, token))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?
    };
    let file_changes = need_upload
        .into_iter()
        .map(|FileMetadata(path, file_type, cid, tracked)| {
            let upload_token = file_content_tokens
                .get(&cid)
                .with_context(|| {
                    format_err!("unexpected error: upload token is missing for ContentId({cid})")
                })?
                .clone();
            let change = if tracked == Tracked {
                BonsaiFileChange::Change {
                    file_type,
                    upload_token,
                    copy_info: None, // TODO(yancouto): Add copy info on tracked changes
                }
            } else {
                BonsaiFileChange::UntrackedChange {
                    file_type,
                    upload_token,
                }
            };
            Ok((path, change))
        })
        .chain(
            removed
                .into_iter()
                .map(|path| Ok((path, BonsaiFileChange::Deletion))),
        )
        .chain(
            missing
                .into_iter()
                .map(|path| Ok((path, BonsaiFileChange::UntrackedDeletion))),
        )
        .collect::<anyhow::Result<Vec<_>>>()?;

    let changeset_response = api
        .upload_bonsai_changeset(
            BonsaiChangesetContent {
                hg_parents,
                author: author.clone(),
                time,
                tz,
                extra: vec![],
                file_changes: file_changes.clone(),
                message: "".to_string(),
                is_snapshot: true,
            },
            Some(bubble_id),
        )
        .await
        .context("Failed to create changeset")?;

    // Wait for caching task to complete if it was started
    if let Some(task) = cache_task {
        if let Err(e) = task.await {
            tracing::warn!("Cache task failed: {}", e);
        }
    }

    // Cache the snapshot data if caching is enabled
    if let Some(cache) = cache {
        if let AnyId::BonsaiChangesetId(changeset_id) = changeset_response.token.data.id {
            let cacheable_snapshot = CacheableSnapshot {
                hg_parents,
                file_changes,
                author,
                time,
                tz,
                bubble_id: Some(bubble_id),
                labels: bubble_properties.labels.unwrap_or_default(),
                cached: None, // Don't store the cached flag
            };

            if let Err(e) = cache.store_snapshot(changeset_id, &cacheable_snapshot) {
                tracing::warn!("Failed to cache snapshot data: {}", e);
            } else {
                tracing::debug!(
                    "Successfully cached snapshot data for changeset: {}",
                    changeset_id
                );
            }
        } else {
            tracing::warn!("Unexpected changeset token type, cannot cache snapshot");
        }
    }

    // Calculate snapshot content bytes from token metadata
    let snapshot_content_bytes: u64 = file_content_tokens
        .values()
        .filter_map(|token| {
            token.data.metadata.as_ref().map(|metadata| match metadata {
                edenapi_types::UploadTokenMetadata::FileContentTokenMetadata(m) => m.content_size,
            })
        })
        .sum();

    Ok(UploadSnapshotResponse {
        changeset_token: changeset_response.token,
        bubble_id,
        bubble_expiration_timestamp,
        snapshot_content_bytes,
    })
}

/// Fetch snapshot with optional local cache support
/// This function checks the cache first before making a remote request
pub async fn fetch_snapshot_with_cache(
    api: &(impl SaplingRemoteApi + ?Sized),
    request: edenapi_types::FetchSnapshotRequest,
    cache: Option<SharedSnapshotFileCache>,
) -> Result<edenapi_types::CacheableSnapshot, edenapi::SaplingRemoteApiError> {
    // Check cache first if available
    if let Some(cache) = &cache {
        if let Ok(Some(mut cached_snapshot)) = cache.get_snapshot(&request.cs_id) {
            cached_snapshot.cached = Some(true); // Mark as cached
            return Ok(cached_snapshot);
        }
    }

    // Fetch from remote if not in cache
    tracing::debug!(
        "Fetching snapshot from remote for changeset: {}",
        request.cs_id
    );
    let response = api.fetch_snapshot(request.clone()).await?;

    // Convert to CacheableSnapshot and mark as not cached
    let mut cacheable_snapshot: edenapi_types::CacheableSnapshot = response.into();
    cacheable_snapshot.cached = Some(false);

    // Cache the response if caching is enabled
    if let Some(cache) = cache {
        // Create a version without the cached flag for storage
        let storage_snapshot = edenapi_types::CacheableSnapshot {
            hg_parents: cacheable_snapshot.hg_parents.clone(),
            file_changes: cacheable_snapshot.file_changes.clone(),
            author: cacheable_snapshot.author.clone(),
            time: cacheable_snapshot.time,
            tz: cacheable_snapshot.tz,
            bubble_id: cacheable_snapshot.bubble_id,
            labels: cacheable_snapshot.labels.clone(),
            cached: None, // Don't store the cached flag
        };

        if let Err(e) = cache.store_snapshot(request.cs_id, &storage_snapshot) {
            tracing::warn!("Failed to cache snapshot response: {}", e);
        }
    }
    Ok(cacheable_snapshot)
}
