/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use cas_client::CasFetchManager;
use configmodel::Config;
use configmodel::ConfigExt;
use configmodel::convert::ByteCount;
use edenapi::Builder;
use fn_error_context::context;
use hgtime::HgTime;
use parking_lot::Mutex;
use progress_model::AggregatingProgressBar;
use storemodel::SerializationFormat;

use crate::IndexedLogHgIdHistoryStore;
use crate::SaplingRemoteApiFileStore;
use crate::SaplingRemoteApiTreeStore;
use crate::indexedlogauxstore::AuxStore;
use crate::indexedlogdatastore::IndexedLogHgIdDataStore;
use crate::indexedlogdatastore::IndexedLogHgIdDataStoreConfig;
use crate::indexedlogtreeauxstore::TreeAuxStore;
use crate::indexedlogutil::StoreType;
use crate::lfs::LfsClient;
use crate::lfs::LfsStore;
use crate::scmstore::FileStore;
use crate::scmstore::TreeStore;
use crate::scmstore::activitylogger::ActivityLogger;
use crate::scmstore::fetch::MaxFetchCount;
use crate::scmstore::file::FileStoreMetrics;
use crate::scmstore::tree::TreeMetadataMode;
use crate::scmstore::tree::new_acl_check_cache;
use crate::util::RUN_ONCE_FILENAME;
use crate::util::check_run_once;
use crate::util::get_cache_path;
use crate::util::get_indexedlogdatastore_aux_path;
use crate::util::get_indexedlogdatastore_path;
use crate::util::get_indexedloghistorystore_path;
use crate::util::get_local_path;
use crate::util::get_tree_aux_store_path;

pub struct FileStoreBuilder<'a> {
    config: &'a dyn Config,
    local_path: Option<PathBuf>,
    suffix: Option<PathBuf>,
    override_edenapi: Option<bool>,

    indexedlog_local: Option<Arc<IndexedLogHgIdDataStore>>,
    indexedlog_cache: Option<Arc<IndexedLogHgIdDataStore>>,

    edenapi: Option<Arc<SaplingRemoteApiFileStore>>,
    cas_manager: Option<Arc<CasFetchManager>>,
    format: Option<SerializationFormat>,

    max_fetch_count: MaxFetchCount,
}

impl<'a> FileStoreBuilder<'a> {
    pub fn new(config: &'a dyn Config) -> Self {
        Self {
            config,
            local_path: None,
            suffix: None,
            override_edenapi: None,
            indexedlog_local: None,
            indexedlog_cache: None,
            edenapi: None,
            cas_manager: None,
            format: None,
            max_fetch_count: MaxFetchCount::default(),
        }
    }

    pub fn max_fetch_count(mut self, max_fetch_count: MaxFetchCount) -> Self {
        self.max_fetch_count = max_fetch_count;
        self
    }

