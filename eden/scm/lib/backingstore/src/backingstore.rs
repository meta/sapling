/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use anyhow::Result;
use anyhow::anyhow;
use arc_swap::ArcSwap;
use blob::Blob;
use configloader::Config;
use configloader::hg::PinnedConfig;
use configloader::hg::RepoInfo;
use edenapi::BlockingResponse;
use edenapi::RECENT_DOGFOODING_REQUESTS;
use edenapi::configmodel::ConfigExt;
use edenapi::configmodel::config::ContentHash;
use log::warn;
#[cfg(fbcode_build)]
use metrics_fb::install as install_metrics_sink;
use repo::RepoMinimalInfo;
use repo::repo::Repo;
use revisionstore::scmstore::KeyFetchError;
use smallvec::SmallVec;
use storemodel::BoxIterator;
use storemodel::CacheUsage;
use storemodel::FileAuxData;
use storemodel::FileStore;
use storemodel::PathAclInfo;
use storemodel::TreeAuxData;
use storemodel::TreeEntry;
use storemodel::TreeStore;
use tracing::instrument;
use types::FetchContext;
use types::HgId;
use types::Key;
use types::RepoPath;
use types::RepoPathBuf;
use types::errors::KeyedError;

use crate::ffi::ffi::BackingStoreErrorKind;
use crate::ffi_errors::classify_backingstore_error;
use crate::walk::WalkConfig;
use crate::walk::WalkDetector;

/// Statistics about the hgcache (Sapling disk cache). Named `HgCacheStats`
/// (not `CacheStats`) to avoid colliding with the unrelated in-memory
/// hit/miss-counter `CacheStats` already used by EdenFS's object caches.
///
/// Each of `blob`/`tree`/`lfs` independently reports one of `CacheUsage`'s
/// states (`NotConfigured`, `Unsupported`, `Unavailable`, or
/// `Available{used, limit}`) - these are never conflated into an ambiguous
/// `(0, 0)`.
#[derive(Debug, Clone, Default)]
pub struct HgCacheStats {
    /// The cache directory for this repo, or `None` if no cache is
    /// configured at all (`remotefilelog.reponame`/`cachepath` unset).
    pub cache_path: Option<PathBuf>,
    /// Disk-usage state of the blob (file content) cache.
    pub blob: CacheUsage,
    /// Disk-usage state of the tree (manifest) cache.
    pub tree: CacheUsage,
    /// Disk-usage state of the LFS blob cache. Measures only the
    /// indexedlog-backed portion (`lfs/blobs`, `lfs/pointers`); the
    /// loose-file `lfs/objects` directory is excluded (see
    /// `storemodel::FileStore::lfs_cache_disk_usage`).
    pub lfs: CacheUsage,
}

pub struct BackingStore {
    // ArcSwap is similar to RwLock, but has lower overhead for read operations.
    inner: ArcSwap<Inner>,
}

pub(crate) struct Inner {
    pub(crate) filestore: Arc<dyn FileStore>,
    treestore: Arc<dyn TreeStore>,
    pub(crate) repo: Arc<Repo>,

    // We store these so we can maintain them when reloading ourself.
    extra_configs: Vec<PinnedConfig>,

    // State used to track the touch file and determine if we need to reload ourself.
    create_time: Instant,
    touch_file_mtime: Option<SystemTime>,

    // To prevent multiple threads reloading at the same time.
    already_reloading: AtomicBool,

    // Last time we did a full reload of the Repo.
    last_reload: Instant,

    // Controlled by config "backingstore.reload-check-interval-secs".
    // Sets the minimum delay before we check if we need to reload (defaults to 3s).
    reload_check_interval: Duration,

    // Controlled by config "backingstore.reload-interval-secs".
    // Sets the maximum time since last reload until we force a reload (defaults to 5m).
    reload_interval: Duration,

    /// Counts the full reloads of the store, starting at 1. Walk detectors compare it with the
    /// generation they built their state from, so they follow configuration changes.
    pub(crate) generation: u64,

    pub(crate) walk: WalkConfig,
    restricted_tree_mode: RestrictedTreeMode,
}

#[derive(Debug, Copy, Clone, PartialEq)]
enum RestrictedTreeMode {
    Disabled,
    Logged,
    Enforced,
}

impl RestrictedTreeMode {
    fn from_config(config: &dyn Config) -> Self {
        match config
            .get("experimental", "restricted-tree-mode")
            .as_deref()
        {
            Some("logged") => Self::Logged,
            Some("enforced") => Self::Enforced,
            _ => Self::Disabled,
        }
    }
}

impl BackingStore {
    /// Initialize `BackingStore`.
    pub fn new<P: AsRef<Path>>(root: P) -> Result<Self> {
        Self::new_with_config(root.as_ref(), &[])
    }

    /// Walk detection for the checkout at `mount`. The detector follows this store's
    /// configuration, including the walk mode, on each call that passes the store.
    pub fn walk_detector(&self, mount: &Path) -> WalkDetector {
        let detector = WalkDetector::new(mount);
        detector.sync(self);
        detector
    }

    pub fn name(&self) -> Result<String> {
        match self.maybe_reload().repo.repo_name() {
            Some(repo_name) => Ok(repo_name.to_string()),
            None => Err(anyhow!("no repo name")),
        }
    }

