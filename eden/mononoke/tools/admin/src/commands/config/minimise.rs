/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! `mononoke_admin config minimise`: rewrite repo specs to name a repo-config
//! preset and keep only what differs from it, without changing what any
//! service of the deployment loads. Jobs that read tiers can't resolve a
//! preset, so that check doesn't make a spec ready to apply.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use metaconfig_parser::Presets;
use metaconfig_parser::layering::ConfigPath;
use metaconfig_parser::layering::merges_by_field;
use metaconfig_parser::layering::union_paths;
use metaconfig_parser::resolve_repo_spec_with_provenance;
use mononoke_app::MononokeApp;
use repos::RawRepoConfig;
use repos::RawStorageConfig;
use repos::RepoSpec;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;

use super::deployment_specs::DeploymentSpecs;
use super::strip_nulls;

#[derive(Parser)]
pub struct MinimiseArgs {
    /// The repos to minimise.
    #[clap(long, required_unless_present = "all")]
    repo: Vec<String>,

    /// Minimise every repo of the deployment.
    #[clap(long, conflicts_with = "repo")]
    all: bool,

    /// The preset to minimise against. Without it, each repo uses
    /// whichever preset gives it the smallest spec.
    #[clap(long)]
    preset: Option<String>,

    /// Minimise against the presets in this file (presets by name, as JSON)
    /// instead of the deployment's, to try presets before adding them.
    #[clap(long)]
    presets: Option<PathBuf>,

    /// Print one JSON object per repo instead of a summary.
    #[clap(long)]
    json: bool,
}

/// The outcome for one repo.
#[derive(Serialize)]
struct Outcome {
    repo: String,
    config_path: String,
    /// The minimised spec, if one resolves to the same config on every tier.
    spec: Option<Value>,
    preset: Option<String>,
    bytes_before: usize,
    bytes_after: Option<usize>,
    /// Why no preset gave an equivalent spec: for each preset tried, the
    /// fields it sets that the repo's resolved config does not have.
    problems: BTreeMap<String, Vec<String>>,
    /// No service of the deployment serves the repo, so nothing checks a
    /// minimised spec: it isn't minimised.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    unserved: bool,
    /// Tiers the spec overrides that no service of the deployment uses: such a
    /// spec isn't minimised.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unserved_tiers: Vec<String>,
    /// Tiers the spec is in that no service of the deployment uses. Their jobs
    /// can't resolve a preset, so a minimised spec waits for them to move.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unserved_member_tiers: Vec<String>,
}

/// The presets specs resolve with today, and the presets a minimised spec is
/// resolved with.
struct PresetChoices {
    current: Option<Presets>,
    proposed: Option<Presets>,
}

pub async fn run(app: MononokeApp, args: MinimiseArgs) -> Result<()> {
    let configs = app.configs();
    let specs = DeploymentSpecs::new(&configs, "minimise")?;
    let deployment = &specs.deployment;
    let presets = PresetChoices {
        current: deployment.presets.clone(),
        proposed: match &args.presets {
            Some(path) => {
                let text = tokio::fs::read_to_string(path)
                    .await
                    .with_context(|| format!("reading {}", path.display()))?;
                Some(
                    serde_json::from_str(&text)
                        .with_context(|| format!("parsing {}", path.display()))?,
                )
            }
            None => deployment.presets.clone(),
        },
    };
    let has_candidates = presets
        .proposed
        .as_ref()
        .is_some_and(|presets| !presets.is_empty());
    if args.preset.is_none() && !has_candidates {
        anyhow::bail!(
            "there are no presets to minimise against: the deployment has none, \
             so pass --presets"
        );
    }

    let storage = specs.storage();
    let repos = specs.select(&args.repo, args.all)?;

    let mut minimised = 0usize;
    let mut failed = 0usize;
    let (mut bytes_before, mut bytes_after) = (0usize, 0usize);
    for repo in repos {
        let result = (|| -> Result<Outcome> {
            let (config_path, spec) = specs.spec(repo)?;
            let tiers: BTreeSet<String> = specs
                .services(repo)?
                .into_iter()
                .map(|(_, tier)| tier)
                .collect();
            minimise(
                &spec,
                &config_path,
                &tiers,
                args.preset.as_deref(),
                &storage,
                &presets,
            )
        })();
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(err) => {
                failed += 1;
                if args.json {
                    println!(
                        "{}",
                        serde_json::json!({"repo": repo.repo_name, "error": format!("{err:#}")})
                    );
                } else {
                    println!("{}: failed: {err:#}", repo.repo_name);
                }
                continue;
            }
        };
        bytes_before += outcome.bytes_before;
        bytes_after += outcome.bytes_after.unwrap_or(outcome.bytes_before);
        if outcome.spec.is_some() {
            minimised += 1;
        }
        if args.json {
            println!("{}", serde_json::to_string(&outcome)?);
        } else if let (Some(preset), Some(after)) = (&outcome.preset, outcome.bytes_after) {
            let waits = if outcome.unserved_member_tiers.is_empty() {
                String::new()
            } else {
                format!(
                    " (in tiers no service uses: {})",
                    outcome.unserved_member_tiers.join(", ")
                )
            };
            println!(
                "{}: preset {preset}, {} -> {after} bytes{waits}",
                outcome.repo, outcome.bytes_before
            );
        } else if outcome.unserved {
            println!(
                "{}: not minimised (no service of the deployment serves it, so nothing checks it)",
                outcome.repo
            );
        } else if !outcome.unserved_tiers.is_empty() {
            println!(
                "{}: not minimised (overrides tiers no service uses: {})",
                outcome.repo,
                outcome.unserved_tiers.join(", ")
            );
        } else {
            let problems: Vec<String> = outcome
                .problems
                .iter()
                .map(|(preset, fields)| format!("{preset}: {}", fields.join(", ")))
                .collect();
            println!("{}: not minimised ({})", outcome.repo, problems.join("; "));
        }
    }
    if !args.json {
        println!("Minimised {minimised} specs: {bytes_before} -> {bytes_after} bytes");
    }
    if failed > 0 {
        anyhow::bail!("{failed} specs could not be resolved");
    }
    Ok(())
}

