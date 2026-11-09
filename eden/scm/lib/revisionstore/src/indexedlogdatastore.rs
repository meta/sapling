/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io::Cursor;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use byteorder::BigEndian;
use byteorder::ReadBytesExt;
use byteorder::WriteBytesExt;
use configmodel::Config;
use configmodel::convert::ByteCount;
use edenapi_types::FileEntry;
use edenapi_types::TreeEntry;
use indexedlog::log::IndexOutput;
use lz4_pyframe::compress;
use lz4_pyframe::decompress;
use minibytes::Bytes;
use revisionstore_types::InternalMetadata;
use storemodel::SerializationFormat;
use tracing::warn;
use try_once_lock::OnceLock;
use types::HgId;
use types::Id20;
use types::Key;
use types::RepoPathBuf;
use types::hgid::ReadHgIdExt;

use crate::ToKeys;
use crate::datastore::Delta;
use crate::datastore::HgIdDataStore;
use crate::datastore::HgIdMutableDeltaStore;
use crate::datastore::Metadata;
use crate::datastore::StoreResult;
use crate::indexedlogutil::Store;
use crate::indexedlogutil::StoreOpenOptions;
use crate::indexedlogutil::StoreType;
use crate::localstore::LocalStore;
use crate::missing::MissingInjection;
use crate::sliceext::SliceExt;
use crate::types::StoreKey;

pub struct IndexedLogHgIdDataStoreConfig {
    pub max_log_count: Option<u8>,
    pub max_bytes_per_log: Option<ByteCount>,
    pub max_bytes: Option<ByteCount>,
    pub btrfs_compression: bool,
}

pub struct IndexedLogHgIdDataStore {
    store: Store,
    missing: MissingInjection,
    format: SerializationFormat,
}

#[derive(Clone, Debug)]
pub struct Entry {
    node: Id20,
    metadata: Metadata,
    acl_children_indices: Option<Vec<u32>>,

    content: OnceLock<Bytes>,
    compressed_content: Option<Bytes>,
}

impl std::cmp::PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node
            && self.metadata == other.metadata
            && self.acl_children_indices == other.acl_children_indices
            && match (self.calculate_content(), other.calculate_content()) {
                (Ok(c1), Ok(c2)) if c1 == c2 => true,
                _ => false,
            }
    }
}

impl Entry {
    pub fn new(node: Id20, content: Bytes, metadata: Metadata) -> Self {
        Entry {
            node,
            metadata,
            acl_children_indices: None,
            content: OnceLock::from(content),
            compressed_content: None,
        }
    }

    /// Read an entry from the slice and deserialize it.
    ///
    /// The on-disk format of an entry is the following:
    /// - HgId <20 bytes>
    /// - Path len: 2 unsigned bytes, big-endian
    /// - Path: <Path len> bytes
    /// - Metadata: metadata-list
    /// - Content len: 8 unsigned bytes, big-endian
    /// - Content: <Content len> bytes, lz4 compressed
    ///
    /// The metadata-list is a list of Metadata, encode with:
    /// - Flag: 1 byte,
    /// - Len: 2 unsigned bytes, big-endian
    /// - Value: <Len> bytes, big-endian
    fn from_bytes(bytes: Bytes) -> Result<Self> {
        let data: &[u8] = bytes.as_ref();
        let mut cur = Cursor::new(data);
        let hgid = cur.read_hgid()?;

        // skip reading name
        let name_len = cur.read_u16::<BigEndian>()? as u64;
        cur.set_position(cur.position() + name_len);

        let metadata = InternalMetadata::read(&mut cur)?;

        let body_len = cur.read_u64::<BigEndian>()?;
        let body = data.get_err(cur.position() as usize..(cur.position() + body_len) as usize)?;
        let body = bytes.slice_to_bytes(body);

        let (content, compressed_content) = if metadata.uncompressed {
            (OnceLock::from(body), None)
        } else {
            (OnceLock::new(), Some(body))
        };

        Ok(Entry {
            node: hgid,
            metadata: metadata.api,
            acl_children_indices: metadata.acl_children_indices,
            content,
            compressed_content,
        })
    }

