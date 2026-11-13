/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::marker::PhantomData;

use super::output::OutputRendererOptions;
use super::render::Ancestor;
use super::render::GraphRow;
use super::render::Renderer;
use crate::pipeline::prefix_lines_to_text::PrefixLinesToText;
use crate::pipeline::row_shape_to_prefix_lines::box_drawing::BoxDrawingGlyphSet;
use crate::pipeline::row_shape_to_prefix_lines::box_drawing::BoxDrawingPrefixLineRenderer;
use crate::pipeline::row_shape_to_prefix_lines::box_drawing::Curved;
use crate::pipeline::row_shape_to_prefix_lines::box_drawing::DecGraphics;
use crate::pipeline::row_shape_to_prefix_lines::box_drawing::Square;
use crate::pipeline::types::GraphRowShape;
use crate::pipeline::types::PrefixLineRenderer;

pub struct BoxDrawingRenderer<N, R, G = Curved>
where
    R: Renderer<N, Output = GraphRow<N>> + Sized,
{
    inner: R,
    prefix_lines: BoxDrawingPrefixLineRenderer<G>,
    text: PrefixLinesToText,
    _phantom: PhantomData<N>,
}

impl<N, R> BoxDrawingRenderer<N, R, Curved>
where
    R: Renderer<N, Output = GraphRow<N>> + Sized,
{
    pub(crate) fn new(inner: R) -> Self {
        BoxDrawingRenderer {
            inner,
            prefix_lines: BoxDrawingPrefixLineRenderer::new(),
            text: PrefixLinesToText::new(),
            _phantom: PhantomData,
        }
    }
}

impl<N, R, G> BoxDrawingRenderer<N, R, G>
where
    R: Renderer<N, Output = GraphRow<N>> + Sized,
    G: BoxDrawingGlyphSet,
{
    pub fn with_square_glyphs(self) -> BoxDrawingRenderer<N, R, Square> {
        BoxDrawingRenderer {
            inner: self.inner,
            prefix_lines: self.prefix_lines.with_square_glyphs(),
            text: self.text,
            _phantom: PhantomData,
        }
    }

    pub fn with_dec_graphics_glyphs(self) -> BoxDrawingRenderer<N, R, DecGraphics> {
        BoxDrawingRenderer {
            inner: self.inner,
            prefix_lines: self.prefix_lines.with_dec_graphics_glyphs(),
            text: self.text,
            _phantom: PhantomData,
        }
    }

    fn options(&self) -> &OutputRendererOptions {
        self.inner.output_options()
    }
}

