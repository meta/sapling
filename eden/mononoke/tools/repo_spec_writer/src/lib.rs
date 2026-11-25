/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Reusable helpers for writing Mononoke `RepoSpec` `.cconf` files via Configo.
//!
//! The SCS `create_repos` API produces `RepoSpec` files at canonical paths in
//! configerator. The path computation, Python-literal formatters, and
//! `repo_index.cinc` updater live here.

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use repos::RawCommitIdentityScheme;
use repos::RepoSpec;
use repos::TShirtSize;
use sha2::Digest;
use sha2::Sha256;

/// Which per-repo directory a RepoSpec `.cconf` lives under. Configerator splits
/// them by commit identity scheme — `repos/git/` vs `repos/hg/` — while using the
/// identical `sha256(repo_name)` sharding within each. Callers must say which,
/// because the two trees are disjoint: looking an hg repo up under `repos/git/`
/// finds nothing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RepoSpecDir {
    Git,
    Hg,
}

impl RepoSpecDir {
    fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Hg => "hg",
        }
    }

    /// The directory a spec with this commit identity scheme lives under. The
    /// single source for the scheme -> directory rule: both the file path a
    /// writer chooses and the `config_path` its index entry records derive
    /// from here, so the two cannot disagree. Only GIT and HG have a tree
    /// (`_IDENTITY_SUBDIR` in generate_repo_index.py); any other scheme is an
    /// error naming the scheme.
    pub fn for_scheme(scheme: RawCommitIdentityScheme) -> Result<Self> {
        match scheme {
            RawCommitIdentityScheme::GIT => Ok(Self::Git),
            RawCommitIdentityScheme::HG => Ok(Self::Hg),
            other => Err(anyhow!(
                "unsupported default_commit_identity_scheme {other:?}: only GIT and HG have a per-repo directory"
            )),
        }
    }
}

/// Returns the configerator path for a RepoSpec file (no source/ prefix, no .cconf extension).
/// E.g., "scm/mononoke/repos/git/a3/org_repo" for repo name "org/repo" under [`RepoSpecDir::Git`].
/// Must match repo_spec_config_path() in generate_repo_index.py and
/// repo_spec_relative_path() in migrate_qrd_to_repo_spec.py.
pub fn make_repo_spec_config_path(repo_name: &str, dir: RepoSpecDir) -> String {
    let hash = Sha256::digest(repo_name.as_bytes());
    let hash_dir = format!("{:02x}", hash[0]);
    format!(
        "scm/mononoke/repos/{}/{}/{}",
        dir.as_str(),
        hash_dir,
        repo_name.replace('/', "_")
    )
}

/// Generates the file path for a RepoSpec file.
/// Path format: source/scm/mononoke/repos/{git|hg}/{hash_dir}/{repo_name_escaped}.cconf
pub fn make_repo_spec_file_path(repo_name: &str, dir: RepoSpecDir) -> String {
    format!(
        "source/{}.cconf",
        make_repo_spec_config_path(repo_name, dir)
    )
}

/// Configerator config path of the `RepoSpec` template for new Git repos:
/// configerator/source/scm/mononoke/repos/common/default_git_repo_spec.cconf.
/// Read by SCS `create_repos` (embeds the template into every new repo) and
/// by `mononoke_admin git-source-of-truth cleanup-stale-reserved` (reads its
/// storage name).
pub const DEFAULT_GIT_REPO_SPEC_PATH: &str = "scm/mononoke/repos/common/default_git_repo_spec";

/// Configerator source path of the same file, for Configo transactions that
/// edit it. Same formula as [`make_repo_spec_file_path`].
pub fn default_git_repo_spec_file_path() -> String {
    format!("source/{DEFAULT_GIT_REPO_SPEC_PATH}.cconf")
}

/// The config tier multi_repo_land_service loads its repos from.
const MULTI_REPO_LAND_TIER: &str = "aosp_multi_repo_land";

