/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Repo derived data
//!
//! Stores configuration and state for data derivation.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use bonsai_git_mapping::BonsaiGitMapping;
use bonsai_hg_mapping::BonsaiHgMapping;
use commit_derived_data_mapping::CommitDerivedDataMapping;
use commit_graph::CommitGraph;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use derived_data_manager::BonsaiDerivable;
use derived_data_manager::DerivationError;
use derived_data_manager::DerivedDataManager;
use derived_data_manager::SharedDerivationError;
use derived_data_remote::DerivationClient;
use ephemeral_blobstore::Bubble;
use filenodes::Filenodes;
use filestore::FilestoreConfig;
use metaconfig_types::DerivedDataConfig;
use metaconfig_types::DerivedDataTypesConfig;
use metaconfig_types::RepoConfig;
use mononoke_types::ChangesetId;
use mononoke_types::RepositoryId;
use repo_blobstore::RepoBlobstore;
use restricted_paths_common::ArcRestrictedPathsConfigBased;
use scuba_ext::MononokeScubaSampleBuilder;

pub mod audit;

/// Repository derived data management.
#[facet::facet]
pub struct RepoDerivedData {
    /// Configuration for derived data.
    config: DerivedDataConfig,

    /// Hashmap of config name to derived data manager for this repo.
    managers: HashMap<String, DerivedDataManager>,

    /// Derived data manager for the enabled types on this repo.
    enabled_manager: DerivedDataManager,
}

impl RepoDerivedData {
    /// Construct a new RepoDerivedData.
    pub fn new(
        repo_id: RepositoryId,
        repo_name: String,
        commit_graph: Arc<CommitGraph>,
        bonsai_hg_mapping: Arc<dyn BonsaiHgMapping>,
        bonsai_git_mapping: Arc<dyn BonsaiGitMapping>,
        filenodes: Arc<dyn Filenodes>,
        repo_blobstore: RepoBlobstore,
        repo_config: Arc<RepoConfig>,
        filestore_config: FilestoreConfig,
        scuba: MononokeScubaSampleBuilder,
        config: DerivedDataConfig,
        derivation_service_client: Option<Arc<dyn DerivationClient>>,
        restricted_paths: ArcRestrictedPathsConfigBased,
        commit_derived_data_mapping: Arc<CommitDerivedDataMapping>,
    ) -> Result<RepoDerivedData> {
        let managers = config
            .available_configs
            .iter()
            .map(|(config_name, config)| {
                (
                    config_name.to_string(),
                    DerivedDataManager::new(
                        repo_id,
                        repo_name.clone(),
                        commit_graph.clone(),
                        bonsai_hg_mapping.clone(),
                        bonsai_git_mapping.clone(),
                        filenodes.clone(),
                        repo_blobstore.clone(),
                        repo_config.clone(),
                        filestore_config,
                        scuba.clone(),
                        config_name.to_string(),
                        config.clone(),
                        derivation_service_client.clone(),
                        restricted_paths.clone(),
                        commit_derived_data_mapping.clone(),
                    ),
                )
            })
            .collect::<HashMap<_, _>>();
        let enabled_manager = managers
            .get(&config.enabled_config_name)
            .ok_or_else(|| {
                anyhow!(
                    "enabled_config_name: {} is not in available_configs",
                    config.enabled_config_name
                )
            })?
            .clone();
        Ok(RepoDerivedData {
            config,
            managers,
            enabled_manager,
        })
    }

    pub fn with_mutated_scuba(
        &self,
        mutator: impl FnOnce(MononokeScubaSampleBuilder) -> MononokeScubaSampleBuilder + Clone,
    ) -> Self {
        let updated_managers = self
            .managers
            .iter()
            .map(|(name, manager)| (name.clone(), manager.with_mutated_scuba(mutator.clone())))
            .collect::<HashMap<_, _>>();
        Self {
            config: self.config.clone(),
            managers: updated_managers,
            enabled_manager: self.enabled_manager.with_mutated_scuba(mutator),
        }
    }

    // For dangerous-override: allow replacement of blobstore
    pub fn with_replaced_blobstore(&self, repo_blobstore: RepoBlobstore) -> Self {
        let updated_managers = self
            .managers
            .iter()
            .map(|(name, manager)| {
                (
                    name.clone(),
                    manager.with_replaced_blobstore(repo_blobstore.clone()),
                )
            })
            .collect::<HashMap<_, _>>();
        Self {
            config: self.config.clone(),
            managers: updated_managers,
            enabled_manager: self.enabled_manager.with_replaced_blobstore(repo_blobstore),
        }
    }

    // For dangerous-override: allow replacement of commit_graph
    pub fn with_replaced_commit_graph(&self, commit_graph: Arc<CommitGraph>) -> Self {
        let updated_managers = self
            .managers
            .iter()
            .map(|(name, manager)| {
                (
                    name.clone(),
                    manager.with_replaced_commit_graph(commit_graph.clone()),
                )
            })
            .collect::<HashMap<_, _>>();
        Self {
            config: self.config.clone(),
            managers: updated_managers,
            enabled_manager: self
                .enabled_manager
                .with_replaced_commit_graph(commit_graph),
        }
    }

