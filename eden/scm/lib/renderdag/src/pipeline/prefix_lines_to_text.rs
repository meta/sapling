/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use crate::output::OutputRendererState;
use crate::pad::pad_lines;
use crate::pipeline::coloring::PrefixLineColor;
use crate::pipeline::types::LinkLinePlacement;
use crate::pipeline::types::PrefixLine;
use crate::pipeline::types::PrefixLineKind;
use crate::pipeline::types::PrefixLinePart;
use crate::pipeline::types::TextLine;

/// Stateful renderer for the final pipeline stage.
///
/// It fills node glyph slots, attaches message lines, and writes complete text
/// lines for one rendered graph row at a time.
pub struct PrefixLinesToText {
    state: OutputRendererState,
    link_line_placement: LinkLinePlacement,
    text_gap: String,
}

impl Default for PrefixLinesToText {
    fn default() -> Self {
        Self {
            state: OutputRendererState::default(),
            link_line_placement: LinkLinePlacement::default(),
            text_gap: " ".to_owned(),
        }
    }
}

impl PrefixLinesToText {
    /// Create a renderer with empty output state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Choose where link-line padding is placed. Defaults to `Top`.
    pub fn with_link_line_placement(mut self, placement: LinkLinePlacement) -> Self {
        self.link_line_placement = placement;
        self
    }

    /// Set additional spaces between graph prefixes and messages.
    /// Defaults to one; zero retains only the prefix's column padding.
    pub fn with_text_gap(mut self, text_gap: usize) -> Self {
        self.text_gap = " ".repeat(text_gap);
        self
    }

    /// Render the next row of prefix lines into a `String`.
    pub fn next_text(
        &mut self,
        prefix_lines: Vec<PrefixLine>,
        separator_line: bool,
        glyph: &str,
        message: &str,
        min_row_height: usize,
    ) -> String {
        let mut out = String::new();
        self.write_next_text(
            &mut out,
            prefix_lines,
            separator_line,
            glyph,
            message,
            min_row_height,
        );
        out
    }

    /// Write the next row of prefix lines into `out`.
    pub fn write_next_text(
        &mut self,
        out: &mut String,
        prefix_lines: Vec<PrefixLine>,
        separator_line: bool,
        glyph: &str,
        message: &str,
        min_row_height: usize,
    ) {
        self.write_next_lines(
            |text, _| out.push_str(text),
            prefix_lines,
            separator_line,
            glyph,
            message,
            min_row_height,
        );
    }

    /// Lay out the next row as text fragments, retaining graph line identities.
    /// Supplied single-line glyph strings fill their slots and retain the slot's IDs.
    /// Each returned line has no trailing whitespace or newline characters.
    pub fn next_lines(
        &mut self,
        prefix_lines: Vec<PrefixLine>,
        separator_line: bool,
        glyph: &str,
        message: &str,
        min_row_height: usize,
    ) -> Vec<TextLine> {
        let mut out = Vec::new();
        let mut line = Vec::new();
        self.write_next_lines(
            |text, color| {
                if text == "\n" {
                    out.push(std::mem::take(&mut line));
                } else {
                    line.push((text.to_owned(), color.clone()));
                }
            },
            prefix_lines,
            separator_line,
            glyph,
            message,
            min_row_height,
        );
        out
    }

    /// Stream borrowed text fragments and their IDs in output order.
    /// A separate `"\n"` fragment ends each line; it carries no line identity.
    /// References are valid for the callback only. Glyphs must be single-line.
    pub fn write_next_lines(
        &mut self,
        mut out: impl FnMut(&str, &PrefixLineColor),
        prefix_lines: Vec<PrefixLine>,
        separator_line: bool,
        glyph: &str,
        message: &str,
        min_row_height: usize,
    ) {
        let mut message_lines = pad_lines(message.lines(), min_row_height);
        let extra_message_lines = if self.link_line_placement == LinkLinePlacement::Bottom {
            let fixed_lines = prefix_lines
                .iter()
                .filter(|line| !line.kind.is_repeatable())
                .count();
            message
                .lines()
                .count()
                .max(min_row_height)
                .saturating_sub(fixed_lines)
        } else {
            0
        };
        let mut repeatable_line = None;
        let mut has_term_row = false;

        self.state.begin_row(&mut out, separator_line);

        for prefix_line in prefix_lines {
            // Text starts at the node; extra message lines precede links only for Bottom.
            if prefix_line.kind == PrefixLineKind::PreNode {
                continue;
            }
            if prefix_line.kind == PrefixLineKind::PostNode {
                if extra_message_lines > 0 {
                    let base_line = prefix_fragments(&prefix_line, glyph);
                    // Reserve message lines for the mandatory Link, Term, and Ancestry lines.
                    self.state.push_pad_lines(
                        &mut out,
                        base_line,
                        message_lines.by_ref().take(extra_message_lines),
                        &self.text_gap,
                    );
                }
                continue;
            }
            if prefix_line.kind.is_repeatable() {
                repeatable_line = Some(prefix_line);
                continue;
            }
            has_term_row |= prefix_line.kind == PrefixLineKind::Term;
            let text = prefix_fragments(&prefix_line, glyph);
            self.state
                .push_line_with_message(&mut out, text, message_lines.next(), &self.text_gap);
        }

        if let Some(repeatable_line) = repeatable_line {
            let base_line = prefix_fragments(&repeatable_line, glyph);
            if !self
                .state
                .push_pad_lines(&mut out, base_line, message_lines, &self.text_gap)
                && has_term_row
            {
                self.state.queue_pad_line(
                    repeatable_line
                        .parts
                        .into_iter()
                        .map(|(part, color)| {
                            let text = match part {
                                PrefixLinePart::Text(text) => text,
                                PrefixLinePart::NodeGlyph => glyph.to_owned(),
                            };
                            (text, color)
                        })
                        .collect(),
                );
            }
        }
    }
}

fn prefix_fragments<'a>(
    prefix_line: &'a PrefixLine,
    glyph: &'a str,
) -> impl ExactSizeIterator<Item = (&'a str, &'a PrefixLineColor)> + DoubleEndedIterator + Clone {
    prefix_line.parts.iter().map(move |(part, color)| {
        let text = match part {
            PrefixLinePart::Text(text) => text.as_str(),
            PrefixLinePart::NodeGlyph => glyph,
        };
        (text, color)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_borrowed_fragments() {
        let graph = "│ ".to_owned();
        let graph_ptr = graph.as_ptr();
        let glyph = "●".to_owned();
        let message = "message \t".to_owned();
        let prefix = PrefixLine {
            parts: vec![
                (PrefixLinePart::Text(graph), Default::default()),
                (PrefixLinePart::NodeGlyph, Default::default()),
                (PrefixLinePart::Text("  ".to_owned()), Default::default()),
            ],
            kind: PrefixLineKind::Node,
        };
        let mut output = String::new();
        PrefixLinesToText::new().write_next_lines(
            |text, _| {
                match text {
                    "│ " => assert_eq!(text.as_ptr(), graph_ptr),
                    "●" => assert_eq!(text.as_ptr(), glyph.as_ptr()),
                    "message" => assert_eq!(text.as_ptr(), message.as_ptr()),
                    _ => {}
                }
                output.push_str(text);
            },
            vec![prefix],
            false,
            &glyph,
            &message,
            0,
        );
        assert_eq!(output, "│ ●   message\n");
    }
}
