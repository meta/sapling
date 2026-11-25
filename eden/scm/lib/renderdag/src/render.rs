/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use bitflags::bitflags;
#[cfg(feature = "serialize")]
use serde::Serialize;

use super::output::OutputRendererBuilder;
use super::output::OutputRendererOptions;
use super::pipeline::graph_to_row_shape::GraphRowShaper;

pub trait Renderer<N> {
    type Output;

    // Returns the width of the graph line, possibly including another node.
    fn width(&self, new_node: Option<&N>, new_parents: Option<&Vec<Ancestor<N>>>) -> u64;

    // Reserve a column for the given node.
    fn reserve(&mut self, node: N);

    // Render the next row.
    fn next_row(
        &mut self,
        node: N,
        parents: Vec<Ancestor<N>>,
        glyph: String,
        message: String,
    ) -> Self::Output;

    // Set the output options.
    fn output_options_mut(&mut self) -> &mut OutputRendererOptions;

    // Get the output options.
    fn output_options(&self) -> &OutputRendererOptions;
}

/// Renderer for a DAG.
///
/// Converts a sequence of DAG node descriptions into rendered graph rows.
pub struct GraphRowRenderer<N> {
    inner: GraphRowShaper<N>,
    output_options: OutputRendererOptions,
}

/// Ancestor type indication for an ancestor or parent node.
pub enum Ancestor<N> {
    /// The node is an eventual ancestor.
    Ancestor(N),

    /// The node is an immediate parent.
    Parent(N),

    /// The node is an anonymous ancestor.
    Anonymous,
}

/// A column in the node row.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serialize", derive(Serialize))]
pub enum NodeLine {
    /// Blank.
    Blank,

    /// Vertical line indicating an ancestor.
    Ancestor,

    /// Vertical line indicating a parent.
    Parent,

    /// The node for this row.
    Node,
}

/// A column in a padding row.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serialize", derive(Serialize))]
pub enum PadLine {
    /// Blank.
    Blank,

    /// Vertical line indicating an ancestor.
    Ancestor,

    /// Vertical line indicating a parent.
    Parent,
}

bitflags! {
    /// A column in a linking row.
    #[cfg_attr(feature = "serialize", derive(Serialize))]
    #[derive(Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Clone, Copy)]
    pub struct LinkLine: u16 {
        /// This cell contains a horizontal line that connects to a parent.
        const HORIZ_PARENT = 0b0_0000_0000_0001;

        /// This cell contains a horizontal line that connects to an ancestor.
        const HORIZ_ANCESTOR = 0b0_0000_0000_0010;

        /// The descendent of this cell is connected to the parent.
        const VERT_PARENT = 0b0_0000_0000_0100;

        /// The descendent of this cell is connected to an ancestor.
        const VERT_ANCESTOR = 0b0_0000_0000_1000;

        /// The parent of this cell is linked in this link row and the child
        /// is to the left.
        const LEFT_FORK_PARENT = 0b0_0000_0001_0000;

        /// The ancestor of this cell is linked in this link row and the child
        /// is to the left.
        const LEFT_FORK_ANCESTOR = 0b0_0000_0010_0000;

        /// The parent of this cell is linked in this link row and the child
        /// is to the right.
        const RIGHT_FORK_PARENT = 0b0_0000_0100_0000;

        /// The ancestor of this cell is linked in this link row and the child
        /// is to the right.
        const RIGHT_FORK_ANCESTOR = 0b0_0000_1000_0000;

        /// The child of this cell is linked to parents on the left.
        const LEFT_MERGE_PARENT = 0b0_0001_0000_0000;

        /// The child of this cell is linked to ancestors on the left.
        const LEFT_MERGE_ANCESTOR = 0b0_0010_0000_0000;

        /// The child of this cell is linked to parents on the right.
        const RIGHT_MERGE_PARENT = 0b0_0100_0000_0000;

        /// The child of this cell is linked to ancestors on the right.
        const RIGHT_MERGE_ANCESTOR = 0b0_1000_0000_0000;

        /// The target node of this link line is the child of this column.
        /// This disambiguates between the node that is connected in this link
        /// line, and other nodes that are also connected vertically.
        const CHILD = 0b1_0000_0000_0000;

        const HORIZONTAL = Self::HORIZ_PARENT.bits() | Self::HORIZ_ANCESTOR.bits();
        const VERTICAL = Self::VERT_PARENT.bits() | Self::VERT_ANCESTOR.bits();
        const LEFT_FORK = Self::LEFT_FORK_PARENT.bits() | Self::LEFT_FORK_ANCESTOR.bits();
        const RIGHT_FORK = Self::RIGHT_FORK_PARENT.bits() | Self::RIGHT_FORK_ANCESTOR.bits();
        const LEFT_MERGE = Self::LEFT_MERGE_PARENT.bits() | Self::LEFT_MERGE_ANCESTOR.bits();
        const RIGHT_MERGE = Self::RIGHT_MERGE_PARENT.bits() | Self::RIGHT_MERGE_ANCESTOR.bits();
        const ANY_MERGE = Self::LEFT_MERGE.bits() | Self::RIGHT_MERGE.bits();
        const ANY_FORK = Self::LEFT_FORK.bits() | Self::RIGHT_FORK.bits();
        const ANY_FORK_OR_MERGE = Self::ANY_MERGE.bits() | Self::ANY_FORK.bits();
    }
}

