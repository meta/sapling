/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Provides the c-bindings for `crate::backingstore`.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Error;
use anyhow::anyhow;
use cxx::SharedPtr;
use cxx::UniquePtr;
use cxx::WeakPtr;
use cxxerror::Result;
use edenapi::types::DirectoryMetadata;
use storemodel::FileAuxData as ScmStoreFileAuxData;
use storemodel::FileType;
use storemodel::TreeEntry;
use storemodel::TreeItemFlag;
use types::FetchContext;
use types::HgId;
use types::Key;
use types::PathComponentBuf;
use types::RepoPath;
use types::fetch_cause::FetchCause;
use types::fetch_mode::FetchMode;

use crate::backingstore::BackingStore;
use crate::backingstore::HgCacheStats;
use crate::facebook;
use crate::ffi_errors::into_backingstore_err;
use crate::walk::WalkDetector;

#[cxx::bridge(namespace = sapling)]
pub(crate) mod ffi {
    // see https://cxx.rs/shared.html#extern-enums
    #[namespace = "facebook::eden"]
    #[repr(u8)]
    pub enum FetchCause {
        // Lowest Priority - unknown origination
        Unknown = 0,
        // Inode garbage collection loading the trees it scans
        InodeGc = 1,
        // The request originated from a Thrift prefetch endpoint
        Prefetch = 2,
        // The request originated from a Thrift glob endpoint
        Glob = 3,
        // The request originated from a Thrift endpoint
        Thrift = 4,
        // Highest Priority - The request originated from FUSE/NFS/PrjFS
        Fs = 5,
    }

    #[namespace = "facebook::eden"]
    unsafe extern "C++" {
        include!("eden/fs/store/ObjectFetchContext.h");

        // The above enum
        type FetchCause;
    }

    #[repr(u8)]
    pub enum FetchMode {
        /// The fetch may hit remote servers.
        AllowRemote,
        /// The fetch is limited to RAM and disk.
        LocalOnly,
        /// The fetch is only hits remote servers.
        RemoteOnly,
    }

    #[namespace = "facebook::eden"]
    #[repr(u8)]
    pub enum TreeEntryType {
        TREE,
        REGULAR_FILE,
        EXECUTABLE_FILE,
        SYMLINK,
    }

    #[namespace = "facebook::eden"]
    unsafe extern "C++" {
        include!("eden/fs/model/TreeEntry.h");

        type TreeEntryType;
    }

    #[derive(Debug)]
    pub struct TreeAuxData {
        digest_size: u64,
        digest_hash: [u8; 32],
    }

