/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use mononoke_macros::mononoke;
use repos::RawRepoConfig;
use repos::RawShardedService;
use repos::RawShardingModeConfig;
use repos::RawWalkerConfig;
use repos::ShardingRegions;

use super::*;

#[mononoke::test]
fn config_path_uses_sha256_hash_dir() {
    // Must match the actual on-disk file: repos/git/04/aosp_..._wasp_proc.cconf
    let path = make_repo_spec_config_path("aosp/platform/vendor/qcom/wasp_proc", RepoSpecDir::Git);
    assert_eq!(
        path, "scm/mononoke/repos/git/04/aosp_platform_vendor_qcom_wasp_proc",
        "hash_dir for aosp/platform/vendor/qcom/wasp_proc must be 04 to match production file"
    );
}

#[mononoke::test]
fn config_path_for_osmeta_matches_production() {
    // Must match the actual on-disk file: repos/git/07/osmeta_external_androidx-media.cconf
    let path = make_repo_spec_config_path("osmeta/external/androidx-media", RepoSpecDir::Git);
    assert_eq!(
        path, "scm/mononoke/repos/git/07/osmeta_external_androidx-media",
        "hash_dir for osmeta/external/androidx-media must be 07 to match production file"
    );
}

#[mononoke::test]
fn config_path_for_hg_repo_matches_production() {
    // Must match the actual on-disk file: repos/hg/e0/scs-configerator_test.cconf.
    // Built under RepoSpecDir::Git this would resolve to repos/git/e0/..., which
    // does not exist — the bug this parameter exists to prevent.
    let path = make_repo_spec_config_path("scs-configerator_test", RepoSpecDir::Hg);
    assert_eq!(
        path, "scm/mononoke/repos/hg/e0/scs-configerator_test",
        "hash_dir for scs-configerator_test must be e0 to match production file"
    );
}

#[mononoke::test]
fn hg_and_git_differ_only_in_directory_segment() {
    // The sharding scheme is identical across both trees; only the
    // git/hg segment changes. Verified against all 10k production repos.
    let git = make_repo_spec_config_path("chromium_test", RepoSpecDir::Git);
    let hg = make_repo_spec_config_path("chromium_test", RepoSpecDir::Hg);
    assert_eq!(git, "scm/mononoke/repos/git/d8/chromium_test");
    assert_eq!(hg, "scm/mononoke/repos/hg/d8/chromium_test");
}

#[mononoke::test]
fn dir_for_scheme_truth_table() {
    assert_eq!(
        RepoSpecDir::for_scheme(RawCommitIdentityScheme::GIT).unwrap(),
        RepoSpecDir::Git
    );
    assert_eq!(
        RepoSpecDir::for_scheme(RawCommitIdentityScheme::HG).unwrap(),
        RepoSpecDir::Hg
    );
    for bad in [
        RawCommitIdentityScheme::UNKNOWN,
        RawCommitIdentityScheme::BONSAI,
    ] {
        let err = RepoSpecDir::for_scheme(bad).unwrap_err().to_string();
        assert!(err.contains(&format!("{bad:?}")), "{err}");
    }
}

#[mononoke::test]
fn file_path_wraps_with_source_and_cconf() {
    let path = make_repo_spec_file_path("manus/next-agent-webapp", RepoSpecDir::Git);
    assert!(path.starts_with("source/scm/mononoke/repos/git/"));
    assert!(path.ends_with("/manus_next-agent-webapp.cconf"));
}

#[mononoke::test]
fn file_path_for_hg_repo_uses_hg_directory() {
    let path = make_repo_spec_file_path("hyper_repo_test", RepoSpecDir::Hg);
    assert_eq!(
        path,
        "source/scm/mononoke/repos/hg/24/hyper_repo_test.cconf"
    );
}

#[mononoke::test]
fn default_git_repo_spec_path_is_a_template_not_a_repo() {
    // The template lives under repos/common/ and must never be indexed or
    // served, so it must not resolve into either per-repo tree.
    assert!(DEFAULT_GIT_REPO_SPEC_PATH.starts_with("scm/mononoke/repos/common/"));
    assert!(!DEFAULT_GIT_REPO_SPEC_PATH.contains("/repos/git/"));
    assert!(!DEFAULT_GIT_REPO_SPEC_PATH.contains("/repos/hg/"));
}

