/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Result;
use bonsai_git_mapping::BonsaiGitMapping;
use bonsai_hg_mapping::BonsaiHgMapping;
use commit_derived_data_mapping::CommitDerivedDataMapping;
use commit_graph::CommitGraph;
use context::CoreContext;
use derived_data_remote::DerivationClient;
use ephemeral_blobstore::BubbleId;
use filenodes::Filenodes;
use filestore::FilestoreConfig;
use metaconfig_types::DerivationPipelineConfig;
use metaconfig_types::DerivedDataTypesConfig;
use metaconfig_types::RepoConfig;
use mononoke_types::ChangesetId;
use mononoke_types::RepositoryId;
use repo_blobstore::RepoBlobstore;
use restricted_paths_common::config_based::ArcRestrictedPathsConfigBased;
use scuba_ext::MononokeScubaSampleBuilder;

use crate::DerivationContext;
use crate::StageInputs;

pub mod bubble;
pub mod derive;
pub mod logging;
pub mod util;

/// Manager for derived data.
///
/// The manager is responsible for ordering derivation of data based
/// on the dependencies between derived data types and the changeset
/// graph.
#[derive(Clone)]
pub struct DerivedDataManager {
    inner: Arc<DerivedDataManagerInner>,
}

#[derive(Clone)]
pub struct DerivedDataManagerInner {
    repo_id: RepositoryId,
    repo_name: String,
    bubble_id: Option<BubbleId>,
    commit_graph: Arc<CommitGraph>,
    repo_blobstore: RepoBlobstore,
    repo_config: Arc<RepoConfig>,
    scuba: MononokeScubaSampleBuilder,
    /// If a (primary) manager has a secondary manager, that means some of the
    /// changesets should be derived using the primary manager, and some the secondary,
    /// in that order. For example, bubble managers are secondary, as all the data in
    /// the persistent blobstore must be derived BEFORE deriving data in the bubble.
    secondary: Option<SecondaryManagerData>,
    /// If this client is set, then derivation will be done remotely on derived data service
    derivation_service_client: Option<Arc<dyn DerivationClient>>,
    derivation_context: DerivationContext,
}

pub struct DerivationAssignment {
    /// Changesets that should be derived by the primary manager
    pub primary: Vec<ChangesetId>,
    /// Changesets that should be derived by the secondary manager, after the first
    /// part of derivation is done.
    pub secondary: Vec<ChangesetId>,
}

#[async_trait::async_trait]
pub trait DerivationAssigner: Send + Sync {
    /// How to split derivation between primary and secondary managers. If not possible
    /// to split, this function should error.
    async fn assign(&self, ctx: &CoreContext, cs: Vec<ChangesetId>)
    -> Result<DerivationAssignment>;
}

#[derive(Clone)]
pub(crate) struct SecondaryManagerData {
    manager: DerivedDataManager,
    assigner: Arc<dyn DerivationAssigner>,
}

