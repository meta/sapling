/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::str::FromStr;
use std::sync::Arc;

use anyhow::Result;
use context::CoreContext;
use fbinit::FacebookInit;
use metaconfig_types::PathRestrictionMetadata;
use metaconfig_types::RestrictedPathsManifestIdStoreConfig;
use mononoke_types::DerivableType;
use mononoke_types::NonRootMPath;
use mononoke_types::RepoPath;
use mononoke_types::RepositoryId;
use permission_checker::AclProvider;
use permission_checker::MononokeIdentity;
use permission_checker::dummy::DummyAclProvider;
use repo_derived_data::RepoDerivedDataArc;
use scuba_ext::MononokeScubaSampleBuilder;
use sql_construct::SqlConstruct;
use test_repo_factory::TestRepoFactory;

use crate::ManifestType;
use crate::RestrictedManifestId;
use crate::RestrictedPathManifestIdEntry;
use crate::RestrictedPaths;
use crate::RestrictedPathsConfig;
use crate::RestrictedPathsConfigBased;
use crate::SqlRestrictedPathsManifestIdStoreBuilder;

#[facet::container]
struct MinimalTestRepo(
    repo_derived_data::RepoDerivedData,
    restricted_paths_common::RestrictedPathsConfigBased,
);

pub(crate) struct RestrictedPathsConfigBuilder {
    config: RestrictedPathsConfig,
}

impl RestrictedPathsConfigBuilder {
    pub(crate) fn new() -> Self {
        Self {
            config: RestrictedPathsConfig {
                manifest_id_store_config: RestrictedPathsManifestIdStoreConfig {
                    use_manifest_id_cache: true,
                    cache_update_interval_ms: 100,
                    ..Default::default()
                },
                ..Default::default()
            },
        }
    }

    pub(crate) fn with_path_restriction_metadata(
        mut self,
        path: &str,
        identity: &str,
    ) -> Result<Self> {
        self.config.path_restriction_metadata.insert(
            NonRootMPath::new(path)?,
            PathRestrictionMetadata {
                repo_region_acl: MononokeIdentity::from_str(identity)?,
                permission_request_group: None,
                rollout_allowlist_group: None,
                read_only: false,
            },
        );
        Ok(self)
    }

    pub(crate) fn build(self) -> RestrictedPathsConfig {
        self.config
    }
}

impl Default for RestrictedPathsConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) async fn build_test_restricted_paths_with_dummy_acl_provider(
    fb: FacebookInit,
    config: RestrictedPathsConfig,
) -> Result<RestrictedPaths> {
    build_test_restricted_paths(fb, config, DummyAclProvider::new(fb)?).await
}

pub(crate) async fn build_test_restricted_paths(
    fb: FacebookInit,
    config: RestrictedPathsConfig,
    acl_provider: Arc<dyn AclProvider>,
) -> Result<RestrictedPaths> {
    build_test_restricted_paths_with_options(fb, config, acl_provider, true).await
}

pub(crate) async fn build_test_restricted_paths_with_options(
    fb: FacebookInit,
    config: RestrictedPathsConfig,
    acl_provider: Arc<dyn AclProvider>,
    acl_manifest_derivation_enabled: bool,
) -> Result<RestrictedPaths> {
    let mut factory = TestRepoFactory::new(fb)?;
    if !acl_manifest_derivation_enabled {
        factory.with_config_override(|repo_config| {
            if let Some(dd_config) = repo_config.derived_data_config.get_active_config_mut() {
                dd_config.types.remove(&DerivableType::AclManifests);
            }
        });
    }
    let test_repo: MinimalTestRepo = factory.build().await?;
    let repo_derived_data = test_repo.repo_derived_data_arc();
    let repo_id = RepositoryId::new(0);
    let manifest_id_store = Arc::new(
        SqlRestrictedPathsManifestIdStoreBuilder::with_sqlite_in_memory()?.with_repo_id(repo_id),
    );
    let scuba = MononokeScubaSampleBuilder::with_discard();
    let config_based = Arc::new(RestrictedPathsConfigBased::new(
        config,
        manifest_id_store,
        None,
    ));

    RestrictedPaths::new(
        "repo".to_string(),
        config_based,
        acl_provider,
        scuba,
        repo_derived_data,
    )
}

pub(crate) async fn add_manifest_entry(
    ctx: &CoreContext,
    restricted_paths: &RestrictedPaths,
    manifest_type: ManifestType,
    manifest_id: RestrictedManifestId,
    path: &str,
) -> Result<()> {
    restricted_paths
        .config_based()
        .manifest_id_store()
        .add_entry(
            ctx,
            RestrictedPathManifestIdEntry::new(manifest_type, manifest_id, RepoPath::dir(path)?)?,
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use context::CoreContext;
    use mononoke_macros::mononoke;

    use super::*;

    // Config builder helpers.
    #[mononoke::test]
    fn test_restricted_paths_config_builder_sets_common_fields() -> Result<()> {
        let config = RestrictedPathsConfigBuilder::new()
            .with_path_restriction_metadata("restricted", "REPO_REGION:restricted_acl")?
            .build();

        assert!(config.manifest_id_store_config.use_manifest_id_cache);
        assert_eq!(
            config.manifest_id_store_config.cache_update_interval_ms,
            100
        );
        assert_eq!(config.path_restriction_metadata.len(), 1);
        Ok(())
    }

    // Manifest-id store helpers.
    #[mononoke::fbinit_test]
    async fn test_add_manifest_entry_stores_manifest_path(fb: FacebookInit) -> Result<()> {
        let restricted_paths = build_test_restricted_paths_with_dummy_acl_provider(
            fb,
            RestrictedPathsConfig::default(),
        )
        .await?;
        let ctx = CoreContext::test_mock(fb);
        let manifest_id = RestrictedManifestId::from("4444444444444444444444444444444444444444");

        add_manifest_entry(
            &ctx,
            &restricted_paths,
            ManifestType::Hg,
            manifest_id.clone(),
            "restricted/dir",
        )
        .await?;

        assert_eq!(
            restricted_paths
                .config_based()
                .manifest_id_store()
                .get_paths_by_manifest_id(&ctx, &manifest_id, &ManifestType::Hg)
                .await?,
            vec![NonRootMPath::new("restricted/dir")?],
        );
        Ok(())
    }
}
