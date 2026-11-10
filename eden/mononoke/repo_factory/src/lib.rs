/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Repository factory.
#![feature(trait_alias)]

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::sync::Arc;
#[cfg(fbcode_build)]
use std::sync::LazyLock;

#[cfg(fbcode_build)]
use MononokeRepoFactoryStats_ods3::Instrument_MononokeRepoFactoryStats;
#[cfg(fbcode_build)]
use MononokeRepoFactoryStats_ods3_types::MononokeRepoFactoryStats;
use acl_regions::ArcAclRegions;
use acl_regions::build_acl_regions;
use anyhow::Context;
use anyhow::Result;
#[cfg(fbcode_build)]
use anyhow::anyhow;
use async_once_cell::AsyncOnceCell;
use blobstore::Blobstore;
use blobstore::BlobstoreEnumerableWithUnlink;
pub use blobstore_factory::BlobstoreOptions;
use blobstore_factory::ComponentSamplingHandler;
use blobstore_factory::MetadataSqlFactory;
pub use blobstore_factory::ReadOnlyStorage;
use blobstore_factory::ScrubHandler;
use blobstore_factory::default_scrub_handler;
use blobstore_factory::make_blobstore;
use blobstore_factory::make_blobstore_enumerable_with_unlink;
use bonsai_blob_mapping::ArcBonsaiBlobMapping;
use bonsai_blob_mapping::BonsaiBlobMapping;
use bonsai_blob_mapping::SqlBonsaiBlobMapping;
use bonsai_git_mapping::ArcBonsaiGitMapping;
use bonsai_git_mapping::CachingBonsaiGitMapping;
use bonsai_git_mapping::SqlBonsaiGitMappingBuilder;
use bonsai_globalrev_mapping::ArcBonsaiGlobalrevMapping;
use bonsai_globalrev_mapping::CachingBonsaiGlobalrevMapping;
use bonsai_globalrev_mapping::SqlBonsaiGlobalrevMappingBuilder;
use bonsai_hg_mapping::ArcBonsaiHgMapping;
use bonsai_hg_mapping::CachingBonsaiHgMapping;
use bonsai_hg_mapping::SqlBonsaiHgMappingBuilder;
use bonsai_p4_mapping::ArcBonsaiP4Mapping;
use bonsai_p4_mapping::CachingBonsaiP4Mapping;
use bonsai_p4_mapping::SqlBonsaiP4MappingBuilder;
use bonsai_svnrev_mapping::ArcBonsaiSvnrevMapping;
use bonsai_svnrev_mapping::CachingBonsaiSvnrevMapping;
use bonsai_svnrev_mapping::SqlBonsaiSvnrevMappingBuilder;
use bonsai_tag_mapping::ArcBonsaiTagMapping;
use bonsai_tag_mapping::CachedBonsaiTagMapping;
use bonsai_tag_mapping::SqlBonsaiTagMappingBuilder;
use bookmark_service_client::BookmarkServiceClient;
use bookmark_service_client::RepoBookmarkServiceClient;
use bookmarks::ArcBookmarkUpdateLog;
use bookmarks::ArcBookmarks;
use bookmarks::CachedBookmarks;
use bookmarks::bookmark_heads_fetcher;
use bookmarks_cache::ArcBookmarksCache;
use bookmarks_cache::ArcScopedBookmarksCache;
use bookmarks_cache::CombinedBookmarksCache;
use bookmarks_cache::WarmerRequirement;
use bundle_uri::ArcGitBundleUri;
use bundle_uri::SqlGitBundleMetadataStorageBuilder;
use cacheblob::CachelibBlobstoreOptions;
use cacheblob::new_cachelib_blobstore_no_lease;
use cacheblob::new_memcache_blobstore;
use caching_commit_graph_storage::CachingCommitGraphStorage;
use caching_ext::CacheHandlerEncoding;
use caching_ext::CacheHandlerFactory;
use clientinfo::ClientEntryPoint;
use clientinfo::ClientInfo;
use commit_cloud::ArcCommitCloud;
use commit_cloud::CommitCloud;
use commit_cloud::sql::builder::SqlCommitCloudBuilder;
use commit_derived_data_mapping::ArcCommitDerivedDataMapping;
use commit_derived_data_mapping::CommitDerivedDataMapping;
use commit_derived_data_mapping::SqlCommitDerivedDataMapping;
use commit_graph::ArcCommitGraph;
use commit_graph::ArcCommitGraphWriter;
use commit_graph::BaseCommitGraphWriter;
use commit_graph::CommitGraph;
use commit_graph::LoggingCommitGraphWriter;
use commit_graph_types::storage::CommitGraphStorage;
use commit_rate_limit_config::ArcCommitRateLimit;
use commit_rate_limit_config::CommitRateLimit;
use commit_rate_limit_config::build_commit_rate_limit;
use context::CoreContext;
use context::SessionContainer;
use cross_repo_sync::create_commit_syncer_lease;
use dbbookmarks::ArcSqlBookmarks;
use dbbookmarks::SqlBookmarksBuilder;
use deletion_log::ArcDeletionLog;
use deletion_log::DeletionLog;
use deletion_log::SqlDeletionLog;
#[cfg(fbcode_build)]
use derived_data_client_library::Client as DerivationServiceClient;
use derived_data_remote::DerivationClient;
use derived_data_remote::RemoteDerivationOptions;
use enabled_derived_data_types::ArcEnabledDerivedDataTypes;
use enabled_derived_data_types::SqlEnabledDerivedDataTypesBuilder;
use environment::BookmarkCacheAddress;
use environment::BookmarkCacheDerivedData;
use environment::BookmarkCacheKind;
use environment::BookmarkCacheOptions;
use environment::Caching;
use environment::LocalCacheEncoding;
use environment::MononokeEnvironment;
use ephemeral_blobstore::ArcRepoEphemeralStore;
use ephemeral_blobstore::RepoEphemeralStore;
use ephemeral_blobstore::RepoEphemeralStoreBuilder;
use fbinit::FacebookInit;
use filenodes::ArcFilenodes;
use filestore::ArcFilestoreConfig;
use filestore::FilestoreConfig;
use futures_watchdog::WatchdogExt;
use git_ref_content_mapping::ArcGitRefContentMapping;
use git_ref_content_mapping::CachedGitRefContentMapping;
use git_ref_content_mapping::SqlGitRefContentMappingBuilder;
use git_source_of_truth::ArcGitSourceOfTruthConfig;
use git_source_of_truth::SqlGitSourceOfTruthConfigBuilder;
use git_symbolic_refs::ArcGitSymbolicRefs;
use git_symbolic_refs::CachedGitSymbolicRefs;
use git_symbolic_refs::SqlGitSymbolicRefsBuilder;
use git_types::ArcCgdmChangesetDivider;
use git_types::CGDMComponentsReloader;
use git_types::DummyCgdmChangesetDivider;
use hook_manager::HookRepo;
use hook_manager::manager::ArcHookManager;
use hook_manager::manager::HookManager;
use hooks::hook_loader::load_hooks;
use live_commit_sync_config::CfgrLiveCommitSyncConfig;
use memcache::KeyGen;
use memcache::MemcacheClient;
use mercurial_mutation::ArcHgMutationStore;
use mercurial_mutation::CachedHgMutationStore;
use mercurial_mutation::SqlHgMutationStoreBuilder;
use metaconfig_types::ArcCommonConfig;
use metaconfig_types::ArcRepoConfig;
use metaconfig_types::ArcRepoConfigSource;
use metaconfig_types::BlobConfig;
use metaconfig_types::CommitIdentityScheme;
use metaconfig_types::CommonConfig;
use metaconfig_types::MetadataCacheConfig;
use metaconfig_types::MetadataDatabaseConfig;
use metaconfig_types::Redaction;
#[cfg(fbcode_build)]
use metaconfig_types::RemoteDerivationConfig;
use metaconfig_types::RepoConfig;
use metaconfig_types::RepoConfigSource;
use metaconfig_types::RepoReadOnly;
#[cfg(fbcode_build)]
use metaconfig_types::ZelosConfig;
use mononoke_configs::MononokeConfigs;
use mutable_blobstore::ArcMutableRepoBlobstore;
use mutable_blobstore::MutableRepoBlobstore;
use mutable_counters::ArcMutableCounters;
use mutable_counters::SqlMutableCountersBuilder;
use mutable_renames::ArcMutableRenames;
use mutable_renames::MutableRenames;
use mutable_renames::SqlMutableRenamesStore;
use newfilenodes::NewFilenodesBuilder;
use parking_lot::Mutex;
use permission_checker::AclProvider;
use phases::ArcPhases;
use preloaded_commit_graph_storage::PreloadedCommitGraphStorage;
use pushrebase_mutation_mapping::ArcPushrebaseMutationMapping;
use pushrebase_mutation_mapping::SqlPushrebaseMutationMappingConnection;
use pushredirect::ArcPushRedirectionConfig;
use pushredirect::SqlPushRedirectionConfigBuilder;
use readonlyblob::ReadOnlyBlobstore;
use redactedblobstore::ArcRedactionConfigBlobstore;
use redactedblobstore::RedactedBlobs;
use redactedblobstore::RedactionConfigBlobstore;
use repo_blobstore::ArcRepoBlobstore;
use repo_blobstore::RepoBlobstore;
use repo_bookmark_attrs::ArcRepoBookmarkAttrs;
use repo_bookmark_attrs::RepoBookmarkAttrs;
use repo_cross_repo::ArcRepoCrossRepo;
use repo_cross_repo::RepoCrossRepo;
use repo_derivation_queues::ArcRepoDerivationQueues;
use repo_derived_data::ArcRepoDerivedData;
use repo_derived_data::RepoDerivedData;
use repo_event_publisher::ArcRepoEventPublisher;
#[cfg(fbcode_build)]
use repo_event_publisher::ScribeRepoEventPublisher;
#[cfg(not(fbcode_build))]
use repo_event_publisher::UnsupportedRepoEventPublisher;
use repo_identity::ArcRepoIdentity;
use repo_identity::RepoIdentity;
use repo_lock::AlwaysLockedRepoLock;
use repo_lock::ArcRepoLock;
use repo_lock::MutableRepoLock;
use repo_lock::SqlRepoLock;
use repo_manifest_mapping::ArcRepoManifestMapping;
use repo_manifest_mapping::SqlRepoManifestMappingBuilder;
use repo_manifest_mapping::UnconfiguredRepoManifestMapping;
use repo_metadata_checkpoint::ArcRepoMetadataCheckpoint;
use repo_metadata_checkpoint::SqlRepoMetadataCheckpointBuilder;
use repo_permission_checker::ArcRepoPermissionChecker;
use repo_permission_checker::ProdRepoPermissionChecker;
use repo_sparse_profiles::ArcRepoSparseProfiles;
use repo_sparse_profiles::RepoSparseProfiles;
use repo_sparse_profiles::SqlSparseProfilesSizes;
use repo_stats_logger::ArcRepoStatsLogger;
use repo_stats_logger::RepoStatsLogger;
use restricted_paths::ArcRestrictedPaths;
use restricted_paths::ArcRestrictedPathsConfigBased;
use restricted_paths::ArcRestrictedPathsManifestIdStore;
use restricted_paths::RestrictedPaths;
use restricted_paths::RestrictedPathsConfigBased;
use restricted_paths::RestrictedPathsManifestIdCacheBuilder;
use restricted_paths::SqlRestrictedPathsManifestIdStoreBuilder;
use scuba_ext::MononokeScubaSampleBuilder;
use sql_commit_graph_storage::ArcCommitGraphBulkFetcher;
use sql_commit_graph_storage::CommitGraphBulkFetcher;
use sql_commit_graph_storage::SqlCommitGraphStorageBuilder;
use sql_construct::SqlConstructFromDatabaseConfig;
use sql_construct::SqlConstructFromMetadataDatabaseConfig;
use sql_construct::SqlShardableConstructFromMetadataDatabaseConfig;
use sql_query_config::ArcSqlQueryConfig;
use sql_query_config::SqlQueryConfig;
use sqlphases::SqlPhasesBuilder;
use stats::prelude::*;
use streaming_clone::ArcStreamingClone;
use streaming_clone::StreamingCloneBuilder;
use synced_commit_mapping::ArcSyncedCommitMapping;
use synced_commit_mapping::CachingSyncedCommitMapping;
use synced_commit_mapping::SqlSyncedCommitMappingBuilder;
use thiserror::Error;
use tracing::error;
use virtually_sharded_blobstore::VirtuallyShardedBlobstore;
use warm_bookmarks_cache::NoopBookmarksCache;
use warm_bookmarks_cache::WarmBookmarksCacheBuilder;
use wireproto_handler::ArcPushRedirectorMode;
use wireproto_handler::ArcRepoHandlerBase;
use wireproto_handler::ArcTargetRepoDbs;
use wireproto_handler::PushRedirectorBase;
use wireproto_handler::PushRedirectorMode;
use wireproto_handler::PushRedirectorMode::Enabled;
use wireproto_handler::RepoHandlerBase;
use wireproto_handler::TargetRepoDbs;
#[cfg(fbcode_build)]
use zelos_queue::zelos_derivation_queues;
#[cfg(fbcode_build)]
use zeus_client::ZeusClient;
#[cfg(fbcode_build)]
use zeus_client::zeus_cpp_client::ZeusCppClient;

