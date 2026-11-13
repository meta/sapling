/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use super::TextWriter;
use super::ancestry_lines;
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
        let writer = TextWriter::new(line);
        let mut lines = Vec::new();
        let spacing = line.style.column_spacing.max(1);
        let gap = " ".repeat(spacing);
        let ancestor = match line.style.dash_style {
            DashStyle::Persistent => '.',
            DashStyle::Once => ':',
        };

        lines.push(render_pad_line(
            &writer,
            ancestor,
            &gap,
            line.pre_node_line(),
            PrefixLineKind::PreNode,
        ));

        // Render the nodeline
        let mut node_line = PrefixLine::new(PrefixLineKind::Node);
        for (i, entry) in line.node_line.iter().enumerate() {
            match entry {
                NodeLine::Node => {
                    node_line
                        .parts
                        .push((PrefixLinePart::NodeGlyph, Default::default()));
                    writer.write_plain(&gap, &mut node_line);
                }
                entry => {
                    let glyph = match entry {
                        NodeLine::Parent => '|',
                        NodeLine::Ancestor => ancestor,
                        _ => ' ',
                    };
                    writer.push_column(&mut node_line, i, glyph.encode_utf8(&mut [0; 4]));
                    writer.write_plain(&gap, &mut node_line);
                }
            }
        }
        lines.push(node_line);

        lines.push(render_pad_line(
            &writer,
            ancestor,
            &gap,
            line.post_node_line(),
            PrefixLineKind::PostNode,
        ));

        // Render the link line
        if let Some(link_row) = line.link_line.as_ref() {
            let mut link_line = PrefixLine::new(PrefixLineKind::Link);
            let mut cell_pairs = String::new();
            let any_horizontal = link_row
                .iter()
                .any(|cur| cur.intersects(LinkLine::HORIZONTAL));
            let mut masks = Vec::new();
            for (i, cur) in link_row.iter().copied().enumerate() {
                let next = link_row.get(i + 1).copied().unwrap_or_default();
                // Draw the parent/ancestor line.
                let (column, column_segments) = if cur.intersects(LinkLine::HORIZONTAL) {
                    if cur.intersects(LinkLine::CHILD | LinkLine::ANY_FORK_OR_MERGE) {
                        (
                            '+',
                            LinkLine::VERTICAL | LinkLine::HORIZONTAL | LinkLine::ANY_FORK_OR_MERGE,
                        )
                    } else {
                        ('-', LinkLine::HORIZONTAL)
                    }
                } else if cur.intersects(LinkLine::VERTICAL) {
                    if cur.intersects(LinkLine::ANY_FORK_OR_MERGE) && any_horizontal {
                        ('+', LinkLine::VERTICAL | LinkLine::ANY_FORK_OR_MERGE)
                    } else if cur.intersects(LinkLine::VERT_PARENT) {
                        ('|', LinkLine::VERTICAL)
                    } else {
                        (ancestor, LinkLine::VERTICAL)
                    }
                } else if cur.intersects(LinkLine::ANY_MERGE) && any_horizontal {
                    ('\'', LinkLine::ANY_MERGE)
                } else if cur.intersects(LinkLine::ANY_FORK) && any_horizontal {
                    ('.', LinkLine::ANY_FORK)
                } else {
                    (' ', LinkLine::empty())
                };

                // Draw the connecting line.
                let (connector, connector_segments) = if cur.intersects(LinkLine::HORIZONTAL) {
                    (
                        '-',
                        LinkLine::HORIZONTAL | LinkLine::RIGHT_MERGE | LinkLine::RIGHT_FORK,
                    )
                } else if cur.intersects(LinkLine::RIGHT_MERGE) {
                    (
                        if next.intersects(LinkLine::LEFT_FORK) && !any_horizontal {
                            '\\'
                        } else {
                            '-'
                        },
                        LinkLine::RIGHT_MERGE,
                    )
                } else if cur.intersects(LinkLine::RIGHT_FORK) {
                    (
                        if next.intersects(LinkLine::LEFT_MERGE) && !any_horizontal {
                            '/'
                        } else {
                            '-'
                        },
                        LinkLine::RIGHT_FORK,
                    )
                } else {
                    (' ', LinkLine::empty())
                };
                if spacing == 1 {
                    writer.write_junction_char(i, column_segments, column, &mut link_line);
                    writer.write_link_char(i, connector_segments, connector, &mut link_line);
                } else {
                    cell_pairs.push(column);
                    cell_pairs.push(connector);
                    if !writer.is_plain() {
                        masks.push([column_segments, connector_segments]);
                    }
                }
            }
            if spacing == 1 {
                lines.push(link_line);
            } else {
                // Extra rows keep diagonal strokes continuous at wider spacing.
                let height = if cell_pairs.contains(['/', '\\']) {
                    spacing
                } else {
                    1
                };
                for row in 0..height {
                    let mut expanded = PrefixLine::new(PrefixLineKind::Link);
                    for (i, cell) in cell_pairs.as_bytes().as_chunks::<2>().0.iter().enumerate() {
                        let [column_segments, connector_segments] =
                            masks.get(i).copied().unwrap_or([LinkLine::empty(); 2]);
                        writer.write_junction_char(
                            i,
                            column_segments,
                            cell[0] as char,
                            &mut expanded,
                        );
                        match cell[1] {
                            b'\\' => {
                                writer.write_plain(&gap[..row], &mut expanded);
                                writer.write_link_char(i, connector_segments, '\\', &mut expanded);
                                writer.write_plain(&gap[..spacing - row - 1], &mut expanded);
                            }
                            b'/' => {
                                writer.write_plain(&gap[..spacing - row - 1], &mut expanded);
                                writer.write_link_char(i, connector_segments, '/', &mut expanded);
                                writer.write_plain(&gap[..row], &mut expanded);
                            }
                            glyph => {
                                for _ in 0..spacing {
                                    writer.write_link_char(
                                        i,
                                        connector_segments,
                                        glyph as char,
                                        &mut expanded,
                                    );
                                }
                            }
                        }
                    }
                    lines.push(expanded);
                }
            }
        }

        // Render the term line
        if let Some(term_row) = line.term_line.as_ref() {
            for term_glyph in ['|', '~'] {
                let mut term_line = PrefixLine::new(PrefixLineKind::Term);
                for (i, term) in term_row.iter().enumerate() {
                    let glyph = if *term {
                        term_glyph
                    } else {
                        match line.ancestry_line[i] {
                            PadLine::Parent => '|',
                            PadLine::Ancestor => ancestor,
                            PadLine::Blank => ' ',
                        }
                    };
                    writer.write_column(
                        i,
                        PrefixLineKind::Term,
                        glyph.encode_utf8(&mut [0; 4]),
                        &mut term_line,
                    );
                    writer.write_plain(&gap, &mut term_line);
                }
                lines.push(term_line);
            }
        }

        for (kind, columns) in ancestry_lines(line) {
            lines.push(render_pad_line(&writer, ancestor, &gap, columns, kind));
        }

        lines
    }
}

#[inline]
fn render_pad_line(
    writer: &TextWriter<'_>,
    ancestor: char,
    gap: &str,
    line: impl IntoIterator<Item = PadLine>,
    kind: PrefixLineKind,
) -> PrefixLine {
    let mut out = PrefixLine::new(kind);
    for (i, entry) in line.into_iter().enumerate() {
        let glyph = match entry {
            PadLine::Parent => '|',
            PadLine::Ancestor => ancestor,
            PadLine::Blank => ' ',
        };
        writer.write_column(i, kind, glyph.encode_utf8(&mut [0; 4]), &mut out);
        writer.write_plain(gap, &mut out);
    }
    out
}
