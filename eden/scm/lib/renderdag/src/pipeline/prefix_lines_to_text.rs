/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use crate::pad::pad_lines;
use crate::pipeline::types::LinePiece;
use crate::pipeline::types::LinkLinePlacement;
use crate::pipeline::types::PrefixLine;
use crate::pipeline::types::PrefixLineKind;
use crate::pipeline::types::PrefixLinePart;

/// Stateful renderer for the final pipeline stage.
///
/// It fills node glyph slots, attaches message lines, and writes complete text lines for one
/// rendered graph row at a time.
#[derive(Default)]
pub struct PrefixLinesToText {
    /// The queued pad line, and the node glyph to render it with.
    queued_pad_line: Option<(PrefixLine, String)>,
    /// If > 1, no need for an extra separator for the next row. Separator is only needed for
    /// adjacent single-line node lines. If there are link, pad, term, or other kinds of node lines,
    /// no need to draw separator.
    row_height: usize,
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
        self.write_next_lines(
            |line| {
                for piece in line {
                    out.push_str(piece.text);
                }
                out.push('\n');
            },
            prefix_lines,
            separator_line,
            glyph,
            message,
            min_row_height,
        );
    }

    /// Give each line of the next row to `out`, as a list of pieces.
    ///
    /// A caller can use this to style each piece, for example to give each column its own color.
    /// The last piece of a line has no trailing whitespace.
    pub fn write_next_lines(
        &mut self,
        mut out: impl FnMut(&[LinePiece<'_>]),
        prefix_lines: Vec<PrefixLine>,
        separator_line: bool,
        glyph: &str,
        message: &str,
        min_row_height: usize,
    ) {
        let out = &mut out;
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

        self.begin_row(out, separator_line);

        for prefix_line in prefix_lines {
            // Text starts at the node; extra message lines precede links only for Bottom.
            if prefix_line.kind == PrefixLineKind::PreNode {
                continue;
            }
            if prefix_line.kind == PrefixLineKind::PostNode {
                if extra_message_lines > 0 {
                    let base_line = line_pieces(&prefix_line, glyph);
                    // Reserve message lines for the mandatory Link, Term, and Ancestry lines.
                    self.push_pad_lines(
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
            let line = line_pieces(&prefix_line, glyph);
            self.push_line_with_message(out, line, message_lines.next());
        }

        if let Some(repeatable_line) = repeatable_line {
            let base_line = line_pieces(&repeatable_line, glyph);
            if !self.push_pad_lines(out, &base_line, message_lines) && has_term_row {
                self.queue_pad_line(repeatable_line, glyph);
            }
        }
    }

    fn begin_row(&mut self, out: &mut impl FnMut(&[LinePiece<'_>]), separator_line: bool) {
        self.flush_queued_pad_line(out);
        if separator_line && self.row_height == 1 {
            self.push_line(out, &mut []);
        }
        self.row_height = 0;
    }

    fn push_line_with_message<'a>(
        &mut self,
        out: &mut impl FnMut(&[LinePiece<'_>]),
        mut line: Vec<LinePiece<'a>>,
        message: Option<&'a str>,
    ) {
        if let Some(message) = message {
            line.push(LinePiece {
                column: None,
                text: " ",
            });
            line.push(LinePiece {
                column: None,
                text: message,
            });
        }
        self.push_line(out, &mut line);
    }

    fn push_pad_lines<'a>(
        &mut self,
        out: &mut impl FnMut(&[LinePiece<'_>]),
        base_pad_line: &[LinePiece<'_>],
        message_lines: impl Iterator<Item = &'a str>,
    ) -> bool {
        let mut emitted = false;
        for message in message_lines {
            self.push_line_with_message(out, base_pad_line.to_vec(), Some(message));
            emitted = true;
        }
        emitted
    }

    fn queue_pad_line(&mut self, pad_line: PrefixLine, glyph: &str) {
        self.queued_pad_line = Some((pad_line, glyph.to_owned()));
    }

    fn push_line(&mut self, out: &mut impl FnMut(&[LinePiece<'_>]), line: &mut [LinePiece<'_>]) {
        out(trim_end(line));
        self.row_height = self.row_height.saturating_add(1);
    }

    fn flush_queued_pad_line(&mut self, out: &mut impl FnMut(&[LinePiece<'_>])) {
        if let Some((pad_line, glyph)) = self.queued_pad_line.take() {
            self.push_line(out, &mut line_pieces(&pad_line, &glyph));
        }
    }
}

/// Remove trailing whitespace from a line, across pieces.
fn trim_end<'s, 'a>(line: &'s mut [LinePiece<'a>]) -> &'s [LinePiece<'a>] {
    let end = line
        .iter_mut()
        .rposition(|piece| {
            piece.text = piece.text.trim_end();
            !piece.text.is_empty()
        })
        .map_or(0, |index| index + 1);
    &line[..end]
}

/// Convert a prefix line into line pieces, with `glyph` in the node glyph slot.
fn line_pieces<'a>(prefix_line: &'a PrefixLine, glyph: &'a str) -> Vec<LinePiece<'a>> {
    prefix_line
        .parts
        .iter()
        .map(|part| match part {
            PrefixLinePart::Text(text) => LinePiece { column: None, text },
            PrefixLinePart::ColumnText { column, text } => LinePiece {
                column: Some(*column),
                text,
            },
            PrefixLinePart::NodeGlyph => LinePiece {
                column: None,
                text: glyph,
            },
        })
        .collect()
}