#[cfg(fbcode_build)]
const ZEUS_CLIENT_ID: &str = "mononoke";

#[cfg(fbcode_build)]
static REPO_FACTORY_INSTRUMENT: LazyLock<Instrument_MononokeRepoFactoryStats> =
    LazyLock::new(Instrument_MononokeRepoFactoryStats::new);

define_stats! {
    prefix = "mononoke.repo_factory";
    cache_hit: dynamic_singleton_counter(
        "cache.{}.hit",
        (cache_name: String)
    ),
    cache_init_error: dynamic_singleton_counter(
        "cache.{}.init.error",
        (cache_name: String)
    ),
    cache_miss: dynamic_singleton_counter(
        "cache.{}.miss",
        (cache_name: String)
    ),
}

#[derive(Clone)]
struct RepoFactoryCache<K: Clone + Eq + Hash, V: Clone> {
    fb: FacebookInit,
    name: String,
    cache: Arc<Mutex<HashMap<K, Arc<AsyncOnceCell<V>>>>>,
}

impl<K: Clone + Eq + Hash, V: Clone> RepoFactoryCache<K, V> {
    fn new(fb: FacebookInit, name: &str) -> Self {
        RepoFactoryCache {
            fb,
            name: name.to_string(),
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn get_or_try_init<F, Fut>(&self, key: &K, init: F) -> Result<V>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V>>,
    {
        let cell = {
            let mut cache = self.cache.lock();
            match cache.get(key) {
                Some(cell) => {
                    if let Some(value) = cell.get() {
                        STATS::cache_hit.increment_value(self.fb, 1, (self.name.clone(),));
                        #[cfg(fbcode_build)]
                        REPO_FACTORY_INSTRUMENT.observe(MononokeRepoFactoryStats {
                            cache_name: Some(self.name.clone()),
                            hits: Some(1.0),
                            ..Default::default()
                        });
                        return Ok(value.clone());
                    }
                    cell.clone()
                }
                None => {
                    STATS::cache_miss.increment_value(self.fb, 1, (self.name.clone(),));
                    #[cfg(fbcode_build)]
                    REPO_FACTORY_INSTRUMENT.observe(MononokeRepoFactoryStats {
                        cache_name: Some(self.name.clone()),
                        misses: Some(1.0),
                        ..Default::default()
                    });
                    let cell = Arc::new(AsyncOnceCell::new());
                    cache.insert(key.clone(), cell.clone());
                    cell
                }
            }
        };
        match cell.get_or_try_init(init).await {
            Ok(value) => Ok(value.clone()),
            Err(e) => {
                STATS::cache_init_error.increment_value(self.fb, 1, (self.name.clone(),));
                #[cfg(fbcode_build)]
                REPO_FACTORY_INSTRUMENT.observe(MononokeRepoFactoryStats {
                    cache_name: Some(self.name.clone()),
                    init_errors: Some(1.0),
                    ..Default::default()
                });
                Err(e)
            }
        }
    }
}

pub trait RepoFactoryOverride<T> = Fn(T) -> T + Send + Sync + 'static;

#[derive(Clone)]
pub struct RepoFactory {
    pub env: Arc<MononokeEnvironment>,
    #[cfg_attr(
        not(fbcode_build),
        expect(
            dead_code,
            reason = "Only the internal repo event publisher reads the configs"
        )
    )]
    configs: Arc<MononokeConfigs>,
    sql_factories: RepoFactoryCache<MetadataDatabaseConfig, Arc<MetadataSqlFactory>>,
    blobstores: RepoFactoryCache<BlobConfig, Arc<dyn Blobstore>>,
    redacted_blobs: RepoFactoryCache<MetadataDatabaseConfig, Arc<RedactedBlobs>>,
    repo_manifest_mappings: RepoFactoryCache<MetadataDatabaseConfig, ArcRepoManifestMapping>,
    #[cfg(fbcode_build)]
    zelos_clients: RepoFactoryCache<ZelosConfig, Arc<dyn ZeusClient>>,
    repo_event_publishers: RepoFactoryCache<Option<MetadataCacheConfig>, ArcRepoEventPublisher>,
    blobstore_override: Option<Arc<dyn RepoFactoryOverride<Arc<dyn Blobstore>>>>,
    scrub_handler: Arc<dyn ScrubHandler>,
    blobstore_component_sampler: Option<Arc<dyn ComponentSamplingHandler>>,
    bonsai_hg_mapping_overwrite: bool,
    derived_data_scuba_table_overwrite: Option<String>,
}

impl RepoFactory {
    pub fn new(env: Arc<MononokeEnvironment>, configs: Arc<MononokeConfigs>) -> RepoFactory {
        RepoFactory {
            sql_factories: RepoFactoryCache::new(env.fb, "sql_factories"),
            blobstores: RepoFactoryCache::new(env.fb, "blobstore"),
            redacted_blobs: RepoFactoryCache::new(env.fb, "redacted_blobs"),
            repo_manifest_mappings: RepoFactoryCache::new(env.fb, "repo_manifest_mappings"),
            #[cfg(fbcode_build)]
            zelos_clients: RepoFactoryCache::new(env.fb, "zelos_clients"),
            repo_event_publishers: RepoFactoryCache::new(env.fb, "repo_event_publishers"),
            blobstore_override: None,
            scrub_handler: default_scrub_handler(),
            blobstore_component_sampler: None,
            bonsai_hg_mapping_overwrite: false,
            configs,
            env,
            derived_data_scuba_table_overwrite: None,
        }
    }

    pub fn with_blobstore_override(
        &mut self,
        blobstore_override: impl RepoFactoryOverride<Arc<dyn Blobstore>>,
    ) -> &mut Self {
        self.blobstore_override = Some(Arc::new(blobstore_override));
        self
    }

    pub fn with_scrub_handler(&mut self, scrub_handler: Arc<dyn ScrubHandler>) -> &mut Self {
        self.scrub_handler = scrub_handler;
        self
    }

    pub fn with_blobstore_component_sampler(
        &mut self,
        handler: Arc<dyn ComponentSamplingHandler>,
    ) -> &mut Self {
        self.blobstore_component_sampler = Some(handler);
        self
    }

    pub fn with_bonsai_hg_mapping_override(&mut self) -> &mut Self {
        self.bonsai_hg_mapping_overwrite = true;
        self
    }

    pub fn with_derived_data_scuba_table(&mut self, scuba_table: String) -> &mut Self {
        self.derived_data_scuba_table_overwrite = Some(scuba_table);
        self
    }

    pub async fn sql_factory(
        &self,
        config: &MetadataDatabaseConfig,
    ) -> Result<Arc<MetadataSqlFactory>> {
        self.sql_factories
            .get_or_try_init(config, || async move {
                let sql_factory = MetadataSqlFactory::new(
                    self.env.fb,
                    config.clone(),
                    self.env.mysql_options.clone(),
                    self.env.readonly_storage,
                )
                .watched()
                .await
                .inspect_err(|e| {
                    error!(
                        "initializing DB connection failed for config: {:?}: {}",
                        config, e
                    )
                })
                .context("initializing DB connection")?;

                Ok(Arc::new(sql_factory))
            })
            .await
    }

    async fn open_sql<T: SqlConstructFromMetadataDatabaseConfig>(
        &self,
        config: &RepoConfig,
    ) -> Result<T> {
        let sql_factory = self.sql_factory(&config.storage_config.metadata).await?;
        sql_factory.open::<T>().await
    }

    async fn open_sql_shardable<T: SqlShardableConstructFromMetadataDatabaseConfig>(
        &self,
        config: &RepoConfig,
    ) -> Result<(Arc<MetadataSqlFactory>, T)> {
        let sql_factory = self.sql_factory(&config.storage_config.metadata).await?;
        let db = sql_factory.open_shardable::<T>().await?;
        Ok((sql_factory, db))
    }

    pub async fn blobstore_no_cache(&self, config: &BlobConfig) -> Result<Arc<dyn Blobstore>> {
        make_blobstore(
            self.env.fb,
            config.clone(),
            &self.env.mysql_options,
            self.env.readonly_storage,
            &self.env.blobstore_options,
            &self.env.config_store,
            &self.scrub_handler,
            self.blobstore_component_sampler.as_ref(),
            None,
        )
        .watched()
        .await
    }

    async fn repo_blobstore_from_blobstore(
        &self,
        repo_identity: &ArcRepoIdentity,
        repo_config: &ArcRepoConfig,
        blobstore: &Arc<dyn Blobstore>,
        common_config: &ArcCommonConfig,
    ) -> Result<RepoBlobstore> {
        let mut blobstore = blobstore.clone();
        if self.env.readonly_storage.0 {
            blobstore = Arc::new(ReadOnlyBlobstore::new(blobstore));
        }

        let redacted_blobs = match repo_config.redaction {
            Redaction::Enabled => {
                let redacted_blobs = self
                    .redacted_blobs(
                        self.ctx(),
                        &repo_config.storage_config.metadata,
                        common_config,
                    )
                    .await?;
                Some(redacted_blobs)
            }
            Redaction::Disabled => None,
        };

        let censored_scuba_builder = self.censored_scuba_builder(common_config)?;

        let repo_blobstore = RepoBlobstore::new(
            blobstore,
            redacted_blobs,
            repo_identity.id(),
            censored_scuba_builder,
        );

        Ok(repo_blobstore)
    }