/// Apply the one tier rule that stays in Rust on top of the template's base
/// tier list: a repo whose pushes the git server hands to Multi-Repo Land
/// (`diverted`, which is `CommonConfig::diverts_to_multi_repo_land` for the
/// repo's name) is also served by `aosp_multi_repo_land`, because
/// multi_repo_land_service can only land a repo it serves. Purely additive;
/// never removes or reorders a base entry; never adds a second
/// `aosp_multi_repo_land`.
///
/// The base list comes from the template and must include `backfill_worker`,
/// or on-demand backfill loads silently break; nothing in Rust enforces its
/// presence (the template is reviewed in configerator; `repos/repo_spec.ctest`
/// checks its tiers resolve but does not pin `backfill_worker` specifically):
/// mononoke_backfill_worker
/// (`fbcode/eden/mononoke/backfill_worker`) accepts ALL repos via
/// `QueueRepoFilter::Except(vec![])` and loads them on-demand when a backfill
/// request arrives. Without this entry the per-repo manifest path doesn't
/// surface the repo, the on-demand load fails, and the worker silently drops
/// backfills for it (the legacy QRD path used to populate this transitively
/// via the scs tier composer; the RepoSpec path requires explicit listing).
pub fn tier_list_for_repo_spec(base: &[String], diverted: bool) -> Vec<String> {
    let mut tiers: Vec<String> = base.to_vec();
    if diverted && !tiers.iter().any(|t| t == MULTI_REPO_LAND_TIER) {
        tiers.push(MULTI_REPO_LAND_TIER.to_string());
    }
    tiers
}

/// One entry in `repo_index.cinc`. Mirrors the Python dict shape that
/// `generate_repo_index.py` writes; field naming matches the dict keys
/// emitted by [`append_to_repo_index`].
///
/// `non_exhaustive` on purpose: outside this crate an entry can only be
/// built via [`RepoIndexEntry::from_repo_spec`]. Hand-built entries are how
/// repo_index.cinc drifted from the specs before (readonly on 770 specs,
/// hipster_acl on one).
#[non_exhaustive]
pub struct RepoIndexEntry {
    pub config_path: String,
    pub repo_id: i32,
    pub tiers: Vec<String>,
    pub is_deep_sharded: bool,
    pub t_shirt_size: TShirtSize,
    pub default_commit_identity_scheme: RawCommitIdentityScheme,
    pub hipster_acl: String,
    pub enabled: bool,
    pub readonly: bool,
    pub enable_git_bundle_uri: Option<bool>,
    /// Sparse walker/storage keys consumed by detectors/walker_scrub.detector.cconf.
    /// All three are `None` unless scrub or validate is enabled, mirroring
    /// `extract_walker_and_storage` in generate_repo_index.py.
    pub walker_scrub_enabled: Option<bool>,
    pub walker_validate_enabled: Option<bool>,
    pub storage_config_key: Option<String>,
}

impl RepoIndexEntry {
    /// The index entry for `spec`, derived from the spec alone. This is the
    /// invariant that keeps repo_index.cinc and the per-repo .cconf in step:
    /// everything written here must be semantically what
    /// generate_repo_index.py extracts from the same file: same fields, same
    /// values. Surface form of enums differs on purpose (symbolic
    /// `TShirtSize.X` / `RawCommitIdentityScheme.X` here; Configo-emitted files
    /// carry the int literal) and the two compare equal in configerator's
    /// thrift Python; `repo_spec_processing.cinc` reads them with `==`.
    /// Nothing may come from the creation request or a constant.
    pub fn from_repo_spec(spec: &RepoSpec) -> Result<Self> {
        let dir = RepoSpecDir::for_scheme(spec.default_commit_identity_scheme)
            .with_context(|| format!("repo {}", spec.repo_name))?;
        let cfg = spec.repo_config.as_ref();
        let is_deep_sharded = cfg
            .and_then(|c| c.deep_sharding_config.as_ref())
            .is_some_and(|s| s.status.values().any(|v| *v));
        let (walker_scrub_enabled, walker_validate_enabled, storage_config_key) =
            match cfg.and_then(|c| c.walker_config.as_ref()) {
                Some(w) if w.scrub_enabled || w.validate_enabled => (
                    Some(w.scrub_enabled),
                    Some(w.validate_enabled),
                    cfg.and_then(|c| c.storage_config.clone()),
                ),
                _ => (None, None, None),
            };
        Ok(Self {
            config_path: make_repo_spec_config_path(&spec.repo_name, dir),
            repo_id: spec.repo_id,
            tiers: spec.tiers.clone(),
            is_deep_sharded,
            t_shirt_size: spec.t_shirt_size,
            default_commit_identity_scheme: spec.default_commit_identity_scheme,
            hipster_acl: spec.hipster_acl.clone(),
            enabled: spec.enabled,
            readonly: spec.readonly,
            enable_git_bundle_uri: spec.enable_git_bundle_uri,
            walker_scrub_enabled,
            walker_validate_enabled,
            storage_config_key,
        })
    }
}

pub fn format_python_bool(val: bool) -> &'static str {
    if val { "True" } else { "False" }
}

