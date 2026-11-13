/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! A repo's raw config is built from layers, lowest first. This module merges
//! the layers and records which layer set each value, so that tools can show a
//! repo's resolved config and where each part of it came from.

mod unions;

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use derive_more::Display;
use repos::RawRepoConfig;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;

/// Where a value of a repo's resolved config came from.
#[derive(Clone, Debug, Display, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConfigSource {
    /// No layer sets the value, so Mononoke's default applies.
    #[display("default")]
    Default,
    /// The named preset in the deployment.
    #[display("preset {_0}")]
    Preset(String),
    /// The repo's spec.
    #[display("spec")]
    Spec,
    /// The repo spec's override for the named tier.
    #[display("tier override {_0}")]
    TierOverride(String),
}

/// One step of a [`ConfigPath`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PathSegment {
    /// A field, or a key of a map.
    Key(String),
    /// An index into a list.
    Index(usize),
}

/// Where a value is in a config: the fields, map keys and list indexes that
/// lead to it.
///
/// Its text form is a list of steps, as in `bookmarks[2].hooks`. A key is a
/// plain name, written bare (first) or after a `.`, or any key written in
/// brackets as a JSON string, as in `service_write_restrictions["clusterops.fbservices"]`.
/// A list index is written in brackets as a number, so `[2]` is an index and
/// `["2"]` a key. A plain name starts with a letter or `_`, followed by letters,
/// digits, `_` and `-`. A path is printed with plain names bare and other keys
/// in brackets, so a printed path always parses back to the same path.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConfigPath(Vec<PathSegment>);

impl ConfigPath {
    /// The path of the whole config.
    pub fn new() -> Self {
        Self::default()
    }

    /// The path of the value at `keys`, one field or map key after another.
    pub fn from_keys<K: Into<String>>(keys: impl IntoIterator<Item = K>) -> Self {
        Self(
            keys.into_iter()
                .map(|key| PathSegment::Key(key.into()))
                .collect(),
        )
    }

    /// The path of the field or map key `key` of the value at this path.
    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.0.push(PathSegment::Key(key.into()));
        self
    }

    /// The path of entry `index` of the list at this path.
    pub fn index(mut self, index: usize) -> Self {
        self.0.push(PathSegment::Index(index));
        self
    }

    /// The path of the value at `path` inside the value at this path.
    pub fn join(mut self, path: &ConfigPath) -> Self {
        self.0.extend_from_slice(&path.0);
        self
    }

    /// The steps of the path, from the top.
    pub fn segments(&self) -> &[PathSegment] {
        &self.0
    }

    /// Whether the value at this path is inside the value at `ancestor`.
    pub fn is_below(&self, ancestor: &ConfigPath) -> bool {
        self.0.len() > ancestor.0.len() && self.0.starts_with(&ancestor.0)
    }
}

impl Borrow<[PathSegment]> for ConfigPath {
    fn borrow(&self) -> &[PathSegment] {
        &self.0
    }
}

impl fmt::Display for ConfigPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (position, segment) in self.0.iter().enumerate() {
            match segment {
                PathSegment::Key(key) if is_plain_name(key) && position == 0 => write!(f, "{key}")?,
                PathSegment::Key(key) if is_plain_name(key) => write!(f, ".{key}")?,
                PathSegment::Key(key) => write!(f, "[{}]", Value::String(key.clone()))?,
                PathSegment::Index(index) => write!(f, "[{index}]")?,
            }
        }
        Ok(())
    }
}

impl FromStr for ConfigPath {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        parse_path(text).map_err(|reason| {
            anyhow!(
                "Invalid config path {text}: {reason}. Write a path as names and list indexes, \
                 such as bookmarks[2].hooks, with a key that is not a plain name in brackets \
                 as a JSON string, such as [\"clusterops.fbservices\"]"
            )
        })
    }
}

