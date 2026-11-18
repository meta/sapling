/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use strum::AsRefStr;
use strum::Display;
use strum::EnumIter;
use strum::EnumString;

use crate::thrift;

/// Enum which consolidates all available derived data types
/// It provides access to `const &'static str` representation to
/// use as Name of the derived data type, which is used to
/// identify or name data (for example lease keys) associated with this
/// particular derived data type.
/// It also provides `as_ref()` method for serialization.
/// and implements FromStr trait for deserialization.
#[derive(
    AsRefStr,
    EnumString,
    Display,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    EnumIter,
    Ord,
    PartialOrd,
    Serialize,
    Deserialize
)]
pub enum DerivableType {
    BlameV2,
    BlameV3,
    BssmV3,
    Ccsm,
    ChangesetInfo,
    DeletedManifests,
    Fastlog,
    FastlogV2,
    FileNodes,
    Fsnodes,
    HgChangesets,
    HgAugmentedManifests,
    GitCommits,
    GitDeltaManifestsV2,
    GitDeltaManifestsV3,
    InferredCopyFrom,
    SkeletonManifests,
    SkeletonManifestsV2,
    TestManifests,
    TestShardedManifests,
    Unodes,
    ContentManifests,
    DirectoryBranchClusterManifest,
    AclManifests,
    HistoryManifests,
}

/// Enum which consolidates all derived data types that can
/// be derived without having data derived for their parents.
#[derive(Clone, Copy, Debug, EnumIter, Eq, PartialEq)]
pub enum DerivableUntopologicallyVariant {
    AclManifests,
    BlameV3,
    BssmV3,
    Ccsm,
    HgAugmentedManifests,
    GitDeltaManifestsV3,
    InferredCopyFrom,
    SkeletonManifestsV2,
    TestShardedManifests,
}

/// Enum which consolidates all derived data types that support
/// multi-stage derivation via the PipelineDerivable trait.
#[derive(Clone, Copy, Debug, EnumIter, Eq, PartialEq)]
pub enum PipelineDerivableVariant {
    FileNodes,
    BssmV3,
    Ccsm,
    Fsnodes,
    Unodes,
    SkeletonManifests,
    SkeletonManifestsV2,
    BlameV2,
    Fastlog,
    AclManifests,
    HgChangesets,
    HgAugmentedManifests,
    ContentManifests,
    DeletedManifests,
}

