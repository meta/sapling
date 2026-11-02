/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(feature = "serialize")]
use serde::Serialize;

pub use crate::render::Ancestor;
pub use crate::render::LinkLine;
pub use crate::render::NodeLine;
pub use crate::render::PadLine;

/// Style choices that affect graph row shapes and prefix-line placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serialize", derive(Serialize))]
pub struct GraphRowShapeStyle {
    /// How long indirect-ancestor dash lines remain in ancestor style.
    pub dash_style: DashStyle,

    /// Where link-related padding lines are placed around the node line.
    pub link_line_placement: LinkLinePlacement,
}

impl Default for GraphRowShapeStyle {
    fn default() -> Self {
        Self {
            dash_style: DashStyle::Persistent,
            link_line_placement: LinkLinePlacement::Top,
        }
    }
}

/// How indirect-ancestor dash lines continue across rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serialize", derive(Serialize))]
pub enum DashStyle {
    /// Keep ancestor lines in ancestor style until the branch disappears.
    ///
    /// ```plain
    ///   o
    ///   ╷
    ///   ╷
    ///   ╷
    ///   o
    /// ```
    Persistent,

    /// Render ancestor style at most once per branch, then continue as parent.
    ///
    /// ```plain
    ///   o
    ///   ┆
    ///   │
    ///   │
    ///   o
    /// ```
    Once,
}

/// Where link-line padding is placed relative to the node line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serialize", derive(Serialize))]
pub enum LinkLinePlacement {
    /// Link lines are at the top.
    ///
    /// ```plain
    ///   o
    ///   ├─╮
    ///   │ │
    ///   │ │
    /// ```
    #[default]
    Top,

    /// Link lines are at the bottom.
    ///
    /// ```plain
    ///   o
    ///   │
    ///   │
    ///   ├─╮
    /// ```
    Bottom,
}

/// Options that affect the graph row shape produced from the input node stream.
///
/// These options belong to the first pipeline stage. They may affect column
/// allocation, separator rows, and the abstract edge shape. They do not choose
/// glyph characters or place message text.
pub type GraphRowShapeOptions = crate::OutputRendererOptions;

/// An abstract graph row shape for one rendered node.
///
/// This is the output of the graph-to-rows stage. It captures the node's
/// current column, the edge shape around it, and any row-level facts needed by
/// later stages. It intentionally does not contain glyphs or message text.
///
/// Example of different type of lines, depending on the rendering style,
/// certain lines can be skipped or repeated:
///
/// ```plain
///   │                            // pre_node_line() (optional, repeatable)
///   │                            // pre_node_line()
///   o      F                     // node_line (mandory, once)
///   │      very long message 0   // post_node_line() (optional, repeatable)
///   │      very long message 0   // post_node_line()
///   ├─┬─╮  very long message 1   // link_line (mandory, once)
///   │ │ ~  very long message 2   // term_line (mandory, once)
///   : │    very long message 3   // ancestry_line (mandory, once or repeatable)
///   │ │    very long message 4   // post_ancestry_line() (optional, repeatable)
///   │ │    very long message 5   // post_ancestry_line()
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
// This is `Serialize` so non-text environment (e.g. via wasm) can get
// the rich data (than just ambiguous text characters) to render more
// precisely. Example: `RenderDag.js` in D41252231.
#[cfg_attr(feature = "serialize", derive(Serialize))]
pub struct GraphRowShape<N> {
    /// The node represented by this row.
    pub node: N,

    /// True if this row connects to multiple parents.
    ///
    /// Some prefix renderers use this to choose clearer junction glyphs.
    pub merge: bool,

    /// True if this row needs a blank separator before its graph lines.
    pub separator_line: bool,

    /// Style used to produce this row shape.
    pub style: GraphRowShapeStyle,

    /// True if this node is a head (no children, uses a new column).
    pub is_head: bool,

    /// True if this node has no parents.
    pub is_root: bool,

    /// The column containing the node glyph.
    pub node_column: usize,

    /// The columns occupied by this node's parents.
    pub parent_columns: Vec<usize>,

    /// Abstract columns for the line containing the node.
    pub node_line: Vec<NodeLine>,

    /// Abstract columns for the line connecting the node to its parents.
    ///
    /// This is absent when the node and its parents can be represented without
    /// an explicit link line.
    pub link_line: Option<Vec<LinkLine>>,