    pub struct Request<'a> {
        oid: &'a [u8],
        cause: FetchCause,
        // TODO: mode: FetchMode
        // TODO: cri: ClientRequestInfo
    }

    pub struct FileAuxData {
        total_size: u64,
        content_sha1: [u8; 20],
        content_blake3: [u8; 32],
    }

    /// Successful batch requests served from local storage and remote servers.
    pub struct BatchFetchStats {
        pub local_count: u64,
        pub remote_count: u64,
    }

    #[namespace = "facebook::eden"]
    #[repr(u8)]
    pub enum HgObjectIdFormat {
        WithPath,
        HashOnly,
    }

    #[namespace = "facebook::eden"]
    unsafe extern "C++" {
        include!("eden/fs/config/HgObjectIdFormat.h");

        type HgObjectIdFormat;
    }

    #[derive(Debug)]
    #[repr(u8)]
    pub enum BackingStoreErrorKind {
        Generic,
        Network,
        IO,
        DataCorruption,
        PermissionDenied,
    }

    unsafe extern "C++" {
        include!("eden/scm/lib/backingstore/include/SaplingBackingStoreError.h");

        type BackingStoreErrorKind;
    }

    unsafe extern "C++" {
        type SaplingBackingStoreError;

        fn backingstore_error(
            msg: &str,
            kind: BackingStoreErrorKind,
        ) -> UniquePtr<SaplingBackingStoreError>;

        fn backingstore_error_with_code(
            msg: &str,
            kind: BackingStoreErrorKind,
            code: i32,
        ) -> UniquePtr<SaplingBackingStoreError>;
    }

    unsafe extern "C++" {
        include!("eden/scm/lib/backingstore/include/ffi.h");

        #[namespace = "facebook::eden"]
        type EdenFsEventsLogger;

        #[namespace = "folly"]
        type IOBuf = crate::facebook::IOBuf;

        type GetTreeBatchResolver;
        type GetTreeAuxBatchResolver;
        type GetBlobBatchResolver;
        type GetFileAuxBatchResolver;

        fn sapling_backingstore_log_edenfs_event(
            logger: &EdenFsEventsLogger,
            sample_json: &str,
        ) -> Result<()>;

        unsafe fn sapling_backingstore_get_tree_batch_handler(
            resolve_state: SharedPtr<GetTreeBatchResolver>,
            index: usize,
            error: UniquePtr<SaplingBackingStoreError>,
            builder: UniquePtr<TreeBuilder>,
        );

        unsafe fn sapling_backingstore_get_tree_aux_batch_handler(
            resolve_state: SharedPtr<GetTreeAuxBatchResolver>,
            index: usize,
            error: UniquePtr<SaplingBackingStoreError>,
            tree: SharedPtr<TreeAuxData>,
        );

        unsafe fn sapling_backingstore_get_blob_batch_handler(
            resolve_state: SharedPtr<GetBlobBatchResolver>,
            index: usize,
            error: UniquePtr<SaplingBackingStoreError>,
            blob: UniquePtr<IOBuf>,
        );

        unsafe fn sapling_backingstore_get_file_aux_batch_handler(
            resolve_state: SharedPtr<GetFileAuxBatchResolver>,
            index: usize,
            error: UniquePtr<SaplingBackingStoreError>,
            blob: SharedPtr<FileAuxData>,
        );
    }

    impl WeakPtr<EdenFsEventsLogger> {}

    unsafe extern "C++" {
        type TreeBuilder;

        fn new_builder(
            case_sensitive: bool,
            oid_format: HgObjectIdFormat,
            oid: &[u8],
        ) -> UniquePtr<TreeBuilder>;

        fn add_entry(
            self: Pin<&mut TreeBuilder>,
            name: &str,
            hg_node: &[u8; 20],
            ttype: TreeEntryType,
            is_restricted: bool,
            has_acl: bool,
        ) -> Result<()>;

        fn add_entry_with_aux_data(
            self: Pin<&mut TreeBuilder>,
            name: &str,
            hg_node: &[u8; 20],
            ttype: TreeEntryType,
            size: u64,
            sha1: &[u8; 20],
            blake3: &[u8; 32],
            is_restricted: bool,
            has_acl: bool,
        ) -> Result<()>;

        fn mark_missing(self: Pin<&mut TreeBuilder>);

        fn reserve(self: Pin<&mut TreeBuilder>, capacity: usize) -> Result<()>;

        fn set_aux_data(self: Pin<&mut TreeBuilder>, digest: &[u8; 32], size: u64) -> Result<()>;
    }

    pub struct GetTreeResult {
        error: UniquePtr<SaplingBackingStoreError>,
    }

    pub struct GetTreeAuxResult {
        data: SharedPtr<TreeAuxData>,
        error: UniquePtr<SaplingBackingStoreError>,
    }

    pub struct GetBlobResult {
        data: UniquePtr<IOBuf>,
        error: UniquePtr<SaplingBackingStoreError>,
    }

    pub struct GetFileAuxResult {
        data: SharedPtr<FileAuxData>,
        error: UniquePtr<SaplingBackingStoreError>,
    }

    pub struct CheckPermissionResult {
        has_access: bool,
        error: UniquePtr<SaplingBackingStoreError>,
    }

    pub struct PathAclEntry {
        restriction_root: String,
        repo_region_acl: String,
        permission_request_group: String,
    }

    pub struct PathAclInfo {
        path: String,
        error: String,
        entries: Vec<PathAclEntry>,
    }

    pub struct GetPathAclInfosResult {
        data: Vec<PathAclInfo>,
        error: UniquePtr<SaplingBackingStoreError>,
    }

    /// Disk-usage state of one cache (blob/tree/lfs). Mirrors
    /// `storemodel::CacheUsage`; `used`/`limit` on `HgCacheStats` are only
    /// meaningful when the corresponding state is `Available`. `Unavailable`
    /// is appended last (not inserted before `Available`) so its ordinal
    /// stays aligned with the Thrift `CacheUsageState` enum, which also
    /// appends its `UNAVAILABLE` value at the end.
    #[repr(u8)]
    pub enum CacheUsageState {
        NotConfigured,
        Unsupported,
        Available,
        /// Configured, but its usage could not be measured due to an error.
        /// The failure reason is logged server-side, not carried here.
        Unavailable,
    }

    /// Stats for per-repo hgcache disk usage. Named `HgCacheStats` (not
    /// `CacheStats`) to avoid colliding with EdenFS's own unrelated
    /// in-memory hit/miss-counter `CacheStats` type on the C++ side.
    pub struct HgCacheStats {
        /// Only meaningful when `cache_path_configured` is true.
        pub cache_path: String,
        pub cache_path_configured: bool,
        pub blob_state: CacheUsageState,
        pub blob_bytes_used: u64,
        /// u64::MAX means uncapped.
        pub blob_bytes_limit: u64,
        pub tree_state: CacheUsageState,
        pub tree_bytes_used: u64,
        pub tree_bytes_limit: u64,
        pub lfs_state: CacheUsageState,
        pub lfs_bytes_used: u64,
        pub lfs_bytes_limit: u64,
    }

    pub struct GetCacheStatsResult {
        data: SharedPtr<HgCacheStats>,
        error: UniquePtr<SaplingBackingStoreError>,
    }

    extern "Rust" {
        type BackingStore;

        pub fn sapling_backingstore_new(
            repository: &str,
            walk_mode: &str,
            eden_fs_events_logger: SharedPtr<EdenFsEventsLogger>,
        ) -> Result<Box<BackingStore>>;

        pub fn sapling_backingstore_get_name(store: &BackingStore) -> Result<String>;

        pub fn sapling_backingstore_get_manifest(
            store: &BackingStore,
            node: &[u8],
        ) -> Result<[u8; 20]>;

        pub fn sapling_backingstore_get_tree(
            store: &BackingStore,
            node: &[u8],
            tree_builder: Pin<&mut TreeBuilder>,
            fetch_mode: FetchMode,
        ) -> GetTreeResult;

        pub fn sapling_backingstore_get_tree_batch(
            store: &BackingStore,
            requests: &[Request],
            fetch_mode: FetchMode,
            oid_format: HgObjectIdFormat,
            case_sensitive: bool,
            resolver: SharedPtr<GetTreeBatchResolver>,
        );

        pub fn sapling_backingstore_get_tree_aux(
            store: &BackingStore,
            node: &[u8],
            fetch_mode: FetchMode,
        ) -> GetTreeAuxResult;

        pub fn sapling_backingstore_get_tree_aux_batch(
            store: &BackingStore,
            requests: &[Request],
            fetch_mode: FetchMode,
            resolver: SharedPtr<GetTreeAuxBatchResolver>,
        );

        pub fn sapling_backingstore_get_blob(
            store: &BackingStore,
            node: &[u8],
            fetch_mode: FetchMode,
        ) -> GetBlobResult;

        pub fn sapling_backingstore_get_blob_batch(
            store: &BackingStore,
            requests: &[Request],
            fetch_mode: FetchMode,
            allow_ignore_result: bool,
            resolver: SharedPtr<GetBlobBatchResolver>,
        ) -> BatchFetchStats;

        pub fn sapling_backingstore_get_file_aux(
            store: &BackingStore,
            node: &[u8],
            fetch_mode: FetchMode,
        ) -> GetFileAuxResult;

        pub fn sapling_backingstore_get_file_aux_batch(
            store: &BackingStore,
            requests: &[Request],
            fetch_mode: FetchMode,
            resolver: SharedPtr<GetFileAuxBatchResolver>,
        );

        pub fn sapling_backingstore_flush(store: &BackingStore);

        /// Process-wide switch for the tracing-to-Scuba sink. Mirrors the
        /// daemon's `telemetry:enable-scribe-logging`; call it at startup and
        /// after every config reload.
        pub fn sapling_backingstore_set_scribe_logging_enabled(enabled: bool);

        pub fn sapling_backingstore_check_permission(
            store: &BackingStore,
            manifest_id: &[u8],
        ) -> CheckPermissionResult;

        pub fn sapling_backingstore_check_path_permissions(
            store: &BackingStore,
            hg_cs_id: &str,
            paths: Vec<String>,
        ) -> GetPathAclInfosResult;

        pub fn sapling_dogfooding_host(store: &BackingStore) -> Result<bool>;

        /// Walk detection for one mount of the store. The witness functions take the store the
        /// detector was created from so it follows the store's current configuration.
        type WalkDetector;

        pub fn sapling_backingstore_walk_detector(
            store: &BackingStore,
            mount: &str,
        ) -> Box<WalkDetector>;

        pub fn sapling_walk_detector_witness_file_read(
            store: &BackingStore,
            detector: &WalkDetector,
            path: &[u8],
            local: bool,
            pid: u32,
            dir_size_known: bool,
            dir_num_files: usize,
            dir_num_dirs: usize,
        );

        pub fn sapling_walk_detector_witness_dir_read(
            store: &BackingStore,
            detector: &WalkDetector,
            path: &[u8],
            num_files: usize,
            num_dirs: usize,
            local: bool,
            pid: u32,
        );

        pub fn sapling_walk_detector_witness_dir_size(
            store: &BackingStore,
            detector: &WalkDetector,
            path: &[u8],
            num_files: usize,
            num_dirs: usize,
        );

        pub fn sapling_walk_detector_set_parent_hint(
            store: &BackingStore,
            detector: &WalkDetector,
            parent_id: &str,
        );

        pub fn sapling_backingstore_get_cache_stats(store: &BackingStore) -> GetCacheStatsResult;

        pub fn sapling_flush_counters();
    }
}

