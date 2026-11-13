/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;

use unicode_width::UnicodeWidthStr;

use super::render::Ancestor;
use super::render::Renderer;
use super::test_fixtures::TestFixture;

pub(crate) fn render_string(
    fixture: &TestFixture,
    renderer: &mut dyn Renderer<String, Output = String>,
) -> String {
    render_string_with_order(fixture, renderer, None)
}

#[derive(Default)]
pub(crate) struct MiniDag {
    parents: BTreeMap<String, BTreeSet<String>>,
    name_to_rev: BTreeMap<String, usize>,
    rev_to_name: Vec<String>,
}

impl MiniDag {
    pub(crate) fn from_drawdag(ascii: &str) -> Self {
        Self {
            parents: drawdag::parse(ascii),
            ..Default::default()
        }
    }

    pub(crate) fn assign_rev(&mut self, name: &str) -> usize {
        if let Some(&rev) = self.name_to_rev.get(name) {
            rev
        } else {
            if let Some(parents) = self.parents.get(name).cloned() {
                for p in parents {
                    self.assign_rev(p.as_str());
                }
            }
            let rev = self.rev_to_name.len();
            self.rev_to_name.push(name.to_owned());
            self.name_to_rev.insert(name.to_owned(), rev);
            rev
        }
    }

    /// All names, in DESC order.
    pub(crate) fn all(&self) -> Vec<String> {
        let mut all = self.rev_to_name.clone();
        all.reverse();
        all
    }

    pub(crate) fn parent_names(&self, name: &str) -> Vec<String> {
        match self.parents.get(name) {
            Some(names) => names.iter().cloned().collect(),
            None => Default::default(),
        }
    }
}

pub(crate) fn render_string_with_order(
    fixture: &TestFixture,
    renderer: &mut dyn Renderer<String, Output = String>,
    order: Option<&[&str]>,
) -> String {
    let TestFixture {
        dag: ascii,
        messages,
        heads,
        reserve,
        ancestors,
        missing,
    } = fixture;

    let mut dag = MiniDag::from_drawdag(ascii);
    for head in heads.iter() {
        dag.assign_rev(head);
    }

    let ancestors: HashSet<(&str, &str)> = ancestors.iter().copied().collect();
    let missing: HashSet<&str> = missing.iter().copied().collect();

    reserve.iter().for_each(|s| renderer.reserve(s.to_string()));

    let messages: HashMap<_, _> = messages.iter().cloned().collect();

    let iter: Vec<String> = match order {
        None => dag.all(),
        Some(order) => order.iter().map(|name| name.to_string()).collect(),
    };

    let mut out = String::new();
    for node in iter {
        if missing.contains(node.as_str()) {
            continue;
        }
        let parents = dag
            .parent_names(&node)
            .into_iter()
            .map(|parent| {
                if missing.contains(parent.as_str()) {
                    Ancestor::Anonymous
                } else if ancestors.contains(&(node.as_str(), parent.as_str())) {
                    Ancestor::Ancestor(parent)
                } else {
                    Ancestor::Parent(parent)
                }
            })
            .collect();
        let name = &node;
        let message = match messages.get(name.as_str()) {
            Some(message) => format!("{name}\n{message}"),
            None => name.clone(),
        };
        let width = renderer.width(Some(&node), Some(&parents));
        let row = renderer.next_row(node.to_string(), parents, String::from("o"), message);
        let row_indent = row
            .lines()
            .filter_map(|line| line.find(name.as_str()).map(|offset| &line[..offset]))
            .next()
            .expect("name should be in the output");
        assert_eq!(
            row_indent.width() as u64,
            width,
            "indent '{row_indent}' for row for {name} is the wrong width"
        );

        out.push_str(&row);
    }

    indent(&out)
}

pub(crate) fn indent(out: &str) -> String {
    format!(
        "\n{}",
        out.trim_end()
            .lines()
            .map(|l| format!("            {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}
