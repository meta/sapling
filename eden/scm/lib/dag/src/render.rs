/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(any(test, feature = "indexedlog-backend"))]
use std::cmp::Ordering;
#[cfg(any(test, feature = "indexedlog-backend"))]
use std::io::Write;

use anyhow::Result;
use nonblocking::non_blocking_result;
// Re-export
pub use renderdag::Ancestor;
pub use renderdag::GraphRow;
pub use renderdag::GraphRowRenderer;
pub use renderdag::Renderer;
pub use renderdag::*;

#[cfg(any(test, feature = "indexedlog-backend"))]
use crate::Dag;
use crate::DagAlgorithm;
#[cfg(any(test, feature = "indexedlog-backend"))]
use crate::Group;
#[cfg(any(test, feature = "indexedlog-backend"))]
use crate::IdSpan;
#[cfg(any(test, feature = "indexedlog-backend"))]
use crate::Level;
use crate::Set;
use crate::Vertex;
#[cfg(any(test, feature = "indexedlog-backend"))]
use crate::ops::IdConvert;
use crate::set::SyncSetQuery;

/// Render a Dag or MemDag into a String.
pub fn render_dag(
    dag: &(impl DagAlgorithm + ?Sized),
    get_message: impl Fn(&Vertex) -> Option<String>,
) -> Result<String> {
    let mut renderer = GraphRowRenderer::new().output().build_box_drawing();

    let mut out = String::new();
    let next_rows = dag_to_renderer_next_rows(dag, None)?;
    for (node, parents) in next_rows {
        let mut name = format!("{node:?}");
        let message = get_message(&node).unwrap_or_default();
        let row = if name.len() == 1 {
            renderer.next_row(node, parents, name, message)
        } else {
            if !message.is_empty() {
                name += &format!(" {message}");
            }
            renderer.next_row(node, parents, String::from("o"), name)
        };
        out.push_str(&row);
    }

    let output = format!(
        "\n{}",
        out.trim_end()
            .lines()
            .map(|l| format!("            {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    Ok(output)
}

/// Render a Dag or MemDag into structured `GraphRow`s.
/// The `GraphRow` can serialize to other formats.
///
/// If `subset` is provided, only render a subset of the graph.
pub fn render_dag_structured(
    dag: &dyn DagAlgorithm,
    subset: Option<Set>,
) -> Result<Vec<GraphRow<Vertex>>> {
    let mut renderer = GraphRowRenderer::new();
    let next_rows = dag_to_renderer_next_rows(dag, subset)?;
    let mut out = Vec::with_capacity(next_rows.len());
    for (node, parents) in next_rows {
        let name = String::from_utf8_lossy(node.as_ref()).into_owned();
        let message = Default::default();
        let row = renderer.next_row(node, parents, name, message);
        out.push(row);
    }
    Ok(out)
}

/// Produce inputs (node, parents) for graph_row.
///
/// If `subset` is provided, only render a subset of the graph in
/// the `subset` order.
fn dag_to_renderer_next_rows(
    dag: &(impl DagAlgorithm + ?Sized),
    subset: Option<Set>,
) -> Result<Vec<(Vertex, Vec<Ancestor<Vertex>>)>> {
    let subset = match subset {
        None => non_blocking_result(dag.all())?,
        Some(set) => set,
    };
    let iter: Vec<_> = subset.iter()?.collect::<crate::Result<_>>()?;
    let subdag = non_blocking_result(dag.subdag(subset))?;

    let mut out = Vec::with_capacity(iter.len());
    for node in iter {
        let direct_parents = non_blocking_result(dag.parent_names(node.clone()))?;
        let subdag_parents = non_blocking_result(subdag.parent_names(node.clone()))?;
        let mut parents: Vec<Ancestor<Vertex>> = subdag_parents
            .iter()
            .cloned()
            .map(|p| {
                if direct_parents.contains(&p) {
                    Ancestor::Parent(p)
                } else {
                    Ancestor::Ancestor(p)
                }
            })
            .collect();
        if parents.len() < direct_parents.len() {
            let subdag_ancestors =
                non_blocking_result(dag.ancestors(Set::from_static_names(subdag_parents.clone())))?;
            for p in &direct_parents {
                if subdag_parents.contains(p) {
                    continue;
                }
                // Is the direct parent connect to any of the subdag parents?
                let direct_reachable = non_blocking_result(dag.ancestors(p.into()))?;
                if direct_reachable
                    .intersection(&subdag_ancestors)
                    .is_empty()?
                {
                    parents.push(Ancestor::Anonymous)
                }
            }
        }
        out.push((node, parents));
    }

    Ok(out)
}

#[cfg(any(test, feature = "indexedlog-backend"))]
pub fn render_segment_dag(
    mut out: impl Write,
    dag: &Dag,
    level: Level,
    group: Group,
) -> Result<()> {
    let segs = dag.dag.next_segments(group.min_id(), level)?;
    // The renderer borrows `segs` and must be dropped first.
    let mut renderer = renderdag::GraphRowRenderer::new()
        .output()
        .build_box_drawing();

    for seg in segs.iter().rev() {
        let mut parents = vec![];
        for parent_id in seg.parents()? {
            // For each parent Id, look for the containing segment.
            let parent_span: IdSpan = parent_id.into();
            let parent_idx = segs.binary_search_by(|s| {
                let span = s.span().unwrap();
                if span.contains(parent_id) {
                    Ordering::Equal
                } else {
                    span.cmp(&parent_span)
                }
            });

            if let Ok(parent_idx) = parent_idx {
                parents.push(Ancestor::Parent(&segs[parent_idx]));
            } else {
                // Probably a non-master segment with master parent.
                parents.push(Ancestor::Anonymous);
            }
        }

        let span = seg.span()?;
        let get_hex = |id| -> String {
            non_blocking_result(dag.vertex_name(id))
                .map(|s| format!("{s:.12?}"))
                .unwrap_or_default()
        };
        let name = format!(
            "{}({})-{}({})",
            get_hex(span.low),
            span.low,
            get_hex(span.high),
            span.high,
        );
        let row = renderer.next_row(seg, parents, String::from("o"), name);
        write!(out, "{row}")?;
    }

    Ok(())
}