    /// Initialize `BackingStore` with extra configs.
    /// This is used by benches/ to set cache path to control warm/code test cases.
    pub fn new_with_config(root: impl AsRef<Path>, extra_configs: &[String]) -> Result<Self> {
        let extra_configs = extra_configs
            .iter()
            .map(|c| PinnedConfig::Raw(c.to_string().into(), "backingstore".into()))
            .collect::<Vec<_>>();

        Ok(Self {
            inner: ArcSwap::new(Arc::new(Self::new_inner(
                root.as_ref(),
                &extra_configs,
                touch_file_mtime(),
                1,
            )?)),
        })
    }

    fn new_inner(
        root: &Path,
        extra_configs: &[PinnedConfig],
        touch_file_mtime: Option<SystemTime>,
        generation: u64,
    ) -> Result<Inner> {
        constructors::init();

        let info = RepoMinimalInfo::from_repo_root(root.to_path_buf())?;
        let mut config = configloader::hg::load(RepoInfo::Disk(&info), extra_configs)?;

        // Allow overrideing scmstore.tree-metadata-mode for eden only.
        if let Some(mode) = config.get_nonempty("eden", "tree-metadata-mode") {
            config.set(
                "scmstore",
                "tree-metadata-mode",
                Some(mode),
                &"backingstore".into(),
            );
        }

        // EdenFS is a long-lived daemon, not a per-command CLI, so the
        // AI-coding-agent fetch guard's per-process counter doesn't apply
        // here.
        config.set(
            "agent",
            "enable-fetch-guard",
            Some("false"),
            &"backingstore".into(),
        );

        #[cfg(feature = "scuba")]
        edenfs_telemetry::tracing_logger::set_logged_targets(
            config
                .get_or::<Vec<String>>("backingstore", "logged-tracing-targets", || {
                    vec!["big_walk".to_string()]
                })?
                .into_iter()
                .collect(),
        );

        // Apply indexed log configs, which can affect edenfs behavior.
        indexedlog_config::configure(&config)?;

        let repo = Repo::load_with_config(root, config)?;
        let filestore = repo.file_store()?;
        let treestore = repo.tree_store()?;

        let config = repo.config().clone();

        #[cfg(fbcode_build)]
        {
            let is_obc_enabled = config.get_or::<bool>("scmstore", "enable-obc", || false)?;
            if let Err(err) = install_metrics_sink(is_obc_enabled) {
                tracing::warn!(?err, "error creating metrics sink");
            }
        }

        let walk = WalkConfig::from_config(config.as_ref())?;
        let restricted_tree_mode = RestrictedTreeMode::from_config(config.as_ref());

        let repo = Arc::new(repo);

        Ok(Inner {
            treestore,
            filestore,
            repo,
            extra_configs: extra_configs.to_vec(),
            create_time: Instant::now(),
            touch_file_mtime,
            already_reloading: AtomicBool::new(false),
            last_reload: Instant::now(),
            reload_check_interval: config
                .get_opt("backingstore", "reload-check-interval-secs")?
                .unwrap_or(Duration::from_secs(3)),
            reload_interval: config
                .get_opt("backingstore", "reload-interval-secs")?
                .unwrap_or(Duration::from_mins(5)),
            generation,
            walk,
            restricted_tree_mode,
        })
    }

    #[instrument(level = "trace", skip(self))]
    pub fn get_blob(&self, fctx: FetchContext, node: &[u8]) -> Result<Option<Blob>> {
        self.maybe_reload().filestore.single(fctx, node)
    }

    /// Fetch file contents in batch. Whenever a blob is fetched, the supplied `resolve` function is
    /// called with the file content or an error message, and the index of the blob in the request
    /// array.
    #[instrument(level = "trace", skip(self, resolve))]
    pub fn get_blob_batch<F>(&self, fctx: FetchContext, keys: Vec<Key>, resolve: F)
    where
        F: Fn(usize, Result<Option<Blob>>),
    {
        self.maybe_reload()
            .filestore
            .batch_with_callback(fctx, keys, resolve)
    }

    #[instrument(level = "trace", skip(self))]
    pub fn get_manifest(&self, node: &[u8]) -> Result<[u8; 20]> {
        let inner = self.maybe_reload();
        let hgid = HgId::from_slice(node)?;
        let root_tree_id = match inner.repo.tree_resolver()?.get_root_id(&hgid) {
            Ok(root_tree_id) => root_tree_id,
            Err(_e) => {
                // This call may fail with a `NotFoundError` if the revision in question
                // was added to the repository after we originally opened it. Invalidate
                // the repository and try again, in case our cached repo data is just stale.
                inner.repo.invalidate_all()?;
                inner.repo.tree_resolver()?.get_root_id(&hgid)?
            }
        };
        Ok(root_tree_id.into_byte_array())
    }

    #[instrument(level = "trace", skip(self))]
    pub fn get_tree(&self, fctx: FetchContext, node: &[u8]) -> Result<Option<Arc<dyn TreeEntry>>> {
        self.maybe_reload().treestore.single(fctx, node)
    }

