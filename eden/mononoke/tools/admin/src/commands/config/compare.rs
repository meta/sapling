/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! `mononoke_admin config compare`: whether each service of this deployment
//! sees what a job running with the service's legacy tier sees.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use cached_config::ConfigStore;
use clap::Parser;
use metaconfig_parser::ConfigRoot;
use metaconfig_parser::DeploymentLocation;
use metaconfig_parser::ServiceManifest;
use metaconfig_parser::configerator_manifest_handle;
use metaconfig_parser::configerator_repo_spec_handle;
use metaconfig_parser::resolve_repo_spec_with_provenance;
use metaconfig_parser::service_view;
use mononoke_app::MononokeApp;
use mononoke_configs::tier_manifest_path;
use repos::MononokeService;
use repos::RawRepoConfig;
use repos::RawStorageConfig;
use repos::RepoSpec;
use repos::TierRepoEntry;
use serde::Serialize;
use serde_json::Value;

/// At most this many repo names are listed for each kind of difference.
const MAX_LISTED: usize = 20;

#[derive(Parser)]
pub struct CompareArgs {
    /// The services to compare, such as SCS_SERVER. Default: every service of
    /// the deployment.
    services: Vec<MononokeService>,

    /// Compare the one service given against this tier's manifest rather
    /// than its legacy tier's.
    #[clap(long)]
    against_tier: Option<String>,

    /// Also resolve every repo's config both ways and compare the results:
    /// the deployment's spec with the service's legacy tier, and the tier's
    /// spec with the tier. A deployment read from a `--config` directory needs
    /// this to compare its specs.
    #[clap(long)]
    specs: bool,
}

pub async fn run(app: MononokeApp, args: CompareArgs) -> Result<()> {
    let configs = app.configs();
    let Some((location, deployment)) = configs.deployment() else {
        bail!("config compare needs a deployment, such as --deployment prod");
    };
    let tier_store = configs
        .config_store()
        .context("config compare needs the configerator store")?;
    let repo_store = configs
        .repo_config_store()
        .context("config compare needs the deployment's store")?;

    let services: Vec<MononokeService> = if args.services.is_empty() {
        deployment.services.keys().copied().collect()
    } else {
        args.services.clone()
    };
    if args.against_tier.is_some() && services.len() != 1 {
        bail!("--against-tier compares exactly one service");
    }

    let mut differing = 0;
    for service in &services {
        let view = service_view(&deployment, *service, &location)?;
        let tier = args
            .against_tier
            .clone()
            .unwrap_or_else(|| view.legacy_tier.clone());
        let manifest = ServiceManifest::from(
            (*configerator_manifest_handle(&tier_manifest_path(&tier), tier_store)
                .with_context(|| format!("Failed to read the {tier} tier manifest"))?
                .get())
            .clone(),
        );

        let mut differences = Vec::new();
        if view.legacy_tier != tier {
            differences.push(format!(
                "repo configs resolve with legacy tier {}, not {tier}",
                view.legacy_tier
            ));
        }
        differences.extend(compare_repos(
            &view.manifest.repos,
            &manifest.repos,
            &location,
            args.specs.then(|| SpecSides {
                deployment_store: repo_store,
                deployment_storage: &view.manifest.storage,
                legacy_tier: &view.legacy_tier,
                tier_store,
                tier_storage: &manifest.storage,
                tier: &tier,
            }),
        ));
        differences.extend(compare_values(
            "common config",
            &view.manifest.common,
            &manifest.common,
        )?);
        differences.extend(compare_values(
            "storage config",
            &view.manifest.storage,
            &manifest.storage,
        )?);

        if differences.is_empty() {
            println!("{service} ({tier}): same");
        } else {
            differing += 1;
            println!("{service} ({tier}): differs");
            for difference in differences {
                println!("  {difference}");
            }
        }
    }

    if differing > 0 {
        bail!(
            "{differing} of {} services differ from their tiers",
            services.len()
        );
    }
    Ok(())
}

/// How to resolve a repo's config on each side of a comparison.
struct SpecSides<'a> {
    deployment_store: &'a ConfigStore,
    deployment_storage: &'a HashMap<String, RawStorageConfig>,
    legacy_tier: &'a str,
    tier_store: &'a ConfigStore,
    tier_storage: &'a HashMap<String, RawStorageConfig>,
    tier: &'a str,
}

/// The repo's own spec fields, its raw config and the storage config it names,
/// from its spec at `entry.config_path` in `store`, resolved for `tier`.
fn resolve(
    entry: &TierRepoEntry,
    store: &ConfigStore,
    storage: &HashMap<String, RawStorageConfig>,
    tier: &str,
) -> Result<(Value, RawRepoConfig, RawStorageConfig)> {
    let spec = configerator_repo_spec_handle(&entry.config_path, store)?.get();
    let resolved = resolve_repo_spec_with_provenance(&spec, tier, storage)?;
    Ok((spec_fields(&spec)?, resolved.config.value, resolved.storage))
}

/// A spec's fields outside the layers that resolve into its config, such as
/// `readonly` and `hipster_acl`.
fn spec_fields(spec: &RepoSpec) -> Result<Value> {
    let mut value = serde_json::to_value(spec)?;
    if let Value::Object(fields) = &mut value {
        for layer in ["repo_config", "tier_overrides"] {
            fields.remove(layer);
        }
    }
    Ok(value)
}

