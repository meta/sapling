/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::ops::Range;

use crate::column::Column;
use crate::column::ColumnsExt;
use crate::pipeline::types::Ancestor;
use crate::pipeline::types::DashStyle;
use crate::pipeline::types::GraphRowShape;
use crate::pipeline::types::GraphRowShapeOptions;
use crate::pipeline::types::LinkLine;
use crate::pipeline::types::NodeLine;
use crate::pipeline::types::PadLine;

/// Stateful renderer for the first pipeline stage.
///
/// It consumes a stream of `(node, parents)` entries and produces one
/// [`GraphRowShape`] per node. The output is purely structural: it describes
/// column placement and abstract edge shapes, without choosing glyph characters
/// or attaching message text.
pub struct GraphRowShaper<N> {
    columns: Vec<Column<N>>,
    options: GraphRowShapeOptions,
    previous_node_column: Option<usize>,
}

impl<N> GraphRowShaper<N>
where
    N: Clone + Eq,
{
    /// Create a renderer with default graph-shape options.
    pub fn new() -> Self {
        Self::with_options(Default::default())
    }

    /// Create a renderer with explicit graph-shape options.
    pub fn with_options(options: GraphRowShapeOptions) -> Self {
        Self {
            columns: Vec::new(),
            options,
            previous_node_column: None,
        }
    }

    /// Return the graph-shape options used by this renderer.
    pub fn options(&self) -> &GraphRowShapeOptions {
        &self.options
    }

    /// Return mutable graph-shape options used by this renderer.
    pub fn options_mut(&mut self) -> &mut GraphRowShapeOptions {
        &mut self.options
    }

    /// Reserve a column for a node before it is rendered.
    pub fn reserve(&mut self, node: N) {
        if self.columns.find(&node).is_none() {
            if let Some(index) = self.columns.first_empty() {
                self.columns[index] = Column::Reserved(node);
            } else {
                self.columns.push(Column::Reserved(node));
            }
        }
    }

    /// Return the number of graph columns needed after optionally considering
    /// the next node and its parents.
    pub fn width(&self, node: Option<&N>, parents: Option<&Vec<Ancestor<N>>>) -> u64 {
        self.width_with_options(node, parents, self.options)
    }

    /// Return the number of graph columns needed with explicit graph-shape
    /// options.
    pub fn width_with_options(
        &self,
        node: Option<&N>,
        parents: Option<&Vec<Ancestor<N>>>,
        options: GraphRowShapeOptions,
    ) -> u64 {
        let mut width = self.columns.len();
        let mut empty_columns = self
            .columns
            .iter()
            .filter(|&column| column == &Column::Empty)
            .count();
        if let Some(node) = node {
            // If the node is not already allocated, and there is no
            // space for the node, then adding the new node would create
            // a new column.
            if self.columns.find(node).is_none() {
                if options.min_row_height <= 1 && options.stagger_consecutive_disconnected_nodes {
                    if let Some(previous_node_column) = self.previous_node_column {
                        if self.columns.get(previous_node_column) == Some(&Column::Empty) {
                            // Dense stagger mode cannot use the previous node's column for an
                            // unallocated node, so do not count that empty column as available.
                            empty_columns = empty_columns.saturating_sub(1);
                        } else if previous_node_column == self.columns.len() {
                            // The previous node's column was trimmed from the end of the column
                            // list. To keep the new node out of that column, rendering it requires
                            // a blank placeholder column plus a new column for the node.
                            width += 1;
                        }
                    }
                }
                if empty_columns == 0 {
                    width += 1;
                } else {
                    empty_columns = empty_columns.saturating_sub(1);
                }
            }
        }
        if let Some(parents) = parents {
            // Non-allocated parents will also need a new column (except
            // for one, which can take the place of the node, and any that could be allocated to
            // empty columns).
            let unallocated_parents = parents
                .iter()
                .filter(|parent| {
                    ancestor_id(parent).is_none_or(|parent| self.columns.find(parent).is_none())
                })
                .count()
                .saturating_sub(empty_columns);
            width += unallocated_parents.saturating_sub(1);
        }
        width as u64
    }

    /// Render the next node into an abstract graph row shape.
    pub fn next_row_shape(&mut self, node: N, parents: Vec<Ancestor<N>>) -> GraphRowShape<N> {
        let existing_column = self.columns.find(&node);
        let column = existing_column.unwrap_or_else(|| self.find_column_for_unallocated_node());
        let is_head = existing_column
            .is_none_or(|index| matches!(self.columns.get(index), Some(Column::Reserved(_))));
        let is_root = parents.is_empty();
        self.columns[column] = Column::Empty;

        let merge = parents.len() > 1;

        let mut node_line: Vec<_> = self.columns.iter().map(column_to_node_line).collect();
        node_line[column] = NodeLine::Node;

        let mut link_line: Vec<_> = self.columns.iter().map(column_to_link_line).collect();
        let mut need_link_line = false;

        let mut term_line: Vec<_> = self.columns.iter().map(|_| false).collect();
        let mut need_term_line = false;

        let mut ancestry_line: Vec<_> = self.columns.iter().map(column_to_pad_line).collect();

        let mut parent_columns = BTreeMap::new();
        for parent in parents.iter() {
            if let Some(parent_id) = ancestor_id(parent) {
                if let Some(index) = self.columns.find(parent_id) {
                    self.columns[index].merge(&ancestor_to_column(parent));
                    parent_columns.insert(index, parent);
                    continue;
                }
            }

            if let Some(index) = self.columns.find_empty(column) {
                self.columns[index].merge(&ancestor_to_column(parent));
                parent_columns.insert(index, parent);
                continue;
            }

            parent_columns.insert(self.columns.len(), parent);
            node_line.push(NodeLine::Blank);
            ancestry_line.push(PadLine::Blank);
            link_line.push(LinkLine::default());
            term_line.push(false);
            self.columns.push(ancestor_to_column(parent));
        }

        for (index, parent) in parent_columns.iter() {
            if ancestor_id(parent).is_none() {
                term_line[*index] = true;
                need_term_line = true;
            }
        }

        let separator_line = existing_column.is_none()
            && self.options.min_row_height <= 1
            && !self.options.stagger_consecutive_disconnected_nodes
            && Some(column) == self.previous_node_column
            && !need_term_line;

        if parents.len() == 1 {
            if let Some((&parent_column, _)) = parent_columns.iter().next() {
                if parent_column > column {
                    self.columns.swap(column, parent_column);
                    if let Some(parent) = parent_columns.remove(&parent_column) {
                        parent_columns.insert(column, parent);
                    }

                    let was_direct = link_line[parent_column].contains(LinkLine::VERT_PARENT);
                    link_line[column] |= if was_direct {
                        LinkLine::RIGHT_FORK_PARENT
                    } else {
                        LinkLine::RIGHT_FORK_ANCESTOR
                    };
                    #[allow(clippy::needless_range_loop)]
                    for i in column + 1..parent_column {
                        link_line[i] |= if was_direct {
                            LinkLine::HORIZ_PARENT
                        } else {
                            LinkLine::HORIZ_ANCESTOR
                        };
                    }
                    link_line[parent_column] = if was_direct {
                        LinkLine::LEFT_MERGE_PARENT
                    } else {
                        LinkLine::LEFT_MERGE_ANCESTOR
                    };
                    need_link_line = true;
                    ancestry_line[parent_column] = PadLine::Blank;
                }
            }
        }

        if let Some(bounds) = AncestorColumnBounds::new(&parent_columns, column) {
            for i in bounds.range() {
                link_line[i] |= bounds.horizontal_line(i);
                need_link_line = true;
            }

            if bounds.max_parent > column {
                link_line[column] |= LinkLine::RIGHT_MERGE_PARENT;
                need_link_line = true;
            } else if bounds.max_ancestor > column {
                link_line[column] |= LinkLine::RIGHT_MERGE_ANCESTOR;
                need_link_line = true;
            }

            if bounds.min_parent < column {
                link_line[column] |= LinkLine::LEFT_MERGE_PARENT;
                need_link_line = true;
            } else if bounds.min_ancestor < column {
                link_line[column] |= LinkLine::LEFT_MERGE_ANCESTOR;
                need_link_line = true;
            }

            #[allow(clippy::comparison_chain)]
            for (&index, parent) in parent_columns.iter() {
                ancestry_line[index] = column_to_pad_line(&self.columns[index]);
                if index < column {
                    link_line[index] |= ancestor_to_link_line(
                        parent,
                        LinkLine::RIGHT_FORK_PARENT,
                        LinkLine::RIGHT_FORK_ANCESTOR,
                    );
                } else if index == column {
                    link_line[index] |= LinkLine::CHILD
                        | ancestor_to_link_line(
                            parent,
                            LinkLine::VERT_PARENT,
                            LinkLine::VERT_ANCESTOR,
                        );
                } else {
                    link_line[index] |= ancestor_to_link_line(
                        parent,
                        LinkLine::LEFT_FORK_PARENT,
                        LinkLine::LEFT_FORK_ANCESTOR,
                    );
                }
            }
        }

        if self.options.graph_row_shape_style.dash_style == DashStyle::Once {
            self.columns.convert_ancestor_to_parent();
        }
        self.columns.reset();
        self.previous_node_column = Some(column);

        GraphRowShape {
            node,
            merge,
            is_head,
            is_root,
            style: self.options.graph_row_shape_style,
            node_column: column,
            parent_columns: parent_columns.keys().copied().collect(),
            node_line,
            link_line: Some(link_line).filter(|_| need_link_line),
            term_line: Some(term_line).filter(|_| need_term_line),
            ancestry_line,
            separator_line,
        }
    }

    fn find_column_for_unallocated_node(&mut self) -> usize {
        if self.options.min_row_height <= 1 && self.options.stagger_consecutive_disconnected_nodes {
            if let Some(index) = self.columns.iter().enumerate().find_map(|(index, column)| {
                (*column == Column::Empty && Some(index) != self.previous_node_column)
                    .then_some(index)
            }) {
                index
            } else {
                if self.previous_node_column == Some(self.columns.len()) {
                    self.columns.push(Column::Empty);
                }
                self.columns.new_empty()
            }
        } else {
            self.columns
                .first_empty()
                .unwrap_or_else(|| self.columns.new_empty())
        }
    }
}