fn json_size(value: &Value) -> usize {
    serde_json::to_string(value).map_or(0, |text| text.len())
}

/// A repo's resolved config on `tier`, without unset fields.
fn resolved_json(
    spec: &RepoSpec,
    tier: &str,
    storage: &HashMap<String, RawStorageConfig>,
    presets: Option<&Presets>,
) -> Result<Value> {
    let mut value = serde_json::to_value(
        resolve_repo_spec_with_provenance(spec, tier, storage, presets)?
            .config
            .value,
    )?;
    strip_nulls(&mut value);
    Ok(value)
}

fn minimise(
    spec: &RepoSpec,
    config_path: &str,
    tiers: &BTreeSet<String>,
    preset: Option<&str>,
    storage: &HashMap<String, RawStorageConfig>,
    presets: &PresetChoices,
) -> Result<Outcome> {
    let mut original_json = serde_json::to_value(spec)?;
    strip_nulls(&mut original_json);
    // Tier-mode jobs read overrides for tiers that no service of the deployment
    // uses, and can't resolve a spec that names a preset.
    let unserved_tiers: Vec<String> = spec
        .tier_overrides
        .iter()
        .flatten()
        .map(|(tier, _)| tier)
        .filter(|tier| !tiers.contains(*tier))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut outcome = Outcome {
        repo: spec.repo_name.clone(),
        config_path: config_path.to_owned(),
        spec: None,
        preset: None,
        bytes_before: json_size(&original_json),
        bytes_after: None,
        problems: BTreeMap::new(),
        unserved: tiers.is_empty(),
        unserved_tiers,
        unserved_member_tiers: spec
            .tiers
            .iter()
            .filter(|tier| !tiers.contains(*tier))
            .cloned()
            .collect(),
    };
    if outcome.unserved || !outcome.unserved_tiers.is_empty() {
        return Ok(outcome);
    }

    // What each of the repo's tiers loads today.
    let targets = tiers
        .iter()
        .map(|tier| {
            Ok((
                tier.clone(),
                resolved_json(spec, tier, storage, presets.current.as_ref())?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;

    let candidates: Vec<String> = match preset {
        Some(preset) => vec![preset.to_owned()],
        None => presets
            .proposed
            .as_ref()
            .map(|presets| presets.keys().cloned().collect())
            .unwrap_or_default(),
    };
    for name in candidates {
        match try_preset(spec, &name, &targets, storage, presets.proposed.as_ref())? {
            Ok(candidate) => {
                let size = json_size(&candidate);
                // A preset that makes the spec no smaller is proposed only when
                // asked for.
                let worth_it = preset.is_some() || size < outcome.bytes_before;
                if worth_it && outcome.bytes_after.is_none_or(|best| size < best) {
                    outcome.bytes_after = Some(size);
                    outcome.spec = Some(candidate);
                    outcome.preset = Some(name);
                }
            }
            Err(fields) => {
                outcome.problems.insert(name, fields);
            }
        }
    }
    Ok(outcome)
}

/// A spec that names preset `name` and resolves to `targets` on
/// every tier, or the fields that stop one from existing: fields the preset
/// sets that a tier's resolved config does not have.
fn try_preset(
    spec: &RepoSpec,
    name: &str,
    targets: &BTreeMap<String, Value>,
    storage: &HashMap<String, RawStorageConfig>,
    presets: Option<&Presets>,
) -> Result<std::result::Result<Value, Vec<String>>> {
    let Some(preset) = presets.and_then(|presets| presets.get(name)) else {
        return Ok(Err(vec![format!("no preset {name}")]));
    };
    let mut preset_json = serde_json::to_value(preset)?;
    strip_nulls(&mut preset_json);
    let spec_raw = spec.repo_config.clone().unwrap_or_default();
    let mut spec_config = serde_json::to_value(&spec_raw)?;
    strip_nulls(&mut spec_config);

    let layer = sparse_layer(
        &spec_config,
        &preset_json,
        &mut Vec::new(),
        &union_paths(&spec_raw)?,
    );
    let layer_config: RawRepoConfig = serde_json::from_value(layer)?;
    let mut candidate = RepoSpec {
        preset: Some(name.to_owned()),
        repo_config: Some(layer_config),
        tier_overrides: None,
        ..spec.clone()
    };

    // A tier override restates each top-level field that the preset and the
    // layer do not give the tier its current value for.
    let mut overrides = BTreeMap::new();
    let mut problems = BTreeSet::new();
    for (tier, target) in targets {
        let merged = match resolved_json(&candidate, tier, storage, presets) {
            Ok(merged) => merged,
            Err(err) => return Ok(Err(vec![format!("tier {tier}: {err:#}")])),
        };
        let (Value::Object(target_fields), Value::Object(merged_fields)) = (target, &merged) else {
            continue;
        };
        let tier_override: Map<String, Value> = target_fields
            .iter()
            .filter(|(field, value)| merged_fields.get(*field) != Some(*value))
            .map(|(field, value)| (field.clone(), value.clone()))
            .collect();
        problems.extend(
            merged_fields
                .keys()
                .filter(|field| !target_fields.contains_key(*field))
                .map(|field| format!("{field} (tier {tier})")),
        );
        if !tier_override.is_empty() {
            overrides.insert(tier.clone(), Value::Object(tier_override));
        }
    }
    if !problems.is_empty() {
        return Ok(Err(problems.into_iter().collect()));
    }
    if !overrides.is_empty() {
        candidate.tier_overrides = Some(serde_json::from_value(serde_json::to_value(&overrides)?)?);
    }

    // The candidate must load exactly what each tier loads today.
    for (tier, target) in targets {
        match resolved_json(&candidate, tier, storage, presets) {
            Ok(merged) if &merged == target => {}
            Ok(_) => {
                return Ok(Err(vec![format!(
                    "does not resolve the same on tier {tier}"
                )]));
            }
            Err(err) => return Ok(Err(vec![format!("tier {tier}: {err:#}")])),
        }
    }
    let mut candidate_json = serde_json::to_value(&candidate)?;
    strip_nulls(&mut candidate_json);
    Ok(Ok(candidate_json))
}

/// The smallest merging layer that turns `base` into `target` where it can:
/// fields equal to the base's are left out, values a merging layer merges by
/// field recurse (structs, maps and unions holding the base's variant; `unions`
/// are the target's union paths), and other values that differ are kept
/// whole. Fields the base has and the target lacks cannot be expressed; the
/// tier overrides restate their top-level field.
fn sparse_layer(
    target: &Value,
    base: &Value,
    segments: &mut Vec<String>,
    unions: &HashSet<ConfigPath>,
) -> Value {
    let (Value::Object(target_fields), Value::Object(base_fields)) = (target, base) else {
        return target.clone();
    };
    let mut layer = Map::new();
    for (field, value) in target_fields {
        segments.push(field.clone());
        let path = ConfigPath::from_keys(segments.iter());
        match base_fields.get(field) {
            Some(base_value) if base_value == value => {}
            Some(base_value)
                if base_value.is_object()
                    && merges_by_field(&path, Some(base_value), value, unions) =>
            {
                let sub = sparse_layer(value, base_value, segments, unions);
                if sub.as_object().is_some_and(|sub| !sub.is_empty()) {
                    layer.insert(field.clone(), sub);
                }
            }
            _ => {
                layer.insert(field.clone(), value.clone());
            }
        }
        segments.pop();
    }
    Value::Object(layer)
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;
    use serde_json::json;

    use super::*;

    fn unions_of(config: &Value) -> HashSet<ConfigPath> {
        let config: RawRepoConfig = serde_json::from_value(config.clone()).unwrap();
        union_paths(&config).unwrap()
    }

    #[mononoke::test]
    fn test_unserved_tier_overrides_are_not_minimised() -> Result<()> {
        let spec = RepoSpec {
            repo_name: "repo".to_owned(),
            repo_config: Some(RawRepoConfig::default()),
            tier_overrides: Some(
                [
                    ("scs".to_owned(), RawRepoConfig::default()),
                    ("test_scs".to_owned(), RawRepoConfig::default()),
                ]
                .into(),
            ),
            ..Default::default()
        };
        let presets = PresetChoices {
            current: None,
            proposed: Some([("git".to_owned(), RawRepoConfig::default())].into()),
        };
        let outcome = minimise(
            &spec,
            "repo",
            &["scs".to_owned()].into(),
            None,
            &HashMap::new(),
            &presets,
        )?;
        assert!(outcome.spec.is_none());
        assert_eq!(outcome.unserved_tiers, ["test_scs"]);
        Ok(())
    }

    // With no tier to check against, any preset would pass, so the spec isn't
    // minimised.
    #[mononoke::test]
    fn test_unserved_repo_is_not_minimised() -> Result<()> {
        let spec = RepoSpec {
            repo_name: "repo".to_owned(),
            repo_config: Some(RawRepoConfig::default()),
            ..Default::default()
        };
        let presets = PresetChoices {
            current: None,
            proposed: Some([("git".to_owned(), RawRepoConfig::default())].into()),
        };
        let outcome = minimise(
            &spec,
            "repo",
            &BTreeSet::new(),
            None,
            &HashMap::new(),
            &presets,
        )?;
        assert!(outcome.unserved);
        assert!(outcome.spec.is_none());
        Ok(())
    }

    #[mononoke::test]
    fn test_no_smaller_spec_is_not_minimised() -> Result<()> {
        let spec = RepoSpec {
            repo_name: "repo".to_owned(),
            repo_config: Some(RawRepoConfig {
                storage_config: Some("blobstore".to_owned()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let storage = [("blobstore".to_owned(), RawStorageConfig::default())].into();
        let presets = PresetChoices {
            current: None,
            proposed: Some([("empty".to_owned(), RawRepoConfig::default())].into()),
        };
        let tiers = ["scs".to_owned()].into();
        let outcome = minimise(&spec, "repo", &tiers, None, &storage, &presets)?;
        assert!(
            outcome.spec.is_none(),
            "naming an empty preset only adds the preset's name"
        );
        let outcome = minimise(&spec, "repo", &tiers, Some("empty"), &storage, &presets)?;
        assert!(
            outcome.spec.is_some(),
            "a preset asked for is still proposed"
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_sparse_layer() {
        let base = json!({
            "storage_config": "shared",
            "phabricator_callsign": "BASE",
            "pushrebase": {"rewritedates": true, "casefolding_check": true},
            "zelos_config": {"local_zelos_port": 1},
            "git_configs": {"git_bundle_uri_config": {
                "uri_generator_type": {"cdn": {"bucket": "b", "api_key": "k"}},
            }},
            "derived_data_config": {"enabled_config_name": "default", "scuba_table": "t"},
        });
        let target = json!({
            "storage_config": "shared",
            "phabricator_callsign": "REPO",
            "pushrebase": {"rewritedates": true, "casefolding_check": false},
            "zelos_config": {"zelos_tier": "tier"},
            "git_configs": {"git_bundle_uri_config": {
                "uri_generator_type": {"cdn": {"bucket": "b", "api_key": "k2"}},
            }},
            "derived_data_config": {"enabled_config_name": "default", "scuba_table": "u"},
            "bookmarks": [],
        });
        assert_eq!(
            sparse_layer(&target, &base, &mut Vec::new(), &unions_of(&target)),
            json!({
                "phabricator_callsign": "REPO",
                "pushrebase": {"casefolding_check": false},
                "zelos_config": {"zelos_tier": "tier"},
                "git_configs": {"git_bundle_uri_config": {
                    "uri_generator_type": {"cdn": {"api_key": "k2"}},
                }},
                "derived_data_config": {"enabled_config_name": "default", "scuba_table": "u"},
                "bookmarks": [],
            }),
            "equal fields are left out, structs and a union holding the base's variant \
             recurse, and a union set to another variant, derived data and new fields are \
             kept whole"
        );
        assert_eq!(
            sparse_layer(&base, &base, &mut Vec::new(), &unions_of(&base)),
            json!({}),
            "a config equal to the base needs no layer"
        );
    }
}
