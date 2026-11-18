/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use blobstore::KeyedBlobstore;
use bonsai_git_mapping::BonsaiGitMapping;
use bonsai_hg_mapping::BonsaiHgMapping;
use cacheblob::MemWritesKeyedBlobstore;
use commit_derived_data_mapping::CommitDerivedDataMapping;
use context::CoreContext;
use filenodes::Filenodes;
use filestore::FilestoreConfig;
use futures::future::try_join_all;
use metaconfig_types::DerivationPipelineConfig;
use metaconfig_types::DerivedDataTypesConfig;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use mononoke_types::RepositoryId;
use restricted_paths_common::config_based::ArcRestrictedPathsConfigBased;

use crate::StageInputs;
use crate::derivable::BonsaiDerivable;
use crate::manager::derive::Rederivation;

/// Context for performing derivation.
///
/// This struct is passed to derivation implementations.  They can use it
/// to access repository attributes or request access to dependent
/// derived data types.
#[derive(Clone)]
pub struct DerivationContext {
    pub(crate) bonsai_hg_mapping: Option<Arc<dyn BonsaiHgMapping>>,
    pub(crate) bonsai_git_mapping: Option<Arc<dyn BonsaiGitMapping>>,
    pub(crate) filenodes: Option<Arc<dyn Filenodes>>,
    pub(crate) stage_inputs: Arc<StageInputs>,
    config_name: String,
    config: DerivedDataTypesConfig,
    pub(crate) rederivation: Option<Arc<dyn Rederivation>>,
    repo_id: RepositoryId,
    repo_name: String,
    pub(crate) blobstore: Arc<dyn KeyedBlobstore>,
    filestore_config: FilestoreConfig,

    /// Write cache layered over the blobstore.  This is the same object
    /// with two views, so we can return a reference to the `Arc<dyn
    /// Blobstore>` version if needed.
    blobstore_write_cache: Option<(
        Arc<dyn KeyedBlobstore>,
        Arc<MemWritesKeyedBlobstore<Arc<dyn KeyedBlobstore>>>,
    )>,
    restricted_paths: ArcRestrictedPathsConfigBased,
    pipeline_config: Option<DerivationPipelineConfig>,
    commit_derived_data_mapping: Option<Arc<CommitDerivedDataMapping>>,
}

impl DerivationContext {
    pub(crate) fn new(
        bonsai_hg_mapping: Arc<dyn BonsaiHgMapping>,
        bonsai_git_mapping: Arc<dyn BonsaiGitMapping>,
        filenodes: Arc<dyn Filenodes>,
        repo_id: RepositoryId,
        repo_name: String,
        config_name: String,
        config: DerivedDataTypesConfig,
        blobstore: Arc<dyn KeyedBlobstore>,
        filestore_config: FilestoreConfig,
        restricted_paths: ArcRestrictedPathsConfigBased,
        pipeline_config: Option<DerivationPipelineConfig>,
        commit_derived_data_mapping: Arc<CommitDerivedDataMapping>,
    ) -> Self {
        // Start with None. Use with_rederivation later if needed
        let rederivation = None;
        DerivationContext {
            bonsai_hg_mapping: Some(bonsai_hg_mapping),
            bonsai_git_mapping: Some(bonsai_git_mapping),
            filenodes: Some(filenodes),
            repo_id,
            repo_name,
            stage_inputs: Arc::default(),
            config_name,
            config,
            rederivation,
            blobstore,
            filestore_config,
            blobstore_write_cache: None,
            restricted_paths,
            pipeline_config,
            commit_derived_data_mapping: Some(commit_derived_data_mapping),
        }
    }