    async fn mutable_repo_blobstore_from_blobstore(
        &self,
        repo_identity: &ArcRepoIdentity,
        blobstore: &Arc<dyn Blobstore>,
    ) -> Result<MutableRepoBlobstore> {
        let mut blobstore = blobstore.clone();
        if self.env.readonly_storage.0 {
            blobstore = Arc::new(ReadOnlyBlobstore::new(blobstore));
        }

        let repo_blobstore = MutableRepoBlobstore::new(blobstore, repo_identity.id());

        Ok(repo_blobstore)
    }
    async fn blobstore_enumerable_with_unlink(
        &self,
        config: &BlobConfig,
    ) -> Result<Arc<dyn BlobstoreEnumerableWithUnlink>> {
        make_blobstore_enumerable_with_unlink(
            self.env.fb,
            config.clone(),
            &self.env.blobstore_options,
        )
        .watched()
        .await
    }

    pub async fn blobstore(&self, config: &BlobConfig) -> Result<Arc<dyn Blobstore>> {
        self.blobstores
            .get_or_try_init(config, || async move {
                let mut blobstore = self.blobstore_no_cache(config).await?;

                match self.env.caching {
                    Caching::Enabled(local_cache_config) => {
                        let fb = self.env.fb;
                        let memcache_blobstore = tokio::task::spawn_blocking(move || {
                            new_memcache_blobstore(fb, blobstore, "multiplexed", "")
                        })
                        .await??;
                        blobstore = cachelib_blobstore(
                            memcache_blobstore,
                            local_cache_config.blobstore_cache_shards,
                            &self.env.blobstore_options.cachelib_options,
                        )?
                    }
                    Caching::LocalOnly(local_cache_config) => {
                        blobstore = cachelib_blobstore(
                            blobstore,
                            local_cache_config.blobstore_cache_shards,
                            &self.env.blobstore_options.cachelib_options,
                        )?;
                    }
                    Caching::Disabled => {}
                };

                if let Some(blobstore_override) = &self.blobstore_override {
                    blobstore = blobstore_override(blobstore);
                }

                Ok(blobstore)
            })
            .await
    }

    #[cfg(fbcode_build)]
    async fn zelos_client(&self, config: &ZelosConfig) -> Result<Arc<dyn ZeusClient>> {
        self.zelos_clients
            .get_or_try_init(config, || async move {
                let zelos_client = match config {
                    ZelosConfig::Local { port } => {
                        ZeusCppClient::zelos_client_for_local_ensemble_reconnecting(*port)
                            .with_context(|| {
                                format!("Error creating Local Zeus client on port {port}")
                            })?
                    }
                    ZelosConfig::Remote { tier } => ZeusCppClient::new_reconnecting(
                        self.env.fb,
                        ZEUS_CLIENT_ID,
                        tier,
                    )
                    .with_context(|| {
                        format!(
                            "Error creating Zeus client to {tier} with client id {ZEUS_CLIENT_ID}"
                        )
                    })?,
                };
                let zelos_client: Arc<dyn ZeusClient> = Arc::new(zelos_client);
                Ok(zelos_client)
            })
            .await
    }

    pub async fn redacted_blobs(
        &self,
        ctx: CoreContext,
        db_config: &MetadataDatabaseConfig,
        common_config: &ArcCommonConfig,
    ) -> Result<Arc<RedactedBlobs>> {
        self.redacted_blobs
            .get_or_try_init(db_config, || async move {
                let blobstore = self.redaction_config_blobstore(common_config).await?;
                Ok(Arc::new(
                    RedactedBlobs::from_configerator(
                        &self.env.config_store,
                        &common_config.redaction_config.redaction_sets_location,
                        ctx,
                        blobstore,
                    )
                    .await?,
                ))
            })
            .await
    }

    pub async fn redaction_config_blobstore_from_config(
        &self,
        config: &BlobConfig,
    ) -> Result<ArcRedactionConfigBlobstore> {
        let blobstore = self.blobstore(config).await?;
        Ok(Arc::new(RedactionConfigBlobstore::new(blobstore)))
    }

    fn ctx(&self) -> CoreContext {
        self.ctx_with_client_entry_point(None)
    }

    fn ctx_with_client_entry_point(
        &self,
        client_entry_point: Option<ClientEntryPoint>,
    ) -> CoreContext {
        let session = if let Some(client_entry_point) = client_entry_point {
            SessionContainer::new_with_client_info(
                self.env.fb,
                ClientInfo::default_with_entry_point(client_entry_point),
            )
        } else {
            SessionContainer::new_with_defaults(self.env.fb)
        };
        session.new_context(self.env.scuba_sample_builder.clone())
    }

    /// Returns a cache builder for the named pool if caching is enabled
    fn cache_handler_factory(&self, name: &str) -> Result<Option<CacheHandlerFactory>> {
        fn map_encoding(encoding: LocalCacheEncoding) -> CacheHandlerEncoding {
            match encoding {
                LocalCacheEncoding::Bincode => CacheHandlerEncoding::Bincode,
            }
        }

        match self.env.caching {
            Caching::Enabled(config) => Ok(Some(CacheHandlerFactory::Shared {
                cachelib_pool: volatile_pool(name)?,
                memcache_client: MemcacheClient::new(self.env.fb)
                    .context("Failed to initialize memcache client")?,
                encoding: map_encoding(config.encoding),
            })),
            Caching::LocalOnly(config) => Ok(Some(CacheHandlerFactory::Local {
                cachelib_pool: volatile_pool(name)?,
                encoding: map_encoding(config.encoding),
            })),
            Caching::Disabled => Ok(None),
        }
    }

    fn censored_scuba_builder(
        &self,
        config: &ArcCommonConfig,
    ) -> Result<MononokeScubaSampleBuilder> {
        let censored_scuba_params = &config.censored_scuba_params;
        let mut builder = MononokeScubaSampleBuilder::with_opt_table(
            self.env.fb,
            censored_scuba_params.table.clone(),
        )?;
        builder.add_common_server_data();
        if let Some(scuba_log_file) = &censored_scuba_params.local_path {
            builder = builder.with_log_file(scuba_log_file)?;
        }
        Ok(builder)
    }

    pub fn acl_provider(&self) -> &dyn AclProvider {
        self.env.acl_provider.as_ref()
    }
}

fn cache_pool(name: &str) -> Result<cachelib::LruCachePool> {
    Ok(cachelib::get_pool(name)
        .ok_or_else(|| RepoFactoryError::MissingCachePool(name.to_string()))?)
}

fn volatile_pool(name: &str) -> Result<cachelib::VolatileLruCachePool> {
    Ok(cachelib::get_volatile_pool(name)?
        .ok_or_else(|| RepoFactoryError::MissingCachePool(name.to_string()))?)
}

pub fn cachelib_blobstore<B: Blobstore + 'static>(
    blobstore: B,
    cache_shards: usize,
    options: &CachelibBlobstoreOptions,
) -> Result<Arc<dyn Blobstore>> {
    const BLOBSTORE_BLOBS_CACHE_POOL: &str = "blobstore-blobs";
    const BLOBSTORE_PRESENCE_CACHE_POOL: &str = "blobstore-presence";

    let blobstore: Arc<dyn Blobstore> = match NonZeroUsize::new(cache_shards) {
        Some(cache_shards) => {
            let blob_pool = volatile_pool(BLOBSTORE_BLOBS_CACHE_POOL)?;
            let presence_pool = volatile_pool(BLOBSTORE_PRESENCE_CACHE_POOL)?;

            Arc::new(VirtuallyShardedBlobstore::new(
                blobstore,
                blob_pool,
                presence_pool,
                cache_shards,
                options.clone(),
            ))
        }
        None => {
            let blob_pool = cache_pool(BLOBSTORE_BLOBS_CACHE_POOL)?;
            let presence_pool = cache_pool(BLOBSTORE_PRESENCE_CACHE_POOL)?;

            Arc::new(new_cachelib_blobstore_no_lease(
                blobstore,
                Arc::new(blob_pool),
                Arc::new(presence_pool),
                options.clone(),
            ))
        }
    };

    Ok(blobstore)
}

#[derive(Debug, Error)]
pub enum RepoFactoryError {
    #[error("Error opening bookmarks")]
    Bookmarks,

    #[error("Error opening phases")]
    Phases,

    #[error("Error opening bonsai-hg mapping")]
    BonsaiHgMapping,

    #[error("Error opening bonsai-git mapping")]
    BonsaiGitMapping,

    #[error("Error opening bonsai-globalrev mapping")]
    BonsaiGlobalrevMapping,

    #[error("Error opening bonsai-p4 mapping")]
    BonsaiP4Mapping,

    #[error("Error opening bonsai-svnrev mapping")]
    BonsaiSvnrevMapping,

    #[error("Error opening bonsai-tag mapping")]
    BonsaiTagMapping,

    #[error("Error opening git-ref-content mapping")]
    GitRefContentMapping,

    #[error("Error opening git-bundle-uri")]
    GitBundleUri,

    #[error("Error opening git-symbolic-refs")]
    GitSymbolicRefs,

    #[error("Error opening repo-metadata-checkpoint")]
    RepoMetadataCheckpoint,

    #[error("Error opening git-push-redirect-config")]
    GitSourceOfTruthConfig,

    #[error("Error opening repo-manifest-mapping")]
    RepoManifestMapping,

    #[error("Error opening enabled-derived-data-types")]
    EnabledDerivedDataTypes,

    #[error("Error opening pushrebase mutation mapping")]
    PushrebaseMutationMapping,

    #[error("Error opening filenodes")]
    Filenodes,

    #[error("Error opening hg mutation store")]
    HgMutationStore,

    #[error("Missing cache pool: {0}")]
    MissingCachePool(String),

    #[error("Error opening long-running request queue")]
    LongRunningRequestsQueue,

    #[error("Error opening mutable renames")]
    MutableRenames,

    #[error("Error opening cross repo sync mapping")]
    RepoCrossRepo,

    #[error("Error opening mutable counters")]
    MutableCounters,

    #[error("Error creating hook manager")]
    HookManager,

    #[error("Error creating bookmark attributes")]
    RepoBookmarkAttrs,

    #[error("Error creating streaming clone")]
    StreamingClone,

    #[error("Error creating push redirector base")]
    PushRedirectorBase,

    #[error("Error creating target repo DB")]
    TargetRepoDbs,

    #[error("Error creating repo handler base")]
    RepoHandlerBase,

    #[error("Error opening bonsai blob mapping DB")]
    BonsaiBlobMapping,

    #[error("Error opening commit derived data mapping DB")]
    CommitDerivedDataMapping,

    #[error("Error opening deletion log DB")]
    SqlDeletionLog,

