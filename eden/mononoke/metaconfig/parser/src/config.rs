/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Functions to load and parse Mononoke configuration.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::str;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::ensure;
use cached_config::ConfigHandle;
use cached_config::ConfigStore;
use im::HashMap as ImHashMap;
use metaconfig_types::AsyncRequestsConfig;
use metaconfig_types::BlobConfig;
use metaconfig_types::CensoredScubaParams;
use metaconfig_types::CommitIdentityScheme;
use metaconfig_types::CommonConfig;
use metaconfig_types::ManifestFormatKind;
use metaconfig_types::ObjectsCountMultiplier;
use metaconfig_types::Redaction;
use metaconfig_types::RedactionConfig;
use metaconfig_types::RepoConfig;
use metaconfig_types::RepoConfigSource;
use metaconfig_types::RepoReadOnly;
use metaconfig_types::StorageConfig;
use mononoke_types::RepositoryId;
use repos::RawCommonConfig;
use repos::RawRepoConfig;
use repos::RawRestrictedPathsConfig;
use repos::RawStorageConfig;
use repos::RepoSpec;
use repos::TierManifest;

use crate::convert::Convert;
use crate::errors::ConfigurationError;

const LIST_KEYS_PATTERNS_MAX_DEFAULT: u64 = 500_000;
const HOOK_MAX_FILE_SIZE_DEFAULT: u64 = 8 * 1024 * 1024; // 8MiB

/// Holds configuration for repositories.
#[derive(Clone, Debug, PartialEq)]
pub struct RepoConfigs {
    /// Configs for all repos, each with where it came from. Persistent +
    /// `Arc`-shared so a copy-on-write update behind `ArcSwap` is O(1) clone +
    /// O(log N), not a whole-map deep clone.
    pub repos: ImHashMap<String, Arc<RepoConfigEntry>>,
    /// Index from RepositoryId to repo name for O(1) lookup by id
    pub repos_by_id: ImHashMap<RepositoryId, String>,
    /// Common configs for all repos
    pub common: CommonConfig,
}

/// One repo's config and where it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct RepoConfigEntry {
    /// The repo's config.
    pub config: RepoConfig,
    /// Where the config came from; empty for a config not loaded through a
    /// per-repo config handle.
    pub source: RepoConfigSource,
}

impl From<RepoConfig> for RepoConfigEntry {
    /// A config with no recorded source.
    fn from(config: RepoConfig) -> Self {
        Self {
            config,
            source: RepoConfigSource::default(),
        }
    }
}

/// Creates a ConfigHandle for a TierManifest from a Configerator path.
///
/// Used in the per-repo config split-loading path where the manifest lists
/// all repos in a tier along with their individual config paths.
pub fn configerator_manifest_handle(
    manifest_path: &str,
    config_store: &ConfigStore,
) -> Result<ConfigHandle<TierManifest>> {
    config_store.get_config_handle::<TierManifest>(manifest_path.to_owned())
}

/// Creates a ConfigHandle for a single repo's RawRepoConfig from a Configerator path.
///
/// Used in the per-repo config split-loading path where each repo has its own
/// config file referenced by the TierManifest.
pub fn configerator_repo_config_handle(
    config_path: &str,
    config_store: &ConfigStore,
) -> Result<ConfigHandle<RawRepoConfig>> {
    config_store.get_config_handle::<RawRepoConfig>(config_path.to_owned())
}

/// Load repo configs for a `tier` by following its `TierManifest` to each
/// per-repo `RepoSpec`, resolving tier overrides.
///
/// `config_store` points at a manifest + per-repo `RepoSpec` layout (live
/// configerator, or a file-backed store over a local `materialized_configs/`
/// tree). The manifest is read from
/// `scm/mononoke/repos/tiers/{tier}_manifest`; each entry's `config_path` is
/// loaded as a `RepoSpec` and parsed for `tier`, with `common` and `storage`
/// taken from the manifest itself.
pub fn load_repo_configs_from_manifest(
    tier: &str,
    config_store: &ConfigStore,
) -> Result<RepoConfigs> {
    let manifest_path = format!("scm/mononoke/repos/tiers/{tier}_manifest");
    let manifest = configerator_manifest_handle(&manifest_path, config_store)
        .with_context(|| format!("Failed to load tier manifest at {manifest_path}"))?
        .get();

    // NOTE: this loader is fail-fast by design — one bad entry fails the whole
    // load via the `collect::<Result<..>>()?` below. The server path isolates
    // per-repo failures via batch_load_repo_configs_checked instead. The
    // repo_id dedup runs before the RepoSpec fetch/parse, so duplicates
    // short-circuit without any I/O.
    let mut seen_repo_ids: HashSet<RepositoryId> = HashSet::new();
    let repos = manifest
        .repos
        .iter()
        .map(|entry| {
            let manifest_repo_id = RepositoryId::new(entry.repo_id);
            if !seen_repo_ids.insert(manifest_repo_id) {
                return Err(ConfigurationError::DuplicatedRepoId(manifest_repo_id).into());
            }
            let repo_spec = configerator_repo_spec_handle(&entry.config_path, config_store)
                .with_context(|| {
                    format!(
                        "Failed to load RepoSpec for repo {} at {}",
                        entry.repo_name, entry.config_path
                    )
                })?
                .get();
            let repo_config = parse_repo_spec((*repo_spec).clone(), tier, &manifest.storage)
                .with_context(|| {
                    format!("Failed to parse RepoSpec for repo {}", entry.repo_name)
                })?;
            if repo_config.repoid != manifest_repo_id {
                return Err(ConfigurationError::RepoIdMismatch {
                    repo_name: entry.repo_name.clone(),
                    manifest_repo_id,
                    spec_repo_id: repo_config.repoid,
                }
                .into());
            }
            Ok((entry.repo_name.clone(), repo_config))
        })
        .collect::<Result<HashMap<String, RepoConfig>>>()?;

    let (common, _storage) = parse_manifest_common_and_storage(&manifest)?;

    Ok(RepoConfigs::new(repos, common))
}

/// A `TierManifest`'s `common`/`storage`, equivalent to the blob's same two sections.
pub fn parse_manifest_common_and_storage(
    manifest: &TierManifest,
) -> Result<(CommonConfig, StorageConfigs)> {
    let common = parse_common_config(manifest.common.clone(), &manifest.storage)
        .context("Failed to parse common config from tier manifest")?;
    let storage = manifest
        .storage
        .clone()
        .into_iter()
        .map(|(name, config)| Ok((name, config.convert()?)))
        .collect::<Result<_>>()
        .context("Failed to parse storage configs from tier manifest")?;

    Ok((common, StorageConfigs { storage }))
}

/// Empty repo configs useful for testing purposes
pub fn load_empty_repo_configs() -> RepoConfigs {
    RepoConfigs::new(HashMap::new(), CommonConfig::default())
}

/// Metadata a repo's `RepoSpec` carries outside its `RawRepoConfig`.
struct RepoMetadata {
    repoid: RepositoryId,
    enabled: bool,
    hipster_acl: Option<String>,
    readonly: RepoReadOnly,
    default_commit_identity_scheme: CommitIdentityScheme,
    enable_git_bundle_uri: bool,
}

/// Creates a ConfigHandle for a repo's RepoSpec from a Configerator path.
///
/// Used in the direct RepoSpec consumption path where each repo has its own
/// RepoSpec file and tier overrides are resolved at runtime.
pub fn configerator_repo_spec_handle(
    config_path: &str,
    config_store: &ConfigStore,
) -> Result<ConfigHandle<RepoSpec>> {
    config_store.get_config_handle::<RepoSpec>(config_path.to_owned())
}

/// Merge a tier-specific override on top of a base RawRepoConfig.
///
/// For each field in the override, if it is set (not None/null), it replaces
/// the corresponding field in the base. Unset fields in the override leave the
/// base value unchanged.
///
/// Uses JSON-based merging to handle all fields generically, avoiding the need
/// to enumerate all ~50 fields of RawRepoConfig. This matches the Python
/// pipeline's merge semantics in repo_spec_processing.cinc.
fn merge_raw_repo_config(
    base: RawRepoConfig,
    tier_override: &RawRepoConfig,
) -> Result<RawRepoConfig> {
    let mut base_json = serde_json::to_value(&base)?;
    let override_json = serde_json::to_value(tier_override)?;

    if let (serde_json::Value::Object(base_map), serde_json::Value::Object(override_map)) =
        (&mut base_json, override_json)
    {
        for (key, value) in override_map {
            if !value.is_null() {
                base_map.insert(key, value);
            }
        }
    }

    serde_json::from_value(base_json).context("Failed to deserialize merged RawRepoConfig")
}

/// Parse a RepoSpec into a RepoConfig for a specific tier.
///
/// Resolves tier_overrides by merging the tier-specific partial config on top
/// of the base repo_config. Uses the RepoSpec's identity fields (hipster_acl,
/// enabled, readonly, etc.) instead of hardcoded defaults.
///
/// `named_storage_configs` typically comes from `TierManifest.storage`.
pub fn parse_repo_spec(
    repo_spec: RepoSpec,
    tier_name: &str,
    named_storage_configs: &HashMap<String, RawStorageConfig>,
) -> Result<RepoConfig> {
    let base_config = repo_spec.repo_config.unwrap_or_default();
    let resolved_config = match repo_spec.tier_overrides {
        Some(ref overrides) => match overrides.get(tier_name) {
            Some(tier_override) => merge_raw_repo_config(base_config, tier_override)?,
            None => base_config,
        },
        None => base_config,
    };

    let readonly = if repo_spec.readonly {
        RepoReadOnly::ReadOnly("Set by config option".to_string())
    } else {
        RepoReadOnly::ReadWrite
    };

    let default_commit_identity_scheme = Some(repo_spec.default_commit_identity_scheme)
        .convert()?
        .unwrap_or_default();

    build_repo_config(
        resolved_config,
        RepoMetadata {
            repoid: RepositoryId::new(repo_spec.repo_id),
            enabled: repo_spec.enabled,
            hipster_acl: Some(repo_spec.hipster_acl).filter(|acl| !acl.is_empty()),
            readonly,
            default_commit_identity_scheme,
            enable_git_bundle_uri: repo_spec.enable_git_bundle_uri.unwrap_or(false),
        },
        named_storage_configs,
    )
}

