/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Repository factory for tests.
#![deny(missing_docs)]

use std::collections::BTreeSet;
#[cfg(fbcode_build)]
use std::collections::HashMap;
use std::sync::Arc;

use acl_regions::ArcAclRegions;
use acl_regions::build_acl_regions;
use anyhow::Result;
use blobstore::Blobstore;
use bonsai_git_mapping::ArcBonsaiGitMapping;
use bonsai_git_mapping::SqlBonsaiGitMappingBuilder;
use bonsai_globalrev_mapping::ArcBonsaiGlobalrevMapping;
use bonsai_globalrev_mapping::SqlBonsaiGlobalrevMappingBuilder;
use bonsai_hg_mapping::ArcBonsaiHgMapping;
use bonsai_hg_mapping::SqlBonsaiHgMappingBuilder;
use bonsai_p4_mapping::ArcBonsaiP4Mapping;
use bonsai_p4_mapping::SqlBonsaiP4MappingBuilder;
use bonsai_svnrev_mapping::ArcBonsaiSvnrevMapping;
use bonsai_svnrev_mapping::SqlBonsaiSvnrevMappingBuilder;
use bonsai_tag_mapping::ArcBonsaiTagMapping;
use bonsai_tag_mapping::SqlBonsaiTagMappingBuilder;
use bookmarks::ArcBookmarkUpdateLog;
use bookmarks::ArcBookmarks;
use bookmarks::bookmark_heads_fetcher;
use bookmarks_cache::ArcBookmarksCache;
use bookmarks_cache::WarmerRequirement;
use bundle_uri::ArcGitBundleUri;
use bundle_uri::BundleUri;
use bundle_uri::LocalFSBUndleUriGenerator;
use bundle_uri::SqlGitBundleMetadataStorageBuilder;
use cacheblob::InProcessLease;
use commit_cloud::ArcCommitCloud;
use commit_cloud::CommitCloud;
use commit_cloud::sql::builder::SqlCommitCloudBuilder;
use commit_derived_data_mapping::CommitDerivedDataMapping;
use commit_derived_data_mapping::SqlCommitDerivedDataMapping;
use commit_graph::ArcCommitGraph;
use commit_graph::ArcCommitGraphWriter;
use commit_graph::BaseCommitGraphWriter;
use commit_graph::CommitGraph;
use commit_rate_limit_config::ArcCommitRateLimit;
use commit_rate_limit_config::CommitRateLimit;
use commit_rate_limit_config::build_commit_rate_limit;
use context::CoreContext;
use dbbookmarks::ArcSqlBookmarks;
use dbbookmarks::SqlBookmarksBuilder;
use enabled_derived_data_types::ArcEnabledDerivedDataTypes;
use enabled_derived_data_types::SqlEnabledDerivedDataTypesBuilder;
use ephemeral_blobstore::ArcRepoEphemeralStore;
use ephemeral_blobstore::RepoEphemeralStore;
use fbinit::FacebookInit;
use filenodes::ArcFilenodes;
use filestore::ArcFilestoreConfig;
use filestore::FilestoreConfig;
use git_ref_content_mapping::ArcGitRefContentMapping;
use git_ref_content_mapping::SqlGitRefContentMappingBuilder;
use git_source_of_truth::ArcGitSourceOfTruthConfig;
use git_source_of_truth::SqlGitSourceOfTruthConfigBuilder;
use git_symbolic_refs::ArcGitSymbolicRefs;
use git_symbolic_refs::CachedGitSymbolicRefs;
use git_symbolic_refs::SqlGitSymbolicRefsBuilder;
use git_types::ArcCgdmChangesetDivider;
use git_types::DummyCgdmChangesetDivider;
use hook_manager::HookRepo;
use hook_manager::manager::ArcHookManager;
use hook_manager::manager::HookManager;
use live_commit_sync_config::LiveCommitSyncConfig;
use live_commit_sync_config::TestLiveCommitSyncConfig;
use maplit::hashmap;
use megarepo_mapping::MegarepoMapping;
use memblob::Memblob;
use mercurial_mutation::ArcHgMutationStore;
use mercurial_mutation::SqlHgMutationStoreBuilder;
#[cfg(fbcode_build)]
use metaconfig_parser::RepoConfigs;
#[cfg(fbcode_build)]
use metaconfig_parser::StorageConfigs;
use metaconfig_types::ArcRepoConfig;
use metaconfig_types::BlameVersion;
#[cfg(fbcode_build)]
use metaconfig_types::CommonConfig;
use metaconfig_types::ComparableRegex;
use metaconfig_types::DerivedDataConfig;
use metaconfig_types::DerivedDataTypesConfig;
use metaconfig_types::GitDeltaManifestV2Config;
use metaconfig_types::GitDeltaManifestV3Config;
use metaconfig_types::HookManagerParams;
use metaconfig_types::InferredCopyFromConfig;
use metaconfig_types::InfinitepushNamespace;
use metaconfig_types::InfinitepushParams;
use metaconfig_types::RepoConfig;
use metaconfig_types::SourceControlServiceParams;
use metaconfig_types::UnodeVersion;
#[cfg(fbcode_build)]
use mononoke_configs::MononokeConfigs;
use mononoke_types::DerivableType;
use mononoke_types::RepositoryId;
use mutable_blobstore::ArcMutableRepoBlobstore;
use mutable_blobstore::MutableRepoBlobstore;
use mutable_counters::ArcMutableCounters;
use mutable_counters::SqlMutableCountersBuilder;
use mutable_renames::ArcMutableRenames;
use mutable_renames::MutableRenames;
use mutable_renames::SqlMutableRenamesStore;
use newfilenodes::NewFilenodesBuilder;
use permission_checker::AclProvider;
use permission_checker::dummy::DummyAclProvider;
use phases::ArcPhases;
use pushrebase_mutation_mapping::ArcPushrebaseMutationMapping;
use pushrebase_mutation_mapping::SqlPushrebaseMutationMappingConnection;
use pushredirect::ArcPushRedirectionConfig;
use pushredirect::NoopPushRedirectionConfig;
use redactedblobstore::RedactedBlobs;
use rendezvous::RendezVousOptions;
use repo_blobstore::ArcRepoBlobstore;
use repo_blobstore::RepoBlobstore;
use repo_bookmark_attrs::ArcRepoBookmarkAttrs;
use repo_bookmark_attrs::RepoBookmarkAttrs;
use repo_cross_repo::ArcRepoCrossRepo;
use repo_cross_repo::RepoCrossRepo;
use repo_derivation_queues::ArcRepoDerivationQueues;
use repo_derivation_queues::RepoDerivationQueues;
use repo_derived_data::ArcRepoDerivedData;
use repo_derived_data::RepoDerivedData;
use repo_event_publisher::ArcRepoEventPublisher;
#[cfg(fbcode_build)]
use repo_event_publisher::ScribeRepoEventPublisher;
#[cfg(not(fbcode_build))]
use repo_event_publisher::UnsupportedRepoEventPublisher;
use repo_identity::ArcRepoIdentity;
use repo_identity::RepoIdentity;
use repo_lock::AlwaysUnlockedRepoLock;
use repo_lock::ArcRepoLock;
use repo_lock::SqlRepoLock;
use repo_manifest_mapping::ArcRepoManifestMapping;
use repo_manifest_mapping::SqlRepoManifestMappingBuilder;
use repo_metadata_checkpoint::ArcRepoMetadataCheckpoint;
use repo_metadata_checkpoint::SqlRepoMetadataCheckpointBuilder;
use repo_permission_checker::AlwaysAllowRepoPermissionChecker;
use repo_permission_checker::ArcRepoPermissionChecker;
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
use sql::rusqlite::Connection as SqliteConnection;
use sql::sqlite::SqliteCallbacks;
use sql_commit_graph_storage::ArcCommitGraphBulkFetcher;
use sql_commit_graph_storage::CommitGraphBulkFetcher;
use sql_commit_graph_storage::SqlCommitGraphStorageBuilder;
use sql_construct::SqlConstruct;
use sql_ext::Connection;
use sql_ext::SqlConnections;
use sql_query_config::ArcSqlQueryConfig;
use sql_query_config::SqlQueryConfig;
use sqlphases::SqlPhasesBuilder;
use streaming_clone::ArcStreamingClone;
use streaming_clone::StreamingCloneBuilder;
use strum::IntoEnumIterator;
use synced_commit_mapping::ArcSyncedCommitMapping;
use synced_commit_mapping::SqlSyncedCommitMappingBuilder;
use warm_bookmarks_cache::WarmBookmarksCacheBuilder;
use wireproto_handler::ArcRepoHandlerBase;
use wireproto_handler::PushRedirectorBase;
use wireproto_handler::RepoHandlerBase;
use wireproto_handler::TargetRepoDbs;

