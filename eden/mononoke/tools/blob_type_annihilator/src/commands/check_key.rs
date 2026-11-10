/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeSet;
use std::io::Write;

use anyhow::Result;
use clap::Parser;
use mononoke_app::MononokeApp;

use crate::policy::RetiredBlobType;
use crate::policy::parse_physical_key;

/// Show which recipe, if any, selects each given physical key
#[derive(Parser)]
pub struct CommandArgs {
    /// Retired types to select; all of them if omitted
    #[clap(long = "retired-type", value_enum)]
    retired_types: Vec<RetiredBlobType>,

    /// Physical blobstore keys to check
    #[clap(required = true)]
    keys: Vec<String>,
}

pub async fn run(_app: MononokeApp, args: CommandArgs) -> Result<()> {
    // A plain set: checking keys of one type needs no dependents.
    let selected: BTreeSet<RetiredBlobType> = if args.retired_types.is_empty() {
        RetiredBlobType::all().collect()
    } else {
        args.retired_types.into_iter().collect()
    };
    let mut out = std::io::stdout().lock();
    for key in &args.keys {
        match parse_physical_key(key) {
            Some((parsed, encoding)) if selected.contains(&parsed.recipe.blob_type) => writeln!(
                out,
                "selected {key}: repo {} recipe {} hash {} ({encoding:?})",
                parsed.repo_id, parsed.recipe.id, parsed.hash
            )?,
            Some((parsed, _)) => writeln!(
                out,
                "not selected {key}: recipe {} of an unselected type",
                parsed.recipe.id
            )?,
            None => writeln!(out, "not selected {key}: matches no recipe")?,
        }
    }
    Ok(())
}
