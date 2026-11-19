/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! `mononoke_admin config` — config-layer subcommands.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use metaconfig_parser::ServiceManifest;
use metaconfig_parser::configerator_repo_spec_handle;
use metaconfig_parser::layering::ConfigPath;
use mononoke_app::MononokeApp;
use repos::RepoSpec;
use serde_json::Value;

mod change_impact;
mod check;
mod compare;
mod deployment_specs;
mod minimise;
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
    /// Check that each service of this deployment sees what a job running
    /// with the service's legacy tier sees: the same repos, specs, common
    /// config and storage configs.
    Compare(compare::CompareArgs),
    /// List the repos whose resolved config a config change would change,
    /// such as an edit to a preset: the loaded deployment against a candidate
    /// version of it, or one field of a preset.
    ChangeImpact(change_impact::ChangeImpactArgs),
    /// Rewrite repo specs to name a preset and keep only what
    /// differs from it, checking that every service of the deployment that
    /// serves each repo still loads the same config. A job that reads a tier
    /// can't resolve a preset, so apply the result only once every job that
    /// reads the specs loads them through a deployment.
    Minimise(minimise::MinimiseArgs),
}

pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()> {
    match args.subcommand {
        ConfigSubcommand::Check(check_args) => check::run(app, check_args).await,
        ConfigSubcommand::Show(show_args) => show::run(app, show_args).await,
        ConfigSubcommand::Compare(compare_args) => compare::run(app, compare_args).await,
        ConfigSubcommand::ChangeImpact(change_impact_args) => {
            change_impact::run(app, change_impact_args).await
        }
        ConfigSubcommand::Minimise(args) => minimise::run(app, args).await,
    }
}

/// A repo's spec and the manifest it was loaded through.
struct LoadedSpec {
    tier: String,
    manifest: Arc<ServiceManifest>,
    spec: RepoSpec,
}

/// Loads a repo's spec through this job's manifest: the tier's, or its
/// service's view of the deployment.
fn load_repo_spec(app: &MononokeApp, repo_name: &str) -> Result<LoadedSpec> {
    let configs = app.configs();
    let (Some(tier), Some(manifest), Some(config_store)) = (
        configs.tier_name(),
        configs.manifest(),
        configs.repo_config_store(),
    ) else {
        anyhow::bail!(
            "config commands need a tier or a deployment, such as --config-tier scs or \
             --deployment prod"
        );
    };
    let entry = manifest
        .repos
        .iter()
        .find(|entry| entry.repo_name == repo_name)
        .with_context(|| match configs.service() {
            Some(service) => format!("repo {repo_name} is not served by {service}"),
            None => format!("repo {repo_name} is not in the {tier} tier manifest"),
        })?;
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

/// A line's value: a JSON value, or the name of the variant a thrift union
/// holds, which no JSON value looks like.
#[derive(Debug, PartialEq)]
pub(crate) enum Leaf<'a> {
    Value(&'a Value),
    Variant(&'a str),
}

impl fmt::Display for Leaf<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Leaf::Value(value) => write!(f, "{value}"),
            Leaf::Variant(variant) => f.write_str(variant),
        }
    }
}

/// The scalar values, and empty lists and objects, inside `value`, by path. A
/// thrift union (its path is in `unions`) gives the variant it holds, then the
/// variant's own values; an empty variant gives only its name.
fn leaves<'a>(
    value: &'a Value,
    path: ConfigPath,
    unions: &HashSet<ConfigPath>,
) -> Vec<(ConfigPath, Leaf<'a>)> {
    let variant = value
        .as_object()
        .filter(|fields| fields.len() == 1 && unions.contains(&path))
        .and_then(|fields| fields.iter().next());
    if let Some((variant, held)) = variant {
        let mut lines = vec![(path.clone(), Leaf::Variant(variant))];
        if held.as_object().is_none_or(|fields| !fields.is_empty()) {
            lines.extend(leaves(held, path.key(variant.as_str()), unions));
        }
        return lines;
    }
    match value {
        Value::Object(fields) if !fields.is_empty() => fields
            .iter()
            .flat_map(|(field, value)| leaves(value, path.clone().key(field.as_str()), unions))
            .collect(),
        Value::Array(items) if !items.is_empty() => items
            .iter()
            .enumerate()
            .flat_map(|(index, value)| leaves(value, path.clone().index(index), unions))
            .collect(),
        _ => vec![(path, Leaf::Value(value))],
    }
}

#[cfg(test)]
mod tests {
    use metaconfig_parser::layering::union_paths;
    use mononoke_macros::mononoke;
    use repos::RawRepoConfig;
    use serde_json::json;

    use super::*;

    #[mononoke::test]
    fn test_leaves_name_union_variants() {
        let config: RawRepoConfig = serde_json::from_value(json!({
            "zelos_config": {"zelos_tier": "tier"},
            "metadata_cache_config": {"wbc_update_mode": {"polling": {}}},
            "pushrebase": {"remote_mode": {"remote_land_service": {"host_port": "host:1"}}},
            "bookmarks": [],
        }))
        .unwrap();
        let mut value = serde_json::to_value(&config).unwrap();
        strip_nulls(&mut value);
        let lines: Vec<String> = leaves(&value, ConfigPath::new(), &union_paths(&config).unwrap())
            .into_iter()
            .map(|(path, value)| format!("{path} = {value}"))
            .collect();
        assert_eq!(
            lines,
            [
                "bookmarks = []",
                "metadata_cache_config.wbc_update_mode = polling",
                "pushrebase.remote_mode = remote_land_service",
                "pushrebase.remote_mode.remote_land_service = host_port",
                "pushrebase.remote_mode.remote_land_service.host_port = \"host:1\"",
                "zelos_config = zelos_tier",
                "zelos_config.zelos_tier = \"tier\"",
            ]
        );
    }
}