/// Shared conversion logic: converts a `RawRepoConfig` plus pre-resolved
/// metadata into a `RepoConfig`.
///
/// Both `parse_with_repo_definition` (monolithic loading path) and
/// `parse_repo_spec` (per-repo split-loading path) delegate to this function.
fn build_repo_config(
    raw_repo_config: RawRepoConfig,
    metadata: RepoMetadata,
    named_storage_configs: &HashMap<String, RawStorageConfig>,
) -> Result<RepoConfig> {
    let RawRepoConfig {
        storage_config,
        storage,
        bookmarks,
        hook_manager_params,
        hooks,
        redaction,
        mirror_upload_redaction_bypass_enabled,
        scuba_table_hooks,
        cache_warmup,
        push,
        pushrebase,
        lfs,
        infinitepush,
        list_keys_patterns_max,
        filestore,
        hook_max_file_size,
        source_control_service,
        source_control_service_monitoring,
        derived_data_config,
        scuba_local_path_hooks,
        enforce_lfs_acl_check,
        repo_client_use_warm_bookmarks_cache,
        phabricator_callsign,
        walker_config,
        cross_repo_commit_validation_config,
        sparse_profiles_config,
        update_logging_config,
        commit_graph_config,
        deep_sharding_config,
        lazy_loading_config,
        everstore_local_path,
        metadata_logger_config,
        commit_cloud_config,
        zelos_config,
        pipeline_zelos_config,
        bookmark_name_for_objects_count,
        default_objects_count,
        override_objects_count,
        objects_count_multiplier,
        x_repo_sync_source_mapping,
        mononoke_cas_sync_config,
        git_configs,
        modern_sync_config,
        metadata_cache_config,
        directory_branch_cluster_config,
        restricted_paths_config,
        remote_diff_config,
        commit_rate_limit_config,
        p4_source,
        ..
    } = raw_repo_config;

    ensure!(
        storage.is_none(),
        "repo_config.storage is not supported: define the storage in the shared storage \
         configs and name it in storage_config"
    );
    let storage_name =
        storage_config.ok_or_else(|| anyhow!("missing storage_config from configuration"))?;

    let hooks: Vec<_> = hooks.unwrap_or_default().convert()?;

    let storage_config: StorageConfig = named_storage_configs
        .get(&storage_name)
        .cloned()
        .ok_or_else(|| {
            ConfigurationError::InvalidConfig(format!("Storage \"{storage_name}\" not defined"))
        })?
        .convert()?;

    let walker_config = walker_config.convert()?;

    let cache_warmup = cache_warmup.convert()?;

    let hook_manager_params = hook_manager_params.convert()?;

    let bookmarks = bookmarks.unwrap_or_default().convert()?;

    let push = push.convert()?.unwrap_or_default();

    let pushrebase = pushrebase.convert()?.unwrap_or_default();

    let lfs = lfs.convert()?.unwrap_or_default();

    let redaction = if redaction.unwrap_or(true) {
        Redaction::Enabled
    } else {
        Redaction::Disabled
    };

    let mirror_upload_redaction_bypass_enabled =
        mirror_upload_redaction_bypass_enabled.unwrap_or(false);

    let infinitepush = infinitepush.convert()?.unwrap_or_default();

    let list_keys_patterns_max: u64 = list_keys_patterns_max
        .map(|v| v.try_into())
        .transpose()?
        .unwrap_or(LIST_KEYS_PATTERNS_MAX_DEFAULT);

    let hook_max_file_size: u64 = hook_max_file_size
        .map(|v| v.try_into())
        .transpose()?
        .unwrap_or(HOOK_MAX_FILE_SIZE_DEFAULT);

    let filestore = filestore.convert()?;

    let source_control_service = source_control_service.convert()?.unwrap_or_default();

    let source_control_service_monitoring = source_control_service_monitoring.convert()?;

    let derived_data_config = derived_data_config.convert()?.unwrap_or_default();

    let enforce_lfs_acl_check = enforce_lfs_acl_check.unwrap_or(false);
    let repo_client_use_warm_bookmarks_cache =
        repo_client_use_warm_bookmarks_cache.unwrap_or(false);

    let cross_repo_commit_validation_config = cross_repo_commit_validation_config.convert()?;

    let sparse_profiles_config = sparse_profiles_config.convert()?;

    let update_logging_config = update_logging_config.convert()?.unwrap_or_default();

    let commit_graph_config = commit_graph_config.convert()?.unwrap_or_default();
    let deep_sharding_config = deep_sharding_config.convert()?;
    let lazy_loading_config = lazy_loading_config.convert()?;
    let metadata_logger_config = metadata_logger_config.convert()?.unwrap_or_default();
    let zelos_config = zelos_config.convert()?;
    let pipeline_zelos_config = pipeline_zelos_config.convert()?;
    let x_repo_sync_source_mapping = x_repo_sync_source_mapping.convert()?;

    let raw_git_configs = git_configs.unwrap_or_default();

    let git_configs = raw_git_configs.convert()?;

    let commit_cloud_config = commit_cloud_config.convert()?.unwrap_or_default();
    let mononoke_cas_sync_config = mononoke_cas_sync_config.convert()?;
    let modern_sync_config = modern_sync_config.convert()?;
    let objects_count_multiplier = objects_count_multiplier.map(ObjectsCountMultiplier::new);
    let metadata_cache_config = metadata_cache_config
        .map(|cache_config| cache_config.convert())
        .transpose()?;
    let directory_branch_cluster_config = directory_branch_cluster_config.convert()?;
    let restricted_paths_config = restricted_paths_config
        .map(RawRestrictedPathsConfig::convert)
        .transpose()?
        .unwrap_or_default();
    let remote_diff_config = remote_diff_config
        .map(|config| config.convert())
        .transpose()?;
    let commit_rate_limit_config = commit_rate_limit_config
        .map(|config| config.convert())
        .transpose()?;
    let p4_source = p4_source.convert()?;

    Ok(RepoConfig {
        enabled: metadata.enabled,
        storage_config,
        repoid: metadata.repoid,
        scuba_table_hooks,
        scuba_local_path_hooks,
        cache_warmup,
        hook_manager_params,
        bookmarks,
        hooks,
        push,
        pushrebase,
        lfs,
        readonly: metadata.readonly,
        redaction,
        mirror_upload_redaction_bypass_enabled,
        infinitepush,
        list_keys_patterns_max,
        filestore,
        hook_max_file_size,
        hipster_acl: metadata.hipster_acl,
        source_control_service,
        source_control_service_monitoring,
        derived_data_config,
        enforce_lfs_acl_check,
        repo_client_use_warm_bookmarks_cache,
        phabricator_callsign,
        // Not expressible in a RepoSpec; the acl_regions feature is unconfigured
        // everywhere and superseded by restricted_paths.
        acl_region_config: None,
        walker_config,
        cross_repo_commit_validation_config,
        sparse_profiles_config,
        update_logging_config,
        commit_graph_config,
        default_commit_identity_scheme: metadata.default_commit_identity_scheme,
        deep_sharding_config,
        lazy_loading_config,
        everstore_local_path,
        metadata_logger_config,
        zelos_config,
        pipeline_zelos_config,
        bookmark_name_for_objects_count,
        default_objects_count,
        override_objects_count,
        objects_count_multiplier,
        x_repo_sync_source_mapping,
        commit_cloud_config,
        mononoke_cas_sync_config,
        git_configs,
        modern_sync_config,
        metadata_cache_config,
        enable_git_bundle_uri: metadata.enable_git_bundle_uri,
        directory_branch_cluster_config,
        restricted_paths_config,
        remote_diff_config,
        commit_rate_limit_config,
        p4_source,
    })
}

/// Holds configuration for storage.
#[derive(Debug, PartialEq)]
pub struct StorageConfigs {
    /// Configs for all storage
    pub storage: HashMap<String, StorageConfig>,
}