/// Factory to construct test repositories.
///
/// This factory acts as a long-lived builder which can produce multiple
/// repositories with shared back-end storage or based on similar or the same
/// config.
///
/// By default, it will use a single in-memory blobstore and a single
/// in-memory metadata database for all repositories.
pub struct TestRepoFactory {
    /// Sometimes needed to construct a facet
    pub fb: FacebookInit,
    ctx: CoreContext,
    name: String,
    config: RepoConfig,
    blobstore: Arc<dyn Blobstore>,
    mutable_blobstore: Arc<dyn Blobstore>,
    bookmarks_cache: Option<ArcBookmarksCache>,
    git_symbolic_refs: Option<ArcGitSymbolicRefs>,
    live_commit_sync_config: Option<Arc<dyn LiveCommitSyncConfig>>,
    metadata_db: SqlConnections,
    hg_mutation_db: SqlConnections,
    redacted: Option<Arc<RedactedBlobs>>,
    permission_checker: Option<ArcRepoPermissionChecker>,
    filenodes_override: Option<Box<dyn Fn(ArcFilenodes) -> ArcFilenodes + Send + Sync>>,
    restricted_paths: Option<ArcRestrictedPaths>,
    acl_provider: Option<Arc<dyn AclProvider>>,
    repo_derivation_queues: Option<ArcRepoDerivationQueues>,
}

/// The default derived data types configuration for test repositories.
///
/// This configuration enables all derived data types at the latest version.
pub fn default_test_repo_derived_data_types_config() -> DerivedDataTypesConfig {
    DerivedDataTypesConfig {
        types: DerivableType::iter().collect(),
        unode_version: UnodeVersion::V2,
        blame_version: BlameVersion::V2,
        git_delta_manifest_v2_config: Some(GitDeltaManifestV2Config {
            max_inlined_object_size: 100,
            max_inlined_delta_size: 100,
            delta_chunk_size: 1000,
        }),
        git_delta_manifest_v3_config: Some(GitDeltaManifestV3Config {
            max_inlined_object_size: 100,
            max_inlined_delta_size: 100,
            delta_chunk_size: 1000,
            entry_chunk_size: 1000,
        }),
        inferred_copy_from_config: Some(InferredCopyFromConfig {
            dir_level_for_basename_lookup: 1,
            basename_match_max_candidates: 10_000,
            partial_match_max_file_size: 4 * 1024 * 1024,
            max_num_changed_files: 6_000,
            partial_match_skip_file_extensions: BTreeSet::from([".obj".to_string()]),
        }),
        xdb_mapping_shard_ids: hashmap! {
            DerivableType::HistoryManifests => 0,
            DerivableType::BlameV3 => 0,
            DerivableType::FastlogV2 => 0,
        },
        ..Default::default()
    }
}