    /// Read an entry from the IndexedLog and deserialize it.
    pub(crate) fn from_log(id: &[u8], log: &Store) -> Result<Option<Self>> {
        let locked_log = log.read()?;
        let mut log_entry = locked_log.lookup(0, id)?;
        let buf = match log_entry.next() {
            None => return Ok(None),
            Some(buf) => buf?,
        };

        let bytes = locked_log.slice_to_bytes(buf);
        drop(locked_log);
        Entry::from_bytes(bytes).map(Some)
    }

    /// Write an entry to the IndexedLog. See [`from_log`] for the detail about the on-disk format.
    pub fn write_to_log(self, log: &Store) -> Result<()> {
        let mut buf = Vec::new();
        self.serialize(&mut buf, log.should_compress())?;
        log.write()?.append(buf)
    }

    fn serialize(&self, buf: &mut dyn Write, should_compress: bool) -> Result<()> {
        buf.write_all(self.node.as_ref())?;

        // write empty name (i.e. zero length)
        buf.write_u16::<BigEndian>(0)?;

        let metadata = InternalMetadata {
            api: self.metadata,
            uncompressed: !should_compress,
            acl_children_indices: self.acl_children_indices.clone(),
        };
        metadata.write(buf)?;

        let body = if let (true, Some(compressed)) = (should_compress, &self.compressed_content) {
            compressed.clone()
        } else {
            let content = self.calculate_content()?;

            if should_compress {
                compress(&content)?.into()
            } else {
                content
            }
        };

        buf.write_u64::<BigEndian>(body.len() as u64)?;
        buf.write_all(&body)?;

        Ok(())
    }

    pub(crate) fn calculate_content(&self) -> Result<Bytes> {
        let content = self.content.get_or_try_init(|| {
            if let Some(compressed) = self.compressed_content.as_ref() {
                let raw = Bytes::from(decompress(compressed)?);
                Ok(raw)
            } else {
                bail!("No content");
            }
        })?;
        Ok(content.clone())
    }

    // Pre-compress content in preparation for insertion into cache.
    fn compress_content(&mut self) -> Result<()> {
        if self.compressed_content.is_some() {
            return Ok(());
        }

        if let Some(content) = self.content.get() {
            self.compressed_content = Some(compress(content)?.into());
            self.content.take();
        }

        Ok(())
    }

    /// Serialized content length, including any Hg copy header, without decoding.
    pub(crate) fn content_size(&self) -> Result<usize> {
        if let Some(content) = self.content.get() {
            Ok(content.len())
        } else if let Some(compressed) = &self.compressed_content {
            Ok(lz4_pyframe::decompress_size(compressed)?)
        } else {
            bail!("No content")
        }
    }

    pub fn content(&self) -> Result<Bytes> {
        self.calculate_content()
    }

    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    pub fn node(&self) -> Id20 {
        self.node
    }

    pub fn acl_children_indices(&self) -> Option<&[u32]> {
        self.acl_children_indices.as_deref()
    }

    pub fn set_acl_children_indices(&mut self, indices: Vec<u32>) {
        self.acl_children_indices = Some(indices);
    }
}

impl IndexedLogHgIdDataStore {
    /// Create or open an `IndexedLogHgIdDataStore`.
    pub fn new(
        config: &dyn Config,
        path: impl AsRef<Path>,
        log_config: &IndexedLogHgIdDataStoreConfig,
        store_type: StoreType,
        format: SerializationFormat,
    ) -> Result<Self> {
        let open_options = IndexedLogHgIdDataStore::open_options(config, log_config);

        let log = match store_type {
            StoreType::Permanent => open_options.permanent(&path),
            StoreType::Rotated => open_options.rotated(&path),
        }?;

        Ok(IndexedLogHgIdDataStore {
            store: log,
            missing: MissingInjection::new_from_env("MISSING_FILES"),
            format,
        })
    }

