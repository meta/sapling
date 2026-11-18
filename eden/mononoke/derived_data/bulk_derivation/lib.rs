/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

use acl_manifest::RootAclManifestId;
use anyhow::Error;
use async_trait::async_trait;
use basename_suffix_skeleton_manifest_v3::RootBssmV3DirectoryId;
use blame::RootBlameV2;
use blame::RootBlameV3;
use case_conflict_skeleton_manifest::RootCaseConflictSkeletonManifestId;
use changeset_info::ChangesetInfo;
use cloned::cloned;
use content_manifest_derivation::RootContentManifestId;
use context::CoreContext;
use deleted_manifest::RootDeletedManifestV2Id;
use derivation_queue_thrift::DerivationPriority;
use derived_data_manager::BonsaiDerivable;
use derived_data_manager::DerivableType;
use derived_data_manager::DerivableUntopologically;
use derived_data_manager::DerivationError;
use derived_data_manager::DerivedDataManager;
use derived_data_manager::PipelineDerivable;
use derived_data_manager::Rederivation;
use derived_data_manager::SharedDerivationError;
use derived_data_manager::StageBatchResult;
use derived_data_manager::StageId;
use derived_data_manager::VisitedDerivableTypesMap;
use derived_data_manager::VisitedDerivableTypesMapStatic;
use derived_data_manager::derivable::DerivationDependencies;
use directory_branch_cluster_manifest::RootDirectoryBranchClusterManifestId;
use fastlog::RootFastlog;
use fastlog::RootFastlogV2;
use filenodes_derivation::FilenodesOnlyPublic;
use fsnodes::RootFsnodeId;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use git_types::MappedGitCommitId;
use git_types::RootGitDeltaManifestV2Id;
use git_types::RootGitDeltaManifestV3Id;
use history_manifest::RootHistoryManifestDirectoryId;
use inferred_copy_from::RootInferredCopyFromId;
use itertools::Itertools;
use mercurial_derivation::MappedHgChangesetId;
use mercurial_derivation::RootHgAugmentedManifestId;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableUntopologicallyVariant;
use mononoke_types::PipelineDerivableVariant;
use skeleton_manifest::RootSkeletonManifestId;
use skeleton_manifest_v2::RootSkeletonManifestV2Id;
use test_manifest::RootTestManifestDirectory;
use test_sharded_manifest::RootTestShardedManifestDirectory;
use unodes::RootUnodeManifestId;

#[async_trait]
pub trait BulkDerivation {
    /// Derive all the desired derived data types for all the desired csids
    ///
    /// If the dependent types or changesets are not derived yet, they will be derived now
    ///
    /// `override_concurrency` controls how many derived data types are derived in parallel (defaults to 10)
    async fn derive_bulk_locally(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
        derived_data_types: &[DerivableType],
        override_batch_size: Option<u64>,
        override_concurrency: Option<usize>,
    ) -> Result<(), SharedDerivationError>;

    /// Derive all the desired derived data types for all the desired csids
    ///
    /// If the dependent types or changesets are not derived yet, they will be derived now
    ///
    /// Perform the derivation remotely using the Derived Data Service, or fall back to local
    /// derivation if necessary
    async fn derive_bulk(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
        derived_data_types: &[DerivableType],
        override_concurrency: Option<usize>,
    ) -> Result<(), SharedDerivationError>;

    /// Derive data for exactly a batch of changesets.
    ///
    /// The provided batch of changesets must be in topological
    /// order. The caller must have arranged for the dependencies
    /// and ancestors of the batch to have already been derived.  If
    /// any dependency or ancestor is not already derived, an error
    /// will be returned.
    async fn derive_exactly_batch(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
        derived_data_type: DerivableType,
    ) -> Result<(), DerivationError>;

    /// Derive data for exactly all underived changesets in a batch.
    ///
    /// The provided batch of changesets must be in topological
    /// order. The caller must have arranged for the dependencies
    /// and ancestors of the batch to have already been derived. If
    /// any dependency or ancestor is not already derived, an error
    /// will be returned.
    async fn derive_exactly_underived_batch(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
        derived_data_type: DerivableType,
    ) -> Result<(), DerivationError>;