/// Borrows EdenServer's logger without delaying its shutdown and queue drain.
struct EdenFsEventsLoggerAdapter {
    logger: Mutex<WeakPtr<ffi::EdenFsEventsLogger>>,
}

// SAFETY: The adapter never exposes its cxx pointers. The mutex serializes weak
// pointer upgrades, C++ calls, and destruction of each temporary SharedPtr. A
// successful upgrade keeps EdenFsEventsLogger alive throughout the call; an
// expired pointer is never dereferenced. EdenFsEventsLogger only forwards const
// logging calls to XplatLoggerCore, whose transform registry and message queue
// are protected by folly::Synchronized, including calls from other C++ owners.
// The pointers have no thread-affine destruction requirements. Dropping this
// adapter requires exclusive ownership and releases only a weak reference.
unsafe impl Send for EdenFsEventsLoggerAdapter {}
unsafe impl Sync for EdenFsEventsLoggerAdapter {}

impl edenfs_telemetry::SampleLogger for EdenFsEventsLoggerAdapter {
    fn log(&self, sample: edenfs_telemetry::EdenSample) -> anyhow::Result<()> {
        let weak_logger = self
            .logger
            .lock()
            .map_err(|_| anyhow!("EdenFsEventsLogger mutex is poisoned"))?;
        let logger = weak_logger.upgrade();
        let Some(logger) = logger.as_ref() else {
            // EdenServer has released its logger during shutdown. Do not keep
            // it alive or route late events back to the legacy logger.
            return Ok(());
        };
        let sample_json = serde_json::to_string(&sample)?;
        ffi::sapling_backingstore_log_edenfs_event(logger, &sample_json)?;
        Ok(())
    }
}

impl From<ffi::FetchMode> for FetchMode {
    fn from(fetch_mode: ffi::FetchMode) -> Self {
        match fetch_mode {
            ffi::FetchMode::AllowRemote => FetchMode::AllowRemote,
            ffi::FetchMode::RemoteOnly => FetchMode::RemoteOnly,
            ffi::FetchMode::LocalOnly => FetchMode::LocalOnly,
            _ => panic!("unknown fetch mode"),
        }
    }
}

impl From<ffi::FetchCause> for FetchCause {
    fn from(fetch_cause: ffi::FetchCause) -> Self {
        match fetch_cause {
            ffi::FetchCause::InodeGc => FetchCause::EdenInodeGc,
            ffi::FetchCause::Unknown => FetchCause::EdenUnknown,
            ffi::FetchCause::Prefetch => FetchCause::EdenPrefetch,
            ffi::FetchCause::Glob => FetchCause::EdenGlob,
            ffi::FetchCause::Thrift => FetchCause::EdenThrift,
            ffi::FetchCause::Fs => FetchCause::EdenFs,
            _ => FetchCause::Unspecified, // should never happen
        }
    }
}

/// Select the most popular cause from a list of causes.
/// If no cause is more than half of the total, return EdenMixed.
/// Bool return value is `true` if all fetch causes are the same.
fn select_cause(fetch_causes_iter: impl Iterator<Item = ffi::FetchCause>) -> (FetchCause, bool) {
    let mut most_popular_cause = None;
    let mut len = 0;
    let mut max_count = 0;
    let mut cause_counts = [0; 6];
    for cause in fetch_causes_iter {
        let cause_index = match cause {
            ffi::FetchCause::Unknown => 0,
            ffi::FetchCause::InodeGc => 1,
            ffi::FetchCause::Prefetch => 2,
            ffi::FetchCause::Glob => 3,
            ffi::FetchCause::Thrift => 4,
            ffi::FetchCause::Fs => 5,
            _ => 0, // should never happen
        };
        len += 1;
        cause_counts[cause_index] += 1;
        if cause_counts[cause_index] > max_count {
            max_count = cause_counts[cause_index];
            most_popular_cause = Some(cause);
        }
    }
    match most_popular_cause {
        Some(cause) => {
            if max_count > len / 2 {
                // If the most popular cause is more than half of the total, return it.
                (cause.into(), max_count == len)
            } else {
                (FetchCause::EdenMixed, false)
            }
        }
        None => (FetchCause::EdenUnknown, false),
    }
}