/// The default configuration for test repositories.
///
/// This configuration enables all derived data types at the latest version.
pub fn default_test_repo_config() -> RepoConfig {
    let derived_data_types_config = default_test_repo_derived_data_types_config();
    RepoConfig {
        derived_data_config: DerivedDataConfig {
            enabled_config_name: "default".to_string(),
            available_configs: hashmap![
                "default".to_string() => derived_data_types_config.clone(),
                "backfilling".to_string() => derived_data_types_config
            ],
            ..Default::default()
        },
        infinitepush: InfinitepushParams {
            namespace: Some(InfinitepushNamespace::new(
                ComparableRegex::new("scratch/.+").unwrap(),
            )),
            ..Default::default()
        },
        source_control_service: SourceControlServiceParams {
            permit_writes: true,
            ..Default::default()
        },
        hook_manager_params: Some(HookManagerParams {
            disable_acl_checker: true,
            ..Default::default()
        }),
        hook_max_file_size: 1000000,
        ..Default::default()
    }
}

/// Create an empty in-memory repo for tests.
///
/// This covers the simplest case.  For more complicated repositories, use
/// `TestRepoFactory`.
pub async fn build_empty<R>(fb: FacebookInit) -> Result<R>
where
    R: for<'builder> facet::AsyncBuildable<'builder, TestRepoFactoryBuilder<'builder>>,
{
    Ok(TestRepoFactory::new(fb)?.build().await?)
}

impl TestRepoFactory {
    /// Create a new factory for test repositories with default settings.
    pub fn new(fb: FacebookInit) -> Result<TestRepoFactory> {
        Self::with_sqlite_connection_callbacks(
            fb,
            SqliteConnection::open_in_memory()?,
            SqliteConnection::open_in_memory()?,
            None,
        )
    }
    /// Create a new factory for test repositories with an existing Sqlite
    /// connection.
    pub fn with_sqlite_connection(
        fb: FacebookInit,
        metadata_con: SqliteConnection,
        hg_mutation_con: SqliteConnection,
    ) -> Result<TestRepoFactory> {
        Self::with_sqlite_connection_callbacks(fb, metadata_con, hg_mutation_con, None)
    }

    /// Create a new factory for test repositories with an existing Sqlite
    /// connection.
    pub fn with_sqlite_connection_callbacks(
        fb: FacebookInit,
        metadata_con: SqliteConnection,
        hg_mutation_con: SqliteConnection,
        callbacks: Option<Box<dyn SqliteCallbacks>>,
    ) -> Result<TestRepoFactory> {
        metadata_con.execute_batch(MegarepoMapping::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlMutableCountersBuilder::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlBookmarksBuilder::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlBonsaiGitMappingBuilder::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlBonsaiGlobalrevMappingBuilder::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlBonsaiP4MappingBuilder::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlBonsaiSvnrevMappingBuilder::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlBonsaiTagMappingBuilder::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlBonsaiHgMappingBuilder::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlGitRefContentMappingBuilder::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlGitSymbolicRefsBuilder::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlPhasesBuilder::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlPushrebaseMutationMappingConnection::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlMutableRenamesStore::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlSyncedCommitMappingBuilder::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlRepoLock::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlSparseProfilesSizes::CREATION_QUERY)?;
        metadata_con.execute_batch(StreamingCloneBuilder::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlCommitDerivedDataMapping::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlCommitGraphStorageBuilder::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlCommitCloudBuilder::CREATION_QUERY)?;
        metadata_con.execute_batch(SqlRestrictedPathsManifestIdStoreBuilder::CREATION_QUERY)?;

        let metadata_db = SqlConnections::new_single(match callbacks {
            Some(callbacks) => Connection::with_sqlite_callbacks(metadata_con, callbacks)?,
            None => Connection::with_sqlite(metadata_con)?,
        });

        hg_mutation_con.execute_batch(SqlHgMutationStoreBuilder::CREATION_QUERY)?;
        let hg_mutation_db = SqlConnections::new_single(Connection::with_sqlite(hg_mutation_con)?);

        Ok(TestRepoFactory {
            fb,
            ctx: CoreContext::test_mock(fb),
            name: "repo".to_string(),
            config: default_test_repo_config(),
            blobstore: Arc::new(Memblob::default()),
            mutable_blobstore: Arc::new(Memblob::default()),
            metadata_db,
            hg_mutation_db,
            redacted: None,
            permission_checker: None,
            filenodes_override: None,
            live_commit_sync_config: None,
            bookmarks_cache: None,
            git_symbolic_refs: None,
            restricted_paths: None,
            acl_provider: None,
            repo_derivation_queues: None,
        })
    }

    /// Get the metadata database this factory is using for repositories.
    pub fn metadata_db(&self) -> &SqlConnections {
        &self.metadata_db
    }

    /// Set the name for the next repository being built.
    pub fn with_name(&mut self, name: impl Into<String>) -> &mut Self {
        self.name = name.into();
        self
    }

    /// Set the ID for the next repository being built.
    pub fn with_id(&mut self, id: RepositoryId) -> &mut Self {
        self.config.repoid = id;
        self
    }

    /// Use a particular blobstore for repos built by this factory.
    pub fn with_blobstore(&mut self, blobstore: Arc<dyn Blobstore>) -> &mut Self {
        self.blobstore = blobstore;
        self
    }