    pub fn local_path(mut self, path: impl AsRef<Path>) -> Self {
        self.local_path = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn suffix(mut self, suffix: impl AsRef<Path>) -> Self {
        self.suffix = Some(suffix.as_ref().to_path_buf());
        self
    }

    pub fn override_edenapi(mut self, use_edenapi: bool) -> Self {
        self.override_edenapi = Some(use_edenapi);
        self
    }

    pub fn edenapi(mut self, edenapi: Arc<SaplingRemoteApiFileStore>) -> Self {
        self.edenapi = Some(edenapi);
        self
    }

    pub fn cas_manager(mut self, cas_manager: Option<Arc<CasFetchManager>>) -> Self {
        self.cas_manager = cas_manager;
        self
    }

    pub fn indexedlog_cache(mut self, indexedlog: Arc<IndexedLogHgIdDataStore>) -> Self {
        self.indexedlog_cache = Some(indexedlog);
        self
    }

    pub fn indexedlog_local(mut self, indexedlog: Arc<IndexedLogHgIdDataStore>) -> Self {
        self.indexedlog_local = Some(indexedlog);
        self
    }

    pub fn format(mut self, format: SerializationFormat) -> Self {
        self.format = Some(format);
        self
    }

    #[context("unable to get LFS threshold")]
    fn get_lfs_threshold(&self) -> Result<Option<ByteCount>> {
        let enable_lfs = self.config.get_or_default::<bool>("remotefilelog", "lfs")?;
        let lfs_threshold = if enable_lfs {
            self.config.get_opt::<ByteCount>("lfs", "threshold")?
        } else {
            None
        };

        Ok(lfs_threshold)
    }

    fn get_edenapi_retries(&self) -> i32 {
        self.config
            .get_or_default::<i32>("scmstore", "retries")
            .unwrap_or_default()
    }

    fn get_format(&self) -> SerializationFormat {
        self.format.unwrap_or(SerializationFormat::Hg)
    }

    #[context("unable to determine whether use edenapi")]
    fn use_edenapi(&self) -> Result<bool> {
        Ok(if let Some(use_edenapi) = self.override_edenapi {
            use_edenapi
        } else {
            self.edenapi.is_some() || use_edenapi_via_config(self.config)?
        })
    }

    #[context("unable to determine whether to use lfs")]
    fn use_lfs(&self) -> Result<bool> {
        Ok(self.get_lfs_threshold()?.is_some())
    }

    #[context("unable to build edenapi")]
    fn build_edenapi(&self) -> Result<Arc<SaplingRemoteApiFileStore>> {
        let client = Builder::from_config(self.config)?.build()?;

        Ok(SaplingRemoteApiFileStore::new(client))
    }

    #[context("failed to build local indexedlog")]
    pub fn build_indexedlog_local(&self) -> Result<Option<Arc<IndexedLogHgIdDataStore>>> {
        Ok(if let Some(local_path) = self.local_path.clone() {
            let local_path = get_local_path(local_path, &self.suffix)?;
            let config = IndexedLogHgIdDataStoreConfig {
                max_log_count: None,
                max_bytes_per_log: None,
                max_bytes: None,
                btrfs_compression: self
                    .config
                    .get_or_default("indexedlog", "data.btrfs-compression")?,
            };
            Some(Arc::new(IndexedLogHgIdDataStore::new(
                self.config,
                get_indexedlogdatastore_path(local_path)?,
                &config,
                StoreType::Permanent,
                self.get_format(),
            )?))
        } else {
            None
        })
    }

    #[context("failed to build indexedlog cache")]
    pub fn build_indexedlog_cache(&self) -> Result<Option<Arc<IndexedLogHgIdDataStore>>> {
        let cache_path = match get_cache_path(self.config, &self.suffix)? {
            Some(p) => p,
            None => return Ok(None),
        };

        let max_log_count = self
            .config
            .get_opt::<u8>("indexedlog", "data.max-log-count")?;
        let max_bytes_per_log = self
            .config
            .get_opt::<ByteCount>("indexedlog", "data.max-bytes-per-log")?;
        let max_bytes = self
            .config
            .get_opt::<ByteCount>("remotefilelog", "cachelimit")?;
        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count,
            max_bytes_per_log,
            max_bytes,
            btrfs_compression: self
                .config
                .get_or_default("indexedlog", "data.btrfs-compression")?,
        };
        Ok(Some(Arc::new(IndexedLogHgIdDataStore::new(
            self.config,
            get_indexedlogdatastore_path(cache_path)?,
            &config,
            StoreType::Rotated,
            self.get_format(),
        )?)))
    }

    #[context("failed to build aux cache")]
    pub fn build_aux_cache(&self) -> Result<Option<Arc<AuxStore>>> {
        let cache_path = match get_cache_path(self.config, &self.suffix)? {
            Some(p) => p,
            None => return Ok(None),
        };

        let cache_path = get_indexedlogdatastore_aux_path(cache_path)?;
        Ok(Some(Arc::new(AuxStore::new(
            cache_path,
            self.config,
            StoreType::Rotated,
        )?)))
    }

