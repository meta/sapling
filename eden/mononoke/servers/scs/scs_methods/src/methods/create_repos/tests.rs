/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use fbinit::FacebookInit;
use git_symbolic_refs::SqlGitSymbolicRefsBuilder;
use mononoke_macros::mononoke;
use repo_spec_writer::make_repo_spec_config_path;
use repos::RawRepoConfig;
use repos::RawShardedService;
use repos::RawShardingModeConfig;
use repos::ShardingRegions;
use sql_construct::SqlConstruct;

use super::*;

fn request_error(err: scs_errors::ServiceError) -> String {
    match err {
        scs_errors::ServiceError::Request(e) => e.reason,
        other => panic!("expected a request error, got {other:?}"),
    }
}

fn storage_request(repo_name: &str, storage_config: Option<&str>) -> thrift::RepoCreationRequest {
    thrift::RepoCreationRequest {
        repo_name: repo_name.to_string(),
        size_bucket: RepoSizeBucket::SMALL,
        storage_config: storage_config.map(str::to_string),
        ..Default::default()
    }
}

// The engine is decided by the org: `<slug>/...` goes on `org_<slug>` when
// the storage map has it, on the template otherwise, whoever the caller is.
#[mononoke::test]
fn test_storage_is_derived_from_the_org() {
    let configs = storage_configs(&[("template", "/db"), ("org_wearables", "/db")]);
    assert_eq!(
        derive_batch_storage(&[storage_request("other/a", None)], &configs).unwrap(),
        BatchStorage(None),
        "an org without an engine is on the template"
    );
    assert_eq!(
        derive_batch_storage(&[storage_request("bare", None)], &configs).unwrap(),
        BatchStorage(None),
        "a bare name has no org"
    );
    assert_eq!(
        derive_batch_storage(
            &[
                storage_request("wearables/a", None),
                storage_request("wearables/b", Some("org_wearables")),
            ],
            &configs
        )
        .unwrap(),
        BatchStorage(Some("org_wearables".to_string())),
        "the org's engine, named or not"
    );
}

#[mononoke::test]
fn test_storage_refuses_batches_spanning_engines() {
    let configs = storage_configs(&[("template", "/db"), ("org_wearables", "/db")]);
    let reason = request_error(
        derive_batch_storage(
            &[
                storage_request("wearables/a", None),
                storage_request("other/b", None),
            ],
            &configs,
        )
        .unwrap_err(),
    );
    assert!(
        reason.starts_with("STORAGE_CONFIG_REFUSED:batch"),
        "{reason}"
    );
    assert!(reason.contains("other/b (template)"), "{reason}");
}

// A named engine is an assertion: it must be what the org maps to.
#[mononoke::test]
fn test_named_storage_must_match_the_orgs_engine() {
    let configs = storage_configs(&[
        ("template", "/db"),
        ("org_wearables", "/db"),
        ("org_other", "/db"),
    ]);
    for (name, named, expect) in [
        ("wearables/a", "org_other", "'org_wearables'"),
        ("plain/a", "org_wearables", "the template engine"),
        ("wearables/a", "template", "'org_wearables'"),
        ("bare", "org_wearables", "the template engine"),
    ] {
        let reason = request_error(
            derive_batch_storage(&[storage_request(name, Some(named))], &configs).unwrap_err(),
        );
        assert!(
            reason.starts_with("STORAGE_CONFIG_REFUSED:org"),
            "{name} naming {named}: {reason}"
        );
        assert!(reason.contains(expect), "{name} naming {named}: {reason}");
    }
}

fn local_storage(path: &str) -> metaconfig_types::StorageConfig {
    metaconfig_types::StorageConfig {
        blobstore: metaconfig_types::BlobConfig::Disabled,
        metadata: metaconfig_types::MetadataDatabaseConfig::Local(
            metaconfig_types::LocalDatabaseConfig { path: path.into() },
        ),
        ephemeral_blobstore: None,
        mutable_blobstore: metaconfig_types::BlobConfig::Disabled,
    }
}

fn storage_configs(engines: &[(&str, &str)]) -> metaconfig_parser::StorageConfigs {
    metaconfig_parser::StorageConfigs {
        storage: engines
            .iter()
            .map(|(name, production)| (name.to_string(), local_storage(production)))
            .collect(),
    }
}

// The production shape: every engine of the tier is Remote and the
// production database is the one field compared.
fn remote_storage(production: &str, others: &str) -> metaconfig_types::StorageConfig {
    let db = |address: &str| metaconfig_types::RemoteDatabaseConfig {
        db_address: address.to_string(),
    };
    metaconfig_types::StorageConfig {
        blobstore: metaconfig_types::BlobConfig::Disabled,
        metadata: metaconfig_types::MetadataDatabaseConfig::Remote(
            metaconfig_types::RemoteMetadataDatabaseConfig {
                production: db(production),
                commit_graph: db(others),
                bookmarks: db(others),
                bonsai_hg_mapping: db(others),
                filenodes: metaconfig_types::ShardableRemoteDatabaseConfig::Unsharded(db(others)),
                mutation: db(others),
                sparse_profiles: db(others),
                bonsai_blob_mapping: None,
                deletion_log: None,
                commit_cloud: None,
                git_bundle_metadata: None,
                repo_metadata: None,
                restricted_paths: None,
                commit_derived_data_mapping: None,
                repo_manifest_mapping: None,
            },
        ),
        ephemeral_blobstore: None,
        mutable_blobstore: metaconfig_types::BlobConfig::Disabled,
    }
}

