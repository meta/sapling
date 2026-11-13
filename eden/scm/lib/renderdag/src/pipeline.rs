/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Render a DAG into text using a pipeline:
//!
//! 1. Node stream `(node, parents)` -> `GraphRowShape`.
//!    Computes edge shapes and column layout.
//! 2. `GraphRowShape`s -> `PrefixLine`s.
//!    Converts abstract graph rows into left-side graph prefixes with optional line IDs.
//!    Does not know about messages or node glyphs.
//! 3. `PrefixLine`s + glyph + message -> text fragments.
//!    Produces the final lines by filling the glyph, repeating prefixes,
//!    and placing message text. Fragments retain IDs for styling by the caller.
//!
//! Design principle: minimal coupling -> reusable, cacheable. For example:
//! - Step 1 is not coupled with glyph or message.
//!   It can be serialized with serde for SVG rendering.
//!   It takes options that might affect graph layout (reserved column,
//!   stagger consecutive disconnected nodes).
//! - Step 2 is not coupled with message.
//!   It can be used or cached standalone. For example, the callsite
//!   might want to show test signals per commit. It might use a placeholder
//!   initially, and replace it with the real test status later. It can cache
//!   Step 2 result (since the commit graph is not changing), and only update
//!   the message part later.

#[cfg(feature = "coloring")]
pub mod coloring;
#[cfg(not(feature = "coloring"))]
pub mod coloring {
    /// No color metadata is stored when graph coloring is disabled.
    pub type PrefixLineColor = ();
}
pub mod graph_text_renderer;
pub mod graph_to_row_shape;
pub mod prefix_lines_to_text;
pub mod row_shape_to_prefix_lines;
pub mod types;