    fn open_options(
        config: &dyn Config,
        log_config: &IndexedLogHgIdDataStoreConfig,
    ) -> StoreOpenOptions {
        // If you update defaults/logic here, please update the "cache" help topic
        // calculations in help.py.

        // Default configuration: 4 x 2.5GB.
        let mut open_options = StoreOpenOptions::new(config)
            .max_log_count(4)
            .max_bytes_per_log(2500 * 1000 * 1000)
            .auto_sync_threshold(50 * 1024 * 1024)
            .load_specific_config(config, "hgdata")
            .create(true)
            .btrfs_compression(log_config.btrfs_compression)
            .index("node", |_| {
                vec![IndexOutput::Reference(0..HgId::len() as u64)]
            });

        if let Some(max_log_count) = log_config.max_log_count {
            open_options = open_options.max_log_count(max_log_count);
        }
        if let Some(max_bytes_per_log) = log_config.max_bytes_per_log {
            open_options = open_options.max_bytes_per_log(max_bytes_per_log.value());
        } else if let Some(max_bytes) = log_config.max_bytes {
            let log_count: u64 = open_options.max_log_count.unwrap_or(1).max(1).into();
            open_options = open_options.max_bytes_per_log((max_bytes.value() / log_count).max(1));
        }

        open_options
    }

    pub fn repair(
        config: &dyn Config,
        path: PathBuf,
        log_config: &IndexedLogHgIdDataStoreConfig,
        store_type: StoreType,
    ) -> Result<String> {
        match store_type {
            StoreType::Permanent => {
                IndexedLogHgIdDataStore::open_options(config, log_config).repair_permanent(path)
            }
            StoreType::Rotated => {
                IndexedLogHgIdDataStore::open_options(config, log_config).repair_rotated(path)
            }
        }
    }

    /// Attempt to read an Entry from IndexedLog, replacing the stored path with the one from the provided Key
    pub(crate) fn get_entry(&self, node: &Id20) -> Result<Option<Entry>> {
        self.get_raw_entry(node)
    }

    /// Attempt to read an Entry from IndexedLog, without overwriting the Key (return Key path may not match the request Key path)
    pub(crate) fn get_raw_entry(&self, id: &HgId) -> Result<Option<Entry>> {
        Entry::from_log(id.as_ref(), &self.store)
    }

    /// Return whether the store contains the given id.
    pub(crate) fn contains(&self, id: &HgId) -> Result<bool> {
        self.store.read()?.contains(0, id.as_ref())
    }

    /// Directly get the local content. Do not ask remote servers.
    pub(crate) fn get_local_content_direct(&self, id: &HgId) -> Result<Option<Bytes>> {
        let entry = match self.get_raw_entry(id)? {
            None => return Ok(None),
            Some(v) => v,
        };
        if entry.metadata().is_lfs() {
            // Does not handle the LFS complexity here.
            // It seems this is not actually used in modern setup.
            return Ok(None);
        }

        // Git objects will never have copy info stored inside them
        Ok(Some(
            format_util::strip_file_metadata(&entry.calculate_content()?, self.format())?.0,
        ))
    }

    /// Write an entry to the IndexedLog
    pub fn put_entry(&self, entry: Entry) -> Result<()> {
        entry.write_to_log(&self.store)
    }

    pub fn is_dirty(&self) -> bool {
        self.store.is_dirty()
    }

    /// Returns the bytes used on disk, flushing first so recently
    /// written entries still sitting in the in-memory buffer (up to the
    /// store's auto_sync_threshold, e.g. 50MiB for the default blob/tree
    /// cache) are reflected. The byte limit is reported separately by
    /// `max_bytes()`. Errors if the store could not be opened/flushed
    /// or its on-disk usage could not be determined.
    pub fn disk_usage(&self) -> Result<u64> {
        self.flush_log()?;
        self.store.disk_usage()
    }

    pub fn max_bytes(&self) -> Result<u64> {
        self.store.max_bytes()
    }

    /// Flush the underlying IndexedLog
    pub fn flush_log(&self) -> Result<()> {
        self.store.write()?.flush()?;
        Ok(())
    }

    pub(crate) fn format(&self) -> SerializationFormat {
        self.format
    }

