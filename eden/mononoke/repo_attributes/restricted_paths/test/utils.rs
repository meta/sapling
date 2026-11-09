/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use blobstore::Loadable;
use clientinfo::ClientEntryPoint;
use clientinfo::ClientInfo;
use clientinfo::ClientRequestInfo;
use content_manifest_derivation::RootContentManifestId;
use context::CoreContext;
use context::SessionContainer;
use derivation_queue_thrift::DerivationPriority;
use fbinit::FacebookInit;
use fsnodes::RootFsnodeId;
use futures::StreamExt;
use futures::TryStreamExt;
use itertools::Itertools;
use manifest::ManifestOps;
use maplit::hashmap;
use mercurial_derivation::RootHgAugmentedManifestId;
use mercurial_derivation::derive_hg_changeset::DeriveHgChangeset;
use mercurial_types::HgAugmentedManifestId;
use metaconfig_types::AclManifestMode;
use metaconfig_types::ComparableRegex;
use metaconfig_types::EnforcementConditionSet;
use metaconfig_types::EnforcementExemptionSet;
use metaconfig_types::PathRestrictionMetadata;
use metaconfig_types::RequestMatchers;
use metaconfig_types::RestrictedPathsConfig;
use metaconfig_types::RestrictedPathsManifestIdStoreConfig;
use metadata::Metadata;
use mononoke_api::MononokeError;
use mononoke_api::Repo as TestRepo;
use mononoke_api::RepoContext;
use mononoke_api::RestrictedPathsPolicy;
use mononoke_api_hg::HgDataId;
use mononoke_api_hg::RepoContextHgExt;
use mononoke_types::ChangesetId;
use mononoke_types::MPath;
use mononoke_types::NonRootMPath;
use mononoke_types::RepositoryId;
use permission_checker::Acl;
use permission_checker::Acls;
use permission_checker::InternalAclProvider;
use permission_checker::MononokeIdentity;
use permission_checker::MononokeIdentitySet;
use pretty_assertions::assert_eq;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataArc;
use repo_derived_data::RepoDerivedDataRef;
use restricted_paths::SqlRestrictedPathsManifestIdStoreBuilder;
use restricted_paths::*;
use scuba_ext::MononokeScubaSampleBuilder;
use sql_construct::SqlConstruct;
use strum::Display as EnumDisplay;
use strum::EnumIter;
use strum::IntoEnumIterator;
use test_repo_factory::TestRepoFactory;
use tests_utils::CreateCommitContext;

pub const TEST_CLIENT_MAIN_ID: &str = "user:myusername0";

pub struct RestrictedPathsScenarioResult {
    pub scuba_logs: Vec<ScubaAccessLogSample>,
    /// All entries present in the manifest-id store after the scenario ran.
    pub manifest_id_store_entries: Vec<RestrictedPathManifestIdEntry>,
}

struct RestrictedPathsScenario {
    repo: TestRepo,
    log_path: PathBuf,
}

pub struct RestrictedPathsTestData {
    pub ctx: CoreContext,
    // The changes that should be made in the test's commit. Each entry represents
    // a file to be created, along with its optional content. If no content is
    // provided, the file path itself is used as the content.
    pub file_path_changes: Vec<(String, Option<String>)>,
    // The entries you expect in the manifest id store after the test runs
    expected_manifest_entries: Option<Vec<RestrictedPathManifestIdEntry>>,
    // The scuba logs you expect to be logged after the test runs
    expected_scuba_logs: Option<Vec<ScubaAccessLogSample>>,
    /// Enforcement scenarios: (enforcement_condition_sets, expect_enforcement).
    /// For each scenario, a new repo is built with those condition sets and
    /// access APIs are called to verify whether enforcement is triggered.
    enforcement_scenarios: Vec<(Vec<EnforcementConditionSet>, bool)>,
    /// Exemption sets written into every scenario repo's config.
    enforcement_exemption_sets: Vec<EnforcementExemptionSet>,
    /// Config-backed restrictions written into `RestrictedPathsConfig.path_acls`.
    config_restricted_paths: Vec<(NonRootMPath, MononokeIdentity)>,
    /// When true, every config restricted path is marked `read_only` in its
    /// `PathRestrictionMetadata`.
    config_paths_read_only: bool,
    /// AclManifest-backed restrictions materialized as `.slacl` files.
    acl_manifest_restricted_paths: Vec<(NonRootMPath, MononokeIdentity)>,
    acl_manifest_mode: AclManifestMode,
    /// Repo regions config for recreating ACLs: (region_name, usernames)
    repo_regions_config: Vec<(String, Vec<String>)>,
    groups_config: Vec<(String, Vec<String>)>,
    tooling_allowlist_group: Option<String>,
}

#[derive(Clone)]
pub struct RestrictedPathsTestDataBuilder {
    config_restricted_paths: Vec<(NonRootMPath, MononokeIdentity)>,
    config_paths_read_only: bool,
    acl_manifest_restricted_paths: Vec<(NonRootMPath, MononokeIdentity)>,
    acl_manifest_mode: AclManifestMode,
    tooling_allowlist_group: Option<String>,
    /// Store the repo regions config for recreating ACLs in enforcement scenarios
    repo_regions_config: Vec<(String, Vec<String>)>,
    groups_config: Vec<(String, Vec<String>)>,
    server_side_tenting: bool,
    partial_responses_opt_in: bool,
    partial_responses_opt_out: bool,
    client_identity: Option<MononokeIdentity>,
    client_machine_tier_identity: Option<MononokeIdentity>,
    file_path_changes: Vec<(String, Option<String>)>,
    expected_manifest_entries: Option<Vec<RestrictedPathManifestIdEntry>>,
    expected_scuba_logs: Option<Vec<ScubaAccessLogSample>>,
    /// List of (enforcement_condition_sets, expect_enforcement) tuples.
    /// The test will run for each scenario, applying the condition sets and
    /// verifying if enforcement is or isn't triggered as expected.
    enforcement_scenarios: Vec<(Vec<EnforcementConditionSet>, bool)>,
    enforcement_exemption_sets: Vec<EnforcementExemptionSet>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScubaAccessLogSample {
    repo_id: RepositoryId,
    restricted_paths: Vec<NonRootMPath>,
    manifest_id: Option<RestrictedManifestId>,
    manifest_type: Option<ManifestType>,
    full_path: Option<NonRootMPath>,
    client_identities: Vec<String>,
    client_main_id: String,
    has_authorization: bool,
    is_allowlisted_tooling: bool,
    has_acl_access: bool,
    acls: Vec<MononokeIdentity>,
    considered_restricted_by: Vec<String>,
    access_enforcement_enabled: Option<bool>,
    acl_manifest_mode: Option<String>,
    config_error: Option<String>,
    acl_manifest_error: Option<String>,
    shadow_mismatch: Option<bool>,
    shadow_mismatch_detail: Option<String>,
}

impl ScubaAccessLogSample {
    pub fn full_path(&self) -> Option<&NonRootMPath> {
        self.full_path.as_ref()
    }

    pub fn has_authorization(&self) -> bool {
        self.has_authorization
    }

    pub fn manifest_type(&self) -> Option<&ManifestType> {
        self.manifest_type.as_ref()
    }

