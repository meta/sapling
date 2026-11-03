/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

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
        let mut lines = Vec::new();
        let spacing = line.style.column_spacing.max(2);
        let leading_gap = " ".repeat(spacing - 1);
        let (ancestor, ancestor_line) = match line.style.dash_style {
            DashStyle::Persistent => ('.', ". "),
            DashStyle::Once => (':', ": "),
        };

        lines.push(render_pad_line(
            ancestor_line,
            &leading_gap,
            line.pre_node_line(),
            PrefixLineKind::PreNode,
        ));

        // Render the nodeline
        let mut node_line = PrefixLine {
            parts: Vec::new(),
            kind: PrefixLineKind::Node,
        };
        for (i, entry) in line.node_line.iter().enumerate() {
            let leading = if i > 0 { leading_gap.as_str() } else { "" };
            match entry {
                NodeLine::Node => {
                    if i > 0 {
                        node_line
                            .parts
                            .push(PrefixLinePart::Text(leading_gap.clone()));
                    }
                    node_line.parts.push(PrefixLinePart::NodeGlyph);
                    node_line
                        .parts
                        .push(PrefixLinePart::Text(String::from(" ")));
                }
                NodeLine::Parent => push_text(&mut node_line, leading, "| "),
                NodeLine::Ancestor => push_text(&mut node_line, leading, ancestor_line),
                NodeLine::Blank => push_text(&mut node_line, leading, "  "),
            }
        }
        lines.push(node_line);

        lines.push(render_pad_line(
            ancestor_line,
            &leading_gap,
            line.post_node_line(),
            PrefixLineKind::PostNode,
        ));

        // Render the link line
        if let Some(link_row) = line.link_line.as_ref() {
            let mut top_link_line = String::new();
            let mut bot_link_line = String::new();
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
                        top_link_line.push(middle);
                        bot_link_line.push(' ');
                    }
                }
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
            }
            lines.push(PrefixLine {
                parts: vec![PrefixLinePart::Text(top_link_line)],
                kind: PrefixLineKind::Link,
            });
            lines.push(PrefixLine {
                parts: vec![PrefixLinePart::Text(bot_link_line)],
                kind: PrefixLineKind::Link,
            });
        }

        // Render the term line
        if let Some(term_row) = line.term_line.as_ref() {
            let term_strs = ["| ", "~ "];
            for term_str in term_strs.iter() {
                let mut term_line = String::new();
                for (i, term) in term_row.iter().enumerate() {
                    if i > 0 {
                        term_line.push_str(&leading_gap);
                    }
                    if *term {
                        term_line.push_str(term_str);
                    } else {
                        term_line.push_str(match line.ancestry_line[i] {
                            PadLine::Parent => "| ",
                            PadLine::Ancestor => ancestor_line,
                            PadLine::Blank => "  ",
                        });
                    }
                }
                lines.push(PrefixLine {
                    parts: vec![PrefixLinePart::Text(term_line)],
                    kind: PrefixLineKind::Term,
                });
            }
        }

        for (kind, columns) in ancestry_lines(line) {
            lines.push(render_pad_line(ancestor_line, &leading_gap, columns, kind));
        }

        lines
    }
}

fn render_pad_line(
    ancestor: &str,
    leading_gap: &str,
    line: impl IntoIterator<Item = PadLine>,
    kind: PrefixLineKind,
) -> PrefixLine {
    PrefixLine {
        parts: vec![PrefixLinePart::Text(render_pad_text(
            ancestor,
            leading_gap,
            line,
        ))],
        kind,
    }
}

fn render_pad_text(
    ancestor: &str,
    leading_gap: &str,
    line: impl IntoIterator<Item = PadLine>,
) -> String {
    let mut out = String::new();
    for (i, entry) in line.into_iter().enumerate() {
        if i > 0 {
            out.push_str(leading_gap);
        }
        out.push_str(match entry {
            PadLine::Parent => "| ",
            PadLine::Ancestor => ancestor,
            PadLine::Blank => "  ",
        });
    }
    out
}

fn push_text(line: &mut PrefixLine, leading_gap: &str, glyph: &str) {
    let mut text = String::with_capacity(leading_gap.len() + glyph.len());
    text.push_str(leading_gap);
    text.push_str(glyph);
    line.parts.push(PrefixLinePart::Text(text));
}