    #[context("failed to build lfs local")]
    pub fn build_lfs_local(&self) -> Result<Option<Arc<LfsStore>>> {
        Ok(if let Some(local_path) = self.local_path.clone() {
            let local_path = get_local_path(local_path, &self.suffix)?;
            Some(Arc::new(LfsStore::permanent(local_path, self.config)?))
        } else {
            None
        })
    }

    #[context("failed to build lfs cache")]
    pub fn build_lfs_cache(&self) -> Result<Option<Arc<LfsStore>>> {
        let cache_path = match get_cache_path(self.config, &self.suffix)? {
            Some(p) => p,
            None => return Ok(None),
        };

        Ok(Some(Arc::new(LfsStore::rotated(cache_path, self.config)?)))
    }

    #[context("failed to build config revisionstore")]
    pub fn build(mut self) -> Result<FileStore> {
        tracing::trace!(target: "revisionstore::filestore", "checking cache");
        if let Some(cache_path) = get_cache_path(self.config, &self.suffix)? {
            check_cache_buster(&self.config, &cache_path);
        }

        tracing::trace!(target: "revisionstore::filestore", "processing lfs threshold");
        let lfs_threshold_bytes = self.get_lfs_threshold()?.map(|b| b.value());

        let edenapi_retries = self.get_edenapi_retries();

        let format: SerializationFormat = self.get_format();

        tracing::trace!(target: "revisionstore::filestore", "processing local");
        let indexedlog_local = if let Some(indexedlog_local) = self.indexedlog_local.take() {
            Some(indexedlog_local)
        } else {
            self.build_indexedlog_local()?
        };

        tracing::trace!(target: "revisionstore::filestore", "processing cache");
        let indexedlog_cache = if let Some(indexedlog_cache) = self.indexedlog_cache.take() {
            Some(indexedlog_cache)
        } else {
            self.build_indexedlog_cache()?
        };

        tracing::trace!(target: "revisionstore::filestore", "processing aux data");
        let aux_cache = self.build_aux_cache()?;

        let lfs_client = if self.use_lfs()? {
            if let Some(lfs_cache) = self.build_lfs_cache()? {
                Some(LfsClient::new(
                    lfs_cache,
                    self.build_lfs_local()?,
                    self.config,
                )?)
            } else {
                tracing::trace!(target: "revisionstore::filestore", "disabling lfs - no cache available");
                None
            }
        } else {
            tracing::trace!(target: "revisionstore::filestore", "lfs not in use");
            None
        };

        tracing::trace!(target: "revisionstore::filestore", "processing edenapi");
        let edenapi = if self.use_edenapi()? {
            if let Some(edenapi) = self.edenapi.take() {
                Some(edenapi)
            } else {
                Some(self.build_edenapi()?)
            }
        } else {
            None
        };

        let allow_write_lfs_ptrs = self
            .config
            .get_or_default::<bool>("scmstore", "lfsptrwrites")?;

        // Top level flag allow disabling all local computation of aux data.
        let compute_aux_data =
            self.config
                .get_or::<bool>("scmstore", "compute-aux-data", || true)?;

        let activity_logger =
            if let Some(path) = self.config.get_opt::<String>("scmstore", "activitylog")? {
                let f = fs_err::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(path)?;
                Some(Arc::new(Mutex::new(ActivityLogger::new(f))))
            } else {
                None
            };

        let verify_hash = !self.config.get_or("unsafe", "skip-verify-hash", || false)?;
        let cas_manager = configured_cas_manager(self.config, self.cas_manager, CasFetchKind::File);

        tracing::trace!(target: "revisionstore::filestore", "constructing FileStore");
        Ok(FileStore {
            lfs_threshold_bytes,
            verify_hash,
            edenapi_retries,
            allow_write_lfs_ptrs,

            compute_aux_data,

            indexedlog_local,
            indexedlog_cache,

            edenapi,
            cas_manager,
            lfs_client,

            activity_logger,
            metrics: FileStoreMetrics::new(),
            max_fetch_count: self.max_fetch_count,

            aux_cache,

            flush_on_drop: true,
            format,

            progress_bar: AggregatingProgressBar::new("fetching from ScmStore", "files"),

            lfs_buffer_in_memory: self
                .config
                .get_or_default("experimental", "lfs-buffer-in-memory")?,
        })
    }
}

