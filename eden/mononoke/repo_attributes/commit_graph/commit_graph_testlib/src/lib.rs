/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![feature(iter_array_chunks)]

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Result;
use cloned::cloned;
use commit_graph::BaseCommitGraphWriter;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphWriter;
use commit_graph_types::edges::Parents;
use commit_graph_types::storage::CommitGraphStorage;
use commit_graph_types::storage::Prefetch;
use context::CoreContext;
use context::PerfCounterType;
use futures::FutureExt;
use futures::stream::TryStreamExt;
use in_memory_commit_graph_storage::InMemoryCommitGraphStorage;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use maplit::hashmap;
use maplit::hashset;
use mononoke_types::ChangesetIdPrefix;
use mononoke_types::ChangesetIdsResolvedFromPrefix;
use mononoke_types::Generation;
use smallvec::smallvec;
use vec1::vec1;

use crate::utils::*;

pub mod counting_writes_storage;
pub mod lagging_replica_storage;
pub mod shuffling_storage;
#[cfg(test)]
pub mod tests;
pub mod utils;

pub trait CommitGraphStorageTest: CommitGraphStorage {
    fn flush(&self) {}
}

impl CommitGraphStorageTest for InMemoryCommitGraphStorage {}

#[macro_export]
macro_rules! impl_commit_graph_tests {
    ( $test_runner:ident ) => {
        $crate::impl_commit_graph_tests_internal!(
            $test_runner,
            test_storage_store_and_fetch,
            test_is_ancestor_exact_prefetching,
            test_is_ancestor_skew_ancestors_prefetching,
            test_filter_ancestors,
            test_is_linear_stack,
            test_is_linear_stack_merge_descendant,
            test_skip_tree,
            test_p1_linear_tree,
            test_parents_and_subtree_sources_tree,
            test_ancestors_difference,
            test_ancestors_difference_many_stacks,
            test_lower_frontier_batch_prefetch,
            test_ancestors_difference_segment_slices,
            test_find_by_prefix,
            test_add_recursive,
            test_add_recursive_many_changesets,
            test_add_many_changesets,
            test_ancestors_frontier_with,
            test_range_stream,
            test_common_base,
            test_slice_ancestors,
            test_find_boundary,
            test_segmented_slice_ancestors,
            test_children,
            test_descendants,
            test_ancestors_difference_segments_1,
            test_ancestors_difference_segments_2,
            test_ancestors_difference_segments_3,
            test_locations_to_changeset_ids,
            test_changeset_ids_to_locations,
            test_process_topologically,
            test_minimize_frontier,
            test_ancestors_within_distance,
            test_linear_ancestors_stream,
        );
    };
}

#[macro_export]
macro_rules! impl_commit_graph_tests_internal {
    ( $test_runner:ident, $($test_name:ident, )* ) => {
        $(
            #[mononoke::fbinit_test]
            pub async fn $test_name(fb: FacebookInit) -> Result<()> {
                $test_runner(fb, $crate::$test_name).await
            }
        )*
    }
}

pub async fn test_storage_store_and_fetch(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
             A-B-C-D-G-H-I
              \     /
               E---F
         ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    // Check the public API.
    assert!(graph.exists(&ctx, name_cs_id("A")).await?);

    assert!(!graph.exists(&ctx, name_cs_id("nonexistent")).await?);
    assert_eq!(
        graph
            .known_changesets(
                &ctx,
                vec![name_cs_id("A"), name_cs_id("B"), name_cs_id("nonexistent")]
            )
            .await?
            .into_iter()
            .collect::<HashSet<_>>(),
        hashset! {name_cs_id("A"), name_cs_id("B")}
    );
    assert_eq!(
        graph
            .changeset_generation(&ctx, name_cs_id("G"))
            .await?
            .value(),
        5
    );
    assert_eq!(
        graph
            .many_changeset_generations(
                &ctx,
                &[
                    name_cs_id("A"),
                    name_cs_id("C"),
                    name_cs_id("F"),
                    name_cs_id("G")
                ]
            )
            .await?,
        hashmap! {
            name_cs_id("A") => Generation::new(1),
            name_cs_id("C") => Generation::new(3),
            name_cs_id("F") => Generation::new(3),
            name_cs_id("G") => Generation::new(5),
        }
    );
    assert_eq!(
        graph.changeset_linear_depth(&ctx, name_cs_id("G")).await?,
        4
    );
    assert_eq!(
        graph
            .many_changeset_linear_depths(
                &ctx,
                &[
                    name_cs_id("A"),
                    name_cs_id("C"),
                    name_cs_id("F"),
                    name_cs_id("G")
                ]
            )
            .await?,
        hashmap! {
            name_cs_id("A") => 0,
            name_cs_id("C") => 2,
            name_cs_id("F") => 2,
            name_cs_id("G") => 4,
        }
    );
    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("A"))
            .await?
            .as_slice(),
        &[]
    );
    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("E"))
            .await?
            .as_slice(),
        &[name_cs_id("A")]
    );
    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("G"))
            .await?
            .as_slice(),
        &[name_cs_id("D"), name_cs_id("F")]
    );
    assert_eq!(
        graph
            .many_changeset_parents(&ctx, &[name_cs_id("A"), name_cs_id("E"), name_cs_id("G")])
            .await?,
        hashmap! {
            name_cs_id("A") => smallvec![],
            name_cs_id("E") => smallvec![name_cs_id("A")],
            name_cs_id("G") => smallvec![name_cs_id("D"), name_cs_id("F")],
        },
    );

    // Check some underlying storage details.
    assert_eq!(
        storage
            .maybe_fetch_edges(&ctx, name_cs_id("A"))
            .await?
            .unwrap()
            .merge_ancestor_or_root::<Parents>(),
        None
    );
    assert_eq!(
        storage
            .maybe_fetch_edges(&ctx, name_cs_id("C"))
            .await?
            .unwrap()
            .merge_ancestor_or_root::<Parents>(),
        Some(&name_cs_node("A", 1, 0, 0))
    );
    assert_eq!(
        storage
            .maybe_fetch_edges(&ctx, name_cs_id("I"))
            .await?
            .unwrap()
            .merge_ancestor_or_root::<Parents>(),
        Some(&name_cs_node("G", 5, 1, 4))
    );

    // fetch_many_edges and maybe_fetch_many_edges return the same result if none of the changesets
    // are missing.
    assert_eq!(
        storage
            .fetch_many_edges(
                &ctx,
                &[name_cs_id("A"), name_cs_id("C"), name_cs_id("I")],
                Prefetch::None
            )
            .await?,
        storage
            .maybe_fetch_many_edges(
                &ctx,
                &[name_cs_id("A"), name_cs_id("C"), name_cs_id("I")],
                Prefetch::None
            )
            .await?,
    );

    // fetch_many_edges returns an error if any of the changesets are missing.
    assert!(
        storage
            .fetch_many_edges(
                &ctx,
                &[name_cs_id("Z"), name_cs_id("A"), name_cs_id("B")],
                Prefetch::None
            )
            .await
            .is_err()
    );

    // maybe_fetch_many_edges ignores missing changesets ("Z" in this case).
    assert_eq!(
        storage
            .maybe_fetch_many_edges(
                &ctx,
                &[
                    name_cs_id("Z"),
                    name_cs_id("A"),
                    name_cs_id("C"),
                    name_cs_id("I")
                ],
                Prefetch::None
            )
            .await?
            .into_keys()
            .collect::<HashSet<_>>(),
        hashset! {name_cs_id("A"), name_cs_id("C"), name_cs_id("I")},
    );

    Ok(())
}

pub async fn test_is_ancestor_exact_prefetching(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    test_is_ancestor_impl(ctx, storage).await
}

pub async fn test_is_ancestor_skew_ancestors_prefetching(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    test_is_ancestor_impl(ctx, storage).await
}