    pub(crate) fn with_replaced_rederivation(
        &self,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Self {
        Self {
            rederivation,
            ..self.clone()
        }
    }

    pub(crate) fn with_replaced_pipeline_config(
        &self,
        pipeline_config: Option<DerivationPipelineConfig>,
    ) -> Self {
        Self {
            pipeline_config,
            ..self.clone()
        }
    }

    // For dangerous-override: allow replacement of bonsai-hg-mapping
    pub(crate) fn with_replaced_bonsai_hg_mapping(
        &self,
        bonsai_hg_mapping: Arc<dyn BonsaiHgMapping>,
    ) -> Self {
        Self {
            bonsai_hg_mapping: Some(bonsai_hg_mapping),
            ..self.clone()
        }
    }

    // For dangerous-override: allow replacement of bonsai-git-mapping
    pub(crate) fn with_replaced_bonsai_git_mapping(
        &self,
        bonsai_git_mapping: Arc<dyn BonsaiGitMapping>,
    ) -> Self {
        Self {
            bonsai_git_mapping: Some(bonsai_git_mapping),
            ..self.clone()
        }
    }

    // For dangerous-override: allow replacement of filenodes
    pub(crate) fn with_replaced_filenodes(&self, filenodes: Arc<dyn Filenodes>) -> Self {
        Self {
            filenodes: Some(filenodes),
            ..self.clone()
        }
    }

    pub(crate) fn with_replaced_config(
        &self,
        config_name: String,
        config: DerivedDataTypesConfig,
    ) -> Self {
        Self {
            config_name,
            config,
            ..self.clone()
        }
    }

    pub(crate) fn with_replaced_blobstore(&self, blobstore: Arc<dyn KeyedBlobstore>) -> Self {
        Self {
            blobstore,
            ..self.clone()
        }
    }

    pub(crate) fn with_replaced_restricted_paths(
        &self,
        restricted_paths: ArcRestrictedPathsConfigBased,
    ) -> Self {
        Self {
            restricted_paths,
            ..self.clone()
        }
    }

    #[allow(dead_code)]
    pub(crate) fn with_replaced_commit_derived_data_mapping(
        &self,
        commit_derived_data_mapping: Arc<CommitDerivedDataMapping>,
    ) -> Self {
        Self {
            commit_derived_data_mapping: Some(commit_derived_data_mapping),
            ..self.clone()
        }
    }

    /// Fetch previously derived data.
    pub async fn fetch_derived<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
    ) -> Result<Option<Derivable>>
    where
        Derivable: BonsaiDerivable,
    {
        if let Some(rederivation) = self.rederivation.as_ref() {
            if rederivation.needs_rederive(Derivable::VARIANT, csid) {
                return Ok(None);
            }
        }
        let derived = Derivable::fetch(ctx, self, csid).await?;
        Ok(derived)
    }

