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
        let spacing = line.style.column_spacing.max(1);
        let gap = " ".repeat(spacing);
        let ancestor = match line.style.dash_style {
            DashStyle::Persistent => '.',
            DashStyle::Once => ':',
        };

        lines.push(render_pad_line(
            ancestor,
            &gap,
            line.pre_node_line(),
            PrefixLineKind::PreNode,
        ));

        // Render the nodeline
        let mut node_line = PrefixLine {
            parts: Vec::new(),
            kind: PrefixLineKind::Node,
        };
        for entry in line.node_line.iter() {
            match entry {
                NodeLine::Node => {
                    node_line.parts.push(PrefixLinePart::NodeGlyph);
                    node_line.parts.push(PrefixLinePart::Text(gap.clone()));
                }
                NodeLine::Parent => push_text(&mut node_line, '|', &gap),
                NodeLine::Ancestor => push_text(&mut node_line, ancestor, &gap),
                NodeLine::Blank => push_text(&mut node_line, ' ', &gap),
            }
        }
        lines.push(node_line);

        lines.push(render_pad_line(
            ancestor,
            &gap,
            line.post_node_line(),
            PrefixLineKind::PostNode,
        ));

        // Render the link line
        if let Some(link_row) = line.link_line.as_ref() {
            let mut link_line = String::new();
            let any_horizontal = link_row
                .iter()
                .any(|cur| cur.intersects(LinkLine::HORIZONTAL));
            let mut iter = link_row
                .iter()
                .copied()
                .chain(std::iter::once(LinkLine::empty()))
                .peekable();
            while let Some(cur) = iter.next() {
                let next = match iter.peek() {
                    Some(&v) => v,
                    None => break,
                };
                // Draw the parent/ancestor line.
                if cur.intersects(LinkLine::HORIZONTAL) {
                    if cur.intersects(LinkLine::CHILD | LinkLine::ANY_FORK_OR_MERGE) {
                        link_line.push('+');
                    } else {
                        link_line.push('-');
                    }
                } else if cur.intersects(LinkLine::VERTICAL) {
                    if cur.intersects(LinkLine::ANY_FORK_OR_MERGE) && any_horizontal {
                        link_line.push('+');
                    } else if cur.intersects(LinkLine::VERT_PARENT) {
                        link_line.push('|');
                    } else {
                        link_line.push(ancestor);
                    }
                } else if cur.intersects(LinkLine::ANY_MERGE) && any_horizontal {
                    link_line.push('\'');
                } else if cur.intersects(LinkLine::ANY_FORK) && any_horizontal {
                    link_line.push('.');
                } else {
                    link_line.push(' ');
                }

                // Draw the connecting line.
                if cur.intersects(LinkLine::HORIZONTAL) {
                    link_line.push('-');
                } else if cur.intersects(LinkLine::RIGHT_MERGE) {
                    if next.intersects(LinkLine::LEFT_FORK) && !any_horizontal {
                        link_line.push('\\');
                    } else {
                        link_line.push('-');
                    }
                } else if cur.intersects(LinkLine::RIGHT_FORK) {
                    if next.intersects(LinkLine::LEFT_MERGE) && !any_horizontal {
                        link_line.push('/');
                    } else {
                        link_line.push('-');
                    }
                } else {
                    link_line.push(' ');
                }
            }
            if spacing == 1 {
                lines.push(PrefixLine {
                    parts: vec![PrefixLinePart::Text(link_line)],
                    kind: PrefixLineKind::Link,
                });
            } else {
                // Extra rows keep diagonal strokes continuous at wider spacing.
                let height = if link_line.contains(['/', '\\']) {
                    spacing
                } else {
                    1
                };
                for row in 0..height {
                    let mut expanded = String::with_capacity(link_row.len() * (spacing + 1));
                    for cell in link_line.as_bytes().as_chunks::<2>().0 {
                        expanded.push(cell[0] as char);
                        match cell[1] {
                            b'\\' => {
                                expanded.push_str(&gap[..row]);
                                expanded.push('\\');
                                expanded.push_str(&gap[..spacing - row - 1]);
                            }
                            b'/' => {
                                expanded.push_str(&gap[..spacing - row - 1]);
                                expanded.push('/');
                                expanded.push_str(&gap[..row]);
                            }
                            glyph => {
                                for _ in 0..spacing {
                                    expanded.push(glyph as char);
                                }
                            }
                        }
                    }
                    lines.push(PrefixLine {
                        parts: vec![PrefixLinePart::Text(expanded)],
                        kind: PrefixLineKind::Link,
                    });
                }
            }
        }

        // Render the term line
        if let Some(term_row) = line.term_line.as_ref() {
            for term_glyph in ['|', '~'] {
                let mut term_line = String::new();
                for (i, term) in term_row.iter().enumerate() {
                    if *term {
                        term_line.push(term_glyph);
                    } else {
                        term_line.push(match line.ancestry_line[i] {
                            PadLine::Parent => '|',
                            PadLine::Ancestor => ancestor,
                            PadLine::Blank => ' ',
                        });
                    }
                    term_line.push_str(&gap);
                }
                lines.push(PrefixLine {
                    parts: vec![PrefixLinePart::Text(term_line)],
                    kind: PrefixLineKind::Term,
                });
            }
        }

        for (kind, columns) in ancestry_lines(line) {
            lines.push(render_pad_line(ancestor, &gap, columns, kind));
        }

        lines
    }
}

fn render_pad_line(
    ancestor: char,
    gap: &str,
    line: impl IntoIterator<Item = PadLine>,
    kind: PrefixLineKind,
) -> PrefixLine {
    PrefixLine {
        parts: vec![PrefixLinePart::Text(render_pad_text(ancestor, gap, line))],
        kind,
    }
}

fn render_pad_text(ancestor: char, gap: &str, line: impl IntoIterator<Item = PadLine>) -> String {
    let mut out = String::new();
    for entry in line {
        out.push(match entry {
            PadLine::Parent => '|',
            PadLine::Ancestor => ancestor,
            PadLine::Blank => ' ',
        });
        out.push_str(gap);
    }
    out
}

fn push_text(line: &mut PrefixLine, glyph: char, gap: &str) {
    let mut text = String::with_capacity(1 + gap.len());
    text.push(glyph);
    text.push_str(gap);
    line.parts.push(PrefixLinePart::Text(text));
}