async fn test_is_ancestor_impl(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
             A-B-C-D-G-H-I
              \     /
               E---F
         ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert!(
        graph
            .is_ancestor(&ctx, name_cs_id("C"), name_cs_id("C"))
            .await?
    );
    assert!(
        graph
            .is_ancestor(&ctx, name_cs_id("A"), name_cs_id("H"))
            .await?
    );
    assert!(
        graph
            .is_ancestor(&ctx, name_cs_id("A"), name_cs_id("F"))
            .await?
    );
    assert!(
        graph
            .is_ancestor(&ctx, name_cs_id("F"), name_cs_id("I"))
            .await?
    );
    assert!(
        graph
            .is_ancestor(&ctx, name_cs_id("C"), name_cs_id("I"))
            .await?
    );
    assert!(
        !graph
            .is_ancestor(&ctx, name_cs_id("I"), name_cs_id("A"))
            .await?
    );
    assert!(
        !graph
            .is_ancestor(&ctx, name_cs_id("E"), name_cs_id("D"))
            .await?
    );
    assert!(
        !graph
            .is_ancestor(&ctx, name_cs_id("B"), name_cs_id("E"))
            .await?
    );

    assert!(
        graph
            .is_ancestor_of_any(
                &ctx,
                name_cs_id("C"),
                vec![name_cs_id("D"), name_cs_id("F")]
            )
            .await?
    );
    assert!(
        graph
            .is_ancestor_of_any(&ctx, name_cs_id("C"), vec![name_cs_id("D")])
            .await?
    );
    assert!(
        graph
            .is_ancestor_of_any(
                &ctx,
                name_cs_id("C"),
                vec![name_cs_id("G"), name_cs_id("I")]
            )
            .await?
    );
    assert!(
        !graph
            .is_ancestor_of_any(
                &ctx,
                name_cs_id("C"),
                vec![name_cs_id("B"), name_cs_id("F")]
            )
            .await?
    );
    assert!(
        graph
            .is_ancestor_of_any(
                &ctx,
                name_cs_id("A"),
                vec![
                    name_cs_id("B"),
                    name_cs_id("C"),
                    name_cs_id("D"),
                    name_cs_id("E"),
                    name_cs_id("F"),
                    name_cs_id("G"),
                    name_cs_id("H"),
                    name_cs_id("I")
                ]
            )
            .await?
    );
    assert!(
        !graph
            .is_ancestor_of_any(
                &ctx,
                name_cs_id("I"),
                vec![name_cs_id("G"), name_cs_id("H")]
            )
            .await?
    );

    Ok(())
}

pub async fn test_filter_ancestors(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
             A-B-C-D-G-H-I
              \     /
               E---F
         ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    let all_names = vec!["A", "B", "C", "D", "E", "F", "G", "H", "I"];

    // Test 1: All of A,B,C,D,E,F,G,H should be ancestors of I
    let candidates: Vec<_> = vec!["A", "B", "C", "D", "E", "F", "G", "H"]
        .into_iter()
        .map(name_cs_id)
        .collect();
    let result = graph
        .filter_ancestors(&ctx, name_cs_id("I"), candidates.clone())
        .await?;
    assert_eq!(result.first(), Some(&name_cs_id("H")));
    let result: HashSet<_> = result.into_iter().collect();
    let expected: HashSet<_> = candidates.into_iter().collect();
    assert_eq!(result, expected);

    // Test 2: Only A,B,C should be ancestors of D (not E,F)
    let candidates: Vec<_> = vec!["A", "B", "C", "E", "F"]
        .into_iter()
        .map(name_cs_id)
        .collect();
    let result: HashSet<_> = graph
        .filter_ancestors(&ctx, name_cs_id("D"), candidates)
        .await?
        .into_iter()
        .collect();
    let expected: HashSet<_> = vec!["A", "B", "C"].into_iter().map(name_cs_id).collect();
    assert_eq!(result, expected);

    // Test 3: Empty candidates -> empty result
    let result = graph
        .filter_ancestors(&ctx, name_cs_id("I"), vec![])
        .await?;
    assert!(result.is_empty());

    // Test 4: Self-ancestry — D is ancestor of D
    let result: HashSet<_> = graph
        .filter_ancestors(&ctx, name_cs_id("D"), vec![name_cs_id("D")])
        .await?
        .into_iter()
        .collect();
    assert_eq!(result, hashset! { name_cs_id("D") });

    // Test 5: No candidates are ancestors of A (I and H are not)
    let result = graph
        .filter_ancestors(
            &ctx,
            name_cs_id("A"),
            vec![name_cs_id("I"), name_cs_id("H")],
        )
        .await?;
    assert!(result.is_empty());

    // Test 6: Verify equivalence with individual is_ancestor calls for all commits against G
    let descendant = name_cs_id("G");
    let candidates: Vec<_> = all_names.iter().map(|n| name_cs_id(n)).collect();
    let batch_result: HashSet<_> = graph
        .filter_ancestors(&ctx, descendant, candidates.clone())
        .await?
        .into_iter()
        .collect();
    let mut individual_result: HashSet<_> = HashSet::new();
    for candidate in &candidates {
        if graph.is_ancestor(&ctx, *candidate, descendant).await? {
            individual_result.insert(*candidate);
        }
    }
    assert_eq!(batch_result, individual_result);

    Ok(())
}

pub async fn test_is_linear_stack(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
             A-B-C-D-G-H-I
              \     /
               E---F
         ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert!(
        graph
            .is_linear_stack(&ctx, name_cs_id("C"), name_cs_id("C"))
            .await?
    );
    assert!(
        !graph
            .is_linear_stack(&ctx, name_cs_id("G"), name_cs_id("I"))
            .await?
    );
    assert!(
        graph
            .is_linear_stack(&ctx, name_cs_id("H"), name_cs_id("I"))
            .await?
    );
    assert!(
        !graph
            .is_linear_stack(&ctx, name_cs_id("A"), name_cs_id("H"))
            .await?
    );
    assert!(
        graph
            .is_linear_stack(&ctx, name_cs_id("A"), name_cs_id("F"))
            .await?
    );
    assert!(
        graph
            .is_linear_stack(&ctx, name_cs_id("B"), name_cs_id("D"))
            .await?
    );
    assert!(
        !graph
            .is_linear_stack(&ctx, name_cs_id("F"), name_cs_id("I"))
            .await?
    );
    assert!(
        !graph
            .is_linear_stack(&ctx, name_cs_id("C"), name_cs_id("I"))
            .await?
    );
    assert!(
        !graph
            .is_linear_stack(&ctx, name_cs_id("I"), name_cs_id("A"))
            .await?
    );
    assert!(
        !graph
            .is_linear_stack(&ctx, name_cs_id("E"), name_cs_id("D"))
            .await?
    );
    assert!(
        !graph
            .is_linear_stack(&ctx, name_cs_id("B"), name_cs_id("E"))
            .await?
    );

    Ok(())
}

/// A merge commit is never the top of a linear stack: the stack includes `descendant`,
/// and a linear stack has no changeset with more than one parent.
pub async fn test_is_linear_stack_merge_descendant(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
             A-B-C-D-G-H-I
              \     /
               E---F
         ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    // G is a merge of D and F, so no stack whose top is G is linear.
    assert!(
        !graph
            .is_linear_stack(&ctx, name_cs_id("D"), name_cs_id("G"))
            .await?
    );
    assert!(
        !graph
            .is_linear_stack(&ctx, name_cs_id("A"), name_cs_id("G"))
            .await?
    );
    assert!(
        !graph
            .is_linear_stack(&ctx, name_cs_id("F"), name_cs_id("G"))
            .await?
    );
    assert!(
        !graph
            .is_linear_stack(&ctx, name_cs_id("G"), name_cs_id("G"))
            .await?
    );

    Ok(())
}

