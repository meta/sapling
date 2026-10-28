/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::marker::PhantomData;

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

mod glyph {
    pub(super) const SPACE: usize = 0;
    pub(super) const HORIZONTAL: usize = 1;
    pub(super) const PARENT: usize = 2;
    pub(super) const ANCESTOR: usize = 3;
    pub(super) const MERGE_LEFT: usize = 4;
    pub(super) const MERGE_RIGHT: usize = 5;
    pub(super) const MERGE_BOTH: usize = 6;
    pub(super) const FORK_LEFT: usize = 7;
    pub(super) const FORK_RIGHT: usize = 8;
    pub(super) const FORK_BOTH: usize = 9;
    pub(super) const JOIN_LEFT: usize = 10;
    pub(super) const JOIN_RIGHT: usize = 11;
    pub(super) const JOIN_BOTH: usize = 12;
    pub(super) const TERMINATION: usize = 13;
    pub(super) const ANCESTOR_ONCE: usize = 14;
    pub(super) const COUNT: usize = 15;
}

const SQUARE_GLYPHS: [&str; glyph::COUNT] = [
    "  ", "──", "│ ", "· ", "┘ ", "└─", "┴─", "┐ ", "┌─", "┬─", "┤ ", "├─", "┼─", "~ ", "┆ ",
];

const CURVED_GLYPHS: [&str; glyph::COUNT] = [
    "  ", "──", "│ ", "╷ ", "╯ ", "╰─", "┴─", "╮ ", "╭─", "┬─", "┤ ", "├─", "┼─", "~ ", "┆ ",
];

const DEC_GLYPHS: [&str; glyph::COUNT] = [
    "  ",
    "\x1B(0qq\x1B(B",
    "\x1B(0x \x1B(B",
    "\x1B(0~ \x1B(B",
    "\x1B(0j \x1B(B",
    "\x1B(0mq\x1B(B",
    "\x1B(0vq\x1B(B",
    "\x1B(0k \x1B(B",
    "\x1B(0lq\x1B(B",
    "\x1B(0wq\x1B(B",
    "\x1B(0u \x1B(B",
    "\x1B(0tq\x1B(B",
    "\x1B(0nq\x1B(B",
    "~ ",
    ": ",
];

/// Glyph table used by box-drawing prefix line renderers.
pub trait BoxDrawingGlyphSet {
    /// The glyph table for this box-drawing style.
    /// The last entry draws vertical ancestor lines for `DashStyle::Once`.
    const GLYPHS: &'static [&'static str; glyph::COUNT];
}

/// Curved box-drawing glyphs.
pub enum Curved {}

/// Square box-drawing glyphs.
pub enum Square {}

/// DEC special graphics glyphs.
pub enum DecGraphics {}

impl BoxDrawingGlyphSet for Curved {
    const GLYPHS: &'static [&'static str; glyph::COUNT] = &CURVED_GLYPHS;
}

impl BoxDrawingGlyphSet for Square {
    const GLYPHS: &'static [&'static str; glyph::COUNT] = &SQUARE_GLYPHS;
}

impl BoxDrawingGlyphSet for DecGraphics {
    const GLYPHS: &'static [&'static str; glyph::COUNT] = &DEC_GLYPHS;
}

/// Converts abstract row shapes into box-drawing graph prefix lines.
pub struct BoxDrawingPrefixLineRenderer<G = Curved> {
    _glyphs: PhantomData<G>,
}

impl<G> BoxDrawingPrefixLineRenderer<G>
where
    G: BoxDrawingGlyphSet,
{
    /// Create a renderer that uses the glyph set from `G`.
    pub fn new() -> Self {
        Self {
            _glyphs: PhantomData,
        }
    }

    /// Use square box-drawing glyphs.
    pub fn with_square_glyphs(self) -> BoxDrawingPrefixLineRenderer<Square> {
        BoxDrawingPrefixLineRenderer::new()
    }

    /// Use DEC special graphics glyphs.
    pub fn with_dec_graphics_glyphs(self) -> BoxDrawingPrefixLineRenderer<DecGraphics> {
        BoxDrawingPrefixLineRenderer::new()
    }
}