#[mononoke::test]
fn test_storage_config_production_check_reads_the_remote_production_db() {
    let configs = metaconfig_parser::StorageConfigs {
        storage: [
            (
                "template".to_string(),
                remote_storage("xdb.mononoke_production", "xdb.mononoke_bookmarks"),
            ),
            (
                "org_same".to_string(),
                remote_storage("xdb.mononoke_production", "xdb.org_shard"),
            ),
            (
                "org_other".to_string(),
                remote_storage("xdb.org_shard", "xdb.org_shard"),
            ),
        ]
        .into_iter()
        .collect(),
    };
    check_storage_config_production("org_same", "template", &configs).unwrap();
    let reason = request_error(
        check_storage_config_production("org_other", "template", &configs).unwrap_err(),
    );
    assert!(
        reason.starts_with("STORAGE_CONFIG_REFUSED:production"),
        "{reason}"
    );
}

#[mononoke::test]
fn test_storage_config_must_share_production() {
    let configs = storage_configs(&[
        ("template", "/db/production"),
        ("org_same", "/db/production"),
        ("org_other", "/db/elsewhere"),
    ]);
    check_storage_config_production("org_same", "template", &configs).unwrap();

    // The engine came from the loaded map a moment ago; its absence now is
    // the service's problem, not the caller's.
    let err = check_storage_config_production("org_missing", "template", &configs).unwrap_err();
    assert!(reason_of_internal(err).contains("'org_missing'"));

    let reason = request_error(
        check_storage_config_production("org_other", "template", &configs).unwrap_err(),
    );
    assert!(
        reason.starts_with("STORAGE_CONFIG_REFUSED:production"),
        "{reason}"
    );

    // A template that is not loaded is the service's problem, not the caller's.
    let err = check_storage_config_production("org_same", "gone", &configs).unwrap_err();
    assert!(reason_of_internal(err).contains("'gone'"));
}

fn reason_of_internal(err: scs_errors::ServiceError) -> String {
    reason(err)
}

#[mononoke::test]
fn test_make_repo_spec_takes_the_storage_engine_from_the_request() {
    let request = storage_request("org/my-repo", Some("org_org"));
    let spec = make_repo_spec(&(RepositoryId::new(7), request), &template()).unwrap();
    let repo_config = spec.repo_config.expect("the template carries one");
    assert_eq!(repo_config.storage_config.as_deref(), Some("org_org"));
    // Only the engine changed inside repo_config.
    assert_eq!(
        RawRepoConfig {
            storage_config: template().repo_config.unwrap().storage_config,
            ..repo_config
        },
        template().repo_config.unwrap()
    );
}

/// The template this method embeds into every new repo. Pinned here so a
/// rename in repo_spec_writer cannot silently repoint create_repos.
#[mononoke::test]
fn test_template_path_is_the_default_git_repo_spec() {
    assert_eq!(
        repo_spec_writer::DEFAULT_GIT_REPO_SPEC_PATH,
        "scm/mononoke/repos/common/default_git_repo_spec"
    );
}

#[mononoke::test]
fn index_entry_agrees_with_repo_spec() {
    for name in ["org/my-repo", "aosp/platform/vendor/foo"] {
        let request = thrift::RepoCreationRequest {
            repo_name: name.to_string(),
            size_bucket: RepoSizeBucket::LARGE,
            ..Default::default()
        };
        let spec = make_repo_spec(&(RepositoryId::new(7), request), &template()).unwrap();
        let e = RepoIndexEntry::from_repo_spec(&spec).unwrap();
        assert_eq!(
            e.config_path,
            make_repo_spec_config_path(name, RepoSpecDir::Git)
        );
        assert_eq!(e.repo_id, spec.repo_id);
        assert_eq!(e.tiers, spec.tiers);
        assert_eq!(e.enabled, spec.enabled);
        assert_eq!(e.readonly, spec.readonly);
        assert_eq!(e.t_shirt_size, spec.t_shirt_size);
        assert_eq!(e.hipster_acl, spec.hipster_acl);
        assert_eq!(e.enable_git_bundle_uri, spec.enable_git_bundle_uri);
        assert_eq!(
            e.default_commit_identity_scheme,
            spec.default_commit_identity_scheme
        );
        // The fixture's deep_sharding_config has one true status.
        assert!(e.is_deep_sharded);
    }
}

/// Stand-in for `scm/mononoke/repos/common/default_git_repo_spec`. Every
/// value is deliberately one the old hard-coded defaults never produced
/// (tier names, ALL_REGIONS, a tier override, HUGE, bundle_uri=Some),
/// so each assertion tells request-owned from template-owned and a
/// regression to a constant cannot pass by coincidence.
fn template() -> RepoSpec {
    let cfg = RawRepoConfig {
        storage_config: Some("manifold_multiplex_write_packed_wal".to_string()),
        deep_sharding_config: Some(RawShardingModeConfig {
            status: [(RawShardedService::SOURCE_CONTROL_SERVICE, true)]
                .into_iter()
                .collect(),
        }),
        ..Default::default()
    };
    RepoSpec {
        repo_id: 0,
        repo_name: "default/git-template".to_string(),
        hipster_acl: "repos/git/default".to_string(),
        enabled: true,
        readonly: false,
        default_commit_identity_scheme: RawCommitIdentityScheme::GIT,
        enable_git_bundle_uri: Some(false),
        tiers: base(),
        t_shirt_size: TShirtSize::HUGE,
        sharding_regions: ShardingRegions::ALL_REGIONS,
        repo_config: Some(cfg),
        tier_overrides: Some(
            [("tmpl_a".to_string(), RawRepoConfig::default())]
                .into_iter()
                .collect(),
        ),
        ..Default::default()
    }
}