pub async fn test_skip_tree(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
         A-B-C-D-G-H---J-K
            \   /   \ /
             E-F     I

         L-M-N-O-P-Q-R-S-T-U
         ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_eq!(
        storage
            .maybe_fetch_edges(&ctx, name_cs_id("K"))
            .await?
            .unwrap()
            .node()
            .cs_id,
        name_cs_id("K")
    );

    assert_skip_tree_parent(&storage, &ctx, "G", "B").await?;
    assert_skip_tree_parent(&storage, &ctx, "K", "J").await?;
    assert_skip_tree_parent(&storage, &ctx, "J", "H").await?;
    assert_skip_tree_parent(&storage, &ctx, "H", "G").await?;

    assert_skip_tree_skew_ancestor(&storage, &ctx, "H", "A").await?;
    assert_skip_tree_skew_ancestor(&storage, &ctx, "K", "J").await?;
    assert_skip_tree_skew_ancestor(&storage, &ctx, "U", "T").await?;
    assert_skip_tree_skew_ancestor(&storage, &ctx, "T", "S").await?;
    assert_skip_tree_skew_ancestor(&storage, &ctx, "S", "L").await?;

    assert_skip_tree_level_ancestor(&graph, &ctx, "S", 4, Some("P")).await?;
    assert_skip_tree_level_ancestor(&graph, &ctx, "U", 7, Some("S")).await?;
    assert_skip_tree_level_ancestor(&graph, &ctx, "T", 7, Some("S")).await?;
    assert_skip_tree_level_ancestor(&graph, &ctx, "O", 2, Some("N")).await?;
    assert_skip_tree_level_ancestor(&graph, &ctx, "N", 3, None).await?;
    assert_skip_tree_level_ancestor(&graph, &ctx, "K", 2, Some("G")).await?;

    assert_skip_tree_lowest_common_ancestor(&graph, &ctx, "D", "F", Some("B")).await?;
    assert_skip_tree_lowest_common_ancestor(&graph, &ctx, "K", "I", Some("H")).await?;
    assert_skip_tree_lowest_common_ancestor(&graph, &ctx, "D", "C", Some("C")).await?;
    assert_skip_tree_lowest_common_ancestor(&graph, &ctx, "N", "K", None).await?;
    assert_skip_tree_lowest_common_ancestor(&graph, &ctx, "A", "I", Some("A")).await?;

    Ok(())
}

pub async fn test_p1_linear_tree(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
         K         V
         |         |
         J         U
         |         |
         I         T
         |\        |
         | \       S
         F  |      |
         |\ |  L   R
         | \|  |   |
         E  H /    Q
         |  |/     |
         D  G      P
         |  |      |
         C /       O
         |/        |
         B         N
         |         |
         A         M
         ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_p1_linear_skew_ancestor(&storage, &ctx, "A", None).await?;
    assert_p1_linear_skew_ancestor(&storage, &ctx, "B", Some("A")).await?;
    assert_p1_linear_skew_ancestor(&storage, &ctx, "C", Some("B")).await?;
    assert_p1_linear_skew_ancestor(&storage, &ctx, "D", Some("A")).await?;
    assert_p1_linear_skew_ancestor(&storage, &ctx, "E", Some("D")).await?;
    assert_p1_linear_skew_ancestor(&storage, &ctx, "F", Some("E")).await?;
    assert_p1_linear_skew_ancestor(&storage, &ctx, "G", Some("B")).await?;
    assert_p1_linear_skew_ancestor(&storage, &ctx, "H", Some("A")).await?;
    assert_p1_linear_skew_ancestor(&storage, &ctx, "I", Some("D")).await?;
    assert_p1_linear_skew_ancestor(&storage, &ctx, "J", Some("A")).await?;
    assert_p1_linear_skew_ancestor(&storage, &ctx, "K", Some("J")).await?;

    assert_p1_linear_level_ancestor(&graph, &ctx, "S", 3, Some("P")).await?;
    assert_p1_linear_level_ancestor(&graph, &ctx, "U", 6, Some("S")).await?;
    assert_p1_linear_level_ancestor(&graph, &ctx, "T", 6, Some("S")).await?;
    assert_p1_linear_level_ancestor(&graph, &ctx, "O", 1, Some("N")).await?;
    assert_p1_linear_level_ancestor(&graph, &ctx, "N", 2, None).await?;
    assert_p1_linear_level_ancestor(&graph, &ctx, "K", 1, Some("B")).await?;
    assert_p1_linear_level_ancestor(&graph, &ctx, "H", 2, Some("G")).await?;
    assert_p1_linear_level_ancestor(&graph, &ctx, "J", 3, Some("D")).await?;

    assert_p1_linear_lowest_common_ancestor(&graph, &ctx, "F", "D", Some("D")).await?;
    assert_p1_linear_lowest_common_ancestor(&graph, &ctx, "E", "H", Some("B")).await?;
    assert_p1_linear_lowest_common_ancestor(&graph, &ctx, "K", "I", Some("I")).await?;
    assert_p1_linear_lowest_common_ancestor(&graph, &ctx, "I", "H", Some("B")).await?;
    assert_p1_linear_lowest_common_ancestor(&graph, &ctx, "L", "H", Some("G")).await?;
    assert_p1_linear_lowest_common_ancestor(&graph, &ctx, "L", "F", Some("B")).await?;
    assert_p1_linear_lowest_common_ancestor(&graph, &ctx, "F", "R", None).await?;

    Ok(())
}