    pub fn manifest_id(&self) -> Option<&RestrictedManifestId> {
        self.manifest_id.as_ref()
    }

    pub fn acl_manifest_mode(&self) -> Option<&str> {
        self.acl_manifest_mode.as_deref()
    }

    pub fn shadow_mismatch(&self) -> Option<bool> {
        self.shadow_mismatch
    }

    pub fn shadow_mismatch_detail(&self) -> Option<&str> {
        self.shadow_mismatch_detail.as_deref()
    }

    pub fn acl_manifest_error(&self) -> Option<&str> {
        self.acl_manifest_error.as_deref()
    }

    pub fn considered_restricted_by(&self) -> &[String] {
        &self.considered_restricted_by
    }

    pub fn access_enforcement_enabled(&self) -> Option<bool> {
        self.access_enforcement_enabled
    }
}

#[derive(Debug, Clone)]
pub struct ScubaAccessLogSampleBuilder {
    repo_id: Option<RepositoryId>,
    restricted_paths: Vec<NonRootMPath>,
    manifest_id: Option<RestrictedManifestId>,
    manifest_type: Option<ManifestType>,
    full_path: Option<NonRootMPath>,
    client_identities: Vec<String>,
    client_main_id: Option<String>,
    has_authorization: Option<bool>,
    is_allowlisted_tooling: Option<bool>,
    has_acl_access: Option<bool>,
    acls: Vec<MononokeIdentity>,
    considered_restricted_by: Vec<String>,
    access_enforcement_enabled: Option<bool>,
    acl_manifest_mode: Option<String>,
    config_error: Option<String>,
    acl_manifest_error: Option<String>,
    shadow_mismatch: Option<bool>,
    shadow_mismatch_detail: Option<String>,
}

impl ScubaAccessLogSampleBuilder {
    pub fn new() -> Self {
        Self {
            repo_id: None,
            restricted_paths: Vec::new(),
            manifest_id: None,
            manifest_type: None,
            full_path: None,
            client_identities: Vec::new(),
            client_main_id: None,
            has_authorization: None,
            is_allowlisted_tooling: None,
            has_acl_access: None,
            acls: Vec::new(),
            considered_restricted_by: vec!["manifest_db".to_string()],
            access_enforcement_enabled: None,
            acl_manifest_mode: Some("disabled".to_string()),
            config_error: None,
            acl_manifest_error: None,
            shadow_mismatch: None,
            shadow_mismatch_detail: None,
        }
    }

    pub fn with_repo_id(mut self, repo_id: RepositoryId) -> Self {
        self.repo_id = Some(repo_id);
        self
    }

    pub fn with_restricted_paths(mut self, restricted_paths: Vec<NonRootMPath>) -> Self {
        self.restricted_paths = restricted_paths;
        self
    }

    pub fn with_manifest_id(mut self, manifest_id: RestrictedManifestId) -> Self {
        self.manifest_id = Some(manifest_id);
        self
    }

    pub fn with_manifest_type(mut self, manifest_type: ManifestType) -> Self {
        self.manifest_type = Some(manifest_type);
        self
    }

    pub fn with_full_path(mut self, full_path: NonRootMPath) -> Self {
        self.full_path = Some(full_path);
        self
    }

    pub fn with_client_identities(mut self, client_identities: Vec<String>) -> Self {
        self.client_identities = client_identities;
        self
    }

    pub fn with_client_main_id(mut self, client_main_id: String) -> Self {
        self.client_main_id = Some(client_main_id);
        self
    }

    pub fn with_has_authorization(mut self, has_authorization: bool) -> Self {
        self.has_authorization = Some(has_authorization);
        self
    }

    pub fn with_is_allowlisted_tooling(mut self, is_allowlisted_tooling: bool) -> Self {
        self.is_allowlisted_tooling = Some(is_allowlisted_tooling);
        self
    }

    pub fn with_has_acl_access(mut self, has_acl_access: bool) -> Self {
        self.has_acl_access = Some(has_acl_access);
        self
    }

    pub fn with_acls(mut self, acls: Vec<MononokeIdentity>) -> Self {
        self.acls = acls;
        self
    }

    #[expect(
        dead_code,
        reason = "not every Scuba sample builder test sets source attribution"
    )]
    pub fn with_considered_restricted_by(mut self, considered_restricted_by: Vec<String>) -> Self {
        self.considered_restricted_by = considered_restricted_by;
        self
    }

    pub fn with_access_enforcement_enabled(mut self, access_enforcement_enabled: bool) -> Self {
        self.access_enforcement_enabled = Some(access_enforcement_enabled);
        self
    }

    pub fn build(self) -> Result<ScubaAccessLogSample> {
        let repo_id = self.repo_id.ok_or_else(|| anyhow!("repo_id is required"))?;
        let client_main_id = self
            .client_main_id
            .ok_or_else(|| anyhow!("client_main_id is required"))?;
        let has_authorization = self
            .has_authorization
            .ok_or_else(|| anyhow!("has_authorization is required"))?;
        // Default to false if not provided, since most tests don't have a tooling allowlist
        let is_allowlisted_tooling = self.is_allowlisted_tooling.unwrap_or(false);
        let has_acl_access = self.has_acl_access.unwrap_or(false);

        Ok(ScubaAccessLogSample {
            repo_id,
            restricted_paths: self.restricted_paths,
            manifest_id: self.manifest_id,
            manifest_type: self.manifest_type,
            full_path: self.full_path,
            client_identities: self.client_identities,
            client_main_id,
            has_authorization,
            is_allowlisted_tooling,
            has_acl_access,
            acls: self.acls,
            considered_restricted_by: self.considered_restricted_by,
            access_enforcement_enabled: self.access_enforcement_enabled,
            acl_manifest_mode: self.acl_manifest_mode,
            config_error: self.config_error,
            acl_manifest_error: self.acl_manifest_error,
            shadow_mismatch: self.shadow_mismatch,
            shadow_mismatch_detail: self.shadow_mismatch_detail,
        })
    }
}

impl RestrictedPathsTestDataBuilder {
    pub fn new() -> Self {
        Self {
            config_restricted_paths: vec![],
            config_paths_read_only: false,
            acl_manifest_restricted_paths: vec![],
            acl_manifest_mode: AclManifestMode::Disabled,
            tooling_allowlist_group: None,
            groups_config: vec![],
            repo_regions_config: vec![],
            server_side_tenting: false,
            partial_responses_opt_in: false,
            partial_responses_opt_out: false,
            client_identity: None,
            client_machine_tier_identity: None,
            file_path_changes: vec![],
            expected_manifest_entries: None,
            expected_scuba_logs: None,
            enforcement_scenarios: vec![],
            enforcement_exemption_sets: vec![],
        }
    }

    /// Configure `enforcement_exemption_sets` on every scenario repo.
    pub fn with_enforcement_exemption_sets(
        mut self,
        enforcement_exemption_sets: Vec<EnforcementExemptionSet>,
    ) -> Self {
        self.enforcement_exemption_sets = enforcement_exemption_sets;
        self
    }

    pub fn with_restricted_paths(
        mut self,
        restricted_paths: Vec<(NonRootMPath, MononokeIdentity)>,
    ) -> Self {
        self.config_restricted_paths = restricted_paths.clone();
        self.acl_manifest_restricted_paths = restricted_paths;
        self
    }