/// Helper macro to resolve Result<Option<T>>
macro_rules! resolve_result {
    // Resolve into (transformed<T> | replace, UniquePtr<SaplingBackingStoreError>)
    ($res:expr, transform_some: $transform_some:expr, replace_none: $replace_none:expr) => {
        match $res {
            Ok(Some(data)) => ($transform_some(data), UniquePtr::null()),
            Ok(None) => ($replace_none, UniquePtr::null()),
            Err(e) => ($replace_none, into_backingstore_err(e)),
        }
    };

    // Variant that only returns error by handling data on its own
    ($res:expr, on_some: $some_handler:expr, on_none: $none_handler:expr) => {
        match $res {
            Ok(opt) => match opt {
                Some(data) => match $some_handler(data) {
                    Ok(()) => UniquePtr::null(),
                    Err(e) => into_backingstore_err(e),
                },
                None => {
                    $none_handler();
                    UniquePtr::null()
                }
            },
            Err(e) => into_backingstore_err(e),
        }
    };
}

pub fn sapling_backingstore_new(
    repository: &str,
    walk_mode: &str,
    eden_fs_events_logger: SharedPtr<ffi::EdenFsEventsLogger>,
) -> Result<Box<BackingStore>> {
    if eden_fs_events_logger.is_null() {
        super::init::backingstore_global_init();
    } else {
        super::init::backingstore_global_init_with_logger(Arc::new(EdenFsEventsLoggerAdapter {
            logger: Mutex::new(eden_fs_events_logger.downgrade()),
        }));
    }

    let mut extra_sapling_configs = Vec::new();

    // Allow configuring walk mode optionally via eden config.
    if let v @ ("off" | "monitor" | "prefetch") = walk_mode {
        tracing::debug!("setting backingstore.walk-mode={v} via eden config");
        extra_sapling_configs.push(format!("backingstore.walk-mode={v}"));
    }

    let store = BackingStore::new_with_config(repository, &extra_sapling_configs)?;
    Ok(Box::new(store))
}

pub fn sapling_backingstore_walk_detector(store: &BackingStore, mount: &str) -> Box<WalkDetector> {
    Box::new(store.walk_detector(Path::new(mount)))
}

/// The path of a walk report. Paths from sapling object ids were copied from a Sapling `RepoPath`
/// and inode paths passed EdenFS's component checks, so only UTF-8 is left to verify: a checkout
/// without `require-utf8-path` can hold a local directory name that is not, and such a report is
/// dropped.
fn walk_path(path: &[u8]) -> Option<&RepoPath> {
    match std::str::from_utf8(path) {
        // SAFETY: the components were validated by Sapling or EdenFS, as described above.
        Ok(path) => Some(unsafe { RepoPath::from_str_unchecked(path) }),
        Err(err) => {
            tracing::debug!(?err, "ignoring walk report for a non-UTF-8 path");
            None
        }
    }
}

pub fn sapling_walk_detector_witness_file_read(
    store: &BackingStore,
    detector: &WalkDetector,
    path: &[u8],
    local: bool,
    pid: u32,
    dir_size_known: bool,
    dir_num_files: usize,
    dir_num_dirs: usize,
) {
    let Some(path) = walk_path(path) else {
        return;
    };
    detector.witness_file_read(
        store,
        path,
        local,
        pid,
        dir_size_known.then_some((dir_num_files, dir_num_dirs)),
    );
}

pub fn sapling_walk_detector_witness_dir_read(
    store: &BackingStore,
    detector: &WalkDetector,
    path: &[u8],
    num_files: usize,
    num_dirs: usize,
    local: bool,
    pid: u32,
) {
    let Some(path) = walk_path(path) else {
        return;
    };
    detector.witness_dir_read(store, path, local, num_files, num_dirs, pid);
}

pub fn sapling_walk_detector_witness_dir_size(
    store: &BackingStore,
    detector: &WalkDetector,
    path: &[u8],
    num_files: usize,
    num_dirs: usize,
) {
    let Some(path) = walk_path(path) else {
        return;
    };
    detector.witness_dir_size(store, path, num_files, num_dirs);
}

pub fn sapling_walk_detector_set_parent_hint(
    store: &BackingStore,
    detector: &WalkDetector,
    parent_id: &str,
) {
    detector.set_parent_hint(store, parent_id);
}

pub fn sapling_backingstore_set_scribe_logging_enabled(enabled: bool) {
    #[cfg(feature = "scuba")]
    edenfs_telemetry::tracing_logger::set_scribe_logging_enabled(enabled);
    #[cfg(not(feature = "scuba"))]
    let _ = enabled;
}

pub fn sapling_backingstore_get_name(store: &BackingStore) -> Result<String> {
    store.name().map_err(Into::into)
}

pub fn sapling_backingstore_get_manifest(store: &BackingStore, node: &[u8]) -> Result<[u8; 20]> {
    store.get_manifest(node).map_err(Into::into)
}

pub fn sapling_backingstore_get_tree(
    store: &BackingStore,
    node: &[u8],
    tree_builder: Pin<&mut ffi::TreeBuilder>,
    fetch_mode: ffi::FetchMode,
) -> ffi::GetTreeResult {
    // the cause is not propagated for this API
    let res = store.get_tree(
        FetchContext::new_with_mode_and_cause(FetchMode::from(fetch_mode), FetchCause::EdenUnknown),
        node,
    );
    let error = resolve_result!(
        res,
        on_some: |entry| add_tree_to_builder(tree_builder, entry),
        on_none: || tree_builder.mark_missing()
    );
    ffi::GetTreeResult { error }
}