pub async fn test_parents_and_subtree_sources_tree(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag_with_subtree_sources(
        &ctx,
        r"
          K         V
          |         |
          J         U
          |         |
          I         T
          |\        |
          | \       S
          F  |      |
          |\ |  L   R
          | \|  |   |
          E  H /    Q
          |  |/     |
          D  G      P
          |  |      |
          C /       O
          |/        |
          B         N
          |         |
          A         M
          ",
        maplit::btreemap! {
            "G".to_string() => vec!["Q".to_string()],
            "I".to_string() => vec!["G".to_string()],
            "K".to_string() => vec!["P".to_string(), "L".to_string()],
            "V".to_string() => vec!["L".to_string()],
        },
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_parents_and_subtree_sources_skew_ancestor(&storage, &ctx, "A", None).await?;
    assert_parents_and_subtree_sources_skew_ancestor(&storage, &ctx, "B", Some("A")).await?;
    assert_parents_and_subtree_sources_skew_ancestor(&storage, &ctx, "C", Some("B")).await?;
    assert_parents_and_subtree_sources_skew_ancestor(&storage, &ctx, "D", Some("A")).await?;
    assert_parents_and_subtree_sources_skew_ancestor(&storage, &ctx, "E", Some("D")).await?;
    assert_parents_and_subtree_sources_skew_ancestor(&storage, &ctx, "F", None).await?;
    assert_parents_and_subtree_sources_skew_ancestor(&storage, &ctx, "G", None).await?;
    assert_parents_and_subtree_sources_skew_ancestor(&storage, &ctx, "H", Some("G")).await?;
    assert_parents_and_subtree_sources_skew_ancestor(&storage, &ctx, "I", None).await?;
    assert_parents_and_subtree_sources_skew_ancestor(&storage, &ctx, "J", Some("I")).await?;
    assert_parents_and_subtree_sources_skew_ancestor(&storage, &ctx, "K", None).await?;

    assert_parents_and_subtree_sources_level_ancestor(&graph, &ctx, "S", 3, Some("P")).await?;
    assert_parents_and_subtree_sources_level_ancestor(&graph, &ctx, "U", 6, Some("S")).await?;
    assert_parents_and_subtree_sources_level_ancestor(&graph, &ctx, "T", 6, Some("S")).await?;
    assert_parents_and_subtree_sources_level_ancestor(&graph, &ctx, "O", 1, Some("N")).await?;
    assert_parents_and_subtree_sources_level_ancestor(&graph, &ctx, "N", 2, None).await?;
    assert_parents_and_subtree_sources_level_ancestor(&graph, &ctx, "K", 1, None).await?;
    assert_parents_and_subtree_sources_level_ancestor(&graph, &ctx, "F", 2, None).await?;
    assert_parents_and_subtree_sources_level_ancestor(&graph, &ctx, "I", 2, None).await?;

    assert_parents_and_subtree_sources_common_base(&graph, &ctx, "F", "D", vec!["D"]).await?;
    assert_parents_and_subtree_sources_common_base(&graph, &ctx, "E", "H", vec!["B"]).await?;
    assert_parents_and_subtree_sources_common_base(&graph, &ctx, "K", "I", vec!["I"]).await?;
    assert_parents_and_subtree_sources_common_base(&graph, &ctx, "I", "G", vec!["G"]).await?;
    assert_parents_and_subtree_sources_common_base(&graph, &ctx, "L", "H", vec!["G"]).await?;
    assert_parents_and_subtree_sources_common_base(&graph, &ctx, "L", "F", vec!["G"]).await?;
    assert_parents_and_subtree_sources_common_base(&graph, &ctx, "F", "R", vec!["Q"]).await?;
    assert_parents_and_subtree_sources_common_base(&graph, &ctx, "V", "K", vec!["L"]).await?;
    assert_parents_and_subtree_sources_common_base(&graph, &ctx, "K", "P", vec!["P"]).await?;
    assert_parents_and_subtree_sources_common_base(&graph, &ctx, "V", "J", vec!["G"]).await?;
    assert_parents_and_subtree_sources_common_base(&graph, &ctx, "F", "U", vec!["Q"]).await?;

    // Test ancestors_within_distance on the parents-and-subtree-sources graph.
    // G has parent B and subtree source Q. In the parents-and-subtree-sources
    // view, both are treated as parents, so ancestors_within_distance([G], 1)
    // should include G (distance 0), B (distance 1), and Q (distance 1).
    let ancestors_within_distance: HashSet<_> = graph
        .parents_and_subtree_sources_graph()
        .ancestors_within_distance_stream(&ctx, vec![name_cs_id("G")], 1)
        .await?
        .try_collect()
        .await?;
    assert_eq!(
        ancestors_within_distance,
        hashset! {
            (name_cs_id("G"), 0),
            (name_cs_id("B"), 1),
            (name_cs_id("Q"), 1),
        },
    );

    Ok(())
}

/// Heads on several stacks over a common trunk, which is the shape of a pull
/// of draft commits: heads above and below the common generation, a head at
/// the same generation as another stack's interior, and heads that are
/// ancestors of common.  Checked with and without the ancestors stream
/// prefetching the common frontier ahead of time.
pub async fn test_ancestors_difference_many_stacks(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
         A-B-C-D-E-F-G
            \   \   \
             H-I N   K-L-M
         ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    async fn assert_many_stacks(graph: &CommitGraph, ctx: &CoreContext) -> Result<()> {
        assert_ancestors_difference(
            graph,
            ctx,
            vec!["M", "N", "I", "G", "C"],
            vec!["G"],
            vec!["M", "L", "K", "N", "I", "H"],
        )
        .await?;
        assert_ancestors_difference(
            graph,
            ctx,
            vec!["M", "N", "H"],
            vec!["G", "I"],
            vec!["M", "L", "K", "N"],
        )
        .await?;
        assert_ancestors_difference(graph, ctx, vec!["I", "N"], vec!["M"], vec!["N", "I", "H"])
            .await
    }

    assert_many_stacks(&graph, &ctx).await?;
    with_just_knobs_async(
        JustKnobsInMemory::new(hashmap! {
            "scm/mononoke:commit_graph_pull_optimizations".to_string() => KnobVal::Bool(false),
        }),
        assert_many_stacks(&graph, &ctx).boxed(),
    )
    .await
}

pub async fn test_lower_frontier_batch_prefetch(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    // Every commit on the main line merges in an unrelated two-commit
    // branch, so lowering a frontier below each merge adds a changeset
    // above the target.
    let graph = from_dag(
        &ctx,
        r"
                A-B-C-D-E-F
                 / / / / /
              L-G / / / /
               M-H / / /
                N-I / /
                 O-J /
                  P-K
        ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    with_just_knobs_async(
        JustKnobsInMemory::new(hashmap! {
            "scm/mononoke:commit_graph_pull_optimizations".to_string() => KnobVal::Bool(true),
            "scm/mononoke:commit_graph_prefetch_step_limit".to_string() => KnobVal::Int(4),
        }),
        async {
            assert!(
                graph
                    .is_ancestor(&ctx, name_cs_id("A"), name_cs_id("F"))
                    .await?
            );
            // Lowering F to A's generation batches E and K, then D and J,
            // then C and I, then B and H, as each merge adds its branch to
            // the frontier.  Each is batched once however many times the
            // frontier grows afterwards.
            assert_eq!(
                ctx.perf_counters()
                    .get_counter(PerfCounterType::CommitGraphBatchPrefetchScanned),
                8
            );
            Ok(())
        }
        .boxed(),
    )
    .await
}

pub async fn test_ancestors_difference_segment_slices(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
         A-B-C-D-G-H---J-K
            \   /   \ /
             E-F     I

         L-M-N-O-P-Q-R-S-T-U
         ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_ancestors_difference_segment_slices(
        &graph,
        &ctx,
        &["K"],
        &[],
        3,
        &[
            &["A", "B", "C"],
            &["D"],
            &["E", "F"],
            &["G", "H", "I"],
            &["J", "K"],
        ],
    )
    .await?;

    assert_ancestors_difference_segment_slices(
        &graph,
        &ctx,
        &["K", "U"],
        &[],
        3,
        &[
            &["L", "M", "N"],
            &["O", "P", "Q"],
            &["R", "S", "T"],
            &["U"],
            &["A", "B"],
            &["C", "D"],
            &["E"],
            &["F"],
            &["G", "H"],
            &["I"],
            &["J", "K"],
        ],
    )
    .await?;

    // No common ancestors → no external parents.
    assert_ancestors_difference_segment_slices_with_external_parents(
        &graph,
        &ctx,
        &["K"],
        &[],
        3,
        &[
            &["A", "B", "C"],
            &["D"],
            &["E", "F"],
            &["G", "H", "I"],
            &["J", "K"],
        ],
        &[],
    )
    .await?;

    // With common ancestors → external parents are segment parents
    // that fall outside all segments (i.e. in the common set).
    // DAG: G has parents D and F, so with common=["D"], both B
    // (parent of E) and D (parent of G) are external parents.
    assert_ancestors_difference_segment_slices_with_external_parents(
        &graph,
        &ctx,
        &["K"],
        &["D"],
        3,
        &[&["E", "F"], &["G"], &["H", "I"], &["J"], &["K"]],
        &["B", "D"],
    )
    .await?;

    assert_ancestors_difference_segment_slices_with_external_parents(
        &graph,
        &ctx,
        &["K"],
        &["H"],
        3,
        &[&["I"], &["J", "K"]],
        &["H"],
    )
    .await?;

    Ok(())
}

pub async fn test_ancestors_difference(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
         A-B-C-D-G-H---J-K
            \   /   \ /
             E-F     I

         L-M-N-O-P-Q-R-S-T-U
         ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_ancestors_difference(
        &graph,
        &ctx,
        vec!["K"],
        vec![],
        vec!["K", "J", "I", "H", "G", "D", "F", "C", "E", "B", "A"],
    )
    .await?;

    assert_ancestors_difference(
        &graph,
        &ctx,
        vec!["K", "U"],
        vec![],
        vec![
            "U", "T", "S", "R", "Q", "P", "O", "N", "M", "L", "K", "J", "I", "H", "G", "D", "F",
            "C", "E", "B", "A",
        ],
    )
    .await?;

    assert_ancestors_difference(&graph, &ctx, vec!["K"], vec!["G"], vec!["K", "J", "I", "H"])
        .await?;

    assert_ancestors_difference(&graph, &ctx, vec!["K", "I"], vec!["J"], vec!["K"]).await?;

    assert_ancestors_difference(
        &graph,
        &ctx,
        vec!["I"],
        vec!["C"],
        vec!["I", "H", "G", "F", "E", "D"],
    )
    .await?;

    assert_ancestors_difference(
        &graph,
        &ctx,
        vec!["J", "S"],
        vec!["C", "E", "O"],
        vec!["J", "I", "H", "G", "F", "D", "S", "R", "Q", "P"],
    )
    .await?;

    let set1 = ["A", "B", "C", "D", "E", "F", "G", "H", "I"]
        .into_iter()
        .map(name_cs_id)
        .collect::<HashSet<_>>();

    let set1_fn = move |cs_id| {
        cloned!(set1);
        async move { Ok(set1.contains(&cs_id)) }
    };

    assert_ancestors_difference_with(
        &graph,
        &ctx,
        vec!["J", "S"],
        vec!["C", "E", "O"],
        set1_fn.clone(),
        vec!["J", "S", "R", "Q", "P"],
    )
    .await?;

    assert_ancestors_difference_with(
        &graph,
        &ctx,
        vec!["K"],
        vec!["C", "E"],
        set1_fn.clone(),
        vec!["K", "J"],
    )
    .await?;

    let set2 = ["A", "B", "C"]
        .into_iter()
        .map(name_cs_id)
        .collect::<HashSet<_>>();

    let set2_fn = move |cs_id| {
        cloned!(set2);
        async move { Ok(set2.contains(&cs_id)) }
    };

    assert_ancestors_difference_with(
        &graph,
        &ctx,
        vec!["H"],
        vec![],
        set2_fn.clone(),
        vec!["D", "E", "F", "G", "H"],
    )
    .await?;

    assert_ancestors_difference_with(
        &graph,
        &ctx,
        vec!["H"],
        vec!["F"],
        set2_fn.clone(),
        vec!["D", "G", "H"],
    )
    .await?;

    Ok(())
}

pub async fn test_find_by_prefix(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r##"
             J-K-L-LZZ
             M-MA-MAA-MAB-MAC
             M-MB-MBB-MBC
             N-NAA
             O-P-QQ
             a-b-c
         "##,
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_eq!(
        graph
            .find_by_prefix(&ctx, ChangesetIdPrefix::from_bytes("Z")?, 10)
            .await?,
        ChangesetIdsResolvedFromPrefix::NoMatch
    );
    assert_eq!(
        graph
            .find_by_prefix(&ctx, ChangesetIdPrefix::from_bytes("Q")?, 10)
            .await?,
        ChangesetIdsResolvedFromPrefix::Single(name_cs_id("QQ"))
    );
    assert_eq!(
        graph
            .find_by_prefix(&ctx, ChangesetIdPrefix::from_bytes("MA")?, 10)
            .await?,
        ChangesetIdsResolvedFromPrefix::Multiple(vec![
            name_cs_id("MA"),
            name_cs_id("MAA"),
            name_cs_id("MAB"),
            name_cs_id("MAC"),
        ])
    );
    assert_eq!(
        graph
            .find_by_prefix(&ctx, ChangesetIdPrefix::from_bytes("M")?, 6)
            .await?,
        ChangesetIdsResolvedFromPrefix::TooMany(vec![
            name_cs_id("M"),
            name_cs_id("MA"),
            name_cs_id("MAA"),
            name_cs_id("MAB"),
            name_cs_id("MAC"),
            name_cs_id("MB"),
        ])
    );
    // Check prefixes that are not a full byte. `P` is `\x50` in ASCII.
    assert_eq!(
        graph
            .find_by_prefix(&ctx, ChangesetIdPrefix::from_str("5")?, 2)
            .await?,
        ChangesetIdsResolvedFromPrefix::Multiple(vec![name_cs_id("P"), name_cs_id("QQ")])
    );

    Ok(())
}

pub async fn test_add_recursive(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let reference_storage = Arc::new(InMemoryCommitGraphStorage::new(test_repo_identity()));

    let reference_graph = Arc::new(
        from_dag(
            &ctx,
            r"
             A-B-C-D-G-H-I
              \     /
               E---F---J
         ",
            reference_storage,
        )
        .await?,
    );

    let graph = CommitGraph::new(storage.clone());
    let graph_writer = BaseCommitGraphWriter::new(graph.clone());

    assert_eq!(
        graph_writer
            .add_recursive(
                &ctx,
                reference_graph.clone(),
                vec1![(name_cs_id("I"), smallvec![name_cs_id("H")], vec![])],
            )
            .await?,
        9
    );
    assert_eq!(
        graph_writer
            .add_recursive(
                &ctx,
                reference_graph,
                vec1![(name_cs_id("J"), smallvec![name_cs_id("F")], vec![])]
            )
            .await?,
        1
    );
    storage.flush();

    assert!(graph.exists(&ctx, name_cs_id("A")).await?);

    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("E"))
            .await?
            .as_slice(),
        &[name_cs_id("A")]
    );
    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("G"))
            .await?
            .as_slice(),
        &[name_cs_id("D"), name_cs_id("F")]
    );
    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("I"))
            .await?
            .as_slice(),
        &[name_cs_id("H")]
    );
    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("J"))
            .await?
            .as_slice(),
        &[name_cs_id("F")]
    );

    Ok(())
}