fn parse_common_config(
    common: RawCommonConfig,
    common_storage_config: &HashMap<String, RawStorageConfig>,
) -> Result<CommonConfig> {
    let trusted_parties_hipster_tier = common
        .trusted_parties_hipster_tier
        .filter(|tier| !tier.is_empty());
    let trusted_parties_allowlist = common
        .trusted_parties_allowlist
        .unwrap_or_default()
        .into_iter()
        .map(Convert::convert)
        .collect::<Result<Vec<_>>>()?;
    let global_allowlist = common
        .global_allowlist
        .unwrap_or_default()
        .into_iter()
        .map(Convert::convert)
        .collect::<Result<Vec<_>>>()?;
    let loadlimiter_category = common
        .loadlimiter_category
        .filter(|category| !category.is_empty());
    let scuba_censored_table = common.scuba_censored_table;
    let scuba_censored_local_path = common.scuba_local_path_censored;
    let internal_identity = common
        .internal_identity
        .map(Convert::convert)
        .transpose()?
        .unwrap_or_default();
    let git_memory_upper_bound = common
        .git_memory_upper_bound
        .map(|bound| bound.try_into())
        .transpose()?;
    let edenapi_dumper_scuba_table = common.edenapi_dumper_scuba_table;

    let censored_scuba_params = CensoredScubaParams {
        table: scuba_censored_table,
        local_path: scuba_censored_local_path,
    };

    let get_mutable_blobstore = |name| -> Result<BlobConfig> {
        Ok(common_storage_config
            .get(name)
            .cloned()
            .ok_or_else(|| {
                ConfigurationError::InvalidConfig(format!(
                    "Storage \"{name}\" not defined for redaction config"
                ))
            })?
            .convert()?
            .mutable_blobstore)
    };

    let redaction_config = common.redaction_config.unwrap_or_default();
    let redaction_config = RedactionConfig {
        blobstore: get_mutable_blobstore(&redaction_config.blobstore)?,
        redaction_sets_location: redaction_config.redaction_sets_location,
    };

    let async_requests_config = match common.async_requests_config {
        Some(config) => AsyncRequestsConfig {
            db_config: Some(config.db_config.convert()?),
            blobstore: Some(config.blobstore_config.convert()?),
        },
        None => AsyncRequestsConfig::default(),
    };

    let rl_land_service_repo_prefix = common.rl_land_service_repo_prefix.filter(|p| !p.is_empty());
    let multi_repo_land_manifest_repos: BTreeMap<String, String> = common
        .multi_repo_land_manifest_repos
        .unwrap_or_default()
        .into_iter()
        .filter(|(marker, repo)| !marker.is_empty() && !repo.is_empty())
        .collect();
    let multi_repo_land_manifest_formats = common
        .multi_repo_land_manifest_formats
        .unwrap_or_default()
        .into_iter()
        .map(|(repo, format)| {
            let kind = match format.to_ascii_lowercase().as_str() {
                "aosp" => ManifestFormatKind::Aosp,
                "west" => ManifestFormatKind::West,
                other => {
                    return Err(anyhow!(
                        "multi_repo_land_manifest_formats[{repo}]: unknown manifest format {other:?} (expected aosp or west)"
                    ));
                }
            };
            Ok((repo, kind))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;

    Ok(CommonConfig {
        trusted_parties_hipster_tier,
        trusted_parties_allowlist,
        global_allowlist,
        loadlimiter_category,
        enable_http_control_api: common.enable_http_control_api.unwrap_or(false),
        censored_scuba_params,
        redaction_config,
        internal_identity,
        git_memory_upper_bound,
        edenapi_dumper_scuba_table,
        async_requests_config,
        rl_land_service_repo_prefix,
        multi_repo_land_manifest_repos,
        multi_repo_land_manifest_formats,
    })
}

impl RepoConfigs {
    /// Create RepoConfigs with an auto-built id index.
    pub fn new(repos: HashMap<String, RepoConfig>, common: CommonConfig) -> Self {
        let repos_by_id = repos
            .iter()
            .map(|(name, config)| (config.repoid, name.clone()))
            .collect();
        let repos = repos
            .into_iter()
            .map(|(name, config)| (name, Arc::new(RepoConfigEntry::from(config))))
            .collect();
        Self {
            repos,
            repos_by_id,
            common,
        }
    }

    /// Build from an already-`Arc`-wrapped persistent map, deriving `repos_by_id`.
    pub fn from_arc_map(
        repos: ImHashMap<String, Arc<RepoConfigEntry>>,
        common: CommonConfig,
    ) -> Self {
        let repos_by_id = repos
            .iter()
            .map(|(name, entry)| (entry.config.repoid, name.clone()))
            .collect();
        Self {
            repos,
            repos_by_id,
            common,
        }
    }

    /// Get individual `RepoConfig`, given its repo name.
    pub fn get(&self, name: &str) -> Option<&RepoConfig> {
        self.repos.get(name).map(|entry| &entry.config)
    }

    /// Get individual `RepoConfig` and where it came from, given its repo name.
    pub fn get_with_source(&self, name: &str) -> Option<(&RepoConfig, &RepoConfigSource)> {
        self.repos
            .get(name)
            .map(|entry| (&entry.config, &entry.source))
    }

    /// Every repo's name and `RepoConfig`.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &RepoConfig)> {
        self.repos.iter().map(|(name, entry)| (name, &entry.config))
    }

    /// Get individual `RepoConfig`, given a repo_id. O(1) via index.
    pub fn get_repo_config(&self, repo_id: RepositoryId) -> Option<(&String, &RepoConfig)> {
        let name = self.repos_by_id.get(&repo_id)?;
        self.get(name).map(|config| (name, config))
    }

    /// O(1) lookup by raw repo id (i32). Constructs RepositoryId internally
    /// so callers don't need the mononoke_types dependency.
    pub fn get_repo_config_by_raw_id(&self, repo_id: i32) -> Option<(&String, &RepoConfig)> {
        self.get_repo_config(RepositoryId::new(repo_id))
    }

    /// Insert a repo and update the ID index.
    /// Cleans up stale index entries if the repo's ID changed.
    pub fn insert_repo(&mut self, name: String, entry: impl Into<RepoConfigEntry>) {
        let entry = entry.into();
        if let Some(old) = self.repos.get(&name) {
            if old.config.repoid != entry.config.repoid {
                self.repos_by_id.remove(&old.config.repoid);
            }
        }
        self.repos_by_id.insert(entry.config.repoid, name.clone());
        self.repos.insert(name, Arc::new(entry));
    }

    /// Remove a repo, keeping `repos` and `repos_by_id` consistent. Returns whether it existed.
    pub fn remove_repo(&mut self, name: &str) -> bool {
        match self.repos.remove(name) {
            Some(entry) => {
                self.repos_by_id.remove(&entry.config.repoid);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod test {
    use std::collections::BTreeSet;
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::time::Duration;

    use bookmarks_types::BookmarkKey;
    use cached_config::TestSource;
    use maplit::btreemap;
    use maplit::hashmap;
    use maplit::hashset;
    use metaconfig_types::Address;
    use metaconfig_types::BlameVersion;
    use metaconfig_types::BlobConfig;
    use metaconfig_types::BlobstoreId;
    use metaconfig_types::BookmarkParams;
    use metaconfig_types::BubbleDeletionMode;
    use metaconfig_types::CacheWarmupParams;
    use metaconfig_types::CommitCloudConfig;
    use metaconfig_types::CommitGraphConfig;
    use metaconfig_types::CommitIdentityScheme;
    use metaconfig_types::CommitSyncConfig;
    use metaconfig_types::CommitSyncConfigVersion;
    use metaconfig_types::ComparableRegex;
    use metaconfig_types::CrossRepoCommitValidation;
    use metaconfig_types::DatabaseConfig;
    use metaconfig_types::DefaultSmallToLargeCommitSyncPathAction;
    use metaconfig_types::DerivedDataConfig;
    use metaconfig_types::DerivedDataTypesConfig;
    use metaconfig_types::EphemeralBlobstoreConfig;
    use metaconfig_types::FilestoreParams;
    use metaconfig_types::GitConcurrencyParams;
    use metaconfig_types::GitConfigs;
    use metaconfig_types::HookBypassActor;
    use metaconfig_types::HookBypassCondition;
    use metaconfig_types::HookConfig;
    use metaconfig_types::HookManagerParams;
    use metaconfig_types::HookParams;
    use metaconfig_types::Identity;
    use metaconfig_types::InfinitepushNamespace;
    use metaconfig_types::InfinitepushParams;
    use metaconfig_types::LfsParams;
    use metaconfig_types::LocalDatabaseConfig;
    use metaconfig_types::LoggingDestination;
    use metaconfig_types::MergeResolutionOverride;
    use metaconfig_types::MetadataCacheConfig;
    use metaconfig_types::MetadataCacheUpdateMode;
    use metaconfig_types::MetadataDatabaseConfig;
    use metaconfig_types::MetadataLoggerConfig;
    use metaconfig_types::MultiplexId;
    use metaconfig_types::MultiplexedStoreType;
    use metaconfig_types::P4SourceConfig;
    use metaconfig_types::PushParams;
    use metaconfig_types::PushrebaseFlags;
    use metaconfig_types::PushrebaseParams;
    use metaconfig_types::PushrebaseRemoteMode;
    use metaconfig_types::RemoteDatabaseConfig;
    use metaconfig_types::RemoteMetadataDatabaseConfig;
    use metaconfig_types::RestrictedPathsConfig;
    use metaconfig_types::ShardableRemoteDatabaseConfig;
    use metaconfig_types::ShardedDatabaseConfig;
    use metaconfig_types::ShardedRemoteDatabaseConfig;
    use metaconfig_types::ShardingModeConfig;
    use metaconfig_types::SmallRepoCommitSyncConfig;
    use metaconfig_types::SourceControlServiceMonitoring;
    use metaconfig_types::SourceControlServiceParams;
    use metaconfig_types::SparseProfilesConfig;
    use metaconfig_types::UnodeVersion;
    use metaconfig_types::UpdateLoggingConfig;
    use metaconfig_types::WalkerConfig;
    use metaconfig_types::XRepoSyncSourceConfig;
    use metaconfig_types::XRepoSyncSourceConfigMapping;
    use mononoke_macros::mononoke;
    use mononoke_types::DerivableType;
    use mononoke_types::NonRootMPath;
    use mononoke_types_mocks::changesetid::THREES_CSID;
    use nonzero_ext::nonzero;
    use permission_checker::MononokeIdentity;
    use pretty_assertions::assert_eq;
    use repos::RawCommitIdentityScheme;
    use repos::RawCommitSyncConfig;
    use serde::Deserialize;

    use super::*;

    /// Parse a collection of raw commit sync config into commit sync config and validate it.
    #[cfg(test)]
    fn parse_commit_sync_config(
        raw_commit_syncs: HashMap<String, RawCommitSyncConfig>,
    ) -> Result<HashMap<String, CommitSyncConfig>> {
        raw_commit_syncs
            .into_iter()
            .map(|(config_name, commit_sync_config)| {
                let commit_sync_config = commit_sync_config.convert()?;
                Ok((config_name, commit_sync_config))
            })
            .collect()
    }

    /// Parse TOML into a thrift struct the way the old `RawRepoConfigs`
    /// directory reader did: unknown keys are warned about, not fatal.
    fn read_toml<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
        let mut unused = BTreeSet::new();
        let de = toml::de::Deserializer::parse(text)?;
        let t: T = serde_ignored::deserialize(de, |path| {
            unused.insert(path.to_string());
        })?;
        if !unused.is_empty() {
            tracing::warn!("unknown keys in config parsing: `{:?}`", unused);
        }
        Ok(t)
    }

    /// The per-repo half of the old `repo_definitions/<repo>/server.toml`.
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct TestRepoDefinition {
        repo_id: Option<i32>,
        repo_name: Option<String>,
        repo_config: Option<String>,
        hipster_acl: Option<String>,
        enabled: Option<bool>,
        readonly: Option<bool>,
        default_commit_identity_scheme: Option<i32>,
        enable_git_bundle_uri: Option<bool>,
    }

    const TEST_TIER: &str = "test";

    /// Render the TOML fixture tree the tests are written in into what
    /// production reads, a `TierManifest` plus one `RepoSpec` per repo, and
    /// parse it through `parse_manifest_common_and_storage` + `parse_repo_spec`
    /// (the same translation the `.t` harness performs).
    fn load_test_configs(files: &BTreeMap<&str, &str>) -> Result<RepoConfigs> {
        let common: RawCommonConfig = read_toml(files.get("common/common.toml").unwrap_or(&""))?;
        let storage: HashMap<String, RawStorageConfig> =
            read_toml(files.get("common/storage.toml").unwrap_or(&""))?;
        let manifest = TierManifest {
            common,
            storage,
            ..Default::default()
        };

        // Repos first, then common: the order the old directory loader used,
        // which several fixtures with a minimal `common` rely on.
        let mut repos = HashMap::new();
        let mut seen_repo_ids = HashSet::new();
        for (path, text) in files {
            let Some(name) = path
                .strip_prefix("repo_definitions/")
                .and_then(|rest| rest.strip_suffix("/server.toml"))
            else {
                continue;
            };
            let def: TestRepoDefinition = read_toml(text)?;
            let repo_name = def.repo_name.unwrap_or_else(|| name.to_string());
            let config_key = def.repo_config.as_deref().unwrap_or(name);
            let repo_config: RawRepoConfig = read_toml(
                files
                    .get(format!("repos/{config_key}/server.toml").as_str())
                    .ok_or_else(|| anyhow!("no repos/{config_key}/server.toml"))?,
            )?;
            let spec = RepoSpec {
                repo_id: def.repo_id.context("missing repoid from configuration")?,
                repo_name: repo_name.clone(),
                hipster_acl: def.hipster_acl.unwrap_or_default(),
                enabled: def.enabled.unwrap_or(true),
                readonly: def.readonly.unwrap_or(false),
                default_commit_identity_scheme: def
                    .default_commit_identity_scheme
                    .map(RawCommitIdentityScheme)
                    .unwrap_or(RawCommitIdentityScheme::HG),
                enable_git_bundle_uri: def.enable_git_bundle_uri,
                tiers: vec![TEST_TIER.to_string()],
                repo_config: Some(repo_config),
                ..Default::default()
            };
            let repo_config = parse_repo_spec(spec, TEST_TIER, &manifest.storage)?;
            if !seen_repo_ids.insert(repo_config.repoid) {
                return Err(ConfigurationError::DuplicatedRepoId(repo_config.repoid).into());
            }
            repos.insert(repo_name, repo_config);
        }
        let (common, _) = parse_manifest_common_and_storage(&manifest)?;
        Ok(RepoConfigs::new(repos, common))
    }

    fn repo_config_with_id(id: i32) -> RepoConfig {
        RepoConfig {
            repoid: RepositoryId::new(id),
            ..Default::default()
        }
    }

    // insert_repo keeps repos_by_id in sync: a new repo is findable by id.
    #[mononoke::test]
    fn test_insert_repo_maintains_id_index() {
        let mut configs = RepoConfigs::new(HashMap::new(), CommonConfig::default());
        configs.insert_repo("foo".to_string(), repo_config_with_id(7));

        assert!(configs.repos.contains_key("foo"), "foo missing by name");
        let (name, config) = configs
            .get_repo_config_by_raw_id(7)
            .expect("foo must be findable by raw id after insert_repo");
        assert_eq!(name, "foo");
        assert_eq!(config.repoid, RepositoryId::new(7));
    }

    // Re-inserting a repo under a new id must drop the stale id-index entry.
    #[mononoke::test]
    fn test_insert_repo_updates_id_index_on_id_change() {
        let mut configs = RepoConfigs::new(HashMap::new(), CommonConfig::default());
        configs.insert_repo("foo".to_string(), repo_config_with_id(7));
        configs.insert_repo("foo".to_string(), repo_config_with_id(8));

        assert!(
            configs.get_repo_config_by_raw_id(7).is_none(),
            "stale id 7 must no longer resolve",
        );
        let (name, config) = configs
            .get_repo_config_by_raw_id(8)
            .expect("foo must resolve under its new id 8");
        assert_eq!(name, "foo");
        assert_eq!(config.repoid, RepositoryId::new(8));
    }

    // remove_repo must drop the entry from both maps.
    #[mononoke::test]
    fn test_remove_repo_maintains_id_index() {
        let mut configs = RepoConfigs::new(HashMap::new(), CommonConfig::default());
        configs.insert_repo("foo".to_string(), repo_config_with_id(7));

        assert!(
            configs.remove_repo("foo"),
            "remove_repo should report prior existence"
        );
        assert!(
            !configs.repos.contains_key("foo"),
            "foo still present by name"
        );
        assert!(
            configs.get_repo_config_by_raw_id(7).is_none(),
            "id 7 still resolves after removal",
        );
        assert!(
            !configs.remove_repo("foo"),
            "second remove should be a no-op"
        );
    }

    // Persistent map: cloning is snapshot-isolated and shares unchanged values
    // by Arc (the O(1)-clone property the fix relies on).
    #[mononoke::test]
    fn test_persistent_map_structural_sharing() {
        let repos: HashMap<String, RepoConfig> = (0..5000)
            .map(|id| (format!("repo{id}"), repo_config_with_id(id)))
            .collect();
        let original = RepoConfigs::new(repos, CommonConfig::default());

        let mut snapshot = original.clone();
        assert!(
            snapshot.remove_repo("repo1234"),
            "entry should exist in clone"
        );

        // Snapshot isolation: original keeps the entry, clone does not.
        assert!(
            original.repos.contains_key("repo1234"),
            "original must still hold the removed entry",
        );
        assert!(
            !snapshot.repos.contains_key("repo1234"),
            "clone must not hold the removed entry",
        );

        // Structural sharing: an untouched entry is the same Arc in both.
        let a = original.repos.get("repo4321").expect("present in original");
        let b = snapshot.repos.get("repo4321").expect("present in clone");
        assert!(
            Arc::ptr_eq(a, b),
            "unchanged entries must be shared by Arc across snapshots",
        );
    }

    #[mononoke::test]
    fn test_commit_sync_config_correct() {
        let commit_sync_config = r#"
            [mega]
            large_repo_id = 1
            common_pushrebase_bookmarks = ["master"]
            version_name = "TEST_VERSION_NAME"

                [[mega.small_repos]]
                repoid = 2
                default_action = "preserve"
                bookmark_prefix = "repo2"
                direction = "small_to_large"

                    [mega.small_repos.mapping]
                    "p1" = ".r2-legacy/p1"
                    "p5" = ".r2-legacy/p5"

                [[mega.small_repos]]
                repoid = 3
                bookmark_prefix = "repo3"
                default_action = "prepend_prefix"
                default_prefix = "subdir"
                direction = "small_to_large"

                    [mega.small_repos.mapping]
                    "p1" = "p1"
                    "p4" = "p5/p4"
        "#;

        let raw_commit_sync: HashMap<String, RawCommitSyncConfig> =
            read_toml(commit_sync_config).expect("expect to read configs");
        let commit_sync = parse_commit_sync_config(raw_commit_sync)
            .expect("expected to get a commit sync config");

        let expected = hashmap! {
            "mega".to_owned() => CommitSyncConfig {
                large_repo_id: RepositoryId::new(1),
                common_pushrebase_bookmarks: vec![BookmarkKey::new("master").unwrap()],
                small_repos: hashmap! {
                    RepositoryId::new(2) => SmallRepoCommitSyncConfig {
                        default_action: DefaultSmallToLargeCommitSyncPathAction::Preserve,
                        map: hashmap! {
                            NonRootMPath::new("p1").unwrap() => NonRootMPath::new(".r2-legacy/p1").unwrap(),
                            NonRootMPath::new("p5").unwrap() => NonRootMPath::new(".r2-legacy/p5").unwrap(),
                        },
                        submodule_config: Default::default(),
                    },
                    RepositoryId::new(3) => SmallRepoCommitSyncConfig {
                        default_action: DefaultSmallToLargeCommitSyncPathAction::PrependPrefix(NonRootMPath::new("subdir").unwrap()),
                        map: hashmap! {
                            NonRootMPath::new("p1").unwrap() => NonRootMPath::new("p1").unwrap(),
                            NonRootMPath::new("p4").unwrap() => NonRootMPath::new("p5/p4").unwrap(),
                        },
                        submodule_config: Default::default(),
                    }
                },
                version_name: CommitSyncConfigVersion("TEST_VERSION_NAME".to_string()),
            }
        };

        assert_eq!(commit_sync, expected);
    }

    #[mononoke::test]
    fn test_commit_sync_config_large_is_small() {
        let commit_sync_config = r#"
            [mega]
            large_repo_id = 1
            common_pushrebase_bookmarks = ["master"]

                [[mega.small_repos]]
                repoid = 1
                bookmark_prefix = "repo2"
                default_action = "preserve"
                direction = "small_to_large"

                    [mega.small_repos.mapping]
                    "p1" = ".r2-legacy/p1"
                    "p5" = "subdir"
        "#;

        let commit_sync: HashMap<String, RawCommitSyncConfig> =
            read_toml(commit_sync_config).unwrap();
        for (_config_name, commit_sync_config) in commit_sync {
            let res = commit_sync_config.convert();
            let msg = format!("{res:#?}");
            println!("res = {msg}");
            assert!(res.is_err());
            assert!(msg.contains("is one of the small repos too"));
        }
    }

    #[mononoke::test]
    fn test_commit_sync_config_duplicated_small_repos() {
        let commit_sync_config = r#"
            [mega]
            large_repo_id = 1
            common_pushrebase_bookmarks = ["master"]

                [[mega.small_repos]]
                repoid = 2
                bookmark_prefix = "repo2"
                default_action = "preserve"
                direction = "small_to_large"

                [[mega.small_repos]]
                repoid = 2
                bookmark_prefix = "repo3"
                default_action = "prepend_prefix"
                default_prefix = "subdir"
                direction = "small_to_large"
        "#;

        let commit_sync: HashMap<String, RawCommitSyncConfig> =
            read_toml(commit_sync_config).unwrap();
        for (_config_name, commit_sync_config) in commit_sync {
            let res = commit_sync_config.convert();
            let msg = format!("{res:#?}");
            println!("res = {msg}");
            assert!(res.is_err());
            assert!(msg.contains("present multiple times in the same CommitSyncConfig"));
        }
    }
    #[mononoke::test]
    fn test_duplicated_repo_ids() {
        let www_content = r#"
            scuba_table_hooks="scm_hooks"
            storage_config="files"
        "#;
        let storage = r#"
            [files.metadata.local]
            local_db_path = "/tmp/www"

            [files.blobstore.blob_files]
            path = "/tmp/www"

            [files.mutable_blobstore.blob_files]
            path = "/tmp/www_mutable"
        "#;
        let common_content = r#"
            loadlimiter_category="test-category"
            trusted_parties_hipster_tier = "tier1"

            [[global_allowlist]]
            identity_type = "username"
            identity_data = "user"
        "#;

        let www1_repo_def = r#"
            repo_id=1
            repo_name="www1"
            repo_config="www1"
        "#;

        let www2_repo_def = r#"
            repo_id=1
            repo_name="www2"
            repo_config="www2"
        "#;

        let paths = btreemap! {
            "common/common.toml" => common_content,
            "common/storage.toml" => storage,
            "repos/www1/server.toml" => www_content,
            "repos/www2/server.toml" => www_content,
            "repo_definitions/www1/server.toml" => www1_repo_def,
            "repo_definitions/www2/server.toml" => www2_repo_def,
        };
        let res = load_test_configs(&paths);
        let msg = format!("{res:#?}");
        println!("res = {msg}");
        assert!(res.is_err());
        assert!(msg.contains("DuplicatedRepoId"));
    }

    #[mononoke::test]
    fn test_read_manifest() {
        let fbsource_content = r#"
            scuba_table_hooks="scm_hooks"
            storage_config="main"
            list_keys_patterns_max=123
            hook_max_file_size=456
            repo_client_use_warm_bookmarks_cache=true
            phabricator_callsign="FBS"

            [cache_warmup]
            bookmark="master"
            commit_limit=100
            [hook_manager_params]
            disable_acl_checker=false
            all_hooks_bypassed=false
            bypassed_commits_scuba_table="commits_bypassed_hooks"

            [derived_data_config]
            enabled_config_name = "default"

            [derived_data_config.available_configs.default]
            types = ["fsnodes", "unodes", "blame"]
            unode_version = 2
            blame_filesize_limit = 101
            derivation_batch_sizes = { "fsnodes" = 20, "unodes" = 20, "blame" = 20 }

            [derived_data_config.blocked_derivation.changesets]
            "3333333333333333333333333333333333333333333333333333333333333333" = { blocked_derived_data_types = ["unodes"] }

            [[bookmarks]]
            name="master"
            allowed_users="^(svcscm|twsvcscm)$"

            [[bookmarks.hooks]]
            hook_name="hook1"

            [[bookmarks.hooks]]
            hook_name="hook2a"

            [[bookmarks]]
            regex="[^/]*/stable"
            ensure_ancestor_of="master"

            [[hooks]]
            name="hook1"
            bypass_commit_string="@allow_hook1"
            config_json = "{\"test\": \"abcde\"}"
            messages = { rejected = "Rejected ${path}" }

            [[hooks]]
            name="hook2a"
            implementation="hook2"
            log_only=true
            config_ints={ int1 = 44 }
            config_ints_64={ int2 = 42 }
            [hooks.config_string_lists]
                list1 = ["val1", "val2"]

            [push]
            pure_push_allowed = false

            [pushrebase]
            rewritedates = false
            recursion_limit = 1024
            forbid_p2_root_rebases = false
            casefolding_check = false
            emit_obsmarkers = false
            allow_change_xrepo_mapping_extra = true

            [pushrebase.remote_mode]
            remote_land_service = { tier = "my-tier" }

            [lfs]
            threshold = 1000
            rollout_percentage = 56
            use_upstream_lfs_server = false

            [infinitepush]
            allow_writes = true
            namespace_pattern = "foobar/.+"

            [filestore]
            chunk_size = 768
            concurrency = 48

            [source_control_service_monitoring]
            bookmarks_to_report_age= ["master", "master2"]

            [walker_config]
            scrub_enabled = true
            validate_enabled = true

            [git_configs.git_concurrency]
            trees_and_blobs = 500
            commits = 1000
            tags = 1000
            shallow = 100

            [cross_repo_commit_validation_config]
            skip_bookmarks = ["weirdy"]

            [sparse_profiles_config]
            sparse_profiles_location = "sparse"

            [update_logging_config]
            new_commit_logging_destination = { scribe = { scribe_category = "cat" } }
            git_content_refs_logging_destination = { logger = {} }

            [commit_graph_config]
            scuba_table = "commit_graph"

            [metadata_logger_config]
            bookmarks = ["master", "release"]
            sleep_interval_secs = 100

            [metadata_cache_config]
            wbc_update_mode = { tailing = { category = "scribe_category" } }
            wbc_event_router_update_mode = { tailing = { category = "event_router_scribe_category" } }
            wbc_process_bookmark_notifications = true
            tags_update_mode = { polling = {} }

            [x_repo_sync_source_mapping.mapping.aros]
            bookmark_regex = "master"
            backsync_enabled = true

            [deep_sharding_config.status]
        "#;
        let fbsource_repo_def = r#"
            repo_id=0
            repo_name="fbsource"
            hipster_acl="foo/test"
            repo_config="fbsource"
        "#;
        let www_content = r#"
            scuba_table_hooks="scm_hooks"
            storage_config="files"
            phabricator_callsign="WWW"
        "#;
        let www_repo_def = r#"
            repo_id=1
            repo_name="www"
            repo_config="www"
        "#;
        let common_content = r#"
            loadlimiter_category="test-category"
            scuba_censored_table="censored_table"
            scuba_local_path_censored="censored_local_path"
            trusted_parties_hipster_tier="tier1"
            git_memory_upper_bound=100
            edenapi_dumper_scuba_table="dumped_requests"

            [internal_identity]
            identity_type = "SERVICE_IDENTITY"
            identity_data = "internal"

            [redaction_config]
            blobstore="main"
            redaction_sets_location="loc"

            [[global_allowlist]]
            identity_type = "username"
            identity_data = "user"
        "#;

        let storage = r#"
        [main.metadata.remote]
        production = { db_address = "db_address" }
        commit_graph = { db_address = "db_address" }
        bookmarks = { db_address = "db_address" }
        bonsai_hg_mapping = { db_address = "db_address" }
        filenodes = { sharded = { shard_map = "db_address_shards", shard_num = 123 } }
        mutation = { db_address = "mutation_db_address" }
        sparse_profiles = { db_address = "sparse_profiles_db_address" }
        bonsai_blob_mapping = { sharded = { shard_map = "blob_mapping_shards", shard_num = 12 } }
        deletion_log = { db_address = "deletion_log" }
        commit_cloud = { db_address = "commit_cloud_db_address" }
        git_bundles = { db_address = "git_bundles" }
        repo_metadata = { db_address = "repo_metadata" }
        restricted_paths = { db_address = "restricted_paths" }
        derived_data_mapping = { unsharded = { db_address = "derived_data_mapping" } }

        [main.blobstore.multiplexed_wal]
        multiplex_id = 1
        inner_blobstores_scuba_table = "blobstore_scuba_table"
        multiplex_scuba_table = "multiplex_scuba_table"
        write_quorum = 1
        components = [
            { blobstore_id = 0, blobstore = { manifold = { manifold_bucket = "bucket" } } },
            { blobstore_id = 1, blobstore = { blob_files = { path = "/tmp/foo" } } },
        ]
        queue_db = { remote = { shard_map = "queue_db_address", shard_num = 13 } }

        [files.metadata.local]
        local_db_path = "/tmp/www"

        [files.blobstore.blob_files]
        path = "/tmp/www"

        [main.mutable_blobstore.multiplexed_wal]
        multiplex_id = 1
        inner_blobstores_scuba_table = "blobstore_scuba_table"
        multiplex_scuba_table = "multiplex_scuba_table"
        write_quorum = 1
        components = [
            { blobstore_id = 0, blobstore = { manifold = { manifold_bucket = "bucket" } } },
            { blobstore_id = 1, blobstore = { blob_files = { path = "/tmp/foo" } } },
        ]
        queue_db = { remote = { shard_map = "queue_db_address", shard_num = 13 } }

        [files.mutable_blobstore.blob_files]
        path = "/tmp/www"

        [files.ephemeral_blobstore]
        initial_bubble_lifespan_secs = 86400
        bubble_expiration_grace_secs = 3600
        bubble_deletion_mode = 1

        [files.ephemeral_blobstore.metadata.local]
        local_db_path = "/tmp/www-ephemeral"

        [files.ephemeral_blobstore.blobstore.blob_files]
        path = "/tmp/www-ephemeral"
        "#;

        let paths = btreemap! {
            "common/storage.toml" => storage,
            "common/common.toml" => common_content,
            "repos/fbsource/server.toml" => fbsource_content,
            "repos/www/server.toml" => www_content,
            "repo_definitions/fbsource/server.toml" => fbsource_repo_def,
            "repo_definitions/www/server.toml" => www_repo_def,
        };
        let repoconfig = load_test_configs(&paths).expect("Read configs failed");

        let multiplex = BlobConfig::MultiplexedWal {
            multiplex_id: MultiplexId::new(1),
            inner_blobstores_scuba_table: Some("blobstore_scuba_table".to_string()),
            multiplex_scuba_table: Some("multiplex_scuba_table".to_string()),
            scuba_sample_rate: nonzero!(100u64),
            blobstores: vec![
                (
                    BlobstoreId::new(0),
                    MultiplexedStoreType::Normal,
                    BlobConfig::Manifold {
                        bucket: "bucket".into(),
                        prefix: "".into(),
                    },
                ),
                (
                    BlobstoreId::new(1),
                    MultiplexedStoreType::Normal,
                    BlobConfig::Files {
                        path: "/tmp/foo".into(),
                    },
                ),
            ],
            write_quorum: 1,
            queue_db: ShardedDatabaseConfig::Sharded(ShardedRemoteDatabaseConfig {
                shard_map: "queue_db_address".into(),
                shard_num: nonzero!(13usize),
            }),
        };
        let main_storage_config = StorageConfig {
            blobstore: multiplex.clone(),
            metadata: MetadataDatabaseConfig::Remote(RemoteMetadataDatabaseConfig {
                production: RemoteDatabaseConfig {
                    db_address: "db_address".into(),
                },
                commit_graph: RemoteDatabaseConfig {
                    db_address: "db_address".into(),
                },
                bookmarks: RemoteDatabaseConfig {
                    db_address: "db_address".into(),
                },
                bonsai_hg_mapping: RemoteDatabaseConfig {
                    db_address: "db_address".into(),
                },
                filenodes: ShardableRemoteDatabaseConfig::Sharded(ShardedRemoteDatabaseConfig {
                    shard_map: "db_address_shards".into(),
                    shard_num: NonZeroUsize::new(123).unwrap(),
                }),
                mutation: RemoteDatabaseConfig {
                    db_address: "mutation_db_address".into(),
                },
                sparse_profiles: RemoteDatabaseConfig {
                    db_address: "sparse_profiles_db_address".into(),
                },
                bonsai_blob_mapping: Some(ShardableRemoteDatabaseConfig::Sharded(
                    ShardedRemoteDatabaseConfig {
                        shard_map: "blob_mapping_shards".into(),
                        shard_num: NonZeroUsize::new(12).unwrap(),
                    },
                )),
                deletion_log: Some(RemoteDatabaseConfig {
                    db_address: "deletion_log".into(),
                }),
                commit_cloud: Some(RemoteDatabaseConfig {
                    db_address: "commit_cloud_db_address".into(),
                }),
                git_bundle_metadata: Some(RemoteDatabaseConfig {
                    db_address: "git_bundles".into(),
                }),
                repo_metadata: Some(RemoteDatabaseConfig {
                    db_address: "repo_metadata".into(),
                }),
                restricted_paths: Some(RemoteDatabaseConfig {
                    db_address: "restricted_paths".into(),
                }),
                commit_derived_data_mapping: Some(ShardableRemoteDatabaseConfig::Unsharded(
                    RemoteDatabaseConfig {
                        db_address: "derived_data_mapping".into(),
                    },
                )),
                repo_manifest_mapping: None,
            }),
            ephemeral_blobstore: None,
            mutable_blobstore: multiplex,
        };

        let mut repos = HashMap::new();
        repos.insert(
            "fbsource".to_string(),
            RepoConfig {
                enable_git_bundle_uri: false,
                enabled: true,
                default_commit_identity_scheme: CommitIdentityScheme::default(),
                directory_branch_cluster_config: None,
                storage_config: main_storage_config.clone(),
                repoid: RepositoryId::new(0),
                scuba_table_hooks: Some("scm_hooks".to_string()),
                scuba_local_path_hooks: None,
                cache_warmup: Some(CacheWarmupParams {
                    bookmark: BookmarkKey::new("master").unwrap(),
                    commit_limit: 100,
                    microwave_preload: false,
                }),
                hook_manager_params: Some(HookManagerParams {
                    disable_acl_checker: false,
                    all_hooks_bypassed: false,
                    bypassed_commits_scuba_table: Some("commits_bypassed_hooks".to_string()),
                }),
                bookmarks: vec![
                    BookmarkParams {
                        bookmark: BookmarkKey::new("master").unwrap().into(),
                        hooks: vec!["hook1".to_string(), "hook2a".to_string()],
                        only_fast_forward: false,
                        allowed_users: Some(ComparableRegex::new("^(svcscm|twsvcscm)$").unwrap()),
                        allowed_hipster_group: None,
                        rewrite_dates: None,
                        hooks_skip_ancestors_of: vec![],
                        ensure_ancestor_of: None,
                    },
                    BookmarkParams {
                        bookmark: ComparableRegex::new("[^/]*/stable").unwrap().into(),
                        hooks: vec![],
                        only_fast_forward: false,
                        allowed_users: None,
                        allowed_hipster_group: None,
                        rewrite_dates: None,
                        hooks_skip_ancestors_of: vec![],
                        ensure_ancestor_of: Some(BookmarkKey::new("master").unwrap()),
                    },
                ],
                hooks: vec![
                    HookParams {
                        name: "hook1".to_string(),
                        implementation: "hook1".to_string(),
                        config: HookConfig {
                            bypasses: vec![
                                HookBypassCondition::commit_message("@allow_hook1")
                                    .with_actor(HookBypassActor::Any),
                            ],
                            options: Some(r#"{"test": "abcde"}"#.to_string()),
                            log_only: false,
                            name: "hook1".to_string(),
                            messages: hashmap! {
                                "rejected".into() => "Rejected ${path}".into(),
                            },
                            strings: hashmap! {},
                            ints: hashmap! {},
                            ints_64: hashmap! {},
                            string_lists: hashmap! {},
                            int_lists: hashmap! {},
                            int_64_lists: hashmap! {},
                        },
                    },
                    HookParams {
                        name: "hook2a".to_string(),
                        implementation: "hook2".to_string(),
                        config: HookConfig {
                            bypasses: vec![],
                            options: None,
                            log_only: true,
                            name: "hook2a".to_string(),
                            messages: hashmap! {},
                            strings: hashmap! {},
                            ints: hashmap! {
                                "int1".into() => 44,
                            },
                            ints_64: hashmap! {
                                "int2".into() => 42,
                            },
                            string_lists: hashmap! {
                                "list1".into() => vec!("val1".to_owned(), "val2".to_owned()),
                            },
                            int_lists: hashmap! {},
                            int_64_lists: hashmap! {},
                        },
                    },
                ],
                push: PushParams {
                    pure_push_allowed: false,
                    unbundle_commit_limit: None,
                },
                pushrebase: PushrebaseParams {
                    flags: PushrebaseFlags {
                        rewritedates: false,
                        recursion_limit: Some(1024),
                        forbid_p2_root_rebases: false,
                        casefolding_check: false,
                        casefolding_check_excluded_paths: Default::default(),
                        not_generated_filenodes_limit: 500,
                        monitoring_bookmark: None,
                        merge_resolution_excluded_path_prefixes: Default::default(),
                        merge_resolution_take_incoming_prefixes: Default::default(),
                        merge_resolution_take_incoming: false, // request-scoped, not loaded from config
                        merge_resolution_override: MergeResolutionOverride::UseJk,
                        land_instance_id: None,
                        phab_diff_id: None,
                        mainline_parent_first: false,
                    },
                    block_merges: false,
                    emit_obsmarkers: false,
                    globalrev_config: None,
                    populate_git_mapping: false,
                    allow_change_xrepo_mapping_extra: true,
                    remote_mode: PushrebaseRemoteMode::RemoteLandService(Address::Tier(
                        "my-tier".to_string(),
                    )),
                },
                lfs: LfsParams {
                    threshold: Some(1000),
                    rollout_percentage: 56,
                    use_upstream_lfs_server: false,
                },
                readonly: RepoReadOnly::ReadWrite,
                redaction: Redaction::Enabled,
                mirror_upload_redaction_bypass_enabled: false,
                infinitepush: InfinitepushParams {
                    allow_writes: true,
                    namespace: Some(InfinitepushNamespace::new(
                        ComparableRegex::new("foobar/.+").unwrap(),
                    )),
                },
                list_keys_patterns_max: 123,
                hook_max_file_size: 456,
                filestore: Some(FilestoreParams {
                    chunk_size: 768,
                    concurrency: 48,
                }),
                hipster_acl: Some("foo/test".to_string()),
                source_control_service: SourceControlServiceParams {
                    permit_writes: false,
                    permit_service_writes: false,
                    service_write_hipster_acl: None,
                    permit_commits_without_parents: false,
                    service_write_restrictions: Default::default(),
                },
                source_control_service_monitoring: Some(SourceControlServiceMonitoring {
                    bookmarks_to_report_age: vec![
                        BookmarkKey::new("master").unwrap(),
                        BookmarkKey::new("master2").unwrap(),
                    ],
                }),
                derived_data_config: DerivedDataConfig {
                    enabled_config_name: "default".to_string(),
                    available_configs: hashmap!["default".to_string() => DerivedDataTypesConfig {
                        types: hashset! {
                            DerivableType::Fsnodes,
                            DerivableType::Unodes,
                            DerivableType::BlameV2,
                        },
                        ephemeral_bubbles_disabled_types: Default::default(),
                        mapping_key_prefixes: hashmap! {},
                        unode_version: UnodeVersion::V2,
                        blame_filesize_limit: Some(101),
                        hg_set_committer_extra: false,
                        blame_version: BlameVersion::V2,
                        git_delta_manifest_version: Default::default(),
                        git_delta_manifest_v2_config: Default::default(),
                        git_delta_manifest_v3_config: Default::default(),
                        derivation_batch_sizes: hashmap! {
                            DerivableType::Fsnodes => 20,
                            DerivableType::Unodes => 20,
                            DerivableType::BlameV2 => 20,
                        },
                        inferred_copy_from_config: Default::default(),
                        xdb_mapping_shard_ids: Default::default(),
                    },],
                    scuba_table: None,
                    bonsai_hg_mapping_scuba_table: None,
                    remote_derivation_config: None,
                    blocked_derivation: hashmap! {
                        THREES_CSID => Some(hashset! { DerivableType::Unodes, }),
                    },
                    extra_types_available_for_read: hashset! {},
                    wbc_excluded_types: hashset! {},
                    pipeline_config: None,
                },
                enforce_lfs_acl_check: false,
                repo_client_use_warm_bookmarks_cache: true,
                phabricator_callsign: Some("FBS".to_string()),
                acl_region_config: None,
                walker_config: Some(WalkerConfig {
                    scrub_enabled: true,
                    validate_enabled: true,
                    params: None,
                }),
                cross_repo_commit_validation_config: Some(CrossRepoCommitValidation {
                    skip_bookmarks: [BookmarkKey::new("weirdy").unwrap()].into(),
                }),
                sparse_profiles_config: Some(SparseProfilesConfig {
                    sparse_profiles_location: "sparse".to_string(),
                    excluded_paths: vec![],
                    monitored_profiles: vec![],
                }),
                update_logging_config: UpdateLoggingConfig {
                    bookmark_logging_destination: None,
                    new_commit_logging_destination: Some(LoggingDestination::Scribe {
                        scribe_category: "cat".to_string(),
                    }),
                    git_content_refs_logging_destination: Some(LoggingDestination::Logger),
                },
                commit_graph_config: CommitGraphConfig {
                    scuba_table: Some("commit_graph".to_string()),
                    preloaded_commit_graph_blobstore_key: None,
                    disable_commit_graph_v2_with_empty_common: false,
                },
                deep_sharding_config: Some(ShardingModeConfig { status: hashmap!() }),
                lazy_loading_config: None,
                everstore_local_path: None,

                metadata_logger_config: MetadataLoggerConfig {
                    bookmarks: vec![
                        BookmarkKey::new("master").unwrap(),
                        BookmarkKey::new("release").unwrap(),
                    ],
                    sleep_interval_secs: Some(100),
                },
                x_repo_sync_source_mapping: Some(XRepoSyncSourceConfigMapping {
                    mapping: hashmap! {
                        "aros".to_string() => XRepoSyncSourceConfig {
                            bookmark_regex: "master".to_string(),
                            backsync_enabled: true,
                        }
                    }
                    .into_iter()
                    .collect(),
                }),
                zelos_config: None,
                pipeline_zelos_config: None,
                bookmark_name_for_objects_count: None,
                default_objects_count: None,
                override_objects_count: None,
                objects_count_multiplier: None,
                commit_cloud_config: CommitCloudConfig {
                    mocked_employees: Vec::new(),
                    disable_interngraph_notification: false,
                },
                mononoke_cas_sync_config: None,
                git_configs: GitConfigs {
                    git_concurrency: Some(GitConcurrencyParams {
                        trees_and_blobs: 500,
                        commits: 1000,
                        tags: 1000,
                        shallow: 100,
                    }),
                    git_lfs_interpret_pointers: false,
                    fetch_message: None,
                    git_bundle_uri: None,
                    preloaded_cgdm_blobstore_key: None,
                },
                modern_sync_config: None,
                metadata_cache_config: Some(MetadataCacheConfig {
                    wbc_update_mode: Some(MetadataCacheUpdateMode::Tailing {
                        category: "scribe_category".to_string(),
                    }),
                    wbc_event_router_update_mode: Some(MetadataCacheUpdateMode::Tailing {
                        category: "event_router_scribe_category".to_string(),
                    }),
                    wbc_process_bookmark_notifications: true,
                    wbc_event_router_batching_enabled: false,
                    tags_update_mode: Some(MetadataCacheUpdateMode::Polling),
                    content_refs_update_mode: None,
                }),
                restricted_paths_config: RestrictedPathsConfig::default(),
                remote_diff_config: None,
                commit_rate_limit_config: None,
                p4_source: None,
            },
        );

        repos.insert(
            "www".to_string(),
            RepoConfig {
                enable_git_bundle_uri: false,
                default_commit_identity_scheme: CommitIdentityScheme::default(),
                directory_branch_cluster_config: None,
                enabled: true,
                storage_config: StorageConfig {
                    metadata: MetadataDatabaseConfig::Local(LocalDatabaseConfig {
                        path: "/tmp/www".into(),
                    }),
                    blobstore: BlobConfig::Files {
                        path: "/tmp/www".into(),
                    },
                    ephemeral_blobstore: Some(EphemeralBlobstoreConfig {
                        blobstore: BlobConfig::Files {
                            path: "/tmp/www-ephemeral".into(),
                        },
                        metadata: DatabaseConfig::Local(LocalDatabaseConfig {
                            path: "/tmp/www-ephemeral".into(),
                        }),
                        initial_bubble_lifespan: Duration::from_hours(24),
                        bubble_expiration_grace: Duration::from_hours(1),
                        bubble_deletion_mode: BubbleDeletionMode::MarkOnly,
                    }),
                    mutable_blobstore: BlobConfig::Files {
                        path: "/tmp/www".into(),
                    },
                },
                repoid: RepositoryId::new(1),
                scuba_table_hooks: Some("scm_hooks".to_string()),
                scuba_local_path_hooks: None,
                cache_warmup: None,
                hook_manager_params: None,
                bookmarks: vec![],
                hooks: vec![],
                push: Default::default(),
                pushrebase: Default::default(),
                lfs: Default::default(),
                readonly: RepoReadOnly::ReadWrite,
                redaction: Redaction::Enabled,
                mirror_upload_redaction_bypass_enabled: false,
                infinitepush: InfinitepushParams::default(),
                list_keys_patterns_max: LIST_KEYS_PATTERNS_MAX_DEFAULT,
                hook_max_file_size: HOOK_MAX_FILE_SIZE_DEFAULT,
                filestore: None,
                hipster_acl: None,
                source_control_service: SourceControlServiceParams::default(),
                source_control_service_monitoring: None,
                derived_data_config: DerivedDataConfig::default(),
                enforce_lfs_acl_check: false,
                repo_client_use_warm_bookmarks_cache: false,
                phabricator_callsign: Some("WWW".to_string()),
                acl_region_config: None,
                walker_config: None,
                cross_repo_commit_validation_config: None,
                sparse_profiles_config: None,
                update_logging_config: UpdateLoggingConfig::default(),
                commit_graph_config: CommitGraphConfig::default(),
                deep_sharding_config: None,
                lazy_loading_config: None,
                everstore_local_path: None,
                metadata_logger_config: MetadataLoggerConfig::default(),
                zelos_config: None,
                pipeline_zelos_config: None,
                bookmark_name_for_objects_count: None,
                default_objects_count: None,
                override_objects_count: None,
                objects_count_multiplier: None,
                x_repo_sync_source_mapping: None,
                commit_cloud_config: CommitCloudConfig::default(),
                mononoke_cas_sync_config: None,
                git_configs: GitConfigs {
                    git_concurrency: None,
                    git_lfs_interpret_pointers: false,
                    fetch_message: None,
                    git_bundle_uri: None,
                    preloaded_cgdm_blobstore_key: None,
                },
                modern_sync_config: None,
                metadata_cache_config: None,
                restricted_paths_config: RestrictedPathsConfig::default(),
                remote_diff_config: None,
                commit_rate_limit_config: None,
                p4_source: None,
            },
        );
        assert_eq!(
            repoconfig.common,
            CommonConfig {
                trusted_parties_hipster_tier: Some("tier1".to_string()),
                trusted_parties_allowlist: vec![],
                global_allowlist: vec![Identity {
                    id_type: "username".to_string(),
                    id_data: "user".to_string()
                }],
                loadlimiter_category: Some("test-category".to_string()),
                enable_http_control_api: false,
                censored_scuba_params: CensoredScubaParams {
                    table: Some("censored_table".to_string()),
                    local_path: Some("censored_local_path".to_string()),
                },
                redaction_config: RedactionConfig {
                    blobstore: main_storage_config.blobstore,
                    redaction_sets_location: "loc".to_string(),
                },
                internal_identity: Identity {
                    id_type: "SERVICE_IDENTITY".to_string(),
                    id_data: "internal".to_string(),
                },
                git_memory_upper_bound: Some(100),
                edenapi_dumper_scuba_table: Some("dumped_requests".to_string()),
                async_requests_config: AsyncRequestsConfig {
                    db_config: None,
                    blobstore: None
                },
                rl_land_service_repo_prefix: None,
                multi_repo_land_manifest_repos: BTreeMap::new(),
                multi_repo_land_manifest_formats: BTreeMap::new(),
            }
        );

        // Wrap expected values in Arc to match the persistent map.
        let repos: ImHashMap<String, Arc<RepoConfigEntry>> = repos
            .into_iter()
            .map(|(name, config)| (name, Arc::new(RepoConfigEntry::from(config))))
            .collect();
        assert_eq!(
            repoconfig.repos.get("www"),
            repos.get("www"),
            "www mismatch\ngot {:#?}\nwant {:#?}",
            repoconfig.repos.get("www"),
            repos.get("www")
        );
        assert_eq!(
            repoconfig.repos.get("fbsource"),
            repos.get("fbsource"),
            "fbsource mismatch\ngot {:#?}\nwant {:#?}",
            repoconfig.repos.get("fbsource"),
            repos.get("fbsource")
        );

        assert_eq!(
            &repoconfig.repos, &repos,
            "Repo mismatch:\n\
             got:\n\
             {:#?}\n\
             Want:\n\
             {:#?}",
            repoconfig.repos, repos
        )
    }

    #[mononoke::test]
    fn test_broken_bypass_config() {
        // Incorrect bypass string
        let storage = r#"
            [sqlite.metadata.local]
            local_db_path = "/tmp/fbsource"

            [sqlite.blobstore.blob_files]
            path = "/tmp/fbsource"
        "#;
        let content = r#"
            storage_config = "sqlite"

            [[bookmarks]]
            name="master"
            [[bookmarks.hooks]]
            hook_name="hook1"
            [[hooks]]
            name="hook1"
            bypass_pushvar="var"
        "#;

        let content_def = r#"
            repo_id = 0
            repo_name = "fbsource"
            repo_config = "fbsource"
        "#;

        let paths = btreemap! {
            "common/storage.toml" => storage,
            "repos/fbsource/server.toml" => content,
            "repo_definitions/fbsource/server.toml" => content_def,
        };
        let res = load_test_configs(&paths);
        let msg = format!("{res:#?}");
        println!("res = {msg}");
        assert!(res.is_err());
        assert!(msg.contains("InvalidPushvar"));
    }

    /// Load a minimal config set whose only repo has one hook, `hook1`, with
    /// `hook1_fields` appended to its `[[hooks]]` entry.
    fn load_configs_with_hook1(hook1_fields: &str) -> Result<RepoConfigs> {
        let storage = r#"
            [sqlite.metadata.local]
            local_db_path = "/tmp/fbsource"

            [sqlite.blobstore.blob_files]
            path = "/tmp/fbsource"

            [sqlite.mutable_blobstore.blob_files]
            path = "/tmp/fbsource"
        "#;

        let common = r#"
            [redaction_config]
            blobstore = "sqlite"
            redaction_sets_location = "loc"

            [internal_identity]
            identity_type = "SERVICE_IDENTITY"
            identity_data = "internal"
        "#;

        let content = format!(
            r#"
            storage_config = "sqlite"

            [[hooks]]
            name="hook1"
            {hook1_fields}
        "#
        );

        let content_def = r#"
            repo_id = 0
            repo_name = "fbsource"
            repo_config = "fbsource"
        "#;

        let paths = btreemap! {
            "common/storage.toml" => storage,
            "common/common.toml" => common,
            "repos/fbsource/server.toml" => content.as_str(),
            "repo_definitions/fbsource/server.toml" => content_def,
        };
        load_test_configs(&paths)
    }

    #[mononoke::test]
    fn test_hook_bypass_flags_parse() {
        let configs = load_configs_with_hook1(
            r#"
            bypass_permission_group="group1"
            bypass_implicit_for_group=true
            allow_agent_bypass=true
            "#,
        )
        .expect("config should parse");

        assert_eq!(
            configs.repos["fbsource"].config.hooks[0].config.bypasses,
            vec![HookBypassCondition::permission_group("group1").with_actor(HookBypassActor::Any)],
        );
    }

    /// Each legacy marker becomes its own condition, restricted to the
    /// permission group and, without `allow_agent_bypass`, to humans.
    #[mononoke::test]
    fn test_legacy_hook_bypass_fields_become_conditions() {
        let configs = load_configs_with_hook1(
            r#"
            bypass_commit_string="@allow"
            bypass_pushvar="ALLOW=true"
            bypass_permission_group="group1"
            bypass_implicit_for_group=true
            "#,
        )
        .expect("config should parse");

        assert_eq!(
            configs.repos["fbsource"].config.hooks[0].config.bypasses,
            vec![
                HookBypassCondition::pushvar("ALLOW", "true").with_permission_group("group1"),
                HookBypassCondition::commit_message("@allow").with_permission_group("group1"),
                HookBypassCondition::permission_group("group1"),
            ],
        );

        // Without a group, anyone may use the markers, agents included.
        let configs = load_configs_with_hook1(r#"bypass_commit_string="@allow""#)
            .expect("config should parse");
        assert_eq!(
            configs.repos["fbsource"].config.hooks[0].config.bypasses,
            vec![HookBypassCondition::commit_message("@allow").with_actor(HookBypassActor::Any)],
        );
    }

    #[mononoke::test]
    fn test_hook_bypasses_parse() {
        let configs = load_configs_with_hook1(
            r#"
            [[hooks.bypasses]]
            pushvar="ALLOW=true"
            permission_group="group1"

            [[hooks.bypasses]]
            commit_message="@allow"
            identities=["SERVICE_IDENTITY:bot"]
            actor=1  # AGENT
            "#,
        )
        .expect("config should parse");

        assert_eq!(
            configs.repos["fbsource"].config.hooks[0].config.bypasses,
            vec![
                HookBypassCondition::pushvar("ALLOW", "true").with_permission_group("group1"),
                HookBypassCondition {
                    commit_message: Some("@allow".into()),
                    ..HookBypassCondition::identities([MononokeIdentity::from_legacy_type_data(
                        "SERVICE_IDENTITY",
                        "bot",
                    )])
                }
                .with_actor(HookBypassActor::Agent),
            ],
        );
    }

    #[mononoke::test]
    fn test_invalid_hook_bypasses() {
        for (fields, expected) in [
            (
                r#"
                [[hooks.bypasses]]
                actor=2  # ANY
                "#,
                "must set at least one of",
            ),
            (
                r#"
                bypass_commit_string="@allow"
                [[hooks.bypasses]]
                commit_message="@allow"
                "#,
                "must not set bypass_commit_string",
            ),
            (
                r#"
                [[hooks.bypasses]]
                pushvar="ALLOW"
                "#,
                "InvalidPushvar",
            ),
            (
                r#"
                [[hooks.bypasses]]
                identities=["bot"]
                "#,
                "expected TYPE:data",
            ),
        ] {
            let res = load_configs_with_hook1(fields);
            let msg = format!("{res:#?}");
            assert!(msg.contains(expected), "expected {expected:?} in {msg}");
        }
    }

    /// Without a permission group the flags are inert: the config still loads
    /// (a load error here would stop the whole repo from initialising) and no
    /// marker-less bypass is created.
    #[mononoke::test]
    fn test_hook_bypass_flags_without_group_are_inert() {
        let configs = load_configs_with_hook1(
            r#"
            bypass_implicit_for_group=true
            allow_agent_bypass=true
            "#,
        )
        .expect("config should parse");

        assert_eq!(
            configs.repos["fbsource"].config.hooks[0].config.bypasses,
            vec![]
        );
    }

    #[mononoke::test]
    fn test_broken_common_config() {
        fn check_fails(common: &str, expect: &str) {
            let content = r#"
                storage_config = "storage"
            "#;
            let storage = r#"
                [storage.metadata.local]
                local_db_path = "/tmp/fbsource"

                [storage.blobstore.blob_sqlite]
                path = "/tmp/fbsource"

                [storage.mutable_blobstore.blob_files]
                path = "/tmp/foo1"
            "#;

            let content_def = r#"
                repo_id = 0
                repo_name = "fbsource"
                repo_config = "fbsource"
            "#;

            let paths = btreemap! {
                "common/common.toml" => common,
                "common/storage.toml" => storage,
                "repos/fbsource/server.toml" => content,
                "repo_definitions/fbsource/server.toml" => content_def,
            };
            let res = load_test_configs(&paths);
            println!("res = {res:?}");
            let msg = format!("{res:?}");
            assert!(res.is_err(), "unexpected success for {common}");
            assert!(
                msg.contains(expect),
                "wrong failure, wanted \"{expect}\" in {common}"
            );
        }

        let common = r#"
        [[global_allowlist]]
        identity_type="user"
        "#;
        check_fails(common, "identity type and data must be specified");

        let common = r#"
        [[global_allowlist]]
        identity_data="user"
        "#;
        check_fails(common, "identity type and data must be specified");
    }

    #[mononoke::test]
    fn test_common_storage() {
        const STORAGE: &str = r#"
        [multiplex_store.metadata.remote]
        production = { db_address = "some_db" }
        commit_graph = { db_address = "some_db" }
        bookmarks = { db_address = "some_db" }
        bonsai_hg_mapping = { db_address = "some_db" }
        filenodes = { sharded = { shard_map = "some-shards", shard_num = 123 } }
        mutation = { db_address = "some_db" }
        sparse_profiles = { db_address = "some_db" }
        commit_cloud = { db_address = "some_db" }
        git_bundles = { db_address = "git_bundles" }
        repo_metadata = { db_address = "repo_metadata" }
        restricted_paths = { db_address = "restricted_paths" }
        derived_data_mapping = { unsharded = { db_address = "derived_data_mapping" } }

        [multiplex_store.blobstore.multiplexed_wal]
        multiplex_id = 1
        components = [
            { blobstore_id = 1, blobstore = { blob_files = { path = "/tmp/foo" } } },
        ]
        queue_db = { remote = { shard_map = "queue_db_address", shard_num = 1 } }
        write_quorum = 1

        [multiplex_store.mutable_blobstore.multiplexed_wal]
        multiplex_id = 1
        components = [
            { blobstore_id = 1, blobstore = { blob_files = { path = "/tmp/foo" } } },
        ]
        queue_db = { remote = { shard_map = "queue_db_address", shard_num = 1 } }
        write_quorum = 1
        "#;

        const REPO: &str = r#"
        storage_config = "multiplex_store"
        "#;

        const REPO_DEF: &str = r#"
            repo_id = 123
            repo_config = "test"
            repo_name = "test"
        "#;

        const COMMON: &str = r#"
        [redaction_config]
        blobstore = "multiplex_store"
        redaction_sets_location = "loc"

        [internal_identity]
        identity_type = "SERVICE_IDENTITY"
        identity_data = "internal"
        "#;

        let paths = btreemap! {
            "common/storage.toml" => STORAGE,
            "common/common.toml" => COMMON,
            "repos/test/server.toml" => REPO,
            "repo_definitions/test/server.toml" => REPO_DEF,
        };
        let res = load_test_configs(&paths).expect("Read configs failed");

        let expected = hashmap! {
            "test".into() => RepoConfig {
                enabled: true,
                directory_branch_cluster_config: None,
                storage_config: StorageConfig {
                    blobstore: BlobConfig::MultiplexedWal {
                        multiplex_id: MultiplexId::new(1),
                        inner_blobstores_scuba_table: None,
                        multiplex_scuba_table: None,
                        scuba_sample_rate: nonzero!(100u64),
                        blobstores: vec![
                            (BlobstoreId::new(1), MultiplexedStoreType::Normal, BlobConfig::Files {
                                path: "/tmp/foo".into()
                            })
                        ],
                        write_quorum: 1,
                        queue_db: ShardedDatabaseConfig::Sharded(
                            ShardedRemoteDatabaseConfig {
                                shard_map: "queue_db_address".into(),
                                shard_num: nonzero!(1usize),
                            }
                        ),
                    },
                    metadata: MetadataDatabaseConfig::Remote(RemoteMetadataDatabaseConfig {
                        production: RemoteDatabaseConfig {
                            db_address: "some_db".into(),
                        },
                        commit_graph: RemoteDatabaseConfig {
                            db_address: "some_db".into(),
                        },
                        bookmarks: RemoteDatabaseConfig {
                            db_address: "some_db".into(),
                        },
                        bonsai_hg_mapping: RemoteDatabaseConfig {
                            db_address: "some_db".into(),
                        },
                        filenodes: ShardableRemoteDatabaseConfig::Sharded(ShardedRemoteDatabaseConfig {
                            shard_map: "some-shards".into(), shard_num: NonZeroUsize::new(123).unwrap()
                        }),
                        mutation: RemoteDatabaseConfig {
                            db_address: "some_db".into(),
                        },
                        sparse_profiles: RemoteDatabaseConfig {
                            db_address: "some_db".into(),
                        },
                        bonsai_blob_mapping: None,
                        deletion_log: None,
                        commit_cloud:  Some(RemoteDatabaseConfig {
                            db_address: "some_db".into(),
                        }),
                        git_bundle_metadata:  Some(RemoteDatabaseConfig {
                            db_address: "git_bundles".into(),
                        }),
                        repo_metadata: Some(RemoteDatabaseConfig {
                            db_address: "repo_metadata".into(),
                        }),
                        restricted_paths: Some(RemoteDatabaseConfig {
                            db_address: "restricted_paths".into(),
                        }),
                        commit_derived_data_mapping: Some(ShardableRemoteDatabaseConfig::Unsharded(RemoteDatabaseConfig {
                            db_address: "derived_data_mapping".into(),
                        })),
                        repo_manifest_mapping: None,
                    }),
                    ephemeral_blobstore: None,
                    mutable_blobstore: BlobConfig::MultiplexedWal {
                        multiplex_id: MultiplexId::new(1),
                        inner_blobstores_scuba_table: None,
                        multiplex_scuba_table: None,
                        scuba_sample_rate: nonzero!(100u64),
                        blobstores: vec![
                            (BlobstoreId::new(1), MultiplexedStoreType::Normal, BlobConfig::Files {
                                path: "/tmp/foo".into()
                            })
                        ],
                        write_quorum: 1,
                        queue_db: ShardedDatabaseConfig::Sharded(
                            ShardedRemoteDatabaseConfig {
                                shard_map: "queue_db_address".into(),
                                shard_num: nonzero!(1usize),
                            }
                        ),
                    },
                },
                repoid: RepositoryId::new(123),
                list_keys_patterns_max: LIST_KEYS_PATTERNS_MAX_DEFAULT,
                hook_max_file_size: HOOK_MAX_FILE_SIZE_DEFAULT,
                ..Default::default()
            }
        };

        let expected: ImHashMap<String, Arc<RepoConfigEntry>> = expected
            .into_iter()
            .map(|(name, config)| (name, Arc::new(RepoConfigEntry::from(config))))
            .collect();
        assert_eq!(
            res.repos, expected,
            "Got: {:#?}\nWant: {:#?}",
            &res.repos, expected
        )
    }

    #[mononoke::test]
    fn test_repo_storage_definitions_are_refused() {
        const STORAGE: &str = r#"
        [multiplex_store.metadata.remote]
        production = { db_address = "some_db" }
        commit_graph = { db_address = "some_db" }
        bookmarks = { db_address = "some_db" }
        bonsai_hg_mapping = { db_address = "some_db" }
        filenodes = { sharded = { shard_map = "some-shards", shard_num = 123 } }

        [multiplex_store.blobstore.multiplexed_wal]
        multiplex_id = 1
        components = [
            { blobstore_id = 1, blobstore = { blob_files = { path = "/tmp/foo" } } },
        ]
        queue_db = { remote = { shard_map = "queue_db_address", shard_num = 1 } }
        write_quorum = 1

        [multiplex_store.mutable_blobstore.multiplexed_wal]
        multiplex_id = 1
        components = [
            { blobstore_id = 1, blobstore = { blob_files = { path = "/tmp/foo" } } },
        ]
        queue_db = { remote = { shard_map = "queue_db_address", shard_num = 1 } }
        write_quorum = 1



        [manifold_store.metadata.remote]
        production = { db_address = "other_db" }
        commit_graph = { db_address = "other_db" }
        bookmarks = { db_address = "other_db" }
        bonsai_hg_mapping = { db_address = "other_db" }
        filenodes = { sharded = { shard_map = "other-shards", shard_num = 456 } }
        mutation = { db_address = "other_mutation_db" }

        [manifold_store.blobstore.manifold]
        manifold_bucket = "bucketybucket"

        [manifold_store.mutable_blobstore.manifold]
        manifold_bucket = "mutable_bucketybucket"
        "#;

        const REPO: &str = r#"
        storage_config = "multiplex_store"

        [storage.multiplex_store.metadata.remote]
        production = { db_address = "other_other_db" }
        commit_graph = { db_address = "other_other_db" }
        bookmarks = { db_address = "other_other_db" }
        bonsai_hg_mapping = { db_address = "other_other_db" }
        filenodes = { sharded = { shard_map = "other-other-shards", shard_num = 789 } }
        mutation = { db_address = "other_other_mutation_db" }
        sparse_profiles = { db_address = "test_db" }
        commit_cloud = { db_address = "other_other_other_mutation_db" }
        git_bundles = { db_address = "git_bundles" }
        repo_metadata = { db_address = "repo_metadata" }
        restricted_paths = { db_address = "restricted_paths" }
        derived_data_mapping = { unsharded = { db_address = "derived_data_mapping" } }

        [storage.multiplex_store.blobstore]
        disabled = {}

        [storage.multiplex_store.mutable_blobstore]
        disabled = {}
        "#;

        const REPO_DEF: &str = r#"
        repo_id = 123
        repo_config = "test"
        repo_name = "test"
        "#;

        const COMMON: &str = r#"
        [redaction_config]
        blobstore = "multiplex_store"
        redaction_sets_location = "loc"

        [internal_identity]
        identity_type = "SERVICE_IDENTITY"
        identity_data = "internal"
        "#;

        let paths = btreemap! {
            "common/storage.toml" => STORAGE,
            "common/common.toml" => COMMON,
            "repos/test/server.toml" => REPO,
            "repo_definitions/test/server.toml" => REPO_DEF,
        };
        let error = load_test_configs(&paths).expect_err("a repo cannot define storage");
        assert!(
            format!("{error:#}").contains("repo_config.storage is not supported"),
            "{error:#}"
        );
    }

    #[mononoke::test]
    fn test_multiplexed_store_types() {
        const STORAGE: &str = r#"
        [multiplex_store.metadata.remote]
        production = { db_address = "some_db" }
        commit_graph = { db_address = "some_db" }
        bookmarks = { db_address = "some_db" }
        bonsai_hg_mapping = { db_address = "some_db" }
        filenodes = { sharded = { shard_map = "some-shards", shard_num = 123 } }

        [multiplex_store.blobstore.multiplexed_wal]
        multiplex_id = 1
        components = [
            { blobstore_id = 1, blobstore = { blob_files = { path = "/tmp/foo1" } } },
            { blobstore_id = 2, store_type = { normal = {}}, blobstore = { blob_files = { path = "/tmp/foo2" } } },
            { blobstore_id = 3, store_type = { write_only = {}}, blobstore = { blob_files = { path = "/tmp/foo3" } } },
        ]
        queue_db = { remote = { shard_map = "queue_db_address", shard_num = 1 } }
        write_quorum = 2

        [multiplex_store.mutable_blobstore.blob_files]
        path = "/tmp/foo4"

        "#;

        const REPO: &str = r#"
        storage_config = "multiplex_store"
        "#;

        const REPO_DEF: &str = r#"
        repo_id = 123
        repo_name = "test"
        repo_config = "test"
        "#;

        const COMMON: &str = r#"
        [redaction_config]
        blobstore = "multiplex_store"
        redaction_sets_location = "loc"

        [internal_identity]
        identity_type = "SERVICE_IDENTITY"
        identity_data = "internal"
        "#;

        let paths = btreemap! {
            "common/storage.toml" => STORAGE,
            "common/common.toml" => COMMON,
            "repos/test/server.toml" => REPO,
            "repo_definitions/test/server.toml" => REPO_DEF,
        };
        let res = load_test_configs(&paths).expect("Read configs failed");

        if let BlobConfig::MultiplexedWal { blobstores, .. } =
            &res.repos["test"].config.storage_config.blobstore
        {
            let expected_blobstores = vec![
                (
                    BlobstoreId::new(1),
                    MultiplexedStoreType::Normal,
                    BlobConfig::Files {
                        path: "/tmp/foo1".into(),
                    },
                ),
                (
                    BlobstoreId::new(2),
                    MultiplexedStoreType::Normal,
                    BlobConfig::Files {
                        path: "/tmp/foo2".into(),
                    },
                ),
                (
                    BlobstoreId::new(3),
                    MultiplexedStoreType::WriteOnly,
                    BlobConfig::Files {
                        path: "/tmp/foo3".into(),
                    },
                ),
            ];

            assert_eq!(
                blobstores, &expected_blobstores,
                "Blobstores parsed from config are wrong"
            );
        } else {
            panic!("Multiplexed config is not a multiplexed blobstore");
        }
    }

    #[mononoke::test]
    fn test_tier_manifest_and_repo_config_handles() {
        use cached_config::ModificationTime;

        let manifest_json = r#"{
            "repos": [
                {
                    "repo_name": "test_repo",
                    "repo_id": 42,
                    "config_path": "scm/mononoke/repos/test_repo",
                    "is_deep_sharded": false
                }
            ],
            "common": {},
            "storage": {}
        }"#;

        let repo_config_json = r#"{
            "storage_config": "main_storage"
        }"#;

        let test_source = Arc::new(TestSource::new());
        test_source.insert_config(
            "scm/mononoke/manifests/tiers/test_tier",
            manifest_json,
            ModificationTime::UnixTimestamp(1),
        );
        test_source.insert_config(
            "scm/mononoke/repos/test_repo",
            repo_config_json,
            ModificationTime::UnixTimestamp(1),
        );

        let config_store = ConfigStore::new(test_source, None, None);

        // Verify manifest handle can be created and deserialized
        let manifest_handle =
            configerator_manifest_handle("scm/mononoke/manifests/tiers/test_tier", &config_store)
                .expect("Failed to get manifest config handle");

        let manifest = manifest_handle.get();
        assert_eq!(manifest.repos.len(), 1);
        assert_eq!(manifest.repos[0].repo_name, "test_repo");
        assert_eq!(manifest.repos[0].repo_id, 42);
        assert_eq!(
            manifest.repos[0].config_path,
            "scm/mononoke/repos/test_repo"
        );
        assert!(!manifest.repos[0].is_deep_sharded);

        // Verify repo config handle can be created and deserialized
        let repo_config_handle =
            configerator_repo_config_handle("scm/mononoke/repos/test_repo", &config_store)
                .expect("Failed to get repo config handle");

        let repo_config = repo_config_handle.get();
        assert_eq!(repo_config.storage_config, Some("main_storage".to_string()));
    }

    #[mononoke::test]
    fn test_load_repo_configs_from_manifest() {
        use cached_config::ModificationTime;

        // A TierManifest listing one repo, with a named storage config the
        // repo's RepoSpec references (so parse_repo_spec can resolve storage).
        let manifest_json = r#"{
            "repos": [
                {
                    "repo_name": "test/repo",
                    "repo_id": 42,
                    "config_path": "scm/mononoke/repos/git/00/test_repo",
                    "is_deep_sharded": false
                }
            ],
            "common": {
                "internal_identity": {
                    "identity_type": "SERVICE_IDENTITY",
                    "identity_data": "internal"
                },
                "redaction_config": {
                    "blobstore": "test_storage",
                    "redaction_sets_location": "loc"
                }
            },
            "storage": {
                "test_storage": {
                    "metadata": {"local": {"local_db_path": "/tmp/test_db"}},
                    "blobstore": {"disabled": {}},
                    "mutable_blobstore": {"disabled": {}}
                }
            }
        }"#;

        // The per-repo RepoSpec, with a tier override for "test_tier".
        let repo_spec_json = r#"{
            "repo_id": 42,
            "repo_name": "test/repo",
            "hipster_acl": "acl.test.repo",
            "enabled": true,
            "readonly": false,
            "default_commit_identity_scheme": 3,
            "repo_config": {
                "storage_config": "test_storage",
                "phabricator_callsign": "BASE"
            },
            "tier_overrides": {
                "test_tier": {"phabricator_callsign": "OVERRIDE"}
            }
        }"#;

        let test_source = Arc::new(TestSource::new());
        test_source.insert_config(
            "scm/mononoke/repos/tiers/test_tier_manifest",
            manifest_json,
            ModificationTime::UnixTimestamp(1),
        );
        test_source.insert_config(
            "scm/mononoke/repos/git/00/test_repo",
            repo_spec_json,
            ModificationTime::UnixTimestamp(1),
        );
        let config_store = ConfigStore::new(test_source, None, None);

        let repo_configs = load_repo_configs_from_manifest("test_tier", &config_store)
            .expect("load_repo_configs_from_manifest should succeed");

        // Regression guard for the silent "0 repos" Layer-3 failure: the loader
        // must return the repos the manifest lists.
        assert_eq!(
            repo_configs.repos.len(),
            1,
            "Should load exactly the one repo listed in the manifest"
        );

        let repo = repo_configs
            .get("test/repo")
            .expect("test/repo should be present");
        assert_eq!(repo.repoid, RepositoryId::new(42));

        // The tier name must be threaded through so tier_overrides resolve:
        // the "test_tier" override must win over the base callsign.
        assert_eq!(
            repo.phabricator_callsign,
            Some("OVERRIDE".to_string()),
            "tier_overrides for the requested tier should be applied"
        );
    }

    /// Builds a `ConfigStore` for `load_repo_configs_from_manifest` tests: a
    /// manifest at `scm/mononoke/repos/tiers/test_tier_manifest` containing the
    /// given JSON entries, plus per-repo RepoSpec configs at the given paths.
    fn manifest_test_config_store(
        manifest_entries_json: &str,
        repo_specs: &[(&str, &str)],
    ) -> ConfigStore {
        use cached_config::ModificationTime;

        let manifest_json = format!(
            r#"{{
            "repos": [{manifest_entries_json}],
            "common": {{
                "internal_identity": {{
                    "identity_type": "SERVICE_IDENTITY",
                    "identity_data": "internal"
                }},
                "redaction_config": {{
                    "blobstore": "test_storage",
                    "redaction_sets_location": "loc"
                }}
            }},
            "storage": {{
                "test_storage": {{
                    "metadata": {{"local": {{"local_db_path": "/tmp/test_db"}}}},
                    "blobstore": {{"disabled": {{}}}},
                    "mutable_blobstore": {{"disabled": {{}}}}
                }}
            }}
        }}"#
        );

        let test_source = Arc::new(TestSource::new());
        test_source.insert_config(
            "scm/mononoke/repos/tiers/test_tier_manifest",
            &manifest_json,
            ModificationTime::UnixTimestamp(1),
        );
        for (path, spec_json) in repo_specs {
            test_source.insert_config(path, spec_json, ModificationTime::UnixTimestamp(1));
        }
        ConfigStore::new(test_source, None, None)
    }

    /// A minimal valid RepoSpec JSON whose storage resolves against the
    /// `test_storage` entry provided by [`manifest_test_config_store`].
    fn manifest_test_repo_spec_json(repo_id: i32, repo_name: &str, storage_config: &str) -> String {
        format!(
            r#"{{
            "repo_id": {repo_id},
            "repo_name": "{repo_name}",
            "hipster_acl": "acl.test.repo",
            "enabled": true,
            "readonly": false,
            "default_commit_identity_scheme": 3,
            "repo_config": {{"storage_config": "{storage_config}"}}
        }}"#
        )
    }

    #[mononoke::test]
    fn test_load_repo_configs_from_manifest_duplicate_repo_id() {
        // Two manifest entries with distinct names but the same repo_id must
        // be rejected: RepoConfigs keys repos by name, so both would silently
        // coexist while repos_by_id can only point at one of them.
        let entries = r#"
            {"repo_name": "repo_a", "repo_id": 1, "config_path": "scm/mononoke/repos/git/aa/repo_a", "is_deep_sharded": false},
            {"repo_name": "repo_b", "repo_id": 1, "config_path": "scm/mononoke/repos/git/bb/repo_b", "is_deep_sharded": false}
        "#;
        let config_store = manifest_test_config_store(
            entries,
            &[
                (
                    "scm/mononoke/repos/git/aa/repo_a",
                    &manifest_test_repo_spec_json(1, "repo_a", "test_storage"),
                ),
                (
                    "scm/mononoke/repos/git/bb/repo_b",
                    &manifest_test_repo_spec_json(1, "repo_b", "test_storage"),
                ),
            ],
        );

        let err = load_repo_configs_from_manifest("test_tier", &config_store)
            .expect_err("two manifest entries sharing repo_id 1 must fail the load");
        match err.downcast_ref::<ConfigurationError>() {
            Some(ConfigurationError::DuplicatedRepoId(id)) => assert_eq!(
                *id,
                RepositoryId::new(1),
                "error should carry the duplicated repo id"
            ),
            other => panic!("expected DuplicatedRepoId, got {other:?} (full error: {err:#})"),
        }
    }

    #[mononoke::test]
    fn test_load_repo_configs_from_manifest_repo_id_mismatch() {
        // The manifest claims repo_id 1 for repo_a, but the RepoSpec its
        // config_path points at says repo_id 2. Trusting either side silently
        // would corrupt the repos_by_id index, so the load must fail.
        let entries = r#"{"repo_name": "repo_a", "repo_id": 1, "config_path": "scm/mononoke/repos/git/aa/repo_a", "is_deep_sharded": false}"#;
        let config_store = manifest_test_config_store(
            entries,
            &[(
                "scm/mononoke/repos/git/aa/repo_a",
                &manifest_test_repo_spec_json(2, "repo_a", "test_storage"),
            )],
        );

        let err = load_repo_configs_from_manifest("test_tier", &config_store)
            .expect_err("manifest repo_id 1 vs RepoSpec repo_id 2 must fail the load");
        match err.downcast_ref::<ConfigurationError>() {
            Some(ConfigurationError::RepoIdMismatch {
                repo_name,
                manifest_repo_id,
                spec_repo_id,
            }) => {
                assert_eq!(repo_name, "repo_a", "error should name the repo");
                assert_eq!(
                    *manifest_repo_id,
                    RepositoryId::new(1),
                    "error should carry the manifest's repo id"
                );
                assert_eq!(
                    *spec_repo_id,
                    RepositoryId::new(2),
                    "error should carry the RepoSpec's repo id"
                );
            }
            other => panic!("expected RepoIdMismatch, got {other:?} (full error: {err:#})"),
        }
    }

    #[mononoke::test]
    fn test_load_repo_configs_from_manifest_missing_repo_spec() {
        // A manifest entry whose config_path resolves to nothing must fail
        // with an error naming the repo and the dangling path, so the broken
        // entry can be found among thousands.
        let entries = r#"{"repo_name": "repo_a", "repo_id": 1, "config_path": "scm/mononoke/repos/git/aa/repo_a", "is_deep_sharded": false}"#;
        // No RepoSpec inserted for the config_path.
        let config_store = manifest_test_config_store(entries, &[]);

        let err = load_repo_configs_from_manifest("test_tier", &config_store)
            .expect_err("a dangling config_path must fail the load");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("repo_a"),
            "error should name the repo, got: {msg}"
        );
        assert!(
            msg.contains("scm/mononoke/repos/git/aa/repo_a"),
            "error should name the dangling config_path, got: {msg}"
        );
    }

    #[mononoke::test]
    fn test_load_repo_configs_from_manifest_is_fail_fast() {
        // Documents the EXISTING all-or-nothing behavior of the bulk loader:
        // one bad RepoSpec among N fails the entire load, via the
        // `collect::<Result<..>>()?` in load_repo_configs_from_manifest. This
        // is by design — the server path isolates per-repo failures via
        // batch_load_repo_configs_checked, while this bulk loader stays
        // fail-fast so a broken repo cannot silently vanish from the tier.
        let entries = r#"
            {"repo_name": "repo_a", "repo_id": 1, "config_path": "scm/mononoke/repos/git/aa/repo_a", "is_deep_sharded": false},
            {"repo_name": "repo_b", "repo_id": 2, "config_path": "scm/mononoke/repos/git/bb/repo_b", "is_deep_sharded": false}
        "#;
        let config_store = manifest_test_config_store(
            entries,
            &[
                (
                    "scm/mononoke/repos/git/aa/repo_a",
                    &manifest_test_repo_spec_json(1, "repo_a", "test_storage"),
                ),
                // repo_b's RepoSpec references a storage config that does not
                // exist in the manifest, so parsing it must fail.
                (
                    "scm/mononoke/repos/git/bb/repo_b",
                    &manifest_test_repo_spec_json(2, "repo_b", "no_such_storage"),
                ),
            ],
        );

        let err = load_repo_configs_from_manifest("test_tier", &config_store)
            .expect_err("one bad RepoSpec among N must fail the whole load (fail-fast by design)");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("repo_b"),
            "error should point at the broken repo, got: {msg}"
        );
    }

    /// Helper to construct a minimal valid RawStorageConfig for tests.
    fn test_raw_storage_config() -> RawStorageConfig {
        use repos::RawBlobstoreConfig;
        use repos::RawBlobstoreDisabled;
        use repos::RawDbLocal;
        use repos::RawMetadataConfig;

        RawStorageConfig {
            metadata: RawMetadataConfig::local(RawDbLocal {
                local_db_path: "/tmp/test_db".to_string(),
            }),
            blobstore: RawBlobstoreConfig::disabled(RawBlobstoreDisabled {}),
            ephemeral_blobstore: None,
            mutable_blobstore: RawBlobstoreConfig::disabled(RawBlobstoreDisabled {}),
        }
    }

    #[mononoke::test]
    fn test_merge_raw_repo_config_override_replaces_set_fields() {
        // Base config has storage_config set
        let base = RawRepoConfig {
            storage_config: Some("base_storage".to_string()),
            ..Default::default()
        };

        // Override sets a different storage_config and adds a phabricator_callsign
        let tier_override = RawRepoConfig {
            storage_config: Some("override_storage".to_string()),
            phabricator_callsign: Some("TEST".to_string()),
            ..Default::default()
        };

        let merged = merge_raw_repo_config(base, &tier_override).expect("merge should succeed");

        assert_eq!(
            merged.storage_config,
            Some("override_storage".to_string()),
            "Override should replace base storage_config"
        );
        assert_eq!(
            merged.phabricator_callsign,
            Some("TEST".to_string()),
            "Override should add phabricator_callsign"
        );
    }

    #[mononoke::test]
    fn test_merge_raw_repo_config_null_fields_preserve_base() {
        let base = RawRepoConfig {
            storage_config: Some("base_storage".to_string()),
            phabricator_callsign: Some("BASE_CALLSIGN".to_string()),
            ..Default::default()
        };

        // Override only sets storage_config, leaves phabricator_callsign as None
        let tier_override = RawRepoConfig {
            storage_config: Some("override_storage".to_string()),
            ..Default::default()
        };

        let merged = merge_raw_repo_config(base, &tier_override).expect("merge should succeed");

        assert_eq!(
            merged.storage_config,
            Some("override_storage".to_string()),
            "Override should replace storage_config"
        );
        assert_eq!(
            merged.phabricator_callsign,
            Some("BASE_CALLSIGN".to_string()),
            "Null override field should preserve base value"
        );
    }

    #[mononoke::test]
    fn test_merge_raw_repo_config_empty_override() {
        let base = RawRepoConfig {
            storage_config: Some("base_storage".to_string()),
            phabricator_callsign: Some("CALLSIGN".to_string()),
            ..Default::default()
        };

        // Completely empty override should preserve all base fields
        let tier_override = RawRepoConfig::default();

        let merged = merge_raw_repo_config(base, &tier_override).expect("merge should succeed");

        assert_eq!(merged.storage_config, Some("base_storage".to_string()));
        assert_eq!(merged.phabricator_callsign, Some("CALLSIGN".to_string()));
    }

    #[mononoke::test]
    fn test_parse_repo_spec_basic() {
        use repos::RawCommitIdentityScheme;
        use repos::RepoSpec;

        let named_storage = hashmap! {
            "test_storage".to_string() => test_raw_storage_config(),
        };

        let repo_spec = RepoSpec {
            repo_id: 42,
            repo_name: "test/repo".to_string(),
            hipster_acl: "acl.test.repo".to_string(),
            enabled: true,
            readonly: false,
            default_commit_identity_scheme: RawCommitIdentityScheme::GIT,
            enable_git_bundle_uri: Some(true),
            tiers: vec!["scs".to_string()],
            repo_config: Some(RawRepoConfig {
                storage_config: Some("test_storage".to_string()),
                ..Default::default()
            }),
            tier_overrides: None,
            ..Default::default()
        };

        let result =
            parse_repo_spec(repo_spec, "scs", &named_storage).expect("parse should succeed");

        assert_eq!(result.repoid, RepositoryId::new(42));
        assert!(result.enabled);
        assert_eq!(result.hipster_acl, Some("acl.test.repo".to_string()));
        assert_eq!(result.readonly, RepoReadOnly::ReadWrite);
        assert_eq!(
            result.default_commit_identity_scheme,
            CommitIdentityScheme::GIT
        );
    }

    #[mononoke::test]
    fn test_parse_repo_spec_with_tier_overrides() {
        use repos::RawCommitIdentityScheme;
        use repos::RepoSpec;

        let named_storage = hashmap! {
            "test_storage".to_string() => test_raw_storage_config(),
        };

        let repo_spec = RepoSpec {
            repo_id: 100,
            repo_name: "test/overridden".to_string(),
            hipster_acl: "acl.test.overridden".to_string(),
            enabled: true,
            readonly: false,
            default_commit_identity_scheme: RawCommitIdentityScheme::GIT,
            enable_git_bundle_uri: None,
            tiers: vec!["scs".to_string(), "gitimport".to_string()],
            repo_config: Some(RawRepoConfig {
                storage_config: Some("test_storage".to_string()),
                phabricator_callsign: Some("BASE".to_string()),
                ..Default::default()
            }),
            tier_overrides: Some(hashmap! {
                "scs".to_string() => RawRepoConfig {
                    phabricator_callsign: Some("SCS_OVERRIDE".to_string()),
                    ..Default::default()
                },
            }),
            ..Default::default()
        };

        // Parse for the "scs" tier — should get the overridden callsign
        let scs_result = parse_repo_spec(repo_spec.clone(), "scs", &named_storage)
            .expect("parse for scs should succeed");

        // Parse for the "gitimport" tier — no override, should get base callsign
        let gitimport_result = parse_repo_spec(repo_spec.clone(), "gitimport", &named_storage)
            .expect("parse for gitimport should succeed");

        // SCS tier should use the overridden callsign
        assert_eq!(
            scs_result.phabricator_callsign,
            Some("SCS_OVERRIDE".to_string()),
            "SCS tier should get the overridden callsign"
        );

        // Gitimport should preserve base callsign
        assert_eq!(
            gitimport_result.phabricator_callsign,
            Some("BASE".to_string()),
            "Gitimport tier should preserve base callsign"
        );
    }

    #[mononoke::test]
    fn test_parse_repo_spec_readonly() {
        use repos::RawCommitIdentityScheme;
        use repos::RepoSpec;

        let named_storage = hashmap! {
            "test_storage".to_string() => test_raw_storage_config(),
        };

        let repo_spec = RepoSpec {
            repo_id: 200,
            repo_name: "test/readonly".to_string(),
            hipster_acl: "acl.test.readonly".to_string(),
            enabled: false,
            readonly: true,
            default_commit_identity_scheme: RawCommitIdentityScheme::GIT,
            tiers: vec!["scs".to_string()],
            repo_config: Some(RawRepoConfig {
                storage_config: Some("test_storage".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let result =
            parse_repo_spec(repo_spec, "scs", &named_storage).expect("parse should succeed");

        assert!(!result.enabled, "enabled should come from RepoSpec");
        assert_eq!(
            result.readonly,
            RepoReadOnly::ReadOnly("Set by config option".to_string()),
            "readonly=true should produce ReadOnly"
        );
    }

    #[mononoke::test]
    fn test_parse_repo_spec_no_repo_config() {
        use repos::RawCommitIdentityScheme;
        use repos::RepoSpec;

        let named_storage = hashmap! {
            "test_storage".to_string() => test_raw_storage_config(),
        };

        // RepoSpec with no repo_config (uses default RawRepoConfig)
        let repo_spec = RepoSpec {
            repo_id: 300,
            repo_name: "test/default".to_string(),
            hipster_acl: "acl.test.default".to_string(),
            enabled: true,
            readonly: false,
            default_commit_identity_scheme: RawCommitIdentityScheme::GIT,
            tiers: vec!["scs".to_string()],
            repo_config: None,
            ..Default::default()
        };

        // This should fail because default RawRepoConfig has no storage_config
        let result = parse_repo_spec(repo_spec, "scs", &named_storage);
        assert!(
            result.is_err(),
            "RepoSpec with no repo_config should fail (missing storage_config)"
        );
    }

    fn p4_repo_spec(depot: &str) -> repos::RepoSpec {
        use repos::RawCommitIdentityScheme;
        use repos::RawP4SourceConfig;
        use repos::RepoSpec;

        RepoSpec {
            repo_id: 200001,
            repo_name: "p4/test1/swarm_playground".to_string(),
            hipster_acl: "repos/p4/test1/swarm_playground".to_string(),
            enabled: true,
            readonly: true,
            default_commit_identity_scheme: RawCommitIdentityScheme::BONSAI,
            tiers: vec!["p4_sync".to_string()],
            repo_config: Some(RawRepoConfig {
                storage_config: Some("test_storage".to_string()),
                p4_source: Some(RawP4SourceConfig {
                    server_alias: "test1".to_string(),
                    depot: depot.to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[mononoke::test]
    fn test_parse_repo_spec_p4_source() {
        let named_storage = hashmap! {
            "test_storage".to_string() => test_raw_storage_config(),
        };

        let result = parse_repo_spec(
            p4_repo_spec("//swarm_playground/"),
            "p4_sync",
            &named_storage,
        )
        .expect("parse should succeed");
        assert_eq!(
            result.p4_source,
            Some(P4SourceConfig {
                server_alias: "test1".to_string(),
                depot: "//swarm_playground/".to_string(),
            })
        );

        // A repo spec without the block parses to None, not an error.
        let mut no_p4 = p4_repo_spec("//swarm_playground/");
        no_p4.repo_config.as_mut().unwrap().p4_source = None;
        let result =
            parse_repo_spec(no_p4, "p4_sync", &named_storage).expect("parse should succeed");
        assert_eq!(result.p4_source, None);
    }

    #[mononoke::test]
    fn test_parse_repo_spec_p4_source_rejects_bad_depot() {
        let named_storage = hashmap! {
            "test_storage".to_string() => test_raw_storage_config(),
        };

        for depot in [
            "swarm_playground",
            "//swarm_playground",
            "/swarm_playground/",
            "//swarm_playground/...",
            "//swarm_playground/sub/",
            "///",
            "",
        ] {
            let result = parse_repo_spec(p4_repo_spec(depot), "p4_sync", &named_storage);
            assert!(
                result.is_err(),
                "depot {depot:?} is not a depot root and must be rejected"
            );
        }
    }
}