/// The fixture template's tier list. Not the production list on purpose:
/// a test passing against these names proves the tiers came from the
/// template, not from a constant.
fn base() -> Vec<String> {
    ["tmpl_a", "tmpl_b"].iter().map(|s| s.to_string()).collect()
}

fn reason(err: scs_errors::ServiceError) -> String {
    match err {
        scs_errors::ServiceError::Internal(e) => e.reason,
        other => panic!("expected an internal error, got {other:?}"),
    }
}

#[mononoke::test]
fn test_validate_template_accepts_the_fixture() {
    validate_template(&template()).expect("the fixture must be a valid template");
    assert_eq!(
        template_storage_name(&template()).unwrap(),
        "manifold_multiplex_write_packed_wal"
    );
}

#[mononoke::test]
fn test_validate_template_rejects_unusable_templates() {
    let no_storage = {
        let mut t = template();
        t.repo_config.as_mut().unwrap().storage_config = None;
        t
    };
    let no_repo_config = RepoSpec {
        repo_config: None,
        ..template()
    };
    let hg = RepoSpec {
        default_commit_identity_scheme: RawCommitIdentityScheme::HG,
        ..template()
    };
    let no_tiers = RepoSpec {
        tiers: vec![],
        ..template()
    };
    let hg_name = format!("{:?}", RawCommitIdentityScheme::HG);
    for (what, bad, expect) in [
        ("missing storage config", no_storage, "storage config"),
        ("repo_config None", no_repo_config, "storage config"),
        ("HG scheme", hg, hg_name.as_str()),
        ("empty tiers", no_tiers, "no tiers"),
    ] {
        let msg = reason(validate_template(&bad).expect_err(what));
        assert!(
            msg.contains(DEFAULT_GIT_REPO_SPEC_PATH),
            "{what}: message must name the template: {msg}"
        );
        assert!(msg.contains(expect), "{what}: {msg}");
    }
}

#[mononoke::test]
fn test_make_repo_spec_copies_template_enabled_false() {
    let disabled = RepoSpec {
        enabled: false,
        ..template()
    };
    let request = thrift::RepoCreationRequest {
        repo_name: "org/my-repo".to_string(),
        size_bucket: RepoSizeBucket::SMALL,
        ..Default::default()
    };
    let spec = make_repo_spec(&(RepositoryId::new(1), request), &disabled).unwrap();
    assert!(!spec.enabled, "enabled is template-owned");
}

fn with_multi_repo_land(mut tiers: Vec<String>) -> Vec<String> {
    tiers.push("aosp_multi_repo_land".to_string());
    tiers
}

#[mononoke::test]
fn test_make_repo_spec_file_path_simple_name() {
    // Test asserts the git-only path. When adding HG support to create_repos, add a
    // parallel test for repos/hg/ paths.
    let path = make_repo_spec_file_path("my-repo", RepoSpecDir::Git);
    assert!(
        path.starts_with("source/scm/mononoke/repos/git/"),
        "Path should start with RepoSpec base path: {path}"
    );
    assert!(
        path.ends_with("/my-repo.cconf"),
        "Path should end with /repo-name.cconf: {path}"
    );
}

#[mononoke::test]
fn test_make_repo_spec_file_path_slash_in_name() {
    let path = make_repo_spec_file_path("org/project/repo", RepoSpecDir::Git);
    assert!(
        path.ends_with("/org_project_repo.cconf"),
        "Slashes should be replaced with underscores: {path}"
    );
}

#[mononoke::test]
fn test_make_repo_spec_file_path_no_collision_slash_vs_underscore() {
    let path1 = make_repo_spec_file_path("org/repo", RepoSpecDir::Git);
    let path2 = make_repo_spec_file_path("org_repo", RepoSpecDir::Git);
    assert_ne!(
        path1, path2,
        "Repos differing only in '/' vs '_' must produce different paths"
    );
}

#[mononoke::test]
fn test_make_repo_spec_file_path_deterministic() {
    let path1 = make_repo_spec_file_path("test/repo", RepoSpecDir::Git);
    let path2 = make_repo_spec_file_path("test/repo", RepoSpecDir::Git);
    assert_eq!(path1, path2, "Hash-based path should be deterministic");
}

#[mononoke::test]
fn test_make_repo_spec_file_path_different_repos_may_differ() {
    let path1 = make_repo_spec_file_path("repo-alpha", RepoSpecDir::Git);
    let path2 = make_repo_spec_file_path("repo-beta", RepoSpecDir::Git);
    assert_ne!(
        path1, path2,
        "Different repos should produce different paths"
    );
}

