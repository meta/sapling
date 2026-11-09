/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Implement traits defined by other crates.

use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use blob::Blob;
use edenapi_types::FileAuxData;
use format_util::git_sha1_digest;
use format_util::hg_sha1_digest;
use storemodel::BoxIterator;
use storemodel::ContentFetch;
use storemodel::ContentFetchItems;
use storemodel::InsertOpts;
use storemodel::KeyStore;
use storemodel::Kind;
use storemodel::SerializationFormat;
use types::FetchContext;
use types::HgId;
use types::Id20;
use types::Key;
use types::RepoPath;
use types::hgid::NULL_ID;

use crate::Metadata;
use crate::indexedlogdatastore::Entry;
use crate::scmstore::FileAttributes;
use crate::scmstore::FileStore;
use crate::scmstore::StoreFile;

// Wrapper types to workaround Rust's orphan rule.
#[derive(Clone)]
pub struct ArcFileStore(pub Arc<FileStore>);

fn file_content_fetch_item((key, store_file): (Key, StoreFile)) -> anyhow::Result<ContentFetch> {
    let content = store_file.file_content()?;
    Ok((key, content))
}

impl storemodel::KeyStore for ArcFileStore {
    fn get_content_iter(
        &self,
        fctx: FetchContext,
        keys: Vec<Key>,
    ) -> anyhow::Result<ContentFetchItems> {
        Ok(self
            .0
            .fetch(fctx, keys, FileAttributes::PURE_CONTENT)
            .into_items()
            .try_map_item(file_content_fetch_item))
    }

    fn get_local_content(&self, _path: &RepoPath, hgid: HgId) -> anyhow::Result<Option<Blob>> {
        self.0.get_local_content_direct(&hgid)
    }

    fn flush(&self) -> Result<()> {
        FileStore::flush(&self.0)
    }

    fn sync(&self) -> Result<()> {
        FileStore::sync(&self.0)
    }

    fn statistics(&self) -> Vec<(String, usize)> {
        FileStore::metrics(&self.0)
    }

    /// Decides whether the store uses git or hg format.
    fn format(&self) -> SerializationFormat {
        self.0.format
    }

    fn insert_data(&self, opts: InsertOpts, path: &RepoPath, data: Blob) -> anyhow::Result<HgId> {
        let data_bytes = data.to_bytes();

        let id = sha1_digest(&opts, &data_bytes, self.format());

        // Check if this should be written as LFS based on hg_flags or size threshold
        let is_lfs_flag = (opts.hg_flags & Metadata::LFS_FLAG as u32) != 0;
        let exceeds_threshold = self
            .0
            .lfs_threshold_bytes
            .is_some_and(|threshold| data_bytes.len() as u64 > threshold);
        let is_lfs = (is_lfs_flag && self.0.lfs_threshold_bytes.is_some()) || exceeds_threshold;

        // For non-permanent inserts, prefer the cache store over local.
        // Fall back to local if cache is not available.
        let target_store = if opts.permanent {
            self.0.indexedlog_local.as_deref()
        } else {
            self.0
                .indexedlog_cache
                .as_deref()
                .or(self.0.indexedlog_local.as_deref())
        };

        // Check if data already exists (read_before_write optimization)
        if opts.read_before_write {
            if is_lfs {
                // For LFS, check if the pointer already exists
                if let Some(lfs_client) = &self.0.lfs_client {
                    if let Some(lfs_local) = &lfs_client.local {
                        if lfs_local.contains_pointer(&id)? {
                            return Ok(id);
                        }
                    }
                }
            } else if let Some(l) = target_store {
                // For non-LFS, check indexedlog
                if l.contains(&id)? {
                    return Ok(id);
                }
            }
        }

        let key = Key::new(path.to_owned(), id);

        if is_lfs_flag && self.0.lfs_threshold_bytes.is_some() {
            // Data is already an LFS pointer
            self.0.write_lfsptr(key, data_bytes)?;
        } else if exceeds_threshold {
            // Data exceeds LFS threshold, write as LFS blob
            self.0.write_lfs(key, data_bytes)?;
        } else {
            // Regular non-LFS write to the target store.
            let store =
                target_store.ok_or_else(|| anyhow!("no IndexedLog available for file insert"))?;
            store.put_entry(Entry::new(key.hgid, data_bytes, Default::default()))?;
        }
        Ok(id)
    }