#[mononoke::test]
fn default_git_repo_spec_file_path_is_the_source_cconf() {
    assert_eq!(
        default_git_repo_spec_file_path(),
        "source/scm/mononoke/repos/common/default_git_repo_spec.cconf"
    );
}

fn base() -> Vec<String> {
    ["gitimport", "gitimport_content", "scs", "backfill_worker"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn with_multi_repo_land(mut tiers: Vec<String>) -> Vec<String> {
    tiers.push("aosp_multi_repo_land".to_string());
    tiers
}

#[mononoke::test]
fn tier_list_diverted_repo_includes_multi_repo_land() {
    assert_eq!(
        tier_list_for_repo_spec(&base(), true),
        with_multi_repo_land(base())
    );
}

#[mononoke::test]
fn tier_list_undiverted_repo_is_the_base_list() {
    assert_eq!(tier_list_for_repo_spec(&base(), false), base());
}

#[mononoke::test]
fn tier_list_never_drops_a_base_entry_and_never_duplicates() {
    let with_tier = with_multi_repo_land(base());
    for diverted in [true, false] {
        // Already present: the output is the input exactly, no reorder, no
        // append, no removal.
        assert_eq!(
            tier_list_for_repo_spec(&with_tier, diverted),
            with_tier,
            "diverted: {diverted}"
        );
    }
}

#[mononoke::test]
fn python_bool_formatting() {
    assert_eq!(format_python_bool(true), "True");
    assert_eq!(format_python_bool(false), "False");
}

#[mononoke::test]
fn python_list_quotes_each_item() {
    let items: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
    assert_eq!(format_python_list(&items), r#"["a", "b", "c"]"#);
    assert_eq!(format_python_list(&[]), "[]");
}

#[mononoke::test]
fn python_list_quotes_and_escapes_each_string() {
    let items = vec!["a".to_string(), "b\"c".to_string()];
    assert_eq!(format_python_list(&items), r#"["a", "b\"c"]"#);
}

#[mononoke::test]
fn commit_identity_scheme_formats_symbolically() {
    assert_eq!(
        format_commit_identity_scheme_python(RawCommitIdentityScheme::GIT).unwrap(),
        "RawCommitIdentityScheme.GIT"
    );
    assert_eq!(
        format_commit_identity_scheme_python(RawCommitIdentityScheme::HG).unwrap(),
        "RawCommitIdentityScheme.HG"
    );
    assert!(format_commit_identity_scheme_python(RawCommitIdentityScheme::UNKNOWN).is_err());
    // No _IDENTITY_SUBDIR entry in generate_repo_index.py.
    assert!(format_commit_identity_scheme_python(RawCommitIdentityScheme::BONSAI).is_err());
}

#[mononoke::test]
fn python_string_escape_handles_quote_and_backslash() {
    // Backslash must be escaped first so the escaped quote's leading
    // backslash isn't itself escaped.
    assert_eq!(escape_python_string(r#"a"b"#), r#"a\"b"#);
    assert_eq!(escape_python_string(r#"a\b"#), r#"a\\b"#);
    assert_eq!(escape_python_string(r#"a\"b"#), r#"a\\\"b"#);
}

/// Test-only stand-in for an entry that would normally come from a spec;
/// the production constructor is [`RepoIndexEntry::from_repo_spec`].
fn plain_entry(
    config_path: &str,
    repo_id: i32,
    tiers: &[&str],
    hipster_acl: &str,
) -> RepoIndexEntry {
    RepoIndexEntry {
        config_path: config_path.to_string(),
        repo_id,
        tiers: tiers.iter().map(|s| s.to_string()).collect(),
        is_deep_sharded: true,
        t_shirt_size: TShirtSize::SMALL,
        default_commit_identity_scheme: RawCommitIdentityScheme::GIT,
        hipster_acl: hipster_acl.to_string(),
        enabled: true,
        readonly: false,
        enable_git_bundle_uri: None,
        walker_scrub_enabled: None,
        walker_validate_enabled: None,
        storage_config_key: None,
    }
}

#[mononoke::test]
fn append_to_repo_index_preserves_trailing_brace() {
    let current = "REPOS = {\n    \"existing\": {\"repo_id\": 1},\n}\n";
    let entry = plain_entry(
        "scm/mononoke/repos/git/aa/new_repo",
        42,
        &["scs", "gitimport"],
        "repos/git/new/repo",
    );
    let updated = append_to_repo_index(current, &[("new/repo".to_string(), entry)]).unwrap();
    assert!(updated.ends_with("\n}\n"), "must end with closing brace");
    assert!(
        updated.contains("\"new/repo\""),
        "must contain new entry key"
    );
    assert!(updated.contains("\"repo_id\": 42"));
    assert!(
        updated.contains("\"existing\""),
        "must preserve existing entry"
    );
}

#[mononoke::test]
fn append_to_repo_index_emits_bundle_uri_when_set() {
    let current = "REPOS = {\n}\n";
    let entry = RepoIndexEntry {
        enable_git_bundle_uri: Some(false),
        ..plain_entry("scm/mononoke/repos/git/aa/r", 1, &["scs"], "a")
    };
    let updated = append_to_repo_index(current, &[("r".to_string(), entry)]).unwrap();
    assert!(updated.contains("\"enable_git_bundle_uri\": False"));
}

#[mononoke::test]
fn append_to_repo_index_omits_bundle_uri_when_none() {
    let current = "REPOS = {\n}\n";
    let entry = plain_entry("scm/mononoke/repos/git/aa/r", 1, &["scs"], "a");
    let updated = append_to_repo_index(current, &[("r".to_string(), entry)]).unwrap();
    assert!(!updated.contains("enable_git_bundle_uri"));
}

#[mononoke::test]
fn append_to_repo_index_emits_the_requested_readonly() {
    let entry_with = |readonly| RepoIndexEntry {
        readonly,
        ..plain_entry("scm/mononoke/repos/git/aa/r", 1, &["scs"], "a")
    };

    let readonly =
        append_to_repo_index("REPOS = {\n}\n", &[("r".to_string(), entry_with(true))]).unwrap();
    assert!(
        readonly.contains("\"readonly\": True"),
        "read-only repo must be read-only in the index: {readonly}"
    );

    let writable =
        append_to_repo_index("REPOS = {\n}\n", &[("r".to_string(), entry_with(false))]).unwrap();
    assert!(
        writable.contains("\"readonly\": False"),
        "the default stays writable: {writable}"
    );
}

#[mononoke::test]
fn append_to_repo_index_rejects_malformed_input() {
    // No `\n}` closing brace
    let result = append_to_repo_index("not a dict", &[]);
    assert!(result.is_err());
}

fn spec_with(
    name: &str,
    scheme: RawCommitIdentityScheme,
    status: &[(RawShardedService, bool)],
    walker: Option<(bool, bool)>,
    storage: Option<&str>,
) -> RepoSpec {
    let cfg = RawRepoConfig {
        storage_config: storage.map(str::to_string),
        deep_sharding_config: (!status.is_empty()).then(|| RawShardingModeConfig {
            status: status.iter().cloned().collect(),
        }),
        walker_config: walker.map(|(scrub, validate)| RawWalkerConfig {
            scrub_enabled: scrub,
            validate_enabled: validate,
            params: None,
        }),
        ..Default::default()
    };
    spec_with_config(name, scheme, Some(cfg))
}

fn spec_with_config(
    name: &str,
    scheme: RawCommitIdentityScheme,
    cfg: Option<RawRepoConfig>,
) -> RepoSpec {
    RepoSpec {
        repo_id: 4242,
        repo_name: name.to_string(),
        hipster_acl: "repos/git/org".to_string(),
        enabled: false,
        readonly: true,
        default_commit_identity_scheme: scheme,
        enable_git_bundle_uri: Some(true),
        tiers: vec!["scs".to_string(), "backfill_worker".to_string()],
        t_shirt_size: TShirtSize::LARGE,
        sharding_regions: ShardingRegions::BGM_ONLY_REGIONS,
        repo_config: cfg,
        tier_overrides: None,
        ..Default::default()
    }
}

#[mononoke::test]
fn from_repo_spec_copies_identity_fields_and_derives_path() {
    let spec = spec_with(
        "org/repo",
        RawCommitIdentityScheme::GIT,
        &[],
        None,
        Some("s"),
    );
    let e = RepoIndexEntry::from_repo_spec(&spec).unwrap();
    assert_eq!(
        e.config_path,
        make_repo_spec_config_path("org/repo", RepoSpecDir::Git)
    );
    assert_eq!(e.repo_id, 4242);
    assert_eq!(e.tiers, spec.tiers);
    assert_eq!(e.t_shirt_size, TShirtSize::LARGE);
    assert_eq!(
        e.default_commit_identity_scheme,
        RawCommitIdentityScheme::GIT
    );
    assert_eq!(e.hipster_acl, "repos/git/org");
    assert!(!e.enabled);
    assert!(e.readonly);
    assert_eq!(e.enable_git_bundle_uri, Some(true));
}

#[mononoke::test]
fn from_repo_spec_hg_scheme_uses_hg_dir_and_unknown_scheme_errors() {
    let hg = spec_with("fbsource", RawCommitIdentityScheme::HG, &[], None, None);
    assert_eq!(
        RepoIndexEntry::from_repo_spec(&hg).unwrap().config_path,
        make_repo_spec_config_path("fbsource", RepoSpecDir::Hg)
    );
    let bad = spec_with("x", RawCommitIdentityScheme::UNKNOWN, &[], None, None);
    assert!(RepoIndexEntry::from_repo_spec(&bad).is_err());
}

#[mononoke::test]
fn from_repo_spec_is_deep_sharded_iff_any_status_true() {
    let t = spec_with(
        "a",
        RawCommitIdentityScheme::GIT,
        &[
            (RawShardedService::SOURCE_CONTROL_SERVICE, false),
            (RawShardedService::EDEN_API, true),
        ],
        None,
        None,
    );
    assert!(RepoIndexEntry::from_repo_spec(&t).unwrap().is_deep_sharded);
    let f = spec_with(
        "b",
        RawCommitIdentityScheme::GIT,
        &[(RawShardedService::SOURCE_CONTROL_SERVICE, false)],
        None,
        None,
    );
    assert!(!RepoIndexEntry::from_repo_spec(&f).unwrap().is_deep_sharded);
    let none = spec_with("c", RawCommitIdentityScheme::GIT, &[], None, None);
    assert!(
        !RepoIndexEntry::from_repo_spec(&none)
            .unwrap()
            .is_deep_sharded
    );
}

#[mononoke::test]
fn from_repo_spec_walker_trio_present_iff_scrub_or_validate() {
    for (walker, storage, expect) in [
        (
            Some((true, false)),
            Some("s"),
            (Some(true), Some(false), Some("s".to_string())),
        ),
        (
            Some((false, true)),
            Some("s"),
            (Some(false), Some(true), Some("s".to_string())),
        ),
        // Scrub on but no storage named: the flags are still written,
        // the key is simply absent (same as the Python extractor).
        (Some((true, false)), None, (Some(true), Some(false), None)),
        (Some((false, false)), Some("s"), (None, None, None)),
        (None, Some("s"), (None, None, None)),
    ] {
        let spec = spec_with("w", RawCommitIdentityScheme::GIT, &[], walker, storage);
        let e = RepoIndexEntry::from_repo_spec(&spec).unwrap();
        assert_eq!(
            (
                e.walker_scrub_enabled,
                e.walker_validate_enabled,
                e.storage_config_key
            ),
            expect,
            "walker={walker:?} storage={storage:?}"
        );
    }
}

#[mononoke::test]
fn from_repo_spec_without_repo_config_is_unsharded_with_no_walker_trio() {
    let spec = spec_with_config("bare", RawCommitIdentityScheme::GIT, None);
    let e = RepoIndexEntry::from_repo_spec(&spec).unwrap();
    assert!(!e.is_deep_sharded);
    assert_eq!(
        (
            e.walker_scrub_enabled,
            e.walker_validate_enabled,
            e.storage_config_key
        ),
        (None, None, None)
    );
}

#[mononoke::test]
fn append_emits_scheme_enabled_and_walker_trio_in_python_order() {
    let spec = spec_with(
        "org/repo",
        RawCommitIdentityScheme::HG,
        &[(RawShardedService::EDEN_API, true)],
        Some((true, true)),
        Some("manifold_x"),
    );
    let e = RepoIndexEntry::from_repo_spec(&spec).unwrap();
    let out = append_to_repo_index("REPO_INDEX = {\n}\n", &[("org/repo".to_string(), e)]).unwrap();
    let path = make_repo_spec_config_path("org/repo", RepoSpecDir::Hg);
    let expected = format!(
        r#"REPO_INDEX = {{
    "org/repo": {{
        "config_path": "{path}",
        "repo_id": 4242,
        "tiers": ["scs", "backfill_worker"],
        "is_deep_sharded": True,
        "t_shirt_size": TShirtSize.LARGE,
        "default_commit_identity_scheme": RawCommitIdentityScheme.HG,
        "hipster_acl": "repos/git/org",
        "enabled": False,
        "readonly": True,
        "enable_git_bundle_uri": True,
        "walker_scrub_enabled": True,
        "walker_validate_enabled": True,
        "storage_config_key": "manifold_x",
    }},
}}
"#
    );
    assert_eq!(out, expected);
}

#[mononoke::test]
fn append_omits_walker_trio_when_absent() {
    let spec = spec_with(
        "org/repo",
        RawCommitIdentityScheme::GIT,
        &[],
        None,
        Some("s"),
    );
    let e = RepoIndexEntry::from_repo_spec(&spec).unwrap();
    let out = append_to_repo_index("REPO_INDEX = {\n}\n", &[("org/repo".to_string(), e)]).unwrap();
    assert!(!out.contains("walker_"));
    assert!(!out.contains("storage_config_key"));
}

/// Mirrors test_data/fixture.cconf field for field. Update both together.
fn spec_with_fixture_values() -> RepoSpec {
    let cfg = RawRepoConfig {
        storage_config: Some("manifold_multiplex_write_packed_wal".to_string()),
        deep_sharding_config: Some(RawShardingModeConfig {
            status: [(RawShardedService::SOURCE_CONTROL_SERVICE, true)]
                .into_iter()
                .collect(),
        }),
        walker_config: Some(RawWalkerConfig {
            scrub_enabled: true,
            validate_enabled: true,
            params: None,
        }),
        ..Default::default()
    };
    RepoSpec {
        repo_id: 99999,
        repo_name: "golden/fixture".to_string(),
        hipster_acl: "repos/git/golden".to_string(),
        enabled: false,
        readonly: true,
        default_commit_identity_scheme: RawCommitIdentityScheme::GIT,
        enable_git_bundle_uri: Some(true),
        tiers: vec!["scs".to_string(), "backfill_worker".to_string()],
        t_shirt_size: TShirtSize::LARGE,
        sharding_regions: ShardingRegions::BGM_ONLY_REGIONS,
        repo_config: Some(cfg),
        tier_overrides: None,
        ..Default::default()
    }
}

/// Lines strictly between `REPO_INDEX = {` and the final `}`.
fn index_body(s: &str) -> Vec<String> {
    let start = s.find("REPO_INDEX = {").expect("no REPO_INDEX") + "REPO_INDEX = {".len();
    let end = s.rfind('}').expect("no closing brace");
    s[start..end]
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

/// The golden was produced by generate_repo_index.py from
/// test_data/fixture.cconf; the Rust side must extract the same entry
/// from the same values, or `conf build`'s regeneration would rewrite
/// what create_repos appended.
#[mononoke::test]
fn index_entry_matches_python_extractor_golden() {
    let e = RepoIndexEntry::from_repo_spec(&spec_with_fixture_values()).unwrap();
    let rust =
        append_to_repo_index("REPO_INDEX = {\n}\n", &[("golden/fixture".to_string(), e)]).unwrap();
    let golden = include_str!("../test_data/fixture.index.txt");
    // The Python regen copies Configo's int literals; Rust writes symbolic names.
    let normalized = rust
        .replace("TShirtSize.LARGE", "2")
        .replace("RawCommitIdentityScheme.GIT", "3");
    assert_eq!(index_body(&normalized), index_body(golden));
    assert_eq!(index_body(golden).len(), 15, "one entry with all keys");
}