/// Build a set of restricted child directory HgIds from known ACL children.
/// Fail-open: if the initial check or individual entries error, log a warning
/// and treat as unrestricted.
fn build_restricted_set(
    tree: &dyn TreeEntry,
    children_with_acl: Vec<(PathComponentBuf, HgId)>,
) -> HashSet<HgId> {
    let iter = match tree.filter_permission_denied(children_with_acl) {
        Ok(iter) => iter,
        Err(e) => {
            tracing::warn!(
                ?e,
                "error checking restricted children; treating all children as unrestricted (fail-open)"
            );
            return HashSet::new();
        }
    };

    iter.filter_map(|entry| match entry {
        Ok((_, hgid, _)) => Some(hgid),
        Err(e) => {
            tracing::warn!(
                ?e,
                "error checking restricted child; treating child as unrestricted (fail-open)"
            );
            None
        }
    })
    .collect()
}

fn build_acl_sets(tree: &dyn TreeEntry) -> (HashSet<HgId>, HashSet<HgId>) {
    let children_with_acl = match tree.children_with_acls() {
        Ok(children) => children,
        Err(e) => {
            tracing::warn!(
                ?e,
                "error reading child ACL metadata; treating all children as unrestricted (fail-open)"
            );
            return (HashSet::new(), HashSet::new());
        }
    };
    let child_has_acl = children_with_acl.iter().map(|(_, hgid)| *hgid).collect();
    let restricted_set = build_restricted_set(tree, children_with_acl);
    (child_has_acl, restricted_set)
}

// Convert the `TreeEntry` trait object into an EdenFS Tree by adding each entry to the TreeBuilder
// object.
fn add_tree_to_builder(
    mut builder: Pin<&mut ffi::TreeBuilder>,
    tree: Arc<dyn TreeEntry>,
) -> anyhow::Result<()> {
    let aux_map: HashMap<HgId, ScmStoreFileAuxData> =
        tree.file_aux_iter()?.collect::<anyhow::Result<_>>()?;

    let (child_has_acl, restricted_set) = build_acl_sets(tree.as_ref());

    // Pre-allocate the per-entry storage.
    if let Some(hint) = tree.size_hint() {
        builder.as_mut().reserve(hint)?;
    }

    for entry in tree.iter()? {
        let (name, node, flag) = entry?;

        let ttype = match flag {
            TreeItemFlag::Directory => ffi::TreeEntryType::TREE,
            TreeItemFlag::File(FileType::Regular) => ffi::TreeEntryType::REGULAR_FILE,
            TreeItemFlag::File(FileType::Executable) => ffi::TreeEntryType::EXECUTABLE_FILE,
            TreeItemFlag::File(FileType::Symlink) => ffi::TreeEntryType::SYMLINK,
            TreeItemFlag::File(FileType::GitSubmodule) => {
                // Submodules shouldn't show up, and we don't have a way to handle them, so just skip.
                tracing::warn!(%name, %node, "skipping git submodule tree entry");
                continue;
            }
        };

        let is_restricted =
            matches!(flag, TreeItemFlag::Directory) && restricted_set.contains(&node);
        let has_acl = matches!(flag, TreeItemFlag::Directory) && child_has_acl.contains(&node);

        if let Some(aux) = aux_map.get(&node) {
            builder.as_mut().add_entry_with_aux_data(
                name.as_str(),
                node.as_byte_array(),
                ttype,
                aux.total_size,
                aux.sha1.as_byte_array(),
                aux.blake3.as_byte_array(),
                is_restricted,
                has_acl,
            )?;
        } else {
            builder.as_mut().add_entry(
                name.as_str(),
                node.as_byte_array(),
                ttype,
                is_restricted,
                has_acl,
            )?;
        }
    }

    if let Some(aux) = tree.aux_data()? {
        builder.as_mut().set_aux_data(
            aux.augmented_manifest_id.as_byte_array(),
            aux.augmented_manifest_size,
        )?;
    }

    Ok(())
}

pub fn sapling_backingstore_get_tree_batch(
    store: &BackingStore,
    requests: &[ffi::Request],
    fetch_mode: ffi::FetchMode,
    oid_format: ffi::HgObjectIdFormat,
    case_sensitive: bool,
    resolver: SharedPtr<ffi::GetTreeBatchResolver>,
) {
    let keys: Vec<Key> = requests.iter().map(|req| req.key()).collect();
    let cause = select_cause(requests.iter().map(|req| req.cause)).0;
    let fetch_mode = FetchMode::from(fetch_mode);

    store.get_tree_batch(
        FetchContext::new_with_mode_and_cause(fetch_mode, cause),
        keys,
        |idx, result| {
            let req = &requests[idx];

            let mut builder = ffi::new_builder(case_sensitive, oid_format, req.oid);

            let error = match result {
                Ok(Some(sl_tree)) => {
                    if let Err(err) = add_tree_to_builder(builder.pin_mut(), sl_tree) {
                        into_backingstore_err(err)
                    } else {
                        UniquePtr::null()
                    }
                }
                Ok(None) => {
                    builder.pin_mut().mark_missing();
                    UniquePtr::null()
                }
                Err(err) => into_backingstore_err(err),
            };

            let resolver = resolver.clone();

            unsafe {
                ffi::sapling_backingstore_get_tree_batch_handler(resolver, idx, error, builder)
            };
        },
    );
}