    /// Fetch tree contents in batch. Whenever a tree is fetched, the supplied `resolve` function is
    /// called with the tree content or an error message, and the index of the tree in the request
    /// array.
    #[instrument(level = "trace", skip(self, resolve))]
    pub fn get_tree_batch<F>(&self, fctx: FetchContext, keys: Vec<Key>, resolve: F)
    where
        F: Fn(usize, Result<Option<Arc<dyn TreeEntry>>>),
    {
        self.maybe_reload()
            .treestore
            .batch_with_callback(fctx, keys, resolve)
    }

    pub fn get_file_aux(&self, fctx: FetchContext, node: &[u8]) -> Result<Option<FileAuxData>> {
        self.maybe_reload().filestore.single(fctx, node)
    }

    pub fn get_file_aux_batch<F>(&self, fctx: FetchContext, keys: Vec<Key>, resolve: F)
    where
        F: Fn(usize, Result<Option<FileAuxData>>),
    {
        self.maybe_reload()
            .filestore
            .batch_with_callback(fctx, keys, resolve)
    }

    pub fn get_tree_aux(&self, fctx: FetchContext, node: &[u8]) -> Result<Option<TreeAuxData>> {
        self.maybe_reload().treestore.single(fctx, node)
    }

    pub fn get_tree_aux_batch<F>(&self, fctx: FetchContext, keys: Vec<Key>, resolve: F)
    where
        F: Fn(usize, Result<Option<TreeAuxData>>),
    {
        self.maybe_reload()
            .treestore
            .batch_with_callback(fctx, keys, resolve)
    }

    pub fn dogfooding_host(&self) -> Result<bool> {
        Ok(RECENT_DOGFOODING_REQUESTS.get())
    }

    /// Forces backing store to rescan pack files or local indexes
    #[instrument(level = "trace", skip(self))]
    pub fn sync(&self) {
        // We don't need maybe_reload() here. It doesn't make sense to
        // potentially reload everything right before syncing it again
        // (although it wouldn't hurt).
        let inner = self.inner.load();

        inner.filestore.sync().ok();
        inner.treestore.sync().ok();
    }

    #[instrument(level = "trace", skip(self))]
    pub fn flush(&self) {
        // No need to maybe_reload() - flush intends to operate on current backingstore.
        // It wouldn't hurt, though, since reloading also flushes.
        self.inner.load().flush();
    }

    #[instrument(level = "trace", skip(self))]
    pub fn check_permission(&self, manifest_id: &[u8]) -> Result<bool> {
        use edenapi::types::CheckManifestPermissionRequest;

        let inner = self.maybe_reload();
        if inner.restricted_tree_mode == RestrictedTreeMode::Disabled {
            return Ok(true);
        }

        let id = HgId::from_slice(manifest_id)?;
        let request = CheckManifestPermissionRequest {
            manifest_ids: vec![id],
        };
        let response = BlockingResponse::from_async(
            inner
                .repo
                .eden_api()
                .map_err(|err| err.tag_network())?
                .check_manifest_permission(request),
        )?;
        for entry in response.entries {
            if entry.manifest_id == id {
                if inner.restricted_tree_mode == RestrictedTreeMode::Logged {
                    if !entry.has_access {
                        tracing::info!(
                            hgid = %id,
                            acl = entry.request_acl.as_deref().unwrap_or("unknown-acl"),
                            "restricted tree detected (logged mode, not enforcing)"
                        );
                    }
                    return Ok(true);
                }
                return Ok(entry.has_access);
            }
        }
        // If no response for our ID, default to allowing access (fail-open)
        Ok(true)
    }

    #[instrument(level = "trace", skip(self, paths))]
    pub fn check_path_permissions(
        &self,
        hg_cs_id: &str,
        paths: Vec<String>,
    ) -> Result<Vec<PathAclInfo>> {
        let hg_cs_id = HgId::from_hex(hg_cs_id.as_bytes())?;
        let path_bufs = paths
            .into_iter()
            .map(RepoPathBuf::from_string)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.maybe_reload()
            .treestore
            .check_path_permissions(hg_cs_id, path_bufs)
    }

    /// Get cache statistics for the hgcache (Sapling disk cache).
    ///
    /// Uses the store's indexedlog `disk_usage()` / `max_bytes()` APIs, which
    /// read small on-disk metadata files rather than scanning cache contents,
    /// keeping this O(number of rotated logs) rather than O(cache size).
    /// `disk_usage()` flushes pending in-memory writes first, so this
    /// reflects data written by this process up to the moment of the call
    /// (it cannot see writes still pending flush in another process's
    /// handle to the same cache).
    ///
    /// Returns `Err` only for the cache-path resolution itself failing (rare
    /// - a config-loading bug). Each of `blob`/`tree`/`lfs` is otherwise
    /// infallible from the caller's perspective: a measurement failure for
    /// one field is logged and downgraded to `CacheUsage::Unavailable`
    /// rather than aborting the whole response, so one broken cache doesn't
    /// suppress the other two, healthy ones.
    #[instrument(level = "trace", skip(self))]
    pub fn get_cache_stats(&self) -> Result<HgCacheStats> {
        let inner = self.maybe_reload();
        let config = inner.repo.config();

        // Resolve per-repo cache path for display purposes. A read-only
        // lookup: querying stats must never create the cache directory as a
        // side effect, and "not configured" is a normal, non-error outcome
        // here (unlike `get_cache_path`, which both of those things).
        let cache_path = get_cache_path_for_stats(config)?;

        let blob = cache_usage_or_unavailable(inner.filestore.cache_disk_usage(), "blob");
        let tree = cache_usage_or_unavailable(inner.treestore.cache_disk_usage(), "tree");
        let lfs = cache_usage_or_unavailable(inner.filestore.lfs_cache_disk_usage(), "LFS");

        Ok(HgCacheStats {
            cache_path,
            blob,
            tree,
            lfs,
        })
    }

