/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use mononoke_app::MononokeApp;
use regex::Regex;

/// List known repositories
#[derive(Parser)]
pub struct CommandArgs {
    /// Pattern to match against repo names.
    pattern: Option<String>,
}

pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()> {
    let pattern = args
        .pattern
        .as_deref()
        .map(Regex::new)
        .transpose()
        .context("Failed to parse pattern")?;

    // Union the legacy blob and the per-tier manifest so split-loaded repos
    // (e.g. AOSP after D102821672) still appear in `list-repos`.
    // BTreeMap keyed by repo_id keeps the output sorted and deduplicated;
    // legacy entries take precedence on collision (the manifest entry would
    // be redundant for repos still in the legacy blob).
    let mut by_id: BTreeMap<i32, String> = BTreeMap::new();
    for (repo_name, repo_config) in app.repo_configs().iter() {
        by_id.insert(repo_config.repoid.id(), repo_name.clone());
    }
    if let Some(manifest) = app.configs().manifest() {
        for entry in &manifest.repos {
            by_id
                .entry(entry.repo_id)
                .or_insert(entry.repo_name.clone());
        }
    }

    for (repo_id, repo_name) in by_id {
        if let Some(pattern) = &pattern {
            if !pattern.is_match(&repo_name) {
                continue;
            }
        }
        println!("{repo_id} {repo_name}");
    }

    Ok(())
}