/// The differences between the repos the deployment gives a service and the
/// repos its tier lists, including their resolved configs when `specs` says
/// how to resolve them.
fn compare_repos(
    deployment: &[TierRepoEntry],
    tier: &[TierRepoEntry],
    location: &DeploymentLocation,
    specs: Option<SpecSides<'_>>,
) -> Vec<String> {
    let by_name = |entries: &[TierRepoEntry]| -> HashMap<String, TierRepoEntry> {
        entries
            .iter()
            .map(|entry| (entry.repo_name.clone(), entry.clone()))
            .collect()
    };
    let (deployment, tier) = (by_name(deployment), by_name(tier));
    let names = |entries: &HashMap<String, TierRepoEntry>| -> BTreeSet<String> {
        entries.keys().cloned().collect()
    };
    let (deployment_names, tier_names) = (names(&deployment), names(&tier));

    let mut differences = Vec::new();
    let mut list = |what: &str, repos: Vec<&String>| {
        if !repos.is_empty() {
            differences.push(format!("{what}: {}", listed(&repos)));
        }
    };
    list(
        "repos only in the deployment",
        deployment_names.difference(&tier_names).collect(),
    );
    list(
        "repos only in the tier",
        tier_names.difference(&deployment_names).collect(),
    );

    let common: Vec<&String> = deployment_names.intersection(&tier_names).collect();
    let mut differing = |what: &str, differs: &dyn Fn(&TierRepoEntry, &TierRepoEntry) -> bool| {
        let repos: Vec<&String> = common
            .iter()
            .copied()
            .filter(|name| differs(&deployment[*name], &tier[*name]))
            .collect();
        list(what, repos);
    };
    differing("repos with a different id", &|d, t| d.repo_id != t.repo_id);
    differing("repos with different deep sharding", &|d, t| {
        d.is_deep_sharded != t.is_deep_sharded
    });
    differing("repos enabled differently", &|d, t| {
        d.enabled.unwrap_or(true) != t.enabled.unwrap_or(true)
    });
    if location.root == ConfigRoot::ConfigStore {
        differing("repos with a different spec path", &|d, t| {
            d.config_path != t.config_path
        });
    }
    drop(differing);

    if let Some(sides) = specs {
        let mut different = Vec::new();
        let mut deployment_fails = Vec::new();
        let mut tier_fails = Vec::new();
        for name in &common {
            let ours = resolve(
                &deployment[*name],
                sides.deployment_store,
                sides.deployment_storage,
                sides.legacy_tier,
            );
            let theirs = resolve(
                &tier[*name],
                sides.tier_store,
                sides.tier_storage,
                sides.tier,
            );
            match (ours, theirs) {
                (Ok(ours), Ok(theirs)) if ours != theirs => different.push(*name),
                (Ok(_), Ok(_)) => {}
                (ours, theirs) => {
                    if let Err(e) = ours {
                        deployment_fails.push((*name, e));
                    }
                    if let Err(e) = theirs {
                        tier_fails.push((*name, e));
                    }
                }
            }
        }
        list("repos whose configs resolve differently", different);
        for (side, fails) in [("deployment", deployment_fails), ("tier", tier_fails)] {
            let Some((first, error)) = fails.first() else {
                continue;
            };
            let names: Vec<&String> = fails.iter().map(|(name, _)| *name).collect();
            differences.push(format!(
                "repos the {side} cannot resolve: {} ({first}: {error:#})",
                listed(&names)
            ));
        }
    }
    differences
}

/// The top-level fields of `what` that differ between the deployment's value
/// and the tier's.
fn compare_values<T: Serialize + PartialEq>(
    what: &str,
    deployment: &T,
    tier: &T,
) -> Result<Vec<String>> {
    if deployment == tier {
        return Ok(Vec::new());
    }
    let fields = |value: &T| -> Result<BTreeMap<String, Value>> {
        Ok(match serde_json::to_value(value)? {
            Value::Object(fields) => fields
                .into_iter()
                .filter(|(_, value)| !value.is_null())
                .collect(),
            other => BTreeMap::from([(String::new(), other)]),
        })
    };
    let (deployment, tier) = (fields(deployment)?, fields(tier)?);
    let keys: BTreeSet<&String> = deployment.keys().chain(tier.keys()).collect();
    let differing: Vec<&String> = keys
        .into_iter()
        .filter(|key| deployment.get(*key) != tier.get(*key))
        .collect();
    // The values can differ in a way their JSON does not show.
    if differing.is_empty() {
        return Ok(vec![format!("{what} differs")]);
    }
    Ok(vec![format!("{what} differs in: {}", listed(&differing))])
}

/// `names`, at most [`MAX_LISTED`] of them, with a count of the rest.
fn listed(names: &[&String]) -> String {
    let shown: Vec<&str> = names
        .iter()
        .take(MAX_LISTED)
        .map(|name| name.as_str())
        .collect();
    let rest = names.len().saturating_sub(MAX_LISTED);
    if rest == 0 {
        shown.join(", ")
    } else {
        format!("{} and {rest} more", shown.join(", "))
    }
}
