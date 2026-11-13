/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use blobstore_factory::BlobstoreOptions;
use blobstore_factory::CachelibBlobstoreOptions;
use blobstore_factory::ChaosOptions;
use blobstore_factory::DelayOptions;
#[cfg(fbcode_build)]
use blobstore_factory::ManifoldOptions;
use blobstore_factory::PackOptions;
use blobstore_factory::ReadOnlyStorage;
use blobstore_factory::ThrottleOptions;
use cached_config::ConfigStore;
use clap::ValueEnum;
use clientinfo::ClientEntryPoint;
use commit_graph_types::environment::CommitGraphOptions;
use derived_data_remote::RemoteDerivationOptions;
use fbinit::FacebookInit;
use megarepo_config::MononokeMegarepoConfigsOptions;
use mononoke_types::DerivableType;
use observability::ObservabilityContext;
use permission_checker::AclProvider;
use permission_checker::dummy::DummyAclProvider;
use rendezvous::RendezVousOptions;
use scuba_ext::MononokeScubaSampleBuilder;
use sql_ext::facebook::MysqlOptions;
use strum::EnumString;
use tokio::runtime::Handle;

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum LocalCacheEncoding {
    /// Local cache uses bincode for encoding
    Bincode,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub struct LocalCacheConfig {
    /// Number of shards in the local blobstore cache
    pub blobstore_cache_shards: usize,

    /// Encoding used by the local cache
    pub encoding: LocalCacheEncoding,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Caching {
    /// Caching is fully enabled, with both local and shared caches.
    Enabled(LocalCacheConfig),

    /// Caching is only enabled locally - the shared cache is disabled.
    LocalOnly(LocalCacheConfig),

    /// Caching is not enabled.
    Disabled,
}

#[derive(Clone, Debug, ValueEnum, EnumString, strum::Display, PartialEq, Eq)]

/// Which derived data types should the cache wait for before
/// exposing the bookmark move to the users.
pub enum BookmarkCacheDerivedData {
    /// Only wait for hg derived data - the option used mainly by Mononoke SaplingRemoteAPI Server.
    HgOnly,
    /// Only wait for git derived data - the option used mainly by Mononoke Git Server.
    GitOnly,
    /// Wait for all derived data types - mainly used by Mononoke SCS Server.
    AllKinds,
    /// Wait for specific derived data types.
    #[value(skip)]
    SpecificTypes(Vec<DerivableType>),
    /// Don't wait for any derived data - advance bookmarks as they move.
    NoDerivation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BookmarkCacheAddress {
    ShardManagerTier(String),
    SmcTier(String),
    HostPort(String),
}

impl Default for BookmarkCacheAddress {
    fn default() -> Self {
        Self::SmcTier("mononoke-bookmark-cache".to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BookmarkCacheKind {
    Disabled,
    Local,
    Remote(BookmarkCacheAddress),
}

#[derive(Clone, Debug)]
pub struct BookmarkCacheOptions {
    pub cache_kind: BookmarkCacheKind,
    pub derived_data: BookmarkCacheDerivedData,
}

impl Default for BookmarkCacheOptions {
    fn default() -> Self {
        Self {
            cache_kind: BookmarkCacheKind::Disabled,
            derived_data: BookmarkCacheDerivedData::NoDerivation,
        }
    }
}

/// Convert BookmarkCacheDerivedData to WarmerRequirement for scoped bookmark cache queries.
/// NoDerivation and SpecificTypes default to AllKinds as they need all configured warmers.
impl From<BookmarkCacheDerivedData> for bookmarks_cache::WarmerRequirement {
    fn from(derived_data: BookmarkCacheDerivedData) -> Self {
        use bookmarks_cache::WarmerRequirement;
        match derived_data {
            BookmarkCacheDerivedData::HgOnly => WarmerRequirement::HgOnly,
            BookmarkCacheDerivedData::GitOnly => WarmerRequirement::GitOnly,
            BookmarkCacheDerivedData::AllKinds => WarmerRequirement::AllKinds,
            // SpecificTypes don't apply remotely. When instantated locally, we wait for all warmers.
            BookmarkCacheDerivedData::SpecificTypes(_) => WarmerRequirement::AllKinds,
            // If there are no warmers we will not wait for anything. We use AllKinds as a default.
            BookmarkCacheDerivedData::NoDerivation => WarmerRequirement::AllKinds,
        }
    }
}

impl From<&BookmarkCacheDerivedData> for bookmarks_cache::WarmerRequirement {
    fn from(derived_data: &BookmarkCacheDerivedData) -> Self {
        use bookmarks_cache::WarmerRequirement;
        match derived_data {
            BookmarkCacheDerivedData::HgOnly => WarmerRequirement::HgOnly,
            BookmarkCacheDerivedData::GitOnly => WarmerRequirement::GitOnly,
            BookmarkCacheDerivedData::AllKinds => WarmerRequirement::AllKinds,
            BookmarkCacheDerivedData::SpecificTypes(_) => WarmerRequirement::AllKinds,
            BookmarkCacheDerivedData::NoDerivation => WarmerRequirement::AllKinds,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RemoteDiffOptions {
    pub diff_remotely: bool,
}

/// Struct representing the configuration associated with a MononokeApp instance which
/// is immutable post the point of app construction.
pub struct MononokeEnvironment {
    pub fb: FacebookInit,
    pub scuba_sample_builder: MononokeScubaSampleBuilder,
    pub warm_bookmarks_cache_scuba_sample_builder: MononokeScubaSampleBuilder,
    pub config_store: ConfigStore,
    pub caching: Caching,
    pub observability_context: ObservabilityContext,
    pub runtime: Handle,
    pub mysql_options: MysqlOptions,
    pub blobstore_options: BlobstoreOptions,
    pub readonly_storage: ReadOnlyStorage,
    pub rendezvous_options: RendezVousOptions,
    pub megarepo_configs_options: MononokeMegarepoConfigsOptions,
    pub remote_derivation_options: RemoteDerivationOptions,
    pub remote_diff_options: RemoteDiffOptions,
    pub disabled_hooks: HashMap<String, HashSet<String>>,
    pub acl_provider: Arc<dyn AclProvider>,
    pub bookmark_cache_options: BookmarkCacheOptions,
    /// Function determining whether given repo (identified by name) should be loaded
    pub filter_repos: Option<Arc<dyn Fn(&str) -> bool + Send + Sync + 'static>>,
    pub commit_graph_options: CommitGraphOptions,
    pub client_entry_point_for_service: ClientEntryPoint,
    pub derivation_queue_namespace: String,
    pub use_pipeline_zelos_config: bool,
    pub redaction_disabled: bool,
}

impl MononokeEnvironment {
    /// An environment for tests that build repos without storage: no caching,
    /// Scuba samples discarded, every ACL allowing. Call it inside a Tokio
    /// runtime.
    pub fn for_test(fb: FacebookInit, config_store: &ConfigStore) -> Self {
        Self {
            fb,
            scuba_sample_builder: MononokeScubaSampleBuilder::with_discard(),
            warm_bookmarks_cache_scuba_sample_builder: MononokeScubaSampleBuilder::with_discard(),
            config_store: config_store.clone(),
            caching: Caching::Disabled,
            observability_context: ObservabilityContext::new_static(config_store),
            runtime: Handle::current(),
            mysql_options: MysqlOptions::default(),
            blobstore_options: BlobstoreOptions::new(
                ChaosOptions::new(None, None),
                DelayOptions::default(),
                ThrottleOptions::default(),
                #[cfg(fbcode_build)]
                ManifoldOptions::default(),
                PackOptions::default(),
                CachelibBlobstoreOptions::default(),
                None,
                MysqlOptions::default(),
            ),
            readonly_storage: ReadOnlyStorage(false),
            rendezvous_options: RendezVousOptions::for_test(),
            megarepo_configs_options: MononokeMegarepoConfigsOptions::Prod,
            remote_derivation_options: RemoteDerivationOptions {
                derive_remotely: false,
            },
            remote_diff_options: RemoteDiffOptions {
                diff_remotely: false,
            },
            disabled_hooks: HashMap::new(),
            acl_provider: Arc::new(DummyAclProvider),
            bookmark_cache_options: BookmarkCacheOptions::default(),
            filter_repos: None,
            commit_graph_options: CommitGraphOptions {
                skip_preloading_commit_graph: true,
            },
            client_entry_point_for_service: ClientEntryPoint::Tests,
            derivation_queue_namespace: String::new(),
            use_pipeline_zelos_config: false,
            redaction_disabled: false,
        }
    }
}