impl<N> Default for GraphRowShaper<N>
where
    N: Clone + Eq,
{
    fn default() -> Self {
        Self::new()
    }
}

struct AncestorColumnBounds {
    target: usize,
    min_ancestor: usize,
    min_parent: usize,
    max_parent: usize,
    max_ancestor: usize,
}

impl AncestorColumnBounds {
    fn new<N>(columns: &BTreeMap<usize, &Ancestor<N>>, target: usize) -> Option<Self> {
        if columns.is_empty() {
            return None;
        }
        let min_ancestor = columns
            .iter()
            .next()
            .map_or(target, |(index, _)| *index)
            .min(target);
        let max_ancestor = columns
            .iter()
            .next_back()
            .map_or(target, |(index, _)| *index)
            .max(target);
        let min_parent = columns
            .iter()
            .find(|(_, ancestor)| ancestor_is_direct(ancestor))
            .map_or(target, |(index, _)| *index)
            .min(target);
        let max_parent = columns
            .iter()
            .rev()
            .find(|(_, ancestor)| ancestor_is_direct(ancestor))
            .map_or(target, |(index, _)| *index)
            .max(target);
        Some(Self {
            target,
            min_ancestor,
            min_parent,
            max_parent,
            max_ancestor,
        })
    }

    fn range(&self) -> Range<usize> {
        if self.min_ancestor < self.max_ancestor {
            self.min_ancestor + 1..self.max_ancestor
        } else {
            Default::default()
        }
    }