pub async fn test_add_recursive_many_changesets(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let reference_storage = Arc::new(InMemoryCommitGraphStorage::new(test_repo_identity()));

    let reference_graph = Arc::new(
        from_dag(
            &ctx,
            r"
             A-B-C-D-G-H-I
              \     /
               E---F---J
         ",
            reference_storage,
        )
        .await?,
    );

    let graph = CommitGraph::new(storage.clone());
    let graph_writer = BaseCommitGraphWriter::new(graph.clone());

    assert_eq!(
        graph_writer
            .add_recursive(
                &ctx,
                reference_graph.clone(),
                vec1![
                    (name_cs_id("I"), smallvec![name_cs_id("H")], vec![]),
                    (name_cs_id("K"), smallvec![name_cs_id("I")], vec![]),
                    (name_cs_id("L"), smallvec![name_cs_id("K")], vec![]),
                    (name_cs_id("M"), smallvec![name_cs_id("J")], vec![]),
                ],
            )
            .await?,
        13
    );
    storage.flush();

    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("I"))
            .await?
            .as_slice(),
        &[name_cs_id("H")]
    );
    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("K"))
            .await?
            .as_slice(),
        &[name_cs_id("I")]
    );
    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("L"))
            .await?
            .as_slice(),
        &[name_cs_id("K")]
    );

    assert_eq!(
        graph_writer
            .add_recursive(
                &ctx,
                reference_graph.clone(),
                vec1![
                    (name_cs_id("N"), smallvec![name_cs_id("M")], vec![]),
                    (
                        name_cs_id("O"),
                        smallvec![name_cs_id("K"), name_cs_id("N")],
                        vec![]
                    ),
                ],
            )
            .await?,
        2
    );
    Ok(())
}

pub async fn test_add_many_changesets(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    //  Reference graph:
    //  r"
    //      A-B-C-D-G-H-I
    //       \     /
    //        E---F---J
    //  "

    let graph = CommitGraph::new(storage.clone());
    let graph_writer = BaseCommitGraphWriter::new(graph.clone());

    assert_eq!(
        graph_writer
            .add_many(
                &ctx,
                vec1![
                    (name_cs_id("A"), smallvec![], vec![]),
                    (name_cs_id("B"), smallvec![name_cs_id("A")], vec![]),
                    (name_cs_id("E"), smallvec![name_cs_id("A")], vec![]),
                    (name_cs_id("C"), smallvec![name_cs_id("B")], vec![]),
                ],
            )
            .await?,
        4
    );
    storage.flush();

    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("A"))
            .await?
            .as_slice(),
        &[]
    );
    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("B"))
            .await?
            .as_slice(),
        &[name_cs_id("A")]
    );
    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("C"))
            .await?
            .as_slice(),
        &[name_cs_id("B")]
    );

    // D is not yet inserted.
    assert!(
        graph
            .changeset_parents(&ctx, name_cs_id("D"))
            .await
            .is_err(),
    );

    // If the provided changesets are not in topological order we will
    // return an error.
    assert!(
        graph_writer
            .add_many(
                &ctx,
                vec1![
                    (
                        name_cs_id("G"),
                        smallvec![name_cs_id("D"), name_cs_id("F")],
                        vec![]
                    ),
                    (name_cs_id("D"), smallvec![name_cs_id("C")], vec![]),
                    (name_cs_id("F"), smallvec![name_cs_id("E")], vec![]),
                ],
            )
            .await
            .is_err()
    );

    assert_eq!(
        graph_writer
            .add_many(
                &ctx,
                vec1![
                    (name_cs_id("D"), smallvec![name_cs_id("C")], vec![]),
                    (name_cs_id("F"), smallvec![name_cs_id("E")], vec![]),
                    (
                        name_cs_id("G"),
                        smallvec![name_cs_id("D"), name_cs_id("F")],
                        vec![]
                    ),
                ],
            )
            .await?,
        3
    );
    storage.flush();

    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("D"))
            .await?
            .as_slice(),
        &[name_cs_id("C")]
    );
    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("F"))
            .await?
            .as_slice(),
        &[name_cs_id("E")]
    );
    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("G"))
            .await?
            .as_slice(),
        &[name_cs_id("D"), name_cs_id("F")]
    );

    // Re-inserting changesets is a no-op.
    graph_writer
        .add_many(
            &ctx,
            vec1![
                (name_cs_id("D"), smallvec![name_cs_id("C")], vec![]),
                (name_cs_id("F"), smallvec![name_cs_id("E")], vec![]),
                (
                    name_cs_id("G"),
                    smallvec![name_cs_id("D"), name_cs_id("F")],
                    vec![]
                ),
            ],
        )
        .await?;
    storage.flush();

    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("D"))
            .await?
            .as_slice(),
        &[name_cs_id("C")]
    );
    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("F"))
            .await?
            .as_slice(),
        &[name_cs_id("E")]
    );
    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("G"))
            .await?
            .as_slice(),
        &[name_cs_id("D"), name_cs_id("F")]
    );

    assert_eq!(
        graph_writer
            .add_many(
                &ctx,
                vec1![
                    (name_cs_id("H"), smallvec![name_cs_id("G")], vec![]),
                    (name_cs_id("J"), smallvec![name_cs_id("F")], vec![]),
                    (name_cs_id("I"), smallvec![name_cs_id("H")], vec![]),
                ],
            )
            .await?,
        3
    );
    storage.flush();

    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("H"))
            .await?
            .as_slice(),
        &[name_cs_id("G")]
    );
    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("I"))
            .await?
            .as_slice(),
        &[name_cs_id("H")]
    );
    assert_eq!(
        graph
            .changeset_parents(&ctx, name_cs_id("J"))
            .await?
            .as_slice(),
        &[name_cs_id("F")]
    );

    assert!(
        graph
            .is_ancestor(&ctx, name_cs_id("A"), name_cs_id("J"))
            .await?
    );
    assert!(
        !graph
            .is_ancestor(&ctx, name_cs_id("J"), name_cs_id("A"))
            .await?
    );
    assert!(
        !graph
            .is_ancestor(&ctx, name_cs_id("D"), name_cs_id("F"))
            .await?
    );
    assert!(
        !graph
            .is_ancestor(&ctx, name_cs_id("F"), name_cs_id("D"))
            .await?
    );
    assert!(
        graph
            .is_ancestor(&ctx, name_cs_id("F"), name_cs_id("H"))
            .await?
    );
    assert!(
        !graph
            .is_ancestor(&ctx, name_cs_id("H"), name_cs_id("F"))
            .await?
    );

    Ok(())
}

