/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

pub mod ascii;
pub mod ascii_large;
pub mod box_drawing;

use crate::pipeline::types::DashStyle;
use crate::pipeline::types::GraphRowShape;
use crate::pipeline::types::LinkLine;
use crate::pipeline::types::PadLine;
use crate::pipeline::types::PrefixLine;
use crate::pipeline::types::PrefixLineKind;
use crate::pipeline::types::PrefixLinePart;

fn ancestry_lines<N>(
    row: &GraphRowShape<N>,
) -> impl Iterator<Item = (PrefixLineKind, impl Iterator<Item = PadLine> + '_)> + '_ {
    [
        row.has_indirect_ancestor()
            .then_some(PrefixLineKind::Ancestry),
        Some(PrefixLineKind::PostAncestry),
    ]
    .into_iter()
    .flatten()
    .map(move |kind| {
        let solid = kind == PrefixLineKind::PostAncestry && row.style.dash_style == DashStyle::Once;
        let columns = row.ancestry_line.iter().copied().map(move |pad| {
            if solid && pad == PadLine::Ancestor {
                PadLine::Parent
            } else {
                pad
            }
        });
        (kind, columns)
    })
}

/// True if a link line cell only carries a horizontal line past its column.
fn is_horizontal_run(cell: LinkLine) -> bool {
    cell.intersects(LinkLine::HORIZONTAL)
        && !cell.intersects(LinkLine::CHILD | LinkLine::ANY_FORK_OR_MERGE)
}

/// Find the owning column of each link line connector, scanning each horizontal run only once.
///
/// A connector joins two columns. It belongs to the column whose line ends in this link line. If
/// neither line or both lines end here, it belongs to the column that is farther from the node,
/// which is the branch that forks or merges.
fn connector_columns(
    node_column: usize,
    link_row: &[LinkLine],
) -> impl Iterator<Item = usize> + '_ {
    let mut right = 0;
    let mut owner = 0;
    (0..link_row.len()).map(move |segment| {
        if segment + 1 == link_row.len() {
            return segment;
        }
        // Every connector before the right endpoint has the same owner.
        if right <= segment {
            let left = segment;
            right = segment + 1;
            while right + 1 < link_row.len() && is_horizontal_run(link_row[right]) {
                right += 1;
            }
            owner = match (
                link_row[left].intersects(LinkLine::VERTICAL),
                link_row[right].intersects(LinkLine::VERTICAL),
            ) {
                (true, false) => right,
                (false, true) => left,
                _ if node_column <= left => right,
                _ => left,
            };
        }
        owner
    })
}

/// Make a part for graph text that draws part of the edge in `column`.
fn column_part(column: usize, text: impl Into<String>) -> PrefixLinePart {
    let text = text.into();
    if text.trim().is_empty() {
        PrefixLinePart::Text(text)
    } else {
        PrefixLinePart::ColumnText { column, text }
    }
}

/// Append graph text, reusing the last part when its column ownership matches.
#[inline]
fn push_column_part(parts: &mut Vec<PrefixLinePart>, column: usize, text: &str) {
    let column = (!text.trim().is_empty()).then_some(column);
    match (parts.last_mut(), column) {
        (Some(PrefixLinePart::Text(last)), None) => last.push_str(text),
        (
            Some(PrefixLinePart::ColumnText {
                column: last_column,
                text: last,
            }),
            Some(column),
        ) if *last_column == column => last.push_str(text),
        (_, None) => parts.push(PrefixLinePart::Text(text.to_owned())),
        (_, Some(column)) => parts.push(PrefixLinePart::ColumnText {
            column,
            text: text.to_owned(),
        }),
    }
}

/// Render one cell per column, coalescing adjacent parts with the same ownership.
#[inline]
fn column_line<'a>(kind: PrefixLineKind, cells: impl IntoIterator<Item = &'a str>) -> PrefixLine {
    let cells = cells.into_iter();
    let mut parts = Vec::with_capacity(cells.size_hint().0.min(4));
    for (column, text) in cells.enumerate() {
        push_column_part(&mut parts, column, text);
    }
    PrefixLine { parts, kind }
}

/// Make the parts for graph text that has one character for each of `columns`.
fn column_parts(
    text: &str,
    columns: impl IntoIterator<Item = usize>,
) -> impl Iterator<Item = PrefixLinePart> {
    text.chars()
        .zip(columns)
        .map(|(c, column)| column_part(column, c))
}
