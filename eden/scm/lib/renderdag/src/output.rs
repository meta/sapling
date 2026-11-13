/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::marker::PhantomData;

use super::ascii::AsciiRenderer;
use super::ascii_large::AsciiLargeRenderer;
use super::box_drawing::BoxDrawingRenderer;
use super::render::GraphRow;
use super::render::Renderer;
use crate::pipeline::coloring::PrefixLineColor;
use crate::pipeline::types::TextLine;

#[cfg(feature = "coloring")]
const PLAIN: PrefixLineColor = None;
#[cfg(not(feature = "coloring"))]
const PLAIN: PrefixLineColor = ();

pub(crate) const DEFAULT_MIN_ROW_HEIGHT: usize = 2;

#[derive(Clone, Copy, Debug)]
pub struct OutputRendererOptions {
    pub min_row_height: usize,
    pub stagger_consecutive_disconnected_nodes: bool,
    pub graph_row_shape_style: crate::pipeline::types::GraphRowShapeStyle,
}

/// Common stateful text line output utilities shared by ASCII and box-drawing
/// renderers.
#[derive(Default)]
pub(crate) struct OutputRendererState {
    queued_pad_line: Option<TextLine>,
    /// If > 1, no need for an extra separator for the next row.
    /// Separator is only needed for adjacent single-line node lines.
    /// If there are link, pad, term, or other kinds of node lines,
    /// no need to draw separator.
    row_height: usize,
}

impl OutputRendererState {
    pub(crate) fn begin_row(
        &mut self,
        out: &mut impl FnMut(&str, &PrefixLineColor),
        separator_line: bool,
    ) {
        if let Some(line) = self.queued_pad_line.take() {
            self.push_line_with_message(
                out,
                line.iter().map(|(text, color)| (text.as_str(), color)),
                None,
                "",
            );
        }
        if separator_line && self.row_height == 1 {
            out("\n", &PLAIN);
        }
        self.row_height = 0;
    }

    pub(crate) fn push_line_with_message<'a>(
        &mut self,
        out: &mut impl FnMut(&str, &PrefixLineColor),
        prefix: impl ExactSizeIterator<Item = (&'a str, &'a PrefixLineColor)>
        + DoubleEndedIterator
        + Clone,
        message: Option<&str>,
        text_gap: &str,
    ) {
        if let Some(message) = message.map(str::trim_end).filter(|text| !text.is_empty()) {
            for (text, color) in prefix {
                if !text.is_empty() {
                    out(text, color);
                }
            }
            if !text_gap.is_empty() {
                out(text_gap, &PLAIN);
            }
            out(message, &PLAIN);
        } else if let Some((last, _)) = prefix
            .clone()
            .enumerate()
            .rev()
            .find(|(_, (text, _))| !text.trim_end().is_empty())
        {
            // Find the final visible fragment before streaming any text.
            for (index, (text, color)) in prefix.enumerate().take(last + 1) {
                let text = if index == last { text.trim_end() } else { text };
                if !text.is_empty() {
                    out(text, color);
                }
            }
        }
        out("\n", &PLAIN);
        self.row_height = self.row_height.saturating_add(1);
    }

    pub(crate) fn push_pad_lines<'a, 'b>(
        &mut self,
        out: &mut impl FnMut(&str, &PrefixLineColor),
        prefix: impl ExactSizeIterator<Item = (&'a str, &'a PrefixLineColor)>
        + DoubleEndedIterator
        + Clone,
        message_lines: impl Iterator<Item = &'b str>,
        text_gap: &str,
    ) -> bool {
        let mut emitted = false;
        for message in message_lines {
            self.push_line_with_message(out, prefix.clone(), Some(message), text_gap);
            emitted = true;
        }
        emitted
    }

    pub(crate) fn queue_pad_line(&mut self, pad_line: TextLine) {
        self.queued_pad_line = Some(pad_line);
    }
}

/// Legacy text-renderer builder.
/// New options since renderdag 0.2 might be ignored.
/// Use [`crate::pipeline::GraphTextRenderer::configure`] for all options.
pub struct OutputRendererBuilder<N, R>
where
    R: Renderer<N, Output = GraphRow<N>> + Sized,
{
    inner: R,
    _phantom: PhantomData<N>,
}

impl Default for OutputRendererOptions {
    fn default() -> Self {
        Self {
            min_row_height: DEFAULT_MIN_ROW_HEIGHT,
            stagger_consecutive_disconnected_nodes: false,
            graph_row_shape_style: Default::default(),
        }
    }
}

impl<N, R> OutputRendererBuilder<N, R>
where
    R: Renderer<N, Output = GraphRow<N>> + Sized,
{
    pub fn new(inner: R) -> Self {
        OutputRendererBuilder {
            inner,
            _phantom: PhantomData,
        }
    }

    pub fn with_options(mut self, options: OutputRendererOptions) -> Self {
        *self.inner.output_options_mut() = options;
        self
    }

    pub fn with_min_row_height(mut self, min_row_height: usize) -> Self {
        self.inner.output_options_mut().min_row_height = min_row_height;
        self
    }

    pub fn with_stagger_consecutive_disconnected_nodes(mut self, stagger: bool) -> Self {
        self.inner
            .output_options_mut()
            .stagger_consecutive_disconnected_nodes = stagger;
        self
    }

    pub fn build_ascii(self) -> AsciiRenderer<N, R> {
        AsciiRenderer::new(self.inner)
    }

    pub fn build_ascii_large(self) -> AsciiLargeRenderer<N, R> {
        AsciiLargeRenderer::new(self.inner)
    }

    pub fn build_box_drawing(self) -> BoxDrawingRenderer<N, R> {
        BoxDrawingRenderer::new(self.inner)
    }
}