impl DerivableType {
    pub fn from_name(s: &str) -> Result<Self> {
        // We need the duplication here to make it a `const fn` so it can be used in
        // BonsaiDerivable::NAME
        Ok(match s {
            "blame" => DerivableType::BlameV2,
            "blame_v3" => DerivableType::BlameV3,
            "bssm_v3" => DerivableType::BssmV3,
            "ccsm" => DerivableType::Ccsm,
            "changeset_info" => DerivableType::ChangesetInfo,
            "deleted_manifest" => DerivableType::DeletedManifests,
            "fastlog" => DerivableType::Fastlog,
            "fastlog_v2" => DerivableType::FastlogV2,
            "filenodes" => DerivableType::FileNodes,
            "fsnodes" => DerivableType::Fsnodes,
            "hgchangesets" => DerivableType::HgChangesets,
            "hg_augmented_manifests" => DerivableType::HgAugmentedManifests,
            "git_commits" => DerivableType::GitCommits,
            "git_delta_manifests_v2" => DerivableType::GitDeltaManifestsV2,
            "git_delta_manifests_v3" => DerivableType::GitDeltaManifestsV3,
            "inferred_copy_from" => DerivableType::InferredCopyFrom,
            "skeleton_manifests" => DerivableType::SkeletonManifests,
            "skeleton_manifests_v2" => DerivableType::SkeletonManifestsV2,
            "test_manifests" => DerivableType::TestManifests,
            "test_sharded_manifests" => DerivableType::TestShardedManifests,
            "unodes" => DerivableType::Unodes,
            "content_manifests" => DerivableType::ContentManifests,
            "directory_branch_cluster_manifest" => DerivableType::DirectoryBranchClusterManifest,
            "acl_manifests" => DerivableType::AclManifests,
            "history_manifests" => DerivableType::HistoryManifests,
            _ => bail!("invalid name for DerivedDataType: {s}"),
        })
    }
    pub const fn name(&self) -> &'static str {
        // We need the duplication here to make it a `const fn` so it can be used in
        // BonsaiDerivable::NAME
        match self {
            DerivableType::BlameV2 => "blame",
            DerivableType::BlameV3 => "blame_v3",
            DerivableType::BssmV3 => "bssm_v3",
            DerivableType::Ccsm => "ccsm",
            DerivableType::ChangesetInfo => "changeset_info",
            DerivableType::DeletedManifests => "deleted_manifest",
            DerivableType::Fastlog => "fastlog",
            DerivableType::FastlogV2 => "fastlog_v2",
            DerivableType::FileNodes => "filenodes",
            DerivableType::Fsnodes => "fsnodes",
            DerivableType::HgChangesets => "hgchangesets",
            DerivableType::HgAugmentedManifests => "hg_augmented_manifests",
            DerivableType::GitCommits => "git_commits",
            DerivableType::GitDeltaManifestsV2 => "git_delta_manifests_v2",
            DerivableType::GitDeltaManifestsV3 => "git_delta_manifests_v3",
            DerivableType::InferredCopyFrom => "inferred_copy_from",
            DerivableType::SkeletonManifests => "skeleton_manifests",
            DerivableType::SkeletonManifestsV2 => "skeleton_manifests_v2",
            DerivableType::TestManifests => "test_manifests",
            DerivableType::TestShardedManifests => "test_sharded_manifests",
            DerivableType::Unodes => "unodes",
            DerivableType::ContentManifests => "content_manifests",
            DerivableType::DirectoryBranchClusterManifest => "directory_branch_cluster_manifest",
            DerivableType::AclManifests => "acl_manifests",
            DerivableType::HistoryManifests => "history_manifests",
        }
    }
    pub fn from_thrift(other: thrift::DerivedDataType) -> Result<Self> {
        Ok(match other {
            thrift::DerivedDataType::BLAME => Self::BlameV2,
            thrift::DerivedDataType::BLAME_V3 => Self::BlameV3,
            thrift::DerivedDataType::BSSM_V3 => Self::BssmV3,
            thrift::DerivedDataType::CCSM => Self::Ccsm,
            thrift::DerivedDataType::CHANGESET_INFO => Self::ChangesetInfo,
            thrift::DerivedDataType::DELETED_MANIFEST_V2 => Self::DeletedManifests,
            thrift::DerivedDataType::FASTLOG => Self::Fastlog,
            thrift::DerivedDataType::FASTLOG_V2 => Self::FastlogV2,
            thrift::DerivedDataType::FILENODE => Self::FileNodes,
            thrift::DerivedDataType::FSNODE => Self::Fsnodes,
            thrift::DerivedDataType::HG_CHANGESET => Self::HgChangesets,
            thrift::DerivedDataType::HG_AUGMENTED_MANIFEST => Self::HgAugmentedManifests,
            thrift::DerivedDataType::COMMIT_HANDLE => Self::GitCommits,
            thrift::DerivedDataType::GIT_DELTA_MANIFEST_V2 => Self::GitDeltaManifestsV2,
            thrift::DerivedDataType::GIT_DELTA_MANIFEST_V3 => Self::GitDeltaManifestsV3,
            thrift::DerivedDataType::INFERRED_COPY_FROM => Self::InferredCopyFrom,
            thrift::DerivedDataType::SKELETON_MANIFEST => Self::SkeletonManifests,
            thrift::DerivedDataType::SKELETON_MANIFEST_V2 => Self::SkeletonManifestsV2,
            thrift::DerivedDataType::TEST_MANIFEST => Self::TestManifests,
            thrift::DerivedDataType::TEST_SHARDED_MANIFEST => Self::TestShardedManifests,
            thrift::DerivedDataType::UNODE => Self::Unodes,
            thrift::DerivedDataType::CONTENT_MANIFEST => Self::ContentManifests,
            thrift::DerivedDataType::DIRECTORY_BRANCH_CLUSTER_MANIFEST => {
                Self::DirectoryBranchClusterManifest
            }
            thrift::DerivedDataType::ACL_MANIFEST => Self::AclManifests,
            thrift::DerivedDataType::HISTORY_MANIFEST => Self::HistoryManifests,
            _ => bail!("invalid thrift value for DerivedDataType: {other:?}"),
        })
    }
    pub fn into_thrift(&self) -> thrift::DerivedDataType {
        match self {
            Self::BlameV2 => thrift::DerivedDataType::BLAME,
            Self::BlameV3 => thrift::DerivedDataType::BLAME_V3,
            Self::BssmV3 => thrift::DerivedDataType::BSSM_V3,
            Self::Ccsm => thrift::DerivedDataType::CCSM,
            Self::ChangesetInfo => thrift::DerivedDataType::CHANGESET_INFO,
            Self::DeletedManifests => thrift::DerivedDataType::DELETED_MANIFEST_V2,
            Self::Fastlog => thrift::DerivedDataType::FASTLOG,
            Self::FastlogV2 => thrift::DerivedDataType::FASTLOG_V2,
            Self::FileNodes => thrift::DerivedDataType::FILENODE,
            Self::Fsnodes => thrift::DerivedDataType::FSNODE,
            Self::HgChangesets => thrift::DerivedDataType::HG_CHANGESET,
            Self::HgAugmentedManifests => thrift::DerivedDataType::HG_AUGMENTED_MANIFEST,
            Self::GitCommits => thrift::DerivedDataType::COMMIT_HANDLE,
            Self::GitDeltaManifestsV2 => thrift::DerivedDataType::GIT_DELTA_MANIFEST_V2,
            Self::GitDeltaManifestsV3 => thrift::DerivedDataType::GIT_DELTA_MANIFEST_V3,
            Self::InferredCopyFrom => thrift::DerivedDataType::INFERRED_COPY_FROM,
            Self::SkeletonManifests => thrift::DerivedDataType::SKELETON_MANIFEST,
            Self::SkeletonManifestsV2 => thrift::DerivedDataType::SKELETON_MANIFEST_V2,
            Self::TestManifests => thrift::DerivedDataType::TEST_MANIFEST,
            Self::TestShardedManifests => thrift::DerivedDataType::TEST_SHARDED_MANIFEST,
            Self::Unodes => thrift::DerivedDataType::UNODE,
            Self::ContentManifests => thrift::DerivedDataType::CONTENT_MANIFEST,
            Self::DirectoryBranchClusterManifest => {
                thrift::DerivedDataType::DIRECTORY_BRANCH_CLUSTER_MANIFEST
            }
            Self::AclManifests => thrift::DerivedDataType::ACL_MANIFEST,
            Self::HistoryManifests => thrift::DerivedDataType::HISTORY_MANIFEST,
            // If the compiler reminds you to add something here, please don't forget to also
            // update the `from_thrift` implementation above.
            // The unit test: `thrift_derived_data_type_conversion_must_be_bidirectional` in this
            // file should prevent you from forgetting at diff time.
        }
    }
    pub fn into_derivable_untopologically_variant(self) -> Result<DerivableUntopologicallyVariant> {
        match self {
            DerivableType::AclManifests => Ok(DerivableUntopologicallyVariant::AclManifests),
            DerivableType::BlameV3 => Ok(DerivableUntopologicallyVariant::BlameV3),
            DerivableType::BssmV3 => Ok(DerivableUntopologicallyVariant::BssmV3),
            DerivableType::Ccsm => Ok(DerivableUntopologicallyVariant::Ccsm),
            DerivableType::HgAugmentedManifests => {
                Ok(DerivableUntopologicallyVariant::HgAugmentedManifests)
            }
            DerivableType::GitDeltaManifestsV3 => {
                Ok(DerivableUntopologicallyVariant::GitDeltaManifestsV3)
            }
            DerivableType::InferredCopyFrom => {
                Ok(DerivableUntopologicallyVariant::InferredCopyFrom)
            }
            DerivableType::SkeletonManifestsV2 => {
                Ok(DerivableUntopologicallyVariant::SkeletonManifestsV2)
            }
            DerivableType::TestShardedManifests => {
                Ok(DerivableUntopologicallyVariant::TestShardedManifests)
            }
            _ => bail!(
                "{} is not an untopologically derived data type",
                self.name()
            ),
        }
    }
    pub fn into_pipeline_derivable_variant(self) -> Result<PipelineDerivableVariant> {
        match self {
            DerivableType::FileNodes => Ok(PipelineDerivableVariant::FileNodes),
            DerivableType::BssmV3 => Ok(PipelineDerivableVariant::BssmV3),
            DerivableType::Ccsm => Ok(PipelineDerivableVariant::Ccsm),
            DerivableType::Fsnodes => Ok(PipelineDerivableVariant::Fsnodes),
            DerivableType::Unodes => Ok(PipelineDerivableVariant::Unodes),
            DerivableType::SkeletonManifests => Ok(PipelineDerivableVariant::SkeletonManifests),
            DerivableType::SkeletonManifestsV2 => Ok(PipelineDerivableVariant::SkeletonManifestsV2),
            DerivableType::BlameV2 => Ok(PipelineDerivableVariant::BlameV2),
            DerivableType::Fastlog => Ok(PipelineDerivableVariant::Fastlog),
            DerivableType::AclManifests => Ok(PipelineDerivableVariant::AclManifests),
            DerivableType::HgChangesets => Ok(PipelineDerivableVariant::HgChangesets),
            DerivableType::HgAugmentedManifests => {
                Ok(PipelineDerivableVariant::HgAugmentedManifests)
            }
            DerivableType::ContentManifests => Ok(PipelineDerivableVariant::ContentManifests),
            DerivableType::DeletedManifests => Ok(PipelineDerivableVariant::DeletedManifests),
            _ => bail!("{} does not support derivation pipeline", self.name()),
        }
    }
}

