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
use crate::pipeline::row_shape_to_prefix_lines::ascii::AsciiPrefixLineRenderer;
use crate::pipeline::types::GraphRowShape;
use crate::pipeline::types::PrefixLineRenderer;

pub struct AsciiRenderer<N, R>
where
    R: Renderer<N, Output = GraphRow<N>> + Sized,
{
    inner: R,
    prefix_lines: AsciiPrefixLineRenderer,
    text: PrefixLinesToText,
    _phantom: PhantomData<N>,
}

impl<N, R> AsciiRenderer<N, R>
where
    R: Renderer<N, Output = GraphRow<N>> + Sized,
{
    pub(crate) fn new(inner: R) -> Self {
        AsciiRenderer {
            inner,
            prefix_lines: AsciiPrefixLineRenderer::new(),
            text: PrefixLinesToText::new(),
            _phantom: PhantomData,
        }
    }

    fn options(&self) -> &OutputRendererOptions {
        self.inner.output_options()
    }
}

impl<N, R> Renderer<N> for AsciiRenderer<N, R>
where
    N: Clone + Eq,
    R: Renderer<N, Output = GraphRow<N>> + Sized,
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
            #[cfg(feature = "coloring")]
            line_ids: None,
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
    use crate::GraphRowRenderer;

    fn render(fixture: &TestFixture) -> String {
        let mut renderer = GraphRowRenderer::new().output().build_ascii();
        render_string(fixture, &mut renderer)
    }

    #[test]
    fn basic() {
        assert_eq!(
            render(&test_fixtures::BASIC),
            r#"
            o  C
            |
            o  B
            |
            o  A"#
        );
    }

    #[test]
    fn basic_disconnected() {
        assert_eq!(
            render(&test_fixtures::BASIC_DISCONNECTED),
            r#"
            o  D
            |
            o  C
            
            o  B
            
            o  A"#
        );
    }

    #[test]
    fn basic_disconnected_min_row_height_1() {
        let mut renderer = GraphRowRenderer::new()
            .output()
            .with_min_row_height(1)
            .build_ascii();
        assert_eq!(
            render_string(&test_fixtures::BASIC_DISCONNECTED, &mut renderer),
            r#"
            o  D
            o  C
            
            o  B
            
            o  A"#
        );
    }

    #[test]
    fn basic_disconnected_min_row_height_1_staggered() {
        let mut renderer = GraphRowRenderer::new()
            .output()
            .with_min_row_height(1)
            .with_stagger_consecutive_disconnected_nodes(true)
            .build_ascii();
        assert_eq!(
            render_string(&test_fixtures::BASIC_DISCONNECTED, &mut renderer),
            r#"
            o  D
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
            |
            o    V
            |\
            | o    U
            | |\
            | | o  T
            | | |
            | o |  S
            |   |
            o   |  R
            |   |
            o   |  Q
            |\  |
            | o |    P
            | +---.
            | | | o  O
            | | | |
            | | | o    N
            | | | |\
            | o | | |  M
            | | | | |
            | o | | |  L
            | | | | |
            o | | | |  K
            +-------'
            o | | |  J
            | | | |
            o | | |  I
            |/  | |
            o   | |  H
            |   | |
            o   | |  G
            +-----+
            |   | o  F
            |   |/
            |   o  E
            |   |
            o   |  D
            |   |
            o   |  C
            +---'
            o  B
            |
            o  A"#
        );
    }

    #[test]
    fn octopus_branch_and_merge() {
        assert_eq!(
            render(&test_fixtures::OCTOPUS_BRANCH_AND_MERGE),
            r#"
            o      J
            +-+-.
            | | o  I
            | | |
            | o |      H
            +-+-+-+-.
            | | | | o  G
            | | | | |
            | | | o |  E
            | | | |/
            | | o |  D
            | | |\|
            | o | |  C
            | +---'
            o | |  F
            |/  |
            o   |  B
            +---'
            o  A"#
        );
    }

    #[test]
    fn reserved_column() {
        assert_eq!(
            render(&test_fixtures::RESERVED_COLUMN),
            r#"
              o  Z
              |
              o  Y
              |
              o  X
             /
            | o  W
            |/
            o  G
            |
            o    F
            |\
            | o  E
            | |
            | o  D
            |
            o  C
            |
            o  B
            |
            o  A"#
        );
    }

    #[test]
    fn ancestors() {
        assert_eq!(
            render(&test_fixtures::ANCESTORS),
            r#"
              o  Z
              |
              o  Y
             /
            o  F
            .
            . o  X
            ./
            | o  W
            |/
            o  E
            .
            o    D
            |\
            | .
            | o  C
            | .
            o .  B
            |/
            o  A"#
        );
    }

    #[test]
    fn split_parents() {
        assert_eq!(
            render(&test_fixtures::SPLIT_PARENTS),
            r#"
                  o  E
            .-+-+-+
            . | | .
            . o | .  D
            ./ \| .
            |   | .
            |   o .  C
            |   |/
            o   |  B
            +---'
            o  A"#
        );
    }

    #[test]
    fn terminations() {
        assert_eq!(
            render(&test_fixtures::TERMINATIONS),
            r#"
              o  K
              |
              | o  J
              |/
              o    I
             /|\
            | | |
            | ~ |
            |   |
            |   o  H
            |   |
            o   |  E
            +---'
            o  D
            |
            ~
            
            o  C
            |
            o  B
            |
            ~"#
        );
    }

    #[test]
    fn long_messages() {
        assert_eq!(
            render(&test_fixtures::LONG_MESSAGES),
            r#"
            o      F
            +-+-.  very long message 1
            | | |  very long message 2
            | | ~  very long message 3
            | |
            | |    very long message 4
            | |    very long message 5
            | |    very long message 6
            | |
            | o  E
            | |
            | o  D
            | |
            o |  C
            |/   long message 1
            |    long message 2
            |    long message 3
            |
            o  B
            |
            o  A
            |  long message 1
            ~  long message 2
               long message 3"#
        );
    }
}