pub struct TreeStoreBuilder<'a> {
    config: &'a dyn Config,
    local_path: Option<PathBuf>,
    suffix: Option<PathBuf>,
    override_edenapi: Option<bool>,

    indexedlog_local: Option<Arc<IndexedLogHgIdDataStore>>,
    indexedlog_cache: Option<Arc<IndexedLogHgIdDataStore>>,
    edenapi: Option<Arc<SaplingRemoteApiTreeStore>>,
    tree_aux_store: Option<Arc<TreeAuxStore>>,
    filestore: Option<Arc<FileStore>>,
    cas_manager: Option<Arc<CasFetchManager>>,
    format: Option<SerializationFormat>,
    permission_denied_paths: Option<types::errors::PermissionDeniedPaths>,
    max_fetch_count: MaxFetchCount,
}

impl<'a> TreeStoreBuilder<'a> {
    pub fn new(config: &'a dyn Config) -> Self {
        Self {
            config,
            local_path: None,
            suffix: None,
            override_edenapi: None,
            indexedlog_local: None,
            indexedlog_cache: None,
            edenapi: None,
            tree_aux_store: None,
            filestore: None,
            cas_manager: None,
            format: None,
            permission_denied_paths: None,
            max_fetch_count: MaxFetchCount::default(),
        }
    }

    pub fn max_fetch_count(mut self, max_fetch_count: MaxFetchCount) -> Self {
        self.max_fetch_count = max_fetch_count;
        self
    }

    pub fn permission_denied_paths(mut self, paths: types::errors::PermissionDeniedPaths) -> Self {
        self.permission_denied_paths = Some(paths);
        self
    }

    pub fn local_path(mut self, path: impl AsRef<Path>) -> Self {
        self.local_path = Some(path.as_ref().to_path_buf());
        self
    }

    // TODO(meyer): Can we remove this since we have separate builders for files and trees?
    // Is this configurable somewhere we can directly check from Config instead of having the
    // caller pass in, or is it just hardcoded elsewhere and we should hardcode it here?
    /// Cache path suffix for the associated indexedlog. For files, this will not be given.
    /// For trees, it will be "manifests".
    pub fn suffix(mut self, suffix: impl AsRef<Path>) -> Self {
        self.suffix = Some(suffix.as_ref().to_path_buf());
        self
    }

    pub fn edenapi(mut self, edenapi: Arc<SaplingRemoteApiTreeStore>) -> Self {
        self.edenapi = Some(edenapi);
        self
    }

    pub fn indexedlog_cache(mut self, indexedlog: Arc<IndexedLogHgIdDataStore>) -> Self {
        self.indexedlog_cache = Some(indexedlog);
        self
    }

    pub fn indexedlog_local(mut self, indexedlog: Arc<IndexedLogHgIdDataStore>) -> Self {
        self.indexedlog_local = Some(indexedlog);
        self
    }

    pub fn override_edenapi(mut self, use_edenapi: bool) -> Self {
        self.override_edenapi = Some(use_edenapi);
        self
    }

    pub fn filestore(mut self, filestore: Arc<FileStore>) -> Self {
        self.filestore = Some(filestore);
        self
    }

    pub fn cas_manager(mut self, cas_manager: Option<Arc<CasFetchManager>>) -> Self {
        self.cas_manager = cas_manager;
        self
    }

    pub fn tree_aux_store(mut self, tree_aux_store: Arc<TreeAuxStore>) -> Self {
        self.tree_aux_store = Some(tree_aux_store);
        self
    }

    pub fn format(mut self, format: SerializationFormat) -> Self {
        self.format = Some(format);
        self
    }

    #[context("failed to determine whether to use edenapi")]
    fn use_edenapi(&self) -> Result<bool> {
        Ok(if let Some(use_edenapi) = self.override_edenapi {
            use_edenapi
        } else {
            self.edenapi.is_some() || use_edenapi_via_config(self.config)?
        })
    }