    #[error("Error opening restricted paths manifest id DB")]
    SqlRestrictedPathsManifestIdStore,

    #[error("Error creating SqlRestrictedPaths")]
    SqlRestrictedPaths,

    #[error("Error opening commit cloud DB")]
    SqlCommitCloud,

    #[error("Error opening push redirector DB")]
    PushRedirectConfig,
}

#[facet::factory(
    name: String,
    repo_config_param: RepoConfig,
    common_config_param: CommonConfig,
    repo_config_source_param: RepoConfigSource
)]
impl RepoFactory {
    pub fn repo_config(&self, repo_config_param: &RepoConfig) -> ArcRepoConfig {
        Arc::new(repo_config_param.clone())
    }

    pub fn repo_config_source(
        &self,
        repo_config_source_param: &RepoConfigSource,
    ) -> ArcRepoConfigSource {
        Arc::new(repo_config_source_param.clone())
    }

    pub fn common_config(&self, common_config_param: &CommonConfig) -> ArcCommonConfig {
        Arc::new(common_config_param.clone())
    }

    pub fn repo_identity(&self, name: &str, repo_config: &ArcRepoConfig) -> ArcRepoIdentity {
        Arc::new(RepoIdentity::new(repo_config.repoid, name.to_string()))
    }

    pub fn caching(&self) -> Caching {
        self.env.caching
    }

    pub async fn repo_stats_logger(
        &self,
        repo_identity: &ArcRepoIdentity,
        repo_config: &ArcRepoConfig,
        bookmarks: &ArcBookmarks,
        repo_blobstore: &ArcRepoBlobstore,
        repo_derived_data: &ArcRepoDerivedData,
    ) -> Result<ArcRepoStatsLogger> {
        Ok(Arc::new(
            RepoStatsLogger::new(
                self.env.fb,
                repo_identity.name().to_string(),
                repo_config.clone(),
                bookmarks.clone(),
                repo_blobstore.clone(),
                repo_derived_data.clone(),
            )
            .await?,
        ))
    }