// re-export
pub use self::graph_text_renderer::GraphTextRenderer;
pub use self::graph_to_row_shape::GraphRowShaper;
pub use self::prefix_lines_to_text::PrefixLinesToText;
pub use self::row_shape_to_prefix_lines::ascii::AsciiPrefixLineRenderer as Ascii;
pub use self::row_shape_to_prefix_lines::ascii_large::AsciiLargePrefixLineRenderer as AsciiLarge;
pub use self::row_shape_to_prefix_lines::box_drawing::BoxDrawingPrefixLineRenderer as BoxDrawing;
pub use self::types::DashStyle;
pub use self::types::GraphRowShapeStyle;
pub use self::types::LinkLinePlacement;
pub use self::types::PrefixLineRenderer;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::indent;

    #[cfg(not(feature = "coloring"))]
    #[test]
    fn test_disabled_prefix_color_size() {
        use types::PrefixLinePart;

        assert_eq!(std::mem::size_of::<coloring::PrefixLineColor>(), 0);
        assert_eq!(
            std::mem::size_of::<(PrefixLinePart, coloring::PrefixLineColor)>(),
            std::mem::size_of::<PrefixLinePart>(),
        );
    }

    #[test]
    fn test_graph_text_renderer_example() {
        let mut renderer = GraphTextRenderer::<&'static str>::default().configure(|o| {
            o.min_row_height = 0;
        });
        assert_eq!(
            renderer.next_text("A", vec![], "x", "commit 1"),
            "x  commit 1\n"
        );
    }

    #[test]
    fn test_deciding_format_at_runtime() {
        use row_shape_to_prefix_lines::box_drawing::Square;
        for i in 0..=1 {
            let prefix_lines = match i {
                0 => Box::new(BoxDrawing::<Square>::new())
                    as Box<dyn PrefixLineRenderer<&'static str> + Send + Sync + 'static>,
                _ => Box::new(AsciiLarge) as Box<_>,
            };
            let _ = GraphTextRenderer::<&'static str>::with_prefix_lines(prefix_lines);
        }
    }

    #[test]
    fn test_box_drawing_column_spacing() {
        use crate::render::Ancestor;

        let render = |spacing| {
            let mut renderer = GraphTextRenderer::<&str>::default().configure(|options| {
                options.graph_row_shape_style.column_spacing = spacing;
            });
            renderer.reserve("A");
            renderer.reserve("B");
            indent(&renderer.next_text(
                "F",
                vec![
                    Ancestor::Parent("A"),
                    Ancestor::Ancestor("B"),
                    Ancestor::Anonymous,
                ],
                "o",
                "F\nmessage 1\nmessage 2\nmessage 3\nmessage 4\nmessage 5",
            ))
        };
        assert_eq!(
            render(3),
            r#"
                    o    F
            ╭───┬───┤    message 1
            │   ╷   │    message 2
            │   ╷   ~    message 3
            │   ╷        message 4
            │   ╷        message 5"#,
        );
        assert_eq!(
            render(0),
            r#"
              o F
            ╭┬┤ message 1
            │╷│ message 2
            │╷~ message 3
            │╷  message 4
            │╷  message 5"#,
        );
    }

    #[test]
    fn test_box_drawing_column_spacing_glyph_sets() {
        use row_shape_to_prefix_lines::box_drawing::DecGraphics;
        use row_shape_to_prefix_lines::box_drawing::Square;

        use crate::render::Ancestor;

        let square = BoxDrawing::<Square>::new();
        let mut renderer = GraphTextRenderer::with_prefix_lines(Box::new(square)).configure(|o| {
            o.graph_row_shape_style.column_spacing = 3;
        });
        renderer.reserve("A");
        let out = renderer.next_text("B", vec![Ancestor::Parent("A")], "o", "B\nlink");
        assert_eq!(out, "    o    B\n┌───┘    link\n");

        let dec = BoxDrawing::<DecGraphics>::new();
        let mut renderer = GraphTextRenderer::with_prefix_lines(Box::new(dec)).configure(|o| {
            o.graph_row_shape_style.column_spacing = 3;
        });
        renderer.reserve("A");
        let out = renderer.next_text("B", vec![Ancestor::Parent("A")], "o", "B\nlink");
        assert_eq!(out, "    o    B\n\x1B(0lqqq\x1B(B\x1B(0j   \x1B(B link\n");
    }

    #[test]
    fn test_ascii_column_spacing() {
        use crate::render::Ancestor;

        let render = |large, spacing| {
            let prefixes: Box<dyn PrefixLineRenderer<&str> + Send + Sync> = if large {
                Box::new(AsciiLarge)
            } else {
                Box::new(Ascii)
            };
            let mut renderer = GraphTextRenderer::with_prefix_lines(prefixes).configure(|o| {
                o.min_row_height = 0;
                o.graph_row_shape_style.column_spacing = spacing;
            });
            let mut out = renderer.next_text(
                "F",
                vec![Ancestor::Parent("A"), Ancestor::Parent("B")],
                "o",
                "F",
            );
            out.push_str(&renderer.next_text("B", vec![Ancestor::Parent("A")], "o", "B"));
            out.push_str(&renderer.next_text("A", vec![], "o", "A"));
            indent(&out)
        };
        assert_eq!(
            render(false, 3),
            r#"
            o        F
            |\
            | \
            |  \
            |   o    B
            |  /
            | /
            |/
            o    A"#,
        );
        assert_eq!(
            render(true, 3),
            r#"
            o      F
            |\_
            |  \
            |   o  B
            | _/
            |/
            o  A"#,
        );
        assert_eq!(render(false, 0), render(false, 1));
        assert_eq!(render(true, 0), render(true, 2));
        assert_eq!(render(true, 1), render(true, 2));
    }

    #[test]
    fn test_ascii_large_wider_column_connections() {
        use crate::render::Ancestor;

        let render_links = |spacing, parent| {
            let mut renderer = GraphTextRenderer::<&str>::new::<AsciiLarge>().configure(|o| {
                o.min_row_height = 0;
                o.graph_row_shape_style.column_spacing = spacing;
            });
            renderer.reserve("A");
            renderer.reserve("B");
            let rendered = renderer.next_text("C", vec![parent], "o", "C");
            indent(
                &rendered
                    .lines()
                    .skip(1)
                    .take(2)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        };
        assert_eq!(
            render_links(3, Ancestor::Parent("A")),
            r#"
              _____/
             /"#,
        );
        assert_eq!(
            render_links(4, Ancestor::Parent("A")),
            r#"
              _______/
             /"#,
        );
        assert_eq!(
            render_links(4, Ancestor::Ancestor("A")),
            r#"
              ........
             ."#,
        );
    }

    #[test]
    fn test_row_shape_derived_padding_lines() {
        use crate::pipeline::types::GraphRowShape;
        use crate::render::NodeLine;
        use crate::render::PadLine;

        let row = GraphRowShape {
            node: "A",
            merge: false,
            separator_line: false,
            style: Default::default(),
            is_head: true,
            is_root: false,
            node_column: 0,
            parent_columns: vec![1],
            node_line: vec![
                NodeLine::Node,
                NodeLine::Parent,
                NodeLine::Ancestor,
                NodeLine::Blank,
            ],
            link_line: None,
            term_line: None,
            ancestry_line: vec![PadLine::Ancestor, PadLine::Parent, PadLine::Blank],
            #[cfg(feature = "coloring")]
            line_ids: None,
        };

        assert!(row.has_indirect_ancestor());
        assert_eq!(
            row.pre_node_line(),
            vec![
                PadLine::Blank,
                PadLine::Parent,
                PadLine::Ancestor,
                PadLine::Blank,
            ],
        );
        assert_eq!(
            row.post_node_line(),
            vec![
                PadLine::Parent,
                PadLine::Parent,
                PadLine::Ancestor,
                PadLine::Blank,
            ],
        );
        assert_eq!(
            row.post_ancestry_line(),
            vec![PadLine::Parent, PadLine::Parent, PadLine::Blank],
        );
    }

    #[test]
    fn test_dash_style_once() {
        use crate::render::Ancestor;

        let render = |dash_style| {
            let mut renderer = GraphTextRenderer::<&str>::default().configure(|options| {
                options.graph_row_shape_style.dash_style = dash_style;
            });
            let mut out = renderer.next_text("A", vec![Ancestor::Ancestor("B")], "o", "A");
            out.push_str(&renderer.next_text("C", vec![], "o", "C"));
            out.push_str(&renderer.next_text("B", vec![], "o", "B"));
            indent(&out)
        };

        assert_eq!(
            render(DashStyle::Persistent),
            r#"
            o  A
            ╷
            ╷ o  C
            ╷
            o  B"#,
        );
        assert_eq!(
            render(DashStyle::Once),
            r#"
            o  A
            ┆
            │ o  C
            │
            o  B"#,
        );
    }

    #[test]
    fn test_node_padding_prefixes() {
        use crate::pipeline::types::PrefixLinePart;
        use crate::render::Ancestor;

        let render = |link_line_placement, dash_style| {
            let mut renderer = GraphTextRenderer::<&str>::default().configure(|options| {
                options.graph_row_shape_style.link_line_placement = link_line_placement;
                options.graph_row_shape_style.dash_style = dash_style;
            });
            renderer.reserve("A");
            renderer.reserve("B");
            let lines = renderer.next_prefix_lines(
                "F",
                vec![
                    Ancestor::Parent("A"),
                    Ancestor::Ancestor("B"),
                    Ancestor::Anonymous,
                ],
            );
            indent(
                &lines
                    .iter()
                    .map(|line| {
                        let text: String = line
                            .parts
                            .iter()
                            .map(|(part, _)| match part {
                                PrefixLinePart::Text(text) => text.as_str(),
                                PrefixLinePart::NodeGlyph => "o",
                            })
                            .collect();
                        format!("    {:<6}  {:?}", text.trim_end(), line.kind)
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        };

        // Placement changes message layout, so both styles expose the same prefixes.
        for placement in [LinkLinePlacement::Top, LinkLinePlacement::Bottom] {
            assert_eq!(
                render(placement, DashStyle::Persistent),
                r#"
                        PreNode
                    o   Node
                    │   PostNode
                ╭─┬─┤   Link
                │ ╷ │   Term
                │ ╷ ~   Term
                │ ╷     Ancestry
                │ ╷     PostAncestry"#,
            );
            assert_eq!(
                render(placement, DashStyle::Once),
                r#"
                        PreNode
                    o   Node
                    │   PostNode
                ╭─┬─┤   Link
                │ ┆ │   Term
                │ ┆ ~   Term
                │ ┆     Ancestry
                │ │     PostAncestry"#,
            );
        }
    }

    #[test]
    fn test_ancestry_with_single_line_message() {
        use crate::render::Ancestor;

        // Show the ancestry marker even when there are no message lines to pad.
        let render = |dash_style| {
            let mut renderer = GraphTextRenderer::<&str>::default().configure(|options| {
                options.min_row_height = 0;
                options.graph_row_shape_style.dash_style = dash_style;
            });
            let mut out = renderer.next_text("A", vec![Ancestor::Ancestor("B")], "o", "A");
            out.push_str(&renderer.next_text("B", vec![], "o", "B"));
            indent(&out)
        };
        assert_eq!(
            render(DashStyle::Once),
            r#"
            o  A
            ┆
            o  B"#,
        );
        assert_eq!(
            render(DashStyle::Persistent),
            r#"
            o  A
            ╷
            o  B"#,
        );
    }
    #[test]
    fn test_dash_style_once_with_long_message() {
        use crate::render::Ancestor;

        // Extra message lines use parent padding after the one-shot ancestry marker.
        let mut renderer = GraphTextRenderer::<&str>::default().configure(|options| {
            options.min_row_height = 0;
            options.graph_row_shape_style.dash_style = DashStyle::Once;
        });
        let mut out = renderer.next_text(
            "A",
            vec![Ancestor::Ancestor("B")],
            "o",
            "A\nmessage 1\nmessage 2\nmessage 3",
        );
        out.push_str(&renderer.next_text("B", vec![], "o", "B"));
        let rendered = indent(&out);
        assert_eq!(
            rendered,
            r#"
            o  A
            ┆  message 1
            │  message 2
            │  message 3
            o  B"#,
        );
    }

    #[test]
    fn test_placements() {
        use crate::render::Ancestor;

        let mut renderer = GraphTextRenderer::<&str>::default();
        renderer.reserve("A");
        renderer.reserve("B");
        let prefixes = renderer.next_prefix_lines(
            "F",
            vec![
                Ancestor::Parent("A"),
                Ancestor::Ancestor("B"),
                Ancestor::Anonymous,
            ],
        );
        // Reuse the same prefixes; only the text stage chooses message placement.
        let render = |placement| {
            let mut text = PrefixLinesToText::new().with_link_line_placement(placement);
            let out = text.next_text(
                prefixes.clone(),
                false,
                "o",
                "F\nmessage 1\nmessage 2\nmessage 3\nmessage 4\nmessage 5\nmessage 6",
                0,
            );
            indent(&out)
        };
        assert_eq!(
            render(LinkLinePlacement::Top),
            r#"
                o  F
            ╭─┬─┤  message 1
            │ ╷ │  message 2
            │ ╷ ~  message 3
            │ ╷    message 4
            │ ╷    message 5
            │ ╷    message 6"#,
        );
        assert_eq!(
            render(LinkLinePlacement::Bottom),
            r#"
                o  F
                │  message 1
                │  message 2
            ╭─┬─┤  message 3
            │ ╷ │  message 4
            │ ╷ ~  message 5
            │ ╷    message 6"#,
        );
    }

    #[test]
    fn test_text_gap_with_cached_prefixes() {
        use crate::render::Ancestor;

        let mut renderer = GraphTextRenderer::<&str>::default();
        renderer.reserve("A");
        renderer.reserve("B");
        let prefixes = renderer.next_prefix_lines(
            "F",
            vec![
                Ancestor::Parent("A"),
                Ancestor::Ancestor("B"),
                Ancestor::Anonymous,
            ],
        );
        let render = |placement, gap| {
            let mut text = PrefixLinesToText::new()
                .with_link_line_placement(placement)
                .with_text_gap(gap);
            indent(&text.next_text(
                prefixes.clone(),
                false,
                "o",
                "F\nmessage 1\nmessage 2\nmessage 3\nmessage 4\nmessage 5\nmessage 6",
                0,
            ))
        };
        assert_eq!(
            render(LinkLinePlacement::Top, 0),
            r#"
                o F
            ╭─┬─┤ message 1
            │ ╷ │ message 2
            │ ╷ ~ message 3
            │ ╷   message 4
            │ ╷   message 5
            │ ╷   message 6"#,
        );
        assert_eq!(
            render(LinkLinePlacement::Bottom, 0),
            r#"
                o F
                │ message 1
                │ message 2
            ╭─┬─┤ message 3
            │ ╷ │ message 4
            │ ╷ ~ message 5
            │ ╷   message 6"#,
        );
        assert_eq!(
            render(LinkLinePlacement::Bottom, 3),
            r#"
                o    F
                │    message 1
                │    message 2
            ╭─┬─┤    message 3
            │ ╷ │    message 4
            │ ╷ ~    message 5
            │ ╷      message 6"#,
        );
    }

    #[test]
    fn test_text_gap_configure() {
        use row_shape_to_prefix_lines::box_drawing::Curved;

        use crate::render::Ancestor;

        let renderers: [(char, Box<dyn PrefixLineRenderer<&str> + Send + Sync>); 3] = [
            ('│', Box::new(BoxDrawing::<Curved>::new())),
            ('|', Box::new(Ascii)),
            ('|', Box::new(AsciiLarge)),
        ];
        for (parent, prefixes) in renderers {
            let mut renderer = GraphTextRenderer::with_prefix_lines(prefixes).configure(|o| {
                o.graph_row_shape_style.text_gap = 0;
            });
            let out = renderer.next_text("A", vec![Ancestor::Parent("B")], "o", "A\ncontinued");
            assert_eq!(out, format!("o A\n{parent} continued\n"));
        }
        let mut renderer = GraphTextRenderer::<&str>::default().configure(|o| {
            o.graph_row_shape_style.column_spacing = 0;
            o.graph_row_shape_style.text_gap = 0;
        });
        assert_eq!(
            renderer.next_text("A", vec![Ancestor::Parent("B")], "o", "A\ncontinued"),
            "oA\n│continued\n",
        );
    }

    #[test]
    fn test_bottom_terminations() {
        use crate::render::Ancestor;

        let render = |placement, message| {
            let mut renderer = GraphTextRenderer::<&str>::default().configure(|options| {
                options.min_row_height = 0;
                options.graph_row_shape_style.link_line_placement = placement;
            });
            renderer.reserve("A");
            renderer.reserve("B");
            let mut out = renderer.next_text(
                "F",
                vec![
                    Ancestor::Parent("A"),
                    Ancestor::Parent("B"),
                    Ancestor::Anonymous,
                ],
                "o",
                message,
            );
            out.push_str(&renderer.next_text("A", vec![], "o", "A"));
            out.push_str(&renderer.next_text("B", vec![], "o", "B"));
            indent(&out)
        };
        let long = "F\nmessage 1\nmessage 2\nmessage 3\nmessage 4\nmessage 5";
        assert_eq!(
            render(LinkLinePlacement::Top, long),
            r#"
                o  F
            ╭─┬─┤  message 1
            │ │ │  message 2
            │ │ ~  message 3
            │ │    message 4
            │ │    message 5
            o │  A
              o  B"#,
        );
        // Padding before Link does not replace continuation after the anonymous termination.
        assert_eq!(
            render(LinkLinePlacement::Bottom, long),
            r#"
                o  F
                │  message 1
                │  message 2
            ╭─┬─┤  message 3
            │ │ │  message 4
            │ │ ~  message 5
            │ │
            o │  A
              o  B"#,
        );
        assert_eq!(
            render(LinkLinePlacement::Bottom, "F"),
            r#"
                o  F
            ╭─┬─┤
            │ │ │
            │ │ ~
            │ │
            o │  A
              o  B"#,
        );
    }

    #[test]
    fn test_bottom_without_link_line() {
        use crate::render::Ancestor;

        let render = |message, min_row_height| {
            let mut renderer = GraphTextRenderer::<&str>::default().configure(|options| {
                options.min_row_height = min_row_height;
                options.graph_row_shape_style = GraphRowShapeStyle {
                    dash_style: DashStyle::Once,
                    link_line_placement: LinkLinePlacement::Bottom,
                    ..Default::default()
                };
            });
            let mut out = renderer.next_text("A", vec![Ancestor::Ancestor("B")], "o", message);
            out.push_str(&renderer.next_text("B", vec![], "o", "B"));
            indent(&out)
        };
        // The ancestry marker stays at the bottom even when no connecting Link line is needed.
        assert_eq!(
            render("A\nmessage 1\nmessage 2\nmessage 3", 0),
            r#"
            o  A
            │  message 1
            │  message 2
            ┆  message 3
            o  B"#,
        );
        assert_eq!(
            render("A", 0),
            r#"
            o  A
            ┆
            o  B"#,
        );
        assert_eq!(
            render("A", 5),
            r#"
            o  A
            │
            │
            │
            ┆
            o  B"#,
        );
    }

    #[test]
    fn test_once_glyphs() {
        use row_shape_to_prefix_lines::box_drawing::BoxDrawingGlyphSet;
        use row_shape_to_prefix_lines::box_drawing::Curved;
        use row_shape_to_prefix_lines::box_drawing::Square;

        use crate::render::Ancestor;

        enum Colon {}
        impl BoxDrawingGlyphSet for Colon {
            const GLYPHS: &'static [&'static str; 15] = &{
                let mut glyphs = *Curved::GLYPHS;
                glyphs[14] = ": ";
                glyphs
            };
        }

        let render = |prefix_renderer: Box<dyn PrefixLineRenderer<&str> + Send + Sync>| {
            let mut renderer =
                GraphTextRenderer::with_prefix_lines(prefix_renderer).configure(|options| {
                    options.min_row_height = 0;
                    options.graph_row_shape_style.dash_style = DashStyle::Once;
                });
            let mut out = renderer.next_text(
                "A",
                vec![Ancestor::Ancestor("B")],
                "o",
                "A\nmessage 1\nmessage 2",
            );
            out.push_str(&renderer.next_text("B", vec![], "o", "B"));
            indent(&out)
        };
        for ascii in [Box::new(Ascii) as Box<_>, Box::new(AsciiLarge) as Box<_>] {
            assert_eq!(
                render(ascii),
                r#"
            o  A
            :  message 1
            |  message 2
            o  B"#,
            );
        }
        assert_eq!(
            render(Box::new(BoxDrawing::<Square>::new())),
            r#"
            o  A
            ┆  message 1
            │  message 2
            o  B"#,
        );
        assert_eq!(
            render(Box::new(BoxDrawing::<Colon>::new())),
            r#"
            o  A
            :  message 1
            │  message 2
            o  B"#,
        );
    }
}