    pub fn put_batch(&self, entries: &mut Vec<(HgId, Entry)>) -> Result<()> {
        let compress = self.store.should_compress();
        self.store.append_batch(
            entries,
            |_, entry, buf| entry.serialize(buf, compress),
            // Files and trees are, in general, not remotely fetched when they are already in local
            // caches, so we don't need to do an extra read-before-write before inserting into
            // cache.
            false,
        )
    }

    /// Pre-compress content of entry if compression is enabled.
    pub fn maybe_compress_content(&self, entry: &mut Entry) -> Result<()> {
        if !self.store.should_compress() {
            return Ok(());
        }

        entry.compress_content()
    }
}

// TODO(meyer): Remove these infallible conversions, replace with fallible or inherent in LazyFile.
impl From<TreeEntry> for Entry {
    fn from(v: TreeEntry) -> Self {
        Entry::new(
            v.key().hgid,
            // TODO(meyer): Why does this infallible conversion exist? Push the failure to consumer of TryFrom, at worst
            v.data_unchecked().unwrap(),
            Metadata::default(),
        )
    }
}

impl From<FileEntry> for Entry {
    fn from(v: FileEntry) -> Self {
        Entry::new(
            v.key().hgid,
            v.content()
                .expect("missing content")
                .data_unchecked()
                .clone(),
            v.metadata().expect("missing content").clone(),
        )
    }
}

impl HgIdMutableDeltaStore for IndexedLogHgIdDataStore {
    fn add(&self, delta: &Delta, metadata: &Metadata) -> Result<()> {
        ensure!(delta.base.is_none(), "Deltas aren't supported.");

        let entry = Entry::new(delta.key.hgid, delta.data.clone(), metadata.clone());
        self.put_entry(entry)
    }

    fn flush(&self) -> Result<Option<Vec<PathBuf>>> {
        self.flush_log().map(|_| None)
    }
}

impl LocalStore for IndexedLogHgIdDataStore {
    fn get_missing(&self, keys: &[StoreKey]) -> Result<Vec<StoreKey>> {
        let missing: Vec<StoreKey> = keys
            .iter()
            .filter(|k| match k {
                StoreKey::HgId(k) => {
                    if self.missing.is_missing(&k.path) {
                        warn!("Force missing: {}", k.path);
                        return true;
                    }

                    !self.contains(&k.hgid).unwrap_or(false)
                }
                StoreKey::Content(_, _) => true,
            })
            .cloned()
            .collect();
        Ok(missing)
    }
}

impl HgIdDataStore for IndexedLogHgIdDataStore {
    fn get(&self, key: StoreKey) -> Result<StoreResult<Vec<u8>>> {
        let key = match key {
            StoreKey::HgId(key) => key,
            content => return Ok(StoreResult::NotFound(content)),
        };

        let entry = match self.get_raw_entry(&key.hgid)? {
            None => return Ok(StoreResult::NotFound(StoreKey::HgId(key))),
            Some(entry) => entry,
        };

        let content = entry.content()?;
        Ok(StoreResult::Found(content.as_ref().to_vec()))
    }

    fn sync(&self) -> Result<()> {
        self.flush_log()
    }
}

