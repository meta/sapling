/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::marker::PhantomData;

use super::TextWriter;
use super::ancestry_lines;
use super::split_glyph;
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
    /// Each entry contains a column glyph followed by a space or horizontal
    /// glyph, optionally wrapped in DEC special graphics escape sequences.
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
    column_spacing: usize,
    spaced_glyphs: Option<Box<[String; glyph::COUNT]>>,
    _glyphs: PhantomData<G>,
}

impl<G> BoxDrawingPrefixLineRenderer<G>
where
    G: BoxDrawingGlyphSet,
{
    /// Create a renderer that uses the glyph set from `G`.
    pub fn new() -> Self {
        Self {
            column_spacing: 1,
            spaced_glyphs: None,
            _glyphs: PhantomData,
        }
    }

    fn set_column_spacing(&mut self, column_spacing: usize) {
        if self.column_spacing == column_spacing {
            return;
        }
        self.column_spacing = column_spacing;
        self.spaced_glyphs = (column_spacing != 1).then(|| {
            Box::new(G::GLYPHS.map(|glyph| {
                let (prefix, column, spacing, suffix) = split_glyph(glyph);
                let mut out = String::with_capacity(
                    prefix.len() + column.len() + spacing.len() * column_spacing + suffix.len(),
                );
                out.push_str(prefix);
                out.push_str(column);
                for _ in 0..column_spacing {
                    out.push_str(spacing);
                }
                out.push_str(suffix);
                out
            }))
        });
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
        self.set_column_spacing(line.style.column_spacing);
        let spaced_glyphs = self
            .spaced_glyphs
            .as_deref()
            .map(|glyphs| glyphs.each_ref().map(String::as_str));
        let glyphs = spaced_glyphs.as_ref().unwrap_or(G::GLYPHS);
        let ancestor = match line.style.dash_style {
            DashStyle::Persistent => glyph::ANCESTOR,
            DashStyle::Once => glyph::ANCESTOR_ONCE,
        };
        let writer = TextWriter::new(line);
        let mut lines = Vec::new();

        lines.push(render_pad_line(
            &writer,
            glyphs,
            ancestor,
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
                    writer.write_plain(&" ".repeat(line.style.column_spacing), &mut node_line);
                }
                NodeLine::Parent => writer.push_column(&mut node_line, i, glyphs[glyph::PARENT]),
                NodeLine::Ancestor => writer.push_column(&mut node_line, i, glyphs[ancestor]),
                NodeLine::Blank => writer.push_column(&mut node_line, i, glyphs[glyph::SPACE]),
            }
        }
        lines.push(node_line);

        lines.push(render_pad_line(
            &writer,
            glyphs,
            ancestor,
            line.post_node_line(),
            PrefixLineKind::PostNode,
        ));

        // Render the link line
        #[allow(clippy::if_same_then_else)]
        if let Some(link_row) = line.link_line.as_ref() {
            let mut link_line = PrefixLine::new(PrefixLineKind::Link);
            for (i, cur) in link_row.iter().enumerate() {
                let selected = if cur.intersects(LinkLine::HORIZONTAL) {
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
                write_link_glyph(&writer, i, selected, glyphs[selected], &mut link_line);
            }
            lines.push(link_line);
        }

        // Render the term line
        if let Some(term_row) = line.term_line.as_ref() {
            let term_strs = [glyphs[glyph::PARENT], glyphs[glyph::TERMINATION]];
            for term_str in term_strs.iter() {
                let mut term_line = PrefixLine::new(PrefixLineKind::Term);
                for (i, term) in term_row.iter().enumerate() {
                    let text = if *term {
                        term_str
                    } else {
                        glyphs[pad_line_to_glyph(line.ancestry_line[i], ancestor)]
                    };
                    writer.write_column(i, PrefixLineKind::Term, text, &mut term_line);
                }
                lines.push(term_line);
            }
        }

        for (kind, columns) in ancestry_lines(line) {
            lines.push(render_pad_line(&writer, glyphs, ancestor, columns, kind));
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

fn write_link_glyph(
    writer: &TextWriter<'_>,
    index: usize,
    selected: usize,
    text: &str,
    out: &mut PrefixLine,
) {
    if writer.is_plain() {
        writer.write_plain(text, out);
        return;
    }
    let (prefix, column, connector, suffix) = split_glyph(text);
    let segments = match selected {
        glyph::HORIZONTAL => LinkLine::HORIZONTAL,
        glyph::PARENT | glyph::ANCESTOR | glyph::ANCESTOR_ONCE => LinkLine::VERTICAL,
        glyph::MERGE_LEFT => LinkLine::LEFT_MERGE,
        glyph::MERGE_RIGHT => LinkLine::RIGHT_MERGE,
        glyph::MERGE_BOTH => LinkLine::LEFT_MERGE | LinkLine::RIGHT_MERGE,
        glyph::FORK_LEFT => LinkLine::LEFT_FORK,
        glyph::FORK_RIGHT => LinkLine::RIGHT_FORK,
        glyph::FORK_BOTH => LinkLine::LEFT_FORK | LinkLine::RIGHT_FORK | LinkLine::HORIZONTAL,
        glyph::JOIN_LEFT => LinkLine::VERTICAL | LinkLine::LEFT_MERGE | LinkLine::LEFT_FORK,
        glyph::JOIN_RIGHT => LinkLine::VERTICAL | LinkLine::RIGHT_MERGE | LinkLine::RIGHT_FORK,
        glyph::JOIN_BOTH => LinkLine::VERTICAL | LinkLine::HORIZONTAL | LinkLine::ANY_FORK_OR_MERGE,
        _ => LinkLine::empty(),
    };
    writer.write_plain(prefix, out);
    writer.write_junction(index, segments, column, out);
    if connector.trim_matches(' ').is_empty() {
        writer.write_plain(connector, out);
    } else {
        // The connector has its own line IDs, independent of the intersection glyph.
        writer.write_link(
            index,
            LinkLine::HORIZONTAL | LinkLine::RIGHT_FORK | LinkLine::RIGHT_MERGE,
            connector,
            out,
        );
    }
    writer.write_plain(suffix, out);
}

fn render_pad_line(
    writer: &TextWriter<'_>,
    glyphs: &[&str; glyph::COUNT],
    ancestor: usize,
    line: impl IntoIterator<Item = PadLine>,
    kind: PrefixLineKind,
) -> PrefixLine {
    let mut out = PrefixLine::new(kind);
    for (i, entry) in line.into_iter().enumerate() {
        writer.write_column(
            i,
            kind,
            glyphs[pad_line_to_glyph(entry, ancestor)],
            &mut out,
        );
    }
    out
}

fn pad_line_to_glyph(line: PadLine, ancestor: usize) -> usize {
    match line {
        PadLine::Parent => glyph::PARENT,
        PadLine::Ancestor => ancestor,
        PadLine::Blank => glyph::SPACE,
    }
}