    // Fully reload the stores if:
    //   - a touch file has a newer mtime than last time we checked, or
    //   - the touch file exists and didn't exist last time, or
    //   - sapling configs appear changed on disk
    //
    // The main purpose of reloading is to allow a running EdenFS to pick up
    // Sapling config changes that affect fetching/caching.
    //
    // We perform the check at most once every `reload_check_interval=3`
    // seconds. If we aren't reloading, we still swap out the Inner object
    // solely to reset the state we use to track the touch file (i.e. we keep
    // all the store objects the same). Any errors reloading are ignored and the
    // existing stores are used.
    //
    // We return an arc_swap::Guard so we only call inner.load() once normally.
    #[instrument(level = "trace", skip(self))]
    pub(crate) fn maybe_reload(&self) -> arc_swap::Guard<Arc<Inner>> {
        let inner = self.inner.load();

        if inner.create_time.elapsed() < inner.reload_check_interval {
            return inner;
        }

        tracing::debug!("checking if we need to reload");

        if inner.already_reloading.swap(true, Ordering::AcqRel) {
            tracing::debug!("another thread is already reloading");
            // No need to wait - just serve up the old one for now.
            return inner;
        }

        let since_last_reload = inner.last_reload.elapsed();

        let mut needs_reload = false;

        // If it has been at least `reload_interval`, check if sapling config has changed.
        if !inner.reload_interval.is_zero() && since_last_reload >= inner.reload_interval {
            if let Ok(info) = RepoMinimalInfo::from_repo_root(inner.repo.path().to_owned()) {
                if let Ok(config) =
                    configloader::hg::load(RepoInfo::Disk(&info), &inner.extra_configs)
                {
                    if let Some(reason) =
                        diff_config_files(&inner.repo.config().files(), &config.files())
                    {
                        tracing::info!("sapling config files differ: {reason}");
                        needs_reload = true;
                    } else {
                        tracing::debug!("sapling config files haven't changed");
                    }
                }
            }
        };

        let new_mtime = touch_file_mtime();

        tracing::debug!(last_reload=?since_last_reload, old_mtime=?inner.touch_file_mtime, ?new_mtime, "checking touch file");

        needs_reload |= new_mtime
            .as_ref()
            .is_some_and(|new_mtime| match &inner.touch_file_mtime {
                Some(old_mtime) => new_mtime > old_mtime,
                None => true,
            });

        let new_inner = if needs_reload {
            tracing::info!("reloading backing store");

            // We are actually going to reload. Flush first to make sure pending
            // cache writes are picked up by newly initialized backingstore.
            // There is no locking, so some cache writes could be missed by the reload.
            inner.flush();

            match Self::new_inner(
                inner.repo.path(),
                &inner.extra_configs,
                new_mtime,
                inner.generation + 1,
            ) {
                Ok(mut new_inner) => {
                    new_inner.last_reload = Instant::now();
                    new_inner
                }
                Err(err) => {
                    tracing::warn!(?err, "error reloading backingstore");
                    inner.as_ref().soft_reload(new_mtime)
                }
            }
        } else {
            tracing::debug!("not reloading backing store");
            inner.as_ref().soft_reload(new_mtime)
        };

        self.inner.store(Arc::new(new_inner));

        if needs_reload {
            // Flush the old stores again right after the swaperoo. This should help
            // reduce the window for missed cache writes. This flush is effective even
            // though we have already created new stores since the scmstore indexedlogs
            // automatically notice things have changed on disk during the read path.
            inner.flush();
        }

        self.inner.load()
    }
}

fn diff_config_files(
    old: &[(PathBuf, Option<ContentHash>)],
    new: &[(PathBuf, Option<ContentHash>)],
) -> Option<String> {
    let mut new: HashMap<PathBuf, Option<ContentHash>> = new.iter().cloned().collect();

    for (old_path, old_hash) in old.iter() {
        if let Some(new_hash) = new.remove(old_path) {
            let mismatch = match (old_hash, new_hash) {
                (None, None) => false,
                (None, Some(_)) => true,
                (Some(_), None) => true,
                (Some(old_hash), Some(ref new_hash)) => old_hash != new_hash,
            };

            if mismatch {
                return Some(format!("file {} metadata mismatch", old_path.display()));
            }
        } else {
            return Some(format!("file {} was deleted", old_path.display()));
        }
    }

    // Anything left is a new file we didn't have last time.
    new.keys()
        .next()
        .map(|added| format!("file {} was added", added.display()))
}

