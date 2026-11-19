/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! `mononoke_admin config change-impact`: the repos whose resolved config a
//! config change would change, such as an edit to a preset.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use cached_config::ConfigStore;
use clap::Parser;
use config_args::local_config_store;
use metaconfig_parser::ConfigRoot;
use metaconfig_parser::DeploymentLocation;
use metaconfig_parser::Presets;
use metaconfig_parser::ResolvedRepoConfig;
use metaconfig_parser::ServiceView;
use metaconfig_parser::configerator_repo_spec_handle;
use metaconfig_parser::deployment_config_handle;
use metaconfig_parser::layering::ConfigPath;
use metaconfig_parser::layering::ConfigSource;
use metaconfig_parser::layering::Layered;
use metaconfig_parser::layering::union_paths;
use metaconfig_parser::resolve_repo_spec_with_provenance;
use metaconfig_parser::service_view;
use mononoke_app::MononokeApp;
use repos::MononokeService;
use repos::RawRepoConfig;
use repos::RawStorageConfig;
use repos::RepoSpec;
use repos::TierRepoEntry;
use serde_json::Value;

use super::Leaf;
use super::leaves;
use super::strip_nulls;

/// How often the candidate's store would poll for changes; it is read once.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Parser)]
pub struct ChangeImpactArgs {
    /// Compare the loaded config with the same deployment in this candidate:
    /// a configerator checkout, or JSON files laid out by config path, as for
    /// `--local-configerator-path`
    #[clap(long, value_name = "PATH", conflicts_with = "candidate_config")]
    candidate_local_configerator_path: Option<PathBuf>,

    /// Compare the loaded config with the same deployment in this candidate:
    /// a directory laid out as for `--config`
    #[clap(long, value_name = "DIR")]
    candidate_config: Option<PathBuf>,

    /// Instead of comparing with a candidate, list the repos of the loaded
    /// config that take `--field` from this preset, so that changing the field
    /// in the preset would change them.
    #[clap(
        long,
        requires = "field",
        conflicts_with_all = ["candidate_local_configerator_path", "candidate_config"]
    )]
    preset: Option<String>,

    /// The repo config field that `--preset` would change, as a path such as
    /// `pushrebase.rewritedates` or `bookmarks[0].hooks`, with
    /// `["clusterops.fbservices"]` for a key that is not a plain name
    #[clap(long, requires = "preset")]
    field: Option<ConfigPath>,

    /// Print the values that change for each changed repo.
    #[clap(long)]
    diff: bool,
}

/// One side of a comparison: one service's view of a version of the
/// deployment, and the store its specs are in.
struct ConfigVersion<'a> {
    store: &'a ConfigStore,
    view: ServiceView,
}

impl<'a> ConfigVersion<'a> {
    fn load(
        store: &'a ConfigStore,
        location: &DeploymentLocation,
        service: MononokeService,
    ) -> Result<Self> {
        let deployment = deployment_config_handle(&location.config_path(), store)
            .with_context(|| format!("loading the deployment at {location}"))?
            .get();
        let view = service_view(&deployment, service, location)?;
        Ok(Self { store, view })
    }

    fn entry(&self, repo_name: &str) -> Option<&TierRepoEntry> {
        self.view
            .manifest
            .repos
            .iter()
            .find(|entry| entry.repo_name == repo_name)
    }

    fn spec(&self, repo_name: &str) -> Result<Option<Arc<RepoSpec>>> {
        let Some(entry) = self.entry(repo_name) else {
            return Ok(None);
        };
        let spec = configerator_repo_spec_handle(&entry.config_path, self.store)
            .with_context(|| format!("loading the spec of {repo_name}"))?
            .get();
        Ok(Some(spec))
    }

    /// Everything about a repo that its config depends on: its manifest entry,
    /// its spec's own fields, its resolved config and the storage config it
    /// names, with its spec.
    fn repo(&self, repo_name: &str) -> Result<(Comparable, Arc<RepoSpec>)> {
        let (Some(entry), Some(spec)) = (self.entry(repo_name), self.spec(repo_name)?) else {
            anyhow::bail!("spec not found");
        };
        let resolved = self.resolve(&spec)?;
        let value = comparable(entry, &spec, &resolved.config, Some(&resolved.storage))?;
        Ok((value, spec))
    }

    /// The repo's config and the storage config it names, resolved for the
    /// service's legacy tier with the deployment's presets.
    fn resolve(&self, spec: &RepoSpec) -> Result<ResolvedRepoConfig<Layered<RawRepoConfig>>> {
        resolve_repo_spec_with_provenance(
            spec,
            &self.view.legacy_tier,
            &self.view.manifest.storage,
            self.view.manifest.presets.as_ref(),
        )
        .with_context(|| format!("resolving the config of {}", spec.repo_name))
    }

    fn presets(&self) -> Option<&Presets> {
        self.view.manifest.presets.as_ref()
    }

    fn repo_names(&self) -> BTreeSet<String> {
        self.view
            .manifest
            .repos
            .iter()
            .map(|entry| entry.repo_name.clone())
            .collect()
    }
}

