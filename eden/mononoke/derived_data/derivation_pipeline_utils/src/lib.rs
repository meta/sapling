/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Shared type ordering and chokepoint batching for the derivation pipeline.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::Result;
use anyhow::ensure;
use derived_data_manager::ManifestStagePayload;
use derived_data_manager::StageId;
use derived_data_manager::StageKey;
use derived_data_manager::partition_ranges;
use metaconfig_types::DerivationPipelineConfig;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use mononoke_types::NonRootMPath;

/// Manifest stages in dependency order, deepest paths first.
pub fn ordered_stage_payloads(
    config: &DerivationPipelineConfig,
    ty: DerivableType,
) -> Vec<ManifestStagePayload> {
    let mut stages = config
        .stages_for(ty)
        .iter()
        .map(|(path, stage)| ManifestStagePayload::from_config(path.clone(), stage))
        .collect::<Vec<_>>();
    stages.sort_by(|a, b| {
        b.path
            .num_components()
            .cmp(&a.path.num_components())
            .then_with(|| a.path.cmp(&b.path))
    });
    stages
}

/// Order selected types with dependencies first, ignoring dependencies outside the set.
pub fn topo_sort_types(
    mut remaining: BTreeSet<DerivableType>,
    dependencies: impl Fn(DerivableType) -> Vec<DerivableType>,
) -> Result<Vec<DerivableType>> {
    let mut sorted = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        let ready: Vec<_> = remaining
            .iter()
            .copied()
            .filter(|t| {
                dependencies(*t)
                    .into_iter()
                    .all(|dep| !remaining.contains(&dep))
            })
            .collect();
        ensure!(
            !ready.is_empty(),
            "Cycle in derivable type dependencies: {remaining:?}"
        );
        for t in ready {
            remaining.remove(&t);
            sorted.push(t);
        }
    }
    Ok(sorted)
}

/// Why a single-changeset batch is a chokepoint, and how far its barrier reaches.
pub enum ChokepointKind {
    /// Subtree change: the source can span many stages, so every stage depends
    /// on the previous batch's terminal stage. Rare, so not scoped.
    Global,
    /// Cross-stage `copy_from`: only these dest stages depend on the previous
    /// batch's terminal stage; every other stage keeps its normal per-stage dep.
    Stages(HashSet<StageKey>),
}

/// A batch of changesets, optionally a single-changeset chokepoint.
pub struct Batch {
    pub commits: Vec<ChangesetId>,
    pub chokepoint: Option<ChokepointKind>,
}

/// The stage that owns `path`: the longest stage path that is a prefix of it.
/// Stages partition the path space (`MPath::ROOT` is the catch-all), so this is
/// always `Some`.
pub fn owning_stage(
    pipeline_config: &DerivationPipelineConfig,
    path: &NonRootMPath,
) -> Option<StageId> {
    let prefix = pipeline_config
        .stages
        .keys()
        .filter(|stage| stage.is_prefix_of(path))
        .max_by_key(|stage| stage.num_components())?;
    let config = &pipeline_config.stages[prefix];
    if !config.partition_cut_points.is_empty() {
        if let Some(range) = partition_ranges(&config.partition_cut_points)
            .find(|range| range.contains_path(prefix, path))
        {
            return Some(StageId::Subentries(prefix.clone(), range));
        }
    }
    Some(StageId::Manifest(prefix.clone()))
}

/// Classify a changeset as a chokepoint and how far its barrier reaches.
pub fn chokepoint_kind(
    bcs: &BonsaiChangeset,
    pipeline_config: &DerivationPipelineConfig,
) -> Option<ChokepointKind> {
    // Subtree changes graft from a source that can cross many stage
    // boundaries; keep the global barrier (rare, not worth scoping).
    if bcs.has_subtree_changes() {
        return Some(ChokepointKind::Global);
    }

    // Cross-stage `copy_from`: scope the barrier to each copy's dest stage. A
    // copy is cross-stage iff the dest's owning stage does not also cover src.
    let dest_stages: HashSet<StageKey> = bcs
        .file_changes()
        .filter_map(|(dest, fc)| {
            let (src, _) = fc.copy_from()?;
            let dest_stage = owning_stage(pipeline_config, dest)?;
            (!dest_stage.covers(src)).then(|| dest_stage.to_key())
        })
        .collect();

    (!dest_stages.is_empty()).then_some(ChokepointKind::Stages(dest_stages))
}

/// Split topologically-ordered batches at chokepoint changesets. A chokepoint
/// changeset becomes its own single-changeset batch so its vertical dep can
/// target the previous batch's terminal stage.
pub fn split_batches_at_chokepoints(
    batches: Vec<Vec<ChangesetId>>,
    bonsais: &HashMap<ChangesetId, BonsaiChangeset>,
    config: &DerivationPipelineConfig,
) -> Vec<Batch> {
    let mut split_batches: Vec<Batch> = Vec::new();
    for batch in batches {
        let mut current_batch: Vec<ChangesetId> = Vec::new();
        for cs_id in batch {
            let chokepoint = bonsais
                .get(&cs_id)
                .and_then(|bcs| chokepoint_kind(bcs, config));
            if chokepoint.is_some() {
                if !current_batch.is_empty() {
                    split_batches.push(Batch {
                        commits: std::mem::take(&mut current_batch),
                        chokepoint: None,
                    });
                }
                split_batches.push(Batch {
                    commits: vec![cs_id],
                    chokepoint,
                });
            } else {
                current_batch.push(cs_id);
            }
        }
        if !current_batch.is_empty() {
            split_batches.push(Batch {
                commits: current_batch,
                chokepoint: None,
            });
        }
    }
    split_batches
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::test]
    fn orders_dependencies_and_ignores_unselected_types() -> Result<()> {
        let dependencies = |ty| match ty {
            DerivableType::BlameV2 | DerivableType::Fastlog => vec![DerivableType::Unodes],
            _ => vec![],
        };
        let types = BTreeSet::from([
            DerivableType::BlameV2,
            DerivableType::Fastlog,
            DerivableType::Unodes,
        ]);
        let sorted = topo_sort_types(types.clone(), dependencies)?;
        assert_eq!(sorted.first(), Some(&DerivableType::Unodes));
        assert_eq!(sorted.into_iter().collect::<BTreeSet<_>>(), types);
        assert_eq!(
            topo_sort_types(BTreeSet::from([DerivableType::BlameV2]), dependencies)?,
            vec![DerivableType::BlameV2],
        );
        assert!(topo_sort_types(BTreeSet::new(), dependencies)?.is_empty());
        Ok(())
    }

    #[mononoke::test]
    fn rejects_cycles() {
        assert!(topo_sort_types(BTreeSet::from([DerivableType::Unodes]), |ty| vec![ty]).is_err());
        let types = BTreeSet::from([DerivableType::BlameV2, DerivableType::Unodes]);
        let error = topo_sort_types(types, |ty| match ty {
            DerivableType::BlameV2 => vec![DerivableType::Unodes],
            _ => vec![DerivableType::BlameV2],
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Cycle in derivable type dependencies")
        );
    }
}