impl Inner {
    // Perform a shallow clone, retaining stores but resetting state related to the touch file.
    fn soft_reload(&self, touch_file_mtime: Option<SystemTime>) -> Self {
        Self {
            filestore: self.filestore.clone(),
            treestore: self.treestore.clone(),
            repo: self.repo.clone(),
            extra_configs: self.extra_configs.clone(),

            touch_file_mtime,
            create_time: Instant::now(),
            last_reload: self.last_reload,
            already_reloading: AtomicBool::new(false),
            reload_check_interval: self.reload_check_interval,
            reload_interval: self.reload_interval,
            generation: self.generation,

            walk: self.walk.clone(),
            restricted_tree_mode: self.restricted_tree_mode,
        }
    }

    fn flush(&self) {
        self.filestore.sync().ok();
        self.treestore.sync().ok();

        // Sync pending counters to ODS/OBC.
        metrics::Registry::global().sync();
    }
}

fn touch_file_mtime() -> Option<SystemTime> {
    let path = if cfg!(windows) {
        std::env::var_os("PROGRAMDATA")
            .map(|dir| PathBuf::from(dir).join(r"Facebook\Mercurial\eden_reload"))
    } else {
        Some(PathBuf::from("/etc/mercurial/eden_reload"))
    };

    let path = path?;
    let res = path.metadata();

    tracing::debug!(?path, ?res, "statting touch file");

    res.ok()?.modified().ok()
}

/// Get the cache path for a repo from config, for display in cache stats.
/// Returns `None` if no cache is configured - a normal outcome, not an
/// error.
///
/// Delegates to `revisionstore::util::peek_cache_path` - the read-only
/// sibling of `get_cache_path` (the path resolution every revisionstore
/// cache uses) - rather than re-deriving reponame/cachepath here, so this
/// can never drift from where the cache actually lives, while also never
/// creating the cache directory as a side effect of a stats query.
fn get_cache_path_for_stats(config: &dyn Config) -> Result<Option<PathBuf>> {
    revisionstore::util::peek_cache_path(config)
}

/// Downgrade a per-field `cache_disk_usage()`/`lfs_cache_disk_usage()`
/// failure into `CacheUsage::Unavailable` instead of letting it abort the
/// whole `HgCacheStats` response - the failure reason is logged here so it
/// isn't silently dropped, but the other fields still get a chance to
/// report their (possibly healthy) state.
fn cache_usage_or_unavailable(result: Result<CacheUsage>, field: &str) -> CacheUsage {
    match result {
        Ok(usage) => usage,
        Err(err) => {
            warn!("failed to read {field} cache disk usage: {err:#}");
            CacheUsage::Unavailable
        }
    }
}

/// Given a single point local fetch function, and a "streaming" (via iterator)
/// remote fetch function, provide `batch_with_callback` for ease-of-use.
trait LocalRemoteImpl<IntermediateType, OutputType = IntermediateType>
where
    IntermediateType: Into<OutputType> + Clone,
{
    fn get_local_single(&self, path: &RepoPath, id: HgId) -> Result<Option<IntermediateType>>;
    fn get_single(&self, fctx: FetchContext, path: &RepoPath, id: HgId)
    -> Result<IntermediateType>;
    fn get_batch_iter(
        &self,
        fctx: FetchContext,
        keys: Vec<Key>,
    ) -> Result<BoxIterator<Result<(Key, IntermediateType)>>>;

    // The following methods are "derived" from the above.

    fn single(&self, fctx: FetchContext, node: &[u8]) -> Result<Option<OutputType>> {
        let hgid = HgId::from_slice(node)?;
        if fctx.mode().is_local() {
            let maybe_value = self
                .get_local_single(RepoPath::empty(), hgid)?
                .map(|v| v.into());
            Ok(maybe_value)
        } else {
            // FetchMode::RemoteOnly and FetchMode::AllowRemote
            let value = self.get_single(fctx, RepoPath::empty(), hgid)?;
            let value = value.into();
            Ok(Some(value))
        }
    }

    fn batch_with_callback<F>(&self, fctx: FetchContext, keys: Vec<Key>, resolve: F)
    where
        F: Fn(usize, Result<Option<OutputType>>),
    {
        if fctx.mode().is_local() {
            // PERF: In some cases this might be sped up using threads in theory.
            // But this needs to be backed by real benchmark data. Besides, edenfs
            // does not call into this path often.
            for (i, key) in keys.iter().enumerate() {
                let result = self.get_local_single(&key.path, key.hgid);
                let result: Result<Option<OutputType>> = match result {
                    Ok(Some(v)) => {
                        fctx.inc_local(1);
                        Ok(Some(v.into()))
                    }
                    Ok(None) => Ok(None),
                    Err(e) => Err(e),
                };
                resolve(i, result);
            }
        } else {
            let ignore_result = fctx.mode().ignore_result();
            let mut key_to_index = indexed_keys(&keys);
            let mut errors = Vec::new();
            match self.get_batch_iter(fctx, keys) {
                Err(e) => errors.push(e),
                Ok(iter) => {
                    for entry in iter {
                        let (key, data) = match entry {
                            Err(e) => {
                                match handle_keyed_fetch_error(&mut key_to_index, &resolve, e) {
                                    Ok(()) => continue,
                                    Err(e) => errors.push(e),
                                }
                                continue;
                            }
                            Ok(v) => v,
                        };
                        if let Some(indices) = key_to_index.remove(&key) {
                            for idx in indices.iter().skip(1) {
                                resolve(*idx, Ok(Some(data.clone().into())));
                            }
                            if let Some(first_idx) = indices.first() {
                                resolve(*first_idx, Ok(Some(data.into())));
                            }
                        }
                    }
                }
            }

            if !key_to_index.is_empty() {
                if ignore_result {
                    // In ignore_result mode, we (intentionally) don't get any results. Propagate as `None`.
                    for (_key, indices) in key_to_index.into_iter() {
                        for index in indices {
                            resolve(index, Ok(None));
                        }
                    }
                } else {
                    // In ffi.rs, the error is converted to a String where, later, empty string is assumed to mean no error.
                    // Ensure we have some error.
                    if errors.is_empty() {
                        errors.push(anyhow!(
                            "{} items in batch missing, but got no errors from get_batch_iter",
                            key_to_index.len()
                        ));
                    }

                    // Report errors. We don't know the index -> error mapping so
                    // we bundle all errors we received.
                    let error = ErrorCollection(Arc::new(errors));
                    for (_key, indices) in key_to_index.into_iter() {
                        for index in indices {
                            resolve(index, Err(error.clone().into()));
                        }
                    }
                }
            }
        }
    }
}