    /// Check if the given derived data type is derived for the given changeset id.
    async fn is_derived(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
        derived_data_type: DerivableType,
    ) -> Result<bool, DerivationError>;

    /// Check if the given derived data type's specific stage is derived for the given changeset id.
    async fn is_stage_derived(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        derived_data_type: DerivableType,
        stage: &StageId,
    ) -> Result<bool, DerivationError>;

    /// Verify that a stage output is consistent with the canonical derived
    /// value.
    async fn verify_stage_output(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        derived_data_type: DerivableType,
        stage: &StageId,
    ) -> Result<bool, DerivationError>;

    /// Returns a `Vec` that contains all changeset ids that don't have the given
    /// derived data type derived from the given changeset ids.
    async fn pending(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
        derived_data_type: DerivableType,
    ) -> Result<Vec<ChangesetId>, DerivationError>;

    /// Fetch derived data for a batch of changesets if they have previously
    /// been derived.
    ///
    /// Returns a hashmap from changeset id to the debug format of the derived data.
    /// Changesets for which the data has not previously been derived are omitted.
    async fn fetch_derived_batch(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
        derived_data_type: DerivableType,
    ) -> Result<HashMap<ChangesetId, String>, DerivationError>;

    /// Returns the number of ancestor of the given changeset that don't have
    /// the given derived data type derived.
    async fn count_underived(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
        derived_data_type: DerivableType,
    ) -> Result<u64, DerivationError>;

    /// Derive the given derived data type for the given changeset id, without
    /// depending on derived data for its parents.
    async fn unsafe_derive_untopologically(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
        derived_data_type: DerivableType,
    ) -> Result<(), DerivationError>;

    /// Returns the derivable types that the given type statically depends on,
    /// as declared via the `dependencies!` macro on its `BonsaiDerivable` impl.
    ///
    /// Only direct dependencies are returned (no transitive closure).
    fn dependency_types(&self, derived_data_type: DerivableType) -> Vec<DerivableType>;
}

struct SingleTypeManager<T: BonsaiDerivable> {
    manager: DerivedDataManager,
    derived_data_type: PhantomData<T>,
}

impl<T: BonsaiDerivable> SingleTypeManager<T> {
    fn new(manager: DerivedDataManager) -> Self {
        Self {
            manager,
            derived_data_type: PhantomData,
        }
    }
}

#[async_trait]
trait SingleTypeDerivation: Send + Sync {
    async fn derive_heads_with_visited(
        &self,
        ctx: CoreContext,
        csids: Vec<ChangesetId>,
        override_batch_size: Option<u64>,
        rederivation: Option<Arc<dyn Rederivation>>,
        visited: VisitedDerivableTypesMapStatic<u64, SharedDerivationError>,
    ) -> Result<(), SharedDerivationError>;

