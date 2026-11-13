/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Line identities used for graph coloring.

use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering as AtomicOrdering;

#[cfg(feature = "serialize")]
use serde::Serialize;
/// A set of line identifiers for graph coloring.
pub use small_set::SmallSet as LineIds;

/// Optional line ID attached to one prefix fragment.
/// A terminal character has one foreground, so a fragment shared by multiple
/// lines carries one of their IDs. `GraphRowLineIds` keeps all of them.
pub type PrefixLineColor = Option<usize>;

use crate::column::Column;
use crate::pipeline::types::Ancestor;
use crate::pipeline::types::LinkLine;

/// Line identities before, within, and after a row's linking stage.
///
/// IDs identify lines independently of their current column. Renderers choose
/// colors from these IDs. The column vectors must align with the row shape;
/// the shape still determines whether and how each segment is drawn.
/// Different lines can reuse an ID chosen by the allocator.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serialize", derive(Serialize))]
pub struct GraphRowLineIds {
    /// IDs for each column of the node line and its pre-/post-node padding
    /// lines, which precede the link line.
    /// Includes the node column's IDs even when the node is a head or root.
    /// Isolated nodes have no line IDs.
    pub before_link: Vec<LineIds>,

    /// IDs for each link-line cell, aligned with `GraphRowShape::link_line`.
    pub link_line: Option<Vec<LinkLineIds>>,

    /// IDs for ancestry and termination columns after the link line.
    /// Includes anonymous parent IDs until termination, even when their
    /// ancestry-line columns are blank.
    pub after_link: Vec<LineIds>,
}

/// Line identities for the six drawable segments of a link-line cell.
/// An empty set carries no line identity; a segment can belong to multiple lines.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serialize", derive(Serialize))]
pub struct LinkLineIds {
    /// The vertical segment passing through the cell.
    pub vertical: LineIds,

    /// The horizontal segment passing through the cell.
    pub horizontal: LineIds,

    /// The segment from the left edge to the bottom edge.
    pub left_fork: LineIds,

    /// The segment from the right edge to the bottom edge.
    pub right_fork: LineIds,

    /// The segment from the left edge to the top edge.
    pub left_merge: LineIds,

    /// The segment from the right edge to the top edge.
    pub right_merge: LineIds,
}

/// Information available while allocating an outgoing parent connection's ID.
pub struct LineIdContext<'a, N> {
    /// The node being rendered.
    pub node: &'a N,
    /// The outgoing connection, including anonymous or indirect parents.
    pub parent: &'a Ancestor<N>,
    /// Number of input parent connections for this node, including anonymous
    /// and indirect parents. Equals the length of the input `parents` vector.
    pub parent_count: usize,
    /// The node's column in this row.
    pub node_column: usize,
    /// The selected parent column, before final column compression.
    pub parent_column: usize,
    /// Incoming IDs at the start of this row. New heads have no incoming IDs.
    /// Newly added columns are padded with empty sets.
    pub before_link: &'a [LineIds],
    /// Current column IDs after releasing the node's incoming lines and
    /// allocating earlier parent connections in this row.
    pub column_line_ids: &'a [LineIds],
    /// The union of the current column IDs.
    pub allocated_line_ids: &'a LineIds,
}

/// Select a line ID for every outgoing parent connection.
///
/// IDs can be reused to group lines together. Keep IDs in a bounded range:
/// `LineIds` uses bitsets whose memory and operation costs grow with the IDs.
pub type LineIdAllocator<N> = dyn Fn(&LineIdContext<'_, N>) -> usize + Send + Sync;

/// Allocation presets by index:
/// - 0: reuse parent IDs, then unused incoming IDs, then the lowest free ID.
/// - 1: continue incoming IDs and cycle through free IDs for new lines.
/// - 2: use the node column for single-parent edges, the parent column otherwise.
///
/// See [`build_line_id_allocator_from_rules`] for the rule syntax.
pub const COLOR_RULE_PRESETS: [&str; 3] = [
    "parent,incoming,free",
    "incoming,next-free",
    "single:node-lane,lane",
];

#[derive(Clone, Copy)]
enum Condition {
    Always,
    Single,
    NotSingle,
}

#[derive(Clone, Copy)]
enum Source {
    Parent,
    Incoming,
    Free,
    NextFree,
    Lane,
    NodeLane,
}

#[derive(Clone, Copy)]
struct Rule {
    condition: Condition,
    source: Source,
}

/// Build an allocator from a comma-separated priority list of `[COND:]SOURCE`.
/// Whitespace around rules is ignored. The only condition is `single`
/// (`parent_count == 1`); `!single` negates it. Rules without a condition
/// always apply. The first applicable source yielding an ID wins; otherwise
/// the allocator returns zero. IDs are restricted to `0..line_id_count`.
///
/// Sources:
/// - `parent`: the first existing parent-column ID in range.
/// - `incoming`: the first incoming node-column ID in range that is unused.
/// - `free`: the lowest unused ID.
/// - `next-free`: search cyclically after the last ID selected by this source.
///   Initially searches from zero. Only a successful selection moves the cursor.
/// - `lane`: the parent column modulo the ID count.
/// - `node-lane`: the node column modulo the ID count.
///
/// Returns `None` for an invalid/empty rule or zero ID count. Rules are parsed
/// only during construction; allocation calls borrow the context and allocate
/// no memory. Cloning the returned `Arc` shares the `next-free` cursor.
/// Construct another allocator to start an independent sequence.
pub fn build_line_id_allocator_from_rules<N>(
    rules: &str,
    line_id_count: usize,
) -> Option<Arc<LineIdAllocator<N>>> {
    if line_id_count == 0 {
        return None;
    }
    let rules: Box<[Rule]> = rules
        .split(',')
        .map(|text| {
            let text = text.trim();
            let (condition, source) = if let Some((condition, source)) = text.split_once(':') {
                let condition = match condition {
                    "single" => Condition::Single,
                    "!single" => Condition::NotSingle,
                    _ => return None,
                };
                (condition, source)
            } else {
                (Condition::Always, text)
            };
            let source = match source {
                "parent" => Source::Parent,
                "incoming" => Source::Incoming,
                "free" => Source::Free,
                "next-free" => Source::NextFree,
                "lane" => Source::Lane,
                "node-lane" => Source::NodeLane,
                _ => return None,
            };
            Some(Rule { condition, source })
        })
        .collect::<Option<Vec<_>>>()?
        .into_boxed_slice();
    let cursor = AtomicUsize::new(line_id_count - 1);
    Some(Arc::new(move |context: &LineIdContext<'_, N>| {
        for rule in &rules {
            let applies = match rule.condition {
                Condition::Always => true,
                Condition::Single => context.parent_count == 1,
                Condition::NotSingle => context.parent_count != 1,
            };
            if !applies {
                continue;
            }
            let id = match rule.source {
                Source::Parent => context.column_line_ids[context.parent_column]
                    .iter()
                    .next()
                    .filter(|id| *id < line_id_count),
                Source::Incoming => context.before_link[context.node_column]
                    .iter()
                    .take_while(|id| *id < line_id_count)
                    .find(|id| !context.allocated_line_ids.contains(*id)),
                Source::Free => {
                    (0..line_id_count).find(|id| !context.allocated_line_ids.contains(*id))
                }
                Source::NextFree => {
                    // A shaper invokes its allocator sequentially; no CAS is needed.
                    let previous = cursor.load(AtomicOrdering::Relaxed);
                    let start = if previous + 1 == line_id_count {
                        0
                    } else {
                        previous + 1
                    };
                    let selected = (start..line_id_count)
                        .chain(0..start)
                        .find(|id| !context.allocated_line_ids.contains(*id));
                    if let Some(selected) = selected {
                        cursor.store(selected, AtomicOrdering::Relaxed);
                    }
                    selected
                }
                Source::Lane => Some(context.parent_column % line_id_count),
                Source::NodeLane => Some(context.node_column % line_id_count),
            };
            if let Some(id) = id {
                return id;
            }
        }
        0
    }))
}

