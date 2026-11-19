/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! `mononoke_admin config show`: a repo's resolved config, as the services of
//! this tier load it.

use std::collections::HashSet;

use anyhow::Result;
use clap::Parser;
use metaconfig_parser::layering::ConfigPath;
use metaconfig_parser::layering::Provenance;
use metaconfig_parser::layering::union_paths;
use mononoke_app::MononokeApp;
use serde_json::Value;

use super::leaves;
use super::load_repo_spec;
use super::strip_nulls;

#[derive(Parser)]
pub struct ShowArgs {
    /// The repo whose config to show.
    #[clap(long)]
    repo: String,

    /// Print the config as one JSON object instead of one line per value.
    #[clap(long, conflicts_with = "provenance")]
    json: bool,

    /// After each value, name the layer that set it: the spec, a tier
    /// override, a named preset, or Mononoke's default; the storage config's
    /// values name the storage config. Unset fields inside a struct that is
    /// set are left out, and also take Mononoke's defaults.
    #[clap(long)]
    provenance: bool,
}

pub async fn run(app: MononokeApp, args: ShowArgs) -> Result<()> {
    let loaded = load_repo_spec(&app, &args.repo)?;
    let config = metaconfig_parser::resolve_repo_config_with_provenance(
        &loaded.spec,
        &loaded.tier,
        loaded.manifest.presets.as_ref(),
    )?;
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

    match app.configs().service() {
        Some(service) => eprintln!(
            "Resolved config of {} as {service} sees it (legacy tier {})",
            args.repo, loaded.tier
        ),
        None => eprintln!("Resolved config of {} on tier {}", args.repo, loaded.tier),
    }
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
    let provenance = args.provenance.then_some(&config.provenance);
    print_lines(&identity, repo_config, &unions, provenance);
    match &storage {
        Ok(storage) => {
            let unions = union_paths(storage)?;
            let mut storage = serde_json::to_value(storage)?;
            strip_nulls(&mut storage);
            let storage_name = args
                .provenance
                .then_some(config.value.storage_config.as_deref())
                .flatten();
            print_storage_lines(&storage, &unions, storage_name);
        }
        Err(e) => println!("# storage: {e:#}"),
    }
    storage.map(|_| ())
}

/// Prints one line per value, `path = value`, with paths from the top of the
/// spec: its own fields, then the repo config's values under `repo_config`,
/// then the top-level repo config fields that are not set. A thrift union in
/// the repo config (its path is in `unions`) has a line naming the variant it
/// holds, `path = variant`, before the variant's own values. With
/// `provenance`, each line ends with the layer that set the value, or for a
/// union the layer that chose its variant; the spec's own fields come from the
/// spec.
fn print_lines(
    identity: &Value,
    mut repo_config: Value,
    unions: &HashSet<ConfigPath>,
    provenance: Option<&Provenance>,
) {
    for (path, value) in leaves(identity, ConfigPath::new(), &HashSet::new()) {
        match provenance {
            Some(_) => println!("{path} = {value}  # spec"),
            None => println!("{path} = {value}"),
        }
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
        let shown = repo_config_path.clone().join(&path);
        match provenance {
            Some(provenance) => println!("{shown} = {value}  # {}", provenance.source(&path)),
            None => println!("{shown} = {value}"),
        }
    }
    if !unset.is_empty() {
        println!(
            "# top-level fields not set, so Mononoke's defaults apply: {}",
            unset.join(", ")
        );
    }
}

/// Prints the values of the storage config the repo names, under `storage`,
/// one line per value as `print_lines` does. With the storage config's `name`,
/// each line ends with it.
fn print_storage_lines(storage: &Value, unions: &HashSet<ConfigPath>, name: Option<&str>) {
    let storage_path = ConfigPath::new().key("storage");
    for (path, value) in leaves(storage, ConfigPath::new(), unions) {
        let shown = storage_path.clone().join(&path);
        match name {
            Some(name) => println!("{shown} = {value}  # storage config {name}"),
            None => println!("{shown} = {value}"),
        }
    }
}
