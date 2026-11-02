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

pub(crate) const DEFAULT_MIN_ROW_HEIGHT: usize = 2;

#[derive(Clone, Copy, Debug)]
pub struct OutputRendererOptions {
    pub min_row_height: usize,
    pub stagger_consecutive_disconnected_nodes: bool,
    pub graph_row_shape_style: crate::pipeline::types::GraphRowShapeStyle,
}

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