fn handle_keyed_fetch_error<OutputType, F>(
    key_to_index: &mut HashMap<Key, SmallVec<[usize; 1]>>,
    resolve: &F,
    err: anyhow::Error,
) -> std::result::Result<(), anyhow::Error>
where
    F: Fn(usize, Result<Option<OutputType>>),
{
    let key_fetch_error = err.downcast::<KeyFetchError>()?;

    let KeyFetchError::KeyedError(KeyedError(key, err)) = key_fetch_error else {
        return Err(key_fetch_error.into());
    };

    let Some(indices) = key_to_index.remove(&key) else {
        tracing::debug!(%key, ?err, "ignoring fetch error for key outside backingstore batch");
        return Ok(());
    };

    let error = SharedKeyedError {
        key,
        source: Arc::new(err),
    };
    for index in indices {
        resolve(index, Err(error.clone().into()));
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct SharedKeyedError {
    key: Key,
    source: Arc<anyhow::Error>,
}

impl fmt::Display for SharedKeyedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "key fetch failed {}: {:#}", self.key, self.source)
    }
}

impl std::error::Error for SharedKeyedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref().as_ref())
    }
}

/// Read file content.
impl LocalRemoteImpl<Blob> for Arc<dyn FileStore> {
    fn get_local_single(&self, path: &RepoPath, id: HgId) -> Result<Option<Blob>> {
        self.get_local_content(path, id)
    }
    fn get_single(&self, fctx: FetchContext, path: &RepoPath, id: HgId) -> Result<Blob> {
        self.get_content(fctx, path, id)
    }
    fn get_batch_iter(
        &self,
        fctx: FetchContext,
        keys: Vec<Key>,
    ) -> Result<BoxIterator<Result<(Key, Blob)>>> {
        Ok(Box::new(self.get_content_iter(fctx, keys)?.into_iter()))
    }
}

/// Read file aux.
impl LocalRemoteImpl<FileAuxData> for Arc<dyn FileStore> {
    fn get_local_single(&self, path: &RepoPath, id: HgId) -> Result<Option<FileAuxData>> {
        self.get_local_aux(path, id)
    }
    fn get_single(&self, fctx: FetchContext, path: &RepoPath, id: HgId) -> Result<FileAuxData> {
        self.get_aux(fctx, path, id)
    }
    fn get_batch_iter(
        &self,
        fctx: FetchContext,
        keys: Vec<Key>,
    ) -> Result<BoxIterator<Result<(Key, FileAuxData)>>> {
        self.get_aux_iter(fctx, keys)
    }
}

/// Read tree content.
impl LocalRemoteImpl<Arc<dyn TreeEntry>> for Arc<dyn TreeStore> {
    fn get_local_single(&self, path: &RepoPath, id: HgId) -> Result<Option<Arc<dyn TreeEntry>>> {
        self.get_local_tree(path, id)
    }
    fn get_single(
        &self,
        fctx: FetchContext,
        path: &RepoPath,
        id: HgId,
    ) -> Result<Arc<dyn TreeEntry>> {
        match self
            .get_tree_iter(fctx, vec![Key::new(path.to_owned(), id)])?
            .into_iter()
            .next()
        {
            Some(Ok((_key, tree))) => Ok(tree),
            Some(Err(e)) => Err(e),
            None => Err(anyhow::format_err!("{path}@{id}: not found remotely")),
        }
    }
    fn get_batch_iter(
        &self,
        fctx: FetchContext,
        keys: Vec<Key>,
    ) -> Result<BoxIterator<Result<(Key, Arc<dyn TreeEntry>)>>> {
        Ok(Box::new(self.get_tree_iter(fctx, keys)?.into_iter()))
    }
}