    pub fn with_config_restricted_paths(
        mut self,
        restricted_paths: Vec<(NonRootMPath, MononokeIdentity)>,
    ) -> Self {
        self.config_restricted_paths = restricted_paths;
        self
    }

    /// Mark every config restricted path as `read_only` in its
    /// `PathRestrictionMetadata`. Defaults to false.
    pub fn with_config_paths_read_only(mut self, read_only: bool) -> Self {
        self.config_paths_read_only = read_only;
        self
    }

    pub fn with_acl_manifest_restricted_paths(
        mut self,
        restricted_paths: Vec<(NonRootMPath, MononokeIdentity)>,
    ) -> Self {
        self.acl_manifest_restricted_paths = restricted_paths;
        self
    }

    pub fn with_acl_manifest_mode(mut self, acl_manifest_mode: AclManifestMode) -> Self {
        self.acl_manifest_mode = acl_manifest_mode;
        self
    }

    /// Set the tooling allowlist group name.
    /// The group should be created via `with_test_groups`.
    pub fn with_tooling_allowlist_group(mut self, group_name: &str) -> Self {
        self.tooling_allowlist_group = Some(group_name.to_string());
        self
    }

    /// Set the client identity for the test context.
    /// Defaults to USER:myusername0 if not specified.
    pub fn with_client_identity(mut self, identity: &str) -> Result<Self> {
        self.client_identity = Some(MononokeIdentity::from_str(identity)?);
        Ok(self)
    }

    /// Replaces the caller's default `USER:myusername0` identity with the same
    /// id carrying an `agent/id` attribute, so `Metadata::likely_an_agent`
    /// returns true. Only available in fbcode builds: OSS has no JSON identity
    /// decoder and its `likely_an_agent` is always false.
    #[cfg(fbcode_build)]
    pub fn with_agentic_client_identity(mut self) -> Result<Self> {
        let identities = MononokeIdentity::try_from_json_encoded(
            r#"{"authn":["mid://TEST/USER/myusername0?agent.id=AGENT%3aclaude_code"]}"#,
        )?;
        self.client_identity = identities.into_iter().next();
        Ok(self)
    }

    /// Adds a MACHINE_TIER identity to the simulated caller. Used by tests
    /// that exercise the `machine_tiers` enforcement condition. Default (no
    /// call) leaves the caller without a MACHINE_TIER identity.
    pub fn with_machine_tier(mut self, tier: &str) -> Self {
        self.client_machine_tier_identity = Some(MononokeIdentity::from_legacy_type_data(
            "MACHINE_TIER",
            tier,
        ));
        self
    }

    /// Set up test groups for membership checking.
    /// Each member should be a full identity string (e.g., "SERVICE_IDENTITY:service_foo"
    /// or "USER:username").
    pub fn with_test_groups(mut self, groups_config: Vec<(&str, Vec<&str>)>) -> Self {
        self.groups_config = groups_config
            .into_iter()
            .map(|(name, users)| {
                (
                    name.to_string(),
                    users.into_iter().map(|s| s.to_string()).collect(),
                )
            })
            .collect();
        self
    }

    pub fn with_test_repo_region_acls(
        mut self,
        repo_regions_config: Vec<(&str, Vec<&str>)>,
    ) -> Self {
        self.repo_regions_config = repo_regions_config
            .into_iter()
            .map(|(repo_region, users)| {
                (
                    repo_region.to_string(),
                    users.into_iter().map(|s| s.to_string()).collect(),
                )
            })
            .collect();
        self
    }

    pub fn with_server_side_tenting(mut self, server_side_tenting: bool) -> Self {
        self.server_side_tenting = server_side_tenting;
        self
    }

    pub fn with_partial_responses_opt_in(mut self, partial_responses_opt_in: bool) -> Self {
        self.partial_responses_opt_in = partial_responses_opt_in;
        self
    }

    pub fn with_partial_responses_opt_out(mut self, partial_responses_opt_out: bool) -> Self {
        self.partial_responses_opt_out = partial_responses_opt_out;
        self
    }

    pub fn with_file_path_changes(mut self, file_path_changes: Vec<(&str, Option<&str>)>) -> Self {
        self.file_path_changes = file_path_changes
            .into_iter()
            .map(|(path, content)| (path.to_string(), content.map(|s| s.to_string())))
            .collect();
        self
    }

    // Set entries you expect in the manifest id store after the test runs
    pub fn expecting_manifest_id_store_entries(
        mut self,
        expected_manifest_entries: Vec<RestrictedPathManifestIdEntry>,
    ) -> Self {
        self.expected_manifest_entries = Some(expected_manifest_entries);
        self
    }

    // Set the scuba logs you expect to be logged after the test runs
    pub fn expecting_scuba_access_logs(
        mut self,
        expected_scuba_logs: Vec<ScubaAccessLogSample>,
    ) -> Self {
        self.expected_scuba_logs = Some(expected_scuba_logs);
        self
    }

    #[expect(
        dead_code,
        reason = "scenario-matrix tests use this helper when expected rows do not vary by scenario"
    )]
    pub fn with_enforcement_scenarios(
        mut self,
        scenarios: Vec<(Vec<EnforcementConditionSet>, bool)>,
    ) -> Self {
        self.enforcement_scenarios = scenarios;
        self
    }

    pub async fn build(self, fb: FacebookInit) -> Result<RestrictedPathsTestData> {
        // Use custom client identity or default to USER:myusername0
        let client_identity = self
            .client_identity
            .unwrap_or_else(|| MononokeIdentity::from_legacy_type_data("USER", "myusername0"));
        let client_main_id = format!(
            "{}:{}",
            client_identity.id_type().to_lowercase(),
            client_identity.id_data()
        );

        let mut cri = ClientRequestInfo::new(ClientEntryPoint::Tests);
        cri.set_main_id(client_main_id);
        let client_info = ClientInfo::new_with_client_request_info(cri);

        let identities = {
            let mut set = BTreeSet::from([client_identity]);
            if let Some(machine_tier_identity) = self.client_machine_tier_identity {
                set.insert(machine_tier_identity);
            }
            set
        };
        let metadata = {
            let mut md = Metadata::new(
                Some(&"restricted_paths_test".to_string()),
                identities,
                false,
                false,
                None,
                None,
            )
            .await;
            md.add_client_info(client_info);
            md.add_partial_responses_opt_in(self.partial_responses_opt_in);
            md.add_partial_responses_opt_out(self.partial_responses_opt_out);
            Arc::new(md)
        };
        let session_container = SessionContainer::builder(fb)
            .metadata(metadata)
            .server_side_tenting(self.server_side_tenting)
            .build();
        let ctx = CoreContext::test_mock_session(session_container);

        Ok(RestrictedPathsTestData {
            ctx,
            file_path_changes: self.file_path_changes,
            expected_manifest_entries: self.expected_manifest_entries,
            expected_scuba_logs: self.expected_scuba_logs,
            enforcement_scenarios: self.enforcement_scenarios,
            enforcement_exemption_sets: self.enforcement_exemption_sets,
            config_restricted_paths: self.config_restricted_paths,
            config_paths_read_only: self.config_paths_read_only,
            acl_manifest_restricted_paths: self.acl_manifest_restricted_paths,
            acl_manifest_mode: self.acl_manifest_mode,
            repo_regions_config: self.repo_regions_config,
            groups_config: self.groups_config,
            tooling_allowlist_group: self.tooling_allowlist_group,
        })
    }
}

