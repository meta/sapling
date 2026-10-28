/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use crate::output::OutputRendererState;
use crate::pad::pad_lines;
use crate::pipeline::types::LinkLinePlacement;
use crate::pipeline::types::PrefixLine;
use crate::pipeline::types::PrefixLineKind;
use crate::pipeline::types::PrefixLinePart;

/// Stateful renderer for the final pipeline stage.
///
/// It fills node glyph slots, attaches message lines, and writes complete text
/// lines for one rendered graph row at a time.
#[derive(Default)]
pub struct PrefixLinesToText {
    state: OutputRendererState,
    link_line_placement: LinkLinePlacement,
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

        self.state.begin_row(out, separator_line);

        for prefix_line in prefix_lines {
            // Text starts at the node; extra message lines precede links only for Bottom.
            if prefix_line.kind == PrefixLineKind::PreNode {
                continue;
            }
            if prefix_line.kind == PrefixLineKind::PostNode {
                if extra_message_lines > 0 {
                    let base_line = render_prefix_line(&prefix_line, glyph);
                    // Reserve message lines for the mandatory Link, Term, and Ancestry lines.
                    self.state.push_pad_lines(
                        out,
                        &base_line,
                        message_lines.by_ref().take(extra_message_lines),
                    );
                }
                continue;
            }
            if prefix_line.kind.is_repeatable() {
                repeatable_line = Some(prefix_line);
                continue;
            }
            has_term_row |= prefix_line.kind == PrefixLineKind::Term;
            let text = render_prefix_line(&prefix_line, glyph);
            self.state
                .push_line_with_message(out, text, message_lines.next());
        }

        if let Some(repeatable_line) = repeatable_line {
            let base_line = render_prefix_line(&repeatable_line, glyph);
            if !self.state.push_pad_lines(out, &base_line, message_lines) && has_term_row {
                self.state.queue_pad_line(base_line);
            }
        }
    }
}

fn render_prefix_line(prefix_line: &PrefixLine, glyph: &str) -> String {
    let mut out = String::new();
    for part in prefix_line.parts.iter() {
        match part {
            PrefixLinePart::Text(text) | PrefixLinePart::ColumnText { text, .. } => {
                out.push_str(text)
            }
            PrefixLinePart::NodeGlyph => out.push_str(glyph),
        }
    }
    out
}