pub async fn run(app: MononokeApp, args: ChangeImpactArgs) -> Result<()> {
    let configs = app.configs();
    let (Some((location, _)), Some(service), Some(store)) = (
        configs.deployment(),
        configs.service(),
        configs.repo_config_store(),
    ) else {
        anyhow::bail!("config change-impact needs a deployment, such as --deployment prod");
    };
    let base = ConfigVersion::load(store, &location, service)?;

    if let (Some(preset), Some(field)) = (&args.preset, &args.field) {
        return plan_preset_change(&base, service, preset, field);
    }

    let name = location.name.clone();
    let (candidate_location, candidate_store) = match (
        &args.candidate_local_configerator_path,
        &args.candidate_config,
    ) {
        (Some(path), None) => (
            DeploymentLocation {
                root: ConfigRoot::ConfigStore,
                name,
            },
            local_config_store(path, POLL_INTERVAL),
        ),
        (None, Some(dir)) => {
            let dir = std::path::absolute(dir)?;
            (
                DeploymentLocation {
                    root: ConfigRoot::Directory(dir.clone()),
                    name,
                },
                ConfigStore::file(
                    true,
                    dir,
                    ConfigRoot::DIRECTORY_EXTENSION.to_owned(),
                    POLL_INTERVAL,
                ),
            )
        }
        _ => anyhow::bail!(
            "config change-impact compares the loaded config with a candidate: give \
             --candidate-local-configerator-path or --candidate-config, or plan a change with \
             --preset and --field"
        ),
    };
    let candidate = ConfigVersion::load(&candidate_store, &candidate_location, service)?;
    compare(&base, &candidate, service, args.diff)
}

/// A repo's config in the form two versions are compared in, with the paths
/// of its thrift unions.
struct Comparable {
    value: Value,
    unions: HashSet<ConfigPath>,
}

/// A resolved config, with the manifest entry under `entry`, the spec's own
/// fields (other than its layers) under `spec` and the storage config it names
/// under `storage`, so that a change to any of them shows up.
fn comparable(
    entry: &TierRepoEntry,
    spec: &RepoSpec,
    resolved: &Layered<RawRepoConfig>,
    storage: Option<&RawStorageConfig>,
) -> Result<Comparable> {
    let mut identity = serde_json::to_value(spec)?;
    if let Value::Object(fields) = &mut identity {
        fields.remove("repo_config");
        fields.remove("tier_overrides");
        fields.remove("preset");
    }
    let mut value = serde_json::json!({
        "entry": {
            "repo_id": entry.repo_id,
            "is_deep_sharded": entry.is_deep_sharded,
            "enabled": entry.enabled.unwrap_or(true),
        },
        "spec": identity,
        "repo_config": serde_json::to_value(&resolved.value)?,
        "storage": serde_json::to_value(storage)?,
    });
    strip_nulls(&mut value);
    let under = |field: &str, paths: HashSet<ConfigPath>| {
        let field = ConfigPath::new().key(field);
        paths.into_iter().map(move |path| field.clone().join(&path))
    };
    let unions = under("repo_config", union_paths(&resolved.value)?)
        .chain(under("storage", union_paths(&storage)?))
        .collect();
    Ok(Comparable { value, unions })
}

/// The presets whose definitions differ between two versions.
fn changed_presets(base: Option<&Presets>, new: Option<&Presets>) -> Vec<String> {
    let named = |presets: Option<&Presets>| presets.cloned().unwrap_or_default();
    let (base, new) = (named(base), named(new));
    let names: BTreeSet<&String> = base.keys().chain(new.keys()).collect();
    names
        .into_iter()
        .filter(|name| base.get(*name) != new.get(*name))
        .cloned()
        .collect()
}

fn compare(
    base: &ConfigVersion<'_>,
    candidate: &ConfigVersion<'_>,
    service: MononokeService,
    show_diff: bool,
) -> Result<()> {
    let changed_presets = changed_presets(base.presets(), candidate.presets());
    let base_names = base.repo_names();
    let candidate_names = candidate.repo_names();

    let mut changed = Vec::new();
    let mut unchanged_users: BTreeMap<&String, Vec<String>> = BTreeMap::new();
    let mut failed = Vec::new();
    for name in base_names.intersection(&candidate_names) {
        let result = (|| -> Result<_> {
            let (before, _) = base.repo(name)?;
            let (after, candidate_spec) = candidate.repo(name)?;
            let users: Vec<&String> = changed_presets
                .iter()
                .filter(|preset| candidate_spec.preset.as_ref() == Some(*preset))
                .collect();
            Ok((before, after, users))
        })();
        match result {
            Ok((before, after, _)) if before.value != after.value => {
                changed.push((name.clone(), before, after));
            }
            Ok((_, _, users)) => {
                for preset in users {
                    unchanged_users
                        .entry(preset)
                        .or_default()
                        .push(name.clone());
                }
            }
            Err(err) => failed.push((name.clone(), format!("{err:#}"))),
        }
    }

    let added: Vec<&String> = candidate_names.difference(&base_names).collect();
    let removed: Vec<&String> = base_names.difference(&candidate_names).collect();
    for name in &added {
        if let Err(err) = candidate.repo(name) {
            failed.push(((*name).clone(), format!("{err:#}")));
        }
    }

    println!(
        "{service}: {} of {} repos change, {} added, {} removed",
        changed.len(),
        base_names.intersection(&candidate_names).count(),
        added.len(),
        removed.len(),
    );
    if base.view.legacy_tier != candidate.view.legacy_tier {
        println!(
            "warning: the legacy tier changes from {} to {}: running jobs refuse this and keep the old config until they restart",
            base.view.legacy_tier, candidate.view.legacy_tier,
        );
    }
    let common = differing_fields(&base.view.manifest.common, &candidate.view.manifest.common)?;
    if !common.is_empty() {
        println!("Common config of every repo changes: {}", common.join(", "));
    }
    for (name, before, after) in &changed {
        println!("  changed {name}");
        if show_diff {
            print_diff(before, after);
        }
    }
    for name in &added {
        println!("  added {name}");
    }
    for name in &removed {
        println!("  removed {name}");
    }
    for preset in &changed_presets {
        let users = unchanged_users.get(preset).map_or(0, Vec::len);
        println!("Changed preset {preset}: {users} repos use it but do not change");
    }
    for (name, err) in &failed {
        println!("  failed {name}: {err}");
    }
    if !failed.is_empty() {
        anyhow::bail!("{} repos failed to resolve", failed.len());
    }
    Ok(())
}

