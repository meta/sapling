/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::error::Error;

use renderdag::pipeline::coloring::COLOR_RULE_PRESETS;
use renderdag::pipeline::coloring::build_line_id_allocator_from_rules;

use crate::graphs::GRAPHS;
use crate::graphs::Graph;

#[derive(Clone, Copy)]
pub enum Format {
    Svg,
    Ansi,
}

pub struct Spec {
    pub label: String,
    pub rules: String,
}

pub struct Options {
    pub format: Format,
    pub id_count: usize,
    pub digits: bool,
    pub graphs: Vec<&'static Graph>,
    pub specs: Vec<Spec>,
}

fn parse_spec(text: &str, id_count: usize) -> Result<Spec, Box<dyn Error>> {
    let (label, rules) = if let Some((label, rules)) = text.split_once('=') {
        if label.is_empty() {
            return Err(format!("invalid SPEC {text:?}: empty label").into());
        }
        (label.to_owned(), rules.to_owned())
    } else if let Ok(index) = text.parse::<usize>() {
        let rules = COLOR_RULE_PRESETS
            .get(index)
            .ok_or_else(|| format!("invalid SPEC {text:?}: unknown preset index"))?;
        (text.to_owned(), (*rules).to_owned())
    } else {
        (text.to_owned(), text.to_owned())
    };
    if build_line_id_allocator_from_rules::<String>(&rules, id_count).is_none() {
        return Err(format!("invalid SPEC {text:?}: invalid allocation rules {rules:?}").into());
    }
    Ok(Spec { label, rules })
}

fn help() {
    println!(
        "Usage: renderdag-graph-coloring [OPTIONS] [SPEC ...]\n\
             --format ansi|svg  Output format (default ansi); SVG is written to stdout\n\
             -n, --ids N        Positive ID count (default: palette size)\n\
             -g, --graph NAME   Select a graph; repeat to select several\n\
             --digits          Show ID characters instead of ANSI colors\n\
             -h, --help         Show this help\n\n\
             SPEC: preset index, LABEL=RULES, or RULES. Defaults to all presets.\n\
             RULES: comma-separated [COND:]SOURCE; single / !single conditions.\n\
             Sources: parent, incoming, free, next-free, lane, node-lane.\n\
             Full semantics: renderdag::pipeline::coloring::build_line_id_allocator_from_rules\n\nPresets:"
    );
    for (index, rules) in COLOR_RULE_PRESETS.iter().enumerate() {
        println!("  {index}: {rules}");
    }
    println!("\nGraphs:");
    for graph in GRAPHS {
        println!("  {}", graph.name);
    }
}

pub fn parse() -> Result<Option<Options>, Box<dyn Error>> {
    let mut format = Format::Ansi;
    let mut id_count = crate::COLORS.len();
    let mut digits = false;
    let mut names = Vec::new();
    let mut specs = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--format" => {
                format = match args.next().as_deref() {
                    Some("svg") => Format::Svg,
                    Some("ansi") => Format::Ansi,
                    _ => return Err("--format requires svg or ansi".into()),
                };
            }
            "-n" | "--ids" => {
                id_count = args.next().ok_or("--ids requires a number")?.parse()?;
                if id_count == 0 {
                    return Err("--ids must be greater than zero".into());
                }
            }
            "-g" | "--graph" => names.push(args.next().ok_or("--graph requires a name")?),
            "--digits" => digits = true,
            "-h" | "--help" => {
                help();
                return Ok(None);
            }
            "--" => {
                specs.extend(args);
                break;
            }
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg:?}").into()),
            _ => specs.push(arg),
        }
    }
    if matches!(format, Format::Svg) && digits {
        return Err("--digits requires --format ansi".into());
    }
    let graphs = if names.is_empty() {
        GRAPHS.iter().collect()
    } else {
        names
            .iter()
            .map(|name| {
                GRAPHS
                    .iter()
                    .find(|graph| graph.name == name)
                    .ok_or_else(|| format!("unknown graph {name:?}"))
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    if specs.is_empty() {
        specs = (0..COLOR_RULE_PRESETS.len())
            .map(|index| index.to_string())
            .collect();
    }
    let specs = specs
        .iter()
        .map(|spec| parse_spec(spec, id_count))
        .collect::<Result<_, _>>()?;
    Ok(Some(Options {
        format,
        id_count,
        digits,
        graphs,
        specs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs() {
        assert_eq!(parse_spec("1", 6).unwrap().rules, COLOR_RULE_PRESETS[1]);
        assert_eq!(parse_spec("mine=parent,free", 6).unwrap().label, "mine");
        assert_eq!(
            parse_spec("!single:lane,node-lane", 6).unwrap().rules,
            "!single:lane,node-lane"
        );
        for spec in ["9", "oops=unknown", "=free"] {
            let error = parse_spec(spec, 6).err().unwrap().to_string();
            assert!(error.contains(&format!("{spec:?}")));
        }
    }
}
