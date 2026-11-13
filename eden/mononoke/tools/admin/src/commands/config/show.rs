/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! `mononoke_admin config show`: a repo's resolved config, as the services of
//! this tier load it.

use std::collections::HashSet;
use std::fmt;

use anyhow::Result;
use clap::Parser;
use metaconfig_parser::layering::ConfigPath;
use metaconfig_parser::layering::union_paths;
use mononoke_app::MononokeApp;
use serde_json::Value;

use super::load_repo_spec;
use super::strip_nulls;

#[derive(Parser)]
pub struct ShowArgs {
    /// The repo whose config to show.
    #[clap(long)]
    repo: String,

    /// Print the config as one JSON object instead of one line per value.
    #[clap(long)]
    json: bool,
}

pub async fn run(app: MononokeApp, args: ShowArgs) -> Result<()> {
    let loaded = load_repo_spec(&app, &args.repo)?;
    let config =
        metaconfig_parser::resolve_repo_config_with_provenance(&loaded.spec, &loaded.tier)?;
    // A config whose storage config is not defined is still shown, so the
    // name is there to see; the command then fails.
    let storage = metaconfig_parser::named_storage_config(&config.value, &loaded.manifest.storage);

    let mut identity = serde_json::to_value(&loaded.spec)?;
    if let Value::Object(fields) = &mut identity {
        fields.remove("tier_overrides");
        fields.remove("repo_config");
    }
    strip_nulls(&mut identity);
    let mut repo_config = serde_json::to_value(&config.value)?;
    let unions = union_paths(&config.value)?;

    eprintln!("Resolved config of {} on tier {}", args.repo, loaded.tier);
    if args.json {
        strip_nulls(&mut repo_config);
        if let Value::Object(fields) = &mut identity {
            fields.insert("repo_config".to_owned(), repo_config);
            if let Ok(storage) = &storage {
                let mut storage = serde_json::to_value(storage)?;
                strip_nulls(&mut storage);
                fields.insert("storage".to_owned(), storage);
            }
        }
        println!("{}", serde_json::to_string_pretty(&identity)?);
        return storage.map(|_| ());
    }
    print_lines(&identity, repo_config, &unions);
    match &storage {
        Ok(storage) => {
            let unions = union_paths(storage)?;
            let mut storage = serde_json::to_value(storage)?;
            strip_nulls(&mut storage);
            print_storage_lines(&storage, &unions);
        }
        Err(e) => println!("# storage: {e:#}"),
    }
    storage.map(|_| ())
}

/// Prints one line per value, `path = value`, with paths from the top of the
/// spec: its own fields, then the repo config's values under `repo_config`,
/// then the top-level repo config fields that are not set. A thrift union in
/// the repo config (its path is in `unions`) has a line naming the variant it
/// holds, `path = variant`, before the variant's own values.
fn print_lines(identity: &Value, mut repo_config: Value, unions: &HashSet<ConfigPath>) {
    for (path, value) in leaves(identity, ConfigPath::new(), &HashSet::new()) {
        println!("{path} = {value}");
    }

    let unset: Vec<String> = match &repo_config {
        Value::Object(fields) => fields
            .iter()
            .filter(|(_, value)| value.is_null())
            .map(|(field, _)| field.clone())
            .collect(),
        _ => Vec::new(),
    };
    strip_nulls(&mut repo_config);
    let repo_config_path = ConfigPath::new().key("repo_config");
    for (path, value) in leaves(&repo_config, ConfigPath::new(), unions) {
        println!("{} = {value}", repo_config_path.clone().join(&path));
    }
    if !unset.is_empty() {
        println!(
            "# top-level fields not set, so Mononoke's defaults apply: {}",
            unset.join(", ")
        );
    }
}

/// Prints the values of the storage config the repo names, under `storage`,
/// one line per value as `print_lines` does.
fn print_storage_lines(storage: &Value, unions: &HashSet<ConfigPath>) {
    let storage_path = ConfigPath::new().key("storage");
    for (path, value) in leaves(storage, ConfigPath::new(), unions) {
        println!("{} = {value}", storage_path.clone().join(&path));
    }
}

/// A line's value: a JSON value, or the name of the variant a thrift union
/// holds, which no JSON value looks like.
#[derive(Debug, PartialEq)]
enum Leaf<'a> {
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
