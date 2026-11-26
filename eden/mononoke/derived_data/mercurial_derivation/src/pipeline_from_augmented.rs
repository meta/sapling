/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! The Hg changeset pipeline's manifest stages when Hg changesets are derived
//! from augmented manifests. Each stage's Hg output is the augmented manifest
//! pipeline's output for the same stage, converted to Hg ids, except a
//! partition a one-parent commit leaves unchanged, which keeps the parent's
//! output. Each stage stores the new Hg trees it owns, so no Hg manifest is
//! read.
//!
//! Hg and augmented manifests must use the same pipeline stage layout: a
//! commit without an augmented output for an Hg stage is an error.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use context::CoreContext;
use derived_data_manager::DerivationContext;
use derived_data_manager::DerivationStagePayload;
use derived_data_manager::ManifestStageDependencies;
use derived_data_manager::ManifestStageOutput;
use derived_data_manager::PipelineDerivable;
use derived_data_manager::StageId;
use derived_data_manager::manifest_partitions::PartitionOutput;
use derived_data_manager::manifest_pipeline::derive_manifest_batch;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future::try_join_all;
use futures::stream;
use manifest::Entry;
use manifest::TrieMapOps;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgManifestId;
use mercurial_types::NULL_HASH;
use mononoke_macros::mononoke;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::MPathElement;
use mononoke_types::SortedVectorTrieMap;
use mononoke_types::path::MPath;
use smallvec::SmallVec;

use crate::derive_hg_augmented_manifest::AugmentedOutput;
use crate::derive_hg_from_augmented::new_hg_trees;
use crate::derive_hg_from_augmented::store_hg_tree;
use crate::derive_hg_from_augmented::store_hg_trees;
use crate::derive_hg_from_augmented::store_new_hg_trees;
use crate::derive_hg_manifest::HgManifestEntry;
use crate::indexed_augmented_manifest::IndexedAugmentedEntry;
use crate::indexed_augmented_manifest::IndexedAugmentedTrieMap;
use crate::mapping::RootHgAugmentedManifestId;
use crate::pipeline::HgStageOutput;
use crate::pipeline::wrap_entry_traced;

/// Derive a manifest stage for a batch: each commit's output is the Hg form
/// of its augmented output for the stage, and the new Hg trees the stage owns
/// are stored.
pub(crate) async fn derive_stage(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    bonsais: Vec<BonsaiChangeset>,
    payload: &DerivationStagePayload,
    parents: HashMap<ChangesetId, HgStageOutput>,
) -> Result<HashMap<ChangesetId, HgStageOutput>> {
    let stage = payload.stage_id();
    let mut augmented = RootHgAugmentedManifestId::fetch_stage_outputs(
        ctx,
        derivation,
        &stage,
        bonsais
            .iter()
            .map(BonsaiChangeset::get_changeset_id)
            .collect(),
    )
    .await?;
    derive_manifest_batch(
        bonsais,
        parents,
        HashMap::new(),
        |bonsai, parent_outputs, _dependencies| {
            let csid = bonsai.get_changeset_id();
            let output = augmented.remove(&csid);
            let stage = &stage;
            async move {
                let output = output.ok_or_else(|| {
                    anyhow!(
                        "missing augmented manifest stage output for {csid} at {stage:?}: Hg and \
                         augmented manifests must use the same pipeline stage layout"
                    )
                })?;
                derive_commit_stage(ctx, derivation, payload, &bonsai, output, parent_outputs).await
            }
        },
    )
    .await
}