    #[context("failed to build SaplingRemoteAPI from config")]
    fn build_edenapi(&self) -> Result<Arc<SaplingRemoteApiTreeStore>> {
        let client = Builder::from_config(self.config)?.build()?;

        Ok(SaplingRemoteApiTreeStore::new(client))
    }

    fn get_format(&self) -> SerializationFormat {
        self.format.unwrap_or(SerializationFormat::Hg)
    }

    #[context("failed to build local indexedlog")]
    pub fn build_indexedlog_local(&self) -> Result<Option<Arc<IndexedLogHgIdDataStore>>> {
        Ok(if let Some(local_path) = self.local_path.clone() {
            let local_path = get_local_path(local_path, &self.suffix)?;
            let config = IndexedLogHgIdDataStoreConfig {
                max_log_count: None,
                max_bytes_per_log: None,
                max_bytes: None,
                btrfs_compression: self
                    .config
                    .get_or_default("indexedlog", "manifest.btrfs-compression")?,
            };
            Some(Arc::new(IndexedLogHgIdDataStore::new(
                self.config,
                get_indexedlogdatastore_path(local_path)?,
                &config,
                StoreType::Permanent,
                self.get_format(),
            )?))
        } else {
            None
        })
    }

    #[context("failed to build indexedlog cache")]
    pub fn build_indexedlog_cache(&self) -> Result<Option<Arc<IndexedLogHgIdDataStore>>> {
        let cache_path = match get_cache_path(self.config, &self.suffix)? {
            Some(p) => p,
            None => return Ok(None),
        };

        let max_log_count = self
            .config
            .get_opt::<u8>("indexedlog", "manifest.max-log-count")?;
        let max_bytes_per_log = self
            .config
            .get_opt::<ByteCount>("indexedlog", "manifest.max-bytes-per-log")?;
        let max_bytes = self
            .config
            .get_opt::<ByteCount>("remotefilelog", "manifestlimit")?;
        let config = IndexedLogHgIdDataStoreConfig {
            max_log_count,
            max_bytes_per_log,
            max_bytes,
            btrfs_compression: self
                .config
                .get_or_default("indexedlog", "manifest.btrfs-compression")?,
        };

        Ok(Some(Arc::new(IndexedLogHgIdDataStore::new(
            self.config,
            get_indexedlogdatastore_path(cache_path)?,
            &config,
            StoreType::Rotated,
            self.get_format(),
        )?)))
    }

    #[context("failed to build tree aux store")]
    pub fn build_tree_aux_store(&self) -> Result<Option<Arc<TreeAuxStore>>> {
        let path = if self
            .config
            .get_or_default("scmstore", "store-tree-aux-in-shared-cache")?
        {
            // This knob is just for testing convenience so blowing away the cache dir
            // will also blow away the tree aux cache.
            get_cache_path(self.config, &self.suffix)
        } else {
            // The TreeAuxStore is a mapping from HgId to augmented
            // manifest digest, and is used to convert from Hg tree
            // ids in order to make augmented manifest lookups.
            //
            // It is technically a cache, however we do not want to put
            // it in the shared cache directory to avoid the risk of
            // poisoning by other shared cache users.  As such, we
            // create it as a rotated log, but in the local store.
            self.local_path
                .clone()
                .map(|path| get_local_path(path, &self.suffix))
                .transpose()
        };

        if let Some(path) = path? {
            Ok(Some(Arc::new(TreeAuxStore::new(
                self.config,
                get_tree_aux_store_path(path)?,
                StoreType::Rotated,
            )?)))
        } else {
            Ok(None)
        }
    }

    #[context("failed to build local history")]
    pub fn build_historystore_local(&self) -> Result<Option<Arc<IndexedLogHgIdHistoryStore>>> {
        Ok(if let Some(local_path) = &self.local_path {
            Some(Arc::new(IndexedLogHgIdHistoryStore::new(
                get_indexedloghistorystore_path(local_path.join("manifests"))?,
                self.config,
                StoreType::Permanent,
            )?))
        } else {
            None
        })
    }