    fn clone_key_store(&self) -> Box<dyn KeyStore> {
        Box::new(self.clone())
    }
}

impl storemodel::FileStore for ArcFileStore {
    fn get_content_size_bound(
        &self,
        fctx: FetchContext,
        path: &RepoPath,
        id: HgId,
    ) -> anyhow::Result<Option<u64>> {
        // Local draft files may have neither remote nor augmented-tree aux data.
        for store in [&self.0.indexedlog_cache, &self.0.indexedlog_local]
            .into_iter()
            .flatten()
        {
            if let Some(entry) = store.get_entry(&id)? {
                // A pointer's length does not bound its LFS payload.
                if !entry.metadata().is_lfs() {
                    return Ok(Some(entry.content_size()? as u64));
                }
            }
        }
        let mut metadata_store = (*self.0).clone();
        metadata_store.compute_aux_data = false;
        metadata_store.flush_on_drop = false;
        let key = Key::new(path.to_owned(), id);
        let mut fetched = metadata_store
            .fetch(fctx, [key], FileAttributes::AUX)
            .into_iter();
        match fetched.next() {
            Some(entry) => Ok(Some(entry?.1.aux_data()?.total_size)),
            None => Ok(None),
        }
    }

    fn get_rename_iter(
        &self,
        keys: Vec<Key>,
    ) -> anyhow::Result<BoxIterator<anyhow::Result<(Key, Key)>>> {
        let fetched = self.0.fetch(
            FetchContext::default(),
            keys,
            FileAttributes::CONTENT_HEADER,
        );
        let iter = fetched
            .into_iter()
            .filter_map(|result| -> Option<anyhow::Result<(Key, Key)>> {
                (move || -> anyhow::Result<Option<(Key, Key)>> {
                    let (key, store_file) = result?;
                    Ok(store_file.copy_info()?.map(|copy_from| (key, copy_from)))
                })()
                .transpose()
            });
        Ok(Box::new(iter))
    }

    fn get_local_aux(&self, _path: &RepoPath, id: HgId) -> anyhow::Result<Option<FileAuxData>> {
        self.0.get_local_aux_direct(&id)
    }

    fn get_aux_iter(
        &self,
        fctx: FetchContext,
        keys: Vec<Key>,
    ) -> anyhow::Result<BoxIterator<anyhow::Result<(Key, FileAuxData)>>> {
        let fetched = self.0.fetch(fctx, keys, FileAttributes::AUX);
        let iter = fetched
            .into_iter()
            .map(|entry| -> anyhow::Result<(Key, FileAuxData)> {
                let (key, store_file) = entry?;
                let aux = store_file.aux_data()?;
                Ok((key, aux))
            });
        Ok(Box::new(iter))
    }

    fn clone_file_store(&self) -> Box<dyn storemodel::FileStore> {
        Box::new(self.clone())
    }

    fn cache_disk_usage(&self) -> anyhow::Result<storemodel::CacheUsage> {
        let cache = match self.0.indexedlog_cache() {
            Some(cache) => cache,
            None => return Ok(storemodel::CacheUsage::NotConfigured),
        };
        let used = cache.disk_usage()?;
        let limit = cache.max_bytes()?;
        Ok(storemodel::CacheUsage::Available {
            used,
            limit: Some(limit),
        })
    }