impl ToKeys for IndexedLogHgIdDataStore {
    fn to_keys(&self) -> Vec<Result<Key>> {
        let log = match self.store.read() {
            Ok(log) => log,
            Err(e) => return vec![Err(e)],
        };
        log.iter()
            .map(|entry| {
                let bytes = log.slice_to_bytes(entry?);
                Entry::from_bytes(bytes)
            })
            .map(|entry| Ok(Key::new(RepoPathBuf::new(), entry?.node)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use fs_err::remove_file;
    use minibytes::Bytes;
    use tempfile::TempDir;
    use types::FetchContext;
    use types::testutil::*;

    use super::*;
    use crate::scmstore::FileAttributes;
    use crate::scmstore::FileStore;
    use crate::testutil::*;

    #[test]
    fn content_size_does_not_decompress_large_blobs() -> Result<()> {
        for size in [0, 17, 2 << 20, (2 << 20) + 1] {
            let mut entry = Entry::new(
                key("a", "1").hgid,
                vec![b'x'; size].into(),
                Metadata::default(),
            );
            assert_eq!(entry.content_size()?, size);
            entry.compress_content()?;
            assert!(entry.content.get().is_none());
            assert_eq!(entry.content_size()?, size);
            assert!(
                entry.content.get().is_none(),
                "size lookup decoded the blob"
            );
        }
        Ok(())
    }

    #[test]
    fn file_store_size_bound_handles_local_drafts_without_tree_aux() -> Result<()> {
        use storemodel::FileStore as _;
        let tempdir = TempDir::new()?;
        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };
        let log = Arc::new(IndexedLogHgIdDataStore::new(
            &BTreeMap::<&str, &str>::new(),
            &tempdir,
            &config,
            StoreType::Permanent,
            SerializationFormat::Hg,
        )?);
        let mut store = FileStore::empty();
        store.indexedlog_local = Some(log.clone());
        store.flush_on_drop = false;
        let store = crate::trait_impls::ArcFileStore(Arc::new(store));
        for (name, id, size) in [
            (".arcconfig", "1", 17),
            ("config.ts", "2", 2 << 20),
            ("large", "3", (2 << 20) + 1),
        ] {
            let key = key(name, id);
            log.put_entry(Entry::new(
                key.hgid,
                vec![b'x'; size].into(),
                Metadata::default(),
            ))?;
            assert_eq!(
                store.get_content_size_bound(
                    FetchContext::sapling_default(),
                    &key.path,
                    key.hgid
                )?,
                Some(size as u64)
            );
        }
        Ok(())
    }

    #[test]
    fn test_serialize_uncompressed_after_precompression() -> Result<()> {
        let content = Bytes::from(&[1, 2, 3, 4][..]);
        let mut entry = Entry::new(key("a", "1").hgid, content.clone(), Metadata::default());
        entry.compress_content()?;

        let mut serialized = Vec::new();
        entry.serialize(&mut serialized, false)?;
        let decoded = Entry::from_bytes(Bytes::from(serialized))?;

        assert_eq!(decoded.content()?, content);
        Ok(())
    }

    #[test]
    fn test_empty() {
        let tempdir = TempDir::new().unwrap();
        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };
        let log = IndexedLogHgIdDataStore::new(
            &BTreeMap::<&str, &str>::new(),
            &tempdir,
            &config,
            StoreType::Rotated,
            SerializationFormat::Hg,
        )
        .unwrap();
        log.flush().unwrap();
    }

    #[test]
    fn test_add() {
        let tempdir = TempDir::new().unwrap();
        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };
        let log = IndexedLogHgIdDataStore::new(
            &BTreeMap::<&str, &str>::new(),
            &tempdir,
            &config,
            StoreType::Rotated,
            SerializationFormat::Hg,
        )
        .unwrap();

        let delta = Delta {
            data: Bytes::from(&[1, 2, 3, 4][..]),
            base: None,
            key: key("a", "1"),
        };
        let metadata = Default::default();

        log.add(&delta, &metadata).unwrap();
        log.flush().unwrap();
    }

    #[test]
    fn test_disk_usage_errors_after_open_cache_loses_latest() -> Result<()> {
        let tempdir = TempDir::new()?;
        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };
        let log = IndexedLogHgIdDataStore::new(
            &BTreeMap::<&str, &str>::new(),
            &tempdir,
            &config,
            StoreType::Rotated,
            SerializationFormat::Hg,
        )?;
        let delta = Delta {
            data: Bytes::from_static(b"data"),
            base: None,
            key: key("a", "1"),
        };
        log.add(&delta, &Metadata::default())?;
        assert!(log.disk_usage()? > 0);

        remove_file(tempdir.path().join("latest"))?;