    #[context("failed to build shared history")]
    pub fn build_historystore_cache(&self) -> Result<Option<Arc<IndexedLogHgIdHistoryStore>>> {
        let cache_path = match get_cache_path(self.config, &Some("manifests"))? {
            Some(p) => p,
            None => return Ok(None),
        };

        Ok(Some(Arc::new(IndexedLogHgIdHistoryStore::new(
            get_indexedloghistorystore_path(cache_path)?,
            self.config,
            StoreType::Rotated,
        )?)))
    }

    #[context("failed to build revision store")]
    pub fn build(mut self) -> Result<TreeStore> {
        // TODO(meyer): Clean this up, just copied and pasted from the other version & did some ugly hacks to get this
        // (the SaplingRemoteApiAdapter stuff needs to be fixed in particular)
        tracing::trace!(target: "revisionstore::treestore", "checking cache");
        if let Some(cache_path) = get_cache_path(self.config, &self.suffix)? {
            check_cache_buster(&self.config, &cache_path);
        }

        let format = self.get_format();

        tracing::trace!(target: "revisionstore::treestore", "processing local");
        let indexedlog_local = if let Some(indexedlog_local) = self.indexedlog_local.take() {
            Some(indexedlog_local)
        } else {
            self.build_indexedlog_local()?
        };

        tracing::trace!(target: "revisionstore::treestore", "processing cache");
        let indexedlog_cache = if let Some(indexedlog_cache) = self.indexedlog_cache.take() {
            Some(indexedlog_cache)
        } else {
            self.build_indexedlog_cache()?
        };

        tracing::trace!(target: "revisionstore::treestore", "processing tree_aux_store");
        let tree_aux_store = if self
            .config
            .get_or("scmstore", "store-tree-aux-data", || true)?
        {
            if let Some(tree_aux_store) = self.tree_aux_store.take() {
                Some(tree_aux_store)
            } else {
                self.build_tree_aux_store()?
            }
        } else {
            None
        };

        tracing::trace!(target: "revisionstore::treestore", "processing edenapi");
        let edenapi = if self.use_edenapi()? {
            if let Some(edenapi) = self.edenapi.take() {
                Some(edenapi)
            } else {
                Some(self.build_edenapi()?)
            }
        } else {
            None
        };

        tracing::trace!(target: "revisionstore::treestore", "processing historystore");
        let (historystore_local, historystore_cache) = (
            self.build_historystore_local()?,
            self.build_historystore_cache()?,
        );

        let prefetch_tree_parents = self
            .config
            .get_or_default("scmstore", "prefetch-tree-parents")?;

        let tree_metadata_mode = match self.config.get("scmstore", "tree-metadata-mode").as_deref()
        {
            Some("always") => TreeMetadataMode::Always,
            None | Some("opt-in") => TreeMetadataMode::OptIn,
            _ => TreeMetadataMode::Never,
        };

        let fetch_tree_aux_data = self
            .config
            .get_or_default::<bool>("scmstore", "fetch-tree-aux-data")?;

        if fetch_tree_aux_data && tree_aux_store.is_none() {
            tracing::warn!(
                "fetch-tree-aux-data is set, but store-tree-aux-data is not set resulting in no tree aux data locally cached"
            );
        }

        let verify_hash = !self.config.get_or("unsafe", "skip-verify-hash", || false)?;
        let cas_manager = configured_cas_manager(self.config, self.cas_manager, CasFetchKind::Tree);

        tracing::trace!(target: "revisionstore::treestore", "constructing TreeStore");
        Ok(TreeStore {
            indexedlog_local,
            indexedlog_cache,
            cache_to_local_cache: true,
            edenapi,
            cas_manager,
            tree_aux_store,
            historystore_local,
            historystore_cache,
            prefetch_tree_parents,
            filestore: self.filestore,
            tree_metadata_mode,
            fetch_tree_aux_data,
            flush_on_drop: true,
            format,
            progress_bar: AggregatingProgressBar::new("fetching from ScmStore", "trees"),
            verify_hash,
            acl_check_cache: new_acl_check_cache(),
            permission_denied_paths: self.permission_denied_paths,
            max_fetch_count: self.max_fetch_count,
        })
    }
}