/// The top-level fields that differ between two values of one type.
fn differing_fields<T: serde::Serialize>(base: &T, new: &T) -> Result<Vec<String>> {
    let fields = |value: &T| -> Result<BTreeMap<String, Value>> {
        let mut value = serde_json::to_value(value)?;
        strip_nulls(&mut value);
        Ok(match value {
            Value::Object(fields) => fields.into_iter().collect(),
            _ => BTreeMap::new(),
        })
    };
    let (base, new) = (fields(base)?, fields(new)?);
    let keys: BTreeSet<&String> = base.keys().chain(new.keys()).collect();
    Ok(keys
        .into_iter()
        .filter(|key| base.get(*key) != new.get(*key))
        .cloned()
        .collect())
}

/// Prints the values that differ between two resolved configs, and the
/// variants of the thrift unions that differ.
fn print_diff(before: &Comparable, after: &Comparable) {
    let before: BTreeMap<ConfigPath, Leaf<'_>> =
        leaves(&before.value, ConfigPath::new(), &before.unions)
            .into_iter()
            .collect();
    let after: BTreeMap<ConfigPath, Leaf<'_>> =
        leaves(&after.value, ConfigPath::new(), &after.unions)
            .into_iter()
            .collect();
    let paths: BTreeSet<&ConfigPath> = before.keys().chain(after.keys()).collect();
    for path in paths {
        match (before.get(path), after.get(path)) {
            (Some(old), Some(new)) if old != new => println!("    {path}: {old} -> {new}"),
            (Some(old), None) => println!("    {path}: {old} -> (unset)"),
            (None, Some(new)) => println!("    {path}: (unset) -> {new}"),
            _ => {}
        }
    }
}

/// Lists the repos that take `field` from preset `preset`.
fn plan_preset_change(
    config: &ConfigVersion<'_>,
    service: MononokeService,
    preset: &str,
    field: &ConfigPath,
) -> Result<()> {
    let mut affected = Vec::new();
    let mut partly = Vec::new();
    let mut shielded: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut failed = Vec::new();
    for name in config.repo_names() {
        let spec = match config.spec(&name) {
            Ok(Some(spec)) => spec,
            Ok(None) => continue,
            Err(err) => {
                failed.push((name, format!("{err:#}")));
                continue;
            }
        };
        if spec.preset.as_deref() != Some(preset) {
            continue;
        }
        let resolved = match config.resolve(&spec) {
            Ok(resolved) => resolved,
            Err(err) => {
                failed.push((name, format!("{err:#}")));
                continue;
            }
        };
        let provenance = &resolved.config.provenance;
        match provenance.source(field) {
            ConfigSource::Preset(_) | ConfigSource::Default => {
                let overridden_inside = provenance.iter().any(|(path, source)| {
                    path.is_below(field)
                        && matches!(source, ConfigSource::Spec | ConfigSource::TierOverride(_))
                });
                if overridden_inside {
                    partly.push(name);
                } else {
                    affected.push(name);
                }
            }
            source => shielded.entry(source.to_string()).or_default().push(name),
        }
    }

    println!(
        "{service}: changing {field} in preset {preset} changes {} repos",
        affected.len()
    );
    for name in &affected {
        println!("  changed {name}");
    }
    for name in &partly {
        println!("  partly changed {name} (it sets part of {field} itself)");
    }
    for (source, names) in &shielded {
        println!("Not changed, set by {source}: {} repos", names.len());
        for name in names {
            println!("  {name}");
        }
    }
    for (name, err) in &failed {
        println!("  failed {name}: {err}");
    }
    if !failed.is_empty() {
        anyhow::bail!("{} repos failed to resolve", failed.len());
    }
    Ok(())
}