/// Read tree aux.
impl LocalRemoteImpl<TreeAuxData> for Arc<dyn TreeStore> {
    fn get_local_single(&self, path: &RepoPath, id: HgId) -> Result<Option<TreeAuxData>> {
        self.get_local_tree_aux_data(path, id)
    }
    fn get_single(&self, fctx: FetchContext, path: &RepoPath, id: HgId) -> Result<TreeAuxData> {
        self.get_tree_aux_data(fctx, path, id)
    }
    fn get_batch_iter(
        &self,
        fctx: FetchContext,
        keys: Vec<Key>,
    ) -> Result<BoxIterator<Result<(Key, TreeAuxData)>>> {
        self.get_tree_aux_data_iter(fctx, keys)
    }
}

/// This type is just for display.
#[derive(Debug, Clone)]
struct ErrorCollection(Arc<Vec<anyhow::Error>>);

impl fmt::Display for ErrorCollection {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some((first, rest)) = self.0.split_first() {
            first.fmt(f)?;

            let n = rest.len();
            if n > 0 {
                write!(f, "\n-- and {n} more errors --\n")?;
                for e in rest {
                    e.fmt(f)?;
                    write!(f, "\n--\n")?;
                }
            }
        }
        Ok(())
    }
}

impl std::error::Error for ErrorCollection {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        let source = self.0.first()?;

        // PermissionDenied is key-specific. If one slips through as a loose
        // batch iterator error, do not let missing unrelated keys inherit that
        // classification. Other loose errors, such as network failures, may
        // still be meaningful batch-wide causes and should remain visible.
        if classify_backingstore_error(source).0 == BackingStoreErrorKind::PermissionDenied {
            None
        } else {
            Some(source.as_ref())
        }
    }
}

/// Index &[Key] so they can be converted back to the index(es).
fn indexed_keys(keys: &[Key]) -> HashMap<Key, SmallVec<[usize; 1]>> {
    keys.iter()
        .enumerate()
        .fold(HashMap::with_capacity(keys.len()), |mut map, (i, k)| {
            map.entry(k.clone()).or_default().push(i);
            map
        })
}