impl RestrictedPathsTestData {
    /// Given a list of restricted paths and a list of file paths, create a changeset
    /// modifying those paths, derive the hg manifest and fetch all the hg manifests
    /// from the last changeset, to simulate access to all directories in the repo.
    ///
    /// If expectations are set via the builder, this method will automatically verify
    /// them against the actual results. Otherwise, it will just run the test without
    /// any assertions.
    ///
    /// Each file path can optionally specify content. If no content is provided,
    /// the file path itself is used as the content.
    pub async fn run_restricted_paths_test(&self) -> Result<()> {
        // ACL manifest checking is enabled via the just_knobs.json default
        // (restricted_paths_access_log_with_acl_manifest = true). This ensures spawned
        // tasks also see the JK value, unlike with_just_knobs_async which
        // only applies to the current thread.

        // Run without enforcement scenarios
        self.run_restricted_paths_test_inner(0, &[], false).await?;

        // Run enforcement testing for each scenario
        for (scenario_idx, (conditions, expect_enforcement)) in
            self.enforcement_scenarios.iter().enumerate()
        {
            self.run_restricted_paths_test_inner(scenario_idx + 1, conditions, *expect_enforcement)
                .await?;
        }

        Ok(())
    }

    /// Runs exactly one restricted paths scenario.
    ///
    /// Use this when the same fixture needs scenario-specific expected logs,
    /// such as Scuba rows that differ by `access_enforcement_enabled`.
    pub async fn run_restricted_paths_test_scenario(
        &self,
        enforcement_condition_sets: &[EnforcementConditionSet],
        expect_enforcement: bool,
    ) -> Result<()> {
        self.run_restricted_paths_test_inner(0, enforcement_condition_sets, expect_enforcement)
            .await?;

        Ok(())
    }

    /// Build a scenario repo without running derivation or access operations.
    /// For tests that only inspect config-derived predicates such as
    /// `may_have_restricted_paths`.
    pub async fn build_repo_only(&self) -> Result<TestRepo> {
        Ok(self.setup_scenario_repo(&[]).await?.repo)
    }

    /// Runs the standard access scenario and returns the Scuba rows it emitted.
    pub async fn observe_restricted_paths_scenario(
        &self,
        enforcement_condition_sets: &[EnforcementConditionSet],
    ) -> Result<RestrictedPathsScenarioResult> {
        self.run_restricted_paths_test_inner(0, enforcement_condition_sets, false)
            .await
    }

    /// Calls path-based restricted paths logging directly and returns emitted Scuba rows.
    pub async fn observe_path_access(
        &self,
        path: NonRootMPath,
        cs_id: Option<ChangesetId>,
        enforcement_condition_sets: &[EnforcementConditionSet],
    ) -> Result<RestrictedPathsScenarioResult> {
        let scenario = self.setup_scenario_repo(enforcement_condition_sets).await?;

        scenario
            .repo
            .restricted_paths()
            .log_access_by_path_if_restricted(&self.ctx, path, cs_id)
            .await?;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let manifest_id_store_entries = scenario
            .repo
            .restricted_paths()
            .config_based()
            .manifest_id_store()
            .get_all_entries(&self.ctx)
            .await?;
        Ok(RestrictedPathsScenarioResult {
            scuba_logs: deserialize_scuba_log_file(&scenario.log_path)?,
            manifest_id_store_entries,
        })
    }

    /// Calls manifest-based restricted paths logging directly and returns emitted Scuba rows.
    pub async fn observe_manifest_access(
        &self,
        manifest_id: RestrictedManifestId,
        manifest_type: ManifestType,
        cs_id: Option<ChangesetId>,
        enforcement_condition_sets: &[EnforcementConditionSet],
    ) -> Result<RestrictedPathsScenarioResult> {
        let scenario = self.setup_scenario_repo(enforcement_condition_sets).await?;

        let bcs_id = self.create_configured_changeset(&scenario.repo).await?;
        scenario.repo.derive_hg_changeset(&self.ctx, bcs_id).await?;
        scenario
            .repo
            .repo_derived_data()
            .derive::<RootHgAugmentedManifestId>(&self.ctx, bcs_id, DerivationPriority::LOW)
            .await?;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        scenario
            .repo
            .restricted_paths()
            .log_access_by_manifest_if_restricted(&self.ctx, manifest_id, manifest_type, cs_id)
            .await?;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let manifest_id_store_entries = scenario
            .repo
            .restricted_paths()
            .config_based()
            .manifest_id_store()
            .get_all_entries(&self.ctx)
            .await?;
        Ok(RestrictedPathsScenarioResult {
            scuba_logs: deserialize_scuba_log_file(&scenario.log_path)?,
            manifest_id_store_entries,
        })
    }

    /// Calls path-based restricted paths enforcement directly and returns true
    /// when access was denied.
    pub async fn observe_path_enforcement(
        &self,
        path: NonRootMPath,
        enforcement_condition_sets: &[EnforcementConditionSet],
    ) -> Result<bool> {
        let scenario = self.setup_scenario_repo(enforcement_condition_sets).await?;

        let mpath = MPath::from(path);
        let result = spawn_enforce_restricted_path_access(
            &self.ctx,
            scenario.repo.restricted_paths_arc().clone(),
            &mpath,
            "restricted_paths_test",
            None,
        )
        .await;
        let was_denied = match result {
            Ok(()) => false,
            Err(RestrictedPathsError::AuthorizationError(_)) => true,
            Err(RestrictedPathsError::AclFileChangeRejected(err)) => return Err(err.into()),
            Err(RestrictedPathsError::InternalError(err)) => return Err(err),
        };

        Ok(was_denied)
    }

    /// Creates the configured test commit, then calls path-based restricted
    /// paths enforcement with that commit's changeset id.
    pub async fn observe_path_enforcement_after_commit(
        &self,
        path: NonRootMPath,
        enforcement_condition_sets: &[EnforcementConditionSet],
    ) -> Result<bool> {
        let scenario = self.setup_scenario_repo(enforcement_condition_sets).await?;
        let bcs_id = self.create_configured_changeset(&scenario.repo).await?;

        let mpath = MPath::from(path);
        let result = spawn_enforce_restricted_path_access(
            &self.ctx,
            scenario.repo.restricted_paths_arc().clone(),
            &mpath,
            "restricted_paths_test",
            Some(bcs_id),
        )
        .await;
        let was_denied = match result {
            Ok(()) => false,
            Err(RestrictedPathsError::AuthorizationError(_)) => true,
            Err(RestrictedPathsError::AclFileChangeRejected(err)) => return Err(err.into()),
            Err(RestrictedPathsError::InternalError(err)) => return Err(err),
        };

        Ok(was_denied)
    }