    fn horizontal_line(&self, index: usize) -> LinkLine {
        if index == self.target {
            LinkLine::empty()
        } else if index > self.min_parent && index < self.max_parent {
            LinkLine::HORIZ_PARENT
        } else if index > self.min_ancestor && index < self.max_ancestor {
            LinkLine::HORIZ_ANCESTOR
        } else {
            LinkLine::empty()
        }
    }
}

fn ancestor_to_column<N>(ancestor: &Ancestor<N>) -> Column<N>
where
    N: Clone,
{
    match ancestor {
        Ancestor::Ancestor(node) => Column::Ancestor(node.clone()),
        Ancestor::Parent(node) => Column::Parent(node.clone()),
        Ancestor::Anonymous => Column::Blocked,
    }
}

fn ancestor_id<N>(ancestor: &Ancestor<N>) -> Option<&N> {
    match ancestor {
        Ancestor::Ancestor(node) => Some(node),
        Ancestor::Parent(node) => Some(node),
        Ancestor::Anonymous => None,
    }
}

fn ancestor_is_direct<N>(ancestor: &Ancestor<N>) -> bool {
    match ancestor {
        Ancestor::Ancestor(_) => false,
        Ancestor::Parent(_) => true,
        Ancestor::Anonymous => true,
    }
}

fn ancestor_to_link_line<N>(
    ancestor: &Ancestor<N>,
    direct: LinkLine,
    indirect: LinkLine,
) -> LinkLine {
    if ancestor_is_direct(ancestor) {
        direct
    } else {
        indirect
    }
}

fn column_to_node_line<N>(column: &Column<N>) -> NodeLine {
    match column {
        Column::Ancestor(_) => NodeLine::Ancestor,
        Column::Parent(_) => NodeLine::Parent,
        _ => NodeLine::Blank,
    }
}

fn column_to_link_line<N>(column: &Column<N>) -> LinkLine {
    match column {
        Column::Ancestor(_) => LinkLine::VERT_ANCESTOR,
        Column::Parent(_) => LinkLine::VERT_PARENT,
        _ => LinkLine::empty(),
    }
}

fn column_to_pad_line<N>(column: &Column<N>) -> PadLine {
    match column {
        Column::Ancestor(_) => PadLine::Ancestor,
        Column::Parent(_) => PadLine::Parent,
        _ => PadLine::Blank,
    }
}
