/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Line identities used for graph coloring.

#[cfg(feature = "serialize")]
use serde::Serialize;
/// A set of line identifiers for graph coloring.
pub use small_set::SmallSet as LineIds;

/// Line identities before, within, and after a row's linking stage.
///
/// IDs identify lines independently of their current column. Renderers choose
/// colors from these IDs. The column vectors must align with the row shape;
/// the shape still determines whether and how each segment is drawn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serialize", derive(Serialize))]
pub struct GraphRowLineIds {
    /// IDs for each column of the node line and its pre-/post-node padding
    /// lines, which precede the link line.
    /// Includes the node column's IDs even when the node is a head or root.
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
