/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use crate::pipeline::graph_to_row_shape::GraphRowShaper;
use crate::pipeline::prefix_lines_to_text::PrefixLinesToText;
use crate::pipeline::row_shape_to_prefix_lines::box_drawing::BoxDrawingPrefixLineRenderer;
use crate::pipeline::types::Ancestor;
use crate::pipeline::types::GraphRowShapeOptions;
use crate::pipeline::types::PrefixLine;
use crate::pipeline::types::PrefixLineRenderer;

/// A convenience renderer that runs all text rendering pipeline stages.
///
/// This keeps the individual pipeline stages available for callers that need
/// to cache or replace one stage, while keeping the common streaming text path
/// short.
pub struct GraphTextRenderer<N> {
    row_shaper: GraphRowShaper<N>,
    // Use trait object instead of a type parameter to make runtime switch easier.
    prefix_lines: Box<dyn PrefixLineRenderer<N> + Send + Sync + 'static>,
    text: PrefixLinesToText,
}

impl<N> GraphTextRenderer<N>
where
    N: Clone + Eq,
{
    /// Create a text renderer with default options and prefix renderer.
    pub fn new<P>() -> Self
    where
        P: PrefixLineRenderer<N> + Default + Send + Sync + 'static,
    {
        Self::with_prefix_lines(Box::new(P::default()))
    }
}

impl<N> GraphTextRenderer<N>
where
    N: Clone + Eq,
{
    /// Create a text renderer with a custom prefix renderer.
    pub fn with_prefix_lines(
        prefix_lines: Box<dyn PrefixLineRenderer<N> + Send + Sync + 'static>,
    ) -> Self {
        Self {
            row_shaper: GraphRowShaper::new(),
            prefix_lines,
            text: PrefixLinesToText::new(),
        }
    }

    /// Configure options.
    pub fn configure(mut self, func: impl FnOnce(&mut GraphRowShapeOptions)) -> Self {
        func(self.row_shaper.options_mut());
        self.text = self
            .text
            .with_text_gap(self.row_shaper.options().graph_row_shape_style.text_gap)
            .with_link_line_placement(
                self.row_shaper
                    .options()
                    .graph_row_shape_style
                    .link_line_placement,
            );
        self
    }

    /// Reserve a column for a node before it is rendered.
    pub fn reserve(&mut self, node: N) {
        self.row_shaper.reserve(node);
    }

    /// Render the next node into text.
    pub fn next_text(
        &mut self,
        node: N,
        parents: Vec<Ancestor<N>>,
        glyph: &str,
        message: &str,
    ) -> String {
        let mut out = String::new();
        self.write_next_text(&mut out, node, parents, glyph, message);
        out
    }

    /// Write the next rendered node into `out`.
    pub fn write_next_text(
        &mut self,
        out: &mut String,
        node: N,
        parents: Vec<Ancestor<N>>,
        glyph: &str,
        message: &str,
    ) {
        let row_shape = self.row_shaper.next_row_shape(node, parents);
        let separator_line = row_shape.separator_line;
        let prefix_lines = self.prefix_lines.next_prefix_lines(&row_shape);
        self.text.write_next_text(
            out,
            prefix_lines,
            separator_line,
            glyph,
            message,
            self.row_shaper.options().min_row_height,
        );
    }

    /// Calculate the next prefix lines.
    pub fn next_prefix_lines(&mut self, node: N, parents: Vec<Ancestor<N>>) -> Vec<PrefixLine> {
        let row_shape = self.row_shaper.next_row_shape(node, parents);
        self.prefix_lines.next_prefix_lines(&row_shape)
    }
}

impl<N> Default for GraphTextRenderer<N>
where
    N: Clone + Eq,
{
    fn default() -> Self {
        Self::new::<BoxDrawingPrefixLineRenderer>()
    }
}