/// Whether `key` can be written bare in a path's text form.
fn is_plain_name(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn parse_path(text: &str) -> Result<ConfigPath, String> {
    let mut segments = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(inside) = rest.strip_prefix('[') {
            // The bracketed value is read by the JSON parser, which also finds its end.
            let mut values = serde_json::Deserializer::from_str(inside).into_iter::<Value>();
            let value = match values.next() {
                Some(Ok(value)) => value,
                Some(Err(e)) => return Err(format!("a `[` holds no JSON string or number ({e})")),
                None => return Err("a `[` is not closed".to_owned()),
            };
            rest = inside[values.byte_offset()..]
                .trim_start()
                .strip_prefix(']')
                .ok_or_else(|| "a `[` is not closed".to_owned())?;
            segments.push(match value {
                Value::String(key) => PathSegment::Key(key),
                Value::Number(number) => number
                    .as_u64()
                    .and_then(|index| usize::try_from(index).ok())
                    .map(PathSegment::Index)
                    .ok_or_else(|| format!("[{number}] is not a list index"))?,
                other => return Err(format!("[{other}] is neither a key nor a list index")),
            });
        } else {
            let name = if segments.is_empty() {
                rest
            } else {
                rest.strip_prefix('.')
                    .ok_or_else(|| format!("`{rest}` does not start with `.` or `[`"))?
            };
            let (name, after) = name.split_at(name.find(['.', '[']).unwrap_or(name.len()));
            if name.is_empty() {
                return Err("a name is missing".to_owned());
            }
            if !is_plain_name(name) {
                return Err(format!(
                    "`{name}` is not a plain name, so it needs brackets: [{}]",
                    Value::String(name.to_owned())
                ));
            }
            segments.push(PathSegment::Key(name.to_owned()));
            rest = after;
        }
    }
    if segments.is_empty() {
        return Err("it names no value".to_owned());
    }
    Ok(ConfigPath(segments))
}

/// Which layer set each value of a layered config, by path. A value's source is
/// the one recorded for its path or for its closest recorded ancestor; a value
/// with neither comes from [`ConfigSource::Default`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Provenance {
    sources: BTreeMap<ConfigPath, ConfigSource>,
}

impl Provenance {
    /// The layer that set the value at `path`.
    pub fn source(&self, path: &ConfigPath) -> &ConfigSource {
        let segments = path.segments();
        (1..=segments.len())
            .rev()
            .find_map(|len| self.sources.get(&segments[..len]))
            .unwrap_or(&ConfigSource::Default)
    }

    /// Records that `source` set the whole value at `path`, replacing anything
    /// lower layers set inside it.
    pub fn record(&mut self, path: &ConfigPath, source: ConfigSource) {
        // The paths of the values inside a value sort straight after its own.
        let inside: Vec<ConfigPath> = self
            .sources
            .range(path.clone()..)
            .map(|(recorded, _)| recorded)
            .take_while(|recorded| recorded.segments().starts_with(path.segments()))
            .cloned()
            .collect();
        for recorded in inside {
            self.sources.remove(&recorded);
        }
        self.sources.insert(path.clone(), source);
    }

    /// Every recorded path and its source, in path order.
    pub fn iter(&self) -> impl Iterator<Item = (&ConfigPath, &ConfigSource)> {
        self.sources.iter()
    }
}

/// A value built from layers, and the layer that set each part of it.
#[derive(Clone, Debug, PartialEq)]
pub struct Layered<T> {
    /// The resolved value.
    pub value: T,
    /// Which layer set each part of the value.
    pub provenance: Provenance,
}

/// How a layer combines with the layers below it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerMode {
    /// A field the layer sets replaces the one below it, except that a struct
    /// is merged field by field, a map key by key, and a thrift union that
    /// holds the same variant as the one below has that variant's value merged
    /// field by field. Lists, sets, a union that holds another variant, and
    /// the fields in [`REPLACED_WHOLE`] are replaced whole (see
    /// [`merges_by_field`]).
    Merge,
    /// Each top-level field the layer sets replaces the one below it whole.
    ReplaceTopLevel,
}

/// One layer of a repo's raw config.
pub struct Layer<'a> {
    /// Where the layer comes from.
    pub source: ConfigSource,
    /// The fields the layer sets; unset fields leave the layers below.
    pub config: &'a RawRepoConfig,
    /// How the layer combines with the layers below it.
    pub mode: LayerMode,
}