#[derive(Clone, Copy)]
enum CasFetchKind {
    File,
    Tree,
}

impl CasFetchKind {
    fn enabled(self, config: &dyn Config) -> bool {
        match config.get("scmstore", "cas-mode").as_deref() {
            Some("on") => true,
            Some("files") => matches!(self, Self::File),
            Some("trees") => matches!(self, Self::Tree),
            None | Some("off") => false,
            Some(mode) => {
                tracing::warn!(
                    target: "cas_client",
                    mode,
                    "unsupported scmstore.cas-mode; CAS fetching is disabled"
                );
                false
            }
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Tree => "tree",
        }
    }
}

fn configured_cas_manager(
    config: &dyn Config,
    cas_manager: Option<Arc<CasFetchManager>>,
    fetch_kind: CasFetchKind,
) -> Option<Arc<CasFetchManager>> {
    if !fetch_kind.enabled(config) {
        return None;
    }

    let Some(cas_manager) = cas_manager else {
        tracing::warn!(
            target: "cas_client",
            kind = fetch_kind.name(),
            "CAS fetching was requested but no CAS client is registered"
        );
        return None;
    };

    if let Err(error) = cas_manager.init() {
        tracing::warn!(
            target: "cas_client",
            ?error,
            kind = fetch_kind.name(),
            "failed to initialize CAS client; CAS fetching is disabled"
        );
        return None;
    }

    Some(cas_manager)
}

#[context("failed to get edenapi via config")]
fn use_edenapi_via_config(config: &dyn Config) -> Result<bool> {
    let mut use_edenapi: bool = config.get_or_default("remotefilelog", "http")?;
    if use_edenapi {
        // If paths.default is not set, there is no server repo. Therefore, do
        // not use edenapi.
        let path: Option<String> = config.get_opt("paths", "default")?;
        if path.is_none() {
            use_edenapi = false;
        }
    }
    Ok(use_edenapi)
}

/// Reads the configs and deletes the hgcache if a hgcache-purge.$KEY=$DATE value hasn't already
/// been processed.
pub fn check_cache_buster(config: &dyn Config, store_path: &Path) {
    for key in config.keys("hgcache-purge").into_iter() {
        if let Some(cutoff) = config
            .get("hgcache-purge", &key)
            .and_then(|c| HgTime::parse(&c))
        {
            if check_run_once(store_path, &key, cutoff) {
                let _ = delete_hgcache(store_path);
                break;
            }
        }
    }
}

/// Recursively deletes the contents of the path, excluding the run-once marker file.
/// Ignores errors on individual files or directories.
fn delete_hgcache(store_path: &Path) -> Result<()> {
    for file in fs::read_dir(store_path)? {
        let _ = (|| -> Result<()> {
            let file = file?;
            if file.file_name() == RUN_ONCE_FILENAME {
                return Ok(());
            }

            let path = file.path();
            let file_type = file.file_type()?;
            if file_type.is_dir() {
                fs::remove_dir_all(path)?;
            } else if file_type.is_file() || file_type.is_symlink() {
                fs::remove_file(path)?;
            }
            Ok(())
        })();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn cas_mode_selects_file_and_tree_fetching() {
        let cases = [
            (None, false, false),
            (Some("off"), false, false),
            (Some("files"), true, false),
            (Some("trees"), false, true),
            (Some("on"), true, true),
        ];

        for (mode, files_enabled, trees_enabled) in cases {
            let config = mode
                .map(|mode| BTreeMap::from([("scmstore.cas-mode", mode)]))
                .unwrap_or_default();
            assert_eq!(CasFetchKind::File.enabled(&config), files_enabled);
            assert_eq!(CasFetchKind::Tree.enabled(&config), trees_enabled);
        }
    }
}