impl<N, R, G> Renderer<N> for BoxDrawingRenderer<N, R, G>
where
    N: Clone + Eq,
    R: Renderer<N, Output = GraphRow<N>> + Sized,
    G: BoxDrawingGlyphSet,
{
    type Output = String;

    fn width(&self, node: Option<&N>, parents: Option<&Vec<Ancestor<N>>>) -> u64 {
        self.inner
            .width(node, parents)
            .saturating_mul(2)
            .saturating_add(1)
    }

    fn reserve(&mut self, node: N) {
        self.inner.reserve(node);
    }

    fn next_row(
        &mut self,
        node: N,
        parents: Vec<Ancestor<N>>,
        glyph: String,
        message: String,
    ) -> String {
        let line = self.inner.next_row(node, parents, glyph, message);
        let glyph = line.glyph;
        let message = line.message;
        let separator_line = line.separator_line;
        let row_shape = GraphRowShape {
            node: line.node,
            merge: line.merge,
            separator_line,
            style: Default::default(),
            is_head: false,
            is_root: false,
            node_column: 0,
            parent_columns: Vec::new(),
            node_line: line.node_line,
            link_line: line.link_line,
            term_line: line.term_line,
            ancestry_line: line.pad_lines,
        };
        let prefix_lines = self.prefix_lines.next_prefix_lines(&row_shape);
        self.text.next_text(
            prefix_lines,
            separator_line,
            &glyph,
            &message,
            self.options().min_row_height,
        )
    }

    fn output_options_mut(&mut self) -> &mut OutputRendererOptions {
        self.inner.output_options_mut()
    }

    fn output_options(&self) -> &OutputRendererOptions {
        self.inner.output_options()
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_fixtures;
    use super::super::test_fixtures::TestFixture;
    use super::super::test_utils::render_string;
    use super::super::test_utils::render_string_with_order;
    use crate::GraphRowRenderer;

    fn render(fixture: &TestFixture) -> String {
        let mut renderer = GraphRowRenderer::new().output().build_box_drawing();
        render_string(fixture, &mut renderer)
    }

    #[test]
    fn basic() {
        assert_eq!(
            render(&test_fixtures::BASIC),
            r#"
            o  C
            │
            o  B
            │
            o  A"#
        );
    }

    #[test]
    fn basic_disconnected() {
        assert_eq!(
            render(&test_fixtures::BASIC_DISCONNECTED),
            r#"
            o  D
            │
            o  C
            
            o  B
            
            o  A"#
        );

        assert_eq!(
            render(&TestFixture {
                missing: &["C"],
                ..test_fixtures::BASIC_DISCONNECTED
            }),
            r#"
            o  D
            │
            ~
            
            o  B
            
            o  A"#
        );
    }

    #[test]
    fn basic_disconnected_min_row_height_1() {
        let get_renderer = || {
            GraphRowRenderer::new()
                .output()
                .with_min_row_height(1)
                .build_box_drawing()
        };
        let render = |t| render_string(t, &mut get_renderer());
        assert_eq!(
            render(&test_fixtures::BASIC_DISCONNECTED),
            r#"
            o  D
            o  C
            
            o  B
            
            o  A"#
        );

        // Suboptimal: extra blank line is unnecessary after "~".
        // Suboptimal: "|" is not necessary.
        assert_eq!(
            render(&TestFixture {
                missing: &["C"],
                ..test_fixtures::BASIC_DISCONNECTED
            }),
            r#"
            o  D
            │
            ~
            
            o  B
            
            o  A"#
        );

        assert_eq!(
            render(&TestFixture {
                messages: &[("C", "\n\n"), ("B", "\n")],
                ..test_fixtures::BASIC_DISCONNECTED
            }),
            r#"
            o  D
            o  C
            
            
            o  B
            
            o  A"#
        );

        assert_eq!(
            render(&TestFixture {
                messages: &[("C", "line 1\nline 2\n")],
                ..test_fixtures::BASIC_DISCONNECTED
            }),
            r#"
            o  D
            o  C
               line 1
               line 2
            o  B
            
            o  A"#
        );
    }

    #[test]
    fn basic_disconnected_min_row_height_0() {
        // jj-vcs sets min row height 0.
        let get_renderer = || {
            GraphRowRenderer::new()
                .output()
                .with_min_row_height(0)
                .build_box_drawing()
        };
        let render = |t| render_string(t, &mut get_renderer());
        assert_eq!(
            render(&test_fixtures::BASIC_DISCONNECTED),
            r#"
            o  D
            o  C
            
            o  B
            
            o  A"#
        );
    }

    #[test]
    fn basic_disconnected_staggered() {
        let get_renderer = |n| {
            GraphRowRenderer::new()
                .output()
                .with_min_row_height(n)
                .with_stagger_consecutive_disconnected_nodes(true)
                .build_box_drawing()
        };
        assert_eq!(
            render_string(&test_fixtures::BASIC_DISCONNECTED, &mut get_renderer(0)),
            r#"
            o  D
            o  C
              o  B
            o  A"#
        );

        assert_eq!(
            render_string(&test_fixtures::BASIC_DISCONNECTED, &mut get_renderer(1)),
            r#"
            o  D
            o  C
              o  B
            o  A"#
        );

        // Should not move "B" to a separate column.
        assert_eq!(
            render_string(&test_fixtures::BASIC_DISCONNECTED, &mut get_renderer(2)),
            r#"
            o  D
            │
            o  C
            
            o  B
            
            o  A"#
        );
    }

    #[test]
    fn branches_and_merges() {
        assert_eq!(
            render(&test_fixtures::BRANCHES_AND_MERGES),
            r#"
            o  W
            │
            o    V
            ├─╮
            │ o    U
            │ ├─╮
            │ │ o  T
            │ │ │
            │ o │  S
            │   │
            o   │  R
            │   │
            o   │  Q
            ├─╮ │
            │ o │    P
            │ ├───╮
            │ │ │ o  O
            │ │ │ │
            │ │ │ o    N
            │ │ │ ├─╮
            │ o │ │ │  M
            │ │ │ │ │
            │ o │ │ │  L
            │ │ │ │ │
            o │ │ │ │  K
            ├───────╯
            o │ │ │  J
            │ │ │ │
            o │ │ │  I
            ├─╯ │ │
            o   │ │  H
            │   │ │
            o   │ │  G
            ├─────╮
            │   │ o  F
            │   ├─╯
            │   o  E
            │   │
            o   │  D
            │   │
            o   │  C
            ├───╯
            o  B
            │
            o  A"#
        );
    }

    #[test]
    fn octopus_branch_and_merge() {
        assert_eq!(
            render(&test_fixtures::OCTOPUS_BRANCH_AND_MERGE),
            r#"
            o      J
            ├─┬─╮
            │ │ o  I
            │ │ │
            │ o │      H
            ╭─┼─┬─┬─╮
            │ │ │ │ o  G
            │ │ │ │ │
            │ │ │ o │  E
            │ │ │ ├─╯
            │ │ o │  D
            │ │ ├─╮
            │ o │ │  C
            │ ├───╯
            o │ │  F
            ├─╯ │
            o   │  B
            ├───╯
            o  A"#
        );
    }

    #[test]
    fn reserved_column() {
        assert_eq!(
            render(&test_fixtures::RESERVED_COLUMN),
            r#"
              o  Z
              │
              o  Y
              │
              o  X
            ╭─╯
            │ o  W
            ├─╯
            o  G
            │
            o    F
            ├─╮
            │ o  E
            │ │
            │ o  D
            │
            o  C
            │
            o  B
            │
            o  A"#
        );
    }

    #[test]
    fn ancestors() {
        assert_eq!(
            render(&test_fixtures::ANCESTORS),
            r#"
              o  Z
              │
              o  Y
            ╭─╯
            o  F
            ╷
            ╷ o  X
            ╭─╯
            │ o  W
            ├─╯
            o  E
            ╷
            o    D
            ├─╮
            │ ╷
            │ o  C
            │ ╷
            o ╷  B
            ├─╯
            o  A"#
        );
    }

    #[test]
    fn split_parents() {
        assert_eq!(
            render(&test_fixtures::SPLIT_PARENTS),
            r#"
                  o  E
            ╭─┬─┬─┤
            ╷ │ │ ╷
            ╷ o │ ╷  D
            ╭─┴─╮ ╷
            │   │ ╷
            │   o ╷  C
            │   ├─╯
            o   │  B
            ├───╯
            o  A"#
        );
    }

    #[test]
    fn terminations() {
        assert_eq!(
            render(&test_fixtures::TERMINATIONS),
            r#"
              o  K
              │
              │ o  J
              ├─╯
              o    I
            ╭─┼─╮
            │ │ │
            │ ~ │
            │   │
            │   o  H
            │   │
            o   │  E
            ├───╯
            o  D
            │
            ~
            
            o  C
            │
            o  B
            │
            ~"#
        );
    }

    #[test]
    fn long_messages() {
        assert_eq!(
            render(&test_fixtures::LONG_MESSAGES),
            r#"
            o      F
            ├─┬─╮  very long message 1
            │ │ │  very long message 2
            │ │ ~  very long message 3
            │ │
            │ │    very long message 4
            │ │    very long message 5
            │ │    very long message 6
            │ │
            │ o  E
            │ │
            │ o  D
            │ │
            o │  C
            ├─╯  long message 1
            │    long message 2
            │    long message 3
            │
            o  B
            │
            o  A
            │  long message 1
            ~  long message 2
               long message 3"#
        );
    }

    #[test]
    fn different_orders() {
        let order = |order: &str| {
            let order = order.matches(|_: char| true).collect::<Vec<_>>();
            let mut renderer = GraphRowRenderer::new().output().build_box_drawing();
            render_string_with_order(&test_fixtures::ORDERS1, &mut renderer, Some(&order))
        };

        assert_eq!(
            order("KJIHGFEDCBZA"),
            r#"
            o    K
            ├─╮
            │ o    J
            │ ├─╮
            │ │ o    I
            │ │ ├─╮
            │ │ │ o    H
            │ │ │ ├─╮
            │ │ │ │ o    G
            │ │ │ │ ├─╮
            o │ │ │ │ │  F
            │ │ │ │ │ │
            │ o │ │ │ │  E
            ├─╯ │ │ │ │
            │   o │ │ │  D
            ├───╯ │ │ │
            │     o │ │  C
            ├─────╯ │ │
            │       o │  B
            ├───────╯ │
            │         o  Z
            │
            o  A"#
        );

        assert_eq!(
            order("KJIHGZBCDEFA"),
            r#"
            o    K
            ├─╮
            │ o    J
            │ ├─╮
            │ │ o    I
            │ │ ├─╮
            │ │ │ o    H
            │ │ │ ├─╮
            │ │ │ │ o    G
            │ │ │ │ ├─╮
            │ │ │ │ │ o  Z
            │ │ │ │ │
            │ │ │ │ o  B
            │ │ │ │ │
            │ │ │ o │  C
            │ │ │ ├─╯
            │ │ o │  D
            │ │ ├─╯
            │ o │  E
            │ ├─╯
            o │  F
            ├─╯
            o  A"#
        );

        // Keeping the p1 branch the longest path (KFEDCBA) is a reasonable
        // optimization for a cleaner graph (less columns, more text space).
        assert_eq!(
            render(&test_fixtures::ORDERS2),
            r#"
            o    K
            ├─╮
            │ o  J
            │ │
            o │    F
            ├───╮
            │ │ o  I
            │ ├─╯
            o │    E
            ├───╮
            │ │ o  H
            │ ├─╯
            o │    D
            ├───╮
            │ │ o  G
            │ ├─╯
            o │    C
            ├───╮
            │ │ o  Z
            │ │
            o │  B
            ├─╯
            o  A"#
        );

        // Try to use the ORDERS2 order. However, the parent ordering in the
        // graph is different, which makes the rendering different.
        //
        // Note: it's KJFIEHDGCZBA in the ORDERS2 graph. To map it to ORDERS1,
        // follow:
        //
        // ORDERS1: KFJEIDHCGBZA
        // ORDERS2: KJFIEHDGCBZA
        //
        // And we get KFJEIDHCGZBA.
        assert_eq!(
            order("KFJEIDHCGZBA"),
            r#"
            o    K
            ├─╮
            o │  F
            │ │
            │ o    J
            │ ├─╮
            │ o │  E
            ├─╯ │
            │   o  I
            │ ╭─┤
            │ │ o  D
            ├───╯
            │ o    H
            │ ├─╮
            │ o │  C
            ├─╯ │
            │   o  G
            │ ╭─┤
            │ o │  Z
            │   │
            │   o  B
            ├───╯
            o  A"#
        );
    }
}