    /// The part of `link_line` that is directly emitted from this row's node.
    ///
    /// For example:
    ///
    /// ```plain
    ///   │ o   // node line
    ///   ├─╯   // link line
    /// ```
    ///
    /// The `link_line_from_node` LinkLine looks like:
    ///
    /// ```plain
    ///   ╭─╯
    /// ```
    ///
    /// Note "├" is changed to "╭".
    ///
    /// Consumers can use this to follow or highlight edges that originate from
    /// the current node without guessing from the full link line.
    pub link_line_from_node: Option<Vec<LinkLine>>,

    /// Terminator columns for anonymous parents.
    ///
    /// A `true` entry marks a column where the edge should terminate. Other
    /// columns should be rendered using the row's padding columns.
    pub term_line: Option<Vec<bool>>,

    /// Abstract columns for ancestry continuation below this row.
    pub ancestry_line: Vec<PadLine>,
}

// See `GraphRowShape` docstring for what the lines mean.
impl<N> GraphRowShape<N> {
    /// True if the ancestry line contains an indirect ancestor edge.
    pub fn has_indirect_ancestor(&self) -> bool {
        self.ancestry_line.contains(&PadLine::Ancestor)
    }

    /// Derive the repeatable padding line before the node line.
    pub fn pre_node_line(&self) -> Vec<PadLine> {
        self.node_line
            .iter()
            .map(|line| node_to_pad_line(*line, self.is_head))
            .collect()
    }

    /// Derive the repeatable padding line after the node line.
    pub fn post_node_line(&self) -> Vec<PadLine> {
        self.node_line
            .iter()
            .map(|line| node_to_pad_line(*line, self.is_root))
            .collect()
    }

    /// Derive the repeatable padding line after the ancestry line.
    pub fn post_ancestry_line(&self) -> Vec<PadLine> {
        self.ancestry_line
            .iter()
            .map(|line| match line {
                PadLine::Ancestor => PadLine::Parent,
                other => *other,
            })
            .collect()
    }
}

fn node_to_pad_line(line: NodeLine, use_blank_for_node: bool) -> PadLine {
    match line {
        NodeLine::Blank => PadLine::Blank,
        NodeLine::Ancestor => PadLine::Ancestor,
        NodeLine::Parent => PadLine::Parent,
        NodeLine::Node => {
            if use_blank_for_node {
                PadLine::Blank
            } else {
                PadLine::Parent
            }
        }
    }
}

/// A left-side graph prefix line before message text is attached.
///
/// This is the output of the row-shapes-to-prefix-lines stage. A prefix line
/// may contain a node glyph slot, but it does not know what glyph will fill
/// that slot and it does not contain any right-side message text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixLine {
    /// The ordered pieces that make up the graph prefix.
    pub parts: Vec<PrefixLinePart>,

    /// The semantic role of this line within the rendered row.
    pub kind: PrefixLineKind,
}

/// A piece of a graph prefix line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrefixLinePart {
    /// Literal graph text.
    Text(String),

    /// Placeholder for the rendered node glyph.
    ///
    /// Keeping the glyph as a slot lets callers cache prefix lines and fill in
    /// status-dependent glyphs later.
    NodeGlyph,
}

/// The semantic role of a prefix line.
///
/// The text-output stage can use this to decide where messages should attach,
/// which lines may repeat, and which lines are meaningful for consumers that do
/// not render plain text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serialize", derive(Serialize))]
pub enum PrefixLineKind {
    /// A blank line separating disconnected one-line rows.
    Separator,

    /// A repeatable line before the node line.
    PreNode,

    /// The line containing the rendered node glyph.
    Node,

    /// A repeatable line after the node line and before edge links.
    PostNode,

    /// A line connecting the node to parent or ancestor columns.
    Link,

    /// A line terminating an anonymous parent edge.
    Term,

    /// A line continuing ancestry or parent edges.
    Ancestry,

    /// A repeatable line after ancestry continuation.
    PostAncestry,
}

impl PrefixLineKind {
    /// True if this line kind can be repeated to carry additional message lines.
    pub fn is_repeatable(self) -> bool {
        matches!(
            self,
            PrefixLineKind::PreNode | PrefixLineKind::PostNode | PrefixLineKind::PostAncestry
        )
    }
}

/// Converts graph row shapes into prefix lines.
pub trait PrefixLineRenderer<N> {
    /// Convert the next graph row shape into prefix lines.
    fn next_prefix_lines(&mut self, row_shape: &GraphRowShape<N>) -> Vec<PrefixLine>;
}