    async fn derive_exactly_batch(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<(), DerivationError>;

    async fn derive_exactly_underived_batch(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<(), DerivationError>;

    async fn is_derived(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<bool, DerivationError>;

    async fn pending(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<Vec<ChangesetId>, DerivationError>;

    async fn fetch_derived_batch(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<HashMap<ChangesetId, String>, DerivationError>;

    async fn count_underived(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<u64, DerivationError>;

    async fn derive(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<(), SharedDerivationError>;

    fn dependency_types(&self) -> Vec<DerivableType>;
}

#[async_trait]
impl<T: BonsaiDerivable> SingleTypeDerivation for SingleTypeManager<T> {
    async fn derive_heads_with_visited(
        &self,
        ctx: CoreContext,
        csids: Vec<ChangesetId>,
        override_batch_size: Option<u64>,
        rederivation: Option<Arc<dyn Rederivation>>,
        visited: VisitedDerivableTypesMapStatic<u64, SharedDerivationError>,
    ) -> Result<(), SharedDerivationError> {
        self.manager
            .clone()
            .derive_heads_with_visited::<T>(ctx, csids, override_batch_size, rederivation, visited)
            .await?;
        Ok(())
    }

    async fn derive_exactly_batch(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<(), DerivationError> {
        self.manager
            .derive_exactly_batch::<T>(ctx, csids.to_vec(), rederivation)
            .await?;
        Ok(())
    }

    async fn derive_exactly_underived_batch(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<(), DerivationError> {
        self.manager
            .derive_exactly_underived_batch::<T>(ctx, csids.to_vec(), rederivation)
            .await?;
        Ok(())
    }

    async fn is_derived(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<bool, DerivationError> {
        Ok(self
            .manager
            .fetch_derived::<T>(ctx, csid, rederivation)
            .await?
            .is_some())
    }

    async fn pending(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<Vec<ChangesetId>, DerivationError> {
        let derived = self
            .manager
            .fetch_derived_batch::<T>(ctx, csids.to_vec(), rederivation)
            .await?;
        Ok(csids
            .iter()
            .filter(|csid| !derived.contains_key(csid))
            .copied()
            .collect())
    }

    async fn fetch_derived_batch(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<HashMap<ChangesetId, String>, DerivationError> {
        let derived = self
            .manager
            .fetch_derived_batch::<T>(ctx, csids.to_vec(), rederivation)
            .await?;
        Ok(derived
            .into_iter()
            .map(|(csid, derived)| (csid, format!("{derived:?}")))
            .collect())
    }

    async fn count_underived(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<u64, DerivationError> {
        self.manager
            .count_underived::<T>(ctx, csid, rederivation)
            .await
    }

    async fn derive(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<(), SharedDerivationError> {
        self.manager
            .derive::<T>(ctx, csid, rederivation, DerivationPriority::LOW)
            .await?;
        Ok(())
    }

    fn dependency_types(&self) -> Vec<DerivableType> {
        <T::Dependencies as DerivationDependencies>::iter().collect()
    }
}

#[async_trait]
trait SingleTypeUntopologicalDerivation: Send + Sync {
    async fn unsafe_derive_untopologically(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<(), DerivationError>;
}

#[async_trait]
impl<T: DerivableUntopologically> SingleTypeUntopologicalDerivation for SingleTypeManager<T> {
    async fn unsafe_derive_untopologically(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
    ) -> Result<(), DerivationError> {
        self.manager
            .unsafe_derive_untopologically::<T>(ctx, csid, rederivation)
            .await?;
        Ok(())
    }
}

fn manager_for_type(
    manager: &DerivedDataManager,
    derived_data_type: DerivableType,
) -> Arc<dyn SingleTypeDerivation + Send + Sync + 'static> {
    let manager = manager.clone();
    match derived_data_type {
        DerivableType::Unodes => Arc::new(SingleTypeManager::<RootUnodeManifestId>::new(manager)),
        DerivableType::BlameV2 => Arc::new(SingleTypeManager::<RootBlameV2>::new(manager)),
        DerivableType::BlameV3 => Arc::new(SingleTypeManager::<RootBlameV3>::new(manager)),
        DerivableType::FileNodes => {
            Arc::new(SingleTypeManager::<FilenodesOnlyPublic>::new(manager))
        }
        DerivableType::HgChangesets => {
            Arc::new(SingleTypeManager::<MappedHgChangesetId>::new(manager))
        }
        DerivableType::HgAugmentedManifests => {
            Arc::new(SingleTypeManager::<RootHgAugmentedManifestId>::new(manager))
        }
        DerivableType::Fsnodes => Arc::new(SingleTypeManager::<RootFsnodeId>::new(manager)),
        DerivableType::Fastlog => Arc::new(SingleTypeManager::<RootFastlog>::new(manager)),
        DerivableType::FastlogV2 => Arc::new(SingleTypeManager::<RootFastlogV2>::new(manager)),
        DerivableType::DeletedManifests => {
            Arc::new(SingleTypeManager::<RootDeletedManifestV2Id>::new(manager))
        }
        DerivableType::SkeletonManifests => {
            Arc::new(SingleTypeManager::<RootSkeletonManifestId>::new(manager))
        }
        DerivableType::SkeletonManifestsV2 => {
            Arc::new(SingleTypeManager::<RootSkeletonManifestV2Id>::new(manager))
        }
        DerivableType::Ccsm => {
            Arc::new(SingleTypeManager::<RootCaseConflictSkeletonManifestId>::new(manager))
        }
        DerivableType::ContentManifests => {
            Arc::new(SingleTypeManager::<RootContentManifestId>::new(manager))
        }
        DerivableType::ChangesetInfo => Arc::new(SingleTypeManager::<ChangesetInfo>::new(manager)),
        DerivableType::GitCommits => Arc::new(SingleTypeManager::<MappedGitCommitId>::new(manager)),
        DerivableType::GitDeltaManifestsV2 => {
            Arc::new(SingleTypeManager::<RootGitDeltaManifestV2Id>::new(manager))
        }
        DerivableType::GitDeltaManifestsV3 => {
            Arc::new(SingleTypeManager::<RootGitDeltaManifestV3Id>::new(manager))
        }
        DerivableType::InferredCopyFrom => {
            Arc::new(SingleTypeManager::<RootInferredCopyFromId>::new(manager))
        }
        DerivableType::BssmV3 => Arc::new(SingleTypeManager::<RootBssmV3DirectoryId>::new(manager)),
        DerivableType::DirectoryBranchClusterManifest => {
            Arc::new(SingleTypeManager::<RootDirectoryBranchClusterManifestId>::new(manager))
        }
        DerivableType::TestManifests => {
            Arc::new(SingleTypeManager::<RootTestManifestDirectory>::new(manager))
        }
        DerivableType::TestShardedManifests => Arc::new(SingleTypeManager::<
            RootTestShardedManifestDirectory,
        >::new(manager)),
        DerivableType::AclManifests => {
            Arc::new(SingleTypeManager::<RootAclManifestId>::new(manager))
        }
        DerivableType::HistoryManifests => Arc::new(SingleTypeManager::<
            RootHistoryManifestDirectoryId,
        >::new(manager)),
    }
}

fn manager_for_derivable_untopologically_variant(
    manager: &DerivedDataManager,
    variant: DerivableUntopologicallyVariant,
) -> Arc<dyn SingleTypeUntopologicalDerivation + Send + Sync + 'static> {
    let manager = manager.clone();
    match variant {
        DerivableUntopologicallyVariant::BlameV3 => {
            Arc::new(SingleTypeManager::<RootBlameV3>::new(manager))
        }
        DerivableUntopologicallyVariant::BssmV3 => {
            Arc::new(SingleTypeManager::<RootBssmV3DirectoryId>::new(manager))
        }
        DerivableUntopologicallyVariant::HgAugmentedManifests => {
            Arc::new(SingleTypeManager::<RootHgAugmentedManifestId>::new(manager))
        }
        DerivableUntopologicallyVariant::SkeletonManifestsV2 => {
            Arc::new(SingleTypeManager::<RootSkeletonManifestV2Id>::new(manager))
        }
        DerivableUntopologicallyVariant::Ccsm => {
            Arc::new(SingleTypeManager::<RootCaseConflictSkeletonManifestId>::new(manager))
        }
        DerivableUntopologicallyVariant::GitDeltaManifestsV3 => {
            Arc::new(SingleTypeManager::<RootGitDeltaManifestV3Id>::new(manager))
        }
        DerivableUntopologicallyVariant::InferredCopyFrom => {
            Arc::new(SingleTypeManager::<RootInferredCopyFromId>::new(manager))
        }
        DerivableUntopologicallyVariant::TestShardedManifests => Arc::new(SingleTypeManager::<
            RootTestShardedManifestDirectory,
        >::new(manager)),
        DerivableUntopologicallyVariant::AclManifests => {
            Arc::new(SingleTypeManager::<RootAclManifestId>::new(manager))
        }
    }
}

pub async fn derive_stage_batch(
    ddm: &DerivedDataManager,
    ctx: &CoreContext,
    csids: Vec<ChangesetId>,
    payload: &derived_data_manager::DerivationStagePayload,
    variant: PipelineDerivableVariant,
) -> Result<StageBatchResult, DerivationError> {
    match variant {
        PipelineDerivableVariant::Fsnodes => {
            ddm.derive_stage_batch::<RootFsnodeId>(ctx, csids, payload)
                .await
        }
        PipelineDerivableVariant::Unodes => {
            ddm.derive_stage_batch::<RootUnodeManifestId>(ctx, csids, payload)
                .await
        }
        PipelineDerivableVariant::SkeletonManifestsV2 => {
            ddm.derive_stage_batch::<RootSkeletonManifestV2Id>(ctx, csids, payload)
                .await
        }
        PipelineDerivableVariant::SkeletonManifests => {
            ddm.derive_stage_batch::<RootSkeletonManifestId>(ctx, csids, payload)
                .await
        }
        PipelineDerivableVariant::BlameV2 => {
            ddm.derive_stage_batch::<RootBlameV2>(ctx, csids, payload)
                .await
        }
        PipelineDerivableVariant::Fastlog => {
            ddm.derive_stage_batch::<RootFastlog>(ctx, csids, payload)
                .await
        }
        PipelineDerivableVariant::AclManifests => {
            ddm.derive_stage_batch::<RootAclManifestId>(ctx, csids, payload)
                .await
        }
        PipelineDerivableVariant::HgChangesets => {
            ddm.derive_stage_batch::<MappedHgChangesetId>(ctx, csids, payload)
                .await
        }
        PipelineDerivableVariant::HgAugmentedManifests => {
            ddm.derive_stage_batch::<RootHgAugmentedManifestId>(ctx, csids, payload)
                .await
        }
        PipelineDerivableVariant::ContentManifests => {
            ddm.derive_stage_batch::<RootContentManifestId>(ctx, csids, payload)
                .await
        }
        PipelineDerivableVariant::DeletedManifests => {
            ddm.derive_stage_batch::<RootDeletedManifestV2Id>(ctx, csids, payload)
                .await
        }
    }
}

pub async fn is_stage_derived(
    ddm: &DerivedDataManager,
    ctx: &CoreContext,
    csid: ChangesetId,
    stage: &StageId,
    variant: PipelineDerivableVariant,
) -> Result<bool, DerivationError> {
    match variant {
        PipelineDerivableVariant::Fsnodes => {
            ddm.is_stage_derived::<RootFsnodeId>(ctx, csid, stage).await
        }
        PipelineDerivableVariant::Unodes => {
            ddm.is_stage_derived::<RootUnodeManifestId>(ctx, csid, stage)
                .await
        }
        PipelineDerivableVariant::SkeletonManifestsV2 => {
            ddm.is_stage_derived::<RootSkeletonManifestV2Id>(ctx, csid, stage)
                .await
        }
        PipelineDerivableVariant::SkeletonManifests => {
            ddm.is_stage_derived::<RootSkeletonManifestId>(ctx, csid, stage)
                .await
        }
        PipelineDerivableVariant::BlameV2 => {
            ddm.is_stage_derived::<RootBlameV2>(ctx, csid, stage).await
        }
        PipelineDerivableVariant::Fastlog => {
            ddm.is_stage_derived::<RootFastlog>(ctx, csid, stage).await
        }
        PipelineDerivableVariant::AclManifests => {
            ddm.is_stage_derived::<RootAclManifestId>(ctx, csid, stage)
                .await
        }
        PipelineDerivableVariant::HgChangesets => {
            ddm.is_stage_derived::<MappedHgChangesetId>(ctx, csid, stage)
                .await
        }
        PipelineDerivableVariant::HgAugmentedManifests => {
            ddm.is_stage_derived::<RootHgAugmentedManifestId>(ctx, csid, stage)
                .await
        }
        PipelineDerivableVariant::ContentManifests => {
            ddm.is_stage_derived::<RootContentManifestId>(ctx, csid, stage)
                .await
        }
        PipelineDerivableVariant::DeletedManifests => {
            ddm.is_stage_derived::<RootDeletedManifestV2Id>(ctx, csid, stage)
                .await
        }
    }
}

pub async fn verify_stage_output(
    ddm: &DerivedDataManager,
    ctx: &CoreContext,
    csid: ChangesetId,
    stage: &StageId,
    variant: PipelineDerivableVariant,
) -> Result<bool, DerivationError> {
    match variant {
        PipelineDerivableVariant::Fsnodes => {
            ddm.verify_stage_output::<RootFsnodeId>(ctx, csid, stage)
                .await
        }
        PipelineDerivableVariant::Unodes => {
            ddm.verify_stage_output::<RootUnodeManifestId>(ctx, csid, stage)
                .await
        }
        PipelineDerivableVariant::SkeletonManifestsV2 => {
            ddm.verify_stage_output::<RootSkeletonManifestV2Id>(ctx, csid, stage)
                .await
        }
        PipelineDerivableVariant::SkeletonManifests => {
            ddm.verify_stage_output::<RootSkeletonManifestId>(ctx, csid, stage)
                .await
        }
        PipelineDerivableVariant::BlameV2 => {
            ddm.verify_stage_output::<RootBlameV2>(ctx, csid, stage)
                .await
        }
        PipelineDerivableVariant::Fastlog => {
            ddm.verify_stage_output::<RootFastlog>(ctx, csid, stage)
                .await
        }
        PipelineDerivableVariant::AclManifests => {
            ddm.verify_stage_output::<RootAclManifestId>(ctx, csid, stage)
                .await
        }
        PipelineDerivableVariant::HgChangesets => {
            ddm.verify_stage_output::<MappedHgChangesetId>(ctx, csid, stage)
                .await
        }
        PipelineDerivableVariant::HgAugmentedManifests => {
            ddm.verify_stage_output::<RootHgAugmentedManifestId>(ctx, csid, stage)
                .await
        }
        PipelineDerivableVariant::ContentManifests => {
            ddm.verify_stage_output::<RootContentManifestId>(ctx, csid, stage)
                .await
        }
        PipelineDerivableVariant::DeletedManifests => {
            ddm.verify_stage_output::<RootDeletedManifestV2Id>(ctx, csid, stage)
                .await
        }
    }
}

/// Whether the given pipeline type has a finalize stage (a step distinct from
/// its terminal manifest stage). Currently only HgChangesets does.
pub fn pipeline_has_finalize(variant: PipelineDerivableVariant) -> bool {
    match variant {
        PipelineDerivableVariant::Fsnodes => RootFsnodeId::HAS_FINALIZE,
        PipelineDerivableVariant::Unodes => RootUnodeManifestId::HAS_FINALIZE,
        PipelineDerivableVariant::SkeletonManifestsV2 => RootSkeletonManifestV2Id::HAS_FINALIZE,
        PipelineDerivableVariant::SkeletonManifests => RootSkeletonManifestId::HAS_FINALIZE,
        PipelineDerivableVariant::BlameV2 => RootBlameV2::HAS_FINALIZE,
        PipelineDerivableVariant::Fastlog => RootFastlog::HAS_FINALIZE,
        PipelineDerivableVariant::AclManifests => RootAclManifestId::HAS_FINALIZE,
        PipelineDerivableVariant::HgChangesets => MappedHgChangesetId::HAS_FINALIZE,
        PipelineDerivableVariant::HgAugmentedManifests => RootHgAugmentedManifestId::HAS_FINALIZE,
        PipelineDerivableVariant::ContentManifests => RootContentManifestId::HAS_FINALIZE,
        PipelineDerivableVariant::DeletedManifests => RootDeletedManifestV2Id::HAS_FINALIZE,
    }
}

#[async_trait]
impl BulkDerivation for DerivedDataManager {
    async fn derive_bulk_locally(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
        derived_data_types: &[DerivableType],
        override_batch_size: Option<u64>,
        override_concurrency: Option<usize>,
    ) -> Result<(), SharedDerivationError> {
        let visited = VisitedDerivableTypesMap::default();
        stream::iter(derived_data_types)
            .map(move |derived_data_type| {
                cloned!(rederivation, visited, ctx);
                let csids = csids.to_vec();
                let manager = manager_for_type(self, *derived_data_type);
                async move {
                    mononoke::spawn_task(async move {
                        manager
                            .derive_heads_with_visited(
                                ctx,
                                csids,
                                override_batch_size,
                                rederivation,
                                visited,
                            )
                            .await
                    })
                    .await
                    .map_err(|err| {
                        SharedDerivationError::from(DerivationError::from(Error::from(err)))
                    })?
                }
            })
            .boxed()
            .buffer_unordered(override_concurrency.unwrap_or(10).max(1))
            .try_collect::<Vec<_>>()
            .await?;

        Ok(())
    }

    async fn derive_bulk(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
        derived_data_types: &[DerivableType],
        override_concurrency: Option<usize>,
    ) -> Result<(), SharedDerivationError> {
        stream::iter(
            derived_data_types
                .iter()
                .map(|ddt| manager_for_type(self, *ddt))
                .cartesian_product(csids),
        )
        .map(async |(manager, csid)| manager.derive(ctx, *csid, rederivation.clone()).await)
        .boxed()
        .buffer_unordered(override_concurrency.unwrap_or(10).max(1))
        .try_collect::<Vec<_>>()
        .await?;

        Ok(())
    }

    async fn derive_exactly_batch(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
        derived_data_type: DerivableType,
    ) -> Result<(), DerivationError> {
        let manager = manager_for_type(self, derived_data_type);
        manager.derive_exactly_batch(ctx, csids, rederivation).await
    }

    async fn derive_exactly_underived_batch(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
        derived_data_type: DerivableType,
    ) -> Result<(), DerivationError> {
        let manager = manager_for_type(self, derived_data_type);
        manager
            .derive_exactly_underived_batch(ctx, csids, rederivation)
            .await
    }

    async fn is_derived(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
        derived_data_type: DerivableType,
    ) -> Result<bool, DerivationError> {
        let manager = manager_for_type(self, derived_data_type);
        manager.is_derived(ctx, csid, rederivation).await
    }

    async fn is_stage_derived(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        derived_data_type: DerivableType,
        stage: &StageId,
    ) -> Result<bool, DerivationError> {
        let variant = derived_data_type.into_pipeline_derivable_variant()?;
        is_stage_derived(self, ctx, csid, stage, variant).await
    }

    async fn verify_stage_output(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        derived_data_type: DerivableType,
        stage: &StageId,
    ) -> Result<bool, DerivationError> {
        let variant = derived_data_type.into_pipeline_derivable_variant()?;
        verify_stage_output(self, ctx, csid, stage, variant).await
    }

    async fn pending(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
        derived_data_type: DerivableType,
    ) -> Result<Vec<ChangesetId>, DerivationError> {
        let manager = manager_for_type(self, derived_data_type);
        manager.pending(ctx, csids, rederivation).await
    }

    async fn fetch_derived_batch(
        &self,
        ctx: &CoreContext,
        csids: &[ChangesetId],
        rederivation: Option<Arc<dyn Rederivation>>,
        derived_data_type: DerivableType,
    ) -> Result<HashMap<ChangesetId, String>, DerivationError> {
        let manager = manager_for_type(self, derived_data_type);
        manager.fetch_derived_batch(ctx, csids, rederivation).await
    }

    async fn count_underived(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
        derived_data_type: DerivableType,
    ) -> Result<u64, DerivationError> {
        let manager = manager_for_type(self, derived_data_type);
        manager.count_underived(ctx, csid, rederivation).await
    }

    async fn unsafe_derive_untopologically(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        rederivation: Option<Arc<dyn Rederivation>>,
        derived_data_type: DerivableType,
    ) -> Result<(), DerivationError> {
        let variant = derived_data_type.into_derivable_untopologically_variant()?;
        let manager = manager_for_derivable_untopologically_variant(self, variant);
        manager
            .unsafe_derive_untopologically(ctx, csid, rederivation)
            .await
    }

    fn dependency_types(&self, derived_data_type: DerivableType) -> Vec<DerivableType> {
        manager_for_type(self, derived_data_type).dependency_types()
    }
}