/// Fields of `RawRepoConfig`, by path, that a merging layer replaces whole.
///
/// This is temporary: `derived_data_config` is listed only until the
/// derived-data config is reworked. Its named configs and its preset form are
/// alternatives, and a layer must be able to move a repo from one to the
/// other.
pub const REPLACED_WHOLE: &[&[&str]] = &[&["derived_data_config"]];

/// The paths of the thrift unions in `config`, found as it serializes: the
/// JSON a union serializes to, an object with one key, does not tell it apart
/// from a struct.
pub fn union_paths<T: Serialize + ?Sized>(config: &T) -> Result<HashSet<ConfigPath>> {
    Ok(unions::union_paths(config)?)
}

/// Whether a merging layer merges `value`, at `path`, into the value `below`
/// it field by field rather than replacing it whole. It does when `value` is a
/// struct or map that is not in [`REPLACED_WHOLE`], and, if `value` is a thrift
/// union (its path is in `unions`, the layer's [`union_paths`]), `below` holds
/// the same variant.
pub fn merges_by_field(
    path: &ConfigPath,
    below: Option<&Value>,
    value: &Value,
    unions: &HashSet<ConfigPath>,
) -> bool {
    value.is_object()
        && !replaced_whole(path)
        && (!unions.contains(path) || same_variant(below, value))
}

/// Whether two serialized thrift unions hold the same variant: each is an
/// object whose one key is the variant's name.
fn same_variant(below: Option<&Value>, value: &Value) -> bool {
    match (below.and_then(Value::as_object), value.as_object()) {
        (Some(below), Some(value)) => below.keys().eq(value.keys()),
        _ => false,
    }
}

fn replaced_whole(path: &ConfigPath) -> bool {
    REPLACED_WHOLE.iter().any(|whole| {
        whole.len() == path.0.len()
            && whole
                .iter()
                .zip(&path.0)
                .all(|(field, segment)| matches!(segment, PathSegment::Key(key) if key == field))
    })
}