impl Drop for BackingStore {
    fn drop(&mut self) {
        // Make sure that all the data that was fetched is written to the hgcache.
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::fmt;
    use std::path::Path;

    use edenapi::types::SaplingRemoteApiServerError;
    use edenapi::types::SaplingRemoteApiServerErrorKind;
    use types::RepoPathBuf;
    use types::fetch_mode::FetchMode;

    use super::*;

    #[test]
    fn test_blob_read_reload_check_interval() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let repo = eagerepo::EagerRepo::open(dir.path())?;
        drop(repo);

        for (interval, elapsed_secs, should_check) in [
            (None, 0, false),
            (None, 4, true),
            (Some("5"), 4, false),
            (Some("0"), 0, true),
        ] {
            let configs = interval
                .map(|value| format!("backingstore.reload-check-interval-secs={value}"))
                .into_iter()
                .collect::<Vec<_>>();
            let store = BackingStore::new_with_config(dir.path(), &configs)?;
            let mut inner = store.inner.load().soft_reload(touch_file_mtime());
            // Advance the stored check age without sleeping or touching the host sentinel.
            inner.create_time = Instant::now() - Duration::from_secs(elapsed_secs);
            let before = Arc::new(inner);
            store.inner.store(before.clone());

            store.get_blob(
                FetchContext::new(FetchMode::LocalOnly),
                HgId::null_id().as_ref(),
            )?;

            assert_eq!(
                !Arc::ptr_eq(&before, &store.inner.load()),
                should_check,
                "interval={interval:?}, elapsed_secs={elapsed_secs}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_get_cache_path_for_stats_joins_reponame_and_cachepath() {
        let config = BTreeMap::from([
            ("remotefilelog.reponame", "test-repo"),
            ("remotefilelog.cachepath", "/tmp/hgcache"),
        ]);

        let path = get_cache_path_for_stats(&config).unwrap();

        assert_eq!(path, Some(Path::new("/tmp/hgcache").join("test-repo")));
    }

    #[test]
    fn test_get_cache_path_for_stats_none_when_reponame_missing() {
        let config = BTreeMap::from([("remotefilelog.cachepath", "/tmp/hgcache")]);

        // Not configured is a normal outcome, not an error.
        assert_eq!(get_cache_path_for_stats(&config).unwrap(), None);
    }

    #[test]
    fn test_get_cache_path_for_stats_none_when_cachepath_missing() {
        let config = BTreeMap::from([("remotefilelog.reponame", "test-repo")]);

        assert_eq!(get_cache_path_for_stats(&config).unwrap(), None);
    }

    #[test]
    fn test_cache_usage_or_unavailable_passes_through_ok() {
        let usage = CacheUsage::Available {
            used: 5,
            limit: Some(10),
        };
        assert_eq!(cache_usage_or_unavailable(Ok(usage), "blob"), usage);
    }

    #[test]
    fn test_cache_usage_or_unavailable_downgrades_err() {
        // A measurement failure for one field must become `Unavailable`,
        // not propagate as an error that would abort the other fields'
        // results too (see `get_cache_stats`).
        let result: Result<CacheUsage> = Err(anyhow::anyhow!("corrupted log"));
        assert_eq!(
            cache_usage_or_unavailable(result, "tree"),
            CacheUsage::Unavailable
        );
    }

    struct MissingBatchStore;

    impl LocalRemoteImpl<u8> for MissingBatchStore {
        fn get_local_single(&self, _path: &RepoPath, _id: HgId) -> Result<Option<u8>> {
            Ok(None)
        }

        fn get_single(&self, _fctx: FetchContext, _path: &RepoPath, _id: HgId) -> Result<u8> {
            unreachable!("test only exercises batch fetches")
        }

        fn get_batch_iter(
            &self,
            _fctx: FetchContext,
            _keys: Vec<Key>,
        ) -> Result<BoxIterator<Result<(Key, u8)>>> {
            let permission_denied = SaplingRemoteApiServerError {
                err: SaplingRemoteApiServerErrorKind::PermissionDenied {
                    tree_id: *HgId::null_id(),
                    request_acl: "test-acl".to_string(),
                    denial_message: None,
                },
                key: None,
            };
            Ok(Box::new(std::iter::once(Err(permission_denied.into()))))
        }
    }

    struct KeyedErrorBatchStore {
        denied_key: Key,
    }

    impl LocalRemoteImpl<u8> for KeyedErrorBatchStore {
        fn get_local_single(&self, _path: &RepoPath, _id: HgId) -> Result<Option<u8>> {
            Ok(None)
        }

        fn get_single(&self, _fctx: FetchContext, _path: &RepoPath, _id: HgId) -> Result<u8> {
            unreachable!("test only exercises batch fetches")
        }

        fn get_batch_iter(
            &self,
            _fctx: FetchContext,
            _keys: Vec<Key>,
        ) -> Result<BoxIterator<Result<(Key, u8)>>> {
            let permission_denied = SaplingRemoteApiServerError {
                err: SaplingRemoteApiServerErrorKind::PermissionDenied {
                    tree_id: self.denied_key.hgid,
                    request_acl: "test-acl".to_string(),
                    denial_message: None,
                },
                key: Some(self.denied_key.clone()),
            };
            Ok(Box::new(std::iter::once(Err(KeyFetchError::KeyedError(
                KeyedError(self.denied_key.clone(), permission_denied.into()),
            )
            .into()))))
        }
    }

    #[test]
    fn test_batch_missing_key_error_does_not_inherit_permission_denied_source() {
        let store = MissingBatchStore;
        let missing_key = Key::new(
            RepoPathBuf::from_string("missing".to_string()).unwrap(),
            HgId::from_hex(b"1111111111111111111111111111111111111111").unwrap(),
        );
        let results = RefCell::new(Vec::new());

        store.batch_with_callback(
            FetchContext::new(FetchMode::AllowRemote),
            vec![missing_key],
            |_, result| results.borrow_mut().push(result),
        );

        let error = results
            .into_inner()
            .pop()
            .expect("callback should be called for the missing key")
            .expect_err("missing key should receive an error");
        assert!(
            error
                .chain()
                .all(|err| err.downcast_ref::<SaplingRemoteApiServerError>().is_none()),
            "missing-key batch errors must not inherit unrelated PermissionDenied causes: {error:?}"
        );
    }

    #[test]
    fn test_batch_keyed_error_is_reported_only_for_that_key() {
        let denied_key = Key::new(
            RepoPathBuf::from_string("denied".to_string()).unwrap(),
            HgId::from_hex(b"1111111111111111111111111111111111111111").unwrap(),
        );
        let missing_key = Key::new(
            RepoPathBuf::from_string("missing".to_string()).unwrap(),
            HgId::from_hex(b"2222222222222222222222222222222222222222").unwrap(),
        );
        let store = KeyedErrorBatchStore {
            denied_key: denied_key.clone(),
        };
        let results = RefCell::new(Vec::new());

        store.batch_with_callback(
            FetchContext::new(FetchMode::AllowRemote),
            vec![denied_key, missing_key],
            |idx, result| results.borrow_mut().push((idx, result)),
        );

        let results = results.into_inner();
        let denied_error = results
            .iter()
            .find(|(idx, _)| *idx == 0)
            .expect("denied key should receive an error")
            .1
            .as_ref()
            .expect_err("denied key should receive an error");
        assert!(
            denied_error
                .chain()
                .any(|err| err.downcast_ref::<SaplingRemoteApiServerError>().is_some()),
            "denied key should keep its PermissionDenied cause: {denied_error:?}"
        );

        let missing_error = results
            .iter()
            .find(|(idx, _)| *idx == 1)
            .expect("missing key should receive an error")
            .1
            .as_ref()
            .expect_err("missing key should receive an error");
        assert!(
            missing_error
                .chain()
                .all(|err| err.downcast_ref::<SaplingRemoteApiServerError>().is_none()),
            "missing key must not inherit keyed PermissionDenied causes: {missing_error:?}"
        );
    }

    #[derive(Debug)]
    struct TestNetworkError;

    impl fmt::Display for TestNetworkError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("test network error")
        }
    }

    impl std::error::Error for TestNetworkError {}

    #[test]
    fn test_batch_missing_key_error_preserves_non_permission_denied_source() {
        let collection = ErrorCollection(Arc::new(vec![TestNetworkError.into()]));

        let source = std::error::Error::source(&collection)
            .expect("non-PermissionDenied source should be preserved");
        assert!(
            source.downcast_ref::<TestNetworkError>().is_some(),
            "non-PermissionDenied source should remain visible: {source:?}"
        );
    }
}
