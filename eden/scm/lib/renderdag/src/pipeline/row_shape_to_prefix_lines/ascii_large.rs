/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use super::ancestry_lines;
use super::column_part;
use super::column_parts;
use super::connector_columns;
use super::is_horizontal_run;
use crate::pipeline::types::DashStyle;
use crate::pipeline::types::GraphRowShape;
use crate::pipeline::types::LinkLine;
use crate::pipeline::types::NodeLine;
use crate::pipeline::types::PadLine;
use crate::pipeline::types::PrefixLine;
use crate::pipeline::types::PrefixLineKind;
use crate::pipeline::types::PrefixLinePart;
use crate::pipeline::types::PrefixLineRenderer;

/// Converts abstract row shapes into large ASCII graph prefix lines.
#[derive(Default)]
pub struct AsciiLargePrefixLineRenderer;

impl AsciiLargePrefixLineRenderer {
    /// Create a large ASCII prefix line renderer.
    pub fn new() -> Self {
        Self
    }
}

impl<N> PrefixLineRenderer<N> for AsciiLargePrefixLineRenderer {
    /// Convert the next graph row shape into prefix lines.
    fn next_prefix_lines(&mut self, line: &GraphRowShape<N>) -> Vec<PrefixLine> {
        let mut lines = Vec::new();
        let (ancestor, ancestor_line) = match line.style.dash_style {
            DashStyle::Persistent => ('.', ". "),
            DashStyle::Once => (':', ": "),
        };

        lines.push(render_pad_line(
            ancestor_line,
            line.pre_node_line(),
            PrefixLineKind::PreNode,
        ));

        // Render the nodeline
        let mut node_line = PrefixLine {
            parts: Vec::new(),
            kind: PrefixLineKind::Node,
        };
        for (i, entry) in line.node_line.iter().enumerate() {
            match entry {
                NodeLine::Node => {
                    node_line.parts.extend(gap(i));
                    node_line.parts.push(PrefixLinePart::NodeGlyph);
                    node_line
                        .parts
                        .push(PrefixLinePart::Text(String::from(" ")));
                }
                NodeLine::Parent => node_line.parts.extend(cell(i, "| ")),
                NodeLine::Ancestor => node_line.parts.extend(cell(i, ancestor_line)),
                NodeLine::Blank => node_line.parts.extend(cell(i, "  ")),
            }
        }
        lines.push(node_line);

        lines.push(render_pad_line(
            ancestor_line,
            line.post_node_line(),
            PrefixLineKind::PostNode,
        ));

        // Render the link line
        if let Some(link_row) = line.link_line.as_ref() {
            let mut top_parts = Vec::new();
            let mut bot_parts = Vec::new();
            let mut previous_column = None;
            for (i, (cur, right_column)) in link_row
                .iter()
                .zip(connector_columns(line.node_column, link_row))
                .enumerate()
            {
                let mut top_link_line = String::new();
                let mut bot_link_line = String::new();
                // The first column has no left slot.
                let left_column = previous_column.replace(right_column);
                let center_column =
                    if !cur.intersects(LinkLine::VERTICAL) && is_horizontal_run(*cur) {
                        right_column
                    } else {
                        i
                    };
                // Top left
                if i > 0 {
                    if cur.intersects(LinkLine::LEFT_MERGE_PARENT) {
                        top_link_line.push('/');
                    } else if cur.intersects(LinkLine::LEFT_MERGE_ANCESTOR) {
                        top_link_line.push('.');
                    } else if cur.intersects(LinkLine::HORIZ_PARENT) {
                        top_link_line.push('_');
                    } else if cur.intersects(LinkLine::HORIZ_ANCESTOR) {
                        top_link_line.push('.');
                    } else {
                        top_link_line.push(' ');
                    }
                }

                // Top center
                if cur.intersects(LinkLine::VERT_PARENT) {
                    top_link_line.push('|');
                } else if cur.intersects(LinkLine::VERT_ANCESTOR) {
                    top_link_line.push(ancestor);
                } else if cur.intersects(LinkLine::ANY_MERGE) {
                    top_link_line.push(' ');
                } else if cur.intersects(LinkLine::HORIZ_PARENT) {
                    top_link_line.push('_');
                } else if cur.intersects(LinkLine::HORIZ_ANCESTOR) {
                    top_link_line.push('.');
                } else {
                    top_link_line.push(' ');
                }

                // Top right
                if cur.intersects(LinkLine::RIGHT_MERGE_PARENT) {
                    top_link_line.push('\\');
                } else if cur.intersects(LinkLine::RIGHT_MERGE_ANCESTOR) {
                    top_link_line.push('.');
                } else if cur.intersects(LinkLine::HORIZ_PARENT) {
                    top_link_line.push('_');
                } else if cur.intersects(LinkLine::HORIZ_ANCESTOR) {
                    top_link_line.push('.');
                } else {
                    top_link_line.push(' ');
                }

                // Bottom left
                if i > 0 {
                    if cur.intersects(LinkLine::LEFT_FORK_PARENT) {
                        bot_link_line.push('\\');
                    } else if cur.intersects(LinkLine::LEFT_FORK_ANCESTOR) {
                        bot_link_line.push('.');
                    } else {
                        bot_link_line.push(' ');
                    }
                }

                // Bottom center
                if cur.intersects(LinkLine::VERT_PARENT) {
                    bot_link_line.push('|');
                } else if cur.intersects(LinkLine::VERT_ANCESTOR) {
                    bot_link_line.push(ancestor);
                } else {
                    bot_link_line.push(' ');
                }

                // Bottom Right
                if cur.intersects(LinkLine::RIGHT_FORK_PARENT) {
                    bot_link_line.push('/');
                } else if cur.intersects(LinkLine::RIGHT_FORK_ANCESTOR) {
                    bot_link_line.push('.');
                } else {
                    bot_link_line.push(' ');
                }

                let top_columns = left_column.into_iter().chain([center_column, right_column]);
                top_parts.extend(column_parts(&top_link_line, top_columns));
                let bot_columns = left_column.into_iter().chain([i, right_column]);
                bot_parts.extend(column_parts(&bot_link_line, bot_columns));
            }
            lines.push(PrefixLine {
                parts: top_parts,
                kind: PrefixLineKind::Link,
            });
            lines.push(PrefixLine {
                parts: bot_parts,
                kind: PrefixLineKind::Link,
            });
        }

        // Render the term line
        if let Some(term_row) = line.term_line.as_ref() {
            let term_strs = ["| ", "~ "];
            for term_str in term_strs.iter() {
                let parts = term_row
                    .iter()
                    .enumerate()
                    .flat_map(|(i, term)| {
                        let text = if *term {
                            term_str
                        } else {
                            pad_cell(ancestor_line, line.ancestry_line[i])
                        };
                        cell(i, text)
                    })
                    .collect();
                lines.push(PrefixLine {
                    parts,
                    kind: PrefixLineKind::Term,
                });
            }
        }

        for (kind, columns) in ancestry_lines(line) {
            lines.push(render_pad_line(ancestor_line, columns, kind));
        }

        lines
    }
}

fn render_pad_line(
    ancestor: &str,
    line: impl IntoIterator<Item = PadLine>,
    kind: PrefixLineKind,
) -> PrefixLine {
    let parts = line
        .into_iter()
        .enumerate()
        .flat_map(|(i, entry)| cell(i, pad_cell(ancestor, entry)))
        .collect();
    PrefixLine { parts, kind }
}

fn pad_cell(ancestor: &str, entry: PadLine) -> &str {
    match entry {
        PadLine::Parent => "| ",
        PadLine::Ancestor => ancestor,
        PadLine::Blank => "  ",
    }
}

/// The space before every column but the first. The first column is only 2 characters wide.
fn gap(column: usize) -> Option<PrefixLinePart> {
    (column > 0).then(|| PrefixLinePart::Text(String::from(" ")))
}

/// A 2-character cell for `column`, after its gap.
fn cell(column: usize, text: &str) -> impl Iterator<Item = PrefixLinePart> {
    gap(column).into_iter().chain([column_part(column, text)])
}
