/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use super::ancestry_lines;
use super::column_line;
use super::connector_columns;
use super::is_horizontal_run;
use super::push_column_part;
use crate::pipeline::types::DashStyle;
use crate::pipeline::types::GraphRowShape;
use crate::pipeline::types::LinkLine;
use crate::pipeline::types::NodeLine;
use crate::pipeline::types::PadLine;
use crate::pipeline::types::PrefixLine;
use crate::pipeline::types::PrefixLineKind;
use crate::pipeline::types::PrefixLinePart;
use crate::pipeline::types::PrefixLineRenderer;

/// Converts abstract row shapes into ASCII graph prefix lines.
#[derive(Default)]
pub struct AsciiPrefixLineRenderer;

impl AsciiPrefixLineRenderer {
    /// Create an ASCII prefix line renderer.
    pub fn new() -> Self {
        Self
    }
}

impl<N> PrefixLineRenderer<N> for AsciiPrefixLineRenderer {
    /// Convert the next graph row shape into prefix lines.
    fn next_prefix_lines(&mut self, line: &GraphRowShape<N>) -> Vec<PrefixLine> {
        let mut lines = Vec::new();
        let (ancestor, ancestor_line) = match line.style.dash_style {
            DashStyle::Persistent => (".", ". "),
            DashStyle::Once => (":", ": "),
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
                    node_line.parts.push(PrefixLinePart::NodeGlyph);
                    node_line
                        .parts
                        .push(PrefixLinePart::Text(String::from(" ")));
                }
                NodeLine::Parent => push_column_part(&mut node_line.parts, i, "| "),
                NodeLine::Ancestor => push_column_part(&mut node_line.parts, i, ancestor_line),
                NodeLine::Blank => push_column_part(&mut node_line.parts, i, "  "),
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
            let mut parts = Vec::new();
            let any_horizontal = link_row
                .iter()
                .any(|cur| cur.intersects(LinkLine::HORIZONTAL));
            for (i, (cur, connector_column)) in link_row
                .iter()
                .copied()
                .zip(connector_columns(line.node_column, link_row))
                .enumerate()
            {
                let next = link_row.get(i + 1).copied().unwrap_or_default();
                // Draw the parent/ancestor line.
                let column_text = if cur.intersects(LinkLine::HORIZONTAL) {
                    if cur.intersects(LinkLine::CHILD | LinkLine::ANY_FORK_OR_MERGE) {
                        "+"
                    } else {
                        "-"
                    }
                } else if cur.intersects(LinkLine::VERTICAL) {
                    if cur.intersects(LinkLine::ANY_FORK_OR_MERGE) && any_horizontal {
                        "+"
                    } else if cur.intersects(LinkLine::VERT_PARENT) {
                        "|"
                    } else {
                        ancestor
                    }
                } else if cur.intersects(LinkLine::ANY_MERGE) && any_horizontal {
                    "'"
                } else if cur.intersects(LinkLine::ANY_FORK) && any_horizontal {
                    "."
                } else {
                    " "
                };

                // Draw the connecting line.
                let connector = if cur.intersects(LinkLine::HORIZONTAL) {
                    "-"
                } else if cur.intersects(LinkLine::RIGHT_MERGE) {
                    if next.intersects(LinkLine::LEFT_FORK) && !any_horizontal {
                        "\\"
                    } else {
                        "-"
                    }
                } else if cur.intersects(LinkLine::RIGHT_FORK) {
                    if next.intersects(LinkLine::LEFT_MERGE) && !any_horizontal {
                        "/"
                    } else {
                        "-"
                    }
                } else {
                    " "
                };

                let column = if is_horizontal_run(cur) {
                    connector_column
                } else {
                    i
                };
                push_column_part(&mut parts, column, column_text);
                push_column_part(&mut parts, connector_column, connector);
            }
            lines.push(PrefixLine {
                parts,
                kind: PrefixLineKind::Link,
            });
        }

        // Render the term line
        if let Some(term_row) = line.term_line.as_ref() {
            let term_strs = ["| ", "~ "];
            for &term_str in &term_strs {
                lines.push(column_line(
                    PrefixLineKind::Term,
                    term_row.iter().enumerate().map(|(i, term)| {
                        if *term {
                            term_str
                        } else {
                            pad_cell(ancestor_line, line.ancestry_line[i])
                        }
                    }),
                ));
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
    column_line(
        kind,
        line.into_iter().map(|entry| pad_cell(ancestor, entry)),
    )
}

fn pad_cell(ancestor: &str, entry: PadLine) -> &str {
    match entry {
        PadLine::Parent => "| ",
        PadLine::Ancestor => ancestor,
        PadLine::Blank => "  ",
    }
}
