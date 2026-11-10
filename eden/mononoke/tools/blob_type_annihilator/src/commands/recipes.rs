/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::io::Write;

use anyhow::Result;
use clap::Parser;
use mononoke_app::MononokeApp;

use crate::policy::POLICY_VERSION;
use crate::policy::RetiredBlobType;

/// List the compiled retired blob families and their key recipes
#[derive(Parser)]
pub struct CommandArgs {}

pub async fn run(_app: MononokeApp, _args: CommandArgs) -> Result<()> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "Policy {POLICY_VERSION}")?;
    for blob_type in RetiredBlobType::all() {
        writeln!(
            out,
            "{blob_type}: mapping type {} versions {:?}",
            blob_type.derivable_type().name(),
            blob_type.mapping_versions()
        )?;
        for recipe in blob_type.recipes() {
            writeln!(
                out,
                "  {} repoNNNN.{}<64 hex> ({:?})",
                recipe.id, recipe.stem, recipe.status
            )?;
        }
        writeln!(
            out,
            "  audited families: {}",
            blob_type.audit_families().join(" ")
        )?;
        if !blob_type.dependents().is_empty() {
            let dependents: Vec<String> = blob_type
                .dependents()
                .iter()
                .map(ToString::to_string)
                .collect();
            writeln!(out, "  must be retired with: {}", dependents.join(" "))?;
        }
    }
    Ok(())
}