impl PipelineDerivableVariant {
    pub fn into_derivable_type(self) -> DerivableType {
        match self {
            PipelineDerivableVariant::FileNodes => DerivableType::FileNodes,
            PipelineDerivableVariant::BssmV3 => DerivableType::BssmV3,
            PipelineDerivableVariant::Ccsm => DerivableType::Ccsm,
            PipelineDerivableVariant::Fsnodes => DerivableType::Fsnodes,
            PipelineDerivableVariant::Unodes => DerivableType::Unodes,
            PipelineDerivableVariant::SkeletonManifests => DerivableType::SkeletonManifests,
            PipelineDerivableVariant::SkeletonManifestsV2 => DerivableType::SkeletonManifestsV2,
            PipelineDerivableVariant::BlameV2 => DerivableType::BlameV2,
            PipelineDerivableVariant::Fastlog => DerivableType::Fastlog,
            PipelineDerivableVariant::AclManifests => DerivableType::AclManifests,
            PipelineDerivableVariant::HgChangesets => DerivableType::HgChangesets,
            PipelineDerivableVariant::HgAugmentedManifests => DerivableType::HgAugmentedManifests,
            PipelineDerivableVariant::ContentManifests => DerivableType::ContentManifests,
            PipelineDerivableVariant::DeletedManifests => DerivableType::DeletedManifests,
        }
    }
}