#[derive(Default)]
pub(crate) struct ColoringState {
    column_ids: Vec<LineIds>,
}

pub(crate) struct RowColoring {
    node_column: usize,
    before_link: Vec<LineIds>,
    connections: Vec<LineIds>,
    head_line_id: Option<usize>,
    moved_parent_column: Option<usize>,
}

impl ColoringState {
    fn allocated_line_ids(&self) -> LineIds {
        let mut allocated = LineIds::empty();
        for ids in &self.column_ids {
            allocated.union_with(ids);
        }
        allocated
    }

    pub(crate) fn begin_row(&mut self, width: usize, node_column: usize) -> RowColoring {
        self.column_ids.resize_with(width, LineIds::empty);
        let before_link = self.column_ids.clone();
        self.column_ids[node_column] = LineIds::empty();
        RowColoring {
            node_column,
            before_link,
            connections: vec![LineIds::empty(); width],
            head_line_id: None,
            moved_parent_column: None,
        }
    }

    pub(crate) fn add_parent(
        &mut self,
        row: &mut RowColoring,
        index: usize,
        allocate: impl FnOnce(&[LineIds], &[LineIds], &LineIds) -> usize,
    ) {
        let width = self.column_ids.len().max(index + 1);
        self.column_ids.resize_with(width, LineIds::empty);
        row.before_link.resize_with(width, LineIds::empty);
        row.connections.resize_with(width, LineIds::empty);
        let allocated = self.allocated_line_ids();
        let id = allocate(&row.before_link, &self.column_ids, &allocated);
        let ids = LineIds::from(id);
        row.head_line_id.get_or_insert(id);
        self.column_ids[index].union_with(&ids);
        row.connections[index].union_with(&ids);
    }

    pub(crate) fn move_parent(&mut self, row: &mut RowColoring, parent_column: usize) {
        self.column_ids.swap(row.node_column, parent_column);
        row.connections.swap(row.node_column, parent_column);
        row.moved_parent_column = Some(parent_column);
    }

    pub(crate) fn finish_row<N>(
        &mut self,
        mut row: RowColoring,
        link_line: Option<&[LinkLine]>,
        columns: &[Column<N>],
    ) -> Box<GraphRowLineIds> {
        let link_line = link_line.map(|line| row.link_line_ids(line));
        // Assign a head's node ID only after allocating all its parent
        // connections, so every allocator call sees the same incoming snapshot.
        if row.before_link[row.node_column].is_empty()
            && let Some(id) = row.head_line_id
        {
            row.before_link[row.node_column] = LineIds::from(id);
        }
        // Snapshot before clearing anonymous parents and trimmed columns.
        let after_link = self.column_ids.clone();
        self.column_ids.truncate(columns.len());
        for (ids, column) in self.column_ids.iter_mut().zip(columns) {
            if !matches!(column, Column::Parent(_) | Column::Ancestor(_)) {
                *ids = LineIds::empty();
            }
        }
        Box::new(GraphRowLineIds {
            before_link: row.before_link,
            link_line,
            after_link,
        })
    }
}

