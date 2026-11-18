/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![feature(associated_type_defaults)]

pub mod context;
pub mod derivable;
pub mod error;
pub mod manager;
pub mod manifest_partitions;
pub mod manifest_pipeline;
pub use self::manifest_partitions::ManifestDerivationInput;
pub use self::manifest_partitions::ManifestStageOutput;
pub mod stage_outputs;
pub mod stage_payload;
pub use mononoke_types::DerivableType;

pub use self::context::DerivationContext;
pub use self::derivable::BonsaiDerivable;
pub use self::derivable::DerivableUntopologically;
pub use self::derivable::PipelineDerivable;
pub use self::error::DerivationError;
pub use self::error::SharedDerivationError;
pub use self::manager::DerivedDataManager;
pub use self::manager::derive::Rederivation;
pub use self::manager::derive::VisitedDerivableTypesMap;
pub use self::manager::derive::VisitedDerivableTypesMapStatic;
pub use self::manifest_pipeline::ManifestDerivable;
pub use self::stage_outputs::StageBatchResult;
pub use self::stage_outputs::StageInputs;
pub use self::stage_outputs::StageOutputs;
pub use self::stage_payload::ChildNameRange;
pub use self::stage_payload::DerivationStagePayload;
pub use self::stage_payload::ManifestStageDependencies;
pub use self::stage_payload::ManifestStagePayload;
pub use self::stage_payload::StageId;
pub use self::stage_payload::StageKey;
pub use self::stage_payload::partition_ranges;