    pub async fn sql_bookmarks(
        &self,
        repo_config: &ArcRepoConfig,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcSqlBookmarks> {
        let sql_bookmarks = self
            .open_sql::<SqlBookmarksBuilder>(repo_config)
            .await
            .context(RepoFactoryError::Bookmarks)?
            .with_repo_id(repo_identity.id());

        Ok(Arc::new(sql_bookmarks))
    }

    pub fn bookmarks(&self, sql_bookmarks: &ArcSqlBookmarks) -> ArcBookmarks {
        Arc::new(CachedBookmarks::new(sql_bookmarks.clone()))
    }

    pub fn bookmark_update_log(&self, sql_bookmarks: &ArcSqlBookmarks) -> ArcBookmarkUpdateLog {
        sql_bookmarks.clone()
    }

    pub async fn phases(
        &self,
        repo_identity: &ArcRepoIdentity,
        repo_config: &ArcRepoConfig,
        bookmarks: &ArcBookmarks,
        commit_graph: &ArcCommitGraph,
    ) -> Result<ArcPhases> {
        let mut sql_phases_builder = self
            .open_sql::<SqlPhasesBuilder>(repo_config)
            .await
            .context(RepoFactoryError::Phases)?;
        if let Some(cache_handler_factory) = self.cache_handler_factory("phases")? {
            sql_phases_builder.enable_caching(cache_handler_factory);
        }
        let heads_fetcher = bookmark_heads_fetcher(bookmarks.clone());
        Ok(sql_phases_builder.build(
            repo_identity.id(),
            commit_graph.clone(),
            heads_fetcher,
            self.env.rendezvous_options,
        ))
    }

    pub async fn bonsai_hg_mapping(
        &self,
        repo_identity: &ArcRepoIdentity,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcBonsaiHgMapping> {
        let mut builder = self
            .open_sql::<SqlBonsaiHgMappingBuilder>(repo_config)
            .await
            .context(RepoFactoryError::BonsaiHgMapping)?;

        if self.bonsai_hg_mapping_overwrite {
            builder = builder.with_overwrite();
        }

        let scuba = match repo_config
            .derived_data_config
            .bonsai_hg_mapping_scuba_table
            .as_deref()
        {
            Some(scuba_table) => MononokeScubaSampleBuilder::new(self.env.fb, scuba_table)
                .context("Couldn't create scuba sample builder for bonsai_hg_mapping")?,
            None => MononokeScubaSampleBuilder::with_discard(),
        };
        builder = builder.with_scuba(scuba);

        let bonsai_hg_mapping = builder.build(repo_identity.id(), self.env.rendezvous_options);

        if let Some(cache_handler_factory) = self.cache_handler_factory("bonsai_hg_mapping")? {
            Ok(Arc::new(CachingBonsaiHgMapping::new(
                Arc::new(bonsai_hg_mapping),
                cache_handler_factory,
            )?))
        } else {
            Ok(Arc::new(bonsai_hg_mapping))
        }
    }

    pub async fn bonsai_git_mapping(
        &self,
        repo_config: &ArcRepoConfig,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcBonsaiGitMapping> {
        let bonsai_git_mapping = self
            .open_sql::<SqlBonsaiGitMappingBuilder>(repo_config)
            .await
            .context(RepoFactoryError::BonsaiGitMapping)?
            .build(repo_identity.id(), self.env.rendezvous_options);
        if let Some(cache_handler_factory) = self.cache_handler_factory("bonsai_git_mapping")? {
            Ok(Arc::new(CachingBonsaiGitMapping::new(
                Arc::new(bonsai_git_mapping),
                cache_handler_factory,
            )?))
        } else {
            Ok(Arc::new(bonsai_git_mapping))
        }
    }

    pub async fn bonsai_globalrev_mapping(
        &self,
        repo_config: &ArcRepoConfig,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcBonsaiGlobalrevMapping> {
        let bonsai_globalrev_mapping = self
            .open_sql::<SqlBonsaiGlobalrevMappingBuilder>(repo_config)
            .await
            .context(RepoFactoryError::BonsaiGlobalrevMapping)?
            .build(self.env.rendezvous_options, repo_identity.id());
        if let Some(cache_handler_factory) =
            self.cache_handler_factory("bonsai_globalrev_mapping")?
        {
            Ok(Arc::new(CachingBonsaiGlobalrevMapping::new(
                Arc::new(bonsai_globalrev_mapping),
                cache_handler_factory,
            )))
        } else {
            Ok(Arc::new(bonsai_globalrev_mapping))
        }
    }

    pub async fn bonsai_p4_mapping(
        &self,
        repo_config: &ArcRepoConfig,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcBonsaiP4Mapping> {
        let bonsai_p4_mapping = self
            .open_sql::<SqlBonsaiP4MappingBuilder>(repo_config)
            .await
            .context(RepoFactoryError::BonsaiP4Mapping)?
            .build(repo_identity.id());
        if let Some(cache_handler_factory) = self.cache_handler_factory("bonsai_p4_mapping")? {
            Ok(Arc::new(CachingBonsaiP4Mapping::new(
                Arc::new(bonsai_p4_mapping),
                cache_handler_factory,
            )))
        } else {
            Ok(Arc::new(bonsai_p4_mapping))
        }
    }

    pub async fn bonsai_svnrev_mapping(
        &self,
        repo_config: &ArcRepoConfig,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcBonsaiSvnrevMapping> {
        let bonsai_svnrev_mapping = self
            .open_sql::<SqlBonsaiSvnrevMappingBuilder>(repo_config)
            .await
            .context(RepoFactoryError::BonsaiSvnrevMapping)?
            .build(repo_identity.id());
        if let Some(cache_handler_factory) = self.cache_handler_factory("bonsai_svnrev_mapping")? {
            Ok(Arc::new(CachingBonsaiSvnrevMapping::new(
                Arc::new(bonsai_svnrev_mapping),
                cache_handler_factory,
            )))
        } else {
            Ok(Arc::new(bonsai_svnrev_mapping))
        }
    }

    pub async fn bonsai_tag_mapping(
        &self,
        repo_config: &ArcRepoConfig,
        repo_identity: &ArcRepoIdentity,
        repo_event_publisher: &ArcRepoEventPublisher,
    ) -> Result<ArcBonsaiTagMapping> {
        let bonsai_tag_mapping = self
            .open_sql::<SqlBonsaiTagMappingBuilder>(repo_config)
            .await
            .context(RepoFactoryError::BonsaiTagMapping)?
            .build(repo_identity.id(), self.env.rendezvous_options);
        let repo_name = repo_identity.name();
        if justknobs::eval(
            "scm/mononoke:enable_bonsai_tag_mapping_caching",
            None,
            Some(repo_name),
        ) {
            match repo_event_publisher.subscribe_for_tag_updates(&repo_name.to_string()) {
                Ok(update_notification_receiver) => {
                    let cached_bonsai_tag_mapping = CachedBonsaiTagMapping::new(
                        &self.ctx(),
                        Arc::new(bonsai_tag_mapping),
                        update_notification_receiver,
                    )
                    .await?;
                    Ok(Arc::new(cached_bonsai_tag_mapping))
                }
                // The scribe configuration does not exist for tag updates for this repo, so use the non-cached
                // version of bonsai_tag_mapping
                Err(_) => Ok(Arc::new(bonsai_tag_mapping)),
            }
        } else {
            Ok(Arc::new(bonsai_tag_mapping))
        }
    }

    pub async fn git_ref_content_mapping(
        &self,
        repo_config: &ArcRepoConfig,
        repo_identity: &ArcRepoIdentity,
        repo_event_publisher: &ArcRepoEventPublisher,
    ) -> Result<ArcGitRefContentMapping> {
        let git_ref_content_mapping = self
            .open_sql::<SqlGitRefContentMappingBuilder>(repo_config)
            .await
            .context(RepoFactoryError::GitRefContentMapping)?
            .build(repo_identity.id());
        let repo_name = repo_identity.name();
        match repo_event_publisher.subscribe_for_content_refs_updates(&repo_name.to_string()) {
            Ok(update_notification_receiver) => {
                let cached_git_ref_content_mapping = CachedGitRefContentMapping::new(
                    &self.ctx(),
                    Arc::new(git_ref_content_mapping),
                    update_notification_receiver,
                )
                .await?;
                Ok(Arc::new(cached_git_ref_content_mapping))
            }
            // The scribe configuration does not exist for content ref updates for this repo, so use the non-cached
            // version of git_ref_content_mapping
            Err(_) => Ok(Arc::new(git_ref_content_mapping)),
        }
    }

    pub async fn git_bundle_uri(
        &self,
        repo_config: &ArcRepoConfig,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcGitBundleUri> {
        let git_bundle_uri_storage = self
            .open_sql::<SqlGitBundleMetadataStorageBuilder>(repo_config)
            .await
            .context(RepoFactoryError::GitBundleUri)?
            .build(repo_identity.id());

        let config = &repo_config
            .git_configs
            .git_bundle_uri
            .as_ref()
            .ok_or(RepoFactoryError::GitBundleUri)?;

        Ok(bundle_uri::bundle_uri_arc(
            self.env.fb,
            git_bundle_uri_storage,
            repo_config.repoid,
            config,
        ))
    }

    pub async fn git_symbolic_refs(
        &self,
        repo_config: &ArcRepoConfig,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcGitSymbolicRefs> {
        let repo_name = repo_identity.name();
        let git_symbolic_refs = self
            .open_sql::<SqlGitSymbolicRefsBuilder>(repo_config)
            .await
            .context(RepoFactoryError::GitSymbolicRefs)?
            .build(repo_identity.id());
        if justknobs::eval(
            "scm/mononoke:disable_git_symbolic_refs_caching",
            None,
            Some(repo_name),
        ) {
            Ok(Arc::new(git_symbolic_refs))
        } else {
            let cached_git_symbolic_refs =
                CachedGitSymbolicRefs::new(&self.ctx(), Arc::new(git_symbolic_refs)).await?;
            Ok(Arc::new(cached_git_symbolic_refs))
        }
    }

    pub async fn repo_metadata_checkpoint(
        &self,
        repo_config: &ArcRepoConfig,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcRepoMetadataCheckpoint> {
        let repo_metadata_info = self
            .open_sql::<SqlRepoMetadataCheckpointBuilder>(repo_config)
            .await
            .context(RepoFactoryError::RepoMetadataCheckpoint)?
            .build(repo_identity.id(), self.ctx().sql_query_telemetry());
        Ok(Arc::new(repo_metadata_info))
    }

    pub async fn git_source_of_truth_config(
        &self,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcGitSourceOfTruthConfig> {
        let git_source_of_truth_config = self
            .open_sql::<SqlGitSourceOfTruthConfigBuilder>(repo_config)
            .await
            .context(RepoFactoryError::GitSourceOfTruthConfig)?
            .build();
        Ok(Arc::new(git_source_of_truth_config))
    }

    /// Cached per metadata config, not per repo: the store is global, and nearly
    /// every repo resolves to the same one, so per-repo would be N identical
    /// copies.
    pub async fn repo_manifest_mapping(
        &self,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcRepoManifestMapping> {
        let db_config = &repo_config.storage_config.metadata;
        self.repo_manifest_mappings
            .get_or_try_init(db_config, || async move {
                if !repo_manifest_mapping::is_configured(db_config) {
                    return Ok(Arc::new(UnconfiguredRepoManifestMapping) as ArcRepoManifestMapping);
                }
                let mapping = self
                    .open_sql::<SqlRepoManifestMappingBuilder>(repo_config)
                    .await
                    .context(RepoFactoryError::RepoManifestMapping)?
                    .build();
                Ok(Arc::new(mapping) as ArcRepoManifestMapping)
            })
            .await
    }

    pub async fn enabled_derived_data_types(
        &self,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcEnabledDerivedDataTypes> {
        let enabled_derived_data_types = self
            .open_sql::<SqlEnabledDerivedDataTypesBuilder>(repo_config)
            .await
            .context(RepoFactoryError::EnabledDerivedDataTypes)?
            .build();
        Ok(Arc::new(enabled_derived_data_types))
    }

    pub async fn pushrebase_mutation_mapping(
        &self,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcPushrebaseMutationMapping> {
        let conn = self
            .open_sql::<SqlPushrebaseMutationMappingConnection>(repo_config)
            .await
            .context(RepoFactoryError::PushrebaseMutationMapping)?;
        Ok(Arc::new(conn.with_repo_id(repo_config.repoid)))
    }

    pub async fn permission_checker(
        &self,
        repo_config: &ArcRepoConfig,
        repo_identity: &ArcRepoIdentity,
        common_config: &ArcCommonConfig,
    ) -> Result<ArcRepoPermissionChecker> {
        let repo_name = repo_identity.name();
        let permission_checker = ProdRepoPermissionChecker::new(
            self.env.acl_provider.as_ref(),
            repo_config.hipster_acl.as_deref(),
            repo_config
                .source_control_service
                .service_write_hipster_acl
                .as_deref(),
            repo_config
                .acl_region_config
                .as_ref()
                .map(|config| {
                    config
                        .allow_rules
                        .iter()
                        .map(|rule| rule.hipster_acl.as_str())
                        .collect()
                })
                .unwrap_or_default(),
            repo_name,
            &common_config.global_allowlist,
        )
        .await?;
        Ok(Arc::new(permission_checker))
    }

    pub async fn filenodes(
        &self,
        repo_config: &ArcRepoConfig,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcFilenodes> {
        let (sql_factory, mut filenodes_builder) = self
            .open_sql_shardable::<NewFilenodesBuilder>(repo_config)
            .await
            .context(RepoFactoryError::Filenodes)?;
        if let (Some(filenodes_cache_handler_factory), Some(history_cache_handler_factory)) = (
            self.cache_handler_factory("filenodes")?,
            self.cache_handler_factory("filenodes_history")?,
        ) {
            let filenodes_tier = sql_factory.tier_info_shardable::<NewFilenodesBuilder>()?;
            filenodes_builder.enable_caching(
                filenodes_cache_handler_factory,
                history_cache_handler_factory,
                "newfilenodes",
                &filenodes_tier.tier_name,
            )?;
        }
        Ok(Arc::new(filenodes_builder.build(repo_identity.id())?))
    }

    pub async fn hg_mutation_store(
        &self,
        repo_config: &ArcRepoConfig,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcHgMutationStore> {
        let mut builder = self
            .open_sql::<SqlHgMutationStoreBuilder>(repo_config)
            .await
            .context(RepoFactoryError::HgMutationStore)?;
        let mutation_limit = justknobs::get_as::<usize>(
            "scm/mononoke:mutation_chain_length_limit",
            Some(repo_identity.name()),
        );
        builder = builder.with_mutation_limit(mutation_limit);
        let hg_mutation_store = builder.with_repo_id(repo_identity.id());

        if let Some(cache_handler_factory) = self.cache_handler_factory("hg_mutation_store")? {
            Ok(Arc::new(CachedHgMutationStore::new(
                Arc::new(hg_mutation_store),
                cache_handler_factory,
            )?))
        } else {
            Ok(Arc::new(hg_mutation_store))
        }
    }

    pub fn repo_derived_data(
        &self,
        repo_identity: &ArcRepoIdentity,
        repo_config: &ArcRepoConfig,
        commit_graph: &ArcCommitGraph,
        bonsai_hg_mapping: &ArcBonsaiHgMapping,
        bonsai_git_mapping: &ArcBonsaiGitMapping,
        filenodes: &ArcFilenodes,
        repo_blobstore: &ArcRepoBlobstore,
        filestore_config: &ArcFilestoreConfig,
        restricted_paths_config_based: &ArcRestrictedPathsConfigBased,
        commit_derived_data_mapping: &ArcCommitDerivedDataMapping,
    ) -> Result<ArcRepoDerivedData> {
        let config = repo_config.derived_data_config.clone();
        let scuba_table = self
            .derived_data_scuba_table_overwrite
            .clone()
            .or(config.scuba_table.clone());
        let scuba = build_scuba(self.env.fb, scuba_table, repo_identity.name())?;
        let derivation_service_client = get_derivation_client(
            self.env.fb,
            self.env.remote_derivation_options.clone(),
            repo_config,
            repo_identity.name(),
        )?;
        Ok(Arc::new(RepoDerivedData::new(
            repo_identity.id(),
            repo_identity.name().to_string(),
            commit_graph.clone(),
            bonsai_hg_mapping.clone(),
            bonsai_git_mapping.clone(),
            filenodes.clone(),
            repo_blobstore.as_ref().clone(),
            repo_config.clone(),
            **filestore_config,
            scuba,
            config,
            derivation_service_client,
            restricted_paths_config_based.clone(),
            commit_derived_data_mapping.clone(),
        )?))
    }

    pub async fn repo_blobstore(
        &self,
        repo_identity: &ArcRepoIdentity,
        repo_config: &ArcRepoConfig,
        common_config: &ArcCommonConfig,
    ) -> Result<ArcRepoBlobstore> {
        let blobstore = self
            .blobstore(&repo_config.storage_config.blobstore)
            .await?;
        Ok(Arc::new(
            self.repo_blobstore_from_blobstore(
                repo_identity,
                repo_config,
                &blobstore,
                common_config,
            )
            .await?,
        ))
    }

    pub async fn mutable_repo_blobstore(
        &self,
        repo_identity: &ArcRepoIdentity,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcMutableRepoBlobstore> {
        let blobstore = self
            .blobstore(&repo_config.storage_config.mutable_blobstore)
            .await?;
        Ok(Arc::new(
            self.mutable_repo_blobstore_from_blobstore(repo_identity, &blobstore)
                .await?,
        ))
    }

    pub fn filestore_config(&self, repo_config: &ArcRepoConfig) -> ArcFilestoreConfig {
        let filestore_config = repo_config.filestore.as_ref().map_or_else(
            FilestoreConfig::no_chunking_filestore,
            |p| FilestoreConfig {
                chunk_size: Some(p.chunk_size),
                concurrency: p.concurrency,
            },
        );
        Arc::new(filestore_config)
    }

    pub async fn redaction_config_blobstore(
        &self,
        common_config: &ArcCommonConfig,
    ) -> Result<ArcRedactionConfigBlobstore> {
        self.redaction_config_blobstore_from_config(&common_config.redaction_config.blobstore)
            .await
    }

    pub async fn repo_ephemeral_store(
        &self,
        repo_identity: &ArcRepoIdentity,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcRepoEphemeralStore> {
        if let Some(ephemeral_config) = &repo_config.storage_config.ephemeral_blobstore {
            let blobstore = self
                .blobstore_enumerable_with_unlink(&ephemeral_config.blobstore)
                .await?;
            let ephemeral_blobstore = RepoEphemeralStoreBuilder::with_database_config(
                self.env.fb,
                &ephemeral_config.metadata,
                &self.env.mysql_options,
                self.env.readonly_storage.0,
            )?
            .build(
                repo_identity.id(),
                blobstore,
                ephemeral_config.initial_bubble_lifespan,
                ephemeral_config.bubble_expiration_grace,
                ephemeral_config.bubble_deletion_mode,
            );
            Ok(Arc::new(ephemeral_blobstore))
        } else {
            Ok(Arc::new(RepoEphemeralStore::disabled(repo_identity.id())))
        }
    }

    pub async fn mutable_renames(&self, repo_config: &ArcRepoConfig) -> Result<ArcMutableRenames> {
        let sql_store = self
            .open_sql::<SqlMutableRenamesStore>(repo_config)
            .await
            .context(RepoFactoryError::MutableRenames)?;
        let cache_handler_factory = self.cache_handler_factory("mutable_renames")?;
        Ok(Arc::new(MutableRenames::new(
            repo_config.repoid,
            sql_store,
            cache_handler_factory,
        )?))
    }

    /// Build config-based restricted paths (shared with derived-data crates).
    pub async fn restricted_paths_config_based(
        &self,
        repo_config: &ArcRepoConfig,
        restricted_paths_manifest_id_store: &ArcRestrictedPathsManifestIdStore,
    ) -> Result<ArcRestrictedPathsConfigBased> {
        let ctx = self.ctx().clone();
        let restricted_paths_config = repo_config.restricted_paths_config.clone();
        let manifest_id_store_config = &restricted_paths_config.manifest_id_store_config;

        let manifest_id_cache = if !restricted_paths_config.is_empty()
            && manifest_id_store_config.use_manifest_id_cache
        {
            let cache = RestrictedPathsManifestIdCacheBuilder::new(
                ctx.clone(),
                restricted_paths_manifest_id_store.clone(),
            )
            .with_manifest_id_store_cache(manifest_id_store_config.clone())
            .build()
            .await?;

            Some(Arc::new(cache))
        } else {
            None
        };

        Ok(Arc::new(RestrictedPathsConfigBased::new(
            restricted_paths_config,
            restricted_paths_manifest_id_store.clone(),
            manifest_id_cache,
        )))
    }

    /// Restricted paths
    pub fn restricted_paths(
        &self,
        repo_identity: &ArcRepoIdentity,
        restricted_paths_config_based: &ArcRestrictedPathsConfigBased,
        repo_derived_data: &ArcRepoDerivedData,
    ) -> Result<ArcRestrictedPaths> {
        // Construct scuba builder for logging access to restricted paths.
        // Check for environment variable override for integration tests.
        let scuba_builder = if let Ok(scuba_file_path) = std::env::var("ACCESS_LOG_SCUBA_FILE_PATH")
        {
            MononokeScubaSampleBuilder::with_discard().with_log_file(scuba_file_path)?
        } else {
            MononokeScubaSampleBuilder::new(self.env.fb, restricted_paths::ACCESS_LOG_SCUBA_TABLE)?
        };

        let restricted_paths = RestrictedPaths::new(
            repo_identity.name().to_string(),
            restricted_paths_config_based.clone(),
            self.env.acl_provider.clone(),
            scuba_builder,
            repo_derived_data.clone(),
        )?;

        Ok(Arc::new(restricted_paths))
    }

    /// Commit rate limit configuration facet.
    pub fn commit_rate_limit(
        &self,
        repo_identity: &ArcRepoIdentity,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcCommitRateLimit> {
        let commit_rate_limit = match &repo_config.commit_rate_limit_config {
            Some(config) => build_commit_rate_limit(config, repo_identity.name())?,
            None => CommitRateLimit::empty(),
        };
        Ok(Arc::new(commit_rate_limit))
    }

    /// Restricted paths root ids store
    pub async fn restricted_paths_manifest_id_store(
        &self,
        repo_identity: &ArcRepoIdentity,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcRestrictedPathsManifestIdStore> {
        let restricted_paths_manifest_id_store = self
            .open_sql::<SqlRestrictedPathsManifestIdStoreBuilder>(repo_config)
            .await
            .context(RepoFactoryError::SqlRestrictedPathsManifestIdStore)?
            .with_repo_id(repo_identity.id());

        Ok(Arc::new(restricted_paths_manifest_id_store))
    }

    pub async fn repo_derivation_queues(
        &self,
        repo_config: &ArcRepoConfig,
        repo_derived_data: &ArcRepoDerivedData,
    ) -> Result<ArcRepoDerivationQueues> {
        #[cfg(not(fbcode_build))]
        {
            let _ = (repo_config, repo_derived_data);
            anyhow::bail!("RepoDerivationQueues is not supported in non-fbcode builds")
        }
        #[cfg(fbcode_build)]
        {
            let zelos_config = if self.env.use_pipeline_zelos_config {
                repo_config.pipeline_zelos_config.as_ref()
            } else {
                repo_config.zelos_config.as_ref()
            }
            .ok_or_else(|| {
                anyhow!("Missing zelos config while trying to construct repo_derivation_queues")
            })?;
            let zelos_client = self.zelos_client(zelos_config).await?;

            let namespace = self.env.derivation_queue_namespace.clone();
            anyhow::Ok(Arc::new(
                zelos_derivation_queues(repo_derived_data.clone(), zelos_client, namespace).await?,
            ))
        }
    }

    /// The commit mapping between repos for synced commits.
    pub async fn synced_commit_mapping(
        &self,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcSyncedCommitMapping> {
        let sql_mapping = Arc::new(
            self.open_sql::<SqlSyncedCommitMappingBuilder>(repo_config)
                .await?
                .build(self.env.rendezvous_options),
        );
        let maybe_cached_mapping: ArcSyncedCommitMapping = if let Some(cache_handler_factory) =
            self.cache_handler_factory("synced_commit_mapping")?
        {
            Arc::new(CachingSyncedCommitMapping::new(
                sql_mapping,
                cache_handler_factory,
            )?)
        } else {
            sql_mapping
        };
        Ok(maybe_cached_mapping)
    }

    /// Cross-repo sync manager for this repo
    pub async fn repo_cross_repo(
        &self,
        synced_commit_mapping: &ArcSyncedCommitMapping,
        push_redirection_config: &ArcPushRedirectionConfig,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcRepoCrossRepo> {
        let sync_lease = create_commit_syncer_lease(self.env.fb, self.env.caching)?;
        let live_commit_sync_config = Arc::new(CfgrLiveCommitSyncConfig::new(
            &self.env.config_store,
            push_redirection_config.clone(),
        )?);
        let repo_xrepo = RepoCrossRepo::new(
            synced_commit_mapping.clone(),
            live_commit_sync_config,
            sync_lease,
            repo_identity.id(),
        )
        .await?;
        Ok(Arc::new(repo_xrepo))
    }

    pub async fn mutable_counters(
        &self,
        repo_identity: &ArcRepoIdentity,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcMutableCounters> {
        Ok(Arc::new(
            self.open_sql::<SqlMutableCountersBuilder>(repo_config)
                .await
                .context(RepoFactoryError::MutableCounters)?
                .build(repo_identity.id()),
        ))
    }

    pub fn acl_regions(
        &self,
        repo_config: &ArcRepoConfig,
        commit_graph: &ArcCommitGraph,
    ) -> ArcAclRegions {
        build_acl_regions(repo_config.acl_region_config.as_ref(), commit_graph.clone())
    }

    pub async fn hook_manager(
        &self,
        repo_config: &ArcRepoConfig,
        repo_identity: &ArcRepoIdentity,
        repo_derived_data: &ArcRepoDerivedData,
        bookmarks: &ArcBookmarks,
        repo_blobstore: &ArcRepoBlobstore,
        bonsai_tag_mapping: &ArcBonsaiTagMapping,
        bonsai_git_mapping: &ArcBonsaiGitMapping,
        permission_checker: &ArcRepoPermissionChecker,
        repo_cross_repo: &ArcRepoCrossRepo,
        commit_graph: &ArcCommitGraph,
        restricted_paths: &ArcRestrictedPaths,
        commit_rate_limit: &ArcCommitRateLimit,
    ) -> Result<ArcHookManager> {
        let name = repo_identity.name();

        let disabled_hooks = self
            .env
            .disabled_hooks
            .get(name)
            .cloned()
            .unwrap_or_default();

        let hooks_scuba_local_path = repo_config.scuba_local_path_hooks.clone();

        let mut hooks_scuba = MononokeScubaSampleBuilder::with_opt_table(
            self.env.fb,
            repo_config.scuba_table_hooks.clone(),
        )?;

        hooks_scuba.add("repo", name);

        if let Some(hooks_scuba_local_path) = hooks_scuba_local_path {
            hooks_scuba = hooks_scuba.with_log_file(hooks_scuba_local_path)?;
        }

        let hook_manager = async {
            let hook_repo = HookRepo {
                repo_identity: repo_identity.clone(),
                repo_config: repo_config.clone(),
                repo_blobstore: repo_blobstore.clone(),
                repo_derived_data: repo_derived_data.clone(),
                bookmarks: bookmarks.clone(),
                bonsai_tag_mapping: bonsai_tag_mapping.clone(),
                bonsai_git_mapping: bonsai_git_mapping.clone(),
                repo_cross_repo: repo_cross_repo.clone(),
                commit_graph: commit_graph.clone(),
                restricted_paths: restricted_paths.clone(),
                commit_rate_limit: commit_rate_limit.clone(),
            };

            let mut hook_manager = HookManager::new(
                self.env.fb,
                self.env.acl_provider.as_ref(),
                hook_repo,
                repo_config.hook_manager_params.clone().unwrap_or_default(),
                permission_checker.clone(),
                hooks_scuba,
                name.to_string(),
            )
            .await?;

            load_hooks(
                self.env.fb,
                self.env.acl_provider.clone(),
                &mut hook_manager,
                repo_config,
                &disabled_hooks,
            )
            .await?;

            <Result<_, anyhow::Error>>::Ok(hook_manager)
        }
        .watched()
        .await
        .context(RepoFactoryError::HookManager)?;

        Ok(Arc::new(hook_manager))
    }

    pub async fn sparse_profiles(
        &self,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcRepoSparseProfiles> {
        let sql_profile_sizes = self
            .open_sql::<SqlSparseProfilesSizes>(repo_config)
            .await
            .ok();
        Ok(Arc::new(RepoSparseProfiles { sql_profile_sizes }))
    }

    pub async fn repo_lock(
        &self,
        repo_config: &ArcRepoConfig,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcRepoLock> {
        match repo_config.readonly {
            RepoReadOnly::ReadOnly(ref reason) => Ok(Arc::new(AlwaysLockedRepoLock::new(
                repo_identity.id(),
                reason.clone(),
            ))),
            RepoReadOnly::ReadWrite => {
                let sql = SqlRepoLock::with_metadata_database_config(
                    self.env.fb,
                    &repo_config.storage_config.metadata,
                    &self.env.mysql_options,
                    self.env.readonly_storage.0,
                )
                .await?;

                Ok(Arc::new(MutableRepoLock::new(sql, repo_identity.id())))
            }
        }
    }

    pub async fn repo_bookmark_attrs(
        &self,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcRepoBookmarkAttrs> {
        let repo_bookmark_attrs = RepoBookmarkAttrs::new(
            self.env.acl_provider.as_ref(),
            repo_config.bookmarks.clone(),
        )
        .await
        .context(RepoFactoryError::RepoBookmarkAttrs)?;
        Ok(Arc::new(repo_bookmark_attrs))
    }

    pub async fn streaming_clone(
        &self,
        repo_config: &ArcRepoConfig,
        repo_identity: &ArcRepoIdentity,
        mutable_repo_blobstore: &ArcMutableRepoBlobstore,
    ) -> Result<ArcStreamingClone> {
        let streaming_clone = self
            .open_sql::<StreamingCloneBuilder>(repo_config)
            .await
            .context(RepoFactoryError::StreamingClone)?
            .build(repo_identity.id(), mutable_repo_blobstore.clone());
        Ok(Arc::new(streaming_clone))
    }

    pub async fn warm_bookmarks_cache(
        &self,
        bookmarks: &ArcBookmarks,
        bookmark_update_log: &ArcBookmarkUpdateLog,
        repo_identity: &ArcRepoIdentity,
        repo_derived_data: &ArcRepoDerivedData,
        repo_event_publisher: &ArcRepoEventPublisher,
        phases: &ArcPhases,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcBookmarksCache> {
        let cache = self
            .build_bookmarks_cache_impl(
                bookmarks,
                bookmark_update_log,
                repo_identity,
                repo_derived_data,
                repo_event_publisher,
                phases,
                repo_config,
            )
            .await?;
        Ok(cache as ArcBookmarksCache)
    }

    /// Resolve the effective bookmark cache options for a repo.
    fn effective_bookmark_cache_options(
        &self,
        repo_config: &ArcRepoConfig,
    ) -> Result<(BookmarkCacheKind, BookmarkCacheDerivedData)> {
        Ok(effective_bookmark_cache_options(
            repo_config,
            &self.env.bookmark_cache_options,
        ))
    }

    async fn build_bookmarks_cache_impl(
        &self,
        bookmarks: &ArcBookmarks,
        bookmark_update_log: &ArcBookmarkUpdateLog,
        repo_identity: &ArcRepoIdentity,
        repo_derived_data: &ArcRepoDerivedData,
        repo_event_publisher: &ArcRepoEventPublisher,
        phases: &ArcPhases,
        repo_config: &ArcRepoConfig,
    ) -> Result<Arc<dyn CombinedBookmarksCache + Send + Sync>> {
        let (effective_cache_kind, effective_derived_data) =
            self.effective_bookmark_cache_options(repo_config)?;
        let warmer_requirement: WarmerRequirement = (&effective_derived_data).into();

        match &effective_cache_kind {
            BookmarkCacheKind::Local => {
                let scuba_sample_builder =
                    self.env.warm_bookmarks_cache_scuba_sample_builder.clone();
                let ctx = self
                    .ctx_with_client_entry_point(Some(self.env.client_entry_point_for_service))
                    .with_mutated_scuba(|_| scuba_sample_builder);

                let mut wbc_builder = WarmBookmarksCacheBuilder::new(
                    ctx,
                    bookmarks.clone(),
                    bookmark_update_log.clone(),
                    repo_identity.clone(),
                    repo_event_publisher.clone(),
                    warmer_requirement,
                )
                .with_event_router_coordinator_enabled(
                    repo_config
                        .metadata_cache_config
                        .as_ref()
                        .is_some_and(|config| config.wbc_process_bookmark_notifications),
                )
                .with_event_router_batching_enabled(
                    repo_config
                        .metadata_cache_config
                        .as_ref()
                        .is_some_and(|config| config.wbc_event_router_batching_enabled),
                );

                match effective_derived_data {
                    BookmarkCacheDerivedData::HgOnly => {
                        wbc_builder.add_hg_warmers(repo_derived_data, phases)?;
                    }
                    BookmarkCacheDerivedData::GitOnly => {
                        wbc_builder.add_git_warmers(repo_derived_data, phases)?;
                    }
                    BookmarkCacheDerivedData::AllKinds => {
                        wbc_builder.add_all_warmers(repo_derived_data, phases)?;
                    }
                    BookmarkCacheDerivedData::SpecificTypes(ref types) => {
                        wbc_builder.add_specific_types_warmers(
                            repo_derived_data,
                            types.as_slice(),
                            phases,
                        )?;
                    }
                    BookmarkCacheDerivedData::NoDerivation => {}
                }

                Ok(Arc::new(wbc_builder.build().watched().await?)
                    as Arc<dyn CombinedBookmarksCache + Send + Sync>)
            }
            BookmarkCacheKind::Remote(address) => {
                match effective_derived_data {
                    BookmarkCacheDerivedData::SpecificTypes(_) => {
                        anyhow::bail!("Remote bookmarks for 'SpecificTypes' is not supported");
                    }
                    BookmarkCacheDerivedData::NoDerivation => {
                        anyhow::bail!("Remote bookmarks for 'NoDerivation' is not supported");
                    }
                    BookmarkCacheDerivedData::HgOnly
                    | BookmarkCacheDerivedData::GitOnly
                    | BookmarkCacheDerivedData::AllKinds => {
                        // Valid cases - continue execution
                    }
                }

                let client = match address {
                    BookmarkCacheAddress::HostPort(host_port) => {
                        BookmarkServiceClient::from_host_port(self.env.fb, host_port.to_string())?
                    }
                    #[cfg(fbcode_build)]
                    BookmarkCacheAddress::SmcTier(tier_name) => {
                        BookmarkServiceClient::from_tier_name(self.env.fb, tier_name.to_string())?
                    }
                    #[cfg(fbcode_build)]
                    BookmarkCacheAddress::ShardManagerTier(tier_name) => {
                        BookmarkServiceClient::from_sm_tier_name(
                            self.env.fb,
                            tier_name.to_string(),
                            repo_identity.name().to_string(),
                        )?
                    }
                    #[cfg(not(fbcode_build))]
                    BookmarkCacheAddress::SmcTier(_)
                    | BookmarkCacheAddress::ShardManagerTier(_) => {
                        anyhow::bail!(
                            "bookmark service tier addresses are unavailable in OSS mode; configure a loopback host:port address"
                        )
                    }
                };
                let repo_client = RepoBookmarkServiceClient::new(
                    repo_identity.name().to_string(),
                    client,
                    warmer_requirement,
                );

                Ok(Arc::new(repo_client) as Arc<dyn CombinedBookmarksCache + Send + Sync>)
            }
            BookmarkCacheKind::Disabled => Ok(Arc::new(NoopBookmarksCache::new(bookmarks.clone()))
                as Arc<dyn CombinedBookmarksCache + Send + Sync>),
        }
    }

    pub async fn scoped_bookmarks_cache(
        &self,
        bookmarks: &ArcBookmarks,
        bookmark_update_log: &ArcBookmarkUpdateLog,
        repo_identity: &ArcRepoIdentity,
        repo_derived_data: &ArcRepoDerivedData,
        repo_event_publisher: &ArcRepoEventPublisher,
        phases: &ArcPhases,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcScopedBookmarksCache> {
        let cache = self
            .build_bookmarks_cache_impl(
                bookmarks,
                bookmark_update_log,
                repo_identity,
                repo_derived_data,
                repo_event_publisher,
                phases,
                repo_config,
            )
            .await?;
        Ok(cache as ArcScopedBookmarksCache)
    }

    pub async fn target_repo_dbs(
        &self,
        bookmarks: &ArcBookmarks,
        bookmark_update_log: &ArcBookmarkUpdateLog,
        mutable_counters: &ArcMutableCounters,
    ) -> Result<ArcTargetRepoDbs> {
        let target_repo_dbs = TargetRepoDbs {
            bookmarks: bookmarks.clone(),
            bookmark_update_log: bookmark_update_log.clone(),
            counters: mutable_counters.clone(),
        };
        Ok(Arc::new(target_repo_dbs))
    }

    pub async fn push_redirector_mode(
        &self,
        repo_identity: &ArcRepoIdentity,
        repo_cross_repo: &ArcRepoCrossRepo,
        target_repo_dbs: &ArcTargetRepoDbs,
    ) -> Result<ArcPushRedirectorMode> {
        let common_commit_sync_config = repo_cross_repo
            .live_commit_sync_config()
            .clone()
            .get_common_config_if_exists(repo_identity.id())
            .context(RepoFactoryError::PushRedirectorBase)?;
        let synced_commit_mapping = repo_cross_repo.synced_commit_mapping();

        let push_redirector_mode = match common_commit_sync_config {
            Some(common_commit_sync_config)
                if common_commit_sync_config.large_repo_id != repo_identity.id() =>
            {
                PushRedirectorMode::Enabled(Arc::new(PushRedirectorBase {
                    common_commit_sync_config,
                    synced_commit_mapping: synced_commit_mapping.clone(),
                    target_repo_dbs: target_repo_dbs.clone(),
                }))
            }
            _ => PushRedirectorMode::Disabled,
        };

        Ok(Arc::new(push_redirector_mode))
    }

    pub async fn repo_handler_base(
        &self,
        _repo_config: &ArcRepoConfig,
        push_redirector_mode: &ArcPushRedirectorMode,
    ) -> Result<ArcRepoHandlerBase> {
        let ctx = self.ctx();
        let scuba = ctx.scuba().clone();
        let maybe_push_redirector_base = match **push_redirector_mode {
            Enabled(ref push_redirector_base) => Some(Arc::clone(push_redirector_base)),
            PushRedirectorMode::Disabled => None,
        };
        Ok(Arc::new(RepoHandlerBase {
            scuba,
            maybe_push_redirector_base,
        }))
    }

    pub async fn sql_query_config(&self) -> Result<ArcSqlQueryConfig> {
        let caching = if let Some(cache_handler_factory) = self.cache_handler_factory("sql")? {
            const KEY_PREFIX: &str = "scm.mononoke.sql";
            const MC_CODEVER: u32 = 0;
            let sitever = justknobs::get_as::<u32>("scm/mononoke_memcache_sitevers:sql", None);
            Some(sql_query_config::CachingConfig {
                keygen: KeyGen::new(KEY_PREFIX, MC_CODEVER, sitever),
                cache_handler_factory,
            })
        } else {
            None
        };
        Ok(Arc::new(SqlQueryConfig { caching }))
    }

    pub async fn commit_graph(
        &self,
        repo_identity: &ArcRepoIdentity,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcCommitGraph> {
        let sql_storage = self
            .open_sql::<SqlCommitGraphStorageBuilder>(repo_config)
            .await?
            .build(self.env.rendezvous_options, repo_identity.clone());

        let maybe_cached_storage: Arc<dyn CommitGraphStorage> =
            if let Some(cache_handler_factory) = self.cache_handler_factory("commit_graph")? {
                Arc::new(CachingCommitGraphStorage::new(
                    Arc::new(sql_storage),
                    cache_handler_factory,
                ))
            } else {
                Arc::new(sql_storage)
            };

        match &repo_config
            .commit_graph_config
            .preloaded_commit_graph_blobstore_key
        {
            Some(preloaded_commit_graph_key)
                if !self.env.commit_graph_options.skip_preloading_commit_graph =>
            {
                let blobstore_without_cache = self
                    .mutable_repo_blobstore_from_blobstore(
                        repo_identity,
                        &self
                            .blobstore_no_cache(&repo_config.storage_config.mutable_blobstore)
                            .await?,
                    )
                    .await?;

                let preloaded_commit_graph_storage = PreloadedCommitGraphStorage::from_blobstore(
                    &self.ctx(),
                    Arc::new(blobstore_without_cache),
                    preloaded_commit_graph_key.clone(),
                    maybe_cached_storage,
                )
                .await?;

                Ok(Arc::new(CommitGraph::new(preloaded_commit_graph_storage)))
            }
            _ => Ok(Arc::new(CommitGraph::new(maybe_cached_storage))),
        }
    }

    pub async fn cgdm_changeset_divider(
        &self,
        repo_identity: &ArcRepoIdentity,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcCgdmChangesetDivider> {
        match &repo_config.git_configs.preloaded_cgdm_blobstore_key {
            Some(preloaded_cgdm_key) => {
                let blobstore_without_cache = self
                    .mutable_repo_blobstore_from_blobstore(
                        repo_identity,
                        &self
                            .blobstore_no_cache(&repo_config.storage_config.blobstore)
                            .await?,
                    )
                    .await?;
                Ok(Arc::new(
                    CGDMComponentsReloader::from_blobstore(
                        &self.ctx(),
                        Arc::new(blobstore_without_cache),
                        preloaded_cgdm_key.to_string(),
                    )
                    .await?,
                ))
            }
            None => Ok(Arc::new(DummyCgdmChangesetDivider)),
        }
    }

    pub async fn commit_graph_writer(
        &self,
        repo_identity: &ArcRepoIdentity,
        repo_config: &ArcRepoConfig,
        commit_graph: &CommitGraph,
    ) -> Result<ArcCommitGraphWriter> {
        let scuba_table = repo_config.commit_graph_config.scuba_table.as_deref();
        let scuba = match scuba_table {
            Some(scuba_table) => MononokeScubaSampleBuilder::new(self.env.fb, scuba_table)
                .with_context(
                    || "Couldn't create scuba sample builder for table mononoke_commit_graph",
                )?,
            None => MononokeScubaSampleBuilder::with_discard(),
        };

        let base_writer = Arc::new(BaseCommitGraphWriter::new(commit_graph.clone()));

        Ok(Arc::new(LoggingCommitGraphWriter::new(
            base_writer,
            scuba,
            repo_identity.name().to_string(),
        )))
    }

    pub async fn commit_graph_bulk_fetcher(
        &self,
        repo_identity: &ArcRepoIdentity,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcCommitGraphBulkFetcher> {
        let sql_storage = self
            .open_sql::<SqlCommitGraphStorageBuilder>(repo_config)
            .await?
            .build(self.env.rendezvous_options, repo_identity.clone());

        Ok(Arc::new(CommitGraphBulkFetcher::new(Arc::new(sql_storage))))
    }

    pub async fn bonsai_blob_mapping(
        &self,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcBonsaiBlobMapping> {
        let (_, sql_bonsai_blob_mapping) = self
            .open_sql_shardable::<SqlBonsaiBlobMapping>(repo_config)
            .await
            .context(RepoFactoryError::BonsaiBlobMapping)?;
        Ok(Arc::new(BonsaiBlobMapping {
            sql_bonsai_blob_mapping,
        }))
    }

    pub async fn commit_derived_data_mapping(
        &self,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcCommitDerivedDataMapping> {
        let (_, sql) = self
            .open_sql_shardable::<SqlCommitDerivedDataMapping>(repo_config)
            .await
            .context(RepoFactoryError::CommitDerivedDataMapping)?;
        let mapping = CommitDerivedDataMapping::new(sql);
        let mapping = match self.cache_handler_factory("commit_derived_data_mapping")? {
            Some(factory) => mapping.with_caching(factory),
            None => mapping,
        };
        Ok(Arc::new(mapping))
    }

    pub async fn repo_event_publisher(
        &self,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcRepoEventPublisher> {
        self.repo_event_publishers
            .get_or_try_init(&repo_config.metadata_cache_config, || async move {
                #[cfg(fbcode_build)]
                {
                    let event_publisher = ScribeRepoEventPublisher::new(
                        self.env.fb,
                        self.configs.clone(),
                        repo_config.metadata_cache_config.as_ref(),
                    )?;
                    Ok(Arc::new(event_publisher) as ArcRepoEventPublisher)
                }
                #[cfg(not(fbcode_build))]
                {
                    Ok(Arc::new(UnsupportedRepoEventPublisher {}) as ArcRepoEventPublisher)
                }
            })
            .await
    }

    pub async fn deletion_log(&self, repo_config: &ArcRepoConfig) -> Result<ArcDeletionLog> {
        let sql_deletion_log = self
            .open_sql::<SqlDeletionLog>(repo_config)
            .await
            .context(RepoFactoryError::SqlDeletionLog)?;
        Ok(Arc::new(DeletionLog { sql_deletion_log }))
    }

    pub async fn commit_cloud(
        &self,
        repo_config: &ArcRepoConfig,
        bonsai_hg_mapping: &ArcBonsaiHgMapping,
        bonsai_git_mapping: &ArcBonsaiGitMapping,
        repo_derived_data: &ArcRepoDerivedData,
    ) -> Result<ArcCommitCloud> {
        let sql_commit_cloud = self
            .open_sql::<SqlCommitCloudBuilder>(repo_config)
            .await
            .context(RepoFactoryError::SqlCommitCloud)?;
        Ok(Arc::new(CommitCloud::new(
            sql_commit_cloud.new(),
            bonsai_hg_mapping.clone(),
            bonsai_git_mapping.clone(),
            repo_derived_data.clone(),
            self.ctx(),
            self.env.acl_provider.clone(),
            repo_config.commit_cloud_config.clone().into(),
        )))
    }

    pub async fn push_redirection_config(
        &self,
        repo_config: &ArcRepoConfig,
        sql_query_config: &ArcSqlQueryConfig,
    ) -> Result<ArcPushRedirectionConfig> {
        let builder = self
            .open_sql::<SqlPushRedirectionConfigBuilder>(repo_config)
            .await
            .context(RepoFactoryError::PushRedirectConfig)?;

        let push_redirection_config = builder.build(sql_query_config.clone());
        Ok(Arc::new(push_redirection_config))
    }
}

fn build_scuba(
    fb: FacebookInit,
    scuba_table: Option<String>,
    reponame: &str,
) -> Result<MononokeScubaSampleBuilder> {
    let mut scuba = MononokeScubaSampleBuilder::with_opt_table(fb, scuba_table)?;
    scuba.add_common_server_data();
    scuba.add("reponame", reponame);
    Ok(scuba)
}

fn get_derivation_client(
    fb: FacebookInit,
    remote_derivation_options: RemoteDerivationOptions,
    repo_config: &ArcRepoConfig,
    repo_name: &str,
) -> Result<Option<Arc<dyn DerivationClient>>> {
    let derivation_service_client: Option<Arc<dyn DerivationClient>> =
        if remote_derivation_options.derive_remotely {
            #[cfg(fbcode_build)]
            {
                match &repo_config.derived_data_config.remote_derivation_config {
                    Some(RemoteDerivationConfig::ShardManagerTier(shard_manager_tier)) => {
                        Some(Arc::new(DerivationServiceClient::from_sm_tier_name(
                            fb,
                            shard_manager_tier.clone(),
                            repo_name.to_string(),
                        )?))
                    }
                    Some(RemoteDerivationConfig::SmcTier(smc_tier)) => Some(Arc::new(
                        DerivationServiceClient::from_tier_name(fb, smc_tier.clone())?,
                    )),
                    Some(RemoteDerivationConfig::HostPort(host_port)) => Some(Arc::new(
                        DerivationServiceClient::from_host_port(fb, host_port.clone())?,
                    )),
                    None => None,
                }
            }
            #[cfg(not(fbcode_build))]
            {
                let _ = (fb, repo_name, repo_config);
                None
            }
        } else {
            None
        };
    Ok(derivation_service_client)
}

/// The bookmark cache options for a repo, given the process-level `configured`
/// ones. Git and Perforce repos have no Bookmark Service shard, so for them a
/// Remote cache is upgraded to a local WBC, keeping the configured derived data
/// scope.
fn effective_bookmark_cache_options(
    repo_config: &RepoConfig,
    configured: &BookmarkCacheOptions,
) -> (BookmarkCacheKind, BookmarkCacheDerivedData) {
    let no_bookmark_service_shard = matches!(
        repo_config.default_commit_identity_scheme,
        CommitIdentityScheme::GIT | CommitIdentityScheme::P4
    ) || repo_config.p4_source.is_some();
    let cache_kind = if no_bookmark_service_shard
        // A Local WBC with no warmers (NoDerivation, e.g. gitimport) can't
        // warm anything; leave such callers on their configured cache kind.
        && configured.derived_data != BookmarkCacheDerivedData::NoDerivation
        // An explicitly disabled cache never queries the Bookmark Service,
        // so it stays disabled; only a Remote default is upgraded to Local.
        && configured.cache_kind != BookmarkCacheKind::Disabled
    {
        BookmarkCacheKind::Local
    } else {
        configured.cache_kind.clone()
    };
    (cache_kind, configured.derived_data.clone())
}

#[cfg(test)]
mod tests {
    use metaconfig_types::P4SourceConfig;
    use mononoke_macros::mononoke;

    use super::*;

    fn remote() -> BookmarkCacheOptions {
        BookmarkCacheOptions {
            cache_kind: BookmarkCacheKind::Remote(BookmarkCacheAddress::default()),
            derived_data: BookmarkCacheDerivedData::AllKinds,
        }
    }

    fn repo(scheme: CommitIdentityScheme, p4_source: Option<P4SourceConfig>) -> RepoConfig {
        RepoConfig {
            default_commit_identity_scheme: scheme,
            p4_source,
            ..Default::default()
        }
    }

    fn p4_source() -> Option<P4SourceConfig> {
        Some(P4SourceConfig {
            server_alias: "test1".to_string(),
            depot: "//test1/".to_string(),
        })
    }

    #[mononoke::test]
    fn test_repos_without_a_bookmark_service_shard_use_a_local_cache() {
        for repo in [
            repo(CommitIdentityScheme::GIT, None),
            repo(CommitIdentityScheme::P4, None),
            repo(CommitIdentityScheme::P4, p4_source()),
            // A mirror whose config still says HG is a Perforce repo all the same.
            repo(CommitIdentityScheme::HG, p4_source()),
        ] {
            assert_eq!(
                effective_bookmark_cache_options(&repo, &remote()),
                (BookmarkCacheKind::Local, BookmarkCacheDerivedData::AllKinds),
                "{repo:?}"
            );
        }
    }

    #[mononoke::test]
    fn test_hg_repos_keep_the_remote_cache() {
        for scheme in [
            CommitIdentityScheme::HG,
            CommitIdentityScheme::BONSAI,
            CommitIdentityScheme::UNKNOWN,
        ] {
            let (cache_kind, derived_data) =
                effective_bookmark_cache_options(&repo(scheme.clone(), None), &remote());
            assert!(
                matches!(cache_kind, BookmarkCacheKind::Remote(_)),
                "{scheme:?}"
            );
            assert_eq!(derived_data, BookmarkCacheDerivedData::AllKinds);
        }
    }

    #[mononoke::test]
    fn test_disabled_and_no_derivation_are_left_alone() {
        let repo = repo(CommitIdentityScheme::P4, p4_source());
        let disabled = BookmarkCacheOptions {
            cache_kind: BookmarkCacheKind::Disabled,
            ..remote()
        };
        assert_eq!(
            effective_bookmark_cache_options(&repo, &disabled).0,
            BookmarkCacheKind::Disabled
        );
        let no_derivation = BookmarkCacheOptions {
            derived_data: BookmarkCacheDerivedData::NoDerivation,
            ..remote()
        };
        let (cache_kind, derived_data) = effective_bookmark_cache_options(&repo, &no_derivation);
        assert!(matches!(cache_kind, BookmarkCacheKind::Remote(_)));
        assert_eq!(derived_data, BookmarkCacheDerivedData::NoDerivation);
    }
}