    // For dangerous-override: allow replacement of bonsai-hg-mapping
    pub fn with_replaced_bonsai_hg_mapping(
        &self,
        bonsai_hg_mapping: Arc<dyn BonsaiHgMapping>,
    ) -> Self {
        let updated_managers = self
            .managers
            .iter()
            .map(|(name, manager)| {
                (
                    name.clone(),
                    manager.with_replaced_bonsai_hg_mapping(bonsai_hg_mapping.clone()),
                )
            })
            .collect::<HashMap<_, _>>();
        Self {
            config: self.config.clone(),
            managers: updated_managers,
            enabled_manager: self
                .enabled_manager
                .with_replaced_bonsai_hg_mapping(bonsai_hg_mapping),
        }
    }

    // For dangerous-override: allow replacement of bonsai-git-mapping
    pub fn with_replaced_bonsai_git_mapping(
        &self,
        bonsai_git_mapping: Arc<dyn BonsaiGitMapping>,
    ) -> Self {
        let updated_managers = self
            .managers
            .iter()
            .map(|(name, manager)| {
                (
                    name.clone(),
                    manager.with_replaced_bonsai_git_mapping(bonsai_git_mapping.clone()),
                )
            })
            .collect::<HashMap<_, _>>();
        Self {
            config: self.config.clone(),
            managers: updated_managers,
            enabled_manager: self
                .enabled_manager
                .with_replaced_bonsai_git_mapping(bonsai_git_mapping),
        }
    }

    // For dangerous-override: allow replacement of filenodes
    pub fn with_replaced_filenodes(&self, filenodes: Arc<dyn Filenodes>) -> Self {
        let updated_managers = self
            .managers
            .iter()
            .map(|(name, manager)| {
                (
                    name.clone(),
                    manager.with_replaced_filenodes(filenodes.clone()),
                )
            })
            .collect::<HashMap<_, _>>();
        Self {
            config: self.config.clone(),
            managers: updated_managers,
            enabled_manager: self.enabled_manager.with_replaced_filenodes(filenodes),
        }
    }

    pub fn with_replaced_derivation_service_client(
        &self,
        derivation_service_client: Option<Arc<dyn DerivationClient>>,
    ) -> Self {
        let updated_managers = self
            .managers
            .iter()
            .map(|(name, manager)| {
                (
                    name.clone(),
                    manager
                        .with_replaced_derivation_service_client(derivation_service_client.clone()),
                )
            })
            .collect::<HashMap<_, _>>();
        Self {
            config: self.config.clone(),
            managers: updated_managers,
            enabled_manager: self
                .enabled_manager
                .with_replaced_derivation_service_client(derivation_service_client),
        }
    }

    pub fn for_bubble(&self, bubble: Bubble) -> Self {
        let updated_managers = self
            .managers
            .iter()
            .map(|(name, manager)| (name.clone(), manager.clone().for_bubble(bubble.clone())))
            .collect::<HashMap<_, _>>();
        Self {
            config: self.config.clone(),
            managers: updated_managers,
            enabled_manager: self.enabled_manager.clone().for_bubble(bubble),
        }
    }

    /// Current derived data configuration for this repo.
    pub fn config(&self) -> &DerivedDataConfig {
        &self.config
    }

    /// Config for the currently active derived data.
    pub fn active_config(&self) -> &DerivedDataTypesConfig {
        self.manager().config()
    }

    /// Default manager for derivation.
    pub fn manager(&self) -> &DerivedDataManager {
        &self.enabled_manager
    }

    /// Name of the repo this derived data belongs to.
    pub fn repo_name(&self) -> &str {
        self.enabled_manager.repo_name()
    }

    /// Returns the manager for the given config name.
    pub fn manager_for_config(&self, config_name: &str) -> Result<&DerivedDataManager> {
        self.managers
            .get(config_name)
            .ok_or_else(|| anyhow!("No manager found for config {config_name}"))
    }

    /// Count the number of ancestors of a commit that are underived.
    pub async fn count_underived<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
    ) -> Result<u64, DerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        self.manager()
            .count_underived::<Derivable>(ctx, csid, None)
            .await
    }

    /// Derive a derived data type using the default manager.
    pub async fn derive<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        priority: DerivationPriority,
    ) -> Result<Derivable, SharedDerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        self.manager()
            .derive::<Derivable>(ctx, csid, None, priority)
            .await
    }

    /// Fetch an already derived derived data type using the default manager.
    pub async fn fetch_derived<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
    ) -> Result<Option<Derivable>, DerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        self.manager()
            .fetch_derived::<Derivable>(ctx, csid, None)
            .await
    }

    pub async fn fetch_derived_direct<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
    ) -> Result<Option<Derivable::Value>, DerivationError>
    where
        Derivable: BonsaiDerivable,
    {
        self.manager()
            .fetch_derived_direct::<Derivable>(ctx, csid, None)
            .await
    }
}