    fn lfs_cache_disk_usage(&self) -> anyhow::Result<storemodel::CacheUsage> {
        let lfs_client = match &self.0.lfs_client {
            Some(lfs_client) => lfs_client,
            None => return Ok(storemodel::CacheUsage::NotConfigured),
        };
        // Flush pending in-memory writes first - LFS blobs buffer up to
        // `auto_sync_threshold` bytes before syncing, so a just-completed
        // write could otherwise be missing from the on-disk usage below.
        lfs_client.shared.flush()?;
        let (used, limit) = lfs_client.shared.cache_disk_usage()?;
        Ok(storemodel::CacheUsage::Available {
            used,
            limit: Some(limit),
        })
    }
}

pub(crate) fn sha1_digest(opts: &InsertOpts, data: &[u8], format: SerializationFormat) -> Id20 {
    match format {
        SerializationFormat::Hg => {
            let p1 = opts.parents.first().copied().unwrap_or(NULL_ID);
            let p2 = opts.parents.get(1).copied().unwrap_or(NULL_ID);
            hg_sha1_digest(data, &p1, &p2)
        }
        SerializationFormat::Git => {
            let kind = match opts.kind {
                Kind::File => "blob",
                Kind::Tree => "tree",
            };
            git_sha1_digest(data, kind)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use storemodel::FileStore as _;
    use storemodel::KeyStore;
    use tempfile::TempDir;
    use types::RepoPathBuf;

    use super::*;
    use crate::StoreType;
    use crate::ToKeys;
    use crate::indexedlogdatastore::IndexedLogHgIdDataStore;
    use crate::indexedlogdatastore::IndexedLogHgIdDataStoreConfig;
    use crate::lfs::LfsClient;
    use crate::lfs::LfsStore;
    use crate::scmstore::FileStore;
    use crate::scmstore::file::FileStoreMetrics;
    use crate::testutil::make_lfs_config;

    fn make_data_store(dir: &TempDir) -> Arc<IndexedLogHgIdDataStore> {
        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };
        Arc::new(
            IndexedLogHgIdDataStore::new(
                &BTreeMap::<&str, &str>::new(),
                dir,
                &config,
                StoreType::Rotated,
                SerializationFormat::Hg,
            )
            .unwrap(),
        )
    }

    #[test]
    fn test_insert_data_permanent_routing() {
        let local_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();

        let local = make_data_store(&local_dir);
        let cache = make_data_store(&cache_dir);

        let mut file_store = FileStore::empty();
        file_store.indexedlog_local = Some(local.clone());
        file_store.indexedlog_cache = Some(cache.clone());
        let arc_store = ArcFileStore(Arc::new(file_store));

        let path = RepoPathBuf::from_string("foo".to_string()).unwrap();

        // Non-permanent insert goes to cache.
        let opts = InsertOpts {
            permanent: false,
            ..Default::default()
        };
        arc_store
            .insert_data(opts, &path, b"data1"[..].into())
            .unwrap();
        assert_eq!(cache.to_keys().len(), 1);
        assert_eq!(local.to_keys().len(), 0);

        // Permanent (default) insert goes to local.
        let opts = InsertOpts::default();
        arc_store
            .insert_data(opts, &path, b"data2"[..].into())
            .unwrap();
        assert_eq!(cache.to_keys().len(), 1);
        assert_eq!(local.to_keys().len(), 1);
    }

    #[test]
    fn test_cache_disk_usage_not_configured() {
        // No indexedlog cache configured - NotConfigured so callers can
        // distinguish "no cache" from "cache genuinely empty".
        let arc_store = ArcFileStore(Arc::new(FileStore::empty()));
        assert_eq!(
            arc_store.cache_disk_usage().unwrap(),
            storemodel::CacheUsage::NotConfigured
        );
    }

    #[test]
    fn test_lfs_cache_disk_usage_not_configured() {
        // No LFS client configured - NotConfigured, same contract as the
        // non-LFS cache.
        let arc_store = ArcFileStore(Arc::new(FileStore::empty()));
        assert_eq!(
            arc_store.lfs_cache_disk_usage().unwrap(),
            storemodel::CacheUsage::NotConfigured
        );
    }

    #[test]
    fn test_cache_disk_usage_reflects_freshly_written_data() {
        let cache_dir = TempDir::new().unwrap();
        let cache = make_data_store(&cache_dir);
        let mut file_store = FileStore::empty();
        file_store.indexedlog_cache = Some(cache.clone());
        let arc_store = ArcFileStore(Arc::new(file_store));

        // Baseline before any inserts: a freshly created log already has a
        // small nonzero header, so this must be compared against the
        // post-insert reading rather than asserting `used > 0` in isolation
        // (a check that would trivially pass on the header alone and never
        // catch a regression where new writes aren't reflected).
        let (used_before, limit) = match arc_store.cache_disk_usage().unwrap() {
            storemodel::CacheUsage::Available { used, limit } => (used, limit),
            other => panic!("expected Available, got {other:?}"),
        };

        let path = RepoPathBuf::from_string("foo".to_string()).unwrap();
        let opts = InsertOpts {
            permanent: false,
            ..Default::default()
        };
        arc_store
            .insert_data(opts, &path, b"data"[..].into())
            .unwrap();

        // disk_usage() flushes before reading, so the just-inserted entry
        // (still only in the in-memory buffer at this point) must already be
        // reflected here rather than requiring a separate explicit flush.
        let (used_after, limit_after) = match arc_store.cache_disk_usage().unwrap() {
            storemodel::CacheUsage::Available { used, limit } => (used, limit),
            other => panic!("expected Available, got {other:?}"),
        };
        assert!(
            used_after > used_before,
            "disk usage should grow after inserting data: {used_before} -> {used_after}"
        );
        assert_eq!(limit, limit_after);
        assert_eq!(limit, Some(cache.max_bytes().unwrap()));
    }

    #[test]
    fn test_insert_data_read_before_write() {
        let tempdir = TempDir::new().unwrap();
        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };
        let indexedlog = Arc::new(
            IndexedLogHgIdDataStore::new(
                &BTreeMap::<&str, &str>::new(),
                &tempdir,
                &config,
                StoreType::Rotated,
                SerializationFormat::Hg,
            )
            .unwrap(),
        );

        let mut file_store = FileStore::empty();
        file_store.indexedlog_local = Some(indexedlog.clone());
        let arc_file_store = ArcFileStore(Arc::new(file_store));

        let path = RepoPathBuf::from_string("test/file.txt".to_string()).unwrap();
        let data: &'static [u8] = b"test content";

        // First insert without read_before_write
        let opts = InsertOpts::default();
        let id1 = arc_file_store
            .insert_data(opts, &path, data.into())
            .unwrap();

        // Verify the data is in the store
        assert!(indexedlog.contains(&id1).unwrap());
        assert_eq!(indexedlog.to_keys().len(), 1);

        // Second insert with read_before_write=true should return same id without writing again
        let opts = InsertOpts {
            read_before_write: true,
            ..Default::default()
        };
        let id2 = arc_file_store
            .insert_data(opts, &path, data.into())
            .unwrap();

        // Should return the same id
        assert_eq!(id1, id2);

        // Crucially, there should still be only 1 entry (no duplicate write)
        assert_eq!(indexedlog.to_keys().len(), 1);

        // Verify that without read_before_write, we would get a duplicate
        let opts = InsertOpts {
            read_before_write: false,
            ..Default::default()
        };
        let id3 = arc_file_store
            .insert_data(opts, &path, data.into())
            .unwrap();
        assert_eq!(id1, id3);

        // Now there should be 2 entries (duplicate was written)
        assert_eq!(indexedlog.to_keys().len(), 2);
    }

    fn make_file_store_with_lfs(dir: &TempDir, lfs_threshold: Option<u64>) -> Result<FileStore> {
        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };
        let indexedlog_local = Arc::new(IndexedLogHgIdDataStore::new(
            &BTreeMap::<&str, &str>::new(),
            dir.path(),
            &config,
            StoreType::Rotated,
            SerializationFormat::Hg,
        )?);

        let lfs_client = if lfs_threshold.is_some() {
            let server = mockito::Server::new();
            let lfs_config = make_lfs_config(&server, dir, "test");
            let lfs_store = Arc::new(LfsStore::rotated(dir.path(), &lfs_config)?);
            Some(LfsClient::new(
                lfs_store.clone(),
                Some(lfs_store),
                &lfs_config,
            )?)
        } else {
            None
        };

        Ok(FileStore {
            lfs_threshold_bytes: lfs_threshold,
            verify_hash: true,
            edenapi_retries: 0,
            allow_write_lfs_ptrs: true,
            compute_aux_data: false,
            indexedlog_local: Some(indexedlog_local),
            indexedlog_cache: None,
            edenapi: None,
            cas_manager: None,
            lfs_client,
            metrics: FileStoreMetrics::new(),
            activity_logger: None,
            aux_cache: None,
            flush_on_drop: false,
            format: SerializationFormat::Hg,
            progress_bar: progress_model::AggregatingProgressBar::new("", ""),
            lfs_buffer_in_memory: false,
            max_fetch_count: Default::default(),
        })
    }

    #[test]
    fn test_insert_data_nonlfs() -> Result<()> {
        let dir = TempDir::new()?;
        let store = make_file_store_with_lfs(&dir, None)?;
        let arc_store = ArcFileStore(Arc::new(store));

        let path = RepoPathBuf::from_string("test/file.txt".to_string())?;
        let data = Blob::from_static(b"small data");
        let opts = InsertOpts::default();

        let id = arc_store.insert_data(opts, &path, data.clone())?;

        // Verify data was written to indexedlog
        let content = arc_store.get_local_content(&path, id)?;
        assert!(content.is_some());
        assert_eq!(content.unwrap(), data);

        Ok(())
    }

    #[test]
    fn test_insert_data_lfs_threshold() -> Result<()> {
        let dir = TempDir::new()?;
        // Set threshold to 10 bytes
        let store = make_file_store_with_lfs(&dir, Some(10))?;
        let arc_store = ArcFileStore(Arc::new(store));

        let path = RepoPathBuf::from_string("test/large_file.txt".to_string())?;
        // Data larger than threshold (10 bytes)
        let data = Blob::from_static(b"this is a large file that exceeds the threshold");
        let opts = InsertOpts::default();

        let id = arc_store.insert_data(opts, &path, data)?;

        // The id should be computed correctly
        assert!(!id.is_null());

        Ok(())
    }

    #[test]
    fn test_insert_data_below_lfs_threshold() -> Result<()> {
        let dir = TempDir::new()?;
        // Set threshold to 100 bytes
        let store = make_file_store_with_lfs(&dir, Some(100))?;
        let arc_store = ArcFileStore(Arc::new(store));

        let path = RepoPathBuf::from_string("test/small_file.txt".to_string())?;
        // Data smaller than threshold
        let data = Blob::from_static(b"small");
        let opts = InsertOpts::default();

        let id = arc_store.insert_data(opts, &path, data.clone())?;

        // Verify data was written to indexedlog (non-LFS path)
        let content = arc_store.get_local_content(&path, id)?;
        assert!(content.is_some());
        assert_eq!(content.unwrap(), data);

        Ok(())
    }

    #[test]
    fn test_lfs_cache_disk_usage_reflects_freshly_written_data() -> Result<()> {
        let dir = TempDir::new()?;
        // Set threshold to 10 bytes so the write below routes through LFS.
        let store = make_file_store_with_lfs(&dir, Some(10))?;
        let arc_store = ArcFileStore(Arc::new(store));

        let (used_before, limit_before) = match arc_store.lfs_cache_disk_usage()? {
            storemodel::CacheUsage::Available { used, limit } => (used, limit),
            other => panic!("expected Available, got {other:?}"),
        };

        let path = RepoPathBuf::from_string("test/large_file.txt".to_string())?;
        let data = Blob::from_static(b"this is a large file that exceeds the threshold");
        let opts = InsertOpts::default();
        arc_store.insert_data(opts, &path, data)?;

        // lfs_cache_disk_usage() flushes before reading, so the just-inserted
        // LFS blob (still only in the in-memory write buffer at this point)
        // must already be reflected here without a separate explicit flush.
        let (used_after, limit_after) = match arc_store.lfs_cache_disk_usage()? {
            storemodel::CacheUsage::Available { used, limit } => (used, limit),
            other => panic!("expected Available, got {other:?}"),
        };
        assert!(
            used_after > used_before,
            "LFS disk usage should grow after inserting an LFS blob: {used_before} -> {used_after}"
        );
        assert_eq!(limit_before, limit_after);

        Ok(())
    }

    #[test]
    fn test_insert_data_lfs_flag() -> Result<()> {
        let dir = TempDir::new()?;
        let store = make_file_store_with_lfs(&dir, Some(100))?;
        let arc_store = ArcFileStore(Arc::new(store));

        let path = RepoPathBuf::from_string("test/lfs_pointer.txt".to_string())?;
        // Create a valid LFS pointer
        let lfs_pointer = Blob::from_static(
            b"version https://git-lfs.github.com/spec/v1\n\
oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\n\
size 12345\n\
x-is-binary 0\n",
        );

        let opts = InsertOpts {
            hg_flags: Metadata::LFS_FLAG as u32,
            ..Default::default()
        };

        let id = arc_store.insert_data(opts, &path, lfs_pointer)?;

        // The id should be computed correctly
        assert!(!id.is_null());

        Ok(())
    }

    #[test]
    fn test_insert_data_read_before_write_nonlfs() -> Result<()> {
        let dir = TempDir::new()?;
        let store = make_file_store_with_lfs(&dir, None)?;
        let arc_store = ArcFileStore(Arc::new(store));

        let path = RepoPathBuf::from_string("test/file.txt".to_string())?;
        let data = Blob::from_static(b"test data for read_before_write");

        // First insert
        let opts1 = InsertOpts {
            read_before_write: true,
            ..Default::default()
        };
        let id1 = arc_store.insert_data(opts1, &path, data.clone())?;

        // Second insert with same data should return same id without re-writing
        let opts2 = InsertOpts {
            read_before_write: true,
            ..Default::default()
        };
        let id2 = arc_store.insert_data(opts2, &path, data)?;

        assert_eq!(id1, id2);

        Ok(())
    }

    #[test]
    fn test_insert_data_read_before_write_lfs() -> Result<()> {
        let dir = TempDir::new()?;
        // Set threshold to 10 bytes
        let store = make_file_store_with_lfs(&dir, Some(10))?;
        let arc_store = ArcFileStore(Arc::new(store));

        let path = RepoPathBuf::from_string("test/large_file.txt".to_string())?;
        // Data larger than threshold
        let data = Blob::from_static(b"this is a large file that exceeds the threshold");

        // First insert
        let opts1 = InsertOpts {
            read_before_write: true,
            ..Default::default()
        };
        let id1 = arc_store.insert_data(opts1, &path, data.clone())?;

        // Flush to ensure data is written
        arc_store.flush()?;

        // Second insert with same data should return same id without re-writing
        let opts2 = InsertOpts {
            read_before_write: true,
            ..Default::default()
        };
        let id2 = arc_store.insert_data(opts2, &path, data)?;

        assert_eq!(id1, id2);

        Ok(())
    }
}