/// Builds a raw repo config from its layers, lowest first, recording which
/// layer set each value.
pub fn layer_repo_config_with_provenance(layers: &[Layer<'_>]) -> Result<Layered<RawRepoConfig>> {
    let mut provenance = Provenance::default();
    let value = merge_layers(layers, Some(&mut provenance))?;
    Ok(Layered { value, provenance })
}

/// Builds the same raw repo config as [`layer_repo_config_with_provenance`]
/// without recording where each value came from.
pub fn layer_repo_config(layers: &[Layer<'_>]) -> Result<RawRepoConfig> {
    if let [layer] = layers {
        return Ok(layer.config.clone());
    }
    merge_layers(layers, None)
}

fn merge_layers(
    layers: &[Layer<'_>],
    mut provenance: Option<&mut Provenance>,
) -> Result<RawRepoConfig> {
    let mut merged = Value::Object(Map::new());
    for (index, layer) in layers.iter().enumerate() {
        let value = serde_json::to_value(layer.config)?;
        if index == 0 && provenance.is_none() {
            merged = value;
            continue;
        }
        match layer.mode {
            LayerMode::Merge => merge(
                &mut merged,
                value,
                &mut Vec::new(),
                &union_paths(layer.config)?,
                &layer.source,
                provenance.as_deref_mut(),
            ),
            LayerMode::ReplaceTopLevel => {
                replace_top_level(&mut merged, value, &layer.source, provenance.as_deref_mut())
            }
        }
    }
    serde_json::from_value(merged).context("Failed to deserialize layered config")
}

fn merge(
    base: &mut Value,
    layer: Value,
    segments: &mut Vec<String>,
    unions: &HashSet<ConfigPath>,
    source: &ConfigSource,
    mut provenance: Option<&mut Provenance>,
) {
    let (Value::Object(base), Value::Object(layer)) = (base, layer) else {
        unreachable!("merge is only called on objects");
    };
    for (key, value) in layer {
        if value.is_null() {
            continue;
        }
        segments.push(key);
        let key = segments.last().expect("just pushed");
        let path = ConfigPath::from_keys(segments.iter());
        let below = base.get(key);
        let unset_below = below.is_none_or(Value::is_null);
        let object_below = below.is_some_and(Value::is_object);
        let by_field = merges_by_field(&path, below, &value, unions);
        let non_empty = value.as_object().is_some_and(|fields| !fields.is_empty());
        if by_field && (object_below || (unset_below && non_empty)) {
            // A struct or map that no layer below sets is merged field by field
            // into an empty one too, so this layer owns only the fields it sets.
            let existing = base
                .entry(key.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            if existing.is_null() {
                *existing = Value::Object(Map::new());
            }
            merge(
                existing,
                value,
                segments,
                unions,
                source,
                provenance.as_deref_mut(),
            );
        } else {
            if let Some(provenance) = provenance.as_deref_mut() {
                provenance.record(&path, source.clone());
            }
            base.insert(key.clone(), value);
        }
        segments.pop();
    }
}

fn replace_top_level(
    base: &mut Value,
    layer: Value,
    source: &ConfigSource,
    mut provenance: Option<&mut Provenance>,
) {
    let (Value::Object(base), Value::Object(layer)) = (base, layer) else {
        unreachable!("replace_top_level is only called on objects");
    };
    for (key, value) in layer {
        if !value.is_null() {
            if let Some(provenance) = provenance.as_deref_mut() {
                provenance.record(&ConfigPath::new().key(key.as_str()), source.clone());
            }
            base.insert(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;
    use serde_json::json;

    use super::*;

    fn path(text: &str) -> ConfigPath {
        text.parse().expect("valid path")
    }

    fn raw(value: Value) -> RawRepoConfig {
        serde_json::from_value(value).expect("valid RawRepoConfig")
    }

    fn preset() -> RawRepoConfig {
        raw(json!({
            "storage_config": "preset_storage",
            "phabricator_callsign": "PRESET",
            "pushrebase": {"rewritedates": true, "casefolding_check": true},
            "derived_data_config": {
                "available_configs": {
                    "default": {"types": ["unodes", "fsnodes"], "mapping_key_prefixes": {}},
                },
                "enabled_config_name": "default",
            },
            "zelos_config": {"local_zelos_port": 1},
            "bookmarks": [{"name": "main", "hooks": [], "only_fast_forward": true}],
            "infinitepush": {"allow_writes": true, "namespace_pattern": "scratch/.*"},
        }))
    }

    fn spec() -> RawRepoConfig {
        raw(json!({
            "phabricator_callsign": "SPEC",
            "pushrebase": {"casefolding_check": false},
            "derived_data_config": {"scuba_table": "spec_table"},
            "zelos_config": {"zelos_tier": "tier"},
            "bookmarks": [],
            "x_repo_sync_source_mapping": {"mapping": {}},
        }))
    }

    fn tier_override() -> RawRepoConfig {
        raw(json!({"infinitepush": {"allow_writes": false}}))
    }

    fn layered() -> Layered<RawRepoConfig> {
        let (preset, spec, tier_override) = (preset(), spec(), tier_override());
        layer_repo_config_with_provenance(&[
            Layer {
                source: ConfigSource::Preset("git".to_owned()),
                config: &preset,
                mode: LayerMode::Merge,
            },
            Layer {
                source: ConfigSource::Spec,
                config: &spec,
                mode: LayerMode::Merge,
            },
            Layer {
                source: ConfigSource::TierOverride("scs".to_owned()),
                config: &tier_override,
                mode: LayerMode::ReplaceTopLevel,
            },
        ])
        .expect("layers merge")
    }

    #[mononoke::test]
    fn test_a_partial_struct_owns_only_its_fields() {
        let preset = raw(json!({"storage_config": "preset_storage"}));
        let spec = raw(json!({"pushrebase": {"casefolding_check": false}}));
        let layered = layer_repo_config_with_provenance(&[
            Layer {
                source: ConfigSource::Preset("git".to_owned()),
                config: &preset,
                mode: LayerMode::Merge,
            },
            Layer {
                source: ConfigSource::Spec,
                config: &spec,
                mode: LayerMode::Merge,
            },
        ])
        .expect("layers merge");
        assert_eq!(
            layered
                .provenance
                .source(&path("pushrebase.casefolding_check")),
            &ConfigSource::Spec
        );
        assert_eq!(
            layered.provenance.source(&path("pushrebase.rewritedates")),
            &ConfigSource::Default,
            "a field the spec leaves unset is not the spec's, so a preset could still set it"
        );
        assert_eq!(
            serde_json::to_value(&layered.value).unwrap()["pushrebase"]["casefolding_check"],
            false
        );
    }

    #[mononoke::test]
    fn test_merge_rules() {
        let merged = serde_json::to_value(layered().value).unwrap();

        assert_eq!(
            merged["storage_config"], "preset_storage",
            "unset fields keep the layer below"
        );
        assert_eq!(
            merged["phabricator_callsign"], "SPEC",
            "set fields replace the layer below"
        );
        assert_eq!(
            merged["pushrebase"]["rewritedates"], true,
            "structs merge by field"
        );
        assert_eq!(merged["pushrebase"]["casefolding_check"], false);
        assert_eq!(merged["derived_data_config"]["scuba_table"], "spec_table");
        assert!(
            merged["derived_data_config"]["available_configs"].is_null(),
            "derived_data_config is replaced whole"
        );
        assert_eq!(merged["zelos_config"]["zelos_tier"], "tier");
        assert!(
            merged["zelos_config"]["local_zelos_port"].is_null(),
            "unions are replaced whole"
        );
        assert_eq!(merged["bookmarks"], json!([]), "lists are replaced whole");
        assert_eq!(merged["infinitepush"]["allow_writes"], false);
        assert!(
            merged["infinitepush"]["namespace_pattern"].is_null(),
            "a tier override replaces top-level fields whole"
        );
    }

    #[mononoke::test]
    fn test_unions() {
        // A union set to another variant is replaced; the same variant has its
        // value merged field by field.
        let preset = raw(json!({
            "zelos_config": {"local_zelos_port": 1},
            "metadata_cache_config": {"wbc_update_mode": {"tailing": {"category": "preset"}}},
            "pushrebase": {"remote_mode": {"remote_land_service": {"tier": "preset_tier"}}},
            "update_logging_config": {
                "bookmark_logging_destination": {"scribe": {"scribe_category": "preset"}},
            },
        }));
        let spec = raw(json!({
            "metadata_cache_config": {"wbc_update_mode": {"polling": {}}},
            "pushrebase": {"remote_mode": {"remote_land_service": {"host_port": "host:1"}}},
            "update_logging_config": {
                "bookmark_logging_destination": {"scribe": {"scribe_category": "spec"}},
            },
        }));
        let layered = layer_repo_config_with_provenance(&[
            Layer {
                source: ConfigSource::Preset("git".to_owned()),
                config: &preset,
                mode: LayerMode::Merge,
            },
            Layer {
                source: ConfigSource::Spec,
                config: &spec,
                mode: LayerMode::Merge,
            },
        ])
        .expect("layers merge");
        let merged = serde_json::to_value(&layered.value).unwrap();
        assert_eq!(
            merged["metadata_cache_config"]["wbc_update_mode"],
            json!({"polling": {}})
        );
        assert_eq!(
            merged["pushrebase"]["remote_mode"],
            json!({"remote_land_service": {"host_port": "host:1"}})
        );
        assert_eq!(
            merged["update_logging_config"]["bookmark_logging_destination"],
            json!({"scribe": {"scribe_category": "spec"}})
        );
        assert_eq!(merged["zelos_config"], json!({"local_zelos_port": 1}));

        let preset_source = ConfigSource::Preset("git".to_owned());
        let source = |text: &str| layered.provenance.source(&path(text)).clone();
        assert_eq!(
            source("metadata_cache_config.wbc_update_mode"),
            ConfigSource::Spec
        );
        assert_eq!(
            source("pushrebase.remote_mode"),
            preset_source,
            "the same variant stays the preset's"
        );
        assert_eq!(
            source("pushrebase.remote_mode.remote_land_service"),
            ConfigSource::Spec
        );
        assert_eq!(
            source("update_logging_config.bookmark_logging_destination.scribe.scribe_category"),
            ConfigSource::Spec
        );
        assert_eq!(source("zelos_config"), preset_source);
    }

    #[mononoke::test]
    fn test_union_paths() {
        let config = raw(json!({
            "storage_config": "storage",
            "zelos_config": {"zelos_tier": "tier"},
            "pushrebase": {"remote_mode": {"remote_land_service": {"tier": "tier"}}},
            "update_logging_config": {"new_commit_logging_destination": {"logger": {}}},
            "x_repo_sync_source_mapping": {"mapping": {"a.b": {"bookmark_regex": "x"}}},
        }));
        let mut unions: Vec<String> = union_paths(&config)
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect();
        unions.sort();
        assert_eq!(
            unions,
            [
                "pushrebase.remote_mode",
                "pushrebase.remote_mode.remote_land_service",
                "update_logging_config.new_commit_logging_destination",
                "zelos_config",
            ]
        );
    }

    #[mononoke::test]
    fn test_map_keys_merge() {
        let lower = raw(json!({"x_repo_sync_source_mapping": {"mapping": {
            "a": {"bookmark_regex": "a", "backsync_enabled": true},
        }}}));
        let upper = raw(json!({"x_repo_sync_source_mapping": {"mapping": {
            "b": {"bookmark_regex": "b", "backsync_enabled": false},
        }}}));
        let layered = layer_repo_config_with_provenance(&[
            Layer {
                source: ConfigSource::Preset("git".to_owned()),
                config: &lower,
                mode: LayerMode::Merge,
            },
            Layer {
                source: ConfigSource::Spec,
                config: &upper,
                mode: LayerMode::Merge,
            },
        ])
        .unwrap();
        let merged = serde_json::to_value(&layered.value).unwrap();
        let mapping = &merged["x_repo_sync_source_mapping"]["mapping"];
        assert_eq!(mapping["a"]["bookmark_regex"], "a");
        assert_eq!(mapping["b"]["bookmark_regex"], "b");
        assert_eq!(
            layered
                .provenance
                .source(&path("x_repo_sync_source_mapping.mapping.a.bookmark_regex")),
            &ConfigSource::Preset("git".to_owned())
        );
        assert_eq!(
            layered
                .provenance
                .source(&path("x_repo_sync_source_mapping.mapping.b.bookmark_regex")),
            &ConfigSource::Spec
        );
    }

    #[mononoke::test]
    fn test_provenance() {
        let provenance = layered().provenance;
        let preset = ConfigSource::Preset("git".to_owned());

        assert_eq!(provenance.source(&path("storage_config")), &preset);
        assert_eq!(
            provenance.source(&path("phabricator_callsign")),
            &ConfigSource::Spec
        );
        assert_eq!(provenance.source(&path("pushrebase.rewritedates")), &preset);
        assert_eq!(
            provenance.source(&path("pushrebase.casefolding_check")),
            &ConfigSource::Spec
        );
        assert_eq!(
            provenance.source(&path("derived_data_config.preset")),
            &ConfigSource::Spec,
            "a value replaced whole comes from the layer that replaced it"
        );
        assert_eq!(provenance.source(&path("bookmarks")), &ConfigSource::Spec);
        assert_eq!(
            provenance.source(&path("infinitepush.allow_writes")),
            &ConfigSource::TierOverride("scs".to_owned())
        );
        assert_eq!(
            provenance.source(&path("infinitepush.namespace_pattern")),
            &ConfigSource::TierOverride("scs".to_owned()),
            "a tier override replaces what lower layers set inside the field"
        );
        assert_eq!(provenance.source(&path("hooks")), &ConfigSource::Default);
        let derived_data_config = path("derived_data_config");
        assert!(
            !provenance
                .iter()
                .any(|(recorded, _)| recorded.is_below(&derived_data_config)),
            "nothing below a replaced value keeps a lower layer's source"
        );
    }

    #[mononoke::test]
    fn test_path_text() {
        let dotted = ConfigPath::new().key("bookmarks").index(2).key("hooks");
        assert_eq!(dotted.to_string(), "bookmarks[2].hooks");
        for text in [
            "bookmarks[2].hooks",
            r#"bookmarks[2]["hooks"]"#,
            r#"["bookmarks"][2]["hooks"]"#,
            r#"[ "bookmarks" ][ 2 ].hooks"#,
        ] {
            assert_eq!(path(text), dotted, "{text}");
        }
        assert_eq!(
            ConfigPath::from_keys(["service_write_restrictions", "clusterops.fbservices"])
                .to_string(),
            r#"service_write_restrictions["clusterops.fbservices"]"#
        );
        // A bracketed number is a list index; a bracketed string is a key.
        assert_eq!(
            path("mapping[2]"),
            ConfigPath::new().key("mapping").index(2)
        );
        assert_eq!(
            path(r#"mapping["2"]"#),
            ConfigPath::from_keys(["mapping", "2"])
        );
        assert_eq!(
            path(r#"mapping["a\"b"]"#),
            ConfigPath::from_keys(["mapping", "a\"b"])
        );

        for key in [
            "a.b", "a[0]", "a]", r#"a"b"#, r"a\b", "a b", "", "2", "-x", "_x", "x-2", "é",
        ] {
            for with_key in [
                ConfigPath::new()
                    .key("mapping")
                    .key(key)
                    .index(0)
                    .key("name"),
                ConfigPath::new().key(key).index(0),
            ] {
                let text = with_key.to_string();
                assert_eq!(path(&text), with_key, "{text} parses back");
            }
        }

        for invalid in [
            "",
            ".a",
            "a.",
            "a..b",
            "2",
            "a.2",
            "a b",
            "a]",
            "a[]",
            "a[x]",
            "a[-1]",
            "a[01]",
            "a[1.5]",
            "a[true]",
            r#"a["b""#,
            r#"a["b"x]"#,
            "a[0",
            "a[0]b",
            r#"["a", 1]"#,
        ] {
            assert!(
                invalid.parse::<ConfigPath>().is_err(),
                "{invalid} should not parse"
            );
        }
    }

    #[mononoke::test]
    fn test_paths_in_provenance() {
        let mut provenance = Provenance::default();
        provenance.record(&path("hooks"), ConfigSource::Spec);
        provenance.record(
            &path("hooks[1]"),
            ConfigSource::Preset("standard".to_owned()),
        );
        assert_eq!(
            provenance.source(&path("hooks[0].name")),
            &ConfigSource::Spec
        );
        assert_eq!(
            provenance.source(&path("hooks[1].name")),
            &ConfigSource::Preset("standard".to_owned())
        );
        provenance.record(&path("hooks"), ConfigSource::TierOverride("scs".to_owned()));
        assert_eq!(
            provenance.source(&path("hooks[1].name")),
            &ConfigSource::TierOverride("scs".to_owned()),
            "recording a value clears what was recorded inside it"
        );

        // Keys with any characters find their ancestors.
        let mapping = path("mapping");
        let quoted = mapping.clone().key(r#"a"b.c[0]"#);
        provenance.record(&mapping, ConfigSource::Spec);
        provenance.record(&quoted, ConfigSource::TierOverride("scs".to_owned()));
        assert_eq!(
            provenance.source(&quoted.clone().key("bookmark_regex")),
            &ConfigSource::TierOverride("scs".to_owned())
        );
        assert_eq!(
            provenance.source(&mapping.clone().key("a").key("bookmark_regex")),
            &ConfigSource::Spec
        );
        assert!(quoted.is_below(&mapping));
        assert!(!mapping.is_below(&mapping));
        assert_eq!(
            path("repo_config").join(&path("hooks[1].name")),
            path("repo_config.hooks[1].name")
        );
    }
}