pub fn sapling_backingstore_get_tree_aux(
    store: &BackingStore,
    node: &[u8],
    fetch_mode: ffi::FetchMode,
) -> ffi::GetTreeAuxResult {
    // the cause is not propagated for this API
    let res = store.get_tree_aux(
        FetchContext::new_with_mode_and_cause(FetchMode::from(fetch_mode), FetchCause::EdenUnknown),
        node,
    );
    let (data, error) = resolve_result!(res, transform_some: |aux: DirectoryMetadata| SharedPtr::new(aux.into()), replace_none: SharedPtr::null());
    ffi::GetTreeAuxResult { data, error }
}

pub fn sapling_backingstore_get_tree_aux_batch(
    store: &BackingStore,
    requests: &[ffi::Request],
    fetch_mode: ffi::FetchMode,
    resolver: SharedPtr<ffi::GetTreeAuxBatchResolver>,
) {
    let keys: Vec<Key> = requests.iter().map(|req| req.key()).collect();
    let cause = select_cause(requests.iter().map(|req| req.cause)).0;

    store.get_tree_aux_batch(
        FetchContext::new_with_mode_and_cause(FetchMode::from(fetch_mode), cause),
        keys,
        |idx, result| {
            let result = result.and_then(|opt| opt.ok_or_else(|| Error::msg("no aux data found")));
            let resolver = resolver.clone();
            let (error, aux) = match result {
                Ok(aux) => (UniquePtr::null(), SharedPtr::new(aux.into())),
                Err(error) => (into_backingstore_err(error), SharedPtr::null()),
            };
            unsafe {
                ffi::sapling_backingstore_get_tree_aux_batch_handler(resolver, idx, error, aux)
            };
        },
    );
}

pub fn sapling_backingstore_get_blob(
    store: &BackingStore,
    node: &[u8],
    fetch_mode: ffi::FetchMode,
) -> ffi::GetBlobResult {
    // the cause is not propagated for this API
    let res = store.get_blob(
        FetchContext::new_with_mode_and_cause(FetchMode::from(fetch_mode), FetchCause::EdenUnknown),
        node,
    );
    let (data, error) = resolve_result!(res, transform_some: facebook::blob_into_iobuf, replace_none: UniquePtr::null());
    ffi::GetBlobResult { data, error }
}

pub fn sapling_backingstore_get_blob_batch(
    store: &BackingStore,
    requests: &[ffi::Request],
    fetch_mode: ffi::FetchMode,
    allow_ignore_result: bool,
    resolver: SharedPtr<ffi::GetBlobBatchResolver>,
) -> ffi::BatchFetchStats {
    let keys: Vec<Key> = requests.iter().map(|req| req.key()).collect();
    let (cause, all_match) = select_cause(requests.iter().map(|req| req.cause));

    // EdenPrefetch means eden doesn't care about the content - we can set the IGNORE_RESULT flag to
    // make ScmStore optimize things.
    let mut fetch_mode = FetchMode::from(fetch_mode);
    if cause == FetchCause::EdenPrefetch && all_match && allow_ignore_result {
        fetch_mode |= FetchMode::IGNORE_RESULT;
    }

    // Keep a handle so we can read stats after the batch completes.
    let fctx = FetchContext::new_with_mode_and_cause(fetch_mode, cause);

    store.get_blob_batch(fctx.clone(), keys, |idx, result| {
        let resolver = resolver.clone();

        let (error, blob) = match result {
            Ok(blob) => match blob {
                None => {
                    if fetch_mode.ignore_result() {
                        (UniquePtr::null(), UniquePtr::null())
                    } else {
                        (
                            into_backingstore_err(anyhow!("no blob found")),
                            UniquePtr::null(),
                        )
                    }
                }
                Some(blob) => (UniquePtr::null(), facebook::blob_into_iobuf(blob)),
            },
            Err(error) => (into_backingstore_err(error), UniquePtr::null()),
        };
        unsafe { ffi::sapling_backingstore_get_blob_batch_handler(resolver, idx, error, blob) };
    });

    batch_fetch_stats(&fctx)
}

fn batch_fetch_stats(fctx: &FetchContext) -> ffi::BatchFetchStats {
    ffi::BatchFetchStats {
        local_count: fctx.local_fetch_count(),
        remote_count: fctx.remote_fetch_count(),
    }
}

pub fn sapling_backingstore_get_file_aux(
    store: &BackingStore,
    node: &[u8],
    fetch_mode: ffi::FetchMode,
) -> ffi::GetFileAuxResult {
    // the cause is not propagated for this API
    let res = store.get_file_aux(
        FetchContext::new_with_mode_and_cause(FetchMode::from(fetch_mode), FetchCause::EdenUnknown),
        node,
    );

    let (data, error) = resolve_result!(res, transform_some: |aux: ScmStoreFileAuxData| SharedPtr::new(aux.into()), replace_none: SharedPtr::null());
    ffi::GetFileAuxResult { data, error }
}

pub fn sapling_backingstore_get_file_aux_batch(
    store: &BackingStore,
    requests: &[ffi::Request],
    fetch_mode: ffi::FetchMode,
    resolver: SharedPtr<ffi::GetFileAuxBatchResolver>,
) {
    let keys: Vec<Key> = requests.iter().map(|req| req.key()).collect();
    let cause = select_cause(requests.iter().map(|req| req.cause)).0;

    store.get_file_aux_batch(
        FetchContext::new_with_mode_and_cause(FetchMode::from(fetch_mode), cause),
        keys,
        |idx, result| {
            let result: anyhow::Result<ScmStoreFileAuxData> =
                result.and_then(|opt| opt.ok_or_else(|| Error::msg("no file aux data found")));
            let resolver = resolver.clone();
            let (error, aux) = match result {
                Ok(aux) => (UniquePtr::null(), SharedPtr::new(aux.into())),
                Err(error) => (into_backingstore_err(error), SharedPtr::null()),
            };
            unsafe {
                ffi::sapling_backingstore_get_file_aux_batch_handler(resolver, idx, error, aux)
            };
        },
    );
}

pub fn sapling_dogfooding_host(store: &BackingStore) -> Result<bool> {
    store.dogfooding_host().map_err(Into::into)
}