    /// Set the bookmarks cache for repos built by this factory.
    pub fn with_bookmarks_cache(&mut self, bookmarks_cache: ArcBookmarksCache) -> &mut Self {
        self.bookmarks_cache = Some(bookmarks_cache);
        self
    }

    /// Set the restricted paths for repos built by this factory.
    pub fn with_restricted_paths(&mut self, restricted_paths: ArcRestrictedPaths) -> &mut Self {
        self.restricted_paths = Some(restricted_paths);
        self
    }

    /// Use a custom ACL provider for the `restricted_paths` facet of repos built
    /// by this factory. Ignored when an explicit `restricted_paths` is set via
    /// [`Self::with_restricted_paths`].
    pub fn with_acl_provider(&mut self, acl_provider: Arc<dyn AclProvider>) -> &mut Self {
        self.acl_provider = Some(acl_provider);
        self
    }

    /// Set the derivation queues for repos built by this factory.
    pub fn with_derivation_queues(
        &mut self,
        repo_derivation_queues: ArcRepoDerivationQueues,
    ) -> &mut Self {
        self.repo_derivation_queues = Some(repo_derivation_queues);
        self
    }

    /// Redact content in repos that are built by this factory.
    pub fn redacted(&mut self, redacted: Option<RedactedBlobs>) -> &mut Self {
        self.redacted = redacted.map(Arc::new);
        self
    }

    /// Set a custom permission checker
    pub fn with_permission_checker(
        &mut self,
        permission_checker: ArcRepoPermissionChecker,
    ) -> &mut Self {
        self.permission_checker = Some(permission_checker);
        self
    }

    /// Modify the config of the repo.
    pub fn with_config_override(&mut self, modify: impl FnOnce(&mut RepoConfig)) -> &mut Self {
        modify(&mut self.config);
        self
    }

    /// Override the live commit sync config used by factor.
    pub fn with_live_commit_sync_config(
        &mut self,
        live_commit_sync_config: Arc<dyn LiveCommitSyncConfig>,
    ) -> &mut Self {
        self.live_commit_sync_config = Some(live_commit_sync_config);
        self
    }

    /// Override filenodes.
    pub fn with_filenodes_override(
        &mut self,
        filenodes_override: impl Fn(ArcFilenodes) -> ArcFilenodes + Send + Sync + 'static,
    ) -> &mut Self {
        self.filenodes_override = Some(Box::new(filenodes_override));
        self
    }

    /// Override git symbolic refs with a cache-less variant.
    pub fn with_cacheless_git_symbolic_refs(&mut self) -> &mut Self {
        let git_symbolic_refs =
            SqlGitSymbolicRefsBuilder::from_sql_connections(self.metadata_db.clone())
                .build(self.config.repoid);
        self.git_symbolic_refs = Some(Arc::new(git_symbolic_refs));
        self
    }

    /// Override core context. BEWARE that using this can impact default
    /// behaviour needed for testing (e.g. logging).
    /// This was exposed so that TestRepoFactory can be used to create temporary
    /// repositories with configurations similar to the ones needed for testing,
    /// (e.g. local file-based storage) while avoiding code duplication.
    /// For more details, see D48946892.
    ///
    /// If you're building repos for testing, you likely do NOT want to use it.
    pub fn with_core_context_that_does_not_override_logger(
        &mut self,
        ctx: CoreContext,
    ) -> &mut Self {
        self.ctx = ctx;
        self
    }

    /// Function to create megarepo mapping from the same connection as other DBs
    pub fn megarepo_mapping(&self) -> Arc<MegarepoMapping> {
        Arc::new(MegarepoMapping::from_sql_connections(
            self.metadata_db.clone(),
        ))
    }
}

#[facet::factory()]
impl TestRepoFactory {
    /// Construct RepoConfig based on the config in the factory.
    pub fn repo_config(&self) -> ArcRepoConfig {
        Arc::new(self.config.clone())
    }

    /// Construct RepoIdentity based on the config and name in the factory.
    pub fn repo_identity(&self, repo_config: &ArcRepoConfig) -> ArcRepoIdentity {
        Arc::new(RepoIdentity::new(repo_config.repoid, self.name.clone()))
    }

    /// Construct SQL bookmarks using the in-memory metadata database.
    pub fn sql_bookmarks(&self, repo_identity: &ArcRepoIdentity) -> Result<ArcSqlBookmarks> {
        Ok(Arc::new(
            SqlBookmarksBuilder::from_sql_connections(self.metadata_db.clone())
                .with_repo_id(repo_identity.id()),
        ))
    }

    /// Construct Bookmarks.
    pub fn bookmarks(&self, sql_bookmarks: &ArcSqlBookmarks) -> ArcBookmarks {
        sql_bookmarks.clone()
    }

    /// Construct Bookmark update log.
    pub fn bookmark_update_log(&self, sql_bookmarks: &ArcSqlBookmarks) -> ArcBookmarkUpdateLog {
        sql_bookmarks.clone()
    }

    /// Construct Phases.
    pub fn phases(
        &self,
        repo_identity: &ArcRepoIdentity,
        bookmarks: &ArcBookmarks,
        commit_graph: &ArcCommitGraph,
    ) -> ArcPhases {
        let sql_phases_builder = SqlPhasesBuilder::from_sql_connections(self.metadata_db.clone());
        let heads_fetcher = bookmark_heads_fetcher(bookmarks.clone());
        sql_phases_builder.build(
            repo_identity.id(),
            commit_graph.clone(),
            heads_fetcher,
            RendezVousOptions::for_test(),
        )
    }