pub fn format_python_list(items: &[String]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|s| format!("\"{}\"", escape_python_string(s)))
        .collect();
    format!("[{}]", quoted.join(", "))
}

/// Only GIT and HG: these are the schemes `_IDENTITY_SUBDIR` in
/// generate_repo_index.py maps to a directory. BONSAI has no entry there, so an
/// index entry carrying it could never be regenerated; refuse to write one.
pub fn format_commit_identity_scheme_python(s: RawCommitIdentityScheme) -> Result<&'static str> {
    match s {
        RawCommitIdentityScheme::HG => Ok("RawCommitIdentityScheme.HG"),
        RawCommitIdentityScheme::GIT => Ok("RawCommitIdentityScheme.GIT"),
        other => Err(anyhow!(
            "unexpected RawCommitIdentityScheme variant: {other:?}"
        )),
    }
}

pub fn format_tshirt_size_python(size: TShirtSize) -> Result<&'static str> {
    match size {
        TShirtSize::SMALL => Ok("TShirtSize.SMALL"),
        TShirtSize::MEDIUM => Ok("TShirtSize.MEDIUM"),
        TShirtSize::LARGE => Ok("TShirtSize.LARGE"),
        TShirtSize::EXTRA_LARGE => Ok("TShirtSize.EXTRA_LARGE"),
        TShirtSize::EXTRA_EXTRA_LARGE => Ok("TShirtSize.EXTRA_EXTRA_LARGE"),
        TShirtSize::HUGE => Ok("TShirtSize.HUGE"),
        other => Err(anyhow!("unexpected TShirtSize variant: {other:?}")),
    }
}

/// Escape a string for embedding in a Python string literal (double-quoted).
/// Matches the escaping in generate_repo_index.py's ast_value_to_python_literal().
pub fn escape_python_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Insert new entries into a `repo_index.cinc` source string, preserving the
/// closing `}\n` so the result remains valid Python syntax.
///
/// `current_content` must contain a top-level dict literal whose closing brace
/// appears as `\n}` (matches the format `generate_repo_index.py` writes).
pub fn append_to_repo_index(
    current_content: &str,
    new_entries: &[(String, RepoIndexEntry)],
) -> Result<String> {
    let insert_pos = current_content
        .rfind("\n}")
        .ok_or_else(|| anyhow!("malformed repo_index.cinc: no closing brace"))?;

    let mut result = current_content[..insert_pos].to_string();

    for (repo_name, entry) in new_entries {
        let t_shirt_size_str = format_tshirt_size_python(entry.t_shirt_size)
            .with_context(|| format!("formatting t_shirt_size for repo {repo_name}"))?;
        let scheme_str = format_commit_identity_scheme_python(entry.default_commit_identity_scheme)
            .with_context(|| {
                format!("formatting default_commit_identity_scheme for repo {repo_name}")
            })?;
        let mut entry_str = format!(
            r#"
    "{}": {{
        "config_path": "{}",
        "repo_id": {},
        "tiers": {},
        "is_deep_sharded": {},
        "t_shirt_size": {},
        "default_commit_identity_scheme": {},
        "hipster_acl": "{}",
        "enabled": {},
        "readonly": {},"#,
            escape_python_string(repo_name),
            escape_python_string(&entry.config_path),
            entry.repo_id,
            format_python_list(&entry.tiers),
            format_python_bool(entry.is_deep_sharded),
            t_shirt_size_str,
            scheme_str,
            escape_python_string(&entry.hipster_acl),
            format_python_bool(entry.enabled),
            format_python_bool(entry.readonly),
        );
        if let Some(bundle_uri) = entry.enable_git_bundle_uri {
            entry_str.push_str(&format!(
                "\n        \"enable_git_bundle_uri\": {},",
                format_python_bool(bundle_uri)
            ));
        }
        // Sparse, same as generate_index_content in generate_repo_index.py:
        // a False scrub/validate flag is omitted, not written as False.
        if entry.walker_scrub_enabled == Some(true) {
            entry_str.push_str("\n        \"walker_scrub_enabled\": True,");
        }
        if entry.walker_validate_enabled == Some(true) {
            entry_str.push_str("\n        \"walker_validate_enabled\": True,");
        }
        if let Some(key) = &entry.storage_config_key {
            entry_str.push_str(&format!(
                "\n        \"storage_config_key\": \"{}\",",
                escape_python_string(key)
            ));
        }
        entry_str.push_str("\n    },");
        result.push_str(&entry_str);
    }

    result.push_str("\n}\n");
    Ok(result)
}

#[cfg(test)]
mod tests;