/// Flatten a `CacheUsage` into (state, used, limit) for the FFI struct.
/// `used`/`limit` are 0 unless `Available`; an uncapped `Available` limit
/// maps to `u64::MAX`, the same "uncapped" sentinel already used internally
/// by `indexedlogutil::Store::max_bytes()` for permanent (non-rotated) logs.
fn to_ffi_cache_usage(usage: storemodel::CacheUsage) -> (ffi::CacheUsageState, u64, u64) {
    match usage {
        storemodel::CacheUsage::NotConfigured => (ffi::CacheUsageState::NotConfigured, 0, 0),
        storemodel::CacheUsage::Unsupported => (ffi::CacheUsageState::Unsupported, 0, 0),
        storemodel::CacheUsage::Unavailable => (ffi::CacheUsageState::Unavailable, 0, 0),
        storemodel::CacheUsage::Available { used, limit } => (
            ffi::CacheUsageState::Available,
            used,
            limit.unwrap_or(u64::MAX),
        ),
    }
}

fn to_ffi_cache_stats(stats: HgCacheStats) -> ffi::HgCacheStats {
    let (blob_state, blob_bytes_used, blob_bytes_limit) = to_ffi_cache_usage(stats.blob);
    let (tree_state, tree_bytes_used, tree_bytes_limit) = to_ffi_cache_usage(stats.tree);
    let (lfs_state, lfs_bytes_used, lfs_bytes_limit) = to_ffi_cache_usage(stats.lfs);
    ffi::HgCacheStats {
        cache_path_configured: stats.cache_path.is_some(),
        // `cache_path` is display-only (never used for path I/O), so a
        // lossy UTF-8 conversion here only risks cosmetic corruption of an
        // already-rare non-UTF8 path on Linux - not worth widening the cxx
        // bridge field to raw bytes for this diagnostic-only value.
        cache_path: stats
            .cache_path
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        blob_state,
        blob_bytes_used,
        blob_bytes_limit,
        tree_state,
        tree_bytes_used,
        tree_bytes_limit,
        lfs_state,
        lfs_bytes_used,
        lfs_bytes_limit,
    }
}

pub fn sapling_backingstore_get_cache_stats(store: &BackingStore) -> ffi::GetCacheStatsResult {
    match store.get_cache_stats() {
        Ok(stats) => ffi::GetCacheStatsResult {
            data: SharedPtr::new(to_ffi_cache_stats(stats)),
            error: UniquePtr::null(),
        },
        Err(err) => ffi::GetCacheStatsResult {
            data: SharedPtr::null(),
            error: into_backingstore_err(err),
        },
    }
}

pub fn sapling_backingstore_flush(store: &BackingStore) {
    store.flush();
    store.sync();
}

pub fn sapling_backingstore_check_permission(
    store: &BackingStore,
    manifest_id: &[u8],
) -> ffi::CheckPermissionResult {
    let res = store.check_permission(manifest_id);
    match res {
        Ok(has_access) => ffi::CheckPermissionResult {
            has_access,
            error: UniquePtr::null(),
        },
        Err(e) => ffi::CheckPermissionResult {
            has_access: false,
            error: into_backingstore_err(e),
        },
    }
}

pub fn sapling_backingstore_check_path_permissions(
    store: &BackingStore,
    hg_cs_id: &str,
    paths: Vec<String>,
) -> ffi::GetPathAclInfosResult {
    let requested_paths = paths.clone();
    match store.check_path_permissions(hg_cs_id, paths) {
        Ok(results) => {
            let mut results = results.into_iter();
            let mut data = Vec::with_capacity(requested_paths.len());
            for requested_path in requested_paths {
                let Some(result) = results.next() else {
                    data.push(ffi::PathAclInfo {
                        path: requested_path.clone(),
                        error: format!("path ACL lookup returned no result for {requested_path}"),
                        entries: Vec::new(),
                    });
                    continue;
                };

                let storemodel::PathAclInfo {
                    path,
                    entries,
                    error,
                } = result;
                let response_path = path.to_string();
                if response_path != requested_path {
                    let error = format!(
                        "path ACL lookup returned response for {response_path} instead of {requested_path}"
                    );
                    data.push(ffi::PathAclInfo {
                        path: requested_path,
                        error,
                        entries: Vec::new(),
                    });
                    continue;
                }

                let Some(error) = error else {
                    let entries = entries
                        .into_iter()
                        .map(|entry| ffi::PathAclEntry {
                            restriction_root: entry.restriction_root.to_string(),
                            repo_region_acl: entry.repo_region_acl,
                            permission_request_group: entry.permission_request_group,
                        })
                        .collect();
                    data.push(ffi::PathAclInfo {
                        path: requested_path,
                        error: String::new(),
                        entries,
                    });
                    continue;
                };

                data.push(ffi::PathAclInfo {
                    path: requested_path,
                    error,
                    entries: Vec::new(),
                });
            }

            if let Some(extra) = results.next() {
                let extra_count = 1 + results.count();
                return ffi::GetPathAclInfosResult {
                    data: Vec::new(),
                    error: into_backingstore_err(anyhow!(
                        "path ACL lookup returned {} extra result(s), starting with {}",
                        extra_count,
                        extra.path
                    )),
                };
            }

            ffi::GetPathAclInfosResult {
                data,
                error: UniquePtr::null(),
            }
        }
        Err(error) => ffi::GetPathAclInfosResult {
            data: Vec::new(),
            error: into_backingstore_err(error),
        },
    }
}