#[mononoke::test]
fn test_make_top_level_acl_name_lowercases_uppercase_org() {
    // Regression for the XF-APAC mirror sync failure on 2026-06-24:
    // before this fix, the uppercase repo name flowed through verbatim
    // to the ACL name on the Mononoke repo config, producing a
    // case-mismatch with the lowercased Hipster ACL.
    assert_eq!(
        make_top_level_acl_name_from_repo_name("XF-APAC/dreamwright-v2"),
        "repos/git/xf-apac",
    );
}

#[mononoke::test]
fn test_make_top_level_acl_name_preserves_already_lowercase_org() {
    // Existing tenants (par-msl) must keep producing the byte-equal
    // ACL name they had before this fix — otherwise their repo
    // configs would point at a different name than the live Hipster
    // entries on the next config rewrite.
    assert_eq!(
        make_top_level_acl_name_from_repo_name("par-msl/risk-test"),
        "repos/git/par-msl",
    );
}

#[mononoke::test]
fn test_make_top_level_acl_name_no_slash() {
    // Defensive: repo name without a slash falls back to using the
    // whole name as the top-level (matches the pre-fix behavior
    // shape), still lowercased.
    assert_eq!(
        make_top_level_acl_name_from_repo_name("Single-Segment"),
        "repos/git/single-segment",
    );
}

#[mononoke::test]
fn test_make_full_acl_name_lowercases() {
    // Custom-ACL path (callers with `custom_acl.is_some()`) also goes
    // through Hipster's lowercasing, so the full ACL name must be
    // lowercased end-to-end.
    assert_eq!(
        make_full_acl_name_from_repo_name("XF-APAC/Dreamwright-V2"),
        "repos/git/xf-apac/dreamwright-v2",
    );
    assert_eq!(
        make_full_acl_name_from_repo_name("par-msl/risk-test"),
        "repos/git/par-msl/risk-test",
    );
}

#[cfg(fbcode_build)]
#[mononoke::test]
fn test_initial_acl_grants_include_coding_crewmates_read() {
    // Every newly-created per-repo Git ACL must grant read to
    // AUTH_SET:coding_crewmates so all Meta engineers can clone the
    // repo. Removing this grant would silently regress the eliminate
    // -per-repo-onboarding-friction commitment made after the
    // provide_gitimport_read_access.sh backfill; grep for that script
    // name before deleting this assertion.
    let grants = initial_acl_grants(&[hipster_identity(AUTH_SET, "some_hipster_group")], &[]);
    let read = grants
        .iter()
        .find(|g| g.action == "read")
        .expect("initial_acl_grants must contain a read action");
    let has_coding_crewmates = read
        .entry_changes
        .iter()
        .any(|e| e.entry.id_type == AUTH_SET && e.entry.id_data == "coding_crewmates");
    assert!(
        has_coding_crewmates,
        "initial_acl_grants read action must grant AUTH_SET:coding_crewmates",
    );
}

#[cfg(fbcode_build)]
#[mononoke::test]
fn test_initial_acl_grants_include_intern_graphql_controller_read() {
    // Phabricator diff pages read the repo through the intern GraphQL
    // controller with no user identity attached; without this grant every
    // `jf submit` against a fresh repo fails on the metadata read.
    let grants = initial_acl_grants(&[hipster_identity(AUTH_SET, "some_hipster_group")], &[]);
    let read = grants
        .iter()
        .find(|g| g.action == "read")
        .expect("initial_acl_grants must contain a read action");
    assert!(
        read.entry_changes.iter().any(|e| {
            e.entry.id_type == INTERN_CONTROLLER
                && e.entry.id_data == "XInternGraphGraphQLController"
        }),
        "initial_acl_grants read action must grant INTERN_CONTROLLER:XInternGraphGraphQLController",
    );
}

#[cfg(fbcode_build)]
#[mononoke::test]
fn test_initial_acl_grants_take_maintainers_and_writers_from_the_request() {
    let maintainers = [
        hipster_identity(AUTH_SET, "luisa_maintainers"),
        hipster_identity(AUTH_SET, "scm"),
    ];
    let writers = [hipster_identity(ONCALL_GROUP_TYPE, "oncall_luisa")];
    let grants = initial_acl_grants(&maintainers, &writers);

    let entries = |action: &str| -> Vec<(String, String)> {
        grants
            .iter()
            .find(|g| g.action == action)
            .unwrap_or_else(|| panic!("initial_acl_grants must contain a {action} action"))
            .entry_changes
            .iter()
            .map(|e| (e.entry.id_type.clone(), e.entry.id_data.clone()))
            .collect()
    };
    assert_eq!(
        entries("maintainers"),
        vec![
            (AUTH_SET.to_string(), "luisa_maintainers".to_string()),
            (AUTH_SET.to_string(), "scm".to_string()),
        ],
        "maintainers are exactly the requested ones",
    );
    let write = entries("write");
    assert!(
        write.contains(&(ONCALL_GROUP_TYPE.to_string(), "oncall_luisa".to_string())),
        "requested writers are granted write",
    );
    assert!(
        write.contains(&(AUTH_SET.to_string(), "svnuser".to_string())),
        "standard write grants stay",
    );
    assert!(
        !entries("read")
            .iter()
            .any(|(_, data)| data == "oncall_luisa"),
        "writers are not granted read on their own",
    );
}