impl DerivableUntopologicallyVariant {
    pub fn into_derivable_type(self) -> DerivableType {
        match self {
            DerivableUntopologicallyVariant::AclManifests => DerivableType::AclManifests,
            DerivableUntopologicallyVariant::BlameV3 => DerivableType::BlameV3,
            DerivableUntopologicallyVariant::BssmV3 => DerivableType::BssmV3,
            DerivableUntopologicallyVariant::Ccsm => DerivableType::Ccsm,
            DerivableUntopologicallyVariant::HgAugmentedManifests => {
                DerivableType::HgAugmentedManifests
            }
            DerivableUntopologicallyVariant::GitDeltaManifestsV3 => {
                DerivableType::GitDeltaManifestsV3
            }
            DerivableUntopologicallyVariant::InferredCopyFrom => DerivableType::InferredCopyFrom,
            DerivableUntopologicallyVariant::SkeletonManifestsV2 => {
                DerivableType::SkeletonManifestsV2
            }
            DerivableUntopologicallyVariant::TestShardedManifests => {
                DerivableType::TestShardedManifests
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;
    use strum::IntoEnumIterator;

    use super::DerivableType;
    use super::DerivableUntopologicallyVariant;
    use super::PipelineDerivableVariant;

    #[mononoke::test]
    fn thrift_derived_data_type_conversion_must_be_bidirectional() {
        for variant in DerivableType::iter() {
            assert_eq!(
                variant,
                DerivableType::from_thrift(variant.into_thrift())
                    .expect("Failed to convert back to DerivableType from thrift")
            );
        }
    }
    #[mononoke::test]
    fn name_derived_data_type_conversion_must_be_bidirectional() {
        for variant in DerivableType::iter() {
            assert_eq!(
                variant,
                DerivableType::from_name(variant.name()).expect(
                    "Failed to convert back to DerivableType from its string representation with DerivableType::name"
                )
            );
        }
    }

    #[mononoke::test]
    fn pipeline_derivable_variant_into_derivable_type_is_bidirectional() {
        for variant in PipelineDerivableVariant::iter() {
            assert_eq!(
                variant,
                variant
                    .into_derivable_type()
                    .into_pipeline_derivable_variant()
                    .expect(
                        "Failed to convert back to PipelineDerivableVariant from DerivableType"
                    )
            );
        }
    }

    #[mononoke::test]
    fn derivable_untopologically_variant_into_derivable_type_is_bidirectional() {
        for variant in DerivableUntopologicallyVariant::iter() {
            assert_eq!(
                variant,
                variant
                    .into_derivable_type()
                    .into_derivable_untopologically_variant()
                    .expect("Failed to convert back to DerivableUntopologicallyVariant from DerivableType")
            );
        }
    }
}