impl RowColoring {
    fn link_line_ids(&self, line: &[LinkLine]) -> Vec<LinkLineIds> {
        let mut ids = vec![LinkLineIds::default(); line.len()];
        for (index, cell) in line.iter().enumerate() {
            if index != self.node_column && cell.intersects(LinkLine::VERTICAL) {
                ids[index].vertical = self.before_link[index].clone();
            }
        }
        for (index, connection) in self.connections.iter().enumerate() {
            if connection.is_empty() {
                continue;
            }
            match index.cmp(&self.node_column) {
                Ordering::Less => {
                    ids[index].right_fork.union_with(connection);
                    ids[self.node_column].left_merge.union_with(connection);
                    for cell in &mut ids[index + 1..self.node_column] {
                        cell.horizontal.union_with(connection);
                    }
                }
                Ordering::Greater => {
                    ids[index].left_fork.union_with(connection);
                    ids[self.node_column].right_merge.union_with(connection);
                    for cell in &mut ids[self.node_column + 1..index] {
                        cell.horizontal.union_with(connection);
                    }
                }
                Ordering::Equal => {
                    ids[index].vertical.union_with(connection);
                }
            }
        }
        if let Some(parent_column) = self.moved_parent_column {
            let mut connection = &self.before_link[parent_column];
            if connection.is_empty() {
                // A reserved parent has no incoming line; use the new connection.
                connection = &self.connections[self.node_column];
            }
            ids[self.node_column].right_fork.union_with(connection);
            ids[parent_column].left_merge.union_with(connection);
            for cell in &mut ids[self.node_column + 1..parent_column] {
                cell.horizontal.union_with(connection);
            }
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::GraphRowShaper;
    use crate::pipeline::row_shape_to_prefix_lines::box_drawing::BoxDrawingPrefixLineRenderer;
    use crate::pipeline::row_shape_to_prefix_lines::box_drawing::Curved;
    use crate::pipeline::types::Ancestor;
    use crate::pipeline::types::GraphRowShape;
    use crate::pipeline::types::NodeLine;
    use crate::pipeline::types::PadLine;
    use crate::pipeline::types::PrefixLine;
    use crate::pipeline::types::PrefixLineKind;
    use crate::pipeline::types::PrefixLinePart;
    use crate::pipeline::types::PrefixLineRenderer;
    use crate::pipeline::types::TextLine;
    use crate::test_utils::indent;

    fn ids(values: impl IntoIterator<Item = usize>) -> LineIds {
        values.into_iter().collect()
    }

    // With `selected_id`, keep only strokes carrying that ID; columns and lines stay in place.
    fn render_graph(rows: &[GraphRowShape<&str>], selected_id: Option<usize>) -> String {
        let width = rows
            .iter()
            .map(|row| row.node_line.len() * 2)
            .max()
            .unwrap();
        let mut renderer = BoxDrawingPrefixLineRenderer::<Curved>::new();
        let mut out = String::new();
        for row in rows {
            let mut filtered = row.clone();
            if let Some(id) = selected_id {
                let colors = row.line_ids.as_ref().unwrap();
                // Node glyphs follow incoming IDs; outgoing IDs may start on the link line.
                for (cell, ids) in filtered.node_line.iter_mut().zip(&colors.before_link) {
                    if !ids.contains(id) {
                        *cell = NodeLine::Blank;
                    }
                }
                if let (Some(line), Some(colors)) = (&mut filtered.link_line, &colors.link_line) {
                    for (cell, colors) in line.iter_mut().zip(colors) {
                        let mut mask = LinkLine::empty();
                        for (flag, ids) in [
                            (LinkLine::VERTICAL, &colors.vertical),
                            (LinkLine::HORIZONTAL, &colors.horizontal),
                            (LinkLine::LEFT_FORK, &colors.left_fork),
                            (LinkLine::RIGHT_FORK, &colors.right_fork),
                            (LinkLine::LEFT_MERGE, &colors.left_merge),
                            (LinkLine::RIGHT_MERGE, &colors.right_merge),
                        ] {
                            if ids.contains(id) {
                                mask |= flag;
                            }
                        }
                        if mask.intersects(LinkLine::VERTICAL) {
                            mask |= LinkLine::CHILD;
                        }
                        *cell &= mask;
                    }
                }
                for (index, ids) in colors.after_link.iter().enumerate() {
                    if !ids.contains(id) {
                        filtered.ancestry_line[index] = PadLine::Blank;
                        if let Some(terms) = &mut filtered.term_line {
                            terms[index] = false;
                        }
                    }
                }
            }
            let mut prefixes = renderer.next_prefix_lines(&filtered);
            if row.has_indirect_ancestor() && !filtered.has_indirect_ancestor() {
                // Keep the original ancestry stage even when this ID has no ancestor edge.
                let index = prefixes
                    .iter()
                    .position(|line| line.kind == PrefixLineKind::PostAncestry)
                    .unwrap();
                let mut ancestry = prefixes[index].clone();
                ancestry.kind = PrefixLineKind::Ancestry;
                prefixes.insert(index, ancestry);
            }
            for line in prefixes {
                // One continuation line per row shows the outgoing IDs.
                if matches!(
                    line.kind,
                    PrefixLineKind::PreNode | PrefixLineKind::PostNode
                ) {
                    continue;
                }
                let text: String = line
                    .parts
                    .into_iter()
                    .map(|(part, _)| match part {
                        PrefixLinePart::Text(text) => text,
                        PrefixLinePart::NodeGlyph => "o".to_owned(),
                    })
                    .collect();
                out.push_str(&format!(
                    "{:width$}  # {} {:?}\n",
                    text.trim_end(),
                    row.node,
                    line.kind
                ));
            }
        }
        indent(&out)
    }

    #[test]
    fn test_default_allocator_preserves_line_id() {
        let graph = || {
            [
                ("H", vec![Ancestor::Parent("A"), Ancestor::Parent("B")]),
                ("A", vec![]),
                ("B", vec![Ancestor::Parent("C")]),
                ("C", vec![]),
            ]
        };
        let shape = |allocator| {
            let mut shaper = GraphRowShaper::new().with_line_id_allocator(allocator);
            graph()
                .into_iter()
                .map(|(node, parents)| shaper.next_row_shape(node, parents))
                .collect::<Vec<_>>()
        };
        let plain = shape(None);
        assert_eq!(
            render_graph(&plain, None),
            r#"
            o     # H Node
            ├─╮   # H Link
            │ │   # H PostAncestry
            o │   # A Node
              │   # A PostAncestry
              o   # B Node
              │   # B PostAncestry
              o   # C Node
                  # C PostAncestry"#
        );
        // A releases ID 0. B -> C keeps incoming ID 1 even though 0 is available.
        let colored = shape(build_line_id_allocator_from_rules(COLOR_RULE_PRESETS[0], 4));
        assert_eq!(
            render_graph(&colored, Some(0)),
            r#"
            o     # H Node
            │     # H Link
            │     # H PostAncestry
            o     # A Node
                  # A PostAncestry
                  # B Node
                  # B PostAncestry
                  # C Node
                  # C PostAncestry"#
        );
        assert_eq!(
            render_graph(&colored, Some(1)),
            r#"
                  # H Node
            ╰─╮   # H Link
              │   # H PostAncestry
              │   # A Node
              │   # A PostAncestry
              o   # B Node
              │   # B PostAncestry
              o   # C Node
                  # C PostAncestry"#
        );
    }

    #[test]
    fn test_graph_coloring() {
        // Z reuses the terminated anonymous edge's ID and crosses R's column move.
        let graph = || {
            [
                (
                    "M",
                    vec![
                        Ancestor::Parent("A"),
                        Ancestor::Parent("B"),
                        Ancestor::Parent("C"),
                    ],
                ),
                ("C", vec![Ancestor::Parent("R")]),
                ("B", vec![Ancestor::Parent("R"), Ancestor::Anonymous]),
                ("Z", vec![Ancestor::Parent("D")]),
                ("A", vec![Ancestor::Parent("R")]),
                ("R", vec![Ancestor::Ancestor("P")]),
                ("D", vec![]),
                ("P", vec![]),
            ]
        };
        let shape = |allocator| {
            let mut shaper = GraphRowShaper::new().with_line_id_allocator(allocator);
            shaper.reserve("P");
            graph()
                .into_iter()
                .map(|(node, parents)| shaper.next_row_shape(node, parents))
                .collect::<Vec<_>>()
        };
        let plain = shape(None);
        assert_eq!(
            render_graph(&plain, None),
            r#"
              o       # M Node
              ├─┬─╮   # M Link
              │ │ │   # M PostAncestry
              │ │ o   # C Node
              │ │ │   # C PostAncestry
              │ o │   # B Node
              │ ├─╮   # B Link
              │ │ │   # B Term
              │ ~ │   # B Term
              │   │   # B PostAncestry
              │ o │   # Z Node
              │ │ │   # Z PostAncestry
              o │ │   # A Node
              ├───╯   # A Link
              │ │     # A PostAncestry
              o │     # R Node
            ╭─╯ │     # R Link
            ╷   │     # R Ancestry
            ╷   │     # R PostAncestry
            ╷   o     # D Node
            ╷         # D Ancestry
            ╷         # D PostAncestry
            o         # P Node
                      # P PostAncestry"#
        );

        let colored = shape(build_line_id_allocator_from_rules(COLOR_RULE_PRESETS[0], 4));
        assert_eq!(
            render_graph(&colored, Some(0)),
            r#"
              o       # M Node
              │       # M Link
              │       # M PostAncestry
              │       # C Node
              │       # C PostAncestry
              │       # B Node
              │       # B Link
              │       # B Term
              │       # B Term
              │       # B PostAncestry
              │       # Z Node
              │       # Z PostAncestry
              o       # A Node
                      # A Link
                      # A PostAncestry
                      # R Node
                      # R Link
                      # R Ancestry
                      # R PostAncestry
                      # D Node
                      # D Ancestry
                      # D PostAncestry
                      # P Node
                      # P PostAncestry"#
        );
        assert_eq!(
            render_graph(&colored, Some(1)),
            r#"
                      # M Node
              ╰─╮     # M Link
                │     # M PostAncestry
                │     # C Node
                │     # C PostAncestry
                o     # B Node
                │     # B Link
                │     # B Term
                ~     # B Term
                      # B PostAncestry
                o     # Z Node
                │     # Z PostAncestry
                │     # A Node
                │     # A Link
                │     # A PostAncestry
                │     # R Node
                │     # R Link
                │     # R Ancestry
                │     # R PostAncestry
                o     # D Node
                      # D Ancestry
                      # D PostAncestry
                      # P Node
                      # P PostAncestry"#
        );
        assert_eq!(
            render_graph(&colored, Some(2)),
            r#"
                      # M Node
              ╰───╮   # M Link
                  │   # M PostAncestry
                  o   # C Node
                  │   # C PostAncestry
                  │   # B Node
                ╰─╮   # B Link
                  │   # B Term
                  │   # B Term
                  │   # B PostAncestry
                  │   # Z Node
                  │   # Z PostAncestry
                  │   # A Node
              ├───╯   # A Link
              │       # A PostAncestry
              o       # R Node
            ╭─╯       # R Link
            ╷         # R Ancestry
            ╷         # R PostAncestry
            ╷         # D Node
            ╷         # D Ancestry
            ╷         # D PostAncestry
            o         # P Node
                      # P PostAncestry"#
        );
    }

    #[test]
    fn test_reserved_parent_move() {
        let shape = |allocator| {
            let mut shaper = GraphRowShaper::new().with_line_id_allocator(allocator);
            let head = shaper.next_row_shape("H", vec![Ancestor::Parent("N")]);
            shaper.reserve("P");
            vec![
                head,
                shaper.next_row_shape("N", vec![Ancestor::Parent("P")]),
                shaper.next_row_shape("P", vec![]),
            ]
        };
        let plain = shape(None);
        assert_eq!(
            render_graph(&plain, None),
            r#"
            o     # H Node
            │     # H PostAncestry
            o     # N Node
            ├─╯   # N Link
            │     # N PostAncestry
            o     # P Node
                  # P PostAncestry"#
        );
        let colored = shape(build_line_id_allocator_from_rules(COLOR_RULE_PRESETS[0], 8));
        assert_eq!(
            render_graph(&colored, Some(0)),
            r#"
            o     # H Node
            │     # H PostAncestry
            o     # N Node
            ├─╯   # N Link
            │     # N PostAncestry
            o     # P Node
                  # P PostAncestry"#
        );
    }

    #[test]
    fn test_id_range_and_exhaustion() {
        let mut shared = GraphRowShaper::new().with_line_id_allocator(
            build_line_id_allocator_from_rules(COLOR_RULE_PRESETS[0], 70),
        );
        for node in 1..=72 {
            let colors = shared
                .next_row_shape(node, vec![Ancestor::Parent(0)])
                .line_ids
                .unwrap();
            assert_eq!(colors.after_link[0], ids([0]));
        }

        let mut shaper = GraphRowShaper::new().with_line_id_allocator(
            build_line_id_allocator_from_rules(COLOR_RULE_PRESETS[0], 70),
        );
        for node in 1..=72 {
            let colors = shaper
                .next_row_shape(node, vec![Ancestor::Parent(1000 + node)])
                .line_ids
                .unwrap();
            let allocated: LineIds = colors.after_link.iter().flat_map(LineIds::iter).collect();
            assert!(allocated.iter().all(|id| id < 70));
            assert_eq!(allocated.len(), node.min(70));
        }
        let mut disabled = GraphRowShaper::new()
            .with_line_id_allocator(build_line_id_allocator_from_rules(COLOR_RULE_PRESETS[0], 0));
        assert!(
            disabled
                .next_row_shape("A", vec![Ancestor::Parent("B")])
                .line_ids
                .is_none()
        );
    }

    #[test]
    fn test_custom_allocator() {
        let allocator: Arc<LineIdAllocator<&str>> = Arc::new(|context| {
            let target = match context.parent {
                Ancestor::Parent(parent) | Ancestor::Ancestor(parent) => parent,
                Ancestor::Anonymous => context.node,
            };
            if *target == "P" {
                0
            } else {
                (1..4)
                    .find(|id| !context.allocated_line_ids.contains(*id))
                    .unwrap_or(1)
            }
        });
        let graph = || {
            [
                ("H", vec![Ancestor::Parent("N"), Ancestor::Parent("P")]),
                ("N", vec![Ancestor::Ancestor("P")]),
                ("P", vec![]),
            ]
        };
        let shape = |allocator| {
            let mut shaper = GraphRowShaper::new().with_line_id_allocator(allocator);
            graph()
                .into_iter()
                .map(|(node, parents)| shaper.next_row_shape(node, parents))
                .collect::<Vec<_>>()
        };
        let plain = shape(None);
        assert_eq!(
            render_graph(&plain, None),
            r#"
            o     # H Node
            ├─╮   # H Link
            │ │   # H PostAncestry
            o │   # N Node
            ├─╯   # N Link
            │     # N PostAncestry
            o     # P Node
                  # P PostAncestry"#
        );
        let colored = shape(Some(allocator.clone()));
        assert_eq!(
            render_graph(&colored, Some(0)),
            r#"
                  # H Node
            ╰─╮   # H Link
              │   # H PostAncestry
              │   # N Node
            ├─╯   # N Link
            │     # N PostAncestry
            o     # P Node
                  # P PostAncestry"#
        );
        assert_eq!(
            render_graph(&colored, Some(1)),
            r#"
            o     # H Node
            │     # H Link
            │     # H PostAncestry
            o     # N Node
                  # N Link
                  # N PostAncestry
                  # P Node
                  # P PostAncestry"#
        );

        let mut other = GraphRowShaper::new().with_line_id_allocator(Some(allocator));
        let colors = other
            .next_row_shape("Q", vec![Ancestor::Parent("P")])
            .line_ids
            .unwrap();
        assert_eq!(colors.after_link, vec![ids([0])]);
    }

    #[test]
    fn test_isolated_node_does_not_allocate_line() {
        let mut shaper = GraphRowShaper::new().with_line_id_allocator(Some(Arc::new(|_| {
            panic!("isolated nodes have no parent connection")
        })));
        let colors = shaper.next_row_shape("A", vec![]).line_ids.unwrap();
        assert_eq!(colors.before_link, vec![ids([])]);
        assert_eq!(colors.after_link, vec![ids([])]);
    }

    #[test]
    fn test_next_free_cursor() {
        let mut shaper = GraphRowShaper::new()
            .with_line_id_allocator(build_line_id_allocator_from_rules(COLOR_RULE_PRESETS[1], 3));
        assert_eq!(
            shaper
                .next_row_shape("H", vec![Ancestor::Parent("A")])
                .line_ids
                .unwrap()
                .after_link[0],
            ids([0])
        );
        // Incoming wins without advancing the next-free cursor.
        assert_eq!(
            shaper
                .next_row_shape("A", vec![Ancestor::Parent("B")])
                .line_ids
                .unwrap()
                .after_link[0],
            ids([0])
        );
        let row = shaper.next_row_shape("Z", vec![Ancestor::Parent("C")]);
        assert_eq!(row.line_ids.unwrap().after_link[1], ids([1]));
        shaper.next_row_shape("B", vec![]);
        let row = shaper.next_row_shape("Q", vec![Ancestor::Parent("D")]);
        assert_eq!(row.line_ids.unwrap().after_link[0], ids([2]));
        shaper.next_row_shape("C", vec![]);
        shaper.next_row_shape("D", vec![]);
        assert_eq!(
            shaper
                .next_row_shape("R", vec![Ancestor::Parent("E")])
                .line_ids
                .unwrap()
                .after_link[0],
            ids([0])
        );

        let mut shaper = GraphRowShaper::new()
            .with_line_id_allocator(build_line_id_allocator_from_rules("next-free", 2));
        shaper.next_row_shape("H", vec![Ancestor::Parent("A")]);
        shaper.next_row_shape("J", vec![Ancestor::Parent("B")]);
        shaper.next_row_shape("Z", vec![Ancestor::Parent("C")]); // Exhausted; fallback must not move the cursor.
        for node in ["A", "B", "C"] {
            shaper.next_row_shape(node, vec![]);
        }
        assert_eq!(
            shaper
                .next_row_shape("Q", vec![Ancestor::Parent("D")])
                .line_ids
                .unwrap()
                .after_link[0],
            ids([0])
        );
    }

    #[test]
    fn test_rule_syntax_and_negation() {
        for rules in [
            "",
            " ",
            ",",
            "parent,",
            "unknown",
            "head:parent",
            "single:",
            ":parent",
            "!head:lane",
            "single:free:incoming",
        ] {
            assert!(
                build_line_id_allocator_from_rules::<usize>(rules, 3).is_none(),
                "{rules}"
            );
        }
        let mut shaper = GraphRowShaper::new().with_line_id_allocator(
            build_line_id_allocator_from_rules("!single:lane,node-lane", 3),
        );
        let row = shaper.next_row_shape("H", vec![Ancestor::Parent("A"), Ancestor::Parent("B")]);
        assert_eq!(row.line_ids.unwrap().after_link, vec![ids([0]), ids([1])]);
        let row = shaper.next_row_shape("A", vec![Ancestor::Parent("B")]);
        assert_eq!(row.line_ids.unwrap().after_link[0], ids([0, 1]));
    }

    #[test]
    fn test_column_allocator_move() {
        let mut shaper = GraphRowShaper::new()
            .with_line_id_allocator(build_line_id_allocator_from_rules(COLOR_RULE_PRESETS[2], 6));
        shaper.next_row_shape("H", vec![Ancestor::Parent("N")]);
        shaper.reserve("P");
        let row = shaper.next_row_shape("N", vec![Ancestor::Ancestor("P")]);
        assert_eq!(row.parent_columns, vec![0]);
        assert_eq!(row.line_ids.unwrap().after_link[0], ids([0]));
    }

    #[test]
    fn test_allocator_preset_limits() {
        for rules in COLOR_RULE_PRESETS {
            assert!(build_line_id_allocator_from_rules::<usize>(rules, 0).is_none());
            for limit in [1, 3] {
                let mut shaper = GraphRowShaper::new()
                    .with_line_id_allocator(build_line_id_allocator_from_rules(rules, limit));
                for node in 0..12 {
                    let row = shaper.next_row_shape(
                        node,
                        vec![Ancestor::Parent(100 + node), Ancestor::Parent(200 + node)],
                    );
                    let colors = row.line_ids.unwrap();
                    assert!(
                        colors
                            .after_link
                            .iter()
                            .flat_map(LineIds::iter)
                            .all(|id| id < limit)
                    );
                }
            }
        }
    }

    #[test]
    fn test_allocation_context() {
        let mut shaper = GraphRowShaper::new().with_line_id_allocator(Some(Arc::new(|context| {
            assert_eq!(context.node_column, 0);
            assert_eq!(
                context.parent_count,
                if *context.node == "H" { 2 } else { 1 }
            );
            match (*context.node, context.parent) {
                ("H", Ancestor::Parent("A")) => {
                    assert_eq!(context.parent_column, 0);
                    assert_eq!(context.before_link, &[ids([])]);
                    assert_eq!(context.column_line_ids, &[ids([])]);
                    assert!(context.allocated_line_ids.is_empty());
                    0
                }
                ("H", Ancestor::Parent("B")) => {
                    assert_eq!(context.parent_column, 1);
                    assert_eq!(context.before_link, &[ids([]), ids([])]);
                    assert_eq!(context.column_line_ids, &[ids([0]), ids([])]);
                    assert_eq!(context.allocated_line_ids, &ids([0]));
                    1
                }
                ("A", Ancestor::Parent("B")) => {
                    assert_eq!(context.parent_column, 1);
                    assert_eq!(context.before_link, &[ids([0]), ids([1])]);
                    assert_eq!(context.column_line_ids, &[ids([]), ids([1])]);
                    assert_eq!(context.allocated_line_ids, &ids([1]));
                    2
                }
                _ => panic!("unexpected connection"),
            }
        })));
        shaper.next_row_shape("H", vec![Ancestor::Parent("A"), Ancestor::Parent("B")]);
        let row = shaper.next_row_shape("A", vec![Ancestor::Parent("B")]);
        assert_eq!(
            row.line_ids.as_ref().unwrap().after_link,
            vec![ids([1, 2]), ids([])]
        );
        let prefixes = BoxDrawingPrefixLineRenderer::<Curved>::new().next_prefix_lines(&row);
        let continuation = prefixes
            .iter()
            .find(|line| line.kind == PrefixLineKind::PostAncestry)
            .unwrap();
        assert_eq!(continuation.parts[0].1, Some(1));
        let row = shaper.next_row_shape("B", vec![]);
        let prefixes = BoxDrawingPrefixLineRenderer::<Curved>::new().next_prefix_lines(&row);
        let node = prefixes
            .iter()
            .find(|line| line.kind == PrefixLineKind::Node)
            .unwrap();
        assert_eq!(node.parts[0], (PrefixLinePart::NodeGlyph, None));
    }

    #[test]
    fn test_line_ids_match_fixture_shapes() {
        use crate::pipeline::GraphRowShaper;
        use crate::render::LinkLine;
        use crate::render::NodeLine;
        use crate::render::PadLine;
        use crate::test_fixtures;
        use crate::test_utils::MiniDag;

        for fixture in [
            test_fixtures::BASIC,
            test_fixtures::BRANCHES_AND_MERGES,
            test_fixtures::OCTOPUS_BRANCH_AND_MERGE,
            test_fixtures::RESERVED_COLUMN,
            test_fixtures::ANCESTORS,
            test_fixtures::SPLIT_PARENTS,
            test_fixtures::TERMINATIONS,
        ] {
            let mut dag = MiniDag::from_drawdag(fixture.dag);
            for head in fixture.heads {
                dag.assign_rev(head);
            }
            let mut plain = GraphRowShaper::new();
            let mut colored = GraphRowShaper::new().with_line_id_allocator(
                build_line_id_allocator_from_rules(COLOR_RULE_PRESETS[0], 8),
            );
            for node in fixture.reserve {
                plain.reserve(node.to_string());
                colored.reserve(node.to_string());
            }
            for node in dag.all() {
                if fixture.missing.contains(&node.as_str()) {
                    continue;
                }
                let parents = || {
                    dag.parent_names(&node)
                        .into_iter()
                        .map(|parent| {
                            if fixture.missing.contains(&parent.as_str()) {
                                Ancestor::Anonymous
                            } else if fixture
                                .ancestors
                                .contains(&(node.as_str(), parent.as_str()))
                            {
                                Ancestor::Ancestor(parent)
                            } else {
                                Ancestor::Parent(parent)
                            }
                        })
                        .collect()
                };
                let mut row = colored.next_row_shape(node.clone(), parents());
                let ids = row.line_ids.take().unwrap();
                assert_eq!(row, plain.next_row_shape(node.clone(), parents()));
                row.line_ids = Some(ids);
                let ids = row.line_ids.as_ref().unwrap();
                assert_eq!(ids.before_link.len(), row.node_line.len());
                assert_eq!(ids.after_link.len(), row.ancestry_line.len());
                for (line, ids) in row.node_line.iter().zip(&ids.before_link) {
                    let isolated = *line == NodeLine::Node && row.is_head && row.is_root;
                    assert_eq!(
                        *line == NodeLine::Blank || isolated,
                        ids.is_empty(),
                        "{node}"
                    );
                }
                for (index, (line, ids)) in
                    row.ancestry_line.iter().zip(&ids.after_link).enumerate()
                {
                    let terminated = row.term_line.as_ref().is_some_and(|terms| terms[index]);
                    assert_eq!(
                        *line == PadLine::Blank && !terminated,
                        ids.is_empty(),
                        "{node}"
                    );
                }
                assert_eq!(row.link_line.is_some(), ids.link_line.is_some());
                if let (Some(line), Some(ids)) = (&row.link_line, &ids.link_line) {
                    assert_eq!(line.len(), ids.len());
                    for (cell, ids) in line.iter().zip(ids) {
                        for (flag, ids) in [
                            (LinkLine::VERTICAL, &ids.vertical),
                            (LinkLine::HORIZONTAL, &ids.horizontal),
                            (LinkLine::LEFT_FORK, &ids.left_fork),
                            (LinkLine::RIGHT_FORK, &ids.right_fork),
                            (LinkLine::LEFT_MERGE, &ids.left_merge),
                            (LinkLine::RIGHT_MERGE, &ids.right_merge),
                        ] {
                            assert_eq!(cell.intersects(flag), !ids.is_empty(), "{node}: {cell:?}");
                            assert!(ids.iter().all(|id| id < 8));
                        }
                    }
                }
            }
        }
    }
    fn strip_sgr(text: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(start) = rest.find("\x1b[") {
            out.push_str(&rest[..start]);
            rest = &rest[start + 2..];
            let end = rest.find('m').unwrap();
            rest = &rest[end + 1..];
        }
        out.push_str(rest);
        out
    }

    const COLORS: [&str; 6] = [
        "\x1b[31m", "\x1b[32m", "\x1b[33m", "\x1b[34m", "\x1b[35m", "\x1b[36m",
    ];

    // Example consumer of the structured output. Glyph styling is supplied separately.
    fn render_text_lines(lines: &[TextLine], palette: Option<&[&str]>) -> String {
        let mut out = String::new();
        for line in lines {
            for (text, ids) in line {
                if let Some(palette) = palette
                    && let Some(id) = *ids
                {
                    let leading = text.len() - text.trim_start_matches(' ').len();
                    let body = text[leading..].trim_end_matches(' ');
                    out.push_str(&text[..leading]);
                    if !body.is_empty() {
                        out.push_str(palette[id % palette.len()]);
                        out.push_str(body);
                        out.push_str("\x1b[39m");
                    }
                    out.push_str(&text[leading + body.len()..]);
                } else {
                    out.push_str(text);
                }
            }
            out.push('\n');
        }
        out
    }

    fn render_fixture_ids(
        fixture: &crate::test_fixtures::TestFixture,
        allocator: Option<Arc<LineIdAllocator<String>>>,
    ) -> String {
        use crate::pipeline::GraphTextRenderer;
        use crate::test_utils::MiniDag;

        let mut dag = MiniDag::from_drawdag(fixture.dag);
        for head in fixture.heads {
            dag.assign_rev(head);
        }
        let mut renderer = GraphTextRenderer::<String>::default()
            .with_line_id_allocator(allocator)
            .configure(|options| options.min_row_height = 0);
        for node in fixture.reserve {
            renderer.reserve(node.to_string());
        }
        let mut out = String::new();
        for node in dag.all() {
            if fixture.missing.contains(&node.as_str()) {
                continue;
            }
            let parents = dag
                .parent_names(&node)
                .into_iter()
                .map(|parent| {
                    if fixture.missing.contains(&parent.as_str()) {
                        Ancestor::Anonymous
                    } else if fixture
                        .ancestors
                        .contains(&(node.as_str(), parent.as_str()))
                    {
                        Ancestor::Ancestor(parent)
                    } else {
                        Ancestor::Parent(parent)
                    }
                })
                .collect();
            let message = fixture.messages.iter().find(|(name, _)| *name == node);
            let message =
                message.map_or_else(|| node.clone(), |(_, text)| format!("{node}\n{text}"));
            renderer.write_next_lines(
                |text, color| {
                    if let Some(id) = *color {
                        let digit = char::from_digit(id as u32, 10).unwrap();
                        out.extend(text.chars().map(|ch| if ch == ' ' { ' ' } else { digit }));
                    } else {
                        out.push_str(text);
                    }
                },
                node,
                parents,
                "o",
                &message,
            );
        }
        out
    }

    fn allocator_gallery(fixture: &crate::test_fixtures::TestFixture) -> String {
        let columns: Vec<_> = [
            ("plain", None),
            (
                "A",
                build_line_id_allocator_from_rules(COLOR_RULE_PRESETS[0], 6),
            ),
            (
                "G",
                build_line_id_allocator_from_rules(COLOR_RULE_PRESETS[1], 6),
            ),
            (
                "D",
                build_line_id_allocator_from_rules(COLOR_RULE_PRESETS[2], 6),
            ),
        ]
        .into_iter()
        .map(|(name, allocator)| {
            let text = render_fixture_ids(fixture, allocator);
            let lines: Vec<_> = text.lines().map(|line| format!("  {line}")).collect();
            (name, lines)
        })
        .collect();
        let width = columns
            .iter()
            .flat_map(|(_, lines)| lines)
            .map(|line| line.chars().count())
            .max()
            .unwrap()
            + 4;
        let mut out = columns
            .iter()
            .map(|(name, _)| format!("  {name:<w$}", w = width - 2))
            .collect::<String>();
        out.truncate(out.trim_end().len());
        out.push('\n');
        for row in 0..columns[0].1.len() {
            let line: String = columns
                .iter()
                .map(|(_, lines)| {
                    assert_eq!(lines.len(), columns[0].1.len());
                    format!("{:<width$}", lines[row])
                })
                .collect();
            out.push_str(line.trim_end());
            out.push('\n');
        }
        indent(&out)
    }

    #[test]
    fn test_allocator_gallery() {
        use crate::test_fixtures;
        assert_eq!(
            allocator_gallery(&test_fixtures::BRANCHES_AND_MERGES),
            r#"
              plain             A                 G                 D
              o  W              o  W              o  W              o  W
              o    V            o    V            o    V            o    V
              ├─╮               011               011               011
              │ o    U          0 o    U          0 o    U          0 o    U
              │ ├─╮             0 122             0 122             0 122
              │ │ o  T          0 1 o  T          0 1 o  T          0 1 o  T
              │ o │  S          0 o 2  S          0 o 2  S          0 o 2  S
              o   │  R          o   2  R          o   2  R          o   2  R
              o   │  Q          o   2  Q          o   2  Q          o   2  Q
              ├─╮ │             011 2             033 2             011 2
              │ o │    P        0 o 2    P        0 o 2    P        0 o 2    P
              │ ├───╮           0 13333           0 34444           0 13333
              │ │ │ o  O        0 1 2 o  O        0 3 2 o  O        0 1 2 o  O
              │ │ │ o    N      0 1 2 o    N      0 3 2 o    N      0 1 2 o    N
              │ │ │ ├─╮         0 1 2 344         0 3 2 455         0 1 2 344
              │ o │ │ │  M      0 o 2 3 4  M      0 o 2 4 5  M      0 o 2 3 4  M
              │ o │ │ │  L      0 o 2 3 4  L      0 o 2 4 5  L      0 o 2 3 4  L
              o │ │ │ │  K      o 1 2 3 4  K      o 3 2 4 5  K      o 1 2 3 4  K
              ├───────╯         444444444         055555555         044444444
              o │ │ │  J        o 1 2 3  J        o 3 2 4  J        o 1 2 3  J
              o │ │ │  I        o 1 2 3  I        o 3 2 4  I        o 1 2 3  I
              ├─╯ │ │           111 2 3           033 2 4           011 2 3
              o   │ │  H        o   2 3  H        o   2 4  H        o   2 3  H
              o   │ │  G        o   2 3  G        o   2 4  G        o   2 3  G
              ├─────╮           1333333           0111111           0333333
              │   │ o  F        1   2 o  F        0   2 o  F        0   2 o  F
              │   ├─╯           1   222           0   211           0   233
              │   o  E          1   o  E          0   o  E          0   o  E
              o   │  D          o   2  D          o   1  D          o   2  D
              o   │  C          o   2  C          o   1  C          o   2  C
              ├───╯             22222             01111             02222
              o  B              o  B              o  B              o  B
              o  A              o  A              o  A              o  A"#
        );
        assert_eq!(
            allocator_gallery(&test_fixtures::OCTOPUS_BRANCH_AND_MERGE),
            r#"
              plain             A                 G                 D
              o      J          o      J          o      J          o      J
              ├─┬─╮             01122             01122             01122
              │ │ o  I          0 1 o  I          0 1 o  I          0 1 o  I
              │ o │      H      0 o 2      H      0 o 2      H      0 o 2      H
              ╭─┼─┬─┬─╮         001223344         551030400         001223344
              │ │ │ │ o  G      0 1 2 3 o  G      0 1 2 4 o  G      0 1 2 3 o  G
              │ │ │ o │  E      0 1 2 o 4  E      0 1 2 o 0  E      0 1 2 o 4  E
              │ │ │ ├─╯         0 1 2 444         0 1 2 400         0 1 2 344
              │ │ o │  D        0 1 o 4  D        0 1 o 0  D        0 1 o 3  D
              │ │ ├─╮           0 1 244           0 1 233           0 1 233
              │ o │ │  C        0 o 2 4  C        0 o 2 0  C        0 o 2 3  C
              │ ├───╯           0 44444           0 10000           0 13333
              o │ │  F          o 4 2  F          o 0 2  F          o 1 2  F
              ├─╯ │             444 2             500 2             011 2
              o   │  B          o   2  B          o   2  B          o   2  B
              ├───╯             22222             02222             02222
              o  A              o  A              o  A              o  A"#
        );
        assert_eq!(
            allocator_gallery(&test_fixtures::SPLIT_PARENTS),
            r#"
              plain           A               G               D
                    o  E            o  E            o  E            o  E
              ╭─┬─┬─┤         1131210         1131210         0010203
              ╷ │ │ ╷         1 3 2 0         1 3 2 0         0 1 2 3
              ╷ o │ ╷  D      1 o 2 0  D      1 o 2 0  D      0 o 2 3  D
              ╭─┴─╮ ╷         11122 0         33344 0         00022 3
              │   │ ╷         1   2 0         1   2 0         0   2 3
              │   o ╷  C      1   o 0  C      1   o 0  C      0   o 3  C
              │   ├─╯         1   000         1   200         0   233
              o   │  B        o   0  B        o   0  B        o   2  B
              ├───╯           00000           10000           02222
              o  A            o  A            o  A            o  A"#
        );
        assert_eq!(
            allocator_gallery(&test_fixtures::ANCESTORS),
            r#"
              plain       A           G           D
                o  Z        o  Z        o  Z        o  Z
                o  Y        o  Y        o  Y        o  Y
              ╭─╯         000         000         111
              o  F        o  F        o  F        o  F
              ╷           0           0           0
              ╷ o  X      0 o  X      0 o  X      0 o  X
              ╭─╯         000         111         111
              │ o  W      0 o  W      0 o  W      0 o  W
              ├─╯         000         022         011
              o  E        o  E        o  E        o  E
              ╷           0           0           0
              o    D      o    D      o    D      o    D
              ├─╮         011         033         011
              │ ╷         0 1         0 3         0 1
              │ o  C      0 o  C      0 o  C      0 o  C
              │ ╷         0 1         0 3         0 1
              o ╷  B      o 1  B      o 3  B      o 1  B
              ├─╯         111         033         011
              o  A        o  A        o  A        o  A"#
        );
    }

    #[test]
    fn test_link_ids_follow_drawn_segments() {
        let mut shaper = GraphRowShaper::new()
            .with_line_id_allocator(build_line_id_allocator_from_rules(COLOR_RULE_PRESETS[0], 6));
        shaper.next_row_shape(
            "H",
            vec![
                Ancestor::Parent("A"),
                Ancestor::Parent("B"),
                Ancestor::Parent("C"),
            ],
        );
        let row = shaper.next_row_shape("A", vec![Ancestor::Parent("C"), Ancestor::Parent("D")]);
        let cell = &row.line_ids.as_ref().unwrap().link_line.as_ref().unwrap()[1];
        assert_eq!(cell.vertical, ids([1]));
        assert_eq!(cell.horizontal, ids([2]));
        let renderers: Vec<Box<dyn PrefixLineRenderer<&str>>> =
            vec![Box::new(BoxDrawingPrefixLineRenderer::<Curved>::new())];
        for mut renderer in renderers {
            let prefixes = renderer.next_prefix_lines(&row);
            let link = prefixes
                .iter()
                .find(|line| line.kind == PrefixLineKind::Link)
                .unwrap();
            // The crossing's glyph draws only the horizontal segment.
            assert!(link.parts.iter().any(|(part, color)| {
                matches!(part, PrefixLinePart::Text(text) if text.contains("──") || text.contains("--"))
                    && *color == Some(2)
            }));
            assert!(link.parts.iter().all(|(_, color)| *color != Some(1)));
        }
    }

    #[test]
    fn test_text_line_ids() {
        use crate::pipeline::PrefixLinesToText;

        let prefixes = vec![
            PrefixLine {
                parts: vec![
                    (PrefixLinePart::Text("│ ".to_owned()), Some(0)),
                    (PrefixLinePart::NodeGlyph, None),
                    (PrefixLinePart::Text(" ╷ ".to_owned()), Some(1)),
                ],
                kind: PrefixLineKind::Node,
            },
            PrefixLine {
                parts: vec![(PrefixLinePart::Text("│ ".to_owned()), Some(0))],
                kind: PrefixLineKind::Term,
            },
            PrefixLine {
                parts: vec![(PrefixLinePart::Text("│ ".to_owned()), Some(0))],
                kind: PrefixLineKind::PostAncestry,
            },
        ];
        let mut renderer = PrefixLinesToText::new();
        let glyph = "\x1b[34m●\x1b[39m";
        let mut lines = renderer.next_lines(prefixes.clone(), false, glyph, "M", 0);
        assert_eq!(lines[0][1], (glyph.to_owned(), None));
        assert_eq!(lines[0].last().unwrap().1, None);
        let following = renderer.next_lines(
            vec![PrefixLine {
                parts: vec![(PrefixLinePart::NodeGlyph, None)],
                kind: PrefixLineKind::Node,
            }],
            false,
            "○",
            "N",
            0,
        );
        assert_eq!(following[0], vec![("│".to_owned(), Some(0))]);
        lines.extend(following);
        let render = |palette| render_text_lines(&lines, palette);
        let plain = render(None);
        assert_eq!(
            indent(&strip_sgr(&plain)),
            r#"
            │ ● ╷  M
            │
            │
            ○ N"#
        );
        let colored = render(Some(&COLORS));
        assert_eq!(strip_sgr(&colored), strip_sgr(&plain));
        assert_eq!(
            indent(&colored),
            r#"
            [31m│[39m [34m●[39m [32m╷[39m  M
            [31m│[39m
            [31m│[39m
            ○ N"#
        );
    }

    // Snapshots contain literal SGR controls so cat and less -R show the colors.
    #[test]
    fn test_box_ansi_coloring() {
        use crate::pipeline::GraphTextRenderer;

        let render = |palette| {
            let mut renderer = GraphTextRenderer::new::<BoxDrawingPrefixLineRenderer<Curved>>()
                .with_line_id_allocator(build_line_id_allocator_from_rules(
                    COLOR_RULE_PRESETS[0],
                    6,
                ));
            let mut out = renderer.next_lines(
                "H",
                vec![Ancestor::Parent("N"), Ancestor::Parent("P")],
                "o",
                "H\nfirst\nsecond",
            );
            out.extend(renderer.next_lines(
                "N",
                vec![Ancestor::Parent("P")],
                "o",
                "N\nmerge\nlast",
            ));
            out.extend(renderer.next_lines("P", vec![], "o", "P"));
            render_text_lines(&out, palette)
        };
        let plain = render(None);
        assert_eq!(
            indent(&plain),
            r#"
            o    H
            ├─╮  first
            │ │  second
            o │  N
            ├─╯  merge
            │    last
            o  P"#
        );
        let colored = render(Some(&COLORS));
        assert_eq!(strip_sgr(&colored), plain);
        assert_eq!(
            indent(&colored),
            r#"
            o    H
            [31m├[39m[32m─╮[39m  first
            [31m│[39m [32m│[39m  second
            o [32m│[39m  N
            [32m├─╯[39m  merge
            [32m│[39m    last
            o  P"#
        );
        let colored = render(Some(&["\x1b[35m", "\x1b[36m"]));
        assert_eq!(strip_sgr(&colored), plain);
        assert_eq!(
            indent(&colored),
            r#"
            o    H
            [35m├[39m[36m─╮[39m  first
            [35m│[39m [36m│[39m  second
            o [36m│[39m  N
            [36m├─╯[39m  merge
            [36m│[39m    last
            o  P"#
        );
    }

    #[test]
    fn test_prefix_coloring_layout() {
        use crate::pipeline::PrefixLinesToText;
        use crate::pipeline::row_shape_to_prefix_lines::box_drawing::DecGraphics;
        use crate::pipeline::types::DashStyle;
        use crate::pipeline::types::GraphRowShapeOptions;
        use crate::pipeline::types::GraphRowShapeStyle;
        use crate::pipeline::types::LinkLinePlacement;

        for dash_style in [DashStyle::Persistent, DashStyle::Once] {
            for column_spacing in [0, 3] {
                for placement in [LinkLinePlacement::Top, LinkLinePlacement::Bottom] {
                    let options = GraphRowShapeOptions {
                        graph_row_shape_style: GraphRowShapeStyle {
                            dash_style,
                            column_spacing,
                            ..Default::default()
                        },
                        ..Default::default()
                    };
                    let mut shaper = GraphRowShaper::with_options(options).with_line_id_allocator(
                        build_line_id_allocator_from_rules(COLOR_RULE_PRESETS[0], 6),
                    );
                    shaper.reserve("R");
                    type PrefixRenderer = Box<dyn PrefixLineRenderer<&'static str>>;
                    let prefixes: Vec<PrefixRenderer> = vec![
                        Box::new(BoxDrawingPrefixLineRenderer::<Curved>::new()),
                        Box::new(
                            BoxDrawingPrefixLineRenderer::<Curved>::new().with_square_glyphs(),
                        ),
                        Box::new(BoxDrawingPrefixLineRenderer::<DecGraphics>::new()),
                    ];
                    let mut renderers: Vec<_> = prefixes
                        .into_iter()
                        .map(|prefix| {
                            (
                                prefix,
                                PrefixLinesToText::new().with_link_line_placement(placement),
                                PrefixLinesToText::new().with_link_line_placement(placement),
                            )
                        })
                        .collect();
                    for (node, parents) in [
                        ("H", vec![Ancestor::Parent("N"), Ancestor::Parent("P")]),
                        ("N", vec![Ancestor::Ancestor("R"), Ancestor::Anonymous]),
                        ("P", vec![Ancestor::Parent("R")]),
                        ("R", vec![]),
                    ] {
                        let row = shaper.next_row_shape(node, parents);
                        let mut untracked = row.clone();
                        untracked.line_ids = None;
                        for (prefix, plain, colored) in &mut renderers {
                            let expected = prefix.next_prefix_lines(&untracked);
                            let actual = prefix.next_prefix_lines(&row);
                            assert_eq!(
                                actual.iter().map(|line| line.kind).collect::<Vec<_>>(),
                                expected.iter().map(|line| line.kind).collect::<Vec<_>>(),
                            );
                            for (part, id) in actual.iter().flat_map(|line| &line.parts) {
                                match part {
                                    PrefixLinePart::NodeGlyph => assert!(id.is_none()),
                                    PrefixLinePart::Text(text) => {
                                        assert!(!text.contains("\x1b["));
                                        if id.is_some() {
                                            assert_eq!(text.trim_matches(' '), text);
                                        }
                                    }
                                }
                            }
                            let message = "commit\n1\n2\n3\n4\n5";
                            let render_row = |lines| {
                                PrefixLinesToText::new()
                                    .with_link_line_placement(placement)
                                    .next_text(lines, false, "●", message, 0)
                            };
                            assert_eq!(render_row(actual.clone()), render_row(expected));
                            // Reuse cached prefixes with different output styling and row state.
                            let plain_text =
                                plain.next_text(actual.clone(), false, "●", message, 0);
                            let structured = colored.next_lines(actual, false, "●", message, 0);
                            let colored_text = render_text_lines(&structured, Some(&COLORS));
                            assert_eq!(strip_sgr(&colored_text), plain_text);
                            assert!(
                                prefix
                                    .next_prefix_lines(&untracked)
                                    .iter()
                                    .flat_map(|line| &line.parts)
                                    .all(|(_, ids)| ids.is_none())
                            );
                        }
                    }
                }
            }
        }
    }
}