pub fn sapling_flush_counters() {
    metrics::Registry::global().sync();
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use edenfs_telemetry::EdenSample;
    use edenfs_telemetry::SampleLogger;

    use super::*;

    #[test]
    fn test_events_logger_adapter_without_live_logger() {
        let logger = Arc::new(EdenFsEventsLoggerAdapter {
            logger: Mutex::new(WeakPtr::null()),
        });

        // An expired weak pointer also upgrades to null. Exercise that path
        // concurrently through the Rust adapter, not just the C++ log helper.
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let logger = Arc::clone(&logger);
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        logger.log(EdenSample::new()).unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
    }

    #[test]
    fn test_batch_fetch_stats_maps_fetch_context_counts() {
        let fctx = FetchContext::new(FetchMode::AllowRemote);
        fctx.inc_local(2);
        fctx.inc_remote(3);

        let stats = batch_fetch_stats(&fctx);
        assert_eq!(stats.local_count, 2);
        assert_eq!(stats.remote_count, 3);
    }

    #[test]
    fn test_to_ffi_cache_stats_maps_each_field_distinctly() {
        // Distinct values per field, including three same-typed triples
        // (blob/tree/lfs used, limit) that a copy-paste transposition would
        // swap without a compile error.
        let stats = HgCacheStats {
            cache_path: Some(PathBuf::from("/tmp/hgcache/repo")),
            blob: storemodel::CacheUsage::Available {
                used: 111,
                limit: Some(222),
            },
            tree: storemodel::CacheUsage::Available {
                used: 333,
                limit: Some(444),
            },
            lfs: storemodel::CacheUsage::Available {
                used: 555,
                limit: Some(666),
            },
        };

        let ffi_stats = to_ffi_cache_stats(stats);

        assert_eq!(ffi_stats.cache_path, "/tmp/hgcache/repo");
        assert!(ffi_stats.cache_path_configured);
        assert!(matches!(
            ffi_stats.blob_state,
            ffi::CacheUsageState::Available
        ));
        assert_eq!(ffi_stats.blob_bytes_used, 111);
        assert_eq!(ffi_stats.blob_bytes_limit, 222);
        assert!(matches!(
            ffi_stats.tree_state,
            ffi::CacheUsageState::Available
        ));
        assert_eq!(ffi_stats.tree_bytes_used, 333);
        assert_eq!(ffi_stats.tree_bytes_limit, 444);
        assert!(matches!(
            ffi_stats.lfs_state,
            ffi::CacheUsageState::Available
        ));
        assert_eq!(ffi_stats.lfs_bytes_used, 555);
        assert_eq!(ffi_stats.lfs_bytes_limit, 666);
    }

    #[test]
    fn test_to_ffi_cache_stats_not_configured_and_unsupported() {
        let stats = HgCacheStats {
            cache_path: None,
            blob: storemodel::CacheUsage::NotConfigured,
            tree: storemodel::CacheUsage::Unsupported,
            lfs: storemodel::CacheUsage::Available {
                used: 1,
                limit: None,
            },
        };

        let ffi_stats = to_ffi_cache_stats(stats);

        assert_eq!(ffi_stats.cache_path, "");
        assert!(!ffi_stats.cache_path_configured);
        assert!(matches!(
            ffi_stats.blob_state,
            ffi::CacheUsageState::NotConfigured
        ));
        assert!(matches!(
            ffi_stats.tree_state,
            ffi::CacheUsageState::Unsupported
        ));
        assert!(matches!(
            ffi_stats.lfs_state,
            ffi::CacheUsageState::Available
        ));
        assert_eq!(ffi_stats.lfs_bytes_used, 1);
        assert_eq!(
            ffi_stats.lfs_bytes_limit,
            u64::MAX,
            "an uncapped Available limit must map to the u64::MAX sentinel"
        );
    }

    #[test]
    fn test_to_ffi_cache_stats_maps_unavailable() {
        // A measurement failure (see `backingstore::cache_usage_or_unavailable`)
        // must map to the `Unavailable` state, not `NotConfigured`/`Unsupported`
        // - those mean something different (no cache / store can't report).
        let stats = HgCacheStats {
            cache_path: Some(PathBuf::from("/tmp/hgcache/repo")),
            blob: storemodel::CacheUsage::Unavailable,
            tree: storemodel::CacheUsage::Available {
                used: 1,
                limit: Some(2),
            },
            lfs: storemodel::CacheUsage::Unavailable,
        };

        let ffi_stats = to_ffi_cache_stats(stats);

        assert!(matches!(
            ffi_stats.blob_state,
            ffi::CacheUsageState::Unavailable
        ));
        assert_eq!(ffi_stats.blob_bytes_used, 0);
        assert_eq!(ffi_stats.blob_bytes_limit, 0);
        assert!(matches!(
            ffi_stats.tree_state,
            ffi::CacheUsageState::Available
        ));
        assert!(matches!(
            ffi_stats.lfs_state,
            ffi::CacheUsageState::Unavailable
        ));
    }

    #[test]
    fn test_select_cause() {
        let causes = [
            ffi::FetchCause::InodeGc,
            ffi::FetchCause::Unknown,
            ffi::FetchCause::Prefetch,
            ffi::FetchCause::Glob,
            ffi::FetchCause::Thrift,
            ffi::FetchCause::Fs,
        ];
        for cause in causes.iter().cloned() {
            let selected = select_cause(std::iter::repeat_n(cause, 3)).0;
            // Repeating causes should always return the same cause
            assert_eq!(selected, cause.into());
        }
        let selected = select_cause(
            std::iter::repeat_n(ffi::FetchCause::Thrift, 3)
                .chain(std::iter::repeat_n(ffi::FetchCause::Prefetch, 2)),
        );

        // Thrift is more popular than Prefetch (> 50%)
        assert_eq!(selected, (FetchCause::EdenThrift, false));

        // There is no single most popular cause
        assert_eq!(
            select_cause(causes.into_iter()),
            (FetchCause::EdenMixed, false)
        );

        // Empty causes
        assert_eq!(
            select_cause(std::iter::empty()),
            (FetchCause::EdenUnknown, false)
        );

        // All the same cause - return `true`.
        let selected = select_cause(std::iter::repeat_n(ffi::FetchCause::Prefetch, 5));
        assert_eq!(selected, (FetchCause::EdenPrefetch, true));
    }
}