#[cfg(fbcode_build)]
#[mononoke::test]
fn test_to_hipster_identity_keeps_type_and_data() {
    let identity = to_hipster_identity(&thrift::AclIdentity {
        id_type: "ONCALL_GROUP".to_string(),
        id_data: "oncall_luisa".to_string(),
        ..Default::default()
    });
    assert_eq!(identity.id_type, "ONCALL_GROUP");
    assert_eq!(identity.id_data, "oncall_luisa");
}

#[cfg(fbcode_build)]
fn org_acl_request(
    custom_acl: Option<&str>,
    maintainers: Vec<thrift::AclIdentity>,
) -> thrift::RepoCreationRequest {
    thrift::RepoCreationRequest {
        repo_name: "luisa/first".to_string(),
        oncall_name: "luisa".to_string(),
        custom_acl: custom_acl.map(|group| thrift::CustomAclParams {
            hipster_group: group.to_string(),
            ..Default::default()
        }),
        org_acl: Some(thrift::OrgAclParams {
            oncall_name: "luisa".to_string(),
            maintainers,
            writers: vec![],
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(fbcode_build)]
fn acl_identity(id_type: &str, id_data: &str) -> thrift::AclIdentity {
    thrift::AclIdentity {
        id_type: id_type.to_string(),
        id_data: id_data.to_string(),
        ..Default::default()
    }
}

#[cfg(fbcode_build)]
#[mononoke::test]
fn test_validate_acl_request_shape() {
    let scm = acl_identity(AUTH_SET, "scm");
    assert!(
        validate_acl_request_shape(&org_acl_request(None, vec![scm.clone()])).is_ok(),
        "org_acl with a maintainer and no custom_acl is accepted",
    );
    assert!(
        validate_acl_request_shape(&thrift::RepoCreationRequest::default()).is_ok(),
        "a request without org_acl is not this check's business",
    );
    let both = validate_acl_request_shape(&org_acl_request(Some("group"), vec![scm]))
        .expect_err("custom_acl and org_acl together are refused");
    assert!(format!("{both:?}").contains("cannot both be set"));
    let no_maintainers = validate_acl_request_shape(&org_acl_request(None, vec![]))
        .expect_err("an org ACL without maintainers is refused");
    assert!(format!("{no_maintainers:?}").contains("at least one maintainer"));
}

#[cfg(fbcode_build)]
fn mirror_request(repo_name: &str, maintainer_oncall: Option<&str>) -> thrift::RepoCreationRequest {
    thrift::RepoCreationRequest {
        repo_name: repo_name.to_string(),
        oncall_name: "scm_server_infra".to_string(),
        size_bucket: RepoSizeBucket::SMALL,
        github_mirror: Some(thrift::GithubMirrorParams {
            maintainer_oncall: maintainer_oncall.map(str::to_string),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(fbcode_build)]
fn grant_entries(grants: &[AclPermissionChange], action: &str) -> Vec<(String, String)> {
    grants
        .iter()
        .find(|g| g.action == action)
        .unwrap_or_else(|| panic!("grants must contain a {action} action"))
        .entry_changes
        .iter()
        .map(|e| (e.entry.id_type.clone(), e.entry.id_data.clone()))
        .collect()
}

#[cfg(fbcode_build)]
#[mononoke::test]
fn test_github_mirror_acl_grants() {
    let grants = github_mirror_acl_grants("luisa");
    let sync = (
        SANDCASTLE_TAG.to_string(),
        GITHUB_MIRROR_SYNC_TAG.to_string(),
    );
    let group = (ONCALL_GROUP_TYPE.to_string(), "oncall_luisa".to_string());

    for action in GITHUB_MIRROR_SYNC_ACTIONS {
        assert!(
            grant_entries(&grants, action).contains(&sync),
            "the sync tag needs {action} to import into a read-only repo",
        );
    }
    assert!(
        grant_entries(&grants, "read").contains(&group),
        "the oncall can read"
    );
    assert!(
        grant_entries(&grants, "read").contains(&(
            DATA_PROJECT.to_string(),
            GITHUB_MIRROR_VERIFIER_DATA_PROJECT.to_string()
        )),
        "the verifier sweep can read, or it never dispatches the first sync",
    );
    assert_eq!(
        grant_entries(&grants, "maintainers"),
        vec![group.clone(), (AUTH_SET.to_string(), "scm".to_string())],
        "the oncall and Source Control maintain the ACL",
    );
    assert!(
        grant_entries(&grants, "read")
            .contains(&(AUTH_SET.to_string(), "coding_crewmates".to_string())),
        "the standard read grants stay",
    );
    assert!(
        !grant_entries(&grants, "write").contains(&group),
        "the oncall cannot write to a mirror",
    );
    assert_eq!(
        grants
            .iter()
            .filter(|g| g.action == "write_no_hooks")
            .count(),
        1,
        "write_no_hooks is added once, not per entry",
    );
}

#[cfg(fbcode_build)]
fn acl_with(entries: &[(&str, Identity)]) -> ACL {
    ACL {
        permissions: GITHUB_MIRROR_SYNC_ACTIONS
            .iter()
            .chain(["maintainers"].iter())
            .map(|action| infrasec_authorization::Permission {
                action: action.to_string(),
                entries: entries
                    .iter()
                    .filter(|(entry_action, _)| entry_action == action)
                    .map(|(_, identity)| infrasec_authorization::ACLEntry {
                        identity: identity.clone(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

#[cfg(fbcode_build)]
#[mononoke::test]
fn test_missing_github_mirror_entries() {
    let complete = required_github_mirror_entries("luisa");
    assert!(
        missing_github_mirror_entries(&acl_with(&complete), "luisa").is_empty(),
        "an ACL holding every required entry is accepted",
    );

    let without_write_no_hooks: Vec<_> = complete
        .iter()
        .filter(|(action, _)| *action != "write_no_hooks")
        .cloned()
        .collect();
    assert_eq!(
        missing_github_mirror_entries(&acl_with(&without_write_no_hooks), "luisa"),
        vec![format!(
            "write_no_hooks:{SANDCASTLE_TAG}:{GITHUB_MIRROR_SYNC_TAG}"
        )],
        "write is not enough: gitimport bypasses hooks",
    );

    assert_eq!(
        missing_github_mirror_entries(&acl_with(&complete), "other"),
        vec![format!("read:{ONCALL_GROUP_TYPE}:oncall_other")],
        "another oncall's read grant does not count",
    );
}

#[cfg(fbcode_build)]
#[mononoke::test]
fn test_validate_github_mirror_request_shape() {
    let refusal = |request: &thrift::RepoCreationRequest| -> String {
        format!(
            "{:?}",
            validate_github_mirror_request_shape(request)
                .expect_err("the request should be refused")
        )
    };

    assert!(
        validate_github_mirror_request_shape(&mirror_request("o/r", Some("luisa"))).is_ok(),
        "a mirror may get its own ACL, in a batch or alone",
    );
    assert!(
        validate_github_mirror_request_shape(&mirror_request("o/r", None)).is_ok(),
        "a mirror inheriting the org ACL is accepted",
    );
    assert!(
        validate_github_mirror_request_shape(&thrift::RepoCreationRequest::default()).is_ok(),
        "a request without github_mirror is not this check's business",
    );

    let writable = thrift::RepoCreationRequest {
        readonly: Some(false),
        ..mirror_request("o/r", None)
    };
    assert!(refusal(&writable).contains("always read-only"));
    let with_custom_acl = thrift::RepoCreationRequest {
        custom_acl: Some(thrift::CustomAclParams {
            hipster_group: "team".to_string(),
            ..Default::default()
        }),
        ..mirror_request("o/r", None)
    };
    assert!(refusal(&with_custom_acl).contains("cannot be combined"));

    let org_acl = thrift::RepoCreationRequest {
        org_acl: Some(thrift::OrgAclParams {
            oncall_name: "team_a".to_string(),
            maintainers: vec![acl_identity(AUTH_SET, "scm")],
            writers: vec![],
            ..Default::default()
        }),
        ..mirror_request("testorg/repo", None)
    };
    assert!(
        validate_github_mirror_request_shape(&org_acl).is_ok(),
        "a batch of mirrors may ask for a missing org ACL to be created",
    );
    let own_acl_and_org_acl = thrift::RepoCreationRequest {
        org_acl: org_acl.org_acl.clone(),
        ..mirror_request("testorg/repo", Some("team_a"))
    };
    assert!(
        refusal(&own_acl_and_org_acl).contains("org_acl does not apply"),
        "a repo with its own ACL does not inherit the org ACL",
    );
}

#[cfg(fbcode_build)]
#[mononoke::test]
fn test_org_acl_grants_carry_the_sync_grants_only_for_mirrors() {
    let maintainers = [hipster_identity(AUTH_SET, "scm")];
    let sync = (
        SANDCASTLE_TAG.to_string(),
        GITHUB_MIRROR_SYNC_TAG.to_string(),
    );

    let mirror = org_acl_grants(&maintainers, &[], true);
    for action in GITHUB_MIRROR_SYNC_ACTIONS {
        assert!(
            grant_entries(&mirror, action).contains(&sync),
            "every mirror in the org inherits this ACL, so the sync needs {action} on it",
        );
    }
    assert_eq!(
        mirror.iter().filter(|g| g.action == "write").count(),
        1,
        "the sync's write lands on the standard write grant",
    );
    assert!(
        grant_entries(&mirror, "read").contains(&(
            DATA_PROJECT.to_string(),
            GITHUB_MIRROR_VERIFIER_DATA_PROJECT.to_string()
        )),
        "the verifier sweep reads every mirror in the org through this ACL",
    );
    assert_eq!(
        grant_entries(&mirror, "maintainers"),
        vec![(AUTH_SET.to_string(), "scm".to_string())],
        "the maintainers stay exactly the requested ones",
    );

    let plain = org_acl_grants(&maintainers, &[], false);
    assert!(
        plain.iter().all(|g| g.action != "write_no_hooks"
            && !g
                .entry_changes
                .iter()
                .any(|e| e.entry.id_data == GITHUB_MIRROR_SYNC_TAG)),
        "an org ACL for ordinary repos gets no sync grants",
    );
}

#[mononoke::test]
fn test_to_repo_spec_tshirt_size_mapping() {
    assert_eq!(
        to_repo_spec_tshirt_size(RepoSizeBucket::EXTRA_SMALL).unwrap(),
        TShirtSize::SMALL
    );
    assert_eq!(
        to_repo_spec_tshirt_size(RepoSizeBucket::SMALL).unwrap(),
        TShirtSize::MEDIUM
    );
    assert_eq!(
        to_repo_spec_tshirt_size(RepoSizeBucket::MEDIUM).unwrap(),
        TShirtSize::MEDIUM
    );
    assert_eq!(
        to_repo_spec_tshirt_size(RepoSizeBucket::LARGE).unwrap(),
        TShirtSize::LARGE
    );
    assert_eq!(
        to_repo_spec_tshirt_size(RepoSizeBucket::EXTRA_LARGE).unwrap(),
        TShirtSize::HUGE
    );
}

#[mononoke::test]
fn test_make_repo_spec_produces_valid_spec() {
    let repo_id = RepositoryId::new(12345);
    let request = thrift::RepoCreationRequest {
        repo_name: "org/my-repo".to_string(),
        size_bucket: RepoSizeBucket::SMALL,
        ..Default::default()
    };

    let spec =
        make_repo_spec(&(repo_id, request), &template()).expect("make_repo_spec should succeed");

    assert_eq!(spec.repo_id, 12345);
    assert_eq!(spec.repo_name, "org/my-repo");
    assert!(
        !spec.readonly,
        "readonly defaults to false when not requested"
    );
    assert_eq!(spec.t_shirt_size, TShirtSize::MEDIUM);
    assert_eq!(
        spec.hipster_acl, "repos/git/org",
        "hipster_acl should be the top-level namespace ACL, not the full repo name"
    );
    // Everything else is the template's, verbatim.
    let t = template();
    assert_eq!(spec.enabled, t.enabled);
    assert_eq!(
        spec.default_commit_identity_scheme,
        t.default_commit_identity_scheme
    );
    assert_eq!(spec.sharding_regions, t.sharding_regions);
    assert_eq!(
        spec.tiers, t.tiers,
        "non-aosp repos get the template tiers unchanged"
    );
    assert_eq!(
        spec.repo_config, t.repo_config,
        "new repos carry the template's repo_config, not none"
    );
    assert_eq!(spec.tier_overrides, t.tier_overrides);
}

#[mononoke::test]
fn test_make_repo_spec_request_wins_for_identity_template_for_the_rest() {
    let request = thrift::RepoCreationRequest {
        repo_name: "org/my-repo".to_string(),
        size_bucket: RepoSizeBucket::SMALL,
        readonly: Some(true),
        ..Default::default()
    };
    let spec = make_repo_spec(&(RepositoryId::new(12345), request), &template()).unwrap();
    // request-owned
    assert_eq!(spec.repo_id, 12345);
    assert_eq!(spec.repo_name, "org/my-repo");
    assert_eq!(spec.hipster_acl, "repos/git/org");
    assert!(spec.readonly);
    assert_eq!(spec.t_shirt_size, TShirtSize::MEDIUM); // SMALL bucket -> MEDIUM
    // template-owned
    assert!(spec.enabled);
    assert_eq!(
        spec.default_commit_identity_scheme,
        RawCommitIdentityScheme::GIT
    );
    assert_eq!(spec.enable_git_bundle_uri, Some(false));
    assert_eq!(spec.sharding_regions, ShardingRegions::ALL_REGIONS);
    assert_eq!(spec.tiers, base());
    assert_eq!(spec.repo_config, template().repo_config);
    assert_eq!(spec.tier_overrides, template().tier_overrides);
    assert!(spec.tier_overrides.is_some(), "the fixture carries one");
}

#[mononoke::test]
fn test_make_repo_spec_honours_readonly_request() {
    let repo_id = RepositoryId::new(12346);
    let request = thrift::RepoCreationRequest {
        repo_name: "org/mirror-repo".to_string(),
        size_bucket: RepoSizeBucket::SMALL,
        readonly: Some(true),
        ..Default::default()
    };

    let spec =
        make_repo_spec(&(repo_id, request), &template()).expect("make_repo_spec should succeed");

    assert!(
        spec.readonly,
        "a request with readonly=true must produce a read-only RepoSpec"
    );
}

#[mononoke::test]
fn test_make_repo_spec_uses_top_level_acl_for_aosp_repo() {
    let repo_id = RepositoryId::new(18279);
    let request = thrift::RepoCreationRequest {
        repo_name: "aosp/platform/vendor/meta/prebuilts/assets".to_string(),
        size_bucket: RepoSizeBucket::SMALL,
        ..Default::default()
    };

    let spec =
        make_repo_spec(&(repo_id, request), &template()).expect("make_repo_spec should succeed");

    assert_eq!(
        spec.hipster_acl, "repos/git/aosp",
        "AOSP repos must use the top-level `repos/git/aosp` ACL, not a non-existent full-path ACL"
    );
}

#[mononoke::test]
fn test_make_repo_spec_uses_full_name_when_no_slash() {
    let repo_id = RepositoryId::new(99999);
    let request = thrift::RepoCreationRequest {
        repo_name: "simple-repo".to_string(),
        size_bucket: RepoSizeBucket::SMALL,
        ..Default::default()
    };

    let spec =
        make_repo_spec(&(repo_id, request), &template()).expect("make_repo_spec should succeed");

    assert_eq!(
        spec.hipster_acl, "repos/git/simple-repo",
        "Repos without `/` should use the full name as the ACL"
    );
}

#[mononoke::test]
fn test_tier_list_for_repo_spec_aosp_prefix_adds_multi_repo_land() {
    assert_eq!(
        tier_list_for_repo_spec(&base(), "aosp/platform/vendor/foo"),
        with_multi_repo_land(base()),
        "aosp/* repos must include aosp_multi_repo_land tier"
    );
}

#[mononoke::test]
fn test_tier_list_for_repo_spec_nested_aosp_adds_multi_repo_land() {
    // Substring match: repos with `aosp/` deeper in the path (e.g. the
    // Oculus AOSP fork) must also be on the aosp_multi_repo_land tier.
    assert_eq!(
        tier_list_for_repo_spec(&base(), "oculus/aosp/vendor/oculus"),
        with_multi_repo_land(base()),
        "repos containing aosp/ as a substring must include aosp_multi_repo_land tier"
    );
}

#[mononoke::test]
fn test_tier_list_for_repo_spec_non_aosp_excluded_from_multi_repo_land() {
    assert_eq!(
        tier_list_for_repo_spec(&base(), "manus/foo"),
        base(),
        "non-aosp repos must NOT include aosp_multi_repo_land tier"
    );
    assert_eq!(
        tier_list_for_repo_spec(&base(), "simple-repo"),
        base(),
        "simple repos must NOT include aosp_multi_repo_land tier"
    );
    // Boundary: a repo literally named "aosp" (no slash) does NOT contain `aosp/`.
    assert_eq!(
        tier_list_for_repo_spec(&base(), "aosp"),
        base(),
        "literal name 'aosp' (no trailing /) must NOT match the aosp/ substring"
    );
    // Boundary: confusingly-named prefix that shares "aosp" but isn't `aosp/`.
    assert_eq!(
        tier_list_for_repo_spec(&base(), "aosp_extras/foo"),
        base(),
        "aosp_extras/* must NOT match the aosp/ substring"
    );
}

#[mononoke::test]
fn test_make_repo_spec_aosp_appends_multi_repo_land_to_template_tiers() {
    let request = thrift::RepoCreationRequest {
        repo_name: "aosp/platform/vendor/meta/prebuilts/assets".to_string(),
        size_bucket: RepoSizeBucket::SMALL,
        ..Default::default()
    };
    let spec = make_repo_spec(&(RepositoryId::new(18279), request), &template()).unwrap();
    assert_eq!(
        spec.tiers,
        ["tmpl_a", "tmpl_b", "aosp_multi_repo_land"],
        "AOSP repos get the template tiers plus aosp_multi_repo_land so multi_repo_land_service can serve them"
    );
}

#[mononoke::test]
fn test_default_branch_validation() {
    for invalid in [
        "",
        "HEAD",
        "head",
        "Head",
        "hEaD",
        "refs/heads/main",
        "refs/tags/v1.0",
    ] {
        let err = validate_default_branch(invalid)
            .expect_err(&format!("'{invalid}' must be rejected as a default_branch"));
        assert!(
            matches!(err, scs_errors::ServiceError::Request(_)),
            "'{invalid}' must be rejected as an invalid request, got: {err:?}"
        );
    }
    for valid in ["main", "release/1.0"] {
        assert_eq!(
            validate_default_branch(valid)
                .unwrap_or_else(|err| panic!("'{valid}' must be accepted, got: {err:?}")),
            valid,
        );
    }
}

#[mononoke::fbinit_test]
async fn test_write_default_branch_symref_writes_head_row(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let store = SqlGitSymbolicRefsBuilder::with_sqlite_in_memory()?.build(RepositoryId::new(1));

    write_default_branch_symref(&ctx, &store, "main")
        .await
        .expect("writing the HEAD symref should succeed");

    let entry = store
        .get_ref_by_symref(&ctx, "HEAD".to_string())
        .await?
        .expect("a HEAD symref row should exist after the write");
    assert_eq!(entry.ref_name, "main");
    assert_eq!(entry.ref_name_with_type(), "refs/heads/main");
    Ok(())
}

#[mononoke::test]
fn test_make_repo_spec_uses_full_acl_when_custom_acl_set() {
    let repo_id = RepositoryId::new(55555);
    let request = thrift::RepoCreationRequest {
        repo_name: "fairinternal/occhi".to_string(),
        size_bucket: RepoSizeBucket::SMALL,
        custom_acl: Some(thrift::CustomAclParams {
            hipster_group: "oncall_onevision".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    };

    let spec =
        make_repo_spec(&(repo_id, request), &template()).expect("make_repo_spec should succeed");

    assert_eq!(
        spec.hipster_acl, "repos/git/fairinternal/occhi",
        "Repos with custom_acl should use the full-path ACL"
    );
}

#[cfg(fbcode_build)]
#[mononoke::test]
fn test_make_repo_spec_for_github_mirrors() {
    let spec = |request| {
        make_repo_spec(&(RepositoryId::new(55556), request), &template())
            .expect("make_repo_spec should succeed")
    };

    let own_acl = spec(mirror_request("fairinternal/Occhi", Some("onevision")));
    assert_eq!(own_acl.hipster_acl, "repos/git/fairinternal/occhi");
    assert!(own_acl.readonly, "a mirror is read-only without asking");

    let inherited = spec(mirror_request("fairinternal/Occhi", None));
    assert_eq!(inherited.hipster_acl, "repos/git/fairinternal");
    assert!(inherited.readonly, "a mirror is read-only without asking");
}
