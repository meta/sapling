/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Compare rule-based graph identities using SVG or terminal output.

use std::collections::HashSet;
use std::error::Error;
use std::fmt::Write as _;
use std::sync::Arc;

use renderdag::pipeline::GraphRowShaper;
use renderdag::pipeline::GraphTextRenderer;
use renderdag::pipeline::coloring::LineIdAllocator;
use renderdag::pipeline::coloring::LineIds;
use renderdag::pipeline::coloring::build_line_id_allocator_from_rules;
use renderdag::pipeline::types::Ancestor;
use renderdag::pipeline::types::GraphRowShape;
use renderdag::pipeline::types::LinkLine;
use renderdag::pipeline::types::NodeLine;
use renderdag::pipeline::types::PadLine;

use crate::cli::Format;
use crate::cli::Options;
use crate::graphs::Graph;

mod cli;
mod graphs;

const COLORS: [&str; 6] = [
    "#e5534b", "#57ab5a", "#c69026", "#539bf5", "#b083f0", "#39c5cf",
];
const COLUMN: f64 = 28.0;
const NODE_HEIGHT: f64 = 26.0;
const LINK_HEIGHT: f64 = 26.0;
const TERM_HEIGHT: f64 = 26.0;
const ANCESTRY_HEIGHT: f64 = 13.0;
const INTERSECT_GAP: f64 = 4.0;
const STROKE: f64 = 4.8;
const COLOR_DASH: f64 = 5.0;

#[derive(Default)]
struct Svg {
    body: String,
    missing_ids: usize,
    multi_id_segments: usize,
    multi_id_ancestors: usize,
}