pub async fn test_ancestors_frontier_with(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
         A-B-C-D-G-H---J-K
            \   /   \ /
             E-F     I

         L-M-N-O-P-Q-R-S-T-U
         ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    let set1 = ["A", "B", "C", "D", "E", "F", "G", "H", "I"]
        .into_iter()
        .map(name_cs_id)
        .collect::<HashSet<_>>();

    assert_ancestors_frontier_with(
        &graph,
        &ctx,
        vec!["K", "U"],
        move |cs_id| {
            cloned!(set1);
            async move { Ok(set1.contains(&cs_id)) }
        },
        vec!["H", "I"],
    )
    .await?;

    let set2 = ["A", "B", "C", "E"]
        .into_iter()
        .map(name_cs_id)
        .collect::<HashSet<_>>();

    assert_ancestors_frontier_with(
        &graph,
        &ctx,
        vec!["D", "F"],
        {
            cloned!(set2);
            move |cs_id| {
                cloned!(set2);
                async move { Ok(set2.contains(&cs_id)) }
            }
        },
        vec!["C", "E"],
    )
    .await?;

    assert_ancestors_frontier_with(
        &graph,
        &ctx,
        vec!["G"],
        {
            cloned!(set2);
            move |cs_id| {
                cloned!(set2);
                async move { Ok(set2.contains(&cs_id)) }
            }
        },
        vec!["C", "E"],
    )
    .await?;

    assert_ancestors_frontier_with(
        &graph,
        &ctx,
        vec!["K"],
        {
            cloned!(set2);
            move |cs_id| {
                cloned!(set2);
                async move { Ok(set2.contains(&cs_id)) }
            }
        },
        vec!["C", "E"],
    )
    .await?;

    assert_ancestors_frontier_with(
        &graph,
        &ctx,
        vec!["D"],
        {
            cloned!(set2);
            move |cs_id| {
                cloned!(set2);
                async move { Ok(set2.contains(&cs_id)) }
            }
        },
        vec!["C"],
    )
    .await?;

    Ok(())
}

pub async fn test_range_stream(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
         A-B-C-D-G-H---J-K
            \   /   \ /
             E-F     I

         L-M-N-O-P-Q-R-S-T-U
         ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_range_stream(
        &graph,
        &ctx,
        "A",
        "K",
        vec!["A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K"],
    )
    .await?;
    assert_range_stream(&graph, &ctx, "D", "K", vec!["D", "G", "H", "I", "J", "K"]).await?;
    assert_range_stream(&graph, &ctx, "A", "U", vec![]).await?;
    assert_range_stream(&graph, &ctx, "O", "T", vec!["O", "P", "Q", "R", "S", "T"]).await?;

    Ok(())
}

pub async fn test_common_base(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
        A-B-C-D-E-L------N
           \       \    /
            F-G-H   M  /
             \     /  /
              I-J-K--/

        O-P-Q-R-S-T-U-V-W
        ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_common_base(&graph, &ctx, "J", "J", vec!["J"]).await?;
    assert_common_base(&graph, &ctx, "K", "J", vec!["J"]).await?;
    assert_common_base(&graph, &ctx, "E", "H", vec!["B"]).await?;
    assert_common_base(&graph, &ctx, "G", "J", vec!["F"]).await?;
    assert_common_base(&graph, &ctx, "M", "N", vec!["K", "L"]).await?;
    assert_common_base(&graph, &ctx, "L", "K", vec!["B"]).await?;
    assert_common_base(&graph, &ctx, "M", "H", vec!["F"]).await?;
    assert_common_base(&graph, &ctx, "A", "B", vec!["A"]).await?;
    assert_common_base(&graph, &ctx, "N", "W", vec![]).await?;
    assert_common_base(&graph, &ctx, "D", "Q", vec![]).await?;

    Ok(())
}

pub async fn test_slice_ancestors(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
         A-B-C-D-G-H---J-K
            \   /   \ /
             E-F     I

         L-M-N-O-P-Q-R-S-T-U
         ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_slice_ancestors(
        &graph,
        &ctx,
        vec!["H"],
        |cs_ids| async { Ok(cs_ids.into_iter().collect::<HashSet<_>>()) },
        2,
        vec![(1, vec!["B"]), (3, vec!["D", "F"]), (5, vec!["H"])],
    )
    .await?;

    assert_slice_ancestors(
        &graph,
        &ctx,
        vec!["Q"],
        |cs_ids| async { Ok(cs_ids.into_iter().collect::<HashSet<_>>()) },
        1,
        vec![
            (1, vec!["L"]),
            (2, vec!["M"]),
            (3, vec!["N"]),
            (4, vec!["O"]),
            (5, vec!["P"]),
            (6, vec!["Q"]),
        ],
    )
    .await?;

    assert_slice_ancestors(
        &graph,
        &ctx,
        vec!["Q"],
        |cs_ids| async { Ok(cs_ids.into_iter().collect::<HashSet<_>>()) },
        3,
        vec![(1, vec!["N"]), (4, vec!["Q"])],
    )
    .await?;

    let set1 = ["P", "Q", "R", "S", "T", "U"]
        .into_iter()
        .map(name_cs_id)
        .collect::<HashSet<_>>();

    assert_slice_ancestors(
        &graph,
        &ctx,
        vec!["Q"],
        |_| async { Ok(set1.clone()) },
        1,
        vec![(5, vec!["P"]), (6, vec!["Q"])],
    )
    .await?;

    Ok(())
}