impl DerivedDataManager {
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
        config_name: String,
        config: DerivedDataTypesConfig,
        derivation_service_client: Option<Arc<dyn DerivationClient>>,
        restricted_paths: ArcRestrictedPathsConfigBased,
        commit_derived_data_mapping: Arc<CommitDerivedDataMapping>,
    ) -> Self {
        let derivation_context = DerivationContext::new(
            bonsai_hg_mapping,
            bonsai_git_mapping,
            filenodes,
            repo_id,
            repo_name.clone(),
            config_name,
            config,
            repo_blobstore.boxed(),
            filestore_config,
            restricted_paths,
            repo_config.derived_data_config.pipeline_config.clone(),
            commit_derived_data_mapping,
        );
        DerivedDataManager {
            inner: Arc::new(DerivedDataManagerInner {
                repo_id,
                repo_name,
                bubble_id: None,
                commit_graph,
                repo_blobstore: repo_blobstore.clone(),
                repo_config,
                scuba,
                secondary: None,
                derivation_service_client,
                derivation_context,
            }),
        }
    }

    /// Supply immutable outputs from parent and dependency stages.
    pub fn with_stage_inputs(&self, inputs: Arc<StageInputs>) -> Self {
        let mut inner = self.inner.as_ref().clone();
        inner.derivation_context.stage_inputs = inputs;
        Self {
            inner: Arc::new(inner),
        }
    }

    pub fn with_mutated_scuba(
        &self,
        mutator: impl FnOnce(MononokeScubaSampleBuilder) -> MononokeScubaSampleBuilder + Clone,
    ) -> Self {
        Self {
            inner: Arc::new(DerivedDataManagerInner {
                scuba: mutator(self.inner.scuba.clone()),
                ..self.inner.as_ref().clone()
            }),
        }
    }

    // For dangerous-override: allow replacement of blobstore
    pub fn with_replaced_blobstore(&self, repo_blobstore: RepoBlobstore) -> Self {
        Self {
            inner: Arc::new(DerivedDataManagerInner {
                repo_blobstore: repo_blobstore.clone(),
                derivation_context: self
                    .inner
                    .derivation_context
                    .with_replaced_blobstore(repo_blobstore.boxed()),
                ..self.inner.as_ref().clone()
            }),
        }
    }

    // For dangerous-override: allow replacement of restricted paths
    pub fn with_replaced_restricted_paths(
        &self,
        restricted_paths: ArcRestrictedPathsConfigBased,
    ) -> Self {
        Self {
            inner: Arc::new(DerivedDataManagerInner {
                derivation_context: self
                    .inner
                    .derivation_context
                    .with_replaced_restricted_paths(restricted_paths),
                ..self.inner.as_ref().clone()
            }),
        }
    }

    // For dangerous-override: allow replacement of commit graph
    pub fn with_replaced_commit_graph(&self, commit_graph: Arc<CommitGraph>) -> Self {
        Self {
            inner: Arc::new(DerivedDataManagerInner {
                commit_graph,
                ..self.inner.as_ref().clone()
            }),
        }
    }

    // For dangerous-override: allow replacement of bonsai-hg-mapping
    pub fn with_replaced_bonsai_hg_mapping(
        &self,
        bonsai_hg_mapping: Arc<dyn BonsaiHgMapping>,
    ) -> Self {
        Self {
            inner: Arc::new(DerivedDataManagerInner {
                derivation_context: self
                    .inner
                    .derivation_context
                    .with_replaced_bonsai_hg_mapping(bonsai_hg_mapping),
                ..self.inner.as_ref().clone()
            }),
        }
    }

    // For dangerous-override: allow replacement of bonsai-git-mapping
    pub fn with_replaced_bonsai_git_mapping(
        &self,
        bonsai_git_mapping: Arc<dyn BonsaiGitMapping>,
    ) -> Self {
        Self {
            inner: Arc::new(DerivedDataManagerInner {
                derivation_context: self
                    .inner
                    .derivation_context
                    .with_replaced_bonsai_git_mapping(bonsai_git_mapping),
                ..self.inner.as_ref().clone()
            }),
        }
    }

    // For dangerous-override: allow replacement of filenodes
    pub fn with_replaced_filenodes(&self, filenodes: Arc<dyn Filenodes>) -> Self {
        Self {
            inner: Arc::new(DerivedDataManagerInner {
                derivation_context: self
                    .inner
                    .derivation_context
                    .with_replaced_filenodes(filenodes),
                ..self.inner.as_ref().clone()
            }),
        }
    }

    pub fn with_replaced_config(
        &self,
        config_name: String,
        config: DerivedDataTypesConfig,
    ) -> Self {
        Self {
            inner: Arc::new(DerivedDataManagerInner {
                derivation_context: self
                    .inner
                    .derivation_context
                    .with_replaced_config(config_name, config),
                ..self.inner.as_ref().clone()
            }),
        }
    }

    // Replace the repo-level derivation pipeline config (used in tests to
    // simulate a pipeline-enabled repo).
    pub fn with_replaced_pipeline_config(
        &self,
        pipeline_config: Option<DerivationPipelineConfig>,
    ) -> Self {
        Self {
            inner: Arc::new(DerivedDataManagerInner {
                derivation_context: self
                    .inner
                    .derivation_context
                    .with_replaced_pipeline_config(pipeline_config),
                ..self.inner.as_ref().clone()
            }),
        }
    }

    pub fn with_replaced_derivation_service_client(
        &self,
        derivation_service_client: Option<Arc<dyn DerivationClient>>,
    ) -> Self {
        Self {
            inner: Arc::new(DerivedDataManagerInner {
                derivation_service_client,
                ..self.inner.as_ref().clone()
            }),
        }
    }

    pub fn repo_id(&self) -> RepositoryId {
        self.inner.repo_id
    }

    pub fn repo_name(&self) -> &str {
        self.inner.repo_name.as_str()
    }

    pub fn bubble_id(&self) -> Option<BubbleId> {
        self.inner.bubble_id
    }

    pub fn commit_graph(&self) -> &CommitGraph {
        self.inner.commit_graph.as_ref()
    }

    pub fn commit_graph_arc(&self) -> Arc<CommitGraph> {
        self.inner.commit_graph.clone()
    }

    pub fn repo_blobstore(&self) -> &RepoBlobstore {
        &self.inner.repo_blobstore
    }

    pub fn scuba(&self) -> &MononokeScubaSampleBuilder {
        &self.inner.scuba
    }

    pub fn repo_config(&self) -> &RepoConfig {
        self.inner.repo_config.as_ref()
    }

    pub fn config(&self) -> &DerivedDataTypesConfig {
        self.inner.derivation_context.config()
    }

    /// Repo-level derivation pipeline configuration, if the repo has one.
    pub fn pipeline_config(&self) -> Option<&DerivationPipelineConfig> {
        self.inner.derivation_context.pipeline_config()
    }

    pub fn config_name(&self) -> String {
        self.inner.derivation_context.config_name()
    }

    pub fn bonsai_hg_mapping(&self) -> Result<&dyn BonsaiHgMapping> {
        self.inner.derivation_context.bonsai_hg_mapping()
    }

    pub fn bonsai_git_mapping(&self) -> Result<&dyn BonsaiGitMapping> {
        self.inner.derivation_context.bonsai_git_mapping()
    }

    pub fn filenodes(&self) -> Result<&dyn Filenodes> {
        self.inner.derivation_context.filenodes()
    }

    pub fn derivation_service_client(&self) -> Option<&dyn DerivationClient> {
        self.inner.derivation_service_client.as_deref()
    }
}