impl Svg {
    fn stroke(&mut self, path: &str, ids: &LineIds, ancestor: bool) {
        if ids.is_empty() {
            self.missing_ids += 1;
            writeln!(
                self.body,
                r##"<path d="{path}" fill="none" stroke="#8b949e" stroke-width="{STROKE}"/>"##
            )
            .unwrap();
            return;
        }
        let count = ids.len();
        self.multi_id_segments += usize::from(count > 1);
        self.multi_id_ancestors += usize::from(count > 1 && ancestor);
        for (slot, id) in ids.iter().enumerate() {
            let color = COLORS[id % COLORS.len()];
            write!(self.body, r#"<path d="{path}" fill="none" stroke="{color}" stroke-width="{STROKE}" stroke-linecap="butt""#).unwrap();
            if count > 1 {
                // With multiple IDs, color alternation replaces ancestor dashes.
                let gap = COLOR_DASH * (count - 1) as f64;
                let offset = -COLOR_DASH * slot as f64;
                write!(
                    self.body,
                    r#" stroke-dasharray="{COLOR_DASH} {gap}" stroke-dashoffset="{offset}""#
                )
                .unwrap();
            } else if ancestor {
                self.body.push_str(r#" stroke-dasharray="3 3""#);
            }
            self.body.push_str("/>\n");
        }
    }

    fn vertical(&mut self, x: f64, top: f64, bottom: f64, ids: &LineIds, ancestor: bool) {
        // Overlap cell boundaries slightly to avoid rasterization seams.
        self.stroke(
            &format!("M {x} {} L {x} {}", top - 0.5, bottom + 0.5),
            ids,
            ancestor,
        );
    }

    fn horizontal(&mut self, y: f64, left: f64, right: f64, ids: &LineIds, ancestor: bool) {
        self.stroke(
            &format!("M {} {y} L {} {y}", left - 0.5, right + 0.5),
            ids,
            ancestor,
        );
    }

    fn corner(
        &mut self,
        horizontal: (f64, f64),
        vertical: (f64, f64),
        ids: &LineIds,
        ancestor: bool,
    ) {
        let (hx, hy) = horizontal;
        let (vx, vy) = vertical;
        // Like ISL, the cell center controls the quadratic curve. Also extend
        // the curve's endpoints past the cell, so adjacent curves overlap.
        let start = hx + if hx < vx { -0.5 } else { 0.5 };
        let end = vy + if vy < hy { -0.5 } else { 0.5 };
        self.stroke(
            &format!("M {start} {hy} L {hx} {hy} Q {vx} {hy} {vx} {vy} L {vx} {end}"),
            ids,
            ancestor,
        );
    }

    fn text(&mut self, x: f64, y: f64, text: &str, size: f64) {
        let text = text
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        writeln!(self.body, r##"<text x="{x}" y="{y}" fill="#e6edf3" font-family="monospace" font-size="{size}" dominant-baseline="middle">{text}</text>"##).unwrap();
    }
}

fn graph_nodes(graph: &Graph) -> Vec<(String, Vec<Ancestor<String>>)> {
    let parents = drawdag::parse(graph.dag);
    let mut seen = HashSet::new();
    let mut order = Vec::new();
    // Iterative postorder without recursion: visit heads in the given order
    // and parents in sorted order, matching renderdag's diagram fixtures.
    for head in graph.heads {
        let mut stack = vec![(head.to_string(), false)];
        while let Some((node, expanded)) = stack.pop() {
            if expanded {
                order.push(node);
            } else if seen.insert(node.clone()) {
                stack.push((node.clone(), true));
                for parent in parents[&node].iter().rev() {
                    stack.push((parent.clone(), false));
                }
            }
        }
    }
    order
        .into_iter()
        .rev()
        .filter(|node| !graph.missing.contains(&node.as_str()))
        .map(|node| {
            let edges = parents[&node]
                .iter()
                .map(|parent| {
                    if graph.missing.contains(&parent.as_str()) {
                        Ancestor::Anonymous
                    } else if graph.ancestors.contains(&(node.as_str(), parent.as_str())) {
                        Ancestor::Ancestor(parent.clone())
                    } else {
                        Ancestor::Parent(parent.clone())
                    }
                })
                .collect();
            (node, edges)
        })
        .collect()
}

fn copy_parents(parents: &[Ancestor<String>]) -> Vec<Ancestor<String>> {
    parents
        .iter()
        .map(|parent| match parent {
            Ancestor::Parent(node) => Ancestor::Parent(node.clone()),
            Ancestor::Ancestor(node) => Ancestor::Ancestor(node.clone()),
            Ancestor::Anonymous => Ancestor::Anonymous,
        })
        .collect()
}

fn shapes(
    graph: &Graph,
    nodes: &[(String, Vec<Ancestor<String>>)],
    allocator: Arc<LineIdAllocator<String>>,
) -> Vec<GraphRowShape<String>> {
    let mut shaper = GraphRowShaper::new().with_line_id_allocator(Some(allocator));
    for node in graph.reserve {
        shaper.reserve(node.to_string());
    }
    nodes
        .iter()
        .map(|(node, parents)| shaper.next_row_shape(node.clone(), copy_parents(parents)))
        .collect()
}

fn audit(rows: &[GraphRowShape<String>]) -> usize {
    let mut mismatches = 0;
    for row in rows {
        let ids = row.line_ids.as_deref().unwrap();
        assert_eq!(ids.before_link.len(), row.node_line.len());
        assert_eq!(ids.after_link.len(), row.ancestry_line.len());
        if let Some(cells) = &ids.link_line {
            assert_eq!(cells.len(), row.link_line.as_ref().unwrap().len());
            for (column, cell) in cells.iter().enumerate() {
                let top: LineIds = [&cell.vertical, &cell.left_merge, &cell.right_merge]
                    .into_iter()
                    .flat_map(LineIds::iter)
                    .collect();
                let bottom: LineIds = [&cell.vertical, &cell.left_fork, &cell.right_fork]
                    .into_iter()
                    .flat_map(LineIds::iter)
                    .collect();
                if column != row.node_column && top != ids.before_link[column] {
                    eprintln!(
                        "{} column {column}: link top {top:?} != incoming {:?}",
                        row.node, ids.before_link[column]
                    );
                    mismatches += 1;
                }
                if bottom != ids.after_link[column] {
                    eprintln!(
                        "{} column {column}: link bottom {bottom:?} != outgoing {:?}",
                        row.node, ids.after_link[column]
                    );
                    mismatches += 1;
                }
            }
            for (column, pair) in cells.windows(2).enumerate() {
                let right: LineIds = [
                    &pair[0].horizontal,
                    &pair[0].right_fork,
                    &pair[0].right_merge,
                ]
                .into_iter()
                .flat_map(LineIds::iter)
                .collect();
                let left: LineIds = [&pair[1].horizontal, &pair[1].left_fork, &pair[1].left_merge]
                    .into_iter()
                    .flat_map(LineIds::iter)
                    .collect();
                if right != left {
                    eprintln!(
                        "{} columns {column}/{}: boundary IDs {right:?} != {left:?}",
                        row.node,
                        column + 1
                    );
                    mismatches += 1;
                }
            }
        }
    }
    mismatches
}

fn draw_panel(
    svg: &mut Svg,
    graph: &Graph,
    origin: (f64, f64),
    title: &str,
    rows: &[GraphRowShape<String>],
) -> f64 {
    let (x, start) = origin;
    svg.text(x, start + 10.0, title, 13.0);
    let mut y = start + 28.0;
    let columns = rows.iter().map(|row| row.node_line.len()).max().unwrap();
    let cx = |column: usize| x + column as f64 * COLUMN + COLUMN / 2.0;
    for row in rows {
        let ids = row.line_ids.as_deref().unwrap();
        let nc = row.node_column;
        let (top, mid, bottom) = (y, y + NODE_HEIGHT / 2.0, y + NODE_HEIGHT);
        for (column, line) in row.node_line.iter().enumerate() {
            match line {
                NodeLine::Parent => {
                    svg.vertical(cx(column), top, bottom, &ids.before_link[column], false)
                }
                NodeLine::Ancestor => {
                    svg.vertical(cx(column), top, bottom, &ids.before_link[column], true)
                }
                NodeLine::Node if !row.is_head => {
                    svg.vertical(cx(column), top, mid, &ids.before_link[column], false)
                }
                _ => {}
            }
        }
        if !row.is_root {
            let outgoing = if let Some(cells) = &ids.link_line {
                let cell = &cells[nc];
                [&cell.vertical, &cell.left_merge, &cell.right_merge]
                    .into_iter()
                    .flat_map(LineIds::iter)
                    .collect()
            } else {
                ids.after_link[nc].clone()
            };
            svg.vertical(cx(nc), mid, bottom, &outgoing, false);
        }
        let label = if graph.public.contains(&row.node.as_str()) {
            format!("{} (public)", row.node)
        } else {
            row.node.clone()
        };
        svg.text(x + columns as f64 * COLUMN + 6.0, mid, &label, 12.0);
        let node_y = mid;
        y = bottom;

        if let Some(flags) = &row.link_line {
            let (top, mid, bottom) = (y, y + LINK_HEIGHT / 2.0, y + LINK_HEIGHT);
            for (column, (flags, cell)) in flags
                .iter()
                .zip(ids.link_line.as_ref().unwrap())
                .enumerate()
            {
                let (left, center, right) = (
                    cx(column) - COLUMN / 2.0,
                    cx(column),
                    cx(column) + COLUMN / 2.0,
                );
                if flags.intersects(LinkLine::VERTICAL) {
                    let ancestor = flags.intersects(LinkLine::VERT_ANCESTOR);
                    if flags.intersects(LinkLine::HORIZONTAL)
                        && !flags.intersects(LinkLine::ANY_FORK_OR_MERGE | LinkLine::CHILD)
                    {
                        svg.vertical(center, top, mid - INTERSECT_GAP, &cell.vertical, ancestor);
                        svg.vertical(
                            center,
                            mid + INTERSECT_GAP,
                            bottom,
                            &cell.vertical,
                            ancestor,
                        );
                    } else {
                        svg.vertical(center, top, bottom, &cell.vertical, ancestor);
                    }
                }
                if flags.intersects(LinkLine::HORIZONTAL) {
                    svg.horizontal(
                        mid,
                        left,
                        right,
                        &cell.horizontal,
                        flags.intersects(LinkLine::HORIZ_ANCESTOR),
                    );
                }
                for (flag, ancestor, endpoint, target, line_ids) in [
                    (
                        LinkLine::LEFT_FORK,
                        LinkLine::LEFT_FORK_ANCESTOR,
                        (left, mid),
                        (center, bottom),
                        &cell.left_fork,
                    ),
                    (
                        LinkLine::RIGHT_FORK,
                        LinkLine::RIGHT_FORK_ANCESTOR,
                        (right, mid),
                        (center, bottom),
                        &cell.right_fork,
                    ),
                    (
                        LinkLine::LEFT_MERGE,
                        LinkLine::LEFT_MERGE_ANCESTOR,
                        (left, mid),
                        (center, top),
                        &cell.left_merge,
                    ),
                    (
                        LinkLine::RIGHT_MERGE,
                        LinkLine::RIGHT_MERGE_ANCESTOR,
                        (right, mid),
                        (center, top),
                        &cell.right_merge,
                    ),
                ] {
                    if flags.intersects(flag) {
                        svg.corner(endpoint, target, line_ids, flags.intersects(ancestor));
                    }
                }
            }
            y = bottom;
        }
        if let Some(terms) = &row.term_line {
            let (top, mid, bottom) = (y, y + TERM_HEIGHT / 2.0, y + TERM_HEIGHT);
            for (column, term) in terms.iter().enumerate() {
                let line_ids = &ids.after_link[column];
                if *term {
                    svg.vertical(cx(column), top, mid, line_ids, false);
                    svg.horizontal(mid, cx(column) - 5.0, cx(column) + 5.0, line_ids, false);
                } else {
                    match row.ancestry_line[column] {
                        PadLine::Parent => svg.vertical(cx(column), top, bottom, line_ids, false),
                        PadLine::Ancestor => svg.vertical(cx(column), top, bottom, line_ids, true),
                        PadLine::Blank => {}
                    }
                }
            }
            y = bottom;
        }
        for (column, line) in row.ancestry_line.iter().enumerate() {
            match line {
                PadLine::Parent => svg.vertical(
                    cx(column),
                    y,
                    y + ANCESTRY_HEIGHT,
                    &ids.after_link[column],
                    false,
                ),
                PadLine::Ancestor => svg.vertical(
                    cx(column),
                    y,
                    y + ANCESTRY_HEIGHT,
                    &ids.after_link[column],
                    true,
                ),
                PadLine::Blank => {}
            }
        }
        y += ANCESTRY_HEIGHT;
        writeln!(svg.body, r##"<circle cx="{}" cy="{node_y}" r="4.5" fill="#e6edf3" stroke="#1e1e1e" stroke-width="1.5"/>"##, cx(nc)).unwrap();
    }
    y - start
}

fn print_svg(options: &Options) {
    let mut svg = Svg::default();
    let mut width: f64 = 0.0;
    let mut y = 16.0;
    for graph in &options.graphs {
        let nodes = graph_nodes(graph);
        let panels: Vec<_> = options
            .specs
            .iter()
            .map(|spec| {
                let title = if spec.label == spec.rules {
                    spec.rules.clone()
                } else {
                    format!("{}: {}", spec.label, spec.rules)
                };
                let allocator =
                    build_line_id_allocator_from_rules(&spec.rules, options.id_count).unwrap();
                (title, shapes(graph, &nodes, allocator))
            })
            .collect();
        let columns = panels
            .iter()
            .flat_map(|(_, rows)| rows)
            .map(|row| row.node_line.len())
            .max()
            .unwrap();
        let label_width = panels[0]
            .1
            .iter()
            .map(|row| {
                row.node.len()
                    + if graph.public.contains(&row.node.as_str()) {
                        9
                    } else {
                        0
                    }
            })
            .max()
            .unwrap() as f64
            * 7.2;
        let panel_width = (columns as f64 * COLUMN + label_width + 30.0).max(210.0);
        svg.text(16.0, y + 10.0, graph.name, 15.0);
        y += 28.0;
        let (missing_before, segments_before, ancestors_before) = (
            svg.missing_ids,
            svg.multi_id_segments,
            svg.multi_id_ancestors,
        );
        let mut height: f64 = 0.0;
        let mut mismatches = 0;
        for (column, (title, rows)) in panels.iter().enumerate() {
            mismatches += audit(rows);
            height = height.max(draw_panel(
                &mut svg,
                graph,
                (16.0 + column as f64 * panel_width, y),
                title,
                rows,
            ));
        }
        y += height + 24.0;
        width = width.max(32.0 + panels.len() as f64 * panel_width);
        // Diagnostics go to stderr so stdout stays a valid SVG document.
        eprintln!(
            "{}: missing IDs={}, boundary mismatches={}, multi-ID segments={}, multi-ID ancestors={}",
            graph.name,
            svg.missing_ids - missing_before,
            mismatches,
            svg.multi_id_segments - segments_before,
            svg.multi_id_ancestors - ancestors_before,
        );
    }
    let height = y;
    print!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">
<rect width="100%" height="100%" fill="#1e1e1e"/>
{}</svg>
"##,
        svg.body
    );
}

fn render_terminal(
    graph: &Graph,
    nodes: &[(String, Vec<Ancestor<String>>)],
    allocator: Option<Arc<LineIdAllocator<String>>>,
    digits: bool,
) -> String {
    const ANSI: [&str; 6] = [
        "\x1b[31m", "\x1b[32m", "\x1b[33m", "\x1b[34m", "\x1b[35m", "\x1b[36m",
    ];
    let mut renderer = GraphTextRenderer::<String>::default()
        .with_line_id_allocator(allocator)
        .configure(|o| o.min_row_height = 0);
    for node in graph.reserve {
        renderer.reserve(node.to_string());
    }
    let mut out = String::new();
    for (node, parents) in nodes {
        let message = if graph.public.contains(&node.as_str()) {
            format!("{node} (public)")
        } else {
            node.clone()
        };
        renderer.write_next_lines(
            |text, color| {
                if let Some(id) = *color {
                    if digits {
                        let digit = char::from_digit((id % 36) as u32, 36).unwrap();
                        out.extend(text.chars().map(|ch| if ch == ' ' { ' ' } else { digit }));
                    } else {
                        out.push_str(ANSI[id % ANSI.len()]);
                        out.push_str(text);
                        out.push_str("\x1b[39m");
                    }
                } else {
                    out.push_str(text);
                }
            },
            node.clone(),
            copy_parents(parents),
            "o",
            &message,
        );
    }
    out
}

fn visible_width(text: &str) -> usize {
    let mut chars = text.chars();
    let mut width = 0;
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // The terminal renderer emits only SGR escape sequences.
            for ch in chars.by_ref() {
                if ch == 'm' {
                    break;
                }
            }
        } else {
            width += 1;
        }
    }
    width
}

fn print_terminal(options: &Options) {
    for graph in &options.graphs {
        let nodes = graph_nodes(graph);
        let mut columns = vec![(
            "plain".to_owned(),
            render_terminal(graph, &nodes, None, options.digits),
        )];
        columns.extend(options.specs.iter().map(|spec| {
            let allocator = build_line_id_allocator_from_rules(&spec.rules, options.id_count);
            (
                spec.label.clone(),
                render_terminal(graph, &nodes, allocator, options.digits),
            )
        }));
        let width = columns
            .iter()
            .flat_map(|(label, text)| {
                text.lines()
                    .map(visible_width)
                    .chain([visible_width(label)])
            })
            .max()
            .unwrap()
            + 6;
        println!("########## {}\n", graph.name);
        let header: String = columns
            .iter()
            .map(|(label, _)| format!("  {label:<w$}", w = width - 2))
            .collect();
        println!("{}", header.trim_end());
        let columns: Vec<Vec<_>> = columns
            .iter()
            .map(|(_, text)| text.lines().collect())
            .collect();
        for row in 0..columns[0].len() {
            let mut line = String::new();
            for column in &columns {
                assert_eq!(column.len(), columns[0].len());
                let text = column[row];
                line.push_str("  ");
                line.push_str(text);
                line.push_str(&" ".repeat(width - 2 - visible_width(text)));
            }
            println!("{}", line.trim_end());
        }
        println!();
    }
    for spec in &options.specs {
        println!("  {} = {}", spec.label, spec.rules);
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let Some(options) = cli::parse()? else {
        return Ok(());
    };
    match options.format {
        Format::Svg => print_svg(&options),
        Format::Ansi => print_terminal(&options),
    }
    Ok(())
}