    /// Creates the configured test commit, then runs both combined path checking
    /// and spawned enforcement against that same repository state.
    pub async fn observe_path_check_and_enforcement_after_commit(
        &self,
        path: NonRootMPath,
        enforcement_condition_sets: &[EnforcementConditionSet],
    ) -> Result<(Vec<PathRestrictionCheckResult>, bool)> {
        let scenario = self.setup_scenario_repo(enforcement_condition_sets).await?;
        let bcs_id = self.create_configured_changeset(&scenario.repo).await?;

        let checks = scenario
            .repo
            .restricted_paths()
            .get_path_restriction_check(&self.ctx, Some(bcs_id), std::slice::from_ref(&path))
            .await?;
        let result = spawn_enforce_restricted_path_access(
            &self.ctx,
            scenario.repo.restricted_paths_arc().clone(),
            &MPath::from(path),
            "restricted_paths_test",
            Some(bcs_id),
        )
        .await;
        let was_denied = match result {
            Ok(()) => false,
            Err(RestrictedPathsError::AuthorizationError(_)) => true,
            Err(err) => return Err(err.into()),
        };

        Ok((checks, was_denied))
    }

    /// Run restricted paths testing for a single scenario.
    /// Creates a repo with the given enforcement condition sets and runs all access operations.
    /// If expect_enforcement is true, expects an AuthorizationError from the access operations.
    async fn run_restricted_paths_test_inner(
        &self,
        scenario_idx: usize,
        enforcement_condition_sets: &[EnforcementConditionSet],
        expect_enforcement: bool,
    ) -> Result<RestrictedPathsScenarioResult> {
        println!(
            "Running scenario {scenario_idx} with expect_enforcement: {expect_enforcement} and enforcement_condition_sets: {enforcement_condition_sets:#?}"
        );
        // Set up a fresh repo and log file for this scenario.
        let RestrictedPathsScenario {
            repo: scenario_repo,
            log_path,
        } = self.setup_scenario_repo(enforcement_condition_sets).await?;

        let blobstore = Arc::new(scenario_repo.repo_blobstore().clone());
        let bcs_id = self.create_configured_changeset(&scenario_repo).await?;

        // Get the hg changeset id for the commit, to trigger hg manifest derivation
        let hg_cs_id = scenario_repo.derive_hg_changeset(&self.ctx, bcs_id).await?;

        let repo = Arc::new(scenario_repo.clone());
        let repo_ctx = RepoContext::new_test(self.ctx.clone(), repo.clone()).await?;
        let cs_ctx = repo_ctx
            .changeset(bcs_id)
            .await?
            .ok_or(anyhow!("failed to get changeset context"))?;
        let hg_repo_ctx = repo_ctx.clone().hg();

        // Derive hg changeset to add entry for restricted paths
        let hg_cs = hg_cs_id
            .load(&self.ctx, scenario_repo.repo_blobstore())
            .await?;

        // Then get the root manifest id, get all the trees and run
        // `HgTreeContext::new_check_exists` to simulate a directory access
        let hg_mfid = hg_cs.manifestid();

        // Derive HgAugmentedManifest
        let _root_hg_aug_manifest_id = scenario_repo
            .repo_derived_data()
            .derive::<RootHgAugmentedManifestId>(&self.ctx, bcs_id, DerivationPriority::LOW)
            .await?;

        // Derive Fsnode to verify restricted manifest registration for V1 fingerprints.
        scenario_repo
            .repo_derived_data()
            .derive::<RootFsnodeId>(&self.ctx, bcs_id, DerivationPriority::LOW)
            .await?;

        // Derive ContentManifest
        let root_content_manifest_id = scenario_repo
            .repo_derived_data()
            .derive::<RootContentManifestId>(&self.ctx, bcs_id, DerivationPriority::LOW)
            .await?;

        // Sleep to ensure that the restricted paths cache was updated
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Type representing all the ways to access the restricted paths covered
        // in this test, so we can ensure that all of them generate
        // RestrictedPathsAuthorizationErrors when expected.
        #[derive(EnumIter, EnumDisplay, Debug, Eq, PartialEq, Hash, Clone, Copy)]
        enum AccessMethod {
            HgManifestId,
            HgAugmentedManifestId,
            Path,
            PathsWithContent,
            PathsWithHistory,
            ContentManifest,
        }

        // Run all the access operations that will trigger enforcement checks.
        // We collect RestrictedPathsAuthorizationErrors tagged by operation instead of
        // short-circuiting, so that all operations execute and produce their
        // log entries even when enforcement is enabled. Non-authorization
        // errors are propagated immediately to fail the test.
        let mut auth_errors: Vec<(AccessMethod, MononokeError)> = Vec::new();

        // Access all hg manifest tree entries
        let hg_manifest_results: Vec<Result<Vec<(AccessMethod, MononokeError)>, MononokeError>> =
            hg_mfid
                // TODO(T239041722): list files as well to ensure access is logged when a file is requested
                .list_tree_entries(self.ctx.clone(), blobstore.clone())
                .map_err(MononokeError::from)
                .and_then(async |(path, hg_manifest_id)| {
                    let mut errs = Vec::new();
                    // Access HgManifest
                    match hg_manifest_id.context(hg_repo_ctx.clone()).await {
                        Ok(_) => {}
                        Err(e @ MononokeError::RestrictedPathsAuthorizationError(_)) => {
                            errs.push((AccessMethod::HgManifestId, e))
                        }
                        Err(e) => return Err(e),
                    }
                    let hg_aug: HgAugmentedManifestId = hg_manifest_id.into();
                    // Access HgAugmentedManifest
                    match hg_aug.context(hg_repo_ctx.clone()).await {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            return Err(anyhow!("No HgAugmentedManifest for path {path:?}").into());
                        }
                        Err(e @ MononokeError::RestrictedPathsAuthorizationError(_)) => {
                            errs.push((AccessMethod::HgAugmentedManifestId, e))
                        }
                        Err(e) => return Err(e),
                    }
                    // Access path
                    match cs_ctx.path(path).await {
                        Ok(_) => {}
                        Err(e @ MononokeError::RestrictedPathsAuthorizationError(_)) => {
                            errs.push((AccessMethod::Path, e))
                        }
                        Err(e) => return Err(e),
                    }
                    Ok(errs)
                })
                .collect()
                .await;

        for result in hg_manifest_results {
            match result {
                Ok(errs) => auth_errors.extend(errs),
                Err(e) => return Err(e.into()),
            }
        }

        // Access all content manifest tree entries
        let content_manifest_results: Vec<
            Result<Option<(AccessMethod, MononokeError)>, MononokeError>,
        > = root_content_manifest_id
            .into_content_manifest_id()
            .list_tree_entries(self.ctx.clone(), blobstore.clone())
            .map_err(MononokeError::from)
            .and_then(async |(_path, content_manifest_id)| {
                match repo_ctx.tree(content_manifest_id).await {
                    Ok(_) => Ok(None),
                    Err(e @ MononokeError::RestrictedPathsAuthorizationError(_)) => {
                        Ok(Some((AccessMethod::ContentManifest, e)))
                    }
                    Err(e) => Err(e),
                }
            })
            .collect()
            .await;

        for result in content_manifest_results {
            match result {
                Ok(Some(err)) => auth_errors.push(err),
                Ok(None) => {}
                Err(e) => return Err(e.into()),
            }
        }

