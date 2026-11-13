/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! `mononoke_admin config` — config-layer subcommands.

use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use metaconfig_parser::configerator_repo_spec_handle;
use mononoke_app::MononokeApp;
use repos::RepoSpec;
use repos::TierManifest;
use serde_json::Value;

mod check;
mod show;

/// Mononoke config operations.
#[derive(Parser)]
pub struct CommandArgs {
    #[clap(subcommand)]
    subcommand: ConfigSubcommand,
}

#[derive(Subcommand)]
pub enum ConfigSubcommand {
    /// Validate per-repo `.cconf` config changes by loading each affected
    /// repo through the same `MononokeApp` + `RepoFactory` code path the
    /// Mononoke server uses at startup. Intended to run pre-`jf submit`
    /// on a config diff.
    Check(check::CheckArgs),
    /// Show a repo's resolved config as this tier's services load it: its
    /// spec with the tier override applied.
    Show(show::ShowArgs),
}

pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()> {
    match args.subcommand {
        ConfigSubcommand::Check(check_args) => check::run(app, check_args).await,
        ConfigSubcommand::Show(show_args) => show::run(app, show_args).await,
    }
}

/// A repo's spec and the tier it was loaded for.
struct LoadedSpec {
    tier: String,
    manifest: Arc<TierManifest>,
    spec: RepoSpec,
}

/// Loads a repo's spec through this tier's manifest.
fn load_repo_spec(app: &MononokeApp, repo_name: &str) -> Result<LoadedSpec> {
    let configs = app.configs();
    let (Some(tier), Some(manifest), Some(config_store)) = (
        configs.tier_name(),
        configs.manifest(),
        configs.config_store(),
    ) else {
        anyhow::bail!("config commands need a tier, such as --config-tier scs");
    };
    let entry = manifest
        .repos
        .iter()
        .find(|entry| entry.repo_name == repo_name)
        .with_context(|| format!("repo {repo_name} is not in the {tier} tier manifest"))?;
    let spec = configerator_repo_spec_handle(&entry.config_path, config_store)
        .with_context(|| format!("loading the spec of {repo_name} from {}", entry.config_path))?
        .get();
    Ok(LoadedSpec {
        tier: tier.to_owned(),
        spec: (*spec).clone(),
        manifest,
    })
}

/// Removes unset (null) fields, so that output shows only what is set.
fn strip_nulls(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|_, field| !field.is_null());
            fields.values_mut().for_each(strip_nulls);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_nulls),
        _ => {}
    }
}