async fn derive_commit_stage(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    payload: &DerivationStagePayload,
    bonsai: &BonsaiChangeset,
    output: AugmentedOutput,
    parents: Vec<HgStageOutput>,
) -> Result<HgStageOutput> {
    let parents = match unchanged_partition(payload, bonsai, parents) {
        Ok(reused) => return Ok(reused),
        Err(parents) => parents,
    };
    match (payload, output) {
        (DerivationStagePayload::Manifest(stage), ManifestStageOutput::Entry(entry)) => {
            if let Some(Entry::Tree(dir)) = &entry {
                let tree = HgAugmentedManifestId::new(dir.node.treenode);
                let parent_trees = parents
                    .iter()
                    .filter_map(HgStageOutput::root_manifest)
                    .map(HgAugmentedManifestId::from)
                    .collect::<Vec<_>>();
                store_stage_trees(
                    ctx,
                    derivation,
                    &stage.path,
                    &stage.deps,
                    tree,
                    parent_trees,
                )
                .await?;
            }
            Ok(HgStageOutput::Manifest(ManifestStageOutput::Entry(
                entry.map(hg_entry),
            )))
        }
        (DerivationStagePayload::Subentries { path, .. }, ManifestStageOutput::Subentries(map)) => {
            let children = load_children(ctx, derivation, map).await?;
            store_partition_trees(ctx, derivation, path, &children, parents).await?;
            Ok(hg_subentries(children))
        }
        (payload, _) => bail!(
            "augmented manifest output does not match stage {:?}",
            payload.stage_id()
        ),
    }
}

/// The parent's output for a partition whose children the commit leaves as
/// they are, as derivation from Hg manifests reuses it: the commit has one
/// parent and no file change at or above the partition's directory or under
/// its children. Otherwise the parents are handed back.
fn unchanged_partition(
    payload: &DerivationStagePayload,
    bonsai: &BonsaiChangeset,
    parents: Vec<HgStageOutput>,
) -> Result<HgStageOutput, Vec<HgStageOutput>> {
    let DerivationStagePayload::Subentries { path, range } = payload else {
        return Err(parents);
    };
    let [parent] = <[HgStageOutput; 1]>::try_from(parents)?;
    let changed = bonsai.file_changes().any(|(change_path, _)| {
        change_path.is_prefix_of(path) || range.contains_path(path, change_path)
    });
    if changed {
        Err(vec![parent])
    } else {
        Ok(parent)
    }
}

/// Store the new Hg trees of a manifest stage at `path`: with child stages,
/// the trees under `path` except those under the child stages; with
/// partitions, only the tree at `path`.
async fn store_stage_trees(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    path: &MPath,
    deps: &ManifestStageDependencies,
    tree: HgAugmentedManifestId,
    parent_trees: Vec<HgAugmentedManifestId>,
) -> Result<()> {
    let blobstore = derivation.blobstore();
    let restricted_paths = &derivation.restricted_paths();
    match deps {
        ManifestStageDependencies::Children(children) => {
            let skip = children
                .iter()
                .map(|child| MPath::ROOT.join_element(Some(child)))
                .collect::<HashSet<_>>();
            store_hg_trees(
                ctx,
                blobstore,
                restricted_paths,
                path,
                tree,
                parent_trees,
                Arc::new(skip),
            )
            .await
        }
        // The null tree is the empty tree, which has no Hg tree to store.
        ManifestStageDependencies::Partitions(_)
            if tree.into_nodehash() == NULL_HASH || parent_trees.contains(&tree) =>
        {
            Ok(())
        }
        ManifestStageDependencies::Partitions(_) => {
            store_hg_tree(ctx, blobstore, restricted_paths, path.clone(), tree).await
        }
    }
}