    /// Construct Bonsai Hg Mapping using the in-memory metadata database.
    pub fn bonsai_hg_mapping(&self, repo_identity: &ArcRepoIdentity) -> Result<ArcBonsaiHgMapping> {
        Ok(Arc::new(
            SqlBonsaiHgMappingBuilder::from_sql_connections(self.metadata_db.clone())
                .build(repo_identity.id(), RendezVousOptions::for_test()),
        ))
    }

    /// Construct Bonsai Git Mapping using the in-memory metadata database.
    pub fn bonsai_git_mapping(
        &self,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcBonsaiGitMapping> {
        Ok(Arc::new(
            SqlBonsaiGitMappingBuilder::from_sql_connections(self.metadata_db.clone())
                .build(repo_identity.id(), RendezVousOptions::for_test()),
        ))
    }

    /// Construct Bonsai Globalrev Mapping using the in-memory metadata
    /// database.
    pub fn bonsai_globalrev_mapping(
        &self,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcBonsaiGlobalrevMapping> {
        Ok(Arc::new(
            SqlBonsaiGlobalrevMappingBuilder::from_sql_connections(self.metadata_db.clone())
                .build(RendezVousOptions::for_test(), repo_identity.id()),
        ))
    }

    /// Construct Bonsai P4 Mapping using the in-memory metadata
    /// database.
    pub fn bonsai_p4_mapping(&self, repo_identity: &ArcRepoIdentity) -> Result<ArcBonsaiP4Mapping> {
        Ok(Arc::new(
            SqlBonsaiP4MappingBuilder::from_sql_connections(self.metadata_db.clone())
                .build(repo_identity.id()),
        ))
    }

    /// Construct Bonsai Svnrev Mapping using the in-memory metadata
    /// database.
    pub fn bonsai_svnrev_mapping(
        &self,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcBonsaiSvnrevMapping> {
        Ok(Arc::new(
            SqlBonsaiSvnrevMappingBuilder::from_sql_connections(self.metadata_db.clone())
                .build(repo_identity.id()),
        ))
    }

    /// Construct Bonsai Tag Mapping using the in-memory metadata
    /// database.
    pub fn bonsai_tag_mapping(
        &self,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcBonsaiTagMapping> {
        Ok(Arc::new(
            SqlBonsaiTagMappingBuilder::from_sql_connections(self.metadata_db.clone())
                .build(repo_identity.id(), RendezVousOptions::for_test()),
        ))
    }

    /// Construct Git Ref Content Mapping using the in-memory metadata
    /// database.
    pub fn git_ref_content_mapping(
        &self,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcGitRefContentMapping> {
        Ok(Arc::new(
            SqlGitRefContentMappingBuilder::from_sql_connections(self.metadata_db.clone())
                .build(repo_identity.id()),
        ))
    }

    /// Construct Git Bundle Uri object with in-memory git bundle metadata database
    /// and local FS urls returned.
    pub async fn git_bundle_uri(&self, repo_identity: &ArcRepoIdentity) -> Result<ArcGitBundleUri> {
        let storage =
            SqlGitBundleMetadataStorageBuilder::from_sql_connections(self.metadata_db.clone())
                .build(repo_identity.id());

        let generator = LocalFSBUndleUriGenerator {};
        let bundle_uri = BundleUri::new(storage, generator, repo_identity.id()).await?;
        Ok(Arc::new(bundle_uri))
    }

    /// Construct Repo Metadata Checkpoint using the in-memory metadata
    pub fn repo_metadata_checkpoint(
        &self,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcRepoMetadataCheckpoint> {
        Ok(Arc::new(
            SqlRepoMetadataCheckpointBuilder::from_sql_connections(self.metadata_db.clone())
                .build(repo_identity.id(), self.ctx.sql_query_telemetry()),
        ))
    }

    /// Construct Git Symbolic Refs using the in-memory metadata
    /// database.
    pub async fn git_symbolic_refs(
        &self,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcGitSymbolicRefs> {
        if let Some(git_symbolic_refs) = &self.git_symbolic_refs {
            Ok(git_symbolic_refs.clone())
        } else {
            let git_symbolic_refs =
                SqlGitSymbolicRefsBuilder::from_sql_connections(self.metadata_db.clone())
                    .build(repo_identity.id());
            let cached_symbolic_refs =
                CachedGitSymbolicRefs::new(&self.ctx, Arc::new(git_symbolic_refs)).await?;
            Ok(Arc::new(cached_symbolic_refs))
        }
    }

    /// Construct Git Push Redirect Config using the in-memory metadata
    /// database.
    pub fn git_source_of_truth_config(&self) -> Result<ArcGitSourceOfTruthConfig> {
        Ok(Arc::new(
            SqlGitSourceOfTruthConfigBuilder::from_sql_connections(self.metadata_db.clone())
                .build(),
        ))
    }

    /// Construct the repo-manifest routing store using the in-memory metadata
    /// database.
    pub fn repo_manifest_mapping(&self) -> Result<ArcRepoManifestMapping> {
        Ok(Arc::new(
            SqlRepoManifestMappingBuilder::from_sql_connections(self.metadata_db.clone()).build(),
        ))
    }

    /// Construct the enabled-derived-data-types facet using the in-memory
    /// metadata database.
    pub fn enabled_derived_data_types(&self) -> Result<ArcEnabledDerivedDataTypes> {
        Ok(Arc::new(
            SqlEnabledDerivedDataTypesBuilder::from_sql_connections(self.metadata_db.clone())
                .build(),
        ))
    }

    /// Construct Pushrebase Mutation Mapping using the in-memory metadata
    /// database.
    pub fn pushrebase_mutation_mapping(
        &self,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcPushrebaseMutationMapping> {
        Ok(Arc::new(
            SqlPushrebaseMutationMappingConnection::from_sql_connections(self.metadata_db.clone())
                .with_repo_id(repo_identity.id()),
        ))
    }

    /// Construct permission checker.  By default this allows all access.
    pub fn permission_checker(&self) -> Result<ArcRepoPermissionChecker> {
        if let Some(permission_checker) = &self.permission_checker {
            Ok(permission_checker.clone())
        } else {
            let permission_checker = AlwaysAllowRepoPermissionChecker::new();
            Ok(Arc::new(permission_checker))
        }
    }

    /// Construct Filenodes.
    ///
    /// Filenodes do not use the metadata db (each repo has its own filenodes
    /// db in memory).
    pub fn filenodes(&self, repo_identity: &ArcRepoIdentity) -> Result<ArcFilenodes> {
        let mut filenodes: ArcFilenodes =
            Arc::new(NewFilenodesBuilder::with_sqlite_in_memory()?.build(repo_identity.id())?);
        if let Some(filenodes_override) = &self.filenodes_override {
            filenodes = filenodes_override(filenodes);
        }
        Ok(filenodes)
    }

    /// Construct Mercurial Mutation Store using the in-memory hg_mutation
    /// database.
    pub fn hg_mutation_store(&self, repo_identity: &ArcRepoIdentity) -> Result<ArcHgMutationStore> {
        Ok(Arc::new(
            SqlHgMutationStoreBuilder::from_sql_connections(self.hg_mutation_db.clone())
                .with_repo_id(repo_identity.id()),
        ))
    }

    /// Construct RepoDerivedData
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
    ) -> Result<ArcRepoDerivedData> {
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
            MononokeScubaSampleBuilder::with_discard(),
            repo_config.derived_data_config.clone(),
            None, // derivation_service_client = None
            restricted_paths_config_based.clone(),
            Arc::new(CommitDerivedDataMapping::new(
                SqlCommitDerivedDataMapping::from_sql_connections(self.metadata_db.clone()),
            )),
        )?))
    }

    /// Construct the RepoBlobstore using the blobstore in the factory.
    pub fn repo_blobstore(&self, repo_identity: &ArcRepoIdentity) -> ArcRepoBlobstore {
        let repo_blobstore = RepoBlobstore::new(
            self.blobstore.clone(),
            self.redacted.clone(),
            repo_identity.id(),
            MononokeScubaSampleBuilder::with_discard(),
        );
        Arc::new(repo_blobstore)
    }

    /// Construct the MutableRepoBlobstore using the blobstore in the factory.
    pub fn mutable_repo_blobstore(
        &self,
        repo_identity: &ArcRepoIdentity,
    ) -> ArcMutableRepoBlobstore {
        let mutable_repo_blobstore =
            MutableRepoBlobstore::new(self.mutable_blobstore.clone(), repo_identity.id());
        Arc::new(mutable_repo_blobstore)
    }

    /// Construct filestore config based on the config in the factory.
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

    /// Disabled ephemeral repo
    pub fn repo_ephemeral_store(&self, repo_identity: &ArcRepoIdentity) -> ArcRepoEphemeralStore {
        Arc::new(RepoEphemeralStore::disabled(repo_identity.id()))
    }

    /// Mutable renames
    pub fn mutable_renames(&self, repo_identity: &ArcRepoIdentity) -> Result<ArcMutableRenames> {
        let sql_store = SqlMutableRenamesStore::from_sql_connections(self.metadata_db.clone());
        Ok(Arc::new(MutableRenames::new_test(
            repo_identity.id(),
            sql_store,
        )))
    }

    /// Build config-based restricted paths
    pub async fn restricted_paths_config_based(
        &self,
        repo_config: &ArcRepoConfig,
        restricted_paths_manifest_id_store: &ArcRestrictedPathsManifestIdStore,
    ) -> Result<ArcRestrictedPathsConfigBased> {
        // If a pre-built RestrictedPaths was provided via with_restricted_paths,
        // use its config_based to ensure repo_derived_data gets the same config.
        if let Some(restricted_paths) = &self.restricted_paths {
            return Ok(restricted_paths.config_based().clone());
        }

        let restricted_paths_config = repo_config.restricted_paths_config.clone();

        let cache = RestrictedPathsManifestIdCacheBuilder::new(
            self.ctx.clone(),
            restricted_paths_manifest_id_store.clone(),
        )
        .with_manifest_id_store_cache(restricted_paths_config.manifest_id_store_config.clone())
        .build()
        .await?;

        Ok(Arc::new(RestrictedPathsConfigBased::new(
            restricted_paths_config,
            restricted_paths_manifest_id_store.clone(),
            Some(Arc::new(cache)),
        )))
    }

    /// Restricted paths
    pub fn restricted_paths(
        &self,
        restricted_paths_config_based: &ArcRestrictedPathsConfigBased,
        repo_derived_data: &ArcRepoDerivedData,
    ) -> Result<ArcRestrictedPaths> {
        if let Some(restricted_paths) = &self.restricted_paths {
            return Ok(restricted_paths.clone());
        }

        let acl_provider = match &self.acl_provider {
            Some(acl_provider) => acl_provider.clone(),
            None => DummyAclProvider::new(self.fb)?,
        };

        // Create scuba builder with discard for tests
        let scuba_builder = MononokeScubaSampleBuilder::with_discard();

        Ok(Arc::new(RestrictedPaths::new(
            restricted_paths_config_based.clone(),
            acl_provider,
            scuba_builder,
            repo_derived_data.clone(),
        )?))
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
    pub fn restricted_paths_manifest_id_store(
        &self,
        repo_identity: &ArcRepoIdentity,
    ) -> ArcRestrictedPathsManifestIdStore {
        Arc::new(
            SqlRestrictedPathsManifestIdStoreBuilder::from_sql_connections(
                self.metadata_db.clone(),
            )
            .with_repo_id(repo_identity.id()),
        )
    }

    /// The commit mapping between repos for synced commits.
    pub fn synced_commit_mapping(&self) -> Result<ArcSyncedCommitMapping> {
        Ok(Arc::new(
            SqlSyncedCommitMappingBuilder::from_sql_connections(self.metadata_db.clone())
                .build(RendezVousOptions::for_test()),
        ))
    }

    /// Cross-repo sync manager for this repo
    pub async fn repo_cross_repo(
        &self,
        synced_commit_mapping: &ArcSyncedCommitMapping,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcRepoCrossRepo> {
        let live_commit_sync_config = self
            .live_commit_sync_config
            .clone()
            .unwrap_or_else(|| Arc::new(TestLiveCommitSyncConfig::new_empty()));
        let sync_lease = Arc::new(InProcessLease::new());
        let repo_xrepo = RepoCrossRepo::new(
            synced_commit_mapping.clone(),
            live_commit_sync_config,
            sync_lease,
            repo_identity.id(),
        )
        .await?;

        Ok(Arc::new(repo_xrepo))
    }

    /// Test repo-handler-base
    pub fn repo_handler_base(
        &self,
        _repo_config: &ArcRepoConfig,
        repo_cross_repo: &ArcRepoCrossRepo,
        repo_identity: &ArcRepoIdentity,
        bookmarks: &ArcBookmarks,
        bookmark_update_log: &ArcBookmarkUpdateLog,
        mutable_counters: &ArcMutableCounters,
    ) -> Result<ArcRepoHandlerBase> {
        let scuba = self.ctx.scuba().clone();

        let common_commit_sync_config = repo_cross_repo
            .live_commit_sync_config()
            .clone()
            .get_common_config_if_exists(repo_identity.id())?;
        let synced_commit_mapping = repo_cross_repo.synced_commit_mapping();
        let target_repo_dbs = Arc::new(TargetRepoDbs {
            bookmarks: bookmarks.clone(),
            bookmark_update_log: bookmark_update_log.clone(),
            counters: mutable_counters.clone(),
        });

        let maybe_push_redirector_base =
            common_commit_sync_config.map(|common_commit_sync_config| {
                Arc::new(PushRedirectorBase {
                    common_commit_sync_config,
                    target_repo_dbs,
                    synced_commit_mapping: synced_commit_mapping.clone(),
                })
            });
        Ok(Arc::new(RepoHandlerBase {
            scuba,
            maybe_push_redirector_base,
        }))
    }

    /// Mutable counters
    pub fn mutable_counters(&self, repo_identity: &ArcRepoIdentity) -> Result<ArcMutableCounters> {
        Ok(Arc::new(
            SqlMutableCountersBuilder::from_sql_connections(self.metadata_db.clone())
                .build(repo_identity.id()),
        ))
    }

    /// ACL regions
    pub fn acl_regions(
        &self,
        repo_config: &ArcRepoConfig,
        commit_graph: &ArcCommitGraph,
    ) -> ArcAclRegions {
        build_acl_regions(repo_config.acl_region_config.as_ref(), commit_graph.clone())
    }

    /// Hook manager
    pub fn hook_manager(
        &self,
        repo_identity: &ArcRepoIdentity,
        repo_blobstore: &ArcRepoBlobstore,
        repo_config: &ArcRepoConfig,
        repo_derived_data: &ArcRepoDerivedData,
        bookmarks: &ArcBookmarks,
        bonsai_tag_mapping: &ArcBonsaiTagMapping,
        bonsai_git_mapping: &ArcBonsaiGitMapping,
        repo_cross_repo: &ArcRepoCrossRepo,
        commit_graph: &ArcCommitGraph,
        restricted_paths: &ArcRestrictedPaths,
        commit_rate_limit: &ArcCommitRateLimit,
    ) -> ArcHookManager {
        let hook_repo = HookRepo {
            repo_identity: repo_identity.clone(),
            repo_config: repo_config.clone(),
            repo_blobstore: repo_blobstore.clone(),
            bookmarks: bookmarks.clone(),
            repo_derived_data: repo_derived_data.clone(),
            bonsai_git_mapping: bonsai_git_mapping.clone(),
            bonsai_tag_mapping: bonsai_tag_mapping.clone(),
            repo_cross_repo: repo_cross_repo.clone(),
            commit_graph: commit_graph.clone(),
            restricted_paths: restricted_paths.clone(),
            commit_rate_limit: commit_rate_limit.clone(),
        };

        Arc::new(HookManager::new_test(
            repo_identity.name().to_string(),
            hook_repo,
        ))
    }

    /// Sparse profiles
    pub fn sparse_profile(&self, _repo_config: &ArcRepoConfig) -> ArcRepoSparseProfiles {
        Arc::new(RepoSparseProfiles {
            sql_profile_sizes: None,
        })
    }

    /// Construct unlocked repo lock.
    pub fn repo_lock(&self, repo_identity: &ArcRepoIdentity) -> Result<ArcRepoLock> {
        let repo_lock = AlwaysUnlockedRepoLock::new(repo_identity.id());
        Ok(Arc::new(repo_lock))
    }

    /// Repo bookmark attrs
    pub fn repo_bookmark_attrs(&self, repo_config: &ArcRepoConfig) -> Result<ArcRepoBookmarkAttrs> {
        Ok(Arc::new(RepoBookmarkAttrs::new_test(
            repo_config.bookmarks.clone(),
        )?))
    }

    /// Repo event publisher
    pub async fn repo_event_publisher(
        &self,
        repo_config: &ArcRepoConfig,
    ) -> Result<ArcRepoEventPublisher> {
        #[cfg(fbcode_build)]
        {
            let configs = Arc::new(MononokeConfigs::new_static(
                RepoConfigs::new(
                    HashMap::from([(self.name.clone(), repo_config.as_ref().clone())]),
                    CommonConfig::default(),
                ),
                StorageConfigs {
                    storage: HashMap::new(),
                },
            ));
            let event_publisher = ScribeRepoEventPublisher::new(
                self.fb,
                configs,
                repo_config.metadata_cache_config.as_ref(),
            )?;
            Ok(Arc::new(event_publisher))
        }
        #[cfg(not(fbcode_build))]
        {
            let _ = repo_config;
            Ok(Arc::new(UnsupportedRepoEventPublisher {}))
        }
    }

    /// Streaming clone
    pub fn streaming_clone(
        &self,
        repo_identity: &ArcRepoIdentity,
        mutable_repo_blobstore: &ArcMutableRepoBlobstore,
    ) -> ArcStreamingClone {
        Arc::new(
            StreamingCloneBuilder::from_sql_connections(self.metadata_db.clone())
                .build(repo_identity.id(), mutable_repo_blobstore.clone()),
        )
    }

    /// Sql query config
    pub fn sql_query_config(&self) -> ArcSqlQueryConfig {
        Arc::new(SqlQueryConfig { caching: None })
    }

    /// Commit graph
    pub fn commit_graph(&self, repo_identity: &ArcRepoIdentity) -> Result<ArcCommitGraph> {
        let sql_storage =
            SqlCommitGraphStorageBuilder::from_sql_connections(self.metadata_db.clone())
                .build(RendezVousOptions::for_test(), repo_identity.clone());
        Ok(Arc::new(CommitGraph::new(Arc::new(sql_storage))))
    }

    /// CGDM changeset divider
    pub fn cgdm_changeset_divider(&self) -> Result<ArcCgdmChangesetDivider> {
        Ok(Arc::new(DummyCgdmChangesetDivider))
    }

    /// Commit graph writer
    pub fn commit_graph_writer(&self, commit_graph: &CommitGraph) -> Result<ArcCommitGraphWriter> {
        let base_writer = BaseCommitGraphWriter::new(commit_graph.clone());
        Ok(Arc::new(base_writer))
    }

    /// Commit graph bulk fetcher
    pub fn commit_graph_bulk_fetcher(
        &self,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcCommitGraphBulkFetcher> {
        let sql_storage =
            SqlCommitGraphStorageBuilder::from_sql_connections(self.metadata_db.clone())
                .build(RendezVousOptions::for_test(), repo_identity.clone());

        Ok(Arc::new(CommitGraphBulkFetcher::new(Arc::new(sql_storage))))
    }

    /// Warm bookmarks cache
    pub async fn warm_bookmarks_cache(
        &self,
        repo_identity: &ArcRepoIdentity,
        bookmarks: &ArcBookmarks,
        bookmark_update_log: &ArcBookmarkUpdateLog,
        repo_derived_data: &ArcRepoDerivedData,
        repo_event_publisher: &ArcRepoEventPublisher,
        phases: &ArcPhases,
    ) -> Result<ArcBookmarksCache> {
        match self.bookmarks_cache {
            Some(ref cache) => Ok(cache.clone()),
            None => {
                let mut warm_bookmarks_cache_builder = WarmBookmarksCacheBuilder::new(
                    self.ctx.clone(),
                    bookmarks.clone(),
                    bookmark_update_log.clone(),
                    repo_identity.clone(),
                    repo_event_publisher.clone(),
                    WarmerRequirement::AllKinds,
                );
                warm_bookmarks_cache_builder.add_all_warmers(repo_derived_data, phases)?;
                warm_bookmarks_cache_builder.wait_until_warmed();
                Ok(Arc::new(warm_bookmarks_cache_builder.build().await?))
            }
        }
    }

    /// Commit cloud
    pub fn commit_cloud(
        &self,
        _repo_identity: &RepoIdentity,
        bonsai_hg_mapping: &ArcBonsaiHgMapping,
        repo_derived_data: &ArcRepoDerivedData,
        bonsai_git_mapping: &ArcBonsaiGitMapping,
    ) -> Result<ArcCommitCloud> {
        let cc = CommitCloud::new(
            SqlCommitCloudBuilder::from_sql_connections(self.metadata_db.clone()).new(),
            bonsai_hg_mapping.clone(),
            bonsai_git_mapping.clone(),
            repo_derived_data.clone(),
            self.ctx.clone(),
            DummyAclProvider::new(self.fb)?,
            self.config.commit_cloud_config.clone().into(),
        );
        Ok(Arc::new(cc))
    }

    /// Function to create a logger for repos stats
    pub async fn repo_stats_logger(&self) -> Result<ArcRepoStatsLogger> {
        Ok(Arc::new(RepoStatsLogger::noop()))
    }

    /// Function to create an object to configure push redirection
    pub async fn push_redirection_config(&self) -> Result<ArcPushRedirectionConfig> {
        Ok(Arc::new(NoopPushRedirectionConfig {}))
    }

    /// Construct RepoDerivationQueues, using override if set, otherwise empty.
    pub fn repo_derivation_queues(&self) -> ArcRepoDerivationQueues {
        self.repo_derivation_queues.clone().unwrap_or_else(|| {
            Arc::new(RepoDerivationQueues::new(std::collections::HashMap::new()))
        })
    }
}