impl LinkLine {
    /// Whether two lines pass through this cell without joining.
    pub(crate) fn is_crossing(self) -> bool {
        self.intersects(Self::HORIZONTAL)
            && self.intersects(Self::VERTICAL)
            && !self.intersects(Self::CHILD | Self::ANY_FORK_OR_MERGE)
    }
}

/// An output graph row.
///
/// This "Row" contains a "node" and its surrounding edges. It does not define
/// precisely how many lines this row should have. Lines characters are abstract
/// so the actual rendering logic (svg, box drawing, etc) can decide what to
/// use.
///
/// ```plain
///                          // separator line
///  o      F                // node line
///  ├─┬─╮  long message 1   // link line
///  ╷ │ ~  long message 2   // term line
///  ╷ │    long message 3   // pad line
///  ╷ │    long message 4   // pad line
/// ```
#[derive(Debug)]
#[cfg_attr(feature = "serialize", derive(Serialize))]
pub struct GraphRow<N> {
    /// The name of the node for this row.
    pub node: N,

    /// The glyph for this node.
    pub glyph: String,

    /// The message for this row.
    pub message: String,

    /// True if this row is for a merge commit.
    pub merge: bool,

    /// True if this row needs a blank-line separator from the previous row.
    pub separator_line: bool,

    /// The node columns for this row.
    pub node_line: Vec<NodeLine>,

    /// The link columns for this row, if a link row is necessary.
    pub link_line: Option<Vec<LinkLine>>,

    /// The location of any terminators, if necessary.  Other columns should be
    /// filled in with pad lines.
    pub term_line: Option<Vec<bool>>,

    /// The pad columns for this row.
    pub pad_lines: Vec<PadLine>,
}

impl<N> GraphRowRenderer<N>
where
    N: Clone + Eq + std::fmt::Debug,
{
    /// Create a new renderer.
    pub fn new() -> Self {
        GraphRowRenderer {
            inner: GraphRowShaper::new(),
            output_options: Default::default(),
        }
    }

    /// Build an output renderer from this renderer.
    pub fn output(self) -> OutputRendererBuilder<N, Self> {
        OutputRendererBuilder::new(self)
    }
}

impl<N> Renderer<N> for GraphRowRenderer<N>
where
    N: Clone + Eq + std::fmt::Debug,
{
    type Output = GraphRow<N>;

    fn width(&self, node: Option<&N>, parents: Option<&Vec<Ancestor<N>>>) -> u64 {
        self.inner
            .width_with_options(node, parents, self.output_options)
    }

    fn reserve(&mut self, node: N) {
        self.inner.reserve(node);
    }

    fn next_row(
        &mut self,
        node: N,
        parents: Vec<Ancestor<N>>,
        glyph: String,
        message: String,
    ) -> GraphRow<N> {
        *self.inner.options_mut() = self.output_options;
        let row = self.inner.next_row_shape(node, parents);

        GraphRow {
            node: row.node,
            glyph,
            message,
            merge: row.merge,
            node_line: row.node_line,
            link_line: row.link_line,
            term_line: row.term_line,
            pad_lines: row.ancestry_line,
            separator_line: row.separator_line,
        }
    }

    // Set the output options.
    fn output_options_mut(&mut self) -> &mut OutputRendererOptions {
        &mut self.output_options
    }

    // Get the output options.
    fn output_options(&self) -> &OutputRendererOptions {
        &self.output_options
    }
}