        // Access path contents as we do in SCS for diffing.
        let bonsai = cs_ctx.bonsai_changeset().await?;
        let paths = bonsai
            .file_changes_map()
            .keys()
            .map(|path| MPath::from(path.clone()))
            .collect::<BTreeSet<_>>();

        match cs_ctx
            .paths_with_content(paths.clone().into_iter(), &RestrictedPathsPolicy::Strict)
            .await
        {
            Ok(stream) => {
                let results: Vec<Result<_, MononokeError>> = stream.collect().await;
                for result in results {
                    match result {
                        Ok(_) => {}
                        Err(e @ MononokeError::RestrictedPathsAuthorizationError(_)) => {
                            auth_errors.push((AccessMethod::PathsWithContent, e))
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            Err(e @ MononokeError::RestrictedPathsAuthorizationError(_)) => {
                auth_errors.push((AccessMethod::PathsWithContent, e))
            }
            Err(e) => return Err(e.into()),
        }

        match cs_ctx
            .paths_with_history(paths.iter().cloned(), &RestrictedPathsPolicy::Strict)
            .await
        {
            Ok(stream) => {
                let results: Vec<Result<(), MononokeError>> = stream
                    .map(|r| async move {
                        match r {
                            Err(e) => Err(e),
                            Ok(context) => {
                                context.last_modified().await?;
                                Ok(())
                            }
                        }
                    })
                    .buffer_unordered(100)
                    .collect()
                    .await;

                for result in results {
                    match result {
                        Ok(()) => {}
                        Err(e @ MononokeError::RestrictedPathsAuthorizationError(_)) => {
                            auth_errors.push((AccessMethod::PathsWithHistory, e))
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            Err(e @ MononokeError::RestrictedPathsAuthorizationError(_)) => {
                auth_errors.push((AccessMethod::PathsWithHistory, e))
            }
            Err(e) => return Err(e.into()),
        }

        // Check authorization errors based on whether enforcement was expected
        if expect_enforcement {
            let mut grouped: HashMap<AccessMethod, Vec<&MononokeError>> = HashMap::new();
            for (op, err) in &auth_errors {
                grouped.entry(*op).or_default().push(err);
            }

            for op in AccessMethod::iter() {
                // Ensure that all access methods returned RestrictedPathsAuthorizationErrors.
                let auth_errors = grouped.remove(&op).unwrap_or_default();

                assert!(
                    !auth_errors.is_empty(),
                    "Scenario {scenario_idx}: expected RestrictedPathsAuthorizationError for operation '{op}' but got none.",
                )
            }
        } else {
            // When enforcement is NOT expected, no errors should have occurred
            assert!(
                auth_errors.is_empty(),
                "Scenario {scenario_idx}: expected access to succeed but got authorization errors: {auth_errors:#?}"
            );
        }

        // Only verify manifest entries and scuba logs when not expecting enforcement
        // (i.e., when access is expected to succeed)
        let manifest_id_store_entries = scenario_repo
            .restricted_paths()
            .config_based()
            .manifest_id_store()
            .get_all_entries(&self.ctx)
            .await?;

        // Access is logged asynchronously, so wait for a bit before reading
        // the log file
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let scuba_logs = deserialize_scuba_log_file(&log_path)?;

        // Verify expectations if they were set
        if let Some(expected_manifest_entries) = self.expected_manifest_entries.clone() {
            assert_eq!(
                expected_manifest_entries
                    .into_iter()
                    .sorted()
                    .collect::<Vec<_>>(),
                manifest_id_store_entries
                    .clone()
                    .into_iter()
                    .sorted()
                    .collect::<Vec<_>>()
            );
        }

        #[cfg(fbcode_build)]
        if let Some(expected_scuba_logs) = &self.expected_scuba_logs {
            // Sort both sides by debug representation since async logging
            // can produce entries in non-deterministic order.
            let mut expected_sorted = expected_scuba_logs.clone();
            let mut actual_sorted = scuba_logs.clone();
            expected_sorted.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
            actual_sorted.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
            assert_eq!(expected_sorted, actual_sorted);
        }
        #[cfg(not(fbcode_build))]
        let _ = (&scuba_logs, &self.expected_scuba_logs);

        println!(
            "Scenario {scenario_idx} finished SUCCESSFULLY with expect_enforcement: {expect_enforcement} and enforcement_condition_sets: {enforcement_condition_sets:#?}"
        );
        Ok(RestrictedPathsScenarioResult {
            scuba_logs,
            manifest_id_store_entries,
        })
    }

    async fn setup_scenario_repo(
        &self,
        enforcement_condition_sets: &[EnforcementConditionSet],
    ) -> Result<RestrictedPathsScenario> {
        let log_file = tempfile::NamedTempFile::new()?;
        let log_path = log_file.into_temp_path().keep()?;
        let acls = self.scenario_acls()?;
        let repo = setup_test_repo(
            &self.ctx,
            self.config_restricted_paths.clone(),
            self.config_paths_read_only,
            self.acl_manifest_mode,
            self.tooling_allowlist_group.clone(),
            acls,
            log_path.clone(),
            enforcement_condition_sets,
            &self.enforcement_exemption_sets,
        )
        .await?;

        Ok(RestrictedPathsScenario { repo, log_path })
    }

    async fn create_configured_changeset(&self, repo: &TestRepo) -> Result<ChangesetId> {
        let mut commit_ctx = CreateCommitContext::new_root(&self.ctx, repo);
        for (path, content) in &self.file_path_changes {
            let file_content = content.as_deref().unwrap_or(path.as_str());
            commit_ctx = commit_ctx.add_file(path.as_str(), file_content.to_string());
        }
        for (root, acl) in &self.acl_manifest_restricted_paths {
            let slacl_path = format!("{root}/.slacl");
            let slacl_content = format!("repo_region_acl = \"{acl}\"\n");
            commit_ctx = commit_ctx.add_file(slacl_path.as_str(), slacl_content);
        }
        commit_ctx.commit().await
    }

    fn scenario_acls(&self) -> Result<Acls> {
        let groups_config: Vec<(&str, Vec<&str>)> = self
            .groups_config
            .iter()
            .map(|(group, users)| (group.as_str(), users.iter().map(|u| u.as_str()).collect()))
            .collect();

        if self.repo_regions_config.is_empty() {
            return default_test_acls(groups_config);
        }

        let repo_regions_config: Vec<(&str, Vec<&str>)> = self
            .repo_regions_config
            .iter()
            .map(|(region, users)| (region.as_str(), users.iter().map(|u| u.as_str()).collect()))
            .collect();
        setup_test_acls_with_groups(repo_regions_config, groups_config)
    }
}

#[derive(Default)]
pub(crate) struct EnforcementConditionSetBuilder {
    always_enabled: bool,
    entry_points: Vec<String>,
    require_client_request_flag: bool,
    restriction_acls: Vec<MononokeIdentity>,
    machine_tiers: Vec<String>,
    client_identity_regexes: Vec<ComparableRegex>,
    is_agent: Option<bool>,
}

impl EnforcementConditionSetBuilder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn with_always_enabled(mut self, always_enabled: bool) -> Self {
        self.always_enabled = always_enabled;
        self
    }

    pub(crate) fn with_entry_points<I, S>(mut self, entry_points: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.entry_points = entry_points.into_iter().map(Into::into).collect();
        self
    }

    pub(crate) fn with_require_client_request_flag(
        mut self,
        require_client_request_flag: bool,
    ) -> Self {
        self.require_client_request_flag = require_client_request_flag;
        self
    }

    pub(crate) fn with_restriction_acls<I>(mut self, restriction_acls: I) -> Self
    where
        I: IntoIterator<Item = MononokeIdentity>,
    {
        self.restriction_acls = restriction_acls.into_iter().collect();
        self
    }

    pub(crate) fn with_machine_tiers<I, S>(mut self, machine_tiers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.machine_tiers = machine_tiers.into_iter().map(Into::into).collect();
        self
    }

    pub(crate) fn with_client_identity_regexes<I, S>(mut self, patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.client_identity_regexes = patterns
            .into_iter()
            .map(|s| ComparableRegex::new(s.as_ref()).expect("test regex must be valid"))
            .collect();
        self
    }

    pub(crate) fn with_is_agent(mut self, is_agent: bool) -> Self {
        self.is_agent = Some(is_agent);
        self
    }

    pub(crate) fn build(self) -> EnforcementConditionSet {
        EnforcementConditionSet {
            always_enabled: self.always_enabled,
            require_client_request_flag: self.require_client_request_flag,
            restriction_acls: self.restriction_acls,
            matchers: RequestMatchers {
                entry_points: self.entry_points,
                machine_tiers: self.machine_tiers,
                // `build_rules` matches on the server's own `build_info` build rule,
                // which can't be set from a unit test (it's a link-time constant), so
                // tests leave it empty. Coverage lives in the `.t` integration test.
                build_rules: Vec::new(),
                client_identity_regexes: self.client_identity_regexes,
                is_agent: self.is_agent,
            },
        }
    }
}

/// Creates an Acls structure for testing with specified repo regions, users, and groups.
/// The ACL provides the test user access to all repos, specified repo regions, and groups.
fn setup_test_acls_with_groups(
    repo_regions_config: Vec<(&str, Vec<&str>)>,
    groups_config: Vec<(&str, Vec<&str>)>,
) -> Result<Acls> {
    let repo_regions = repo_regions_config
        .into_iter()
        .map(|(region_name, usernames)| {
            let users = usernames
                .into_iter()
                .map(|username| MononokeIdentity::from_str(&format!("USER:{username}")))
                .collect::<Result<MononokeIdentitySet, _>>()?;
            Ok((
                region_name.to_string(),
                Arc::new(Acl {
                    actions: hashmap! {
                        "read".to_string() => users,
                    },
                }),
            ))
        })
        .collect::<Result<HashMap<_, _>>>()?;

    let groups = groups_config
        .into_iter()
        .map(|(group_name, identities)| {
            let members = identities
                .into_iter()
                .map(MononokeIdentity::from_str)
                .collect::<Result<MononokeIdentitySet, _>>()?;
            Ok((group_name.to_string(), Arc::new(members)))
        })
        .collect::<Result<HashMap<_, _>>>()?;

    let default_user = MononokeIdentity::from_str("USER:myusername0")?;
    let default_read_users = MononokeIdentitySet::from([default_user.clone()]);
    let default_write_users = MononokeIdentitySet::from([default_user]);

    let repos = hashmap! {
        "default".to_string() => Arc::new(Acl {
            actions: hashmap! {
                "read".to_string() => default_read_users,
                "write".to_string() => default_write_users,
            },
        }),
    };

    Ok(Acls {
        repos,
        repo_regions,
        tiers: HashMap::new(),
        workspaces: HashMap::new(),
        groups,
    })
}

/// Creates a default Acls structure for testing.
/// The ACL provides the test user access to all repos and appropriate repo regions.
fn default_test_acls(groups_config: Vec<(&str, Vec<&str>)>) -> Result<Acls> {
    setup_test_acls_with_groups(
        vec![
            ("myusername_project", vec!["myusername0"]),
            ("restricted_acl", vec!["another_user"]),
        ],
        groups_config,
    )
}

/// Sets up an ACL file that will be used to create an ACL checker.
/// The ACL provides the test user access to all repos and appropriate repo regions.
fn setup_acl_file(acls: Acls) -> Result<std::path::PathBuf> {
    use std::io::Write;

    let mut temp_file = tempfile::NamedTempFile::new()?;

    let acl_content = serde_json::to_string_pretty(&acls)?;

    temp_file.write_all(acl_content.as_bytes())?;
    temp_file.flush()?;
    let acl_path = temp_file.into_temp_path().keep()?;
    Ok(acl_path.to_path_buf())
}

async fn setup_test_repo(
    ctx: &CoreContext,
    config_restricted_paths: Vec<(NonRootMPath, MononokeIdentity)>,
    config_paths_read_only: bool,
    acl_manifest_mode: AclManifestMode,
    tooling_allowlist_group: Option<String>,
    acls: Acls,
    log_file_path: std::path::PathBuf,
    enforcement_condition_sets: &[EnforcementConditionSet],
    enforcement_exemption_sets: &[EnforcementExemptionSet],
) -> Result<TestRepo> {
    let repo_id = RepositoryId::new(0);
    let use_manifest_id_cache = true;
    let cache_update_interval_ms = 5;
    let acl_file = setup_acl_file(acls)?;

    let acl_provider = InternalAclProvider::from_file(&acl_file)
        .with_context(|| format!("Failed to load ACLs from '{}'", acl_file.to_string_lossy()))?;

    let path_restriction_metadata = config_restricted_paths
        .into_iter()
        .map(|(path, acl)| {
            (
                path,
                PathRestrictionMetadata {
                    repo_region_acl: acl,
                    permission_request_group: None,
                    rollout_allowlist_group: None,
                    read_only: config_paths_read_only,
                },
            )
        })
        .collect();

    let manifest_id_store = Arc::new(
        SqlRestrictedPathsManifestIdStoreBuilder::with_sqlite_in_memory()
            .context("Failed to create Sqlite connection")?
            .with_repo_id(repo_id),
    );

    let config = RestrictedPathsConfig {
        path_restriction_metadata,
        manifest_id_store_config: RestrictedPathsManifestIdStoreConfig {
            use_manifest_id_cache,
            cache_update_interval_ms,
            use_incremental_cache_updates: true,
            ..Default::default()
        },
        tooling_allowlist_group,
        acl_manifest_mode,
        enforcement_condition_sets: enforcement_condition_sets.to_vec(),
        enforcement_exemption_sets: enforcement_exemption_sets.to_vec(),
        enforcement_enabled: !enforcement_condition_sets.is_empty(),
        ..Default::default()
    };

    let cache = Arc::new(
        RestrictedPathsManifestIdCacheBuilder::new(ctx.clone(), manifest_id_store.clone())
            .with_manifest_id_store_cache(config.manifest_id_store_config.clone())
            .build()
            .await?,
    );

    // Create scuba builder that logs to the test file
    let scuba_builder = MononokeScubaSampleBuilder::with_discard().with_log_file(log_file_path)?;

    let config_based = Arc::new(RestrictedPathsConfigBased::new(
        config,
        manifest_id_store.clone(),
        Some(cache),
    ));

    // Build a repo first to get ArcRepoDerivedData. We use the same factory
    // instance so that the final repo shares the same blobstore, ensuring
    // that ACL manifest derivation inside spawned logging tasks can find
    // changesets committed to this repo.
    let mut factory = TestRepoFactory::new(ctx.fb)?;
    let repo: TestRepo = factory.build().await?;
    let repo_derived_data = repo.repo_derived_data_arc();

    let repo_restricted_paths = Arc::new(RestrictedPaths::new(
        "repo".to_string(),
        config_based,
        acl_provider,
        scuba_builder,
        repo_derived_data,
    )?);

    // Rebuild with the custom restricted_paths, reusing the same factory
    // so the blobstore is shared with repo_derived_data above.
    let repo = factory
        .with_restricted_paths(repo_restricted_paths)
        .build()
        .await?;

    Ok(repo)
}

/// Reads the scuba log file and parses all samples as ScubaAccessLogSample
fn deserialize_scuba_log_file(
    scuba_log_file: &std::path::Path,
) -> Result<Vec<ScubaAccessLogSample>> {
    use std::fs::File;
    use std::io::BufRead;
    use std::io::BufReader;

    let file = match File::open(scuba_log_file) {
        Ok(file) => file,
        // If nothing is logged, file won't be created
        Err(_e) => return Ok(vec![]),
    };
    let reader = BufReader::new(file);

    // Collect all lines first (not efficient for very large files, but works for test logs)
    let lines: Vec<String> = reader.lines().collect::<std::io::Result<_>>()?;

    // Parse each line as a ScubaTelemetryLogSample object
    let log_samples: Vec<ScubaAccessLogSample> = lines
        .into_iter()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(&line)
                .map_err(anyhow::Error::from)
                .with_context(|| format!("Failed to parse line: {line}"))
                .map(|json| {
                    // Scuba groups the logs by type (e.g. int, normal), so
                    // let's remove those and flatten the sample into a single
                    // json object.
                    let flattened_log =
                        json.as_object()
                            .iter()
                            .flat_map(|obj| {
                                obj.iter().flat_map(|(_, category_values)| {
                                    category_values.as_object().into_iter().flat_map(
                                        |category_obj| {
                                            category_obj.iter().map(|(k, v)| (k.clone(), v.clone()))
                                        },
                                    )
                                })
                            })
                            .collect::<serde_json::Value>();

                    let repo_id: RepositoryId = flattened_log["repo_id"]
                        .as_number()
                        .and_then(|s| s.as_i64())
                        .and_then(|i| i.try_into().ok())
                        .map(RepositoryId::new)
                        .ok_or(anyhow!("missing repo_id"))?;

                    let client_main_id: String = flattened_log["client_main_id"]
                        .as_str()
                        .map(String::from)
                        .ok_or(anyhow!("missing client_main_id"))?;

                    let manifest_id = flattened_log["manifest_id"]
                        .as_str()
                        .map(String::from)
                        .map(RestrictedManifestId::from);

                    let manifest_type = flattened_log["manifest_type"]
                        .as_str()
                        .map(ManifestType::from_str)
                        .transpose()?;

                    let full_path = flattened_log["full_path"]
                        .as_str()
                        .map(NonRootMPath::new)
                        .transpose()?;

                    let has_authorization: bool = flattened_log["has_authorization"]
                        .as_str()
                        .map(|st| st.parse::<bool>())
                        .transpose()?
                        .ok_or(anyhow!("missing has_authorization"))?;

                    let is_allowlisted_tooling: bool = flattened_log["is_allowlisted_tooling"]
                        .as_str()
                        .map(|st| st.parse::<bool>())
                        .transpose()?
                        .unwrap_or(false);

                    let has_acl_access: bool = flattened_log["has_acl_access"]
                        .as_str()
                        .map(|st| st.parse::<bool>())
                        .transpose()?
                        .unwrap_or(false);

                    let restricted_paths: Vec<NonRootMPath> = flattened_log["restricted_paths"]
                        .as_array()
                        .map(|ids| {
                            ids.iter()
                                .filter_map(|id| id.as_str())
                                .sorted()
                                .map(NonRootMPath::new)
                                .collect::<Result<Vec<_>>>()
                        })
                        .transpose()?
                        .ok_or(anyhow!("missing restricted_paths"))?;

                    let client_identities: Vec<String> = flattened_log["client_identities"]
                        .as_array()
                        .map(|ids| {
                            ids.iter()
                                .filter_map(|id| id.as_str())
                                .map(String::from)
                                .sorted()
                                .collect()
                        })
                        .unwrap_or_default();

                    let acls: Vec<MononokeIdentity> = flattened_log["acls"]
                        .as_array()
                        .map(|ids| {
                            let mut acls: Vec<MononokeIdentity> = ids
                                .iter()
                                .filter_map(|id| id.as_str())
                                .filter_map(|s| MononokeIdentity::from_str(s).ok())
                                .collect();
                            acls.sort();
                            acls
                        })
                        .unwrap_or_default();

                    let considered_restricted_by: Vec<String> =
                        flattened_log["considered_restricted_by"]
                            .as_array()
                            .map(|ids| {
                                ids.iter()
                                    .filter_map(|id| id.as_str())
                                    .map(String::from)
                                    .sorted()
                                    .collect()
                            })
                            .unwrap_or_default();

                    let access_enforcement_enabled =
                        optional_bool_field(&flattened_log, "access_enforcement_enabled")?;
                    let acl_manifest_mode =
                        optional_string_field(&flattened_log, "acl_manifest_mode");
                    let config_error = optional_string_field(&flattened_log, "config_error");
                    let acl_manifest_error =
                        optional_string_field(&flattened_log, "acl_manifest_error");
                    let shadow_mismatch = optional_bool_field(&flattened_log, "shadow_mismatch")?;
                    let shadow_mismatch_detail =
                        optional_string_field(&flattened_log, "shadow_mismatch_detail");

                    Ok(ScubaAccessLogSample {
                        repo_id,
                        restricted_paths,
                        manifest_id,
                        manifest_type,
                        full_path,
                        client_identities,
                        has_authorization,
                        is_allowlisted_tooling,
                        has_acl_access,
                        client_main_id,
                        acls,
                        considered_restricted_by,
                        access_enforcement_enabled,
                        acl_manifest_mode,
                        config_error,
                        acl_manifest_error,
                        shadow_mismatch,
                        shadow_mismatch_detail,
                    })
                })?
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(log_samples)
}

fn optional_string_field(flattened_log: &serde_json::Value, key: &str) -> Option<String> {
    flattened_log[key].as_str().map(String::from)
}

fn optional_bool_field(flattened_log: &serde_json::Value, key: &str) -> Result<Option<bool>> {
    match &flattened_log[key] {
        serde_json::Value::Bool(value) => Ok(Some(*value)),
        value => value
            .as_str()
            .map(|value| value.parse::<bool>())
            .transpose()
            .with_context(|| format!("failed to parse {key} as bool")),
    }
}

pub(crate) fn cast_to_non_root_mpaths(paths: Vec<&str>) -> Result<Vec<NonRootMPath>> {
    paths
        .into_iter()
        .map(NonRootMPath::new)
        .collect::<Result<Vec<_>>>()
        .context("Failed to cast to NonRootMPath")
}