pub async fn test_segmented_slice_ancestors(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
         A-B-C-D-G-H---J-K
            \   /   \ /
             E-F     I

         L-M-N-O-P-Q-R-S-T-U-V
         ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_segmented_slice_ancestors(
        &graph,
        &ctx,
        vec!["H"],
        vec![],
        2,
        vec![
            vec![("B", "A")],
            vec![("D", "C")],
            vec![("F", "E")],
            vec![("H", "G")],
        ],
        vec!["B", "D", "F"],
    )
    .await?;

    assert_segmented_slice_ancestors(
        &graph,
        &ctx,
        vec!["Q"],
        vec![],
        1,
        vec![
            vec![("L", "L")],
            vec![("M", "M")],
            vec![("N", "N")],
            vec![("O", "O")],
            vec![("P", "P")],
            vec![("Q", "Q")],
        ],
        vec!["L", "M", "N", "O", "P"],
    )
    .await?;

    // Each slice is assigned the correct boundary: ("L", "L") depends on nothing,
    // ("M", "M") depends on "L" and so on.
    assert_segmented_slice_ancestors_with_per_slice_boundaries(
        &graph,
        &ctx,
        vec!["Q"],
        vec![],
        1,
        vec![
            vec![("L", "L")],
            vec![("M", "M")],
            vec![("N", "N")],
            vec![("O", "O")],
            vec![("P", "P")],
            vec![("Q", "Q")],
        ],
        vec![
            vec![],
            vec!["L"],
            vec!["M"],
            vec!["N"],
            vec!["O"],
            vec!["P"],
        ],
    )
    .await?;

    assert_segmented_slice_ancestors(
        &graph,
        &ctx,
        vec!["K", "V"],
        vec![],
        5,
        vec![
            vec![("P", "L")],
            vec![("U", "Q")],
            vec![("V", "V"), ("D", "A")],
            vec![("F", "E"), ("I", "G")],
            vec![("K", "J")],
        ],
        vec!["B", "D", "H", "I", "P", "U"],
    )
    .await?;

    assert_segmented_slice_ancestors(
        &graph,
        &ctx,
        vec!["K", "V"],
        vec!["D", "N"],
        5,
        vec![
            vec![("S", "O")],
            vec![("V", "T"), ("F", "E")],
            vec![("I", "G"), ("K", "J")],
        ],
        vec!["F", "S"],
    )
    .await?;

    Ok(())
}

pub async fn test_children(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
        A-B-C-D-E-L------N
           \       \    /
            F-G-H   M  /
             \     /  /
              I-J-K--/
        ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_children(&graph, &ctx, "A", vec!["B"]).await?;
    assert_children(&graph, &ctx, "B", vec!["C", "F"]).await?;
    assert_children(&graph, &ctx, "C", vec!["D"]).await?;
    assert_children(&graph, &ctx, "D", vec!["E"]).await?;
    assert_children(&graph, &ctx, "E", vec!["L"]).await?;
    assert_children(&graph, &ctx, "F", vec!["G", "I"]).await?;
    assert_children(&graph, &ctx, "G", vec!["H"]).await?;
    assert_children(&graph, &ctx, "H", vec![]).await?;
    assert_children(&graph, &ctx, "I", vec!["J"]).await?;
    assert_children(&graph, &ctx, "J", vec!["K"]).await?;
    assert_children(&graph, &ctx, "K", vec!["M", "N"]).await?;
    assert_children(&graph, &ctx, "L", vec!["M", "N"]).await?;
    assert_children(&graph, &ctx, "M", vec![]).await?;
    assert_children(&graph, &ctx, "N", vec![]).await?;

    Ok(())
}

pub async fn test_descendants(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
        A-B-C-D-E-L------N
           \       \    /
            F-G-H   M  /
             \     /  /
              I-J-K--/
        ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_descendants(
        &graph,
        &ctx,
        vec!["A"],
        vec![
            "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N",
        ],
    )
    .await?;
    assert_descendants(
        &graph,
        &ctx,
        vec!["B"],
        vec![
            "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N",
        ],
    )
    .await?;
    assert_descendants(&graph, &ctx, vec!["C"], vec!["C", "D", "E", "L", "M", "N"]).await?;
    assert_descendants(&graph, &ctx, vec!["D"], vec!["D", "E", "L", "M", "N"]).await?;
    assert_descendants(&graph, &ctx, vec!["E"], vec!["E", "L", "M", "N"]).await?;
    assert_descendants(
        &graph,
        &ctx,
        vec!["F"],
        vec!["F", "G", "H", "I", "J", "K", "M", "N"],
    )
    .await?;
    assert_descendants(&graph, &ctx, vec!["G"], vec!["G", "H"]).await?;
    assert_descendants(&graph, &ctx, vec!["H"], vec!["H"]).await?;
    assert_descendants(&graph, &ctx, vec!["I"], vec!["I", "J", "K", "M", "N"]).await?;
    assert_descendants(&graph, &ctx, vec!["J"], vec!["J", "K", "M", "N"]).await?;
    assert_descendants(&graph, &ctx, vec!["K"], vec!["K", "M", "N"]).await?;
    assert_descendants(&graph, &ctx, vec!["L"], vec!["L", "M", "N"]).await?;
    assert_descendants(&graph, &ctx, vec!["M"], vec!["M"]).await?;
    assert_descendants(&graph, &ctx, vec!["N"], vec!["N"]).await?;

    assert_descendants(
        &graph,
        &ctx,
        vec!["C", "G", "H"],
        vec!["C", "D", "E", "G", "H", "L", "M", "N"],
    )
    .await?;
    assert_descendants(&graph, &ctx, vec![], vec![]).await?;

    Ok(())
}

pub async fn test_ancestors_difference_segments_1(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
        A-B-C-D-E---L------N----O
           \         \    /
            F-G-H     M  /
             \       /  /
              I-J---K--/---Q---R
                 \
                  \---------P
        ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_ancestors_difference_segments(&ctx, &graph, vec!["N"], vec![], 3).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["N"], vec!["D"], 3).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["H"], vec!["G"], 1).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["M"], vec![], 3).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["M"], vec!["H"], 3).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["N"], vec!["E", "J"], 3).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["O", "P"], vec![], 4).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["O", "P"], vec!["H"], 4).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["O", "P"], vec!["D", "I"], 4).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["F"], vec!["H"], 0).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["M"], vec!["K"], 2).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["N", "R"], vec![], 3).await?;

    Ok(())
}

pub async fn test_ancestors_difference_segments_2(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
        A--B------C----E---J---K
         \  \      \
          \  \--D   \-----F----L
           \  \            \
            \  \--G---H     \--M
             \     \
              \-P   \--I--N----O
        ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_ancestors_difference_segments(&ctx, &graph, vec!["K"], vec![], 1).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["L"], vec![], 1).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["M"], vec![], 1).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["O"], vec![], 1).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["K", "L"], vec![], 2).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["K", "L", "M", "O"], vec![], 4).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["K", "L"], vec!["M"], 2).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["K", "L", "H"], vec!["M", "O"], 3)
        .await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["C"], vec!["M"], 0).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["A", "B", "E"], vec![], 1).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["B", "H", "O"], vec!["D"], 2).await?;
    assert_ancestors_difference_segments(&ctx, &graph, vec!["E", "L", "K"], vec!["J"], 2).await?;
    assert_ancestors_difference_segments(
        &ctx,
        &graph,
        vec![
            "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P",
        ],
        vec![],
        7,
    )
    .await?;
    Ok(())
}

pub async fn test_ancestors_difference_segments_3(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
        A--B--C--D
            \  \
             E--F
        ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_ancestors_difference_segments(&ctx, &graph, vec!["F"], vec!["D"], 2).await?;

    Ok(())
}

pub async fn test_locations_to_changeset_ids(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
        A-B-C-D-E---L------N----O
           \         \    /
            F-G-H     M  /
             \       /  /
              I-J---K--/---Q---R
                 \
                  \---------P
        ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_locations_to_changeset_ids(&ctx, &graph, "L", 2, 4, vec!["D", "C", "B", "A"]).await?;
    assert_locations_to_changeset_ids(&ctx, &graph, "H", 0, 5, vec!["H", "G", "F", "B", "A"])
        .await?;
    assert_locations_to_changeset_ids(&ctx, &graph, "R", 1, 2, vec!["Q", "K"]).await?;
    assert_locations_to_changeset_ids(&ctx, &graph, "R", 2, 2, vec!["K", "J"]).await?;
    assert_locations_to_changeset_ids(&ctx, &graph, "R", 3, 2, vec!["J", "I"]).await?;
    assert_locations_to_changeset_ids(&ctx, &graph, "R", 4, 2, vec!["I", "F"]).await?;
    assert_locations_to_changeset_ids(&ctx, &graph, "M", 0, 1, vec!["M"]).await?;
    assert_locations_to_changeset_ids_errors(&ctx, &graph, "M", 1, 1).await?;
    assert_locations_to_changeset_ids(&ctx, &graph, "O", 0, 1, vec!["O"]).await?;
    assert_locations_to_changeset_ids(&ctx, &graph, "O", 0, 2, vec!["O", "N"]).await?;
    assert_locations_to_changeset_ids_errors(&ctx, &graph, "O", 0, 3).await?;

    Ok(())
}