        assert!(log.disk_usage().is_err());
        Ok(())
    }

    #[test]
    fn test_add_get() {
        let tempdir = TempDir::new().unwrap();
        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };
        let log = IndexedLogHgIdDataStore::new(
            &BTreeMap::<&str, &str>::new(),
            &tempdir,
            &config,
            StoreType::Rotated,
            SerializationFormat::Hg,
        )
        .unwrap();

        let delta = Delta {
            data: Bytes::from(&[1, 2, 3, 4][..]),
            base: None,
            key: key("a", "1"),
        };
        let metadata = Default::default();

        log.add(&delta, &metadata).unwrap();
        log.flush().unwrap();

        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };
        let log = IndexedLogHgIdDataStore::new(
            &BTreeMap::<&str, &str>::new(),
            &tempdir,
            &config,
            StoreType::Rotated,
            SerializationFormat::Hg,
        )
        .unwrap();
        let read_data = log.get(StoreKey::hgid(delta.key)).unwrap();
        assert_eq!(StoreResult::Found(delta.data.as_ref().to_vec()), read_data);
    }

    #[test]
    fn test_lookup_failure() {
        let tempdir = TempDir::new().unwrap();
        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };
        let log = IndexedLogHgIdDataStore::new(
            &BTreeMap::<&str, &str>::new(),
            &tempdir,
            &config,
            StoreType::Rotated,
            SerializationFormat::Hg,
        )
        .unwrap();

        let key = StoreKey::hgid(key("a", "1"));
        assert_eq!(log.get(key.clone()).unwrap(), StoreResult::NotFound(key));
    }

    #[test]
    fn test_add_chain() -> Result<()> {
        let tempdir = TempDir::new()?;
        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };
        let log = IndexedLogHgIdDataStore::new(
            &BTreeMap::<&str, &str>::new(),
            &tempdir,
            &config,
            StoreType::Rotated,
            SerializationFormat::Hg,
        )?;

        let delta = Delta {
            data: Bytes::from(&[1, 2, 3, 4][..]),
            base: Some(key("a", "1")),
            key: key("a", "2"),
        };
        let metadata = Default::default();

        assert!(log.add(&delta, &metadata).is_err());
        Ok(())
    }

    #[test]
    fn test_iter() -> Result<()> {
        let tempdir = TempDir::new()?;
        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };
        let log = IndexedLogHgIdDataStore::new(
            &BTreeMap::<&str, &str>::new(),
            &tempdir,
            &config,
            StoreType::Rotated,
            SerializationFormat::Hg,
        )?;

        let k = key("a", "2");
        let delta = Delta {
            data: Bytes::from(&[1, 2, 3, 4][..]),
            base: None,
            key: k.clone(),
        };
        let metadata = Default::default();

        log.add(&delta, &metadata)?;
        assert!(
            log.to_keys()
                .into_iter()
                .all(|e| e.unwrap() == key("", "2"))
        );
        Ok(())
    }

    #[test]
    fn test_corrupted() -> Result<()> {
        let tempdir = TempDir::new()?;
        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };
        let log = IndexedLogHgIdDataStore::new(
            &BTreeMap::<&str, &str>::new(),
            &tempdir,
            &config,
            StoreType::Rotated,
            SerializationFormat::Hg,
        )?;

        let k = key("a", "2");
        let delta = Delta {
            data: Bytes::from(&[1, 2, 3, 4][..]),
            base: None,
            key: k,
        };
        let metadata = Default::default();

        log.add(&delta, &metadata)?;
        log.flush()?;
        drop(log);

        // Corrupt the log by removing the "log" file.
        let mut rotate_log_path = tempdir.path().to_path_buf();
        rotate_log_path.push("0");
        rotate_log_path.push("log");
        remove_file(rotate_log_path)?;

        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };
        let log = IndexedLogHgIdDataStore::new(
            &BTreeMap::<&str, &str>::new(),
            &tempdir,
            &config,
            StoreType::Rotated,
            SerializationFormat::Hg,
        )?;
        let k = key("a", "3");
        let delta = Delta {
            data: Bytes::from(&[1, 2, 3, 4][..]),
            base: None,
            key: k,
        };
        let metadata = Default::default();
        log.add(&delta, &metadata)?;
        log.flush()?;

        // There should be only one key in the store.
        assert_eq!(log.to_keys().len(), 1);
        Ok(())
    }

    #[test]
    fn test_extstored_use() -> Result<()> {
        let tempdir = TempDir::new().unwrap();
        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };
        let log = IndexedLogHgIdDataStore::new(
            &BTreeMap::<&str, &str>::new(),
            &tempdir,
            &config,
            StoreType::Rotated,
            SerializationFormat::Hg,
        )?;

        let delta = Delta {
            data: Bytes::from(&[1, 2, 3, 4][..]),
            base: None,
            key: key("a", "1"),
        };

        log.add(
            &delta,
            &Metadata {
                size: None,
                flags: Some(Metadata::LFS_FLAG),
            },
        )?;

        let k = StoreKey::hgid(delta.key);
        assert_eq!(
            log.get(k)?,
            StoreResult::Found(delta.data.as_ref().to_vec())
        );

        Ok(())
    }

    #[test]
    fn test_scmstore_read() -> Result<()> {
        let k = key("a", "def6f29d7b61f9cb70b2f14f79cd5c43c38e21b2");
        let d = delta("1234", None, k.clone());
        let meta = Default::default();

        // Setup local indexedlog
        let tmp = TempDir::new()?;
        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };
        let local = Arc::new(IndexedLogHgIdDataStore::new(
            &BTreeMap::<&str, &str>::new(),
            &tmp,
            &config,
            StoreType::Rotated,
            SerializationFormat::Hg,
        )?);

        local.add(&d, &meta).unwrap();
        local.flush().unwrap();

        // Set up local-only FileStore
        let mut store = FileStore::empty();
        store.indexedlog_local = Some(local);

        // Attempt fetch.
        let fetched = store
            .fetch(
                FetchContext::default(),
                std::iter::once(k),
                FileAttributes::CONTENT,
            )
            .single()?
            .expect("key not found");
        assert_eq!(
            fetched.file_content()?.into_bytes().to_vec(),
            d.data.as_ref().to_vec()
        );

        Ok(())
    }

    #[test]
    fn test_scmstore_write_read() -> Result<()> {
        let k = key("a", "def6f29d7b61f9cb70b2f14f79cd5c43c38e21b2");
        let d = delta("1234", None, k.clone());
        let meta = Default::default();

        // Setup local indexedlog
        let tmp = TempDir::new()?;
        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };
        let local = Arc::new(IndexedLogHgIdDataStore::new(
            &BTreeMap::<&str, &str>::new(),
            &tmp,
            &config,
            StoreType::Rotated,
            SerializationFormat::Hg,
        )?);

        // Set up local-only FileStore
        let mut store = FileStore::empty();
        store.indexedlog_local = Some(local);

        // Write a file
        store.write_batch(std::iter::once((k.clone(), d.data.clone(), meta)))?;

        // Attempt fetch.
        let fetched = store
            .fetch(
                FetchContext::default(),
                std::iter::once(k),
                FileAttributes::CONTENT,
            )
            .single()?
            .expect("key not found");
        assert_eq!(
            fetched.file_content()?.into_bytes().to_vec(),
            d.data.as_ref().to_vec()
        );

        Ok(())
    }

    #[test]
    fn test_scmstore_extstore_ignore() -> Result<()> {
        let tempdir = TempDir::new()?;
        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };
        let log = IndexedLogHgIdDataStore::new(
            &BTreeMap::<&str, &str>::new(),
            &tempdir,
            &config,
            StoreType::Rotated,
            SerializationFormat::Hg,
        )?;

        let lfs_key = key("a", "1");
        let nonlfs_key = key("b", "2");
        let content = Bytes::from(&[1, 2, 3, 4][..]);
        let lfs_metadata = Metadata {
            size: None,
            flags: Some(Metadata::LFS_FLAG),
        };
        let nonlfs_metadata = Metadata {
            size: None,
            flags: None,
        };

        let lfs_entry = Entry::new(lfs_key.hgid, content.clone(), lfs_metadata);
        let nonlfs_entry = Entry::new(nonlfs_key.hgid, content.clone(), nonlfs_metadata);

        log.put_entry(lfs_entry)?;
        log.put_entry(nonlfs_entry)?;

        // Set up local-only FileStore
        let mut store = FileStore::empty();
        store.indexedlog_local = Some(Arc::new(log));
        store.lfs_threshold_bytes = Some(123);

        let fetched = store.fetch(
            FetchContext::default(),
            vec![lfs_key.clone(), nonlfs_key.clone()],
            FileAttributes::CONTENT,
        );

        let (mut found, missing, _errors) = fetched.consume();
        assert_eq!(
            found
                .get_mut(&nonlfs_key)
                .expect("key not found")
                .file_content()?
                .into_bytes(),
            content
        );

        assert!(missing.contains_key(&lfs_key));
        Ok(())
    }

    #[test]
    fn test_git_serialization_format() -> Result<()> {
        let tempdir = TempDir::new().unwrap();
        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };
        let log = IndexedLogHgIdDataStore::new(
            &BTreeMap::<&str, &str>::new(),
            &tempdir,
            &config,
            StoreType::Rotated,
            SerializationFormat::Git,
        )
        .unwrap();

        // We construct a blob that looks like it could be a blob with hg copy info in it
        let data = Bytes::from(&b"\x01\n\x01\nthis is a blob"[..]);
        let delta = Delta {
            data: data.clone(),
            base: None,
            key: key("a", "1"),
        };
        let metadata = Default::default();

        assert!(log.add(&delta, &metadata).is_ok());
        log.flush().unwrap();

        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count: None,
            max_bytes_per_log: None,
            max_bytes: None,
            btrfs_compression: false,
        };

        // Using Git Serialization format, we should parse the blob as is (despite it looking like
        // it has copy info in it)
        let git_log = IndexedLogHgIdDataStore::new(
            &BTreeMap::<&str, &str>::new(),
            &tempdir,
            &config,
            StoreType::Rotated,
            SerializationFormat::Git,
        )
        .unwrap();
        let search_result = git_log.get(StoreKey::hgid(delta.key.clone())).unwrap();
        let blob = git_log
            .get_local_content_direct(&key("a", "1").hgid)?
            .unwrap();

        // Both the search result and blob still contain the "copy data"
        assert_eq!(search_result, StoreResult::Found(data.to_vec()));
        assert_eq!(blob, data.to_vec());

        // Using Hg serialization format, get_local_content_direct will ignore the copy data
        let hg_log = IndexedLogHgIdDataStore::new(
            &BTreeMap::<&str, &str>::new(),
            &tempdir,
            &config,
            StoreType::Rotated,
            SerializationFormat::Hg,
        )
        .unwrap();
        let search_result = hg_log.get(StoreKey::hgid(delta.key)).unwrap();

        // The search result returns the raw entry, not the stripped content.
        assert_eq!(search_result, StoreResult::Found(data.to_vec()));
        // The actual blob has the copy data stripped
        let hg_blob = hg_log
            .get_local_content_direct(&key("a", "1").hgid)?
            .unwrap();
        assert_eq!(hg_blob, Bytes::from(&b"this is a blob"[..]));
        Ok(())
    }

    #[test]
    fn test_serialization_compression() -> Result<()> {
        let key = key("a", "1");
        let content = Bytes::from_static(b"hello hello hello hello hello hello hello hello hello");

        let entry = Entry::new(key.hgid, content.clone(), Metadata::default());

        let mut serialized = Vec::new();
        // Enable compression.
        entry.serialize(&mut serialized, true)?;

        // Notice it is indeed compressed.
        assert_eq!(serialized, b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x145\x00\x00\x00ohello \x06\x00\x17Phello");

        let got = Entry::from_bytes(serialized.into())?;

        assert_eq!(got.content()?, content);

        Ok(())
    }

    #[test]
    fn test_serialization_no_compression() -> Result<()> {
        let key = key("a", "1");
        let content = Bytes::from_static(b"hello hello hello hello hello hello hello hello hello");

        let entry = Entry::new(key.hgid, content.clone(), Metadata::default());

        let mut serialized = Vec::new();
        // Disable compression.
        entry.serialize(&mut serialized, false)?;

        // Notice it is indeed not compressed.
        assert_eq!(serialized, b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00\x00\x01u\x00\x00\x00\x00\x00\x00\x005hello hello hello hello hello hello hello hello hello");

        let got = Entry::from_bytes(serialized.into())?;

        assert_eq!(got.content()?, content);

        Ok(())
    }
}