impl<G: BoxDrawingGlyphSet, N> PrefixLineRenderer<N> for BoxDrawingPrefixLineRenderer<G> {
    /// Convert the next graph row shape into prefix lines.
    fn next_prefix_lines(&mut self, line: &GraphRowShape<N>) -> Vec<PrefixLine> {
        let glyphs = G::GLYPHS;
        let ancestor = match line.style.dash_style {
            DashStyle::Persistent => glyph::ANCESTOR,
            DashStyle::Once => glyph::ANCESTOR_ONCE,
        };
        let mut lines = Vec::new();

        lines.push(render_pad_line(
            glyphs,
            ancestor,
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
                NodeLine::Parent => {
                    push_column_part(&mut node_line.parts, i, glyphs[glyph::PARENT])
                }
                NodeLine::Ancestor => push_column_part(&mut node_line.parts, i, glyphs[ancestor]),
                NodeLine::Blank => push_column_part(&mut node_line.parts, i, glyphs[glyph::SPACE]),
            }
        }
        lines.push(node_line);

        lines.push(render_pad_line(
            glyphs,
            ancestor,
            line.post_node_line(),
            PrefixLineKind::PostNode,
        ));

        // Render the link line
        #[allow(clippy::if_same_then_else)]
        if let Some(link_row) = line.link_line.as_ref() {
            let mut parts = Vec::new();
            for (i, (cur, connector_column)) in link_row
                .iter()
                .zip(connector_columns(line.node_column, link_row))
                .enumerate()
            {
                let link_glyph = if cur.intersects(LinkLine::HORIZONTAL) {
                    if cur.intersects(LinkLine::CHILD) {
                        glyph::JOIN_BOTH
                    } else if cur.intersects(LinkLine::ANY_FORK)
                        && cur.intersects(LinkLine::ANY_MERGE)
                    {
                        glyph::JOIN_BOTH
                    } else if cur.intersects(LinkLine::ANY_FORK)
                        && cur.intersects(LinkLine::VERT_PARENT)
                        && !line.merge
                    {
                        glyph::JOIN_BOTH
                    } else if cur.intersects(LinkLine::ANY_FORK) {
                        glyph::FORK_BOTH
                    } else if cur.intersects(LinkLine::ANY_MERGE) {
                        glyph::MERGE_BOTH
                    } else {
                        glyph::HORIZONTAL
                    }
                } else if cur.intersects(LinkLine::VERT_PARENT) && !line.merge {
                    let left = cur.intersects(LinkLine::LEFT_MERGE | LinkLine::LEFT_FORK);
                    let right = cur.intersects(LinkLine::RIGHT_MERGE | LinkLine::RIGHT_FORK);
                    match (left, right) {
                        (true, true) => glyph::JOIN_BOTH,
                        (true, false) => glyph::JOIN_LEFT,
                        (false, true) => glyph::JOIN_RIGHT,
                        (false, false) => glyph::PARENT,
                    }
                } else if cur.intersects(LinkLine::VERT_PARENT | LinkLine::VERT_ANCESTOR)
                    && !cur.intersects(LinkLine::LEFT_FORK | LinkLine::RIGHT_FORK)
                {
                    let left = cur.intersects(LinkLine::LEFT_MERGE);
                    let right = cur.intersects(LinkLine::RIGHT_MERGE);
                    match (left, right) {
                        (true, true) => glyph::JOIN_BOTH,
                        (true, false) => glyph::JOIN_LEFT,
                        (false, true) => glyph::JOIN_RIGHT,
                        (false, false) => {
                            if cur.intersects(LinkLine::VERT_ANCESTOR) {
                                ancestor
                            } else {
                                glyph::PARENT
                            }
                        }
                    }
                } else if cur.intersects(LinkLine::LEFT_FORK)
                    && cur.intersects(LinkLine::LEFT_MERGE | LinkLine::CHILD)
                {
                    glyph::JOIN_LEFT
                } else if cur.intersects(LinkLine::RIGHT_FORK)
                    && cur.intersects(LinkLine::RIGHT_MERGE | LinkLine::CHILD)
                {
                    glyph::JOIN_RIGHT
                } else if cur.intersects(LinkLine::LEFT_MERGE)
                    && cur.intersects(LinkLine::RIGHT_MERGE)
                {
                    glyph::MERGE_BOTH
                } else if cur.intersects(LinkLine::LEFT_FORK)
                    && cur.intersects(LinkLine::RIGHT_FORK)
                {
                    glyph::FORK_BOTH
                } else if cur.intersects(LinkLine::LEFT_FORK) {
                    glyph::FORK_LEFT
                } else if cur.intersects(LinkLine::LEFT_MERGE) {
                    glyph::MERGE_LEFT
                } else if cur.intersects(LinkLine::RIGHT_FORK) {
                    glyph::FORK_RIGHT
                } else if cur.intersects(LinkLine::RIGHT_MERGE) {
                    glyph::MERGE_RIGHT
                } else {
                    glyph::SPACE
                };
                let (column_text, connector) = split_glyph(glyphs[link_glyph]);
                let column = if is_horizontal_run(*cur) {
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
            let term_strs = [glyphs[glyph::PARENT], glyphs[glyph::TERMINATION]];
            for &term_str in &term_strs {
                lines.push(column_line(
                    PrefixLineKind::Term,
                    term_row.iter().enumerate().map(|(i, term)| {
                        if *term {
                            term_str
                        } else {
                            glyphs[pad_line_to_glyph(line.ancestry_line[i], ancestor)]
                        }
                    }),
                ));
            }
        }

        for (kind, columns) in ancestry_lines(line) {
            lines.push(render_pad_line(glyphs, ancestor, columns, kind));
        }

        lines
    }
}

impl<G> Default for BoxDrawingPrefixLineRenderer<G>
where
    G: BoxDrawingGlyphSet,
{
    fn default() -> Self {
        Self::new()
    }
}

/// Split a glyph into the text in its own column and the connector to the next column.
fn split_glyph(glyph: &str) -> (&str, &str) {
    // A DEC special graphics glyph starts with an escape sequence. Keep it with the first
    // character, so that the two halves join to the same bytes.
    let start = if glyph.starts_with("\x1B(0") { 3 } else { 0 };
    let end = start + glyph[start..].chars().next().map_or(0, char::len_utf8);
    glyph.split_at(end)
}

fn render_pad_line(
    glyphs: &[&str; glyph::COUNT],
    ancestor: usize,
    line: impl IntoIterator<Item = PadLine>,
    kind: PrefixLineKind,
) -> PrefixLine {
    column_line(
        kind,
        line.into_iter()
            .map(|entry| glyphs[pad_line_to_glyph(entry, ancestor)]),
    )
}

fn pad_line_to_glyph(line: PadLine, ancestor: usize) -> usize {
    match line {
        PadLine::Parent => glyph::PARENT,
        PadLine::Ancestor => ancestor,
        PadLine::Blank => glyph::SPACE,
    }
}