pub async fn test_changeset_ids_to_locations(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
        A-B-C-D-E---L------N----O
           \         \    /
            F-G-H     M  /
             \       /  /
              I-J---K--/---Q---R
                 \
                  \---------P
        ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_changeset_ids_to_locations(
        &ctx,
        &graph,
        vec!["O"],
        vec![
            "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q",
            "R",
        ],
    )
    .await?;
    assert_changeset_ids_to_locations(
        &ctx,
        &graph,
        vec!["O", "R"],
        vec![
            "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q",
            "R",
        ],
    )
    .await?;
    assert_changeset_ids_to_locations(
        &ctx,
        &graph,
        vec!["O", "R", "P"],
        vec![
            "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q",
            "R",
        ],
    )
    .await?;

    Ok(())
}

pub async fn test_process_topologically(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
        A--B--C--D--E--F
         \
          G--H---I--J--K
           \    /
            L--M
        ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_process_topologically(
        &ctx,
        &graph,
        vec![
            "I", "J", "K", "F", "B", "C", "G", "H", "L", "D", "E", "M", "A",
        ],
    )
    .await?;
    assert_process_topologically(&ctx, &graph, vec!["F", "C", "A", "B", "E", "D"]).await?;
    assert_process_topologically(&ctx, &graph, vec!["H", "C", "L"]).await?;
    assert_process_topologically(&ctx, &graph, vec!["B", "C", "J", "I"]).await?;
    assert_process_topologically(&ctx, &graph, vec![]).await?;

    Ok(())
}

pub async fn test_minimize_frontier(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
        A-B-C-D-E-L------N
           \       \    /
            F-G-H   M  /
             \     /  /
              I-J-K--/
        ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_minimize_frontier(&ctx, &graph, vec!["L", "M", "N"], vec!["M", "N"]).await?;
    assert_minimize_frontier(&ctx, &graph, vec!["A", "B", "C", "D"], vec!["D"]).await?;
    assert_minimize_frontier(&ctx, &graph, vec!["D", "L", "I", "K"], vec!["L", "K"]).await?;
    assert_minimize_frontier(&ctx, &graph, vec![], vec![]).await?;
    assert_minimize_frontier(&ctx, &graph, vec!["B", "C", "H"], vec!["C", "H"]).await?;
    assert_minimize_frontier(
        &ctx,
        &graph,
        vec![
            "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N",
        ],
        vec!["H", "M", "N"],
    )
    .await?;

    Ok(())
}

pub async fn test_ancestors_within_distance(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
        A-B-C-D-E-L------N
           \       \    /
            F-G-H   M  /
             \     /  /
              I-J-K--/
        ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_ancestors_within_distance(&ctx, &graph, vec!["N"], 0, vec![("N", 0)]).await?;
    assert_ancestors_within_distance(
        &ctx,
        &graph,
        vec!["N"],
        1,
        vec![("N", 0), ("L", 1), ("K", 1)],
    )
    .await?;
    assert_ancestors_within_distance(
        &ctx,
        &graph,
        vec!["N"],
        2,
        vec![("N", 0), ("L", 1), ("K", 1), ("E", 2), ("J", 2)],
    )
    .await?;
    assert_ancestors_within_distance(
        &ctx,
        &graph,
        vec!["N"],
        3,
        vec![
            ("N", 0),
            ("L", 1),
            ("K", 1),
            ("E", 2),
            ("J", 2),
            ("I", 3),
            ("D", 3),
        ],
    )
    .await?;
    assert_ancestors_within_distance(
        &ctx,
        &graph,
        vec!["N"],
        4,
        vec![
            ("N", 0),
            ("L", 1),
            ("K", 1),
            ("E", 2),
            ("J", 2),
            ("I", 3),
            ("D", 3),
            ("F", 4),
            ("C", 4),
        ],
    )
    .await?;
    assert_ancestors_within_distance(
        &ctx,
        &graph,
        vec!["N"],
        5,
        vec![
            ("N", 0),
            ("L", 1),
            ("K", 1),
            ("E", 2),
            ("J", 2),
            ("I", 3),
            ("D", 3),
            ("F", 4),
            ("C", 4),
            ("B", 5),
        ],
    )
    .await?;
    assert_ancestors_within_distance(
        &ctx,
        &graph,
        vec!["N"],
        6,
        vec![
            ("N", 0),
            ("L", 1),
            ("K", 1),
            ("E", 2),
            ("J", 2),
            ("I", 3),
            ("D", 3),
            ("F", 4),
            ("C", 4),
            ("B", 5),
            ("A", 6),
        ],
    )
    .await?;
    assert_ancestors_within_distance(
        &ctx,
        &graph,
        vec!["N"],
        100,
        vec![
            ("N", 0),
            ("L", 1),
            ("K", 1),
            ("E", 2),
            ("J", 2),
            ("I", 3),
            ("D", 3),
            ("F", 4),
            ("C", 4),
            ("B", 5),
            ("A", 6),
        ],
    )
    .await?;
    assert_ancestors_within_distance(
        &ctx,
        &graph,
        vec!["N", "M", "H"],
        0,
        vec![("N", 0), ("M", 0), ("H", 0)],
    )
    .await?;
    assert_ancestors_within_distance(
        &ctx,
        &graph,
        vec!["N", "M", "H"],
        1,
        vec![("N", 0), ("M", 0), ("H", 0), ("L", 1), ("K", 1), ("G", 1)],
    )
    .await?;
    assert_ancestors_within_distance(&ctx, &graph, vec![], 100, vec![]).await?;

    Ok(())
}

pub async fn test_find_boundary(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
        A-B-C-D-E-L------N
           \       \    /
            F-G-H   M  /
             \     /  /
              I-J-K--/
        ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_find_boundary(&ctx, &graph, vec!["N"], vec!["N"]).await?;
    assert_find_boundary(&ctx, &graph, vec!["N", "L", "K", "J", "I"], vec!["L", "I"]).await?;
    assert_find_boundary(
        &ctx,
        &graph,
        vec![
            "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N",
        ],
        vec![],
    )
    .await?;
    assert_find_boundary(
        &ctx,
        &graph,
        vec!["I", "F", "G", "M", "L", "E"],
        vec!["F", "M", "E"],
    )
    .await?;
    Ok(())
}

pub async fn test_linear_ancestors_stream(
    ctx: CoreContext,
    storage: Arc<dyn CommitGraphStorageTest>,
) -> Result<()> {
    let graph = from_dag(
        &ctx,
        r"
        P     O
        |     |
        N     |
        |\    |
        | \   M
        |  \  |
        J   \ |   L
        |    \|  /
        |     K /
        |  Q  |/
        | /   I
        |/ G  |
        E  |  H
        |  F /
        D  |/ 
        |  |
        C /
        |/
        B
        |
        A
        ",
        storage.clone(),
    )
    .await?;
    storage.flush();

    assert_linear_ancestors_stream(
        &ctx,
        &graph,
        "P",
        None,
        None,
        None,
        vec!["P", "N", "J", "E", "D", "C", "B", "A"],
    )
    .await?;

    assert_linear_ancestors_stream(
        &ctx,
        &graph,
        "P",
        Some("L"),
        None,
        None,
        vec!["P", "N", "J", "E", "D", "C"],
    )
    .await?;

    assert_linear_ancestors_stream(
        &ctx,
        &graph,
        "P",
        Some("O"),
        None,
        None,
        vec!["P", "N", "J", "E", "D", "C"],
    )
    .await?;

    assert_linear_ancestors_stream(
        &ctx,
        &graph,
        "P",
        Some("Q"),
        None,
        None,
        vec!["P", "N", "J"],
    )
    .await?;

    assert_linear_ancestors_stream(
        &ctx,
        &graph,
        "P",
        None,
        Some("E"),
        None,
        vec!["P", "N", "J", "E"],
    )
    .await?;

    assert_linear_ancestors_stream(
        &ctx,
        &graph,
        "P",
        Some("Q"),
        Some("E"),
        None,
        vec!["P", "N", "J"],
    )
    .await?;

    assert_linear_ancestors_stream(&ctx, &graph, "P", None, Some("F"), None, vec![]).await?;

    assert_linear_ancestors_stream(
        &ctx,
        &graph,
        "P",
        Some("L"),
        None,
        Some(2),
        vec!["J", "E", "D", "C"],
    )
    .await?;

    Ok(())
}