/// Store the new Hg trees under the directories of a partition of `path`'s
/// children.
async fn store_partition_trees(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    path: &MPath,
    children: &[(SmallVec<[u8; 24]>, IndexedAugmentedEntry)],
    parents: Vec<HgStageOutput>,
) -> Result<()> {
    let blobstore = derivation.blobstore();
    let parent_maps = try_join_all(parents.into_iter().map(parent_partition)).await?;
    let new_children = children
        .iter()
        .filter_map(|(name, entry)| match entry {
            Entry::Tree(dir) => Some((name, HgAugmentedManifestId::new(dir.node.treenode))),
            Entry::Leaf(_) => None,
        })
        .filter_map(|(name, tree)| {
            let parents = parent_maps
                .iter()
                .filter_map(|map| map.get(name)?.clone().into_tree())
                .map(|tree| HgAugmentedManifestId::from(*tree.untraced()))
                .collect::<Vec<_>>();
            if parents.contains(&tree) {
                return None;
            }
            Some(
                MPathElement::from_smallvec(name.clone())
                    .map(|name| (path.join_element(Some(&name)), tree, parents)),
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let trees = stream::iter(new_children)
        .map(|(path, tree, parents)| {
            let trees = new_hg_trees(
                ctx,
                blobstore,
                path,
                tree,
                parents,
                Arc::new(HashSet::new()),
            );
            // Each child's directories are listed in a task of their own;
            // writes stay capped by `store_new_hg_trees`.
            async move { mononoke::spawn_task(trees.try_collect::<Vec<_>>()).await? }
        })
        .buffer_unordered(10)
        .map_ok(|trees| stream::iter(trees.into_iter().map(Ok)))
        .try_flatten();
    store_new_hg_trees(ctx, blobstore, &derivation.restricted_paths(), trees).await
}

/// A parent's children in the same partition.
async fn parent_partition(parent: HgStageOutput) -> Result<SortedVectorTrieMap<HgManifestEntry>> {
    let HgStageOutput::Manifest(ManifestStageOutput::Subentries(map)) = parent else {
        bail!("expected the parent's output for a partition stage to be a partition");
    };
    map.load().await
}

/// The Hg output of the stage `stage` of the commit whose root Hg tree is
/// `root`, taken from the commit's augmented manifest.
pub(crate) async fn extract_stage(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    root: HgManifestId,
    stage: &StageId,
) -> Result<HgStageOutput> {
    let augmented = RootHgAugmentedManifestId::new(HgAugmentedManifestId::from(root));
    let output = RootHgAugmentedManifestId::extract_stage_output_from_derived(
        ctx, derivation, &augmented, stage,
    )
    .await
    .with_context(|| format!("failed to read the augmented manifest of Hg tree {root}"))?;
    Ok(match output {
        ManifestStageOutput::Entry(entry) => {
            HgStageOutput::Manifest(ManifestStageOutput::Entry(entry.map(hg_entry)))
        }
        ManifestStageOutput::Subentries(map) => {
            hg_subentries(load_children(ctx, derivation, map).await?)
        }
    })
}

async fn load_children(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    map: PartitionOutput<IndexedAugmentedTrieMap>,
) -> Result<Vec<(SmallVec<[u8; 24]>, IndexedAugmentedEntry)>> {
    map.load()
        .await?
        .into_stream(ctx, derivation.blobstore())
        .await?
        .try_collect()
        .await
}

fn hg_subentries(children: Vec<(SmallVec<[u8; 24]>, IndexedAugmentedEntry)>) -> HgStageOutput {
    HgStageOutput::Manifest(ManifestStageOutput::Subentries(
        SortedVectorTrieMap::new(
            children
                .into_iter()
                .map(|(name, entry)| (name, hg_entry(entry)))
                .collect(),
        )
        .into(),
    ))
}

/// The Hg entry an augmented entry describes, tagged as generated.
fn hg_entry(entry: IndexedAugmentedEntry) -> HgManifestEntry {
    wrap_entry_traced(
        entry
            .map_tree(|dir| HgManifestId::new(dir.node.treenode))
            .map_leaf(|file| file.node.hg_leaf()),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use derived_data_manager::ChildNameRange;
    use derived_data_manager::StageInputs;
    use derived_data_manager::manifest_partitions::decode_partition_reference;
    use derived_data_manager::manifest_partitions::store_sorted_subentries;
    use mercurial_types::HgNodeHash;
    use mononoke_types::BlobstoreBytes;
    use mononoke_types::BonsaiChangesetMut;
    use mononoke_types::ContentId;
    use mononoke_types::FileChange;
    use mononoke_types::FileType;
    use mononoke_types::GitLfs;
    use mononoke_types::NonRootMPath;
    use mononoke_types::hash::Blake2;
    use mononoke_types::sha1_hash::Sha1;
    use repo_blobstore::RepoBlobstore;
    use repo_blobstore::RepoBlobstoreRef;

    use super::*;

    #[facet::container]
    struct TestRepo(RepoBlobstore);

    fn commit(parents: usize, changed: &[&str]) -> Result<BonsaiChangeset> {
        BonsaiChangesetMut {
            parents: (0..parents)
                .map(|index| ChangesetId::new(Blake2::from_byte_array([index as u8; 32])))
                .collect(),
            file_changes: changed
                .iter()
                .map(|path| {
                    Ok((
                        NonRootMPath::new(path)?,
                        FileChange::tracked(
                            ContentId::new(Blake2::from_byte_array([9; 32])),
                            FileType::Regular,
                            1,
                            None,
                            GitLfs::FullContent,
                        ),
                    ))
                })
                .collect::<Result<_>>()?,
            ..Default::default()
        }
        .freeze()
    }

    #[mononoke::fbinit_test]
    async fn unchanged_partition_reuses_parent_without_enumerating_it(
        fb: fbinit::FacebookInit,
    ) -> Result<()> {
        // Given: a parent's partition of `dir`'s children from `a` to `m`,
        // stored as derivation from Hg manifests stores it, and read back so
        // that enumerating its children would decode them.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let store = repo.repo_blobstore();
        let tree = HgManifestId::new(HgNodeHash::new(Sha1::from_byte_array([1; 20])));
        let map = SortedVectorTrieMap::new(
            [(
                b"alpha".as_slice().into(),
                wrap_entry_traced(Entry::Tree(tree)),
            )]
            .into_iter()
            .collect(),
        );
        let reference = store_sorted_subentries(&ctx, store, map.into(), |_| {
            Ok(BlobstoreBytes::from_bytes(b"entry".as_slice()))
        })
        .await?;
        let decodes = Arc::new(AtomicUsize::new(0));
        let parent = || {
            let decodes = decodes.clone();
            decode_partition_reference(
                &ctx,
                store.clone(),
                Arc::new(StageInputs::default()),
                reference.clone(),
                move |_| {
                    decodes.fetch_add(1, Ordering::Relaxed);
                    Ok(wrap_entry_traced(Entry::Tree(tree)))
                },
            )
            .map(|map| HgStageOutput::Manifest(ManifestStageOutput::Subentries(map)))
        };
        let payload = DerivationStagePayload::Subentries {
            path: MPath::new("dir")?,
            range: ChildNameRange::new(b'a', b'm')?,
        };

        // When: a one-parent commit that changes only files outside the
        // partition derives it.
        let reused = unchanged_partition(
            &payload,
            &commit(1, &["elsewhere", "dir/zebra", "dir2/apple"])?,
            vec![parent()?],
        );

        // Then: the parent's output is reused as it is, without enumerating
        // or encoding its children.
        let Ok(HgStageOutput::Manifest(ManifestStageOutput::Subentries(reused))) = reused else {
            anyhow::bail!("an unchanged partition should reuse the parent's output");
        };
        assert_eq!(
            store_sorted_subentries(&ctx, store, reused, |_| {
                panic!("a reused partition should not be encoded again")
            })
            .await?,
            reference
        );
        assert_eq!(decodes.load(Ordering::Relaxed), 0);

        // And: a change under one of its children, a change replacing an
        // ancestor directory, or a second parent rebuilds it.
        for (parents, changed) in [(1, vec!["dir/apple/x"]), (1, vec!["dir"]), (2, vec![])] {
            let parent_outputs = (0..parents).map(|_| parent()).collect::<Result<_>>()?;
            assert!(
                unchanged_partition(&payload, &commit(parents, &changed)?, parent_outputs).is_err(),
                "{parents} parents and changes {changed:?} should rebuild the partition"
            );
        }
        Ok(())
    }
}