    /// Directly fetch the value of previously derived data without going through
    /// the mapping.
    pub async fn fetch_derived_direct<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
    ) -> Result<Option<Derivable::Value>>
    where
        Derivable: BonsaiDerivable,
    {
        if let Some(rederivation) = self.rederivation.as_ref() {
            if rederivation.needs_rederive(Derivable::VARIANT, csid) {
                return Ok(None);
            }
        }
        let derived = Derivable::fetch_direct(ctx, self, csid).await?;
        Ok(derived)
    }

    /// Fetch a batch of previously derived data.
    pub async fn fetch_derived_batch<Derivable>(
        &self,
        ctx: &CoreContext,
        mut csids: Vec<ChangesetId>,
    ) -> Result<HashMap<ChangesetId, Derivable>>
    where
        Derivable: BonsaiDerivable,
    {
        if let Some(rederivation) = self.rederivation.as_ref() {
            csids.retain(|csid| !rederivation.needs_rederive(Derivable::VARIANT, *csid));
        }
        let derived = Derivable::fetch_batch(ctx, self, &csids).await?;
        Ok(derived)
    }

    /// Fetch previously derived data that is known to be derived as it is a
    /// dependency of the current changeset.
    pub async fn fetch_dependency<Derivable>(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
    ) -> Result<Derivable>
    where
        Derivable: BonsaiDerivable,
    {
        self.fetch_derived(ctx, csid).await?.ok_or_else(|| {
            anyhow!(
                "dependency '{}' of {} was not already derived",
                Derivable::NAME,
                csid
            )
        })
    }

    /// Fetch a dependency for all parents of a changeset.
    pub async fn fetch_parents<Derivable>(
        &self,
        ctx: &CoreContext,
        bonsai: &BonsaiChangeset,
    ) -> Result<Vec<Derivable>>
    where
        Derivable: BonsaiDerivable,
    {
        self.fetch_unknown_parents(ctx, None, bonsai).await
    }

    /// Fetch a dependency for all parents of a changeset if the dependency is
    /// not already known.
    pub async fn fetch_unknown_parents<Derivable>(
        &self,
        ctx: &CoreContext,
        known: Option<&HashMap<ChangesetId, Derivable>>,
        bonsai: &BonsaiChangeset,
    ) -> Result<Vec<Derivable>>
    where
        Derivable: BonsaiDerivable,
    {
        try_join_all(bonsai.parents().map(|p| async move {
            self.fetch_unknown_dependency(ctx, known, p)
                .await
                .with_context(|| {
                    format!(
                        "could not fetch '{}' for parents of {}",
                        Derivable::NAME,
                        bonsai.get_changeset_id(),
                    )
                })
        }))
        .await
    }

    /// Fetch derived data value for changeset if it is not already known.
    pub async fn fetch_unknown_dependency<Derivable>(
        &self,
        ctx: &CoreContext,
        known: Option<&HashMap<ChangesetId, Derivable>>,
        csid: ChangesetId,
    ) -> Result<Derivable>
    where
        Derivable: BonsaiDerivable,
    {
        if let Some(parent) = known.and_then(|k| k.get(&csid)) {
            return Ok(parent.clone());
        }
        self.fetch_dependency(ctx, csid).await
    }

    /// Like `fetch_unknown_dependency`, but returns `None` instead of an error
    /// when the derived data has not been derived yet.
    pub async fn fetch_unknown<Derivable>(
        &self,
        ctx: &CoreContext,
        known: Option<&HashMap<ChangesetId, Derivable>>,
        csid: ChangesetId,
    ) -> Result<Option<Derivable>>
    where
        Derivable: BonsaiDerivable,
    {
        if let Some(value) = known.and_then(|k| k.get(&csid)) {
            return Ok(Some(value.clone()));
        }
        self.fetch_derived(ctx, csid).await
    }

    /// The blobstore that should be used for storing and retrieving blobs.
    pub fn stage_inputs(&self) -> &Arc<StageInputs> {
        &self.stage_inputs
    }

    pub fn blobstore(&self) -> &Arc<dyn KeyedBlobstore> {
        match &self.blobstore_write_cache {
            Some((blobstore, _)) => blobstore,
            None => &self.blobstore,
        }
    }

    pub fn filestore_config(&self) -> FilestoreConfig {
        self.filestore_config
    }

    pub fn bonsai_hg_mapping(&self) -> Result<&dyn BonsaiHgMapping> {
        self.bonsai_hg_mapping
            .as_deref()
            .context("Missing BonsaiHgMapping")
    }

    pub fn bonsai_git_mapping(&self) -> Result<&dyn BonsaiGitMapping> {
        self.bonsai_git_mapping
            .as_deref()
            .context("Missing BonsaiGitMapping")
    }

    pub fn filenodes(&self) -> Result<&dyn Filenodes> {
        self.filenodes.as_deref().context("Missing filenodes")
    }

    pub fn config_name(&self) -> String {
        self.config_name.clone()
    }

    pub fn repo_id(&self) -> RepositoryId {
        self.repo_id
    }

    pub fn repo_name(&self) -> &str {
        &self.repo_name
    }

    pub fn restricted_paths(&self) -> ArcRestrictedPathsConfigBased {
        self.restricted_paths.clone()
    }

    pub fn commit_derived_data_mapping(&self) -> Result<&CommitDerivedDataMapping> {
        self.commit_derived_data_mapping
            .as_deref()
            .context("Missing CommitDerivedDataMapping")
    }

    /// The XDB shard ID for a given derived data type.
    pub fn xdb_shard_id(&self, variant: DerivableType) -> Result<usize> {
        self.config
            .xdb_mapping_shard_ids
            .get(&variant)
            .copied()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "xdb_mapping_shard_ids not configured for {}",
                    variant.name()
                )
            })
    }

    /// The config that should be used for derivation.
    pub fn config(&self) -> &DerivedDataTypesConfig {
        &self.config
    }

    /// Repo-level pipeline configuration.
    pub fn pipeline_config(&self) -> Option<&DerivationPipelineConfig> {
        self.pipeline_config.as_ref()
    }

    /// Mapping key prefix for a particular derived data type.
    pub fn mapping_key_prefix<Derivable>(&self) -> &str
    where
        Derivable: BonsaiDerivable,
    {
        self.config()
            .mapping_key_prefixes
            .get(&Derivable::VARIANT)
            .map_or("", String::as_str)
    }

    /// Batch size for a particular derived data type.
    pub fn batch_size<Derivable>(&self) -> u64
    where
        Derivable: BonsaiDerivable,
    {
        const DEFAULT_BATCH_SIZE: u64 = 20;
        self.config()
            .derivation_batch_sizes
            .get(&Derivable::VARIANT)
            .map_or(DEFAULT_BATCH_SIZE, |size| *size as u64)
    }

    /// Enable write batching for this derivation context.
    ///
    /// With write batching enabled, blobstore writes are sent to a write
    /// cache, rather than directly to the blobstore.  They must be flushed by
    /// a call to `flush` to make them persistent.
    pub(crate) fn enable_write_batching(&mut self) {
        if self.blobstore_write_cache.is_none() {
            let blobstore = Arc::new(MemWritesKeyedBlobstore::new(self.blobstore.clone()));

            self.blobstore_write_cache = Some((blobstore.clone(), blobstore));
        }
    }

    /// Flush any pending writes for this derivation context.
    pub async fn flush(&self, ctx: &CoreContext) -> Result<()> {
        if let Some((_, blobstore)) = &self.blobstore_write_cache {
            blobstore.persist(ctx).await?;
        }
        Ok(())
    }

    /// Remove all entries from the write cache whose blobstore keys start
    /// with the given prefix. This prevents them from being flushed to
    /// persistent storage on the next `flush()`. The entries remain
    /// readable until the next `flush()`. Returns the number of entries
    /// removed, or 0 if write batching is not enabled.
    pub fn remove_write_cache_by_prefix(&self, prefix: &str) -> usize {
        match &self.blobstore_write_cache {
            Some((_, blobstore)) => blobstore.remove_by_prefix(prefix),
            None => 0,
        }
    }
}
