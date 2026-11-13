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
        let writer = TextWriter::new(line);
        let mut lines = Vec::new();
        let spacing = line.style.column_spacing.max(2);
        let leading_gap = " ".repeat(spacing - 1);
        let (ancestor, ancestor_line) = match line.style.dash_style {
            DashStyle::Persistent => ('.', ". "),
            DashStyle::Once => (':', ": "),
        };

        lines.push(render_pad_line(
            &writer,
            ancestor_line,
            &leading_gap,
            line.pre_node_line(),
            PrefixLineKind::PreNode,
        ));

        // Render the nodeline
        let mut node_line = PrefixLine::new(PrefixLineKind::Node);
        for (i, entry) in line.node_line.iter().enumerate() {
            let leading = if i > 0 { leading_gap.as_str() } else { "" };
            match entry {
                NodeLine::Node => {
                    if i > 0 {
                        writer.write_plain(&leading_gap, &mut node_line);
                    }
                    node_line
                        .parts
                        .push((PrefixLinePart::NodeGlyph, Default::default()));
                    writer.write_plain(" ", &mut node_line);
                }
                NodeLine::Parent => push_text(&writer, &mut node_line, i, leading, "| "),
                NodeLine::Ancestor => push_text(&writer, &mut node_line, i, leading, ancestor_line),
                NodeLine::Blank => push_text(&writer, &mut node_line, i, leading, "  "),
            }
        }
        lines.push(node_line);

        lines.push(render_pad_line(
            &writer,
            ancestor_line,
            &leading_gap,
            line.post_node_line(),
            PrefixLineKind::PostNode,
        ));

        // Render the link line
        if let Some(link_row) = line.link_line.as_ref() {
            let mut top_link_line = PrefixLine::new(PrefixLineKind::Link);
            let mut bot_link_line = PrefixLine::new(PrefixLineKind::Link);
            for (i, cur) in link_row.iter().enumerate() {
                if i > 0 {
                    let prev = link_row[i - 1];
                    // Horizontal strokes join the two diagonal endpoints.
                    let middle = if prev
                        .intersects(LinkLine::RIGHT_MERGE_PARENT | LinkLine::HORIZ_PARENT)
                        || cur.intersects(LinkLine::LEFT_MERGE_PARENT | LinkLine::HORIZ_PARENT)
                    {
                        '_'
                    } else if prev
                        .intersects(LinkLine::RIGHT_MERGE_ANCESTOR | LinkLine::HORIZ_ANCESTOR)
                        || cur.intersects(LinkLine::LEFT_MERGE_ANCESTOR | LinkLine::HORIZ_ANCESTOR)
                    {
                        '.'
                    } else {
                        ' '
                    };
                    for _ in 0..spacing - 2 {
                        writer.write_links_char(
                            &[
                                (i - 1, LinkLine::RIGHT_MERGE | LinkLine::HORIZONTAL),
                                (i, LinkLine::LEFT_MERGE | LinkLine::HORIZONTAL),
                            ],
                            middle,
                            &mut top_link_line,
                        );
                        writer.write_plain(" ", &mut bot_link_line);
                    }
                }
                // Top left
                if i > 0 {
                    if cur.intersects(LinkLine::LEFT_MERGE_PARENT) {
                        writer.write_link_char(i, LinkLine::LEFT_MERGE, '/', &mut top_link_line);
                    } else if cur.intersects(LinkLine::LEFT_MERGE_ANCESTOR) {
                        writer.write_link_char(i, LinkLine::LEFT_MERGE, '.', &mut top_link_line);
                    } else if cur.intersects(LinkLine::HORIZ_PARENT) {
                        writer.write_link_char(i, LinkLine::HORIZONTAL, '_', &mut top_link_line);
                    } else if cur.intersects(LinkLine::HORIZ_ANCESTOR) {
                        writer.write_link_char(i, LinkLine::HORIZONTAL, '.', &mut top_link_line);
                    } else {
                        writer.write_link_char(i, LinkLine::empty(), ' ', &mut top_link_line);
                    }
                }

                // Top center
                if cur.intersects(LinkLine::VERT_PARENT) {
                    writer.write_junction_char(i, LinkLine::VERTICAL, '|', &mut top_link_line);
                } else if cur.intersects(LinkLine::VERT_ANCESTOR) {
                    writer.write_junction_char(i, LinkLine::VERTICAL, ancestor, &mut top_link_line);
                } else if cur.intersects(LinkLine::ANY_MERGE) {
                    writer.write_junction_char(i, LinkLine::empty(), ' ', &mut top_link_line);
                } else if cur.intersects(LinkLine::HORIZ_PARENT) {
                    writer.write_junction_char(i, LinkLine::HORIZONTAL, '_', &mut top_link_line);
                } else if cur.intersects(LinkLine::HORIZ_ANCESTOR) {
                    writer.write_junction_char(i, LinkLine::HORIZONTAL, '.', &mut top_link_line);
                } else {
                    writer.write_junction_char(i, LinkLine::empty(), ' ', &mut top_link_line);
                }

                // Top right
                if cur.intersects(LinkLine::RIGHT_MERGE_PARENT) {
                    writer.write_link_char(i, LinkLine::RIGHT_MERGE, '\\', &mut top_link_line);
                } else if cur.intersects(LinkLine::RIGHT_MERGE_ANCESTOR) {
                    writer.write_link_char(i, LinkLine::RIGHT_MERGE, '.', &mut top_link_line);
                } else if cur.intersects(LinkLine::HORIZ_PARENT) {
                    writer.write_link_char(i, LinkLine::HORIZONTAL, '_', &mut top_link_line);
                } else if cur.intersects(LinkLine::HORIZ_ANCESTOR) {
                    writer.write_link_char(i, LinkLine::HORIZONTAL, '.', &mut top_link_line);
                } else {
                    writer.write_link_char(i, LinkLine::empty(), ' ', &mut top_link_line);
                }

                // Bottom left
                if i > 0 {
                    if cur.intersects(LinkLine::LEFT_FORK_PARENT) {
                        writer.write_link_char(i, LinkLine::LEFT_FORK, '\\', &mut bot_link_line);
                    } else if cur.intersects(LinkLine::LEFT_FORK_ANCESTOR) {
                        writer.write_link_char(i, LinkLine::LEFT_FORK, '.', &mut bot_link_line);
                    } else {
                        writer.write_link_char(i, LinkLine::empty(), ' ', &mut bot_link_line);
                    }
                }

                // Bottom center
                if cur.intersects(LinkLine::VERT_PARENT) {
                    writer.write_junction_char(i, LinkLine::VERTICAL, '|', &mut bot_link_line);
                } else if cur.intersects(LinkLine::VERT_ANCESTOR) {
                    writer.write_junction_char(i, LinkLine::VERTICAL, ancestor, &mut bot_link_line);
                } else {
                    writer.write_junction_char(i, LinkLine::empty(), ' ', &mut bot_link_line);
                }

                // Bottom Right
                if cur.intersects(LinkLine::RIGHT_FORK_PARENT) {
                    writer.write_link_char(i, LinkLine::RIGHT_FORK, '/', &mut bot_link_line);
                } else if cur.intersects(LinkLine::RIGHT_FORK_ANCESTOR) {
                    writer.write_link_char(i, LinkLine::RIGHT_FORK, '.', &mut bot_link_line);
                } else {
                    writer.write_link_char(i, LinkLine::empty(), ' ', &mut bot_link_line);
                }
            }
            lines.push(top_link_line);
            lines.push(bot_link_line);
        }

        // Render the term line
        if let Some(term_row) = line.term_line.as_ref() {
            let term_strs = ["| ", "~ "];
            for term_str in term_strs.iter() {
                let mut term_line = PrefixLine::new(PrefixLineKind::Term);
                for (i, term) in term_row.iter().enumerate() {
                    if i > 0 {
                        writer.write_plain(&leading_gap, &mut term_line);
                    }
                    let glyph = if *term {
                        *term_str
                    } else {
                        match line.ancestry_line[i] {
                            PadLine::Parent => "| ",
                            PadLine::Ancestor => ancestor_line,
                            PadLine::Blank => "  ",
                        }
                    };
                    writer.write_column(i, PrefixLineKind::Term, glyph, &mut term_line);
                }
                lines.push(term_line);
            }
        }

        for (kind, columns) in ancestry_lines(line) {
            lines.push(render_pad_line(
                &writer,
                ancestor_line,
                &leading_gap,
                columns,
                kind,
            ));
        }

        lines
    }
}

#[inline]
fn render_pad_line(
    writer: &TextWriter<'_>,
    ancestor: &str,
    leading_gap: &str,
    line: impl IntoIterator<Item = PadLine>,
    kind: PrefixLineKind,
) -> PrefixLine {
    let mut out = PrefixLine::new(kind);
    for (i, entry) in line.into_iter().enumerate() {
        if i > 0 {
            writer.write_plain(leading_gap, &mut out);
        }
        let glyph = match entry {
            PadLine::Parent => "| ",
            PadLine::Ancestor => ancestor,
            PadLine::Blank => "  ",
        };
        writer.write_column(i, kind, glyph, &mut out);
    }
    out
}

fn push_text(
    writer: &TextWriter<'_>,
    line: &mut PrefixLine,
    index: usize,
    leading_gap: &str,
    glyph: &str,
) {
    writer.write_plain(leading_gap, line);
    writer.push_column(line, index, glyph);
}
